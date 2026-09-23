//! 字段值与 SQL 值的表示：介于 Access(Jet) 列类型与 Rust 类型之间的桥梁。
//!
//! 对应 ArcObjects 中 `IRow::Value` 的 variant 语义。

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

impl From<bool> for SqlValue {
    fn from(v: bool) -> Self {
        SqlValue::Bool(v)
    }
}

impl From<i16> for SqlValue {
    fn from(v: i16) -> Self {
        SqlValue::I16(v)
    }
}

impl From<i32> for SqlValue {
    fn from(v: i32) -> Self {
        SqlValue::I32(v)
    }
}

impl From<i64> for SqlValue {
    fn from(v: i64) -> Self {
        SqlValue::I64(v)
    }
}

impl From<f32> for SqlValue {
    fn from(v: f32) -> Self {
        SqlValue::F32(v)
    }
}

impl From<f64> for SqlValue {
    fn from(v: f64) -> Self {
        SqlValue::F64(v)
    }
}

impl From<String> for SqlValue {
    fn from(v: String) -> Self {
        SqlValue::Text(v)
    }
}

impl From<&str> for SqlValue {
    fn from(v: &str) -> Self {
        SqlValue::Text(v.to_string())
    }
}

impl From<Vec<u8>> for SqlValue {
    fn from(v: Vec<u8>) -> Self {
        SqlValue::Binary(v)
    }
}

/// 传输层使用的 SQL 值
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    /// NULL
    Null,
    /// 布尔（Access Yes/No）
    Bool(bool),
    /// 16 位整数
    I16(i16),
    /// 32 位整数（Access Long Integer）
    I32(i32),
    /// 64 位整数
    I64(i64),
    /// 单精度
    F32(f32),
    /// 双精度
    F64(f64),
    /// 文本
    Text(String),
    /// 十进制/货币，保留字符串精度
    Decimal(String),
    /// 日期时间
    DateTime(DateTime<Utc>),
    /// 二进制（OLE Object，几何列使用）
    Binary(Vec<u8>),
    /// GUID
    Guid(String),
}

impl SqlValue {
    /// 是否为 NULL
    pub fn is_null(&self) -> bool {
        matches!(self, SqlValue::Null)
    }

    /// 转为 i64
    pub fn to_i64(&self) -> Option<i64> {
        match self {
            SqlValue::Null => None,
            SqlValue::Bool(b) => Some(*b as i64),
            SqlValue::I16(v) => Some(*v as i64),
            SqlValue::I32(v) => Some(*v as i64),
            SqlValue::I64(v) => Some(*v),
            SqlValue::F32(v) => Some(*v as i64),
            SqlValue::F64(v) => Some(*v as i64),
            SqlValue::Text(s) => s.parse::<i64>().ok(),
            SqlValue::Decimal(s) => s.parse::<f64>().ok().map(|v| v as i64),
            SqlValue::DateTime(_) | SqlValue::Binary(_) | SqlValue::Guid(_) => None,
        }
    }

    /// 转为 f64
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            SqlValue::Null => None,
            SqlValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            SqlValue::I16(v) => Some(*v as f64),
            SqlValue::I32(v) => Some(*v as f64),
            SqlValue::I64(v) => Some(*v as f64),
            SqlValue::F32(v) => Some(*v as f64),
            SqlValue::F64(v) => Some(*v),
            SqlValue::Text(s) => s.parse::<f64>().ok(),
            SqlValue::Decimal(s) => s.parse::<f64>().ok(),
            SqlValue::DateTime(_) | SqlValue::Binary(_) | SqlValue::Guid(_) => None,
        }
    }

    /// 转为字符串（用于显示，非严格）
    pub fn to_display_string(&self) -> String {
        match self {
            SqlValue::Null => String::new(),
            SqlValue::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            SqlValue::I16(v) => v.to_string(),
            SqlValue::I32(v) => v.to_string(),
            SqlValue::I64(v) => v.to_string(),
            SqlValue::F32(v) => v.to_string(),
            SqlValue::F64(v) => v.to_string(),
            SqlValue::Text(s) => s.clone(),
            SqlValue::Decimal(s) => s.clone(),
            SqlValue::DateTime(d) => d.format("%Y-%m-%d %H:%M:%S").to_string(),
            SqlValue::Binary(b) => format!("<binary {} bytes>", b.len()),
            SqlValue::Guid(s) => s.clone(),
        }
    }

    /// 转为二进制
    pub fn to_binary(&self) -> Option<&[u8]> {
        match self {
            SqlValue::Binary(b) => Some(b),
            _ => None,
        }
    }
}

