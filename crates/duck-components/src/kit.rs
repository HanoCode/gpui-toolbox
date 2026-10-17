//! 全应用共用的控件规格：按钮 / 输入框统一高度与字号、分段标签、复选行、表单原语。
//!
//! 规则（所有窗口与页面一致，数值对齐 FluxDown `settings::ui`）：
//! - 唯一一档控件高度 `density.control`（默认 28）、13px 正文字号；页面工具栏、对话框、
//!   表单页、设置行、列表行内、独立窗口里的按钮 / 输入框 / 下拉全部同高同内边距。
//! - 分组卡片 = `surface` + hairline 描边 + `radius.lg`，行间 hairline 分隔线**左侧留白**
//!   （`pl(spacing.lg)`），不加阴影。
//! - 设置行左右内边距 `spacing.lg`，横排行最小行高 = `density.control` + 20px，
//!   标题列保底 160px、控件列可收缩；竖排行上下内边距 `spacing.md`。
//! - 组标题 = `xs` MEDIUM + 三级文字色，左内边距 `spacing.xs`、下内边距 `spacing.sm`。
//! - 凡「标题」一律走 [`title_text`]（`md` / 半粗）或 [`detail_title_text`]（`lg` / 半粗），
//!   不在调用点自己拼 `text_size` + `font_weight`。字阶逐档对应 macOS HIG 内建文本样式，
//!   见 `duck_theme::builtin::base_tokens` 的注释。
//! - chrome 纯图标按钮优先 [`crate::toolbar_action_button`] 与 gpui-base 自绘按钮族。
//! - 布尔设置项用 [`switch_control`]，不用 gpui-component `Checkbox`。
//! - gpui 的 `hover` 每个元素只能设置一次：本 crate 返回的按钮 / 行已设置悬停，
//!   调用方不得再 `.hover()`。

use std::cell::RefCell;
use std::rc::Rc;

use duck_theme::active_theme;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Bounds, BoxShadow, Context, Div, ElementId, Entity,
    Focusable as _, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement, Pixels,
    Render, RenderOnce, Role, SharedString, StatefulInteractiveElement as _, Styled, Subscription,
    Toggled, Window, div, point, prelude::FluentBuilder as _, px, size,
};
use gpui_base::SelectableText;
use gpui_component::{
    Icon, Sizable as _, Size,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    popover::Popover,
    switch::Switch,
    v_flex,
};

use crate::{
    CheckState, DuckIcon, SlidingHighlight, SlidingRowHighlight, check_mark, color_transition,
    tabular_numbers,
};

/// 设置行最小行高在 `density.control` 之上的上下留白（对齐 FluxDown `ROW_MIN_PADDING`）。
pub const ROW_MIN_PADDING: f32 = 20.;
/// 设置行标题列保底宽度。
pub const TITLE_MIN_WIDTH: f32 = 160.;
/// 下拉按钮最小宽度（文案更长时自然撑开）。
pub const DROPDOWN_MIN_WIDTH: f32 = 160.;

/// 状态栏带文字的小按钮外壳：ghost、`density.statusControl` 高、横向 `spacing.xs`。
///
/// gpui-component 按钮会按 `Size` 在内部 label 上覆盖字号与图标尺寸，所以内容一律经
/// [`status_button_content`] 作为子元素传入，确保 `xs` 字号 + `icon.sm` 生效。
pub fn status_button(id: impl Into<ElementId>, cx: &App) -> Button {
    let theme = active_theme(cx);
    Button::new(id.into())
        .ghost()
        .with_size(Size::XSmall)
        .h(theme.density().status_control)
        .px(theme.tokens().spacing.xs)
}

/// 状态栏按钮内容：可选图标 + 可选文字，`xs` 字号、等宽数字；`color` 为空时继承按钮前景色。
pub fn status_button_content(
    icon: Option<crate::DuckIcon>,
    text: Option<SharedString>,
    color: Option<Hsla>,
    cx: &App,
) -> Div {
    let theme = active_theme(cx);
    let extended = theme.extended();
    let xs = theme.tokens().typography.xs;
    let icon_size = extended.icon.sm;
    let mut row = div()
        .flex()
        .items_center()
        .gap(theme.tokens().spacing.xxs)
        .text_size(xs.size)
        .line_height(xs.line_height)
        .font_features(tabular_numbers());
    if let Some(color) = color {
        row = row.text_color(color);
    }
    if let Some(icon) = icon {
        let mut item = gpui_component::Icon::new(icon).size(icon_size);
        if let Some(color) = color {
            item = item.text_color(color);
        }
        row = row.child(item);
    }
    if let Some(text) = text {
        row = row.child(text);
    }
    row
}

/// 统一控件尺寸（带文字按钮与输入框）。
pub trait ControlExt: Sized {
    /// `density.control` 高、13px 字。
    fn control(self, cx: &App) -> Self;
}

/// 纯图标按钮尺寸。
pub trait IconControlExt: Sized {
    /// `density.control` 见方（与同行输入框等高）。
    fn control_icon(self, cx: &App) -> Self;
}

// gpui-component 的按钮文字字号由 `Size` 决定并写在内部 label 上（根节点的
// `text_size` 覆盖不到）：`Medium` 取 `text_base` = 1rem = `typography.sm`(13)，
// 再用实例样式把高度与内边距收回到目标档位。
impl ControlExt for Button {
    fn control(self, cx: &App) -> Self {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        self.with_size(Size::Medium)
            .h(theme.density().control)
            .px(tokens.spacing.sm + tokens.spacing.xxs)
            .text_size(tokens.typography.sm.size)
            .line_height(tokens.typography.sm.line_height)
    }
}

impl IconControlExt for Button {
    fn control_icon(self, cx: &App) -> Self {
        Styled::size(
            self.with_size(Size::Medium),
            active_theme(cx).density().control,
        )
    }
}

// 输入框用 `Medium` 取得宽松的横向内边距；字号 / 高度由实例样式覆盖。
// 高度必须走 `Styled::h`：`Input` 自带的同名 `h()` 只作用于多行输入。
// 纵向内边距必须清零：`Medium` 单行自带上下各 8px，28px 外框扣掉后单行文本会被裁。
impl ControlExt for Input {
    fn control(self, cx: &App) -> Self {
        let theme = active_theme(cx);
        Styled::h(self.with_size(Size::Medium), theme.density().control)
            .py_0()
            .text_size(theme.tokens().typography.sm.size)
    }
}

type TabSelect = Rc<dyn Fn(usize, &mut Window, &mut App)>;
type NavSelect = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// 分段标签的一项：可选图标 + 文案。
///
/// 图标是可选的而不是必填：同一个分段控件在别处只放文字（比如设置页的语言档），
/// 强行给每处都配一个图标只会多出一堆凑数的装饰。
#[derive(Clone)]
pub struct SegmentItem {
    label: SharedString,
    icon: Option<DuckIcon>,
}

