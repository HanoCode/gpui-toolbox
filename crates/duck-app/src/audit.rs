//! 审计日志的应用层收口：稳定键 → 文案、事件构造、保留上限、错误关联 id。
//!
//! 形状与读写都在 `duck-store`（[`duck_store::audit`]）；这里只放**应用**要的东西：
//!
//! - 类别与结果的枚举（键是英文常量，界面文案按当次语言 `tr`）；
//! - 「保留多少条」这个用户设置的读写（kv 单键，缺省 [`AUDIT_DEFAULT_KEEP`]）；
//! - 错误关联 id 的生成与「点徽标跳审计页」的待处理槽。
//!
//! # 错误关联 id（trace id）
//!
//! 一次失败的**提示语**与它落在审计里的那一行，靠同一个 `t-xxxxxxxxxxxx` 对上。
//! 用户在通知里看到 `连接失败（t-3f9c1a2b7d01）：…`，点状态栏徽标就只看这一次的东西 ——
//! 不必在几百行流水里按时间猜哪条是刚才那次。
//!
//! 生成不引依赖：纳秒时间戳 + 进程内计数器过一遍 FNV-1a。**不追求密码学强度**，
//! 它要的只是「同一台机器上不重号、且在列表里一眼可认」。

use std::sync::atomic::{AtomicU64, Ordering};

use duck_store::{AUDIT_DEFAULT_KEEP, AuditFilter, AuditRow};
pub use duck_store::{AuditEntry, AuditOutcome};
use duck_theme::tr;
use gpui::{App, BorrowAppContext as _, Global};

/// 审计页在设置里的分类 key（规格表与渲染分支同源）。
pub const AUDIT_PAGE: &str = "audit";

/// kv 单键：审计日志保留条数。
const KEY_AUDIT_KEEP: &str = "audit_keep";

/// 保留条数的可设范围。
///
/// 下限 100：少于这个数，「回溯一下刚才发生了什么」根本不够看。
/// 上限 10 万：一行不到 200 字节，10 万行约 20 MB —— 再往上就不该靠这张表去留证了
/// （⚠️ 现在**还没有**审计导出：上限是唯一能延长留证窗口的手段，界面上那句话也是这么说的）。
pub const AUDIT_KEEP_RANGE: std::ops::RangeInclusive<usize> = 100..=100_000;

/// 界面上可选保留档位。
///
/// 给的是**档位**而不是自由输入：这个值决定「库能长多大」，用户很难对「5000 条」
/// 有个体感，档位的跨度（200 → 10 万，500 倍）本身就是有用的信息；
/// 而一个数字输入框既能填 3 也能填 300 万，反而要额外解释边界。
pub const AUDIT_KEEP_CHOICES: [usize; 7] = [200, 500, 1000, 2000, 5000, 20_000, 100_000];

/// 审计事件的类别（**稳定英文键**；界面文案另译）。
///
/// 只分这几类是有意的：类别太多会让人先花时间想「这条算哪一类」，
/// 而审计的实际用法是「最近出什么事了」与「这条连接做过什么」（后者靠 `scope`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AuditKind {
    /// 连接 / 断开 / 探活。
    Connection,
    /// 读路径：SQL 执行、表浏览、EXPLAIN。
    Query,
    /// 写路径：DML / DDL / 行内编辑 / 批量修改。
    Write,
    /// 传输通道：隧道 / 代理 / SSM 的建立与失败。
    Transport,
    /// 连接钩子。
    Hook,
    /// 插件宿主调用（网络、文件、剪贴板）。
    Plugin,
    /// MCP 服务端会话（#26 落地后接入）。
    Mcp,
}

impl AuditKind {
    /// 全部类别（筛选项按这个顺序渲染）。
    pub const ALL: [Self; 7] = [
        Self::Connection,
        Self::Query,
        Self::Write,
        Self::Transport,
        Self::Hook,
        Self::Plugin,
        Self::Mcp,
    ];

