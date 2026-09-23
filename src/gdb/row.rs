//! 行与要素缓冲区：对应 ArcObjects 的 `IRow / IFeature / IRowBuffer / IFeatureBuffer`。
//!
//! 语义要点：
//! - `Row` 通过 `&dyn Table` 持有宿主表，修改后用 [`Row::store`] 落库（等价于 `IRow::Store`）；
//! - 只有真正被赋值的列才会出现在 UPDATE 语句中，避免覆盖并发字段；
//! - `Feature` 在 `Row` 之上增加了 Shape 列的几何读写（懒解码）。

use std::collections::HashSet;

use crate::error::{PgdbError, Result};
use crate::field::Fields;
use crate::geom::codec::{decode_shape, encode_shape};
use crate::geom::Geometry;
use crate::value::Value;

use super::featureclass::FeatureClass;
use super::table::Table;

/// 插入缓冲区（`IRowBuffer / IFeatureBuffer`）
#[derive(Debug, Clone, Default)]
pub struct RowBuffer {
    values: Vec<(String, Value)>,
}

impl RowBuffer {
    /// 新建空缓冲区
    pub fn new() -> Self {
        Self::default()
    }

    /// 由键值对构造
    pub fn from_pairs(values: Vec<(String, Value)>) -> Self {
        Self { values }
    }

    /// 赋值（同名字段覆盖）
    pub fn set_value(&mut self, name: impl Into<String>, value: Value) {
        let name = name.into();
        if let Some((_, v)) = self
            .values
            .iter_mut()
            .find(|(n, _)| n.eq_ignore_ascii_case(&name))
        {
            *v = value;
        } else {
            self.values.push((name, value));
        }
    }

    /// 按名取值
    pub fn value(&self, name: &str) -> Option<&Value> {
        self.values
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    /// 设置几何（写入 Shape 列的二进制值）
    pub fn set_geometry(&mut self, shape_field: &str, geometry: &Geometry) -> Result<()> {
        let bytes = encode_shape(geometry)?;
        self.set_value(shape_field, Value::Blob(bytes));
        Ok(())
    }

    /// 读取几何
    pub fn geometry(&self, shape_field: &str) -> Result<Option<Geometry>> {
        match self.value(shape_field) {
            Some(Value::Blob(b)) => Ok(Some(decode_shape(b)?)),
            Some(Value::Null) | None => Ok(None),
            Some(other) => Err(PgdbError::Conversion {
                field: shape_field.into(),
                message: format!("几何字段不是二进制类型: {other}"),
            }),
        }
    }

    /// 清空缓冲区，便于复用（`IFeatureBuffer` 的重复使用约定）
    pub fn clear(&mut self) {
        self.values.clear();
    }

    /// 取出内部键值对
    pub fn into_pairs(self) -> Vec<(String, Value)> {
        self.values
    }

    /// 引用内部键值对
    pub fn pairs(&self) -> &[(String, Value)] {
        &self.values
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// 变更追踪
#[derive(Debug, Default, Clone)]
struct ChangeSet {
    dirty: HashSet<usize>,
}

impl ChangeSet {
    fn touch(&mut self, i: usize) {
        self.dirty.insert(i);
    }
    fn clear(&mut self) {
        self.dirty.clear();
    }
    fn is_empty(&self) -> bool {
        self.dirty.is_empty()
    }
}

/// 数据行（`IRow`）
///
/// 值数组始终与宿主表的完整字段列表**按下标对齐**（与 ArcObjects 的 `IRow` 一致）：
/// 查询未取回的字段为 [`Value::Null`]，并通过 [`Row::is_selected`] 标记出来。
pub struct Row<'a> {
    class: &'a dyn Table,
    values: Vec<Value>,
    /// 与 `values` 同长：该列是否由本次查询真正取回
    selected: Vec<bool>,
    oid: Option<i64>,
    changes: ChangeSet,
    deleted: bool,
}

impl<'a> Row<'a> {
    /// 由宿主表与一行数据构造
    pub(crate) fn from_row(
        class: &'a dyn Table,
        data: &crate::datastore::DataTableRow,
        with_changes_none: bool,
    ) -> Result<Self> {
        let fields = class.fields();
        let mut values = Vec::with_capacity(fields.count());
        let mut selected = Vec::with_capacity(fields.count());
        for f in fields.iter() {
            match data.get(&f.name) {
                Some(v) => {
                    values.push(v.clone().into());
                    selected.push(true);
                }
                None => {
                    values.push(Value::Null);
                    selected.push(false);
                }
            }
        }
        let oid = class
            .fields()
            .find(class.oid_field_name())
            .and_then(|idx| data.get(&class.fields().field(idx).unwrap().name))
            .and_then(|v| v.to_i64());
        let _ = with_changes_none;
        Ok(Self {
            class,
            values,
            selected,
            oid,
            changes: ChangeSet::default(),
            deleted: false,
        })
    }

    /// 该下标的列是否由本次查询取回（`false` 表示只是完整模式里的占位）
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.get(index).copied().unwrap_or(false)
    }

    /// 本次查询实际取回的列下标
    pub fn selected_indices(&self) -> Vec<usize> {
        self.selected
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.then_some(i))
            .collect()
    }

