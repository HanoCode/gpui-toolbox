//! 辅助窗口 chrome：自绘标题栏 + 任意内容视图。
//!
//! 对应 FluxDown `fluxdown_ui_shell::AuxiliaryWindowView`：标题行以绝对定位铺满
//! 拖动区（gpui-component 的拖动区不允许收缩，流内标题会把窗口控制按钮挤出窗口），
//! 底色 chrome、hairline 底线，与主窗口顶栏一致。

use duck_theme::active_theme;
use gpui::{
    AnyElement, AnyView, App, Context, FontWeight, IntoElement, ParentElement, Pixels, Render,
    SharedString, Styled, Subscription, Window, div,
};
use gpui_component::{TitleBar, h_flex, v_flex};
use std::rc::Rc;

use crate::{BoundsHook, initial_traffic_light_y, sync_traffic_light, title_bar_height};

/// 窗口标题：渲染时按当前语言解析（语言切换后刷新即换文案）。
type TitleSource = Rc<dyn Fn(&App) -> SharedString>;

/// 标题左侧的插槽（例：设置窗口的侧栏折叠按钮）。
///
/// 是闭包而不是元素：它每帧求值，所以里面的按钮能读到**实时**状态
/// （提示语随折叠状态换词），不会定格在 chrome 构建那一刻。
type LeadingSource = Rc<dyn Fn(&App) -> AnyElement>;

/// 承载任意能力页面的辅助窗口。
pub struct AuxiliaryWindowView {
    title: TitleSource,
    /// 标题文字左侧的插槽；`None` 时不占位。
    leading: Option<LeadingSource>,
    content: AnyView,
    /// 已应用到 macOS 交通灯的纵向偏移；标题栏随文字放大撑高时重新居中。
    traffic_light_y: Pixels,
    /// 窗口几何变化回调（由应用注入，用于持久化位置 / 尺寸）。
    bounds_hook: Option<BoundsHook>,
    /// 几何观察订阅；首次渲染时建立一次。
    bounds_subscription: Option<Subscription>,
}

impl AuxiliaryWindowView {
    /// 创建辅助窗口 chrome（标题在窗口选项里已同步给 OS）。
    pub fn new(title: impl Into<SharedString>, content: AnyView, _cx: &mut Context<Self>) -> Self {
        let title = title.into();
        Self {
            title: Rc::new(move |_| title.clone()),
            leading: None,
            content,
            traffic_light_y: initial_traffic_light_y(),
            bounds_hook: None,
            bounds_subscription: None,
        }
    }

    /// 挂载「窗口几何变化」回调（用于持久化位置 / 尺寸）。
    ///
    /// 与 [`crate::ShellView::on_window_bounds_changed`] 同形：`observe_window_bounds`
    /// 需要窗口，所以订阅推迟到首次渲染建立。
    #[must_use]
    pub fn on_window_bounds_changed(mut self, hook: impl Fn(&Window, &mut App) + 'static) -> Self {
        self.bounds_hook = Some(Rc::new(hook));
        self
    }

    /// 让标题随语言解析（例：`设置 / Settings`）。
    #[must_use]
    pub fn with_title_fn(mut self, title: impl Fn(&App) -> SharedString + 'static) -> Self {
        self.title = Rc::new(title);
        self
    }

    /// 在标题文字左侧加一个插槽（例：设置窗口的侧栏折叠按钮）。
    #[must_use]
    pub fn with_title_leading(mut self, leading: impl Fn(&App) -> AnyElement + 'static) -> Self {
        self.leading = Some(Rc::new(leading));
        self
    }

    /// 标题栏：chrome 底 + hairline 底线；标题 sm MEDIUM，左对齐。
    fn render_title_bar(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().colors;
        let height = title_bar_height(cx);
        let title_row = h_flex()
            .absolute()
            .inset_0()
            .items_center()
            .gap(tokens.spacing.sm)
            .pl(tokens.spacing.sm)
            .pr(tokens.spacing.md)
            .children(self.leading.as_ref().map(|leading| leading(cx)))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.sm.size)
                    .line_height(tokens.typography.sm.line_height)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.foreground)
                    .child((self.title)(cx)),
            );
        // 显式 `.bg` 覆盖 gpui-component 默认渐变；`.h` 覆盖默认 34px。
        TitleBar::new()
            .h(height)
            .bg(extended.chrome)
            .border_color(extended.hairline)
            .child(title_row)
            .into_any_element()
    }
}

impl Render for AuxiliaryWindowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 首次渲染时建立窗口几何观察（`observe_window_bounds` 需要窗口）。
        if self.bounds_subscription.is_none()
            && let Some(hook) = self.bounds_hook.clone()
        {
            self.bounds_subscription =
                Some(cx.observe_window_bounds(window, move |_aux, window, cx| hook(window, cx)));
        }
        sync_traffic_light(window, title_bar_height(cx), &mut self.traffic_light_y);
        let colors = active_theme(cx).tokens().colors;
        v_flex()
            .size_full()
            .relative()
            .bg(colors.surface)
            .text_color(colors.foreground)
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .bg(colors.surface)
                    .child(self.content.clone()),
            )
    }
}
