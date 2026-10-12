//! 工具协议、注册表与输出截断。
//!
//! 设计来源：`temp/zed` 的 `agent/src/tool.rs`（`AgentTool` 的
//! `spec + run` 两分法、以及「错误也走正常返回值」的纪律）。做了两处裁剪：
//!
//! 1. **去掉 GPUI 与异步**。这里的 `run` 是同步阻塞的，取消靠
//!    [`std::sync::atomic::AtomicBool`]——与 [`crate::provider::stream_chat`] 同一口径。
//! 2. **不把工具实现放进内核**。`duck-ai` 只提供 [`ToolRegistry`]，
//!    真正的 `read_file` / `search_kb` 等由上层（`duck-app`）注册，
//!    因此本 crate 无需依赖任何引擎 crate。
//!
//! ## 为什么「错误也返回 Ok」
//!
//! zed 的结论值得照抄：模型看不到 `Err`，它只能看到**回灌进对话的那段文本**。
//! 把「文件不存在」写成 `Err` 等于让模型收到一个空结果，它只会换个参数再试一次，
//! 而写成 `ToolOutcome { ok: false, content: "file not found: …" }` 它就能立刻改口径。
//! 所以本模块**没有** `Result` 返回值的工具——`ok` 只是给界面着色用的。
//!
//! ## 不带中文
//!
//! 本模块产出的文案（截断标记、未知工具提示）会**进模型上下文**，
//! 因此一律英文：一是模型对英文元指令更稳，二是内核吐中文会触发本仓
//! `every_chinese_literal_is_translated` 守卫（界面文案由上层 `tr` 组织）。

use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::wire::ToolCall;

/// 工具的风险分级。审批闸门的**唯一依据**——不看工具名、不看提示词。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    /// 只读、可逆、不改动本机与远端状态：直接执行。
    ReadOnly,
    /// 需要人工点头（写盘、改远端状态、执行用户给的语句）。
    NeedsApproval,
}

impl ToolRisk {
    /// 是否需要弹确认。写成一个方法而不是在各处比枚举：
    /// 将来若加第三档（如「只在沙箱内自动」），改这里一处即可。
    #[must_use]
    pub const fn needs_approval(self) -> bool {
        matches!(self, Self::NeedsApproval)
    }

    /// 落库 / 日志用的 wire 名。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::NeedsApproval => "needs_approval",
        }
    }
}

/// 工具的自述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// 模型看到的工具名（`snake_case`，全局唯一）。
    pub name: String,
    /// 给模型看的说明。**写给模型看**，不是写给用户看的，无需 i18n。
    pub description: String,
    /// JSON Schema（`{"type":"object","properties":…}`）。
    pub schema: serde_json::Value,
    /// 默认风险档。输入相关的风险用 [`Tool::assess`] 覆盖。
    pub risk: ToolRisk,
}

impl ToolSpec {
    /// 组装 OpenAI `tools` 数组里的一项。
    #[must_use]
    pub fn declaration(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.schema,
            }
        })
    }
}

/// 一个可被模型调用的工具。
///
/// `Send + Sync`：注册表随页面任务跨线程（执行发生在专用 `std::thread` 上）。
pub trait Tool: Send + Sync {
    /// 自述（名字 / 说明 / 参数 schema）。
    fn spec(&self) -> ToolSpec;

    /// 按本次**实际入参**判定风险，缺省沿用 [`ToolSpec::risk`]。
    ///
    /// 典型用途：[`ToolRisk::ReadOnly`] 的工具在拿到一条写语句时应当升为
    /// [`ToolRisk::NeedsApproval`]，而不是先执行再解释。
    fn assess(&self, _input: &serde_json::Value) -> ToolRisk {
        self.spec().risk
    }

    /// 同步执行。**错误不是 `Err`**，见模块头。
    fn run(&self, input: &serde_json::Value, cancel: &AtomicBool) -> ToolOutcome;
}

