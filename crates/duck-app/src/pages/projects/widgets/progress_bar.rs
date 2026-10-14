//! 任务进度条。
//!
//! 条本身复用 `duck-components` 的 `progress_bar`（它已经把上游 `Progress` 的
//! 高度 / 圆角对齐了主题 token），这里只补右侧那个百分比数字 —— 纯进度条在卡片上
//! 太小，没有数字读者要拿尺子量。

use duck_components::content::progress_bar;
use duck_theme::active_theme;
use gpui::{AnyElement, App, ElementId, IntoElement, ParentElement, SharedString, Styled, div};

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
                .child(progress_bar(id, f32::from(progress) / 100., cx)),
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
