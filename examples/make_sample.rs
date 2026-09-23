//! 生成一份可以直接被本库（以及 `pgdb-cli`）打开的 **示例个人地理数据库镜像**。
//!
//! 演示内容对应 ArcMap 目录树里的三种典型对象：
//!
//! ```text
//! sample.mdb
//! ├── Roads            独立要素类（线）
//! ├── OwnerTable       独立数据表
//! └── Hydrology        要素数据集
//!     └── Ponds        要素数据集内的要素类（面）
//! ```
//!
//! 运行：
//!
//! ```bash
//! cargo run --example make_sample -- examples/sample.mdb.json
//! ```

use std::path::Path;

use pgdb::datastore::mirror::MirrorBackend;
use pgdb::datastore::{ColumnDef, SqlBackend, SqlValue};
use pgdb::field::FieldType;
use pgdb::geom::codec::encode_shape;
use pgdb::geom::{Geometry, Vertex};
use pgdb::Result;

fn col(name: &str, sql: &str, auto: bool) -> ColumnDef {
    let kind = FieldType::from_sql_type(sql);
    ColumnDef {
        name: name.to_string(),
        sql_type: sql.to_string(),
        size: if kind == FieldType::String { Some(255) } else { None },
        scale: None,
        nullable: !auto,
        is_auto: auto,
        kind,
    }
}

