//! 工作空间：`IWorkspace` / `IFeatureWorkspace` 的 Rust 版本。
//!
//! 与 ArcEngine 的对应关系：
//!
//! | ArcObjects | 本项目 |
//! |------------|--------|
//! | `IWorkspaceFactory` | [`crate::gdb::factory::WorkspaceFactory`] |
//! | `IWorkspace` | [`Workspace`] |
//! | `IFeatureWorkspace` | [`FeatureWorkspace`] |
//! | `IEnumDataset` | [`DatasetEnum`] |
//! | `IDataset` | [`super::dataset::DatasetNode`] |

use std::collections::HashMap;
use std::sync::Arc;

use crate::datastore::{AccessMode, DataTableRow, SqlBackend};
use crate::error::{PgdbError, Result};
use crate::value::Value;

use super::dataset::DatasetNode;
use super::edit::EditOptions;
use super::featureclass::{FeatureClass, PgdbFeatureClass, WritePolicy};
use super::featuredataset::{FeatureDataset, PgdbFeatureDataset};
use super::filter::QueryFilter;
use super::metadata::{CatalogEntry, DatasetKind, MetadataCatalog, MetadataModel, SpatialReference};
use super::table::{PgdbTable, Table};

/// 工作空间打开选项
#[derive(Debug, Clone, Default)]
pub struct WorkspaceOptions {
    /// 访问权限：只读（jetdb）或读写（ODBC）。**默认只读**（零依赖、防误写）；
    /// 需要写入时显式传 `AccessMode::ReadWrite`。
    pub access_mode: AccessMode,
    /// 几何写入策略
    pub write_policy: WritePolicy,
    /// 是否在打开时立即构造所有要素数据集的子数据集
    pub eager_navigation: bool,
}

/// 目录中的一个已打开数据集
pub enum DatasetHandle {
    /// 独立或数据集内的表
    Table(Arc<PgdbTable>),
    /// 要素类
    FeatureClass(Arc<PgdbFeatureClass>),
    /// 要素数据集
    FeatureDataset(Arc<PgdbFeatureDataset>),
}

impl DatasetHandle {
    /// 名称
    pub fn name(&self) -> String {
        match self {
            DatasetHandle::Table(t) => t.name().to_string(),
            DatasetHandle::FeatureClass(fc) => fc.name().to_string(),
            DatasetHandle::FeatureDataset(fd) => fd.name().to_string(),
        }
    }

    /// 限定名
    pub fn qualified_name(&self) -> String {
        match self {
            DatasetHandle::Table(t) => t.qualified_name(),
            DatasetHandle::FeatureClass(fc) => fc.qualified_name(),
            DatasetHandle::FeatureDataset(fd) => fd.qualified_name(),
        }
    }

    /// 类型
    pub fn kind(&self) -> DatasetKind {
        match self {
            DatasetHandle::Table(t) => t.kind(),
            DatasetHandle::FeatureClass(fc) => fc.kind(),
            DatasetHandle::FeatureDataset(fd) => fd.kind(),
        }
    }

    /// 所属要素数据集
    pub fn parent_name(&self) -> Option<String> {
        match self {
            DatasetHandle::Table(t) => t.parent_name().map(|s| s.to_string()),
            DatasetHandle::FeatureClass(fc) => fc.parent_name().map(|s| s.to_string()),
            DatasetHandle::FeatureDataset(_) => None,
        }
    }

    /// 作为表
    pub fn as_table(&self) -> Option<&dyn Table> {
        match self {
            DatasetHandle::Table(t) => Some(t.as_ref()),
            DatasetHandle::FeatureClass(fc) => Some(fc.as_ref()),
            DatasetHandle::FeatureDataset(_) => None,
        }
    }

    /// 作为要素类
    pub fn as_feature_class(&self) -> Option<&dyn FeatureClass> {
        match self {
            DatasetHandle::FeatureClass(fc) => Some(fc.as_ref()),
            _ => None,
        }
    }

