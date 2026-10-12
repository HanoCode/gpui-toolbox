//! 「把标签拖到画布右半并入分屏」的无头回归测试。
//!
//! # 为什么测的是**复刻结构**而不是整页
//!
//! 这条链路全部由 gpui 的鼠标事件规则决定，跟终端页的数据模型无关：
//!
//! 1. **拖拽何时成立**：`Div` 把 mouse-down 记进 `pending_mouse_down`，之后第一个
//!    位移超过 `DRAG_THRESHOLD`（`div.rs:52`，2px）的 mouse-move（**Bubble 相位**，
//!    `div.rs:2975`）才把 `on_drag` 的构造器叫起来，并把值放进 `cx.active_drag`；
//! 2. **`on_drag_move` 何时回调**：监听器被注册成 `window.on_mouse_event`，
//!    条件是 **Capture 相位 + `active_drag` 的类型对得上**（`div.rs:360`），
//!    与指针是否悬停无关 —— `DragMoveEvent::bounds` 给的就是**接收方自己**的
//!    hitbox 边界，投放到哪一半的判定因此写在这条回调里；
//! 3. **`on_drop` 何时回调**：**MouseUp 的 Bubble 相位 + `hitbox.is_hovered()`**
//!    （`div.rs:2903`）。少了前一条（悬停跟踪）后一条就永远不会触发。
//!
//! 所以这里复刻「拖拽源标签 + 一个包着画布的投放容器」这层骨架，
//! 钉住「越过中线 → 悬停点亮 → 松手回调被调用」这条契约。
//!
//! # ⚠️ 测试里必须手动补两帧
//!
//! 鼠标事件的派发路径来自**上一帧**渲染时登记的 `rendered_frame.mouse_listeners`
//! （`window.rs:5806`），而 `dispatch_mouse_event` **不会**像键盘那样顺手补一次
//! `draw`（`window.rs:5871` 的 `dispatch_key_event` 才会）。真实运行时平台按帧驱动
//! 重绘，测试里没有这个驱动，因此：
//!
//! - 派发任何鼠标事件**之前**先画一帧，否则一个监听器都没有；
//! - `on_drag` 的构造器里 `cx.notify()` 之后（真实运行时下一帧就重绘了）
//!   也要补一帧，否则投放区的 `on_drag_move` 还停在「拖拽尚未开始」的那一帧。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    AppContext as _, Context, DragMoveEvent, InteractiveElement as _, IntoElement, Modifiers,
    MouseButton, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    TestAppContext, VisualTestContext, Window, div, point, px,
};

/// 拖拽载荷：与 `pages::terminal::TabDrag` 同形 —— 只带被拖标签的 id。
struct TabDrag {
    tab_id: String,
}

/// 跟在指针后面的幽灵视图（`on_drag` 要求当场构造一个可渲染视图）。
struct Ghost;

impl Render for Ghost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child("ghost")
    }
}

/// 观测点。全是 `Rc` 句柄：测试在窗口外也能读，不必再借一层上下文。
#[derive(Default)]
struct Probes {
    /// `on_drag` 的构造器被叫起来了（拖拽真的成立）。
    drag_started: Cell<bool>,
    /// `on_drag_move` 的回调次数。
    moves: Cell<usize>,
    /// 指针是否越过了投放容器的中线。
    hover: Cell<bool>,
    /// `on_drop` 是否被触发了。
    drop_fired: Cell<bool>,
    /// 实际被「并入」的载荷（`on_drop` 里按悬停态裁决，与
    /// `TerminalPage::finish_tab_drop` 同一套判据）。
    dropped: RefCell<Option<String>>,
}

/// 标签栏的高度。投放容器从它下面开始 —— 测试坐标由它推出，不用读 `debug_bounds`。
const TAB_HEIGHT: f32 = 30.;

/// 拖拽源 + 投放区的最小复刻。
struct Scene {
    probes: Rc<Probes>,
}

impl Render for Scene {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let probes = self.probes.clone();
        // `on_drag_move` 的回调签名是 `Fn(&DragMoveEvent<T>, &mut Window, &mut App)`
        // —— 拿到的是 `App` 而不是本页的 `Context<Self>`，`notify()` 必须显式给实体 id。
        // （真实代码里 notify 只为重绘高亮；投放裁决读的是字段，不依赖这一帧。）
        let scene_id = cx.entity_id();

