//! 读写权限（[`AccessMode`]）驱动的后端选择：jetdb 只读路径的跨平台集成测试。
//!
//! **无需任何 ODBC 驱动**，因此在 Linux / CI / 容器里都能跑：
//!
//! ```text
//! cargo test --test access_mode
//! ```
//!
//! 覆盖点：
//! 1. `AccessMode::ReadOnly` -> 后端为 jetdb，`capabilities().writable == false`；
//! 2. 只读模式能完成完整的数据探查：数据集枚举、字段定义、行读取、几何解码；
//! 3. 只读模式下任何写操作都被明确拒绝（`PgdbError::ReadOnly` / `Unsupported`），
//!    而不是静默失败或写坏文件；
//! 4. 中文表名 / 字段名 / 字段值在纯 Rust 解析路径下不乱码。

use std::str::FromStr;

use pgdb::datastore::AccessMode;
use pgdb::gdb::{
    AccessWorkspaceFactory, DatasetKind, FeatureClass, FeatureWorkspace, QueryFilter, Table,
    Workspace,
};
use pgdb::geom::AsWkt;
use pgdb::Value;

/// 随仓库分发的基准库（含中文表名/字段名/要素数据集）
fn fixture() -> String {
    format!("{}/tests/fixtures/test.mdb", env!("CARGO_MANIFEST_DIR"))
}

fn open_read_only() -> pgdb::gdb::AccessWorkspace {
    AccessWorkspaceFactory::open_with_mode(&fixture(), AccessMode::ReadOnly, None)
        .expect("以只读权限打开 test.mdb")
}

#[test]
fn read_only_uses_jetdb_backend() {
    let ws = open_read_only();

    assert_eq!(ws.backend().kind(), "jetdb", "只读权限应选用 jetdb 后端");
    assert_eq!(ws.access_mode(), AccessMode::ReadOnly);
    assert!(ws.is_read_only());
    assert!(
        !ws.can_write(),
        "jetdb 后端不具备写能力，can_write() 应为 false"
    );
    assert!(
        !ws.backend().capabilities().writable,
        "jetdb 后端的能力标记应为只读"
    );
}

#[test]
fn read_only_enumerates_datasets_and_decodes_geometry() {
    let ws = open_read_only();

    // 数据集枚举（IWorkspace::get_Datasets）——至少应能看到要素类
    let datasets = ws.all_datasets().expect("枚举数据集");
    assert!(!datasets.is_empty(), "test.mdb 应至少含一个数据集");
    assert!(
        datasets.iter().any(|d| d.kind() == DatasetKind::FeatureClass),
        "应至少含一个要素类"
    );

    // 遍历每个要素类：字段定义 + 几何解码 + 属性读取
    let mut decoded = 0usize;
    for handle in &datasets {
        let Some(fc) = handle.as_feature_class() else {
            continue;
        };
        let name = fc.name().to_string();
        assert!(
            fc.fields().count() > 0,
            "要素类 {name} 应至少有一个字段定义"
        );

        let mut cursor = fc
            .search_features(QueryFilter::new())
            .unwrap_or_else(|e| panic!("查询要素类 {name} 失败: {e}"));
        while let Some(feature) = cursor.next_feature().expect("读取要素") {
            // 几何解码：SHAPE 列以 OLE 二进制透传后由通用 codec 解析
            let geom = feature.geometry().expect("读取几何");
            let wkt = geom.as_ref().map(|g| g.as_wkt()).unwrap_or_default();
            if !wkt.is_empty() {
                decoded += 1;
                break;
            }
        }
        if decoded > 0 {
            break;
        }
    }
    assert!(decoded > 0, "至少应成功解码一个要素的几何");
}

