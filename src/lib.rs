//! # pgdb-rs
//!
//! 用 Rust 解析与管理 **ESRI Personal Geodatabase（*.mdb）** 的库。
//!
//! ## 能做什么
//!
//! - 遍历工作空间：独立要素类、独立数据表、**要素数据集内部的要素类**（`IFeatureDataset::Subsets` 语义）
//! - 同时支持两套元数据模型：ArcGIS 9.2+ 的 `GDB_Items` 模型与 8.x/9.0 的
//!   `GDB_ObjectClasses` / `GDB_GeomColumns` 旧模型
//! - 完整的 ESRI Shape 二进制编解码（= shapefile 记录体），支持点/多点/线/面及 Z、M 变体
//! - ArcEngine 风格的行/要素游标，支持属性与几何更新（`IFeature::Store` 语义）
//! - 写几何时自动维护 ESRI 依赖的一致性数据：`Shape_Length` / `Shape_Area`、
//!   `<业务表>_SHAPE_Index` 网格记录、`GDB_GeomColumns` 图层范围
//!
//! ## 快速示例
//!
//! ### 遍历：独立要素类 / 独立表 / 要素数据集内部的要素类
//!
//! ```rust,no_run
//! use pgdb::gdb::{
//!     DatasetNode, FeatureDataset, FeatureWorkspace, Workspace, WorkspaceFactory,
//!     AccessWorkspaceFactory,
//! };
//!
//! # fn demo() -> pgdb::Result<()> {
//! // 真实 *.mdb 用 open_odbc（Windows + ACE 驱动）：
//! // let ws = AccessWorkspaceFactory::open_odbc("sample.mdb", None)?;
//! let ws = AccessWorkspaceFactory::open_mirror("sample.mdb.json")?;
//!
//! // IWorkspace::get_Datasets —— 顶层数据集枚举
//! let mut enum_ds = ws.datasets()?;
//! while let Some(handle) = enum_ds.next_dataset() {
//!     println!("{}", handle.describe_line());
//!     // IFeatureDataset::Subsets —— 要素数据集内部继续下钻
//!     if let Some(fd) = handle.as_feature_dataset() {
//!         for child in fd.subsets() {
//!             println!("    {}", child.describe_line());
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ### 更新：属性与几何（`IFeature::Store` 语义）
//!
//! ```rust,no_run
//! use pgdb::gdb::{FeatureClass, FeatureWorkspace, QueryFilter, Table, WorkspaceFactory};
//! use pgdb::gdb::AccessWorkspaceFactory;
//! use pgdb::{Geometry, Value};
//!
//! # fn demo() -> pgdb::Result<()> {
//! let ws = AccessWorkspaceFactory::open_mirror("sample.mdb.json")?;
//!
//! // 数据集内的要素类用限定名：`数据集\要素类`
//! let fc = ws.open_feature_class("Hydrology\\Ponds")?;
//! let mut cursor = fc.update_features(QueryFilter::new())?;
//! while let Some(mut feature) = cursor.next_feature()? {
//!     // 属性
//!     feature.set_value_by_name("NAME", Value::String("已更新".into()))?;
//!     // 几何（也可直接使用 Polygon/Polyline 结构体）
//!     feature.set_geometry(&Geometry::point(1.0, 2.0))?;
//!     // IFeature::Store —— 同时维护 Shape_Length/Shape_Area、<表>_SHAPE_Index、图层范围
//!     feature.store()?;
//! }
//! # Ok(())
//! # }
//! ```

pub mod datastore;
pub mod error;
pub mod field;
pub mod gdb;
pub mod geom;
pub mod sql;
pub mod text;
pub mod value;

pub use error::{PgdbError, Result};
pub use field::{Field, Fields, FieldType};
pub use geom::{Envelope, Geometry, GeometryType, Vertex};
pub use text::{display_width, pad_display};
pub use value::{SqlValue, Value};

