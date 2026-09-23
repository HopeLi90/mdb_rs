//! 表对象：`ITable` 的 Rust 版本。
//!
//! [`TableCore`] 保存所有与具体 DBMS 无关的表级状态与逻辑，`PgdbTable` 与
//! `PgdbFeatureClass` 都基于它，从而保证两者的读写行为一致。

use std::sync::Arc;

use crate::datastore::{DataTableRow, Predicate, SqlBackend, SqlValue};
use crate::error::{PgdbError, Result};
use crate::field::{Field, FieldType, Fields};
use crate::geom::codec::decode_shape;
use crate::geom::Envelope;
use crate::value::Value;

use super::cursor::{InsertRowCursor, RowIter};
use super::dataset::DatasetNode;
use super::filter::{QueryFilter, SpatialRel};
use super::metadata::{CatalogEntry, DatasetKind};
use super::row::Row;

/// 表级通用状态与逻辑
pub struct TableCore {
    /// 后端
    pub backend: Arc<dyn SqlBackend>,
    /// 物理表名
    pub table_name: String,
    /// 逻辑名称
    pub name: String,
    /// 所属要素数据集
    pub parent: Option<String>,
    /// 字段集合
    pub fields: Fields,
    /// OBJECTID 字段
    pub oid_field: String,
    /// 数据集类型
    pub kind: DatasetKind,
}

impl TableCore {
    /// 由后端与目录条目构造
    pub fn open(
        backend: Arc<dyn SqlBackend>,
        entry: &CatalogEntry,
        aliases: Option<&std::collections::HashMap<String, String>>,
        srid_owned: Option<i64>,
    ) -> Result<Self> {
        let cols = backend
            .columns(&entry.table_name)
            .map_err(|e| PgdbError::NotFound(format!("业务表 {}: {e}", entry.table_name)))?;
        let is_fc = matches!(entry.kind, DatasetKind::FeatureClass);
        let shape_field = entry.shape_field.clone();
        let mut fields_vec: Vec<Field> = Vec::with_capacity(cols.len());
        for c in &cols {
            let mut f = Field::new(c.name.clone(), c.kind);
            f.length = c.size;
            f.nullable = c.nullable;
            f.required = !c.nullable;
            f.editable = !c.is_auto;
            if is_fc && shape_field.as_ref().map(|s| s.eq_ignore_ascii_case(&c.name)).unwrap_or(false) {
                f.is_geometry = true;
                f.field_type = FieldType::Geometry;
                f.alias = Some("Shape".to_string());
            } else if c.is_auto {
                f.field_type = FieldType::Oid;
            } else if matches!(c.kind, FieldType::Double) && &c.name.to_lowercase() == "shape_length" {
                f.alias = Some("Shape_Length".to_string());
            } else if matches!(c.kind, FieldType::Double) && &c.name.to_lowercase() == "shape_area" {
                f.alias = Some("Shape_Area".to_string());
            }
            if let Some(alias_map) = aliases {
                let key = format!("{}:{}", entry.table_name, c.name).to_lowercase();
                let _ = key;
                if let Some(a) = alias_map.get(&c.name.to_lowercase()) {
                    if f.alias.is_none() {
                        f.alias = Some(a.clone());
                    }
                }
            }
            fields_vec.push(f);
        }
        let _ = srid_owned;
        Ok(Self {
            backend,
            table_name: entry.table_name.clone(),
            name: entry.name.clone(),
            parent: entry.parent.clone(),
            fields: Fields::new(fields_vec),
            oid_field: entry.oid_field.clone(),
            kind: entry.kind,
        })
    }

