//! 工作空间工厂：对应 ArcObjects 的 `IWorkspaceFactory` / `AccessWorkspaceFactory`。

use std::sync::Arc;

use crate::datastore::{AccessMode, SqlBackend};
use crate::error::Result;

use super::workspace::{AccessWorkspace, FeatureWorkspace, WorkspaceOptions};

/// 工作空间工厂（`IWorkspaceFactory`）
pub trait WorkspaceFactory {
    /// 该工厂能处理的文件扩展名
    fn extensions(&self) -> &[&'static str];

    /// 路径是否可能由该工厂处理（基于扩展名与文件头，不打开连接）
    fn probe(&self, path: &str) -> bool;

    /// 打开工作空间
    fn open(&self, path: &str, options: Option<WorkspaceOptions>) -> Result<AccessWorkspace>;
}

/// 个人地理数据库工作空间工厂（`AccessWorkspaceFactory`）
pub struct AccessWorkspaceFactory;

/// Jet 4 (Access 2000+) 文件头签名
const JET4_SIGNATURE: &[u8] = b"Standard Jet DB";

impl AccessWorkspaceFactory {
    /// 读取文件头判断是否为 Access mdb
    pub fn looks_like_mdb(path: &str) -> bool {
        use std::io::Read;
        let Ok(mut f) = std::fs::File::open(path) else {
            return false;
        };
        let mut head = [0u8; 32];
        let Ok(n) = f.read(&mut head) else {
            return false;
        };
        head[..n]
            .windows(JET4_SIGNATURE.len())
            .any(|w| w == JET4_SIGNATURE)
    }

    /// 由已有后端（ODBC / jetdb / 内存镜像）构造工作空间
    pub fn open_from_backend(
        backend: Arc<dyn SqlBackend>,
        path: impl Into<String>,
        options: Option<WorkspaceOptions>,
    ) -> Result<AccessWorkspace> {
        match options {
            Some(o) => AccessWorkspace::open_with_options(backend, path, o),
            None => AccessWorkspace::open(backend, path),
        }
    }

    /// 以指定**读写权限**打开 `.mdb` / `.accdb`。
    ///
    /// 这是推荐的入口：由 [`AccessMode`] 在运行时决定用哪种解析方式，
    /// 因此同一个可执行文件同时支持两条路径，无需在编译期挑选 feature。
    ///
    /// - [`AccessMode::ReadOnly`]：纯 Rust 的 jetdb 解析，无需任何驱动、跨平台，仅可查询；
    /// - [`AccessMode::ReadWrite`]：ODBC 驱动解析，可读写（Windows 需安装与程序位数
    ///   匹配的 Access/ACE 驱动；Linux 下的 MDBTools 驱动本身为只读）。
    pub fn open_with_mode(
        path: &str,
        mode: AccessMode,
        options: Option<WorkspaceOptions>,
    ) -> Result<AccessWorkspace> {
        let opts = WorkspaceOptions {
            access_mode: mode,
            ..options.unwrap_or_default()
        };
        let backend = crate::datastore::open_backend(path, opts.access_mode, None)?;
        let backend: Arc<dyn SqlBackend> = Arc::from(backend);
        Self::open_from_backend(backend, path, Some(opts))
    }

    /// 打开 ODBC 数据源把 `.mdb` 作为个人地理数据库访问（读写）
    ///
    /// Windows 使用 `Driver={Microsoft Access Driver (*.mdb)}`，Linux 可用只读的
    /// MDBTools 驱动。写操作需要 Windows + ACE/Jet 驱动。
    pub fn open_odbc(path: &str, connection_string: Option<&str>) -> Result<AccessWorkspace> {
        let backend = Arc::new(crate::datastore::odbc::OdbcBackend::connect(
            path,
            connection_string,
        )?);
        let opts = WorkspaceOptions {
            access_mode: AccessMode::ReadWrite,
            ..Default::default()
        };
        Self::open_from_backend(backend, path, Some(opts))
    }

    /// 以**只读**权限打开：使用纯 Rust 的 jetdb 解析，无需任何驱动、跨平台。
    pub fn open_read_only(path: &str) -> Result<AccessWorkspace> {
        Self::open_with_mode(path, AccessMode::ReadOnly, None)
    }
}

impl WorkspaceFactory for AccessWorkspaceFactory {
    fn extensions(&self) -> &[&'static str] {
        &["mdb", "accdb"]
    }

    fn probe(&self, path: &str) -> bool {
        let ext_ok = path
            .rsplit('.')
            .next()
            .map(|e| self.extensions().iter().any(|x| x.eq_ignore_ascii_case(e)))
            .unwrap_or(false);
        ext_ok || Self::looks_like_mdb(path)
    }

    fn open(&self, path: &str, options: Option<WorkspaceOptions>) -> Result<AccessWorkspace> {
        // 按 `WorkspaceOptions::access_mode` 选择后端：只读 -> jetdb（纯 Rust，无驱动）；
        // 读写 -> ODBC（需安装与程序位数匹配的 Access/ACE 驱动）。
        // 未显式给选项时按默认权限（只读）打开；要写入请显式传 ReadWrite
        // 或改用 `open_with_mode(path, AccessMode::ReadWrite, ...)`。
        let opts = options.unwrap_or_default();
        let backend = crate::datastore::open_backend(path, opts.access_mode, None)?;
        let backend: Arc<dyn SqlBackend> = Arc::from(backend);
        Self::open_from_backend(backend, path, Some(opts))
    }
}

/// 便捷打开函数：按指定**读写权限**打开工作空间。
///
/// ```rust,no_run
/// use pgdb::gdb::{open_workspace, AccessMode};
///
/// # fn demo() -> pgdb::Result<()> {
/// // 只读：纯 Rust jetdb，无需任何驱动
/// let ws = open_workspace("sample.mdb", AccessMode::ReadOnly)?;
/// // 读写：ODBC，需安装与程序位数匹配的 Access/ACE 驱动
/// let rw = open_workspace("sample.mdb", AccessMode::ReadWrite)?;
/// # Ok(())
/// # }
/// ```
pub fn open_workspace(path: &str, mode: AccessMode) -> Result<AccessWorkspace> {
    AccessWorkspaceFactory::open_with_mode(path, mode, None)
}

/// 便捷打开函数：以只读权限打开（jetdb，无驱动依赖）
pub fn open_workspace_read_only(path: &str) -> Result<AccessWorkspace> {
    open_workspace(path, AccessMode::ReadOnly)
}

/// 便捷函数：判断某个要素类是否存在
pub fn contains_feature_class(ws: &AccessWorkspace, name: &str) -> bool {
    ws.open_feature_class(name).is_ok()
}