    /// 宿主表
    pub fn table(&self) -> &'a dyn Table {
        self.class
    }

    /// 字段集合
    pub fn fields(&self) -> &Fields {
        self.class.fields()
    }

    /// OBJECTID
    pub fn oid(&self) -> Option<i64> {
        self.oid
    }

    /// 是否已被删除
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }

    /// 是否有未保存的修改
    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty()
    }

    /// 放弃未保存的修改
    pub fn discard_changes(&mut self) {
        self.changes.clear();
    }

    /// 按下标取值（`IRow::get_Value`）
    pub fn value(&self, index: usize) -> Result<&Value> {
        self.values
            .get(index)
            .ok_or_else(|| PgdbError::InvalidArgument(format!("字段下标越界: {index}")))
    }

    /// 按名取值
    pub fn value_by_name(&self, name: &str) -> Result<&Value> {
        let i = self.class.fields().require_field(name)?;
        self.value(i)
    }

    /// 所有值
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// 按下标赋值（`IRow::put_Value`）
    pub fn set_value(&mut self, index: usize, value: Value) -> Result<()> {
        let f = self
            .class
            .fields()
            .field(index)
            .ok_or_else(|| PgdbError::InvalidArgument(format!("字段下标越界: {index}")))?;
        if !f.editable {
            return Err(PgdbError::InvalidArgument(format!("字段 {} 不可编辑", f.name)));
        }
        if self.values.len() <= index {
            return Err(PgdbError::InvalidArgument("字段下标越界".into()));
        }
        self.values[index] = value;
        self.changes.touch(index);
        Ok(())
    }

    /// 按名赋值
    pub fn set_value_by_name(&mut self, name: &str, value: Value) -> Result<()> {
        let i = self.class.fields().require_field(name)?;
        self.set_value(i, value)
    }

    /// 落库（`IRow::Store`）
    pub fn store(&mut self) -> Result<()> {
        if self.deleted {
            return Err(PgdbError::InvalidArgument("该行已被删除".into()));
        }
        if self.changes.is_empty() {
            return Ok(());
        }
        let mut sets: Vec<(String, Value)> = Vec::new();
        for i in self.changes.dirty.iter() {
            let name = self.class.fields().field(*i).map(|f| f.name.clone());
            if let Some(name) = name {
                if name.eq_ignore_ascii_case(self.class.oid_field_name()) {
                    continue;
                }
                sets.push((name, self.values[*i].clone()));
            }
        }
        self.class.store_row(self.oid, &sets)?;
        self.changes.clear();
        Ok(())
    }

    /// 删除（`IRow::Delete`）
    pub fn delete(&mut self) -> Result<()> {
        if self.deleted {
            return Ok(());
        }
        if let Some(oid) = self.oid {
            self.class.delete_row(oid)?;
        }
        self.deleted = true;
        Ok(())
    }

    /// 本次发生变更的字段列表（`(字段名, 新值)`），便于审计/日志
    pub fn changed_pairs(&self) -> Vec<(String, Value)> {
        self.changes
            .dirty
            .iter()
            .filter_map(|i| {
                self.class
                    .fields()
                    .field(*i)
                    .map(|f| (f.name.clone(), self.values[*i].clone()))
            })
            .collect()
    }
}

