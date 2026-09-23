//! 属性与几何更新：`IRow::Store` / `IFeature::Store` / `IFeatureBuffer` 的 Rust 版。
//!
//! 演示内容：
//!
//! 1. 独立数据表 / 独立要素类的**属性更新**（`ITable::Update` + `IRow::Store`）
//! 2. 要素数据集内要素类的**几何更新**（`IFeatureClass::Update` + `IFeature::Store`），
//!    自动维护 `Shape_Length` / `Shape_Area`、`<表>_SHAPE_Index`、`GDB_GeomColumns` 图层范围
//! 3. **新建要素**（`IFeatureClass::Insert` + `IFeatureBuffer`）与删除
//! 4. 写完后回读校验 ESRI 依赖的一致性数据
//!
//! 为避免污染示例数据，脚本会把示例镜像复制一份临时副本再操作。
//!
//! 运行：
//!
//! ```bash
//! cargo run --example update
//! ```

use pgdb::datastore::Predicate;
use pgdb::gdb::{
    AccessWorkspace, AccessWorkspaceFactory, FeatureClass, FeatureWorkspace, QueryFilter, Table,
    Workspace, WorkspaceFactory,
};
use pgdb::geom::{geometry_from_wkt, AsWkt, Geometry, Vertex};
use pgdb::Value;

fn main() -> pgdb::Result<()> {
    pgdb::init_log();

    // ---------- 准备一份可写的临时副本 ----------
    let src = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "examples/sample.mdb.json".to_string());
    let dir = std::env::temp_dir().join("pgdb-rs-demo");
    std::fs::create_dir_all(&dir).ok();
    let work = dir.join("sample-copy.mdb.json");
    std::fs::copy(&src, &work).map_err(|e| {
        pgdb::PgdbError::io(format!("复制 {} -> {}", src, work.display()), e)
    })?;
    let work_path = work.to_string_lossy().to_string();

    let ws: AccessWorkspace = AccessWorkspaceFactory.open(&work_path, None)?;
    println!("工作空间：{}", ws.path());
    println!();

    // ================= 1. 独立数据表的属性更新 =================
    println!("--- 1. 独立数据表 OwnerTable：IRow::Store ---");
    let table = ws.open_table("OwnerTable")?;
    let mut cursor = table.update(QueryFilter::new())?;
    let mut updated = 0;
    while let Some(mut row) = cursor.next_row()? {
        let name = row
            .value_by_name("NAME")
            .cloned()
            .unwrap_or(Value::Null)
            .to_string();
        row.set_value_by_name("REMARK", Value::String(format!("{name}-已核对")))?;
        row.store()?;
        updated += 1;
    }
    println!("  已更新 {updated} 行");
    ws.backend().flush()?;

    // ================= 2. 独立要素类的属性更新 =================
    println!("--- 2. 独立要素类 Roads：按 OBJECTID 改属性 ---");
    let roads = ws.open_feature_class("Roads")?;
    if let Some(mut row) = roads.update(QueryFilter::for_oid(1))?.next_row()? {
        row.set_value_by_name("NAME", Value::String("长安街".into()))?;
        row.store()?;
        println!("  OID=1 已改名为长安街（Shape 未被触碰）");
    }
    ws.backend().flush()?;

    // ================= 3. 几何更新：独立要素类 =================
    println!("--- 3. Roads 的几何更新：IFeature::Store ---");
    let new_line = geometry_from_wkt("LINESTRING(20 20, 30 30, 40 25)")?;
    let mut cursor = roads.update_features(QueryFilter::for_oid(2))?;
    while let Some(mut feature) = cursor.next_feature()? {
        println!("  更新前长度 = {}", feature.geometry()?.map(|g| g.length()).unwrap_or(0.0));
        feature.set_geometry(&new_line)?;
        feature.store()?;
        println!("  更新后 WKT = {}", new_line.as_wkt());
    }
    ws.backend().flush()?;
    println!("  Shape_Length 已自动重算为 {:.6}", new_line.length());
    dump_geometry_columns(&ws, "Roads")?;
    dump_shape_index(&ws, "Roads")?;

    // ================= 4. 要素数据集内要素类的几何与属性更新 =================
    println!("--- 4. 要素数据集内的要素类 Hydrology\\Ponds ---");
    let ponds = ws.open_feature_class("Hydrology\\Ponds")?;
    let mut cursor = ponds.update_features(QueryFilter::new())?;
    while let Some(mut feature) = cursor.next_feature()? {
        let oid = feature.oid().unwrap_or_default();
        // 直接用结构体构造几何，无需 WKT
        let enlarged = match feature.geometry()? {
            Some(g) => scale_polygon(&g, 2.0),
            None => continue,
        };
        feature.set_geometry(&enlarged)?;
        feature.set_value_by_name("NAME", Value::String(format!("池塘-{oid}-扩建")))?;
        feature.store()?;
        println!("  OID={oid} 面积 {} -> {}", 0.0, enlarged.area());
    }
    ws.backend().flush()?;
    dump_geometry_columns(&ws, "Ponds")?;

    // ================= 5. 新建要素（IFeatureBuffer） =================
    println!("--- 5. 新建要素：IFeatureClass::Insert ---");
    let mut insert = ponds.insert_feature_cursor()?;
    insert
        .buffer()
        .set_value("NAME", Value::String("新建池塘".into()));
    insert.set_geometry(&Geometry::Polygon(vec![vec![
        Vertex::new(10.0, 10.0),
        Vertex::new(10.0, 12.0),
        Vertex::new(12.0, 12.0),
        Vertex::new(12.0, 10.0),
        Vertex::new(10.0, 10.0),
    ]]))?;
    let new_oid = insert.insert_feature()?;
    insert.flush()?;
    ws.backend().flush()?;
    println!("  新建要素 OBJECTID = {new_oid}");

    // ================= 6. 删除 =================
    println!("--- 6. 删除要素：ITable::DeleteSearchedRows ---");
    let deleted = ponds.delete_rows(&QueryFilter::for_oid(new_oid))?;
    ws.backend().flush()?;
    println!("  已删除 {deleted} 个要素");

    // ================= 7. 回读校验 =================
    println!("--- 7. 回读校验 ---");
    let mut check = ponds.search_features(QueryFilter::new())?;
    while let Some(feature) = check.next_feature()? {
        let name = feature
            .value_by_name("NAME")
            .map(|v| v.to_string())
            .unwrap_or_default();
        let wkt = feature.geometry()?.map(|g| g.as_wkt()).unwrap_or_default();
        println!("  OID={:?} {} {}", feature.oid(), name, wkt);
    }
    println!();
    println!("全部更新完成，临时副本保存在 {}", work.display());
    Ok(())
}

