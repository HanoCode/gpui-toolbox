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
    Anchor, AnyElement, App, Bounds, BoxShadow, Context, Div, ElementId, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement, Pixels, RenderOnce, Role, SharedString,
    StatefulInteractiveElement as _, Styled, Toggled, Window, div, point,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::{
    Sizable as _, Size,
    button::{Button, ButtonVariants as _},
    input::Input,
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    switch::Switch,
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
        .p(px(2.))
        .rounded(tab_radius + px(1.))
        .bg(theme.extended().colors.nav_hover)
        .child(SegmentedTabs {
            id: id.into(),
            labels: labels.into_iter().collect(),
            selected,
            on_select: Rc::new(on_select),
            style: SegmentedStyle {
                height: control - px(4.),
                radius: tab_radius,
                padding_x: tokens.spacing.md,
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
    labels: Vec<SharedString>,
    selected: usize,
    on_select: TabSelect,
    style: SegmentedStyle,
}

impl RenderOnce for SegmentedTabs {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            labels,
            selected,
            on_select,
            style,
        } = self;
        let count = labels.len();

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
                    .gap(px(2.))
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
                    .children(labels.into_iter().zip(text_colors).enumerate().map(
                        |(index, (label, text_color))| {
                            let active = index == selected;
                            let on_select = Rc::clone(&on_select);
                            // 未选中标签：悬停底色走强调色薄层 + 文字转强调色。
                            let (foreground, ring) = (style.accent_text, style.ring);
                            let hover_tint = style.hover_tint;
                            div()
                                .id(ElementId::NamedInteger(
                                    SharedString::from(format!("{id}-tab")),
                                    index as u64,
                                ))
                                .h(style.height)
                                .px(style.padding_x)
                                .flex()
                                .items_center()
                                .rounded(style.radius)
                                .text_size(style.text_size)
                                .line_height(style.line_height)
                                .cursor_pointer()
                                .text_color(text_color)
                                // 无障碍：分段控件对屏幕阅读器是一组「页签」，选中态必须报出去，
                                // 否则读屏只会念到两个标签名，用户不知道当前在哪一档。
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
pub fn field_hint(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let xs = theme.tokens().typography.xs;
    div()
        .text_size(xs.size)
        .line_height(xs.line_height)
        .text_color(theme.extended().colors.text_tertiary)
        .child(text.into())
}

/// 字段错误：12px 危险色。
pub fn field_error(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let xs = theme.tokens().typography.xs;
    div()
        .text_size(xs.size)
        .line_height(xs.line_height)
        .text_color(theme.tokens().colors.destructive)
        .child(text.into())
}

/// 标准字段：标签 → 控件 → 可选说明。标签与控件、控件与说明间距 6px。
pub fn form_field(
    label: impl Into<SharedString>,
    control: impl IntoElement,
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

/// 正文文字（`typography.sm`，正文色）。
pub fn body_text(text: impl Into<SharedString>, cx: &App) -> Div {
    let tokens = active_theme(cx).tokens();
    div()
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .text_color(tokens.colors.foreground)
        .child(text.into())
}

/// 辅助说明 / 元信息文字（`typography.xs`，二级文字色）。
pub fn meta_text(text: impl Into<SharedString>, cx: &App) -> Div {
    let tokens = active_theme(cx).tokens();
    div()
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(text.into())
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
    let hairline = theme.extended().colors.hairline;
    let mut group = crate::card(cx).flex().flex_col().w_full().overflow_hidden();
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            group = group.child(
                div()
                    .w_full()
                    .h(px(1.))
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
/// `anchor` 决定菜单相对触发器的弹出方向（顶栏控件用 `Anchor::BottomRight`，避免向上出界）。
pub fn toolbar_dropdown_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    anchor: Anchor,
    build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    cx: &App,
) -> AnyElement {
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
        .accessibility_label(tooltip_label)
        .child(icon)
        .dropdown_menu_with_anchor(anchor, build)
        .into_any_element()
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
    let current: SharedString = current.into();
    let options: Vec<(SharedString, SharedString)> = options.into_iter().collect();
    let label = options
        .iter()
        .find(|(value, _)| *value == current)
        .map_or_else(|| current.clone(), |(_, label)| label.clone());
    let on_select: Rc<dyn Fn(SharedString, &mut Window, &mut App)> = Rc::new(on_select);
    Button::new(id)
        .outline()
        .control(cx)
        .label(label)
        .dropdown_caret(true)
        .min_w(px(DROPDOWN_MIN_WIDTH))
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            options.iter().fold(menu, |menu, (value, label)| {
                let checked = *value == current;
                let value = value.clone();
                let on_select = Rc::clone(&on_select);
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .checked(checked)
                        .on_click(move |_, window, cx| on_select(value.clone(), window, cx)),
                )
            })
        })
        .into_any_element()
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
