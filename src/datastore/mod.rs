//! 数据源抽象层。
//!
//! 所有 SQL 能力最终都由 [`SqlBackend`] 承担，目录/游标层不感知具体是
//! ODBC 连接还是纯 Rust 的 jetdb 解析。
//!
//! 出于可移植性考虑，谓词被建模为结构化的 [`Predicate`]，而非拼接 SQL 字符串：
//! - ODBC 后端把它翻译成参数化 SQL（避免注入，且正确处理二进制）；
//! - 内存后端直接在内存中求值（无需 SQL 引擎，便于在 Linux 下测试与演示）。

pub mod jetdb_backend;
pub mod mirror;
pub mod odbc;

pub use crate::value::SqlValue;

use crate::error::Result;

/// 工作空间访问权限（对应 ArcObjects 中「工作空间是否可编辑」的概念）。
///
/// 该枚举是**运行时**选择后端的唯一开关：调用 [`open_backend`] 时传入即可，
/// 无需在编译期用 feature 挑选，因此同一个可执行文件同时内置两种解析能力。
///
/// - [`AccessMode::ReadOnly`]：选用纯 Rust 的 [`jetdb_backend::JetdbBackend`]，
///   无需任何驱动、跨平台，但只能查询。
/// - [`AccessMode::ReadWrite`]：选用 [`odbc::OdbcBackend`]，经由 ODBC 驱动访问，
///   支持属性与几何的写入（需要安装与程序位数匹配的 Access / ACE 驱动）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessMode {
    /// 只读（jetdb 后端）
    #[default]
    ReadOnly,
    /// 读写（ODBC 后端）
    ReadWrite,
}

impl AccessMode {
    /// 该权限下选用的后端名称（便于日志与诊断输出）
    pub fn backend_name(self) -> &'static str {
        match self {
            AccessMode::ReadOnly => "jetdb",
            AccessMode::ReadWrite => "odbc",
        }
    }

    /// 该权限是否为只读
    pub fn is_read_only(self) -> bool {
        matches!(self, AccessMode::ReadOnly)
    }
}

impl std::str::FromStr for AccessMode {
    type Err = crate::error::PgdbError;

    /// 解析用户输入的权限名（`readonly` / `read-only` / `rw` 等，大小写不敏感）
    fn from_str(s: &str) -> Result<Self> {
        let key = s.trim().to_ascii_lowercase().replace(['-', '_'], "");
        match key.as_str() {
            "readonly" | "ro" | "read" | "r" => Ok(AccessMode::ReadOnly),
            "readwrite" | "rw" | "write" | "w" => Ok(AccessMode::ReadWrite),
            other => Err(crate::error::PgdbError::InvalidArgument(format!(
                "无法识别的读写权限 `{other}`；可选值：readonly / readwrite"
            ))),
        }
    }
}

impl std::fmt::Display for AccessMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessMode::ReadOnly => f.write_str("readonly"),
            AccessMode::ReadWrite => f.write_str("readwrite"),
        }
    }
}

/// 按访问权限打开对应的后端。
///
/// 这是「权限 -> 解析方式」的唯一分派点：
/// - [`AccessMode::ReadOnly`] -> [`jetdb_backend::JetdbBackend`]（纯 Rust，无驱动）
/// - [`AccessMode::ReadWrite`] -> [`odbc::OdbcBackend`]（需 ODBC 驱动）
pub fn open_backend(
    path: &str,
    mode: AccessMode,
    connection_string: Option<&str>,
) -> Result<Box<dyn SqlBackend>> {
    match mode {
        AccessMode::ReadOnly => Ok(Box::new(jetdb_backend::JetdbBackend::open(path)?)),
        AccessMode::ReadWrite => Ok(Box::new(odbc::OdbcBackend::connect(
            path,
            connection_string,
        )?)),
    }
}

