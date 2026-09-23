//! 几何模型：对应 ArcObjects 中的 `IPoint / IPointCollection / IPolyline / IPolygon / IEnvelope`。
//!
//! 本模块与存储格式无关，几何的二进制序列化见 [`crate::geom::codec`]。

pub mod codec;
pub mod ops;

use crate::error::{PgdbError, Result};

/// 二维/三维顶点。Z、M 为可选值，对应 Whether the geometry is Z-aware / M-aware。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    /// X 坐标（通常东/经）
    pub x: f64,
    /// Y 坐标（通常北/纬）
    pub y: f64,
    /// Z 值（高程）
    pub z: Option<f64>,
    /// M 值（线性参考度量值）
    pub m: Option<f64>,
}

impl Vertex {
    /// 构造二维顶点。
    pub fn new(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            z: None,
            m: None,
        }
    }
    /// 构造三维顶点。
    pub fn new_z(x: f64, y: f64, z: f64) -> Self {
        Self {
            x,
            y,
            z: Some(z),
            m: None,
        }
    }
    /// 构造带 M 值的顶点。
    pub fn new_m(x: f64, y: f64, m: f64) -> Self {
        Self {
            x,
            y,
            z: None,
            m: Some(m),
        }
    }
    /// 就地追加 Z 值（链式构造）
    pub fn with_z(mut self, z: f64) -> Self {
        self.z = Some(z);
        self
    }
    /// 就地追加 M 值（链式构造）
    pub fn with_m(mut self, m: f64) -> Self {
        self.m = Some(m);
        self
    }
}

/// 路径 / 环：一组有序顶点。等价于 `IPointCollection`。
pub type Path = Vec<Vertex>;

/// 矩形范围，对应 `IEnvelope`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Envelope {
    /// 最小 X
    pub min_x: f64,
    /// 最小 Y
    pub min_y: f64,
    /// 最大 X
    pub max_x: f64,
    /// 最大 Y
    pub max_y: f64,
}

impl Envelope {
    /// 构造范围（自动纠正反向输入）。
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x: min_x.min(max_x),
            min_y: min_y.min(max_y),
            max_x: min_x.max(max_x),
            max_y: min_y.max(max_y),
        }
    }

    /// 是否为空（`IEnvelope::IsEmpty`）
    /// 是否为空（`IEnvelope::IsEmpty`）。NaNs 视为不可用包络，按空处理。
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    pub fn is_empty(&self) -> bool {
        !(self.max_x > self.min_x) && !(self.max_y > self.min_y)
    }

    /// 宽度
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    /// 高度
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    /// 并集扩展（空间中 Union）
    pub fn union(&self, other: &Envelope) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    /// 点是否落在范围内（含边界）
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    /// 是否与另一范围相交
    pub fn intersects(&self, other: &Envelope) -> bool {
        !(other.max_x < self.min_x
            || other.max_y < self.min_y
            || other.min_x > self.max_x
            || other.min_y > self.max_y)
    }

    /// 是否完全包含另一范围
    pub fn contains_envelope(&self, other: &Envelope) -> bool {
        other.min_x >= self.min_x
            && other.max_x <= self.max_x
            && other.min_y >= self.min_y
            && other.max_y <= self.max_y
    }
}

/// ESRI 几何类型（对应 GDB_GeomColumns.ShapeType 与 `esriGeometryType`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeometryType {
    /// 空几何
    Null = 0,
    /// 点
    Point = 1,
    /// 多点
    Multipoint = 2,
    /// 线
    Polyline = 3,
    /// 面
    Polygon = 4,
    /// 包络矩形
    Envelope = 5,
    /// 任意类型
    Any = 6,
    /// 多面体
    Multipatch = 9,
}

impl GeometryType {
    /// 由 ESRI ShapeType（GDB_GeomColumns.ShapeType）整数构造
    pub fn from_shape_type(v: i32) -> Result<Self> {
        Ok(match v {
            0 => GeometryType::Null,
            1 => GeometryType::Point,
            2 => GeometryType::Multipoint,
            3 => GeometryType::Polyline,
            4 => GeometryType::Polygon,
            5 => GeometryType::Envelope,
            6 => GeometryType::Any,
            9 => GeometryType::Multipatch,
            other => {
                return Err(PgdbError::Unsupported(format!(
                    "未知的 esriGeometryType: {other}"
                )))
            }
        })
    }