    /// 稳定键（落库与筛选都用它）。
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Query => "query",
            Self::Write => "write",
            Self::Transport => "transport",
            Self::Hook => "hook",
            Self::Plugin => "plugin",
            Self::Mcp => "mcp",
        }
    }

    /// 从稳定键还原；认不出来返回 `None`（旧版本写的记录不该让列表整页崩掉）。
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// 界面文案。
    #[must_use]
    pub fn label(self, cx: &App) -> &'static str {
        match self {
            Self::Connection => tr(cx, "连接"),
            Self::Query => tr(cx, "查询"),
            Self::Write => tr(cx, "写入"),
            Self::Transport => tr(cx, "传输通道"),
            Self::Hook => tr(cx, "钩子"),
            Self::Plugin => tr(cx, "插件"),
            Self::Mcp => tr(cx, "MCP"),
        }
    }
}

/// 结果键的界面文案。认不出来的键**原样带出**——宁可让用户看见一个生词，
/// 也不要把它悄悄显示成「未知」而丢掉唯一的线索。
#[must_use]
pub fn outcome_label(outcome: &str, cx: &App) -> String {
    match AuditOutcome::from_key(outcome) {
        Some(AuditOutcome::Ok) => tr(cx, "成功").to_owned(),
        Some(AuditOutcome::Failed) => tr(cx, "失败").to_owned(),
        Some(AuditOutcome::Blocked) => tr(cx, "已拦下").to_owned(),
        Some(AuditOutcome::Cancelled) => tr(cx, "已取消").to_owned(),
        None => outcome.to_owned(),
    }
}

/// 审计动作的稳定键（调用点一律用这些常量，别写裸串）。
///
/// 与 [`AuditKind`] 的区别：类别是**封闭**的（就那七类，多一类要动枚举），
/// 动作是**开放**的 —— 以后加一个功能就多一两个动作，不该为了一个动作去改枚举。
/// 所以动作只在这里登记当前这批，落库的是键本身。
pub mod action {
    /// 建立连接（含传输通道已就绪）。
    pub const CONNECT: &str = "connect";
    /// 断开连接。
    pub const DISCONNECT: &str = "disconnect";
    /// 探活 / 测试连接。
    pub const PROBE: &str = "probe";
    /// 建立传输通道（SSH 隧道 / 代理 / SSM）。
    pub const OPEN_TRANSPORT: &str = "open_transport";
    /// 执行 SQL。
    pub const EXECUTE: &str = "execute";
    /// EXPLAIN。
    pub const EXPLAIN: &str = "explain";
    /// 浏览表数据。
    pub const BROWSE: &str = "browse";
    /// 写路径提交（DML / DDL）。
    pub const COMMIT: &str = "commit";
    /// 行内编辑。
    pub const EDIT_ROW: &str = "edit_row";
    /// 批量删行。
    pub const DELETE_ROWS: &str = "delete_rows";
    /// 跑连接钩子。
    pub const RUN_HOOK: &str = "run_hook";
    /// 跑一个插件提供的工具（JS 沙箱）。
    pub const INVOKE: &str = "invoke";
    /// 清空审计本身。
    pub const CLEAR: &str = "clear";
}

/// 动作键 → **中文源串**。
///
/// 表而不是 `match`：源串必须是 `&'static str`（[`tr`] 的签名要它），
/// 而 `match` 的臂里写不了「从表里取」。有了这张表，守卫测试能在**没有 App**
/// 的情况下核对「每个常量都有文案」，不必为了跑一次断言去起一个 gpui 上下文。
const ACTIONS: [(&str, &str); 13] = [
    (action::CONNECT, "建立连接"),
    (action::DISCONNECT, "断开连接"),
    (action::PROBE, "测试连接"),
    (action::OPEN_TRANSPORT, "建立传输通道"),
    (action::EXECUTE, "执行语句"),
    (action::EXPLAIN, "查看执行计划"),
    (action::BROWSE, "浏览表数据"),
    (action::COMMIT, "提交写入"),
    (action::EDIT_ROW, "修改行"),
    (action::DELETE_ROWS, "删除行"),
    (action::RUN_HOOK, "运行钩子"),
    (action::INVOKE, "调用插件"),
    (action::CLEAR, "清空审计"),
];

