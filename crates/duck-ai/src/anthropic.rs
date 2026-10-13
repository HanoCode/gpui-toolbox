//! Anthropic 原生 Messages API（`POST /v1/messages`）。
//!
//! 与 OpenAI 兼容协议的差异**都是真差异**，不是换名字那么简单：
//!
//! 1. **鉴权头**是 `x-api-key` + `anthropic-version`，不是 `Authorization: Bearer`。
//! 2. **`system` 是顶层字段**，不放进 `messages`；把 system 当成一条消息发过去会被拒。
//! 3. **`max_tokens` 必填**（OpenAI 侧可选）——调用方没给时用 [`DEFAULT_MAX_TOKENS`] 兜底。
//! 4. **工具结果属于 `user` 消息**（content 数组里的一个 `tool_result` 块），
//!    而 OpenAI 是独立的 `role: tool` 消息。
//! 5. 工具的 schema 字段叫 `input_schema`，对应 OpenAI 的 `function.parameters`。
//! 6. 流式用**具名事件**（`message_start` / `content_block_delta` / …）；但 `data:`
//!    负载里自带 `type`，所以复用 [`crate::sse::parse_line`] 只读 `data:` 即可，
//!    `event:` 行会被它当无关字段跳过。
//!
//! 分工与 [`crate::provider`] 一致：**纯协议 + 阻塞 I/O**，线程由调用方起。
//! 内核不建 runtime，也不碰 UI。

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::AiError;
use crate::provider::{ChatRequest, ProviderProfile, agent, extract_error_message, map_transport};
use crate::sse::{SseLine, parse_line};
use crate::wire::{AiEvent, ChatMessage, ContentBlock, Role, StopReason, ToolCall, Usage};

/// Anthropic 要求 `max_tokens` 必填；调用方没给时用这个兜底。
///
/// 不选更小的值：截断比慢更糟（用户看到半句话会以为是模型的问题）。
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Messages API 的版本头。Anthropic 要求显式声明。
pub const API_VERSION: &str = "2023-06-01";

/// `POST {base_url}/messages` 的完整 URL。
#[must_use]
pub fn messages_url(profile: &ProviderProfile) -> String {
    format!("{}/messages", profile.base_url.trim_end_matches('/'))
}

/// 组装 `POST /v1/messages` 的请求体。
#[must_use]
pub fn build_body(model: &str, request: &ChatRequest, stream: bool) -> Value {
    let (system, messages) = to_anthropic_messages(&request.messages);
    let mut body = json!({
        "model": model,
        "max_tokens": request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": messages,
        "stream": stream,
    });
    if let Some(system) = system {
        body["system"] = json!(system);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(to_anthropic_tools(&request.tools));
    }
    body
}

/// 把 OpenAI 形状的工具声明翻成 Anthropic 的 `input_schema` 形状。
///
/// 入参是既有的 `{"type":"function","function":{…}}` 数组（上层 `ai_tools` 按
/// OpenAI 形状构造，见 [`crate::provider::build_body`]）。缺 `parameters` 时补一个
/// 空对象 schema —— Anthropic 侧 `input_schema` 也是必填。
#[must_use]
pub fn to_anthropic_tools(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for tool in tools {
        // 兼容「已剥掉外层」的输入：没有 `function` 时就把自己当 function。
        let function = tool.get("function").unwrap_or(tool);
        let Some(name) = function.get("name").and_then(Value::as_str) else {
            continue;
        };
        let mut entry = json!({ "name": name });
        if let Some(description) = function.get("description").and_then(Value::as_str) {
            entry["description"] = json!(description);
        }
        entry["input_schema"] = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" }));
        out.push(entry);
    }
    out
}

