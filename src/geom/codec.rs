//! ESRI Shape 二进制编解码 —— Personal Geodatabase 中 `Shape` 列的核心。
//!
//! # 存储事实
//! Personal Geodatabase 把几何放在业务表的 OLE Object（长二进制）字段中，
//! 其内容就是 **shapefile 记录的 record body**（小端），即：
//!
//! ```text
//! int32   ShapeType
//! Point:        X(f64) Y(f64)                          [+Z(f64)] [+M(f64)]
//! MultiPoint:   Box(4×f64) NumPoints(i32) Points[]     [+Zrange(2×f64) Z[]] [+Mrange M[]]
//! PolyLine:     Box(4×f64) NumParts(i32) NumPoints(i32) Parts[i32] Points[]
//! Polygon:      同 PolyLine
//! 带 Z 的形态在 Points 之后追加 Zrang/Z 数组，带 M 的形态追加 Mrange/M 数组。
//! ```
//!
//! 与真正 shapefile 文件的差异仅在于 **没有 100 字节文件头**，因此把 `.shp`
//! 的每条记录字节直接写入 Shape 列即可被 ArcMap 正确识别。
//!
//! 某些 ODBC 驱动在读取 OLE Object 时会在前面附加 4 字节长度前缀，
//! 因此 [`decode_shape`] 会自动嗅探偏移量。

use std::io::Cursor;

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use crate::error::{PgdbError, Result};

use super::{Envelope, Geometry, ShapeCode, Vertex};

/// 解码 Shape 二进制流（自动嗅探 0/4 字节前缀）。
pub fn decode_shape(bytes: &[u8]) -> Result<Geometry> {
    if bytes.is_empty() {
        return Ok(Geometry::Null);
    }
    // 1) 常规布局：头部即 ShapeType
    if let Ok(code) = Cursor::new(bytes).read_i32::<LittleEndian>() {
        if ShapeCode::try_from_i32(code).is_some() {
            return decode_at(bytes, 0);
        }
    }
    // 2) 兼容部分 ODBC 驱动附加的 4 字节长度前缀
    if bytes.len() > 4 {
        if let Ok(code) = Cursor::new(&bytes[4..]).read_i32::<LittleEndian>() {
            if ShapeCode::try_from_i32(code).is_some() {
                return decode_at(bytes, 4);
            }
        }
    }
    Err(PgdbError::geometry(format!(
        "无法识别的 Shape 二进制头部: {:02X?}",
        &bytes[..bytes.len().min(8)]
    )))
}

