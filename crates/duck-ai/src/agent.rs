//! 一轮问答的驱动与取消句柄。
//!
//! 本模块是内核与 UI 之间唯一的「动词」层：把一份完整对话喂进来，起一次流式请求，
//! 把增量事件交给调用方的 `sink`，最后交回拼装好的助手消息。
//!
//! **线程归属**：本模块**自己不开线程**。`run_turn` 是同步阻塞的，
//! 调用方（`duck-app` 的助手页）负责把它丢进专用 `std::thread`，
//! 并把 `sink` 里的事件经 `mpsc` 回传主线程。这样内核保持可单测，
//! 也不会占住 gpui 的共享后台执行器。
//!
//! **取消**：`RunHandle` 是一个 `Arc<AtomicBool>` 的薄包装。`stream_chat`
//! 每读完一行 SSE 检查一次，置位即返回 [`crate::AiError::Cancelled`]；
//! 底层连接随线程栈 drop 关闭，因此停止是「立刻」而不是「等这一轮说完」。
//!
//! **工具循环**：见 [`run_agent`]。内核自己驱动多轮（发请求 → 执行工具 → 回灌结果 →
//! 再发请求），上层只需要给一个注册表 + 一个审批回调。把循环放在内核而不是 UI，
//! 是因为「何时该回灌、回灌什么形状」纯属协议细节，UI 不该知道。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::AiError;
use crate::provider::{ChatRequest, ProviderProfile, stream_chat};
use crate::tool::{
    ToolDetail, ToolOutcome, ToolRegistry, ToolRisk, TruncationTier, truncate_tiered,
};
use crate::wire::{AiEvent, ChatMessage, ContentBlock, StopReason, ToolCall, Usage};

/// 一次问答的生成参数。
#[derive(Debug, Clone, Copy)]
pub struct ChatOptions {
    /// 采样温度；`None` = 用服务端默认。
    pub temperature: Option<f32>,
    /// 最大输出 token；`None` = 用服务端默认。
    pub max_tokens: Option<u32>,
    /// 单次请求的网络超时（秒）。流式只作用于连接阶段。
    pub timeout_secs: u64,
}

impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            temperature: None,
            max_tokens: None,
            timeout_secs: 300,
        }
    }
}

/// 取消句柄。可克隆，克隆体共享同一个标志位。
#[derive(Debug, Clone, Default)]
pub struct RunHandle {
    cancel: Arc<AtomicBool>,
}

impl RunHandle {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求取消。幂等。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 是否已被取消。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 取底层标志位，喂给 [`stream_chat`]。
    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }
}

/// 一轮问答的产出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// 拼装好的助手消息（含思考块与工具调用块）。
    pub message: ChatMessage,
    /// 服务端报的用量（拿不到时为全 0）。
    pub usage: Usage,
    /// 结束原因。
    pub stop_reason: StopReason,
}

/// 把增量事件拼成一条助手消息。
#[derive(Debug, Default)]
struct AssistantBuilder {
    text: String,
    thinking: String,
    calls: Vec<ToolCall>,
}

impl AssistantBuilder {
    fn absorb(&mut self, event: &AiEvent) {
        match event {
            AiEvent::TextDelta(chunk) => self.text.push_str(chunk),
            AiEvent::ThinkingDelta(chunk) => self.thinking.push_str(chunk),
            AiEvent::ToolCalls(calls) => self.calls = calls.clone(),
            AiEvent::Started | AiEvent::Finished { .. } | AiEvent::Failed(_) => {}
        }
    }

    /// 拼成消息。块顺序固定为「思考 → 正文 → 工具调用」：
    /// 界面按这个顺序渲染，落库后回读也保持同一观感。
    fn into_message(self) -> ChatMessage {
        let mut blocks = Vec::new();
        if !self.thinking.is_empty() {
            blocks.push(ContentBlock::Thinking {
                text: self.thinking,
            });
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text { text: self.text });
        }
        for call in self.calls {
            blocks.push(ContentBlock::ToolUse { call });
        }
        ChatMessage {
            role: crate::wire::Role::Assistant,
            blocks,
        }
    }
}