/// 把内部消息翻成 Anthropic 的 `(system, messages)` 两半。
///
/// 三条规则：
/// 1. `System` 消息的文本**抽到顶层**（多条用空行连接），不进 `messages`。
/// 2. `Tool` 角色落到 `user` 消息里的 `tool_result` 块；**相邻同角色**的消息合并成
///    一条（Anthropic 要求 `user` / `assistant` 交替，连续两条 `user` 会被拒）。
/// 3. `Thinking` 块**不回灌**——与 OpenAI 侧同一口径（见 `provider::to_openai_messages`）。
#[must_use]
pub fn to_anthropic_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
    let mut system = String::new();
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();

    for message in messages {
        let role = match message.role {
            Role::System => {
                for block in &message.blocks {
                    if let ContentBlock::Text { text } = block {
                        if text.is_empty() {
                            continue;
                        }
                        if !system.is_empty() {
                            system.push_str("\n\n");
                        }
                        system.push_str(text);
                    }
                }
                continue;
            }
            Role::User | Role::Tool => "user",
            Role::Assistant => "assistant",
        };

        let mut blocks = Vec::new();
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                }
                // 同 OpenAI 侧：思考过程不回灌。
                ContentBlock::Thinking { .. } => {}
                ContentBlock::ToolUse { call } => {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.parsed_arguments(),
                    }));
                }
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                    ..
                } => {
                    let mut entry = json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": content,
                    });
                    if *is_error {
                        entry["is_error"] = json!(true);
                    }
                    blocks.push(entry);
                }
            }
        }

        if blocks.is_empty() {
            continue;
        }
        // 合并相邻同角色：Anthropic 不接受连续两条 `user`（工具结果常与用户输入挨着）。
        match out.last_mut() {
            Some(last) if last.0 == role => {
                last.1.extend(blocks);
                continue;
            }
            _ => {}
        }
        out.push((role, blocks));
    }

    let messages = out
        .into_iter()
        .map(|(role, blocks)| json!({ "role": role, "content": blocks }))
        .collect();
    ((!system.is_empty()).then_some(system), messages)
}

/// Anthropic 的 `stop_reason` 取词。
///
/// 与 OpenAI 的 `finish_reason` **不是同一套值**（`end_turn` ≠ `stop`），
/// 所以不能复用 [`StopReason::from_wire`]。
#[must_use]
pub fn stop_reason_from_wire(value: &str) -> StopReason {
    match value {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Other,
    }
}

/// 一个分片累加中的工具调用槽位。
#[derive(Debug, Default)]
struct PartialTool {
    id: String,
    name: String,
    json: String,
}

/// Anthropic 具名流式事件的累积器。
///
/// 文本与思考增量**立即**上报；工具调用**攒到最后**一次性上报（`input_json_delta`
/// 是分片到达的，半成品 JSON 解析必失败）。
#[derive(Debug, Default)]
pub struct AnthropicAccumulator {
    tools: BTreeMap<i64, PartialTool>,
    usage: Usage,
    stop_reason: StopReason,
    finished: bool,
}

impl AnthropicAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 合并一份 `usage`（Anthropic 只给 `input_tokens` / `output_tokens`，无 total）。
    fn merge_usage(&mut self, value: &Value) {
        let pick = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|tokens| u32::try_from(tokens).ok())
        };
        if let Some(input) = pick("input_tokens") {
            self.usage.input_tokens = input;
        }
        if let Some(output) = pick("output_tokens") {
            self.usage.output_tokens = output;
        }
        // Anthropic 不报 total，自己加；两半都缺时保持 0。
        self.usage.total_tokens = self.usage.input_tokens + self.usage.output_tokens;
    }

    /// 吃一条 `data:` 负载，返回本次要上报的事件。
    ///
    /// 未识别的 `type`（`ping` / `content_block_stop` / `message_stop`）静默跳过——
    /// 它们不带新信息，报错反而会把正常流打断。
    pub fn feed(&mut self, data: &str) -> Vec<AiEvent> {
        let mut events = Vec::new();
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return events;
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match kind {
            "message_start" => {
                if let Some(usage) = value.pointer("/message/usage") {
                    self.merge_usage(usage);
                }
            }
            "content_block_start" => {
                let index = value.get("index").and_then(Value::as_i64).unwrap_or(0);
                let tool_use = value
                    .get("content_block")
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"));
                if let Some(block) = tool_use {
                    let slot = self.tools.entry(index).or_default();
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        slot.id = id.to_owned();
                    }
                    if let Some(name) = block.get("name").and_then(Value::as_str) {
                        slot.name = name.to_owned();
                    }
                }
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(Value::as_i64).unwrap_or(0);
                if let Some(delta) = value.get("delta") {
                    match delta
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "text_delta" => {
                            let text = delta
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty());
                            if let Some(text) = text {
                                events.push(AiEvent::TextDelta(text.to_owned()));
                            }
                        }
                        "thinking_delta" => {
                            let text = delta
                                .get("thinking")
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty());
                            if let Some(text) = text {
                                events.push(AiEvent::ThinkingDelta(text.to_owned()));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(part) = delta.get("partial_json").and_then(Value::as_str) {
                                let slot = self.tools.entry(index).or_default();
                                slot.json.push_str(part);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = stop_reason_from_wire(reason);
                }
                if let Some(usage) = value.get("usage").filter(|entry| !entry.is_null()) {
                    self.merge_usage(usage);
                }
            }
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        value
                            .get("error")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| value.to_string());
                events.push(AiEvent::Failed(message));
            }
            _ => {}
        }
        events
    }

    /// 流结束后收尾：吐工具调用（若有）与 `Finished`。幂等。
    pub fn finish(&mut self) -> Vec<AiEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut events = Vec::new();
        let calls: Vec<ToolCall> = std::mem::take(&mut self.tools)
            .into_iter()
            .filter(|(_, slot)| !slot.name.is_empty())
            .map(|(_, slot)| ToolCall {
                id: slot.id,
                name: slot.name,
                arguments: if slot.json.trim().is_empty() {
                    "{}".to_owned()
                } else {
                    slot.json
                },
            })
            .collect();
        if !calls.is_empty() {
            self.stop_reason = StopReason::ToolUse;
            events.push(AiEvent::ToolCalls(calls));
        }
        events.push(AiEvent::Finished {
            stop_reason: self.stop_reason,
            usage: self.usage,
        });
        events
    }

    /// 目前累计到的用量。
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// 目前累计到的结束原因。
    #[must_use]
    pub fn stop_reason(&self) -> StopReason {
        self.stop_reason
    }
}