    /// 作为要素数据集
    pub fn as_feature_dataset(&self) -> Option<&dyn FeatureDataset> {
        match self {
            DatasetHandle::FeatureDataset(fd) => Some(fd.as_ref()),
            _ => None,
        }
    }

    /// 摘要行
    pub fn describe_line(&self) -> String {
        let label = self.kind().label();
        let extra = match self.kind() {
            DatasetKind::FeatureDataset => {
                let n = self
                    .as_feature_dataset()
                    .map(|d| d.feature_class_count())
                    .unwrap_or(0);
                format!("（含 {n} 个要素类）")
            }
            DatasetKind::FeatureClass => match self.as_feature_class() {
                Some(fc) => format!("（几何类型：{}）", fc.shape_type().label()),
                None => String::new(),
            },
            _ => String::new(),
        };
        let rows = match self.as_table() {
            Some(t) => match Table::row_count(t, &QueryFilter::new()) {
                Ok(n) => format!("，{n} 行"),
                Err(_) => String::new(),
            },
            None => String::new(),
        };
        format!(
            "{}{}{extra}{rows}",
            crate::text::pad_display(label, 12),
            self.qualified_name()
        )
    }
}

/// 数据集枚举器（`IEnumDataset`）
pub struct DatasetEnum {
    items: std::vec::IntoIter<DatasetHandle>,
}

impl DatasetEnum {
    /// 构造
    pub fn new(items: Vec<DatasetHandle>) -> Self {
        Self {
            items: items.into_iter(),
        }
    }

    /// 取下一个数据集（`IEnumDataset::Next`），返回 None 表示结束
    pub fn next_dataset(&mut self) -> Option<DatasetHandle> {
        self.items.next()
    }

    /// 重置到开头（`IEnumDataset::Reset`）
    pub fn reset(&mut self, items: Vec<DatasetHandle>) {
        self.items = items.into_iter();
    }
}

/// 工作空间接口（`IWorkspace`）
pub trait Workspace {
    /// 文件路径（`IWorkspace::PathName`）
    fn path(&self) -> &str;

    /// 后端句柄
    fn backend(&self) -> &Arc<dyn SqlBackend>;

    /// 元数据目录
    fn catalog(&self) -> &MetadataCatalog;

    /// 工作空间选项
    fn options(&self) -> &WorkspaceOptions;

    /// 访问权限（只读 / 读写），对应 ArcObjects 中工作空间「是否可编辑」的概念
    fn access_mode(&self) -> AccessMode {
        self.options().access_mode
    }

    /// 是否为只读工作空间（请求了只读权限即视为只读）
    fn is_read_only(&self) -> bool {
        self.access_mode() == AccessMode::ReadOnly
    }

    /// 是否真正可写：既请求了读写权限，后端又具备写能力
    /// （Linux 下 ODBC 走 MDBTools 驱动时为只读，因此即使请求读写也返回 false）
    fn can_write(&self) -> bool {
        !self.is_read_only() && self.backend().capabilities().writable
    }

    /// 顶层数据集（`IWorkspace::get_Datasets`）
    fn datasets(&self) -> Result<DatasetEnum>;

    /// 扁平列出所有数据集（含要素数据集内部的要素类）
    fn all_datasets(&self) -> Result<Vec<DatasetHandle>>;

    /// 执行任意 SQL（需要后端支持）
    fn execute_sql(&self, sql: &str) -> Result<u64> {
        self.backend().raw_execute(sql)
    }

    /// 查询任意 SQL（需要后端支持）
    fn query_sql(&self, sql: &str) -> Result<Vec<DataTableRow>> {
        self.backend().raw_query(sql)
    }

    /// 按名称查找目录条目
    fn find_entry(&self, name: &str) -> Result<CatalogEntry> {
        self.catalog()
            .find(name)
            .cloned()
            .ok_or_else(|| PgdbError::not_found(name))
    }
}

