//! 内存镜像后端：把 Personal Geodatabase 的表结构 + 数据完整保存在进程内存里。
//!
//! 用途：
//! 1. 在单元测试 / 集成测试里充当确定性的数据源，演练目录遍历、要素更新、
//!    Shape 二进制读写等全部逻辑，而无需任何 ODBC 驱动；
//! 2. 作为 `SqlBackend` 的一种实现，与 ODBC 后端共享上层的目录 / 游标逻辑。
//!
//! 该后端**不落盘、不涉及任何 JSON 序列化**：它只是数据库在内存中的一份副本。
//! 需要持久化时，请直接对真实 `*.mdb` 使用 ODBC 后端。

use std::collections::HashMap;
use std::sync::RwLock;

use crate::datastore::predicate_matches;
use crate::error::Result;
use crate::value::SqlValue;

use super::{BackendCapabilities, ColumnDef, DataTableRow, Predicate, SqlBackend};

/// 内存中的表
#[derive(Debug, Clone)]
struct Table {
    columns: Vec<ColumnDef>,
    rows: Vec<Vec<SqlValue>>,
    /// 自增列下标
    auto_column: Option<usize>,
    /// 下一个自增值
    next_auto: i64,
}

impl Table {
    fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }

    fn build_row(&self, values: &[(String, SqlValue)], assign_auto: Option<i64>) -> Result<Vec<SqlValue>> {
        let mut out = Vec::with_capacity(self.columns.len());
        for (idx, col) in self.columns.iter().enumerate() {
            if let Some((_, v)) = values.iter().find(|(n, _)| n.eq_ignore_ascii_case(&col.name)) {
                out.push(v.clone());
                continue;
            }
            let auto_here = Some(idx) == self.auto_column;
            if let Some(next) = assign_auto.filter(|_| auto_here) {
                out.push(SqlValue::I64(next));
                continue;
            }
            out.push(SqlValue::Null);
        }
        Ok(out)
    }

    fn to_data_row(&self, idx: usize, columns: &[String]) -> DataTableRow {
        let names = if columns.is_empty() {
            self.column_names()
        } else {
            columns.to_vec()
        };
        let vals: Vec<SqlValue> = names
            .iter()
            .map(|n| {
                let ci = self
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(n));
                match ci {
                    Some(i) => self.rows[idx][i].clone(),
                    None => SqlValue::Null,
                }
            })
            .collect();
        DataTableRow::new(names, vals).expect("列数一致")
    }
}

/// 内存镜像后端
pub struct MirrorBackend {
    state: RwLock<MirrorState>,
}

#[derive(Default)]
struct MirrorState {
    order: Vec<String>,
    tables: HashMap<String, Table>,
}

impl MirrorBackend {
    /// 创建空的内存数据库
    pub fn new() -> Self {
        Self {
            state: RwLock::new(MirrorState::default()),
        }
    }

    /// 新建一张表（供测试与内存构造使用）
    pub fn create_table(&self, name: &str, columns: Vec<ColumnDef>) -> Result<()> {
        let mut st = self.state.write().unwrap();
        Self::create_table_locked(&mut st, name, columns)
    }

    fn create_table_locked(st: &mut MirrorState, name: &str, columns: Vec<ColumnDef>) -> Result<()> {
        let auto_column = columns.iter().position(|c| c.is_auto);
        if !st.tables.contains_key(name) {
            st.order.push(name.to_string());
        }
        st.tables.insert(
            name.to_string(),
            Table {
                columns,
                rows: Vec::new(),
                auto_column,
                next_auto: 1,
            },
        );
        Ok(())
    }

    fn with_table<R>(&self, table: &str, f: impl FnOnce(&Table) -> Result<R>) -> Result<R> {
        let st = self.state.read().unwrap();
        let t = st
            .tables
            .get(table)
            .ok_or_else(|| crate::error::PgdbError::NotFound(format!("表 {table}")))?;
        f(t)
    }
}

