//! 工作空间工厂：对应 ArcObjects 的 `IWorkspaceFactory` / `AccessWorkspaceFactory`。

use std::sync::Arc;

use crate::datastore::SqlBackend;
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

    /// 由已有后端（ODBC / 镜像）构造工作空间
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

    /// 直接打开本地镜像文件（无需任何数据库驱动）
    pub fn open_mirror(path: &str) -> Result<AccessWorkspace> {
        let backend = Arc::new(crate::datastore::mirror::MirrorBackend::open_file(path)?);
        Self::open_from_backend(backend, path, None)
    }

    /// 打开本地镜像文件并使用指定选项
    pub fn open_mirror_with_options(path: &str, options: WorkspaceOptions) -> Result<AccessWorkspace> {
        let backend = Arc::new(crate::datastore::mirror::MirrorBackend::open_file(path)?);
        Self::open_from_backend(backend, path, Some(options))
    }

    /// 打开 ODBC 数据源把?.mdb 作为个人地理数据库访问
    ///
    /// Windows 使用 `Driver={Microsoft Access Driver (*.mdb)}`，Linux 可用只读的
    /// MDBTools 驱动。写操作需要 Windows + ACE/Jet 驱动。
    #[cfg(feature = "odbc")]
    pub fn open_odbc(path: &str, connection_string: Option<&str>) -> Result<AccessWorkspace> {
        let backend = Arc::new(crate::datastore::odbc::OdbcBackend::connect(
            path,
            connection_string,
        )?);
        Self::open_from_backend(backend, path, None)
    }

    /// 未启用 odbc feature 时的占位实现，用于给出明确错误提示
    #[cfg(not(feature = "odbc"))]
    pub fn open_odbc(_path: &str, _connection_string: Option<&str>) -> Result<AccessWorkspace> {
        Err(crate::error::PgdbError::Unsupported(
            "未编译 'odbc' feature，请 cargo build --features odbc 后重试".into(),
        ))
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
        let is_json = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            == Some("json");
        if is_json {
            return match options {
                Some(o) => Self::open_mirror_with_options(path, o),
                None => Self::open_mirror(path),
            };
        }
        let backend = crate::datastore::odbc_backend(path, None)?;
        let backend: Arc<dyn SqlBackend> = Arc::from(backend);
        Self::open_from_backend(backend, path, options)
    }
}

/// 便捷打开函数：按路径自动选择后端
pub fn open_workspace(path: &str) -> Result<AccessWorkspace> {
    let factory = AccessWorkspaceFactory;
    factory.open(path, None)
}

/// 便捷函数：判断某个要素类是否存在
pub fn contains_feature_class(ws: &AccessWorkspace, name: &str) -> bool {
    ws.open_feature_class(name).is_ok()
}
