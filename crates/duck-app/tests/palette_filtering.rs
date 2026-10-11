//! 命令面板「过滤权在宿主手里」这条接线的无头守卫。
//!
//! `duck-shell` 的 `render_palette` 给 `Command` 挂了 `filterable(false)`，把命中与排序
//! 整体交给宿主的打分器（`duck_command::fuzzy`）。这条接线依赖上游两个行为：
//!
//! 1. `filterable(false)` 时**供给的每个条目都留着**，查询串不再在本地过滤；
//! 2. 查询串照旧从 `on_query` 回报给宿主。
//!
//! 两者都是上游文档承诺的，但一旦升级上游，破坏方式是**静默的**：面板会退化成
//! 「输入什么都不再过滤」，症状看起来像我们的打分器坏了。所以在这里钉住。
//!
//! 对照组（默认过滤开着）同时给出——证明「条目全留着」确实来自 `filterable(false)`，
//! 而不是上游本来就不按查询过滤。

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Context, Entity, IntoElement, Render, TestAppContext, VisualTestContext,
    Window,
};
use gpui_component::command::{Command, CommandItem, CommandState};
use gpui_component::init as component_init;

/// 面板宿主的最小复制品：一条 `Command` + 三个条目。接线形状照 `render_palette`。
struct PaletteReplica {
    state: Entity<CommandState>,
    filterable: bool,
    /// 每次 `on_query` 回报的查询串都记在这里。
    seen: Rc<RefCell<Vec<String>>>,
}

impl PaletteReplica {
    fn new(
        filterable: bool,
        seen: Rc<RefCell<Vec<String>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = cx.new(|cx| CommandState::new(window, cx));
        Self {
            state,
            filterable,
            seen,
        }
    }
}

impl Render for PaletteReplica {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let seen = self.seen.clone();
        Command::new(&self.state)
            .filterable(self.filterable)
            .on_query(move |query, _window, _cx| seen.borrow_mut().push(query.to_owned()))
            .items([
                CommandItem::new().label("视频统一解析"),
                CommandItem::new().label("批量重命名"),
                CommandItem::new().label("归档解压"),
            ])
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

/// 换一个查询串并重绘（与面板里用户打字同一条路径）。
fn type_query(view: &Entity<PaletteReplica>, query: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.state
                .clone()
                .update(cx, |state, cx| state.set_query(query, window, cx));
        });
    });
    draw(cx);
}

fn matched_count(view: &Entity<PaletteReplica>, cx: &mut VisualTestContext) -> usize {
    cx.update(|_window, cx| view.read(cx).state.read(cx).matched_count())
}

/// 建面板复制品（`filterable` 由调用方指定，两组用例只差这一个开关）。
fn palette(
    cx: &mut TestAppContext,
    filterable: bool,
) -> (
    Entity<PaletteReplica>,
    Rc<RefCell<Vec<String>>>,
    &mut VisualTestContext,
) {
    cx.update(component_init);
    cx.update(duck_theme::init);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (view, cx) = cx.add_window_view({
        let seen = seen.clone();
        move |window, cx| PaletteReplica::new(filterable, seen, window, cx)
    });
    draw(cx);
    (view, seen, cx)
}

#[gpui::test]
fn filterable_off_keeps_every_item_and_still_reports_the_query(cx: &mut TestAppContext) {
    let (view, seen, cx) = palette(cx, false);

    // 一个谁也匹配不上的串：上游的 `contains` 在这里会滤掉全部三个条目。
    type_query(&view, "zzzz-不存在", cx);

    assert_eq!(
        matched_count(&view, cx),
        3,
        "关闭本地过滤后，供给的条目必须原样留着——命中与否由宿主的打分器决定"
    );
    assert!(
        seen.borrow().iter().any(|query| query == "zzzz-不存在"),
        "查询串必须仍从 on_query 回报给宿主，否则宿主算不出命中；实收 {:?}",
        seen.borrow()
    );
}

/// 对照组：默认（本地过滤开着）时同一个串会把三个条目全滤掉。
///
/// 没有这条，上面那条断言可能只是「上游根本不按查询过滤」的假象。
#[gpui::test]
fn the_default_local_filtering_would_have_hidden_everything(cx: &mut TestAppContext) {
    let (view, _seen, cx) = palette(cx, true);

    type_query(&view, "zzzz-不存在", cx);
    assert_eq!(
        matched_count(&view, cx),
        0,
        "默认过滤下上游会按查询串本地过滤——这正是必须关掉它的原因"
    );
}
