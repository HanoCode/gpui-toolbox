//! 「弹窗恒居中 + 不再可拖」的无头回归测试。
//!
//! # 为什么值得单测
//!
//! 面板落点由库算死（`Positioner::corner(Anchor::TopLeft, point(x, y))`，`y` 只认
//! `DialogProps::margin_top`），库里没有「居中」这个东西——本仓是自己量出内容盒高度、
//! 加上 [`panel_chrome`] 补齐的面板边距，再反推 `margin_top`。其中「面板边距」是一串
//! 与库里实现耦合的推导值（16×3 内边距/gap + 1×2 边框 + 一行 `density.control` 脚的
//! 按钮高），**上游一改就会静默偏掉**：弹窗不会报错，只是不再居中。
//!
//! 所以这里用真实布局 + 真实绘制断言最终落点，不碰任何内部字段：
//! `debug_bounds("dialog-0")` 读的是库给面板挂的 `debug_selector`
//! （`gpui-component/src/dialog/dialog.rs:668`），给的是**画出来**的绝对坐标。
//!
//! 另一条同样容易悄悄退化的契约是「标题行不再是拖拽把手」——用一次真实拖拽手势钉住。

use std::rc::Rc;

use duck_components::{
    ButtonVariant, ConfirmHandler, ModalDrag, ModalOffset, dialog_footer, dialog_top_offset,
    modal_drag, modal_drag_area, modal_offset, modal_title_row,
};
use gpui::{
    AppContext as _, Bounds, Context, Entity, Half as _, IntoElement, Modifiers, MouseButton,
    ParentElement as _, Pixels, Render, Size, Styled as _, TestAppContext, VisualTestContext,
    Window, div, point, px, size,
};
use gpui_component::{Root, WindowExt as _, init as component_init, v_flex};

/// 弹窗面板宽度，用于核对水平居中。
const DIALOG_WIDTH: f32 = 400.;

fn window_size() -> Size<Pixels> {
    size(px(1000.), px(800.))
}

/// 宿主视图：只铺一层空 div，弹窗层由 `gpui_component::Root` 渲染
/// （`WindowExt::open_dialog` 把层挂在 Root 上，Root 不在窗口根上就画不出来）。
struct Host;

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

/// 按 `main.rs` 的顺序初始化（组件 → 主题），窗口根用 `Root`。
fn harness(cx: &mut TestAppContext) -> &mut VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
        // 关掉动效：`Dialog` 的下落动画把落点插值成 `y * delta`，开着就永远读到半路。
        cx.set_reduce_motion(true);
    });
    let (_, cx) = cx.add_window_view(|window, cx| {
        let host = cx.new(|_| Host);
        Root::new(host, window, cx)
    });
    cx.simulate_resize(window_size());
    draw(cx);
    cx
}

/// 绘一帧并清掉帧，让 prepaint（量尺）与 paint（debug_bounds）都真的跑过。
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
}

/// 只画一帧，不先跑事件循环——避免测试 App 顺带把待画帧刷掉，好让「逐帧」可控。
fn draw_raw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
}

/// 交付下一帧回调（测试没有平台帧循环，要手动递一次）。
///
/// 返回值是**挂上了几个**回调；动效被减弱时库的下落动画一帧就结束、不再要帧，
/// 于是这个数字只可能来自 [`modal_drag_area`] 的量尺。
fn next_frame(cx: &mut VisualTestContext) -> usize {
    cx.update(|window, cx| window.simulate_next_frame(cx))
}

/// 当前这一帧算出来的落点与显隐。
fn offset_of(cx: &mut VisualTestContext, geometry: &Entity<ModalDrag>) -> ModalOffset {
    cx.update(|_, cx| modal_offset(geometry, cx))
}