    /// 转为 ESRI ShapeType 整数
    pub fn as_shape_type(self) -> i32 {
        self as i32
    }

    /// 中文名称，便于日志与 CLI 输出
    pub fn label(self) -> &'static str {
        match self {
            GeometryType::Null => "空",
            GeometryType::Point => "点",
            GeometryType::Multipoint => "多点",
            GeometryType::Polyline => "线",
            GeometryType::Polygon => "面",
            GeometryType::Envelope => "矩形",
            GeometryType::Any => "任意",
            GeometryType::Multipatch => "多面体",
        }
    }
}

/// 几何体（对应 `IGeometry`）。
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    /// 空几何
    Null,
    /// 点
    Point(Vertex),
    /// 多点
    Multipoint(Vec<Vertex>),
    /// 线：一条或多条路径
    Polyline(Vec<Path>),
    /// 面：一个或多个环（首环为外环）
    Polygon(Vec<Path>),
    /// 包络矩形
    Envelope(Envelope),
}

/// Shapefile/PGDB 二进制记录的几何类型码
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ShapeCode {
    /// 空
    NullShape = 0,
    /// 点
    Point = 1,
    /// 线
    PolyLine = 3,
    /// 面
    Polygon = 5,
    /// 多点
    MultiPoint = 8,
    /// 点 Z
    PointZ = 11,
    /// 线 Z
    PolyLineZ = 13,
    /// 面 Z
    PolygonZ = 15,
    /// 多点 Z
    MultiPointZ = 18,
    /// 点 M
    PointM = 21,
    /// 线 M
    PolyLineM = 23,
    /// 面 M
    PolygonM = 25,
    /// 多点 M
    MultiPointM = 28,
    /// 多面体
    Multipatch = 31,
}

impl ShapeCode {
    /// 由整数构造
    pub fn try_from_i32(v: i32) -> Option<Self> {
        Some(match v {
            0 => ShapeCode::NullShape,
            1 => ShapeCode::Point,
            3 => ShapeCode::PolyLine,
            5 => ShapeCode::Polygon,
            8 => ShapeCode::MultiPoint,
            11 => ShapeCode::PointZ,
            13 => ShapeCode::PolyLineZ,
            15 => ShapeCode::PolygonZ,
            18 => ShapeCode::MultiPointZ,
            21 => ShapeCode::PointM,
            23 => ShapeCode::PolyLineM,
            25 => ShapeCode::PolygonM,
            28 => ShapeCode::MultiPointM,
            31 => ShapeCode::Multipatch,
            _ => return None,
        })
    }
}

impl Geometry {
    /// 构造点（`IPoint`）
    pub fn point(x: f64, y: f64) -> Self {
        Geometry::Point(Vertex::new(x, y))
    }

    /// 构造带 Z 的点（`IPoint` + `IZ`）
    pub fn point_z(x: f64, y: f64, z: f64) -> Self {
        Geometry::Point(Vertex::new_z(x, y, z))
    }

    /// 构造多点（`IMultipoint`）
    pub fn multipoint(points: Vec<Vertex>) -> Self {
        Geometry::Multipoint(points)
    }

    /// 构造单部件的线（`IPolyline`）
    pub fn line(path: Path) -> Self {
        Geometry::Polyline(vec![path])
    }

    /// 构造多部件线（`IPolyline`）
    pub fn polyline(paths: Vec<Path>) -> Self {
        Geometry::Polyline(paths)
    }

    /// 构造单环面（`IPolygon`）
    pub fn ring(ring: Path) -> Self {
        Geometry::Polygon(vec![ring])
    }

    /// 构造多环面（`IPolygon`，第一个环为外环）
    pub fn polygon(rings: Vec<Path>) -> Self {
        Geometry::Polygon(rings)
    }

    /// 由包络矩形构造（`IEnvelope` -> `IGeometry`）
    pub fn from_envelope(env: Envelope) -> Self {
        Geometry::Envelope(env)
    }

    /// 几何是否为空（`IGeometry::IsEmpty`）
    pub fn is_empty_geometry(&self) -> bool {
        match self {
            Geometry::Null => true,
            Geometry::Multipoint(p) => p.is_empty(),
            Geometry::Polyline(p) => p.iter().all(|x| x.is_empty()),
            Geometry::Polygon(p) => p.iter().all(|x| x.is_empty()),
            _ => false,
        }
    }

