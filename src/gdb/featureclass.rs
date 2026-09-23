//! 要素类：`IFeatureClass` 的 Rust 版本，包含几何写入时的 ESRI 一致性维护。

use std::sync::Arc;

use crate::datastore::{Predicate, SqlBackend, SqlValue};
use crate::error::Result;
use crate::field::{Field, Fields};
use crate::geom::codec::{decode_shape, encode_shape};
use crate::geom::{Envelope, Geometry, GeometryType};
use crate::value::Value;

use super::cursor::{FeatureIter, InsertFeatureCursor, InsertRowCursor, RowIter};
use super::dataset::DatasetNode;
use super::filter::QueryFilter;
use super::metadata::{CatalogEntry, DatasetKind, FeatureType, GridInfo, SpatialReference};
use super::row::{Feature, Row, RowBuffer};
use super::table::{Table, TableCore};

/// 写入策略：控制几何更新时是否同步维护 ESRI 依赖表。
///
/// 参考 ArcMap 的实际行为：
/// - 几何变化后必须重算 `Shape_Length` / `Shape_Area`（若字段存在）；
/// - 必须同步 `<业务表>_SHAPE_Index` 的网格记录，否则属性表有数据、地图却无法显示/选中；
/// - `GDB_GeomColumns` 的图层范围用于图层缩放范围，建议批量编辑后统一调用
///   [`crate::gdb::featureclass::FeatureClass::refresh_layer_extent`] 重算。
#[derive(Debug, Clone)]
pub struct WritePolicy {
    /// 自动维护 Shape_Length / Shape_Area
    pub maintain_shape_length_area: bool,
    /// 自动维护 &lt;表&gt;_SHAPE_Index 空间索引记录
    pub maintain_shape_index: bool,
    /// 几何变化后立即扩展 GDB_GeomColumns 的图层范围
    pub maintain_layer_extent: bool,
    /// 写入面要素前统一环方向（外环顺时针）
    pub normalize_polygon_orientation: bool,
    /// GDB_GeomColumns 未给出网格尺寸时使用的默认值
    pub default_grid_size: f64,
    /// 空间索引表名后缀（Access 中通常为 `_SHAPE_Index`）
    pub shape_index_suffix: String,
}

impl Default for WritePolicy {
    fn default() -> Self {
        Self {
            maintain_shape_length_area: true,
            maintain_shape_index: true,
            maintain_layer_extent: true,
            normalize_polygon_orientation: true,
            default_grid_size: 420.0,
            shape_index_suffix: "_SHAPE_Index".to_string(),
        }
    }
}

/// 要素类接口（`IFeatureClass`）
pub trait FeatureClass: Table {
    /// 几何字段名（`IFeatureClass::ShapeFieldName`）
    fn shape_field_name(&self) -> &str;

    /// 向上取出 Table 视图，便于以统一的 Table 视角操作同一对象
    fn table(&self) -> &dyn Table;


    /// 几何类型（`IFeatureClass::ShapeType`）
    fn shape_type(&self) -> GeometryType;

    /// 要素类型（`IFeatureClass::FeatureType`）
    fn feature_type(&self) -> FeatureType;

    /// 空间参考（`IGeoDataset::SpatialReference`）
    fn spatial_reference(&self) -> Option<&SpatialReference>;

    /// 图层范围（`IGeoDataset::Extent`）
    fn extent(&self) -> Option<Envelope>;

    /// 空间索引网格参数
    fn grid_info(&self) -> GridInfo;

    /// 长度字段名
    fn length_field_name(&self) -> Option<&str>;

    /// 面积字段名
    fn area_field_name(&self) -> Option<&str>;

    /// ESRI 空间索引表名（`<业务表>_SHAPE_Index`），不存在时返回 None
    fn shape_index_table(&self) -> Option<&str> {
        None
    }

    /// 查询要素游标（`IFeatureClass::Search`）
    fn search_features(&self, filter: QueryFilter) -> Result<FeatureIter<'_>>;

    /// 更新要素游标（`IFeatureClass::Update`）
    fn update_features(&self, filter: QueryFilter) -> Result<FeatureIter<'_>>;

    /// 插入要素游标（`IFeatureClass::Insert`）
    fn insert_feature_cursor(&self) -> Result<InsertFeatureCursor<'_>>;

    /// 按 OBJECTID 取要素（`IFeatureClass::GetFeature`）
    fn get_feature(&self, oid: i64) -> Result<Option<Feature<'_>>>;

