# pgdb-rs

用 **Rust** 解析与管理 **ESRI Personal Geodatabase（`*.mdb`）** 的库与命令行工具。

代码组织**严格对标 ArcGIS ArcEngine / ArcObjects 的对象模型**：`IWorkspaceFactory → IWorkspace → IFeatureWorkspace → IEnumDataset → IDataset → ITable/IFeatureClass → ICursor/IFeatureCursor → IRow/IFeature → Store()`，熟悉 ArcEngine 的人可以零成本上手。

---

## 能力一览

| 能力 | 说明 |
|------|------|
| **遍历三种数据形态** | 独立要素类、独立数据表、**要素数据集内部的要素类**（`IFeatureDataset::Subsets` 语义） |
| **两套元数据模型** | ArcGIS 9.2+ 的 `GDB_Items` 模型；ArcGIS 8.x/9.0 的 `GDB_ObjectClasses` / `GDB_GeomColumns` 旧模型，打开时自动探测 |
| **Shape 二进制编解码** | 点/多点/线/面 + Z/M 变体；字节布局与 shapefile 记录体完全一致（去掉 100 字节文件头） |
| **属性更新** | `ITable::Update` + `IRow::put_Value` + `IRow::Store`，**只 UPDATE 真正赋值的列** |
| **几何更新** | `IFeatureClass::Update` + `IFeature::put_Shape` + `IFeature::Store` |
| **ESRI 一致性自动维护** | 写几何时自动重算 `Shape_Length`/`Shape_Area`、同步 `<表>_SHAPE_Index` 网格记录、更新 `GDB_GeomColumns` 图层范围，并按 ESRI 约定归一化面环方向 |
| **内存后端** | 纯内存镜像后端（无 JSON、不落盘），用于单元测试与无驱动环境的逻辑演练 |
| **空间过滤** | `ISpatialFilter` 语义的包络粗筛 + 内存精算 |
| **CLI 工具** | `list / fields / rows / export-wkt / update-attr / set-geometry / create-feature / delete-rows / rebuild-index / sql` |

---

## 目录结构

```text
src/
├── lib.rs                  # crate 根：模块声明与常用类型的 re-export
├── error.rs                # 统一错误类型 PgdbError（按 IO/后端/SQL/几何/元数据 分类）
├── value.rs                # SqlValue（Transport 层）与 Value（业务层）双向转换
├── field.rs                # Field / Fields / FieldType，对应 IField / IFields / esriFieldType
├── text.rs                 # 中文全角宽度感知的终端排版
├── sql.rs                  # Jet SQL 方言构造（方括号标识符、#日期#、0xHEX 二进制）
├── geom/
│   ├── mod.rs              # Geometry / Vertex / Envelope / GeometryType + WKT 读写
│   ├── codec.rs            # Shape 二进制 <-> Geometry（本项目最核心的字节层）
│   └── ops.rs              # 距离、鞋带公式面积、环向判定、闭环、归一化、合法性校验
├── datastore/
│   ├── mod.rs              # trait SqlBackend + 结构化 Predicate（不用字符串拼 SQL）
│   ├── odbc.rs             # ODBC 后端（Windows ACE 可读写 / Linux MDBTools 只读）
│   └── mirror.rs           # 内存镜像后端（纯内存，无 JSON、不落盘）
└── gdb/
    ├── mod.rs              # 对象模型聚合出口
    ├── factory.rs          # AccessWorkspaceFactory   -> IWorkspaceFactory
    ├── workspace.rs        # Workspace / FeatureWorkspace / DatasetEnum / DatasetHandle
    ├── dataset.rs          # trait DatasetNode        -> IDataset
    ├── metadata.rs         # GDB_* 系统表解析（Items 与 Legacy 双模型）
    ├── table.rs            # Table / TableCore / PgdbTable
    ├── featureclass.rs     # FeatureClass / PgdbFeatureClass / WritePolicy
    ├── featuredataset.rs   # FeatureDataset / PgdbFeatureDataset
    ├── row.rs              # Row / Feature / RowBuffer   -> IRow / IFeature / IFeatureBuffer
    ├── cursor.rs           # RowIter / FeatureIter / InsertRowCursor / InsertFeatureCursor
    └── filter.rs           # QueryFilter / SpatialFilter -> IQueryFilter / ISpatialFilter

src/bin/pgdb-cli.rs         # 命令行工具
examples/                   # traverse / update / shape_codec
tests/                      # harness.rs（构造假库）+ integration.rs（10 个用例）
```

---

## 编译

```bash
cargo build    # 两种后端（jetdb 只读 + ODBC 读写）已全部内置，无需任何 feature 开关
               # Linux 若要从源码编译，需 unixODBC 开发库：apt install unixodbc-dev
cargo test     # 单元测试 + 集成测试 + 文档测试
```

---

## 两种解析后端：由「读写权限」在运行时选择

同一条 `.mdb` 有两条解析路径，**不需要在编译期挑选 feature**，由打开时的读写权限决定：

| 读写权限 | 后端 | 是否需要驱动 | 能力 |
|----------|------|--------------|------|
| `AccessMode::ReadOnly`（**默认**） | 纯 Rust 的 **jetdb** | **不需要**，跨平台（含 WASM） | 仅查询 |
| `AccessMode::ReadWrite`（显式选择） | **ODBC** | 需要与程序位数匹配的 Access/ACE 驱动 | 查询 + 写入 |

