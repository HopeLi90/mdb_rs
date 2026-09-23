//! 地理数据库元数据目录：读取 GDB_* 系统表，还原"独立表 / 独立要素类 / 要素数据集中的要素类"的结构。
//!
//! 支持两种元数据模型：
//!
//! | 模型 | 出现版本 | 系统表 |
//! |------|---------|-------|
//! | Items（新） | ArcGIS 9.2+ / 10.x PGDB、FGDB | `GDB_Items` `GDB_ItemTypes` `GDB_ItemRelationships` `GDB_ItemRelationshipTypes` |
//! | Legacy（旧） | ArcGIS 8.x / 9.0-9.1 PGDB | `GDB_ObjectClasses` `GDB_FeatureClasses` `GDB_FeatureDataset` `GDB_GeomColumns` `GDB_SpatialRefs` `GDB_FieldInfo` |
//!
//! 各版本的列名存在细微差异，所有取值都通过 [`pick`] 做候选名容错。

use std::collections::HashMap;

use crate::datastore::{DataTableRow, Predicate, SqlBackend, SqlValue};
use crate::error::{PgdbError, Result};
use crate::field::FieldType;
use crate::geom::{Envelope, GeometryType};

/// 数据源中发现的对象类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DatasetKind {
    /// 独立（或在要素数据集内的）要素类
    FeatureClass,
    /// 独立表
    Table,
    /// 要素数据集容器
    FeatureDataset,
    /// 其他（关系类、拓扑、域等）
    Other,
}

impl DatasetKind {
    /// 中文标签
    pub fn label(&self) -> &'static str {
        match self {
            DatasetKind::FeatureClass => "要素类",
            DatasetKind::Table => "表",
            DatasetKind::FeatureDataset => "要素数据集",
            DatasetKind::Other => "其他对象",
        }
    }
}

/// 要素类型（`esriFeatureType`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FeatureType {
    /// 简单要素
    #[default]
    Simple,
    /// 简单交点要素（几何网络）
    SimpleJunction,
    /// 简单边要素
    SimpleEdge,
    /// 复杂边要素
    ComplexEdge,
    /// 注记
    Annotation,
    /// 尺寸（Dimension）
    Dimension,
    /// 栅格目录项
    RasterCatalogItem,
}

impl FeatureType {
    /// 由整数构造
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => FeatureType::Simple,
            7 => FeatureType::SimpleJunction,
            8 => FeatureType::SimpleEdge,
            10 => FeatureType::ComplexEdge,
            11 => FeatureType::Annotation,
            13 => FeatureType::Dimension,
            14 => FeatureType::RasterCatalogItem,
            _ => FeatureType::Simple,
        }
    }

    /// 中文名称
    pub fn label(self) -> &'static str {
        match self {
            FeatureType::Simple => "简单要素",
            FeatureType::SimpleJunction => "简单交点",
            FeatureType::SimpleEdge => "简单边",
            FeatureType::ComplexEdge => "复杂边",
            FeatureType::Annotation => "注记",
            FeatureType::Dimension => "尺寸标注",
            FeatureType::RasterCatalogItem => "栅格目录项",
        }
    }

    /// 转为 `esriFeatureType` 整数值
    pub fn as_i32(self) -> i32 {
        match self {
            FeatureType::Simple => 1,
            FeatureType::SimpleJunction => 7,
            FeatureType::SimpleEdge => 8,
            FeatureType::ComplexEdge => 10,
            FeatureType::Annotation => 11,
            FeatureType::Dimension => 13,
            FeatureType::RasterCatalogItem => 14,
        }
    }
}

/// 空间索引网格参数（写 shape 索引时使用）
#[derive(Debug, Clone, Copy)]
pub struct GridInfo {
    /// 网格原点 X
    pub origin_x: f64,
    /// 网格原点 Y
    pub origin_y: f64,
    /// 一级网格尺寸
    pub grid_size: f64,
}

impl Default for GridInfo {
    fn default() -> Self {
        Self {
            origin_x: 0.0,
            origin_y: 0.0,
            grid_size: 420.0,
        }
    }
}

