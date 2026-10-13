//! 审计日志的数据形状。
//!
//! 与 [`crate::Store::kv_put`] 那类「状态型」写入的分工：审计记的是**发生过什么**，
//! 一次一行、永不合并 —— 所以它既不进 `kv`（那里一个键只有一份现值），也不进
//! `recent`（那里同一个 id 只留最后一次）。
//!
//! 三个英文列（`kind` / `action` / `outcome`）是**稳定英文键**，不是给人看的文案：
//! 界面按当次语言另译（`tr`），筛选与日志按这些键走。这样换一次语言不会让历史记录
//! 里的分类对不上号，导出给别人也不会因为语言不同而分成两堆。
//!
//! 本模块只定义形状与查询条件，不碰 SQL —— 写走 [`crate::writer`] 的去抖线程，
//! 读走 [`crate::Store`] 的读连接（见 [`crate::Store::audit_query`]）。

/// 审计日志的**出厂**保留条数（可在「设置 → 审计」里改）。
///
/// 2000 条是「够回溯一整天的操作」与「库不会无限长」之间的折中：一行不到 200 字节，
/// 整表几百 KB，比日志文件还小。需要更长的留证窗口就把上限调大（界面有档位），
/// 这张表的定位是**最近的流水**，不是归档。
pub const DEFAULT_KEEP: usize = 2000;

/// 一条待写入的审计记录。
///
/// 没有 `at`：时间由**写线程**在落盘那一刻统一取（[`crate::now_seconds`]），
/// 免得调用方各自算一遍、还算出不一致的「现在」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditEntry {
    /// 类别：`connection` / `query` / `write` / `transport` / `hook` / `plugin` / `mcp`…
    pub kind: String,
    /// 动作：`connect` / `execute` / `commit` / `open_tunnel` / `invoke`…
    pub action: String,
    /// 结果：`ok` / `failed` / `blocked` / `cancelled`。
    pub outcome: String,
    /// 作用对象（连接 id / 工具 id / 空串）。用于「只看这条连接的记录」。
    pub scope: String,
    /// 一行摘要。**调用方负责脱敏**：口令、密文 key 一律不许出现在这里。
    pub detail: String,
    /// 语句指纹（归一化后哈希；非语句类事件留空）。
    pub fingerprint: String,
    /// 错误关联 id（只有失败事件才有；成功事件留空）。
    pub trace_id: String,
    /// 耗时（毫秒；没有意义的事件留 `None`，不要填 0）。
    pub duration_ms: Option<i64>,
}

impl AuditEntry {
    /// 造一条记录。`outcome` 用 [`AuditOutcome`] 的稳定键。
    #[must_use]
    pub fn new(
        kind: impl Into<String>,
        action: impl Into<String>,
        outcome: AuditOutcome,
        scope: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            action: action.into(),
            outcome: outcome.key().to_owned(),
            scope: scope.into(),
            detail: detail.into(),
            fingerprint: String::new(),
            trace_id: String::new(),
            duration_ms: None,
        }
    }

    /// 带上语句指纹（流式构造，便于一行写下来）。
    #[must_use]
    pub fn with_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.fingerprint = fingerprint.into();
        self
    }

    /// 带上错误关联 id。
    #[must_use]
    pub fn with_trace(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = trace_id.into();
        self
    }

    /// 带上耗时（毫秒）。
    #[must_use]
    pub fn with_duration(mut self, duration_ms: i64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }
}

/// 结果三档 + 取消。`ok` 以外都算「值得看一眼」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuditOutcome {
    /// 成功。
    Ok,
    /// 失败（有错误，含关联 id）。
    Failed,
    /// 被拦下（只读档、危险 SQL 闸门、钩子阻断）——**不是错误**，是策略生效。
    Blocked,
    /// 用户主动取消 / 超时取消。
    Cancelled,
}

impl AuditOutcome {
    /// 稳定键（落库与筛选都用它）。
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }

    /// 从稳定键还原；认不出来（旧版本写的、手工改的）返回 `None`。
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "ok" => Some(Self::Ok),
            "failed" => Some(Self::Failed),
            "blocked" => Some(Self::Blocked),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// 一条读出来的审计记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    /// 自增主键（只增不复用，天然是「发生顺序」）。
    pub id: i64,
    /// Unix 秒。
    pub at: i64,
    /// 类别（稳定键）。
    pub kind: String,
    /// 动作（稳定键）。
    pub action: String,
    /// 结果（稳定键；认不出来时原样带出）。
    pub outcome: String,
    /// 作用对象。
    pub scope: String,
    /// 一行摘要。
    pub detail: String,
    /// 语句指纹。
    pub fingerprint: String,
    /// 错误关联 id。
    pub trace_id: String,
    /// 耗时（毫秒）。
    pub duration_ms: Option<i64>,
}