/// 打开 ODBC 后端：`Windows + ACE/Jet 驱动` 可读写，`Linux + MDBTools` 为只读。
pub fn odbc_backend(
    path: &str,
    connection_string: Option<&str>,
) -> Result<Box<dyn SqlBackend>> {
    open_backend(path, AccessMode::ReadWrite, connection_string)
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
    /// 纯内存后端同样无需操作。仅作为统一接口的一部分存在。
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// 在内存中对结构化谓词求值（供没有 SQL 引擎的后端复用）。
///
/// 对 [`Predicate::Raw`]（`--where` 传入的原始子句）会尝试**在内存里解释**它，
/// 见 [`eval_where_clause`]。这很重要：若不解释就直接返回 `true`，
/// 只读后端上的 `--where` 会被静默忽略并返回全表数据，属于危险的「看起来成功」。
pub fn predicate_matches(filter: &Predicate, row: &DataTableRow) -> bool {
    match filter {
        Predicate::All => true,
        Predicate::Nothing => false,
        Predicate::FieldEq { field, value } => match row.get(field) {
            Some(v) => value_loose_eq(value, v),
            None => false,
        },
        Predicate::And(list) => list.iter().all(|p| predicate_matches(p, row)),
        Predicate::Raw(sql) => eval_where_clause(sql, row),
    }
}

/// 比较运算符
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Like,
}

/// 在内存中解释一条简单 `WHERE` 子句。
///
/// 支持 `A AND B` 连接，以及单个条件 `字段 运算符 值`，其中运算符为
/// `= <> != > >= < <= LIKE`。字段名可用 `[]` 包裹（Access 习惯），
/// 值可加单引号；未加引号的裸值会先按数字解析，失败则按字符串比较。
///
/// **无法解析时返回 `false`**（即不匹配）而不是 `true`：
/// 宁可过滤掉全部行让用户察觉条件有问题，也不要静默返回全表数据。
fn eval_where_clause(clause: &str, row: &DataTableRow) -> bool {
    let trimmed = clause.trim();
    if trimmed.is_empty() {
        return true;
    }
    // 支持 AND：整体为与关系（OR 不做解释，避免与 SQL 语义不符时误判）
    if let Some(parts) = split_top_level(trimmed, " AND ") {
        if parts.len() > 1 {
            return parts.iter().all(|p| eval_where_clause(p, row));
        }
    }
    let Some((op, op_len)) = find_operator(trimmed) else {
        log::warn!("只读后端无法在内存中解释该 WHERE 子句，按「不匹配」处理: {trimmed}");
        return false;
    };
    // `op_len` 是运算符结束位置（即值开始处），字段名截止到运算符起始处。
    let (field_raw, value_raw) = trimmed.split_at(op_len);
    let field = strip_wrapping(field_raw[..field_raw.len() - op.token_len()].trim());
    let value_lit = strip_quotes(value_raw.trim());

    let Some(cell) = row.get(&field) else {
        return false;
    };
    let expected = parse_literal(&value_lit);
    compare_cell(cell, &expected, op)
}

impl CmpOp {
    /// 运算符字面量长度，用于从子句中切分字段名与值
    fn token_len(self) -> usize {
        match self {
            CmpOp::Eq | CmpOp::Gt | CmpOp::Lt => 1,
            CmpOp::Ne | CmpOp::Ge | CmpOp::Le => 2,
            CmpOp::Like => 4,
        }
    }
}