    /// ESRI 几何类型（`IGeometry::GeometryType`）
    pub fn geometry_type(&self) -> GeometryType {
        match self {
            Geometry::Null => GeometryType::Null,
            Geometry::Point(_) => GeometryType::Point,
            Geometry::Multipoint(_) => GeometryType::Multipoint,
            Geometry::Polyline(_) => GeometryType::Polyline,
            Geometry::Polygon(_) => GeometryType::Polygon,
            Geometry::Envelope(_) => GeometryType::Envelope,
        }
    }

    /// 包络矩形（`IGeometry::Envelope`）
    pub fn envelope(&self) -> Option<Envelope> {
        match self {
            Geometry::Null => None,
            Geometry::Point(p) => Some(Envelope::new(p.x, p.y, p.x, p.y)),
            Geometry::Envelope(e) => Some(*e),
            _ => {
                let mut env: Option<Envelope> = None;
                for p in self.parts() {
                    for v in p {
                        env = Some(match env {
                            None => Envelope::new(v.x, v.y, v.x, v.y),
                            Some(e) => {
                                Envelope::new(e.min_x.min(v.x), e.min_y.min(v.y), e.max_x.max(v.x), e.max_y.max(v.y))
                            }
                        });
                    }
                }
                env
            }
        }
    }

    /// 所有部件（针对线/面/多点返回各部件；点返回空）
    pub fn parts(&self) -> Vec<&Path> {
        match self {
            Geometry::Multipoint(p) => vec![p],
            Geometry::Polyline(p) => p.iter().collect(),
            Geometry::Polygon(p) => p.iter().collect(),
            _ => Vec::new(),
        }
    }

    /// 顶点总数（`IPointCollection::PointCount`）
    pub fn point_count(&self) -> usize {
        match self {
            Geometry::Null => 0,
            Geometry::Point(_) => 1,
            Geometry::Envelope(_) => 4,
            other => other.parts().iter().map(|p| p.len()).sum(),
        }
    }

    /// 是否含有 Z 值
    pub fn has_z(&self) -> bool {
        match self {
            Geometry::Point(v) => v.z.is_some(),
            Geometry::Envelope(_) | Geometry::Null => false,
            other => other.parts().iter().any(|p| p.iter().any(|v| v.z.is_some())),
        }
    }

    /// 是否含有 M 值
    pub fn has_m(&self) -> bool {
        match self {
            Geometry::Point(v) => v.m.is_some(),
            Geometry::Envelope(_) | Geometry::Null => false,
            other => other.parts().iter().any(|p| p.iter().any(|v| v.m.is_some())),
        }
    }

    /// 长度（`IPolyline::Length`），非线类型返回 0
    pub fn length(&self) -> f64 {
        let mut total = 0.0;
        for part in self.parts() {
            for pair in part.windows(2) {
                total += ops::dist2d(pair[0], pair[1]);
            }
        }
        total
    }

    /// 面积（`IArea::Area`），非面类型返回 0
    pub fn area(&self) -> f64 {
        let Geometry::Polygon(rings) = self else {
            return 0.0;
        };
        ops::ring_area_signed(rings)
    }

    /// 转为二进制字符串的人类可读形式（调试）
    pub fn brief(&self) -> String {
        match self {
            Geometry::Null => "Null".into(),
            Geometry::Point(v) => format!("Point({}, {})", v.x, v.y),
            other => format!(
                "{:?} parts={} points={}",
                other.geometry_type(),
                other.parts().len(),
                other.point_count()
            ),
        }
    }
}

/// WKT 输出（OGC Simple Feature 风格，含 Z 时输出 POINT Z 等形式）
pub trait AsWkt {
    /// 输出 WKT 文本
    fn as_wkt(&self) -> String;
    /// 输出简化 WKT（保留小数位）
    fn as_wkt_precision(&self, decimals: usize) -> String;
}

