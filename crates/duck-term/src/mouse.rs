//! 鼠标上报：把宿主的鼠标事件编成终端应用要的 CSI 序列。
//!
//! 与键盘编码（[`crate::encode`]）同理，**序列只在这里生成一处**：宿主只负责把
//! 「像素 → 网格坐标」「点的是哪个键、按了哪些修饰键」翻译成本模块的输入，
//! 而「发什么字节」这件事留在内核，可单测、也只在升级 vendored crate 时改一处。
//!
//! # 三种模式（xterm 的 `CSI ? 1000 / 1002 / 1003`）
//!
//! - `MOUSE_REPORT_CLICK`（1000）：只上报按下 / 抬起；
//! - `MOUSE_DRAG`（1002）：按下时移动也上报（拖拽）；
//! - `MOUSE_MOTION`（1003）：任何移动都上报。
//!
//! 编码格式另有两条：`SGR_MOUSE`（`CSI ? 1006`，`CSI < b ; x ; y M/m`）与
//! `UTF8_MOUSE`（`CSI ? 1005`，坐标超 95 用两字节 UTF-8）。都不开时退回
//! 最老的 `CSI M Cb Cx Cy`（单字节，坐标上限 223）。
//!
//! 参考实现：alacritty 的 `input.rs`，与 zed 的
//! `crates/terminal/src/mappings/mouse.rs`（本项目 vendored 的 alacritty 没有
//! 宿主侧的上报胶水，只保留了终端状态机）。

use alacritty_terminal::term::TermMode;

/// 参与上报的鼠标按钮。
///
/// 滚轮**不在这里**：它在协议里是两种「按钮码」（64 / 65），与左中右三键的
/// 按下 / 抬起语义不同，因此单列在 [`MouseAction::Scroll`] 里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    /// 左键。
    Left,
    /// 中键。
    Middle,
    /// 右键。
    Right,
}

impl MouseButton {
    /// xterm 的按钮码。
    const fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
        }
    }
}

/// 一次鼠标事件的语义（宿主把平台事件翻译成它）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    /// 某个按钮按下。
    Press(MouseButton),
    /// 某个按钮抬起。
    Release(MouseButton),
    /// 指针移动；`held` 是此刻按住的按钮（`None` = 没有按下任何键）。
    Motion { held: Option<MouseButton> },
    /// 滚轮；`up` 为 `true` 表示向上滚。
    Scroll { up: bool },
}

/// 鼠标事件的修饰键（xterm 的位 4 / 8 / 16）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// 移动事件的按钮码偏移（xterm：`b + 32`）。
const MOTION_OFFSET: u8 = 32;
/// 移动但没有任何按钮按住时的按钮码（`3 + 32`）。
const NONE_MOVE: u8 = 3 + MOTION_OFFSET;
/// 滚轮向上 / 向下的按钮码。
const SCROLL_UP: u8 = 64;
const SCROLL_DOWN: u8 = 65;

/// 编码格式：SGR 用 `M` / `m` 收尾（动作为独立参数），旧格式把按钮码与坐标
/// 挤在一个 `CSI M` 之后的三个字节里。
#[derive(Clone, Copy)]
enum MouseFormat {
    /// `CSI < b ; x ; y M/m`。
    Sgr,
    /// `CSI M Cb Cx Cy`；`utf8` 为真时坐标超 95 用两字节编码（`CSI ? 1005`）。
    Normal { utf8: bool },
}

fn format_of(mode: TermMode) -> MouseFormat {
    if mode.contains(TermMode::SGR_MOUSE) {
        MouseFormat::Sgr
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        MouseFormat::Normal { utf8: true }
    } else {
        MouseFormat::Normal { utf8: false }
    }
}

/// 终端应用是否要求宿主上报鼠标事件（任意一种上报模式）。
#[must_use]
pub fn reports_mouse(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_MODE)
}

/// 终端应用是否要求上报**指针移动**（拖拽或全量移动）。
#[must_use]
pub fn reports_motion(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
}

