//! Duck Toolbox 视觉组件库。
//!
//! gpui-base 提供交互、键盘与无障碍语义；本 crate 只负责从完整主题 token 组装稳定的
//! shadcn 风格外观。业务组件依赖这里，不直接散落颜色和尺寸字面量。
//!
//! 结构与 FluxDown `fluxdown_ui_components` 对应：按钮族 / 图标 / 交互动效 / 表单原语 /
//! 侧栏面板 / 弹窗外壳六块。

pub mod command_palette;
pub mod content;
pub mod data_table;
pub mod dblogos;
mod dialog;
mod icons;
mod kit;
mod maillogos;
mod motion;
pub mod notify;
mod sidebar;

pub use dblogos::DatabaseLogo;
pub use dialog::{
    ActionHandler, ConfirmHandler, FooterAction, ModalDrag, ModalOffset, dialog_footer,
    dialog_footer_with_action, dialog_footer_with_hint, dialog_top_offset, modal_drag,
    modal_drag_area, modal_offset, modal_title_row,
};
pub use icons::{ComponentAssets, DuckIcon, LOGO_ASSET_PATH};
pub use kit::{
    ControlExt, DROPDOWN_MIN_WIDTH, IconControlExt, NavItem, ROW_MIN_PADDING, SegmentItem,
    TITLE_MIN_WIDTH, body_text, caption_heading, caption_number, check_row, detail_title_text,
    dropdown_select, field_error, field_hint, field_label, field_success, form, form_field,
    form_gap, form_row, input_with_action, meta_text, nav_list, option_group, option_row,
    option_row_stacked, segmented_tabs, segmented_tabs_with_icons, settings_section, status_button,
    status_button_content, switch_control, title_text, toolbar_dropdown_button,
    toolbar_text_dropdown_button,
};
pub use maillogos::MailLogo;
pub use motion::{SlidingHighlight, SlidingRowHighlight, StateLayer, color_transition};
pub use sidebar::{SidebarChange, SidebarPanel, SidebarState};

use duck_theme::active_theme;
use gpui::{
    App, Div, ElementId, FontFeatures, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, Pixels, Role, SharedString, StatefulInteractiveElement as _, Styled, div, px,
};
pub use gpui_base::Button;
use gpui_component::tooltip::Tooltip;

/// 给图标按钮挂悬停提示。
///
/// 图标按钮**没有可见文字**：`accessibility_label` 只有读屏消费得到，鼠标用户悬停时
/// 依然看不出这个图标是干什么的（此前全仓的图标按钮都是这样）。提示与读屏标签
/// 共用同一份文案 —— 两者本来就该是同一句话，分两份写迟早会漂移。
fn with_icon_tooltip(button: Button, label: SharedString) -> Button {
    button.tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
}

/// 等宽数字（OpenType `tnum`）：计数、大小、百分比等会刷新的数字统一使用，
/// 避免比例数字在刷新时左右跳动。
#[must_use]
pub fn tabular_numbers() -> FontFeatures {
    FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)]))
}

/// 按钮的视觉语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Ghost,
    /// 外链 / 跳转类文字操作：幽灵按钮外观 + `colors.accentText` 文字。
    Link,
    Destructive,
}

/// 创建具备键盘、焦点、无障碍与完整交互态的主题按钮。
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    variant: ButtonVariant,
    cx: &App,
) -> Button {
    let label = label.into();
    text_button_frame(id, variant, cx)
        .accessibility_label(label.clone())
        .child(label)
}

/// 带前置图标的文字按钮（图标 + 文字横排），其余外观与 [`button`] 完全一致。
pub fn icon_text_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    variant: ButtonVariant,
    cx: &App,
) -> Button {
    let label = label.into();
    text_button_frame(id, variant, cx)
        .accessibility_label(label.clone())
        .child(icon)
        .child(label)
}