/// 找到子句中出现的运算符（**长运算符优先**，避免把 `>=` 误判成 `>`）。
fn find_operator(s: &str) -> Option<(CmpOp, usize)> {
    // 按长度降序扫描，保证 `>=` / `<=` / `<>` / `LIKE` 先于单字符运算符命中
    let candidates: [(CmpOp, &str); 7] = [
        (CmpOp::Like, "LIKE"),
        (CmpOp::Ge, ">="),
        (CmpOp::Le, "<="),
        (CmpOp::Ne, "<>"),
        (CmpOp::Ne, "!="),
        (CmpOp::Eq, "="),
        (CmpOp::Gt, ">"),
    ];
    let upper = s.to_uppercase();
    let mut best: Option<(CmpOp, usize)> = None;
    for (op, tok) in candidates {
        let tok_upper = tok.to_uppercase();
        // LIKE 需要词边界，否则会把字段名里的字母也匹配上
        let pos = if op == CmpOp::Like {
            find_word(&upper, &tok_upper)
        } else {
            upper.find(&tok_upper)
        };
        if let Some(p) = pos {
            let end = p + tok.len();
            // 单字符 `<` / `>` 若紧跟 `=` 则交给双字符分支
            if matches!(op, CmpOp::Gt) && upper[end..].starts_with('=') {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, cur)) => tok.len() > cur,
            };
            if better {
                best = Some((op, end));
            }
        }
    }
    if best.is_none() {
        // 单独处理 `<`
        if let Some(p) = s.find('<') {
            return Some((CmpOp::Lt, p + 1));
        }
    }
    best
}

/// 查找独立单词（前后非字母数字）
fn find_word(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(needle) {
        let p = from + rel;
        let before_ok = p == 0 || !bytes[p - 1].is_ascii_alphanumeric();
        let after = p + needle.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(p);
        }
        from = p + 1;
        if from >= haystack.len() {
            break;
        }
    }
    None
}

/// 在顶层（忽略引号内与括号内）按分隔符切分
fn split_top_level(s: &str, sep: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quote = false;
    let mut start = 0usize;
    let bytes = s.as_bytes();
    let sep_bytes = sep.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '\'' {
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if !in_quote {
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
            } else if depth == 0 && bytes[i..].starts_with(sep_bytes) {
                parts.push(s[start..i].to_string());
                i += sep.len();
                start = i;
                continue;
            }
        }
        i += 1;
    }
    if parts.is_empty() {
        None
    } else {
        parts.push(s[start..].to_string());
        Some(parts)
    }
}

