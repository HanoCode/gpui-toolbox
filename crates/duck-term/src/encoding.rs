//! 会话编码：UTF-8 与 GB18030（GBK / GB2312 的超集）。
//!
//! # 为什么要自己解码
//!
//! `alacritty_terminal` 的 `vte` 解析器只认 UTF-8：GBK 的 `D6 D0` 会被当成
//! 两个非法 UTF-8 字节、渲染成一串替换符。GBK 文件（`cat gbk.txt`）、GBK 编码的
//! 日志、只认 GB 字节的老程序都属于这种情形 —— 这是**客户端解码**问题，
//! 与远端 locale（`ls` 主动输出 `\350\264\265` 这类 ASCII 转义）是两件事。
//!
//! # 为什么能在解析器之前整体转码
//!
//! 转码点选在「PTY 原始字节 → 解析器」之间（见 `Session::spawn` 的预处理器）。
//! 关键在于：**ANSI 控制序列是纯 7 位的**，而 GBK 的首字节恒在 `0x81..=0xFE`，
//! 尾字节恒在 `0x40..=0xFE`（且不含 `0x1B`）—— 所以「高位字节 = 多字节字符的
//! 首字节」这条判定不会吃掉控制序列，不需要维护一套 ANSI 状态机：
//!
//! - `< 0x80` 的字节原样放行（控制序列、ASCII 文本都走这条）；
//! - `>= 0x81` 的字节开始一次 GB 编码字符，凑齐 1~4 字节后转成 UTF-8 写出。
//!
//! 多字节字符可能被 `read` 切断（PTY 的块边界不保证落在字符边界上），残留的
//! 半截字节留在 [`OutputDecoder`] 里跨块续接 —— 这是解码器必须是有状态的原因。

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// UTF-8 的替换符（非法字节的落点，与解析器自己遇到坏 UTF-8 时一致）。
const REPLACEMENT: &[u8] = b"\xEF\xBF\xBD";

/// 会话编码。
/// 三档：**自动判别**（默认）、UTF-8、GB18030。GB18030 是 GBK / GB2312 的
/// **严格超集**，一个选项覆盖三种常见的中文编码，不做「GBK 还是 GB2312」
/// 这种只会让用户选错的区分。
///
/// 为什么默认是自动：远端吐哪种编码客户端猜不到（Java 日志多半 UTF-8，
/// 老系统 / GBK 文件是 GB），选反了比不选更糟 —— 「UTF-8 字节按 GBK 解」
/// 会得到一堆看似中文的乱码（`施T~_严管…`），「GBK 字节按 UTF-8 解」全是
/// 替换符。自动档按字节流特征现判（见 [`OutputDecoder`]），判错的可能性
/// 远小于人选错。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    /// 自动：UTF-8 优先，出现「非法 UTF-8 但合法 GB」的字节序列时切到 GB18030。
    #[default]
    Auto,
    /// 强制 UTF-8。
    Utf8,
    /// 强制 GB18030（含 GBK / GB2312 的全部字符）。
    Gb18030,
}

impl Encoding {
    /// 全部档位（设置页与菜单的展示顺序）。
    pub const ALL: [Self; 3] = [Self::Auto, Self::Utf8, Self::Gb18030];

    /// 持久化 / 菜单标识用的值。
    pub const fn value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Utf8 => "utf-8",
            Self::Gb18030 => "gb18030",
        }
    }

    /// 由持久化值还原；未知值返回 `None`（调用方据此回落默认，不猜）。
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.value() == value)
    }

    const fn code(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Utf8 => 1,
            Self::Gb18030 => 2,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Utf8,
            2 => Self::Gb18030,
            _ => Self::Auto,
        }
    }
}

/// 会话级编码开关：**读侧（解码器）与写侧（键盘输入）共用同一个原子量**。
///
/// 热切换因此不需要碰事件循环线程：解码器每块数据现读一次，用户改完设置后
/// 下一条输出就按新编码解释（已经画在屏幕上的旧内容不会回溯重排 —— 那需要
/// 一条字节级的历史，代价远大于收益，用户按一下 `⌃L` 即可）。
#[derive(Clone, Debug)]
pub struct EncodingHandle(Arc<AtomicU8>);

impl EncodingHandle {
    /// 以初始编码建一个把手。
    pub fn new(encoding: Encoding) -> Self {
        Self(Arc::new(AtomicU8::new(encoding.code())))
    }