    /// 创建插入缓冲区（`IFeatureClass::CreateFeatureBuffer`）
    fn create_feature_buffer(&self) -> RowBuffer;

    /// 插入要素
    fn insert_feature(&self, values: &[(String, Value)]) -> Result<i64>;

    /// 当前的写入策略
    fn write_policy(&self) -> &WritePolicy;

    /// 全表重算图层范围并写回 GDB_GeomColumns
    fn refresh_layer_extent(&self) -> Result<Envelope>;

    /// 重建 &lt;表&gt;_SHAPE_Index
    fn rebuild_shape_index(&self) -> Result<usize>;
}

/// Personal Geodatabase 中的要素类
pub struct PgdbFeatureClass {
    core: TableCore,
    shape_field: String,
    shape_index: usize,
    shape_type: GeometryType,
    feature_type: FeatureType,
    srid: Option<i64>,
    spatial_ref: Option<SpatialReference>,
    extent: Option<Envelope>,
    grid: GridInfo,
    length_field: Option<String>,
    area_field: Option<String>,
    shape_index_table: Option<String>,
    policy: WritePolicy,
}

impl PgdbFeatureClass {
    /// 打开目录条目对应的要素类
    pub fn open(
        backend: Arc<dyn SqlBackend>,
        entry: &CatalogEntry,
        spatial_ref: Option<SpatialReference>,
        aliases: Option<&std::collections::HashMap<String, String>>,
        policy: WritePolicy,
    ) -> Result<Self> {
        // 没有显式 Shape 字段时，尝试常见的 Shape 列名
        let shape_field = entry
            .shape_field
            .clone()
            .unwrap_or_else(|| default_shape_field(&backend, &entry.table_name));
        let core = TableCore::open(backend.clone(), entry, aliases, None)?;
        let shape_index = core
            .fields
            .require_field(&shape_field)
            .map_err(|_| crate::error::PgdbError::Metadata(format!(
                "要素类 {} 缺少几何字段 {}",
                entry.table_name, shape_field
            )))?;
        let length_field = find_numeric_field(&core.fields, "Shape_Length");
        let area_field = find_numeric_field(&core.fields, "Shape_Area");
        let shape_index_table = detect_shape_index_table(&backend, &entry.table_name, &policy);
        let grid = entry.grid.unwrap_or(GridInfo {
            origin_x: 0.0,
            origin_y: 0.0,
            grid_size: policy.default_grid_size,
        });
        Ok(Self {
            core,
            shape_field,
            shape_index,
            shape_type: entry.shape_type.unwrap_or(GeometryType::Null),
            feature_type: entry.feature_type,
            srid: entry.srid,
            spatial_ref,
            extent: entry.extent,
            grid,
            length_field,
            area_field,
            shape_index_table,
            policy,
        })
    }

