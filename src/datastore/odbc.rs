//! ODBC 后端：通过 ODBC 驱动直接访问 `*.mdb`。
//!
//! - Windows：安装 **Access Database Engine (ACE)** 或旧版 Jet 驱动后可读写；
//! - Linux/macOS：`mdbtools` 的 ODBC 驱动可读、但不支持写。
//!
//! 二进制列（Shape）通过 Jet SQL 的十六进制字面量写入（`0x0102...`），
//! 这是 Access 方言中唯一无需特殊驱动的二进制写入方式；读取则统一按
//! 文本 / 二进制两种列缓冲处理，最大程度兼容不同驱动的行为差异。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use odbc_api::buffers::{AnySlice, BinColumn, BufferDesc, ColumnarAnyBuffer, TextColumn};
use odbc_api::handles::{AsStatementRef, SqlChar, SqlText, Statement};
use odbc_api::{ColumnDescription, Connection, Cursor, DataType, Environment};
#[cfg(not(windows))]
use odbc_api::ConnectionOptions;

use crate::error::{PgdbError, Result};

use super::{BackendCapabilities, ColumnDef, DataTableRow, Predicate, SqlBackend};
use crate::sql::{
    count_sql, delete_sql, insert_sql, literal, read_count, select_sql, update_sql, where_clause,
};
use crate::value::SqlValue;

/// 一次 fetch 的批量行数
const BATCH_SIZE: usize = 200;
/// 文本列缓冲上限
const MAX_TEXT_LEN: usize = 4096;
/// 二进制列缓冲上限（单个几何体通常远小于此）
const MAX_BINARY_LEN: usize = 4 * 1024 * 1024;
/// 逐行读取时单列的累积字节上限（文本列在此范围内不会截断）
const COLUMN_BYTE_CAP: usize = 8 * 1024 * 1024;

static ENV: OnceLock<Environment> = OnceLock::new();

/// 进程内唯一的 ODBC 环境
fn environment() -> Result<&'static Environment> {
    ENV.get_or_init(|| Environment::new().expect("初始化 ODBC 环境失败"))
        .into_ok()
}

trait OnceLockExt {
    fn into_ok(self) -> Result<&'static Environment>;
}

impl OnceLockExt for &'static Environment {
    fn into_ok(self) -> Result<&'static Environment> {
        Ok(self)
    }
}

/// ODBC 后端
pub struct OdbcBackend {
    conn: Mutex<Connection<'static>>,
    caps: BackendCapabilities,
    #[allow(dead_code)]
    path: String,
    /// 实际建立连接所用的连接串（诊断用）
    connection_string: String,
    /// 实际使用的驱动名（诊断用）
    driver: String,
    /// 是否可用批量取行（`SQL_ATTR_ROW_ARRAY_SIZE`）。
    /// MDBTools 这类轻量驱动不支持，会被自动降级为逐行读取。
    bulk_fetch: AtomicBool,
}

impl OdbcBackend {
    /// 连接一个 mdb 文件
    ///
    /// `connection_string` 为 `None` 时自动探测本机可用的 Access / ODBC 驱动：
    /// Windows 依次尝试 ACE（`*.mdb, *.accdb`）与 Jet 4（`*.mdb`），
    /// 其它平台尝试 MDBTools（只读）。也支持直接传入 `DSN=xxx` 用户数据源。
    pub fn connect(path: &str, connection_string: Option<&str>) -> Result<Self> {
        let env = environment()?;
        let candidates = match connection_string {
            Some(cs) => vec![normalize_connection_string(cs)],
            None => connection_candidates(path),
        };
        let mut last_error = String::new();
        for cs in &candidates {
            match connect_with_encoded_string(env, cs) {
                Ok(conn) => {
                    let driver = extract_driver(cs);
                    log::debug!("ODBC 连接成功，驱动: {driver}");
                    // MDBTools 等轻量驱动不支持批量取行，直接降级为逐行读取
                    let bulk = !matches!(
                        driver.to_ascii_lowercase().as_str(),
                        "mdbtools" | "mdbtoolsodbc" | "libmdbodbc"
                    );
                    return Ok(Self {
                        conn: Mutex::new(conn),
                        caps: capabilities_for(cs),
                        path: path.to_string(),
                        connection_string: cs.clone(),
                        driver,
                        bulk_fetch: AtomicBool::new(bulk),
                    });
                }
                Err(e) => {
                    log::debug!("ODBC 连接失败（{}）: {e}", extract_driver(cs));
                    last_error = format!("{e}");
                }
            }
        }
        Err(PgdbError::Backend(format!(
            "ODBC 连接失败，已尝试 {} 个候选连接串。\n最后错误: {last_error}\n{}",
            candidates.len(),
            driver_troubleshooting()
        )))
    }

    /// 用自定义连接串连接
    pub fn with_connection_string(connection_string: &str) -> Result<Self> {
        Self::connect("", Some(connection_string))
    }

    /// 实际使用的连接串
    pub fn connection_string(&self) -> &str {
        &self.connection_string
    }

