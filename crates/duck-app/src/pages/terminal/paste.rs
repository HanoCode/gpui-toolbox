//! 粘贴：多行确认、换行替换、裁剪首尾。
//!
//! # 为什么粘贴要问一句
//!
//! 终端里的粘贴是**会立刻执行**的：多行文本里的每一个换行都等于一次 ⏎。
//! 从网页上复制一段带行号的代码、从聊天记录里复制三行命令，粘进 `ssh` 会话就是
//! 三条命令连着跑 —— 其中可能有一条是 `rm`。主流终端（iTerm2、Windows Terminal、
//! Termius）都在「多行粘贴」这一步给了确认，本仓此前是直写 PTY。
//!
//! 三件事一起做（缺一件都不完整）：
//!
//! 1. **多行确认**：只在文本真的含换行时才问（单行粘贴问一次就是纯骚扰）；
//! 2. **换行替换为空格**：把多行折成一行再粘贴 —— 粘一串路径、一串参数时最常用，
//!    而且它**天然避免了误执行**（没有换行就没有 ⏎）；
//! 3. **裁剪首尾**：折行之后把首尾空白去掉，否则粘进命令行会多出一串空格。
//!
//! 三档偏好（`module_settings::PasteConfirm`）：始终问 / 仅多行问（默认）/ 从不问。
//! 「从不问」是给「我知道自己在粘什么」的用户留的出口，**不是**默认值。

use duck_components::{ButtonVariant, FooterAction, dialog_footer_with_action};

use super::*;

/// 粘贴确认的三档里，这一次要不要问。
///
/// 纯函数：三档 × 是否多行 共六种组合都能单测，不必起弹窗。
pub(super) const fn needs_confirm(mode: PasteConfirm, multiline: bool) -> bool {
    match mode {
        PasteConfirm::Always => true,
        PasteConfirm::Multiline => multiline,
        PasteConfirm::Never => false,
    }
}

/// 文本是否「含换行」。
///
/// **尾随的那一个换行不算**：从编辑器里复制一整行，剪贴板里几乎总带着行尾的 `\n`，
/// 而它的效果等同于「用户自己按了一次 ⏎」—— 为此弹一次确认，等于每次复制单行都弹。
/// 只看「去掉尾部空白之后还有没有换行」。
pub(super) fn is_multiline(text: &str) -> bool {
    text.trim_end().contains('\n')
}

