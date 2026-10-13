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
use std::time::Duration;

use crate::AiError;
use crate::sse::{SseLine, StreamAccumulator, parse_line};
use crate::wire::{AiEvent, ChatMessage, ContentBlock, Role, Usage};

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
    /// 本模板在当前版本是否可用。
    ///
    /// 不可用的模板**不会出现在模板下拉里**（`settings.rs` 按这个过滤）：能选中、
    /// 能保存、却在第一次发请求才失败，正是设计文档点名不许的「静默失败」。
    /// Anthropic 原生协议已接入（见 [`crate::anthropic`]），Gemini 仍未做。
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self.protocol, Protocol::OpenAi | Protocol::Anthropic)
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

/// 一条档案实际使用的 wire 协议族。
///
/// **由 `template_id` 推导，而不是在档案里存一个字段**：`ProviderProfile` 的字面量
/// 构造点横跨三个 crate（`duck-app` / `duck-db` / 内核测试），加字段要全部同步，
/// 而协议本来就由模板决定——自定义模板（`openai_compatible`）与未知 id 一律按
/// OpenAI 兼容处理。
#[must_use]
pub fn protocol_of(profile: &ProviderProfile) -> Protocol {
    template(&profile.template_id).map_or(Protocol::OpenAi, |entry| entry.protocol)
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

/// 拼 `models` 的完整 URL（OpenAI 兼容的模型清单端点）。
#[must_use]
pub fn models_url(profile: &ProviderProfile) -> String {
    format!("{}/models", profile.base_url.trim_end_matches('/'))
}

/// 从 `/models` 的响应体里挑出模型 id —— **纯函数**，不碰网络。
///
/// 兼容两种常见形状：
///
/// - OpenAI 系（绝大多数厂商，含 Ollama / vLLM 的兼容端点）：`{"data":[{"id":"gpt-4o"},…]}`
/// - 少数自建网关：`{"models":[{"name":"llama3.1"},…]}`
///
/// 顺序**照服务端给的顺序**（新模型通常排在前面），只去重、去空串 ——
/// 自己再排一次序会把「厂商认为的常用顺序」丢掉，用户反而更难挑。
pub fn parse_models(body: &str) -> Result<Vec<String>, AiError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| AiError::Protocol(format!("response parse failed: {error}")))?;
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| AiError::Protocol("response has no model list".into()))?;
    let mut models: Vec<String> = Vec::new();
    for entry in entries {
        let id = entry
            .get("id")
            .or_else(|| entry.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        if id.is_empty() || models.iter().any(|known| known == id) {
            continue;
        }
        models.push(id.to_owned());
    }
    Ok(models)
}