/// 工具执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// 成功与否。**只影响界面着色与回灌时的措辞**，不影响「是否返回」。
    pub ok: bool,
    /// 回给模型的正文（已按 [`TruncationTier::Context`] 截断）。
    pub content: String,
    /// 给界面展示的产物。**不进对话上下文**——它是同一份内容的另一种投影，
    /// 回灌会让 token 数翻倍。
    pub detail: Option<ToolDetail>,
}

impl ToolOutcome {
    /// 成功，只有文本。
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            ok: true,
            content: truncate_tiered(&content.into(), TruncationTier::Context),
            detail: None,
        }
    }

    /// 失败。`content` 是**给模型读的失败原因**，不是给用户看的错误码。
    #[must_use]
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            ok: false,
            content: truncate_tiered(&content.into(), TruncationTier::Context),
            detail: None,
        }
    }

    /// 挂上界面产物。
    #[must_use]
    pub fn with_detail(mut self, detail: ToolDetail) -> Self {
        self.detail = Some(detail);
        self
    }
}

/// 给界面看的结构化产物。
///
/// 刻意做成「两三种通用形状」而不是「每个工具一种枚举」：内核不认识
/// `read_file` 也不认识 `search_kb`，加一种新工具不该改本文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDetail {
    /// 一张表（SQL 结果、文件清单、知识库命中）。
    Table {
        /// 表头。
        columns: Vec<String>,
        /// 数据行。每行长度与 `columns` 对齐由**产出方**保证。
        rows: Vec<Vec<String>>,
    },
    /// 一段原样转交的文本（未通过只读校验的语句等），界面给「复制」。
    Handoff { label: String, body: String },
}

/// 分级截断档位。
///
/// 三档来自 `temp/oxideterm-ai/tool_result_protocol.rs` 的做法：
/// **不同消费方给不同上限**，而不是一个全局常数。一个 200 KB 的日志文件
/// 进上下文会把整轮对话挤爆，但同一份内容放在卡片里展开给用户看毫无问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationTier {
    /// 回灌给模型的正文：12000 字符。
    Context,
    /// 界面卡片正文：2000 字符。
    Preview,
    /// 单行摘要（列表里的副标题）：1000 字符。
    Headline,
}

impl TruncationTier {
    /// 本档的字符上限（按 `char` 计数，中文按字算才符合直觉）。
    #[must_use]
    pub const fn limit(self) -> usize {
        match self {
            Self::Context => 12_000,
            Self::Preview => 2_000,
            Self::Headline => 1_000,
        }
    }
}

/// 按档位截断，**并在截断处显式写明**。
///
/// 静默截断是本仓明令禁止的失败模式：模型会以为「看全了」，
/// 于是基于残缺内容给出自信的错误结论。宁可多花几十个 token 写清楚。
#[must_use]
pub fn truncate_tiered(text: &str, tier: TruncationTier) -> String {
    let limit = tier.limit();
    let total = text.chars().count();
    if total <= limit {
        return text.to_owned();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}\n\n[truncated: showing first {limit} of {total} characters]")
}