/// 跑一轮问答：发请求 → 流式回调 → 交回助手消息。
///
/// `messages` 是**完整**对话历史（含 system 与之前的工具结果），内核不做裁剪——
/// 上下文窗口管理是上层的事，内核不猜用户的意图。
///
/// 不声明工具的单轮调用。需要工具循环请用 [`run_agent`]。
pub fn run_turn(
    profile: &ProviderProfile,
    api_key: &str,
    model: &str,
    messages: &[ChatMessage],
    options: &ChatOptions,
    handle: &RunHandle,
    sink: &mut dyn FnMut(AiEvent),
) -> Result<TurnOutcome, AiError> {
    run_turn_with(
        profile,
        api_key,
        model,
        messages,
        options,
        &[],
        handle,
        sink,
    )
}

/// 同 [`run_turn`]，额外下发工具声明（OpenAI `tools` 数组）。
///
/// `tools` 为空时**不往请求体里塞 `tools` 字段**：部分服务端见到空数组直接报参数错误。
// 参数多是因为它必须同时接收「谁在问、问哪一轮的上下文、怎么取消、事件往哪送」，
// 打包成结构体只会把同样的字段再抄一遍。同仓 `plugin_center.rs:1311` 同款处理。
#[allow(clippy::too_many_arguments)]
pub fn run_turn_with(
    profile: &ProviderProfile,
    api_key: &str,
    model: &str,
    messages: &[ChatMessage],
    options: &ChatOptions,
    tools: &[serde_json::Value],
    handle: &RunHandle,
    sink: &mut dyn FnMut(AiEvent),
) -> Result<TurnOutcome, AiError> {
    let mut request = ChatRequest::new(model, messages.to_vec());
    request.temperature = options.temperature;
    request.max_tokens = options.max_tokens;
    request.tools = tools.to_vec();

    let flag = handle.flag();
    let mut builder = AssistantBuilder::default();
    let mut usage = Usage::default();
    let mut stop_reason = StopReason::default();

    stream_chat(
        profile,
        api_key,
        &request,
        options.timeout_secs,
        &flag,
        &mut |event| {
            // 先累积再转发：`Finished` 里带着用量与结束原因，必须在这里截下来。
            if let AiEvent::Finished {
                stop_reason: reason,
                usage: used,
            } = &event
            {
                stop_reason = *reason;
                usage = *used;
            }
            builder.absorb(&event);
            sink(event);
        },
    )?;

    Ok(TurnOutcome {
        message: builder.into_message(),
        usage,
        stop_reason,
    })
}

// ── 工具循环 ──────────────────────────────────────────────────────────

/// 一次问答（可能含多轮工具调用）的驱动参数。
#[derive(Debug, Clone, Copy)]
pub struct AgentOptions {
    /// 生成参数（每轮共用）。
    pub chat: ChatOptions,
    /// 工具轮次上限。
    ///
    /// 兜底而非参数：模型偶发会陷入「调工具 → 看结果 → 再调同一个」的循环，
    /// 没有上限就是无限花钱。撞上限时**明确告知用户**，不静默停止（§4.6）。
    pub max_rounds: usize,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            chat: ChatOptions::default(),
            max_rounds: 8,
        }
    }
}

/// 审批闸门的裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// 放行。
    Run,
    /// 拒绝，`String` 是**回灌给模型的原因**（要写成人话，模型据此换方案）。
    Deny(String),
}

