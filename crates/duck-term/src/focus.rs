//! 焦点事件上报：画布获得 / 失去键盘焦点时，通知终端应用。
//!
//! 与键盘编码（[`crate::encode`]）、鼠标上报（[`crate::mouse`]）同理，
//! **序列只在这里生成一处**：宿主只负责「画布现在有焦点 / 没焦点了」，
//! 「发什么字节」留在内核，可单测。
//!
//! # 协议
//!
//! 终端应用用 `CSI ? 1004 h` / `l` 开关上报（内核的
//! [`TermMode::FOCUS_IN_OUT`]）。开启后，宿主在画布**获得**焦点时写
//! `CSI I`（`\x1b[I`），**失去**焦点时写 `CSI O`（`\x1b[O`）。
//!
//! 典型用途是「多窗格编辑器失焦时自动保存 / 切换光标样式」：没有这两个序列，
//! 应用只能靠键盘输入猜测焦点是否还在自己身上。
//!
//! 参考实现：alacritty 的 `input.rs`（`TermMode::FOCUS_IN_OUT` → `\x1b[I` / `\x1b[O`）。

use alacritty_terminal::term::TermMode;

/// 终端应用是否要求上报焦点变化（`CSI ? 1004`）。
#[must_use]
pub fn reports_focus(mode: TermMode) -> bool {
    mode.contains(TermMode::FOCUS_IN_OUT)
}

/// 把一次焦点变化编成要写进 PTY 的字节；应用没要求上报时返回 `None`。
///
/// `focused` 为 `true` 表示画布**刚拿到**焦点。
#[must_use]
pub fn encode(focused: bool, mode: TermMode) -> Option<Vec<u8>> {
    reports_focus(mode).then(|| {
        if focused {
            b"\x1b[I".to_vec()
        } else {
            b"\x1b[O".to_vec()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_reported_without_the_focus_mode() {
        let mode = TermMode::SHOW_CURSOR;
        assert!(!reports_focus(mode));
        assert_eq!(encode(true, mode), None);
        assert_eq!(encode(false, mode), None);
    }

    #[test]
    fn focus_in_and_out_use_csi_i_and_csi_o() {
        let mode = TermMode::FOCUS_IN_OUT;
        assert!(reports_focus(mode));
        assert_eq!(encode(true, mode), Some(b"\x1b[I".to_vec()));
        assert_eq!(encode(false, mode), Some(b"\x1b[O".to_vec()));
    }

    #[test]
    fn the_mode_bit_is_read_from_a_broader_mode_set() {
        // 真实场景里 1004 总与别的模式位并存（1000 / 1002 / 1006 …）。
        let mode = TermMode::FOCUS_IN_OUT | TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(encode(false, mode), Some(b"\x1b[O".to_vec()));
    }
}
