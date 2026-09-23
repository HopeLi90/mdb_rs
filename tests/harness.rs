//! 集成测试用的夹具：构造两个"假的个人地理数据库"结构。
//!
//! 一个使用 ArcGIS 8.x/9.0 的旧模型（GDB_ObjectClasses 系列），
//! 另一个使用 9.2+ 的 GDB_Items 模型。

use std::sync::Arc;

use pgdb::datastore::mirror::MirrorBackend;
use pgdb::datastore::{ColumnDef, SqlBackend, SqlValue};
use pgdb::field::FieldType;
use pgdb::geom::codec::encode_shape;
use pgdb::geom::{Geometry, Vertex};

/// 构造列定义
pub fn col(name: &str, sql: &str, is_auto: bool) -> ColumnDef {
    let kind = FieldType::from_sql_type(sql);
    ColumnDef {
        name: name.to_string(),
        sql_type: sql.to_string(),
        size: if kind == FieldType::String { Some(255) } else { None },
        scale: None,
        nullable: !is_auto,
        is_auto,
        kind,
    }
}

/// 两点折线的 Shape 二进制
pub fn line_bytes(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<u8> {
    let g = Geometry::Polyline(vec![vec![Vertex::new(x0, y0), Vertex::new(x1, y1)]]);
    encode_shape(&g).unwrap()
}

/// 矩形的 Shape 二进制（面，顺时针）
pub fn polygon_bytes(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<u8> {
    let g = Geometry::Polygon(vec![vec![
        Vertex::new(min_x, min_y),
        Vertex::new(min_x, max_y),
        Vertex::new(max_x, max_y),
        Vertex::new(max_x, min_y),
        Vertex::new(min_x, min_y),
    ]]);
    encode_shape(&g).unwrap()
}

/// 业务表 + 对应的空间索引表 + 系统表（旧模型）
pub fn legacy_database() -> MirrorBackend {
    let db = MirrorBackend::new();

    // ---------------- 系统表 ----------------
    db.create_table(
        "GDB_SpatialRefs",
        vec![
            col("ID", "LONG", false),
            col("FalseX", "DOUBLE", false),
            col("FalseY", "DOUBLE", false),
            col("XYUnits", "DOUBLE", false),
            col("SRTEXT", "MEMO", false),
        ],
    )
    .unwrap();
    db.insert(
        "GDB_SpatialRefs",
        &[
            ("ID".into(), SqlValue::I64(1)),
            ("FalseX".into(), SqlValue::F64(0.0)),
            ("FalseY".into(), SqlValue::F64(0.0)),
            ("XYUnits".into(), SqlValue::F64(10000.0)),
            (
                "SRTEXT".into(),
                SqlValue::Text(
                    "GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\"]]".to_string(),
                ),
            ),
        ],
    )
    .unwrap();

    db.create_table(
        "GDB_FeatureDataset",
        vec![col("ID", "LONG", false), col("Name", "TEXT", false), col("SRID", "LONG", false)],
    )
    .unwrap();
    db.insert(
        "GDB_FeatureDataset",
        &[
            ("ID".into(), SqlValue::I64(1)),
            ("Name".into(), SqlValue::Text("Hydrology".into())),
            ("SRID".into(), SqlValue::I64(1)),
        ],
    )
    .unwrap();

    db.create_table(
        "GDB_ObjectClasses",
        vec![
            col("ID", "LONG", false),
            col("Name", "TEXT", false),
            col("DatasetType", "LONG", false),
            col("DatasetID", "LONG", false),
        ],
    )
    .unwrap();
    for (id, name, dtype, dataset_id) in [
        (1, "Roads", 3, None),
        (2, "Counties", 3, None),
        (3, "OwnerTable", 1, None),
        (4, "Ponds", 3, Some(1)),
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
        )
        .unwrap();
    }

    db.create_table(
        "GDB_FeatureClasses",
        vec![
            col("ObjectClassID", "LONG", false),
            col("FeatureType", "LONG", false),
            col("GeometryType", "LONG", false),
            col("ShapeFieldName", "TEXT", false),
        ],
    )
    .unwrap();
    for (ocid, geom_type) in [(1, 3), (2, 4), (4, 4)] {
        db.insert(
            "GDB_FeatureClasses",
            &[
                ("ObjectClassID".into(), SqlValue::I64(ocid)),
                ("FeatureType".into(), SqlValue::I64(1)),
                ("GeometryType".into(), SqlValue::I64(geom_type)),
                ("ShapeFieldName".into(), SqlValue::Text("Shape".into())),
            ],
        )
        .unwrap();
    }

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
    )
    .unwrap();
    for (table, shape_type, left, bottom, right, top) in [
        ("Roads", 3, 0.0, 0.0, 10.0, 10.0),
        ("Counties", 4, 0.0, 0.0, 20.0, 20.0),
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
        )
        .unwrap();
    }

    db.create_table(
        "GDB_FieldInfo",
        vec![
            col("TableName", "TEXT", false),
            col("FieldName", "TEXT", false),
            col("AliasName", "TEXT", false),
        ],
    )
    .unwrap();
    db.insert(
        "GDB_FieldInfo",
        &[
            ("TableName".into(), SqlValue::Text("Roads".into())),
            ("FieldName".into(), SqlValue::Text("NAME".into())),
            ("AliasName".into(), SqlValue::Text("道路名称".into())),
        ],
    )
    .unwrap();

    // ---------------- 业务表 ----------------
    db.create_table(
        "Roads",
        vec![
            col("OBJECTID", "LONG", true),
            col("NAME", "TEXT", false),
            col("Shape_Length", "DOUBLE", false),
            col("Shape", "LONGVARBINARY", false),
        ],
    )
    .unwrap();
    db.insert(
        "Roads",
        &[
            ("NAME".into(), SqlValue::Text("G1".into())),
            ("Shape_Length".into(), SqlValue::F64(10.0)),
            ("Shape".into(), SqlValue::Binary(line_bytes(0.0, 0.0, 10.0, 0.0))),
        ],
    )
    .unwrap();
    db.insert(
        "Roads",
        &[
            ("NAME".into(), SqlValue::Text("G2".into())),
            ("Shape_Length".into(), SqlValue::F64(5.0)),
            ("Shape".into(), SqlValue::Binary(line_bytes(0.0, 5.0, 5.0, 5.0))),
        ],
    )
    .unwrap();

    db.create_table(
        "Counties",
        vec![
            col("OBJECTID", "LONG", true),
            col("NAME", "TEXT", false),
            col("Shape_Area", "DOUBLE", false),
            col("Shape_Length", "DOUBLE", false),
            col("Shape", "LONGVARBINARY", false),
        ],
    )
    .unwrap();
    db.insert(
        "Counties",
        &[
            ("NAME".into(), SqlValue::Text("东城区".into())),
            ("Shape_Area".into(), SqlValue::F64(16.0)),
            ("Shape_Length".into(), SqlValue::F64(16.0)),
            (
                "Shape".into(),
                SqlValue::Binary(polygon_bytes(0.0, 0.0, 4.0, 4.0)),
            ),
        ],
    )
    .unwrap();

    db.create_table(
        "OwnerTable",
        vec![
            col("OBJECTID", "LONG", true),
            col("NAME", "TEXT", false),
            col("REMARK", "MEMO", false),
        ],
    )
    .unwrap();
    db.insert(
        "OwnerTable",
        &[
            ("NAME".into(), SqlValue::Text("北京市".into())),
            ("REMARK".into(), SqlValue::Text("备注".into())),
        ],
    )
    .unwrap();

    db.create_table(
        "Ponds",
        vec![
            col("OBJECTID", "LONG", true),
            col("NAME", "TEXT", false),
            col("Shape", "LONGVARBINARY", false),
        ],
    )
    .unwrap();
    db.insert(
        "Ponds",
        &[
            ("NAME".into(), SqlValue::Text("池塘A".into())),
            (
                "Shape".into(),
                SqlValue::Binary(polygon_bytes(1.0, 1.0, 3.0, 3.0)),
            ),
        ],
    )
    .unwrap();

    // 空间索引表
    for name in ["Roads_SHAPE_Index", "Counties_SHAPE_Index", "Ponds_SHAPE_Index"] {
        db.create_table(
            name,
            vec![
                col("IndexedObjectId", "LONG", false),
                col("MinGX", "DOUBLE", false),
                col("MinGY", "DOUBLE", false),
                col("MaxGX", "DOUBLE", false),
                col("MaxGY", "DOUBLE", false),
            ],
        )
        .unwrap();
    }
    db.insert(
        "Roads_SHAPE_Index",
        &[
            ("IndexedObjectId".into(), SqlValue::I64(1)),
            ("MinGX".into(), SqlValue::F64(0.0)),
            ("MinGY".into(), SqlValue::F64(0.0)),
            ("MaxGX".into(), SqlValue::F64(1.0)),
            ("MaxGY".into(), SqlValue::F64(1.0)),
        ],
    )
    .unwrap();

    db
}