impl SegmentItem {
    /// 纯文字项。
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            icon: None,
        }
    }

    /// 带前置图标。
    #[must_use]
    pub fn icon(mut self, icon: DuckIcon) -> Self {
        self.icon = Some(icon);
        self
    }
}

/// 分段标签（macOS segmented control 风格）：浅灰轨道内，选中项白底 + 细阴影 +
/// 正文色中等字重，未选中为二级文字色。轨道高 `density.control`，项高再减 4。
///
/// 选中白底是一块随选中项滑动的底块（弹簧驱动，宽度同步过渡），标签文字色同步淡变。
pub fn segmented_tabs(
    id: impl Into<ElementId>,
    labels: impl IntoIterator<Item = SharedString>,
    selected: usize,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    segmented_tabs_with_icons(
        id,
        labels.into_iter().map(SegmentItem::new),
        selected,
        on_select,
        cx,
    )
}

/// 带图标的分段标签：外观与 [`segmented_tabs`] 完全一致，只是每一项可以前置一个图标。
///
/// 与 `segmented_tabs` 共用同一条渲染路径（前者只是把文案包成 [`SegmentItem`]），
/// 所以两处的几何量、选中块动效、无障碍语义不可能走偏。
pub fn segmented_tabs_with_icons(
    id: impl Into<ElementId>,
    items: impl IntoIterator<Item = SegmentItem>,
    selected: usize,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let colors = tokens.colors;
    let control = theme.density().control;
    let tab_radius = theme.components().tab_radius;

    div()
        .flex()
        .flex_none()
        .items_center()
        .h(control)
        // 轨道内边距 = 最紧一档间距，与选中底块的 `control - 2*间距` 内高配套。
        .p(tokens.spacing.xxs)
        .rounded(tab_radius + px(1.))
        .bg(theme.extended().colors.nav_hover)
        .child(SegmentedTabs {
            id: id.into(),
            items: items.into_iter().collect(),
            selected,
            on_select: Rc::new(on_select),
            style: SegmentedStyle {
                // 内高 = 轨道高 − 上下各一份轨道内边距，与上面的 `.p(间距)` 同源。
                height: control - tokens.spacing.xxs * 2.,
                radius: tab_radius,
                padding_x: tokens.spacing.md,
                icon_size: tokens.typography.sm.size,
                text_size: tokens.typography.sm.size,
                line_height: tokens.typography.sm.line_height,
                surface: colors.surface,
                foreground: colors.foreground,
                muted_foreground: colors.muted_foreground,
                accent_text: theme.extended().colors.accent_text,
                hover_tint: theme.extended().colors.hover_tint,
                ring: colors.ring,
                shadow: tokens.shadow.sm.clone(),
            },
        })
}

struct SegmentedStyle {
    height: Pixels,
    radius: Pixels,
    padding_x: Pixels,
    icon_size: Pixels,
    text_size: Pixels,
    line_height: Pixels,
    surface: Hsla,
    foreground: Hsla,
    muted_foreground: Hsla,
    /// 未选中标签的悬停文字色（强调色，悬停即见主题色）。
    accent_text: Hsla,
    /// 未选中标签的悬停底色（强调色薄层）。
    hover_tint: Hsla,
    ring: Hsla,
    shadow: Vec<BoxShadow>,
}

/// 上一帧量得的各标签相对首个标签的 `(左缘, 宽度)`，由标签行的 `on_children_prepainted`
/// 写入。放在元素状态而非实体里（不被依赖追踪），所以测量值变化时由写入方自己请求一帧。
#[derive(Clone, Default)]
struct SegmentGeometry(Rc<RefCell<Vec<(Pixels, Pixels)>>>);

#[derive(IntoElement)]
struct SegmentedTabs {
    id: ElementId,
    items: Vec<SegmentItem>,
    selected: usize,
    on_select: TabSelect,
    style: SegmentedStyle,
}

impl RenderOnce for SegmentedTabs {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            items,
            selected,
            on_select,
            style,
        } = self;
        let count = items.len();

        let (geometry, highlight, text_colors) = window.with_id(id.clone(), |window| {
            let geometry =
                window.with_global_id(ElementId::from("geometry"), |global_id, window| {
                    window.with_element_state(global_id, |state: Option<SegmentGeometry>, _| {
                        let state = state.unwrap_or_default();
                        (state.clone(), state)
                    })
                });
            // 选中项的底块外框；尚无测量（首帧）或与标签数不符时本帧由选中项自己画底。
            let highlight = {
                let tabs = geometry.0.borrow();
                tabs.get(selected)
                    .filter(|_| tabs.len() == count)
                    .map(|&(left, width)| {
                        Bounds::new(point(left, px(0.)), size(width, style.height))
                    })
            };
            let text_colors: Vec<Hsla> = (0..count)
                .map(|index| {
                    let color = if index == selected {
                        style.foreground
                    } else {
                        style.muted_foreground
                    };
                    color_transition(index, color, window, cx)
                })
                .collect();
            (geometry, highlight, text_colors)
        });

        let has_highlight = highlight.is_some();
        div()
            .relative()
            .h_full()
            .when_some(highlight, |this, bounds| {
                this.child(
                    SlidingHighlight::new(id.clone(), bounds, style.surface)
                        .radius(style.radius)
                        .shadow(style.shadow.clone()),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .h_full()
                    .gap(active_theme(cx).tokens().spacing.xxs)
                    .on_children_prepainted(move |bounds, window, _| {
                        let origin = bounds.first().map_or(px(0.), |first| first.origin.x);
                        let measured: Vec<(Pixels, Pixels)> = bounds
                            .iter()
                            .map(|tab| (tab.origin.x - origin, tab.size.width))
                            .collect();
                        let mut tabs = geometry.0.borrow_mut();
                        if *tabs != measured {
                            *tabs = measured;
                            window.request_animation_frame();
                        }
                    })
                    // 容器语义放在**直接包住标签行的那一层**，它才是 Tab 的真父节点。
                    // 没有它，读屏只会挨个念标签名，不会播报「共 N 项、第 M 项」。
                    // ⚠️ 必须排在 `on_children_prepainted` 之后：那是 `Div` 的**固有方法**，
                    // 而 `.id()` 会把类型变成 `Stateful<Div>`，之后就叫不到了。
                    .id(ElementId::from(format!("{id}-tablist")))
                    .role(Role::TabList)
                    .children(items.into_iter().zip(text_colors).enumerate().map(
                        |(index, (item, text_color))| {
                            let active = index == selected;
                            let on_select = Rc::clone(&on_select);
                            let SegmentItem { label, icon } = item;
                            // 未选中标签：悬停底色走强调色薄层 + 文字转强调色。
                            let (foreground, ring) = (style.accent_text, style.ring);
                            let hover_tint = style.hover_tint;
                            // 没有图标时不留间隙：`gap` 是容器级的，留着会让纯文字项
                            // 多出一段看不见的左内边距。
                            let icon_gap = if icon.is_some() {
                                active_theme(cx).tokens().spacing.xxs
                            } else {
                                px(0.)
                            };
                            div()
                                .id(ElementId::NamedInteger(
                                    SharedString::from(format!("{id}-tab")),
                                    index as u64,
                                ))
                                .h(style.height)
                                .px(style.padding_x)
                                .flex()
                                .items_center()
                                .gap(icon_gap)
                                .rounded(style.radius)
                                .text_size(style.text_size)
                                .line_height(style.line_height)
                                .cursor_pointer()
                                .text_color(text_color)
                                // 无障碍：分段控件对屏幕阅读器是一组「页签」，选中态必须报出去，
                                // 否则读屏只会念到两个标签名，用户不知道当前在哪一档。
                                // 图标是纯装饰，不进名字 —— 名字只取文案。
                                .role(Role::Tab)
                                .aria_selected(active)
                                .aria_label(label.clone())
                                .aria_position_in_set(index + 1)
                                .aria_size_of_set(count)
                                .map(|this| {
                                    if !active {
                                        this.hover(move |hover| {
                                            hover.bg(hover_tint).text_color(foreground)
                                        })
                                    } else if has_highlight {
                                        this.font_weight(FontWeight::MEDIUM)
                                    } else {
                                        this.font_weight(FontWeight::MEDIUM)
                                            .bg(style.surface)
                                            .shadow(style.shadow.clone())
                                    }
                                })
                                .border_1()
                                .border_color(gpui::transparent_black())
                                .focusable()
                                .tab_stop(true)
                                .focus_visible(move |focus| focus.border_color(ring))
                                .on_click(move |_, window, cx| on_select(index, window, cx))
                                // 图标不显式上色：它继承容器的 `text_color`，
                                // 于是选中 / 悬停的文字色变化会连图标一起带走。
                                .children(icon.map(|icon| Icon::new(icon).size(style.icon_size)))
                                .child(label)
                        },
                    )),
            )
    }
}