    /// 当前编码。
    pub fn get(&self) -> Encoding {
        Encoding::from_code(self.0.load(Ordering::Relaxed))
    }

    /// 切换编码（幂等；同一编码重复设置不做任何事）。
    pub fn set(&self, encoding: Encoding) {
        self.0.store(encoding.code(), Ordering::Relaxed);
    }
}

impl Default for EncodingHandle {
    fn default() -> Self {
        Self::new(Encoding::default())
    }
}

/// PTY 输出的增量解码器。
///
/// 三种工作方式（按调用方传入的当前编码）：
///
/// - **Utf8**：原样直通 —— 解析器自己就是 UTF-8 解码器，没必要多过一遍；
/// - **Gb18030**：整流按 GB 解码（见 [`OutputDecoder::push_gb`]）；
/// - **Auto**：UTF-8 优先的探测。ASCII 直通；遇到高位字节就把它和后续字节
///   暂存进探针，哪边先凑出**完整合法**的字符就按哪边放行：
///   UTF-8 序列合法 → 原样输出（继续 Auto）；UTF-8 判负而 GB 成熟 → 按 GB
///   解出并**黏住**（返回 `Some(Gb18030)`，调用方应把它写回把手，让输入方向
///   一起切）；两边都判不通 → 报替换符并从下一字节重新起探。
///
/// 有状态是因为**字符可能被 `read` 切断**：半截序列留在缓冲里跨块续接。
/// 已知取舍：GB 编码里「首字节 C2..DF + 尾字节 80..BF」的字符恰好也是合法的
/// 两字节 UTF-8（如 `D2 A1`），探测会先判给 UTF-8 —— 直到流里出现第一个
/// 判负的字符才切到 GB。这是无字典判别的固有歧义，取「UTF-8 优先」是因为
/// 现代系统的输出绝大多数是 UTF-8，误判代价（错几个字后自动纠正）远小于
/// 把 UTF-8 流整个判成 GB。
#[derive(Debug, Default)]
pub struct OutputDecoder {
    /// 探测中的原始字节（Auto），或 GB 状态机的半截前缀（Gb）。
    pending: Vec<u8>,
    /// 解码器内部的工作方式（Auto 只在入口出现，内部判别后落为 Gb）。
    mode: Mode,
    /// 上一次 `push` 看到的编码：变了说明用户热切过，就地重启状态机。
    current: Encoding,
}

#[derive(Debug, Default, PartialEq)]
enum Mode {
    #[default]
    Auto,
    Gb,
}

/// UTF-8 首字节还需要几个续字节；`None` = 这个字节根本当不了首字节。
fn utf8_continuation_need(first: u8) -> Option<usize> {
    match first {
        0xC2..=0xDF => Some(1),
        0xE0..=0xEF => Some(2),
        0xF0..=0xF4 => Some(3),
        _ => None,
    }
}

const fn is_utf8_continuation(byte: u8) -> bool {
    // 直接比较：`RangeInclusive::contains` 在 const fn 里尚未稳定（MSRV 1.85）。
    byte >= 0x80 && byte <= 0xBF
}

/// GB 序列的结构判定（只看字节范围，不解码）。
enum GbStatus {
    /// 前 `usize` 个字节是一个完整字符。
    Complete(usize),
    Incomplete,
    Invalid,
}

fn gb_sequence_len(bytes: &[u8]) -> GbStatus {
    let Some(&first) = bytes.first() else {
        return GbStatus::Incomplete;
    };
    if !(0x81..=0xFE).contains(&first) {
        return GbStatus::Invalid;
    }
    let Some(&second) = bytes.get(1) else {
        return GbStatus::Incomplete;
    };
    if (0x30..=0x39).contains(&second) {
        // GB18030 四字节段：后两位也必须是数字。
        if bytes.len() < 4 {
            return GbStatus::Incomplete;
        }
        if (0x30..=0x39).contains(&bytes[2]) && (0x30..=0x39).contains(&bytes[3]) {
            GbStatus::Complete(4)
        } else {
            GbStatus::Invalid
        }
    } else if (0x40..=0xFE).contains(&second) && second != 0x7F {
        GbStatus::Complete(2)
    } else {
        GbStatus::Invalid
    }
}

