//! 侧栏导航行被挤窄时「图标 + 名称」不许消失（无头布局回归）。
//!
//! # 实机现象（2026-10-16 截图）
//!
//! 数据库左栏「索引」分类展开后，每行的 ListOrdered 图标画在文字开头几个字上，
//! 而且那串文字全是同一张表名 —— 索引名**整条消失**了。
//!
//! # 根因（照抄机制，别自己重新推）
//!
//! [`sidebar_navigation_button`] 的行尾说明文字（索引 = 挂在哪张表、用户 = host）
//! 若**裸着**放（`meta_text` 原样进 Button），它是行里唯一没有伸缩约束的元素：
//! flex 的自动最小尺寸对「overflow 可见」的项等于 min-content ⇒ **挤不动**。
//! 于是行宽不够时，前面「图标 + 名称」那组（`min_w_0`，能缩到 0）被整组挤塌：
//! 名称宽 0（`truncate` 自己裁自己 ⇒ 不可见），而 `flex_shrink_0` 的图标照样
//! 按 14px 从 0 宽的框里**溢出来画**，正好压在行尾说明文字的开头 —— 就是截图
//! 里「图标压在文字上」的形状。
//!
//! 修法 = 调用方（`browse.rs::sidebar_database_object_row`）给说明文字套
//! `min_w_0().truncate()`：它变成可伸缩 + 超宽省略号，挤压由它和名称**按比例**
//! 分担，图标永远有位置。两条测试把这对形态钉死。

use duck_components::{DuckIcon, meta_text, sidebar_navigation_button};
use gpui::{
    AnyElement, Context, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext,
    VisualTestContext, Window, div, px, size,
};

struct Host {
    width: f32,
    /// `true` = 实机原来的形态（裸 `meta_text` 当尾巴，挤塌）；`false` = 修过的
    /// 形态（`min_w_0().truncate()`）。
    broken: bool,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = duck_theme::active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let muted = tokens.colors.muted_foreground;
        // 与 `browse.rs::sidebar_database_object_row` 同一串实参：icon.md + 长名 +
        // 一行说明文字（索引的说明 = 表名，往往和名字一样长）。
        let trailing: AnyElement = if self.broken {
            meta_text("apv_reset_20260802", cx).into_any_element()
        } else {
            meta_text("apv_reset_20260802", cx)
                .min_w_0()
                .truncate()
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .w(px(self.width))
            .bg(tokens.colors.background)
            .child(
                div().child(sidebar_navigation_button(
                    "btn",
                    "idx_ak_apv_reset_20260802_bidirection",
                    gpui_component::Icon::new(DuckIcon::ListOrdered)
                        .size(extended.icon.md)
                        .text_color(muted),
                    trailing,
                    false,
                    cx,
                )),
            )
    }
}

/// 每个用例独占一个窗口：`navigation_row` 内部的 `debug_selector` 是行内共享的，
/// 同窗多行会互相覆盖（`debug_bounds` 只还最后画的那份）。
fn row_window(cx: &mut TestAppContext, width: f32, broken: bool) -> VisualTestContext {
    cx.update(|cx| {
        gpui_component::init(cx);
        duck_theme::init(cx);
        cx.set_reduce_motion(true);
    });
    let (_, ctx) = cx.add_window_view(move |_, _| Host { width, broken });
    let mut cx = ctx.clone();
    cx.simulate_resize(size(px(400.), px(300.)));
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx
}

/// 行窄到放不下整串说明文字时，图标 + 名称必须仍在（名称给省略号，不消失）。
#[gpui::test]
fn a_squeezed_row_keeps_its_icon_and_name_visible(cx: &mut TestAppContext) {
    let mut cx = row_window(cx, 150., false);
    let lead = cx.debug_bounds("nav-row-lead").expect("lead 组有几何");
    let label = cx.debug_bounds("nav-row-label").expect("文案有几何");
    // 图标（14px，`flex_shrink_0`）+ 间距（8px）之后才是文案；这条偏移变了，
    // 说明行内部结构被动过。
    assert_eq!(
        label.origin.x - lead.origin.x,
        px(22.),
        "文案应紧跟在图标 + 间距之后（14 + 8）"
    );
    assert!(
        lead.size.width > px(22.),
        "lead 组被挤塌成 {:?}：图标没有布局空间，会从 0 宽的框里溢出来压在文字上",
        lead.size.width
    );
    assert!(
        label.size.width > px(20.),
        "名称被挤到只剩 {:?}（等于不可见）：说明文字没有分担挤压",
        label.size.width
    );
}

/// 负向对照：**这条记录的是坑本身**，不是期望行为。
///
/// 裸 `meta_text` 当尾巴 ⇒ 说明文字挤不动 ⇒ lead 组缩到 0 宽 ⇒ 名称消失、
/// 图标溢出压字。谁要是把 `browse.rs` 里的 `min_w_0().truncate()` 又简化掉，
/// 这里不会红 —— 但上面那条用户场景会红。
#[gpui::test]
fn a_bare_meta_text_trailing_collapses_the_row(cx: &mut TestAppContext) {
    let mut cx = row_window(cx, 150., true);
    let lead = cx.debug_bounds("nav-row-lead").expect("lead 组有几何");
    assert_eq!(
        lead.size.width,
        px(0.),
        "裸尾巴下 lead 组应被挤到 0 宽（这正是图标压字的机制）"
    );
}
