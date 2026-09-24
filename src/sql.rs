//! Jet/Access SQL 构造工具：标识符引用、字面量转义、谓词翻译。
//!
//! Personal Geodatabase 使用 Jet SQL 方言：
//! - 标识符用方括号 `[My Table]`（表名含空格或以数字开头时必须）
//! - 文本用单引号，内部引号双写转义
//! - 日期用 `#2024-01-01 12:00:00#`
//! - 二进制可用十六进制字面量 `0x0102FF`
//!
//! 所有外部输入在此统一转义，避免 SQL 注入。

use crate::datastore::{DataTableRow, Predicate, SqlValue};
use crate::error::{PgdbError, Result};
use chrono::{DateTime, Utc};

/// 引用标识符：含空格、保留字、非字母开头时加方括号
pub fn quote_ident(name: &str) -> String {
    let needs_quote = name
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '_'))
        || name
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
    if needs_quote {
        format!("[{}]", name.replace(']', "]]"))
    } else {
        name.to_string()
    }
}

/// 将值转为 Jet SQL 字面量（用于无法参数化的场景）
pub fn literal(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => "NULL".into(),
        SqlValue::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        SqlValue::I16(v) => v.to_string(),
        SqlValue::I32(v) => v.to_string(),
        SqlValue::I64(v) => v.to_string(),
        SqlValue::F32(v) => v.to_string(),
        SqlValue::F64(v) => format!("{v:?}"),
        SqlValue::Text(s) => format!("'{}'", escape_text(s)),
        SqlValue::Decimal(s) => s.clone(),
        SqlValue::DateTime(d) => format!("#{}#", format_datetime(d)),
        SqlValue::Binary(b) => hex_literal(b),
        SqlValue::Guid(s) => format!("'{}'", escape_text(s)),
    }
}

/// 文本转义：单引号双写
pub fn escape_text(s: &str) -> String {
    s.replace('\'', "''")
}

/// 日期格式化为 Jet 可接受的 `#yyyy-mm-dd HH:MM:SS#`
pub fn format_datetime(d: &DateTime<Utc>) -> String {
    d.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 二进制十六进制字面量
pub fn hex_literal(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "NULL".into();
    }
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02X}"));
    }
    format!("0x{s}")
}

/// SELECT 语句构造
pub fn select_sql(table: &str, columns: &[String], filter: Option<&Predicate>) -> Result<String> {
    let cols = if columns.is_empty() {
        "*".to_string()
    } else {
        columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut sql = format!("SELECT {cols} FROM {}", quote_ident(table));
    if let Some(f) = filter {
        if let Some(w) = where_clause(f)? {
            sql.push_str(&format!(" WHERE {w}"));
        }
    }
    Ok(sql)
}

/// INSERT 语句构造（列顺序与 sets 一致）
pub fn insert_sql(table: &str, cols: &[String], values_literal: Vec<String>) -> String {
    let cols_sql = cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote_ident(table),
        cols_sql,
        values_literal.join(", ")
    )
}