    /// 构造查询列清单：保证 OBJECTID 总被选中，且过滤掉不存在的字段
    fn select_columns(&self, filter: &QueryFilter) -> Vec<String> {
        let oid_idx = self.fields.find(&self.oid_field);
        let mut out: Vec<String> = Vec::new();
        if filter.sub_fields.is_empty() {
            return Vec::new(); // 空 = SELECT *
        }
        let mut added_oid = false;
        if let Some(i) = oid_idx {
            if let Some(f) = self.fields.field(i) {
                out.push(f.name.clone());
                added_oid = true;
            }
        }
        for name in &filter.sub_fields {
            if let Some(f) = self.fields.by_name(name) {
                if !added_oid || !f.name.eq_ignore_ascii_case(&self.oid_field) {
                    out.push(f.name.clone());
                }
            } else {
                log::warn!("过滤器指定字段 {name} 不存在，已忽略");
            }
        }
        out
    }

    /// 查询并按（可选）空间条件在内存中过滤
    pub fn select_rows(
        &self,
        filter: &QueryFilter,
        shape_field: Option<&str>,
    ) -> Result<Vec<DataTableRow>> {
        let cols = self.select_columns(filter);
        let pred = filter.to_predicate(&self.oid_field);
        let mut rows = self.backend.select(&self.table_name, &cols, &pred)?;
        if let Some(spatial) = &filter.spatial {
            let Some(shape_field) = shape_field else {
                if !rows.is_empty() {
                    return Err(PgdbError::InvalidArgument(
                        "该数据集没有几何字段，无法使用空间过滤".into(),
                    ));
                }
                return Ok(rows);
            };
            let target_env = spatial.geometry.envelope();
            rows.retain(|r| match r.get(shape_field).and_then(|v| v.to_binary()) {
                Some(bytes) => match (decode_shape(bytes), target_env) {
                    (Ok(g), Some(env)) => spatial_match(&spatial.relation, g.envelope(), &env),
                    _ => false,
                },
                None => false,
            });
        }
        Ok(rows)
    }

    /// 原始行数
    pub fn row_count(&self, filter: &QueryFilter) -> Result<u64> {
        if filter.needs_memory_spatial_filter() {
            return Ok(self.select_rows(filter, None)?.len() as u64);
        }
        let pred = filter.to_predicate(&self.oid_field);
        self.backend.count(&self.table_name, &pred)
    }

    /// 批量更新
    pub fn update_rows(&self, sets: &[(String, Value)], filter: &QueryFilter) -> Result<u64> {
        let pred = filter.to_predicate(&self.oid_field);
        let sql_sets = to_sql_sets(sets, &self.fields, self.oid_field.clone())?;
        if sql_sets.is_empty() {
            return Err(PgdbError::InvalidArgument("没有要更新的字段".into()));
        }
        self.backend.update(&self.table_name, &sql_sets, &pred)
    }

    /// 批量删除
    pub fn delete_rows(&self, filter: &QueryFilter) -> Result<u64> {
        let pred = filter.to_predicate(&self.oid_field);
        self.backend.delete(&self.table_name, &pred)
    }

    /// 单条插入，返回 OBJECTID
    pub fn insert_row(&self, values: &[(String, Value)]) -> Result<i64> {
        let sql_vals = to_sql_inserts(values, &self.fields, self.oid_field.clone())?;
        if sql_vals.is_empty() {
            return Err(PgdbError::InvalidArgument("没有可插入的字段值".into()));
        }
        self.backend.insert(&self.table_name, &sql_vals)
    }

    /// 单条保存
    pub fn store_row(&self, oid: Option<i64>, sets: &[(String, Value)]) -> Result<()> {
        let Some(oid) = oid else {
            return Err(PgdbError::InvalidArgument(
                "没有 OBJECTID，无法定位待更新的行".into(),
            ));
        };
        let sql_sets = to_sql_sets(sets, &self.fields, self.oid_field.clone())?;
        if sql_sets.is_empty() {
            return Err(PgdbError::InvalidArgument("没有要更新的字段".into()));
        }
        let pred = Predicate::eq(&self.oid_field, oid);
        let affected = self.backend.update(&self.table_name, &sql_sets, &pred)?;
        if affected == 0 {
            return Err(PgdbError::NotFound(format!(
                "表 {} 中 OBJECTID={} 的行不存在",
                self.table_name, oid
            )));
        }
        Ok(())
    }

