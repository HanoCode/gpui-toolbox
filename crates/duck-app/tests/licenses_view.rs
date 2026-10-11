//! 开源许可窗口的无头回归守卫：1.2 MiB 的内嵌许可全文要能画出来、并且能滚。
//!
//! # 为什么单独守这一条
//!
//! `pages/licenses.rs` 把整份 `THIRD_PARTY_LICENSES.md`（849 个组件 / 409 份全文 /
//! 约 1.2 MiB / 25 553 行）当**一段文本**渲染。同仓日志查看器只到 512 KiB，这是它的
//! 2.4 倍——而这个量级一旦出问题，表现是「打开许可窗口界面卡住」，不是崩溃，
//! 单测不会红、clippy 也不会说话。
//!
//! 所以这里守两件可观测的事：
//! 1. 内容块比视口高（说明真的排进去了，没有被裁成 0 高）；
//! 2. 在正文上派发竖向滚轮后，内容块真的移动了（说明滚动链是通的）。
//!
//! 结构逐字复制 `LicensesView::render`（外层 `flex_1().min_h_0()` 的滚动容器 +
//! 内层可换行的单段文本），只有底色 / 字号这类装饰被省掉。
//!
//! 顺带记录一次实测量级（2026-10-11，本机 debug 构建）：首帧 0.54 ms、次帧 0.23 ms，
//! **自动换行与不换行同量级**——所以真实实现用了换行排版（许可正文横着滚很难读），
//! 不必为了性能退回 `whitespace_nowrap`。

use gpui::{
    Context, InteractiveElement as _, IntoElement, MouseMoveEvent, ParentElement, Render,
    ScrollDelta, ScrollWheelEvent, Styled, TestAppContext, TouchPhase, VisualTestContext, Window,
    div, point, px,
};
use gpui_component::{init as component_init, scroll::ScrollableElement as _, v_flex};

/// 与 `pages/licenses.rs` 内嵌的是同一份文件。
const DOC: &str = include_str!("../../../assets/licenses/THIRD_PARTY_LICENSES.md");

/// 内容块的 debug 选择器（断言滚动位移用）。
const DOC_BODY: &str = "licenses-doc-body";

/// 滚轮事件 + 重绘。gpui-fast 的滚轮处理要求 `hitbox.should_handle_scroll`，
/// 即滚轮位置必须已在 `window.mouse_hit_test` 命中链里——所以先把指针移过去。
fn scroll(cx: &mut VisualTestContext, x: f32, y: f32, dx: f32, dy: f32) {
    cx.simulate_event(MouseMoveEvent {
        position: point(px(x), px(y)),
        ..Default::default()
    });
    cx.simulate_event(ScrollWheelEvent {
        position: point(px(x), px(y)),
        delta: ScrollDelta::Pixels(point(px(dx), px(dy))),
        touch_phase: TouchPhase::Moved,
        ..Default::default()
    });
    draw(cx);
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

/// `LicensesView::render` 的复制品：只有滚动容器 + 一段可换行的长文本。
struct LicensesReplica;

impl Render for LicensesReplica {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().min_w_0().min_h_0().child(
            div()
                .id("licenses-doc")
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scrollbar()
                .child(
                    div()
                        .debug_selector(|| DOC_BODY.to_string())
                        .w_full()
                        .child(DOC),
                ),
        )
    }
}

/// 许可全文排得进布局（比视口高），并且竖直滚轮能推动它。
#[gpui::test]
fn the_licenses_document_lays_out_and_scrolls(cx: &mut TestAppContext) {
    cx.update(component_init);
    cx.update(duck_theme::init);
    let (_, cx) = cx.add_window_view(|_, _| LicensesReplica);
    draw(cx);

    let viewport = cx.update(|window, _| window.viewport_size());
    let content = cx
        .debug_bounds(DOC_BODY)
        .expect("内容块应当排进布局（选择器没命中说明它没被渲染）");
    assert!(
        content.size.height > viewport.height,
        "许可全文应当比视口高：内容 {} px vs 视口 {} px",
        content.size.height,
        viewport.height
    );

    let before = content.origin.y;
    scroll(cx, 400., 400., 0., -120.);
    let after = cx
        .debug_bounds(DOC_BODY)
        .expect("滚动后内容块仍应在布局里")
        .origin
        .y;
    assert!(
        after < before,
        "竖向滚轮应当把内容往上推：滚动前 y={before:?}，滚动后 y={after:?}"
    );
}