    /// 内部：识别几何变更后需要一并写入的 Shape_Length / Shape_Area
    fn measurement_updates(&self, geometry: &Geometry) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        if !self.policy.maintain_shape_length_area {
            return out;
        }
        if let Some(f) = &self.length_field {
            out.push((f.clone(), Value::Double(geometry.length())));
        }
        if let Some(f) = &self.area_field {
            out.push((f.clone(), Value::Double(geometry.area())));
        }
        out
    }

    /// 内部：从几何申请 — Shape 列的二进制值
    fn decode_values(&self, values: &[(String, Value)]) -> Option<Result<crate::geom::Geometry>> {
        values
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(&self.shape_field))
            .map(|(_, v)| match v {
                Value::Blob(b) => decode_shape(b),
                Value::Null => Ok(Geometry::Null),
                other => Err(crate::error::PgdbError::Conversion {
                    field: self.shape_field.clone(),
                    message: format!("几何值不是二进制: {other}"),
                }),
            })
    }

    /// 内部：同步 <表>_SHAPE_Index 的网格记录
    fn sync_shape_index(&self, oid: i64, env: Option<Envelope>) -> Result<()> {
        if !self.policy.maintain_shape_index {
            return Ok(());
        }
        let Some(table) = &self.shape_index_table else {
            return Ok(());
        };
        let backend = &self.core.backend;
        let id_col = shape_index_id_column(backend, table).unwrap_or_else(|| "IndexedObjectId".into());
        let _ = backend.delete(table, &Predicate::eq(id_col.clone(), oid))?;
        let Some(env) = env else {
            return Ok(());
        };
        let g = self.grid;
        let size = if g.grid_size > 0.0 { g.grid_size } else { self.policy.default_grid_size };
        let (min_gx, min_gy, max_gx, max_gy) = grid_cells(&env, g.origin_x, g.origin_y, size);
        backend.insert(
            table,
            &[
                (id_col, SqlValue::I64(oid)),
                ("MinGX".into(), SqlValue::F64(min_gx)),
                ("MinGY".into(), SqlValue::F64(min_gy)),
                ("MaxGX".into(), SqlValue::F64(max_gx)),
                ("MaxGY".into(), SqlValue::F64(max_gy)),
            ],
        )?;
        Ok(())
    }

    /// 内部：把新几何的范围并入 GDB_GeomColumns 的图层范围
    fn expand_layer_extent(&self, env: &Envelope) -> Result<()> {
        if !self.policy.maintain_layer_extent {
            return Ok(());
        }
        let table = &self.core.table_name;
        update_geom_columns_extent(&self.core.backend, table, env)
    }

    /// 写入前的几何规范化（环方向、闭合），并把结果写回 sets 中的 Shape 列
    fn normalize_in_place(
        &self,
        sets: &mut [(String, Value)],
        geometry: Geometry,
    ) -> Result<Geometry> {
        let normalized = match &geometry {
            Geometry::Polygon(_) if self.policy.normalize_polygon_orientation => {
                let normalized = self.normalize_if_needed(&geometry);
                // 把规范化后的几何重新编码写回 Shape 列，保证落库的就是最终几何
                let bytes = encode_shape(&normalized)?;
                if let Some((_, v)) = sets
                    .iter_mut()
                    .find(|(n, _)| n.eq_ignore_ascii_case(&self.shape_field))
                {
                    *v = Value::Blob(bytes);
                }
                normalized
            }
            _ => geometry,
        };
        Ok(normalized)
    }

    /// 写入前的几何规范化（环方向等）
    fn normalize_if_needed(&self, geometry: &Geometry) -> Geometry {
        match geometry {
            Geometry::Polygon(rings) if self.policy.normalize_polygon_orientation => {
                let mut rings = rings.clone();
                crate::geom::ops::normalize_polygon_orientation(&mut rings);
                crate::geom::ops::close_rings(&mut rings);
                Geometry::Polygon(rings)
            }
            other => other.clone(),
        }
    }

    /// 全部几何（用于范围重算与空间索引重建）
    fn scan_feature_envelopes(&self) -> Result<Vec<(i64, Option<Envelope>)>> {
        let fields = vec![
            self.core.oid_field.clone(),
            self.shape_field.clone(),
        ];
        let rows = self
            .core
            .backend
            .select(&self.core.table_name, &fields, &Predicate::All)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let oid = r
                .get(&self.core.oid_field)
                .and_then(|v| v.to_i64())
                .unwrap_or(0);
            let env = match r.get(&self.shape_field).and_then(|v| v.to_binary()) {
                Some(b) => decode_shape(b).ok().and_then(|g| g.envelope()),
                None => None,
            };
            out.push((oid, env));
        }
        Ok(out)
    }

    /// 核心状态
    pub fn core(&self) -> &TableCore {
        &self.core
    }

    /// SRID
    pub fn srid(&self) -> Option<i64> {
        self.srid
    }

    /// 空间索引表名
    pub fn shape_index_table(&self) -> Option<&str> {
        self.shape_index_table.as_deref()
    }
}

/// 默认几何字段名推断
fn default_shape_field(backend: &Arc<dyn SqlBackend>, table: &str) -> String {
    let cols = backend.columns(table).unwrap_or_default();
    let hit = cols.iter().find(|c| c.name.eq_ignore_ascii_case("Shape"));
    hit.map(|c| c.name.clone()).unwrap_or_else(|| "Shape".into())
}

/// 查找数值型字段（如 Shape_Length）
fn find_numeric_field(fields: &Fields, name: &str) -> Option<String> {
    fields
        .by_name(name)
        .filter(|f| f.field_type.is_numeric())
        .map(|f: &Field| f.name.clone())
}

/// 探测 <表>_SHAPE_Index（大小写容错）
fn detect_shape_index_table(
    backend: &Arc<dyn SqlBackend>,
    table: &str,
    policy: &WritePolicy,
) -> Option<String> {
    let all = backend.table_names().ok()?;
    let candidate = format!("{}{}", table, policy.shape_index_suffix);
    all.into_iter()
        .find(|n| n.eq_ignore_ascii_case(&candidate))
}

