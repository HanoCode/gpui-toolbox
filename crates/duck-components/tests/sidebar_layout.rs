//! 右侧栏「不把主内容放行到侧栏上 / 不压扁主内容 / 自带底色」的无头布局回归测试。
//!
//! # 为什么值得单测
//!
//! 2026-10-14 实机现象：数据库页右栏展开后**主内容从面板里溢出来、透过后栏显形**
//! （截图上「右栏区域」里能看见主内容的工具条下拉与空态文案）。根因不在页面：
//! [`SidebarPanel`] 把侧栏宽度交给 `ResizableState` 的实测尺寸驱动，只在**拖拽结束**
//! 时按 `MAX_SHARE` 修正 `SidebarState::width`（那份值根本不是布局真源），窗口一变窄
//! 或从折叠态展开时没有任何重钳 —— 侧栏可以占掉整条内容区的宽度，主内容被压到
//! `PANEL_MIN_SIZE`（100px）；而 gpui 的 `div` 默认**不裁剪**子内容，页面（编辑器工具条 /
//! 结果网格）放不下自己的最小宽时就直接画进了侧栏区域，侧栏又**没有底色**，
//! 于是溢出内容全保真透出。三条不变量各钉一条测试：
//!
//! 1. 主内容**一个像素都不许落到侧栏上**（按绘制四边形 + 内容遮罩算可见区）；
//! 2. 侧栏不超过内容区宽度的 `MAX_SHARE`（60%）；
//! 3. 侧栏自绘**不透明**的一层 `chrome` 平面（与设置页左栏同一族）。

use duck_components::{SidebarPanel, SidebarState};
use gpui::{
    AppContext as _, Bounds, Context, Entity, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, Size, Styled as _, TestAppContext, VisualTestContext,
    Window, div, px, rgb, size,
};
use gpui_component::init as component_init;

fn window_size() -> Size<Pixels> {
    size(px(900.), px(700.))
}

/// 主内容按「一个放不下就会溢出的页面」建模：最小宽度大于内容区，模拟 db 页那种
/// 自身有最小宽（编辑器工具条 + 结果网格）的真实页面。
const CONTENT_MIN_WIDTH: f32 = 520.;

/// 主内容的标识色：全场只有它被画成这个颜色，用来在绘制四边形里认出它。
fn content_paint() -> Hsla {
    rgb(0xff00ff).into()
}

struct Harness {
    state: Entity<SidebarState>,
    /// 主内容自身的最小宽：`0` 表示「能被压扁的页面」（如 Redis / 工具箱），
    /// 大于内容区则表示「压不下就会溢出的页面」（如数据库页）。
    content_min: Pixels,
}

impl Render for Harness {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = div()
            .debug_selector(|| "content".to_owned())
            .size_full()
            .min_w(self.content_min)
            .bg(content_paint());
        let sidebar = div()
            .debug_selector(|| "sidebar".to_owned())
            .size_full()
            .child("侧栏");
        let mut panel = SidebarPanel::new_right("layout-probe", &self.state, content, window, cx);
        if panel.is_sidebar_visible() {
            panel = panel.sidebar(sidebar);
        }
        // 页面底板：与 shell 根容器一样铺一层 `colors.background`。
        let background = duck_theme::active_theme(cx).tokens().colors.background;
        div().size_full().bg(background).child(panel)
    }
}

/// 按 `main.rs` 的顺序初始化（组件 → 主题），并关掉动效（展开量一帧到位）。
fn harness(
    cx: &mut TestAppContext,
    window_size: Size<Pixels>,
    content_min: Pixels,
) -> &mut VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
        cx.set_reduce_motion(true);
    });
    let (_, cx) = cx.add_window_view(|_, cx| Harness {
        state: cx.new(|cx| SidebarState::new(px(400.), px(260.)..px(400.), cx)),
        content_min,
    });
    cx.simulate_resize(window_size);
    draw(cx);
    cx
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
}

fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 应已绘制"))
}

/// 主内容那层色块**实际可见**的右缘（绘制四边形 ∩ 内容遮罩），单位是设备像素。
///
/// 没有可见色块时返回 `None`（内容被完全裁掉）。
fn content_visible_right_edge(cx: &mut VisualTestContext) -> Option<f32> {
    let paint = content_paint();
    cx.update(|window, _| {
        window
            .painted_quads()
            .iter()
            .filter(|quad| quad.background.as_solid() == Some(paint))
            .map(|quad| {
                let b = quad.bounds;
                let m = quad.content_mask.bounds;
                let left = b.origin.x.0.max(m.origin.x.0);
                let right = (b.origin.x.0 + b.size.width.0).min(m.origin.x.0 + m.size.width.0);
                right.max(left)
            })
            .reduce(f32::max)
    })
}

