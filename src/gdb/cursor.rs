//! 游标：对应 ArcObjects 的 `ICursor / IFeatureCursor / IInsertCursor`。
//!
//! 与 COM 世界的差异：Rust 里游标借用宿主类的引用（`'a`），因此不需要 Release。
//! 典型用法与 ArcEngine 一致：
//!
//! ```rust,ignore
//! let mut cursor = feature_class.update(QueryFilter::new())?;   // IFeatureClass::Update
//! while let Some(mut feature) = cursor.next_feature()? {        // IFeatureCursor::NextFeature
//!     feature.set_geometry(&geom)?;                             // IFeature::put_Shape
//!     feature.set_value_by_name("NAME", Value::String("A".into()))?;
//!     feature.store()?;                                         // IFeature::Store
//! }
//! ```

use crate::error::Result;
use crate::geom::Geometry;
use crate::value::Value;

use super::featureclass::FeatureClass;
use super::row::{Feature, Row, RowBuffer};
use super::table::Table;

/// 行游标（`ICursor`）
pub struct RowIter<'a> {
    class: &'a dyn Table,
    rows: std::vec::IntoIter<crate::datastore::DataTableRow>,
}

impl<'a> RowIter<'a> {
    /// 构造游标
    pub(crate) fn new(
        class: &'a dyn Table,
        rows: Vec<crate::datastore::DataTableRow>,
    ) -> Self {
        Self {
            class,
            rows: rows.into_iter(),
        }
    }

    /// 取下一行（`ICursor::NextRow`）
    pub fn next_row(&mut self) -> Result<Option<Row<'a>>> {
        match self.rows.next() {
            Some(data) => Ok(Some(Row::from_row(self.class, &data, true)?)),
            None => Ok(None),
        }
    }

    /// 剩余行数提示
    pub fn remaining_hint(&self) -> usize {
        self.rows.size_hint().0
    }

    /// 收集全部行（数据量大时请改用 while 循环）
    pub fn collect_rows(self) -> Result<Vec<Row<'a>>> {
        let mut out = Vec::new();
        let mut this = self;
        while let Some(r) = this.next_row()? {
            out.push(r);
        }
        Ok(out)
    }

    /// 遍历所有行并在闭包中修改后自动 Store（更新游标的常见写法）
    pub fn for_each_update<F>(&mut self, mut f: F) -> Result<usize>
    where
        F: FnMut(&mut Row<'a>) -> Result<()>,
    {
        let mut changed = 0usize;
        while let Some(mut row) = self.next_row()? {
            f(&mut row)?;
            if row.has_changes() {
                row.store()?;
                changed += 1;
            }
        }
        Ok(changed)
    }
}

/// 要素游标（`IFeatureCursor`）
pub struct FeatureIter<'a> {
    class: &'a dyn FeatureClass,
    rows: std::vec::IntoIter<crate::datastore::DataTableRow>,
}

impl<'a> FeatureIter<'a> {
    /// 构造要素游标
    pub(crate) fn new(
        class: &'a dyn FeatureClass,
        rows: Vec<crate::datastore::DataTableRow>,
    ) -> Self {
        Self {
            class,
            rows: rows.into_iter(),
        }
    }

    /// 取下一要素（`IFeatureCursor::NextFeature`）
    pub fn next_feature(&mut self) -> Result<Option<Feature<'a>>> {
        match self.rows.next() {
            Some(data) => Ok(Some(Feature::from_row(self.class, &data)?)),
            None => Ok(None),
        }
    }

    /// 剩余要素数提示
    pub fn remaining_hint(&self) -> usize {
        self.rows.size_hint().0
    }

    /// 遍历全部要素，在闭包中完成修改并通过 Store 落库
    pub fn for_each_update<F>(&mut self, mut f: F) -> Result<usize>
    where
        F: FnMut(&mut Feature<'a>) -> Result<()>,
    {
        let mut changed = 0usize;
        while let Some(mut feature) = self.next_feature()? {
            f(&mut feature)?;
            if feature.has_changes() {
                feature.store()?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// 收集全部要素
    pub fn collect_features(self) -> Result<Vec<Feature<'a>>> {
        let mut this = self;
        let mut out = Vec::new();
        while let Some(f) = this.next_feature()? {
            out.push(f);
        }
        Ok(out)
    }
}

/// 插入游标（`IInsertCursor` / `IFeatureCursor` 的 buffer 模式）
pub struct InsertRowCursor<'a> {
    class: &'a dyn Table,
    buffer: RowBuffer,
    inserted: usize,
}

impl<'a> InsertRowCursor<'a> {
    /// 构造插入游标
    pub(crate) fn new(class: &'a dyn Table) -> Self {
        Self {
            class,
            buffer: RowBuffer::new(),
            inserted: 0,
        }
    }

    /// 缓冲区（`ITable::CreateRowBuffer`）
    pub fn buffer(&mut self) -> &mut RowBuffer {
        &mut self.buffer
    }

    /// 提交当前缓冲区中的一行（`ICursor::InsertRow`）。清空缓冲区以便复用。
    pub fn insert_row(&mut self) -> Result<i64> {
        let pairs = std::mem::take(&mut self.buffer).into_pairs();
        if pairs.is_empty() {
            return Err(crate::error::PgdbError::InvalidArgument(
                "缓冲区为空，无可插入的数据".into(),
            ));
        }
        let oid = self.class.insert_row(&pairs)?;
        self.inserted += 1;
        Ok(oid)
    }

    /// 已插入行数
    pub fn inserted_count(&self) -> usize {
        self.inserted
    }

    /// 刷新（`IFeatureCursor::Flush`）
    pub fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// 要素插入游标
pub struct InsertFeatureCursor<'a> {
    class: &'a dyn FeatureClass,
    buffer: RowBuffer,
    inserted: usize,
}

impl<'a> InsertFeatureCursor<'a> {
    /// 构造
    pub(crate) fn new(class: &'a dyn FeatureClass) -> Self {
        Self {
            class,
            buffer: RowBuffer::new(),
            inserted: 0,
        }
    }

    /// 缓冲区（`IFeatureClass::CreateFeatureBuffer`）
    pub fn buffer(&mut self) -> &mut RowBuffer {
        &mut self.buffer
    }

    /// 写入几何
    pub fn set_geometry(&mut self, geometry: &Geometry) -> Result<()> {
        let name = self.class.shape_field_name();
        self.buffer.set_geometry(name, geometry)
    }

    /// 提交当前要素（`IFeatureCursor::InsertFeature`）
    pub fn insert_feature(&mut self) -> Result<i64> {
        let pairs = std::mem::take(&mut self.buffer).into_pairs();
        if pairs.is_empty() {
            return Err(crate::error::PgdbError::InvalidArgument(
                "要素缓冲区为空".into(),
            ));
        }
        let oid = self.class.insert_feature(&pairs)?;
        self.inserted += 1;
        Ok(oid)
    }

    /// 已插入要素数
    pub fn inserted_count(&self) -> usize {
        self.inserted
    }

    /// 刷新
    pub fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// 通用辅助：把一批字段值转成（列名，值）对，便于批量更新调用方使用
pub fn pairs(values: &[(&str, Value)]) -> Vec<(String, Value)> {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}