/// 要素工作空间接口（`IFeatureWorkspace`）
pub trait FeatureWorkspace: Workspace {
    /// 打开表（`IFeatureWorkspace::OpenTable`）
    fn open_table(&self, name: &str) -> Result<Arc<PgdbTable>>;

    /// 打开要素类（`IFeatureWorkspace::OpenFeatureClass`）
    fn open_feature_class(&self, name: &str) -> Result<Arc<PgdbFeatureClass>>;

    /// 打开要素数据集（`IFeatureWorkspace::OpenFeatureDataset`）
    fn open_feature_dataset(&self, name: &str) -> Result<Arc<PgdbFeatureDataset>>;

    /// 按名称打开任意数据集：支持 `数据集\要素类` 的限定名
    fn open_dataset(&self, name: &str) -> Result<DatasetHandle> {
        let entry = self.find_entry(name)?;
        self.open_entry(&entry)
    }

    /// 由目录条目打开
    fn open_entry(&self, entry: &CatalogEntry) -> Result<DatasetHandle> {
        match entry.kind {
            DatasetKind::FeatureClass => Ok(DatasetHandle::FeatureClass(
                self.open_feature_class_entry(entry)?,
            )),
            DatasetKind::Table => Ok(DatasetHandle::Table(self.open_table_entry(entry)?)),
            DatasetKind::FeatureDataset => Ok(DatasetHandle::FeatureDataset(
                self.open_feature_dataset(entry.name.as_str())?,
            )),
            DatasetKind::Other => Err(PgdbError::Unsupported(format!(
                "暂不支持打开该类型的对象: {}",
                entry.name
            ))),
        }
    }

    /// 由条目打开要素类
    fn open_feature_class_entry(&self, entry: &CatalogEntry) -> Result<Arc<PgdbFeatureClass>>;

    /// 由条目打开表
    fn open_table_entry(&self, entry: &CatalogEntry) -> Result<Arc<PgdbTable>>;
}

/// 个人地理数据库工作空间
pub struct AccessWorkspace {
    backend: Arc<dyn SqlBackend>,
    catalog: MetadataCatalog,
    path: String,
    options: WorkspaceOptions,
}

impl AccessWorkspace {
    /// 由后端与路径构造，同时完成元数据采集
    pub fn open(backend: Arc<dyn SqlBackend>, path: impl Into<String>) -> Result<Self> {
        let catalog = MetadataCatalog::discover(backend.as_ref())?;
        Ok(Self {
            backend,
            catalog,
            path: path.into(),
            options: WorkspaceOptions::default(),
        })
    }

    /// 使用指定选项构造
    pub fn open_with_options(
        backend: Arc<dyn SqlBackend>,
        path: impl Into<String>,
        options: WorkspaceOptions,
    ) -> Result<Self> {
        let mut ws = Self::open(backend, path)?;
        ws.options = options;
        Ok(ws)
    }

    /// 元数据模型类型（用于诊断遇到的是哪一版 GDB 系统表）
    pub fn metadata_model(&self) -> MetadataModel {
        self.catalog.model
    }

    /// 空间参考查询
    pub fn spatial_ref(&self, srid: i64) -> Option<&SpatialReference> {
        self.catalog.spatial_ref(srid)
    }

    /// 递归打印目录树
    pub fn describe_tree(&self) -> Result<String> {
        let mut out = String::new();
        let top = self.datasets()?;
        let mut enum_ = top;
        while let Some(handle) = enum_.next_dataset() {
            out.push_str(&handle.describe_line());
            out.push('\n');
            if let Some(fd) = handle.as_feature_dataset() {
                for child in fd.subsets() {
                    out.push_str(&format!("    {}\n", child.describe_line()));
                }
            }
        }
        Ok(out)
    }

    fn aliases_for(&self, table: &str) -> Option<&HashMap<String, String>> {
        self.catalog.aliases_for(table)
    }

