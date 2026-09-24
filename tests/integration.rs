//! 集成测试：遍历 + 属性更新 + 几何更新 + 空间索引一致性。

mod harness;

use pgdb::gdb::{
    DatasetHandle, DatasetKind, DatasetNode, FeatureClass, FeatureDataset, FeatureWorkspace, QueryFilter, SpatialFilter, Table, Value, Workspace, open_workspace_read_only,
};
use pgdb::geom::codec::decode_shape;
use pgdb::geom::{AsWkt, Geometry, GeometryType, Vertex};

#[test]
fn traverse_dataset_tree() {
    let ws = harness::open_legacy();
    let all = ws.all_datasets().unwrap();
    let names: Vec<String> = all.iter().map(|h| h.qualified_name()).collect();
    assert!(names.contains(&"Roads".to_string()), "{names:?}");
    assert!(names.contains(&"Counties".to_string()));
    assert!(names.contains(&"OwnerTable".to_string()));
    assert!(
        names.contains(&"Hydrology\\Ponds".to_string()),
        "要素数据集内的要素类应以限定名出现: {names:?}"
    );

    let mut top = ws.datasets().unwrap();
    let mut top_names = Vec::new();
    while let Some(h) = top.next_dataset() {
        top_names.push(h.qualified_name());
    }
    assert!(top_names.contains(&"Hydrology".to_string()));

    // 打开要素数据集并枚举子数据集
    let fds = ws.open_feature_dataset("Hydrology").unwrap();
    let children: Vec<String> = fds
        .subsets()
        .iter()
        .map(|h| h.qualified_name())
        .collect();
    assert_eq!(children, vec!["Hydrology\\Ponds".to_string()]);
}

#[test]
fn open_independent_table_and_feature_class() {
    let ws = harness::open_legacy();
    let table = ws.open_table("OwnerTable").unwrap();
    assert_eq!(table.kind(), DatasetKind::Table);
    assert_eq!(table.row_count(&QueryFilter::new()).unwrap(), 1);
    assert_eq!(table.fields().count(), 3);

    let fc = ws.open_feature_class("Roads").unwrap();
    assert_eq!(fc.shape_field_name(), "Shape");
    assert_eq!(fc.shape_type(), GeometryType::Polyline);
    assert_eq!(fc.length_field_name(), Some("Shape_Length"));
    assert_eq!(fc.row_count(&QueryFilter::new()).unwrap(), 2);

    // 字段别名应从 GDB_FieldInfo 读取
    let alias = fc.fields().by_name("NAME").and_then(|f| f.alias.clone());
    assert_eq!(alias, Some("道路名称".into()));

    // 空间参考应关联到 GDB_SpatialRefs
    let sr = ws.spatial_ref(1).expect("应有 SRID=1 的空间参考");
    assert!(sr.wkt.as_deref().unwrap().contains("GCS_WGS_1984"));
}

#[test]
fn read_geometry_of_features() {
    let ws = harness::open_legacy();
    let fc = ws.open_feature_class("Roads").unwrap();
    let mut cursor = fc.search_features(QueryFilter::new()).unwrap();
    let feature = cursor.next_feature().unwrap().unwrap();
    let geom = feature.geometry().unwrap().unwrap();
    match geom {
        Geometry::Polyline(ref paths) => {
            assert_eq!(paths.len(), 1);
            assert_eq!(paths[0].len(), 2);
            assert_eq!(paths[0][0].x, 0.0);
            assert_eq!(paths[0][1].x, 10.0);
        }
        other => panic!("应为线要素: {other:?}"),
    }
    assert_eq!(
        geom.as_wkt_precision(1),
        "LINESTRING(0 0, 10 0)".to_string()
    );

    // GetFeature(OID) 等价物
    let f = fc.get_feature(2).unwrap().unwrap();
    assert_eq!(f.oid(), Some(2));
    assert_eq!(
        f.value_by_name("NAME").unwrap().as_string().unwrap(),
        "G2".to_string()
    );
}

#[test]
fn update_attributes_through_update_cursor() {
    let ws = harness::open_legacy();
    let table = ws.open_table("OwnerTable").unwrap();
    let changed = {
        let mut cursor = table.update(QueryFilter::new()).unwrap();
        cursor
            .for_each_update(|row| {
                row.set_value_by_name("REMARK", Value::String("已核对".into()))?;
                Ok(())
            })
            .unwrap()
    };
    assert_eq!(changed, 1);

    let row = table.get_row(1).unwrap().unwrap();
    assert_eq!(
        row.value_by_name("REMARK").unwrap().as_string().unwrap(),
        "已核对".to_string()
    );

    // 未赋值的字段不应被覆盖
    assert_eq!(
        row.value_by_name("NAME").unwrap().as_string().unwrap(),
        "北京市".to_string()
    );
}

