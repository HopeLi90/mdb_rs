//! `pgdb-cli`：ESRI Personal Geodatabase（*.mdb）的命令行解析与管理工具。
//!
//! 子命令设计对应 ArcEngine 的日常操作：
//!
//! | 命令 | ArcObjects 语义 |
//! |------|----------------|
//! | `tree` / `list` | `IWorkspace::get_Datasets` + `IFeatureDataset::Subsets` |
//! | `fields` | `ITable::Fields` |
//! | `rows` | `ITable::Search` + `ICursor::NextRow` |
//! | `export-wkt` | `IFeatureClass::Search` + `IFeature::Shape` |
//! | `update-attr` | `ITable::Update` + `IRow::Store` |
//! | `set-geometry` | `IFeatureClass::Update` + `IFeature::Store` |
//! | `create-feature` | `IFeatureClass::Insert` + `IFeatureBuffer` |
//! | `delete-rows` | `ITable::DeleteSearchedRows` |
//! | `rebuild-index` | 维护 `<表>_SHAPE_Index` 与 `GDB_GeomColumns` |
//!
//! 数据源为真实 `*.mdb` / `*.accdb`，由 `--access` 指定**读写权限**，
//! 权限决定使用哪种解析方式：
//! - `--access readonly`（**默认**，或兼容简写 `--read-only`）：纯 Rust 的 **jetdb**
//!   解析，无需任何驱动、跨平台，仅支持查询；
//! - `--access readwrite`：**ODBC** 驱动解析，支持读写，
//!   需要安装与程序位数匹配的 Access/ACE 驱动。

use std::path::Path;

use clap::{Args, Parser, Subcommand};

use pgdb::gdb::{
    AccessMode, AccessWorkspace, AccessWorkspaceFactory, DatasetHandle, DatasetKind, EditOptions,
    FeatureClass, FeatureWorkspace, InsertFeatureCursor, MetadataModel, Preflight, QueryFilter,
    SpatialReference, Table, Workspace,
};
use pgdb::datastore::Predicate;
use pgdb::geom::{geometry_from_wkt, AsWkt, Geometry};
use pgdb::{pad_display, Field, FieldType, Fields, PgdbError, Result, Value};

fn main() {
    // Windows 控制台默认按本地代码页（简中 GBK/CP936）解码输出，
    // 直接打印 UTF-8 的中文日志/表名会变成 `瑕佺礌绫� BDC涓嶅姩浜�`。
    // `init` 内部会先把控制台输出代码页切到 UTF-8 再初始化日志。
    pgdb::init();
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        eprintln!("错误: {err}");
        std::process::exit(1);
    }
}

/// 命令行工具入口
#[derive(Debug, Parser)]
#[command(
    name = "pgdb-cli",
    version,
    about = "ESRI Personal Geodatabase (*.mdb) 解析与管理工具"
)]
struct Cli {
    /// 读写权限：`readonly` 用纯 Rust 的 jetdb 解析（仅查询、无需驱动，默认）；
    /// `readwrite` 用 ODBC 驱动解析（可写，需匹配位数的 Access/ACE 驱动）
    #[arg(long, value_name = "MODE", value_parser = parse_access_mode, global = true)]
    access: Option<AccessMode>,
    /// 等价于 `--access readonly`（兼容的显式简写；不传 --access 时本就是只读）
    #[arg(long, global = true, conflicts_with = "access")]
    read_only: bool,
    /// 演练模式：对更新/删除命令只做预检（打印命中行数/范围/可写性），不实际写库
    #[arg(long, global = true)]
    dry_run: bool,
    /// 数据源路径（真实 `*.mdb` / `*.accdb`）
    database: String,

    #[command(subcommand)]
    command: Command,
}

