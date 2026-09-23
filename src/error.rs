//! pgdb-rs 的错误类型。
//!
//! 设计时参考 ArcObjects 的错误语义：底层 IO / 数据库访问 / 几何解析 / 元数据不一致
//! 分别归类，便于上层按来源处理。

use std::path::PathBuf;

/// pgdb 统一结果别名。
pub type Result<T> = std::result::Result<T, PgdbError>;

/// pgdb 的所有错误。
#[derive(Debug, thiserror::Error)]
pub enum PgdbError {
    /// 文件系统错误（打开 mdb / 镜像文件失败等）
    #[error("IO 错误: {context}: {source}")]
    Io {
        /// 出错上下文
        context: String,
        /// 底层 IO 错误
        source: std::io::Error,
    },

    /// 底层数据源（ODBC / Access 驱动）返回的错误
    #[error("数据源错误: {0}")]
    Backend(String),

    /// SQL 语句构造或执行错误
    #[error("SQL 错误: {0}")]
    Sql(String),

    /// 二进制几何解析失败
    #[error("几何解析错误: {0}")]
    Geometry(String),

    /// 不受支持的几何类型 / 特性
    #[error("不受支持的特性: {0}")]
    Unsupported(String),

    /// 请求的数据集不存在
    #[error("找不到数据集: {0}")]
    NotFound(String),

    /// GDB 系统表缺失或内容不自洽
    #[error("地理数据库元数据错误: {0}")]
    Metadata(String),

    /// 字段值类型转换失败
    #[error("类型转换错误: 字段 {field}: {message}")]
    Conversion {
        /// 字段名
        field: String,
        /// 说明
        message: String,
    },

    /// 参数不合法
    #[error("无效参数: {0}")]
    InvalidArgument(String),

    /// 工作空间为只读，不允许写入操作
    #[error("只读工作空间，不支持写入: {0}")]
    ReadOnly(String),

    /// 多个错误聚合（例如批量刷新游标时的部分失败）
    #[error("批处理中 {0} 项失败: {1}")]
    Batch(usize, String),
}

impl PgdbError {
    /// 构造 IO 错误。
    pub fn io<S: Into<String>>(context: S, e: std::io::Error) -> Self {
        PgdbError::Io {
            context: context.into(),
            source: e,
        }
    }

    /// 构造后端错误。
    pub fn backend<S: Into<String>>(msg: S) -> Self {
        PgdbError::Backend(msg.into())
    }

    /// 构造几何错误。
    pub fn geometry<S: Into<String>>(msg: S) -> Self {
        PgdbError::Geometry(msg.into())
    }

    /// 构造“对象不存在”错误。
    pub fn not_found<S: Into<String>>(name: S) -> Self {
        PgdbError::NotFound(name.into())
    }

    /// 构造“只读工作空间”错误。
    pub fn read_only<S: Into<String>>(msg: S) -> Self {
        PgdbError::ReadOnly(msg.into())
    }
}

impl From<std::io::Error> for PgdbError {
    fn from(e: std::io::Error) -> Self {
        PgdbError::Io {
            context: "未指定".to_string(),
            source: e,
        }
    }
}

impl From<std::str::Utf8Error> for PgdbError {
    fn from(e: std::str::Utf8Error) -> Self {
        PgdbError::Conversion {
            field: "<bytes>".into(),
            message: e.to_string(),
        }
    }
}

impl From<std::num::TryFromIntError> for PgdbError {
    fn from(e: std::num::TryFromIntError) -> Self {
        PgdbError::Conversion {
            field: "<int>".into(),
            message: e.to_string(),
        }
    }
}

/// 便捷构造：路径相关错误
pub fn path_err(p: &std::path::Path) -> String {
    PathBuf::from(p).display().to_string()
}