/// 动作键的界面文案；**认不出的键原样带出**（与 [`outcome_label`] 同一条理由：
/// 一个生词好过一条被抹掉的线索）。
#[must_use]
pub fn action_label(action: &str, cx: &App) -> String {
    ACTIONS.iter().find(|(key, _)| *key == action).map_or_else(
        || action.to_owned(),
        |(_, source)| tr(cx, source).to_owned(),
    )
}

/// 当前保留条数（kv 里没有 / 读不出来时用出厂值）。
#[must_use]
pub fn keep_limit(cx: &App) -> usize {
    duck_store::kv_get(cx, KEY_AUDIT_KEEP)
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map_or(AUDIT_DEFAULT_KEEP, clamp_keep)
}

/// 写保留条数（去抖落盘）。
///
/// 超出范围的值被夹回范围内。**调小要当场生效**：这里顺带发一条「只裁剪」的请求，
/// 否则用户把 2000 改成 200 之后列表里还是那 2000 行 —— 得等下一次写入才缩，
/// 看起来就是「改了没反应」。裁剪本身也是去抖的，界面要在它之后读列表得先 `flush`。
pub fn set_keep_limit(cx: &App, keep: usize) {
    let keep = clamp_keep(keep);
    duck_store::kv_put(cx, KEY_AUDIT_KEEP, keep.to_string());
    duck_store::audit_trim(cx, keep);
}

/// 把保留条数夹进 [`AUDIT_KEEP_RANGE`]。
#[must_use]
pub fn clamp_keep(keep: usize) -> usize {
    keep.clamp(*AUDIT_KEEP_RANGE.start(), *AUDIT_KEEP_RANGE.end())
}

/// 进程内计数器：纳秒时间戳在同一纳秒内重号时靠它分开。
static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 生成一个错误关联 id（`t-` + 12 位十六进制）。
#[must_use]
pub fn new_trace_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_nanos())
        .unwrap_or_default();
    let seq = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in nanos
        .to_le_bytes()
        .into_iter()
        .chain(seq.to_le_bytes())
        .chain(std::process::id().to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    // 12 位十六进制 = 48 位：几百条记录里撞号的概率低到不必考虑，
    // 又比满 16 位短一点 —— 它要出现在通知与徽标里，短一点更好认。
    format!("t-{:012x}", hash & 0xffff_ffff_ffff)
}

/// 记一条审计（保留上限自动取当前设置）。
///
/// 调用方**必须**负责 `detail` 里没有口令 / 密文 key：审计是**明文落盘**的一张表，
/// 而且它的用途就是被人读、被人复制出去（留证）。
pub fn record(cx: &App, entry: AuditEntry) {
    duck_store::audit_push(cx, entry, keep_limit(cx));
}

/// 记一条事件；**失败 / 被拦 / 被取消时**自动补一个关联 id 并把该 id 回传。
///
/// 回传的 id 由调用方带进提示语（`trf(cx, "连接失败（{}）：{}", …)`）。成功事件没有 id
/// ——「每次成功都给一个号」只会让号变成噪音，而错误关联的用处正是「少见、可查」。
///
/// 顺带把失败挂到 [`PendingFailure`] 上（状态栏徽标），所以收 `&mut App`。
pub fn record_event(
    cx: &mut App,
    kind: AuditKind,
    action: &str,
    outcome: AuditOutcome,
    scope: &str,
    detail: impl Into<String>,
) -> Option<String> {
    record_entry(
        cx,
        AuditEntry::new(kind.key(), action, outcome, scope, detail),
    )
}