impl AuditRow {
    /// 是不是一次「值得看一眼」的记录（失败 / 被拦 / 被取消）。
    #[must_use]
    pub fn is_problem(&self) -> bool {
        !matches!(
            AuditOutcome::from_key(&self.outcome),
            Some(AuditOutcome::Ok)
        )
    }
}

/// 审计查询条件。
///
/// 全部是「缩小范围」，不是「取交集之外」——空条件 = 最近 `limit` 条。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// 只看这一类（`None` = 全部）。
    pub kind: Option<String>,
    /// 只看失败 / 被拦 / 被取消（成功的不看）。
    pub problems_only: bool,
    /// 只看这一次错误关联（点状态栏徽标进来时用）。
    pub trace_id: Option<String>,
    /// 只看这条连接 / 工具。
    pub scope: Option<String>,
    /// 最多取多少条（新 → 旧）。
    pub limit: usize,
}

impl AuditFilter {
    /// 缺省条数：一屏多一点，够看出「刚刚发生了什么」。
    pub const DEFAULT_LIMIT: usize = 200;

    /// 全部条件为空（可用来判断「当前有没有在筛选」）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kind.is_none()
            && !self.problems_only
            && self.trace_id.is_none()
            && self.scope.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::{AuditEntry, AuditFilter, AuditOutcome, AuditRow};

    /// 结果键往返，且四档的键两两不同（落库与筛选都靠它）。
    #[test]
    fn outcome_keys_round_trip_and_are_distinct() {
        let keys: std::collections::BTreeSet<&str> = [
            AuditOutcome::Ok,
            AuditOutcome::Failed,
            AuditOutcome::Blocked,
            AuditOutcome::Cancelled,
        ]
        .iter()
        .map(|outcome| outcome.key())
        .collect();
        assert_eq!(keys.len(), 4);
        for outcome in [
            AuditOutcome::Ok,
            AuditOutcome::Failed,
            AuditOutcome::Blocked,
            AuditOutcome::Cancelled,
        ] {
            assert_eq!(AuditOutcome::from_key(outcome.key()), Some(outcome));
        }
        assert_eq!(AuditOutcome::from_key("weird"), None);
    }

    /// 「值得看一眼」的判据：`ok` 之外都算 —— 认不出来的键**也算**（宁可多看一眼）。
    #[test]
    fn anything_but_ok_is_worth_a_look() {
        let row = |outcome: &str| AuditRow {
            id: 1,
            at: 0,
            kind: "query".into(),
            action: "execute".into(),
            outcome: outcome.into(),
            scope: String::new(),
            detail: String::new(),
            fingerprint: String::new(),
            trace_id: String::new(),
            duration_ms: None,
        };
        assert!(!row("ok").is_problem());
        assert!(row("failed").is_problem());
        assert!(row("blocked").is_problem());
        assert!(row("cancelled").is_problem());
        assert!(row("来自未来版本的键").is_problem());
    }

    /// 流式构造：指纹 / trace / 耗时各自落位，没设的保持空。
    #[test]
    fn builders_only_touch_what_they_name() {
        let entry = AuditEntry::new("query", "execute", AuditOutcome::Ok, "conn-1", "SELECT 1");
        assert_eq!(entry.outcome, "ok");
        assert_eq!(entry.scope, "conn-1");
        assert!(entry.fingerprint.is_empty());
        assert!(entry.trace_id.is_empty());
        assert_eq!(entry.duration_ms, None);

        let entry = entry
            .with_fingerprint("ab12")
            .with_trace("t-1")
            .with_duration(7);
        assert_eq!(entry.fingerprint, "ab12");
        assert_eq!(entry.trace_id, "t-1");
        assert_eq!(entry.duration_ms, Some(7));
    }

    /// 空筛选就是「最近 N 条」，且能被认出来。
    #[test]
    fn an_empty_filter_is_recognisable() {
        assert!(AuditFilter::default().is_empty());
        let filter = AuditFilter {
            problems_only: true,
            ..AuditFilter::default()
        };
        assert!(!filter.is_empty());
    }
}
