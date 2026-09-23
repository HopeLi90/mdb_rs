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
//! 数据源可以是真实 `*.mdb`（需要 `odbc` feature 与驱动），
//! 也可以是 JSON 镜像文件（`*.json`，无驱动依赖）。

use std::path::Path;

use clap::{Args, Parser, Subcommand};

use pgdb::gdb::{
    AccessWorkspace, AccessWorkspaceFactory, DatasetHandle, DatasetKind, FeatureClass,
    FeatureWorkspace, InsertFeatureCursor, MetadataModel, QueryFilter, SpatialReference, Table,
    Workspace, WorkspaceFactory,
};
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
    /// 数据源路径：`*.mdb` 走 ODBC 驱动，`*.json` 走本地镜像
    database: String,

    #[command(subcommand)]
    command: Command,
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
    /// 更新属性（`ITable::Update` + `IRow::Store`）
    UpdateAttr(UpdateAttrArgs),
    /// 更新几何（`IFeature::Store`，自动维护长度/面积、空间索引、图层范围）
    SetGeometry(SetGeometryArgs),
    /// 新建要素（`IFeatureClass::Insert` + `IFeatureBuffer`）
    CreateFeature(CreateFeatureArgs),
    /// 新建一行属性记录（不含几何）
    CreateRow(CreateRowArgs),
    /// 按条件删除行
    DeleteRows(DeleteRowsArgs),
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
    /// 把数据源完整导出为本地 JSON 镜像文件（离线快照 / 无驱动环境演练）
    ExportMirror {
        /// 输出文件路径，例如 `snapshot.mdb.json`
        #[arg(long, value_name = "FILE")]
        output: String,
        /// 是否同时导出 Access 系统表（`MSys*`）
        #[arg(long)]
        include_system: bool,
    },
}

/// 过滤条件参数（多个子命令共用）
#[derive(Debug, Args, Clone, Default)]
struct FilterArgs {
    /// 按 OBJECTID 精确过滤（所有后端都支持）
    #[arg(long, value_name = "OID")]
    oid: Option<i64>,
    /// WHERE 子句（不含 WHERE 关键字）。镜像后端不支持原生 WHERE，会自动降级为内存过滤
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
    #[command(flatten)]
    filter: FilterArgs,
}

fn run(cli: Cli) -> Result<()> {
    // drivers 命令只探测系统驱动，不需要打开数据库
    if let Command::Drivers = cli.command {
        return cmd_drivers();
    }
    let ws = open_workspace(&cli.database)?;
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
        Command::RebuildIndex { dataset } => cmd_rebuild_index(&ws, &dataset),
        Command::Sql { statement } => cmd_sql(&ws, &statement),
        Command::ExportMirror {
            output,
            include_system,
        } => cmd_export_mirror(&ws, &output, include_system),
    }
}

/// 导出为本地镜像：把后端里的每张表（含 GDB_* 元数据）原样复制到 JSON 文件
fn cmd_export_mirror(ws: &AccessWorkspace, output: &str, include_system: bool) -> Result<()> {
    use pgdb::datastore::{mirror::MirrorBackend, Predicate, SqlBackend};

    let source = ws.backend();
    let target = MirrorBackend::new();
    let mut tables = 0usize;
    let mut rows_total = 0usize;
    for name in source.table_names()? {
        if !include_system && (name.starts_with("MSys") || name.starts_with("~TMP")) {
            continue;
        }
        let cols = source.columns(&name)?;
        target.create_table(&name, cols)?;
        let rows = source.select(&name, &[], &Predicate::All)?;
        let mut inserted = 0usize;
        for row in &rows {
            target.insert(&name, &row.pairs())?;
            inserted += 1;
        }
        rows_total += inserted;
        tables += 1;
        println!("  {name}: {inserted} 行");
    }
    target.save_file(output)?;
    println!("已导出 {tables} 张表、共 {rows_total} 行 -> {output}");
    Ok(())
}

fn open_workspace(path: &str) -> Result<AccessWorkspace> {
    if !Path::new(path).exists() {
        return Err(PgdbError::InvalidArgument(format!("数据源不存在: {path}")));
    }
    AccessWorkspaceFactory.open(path, None)
}

// ------------------------------------------------------------------ 查询类

/// 列出系统里登记的 ODBC 驱动与 DSN，用于排查"驱动没装 / 位数不匹配"
fn cmd_drivers() -> Result<()> {
    #[cfg(feature = "odbc")]
    {
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
        }
        Ok(())
    }
    #[cfg(not(feature = "odbc"))]
    {
        Err(PgdbError::Unsupported(
            "未编译 'odbc' feature，请 cargo build --features odbc 后重试".into(),
        ))
    }
}

fn cmd_info(ws: &AccessWorkspace) -> Result<()> {
    let caps = ws.backend().capabilities();
    let catalog = ws.catalog();
    println!("数据源        : {}", ws.path());
    println!("后端          : {}", ws.backend().kind());
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
    let handle = ws.open_dataset(&args.dataset)?;
    let Some(table) = handle.as_table() else {
        return Err(PgdbError::Unsupported(format!(
            "{} 不是表/要素类",
            args.dataset
        )));
    };
    let sets = parse_assignments(table.fields(), &args.set)?;
    let filter = args.filter.to_filter();

    // ArcEngine 语义：ITable::Update 取游标 -> IRow::put_Value -> IRow::Store
    let mut cursor = table.update(filter)?;
    let mut affected = 0usize;
    while let Some(mut row) = cursor.next_row()? {
        for (name, value) in &sets {
            row.set_value_by_name(name, value.clone())?;
        }
        row.store()?;
        affected += 1;
    }
    ws.backend().flush()?;
    println!("已更新 {affected} 行：{}", join_names(&sets));
    Ok(())
}

fn cmd_set_geometry(ws: &AccessWorkspace, args: &SetGeometryArgs) -> Result<()> {
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
    let n = table.delete_rows(&args.filter.to_filter())?;
    ws.backend().flush()?;
    println!("已删除 {n} 行");
    Ok(())
}

fn cmd_rebuild_index(ws: &AccessWorkspace, dataset: &str) -> Result<()> {
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
