//! 派发回落行为的无头验证：**没有窗口时动作也要执行**。
//!
//! 背景：macOS 关掉所有窗口后应用仍驻留，菜单栏照旧可点。而菜单项是否置灰只看
//! 「该动作有没有注册过全局监听器」——`RunCommand` 注册过，所以菜单项是**可用**的；
//! 如果派发时因为没有活跃窗口就默默跳过，「退出」「打开设置」「打开日志」点下去
//! 会毫无反应，看起来像应用卡死。
//!
//! 所以：无窗口时改派给 App 级处理器（[`install_app_handler`]）；有窗口时窗口级
//! 优先，只登记了 App 级也照跑。

use std::cell::Cell;

use duck_command::{RunCommand, dispatch, install, install_app_handler, install_handler};
use gpui::{App, Context, Global, IntoElement, Render, TestAppContext, Window, div};

/// 执行记录：两个计数器，分别记 App 级与窗口级处理器被调用的次数。
#[derive(Default)]
struct Ran {
    app: Cell<u32>,
    window: Cell<u32>,
}

impl Global for Ran {}

fn bump_app(cx: &mut App) {
    let next = cx.global::<Ran>().app.get() + 1;
    cx.global_mut::<Ran>().app.set(next);
}

fn bump_window(cx: &mut App) {
    let next = cx.global::<Ran>().window.get() + 1;
    cx.global_mut::<Ran>().window.set(next);
}

fn ran(cx: &App) -> (u32, u32) {
    let ran = cx.global::<Ran>();
    (ran.app.get(), ran.window.get())
}

/// 最简窗口内容（本文件只用它造一个真实窗口）。
struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 承重用例：一个窗口都没有时，经菜单派发的动作仍要打到 App 级处理器。
#[gpui::test]
fn app_handler_runs_with_no_window_at_all(cx: &mut TestAppContext) {
    cx.update(|cx| {
        install(cx);
        cx.set_global(Ran::default());
        install_app_handler(cx, "test.app", bump_app);
    });

    cx.update(|cx| {
        cx.dispatch_action(&RunCommand {
            id: "test.app".into(),
        })
    });

    let (app, window) = cx.update(|cx| ran(cx));
    assert_eq!(
        (app, window),
        (1, 0),
        "没有活跃窗口时，App 级处理器必须照常执行（否则菜单栏点了没反应）"
    );
}

/// 有窗口时，窗口级处理器优先——`dispatch` 拿到窗口就该用它。
#[gpui::test]
fn window_handler_wins_when_a_window_is_available(cx: &mut TestAppContext) {
    cx.update(|cx| {
        install(cx);
        cx.set_global(Ran::default());
        install_handler(cx, "test.window", |_, cx| bump_window(cx));
        install_app_handler(cx, "test.window", bump_app);
    });

    let (_, cx) = cx.add_window_view(|_, _| Blank);
    cx.update(|window, cx| dispatch("test.window", window, cx));

    let (app, window) = cx.update(|_, cx| ran(cx));
    assert_eq!((app, window), (0, 1), "两种处理器都在时应走窗口级");
}

/// 只登记了 App 级处理器时，窗口级派发要能回落——活动栏按钮、托盘都拿得到窗口，
/// 但动作本身不需要它。
#[gpui::test]
fn window_dispatch_falls_back_to_the_app_handler(cx: &mut TestAppContext) {
    cx.update(|cx| {
        install(cx);
        cx.set_global(Ran::default());
        install_app_handler(cx, "test.app", bump_app);
    });

    let (_, cx) = cx.add_window_view(|_, _| Blank);
    cx.update(|window, cx| dispatch("test.app", window, cx));

    let (app, window) = cx.update(|_, cx| ran(cx));
    assert_eq!(
        (app, window),
        (1, 0),
        "只登记 App 级处理器时，有窗口也要执行它"
    );
}

/// 谁都没登记时静默无事（只记一条 debug 日志），不该 panic。
#[gpui::test]
fn unknown_action_is_ignored(cx: &mut TestAppContext) {
    cx.update(|cx| {
        install(cx);
        cx.set_global(Ran::default());
    });

    cx.update(|cx| {
        cx.dispatch_action(&RunCommand {
            id: "test.missing".into(),
        })
    });

    assert_eq!(cx.update(|cx| ran(cx)), (0, 0));
}