        let tab = div()
            .id("tab")
            .w_full()
            .h(px(TAB_HEIGHT))
            .flex_none()
            .on_drag(
                TabDrag {
                    tab_id: "second".to_owned(),
                },
                {
                    let probes = probes.clone();
                    move |_drag: &TabDrag, _point, _window, cx| {
                        probes.drag_started.set(true);
                        cx.new(|_| Ghost)
                    }
                },
            );

        let drop_target = div()
            .id("drop")
            .relative()
            .w_full()
            .flex_1()
            .on_drag_move::<TabDrag>({
                let probes = probes.clone();
                move |event: &DragMoveEvent<TabDrag>, _window, cx| {
                    // 与 `pages::terminal::with_split_drop_target` 同一条判据：
                    // 指针越过**容器**中线即点亮投放区。
                    let midline = event.bounds.left() + event.bounds.size.width / 2.;
                    let hovered = event.event.position.x >= midline;
                    probes.moves.set(probes.moves.get() + 1);
                    if probes.hover.get() != hovered {
                        probes.hover.set(hovered);
                        cx.notify(scene_id);
                    }
                }
            })
            .on_drop::<TabDrag>({
                let probes = probes.clone();
                move |drag: &TabDrag, _window, _cx| {
                    // 与 `TerminalPage::finish_tab_drop` 同一条裁决：
                    // 没点亮投放区（左半屏松手）就当取消，什么都不做。
                    probes.drop_fired.set(true);
                    if probes.hover.get() {
                        probes.dropped.borrow_mut().replace(drag.tab_id.clone());
                    }
                }
            })
            .child(div().size_full().child("pane"));

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(tab)
            .child(drop_target)
    }
}

/// 建一个场景窗口，返回上下文与窗口宽度。
fn scene(cx: &mut TestAppContext, probes: Rc<Probes>) -> (VisualTestContext, f32) {
    let window = cx.add_window(move |_window, _cx| Scene { probes });
    let size = window
        .update(cx, |_, window, _cx| window.bounds().size)
        .expect("窗口还在");
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    // 见模块头：鼠标事件的监听器来自上一帧。
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    (visual, size.width.to_f64() as f32)
}

/// 按住标签、拖到 `ratio`（相对窗口宽度的落点），然后松手。
fn drag_tab_to(visual: &mut VisualTestContext, width: f32, ratio: f32) {
    let tab_point = point(px(width / 2.), px(TAB_HEIGHT / 2.));
    let target = point(px(width * ratio), px(TAB_HEIGHT + 40.));

    visual.simulate_mouse_move(tab_point, None, Modifiers::none());
    visual.simulate_mouse_down(tab_point, MouseButton::Left, Modifiers::none());
    // 越过 2px 阈值：拖拽在这一步才成立。
    visual.simulate_mouse_move(
        point(px(width / 2.), px(TAB_HEIGHT / 2. + 10.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    // 拖拽成立后调用方会 `notify` 重绘；真实运行时由平台按帧驱动，这里手动补。
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    visual.simulate_mouse_move(target, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
}

#[gpui::test]
fn dragging_a_tab_over_the_right_half_drops_it(cx: &mut TestAppContext) {
    let probes = Rc::new(Probes::default());
    let (mut visual, width) = scene(cx, probes.clone());

    drag_tab_to(&mut visual, width, 0.75);

    assert!(
        probes.drag_started.get(),
        "位移越过阈值后 `on_drag` 的构造器应当被调用"
    );
    assert!(
        probes.moves.get() > 0,
        "拖拽期间 `on_drag_move` 应当持续回调（实际 {} 次）",
        probes.moves.get()
    );
    assert!(
        probes.hover.get(),
        "指针越过中线后投放区应当点亮（实际未点亮）"
    );
    assert!(
        probes.drop_fired.get(),
        "在投放容器上松手应当触发 `on_drop`"
    );
    assert_eq!(
        probes.dropped.borrow().as_deref(),
        Some("second"),
        "右半屏松手应当真的并入，并带上被拖标签的 id"
    );
}

#[gpui::test]
fn releasing_on_the_left_half_is_a_cancel(cx: &mut TestAppContext) {
    let probes = Rc::new(Probes::default());
    let (mut visual, width) = scene(cx, probes.clone());

    drag_tab_to(&mut visual, width, 0.24);

    assert!(!probes.hover.get(), "指针在中线左侧时不该点亮投放区");
    assert!(
        probes.drop_fired.get(),
        "`on_drop` 本身仍应触发（裁决在调用方，不在事件层）"
    );
    assert_eq!(
        probes.dropped.borrow().as_deref(),
        None,
        "左半屏松手 = 取消：不该并入"
    );
}