/// 单次流式尝试（不含重试）。`committed` 在第一次向 `sink` 发事件时置位。
pub(crate) fn stream_once(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(AiEvent),
    committed: &mut bool,
) -> Result<(), AiError> {
    let body = build_body(&request.model, request, true);
    let payload = serde_json::to_vec(&body)
        .map_err(|error| AiError::Protocol(format!("request encode failed: {error}")))?;
    let client = agent(timeout_secs, true);
    let mut response = client
        .post(&messages_url(profile))
        .header("x-api-key", api_key.trim())
        .header("anthropic-version", API_VERSION)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .send(payload.as_slice())
        .map_err(map_transport)?;

    let status = response.status();
    if !status.is_success() {
        let detail = response.body_mut().read_to_string().unwrap_or_default();
        return Err(AiError::Protocol(format!(
            "HTTP {}: {}",
            status.as_u16(),
            extract_error_message(&detail)
        )));
    }

    // 到这里还没吐过任何东西 —— 期间失败都可安全重试。
    *committed = true;
    sink(AiEvent::Started);

    let mut accumulator = AnthropicAccumulator::new();
    let reader = response.body_mut().as_reader();
    for line in BufReader::new(reader).lines() {
        if cancel.load(Ordering::Relaxed) {
            return Err(AiError::Cancelled);
        }
        let line = line.map_err(|error| AiError::Network(error.to_string()))?;
        match parse_line(&line) {
            SseLine::Skip => {}
            // Anthropic 不发 `[DONE]`，以 `message_stop` 事件收尾；这里照样认它。
            SseLine::Done => break,
            SseLine::Data(data) => {
                for event in accumulator.feed(data) {
                    if let AiEvent::Failed(message) = &event {
                        let message = message.clone();
                        sink(event);
                        return Err(AiError::Protocol(message));
                    }
                    sink(event);
                }
            }
        }
    }

    for event in accumulator.finish() {
        sink(event);
    }
    Ok(())
}

