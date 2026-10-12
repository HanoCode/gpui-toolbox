//! 厂商模板目录 + OpenAI 兼容的流式 / 非流式实现。
//!
//! 决定性简化：**多模板 ≠ 多协议**。首版所有模板都走 OpenAI 兼容的
//! `POST {base_url}/chat/completions`，差异只在 `base_url` 与默认模型列表——
//! 因为国内主流厂商与本地 Ollama 都提供 OpenAI 兼容端点
//! （参考 `temp/oxideterm-ai/src/providers/catalog.rs`）。
//!
//! 协议实现走 `ureq` 的**阻塞式**流读取（`Body::as_reader()` → `impl Read`），
//! 这是本仓「零 tokio 纪律」下流式输出的唯一可行路径。
//!
//! **本模块自己不开线程、不建 runtime**：`stream_chat` 是同步阻塞函数，
//! 线程由调用方（UI 层）起。这样内核保持可单测，也不会污染 gpui 的后台执行器。

use std::io::{BufRead as _, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::AiError;
use crate::sse::{SseLine, StreamAccumulator, parse_line};
use crate::wire::{AiEvent, ChatMessage, ContentBlock, Role};

/// wire 协议族。首版只实现 [`Protocol::OpenAi`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// OpenAI 兼容 `/chat/completions`（覆盖绝大多数厂商与本地推理服务）。
    OpenAi,
    /// Anthropic 原生 `/v1/messages`。**M2 接入**——UI 上须灰显标注，不做静默失败。
    Anthropic,
    /// Google Gemini 原生 `generateContent`。**M2 接入**。
    Gemini,
}

/// 一个内置厂商模板。
#[derive(Debug, Clone, Copy)]
pub struct ProviderTemplate {
    /// 稳定 id，用于落盘与查表。
    pub id: &'static str,
    /// 展示名（品牌名，不进 i18n 表）。
    pub name: &'static str,
    /// 端点前缀，**不带尾斜杠**。自定义模板为空串，由用户填。
    pub base_url: &'static str,
    /// 模板自带的可选模型。
    pub default_models: &'static [&'static str],
    /// API key 申请入口，设置页展示为外链。
    pub key_url: Option<&'static str>,
    /// 本地端点（Ollama 等），免 key。
    pub is_local: bool,
    /// wire 协议族。
    pub protocol: Protocol,
}

impl ProviderTemplate {
    /// 本模板在当前版本是否可用（原生协议留待 M2）。
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self.protocol, Protocol::OpenAi)
    }
}

/// 内置模板目录。顺序即设置页下拉里的展示顺序，常用厂商在前。
pub const TEMPLATES: &[ProviderTemplate] = &[
    ProviderTemplate {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        default_models: &["gpt-4o-mini", "gpt-4o", "gpt-4.1", "o4-mini"],
        key_url: Some("https://platform.openai.com/api-keys"),
        is_local: false,
        protocol: Protocol::OpenAi,
    },
    ProviderTemplate {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        default_models: &["deepseek-chat", "deepseek-reasoner"],
        key_url: Some("https://platform.deepseek.com/api_keys"),
        is_local: false,
        protocol: Protocol::OpenAi,
    },
    ProviderTemplate {
        id: "moonshot",
        name: "Moonshot (Kimi)",
        base_url: "https://api.moonshot.cn/v1",
        default_models: &[
            "moonshot-v1-8k",
            "moonshot-v1-32k",
            "moonshot-v1-128k",
            "kimi-k2-0711-preview",
        ],
        key_url: Some("https://platform.moonshot.cn/console/api-keys"),
        is_local: false,
        protocol: Protocol::OpenAi,
    },
    ProviderTemplate {
        id: "zhipu",
        name: "Zhipu GLM",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        default_models: &["glm-4-plus", "glm-4-air", "glm-4-flash"],
        key_url: Some("https://open.bigmodel.cn/usercenter/apikeys"),
        is_local: false,
        protocol: Protocol::OpenAi,
    },
    ProviderTemplate {
        id: "qwen",
        name: "Qwen (DashScope)",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        default_models: &["qwen-plus", "qwen-max", "qwen-turbo"],
        key_url: Some("https://bailian.console.aliyun.com/"),
        is_local: false,
        protocol: Protocol::OpenAi,
    },
    ProviderTemplate {
        id: "ollama",
        name: "Ollama (local)",
        base_url: "http://127.0.0.1:11434/v1",
        default_models: &["llama3.1", "qwen2.5", "deepseek-r1"],
        key_url: None,
        is_local: true,
        protocol: Protocol::OpenAi,
    },
    ProviderTemplate {
        id: "openai_compatible",
        name: "OpenAI compatible (custom)",
        base_url: "",
        default_models: &[],
        key_url: None,
        is_local: false,
        protocol: Protocol::OpenAi,
    },
    ProviderTemplate {
        id: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
        default_models: &["claude-sonnet-4-5", "claude-opus-4-1"],
        key_url: Some("https://console.anthropic.com/settings/keys"),
        is_local: false,
        protocol: Protocol::Anthropic,
    },
    ProviderTemplate {
        id: "gemini",
        name: "Google Gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        default_models: &["gemini-2.0-flash", "gemini-2.5-pro"],
        key_url: Some("https://aistudio.google.com/apikey"),
        is_local: false,
        protocol: Protocol::Gemini,
    },
];

