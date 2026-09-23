# Windows 本地编译指南

本文说明如何把本项目源码包（`pgdb-rs-src.zip`）在 Windows 上编译为可执行程序，
并用真实的 `.mdb` 做读写验证。

## 1. 环境准备

| 组件 | 说明 | 获取方式 |
|------|------|----------|
| Rust 1.70+ | 编译工具链 | https://rustup.rs 或 `winget install Rustlang.Rustup` |
| MSVC Build Tools | 默认 `x86_64-pc-windows-msvc` 目标需要 | https://visualstudio.microsoft.com/visual-cpp-build-tools/ （勾选"使用 C++ 的桌面开发"） |
| Access Database Engine | ODBC 驱动（读写 `.mdb` 必需） | 微软官网搜索 "Microsoft Access Database Engine 2016 Redistributable" |

网络较慢时建议先给 cargo 配国内镜像（`%USERPROFILE%\.cargo\config.toml`）：

```toml
[source.crates-io]
replace-with = 'rsproxy'
[source.rsproxy]
registry = "sparse+https://rsproxy.cn/index/"
```

## 2. 编译

```bat
:: 解压 pgdb-rs-src.zip 后进入目录
cd pgdb-rs

:: 默认（仅内存后端，CLI 仍需 ODBC 才能打开真实 mdb）
cargo build --release

:: 完整版（ODBC 直连真实 mdb，推荐）
cargo build --release

:: 产物
target\release\pgdb-cli.exe
```

32 位程序（只有 32 位 ACE 时才需要）：

```bat
rustup target add i686-pc-windows-msvc
cargo build --release --target i686-pc-windows-msvc
```

## 3. 安装 Access 驱动（关键）

- 64 位 exe ↔ 64 位 ACE（`AccessDatabaseEngine_X64.exe`）
- 32 位 exe ↔ 32 位 ACE（`AccessDatabaseEngine.exe`）
- **位数不匹配是连接失败的最常见原因**，二者不能共存安装。
- 若本机装了 Office，通常已带 ACE；位数以 Office 为准。

安装后验证：

```bat
pgdb-cli.exe x drivers
```

应列出 `Microsoft Access Driver (*.mdb, *.accdb)` 或
`Microsoft Access Driver (*.mdb)`。

## 4. 快速验证

```bat
:: 目录树 / 信息
pgdb-cli.exe D:\data\你的库.mdb info
pgdb-cli.exe D:\data\你的库.mdb tree
pgdb-cli.exe D:\data\你的库.mdb export-wkt Roads

:: 属性更新（IFeature::Store 语义）
pgdb-cli.exe D:\data\你的库.mdb update-attr Roads --oid 1 --set NAME=长安街

:: 几何更新（自动维护 Shape_Length / Shape_Area / 空间索引 / 图层范围）
pgdb-cli.exe D:\data\你的库.mdb set-geometry Roads --oid 2 --wkt "LINESTRING(20 20, 30 30)"

:: 要素数据集内的要素类用限定名
pgdb-cli.exe D:\data\你的库.mdb export-wkt Hydrology\Ponds
```

连接失败时按 `info`/`drivers` 的提示排查，或显式指定连接串：

```bat
set PGDB_ODBC_CONN=Driver={Microsoft Access Driver (*.mdb, *.accdb)};DBQ=D:\data\你的库.mdb;
pgdb-cli.exe D:\data\你的库.mdb tree
```

## 5. 运行测试（可选）

```bat
cargo test
cargo test

:: 基准库验收测试：以 tests\fixtures\test.mdb 为基准（ArcGIS 10.1 / GDB_Items 模型，
:: 含中文要素数据集 BDC不动产、中文要素类 界址点/其他、中文表 附加，共 9 个用例）
cargo test --test test_mdb -- --ignored --test-threads=1

:: 换用其它基准文件
set PGDB_TEST_MDB=D:\data\你的库.mdb
cargo test --test test_mdb -- --ignored --test-threads=1

:: 上一轮 Jackcess 样本库的集成测试
set PGDB_TEST_MDB=D:\tmp\sample_legacy.mdb
set PGDB_TEST_MDB_ITEMS=D:\tmp\sample_items.mdb
cargo test --test odbc_real -- --ignored --test-threads=1
```

在 Windows + ACE 驱动下，`test_mdb.rs` 中的**写回往返用例会真正执行**
（属性写入中文列、几何写入 + 坐标校验、空间索引同步）；
在 Linux 只读驱动下该用例会打印提示并跳过。

## 6. 作为库引用

`Cargo.toml`：

```toml
[dependencies]
pgdb = { path = "path/to/pgdb-rs", features = ["odbc"] }
```

入口 API 与 ArcObjects 对应：`AccessWorkspaceFactory::open_odbc`（`IWorkspaceFactory::Open`）
→ `ws.datasets()`（`IEnumDataset`）→ `ws.open_feature_class`（`IFeatureWorkspace::OpenFeatureClass`），
遍历与更新示例见 `README.md` 与 `examples/`。

## 7. 关于中文对象名与中文乱码

本项目的 `odbc-api` 依赖以 **`narrow` 特性**编译：

```toml
odbc-api = { version = "8", optional = true, default-features = false,
             features = ["narrow", "odbc_version_3_80"] }
```