/// 格式化浮点并去掉多余的零
fn fmt_f(v: f64, decimals: usize) -> String {
    let s = format!("{v:.*}", decimals);
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

impl AsWkt for Envelope {
    fn as_wkt(&self) -> String {
        self.as_wkt_precision(10)
    }
    fn as_wkt_precision(&self, decimals: usize) -> String {
        format!(
            "ENVELOPE({} {} , {} {})",
            fmt_f(self.min_x, decimals),
            fmt_f(self.min_y, decimals),
            fmt_f(self.max_x, decimals),
            fmt_f(self.max_y, decimals)
        )
    }
}

impl AsWkt for Geometry {
    fn as_wkt(&self) -> String {
        self.as_wkt_precision(10)
    }

    fn as_wkt_precision(&self, decimals: usize) -> String {
        let p = decimals;
        let sfx = if self.has_z() { "Z" } else { "" };
        let coord = |v: &Vertex| -> String {
            if self.has_z() {
                format!(
                    "{} {} {}",
                    fmt_f(v.x, p),
                    fmt_f(v.y, p),
                    fmt_f(v.z.unwrap_or(0.0), p)
                )
            } else {
                format!("{} {}", fmt_f(v.x, p), fmt_f(v.y, p))
            }
        };
        let ring = |path: &Path| -> String {
            let pts: Vec<String> = path.iter().map(&coord).collect();
            format!("({})", pts.join(", "))
        };
        match self {
            Geometry::Null => "GEOMETRYCOLLECTION EMPTY".into(),
            Geometry::Point(v) => format!("POINT{sfx}({})", coord(v)),
            Geometry::Multipoint(pts) => {
                let items: Vec<String> = pts.iter().map(&coord).collect();
                format!("MULTIPOINT{sfx}({})", items.join(", "))
            }
            Geometry::Polyline(paths) => {
                let items: Vec<String> = paths.iter().map(&ring).collect();
                // 单部件输出 LINESTRING，多部件输出 MULTILINESTRING（OGC 习惯）
                if items.len() == 1 {
                    format!("LINESTRING{sfx}{}", items[0])
                } else {
                    format!("MULTILINESTRING{sfx}({})", items.join(", "))
                }
            }
            // 多个环属于同一个面：外环在前，其余为内环/其他部件（OGC 的 POLYGON）
            Geometry::Polygon(rings) => {
                let items: Vec<String> = rings.iter().map(&ring).collect();
                format!("POLYGON{sfx}({})", items.join(", "))
            }
            Geometry::Envelope(e) => e.as_wkt_precision(p),
        }
    }
}

/// 由 WKT 文本解析几何。
///
/// 支持 `POINT`、`LINESTRING`、`POLYGON`、`MULTIPOINT`、`MULTILINESTRING`，
/// 以及含 Z 的形式（例如 `POINT Z (1 2 3)`）。
pub fn geometry_from_wkt(s: &str) -> Result<Geometry> {
    let s = s.trim();
    let outer_start = s.find('(').unwrap_or(0);
    let outer = &s[outer_start + 1..s.rfind(')').unwrap_or(s.len())];
    let kind = s
        .split_once('(')
        .map(|(k, _)| k.trim().to_ascii_uppercase())
        .unwrap_or_else(|| s.to_ascii_uppercase())
        .replace(' ', ""); // `POINT Z (...)` -> `POINTZ(...)`，便于统一判断维度
    // 只有明确带 Z 后缀时才按三维解析（MULTI* 里的 M 不算）
    let dim = if kind.contains('Z') { 3 } else { 2 };
    match kind.as_str() {
        k if k.starts_with("POINT") => {
            let mut nums = parse_numbers(&strip_parens(outer))?;
            if nums.len() < 2 {
                return Err(PgdbError::Unsupported("POINT 坐标不足".into()));
            }
            if nums.len() == 3 && dim == 2 {
                nums.truncate(2); // `POINT(x y)` 形式的多余分量
            }
            let mut v = Vertex::new(nums[0], nums[1]);
            if nums.len() >= 3 {
                v.z = Some(nums[2]);
            }
            Ok(Geometry::Point(v))
        }
        k if k.starts_with("LINESTRING") || k.starts_with("MULTILINESTRING") => {
            Ok(Geometry::Polyline(parse_path_list(outer, dim)?))
        }
        k if k.starts_with("POLYGON") || k.starts_with("MULTIPOLYGON") => {
            Ok(Geometry::Polygon(parse_ring_list(outer, dim)?))
        }
        k if k.starts_with("MULTIPOINT") => {
            // 兼容 `MULTIPOINT(0 0, 1 1)` 与 `MULTIPOINT((0 0), (1 1))`
            let cleaned: String = outer.replace(['(', ')'], " ");
            let nums = parse_numbers(&cleaned)?;
            let mut pts = Vec::new();
            for chunk in nums.chunks(dim) {
                if chunk.len() < 2 {
                    return Err(PgdbError::Unsupported(format!("MULTIPOINT 坐标不合法: {outer}")));
                }
                let mut v = Vertex::new(chunk[0], chunk[1]);
                if chunk.len() >= 3 {
                    v.z = Some(chunk[2]);
                }
                pts.push(v);
            }
            Ok(Geometry::Multipoint(pts))
        }
        other => Err(PgdbError::Unsupported(format!(
            "暂不支持解析的 WKT 类型: {other}"
        ))),
    }
}

/// 解析线对象的部件列表。
///
/// 同时兼容 OGC 标准的 `MULTILINESTRING((0 0, 1 1), (2 2, 3 3))`
/// 与省略一层括号的 `LINESTRING(0 0, 1 1)`。
fn parse_path_list(body: &str, dim: usize) -> Result<Vec<Path>> {
    let segs = top_level_parts(body);
    if segs.is_empty() {
        return Ok(Vec::new());
    }
    if segs.len() == 1 {
        return Ok(vec![parse_coord_list(&strip_parens(&segs[0]), dim)?]);
    }
    // 没有任何括号分组 -> 逗号只是坐标分隔符，整体是一条路径
    if !segs.iter().any(|s| s.starts_with('(')) {
        return Ok(vec![parse_coord_list(body, dim)?]);
    }
    segs.iter()
        .map(|seg| parse_coord_list(&strip_parens(seg), dim))
        .collect()
}

/// 解析面对象的环列表。
///
/// 支持：
/// - `POLYGON((0 0, 1 1, 1 0, 0 0))`
/// - `POLYGON((外环...), (内环...))`
/// - `MULTIPOLYGON(((外环), (内环)), ((另一个面)))`
fn parse_ring_list(body: &str, dim: usize) -> Result<Vec<Path>> {
    let segs = top_level_parts(body);
    if segs.is_empty() {
        return Ok(Vec::new());
    }
    // 完全没有括号分组 -> 整段是一个环
    if !segs.iter().any(|s| s.starts_with('(')) {
        return Ok(vec![parse_coord_list(body, dim)?]);
    }
    let mut rings = Vec::new();
    for seg in segs {
        let inner = strip_parens(&seg);
        // 内部还有分组 -> 标准 MULTIPOLYGON 的一个多边形，递归展开
        if top_level_parts(&inner).iter().any(|s| s.starts_with('(')) {
            rings.extend(parse_ring_list(&inner, dim)?);
        } else {
            rings.push(parse_coord_list(&inner, dim)?);
        }
    }
    Ok(rings)
}

/// 去掉字符串外围的一对括号
fn strip_parens(s: &str) -> String {
    let t = s.trim();
    if t.starts_with('(') && t.ends_with(')') {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// 解析 "x y" / "x y z" 形式的数字
fn parse_numbers(s: &str) -> Result<Vec<f64>> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(|t| {
            t.trim().parse::<f64>().map_err(|e| {
                PgdbError::Unsupported(format!("坐标解析失败 '{t}': {e}"))
            })
        })
        .collect()
}

/// 解析 "x y, x y" 形式的坐标列表
fn parse_coord_list(inner: &str, dim: usize) -> Result<Path> {
    let nums = parse_numbers(inner)?;
    if nums.is_empty() || nums.len() % dim != 0 {
        return Err(PgdbError::Unsupported(format!(
            "坐标列表不合法: {inner}"
        )));
    }
    let mut out = Path::new();
    for chunk in nums.chunks(dim) {
        let mut v = Vertex::new(chunk[0], chunk[1]);
        if chunk.len() >= 3 {
            v.z = Some(chunk[2]);
        }
        out.push(v);
    }
    Ok(out)
}

/// 按最外层逗号切分，**保留**每段的括号。
///
/// 例如 `((A),(B)),((C))` -> `["((A),(B))", "((C))"]`，`0 0, 1 1` -> `["0 0", "1 1"]`，
/// 这样上层就能区分"坐标分隔符"与"部件/环分组"。
fn top_level_parts(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_union() {
        let a = Envelope::new(0.0, 0.0, 10.0, 10.0);
        let b = Envelope::new(5.0, -5.0, 15.0, 5.0);
        let u = a.union(&b);
        assert_eq!(u.min_x, 0.0);
        assert_eq!(u.max_x, 15.0);
        assert!(a.intersects(&b));
        assert!(!a.contains_envelope(&b));
    }

    #[test]
    fn test_wkt_roundtrip() {
        let g = Geometry::Polyline(vec![vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(1.0, 1.0),
            Vertex::new(2.0, 0.0),
        ]]);
        assert_eq!(
            g.as_wkt_precision(3),
            "LINESTRING(0 0, 1 1, 2 0)".to_string()
        );
        let parsed = geometry_from_wkt("LINESTRING(0 0, 1 1, 2 0)").unwrap();
        assert_eq!(parsed, g);
    }

    #[test]
    fn test_polygon_wkt() {
        let g = Geometry::Polygon(vec![vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(0.0, 1.0),
            Vertex::new(1.0, 1.0),
            Vertex::new(1.0, 0.0),
            Vertex::new(0.0, 0.0),
        ]]);
        let parsed = geometry_from_wkt(&g.as_wkt()).unwrap();
        assert_eq!(parsed, g);
    }

    #[test]
    fn test_wkt_output_is_ogc_style() {
        let single = Geometry::Polygon(vec![vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(4.0, 0.0),
            Vertex::new(4.0, 4.0),
            Vertex::new(0.0, 0.0),
        ]]);
        assert_eq!(
            single.as_wkt_precision(0),
            "POLYGON((0 0, 4 0, 4 4, 0 0))".to_string()
        );
        let with_hole = Geometry::Polygon(vec![
            vec![
                Vertex::new(0.0, 0.0),
                Vertex::new(10.0, 0.0),
                Vertex::new(10.0, 10.0),
                Vertex::new(0.0, 0.0),
            ],
            vec![
                Vertex::new(2.0, 2.0),
                Vertex::new(8.0, 2.0),
                Vertex::new(8.0, 8.0),
                Vertex::new(2.0, 2.0),
            ],
        ]);
        let wkt = with_hole.as_wkt_precision(0);
        assert!(wkt.starts_with("POLYGON(("), "{wkt}");
        assert_eq!(with_hole, geometry_from_wkt(&wkt).unwrap());

        let mp = Geometry::multipoint(vec![Vertex::new(0.0, 0.0), Vertex::new(1.0, 1.0)]);
        assert_eq!(mp.as_wkt_precision(0), "MULTIPOINT(0 0, 1 1)".to_string());
        assert_eq!(mp, geometry_from_wkt("MULTIPOINT(0 0, 1 1)").unwrap());
        assert_eq!(mp, geometry_from_wkt("MULTIPOINT((0 0), (1 1))").unwrap());

        let pz = Geometry::point_z(1.0, 2.0, 3.0);
        assert_eq!(pz.as_wkt_precision(0), "POINTZ(1 2 3)".to_string());
        assert_eq!(pz, geometry_from_wkt("POINT Z (1 2 3)").unwrap());
        assert_eq!(pz, geometry_from_wkt("POINTZ(1 2 3)").unwrap());
    }

    #[test]
    fn test_wkt_parser_accepts_standard_forms() {
        // 标准 Nested MULTIPOLYGON
        let std = "MULTIPOLYGON(((0 0, 4 0, 4 4, 0 0)), ((5 5, 6 5, 6 6, 5 5)))";
        let parsed = geometry_from_wkt(std).unwrap();
        match parsed {
            Geometry::Polygon(ref rings) => assert_eq!(rings.len(), 2),
            other => panic!("应为面: {other:?}"),
        }
        // 标准 MULTILINESTRING
        let line = geometry_from_wkt("MULTILINESTRING((0 0, 1 1), (2 2, 3 3))").unwrap();
        match line {
            Geometry::Polyline(ref paths) => assert_eq!(paths.len(), 2),
            other => panic!("应为多部件线: {other:?}"),
        }
        // 带 Z 的线
        let z_line = geometry_from_wkt("LINESTRING Z (0 0 1, 1 1 2)").unwrap();
        assert!(z_line.has_z());
        assert_eq!(z_line.parts()[0][1].z, Some(2.0));
    }
}