/// 在指定偏移处开始解码。
pub fn decode_at(bytes: &[u8], offset: usize) -> Result<Geometry> {
    let mut cur = Cursor::new(&bytes[offset..]);
    let code_i32 = cur.read_i32::<LittleEndian>()?;
    let code = ShapeCode::try_from_i32(code_i32)
        .ok_or_else(|| PgdbError::geometry(format!("未知 ShapeType: {code_i32}")))?;

    let g = match code {
        ShapeCode::NullShape => Geometry::Null,
        ShapeCode::Point => {
            let v = read_xy(&mut cur)?;
            Geometry::Point(v)
        }
        ShapeCode::PointZ => {
            let mut v = read_xy(&mut cur)?;
            v.z = Some(cur.read_f64::<LittleEndian>()?);
            if remains(&cur) >= 8 {
                v.m = read_opt_f64(&mut cur);
            }
            Geometry::Point(v)
        }
        ShapeCode::PointM => {
            let mut v = read_xy(&mut cur)?;
            v.m = read_opt_f64(&mut cur);
            Geometry::Point(v)
        }
        ShapeCode::MultiPoint
        | ShapeCode::MultiPointZ
        | ShapeCode::MultiPointM
        | ShapeCode::PolyLine
        | ShapeCode::PolyLineZ
        | ShapeCode::PolyLineM
        | ShapeCode::Polygon
        | ShapeCode::PolygonZ
        | ShapeCode::PolygonM => {
            let _box = read_box(&mut cur)?;
            let (num_parts, num_points) = if is_multipoint_family(&code) {
                // MultiPoint 系列布局：Box, NumPoints, Points[]
                (0i32, cur.read_i32::<LittleEndian>()?)
            } else {
                // PolyLine / Polygon 系列布局：Box, NumParts, NumPoints, Parts[], Points[]
                let parts = cur.read_i32::<LittleEndian>()?;
                (parts, cur.read_i32::<LittleEndian>()?)
            };
            if num_points < 0 {
                return Err(PgdbError::geometry(format!("顶点数为负: {num_points}")));
            }
            let parts_idx = if is_multipoint_family(&code) {
                None
            } else {
                if num_parts < 0 {
                    return Err(PgdbError::geometry(format!("部件数为负: {num_parts}")));
                }
                Some(read_part_index(&mut cur, num_parts as usize)?)
            };
            let points = read_points(&mut cur, num_points as usize)?;
            let points = apply_zm(&mut cur, &code, points)?;
            match (code, parts_idx) {
                (ShapeCode::MultiPoint, _) | (ShapeCode::MultiPointZ, _) | (ShapeCode::MultiPointM, _) => {
                    Geometry::Multipoint(points)
                }
                (ShapeCode::PolyLine, Some(idx))
                | (ShapeCode::PolyLineZ, Some(idx))
                | (ShapeCode::PolyLineM, Some(idx)) => Geometry::Polyline(split_parts(points, &idx)?),
                (ShapeCode::Polygon, Some(idx))
                | (ShapeCode::PolygonZ, Some(idx))
                | (ShapeCode::PolygonM, Some(idx)) => Geometry::Polygon(split_parts(points, &idx)?),
                _ => unreachable!(),
            }
        }
        ShapeCode::Multipatch => {
            return Err(PgdbError::Unsupported(
                "MultiPatch 暂不支持，可在 issue 中提出需求".into(),
            ))
        }
    };
    Ok(g)
}

/// 是否为 MultiPoint 系列（布局中没有 NumParts）
fn is_multipoint_family(code: &ShapeCode) -> bool {
    matches!(
        code,
        ShapeCode::MultiPoint | ShapeCode::MultiPointZ | ShapeCode::MultiPointM
    )
}

/// 剩余可读字节数
fn remains(cur: &Cursor<&[u8]>) -> u64 {
    let pos = cur.position();
    cur.get_ref().len() as u64 - pos
}

/// 读取可能缺失（长度不足）的 f64
fn read_opt_f64(cur: &mut Cursor<&[u8]>) -> Option<f64> {
    if remains(cur) >= 8 {
        cur.read_f64::<LittleEndian>().ok()
    } else {
        None
    }
}

fn read_xy(cur: &mut Cursor<&[u8]>) -> Result<Vertex> {
    let x = cur.read_f64::<LittleEndian>()?;
    let y = cur.read_f64::<LittleEndian>()?;
    Ok(Vertex::new(x, y))
}

fn read_box(cur: &mut Cursor<&[u8]>) -> Result<Envelope> {
    let min_x = cur.read_f64::<LittleEndian>()?;
    let min_y = cur.read_f64::<LittleEndian>()?;
    let max_x = cur.read_f64::<LittleEndian>()?;
    let max_y = cur.read_f64::<LittleEndian>()?;
    Ok(Envelope::new(min_x, min_y, max_x, max_y))
}

fn read_part_index(cur: &mut Cursor<&[u8]>, n: usize) -> Result<Vec<usize>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(cur.read_i32::<LittleEndian>()? as usize);
    }
    Ok(out)
}

fn read_points(cur: &mut Cursor<&[u8]>, n: usize) -> Result<Vec<Vertex>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_xy(cur)?);
    }
    Ok(out)
}

