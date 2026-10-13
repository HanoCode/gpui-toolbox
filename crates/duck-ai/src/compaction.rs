//! 上下文预算与压缩（防爆窗）。
//!
//! # 为什么需要它
//!
//! [`crate::agent::run_turn`] 的文档写着「内核不做裁剪，上下文窗口管理是上层的事」，
//! 而在本轮之前**没有任何一层真的做**——于是长会话会把全量历史一直往外发，
//! 直到服务端回一个「超出上下文长度」的 400。这个模块就是补上那一层。
//!
//! # 首版策略：确定性、不调模型
//!
//! 参考 `temp/deepseek-harness/packages/compaction`（`compaction-tool-result-pruner`
//! 的模型无关裁枝 + `compaction/src/tool-pairing.ts` 的配对平衡），但**不调 LLM 做摘要**：
//! 摘要要额外一次网络往返，多一个失败面，还会在流式路径里引入「压缩本身也可能失败」的
//! 分支。首版只做两件**收敛、可解释、可单测**的事：
//!
//! 1. **裁旧工具结果**：超出「近端窗口」的工具结果，保留头尾、中间用显式标记替代。
//! 2. **丢最旧的整段**：仍超预算时，从最前面丢掉若干条完整消息，直到不超。
//!
//! 两步都产出一份 [`CompactionReport`]，供上层在界面上说明「这一轮省了什么」。
//!
//! # 工具配对（最关键的一条纪律）
//!
//! OpenAI 兼容协议要求：每条助手消息里的 `tool_calls` 都必须跟着**等量**的
//! `role=tool` 结果消息。所以**绝不能**在「一个调用还没等到结果」的位置切断历史——
//! 那样拼出来的请求会被服务端直接判为非法。
//!
//! 裁枝不影响配对（消息还在，只是内容变短）；丢弃只允许落在**配对平衡的切点**
//! （[`is_balanced_cut`]：前半段里 `ToolUse` 与 `ToolResult` 数量相等）上。
//!
//! # 预算口径
//!
//! 单位是**字符数**（`char`），不是 token：内核没有分词器，凭空「估算 token」
//! 只会给出一个看着精确、实际不可验证的数字。字符数是 token 数的**单调代理**
//! （中文约 1 字 ≈ 1 token，英文约 4 字符 ≈ 1 token），用它做阈值足够。
//! 阈值见 [`CompactionPolicy`] 的默认值——目前是常量，配置化留待后续（见文件末尾）。
//!
//! # 不带中文
//!
//! 本模块产出的标记（[`OMISSION_MARKER`] 一类）会进模型上下文，因此一律英文，
//! 理由同 [`crate::tool`] 模块头。

use crate::wire::{ChatMessage, ContentBlock, Role};

/// 裁掉工具结果中段时插入的标记（`{}` 处填被省略的字符数）。
const OMISSION_MARKER: &str = "[... {} characters omitted ...]";

/// 上下文预算与压缩口径。
///
/// 全部是常量默认值（本轮不做设置项）：`docs/ai-design.md` 的工具/检索之外，
/// 上下文预算是**安全阀**而不是用户偏好，先给定一组保守值比留空更稳。
/// 将来要配置化，照 `AiSettings` 的 `Option` + `#[serde(default)]` 模式加字段即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// 单次请求允许的正文总字符数；不超过它就不做任何压缩。
    ///
    /// `0` = **关闭压缩**（不是「压到零」）——写成一个显式开关，免得手改配置写 0
    /// 时把整段历史丢光。
    pub max_chars: usize,
    /// 近端窗口的字符预算：这一段**永不裁、永不丢**。
    ///
    /// 保护「当前这一轮」的完整上下文（用户刚问的话、刚拿到的工具结果），
    /// 是压缩方案里唯一不能让步的东西——丢了近端就等于答非所问。
    pub keep_recent_chars: usize,
    /// 裁枝后保留的工具结果头部字符数。
    pub tool_result_head_chars: usize,
    /// 裁枝后保留的工具结果尾部字符数。
    pub tool_result_tail_chars: usize,
    /// 工具结果短于它就不裁（避免给本来就不长的结果平添噪声标记）。
    pub tool_result_min_chars: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            // ≈ 96k 字符（粗算 30k 上下 token），对 128k 窗口的模型留足余量；
            // 加上近端窗口与输出，仍远小于常见窗口上限。
            max_chars: 96_000,
            keep_recent_chars: 32_000,
            tool_result_head_chars: 2_000,
            tool_result_tail_chars: 500,
            // 工具结果本身已被 `ToolOutcome` 截到 12k 字符，所以「4k 以下不动手」
            // 不会误伤正常大小的结果。
            tool_result_min_chars: 4_000,
        }
    }
}

