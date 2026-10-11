//! 反馈类内容件：空状态 / 骨架屏 / 加载指示 / 进度条 / 提示条。

use duck_theme::active_theme;
use gpui::{
    AnyElement, App, Div, ElementId, FontWeight, ParentElement as _, Pixels, SharedString, Styled,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon, Sizable as _,
    alert::{Alert, AlertVariant},
    empty::{
        Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant,
        EmptyTitle,
    },
    progress::Progress,
    skeleton::Skeleton,
    spinner::Spinner,
};

use crate::{DuckIcon, notify::Tone};

/// 空状态：图标 + 标题 + 可选说明 + 可选操作。
///
/// 上游 [`Empty`] 要嵌 `Empty` → `EmptyHeader` → `EmptyMedia` / `EmptyTitle` /
/// `EmptyDescription` 四层 builder，这里收成一次调用；标题 / 说明字号显式取主题 token
/// （上游 `text_sm` 是 rem 类名，本项目 rem = 13px，会落成 11.375px）。
///
/// 默认带虚线描边并吃满剩余高度（列表 / 整页空态）。嵌在卡片或面板里时用
/// `.border_0().flex_none()` 收掉 —— [`Empty`] 实现了 `Styled`，调用点可直接改。
pub fn empty_state(
    icon: DuckIcon,
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    action: Option<AnyElement>,
    cx: &App,
) -> Empty {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let body = tokens.typography.sm;
    let meta = tokens.typography.xs;

    let header = EmptyHeader::new()
        .media(
            EmptyMedia::new()
                .with_variant(EmptyMediaVariant::Icon)
                .child(
                    Icon::new(icon)
                        .size(extended.icon.lg)
                        .text_color(extended.colors.text_tertiary),
                ),
        )
        .title(
            EmptyTitle::new()
                .text_size(body.size)
                .line_height(body.line_height)
                .font_weight(FontWeight::MEDIUM)
                .child(title.into()),
        )
        .when_some(description, |this, description| {
            this.description(
                EmptyDescription::new()
                    .text_size(meta.size)
                    .line_height(meta.line_height)
                    .child(description),
            )
        });

    let mut state = Empty::new().header(header);
    if let Some(action) = action {
        state = state.content(EmptyContent::new().child(action));
    }
    state
}

/// 骨架块：`width` 为 `None` 时铺满可用宽度。
pub fn skeleton(height: Pixels, width: Option<Pixels>, cx: &App) -> Skeleton {
    let radius = active_theme(cx).tokens().radius.sm;
    let block = Skeleton::new().h(height).rounded(radius);
    match width {
        Some(width) => block.w(width),
        None => block,
    }
}

/// 圆形骨架：头像 / 图标位。
pub fn skeleton_circle(size: Pixels) -> Skeleton {
    Skeleton::new().size(size).rounded_full()
}

/// 列表骨架：`rows` 行「圆形图标位 + 两行文字」，用于列表 / 表格的加载态。
pub fn skeleton_rows(rows: usize, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let row_height = theme.density().task_row;
    let control = theme.density().control;
    // 闭包只捕获 Copy 值：`cx` 是借用，捕进 `map` 闭包会让借用活得比 `cx` 长。
    let radius = tokens.radius.sm;
    let (gap_xs, gap_sm) = (tokens.spacing.xs, tokens.spacing.sm);
    let (line_height, meta_height) = (tokens.typography.sm.size, tokens.typography.xs.size);
    let (line_width, meta_width) = (px(220.), px(140.));

    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(gap_xs)
        .children((0..rows).map(move |_| {
            let line =
                |width: Pixels, height: Pixels| Skeleton::new().w(width).h(height).rounded(radius);
            div()
                .flex()
                .items_center()
                .w_full()
                .h(row_height)
                .gap(gap_sm)
                .child(skeleton_circle(control))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(gap_xs)
                        .child(line(line_width, line_height))
                        .child(line(meta_width, meta_height)),
                )
        }))
}

/// 加载指示：旋转图标 + 可选文案，水平居中。
pub fn loading(text: Option<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .flex()
        .items_center()
        .gap(tokens.spacing.sm)
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(
            Spinner::new()
                .with_size(extended.icon.lg)
                .color(tokens.colors.muted_foreground),
        )
        .children(text)
}

/// 加载块：把 [`loading`] 放在一块吃满剩余空间、内容居中的区域里。
pub fn loading_block(text: Option<SharedString>, cx: &App) -> Div {
    div()
        .flex()
        .flex_1()
        .size_full()
        .min_h_0()
        .items_center()
        .justify_center()
        .child(loading(text, cx))
}

/// 进度条：`value` 取 0..=1；`loading` 为真时忽略 `value` 走不确定态动画。
pub fn progress_bar(id: impl Into<ElementId>, value: f32, cx: &App) -> Progress {
    let theme = active_theme(cx);
    let components = theme.components();
    Progress::new(id)
        .value(value.clamp(0., 1.))
        .h(components.progress_height)
        .rounded(components.progress_radius)
}

/// 提示条：语义色 + 图标 + 文案。`tone` 与 toast 共用 [`Tone`]，两处语义一致。
///
/// 上游 [`Alert`] 的配色走 kit 的 `info` / `success` / `warning` / `danger` 语义槽，
/// 本项目已把这些槽投射到自有 token，所以这里只挑变体、不重设颜色。
pub fn alert_block(
    id: impl Into<ElementId>,
    tone: Tone,
    message: impl Into<SharedString>,
    cx: &App,
) -> Alert {
    let variant = match tone {
        Tone::Info => AlertVariant::Info,
        Tone::Success => AlertVariant::Success,
        Tone::Warning => AlertVariant::Warning,
        Tone::Error => AlertVariant::Error,
    };
    Alert::new(id, message.into())
        .with_variant(variant)
        .icon(Icon::new(tone.icon_name()).text_color(tone.color(cx)))
        .text_size(active_theme(cx).tokens().typography.sm.size)
}