/// 导航列表的一行：图标 + 文案。
///
/// 行样式由 [`nav_list`] 统一构建（与 [`sidebar_navigation_button`] 同一规格）；
/// `id` 同时作为图标色动效的状态键，列表内须唯一且稳定。
pub struct NavItem {
    id: ElementId,
    label: SharedString,
    icon: DuckIcon,
}

impl NavItem {
    /// 创建一行。
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, icon: DuckIcon) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon,
        }
    }
}

/// 纵向导航列表：选中底是一块随选中项在行间滑动的高亮块（与 shell 活动栏同一动效），
/// 图标色随选中淡变；行样式与 [`sidebar_navigation_button`] 完全一致。
///
/// `selected` 为选中行下标（`None` 表示当前无选中，例如搜索后选中项被过滤掉）。
/// 行高固定为 `density.navRow`、行间间距 `spacing.xxs`，所以高亮块位置按下标直接算出，
/// 无需测量；列表不适合放非导航行。
pub fn nav_list(
    id: impl Into<ElementId>,
    items: impl IntoIterator<Item = NavItem>,
    selected: Option<usize>,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> NavList {
    let theme = active_theme(cx);
    NavList {
        id: id.into(),
        items: items.into_iter().collect(),
        selected,
        on_select: Rc::new(on_select),
        gap: theme.tokens().spacing.xxs,
        row_height: theme.density().nav_row,
        radius: theme.components().nav_item_radius,
        color: theme.extended().colors.nav_selected,
        icon_size: theme.extended().icon.lg,
    }
}

#[derive(IntoElement)]
pub struct NavList {
    id: ElementId,
    items: Vec<NavItem>,
    selected: Option<usize>,
    on_select: NavSelect,
    gap: Pixels,
    /// 行高（= `density.navRow`，与 [`crate::navigation_row`] 的 `h()` 同源：
    /// 滑动块按它算位置，所以两者必须取同一个 token）。
    row_height: Pixels,
    radius: Pixels,
    color: Hsla,
    icon_size: Pixels,
}

impl RenderOnce for NavList {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            items,
            selected,
            on_select,
            gap,
            row_height,
            radius,
            color,
            icon_size,
        } = self;
        let count = items.len();

        // 高亮块：整行宽，纵向按步距（行高 + 行距）落在选中行上。
        let highlight = selected.filter(|index| *index < count).map(|index| {
            let top = (row_height + gap) * index as f32;
            SlidingRowHighlight::new(id.clone(), top, row_height, color).radius(radius)
        });

        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                let active = selected == Some(index);
                let on_select = Rc::clone(&on_select);
                // 图标不设色，继承 navigation_row 的文字色：悬停/选中随文字一起转强调色。
                crate::navigation_row(
                    item.id,
                    item.label,
                    gpui_component::Icon::new(item.icon).size(icon_size),
                    div(),
                    active,
                    crate::SelectionPaint::Slider,
                    cx,
                )
                .on_click(move |_, window, cx| on_select(index, window, cx))
                .into_any_element()
            })
            .collect();

        div()
            .id(ElementId::from(format!("{id}-tablist")))
            // 同 `segmented_tabs`：导航行按 `Role::Tab` 报，容器就要是 `TabList`。
            .role(Role::TabList)
            .relative()
            .w_full()
            .when_some(highlight, |this, highlight| this.child(highlight))
            .child(div().flex().flex_col().w_full().gap(gap).children(rows))
    }
}

/// 可点击的复选行：[`check_mark`] + 文字（可选尾部计数 / 说明）。整行可点。
///
/// 无障碍：整行按 `CheckBox` 报给辅助技术，勾选态走 `aria_toggled`。**可访问名取自行内
/// 文字**（AccessKit 从子节点文字派生），所以 `label` 必须是能读出来的内容，不要塞纯图标。
pub fn check_row(
    id: impl Into<ElementId>,
    checked: bool,
    label: impl IntoElement,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
    cx: &App,
) -> gpui::Stateful<Div> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let hover = theme.extended().colors.hover_tint;
    // 按下比悬停浓一档（同色系）——整行可点却没有按下反馈，点击会显得「没吃上」。
    let press = theme.extended().colors.hover_tint_press;
    let ring = tokens.colors.ring;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(tokens.spacing.sm)
        .min_h(theme.density().control)
        .px(tokens.spacing.xs)
        .rounded(tokens.radius.md)
        .cursor_pointer()
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .text_color(tokens.colors.foreground)
        .border_1()
        .border_color(gpui::transparent_black())
        .role(Role::CheckBox)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(press))
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.border_color(ring))
        .on_click(move |_, window, cx| on_toggle(!checked, window, cx))
        .child(check_mark(
            if checked {
                CheckState::Checked
            } else {
                CheckState::Unchecked
            },
            cx,
        ))
        .child(label)
}