/// 空间参考
#[derive(Debug, Clone, Default)]
pub struct SpatialReference {
    /// SRID
    pub srid: Option<i64>,
    /// X 偏移
    pub false_x: f64,
    /// Y 偏移
    pub false_y: f64,
    /// XY 缩放单位
    pub xy_units: f64,
    /// Z 偏移
    pub false_z: f64,
    /// Z 缩放单位
    pub z_units: f64,
    /// M 偏移
    pub false_m: f64,
    /// M 缩放单位
    pub m_units: f64,
    /// XY 容差
    pub xy_tolerance: f64,
    /// WKT 文本（可能为空）
    pub wkt: Option<String>,
}

/// 目录中一个数据集的元数据
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// 逻辑名称
    pub name: String,
    /// 物理业务表名
    pub table_name: String,
    /// 类型
    pub kind: DatasetKind,
    /// 所属要素数据集（顶层数据集为空）
    pub parent: Option<String>,
    /// 几何字段名
    pub shape_field: Option<String>,
    /// 几何类型
    pub shape_type: Option<GeometryType>,
    /// 要素类型
    pub feature_type: FeatureType,
    /// SRID
    pub srid: Option<i64>,
    /// OBJECTID 字段名
    pub oid_field: String,
    /// 图层范围
    pub extent: Option<Envelope>,
    /// 空间索引网格
    pub grid: Option<GridInfo>,
    /// ObjectClassID（旧模型）
    pub object_class_id: Option<i64>,
    /// Item UUID（新模型）
    pub uuid: Option<String>,
    /// GDB_Items.Definition 中的 XML（新模型，可能为 None）
    pub definition: Option<String>,
}

impl CatalogEntry {
    /// 是否为要素类
    pub fn is_feature_class(&self) -> bool {
        matches!(self.kind, DatasetKind::FeatureClass)
    }
    /// 该数据集在目录树中的完整限定名（数据集\要素类）
    pub fn qualified_name(&self) -> String {
        match &self.parent {
            Some(p) => format!("{p}\\{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// 元数据目录
#[derive(Debug, Clone, Default)]
pub struct MetadataCatalog {
    entries: Vec<CatalogEntry>,
    spatial_refs: HashMap<i64, SpatialReference>,
    field_aliases: HashMap<String, HashMap<String, String>>,
    /// 使用的元数据模型
    pub model: MetadataModel,
}

/// 元数据模型类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetadataModel {
    /// 未知（没有任何 GDB_* 系统表）
    #[default]
    Unknown,
    /// ArcGIS 9.2+ 的 GDB_Items 模型
    Items,
    /// ArcGIS 8.x/9.0 的旧模型
    Legacy,
}

/// 从多个候选列名中取值
pub fn pick<'a>(row: &'a DataTableRow, candidates: &[&str]) -> Option<&'a SqlValue> {
    candidates.iter().find_map(|c| row.get(c))
}

/// 取字符串
fn pick_str(row: &DataTableRow, candidates: &[&str]) -> Option<String> {
    pick(row, candidates).and_then(|v| match v {
        SqlValue::Text(s) | SqlValue::Guid(s) => Some(s.trim().to_string()),
        SqlValue::Decimal(s) => Some(s.clone()),
        other if !other.is_null() => Some(other.to_display_string()),
        _ => None,
    })
}

/// 取整数
fn pick_i64(row: &DataTableRow, candidates: &[&str]) -> Option<i64> {
    pick(row, candidates).and_then(|v| v.to_i64())
}

/// 取浮点
fn pick_f64(row: &DataTableRow, candidates: &[&str]) -> Option<f64> {
    pick(row, candidates).and_then(|v| v.to_f64())
}

/// 取二进制
fn pick_bytes(row: &DataTableRow, candidates: &[&str]) -> Option<Vec<u8>> {
    pick(row, candidates).and_then(|v| v.to_binary().map(|b| b.to_vec()))
}

impl MetadataCatalog {
    /// 从后端读取全部元数据
    pub fn discover(backend: &dyn SqlBackend) -> Result<Self> {
        let has = |name: &str| -> Result<bool> {
            backend.table_names().map(|names| {
                names.iter().any(|n| n.eq_ignore_ascii_case(name))
            })
        };

        if has("GDB_Items")? && has("GDB_ItemTypes")? {
            Ok(Self::discover_items_model(backend)?)
        } else if has("GDB_ObjectClasses")? {
            Ok(Self::discover_legacy_model(backend)?)
        } else {
            Err(PgdbError::Metadata(
                "未找到 GDB_* 系统表，该文件可能不是 ESRI Personal Geodatabase".into(),
            ))
        }
    }

    // ---------------------------------------------------------------- Items