/// 解析 `--access` 的取值，把 clap 的报错转成项目统一错误文案
fn parse_access_mode(s: &str) -> std::result::Result<AccessMode, String> {
    s.parse::<AccessMode>().map_err(|e| e.to_string())
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 列出本机 ODBC 驱动与数据源（排查驱动未装 / 位数不匹配）
    Drivers,
    /// 打印工作空间信息（后端类型、元数据模型、数据集统计）
    Info,
    /// 按目录树列出数据集（含要素数据集内部的要素类）
    Tree,
    /// 扁平列出所有数据集
    List {
        /// 只列出要素类
        #[arg(long)]
        feature_classes: bool,
        /// 只列出要素数据集内部的要素类
        #[arg(long)]
        in_dataset: bool,
    },
    /// 列出数据集的字段定义（`ITable::Fields`）
    Fields {
        /// 数据集名，要素数据集内的要素类用 `数据集\要素类`
        dataset: String,
    },
    /// 打印行（`ITable::Search`）
    Rows(RowsArgs),
    /// 导出要素几何的 WKT（`IFeature::Shape`）
    ExportWkt(ExportWktArgs),
    /// 批量更新属性（`ITable::UpdateSearchedRows`）
    UpdateAttr(UpdateAttrArgs),
    /// 更新几何（`IFeature::Store`，自动维护长度/面积、空间索引、图层范围）
    SetGeometry(SetGeometryArgs),
    /// 新建要素（`IFeatureClass::Insert` + `IFeatureBuffer`）
    CreateFeature(CreateFeatureArgs),
    /// 新建一行属性记录（不含几何）
    CreateRow(CreateRowArgs),
    /// 按条件或 OBJECTID 列表删除（`DeleteSearchedRows` / `ITable::DeleteRows`）
    DeleteRows(DeleteRowsArgs),
    /// 清空整张表（`ITable::DeleteAllRows` 语义，必须 --yes）
    DeleteAllRows {
        /// 表名或要素类名
        dataset: String,
        /// 确认清空（必填，防误操作）
        #[arg(long)]
        yes: bool,
    },
    /// 编辑前体检：打印命中行数/作用范围/可写性，不做任何修改
    Preflight {
        /// 数据集名
        dataset: String,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// 重建空间索引并重算图层范围
    RebuildIndex {
        /// 要素类名
        dataset: String,
    },
    /// 执行原始 SQL（仅 ODBC 后端可用）
    Sql {
        /// SQL 语句
        statement: String,
    },
}

/// 过滤条件参数（多个子命令共用）
#[derive(Debug, Args, Clone, Default)]
struct FilterArgs {
    /// 按 OBJECTID 精确过滤（所有后端都支持）
    #[arg(long, value_name = "OID")]
    oid: Option<i64>,
    /// WHERE 子句（不含 WHERE 关键字）。ODBC 后端直接下推；其它后端会降级为内存过滤
    #[arg(long = "where", value_name = "CLAUSE")]
    where_clause: Option<String>,
}

impl FilterArgs {
    fn to_filter(&self) -> QueryFilter {
        let mut f = QueryFilter::new();
        if let Some(clause) = &self.where_clause {
            f = f.with_where(clause.clone());
        }
        if let Some(oid) = self.oid {
            f.oid = Some(oid);
        }
        f
    }
}

#[derive(Debug, Args)]
struct RowsArgs {
    /// 数据集名
    dataset: String,
    /// 只显示这些字段（逗号分隔，会自动补上 OBJECTID）
    #[arg(long, value_delimiter = ',')]
    fields: Vec<String>,
    /// 最多输出多少行，0 表示不限制
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    filter: FilterArgs,
}

#[derive(Debug, Args)]
struct ExportWktArgs {
    /// 要素类名
    dataset: String,
    /// 输出文件（缺省打印到标准输出）
    #[arg(long, value_name = "FILE")]
    output: Option<String>,
    /// WKT 坐标小数位
    #[arg(long, default_value_t = 6)]
    precision: usize,
    #[command(flatten)]
    filter: FilterArgs,
}

#[derive(Debug, Args)]
struct UpdateAttrArgs {
    /// 数据集名
    dataset: String,
    /// 字段赋值，形如 `FIELD=VALUE`，可重复
    #[arg(long = "set", value_name = "FIELD=VALUE", required = true)]
    set: Vec<String>,
    /// 显式确认作用于全表（过滤器未给条件时必须加此开关）
    #[arg(long)]
    all: bool,
    #[command(flatten)]
    filter: FilterArgs,
}

#[derive(Debug, Args)]
struct SetGeometryArgs {
    /// 要素类名
    dataset: String,
    /// WKT 文本，例如 `POLYGON((0 0, 10 0, 10 10, 0 0))`
    #[arg(long, value_name = "WKT")]
    wkt: Option<String>,
    /// 从文件读取 WKT（每行一个几何，按行号依次赋给过滤出的要素）
    #[arg(long, value_name = "FILE")]
    wkt_file: Option<String>,
    #[command(flatten)]
    filter: FilterArgs,
}

#[derive(Debug, Args)]
struct CreateFeatureArgs {
    /// 要素类名
    dataset: String,
    /// 几何 WKT
    #[arg(long, value_name = "WKT")]
    wkt: Option<String>,
    /// 从文件读取几何 WKT（每个几何一行，一次全部插入）
    #[arg(long, value_name = "FILE")]
    wkt_file: Option<String>,
    /// 字段赋值，形如 `FIELD=VALUE`，可重复
    #[arg(long = "set", value_name = "FIELD=VALUE")]
    set: Vec<String>,
}

#[derive(Debug, Args)]
struct CreateRowArgs {
    /// 表名或要素类名
    dataset: String,
    /// 字段赋值，形如 `FIELD=VALUE`，可重复
    #[arg(long = "set", value_name = "FIELD=VALUE", required = true)]
    set: Vec<String>,
}

#[derive(Debug, Args)]
struct DeleteRowsArgs {
    /// 数据集名
    dataset: String,
    /// 确认删除（避免误操作）
    #[arg(long)]
    yes: bool,
    /// 按 OBJECTID 列表删除（`ITable::DeleteRows` 语义），逗号分隔；
    /// 给出时忽略 --where/--oid 过滤器
    #[arg(long, value_delimiter = ',', value_name = "OID,OID,...")]
    oids: Vec<i64>,
    #[command(flatten)]
    filter: FilterArgs,
}

fn run(cli: Cli) -> Result<()> {
    // drivers 命令只探测系统驱动，不需要打开数据库
    if let Command::Drivers = cli.command {
        return cmd_drivers();
    }
    // 由 `--access`（或兼容简写 `--read-only`）解析出读写权限，权限决定解析后端；
    // 未显式指定时默认 **只读**（jetdb，无需驱动、不可能误改数据）
    let mode = cli.access.unwrap_or(AccessMode::ReadOnly);
    let ws = open_workspace(&cli.database, mode)?;
    if cli.dry_run && is_destructive(&cli.command) {
        return cmd_dry_run(&ws, &cli.command);
    }
    match cli.command {
        Command::Drivers => unreachable!("已在上方处理"),
        Command::Info => cmd_info(&ws),
        Command::Tree => cmd_tree(&ws),
        Command::List {
            feature_classes,
            in_dataset,
        } => cmd_list(&ws, feature_classes, in_dataset),
        Command::Fields { dataset } => cmd_fields(&ws, &dataset),
        Command::Rows(args) => cmd_rows(&ws, &args),
        Command::ExportWkt(args) => cmd_export_wkt(&ws, &args),
        Command::UpdateAttr(args) => cmd_update_attr(&ws, &args),
        Command::SetGeometry(args) => cmd_set_geometry(&ws, &args),
        Command::CreateFeature(args) => cmd_create_feature(&ws, &args),
        Command::CreateRow(args) => cmd_create_row(&ws, &args),
        Command::DeleteRows(args) => cmd_delete_rows(&ws, &args),
        Command::DeleteAllRows { dataset, yes } => cmd_delete_all_rows(&ws, &dataset, yes),
        Command::Preflight { dataset, filter } => cmd_preflight(&ws, &dataset, &filter),
        Command::RebuildIndex { dataset } => cmd_rebuild_index(&ws, &dataset),
        Command::Sql { statement } => cmd_sql(&ws, &statement),
    }
}

/// 该命令是否会修改数据（`--dry-run` 只对这些命令做预检拦截）
fn is_destructive(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::UpdateAttr(_)
            | Command::SetGeometry(_)
            | Command::DeleteRows(_)
            | Command::DeleteAllRows { .. }
            | Command::RebuildIndex { .. }
    )
}