**默认权限是只读**：安全（任何写操作都被明确拒绝）且零依赖（无驱动即可探查数据）；
**需要改数据**时才显式以读写权限打开。

```rust
use pgdb::gdb::{AccessMode, AccessWorkspaceFactory};

// 默认（不传权限）即只读：纯 Rust 解析，无需安装任何驱动
let ws = AccessWorkspaceFactory.open("test.mdb", None)?;
// 也可以显式声明
let ws = AccessWorkspaceFactory::open_with_mode("test.mdb", AccessMode::ReadOnly, None)?;

// 读写：必须显式传 AccessMode::ReadWrite（Windows 需装好与程序位数匹配的 ACE 驱动）
let ws = AccessWorkspaceFactory::open_with_mode("test.mdb", AccessMode::ReadWrite, None)?;
```

命令行对应 `--access`（默认 `readonly`）：

```bash
pgdb-cli test.mdb tree                        # 默认只读：jetdb，无需驱动
pgdb-cli --read-only   test.mdb rows QLR      # 兼容的显式简写
pgdb-cli --access readwrite test.mdb info     # ODBC，需驱动（写入必须）
```

> 只读模式下任何写操作都会被**明确拒绝**（并提示改用 `--access readwrite`），
> 不会静默失败或写坏文件。

### ODBC 驱动要求（仅读写模式需要）

| 平台 | 驱动 | 能力 |
|------|------|------|
| Windows | Microsoft Access Driver（`*.mdb`）/ ACE | **读写** |
| Linux | MDBTools ODBC 驱动（`libmdbodbc`） | 仅只读 |

> 只读模式（jetdb）不依赖上表任何驱动，因此在 Linux / CI / 容器里也能完整做数据探查。
> 另外，`tests/harness.rs` 会在内存里构造与真实 PGDB 结构一致的假库，
> 无需任何驱动即可完整跑通单元测试与集成测试。

---

## 快速上手

```bash
# 1. 遍历（独立要素类 + 独立表 + 要素数据集 + 数据集内要素类）
cargo run --example traverse -- 你的库.mdb

# 2. 属性与几何更新（需读写权限；含 WritePolicy 自动维护的一致性数据）
cargo run --example update -- 你的库.mdb

# 3. Shape 二进制布局演示（无需驱动）
cargo run --example shape_codec
```

---

## 命令行工具 `pgdb-cli`

```bash
pgdb-cli [全局选项] <数据源> <子命令> [选项]
# 数据源为真实 *.mdb / *.accdb
```

全局选项（决定后端与写保护）：

| 选项 | 作用 |
|------|------|
| `--access readonly`（**默认**，`--read-only` 为兼容简写） | 纯 Rust **jetdb** 解析（无需驱动、跨平台），仅查询 |
| `--access readwrite` | **ODBC** 驱动解析（可写，需匹配位数的 Access/ACE 驱动），写入时必须显式指定 |
| `--dry-run` | 更新/删除命令只做预检（命中行数/范围/可写性），不写库 |

| 子命令 | 作用 | 主要选项 |
|--------|------|----------|
| `drivers` | 列出系统 ODBC 驱动与 DSN（排查驱动未装/位数不匹配） | – |
| `info` | 后端、元数据模型、数据集统计 | – |
| `tree` | 目录树（含要素数据集下钻） | – |
| `list` | 扁平列出数据集 | `--feature-classes` / `--in-dataset` |
| `fields <ds>` | 字段定义、空间参考、图层范围、网格 | – |
| `rows <ds>` | 打印数据行 | `--oid` `--where` `--fields` `--limit` |
| `export-wkt <fc>` | 导出 WKT | `--output` `--precision` `--oid` |
| `preflight <ds>` | 编辑前体检：命中行数/范围/可写性，只读不写库 | `--oid` `--where` |
| `update-attr <ds>` | 批量属性更新（`UpdateSearchedRows`） | `--set FIELD=VALUE`（可多次）`--oid` `--where` `--all` |
| `set-geometry <fc>` | 几何更新 | `--wkt` / `--wkt-file`、`--oid` `--where` |
| `create-feature <fc>` | 新建要素 | `--wkt`、`--set FIELD=VALUE` |
| `create-row <ds>` | 新建属性行 | `--set FIELD=VALUE` |
| `delete-rows <ds>` | 删除行 | `--oids 1,2,3`（`ITable::DeleteRows`）/ `--oid` `--where` / `--yes` |
| `delete-all-rows <ds>` | 清空整张表（`DeleteAllRows`） | `--yes` |
| `rebuild-index <fc>` | 重建 `<表>_SHAPE_Index` 并重算图层范围 | – |
| `sql <stmt>` | 原始 SQL（仅 ODBC 后端可用） | – |

数据集名支持 ArcMap 风格的限定名：`Hydrology\Ponds`。

示例：

