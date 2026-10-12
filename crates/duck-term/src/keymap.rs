//! 按键 → 终端字节序列。
//!
//! 这一层**刻意不依赖 GPUI**：调用方把一次按键描述成 [`KeyStroke`]（键名 + 可插入文本 +
//! 三个修饰键），这里给出该写进 PTY 的字节。好处是这套转义规则能被单测逐条钉住 ——
//! 否则「方向键不动」「Ctrl-C 不响应」这类问题只能靠手按来发现。
//!
//! 键名沿用 GPUI 的 `Keystroke::key` 拼写（`enter` / `backspace` / `up` / `f5` …），
//! 于是调用点是一次直译，不需要中间映射表。
//!
//! # 编码规则（xterm 事实标准）
//!
//! | 情况 | 结果 |
//! | --- | --- |
//! | ⌘（macOS）/ ⊞（Windows）+ 任意 | 拒收 —— 快捷键的地盘，终端不认 |
//! | Ctrl + 字母 | `0x01..=0x1a`（`Ctrl-A`=1 … `Ctrl-Z`=26），`Ctrl-[`=27 / `Ctrl-\`=28 / `Ctrl-]`=29 |
//! | Alt + 任意 | 前置 `ESC`（meta 前缀），其余同不带 Alt |
//! | 方向 / Home / End | `CSI A/B/C/D`、`CSI H/F`；带修饰键时插参数 `CSI 1;<n> A` |
//! | PgUp / PgDn / Ins / Del | `CSI 5~ / 6~ / 2~ / 3~`；带修饰键时 `CSI <code>;<n> ~` |
//! | F1..F4 | `SS3 P Q R S`；F5..F12 → `CSI 15~..24~` |
//! | Enter | `CR`（`\r`）。**不是** `LF`：行规程会把 CR 翻成 NL |
//! | Shift+Tab | `CSI Z`（CBT，反向制表） |
//! | Backspace | `DEL`（`0x7f`） |
//! | 其余可打印文本 | 原样（IME 上屏、中文、emoji 都走这条） |
//!
//! 修饰键参数 `n` = `1 + shift + 2·alt + 4·ctrl`（xterm 约定）。

/// 一次按键。
#[derive(Clone, Copy, Debug)]
pub struct KeyStroke<'a> {
    /// 键名，GPUI `Keystroke::key` 的拼写（小写）。
    pub key: &'a str,
    /// 该按键能直接插入的文本（`KeyDownEvent::keystroke.key_char` 或输入法上屏结果）。
    pub text: Option<&'a str>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// 平台修饰键（macOS ⌘ / Windows ⊞）。**终端永远不认它** —— 见 [`encode`] 第 0 步。
    ///
    /// 单独一个字段而不是塞进 `text` 的判空里：漏填会编译报错，比「靠调用点自觉」可靠。
    pub platform: bool,
}

impl<'a> KeyStroke<'a> {
    /// 只有键名、无修饰键的最小构造（测试与简单场景用）。
    #[must_use]
    pub const fn key(key: &'a str) -> Self {
        Self {
            key,
            text: None,
            ctrl: false,
            alt: false,
            shift: false,
            platform: false,
        }
    }
}

/// 编码结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Encoded {
    /// 这串字节应当写进 PTY。
    Bytes(Vec<u8>),
    /// 终端不认这个组合键，交回外层（例如 `⌘-W` 关标签页）。
    Ignored,
}

/// 把一次按键编码成写进 PTY 的字节。
#[must_use]
pub fn encode(stroke: KeyStroke<'_>) -> Encoded {
    let KeyStroke {
        key,
        text,
        ctrl,
        alt,
        shift,
        platform,
    } = stroke;

    // 0) 平台修饰键（macOS ⌘ / Windows ⊞）是**快捷键**的地盘，终端一律不认。
    //
    //    调用方只有在「宿主没有任何绑定命中」时才会走到编码层（gpui 的派发顺序是
    //    action 绑定优先，命中的话事件根本到不了 `on_key_down`）。所以这里出现 ⌘ 组合，
    //    含义就是「用户按了一个本应用没绑的快捷键」—— 若继续往下走，`⌘A` / `⌘S` / `⌘Q`
    //    会在 shell 里凭空多出一个 `a` / `s` / `q`。拒收它，并让调用方也**不吞**事件。
    if platform {
        return Encoded::Ignored;
    }

    // 1) Ctrl 组合优先于一切文本：`Ctrl-C` 不该插入字母 `c`。
    //    Ctrl+Shift+字母 仍是同一个控制字节（终端协议里没有区分）。
    if ctrl {
        if let Some(byte) = control_byte(key) {
            // `Ctrl-Alt-X` = meta 前缀 + 控制字节。
            return Encoded::Bytes(if alt { vec![0x1b, byte] } else { vec![byte] });
        }
    }

    // 2) 功能键 / 编辑键的固定序列。带 Alt 时统一走「ESC 前缀 + 该序列」，
    //    只有方向键与 Home/End 例外 —— 它们有专门的修饰键参数形式，
    //    两者都发会让应用收到两次移动。
    if let Some(sequence) = named_key(key, shift, alt, ctrl) {
        return Encoded::Bytes(prefix_alt(sequence.into_bytes(), alt, key));
    }

    // 3) 其余：能插入什么就写什么（含中文、emoji、空格）。Alt 仍要加 meta 前缀。
    if !ctrl {
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            let mut bytes = Vec::with_capacity(text.len() + 1);
            if alt {
                bytes.push(0x1b);
            }
            bytes.extend_from_slice(text.as_bytes());
            return Encoded::Bytes(bytes);
        }
    }

    Encoded::Ignored
}