/// 业务使用的字段值（比 SqlValue 更贴近 GIS 语义）
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// NULL
    Null,
    /// 布尔
    Bool(bool),
    /// 短整型
    Short(i16),
    /// 长整型
    Long(i32),
    /// 64 位整型
    Int64(i64),
    /// 单精度浮点
    Single(f32),
    /// 双精度浮点
    Double(f64),
    /// 文本
    String(String),
    /// 日期时间
    Date(DateTime<Utc>),
    /// 二进制 / 几何
    Blob(Vec<u8>),
    /// 未知/透传类型
    Raw(String),
}

impl Value {
    /// 是否 NULL
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// 取长整型
    pub fn as_long(&self) -> Option<i64> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(*b as i64),
            Value::Short(v) => Some(*v as i64),
            Value::Long(v) => Some(*v as i64),
            Value::Int64(v) => Some(*v),
            Value::Single(v) => Some(*v as i64),
            Value::Double(v) => Some(*v as i64),
            Value::String(s) => s.parse::<i64>().ok(),
            Value::Date(_) | Value::Blob(_) | Value::Raw(_) => None,
        }
    }

    /// 取双精度
    pub fn as_double(&self) -> Option<f64> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Short(v) => Some(*v as f64),
            Value::Long(v) => Some(*v as f64),
            Value::Int64(v) => Some(*v as f64),
            Value::Single(v) => Some(*v as f64),
            Value::Double(v) => Some(*v),
            Value::String(s) => s.parse::<f64>().ok(),
            Value::Date(_) | Value::Blob(_) | Value::Raw(_) => None,
        }
    }

    /// 取字符串
    pub fn as_string(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }

    /// 取字节
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(b) => Some(b),
            _ => None,
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => write!(f, ""),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Short(v) => write!(f, "{v}"),
            Value::Long(v) => write!(f, "{v}"),
            Value::Int64(v) => write!(f, "{v}"),
            Value::Single(v) => write!(f, "{v}"),
            Value::Double(v) => write!(f, "{v}"),
            Value::String(s) => write!(f, "{s}"),
            Value::Date(d) => write!(f, "{}", d.format("%Y-%m-%d %H:%M:%S")),
            Value::Blob(b) => write!(f, "<binary {} bytes>", b.len()),
            Value::Raw(s) => write!(f, "{s}"),
        }
    }
}

impl From<SqlValue> for Value {
    fn from(v: SqlValue) -> Self {
        match v {
            SqlValue::Null => Value::Null,
            SqlValue::Bool(b) => Value::Bool(b),
            SqlValue::I16(v) => Value::Short(v),
            SqlValue::I32(v) => Value::Long(v),
            SqlValue::I64(v) => Value::Int64(v),
            SqlValue::F32(v) => Value::Single(v),
            SqlValue::F64(v) => Value::Double(v),
            SqlValue::Text(s) => Value::String(s),
            SqlValue::Decimal(s) => Value::Raw(s),
            SqlValue::DateTime(d) => Value::Date(d),
            SqlValue::Binary(b) => Value::Blob(b),
            SqlValue::Guid(s) => Value::String(s),
        }
    }
}

impl From<Value> for SqlValue {
    fn from(v: Value) -> Self {
        match v {
            Value::Null => SqlValue::Null,
            Value::Bool(b) => SqlValue::Bool(b),
            Value::Short(v) => SqlValue::I16(v),
            Value::Long(v) => SqlValue::I32(v),
            Value::Int64(v) => SqlValue::I64(v),
            Value::Single(v) => SqlValue::F32(v),
            Value::Double(v) => SqlValue::F64(v),
            Value::String(s) => SqlValue::Text(s),
            Value::Date(d) => SqlValue::DateTime(d),
            Value::Blob(b) => SqlValue::Binary(b),
            Value::Raw(s) => SqlValue::Decimal(s),
        }
    }
}

/// 日期解析帮助函数（Access 多种日期字符串）
pub fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(d) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(DateTime::from_naive_utc_and_offset(d, Utc));
    }
    if let Ok(d) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(DateTime::from_naive_utc_and_offset(d, Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(DateTime::from_naive_utc_and_offset(
            d.and_hms_opt(0, 0, 0)?,
            Utc,
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_conversion() {
        let v: Value = SqlValue::F64(12.5).into();
        assert_eq!(v.as_double(), Some(12.5));
        let back: SqlValue = v.into();
        assert_eq!(back, SqlValue::F64(12.5));
    }

    #[test]
    fn test_datetime_parse() {
        let d = parse_datetime("2024-01-05 13:04:05").unwrap();
        assert_eq!(d.format("%Y-%m-%d").to_string(), "2024-01-05");
    }
}