```bash
pgdb-cli 你的库.mdb tree
# 要素类      Roads（几何类型：线），2 行
# 表          OwnerTable，2 行
# 要素数据集  Hydrology（含 1 个要素类）
#     要素类      Hydrology\Ponds（几何类型：面），2 行

# 只读探查：默认权限即 readonly（jetdb 后端，无需任何驱动）
pgdb-cli 你的库.mdb rows Roads --limit 10

# 编辑前体检 / 演练
pgdb-cli --dry-run 你的库.mdb delete-rows Roads --oids 1,2,999
pgdb-cli 你的库.mdb preflight Roads --where "NAME = 'G1'"

# 写操作必须显式 --access readwrite（ODBC 后端，需驱动）
pgdb-cli --access readwrite 你的库.mdb update-attr Roads --oid 1 --set NAME=长安街
pgdb-cli --access readwrite 你的库.mdb update-attr Roads --set REMARK=已核对 --all

# 删除：OID 列表（忽略 --where），或按条件；都必须 --yes
pgdb-cli --access readwrite 你的库.mdb delete-rows Roads --oids 3,4 --yes
pgdb-cli --access readwrite 你的库.mdb delete-rows Roads --where "NAME = '旧路'" --yes
pgdb-cli --access readwrite 你的库.mdb delete-all-rows 临时表 --yes
```

Windows 下的驱动自动探测顺序（DSN-less，`DBQ=` 直指文件）：

1. `Microsoft Access Driver (*.mdb, *.accdb)`（ACE，Access 2007+ 可读写）
2. `Microsoft Access Driver (*.mdb)`（Jet 4，仅 mdb，可读写）

> 注意：**驱动位数必须与程序位数一致**——64 位 exe 需要 64 位 ACE
> （`AccessDatabaseEngine_X64.exe`），32 位 exe 需要 32 位 ACE，两者不能共存。
> 也可用环境变量 `PGDB_ODBC_CONN='Driver={...};DBQ=C:\data\a.mdb;'` 显式指定连接串，
> 或建 DSN 后传 `DSN=名字`。连接串兼容 GDAL 风格的 `PGeo:DSN=xxx`。

---

## 库用法

### 遍历（对应 `IWorkspace::get_Datasets` + `IFeatureDataset::Subsets`）

```rust
use pgdb::gdb::{
    AccessWorkspaceFactory, DatasetNode, FeatureClass, FeatureWorkspace, Table, Workspace,
    WorkspaceFactory, QueryFilter,
};

let ws = AccessWorkspaceFactory.open("sample.mdb", None)?;

let mut en = ws.datasets()?;                     // IEnumDataset
while let Some(handle) = en.next_dataset() {
    println!("{}", handle.describe_line());
    match handle.kind() {
        pgdb::gdb::DatasetKind::FeatureClass => {
            let fc = handle.as_feature_class().unwrap();
            println!("几何类型 {}", fc.shape_type().label());
        }
        pgdb::gdb::DatasetKind::FeatureDataset => {
            let children = handle.as_feature_dataset().unwrap().subsets();
            for c in children { println!("  {}", c.describe_line()); }
        }
        _ => {}
    }
}

// 也可以按限定名直接打开： `要素数据集\要素类`
let fc = ws.open_feature_class("Hydrology\\Ponds")?;
```

### 属性更新（`IRow::Store`）

```rust
let table = ws.open_table("OwnerTable")?;
let mut cursor = table.update(QueryFilter::new())?;
while let Some(mut row) = cursor.next_row()? {
    row.set_value_by_name("REMARK", pgdb::Value::String("已核对".into()))?;
    row.store()?;                                 // 只 UPDATE 被赋值的列
}
```

### 几何更新（`IFeature::Store`）

```rust
use pgdb::geom::{geometry_from_wkt, AsWkt, Geometry, Vertex};

let fc = ws.open_feature_class("Roads")?;
let mut cursor = fc.update_features(QueryFilter::for_oid(2))?;
while let Some(mut feature) = cursor.next_feature()? {
    // 写 Shape 的同时：重算 Shape_Length，同步 Roads_SHAPE_Index，扩展 GDB_GeomColumns 范围
    feature.set_geometry(&geometry_from_wkt("LINESTRING(20 20, 30 30, 40 25)")?)?;
    feature.store()?;
}

// 新建要素：IFeatureClass::Insert + IFeatureBuffer
let mut insert = fc.insert_feature_cursor()?;
insert.buffer().set_value("NAME", pgdb::Value::String("新建道路".into()));
insert.set_geometry(&Geometry::line(vec![
    Vertex::new(0.0, 0.0),
    Vertex::new(10.0, 5.0),
]))?;
let new_oid = insert.insert_feature()?;
insert.flush()?;
```

### 批量编辑（`ITable::UpdateSearchedRows` / `DeleteSearchedRows`）

按 [`QueryFilter`] 批量更新/删除，返回 [`EditResult`]（命中数/影响数/范围），
用 [`EditOptions`] 声明防护要求——**默认拒绝全表、允许零命中**：

```rust
use pgdb::gdb::{EditOptions, EditResult, EditScope, QueryFilter, Table};

let table = ws.open_table("OwnerTable")?;

// 按条件更新：返回命中与影响行数，零命中不再是"静默无事"
let result: EditResult = table.update_searched_rows(
    &[("REMARK".to_string(), pgdb::Value::String("已核对".into()))],
    &QueryFilter::new().with_where("NAME = '北京市'"),
    EditOptions::default(),               // 拒绝全表 + 允许零命中
)?;
println!("{result}");                     // 命中 1 行，影响 1 行（按条件过滤）

// 全表更新必须显式放行（防止 QueryFilter::new() 笔误清库）
table.update_searched_rows(
    &[("REMARK".to_string(), pgdb::Value::String("批量备注".into()))],
    &QueryFilter::new(),
    EditOptions::default().whole_table(),
)?;

// 按 OBJECTID 列表删除（ITable::DeleteRows 的 OID 数组语义）
let result = table.delete_rows(&[3, 4, 5])?;
if result.matched < 3 {
    eprintln!("有 {} 个 OID 不存在", 3 - result.matched as usize);
}

// "必须命中"的场景：零命中直接报 NotFound，适合单条业务操作
table.delete_searched_rows(
    &QueryFilter::for_oid(2),
    EditOptions::default().require_hit(),
)?;

// 清空整张表（ITable::DeleteAllRows）
let result = table.delete_all_rows()?;

// 编辑前体检（ISelectionSet::Count + IWorkspace::IsReadOnly），只读不写库
let pre = table.preflight_edit(&QueryFilter::new())?;
println!("{pre}");                        // 预检：命中 2 行（全表），数据源可写
```