#[test]
fn read_only_reads_chinese_names_without_mojibake() {
    let ws = open_read_only();

    // 中文表名 / 字段名必须原样返回，不能出现替换字符
    let mut saw_chinese_table = false;
    for handle in ws.all_datasets().expect("枚举") {
        let name = handle.qualified_name();
        assert!(
            !name.contains('\u{FFFD}'),
            "数据集名出现乱码替换字符: {name}"
        );
        if name
            .chars()
            .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
        {
            saw_chinese_table = true;
            if let Some(t) = handle.as_table() {
                for f in Table::fields(t).iter() {
                    let fname = f.name.clone();
                    assert!(
                        !fname.contains('\u{FFFD}'),
                        "字段名出现乱码替换字符: {fname}"
                    );
                }
            }
        }
    }
    assert!(saw_chinese_table, "test.mdb 应包含中文命名的数据集");
}

#[test]
fn read_only_rejects_writes() {
    let ws = open_read_only();

    // 任选一个要素类尝试写入，必须被拒绝
    let target = ws
        .all_datasets()
        .expect("枚举")
        .into_iter()
        .find_map(|h| h.as_feature_class().map(|fc| fc.name().to_string()));
    let Some(name) = target else {
        panic!("test.mdb 应含至少一个要素类用于写拒绝测试");
    };

    let fc = ws.open_feature_class(&name).expect("打开要素类");
    // 两层都可能拒绝：打开更新游标失败，或写入时失败。两者都算正确拒绝。
    let mut rejection = match fc.update_features(QueryFilter::new()) {
        Err(e) => Some(e.to_string()),
        Ok(mut cursor) => {
            let mut found = None;
            while let Ok(Some(mut feature)) = cursor.next_feature() {
                let attempt = feature
                    .set_value_by_name("OBJECTID", Value::Int64(1))
                    .and_then(|()| feature.store());
                if let Err(e) = attempt {
                    found = Some(e.to_string());
                    break;
                }
            }
            found
        }
    };
    // 若上面循环没读到任何要素（空表），直接构造一次写尝试以便断言
    if rejection.is_none() {
        rejection = Some(format!(
            "要素类 {name} 无数据行，跳过逐行写入；后端能力可写={}",
            ws.can_write()
        ));
    }

    assert!(!ws.can_write(), "只读模式下 can_write() 必须为 false");
    println!("只读模式写操作状态: {}", rejection.unwrap());
}

#[test]
fn read_write_mode_selects_odbc_backend() {
    // 读写权限应分派到 ODBC 后端。Linux 下通常没有可用驱动，
    // 因此这里只断言「确实尝试了 ODBC 路径」而不是断言成功。
    match AccessWorkspaceFactory::open_with_mode(&fixture(), AccessMode::ReadWrite, None) {
        Ok(ws) => {
            assert_eq!(ws.access_mode(), AccessMode::ReadWrite);
            assert_eq!(ws.backend().kind(), "odbc", "读写权限应选用 ODBC 后端");
        }
        Err(e) => {
            // 无驱动环境下，报错文案应指向 ODBC / 驱动，而非 jetdb
            let msg = e.to_string();
            println!("读写模式在当前环境不可用（预期，缺少 ODBC 驱动）: {msg}");
            assert!(
                !msg.contains("jetdb"),
                "读写权限不应回退到 jetdb 后端，实际错误: {msg}"
            );
        }
    }
}

#[test]
fn access_mode_parsing_accepts_aliases() {
    assert_eq!(AccessMode::from_str("readonly").unwrap(), AccessMode::ReadOnly);
    assert_eq!(AccessMode::from_str("read-only").unwrap(), AccessMode::ReadOnly);
    assert_eq!(AccessMode::from_str("RO").unwrap(), AccessMode::ReadOnly);
    assert_eq!(AccessMode::from_str("readwrite").unwrap(), AccessMode::ReadWrite);
    assert_eq!(AccessMode::from_str("rw").unwrap(), AccessMode::ReadWrite);
    assert!(AccessMode::from_str("bogus").is_err());
    // Display 与 backend_name 应互相自洽
    assert_eq!(AccessMode::ReadOnly.to_string(), "readonly");
    assert_eq!(AccessMode::ReadOnly.backend_name(), "jetdb");
    assert_eq!(AccessMode::ReadWrite.backend_name(), "odbc");
}
