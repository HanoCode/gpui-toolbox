//! AI 助手内核：协议 / 流式 / 会话持久化三件事，**不依赖 gpui、不引 tokio**。
//!
//! 参考实现与取舍见 `docs/ai-design.md`。本 crate 的边界：
//!
//! - 认两套 wire 协议：**OpenAI 兼容**（多厂商模板差异仅在 `base_url`）与
//!   **Anthropic 原生 Messages API**（[`anthropic`]）；Gemini 留了接口但未实现。
//! - 只做同步阻塞调用；流式靠 `ureq` 的 `Body::as_reader()` 阻塞读，
//!   **线程由调用方起**（本 crate 自己不开线程、不建 runtime）。
//! - 不依赖 `duck-db` / `duck-redis` / `duck-term` 等引擎 crate；工具（tool use）
//!   的**执行**留在上层，内核只负责协议侧（M2 落地）。
//!
//! 模块地图：
//!
//! | 模块 | 职责 |
//! | --- | --- |
//! | [`wire`] | 消息 / 内容块 / 工具调用 / 流式事件的 wire 类型（serde，可单测） |
//! | [`sse`] | SSE 行解析 + 流式增量累积（含 `ToolCallAccumulator`，纯函数，可单测） |
//! | [`provider`] | 厂商模板目录 + OpenAI 兼容流式实现 + 非流式便捷方法 + 协议分派 |
//! | [`anthropic`] | Anthropic 原生 Messages API（请求体 / 具名 SSE 事件 / 非流式） |
//! | [`secret`] | API key 加密存取（转发 `duck_platform::vault`，密文进厂商档案） |
//! | [`store`] | 会话持久化（自管 `ai.db`，不碰 `duck-store` 表结构） |
//! | [`tool`] | 工具协议 / 注册表 / 分级截断 / 风险分级（**实现由上层注册**） |
//! | [`kb`] | 本地知识库：FTS5 索引与检索（含 CJK 预分词） |
//! | [`agent`] | 多轮驱动的取消句柄、工具循环与审批闸门 |
//! | [`compaction`] | 上下文预算与压缩（裁旧工具结果 + 丢最旧整段，保持工具配对） |
//! | [`tokens`] | 上下文规模的启发式估算（**不是**精确 tokenizer，只报量级） |

pub mod agent;
pub mod anthropic;
pub mod compaction;
pub mod kb;
pub mod provider;
pub mod secret;
pub mod sse;
pub mod store;
pub mod tokens;
pub mod tool;
pub mod wire;

pub use agent::{
    AgentEvent, AgentOptions, AgentOutcome, Approval, ChatOptions, RunHandle, TurnOutcome,
    run_agent, run_turn,
};
pub use anthropic::{AnthropicAccumulator, DEFAULT_MAX_TOKENS, messages_url};
pub use compaction::{CompactionPolicy, CompactionReport, compact, compact_in_place};
pub use kb::{IndexProgress, IndexReport, KbHit, KbIndex, KbSource, KbStats};
pub use provider::{
    ChatRequest, Protocol, ProviderProfile, ProviderTemplate, RetryPolicy, TEMPLATES, chat_url,
    complete, complete_with_usage, complete_with_usage_with_retry, is_retryable, list_models,
    models_url, parse_models, protocol_of, stream_chat, stream_chat_with_retry, template,
};
pub use store::{AiDbRestore, AiStore, Conversation, StoredMessage, TableRestore};
pub use tokens::{estimate_conversation, estimate_message, estimate_messages, estimate_text};
pub use tool::{
    Tool, ToolDetail, ToolOutcome, ToolRegistry, ToolRisk, ToolSpec, TruncationTier,
    truncate_tiered,
};
pub use wire::{AiEvent, ChatMessage, ContentBlock, Role, StopReason, ToolCall, Usage};

use std::fmt;

/// 内核统一错误。
///
/// `Display` 为英文（本仓日志与错误约定），界面文案由调用方用 `tr` 组织。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiError {
    /// 配置不完整（缺 base_url / 模型名，或非本地端点缺 API key）。
    Config(String),
    /// 出网失败（连接、TLS、超时）。
    Network(String),
    /// 响应不符合协议（HTTP 错误码、JSON 结构不对、流里带 error）。
    Protocol(String),
    /// 本地持久化失败。
    Storage(String),
    /// 凭据存储失败（含平台不支持）。
    Secret(String),
    /// 调用方主动取消。
    Cancelled,
    /// 明确不支持的能力（如 Gemini 原生协议、SSH 隧道）。
    Unsupported(String),
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(detail) => write!(f, "AI configuration is incomplete: {detail}"),
            Self::Network(detail) => write!(f, "AI request failed: {detail}"),
            Self::Protocol(detail) => write!(f, "AI response is not valid: {detail}"),
            Self::Storage(detail) => write!(f, "AI storage failed: {detail}"),
            Self::Secret(detail) => write!(f, "AI credentials failed: {detail}"),
            Self::Cancelled => f.write_str("AI request was cancelled"),
            Self::Unsupported(detail) => write!(f, "unsupported: {detail}"),
        }
    }
}

impl std::error::Error for AiError {}