/// 挂上一个用本仓模态原语的弹窗，返回几何状态句柄 + **首帧 builder 会拿到的那份 offset**。
///
/// 第二个返回值是在 `open_dialog` 之后、**任何一帧画出来之前**算的：`open_dialog` 只是把
/// 弹窗登记进 `Root` 的层，真正调用 builder 是在之后的第一帧。所以这份值就是首帧的输入，
/// 也正是「还没量到高度」的那个状态。`body_height` 控制内容高度。
fn open_raw(cx: &mut VisualTestContext, body_height: f32) -> (Entity<ModalDrag>, ModalOffset) {
    let confirm: ConfirmHandler = Rc::new(|_, _| true);
    cx.update(|window, cx| {
        // ⚠️ 几何状态必须建在 `open_dialog` **之外**：闭包每帧重跑，建在里面等于每帧
        // 换一份新状态，上一帧量到的高度永远读不到（弹窗会一直贴在窗口顶部）。
        let geometry = modal_drag(cx);
        let geometry_for_dialog = geometry.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let geometry_for_content = geometry_for_dialog.clone();
            let confirm = Rc::clone(&confirm);
            let offset = modal_offset(&geometry_for_dialog, cx);
            dialog
                .w(px(DIALOG_WIDTH))
                .ml(offset.x)
                // 镜像生产写法：库默认还会自画一个右上角 ✕，与 `modal_title_row` 的
                // 关闭按钮叠成两个（生产 11 处调用点都已显式关掉）。
                .close_button(false)
                .margin_top(dialog_top_offset(cx) + offset.y)
                // 同理镜像生产：量到高度之前面板整块透明（首帧落点是从零值算的）。
                .opacity(offset.opacity)
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &geometry_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "centering-title",
                                &geometry_for_content,
                                "标题",
                                cx,
                            ))
                            .child(div().h(px(body_height)).child("正文")),
                    ))
                })
                .footer(dialog_footer(
                    "centering-footer",
                    "确定",
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        // 一帧都还没画：这份就是首帧 builder 的输入（见上面文档）。
        let first_frame = modal_offset(&geometry, cx);
        (geometry, first_frame)
    })
}

/// 开一个弹窗并绘制到落点稳定：第一帧量高度，第二帧才按量到的值摆正。
///
/// ⚠️ 中间那个 [`next_frame`] **不是保险，是必需**：测试 App 没有平台帧循环，
/// [`modal_drag_area`] 在 prepaint 里挂上的「下一帧回调」没人递交
/// （[`Window::on_next_frame`] 的注释与 `next_frame()` 的文档都写着这一条）。
/// 少了它，量到高度后的那次重绘永远不发生，面板就停在「还没量到」的贴顶落点。
///
/// 它之所以曾经看着是好的：`open_raw` / `draw` 里的 `cx.update` 出去时会把当时**已有的**
/// 待画帧顺手刷掉——而窗口里还留着别的帧需求（首个弹窗的开场动效、`Root` 的首次布局）
/// 时才会发生。**先开关过一个弹窗之后**这条侥幸就没了：帧需求归零，回调就一直躺着，
/// 于是「关掉一个弹窗、再开任意一个弹窗」都会贴顶（2026-10-15 实测：单独开 200 / 400
/// 都居中，`close()` 之后再开则一律停在 `y=16`，与内容高度无关）。
fn open(cx: &mut VisualTestContext, body_height: f32) {
    let _ = open_raw(cx, body_height);
    draw(cx); // 首帧：面板整块透明，prepaint 量到内容高度、挂下一帧回调
    next_frame(cx); // 手动递交那一帧（测试没有平台帧循环）
    draw(cx); // 第二帧：按量到的落点摆正
}

/// 关掉当前弹窗并绘两帧，让 `dialog-0` 这个层号能被下一次打开重新占用。
///
/// ⚠️ 同 [`open`]：测试没有平台帧循环，关闭前那个弹窗挂上的回调也要**自己递掉**。
/// 不递的话它会一直躺在队列里，被**下一次** `open` 的递帧一并执行——于是「下一次打开的
/// 弹窗拿到几帧」取决于上一次关没关过，测试就变成看顺序的（2026-10-15 的失败正是这种
/// 顺序耦合：同一段代码，单独开绿、关过之后再开红）。
fn close(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.close_dialog(cx));
    draw(cx);
    next_frame(cx);
    draw(cx);
}

fn panel(cx: &mut VisualTestContext) -> Bounds<Pixels> {
    cx.debug_bounds("dialog-0")
        .expect("弹窗面板应已绘制（debug selector 由库挂在面板上）")
}

/// 竖直居中：面板中心落在窗口内容区中心。
#[gpui::test]
fn dialog_is_vertically_centered_in_the_window(cx: &mut TestAppContext) {
    let cx = harness(cx);
    open(cx, 200.);

    let bounds = panel(cx);
    let window = window_size();

    // 面板高度必须明显小于窗口：量尺若落错包含块（量成了窗口），这里会先炸，
    // 比后面那条「中心偏移」更容易看出根因。
    assert!(
        bounds.size.height < px(600.),
        "面板高度应由内容决定，而不是窗口高度（实测 {:?}）",
        bounds.size.height
    );

    let center = bounds.origin.y + bounds.size.height.half();
    let expected = window.height.half();
    assert!(
        (f32::from(center) - f32::from(expected)).abs() <= 2.,
        "面板应竖直居中：实测中心 {center:?}，期望 {expected:?}（面板 {bounds:?}）"
    );

    // 水平居中由库负责，不该被本改动带偏。
    let expected_x = (f32::from(window.width) - DIALOG_WIDTH) / 2.;
    assert!(
        (f32::from(bounds.origin.x) - expected_x).abs() <= 0.5,
        "面板应水平居中：实测 x {:?}，期望 {expected_x}",
        bounds.origin.x
    );
}