/// 一次压缩做了什么。给界面用来解释「这一轮省了什么」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactionReport {
    /// 压缩前的正文总字符数。
    pub original_chars: usize,
    /// 压缩后的正文总字符数。
    pub final_chars: usize,
    /// 被裁枝的工具结果条数。
    pub pruned_tool_results: usize,
    /// 被丢弃的（最旧的整段）消息条数。
    pub dropped_messages: usize,
}

impl CompactionReport {
    /// 是否真的动了历史。没动时上层不该显示任何「已压缩」提示。
    #[must_use]
    pub fn changed(&self) -> bool {
        self.dropped_messages > 0 || self.pruned_tool_results > 0
    }

    /// 合并两次压缩的统计（多轮 Agent 的每一轮各压一次，收尾要给一个总数）。
    ///
    /// 字符数取**后者**（最近一轮最有代表性），计数则累加。
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            original_chars: other.original_chars,
            final_chars: other.final_chars,
            pruned_tool_results: self
                .pruned_tool_results
                .saturating_add(other.pruned_tool_results),
            dropped_messages: self.dropped_messages.saturating_add(other.dropped_messages),
        }
    }
}

/// 就地把 `messages` 压到预算内，返回本次做了什么。
///
/// 两条不变量：
/// - **系统消息（若在首位）不会被丢**：它是对话的基调；
/// - **近端窗口（[`CompactionPolicy::keep_recent_chars`]）不会被丢**：
///   哪怕压完仍超预算，也只压到这里为止。
#[must_use]
pub fn compact_in_place(
    messages: &mut Vec<ChatMessage>,
    policy: &CompactionPolicy,
) -> CompactionReport {
    let original_chars = total_chars(messages);
    let mut report = CompactionReport {
        original_chars,
        final_chars: original_chars,
        ..CompactionReport::default()
    };
    // 关闭，或本来就够小：一个字节都不动。
    if policy.max_chars == 0 || original_chars <= policy.max_chars {
        return report;
    }

    // 第 1 步：裁旧的工具结果（不改变消息条数，因而**天然不破坏工具配对**）。
    let window_start = recent_window_start(messages, policy.keep_recent_chars);
    let mut pruned = 0usize;
    for message in messages.iter_mut().take(window_start) {
        if message.role != Role::Tool {
            continue;
        }
        for block in &mut message.blocks {
            if let ContentBlock::ToolResult { content, .. } = block
                && let Some(shorter) = prune_text(content, policy)
            {
                *content = shorter;
                pruned += 1;
            }
        }
    }
    report.pruned_tool_results = pruned;
    let after_prune = total_chars(messages);
    if after_prune <= policy.max_chars {
        report.final_chars = after_prune;
        return report;
    }

    // 第 2 步：从最旧处丢整段消息，只落在「配对平衡」的切点上。
    let keep_from = recent_window_start(messages, policy.keep_recent_chars);
    let head = usize::from(matches!(messages.first(), Some(first) if first.role == Role::System));
    // 找**最小**的可行切点（丢得越少越好）；一个都不够就退而求其次，
    // 停在近端窗口前能到达的最后一个平衡切点。
    let mut chosen: Option<usize> = None;
    for cut in head..=keep_from {
        if !is_balanced_cut(messages, cut) {
            continue;
        }
        chosen = Some(cut);
        if after_prune.saturating_sub(chars_in_range(messages, head, cut)) <= policy.max_chars {
            break;
        }
    }
    let dropped = chosen.filter(|cut| *cut > head).map_or(0, |cut| {
        messages.drain(head..cut);
        cut - head
    });
    if dropped > 0 {
        // 明确告诉模型「前面有内容被省略了」——否则它可能把截断处的上下文
        // 当成对话的开头，给出基于残缺信息的自信结论。
        let note =
            format!("[Note: {dropped} earlier message(s) were omitted to fit the context window.]");
        match messages.first_mut() {
            // 有系统消息就追加到它身上，避免出现两条 system 消息。
            Some(first) if first.role == Role::System => {
                first.blocks.push(ContentBlock::Text {
                    text: format!("\n{note}"),
                });
            }
            // 没有系统消息（少见）就补一条 user 提示，保持「只有一条」的规范。
            _ => messages.insert(0, ChatMessage::user(note)),
        }
    }
    report.dropped_messages = dropped;
    report.final_chars = total_chars(messages);
    report
}

