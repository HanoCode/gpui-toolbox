//! 命令面板的外层 chrome：卡片 + 右上角关闭按钮 + 底部提示条（以及主窗浮层用的遮罩）。
//!
//! 面板的搜索框与列表由 `gpui_component::command::Command` 渲染；本模块只负责「浮层该长
//! 什么样」——用主题 token 组装遮罩与卡片，让面板与其余界面同源。行为（打开 / 关闭 / 确认）
//! 由挂载方（`duck-shell`）注入，保持组件层无业务。
//!
//! **两个入口，一张卡片**：主窗内的浮层用 [`palette_frame`]（遮罩 + 居中卡片）；
//! 独立弹窗用 [`palette_card`]（只要卡片，窗口本身就是它）。拆开是因为弹窗里画遮罩
//! 没有意义——遮罩只会盖住面板自己那一块；而卡片必须只有一份实现，否则两处迟早长歪。
//!
//! 圆角说明：gpui-fast 的溢出裁剪不贴圆角，`Command` 内部自铺的方形底色会盖掉外层卡片的
//! 圆角——所以卡片本身保持圆角边框，同时由挂载方给 `Command` 自身（Styled refinement）
//! 加同样的圆角与统一底色，双保险。

use gpui::{
    AnyElement, App, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Role, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::{Icon, h_flex, v_flex};

use duck_theme::{active_theme, tr, trf};

use crate::DuckIcon;

/// 面板最大宽度。
const PALETTE_MAX_WIDTH: f32 = 560.;
/// 面板距窗口顶部的距离（偏上更贴近命令面板习惯）。
const PALETTE_TOP_INSET: f32 = 88.;
/// 右上角关闭按钮的边长。
const CLOSE_BUTTON_SIZE: f32 = 24.;

/// 把命令面板内容包进**遮罩 + 居中卡片**（主窗内的浮层用）。
///
/// - 点遮罩空白处或右上角关闭按钮调用 `on_dismiss`（关闭面板）；
/// - 卡片自身的左键按下被拦截，不会冒泡成「关闭」。
///
/// 卡片本体在 [`palette_card`]；独立弹窗只用那一半，不用这里的遮罩。
pub fn palette_frame(
    inner: impl IntoElement,
    on_dismiss: impl Fn(&mut Window, &mut App) + Clone + 'static,
    cx: &App,
) -> AnyElement {
    let extended = active_theme(cx).extended();
    let scrim = Hsla {
        h: 0.,
        s: 0.,
        l: 0.,
        a: extended.opacity.scrim,
    };
    let on_scrim = on_dismiss.clone();

    div()
        .absolute()
        .inset_0()
        .flex()
        .justify_center()
        .items_start()
        .bg(scrim)
        .pt(px(PALETTE_TOP_INSET))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            on_scrim(window, cx);
        })
        .child(
            v_flex()
                .w_full()
                .max_w(px(PALETTE_MAX_WIDTH))
                .child(palette_card(inner, on_dismiss, cx)),
        )
        .into_any_element()
}