/// 根据 ShapeCode 追加 Z / M 数组
fn apply_zm(cur: &mut Cursor<&[u8]>, code: &ShapeCode, mut pts: Vec<Vertex>) -> Result<Vec<Vertex>> {
    let n = pts.len();
    let need_z = matches!(
        code,
        ShapeCode::MultiPointZ | ShapeCode::PolyLineZ | ShapeCode::PolygonZ
    );
    let need_m = matches!(
        code,
        ShapeCode::MultiPointZ
            | ShapeCode::MultiPointM
            | ShapeCode::PolyLineZ
            | ShapeCode::PolyLineM
            | ShapeCode::PolygonZ
            | ShapeCode::PolygonM
    );
    if need_z && remains(cur) >= 16 + 8 * n as u64 {
        let _zmin = cur.read_f64::<LittleEndian>()?;
        let _zmax = cur.read_f64::<LittleEndian>()?;
        for p in pts.iter_mut() {
            p.z = Some(cur.read_f64::<LittleEndian>()?);
        }
    }
    if need_m && remains(cur) >= 16 + 8 * n as u64 {
        let _mmin = cur.read_f64::<LittleEndian>()?;
        let _mmax = cur.read_f64::<LittleEndian>()?;
        for p in pts.iter_mut() {
            p.m = Some(cur.read_f64::<LittleEndian>()?);
        }
    }
    Ok(pts)
}

/// 按 part 索引切分顶点数组
fn split_parts(pts: Vec<Vertex>, idx: &[usize]) -> Result<Vec<Vec<Vertex>>> {
    if idx.is_empty() {
        // 容错：无 part 信息时整体作为一个部件
        return Ok(vec![pts]);
    }
    let mut out = Vec::with_capacity(idx.len());
    for i in 0..idx.len() {
        let start = idx[i];
        let end = if i + 1 < idx.len() {
            idx[i + 1]
        } else {
            pts.len()
        };
        if start > pts.len() || end > pts.len() || end < start {
            return Err(PgdbError::geometry(format!(
                "部件索引越界: start={start} end={end} points={}",
                pts.len()
            )));
        }
        out.push(pts[start..end].to_vec());
    }
    Ok(out)
}

/// 编码几何为 Shape 二进制。
///
/// 输出内容与 shapefile record body 完全一致，可直接写入 mdb 的 Shape 字段。
pub fn encode_shape(g: &Geometry) -> Result<Vec<u8>> {
    let mut w: Vec<u8> = Vec::new();
    match g {
        Geometry::Null => {
            w.write_i32::<LittleEndian>(ShapeCode::NullShape as i32)?;
        }
        Geometry::Point(v) => {
            w.write_i32::<LittleEndian>(if v.z.is_some() {
                ShapeCode::PointZ as i32
            } else if v.m.is_some() {
                ShapeCode::PointM as i32
            } else {
                ShapeCode::Point as i32
            })?;
            w.write_f64::<LittleEndian>(v.x)?;
            w.write_f64::<LittleEndian>(v.y)?;
            if v.z.is_some() {
                w.write_f64::<LittleEndian>(v.z.unwrap_or(f64::NAN))?;
            }
            if v.m.is_some() {
                w.write_f64::<LittleEndian>(v.m.unwrap_or(f64::NAN))?;
            }
        }
        Geometry::Envelope(e) => {
            w.write_i32::<LittleEndian>(ShapeCode::Polygon as i32)?;
            write_box(&mut w, e)?;
            w.write_i32::<LittleEndian>(1)?;
            w.write_i32::<LittleEndian>(5)?;
            w.write_i32::<LittleEndian>(0)?;
            let ring = vec![
                Vertex::new(e.min_x, e.min_y),
                Vertex::new(e.min_x, e.max_y),
                Vertex::new(e.max_x, e.max_y),
                Vertex::new(e.max_x, e.min_y),
                Vertex::new(e.min_x, e.min_y),
            ];
            for v in ring {
                write_xy(&mut w, v)?;
            }
        }
        Geometry::Multipoint(pts) => {
            let has_z = pts.iter().any(|v| v.z.is_some());
            let has_m = pts.iter().any(|v| v.m.is_some());
            let code = if has_z {
                ShapeCode::MultiPointZ
            } else if has_m {
                ShapeCode::MultiPointM
            } else {
                ShapeCode::MultiPoint
            } as i32;
            let env = Envelope::new(
                min_of(pts, |v| v.x),
                min_of(pts, |v| v.y),
                max_of(pts, |v| v.x),
                max_of(pts, |v| v.y),
            );
            w.write_i32::<LittleEndian>(code)?;
            write_box(&mut w, &env)?;
            w.write_i32::<LittleEndian>(pts.len() as i32)?;
            for v in pts {
                write_xy(&mut w, *v)?;
            }
            write_zm_arrays(&mut w, pts, has_z, has_m)?;
        }
        Geometry::Polyline(paths) => {
            write_multipart(&mut w, ShapeCode::PolyLine, paths)?;
        }
        Geometry::Polygon(rings) => {
            write_multipart(&mut w, ShapeCode::Polygon, rings)?;
        }
    }
    Ok(w)
}