impl Default for MirrorBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SqlBackend for MirrorBackend {
    fn kind(&self) -> &'static str {
        "memory"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            raw_sql: false,
            binary_parameter: true,
            hex_literal_binary: false,
            writable: true,
            transaction: false,
        }
    }

    fn table_names(&self) -> Result<Vec<String>> {
        let st = self.state.read().unwrap();
        Ok(st.order.clone())
    }

    fn table_exists(&self, table: &str) -> Result<bool> {
        let st = self.state.read().unwrap();
        let key = table.to_string();
        Ok(st
            .tables
            .keys()
            .any(|k| k.eq_ignore_ascii_case(&key) || key.eq_ignore_ascii_case(k)))
    }

    fn columns(&self, table: &str) -> Result<Vec<ColumnDef>> {
        self.with_table(table, |t| Ok(t.columns.clone()))
    }

    fn select(
        &self,
        table: &str,
        columns: &[String],
        filter: &Predicate,
    ) -> Result<Vec<DataTableRow>> {
        self.with_table(table, |t| {
            let mut out = Vec::new();
            for i in 0..t.rows.len() {
                let row = t.to_data_row(i, columns);
                if predicate_matches(filter, &row) {
                    out.push(row);
                }
            }
            Ok(out)
        })
    }

    fn update(
        &self,
        table: &str,
        sets: &[(String, SqlValue)],
        filter: &Predicate,
    ) -> Result<u64> {
        let mut st = self.state.write().unwrap();
        let t = st
            .tables
            .get_mut(table)
            .ok_or_else(|| crate::error::PgdbError::NotFound(format!("表 {table}")))?;
        let mut affected = 0u64;
        for i in 0..t.rows.len() {
            let row = t.to_data_row(i, &[]);
            if !predicate_matches(filter, &row) {
                continue;
            }
            for (name, value) in sets {
                if let Some(ci) = t
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(name))
                {
                    t.rows[i][ci] = value.clone();
                } else {
                    return Err(crate::error::PgdbError::NotFound(format!(
                        "表 {table} 中不存在字段 {name}"
                    )));
                }
            }
            affected += 1;
        }
        Ok(affected)
    }

    fn insert(&self, table: &str, values: &[(String, SqlValue)]) -> Result<i64> {
        let mut st = self.state.write().unwrap();
        let t = st
            .tables
            .get_mut(table)
            .ok_or_else(|| crate::error::PgdbError::NotFound(format!("表 {table}")))?;
        let auto_slot = t.auto_column;
        let provided_auto = match auto_slot {
            Some(ci) => values
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(&t.columns[ci].name))
                .map(|(_, v)| v.to_i64().unwrap_or(0)),
            None => None,
        };
        let new_oid = provided_auto.unwrap_or(t.next_auto);
        if provided_auto.is_none() && auto_slot.is_some() {
            t.next_auto = t.next_auto.max(new_oid + 1);
        }
        let auto_assign = if auto_slot.is_some() && provided_auto.is_none() {
            Some(new_oid)
        } else {
            None
        };
        let row = t.build_row(values, auto_assign)?;
        t.rows.push(row);
        Ok(new_oid)
    }

    fn delete(&self, table: &str, filter: &Predicate) -> Result<u64> {
        let mut st = self.state.write().unwrap();
        let t = st
            .tables
            .get_mut(table)
            .ok_or_else(|| crate::error::PgdbError::NotFound(format!("表 {table}")))?;
        if matches!(filter, Predicate::All) {
            let n = t.rows.len() as u64;
            t.rows.clear();
            return Ok(n);
        }
        let mut kept = Vec::with_capacity(t.rows.len());
        let mut removed = 0u64;
        for i in 0..t.rows.len() {
            let row = t.to_data_row(i, &[]);
            if predicate_matches(filter, &row) {
                removed += 1;
            } else {
                kept.push(t.rows[i].clone());
            }
        }
        t.rows = kept;
        Ok(removed)
    }

    fn count(&self, table: &str, filter: &Predicate) -> Result<u64> {
        Ok(self.select(table, &[], filter)?.len() as u64)
    }

    fn flush(&self) -> Result<()> {
        // 纯内存后端，无需持久化。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datastore::Predicate;
    use crate::field::FieldType;

    fn sample_store() -> MirrorBackend {
        let b = MirrorBackend::new();
        b.create_table(
            "GDB_Items",
            vec![ColumnDef {
                name: "Name".into(),
                sql_type: "TEXT".into(),
                size: Some(100),
                scale: None,
                nullable: true,
                is_auto: false,
                kind: FieldType::String,
            }],
        )
        .unwrap();
        b.insert("GDB_Items", &[("Name".to_string(), SqlValue::Text("Roads".into()))])
            .unwrap();
        b
    }

    #[test]
    fn test_mirror_lookup() {
        let b = sample_store();
        assert_eq!(b.table_names().unwrap(), vec!["GDB_Items".to_string()]);
        let rows = b.select("GDB_Items", &[], &Predicate::All).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("Name").unwrap().to_display_string(),
            "Roads".to_string()
        );
    }

    #[test]
    fn test_insert_autoincrement_and_update_delete() {
        let b = MirrorBackend::new();
        b.create_table(
            "T",
            vec![
                ColumnDef {
                    name: "OBJECTID".into(),
                    sql_type: "LONG".into(),
                    size: None,
                    scale: None,
                    nullable: false,
                    is_auto: true,
                    kind: FieldType::Oid,
                },
                ColumnDef {
                    name: "NAME".into(),
                    sql_type: "TEXT".into(),
                    size: Some(20),
                    scale: None,
                    nullable: true,
                    is_auto: false,
                    kind: FieldType::String,
                },
            ],
        )
        .unwrap();
        let oid1 = b
            .insert("T", &[("NAME".into(), SqlValue::Text("a".into()))])
            .unwrap();
        let oid2 = b
            .insert("T", &[("NAME".into(), SqlValue::Text("b".into()))])
            .unwrap();
        assert_eq!((oid1, oid2), (1, 2));
        let n = b
            .update("T", &[("NAME".into(), SqlValue::Text("c".into()))], &Predicate::eq("OBJECTID", 1i64))
            .unwrap();
        assert_eq!(n, 1);
        let rows = b.select("T", &[], &Predicate::All).unwrap();
        assert_eq!(rows[0].get("NAME").unwrap().to_display_string(), "c");
        let d = b.delete("T", &Predicate::eq("OBJECTID", 2i64)).unwrap();
        assert_eq!(d, 1);
        assert_eq!(b.count("T", &Predicate::All).unwrap(), 1);
    }

    #[test]
    fn test_binary_column_roundtrip() {
        let b = MirrorBackend::new();
        b.create_table(
            "FC",
            vec![ColumnDef {
                name: "Shape".into(),
                sql_type: "LONGVARBINARY".into(),
                size: None,
                scale: None,
                nullable: true,
                is_auto: false,
                kind: FieldType::Blob,
            }],
        )
        .unwrap();
        let payload = vec![1u8, 2, 3, 0xFF];
        b.insert("FC", &[("Shape".into(), SqlValue::Binary(payload.clone()))])
            .unwrap();
        let rows = b.select("FC", &[], &Predicate::All).unwrap();
        assert_eq!(rows[0].get("Shape").unwrap().to_binary().unwrap(), &payload[..]);
    }
}