对要素类（`FeatureClass`）调用同样的方法时，删除会**同步清理
`<表>_SHAPE_Index` 并回缩 `GDB_GeomColumns` 图层范围**（被删要素不触及
边界时跳过 O(n) 重算），几何更新接受 ESRI 二进制 / WKT 文本 / `Null`
三态值并统一归一落库。

### 写入策略 `WritePolicy`

几何写入默认会做这些 ESRI 一致性维护，可按需关闭：

| 开关 | 默认 | 作用 |
|------|------|------|
| `maintain_shape_length_area` | 开 | 自动重算 `Shape_Length` / `Shape_Area` |
| `maintain_shape_index` | 开 | 同步 `<业务表>_SHAPE_Index`（`IndexedObjectId, MinGX, MinGY, MaxGX, MaxGY`） |
| `maintain_layer_extent` | 开 | 更新 `GDB_GeomColumns` 的 `Extent*` |
| `normalize_polygon_orientation` | 开 | 面外环按 ESRI 约定整理为顺时针 |
| `default_grid_size` | 420 | 网格尺寸（PGDB 常用 420） |
| `shape_index_suffix` | `_SHAPE_Index` | 空间索引表后缀 |

```rust
use pgdb::gdb::{AccessWorkspaceFactory, WritePolicy};

let policy = WritePolicy { maintain_shape_index: false, ..WritePolicy::default() };
// 默认打开权限即只读（jetdb，无需驱动）；写策略在打开后通过工作空间选项生效
// 需要写入时显式传 AccessMode::ReadWrite（走 ODBC，需驱动）
let ws = AccessWorkspaceFactory::open_with_mode("你的库.mdb", AccessMode::ReadOnly, None)?;
// policy 可用于后续更新时控制是否维护空间索引等一致性数据
```

> 为什么必须维护 `_SHAPE_Index`：ArcMap/ArcGIS Pro 定位与选中要素依赖这套网格记录，缺更新会出现"属性表里有值、地图里选不中/缩放到图层失败"的现象。

---

## ArcObjects 映射表

| ArcObjects | 本项目 |
|------------|--------|
| `IWorkspaceFactory` / `AccessWorkspaceFactory` | `WorkspaceFactory` / `AccessWorkspaceFactory` |
| `IWorkspace` | `Workspace` |
| `IFeatureWorkspace` | `FeatureWorkspace` |
| `IEnumDataset` | `DatasetEnum`（`next_dataset` / `reset`） |
| `IDataset` | `DatasetNode` |
| `IFeatureDataset` | `FeatureDataset`（`subsets`） |
| `ITable` | `Table`（`search` / `update` / `insert_cursor` / `get_row`） |
| `ITable::UpdateSearchedRows` / `ITable::DeleteSearchedRows` | `Table::update_searched_rows` / `Table::delete_searched_rows`（`EditOptions` + `EditResult`） |
| `ITable::DeleteRows`（OID 数组） | `Table::delete_rows(&[i64])` |
| `ITable::DeleteAllRows` | `Table::delete_all_rows` |
| `ISelectionSet::Count` + `IWorkspace::IsReadOnly` | `Table::preflight_edit`（`Preflight`） |
| `IWorkspaceEdit` 的确认语义 | `EditOptions::whole_table()` / `require_hit()` |
| `IFeatureClass` | `FeatureClass`（`search_features` / `update_features` / `insert_feature_cursor`） |
| `IRowBuffer` / `IFeatureBuffer` | `RowBuffer` |
| `IRow` / `IFeature` | `Row` / `Feature`（`store` / `delete` / `set_geometry`） |
| `ICursor` / `IFeatureCursor` | `RowIter` / `FeatureIter` / `InsertRowCursor` / `InsertFeatureCursor` |
| `IQueryFilter` / `ISpatialFilter` | `QueryFilter` / `SpatialFilter` |
| `IField` / `IFields` / `esriFieldType` | `Field` / `Fields` / `FieldType` |
| `IGeometry` / `IPoint` / `IPolyline` / `IPolygon` / `IEnvelope` | `Geometry` / `Vertex` / `Path` / `Envelope` |

---

## Shape 二进制格式

业务表 `Shape` 列（OLE Object / Long Binary）里存放的就是 **shapefile 记录体**（小端），与 `.shp` 的唯一差异是没有 100 字节文件头：

```text
面 POLYGON((0 0, 0 10, 10 10, 10 0, 0 0))
  [0..4)    ShapeType = 5（i32）
  [4..36)   Box       = 4 × f64  (Xmin, Ymin, Xmax, Ymax)
  [36..40)  NumParts  = 1
  [40..44)  NumPoints = 5
  [44..48)  Parts[0]  = 0
  [48..128) Points    = 5 × (x, y) = 5 × 16 字节
  总长 128 字节
```