/// 文字按钮的公共外观（不含内容）。
fn text_button_frame(id: impl Into<ElementId>, variant: ButtonVariant, cx: &App) -> Button {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let palette = ButtonPalette::for_variant(variant, tokens.colors, theme.extended().colors);
    let stroke = theme.extended().stroke.thin;
    let disabled_opacity = theme.extended().opacity.disabled;

    Button::new(id)
        .h(theme.density().control)
        .px(tokens.spacing.sm + tokens.spacing.xxs)
        .line_height(tokens.typography.sm.line_height)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .rounded(theme.components().button_radius)
        .border(stroke)
        .border_color(palette.border)
        .bg(palette.background)
        .text_color(palette.foreground)
        .text_size(tokens.typography.sm.size)
        .font_weight(tokens.typography.sm.weight)
        .focus_visible(move |style| style.border_color(tokens.colors.ring))
        .styles(move |styles| styles.disabled(move |style| style.opacity(disabled_opacity)))
        .child(palette.state_layer(theme.components().button_radius, stroke))
}

/// 创建仅图标的方形按钮（`density.control` 尺寸，与同行输入框等高）。
///
/// `label` 只进无障碍名与悬停提示，不渲染文字 —— 两者本来就该是同一句话
/// （见 [`with_icon_tooltip`]）。提示想比无障碍名多说一句（例如带上当前状态）
/// 用 [`icon_button_tip`]。
pub fn icon_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    variant: ButtonVariant,
    cx: &App,
) -> Button {
    let label: SharedString = label.into();
    with_icon_tooltip(icon_button_frame(id, label.clone(), variant, cx), label).child(icon)
}

/// 同 [`icon_button`]，但悬停提示单独给：无障碍名用 `label`（简短、读屏友好），
/// 提示用 `tip`（可带状态、补充说明或动态读数）。
///
/// ⚠️ **提示只能挂一次**：gpui 对同一元素二次 `.tooltip()` 会直接 panic
/// （`div.rs` 里那句 `calling tooltip more than once on the same element`，
/// 且是 abort 级别，整个程序秒退）。所以提示统一在组件内部挂，
/// 调用方**不要**再在这个返回值上挂 `.tooltip()`。
pub fn icon_button_tip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    tip: impl Into<SharedString>,
    variant: ButtonVariant,
    cx: &App,
) -> Button {
    with_icon_tooltip(icon_button_frame(id, label, variant, cx), tip.into()).child(icon)
}

/// 图标按钮的公共外观与其无障碍名（**不含提示**）。
///
/// 提示单独由 [`icon_button`] / [`icon_button_tip`] 挂：挂载点唯一，
/// 调用方就没有「再挂一次」的机会。
fn icon_button_frame(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    variant: ButtonVariant,
    cx: &App,
) -> Button {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let palette = ButtonPalette::for_variant(variant, tokens.colors, theme.extended().colors);
    let radius = theme.components().button_radius;
    let stroke = theme.extended().stroke.thin;
    let disabled_opacity = theme.extended().opacity.disabled;
    let label: SharedString = label.into();

    Button::new(id)
        .size(theme.density().control)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .rounded(radius)
        .border(stroke)
        .border_color(palette.border)
        .bg(palette.background)
        .text_color(palette.foreground)
        .focus_visible(move |style| style.border_color(tokens.colors.ring))
        .styles(move |styles| styles.disabled(move |style| style.opacity(disabled_opacity)))
        .accessibility_label(label)
        .child(palette.state_layer(radius, stroke))
}

/// 创建带前置图标的主要操作按钮。
pub fn primary_icon_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    cx: &App,
) -> Button {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let label = label.into();
    let palette = ButtonPalette::for_variant(
        ButtonVariant::Primary,
        tokens.colors,
        theme.extended().colors,
    );
    let radius = theme.components().button_radius;
    let stroke = theme.extended().stroke.thin;

    Button::new(id)
        .h(theme.density().control)
        .px(tokens.spacing.sm + tokens.spacing.xxs)
        .line_height(tokens.typography.sm.line_height)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .rounded(radius)
        .border(stroke)
        .border_color(palette.border)
        .bg(palette.background)
        .text_color(palette.foreground)
        .text_size(tokens.typography.sm.size)
        .font_weight(tokens.typography.sm.weight)
        .focus_visible(move |style| style.border_color(tokens.colors.ring))
        .accessibility_label(label.clone())
        .child(palette.state_layer(radius, stroke))
        .child(
            div()
                .flex()
                .items_center()
                .gap(tokens.spacing.xs + tokens.spacing.xxs)
                .child(icon)
                .child(label),
        )
}