/// 便捷版：不改原切片，返回压缩后的新历史。
#[must_use]
pub fn compact(
    messages: &[ChatMessage],
    policy: &CompactionPolicy,
) -> (Vec<ChatMessage>, CompactionReport) {
    let mut owned = messages.to_vec();
    let report = compact_in_place(&mut owned, policy);
    (owned, report)
}

/// 一条消息**会被真正发出去**的正文的字符数。
///
/// `Thinking` 块不计：`to_openai_messages` 明确不回灌思考内容（部分厂商会报错），
/// 把它算进预算会让「其实没占多少上下文」的历史触发无谓的压缩。
fn chars_of(message: &ChatMessage) -> usize {
    let mut total = 0usize;
    for block in &message.blocks {
        match block {
            ContentBlock::Text { text } => total += text.chars().count(),
            ContentBlock::ToolResult { content, .. } => total += content.chars().count(),
            ContentBlock::ToolUse { call } => {
                total += call.arguments.chars().count() + call.name.chars().count();
            }
            ContentBlock::Thinking { .. } => {}
        }
    }
    total
}

fn total_chars(messages: &[ChatMessage]) -> usize {
    messages.iter().map(chars_of).sum()
}

fn chars_in_range(messages: &[ChatMessage], from: usize, to: usize) -> usize {
    messages[from.min(messages.len())..to.min(messages.len())]
        .iter()
        .map(chars_of)
        .sum()
}

/// 近端窗口的起始下标：窗口是 `messages[start..]`。
///
/// **至少包含最后一条消息**——哪怕它自己就超过 `keep_recent_chars`
/// （比如刚贴进来一大段日志）。当前这一轮是底线，丢它等于答非所问。
fn recent_window_start(messages: &[ChatMessage], keep: usize) -> usize {
    let mut acc = 0usize;
    let mut start = messages.len();
    while start > 0 {
        let next = chars_of(&messages[start - 1]);
        // `start < messages.len()` 保证「至少一条」：第一条（即最后一条消息）无条件纳入。
        if acc + next > keep && start < messages.len() {
            break;
        }
        acc += next;
        start -= 1;
    }
    start
}

/// 切点 `cut` 是否工具配对平衡：`messages[..cut]` 里的 `ToolUse` 与 `ToolResult` 数量相等。
///
/// 只有平衡的切点才能安全丢弃——否则会把某条 `tool_calls` 和它的结果拆到两边，
/// 拼出来的请求缺一半，服务端会直接拒绝。
fn is_balanced_cut(messages: &[ChatMessage], cut: usize) -> bool {
    let mut open: i64 = 0;
    for message in messages.iter().take(cut.min(messages.len())) {
        for block in &message.blocks {
            match block {
                ContentBlock::ToolUse { .. } => open += 1,
                ContentBlock::ToolResult { .. } => open -= 1,
                ContentBlock::Text { .. } | ContentBlock::Thinking { .. } => {}
            }
        }
    }
    open == 0
}