/// 记一条**已经构造好**的事件（同样是「失败自动带关联 id」那一套）。
///
/// 存在的理由是语句级事件要带指纹与耗时，而 [`AuditEntry`] 的链式构造器正好表达它们；
/// 硬塞进 [`record_event`] 的参数表会变成八个参数，而其中两个只有 SQL 路径用得上。
pub fn record_entry(cx: &mut App, entry: AuditEntry) -> Option<String> {
    let is_problem = !matches!(
        AuditOutcome::from_key(&entry.outcome),
        Some(AuditOutcome::Ok)
    );
    let trace = is_problem.then(new_trace_id);
    let mut entry = entry;
    if let Some(trace) = trace.as_ref() {
        entry = entry.with_trace(trace.clone());
    }
    let summary = entry.detail.clone();
    record(cx, entry);
    if is_problem {
        note_failure(cx, summary, trace.clone());
    }
    trace
}

/// 主窗状态栏错误徽标的状态：最近一次失败 + 自上次查看审计页起累计的条数。
///
/// 为什么不是一个「最近失败」的单值：徽标要说的是「有 N 条你还没看」，
/// 而单值只能表达「最后一次是什么」—— 用户看完一条之后剩下的那几条就此消失，
/// 正是最该被提醒的那种漏。
///
/// 它**只活在内存里**：这是「还没看过」的会话内状态，不是审计事实。
/// 重启后审计仍在（那里有全部记录），徽标归零是应该的。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingFailure {
    summary: String,
    trace: Option<String>,
    count: usize,
}

impl PendingFailure {
    /// 最近一次失败的一句话摘要（徽标的提示语）。
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// 最近一次失败的关联 id（点徽标就只看它）。
    #[must_use]
    pub fn trace(&self) -> Option<&str> {
        self.trace.as_deref()
    }

    /// 还没查看的失败条数。
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }
}

impl Global for PendingFailure {}

/// 记一次失败（覆盖摘要、累加条数）。
///
/// ⚠️ 用 `update_default_global` 而不是 `update_global`：后者在全局**还没被创建**时
/// 会 panic（`lease_global` 假定它存在），而「还没失败过」正是最常见的初始状态。
fn note_failure(cx: &mut App, summary: String, trace: Option<String>) {
    cx.update_default_global::<PendingFailure, _>(|pending, _| {
        pending.summary = summary;
        pending.trace = trace;
        pending.count += 1;
    });
}

/// 当前待查看的失败；没有过失败、或已经被查看过时返回 `None`。
#[must_use]
pub fn pending_failure(cx: &App) -> Option<PendingFailure> {
    cx.try_global::<PendingFailure>()
        .filter(|pending| pending.count > 0)
        .cloned()
}

/// 清掉徽标（用户看了审计页）。
pub fn clear_failure(cx: &mut App) {
    *cx.default_global::<PendingFailure>() = PendingFailure::default();
}

/// 读审计记录。⚠️ 写是去抖的，**要「刚记的立刻读得到」先 [`duck_store::flush`]**。
#[must_use]
pub fn query(cx: &App, filter: &AuditFilter) -> Vec<AuditRow> {
    duck_store::audit_query(cx, filter)
}

/// 「打开审计页时只看这一次错误」的待处理槽。
///
/// 与设置的跳转槽同一条路子（`Desktop::set_settings_pending`）：状态栏徽标在**主窗**里，
/// 审计页在**设置窗**里，两者拿不到对方的视图句柄，只能把意图投递过去、由对方取走。
#[derive(Default)]
pub struct PendingAuditTrace(pub Option<String>);

impl Global for PendingAuditTrace {}

/// 投递一个「只看这一次」的意图。
pub fn set_pending_trace(cx: &mut App, trace: Option<String>) {
    cx.set_global(PendingAuditTrace(trace));
}

/// 取走待处理意图（审计页渲染时调）。
///
/// ⚠️ [`Default`] 不是可有可无的：审计页**每次**渲染都会问一次，而「从来没有人投递过」
/// 是最常见的状态；用 `update_global` 去动一个还没创建的全局会 panic（`lease_global`
/// 假定它存在）。`update_default_global` 兼顾「没建就先建一个空的」。
#[must_use]
pub fn take_pending_trace(cx: &mut App) -> Option<String> {
    cx.update_default_global::<PendingAuditTrace, _>(|pending, _| pending.0.take())
}