    fn discover_items_model(backend: &dyn SqlBackend) -> Result<Self> {
        let mut catalog = MetadataCatalog {
            model: MetadataModel::Items,
            ..Default::default()
        };

        // 1) 类型字典 UUID -> 名称
        let mut type_map: HashMap<String, String> = HashMap::new();
        for row in backend.select("GDB_ItemTypes", &[], &Predicate::All)? {
            if let (Some(uuid), Some(name)) = (
                pick_str(&row, &["UUID", "Uuid"]),
                pick_str(&row, &["Name"]),
            ) {
                type_map.insert(uuid.to_lowercase(), name);
            }
        }

        // 2) 关系类型字典
        let mut rel_type_map: HashMap<String, String> = HashMap::new();
        if backend.table_exists("GDB_ItemRelationshipTypes")? {
            for row in backend.select("GDB_ItemRelationshipTypes", &[], &Predicate::All)? {
                if let (Some(uuid), Some(name)) =
                    (pick_str(&row, &["UUID", "Uuid"]), pick_str(&row, &["Name"]))
                {
                    rel_type_map.insert(uuid.to_lowercase(), name);
                }
            }
        }

        // 3) 读取所有 item
        let mut items: Vec<DataTableRow> = backend.select("GDB_Items", &[], &Predicate::All)?;
        // uuid -> kind
        let mut kind_of: HashMap<String, DatasetKind> = HashMap::new();
        let mut name_of: HashMap<String, String> = HashMap::new();
        for row in &items {
            let type_uuid = pick_str(row, &["Type"]).unwrap_or_default().to_lowercase();
            let kind = match type_map.get(&type_uuid).map(|s| s.to_lowercase()) {
                Some(t) if t.contains("feature class") => DatasetKind::FeatureClass,
                Some(t) if t.contains("feature dataset") => DatasetKind::FeatureDataset,
                Some(t) if t == "table" => DatasetKind::Table,
                _ => DatasetKind::Other,
            };
            if let Some(uuid) = pick_str(row, &["UUID", "Uuid"]) {
                kind_of.insert(uuid.to_lowercase(), kind);
                name_of.insert(
                    uuid.to_lowercase(),
                    pick_str(row, &["Name"]).unwrap_or_default(),
                );
            }
        }

        // 4) 关系：确定父子归属
        let mut parent_of: HashMap<String, String> = HashMap::new();
        if backend.table_exists("GDB_ItemRelationships")? {
            for row in backend.select("GDB_ItemRelationships", &[], &Predicate::All)? {
                let rel_type = pick_str(&row, &["Type"])
                    .map(|t| rel_type_map.get(&t.to_lowercase()).cloned().unwrap_or_default())
                    .unwrap_or_default();
                let is_fds_rel = rel_type.to_lowercase().contains("featuredataset");
                let origin = pick_str(&row, &["OriginID", "Originid"]);
                let dest = pick_str(&row, &["DestID", "Destid", "DestinationID"]);
                let (Some(origin), Some(dest)) = (origin, dest) else {
                    continue;
                };
                // 两端中类型为 FeatureDataset 的那一侧作为父级
                let dest_is_fd =
                    matches!(kind_of.get(&dest.to_lowercase()), Some(DatasetKind::FeatureDataset));
                let origin_is_fd =
                    matches!(kind_of.get(&origin.to_lowercase()), Some(DatasetKind::FeatureDataset));
                if let Some(parent_uuid) = match (dest_is_fd, origin_is_fd) {
                    (true, _) => Some(dest.clone()),
                    (false, true) if is_fds_rel => Some(origin.clone()),
                    _ => None,
                } {
                    let child = if parent_uuid == dest {
                        origin.clone()
                    } else {
                        dest.clone()
                    };
                    if let Some(p) = name_of.get(&parent_uuid.to_lowercase()) {
                        if let Some(c) = name_of.get(&child.to_lowercase()) {
                            parent_of.insert(c.clone(), p.clone());
                        }
                    }
                }
            }
        }

        // 5) 空间参考与几何列（Items 模型通常没有 GDB_GeomColumns，容错处理）
        catalog.load_spatial_refs(backend)?;
        let geom_cols = load_geom_columns(backend)?;

        items.sort_by_key(|a| pick_i64(a, &["OBJECTID"]));
        for row in &items {
            let type_uuid = pick_str(row, &["Type"]).unwrap_or_default().to_lowercase();
            let kind = match type_map.get(&type_uuid).map(|s| s.to_lowercase()) {
                Some(t) if t.contains("feature class") => DatasetKind::FeatureClass,
                Some(t) if t.contains("feature dataset") => DatasetKind::FeatureDataset,
                Some(t) if t == "table" => DatasetKind::Table,
                _ => DatasetKind::Other,
            };
            if matches!(kind, DatasetKind::Other) {
                continue;
            }
            let name = pick_str(row, &["Name"]).unwrap_or_default();
            if kind == DatasetKind::FeatureDataset {
                let mut e = CatalogEntry {
                    name: name.clone(),
                    table_name: name.clone(),
                    kind,
                    parent: None,
                    oid_field: oid_field_of(backend, ""),
                    ..entry_defaults()
                };
                e.oid_field = String::new();
                catalog.entries.push(e);
                continue;
            }
            let table_name = pick_str(row, &["PhysicalName"])
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| name.clone());
            let entry_type = kind == DatasetKind::FeatureClass;
            let shape_field = pick_str(row, &["DatasetInfo1"]).filter(|s| !s.is_empty());
            let shape_type = pick_i64(row, &["DatasetSubtype2"])
                .and_then(|v| GeometryType::from_shape_type(v as i32).ok());
            let feature_type =
                FeatureType::from_i32(pick_i64(row, &["DatasetSubtype1"]).unwrap_or(1) as i32);

            let mut entry = CatalogEntry {
                name: name.clone(),
                table_name: table_name.clone(),
                kind: if entry_type { DatasetKind::FeatureClass } else { DatasetKind::Table },
                parent: parent_of.get(&name).cloned(),
                shape_field,
                shape_type,
                feature_type,
                oid_field: oid_field_of(backend, &table_name),
                uuid: pick_str(row, &["UUID", "Uuid"]),
                definition: pick_str(row, &["Definition"]),
                ..entry_defaults()
            };
            apply_geom_column(&mut entry, &geom_cols);
            catalog.entries.push(entry);
        }
        catalog.load_field_aliases(backend)?;
        Ok(catalog)
    }

