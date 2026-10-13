//! 上下文规模的**启发式估算**——不是精确 tokenizer。
//!
//! 本 crate 不引任何分词器：接一份真实 BPE 词表意味着几 MB 的二进制资源、以及
//! 每个厂商各自的差异，而这里要回答的问题只有一个——**「这段对话大概占多少预算」**，
//! 让用户在把消息发出去**之前**看到量级即可。
//!
//! 经验规则（对主流 BPE 词表的公开统计取整得到）：
//!
//! | 字符类 | 估算 | 依据 |
//! | --- | --- | --- |
//! | CJK / 全角（汉字、假名、谚文、全角标点） | ≈ 1 token / 字 | 常用汉字在词表里多为 1 个 token，生僻字 2~3 |
//! | 其它非 ASCII（重音字母、emoji…） | ≈ 1 token / 字 | emoji 常占 3~4 字节，按字估比按字节估稳 |
//! | ASCII 字母 / 数字 | ≈ 1 token / 4 字 | 英文经验值 ≈ 4 字符 / token |
//! | ASCII 空白与标点 | ≈ 1 token / 3 字 | 标点常与相邻词合并成同一个 token，略省 |
//!
//! 每条消息另计一个固定开销（role、分隔符、chat 模板）。
//!
//! ⚠️ **一律称「估算」**：偏差 20%~40% 是常态（代码、JSON、长数字串偏得更多）。
//! 把它当精确值去卡上限会误导人——真值只能以服务端回的 `usage` 为准。
//!
//! ⚠️ `Thinking` 块**不计**：与 [`crate::compaction`] 同一口径——发请求时思考内容
//! 不回灌（部分厂商会直接报错），算进去会让估算虚高、让用户以为该压缩了。

use crate::wire::{ChatMessage, ContentBlock};

/// 每条消息的固定开销（role 名、role 分隔符、chat 模板的头尾）。
const PER_MESSAGE_OVERHEAD: usize = 4;
/// ASCII 字母 / 数字：约几个字符一个 token。
const ASCII_ALNUM_PER_TOKEN: usize = 4;
/// ASCII 空白 / 标点：合并得更狠一点。
const ASCII_PUNCT_PER_TOKEN: usize = 3;

/// 估算一段文本的 token 数。
///
/// 分段计数后**向上取整**：不足一个 token 的余量也算一个。宁可略高——估低会让
/// 「还能再塞一点」的判断出错，而这条估算的用途正是「发出去之前看看够不够」。
#[must_use]
pub fn estimate_text(text: &str) -> usize {
    let mut wide = 0usize;
    let mut alnum = 0usize;
    let mut punct = 0usize;
    for ch in text.chars() {
        if is_wide(ch) {
            wide += 1;
        } else if ch.is_ascii_alphanumeric() {
            alnum += 1;
        } else if ch.is_ascii() {
            punct += 1;
        } else {
            // 拉丁重音字母 / 西里尔 / 阿拉伯 / emoji：一个字大约一个 token。
            wide += 1;
        }
    }
    wide + alnum.div_ceil(ASCII_ALNUM_PER_TOKEN) + punct.div_ceil(ASCII_PUNCT_PER_TOKEN)
}

/// 估算一整段对话（含 system、工具结果与工具调用参数）的 token 数。
#[must_use]
pub fn estimate_messages(messages: &[ChatMessage]) -> usize {
    messages.iter().map(estimate_message).sum()
}

/// 估算「下一次请求会发出去的那份上下文」：可选的 system 提示 + 历史消息。
///
/// 与调用方组装请求的顺序一致（system 在最前、空提示不加），所以界面可以直接拿它
/// 显示「当前上下文大概多大」。空白 system 提示不计——那一条压根不会出现在请求里。
#[must_use]
pub fn estimate_conversation(system_prompt: Option<&str>, messages: &[ChatMessage]) -> usize {
    let system = system_prompt
        .filter(|text| !text.trim().is_empty())
        .map_or(0, |text| PER_MESSAGE_OVERHEAD + estimate_text(text));
    system + estimate_messages(messages)
}

/// 估算单条消息。
#[must_use]
pub fn estimate_message(message: &ChatMessage) -> usize {
    let mut total = PER_MESSAGE_OVERHEAD;
    for block in &message.blocks {
        match block {
            ContentBlock::Text { text } => total += estimate_text(text),
            ContentBlock::ToolResult { content, .. } => total += estimate_text(content),
            ContentBlock::ToolUse { call } => {
                total += estimate_text(&call.name) + estimate_text(&call.arguments);
            }
            // 不回灌，不计（见模块头）。
            ContentBlock::Thinking { .. } => {}
        }
    }
    total
}

