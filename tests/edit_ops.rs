//! 编辑 API（ArcObjects 风格增删改）的集成测试。
//!
//! 覆盖 [`pgdb::gdb::Table`] 的批量编辑契约：
//! - `update_searched_rows` / `delete_searched_rows`（`ITable::UpdateSearchedRows` /
//!   `ITable::DeleteSearchedRows`）按 [`QueryFilter`] 操作；
//! - `delete_rows(&[i64])`（`ITable::DeleteRows` 的 OID 数组语义）；
//! - `delete_all_rows`（`ITable::DeleteAllRows`）；
//! - [`EditOptions`] 的全表防护与零命中语义；
//! - [`EditResult`] 的 `matched`/`affected`/`scope` 三元组；
//! - `preflight_edit` 只读体检；
//! - 要素类删除路径的空间索引清理与图层范围回缩。

/// 夹具中的 items 模型数据库本文件未用到，允许死代码
#[allow(dead_code)]
mod harness;

use pgdb::datastore::{Predicate, SqlValue};
use pgdb::gdb::{
    EditOptions, EditResult, EditScope, FeatureClass, FeatureWorkspace, QueryFilter, Table,
};
use pgdb::value::Value;
use pgdb::PgdbError;

/// 断言错误为「无效参数」（全表未放行等防护类错误）
fn assert_invalid_argument(err: PgdbError) {
    assert!(
        matches!(err, PgdbError::InvalidArgument(_)),
        "期望 InvalidArgument，实际: {err:?}"
    );
}

/// 断言错误为「找不到对象」（require_hit 零命中语义）
fn assert_not_found(err: PgdbError) {
    assert!(
        matches!(err, PgdbError::NotFound(_)),
        "期望 NotFound，实际: {err:?}"
    );
}

#[test]
fn update_searched_rows_updates_only_matched_rows() {
    let ws = harness::open_legacy();
    let roads = ws.open_feature_class("Roads").unwrap();

    // 按 OID 精确更新一行
    let result = roads
        .update_searched_rows(
            &[("NAME".to_string(), Value::String("沪高速".into()))],
            &QueryFilter::for_oid(1),
            EditOptions::default(),
        )
        .unwrap();
    assert_eq!(result, EditResult::new(1, 1, EditScope::Filtered));

    // 命中的行已更新
    let f1 = roads.get_feature(1).unwrap().unwrap();
    assert_eq!(f1.value_by_name("NAME").unwrap(), &Value::String("沪高速".into()));
    // 未命中的行原样保留
    let f2 = roads.get_feature(2).unwrap().unwrap();
    assert_eq!(f2.value_by_name("NAME").unwrap(), &Value::String("G2".into()));
}

#[test]
fn update_searched_rows_supports_where_clause() {
    let ws = harness::open_legacy();
    let roads = ws.open_feature_class("Roads").unwrap();

    let filter = QueryFilter::new().with_where("NAME = 'G2'");
    let result = roads
        .update_searched_rows(
            &[("Shape_Length".to_string(), Value::Double(99.5))],
            &filter,
            EditOptions::default(),
        )
        .unwrap();
    assert_eq!(result.matched, 1);
    assert_eq!(result.affected, 1);

    let f2 = roads.get_feature(2).unwrap().unwrap();
    assert_eq!(f2.value_by_name("Shape_Length").unwrap(), &Value::Double(99.5));
}

#[test]
fn whole_table_edits_are_rejected_unless_explicitly_allowed() {
    let ws = harness::open_legacy();
    let owner = ws.open_table("OwnerTable").unwrap();

    // 默认防护：trivial filter（无任何条件）必须被拒绝
    let err = owner
        .update_searched_rows(
            &[("REMARK".to_string(), Value::String("x".into()))],
            &QueryFilter::new(),
            EditOptions::default(),
        )
        .unwrap_err();
    assert_invalid_argument(err);

    let err = owner
        .delete_searched_rows(&QueryFilter::new(), EditOptions::default())
        .unwrap_err();
    assert_invalid_argument(err);

    // 拒绝后行数不变
    assert_eq!(owner.row_count(&QueryFilter::new()).unwrap(), 1);

    // 显式 whole_table() 放行后成功，且 scope 报告为 WholeTable
    let result = owner
        .update_searched_rows(
            &[("REMARK".to_string(), Value::String("全表备注".into()))],
            &QueryFilter::new(),
            EditOptions::default().whole_table(),
        )
        .unwrap();
    assert_eq!(result, EditResult::new(1, 1, EditScope::WholeTable));
    assert!(result.scope.is_whole_table());
}

