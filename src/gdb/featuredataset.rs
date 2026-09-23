//! 要素数据集：`IFeatureDataset`，作为若干要素类的容器。

use std::sync::Arc;

use crate::datastore::SqlBackend;
use crate::error::Result;

use super::dataset::DatasetNode;
use super::metadata::{DatasetKind, SpatialReference};
use super::workspace::DatasetHandle;

/// 要素数据集接口（`IFeatureDataset`）
pub trait FeatureDataset: DatasetNode {
    /// 数据集中所有子数据集（`IFeatureDataset::Subsets`）
    fn subsets(&self) -> &[DatasetHandle];

    /// 空间参考（`IGeoDataset::SpatialReference`）
    fn spatial_reference(&self) -> Option<&SpatialReference>;

    /// 内部的要素类数量
    fn feature_class_count(&self) -> usize {
        self.subsets()
            .iter()
            .filter(|h| h.kind() == DatasetKind::FeatureClass)
            .count()
    }
}

/// Personal Geodatabase 中的要素数据集
pub struct PgdbFeatureDataset {
    backend: Arc<dyn SqlBackend>,
    name: String,
    #[allow(dead_code)]
    srid: Option<i64>,
    spatial_ref: Option<SpatialReference>,
    children: Vec<DatasetHandle>,
}

impl PgdbFeatureDataset {
    /// 构造（由 workspace 负责填充 children）
    pub(crate) fn new(
        backend: Arc<dyn SqlBackend>,
        name: impl Into<String>,
        srid: Option<i64>,
        spatial_ref: Option<SpatialReference>,
    ) -> Self {
        Self {
            backend,
            name: name.into(),
            srid,
            spatial_ref,
            children: Vec::new(),
        }
    }

    /// 追加子数据集
    pub(crate) fn push_child(&mut self, handle: DatasetHandle) {
        self.children.push(handle);
    }

    /// 后端句柄
    pub fn backend(&self) -> &Arc<dyn SqlBackend> {
        &self.backend
    }

    /// 遍历子数据集
    pub fn iter(&self) -> impl Iterator<Item = &DatasetHandle> {
        self.children.iter()
    }
}

impl DatasetNode for PgdbFeatureDataset {
    fn name(&self) -> &str {
        &self.name
    }
    fn qualified_name(&self) -> String {
        self.name.clone()
    }
    fn kind(&self) -> DatasetKind {
        DatasetKind::FeatureDataset
    }
    fn parent_name(&self) -> Option<&str> {
        None
    }
    fn as_feature_dataset(&self) -> Option<&dyn FeatureDataset> {
        Some(self)
    }
    fn try_row_count(&self) -> Option<Result<u64>> {
        None
    }
}

impl FeatureDataset for PgdbFeatureDataset {
    fn subsets(&self) -> &[DatasetHandle] {
        &self.children
    }
    fn spatial_reference(&self) -> Option<&SpatialReference> {
        self.spatial_ref.as_ref()
    }
}