/// 工具循环的事件。
///
/// 流式增量被包了一层 [`AgentEvent::Stream`]，是为了让上层只需要订阅**一条**通道：
/// 否则工具卡片的「运行中 → 完成」与正文流式会落在两个队列里，顺序无法保证。
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// 模型侧的流式事件（原样透传）。
    Stream(AiEvent),
    /// 某个工具即将执行（`risk` 是本次入参判定出来的档位）。
    ToolStarted {
        round: usize,
        call: ToolCall,
        risk: ToolRisk,
    },
    /// 某个工具执行完毕。`preview` 已按 [`TruncationTier::Preview`] 截断，给卡片用。
    ToolFinished {
        round: usize,
        call: ToolCall,
        ok: bool,
        preview: String,
        detail: Option<ToolDetail>,
    },
    /// 审批被拒。**不执行**，只把原因回灌给模型。
    ToolDenied {
        round: usize,
        call: ToolCall,
        reason: String,
    },
    /// 一轮的**全部**消息已经定稿（助手消息 + 该轮的工具结果）。
    ///
    /// 这条事件是为界面加的：一次 `run_agent` 里可能跑好几轮，界面要在过程中就看到
    /// 前几轮的内容，不能等整轮结束才一起拿到。有了它，界面可以「收到就用」并顺手
    /// 清掉当前轮的流式缓冲，于是**取消时不需要任何重建**——屏幕上的内容与最终
    /// 落库的内容天然一致。
    RoundCompleted {
        round: usize,
        messages: Vec<ChatMessage>,
    },
    /// 撞到轮次上限，循环停止。上层应当把这件事显示出来。
    RoundsExhausted { max_rounds: usize },
}

/// 一次 [`run_agent`] 的产出。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// **本次新增**的消息（助手消息 + 工具结果），按发生顺序；
    /// 不含入参里的历史——上层直接 `extend` 到自己的消息列表即可。
    pub messages: Vec<ChatMessage>,
    /// 各轮用量之和。
    pub usage: Usage,
    /// 最后一轮的结束原因。
    pub stop_reason: StopReason,
    /// 实际跑了几轮。
    pub rounds: usize,
    /// 是否因取消而提前收尾。
    ///
    /// **取消不算失败**：已经跑完的轮次是真实发生过的对话，丢掉它们会让历史缺一段。
    /// 所以取消走 `Ok` + 这个标志，而不是 `Err`。只有「请求本身就失败了」才返回 `Err`。
    pub cancelled: bool,
}