#[test]
fn require_hit_treats_zero_match_as_error() {
    let ws = harness::open_legacy();
    let roads = ws.open_feature_class("Roads").unwrap();
    let missing = QueryFilter::for_oid(9999);

    // 默认：零命中不是错误，返回 matched=0 交调用方判断
    let result = roads
        .update_searched_rows(
            &[("NAME".to_string(), Value::String("x".into()))],
            &missing,
            EditOptions::default(),
        )
        .unwrap();
    assert!(result.is_no_hit());
    assert_eq!(result.matched, 0);
    assert_eq!(result.affected, 0);

    // require_hit：零命中升级为 NotFound
    let err = roads
        .update_searched_rows(
            &[("NAME".to_string(), Value::String("x".into()))],
            &missing,
            EditOptions::default().require_hit(),
        )
        .unwrap_err();
    assert_not_found(err);

    let err = roads
        .delete_searched_rows(&missing, EditOptions::default().require_hit())
        .unwrap_err();
    assert_not_found(err);
}

#[test]
fn delete_rows_deletes_by_oid_list() {
    let ws = harness::open_legacy();
    let owner = ws.open_table("OwnerTable").unwrap();

    // 补两行，拿到各自的 OID
    let oid_a = owner
        .insert_row(&[("NAME".to_string(), Value::String("上海市".into()))])
        .unwrap();
    let oid_b = owner
        .insert_row(&[("NAME".to_string(), Value::String("广州市".into()))])
        .unwrap();
    assert_eq!(owner.row_count(&QueryFilter::new()).unwrap(), 3);

    // OID 数组删除（ITable::DeleteRows 语义）
    let result = owner.delete_rows(&[oid_a, oid_b]).unwrap();
    assert_eq!(result, EditResult::new(2, 2, EditScope::Filtered));
    assert_eq!(owner.row_count(&QueryFilter::new()).unwrap(), 1);

    // 含不存在 OID 时：matched 只统计真实存在的行
    let result = owner.delete_rows(&[9999]).unwrap();
    assert!(result.is_no_hit());
    assert_eq!(result.matched, 0);

    // 空列表是安全的空操作
    let result = owner.delete_rows(&[]).unwrap();
    assert_eq!(result.matched, 0);
    assert_eq!(owner.row_count(&QueryFilter::new()).unwrap(), 1);
}

#[test]
fn delete_searched_rows_deletes_matched_rows() {
    let ws = harness::open_legacy();
    let owner = ws.open_table("OwnerTable").unwrap();

    let oid = owner
        .insert_row(&[("NAME".to_string(), Value::String("深圳市".into()))])
        .unwrap();

    // WHERE 条件删除
    let filter = QueryFilter::new().with_where("NAME = '深圳市'");
    let result = owner
        .delete_searched_rows(&filter, EditOptions::default())
        .unwrap();
    assert_eq!(result, EditResult::new(1, 1, EditScope::Filtered));
    assert_eq!(owner.row_count(&QueryFilter::new()).unwrap(), 1);

    // OID 条件删除
    let oid2 = owner
        .insert_row(&[("NAME".to_string(), Value::String("杭州市".into()))])
        .unwrap();
    let result = owner
        .delete_searched_rows(&QueryFilter::for_oid(oid2), EditOptions::default())
        .unwrap();
    assert_eq!(result.matched, 1);
    let _ = oid;
}

#[test]
fn delete_all_rows_clears_table() {
    let ws = harness::open_legacy();
    let owner = ws.open_table("OwnerTable").unwrap();

    let result = owner.delete_all_rows().unwrap();
    assert_eq!(result, EditResult::new(1, 1, EditScope::WholeTable));
    assert_eq!(owner.row_count(&QueryFilter::new()).unwrap(), 0);

    // 空表上重复清空是安全的
    let result = owner.delete_all_rows().unwrap();
    assert_eq!(result, EditResult::new(0, 0, EditScope::WholeTable));
}

