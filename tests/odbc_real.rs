//! 真实 `.mdb` 文件的 ODBC 集成测试（对应 ArcObjects 语义在真实数据库上的验收）。
//!
//! 默认**跳过**——CI / 无驱动环境下不会失败。启用方式：
//!
//! 1. 准备真实 mdb：用本仓库 `tools/make_mdb/MakeMdb.java` 生成样本，
//!    或直接使用 ArcGIS 导出的个人地理数据库；
//! 2. 安装 ODBC 驱动：
//!    - Windows：Microsoft Access Database Engine（ACE）或 Jet 4 驱动；
//!    - Linux：`sudo apt install unixodbc odbc-mdbtools`（只读）；
//! 3. 运行：
//!
//! ```bash
//! PGDB_TEST_MDB=/tmp/sample_legacy.mdb \
//! PGDB_TEST_MDB_ITEMS=/tmp/sample_items.mdb \
//!     cargo test --features odbc --test odbc_real -- --ignored --test-threads=1
//! ```
//!
//! 未设置环境变量时各用例自动跳过；写回测试会把 mdb **复制到临时目录**再改，
//! 不会碰原始文件；在只读驱动（mdbtools）下写回用例打印提示后跳过。

#![cfg(feature = "odbc")]

use pgdb::gdb::{
    AccessWorkspaceFactory, DatasetKind, FeatureClass, FeatureDataset, FeatureWorkspace,
    MetadataModel, QueryFilter, Workspace,
};
use pgdb::geom::{AsWkt, Geometry};
use pgdb::value::Value;