/// 只画卡片本身：圆角 + 细边框 + `surface` 底色 + 右上角关闭按钮；**不含遮罩**。
///
/// 两种挂载方式共用同一张卡片，面板才不会长出一个以上的样子：
/// ① [`palette_frame`] —— 主窗内的浮层，这里返回的卡片被它居中铺在遮罩上；
/// ② 独立弹窗（`duck-shell` 的 `palette_window`）—— 窗口本身就是那张卡片，
///    再画一层遮罩只会把面板自己那一块抹黑。
///
/// 宽度由挂载方给：浮层靠外层 `max_w` 约束，弹窗靠窗口尺寸约束。
pub fn palette_card(
    inner: impl IntoElement,
    on_dismiss: impl Fn(&mut Window, &mut App) + Clone + 'static,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let colors = tokens.colors;
    let extended = theme.extended();
    let radius = theme.components().dialog_radius;

    // 关闭按钮：叠在搜索行右端。悬停走强调色薄层，图标转主题色。
    //
    // 无障碍：它只有图标，读屏念不出「叉」，所以必须按 `Button` 报出去并给一个可读名。
    // 键盘也要能到（`tab_stop`）；到了之后 Enter / Space 关面板，**并且接管 Esc** ——
    // 焦点在这个按钮上时，Esc 不会再落到面板自己的处理器上，不接管的话「Esc 关面板」这
    // 个本来好用的动作会在这一种焦点态下静默失灵。
    let on_click = on_dismiss.clone();
    let on_key = on_dismiss.clone();
    let close_button = div()
        .id("palette-close")
        .absolute()
        // 贴着搜索行的内边距落位，属该行的几何对齐（`CLOSE_BUTTON_SIZE` 也是常量），
        // 不属 spacing 阶梯 —— 保留裸值并在此说明。
        .top(px(7.))
        .right(px(7.))
        .size(px(CLOSE_BUTTON_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(tokens.radius.md)
        .text_color(extended.colors.text_tertiary)
        // 可点元素一律手型光标（gpui-fast 没有 `cursor_point`，用 `cursor_pointer`）。
        .cursor_pointer()
        .border_1()
        .border_color(gpui::transparent_black())
        .role(Role::Button)
        .aria_label(tr(cx, "关闭"))
        .aria_keyshortcuts("Escape")
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.border_color(colors.ring))
        .hover(move |style| {
            style
                .bg(extended.colors.hover_tint)
                .text_color(extended.colors.accent_text)
        })
        .on_click(move |_, window, cx| on_click(window, cx))
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "escape" | "enter" | "space") {
                // 先拦住：外层也在听 Esc，处理两遍会让面板关两次。
                cx.stop_propagation();
                on_key(window, cx);
            }
        })
        .child(Icon::new(DuckIcon::X).size(extended.icon.sm));

    v_flex()
        .relative()
        // 高度交父容器定：弹窗挂载时父高是窗口的确定值，`h_full` 让卡片**撑满整窗**——
        // 否则卡片只长到内容高，窗口底部露出一条底色，看起来像卡片浮在另一层底上。
        // 浮层挂载时父高是自动的，百分比回落为 auto，等于没写。
        .h_full()
        .w_full()
        .rounded(radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(colors.surface)
        .overflow_hidden()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(inner)
        .child(close_button)
        .into_any_element()
}

/// 面板底部提示条：键位提示 + 当前匹配数。挂在 `Command::footer` 上。
pub fn palette_footer(matched: usize, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let hairline = extended.colors.hairline;
    let tertiary = extended.colors.text_tertiary;
    let text_size = tokens.typography.xs.size;
    let chip_bg = tokens.colors.background;
    // 键帽与提示条的尺寸一律走 token（裸 px 不随「界面缩放」走）。括号里是原裸值：
    // 16 / 12 / 4 是同值搬迁；3、4、7 原本不在 spacing(2·4·8·12·16·24·32) /
    // radius(3·6·8·12) 阶梯上，收敛到最近档。
    let chip_h = tokens.spacing.lg; // 16
    let chip_pad_x = tokens.spacing.xxs; // 原 3 → 2
    let chip_radius = tokens.radius.sm; // 原 4 → 3
    let row_pad_x = tokens.spacing.md; // 12
    let row_pad_y = tokens.spacing.sm; // 原 7 → 8
    let row_gap = tokens.spacing.xs; // 4
    let sep_margin_x = tokens.spacing.xxs; // 原 3 → 2

    // 键帽：小圆角框，弱于正文的示意元素。
    let chip = move |label: &'static str| -> AnyElement {
        div()
            .flex()
            .h(chip_h)
            .min_w(chip_h)
            .items_center()
            .justify_center()
            .px(chip_pad_x)
            .rounded(chip_radius)
            .bg(chip_bg)
            .border_1()
            .border_color(hairline)
            .text_size(text_size)
            .text_color(tertiary)
            .child(label)
            .into_any_element()
    };
    let label = move |text: &'static str| -> AnyElement { div().child(text).into_any_element() };
    let sep = div()
        .mx(sep_margin_x)
        .text_color(hairline)
        .child("·")
        .into_any_element();

    let count = trf(cx, "{} 项匹配", &[&matched.to_string()]);

    h_flex()
        .w_full()
        .flex_none()
        .items_center()
        .justify_between()
        .px(row_pad_x)
        .py(row_pad_y)
        .border_t_1()
        .border_color(hairline)
        .text_size(text_size)
        .text_color(tertiary)
        .child(
            h_flex()
                .items_center()
                .gap(row_gap)
                .child(chip("↑"))
                .child(chip("↓"))
                .child(label(tr(cx, "选择")))
                .child(sep)
                .child(chip("↵"))
                .child(label(tr(cx, "打开")))
                .child(div().mx(sep_margin_x).text_color(hairline).child("·"))
                .child(chip("Esc"))
                .child(label(tr(cx, "关闭"))),
        )
        .child(div().child(count).into_any_element())
        .into_any_element()
}