/// 内容高度变化时落点跟着重算（面板高度不是写死的）。
#[gpui::test]
fn a_taller_body_keeps_the_dialog_centered(cx: &mut TestAppContext) {
    let cx = harness(cx);
    let window = window_size();

    open(cx, 200.);
    let short = panel(cx);
    close(cx);

    open(cx, 400.);
    let tall = panel(cx);

    assert!(
        f32::from(tall.size.height) - f32::from(short.size.height) >= 199.,
        "正文高 200 → 400，面板应同步长高约 200px（实测 {:?} → {:?}）",
        short.size.height,
        tall.size.height
    );

    let center = tall.origin.y + tall.size.height.half();
    assert!(
        (f32::from(center) - f32::from(window.height.half())).abs() <= 2.,
        "长内容弹窗同样要居中：实测中心 {center:?}（面板 {tall:?}）"
    );
}

/// 「关掉一个弹窗、再开另一个」也必须居中——实机最常走的一条路径（关掉表单、再开新链接）。
///
/// 为什么与上一条分开留：上一条的失败信息是「长内容不居中」，看着像量尺算错了；而
/// 2026-10-15 实测的真凶是**窗口里关过一次弹窗之后，量到高度的那一帧没人递交**——
/// 与内容高度无关（拿正文 200 去测一样贴顶，实测中心 169px）。两条钉的是两种不同的
/// 失败形态（量错 vs 没重绘），别合并成一条。
#[gpui::test]
fn a_dialog_opened_after_a_close_is_still_centered(cx: &mut TestAppContext) {
    let cx = harness(cx);
    let window = window_size();

    open(cx, 200.);
    close(cx);
    open(cx, 200.);

    let bounds = panel(cx);
    let center = bounds.origin.y + bounds.size.height.half();
    assert!(
        (f32::from(center) - f32::from(window.height.half())).abs() <= 2.,
        "关掉一个弹窗后再开的弹窗同样要居中：实测中心 {center:?}（面板 {bounds:?}）"
    );
}

/// 内容比窗口还高的弹窗：库会把面板钳进窗口，至少不该被顶出屏幕。
#[gpui::test]
fn a_taller_than_window_dialog_stays_inside_the_window(cx: &mut TestAppContext) {
    let cx = harness(cx);
    open(cx, 1400.);

    let bounds = panel(cx);
    let window = window_size();

    assert!(
        bounds.origin.y >= px(0.),
        "面板顶边不该被推到窗口外（实测 {:?}）",
        bounds.origin.y
    );
    assert!(
        bounds.origin.y + bounds.size.height <= window.height,
        "面板底边不该超出窗口（实测底边 {:?}，窗口高 {:?}）",
        bounds.origin.y + bounds.size.height,
        window.height
    );
}

/// 标题行不再是拖拽把手：按住标题拖一大段，面板必须纹丝不动。
///
/// 主要证据是代码里已没有任何 `on_drag` / `on_drag_move`；这条测试是廉价保险——
/// 拖动这类「不做任何事」的契约没有别的可观测形式。
#[gpui::test]
fn dragging_the_title_row_does_not_move_the_dialog(cx: &mut TestAppContext) {
    let cx = harness(cx);
    open(cx, 200.);
    let before = panel(cx);

    // 标题行在面板顶部内侧（面板 padding 16 + 标题行自身高度的一半）。
    let grab = point(before.origin.x + px(60.), before.origin.y + px(30.));
    let drag_to = point(grab.x + px(120.), grab.y + px(90.));

    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::default());
    for step in 1..=8 {
        let t = step as f32 / 8.;
        cx.simulate_mouse_move(
            point(grab.x + px(120. * t), grab.y + px(90. * t)),
            MouseButton::Left,
            Modifiers::default(),
        );
        draw(cx);
    }
    cx.simulate_mouse_up(drag_to, MouseButton::Left, Modifiers::default());
    draw(cx);
    draw(cx);

    let after = panel(cx);
    assert_eq!(
        (after.origin.x, after.origin.y),
        (before.origin.x, before.origin.y),
        "弹窗不该再支持拖动（前 {before:?}，后 {after:?}）"
    );
}