其它类型布局：

| 类型 | ShapeType | 记录体（在 ShapeType 之后） |
|------|-----------|------------------------------|
| Null | 0 | 无 |
| Point | 1 | X, Y（f64 × 2） |
| MultiPoint | 8 | Box + NumPoints + Points（**没有 NumParts**） |
| PolyLine | 3 | Box + NumParts + NumPoints + Parts[] + Points[] |
| Polygon | 5 | 同上 |
| 含 Z | 11/13/15/18 | 追加 Zrange(2×f64) + Z[] |
| 含 M | 21/23/25/28 | 追加 Mrange(2×f64) + M[] |

因为字节与 shapefile 记录体一致，本库编码出的 `Shape` 值可以被 GDAL / QGIS / shapefile 解析器直接读取（见 `tests/integration.rs::raw_shape_bytes_stay_shapefile_compatible`）。

WKT 读写遵循 OGC：`POLYGON((环1),(环2))`、`MULTILINESTRING((路径1),(路径2))`、`MULTIPOINT(0 0, 1 1)`、`POINTZ(x y z)`；解析器同时也接受 `MULTIPOLYGON(((环)),((环)))` 标准嵌套形式。

---

## 元数据如何定位三种数据形态

**Items 模型（ArcGIS 9.2+）**

```text
GDB_Items                     逻辑对象（Type 指向 GDB_ItemTypes）
GDB_ItemTypes                 "Feature Class" / "Table" / "Feature Dataset"
GDB_ItemRelationships         DatasetInFeatureDataset 关系 -> 找到要素数据集的父子关系
GDB_ItemRelationshipTypes     关系类型字典
DatasetInfo1 = Shape 字段名；DatasetSubtype1 = 要素类型；DatasetSubtype2 = 几何类型
```

**Legacy 模型（ArcGIS 8.x/9.0）**

```text
GDB_ObjectClasses    ID / Name / DatasetType(1=表, 3=要素类) / DatasetID(非空 => 位于要素数据集内)
GDB_FeatureClasses   ObjectClassID / FeatureType / GeometryType(3=线, 4=面) / ShapeFieldName
GDB_FeatureDataset   ID / Name / SRID
GDB_GeomColumns      TableName / FieldName / ShapeType / Extent* / IdxOrigin* / IdxGridSize / SRID
GDB_SpatialRefs      ID / FalseX / FalseY / XYUnits / SRTEXT
GDB_FieldInfo        TableName / FieldName / AliasName（字段别名）
```

不同版本列名有差异，元数据采集对 `DatasetID`/`ParentID`/`ContainerID`、`ObjectClassID`/`ClassID`、`DatasetInfo1`/`ShapeFieldName` 等采用**多候选列名容错**。

---

## 内存镜像后端

`mirror` 模块提供一个纯内存的 `MirrorBackend`（**不落盘、不涉及任何 JSON 序列化**）。

用途：① 单元测试 / 集成测试里充当确定性的数据源，无需任何 ODBC 驱动即可演练目录遍历、
要素更新、Shape 二进制读写等全部逻辑；② 作为 `SqlBackend` 的一种实现，与 ODBC 后端共享上层的
目录 / 游标逻辑。

所有数据都保存在进程内存中；要持久化到磁盘，请直接对真实 `*.mdb` 使用 ODBC 后端。本库不提供
把内存镜像另存为 `.mdb` 或 JSON 的能力——查询 / 更新 / 删除只面向真实的 Personal Geodatabase。

---

## 测试

```bash
cargo test
# lib: 52 个单元测试（几何编解码 / WKT / 谓词 / SQL 构造 / 排版 / 编辑防护）
# tests/access_mode.rs: 6 个用例（双后端选择、数据集/几何/中文、写拒绝）
# tests/edit_ops.rs: 13 个用例（批量编辑契约：EditOptions 防护、EditResult、
#                     preflight、索引清理、范围回缩、WKT/Blob/Null 三态几何）
# tests/integration.rs: 10 个用例（目录树遍历、双元数据模型、游标读写、
#                       空间过滤、面环方向归一化、索引与范围一致性、shapefile 兼容）
# tests/test_mdb.rs: 9 个基准库验收用例（见下，默认忽略）
# doc-tests: 3 个（README 级示例保证可编译）
```

### 基准库验收测试（`tests/test_mdb.rs`）

以 `tests/fixtures/test.mdb` 为基准（ArcGIS 10.1 / `GDB_Items` 模型），
覆盖需求中的全部数据形态与中文命名场景：

| 对象 | 形态 | 几何 | 行数 |
|------|------|------|------|
| `ZD`（宗地，位于要素数据集 `BDC不动产` 内） | 数据集内要素类 | 面 | 5 |
| `界址点`（JZD，位于要素数据集 `BDC不动产` 内） | 数据集内要素类 | 点 | 112 |
| `JZX`（界址线） | 独立要素类 | 线 | 5 |
| `其他` | 独立要素类 | 面 | 5 |
| `QLR`（权利人） | 独立表 | — | 2 |
| `附加`（FJ） | 独立表 | — | 2 |

```bash
# Linux 安装只读驱动；Windows 安装 Access Database Engine
sudo apt install unixodbc odbc-mdbtools gdal-bin

cargo test --test test_mdb -- --ignored --test-threads=1
```