/// 已注册工具的集合。
///
/// 顺序即模型看到的顺序；重名以**先注册者**为准（后注册的被忽略并计一次重复），
/// 这样上层「先注册基础工具、后叠加用户自定义」时行为可预测。
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// 空注册表。空表意味着**不给模型声明任何工具**（不是「声明了但都失败」）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个工具。重名返回 `false`（调用方可据此报警）。
    pub fn register(&mut self, tool: impl Tool + 'static) -> bool {
        let name = tool.spec().name;
        if self.tools.iter().any(|item| item.spec().name == name) {
            return false;
        }
        self.tools.push(Box::new(tool));
        true
    }

    /// 已注册数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否一个工具都没有。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 全部自述。
    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    /// 全部工具名（右栏「工具能力」列表用）。
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.spec().name).collect()
    }

    /// OpenAI `tools` 数组。空表时返回空 `Vec`——调用方**不要**下发空数组，
    /// 部分服务端见到 `"tools": []` 会直接报参数错误。
    #[must_use]
    pub fn declarations(&self) -> Vec<serde_json::Value> {
        self.tools
            .iter()
            .map(|tool| tool.spec().declaration())
            .collect()
    }

    /// 按入参判定风险。未知工具一律 [`ToolRisk::NeedsApproval`]：
    /// 模型幻觉出的工具名不该被当成只读操作放行。
    #[must_use]
    pub fn risk_of(&self, call: &ToolCall) -> ToolRisk {
        match self.find(&call.name) {
            Some(tool) => tool.assess(&call.parsed_arguments()),
            None => ToolRisk::NeedsApproval,
        }
    }

    /// 执行一次调用。未知工具回一条**可诊断**的失败，而不是 panic 或空串。
    #[must_use]
    pub fn run(&self, call: &ToolCall, cancel: &AtomicBool) -> ToolOutcome {
        let Some(tool) = self.find(&call.name) else {
            return ToolOutcome::error(format!(
                "unknown tool {:?}; available: {}",
                call.name,
                if self.tools.is_empty() {
                    "(none)".to_owned()
                } else {
                    self.names().join(", ")
                }
            ));
        };
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return ToolOutcome::error("cancelled before execution".to_owned());
        }
        tool.run(&call.parsed_arguments(), cancel)
    }

    fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .map(AsRef::as_ref)
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定返回一段文本的测试工具。
    struct Echo {
        risk: ToolRisk,
        answer: String,
        seen: std::sync::Mutex<Option<serde_json::Value>>,
    }

    impl Echo {
        fn new(risk: ToolRisk, answer: &str) -> Self {
            Self {
                risk,
                answer: answer.to_owned(),
                seen: std::sync::Mutex::new(None),
            }
        }
    }

    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".to_owned(),
                description: "echo back".to_owned(),
                schema: serde_json::json!({"type": "object"}),
                risk: self.risk,
            }
        }

        fn run(&self, input: &serde_json::Value, _cancel: &AtomicBool) -> ToolOutcome {
            *self.seen.lock().unwrap() = Some(input.clone());
            ToolOutcome::text(self.answer.clone())
        }
    }

    /// 风险随入参变化的工具（`run_sql_readonly` 的模型）。
    struct Gated;

    impl Tool for Gated {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "gated".to_owned(),
                description: "risk depends on input".to_owned(),
                schema: serde_json::json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            }
        }

        fn assess(&self, input: &serde_json::Value) -> ToolRisk {
            if input.get("write").and_then(serde_json::Value::as_bool) == Some(true) {
                ToolRisk::NeedsApproval
            } else {
                ToolRisk::ReadOnly
            }
        }

        fn run(&self, _input: &serde_json::Value, _cancel: &AtomicBool) -> ToolOutcome {
            ToolOutcome::text("ran")
        }
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    #[test]
    fn truncation_is_a_no_op_below_the_limit() {
        let text = "a".repeat(TruncationTier::Headline.limit());
        assert_eq!(truncate_tiered(&text, TruncationTier::Headline), text);
    }

    #[test]
    fn truncation_announces_what_it_dropped() {
        let text = "字".repeat(TruncationTier::Headline.limit() + 5);
        let cut = truncate_tiered(&text, TruncationTier::Headline);
        // 静默截断是禁止的：必须写明总量与已显示量。
        assert!(
            cut.contains("showing first 1000 of 1005 characters"),
            "{cut}"
        );
        // 被留下的部分**按字符**计数，而不是字节（中文一字三字节，按字节会砍出半个字）。
        assert_eq!(cut.chars().filter(|c| *c == '字').count(), 1000);
    }

    #[test]
    fn tiers_have_three_distinct_limits() {
        assert_eq!(TruncationTier::Context.limit(), 12_000);
        assert_eq!(TruncationTier::Preview.limit(), 2_000);
        assert_eq!(TruncationTier::Headline.limit(), 1_000);
    }

    #[test]
    fn outcome_text_is_truncated_to_context_tier() {
        let long = "x".repeat(20_000);
        let outcome = ToolOutcome::text(long);
        assert!(outcome.ok);
        assert!(outcome.content.starts_with('x'));
        assert!(outcome.content.contains("[truncated:"));
        assert!(outcome.content.chars().count() < 20_000);
    }

    #[test]
    fn declaration_matches_the_openai_shape() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo::new(ToolRisk::ReadOnly, "hi"));
        let declarations = registry.declarations();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0]["type"], "function");
        assert_eq!(declarations[0]["function"]["name"], "echo");
        assert_eq!(declarations[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn empty_registry_declares_nothing() {
        let registry = ToolRegistry::new();
        assert!(registry.is_empty());
        assert!(registry.declarations().is_empty());
    }

    #[test]
    fn duplicate_names_keep_the_first_registration() {
        let mut registry = ToolRegistry::new();
        assert!(registry.register(Echo::new(ToolRisk::ReadOnly, "first")));
        assert!(!registry.register(Echo::new(ToolRisk::ReadOnly, "second")));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn unknown_tool_is_reported_not_panicked() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo::new(ToolRisk::ReadOnly, "hi"));
        let outcome = registry.run(&call("nope", "{}"), &AtomicBool::new(false));
        assert!(!outcome.ok);
        assert!(outcome.content.contains("unknown tool"));
        // 提示里带可用清单，模型下一轮就能自己改对名字。
        assert!(outcome.content.contains("echo"));
    }

    #[test]
    fn unknown_tool_defaults_to_needs_approval() {
        let registry = ToolRegistry::new();
        assert_eq!(
            registry.risk_of(&call("nope", "{}")),
            ToolRisk::NeedsApproval
        );
    }

    #[test]
    fn assess_can_raise_risk_above_the_declared_default() {
        let mut registry = ToolRegistry::new();
        registry.register(Gated);
        assert_eq!(registry.risk_of(&call("gated", "{}")), ToolRisk::ReadOnly);
        assert_eq!(
            registry.risk_of(&call("gated", r#"{"write":true}"#)),
            ToolRisk::NeedsApproval
        );
    }

    #[test]
    fn run_receives_parsed_arguments() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo::new(ToolRisk::ReadOnly, "ok"));
        let outcome = registry.run(&call("echo", r#"{"path":"a/b"}"#), &AtomicBool::new(false));
        assert!(outcome.ok);
        assert_eq!(outcome.content, "ok");
    }

    #[test]
    fn run_refuses_after_cancel() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo::new(ToolRisk::ReadOnly, "ok"));
        let outcome = registry.run(&call("echo", "{}"), &AtomicBool::new(true));
        assert!(!outcome.ok);
        assert!(outcome.content.contains("cancelled"));
    }

    #[test]
    fn malformed_arguments_do_not_panic() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo::new(ToolRisk::ReadOnly, "ok"));
        // 流式拼出来的 arguments 可能是半截 JSON；工具必须能自己兜住。
        let outcome = registry.run(&call("echo", "{\"path\":"), &AtomicBool::new(false));
        assert!(outcome.ok);
    }

    #[test]
    fn risk_helpers_agree() {
        assert!(!ToolRisk::ReadOnly.needs_approval());
        assert!(ToolRisk::NeedsApproval.needs_approval());
        assert_eq!(ToolRisk::ReadOnly.as_str(), "read_only");
    }

    #[test]
    fn detail_round_trips_through_json() {
        let detail = ToolDetail::Table {
            columns: vec!["a".to_owned()],
            rows: vec![vec!["1".to_owned()]],
        };
        let encoded = serde_json::to_string(&detail).unwrap();
        assert!(encoded.contains("\"kind\":\"table\""));
        assert_eq!(
            serde_json::from_str::<ToolDetail>(&encoded).unwrap(),
            detail
        );
    }
}
