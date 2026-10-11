//! 停靠系统接入：一个通用内容面板 + 开箱可用的停靠区。
//!
//! 上游 `gpui_component::dock` 是完整的停靠实现（拖拽 / 分屏 / 缩放 / 布局持久化 / 皮肤），
//! 缺的只是「一个通用面板」—— 它要求每种面板各写一遍行为 trait 与呈现 trait。
//!
//! 本模块补这一层：[`ContentPanel`] 用「标题 + 图标 + 视图」就能注册成面板，于是
//! 「把任意页面放进停靠区」不必每页重复实现 trait。停靠区本身用上游 `DockSkin`
//! （自带标题栏、拖拽预览、分屏把手），不另画一套。
//!
//! 未做：布局持久化。`DockArea::load` 需要按面板类型名注册构造函数，而通用面板的
//! 内容（`AnyView`）无法从名字重建；本轮停靠区每次启动按代码重建，不做恢复。

use duck_theme::active_theme;
use gpui::{
    AnyView, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, Render, SharedString, Styled, Window, div,
};
use gpui_component::{
    Icon,
    dock::{DockArea, DockLayout, DockSkin, Panel as PresentationPanel, PanelEvent},
};

use crate::DuckIcon;

/// 通用内容面板：一段标题 + 一个图标 + 一个视图。
///
/// 一个进程里只需要这一个 `Panel` 实现 —— 页面各自建自己的视图，再用
/// [`content_panel`] 包成面板，不必每页各写一遍 trait。
pub struct ContentPanel {
    title: SharedString,
    icon: DuckIcon,
    view: AnyView,
    focus: FocusHandle,
    closable: bool,
}

impl ContentPanel {
    /// 用标题 + 图标 + 视图创建面板。
    pub fn new(
        title: impl Into<SharedString>,
        icon: DuckIcon,
        view: impl Into<AnyView>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            title: title.into(),
            icon,
            view: view.into(),
            focus: cx.focus_handle(),
            closable: true,
        }
    }

    /// 是否允许关闭该面板（默认允许）。
    #[must_use]
    pub fn closable(mut self, closable: bool) -> Self {
        self.closable = closable;
        self
    }

    /// 面板标题。
    #[must_use]
    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// 面板图标。
    #[must_use]
    pub const fn icon(&self) -> DuckIcon {
        self.icon
    }
}

impl Render for ContentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = active_theme(cx).tokens().colors;
        div()
            .size_full()
            .min_h_0()
            .bg(colors.surface)
            .text_color(colors.foreground)
            .child(self.view.clone())
    }
}

impl EventEmitter<PanelEvent> for ContentPanel {}

impl Focusable for ContentPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl gpui_base::dock::Panel for ContentPanel {
    fn panel_name(&self) -> &'static str {
        CONTENT_PANEL_NAME
    }

    fn closable(&self, _cx: &App) -> bool {
        self.closable
    }
}

impl PresentationPanel for ContentPanel {
    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(self.title.clone())
    }

    fn title(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        gpui_component::h_flex()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(
                Icon::new(self.icon)
                    .size(extended.icon.md)
                    .text_color(extended.colors.text_tertiary),
            )
            .child(self.title.clone())
    }

    /// 面板自带完整内容（多半自带内边距），不再让标签组额外补一圈。
    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

/// 面板类型名：用于元素 id 与调试，不参与持久化（本轮不做布局恢复）。
pub const CONTENT_PANEL_NAME: &str = "duck-content";

/// 创建通用内容面板。
pub fn content_panel(
    title: impl Into<SharedString>,
    icon: DuckIcon,
    view: impl Into<AnyView>,
    cx: &mut Context<ContentPanel>,
) -> Entity<ContentPanel> {
    let title = title.into();
    let view = view.into();
    cx.new(|cx| ContentPanel::new(title, icon, view, cx))
}

/// 建一个单面板停靠区：中心放置 `panel`，采用上游皮肤（标题栏、拖拽预览、分屏把手）。
///
/// 多面板 / 分屏布局直接用上游 `DockLayout::{tabs, h_split, v_split}` 组好再交给
/// `set_center`；本函数只是把「建区 + 装皮肤 + 放中心面板」三步收成一步。
pub fn dock_area(
    id: impl Into<SharedString>,
    panel: Entity<ContentPanel>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<DockArea> {
    let (area, _skin) = DockSkin::dock_area(id, None, window, cx);
    area.update(cx, |area, cx| {
        area.set_center(DockLayout::tabs().panel(panel), window, cx);
    });
    area
}