9 个用例分别是：工作空间信息、三类数据集遍历、中文名称查找、要素类结构、
含中文列的字段定义、属性值（含 NULL 与中文）、几何解码、写回往返、
GDAL 交叉验证。未安装驱动时会打印提示并**自动跳过**，不会误报失败。
设置 `PGDB_TEST_MDB=<路径>` 可切换到其它 mdb 文件。

### 中文对象名支持（表名 / 要素类名 / 数据集名）

中文（及任何非 ASCII）名称在 mdbtools 下曾出现两类失败，现已修复：

1. **驱动只导出窄字符 API**。`libmdbodbc.so` 只提供 `SQLExecDirect` /
   `SQLTables` / `SQLColumns` 的窄字符版本，没有 `*W` 版本。若让 odbc-api
   走宽字符（UTF-16）路径，unixODBC 的宽→窄桥接会把中文名破坏成乱码，
   进而导致"找不到表"。因此 `Cargo.toml` 中 odbc-api 必须以 **`narrow`**
   特性编译：

   ```toml
   odbc-api = { version = "8", optional = true, default-features = false,
                features = ["narrow", "odbc_version_3_80"] }
   ```

   （`odbc_version_3_80` 是 `narrow` 关闭默认特性后必须显式补回的 ODBC 版本特性。）

2. **列发现三级回退**。mdbtools 的 `SQLColumns` 不给 `COLUMN_SIZE`
   （`COLUMN_SIZE` / `DECIMAL_DIGITS` 恒为 NULL），中文表名还会进一步退化，
   导致字段"没有长度"。本库按信息完整度依次尝试：

   | 顺序 | 途径 | 提供的信息 | 适用 |
   |------|------|-----------|------|
   | 1 | `DESCRIBE TABLE <t>` | 列名 + 类型 + 字节宽度（Text 除以 2 得字符数） | mdbtools 首选 |
   | 2 | `SQLColumns` 目录函数 | 列名 + 类型 + `COLUMN_SIZE` + `DECIMAL_DIGITS` + `NULLABLE` | Windows ACE/Jet 首选 |
   | 3 | `SELECT * ... WHERE 1 = 0` + `SQLDescribeCol` | 仅列名 + 类型 | 最后兜底 |

   `DESCRIBE TABLE` 直接读 Jet 表定义页，对中文表名（如 `附加`、`其他`）同样有效。

### Windows 中文乱码的根因与修复

在 Windows + ACE/Jet 驱动下曾出现两类乱码，二者**根因不同**：

| 现象 | 根因 | 修复位置 |
|------|------|---------|
| 日志/控制台输出乱码：`要素类 BDC不动产` → `瑕佺礌绫? BDC涓嶅姩浜?` | Rust 输出 UTF-8 字节，Windows 控制台按**本地代码页**（简中 GBK/CP936）解码 | `pgdb::init_console_utf8()` |
| 表名/字段值乱码：`界址点` → `鐣屽潃鐐?` | 窄字符 API 传出的 UTF-8 被驱动按 CP936 解码成 UTF-16 再编回 ANSI，字节被改写（末字节 `B9` 被吞成 `3F`） | `datastore::odbc` 的出站/入站编码桥 |

**问题 1（日志乱码）**：`pgdb-cli` 的 `main` 第一条语句调用 `pgdb::init()`，
其中 `init_console_utf8()` 会：

```rust
SetConsoleOutputCP(65001);   // 控制台输出代码页 -> UTF-8
SetConsoleCP(65001);         // 控制台输入代码页 -> UTF-8（接收中文 --where 参数）
```

仅在检测到真实控制台时执行（重定向到文件/管道时跳过，此时字节本就是 UTF-8）。
`PGDB_NO_UTF8_CONSOLE=1` 可关闭。非 Windows 平台该函数为空操作。

> 也可以直接用 `chcp 65001` 手动切换，但要求程序自身也调用
> `SetConsoleOutputCP` —— 否则子进程/管道场景下仍会乱码。

**问题 2（表名乱码）**：微软文档对此有明确说明 —— ODBC 3.5+ 的 Driver Manager
只提供**有限**的 Unicode↔ANSI 映射；ANSI 应用访问 Jet 4.0 时驱动只能暴露
`SQL_CHAR/SQL_VARCHAR/SQL_LONGVARCHAR`，且该限制同样适用于
"old formats ... with the Jet 4.0 Database Engine"。

关键观察：**该往返变换是双射的**（GBK 对任意字节序列有定义且可逆），
所以只要在 Windows 上对每个进出驱动的字符串各做一次"逆变换"，乱码即可完全消除：

```
出站 encode_outbound:  UTF-8 字节 ──按 CP936 解码──> 文本 ──按 CP936 编码──> 交给驱动的字节
入站 decode_inbound:   驱动字节 ──按 CP936 解码──> 文本 ──还原为 UTF-8 字节──> 上层
```

| 环节 | 函数 | 说明 |
|------|------|------|
| 入站（读取） | `decode_inbound` | **先判断是否为合法 UTF-8**：是则无损直通（Linux/mdbtools 与部分 ACE 版本走的正是这条路），否则按 ANSI 代码页解码 |
| 出站（SQL / 连接串） | `encode_outbound` | Windows 上按 ANSI 代码页编码；非 Windows 直接返回 UTF-8 |
| 连接串（含中文路径） | `connect_with_encoded_string` | 走 `Environment::driver_connect` 而非 `connect_with_connection_string`，绕开后者硬编码的 `&str`→UTF-8 转发 |

