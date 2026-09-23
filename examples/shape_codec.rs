//! Shape 二进制编解码：ESRI Personal Geodatabase 的 `Shape` 列到底存了什么。
//!
//! 结论：业务表 `Shape` 列里的字节 就是 **去掉 100 字节文件头的 shapefile 记录体**
//!（小端，`i32` 类型码 + 几何数据）。因此本库编码出的字节可直接塞回 Shape 列，
//! 也能被 GDAL / shapefile 解析器按记录体读取。
//!
//! 运行：
//!
//! ```bash
//! cargo run --example shape_codec
//! ```

use pgdb::geom::codec::{decode_shape, encode_shape, peek_shape_type};
use pgdb::geom::{geometry_from_wkt, AsWkt, Envelope, Geometry, GeometryType, Vertex};

fn show(label: &str, g: &Geometry) -> pgdb::Result<()> {
    let bytes = encode_shape(g)?;
    println!("{label}");
    println!("  WKT            : {}", g.as_wkt());
    println!(
        "  ShapeType 码   : {}（{}）",
        peek_shape_type(&bytes)?,
        g.geometry_type().label()
    );
    println!("  字节长度       : {} 字节", bytes.len());
    println!("  前 32 字节 hex : {}", hex_head(&bytes));
    println!("  包络矩形       : {}", env_text(g.envelope()));
    println!("  Z / M          : {} / {}", g.has_z(), g.has_m());

    // 回读校验必须完全相等
    let back = decode_shape(&bytes)?;
    println!("  回读一致       : {}", back == *g);
    println!();
    Ok(())
}

fn main() -> pgdb::Result<()> {
    pgdb::init_log();

    show("① 点", &Geometry::point(116.3913, 39.9075))?;
    show("② 点（带 Z）", &Geometry::point_z(116.3913, 39.9075, 44.5))?;
    show(
        "③ 多点",
        &Geometry::multipoint(vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(1.0, 1.0),
            Vertex::new(2.0, 0.5),
        ]),
    )?;
    show(
        "④ 单部件线",
        &Geometry::line(vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(10.0, 0.0),
            Vertex::new(10.0, 10.0),
        ]),
    )?;
    show(
        "⑤ 多部件线",
        &Geometry::polyline(vec![
            vec![Vertex::new(0.0, 0.0), Vertex::new(1.0, 1.0)],
            vec![Vertex::new(5.0, 5.0), Vertex::new(6.0, 6.0)],
        ]),
    )?;
    show(
        "⑥ 面（外环 + 内环）",
        &Geometry::polygon(vec![
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
        ]),
    )?;
    show(
        "⑦ 线（带 M）",
        &Geometry::line(vec![
            Vertex::new(0.0, 0.0).with_m(0.0),
            Vertex::new(10.0, 0.0).with_m(12.5),
        ]),
    )?;

    // 从 WKT 读入再编码：CLI 的 set-geometry / create-feature 走的就是这条路
    let g = geometry_from_wkt("POLYGON((0 0, 0 4, 4 4, 4 0, 0 0))")?;
    show("⑧ 由 WKT 构造", &g)?;

    println!("二进制布局示例（⑥ 面）：");
    let bytes = encode_shape(&Geometry::polygon(vec![vec![
        Vertex::new(0.0, 0.0),
        Vertex::new(0.0, 10.0),
        Vertex::new(10.0, 10.0),
        Vertex::new(10.0, 0.0),
        Vertex::new(0.0, 0.0),
    ]]))?;
    println!(
        "  [0..4)   ShapeType   = 5\n  [4..36)  Box         = 4 × f64\n  [36..40) NumParts    = 1\n  \
         [40..44) NumPoints   = 5\n  [44..48) Parts[0]    = 0\n  [48..128) Points     = 5 × (x, y)",
    );
    println!("  实际总长度 = {} 字节（= 48 + 5×16）", bytes.len());
    println!(
        "  {}",
        if bytes.len() == 128 {
            "✓ 与手工计算一致"
        } else {
            "✗ 长度异常"
        }
    );
    let _ = GeometryType::Polygon;
    Ok(())
}

fn env_text(env: Option<Envelope>) -> String {
    match env {
        Some(e) => format!(
            "({:.4}, {:.4}, {:.4}, {:.4})",
            e.min_x, e.min_y, e.max_x, e.max_y
        ),
        None => "-".to_string(),
    }
}

fn hex_head(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(32)
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}
