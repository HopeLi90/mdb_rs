//! 数据集节点 trait：`IWorkspace::get_Datasets` 返回的每个对象都实现它。

use crate::error::Result;
use crate::value::Value;

use super::featureclass::FeatureClass;
use super::featuredataset::FeatureDataset;
use super::metadata::DatasetKind;
use super::table::Table;

/// 目录树节点（`IDataset`）
pub trait DatasetNode: Send + Sync {
    /// 数据集名称（`IDataset::Name`）
    fn name(&self) -> &str;

    /// 限定名（要素数据集内的要素类为 `数据集\要素类`）
    fn qualified_name(&self) -> String;

    /// 数据类型（`IDataset::Type`）
    fn kind(&self) -> DatasetKind;

    /// 所属要素数据集名称
    fn parent_name(&self) -> Option<&str>;

    /// 向下转型为表
    fn as_table(&self) -> Option<&dyn Table> {
        None
    }

    /// 向下转型为要素类
    fn as_feature_class(&self) -> Option<&dyn FeatureClass> {
        None
    }

    /// 向下转型为要素数据集
    fn as_feature_dataset(&self) -> Option<&dyn FeatureDataset> {
        None
    }

    /// 数据集摘要行，便于日志与 CLI 输出
    fn describe_line(&self) -> String {
        format!("{:<10} {}", self.kind().label(), self.qualified_name())
    }

    /// 行数（非表返回 None）
    fn try_row_count(&self) -> Option<Result<u64>> {
        None
    }

    /// 通用属性批量更新入口（便于在未知类型时统一调用）
    fn update_any(&self, _filter: &super::filter::QueryFilter, _sets: &[(String, Value)]) -> Result<u64> {
        Err(crate::error::PgdbError::Unsupported(
            "该数据集类型不支持批量更新".into(),
        ))
    }
}