#[test]
fn preflight_edit_reports_scope_hits_and_writability() {
    let ws = harness::open_legacy();
    let roads = ws.open_feature_class("Roads").unwrap();

    // 条件体检
    let pre = roads.preflight_edit(&QueryFilter::for_oid(2)).unwrap();
    assert_eq!(pre.matched, 1);
    assert!(!pre.is_whole_table());
    assert!(pre.writable);

    // 全表体检
    let pre = roads.preflight_edit(&QueryFilter::new()).unwrap();
    assert_eq!(pre.matched, 2);
    assert!(pre.is_whole_table());

    // 零命中体检
    let pre = roads.preflight_edit(&QueryFilter::for_oid(4242)).unwrap();
    assert!(pre.is_no_hit());

    // 体检不写库：行数不变
    assert_eq!(roads.row_count(&QueryFilter::new()).unwrap(), 2);
}

#[test]
fn delete_feature_cleans_shape_index_and_shrinks_extent() {
    let ws = harness::open_legacy();
    let roads = ws.open_feature_class("Roads").unwrap();

    // 初始范围 (0,0)-(10,10)（来自 GDB_GeomColumns 预置值）
    let initial = roads.extent().unwrap();
    assert_eq!(initial.min_x, 0.0);
    assert_eq!(initial.max_x, 10.0);

    // 删除 OID=1（线 (0,0)-(10,0)，触及原范围边界）→ 触发重算
    let result = roads
        .delete_searched_rows(&QueryFilter::for_oid(1), EditOptions::default())
        .unwrap();
    assert_eq!(result, EditResult::new(1, 1, EditScope::Filtered));

    // 空间索引记录同步清除
    let idx = roads
        .backend()
        .select(
            "Roads_SHAPE_Index",
            &[],
            &Predicate::eq("IndexedObjectId", 1i64),
        )
        .unwrap();
    assert!(idx.is_empty(), "删除要素后空间索引记录应被清理");

    // 剩余要素 G2（线 (0,5)-(5,5)）的范围 (0,5)-(5,5)
    let shrunk = roads.extent().unwrap();
    assert_eq!(shrunk.min_x, 0.0);
    assert_eq!(shrunk.min_y, 5.0);
    assert_eq!(shrunk.max_x, 5.0);
    assert_eq!(shrunk.max_y, 5.0);
}

#[test]
fn delete_all_rows_resets_layer_extent() {
    let ws = harness::open_legacy();
    let roads = ws.open_feature_class("Roads").unwrap();

    let result = roads.delete_all_rows().unwrap();
    assert_eq!(result.matched, 2);
    assert_eq!(roads.row_count(&QueryFilter::new()).unwrap(), 0);

    // 清空后图层范围归零（与 ArcCatalog 清空图层后的表现一致）
    let env = roads.extent().unwrap();
    assert_eq!(env.min_x, 0.0);
    assert_eq!(env.min_y, 0.0);
    assert_eq!(env.max_x, 0.0);
    assert_eq!(env.max_y, 0.0);

    // 空间索引表整表清空
    let idx = roads
        .backend()
        .select("Roads_SHAPE_Index", &[], &Predicate::All)
        .unwrap();
    assert!(idx.is_empty());
}

