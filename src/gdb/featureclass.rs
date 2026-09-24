//! 要素类：`IFeatureClass` 的 Rust 版本，包含几何写入时的 ESRI 一致性维护。

use std::sync::{Arc, RwLock};

use crate::datastore::{Predicate, SqlBackend, SqlValue};
use crate::error::{PgdbError, Result};
use crate::field::{Field, Fields};
use crate::geom::codec::{decode_shape, encode_shape};
use crate::geom::{geometry_from_wkt, Envelope, Geometry, GeometryType};
use crate::value::Value;

use super::cursor::{FeatureIter, InsertFeatureCursor, InsertRowCursor, RowIter};
use super::dataset::DatasetNode;
use super::edit::{EditOptions, EditResult, EditScope, Preflight};
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
    /// 图层范围缓存（删除/重算后同步更新；用 RwLock 支持 `&self` 内部可变）
    extent: RwLock<Option<Envelope>>,
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
            extent: RwLock::new(entry.extent),
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

    /// 统一的「字段值 -> 几何」解码入口（`Blob` 二进制 / WKT 文本 / `Null` 三态一致）。
    ///
    /// 无论从 CLI 的 `--set Shape=<WKT>`、游标的 `set_value` 还是 `store_row`
    /// 进入，几何值的解释都在这里收敛，避免出现"批量更新只认 Blob、
    /// WKT 被静默丢弃"的缝隙。
    pub(crate) fn geometry_from_value(&self, v: &Value) -> Result<Geometry> {
        match v {
            Value::Null => Ok(Geometry::Null),
            Value::Blob(b) => decode_shape(b),
            Value::String(s) => geometry_from_wkt(s),
            other => Err(PgdbError::Conversion {
                field: self.shape_field.clone(),
                message: format!("无法把该值解释为几何: {other}"),
            }),
        }
    }

    /// 统一的「几何 -> 落库值」编码出口（解码的镜像操作）。
    ///
    /// `Null` 保持 NULL，其余几何一律编码为 ESRI Shape 二进制，
    /// 保证 LONGVARBINARY 几何列里永远不会混入 TEXT 值。
    fn encode_geometry_value(&self, g: &Geometry) -> Result<Value> {
        match g {
            Geometry::Null => Ok(Value::Null),
            other => Ok(Value::Blob(encode_shape(other)?)),
        }
    }

    /// 内部：从 sets 中提取 Shape 列的几何（无几何列时返回 None）
    fn decode_values(&self, values: &[(String, Value)]) -> Option<Result<Geometry>> {
        values
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(&self.shape_field))
            .map(|(_, v)| self.geometry_from_value(v))
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
        let merged = update_geom_columns_extent(&self.core.backend, table, env)?;
        // 同步内存缓存，保证 extent() 与 GDB_GeomColumns 一致
        *self.extent.write().unwrap() = Some(merged);
        Ok(())
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
        self.select_oid_envelopes(&Predicate::All, None)
    }

    /// 按谓词选取（OBJECTID, 几何信封）对，可选附加内存空间过滤。
    ///
    /// 删除路径专用：**只取 OID 与 Shape 两列**（忽略 filter.sub_fields，
    /// 保证删除前总能拿到主键与几何），供索引清理与范围回缩决策使用。
    fn select_oid_envelopes(
        &self,
        pred: &Predicate,
        spatial: Option<&super::filter::SpatialFilter>,
    ) -> Result<Vec<(i64, Option<Envelope>)>> {
        let fields = vec![
            self.core.oid_field.clone(),
            self.shape_field.clone(),
        ];
        let rows = self
            .core
            .backend
            .select(&self.core.table_name, &fields, pred)?;
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
            if let Some(sp) = spatial {
                let target = sp.geometry.envelope();
                let Some(target) = target else { continue };
                let keep = super::table::spatial_match(&sp.relation, env, &target);
                if !keep {
                    continue;
                }
            }
            out.push((oid, env));
        }
        Ok(out)
    }

    /// 删除后的图层范围回缩。
    ///
    /// 若被删要素的合并信封**严格位于**当前图层范围内部（不触及任何边界），
    /// 删除不可能缩小范围，直接跳过全表重算；否则调用
    /// [`FeatureClass::refresh_layer_extent`] 覆写回正确范围。
    fn shrink_extent_after_delete(&self, deleted_env: Option<Envelope>) -> Result<()> {
        if !self.policy.maintain_layer_extent {
            return Ok(());
        }
        let Some(deleted) = deleted_env else {
            return Ok(()); // 删除的要素没有几何，范围必然不变
        };
        let Some(current) = self.extent() else {
            return Ok(());
        };
        let touches_boundary = approx_eq(deleted.min_x, current.min_x)
            || approx_eq(deleted.min_y, current.min_y)
            || approx_eq(deleted.max_x, current.max_x)
            || approx_eq(deleted.max_y, current.max_y);
        if !touches_boundary && current.contains_envelope(&deleted) {
            return Ok(()); // 内部删除：范围不变，跳过 O(n) 重算
        }
        self.refresh_layer_extent()?;
        Ok(())
    }

    /// 清空图层范围（delete-all 后调用，写回零范围并同步缓存）
    fn reset_layer_extent(&self) -> Result<()> {
        if !self.policy.maintain_layer_extent {
            return Ok(());
        }
        let zero = Envelope::new(0.0, 0.0, 0.0, 0.0);
        set_geom_columns_extent(&self.core.backend, &self.core.table_name, &zero)?;
        *self.extent.write().unwrap() = Some(zero);
        Ok(())
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

/// 更新 GDB_GeomColumns 的图层范围：与现有范围做**并集**（只扩不缩），
/// 供插入/更新几何时使用。返回合并后的最终范围，供调用方同步内存缓存。
fn update_geom_columns_extent(
    backend: &Arc<dyn SqlBackend>,
    table: &str,
    env: &Envelope,
) -> Result<Envelope> {
    if !backend.table_exists("GDB_GeomColumns")? {
        return Ok(*env);
    }
    let rows = backend.select(
        "GDB_GeomColumns",
        &[],
        &Predicate::eq("TableName", table.to_string()),
    )?;
    let Some(row) = rows.first() else {
        return Ok(*env);
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
    set_geom_columns_extent(backend, table, &merged)?;
    Ok(merged)
}

/// **覆写** GDB_GeomColumns 的图层范围（不做并集）。
///
/// 供范围重算（`refresh_layer_extent`）与删除后回缩使用：
/// 重算得到的范围就是最终事实，若仍走并集会让"曾经偏大"的范围永远无法纠正。
fn set_geom_columns_extent(
    backend: &Arc<dyn SqlBackend>,
    table: &str,
    env: &Envelope,
) -> Result<()> {
    if !backend.table_exists("GDB_GeomColumns")? {
        return Ok(());
    }
    backend.update(
        "GDB_GeomColumns",
        &[
            ("ExtentLeft".into(), SqlValue::F64(env.min_x)),
            ("ExtentBottom".into(), SqlValue::F64(env.min_y)),
            ("ExtentRight".into(), SqlValue::F64(env.max_x)),
            ("ExtentTop".into(), SqlValue::F64(env.max_y)),
        ],
        &Predicate::eq("TableName", table.to_string()),
    )?;
    Ok(())
}

/// 浮点近似相等（边界比较用）
fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9f64.max(a.abs().max(b.abs()) * 1e-9)
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
        self.update_features_rows(sets, filter, EditOptions::default())
            .map(|r| r.affected)
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
    fn update_searched_rows(
        &self,
        sets: &[(String, Value)],
        filter: &QueryFilter,
        opts: EditOptions,
    ) -> Result<EditResult> {
        self.update_features_rows(sets, filter, opts)
    }
    fn delete_searched_rows(&self, filter: &QueryFilter, opts: EditOptions) -> Result<EditResult> {
        let scope = if filter.is_trivial() {
            EditScope::WholeTable
        } else {
            EditScope::Filtered
        };
        self.delete_features_rows(
            &filter.to_predicate(&self.core.oid_field),
            filter.spatial.as_ref(),
            opts,
            scope,
            "删除",
        )
    }
    fn delete_rows(&self, oids: &[i64]) -> Result<EditResult> {
        let pred = Predicate::in_ids(self.core.oid_field.clone(), oids);
        self.delete_features_rows(
            &pred,
            None,
            EditOptions::default(),
            EditScope::Filtered,
            "删除",
        )
    }
    fn delete_all_rows(&self) -> Result<EditResult> {
        let matched = self.core.row_count(&QueryFilter::new())?;
        if matched == 0 {
            return Ok(EditResult::new(0, 0, EditScope::WholeTable));
        }
        // 清空空间索引记录（整表删除索引行即可，无需逐 OID）
        if self.policy.maintain_shape_index {
            if let Some(table) = &self.shape_index_table {
                self.core.backend.delete(table, &Predicate::All)?;
            }
        }
        let affected = self
            .core
            .backend
            .delete(&self.core.table_name, &Predicate::All)?;
        self.reset_layer_extent()?;
        Ok(EditResult::new(matched, affected, EditScope::WholeTable))
    }
    fn preflight_edit(&self, filter: &QueryFilter) -> Result<Preflight> {
        self.core.preflight_edit(filter)
    }
    fn insert_row(&self, values: &[(String, Value)]) -> Result<i64> {
        self.insert_feature(values)
    }
    fn store_row(&self, oid: Option<i64>, sets: &[(String, Value)]) -> Result<()> {
        let mut sets = sets.to_vec();
        if let Some(geometry) = self.decode_values(&sets) {
            let geometry = geometry?;
            let geometry = self.normalize_in_place(&mut sets, geometry)?;
            // 几何列统一落库为 ESRI 二进制：WKT 文本在此编码，Null 保持 NULL，
            // 避免 TEXT 值混入 LONGVARBINARY 列导致后续读取解码失败
            let encoded = self.encode_geometry_value(&geometry)?;
            if let Some((_, v)) = sets
                .iter_mut()
                .find(|(n, _)| n.eq_ignore_ascii_case(&self.shape_field))
            {
                *v = encoded;
            }
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

    /// 要素类统一的批量删除入口：删除业务行 + 清理 `_SHAPE_Index` + 回缩图层范围。
    ///
    /// `pred`/`spatial` 共同确定删除集合；先以「OID + Shape」两列取出待删要素
    /// （顺带得到合并信封），再整体删除，最后按需重算图层范围。
    fn delete_features_rows(
        &self,
        pred: &Predicate,
        spatial: Option<&super::filter::SpatialFilter>,
        opts: EditOptions,
        scope: EditScope,
        action: &str,
    ) -> Result<EditResult> {
        self.core.ensure_writable(action)?;
        if scope.is_whole_table() {
            // 全表删除走 delete_all_rows，这里防御性地拒绝
            return Err(PgdbError::InvalidArgument(
                "清空整张表请使用 delete_all_rows()（CLI: delete-all-rows）".into(),
            ));
        }
        let pairs = self.select_oid_envelopes(pred, spatial)?;
        let oids: Vec<i64> = pairs.iter().map(|(o, _)| *o).collect();
        let deleted_env = pairs.iter().fold(None, |acc: Option<Envelope>, (_, e)| {
            match (acc, e) {
                (None, Some(e)) => Some(*e),
                (Some(a), Some(e)) => Some(a.union(e)),
                (a, None) => a,
            }
        });
        let affected = if oids.is_empty() {
            0
        } else {
            let del_pred = Predicate::in_ids(self.core.oid_field.clone(), &oids);
            self.core
                .backend
                .delete(&self.core.table_name, &del_pred)?
        };
        for oid in &oids {
            self.delete_shape_index_rows(*oid)?;
        }
        self.shrink_extent_after_delete(deleted_env)?;
        self.core
            .check_hit(EditResult::new(oids.len() as u64, affected, scope), opts, action)
    }

    /// 要素类的批量属性/几何更新入口（`ITable::UpdateSearchedRows`）。
    ///
    /// 含几何字段时逐要素走 `store`（维护 Shape_Length/Shape_Area、
    /// `_SHAPE_Index` 与图层范围）；纯属性更新走单条 SQL，效率更高。
    /// 几何值经 [`Self::geometry_from_value`] 统一解释：Blob / WKT 文本 / Null 均可。
    fn update_features_rows(
        &self,
        sets: &[(String, Value)],
        filter: &QueryFilter,
        opts: EditOptions,
    ) -> Result<EditResult> {
        let has_geometry = sets
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(&self.shape_field));
        if !has_geometry {
            return self.core.update_searched_rows(sets, filter, opts);
        }
        let scope = self.core.check_edit_options(filter, opts, "更新")?;
        // 写前统一校验几何值，避免"改到第 N 行才报错"的半成品状态
        for (name, v) in sets {
            if name.eq_ignore_ascii_case(&self.shape_field) {
                self.geometry_from_value(v)?;
            }
        }
        let mut cursor = self.update_features(filter.clone())?;
        let mut n = 0u64;
        while let Some(mut feature) = cursor.next_feature()? {
            for (k, v) in sets {
                if k.eq_ignore_ascii_case(&self.shape_field) {
                    feature.set_value(self.shape_index, v.clone())?;
                } else {
                    feature.set_value_by_name(k, v.clone())?;
                }
            }
            feature.store()?;
            n += 1;
        }
        self.core
            .check_hit(EditResult::new(n, n, scope), opts, "更新")
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
        *self.extent.read().unwrap()
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
            // 几何列统一落库为 ESRI 二进制：WKT 文本在此编码，Null 保持 NULL
            let encoded = self.encode_geometry_value(&geometry)?;
            if let Some((_, v)) = values
                .iter_mut()
                .find(|(n, _)| n.eq_ignore_ascii_case(&self.shape_field))
            {
                *v = encoded;
            }
            // 编码/规范化可能改变了 Shape 字节，需要同步更新刚插入的记录
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
        // 重算的结果是最终事实，必须**覆写**而非并集，
        // 否则历史上偏大的范围永远无法纠正。
        set_geom_columns_extent(&self.core.backend, &self.core.table_name, &env)?;
        *self.extent.write().unwrap() = Some(env);
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