/// 提取破坏性命令的数据集与过滤器，供演练预检
fn destructive_target(cmd: &Command) -> Option<(String, QueryFilter)> {
    match cmd {
        Command::UpdateAttr(a) => Some((a.dataset.clone(), a.filter.to_filter())),
        Command::SetGeometry(a) => Some((a.dataset.clone(), a.filter.to_filter())),
        Command::DeleteRows(a) => Some((a.dataset.clone(), a.filter.to_filter())),
        Command::DeleteAllRows { dataset, .. } => {
            Some((dataset.clone(), QueryFilter::new()))
        }
        Command::RebuildIndex { dataset } => Some((dataset.clone(), QueryFilter::new())),
        _ => None,
    }
}

/// `--dry-run`：对破坏性命令只做体检（preflight），不写库
fn cmd_dry_run(ws: &AccessWorkspace, cmd: &Command) -> Result<()> {
    println!("[dry-run] 以下为预检结果，未做任何修改：");
    // delete-rows --oids 走 ITable::DeleteRows 的 OID 数组语义，按列表预检
    if let Command::DeleteRows(a) = cmd {
        if !a.oids.is_empty() {
            return dry_run_oid_list(ws, &a.dataset, &a.oids);
        }
    }
    let Some((dataset, filter)) = destructive_target(cmd) else {
        return Ok(());
    };
    let handle = ws.open_dataset(&dataset)?;
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!("{dataset} 不是表/要素类")));
    };
    let pre = table.preflight_edit(&filter)?;
    println!("  数据集   : {dataset}");
    println!("  命中行数 : {}", pre.matched);
    println!("  作用范围 : {}", pre.scope);
    println!("  可写性   : {}", if pre.writable { "可写" } else { "只读" });
    if pre.is_whole_table() {
        println!("  ⚠ 全表操作：执行时需要显式确认（--all / --yes）");
    }
    if pre.is_no_hit() {
        println!("  ⚠ 零命中：请核对过滤条件是否写错");
    }
    Ok(())
}

/// `--dry-run` 下 `delete-rows --oids` 的预检：按 OID 列表计数并提示不存在的 OID
fn dry_run_oid_list(ws: &AccessWorkspace, dataset: &str, oids: &[i64]) -> Result<()> {
    let handle = ws.open_dataset(dataset)?;
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!("{dataset} 不是表/要素类")));
    };
    let pred = Predicate::in_ids(table.oid_field_name().to_string(), oids);
    let matched = table.backend().count(table.table_name(), &pred)?;
    let writable = table.backend().capabilities().writable;
    println!("  数据集   : {dataset}");
    println!("  指定 OID : {} 个", oids.len());
    println!("  命中行数 : {matched}");
    println!("  可写性   : {}", if writable { "可写" } else { "只读" });
    let missing = (oids.len() as u64).saturating_sub(matched);
    if missing > 0 {
        println!("  ⚠ 有 {missing} 个 OBJECTID 在表中不存在");
    }
    Ok(())
}

/// 按读写权限打开工作空间：权限枚举直接决定后端（jetdb / ODBC），
/// 不再依赖编译期的 feature 开关。
fn open_workspace(path: &str, mode: AccessMode) -> Result<AccessWorkspace> {
    if !Path::new(path).exists() {
        return Err(PgdbError::InvalidArgument(format!("数据源不存在: {path}")));
    }
    AccessWorkspaceFactory::open_with_mode(path, mode, None)
}