    /// 单条删除
    pub fn delete_row(&self, oid: i64) -> Result<()> {
        let pred = Predicate::eq(&self.oid_field, oid);
        let n = self.backend.delete(&self.table_name, &pred)?;
        if n == 0 {
            return Err(PgdbError::NotFound(format!(
                "表 {} 中 OBJECTID={} 的行不存在",
                self.table_name, oid
            )));
        }
        Ok(())
    }

    /// 按 OBJECTID 取一行
    pub fn fetch_row(&self, oid: i64, shape_field: Option<&str>) -> Result<Option<DataTableRow>> {
        let rows = self.select_rows(&QueryFilter::for_oid(oid), shape_field)?;
        Ok(rows.into_iter().next())
    }
}

/// 值 -> SQL 值，并移除 OBJECTID 列（主键不可写）
fn sanitize(values: &[(String, Value)], fields: &Fields, oid_field: String) -> Result<Vec<(String, SqlValue)>> {
    let mut out = Vec::with_capacity(values.len());
    for (name, value) in values {
        if name.eq_ignore_ascii_case(&oid_field) {
            continue;
        }
        let Some(f) = fields.by_name(name) else {
            return Err(PgdbError::NotFound(format!("表字段 {name} 不存在")));
        };
        if !f.editable {
            return Err(PgdbError::InvalidArgument(format!("字段 {name} 不可编辑")));
        }
        out.push((f.name.clone(), value.clone().into()));
    }
    Ok(out)
}

fn to_sql_sets(
    values: &[(String, Value)],
    fields: &Fields,
    oid_field: String,
) -> Result<Vec<(String, SqlValue)>> {
    let sets = sanitize(values, fields, oid_field)?;
    if sets.is_empty() {
        return Err(PgdbError::InvalidArgument("没有可写的列".into()));
    }
    Ok(sets)
}

fn to_sql_inserts(
    values: &[(String, Value)],
    fields: &Fields,
    oid_field: String,
) -> Result<Vec<(String, SqlValue)>> {
    sanitize(values, fields, oid_field)
}

/// 内存中的空间关系判断（基于包络矩形，够 Rao reliable 用于粗筛）
fn spatial_match(rel: &SpatialRel, a: Option<Envelope>, b: &Envelope) -> bool {
    let Some(a) = a else { return false };
    match rel {
        SpatialRel::Intersects | SpatialRel::Overlaps | SpatialRel::Touches => a.intersects(b),
        SpatialRel::Contains => a.contains_envelope(b),
        SpatialRel::Within => b.contains_envelope(&a),
    }
}

/// 表接口（`ITable`）
pub trait Table: DatasetNode {
    /// 字段集合（`ITable::Fields`）
    fn fields(&self) -> &Fields;

    /// OBJECTID 字段名（`IObjectClass::OIDFieldName`）
    fn oid_field_name(&self) -> &str;

    /// 物理表名
    fn table_name(&self) -> &str;

    /// 后端句柄
    fn backend(&self) -> &Arc<dyn SqlBackend>;

    /// 核心状态
    fn core(&self) -> &TableCore;

    /// 查询游标（`ITable::Search`）
    fn search(&self, filter: QueryFilter) -> Result<RowIter<'_>>;

    /// 更新游标（`ITable::Update`）
    fn update(&self, filter: QueryFilter) -> Result<RowIter<'_>> {
        self.search(filter)
    }

    /// 插入游标（`ITable::Insert`）
    fn insert_cursor(&self) -> Result<InsertRowCursor<'_>>;

    /// 按 OBJECTID 取行（`ITable::GetRow`）
    fn get_row(&self, oid: i64) -> Result<Option<Row<'_>>>;

    /// 行数（`ITable::RowCount`）
    fn row_count(&self, filter: &QueryFilter) -> Result<u64>;

    /// 批量更新
    fn update_rows(&self, sets: &[(String, Value)], filter: &QueryFilter) -> Result<u64>;

    /// 批量删除
    fn delete_rows(&self, filter: &QueryFilter) -> Result<u64>;

    /// 插入一行
    fn insert_row(&self, values: &[(String, Value)]) -> Result<i64>;

    /// 保存一行
    fn store_row(&self, oid: Option<i64>, sets: &[(String, Value)]) -> Result<()>;

    /// 删除一行
    fn delete_row(&self, oid: i64) -> Result<()>;

    /// 几何字段名（非要素类返回 None）
    fn geometry_column(&self) -> Option<&str> {
        None
    }
}