/// `Ctrl-<字母 / 符号>` 对应的控制字节。
///
/// `Ctrl-A`..`Ctrl-Z` → 1..26；`Ctrl-[` / `Ctrl-\` / `Ctrl-]` / `Ctrl-^` / `Ctrl-_`
/// 接着排到 27..31。`Ctrl-@`（即 `Ctrl-2`）是 NUL。
fn control_byte(key: &str) -> Option<u8> {
    let mut chars = key.chars();
    let (first, rest) = (chars.next()?, chars.next());
    if rest.is_some() {
        return None;
    }
    // 键名大小写不敏感（GPUI 给小写，但别让上层的大小写差异静默失效）。
    let lower = first.to_ascii_lowercase();
    Some(match lower {
        'a'..='z' => lower as u8 - b'a' + 1,
        '@' | '2' => 0,
        '[' | '3' => 27,
        '\\' | '4' => 28,
        ']' | '5' => 29,
        '^' | '6' => 30,
        '_' | '7' | '/' => 31,
        ' ' => 0,
        _ => return None,
    })
}

/// 具名键的字节序列；不认识则 `None`（交给文本兜底）。
fn named_key(key: &str, shift: bool, alt: bool, ctrl: bool) -> Option<String> {
    let modifier = modifier_param(shift, alt, ctrl);
    // 单字符键名（`a`、`1`）不是具名键。
    let mut chars = key.chars();
    if chars.next().is_some() && chars.next().is_none() {
        return None;
    }

    Some(match key {
        "enter" | "return" => "\r".to_owned(),
        "tab" if shift => "\x1b[Z".to_owned(),
        "tab" => "\t".to_owned(),
        "backspace" => "\x7f".to_owned(),
        "escape" => "\x1b".to_owned(),
        // 方向键：xterm 的 `CSI 1;<n> A`；无修饰键时省掉 `1;`（老应用更认短形式）。
        "up" | "down" | "right" | "left" => {
            let letter = match key {
                "up" => 'A',
                "down" => 'B',
                "right" => 'C',
                _ => 'D',
            };
            if modifier == 1 {
                format!("\x1b[{letter}")
            } else {
                format!("\x1b[1;{modifier}{letter}")
            }
        }
        "home" | "end" => {
            let letter = if key == "home" { 'H' } else { 'F' };
            if modifier == 1 {
                format!("\x1b[{letter}")
            } else {
                format!("\x1b[1;{modifier}{letter}")
            }
        }
        // 波浪号族：`CSI <code> ~`，带修饰键时 `CSI <code>;<n> ~`。
        "insert" | "delete" | "pageup" | "pagedown" => {
            let code = match key {
                "insert" => 2,
                "delete" => 3,
                "pageup" => 5,
                _ => 6,
            };
            if modifier == 1 {
                format!("\x1b[{code}~")
            } else {
                format!("\x1b[{code};{modifier}~")
            }
        }
        // F1..F4 走 SS3（`ESC O P`…）；F5 起换成 CSI 数字。
        "f1" | "f2" | "f3" | "f4" => {
            let letter = match key {
                "f1" => 'P',
                "f2" => 'Q',
                "f3" => 'R',
                _ => 'S',
            };
            if modifier == 1 {
                format!("\x1bO{letter}")
            } else {
                format!("\x1b[1;{modifier}{letter}")
            }
        }
        "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            let code = match key {
                "f5" => 15,
                "f6" => 17,
                "f7" => 18,
                "f8" => 19,
                "f9" => 20,
                "f10" => 21,
                "f11" => 23,
                _ => 24,
            };
            if modifier == 1 {
                format!("\x1b[{code}~")
            } else {
                format!("\x1b[{code};{modifier}~")
            }
        }
        _ => return None,
    })
}