/// 写操作前的权限/能力检查（对标 ArcEngine 的 `IWorkspaceEdit` 编辑约束）
fn ensure_writable(ws: &AccessWorkspace) -> Result<()> {
    if ws.can_write() {
        Ok(())
    } else if ws.is_read_only() {
        Err(PgdbError::read_only(
            "当前以只读权限打开（jetdb 后端），不支持写入；如需写入，请改用 \
             `--access readwrite`（ODBC 后端，需安装与程序位数匹配的 Access/ACE 驱动）",
        ))
    } else {
        Err(PgdbError::read_only(
            "当前后端的驱动本身为只读（例如 Linux 下的 MDBTools 驱动），不支持写入；\
             请使用 Windows + Microsoft Access Database Engine 驱动",
        ))
    }
}

// ------------------------------------------------------------------ 查询类

/// 列出系统里登记的 ODBC 驱动与 DSN，用于排查"驱动没装 / 位数不匹配"
fn cmd_drivers() -> Result<()> {
    let probe = pgdb::datastore::odbc::probe_drivers();
    println!("已登记的 ODBC 驱动：");
    if probe.installed.is_empty() {
        println!("  （无）");
    }
    for d in &probe.installed {
        let tag = if probe.access_drivers.contains(d) {
            "   [可用于 mdb]"
        } else {
            ""
        };
        println!("  - {d}{tag}");
    }
    println!();
    println!("用户/系统 DSN：");
    if probe.data_sources.is_empty() {
        println!("  （无）");
    }
    for d in &probe.data_sources {
        println!("  - {d}");
    }
    println!();
    if probe.access_drivers.is_empty() {
        println!("未找到 Access 驱动：请安装 Microsoft Access Database Engine Redistributable，");
        println!("并确保驱动位数与程序位数一致（64 位程序需要 64 位 ACE）。");
        println!("提示：仅做查询时可用 `--access readonly`，走纯 Rust 的 jetdb 后端，无需任何驱动。");
    }
    Ok(())
}

fn cmd_info(ws: &AccessWorkspace) -> Result<()> {
    let caps = ws.backend().capabilities();
    let catalog = ws.catalog();
    println!("数据源        : {}", ws.path());
    println!("后端          : {}", ws.backend().kind());
    println!(
        "读写权限      : {}（{}）",
        ws.access_mode(),
        if ws.is_read_only() { "只读" } else { "读写" }
    );
    println!(
        "可写/事务/SQL : {} / {} / {}",
        bool_cn(caps.writable),
        bool_cn(caps.transaction),
        bool_cn(caps.raw_sql)
    );
    println!("元数据模型    : {}", model_label(ws.metadata_model()));
    println!(
        "数据集统计    : 要素类 {} 个（其中数据集内 {} 个）/ 独立表 {} 个 / 要素数据集 {} 个",
        catalog.all_feature_classes().count(),
        catalog
            .all_feature_classes()
            .filter(|e| e.parent.is_some())
            .count(),
        catalog.standalone_tables().count(),
        catalog.feature_datasets().count()
    );
    Ok(())
}

fn cmd_tree(ws: &AccessWorkspace) -> Result<()> {
    print!("{}", ws.describe_tree()?);
    Ok(())
}

fn cmd_list(ws: &AccessWorkspace, only_fc: bool, only_in_fd: bool) -> Result<()> {
    let mut en = ws.datasets()?;
    while let Some(handle) = en.next_dataset() {
        let is_fc = matches!(handle.kind(), DatasetKind::FeatureClass);
        let nested = handle.parent_name().is_some();
        if only_fc && !is_fc {
            continue;
        }
        if only_in_fd && !nested {
            continue;
        }
        println!("{}", handle.describe_line());
        if let Some(fd) = handle.as_feature_dataset() {
            for child in fd.subsets() {
                println!("    {}", child.describe_line());
            }
        }
    }
    Ok(())
}

fn cmd_fields(ws: &AccessWorkspace, dataset: &str) -> Result<()> {
    let handle = ws.open_dataset(dataset)?;
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!("{dataset} 不是表/要素类")));
    };
    println!("数据集        : {}", handle.qualified_name());
    println!("物理表        : {}", table.table_name());
    println!("对象类型      : {}", handle.kind().label());
    println!("OBJECTID 字段 : {}", table.oid_field_name());
    if let Some(fc) = handle.as_feature_class() {
        println!("几何字段      : {}", fc.shape_field_name());
        println!("几何类型      : {}", fc.shape_type().label());
        println!("要素类型      : {}", fc.feature_type().label());
        if let Some(sr) = fc.spatial_reference() {
            println!("空间参考      : {}", spatial_ref_line(sr));
        }
        if let Some(env) = fc.extent() {
            println!(
                "图层范围      : {:.6} {:.6} {:.6} {:.6}",
                env.min_x, env.min_y, env.max_x, env.max_y
            );
        }
        let grid = fc.grid_info();
        println!(
            "空间网格      : 原点 ({}, {})，格网 {}",
            grid.origin_x, grid.origin_y, grid.grid_size
        );
        if let Some(name) = fc.shape_index_table() {
            println!("空间索引表    : {name}");
        }
        if let Some(len) = fc.length_field_name() {
            print!("长度字段      : {len}");
            if let Some(area) = fc.area_field_name() {
                print!("，面积字段 : {area}");
            }
            println!();
        }
    }
    let count = Table::row_count(table, &QueryFilter::new())?;
    println!("行数          : {count}");
    println!();
    println!(
        "{}{}{}{}{}{}{}",
        pad_display("序号", 6),
        pad_display("字段名", 24),
        pad_display("别名", 18),
        pad_display("类型", 12),
        pad_display("长度", 8),
        pad_display("可空", 8),
        pad_display("可编辑", 8)
    );
    for (i, f) in table.fields().iter().enumerate() {
        let alias = f
            .alias
            .as_deref()
            .filter(|a| !a.eq_ignore_ascii_case(&f.name))
            .unwrap_or("");
        println!(
            "{}{}{}{}{}{}{}",
            pad_display(&i.to_string(), 6),
            pad_display(&f.name, 24),
            pad_display(alias, 18),
            pad_display(f.field_type.label(), 12),
            pad_display(&f.length.map(|l| l.to_string()).unwrap_or_default(), 8),
            pad_display(bool_cn(f.nullable), 8),
            bool_cn(f.editable)
        );
    }
    Ok(())
}

