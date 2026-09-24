//! 编辑操作的结果与防护选项。
//!
//! 对应 ArcObjects 中「编辑工作空间」的防护语义（`IWorkspaceEdit`）与
//! `ITable::DeleteSearchedRows` / `ITable::UpdateSearchedRows` 的返回约定。
//!
//! 设计原则：
//! - 批量更新/删除是危险操作，**返回值必须能区分**「确实没有匹配行」与「条件写错了」；
//! - **全表操作必须显式确认**（[`EditOptions::allow_whole_table`]），防止 `QueryFilter::new()`
//!   笔误导致清空整张表；
//! - 不提供 `*_checked` 双份 API：一个 `Default` 的 [`EditOptions`] 参数即可覆盖两类风险，
//!   方法命名保持与 ArcObjects 一致。

use std::fmt;

/// 编辑操作的作用范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditScope {
    /// 按 [`crate::gdb::QueryFilter`] 过滤后的子集
    Filtered,
    /// 整张表（filter 未给出任何条件）
    WholeTable,
}

impl EditScope {
    /// 中文标签
    pub fn label(self) -> &'static str {
        match self {
            EditScope::Filtered => "按条件过滤",
            EditScope::WholeTable => "全表",
        }
    }

    /// 是否为全表操作
    pub fn is_whole_table(self) -> bool {
        matches!(self, EditScope::WholeTable)
    }
}

impl fmt::Display for EditScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// 一次批量编辑（更新/删除）的结果。
///
/// - `matched`：过滤条件**命中**的行数（写前统计）；
/// - `affected`：后端报告的**受影响**行数。ODBC/Jet 没有可靠的 `@@ROWCOUNT`，
///   实现为「写前 COUNT + 执行」时两者相等，故 `affected` 是近似值
///   （例如把值更新为相同值时，数据库可能实际未写，但仍计入）；
/// - `scope`：本次操作是子集还是全表。
///
/// `matched == 0` 通常意味着条件写错（例如 OID 不存在、WHERE 拼写错误），
/// 调用方应检查或改用 [`EditOptions::require_hit`] 让库直接报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditResult {
    /// 条件命中的行数（写前统计）
    pub matched: u64,
    /// 受影响的行数（见类型说明的近似语义）
    pub affected: u64,
    /// 作用范围（子集 / 全表）
    pub scope: EditScope,
}

impl EditResult {
    /// 构造
    pub fn new(matched: u64, affected: u64, scope: EditScope) -> Self {
        Self {
            matched,
            affected,
            scope,
        }
    }

    /// 是否零命中
    pub fn is_no_hit(&self) -> bool {
        self.matched == 0
    }
}

impl fmt::Display for EditResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "命中 {} 行，影响 {} 行（{}）",
            self.matched, self.affected, self.scope
        )
    }
}

/// 批量编辑的防护选项（对应 `IWorkspaceEdit` 的确认语义）。
///
/// 默认值最保守：
/// - 拒绝全表操作（需要 [`EditOptions::allow_whole_table`] 显式放行）；
/// - 允许零命中（返回 [`EditResult::is_no_hit`] 交由调用方判断；
///   「必须命中」的场景用 [`EditOptions::require_hit`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditOptions {
    /// 是否允许全表操作（filter 未给任何条件时不拒绝）
    pub allow_whole_table: bool,
    /// 命中 0 行时视为错误（`PgdbError::NotFound`）
    pub require_hit: bool,
}

impl EditOptions {
    /// 默认防护（拒绝全表、允许零命中）
    pub fn new() -> Self {
        Self::default()
    }

    /// 允许全表操作（显式确认后放行）
    pub fn whole_table(mut self) -> Self {
        self.allow_whole_table = true;
        self
    }

    /// 要求至少命中一行，否则报 `NotFound`
    pub fn require_hit(mut self) -> Self {
        self.require_hit = true;
        self
    }
}

/// 编辑前体检报告（[`crate::gdb::Table::preflight_edit`] 的返回值）。
///
/// 只读探测，不写库。CLI 的 `--dry-run` / `preflight` 子命令基于它输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preflight {
    /// 条件命中的行数
    pub matched: u64,
    /// 作用范围（子集 / 全表）
    pub scope: EditScope,
    /// 数据源是否可写
    pub writable: bool,
}

impl Preflight {
    /// 是否为全表操作
    pub fn is_whole_table(&self) -> bool {
        self.scope.is_whole_table()
    }

    /// 是否零命中
    pub fn is_no_hit(&self) -> bool {
        self.matched == 0
    }
}

impl fmt::Display for Preflight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "预检：命中 {} 行（{}），数据源{}",
            self.matched,
            self.scope,
            if self.writable { "可写" } else { "只读" }
        )
    }
}