/// 驱动一次问答，自带工具循环。
///
/// 一轮的形状：发请求 → 若模型要求调工具 → 逐个走审批闸门 → 执行 → 回灌结果 →
/// 再发一轮，直到模型不再要求工具、或撞到 [`AgentOptions::max_rounds`]。
///
/// **审批回调是唯一的用户交互点**。内核不弹窗、不知道「确认框」长什么样：
/// 上层可以在里面同步阻塞（真弹一个模态），也可以直接答 [`Approval::Run`]
/// （只读工具）。返回 [`Approval::Deny`] 时拒绝原因**会回灌给模型**——
/// 这是刻意的：模型知道「用户不同意」才会换个做法，否则它会一直重试。
#[allow(clippy::too_many_arguments)]
pub fn run_agent(
    profile: &ProviderProfile,
    api_key: &str,
    model: &str,
    history: &[ChatMessage],
    options: &AgentOptions,
    registry: &ToolRegistry,
    approve: &mut dyn FnMut(&ToolCall, ToolRisk) -> Approval,
    handle: &RunHandle,
    sink: &mut dyn FnMut(AgentEvent),
) -> Result<AgentOutcome, AiError> {
    let declarations = registry.declarations();
    let max_rounds = options.max_rounds.max(1);

    let mut conversation: Vec<ChatMessage> = history.to_vec();
    let mut produced: Vec<ChatMessage> = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason = StopReason::default();

    for round in 1..=max_rounds {
        let outcome = match run_turn_with(
            profile,
            api_key,
            model,
            &conversation,
            &options.chat,
            &declarations,
            handle,
            &mut |event| sink(AgentEvent::Stream(event)),
        ) {
            Ok(outcome) => outcome,
            // 取消发生在**流中间**：本轮的正文只到一半，不算一条消息，
            // 所以直接收尾（前面那些已完成的轮次已经通过 `RoundCompleted` 交出去了）。
            Err(AiError::Cancelled) => {
                return Ok(AgentOutcome {
                    messages: produced,
                    usage,
                    stop_reason,
                    rounds: round,
                    cancelled: true,
                });
            }
            Err(other) => return Err(other),
        };
        usage = merge_usage(usage, outcome.usage);
        stop_reason = outcome.stop_reason;

        let calls: Vec<ToolCall> = outcome.message.tool_calls().into_iter().cloned().collect();
        conversation.push(outcome.message.clone());
        produced.push(outcome.message.clone());
        // 本轮已定稿的消息。随 `RoundCompleted` 一起交出去，界面据此边跑边显示。
        let mut settled: Vec<ChatMessage> = vec![outcome.message];

        if calls.is_empty() {
            sink(AgentEvent::RoundCompleted {
                round,
                messages: settled,
            });
            return Ok(AgentOutcome {
                messages: produced,
                usage,
                stop_reason,
                rounds: round,
                cancelled: false,
            });
        }

        for (position, call) in calls.iter().enumerate() {
            if handle.is_cancelled() {
                // 取消恰好落在两个工具之间：**把剩下的调用补成失败结果**再收尾。
                // 协议要求每条 `tool_calls` 都有等量的 `tool` 结果消息，
                // 少一条的话这段历史下次被原样回灌时会被服务端判为非法请求。
                for pending in &calls[position..] {
                    let message = ChatMessage::tool_result(
                        pending.id.clone(),
                        pending.name.clone(),
                        "cancelled before execution",
                        true,
                    );
                    settled.push(message.clone());
                    produced.push(message);
                }
                sink(AgentEvent::RoundCompleted {
                    round,
                    messages: settled,
                });
                return Ok(AgentOutcome {
                    messages: produced,
                    usage,
                    stop_reason,
                    rounds: round,
                    cancelled: true,
                });
            }

            let call = call.clone();
            let risk = registry.risk_of(&call);
            sink(AgentEvent::ToolStarted {
                round,
                call: call.clone(),
                risk,
            });

            let outcome = match approve(&call, risk) {
                Approval::Run => registry.run(&call, &handle.flag()),
                Approval::Deny(reason) => {
                    sink(AgentEvent::ToolDenied {
                        round,
                        call: call.clone(),
                        reason: reason.clone(),
                    });
                    ToolOutcome::error(reason)
                }
            };

            sink(AgentEvent::ToolFinished {
                round,
                call: call.clone(),
                ok: outcome.ok,
                preview: truncate_tiered(&outcome.content, TruncationTier::Preview),
                detail: outcome.detail.clone(),
            });

            let message = ChatMessage::tool_result(
                call.id.clone(),
                call.name.clone(),
                outcome.content.clone(),
                !outcome.ok,
            );
            conversation.push(message.clone());
            produced.push(message.clone());
            settled.push(message);
        }

        sink(AgentEvent::RoundCompleted {
            round,
            messages: settled,
        });

        if handle.is_cancelled() {
            // 本轮已完整交出去（含全部工具结果），所以这里只是个干净的提前收尾。
            return Ok(AgentOutcome {
                messages: produced,
                usage,
                stop_reason,
                rounds: round,
                cancelled: true,
            });
        }
    }

    // 撞上限。本轮的每个工具**都已经执行完并回灌**了——这是刻意的：
    // 上面那条「每条 tool_calls 必须有配对的 tool 结果」既约束取消路径也约束这里，
    // 所以循环只在「本轮调用全部有结果」之后才可能退出。剩下要做的只是把
    // 「停在几个轮次」这件事说出去，让界面能显示出来，而不是看起来像说完了。
    sink(AgentEvent::RoundsExhausted { max_rounds });
    Ok(AgentOutcome {
        messages: produced,
        usage,
        stop_reason,
        rounds: max_rounds,
        cancelled: false,
    })
}