/// 创建顶栏 / 状态栏等 chrome 区域的紧凑图标按钮。
///
/// 默认图标为二级文字色，悬停时淡入 `navHover` 底、图标变为正文色，按下淡入 `navSelected`。
///
/// ⚠️ **图标档位契约：一律传 `extended.icon.md`**。方形内边距是按它算的
/// （`px = (高度 - icon.md) / 2`，与 [`crate::toolbar_dropdown_button`] 同公式），
/// 传更小 / 更大的档位会让按钮不再是正方形，且图标与相邻 chrome 按钮差一档。
/// 需要更小图标时（如标签页关闭 ✕）请连同「按钮不再方形」一起接受，并在调用点写明原因。
pub fn toolbar_action_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    destructive: bool,
    disabled: bool,
    cx: &App,
) -> Button {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended().colors;
    let opacity = theme.extended().opacity;
    let foreground = if destructive {
        tokens.colors.destructive
    } else {
        tokens.colors.muted_foreground
    };
    let hover_foreground = if destructive {
        tokens.colors.destructive
    } else {
        // 悬停转强调色文字：图标 / 文字都带上主题色（对比度由 accent_text 守卫）。
        extended.accent_text
    };

    let radius = theme.components().button_radius;
    // 高度定死、宽度自适应：纯图标时左右 padding 使其恰为方形（与
    // `toolbar_dropdown_button` 同公式），传入图标+文字组合时自动加宽，
    // 内容不再溢出按钮命中区。
    let height = theme.density().toolbar_button;
    let label: SharedString = label.into();
    let button = Button::new(id)
        .h(height)
        .px((height - theme.extended().icon.md) / 2.)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(radius)
        .text_color(foreground)
        // 焦点与悬停必须是**两种**视觉：悬停是强调色薄底（`StateLayer`），焦点是主题色的
        // 一圈描边。两者都用 `hover_tint` 的话，键盘用户看不出自己停在哪，鼠标用户也会
        // 误以为指针还悬在上面。留一圈透明描边占位，聚焦时只换颜色，不引起 1px 位移。
        .border_1()
        .border_color(gpui::transparent_black())
        .disabled(disabled)
        .accessibility_label(label.clone());
    if disabled {
        return with_icon_tooltip(
            button.child(icon).opacity(opacity.muted).cursor_default(),
            label,
        );
    }
    with_icon_tooltip(
        button
            .cursor_pointer()
            .hover(move |style| style.text_color(hover_foreground))
            .focus_visible(move |style| style.border_color(tokens.colors.ring))
            .child(
                StateLayer::new(radius)
                    .hover(extended.hover_tint)
                    .press(extended.hover_tint_press),
            )
            .child(icon),
        label,
    )
}

