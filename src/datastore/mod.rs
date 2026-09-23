//! 数据源抽象层。
//!
//! 所有 SQL 能力最终都由 [`SqlBackend`] 承担，目录/游标层不感知具体是
//! ODBC 连接还是本地镜像文件。
//!
//! 出于可移植性考虑，谓词被建模为结构化的 [`Predicate`]，而非拼接 SQL 字符串：
//! - ODBC 后端把它翻译成参数化 SQL（避免注入，且正确处理二进制）；
//! - 本地镜像后端直接在内存中求值（无需 SQL 引擎，便于在 Linux 下测试与演示）。

pub mod mirror;
#[cfg(feature = "odbc")]
pub mod odbc;

pub use crate::value::SqlValue;

use crate::error::Result;

/// 打开 ODBC 后端：`Windows + ACE/Jet 驱动` 可读写，`Linux + MDBTools` 为只读。
///
/// 未启用 `odbc` feature 时返回明确的错误，便于上层给出安装提示。
pub fn odbc_backend(
    path: &str,
    connection_string: Option<&str>,
) -> Result<Box<dyn SqlBackend>> {
    #[cfg(feature = "odbc")]
    {
        Ok(Box::new(odbc::OdbcBackend::connect(
            path,
            connection_string,
        )?))
    }
    #[cfg(not(feature = "odbc"))]
    {
        let _ = (path, connection_string);
        Err(crate::error::PgdbError::Unsupported(
            "未启用 'odbc' feature，无法直接打开 .mdb；请 cargo build --features odbc，或先导出镜像文件".into(),
        ))
    }
}
use crate::field::FieldType;

/// 列定义（来自数据源的系统编目）
#[derive(Debug, Clone)]
pub struct ColumnDef {
    /// 列名
    pub name: String,
    /// 数据源报告的类型名
    pub sql_type: String,
    /// 长度
    pub size: Option<usize>,
    /// 小数位
    pub scale: Option<usize>,
    /// 是否可为空
    pub nullable: bool,
    /// 是否自增列
    pub is_auto: bool,
    /// 映射后的字段类型
    pub kind: FieldType,
}

/// 一行数据（保留列顺序，同时支持按名/按序号取值）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DataTableRow {
    cols: Vec<String>,
    vals: Vec<SqlValue>,
}

impl DataTableRow {
    /// 由列名与值构造
    pub fn new(cols: Vec<String>, vals: Vec<SqlValue>) -> Result<Self> {
        if cols.len() != vals.len() {
            return Err(crate::error::PgdbError::Backend(format!(
                "列数与值数量不一致: {} vs {}",
                cols.len(),
                vals.len()
            )));
        }
        Ok(Self { cols, vals })
    }

    /// 列名列表
    pub fn columns(&self) -> &[String] {
        &self.cols
    }

    /// 值列表
    pub fn values(&self) -> &[SqlValue] {
        &self.vals
    }

    /// 解构为列名与值
    pub fn into_parts(self) -> (Vec<String>, Vec<SqlValue>) {
        (self.cols, self.vals)
    }

    /// 按列名取值（大小写不敏感）
    pub fn get(&self, name: &str) -> Option<&SqlValue> {
        self.cols
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .and_then(|i| self.vals.get(i))
    }

    /// 按下标取值
    pub fn get_index(&self, i: usize) -> Option<&SqlValue> {
        self.vals.get(i)
    }

    /// 转成（列 -> 值）有序序列
    pub fn pairs(&self) -> Vec<(String, SqlValue)> {
        self.cols
            .iter()
            .cloned()
            .zip(self.vals.iter().cloned())
            .collect()
    }

    /// 设置某列的值（不存在则追加）
    pub fn set(&mut self, name: &str, value: SqlValue) {
        if let Some(i) = self
            .cols
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
        {
            self.vals[i] = value;
        } else {
            self.cols.push(name.to_string());
            self.vals.push(value);
        }
    }
}

/// 结构化查询谓词
#[derive(Debug, Clone, Default)]
pub enum Predicate {
    /// 全部记录（默认）
    #[default]
    All,
    /// 无记录
    Nothing,
    /// 字段等于某个值（ODBC 下转换为参数化 `col = ?`）
    FieldEq {
        /// 列名
        field: String,
        /// 目标值
        value: SqlValue,
    },
    /// 多个谓词的与组合
    And(Vec<Predicate>),
    /// 原始 SQL 片段（仅 ODBC 后端支持，请自行保证安全与转义）
    Raw(String),
}

impl Predicate {
    /// 相等谓词
    pub fn eq<V: Into<SqlValue>>(field: impl Into<String>, value: V) -> Self {
        Predicate::FieldEq {
            field: field.into(),
            value: value.into(),
        }
    }

    /// 追加一个条件
    pub fn and(self, other: Predicate) -> Self {
        match self {
            Predicate::All => other,
            Predicate::And(mut v) => {
                v.push(other);
                Predicate::And(v)
            }
            other_head => Predicate::And(vec![other_head, other]),
        }
    }
}