/// 初始化日志（RUST_LOG=info 时输出元数据采集过程）
///
/// 在 Windows 上会**同时把控制台与日志输出流切到 UTF-8**，见 [`init_console_utf8`]：
/// 否则中文日志会被控制台按本地代码页（简中为 GBK/CP936）解读，
/// 例如 `要素类 BDC不动产` 会显示成 `瑕佺礌绫� BDC涓嶅姩浜�`。
pub fn init_log() {
    init_console_utf8();
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .is_test(false)
        .try_init();
}

/// 让本进程的标准输出 / 标准错误按 UTF-8 呈现（仅 Windows 需要，其它平台为空操作）。
///
/// 做三件事：
///
/// 1. `SetConsoleOutputCP(65001)`：把控制台**输出**代码页设为 UTF-8。这一步只对
///    控制台生效；重定向到文件 / 管道时不影响字节流（此时字节本就是 UTF-8，
///    控制台代码页根本不参与）。
/// 2. `SetConsoleCP(65001)`：同样把控制台**输入**代码页设为 UTF-8 —— 便于
///    `pgdb-cli` 接收中文参数（如 `--where "名称 = '界址点'"`），
///    否则 `cmd.exe` 会把命令行按 OEM 代码页（CP437/CP936）编码，
///    UTF-8 参数会被截断成 `?`。
/// 3. 刷新 `stdout`/`stderr`，丢弃切换前可能残留的缓冲。
///
/// **顺序很重要**：`SetConsoleOutputCP` 必须在任何 `println!` / 日志写出之前调用。
/// `pgdb-cli::main` 的第一条语句就是 [`init`]，因此只要走标准入口就不会踩坑。
///
/// 环境变量 `PGDB_NO_UTF8_CONSOLE=1` 可关闭该行为（例如需要用管道对接
/// 只吃 GBK 的旧脚本时）。
///
/// 该函数幂等，重复调用无副作用；对非 Windows 平台是空操作。
pub fn init_console_utf8() {
    #[cfg(windows)]
    {
        // 常量直接内联，避免为几个数字引入 windows-sys / winapi 依赖，
        // 也顺带规避 MSRV 约束。
        const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
        const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // (DWORD)-12
        const CP_UTF8: u32 = 65001;

        extern "system" {
            fn SetConsoleOutputCP(code_page: u32) -> i32;
            fn SetConsoleCP(code_page: u32) -> i32;
            fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut u32) -> i32;
            fn GetStdHandle(std_handle: u32) -> *mut core::ffi::c_void;
        }

        if std::env::var_os("PGDB_NO_UTF8_CONSOLE").is_some() {
            return;
        }

        // 输出被重定向到文件/管道时 `GetConsoleMode` 会失败：
        // 此时字节流本就是 UTF-8，无需（也无法）改代码页。
        let has_console = {
            let mut mode: u32 = 0;
            // SAFETY: 传入标准句柄常量与有效 `&mut u32`；失败只返回 0。
            let out = unsafe { GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut mode) } != 0;
            let err = unsafe { GetConsoleMode(GetStdHandle(STD_ERROR_HANDLE), &mut mode) } != 0;
            out || err
        };

        if has_console {
            // SAFETY: 两个 API 均只接受一个整型代码页参数，无内存安全问题。
            let ok_out = unsafe { SetConsoleOutputCP(CP_UTF8) } != 0;
            let _ = unsafe { SetConsoleCP(CP_UTF8) };
            if ok_out {
                // 丢掉切换前可能已写入的缓冲，避免旧编码内容混在新内容之前。
                use std::io::Write;
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
            }
        }
    }
}

/// 可执行文件入口统一调用：初始化控制台编码 + 日志。
///
/// `pgdb-cli` 的 `main` 与自定义二进制都应调用本函数，
/// 保证任何平台下输出编码一致。
pub fn init() {
    init_log();
}

/// 库版本
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
