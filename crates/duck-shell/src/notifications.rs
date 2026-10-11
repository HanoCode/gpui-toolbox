//! 顶栏通知中心：铃铛入口（未读角标）+ 历史面板。
//!
//! 历史由 [`duck_components::notify`] 在投递时统一记录（App 级内存，环形上限 100 条，
//! 重启清空）；本模块只负责展示：铃铛挂在主窗口顶栏尾随插槽之后、右侧栏开关之前，
//! 点击弹出历史面板并清零未读。toast 约 5 秒自动消失，错过的结论在这里回看。

use std::time::SystemTime;

use duck_components::{
    ButtonVariant, DuckIcon, body_text, button, caption_heading, meta_text, notify,
    toolbar_action_button,
};
use duck_theme::{active_theme, tr, trf};
use gpui::{
    Anchor, AnyElement, App, ElementId, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::popover::Popover;
use gpui_component::{Icon, h_flex, tooltip::Tooltip, v_flex};

/// 面板宽度：够放两行正文与时间戳，不至于盖住半个窗口。
const PANEL_WIDTH: f32 = 320.;
/// 列表最大高度：超出滚动。
const PANEL_MAX_HEIGHT: f32 = 420.;
/// 空态的固定高度（撑一点，免得面板瘪成一条标题）。
const EMPTY_HEIGHT: f32 = 120.;
/// 未读角标数字上限：超过显示 `99+`。
const BADGE_COUNT_CAP: usize = 99;

/// 顶栏通知中心铃铛：未读时带数字角标，点击弹出历史面板（打开即清零未读）。
pub fn bell(cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let label = SharedString::from(tr(cx, "通知"));
    let tooltip_label = label.clone();
    let unread = notify::unread_count(cx);

    // 角标叠在铃铛右上角：负向偏移压出图标边缘，数字超限显示 99+。
    let badge = (unread > 0).then(|| {
        let tokens = theme.tokens();
        let text = if unread > BADGE_COUNT_CAP {
            "99+".to_owned()
        } else {
            unread.to_string()
        };
        div()
            .absolute()
            .top(px(-5.))
            .right(px(-7.))
            .min_w(px(14.))
            .h(px(14.))
            .px(px(3.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(duck_theme::button_surface(
                tokens.colors.destructive,
                tokens.colors.destructive_foreground,
            ))
            .text_color(tokens.colors.destructive_foreground)
            .text_size(theme.extended().badge.size)
            .line_height(theme.extended().badge.line_height)
            .font_weight(FontWeight::MEDIUM)
            .child(text)
    });

    let trigger = toolbar_action_button(
        "shell-notification-bell",
        label,
        div()
            .relative()
            .flex()
            .child(Icon::new(DuckIcon::Bell).size(theme.extended().icon.md))
            .children(badge),
        false,
        false,
        cx,
    );

    div()
        .id("shell-notification-bell-tooltip")
        .flex_none()
        // 顶栏整条是拖动区：不拦下鼠标按下，点一下会变成拖窗口。
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
        .child(
            Popover::new("shell-notification-center")
                // 菜单锚在按钮右下角：顶栏贴窗口上缘，向上弹会出界（同语言切换）。
                .anchor(Anchor::BottomRight)
                .offset(px(6.))
                .trigger(trigger)
                .on_open_change(|open, _, cx| {
                    if *open {
                        notify::mark_history_read(cx);
                    }
                })
                .child(history_panel(cx)),
        )
        .into_any_element()
}

/// 历史面板：标题行（通知 + 清空）+ hairline 分隔 + 通知卡片列表（新→旧，超出滚动）。
///
/// 每条通知渲染成一张圆角软底卡片：左侧是语义色薄底圆片承托的图标（取
/// `opacity.tint` 档，token 注释里「图标底色」的本职），时间戳右上角对齐，
/// 正文占满剩余宽度——比裸行列表更有层次，也与设置页的分组卡片同一观感。
fn history_panel(cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let spacing = tokens.spacing;
    let entries = notify::history(cx);
    let empty = entries.is_empty();
    // 图标圆片边长：`icon.lg` 居中，四周各留半个 `spacing.md` 的呼吸空间。
    let chip_size = theme.extended().icon.lg + spacing.md;

    let header = h_flex()
        .flex_none()
        .items_center()
        .justify_between()
        .pb(spacing.sm)
        .child(caption_heading(tr(cx, "通知"), cx))
        .when(!empty, |row| {
            row.child(
                button(
                    "shell-notification-clear",
                    tr(cx, "清空"),
                    ButtonVariant::Link,
                    cx,
                )
                .on_click(|_, _, cx| notify::clear_history(cx)),
            )
        });

    let body = if empty {
        v_flex()
            .flex_none()
            .w(px(PANEL_WIDTH))
            .h(px(EMPTY_HEIGHT))
            .items_center()
            .justify_center()
            .gap(spacing.sm)
            .child(
                // 空态：中性色圆片承托铃铛，比一行孤零零的灰字更有落位感。
                div()
                    .size(chip_size)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(tokens.colors.muted)
                    .child(
                        Icon::new(DuckIcon::Bell)
                            .size(theme.extended().icon.lg)
                            .text_color(tokens.colors.muted_foreground),
                    ),
            )
            .child(meta_text(tr(cx, "无通知"), cx))
            .into_any_element()
    } else {
        let typography = &tokens.typography;
        let icon_size = theme.extended().icon.lg;
        let tint = theme.extended().opacity.tint;
        let radius = theme.components().card_radius;
        v_flex()
            .id("shell-notification-history")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .pt(spacing.sm)
            .gap(spacing.xxs)
            .children(entries.into_iter().map(move |entry| {
                let tone = entry.tone();
                let color = tone.color(cx);
                let time = relative_time(entry.at(), cx);
                let entry_id = entry.id();
                h_flex()
                    .items_start()
                    .gap(spacing.sm)
                    .px(spacing.sm)
                    .py(spacing.sm)
                    .rounded(radius)
                    .bg(tokens.colors.muted)
                    .child(
                        // 语义色薄底圆片：色相随级别走，衬底取统一 tint 档。
                        div()
                            .size(chip_size)
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(color.opacity(tint))
                            .child(
                                Icon::new(tone.icon_name())
                                    .size(icon_size)
                                    .text_color(color),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .children(entry.title().clone().map(|title| {
                                div()
                                    .text_size(typography.sm.size)
                                    .line_height(typography.sm.line_height)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title)
                            }))
                            .child(body_text(entry.message().clone(), cx)),
                    )
                    .child(
                        // 尾随列：时间在上、删除在下——正文多行时操作位不掉到卡片中部。
                        v_flex()
                            .flex_none()
                            .items_end()
                            .gap(spacing.xxs)
                            .child(meta_text(time, cx))
                            .child(
                                toolbar_action_button(
                                    ElementId::NamedInteger(
                                        SharedString::from("shell-notification-delete"),
                                        entry_id,
                                    ),
                                    tr(cx, "删除这条通知"),
                                    Icon::new(DuckIcon::X).size(theme.extended().icon.md),
                                    false,
                                    false,
                                    cx,
                                )
                                // 逐条删除：只摘掉这一条，其余历史原样保留。
                                .on_click(move |_, _, cx| {
                                    notify::remove_from_history(entry_id, cx);
                                }),
                            ),
                    )
            }))
            .into_any_element()
    };

    let divider = div()
        .flex_none()
        .w_full()
        .h(px(1.))
        .bg(theme.extended().colors.hairline);

    v_flex()
        .w(px(PANEL_WIDTH))
        .max_h(px(PANEL_MAX_HEIGHT))
        .child(header)
        .child(divider)
        .child(body)
        .into_any_element()
}

/// 相对时间文案：刚刚 / N 分钟前 / N 小时前 / N 天前（i18n 条目已存在）。
fn relative_time(at: SystemTime, cx: &App) -> SharedString {
    let Ok(elapsed) = at.elapsed() else {
        return SharedString::from(tr(cx, "刚刚"));
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        SharedString::from(tr(cx, "刚刚"))
    } else if secs < 3600 {
        trf(cx, "{} 分钟前", &[&(secs / 60).to_string()]).into()
    } else if secs < 86_400 {
        trf(cx, "{} 小时前", &[&(secs / 3600).to_string()]).into()
    } else {
        trf(cx, "{} 天前", &[&(secs / 86_400).to_string()]).into()
    }
}