fn cmd_rows(ws: &AccessWorkspace, args: &RowsArgs) -> Result<()> {
    let handle = ws.open_dataset(&args.dataset)?;
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!(
            "{} 不是表/要素类",
            args.dataset
        )));
    };
    let mut filter = args.filter.to_filter();
    if !args.fields.is_empty() {
        let mut sub = vec![table.oid_field_name().to_string()];
        for f in &args.fields {
            if !sub.iter().any(|x| x.eq_ignore_ascii_case(f)) {
                sub.push(f.clone());
            }
        }
        filter = filter.with_sub_fields(sub);
    }
    let mut cursor = table.search(filter)?;
    let mut shown = 0usize;
    while let Some(row) = cursor.next_row()? {
        if args.limit > 0 && shown >= args.limit {
            println!("... （输出已限制为 {} 行，--limit 0 取消限制）", args.limit);
            break;
        }
        if shown == 0 {
            let names = header_labels(&row);
            print_separator(&names);
            println!("{}", join_line(&names));
            print_separator(&names);
        }
        println!("{}", join_line(&cell_labels(&row)));
        shown += 1;
    }
    if shown == 0 {
        println!("（没有匹配的行）");
    }
    Ok(())
}

fn cmd_export_wkt(ws: &AccessWorkspace, args: &ExportWktArgs) -> Result<()> {
    let fc = ws.open_feature_class(&args.dataset)?;
    let filter = args.filter.to_filter();
    let mut cursor = fc.search_features(filter)?;
    let mut out = String::new();
    let mut n = 0usize;
    while let Some(feature) = cursor.next_feature()? {
        let oid = feature.oid().unwrap_or(-1);
        match feature.geometry()? {
            Some(g) => out.push_str(&format!("{oid}\t{}\n", g.as_wkt_precision(args.precision))),
            None => out.push_str(&format!("{oid}\t<NULL GEOMETRY>\n")),
        }
        n += 1;
    }
    match &args.output {
        Some(path) => {
            std::fs::write(path, out).map_err(|e| PgdbError::io(format!("写入 {path}"), e))?;
            println!("已导出 {n} 个几何到 {path}");
        }
        None => print!("{out}"),
    }
    Ok(())
}

// ------------------------------------------------------------------ 写入类

fn cmd_update_attr(ws: &AccessWorkspace, args: &UpdateAttrArgs) -> Result<()> {
    ensure_writable(ws)?;
    let handle = ws.open_dataset(&args.dataset)?;
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!(
            "{} 不是表/要素类",
            args.dataset
        )));
    };
    let sets = parse_assignments(table.fields(), &args.set)?;
    let filter = args.filter.to_filter();
    // ArcEngine 语义：ITable::UpdateSearchedRows —— 一条批量 UPDATE；
    // 全表防护由 EditOptions 承载，--all 即显式确认全表。
    let opts = EditOptions::default();
    let opts = if args.all { opts.whole_table() } else { opts };
    let result = table.update_searched_rows(&sets, &filter, opts)?;
    ws.backend().flush()?;
    println!(
        "已更新 {} 行（命中 {}，范围：{}）：{}",
        result.affected,
        result.matched,
        result.scope,
        join_names(&sets)
    );
    if result.is_no_hit() {
        println!("⚠ 零命中：过滤条件没有匹配到任何行，请核对条件是否写错。");
    }
    Ok(())
}

fn cmd_set_geometry(ws: &AccessWorkspace, args: &SetGeometryArgs) -> Result<()> {
    ensure_writable(ws)?;
    let fc = ws.open_feature_class(&args.dataset)?;
    let geometries = read_wkt_sources(&args.wkt, &args.wkt_file)?;
    if geometries.is_empty() {
        return Err(PgdbError::InvalidArgument(
            "需要提供 --wkt 或 --wkt-file".into(),
        ));
    }
    let mut cursor = fc.update_features(args.filter.to_filter())?;
    let mut affected = 0usize;
    while let Some(mut feature) = cursor.next_feature()? {
        let geom = match geometries.len() {
            1 => geometries[0].clone(),
            _ => match geometries.get(affected) {
                Some(g) => g.clone(),
                None => {
                    println!("几何数量少于要素数量，剩余要素未被修改");
                    break;
                }
            },
        };
        feature.set_geometry(&geom)?;
        feature.store()?;
        affected += 1;
    }
    ws.backend().flush()?;
    let g = &geometries[0];
    println!(
        "已更新 {affected} 个要素的几何（{}，长度={:.6}，面积={:.6}）",
        g.brief(),
        g.length(),
        g.area()
    );
    Ok(())
}

