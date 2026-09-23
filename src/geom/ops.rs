//! 几何基础运算：距离、面积、环方向、部件合法性校验。
//!
//! 这些方法用于几何写库前的规范化处理（例如面外环必须顺时针、Z/M 一致性检查），
//! 对应 ArcObjects 中的 `ITopologicalOperator`、`IArea`、`ICurve` 等接口的部分行为。

use super::{Envelope, Geometry, Vertex};

/// 二维距离
pub fn dist2d(a: Vertex, b: Vertex) -> f64 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    (dx * dx + dy * dy).sqrt()
}

/// 三维距离（任一点无 Z 时退化为二维）
pub fn dist3d(a: Vertex, b: Vertex) -> f64 {
    match (a.z, b.z) {
        (Some(za), Some(zb)) => {
            let dz = zb - za;
            (dist2d(a, b).powi(2) + dz * dz).sqrt()
        }
        _ => dist2d(a, b),
    }
}

/// 路径长度（二维）
pub fn path_length(path: &[Vertex]) -> f64 {
    path.windows(2).map(|w| dist2d(w[0], w[1])).sum()
}

/// 环的有符号面积（shoelace 公式）。右手系下正值表示逆时针。
pub fn signed_area(ring: &[Vertex]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        sum += ring[i].x * ring[j].y - ring[j].x * ring[i].y;
    }
    sum / 2.0
}

/// 多环多边形的有符号面积：外环减内环。
pub fn ring_area_signed(rings: &[Vec<Vertex>]) -> f64 {
    let mut total = 0.0;
    for (idx, ring) in rings.iter().enumerate() {
        let a = signed_area(ring).abs();
        // 约定：第一个环为外环，其余为内环
        if idx == 0 {
            total += a;
        } else {
            total -= a;
        }
    }
    total
}

/// 计算几何的包络矩形
pub fn envelope_of(g: &Geometry) -> Option<Envelope> {
    g.envelope()
}

/// 环是否闭合（首尾点 XY 相等）
pub fn is_closed(ring: &[Vertex]) -> bool {
    if ring.len() < 2 {
        return false;
    }
    let a = ring.first().unwrap();
    let b = ring.last().unwrap();
    (a.x - b.x).abs() < 1e-12 && (a.y - b.y).abs() < 1e-12
}

/// 闭合多边形的所有环（缺失闭合点时补齐）
pub fn close_rings(rings: &mut [Vec<Vertex>]) {
    for ring in rings.iter_mut() {
        if !ring.is_empty() && !is_closed(ring) {
            ring.push(*ring.first().unwrap());
        }
    }
}

/// 反转部件顶点顺序
pub fn reverse_path(path: &mut [Vertex]) {
    path.reverse();
}

/// 统一 Polygon 的环方向：外环顺时针（有符号面积 <= 0），内环逆时针。
///
/// ESRI shapefile 规范约定：外环顺时针，内环逆时针。写入前调用可避免
/// ArcMap 中面填充反转或面积符号异常。
pub fn normalize_polygon_orientation(rings: &mut [Vec<Vertex>]) {
    for (idx, ring) in rings.iter_mut().enumerate() {
        let area = signed_area(ring);
        let want_ccw = idx > 0;
        let is_ccw = area > 0.0;
        if area.abs() > 1e-12 && is_ccw != want_ccw {
            reverse_path(ring);
        }
    }
}

/// 移除相邻重复点（避免零长度 segment）
pub fn dedupe_path(path: &mut Vec<Vertex>, tolerance: f64) {
    if path.len() < 2 {
        return;
    }
    let mut out: Vec<Vertex> = Vec::with_capacity(path.len());
    let mut last: Option<Vertex> = None;
    for v in path.iter() {
        if let Some(l) = last {
            if dist2d(l, *v) <= tolerance {
                continue;
            }
        }
        last = Some(*v);
        out.push(*v);
    }
    *path = out;
}

/// 几何合法性检查返回的问题列表
#[derive(Debug, Default)]
pub struct ValidityReport {
    /// 问题描述
    pub problems: Vec<String>,
}

impl ValidityReport {
    /// 是否通过校验
    pub fn is_valid(&self) -> bool {
        self.problems.is_empty()
    }
    /// 追加问题
    pub fn push<S: Into<String>>(&mut self, msg: S) {
        self.problems.push(msg.into());
    }
}

/// 几何合法性校验（`ITopologicalOperator2::IsKnownSimple` 的轻量等价物）
pub fn validate(g: &Geometry) -> ValidityReport {
    let mut r = ValidityReport::default();
    match g {
        Geometry::Null => {}
        Geometry::Point(v) => {
            if !v.x.is_finite() || !v.y.is_finite() {
                r.push("点坐标不是有限数值");
            }
        }
        Geometry::Multipoint(pts) => {
            if pts.is_empty() {
                r.push("多点没有顶点");
            }
        }
        Geometry::Polyline(paths) => {
            if paths.iter().all(|p| p.is_empty()) {
                r.push("线没有任何顶点");
            }
            for (i, p) in paths.iter().enumerate() {
                if p.len() == 1 {
                    r.push(format!("第 {i} 条路径仅有一个顶点，无法构成线段"));
                }
            }
        }
        Geometry::Polygon(rings) => {
            if rings.iter().all(|p| p.is_empty()) {
                r.push("面没有任何环");
            }
            for (i, ring) in rings.iter().enumerate() {
                if ring.len() < 4 {
                    r.push(format!("第 {i} 个环顶点数少于 4"));
                } else if !is_closed(ring) {
                    r.push(format!("第 {i} 个环未闭合"));
                }
            }
        }
        Geometry::Envelope(e) => {
            if !e.max_x.is_finite() || e.is_empty() {
                r.push("矩形范围无效或为空");
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signed_area() {
        let ring = vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(0.0, 1.0),
            Vertex::new(1.0, 1.0),
            Vertex::new(1.0, 0.0),
        ];
        // 顺时针顶点序列的有符号面积为负
        assert!((signed_area(&ring) + 1.0).abs() < 1e-9);
        // 而 Geometry::area() 使用绝对值，始终为正
        assert!((ring_area_signed(&[ring]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_normalize_orientation_makes_outer_cw() {
        let ccw = vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(1.0, 0.0),
            Vertex::new(1.0, 1.0),
            Vertex::new(0.0, 1.0),
            Vertex::new(0.0, 0.0),
        ];
        let mut rings = vec![ccw];
        normalize_polygon_orientation(&mut rings);
        assert!(signed_area(&rings[0]) < 0.0);
    }

    #[test]
    fn test_dedupe() {
        let mut p = vec![
            Vertex::new(0.0, 0.0),
            Vertex::new(0.0, 0.0),
            Vertex::new(1.0, 0.0),
        ];
        dedupe_path(&mut p, 1e-9);
        assert_eq!(p.len(), 2);
    }
}