impl OutputDecoder {
    /// 把一块原始字节解码后追加进 `out`。
    ///
    /// 返回 `Some(encoding)` 表示 **Auto 档探测到了 GB 字节流**：调用方应当把
    /// 把手切到该编码（黏住），输入方向随之一起转。
    pub fn push(
        &mut self,
        bytes: &[u8],
        encoding: Encoding,
        out: &mut Vec<u8>,
    ) -> Option<Encoding> {
        if encoding != self.current {
            // 用户热切：丢掉半截前缀（它已不可能被凑齐），就地重启。
            self.current = encoding;
            self.pending.clear();
            self.mode = if encoding == Encoding::Gb18030 {
                Mode::Gb
            } else {
                Mode::Auto
            };
        }
        let mut detected = None;
        match encoding {
            Encoding::Utf8 => out.extend_from_slice(bytes),
            Encoding::Gb18030 => {
                for &byte in bytes {
                    self.push_gb(byte, out);
                }
            }
            Encoding::Auto => {
                for &byte in bytes {
                    if let Some(encoding) = self.push_auto(byte, out) {
                        detected = Some(encoding);
                    }
                }
            }
        }
        detected
    }

    /// Auto 档单字节推进。
    fn push_auto(&mut self, byte: u8, out: &mut Vec<u8>) -> Option<Encoding> {
        if self.mode == Mode::Gb {
            // 已判别为 GB（黏住）：后续字节全走 GB 状态机。
            self.push_gb(byte, out);
            return None;
        }
        if self.pending.is_empty() && byte < 0x80 {
            // ASCII 直通（控制序列、英文文本都在这条）。
            out.push(byte);
            return None;
        }
        self.pending.push(byte);
        self.try_resolve(out)
    }

    /// 尝试对手里的探针做出裁决。
    fn try_resolve(&mut self, out: &mut Vec<u8>) -> Option<Encoding> {
        if self.pending.is_empty() {
            return None;
        }
        let probe = self.pending.clone();
        let first = probe[0];

        // 首字节已经落到「既不是 ASCII 也不是任何编码的合法首字节」（上一轮
        // 裁决切出来的余量）—— 原样放行，继续裁决余量。
        if first < 0x80 {
            out.push(first);
            self.pending.drain(..1);
            return if self.pending.is_empty() {
                None
            } else {
                self.try_resolve(out)
            };
        }

        // —— UTF-8 侧：合法首字节且续字节全部合法才算赢 ——
        if let Some(need) = utf8_continuation_need(first) {
            let have = probe.len() - 1;
            let broken = probe[1..].iter().any(|b| !is_utf8_continuation(*b));
            if !broken && have < need {
                return None; // 还差续字节，继续探
            }
            if !broken {
                // 完整合法的 UTF-8 序列：原样放行；余量交回去重新起探。
                out.extend_from_slice(&probe[..need + 1]);
                self.pending.drain(..need + 1);
                return if self.pending.is_empty() {
                    None
                } else {
                    self.try_resolve(out)
                };
            }
            // broken：UTF-8 判负，转 GB 侧
        }

        // —— GB 侧 ——
        match gb_sequence_len(&probe) {
            GbStatus::Incomplete if probe.len() < 4 => return None,
            GbStatus::Complete(len) => {
                // GB 胜出：本会话黏住 GB。
                self.mode = Mode::Gb;
                let (text, _) = encoding_rs::GB18030.decode_without_bom_handling(&probe[..len]);
                out.extend_from_slice(text.as_bytes());
                self.pending.drain(..len);
                // 余量按 GB 继续（探针里可能还跟着整字符）。
                while !self.pending.is_empty() {
                    let byte = self.pending.remove(0);
                    self.push_gb(byte, out);
                }
                return Some(Encoding::Gb18030);
            }
            _ => {}
        }

        // 两种编码都判不通：报掉首字节，从下一个字节重新起探。
        self.pending.drain(..1);
        out.extend_from_slice(REPLACEMENT);
        if self.pending.is_empty() {
            None
        } else {
            self.try_resolve(out)
        }
    }

    /// 单字节推进 GB18030 状态机。
    fn push_gb(&mut self, byte: u8, out: &mut Vec<u8>) {
        if self.pending.is_empty() {
            if byte < 0x80 {
                // 控制序列与 ASCII 文本：原样放行，解析器自己处理。
                out.push(byte);
            } else if (0x81..=0xFE).contains(&byte) {
                self.pending.push(byte);
            } else {
                // 0x80 / 0xFF 在 GB18030 里不是首字节：如实报替换符。
                out.extend_from_slice(REPLACEMENT);
            }
            return;
        }

        match self.pending.len() {
            1 if (0x30..=0x39).contains(&byte) => {
                // 四字节段的第二字节是数字（GB18030 独有的那段）。
                self.pending.push(byte);
            }
            1 if (0x40..=0xFE).contains(&byte) && byte != 0x7F => {
                self.pending.push(byte);
                self.flush(out);
            }
            2 | 3 if (0x30..=0x39).contains(&byte) => {
                self.pending.push(byte);
                if self.pending.len() == 4 {
                    self.flush(out);
                }
            }
            _ => {
                // 非法尾字节：先把前缀当作坏字符报掉，再把这个字节当首字节重来
                //（它可能是下一个字符的开头，直接吞掉会连带丢字）。
                self.pending.clear();
                out.extend_from_slice(REPLACEMENT);
                self.push_gb(byte, out);
            }
        }
    }