    // --------------------------------------------------------------- Legacy

    fn discover_legacy_model(backend: &dyn SqlBackend) -> Result<Self> {
        let mut catalog = MetadataCatalog {
            model: MetadataModel::Legacy,
            ..Default::default()
        };
        catalog.load_spatial_refs(backend)?;
        let geom_cols = load_geom_columns(backend)?;

        // 要素数据集：名称 -> (ID, SRID)
        let mut fd_names: HashMap<i64, String> = HashMap::new();
        if backend.table_exists("GDB_FeatureDataset")? {
            for row in backend.select("GDB_FeatureDataset", &[], &Predicate::All)? {
                let id = pick_i64(&row, &["ID", "ObjectClassID", "DatasetID"]);
                let name = pick_str(&row, &["Name", "DatasetName"]);
                if let (Some(id), Some(name)) = (id, name) {
                    fd_names.insert(id, name);
                }
            }
        }

        // 要素类登记：ObjectClassID -> (feature_type, geometry_type, shape_field)
        #[derive(Default)]
        struct FcInfo {
            feature_type: i64,
            geometry_type: Option<i64>,
            shape_field: Option<String>,
        }
        let mut fc_map: HashMap<i64, FcInfo> = HashMap::new();
        if backend.table_exists("GDB_FeatureClasses")? {
            for row in backend.select("GDB_FeatureClasses", &[], &Predicate::All)? {
                let ocid = pick_i64(&row, &["ObjectClassID", "ObjectClassId", "ID", "ClassID"]);
                let Some(ocid) = ocid else { continue };
                fc_map.insert(
                    ocid,
                    FcInfo {
                        feature_type: pick_i64(&row, &["FeatureType"]).unwrap_or(1),
                        geometry_type: pick_i64(&row, &["GeometryType", "ShapeType"]),
                        shape_field: pick_str(&row, &["ShapeFieldName", "ShapeField", "FieldName"]),
                    },
                );
            }
        }

        // 要素数据集本身也作为目录条目登记（供 IFeatureDataset 导航使用）
        for (id, name) in fd_names.iter() {
            let srid_row = pick_i64_from_feature_dataset(backend, *id);
            catalog.entries.push(CatalogEntry {
                name: name.clone(),
                table_name: name.clone(),
                kind: DatasetKind::FeatureDataset,
                parent: None,
                oid_field: String::new(),
                srid: srid_row,
                ..entry_defaults()
            });
        }

        // 对象类：核心清单
        for row in backend.select("GDB_ObjectClasses", &[], &Predicate::All)? {
            let ocid = pick_i64(&row, &["ID", "ObjectClassID", "ObjectClassId"]);
            let name = pick_str(&row, &["Name", "TableName"]);
            let Some(name) = name else { continue };
            let dataset_type = pick_i64(&row, &["DatasetType"]).unwrap_or(0);
            let parent_id = pick_i64(&row, &["DatasetID", "ParentID", "ContainerID", "OwnerID"]);
            let parent = parent_id.and_then(|id| fd_names.get(&id).cloned());

            // GDB_GeomColumns 中出现表名 => 必然是要素类（这是最可靠的判据）
            let is_spatial = geom_cols.contains_key(&name.to_lowercase());
            let in_fc_table = ocid.and_then(|id| fc_map.get(&id)).is_some();
            // 三个判据任一成立即为要素类：几何列登记、要素类登记表、DatasetType 标志位
            let is_fc = is_spatial || in_fc_table || is_feature_class_flag(dataset_type);
            let kind = if is_fc {
                DatasetKind::FeatureClass
            } else {
                DatasetKind::Table
            };

            let info = ocid.and_then(|id| fc_map.get(&id));
            let mut entry = CatalogEntry {
                name: name.clone(),
                table_name: name.clone(),
                kind,
                parent,
                shape_field: info.and_then(|i| i.shape_field.clone()),
                shape_type: info
                    .and_then(|i| i.geometry_type)
                    .and_then(|v| GeometryType::from_shape_type(v as i32).ok()),
                feature_type: FeatureType::from_i32(
                    info.map(|i| i.feature_type).unwrap_or(1) as i32
                ),
                oid_field: oid_field_of(backend, &name),
                object_class_id: ocid,
                ..entry_defaults()
            };
            apply_geom_column(&mut entry, &geom_cols);
            catalog.entries.push(entry);
        }
        catalog.load_field_aliases(backend)?;
        Ok(catalog)
    }