/// 该几何对应的 ShapeType 码
pub fn shape_code(g: &Geometry) -> i32 {
    match g {
        Geometry::Null => ShapeCode::NullShape as i32,
        Geometry::Point(v) => {
            if v.z.is_some() {
                ShapeCode::PointZ as i32
            } else if v.m.is_some() {
                ShapeCode::PointM as i32
            } else {
                ShapeCode::Point as i32
            }
        }
        Geometry::Multipoint(pts) => {
            if pts.iter().any(|v| v.z.is_some()) {
                ShapeCode::MultiPointZ as i32
            } else if pts.iter().any(|v| v.m.is_some()) {
                ShapeCode::MultiPointM as i32
            } else {
                ShapeCode::MultiPoint as i32
            }
        }
        Geometry::Polyline(paths) => {
            if paths.iter().any(|p| p.iter().any(|v| v.z.is_some())) {
                ShapeCode::PolyLineZ as i32
            } else if paths.iter().any(|p| p.iter().any(|v| v.m.is_some())) {
                ShapeCode::PolyLineM as i32
            } else {
                ShapeCode::PolyLine as i32
            }
        }
        Geometry::Polygon(rings) => {
            if rings.iter().any(|p| p.iter().any(|v| v.z.is_some())) {
                ShapeCode::PolygonZ as i32
            } else if rings.iter().any(|p| p.iter().any(|v| v.m.is_some())) {
                ShapeCode::PolygonM as i32
            } else {
                ShapeCode::Polygon as i32
            }
        }
        Geometry::Envelope(_) => ShapeCode::Polygon as i32,
    }
}

fn min_of(pts: &[Vertex], f: fn(&Vertex) -> f64) -> f64 {
    pts.iter().map(f).fold(f64::INFINITY, f64::min)
}

fn max_of(pts: &[Vertex], f: fn(&Vertex) -> f64) -> f64 {
    pts.iter().map(f).fold(f64::NEG_INFINITY, f64::max)
}

fn write_box(w: &mut Vec<u8>, e: &Envelope) -> Result<()> {
    w.write_f64::<LittleEndian>(e.min_x)?;
    w.write_f64::<LittleEndian>(e.min_y)?;
    w.write_f64::<LittleEndian>(e.max_x)?;
    w.write_f64::<LittleEndian>(e.max_y)?;
    Ok(())
}

fn write_xy(w: &mut Vec<u8>, v: Vertex) -> Result<()> {
    w.write_f64::<LittleEndian>(v.x)?;
    w.write_f64::<LittleEndian>(v.y)?;
    Ok(())
}

/// 写 multipoint/polyline/polygon 的公共结构
fn write_multipart(w: &mut Vec<u8>, base: ShapeCode, parts: &[Vec<Vertex>]) -> Result<()> {
    let all: Vec<Vertex> = parts.iter().flatten().copied().collect();
    if all.is_empty() {
        return Err(PgdbError::geometry("几何没有任何顶点，无法编码"));
    }
    let has_z = all.iter().any(|v| v.z.is_some());
    let has_m = all.iter().any(|v| v.m.is_some());
    let code = match base {
        ShapeCode::PolyLine => {
            if has_z {
                ShapeCode::PolyLineZ
            } else if has_m {
                ShapeCode::PolyLineM
            } else {
                ShapeCode::PolyLine
            }
        }
        _ => {
            if has_z {
                ShapeCode::PolygonZ
            } else if has_m {
                ShapeCode::PolygonM
            } else {
                ShapeCode::Polygon
            }
        }
    } as i32;

    let env = Envelope::new(
        min_of(&all, |v| v.x),
        min_of(&all, |v| v.y),
        max_of(&all, |v| v.x),
        max_of(&all, |v| v.y),
    );
    w.write_i32::<LittleEndian>(code)?;
    write_box(w, &env)?;
    w.write_i32::<LittleEndian>(parts.len() as i32)?;
    w.write_i32::<LittleEndian>(all.len() as i32)?;
    let mut acc = 0usize;
    for p in parts {
        w.write_i32::<LittleEndian>(acc as i32)?;
        acc += p.len();
    }
    for v in &all {
        write_xy(w, *v)?;
    }
    write_zm_arrays(w, &all, has_z, has_m)?;
    Ok(())
}