#[test]
fn update_geometry_maintains_length_and_spatial_index() {
    let ws = harness::open_legacy();
    let fc = ws.open_feature_class("Roads").unwrap();

    let new_geom = Geometry::Polyline(vec![vec![
        Vertex::new(0.0, 0.0),
        Vertex::new(3.0, 4.0),
    ]]);
    {
        let mut cursor = fc
            .update_features(QueryFilter::for_oid(1))
            .unwrap();
        while let Some(mut feature) = cursor.next_feature().unwrap() {
            feature.set_geometry(&new_geom).unwrap();
            feature.store().unwrap();
        }
    }

    // 1) Shape 二进制被重写，几何可正确解码
    let updated = fc.get_feature(1).unwrap().unwrap();
    let decoded = updated.geometry().unwrap().unwrap();
    assert_eq!(decoded, new_geom);

    // 2) Shape_Length 自动重算（3-4-5 三角形）
    assert_eq!(
        updated.value_by_name("Shape_Length").unwrap().as_double().unwrap(),
        5.0
    );

    // 3) _SHAPE_Index 记录同步更新：grid size = 420
    let backend = fc.backend();
    let idx_rows = backend
        .select(
            "Roads_SHAPE_Index",
            &[],
            &pgdb::datastore::Predicate::eq("IndexedObjectId", 1i64),
        )
        .unwrap();
    assert_eq!(idx_rows.len(), 1);
    assert_eq!(
        idx_rows[0].get("MinGX").unwrap().to_f64().unwrap(),
        0.0_f64
    );
    assert_eq!(idx_rows[0].get("MinGY").unwrap().to_f64().unwrap(), 0.0);
    assert_eq!(idx_rows[0].get("MaxGX").unwrap().to_f64().unwrap(), 1.0);
    assert_eq!(idx_rows[0].get("MaxGY").unwrap().to_f64().unwrap(), 1.0);

    // 4) GDB_GeomColumns 的图层范围被扩展（原 0~10，新几何在内）
    let geom_cols = backend
        .select(
            "GDB_GeomColumns",
            &[],
            &pgdb::datastore::Predicate::eq("TableName", "Roads".to_string()),
        )
        .unwrap();
    assert_eq!(
        geom_cols[0].get("ExtentLeft").unwrap().to_f64().unwrap(),
        0.0
    );
}

#[test]
fn insert_and_delete_feature_keeps_index_consistent() {
    let ws = harness::open_legacy();
    let ponds = ws.open_feature_class("Hydrology\\Ponds").unwrap();

    let oid = {
        let mut cursor = ponds.insert_feature_cursor().unwrap();
        cursor
            .buffer()
            .set_value("NAME", Value::String("新池塘".into()));
        cursor
            .set_geometry(&Geometry::Polygon(vec![vec![
                Vertex::new(10.0, 10.0),
                Vertex::new(10.0, 14.0),
                Vertex::new(14.0, 14.0),
                Vertex::new(14.0, 10.0),
                Vertex::new(10.0, 10.0),
            ]]))
            .unwrap();
        let oid = cursor.insert_feature().unwrap();
        cursor.flush().unwrap();
        oid
    };
    assert!(oid > 0);
    assert_eq!(ponds.row_count(&QueryFilter::new()).unwrap(), 2);

    let rows = ponds
        .backend()
        .select(
            "Ponds_SHAPE_Index",
            &[],
            &pgdb::datastore::Predicate::eq("IndexedObjectId", oid),
        )
        .unwrap();
    assert_eq!(rows.len(), 1, "新要素必须写入空间索引，否则 ArcMap 无法定位");

    // 删除要素时同步清理索引记录
    let feature = ponds.get_feature(oid).unwrap().unwrap();
    let _ = feature;
    ponds.delete_searched_rows(
        &QueryFilter::for_oid(oid),
        pgdb::gdb::EditOptions::default(),
    )
    .unwrap();
    let rows = ponds
        .backend()
        .select(
            "Ponds_SHAPE_Index",
            &[],
            &pgdb::datastore::Predicate::eq("IndexedObjectId", oid),
        )
        .unwrap();
    assert!(rows.is_empty(), "删除要素后空间索引记录应被清理");
}

