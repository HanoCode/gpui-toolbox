//! 标签片。
//!
//! 这是全页**唯一**允许出现非 token 颜色的地方：标签色是用户数据
//! （`tags.color` 存的就是 `#RRGGBB`），必须原样呈现。转换集中在
//! [`duck_theme::color_from_hex`]，这里不自己解析。
//!
//! 底色刻意用中性 `muted` 而不是标签色本身：标签色只作圆点，文字才始终跟主题走 ——
//! 浅黄标签配白字、深蓝标签配黑字这类问题就没了（配色由用户定，没法定对比度）。

use duck_project::Tag;
use duck_theme::{active_theme, color_from_hex};
use gpui::{AnyElement, App, IntoElement, ParentElement, SharedString, Styled, div};

/// 一个标签片：中性底 + 彩色圆点 + 名称。
pub(crate) fn tag_chip(tag: &Tag, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let dot = color_from_hex(&tag.color).unwrap_or(tokens.colors.muted_foreground);

    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(tokens.spacing.xxs)
        .px(tokens.spacing.xs)
        .rounded_full()
        .bg(tokens.colors.muted)
        .child(
            div()
                .size(theme.extended().icon.sm / 2.)
                .flex_none()
                .rounded_full()
                .bg(dot),
        )
        .child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(tag.name.clone())),
        )
        .into_any_element()
}
