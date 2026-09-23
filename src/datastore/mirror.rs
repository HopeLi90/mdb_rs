//! 本地镜像后端：把 Personal Geodatabase 的表结构 + 数据完整镜像为单个 JSON 文件。
//!
//! 用途：
//! 1. 在没有 ACE/Jet ODBC 驱动的平台（Linux/macOS）上，仍然可以完整演练
//!    目录遍历、要素更新、Shape 二进制读写等全部逻辑；
//! 2. 单元测试与集成测试的确定性数据源；
//! 3. mdb 内容的离线快照（`pgdb-cli export`）。
//!
//! 镜像是"数据副本"，并非 Access 文件格式本身。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::datastore::predicate_matches;
use crate::error::{PgdbError, Result};
use crate::field::FieldType;
use crate::value::SqlValue;

use super::{BackendCapabilities, ColumnDef, DataTableRow, Predicate, SqlBackend};

/// 序列化用的列表示
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnRepr {
    name: String,
    sql_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scale: Option<usize>,
    #[serde(default = "default_true")]
    nullable: bool,
    #[serde(default)]
    is_auto: bool,
}

fn default_true() -> bool {
    true
}

/// 序列化用的值表示（二进制列以十六进制字符串存储，控制文件体积）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum ValueRepr {
    Null,
    Bool(bool),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Text(String),
    Decimal(String),
    DateTime(String),
    Binary(String),
    Guid(String),
}

impl From<&SqlValue> for ValueRepr {
    fn from(v: &SqlValue) -> Self {
        match v {
            SqlValue::Null => ValueRepr::Null,
            SqlValue::Bool(b) => ValueRepr::Bool(*b),
            SqlValue::I16(v) => ValueRepr::I16(*v),
            SqlValue::I32(v) => ValueRepr::I32(*v),
            SqlValue::I64(v) => ValueRepr::I64(*v),
            SqlValue::F32(v) => ValueRepr::F32(*v),
            SqlValue::F64(v) => ValueRepr::F64(*v),
            SqlValue::Text(s) => ValueRepr::Text(s.clone()),
            SqlValue::Decimal(s) => ValueRepr::Decimal(s.clone()),
            SqlValue::DateTime(d) => ValueRepr::DateTime(d.to_rfc3339()),
            SqlValue::Binary(b) => ValueRepr::Binary(hex::encode(b)),
            SqlValue::Guid(s) => ValueRepr::Guid(s.clone()),
        }
    }
}

impl TryFrom<ValueRepr> for SqlValue {
    type Error = PgdbError;
    fn try_from(v: ValueRepr) -> Result<Self> {
        Ok(match v {
            ValueRepr::Null => SqlValue::Null,
            ValueRepr::Bool(b) => SqlValue::Bool(b),
            ValueRepr::I16(v) => SqlValue::I16(v),
            ValueRepr::I32(v) => SqlValue::I32(v),
            ValueRepr::I64(v) => SqlValue::I64(v),
            ValueRepr::F32(v) => SqlValue::F32(v),
            ValueRepr::F64(v) => SqlValue::F64(v),
            ValueRepr::Text(s) => SqlValue::Text(s),
            ValueRepr::Decimal(s) => SqlValue::Decimal(s),
            ValueRepr::DateTime(s) => SqlValue::DateTime(
                chrono::DateTime::parse_from_rfc3339(&s)
                    .map_err(|e| PgdbError::Serialize(e.to_string()))?
                    .with_timezone(&chrono::Utc),
            ),
            ValueRepr::Binary(h) => SqlValue::Binary(
                hex::decode(h).map_err(|e| PgdbError::Serialize(e.to_string()))?,
            ),
            ValueRepr::Guid(s) => SqlValue::Guid(s),
        })
    }
}

/// 序列化用的表表示
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRepr {
    name: String,
    #[serde(default)]
    columns: Vec<ColumnRepr>,
    /// 每行一组值，与 columns 顺序一致
    #[serde(default)]
    rows: Vec<Vec<ValueRepr>>,
    /// 自增列的下一位取值（缺省时由现有数据推导）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_auto: Option<i64>,
}

/// 镜像文件整体结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorFile {
    /// 文件格式版本
    pub version: u32,
    /// 原始 mdb 路径（仅记录用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// 全部表
    pub tables: Vec<TableRepr>,
}

/// 内存中的表
#[derive(Debug, Clone)]
struct Table {
    name: String,
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
        for col in &self.columns {
            if let Some((_, v)) = values.iter().find(|(n, _)| n.eq_ignore_ascii_case(&col.name)) {
                out.push(v.clone());
                continue;
            }
            let auto_here = Some(true) == self.columns.get(out.len()).map(|c| c.is_auto);
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

/// 本地镜像后端
pub struct MirrorBackend {
    state: RwLock<MirrorState>,
    source_hint: Option<String>,
    /// 镜像文件路径（存在时 `flush()` 会写回该文件）
    file_path: Option<PathBuf>,
}

#[derive(Default)]
struct MirrorState {
    order: Vec<String>,
    tables: HashMap<String, Table>,
}

impl MirrorBackend {
    /// 创建空镜像
    pub fn new() -> Self {
        Self {
            state: RwLock::new(MirrorState::default()),
            source_hint: None,
            file_path: None,
        }
    }

    /// 镜像文件路径（若由文件加载）
    pub fn path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    /// 把当前内存状态写回来源文件；没有来源文件时返回 None
    pub fn save(&self) -> Result<Option<std::path::PathBuf>> {
        let Some(path) = self.file_path.clone() else {
            return Ok(None);
        };
        self.save_file(&path)?;
        Ok(Some(path))
    }

    /// 从 JSON 文件加载镜像
    pub fn open_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| PgdbError::io(format!("读取镜像 {}", path.as_ref().display()), e))?;
        let mut backend = Self::from_json(&text)?;
        backend.file_path = Some(path.as_ref().to_path_buf());
        Ok(backend)
    }