/// 去掉 `[...]` 或 `"..."` 包裹的标识符
fn strip_wrapping(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 {
        let b = t.as_bytes();
        if (b[0] == b'[' && b[t.len() - 1] == b']') || (b[0] == b'"' && b[t.len() - 1] == b'"') {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

/// 去掉单引号并解转义（SQL 中用两个单引号表示一个单引号）
fn strip_quotes(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return t[1..t.len() - 1].replace("''", "'");
    }
    t.to_string()
}

/// 把字面量解析为 [`SqlValue`]：优先数字，其次布尔，否则字符串
fn parse_literal(s: &str) -> SqlValue {
    let t = s.trim();
    if t.is_empty() {
        return SqlValue::Null;
    }
    if t.eq_ignore_ascii_case("null") {
        return SqlValue::Null;
    }
    if let Ok(i) = t.parse::<i64>() {
        return SqlValue::I64(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        return SqlValue::F64(f);
    }
    if t.eq_ignore_ascii_case("true") {
        return SqlValue::Bool(true);
    }
    if t.eq_ignore_ascii_case("false") {
        return SqlValue::Bool(false);
    }
    SqlValue::Text(t.to_string())
}

/// 按运算符比较单元格与期望值
fn compare_cell(cell: &SqlValue, expected: &SqlValue, op: CmpOp) -> bool {
    match op {
        CmpOp::Eq => value_loose_eq(cell, expected),
        CmpOp::Ne => !value_loose_eq(cell, expected),
        CmpOp::Like => match (cell, expected) {
            (SqlValue::Text(hay), SqlValue::Text(pat)) => like_match(hay, pat),
            (other, SqlValue::Text(pat)) => like_match(&other.to_display_string(), pat),
            _ => false,
        },
        CmpOp::Gt | CmpOp::Ge | CmpOp::Lt | CmpOp::Le => {
            // 数值优先，退回字符串比较
            if let (Some(a), Some(b)) = (cell.to_f64(), expected.to_f64()) {
                return match op {
                    CmpOp::Gt => a > b,
                    CmpOp::Ge => a >= b,
                    CmpOp::Lt => a < b,
                    CmpOp::Le => a <= b,
                    _ => unreachable!(),
                };
            }
            let a = cell.to_display_string();
            let b = expected.to_display_string();
            match op {
                CmpOp::Gt => a > b,
                CmpOp::Ge => a >= b,
                CmpOp::Lt => a < b,
                CmpOp::Le => a <= b,
                _ => unreachable!(),
            }
        }
    }
}

/// SQL `LIKE` 匹配（`%` 任意串，`_` 任意单字符），大小写不敏感
fn like_match(text: &str, pattern: &str) -> bool {
    fn rec(t: &[char], p: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('%', rest)) => {
                (0..=t.len()).any(|i| rec(&t[i..], rest))
            }
            Some(('_', rest)) => !t.is_empty() && rec(&t[1..], rest),
            Some((c, rest)) => match t.split_first() {
                Some((tc, trest)) if tc.eq_ignore_ascii_case(c) => rec(trest, rest),
                _ => false,
            },
        }
    }
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    rec(&t, &p)
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

    fn sample_row() -> DataTableRow {
        DataTableRow::new(
            vec!["OBJECTID".into(), "名称".into(), "AREA".into()],
            vec![
                SqlValue::I32(7),
                SqlValue::Text("界址点".into()),
                SqlValue::F64(12.5),
            ],
        )
        .unwrap()
    }

    #[test]
    fn raw_where_is_evaluated_in_memory() {
        let r = sample_row();
        // 等值（数字、带引号字符串、中文）
        assert!(predicate_matches(&Predicate::Raw("OBJECTID = 7".into()), &r));
        assert!(!predicate_matches(&Predicate::Raw("OBJECTID = 8".into()), &r));
        assert!(predicate_matches(&Predicate::Raw("名称 = '界址点'".into()), &r));
        // 方括号包裹的字段名（Access 习惯）
        assert!(predicate_matches(&Predicate::Raw("[OBJECTID] = 7".into()), &r));
        // 不等 / 比较
        assert!(predicate_matches(&Predicate::Raw("OBJECTID <> 8".into()), &r));
        assert!(predicate_matches(&Predicate::Raw("OBJECTID >= 7".into()), &r));
        assert!(predicate_matches(&Predicate::Raw("AREA > 12".into()), &r));
        assert!(!predicate_matches(&Predicate::Raw("AREA < 12".into()), &r));
        // LIKE
        assert!(predicate_matches(&Predicate::Raw("名称 LIKE '界%'".into()), &r));
        assert!(!predicate_matches(&Predicate::Raw("名称 LIKE '址%'".into()), &r));
        // AND 组合
        assert!(predicate_matches(
            &Predicate::Raw("OBJECTID = 7 AND AREA > 10".into()),
            &r
        ));
        assert!(!predicate_matches(
            &Predicate::Raw("OBJECTID = 7 AND AREA > 100".into()),
            &r
        ));
        // 字段不存在 -> 不匹配
        assert!(!predicate_matches(&Predicate::Raw("NOSUCH = 1".into()), &r));
        // 无法解析的子句：必须「不匹配」，绝不能静默放行全表
        assert!(!predicate_matches(
            &Predicate::Raw("完全不是条件的垃圾输入".into()),
            &r
        ));
    }

    #[test]
    fn like_handles_wildcards() {
        assert!(like_match("界址点", "界%"));
        assert!(like_match("界址点", "%址%"));
        assert!(like_match("abc", "a_c"));
        assert!(like_match("ABC", "abc")); // 大小写不敏感
        assert!(!like_match("abc", "a_d"));
        assert!(like_match("anything", "%"));
    }
}