    fn load_spatial_refs(&mut self, backend: &dyn SqlBackend) -> Result<()> {
        if !backend.table_exists("GDB_SpatialRefs")? {
            return Ok(());
        }
        for row in backend.select("GDB_SpatialRefs", &[], &Predicate::All)? {
            let srid = pick_i64(&row, &["SRID", "ID", "Id"]);
            let Some(srid) = srid else { continue };
            let sr = SpatialReference {
                srid: Some(srid),
                false_x: pick_f64(&row, &["FalseX", "FalseOriginX"]).unwrap_or(0.0),
                false_y: pick_f64(&row, &["FalseY", "FalseOriginY"]).unwrap_or(0.0),
                xy_units: pick_f64(&row, &["XYUnits", "XYScale", "XYUnit"]).unwrap_or(0.0),
                false_z: pick_f64(&row, &["FalseZ", "FalseOriginZ"]).unwrap_or(0.0),
                z_units: pick_f64(&row, &["ZUnits", "ZScale"]).unwrap_or(0.0),
                false_m: pick_f64(&row, &["FalseM", "FalseOriginM"]).unwrap_or(0.0),
                m_units: pick_f64(&row, &["MUnits", "MScale"]).unwrap_or(0.0),
                xy_tolerance: pick_f64(&row, &["XYClusterTol", "XYTolerance", "XYClusterTolerance"])
                    .unwrap_or(0.0),
                wkt: pick_str(&row, &["SRTEXT", "SRText", "WKT"])
                    .or_else(|| {
                        pick_bytes(&row, &["SRTEXT", "SRText"])
                            .and_then(|b| String::from_utf8(b).ok())
                    }),
            };
            self.spatial_refs.insert(srid, sr);
        }
        Ok(())
    }