/// 按 id 查模板。
#[must_use]
pub fn template(id: &str) -> Option<&'static ProviderTemplate> {
    TEMPLATES.iter().find(|entry| entry.id == id)
}

/// 一条用户配置的厂商档案。
///
/// 落盘在 `kv: ai_providers`（单键 JSON 数组，范式同 `db_connections` / `redis_connections`）。
/// **API key 也在这个结构里，但是密文**：由 [`crate::secret::seal`] 产出（AES-256-GCM，
/// 见 `duck_platform::vault`）。解密的密钥在 `app_data_dir()/vault.key`（0600，不随
/// 备份包走），所以「拷走数据库文件」或「拿到导出的备份包」都读不出明文。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderProfile {
    /// 档案 id（新建时由调用方生成，建议用时间戳 + 随机后缀）。
    pub id: String,
    /// 来源模板 id。
    pub template_id: String,
    /// 展示名（用户可改）。
    pub name: String,
    /// 端点前缀。
    pub base_url: String,
    /// 当前选中的模型。
    pub model: String,
    /// 本地端点免 key。
    #[serde(default)]
    pub is_local: bool,
    /// API key 密文（[`crate::secret::seal`] 的产出）；`None` = 这条档案没配密钥。
    ///
    /// `#[serde(default)]`：旧存档里没有这个字段，读出来是 `None`（当作「没配」），
    /// 而不是让整份档案解析失败——那会让用户所有的厂商配置一起消失。
    #[serde(default)]
    pub api_key: Option<String>,
}

impl ProviderProfile {
    /// 由模板生成一条默认档案。
    #[must_use]
    pub fn from_template(source: &ProviderTemplate, id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            template_id: source.id.to_owned(),
            name: source.name.to_owned(),
            base_url: source.base_url.to_owned(),
            model: source
                .default_models
                .first()
                .copied()
                .unwrap_or("")
                .to_owned(),
            is_local: source.is_local,
            api_key: None,
        }
    }

    /// 是否必须配 API key。
    #[must_use]
    pub const fn needs_key(&self) -> bool {
        !self.is_local
    }

    /// 配置是否完整到可以发请求。
    pub fn validate(&self) -> Result<(), AiError> {
        if self.base_url.trim().is_empty() {
            return Err(AiError::Config("base_url is empty".into()));
        }
        if self.model.trim().is_empty() {
            return Err(AiError::Config("model is empty".into()));
        }
        Ok(())
    }
}

/// 一次请求的输入。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// 模型名（覆盖档案里的默认值）。
    pub model: String,
    /// 完整对话（含 system）。
    pub messages: Vec<ChatMessage>,
    /// 采样温度；`None` = 不传（用服务端默认）。
    pub temperature: Option<f32>,
    /// 最大输出 token；`None` = 不传。
    pub max_tokens: Option<u32>,
    /// 工具声明（OpenAI 的 `tools` 数组）。M1 为空。
    pub tools: Vec<serde_json::Value>,
}

impl ChatRequest {
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
        }
    }
}

/// 拼 `chat/completions` 的完整 URL。
#[must_use]
pub fn chat_url(profile: &ProviderProfile) -> String {
    format!(
        "{}/chat/completions",
        profile.base_url.trim_end_matches('/')
    )
}

/// 组装请求体。`stream` 决定是否开 SSE。
#[must_use]
pub fn build_body(model: &str, request: &ChatRequest, stream: bool) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": to_openai_messages(&request.messages),
        "stream": stream,
    });
    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }
    if !request.tools.is_empty() {
        body["tools"] = serde_json::json!(request.tools);
    }
    body
}