/// 单次非流式请求（不含重试）。
pub(crate) fn complete_once(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
) -> Result<(String, Usage), AiError> {
    let body = build_body(&request.model, request, false);
    let payload = serde_json::to_vec(&body)
        .map_err(|error| AiError::Protocol(format!("request encode failed: {error}")))?;
    let client = agent(timeout_secs, false);
    let mut response = client
        .post(&messages_url(profile))
        .header("x-api-key", api_key.trim())
        .header("anthropic-version", API_VERSION)
        .header("Content-Type", "application/json")
        .send(payload.as_slice())
        .map_err(map_transport)?;

    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| AiError::Network(error.to_string()))?;
    if !status.is_success() {
        return Err(AiError::Protocol(format!(
            "HTTP {}: {}",
            status.as_u16(),
            extract_error_message(&text)
        )));
    }

    let value: Value = serde_json::from_str(&text)
        .map_err(|error| AiError::Protocol(format!("response parse failed: {error}")))?;

    // 正文：把所有 `text` 块拼起来（Anthropic 的 content 是块数组）。
    let mut content = String::new();
    if let Some(blocks) = value.get("content").and_then(Value::as_array) {
        for text in blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
        {
            content.push_str(text);
        }
    }
    let content = content.trim().to_owned();
    if content.is_empty() {
        return Err(AiError::Protocol("response has no message content".into()));
    }

    let read_tokens = |key: &str| {
        value
            .pointer(&format!("/usage/{key}"))
            .and_then(Value::as_u64)
            .and_then(|tokens| u32::try_from(tokens).ok())
            .unwrap_or_default()
    };
    let usage = Usage {
        input_tokens: read_tokens("input_tokens"),
        output_tokens: read_tokens("output_tokens"),
        total_tokens: 0,
    };
    let usage = Usage {
        total_tokens: usage.input_tokens + usage.output_tokens,
        ..usage
    };
    Ok((content, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_request() -> ChatRequest {
        ChatRequest::new("claude-sonnet-4-5", vec![ChatMessage::user("hi")])
    }

    fn stub_profile(base_url: &str) -> ProviderProfile {
        ProviderProfile {
            id: "stub".into(),
            template_id: "anthropic".into(),
            name: "stub".into(),
            base_url: base_url.to_owned(),
            model: "claude-sonnet-4-5".into(),
            is_local: true,
            api_key: None,
        }
    }

    #[test]
    fn messages_url_has_no_double_slash() {
        assert_eq!(
            messages_url(&stub_profile("https://api.anthropic.com/v1/")),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url(&stub_profile("https://api.anthropic.com/v1")),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn system_is_lifted_to_the_top_level() {
        let messages = vec![
            ChatMessage::system("you are duck"),
            ChatMessage::system("be terse"),
            ChatMessage::user("hi"),
        ];
        let (system, wire) = to_anthropic_messages(&messages);
        assert_eq!(system.as_deref(), Some("you are duck\n\nbe terse"));
        // system 不进 messages。
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], "user");
    }

    #[test]
    fn max_tokens_is_always_present() {
        // 调用方没给也要兜底 —— Anthropic 拒收缺 `max_tokens` 的请求。
        let body = build_body("m", &user_request(), true);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["stream"], true);
        assert!(body.get("system").is_none());

        let mut request = user_request();
        request.max_tokens = Some(64);
        assert_eq!(build_body("m", &request, false)["max_tokens"], 64);
    }

    #[test]
    fn tool_results_become_user_messages() {
        let messages = vec![
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    call: ToolCall {
                        id: "toolu_1".into(),
                        name: "read_file".into(),
                        arguments: "{\"path\":\"a.txt\"}".into(),
                    },
                }],
            },
            ChatMessage::tool_result("toolu_1", "read_file", "hello", false),
        ];
        let (_, wire) = to_anthropic_messages(&messages);
        assert_eq!(wire.len(), 2);
        // 助手的工具调用：input 是**对象**，不是 JSON 字符串。
        assert_eq!(wire[0]["role"], "assistant");
        assert_eq!(wire[0]["content"][0]["type"], "tool_use");
        assert_eq!(wire[0]["content"][0]["input"]["path"], "a.txt");
        // 工具结果落在 user 消息里。
        assert_eq!(wire[1]["role"], "user");
        assert_eq!(wire[1]["content"][0]["type"], "tool_result");
        assert_eq!(wire[1]["content"][0]["tool_use_id"], "toolu_1");
        assert!(wire[1]["content"][0].get("is_error").is_none());
    }

    #[test]
    fn tool_errors_are_flagged() {
        let messages = vec![ChatMessage::tool_result("t", "boom", "no such file", true)];
        let (_, wire) = to_anthropic_messages(&messages);
        assert_eq!(wire[0]["content"][0]["is_error"], true);
    }

    #[test]
    fn adjacent_same_role_messages_are_merged() {
        // 用户输入后紧跟工具结果：两者都是 `user`，必须合成一条。
        let messages = vec![
            ChatMessage::user("check the file"),
            ChatMessage::tool_result("t1", "read_file", "ok", false),
        ];
        let (_, wire) = to_anthropic_messages(&messages);
        assert_eq!(wire.len(), 1, "连续 user 会被 Anthropic 拒绝");
        assert_eq!(wire[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn thinking_blocks_are_not_sent_back() {
        let mut message = ChatMessage::assistant("answer");
        message.blocks.insert(
            0,
            ContentBlock::Thinking {
                text: "internal".into(),
            },
        );
        let (_, wire) = to_anthropic_messages(&[message]);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(wire[0]["content"][0]["text"], "answer");
    }

    #[test]
    fn tools_map_to_input_schema() {
        let tools = vec![
            json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "read a file",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            }),
            // 缺 parameters：补空对象 schema，不能整条丢掉。
            json!({ "type": "function", "function": { "name": "noop" } }),
        ];
        let converted = to_anthropic_tools(&tools);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0]["name"], "read_file");
        assert_eq!(converted[0]["description"], "read a file");
        assert_eq!(
            converted[0]["input_schema"]["properties"]["path"]["type"],
            "string"
        );
        assert!(
            converted[0].get("function").is_none(),
            "不该留着 function 外层"
        );
        assert_eq!(converted[1]["input_schema"]["type"], "object");
    }

    #[test]
    fn body_carries_tools_and_temperature() {
        let mut request = user_request();
        request.temperature = Some(0.2);
        request.tools = vec![json!({
            "type": "function",
            "function": { "name": "x" }
        })];
        let body = build_body("m", &request, false);
        assert_eq!(body["tools"][0]["name"], "x");
        let temperature = body["temperature"]
            .as_f64()
            .expect("temperature is a number");
        assert!((temperature - 0.2).abs() < 1e-6, "got {temperature}");
    }

    #[test]
    fn text_and_thinking_deltas_are_emitted_in_order() {
        let mut acc = AnthropicAccumulator::new();
        let mut events = Vec::new();
        events.extend(acc.feed(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":9,"output_tokens":1}}}"#,
        ));
        events.extend(acc.feed(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        ));
        events.extend(acc.feed(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        ));
        events.extend(acc.feed(r#"{"type":"ping"}"#));
        events.extend(acc.feed(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#,
        ));
        assert_eq!(
            events,
            vec![
                AiEvent::ThinkingDelta("hmm".into()),
                AiEvent::TextDelta("Hi".into()),
            ]
        );
        let tail = acc.finish();
        assert_eq!(
            tail,
            vec![AiEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 9,
                    output_tokens: 4,
                    total_tokens: 13,
                },
            }]
        );
        // 幂等。
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn tool_use_input_is_accumulated_from_json_deltas() {
        let mut acc = AnthropicAccumulator::new();
        acc.feed(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"read_file"}}"#,
        );
        acc.feed(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        );
        acc.feed(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}"#,
        );
        acc.feed(r#"{"type":"content_block_stop","index":1}"#);
        let events = acc.finish();
        let AiEvent::ToolCalls(calls) = &events[0] else {
            panic!("expected tool calls, got {events:?}");
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_9");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].parsed_arguments()["path"], "a.txt");
        // 工具调用会把 stop_reason 抬成 ToolUse（与 OpenAI 侧同一口径）。
        assert!(matches!(
            events[1],
            AiEvent::Finished {
                stop_reason: StopReason::ToolUse,
                ..
            }
        ));
    }

    #[test]
    fn stop_reason_mapping_differs_from_openai() {
        assert_eq!(stop_reason_from_wire("end_turn"), StopReason::EndTurn);
        assert_eq!(stop_reason_from_wire("stop_sequence"), StopReason::EndTurn);
        assert_eq!(stop_reason_from_wire("max_tokens"), StopReason::MaxTokens);
        assert_eq!(stop_reason_from_wire("tool_use"), StopReason::ToolUse);
        assert_eq!(stop_reason_from_wire("weird"), StopReason::Other);
        // 反向钉住：OpenAI 的 "stop" 在这套取词里**不是** EndTurn 以外的值。
        assert_eq!(stop_reason_from_wire("stop"), StopReason::Other);
    }

    #[test]
    fn error_event_stops_the_stream() {
        let mut acc = AnthropicAccumulator::new();
        let events = acc
            .feed(r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#);
        assert_eq!(events, vec![AiEvent::Failed("overloaded".into())]);
    }

    #[test]
    fn non_json_payloads_are_ignored() {
        let mut acc = AnthropicAccumulator::new();
        assert!(acc.feed("ping").is_empty());
        assert!(acc.feed(r#"{"type":"message_stop"}"#).is_empty());
    }

    // ── 端到端（本机 stub，只覆盖 Anthropic 用到的响应形状） ────────────────

    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    struct Reply {
        status: u16,
        body: String,
    }

    impl Reply {
        fn ok(body: &str) -> Self {
            Self {
                status: 200,
                body: body.to_owned(),
            }
        }

        fn status(code: u16) -> Self {
            Self {
                status: code,
                body: format!("{{\"type\":\"error\",\"error\":{{\"message\":\"boom {code}\"}}}}"),
            }
        }
    }

    struct Stub {
        base_url: String,
        requests: Arc<Mutex<usize>>,
    }

    fn stub_server(replies: Vec<Reply>, repeat: bool) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(0usize));
        let recorded = Arc::clone(&requests);
        std::thread::spawn(move || {
            let total = replies.len();
            let mut index = 0usize;
            loop {
                if index >= total && !repeat {
                    return;
                }
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                *recorded.lock().unwrap() += 1;
                drain_request(&mut stream);
                let reply = &replies[index.min(total - 1)];
                let reason = if reply.status == 200 { "OK" } else { "Error" };
                let head = format!(
                    "HTTP/1.1 {} {reason}\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.status,
                    reply.body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.body.as_bytes());
                let _ = stream.flush();
                index += 1;
            }
        });
        Stub {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    fn drain_request(stream: &mut TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let Ok(read) = stream.read(&mut chunk) else {
                return;
            };
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        while buffer.len() < header_end + length {
            let Ok(read) = stream.read(&mut chunk) else {
                return;
            };
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }

    /// 拼一串 Anthropic 的 SSE 行（只发 `data:`，与真实响应等价）。
    fn sse(payloads: &[&str]) -> String {
        let mut out = String::new();
        for payload in payloads {
            out.push_str("event: x\n");
            out.push_str("data: ");
            out.push_str(payload);
            out.push_str("\n\n");
        }
        out
    }

    #[test]
    fn streaming_reads_named_events_end_to_end() {
        let body = sse(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":1}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let stub = stub_server(vec![Reply::ok(&body)], false);
        let mut events = Vec::new();
        let mut committed = false;
        stream_once(
            &stub_profile(&stub.base_url),
            "sk-ant-test",
            &user_request(),
            5,
            &AtomicBool::new(false),
            &mut |event| events.push(event),
            &mut committed,
        )
        .expect("流式应当跑完");
        assert!(committed);
        assert_eq!(events.first(), Some(&AiEvent::Started));
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                AiEvent::TextDelta(chunk) => Some(chunk.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert_eq!(
            events.last(),
            Some(&AiEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 3,
                    total_tokens: 8,
                },
            })
        );
        assert_eq!(*stub.requests.lock().unwrap(), 1);
    }

    #[test]
    fn streaming_surfaces_http_errors() {
        let stub = stub_server(vec![Reply::status(401)], false);
        let mut committed = false;
        let error = stream_once(
            &stub_profile(&stub.base_url),
            "bad",
            &user_request(),
            5,
            &AtomicBool::new(false),
            &mut |_| {},
            &mut committed,
        )
        .expect_err("401 应当报错");
        assert!(error.to_string().contains("401"), "{error}");
        // 连接阶段就失败：没有吐过事件，可安全重试。
        assert!(!committed);
    }

    #[test]
    fn non_streaming_joins_text_blocks() {
        let body = r#"{"content":[{"type":"text","text":"SQL: "},{"type":"text","text":"SELECT 1"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}"#;
        let stub = stub_server(vec![Reply::ok(body)], false);
        let (content, usage) =
            complete_once(&stub_profile(&stub.base_url), "k", &user_request(), 5)
                .expect("非流式应当成功");
        assert_eq!(content, "SQL: SELECT 1");
        assert_eq!(
            usage,
            Usage {
                input_tokens: 7,
                output_tokens: 2,
                total_tokens: 9,
            }
        );
    }
}
