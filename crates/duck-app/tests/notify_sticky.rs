//! 「可替换提示按 key 替换」的无头验证。
//!
//! 背景：gpui-component 给每条通知的默认 id 是随机 uuid（上游注释：default ids
//! never collide）。所以同一个动作被反复触发（用户等不及会连点「检查更新」）就会
//! 在窗口右上角堆成一列红条。`notify::sticky` 用调用方给的 key 作为通知 id，
//! 同 key 再推就是替换（提示本身约 5 秒自动消失，替换语义仍在）。
//!
//! 这里用真实 `Root`（通知宿主）建窗口，用 `WindowExt::notifications` 直接数条数——
//! 不依赖任何渲染细节。

use duck_components::notify::{self, Tone};
use gpui::{
    AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext,
    VisualTestContext, Window, div,
};
use gpui_component::{Root, WindowExt as _, init as component_init, v_flex};

/// 最简内容根：通知挂在窗口的 `Root` 上，与内容树无关。
struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().child(div())
    }
}

/// 建一个挂着 `Root` 的窗口（不套 `Root` 时通知推不出去）。
///
/// `Root` 必须是窗口的**根视图**：通知宿主是挂在它上面的 `WindowState` 插件。
/// 初始化顺序照 `main.rs`：组件库 → 主题（通知按活动主题取色）。
fn notify_window(cx: &mut TestAppContext) -> &mut VisualTestContext {
    cx.update(component_init);
    cx.update(duck_theme::init);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let content = cx.new(|_| Blank);
        Root::new(content, window, cx)
    });
    cx
}

/// 当前窗口里的通知条数。
fn count(cx: &mut VisualTestContext) -> usize {
    cx.update(|window, cx| window.notifications(cx).len())
}

/// 同一个 key 再推一次 = 替换，而不是再堆一条。
#[gpui::test]
fn sticky_replaces_the_previous_notification_with_the_same_key(cx: &mut TestAppContext) {
    let cx = notify_window(cx);

    cx.update(|window, cx| {
        notify::sticky(
            "update.check",
            Tone::Warning,
            "尚未配置更新源。",
            window,
            cx,
        )
    });
    assert_eq!(count(cx), 1);

    cx.update(|window, cx| {
        notify::sticky(
            "update.check",
            Tone::Error,
            "检查更新失败：连接超时",
            window,
            cx,
        )
    });
    assert_eq!(
        count(cx),
        1,
        "同 key 的常驻提示应替换上一条，而不是堆成两条"
    );
}

/// 不同 key 互不干扰：一次检查的结论与一次安装的结论可以同时在。
#[gpui::test]
fn sticky_keeps_notifications_under_different_keys(cx: &mut TestAppContext) {
    let cx = notify_window(cx);

    cx.update(|window, cx| notify::sticky("update.check", Tone::Error, "检查更新失败", window, cx));
    cx.update(|window, cx| notify::sticky("update.install", Tone::Error, "更新失败", window, cx));

    assert_eq!(count(cx), 2);
}

/// 对照组：没有 key 的 `error` 仍然是「一条一条往上堆」——
/// 说明上面的替换确实来自 key，而不是通知系统本来就去重。
#[gpui::test]
fn plain_error_notifications_still_stack(cx: &mut TestAppContext) {
    let cx = notify_window(cx);

    cx.update(|window, cx| notify::error("同一条错误", window, cx));
    cx.update(|window, cx| notify::error("同一条错误", window, cx));

    assert_eq!(count(cx), 2);
}