    /// 由 JSON 文本构造
    pub fn from_json(text: &str) -> Result<Self> {
        let file: MirrorFile =
            serde_json::from_str(text).map_err(|e| PgdbError::Serialize(e.to_string()))?;
        let backend = Self {
            state: RwLock::new(MirrorState::default()),
            source_hint: file.source.clone(),
            file_path: None,
        };
        {
            let mut st = backend.state.write().unwrap();
            for t in file.tables {
                let columns: Vec<ColumnDef> = t
                    .columns
                    .iter()
                    .map(|c| ColumnDef {
                        name: c.name.clone(),
                        sql_type: c.sql_type.clone(),
                        size: c.size,
                        scale: c.scale,
                        nullable: c.nullable,
                        is_auto: c.is_auto,
                        kind: FieldType::from_sql_type(&c.sql_type),
                    })
                    .collect();
                let auto_column = columns.iter().position(|c| c.is_auto);
                let mut rows: Vec<Vec<SqlValue>> = Vec::with_capacity(t.rows.len());
                for r in &t.rows {
                    let mut vals = Vec::with_capacity(r.len());
                    for v in r {
                        vals.push(SqlValue::try_from(v.clone())?);
                    }
                    rows.push(vals);
                }
                // 推导自增值
                let next_auto = t.next_auto.unwrap_or_else(|| match auto_column {
                    Some(ci) => rows
                        .iter()
                        .filter_map(|r| r.get(ci).map(|v| v.to_i64().unwrap_or(0)))
                        .max()
                        .map(|m| m + 1)
                        .unwrap_or(1),
                    None => 1,
                });
                st.order.push(t.name.clone());
                st.tables.insert(
                    t.name.clone(),
                    Table {
                        name: t.name.clone(),
                        columns,
                        rows,
                        auto_column,
                        next_auto,
                    },
                );
            }
        }
        Ok(backend)
    }

    /// 序列化为 JSON 文本
    pub fn to_json(&self) -> Result<String> {
        let st = self.state.read().unwrap();
        let mut tables = Vec::with_capacity(st.order.len());
        for name in &st.order {
            let t = &st.tables[name];
            tables.push(TableRepr {
                name: t.name.clone(),
                columns: t
                    .columns
                    .iter()
                    .map(|c| ColumnRepr {
                        name: c.name.clone(),
                        sql_type: c.sql_type.clone(),
                        size: c.size,
                        scale: c.scale,
                        nullable: c.nullable,
                        is_auto: c.is_auto,
                    })
                    .collect(),
                rows: t
                    .rows
                    .iter()
                    .map(|r| r.iter().map(ValueRepr::from).collect())
                    .collect(),
                next_auto: Some(t.next_auto),
            });
        }
        let file = MirrorFile {
            version: 1,
            source: self.source_hint.clone(),
            tables,
        };
        serde_json::to_string_pretty(&file).map_err(|e| PgdbError::Serialize(e.to_string()))
    }

    /// 保存到 JSON 文件
    pub fn save_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let json = self.to_json()?;
        std::fs::write(path.as_ref(), json)
            .map_err(|e| PgdbError::io(format!("写入镜像 {}", path.as_ref().display()), e))
    }

    /// 新建一张表（供测试与 mdb->镜像 导出使用）
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
                name: name.to_string(),
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
            .ok_or_else(|| PgdbError::NotFound(format!("表 {table}")))?;
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
        "mirror"
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
            .ok_or_else(|| PgdbError::NotFound(format!("表 {table}")))?;
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
                    return Err(PgdbError::NotFound(format!("表 {table} 中不存在字段 {name}")));
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
            .ok_or_else(|| PgdbError::NotFound(format!("表 {table}")))?;
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
            .ok_or_else(|| PgdbError::NotFound(format!("表 {table}")))?;
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
        if self.file_path.is_some() {
            self.save()?;
        }
        Ok(())
    }
}

mod hex {
    /// 字节转十六进制字符串
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2 + 2);
        s.push_str("0x");
        for b in bytes {
            s.push_str(&format!("{b:02X}"));
        }
        s
    }

    /// 十六进制字符串转字节
    pub fn decode(s: String) -> Result<Vec<u8>, String> {
        let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
        if s.len() % 2 != 0 {
            return Err("十六进制长度不是偶数".to_string());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_mirror_roundtrip_file() {
        let b = sample_store();
        let json = b.to_json().unwrap();
        let b2 = MirrorBackend::from_json(&json).unwrap();
        assert_eq!(b2.table_names().unwrap(), vec!["GDB_Items".to_string()]);
        let rows = b2.select("GDB_Items", &[], &Predicate::All).unwrap();
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
        let json = b.to_json().unwrap();
        let b2 = MirrorBackend::from_json(&json).unwrap();
        let rows = b2.select("FC", &[], &Predicate::All).unwrap();
        assert_eq!(rows[0].get("Shape").unwrap().to_binary().unwrap(), &payload[..]);
    }
}
