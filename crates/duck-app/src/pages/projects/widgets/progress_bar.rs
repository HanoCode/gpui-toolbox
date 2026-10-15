//! 任务进度条。
//!
//! 条本身复用 `duck-components` 的 `progress_bar`（它已经把上游 `Progress` 的
//! 高度 / 圆角对齐了主题 token），这里补两样：
//!
//! 1. **右侧那个百分比数字** —— 纯进度条在卡片上太小，没有数字读者要拿尺子量；
//! 2. **填色按进度换档**（原型 `progressTone`，`docs/projects-mock.html:1040`）：
//!    ≥80 绿 / ≥40 强调 / >0 橙 / 0 灰。「快完成了」和「刚起步」在一屏几十张卡里
//!    本该一眼分得开，全用同一个蓝就得挨个读数字。

use duck_components::content::progress_bar;
use duck_theme::active_theme;
use gpui::{
    AnyElement, App, ElementId, Hsla, IntoElement, ParentElement, SharedString, Styled, div,
};

/// 进度色调：与原型 `progressTone` 的三档边界逐条对应。
pub(crate) fn progress_tone(progress: u8, cx: &App) -> Hsla {
    let theme = active_theme(cx);
    match progress {
        80..=u8::MAX => theme.extended().colors.success,
        40..=79 => theme.tokens().colors.primary,
        1..=39 => theme.extended().colors.warning,
        _ => theme.extended().colors.text_tertiary,
    }
}

/// 只有条、没有数字（看板卡片的 `.pbar`：`n%` 在 `.pmeta` 里右对齐）。
pub(crate) fn task_progress_bar(id: impl Into<ElementId>, progress: u8, cx: &App) -> AnyElement {
    progress_bar(id, f32::from(progress) / 100., cx)
        .color(progress_tone(progress, cx))
        .into_any_element()
}

/// 「条 + `n%`」。
///
/// `progress` 是 0–100 的整数（内核口径），而 `progress_bar` 收 0–1 —— 换算只在这里做一次。
pub(crate) fn task_progress(id: impl Into<ElementId>, progress: u8, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    div()
        .flex()
        .w_full()
        .items_center()
        .gap(tokens.spacing.xs)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(task_progress_bar(id, progress, cx)),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(format!("{progress}%"))),
        )
        .into_any_element()
}
