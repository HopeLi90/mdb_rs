//! jetdb 只读后端：纯 Rust 解析 `*.mdb` / `*.accdb`，**无需任何 ODBC 驱动**。
//!
//! 当工作空间以「只读」权限打开时选用本后端。它跨平台、无位数要求，特别适合
//! Linux / CI / 浏览器（WASM）等无法安装 Access 驱动的环境做数据探查与迁移。
//!
//! 与 [`crate::datastore::odbc::OdbcBackend`] 不同，本后端**不支持写入**；任何
//! 写操作都会返回 [`crate::error::PgdbError::Unsupported`]，由上层给出明确提示。
//!
//! 几何（SHAPE）列在 PGDB 中以 OLE Object 存储，jetdb 会把它作为原始二进制
//!（`Vec<u8>`）返回，与 ODBC 路径拿到的字节一致，因此下游的 ESRI Shape 解码逻辑
//! 可以完全复用。

use std::sync::Mutex;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime, Utc};
use jetdb::format::{column_flags, ColumnType, ObjectType};
use jetdb::{read_catalog, read_table_def, read_table_rows, CatalogEntry, PageReader, Value as JetValue};

use crate::datastore::predicate_matches;
use crate::datastore::{
    BackendCapabilities, ColumnDef, DataTableRow, Predicate, SqlBackend, SqlValue,
};
use crate::error::{PgdbError, Result};
use crate::field::FieldType;

/// Jet 列标志位（稳定，见 Jet / ACE 列定义页）
const JET_NULLABLE: u8 = column_flags::NULLABLE; // 0x02
const JET_AUTO_LONG: u8 = column_flags::AUTO_LONG; // 0x04
const JET_AUTO_UUID: u8 = column_flags::AUTO_UUID; // 0x40

/// 由 jetdb 驱动的只读后端。
pub struct JetdbBackend {
    path: String,
    /// jetdb 的读取 API 需要 `&mut PageReader`，这里用互斥锁在 `&self` 上提供内部可变性。
    reader: Mutex<PageReader>,
    /// 启动时一次性读入的系统编目，避免每次调用都重扫 `MSysObjects`。
    catalog: Vec<CatalogEntry>,
}

impl JetdbBackend {
    /// 打开一个 MDB / ACCDB 文件（只读）。
    pub fn open(path: &str) -> Result<Self> {
        let mut reader = PageReader::open(path).map_err(|e| {
            PgdbError::Backend(format!("jetdb 打开失败（{path}）: {e}"))
        })?;
        let catalog = read_catalog(&mut reader)
            .map_err(|e| PgdbError::Backend(format!("jetdb 读取系统编目失败: {e}")))?;
        Ok(Self {
            path: path.to_string(),
            reader: Mutex::new(reader),
            catalog,
        })
    }

    /// 被打开的 MDB / ACCDB 文件路径。
    pub fn path(&self) -> &str {
        &self.path
    }

    fn find_entry(&self, table: &str) -> Result<&CatalogEntry> {
        self.catalog
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(table) && is_table_like(e.object_type))
            .ok_or_else(|| PgdbError::not_found(table))
    }

    /// 读取整张表的所有行，返回（列名, 行值）。SHAPE 等 OLE 列以 `Binary` 原样返回。
    fn read_rows(&self, table: &str) -> Result<(Vec<String>, Vec<Vec<SqlValue>>)> {
        let entry = self.find_entry(table)?;
        let mut reader = self.reader.lock().unwrap();
        let tdef = read_table_def(&mut reader, &entry.name, entry.table_page)
            .map_err(|e| PgdbError::Backend(format!("读取列定义失败（{table}）: {e}")))?;
        let col_names: Vec<String> = tdef.columns.iter().map(|c| c.name.clone()).collect();
        let result = read_table_rows(&mut reader, &tdef)
            .map_err(|e| PgdbError::Backend(format!("读取数据行失败（{table}）: {e}")))?;
        drop(reader);
        let rows = result
            .rows
            .iter()
            .map(|raw| raw.iter().map(map_value).collect::<Vec<_>>())
            .collect();
        Ok((col_names, rows))
    }
}