/// 打开审计页；`trace` 非空时只看那一次错误。
pub fn open_audit(cx: &mut App, trace: Option<String>) {
    set_pending_trace(cx, trace);
    crate::pages::open_settings_at(AUDIT_PAGE, None, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 类别键两两不同、可往返，且「认不出来的键」得到 `None` 而不是随便挑一个。
    #[test]
    fn kind_keys_round_trip_and_are_distinct() {
        let keys: std::collections::BTreeSet<&str> =
            AuditKind::ALL.iter().map(|kind| kind.key()).collect();
        assert_eq!(keys.len(), AuditKind::ALL.len());
        for kind in AuditKind::ALL {
            assert_eq!(AuditKind::from_key(kind.key()), Some(kind));
        }
        assert_eq!(AuditKind::from_key("来自未来的类别"), None);
    }

    /// 保留条数的夹取：低于下限 / 高于上限都被拉回范围内。
    #[test]
    fn keep_limit_is_clamped() {
        assert_eq!(clamp_keep(0), *AUDIT_KEEP_RANGE.start());
        assert_eq!(clamp_keep(1), *AUDIT_KEEP_RANGE.start());
        assert_eq!(clamp_keep(5000), 5000);
        assert_eq!(clamp_keep(usize::MAX), *AUDIT_KEEP_RANGE.end());
        // 档位必须落在可设范围内，否则选了就被夹走，界面显示的和存下的不是一回事。
        for choice in AUDIT_KEEP_CHOICES {
            assert_eq!(clamp_keep(choice), choice, "档位 {choice} 越界");
        }
    }

    /// 动作常量与文案表**一一对应**：少一条 ⇒ 那个动作在界面上永远显示英文键。
    #[test]
    fn every_action_constant_has_a_label() {
        let declared = [
            action::CONNECT,
            action::DISCONNECT,
            action::PROBE,
            action::OPEN_TRANSPORT,
            action::EXECUTE,
            action::EXPLAIN,
            action::BROWSE,
            action::COMMIT,
            action::EDIT_ROW,
            action::DELETE_ROWS,
            action::RUN_HOOK,
            action::INVOKE,
            action::CLEAR,
        ];
        assert_eq!(
            declared.len(),
            ACTIONS.len(),
            "常量数（{}）与文案表条目数（{}）不一致",
            declared.len(),
            ACTIONS.len()
        );
        for key in declared {
            let (_, source) = ACTIONS
                .iter()
                .find(|(candidate, _)| *candidate == key)
                .unwrap_or_else(|| panic!("动作 {key} 没有文案"));
            // 文案必须是**中文源串**：写成一个英文键就说明忘了译，而那种漏法在
            // 中文界面上看不出来（键本身就是英文，看起来「像是有译」）。
            assert!(
                source
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "动作 {key} 的文案 {source:?} 里没有汉字"
            );
        }
        // 键两两不同（撞键会让两个动作共用一条文案）。
        let keys: std::collections::BTreeSet<&str> = ACTIONS.iter().map(|(key, _)| *key).collect();
        assert_eq!(keys.len(), ACTIONS.len());
    }

    /// 关联 id：带 `t-` 前缀、12 位十六进制、连续取不重号。
    #[test]
    fn trace_ids_are_short_unique_and_prefixed() {
        let first = new_trace_id();
        let second = new_trace_id();
        assert_ne!(first, second, "同一纳秒内也必须分开");
        for trace in [&first, &second] {
            assert!(trace.starts_with("t-"), "{trace} 少了前缀");
            let body = &trace[2..];
            assert_eq!(body.len(), 12, "{trace} 的主体应当 12 位");
            assert!(
                body.chars().all(|c| c.is_ascii_hexdigit()),
                "{trace} 不是十六进制"
            );
        }
        // 取一批也不该撞（进程内计数器保证）。
        let batch: std::collections::BTreeSet<String> = (0..64).map(|_| new_trace_id()).collect();
        assert_eq!(batch.len(), 64);
    }
}