#[test]
fn update_geometry_accepts_wkt_blob_and_null() {
    let ws = harness::open_legacy();
    let roads = ws.open_feature_class("Roads").unwrap();

    // 1) WKT 文本更新几何：解码、编码落库、量算与范围同步
    let result = roads
        .update_searched_rows(
            &[("Shape".to_string(), Value::String("LINESTRING(0 0, 20 0)".into()))],
            &QueryFilter::for_oid(1),
            EditOptions::default(),
        )
        .unwrap();
    assert_eq!(result.matched, 1);

    // Shape_Length 被重算为 20
    let f1 = roads.get_feature(1).unwrap().unwrap();
    assert_eq!(f1.value_by_name("Shape_Length").unwrap(), &Value::Double(20.0));

    // Shape 落库为 ESRI 二进制（而非 WKT 文本），可正常解码
    let stored = f1.value_by_name("Shape").unwrap();
    let SqlValue::Binary(bytes) = SqlValue::from(stored.clone()) else {
        panic!("Shape 列应落库为二进制，实际: {stored:?}");
    };
    let geom = pgdb::geom::codec::decode_shape(&bytes).unwrap();
    let env = geom.envelope().unwrap();
    assert_eq!(env.max_x, 20.0);

    // 图层范围因并集扩展到 (0,0)-(20,10)
    let ext = roads.extent().unwrap();
    assert_eq!(ext.max_x, 20.0);
    assert_eq!(ext.max_y, 10.0);

    // 空间索引记录同步到新几何
    let idx = roads
        .backend()
        .select(
            "Roads_SHAPE_Index",
            &[],
            &Predicate::eq("IndexedObjectId", 1i64),
        )
        .unwrap();
    assert_eq!(idx.len(), 1);

    // 2) Blob 二进制更新几何
    let result = roads
        .update_searched_rows(
            &[("Shape".to_string(), Value::Blob(harness::line_bytes(1.0, 1.0, 2.0, 2.0)))],
            &QueryFilter::for_oid(1),
            EditOptions::default(),
        )
        .unwrap();
    assert_eq!(result.matched, 1);
    let f1 = roads.get_feature(1).unwrap().unwrap();
    assert_eq!(f1.value_by_name("Shape_Length").unwrap(), &Value::Double(2.0_f64.sqrt()));

    // 3) 置空几何：索引记录清除、量算归零、其余行不受影响
    let result = roads
        .update_searched_rows(
            &[("Shape".to_string(), Value::Null)],
            &QueryFilter::for_oid(1),
            EditOptions::default(),
        )
        .unwrap();
    assert_eq!(result.matched, 1);

    let idx = roads
        .backend()
        .select(
            "Roads_SHAPE_Index",
            &[],
            &Predicate::eq("IndexedObjectId", 1i64),
        )
        .unwrap();
    assert!(idx.is_empty(), "几何置空后空间索引记录应被清除");

    let f1 = roads.get_feature(1).unwrap().unwrap();
    assert_eq!(f1.value_by_name("Shape").unwrap(), &Value::Null);
    assert_eq!(f1.value_by_name("Shape_Length").unwrap(), &Value::Double(0.0));
}

#[test]
fn insert_feature_accepts_wkt_geometry() {
    let ws = harness::open_legacy();
    let ponds = ws.open_feature_class("Hydrology\\Ponds").unwrap();

    let oid = ponds
        .insert_row(&[
            ("NAME".to_string(), Value::String("WKT池塘".into())),
            ("Shape".to_string(), Value::String("POLYGON((0 0, 0 4, 4 4, 4 0, 0 0))".into())),
        ])
        .unwrap();

    // WKT 落库为 ESRI 二进制且面积被重算
    let f = ponds.get_feature(oid).unwrap().unwrap();
    let stored = f.value_by_name("Shape").unwrap();
    let SqlValue::Binary(bytes) = SqlValue::from(stored.clone()) else {
        panic!("Shape 列应落库为二进制，实际: {stored:?}");
    };
    let geom = pgdb::geom::codec::decode_shape(&bytes).unwrap();
    assert_eq!(geom.envelope().unwrap(), pgdb::Envelope::new(0.0, 0.0, 4.0, 4.0));
    assert_eq!(f.value_by_name("NAME").unwrap(), &Value::String("WKT池塘".into()));

    // 清理
    let result = ponds.delete_rows(&[oid]).unwrap();
    assert_eq!(result.matched, 1);
}

#[test]
fn predicate_in_matches_ids_and_values() {
    let ws = harness::open_legacy();
    let roads = ws.open_feature_class("Roads").unwrap();
    let backend = roads.backend();

    // in_ids：按 OID 集合
    let rows = backend
        .select("Roads", &[], &Predicate::in_ids("OBJECTID", &[1]))
        .unwrap();
    assert_eq!(rows.len(), 1);

    // in_values：按字段值集合
    let rows = backend
        .select(
            "Roads",
            &[],
            &Predicate::in_values("NAME", ["G1".to_string(), "G2".to_string()]),
        )
        .unwrap();
    assert_eq!(rows.len(), 2);

    // 空集合匹配不到任何行（SQL 侧渲染为 1 = 0）
    let rows = backend
        .select("Roads", &[], &Predicate::in_ids("OBJECTID", &[]))
        .unwrap();
    assert!(rows.is_empty());

    // delete_rows 内部即走 In 谓词：混合存在/不存在的 OID
    let result = roads.delete_rows(&[2, 4242]).unwrap();
    assert_eq!(result.matched, 1);
    assert_eq!(roads.row_count(&QueryFilter::new()).unwrap(), 1);
}