fn cmd_create_feature(ws: &AccessWorkspace, args: &CreateFeatureArgs) -> Result<()> {
    ensure_writable(ws)?;
    let fc = ws.open_feature_class(&args.dataset)?;
    let geometries = read_wkt_sources(&args.wkt, &args.wkt_file)?;
    let sets = parse_assignments(fc.fields(), &args.set)?;

    let mut cursor: InsertFeatureCursor<'_> = fc.insert_feature_cursor()?;
    let mut oids = Vec::new();
    if geometries.is_empty() {
        oids.push(insert_one(&mut cursor, &sets, None)?);
    } else {
        for g in &geometries {
            oids.push(insert_one(&mut cursor, &sets, Some(g))?);
        }
    }
    cursor.flush()?;
    ws.backend().flush()?;
    println!(
        "已在 {} 新建 {} 个要素，OBJECTID = {:?}",
        args.dataset,
        oids.len(),
        oids
    );
    Ok(())
}

fn insert_one(
    cursor: &mut InsertFeatureCursor<'_>,
    sets: &[(String, Value)],
    geometry: Option<&Geometry>,
) -> Result<i64> {
    for (name, value) in sets {
        cursor.buffer().set_value(name.as_str(), value.clone());
    }
    if let Some(g) = geometry {
        cursor.set_geometry(g)?;
    }
    cursor.insert_feature()
}

fn cmd_create_row(ws: &AccessWorkspace, args: &CreateRowArgs) -> Result<()> {
    ensure_writable(ws)?;
    let handle = ws.open_dataset(&args.dataset)?;
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!(
            "{} 不是表/要素类",
            args.dataset
        )));
    };
    let sets = parse_assignments(table.fields(), &args.set)?;
    let oid = table.insert_row(&sets)?;
    ws.backend().flush()?;
    println!("已插入一行，OBJECTID = {oid}");
    Ok(())
}

fn cmd_delete_rows(ws: &AccessWorkspace, args: &DeleteRowsArgs) -> Result<()> {
    ensure_writable(ws)?;
    if !args.yes {
        return Err(PgdbError::InvalidArgument(
            "删除操作需要显式加 --yes 确认".into(),
        ));
    }
    let handle = ws.open_dataset(&args.dataset)?;
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!(
            "{} 不是表/要素类",
            args.dataset
        )));
    };
    // --oids 给出时走 ITable::DeleteRows（OID 数组）路径；否则走 DeleteSearchedRows
    let result = if !args.oids.is_empty() {
        table.delete_rows(&args.oids)?
    } else {
        let filter = args.filter.to_filter();
        // --yes 即全表确认（过滤器无任何条件时放行）
        let opts = if filter.is_trivial() {
            EditOptions::default().whole_table()
        } else {
            EditOptions::default()
        };
        table.delete_searched_rows(&filter, opts)?
    };
    ws.backend().flush()?;
    println!(
        "已删除 {} 行（命中 {}，范围：{}）",
        result.affected, result.matched, result.scope
    );
    if result.is_no_hit() {
        println!("⚠ 零命中：过滤条件没有匹配到任何行，请核对条件是否写错。");
    } else if !args.oids.is_empty() && result.matched < args.oids.len() as u64 {
        println!(
            "⚠ 有 {} 个 OBJECTID 不存在（给 {} 个，命中 {} 个）。",
            args.oids.len() as u64 - result.matched,
            args.oids.len(),
            result.matched
        );
    }
    Ok(())
}

/// 清空整张表（`ITable::DeleteAllRows` 语义）
fn cmd_delete_all_rows(ws: &AccessWorkspace, dataset: &str, yes: bool) -> Result<()> {
    ensure_writable(ws)?;
    if !yes {
        return Err(PgdbError::InvalidArgument(
            "清空整张表属于危险操作，必须显式加 --yes 确认".into(),
        ));
    }
    let handle = ws.open_dataset(dataset)?;
    // 先记录要素类的当前图层范围，用于删除后对比展示
    let old_extent = handle.as_feature_class().and_then(|fc| fc.extent());
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!("{dataset} 不是表/要素类")));
    };
    let result = table.delete_all_rows()?;
    ws.backend().flush()?;
    println!("已清空 {dataset}：删除 {} 行（全表）", result.affected);
    if handle.as_feature_class().is_some() {
        match old_extent {
            Some(env) if !env.is_empty() => println!(
                "图层范围已重置为零范围（原范围 {:.4}, {:.4} ~ {:.4}, {:.4}）",
                env.min_x, env.min_y, env.max_x, env.max_y
            ),
            _ => println!("图层范围已重置为零范围"),
        }
    }
    Ok(())
}

/// 编辑前体检（`ISelectionSet::Count` + `IWorkspace::IsReadOnly` 语义）
fn cmd_preflight(ws: &AccessWorkspace, dataset: &str, filter: &FilterArgs) -> Result<()> {
    let handle = ws.open_dataset(dataset)?;
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!("{dataset} 不是表/要素类")));
    };
    let pre: Preflight = table.preflight_edit(&filter.to_filter())?;
    println!("数据集   : {dataset}");
    println!("命中行数 : {}", pre.matched);
    println!("作用范围 : {}", pre.scope);
    println!("可写性   : {}", if pre.writable { "可写" } else { "只读" });
    if pre.is_whole_table() {
        println!("⚠ 全表操作：执行更新/删除需要显式确认（--all / --yes）");
    }
    if pre.matched == 0 {
        println!("⚠ 零命中：请核对过滤条件是否写错");
    }
    Ok(())
}