/// xterm 的修饰键参数：`1 + shift + 2·alt + 4·ctrl`。
const fn modifier_param(shift: bool, alt: bool, ctrl: bool) -> u8 {
    1 + shift as u8 + 2 * (alt as u8) + 4 * (ctrl as u8)
}

/// Alt 的前缀。
///
/// 方向键 / Home / End 已经在序列里带了修饰键参数（`CSI 1;3A`），再补一个 `ESC`
/// 会被应用读成「Alt-Esc 然后 Up」——它们**不**走这条。
fn prefix_alt(sequence: Vec<u8>, alt: bool, key: &str) -> Vec<u8> {
    let carries_modifier = matches!(
        key,
        "up" | "down"
            | "right"
            | "left"
            | "home"
            | "end"
            | "insert"
            | "delete"
            | "pageup"
            | "pagedown"
            | "f1"
            | "f2"
            | "f3"
            | "f4"
            | "f5"
            | "f6"
            | "f7"
            | "f8"
            | "f9"
            | "f10"
            | "f11"
            | "f12"
    );
    if !alt || carries_modifier {
        return sequence;
    }
    let mut bytes = Vec::with_capacity(sequence.len() + 1);
    bytes.push(0x1b);
    bytes.extend_from_slice(&sequence);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(key: &str, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
        match encode(KeyStroke {
            key,
            text: None,
            ctrl,
            alt,
            shift,
            platform: false,
        }) {
            Encoded::Bytes(bytes) => bytes,
            Encoded::Ignored => panic!("{key} 应当被编码"),
        }
    }

    #[test]
    fn control_letters_map_to_one_through_twenty_six() {
        assert_eq!(bytes("a", true, false, false), vec![0x01]);
        assert_eq!(bytes("c", true, false, false), vec![0x03]);
        assert_eq!(bytes("d", true, false, false), vec![0x04]);
        assert_eq!(bytes("z", true, false, false), vec![0x1a]);
        // 大写键名也不该静默失效。
        assert_eq!(bytes("C", true, false, false), vec![0x03]);
    }

    /// 行编辑快捷键（画布层的 `⌘←` / `⌘→` / `⌘⌫` / `⌘⌦` 动作）发出去的字节。
    ///
    /// 这几个动作**只**做一件事：把 macOS 手感翻译成 shell 行编辑认的控制字节。
    /// 钉在这里，是为了让「映射结果」不必靠真机手按来确认。
    #[test]
    fn line_editing_actions_map_to_the_expected_control_bytes() {
        assert_eq!(bytes("a", true, false, false), vec![0x01]); // ⌘← → Ctrl-A 行首
        assert_eq!(bytes("e", true, false, false), vec![0x05]); // ⌘→ → Ctrl-E 行尾
        assert_eq!(bytes("u", true, false, false), vec![0x15]); // ⌘⌫ → Ctrl-U 删到行首
        assert_eq!(bytes("k", true, false, false), vec![0x0b]); // ⌘⌦ → Ctrl-K 删到行尾
    }

    /// `⌥⌦`（`DeleteWordForward`）= `ESC d`（readline 删到词尾）。
    ///
    /// 刻意**不是** `CSI 3;3~`：那是 xterm 的 Alt-Delete。zed 的 `"Terminal"` 上下文
    /// 选的是 Terminal.app 兼容的 `\u001bd`，这里跟随它。
    #[test]
    fn alt_word_forward_delete_is_escape_then_d() {
        let encoded = encode(KeyStroke {
            key: "d",
            text: Some("d"),
            ctrl: false,
            alt: true,
            shift: false,
            platform: false,
        });
        assert_eq!(encoded, Encoded::Bytes(b"\x1bd".to_vec()));
    }

    /// ⌘（`platform`）+ 任意键一律拒收。
    ///
    /// 这条是**回归守卫**：曾经把 ⌘A / ⌘S 当普通字母写进了 shell。根因是编码层
    /// 看不到 `platform`，只能靠 UI 层自觉 —— 现在规则进了类型（漏填编译不过）。
    #[test]
    fn platform_modified_strokes_are_never_encoded() {
        for (key, text) in [
            ("a", Some("a")),
            ("s", Some("s")),
            ("v", Some("v")),
            ("left", None),
            ("enter", None),
        ] {
            assert_eq!(
                encode(KeyStroke {
                    key,
                    text,
                    ctrl: false,
                    alt: false,
                    shift: false,
                    platform: true,
                }),
                Encoded::Ignored,
                "⌘+{key} 不该被编码"
            );
        }
        // 同时按 ⌘ 与 Ctrl 也一样拒收（⌃⌘A 这类组合仍属快捷键地盘）。
        assert_eq!(
            encode(KeyStroke {
                key: "a",
                text: None,
                ctrl: true,
                alt: false,
                shift: false,
                platform: true,
            }),
            Encoded::Ignored
        );
    }

    #[test]
    fn control_brackets_and_backslash_follow_the_table() {
        assert_eq!(bytes("[", true, false, false), vec![0x1b]);
        assert_eq!(bytes("\\", true, false, false), vec![0x1c]);
        assert_eq!(bytes("]", true, false, false), vec![0x1d]);
    }

    #[test]
    fn arrows_without_modifiers_use_the_short_form() {
        assert_eq!(bytes("up", false, false, false), b"\x1b[A");
        assert_eq!(bytes("down", false, false, false), b"\x1b[B");
        assert_eq!(bytes("right", false, false, false), b"\x1b[C");
        assert_eq!(bytes("left", false, false, false), b"\x1b[D");
    }

    #[test]
    fn arrows_with_modifiers_carry_the_parameter() {
        // Alt-Up → 1 + 2 = 3；Ctrl-Right → 1 + 4 = 5；Shift-Left → 2。
        assert_eq!(bytes("up", false, true, false), b"\x1b[1;3A");
        assert_eq!(bytes("right", true, false, false), b"\x1b[1;5C");
        assert_eq!(bytes("left", false, false, true), b"\x1b[1;2D");
    }

    #[test]
    fn tilde_family_uses_the_documented_codes() {
        assert_eq!(bytes("pageup", false, false, false), b"\x1b[5~");
        assert_eq!(bytes("pagedown", false, false, false), b"\x1b[6~");
        assert_eq!(bytes("delete", false, false, false), b"\x1b[3~");
        assert_eq!(bytes("insert", false, false, false), b"\x1b[2~");
        assert_eq!(bytes("delete", true, false, false), b"\x1b[3;5~");
    }

    #[test]
    fn function_keys_split_between_ss3_and_csi() {
        assert_eq!(bytes("f1", false, false, false), b"\x1bOP");
        assert_eq!(bytes("f4", false, false, false), b"\x1bOS");
        assert_eq!(bytes("f5", false, false, false), b"\x1b[15~");
        assert_eq!(bytes("f12", false, false, false), b"\x1b[24~");
    }

    #[test]
    fn enter_is_carriage_return_not_newline() {
        assert_eq!(bytes("enter", false, false, false), b"\r");
    }

    #[test]
    fn backspace_sends_del_and_shift_tab_sends_cbt() {
        assert_eq!(bytes("backspace", false, false, false), b"\x7f");
        assert_eq!(bytes("tab", false, false, false), b"\t");
        assert_eq!(bytes("tab", false, false, true), b"\x1b[Z");
    }

    #[test]
    fn alt_letters_get_an_escape_prefix() {
        let encoded = encode(KeyStroke {
            key: "f",
            text: Some("f"),
            ctrl: false,
            alt: true,
            shift: false,
            platform: false,
        });
        assert_eq!(encoded, Encoded::Bytes(b"\x1bf".to_vec()));
    }

    #[test]
    fn printable_text_is_passed_through_untouched() {
        let encoded = encode(KeyStroke {
            key: "space",
            text: Some(" "),
            ctrl: false,
            alt: false,
            shift: false,
            platform: false,
        });
        assert_eq!(encoded, Encoded::Bytes(b" ".to_vec()));

        // 输入法上屏的中文走同一条路。
        let encoded = encode(KeyStroke {
            key: "unidentified",
            text: Some("终端"),
            ctrl: false,
            alt: false,
            shift: false,
            platform: false,
        });
        assert_eq!(encoded, Encoded::Bytes("终端".as_bytes().to_vec()));
    }

    #[test]
    fn plain_letters_without_text_are_ignored() {
        // 没有 `text` 的单字母键说明这是一次「只按了修饰键」或输入法未上屏的事件，
        // 不该凭空插入字母。
        assert_eq!(encode(KeyStroke::key("a")), Encoded::Ignored);
        assert_eq!(encode(KeyStroke::key("shift")), Encoded::Ignored);
    }
}