/// 使用 GDB_Items 模型描述的同一个库
pub fn items_database() -> MirrorBackend {
    let db = MirrorBackend::new();

    db.create_table(
        "GDB_ItemTypes",
        vec![col("UUID", "TEXT", false), col("Name", "TEXT", false)],
    )
    .unwrap();
    for (uuid, name) in [
        ("{FC-0001-0000-0000-000000000000}", "Feature Class"),
        ("{TB-0002-0000-0000-000000000000}", "Table"),
        ("{FD-0003-0000-0000-000000000000}", "Feature Dataset"),
    ] {
        db.insert(
            "GDB_ItemTypes",
            &[
                ("UUID".into(), SqlValue::Text(uuid.into())),
                ("Name".into(), SqlValue::Text(name.into())),
            ],
        )
        .unwrap();
    }

    db.create_table(
        "GDB_ItemRelationshipTypes",
        vec![col("UUID", "TEXT", false), col("Name", "TEXT", false)],
    )
    .unwrap();
    db.insert(
        "GDB_ItemRelationshipTypes",
        &[
            (
                "UUID".into(),
                SqlValue::Text("{REL-0001-0000-0000-000000000000}".into()),
            ),
            (
                "Name".into(),
                SqlValue::Text("DatasetInFeatureDataset".into()),
            ),
        ],
    )
    .unwrap();

    db.create_table(
        "GDB_ItemRelationships",
        vec![
            col("OriginID", "TEXT", false),
            col("DestID", "TEXT", false),
            col("Type", "TEXT", false),
        ],
    )
    .unwrap();
    db.insert(
        "GDB_ItemRelationships",
        &[
            (
                "OriginID".into(),
                SqlValue::Text("{HYDROLOGY-ITEM}".into()),
            ),
            ("DestID".into(), SqlValue::Text("{PONDS-ITEM}".into())),
            (
                "Type".into(),
                SqlValue::Text("{REL-0001-0000-0000-000000000000}".into()),
            ),
        ],
    )
    .unwrap();

    db.create_table(
        "GDB_Items",
        vec![
            col("UUID", "TEXT", false),
            col("Type", "TEXT", false),
            col("Name", "TEXT", false),
            col("PhysicalName", "TEXT", false),
            col("DatasetSubtype1", "LONG", false),
            col("DatasetSubtype2", "LONG", false),
            col("DatasetInfo1", "TEXT", false),
        ],
    )
    .unwrap();
    let items = [
        ("{HYDROLOGY-ITEM}", "{FD-0003-0000-0000-000000000000}", "Hydrology", "Hydrology", Some(0), None, Some("")),
        (
            "{PONDS-ITEM}",
            "{FC-0001-0000-0000-000000000000}",
            "Ponds",
            "Ponds",
            Some(1),
            Some(4),
            Some("Shape"),
        ),
        (
            "{ROADS-ITEM}",
            "{FC-0001-0000-0000-000000000000}",
            "Roads",
            "Roads",
            Some(1),
            Some(3),
            Some("Shape"),
        ),
        (
            "{OWNER-ITEM}",
            "{TB-0002-0000-0000-000000000000}",
            "OwnerTable",
            "OwnerTable",
            None,
            None,
            None,
        ),
    ];
    for (uuid, type_uuid, name, physical, sub1, sub2, info1) in items {
        db.insert(
            "GDB_Items",
            &[
                ("UUID".into(), SqlValue::Text(uuid.into())),
                ("Type".into(), SqlValue::Text(type_uuid.into())),
                ("Name".into(), SqlValue::Text(name.into())),
                ("PhysicalName".into(), SqlValue::Text(physical.into())),
                ("DatasetSubtype1".into(), sub1.map_or(SqlValue::Null, SqlValue::I64)),
                ("DatasetSubtype2".into(), sub2.map_or(SqlValue::Null, SqlValue::I64)),
                ("DatasetInfo1".into(), info1.map_or(SqlValue::Null, |s| SqlValue::Text(s.into()))),
            ],
        )
        .unwrap();
    }

    // 复用业务表
    for name in ["Roads", "Ponds", "OwnerTable"] {
        let cols = match name {
            "Roads" => vec![
                col("OBJECTID", "LONG", true),
                col("NAME", "TEXT", false),
                col("Shape_Length", "DOUBLE", false),
                col("Shape", "LONGVARBINARY", false),
            ],
            "Ponds" => vec![
                col("OBJECTID", "LONG", true),
                col("NAME", "TEXT", false),
                col("Shape", "LONGVARBINARY", false),
            ],
            _ => vec![
                col("OBJECTID", "LONG", true),
                col("NAME", "TEXT", false),
                col("REMARK", "MEMO", false),
            ],
        };
        db.create_table(name, cols).unwrap();
    }
    db.insert(
        "Roads",
        &[
            ("NAME".into(), SqlValue::Text("G1".into())),
            ("Shape_Length".into(), SqlValue::F64(10.0)),
            ("Shape".into(), SqlValue::Binary(line_bytes(0.0, 0.0, 10.0, 0.0))),
        ],
    )
    .unwrap();
    db.insert(
        "Ponds",
        &[
            ("NAME".into(), SqlValue::Text("池塘A".into())),
            (
                "Shape".into(),
                SqlValue::Binary(polygon_bytes(1.0, 1.0, 3.0, 3.0)),
            ),
        ],
    )
    .unwrap();
    db.insert(
        "OwnerTable",
        &[
            ("NAME".into(), SqlValue::Text("北京市".into())),
            ("REMARK".into(), SqlValue::Text("备注".into())),
        ],
    )
    .unwrap();
    db.create_table(
        "Ponds_SHAPE_Index",
        vec![
            col("IndexedObjectId", "LONG", false),
            col("MinGX", "DOUBLE", false),
            col("MinGY", "DOUBLE", false),
            col("MaxGX", "DOUBLE", false),
            col("MaxGY", "DOUBLE", false),
        ],
    )
    .unwrap();

    db
}

/// 打开一个工作空间
pub fn open_legacy() -> pgdb::gdb::AccessWorkspace {
    let backend = Arc::new(legacy_database());
    pgdb::gdb::AccessWorkspace::open(backend, "test.mdb").unwrap()
}

/// 打开 Items 模型的工作空间
pub fn open_items() -> pgdb::gdb::AccessWorkspace {
    let backend = Arc::new(items_database());
    pgdb::gdb::AccessWorkspace::open(backend, "test-items.mdb").unwrap()
}