/// 小号计数 / 数字徽标文字样式（`xs` + 等宽数字 + 三级文字色）。
pub fn caption_number(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let xs = theme.tokens().typography.xs;
    div()
        .text_size(xs.size)
        .line_height(xs.line_height)
        .font_features(tabular_numbers())
        .text_color(theme.extended().colors.text_tertiary)
        .child(text.into())
}

/// 表单字段之间的纵向间距。
#[must_use]
pub fn form_gap(cx: &App) -> Pixels {
    active_theme(cx).tokens().spacing.lg
}

/// 表单整体：纵向排列字段，间距 [`form_gap`]。
pub fn form(cx: &App) -> Div {
    div().flex().flex_col().w_full().gap(form_gap(cx))
}

/// 字段标签：12px 中等字重、正文色。
pub fn field_label(text: impl Into<SharedString>, cx: &App) -> Div {
    let tokens = active_theme(cx).tokens();
    div()
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .font_weight(FontWeight::MEDIUM)
        .text_color(tokens.colors.foreground)
        .child(text.into())
}

/// 字段说明：12px 三级文字色。
/// 字段说明 / 提示。文字**可拖选复制**（邮件详情的「收件人 / 抄送 / 大小」这类
/// 值就走它渲染，那些值必须能选）。
#[track_caller]
pub fn field_hint(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let xs = theme.tokens().typography.xs;
    div()
        .text_size(xs.size)
        .line_height(xs.line_height)
        .text_color(theme.extended().colors.text_tertiary)
        .child(selectable_text_at(std::panic::Location::caller(), text))
}

/// 字段错误：12px 危险色。文字**可拖选复制**（错误详情要能整段拷给别人）。
#[track_caller]
pub fn field_error(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let xs = theme.tokens().typography.xs;
    div()
        .text_size(xs.size)
        .line_height(xs.line_height)
        .text_color(theme.tokens().colors.destructive)
        .child(selectable_text_at(std::panic::Location::caller(), text))
}

/// 字段成功提示：12px 成功绿。与 [`field_error`] 对称——测试通过、校验通过这类
/// 「值得停下看一眼」的正向结论要有色彩语义，落回中性说明色就和旁边的字段说明
/// 没有区别了。绿与「已连接 / 已完成」状态点同源（`extended().success`）。
#[track_caller]
pub fn field_success(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let xs = theme.tokens().typography.xs;
    div()
        .text_size(xs.size)
        .line_height(xs.line_height)
        .text_color(theme.extended().colors.success)
        .child(selectable_text_at(std::panic::Location::caller(), text))
}

/// 标准字段：标签 → 控件 → 可选错误 / 说明。标签与控件、控件与说明间距 6px
///（`spacing.xs + spacing.xxs`，即「一个控件内部」的紧密度）。
///
/// 校验提示**贴着它描述的控件**，不另起一行汇总——汇总行会让用户来回找是哪一项错了。
/// `error` 与 `hint` 可以同时给：说明常驻、错误按需出现，两者语义不同。
pub fn form_field(
    label: impl Into<SharedString>,
    control: impl IntoElement,
    error: Option<SharedString>,
    hint: Option<SharedString>,
    cx: &App,
) -> Div {
    let spacing = active_theme(cx).tokens().spacing;
    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .gap(spacing.xs + spacing.xxs)
        .child(field_label(label, cx))
        .child(control)
        .when_some(error, |this, error| this.child(field_error(error, cx)))
        .when_some(hint, |this, hint| this.child(field_hint(hint, cx)))
}

/// 同一行并排的多个字段，等分宽度。
pub fn form_row(fields: impl IntoIterator<Item = AnyElement>, cx: &App) -> Div {
    let spacing = active_theme(cx).tokens().spacing;
    div().flex().w_full().gap(spacing.md).children(
        fields
            .into_iter()
            .map(|field| div().flex_1().min_w_0().child(field)),
    )
}

/// 输入框 + 同行操作按钮：输入框吃满剩余宽度。
pub fn input_with_action(input: impl IntoElement, action: impl IntoElement, cx: &App) -> Div {
    let spacing = active_theme(cx).tokens().spacing;
    div()
        .flex()
        .items_center()
        .w_full()
        .gap(spacing.sm)
        .child(div().flex_1().min_w_0().child(input))
        .child(div().flex_none().child(action))
}

/// 可拖选、可 ⌘C 复制的纯文本 —— **内容区文字**一律走这里。
///
/// 为什么需要它：引擎（`gpui_base`）的文本选中是**元素主动报名**的 —— 只有把文本交给
/// [`gpui_base::SelectableText`]，它才会把自己的命中区与文本 run 注册进窗口级的
/// `TextSelection`；`gpui_base::Root` 里的 `TextSelectionLayer` 负责画选区、`⌘C` 负责
/// 取文字。gpui 的普通文本子元素（`div().child("文字")`）**不参与报名**，因此天然
/// 不可选中 —— 这是框架行为，不是遗漏。`TextView` 也能选中，但它自带 Markdown 解析与
/// 虚拟化，给一行状态文字用是杀鸡用牛刀，所以内容区统一走本函数。
///
/// 样式无需额外传：`SelectableText` 内部的 `StyledText` 继承父元素的 text style，
/// 外层照旧写 `div().text_size(..).text_color(..).child(selectable_text(t))`。
///
/// # id 是「调用点 + 文本内容」
///
/// 引擎把参与者按 `TextSelectionHandle` 的**实体**归档（`participants: HashMap<EntityId, _>`），
/// 而 handle 是按**元素 id** 保留的 ⇒ 两个元素拿到同一个 id，就会共用同一个 handle，
/// **后注册的覆盖先注册的**，先那个静默失去可选中（不 panic、不报错）。
/// 所以本函数的 id 取 `NamedChild(CodeLocation(调用点), 文本内容)`：
/// 调用点保证「不同代码位置」不撞，文本内容保证「同一处渲染多份」（循环 / 列表）
/// 不撞 —— 于是循环里**不必**手工造号。
///
/// 代价：完全相同的两段文字若由同一行代码渲染，会共用 handle（互相同步选区）。
/// 这种位置请改用 [`selectable_text_with_id`] 显式给号。
#[track_caller]
pub fn selectable_text(text: impl Into<SharedString>) -> SelectableText {
    selectable_text_at(std::panic::Location::caller(), text)
}

