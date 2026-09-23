//! 过滤器：对应 ArcObjects 的 `IQueryFilter` 与 `ISpatialFilter`。

use crate::datastore::{Predicate, SqlValue};
use crate::geom::Geometry;

/// 空间关系（`esriSpatialRelEnum` 的子集）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialRel {
    /// 相交
    Intersects,
    /// 包含
    Contains,
    /// 被包含
    Within,
    /// 相触
    Touches,
    /// 叠置时取 ==，此处等同相交
    Overlaps,
}

impl SpatialRel {
    /// 中文标签
    pub fn label(&self) -> &'static str {
        match self {
            SpatialRel::Intersects => "相交",
            SpatialRel::Contains => "包含",
            SpatialRel::Within => "被包含",
            SpatialRel::Touches => "接触",
            SpatialRel::Overlaps => "叠置",
        }
    }
}

/// 空间过滤器（`ISpatialFilter`）
#[derive(Debug, Clone)]
pub struct SpatialFilter {
    /// 过滤几何
    pub geometry: Geometry,
    /// 参考字段名（通常为 Shape）
    pub geometry_field: Option<String>,
    /// 空间关系
    pub relation: SpatialRel,
    /// 是否先按包络粗筛（对应 ISpatialFilter::FilterOwnsGeometry 的反面）
    pub use_envelope_shortcut: bool,
}

impl SpatialFilter {
    /// 构造“相交”过滤器
    pub fn intersects(geometry: Geometry) -> Self {
        Self {
            geometry,
            geometry_field: None,
            relation: SpatialRel::Intersects,
            use_envelope_shortcut: true,
        }
    }
}

/// 查询过滤器（`IQueryFilter`）
#[derive(Debug, Clone, Default)]
pub struct QueryFilter {
    /// 返回字段（`SubFields`，为空表示全部字段）
    pub sub_fields: Vec<String>,
    /// WHERE 子句（不含 WHERE 关键字）。出于可移植性考虑，镜像后端会忽略它。
    pub where_clause: Option<String>,
    /// 按 OBJECTID 精确过滤（最常用、且两个后端都支持）
    pub oid: Option<i64>,
    /// 排序子句
    pub order_by: Option<String>,
    /// 空间过滤器
    pub spatial: Option<SpatialFilter>,
}

impl QueryFilter {
    /// 空过滤器（全部记录）
    pub fn new() -> Self {
        Self::default()
    }

    /// 仅返回指定 OBJECTID 的记录
    pub fn for_oid(oid: i64) -> Self {
        Self {
            oid: Some(oid),
            ..Default::default()
        }
    }

    /// 设置 WHERE 子句
    pub fn with_where<S: Into<String>>(mut self, clause: S) -> Self {
        self.where_clause = Some(clause.into());
        self
    }

    /// 设置返回字段
    pub fn with_sub_fields<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.sub_fields = fields.into_iter().map(Into::into).collect();
        self
    }

    /// 追加空间过滤器
    pub fn with_spatial(mut self, filter: SpatialFilter) -> Self {
        self.spatial = Some(filter);
        self
    }

    /// 是否需要将空间过滤降级为内存过滤（判断 lay explain: 后端不支持空间 SQL）
    pub fn needs_memory_spatial_filter(&self) -> bool {
        self.spatial.is_some()
    }
}

impl QueryFilter {
    pub(crate) fn to_predicate(&self, oid_field: &str) -> Predicate {
        let mut pred = Predicate::All;
        if let Some(oid) = self.oid {
            pred = pred.and(Predicate::FieldEq {
                field: oid_field.to_string(),
                value: SqlValue::I64(oid),
            });
        }
        if let Some(clause) = &self.where_clause {
            if !clause.trim().is_empty() {
                pred = pred.and(Predicate::Raw(clause.clone()));
            }
        }
        pred
    }
}