    fn load_field_aliases(&mut self, backend: &dyn SqlBackend) -> Result<()> {
        if !backend.table_exists("GDB_FieldInfo")? {
            return Ok(());
        }
        for row in backend.select("GDB_FieldInfo", &[], &Predicate::All)? {
            let table = pick_str(&row, &["TableName", "ObjectClassName", "ObjectClass"]);
            let field = pick_str(&row, &["FieldName", "Field"]);
            let alias = pick_str(&row, &["AliasName", "Alias"]);
            match (table, field, alias) {
                (Some(t), Some(f), Some(a)) if !a.is_empty() => {
                    self.field_aliases
                        .entry(t.to_lowercase())
                        .or_default()
                        .insert(f.to_lowercase(), a);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 全部已登记的条目
    pub fn entries(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries.iter()
    }

    /// 全部要素数据集
    pub fn feature_datasets(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries.iter().filter(|e| e.kind == DatasetKind::FeatureDataset)
    }

    /// 顶层（非要素数据集中）的表与要素类，对应 `IWorkspace::get_Datasets` 的顶层结果
    pub fn top_level(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries
            .iter()
            .filter(|e| e.parent.is_none() && e.kind != DatasetKind::FeatureDataset)
    }

    /// 指定要素数据集中的所有要素类，对应 `IFeatureDataset::Subsets`
    pub fn children_of(&self, dataset: &str) -> Vec<&CatalogEntry> {
        self.entries
            .iter()
            .filter(|e| e.parent.as_deref() == Some(dataset))
            .collect()
    }

    /// 独立表（`IFeatureWorkspace::OpenTable` 的目标）
    pub fn standalone_tables(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries
            .iter()
            .filter(|e| e.kind == DatasetKind::Table && e.parent.is_none())
    }

    /// 独立要素类
    pub fn standalone_feature_classes(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries
            .iter()
            .filter(|e| e.kind == DatasetKind::FeatureClass && e.parent.is_none())
    }

    /// 所有要素类（含要素数据集内部）
    pub fn all_feature_classes(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries
            .iter()
            .filter(|e| e.kind == DatasetKind::FeatureClass)
    }

    /// 按名称查找（支持 "数据集\要素类" 形式）
    pub fn find(&self, name: &str) -> Option<&CatalogEntry> {
        let name = name.trim();
        let key = name.to_lowercase();
        // 精确：限定名
        if let Some(e) = self.entries.iter().find(|e| e.qualified_name().to_lowercase() == key) {
            return Some(e);
        }
        if let Some(e) = self.entries.iter().find(|e| e.name.to_lowercase() == key) {
            return Some(e);
        }
        self.entries
            .iter()
            .find(|e| e.table_name.to_lowercase() == key)
    }

    /// 按 SRID 取空间参考
    pub fn spatial_ref(&self, srid: i64) -> Option<&SpatialReference> {
        self.spatial_refs.get(&srid)
    }

    /// 某表的字段别名（物理列名 -> 别名）
    pub fn aliases_for(&self, table: &str) -> Option<&HashMap<String, String>> {
        self.field_aliases.get(&table.to_lowercase())
    }
}

/// 读取某个要素数据集登记的 SRID（兼容列名差异）
fn pick_i64_from_feature_dataset(backend: &dyn SqlBackend, dataset_id: i64) -> Option<i64> {
    if !backend.table_exists("GDB_FeatureDataset").ok()? {
        return None;
    }
    for row in backend
        .select("GDB_FeatureDataset", &[], &Predicate::All)
        .ok()?
    {
        if pick_i64(&row, &["ID", "ObjectClassID", "DatasetID"]) == Some(dataset_id) {
            return pick_i64(&row, &["SRID", "SpatialRefID"]);
        }
    }
    None
}

/// 默认 Z/M 等字段为 false 的默认值
fn entry_defaults() -> CatalogEntry {
    CatalogEntry {
        name: String::new(),
        table_name: String::new(),
        kind: DatasetKind::Other,
        parent: None,
        shape_field: None,
        shape_type: None,
        feature_type: FeatureType::Simple,
        srid: None,
        oid_field: "OBJECTID".to_string(),
        extent: None,
        grid: None,
        object_class_id: None,
        uuid: None,
        definition: None,
    }
}

/// 判断 DatasetType 是否代表要素类。
///
/// 不同版本的 `GDB_ObjectClasses.DatasetType` 取值语义略有差异，因此这里只需
/// 作为"可能是要素类"的弱判据，真正可靠的判据是 GDB_GeomColumns 是否登记该表。
fn is_feature_class_flag(dataset_type: i64) -> bool {
    matches!(dataset_type, 3..=5)
}

/// GDB_GeomColumns 一行解析后的结构
#[derive(Debug, Clone)]
struct GeomColumnRow {
    field_name: Option<String>,
    shape_type: Option<i64>,
    extent: Option<Envelope>,
    grid: Option<GridInfo>,
    srid: Option<i64>,
}

fn load_geom_columns(backend: &dyn SqlBackend) -> Result<HashMap<String, GeomColumnRow>> {
    let mut map = HashMap::new();
    if !backend.table_exists("GDB_GeomColumns")? {
        return Ok(map);
    }
    for row in backend.select("GDB_GeomColumns", &[], &Predicate::All)? {
        let table = pick_str(&row, &["TableName", "ObjectClassName", "Name"]);
        let Some(table) = table else { continue };
        let extent = match (
            pick_f64(&row, &["ExtentLeft"]),
            pick_f64(&row, &["ExtentBottom"]),
            pick_f64(&row, &["ExtentRight"]),
            pick_f64(&row, &["ExtentTop"]),
        ) {
            (Some(l), Some(b), Some(r), Some(t)) => Some(Envelope::new(l, b, r, t)),
            _ => None,
        };
        let grid = pick_f64(&row, &["IdxGridSize", "IdxGridSize1"]).map(|size| GridInfo {
            origin_x: pick_f64(&row, &["IdxOriginX"]).unwrap_or(0.0),
            origin_y: pick_f64(&row, &["IdxOriginY"]).unwrap_or(0.0),
            grid_size: size,
        });
        map.insert(
            table.to_lowercase(),
            GeomColumnRow {
                field_name: pick_str(&row, &["FieldName", "ShapeFieldName"]),
                shape_type: pick_i64(&row, &["ShapeType", "GeometryType"]),
                extent,
                grid,
                srid: pick_i64(&row, &["SRID", "SpatialRefID"]),
            },
        );
    }
    Ok(map)
}

fn apply_geom_column(entry: &mut CatalogEntry, map: &HashMap<String, GeomColumnRow>) {
    let Some(g) = map.get(&entry.table_name.to_lowercase()) else {
        return;
    };
    if entry.shape_field.is_none() {
        entry.shape_field = g.field_name.clone();
    }
    if entry.shape_type.is_none() {
        if let Some(st) = g.shape_type {
            entry.shape_type = GeometryType::from_shape_type(st as i32).ok();
        }
    }
    if entry.kind != DatasetKind::Table {
        entry.extent = entry.extent.or(g.extent);
        entry.grid = entry.grid.or(g.grid);
        if entry.srid.is_none() {
            entry.srid = g.srid;
        }
    }
}

/// 推断表的 OBJECTID 字段：优先自增列，其次常见命名，最后第一个整型列
fn oid_field_of(backend: &dyn SqlBackend, table: &str) -> String {
    if table.is_empty() {
        return String::new();
    }
    let cols = match backend.columns(table) {
        Ok(c) => c,
        Err(_) => return "OBJECTID".to_string(),
    };
    if let Some(c) = cols.iter().find(|c| c.is_auto) {
        return c.name.clone();
    }
    for candidate in ["OBJECTID", "ObjectID", "OID", "RowID", "FID", "PKID"] {
        if let Some(c) = cols.iter().find(|c| c.name.eq_ignore_ascii_case(candidate)) {
            if matches!(c.kind, FieldType::Integer | FieldType::Oid | FieldType::SmallInteger) {
                return c.name.clone();
            }
        }
    }
    if let Some(first) = cols.first() {
        return first.name.clone();
    }
    "OBJECTID".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datastore::mirror::MirrorBackend;
    use crate::datastore::ColumnDef;

    fn col(name: &str, sql: &str, is_auto: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            sql_type: sql.into(),
            size: None,
            scale: None,
            nullable: true,
            is_auto,
            kind: FieldType::from_sql_type(sql),
        }
    }

    #[test]
    fn test_pick_candidates() {
        let row = DataTableRow::new(
            vec!["TableName".into(), "ShapeType".into()],
            vec![SqlValue::Text("Roads".into()), SqlValue::I32(3)],
        )
        .unwrap();
        assert_eq!(pick_str(&row, &["TABLENAME"]), Some("Roads".into()));
        assert_eq!(pick_i64(&row, &["ShapeType", "GeometryType"]), Some(3));
    }

    #[test]
    fn test_oid_detection() {
        let b = MirrorBackend::new();
        b.create_table("Roads", vec![col("OBJECTID", "LONG", true), col("NAME", "TEXT", false)])
            .unwrap();
        assert_eq!(oid_field_of(&b, "Roads"), "OBJECTID");
    }
}
