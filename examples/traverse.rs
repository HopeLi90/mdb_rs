//! 遍历工作空间：独立要素类 / 独立数据表 / **要素数据集内部的要素类**。
//!
//! 对应关系：
//!
//! ```text
//! AccessWorkspaceFactory  ->  IWorkspaceFactory
//! AccessWorkspace         ->  IWorkspace / IFeatureWorkspace
//! ws.datasets()           ->  IWorkspace::get_Datasets
//! DatasetEnum::next_dataset -> IEnumDataset::Next
//! fd.subsets()            ->  IFeatureDataset::Subsets
//! ```
//!
//! 运行（需要 `odbc` feature 与驱动）：
//!
//! ```bash
//! cargo run --example traverse -- 你的库.mdb
//! ```

use pgdb::gdb::{
    AccessWorkspace, AccessWorkspaceFactory, DatasetHandle, DatasetNode, FeatureClass,
    FeatureWorkspace, QueryFilter, Table, Workspace, WorkspaceFactory,
};
use pgdb::geom::AsWkt;

fn main() -> pgdb::Result<()> {
    pgdb::init_log();

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "你的库.mdb".to_string());
    let ws: AccessWorkspace = AccessWorkspaceFactory.open(&path, None)?;

    println!("=== 工作空间：{} ===", ws.path());
    println!("=== 元数据模型：{:?} ===", ws.metadata_model());
    println!();

    // ---------- 1. 顶层枚举（IWorkspace::get_Datasets） ----------
    let mut enumeration = ws.datasets()?;
    while let Some(handle) = enumeration.next_dataset() {
        print_dataset(&handle)?;

        // ---------- 2. 要素数据集内部下钻（IFeatureDataset::Subsets） ----------
        if let Some(fd) = handle.as_feature_dataset() {
            for child in fd.subsets() {
                print_dataset(child)?;
            }
        }
        println!();
    }

    // ---------- 3. 按限定名直接打开数据集内的要素类 ----------
    println!("=== 直接按限定名打开 `Hydrology\\Ponds` ===");
    let fc = ws.open_feature_class("Hydrology\\Ponds")?;
    println!(
        "{} | 几何字段 {} | {} | SRID {:?}",
        fc.qualified_name(),
        fc.shape_field_name(),
        fc.shape_type().label(),
        fc.spatial_reference().and_then(|sr| sr.srid)
    );
    let mut cursor = fc.search_features(QueryFilter::new())?;
    while let Some(feature) = cursor.next_feature()? {
        let name = feature
            .value_by_name("NAME")
            .map(|v| v.to_string())
            .unwrap_or_default();
        let wkt = feature.geometry()?.map(|g| g.as_wkt()).unwrap_or_default();
        println!("  OID={:?} NAME={} {}", feature.oid(), name, wkt);
    }
    Ok(())
}

fn print_dataset(handle: &DatasetHandle) -> pgdb::Result<()> {
    let rows = match handle.as_table() {
        Some(t) => format!("，{} 行", Table::row_count(t, &QueryFilter::new())?),
        None => String::new(),
    };
    println!("{}（{}）{}", handle.qualified_name(), handle.kind().label(), rows);

    if let Some(fc) = handle.as_feature_class() {
        println!(
            "    几何类型 {}，要素类型 {}，Shape 字段 {}",
            fc.shape_type().label(),
            fc.feature_type().label(),
            fc.shape_field_name()
        );
        if let Some(sr) = fc.spatial_reference() {
            println!("    空间参考 SRID={:?}", sr.srid);
        }
    }

    // 表/要素类的字段清单（ITable::Fields）
    if let Some(t) = handle.as_table() {
        let names: Vec<String> = t
            .fields()
            .iter()
            .map(|f| match f.alias.as_deref() {
                Some(a) if !a.eq_ignore_ascii_case(&f.name) => format!("{}({})", f.name, a),
                _ => f.name.clone(),
            })
            .collect();
        println!("    字段：{}", names.join(", "));
    }
    Ok(())
}