/// 后端能力标记
#[derive(Debug, Clone, Copy, Default)]
pub struct BackendCapabilities {
    /// 是否支持执行任意 SQL（consider raw_query）
    pub raw_sql: bool,
    /// 是否支持参数化的二进制写入（Shape 字段更新依赖它）
    pub binary_parameter: bool,
    /// 是否只能通过十六进制字面量写二进制（部分 Jet 驱动的回退方案）
    pub hex_literal_binary: bool,
    /// 是否可写（MDBTools 驱动只读）
    pub writable: bool,
    /// 是否支持事务
    pub transaction: bool,
}

/// 数据源后端：一个 Personal Geodatabase 的底层读写通道。
pub trait SqlBackend: Send + Sync {
    /// 后端名称
    fn kind(&self) -> &'static str;

    /// 能力集合
    fn capabilities(&self) -> BackendCapabilities;

    /// 列出所有用户表与系统表名
    fn table_names(&self) -> Result<Vec<String>>;

    /// 指定表是否存在
    fn table_exists(&self, table: &str) -> Result<bool>;

    /// 读取列定义
    fn columns(&self, table: &str) -> Result<Vec<ColumnDef>>;

    /// 按谓词查询（columns 为空表示全部列）
    fn select(&self, table: &str, columns: &[String], filter: &Predicate) -> Result<Vec<DataTableRow>>;

    /// 更新，返回受影响行数
    fn update(&self, table: &str, sets: &[(String, SqlValue)], filter: &Predicate) -> Result<u64>;

    /// 插入一行，返回新行的 OBJECTID（后端负责生成自增主键）
    fn insert(&self, table: &str, values: &[(String, SqlValue)]) -> Result<i64>;

    /// 删除，返回受影响行数
    fn delete(&self, table: &str, filter: &Predicate) -> Result<u64>;

    /// 计数
    fn count(&self, table: &str, filter: &Predicate) -> Result<u64>;

    /// 执行原始 SQL（不支持时返回错误）
    fn raw_query(&self, sql: &str) -> Result<Vec<DataTableRow>> {
        let _ = sql;
        Err(crate::error::PgdbError::Unsupported(format!(
            "后端 {} 不支持原始 SQL 查询",
            self.kind()
        )))
    }

    /// 执行原始 SQL（不支持时返回错误）
    fn raw_execute(&self, sql: &str) -> Result<u64> {
        let _ = sql;
        Err(crate::error::PgdbError::Unsupported(format!(
            "后端 {} 不支持原始 SQL 执行",
            self.kind()
        )))
    }

    /// 开启事务，返回是否真正启用
    fn begin(&self) -> Result<bool> {
        Ok(false)
    }

    /// 提交事务
    fn commit(&self) -> Result<()> {
        Ok(())
    }

    /// 回滚事务
    fn rollback(&self) -> Result<()> {
        Ok(())
    }

    /// 把内存中的修改写回持久化介质。
    ///
    /// ODBC 后端每次 `update/insert/delete` 都直接落到数据库，因此这里无需操作；
    /// 文件型后端（如本地 JSON 镜像）在此处落盘。
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// 在内存中对结构化谓词求值（供没有 SQL 引擎的后端复用）
pub fn predicate_matches(filter: &Predicate, row: &DataTableRow) -> bool {
    match filter {
        Predicate::All => true,
        Predicate::Nothing => false,
        Predicate::FieldEq { field, value } => match row.get(field) {
            Some(v) => value_loose_eq(value, v),
            None => false,
        },
        Predicate::And(list) => list.iter().all(|p| predicate_matches(p, row)),
        Predicate::Raw(sql) => {
            log::warn!("本地镜像后端忽略原始 SQL 谓词: {sql}");
            true
        }
    }
}

/// 宽松比较：容忍 int/float/text 的差异（Access 中 INTEGER 与 DOUBLE 常混用）
fn value_loose_eq(a: &SqlValue, b: &SqlValue) -> bool {
    match (a, b) {
        (SqlValue::Null, SqlValue::Null) => true,
        (SqlValue::Bool(a), SqlValue::Bool(b)) => a == b,
        (SqlValue::Text(a), SqlValue::Text(b)) => a == b,
        (SqlValue::Binary(a), SqlValue::Binary(b)) => a == b,
        (SqlValue::Guid(a), SqlValue::Guid(b)) => a == b,
        (SqlValue::DateTime(a), SqlValue::DateTime(b)) => a == b,
        (a, b) if a.is_null() || b.is_null() => false,
        // 一侧为数值：统一用 f64 比较
        (a, b) if a.to_f64().is_some() && b.to_f64().is_some() => a.to_f64() == b.to_f64(),
        (SqlValue::Text(s), other) | (other, SqlValue::Text(s)) => other.to_display_string() == *s,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_row_lookup() {
        let row = DataTableRow::new(
            vec!["OID".into(), "NAME".into()],
            vec![SqlValue::I32(1), SqlValue::Text("北京".into())],
        )
        .unwrap();
        assert_eq!(row.get("name").unwrap().to_display_string(), "北京");
        assert!(predicate_matches(&Predicate::eq("NAME", "北京"), &row));
        assert!(!predicate_matches(&Predicate::eq("OID", 2i32), &row));
        // 数值类型混用
        assert!(predicate_matches(&Predicate::eq("OID", 1i64), &row));
        assert!(predicate_matches(&Predicate::eq("OID", 1.0f64), &row));
    }
}