/// 探测空间索引表中的对象 ID 列名
fn shape_index_id_column(backend: &Arc<dyn SqlBackend>, table: &str) -> Option<String> {
    let cols = backend.columns(table).ok()?;
    for preferred in ["IndexedObjectId", "IndexedObjectID", "IndexedObjectid", "ObjectID", "OBJECTID"] {
        if let Some(c) = cols.iter().find(|c| c.name.eq_ignore_ascii_case(preferred)) {
            return Some(c.name.clone());
        }
    }
    None
}

/// 把地理坐标转换为网格单元坐标，Min 向下取整、Max 向上取整
fn grid_cells(env: &Envelope, origin_x: f64, origin_y: f64, size: f64) -> (f64, f64, f64, f64) {
    let min_gx = ((env.min_x - origin_x) / size).floor();
    let min_gy = ((env.min_y - origin_y) / size).floor();
    let max_gx = ((env.max_x - origin_x) / size).ceil();
    let max_gy = ((env.max_y - origin_y) / size).ceil();
    (min_gx, min_gy, max_gx, max_gy)
}

/// 更新 GDB_GeomColumns 的图层范围
fn update_geom_columns_extent(
    backend: &Arc<dyn SqlBackend>,
    table: &str,
    env: &Envelope,
) -> Result<()> {
    if !backend.table_exists("GDB_GeomColumns")? {
        return Ok(());
    }
    let rows = backend.select(
        "GDB_GeomColumns",
        &[],
        &Predicate::eq("TableName", table.to_string()),
    )?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let merged = match (
        super::metadata::pick(row, &["ExtentLeft"]).and_then(|v| v.to_f64()),
        super::metadata::pick(row, &["ExtentBottom"]).and_then(|v| v.to_f64()),
        super::metadata::pick(row, &["ExtentRight"]).and_then(|v| v.to_f64()),
        super::metadata::pick(row, &["ExtentTop"]).and_then(|v| v.to_f64()),
    ) {
        (Some(l), Some(b), Some(r), Some(t)) => env.union(&Envelope::new(l, b, r, t)),
        _ => *env,
    };
    backend.update(
        "GDB_GeomColumns",
        &[
            ("ExtentLeft".into(), SqlValue::F64(merged.min_x)),
            ("ExtentBottom".into(), SqlValue::F64(merged.min_y)),
            ("ExtentRight".into(), SqlValue::F64(merged.max_x)),
            ("ExtentTop".into(), SqlValue::F64(merged.max_y)),
        ],
        &Predicate::eq("TableName", table.to_string()),
    )?;
    Ok(())
}

impl DatasetNode for PgdbFeatureClass {
    fn name(&self) -> &str {
        &self.core.name
    }
    fn qualified_name(&self) -> String {
        match &self.core.parent {
            Some(p) => format!("{p}\\{}", self.core.name),
            None => self.core.name.clone(),
        }
    }
    fn kind(&self) -> DatasetKind {
        DatasetKind::FeatureClass
    }
    fn parent_name(&self) -> Option<&str> {
        self.core.parent.as_deref()
    }
    fn as_table(&self) -> Option<&dyn Table> {
        Some(self)
    }
    fn as_feature_class(&self) -> Option<&dyn FeatureClass> {
        Some(self)
    }
    fn try_row_count(&self) -> Option<Result<u64>> {
        Some(Table::row_count(self, &QueryFilter::new()))
    }
    fn update_any(&self, filter: &QueryFilter, sets: &[(String, Value)]) -> Result<u64> {
        self.update_features_rows(sets, filter)
    }
}