`decode_inbound` 的"合法 UTF-8 直通"判断是**防回归的关键**：它保证了修复不会把
本来正确的数据修坏。相关的契约测试见 `src/datastore/odbc.rs` 的 `encoding_tests`
模块（15 个用例，覆盖编码桥、SQLSTATE 说明、大二进制写入阈值）。

> 注：v1 的 `narrow` 特性仍然必须保留（见上文）——它是让 odbc-api 调用窄字符 API
> 而非宽字符桥接的前提；本节的编码桥是在此基础上补齐 Windows 侧的字节口径。

**问题 3（驱动诊断消息乱码）**：这是最容易漏掉的一处——驱动报错时的
`State: 01004 / 22003 / 42000` 消息同样乱码，但走的是**完全不同的代码路径**：

```
odbc_exec_direct()
  └─ SQL_SUCCESS_WITH_INFO / SQL_ERROR
       └─ odbc_api::handles::logging::log_diagnostics()   <- 库内部自动调用
            └─ slice_to_cow_utf8() -> String::from_utf8_lossy()   <- GBK 字节被 lossy 解
```

这条路径发生在 odbc-api 内部，应用层**没有插入时机**。实测三者的对应关系：

| 驱动原文（GBK） | `from_utf8_lossy` 结果 |
|----------------|----------------------|
| `字符串数据，右截断` | `�ַ������ݣ��ҽض�` |
| `数值超出范围 在行号 17 (MHigh) 中` | `��ֵ������Χ ���к� 17 (MHigh) ��` |
| `无效的 SQL 语句；在此 'DELETE'、'INSERT'…` | `��Ч�� SQL ��䣻�ڴ� 'DELETE'��'INSERT'…` |

**对策**：`log_diagnostics` 开头有一句早退判断 ——

```rust
if log::max_level() < Level::Warn { return; }
```

于是本库用 `DiagnosticCapture`（RAII）在**调用驱动期间**把 `log::max_level`
压到 `Error`，让 odbc-api 静默跳过；随后自己用 `Diagnostics` 接口重新读取原始
字节，经 `decode_inbound` 桥接后再 `warn!` 输出。`Drop` 实现保证异常路径
（`?` 提前返回）下日志级别也一定恢复。

同时补了一份 **SQLSTATE → 中文说明**映射（`sqlstate_hint`），
这样即使某些驱动的消息文本仍不可读，也能从状态码判断问题类别：

```
ODBC [42000] (native -3500) 无效的 SQL 语句；在此 'DELETE'… —— SQL 语法错误或访问违规
ODBC [22003] (native 34) 数值超出范围 在行号 17 (MHigh) 中 —— 数值超出范围（写入值超过字段容量）
ODBC [01004] (native 0) 字符串数据，右截断 —— 字符串数据被右截断（结果列缓冲区不足，数据本身未丢失）
```

### Windows 上非标准 SQL 语句的兼容性

排查 `42000 无效的 SQL 语句；在此 'DELETE'、'INSERT'、'PROCEDURE'、'SELECT'、或 'UPDATE'`
时，根因是**本库向 Microsoft ACE/Jet 发了它不认识的语句**：

| 语句 | mdbtools | Microsoft ACE/Jet | 处理 |
|------|----------|-------------------|------|
| `DESCRIBE TABLE <t>` | ✅ 支持（读 Jet 表定义页） | ❌ **不支持**，报 `42000 native -3500` | 按驱动名门控，**只对 mdbtools 发** |
| `SELECT @@IDENTITY` | ❌ | ✅ 支持 | 仅写入后调用，失败静默降级 |
| `SELECT * ... WHERE 1 = 0` | ✅ | ✅ | 通用 |

`DESCRIBE TABLE` 的报错信息（"无效的 SQL 语句"）极具误导性——听起来像 SQL 拼接
写错了，实际是驱动能力差异。因此 `columns()` 现在先查 `supports_describe_table()`，
**绝不盲发**：

| 顺序 | 途径 | 适用驱动 |
|------|------|---------|
| 1 | `DESCRIBE TABLE <t>` | **仅** mdbtools（`mdb-odbc` / `libmdbodbc` / `mdbtools`） |
| 2 | `SQLColumns` 目录函数 | Windows ACE/Jet 首选 |
| 3 | `SELECT * ... WHERE 1 = 0` + `SQLDescribeCol` | 最后兜底 |

### 大二进制写入：绕开 Jet 的 64,000 字符 SQL 上限

几何 Shape / OLE 写入原本走 Jet SQL 的十六进制字面量 `0x0102...`，
每个字节占 2 个**字符**。而 Access 官方规格限定
"Number of characters in an SQL statement **approximately 64,000**"，
超出后驱动报：

```
State: 22003, Native error: 34
Message: 数值超出范围 在行号 17 (MHigh) 中
```

`MHigh` 是 Jet 内部的 Memo-High 记号，指的正是超长的字面量 token。
换算下来单个二进制值超过约 **31 KB** 就会触发，而真实面要素动辄几万字节。

**修复**：引入阈值 `MAX_HEX_LITERAL_BYTES = 24 KiB`（对应约 48,000 字符，
为 `UPDATE <表> SET <列> = … WHERE …` 的其余部分留足余量）：