/// 要素（`IFeature`）：带 Shape 的行
pub struct Feature<'a> {
    class: &'a dyn FeatureClass,
    row: Row<'a>,
}

impl<'a> std::fmt::Debug for Row<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("oid", &self.oid)
            .field("changes", &self.changes.dirty.len())
            .field("values", &self.values)
            .finish()
    }
}

impl<'a> std::fmt::Debug for Feature<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feature")
            .field("class", &self.class.name())
            .field("row", &self.row)
            .finish()
    }
}

impl<'a> Feature<'a> {
    /// 由宿主要素类与原始数据构造
    pub(crate) fn from_row(
        class: &'a dyn FeatureClass,
        data: &crate::datastore::DataTableRow,
    ) -> Result<Self> {
        let row = Row::from_row(class.table(), data, true)?;
        Ok(Self { class, row })
    }

    /// 所属要素类
    pub fn class(&self) -> &'a dyn FeatureClass {
        self.class
    }

    /// OBJECTID
    pub fn oid(&self) -> Option<i64> {
        self.row.oid()
    }

    /// 取出内部 Row
    pub fn into_row(self) -> Row<'a> {
        self.row
    }

    /// 内部 Row 的可变引用
    pub fn row_mut(&mut self) -> &mut Row<'a> {
        &mut self.row
    }

    /// 内部 Row 的引用
    pub fn row(&self) -> &Row<'a> {
        &self.row
    }

    /// 字段集合
    pub fn fields(&self) -> &Fields {
        self.row.fields()
    }

    /// 按下标取值
    pub fn value(&self, index: usize) -> Result<&Value> {
        self.row.value(index)
    }

    /// 按名取值
    pub fn value_by_name(&self, name: &str) -> Result<&Value> {
        self.row.value_by_name(name)
    }

    /// 按下标赋值
    pub fn set_value(&mut self, index: usize, value: Value) -> Result<()> {
        self.row.set_value(index, value)
    }

    /// 按名赋值
    pub fn set_value_by_name(&mut self, name: &str, value: Value) -> Result<()> {
        self.row.set_value_by_name(name, value)
    }

    /// 读取几何（`IFeature::Shape`）
    pub fn geometry(&self) -> Result<Option<Geometry>> {
        let idx = self.row.fields().require_field(self.class.shape_field_name())?;
        match self.row.value(idx)? {
            Value::Blob(b) => Ok(Some(decode_shape(b)?)),
            Value::Null => Ok(None),
            other => Err(PgdbError::Conversion {
                field: self.class.shape_field_name().into(),
                message: format!("Shape 列不是二进制: {other}"),
            }),
        }
    }

    /// 读取几何的副本（`IFeature::ShapeCopy`）
    pub fn geometry_copy(&self) -> Result<Option<Geometry>> {
        self.geometry()
    }

    /// 写入几何（`IFeature::put_Shape`）
    pub fn set_geometry(&mut self, geometry: &Geometry) -> Result<()> {
        let idx = self.row.fields().require_field(self.class.shape_field_name())?;
        let bytes = encode_shape(geometry)?;
        self.row.set_value(idx, Value::Blob(bytes))
    }

    /// 落库（`IFeature::Store`）
    pub fn store(&mut self) -> Result<()> {
        self.row.store()
    }

    /// 删除（`IFeature::Delete`）
    pub fn delete(&mut self) -> Result<()> {
        self.row.delete()
    }

    /// 是否有未保存修改
    pub fn has_changes(&self) -> bool {
        self.row.has_changes()
    }
}