/// 把内部消息映射成 OpenAI 兼容的 `messages` 数组。
///
/// 三条规则：
/// 1. `Thinking` 块**不回灌**——它是我方展示用的，回灌会让部分厂商报错。
/// 2. `Tool` 角色的一条消息可能含多个结果块，这里**摊平成多条** `role=tool` 消息
///    （OpenAI 要求一次工具结果一条消息）。
/// 3. 助手消息的文本与工具调用**合并**进同一条消息（协议要求）。
#[must_use]
pub fn to_openai_messages(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for message in messages {
        if message.role == Role::Tool {
            for block in &message.blocks {
                if let ContentBlock::ToolResult {
                    call_id, content, ..
                } = block
                {
                    out.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content,
                    }));
                }
            }
            continue;
        }

        let mut text = String::new();
        let mut calls = Vec::new();
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text: chunk } => text.push_str(chunk),
                ContentBlock::Thinking { .. } | ContentBlock::ToolResult { .. } => {}
                ContentBlock::ToolUse { call } => calls.push(serde_json::json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments },
                })),
            }
        }

        let mut object = serde_json::Map::new();
        object.insert("role".to_owned(), serde_json::json!(message.role.as_str()));
        object.insert("content".to_owned(), serde_json::json!(text));
        if !calls.is_empty() {
            object.insert("tool_calls".to_owned(), serde_json::json!(calls));
        }
        out.push(serde_json::Value::Object(object));
    }
    out
}

/// 构造 HTTP 客户端。
///
/// `http_status_as_error(false)` 是刻意的：非 2xx 也当正常响应返回，
/// 这样能把服务端的错误正文读出来给用户看——否则只能拿到一个裸的 `HTTP 401`。
fn agent(timeout_secs: u64, stream: bool) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(std::time::Duration::from_secs(20)));
    // 流式请求不能设全局超时：长回答会读到一半被掐断。
    // 非流式给一个上限，防止服务端挂死。
    if !stream {
        builder = builder.timeout_global(Some(std::time::Duration::from_secs(timeout_secs.max(1))));
    }
    ureq::Agent::new_with_config(builder.build())
}

/// 从错误正文里捞出人话。厂商之间结构不一，尽力而为。
fn extract_error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        for pointer in ["/error/message", "/message", "/error"] {
            if let Some(found) = value.pointer(pointer).and_then(serde_json::Value::as_str) {
                return found.to_owned();
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty response body".to_owned()
    } else {
        trimmed.chars().take(400).collect()
    }
}

/// 把传输层错误归类。
fn map_transport(error: ureq::Error) -> AiError {
    match error {
        ureq::Error::StatusCode(code) => AiError::Protocol(format!("HTTP {code}")),
        other => AiError::Network(other.to_string()),
    }
}