/// 追加 Z / M 范围与数组
fn write_zm_arrays(w: &mut Vec<u8>, pts: &[Vertex], has_z: bool, has_m: bool) -> Result<()> {
    if has_z {
        let zs: Vec<f64> = pts.iter().map(|v| v.z.unwrap_or(f64::NAN)).collect();
        let zmin = zs.iter().copied().fold(f64::INFINITY, f64::min);
        let zmax = zs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        w.write_f64::<LittleEndian>(zmin)?;
        w.write_f64::<LittleEndian>(zmax)?;
        for z in zs {
            w.write_f64::<LittleEndian>(z)?;
        }
    }
    if has_m {
        let ms: Vec<f64> = pts.iter().map(|v| v.m.unwrap_or(f64::NAN)).collect();
        let mmin = ms.iter().copied().fold(f64::INFINITY, f64::min);
        let mmax = ms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        w.write_f64::<LittleEndian>(mmin)?;
        w.write_f64::<LittleEndian>(mmax)?;
        for m in ms {
            w.write_f64::<LittleEndian>(m)?;
        }
    }
    Ok(())
}

/// 直接读取 ShapeType 码（不解码完整几何）
pub fn peek_shape_type(bytes: &[u8]) -> Result<i32> {
    if bytes.len() < 4 {
        return Err(PgdbError::geometry("Shape 二进制不足 4 字节"));
    }
    let v = Cursor::new(&bytes[..4]).read_i32::<LittleEndian>()?;
    if ShapeCode::try_from_i32(v).is_some() {
        return Ok(v);
    }
    if bytes.len() >= 8 {
        let v2 = Cursor::new(&bytes[4..8]).read_i32::<LittleEndian>()?;
        if ShapeCode::try_from_i32(v2).is_some() {
            return Ok(v2);
        }
    }
    Err(PgdbError::geometry("无法识别 ShapeType"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::geometry_from_wkt;

    fn roundtrip(g: &Geometry) {
        let bytes = encode_shape(g).unwrap();
        let back = decode_shape(&bytes).unwrap();
        match (&back, g) {
            (Geometry::Point(a), Geometry::Point(b)) => {
                assert!((a.x - b.x).abs() < 1e-12 && (a.y - b.y).abs() < 1e-12);
                assert_eq!(a.z, b.z);
                assert_eq!(a.m, b.m);
            }
            _ => assert_eq!(&back, g),
        }
    }

    #[test]
    fn test_point_roundtrip() {
        roundtrip(&Geometry::Point(Vertex::new(116.39, 39.9)));
        roundtrip(&Geometry::Point(Vertex::new_z(1.0, 2.0, 3.0)));
        roundtrip(&Geometry::Point(Vertex::new_m(1.0, 2.0, 9.5)));
    }

    #[test]
    fn test_multipoint_roundtrip() {
        roundtrip(&Geometry::Multipoint(vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(1.0, 1.0),
        ]));
        roundtrip(&Geometry::Multipoint(vec![
            Vertex::new_z(0.0, 0.0, 5.0),
            Vertex::new_z(1.0, 1.0, 6.0),
        ]));
    }

    #[test]
    fn test_polyline_roundtrip() {
        let g = Geometry::Polyline(vec![
            vec![Vertex::new(0.0, 0.0), Vertex::new(10.0, 0.0)],
            vec![
                Vertex::new(0.0, 5.0),
                Vertex::new(3.0, 5.0),
                Vertex::new(6.0, 7.0),
            ],
        ]);
        roundtrip(&g);
        // 与 shapefile 记录体一致：4 + 32 + 4 + 4 + 8(parts) + 5*16(points)
        let bytes = encode_shape(&g).unwrap();
        assert_eq!(bytes.len(), 4 + 32 + 4 + 4 + 2 * 4 + 5 * 16);
        assert_eq!(shape_code(&g), ShapeCode::PolyLine as i32);
    }

    #[test]
    fn test_polygon_roundtrip() {
        let g = Geometry::Polygon(vec![vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(0.0, 4.0),
            Vertex::new(4.0, 4.0),
            Vertex::new(4.0, 0.0),
            Vertex::new(0.0, 0.0),
        ]]);
        roundtrip(&g);
        let bytes = encode_shape(&g).unwrap();
        assert_eq!(bytes.len(), 4 + 32 + 4 + 4 + 4 + 5 * 16);
    }

    #[test]
    fn test_polygon_with_hole_roundtrip() {
        let g = Geometry::Polygon(vec![
            vec![
                Vertex::new(0.0, 0.0),
                Vertex::new(0.0, 10.0),
                Vertex::new(10.0, 10.0),
                Vertex::new(10.0, 0.0),
                Vertex::new(0.0, 0.0),
            ],
            vec![
                Vertex::new(2.0, 2.0),
                Vertex::new(2.0, 8.0),
                Vertex::new(8.0, 8.0),
                Vertex::new(8.0, 2.0),
                Vertex::new(2.0, 2.0),
            ],
        ]);
        roundtrip(&g);
        assert_eq!(g.parts().len(), 2);
    }

    #[test]
    fn test_z_polyline_roundtrip() {
        let g = Geometry::Polyline(vec![vec![
            Vertex::new_z(0.0, 0.0, 1.0),
            Vertex::new_z(1.0, 1.0, 2.0),
        ]]);
        let bytes = encode_shape(&g).unwrap();
        assert_eq!(shape_code(&g), ShapeCode::PolyLineZ as i32);
        assert_eq!(bytes.len(), 4 + 32 + 4 + 4 + 4 + 2 * 16 + 2 * 8 + 2 * 8);
        let back = decode_shape(&bytes).unwrap();
        assert!(back.has_z());
        match back {
            Geometry::Polyline(p) => assert_eq!(p[0][0].z, Some(1.0)),
            _ => panic!("类型错误"),
        }
    }

    #[test]
    fn test_null_shape() {
        let bytes = encode_shape(&Geometry::Null).unwrap();
        assert_eq!(bytes.len(), 4);
        assert_eq!(decode_shape(&bytes).unwrap(), Geometry::Null);
    }

    #[test]
    fn test_known_layout_point() {
        // 手写的 point record：type=1, x=1.0, y=2.0
        let mut v = Vec::new();
        v.write_i32::<LittleEndian>(1).unwrap();
        v.write_f64::<LittleEndian>(1.0).unwrap();
        v.write_f64::<LittleEndian>(2.0).unwrap();
        match decode_shape(&v).unwrap() {
            Geometry::Point(p) => {
                assert_eq!(p.x, 1.0);
                assert_eq!(p.y, 2.0);
            }
            _ => panic!("应为点"),
        }
    }

    #[test]
    fn test_wkt_input_then_encode_decode() {
        let g = geometry_from_wkt("POLYGON((0 0, 0 3, 4 3, 4 0, 0 0))").unwrap();
        let bytes = encode_shape(&g).unwrap();
        let back = decode_shape(&bytes).unwrap();
        assert_eq!(back.to_string_check(), g.to_string_check());
    }

    trait Check {
        fn to_string_check(&self) -> String;
    }
    impl Check for Geometry {
        fn to_string_check(&self) -> String {
            format!("{self:?}")
        }
    }

    #[test]
    fn test_four_byte_prefix_skip() {
        let g = Geometry::Point(Vertex::new(3.0, 4.0));
        let body = encode_shape(&g).unwrap();
        let mut blob = Vec::new();
        blob.write_i32::<LittleEndian>(body.len() as i32).unwrap();
        blob.extend_from_slice(&body);
        match decode_shape(&blob).unwrap() {
            Geometry::Point(p) => assert_eq!((p.x, p.y), (3.0, 4.0)),
            _ => panic!("应为点"),
        }
    }
}