/// 环境变量指定的 legacy 模型 mdb
fn legacy_mdb() -> Option<String> {
    std::env::var("PGDB_TEST_MDB")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// 环境变量指定的 items 模型 mdb（可选）
fn items_mdb() -> Option<String> {
    std::env::var("PGDB_TEST_MDB_ITEMS")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

macro_rules! skip_unless_env {
    ($env_fn:ident, $out:ident) => {
        let Some($out) = $env_fn() else {
            println!(
                "跳过：未设置环境变量（PGDB_TEST_MDB / PGDB_TEST_MDB_ITEMS），真实 mdb 的 ODBC 测试"
            );
            return;
        };
    };
}

// ------------------------------------------------------------------ 发现与遍历

#[test]
#[ignore = "需要真实 mdb：设置 PGDB_TEST_MDB 后以 --ignored 运行"]
fn odbc_legacy_workspace_discovery() {
    skip_unless_env!(legacy_mdb, mdb);
    let ws = AccessWorkspaceFactory::open_odbc(&mdb, None)
        .unwrap_or_else(|e| panic!("打开 {mdb} 失败: {e}"));

    assert_eq!(ws.metadata_model(), MetadataModel::Legacy);
    assert!(
        ws.backend().capabilities().raw_sql,
        "ODBC 后端必须支持 raw SQL"
    );

    // ArcObjects 语义：顶层枚举 = 独立要素类 + 独立表 + 要素数据集
    let mut fc_names = Vec::new();
    let mut table_names = Vec::new();
    let mut fd_names = Vec::new();
    let mut en = ws.datasets().expect("枚举数据集失败");
    while let Some(handle) = en.next_dataset() {
        match handle.kind() {
            DatasetKind::FeatureClass => fc_names.push(handle.qualified_name()),
            DatasetKind::Table => table_names.push(handle.qualified_name()),
            DatasetKind::FeatureDataset => fd_names.push(handle.qualified_name()),
            DatasetKind::Other => {}
        }
    }
    fc_names.sort();
    table_names.sort();
    assert_eq!(fd_names, vec!["Hydrology".to_string()]);
    assert!(
        table_names.contains(&"OwnerTable".to_string()),
        "独立表 OwnerTable 应在顶层枚举中：{table_names:?}"
    );
    for expect in ["Roads", "Sensors", "Meters"] {
        assert!(
            fc_names.iter().any(|n| n == expect),
            "独立要素类 {expect} 应在顶层枚举中：{fc_names:?}"
        );
    }

    // 要素数据集内的要素类：IFeatureDataset::Subsets
    let fd = ws.open_feature_dataset("Hydrology").expect("打开要素数据集");
    let subsets: Vec<String> = fd.subsets().iter().map(|d| d.qualified_name()).collect();
    assert!(
        subsets.iter().any(|n| n.ends_with("Ponds")),
        "Hydrology 内应有 Ponds：{subsets:?}"
    );
    assert_eq!(fd.feature_class_count(), 1);
}

#[test]
#[ignore = "需要真实 mdb：设置 PGDB_TEST_MDB_ITEMS 后以 --ignored 运行"]
fn odbc_items_workspace_discovery() {
    skip_unless_env!(items_mdb, mdb);
    let ws = AccessWorkspaceFactory::open_odbc(&mdb, None)
        .unwrap_or_else(|e| panic!("打开 {mdb} 失败: {e}"));

    assert_eq!(ws.metadata_model(), MetadataModel::Items);
    let mut fc = 0usize;
    let mut in_fd = 0usize;
    let mut tables = 0usize;
    let mut fds = 0usize;
    for e in ws.catalog().entries() {
        match e.kind {
            DatasetKind::FeatureClass => {
                fc += 1;
                if e.parent.is_some() {
                    in_fd += 1;
                }
            }
            DatasetKind::Table => tables += 1,
            DatasetKind::FeatureDataset => fds += 1,
            DatasetKind::Other => {}
        }
    }
    assert_eq!((fc, in_fd, tables, fds), (4, 1, 1, 1), "Items 模型目录发现不符");
}

// ------------------------------------------------------------------ 行与几何

#[test]
#[ignore = "需要真实 mdb：设置 PGDB_TEST_MDB 后以 --ignored 运行"]
fn odbc_row_counts_and_geometry() {
    skip_unless_env!(legacy_mdb, mdb);
    let ws = AccessWorkspaceFactory::open_odbc(&mdb, None).unwrap();

    // 行数（ITable::RowCount）
    let roads = ws.open_feature_class("Roads").expect("打开 Roads");
    assert_eq!(roads.table().row_count(&QueryFilter::new()).unwrap(), 2);
    let sensors = ws.open_feature_class("Sensors").unwrap();
    assert_eq!(sensors.table().row_count(&QueryFilter::new()).unwrap(), 3);

    // 几何解码（IFeature::Shape -> WKT）。样本坐标来自 MakeMdb.java，固定可断言。
    let features = roads
        .search_features(QueryFilter::new())
        .unwrap()
        .collect_features()
        .unwrap();
    let mut wkts: Vec<String> = features
        .iter()
        .map(|f| {
            f.geometry()
                .unwrap()
                .expect("Roads 要素必须有几何")
                .as_wkt()
        })
        .collect();
    wkts.sort();
    assert_eq!(
        wkts,
        vec![
            "LINESTRING(0 0, 10 0, 10 10)".to_string(),
            "LINESTRING(20 5, 30 5)".to_string(),
        ],
        "Roads 的 WKT 与样本不符"
    );

    // 点要素（含 Z）
    let sensors_feats = sensors
        .search_features(QueryFilter::new())
        .unwrap()
        .collect_features()
        .unwrap();
    let mut swkt: Vec<String> = sensors_feats
        .iter()
        .map(|f| f.geometry().unwrap().unwrap().as_wkt())
        .collect();
    swkt.sort();
    assert!(
        swkt.contains(&"POINTZ(1 1 5)".to_string()),
        "Sensors 应包含带 Z 的点：{swkt:?}"
    );

    // 属性读取（中文）
    let f = roads.get_feature(1).unwrap().expect("OBJECTID=1 应存在");
    assert_eq!(f.value_by_name("NAME").unwrap().as_string().unwrap(), "人民路");

    // 字段别名（GDB_FieldInfo）。Fields::find 返回索引，再按索引取字段。
    let idx = roads.table().fields().find("NAME").expect("NAME 字段");
    let name_field = roads
        .table()
        .fields()
        .field(idx)
        .expect("NAME 字段存在");
    assert_eq!(name_field.alias.as_deref(), Some("道路名称"));
}

#[test]
#[ignore = "需要真实 mdb：设置 PGDB_TEST_MDB 后以 --ignored 运行"]
fn odbc_write_roundtrip_on_temp_copy() {
    skip_unless_env!(legacy_mdb, mdb);

    // 复制到临时目录，避免污染原始样本
    let tmp = tempfile::tempdir().expect("创建临时目录");
    let copy_path = tmp.path().join("copy.mdb");
    std::fs::copy(&mdb, &copy_path).expect("复制 mdb");
    let copy = copy_path.to_string_lossy().to_string();

    let ws = AccessWorkspaceFactory::open_odbc(&copy, None).unwrap();
    if !ws.backend().capabilities().writable {
        println!(
            "跳过写回测试：当前 ODBC 驱动只读（Linux mdbtools）。\
             写回验证请在 Windows + Access Database Engine 下执行。"
        );
        return;
    }

    // 1) 属性更新（ITable::Update + IRow::Store）
    let roads = ws.open_feature_class("Roads").unwrap();
    let mut feat = roads.get_feature(2).unwrap().expect("OBJECTID=2");
    feat.set_value_by_name("NAME", Value::String("更新路".into()))
        .unwrap();
    feat.store().unwrap();

    let reread = ws.open_feature_class("Roads").unwrap();
    let f = reread.get_feature(2).unwrap().unwrap();
    assert_eq!(
        f.value_by_name("NAME").unwrap().as_string().unwrap(),
        "更新路"
    );

    // 2) 几何更新（IFeature::Store），并验证 Shape_Length 自动重算
    let mut g = f.geometry().unwrap().unwrap();
    let Geometry::Polyline(paths) = &mut g else {
        panic!("Roads 应为 Polyline，实际 {g:?}");
    };
    for p in paths.iter_mut() {
        for v in p.iter_mut() {
            v.x += 100.0;
        }
    }
    let mut feat = reread.get_feature(2).unwrap().unwrap();
    feat.set_geometry(&g).unwrap();
    feat.store().unwrap();

    let verify = ws.open_feature_class("Roads").unwrap();
    let f2 = verify.get_feature(2).unwrap().unwrap();
    let geom = f2.geometry().unwrap().unwrap();
    assert_eq!(geom.as_wkt(), g.as_wkt(), "几何写回后应一致");
    let len = f2
        .value_by_name("Shape_Length")
        .unwrap()
        .as_double()
        .expect("Shape_Length 应为数值");
    assert!((len - 10.0).abs() < 1e-9, "平移不改变长度（应为 10），实际 {len}");

    // 3) 空间索引表同步（<表>_SHAPE_Index）
    let idx_rows = verify
        .table()
        .backend()
        .select(
            "Roads_SHAPE_Index",
            &[],
            &pgdb::datastore::Predicate::eq("IndexedObjectId", 2i32),
        )
        .unwrap();
    assert_eq!(
        idx_rows.len(),
        1,
        "空间索引应恰好有一条 OBJECTID=2 的网格记录"
    );
}

#[test]
#[ignore = "需要真实 mdb + ogrinfo：设置 PGDB_TEST_MDB 后以 --ignored 运行"]
fn odbc_gdal_cross_validation() {
    skip_unless_env!(legacy_mdb, mdb);

    // ogrinfo（GDAL）可用时，验证 GDAL 读取的要素数与本库一致
    let Ok(out) = std::process::Command::new("ogrinfo")
        .arg("-so")
        .arg("-al")
        .arg(&mdb)
        .output()
    else {
        println!("跳过：未安装 ogrinfo（apt install gdal-bin）");
        return;
    };
    if !out.status.success() {
        println!("跳过：ogrinfo 无法读取该文件（PGeo 驱动未装？）");
        return;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // "Feature Count" 出现的次数应 >= 4（Roads/Ponds/Sensors/Meters）
    let count = text
        .lines()
        .filter(|l| l.trim_start().starts_with("Feature Count:"))
        .count();
    assert!(count >= 4, "GDAL 应读到至少 4 个几何图层，实际 {count}：\n{text}");
    assert!(
        text.contains("Line String"),
        "GDAL 应把 Roads 识别为线"
    );
    assert!(text.contains("Polygon"), "GDAL 应把 Ponds 识别为面");
}