| 值大小 | 路径 | 说明 |
|--------|------|------|
| ≤ 24 KiB | 内联 `0x...` 字面量 | 兼容极简驱动，无参数绑定要求 |
| > 24 KiB | **参数绑定**（`?` + `IntoParameter`） | 参数不占 SQL 文本长度，几 MB 几何体也能一次写入 |

对应函数：`split_bindable` / `render_value` / `exec_write` / `exec_insert` /
`exec_with_binary_params`。阈值与占位符渲染均有契约测试覆盖
（`hex_literal_threshold_is_within_sql_limit`、`large_binary_values_switch_to_parameters`、
`render_value_uses_placeholder_for_bound_binary`）。

### 元数据编码注意：`esriGeometryType` ≠ Shape 二进制编码

`GDB_GeomColumns.ShapeType`、`GDB_FeatureClasses.GeometryType`、`GDB_Items.DatasetSubtype2`
使用 **ArcObjects `esriGeometryType`** 枚举（1=点，2=多点，3=线，4=面，5=矩形）；
而 Shape 列的二进制 blob 首字节使用 **shapefile ShapeType** 编码（1=点，3=线，5=面，8=多点）。
两者数值不同，自造数据时不可混用——本库发现层按前者解析，几何解码层按后者解析。

### 关于 GDAL 交叉验证的口径

GDAL 的 **PGeo 驱动对非 ASCII 图层名的行数统计不可靠**：对本基准库，它把
`界址点` 报成 0 行、`附加` 报成 0 行，而 Jet 表定义页与 `mdb-sql` 给出的真实值
分别是 112 与 2。因此 `tests/test_mdb.rs` 中：

- ASCII 图层（`ZD` / `JZX`）→ 与 GDAL 交叉验证行数；
- 中文图层（`界址点` / `其他` / `附加`）→ 结构（图层名、几何类型）与 GDAL 对照，
  行数以本库为准，并已用 `mdb-sql` + Jet 表定义页双重核实。

此外，`mdb-schema` 对中文表名会报出偏大的 Text 长度（如 `附加` 报 20 而非 10），
本库以 `DESCRIBE TABLE` / `GDB_FieldInfo` 的口径为准，与 Access 自身的
`COLUMN_SIZE` 一致。

---

### 真实 `.mdb` 的 ODBC 集成测试（`tests/odbc_real.rs`）

默认跳过，设置环境变量后启用（详见该文件头注释）：

```bash
# 生成真实 mdb 样本（GDAL 的 PGeo 驱动只读、无法创建 mdb，因此用 Java + Jackcess 造库）
cd tools/make_mdb
javac -encoding UTF-8 -cp "jackcess-4.0.5.jar:commons-lang3-3.12.0.jar:commons-logging-1.2.jar" MakeMdb.java
java -cp ".:jackcess-4.0.5.jar:commons-lang3-3.12.0.jar:commons-logging-1.2.jar" MakeMdb /tmp/sample_legacy.mdb --model=legacy
java -cp ".:jackcess-4.0.5.jar:commons-lang3-3.12.0.jar:commons-logging-1.2.jar" MakeMdb /tmp/sample_items.mdb --model=items

# Linux 安装只读驱动；Windows 安装 Access Database Engine
sudo apt install unixodbc odbc-mdbtools gdal-bin

# 运行（含 GDAL ogrinfo 交叉验证；写回测试在只读驱动下自动跳过）
PGDB_TEST_MDB=/tmp/sample_legacy.mdb \
PGDB_TEST_MDB_ITEMS=/tmp/sample_items.mdb \
    cargo test --test odbc_real -- --ignored --test-threads=1
```

---

## 已知限制

- **真实 `.mdb` 写入**依赖 Windows + ACE/Jet ODBC 驱动；Linux 上的 MDBTools 驱动为只读，本库会自动把 `capabilities.writable` 置为 false。
- mdbtools 的 SQL 解析器不支持 `SELECT ... AS <别名>`（`syntax error`），也不支持反引号引用标识符——请用 `[]` 或 `""`。经本库 `sql` 子命令执行时请遵循该方言。
- mdbtools 的 `SQLColumns` 不返回 `COLUMN_SIZE` / `DECIMAL_DIGITS`，因此列长度改由 `DESCRIBE TABLE` 提供（见上文"中文对象名支持"）。注意 `DESCRIBE TABLE` 是 mdbtools 专有扩展，**Microsoft ACE/Jet 不支持**，本库已按驱动名门控。
- 单个二进制字段（几何/OLE）超过 24 KiB 时改走参数绑定；一次写入**最多 3 个**大二进制字段（`exec_with_binary_params` 的分支上限），超出需分批更新。
- `SELECT @@IDENTITY` 在 ACE/Jet 可用，在 mdbtools 不可用（写入后取新 OBJECTID 会降级为返回 0 并打 WARN）。
- 尚未支持：拓扑/几何网络/关系类/注记的文字排布、`MULTIPATCH` 的完整语义、`SHAPE_AREA/LENGTH` 之外的地理数据库行为（子类型、属性域、编辑版本化）。
- `QueryFilter::where_clause` 在镜像后端会被忽略（降为全表内存过滤），建议优先用 `--oid` / `Predicate::FieldEq`。
- 镜像 → 真实 mdb 的批量回写尚未提供。

## 许可

MIT OR Apache-2.0
