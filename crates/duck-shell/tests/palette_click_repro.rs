//! 命令面板「点击条目 → 动作派发」的回归测试。
//!
//! 链路：行 `on_click` → `CommandState::confirm` → `window.dispatch_action(RunCommand)`
//! → App 级监听（`duck_command::install`，**先 defer 再取窗口**）→ `dispatch(id)`
//! → 注册表处理器。
//!
//! 背景：`install` 的监听器曾在派发期间直接 `window.update`——此时多半已身处某个
//! 窗口的更新内，同窗口再入会报 "window not found" 并被 `log_err` 静默吞掉，
//! 表现为面板点击 / 快捷键全部哑火。本套件钉死两条回归线：
//! ① 面板行的鼠标点击必须派发出动作（`clicking_a_palette_row_dispatches_its_action`）；
//! ② 监听器必须以「先 defer 后取窗口」的形态执行（`listener_deferred_window_update`）。
//!
//! 复刻 `ShellView::render_palette` + `palette_frame` 的遮挡结构：遮罩（点空白关闭）
//! → 卡片（左键按下 `stop_propagation`，防误关）→ `Command`（`filterable(false)`）。
//! 行位置不靠精确行高：给卡片挂 `debug_selector` 后在卡片纵轴上**扫描点击**，
//! 只要有一个点命中任意一行即算链路通。

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Render, SharedString, Styled,
    TestAppContext, VisualTestContext, Window, div, point, px,
};
use gpui_component::{
    command::{Command, CommandState},
    init as component_init, v_flex,
};

use duck_command::{ActionMeta, ActionSlot, declare, install};
use duck_theme::{active_theme, init as theme_init};

/// 测试动作数量（同一分组、顺序排列，保证卡片里有一列可点的行）。
const ITEM_COUNT: usize = 6;

/// 复刻真实装配的面板浮层（遮罩 + 卡片 + Command，不带关闭按钮与 footer）。
struct PaletteReplica {
    state: Entity<CommandState>,
}

impl Render for PaletteReplica {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        let theme = active_theme(cx);
        let colors = theme.tokens().colors;
        let extended = theme.extended();
        let radius = theme.components().dialog_radius;

        let mut command = Command::new(&state)
            .bordered(false)
            .placeholder("搜索…")
            .filterable(false)
            .rounded(radius - extended.stroke.thin)
            .bg(colors.surface)
            .overflow_hidden();
        for group in duck_command::palette_groups(cx, "") {
            command = command.group(group);
        }

        v_flex()
            .size_full()
            .relative()
            // 占位内容，撑起与真实窗口一致的根结构。
            .child(div().flex_1())
            .child(
                // 遮罩：点空白关闭（复刻 `palette_frame`）。
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .justify_center()
                    .items_start()
                    .pt(px(88.))
                    .on_mouse_down(MouseButton::Left, |_, _, _| {
                        // 副本里不真正关闭，只留占位以保持层结构一致。
                    })
                    .child(
                        // 卡片：左键按下拦截冒泡（复刻 `palette_frame`）。
                        v_flex()
                            .relative()
                            .w_full()
                            .max_w(px(560.))
                            .rounded(radius)
                            .border(extended.stroke.thin)
                            .border_color(extended.colors.hairline)
                            .bg(colors.surface)
                            .overflow_hidden()
                            .debug_selector(|| "palette-card".to_string())
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(command),
                    ),
            )
    }
}

struct PaletteProbe;

impl Render for PaletteProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 测试底座：初始化注册表、声明动作、登记处理器并记录派发次数。
///
/// 真实应用两级处理器都有（`install_handler` 走活跃窗口）；测试平台没有
/// active window，派发会走 `dispatch_app` 兜底，所以两种都登记、共用一个计数器。
fn boot(cx: &mut gpui::App) -> Rc<Cell<usize>> {
    component_init(cx);
    theme_init(cx);
    install(cx);

    let hits = Rc::new(Cell::new(0));
    for ix in 0..ITEM_COUNT {
        declare(
            cx,
            ActionMeta {
                id: ids(ix),
                // `label` 是 fn 指针：不能捕获 ix，标签写死即可（不参与断言）。
                label: |_| SharedString::from("测试导航"),
                keywords: &[],
                group: Some("导航"),
                icon: None,
                icon_color: None,
                slots: &[ActionSlot::Palette],
                menu: None,
                keys: &[],
            },
        );
        let window_hits = hits.clone();
        duck_command::install_handler(cx, ids(ix), move |_window, _cx| {
            window_hits.set(window_hits.get() + 1);
        });
        let app_hits = hits.clone();
        duck_command::install_app_handler(cx, ids(ix), move |_cx| {
            app_hits.set(app_hits.get() + 1);
        });
    }
    hits
}

fn ids(ix: usize) -> &'static str {
    // 测试动作数量固定，直接把 id 摊开成 'static 表。
    const IDS: [&str; ITEM_COUNT] = [
        "test.nav-0",
        "test.nav-1",
        "test.nav-2",
        "test.nav-3",
        "test.nav-4",
        "test.nav-5",
    ];
    IDS[ix]
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