/// 裁掉工具结果的中段，保留头尾并插入显式标记；不需要裁时返回 `None`。
fn prune_text(content: &str, policy: &CompactionPolicy) -> Option<String> {
    let total = content.chars().count();
    if total <= policy.tool_result_min_chars {
        return None;
    }
    let head = policy.tool_result_head_chars;
    let tail = policy.tool_result_tail_chars;
    // 保留头尾后没有净收益（甚至更长）就放弃——绝不产出比原文更大的「压缩」结果。
    if head + tail >= total {
        return None;
    }
    let head_text: String = content.chars().take(head).collect();
    let tail_chars: Vec<char> = content.chars().rev().take(tail).collect();
    let tail_text: String = tail_chars.into_iter().rev().collect();
    let omitted = total - head - tail;
    Some(format!(
        "{head_text}\n{}\n{tail_text}",
        OMISSION_MARKER.replace("{}", &omitted.to_string())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 长文本助手消息里确实带 `tool_calls` 时，须能被配对检查识别。
    fn assistant_with_call(id: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                call: crate::wire::ToolCall {
                    id: id.to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{}".to_owned(),
                },
            }],
        }
    }

    fn big_tool_result(chars: usize) -> ChatMessage {
        ChatMessage::tool_result("c", "read_file", "x".repeat(chars), false)
    }

    /// 校验配对不变量：每个结果都有前置调用，每个调用都有结果。
    fn pairing_is_valid(messages: &[ChatMessage]) -> bool {
        let mut open: Vec<String> = Vec::new();
        for message in messages {
            for block in &message.blocks {
                match block {
                    ContentBlock::ToolUse { call } => open.push(call.id.clone()),
                    ContentBlock::ToolResult { call_id, .. } => {
                        if let Some(position) = open.iter().position(|id| id == call_id) {
                            open.remove(position);
                        } else {
                            return false;
                        }
                    }
                    _ => {}
                }
            }
        }
        open.is_empty()
    }

    #[test]
    fn under_budget_is_a_no_op() {
        let messages = vec![ChatMessage::system("sys"), ChatMessage::user("hello")];
        let (out, report) = compact(&messages, &CompactionPolicy::default());
        assert_eq!(out, messages);
        assert!(!report.changed());
        assert_eq!(report.original_chars, report.final_chars);
    }

    #[test]
    fn zero_budget_disables_compaction() {
        let policy = CompactionPolicy {
            max_chars: 0,
            ..CompactionPolicy::default()
        };
        let messages = vec![ChatMessage::user("x".repeat(10_000))];
        let (out, report) = compact(&messages, &policy);
        assert_eq!(out, messages);
        assert!(!report.changed());
    }

    #[test]
    fn prunes_only_old_tool_results_and_marks_the_cut() {
        let policy = CompactionPolicy {
            max_chars: 3_000,
            keep_recent_chars: 1_000,
            tool_result_head_chars: 100,
            tool_result_tail_chars: 50,
            tool_result_min_chars: 200,
        };
        let messages = vec![
            ChatMessage::user("start"),
            big_tool_result(4_000),
            // 近端窗口：这条不该被裁。
            ChatMessage::user("recent"),
        ];
        let (out, report) = compact(&messages, &policy);
        assert!(report.pruned_tool_results >= 1);
        assert_eq!(report.dropped_messages, 0, "裁枝够用时不该丢消息");
        // 消息条数不变 = 工具配对天然保持。
        assert_eq!(out.len(), messages.len());
        // 近端窗口里的那条原样保留。
        assert_eq!(out[2], messages[2]);
        // 旧的工具结果被裁并带显式标记，绝不静默截断。
        let ContentBlock::ToolResult { content, .. } = &out[1].blocks[0] else {
            panic!("期望 tool result 块");
        };
        assert!(content.contains("characters omitted"), "{content}");
        assert!(content.chars().count() < 4_000);
    }

    #[test]
    fn drops_the_oldest_balanced_prefix() {
        let policy = CompactionPolicy {
            max_chars: 3_000,
            keep_recent_chars: 1_000,
            tool_result_head_chars: 100,
            tool_result_tail_chars: 50,
            tool_result_min_chars: 200,
        };
        // 每条 600 字符，共 10 条 → 远超预算；近端窗口只留得住末尾一两条。
        let messages: Vec<ChatMessage> = (0..10)
            .map(|index| ChatMessage::user(format!("{index}").repeat(600)))
            .collect();
        let (out, report) = compact(&messages, &policy);
        assert!(report.dropped_messages > 0, "超预算应当丢弃最旧的消息");
        assert!(out.len() < messages.len());
        // 丢的是最旧的：保留的是原来的尾部。
        assert_eq!(out.last(), messages.last());
        // 仍超预算时也不侵入近端窗口：最后一条一定在。
        assert!(out.iter().any(|message| message == &messages[9]));
    }

    #[test]
    fn never_splits_a_tool_call_from_its_result() {
        let policy = CompactionPolicy {
            max_chars: 1_000,
            keep_recent_chars: 500,
            tool_result_head_chars: 100,
            tool_result_tail_chars: 50,
            tool_result_min_chars: 200,
        };
        // 两条「调用 + 结果」对，加一条很长的当前提问把总长顶上去。
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("q1".repeat(50)),
            assistant_with_call("c1"),
            ChatMessage::tool_result("c1", "read_file", "r".repeat(300), false),
            ChatMessage::user("q2".repeat(50)),
            assistant_with_call("c2"),
            ChatMessage::tool_result("c2", "read_file", "s".repeat(300), false),
            ChatMessage::user("z".repeat(800)),
        ];
        assert!(pairing_is_valid(&messages), "构造的样例本身就该是配对的");
        let (out, report) = compact(&messages, &policy);
        assert!(report.dropped_messages > 0, "应当有丢弃才测得到配对");
        assert!(
            pairing_is_valid(&out),
            "压缩后出现了孤儿调用 / 孤儿结果：{out:#?}"
        );
        // 系统消息与最后一条提问都在。
        assert_eq!(out.first().unwrap().role, Role::System);
        assert_eq!(out.last(), messages.last());
    }

    #[test]
    fn keeps_system_message_and_notes_the_omission() {
        let policy = CompactionPolicy {
            max_chars: 500,
            keep_recent_chars: 100,
            tool_result_head_chars: 50,
            tool_result_tail_chars: 20,
            tool_result_min_chars: 100,
        };
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("a".repeat(400)),
            ChatMessage::user("b".repeat(400)),
            ChatMessage::user("tail"),
        ];
        let (out, report) = compact(&messages, &policy);
        assert!(report.dropped_messages > 0);
        // 系统消息仍是最前面那条，且被追加了「有内容被省略」的提示。
        assert_eq!(out[0].role, Role::System);
        assert!(out[0].text().contains("were omitted"), "{:?}", out[0]);
        assert_eq!(out.last().unwrap().text(), "tail");
    }

    #[test]
    fn a_single_oversized_message_is_still_kept() {
        let policy = CompactionPolicy {
            max_chars: 100,
            keep_recent_chars: 10,
            ..CompactionPolicy::default()
        };
        let messages = vec![ChatMessage::user("x".repeat(10_000))];
        let (out, _report) = compact(&messages, &policy);
        // 近端窗口「至少一条」：即便它自己超预算，也必须留着。
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text().chars().count(), 10_000);
    }

    #[test]
    fn thinking_blocks_do_not_count_toward_the_budget() {
        // 思考内容不回灌，所以它不该把对话顶进压缩。
        let mut message = ChatMessage::assistant("hi");
        message.blocks.insert(
            0,
            ContentBlock::Thinking {
                text: "z".repeat(50_000),
            },
        );
        assert_eq!(chars_of(&message), 2);
        assert!(
            !compact(&[message], &CompactionPolicy::default())
                .1
                .changed()
        );
    }

    #[test]
    fn balanced_cuts_track_open_calls() {
        let messages = vec![
            ChatMessage::user("q"),
            assistant_with_call("c1"),
            ChatMessage::tool_result("c1", "read_file", "r", false),
        ];
        assert!(is_balanced_cut(&messages, 0));
        assert!(is_balanced_cut(&messages, 1));
        // 切在调用与其结果之间 = 不平衡。
        assert!(!is_balanced_cut(&messages, 2));
        assert!(is_balanced_cut(&messages, 3));
    }

    #[test]
    fn report_merge_sums_counters_and_keeps_latest_chars() {
        let first = CompactionReport {
            original_chars: 100,
            final_chars: 80,
            pruned_tool_results: 1,
            dropped_messages: 2,
        };
        let second = CompactionReport {
            original_chars: 90,
            final_chars: 60,
            pruned_tool_results: 3,
            dropped_messages: 4,
        };
        let merged = first.merge(second);
        assert_eq!(merged.original_chars, 90);
        assert_eq!(merged.final_chars, 60);
        assert_eq!(merged.pruned_tool_results, 4);
        assert_eq!(merged.dropped_messages, 6);
    }
}