/// 折成一行：换行换成空格 + 折叠连续空白 + 裁剪首尾。
///
/// 折叠连续空白是刻意的：多行代码折行后会留下「缩进 + 换行」两个空格，
/// 不折叠的话粘进命令行是一串不可读的空隙。
pub(super) fn fold_to_single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl TerminalPage {
    /// 粘贴入口（`⌘V` 与右键菜单共用）：按偏好决定是否先问一句。
    pub(crate) fn paste_from_clipboard(
        &mut self,
        tab_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let mode = module_settings::terminal_settings(cx).paste_confirm;
        if needs_confirm(mode, is_multiline(&text)) {
            self.open_paste_confirm(tab_id, text, window, cx);
            return;
        }
        self.paste_text(tab_id, &text, cx);
    }

    /// 把文本真正写进 PTY（走内核的括号粘贴处理），并回到屏幕底部。
    ///
    /// 粘贴是「我现在就要看结果」的动作：用户可能正翻着历史，粘完还停在旧位置，
    /// 就会以为没粘上。清屏 / 粘贴这类「作用于当下」的操作都回底。
    pub(crate) fn paste_text(&mut self, tab_id: &str, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) else {
            return;
        };
        session.paste(text);
        session.scroll_to_bottom();
        cx.notify();
    }

    /// 右键菜单「粘贴并执行」：折成一行 → 粘贴 → 补一个 ⏎。
    ///
    /// 折行是这个动作**全部的意义**：多行文本原样粘进去会逐行执行，而「粘贴并执行」
    /// 想表达的是「把这一段当成一条命令跑起来」。想原样粘就选上一条「粘贴」。
    ///
    /// 不弹多行确认：用户点的就是「执行」，再问一次只是把已经做过的决定再问一遍。
    pub(crate) fn paste_and_run(
        &mut self,
        tab_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let folded = fold_to_single_line(&text);
        if folded.is_empty() {
            return;
        }
        self.paste_text(tab_id, &folded, cx);
        // ⏎ 的线上编码就是 `CR`（`encode(KeyStroke::key("enter"))` 的产物）。
        // 直接写字节而不是绕 `send_encoded`：那一层在画布模块里是私有的，
        // 而这里要发的就是一个固定字节，没有编码表可查。
        if let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) {
            session.write(b"\r");
            session.scroll_to_bottom();
        }
        cx.notify();
    }

    /// 「粘贴 N 行文本？」确认框。
    ///
    /// 两个动作都是**明确的粘贴方式**，没有「默认帮我改一下」的隐藏行为：
    /// - 主按钮「原样粘贴」 —— ⏎ 也走它（用户已经看见行数了，按 ⏎ 就是同意）；
    /// - 旁路按钮「折成一行粘贴」 —— 换行换空格 + 裁剪首尾。
    ///
    /// 正文只显示前 3 行：粘贴确认要解决的问题是「我粘的是不是我以为的那段」，
    /// 看开头三行足够判断，而完整的 200 行贴进弹窗只会把两个按钮挤出屏幕。
    fn open_paste_confirm(
        &mut self,
        tab_id: &str,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lines = text.trim_end().lines().count();
        let preview: Vec<String> = text
            .trim_end()
            .lines()
            .take(PREVIEW_LINES)
            .map(|line| truncate_line(line, PREVIEW_LINE_WIDTH))
            .collect();
        let folded = fold_to_single_line(&text);
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let title = trf(cx, "粘贴 {} 行文本？", &[&lines.to_string()]);
        // 会话键先转成 `String`：`open_dialog` 的闭包要求 `'static`，
        // 直接捕获 `&str` 会让这个引用逃出函数体。
        let tab = tab_id.to_owned();

        window.open_dialog(cx, move |dialog, _, cx| {
            let offset = modal_offset(&drag, cx);
            // 外层闭包是 `Fn`（每次重排都会重跑）⇒ 每个内层闭包各拿一份克隆。
            let (view_ok, tab_ok, text_ok) = (view.clone(), tab.clone(), text.clone());
            let (view_enter, tab_enter, text_enter) = (view.clone(), tab.clone(), text.clone());
            let (view_fold, tab_fold, folded_fold) = (view.clone(), tab.clone(), folded.clone());
            let drag_for_content = drag.clone();
            let title_for_content = title.clone();
            let preview_for_content = preview.clone();
            let folded_for_content = folded.clone();

            dialog
                .w(px(CONFIRM_DIALOG_W))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    let title = title_for_content.clone();
                    let preview = preview_for_content.clone();
                    let folded = folded_for_content.clone();
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "terminal-paste-title",
                                &drag_for_content,
                                title,
                                cx,
                            ))
                            .child(body_text(tr(cx, "多行文本里的每个换行都会立刻执行。"), cx))
                            .child(
                                v_flex()
                                    .w_full()
                                    .mt(px(6.))
                                    .px(px(8.))
                                    .py(px(6.))
                                    .rounded(px(4.))
                                    .bg(active_theme(cx).extended().colors.hover_tint)
                                    .children(preview.into_iter().map(|line| {
                                        meta_text(line, cx).w_full().min_w_0().truncate()
                                    })),
                            )
                            // 折行的**结果**要看得见：折完可能长到看不出是同一段文本，
                            // 让用户先读一眼再决定，比「点了才知道」好。
                            .child(
                                meta_text(trf(cx, "折成一行后：{}", &[&folded]), cx)
                                    .w_full()
                                    .min_w_0()
                                    .truncate()
                                    .mt(px(6.)),
                            ),
                    ))
                })
                .footer(dialog_footer_with_action(
                    "terminal-paste",
                    FooterAction {
                        id: SharedString::from("terminal-paste-fold"),
                        text: tr(cx, "折成一行粘贴").into(),
                        on_click: Rc::new(move |window, cx| {
                            let _ = view_fold.update(cx, |this, cx| {
                                this.paste_text(&tab_fold, &folded_fold, cx);
                            });
                            window.close_dialog(cx);
                        }),
                    },
                    tr(cx, "原样粘贴"),
                    ButtonVariant::Primary,
                    Rc::new(move |_, cx| {
                        let _ = view_ok.update(cx, |this, cx| {
                            this.paste_text(&tab_ok, &text_ok, cx);
                        });
                        true
                    }),
                    cx,
                ))
                // ⏎ 等同「原样粘贴」：用户已经看见行数了，按 ⏎ 就是同意。
                // 取消（按钮 / ✕ / Esc）只关窗 —— 不注册 `on_cancel`，
                // 它的默认行为就是关窗，注册一个空实现反而要多持有一份 `view`。
                .on_ok(move |_, _window, cx| {
                    let _ = view_enter.update(cx, |this, cx| {
                        this.paste_text(&tab_enter, &text_enter, cx);
                    });
                    true
                })
        });
    }
}

/// 确认框里最多预览几行。
const PREVIEW_LINES: usize = 3;
/// 每行预览截到多少个字符。
///
/// 弹窗正文宽度在 13 寸屏上是 420 逻辑像素上下，`xs` 字号一行约 52 个半角字符 ——
/// 取 60 是「宁可多一个字符再省略」，而不是让长行把弹窗撑开。
const PREVIEW_LINE_WIDTH: usize = 60;

/// 按**字符**截断（不是字节）：中文按字节截会把 UTF-8 切坏，直接 panic。
fn truncate_line(line: &str, limit: usize) -> String {
    if line.chars().count() <= limit {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_newline_is_not_multiline() {
        // 从编辑器复制单行常带行尾 `\n`：它等价于用户自己按了一次 ⏎，不该弹确认。
        assert!(!is_multiline("ls -la\n"));
        assert!(!is_multiline("ls -la"));
        // 只有**中间**的换行才算多行 —— 行尾那一个不算（trim 掉再判）。
        assert!(is_multiline("ls\npwd"));
        assert!(!is_multiline("ls\n"));
        // 连续多个行尾换行同样不算：「一整行 + 几个空行」是复制代码块常见的形态。
        assert!(!is_multiline("ls\n\n\n"));
    }

    #[test]
    fn fold_collapses_and_trims() {
        assert_eq!(fold_to_single_line("  a  \n\n b\tc \n"), "a b c");
        assert_eq!(fold_to_single_line("single"), "single");
    }

    #[test]
    fn confirm_matrix() {
        assert!(needs_confirm(PasteConfirm::Always, false));
        assert!(needs_confirm(PasteConfirm::Always, true));
        assert!(!needs_confirm(PasteConfirm::Multiline, false));
        assert!(needs_confirm(PasteConfirm::Multiline, true));
        assert!(!needs_confirm(PasteConfirm::Never, true));
    }

    #[test]
    fn truncation_is_char_based() {
        let long = "中".repeat(PREVIEW_LINE_WIDTH + 5);
        let cut = truncate_line(&long, PREVIEW_LINE_WIDTH);
        assert_eq!(cut.chars().count(), PREVIEW_LINE_WIDTH + 1);
        assert!(cut.ends_with('…'));
    }
}