原因：Linux 的 mdbtools 驱动只导出窄字符 API（无 `*W` 版本），走宽字符路径会被
unixODBC 的宽→窄桥接破坏中文名。Windows + ACE 下同样使用 `narrow`——但
**ACE/Jet 会把窄字节按本地 ANSI 代码页（简中 = CP936/GBK）解读**，
直接用 UTF-8 字节调用窄字符 API 会导致表名/字段值乱码
（`界址点` → `鐣屽潃鐐`，末字节被改写）。

本库的 `datastore::odbc` 模块内置了**代码页桥**处理该差异：

| 方向 | 行为 |
|------|------|
| 出站（SQL / 连接串） | Windows 上把 UTF-8 文本按 ANSI 代码页编码后再交给驱动 |
| 入站（读取结果） | 先判断是否为合法 UTF-8：是则无损直通（Linux 与部分 ACE 版本），否则按 ANSI 代码页解码还原 |

因此**正常情况下你不需要做任何额外配置**：中文表名、中文列名、中文值、
含中文的 `DBQ` 路径都能直接工作。

### 7.1 日志/控制台乱码

`pgdb-cli.exe` 启动时会自动把控制台切到 UTF-8（`SetConsoleOutputCP(65001)` +
`SetConsoleCP(65001)`），因此 **不需要手工 `chcp 65001`**。

日志若仍有乱码，按此排查：

```bat
:: 1. 确认不是重定向或管道（此时字节本就是 UTF-8，用支持 UTF-8 的编辑器查看）
pgdb-cli.exe D:\data\test.mdb tree > out.txt
type out.txt

:: 2. 老版本 conhost 字体不含中文字形时会显示方块（不是编码问题）
::    换 Windows Terminal / 把字体换成 "NSimSun" 或 "Consolas + 中文回退"
```

若要把输出对接只吃 GBK 的旧脚本，可关闭自动切换：

```bat
set PGDB_NO_UTF8_CONSOLE=1
pgdb-cli.exe D:\data\test.mdb tree
```

### 7.2 非默认代码页环境

若系统 ANSI 代码页不是 936（如日文 932、繁中 950），本库会自动读取 `GetACP()`
并按其转换，通常无需干预。**仅当**驱动装了非默认 ISAM 时才需手工覆盖：

```bat
set PGDB_ANSI_CP=936
pgdb-cli.exe D:\data\你的库.mdb tree
```

### 7.3 连接串中的中文路径

`DBQ=D:\数据\你的库.mdb` 这类含中文的路径同样走出站编码，可直接使用。
若显式指定连接串，注意**不要**加多余引号：

```bat
set PGDB_ODBC_CONN=Driver={Microsoft Access Driver (*.mdb, *.accdb)};DBQ=D:\数据\test.mdb;
pgdb-cli.exe D:\数据\test.mdb tree
```

## 常见问题

| 现象 | 原因与处理 |
|------|-----------|
| `数据源名称过长 / 找不到驱动 IM002` | 未装 ACE 或位数不匹配；用 `drivers` 子命令核对 |
| `IM001 Driver does not support this function` | 用了错误驱动（如文本驱动）；确认连接串 Driver 名 |
| 属性改了但 ArcMap 里选不中要素 | 空间索引未同步；执行 `rebuild-index <要素类>`（正常写入路径会自动维护） |
| 中文乱码 | 本库已自动切 UTF-8 控制台并桥接代码页；若仍异常见 §7.1/§7.2 |
| 中文显示为方块 `□□□` | 字体缺字形（非编码问题）；换 Windows Terminal 或 `NSimSun` 字体 |
| 提示缺少 ODBC 驱动 | 安装 Microsoft Access Database Engine；仅做查询可改用 `--access readonly`（纯 Rust，无需驱动） |
| 中文表名报 `找不到表 / Couldn't parse SQL` | 需用 `[]` 或 `""` 引用中文标识符（mdbtools 不接受反引号）；本库内部已自动处理 |

### 关于 `42000 / 22003` 这两个报错

这两个 SQLSTATE 曾在本库内部触发（已修复），若你看到它们，可对照下表理解含义：

| 报错 | 含义 | 本库的处理 |
|------|------|-----------|
| `42000 native -3500` 无效的 SQL 语句；在此 'DELETE'、'INSERT'… | 向 ACE/Jet 发了它不认识的语句（原先本库误发 mdbtools 专有的 `DESCRIBE TABLE`） | 已按驱动名门控，**只对 mdbtools 发** `DESCRIBE TABLE`，ACE/Jet 走标准 `SQLColumns` |
| `22003 native 34` 数值超出范围 在行号 17 (MHigh) 中 | 该行某个 `0x...` 十六进制字面量超过了 Jet 的 SQL 语句长度上限（约 64,000 字符） | 超过 24 KiB 的二进制值改走**参数绑定**，不再受 SQL 文本长度限制 |
| `01004` 字符串数据，右截断 | 读取时列缓冲小于实际值（数据本身未丢失） | 仅告警；本库按列类型给出足够缓冲 |

> `MHigh`（Memo-High）是 Jet 内部的超长 token 记号。看到它基本可断定
> 「某个字段的值太大了」，而非 SQL 语法写错。

诊断消息的中文现在也能正常显示（原先会被 `from_utf8_lossy` 解成
`��ֵ������Χ`）；若你仍看到这种 `�` 连串，说明买到的是旧版 exe，
请用最新 `dist/` 里的可执行文件。