/// [`selectable_text`] 的「调用点由调用方传入」版本。
///
/// 只给**样式 helper**（`body_text` / `meta_text`）用：它们自己被千余处调用，
/// 若在 helper 体内调 [`selectable_text`]，id 的「调用点」会全部落在 helper 内部
/// 那一行 ⇒ 所有调用点挤在同一个 id 上（只剩文本内容能区分，重复文字就会撞）。
/// 所以 helper 一律自己 `#[track_caller]` 取 caller，再透传给本函数。
#[track_caller]
pub fn selectable_text_at(
    caller: &'static std::panic::Location<'static>,
    text: impl Into<SharedString>,
) -> SelectableText {
    let text = text.into();
    let id = ElementId::from((ElementId::CodeLocation(*caller), text.clone()));
    SelectableText::new(id, text)
}

/// 同 [`selectable_text`]，但 id 由调用方给 —— 循环 / 列表 / 表格单元格走这条。
///
/// `id` 必须同时满足两点：**同一帧内唯一**（否则选区串行）且**跨帧稳定**
/// （否则选区挂在旧文本上）。所以用「内容键」而不是「本帧序号」。
///
/// 另外：**跨多段拖选**的复制顺序只由 `SelectableText::document_order` 决定
/// （引擎按它排序、`\n` 拼接；都为 0 时顺序不定）。相邻的多段文字若希望复制顺序
/// 稳定，用本函数配 `.document_order(n)` 显式编号。
pub fn selectable_text_with_id(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
) -> SelectableText {
    SelectableText::new(id, text)
}

/// 正文文字（`typography.sm`，正文色）。文字**可拖选复制**。
#[track_caller]
pub fn body_text(text: impl Into<SharedString>, cx: &App) -> Div {
    let tokens = active_theme(cx).tokens();
    div()
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .text_color(tokens.colors.foreground)
        .child(selectable_text_at(std::panic::Location::caller(), text))
}

/// 辅助说明 / 元信息文字（`typography.xs`，二级文字色）。文字**可拖选复制**。
#[track_caller]
pub fn meta_text(text: impl Into<SharedString>, cx: &App) -> Div {
    let tokens = active_theme(cx).tokens();
    div()
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(selectable_text_at(std::panic::Location::caller(), text))
}

/// 分区 / 组标题文字（`xs` MEDIUM，三级文字色）。
pub fn caption_heading(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let xs = theme.tokens().typography.xs;
    div()
        .text_size(xs.size)
        .line_height(xs.line_height)
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.extended().colors.text_tertiary)
        .child(text.into())
}

/// 页面 / 对话框 / 区块标题文字（`md`，macOS Title 3 档，半粗 + 正文色）。
///
/// 全站**唯一**的标题口径：列表页与详情页的页内标题、`Dialog::title`、以及将来的
/// 区块大标题都走这里。此前概览 / 设置 / 占位页各写一份实现、插件中心详情标题另取
/// `lg` 档（18px，阶梯里没有的层级），同一屏里出现四种「标题」字号。
///
/// 字重固定 SEMIBOLD：macOS HIG 的 Title 3 强调字重就是 Semibold，标题与正文的区别
/// 不能只靠 2px 字号差表达。
pub fn title_text(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .text_size(tokens.typography.md.size)
        .line_height(tokens.typography.md.line_height)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(tokens.colors.foreground)
        .child(text.into())
}

/// 详情视图标题文字（`lg`，macOS Title 2 档，半粗 + 正文色）。
///
/// 与 [`title_text`] 的关系照搬 macOS 的两级标题：列表 / 页面标题用 Title 3，
/// **详情视图**（从列表钻进去的那一层）用 Title 2。全站目前只有插件中心详情页用到。
pub fn detail_title_text(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .text_size(tokens.typography.lg.size)
        .line_height(tokens.typography.lg.line_height)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(tokens.colors.foreground)
        .child(text.into())
}

/// 设置行（横排）：标题 / 说明在左，控件右对齐、可收缩。
///
/// 行高保底 `density.control` + [`ROW_MIN_PADDING`]，标题列保底 [`TITLE_MIN_WIDTH`]。
/// 放进 [`option_group`] 里使用。
pub fn option_row(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .flex()
        .items_center()
        .w_full()
        .gap(tokens.spacing.lg)
        .px(tokens.spacing.lg)
        .py(tokens.spacing.sm)
        .min_h(theme.density().control + px(ROW_MIN_PADDING))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(TITLE_MIN_WIDTH))
                .gap(tokens.spacing.xxs)
                .child(body_text(title, cx))
                .when_some(description, |this, description| {
                    this.child(meta_text(description, cx))
                }),
        )
        .child(div().min_w_0().child(control))
}

/// 设置行（竖排）：标题 / 说明在上，控件独占一行（长输入、选择器、卡片网格）。
pub fn option_row_stacked(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> Div {
    let tokens = active_theme(cx).tokens();
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.lg)
        .py(tokens.spacing.md)
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .gap(tokens.spacing.xxs)
                .child(body_text(title, cx))
                .when_some(description, |this, description| {
                    this.child(meta_text(description, cx))
                }),
        )
        .child(div().w_full().min_w_0().child(control))
}

/// 设置分区：组标题 + 一张分组卡片（行间 hairline 左侧留白分隔）。
pub fn settings_section(
    title: Option<SharedString>,
    rows: impl IntoIterator<Item = AnyElement>,
    cx: &App,
) -> Div {
    let tokens = active_theme(cx).tokens();
    div()
        .flex()
        .flex_col()
        .w_full()
        .children(title.map(|title| {
            caption_heading(title, cx)
                .px(tokens.spacing.xs)
                .pb(tokens.spacing.sm)
        }))
        .child(option_group(rows, cx))
}

/// 设置分组卡片：`card` + 行间 hairline 分隔线；分隔线左侧留 `spacing.lg` 不贯通，
/// 与设置行左内边距对齐（macOS 分组表单风格）。
pub fn option_group(rows: impl IntoIterator<Item = AnyElement>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let hairline = extended.colors.hairline;
    // 行间分隔线是一根 1px hairline（不随缩放变粗），宽度取 `stroke.thin`。
    let hairline_w = extended.stroke.thin;
    let mut group = crate::card(cx).flex().flex_col().w_full().overflow_hidden();
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            group = group.child(
                div()
                    .w_full()
                    .h(hairline_w)
                    .pl(tokens.spacing.lg)
                    .child(div().size_full().bg(hairline)),
            );
        }
        group = group.child(row);
    }
    group
}