/// 把一次鼠标事件编成要写进 PTY 的字节；应用没要求上报时返回 `None`。
///
/// `column` 是 0 基网格列，`line` 是 0 基网格行（相对当前视口）。行号为负
///（指针落在视口上方的历史区）时不上报：协议里的坐标是 1 基正整数，
/// 发一个负行号会把应用带进未定义行为。
#[must_use]
pub fn encode(
    action: MouseAction,
    column: usize,
    line: i32,
    modifiers: MouseModifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if !reports_mouse(mode) {
        return None;
    }
    let (code, down) = match action {
        MouseAction::Press(button) => (button.code(), true),
        MouseAction::Release(button) => (button.code(), false),
        MouseAction::Motion { held } => {
            if mode.contains(TermMode::MOUSE_MOTION) {
                // 1003：任何移动都上报，没按键时用 `NoneMove`。
                (held.map_or(NONE_MOVE, |b| b.code() + MOTION_OFFSET), true)
            } else if mode.contains(TermMode::MOUSE_DRAG) {
                // 1002：只有拖着按钮的移动才算数（否则每帧一次光标移动都发给应用）。
                (held?.code() + MOTION_OFFSET, true)
            } else {
                // 只有 1000（点击）时移动不上报。
                return None;
            }
        }
        MouseAction::Scroll { up } => (if up { SCROLL_UP } else { SCROLL_DOWN }, true),
    };
    if line < 0 {
        return None;
    }

    let mut mods = 0u8;
    if modifiers.shift {
        mods += 4;
    }
    if modifiers.alt {
        mods += 8;
    }
    if modifiers.control {
        mods += 16;
    }

    match format_of(mode) {
        MouseFormat::Sgr => Some(sgr_report(column, line, code + mods, down)),
        MouseFormat::Normal { utf8 } => {
            // 旧格式没有「按下 / 抬起」两个参数：抬起把按钮码固定成 `3 + mods`。
            let released = 3 + mods;
            let code = if down { code + mods } else { released };
            normal_report(column, line, code, utf8)
        }
    }
}

/// `CSI < b ; x ; y M`（按下）/ `CSI < b ; x ; y m`（抬起）。
fn sgr_report(column: usize, line: i32, button: u8, down: bool) -> Vec<u8> {
    let terminator = if down { 'M' } else { 'm' };
    format!("\x1b[<{};{};{}{}", button, column + 1, line + 1, terminator).into_bytes()
}