fn cmd_rebuild_index(ws: &AccessWorkspace, dataset: &str) -> Result<()> {
    ensure_writable(ws)?;
    let fc = ws.open_feature_class(dataset)?;
    let rows = fc.rebuild_shape_index()?;
    let env = fc.refresh_layer_extent()?;
    ws.backend().flush()?;
    println!(
        "已重建空间索引（{rows} 条网格记录），图层范围 = ({:.6}, {:.6}, {:.6}, {:.6})",
        env.min_x, env.min_y, env.max_x, env.max_y
    );
    Ok(())
}

fn cmd_sql(ws: &AccessWorkspace, statement: &str) -> Result<()> {
    let head = statement.trim_start().to_ascii_lowercase();
    if head.starts_with("select") {
        let rows = match ws.query_sql(statement) {
            Ok(rows) => rows,
            Err(_) if !ws.backend().capabilities().raw_sql => simple_select(ws, statement)?,
            Err(e) => return Err(e),
        };
        if rows.is_empty() {
            println!("（空结果集）");
            return Ok(());
        }
        let names: Vec<String> = rows[0].columns().to_vec();
        print_separator(&names);
        println!("{}", join_line(&names));
        print_separator(&names);
        for r in &rows {
            let cells: Vec<String> = r.values().iter().map(|v| v.to_display_string()).collect();
            println!("{}", join_line(&cells));
        }
        println!("共 {} 行", rows.len());
    } else {
        ensure_writable(ws)?;
        let n = ws.execute_sql(statement)?;
        ws.backend().flush()?;
        println!("受影响行数 {n}");
    }
    Ok(())
}

// ------------------------------------------------------------------ 工具函数

/// 无 SQL 引擎的后端（本地镜像）上的降级实现：支持最简单的
/// `SELECT * FROM <表名> [WHERE ...]`，便于查看 `_SHAPE_Index` 等系统/辅助表。
fn simple_select(ws: &AccessWorkspace, sql: &str) -> Result<Vec<pgdb::datastore::DataTableRow>> {
    use pgdb::datastore::Predicate;
    let upper = sql.to_ascii_uppercase();
    let Some(pos) = upper.find(" FROM ") else {
        return Err(PgdbError::Unsupported(
            "该后端只支持 `SELECT * FROM <表名>` 形式的简单查询".into(),
        ));
    };
    let rest = sql[pos + 6..].trim();
    let mut tokens = rest.split_whitespace();
    let Some(raw_table) = tokens.next() else {
        return Err(PgdbError::Sql("缺少表名".into()));
    };
    let table = raw_table
        .trim_matches(|c| c == '[' || c == ']' || c == ';' || c == ',')
        .to_string();
    let filter = match upper.find(" WHERE ") {
        Some(i) => Predicate::Raw(sql[i + 7..].trim().trim_end_matches(';').to_string()),
        None => Predicate::All,
    };
    let rows = ws.backend().select(&table, &[], &filter)?;
    println!("（提示：后端不支持原生 SQL，已降级为只读表扫描）");
    Ok(rows)
}

fn read_wkt_sources(wkt: &Option<String>, file: &Option<String>) -> Result<Vec<Geometry>> {
    let mut out = Vec::new();
    if let Some(text) = wkt {
        out.push(geometry_from_wkt(text)?);
    }
    if let Some(path) = file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| PgdbError::io(format!("读取 WKT 文件 {path}"), e))?;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            out.push(geometry_from_wkt(line)?);
        }
    }
    Ok(out)
}

/// 把 `FIELD=VALUE` 按目标字段类型解析成 `Value`
fn parse_assignments(fields: &Fields, items: &[String]) -> Result<Vec<(String, Value)>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (name, text) = item.split_once('=').ok_or_else(|| {
            PgdbError::InvalidArgument(format!("赋值格式应为 FIELD=VALUE，收到：{item}"))
        })?;
        let idx = fields.require_field(name)?;
        let field: &Field = fields.field(idx).unwrap();
        if field.is_geometry {
            return Err(PgdbError::InvalidArgument(format!(
                "字段 {} 是几何字段，请用 set-geometry / create-feature 的 --wkt",
                field.name
            )));
        }
        if !field.editable {
            return Err(PgdbError::InvalidArgument(format!(
                "字段 {} 不可编辑",
                field.name
            )));
        }
        let value = parse_value(field, text.trim())?;
        out.push((field.name.clone(), value));
    }
    Ok(out)
}

