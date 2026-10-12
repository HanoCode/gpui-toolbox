//! 对话与流式事件的 wire 类型。
//!
//! 设计来源：`temp/zed` 的 `language_model_core/src/request.rs`（`MessageContent`
//! 的 `Text` / `Thinking` / `ToolUse` / `ToolResult` 四态）与
//! `temp/oxideterm-ai` 的 `types.rs`（流式事件枚举）。这里做了裁剪：
//! 去掉图像块、去掉 Zed 特有的缓存控制字段，只保留工具箱对话实际会用到的四类内容块。
//!
//! 本模块**只描述数据**，不做网络、不碰 UI。

use serde::{Deserialize, Serialize};

/// 消息角色。
///
/// `Tool` 是工具结果消息：OpenAI 兼容协议里它必须带 `tool_call_id`，
/// 且紧跟产生该调用的助手消息之后。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    /// 落库 / 请求体里的 wire 名。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    /// 从 wire 名解析；未知值回落到 [`Role::Assistant`]。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "system" => Self::System,
            "user" => Self::User,
            "tool" => Self::Tool,
            _ => Self::Assistant,
        }
    }
}

/// 一次工具调用（助手发起的）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// provider 给的调用 id（回灌 `ToolResult` 时必须原样带回）。
    pub id: String,
    /// 工具名。
    pub name: String,
    /// 原始参数 JSON 字符串——**保持原样不解析**：流式分片拼装后的结果可能与
    /// 结构化再序列化不等价（键序、空白），原样回灌最安全。
    pub arguments: String,
}

impl ToolCall {
    /// 解析参数为 JSON；非法或空串时返回 `Value::Null`。
    #[must_use]
    pub fn parsed_arguments(&self) -> serde_json::Value {
        if self.arguments.trim().is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::from_str(&self.arguments).unwrap_or(serde_json::Value::Null)
    }
}

/// 消息内容块。
///
/// 用带标签的枚举而不是「字段可选的结构体」：一条助手消息里
/// 「说了什么（`Text`）」「想了什么（`Thinking`）」「调了什么（`ToolUse`）」
/// 是并列关系且有顺序意义，扁平结构体表达不了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 正文文本。
    Text { text: String },
    /// 思考过程（`reasoning_content` / `reasoning`）。
    Thinking { text: String },
    /// 一次工具调用。
    ToolUse { call: ToolCall },
    /// 一次工具结果（`role = Tool` 的消息用这个块）。
    ToolResult {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

/// 一条对话消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
}

impl ChatMessage {
    /// 系统消息。
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            blocks: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 用户消息。
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 助手消息（纯文本）。
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 工具结果消息。
    #[must_use]
    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                call_id: call_id.into(),
                name: name.into(),
                content: content.into(),
                is_error,
            }],
        }
    }

    /// 拼接全部 `Text` 块（界面与落库用）。
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            if let ContentBlock::Text { text } = block {
                out.push_str(text);
            }
        }
        out
    }

    /// 拼接全部 `Thinking` 块；一个都没有时返回 `None`。
    #[must_use]
    pub fn thinking(&self) -> Option<String> {
        let mut out = String::new();
        for block in &self.blocks {
            if let ContentBlock::Thinking { text } = block {
                out.push_str(text);
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// 本条消息里的全部工具调用。
    #[must_use]
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { call } => Some(call),
                _ => None,
            })
            .collect()
    }

    /// 是否含有可见内容（用于决定要不要渲染一个空壳气泡）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Thinking { text } => text.is_empty(),
            ContentBlock::ToolUse { .. } => false,
            ContentBlock::ToolResult { content, .. } => content.is_empty(),
        })
    }
}

/// 一次请求的用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

/// 结束原因。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 模型自然说完。
    #[default]
    EndTurn,
    /// 撞到 `max_tokens`。
    MaxTokens,
    /// 模型要求调用工具（M2 起生效）。
    ToolUse,
    /// 其它 / 未知。
    Other,
}

impl StopReason {
    /// 从 provider 的 `finish_reason` 解析。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "stop" => Self::EndTurn,
            "length" => Self::MaxTokens,
            "tool_calls" | "function_call" => Self::ToolUse,
            _ => Self::Other,
        }
    }
}

/// 流式事件：内核 → 上层的唯一出口。
///
/// 上层（UI）按事件类型累积 UI 状态；内核不持有任何 UI 概念。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiEvent {
    /// 请求已发出，开始收到响应。
    Started,
    /// 正文增量。
    TextDelta(String),
    /// 思考增量。
    ThinkingDelta(String),
    /// 工具调用已拼装完整（一次给全，M2 起生效）。
    ToolCalls(Vec<ToolCall>),
    /// 本轮结束。
    Finished {
        stop_reason: StopReason,
        usage: Usage,
    },
    /// 出错（流中途带 error 或读取失败）。
    Failed(String),
}
