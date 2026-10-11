//! 占位页：已登记但尚未实现的路由所用的空白页 —— 居中图标 + 标题 + 建设中说明。
//!
//! 只做视觉占位，不含任何交互；功能落地后由对应业务页替换同一路由的视图。

use duck_components::{DuckIcon, title_text};
use duck_theme::{active_theme, tr};
use gpui::{IntoElement, ParentElement, Render, Styled, Window, div};
use gpui_component::{Icon, v_flex};

/// 占位图标相对 `extended.icon.lg` 的放大倍数：整页唯一的视觉主体。
const ICON_SCALE: f32 = 2.5;

/// 空白占位页。
pub struct PlaceholderPage {
    /// 页面标题（i18n 源串，渲染时按当前语言解析）。
    title: &'static str,
    /// 页面图标。
    icon: DuckIcon,
}

impl PlaceholderPage {
    /// 创建占位页；`title` 传 i18n 源串。
    #[must_use]
    pub const fn new(title: &'static str, icon: DuckIcon) -> Self {
        Self { title, icon }
    }
}

impl Render for PlaceholderPage {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(tokens.spacing.md)
            .bg(tokens.colors.surface)
            .child(
                Icon::new(self.icon)
                    .size(extended.icon.lg * ICON_SCALE)
                    .text_color(extended.colors.text_tertiary),
            )
            .child(title_text(tr(cx, self.title), cx))
            .child(
                div()
                    .text_size(tokens.typography.sm.size)
                    .line_height(tokens.typography.sm.line_height)
                    .text_color(tokens.colors.muted_foreground)
                    .child(tr(cx, "功能正在建设中")),
            )
    }
}
