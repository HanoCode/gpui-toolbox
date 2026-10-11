//! 通知历史「逐条删除」的无头验证。
//!
//! 面板上的 × 按钮调用 `notify::remove_from_history(id, cx)`。这里钉两件事：
//! 一是**只删这一条**（其余历史按原序保留，不是清空），二是**未读角标的减法**——
//! 删一条早已读过的旧通知不该把角标抹掉，删未读的那条才该减。
//!
//! 条目 id 是投递时分配的单调序号：消息与时间都可能重复（同一条错误连推两次），
//! 序号不会，所以它是删除入口唯一可靠的定位键。

use duck_components::notify::{self, Tone};
use gpui::{
    AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext,
    VisualTestContext, Window, div,
};
use gpui_component::{Root, init as component_init, v_flex};

/// 最简内容根：通知挂在窗口的 `Root` 上，与内容树无关。
struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().child(div())
    }
}

/// 建一个挂着 `Root` 的窗口，并按 `main.rs` 的顺序做好初始化。
fn notify_window(cx: &mut TestAppContext) -> &mut VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
    });
    let (_, cx) = cx.add_window_view(|window, cx| {
        let content = cx.new(|_| Blank);
        Root::new(content, window, cx)
    });
    cx
}

/// 推三条不同级别的通知，返回它们的 id（新→旧）。
fn push_three(cx: &mut VisualTestContext) -> Vec<u64> {
    cx.update(|window, cx| notify::info("第一条", window, cx));
    cx.update(|window, cx| notify::warning("第二条", window, cx));
    cx.update(|window, cx| notify::error("第三条", window, cx));
    cx.update(|_, cx| {
        notify::history(cx)
            .iter()
            .map(notify::NotificationEntry::id)
            .collect()
    })
}

/// 当前历史（新→旧）的正文。
fn messages(cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_, cx| {
        notify::history(cx)
            .iter()
            .map(|entry| entry.message().to_string())
            .collect()
    })
}

/// 历史条数。
fn count(cx: &mut VisualTestContext) -> usize {
    cx.update(|_, cx| notify::history(cx).len())
}

/// 删中间那条：只剩另外两条，且顺序不变。
#[gpui::test]
fn removing_one_entry_keeps_the_rest_in_order(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    let ids = push_three(cx);
    assert_eq!(messages(cx), ["第三条", "第二条", "第一条"]);

    let removed = cx.update(|_, cx| notify::remove_from_history(ids[1], cx));
    assert!(removed, "删得到那条");
    assert_eq!(messages(cx), ["第三条", "第一条"], "只摘掉被删的那条");
    assert_eq!(count(cx), 2);
}

/// 删不存在的 id：什么都不做，也不误删别的条目。
#[gpui::test]
fn removing_an_unknown_id_changes_nothing(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    let ids = push_three(cx);

    let removed = cx.update(|_, cx| notify::remove_from_history(u64::MAX, cx));
    assert!(!removed, "找不到就返回 false");
    assert_eq!(count(cx), 3);
    assert!(ids.iter().all(|id| {
        cx.update(|_, cx| notify::history(cx).iter().any(|entry| entry.id() == *id))
    }));
}

/// 未读角标：删**未读**那条减一位（推三条 → 删一条 → 剩 2）。
#[gpui::test]
fn removing_an_unread_entry_drops_the_unread_badge(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    let ids = push_three(cx);
    assert_eq!(cx.update(|_, cx| notify::unread_count(cx)), 3);

    cx.update(|_, cx| notify::remove_from_history(ids[0], cx));
    assert_eq!(
        cx.update(|_, cx| notify::unread_count(cx)),
        2,
        "删未读的要减"
    );
}

/// 未读角标：删**已读**那条不减 —— 否则清旧记录会把新提示也标成已读。
#[gpui::test]
fn removing_a_read_entry_keeps_the_unread_badge(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    let ids = push_three(cx);
    // 打开面板（一次性清零未读）后再来一条：旧三条已读，新的那条未读。
    cx.update(|_, cx| notify::mark_history_read(cx));
    cx.update(|window, cx| notify::success("第四条", window, cx));

    let read_id = ids[1];
    cx.update(|_, cx| notify::remove_from_history(read_id, cx));

    assert_eq!(
        cx.update(|_, cx| notify::unread_count(cx)),
        1,
        "删已读的不该动角标（那条新提示仍是未读）"
    );
    assert_eq!(count(cx), 3);
}

/// 删到空：历史空了，角标归零，`history()` 给空表（面板落回空态）。
#[gpui::test]
fn removing_every_entry_empties_the_history(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    let ids = push_three(cx);

    for id in ids {
        cx.update(|_, cx| notify::remove_from_history(id, cx));
    }

    assert!(cx.update(|_, cx| notify::history(cx)).is_empty());
    assert_eq!(cx.update(|_, cx| notify::unread_count(cx)), 0);
}

/// 删除后新通知的 id 不复用：曾在列表里的旧 id 不会「复活」指到新条目上。
#[gpui::test]
fn ids_are_not_reused_after_removal(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    let ids = push_three(cx);
    cx.update(|_, cx| notify::remove_from_history(ids[2], cx));

    cx.update(|window, cx| notify::info("新来的", window, cx));
    let fresh = cx.update(|_, cx| notify::history(cx)[0].id());

    assert!(!ids.contains(&fresh), "新条目的 id 不该复用被删掉的号");
    assert_eq!(
        cx.update(|_, cx| notify::history(cx)[0].message().to_string()),
        "新来的"
    );
    assert_eq!(cx.update(|_, cx| notify::history(cx)[0].tone()), Tone::Info);
}
