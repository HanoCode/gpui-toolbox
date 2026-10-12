//! SSE 行解析与流式增量累积。
//!
//! 参考 `temp/zed`：`open_ai/src/open_ai.rs` 的 SSE 逐行解析
//! （`BufReader::lines()` + `data:` 前缀 + `[DONE]` 终止），
//! 以及 `language_model_core/src/chat_completion.rs:424` 的 `ChatCompletionEventMapper`
//! 与 `:312` 的 `ToolCallAccumulator`。
//!
//! **`ToolCallAccumulator` 是自研 OpenAI 客户端最容易踩的坑**：流式 `tool_calls`
//! 是**分片**到达的——第一片带 `id` / `function.name` / 前几个字符的
//! `function.arguments`，后续片只带 `index` 与更多 `arguments` 字符。必须按
//! `index` 累加字符串拼装，且要容忍三种脏数据：
//!
//! 1. `index` 字段缺失或为负（部分自建网关会这么发）；
//! 2. `id` / `name` 只在首片出现，后续片没有；
//! 3. 思考字段两种写法：`reasoning_content`（DeepSeek / 通义）与 `reasoning`（部分网关）。
//!
//! 本模块是**纯函数**，不碰网络也不碰 UI，可用普通 `#[test]` 全覆盖。

use std::collections::BTreeMap;

use crate::wire::{AiEvent, StopReason, ToolCall, Usage};

/// 一行 SSE 的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseLine<'a> {
    /// `data: <payload>`——交给 [`StreamAccumulator::feed`]。
    Data(&'a str),
    /// `data: [DONE]`——流正常结束。
    Done,
    /// 空行、`:` 注释行、`event:` / `id:` / `retry:` 等本实现不关心的字段。
    Skip,
}

/// 解析一行 SSE。
///
/// 只认 `data:`；`:` 开头是注释（SSE 规范里服务端常用它做心跳），必须忽略而不是
/// 当数据解析——否则心跳会把 JSON 解析打出一串假错误。
#[must_use]
pub fn parse_line(line: &str) -> SseLine<'_> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return SseLine::Skip;
    }
    let Some(rest) = trimmed.strip_prefix("data:") else {
        return SseLine::Skip;
    };
    let payload = rest.trim_start();
    if payload == "[DONE]" {
        return SseLine::Done;
    }
    SseLine::Data(payload)
}

/// 按 `index` 累加分片工具调用的拼装器。
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    /// `index` → 分片累计值。用 `BTreeMap` 而不是 `Vec`：`index` 不保证从 0 连续。
    slots: BTreeMap<i64, PartialToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    /// 累积一个分片。负 `index`（含 `-1`）视为「继续最后一个槽位」。
    pub fn push(
        &mut self,
        index: i64,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) {
        let key = if index < 0 {
            self.slots.keys().next_back().copied().unwrap_or(0)
        } else {
            index
        };
        let slot = self.slots.entry(key).or_default();
        // `id` / `name` 只在首片出现，后续片缺字段时**不能覆盖**成空串。
        if let Some(id) = id.filter(|value| !value.is_empty()) {
            slot.id = id.to_owned();
        }
        if let Some(name) = name.filter(|value| !value.is_empty()) {
            slot.name = name.to_owned();
        }
        if let Some(arguments) = arguments {
            slot.arguments.push_str(arguments);
        }
    }

    /// 是否一个分片都没收到。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 取出拼装结果（按 `index` 升序）。参数为空的槽位补 `{}`，
    /// 否则调用方会拿到一个解析不出参数的调用。
    #[must_use]
    pub fn into_calls(self) -> Vec<ToolCall> {
        self.slots
            .into_values()
            .filter(|slot| !slot.name.is_empty())
            .map(|slot| ToolCall {
                id: slot.id,
                name: slot.name,
                arguments: if slot.arguments.trim().is_empty() {
                    "{}".to_owned()
                } else {
                    slot.arguments
                },
            })
            .collect()
    }
}

/// 一条流的累积器：吃 SSE 数据负载，吐 [`AiEvent`]。
///
/// 文本与思考增量**立即**上报（UI 要逐字长出来）；工具调用**攒到最后**一次性上报
/// （分片状态对 UI 没有意义，且半成品参数解析必失败）。
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    tool_calls: ToolCallAccumulator,
    usage: Usage,
    stop_reason: StopReason,
    /// 已经上报过 `Finished`，防止重复。
    finished: bool,
}