    /// 凑齐一个完整字符，转成 UTF-8 写出。
    fn flush(&mut self, out: &mut Vec<u8>) {
        let bytes = std::mem::take(&mut self.pending);
        let (text, _) = encoding_rs::GB18030.decode_without_bom_handling(&bytes);
        out.extend_from_slice(text.as_bytes());
    }
}

/// 把一段文本按会话编码转成要写进 PTY 的字节。
///
/// Auto 档在探测出 GB 之前按 UTF-8 发 —— 探测一旦判给 GB，把手已切到
/// Gb18030，这里自然走 GB 分支，不需要单独记忆。GB18030 表达不了的字符
///（编码器会退化成数字字符引用 `&#...;`）在实践中只出现在生僻 emoji 上，
/// 不影响绝大多数输入 —— 好过直接拒绝发送。
pub fn encode_input(text: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Auto | Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Gb18030 => encoding_rs::GB18030.encode(text).0.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解一块字节，忽略「Auto 探测到了 GB」的回报（强制档的测试用）。
    fn decode(raw: &[u8], encoding: Encoding) -> String {
        decode_detected(raw, encoding).0
    }

    /// 解一块字节，同时把「Auto 探测到了 GB」的回报带出来。
    fn decode_detected(raw: &[u8], encoding: Encoding) -> (String, Option<Encoding>) {
        let mut out = Vec::new();
        let detected = OutputDecoder::default().push(raw, encoding, &mut out);
        (
            String::from_utf8(out).expect("解码结果必须是合法 UTF-8"),
            detected,
        )
    }

    #[test]
    fn utf8_passes_through_untouched() {
        let raw = "中文 abc\x1b[31m红\x1b[0m".as_bytes();
        assert_eq!(decode(raw, Encoding::Utf8), "中文 abc\x1b[31m红\x1b[0m");
    }

    #[test]
    fn gbk_two_byte_characters_become_utf8() {
        // 「中文」= D6 D0 CE C4（GBK）
        assert_eq!(decode(&[0xD6, 0xD0, 0xCE, 0xC4], Encoding::Gb18030), "中文");
    }

    #[test]
    fn escape_sequences_survive_gbk_mode() {
        // 控制序列是纯 7 位：不能被当成 GB 字符吞掉。
        let raw = b"\x1b[1;31mOK\x1b[0m";
        assert_eq!(decode(raw, Encoding::Gb18030), "\x1b[1;31mOK\x1b[0m");
    }

    #[test]
    fn characters_split_across_chunks_are_reassembled() {
        let mut decoder = OutputDecoder::default();
        let mut out = Vec::new();
        decoder.push(&[0xD6], Encoding::Gb18030, &mut out);
        assert!(out.is_empty(), "半截字符不该先输出，否则会先闪一个替换符");
        decoder.push(&[0xD0], Encoding::Gb18030, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "中");
    }

    #[test]
    fn invalid_trailing_byte_keeps_following_text() {
        // D6 后面跟 ESC（0x1B）不是合法尾字节：报掉 D6，ESC 序列照常解析。
        let raw = [0xD6, 0x1B, b'[', b'0', b'm'];
        assert_eq!(decode(&raw, Encoding::Gb18030), "\u{FFFD}\x1b[0m");
    }

    // ── Auto 档 ──────────────────────────────────────────────────────────

    #[test]
    fn auto_passes_valid_utf8_through() {
        let raw = "2026-10-17 生产管理平台 任务调度 中英混排 abc\x1b[31m红\x1b[0m\n";
        let (text, detected) = decode_detected(raw.as_bytes(), Encoding::Auto);
        assert_eq!(text, raw);
        assert_eq!(detected, None, "合法 UTF-8 流不该触发切换");
    }

    #[test]
    fn auto_detects_gbk_and_reports_it() {
        // 「中文」的 GBK 字节。
        let (text, detected) = decode_detected(&[0xD6, 0xD0, 0xCE, 0xC4], Encoding::Auto);
        assert_eq!(text, "中文");
        assert_eq!(detected, Some(Encoding::Gb18030), "判给 GB 后必须回报");
    }

    #[test]
    fn auto_keeps_ascii_and_escapes_in_gbk_streams() {
        let mut raw = b"2026-10-17 INFO \x1b[1;31m".to_vec();
        raw.extend_from_slice(&[0xD6, 0xD0]);
        let (text, detected) = decode_detected(&raw, Encoding::Auto);
        assert!(text.starts_with("2026-10-17 INFO \x1b[1;31m"));
        assert!(text.contains("中"));
        assert_eq!(detected, Some(Encoding::Gb18030));
    }

    #[test]
    fn auto_resolves_gbk_split_across_chunks() {
        let mut decoder = OutputDecoder::default();
        let mut out = Vec::new();
        assert_eq!(decoder.push(&[0xD6], Encoding::Auto, &mut out), None);
        assert!(out.is_empty(), "半截字符两边都判不出，必须等");
        assert_eq!(
            decoder.push(&[0xD0], Encoding::Auto, &mut out),
            Some(Encoding::Gb18030)
        );
        assert_eq!(String::from_utf8(out).unwrap(), "中");
    }

    #[test]
    fn auto_is_sticky_after_detection() {
        // 探测判给 GB 后，后续 GBK 字节在 Auto 档下继续按 GB 解 —— 会话级黏住。
        let mut decoder = OutputDecoder::default();
        let mut out = Vec::new();
        decoder.push(&[0xD6, 0xD0], Encoding::Auto, &mut out);
        decoder.push(&[0xCE, 0xC4], Encoding::Auto, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "中文");
    }

    #[test]
    fn auto_recovery_when_neither_encoding_fits() {
        // 0x80 / 0xFF 两边都不收：如实报替换符，后续文本照常。
        let mut raw = vec![0x80];
        raw.extend_from_slice(b"ok");
        let (text, detected) = decode_detected(&raw, Encoding::Auto);
        assert_eq!(text, "\u{FFFD}ok");
        assert_eq!(detected, None);
    }

    #[test]
    fn auto_resets_when_user_switches_encoding() {
        let mut decoder = OutputDecoder::default();
        let mut out = Vec::new();
        // 第一步就把 GBK 流判成了 GB（「中」已经合法解出并输出）。
        decoder.push(&[0xD6, 0xD0], Encoding::Auto, &mut out);
        // 切到 UTF-8 再切回 Auto：黏住的 GB 状态必须被清掉，重新按字节流判。
        decoder.push(b"reset", Encoding::Utf8, &mut out);
        decoder.push(&[0xD6, 0xD0, 0xCE, 0xC4], Encoding::Auto, &mut out);
        let text = String::from_utf8(std::mem::take(&mut out)).unwrap();
        assert_eq!(text, "中reset中文");
    }

    #[test]
    fn input_roundtrips_between_encodings() {
        let gb = encode_input("中文", Encoding::Gb18030);
        assert_eq!(gb, vec![0xD6, 0xD0, 0xCE, 0xC4], "GBK 编码的「中文」");
        assert_eq!(decode(&gb, Encoding::Gb18030), "中文");
        // ASCII 在所有档位下逐字节相同：命令、快捷键不受影响。
        for encoding in Encoding::ALL {
            assert_eq!(encode_input("ls -la\r", encoding), b"ls -la\r");
        }
    }

    #[test]
    fn value_roundtrip_covers_every_variant() {
        for encoding in Encoding::ALL {
            assert_eq!(Encoding::from_value(encoding.value()), Some(encoding));
        }
        assert_eq!(Encoding::from_value("gbk"), None, "未知值必须回落，不许猜");
    }

    #[test]
    fn switching_encoding_drops_stale_prefix() {
        let mut decoder = OutputDecoder::default();
        let mut out = Vec::new();
        decoder.push(&[0xD6], Encoding::Gb18030, &mut out);
        decoder.push(b"ok", Encoding::Utf8, &mut out);
        assert_eq!(String::from_utf8(std::mem::take(&mut out)).unwrap(), "ok");
        // 再切回 GB：那半截 D6 已经被丢掉，不会和新数据拼成一个错字。
        decoder.push(&[0xD6, 0xD0], Encoding::Gb18030, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "中");
    }
}