/// 纯图标（或图标 + 短标记）下拉按钮：外观与 [`crate::toolbar_action_button`] 同一档
/// （幽灵底 + `density.toolbar_button` 高度 + 二级文字色内容），点击弹出下拉菜单。
///
/// 为什么不直接用 `toolbar_action_button`：上游 `DropdownMenu` 只给 gpui-component 的
/// `Button` 实现（`impl DropdownMenu for Button`），而 Duck 的 chrome 按钮是 gpui-base 的
/// `Button`。这里按同一套 token 重拼一个，保证与相邻的顶栏图标按钮视觉一致。
///
/// 尺寸只钉**高度**，宽度随内容流（`Styled::size` 会同时钉死宽高成方形，
/// 图标 + 短标记会被裁掉）；水平内边距取 `(高度 - 图标) / 2`，图标落点与
/// 方形图标按钮完全重合，内容加宽时向右伸展。
///
/// 工具条纯图标下拉的**触发按钮**（不带菜单主体）：[`toolbar_dropdown_button`] 与
/// [`toolbar_searchable_dropdown`] 共用的外壳。
fn toolbar_dropdown_trigger(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    cx: &App,
) -> Button {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let tooltip_label = label.into();
    let height = theme.density().toolbar_button;
    Button::new(id.into())
        .ghost()
        .flex_none()
        .h(height)
        // 方形图标按钮的图标落点：内容从 `高度-图标)/2` 处起排。
        .px((height - theme.extended().icon.md) / 2.)
        .items_center()
        .rounded(theme.components().button_radius)
        .text_color(tokens.colors.muted_foreground)
        .accessibility_label(tooltip_label.clone())
        // 纯图标必须有悬停提示（与无障碍名同一份文案）。gpui-component 的 Button
        // 自带 tooltip 字段（内部渲染，不走 gpui 的 div 挂载，无「二次挂载」panic
        // 之说）。
        .tooltip(tooltip_label)
        .child(icon)
}

/// 工具条纯图标下拉：点按弹出 [`PopupMenu`]。
///
/// `anchor` 决定菜单相对触发器的弹出方向（顶栏控件用 `Anchor::BottomRight`，避免向上出界）。
pub fn toolbar_dropdown_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    anchor: Anchor,
    build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    cx: &App,
) -> AnyElement {
    toolbar_dropdown_trigger(id, label, icon, cx)
        .dropdown_menu_with_anchor(anchor, build)
        .into_any_element()
}

/// 文字版下拉的**触发按钮**（图标 + 文字，不带菜单主体）：[`toolbar_text_dropdown_button`]
/// 与 [`toolbar_text_searchable_dropdown`] 共用的外壳。
fn toolbar_text_dropdown_trigger(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    cx: &App,
) -> Button {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let label = label.into();
    let height = theme.density().toolbar_button;
    Button::new(id.into())
        .ghost()
        .flex_none()
        .h(height)
        .px(tokens.spacing.xs + tokens.spacing.xxs)
        .flex()
        .items_center()
        .gap(tokens.spacing.xxs)
        .rounded(theme.components().button_radius)
        .text_color(tokens.colors.muted_foreground)
        .text_size(tokens.typography.sm.size)
        .accessibility_label(label.clone())
        // 提示与无障碍名共用同一份文案（纯图标按钮同款纪律）：触发器上那点文字
        // 常常只是「当前值」，看不出点开能改什么 —— 调用方给的 `label` 才是说明。
        .tooltip(label.clone())
        .child(icon)
        .child(label)
}

/// 文字版下拉触发按钮：图标 + **文字** + caret，外观与 [`toolbar_dropdown_button`]
/// 同一档（幽灵底 + `density.toolbar_button` 高度）。
///
/// 为什么不直接给 [`toolbar_dropdown_button`] 渲染 label：它的既有调用点（语言切换、
/// 邮件工具条等）全部按「纯图标」排版，补文字会同时改一排不相干的界面。设计稿里
/// globalbar 的「更多」与结果脚注的「导出 ∨」都是文字按钮，用这个。
pub fn toolbar_text_dropdown_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    anchor: Anchor,
    build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    cx: &App,
) -> AnyElement {
    toolbar_text_dropdown_trigger(id, label, icon, cx)
        .dropdown_menu_with_anchor(anchor, build)
        .into_any_element()
}

/// 工具条纯图标可搜索下拉：搜索框**钉在弹层顶部**（[`SearchableDropdown`]）。
///
/// 与 [`toolbar_dropdown_button`] + PopupMenu 的差别只在弹层结构 ——
/// 候选多到要滚动时，PopupMenu 版的搜索框会被一起卷走（structure 编辑器的
/// 类型下拉实况），这一档不会。
#[allow(clippy::too_many_arguments)]
pub fn toolbar_searchable_dropdown(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    anchor: Anchor,
    options: Vec<MenuItemSpec>,
    search_placeholder: Option<SharedString>,
    on_pick: ValuePick,
    cx: &App,
) -> AnyElement {
    let id = id.into();
    let trigger = toolbar_dropdown_trigger(id.clone(), label, icon, cx);
    SearchableDropdown::new(
        searchable_popover_id(&id),
        anchor,
        trigger,
        options,
        search_placeholder,
        on_pick,
    )
    .into_any_element()
}

/// 文字版可搜索下拉：搜索框**钉在弹层顶部**（见 [`toolbar_searchable_dropdown`]）。
#[allow(clippy::too_many_arguments)]
pub fn toolbar_text_searchable_dropdown(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    anchor: Anchor,
    options: Vec<MenuItemSpec>,
    search_placeholder: Option<SharedString>,
    on_pick: ValuePick,
    cx: &App,
) -> AnyElement {
    let id = id.into();
    let trigger = toolbar_text_dropdown_trigger(id.clone(), label, icon, cx);
    SearchableDropdown::new(
        searchable_popover_id(&id),
        anchor,
        trigger,
        options,
        search_placeholder,
        on_pick,
    )
    .into_any_element()
}

/// 可搜索下拉弹层的元素 id：触发器 id 加前缀 —— 既不与按钮元素状态撞键，
/// 也不与 `Popover` 内部的 keyed state 撞键（`DropdownMenuPopover` 同一款处置）。
fn searchable_popover_id(trigger_id: &ElementId) -> SharedString {
    SharedString::from(format!("searchable-dropdown:{trigger_id:?}"))
}

/// 下拉菜单的高度上限：候选再多，弹层也不长过这个高度（超出部分滚动）。
///
/// gpui-component 的 `PopupMenu` **只有 `scrollable(true)` 时才有 `max_h`**，且它的
/// 默认值是 `min(半窗高, 450px)` —— 26 个类型候选排下来是一根 450px 的长条，找一项
/// 得从头扫到尾；不调 `scrollable` 更是**完全不限高**（清单能长到屏幕外）。这里统一
/// 收到一屏能一眼扫完的量，并顺手把纵向滚动条打开。
pub const MENU_MAX_HEIGHT: f32 = 264.0;

/// 候选超过这个数才在菜单顶部给搜索框。
///
/// 三个五个选项配一个搜索框只是多一层噪音；类型候选（26 项）、字段清单（动辄上百）
/// 这类才需要。
pub const MENU_SEARCH_THRESHOLD: usize = 8;