/// 创建活动栏按钮；选中底色由调用方绘制（活动栏以一块随选中项滑动的底块表达）。
///
/// `role` 由调用方定：活动栏上半是**路由**（`Role::Tab`，配容器的 `TabList`），
/// 下半是**命令**（`Role::Button`，配容器的 `Toolbar`）。两者共用同一套外观，
/// 但语义不同 —— 硬编一个角色会让其中一半对读屏报错身份。
pub fn activity_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    role: Role,
    selected: bool,
    size: Pixels,
    cx: &App,
) -> Button {
    let theme = active_theme(cx);
    let colors = theme.tokens().colors;
    let extended = theme.extended().colors;
    let (foreground, hover_foreground) = if selected {
        (extended.nav_selected_icon, extended.nav_selected_icon)
    } else {
        // 未选中项悬停转强调色图标。
        (colors.muted_foreground, extended.accent_text)
    };
    let radius = theme.components().nav_item_radius;
    // 选中底是调用方的滑块，滑到之前这里不能露空：选中项的悬停 / 按下层改用选中色。
    let layer = if selected {
        StateLayer::new(radius)
            .hover(extended.nav_selected)
            .press(extended.nav_selected)
    } else {
        StateLayer::new(radius)
            .hover(extended.hover_tint)
            .press(extended.hover_tint_press)
    };

    Button::new(id)
        .size(size)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .rounded(radius)
        .text_color(foreground)
        .hover(move |style| style.text_color(hover_foreground))
        // 焦点描边，不是悬停薄底 —— 活动栏的悬停底由 `StateLayer` 画，两者分开才不会
        // 「Tab 到哪儿」和「指针在哪儿」看起来是同一件事。
        .border_1()
        .border_color(gpui::transparent_black())
        .focus_visible(move |style| style.border_color(colors.ring))
        // `selected` 只改图标色；辅助技术要靠 `aria_selected` 才知道当前在哪一项。
        .role(role)
        .aria_selected(selected)
        // `Button::accessibility_label()` 内部就是 `.aria_label()`（`gpui-base/src/button.rs:232`），
        // 所以这个名字既喂给读屏，也能被 `test_support` 的 `label()` 断言到。
        .accessibility_label(label)
        .child(layer)
        .child(icon)
}

/// 创建带图标与尾部信息的侧栏导航按钮（单行，选中底在行内交叉淡入）。
///
/// 选中态是中性底色 + `navSelectedForeground` 文字 + 中等字重，未选中为二级文字色；
/// 图标颜色由调用方决定，常规导航项用 [`nav_icon_color`]。整列导航请用 [`nav_list`]：
/// 那里的选中底是一块随选中项滑动的底块，与 shell 活动栏一致。
pub fn sidebar_navigation_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    trailing: impl IntoElement,
    selected: bool,
    cx: &App,
) -> Button {
    navigation_row(
        id,
        label,
        icon,
        trailing,
        selected,
        SelectionPaint::Inside,
        cx,
    )
}

/// 导航行的选中底由谁画。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionPaint {
    /// 行内自绘：选中底在行内交叉淡入。
    Inside,
    /// 外层滑动块：行不画选中底，交给 [`crate::nav_list`] 的高亮块滑动。
    Slider,
}

/// 导航行的共同实现；[`sidebar_navigation_button`] 与 [`crate::nav_list`] 共用。
///
/// 行高固定 `density.navRow`：[`crate::nav_list`] 的滑动块按下标 ×（行高 + 行距）定位，
/// 改这里的高度必须同步改那边。
fn navigation_row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: impl IntoElement,
    trailing: impl IntoElement,
    selected: bool,
    paint: SelectionPaint,
    cx: &App,
) -> Button {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended().colors;
    let label = label.into();
    let (foreground, weight) = if selected {
        // 选中：文字与图标（继承）都是强调色。
        (extended.accent_text, FontWeight::MEDIUM)
    } else {
        (tokens.colors.muted_foreground, FontWeight::NORMAL)
    };
    // 悬停：文字与图标（继承）一起转强调色；选中项保持强调色不变。
    let hover_foreground = extended.accent_text;
    let ring = tokens.colors.ring;
    let radius = theme.components().nav_item_radius;

    Button::new(id)
        .h(theme.density().nav_row)
        .w_full()
        .px(tokens.spacing.sm)
        // 行高须容纳回退字体的 ascent+descent，否则配合 `truncate` 会裁掉下伸笔画。
        .line_height(tokens.typography.sm.line_height)
        .flex()
        .items_center()
        .justify_between()
        .gap(tokens.spacing.xs)
        .cursor_pointer()
        .rounded(radius)
        .text_color(foreground)
        .text_size(tokens.typography.sm.size)
        .font_weight(weight)
        .hover(move |style| style.text_color(hover_foreground))
        // 焦点用**描边**而不是底色：悬停底是强调色薄层、选中底是不透明底，焦点再画底就和
        // 「指针在哪儿」长得一样了（与 [`activity_button`] 同款处理）。`border_1` 先占位，
        // 描边出现时不会引起位移。
        .border_1()
        .border_color(gpui::transparent_black())
        .focus_visible(move |style| style.border_color(ring))
        // `selected` 只改视觉；辅助技术要靠 `aria_selected` 才知道哪一项是当前项。
        // 角色取 `Tab`：侧栏 / 活动栏 / 设置分类都是一组「选一个」的导航项。
        .role(Role::Tab)
        .aria_selected(selected)
        .selected(selected)
        .accessibility_label(label.clone())
        .child(nav_state_layer(
            radius,
            selected,
            paint,
            theme.extended().colors,
        ))
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap(tokens.spacing.sm)
                .child(icon)
                .child(div().truncate().child(label)),
        )
        .child(trailing)
}