/// Personal Geodatabase 中的独立表 / 业务表
pub struct PgdbTable {
    core: TableCore,
}

impl PgdbTable {
    /// 打开目录条目对应的表
    pub fn open(backend: Arc<dyn SqlBackend>, entry: &CatalogEntry) -> Result<Self> {
        let core = TableCore::open(backend, entry, None, None)?;
        Ok(Self { core })
    }

    /// 打开并注入字段别名
    pub fn open_with_aliases(
        backend: Arc<dyn SqlBackend>,
        entry: &CatalogEntry,
        aliases: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<Self> {
        let core = TableCore::open(backend, entry, aliases, None)?;
        Ok(Self { core })
    }

    /// 访问核心状态
    pub fn core(&self) -> &TableCore {
        &self.core
    }

    /// 遍历全部行
    pub fn rows(&self) -> Result<Vec<Row<'_>>> {
        let cursor = self.search(QueryFilter::new())?;
        cursor.collect_rows()
    }

    /// 读取某一行的所有字段值（调试友好）
    pub fn dump_row(&self, oid: i64) -> Result<Option<Vec<(String, String)>>> {
        let row = self.core.fetch_row(oid, None)?;
        Ok(row.map(|r| {
            r.pairs()
                .into_iter()
                .map(|(k, v)| (k, SqlValue::display_box(v)))
                .collect()
        }))
    }
}

impl DatasetNode for PgdbTable {
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
        self.core.kind
    }
    fn parent_name(&self) -> Option<&str> {
        self.core.parent.as_deref()
    }
    fn as_table(&self) -> Option<&dyn Table> {
        Some(self)
    }
    fn try_row_count(&self) -> Option<Result<u64>> {
        Some(Table::row_count(self, &QueryFilter::new()))
    }
    fn update_any(&self, filter: &QueryFilter, sets: &[(String, Value)]) -> Result<u64> {
        self.update_rows(sets, filter)
    }
}

impl Table for PgdbTable {
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
        let rows = self.core.select_rows(&filter, None)?;
        Ok(RowIter::new(self, rows))
    }
    fn insert_cursor(&self) -> Result<InsertRowCursor<'_>> {
        Ok(InsertRowCursor::new(self))
    }
    fn get_row(&self, oid: i64) -> Result<Option<Row<'_>>> {
        let data = self.core.fetch_row(oid, None)?;
        Ok(match data {
            Some(d) => Some(Row::from_row(self, &d, true)?),
            None => None,
        })
    }
    fn row_count(&self, filter: &QueryFilter) -> Result<u64> {
        self.core.row_count(filter)
    }
    fn update_rows(&self, sets: &[(String, Value)], filter: &QueryFilter) -> Result<u64> {
        self.core.update_rows(sets, filter)
    }
    fn delete_rows(&self, filter: &QueryFilter) -> Result<u64> {
        self.core.delete_rows(filter)
    }
    fn insert_row(&self, values: &[(String, Value)]) -> Result<i64> {
        self.core.insert_row(values)
    }
    fn store_row(&self, oid: Option<i64>, sets: &[(String, Value)]) -> Result<()> {
        self.core.store_row(oid, sets)
    }
    fn delete_row(&self, oid: i64) -> Result<()> {
        self.core.delete_row(oid)
    }
}

/// 辅助显示 trait，供 table dump 使用
pub(crate) trait DisplayBoxed {
    /// 值显示
    fn display_box(v: SqlValue) -> String {
        v.to_display_string()
    }
}

impl DisplayBoxed for SqlValue {}