/// 拉取厂商当前可用的模型清单（`GET {base_url}/models`，一次**阻塞**请求）。
///
/// 模板里那份 [`ProviderTemplate::default_models`] 只能当**起点**：新模型上线、
/// 旧模型下线、自建网关只挂了自己那几路 —— 让用户凭记忆填模型名，很容易填出一个
/// 服务端不存在的名字，而那要等到第一次对话才报错。这里直接问服务端要权威清单。
///
/// **不要求已选模型**（与 [`ProviderProfile::validate`] 不同）：打开编辑器、
/// 还没挑模型时就该能拉清单 —— 清单正是用来挑模型的。只要求端点非空、
/// 非本地端点有密钥。
pub fn list_models(
    profile: &ProviderProfile,
    api_key: &str,
    timeout_secs: u64,
) -> Result<Vec<String>, AiError> {
    if profile.base_url.trim().is_empty() {
        return Err(AiError::Config("base_url is empty".into()));
    }
    if profile.needs_key() && api_key.trim().is_empty() {
        return Err(AiError::Config("API key is empty".into()));
    }

    let agent = agent(timeout_secs, false);
    let mut response = agent
        .get(&models_url(profile))
        .header("Authorization", &format!("Bearer {}", api_key.trim()))
        .call()
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
    parse_models(&text)
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
pub(crate) fn agent(timeout_secs: u64, stream: bool) -> ureq::Agent {
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
pub(crate) fn extract_error_message(body: &str) -> String {
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
pub(crate) fn map_transport(error: ureq::Error) -> AiError {
    match error {
        ureq::Error::StatusCode(code) => AiError::Protocol(format!("HTTP {code}")),
        other => AiError::Network(other.to_string()),
    }
}

// ── 重试与指数退避 ────────────────────────────────────────────────────

/// 一次请求失败后的重试策略。
///
/// 设计取舍（见 `docs/ai-design.md` 与 `temp/deepseek-harness/packages/llm/llm-retry`）：
///
/// - **只重试「重试会改变结果」的错误**：网络失败、`429`、`5xx`。
///   `4xx` 里的参数错误（401 / 400 / 404…）再试一万次还是同样的错，
///   只会把「秒失败」拖成「等一分钟后同样失败」，所以一律不重试。
/// - **次数与单次等待都有上限**：避免服务端持续 5xx 时把界面卡成假死。
/// - **退避期间的取消要生效**：等待拆成小片，每片检查一次取消标志
///   （见 [`RetryPolicy::delay_for`] 与 [`wait_backoff`]）。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 最多重试几次（不含首次请求）。`0` = 不重试。
    pub max_retries: u32,
    /// 首次重试前的等待（之后翻倍）。
    pub initial_delay: Duration,
    /// 单次等待的上限（指数增长封顶在这里）。
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// 不重试。
    #[must_use]
    pub const fn none() -> Self {
        Self {
            max_retries: 0,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    /// 第 `attempt` 次重试（**1-based**）前应当等待的时长：指数退避，封顶在 `max_delay`。
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        // 位移封顶，避免 `1 << n` 在极大尝试次数上溢出。
        let shift = (attempt - 1).min(16);
        let base = u64::try_from(self.initial_delay.as_millis()).unwrap_or(u64::MAX);
        let scaled = base.saturating_mul(1u64 << shift);
        Duration::from_millis(scaled).min(self.max_delay)
    }
}

/// 该错误是否值得重试。
///
/// 只有**传输层**的暂时性故障算数：网络读写失败（含连接被拒、TLS、超时），
/// 以及服务端侧的 `429` / `5xx`。协议解析失败、配置不全、取消、存储失败
/// 都不重试——它们要么不会因重试而好转，要么重试本身就是错的。
#[must_use]
pub fn is_retryable(error: &AiError) -> bool {
    match error {
        AiError::Network(_) => true,
        AiError::Protocol(detail) => {
            http_status_of(detail).is_some_and(|code| code == 429 || (500..600).contains(&code))
        }
        AiError::Config(_)
        | AiError::Storage(_)
        | AiError::Secret(_)
        | AiError::Cancelled
        | AiError::Unsupported(_) => false,
    }
}

/// 从协议错误的文案里抠出 HTTP 状态码（`"HTTP 503: …"` / `"HTTP 429"`）。
fn http_status_of(detail: &str) -> Option<u16> {
    let rest = detail.strip_prefix("HTTP ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// 退避等待，期间可被取消打断。返回 `false` = 等待途中被取消。
///
/// **不整段 `sleep`**：拆成 50ms 的小片逐片检查取消标志。否则用户点了「停止」
/// 要等满整个退避窗口（默认最长 8s）才看到反应——那正是「睡死」。
fn wait_backoff(delay: Duration, cancel: &AtomicBool) -> bool {
    const SLICE: Duration = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < delay {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let step = SLICE.min(delay - slept);
        std::thread::sleep(step);
        slept += step;
    }
    !cancel.load(Ordering::Relaxed)
}

/// 流式对话：阻塞直到流结束或取消。
///
/// `sink` 会收到 [`AiEvent::Started`]、若干增量、以及收尾的 `Finished`。
/// 出错时返回 `Err` **且**已经通过 `sink` 报过 `Failed`（若错误发生在流内）；
/// 连接阶段的错误只有 `Err`，因为那时还没有事件可言。
///
/// `cancel` 在每读完一行检查一次——取消的粒度是「一行 SSE」，不是「一个 token」。
/// 立即性来自调用方在线程栈 drop 时关闭底层连接。
///
/// 用默认 [`RetryPolicy`] 重试可重试错误；需要自定义策略请用
/// [`stream_chat_with_retry`]。
pub fn stream_chat(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(AiEvent),
) -> Result<(), AiError> {
    stream_chat_with_retry(
        profile,
        api_key,
        request,
        timeout_secs,
        &RetryPolicy::default(),
        cancel,
        sink,
    )
}

/// 同 [`stream_chat`]，但由调用方指定重试策略。
///
/// **流式的重试边界**：只在**尚未向 `sink` 吐过任何事件**时重试。
/// 一旦吐过内容（含 `Started`），再重试就等于把已经显示出来的正文从头再说一遍——
/// 界面会出现重复段落，用户会以为模型在胡言乱语。所以：
///
/// - 连接失败、`429` / `5xx`（都发生在 `Started` 之前）→ 可重试；
/// - 流到一半的读失败 / 服务端中途断流 → **不重试**，原样报错。
///
/// 这条边界靠 `stream_chat_once` 回传的 `committed` 标志判定，不靠猜错误类型。
pub fn stream_chat_with_retry(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
    policy: &RetryPolicy,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(AiEvent),
) -> Result<(), AiError> {
    // 配置类的前置校验放在重试之外：它们不属于「重试可改变结果」的错误。
    profile.validate()?;
    if profile.needs_key() && api_key.trim().is_empty() {
        return Err(AiError::Config("API key is empty".into()));
    }

    let mut attempt: u32 = 0;
    loop {
        // 每次尝试独立记「是否已经吐过内容」：上一次尝试若吐过就不会走到这里。
        let mut committed = false;
        match stream_chat_once(
            profile,
            api_key,
            request,
            timeout_secs,
            cancel,
            &mut *sink,
            &mut committed,
        ) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if committed || !is_retryable(&error) || attempt >= policy.max_retries {
                    return Err(error);
                }
                attempt += 1;
                if !wait_backoff(policy.delay_for(attempt), cancel) {
                    return Err(AiError::Cancelled);
                }
            }
        }
    }
}

/// 单次流式尝试（不含重试）。`committed` 在第一次向 `sink` 发事件时置位。
fn stream_chat_once(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(AiEvent),
    committed: &mut bool,
) -> Result<(), AiError> {
    // 协议分派：Anthropic 走原生 Messages API；Gemini 尚未实现，**明确报错**而不是
    // 静默按 OpenAI 兼容发出去——后者会在服务端撞一个看不懂的 400/404，用户
    // 只会看到「请求失败」，无从判断是配置问题还是我们发错了协议。
    match protocol_of(profile) {
        Protocol::Anthropic => {
            return crate::anthropic::stream_once(
                profile,
                api_key,
                request,
                timeout_secs,
                cancel,
                sink,
                committed,
            );
        }
        Protocol::Gemini => {
            return Err(AiError::Unsupported(
                "Gemini native protocol is not implemented".into(),
            ));
        }
        Protocol::OpenAi => {}
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

    // 到这里为止都还没吐过任何东西 —— 期间失败（连接被拒 / 429 / 5xx）都可安全重试。
    *committed = true;
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
    complete_with_usage(profile, api_key, request, timeout_secs).map(|(content, _usage)| content)
}

/// 非流式便捷方法：正文 + 用量。
///
/// `duck-db` 的 text-to-SQL 要在界面上显示本次消耗的 token，所以它是
/// [`complete`] 的**超集**；不额外写一套 HTTP 代码。
///
/// 用默认 [`RetryPolicy`] 重试可重试错误。
/// ⚠️ 这条路径**没有取消句柄**（调用方 `duck-db` 也没传），所以退避等待期间
/// 无法被打断。默认策略下最坏总等待约 3.5s（0.5 + 1 + 2s），不会长到像假死。
pub fn complete_with_usage(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
) -> Result<(String, Usage), AiError> {
    complete_with_usage_with_retry(
        profile,
        api_key,
        request,
        timeout_secs,
        &RetryPolicy::default(),
    )
}

/// 同 [`complete_with_usage`]，但由调用方指定重试策略。
///
/// 非流式没有「吐了一半」的问题：整份响应要么完整拿到，要么就是失败，
/// 所以任何可重试错误都能安全重来。
pub fn complete_with_usage_with_retry(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
    policy: &RetryPolicy,
) -> Result<(String, Usage), AiError> {
    profile.validate()?;
    if profile.needs_key() && api_key.trim().is_empty() {
        return Err(AiError::Config("API key is empty".into()));
    }

    let mut attempt: u32 = 0;
    loop {
        match complete_with_usage_once(profile, api_key, request, timeout_secs) {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !is_retryable(&error) || attempt >= policy.max_retries {
                    return Err(error);
                }
                attempt += 1;
                std::thread::sleep(policy.delay_for(attempt));
            }
        }
    }
}

/// 单次非流式请求（不含重试）。
fn complete_with_usage_once(
    profile: &ProviderProfile,
    api_key: &str,
    request: &ChatRequest,
    timeout_secs: u64,
) -> Result<(String, Usage), AiError> {
    match protocol_of(profile) {
        Protocol::Anthropic => {
            return crate::anthropic::complete_once(profile, api_key, request, timeout_secs);
        }
        Protocol::Gemini => {
            return Err(AiError::Unsupported(
                "Gemini native protocol is not implemented".into(),
            ));
        }
        Protocol::OpenAi => {}
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
    let read_tokens = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_u64)
            .and_then(|tokens| u32::try_from(tokens).ok())
            .unwrap_or_default()
    };
    let usage = Usage {
        input_tokens: read_tokens("/usage/prompt_tokens"),
        output_tokens: read_tokens("/usage/completion_tokens"),
        total_tokens: read_tokens("/usage/total_tokens"),
    };
    Ok((content, usage))
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
    fn availability_matches_implemented_protocols() {
        assert!(template("openai").unwrap().is_available());
        // Anthropic 原生协议已接入（见 `anthropic` 模块），模板必须可选中 ——
        // 网关灰显之所以存在，是因为「能选中却发不出去」比不给选更糟。
        assert!(template("anthropic").unwrap().is_available());
        // Gemini 仍未实现：不许出现在下拉里。
        assert!(!template("gemini").unwrap().is_available());
    }

    #[test]
    fn protocol_is_derived_from_the_template() {
        let mut profile = ProviderProfile::from_template(template("anthropic").unwrap(), "p");
        assert_eq!(protocol_of(&profile), Protocol::Anthropic);
        profile.template_id = "openai".into();
        assert_eq!(protocol_of(&profile), Protocol::OpenAi);
        // 自定义与未知模板一律按 OpenAI 兼容（不引入一个新的失败模式）。
        profile.template_id = "openai_compatible".into();
        assert_eq!(protocol_of(&profile), Protocol::OpenAi);
        profile.template_id = "not-a-template".into();
        assert_eq!(protocol_of(&profile), Protocol::OpenAi);
    }

    #[test]
    fn model_list_parsing_accepts_both_shapes() {
        // OpenAI 系：`data[].id`。
        assert_eq!(
            parse_models(r#"{"data":[{"id":"gpt-4o"},{"id":"gpt-4o-mini"}]}"#).unwrap(),
            vec!["gpt-4o".to_owned(), "gpt-4o-mini".to_owned()]
        );
        // 少数自建网关：`models[].name`。
        assert_eq!(
            parse_models(r#"{"models":[{"name":"llama3.1"},{"name":"qwen2.5"}]}"#).unwrap(),
            vec!["llama3.1".to_owned(), "qwen2.5".to_owned()]
        );
    }

    #[test]
    fn model_list_parsing_keeps_server_order_and_drops_dupes() {
        // 顺序照服务端给的（新模型通常在前），不自己重排。
        assert_eq!(
            parse_models(r#"{"data":[{"id":"b"},{"id":"a"},{"id":"b"},{"id":"  "},{"id":""}]}"#)
                .unwrap(),
            vec!["b".to_owned(), "a".to_owned()],
            "去重、去空串，但保留服务端顺序"
        );
    }

    #[test]
    fn model_list_parsing_rejects_garbage() {
        assert!(parse_models("not json").is_err());
        // 合法的 JSON 但没有清单字段：不能当成「零个模型」静默通过 ——
        // 那会让界面显示一个空的下拉框，用户以为服务端没有模型。
        assert!(parse_models(r#"{"object":"list"}"#).is_err());
    }

    #[test]
    fn models_url_has_no_double_slash() {
        let mut profile = stub_profile("http://127.0.0.1:1/v1/");
        assert_eq!(models_url(&profile), "http://127.0.0.1:1/v1/models");
        profile.base_url = "http://127.0.0.1:1/v1".to_owned();
        assert_eq!(models_url(&profile), "http://127.0.0.1:1/v1/models");
    }

    #[test]
    fn list_models_reads_the_provider_catalogue() {
        let stub = stub_server(
            vec![Reply::ok(r#"{"data":[{"id":"x-1"},{"id":"x-2"}]}"#)],
            false,
        );
        let models = list_models(&stub_profile(&stub.base_url), "", 5).expect("应当拉到清单");
        assert_eq!(models, vec!["x-1".to_owned(), "x-2".to_owned()]);
        assert_eq!(*stub.requests.lock().unwrap(), 1);
    }

    #[test]
    fn list_models_surfaces_http_errors() {
        let stub = stub_server(vec![Reply::status(401)], false);
        let error = list_models(&stub_profile(&stub.base_url), "", 5).expect_err("401 应当报错");
        assert!(
            error.to_string().contains("401"),
            "错误里要带状态码，界面才好告诉用户是密钥问题：{error}"
        );
    }

    #[test]
    fn list_models_needs_a_key_only_for_remote_endpoints() {
        // 本地端点（Ollama）免 key。
        let mut local = stub_profile("http://127.0.0.1:1/v1");
        local.is_local = true;
        assert!(list_models(&local, "", 5).is_err(), "连不上应当报 Network");

        // 远端端点没给密钥：本地就拦下，不必花一次往返。
        let mut remote = stub_profile("http://127.0.0.1:1/v1");
        remote.is_local = false;
        let error = list_models(&remote, "  ", 5).expect_err("缺密钥应当拒绝");
        assert!(
            matches!(error, AiError::Config(_)),
            "应当是配置错误：{error}"
        );

        // 端点为空同理。
        let mut empty = stub_profile("   ");
        empty.is_local = true;
        assert!(matches!(
            list_models(&empty, "", 5).expect_err("空端点应当拒绝"),
            AiError::Config(_)
        ));
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

    // ── 重试与指数退避 ────────────────────────────────────────────────

    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::Mutex;

    /// 一条给 stub 用的固定响应。`declared_len` 与 `body.len()` 故意解耦，
    /// 用来制造「`Content-Length` 说 999 字节，实际只给几十字节就断」的中途截断。
    struct Reply {
        status: u16,
        declared_len: usize,
        body: String,
    }

    impl Reply {
        fn ok(body: &str) -> Self {
            Self {
                status: 200,
                declared_len: body.len(),
                body: body.to_owned(),
            }
        }

        fn status(code: u16) -> Self {
            let body = format!("{{\"error\":{{\"message\":\"boom {code}\"}}}}");
            Self {
                status: code,
                declared_len: body.len(),
                body,
            }
        }

        fn truncated(body: &str, declared_len: usize) -> Self {
            Self {
                status: 200,
                declared_len,
                body: body.to_owned(),
            }
        }
    }

    /// 本机 stub：按顺序回 `replies`；`repeat` 为真时把最后一条重复到永远。
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
                    reply.status, reply.declared_len
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

    /// 把请求读干净（headers + `Content-Length` 指定的 body），避免客户端吃 RST。
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

    fn stub_profile(base_url: &str) -> ProviderProfile {
        ProviderProfile {
            id: "stub".into(),
            template_id: "openai_compatible".into(),
            name: "stub".into(),
            base_url: base_url.to_owned(),
            model: "stub".into(),
            is_local: true,
            api_key: None,
        }
    }

    /// 极短退避：让重试测试跑得快，同时不影响被测逻辑。
    fn fast_policy(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        }
    }

    fn sse_text(text: &str) -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices": [{"delta": {"content": text}}]})
        )
    }

    fn user_request() -> ChatRequest {
        ChatRequest::new("m", vec![ChatMessage::user("hi")])
    }

    #[test]
    fn retry_backoff_grows_then_caps() {
        let policy = RetryPolicy {
            max_retries: 5,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
        };
        assert_eq!(policy.delay_for(0), Duration::ZERO);
        assert_eq!(policy.delay_for(1), Duration::from_millis(100));
        assert_eq!(policy.delay_for(2), Duration::from_millis(200));
        assert_eq!(policy.delay_for(3), Duration::from_millis(400));
        // 封顶：指数继续涨也不能超过 `max_delay`。
        assert_eq!(policy.delay_for(4), Duration::from_millis(500));
        assert_eq!(policy.delay_for(999), Duration::from_millis(500));
        // `none()` 就是不重试。
        assert_eq!(RetryPolicy::none().max_retries, 0);
    }

    #[test]
    fn only_transient_failures_are_retryable() {
        assert!(is_retryable(&AiError::Network("reset".into())));
        assert!(is_retryable(&AiError::Protocol(
            "HTTP 429: slow down".into()
        )));
        assert!(is_retryable(&AiError::Protocol(
            "HTTP 503: unavailable".into()
        )));
        // 参数类错误重试也不会成功，只会把「秒失败」拖成「慢失败」。
        assert!(!is_retryable(&AiError::Protocol(
            "HTTP 400: bad request".into()
        )));
        assert!(!is_retryable(&AiError::Protocol(
            "HTTP 401: bad key".into()
        )));
        assert!(!is_retryable(&AiError::Protocol(
            "HTTP 404: no model".into()
        )));
        assert!(!is_retryable(&AiError::Config("no key".into())));
        assert!(!is_retryable(&AiError::Cancelled));
        // 没有 HTTP 前缀的协议错误（解析失败 / 流里带 error）不是暂时性的。
        assert!(!is_retryable(&AiError::Protocol(
            "response parse failed".into()
        )));
    }

    #[test]
    fn streaming_retries_transient_http_errors_before_output() {
        let stub = stub_server(
            vec![
                Reply::status(429),
                Reply::status(503),
                Reply::ok(&sse_text("ok")),
            ],
            false,
        );
        let mut events = Vec::new();
        let result = stream_chat_with_retry(
            &stub_profile(&stub.base_url),
            "k",
            &user_request(),
            5,
            &fast_policy(3),
            &AtomicBool::new(false),
            &mut |event| events.push(event),
        );
        assert!(result.is_ok(), "{result:?}");
        // 两次失败各重试一次，第三次成功。
        assert_eq!(*stub.requests.lock().unwrap(), 3);
        // 重试成功后照样从头走完流。
        assert!(matches!(events.first(), Some(AiEvent::Started)));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::TextDelta(t) if t == "ok"))
        );
    }

    #[test]
    fn streaming_does_not_retry_parameter_errors() {
        let stub = stub_server(
            vec![Reply::status(400), Reply::ok(&sse_text("unreachable"))],
            false,
        );
        let result = stream_chat_with_retry(
            &stub_profile(&stub.base_url),
            "k",
            &user_request(),
            5,
            &fast_policy(3),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert!(matches!(result, Err(AiError::Protocol(_))));
        // 只发了一次：4xx 不重试。
        assert_eq!(*stub.requests.lock().unwrap(), 1, "4xx 不该重试");
    }

    #[test]
    fn streaming_does_not_retry_after_output_has_started() {
        // 先给一条正文增量，然后 `Content-Length` 声称 999 字节却只给这一行就断。
        // 读到断点时「已是 Network 错误、且已经吐过内容」——必须**不重试**，
        // 否则界面会把 "half" 再吐一遍。
        let partial = format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": "half"}}]})
        );
        let stub = stub_server(vec![Reply::truncated(&partial, 999)], true);
        let mut events = Vec::new();
        let result = stream_chat_with_retry(
            &stub_profile(&stub.base_url),
            "k",
            &user_request(),
            5,
            &fast_policy(3),
            &AtomicBool::new(false),
            &mut |event| events.push(event),
        );
        assert!(result.is_err(), "中途断流应当报错");
        assert_eq!(*stub.requests.lock().unwrap(), 1, "吐过内容后不该重试");
        // 而且那段内容只被吐了一次。
        let halves = events
            .iter()
            .filter(|event| matches!(event, AiEvent::TextDelta(t) if t == "half"))
            .count();
        assert_eq!(halves, 1);
    }

    #[test]
    fn streaming_can_be_cancelled_during_backoff() {
        let stub = stub_server(vec![Reply::status(503)], true);
        let flag = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            trigger.store(true, Ordering::Relaxed);
        });
        // 大退避窗口：不用取消就该等 200ms 才继续。取消应当把它打断。
        let policy = RetryPolicy {
            max_retries: 5,
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(2),
        };
        let started = std::time::Instant::now();
        let result = stream_chat_with_retry(
            &stub_profile(&stub.base_url),
            "k",
            &user_request(),
            5,
            &policy,
            flag.as_ref(),
            &mut |_| {},
        );
        assert!(matches!(result, Err(AiError::Cancelled)));
        // 远小于第一个退避窗口（200ms）：说明等待被拆成了可中断的小片。
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "取消没能打断退避等待：{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn non_streaming_retries_transient_errors() {
        let stub = stub_server(
            vec![
                Reply::status(500),
                Reply::ok("{\"choices\":[{\"message\":{\"content\":\"sql\"}}]}"),
            ],
            false,
        );
        let (content, _usage) = complete_with_usage_with_retry(
            &stub_profile(&stub.base_url),
            "k",
            &user_request(),
            5,
            &fast_policy(2),
        )
        .expect("非流式应当重试 5xx");
        assert_eq!(content, "sql");
        assert_eq!(*stub.requests.lock().unwrap(), 2);
    }
}