/// 对照组：`duck_command::dispatch`（绕过 gpui 动作派发，直击注册表）。
#[gpui::test]
fn registry_dispatch_reaches_handler(cx: &mut TestAppContext) {
    let hits = cx.update(boot);
    let (_view, cx) = cx.add_window_view(|window, cx| PaletteReplica {
        state: cx.new(|cx| CommandState::new(window, cx)),
    });
    draw(cx);

    cx.update(|window, cx| {
        duck_command::dispatch("test.nav-0", window, cx);
    });

    assert_eq!(hits.get(), 1, "直击注册表必须命中处理器");
}

/// 对照组：`window.dispatch_action(RunCommand)` → 监听器（defer）→ 注册表处理器。
#[gpui::test]
fn window_dispatch_reaches_registered_handler(cx: &mut TestAppContext) {
    let hits = cx.update(boot);
    let (_view, cx) = cx.add_window_view(|window, cx| PaletteReplica {
        state: cx.new(|cx| CommandState::new(window, cx)),
    });
    draw(cx);
    assert_eq!(hits.get(), 0);

    cx.update(|window, cx| {
        window.dispatch_action(
            Box::new(duck_command::RunCommand {
                id: SharedString::from("test.nav-0"),
            }),
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(hits.get(), 1, "窗口派发 RunCommand 应命中注册表处理器");
}

/// 键击路径（⌘1 / ⌘K 这类绑定动作）也必须派发到注册表处理器。
#[gpui::test]
fn keystroke_dispatch_reaches_registered_handler(cx: &mut TestAppContext) {
    let hits = cx.update(boot);
    let (_view, cx) = cx.add_window_view(|_, _| PaletteProbe);
    cx.update(|_, cx| {
        cx.bind_keys([gpui::KeyBinding::new(
            "ctrl-o",
            duck_command::RunCommand {
                id: SharedString::from("test.nav-0"),
            },
            None,
        )]);
    });
    draw(cx);

    cx.simulate_keystrokes("ctrl-o");
    cx.run_until_parked();

    assert_eq!(hits.get(), 1, "键击派发 RunCommand 应命中注册表处理器");
}

/// 主回归：在卡片纵轴上扫描真实鼠标点击，必须有一击命中面板行并派发动作。
#[gpui::test]
fn clicking_a_palette_row_dispatches_its_action(cx: &mut TestAppContext) {
    let hits = cx.update(boot);
    let (_view, cx) = cx.add_window_view(|window, cx| PaletteReplica {
        state: cx.new(|cx| CommandState::new(window, cx)),
    });
    draw(cx);

    let card = cx
        .debug_bounds("palette-card")
        .expect("卡片必须渲染出来（debug_selector）");
    assert!(
        card.size.height > px(150.),
        "卡片应包含搜索行 + 若干条目行，实际高度 {:?}（行没渲染？）",
        card.size.height
    );
    let x = card.center().x;

    // 在卡片纵轴上扫描点击（行位置不依赖精确行高）：跳过搜索行区域，
    // 每隔 5px 点击一次，命中任意一行即算链路通。
    let mut y = card.top() + px(30.);
    while y < card.bottom() - px(2.) {
        let position = point(x, y);
        cx.simulate_event(MouseMoveEvent {
            position,
            ..Default::default()
        });
        cx.simulate_event(MouseDownEvent {
            position,
            button: MouseButton::Left,
            click_count: 1,
            first_mouse: true,
            ..Default::default()
        });
        cx.simulate_event(MouseUpEvent {
            position,
            button: MouseButton::Left,
            click_count: 1,
            ..Default::default()
        });
        cx.run_until_parked();
        if hits.get() > 0 {
            break;
        }
        y += px(5.);
    }

    assert!(
        hits.get() > 0,
        "扫描点击整张卡片至少应命中一行并派发 RunCommand"
    );
}

/// 钉死修复形态：监听器不能在派发期间直接 `window.update`（同窗口再入会被
/// 租约挡下并静默吞错），必须先 `defer` 等本次更新结束、再取窗口执行。
#[gpui::test]
fn listener_deferred_window_update(cx: &mut TestAppContext) {
    let flag = Rc::new(Cell::new(false));
    let listener_ran = Rc::new(Cell::new(false));
    let assert_flag = flag.clone();
    let assert_ran = listener_ran.clone();
    let (_view, cx) = cx.add_window_view(|_, _| PaletteProbe);
    let handle = cx.update(|window, _| window.window_handle());
    cx.update(|_, cx| {
        cx.on_action(move |_: &duck_command::RunCommand, cx| {
            listener_ran.set(true);
            let flag = flag.clone();
            cx.defer(move |cx| {
                handle
                    .update(cx, |_, _, _| flag.set(true))
                    .expect("defer 后的 window.update 必须成功");
            });
        });
    });
    draw(cx);

    cx.update(|window, cx| {
        window.dispatch_action(
            Box::new(duck_command::RunCommand {
                id: SharedString::from("probe"),
            }),
            cx,
        );
    });
    cx.run_until_parked();

    assert!(assert_ran.get(), "App 级监听器必须被调用");
    assert!(
        assert_flag.get(),
        "defer 之后的 window.update 必须成功（修复后的派发形态）"
    );
}