/// UPDATE 语句构造
pub fn update_sql(
    table: &str,
    sets: &[(String, SqlValue)],
    filter: &Predicate,
) -> Result<String> {
    if sets.is_empty() {
        return Err(PgdbError::Sql("UPDATE 语句没有要设置的字段".into()));
    }
    let set_sql = sets
        .iter()
        .map(|(c, v)| format!("{} = {}", quote_ident(c), literal(v)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("UPDATE {} SET {set_sql}", quote_ident(table));
    if let Some(w) = where_clause(filter)? {
        sql.push_str(&format!(" WHERE {w}"));
    }
    Ok(sql)
}

/// DELETE 语句构造
pub fn delete_sql(table: &str, filter: &Predicate) -> Result<String> {
    let mut sql = format!("DELETE FROM {}", quote_ident(table));
    if let Some(w) = where_clause(filter)? {
        sql.push_str(&format!(" WHERE {w}"));
    }
    Ok(sql)
}

/// COUNT 语句构造
pub fn count_sql(table: &str, filter: &Predicate) -> Result<String> {
    // 注意：部分轻量驱动（如 mdbtools）的 SQL 解析器不支持 `AS <别名>`，
    // 因此这里不写别名，读取时统一按列序（第 0 列）取值。
    let mut sql = format!("SELECT COUNT(*) FROM {}", quote_ident(table));
    if let Some(w) = where_clause(filter)? {
        sql.push_str(&format!(" WHERE {w}"));
    }
    Ok(sql)
}

/// 谓词 -> WHERE 子句（不含 WHERE 关键字）。返回 None 表示无条件。
pub fn where_clause(filter: &Predicate) -> Result<Option<String>> {
    match filter {
        Predicate::All => Ok(None),
        Predicate::Nothing => Ok(Some("1 = 0".to_string())),
        Predicate::FieldEq { field, value } => {
            if value.is_null() {
                Ok(Some(format!("{} IS NULL", quote_ident(field))))
            } else {
                Ok(Some(format!(
                    "{} = {}",
                    quote_ident(field),
                    literal(value)
                )))
            }
        }
        Predicate::In { field, values } => {
            if values.is_empty() {
                // 空 IN 集合等价于永假，避免生成 `IN ()` 语法错误
                Ok(Some("1 = 0".to_string()))
            } else {
                let list = values
                    .iter()
                    .map(literal)
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(Some(format!("{} IN ({})", quote_ident(field), list)))
            }
        }
        Predicate::And(list) => {
            let parts: Result<Vec<String>> = list
                .iter()
                .filter_map(|p| match where_clause(p) {
                    Ok(Some(s)) => Some(Ok(s)),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect();
            let parts = parts?;
            if parts.is_empty() {
                Ok(None)
            } else {
                Ok(Some(format!("({})", parts.join(" AND "))))
            }
        }
        Predicate::Raw(raw) => Ok(Some(raw.clone())),
    }
}

/// 从查询结果行中获取计数（按列序第 0 列；兼容列名 CNT）
pub fn read_count(rows: &[DataTableRow]) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let v = rows[0]
        .get_index(0)
        .or_else(|| rows[0].get("CNT"))
        .ok_or_else(|| PgdbError::Sql("COUNT 结果为空".into()))?;
    v.to_i64()
        .map(|n| n as u64)
        .ok_or_else(|| PgdbError::Sql("COUNT 结果无法解析".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quoting() {
        assert_eq!(quote_ident("Roads"), "Roads");
        assert_eq!(quote_ident("My Table"), "[My Table]");
        assert_eq!(quote_ident("2023Data"), "[2023Data]");
    }

    #[test]
    fn test_literals() {
        assert_eq!(literal(&SqlValue::Text("O'Brien".into())), "'O''Brien'");
        assert_eq!(
            literal(&SqlValue::Binary(vec![0x01, 0xAB])),
            "0x01AB".to_string()
        );
        assert_eq!(literal(&SqlValue::Null), "NULL");
    }

    #[test]
    fn test_select_and_where() {
        let sql = select_sql(
            "Roads",
            &["OBJECTID".into(), "Shape".into()],
            Some(&Predicate::eq("OBJECTID", 3i32)),
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT OBJECTID, Shape FROM Roads WHERE OBJECTID = 3".to_string()
        );

        let none = select_sql("Roads", &[], None).unwrap();
        assert_eq!(none, "SELECT * FROM Roads");
    }

    #[test]
    fn test_update_sql() {
        let sql = update_sql(
            "Roads",
            &[("NAME".into(), SqlValue::Text("G1".into()))],
            &Predicate::eq("OBJECTID", 2i64),
        )
        .unwrap();
        assert_eq!(sql, "UPDATE Roads SET NAME = 'G1' WHERE OBJECTID = 2");
    }
}