/// 下拉菜单的一档候选。
#[derive(Clone)]
pub struct MenuItemSpec {
    /// 选中时回传给回调的稳定值。
    pub value: SharedString,
    /// 展示文案；**搜索也按它匹配**（值只是回传口径，可能带前缀之类）。
    pub label: SharedString,
    /// 勾选态（多选菜单用；单选菜单恒 `false`）。
    pub checked: bool,
}

impl MenuItemSpec {
    pub fn new(value: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        let value = value.into();
        Self {
            label: label.into(),
            value,
            checked: false,
        }
    }

    #[must_use]
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
}

/// 过滤口径：`label` 里含 `query`（大小写不敏感）；空查询即全通过。
fn menu_label_matches(label: &str, query: &str) -> bool {
    query.is_empty() || label.to_lowercase().contains(&query.to_lowercase())
}

/// 菜单项被选中：把该选项的 `value` 交回调用方。
///
/// 可搜索下拉（[`SearchableDropdown`]）与各下拉入口共用同一个
/// 签名；不给别名的话 clippy 的 `type_complexity` 会在四处各报一遍
/// （同 [`TabSelect`] / [`NavSelect`] 的处置）。
type ValuePick = Rc<dyn Fn(SharedString, &mut Window, &mut App)>;

/// 把候选装进菜单（PopupMenu 档，不带搜索）。
fn fill_menu(menu: PopupMenu, options: &[MenuItemSpec], on_pick: &ValuePick) -> PopupMenu {
    options.iter().fold(menu, |menu, option| {
        let on_pick = Rc::clone(on_pick);
        let value = option.value.clone();
        menu.item(
            PopupMenuItem::new(option.label.clone())
                .checked(option.checked)
                .on_click(move |_, window, cx| on_pick(value.clone(), window, cx)),
        )
    })
}

/// [`SearchableDropdown`] 弹层里的清单：**钉顶搜索行** + 限高滚动的候选行。
///
/// 独立实体的理由与被它替代的 `MenuSearch` 相同：搜索词一变就要重筛候选，而
/// 「订阅输入变化」得有一条挂得住的生命周期 —— 只有实体自己的字段存得住它。
/// 与 PopupMenu 版的差异：没有方向键 / 回车导航（PopupMenu 的 key context 不在了），
/// 主路径与「选择列」一致 —— 打字过滤、点选。
struct SearchList {
    /// 搜索输入框（`None` = 候选没过 [`MENU_SEARCH_THRESHOLD`]，不出搜索行）。
    input: Option<Entity<InputState>>,
    options: Vec<MenuItemSpec>,
    on_pick: ValuePick,
    query: String,
    /// 订阅自己拿着：实体活着，订阅就活着；弹层关掉（实体被丢）时一起释放。
    _subscription: Option<Subscription>,
}

impl SearchList {
    fn new(
        options: Vec<MenuItemSpec>,
        placeholder: Option<SharedString>,
        on_pick: ValuePick,
        searchable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if !searchable {
            return Self {
                input: None,
                options,
                on_pick,
                query: String::new(),
                _subscription: None,
            };
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.unwrap_or_default()));
        let subscription =
            cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let query = input.read(cx).value().to_string();
                if query == this.query {
                    return;
                }
                this.query = query;
                cx.notify();
            });
        Self {
            input: Some(input),
            options,
            on_pick,
            query: String::new(),
            _subscription: Some(subscription),
        }
    }
}

impl Render for SearchList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let on_pick = self.on_pick.clone();
        // 勾选标记占位：只要有一项带勾选态，所有行都留出同宽的左槽，标签才对得齐。
        let any_checked = self.options.iter().any(|option| option.checked);

        let search_row = self.input.clone().map(|input| {
            h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap(tokens.spacing.xs)
                .pb(tokens.spacing.xs)
                // 与清单分隔：参考「选择列」弹层的一条 hairline。
                .border_b(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(
                    Icon::new(DuckIcon::Search)
                        .size(extended.icon.sm)
                        .text_color(extended.colors.text_tertiary),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&input)
                            .appearance(false)
                            .with_size(Size::Small)
                            .w_full(),
                    ),
                )
        });

        let rows = self
            .options
            .iter()
            .enumerate()
            .filter(|(_, option)| menu_label_matches(&option.label, &self.query))
            .map(|(index, option)| {
                let on_pick = on_pick.clone();
                let value = option.value.clone();
                h_flex()
                    .id(("searchable-option", index))
                    .min_h(px(26.))
                    .px(px(8.))
                    .rounded(tokens.radius.sm)
                    .cursor_pointer()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.foreground)
                    .role(Role::MenuItem)
                    .aria_label(option.label.clone())
                    .hover(move |style| style.bg(extended.colors.hover_tint))
                    .on_click(move |_, window, cx| on_pick(value.clone(), window, cx))
                    .when(any_checked, |row| {
                        row.child(div().w(extended.icon.sm).flex_none().when(
                            option.checked,
                            |slot| {
                                slot.child(
                                    Icon::new(DuckIcon::CheckboxCheck)
                                        .size(extended.icon.sm)
                                        .text_color(tokens.colors.foreground),
                                )
                            },
                        ))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(option.label.clone()),
                    )
            });

        v_flex().w_full().min_w_0().children(search_row).child(
            v_flex()
                .id("searchable-list")
                .w_full()
                .min_w_0()
                .max_h(px(MENU_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(rows),
        )
    }
}

/// 可搜索下拉（**搜索框钉顶**档）：触发按钮 + 弹层 = 固定搜索行 + 限高滚动的候选
/// 清单，观感与 gpui-component `Select`（结果区「选择列」）一致。
///
/// 为什么不走 `PopupMenu`（`searchable_menu` 那条路）：它的 `scrollable` 把搜索行
/// （也是一个 menu item）和候选项装进**同一个**滚动容器，候选一多、随手一滚搜索框
/// 就出屏（structure 编辑器的类型下拉实况）。这里搜索行钉在弹层顶部，不参与滚动。
///
/// 开关走 `Popover` 的**受控**档（`open` + `on_open_change`，见其文档约定）：开关
/// 状态放在自己的 keyed state 里，选中候选时置回 `false` 关层。
///
/// 清单实体在 **`on_open_change`（开层那一刻，事件回调）** 里建一份、关层即丢 ——
/// 重开按最新候选 / 勾选态重建，搜索词随之清空（旧 `searchable_menu`「每次打开
/// 重跑构建闭包」的同一口径）。**不能**挪进 `content`：那是**渲染期**回调
/// （`Popover::content` 文档明说别在里建实体），每次重绘都会走一遍。
#[derive(IntoElement)]
pub struct SearchableDropdown {
    /// 弹层元素 id（由触发器 id 派生，见 [`searchable_popover_id`]）。
    id: SharedString,
    anchor: Anchor,
    trigger: Button,
    options: Vec<MenuItemSpec>,
    search_placeholder: Option<SharedString>,
    on_pick: ValuePick,
}