#[test]
fn polygon_orientation_is_normalized_on_write() {
    let ws = harness::open_legacy();
    let counties = ws.open_feature_class("Counties").unwrap();
    let ccw = Geometry::Polygon(vec![vec![
        Vertex::new(0.0, 0.0),
        Vertex::new(4.0, 0.0),
        Vertex::new(4.0, 4.0),
        Vertex::new(0.0, 4.0),
        Vertex::new(0.0, 0.0),
    ]]);
    {
        let mut cursor = counties.update_features(QueryFilter::new()).unwrap();
        while let Some(mut f) = cursor.next_feature().unwrap() {
            f.set_geometry(&ccw).unwrap();
            f.store().unwrap();
        }
    }
    let stored = counties.get_feature(1).unwrap().unwrap();
    let geom = stored.geometry().unwrap().unwrap();
    // 写入时被规范为顺时针（有符号面积为负）
    let area = pgdb::geom::ops::signed_area(geom.parts()[0]);
    assert!(area < 0.0, "外环应为顺时针，实际 signed_area={area}");
    assert_eq!(
        stored.value_by_name("Shape_Area").unwrap().as_double().unwrap(),
        16.0
    );
}

#[test]
fn spatial_filter_and_row_cursor_selection() {
    let ws = harness::open_legacy();
    let fc = ws.open_feature_class("Roads").unwrap();
    let filter = QueryFilter::new().with_spatial(SpatialFilter::intersects(
        Geometry::Point(Vertex::new(0.0, 5.0)),
    ));
    let features = fc.search_features(filter).unwrap().collect_features().unwrap();
    assert_eq!(features.len(), 1, "只有穿过/落在 (0,5) 附近的线会被选中");
    assert_eq!(
        features[0].value_by_name("NAME").unwrap().as_string().unwrap(),
        "G2".to_string()
    );
}

#[test]
fn items_model_is_supported() {
    let ws = harness::open_items();
    assert_eq!(ws.metadata_model(), pgdb::gdb::MetadataModel::Items);

    let all = ws.all_datasets().unwrap();
    let names: Vec<String> = all.iter().map(|h| h.qualified_name()).collect();
    assert!(names.contains(&"Hydrology\\Ponds".to_string()), "{names:?}");
    assert!(names.contains(&"Roads".to_string()));
    assert!(names.contains(&"OwnerTable".to_string()));

    let ponds = ws.open_feature_class("Ponds").unwrap();
    assert_eq!(ponds.shape_type(), GeometryType::Polygon);
    let mut cursor = ponds.search_features(QueryFilter::new()).unwrap();
    let f = cursor.next_feature().unwrap().unwrap();
    let g = f.geometry().unwrap().unwrap();
    assert_eq!(g.geometry_type(), GeometryType::Polygon);
    assert_eq!(g.envelope().unwrap().min_x, 1.0);
}

#[test]
fn raw_shape_bytes_stay_shapefile_compatible() {
    // 写入后的 Shape 二进制必须能被标准 shapefile 记录体解析方式读取
    let ws = harness::open_legacy();
    let fc = ws.open_feature_class("Roads").unwrap();
    let row = fc.get_row(1).unwrap().unwrap();
    let bytes = row
        .value_by_name("Shape")
        .unwrap()
        .as_blob()
        .unwrap()
        .to_vec();
    let geom = decode_shape(&bytes).unwrap();
    assert!(matches!(geom, Geometry::Polyline(_)));
}


#[test]
fn open_mdb_test01() {
    let mdb_path = r"C:\Users\ASUS\Desktop\test.mdb" ;
    let db = open_workspace_read_only(mdb_path) ;
    assert!(db.is_ok()) ;
    let all_datasets = db.unwrap().all_datasets().unwrap() ;
    for item in all_datasets {
        let tb_info = match item {
            DatasetHandle::Table(_t) => (&_t.name().to_string(), _t.row_count(&QueryFilter::default()).unwrap_or_default()),
            DatasetHandle::FeatureClass(_f) => (&_f.name().to_string(), _f.row_count(&QueryFilter::default()).unwrap_or_default()) ,
            DatasetHandle::FeatureDataset(_d) => (&_d.name().to_string(), 0),
        } ;
        
        println!("{:?}", tb_info) ;
    }
}