    /// 实际使用的驱动名
    pub fn driver_name(&self) -> &str {
        &self.driver
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection<'static>> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前驱动是否支持 `DESCRIBE TABLE`。
    ///
    /// 这是 **mdbtools 的专有 SQL 扩展**，微软 ACE/Jet 会直接报
    /// `42000 native -3500 无效的 SQL 语句` —— 错误消息听起来像"SQL 写错了"，
    /// 极易误导排查方向，因此这里按驱动名做一次性判定，绝不盲发。
    ///
    /// 判定依据是连接时记录的驱动名（见 [`OdbcBackend::driver_name`]）。
    fn supports_describe_table(&self) -> bool {
        let d = self.driver.to_ascii_lowercase();
        d.contains("mdbtools") || d.contains("libmdbodbc") || d.contains("mdb-odbc")
    }

    /// 执行非查询语句
    fn exec(&self, sql: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(sql, ())
            .map_err(|e| PgdbError::Sql(format!("{sql} => {e}")))?;
        Ok(())
    }

    /// 查询并返回一个字符串矩阵（用于元数据采集）
    pub fn query_strings(&self, sql: &str) -> Result<Vec<Vec<String>>> {
        let rows = self.raw_query(sql)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                r.values()
                    .iter()
                    .map(|v| v.to_display_string())
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    fn ensure_writable(&self) -> Result<()> {
        if !self.caps.writable {
            return Err(PgdbError::read_only(
                "当前 ODBC 驱动为只读（例如 Linux 下的 MDBTools 驱动），不支持写入；\
                 请使用 Windows + Microsoft Access Database Engine 驱动（位数须与程序一致）",
            ));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ 驱动探测

/// 建立连接，但**先把连接串按驱动的编码转成窄字节**。
///
/// `Environment::connect_with_connection_string` 只能收 `&str`，内部按 UTF-8
/// 原样转发；当 `DBQ` 指向含中文的路径（`D:\数据\test.mdb`）时，Windows 的
/// 驱动管理器会把 UTF-8 字节按 ANSI 代码页解读而找不到文件。这里直接操作
/// 底层句柄，先 [`encode_outbound`] 再传给 `SQLDriverConnect`。
fn connect_with_encoded_string(
    env: &'static Environment,
    connection_string: &str,
) -> std::result::Result<Connection<'static>, odbc_api::Error> {
    #[cfg(windows)]
    {
        use odbc_api::handles::OutputStringBuffer;
        use odbc_api::DriverCompleteOption;

        // 出站编码：连接串里的中文路径要先转成驱动管理器期望的 ANSI 代码页字节。
        // `SqlText` 在 `narrow` 下就是 `&str`，其 `as_bytes()` 即送给
        // `SQLDriverConnectA` 的字节序列，因此这里把编码后的字节按“字节等价”
        // 的字符串交回——非 ASCII 字节恰好只落在 `U+0080..=U+00FF`，
        // 与原始字节一一对应，可无损往返（见 `bytes_to_transport_str`）。
        let bytes = encode_outbound(connection_string);
        let transport = bytes_to_transport_str(&bytes);
        let mut completed = OutputStringBuffer::empty();
        let conn = env.driver_connect(
            &transport,
            &mut completed,
            DriverCompleteOption::NoPrompt,
        )?;
        // SAFETY: 句柄不借用栈上数据（连接串已复制进驱动），`env` 为进程级 `'static`。
        Ok(unsafe {
            std::mem::transmute::<Connection<'_>, Connection<'static>>(conn)
        })
    }
    #[cfg(not(windows))]
    {
        env.connect_with_connection_string(connection_string, ConnectionOptions::default())
    }
}

/// 把任意字节序列映射成“字节等价”的字符串：每个字节 `b` 映射到码点 `b`。
///
/// 因为 odbc-api 的 `SqlText::new` 只接受 `&str`，而我们又必须把
/// ANSI 代码页字节原封不动送给 `SQLDriverConnectA`，所以需要这样一层
/// 无损失的字节↔字符串映射。对纯 ASCII 输入它就是恒等变换。
#[cfg(windows)]
fn bytes_to_transport_str(bytes: &[u8]) -> String {
    if bytes.is_ascii() {
        // 快路径：ASCII 直接就是合法 UTF-8
        return String::from_utf8_lossy(bytes).into_owned();
    }
    bytes.iter().map(|&b| b as char).collect()
}

/// 驱动探测结果（供诊断与 CLI 输出）
#[derive(Debug, Clone, Default)]
pub struct DriverProbe {
    /// 系统里登记的全部 ODBC 驱动名
    pub installed: Vec<String>,
    /// 被识别为 Access / mdb 可用的驱动（按推荐顺序）
    pub access_drivers: Vec<String>,
    /// 系统里登记的 DSN
    pub data_sources: Vec<String>,
}

/// 列出本机 ODBC 驱动与数据源，用于排查"驱动没装 / 位数不匹配"等问题
pub fn probe_drivers() -> DriverProbe {
    let mut probe = DriverProbe::default();
    if let Ok(env) = environment() {
        probe.installed = env
            .drivers()
            .unwrap_or_default()
            .into_iter()
            .map(|d| d.description)
            .collect();
        probe.data_sources = env
            .data_sources()
            .unwrap_or_default()
            .into_iter()
            .map(|d| format!("{} [{}]", d.server_name, d.driver))
            .collect();
    }
    probe.access_drivers = access_driver_candidates(&probe.installed);
    probe
}

/// 已安装的 ODBC 驱动名
fn installed_driver_names() -> Vec<String> {
    environment()
        .and_then(|env| {
            env.drivers()
                .map_err(|e| PgdbError::Backend(format!("枚举 ODBC 驱动失败: {e}")))
        })
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.description)
        .collect()
}

/// 从已安装驱动中挑选可用的 Access 驱动（ACE 优先于 Jet 4）。
/// 只报告真实安装的驱动；连接串兜底名称见 [`connection_candidates`]。
fn access_driver_candidates(installed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in installed {
        let lower = name.to_ascii_lowercase();
        let is_access = lower.contains("access");
        let supports_mdb = lower.contains("mdb") || lower.contains("accdb");
        let is_other_isam = lower.contains("dbase")
            || lower.contains("excel")
            || lower.contains("paradox")
            || lower.contains("text")
            || lower.contains("sharepoint");
        if is_access && supports_mdb && !is_other_isam {
            out.push(name.clone());
        }
    }
    // ACE（支持 accdb）优先于 Jet 4
    out.sort_by_key(|n| !n.to_ascii_lowercase().contains("accdb"));
    out
}

/// 各版本 Access 驱动的固定名称（ACE 12/16 与 Jet 4）
const KNOWN_ACCESS_DRIVERS: &[&str] = &[
    "Microsoft Access Driver (*.mdb, *.accdb)",
    "Microsoft Access Driver (*.mdb)",
];

/// 非 Windows 平台使用的只读开源驱动
const MDBTOOLS_DRIVER: &str = "MDBTools";

/// 按平台给出候选连接串（按尝试顺序）
fn connection_candidates(path: &str) -> Vec<String> {
    if let Some(cs) = std::env::var("PGDB_ODBC_CONN").ok().filter(|s| !s.is_empty()) {
        return vec![normalize_connection_string(&cs)];
    }
    let mut out = Vec::new();
    if cfg!(target_os = "windows") {
        let probed = access_driver_candidates(&installed_driver_names());
        for driver in &probed {
            out.push(access_connection_string(driver, path));
        }
        // 兜底：即便 SQLDrivers 没枚举到（权限/位数问题）也尝试各版本内置名称
        for known in KNOWN_ACCESS_DRIVERS {
            if !probed.iter().any(|n| n == known) {
                out.push(access_connection_string(known, path));
            }
        }
    } else {
        out.push(format!(
            "Driver={{{MDBTOOLS_DRIVER}}};DBQ={path};UID=Admin;PWD=;"
        ));
        // 部分发行版把驱动注册成 MDBToolsODBC / libmdbodbc
        for alt in ["MDBToolsODBC", "libmdbodbc"] {
            out.push(format!("Driver={{{alt}}};DBQ={path};UID=Admin;PWD=;"));
        }
    }
    out
}

/// 标准 DSN-less 连接串：直接指向 mdb/accdb 文件
fn access_connection_string(driver: &str, path: &str) -> String {
    format!("Driver={{{driver}}};DBQ={path};UID=Admin;PWD=;")
}

/// 规范化用户传入的连接串：
/// - 兼容 GDAL 写法 `PGeo:DSN=xxx` / `PGEO:xxx.mdb`
/// - 传入裸文件路径时按平台自动补全驱动
pub fn normalize_connection_string(cs: &str) -> String {
    let trimmed = cs.trim();
    let stripped = ["PGEO:", "PGeo:", "pgeo:"]
        .iter()
        .find_map(|p| trimmed.strip_prefix(*p))
        .unwrap_or(trimmed);
    let upper = stripped.to_ascii_uppercase();
    let has_target = upper.contains("DSN=") || upper.contains("DRIVER=") || upper.contains("DBQ=");
    if has_target {
        return stripped.to_string();
    }
    // 裸路径：当作 mdb 文件处理
    if !cfg!(target_os = "windows") {
        return format!("Driver={{{MDBTOOLS_DRIVER}}};DBQ={stripped};UID=Admin;PWD=;");
    }
    let driver = access_driver_candidates(&installed_driver_names())
        .first()
        .cloned()
        .unwrap_or_else(|| KNOWN_ACCESS_DRIVERS[0].to_string());
    access_connection_string(&driver, stripped)
}

/// 从连接串里取出驱动名（用于诊断输出）
fn extract_driver(cs: &str) -> String {
    cs.split(';')
        .find_map(|part| {
            let (k, v) = part.split_once('=')?;
            if k.trim().eq_ignore_ascii_case("driver") {
                Some(v.trim().trim_matches('{').trim_matches('}').to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "DSN".to_string())
}

/// 按连接串判定后端能力
fn capabilities_for(cs: &str) -> BackendCapabilities {
    let upper = cs.to_ascii_uppercase();
    let mut readonly = upper.contains("MDBTOOLS")
        || upper.contains("MDBTOOLSODBC")
        || upper.contains("LIBMDBODBC")
        || upper.contains("READONLY=TRUE")
        || upper.contains("MODE=READ");
    // 非 Windows 平台上 "Microsoft Access Driver" 只可能是 mdbtools 的别名注册
    // （Debian odbc-mdbtools 包会用这个名字注册 libmdbodbc），真实微软驱动并不存在。
    if !cfg!(target_os = "windows") && upper.contains("MICROSOFT ACCESS DRIVER") {
        readonly = true;
    }
    let writable = !readonly;
    BackendCapabilities {
        raw_sql: true,
        binary_parameter: writable,
        hex_literal_binary: true,
        writable,
        transaction: writable,
    }
}

/// 连接失败时的排查提示
fn driver_troubleshooting() -> String {
    if cfg!(target_os = "windows") {
        "排查建议：\n\
         1. 运行 `pgdb-cli <任意.mdb> drivers` 查看系统里可用的 ODBC 驱动；\n\
         2. 未列出 Access 驱动时，安装 Microsoft Access Database Engine 2016 Redistributable\n\
            （AccessDatabaseEngine.exe 或 AccessDatabaseEngine_X64.exe）；\n\
         3. 驱动位数必须与程序位数一致：64 位程序需要 64 位 ACE，32 位程序需要 32 位 ACE，\n\
            二者不能共存，可用 `cargo build --target i686-pc-windows-msvc` 编译 32 位版本；\n\
         4. 仍失败时可用 `PGDB_ODBC_CONN=Driver={Microsoft Access Driver (*.mdb, *.accdb)};DBQ=C:\\a.mdb;`\n\
            环境变量显式指定连接串，或用 Windows 的 ODBC 数据源管理器建一个用户 DSN 后传 `DSN=名字`。"
            .to_string()
    } else {
        "排查建议：Linux/macOS 下 mdbtools 的 ODBC 驱动为只读且不支持写，\n\
         如需读写请使用 Windows + Access Database Engine 驱动。"
            .to_string()
    }
}

impl SqlBackend for OdbcBackend {
    fn kind(&self) -> &'static str {
        "odbc"
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.caps
    }

    fn table_names(&self) -> Result<Vec<String>> {
        // 优先使用 ODBC 的 SQLTables 目录接口，避免依赖 MSysObjects 权限；
        // 走底层原语以兼容不实现 SQLSetStmtAttr 的轻量驱动（如 mdbtools）。
        let conn = self.lock();
        let mut out = self
            .table_names_bulk(&conn)
            .or_else(|_| Self::table_names_sequential(&conn))?;
        if out.is_empty() {
            return Err(PgdbError::Backend(
                "ODBC 驱动没有返回任何表，请检查连接串与驱动位数（32/64 位需匹配）".into(),
            ));
        }
        // 稳定排序，便于脚本比对；Access 表名大小写不敏感但保留原样
        out.sort_by_key(|n| n.to_lowercase());
        out.dedup();
        Ok(out)
    }

    fn table_exists(&self, table: &str) -> Result<bool> {
        Ok(self
            .table_names()?
            .iter()
            .any(|n| n.eq_ignore_ascii_case(table)))
    }

    fn columns(&self, table: &str) -> Result<Vec<ColumnDef>> {
        let conn = self.lock();
        // 两条来源各有短板，按**驱动能力**择优，而不是盲目重试：
        //
        // - `DESCRIBE TABLE`：**mdbtools 专有扩展**，其 SQL 层直接读 Jet 表定义页，
        //   能给出列名 + 类型 + 字节宽度（Text 除以 2 得字符数），且支持中文表名。
        //   ⚠️ Microsoft ACE/Jet **不支持**该语句，发了会报：
        //      `42000 native -3500 无效的 SQL 语句；在此 'DELETE'、'INSERT'…`
        //   所以只在 mdbtools 驱动上尝试。
        // - `SQLColumns`：ODBC 标准目录函数，Windows + ACE/Jet 下最完整
        //   （`COLUMN_SIZE` / `DECIMAL_DIGITS` / `NULLABLE` 齐全）；
        //   但 mdbtools 的实现只给列名与类型，`COLUMN_SIZE` 恒为 NULL。
        //
        // 最后才用空结果集 + `SQLDescribeCol` 兜底（只有列名与类型）。
        if self.supports_describe_table() {
            match describe_table_columns(&conn, table) {
                Ok(defs) if !defs.is_empty() => {
                    log::debug!("{table}: DESCRIBE TABLE 命中 {} 列", defs.len());
                    return Ok(defs);
                }
                Ok(_) => log::debug!("{table}: DESCRIBE TABLE 无结果，改用 SQLColumns"),
                Err(e) => log::debug!("{table}: DESCRIBE TABLE 不可用（{e}），改用 SQLColumns"),
            }
        }
        match catalog_column_defs(&conn, table) {
            Ok(defs) if !defs.is_empty() => {
                log::debug!("{table}: SQLColumns 命中 {} 列", defs.len());
                return Ok(defs);
            }
            Ok(_) => log::debug!("{table}: SQLColumns 返回空集，改用空结果集探测列"),
            Err(e) => log::debug!("{table}: SQLColumns 失败（{e}），改用空结果集探测列"),
        }
        log::debug!("{table}: 落到空结果集探测列");
        describe_columns_by_query(&conn, table)
    }

    fn select(
        &self,
        table: &str,
        columns: &[String],
        filter: &Predicate,
    ) -> Result<Vec<DataTableRow>> {
        let sql = select_sql(table, columns, Some(filter))?;
        self.raw_query(&sql)
    }

    fn update(
        &self,
        table: &str,
        sets: &[(String, crate::value::SqlValue)],
        filter: &Predicate,
    ) -> Result<u64> {
        self.ensure_writable()?;
        if sets.is_empty() {
            return Ok(0);
        }
        // Jet 没有可靠的 @@ROWCOUNT，先统计命中行数
        let before = self.count(table, filter)?;
        let conn = self.lock();
        exec_write(&conn, table, sets, filter)?;
        Ok(before)
    }

    fn insert(
        &self,
        table: &str,
        values: &[(String, crate::value::SqlValue)],
    ) -> Result<i64> {
        self.ensure_writable()?;
        let filtered: Vec<_> = values
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if filtered.is_empty() {
            return Ok(0);
        }
        let conn = self.lock();
        exec_insert(&conn, table, &filtered)?;
        drop(conn);
        match self.identity() {
            Ok(Some(id)) => Ok(id),
            Ok(None) => {
                log::warn!("驱动未提供 @@IDENTITY，无法返回新记录的 OBJECTID");
                Ok(0)
            }
            Err(e) => Err(e),
        }
    }

    fn delete(&self, table: &str, filter: &Predicate) -> Result<u64> {
        self.ensure_writable()?;
        let before = self.count(table, filter)?;
        let sql = delete_sql(table, filter)?;
        self.exec(&sql)?;
        Ok(before)
    }

    fn count(&self, table: &str, filter: &Predicate) -> Result<u64> {
        let sql = count_sql(table, filter)?;
        let rows = self.raw_query(&sql)?;
        read_count(&rows)
    }

    fn raw_query(&self, sql: &str) -> Result<Vec<DataTableRow>> {
        self.raw_query_with_retry(sql, true)
    }

    fn raw_execute(&self, sql: &str) -> Result<u64> {
        self.exec(sql)?;
        Ok(1)
    }

    fn begin(&self) -> Result<bool> {
        let conn = self.lock();
        conn.set_autocommit(false)
            .map_err(|e| PgdbError::Backend(e.to_string()))?;
        Ok(true)
    }

    fn commit(&self) -> Result<()> {
        let conn = self.lock();
        conn.commit()
            .map_err(|e| PgdbError::Backend(e.to_string()))?;
        conn.set_autocommit(true)
            .map_err(|e| PgdbError::Backend(e.to_string()))?;
        Ok(())
    }

    fn rollback(&self) -> Result<()> {
        let conn = self.lock();
        conn.rollback()
            .map_err(|e| PgdbError::Backend(e.to_string()))?;
        conn.set_autocommit(true)
            .map_err(|e| PgdbError::Backend(e.to_string()))?;
        Ok(())
    }
}

impl OdbcBackend {
    /// 执行查询；若驱动不支持批量取行则自动降级为逐行读取并重试一次
    fn raw_query_with_retry(&self, sql: &str, allow_retry: bool) -> Result<Vec<DataTableRow>> {
        let conn = self.lock();
        // 批量模式：用 odbc-api 的安全入口（内部会设置 SQL_ATTR_ROW_ARRAY_SIZE）
        if self.bulk_fetch.load(Ordering::Relaxed) {
            // 接管诊断输出：否则 odbc-api 会用 `from_utf8_lossy` 把 GBK 报错
            // 解成 `�ַ������ݣ��ҽض�` 这类乱码
            let capture = DiagnosticCapture::new();
            let outcome = conn.execute(sql, ());
            match outcome {
                Ok(Some(cursor)) => {
                    capture.finish_silent();
                    return read_rows_bulk(cursor, None);
                }
                Ok(None) => {
                    capture.finish_silent();
                    return Ok(Vec::new());
                }
                Err(e) => {
                    capture.finish_silent();
                    let rendered = format!("{e}");
                    if is_bulk_unsupported(&rendered) && allow_retry {
                        log::debug!("驱动不支持批量取行，改为逐行读取");
                        self.bulk_fetch.store(false, Ordering::Relaxed);
                        // 落到下方的逐行路径
                    } else {
                        return Err(PgdbError::Sql(format!("{sql} => {rendered}")));
                    }
                }
            }
        }
        // 逐行模式（mdbtools 等轻量驱动）：底层 SQLExecDirect + SQLGetData 逐列读取
        Self::raw_query_sequential(&conn, sql)
    }

    /// 批量路径读取表清单（`SqlTables` 目录接口 + 列式缓冲）
    fn table_names_bulk(&self, conn: &Connection<'static>) -> Result<Vec<String>> {
        if !self.bulk_fetch.load(Ordering::Relaxed) {
            return Err(PgdbError::Backend("已切换到逐行模式".into()));
        }
        let cursor = conn
            .tables("", "", "", "TABLE,SYSTEM TABLE,VIEW")
            .map_err(|e| PgdbError::Backend(format!("读取表清单失败: {e}")))?;
        let rows = read_rows_bulk(cursor, Some(TABLES_BUFFER_DESCS.to_vec()))?;
        Ok(table_names_from_rows(rows))
    }

    /// 逐行路径读取表清单（不设置任何语句属性，兼容 mdbtools）
    fn table_names_sequential(conn: &Connection<'static>) -> Result<Vec<String>> {
        log::debug!("表清单：改用逐行读取");
        let mut stmt = conn
            .preallocate()
            .map_err(|e| PgdbError::Backend(format!("分配语句句柄失败: {e}")))?
            .into_statement();
        // 先把 UTF-8 文本转成驱动期望的窄字节（Windows = ANSI 代码页），
        // 用自持有结构持有底层数组，保证整个调用期间字节存活。
        let filter = OwnedSqlText::new("TABLE,SYSTEM TABLE,VIEW");
        let empty = OwnedSqlText::new("");
        // SAFETY: `stmt` 为刚分配、未绑定缓冲的句柄；四个 SqlText 在调用期间有效。
        stmt.tables(
            &empty.as_text()?,
            &empty.as_text()?,
            &empty.as_text()?,
            &filter.as_text()?,
        )
        .into_result(&stmt)
        .map_err(|e| PgdbError::Backend(format!("读取表清单失败: {e}")))?;
        let (names, types) = describe_cursor(&mut stmt)?;
        let kinds: Vec<ColKind> = types.iter().map(kind_of_datatype).collect();
        Ok(table_names_from_rows(fetch_all(&mut stmt, &names, &kinds)?))
    }

    /// 在已持有连接锁的前提下做逐行查询（避免重入 Mutex）
    ///
    /// 直接使用 `SQLExecDirect` + `SQLFetch` + `SQLGetData` 原语，绕开 odbc-api
    /// 便捷封装的限制（`SQL_ATTR_ROW_ARRAY_SIZE` 不受支持、`SQLGetData` 返回
    /// `SQL_NO_DATA` 时封装会 panic），从而兼容 mdbtools 等只实现最小子集的驱动。
    fn raw_query_sequential(conn: &Connection<'static>, sql: &str) -> Result<Vec<DataTableRow>> {
        let Some(mut stmt) = exec_direct_bridged(conn, sql)? else {
            return Ok(Vec::new());
        };
        let ncols = stmt
            .num_result_cols()
            .into_result(&stmt)
            .map_err(|e| PgdbError::Sql(e.to_string()))? as usize;
        let (names, kinds) = describe_result(conn, &mut stmt, sql, ncols)?;
        fetch_all(&mut stmt, &names, &kinds)
    }

    /// 读取 `SELECT @@IDENTITY`（Jet 4 支持）
    pub fn identity(&self) -> Result<Option<i64>> {
        let rows = self.raw_query("SELECT @@IDENTITY").map_err(|_| ());
        match rows {
            Ok(rows) => Ok(rows.first().and_then(|r| {
                r.get_index(0)
                    .and_then(|v| v.to_i64())
                    .or_else(|| {
                        r.get("Expr1000").or_else(|| r.get("Expr1001")).and_then(|v| v.to_i64())
                    })
            })),
            Err(_) => Ok(None),
        }
    }
}

/// SQLTables 结果集的 5 列缓冲描述
const TABLES_BUFFER_DESCS: [BufferDesc; 5] = [
    BufferDesc::Text { max_str_len: 256 }, // TABLE_CAT
    BufferDesc::Text { max_str_len: 256 }, // TABLE_SCHEM
    BufferDesc::Text { max_str_len: 512 }, // TABLE_NAME
    BufferDesc::Text { max_str_len: 64 },  // TABLE_TYPE
    BufferDesc::Text { max_str_len: 256 }, // REMARKS
];

/// 判断错误是否为"驱动不支持批量取行"（IM001 / InvalidRowArraySize）
fn is_bulk_unsupported(err: &str) -> bool {
    err.contains("IM001")
        || err.contains("does not support this function")
        || err.contains("Invalid attribute")
        || err.contains("Invalid row array size")
        || err.contains("ROW_ARRAY_SIZE")
}

/// 读取结果集的列名与列类型（两种读取路径共用）
fn describe_cursor<S>(source: &mut S) -> Result<(Vec<String>, Vec<DataType>)>
where
    S: AsStatementRef,
{
    let mut stmt = source.as_stmt_ref();
    let ncols = stmt
        .num_result_cols()
        .into_result(&stmt)
        .map_err(|e| PgdbError::Sql(e.to_string()))? as usize;
    let mut names = Vec::with_capacity(ncols);
    let mut types = Vec::with_capacity(ncols);
    for i in 1..=ncols as u16 {
        // 优先用 describe_col 取列名：部分驱动（如 mdbtools）的 col_name 返回空串。
        //
        // 注意：不能用 `desc.name_to_string()` —— 它内部是 `slice_to_utf8` →
        // `String::from_utf8_lossy`，在 Windows 上会把 GBK 列名解成乱码。
        // 这里直接取原始字节走 [`decode_inbound`] 桥接。
        let mut desc = ColumnDescription::default();
        let name = match stmt.describe_col(i, &mut desc).into_result(&stmt) {
            Ok(()) => decode_inbound(&desc.name),
            Err(_) => {
                let mut buf: Vec<SqlChar> = Vec::new();
                stmt.col_name(i, &mut buf)
                    .into_result(&stmt)
                    .map_err(|e| PgdbError::Sql(e.to_string()))?;
                decode_inbound(&buf)
            }
        };
        let dt = data_type_of(&mut stmt, i)?;
        names.push(name);
        types.push(dt);
    }
    Ok((names, types))
}

/// 由 `SQLColAttribute(SQL_DESC_CONCISE_TYPE)` 推导 `DataType`
///
/// 等价于 `ResultSetMetadata::col_data_type`，但直接作用在裸语句句柄上，
/// 便于两种读取路径（安全 Cursor / 底层原语）共用同一套列类型判定。
fn data_type_of<Stmt: Statement>(stmt: &mut Stmt, idx: u16) -> Result<DataType> {
    let kind = stmt
        .col_concise_type(idx)
        .into_result(stmt)
        .map_err(|e| PgdbError::Sql(e.to_string()))?;
    let size = stmt
        .col_octet_length(idx)
        .into_result(stmt)
        .unwrap_or(-1)
        .max(0) as usize;
    let digits = stmt
        .col_scale(idx)
        .into_result(stmt)
        .unwrap_or(0)
        .clamp(0, i16::MAX as isize) as i16;
    Ok(DataType::new(kind, size, digits))
}

/// `SQLGetData` 的列读取封装：用公开的行缓冲类型 `TextColumn` / `BinColumn` 作为目标，
/// 从而拿到 indicator（NULL / 截断）语义，而不触发 odbc-api 在 `SQL_NO_DATA` 上的 panic。
///
/// 返回 `Ok(None)` 表示该列为 NULL；`Ok(Some(bytes))` 为本次取回的内容。
trait GetDataExt: Statement {
    /// 读取文本列的**原始字节**（驱动口径：Windows 为 ANSI 代码页，Linux 为 UTF-8）。
    ///
    /// 这里刻意不做任何编码转换：转换统一由 [`decode_cell`] → [`decode_inbound`]
    /// 完成。若在此处先转一次、`decode_cell` 里再转一次，会造成二次转换
    /// （Linux 下把 UTF-8 误当 GBK 重编码，反而把正确数据弄坏）。
    fn get_data_text(&mut self, idx: u16, cap: usize) -> Result<Option<Vec<u8>>>
    where
        Self: Sized,
    {
        let mut buf = TextColumn::<SqlChar>::new(1, cap);
        // `into_result_option`：SQLGetData 返回 SQL_NO_DATA 表示该列已无更多数据
        match self.get_data(idx, &mut buf).into_result_option(self) {
            Ok(Some(())) => {}
            Ok(None) => return Ok(None),
            Err(e) => return Err(PgdbError::Sql(format!("读取第 {idx} 列失败: {e}"))),
        }
        // `TextColumn<SqlChar>` 在 narrow 下底层就是 `&[u8]`，直接取原始字节
        Ok(buf.value_at(0).map(|chars| chars.to_vec()))
    }

    /// 读取二进制列（返回原始字节）
    fn get_data_binary(&mut self, idx: u16, cap: usize) -> Result<Option<Vec<u8>>>
    where
        Self: Sized,
    {
        let mut buf = BinColumn::new(1, cap);
        match self.get_data(idx, &mut buf).into_result_option(self) {
            Ok(Some(())) => {}
            Ok(None) => return Ok(None),
            Err(e) => return Err(PgdbError::Sql(format!("读取第 {idx} 列失败: {e}"))),
        }
        Ok(buf.value_at(0).map(|b| b.to_vec()))
    }
}

impl<Stmt: Statement> GetDataExt for Stmt {}

/// 读取一列的值（`SQLGetData` 原语），自动处理长文本 / 二进制分块。
///
/// - 返回 `SqlValue::Null` 表示该列为 NULL；
/// - 文本列按 UTF-8 解释，二进制列（几何 Shape）保留原始字节；
/// - `cap` 为单列累积字节上限，超出即截断（几何体远小于此值）。
fn read_column<Stmt>(
    stmt: &mut Stmt,
    idx: u16,
    kind: ColKind,
    cap: usize,
) -> Result<crate::value::SqlValue>
where
    Stmt: Statement,
{
    let binary = kind == ColKind::Binary;
    let mut buf: Vec<u8> = Vec::new();
    // 分块读取：部分驱动一次 `SQLGetData` 有长度上限，需循环追加
    let mut chunk = 64 * 1024usize;
    loop {
        let room = cap.saturating_sub(buf.len()).max(1);
        let cap_here = chunk.min(room);
        // `SQLGetData` 通过 indicator 返回实际长度 / NULL / 截断状态
        match if binary {
            stmt.get_data_binary(idx, cap_here)
        } else {
            stmt.get_data_text(idx, cap_here)
        }
        .map_err(|e| PgdbError::Sql(format!("读取第 {idx} 列失败: {e}")))?
        {
            Some(bytes) => {
                if bytes.is_empty() && !binary {
                    // 空字符串区别于 NULL：继续读取后续块
                }
                let short = bytes.len() < cap_here;
                buf.extend_from_slice(&bytes);
                if short || buf.len() >= cap {
                    break;
                }
            }
            None => {
                if buf.is_empty() {
                    return Ok(crate::value::SqlValue::Null);
                }
                break;
            }
        }
        chunk = chunk.saturating_mul(8);
    }
    Ok(decode_cell(&buf, kind))
}

/// 求得结果集的列名与逻辑类型。
///
/// 优先用 `SQLDescribeCol`（`describe_cursor`）；部分驱动（如 mdbtools）对含
/// OLE/BLOB 列的表调用 `SQLColAttribute` 会失败，此时改用 `SQLColumns` 目录函数
/// 取列信息（按 `SELECT` 列序对齐），保证几何列（Shape）等也能正确被识别为二进制。
///
/// 目录回退时按“查询列数 vs. 表列数”判定：列数一致说明是 `SELECT *`，可直接按
/// 顺序对齐；列数不同（显式投影）则退回全部文本读取并给出告警。
fn describe_result<Stmt>(
    conn: &Connection<'static>,
    stmt: &mut Stmt,
    sql: &str,
    ncols: usize,
) -> Result<(Vec<String>, Vec<ColKind>)>
where
    Stmt: Statement + AsStatementRef,
{
    if let Ok((names, types)) = describe_cursor(stmt) {
        if names.iter().all(|n| !n.trim().is_empty()) {
            let kinds = types.iter().map(kind_of_datatype).collect();
            return Ok((names, kinds));
        }
    }
    let table = table_of_sql(sql)
        .ok_or_else(|| PgdbError::Sql(format!("无法从查询推断表名以获取列元数据: {sql}")))?;
    let defs = catalog_column_defs(conn, &table)?;
    if defs.len() != ncols {
        log::warn!(
            "{table}: 目录列数 {} 与查询列数 {ncols} 不一致，按文本读取全部列",
            defs.len()
        );
        let (names, _) = describe_cursor(stmt)?;
        return Ok((names, vec![ColKind::Text; ncols]));
    }
    let names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
    let kinds: Vec<ColKind> = defs
        .iter()
        .map(|d| kind_of_typename(&d.sql_type))
        .collect();
    Ok((names, kinds))
}

/// 用标准 `Cursor` 接口逐行读取（用于 `SQLTables` / `SQLColumns` 等目录结果集）。
///
/// 目录结果集由驱动管理器生成、结构固定，不含 Blob 列，因此走安全封装即可。
fn read_rows_cursor(
    cursor: &mut impl Cursor,
    names: &[String],
    kinds: &[ColKind],
) -> Result<Vec<DataTableRow>> {
    let ncols = names.len().min(kinds.len());
    let names = &names[..ncols];
    let kinds = &kinds[..ncols];
    let mut out: Vec<DataTableRow> = Vec::new();
    while let Some(mut row) = cursor.next_row().map_err(|e| PgdbError::Sql(e.to_string()))? {
        let mut vals = Vec::with_capacity(ncols);
        for (i, &kind) in kinds.iter().enumerate() {
            let idx = (i + 1) as u16;
            let mut buf: Vec<u8> = Vec::new();
            let has_value = if kind == ColKind::Binary {
                row.get_binary(idx, &mut buf)
            } else {
                row.get_text(idx, &mut buf)
            }
            .map_err(|e| PgdbError::Sql(format!("读取第 {idx} 列失败: {e}")))?;
            vals.push(if has_value {
                decode_cell(&buf, kind)
            } else {
                crate::value::SqlValue::Null
            });
        }
        out.push(DataTableRow::new(names.to_vec(), vals)?);
    }
    Ok(out)
}

/// 从 `SQLTables` 结果集中取出表名（第 3 列，TABLES 目录的标准布局）
fn table_names_from_rows(rows: Vec<DataTableRow>) -> Vec<String> {
    let mut out = Vec::new();
    for row in rows {
        let name = row
            .get("TABLE_NAME")
            .or_else(|| row.get_index(2))
            .filter(|v| !v.is_null())
            .map(|v| v.to_display_string())
            .unwrap_or_default();
        let name = name.trim().to_string();
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

/// 逐行走完结果集并把每行按 `names` / `kinds` 解释成 [`DataTableRow`]
fn fetch_all<Stmt>(
    stmt: &mut Stmt,
    names: &[String],
    kinds: &[ColKind],
) -> Result<Vec<DataTableRow>>
where
    Stmt: Statement,
{
    let ncols = names.len().min(kinds.len());
    let names = &names[..ncols];
    let kinds = &kinds[..ncols];
    let mut out: Vec<DataTableRow> = Vec::new();
    loop {
        // SAFETY: `stmt` 已成功执行且存在结果集，此处处于“游标已打开”状态。
        // `into_result_option` 把 SQLFetch 的 SQL_NO_DATA（结果集耗尽）映射为 None。
        match unsafe { stmt.fetch() }.into_result_option(stmt) {
            Ok(Some(())) => {}
            Ok(None) => break,
            Err(e) => return Err(PgdbError::Sql(format!("读取行失败: {e}"))),
        }
        let mut vals = Vec::with_capacity(ncols);
        for (i, &kind) in kinds.iter().enumerate() {
            vals.push(read_column(stmt, (i + 1) as u16, kind, COLUMN_BYTE_CAP)?);
        }
        out.push(DataTableRow::new(names.to_vec(), vals)?);
    }
    Ok(out)
}

/// 从 `SELECT ... FROM <table>` 中解析基础表名（用于目录回退）
fn table_of_sql(sql: &str) -> Option<String> {
    let upper = sql.to_ascii_uppercase();
    let pos = upper.find(" FROM ")?;
    let rest = sql[pos + 6..].trim();
    let token = rest.split_whitespace().next()?;
    let token = token.trim_matches(['[', ']', '"', '\'']);
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// 通过 `DESCRIBE TABLE <t>` 取列定义（mdbtools 的 SQL 层实现）。
///
/// mdbtools 的 `SQLColumns` 目录函数**永远返回 0 行**（即使对纯 ASCII 表名），
/// 而 `SELECT * ... WHERE 1 = 0` 只能拿到类型、拿不到长度。`DESCRIBE TABLE`
/// 直接读 MSysObjects 的表定义页，返回三元组：
///
/// | 列 1 | 列 2 | 列 3 |
/// |------|------|------|
/// | 列名 | Access 类型名 | 字节宽度 |
///
/// 这是 mdbtools 下唯一能同时拿到**列名 + 类型 + 长度**的途径，
/// 且对中文表名（如 `附加`）同样有效。
fn describe_table_columns(conn: &Connection<'static>, table: &str) -> Result<Vec<ColumnDef>> {
    let quoted = crate::sql::quote_ident(table);
    let sql = format!("DESCRIBE TABLE {quoted}");
    let Some(mut stmt) = exec_direct_bridged(conn, &sql)
        .map_err(|e| PgdbError::Sql(format!("{table}: DESCRIBE 失败: {e}")))?
    else {
        return Ok(Vec::new());
    };
    let (names, types) = describe_cursor(&mut stmt)?;
    let kinds: Vec<ColKind> = types.iter().map(kind_of_datatype).collect();
    let rows = read_rows_statement(&mut stmt, &names, &kinds)?;

    let mut out = Vec::new();
    for row in &rows {
        // 结果集固定 3 列；列名在不同版本可能为空，故按位置取值更稳。
        let Some(name) = row.get_index(0).and_then(text_of) else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let type_name = row.get_index(1).and_then(text_of).unwrap_or_default();
        // 第 3 列是字节宽度：Text 需除以 2 才是字符数（Jet 用 UTF-16 存储）
        let bytes = row.get_index(2).and_then(|v| v.to_i64()).unwrap_or(0);
        let size = match type_name.to_ascii_uppercase().as_str() {
            "TEXT" | "MEMO" | "VARCHAR" | "CHAR" | "LONGCHAR" if bytes > 0 => {
                // 向上取整，避免 MSRV 限制（div_ceil 需 Rust 1.73）
                Some((bytes as usize + 1) / 2)
            }
            _ if bytes > 0 => Some(bytes as usize),
            _ => None,
        };
        let mut def = ColumnDef {
            name: name.clone(),
            sql_type: type_name.to_ascii_uppercase(),
            size,
            scale: None,
            nullable: true,
            is_auto: is_auto_column(&name),
            kind: crate::field::FieldType::from_sql_type(&type_name),
        };
        if matches!(def.kind, crate::field::FieldType::Blob) {
            def.sql_type = "OLE".into();
        }
        out.push(def);
    }
    Ok(out)
}

/// 值转字符串（Null 返回 None）
fn text_of(v: &SqlValue) -> Option<String> {
    if v.is_null() {
        None
    } else {
        Some(v.to_display_string())
    }
}

/// 与 [`read_rows_cursor`] 等价，但作用于裸 `StatementImpl`（逐行 `SQLFetch` + `SQLGetData`）。
///
/// mdbtools 不支持 `SQLExtendedFetch` / 行数组，因此不能用 `Cursor::next_row`。
fn read_rows_statement<Stmt: Statement + AsStatementRef>(
    stmt: &mut Stmt,
    names: &[String],
    kinds: &[ColKind],
) -> Result<Vec<DataTableRow>> {
    let ncols = names.len().min(kinds.len());
    let names = &names[..ncols];
    let kinds = &kinds[..ncols];
    let mut out: Vec<DataTableRow> = Vec::new();
    loop {
        match unsafe { stmt.fetch() }.into_result_option(&*stmt) {
            Ok(Some(())) => {}
            Ok(None) => break,
            Err(e) => return Err(PgdbError::Sql(format!("读取行失败: {e}"))),
        }
        let mut vals = Vec::with_capacity(ncols);
        for (i, &kind) in kinds.iter().enumerate() {
            let idx = (i + 1) as u16;
            let cell = if kind == ColKind::Binary {
                stmt.get_data_binary(idx, COLUMN_BYTE_CAP)?
            } else {
                stmt.get_data_text(idx, COLUMN_BYTE_CAP)?
            };
            vals.push(match cell {
                Some(buf) => decode_cell(&buf, kind),
                None => SqlValue::Null,
            });
        }
        out.push(DataTableRow::new(names.to_vec(), vals)?);
    }
    Ok(out)
}

/// 通过空结果集 + `SQLDescribeCol` 探测列定义。
///
/// 用于 `SQLColumns` 与 `DESCRIBE TABLE` 都不可用时的最后回退。
/// 查询写成不返回行的形式，避免拉取真实数据。
fn describe_columns_by_query(conn: &Connection<'static>, table: &str) -> Result<Vec<ColumnDef>> {
    let quoted = crate::sql::quote_ident(table);
    // Jet 用 `WHERE 1=0`；Windows 上 `SELECT TOP 1` 更利于部分驱动返回列描述
    let sql = format!("SELECT * FROM {quoted} WHERE 1 = 0");
    let mut stmt = exec_direct_bridged(conn, &sql)
        .map_err(|e| PgdbError::Sql(format!("{table}: 探测列失败: {e}")))?
        .ok_or_else(|| PgdbError::Sql(format!("{table}: 探测列时没有返回结果集")))?;
    let (names, types) = describe_cursor(&mut stmt)?;
    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if name.trim().is_empty() {
            continue;
        }
        out.push(column_def_from_type(name, &types[i]));
    }
    Ok(out)
}

/// 由 ODBC 目录行构造 [`ColumnDef`]
fn column_def_from_catalog_row(row: &DataTableRow) -> Option<ColumnDef> {
    let name = row_text(row, "COLUMN_NAME")?;
    if name.trim().is_empty() {
        return None;
    }
    let type_name = row_text(row, "TYPE_NAME").unwrap_or_default();
    let mut def = ColumnDef {
        name: name.clone(),
        sql_type: type_name.clone(),
        size: row_i64(row, "COLUMN_SIZE").and_then(|v| usize::try_from(v).ok()),
        scale: row_i64(row, "DECIMAL_DIGITS").and_then(|v| usize::try_from(v).ok()),
        nullable: row_i64(row, "NULLABLE").map(|v| v != 0).unwrap_or(true),
        is_auto: is_auto_column(&name),
        kind: crate::field::FieldType::from_sql_type(&type_name),
    };
    if matches!(def.kind, crate::field::FieldType::Blob) {
        def.sql_type = "OLE".into();
    }
    Some(def)
}

/// 由 `DataType` 构造 [`ColumnDef`]
fn column_def_from_type(name: &str, dt: &DataType) -> ColumnDef {
    let type_name = sql_type_name(dt);
    ColumnDef {
        name: name.to_string(),
        sql_type: type_name.clone(),
        size: match dt {
            DataType::Varchar { length }
            | DataType::Char { length }
            | DataType::WVarchar { length }
            | DataType::WChar { length }
            | DataType::LongVarchar { length }
            | DataType::Binary { length }
            | DataType::Varbinary { length }
            | DataType::LongVarbinary { length } => length.map(|l| l.get()),
            _ => None,
        },
        scale: None,
        nullable: true,
        is_auto: is_auto_column(name),
        kind: crate::field::FieldType::from_sql_type(&type_name),
    }
}

/// `DataType` -> Jet 方言的类型名（供 [`FieldType::from_sql_type`] 与展示使用）
fn sql_type_name(dt: &DataType) -> String {
    match dt {
        DataType::Unknown => "UNKNOWN",
        DataType::Binary { .. } | DataType::LongVarbinary { .. } | DataType::Varbinary { .. } => {
            "OLE"
        }
        DataType::Bit => "BIT",
        DataType::Char { .. } | DataType::Varchar { .. } | DataType::WVarchar { .. }
        | DataType::WChar { .. } | DataType::LongVarchar { .. } => "TEXT",
        DataType::Date => "DATETIME",
        DataType::Time { .. } | DataType::Timestamp { .. } => "DATETIME",
        DataType::Decimal { .. } | DataType::Numeric { .. } => "DECIMAL",
        DataType::Double | DataType::Float { .. } | DataType::Real => "DOUBLE",
        DataType::Integer => "LONGINT",
        DataType::SmallInt => "SHORT",
        DataType::TinyInt => "BYTE",
        DataType::BigInt => "BIGINT",
        DataType::Other { .. } => "UNKNOWN",
    }
    .to_string()
}

/// 通过 ODBC `SQLColumns` 目录函数取得某表的列定义（跨驱动最稳健的元数据来源，
/// mdbtools 下 `describe_col` 对 Blob 列会失败，而目录函数不受影响）。
fn catalog_column_defs(conn: &Connection<'static>, table: &str) -> Result<Vec<ColumnDef>> {
    let mut outer = conn
        .columns("", "", table, "%")
        .map_err(|e| PgdbError::Sql(format!("读取列目录失败: {e}")))?;
    // 目录结果集是标准 18 列结构，列名/类型可直接取得
    let (names, types) = describe_cursor(&mut outer)?;
    let kinds: Vec<ColKind> = types.iter().map(kind_of_datatype).collect();
    let rows = read_rows_cursor(&mut outer, &names, &kinds)?;
    Ok(rows.iter().filter_map(column_def_from_catalog_row).collect())
}

/// 取列的文本值（空/Null 返回 None）
fn row_text(row: &DataTableRow, name: &str) -> Option<String> {
    row.get(name)
        .filter(|v| !v.is_null())
        .map(|v| v.to_display_string())
}

/// 取列的整数值
fn row_i64(row: &DataTableRow, name: &str) -> Option<i64> {
    row.get(name).and_then(|v| v.to_i64())
}

/// 批量读取（绑定列式缓冲 + `SQLExtendedFetch`）：性能更好，主流驱动均支持
fn read_rows_bulk(
    mut cursor: impl Cursor,
    descs: Option<Vec<BufferDesc>>,
) -> Result<Vec<DataTableRow>> {
    let ncols = cursor
        .num_result_cols()
        .map_err(|e| PgdbError::Sql(e.to_string()))? as usize;
    let (names, types) = describe_cursor(&mut cursor)?;
    // 调用方指定缓冲（如 SQLTables 的固定 5 列）时优先使用，否则按列类型推导
    let descs = match descs {
        Some(d) => d,
        None => types
            .iter()
            .map(|dt| match dt {
                DataType::Binary { length }
                | DataType::Varbinary { length }
                | DataType::LongVarbinary { length } => BufferDesc::Binary {
                    length: sane_length(length.map(|l| l.get()), MAX_BINARY_LEN),
                },
                DataType::Varchar { length }
                | DataType::WVarchar { length }
                | DataType::Char { length }
                | DataType::WChar { length }
                | DataType::LongVarchar { length } => BufferDesc::Text {
                    max_str_len: sane_length(length.map(|l| l.get()), MAX_TEXT_LEN),
                },
                _ => BufferDesc::Text {
                    max_str_len: MAX_TEXT_LEN,
                },
            })
            .collect::<Vec<_>>(),
    };
    let mut buffers = ColumnarAnyBuffer::from_descs(BATCH_SIZE, descs);
    let mut row_set = cursor
        .bind_buffer(&mut buffers)
        .map_err(|e| PgdbError::Sql(e.to_string()))?;
    let mut out: Vec<DataTableRow> = Vec::new();
    while let Some(batch) = row_set
        .fetch()
        .map_err(|e| PgdbError::Sql(e.to_string()))?
    {
        for row_idx in 0..batch.num_rows() {
            let mut vals = Vec::with_capacity(ncols);
            for (col_idx, col_type) in types.iter().enumerate().take(ncols) {
                let raw: Option<&[u8]> = match batch.column(col_idx) {
                    AnySlice::Text(view) => view.get(row_idx),
                    AnySlice::Binary(view) => view.get(row_idx),
                    AnySlice::I32(s) => {
                        vals.push(crate::value::SqlValue::I32(s[row_idx]));
                        continue;
                    }
                    AnySlice::I64(s) => {
                        vals.push(crate::value::SqlValue::I64(s[row_idx]));
                        continue;
                    }
                    AnySlice::F64(s) => {
                        vals.push(crate::value::SqlValue::F64(s[row_idx]));
                        continue;
                    }
                    _ => None,
                };
                vals.push(match raw {
                    None => crate::value::SqlValue::Null,
                    Some(bytes) => decode_cell(bytes, kind_of_datatype(col_type)),
                });
            }
            out.push(DataTableRow::new(names.clone(), vals)?);
        }
    }
    Ok(out)
}

/// 列的逻辑类型：批量路径由 ODBC `DataType` 推导，逐行回退路径由目录类型名推导。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColKind {
    Binary,
    Integer,
    Float,
    Bool,
    Date,
    Text,
}

// ------------------------------------------------------------------ 代码页桥接
//
// 关于「中文表名/字段值乱码」的根因与对策
// ----------------------------------------
// odbc-api 以 `narrow` 特性编译（见 Cargo.toml），全程调用 `SQLExecDirectA` /
// `SQLGetData` 这类窄字符 API，传给驱动的字节就是 Rust 字符串的 **UTF-8** 字节。
//
// | 环境 | 驱动把窄字节当什么 | 结果 |
// |------|--------------------|------|
// | Linux + mdbtools | UTF-8 | `界址点` 原样返回，正常 |
// | Windows + ACE/Jet | 本地 ANSI 代码页（简中 = CP936/GBK） | 把 UTF-8 误当 GBK 解码成 UTF-16 再按 GBK 编回，字节被改写（`界址点` → `鐣屽潃鐐`，末字节 `B9` 变 `3F`） |
//
// 微软文档对此有明确说明：ODBC 3.5+ 的 Driver Manager 只提供**有限**的
// Unicode↔ANSI 映射；ANSI 应用访问 Jet 4.0 时驱动只能暴露
// `SQL_CHAR/SQL_VARCHAR/SQL_LONGVARCHAR`，且该限制同样适用于
// "old formats ... with the Jet 4.0 Database Engine"。
//
// 该往返变换是**双射**的（GBK 对任意字节序列有定义、可逆），因此只要在
// Windows 上把每个进出驱动的字符串都做一次“逆变换”，乱码即可完全消除：
//
// - 出：`UTF-8 字节 --按 CP936 解码--> 文本 --按 CP936 编码--> 字节`（送驱动）
// - 入：驱动返回字节 --按 CP936 解码--> 文本 --按 CP936 编码--> 原始 UTF-8 字节
//
// 反向（入）方向做了“是否为合法 UTF-8”的前置判断，因此对纯 ASCII 以及
// 本来就返回 UTF-8 的驱动（部分 ACE 版本）是无损直通，不会二次破坏。

/// 按指定代码页把字节解码成文本。
#[cfg(windows)]
fn decode_with_code_page(bytes: &[u8], code_page: u32) -> Option<String> {
    extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            dw_flags: u32,
            lp_multibyte_str: *const u8,
            cb_multibyte: i32,
            lp_wide_char_str: *mut u16,
            cch_wide_char: i32,
        ) -> i32;
    }
    if bytes.is_empty() {
        return Some(String::new());
    }
    // 先问长度（cch_wide_char = 0）
    let need = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        )
    };
    if need <= 0 {
        return None;
    }
    let mut wide = vec![0u16; need as usize];
    let got = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            need,
        )
    };
    if got <= 0 {
        return None;
    }
    wide.truncate(got as usize);
    Some(String::from_utf16_lossy(&wide))
}