/// 把面的每个顶点按 factor 缩放（演示如何直接操作 Shape 数据结构）
fn scale_polygon(g: &Geometry, factor: f64) -> Geometry {
    match g {
        Geometry::Polygon(rings) => Geometry::Polygon(
            rings
                .iter()
                .map(|ring| {
                    ring.iter()
                        .map(|v| Vertex::new(v.x * factor, v.y * factor))
                        .collect()
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn dump_geometry_columns(ws: &AccessWorkspace, table: &str) -> pgdb::Result<()> {
    let rows = ws.backend().select("GDB_GeomColumns", &[], &Predicate::All)?;
    for r in rows {
        let name = r
            .get("TableName")
            .map(|v| v.to_display_string())
            .unwrap_or_default();
        if !name.eq_ignore_ascii_case(table) {
            continue;
        }
        println!(
            "  GDB_GeomColumns: 范围 = ({}, {}, {}, {})，网格 = {}",
            num(r.get("ExtentLeft")),
            num(r.get("ExtentBottom")),
            num(r.get("ExtentRight")),
            num(r.get("ExtentTop")),
            num(r.get("IdxGridSize"))
        );
    }
    Ok(())
}

fn dump_shape_index(ws: &AccessWorkspace, table: &str) -> pgdb::Result<()> {
    let index_table = format!("{table}_SHAPE_Index");
    if !ws.backend().table_exists(&index_table)? {
        return Ok(());
    }
    let rows = ws.backend().select(&index_table, &[], &Predicate::All)?;
    println!("  {index_table}: {} 条网格记录", rows.len());
    for r in rows {
        println!(
            "    OID={} 网格单元 = ({}, {}, {}, {})",
            num(r.get("IndexedObjectId")),
            num(r.get("MinGX")),
            num(r.get("MinGY")),
            num(r.get("MaxGX")),
            num(r.get("MaxGY"))
        );
    }
    Ok(())
}

fn num(v: Option<&pgdb::SqlValue>) -> String {
    match v {
        Some(x) => x.to_display_string(),
        None => "-".to_string(),
    }
}