/// 「打开弹窗的第一帧」必须整块不可见，并且量到高度后**主动要一帧**。
///
/// # 这一步为什么必测
///
/// 面板高度只能等到首帧 prepaint 才量到，所以首帧一定是**不知道落点**时画的：
/// [`modal_offset`] 这时只有零值可用，面板会画在窗口顶部。2026-10-12 实机现象就是
/// 「弹窗先从顶部冒出来，再跳到居中」——两个成因叠在一起：
///
/// 1. 首帧是**可见**的（面板贴在窗口顶部，落点是零值的产物）；
/// 2. 量到高度后没人重绘：`window.refresh()` 写在 prepaint 里是空操作
///    （`Window::refresh` 整个函数体裹在 `invalidator.not_drawing()` 里），
///    动效未减弱时靠库的下落动画顺带续帧才像是好的，动效一减弱就没人要帧了 ——
///    弹窗停在窗口顶部，直到鼠标动一下、窗口被弄脏才「跳」到居中。
///
/// `harness` 已经开了 `reduce_motion`（库的动画一帧结束、不再要帧），于是「下一帧回调」
/// 这个数字只可能来自本仓的量尺；两个成因各有一条断言。
///
/// ⚠️ 断言「首帧落点」用的是 `open_raw` 带回的那份 offset，**不是**画完之后读的
/// `debug_bounds`：测试 App 的 `update` 出去时会自己把待画帧刷掉一两次，等能读到
/// 面板位置时早就不是首帧了（实测首帧读到的是第二帧的居中结果）。
#[gpui::test]
fn the_first_frame_is_invisible_and_the_measurement_asks_for_a_repaint(cx: &mut TestAppContext) {
    let cx = harness(cx);
    let (geometry, first) = open_raw(cx, 200.);

    // ① 首帧 builder 拿到的是「还没量到」那一份：落点退到窗口顶部，也就是相对基线
    //    的补差正好等于负的基线；并且整块透明——否则用户看到的就是「先从顶部冒出来」。
    let baseline = cx.update(|_, cx| f32::from(dialog_top_offset(cx)));
    assert_eq!(
        f32::from(first.y),
        -baseline,
        "首帧只能用零值几何算出的兜底落点（相对基线的补差 = -基线）"
    );
    assert_eq!(first.opacity, 0., "首帧还没有落点，面板必须整块不可见");

    // ② 量到高度 → 必须自己挂一帧回调（库的动画此时已经结束，不会再要帧）。
    assert!(
        next_frame(cx) > 0,
        "量到高度后必须主动要一帧，否则没人重绘，弹窗会一直贴在窗口顶部"
    );

    // ③ 要来的那一帧把面板摆到居中并露脸。
    draw_raw(cx);
    let settled = panel(cx);
    let window = window_size();
    let center = settled.origin.y + settled.size.height.half();
    assert!(
        (f32::from(center) - f32::from(window.height.half())).abs() <= 2.,
        "补上的那一帧应把面板摆到居中：实测中心 {center:?}，期望 {:?}（面板 {settled:?}）",
        window.height.half()
    );
    assert_eq!(
        offset_of(cx, &geometry).opacity,
        1.,
        "落点确定之后面板要露脸"
    );

    // ④ 高度没变就不再要帧——否则量尺每帧回调都会要帧，自激成常驻重绘。
    draw_raw(cx);
    assert_eq!(
        next_frame(cx),
        0,
        "高度没变时不该再要帧（量尺的 prepaint 回调每帧都跑）"
    );
}

/// 同一份判据的另一面：**没有**「可见 + 落点未知」的帧。
///
/// 逐帧看 `(opacity, y)`——读的是**每帧 builder 的输入**（画之前采的样）：只要面板是
/// 可见的（`opacity > 0`），它用的就必须是量到高度之后算出来的落点，绝不能是零值兜底的
/// 那一份（`-dialog_top_offset`）。
#[gpui::test]
fn no_visible_frame_uses_the_unmeasured_fallback_position(cx: &mut TestAppContext) {
    let cx = harness(cx);
    let (geometry, first) = open_raw(cx, 200.);

    let fallback_y = cx.update(|_, cx| -f32::from(dialog_top_offset(cx)));
    let mut frames = vec![(first.opacity, f32::from(first.y))];
    for _ in 0..4 {
        next_frame(cx);
        draw_raw(cx);
        let offset = offset_of(cx, &geometry);
        frames.push((offset.opacity, f32::from(offset.y)));
    }

    assert!(
        frames
            .iter()
            .all(|(opacity, y)| *opacity == 0. || (*y - fallback_y).abs() > 1.),
        "有帧把面板按「还没量到」的落点画了出来：{frames:?}（兜底 y = {fallback_y}）"
    );
    assert!(
        frames.iter().any(|(opacity, _)| *opacity > 0.),
        "面板最终还是得露脸：{frames:?}"
    );
}