fn parse_value(field: &Field, text: &str) -> Result<Value> {
    if text.is_empty() || text.eq_ignore_ascii_case("<null>") {
        return Ok(Value::Null);
    }
    let value = match field.field_type {
        FieldType::SmallInteger => Value::Short(text.parse::<i16>().map_err(|_| conv(field, text))?),
        FieldType::Integer | FieldType::Oid => {
            Value::Long(text.parse::<i32>().map_err(|_| conv(field, text))?)
        }
        FieldType::Single => Value::Single(text.parse::<f32>().map_err(|_| conv(field, text))?),
        FieldType::Double => Value::Double(text.parse::<f64>().map_err(|_| conv(field, text))?),
        FieldType::String | FieldType::Guid | FieldType::Xml => Value::String(text.to_string()),
        FieldType::Date => {
            let dt = pgdb::value::parse_datetime(text).ok_or_else(|| conv(field, text))?;
            Value::Date(dt)
        }
        FieldType::Blob => match text.strip_prefix('@') {
            Some(path) => {
                let bytes = std::fs::read(path)
                    .map_err(|e| PgdbError::io(format!("读取二进制文件 {path}"), e))?;
                Value::Blob(bytes)
            }
            None => Value::Blob(hex::decode_hex(text)?),
        },
        FieldType::Geometry | FieldType::Raster => {
            return Err(PgdbError::InvalidArgument(format!(
                "字段 {} 不能直接赋值",
                field.name
            )))
        }
    };
    Ok(value)
}

fn conv(field: &Field, text: &str) -> PgdbError {
    PgdbError::Conversion {
        field: field.name.clone(),
        message: format!("无法把 '{text}' 转换为{}", field.field_type.label()),
    }
}

/// 表头：只显示本次查询真正取回的列
fn header_labels(row: &pgdb::gdb::Row<'_>) -> Vec<String> {
    let fields = row.fields();
    row.selected_indices()
        .into_iter()
        .map(|i| {
            let name = fields
                .field(i)
                .map(|f| f.name.clone())
                .unwrap_or_else(|| format!("col{i}"));
            match row.value(i) {
                Ok(Value::Blob(b)) => format!("{name} <binary {}>", b.len()),
                _ => name,
            }
        })
        .collect()
}

/// 单元格：与表头同一投影
fn cell_labels(row: &pgdb::gdb::Row<'_>) -> Vec<String> {
    row.selected_indices()
        .into_iter()
        .map(|i| {
            row.value(i)
                .map(|v| v.to_string())
                .unwrap_or_default()
        })
        .collect()
}

fn join_line(cells: &[String]) -> String {
    cells.join(" | ")
}

fn print_separator(cells: &[String]) {
    let line: Vec<String> = cells
        .iter()
        .map(|c| "-".repeat(pgdb::display_width(c)))
        .collect();
    println!("{}", line.join("-+-"));
}

fn join_names(sets: &[(String, Value)]) -> String {
    sets.iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn bool_cn(v: bool) -> &'static str {
    if v {
        "是"
    } else {
        "否"
    }
}

fn model_label(model: MetadataModel) -> &'static str {
    match model {
        MetadataModel::Items => "GDB_Items（ArcGIS 9.2+）",
        MetadataModel::Legacy => "GDB_ObjectClasses（ArcGIS 8.x/9.0）",
        MetadataModel::Unknown => "未知（未发现 GDB_* 系统表）",
    }
}

fn spatial_ref_line(sr: &SpatialReference) -> String {
    let srid = match sr.srid {
        Some(id) => format!("SRID={id}"),
        None => "SRID=-".to_string(),
    };
    match &sr.wkt {
        Some(wkt) if !wkt.is_empty() => format!("{srid}，{}", truncate(wkt, 60)),
        _ => format!(
            "{srid}，单位={}，容差={}",
            sr.xy_units, sr.xy_tolerance
        ),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // 省略号用 ASCII 的 `~`：`…`(U+2026) 在非 UTF-8 的老控制台里
    // 会退化成单字节 `?`，与中文一起出现时反而更难定位乱码来源。
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "~"
}

/// 说明如何按 `DatasetKind` 手工分支处理三种数据形态
#[allow(dead_code)]
fn describe_handle(handle: &DatasetHandle) -> String {
    match handle.kind() {
        DatasetKind::Table => format!("独立表 {}", handle.name()),
        DatasetKind::FeatureClass => format!("要素类 {}", handle.qualified_name()),
        DatasetKind::FeatureDataset => format!("要素数据集 {}", handle.name()),
        DatasetKind::Other => format!("其他对象 {}", handle.name()),
    }
}

mod hex {
    use pgdb::PgdbError;

    /// 十六进制字符串解码（`0x` 前缀可选）
    pub fn decode_hex(s: &str) -> Result<Vec<u8>, PgdbError> {
        let lower = s.trim().to_ascii_lowercase();
        let text = match lower.strip_prefix("0x") {
            Some(rest) => rest,
            None => lower.as_str(),
        };
        if text.len() % 2 != 0 {
            return Err(PgdbError::Conversion {
                field: String::new(),
                message: format!("十六进制串长度必须为偶数: {s}"),
            });
        }
        let mut out = Vec::with_capacity(text.len() / 2);
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let hi = hex_val(bytes[i]).ok_or_else(|| bad(s, bytes[i]))?;
            let lo = hex_val(bytes[i + 1]).ok_or_else(|| bad(s, bytes[i + 1]))?;
            out.push(hi * 16 + lo);
            i += 2;
        }
        Ok(out)
    }

    fn bad(s: &str, c: u8) -> PgdbError {
        PgdbError::Conversion {
            field: String::new(),
            message: format!("非法十六进制串 {s}（字符 {}）", c as char),
        }
    }

    fn hex_val(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
}