/// 流式对话：阻塞直到流结束或取消。
///
/// `sink` 会收到 [`AiEvent::Started`]、若干增量、以及收尾的 `Finished`。
/// 出错时返回 `Err` **且**已经通过 `sink` 报过 `Failed`（若错误发生在流内）；
/// 连接阶段的错误只有 `Err`，因为那时还没有事件可言。
///
/// `cancel` 在每读完一行检查一次——取消的粒度是「一行 SSE」，不是「一个 token」。
/// 立即性来自调用方在线程栈 drop 时关闭底层连接。
pub fn stream_chat(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(AiEvent),
) -> Result<(), AiError> {
    profile.validate()?;
    if profile.needs_key() && api_key.trim().is_empty() {
        return Err(AiError::Config("API key is empty".into()));
    }

    let url = chat_url(profile);
    let body = build_body(&request.model, request, true);
    let payload = serde_json::to_vec(&body)
        .map_err(|error| AiError::Protocol(format!("request encode failed: {error}")))?;
    let agent = agent(timeout_secs, true);
    let mut response = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", api_key.trim()))
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

    sink(AiEvent::Started);

    let mut accumulator = StreamAccumulator::new();
    let reader = response.body_mut().as_reader();
    for line in BufReader::new(reader).lines() {
        if cancel.load(Ordering::Relaxed) {
            return Err(AiError::Cancelled);
        }
        let line = line.map_err(|error| AiError::Network(error.to_string()))?;
        match parse_line(&line) {
            SseLine::Skip => {}
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

/// 非流式便捷方法：只取正文。
///
/// 给 `duck-db` 的 text-to-SQL 用（它只要一条 SQL，不需要逐字）。是
/// [`stream_chat`] 能力的**子集**，因此不必两套 HTTP 代码。
pub fn complete(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
) -> Result<String, AiError> {
    profile.validate()?;
    if profile.needs_key() && api_key.trim().is_empty() {
        return Err(AiError::Config("API key is empty".into()));
    }

    let url = chat_url(profile);
    let body = build_body(&request.model, request, false);
    let payload = serde_json::to_vec(&body)
        .map_err(|error| AiError::Protocol(format!("request encode failed: {error}")))?;
    let agent = agent(timeout_secs, false);
    let mut response = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", api_key.trim()))
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

    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| AiError::Protocol(format!("response parse failed: {error}")))?;
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if content.is_empty() {
        return Err(AiError::Protocol("response has no message content".into()));
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_unique_and_lookup_works() {
        let mut ids: Vec<&str> = TEMPLATES.iter().map(|entry| entry.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "template ids must be unique");
        assert_eq!(
            template("deepseek").map(|entry| entry.name),
            Some("DeepSeek")
        );
        assert!(template("nope").is_none());
    }

    #[test]
    fn native_protocol_templates_are_flagged_unavailable() {
        assert!(template("openai").unwrap().is_available());
        assert!(!template("anthropic").unwrap().is_available());
        assert!(!template("gemini").unwrap().is_available());
    }

    #[test]
    fn profile_from_template_fills_defaults() {
        let profile = ProviderProfile::from_template(template("ollama").unwrap(), "p1");
        assert_eq!(profile.base_url, "http://127.0.0.1:11434/v1");
        assert!(profile.is_local);
        assert!(!profile.needs_key());
        assert_eq!(profile.model, "llama3.1");
        assert_eq!(
            chat_url(&profile),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn trailing_slash_is_normalised() {
        let profile = ProviderProfile {
            id: "p".into(),
            template_id: "openai".into(),
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1/".into(),
            model: "gpt-4o-mini".into(),
            is_local: false,
            api_key: None,
        };
        assert_eq!(
            chat_url(&profile),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn validate_rejects_empty_fields() {
        let mut profile = ProviderProfile::from_template(template("openai").unwrap(), "p");
        profile.base_url = "  ".into();
        assert!(matches!(profile.validate(), Err(AiError::Config(_))));
        profile.base_url = "https://api.openai.com/v1".into();
        profile.model = String::new();
        assert!(matches!(profile.validate(), Err(AiError::Config(_))));
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
        let wire = to_openai_messages(&[message]);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["content"], "answer");
        assert!(wire[0].get("tool_calls").is_none());
    }

    #[test]
    fn tool_results_are_flattened_into_individual_messages() {
        let mut message = ChatMessage {
            role: Role::Tool,
            blocks: Vec::new(),
        };
        message.blocks.push(ContentBlock::ToolResult {
            call_id: "a".into(),
            name: "read_file".into(),
            content: "one".into(),
            is_error: false,
        });
        message.blocks.push(ContentBlock::ToolResult {
            call_id: "b".into(),
            name: "list_dir".into(),
            content: "two".into(),
            is_error: true,
        });
        let wire = to_openai_messages(&[message]);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0]["role"], "tool");
        assert_eq!(wire[0]["tool_call_id"], "a");
        assert_eq!(wire[1]["tool_call_id"], "b");
    }

    #[test]
    fn assistant_tool_calls_are_merged_into_one_message() {
        let message = ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "let me look".into(),
                },
                ContentBlock::ToolUse {
                    call: crate::wire::ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        arguments: "{\"path\":\"a\"}".into(),
                    },
                },
            ],
        };
        let wire = to_openai_messages(&[message]);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["content"], "let me look");
        assert_eq!(wire[0]["tool_calls"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn request_body_carries_options() {
        let mut request = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        request.temperature = Some(0.3);
        request.max_tokens = Some(512);
        let body = build_body("m", &request, true);
        assert_eq!(body["stream"], true);
        // `f32 → f64` 会带出二进制误差，按浮点比较而不是位相等。
        let temperature = body["temperature"]
            .as_f64()
            .expect("temperature is a number");
        assert!((temperature - 0.3).abs() < 1e-6, "got {temperature}");
        assert_eq!(body["max_tokens"], 512);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn error_body_extraction_handles_both_shapes() {
        assert_eq!(
            extract_error_message("{\"error\":{\"message\":\"bad key\"}}"),
            "bad key"
        );
        assert_eq!(extract_error_message("{\"error\":\"nope\"}"), "nope");
        assert_eq!(extract_error_message("  "), "empty response body");
    }
}