/// 是否「表类」对象（用户表 / 系统表 / 链接表），排除查询、窗体、报表、模块等。
fn is_table_like(t: ObjectType) -> bool {
    matches!(
        t,
        ObjectType::Table | ObjectType::SystemTable | ObjectType::LinkedTable
    )
}

/// 把 jetdb 的值映射到传输层 [`SqlValue`]。
fn map_value(v: &JetValue) -> SqlValue {
    match v {
        JetValue::Null => SqlValue::Null,
        JetValue::Bool(b) => SqlValue::Bool(*b),
        JetValue::Byte(b) => SqlValue::I16(*b as i16),
        JetValue::Int(i) => SqlValue::I16(*i),
        JetValue::Long(l) => SqlValue::I32(*l),
        JetValue::BigInt(i) => SqlValue::I64(*i),
        JetValue::Float(f) => SqlValue::F32(*f),
        JetValue::Double(d) => SqlValue::F64(*d),
        JetValue::Text(s) => SqlValue::Text(s.clone()),
        // OLE Object（含 PGDB 的 SHAPE 几何列）与长二进制均作为原始字节透传。
        JetValue::Binary(b) => SqlValue::Binary(b.clone()),
        JetValue::Money(s) => SqlValue::Decimal(s.clone()),
        JetValue::Numeric(s) => SqlValue::Decimal(s.clone()),
        JetValue::Timestamp(d) => SqlValue::DateTime(jetdb_timestamp_to_chrono(*d)),
        JetValue::Guid(s) => SqlValue::Guid(s.clone()),
        JetValue::DateTimeExtended(s) => SqlValue::DateTime(parse_jet_datetime(s)),
    }
}

/// jetdb 的 `Timestamp`：自 1899-12-30 起的浮点天数，转成 chrono 时间。
fn jetdb_timestamp_to_chrono(days: f64) -> DateTime<Utc> {
    let whole = days.floor() as i64;
    let frac = days - days.floor();
    let base = NaiveDate::from_ymd_opt(1899, 12, 30)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .unwrap_or_default();
    let secs = (frac * 86_400.0).round() as i64;
    let dt = base + ChronoDuration::days(whole) + ChronoDuration::seconds(secs);
    DateTime::from_naive_utc_and_offset(dt, Utc)
}

/// jetdb 的 `DateTimeExtended`：ISO 8601 字符串，转成 chrono 时间。
fn parse_jet_datetime(s: &str) -> DateTime<Utc> {
    crate::value::parse_datetime(s).unwrap_or_else(|| {
        DateTime::from_naive_utc_and_offset(NaiveDateTime::default(), Utc)
    })
}

/// jetdb 列类型 -> 与 ODBC 一致的 SQL 类型名（驱动 SQL 类型），便于复用 `FieldType` 映射。
fn jetdb_type_name(t: ColumnType) -> &'static str {
    match t {
        ColumnType::Boolean => "BIT",
        ColumnType::Byte => "BYTE",
        ColumnType::Int => "SMALLINT",
        ColumnType::Long => "LONG",
        ColumnType::Money => "CURRENCY",
        ColumnType::Float => "REAL",
        ColumnType::Double => "DOUBLE",
        ColumnType::Timestamp => "DATETIME",
        ColumnType::Binary => "BINARY",
        ColumnType::Text => "TEXT",
        ColumnType::Ole => "OLEOBJECT",
        ColumnType::Memo => "LONGCHAR",
        ColumnType::Guid => "GUID",
        ColumnType::Numeric => "NUMERIC",
        ColumnType::ComplexType => "COMPLEX",
        ColumnType::BigInt => "BIGINT",
        ColumnType::DateTimeExtended => "DATETIME",
        ColumnType::Unknown(_) => "UNKNOWN",
    }
}