/// 导航项（侧栏、活动栏、设置分类）的图标色：选中取 `navSelectedIcon`（强调色），
/// 未选中取二级文字色。
pub fn nav_icon_color(selected: bool, cx: &App) -> Hsla {
    let theme = active_theme(cx);
    if selected {
        theme.extended().colors.nav_selected_icon
    } else {
        theme.tokens().colors.muted_foreground
    }
}

/// 侧栏导航项的状态层：选中底交叉淡入；选中底交给外层滑动块时不画。
fn nav_state_layer(
    radius: Pixels,
    selected: bool,
    paint: SelectionPaint,
    extended: duck_theme::ExtendedColors,
) -> StateLayer {
    let hover = if selected {
        extended.nav_selected
    } else {
        extended.hover_tint
    };
    let layer = StateLayer::new(radius)
        .hover(hover)
        .press(extended.hover_tint_press);
    match paint {
        SelectionPaint::Inside => layer.selected(extended.nav_selected, selected),
        // 滑动块滑到之前，选中项自己的悬停 / 按下层已用选中色，不会露空。
        SelectionPaint::Slider => layer,
    }
}

/// 复选框的三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Unchecked,
    Checked,
    /// 部分选中（表头全选在只选中部分行时）。
    Indeterminate,
}

/// 列表用复选框外观（纯视觉，点击由调用方挂在外层）。
pub fn check_mark(state: CheckState, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let colors = tokens.colors;
    let glyph = match state {
        CheckState::Unchecked => None,
        CheckState::Checked => Some(DuckIcon::CheckboxCheck),
        CheckState::Indeterminate => Some(DuckIcon::CheckboxMinus),
    };
    let base = div()
        .size(theme.density().check_mark)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(theme.components().checkbox_radius)
        .border(theme.extended().stroke.thin);
    match glyph {
        None => base
            .border_color(
                colors
                    .muted_foreground
                    .opacity(theme.extended().opacity.border),
            )
            .bg(colors.surface),
        Some(glyph) => base.border_color(colors.primary).bg(colors.primary).child(
            gpui_component::Icon::new(glyph)
                .size(theme.extended().icon.sm)
                .text_color(colors.primary_foreground),
        ),
    }
}

/// BETA 角标的水平内边距 / 圆角（几何量，与字号无关，故留在本模块）。
const BETA_BADGE_PADDING_X: f32 = 4.;
const BETA_BADGE_RADIUS: f32 = 3.;

