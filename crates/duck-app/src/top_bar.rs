//! 窗口级顶栏插槽：程序标题 + 搜索入口。
//!
//! 由 shell 挂在所有路由共用的顶栏位置（[`duck_shell::ShellView::with_title_bar_leading`]），
//! 因此概览、工具箱等任何页面都能看到同一段前导区。
//!
//! 搜索入口本身**不是输入框**：点击它打开命令面板（工具与命令都在面板里搜，
//! 角标显示该动作当前生效的键位），与 `palette.toggle` 动作走同一条派发链。

use duck_shell::interactive;
use duck_theme::{active_theme, tr};
use gpui::{
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::{Icon, h_flex};

use crate::actions::ids;

/// 顶栏搜索入口宽度（窄窗口下可收缩）。
const SEARCH_WIDTH: f32 = 220.;
/// 顶栏搜索入口最小宽度。
const SEARCH_MIN_WIDTH: f32 = 120.;

/// 顶栏前导插槽：程序标题 + 搜索入口。
///
/// 标题常驻左侧（`flex_none`），搜索入口在右、窄窗口时先收缩——标题永远完整可读。
#[derive(Default)]
pub struct TopBarSearch;

impl TopBarSearch {
    /// 创建搜索入口（每个窗口一个，跨路由共用）。
    pub fn new() -> Self {
        Self
    }
}

impl Render for TopBarSearch {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let colors = tokens.colors;
        let chrome = extended.colors;
        let opacity = extended.opacity;
        let hint = tokens.typography.xs;
        let title_typography = tokens.typography.md;

        // 角标文案从键位注册表派生（含用户在 `keybindings.toml` 里的覆盖），而不是写死一个
        // `⌘K`：键位改了角标跟着改，两者不会对不上。取不到绑定就干脆不画角标——宁可不提示，
        // 也不要提示一个按下去没反应的键。
        let shortcut_badge = duck_command::keybindings_for(cx, ids::PALETTE_TOGGLE)
            .first()
            .map(|keys| {
                div()
                    .flex_none()
                    .px(tokens.spacing.xs)
                    .rounded(tokens.radius.sm)
                    .border(extended.stroke.thin)
                    .border_color(chrome.hairline)
                    .bg(colors.surface)
                    .text_size(hint.size)
                    .line_height(hint.line_height)
                    .text_color(chrome.text_tertiary)
                    .child(SharedString::from(duck_command::pretty(keys)))
                    .into_any_element()
            });

        // 外层一行：程序标题（固定宽）+ 搜索入口胶囊（可收缩）。
        h_flex()
            .items_center()
            .gap(tokens.spacing.lg)
            .flex_shrink(1.)
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(title_typography.size)
                    .line_height(title_typography.line_height)
                    .font_weight(title_typography.weight)
                    .text_color(colors.foreground)
                    // 品牌名不翻译：与系统通知发送方、「关于」页共用唯一来源。
                    .child(crate::APP_NAME),
            )
            .child(interactive(
                h_flex()
                    .id("top-search-trigger")
                    .w(px(SEARCH_WIDTH))
                    .min_w(px(SEARCH_MIN_WIDTH))
                    .flex_shrink(1.)
                    .h(theme.density().control)
                    .pl(tokens.spacing.sm)
                    .pr(tokens.spacing.xs)
                    .gap(tokens.spacing.xs)
                    .items_center()
                    .rounded(tokens.radius.md)
                    .border(extended.stroke.thin)
                    .bg(colors.surface)
                    .border_color(colors.primary.opacity(0.))
                    // 悬停：强调色薄底 + 强调色描边（与旧搜索框未聚焦态同款）。
                    .hover(move |style| {
                        style
                            .bg(chrome.hover_tint)
                            .border_color(colors.primary.opacity(opacity.muted))
                    })
                    .cursor_pointer()
                    // 与快捷键 / 菜单同一个动作：打开命令面板。
                    .on_click(|_, window, cx| {
                        duck_command::dispatch(ids::PALETTE_TOGGLE, window, cx);
                    })
                    .child(
                        Icon::new(duck_components::DuckIcon::Search)
                            .size(extended.icon.md)
                            .text_color(colors.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(colors.muted_foreground)
                            .child(tr(cx, "搜索工具、命令…")),
                    )
                    // 该动作当前生效的键位角标（没有绑定时不画）。
                    .children(shortcut_badge)
                    .flex_shrink(1.)
                    .min_w(px(SEARCH_MIN_WIDTH)),
            ))
    }
}