impl Table for PgdbFeatureClass {
    fn fields(&self) -> &Fields {
        &self.core.fields
    }
    fn oid_field_name(&self) -> &str {
        &self.core.oid_field
    }
    fn table_name(&self) -> &str {
        &self.core.table_name
    }
    fn backend(&self) -> &Arc<dyn SqlBackend> {
        &self.core.backend
    }
    fn core(&self) -> &TableCore {
        &self.core
    }
    fn search(&self, filter: QueryFilter) -> Result<RowIter<'_>> {
        let rows = self.core.select_rows(&filter, Some(&self.shape_field))?;
        Ok(RowIter::new(self, rows))
    }
    fn update(&self, filter: QueryFilter) -> Result<RowIter<'_>> {
        self.search(filter)
    }
    fn insert_cursor(&self) -> Result<InsertRowCursor<'_>> {
        Ok(InsertRowCursor::new(self))
    }
    fn get_row(&self, oid: i64) -> Result<Option<Row<'_>>> {
        let data = self.core.fetch_row(oid, Some(&self.shape_field))?;
        Ok(match data {
            Some(d) => Some(Row::from_row(self, &d, true)?),
            None => None,
        })
    }
    fn row_count(&self, filter: &QueryFilter) -> Result<u64> {
        self.core.row_count(filter)
    }
    fn update_rows(&self, sets: &[(String, Value)], filter: &QueryFilter) -> Result<u64> {
        self.update_features_rows(sets, filter)
    }
    fn delete_rows(&self, filter: &QueryFilter) -> Result<u64> {
        // 先取到 OID 列表，保证空间索引记录同步删除
        let pred = filter.to_predicate(&self.core.oid_field);
        let rows = self.core.backend.select(
            &self.core.table_name,
            std::slice::from_ref(&self.core.oid_field),
            &pred,
        )?;
        let oids: Vec<i64> = rows
            .iter()
            .filter_map(|r| r.get(&self.core.oid_field).and_then(|v| v.to_i64()))
            .collect();
        let n = self.core.delete_rows(filter)?;
        for oid in oids {
            self.delete_shape_index_rows(oid)?;
        }
        Ok(n)
    }
    fn insert_row(&self, values: &[(String, Value)]) -> Result<i64> {
        self.insert_feature(values)
    }
    fn store_row(&self, oid: Option<i64>, sets: &[(String, Value)]) -> Result<()> {
        let mut sets = sets.to_vec();
        if let Some(geometry) = self.decode_values(&sets) {
            let geometry = geometry?;
            let geometry = self.normalize_in_place(&mut sets, geometry)?;
            for (k, v) in self.measurement_updates(&geometry) {
                if !sets.iter().any(|(n, _)| n.eq_ignore_ascii_case(&k)) {
                    sets.push((k, v));
                }
            }
            let env = geometry.envelope();
            let Some(oid) = oid else {
                return Err(crate::error::PgdbError::InvalidArgument(
                    "缺少 OBJECTID，无法确定要更新的要素".into(),
                ));
            };
            if let Some(env) = env {
                self.expand_layer_extent(&env)?;
            }
            self.sync_shape_index(oid, env)?;
        }
        self.core.store_row(oid, &sets)
    }
    fn delete_row(&self, oid: i64) -> Result<()> {
        self.delete_shape_index_rows(oid)?;
        self.core.delete_row(oid)
    }
    fn geometry_column(&self) -> Option<&str> {
        Some(&self.shape_field)
    }
}

impl PgdbFeatureClass {
    /// 同步删除空间索引表中的记录
    fn delete_shape_index_rows(&self, oid: i64) -> Result<()> {
        if !self.policy.maintain_shape_index {
            return Ok(());
        }
        let Some(table) = &self.shape_index_table else {
            return Ok(());
        };
        let id_col = shape_index_id_column(&self.core.backend, table)
            .unwrap_or_else(|| "IndexedObjectId".into());
        self.core
            .backend
            .delete(table, &Predicate::eq(id_col, oid))?;
        Ok(())
    }

    /// 要素类的批量属性/几何更新入口
    fn update_features_rows(
        &self,
        sets: &[(String, Value)],
        filter: &QueryFilter,
    ) -> Result<u64> {
        // 若批量更新里包含几何，逐要素走 store_row 以维护索引与量算字段
        if sets
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(&self.shape_field))
        {
            let mut cursor = self.update_features(filter.clone())?;
            let mut n = 0u64;
            while let Some(mut feature) = cursor.next_feature()? {
                for (k, v) in sets {
                    if k.eq_ignore_ascii_case(&self.shape_field) {
                        if let Value::Blob(b) = v {
                            feature.set_value(self.shape_index, Value::Blob(b.clone()))?;
                        }
                    } else {
                        feature.set_value_by_name(k, v.clone())?;
                    }
                }
                feature.store()?;
                n += 1;
            }
            return Ok(n);
        }
        self.core.update_rows(sets, filter)
    }
}