impl SearchableDropdown {
    pub fn new(
        id: impl Into<SharedString>,
        anchor: Anchor,
        trigger: Button,
        options: Vec<MenuItemSpec>,
        search_placeholder: Option<SharedString>,
        on_pick: ValuePick,
    ) -> Self {
        Self {
            id: id.into(),
            anchor,
            trigger,
            options,
            search_placeholder,
            on_pick,
        }
    }
}

impl RenderOnce for SearchableDropdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // ⚠️ keyed state 的键不能与 `Popover` 内部（用 `self.id` 原样）或触发按钮
        // 元素撞键，所以各自加后缀。
        let open_state = window.use_keyed_state(
            SharedString::from(format!("{}:open", self.id)),
            cx,
            |_, _| false,
        );
        let list_state = window.use_keyed_state(
            SharedString::from(format!("{}:list", self.id)),
            cx,
            |_, _| None::<Entity<SearchList>>,
        );
        let searchable =
            self.search_placeholder.is_some() && self.options.len() > MENU_SEARCH_THRESHOLD;
        let open = *open_state.read(cx);
        // 候选 / 占位 / 回调要同时进两个闭包（开层回调建清单、content 只读渲染它），
        // 走 `Rc` 共享：避免按帧整表克隆候选。
        let options = Rc::new(self.options);
        let placeholder = self.search_placeholder;
        let on_pick = self.on_pick;

        Popover::new(self.id.clone())
            .anchor(self.anchor)
            // `Popover` 外观档默认 `p_3`（12px），压回菜单密度。
            .p_1()
            .open(open)
            .on_open_change({
                let open_state = open_state.clone();
                let list_state = list_state.clone();
                let options = options.clone();
                let placeholder = placeholder.clone();
                let on_pick = on_pick.clone();
                move |open, window, cx| {
                    // 开层：**在这里**建清单实体（事件回调期），不在 `content` 里建。
                    if *open && list_state.read(cx).is_none() {
                        // 选中即关层：pick 包一层，回传值之后把受控开关置回 false。
                        let pick: ValuePick = {
                            let on_pick = on_pick.clone();
                            let open_state = open_state.clone();
                            Rc::new(move |value, window, cx| {
                                on_pick(value, window, cx);
                                open_state.update(cx, |slot, cx| {
                                    *slot = false;
                                    cx.notify();
                                });
                            })
                        };
                        let list = cx.new(|list_cx| {
                            SearchList::new(
                                (*options).clone(),
                                placeholder.clone(),
                                pick,
                                searchable,
                                window,
                                list_cx,
                            )
                        });
                        // 开层即定位到搜索框（与「选择列」一致）；短清单无框可 focus。
                        if let Some(input) = list.read(cx).input.clone() {
                            input.read(cx).focus_handle(cx).focus(window, cx);
                        }
                        list_state.update(cx, |slot, cx| {
                            *slot = Some(list);
                            cx.notify();
                        });
                    }
                    open_state.update(cx, |slot, cx| {
                        *slot = *open;
                        cx.notify();
                    });
                    if !*open {
                        list_state.update(cx, |slot, cx| {
                            *slot = None;
                            cx.notify();
                        });
                    }
                }
            })
            .trigger(self.trigger)
            .content({
                let list_state = list_state.clone();
                move |_, _, cx| {
                    // 只读：清单要么在（开层时建好），要么为 None（未开 / 已关）。
                    // `Popover` 在 `open == false` 时根本不渲染 content，这里的空态不落屏。
                    v_flex()
                        .min_w(px(DROPDOWN_MIN_WIDTH))
                        .children(list_state.read(cx).clone())
                }
            })
            .into_any_element()
    }
}

/// 下拉选择按钮：`outline` 变体 + 统一控件高度 + 尾部 caret；菜单项带勾选态。
///
/// `options` 为 `(值, 显示文案)`；`current` 与值比对决定按钮文案与勾选项。
pub fn dropdown_select(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (SharedString, SharedString)>,
    current: impl Into<SharedString>,
    on_select: impl Fn(SharedString, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    dropdown_select_with(id, options, current, None, on_select, cx)
}

/// 同 [`dropdown_select`]，但菜单顶部可搜索（候选多时自动出现搜索框）。
///
/// `search_placeholder` 是搜索框占位符 —— 文案由调用方给（本 crate 不做 i18n，
/// 界面文案一律由页面用 `tr` 取词后传进来）。
pub fn dropdown_select_searchable(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (SharedString, SharedString)>,
    current: impl Into<SharedString>,
    search_placeholder: impl Into<SharedString>,
    on_select: impl Fn(SharedString, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    dropdown_select_with(
        id,
        options,
        current,
        Some(search_placeholder.into()),
        on_select,
        cx,
    )
}

/// [`dropdown_select`] 与 [`dropdown_select_searchable`] 的公共实现。
fn dropdown_select_with(
    id: impl Into<ElementId>,
    options: impl IntoIterator<Item = (SharedString, SharedString)>,
    current: impl Into<SharedString>,
    search_placeholder: Option<SharedString>,
    on_select: impl Fn(SharedString, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let current: SharedString = current.into();
    let options: Vec<(SharedString, SharedString)> = options.into_iter().collect();
    let label = options
        .iter()
        .find(|(value, _)| *value == current)
        .map_or_else(|| current.clone(), |(_, label)| label.clone());
    let specs: Vec<MenuItemSpec> = options
        .iter()
        .map(|(value, label)| {
            MenuItemSpec::new(value.clone(), label.clone()).checked(*value == current)
        })
        .collect();
    let on_select: ValuePick = Rc::new(on_select);
    let id = id.into();
    let trigger = Button::new(id.clone())
        .outline()
        .control(cx)
        .label(label)
        .dropdown_caret(true)
        .min_w(px(DROPDOWN_MIN_WIDTH));
    match search_placeholder {
        // 可搜索档：搜索框钉顶（`SearchableDropdown`），不用 PopupMenu ——
        // 它的滚动容器会把搜索行一起卷走。
        Some(placeholder) => SearchableDropdown::new(
            searchable_popover_id(&id),
            Anchor::TopRight,
            trigger,
            specs,
            Some(placeholder),
            on_select,
        )
        .into_any_element(),
        None => trigger
            .dropdown_menu_with_anchor(Anchor::TopRight, {
                move |menu, _, _| fill_menu(menu, &specs, &on_select)
            })
            .into_any_element(),
    }
}

/// 开关控件：选中底为强调色，与设置行的控件列同高。
///
/// 回调签名与 gpui-component `Switch::on_change` 一致，可直接传 `cx.listener(...)`。
pub fn switch_control(
    id: impl Into<ElementId>,
    checked: bool,
    on_toggle: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Switch {
    Switch::new(id).checked(checked).on_change(on_toggle)
}