/// 累加两份用量。
fn merge_usage(left: Usage, right: Usage) -> Usage {
    Usage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderProfile;
    use crate::tool::{Tool, ToolOutcome, ToolRisk, ToolSpec};
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn handle_cancel_is_shared_between_clones() {
        let handle = RunHandle::new();
        let clone = handle.clone();
        assert!(!handle.is_cancelled());
        clone.cancel();
        assert!(handle.is_cancelled());
        assert!(clone.flag().load(Ordering::Relaxed));
    }

    #[test]
    fn builder_orders_blocks_thinking_text_tools() {
        let mut builder = AssistantBuilder::default();
        builder.absorb(&AiEvent::TextDelta("answer".into()));
        builder.absorb(&AiEvent::ThinkingDelta("reason".into()));
        builder.absorb(&AiEvent::Started);
        builder.absorb(&AiEvent::ToolCalls(vec![ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }]));
        let message = builder.into_message();
        assert_eq!(message.role, crate::wire::Role::Assistant);
        assert_eq!(message.thinking().as_deref(), Some("reason"));
        assert_eq!(message.text(), "answer");
        assert_eq!(message.tool_calls().len(), 1);
        match &message.blocks[0] {
            ContentBlock::Thinking { .. } => {}
            other => panic!("thinking must come first, got {other:?}"),
        }
    }

    #[test]
    fn builder_skips_empty_block_kinds() {
        let mut builder = AssistantBuilder::default();
        builder.absorb(&AiEvent::TextDelta("only".into()));
        let message = builder.into_message();
        assert_eq!(message.blocks.len(), 1);
        assert!(message.thinking().is_none());
    }

    #[test]
    fn default_options_are_unset() {
        let options = ChatOptions::default();
        assert!(options.temperature.is_none());
        assert!(options.max_tokens.is_none());
        assert_eq!(options.timeout_secs, 300);
    }

    #[test]
    fn usage_is_summed_across_rounds() {
        let left = Usage {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
        };
        let right = Usage {
            input_tokens: 7,
            output_tokens: 3,
            total_tokens: 10,
        };
        let merged = merge_usage(left, right);
        assert_eq!(merged.input_tokens, 17);
        assert_eq!(merged.output_tokens, 7);
        assert_eq!(merged.total_tokens, 24);
    }

    #[test]
    fn round_cap_is_never_zero() {
        // `max_rounds = 0` 会让循环一次都不跑、直接报「已达上限」——不可接受。
        let options = AgentOptions {
            chat: ChatOptions::default(),
            max_rounds: 0,
        };
        assert_eq!(options.max_rounds.max(1), 1);
        assert_eq!(AgentOptions::default().max_rounds, 8);
    }

    // ── 工具循环的端到端测试 ──────────────────────────────────────────
    //
    // 起一个本机 stub SSE 服务，让 `stream_chat` 真走一遍 HTTP：
    // 「发了什么请求、回灌了什么」只有在这一层才看得见。

    /// 本机 stub 服务：按顺序把预设的响应体回给前若干个请求。
    struct Stub {
        base_url: String,
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    fn stub_server(responses: Vec<String>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        std::thread::spawn(move || {
            for body in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                if let Some(payload) = read_request_body(&mut stream) {
                    recorded.lock().unwrap().push(payload);
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });
        Stub {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    fn read_request_body(stream: &mut TcpStream) -> Option<serde_json::Value> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        let header_end = loop {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                return None;
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
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let end = (header_end + length).min(buffer.len());
        serde_json::from_slice(&buffer[header_end..end]).ok()
    }

    /// 一段「只说话」的 SSE。
    fn sse_text(text: &str) -> String {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices": [{"delta": {"content": text}}]}),
            serde_json::json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
            })
        )
    }

    /// 一段「要调工具」的 SSE。参数**分两片**发，顺带盖住分片拼装。
    fn sse_tool_call(id: &str, name: &str, arguments: &str) -> String {
        let (head, tail) = arguments.split_at(arguments.len() / 2);
        format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": id, "function": {"name": name, "arguments": head}}
            ]}}]}),
            serde_json::json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": tail}}
            ]}}]}),
            // 带上 usage：多轮用量要能累加，这一轮少一份就测不出来。
            serde_json::json!({
                "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
            })
        )
    }

    fn stub_profile(base_url: &str) -> ProviderProfile {
        ProviderProfile {
            id: "stub".to_owned(),
            template_id: "openai_compatible".to_owned(),
            name: "stub".to_owned(),
            base_url: base_url.to_owned(),
            model: "stub-model".to_owned(),
            is_local: true,
            api_key: None,
        }
    }

    /// 记录收到什么参数、回固定内容的工具。
    struct Recorder {
        seen: Mutex<Vec<serde_json::Value>>,
        answer: String,
    }

    impl Tool for Recorder {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".to_owned(),
                description: "echo".to_owned(),
                schema: serde_json::json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            }
        }

        fn run(&self, input: &serde_json::Value, _cancel: &AtomicBool) -> ToolOutcome {
            self.seen.lock().unwrap().push(input.clone());
            ToolOutcome::text(self.answer.clone())
        }
    }

    #[test]
    fn tool_loop_executes_the_tool_and_feeds_the_result_back() {
        let stub = stub_server(vec![
            sse_tool_call("call-1", "echo", r#"{"path":"docs/ai-design.md"}"#),
            sse_text("看完了"),
        ]);
        let recorder = Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
            answer: "文件前 3 行：…".to_owned(),
        });
        let mut registry = ToolRegistry::new();
        registry.register(SharedRecorder(Arc::clone(&recorder)));

        let mut events = Vec::new();
        let mut approve = |_call: &ToolCall, risk: ToolRisk| {
            assert_eq!(risk, ToolRisk::ReadOnly);
            Approval::Run
        };
        let outcome = run_agent(
            &stub_profile(&stub.base_url),
            "k",
            "stub-model",
            &[ChatMessage::user("看看设计文档")],
            &AgentOptions::default(),
            &registry,
            &mut approve,
            &RunHandle::new(),
            &mut |event| events.push(event),
        )
        .expect("agent loop");

        // 工具真被调了，且拿到的是**拼装完整**的参数。
        let seen = recorder.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["path"], "docs/ai-design.md");

        // 新增消息 = 助手(要求调工具) + 工具结果 + 助手(结论)。
        assert_eq!(outcome.rounds, 2);
        assert_eq!(outcome.messages.len(), 3);
        assert_eq!(outcome.messages[0].tool_calls().len(), 1);
        assert_eq!(outcome.messages[1].role, crate::wire::Role::Tool);
        assert_eq!(outcome.messages[2].text(), "看完了");

        // 用量取两轮之和。
        assert_eq!(outcome.usage.total_tokens, 14);

        // 第二次请求必须带上工具结果，否则模型是在瞎猜。
        let requests = stub.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let second = &requests[1]["messages"];
        let tool_message = second
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("工具结果没有回灌");
        assert_eq!(tool_message["tool_call_id"], "call-1");
        assert!(
            tool_message["content"]
                .as_str()
                .unwrap()
                .contains("文件前 3 行")
        );
        // 工具声明只在有工具时下发。
        assert_eq!(requests[0]["tools"][0]["function"]["name"], "echo");

        // 事件序列：Started → … → Finished，中间夹着工具的开始与结束。
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolStarted { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolFinished { ok: true, .. }))
        );
    }

    /// 把 `Arc<Recorder>` 包成 `Tool`（`Tool` 要求 `Send + Sync`，`Arc` 满足）。
    struct SharedRecorder(Arc<Recorder>);

    impl Tool for SharedRecorder {
        fn spec(&self) -> ToolSpec {
            self.0.spec()
        }
        fn run(&self, input: &serde_json::Value, cancel: &AtomicBool) -> ToolOutcome {
            self.0.run(input, cancel)
        }
    }

    #[test]
    fn denied_tool_is_reported_to_the_model_not_executed() {
        let stub = stub_server(vec![
            sse_tool_call("call-9", "echo", r#"{"path":"secret"}"#),
            sse_text("那我换个办法"),
        ]);
        let recorder = Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
            answer: "不该被调用".to_owned(),
        });
        let mut registry = ToolRegistry::new();
        registry.register(SharedRecorder(Arc::clone(&recorder)));

        let mut events = Vec::new();
        let mut approve =
            |_call: &ToolCall, _risk: ToolRisk| Approval::Deny("user declined".to_owned());
        let outcome = run_agent(
            &stub_profile(&stub.base_url),
            "k",
            "stub-model",
            &[ChatMessage::user("读一下")],
            &AgentOptions::default(),
            &registry,
            &mut approve,
            &RunHandle::new(),
            &mut |event| events.push(event),
        )
        .expect("agent loop");

        assert!(
            recorder.seen.lock().unwrap().is_empty(),
            "被拒的工具仍然执行了"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolDenied { .. }))
        );
        // 拒绝原因要回灌，模型才知道「不是失败，是用户不同意」。
        let requests = stub.requests.lock().unwrap();
        let tool_message = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("拒绝没有回灌");
        assert!(
            tool_message["content"]
                .as_str()
                .unwrap()
                .contains("user declined")
        );
        assert_eq!(outcome.messages.len(), 3);
    }

    #[test]
    fn loop_stops_at_the_round_cap_and_says_so() {
        // 服务端**每一轮**都要调工具：不设上限就是无限循环。
        let stub = stub_server(vec![
            sse_tool_call("c1", "echo", "{}"),
            sse_tool_call("c2", "echo", "{}"),
            sse_tool_call("c3", "echo", "{}"),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(Recorder {
            seen: Mutex::new(Vec::new()),
            answer: "again".to_owned(),
        });

        let mut events = Vec::new();
        let mut approve = |_call: &ToolCall, _risk: ToolRisk| Approval::Run;
        let outcome = run_agent(
            &stub_profile(&stub.base_url),
            "k",
            "stub-model",
            &[ChatMessage::user("一直调工具")],
            &AgentOptions {
                chat: ChatOptions::default(),
                max_rounds: 2,
            },
            &registry,
            &mut approve,
            &RunHandle::new(),
            &mut |event| events.push(event),
        )
        .expect("agent loop");

        assert_eq!(outcome.rounds, 2);
        // 撞击上限必须**说出来**，不能静默停：UI 要显示这条。
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::RoundsExhausted { max_rounds: 2 }))
        );
        // 最后一轮的调用**仍然被执行**：每条 `tool_calls` 都必须有配对的 `tool` 结果，
        // 否则这段历史下次回灌会被服务端判为非法。所以末尾一定是结果消息，不是空挂的调用。
        assert_eq!(outcome.messages.len(), 4);
        assert_eq!(outcome.messages[2].tool_calls().len(), 1);
        assert_eq!(outcome.messages[3].role, crate::wire::Role::Tool);
        // 两轮各带一份 usage，必须累加（少了就是「只统计了最后一轮」）。
        assert_eq!(outcome.usage.total_tokens, 14);
    }

    #[test]
    fn cancel_between_tools_still_returns_ok_with_a_complete_round() {
        // 本轮有两个工具；审批第一个时就按下停止。
        let stub = stub_server(vec![
            // 两个调用拼成一次响应（`index` 区分）。
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "c1", "function": {"name": "echo", "arguments": "{}"}},
                    {"index": 1, "id": "c2", "function": {"name": "echo", "arguments": "{}"}}
                ]}}]}),
                serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
            ),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(Recorder {
            seen: Mutex::new(Vec::new()),
            answer: "ran".to_owned(),
        });

        let handle = RunHandle::new();
        let canceller = handle.clone();
        let mut events = Vec::new();
        let mut approve = move |_call: &ToolCall, _risk: ToolRisk| {
            // 批到第一个就喊停：第二个必须**不执行**，但要有一个配对的结果消息。
            canceller.cancel();
            Approval::Run
        };
        let outcome = run_agent(
            &stub_profile(&stub.base_url),
            "k",
            "stub-model",
            &[ChatMessage::user("两个都读一下")],
            &AgentOptions::default(),
            &registry,
            &mut approve,
            &handle,
            &mut |event| events.push(event),
        )
        .expect("取消不该变成 Err");

        // 取消走 `Ok` + 标志位：已经发生的对话不该被丢掉。
        assert!(outcome.cancelled);
        assert_eq!(outcome.messages.len(), 3);
        assert_eq!(outcome.messages[1].role, crate::wire::Role::Tool);
        assert_eq!(outcome.messages[2].role, crate::wire::Role::Tool);
        // 第二个工具没执行，但历史里必须有它的结果，否则下次请求非法。
        let cancelled_result = outcome.messages[2].blocks.first().cloned().unwrap();
        assert!(matches!(
            cancelled_result,
            ContentBlock::ToolResult { is_error: true, .. }
        ));
        // 只有第一个工具报过「开始」。
        let started = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolStarted { .. }))
            .count();
        assert_eq!(started, 1);
        // 本轮定稿事件带了全部三条消息（界面据此边跑边显示）。
        let settled = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::RoundCompleted { messages, .. } => Some(messages.len()),
                _ => None,
            })
            .expect("缺少 RoundCompleted");
        assert_eq!(settled, 3);
    }

    #[test]
    fn every_round_reports_its_messages() {
        let stub = stub_server(vec![
            sse_tool_call("call-1", "echo", "{}"),
            sse_text("结论"),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(Recorder {
            seen: Mutex::new(Vec::new()),
            answer: "ok".to_owned(),
        });
        let mut events = Vec::new();
        let mut approve = |_call: &ToolCall, _risk: ToolRisk| Approval::Run;
        let outcome = run_agent(
            &stub_profile(&stub.base_url),
            "k",
            "stub-model",
            &[ChatMessage::user("读一下")],
            &AgentOptions::default(),
            &registry,
            &mut approve,
            &RunHandle::new(),
            &mut |event| events.push(event),
        )
        .expect("agent loop");
        assert!(!outcome.cancelled);

        // 两轮各一条 RoundCompleted，**最后一轮（无工具的纯文本轮）也要有**——
        // 界面靠它清掉流式缓冲，漏了就会在正文后面留一截重复的实时行。
        let rounds: Vec<usize> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::RoundCompleted { round, .. } => Some(*round),
                _ => None,
            })
            .collect();
        assert_eq!(rounds, vec![1, 2]);
        // 两轮的消息加起来必须正好等于总产出，一条不多一条不少。
        let total: usize = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::RoundCompleted { messages, .. } => Some(messages.len()),
                _ => None,
            })
            .sum();
        assert_eq!(total, outcome.messages.len());
    }

    #[test]
    fn no_tools_means_no_tools_field_in_the_request() {
        let stub = stub_server(vec![sse_text("直接回答")]);
        let registry = ToolRegistry::new();
        let mut approve = |_call: &ToolCall, _risk: ToolRisk| Approval::Run;
        run_agent(
            &stub_profile(&stub.base_url),
            "k",
            "stub-model",
            &[ChatMessage::user("你好")],
            &AgentOptions::default(),
            &registry,
            &mut approve,
            &RunHandle::new(),
            &mut |_| {},
        )
        .expect("agent loop");

        let requests = stub.requests.lock().unwrap();
        // 空数组会让部分服务端直接报参数错误，所以宁可不发这个字段。
        assert!(requests[0].get("tools").is_none());
    }
}