/// 东亚全角字符（CJK 统一表意、假名、谚文、全角标点等）。
///
/// 区间取自 Unicode 的 East Asian Width = Wide / Fullwidth 集合（与终端排版里
/// 「一个字符占两列」是同一批码位）。这里只用来分段计数，不需要更高的精度。
fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F      // 谚文字母
        | 0x2E80..=0x303E    // CJK 部首 / 康熙部首 / CJK 标点
        | 0x3041..=0x33FF    // 平假名 / 片假名 / 注音 / CJK 兼容
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 统一表意
        | 0xA000..=0xA4CF    // 彝文
        | 0xAC00..=0xD7A3    // 谚文音节
        | 0xF900..=0xFAFF    // CJK 兼容表意
        | 0xFE30..=0xFE4F    // CJK 兼容形式
        | 0xFF00..=0xFF60    // 全角 ASCII
        | 0xFFE0..=0xFFE6    // 全角符号
        | 0x20000..=0x2FA1F  // CJK 扩展 B 及以后
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Role, ToolCall};

    #[test]
    fn empty_input_costs_nothing() {
        assert_eq!(estimate_text(""), 0);
        assert_eq!(estimate_messages(&[]), 0);
    }

    #[test]
    fn ascii_is_about_a_quarter_of_its_length() {
        // 4 字符 / token ⇒ 8 字符 = 2。
        assert_eq!(estimate_text("abcdefgh"), 2);
        // 余数向上取整：5 字符 = 2。
        assert_eq!(estimate_text("abcde"), 2);
        assert!(estimate_text("hello world") > 0);
    }

    #[test]
    fn cjk_is_about_one_token_per_character() {
        assert_eq!(estimate_text("数据库"), 3);
        assert_eq!(estimate_text("连接失败"), 4);
        // 全角标点也算 wide。
        assert_eq!(estimate_text("（一）"), 3);
    }

    #[test]
    fn mixed_text_adds_both_halves() {
        // "数据库" 3 + "abc" ceil(3/4)=1 = 4
        assert_eq!(estimate_text("数据库abc"), 4);
        // 中英混排一定比只数宽字符大。
        assert!(estimate_text("数据库abc") > estimate_text("数据库"));
    }

    #[test]
    fn emoji_and_accents_count_as_wide() {
        assert_eq!(estimate_text("é"), 1);
        assert_eq!(estimate_text("🚀"), 1);
    }

    #[test]
    fn estimate_grows_monotonically_with_content() {
        let mut previous = 0usize;
        let mut text = String::new();
        for piece in ["hello ", "world ", "数据库 ", "连接 ", "失败 ", "abc "] {
            text.push_str(piece);
            let now = estimate_text(&text);
            assert!(now >= previous, "{now} < {previous}（内容变多估算却变小）");
            previous = now;
        }
    }

    #[test]
    fn each_message_carries_a_fixed_overhead() {
        let one = estimate_messages(&[ChatMessage::user("")]);
        assert_eq!(one, PER_MESSAGE_OVERHEAD);
        let three = estimate_messages(&[
            ChatMessage::system("s"),
            ChatMessage::user("u"),
            ChatMessage::assistant("a"),
        ]);
        assert!(three > one);
    }

    #[test]
    fn tool_calls_and_results_count_but_thinking_does_not() {
        let call = ToolCall {
            id: "c1".to_owned(),
            name: "read_file".to_owned(),
            arguments: "{\"path\":\"a.md\"}".to_owned(),
        };
        let with_call = ChatMessage {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse { call }],
        };
        assert!(estimate_message(&with_call) > PER_MESSAGE_OVERHEAD);

        let result = ChatMessage::tool_result("c1", "read_file", "内容".repeat(50), false);
        assert!(estimate_message(&result) > PER_MESSAGE_OVERHEAD);

        // 思考内容不回灌 ⇒ 不计入估算。
        let plain = ChatMessage::assistant("答案");
        let with_thinking = ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "答案".to_owned(),
                },
                ContentBlock::Thinking {
                    text: "很长的思考过程".repeat(100),
                },
            ],
        };
        assert_eq!(estimate_message(&plain), estimate_message(&with_thinking));
    }

    #[test]
    fn conversation_adds_the_system_prompt_only_when_it_is_not_blank() {
        let messages = [ChatMessage::user("你好")];
        let without = estimate_conversation(None, &messages);
        assert_eq!(without, estimate_messages(&messages));

        // 空白提示不占上下文——它压根不进请求。
        assert_eq!(estimate_conversation(Some("   "), &messages), without);

        let with = estimate_conversation(Some("你是助手"), &messages);
        assert_eq!(
            with,
            without + PER_MESSAGE_OVERHEAD + 4,
            "system 也要算一条消息"
        );
    }
}