    /// 批量把某个字段更新为固定值（便捷 API）。
    ///
    /// 内部走 [`Table::update_searched_rows`]（`ITable::UpdateSearchedRows`），
    /// 受同样的全表防护约束：`filter` 为空过滤器时会被拒绝。
    pub fn update_field_values(
        &self,
        dataset: &str,
        field: &str,
        value: &Value,
        filter: &QueryFilter,
    ) -> Result<u64> {
        let handle = self.open_dataset(dataset)?;
        match handle.as_table() {
            Some(t) => t
                .update_searched_rows(
                    &[(field.to_string(), value.clone())],
                    filter,
                    EditOptions::default(),
                )
                .map(|r| r.affected),
            None => Err(PgdbError::Unsupported(format!("{dataset} 不是表/要素类"))),
        }
    }
}

impl Workspace for AccessWorkspace {
    fn path(&self) -> &str {
        &self.path
    }
    fn backend(&self) -> &Arc<dyn SqlBackend> {
        &self.backend
    }
    fn catalog(&self) -> &MetadataCatalog {
        &self.catalog
    }
    fn options(&self) -> &WorkspaceOptions {
        &self.options
    }
    fn datasets(&self) -> Result<DatasetEnum> {
        let mut handles = Vec::new();
        for entry in self.catalog.top_level() {
            handles.push(self.open_entry(entry)?);
        }
        for entry in self.catalog.feature_datasets() {
            handles.push(self.open_entry(entry)?);
        }
        Ok(DatasetEnum::new(handles))
    }

    fn all_datasets(&self) -> Result<Vec<DatasetHandle>> {
        let mut out = Vec::new();
        for entry in self.catalog.entries() {
            if entry.kind == DatasetKind::FeatureDataset {
                continue;
            }
            out.push(self.open_entry(entry)?);
        }
        Ok(out)
    }
}

impl FeatureWorkspace for AccessWorkspace {
    fn open_table(&self, name: &str) -> Result<Arc<PgdbTable>> {
        let entry = self.find_entry(name)?;
        if entry.kind != DatasetKind::Table {
            return Err(PgdbError::InvalidArgument(format!(
                "{name} 不是一张表（{}）",
                entry.kind.label()
            )));
        }
        self.open_table_entry(&entry)
    }

    fn open_feature_class(&self, name: &str) -> Result<Arc<PgdbFeatureClass>> {
        let entry = self.find_entry(name)?;
        if entry.kind != DatasetKind::FeatureClass {
            return Err(PgdbError::InvalidArgument(format!(
                "{name} 不是要素类（{}）",
                entry.kind.label()
            )));
        }
        self.open_feature_class_entry(&entry)
    }

    fn open_feature_dataset(&self, name: &str) -> Result<Arc<PgdbFeatureDataset>> {
        let entry = self
            .catalog
            .feature_datasets()
            .find(|e| e.name.eq_ignore_ascii_case(name))
            .cloned()
            .ok_or_else(|| PgdbError::not_found(name))?;
        let sr = entry.srid.and_then(|id| self.catalog.spatial_ref(id).cloned());
        let mut fd = PgdbFeatureDataset::new(
            self.backend.clone(),
            entry.name.clone(),
            entry.srid,
            sr.clone(),
        );
        for child in self.catalog.children_of(&entry.name) {
            fd.push_child(self.open_entry(child)?);
        }
        Ok(Arc::new(fd))
    }

    fn open_feature_class_entry(&self, entry: &CatalogEntry) -> Result<Arc<PgdbFeatureClass>> {
        let sr = entry.srid.and_then(|id| self.catalog.spatial_ref(id).cloned());
        Ok(Arc::new(PgdbFeatureClass::open(
            self.backend.clone(),
            entry,
            sr,
            self.aliases_for(&entry.table_name),
            self.options.write_policy.clone(),
        )?))
    }

    fn open_table_entry(&self, entry: &CatalogEntry) -> Result<Arc<PgdbTable>> {
        Ok(Arc::new(PgdbTable::open_with_aliases(
            self.backend.clone(),
            entry,
            self.aliases_for(&entry.table_name),
        )?))
    }
}