fn line(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<u8> {
    encode_shape(&Geometry::Polyline(vec![vec![
        Vertex::new(x0, y0),
        Vertex::new(x1, y1),
    ]]))
    .unwrap()
}

fn box_ring(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<u8> {
    // 注意：这里按 ESRI 约定的顺时针外环录入
    encode_shape(&Geometry::Polygon(vec![vec![
        Vertex::new(min_x, min_y),
        Vertex::new(min_x, max_y),
        Vertex::new(max_x, max_y),
        Vertex::new(max_x, min_y),
        Vertex::new(min_x, min_y),
    ]]))
    .unwrap()
}

fn shape_index(name: &str) -> Vec<ColumnDef> {
    let _ = name;
    vec![
        col("IndexedObjectId", "LONG", false),
        col("MinGX", "DOUBLE", false),
        col("MinGY", "DOUBLE", false),
        col("MaxGX", "DOUBLE", false),
        col("MaxGY", "DOUBLE", false),
    ]
}

fn main() -> Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "examples/sample.mdb.json".to_string());
    let db = MirrorBackend::new();

    // ---------- 1. 空间参考（GDB_SpatialRefs） ----------
    db.create_table(
        "GDB_SpatialRefs",
        vec![
            col("ID", "LONG", false),
            col("FalseX", "DOUBLE", false),
            col("FalseY", "DOUBLE", false),
            col("XYUnits", "DOUBLE", false),
            col("SRTEXT", "MEMO", false),
        ],
    )?;
    db.insert(
        "GDB_SpatialRefs",
        &[
            ("ID".into(), SqlValue::I64(1)),
            ("FalseX".into(), SqlValue::F64(-400.0)),
            ("FalseY".into(), SqlValue::F64(-400.0)),
            ("XYUnits".into(), SqlValue::F64(1000000.0)),
            (
                "SRTEXT".into(),
                SqlValue::Text("GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\"]]".into()),
            ),
        ],
    )?;

    // ---------- 2. 要素数据集（GDB_FeatureDataset） ----------
    db.create_table(
        "GDB_FeatureDataset",
        vec![
            col("ID", "LONG", false),
            col("Name", "TEXT", false),
            col("SRID", "LONG", false),
        ],
    )?;
    db.insert(
        "GDB_FeatureDataset",
        &[
            ("ID".into(), SqlValue::I64(1)),
            ("Name".into(), SqlValue::Text("Hydrology".into())),
            ("SRID".into(), SqlValue::I64(1)),
        ],
    )?;

    // ---------- 3. 对象类注册表（GDB_ObjectClasses） ----------
    // DatasetType: 1 = 表；3 = 要素类。DatasetID 非空表示该要素类位于要素数据集内
    db.create_table(
        "GDB_ObjectClasses",
        vec![
            col("ID", "LONG", false),
            col("Name", "TEXT", false),
            col("DatasetType", "LONG", false),
            col("DatasetID", "LONG", false),
        ],
    )?;
    for (id, name, dtype, dataset_id) in [
        (1, "Roads", 3i64, None),
        (2, "OwnerTable", 1, None),
        (3, "Ponds", 3, Some(1i64)),
    ] {
        db.insert(
            "GDB_ObjectClasses",
            &[
                ("ID".into(), SqlValue::I64(id)),
                ("Name".into(), SqlValue::Text(name.into())),
                ("DatasetType".into(), SqlValue::I64(dtype)),
                (
                    "DatasetID".into(),
                    dataset_id.map_or(SqlValue::Null, SqlValue::I64),
                ),
            ],
        )?;
    }

    // ---------- 4. 要素类登记表（GDB_FeatureClasses） ----------
    // GeometryType: 3 = 线（Polyline），4 = 面（Polygon）
    db.create_table(
        "GDB_FeatureClasses",
        vec![
            col("ObjectClassID", "LONG", false),
            col("FeatureType", "LONG", false),
            col("GeometryType", "LONG", false),
            col("ShapeFieldName", "TEXT", false),
        ],
    )?;
    for (ocid, geom_type) in [(1i64, 3i64), (3, 4)] {
        db.insert(
            "GDB_FeatureClasses",
            &[
                ("ObjectClassID".into(), SqlValue::I64(ocid)),
                ("FeatureType".into(), SqlValue::I64(1)),
                ("GeometryType".into(), SqlValue::I64(geom_type)),
                ("ShapeFieldName".into(), SqlValue::Text("Shape".into())),
            ],
        )?;
    }

    // ---------- 5. 几何列 / 空间索引参数（GDB_GeomColumns） ----------
    db.create_table(
        "GDB_GeomColumns",
        vec![
            col("TableName", "TEXT", false),
            col("FieldName", "TEXT", false),
            col("ShapeType", "LONG", false),
            col("ExtentLeft", "DOUBLE", false),
            col("ExtentBottom", "DOUBLE", false),
            col("ExtentRight", "DOUBLE", false),
            col("ExtentTop", "DOUBLE", false),
            col("IdxOriginX", "DOUBLE", false),
            col("IdxOriginY", "DOUBLE", false),
            col("IdxGridSize", "DOUBLE", false),
            col("SRID", "LONG", false),
        ],
    )?;
    for (table, shape_type, left, bottom, right, top) in [
        ("Roads", 3i64, 0.0, 0.0, 10.0, 10.0),
        ("Ponds", 4, 1.0, 1.0, 5.0, 5.0),
    ] {
        db.insert(
            "GDB_GeomColumns",
            &[
                ("TableName".into(), SqlValue::Text(table.into())),
                ("FieldName".into(), SqlValue::Text("Shape".into())),
                ("ShapeType".into(), SqlValue::I64(shape_type)),
                ("ExtentLeft".into(), SqlValue::F64(left)),
                ("ExtentBottom".into(), SqlValue::F64(bottom)),
                ("ExtentRight".into(), SqlValue::F64(right)),
                ("ExtentTop".into(), SqlValue::F64(top)),
                ("IdxOriginX".into(), SqlValue::F64(0.0)),
                ("IdxOriginY".into(), SqlValue::F64(0.0)),
                ("IdxGridSize".into(), SqlValue::F64(420.0)),
                ("SRID".into(), SqlValue::I64(1)),
            ],
        )?;
    }

    // ---------- 6. 字段别名（GDB_FieldInfo） ----------
    db.create_table(
        "GDB_FieldInfo",
        vec![
            col("TableName", "TEXT", false),
            col("FieldName", "TEXT", false),
            col("AliasName", "TEXT", false),
        ],
    )?;
    db.insert(
        "GDB_FieldInfo",
        &[
            ("TableName".into(), SqlValue::Text("Roads".into())),
            ("FieldName".into(), SqlValue::Text("NAME".into())),
            ("AliasName".into(), SqlValue::Text("道路名称".into())),
        ],
    )?;

    // ---------- 7. 业务表：独立要素类 Roads ----------
    db.create_table(
        "Roads",
        vec![
            col("OBJECTID", "LONG", true),
            col("NAME", "TEXT", false),
            col("Shape_Length", "DOUBLE", false),
            col("Shape", "LONGVARBINARY", false),
        ],
    )?;
    for (name, (x0, y0, x1, y1)) in [
        ("G1", (0.0f64, 0.0f64, 10.0f64, 0.0f64)),
        ("G2", (0.0f64, 5.0f64, 5.0f64, 5.0f64)),
    ] {
        let length = (x1 - x0).abs() + (y1 - y0).abs();
        db.insert(
            "Roads",
            &[
                ("NAME".into(), SqlValue::Text(name.into())),
                ("Shape_Length".into(), SqlValue::F64(length)),
                ("Shape".into(), SqlValue::Binary(line(x0, y0, x1, y1))),
            ],
        )?;
    }

    // ---------- 8. 业务表：独立数据表 OwnerTable ----------
    db.create_table(
        "OwnerTable",
        vec![
            col("OBJECTID", "LONG", true),
            col("NAME", "TEXT", false),
            col("REMARK", "MEMO", false),
        ],
    )?;
    for (name, remark) in [("北京市", "首都"), ("天津市", "直辖市")] {
        db.insert(
            "OwnerTable",
            &[
                ("NAME".into(), SqlValue::Text(name.into())),
                ("REMARK".into(), SqlValue::Text(remark.into())),
            ],
        )?;
    }

    // ---------- 9. 业务表：要素数据集内的要素类 Ponds ----------
    db.create_table(
        "Ponds",
        vec![
            col("OBJECTID", "LONG", true),
            col("NAME", "TEXT", false),
            col("Shape", "LONGVARBINARY", false),
        ],
    )?;
    for (name, rect) in [("池塘A", (1.0, 1.0, 3.0, 3.0)), ("池塘B", (3.0, 3.0, 5.0, 5.0))] {
        db.insert(
            "Ponds",
            &[
                ("NAME".into(), SqlValue::Text(name.into())),
                (
                    "Shape".into(),
                    SqlValue::Binary(box_ring(rect.0, rect.1, rect.2, rect.3)),
                ),
            ],
        )?;
    }

    // ---------- 10. ESRI 空间索引表 ----------
    for name in ["Roads_SHAPE_Index", "Ponds_SHAPE_Index"] {
        db.create_table(name, shape_index(name))?;
    }

    if let Some(dir) = Path::new(&out).parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }
    db.save_file(&out)?;
    println!("已生成示例地理数据库镜像: {out}");
    println!("  Roads       独立要素类（线，2 条）");
    println!("  OwnerTable  独立数据表（2 行）");
    println!("  Hydrology   要素数据集");
    println!("    Ponds     数据集内的要素类（面，2 个）");
    Ok(())
}