/// 「BETA」角标：工具箱路由标签与设置页工具列表共用同一外观。
///
/// 源串是 ASCII，无需进 i18n 表；字号走 `extended.badge`（macOS Caption 1 档），
/// 因此会跟随「设置 → 外观 → 字号」一起缩放——此前是模块内的 `px(9.)` 常量，
/// 用户调大字号后角标不动，是「文字不跟主题走」的残留点之一。
#[must_use]
pub fn beta_badge(cx: &App) -> Div {
    let theme = active_theme(cx);
    let colors = theme.tokens().colors;
    let badge = theme.extended().badge;
    div()
        .flex_none()
        .px(px(BETA_BADGE_PADDING_X))
        .rounded(px(BETA_BADGE_RADIUS))
        .bg(colors.muted)
        .text_size(badge.size)
        .line_height(badge.line_height)
        .font_weight(FontWeight::MEDIUM)
        .text_color(colors.muted_foreground)
        .child("BETA")
}

/// 基础卡片：内容底色 + hairline 描边 + `components.card.radius`，不加阴影（阴影只留给浮层）。
pub fn card(cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .bg(tokens.colors.surface)
        .text_color(tokens.colors.surface_foreground)
        .border(theme.extended().stroke.thin)
        .border_color(theme.extended().colors.hairline)
        .rounded(theme.components().card_radius)
}

#[derive(Clone, Copy)]
struct ButtonPalette {
    background: Hsla,
    foreground: Hsla,
    border: Hsla,
    hover: Hsla,
    active: Hsla,
}

impl ButtonPalette {
    fn for_variant(
        variant: ButtonVariant,
        colors: duck_theme::ColorTokens,
        extended: duck_theme::ExtendedColors,
    ) -> Self {
        // 幽灵 / 次要按钮：悬停 / 按下走强调色薄层，反馈带主题色。
        let ghost = Self {
            background: transparent(colors.background),
            foreground: colors.foreground,
            border: transparent(colors.border),
            hover: extended.hover_tint,
            active: extended.hover_tint_press,
        };
        match variant {
            ButtonVariant::Primary => Self::filled(colors.primary, colors.primary_foreground),
            // 次要按钮：内容底色 + 描边（outline），悬停同样走强调色薄层。
            ButtonVariant::Secondary => Self {
                background: colors.surface,
                foreground: colors.foreground,
                border: colors.border,
                hover: extended.hover_tint,
                active: extended.hover_tint_press,
            },
            ButtonVariant::Ghost => ghost,
            ButtonVariant::Link => Self {
                foreground: extended.accent_text,
                ..ghost
            },
            ButtonVariant::Destructive => {
                // 底色过一遍 AA 守卫：`destructive` 本身还被当**文字色**用（错误提示、
                // 「失败」状态），所以不能全局压暗，只在这里把它收敛成可读的危险按钮底。
                let surface =
                    duck_theme::button_surface(colors.destructive, colors.destructive_foreground);
                Self::filled(surface, colors.destructive_foreground)
            }
        }
    }

    fn filled(background: Hsla, foreground: Hsla) -> Self {
        Self {
            background,
            foreground,
            border: background,
            hover: shift_toward_contrast(background, 0.08),
            active: shift_toward_contrast(background, 0.13),
        }
    }

    /// 本配色的悬停 / 按下状态层。描边透明时外扩盖住描边，与整块底色同几何；
    /// 有色描边时落在描边以内，悬停只改底色、不吞掉描边。
    fn state_layer(self, radius: Pixels, stroke: Pixels) -> StateLayer {
        let layer = if self.border.a == 0. {
            StateLayer::new(radius).inset(-stroke)
        } else {
            StateLayer::new(inner_radius(radius, stroke))
        };
        layer.hover(self.hover).press(self.active)
    }
}

fn transparent(color: Hsla) -> Hsla {
    Hsla { a: 0., ..color }
}

/// 描边以内的圆角：外圆角减去描边宽，最小为 0。
fn inner_radius(radius: Pixels, stroke: Pixels) -> Pixels {
    if radius > stroke {
        radius - stroke
    } else {
        px(0.)
    }
}

fn shift_toward_contrast(color: Hsla, amount: f32) -> Hsla {
    let delta = if color.l >= 0.5 { -amount } else { amount };
    Hsla {
        l: (color.l + delta).clamp(0., 1.),
        ..color
    }
}
