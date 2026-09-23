//! 地理数据库对象模型：workspace / dataset / table / featureclass / cursor。

pub mod cursor;
pub mod dataset;
pub mod factory;
pub mod featureclass;
pub mod featuredataset;
pub mod filter;
pub mod metadata;
pub mod row;
pub mod table;
pub mod workspace;

pub use cursor::{FeatureIter, InsertFeatureCursor, InsertRowCursor, RowIter};
pub use dataset::DatasetNode;
pub use factory::{AccessWorkspaceFactory, WorkspaceFactory};
pub use featureclass::{FeatureClass, PgdbFeatureClass, WritePolicy};
pub use featuredataset::{FeatureDataset, PgdbFeatureDataset};
pub use filter::{QueryFilter, SpatialFilter, SpatialRel};
pub use metadata::{
    CatalogEntry, DatasetKind, FeatureType, GridInfo, MetadataCatalog, MetadataModel,
    SpatialReference,
};
pub use row::{Feature, Row, RowBuffer};
pub use table::{PgdbTable, Table};
pub use workspace::{
    AccessWorkspace, DatasetEnum, DatasetHandle, FeatureWorkspace, Workspace, WorkspaceOptions,
};

pub use crate::value::{SqlValue, Value};