impl SqlBackend for JetdbBackend {
    fn kind(&self) -> &'static str {
        "jetdb"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            raw_sql: false,
            binary_parameter: false,
            hex_literal_binary: false,
            writable: false,
            transaction: false,
        }
    }

    fn table_names(&self) -> Result<Vec<String>> {
        let mut out: Vec<String> = self
            .catalog
            .iter()
            .filter(|e| is_table_like(e.object_type))
            .map(|e| e.name.trim().to_string())
            // 跳过 Access 内部表（与 ODBC 处理习惯一致，也避免探查 MSys 报错）
            .filter(|n| !n.starts_with("MSys") && !n.starts_with('~'))
            .collect();
        out.sort_by_key(|n| n.to_lowercase());
        out.dedup();
        Ok(out)
    }

    fn table_exists(&self, table: &str) -> Result<bool> {
        Ok(self
            .catalog
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case(table) && is_table_like(e.object_type)))
    }

    fn columns(&self, table: &str) -> Result<Vec<ColumnDef>> {
        let entry = self.find_entry(table)?;
        let mut reader = self.reader.lock().unwrap();
        let tdef = read_table_def(&mut reader, &entry.name, entry.table_page)
            .map_err(|e| PgdbError::Backend(format!("读取列定义失败（{table}）: {e}")))?;
        drop(reader);
        let mut defs = Vec::with_capacity(tdef.columns.len());
        for col in &tdef.columns {
            let sql_type = jetdb_type_name(col.col_type).to_string();
            let kind = FieldType::from_sql_type(&sql_type);
            defs.push(ColumnDef {
                name: col.name.clone(),
                sql_type,
                size: Some(col.col_size as usize),
                scale: Some(col.scale as usize),
                nullable: (col.flags & JET_NULLABLE) != 0,
                is_auto: (col.flags & (JET_AUTO_LONG | JET_AUTO_UUID)) != 0,
                kind,
            });
        }
        Ok(defs)
    }

    fn select(
        &self,
        table: &str,
        _columns: &[String],
        filter: &Predicate,
    ) -> Result<Vec<DataTableRow>> {
        let (col_names, rows) = self.read_rows(table)?;
        let mut out = Vec::with_capacity(rows.len());
        for vals in &rows {
            let row = DataTableRow::new(col_names.clone(), vals.clone())?;
            if predicate_matches(filter, &row) {
                out.push(row);
            }
        }
        Ok(out)
    }

    fn count(&self, table: &str, filter: &Predicate) -> Result<u64> {
        let (col_names, rows) = self.read_rows(table)?;
        let mut n = 0u64;
        for vals in &rows {
            let row = DataTableRow::new(col_names.clone(), vals.clone())?;
            if predicate_matches(filter, &row) {
                n += 1;
            }
        }
        Ok(n)
    }

    fn update(&self, table: &str, _sets: &[(String, SqlValue)], _filter: &Predicate) -> Result<u64> {
        Err(PgdbError::Unsupported(format!(
            "jetdb 后端为只读，不支持更新表 {table}"
        )))
    }

    fn insert(&self, table: &str, _values: &[(String, SqlValue)]) -> Result<i64> {
        Err(PgdbError::Unsupported(format!(
            "jetdb 后端为只读，不支持向表 {table} 插入行"
        )))
    }

    fn delete(&self, table: &str, _filter: &Predicate) -> Result<u64> {
        Err(PgdbError::Unsupported(format!(
            "jetdb 后端为只读，不支持删除表 {table} 的行"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        // 与集成测试共享的基准库
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.mdb");
        p.to_string()
    }

    #[test]
    fn open_and_list() {
        let b = JetdbBackend::open(&fixture()).expect("open");
        let names = b.table_names().expect("names");
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("ZD") || n.eq_ignore_ascii_case("QLR")));
        let cols = b.columns("QLR").expect("cols");
        assert!(cols.iter().any(|c| c.name.eq_ignore_ascii_case("OBJECTID")));
    }

    #[test]
    fn select_rows_and_filter() {
        let b = JetdbBackend::open(&fixture()).expect("open");
        let rows = b
            .select("QLR", &[], &Predicate::All)
            .expect("select");
        assert_eq!(rows.len(), 2);
        let one = b
            .select("QLR", &[], &Predicate::eq("OBJECTID", 1i32))
            .expect("select1");
        assert_eq!(one.len(), 1);
    }
}