impl FeatureClass for PgdbFeatureClass {
    fn shape_field_name(&self) -> &str {
        &self.shape_field
    }
    fn table(&self) -> &dyn Table {
        self
    }
    fn shape_type(&self) -> GeometryType {
        self.shape_type
    }
    fn feature_type(&self) -> FeatureType {
        self.feature_type
    }
    fn spatial_reference(&self) -> Option<&SpatialReference> {
        self.spatial_ref.as_ref()
    }
    fn extent(&self) -> Option<Envelope> {
        self.extent
    }
    fn grid_info(&self) -> GridInfo {
        self.grid
    }
    fn length_field_name(&self) -> Option<&str> {
        self.length_field.as_deref()
    }
    fn area_field_name(&self) -> Option<&str> {
        self.area_field.as_deref()
    }
    fn shape_index_table(&self) -> Option<&str> {
        self.shape_index_table.as_deref()
    }
    fn search_features(&self, filter: QueryFilter) -> Result<FeatureIter<'_>> {
        let rows = self.core.select_rows(&filter, Some(&self.shape_field))?;
        Ok(FeatureIter::new(self, rows))
    }
    fn update_features(&self, filter: QueryFilter) -> Result<FeatureIter<'_>> {
        self.search_features(filter)
    }
    fn insert_feature_cursor(&self) -> Result<InsertFeatureCursor<'_>> {
        Ok(InsertFeatureCursor::new(self))
    }
    fn get_feature(&self, oid: i64) -> Result<Option<Feature<'_>>> {
        let data = self.core.fetch_row(oid, Some(&self.shape_field))?;
        Ok(match data {
            Some(d) => Some(Feature::from_row(self, &d)?),
            None => None,
        })
    }
    fn create_feature_buffer(&self) -> RowBuffer {
        RowBuffer::new()
    }
    fn insert_feature(&self, values: &[(String, Value)]) -> Result<i64> {
        let mut values = values.to_vec();
        let oid = self.core.insert_row(&values)?;
        if let Some(geometry) = self.decode_values(&values) {
            let geometry = geometry?;
            let geometry = self.normalize_in_place(&mut values, geometry)?;
            // 规范化可能改变了 Shape 字节，需要同步更新刚插入的记录
            self.core.store_row(Some(oid), &values)?;
            let env = geometry.envelope();
            // 补齐量算字段
            let measures = self.measurement_updates(&geometry);
            if !measures.is_empty() {
                let _ = self
                    .core
                    .store_row(Some(oid), &measures)
                    .map_err(|e| log::warn!("写入量算字段失败: {e}"));
            }
            if let Some(env) = env {
                self.expand_layer_extent(&env)?;
            }
            self.sync_shape_index(oid, env)?;
        }
        Ok(oid)
    }
    fn write_policy(&self) -> &WritePolicy {
        &self.policy
    }
    fn refresh_layer_extent(&self) -> Result<Envelope> {
        let mut merged: Option<Envelope> = None;
        for (_, env) in self.scan_feature_envelopes()? {
            merged = Some(match merged {
                None => env.unwrap_or(Envelope::new(0.0, 0.0, 0.0, 0.0)),
                Some(m) => match env {
                    Some(e) => m.union(&e),
                    None => m,
                },
            });
        }
        let env = merged.unwrap_or(Envelope::new(0.0, 0.0, 0.0, 0.0));
        update_geom_columns_extent(&self.core.backend, &self.core.table_name, &env)?;
        Ok(env)
    }
    fn rebuild_shape_index(&self) -> Result<usize> {
        let Some(table) = &self.shape_index_table else {
            return Err(crate::error::PgdbError::NotFound(format!(
                "未找到 {} 的空间索引表",
                self.core.table_name
            )));
        };
        self.core.backend.delete(table, &Predicate::All)?;
        let id_col = shape_index_id_column(&self.core.backend, table)
            .unwrap_or_else(|| "IndexedObjectId".into());
        let mut count = 0usize;
        for (oid, env) in self.scan_feature_envelopes()? {
            let Some(env) = env else { continue };
            let size = if self.grid.grid_size > 0.0 {
                self.grid.grid_size
            } else {
                self.policy.default_grid_size
            };
            let (min_gx, min_gy, max_gx, max_gy) =
                grid_cells(&env, self.grid.origin_x, self.grid.origin_y, size);
            self.core.backend.insert(
                table,
                &[
                    (id_col.clone(), SqlValue::I64(oid)),
                    ("MinGX".into(), SqlValue::F64(min_gx)),
                    ("MinGY".into(), SqlValue::F64(min_gy)),
                    ("MaxGX".into(), SqlValue::F64(max_gx)),
                    ("MaxGY".into(), SqlValue::F64(max_gy)),
                ],
            )?;
            count += 1;
        }
        Ok(count)
    }
}