/// 把文本按指定代码页编码成字节。
#[cfg(windows)]
fn encode_with_code_page(text: &str, code_page: u32) -> Option<Vec<u8>> {
    extern "system" {
        fn WideCharToMultiByte(
            code_page: u32,
            dw_flags: u32,
            lp_wide_char_str: *const u16,
            cch_wide_char: i32,
            lp_multibyte_str: *mut u8,
            cb_multibyte: i32,
            lp_default_char: *const u8,
            lp_used_default_char: *mut i32,
        ) -> i32;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        return Some(Vec::new());
    }
    // 先问长度（cb_multibyte = 0）
    let need = unsafe {
        WideCharToMultiByte(
            code_page,
            0,
            wide.as_ptr(),
            wide.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if need <= 0 {
        return None;
    }
    let mut out = vec![0u8; need as usize];
    let got = unsafe {
        WideCharToMultiByte(
            code_page,
            0,
            wide.as_ptr(),
            wide.len() as i32,
            out.as_mut_ptr(),
            need,
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if got <= 0 {
        return None;
    }
    out.truncate(got as usize);
    Some(out)
}

/// 进程当前的 ANSI 代码页（Windows 简中/繁中为 936/950）。
///
/// 可用环境变量 `PGDB_ANSI_CP` 覆盖（例如驱动装了非默认 ISAM 时）。
#[cfg(windows)]
fn ansi_code_page() -> u32 {
    use std::sync::OnceLock;
    static CP: OnceLock<u32> = OnceLock::new();
    *CP.get_or_init(|| {
        if let Ok(v) = std::env::var("PGDB_ANSI_CP") {
            if let Ok(n) = v.trim().parse::<u32>() {
                return n;
            }
        }
        const CP_ACP: u32 = 0; // 让 API 用系统当前 ANSI 代码页
        unsafe {
            // GetACP 返回当前 ANSI 代码页号（无参数）
            extern "system" {
                fn GetACP() -> u32;
            }
            let got = GetACP();
            if got == 0 {
                CP_ACP
            } else {
                got
            }
        }
    })
}

/// 出站：把 UTF-8 文本转成驱动期望的窄字节（Windows 上为 ANSI 代码页字节）。
///
/// 非 Windows 平台直接返回 UTF-8（mdbtools 期望 UTF-8）。
fn encode_outbound(text: &str) -> Vec<u8> {
    #[cfg(windows)]
    {
        match encode_with_code_page(text, ansi_code_page()) {
            Some(bytes) => bytes,
            // 编码失败（含无法映射的字符）时退回 UTF-8，至少不 panic
            None => text.as_bytes().to_vec(),
        }
    }
    #[cfg(not(windows))]
    {
        text.as_bytes().to_vec()
    }
}

/// 入站：把驱动返回的窄字节还原成 UTF-8 文本。
///
/// - 已经是合法 UTF-8（Linux/mdbtools、部分 ACE 版本）→ 原样使用；
/// - 否则按 Windows ANSI 代码页 decode 出文本，再把文本的 UTF-8 字节交回上层。
fn decode_inbound(bytes: &[u8]) -> String {
    // 纯 ASCII 一定两种解释一致，省掉一次系统调用
    if bytes.is_ascii() {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    {
        if let Some(text) = decode_with_code_page(bytes, ansi_code_page()) {
            return text;
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

// ------------------------------------------------------------------ 诊断消息桥
//
// 为什么需要单独处理诊断消息
// --------------------------
// `odbc_api::handles::logging::log_diagnostics` 是 odbc-api 在
// `SQL_SUCCESS_WITH_INFO` / `SQL_ERROR` 时自动调用的日志钩子，它内部用
// `slice_to_cow_utf8` → `String::from_utf8_lossy` 解释驱动返回的消息。
//
// 在 Windows + 中文 ACE 驱动下，诊断消息是 **GBK 字节**，被 lossy 解成：
//
// ```
// 字符串数据，右截断        ->  �ַ������ݣ��ҽض�
// 数值超出范围 在行号 17 (MHigh) 中 ->  ��ֵ������Χ ���к� 17 (MHigh) ��
// 无效的 SQL 语句；在此 'DELETE'... ->  ��Ч�� SQL ��䣻�ڴ� 'DELETE'...
// ```
//
// 这条路径**绕过了**本模块的 [`decode_inbound`]，因此之前的数据面修复
// 没能覆盖它。对策：自己实现一份 `Diagnostics` 读取 + 日志输出，
// 消息文本统一过 [`decode_inbound`]。

/// 人类可读的 ODBC SQLSTATE 说明（高频状态码）。
///
/// 驱动只给 5 位状态码，遇到乱码时连"哪里错"都判断不出来；
/// 这里补一份中文说明，即使消息本身仍不可读也能定位问题类别。
fn sqlstate_hint(state: &str) -> &'static str {
    match state {
        "01004" => "字符串数据被右截断（结果列缓冲区不足，数据本身未丢失）",
        "01000" => "一般性警告",
        "07006" => "数据类型转换错误",
        "22001" => "字符串数据被右截断（写入值超过字段长度）",
        "22003" => "数值超出范围（写入值超过字段容量）",
        "22005" => "赋值时发生数据类型转换错误",
        "22018" => "字符值无法转换为目标类型",
        "23000" => "违反完整性约束",
        "24000" => "游标状态无效",
        "37000" => "SQL 语法错误",
        "42000" => "SQL 语法错误或访问违规（语句无法被解析/执行）",
        "42S02" => "找不到表或视图",
        "42S22" => "找不到列",
        "HY000" => "一般性驱动错误",
        "HY001" => "内存分配失败",
        "HY010" => "函数序列错误（调用顺序不正确）",
        "HY090" => "字符串或缓冲区长度无效",
        "HY092" => "属性/选项标识符无效",
        "IM001" => "驱动不支持该函数",
        "IM002" => "找不到数据源名称且未指定默认驱动",
        "IM014" => "DSN 中指定的驱动与应用程序位数不匹配（32/64 位）",
        "IM015" => "驱动与应用程序位数不匹配",
        _ => "",
    }
}

/// 读取并记录一条句柄上的全部诊断记录（消息经 [`decode_inbound`] 桥接）。
///
/// 与 `odbc_api::handles::log_diagnostics` 的区别只有一点：**文本解码**。
/// 其余语义（逐条读取、`i16::MAX` 防溢出、只在 WARN 级别以上工作）保持一致。
fn log_diagnostics_bridged(handle: &(impl odbc_api::handles::Diagnostics + ?Sized)) {
    use log::Level;

    if log::max_level() < Level::Warn {
        return;
    }

    let mut messages: Vec<u8> = Vec::with_capacity(512);
    let mut rec_number: i16 = 1;
    loop {
        // 每轮先把缓冲用 0 填满（`diagnostic_record` 要求传入可写切片）
        let cap = messages.capacity().max(512);
        messages.clear();
        messages.resize(cap, 0);

        let Some(mut result) = handle.diagnostic_record(rec_number, &mut messages) else {
            break;
        };
        let mut len: usize = result.text_length.try_into().unwrap_or(0);

        // 消息比缓冲长：按指示的实际长度扩容后重取一次
        if len > messages.len() {
            messages.resize(len + 1, 0);
            if let Some(r2) = handle.diagnostic_record(rec_number, &mut messages) {
                result = r2;
                len = result.text_length.try_into().unwrap_or(0);
            }
        }
        // 驱动可能用 0 填充尾部
        while len > 0 && messages.get(len - 1) == Some(&0) {
            len -= 1;
        }
        let raw = &messages[..len.min(messages.len())];

        let state = result.state.as_str();
        let message = if raw.is_empty() {
            "(无消息文本)".to_string()
        } else {
            decode_inbound(raw)
        };
        let hint = sqlstate_hint(state);
        if hint.is_empty() {
            log::warn!("ODBC [{state}] (native {}) {message}", result.native_error);
        } else {
            log::warn!(
                "ODBC [{state}] (native {}) {message} —— {hint}",
                result.native_error
            );
        }

        if rec_number == i16::MAX {
            log::warn!("诊断记录过多，已截断输出");
            break;
        }
        rec_number += 1;
    }
}

/// 取句柄上的第一条诊断记录，消息经 [`decode_inbound`] 桥接后返回
/// `(SQLSTATE, native_error, message)`。
///
/// 用于把 `odbc_api::Error` 里那条同样被 lossy 解码过的消息替换成正确文本。
fn first_diagnostic_bridged(
    handle: &(impl odbc_api::handles::Diagnostics + ?Sized),
) -> Option<(String, i32, String)> {
    let mut buf: Vec<u8> = vec![0; 512];
    let result = handle.diagnostic_record(1, &mut buf)?;
    let mut len: usize = result.text_length.try_into().unwrap_or(0);
    if len > buf.len() {
        buf.resize(len + 1, 0);
        if let Some(r2) = handle.diagnostic_record(1, &mut buf) {
            len = r2.text_length.try_into().unwrap_or(len);
        }
    }
    while len > 0 && buf.get(len - 1) == Some(&0) {
        len -= 1;
    }
    let message = decode_inbound(&buf[..len.min(buf.len())]);
    Some((result.state.as_str().to_string(), result.native_error, message))
}

/// 诊断消息接管开关：暂时压低 `log::max_level` 以阻止 odbc-api 自带的
/// `log_diagnostics` 输出乱码消息，改由本模块用 [`log_diagnostics_bridged`]
/// 输出版正文本。
///
/// # 为什么不能只改日志格式
///
/// odbc-api 在每次 `SQLExecDirect` / `SQLFetch` 返回 `SQL_SUCCESS_WITH_INFO`
/// 时**主动**调用 `warn!("{}", rec)`，消息文本在它内部就已用
/// `String::from_utf8_lossy` 解码完毕（GBK 消息被解成 `�ַ������ݣ��ҽض�`）。
/// 这个调用发生在库内部，应用层拿不到插手的时机。
///
/// 但该函数开头有一句 `if log::max_level() < Level::Warn { return; }`，
/// 于是我们可以在调用驱动期间把 `max_level` 压到 `Error`，
/// 让 odbc-api 静默跳过；随后自己用 `Diagnostics` 接口重新读一遍原始字节，
/// 经 [`decode_inbound`] 桥接后再 `warn!` 输出。
///
/// 该结构体在 `Drop` 时恢复原级别，保证异常路径（`?` 提前返回）下也不会泄漏状态。
struct DiagnosticCapture {
    /// 进入前日志框架的最大级别
    previous: log::LevelFilter,
}

impl DiagnosticCapture {
    /// 进入"静默 odbc-api 日志"状态
    fn new() -> Self {
        let previous = log::max_level();
        if previous >= log::LevelFilter::Warn {
            // 压低到 Error：odbc-api 的 `max_level() < Warn` 早退条件成立
            log::set_max_level(log::LevelFilter::Error);
        }
        Self { previous }
    }

    /// 结束静默，并把句柄上的诊断消息用正确的编码输出
    fn finish(self, handle: &(impl odbc_api::handles::Diagnostics + ?Sized)) {
        // 先恢复级别，否则下面的 warn! 也会被压掉
        log::set_max_level(self.previous);
        std::mem::forget(self);
        log_diagnostics_bridged(handle);
    }

    /// 只恢复日志级别，不输出诊断。
    ///
    /// 用于"错误由上层 `Error` 携带、无需在此重复打印"的场景，
    /// 避免同一问题被打印两遍。
    fn finish_silent(self) {
        log::set_max_level(self.previous);
        std::mem::forget(self);
    }
}

impl Drop for DiagnosticCapture {
    fn drop(&mut self) {
        // 正常路径下 `finish` 已 `forget` 掉本对象；
        // 走到这里说明是 `?` 提前返回等异常路径，仅恢复级别即可。
        log::set_max_level(self.previous);
    }
}

/// 执行一条会产生结果集的语句，并**接管诊断输出**。
///
/// 与 `conn.execute()` / 裸 `exec_direct` 的区别：
/// 调用驱动期间把 `log::max_level` 压到 `Error`，让 odbc-api 内部的
/// `log_diagnostics` 静默跳过，随后用 [`first_diagnostic_bridged`] 读取
/// 原始字节并按正确代码页输出 —— 这是让驱动报错的中文可读的关键。
fn exec_direct_bridged<'a>(
    conn: &'a Connection<'static>,
    sql: &str,
) -> Result<Option<RawStmt<'a>>> {
    let preallocated = conn
        .preallocate()
        .map_err(|e| PgdbError::Sql(format!("分配语句句柄失败: {e}")))?;
    let mut stmt = preallocated.into_statement();
    let text = sql_text(sql);

    let capture = DiagnosticCapture::new();
    // SAFETY: `stmt` 是刚分配、未绑定任何缓冲的语句句柄；`text` 在整个调用期间有效。
    let rc = unsafe { stmt.exec_direct(&text) };
    capture.finish(&stmt);

    match rc {
        odbc_api::handles::SqlResult::Success(())
        | odbc_api::handles::SqlResult::SuccessWithInfo(()) => {}
        odbc_api::handles::SqlResult::NoData => return Ok(None),
        _ => {
            let detail = first_diagnostic_bridged(&stmt)
                .map(|(state, native, msg)| format_diagnostic(&state, native, &msg))
                .unwrap_or_else(|| "驱动未提供诊断信息".to_string());
            return Err(PgdbError::Sql(format!("{sql} => {detail}")));
        }
    }

    let ncols = stmt
        .num_result_cols()
        .into_result(&stmt)
        .map_err(|e| PgdbError::Sql(format!("{sql} => 读取列数失败: {e}")))?;
    if ncols <= 0 {
        return Ok(None);
    }
    Ok(Some(stmt))
}

/// 低层 ODBC 语句句柄的别名（仅用于诊断读取）
type RawStmt<'a> = odbc_api::handles::StatementImpl<'a>;

/// 把 `(state, native, message)` 格式化成一行带 SQLSTATE 说明的诊断文本
fn format_diagnostic(state: &str, native: i32, message: &str) -> String {
    let hint = sqlstate_hint(state);
    if hint.is_empty() {
        format!("[{state}] native {native}: {message}")
    } else {
        format!("[{state}] native {native}: {message}（{hint}）")
    }
}

/// 自持有的 `SqlText`：先把文本编码成窄字节，再借用自身数组。
///
/// 避免把临时 `Vec<u8>` 的引用传出语句范围。
struct OwnedSqlText {
    bytes: Vec<u8>,
}

impl OwnedSqlText {
    fn new(s: &str) -> Self {
        Self {
            bytes: encode_outbound(s),
        }
    }

    /// 借用已编码的字节构造 `SqlText`（生命周期绑在 `self` 上）。
    fn as_text(&self) -> Result<SqlText<'_>> {
        let s = std::str::from_utf8(&self.bytes)
            .map_err(|e| PgdbError::Backend(format!("连接参数编码异常: {e}")))?;
        Ok(SqlText::new(s))
    }
}

/// 构造 `SqlText`：按目标驱动的编码把文本转成窄字节。
///
/// `narrow` 特性下 `SqlText` 内部只是 `&str`，调用时取 `as_bytes()` 传给
/// `SQLExecDirectA`，因此这里直接给 UTF-8 原串；Windows 的 ANSI 桥接在
/// [`encode_outbound`] 负责的入站/出站边界上单独完成。
fn sql_text(text: &str) -> SqlText<'_> {
    SqlText::new(text)
}

/// 从 ODBC `DataType` 推导 `ColKind`
fn kind_of_datatype(dt: &DataType) -> ColKind {
    match dt {
        DataType::Binary { .. }
        | DataType::Varbinary { .. }
        | DataType::LongVarbinary { .. } => ColKind::Binary,
        DataType::Integer | DataType::SmallInt | DataType::TinyInt | DataType::BigInt => {
            ColKind::Integer
        }
        DataType::Real
        | DataType::Float { .. }
        | DataType::Double
        | DataType::Numeric { .. }
        | DataType::Decimal { .. } => ColKind::Float,
        DataType::Bit => ColKind::Bool,
        DataType::Date | DataType::Timestamp { .. } | DataType::Time { .. } => ColKind::Date,
        _ => ColKind::Text,
    }
}

/// 目录 `TYPE_NAME` 字符串 -> `ColKind`
fn kind_of_typename(name: &str) -> ColKind {
    let u = name.to_ascii_uppercase();
    if u.contains("BINARY") || u.contains("OLE") || u.contains("BLOB") || u.contains("IMAGE") {
        ColKind::Binary
    } else if u.contains("INTEGER") || u.contains("LONG") || u.contains("COUNTER") || u.contains("BIGINT") {
        ColKind::Integer
    } else if u.contains("DOUBLE") || u.contains("FLOAT") || u.contains("NUMERIC") || u.contains("DECIMAL") || u.contains("CURRENCY") || u.contains("REAL") {
        ColKind::Float
    } else if u.contains("BIT") || u.contains("LOGICAL") || u.contains("YESNO") {
        ColKind::Bool
    } else if u.contains("DATE") || u.contains("TIMESTAMP") || u.contains("TIME") || u.contains("DATETIME") {
        ColKind::Date
    } else {
        ColKind::Text
    }
}

/// 由列类型把 ODBC 返回的字节解释成 SqlValue
fn decode_cell(bytes: &[u8], kind: ColKind) -> crate::value::SqlValue {
    use crate::value::SqlValue as V;
    if kind == ColKind::Binary {
        return V::Binary(bytes.to_vec());
    }
    // 文本列必须经过入站解码：Windows 的 ACE/Jet 返回的是 ANSI 代码页字节，
    // 直接 `from_utf8_lossy` 会把中文表名/字段值变成 `鐣屽潃鐐` 这类乱码。
    let text = decode_inbound(bytes).trim().to_string();
    if text.is_empty() {
        return V::Null;
    }
    match kind {
        ColKind::Integer => match text.parse::<i64>() {
            Ok(v) => V::I64(v),
            Err(_) => V::Text(text),
        },
        ColKind::Float => match text.parse::<f64>() {
            Ok(v) => V::F64(v),
            Err(_) => V::Decimal(text),
        },
        ColKind::Bool => V::Bool(matches!(text.as_str(), "1" | "true" | "True" | "TRUE")),
        ColKind::Date => match crate::value::parse_datetime(&text) {
            Some(d) => V::DateTime(d),
            None => V::Text(text),
        },
        ColKind::Text | ColKind::Binary => V::Text(text),
    }
}

/// 选择合理的缓冲长度
fn sane_length(length: Option<usize>, fallback: usize) -> usize {
    match length {
        Some(0) => fallback,
        Some(l) => l.clamp(1, fallback * 4).max(1),
        None => fallback,
    }
}

/// Access 自增列的启发式判断
fn is_auto_column(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "objectid" | "oid" | "object_id" | "fid" | "id"
    )
}

/// 二进制值改走参数绑定的阈值（字节）。
///
/// # 为什么需要这个阈值
///
/// 二进制（几何 Shape / OLE）写入原本走 Jet SQL 的十六进制字面量 `0x0102...`，
/// 每个字节占 2 个**字符**，而 Jet 的 **SQL 语句总长度上限约 64,000 字符**
/// （Access 官方规格："Number of characters in an SQL statement approximately
/// 64,000"）。超出后驱动报：
///
/// ```text
/// State: 22003, Native error: 34
/// Message: 数值超出范围 在行号 17 (MHigh) 中
/// ```
///
/// `MHigh` 是 Jet 内部的 Memo-High 记号，指的正是超长的字面量 token。
/// 因此在 Windows 上把超过本阈值的二进制值改用 [`exec_with_binary_params`]
/// 的**参数绑定**路径——参数不占 SQL 文本长度，几 MB 的几何体也能一次写入。
const MAX_HEX_LITERAL_BYTES: usize = 24 * 1024;

/// 一条写入语句里，哪些值走参数绑定、哪些内联为字面量。
///
/// 大二进制值（几何 Shape / OLE）如果内联成 `0x...`，会撞上 Jet 的
/// 64,000 字符 SQL 上限（报 `22003 / MHigh`）。因此把它们挑出来绑定为参数：
/// 参数不占 SQL 文本长度，且避免了把几万个字符再编码一遍。
fn split_bindable(values: &[(String, crate::value::SqlValue)]) -> (Vec<(String, Vec<u8>)>, bool) {
    let mut bindable = Vec::new();
    for (name, value) in values {
        if let Some(bytes) = value.to_binary() {
            if bytes.len() > MAX_HEX_LITERAL_BYTES {
                bindable.push((name.clone(), bytes.to_vec()));
            }
        }
    }
    let any = !bindable.is_empty();
    (bindable, any)
}

/// 把值渲染成 SQL 片段：需要绑定的大二进制值换成 `?` 占位符
fn render_value(
    name: &str,
    value: &crate::value::SqlValue,
    bound_names: &[String],
) -> String {
    if let Some(bytes) = value.to_binary() {
        if bytes.len() > MAX_HEX_LITERAL_BYTES && bound_names.iter().any(|n| n == name) {
            return "?".to_string();
        }
    }
    literal(value)
}

/// 执行 UPDATE，大二进制值走参数绑定
fn exec_write(
    conn: &Connection<'static>,
    table: &str,
    sets: &[(String, crate::value::SqlValue)],
    filter: &Predicate,
) -> Result<()> {
    if sets.is_empty() {
        return Ok(());
    }
    let (bindable, has_big) = split_bindable(sets);
    if !has_big {
        // 小值：沿用字面量路径（无需参数，兼容只读/极简驱动）
        let sql = update_sql(table, sets, filter)?;
        return exec_statement(conn, &sql);
    }

    let bound_names: Vec<String> = bindable.iter().map(|(n, _)| n.clone()).collect();
    let set_sql = sets
        .iter()
        .map(|(c, v)| format!("{} = {}", crate::sql::quote_ident(c), render_value(c, v, &bound_names)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("UPDATE {} SET {set_sql}", crate::sql::quote_ident(table));
    if let Some(w) = where_clause(filter)? {
        sql.push_str(&format!(" WHERE {w}"));
    }

    let mut params: Vec<Option<&[u8]>> = Vec::with_capacity(bound_names.len());
    for (_, bytes) in &bindable {
        params.push(Some(bytes.as_slice()));
    }
    exec_with_binary_params(conn, &sql, &bindable)
}

/// 执行 INSERT，大二进制值走参数绑定
fn exec_insert(
    conn: &Connection<'static>,
    table: &str,
    values: &[(String, crate::value::SqlValue)],
) -> Result<()> {
    let (bindable, has_big) = split_bindable(values);
    if !has_big {
        let cols: Vec<String> = values.iter().map(|(k, _)| k.clone()).collect();
        let rendered: Vec<String> = values.iter().map(|(_, v)| literal(v)).collect();
        let sql = insert_sql(table, &cols, rendered);
        return exec_statement(conn, &sql);
    }

    let bound_names: Vec<String> = bindable.iter().map(|(n, _)| n.clone()).collect();
    let cols_sql = values
        .iter()
        .map(|(c, _)| crate::sql::quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let vals_sql = values
        .iter()
        .map(|(c, v)| render_value(c, v, &bound_names))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO {} ({cols_sql}) VALUES ({vals_sql})",
        crate::sql::quote_ident(table)
    );
    exec_with_binary_params(conn, &sql, &bindable)
}

/// 不带参数的普通执行
fn exec_statement(conn: &Connection<'static>, sql: &str) -> Result<()> {
    let capture = DiagnosticCapture::new();
    let outcome = conn.execute(sql, ());
    match outcome {
        Ok(_) => {
            capture.finish_silent();
            Ok(())
        }
        Err(e) => {
            capture.finish_silent();
            Err(PgdbError::Sql(format!("{sql} => {e}")))
        }
    }
}

/// 用绑定的二进制参数执行语句（`?` 占位符按 `bindable` 顺序对应）
fn exec_with_binary_params(
    conn: &Connection<'static>,
    sql: &str,
    bindable: &[(String, Vec<u8>)],
) -> Result<()> {
    use odbc_api::IntoParameter;

    let capture = DiagnosticCapture::new();
    let outcome = match bindable.len() {
        1 => {
            let p = bindable[0].1.as_slice().into_parameter();
            conn.execute(sql, &p)
        }
        2 => {
            let a = bindable[0].1.as_slice().into_parameter();
            let b = bindable[1].1.as_slice().into_parameter();
            conn.execute(sql, (&a, &b))
        }
        3 => {
            let a = bindable[0].1.as_slice().into_parameter();
            let b = bindable[1].1.as_slice().into_parameter();
            let c = bindable[2].1.as_slice().into_parameter();
            conn.execute(sql, (&a, &b, &c))
        }
        n => {
            capture.finish_silent();
            return Err(PgdbError::Sql(format!(
                "一次写入的二进制字段最多支持 3 个，当前为 {n} 个；请分批更新"
            )));
        }
    };
    match outcome {
        Ok(_) => {
            capture.finish_silent();
            Ok(())
        }
        Err(e) => {
            capture.finish_silent();
            Err(PgdbError::Sql(format!("{sql} => {e}")))
        }
    }
}

/// WHERE 子句构造工具：便于上层诊断生成的 SQL
pub fn explain(filter: &Predicate) -> Result<Option<String>> {
    where_clause(filter)
}

#[cfg(test)]
mod encoding_tests {
    //! 中文编码路径的回归测试。
    //!
    //! Windows 上曾出现两类乱码：
    //!
    //! 1. **日志乱码** —— UTF-8 字节被控制台按 GBK 解读，
    //!    `要素类 BDC不动产` 显示成 `瑕佺礌绫? BDC涓嶅姩浜?`；
    //! 2. **表名乱码** —— 窄字符 API 传出的 UTF-8 被驱动按 CP936 解码成
    //!    UTF-16 再编回 ANSI，`界址点` 变成 `鐣屽潃鐐?`（末字节 `B9` 被吞成 `3F`）。
    //!
    //! 下面的用例锁定修复契约：入站侧必须把 ANSI 字节还原成 UTF-8 文本，
    //! 且对本来就是 UTF-8 的输入（Linux/mdbtools）保持无损直通。
    use super::*;

    /// 模拟 Windows CP936 驱动的往返：UTF-8 字节 --按 CP936 解码--> 文本
    ///                      --按 CP936 编码--> 新字节。
    ///
    /// 这正是驱动内部对窄字符 API 所做的变换，也是表名乱码的来源。
    /// Linux 上没有 CP936 codec，用预计算的等价样本代替（见
    /// `windows_cp936_roundtrip_is_reverted`）。
    #[cfg(windows)]
    fn cp936_roundtrip(text: &str) -> Option<Vec<u8>> {
        let decoded = decode_with_code_page(text.as_bytes(), 936)?;
        encode_with_code_page(&decoded, 936)
    }

    #[test]
    fn inbound_ascii_is_identity() {
        assert_eq!(decode_inbound(b"JZX"), "JZX");
        assert_eq!(decode_inbound(b"OBJECTID"), "OBJECTID");
        assert_eq!(decode_inbound(b""), "");
    }

    #[test]
    fn inbound_utf8_is_passed_through() {
        // Linux/mdbtools 与部分 ACE 版本直接返回 UTF-8——必须无损保留，
        // 否则会把好数据“修坏”（这是修复中最需要避免的回归）。
        for text in ["界址点", "附加", "BDC不动产", "求和", "丽丽", "啊啊"] {
            assert_eq!(
                decode_inbound(text.as_bytes()),
                text,
                "UTF-8 入站必须直通: {text}"
            );
        }
    }

    #[test]
    fn inbound_lossy_fallback_never_panics() {
        // 既非 ASCII、又非合法 UTF-8 的字节（真·乱码）不能 panic
        let junk = [0x81u8, 0x40, 0xFE, 0xFF];
        let out = decode_inbound(&junk);
        assert!(!out.is_empty());
    }

    #[test]
    fn outbound_utf8_is_stable_on_non_windows() {
        // 非 Windows 平台出站必须是 UTF-8 原字节（mdbtools 期望 UTF-8）
        let bytes = encode_outbound("界址点");
        if cfg!(windows) {
            // Windows 上应转成 ANSI 代码页字节（简中为 2 字节 GBK）
            assert_ne!(bytes, "界址点".as_bytes());
        } else {
            assert_eq!(bytes, "界址点".as_bytes());
        }
    }

    #[test]
    fn cell_decoding_keeps_chinese_values() {
        // 字段值走 `decode_cell`：UTF-8 输入必须还原成同样的字符串
        for text in ["啊啊", "到底", "11abc", "丽丽", "存储"] {
            match decode_cell(text.as_bytes(), ColKind::Text) {
                crate::value::SqlValue::Text(got) => assert_eq!(got, text),
                other => panic!("期望 Text，得到 {other:?}"),
            }
        }
    }

    #[test]
    fn binary_columns_are_never_transcoded() {
        // Shape 几何列是原始字节，绝不能走代码页转换
        let shape = vec![0x00u8, 0x00, 0x27, 0x0A, 0xFF, 0x81, 0x40];
        match decode_cell(&shape, ColKind::Binary) {
            crate::value::SqlValue::Binary(got) => assert_eq!(got, shape),
            other => panic!("期望 Binary，得到 {other:?}"),
        }
    }

    /// **核心回归**：驱动把 UTF-8 误按 CP936 往返后产生的乱码字节，
    /// 必须能被 `decode_inbound` 还原成原始中文。
    ///
    /// 这条用真实 Windows API（`MultiByteToWideChar`/`WideCharToMultiByte`）
    /// 构造乱码再还原，等价于「问题 2 表名乱码」的完整闭环验证。
    #[cfg(windows)]
    #[test]
    fn windows_cp936_roundtrip_is_reverted() {
        for text in ["界址点", "附加", "BDC不动产", "求和"] {
            // 1) 构造驱动产生的乱码字节
            let garbled = cp936_roundtrip(text).expect("CP936 往返失败");
            assert_ne!(
                garbled,
                text.as_bytes(),
                "前提：{text} 经 CP936 往返应当发生变化，否则本用例失去意义"
            );
            // 2) 修复后的入站解码必须还原原文
            assert_eq!(
                decode_inbound(&garbled),
                text,
                "CP936 乱码未能还原: {text} -> {garbled:02X?}"
            );
        }
    }

    /// 出站编码后再由驱动按 CP936 解读，应当得到原文的 UTF-16（即表名能匹配上）。
    #[cfg(windows)]
    #[test]
    fn outbound_survives_driver_ansi_interpretation() {
        for text in ["界址点", "附加", "BDC不动产", "求和"] {
            let bytes = encode_outbound(text);
            let seen_by_driver = decode_with_code_page(&bytes, ansi_code_page())
                .expect("驱动侧解读失败");
            assert_eq!(seen_by_driver, text, "出站编码后驱动看到的应当是原文");
        }
    }

    /// 字节等价映射（用于连接串）必须是可逆的
    #[cfg(windows)]
    #[test]
    fn transport_str_is_byte_preserving() {
        let raw: Vec<u8> = vec![0x44, 0x3A, 0x5C, 0xD0, 0xC5, 0xCF, 0xA2, 0x5C, 0x61];
        let s = bytes_to_transport_str(&raw);
        assert_eq!(s.as_bytes(), raw.as_slice(), "字节等价映射破坏了字节序列");
    }

    // ---------------------------------------------------------------- 诊断消息
    //
    // 用户报告的三个 SQLSTATE（01004 / 22003 / 42000）在 Windows 上消息全是
    // 乱码。修复分两步：消息文本走 `decode_inbound` 桥接（上文的编码测试覆盖），
    // 同时补一份 SQLSTATE → 中文说明的映射，即使消息本身仍不可读，
    // 也能判断问题类别。

    #[test]
    fn sqlstate_hint_covers_reported_errors() {
        // 用户实际遇到的三个状态码都必须有说明
        for state in ["01004", "22003", "42000"] {
            assert!(
                !sqlstate_hint(state).is_empty(),
                "{state} 缺少中文说明——用户报告过这个错误，必须可解读"
            );
        }
    }

    #[test]
    fn sqlstate_hint_unknown_is_empty() {
        // 未知状态码返回空串，由调用方决定是否显示
        assert!(sqlstate_hint("ZZZZZ").is_empty());
        assert!(sqlstate_hint("").is_empty());
    }

    #[test]
    fn format_diagnostic_includes_hint() {
        let line = format_diagnostic("42000", -3500, "无效的 SQL 语句");
        assert!(line.contains("42000"), "应保留 SQLSTATE: {line}");
        assert!(line.contains("-3500"), "应保留 native error: {line}");
        assert!(line.contains("无效的 SQL 语句"), "应保留消息: {line}");
        assert!(line.contains("语法错误"), "应附中文说明: {line}");
    }

    #[test]
    fn format_diagnostic_without_hint_stays_clean() {
        let line = format_diagnostic("ZZZZZ", 1, "msg");
        assert_eq!(line, "[ZZZZZ] native 1: msg");
    }

    // ------------------------------------------------------- 22003 大二进制写入
    //
    // `0xXXXX...` 十六进制字面量每字节占 2 个字符，Jet 的 SQL 语句上限约
    // 64,000 字符；超过后驱动报 `22003 native 34 数值超出范围 在行号 17
    // (MHigh) 中`。修复：超过阈值的二进制值改走参数绑定。

    #[test]
    fn hex_literal_threshold_is_within_sql_limit() {
        // 阈值换算成 SQL 字符数必须安全落在 64,000 以内（留足余量给
        // `UPDATE <表> SET <列> = 0x...  WHERE ...` 的其余部分）
        let as_sql_chars = MAX_HEX_LITERAL_BYTES * 2;
        assert!(
            as_sql_chars < 60_000,
            "阈值 {MAX_HEX_LITERAL_BYTES} 字节 = {as_sql_chars} 字符，过于接近 Jet 的 64,000 上限"
        );
    }

    #[test]
    fn small_binary_values_stay_on_literal_path() {
        // 小几何体（如单点 28 字节）不应触发参数绑定——保持对极简驱动的兼容
        let small = vec![0u8; 1024];
        let sets = vec![("SHAPE".to_string(), crate::value::SqlValue::Binary(small))];
        let (bindable, has_big) = split_bindable(&sets);
        assert!(!has_big, "1 KB 的几何体不应走参数绑定");
        assert!(bindable.is_empty());
    }

    #[test]
    fn large_binary_values_switch_to_parameters() {
        // 大几何体必须改走参数绑定，否则会撞上 64,000 字符上限
        let big = vec![0u8; MAX_HEX_LITERAL_BYTES + 1];
        let sets = vec![("SHAPE".to_string(), crate::value::SqlValue::Binary(big))];
        let (bindable, has_big) = split_bindable(&sets);
        assert!(has_big, "超过阈值的几何体必须走参数绑定");
        assert_eq!(bindable.len(), 1);
        assert_eq!(bindable[0].0, "SHAPE");
    }

    #[test]
    fn render_value_uses_placeholder_for_bound_binary() {
        // 被绑定的列在 SQL 里应渲染成 `?`，而不是又臭又长的十六进制
        let big = vec![0u8; MAX_HEX_LITERAL_BYTES + 1];
        let value = crate::value::SqlValue::Binary(big);
        let names = vec!["SHAPE".to_string()];
        assert_eq!(render_value("SHAPE", &value, &names), "?");

        // 普通文本仍然是内联字面量
        let text = crate::value::SqlValue::Text("啊啊".into());
        assert_eq!(render_value("名称", &text, &names), "'啊啊'");
    }

    #[test]
    fn render_value_keeps_literal_when_not_bound() {
        // 同为二进制但未列入绑定名单（如小值）→ 仍然内联为 0x...
        let small = vec![0x01u8, 0xAB];
        let value = crate::value::SqlValue::Binary(small);
        assert_eq!(render_value("SHAPE", &value, &[]), "0x01AB");
    }
}