/// 主内容**一个像素都不许落到侧栏上**（这正是实机「右栏背景透出主内容」的形状）。
#[gpui::test]
fn the_content_never_bleeds_onto_the_sidebar(cx: &mut TestAppContext) {
    // 三种窗口宽：宽（不用让位）、中（刚好让位）、窄（内容放不下、必须被裁）。
    for width in [900., 700., 600.] {
        let window = size(px(width), px(700.));
        let cx = harness(cx, window, px(CONTENT_MIN_WIDTH));
        let sidebar = bounds(cx, "sidebar");
        let scale = cx.update(|window, _| window.scale_factor());
        let sidebar_left = f32::from(sidebar.origin.x) * scale;

        if let Some(right) = content_visible_right_edge(cx) {
            assert!(
                right <= sidebar_left + scale,
                "窗口宽 {width}：主内容可见右缘 {right:.1} 越过了侧栏左缘 {sidebar_left:.1} \
                 ⇒ 内容画到了侧栏区域上（侧栏无底色时就会「透出」）"
            );
        }
    }
}

/// 侧栏不超过内容区宽度的 60%（`MAX_SHARE`），无论它记住的宽度（400）是多少。
///
/// 主内容取「能被压扁的页面」：这样唯一挡住侧栏吃满整条内容区的就是占比闸门
/// （内容自身的最小宽会把侧栏顶回去，那是另一条测试）。
///
/// 窗口取 500：60% = 300 < 记住的 400，闸门必须真的咬住。
#[gpui::test]
fn the_sidebar_caps_itself_at_sixty_percent(cx: &mut TestAppContext) {
    let window = size(px(500.), px(700.));
    let cx = harness(cx, window, px(0.));
    let sidebar = bounds(cx, "sidebar");

    assert!(
        sidebar.size.width <= px(f32::from(window.width) * 0.6 + 1.),
        "侧栏超过内容区宽度的 60%：sidebar {:?} / 窗口 {:?}",
        sidebar.size.width,
        window.width
    );
}

/// 窗口再窄，主内容也要留下可用宽度（`MIN_CONTENT_WIDTH`）。
///
/// 窗口 500、主内容能压扁：闸门取「60% = 300」与「留 240 → 260」的较小者，
/// 于是侧栏 260、主内容 240。没有这道闸门时侧栏会拿满记住的 400，
/// 主内容只剩 100 —— 正是实机「主内容被压扁」那一半。
#[gpui::test]
fn the_content_keeps_a_usable_width_on_a_narrow_window(cx: &mut TestAppContext) {
    let cx = harness(cx, size(px(500.), px(700.)), px(0.));
    let content = bounds(cx, "content");
    let sidebar = bounds(cx, "sidebar");

    assert!(
        content.size.width >= px(240.),
        "窄窗口下主内容只剩 {:?}（应至少留 240px）；sidebar {:?}",
        content.size.width,
        sidebar.size.width
    );
}

/// 侧栏自绘**不透明**的一层 `chrome` 平面：没有它，任何位于侧栏之下的像素都会透出。
#[gpui::test]
fn the_sidebar_paints_an_opaque_chrome_plane(cx: &mut TestAppContext) {
    let cx = harness(cx, window_size(), px(0.));
    let sidebar = bounds(cx, "sidebar");
    let scale = cx.update(|window, _| window.scale_factor());
    let expected =
        cx.update(|_, cx| -> Hsla { duck_theme::active_theme(cx).extended().colors.chrome });

    let (quads, page) = cx.update(|window, cx| {
        let page: Hsla = duck_theme::active_theme(cx).tokens().colors.background;
        (window.painted_quads(), page)
    });

    // 侧栏区域（换算成设备像素）里最靠上的一层「非页面底色」的实底色。
    let left = f32::from(sidebar.origin.x) * scale;
    let right = f32::from(sidebar.origin.x + sidebar.size.width) * scale;
    let plane = quads
        .iter()
        .filter_map(|quad| {
            let b = quad.bounds;
            let covers = b.origin.x.0 <= left + 1. && b.origin.x.0 + b.size.width.0 >= right - 1.;
            if covers {
                quad.background.as_solid()
            } else {
                None
            }
        })
        .rfind(|color| *color != page)
        .expect(
            "侧栏区域应有一层**非页面底色**的实底色（此前这里只有页面底色 ⇒ 侧栏没有自己的平面）",
        );

    assert!(
        plane.a >= 0.999,
        "侧栏底色必须不透明（实得 alpha {}）",
        plane.a
    );
    assert!(
        (plane.h - expected.h).abs() < 0.01
            && (plane.s - expected.s).abs() < 0.01
            && (plane.l - expected.l).abs() < 0.01,
        "侧栏底色应是主题的 `chrome` 平面：实得 {plane:?}，期望 {expected:?}"
    );
}