/// `CSI M Cb Cx Cy`（坐标各加 33；UTF-8 模式下 ≥ 95 的坐标用两字节）。
fn normal_report(column: usize, line: i32, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let max_point: usize = if utf8 { 2015 } else { 223 };
    let line = usize::try_from(line).ok()?;
    if line >= max_point || column >= max_point {
        return None;
    }

    let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];
    let encode_pos = |pos: usize| -> Vec<u8> {
        let pos = 32 + 1 + pos;
        vec![(0xC0 + pos / 64) as u8, (0x80 + (pos & 63)) as u8]
    };
    if utf8 && column >= 95 {
        msg.append(&mut encode_pos(column));
    } else {
        msg.push(32 + 1 + column as u8);
    }
    if utf8 && line >= 95 {
        msg.append(&mut encode_pos(line));
    } else {
        msg.push(32 + 1 + line as u8);
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods() -> MouseModifiers {
        MouseModifiers::default()
    }

    #[test]
    fn nothing_is_reported_without_a_mouse_mode() {
        let mode = TermMode::SHOW_CURSOR;
        assert!(!reports_mouse(mode));
        assert_eq!(
            encode(MouseAction::Press(MouseButton::Left), 0, 0, mods(), mode),
            None
        );
        assert_eq!(
            encode(MouseAction::Scroll { up: true }, 0, 0, mods(), mode),
            None
        );
    }

    #[test]
    fn sgr_press_and_release_use_m_and_m() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        // 坐标是 1 基：网格 (列 3, 行 0) → `;4;1`。
        assert_eq!(
            encode(MouseAction::Press(MouseButton::Left), 3, 0, mods(), mode),
            Some(b"\x1b[<0;4;1M".to_vec())
        );
        assert_eq!(
            encode(MouseAction::Release(MouseButton::Left), 3, 0, mods(), mode),
            Some(b"\x1b[<0;4;1m".to_vec())
        );
        // 右键 = 按钮码 2；中键 = 1。
        assert_eq!(
            encode(MouseAction::Press(MouseButton::Right), 0, 0, mods(), mode),
            Some(b"\x1b[<2;1;1M".to_vec())
        );
        assert_eq!(
            encode(MouseAction::Press(MouseButton::Middle), 0, 0, mods(), mode),
            Some(b"\x1b[<1;1;1M".to_vec())
        );
    }

    #[test]
    fn modifiers_add_the_xterm_bits() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let all = MouseModifiers {
            shift: true,
            alt: true,
            control: true,
        };
        // 4 + 8 + 16 = 28，左键 0 → 按钮码 28。
        assert_eq!(
            encode(MouseAction::Press(MouseButton::Left), 0, 0, all, mode),
            Some(b"\x1b[<28;1;1M".to_vec())
        );
    }

    #[test]
    fn scroll_codes_follow_xterm() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            encode(MouseAction::Scroll { up: true }, 1, 2, mods(), mode),
            Some(b"\x1b[<64;2;3M".to_vec())
        );
        assert_eq!(
            encode(MouseAction::Scroll { up: false }, 1, 2, mods(), mode),
            Some(b"\x1b[<65;2;3M".to_vec())
        );
    }

    #[test]
    fn motion_requires_motion_or_drag_mode() {
        // 1003（全量移动）：没按键也上报，用 `NoneMove`（3 + 32 = 35）。
        let motion = TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE;
        assert!(reports_motion(motion));
        assert_eq!(
            encode(MouseAction::Motion { held: None }, 0, 0, mods(), motion),
            Some(b"\x1b[<35;1;1M".to_vec())
        );
        assert_eq!(
            encode(
                MouseAction::Motion {
                    held: Some(MouseButton::Left)
                },
                0,
                0,
                mods(),
                motion
            ),
            Some(b"\x1b[<32;1;1M".to_vec())
        );

        // 1002（拖拽）：只有按住按钮的移动才上报。
        let drag = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        assert_eq!(
            encode(MouseAction::Motion { held: None }, 0, 0, mods(), drag),
            None
        );
        assert_eq!(
            encode(
                MouseAction::Motion {
                    held: Some(MouseButton::Left)
                },
                0,
                0,
                mods(),
                drag
            ),
            Some(b"\x1b[<32;1;1M".to_vec())
        );

        // 只有 1000（点击）时移动一律不上报。
        let click = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert!(!reports_motion(click));
        assert_eq!(
            encode(
                MouseAction::Motion {
                    held: Some(MouseButton::Left)
                },
                0,
                0,
                mods(),
                click
            ),
            None
        );
    }

    #[test]
    fn negative_line_is_dropped() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            encode(MouseAction::Press(MouseButton::Left), 0, -1, mods(), mode),
            None
        );
    }

    #[test]
    fn legacy_format_packs_three_bytes() {
        // 1000 + 1005（UTF-8）：坐标各 + 33，逗号式无 `M` / `m`。
        let utf8 = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        assert_eq!(
            encode(MouseAction::Press(MouseButton::Left), 1, 2, mods(), utf8),
            Some(vec![0x1b, b'[', b'M', 32, 34, 35])
        );

        // 经典 `CSI M`（1000）：抬起把按钮码固定成 3。
        let plain = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            encode(MouseAction::Release(MouseButton::Left), 0, 0, mods(), plain),
            Some(vec![0x1b, b'[', b'M', 35, 33, 33])
        );
    }

    #[test]
    fn utf8_coordinates_use_two_bytes_above_95() {
        let utf8 = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        let msg =
            encode(MouseAction::Press(MouseButton::Left), 100, 0, mods(), utf8).expect("应能编码");
        // 前三字节是 `ESC [ M`，第 4 字节是按钮码，坐标从第 5 字节起。
        // 坐标 100 → 32 + 1 + 100 = 133 → 0xC0 + 133/64 = 0xC2, 0x80 + 133%64 = 0x85。
        assert_eq!(&msg[4..6], &[0xC2, 0x85]);
    }
}