impl StreamAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 累计用量（后到的覆盖先到的）。
    fn merge_usage(&mut self, value: &serde_json::Value) {
        let pick = |keys: &[&str]| -> Option<u32> {
            keys.iter()
                .find_map(|key| value.get(*key))
                .and_then(serde_json::Value::as_u64)
                .and_then(|tokens| u32::try_from(tokens).ok())
        };
        if let Some(input) = pick(&["prompt_tokens", "input_tokens"]) {
            self.usage.input_tokens = input;
        }
        if let Some(output) = pick(&["completion_tokens", "output_tokens"]) {
            self.usage.output_tokens = output;
        }
        if let Some(total) = pick(&["total_tokens"]) {
            self.usage.total_tokens = total;
        }
        if self.usage.total_tokens == 0 {
            self.usage.total_tokens = self.usage.input_tokens + self.usage.output_tokens;
        }
    }

    /// 吃一条 `data:` 负载，返回本次要上报的事件。
    ///
    /// 非法 JSON **不报错**：部分网关会在流里夹非 JSON 的心跳行，
    /// 静默跳过比中断整轮对话正确。真正的错误走顶层 `error` 字段。
    pub fn feed(&mut self, data: &str) -> Vec<AiEvent> {
        let mut events = Vec::new();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            return events;
        };

        // 顶层 `error`：这是协议级失败，必须终止。
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| error.as_str().map(str::to_owned))
                .unwrap_or_else(|| error.to_string());
            events.push(AiEvent::Failed(message));
            return events;
        }

        if let Some(usage) = value.get("usage").filter(|value| !value.is_null()) {
            self.merge_usage(usage);
        }

        let Some(choice) = value.pointer("/choices/0") else {
            // 只有 usage 的收尾块（部分网关会单独发一条）——不算错。
            return events;
        };

        if let Some(delta) = choice.get("delta") {
            let thinking = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !thinking.is_empty() {
                events.push(AiEvent::ThinkingDelta(thinking.to_owned()));
            }

            if let Some(content) = delta.get("content").and_then(serde_json::Value::as_str)
                && !content.is_empty()
            {
                events.push(AiEvent::TextDelta(content.to_owned()));
            }

            if let Some(calls) = delta
                .get("tool_calls")
                .and_then(serde_json::Value::as_array)
            {
                for (position, call) in calls.iter().enumerate() {
                    let index = call
                        .get("index")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(position as i64);
                    let id = call.get("id").and_then(serde_json::Value::as_str);
                    let function = call.get("function");
                    let name = function
                        .and_then(|value| value.get("name"))
                        .and_then(serde_json::Value::as_str);
                    let arguments = function
                        .and_then(|value| value.get("arguments"))
                        .and_then(serde_json::Value::as_str);
                    self.tool_calls.push(index, id, name, arguments);
                }
            }
        }

        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
        {
            self.stop_reason = StopReason::from_wire(reason);
        }

        events
    }

    /// 流结束后收尾：吐工具调用（若有）与 `Finished`。
    ///
    /// 幂等——重复调用只出一次事件。
    pub fn finish(&mut self) -> Vec<AiEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut events = Vec::new();
        let calls = std::mem::take(&mut self.tool_calls).into_calls();
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

    /// 目前累计到的用量 / 结束原因（出错时也要能读到已产生的部分）。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_parsing_ignores_comments_and_blank_lines() {
        assert_eq!(parse_line(""), SseLine::Skip);
        assert_eq!(parse_line("   "), SseLine::Skip);
        assert_eq!(parse_line(": keep-alive"), SseLine::Skip);
        assert_eq!(parse_line("event: message"), SseLine::Skip);
        assert_eq!(parse_line("data: [DONE]"), SseLine::Done);
        assert_eq!(parse_line("data: {\"a\":1}"), SseLine::Data("{\"a\":1}"));
        assert_eq!(parse_line("data:{\"a\":1}\r\n"), SseLine::Data("{\"a\":1}"));
    }

    #[test]
    fn text_deltas_are_emitted_in_order() {
        let mut acc = StreamAccumulator::new();
        let chunk = |text: &str| {
            format!(
                "{{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}"
            )
        };
        let mut events = Vec::new();
        events.extend(acc.feed(&chunk("Hello")));
        events.extend(acc.feed(&chunk(" world")));
        events.extend(acc.feed(
            "{\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}",
        ));
        assert_eq!(
            events,
            vec![
                AiEvent::TextDelta("Hello".into()),
                AiEvent::TextDelta(" world".into()),
            ]
        );
        let tail = acc.finish();
        assert_eq!(
            tail,
            vec![AiEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 7,
                    output_tokens: 2,
                    total_tokens: 9
                }
            }]
        );
        // 幂等。
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn reasoning_field_both_spellings() {
        let mut acc = StreamAccumulator::new();
        let first = acc.feed("{\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}");
        let second = acc.feed("{\"choices\":[{\"delta\":{\"reasoning\":\"more\"}}]}");
        assert_eq!(first, vec![AiEvent::ThinkingDelta("think".into())]);
        assert_eq!(second, vec![AiEvent::ThinkingDelta("more".into())]);
    }

    #[test]
    fn tool_calls_are_accumulated_across_fragments() {
        let mut acc = StreamAccumulator::new();
        // 首片：带 id 与 name，参数只到一半。
        acc.feed(
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}",
        );
        // 后续片：只有更多 arguments，没有 id / name。
        acc.feed(
            "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]}}]}",
        );
        acc.feed("{\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}");
        let events = acc.finish();
        let AiEvent::ToolCalls(calls) = &events[0] else {
            panic!("expected tool calls, got {events:?}");
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].parsed_arguments()["path"], "a.txt");
    }

    #[test]
    fn negative_and_missing_index_fall_back() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(-1, Some("c1"), Some("list_dir"), Some("{\"p\""));
        acc.push(-1, None, None, Some(":1}"));
        let calls = acc.into_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, "{\"p\":1}");
    }

    #[test]
    fn empty_arguments_are_normalised_to_object() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(0, Some("c1"), Some("noop"), None);
        assert_eq!(acc.into_calls()[0].arguments, "{}");
    }

    #[test]
    fn nameless_slots_are_dropped() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(0, Some("c1"), None, Some("{}"));
        assert!(acc.into_calls().is_empty());
    }

    #[test]
    fn error_payloads_stop_the_stream() {
        let mut acc = StreamAccumulator::new();
        let events = acc.feed("{\"error\":{\"message\":\"rate limited\"}}");
        assert_eq!(events, vec![AiEvent::Failed("rate limited".into())]);
    }

    #[test]
    fn non_json_and_usage_only_chunks_are_ignored() {
        let mut acc = StreamAccumulator::new();
        assert!(acc.feed("ping").is_empty());
        assert!(
            acc.feed("{\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}")
                .is_empty()
        );
        assert_eq!(acc.usage().total_tokens, 4);
    }
}
