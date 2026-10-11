//! 可拖拽调宽、可动画收起的侧栏与主内容组合。
//!
//! 与 FluxDown 的 `SidebarPanel` 同构：宽度可拖拽（`h_resizable`）、收起 / 展开带 200ms
//! 缓动，展开量同时驱动宽度与透明度。页面持有 [`SidebarState`]，shell 顶栏提供统一的
//! 收起 / 展开入口。

use std::ops::Range;
use std::time::{Duration, Instant};

use duck_theme::active_theme;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, EventEmitter, IntoElement,
    ParentElement as _, Pixels, RenderOnce, Styled as _, Window, div, px,
};
use gpui_component::{ResizableState, h_flex, h_resizable, resizable_panel};

const MOTION_DURATION: Duration = Duration::from_millis(200);

/// 侧栏最多占用窗口宽度的比例。
///
/// 拖拽本身没有上限（`width_range` 只给下限与常规上限），窗口一窄就会把内容区挤没；
/// 这里再压一道「占比上限」，保证面板旁边永远留得住东西。取自 herdr `panel_resize`
/// 的同名约束（`MAX_SHARE = 0.6`）。
const MAX_SHARE: f32 = 0.6;

/// 侧栏宽度与透明度共用的展开量；反向切换从当前帧继续，不跳到端点。
struct SidebarMotion {
    from: f32,
    to: f32,
    started_at: Option<Instant>,
}

impl SidebarMotion {
    fn settled(open: bool) -> Self {
        let amount = if open { 1. } else { 0. };
        Self {
            from: amount,
            to: amount,
            started_at: None,
        }
    }

    fn retarget(&mut self, open: bool, now: Instant, animate: bool) {
        let target = if open { 1. } else { 0. };
        if !animate {
            *self = Self::settled(open);
        } else if self.to != target {
            self.from = self.amount(now);
            self.to = target;
            self.started_at = Some(now);
        }
    }

    fn needs_retarget(&self, open: bool, animate: bool) -> bool {
        let target = if open { 1. } else { 0. };
        if animate {
            self.to != target
        } else {
            self.from != target || self.to != target || self.started_at.is_some()
        }
    }

    fn amount(&self, now: Instant) -> f32 {
        let progress = self.progress(now);
        self.from + (self.to - self.from) * ease_in_out_cubic(progress)
    }

    fn is_animating(&self, now: Instant) -> bool {
        self.from != self.to && self.progress(now) < 1.
    }

    fn progress(&self, now: Instant) -> f32 {
        self.started_at.map_or(1., |started_at| {
            (now.saturating_duration_since(started_at).as_secs_f32()
                / MOTION_DURATION.as_secs_f32())
            .clamp(0., 1.)
        })
    }
}

/// 三次缓入缓出，与 gpui-component 内置动效同曲线。
fn ease_in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4. * t * t * t
    } else {
        let f = 2. * t - 2.;
        0.5 * f * f * f + 1.
    }
}

/// 用户修改的侧栏布局；宿主可据此保存设备本地偏好。
#[derive(Clone, Copy, Debug)]
pub struct SidebarChange {
    /// 展开时的宽度，收起不会将它改成零。
    pub width: Pixels,
    /// 用户手动收起状态，与内容是否可见独立。
    pub collapsed: bool,
}

/// 页面独立的侧栏状态，供页面布局和 shell 顶栏按钮共享。
pub struct SidebarState {
    width: Pixels,
    width_range: Range<Pixels>,
    collapsed: bool,
    available: bool,
    motion: Option<SidebarMotion>,
    split: Entity<ResizableState>,
    split_initialized: bool,
}

impl SidebarState {
    /// 创建默认展开的侧栏；宽度范围由页面决定。
    pub fn new(width: Pixels, width_range: Range<Pixels>, cx: &mut Context<Self>) -> Self {
        Self {
            width: width.clamp(width_range.start, width_range.end),
            width_range,
            collapsed: false,
            available: true,
            motion: None,
            split: cx.new(|_| ResizableState::default()),
            split_initialized: false,
        }
    }

    /// 当前记住的展开宽度。
    #[must_use]
    pub fn width(&self) -> Pixels {
        self.width
    }

    /// 用户是否手动收起侧栏。
    #[must_use]
    pub fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// 是否有可供展示的侧栏内容；为假时 shell 不显示切换按钮。
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.available
    }

    /// 内容全部隐藏时立即释放占位，但保留用户的展开宽度和手动收起状态。
    pub fn set_available(&mut self, available: bool, cx: &mut Context<Self>) {
        if self.available != available {
            self.available = available;
            cx.notify();
        }
    }

    /// 直接设置用户手动收起状态（初始化默认态用，不等同于用户点击的 [`Self::toggle`]）。
    pub fn set_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.collapsed != collapsed {
            self.collapsed = collapsed;
            self.emit_change(cx);
        }
    }

    /// 用户触发收起 / 展开；无可见内容时不改变布局偏好。
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.available {
            self.collapsed = !self.collapsed;
            self.emit_change(cx);
        }
    }

    fn resize(&mut self, width: Pixels, cx: &mut Context<Self>) {
        let width = width.clamp(self.width_range.start, self.width_range.end);
        if self.width != width {
            self.width = width;
            self.emit_change(cx);
        }
    }

    fn emit_change(&self, cx: &mut Context<Self>) {
        cx.emit(SidebarChange {
            width: self.width,
            collapsed: self.collapsed,
        });
        cx.notify();
    }
}

impl EventEmitter<SidebarChange> for SidebarState {}

/// 可拖拽调宽、可动画收起的侧栏与主内容组合。
///
/// 先创建面板，再根据 [`Self::is_sidebar_visible`] 按需构造侧栏；完全隐藏时不构造导航控件。
#[derive(IntoElement)]
pub struct SidebarPanel {
    id: ElementId,
    state: Entity<SidebarState>,
    split: Entity<ResizableState>,
    width: Pixels,
    width_range: Range<Pixels>,
    amount: f32,
    /// 侧栏是否渲染在主内容右侧（构造期定死，见 [`Self::new_right`]）。
    right: bool,
    sidebar: Option<AnyElement>,
    content: AnyElement,
}

impl SidebarPanel {
    /// 采样本帧动画，并按需请求下一帧；侧栏内容以固定展开宽度布局。
    pub fn new(
        id: impl Into<ElementId>,
        state: &Entity<SidebarState>,
        content: impl IntoElement,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        Self::build(id, state, content, false, window, cx)
    }

    /// 同 [`Self::new`]，但侧栏渲染在主内容**右侧**（拖拽把手在侧栏左缘）。
    ///
    /// 侧别必须是构造期决定：`new` 每帧都会跑分栏初始化，里面用到的面板下标
    /// 依赖侧别，若做成构造后的 builder 会有首帧用错面板序的问题。
    pub fn new_right(
        id: impl Into<ElementId>,
        state: &Entity<SidebarState>,
        content: impl IntoElement,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        Self::build(id, state, content, true, window, cx)
    }

    fn build(
        id: impl Into<ElementId>,
        state: &Entity<SidebarState>,
        content: impl IntoElement,
        right: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let now = Instant::now();
        let sampled = Self::sample(state, now, window, cx, right);
        let (split, width, width_range, amount) = sampled.unwrap_or_else(|| {
            state.update(cx, |state, cx| {
                let open = state.available && !state.collapsed;
                let motion = state
                    .motion
                    .get_or_insert_with(|| SidebarMotion::settled(open));
                motion.retarget(open, now, state.available && !cx.reduce_motion());
                let amount = motion.amount(now);
                if motion.is_animating(now) {
                    window.request_animation_frame();
                }
                let content_ix = Self::content_panel_ix(right);
                if amount == 1.
                    && !state.split_initialized
                    && Self::split_has_content(state, content_ix, cx)
                {
                    state
                        .split
                        .update(cx, |split, cx| split.reset_panel(content_ix, cx));
                    state.split_initialized = true;
                }
                (
                    state.split.clone(),
                    state.width,
                    state.width_range.clone(),
                    amount,
                )
            })
        });
        Self {
            id: id.into(),
            state: state.clone(),
            split,
            width,
            width_range,
            amount,
            right,
            sidebar: None,
            content: content.into_any_element(),
        }
    }

    /// 主内容所在的面板下标（`h_resizable` 的 children 序）：左侧栏在 0，
    /// 内容在 1；右侧栏相反。
    fn content_panel_ix(right: bool) -> usize {
        if right { 0 } else { 1 }
    }

    /// 只读采样；需要改写动画 / 分栏状态时返回 `None`，由调用方走 `update`。
    fn sample(
        state: &Entity<SidebarState>,
        now: Instant,
        window: &mut Window,
        cx: &App,
        right: bool,
    ) -> Option<(Entity<ResizableState>, Pixels, Range<Pixels>, f32)> {
        let state = state.read(cx);
        let open = state.available && !state.collapsed;
        let motion = state.motion.as_ref()?;
        if motion.needs_retarget(open, state.available && !cx.reduce_motion()) {
            return None;
        }
        let amount = motion.amount(now);
        if amount == 1.
            && !state.split_initialized
            && Self::split_has_content(state, Self::content_panel_ix(right), cx)
        {
            return None;
        }
        if motion.is_animating(now) {
            window.request_animation_frame();
        }
        Some((
            state.split.clone(),
            state.width,
            state.width_range.clone(),
            amount,
        ))
    }

    /// 把拖到的宽度收进「窗口占比上限」内。
    ///
    /// `sizes` 是分栏组当前两块的尺寸；窗口宽度即两者之和。组里还没有尺寸时（首帧）
    /// 原样返回，交给 `width_range` 兜底。
    fn clamp_to_share(width: Pixels, sizes: &[Pixels]) -> Pixels {
        let total: Pixels = sizes.iter().copied().sum();
        if total <= px(0.) {
            return width;
        }
        width.min(total * MAX_SHARE)
    }

    fn split_has_content(state: &SidebarState, content_ix: usize, cx: &App) -> bool {
        state
            .split
            .read(cx)
            .sizes()
            .get(content_ix)
            .is_some_and(|size| *size > px(1.))
    }

    /// 动画尚未完全收起时仍需要侧栏内容，以完成淡出。
    #[must_use]
    pub fn is_sidebar_visible(&self) -> bool {
        self.amount > 0.
    }

    /// 添加本帧侧栏内容；主内容始终保持挂载。
    pub fn sidebar(mut self, sidebar: impl IntoElement) -> Self {
        self.sidebar = Some(sidebar.into_any_element());
        self
    }
}

impl RenderOnce for SidebarPanel {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        if self.amount == 1. {
            let state = self.state;
            // 左侧布局：[侧栏(0) | 内容(1)]，把手改侧栏宽，读 sizes()[0]；
            // 右侧布局：[内容(0) | 侧栏(1)]，把手在侧栏左缘，改内容宽、余量给
            // 侧栏，侧栏宽读 sizes()[1]。两侧都复位内容面板，让它保持弹性重测。
            let group = h_resizable(self.id).with_state(&self.split);
            if self.right {
                group
                    .on_resize(move |split, _, cx| {
                        split.update(cx, |split, cx| split.reset_panel(0, cx));
                        let sizes = split.read(cx).sizes().clone();
                        if let Some(width) = sizes.get(1).copied()
                            && width > px(0.)
                        {
                            let width = SidebarPanel::clamp_to_share(width, &sizes);
                            state.update(cx, |state, cx| state.resize(width, cx));
                        }
                    })
                    .child(resizable_panel().child(self.content))
                    .child(
                        resizable_panel()
                            .size(self.width)
                            .flex_none()
                            .size_range(self.width_range)
                            .children(self.sidebar),
                    )
                    .into_any_element()
            } else {
                group
                    .on_resize(move |split, _, cx| {
                        split.update(cx, |split, cx| split.reset_panel(1, cx));
                        let sizes = split.read(cx).sizes().clone();
                        if let Some(width) = sizes.first().copied()
                            && width > px(0.)
                        {
                            let width = SidebarPanel::clamp_to_share(width, &sizes);
                            state.update(cx, |state, cx| state.resize(width, cx));
                        }
                    })
                    .child(
                        resizable_panel()
                            .size(self.width)
                            .flex_none()
                            .size_range(self.width_range)
                            .children(self.sidebar),
                    )
                    .child(resizable_panel().child(self.content))
                    .into_any_element()
            }
        } else {
            let extended = active_theme(cx).extended();
            let content_cell = div()
                .flex_1()
                .h_full()
                .min_w_0()
                .min_h_0()
                .child(self.content);
            let mut row = h_flex().size_full().min_w_0().min_h_0();
            if self.amount > 0. {
                // 动画尚未完全收起时仍需要侧栏内容，以完成淡出。
                let sidebar_cell = div()
                    .w(self.width * self.amount)
                    .h_full()
                    .flex_none()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(self.width)
                            .h_full()
                            .opacity(self.amount)
                            .when(self.right, |edge| edge.border_l(extended.stroke.thin))
                            .when(!self.right, |edge| edge.border_r(extended.stroke.thin))
                            .border_color(extended.colors.hairline)
                            .children(self.sidebar),
                    );
                if self.right {
                    row = row.child(content_cell).child(sidebar_cell);
                } else {
                    row = row.child(sidebar_cell).child(content_cell);
                }
            } else {
                row = row.child(content_cell);
            }
            row.into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::px;

    use super::SidebarPanel;

    /// 首帧分栏组还没有尺寸时**原样返回**，不能被当成「占比 0」把宽度压成 0。
    ///
    /// `sizes()` 在第一次 `prepaint` 之前是空的；此时若照常求和，`total` 为 0、占比上限
    /// 也是 0，侧栏会在首帧被压没。空集与「两块都是 0」两种情况都钉住。
    #[test]
    fn clamp_leaves_the_width_alone_before_the_group_has_sizes() {
        assert_eq!(SidebarPanel::clamp_to_share(px(280.), &[]), px(280.));
        assert_eq!(
            SidebarPanel::clamp_to_share(px(280.), &[px(0.), px(0.)]),
            px(280.)
        );
    }

    /// 占比上限：超过两块总宽 60% 时收到 60%，未超过则原样保留。
    ///
    /// 拖拽本身没有上限（`width_range` 只给下限与常规上限），窗口一窄就会把内容区挤没；
    /// 这道占比上限保证面板旁边永远留得住东西。边界值（正好 60%）必须**不**被削。
    #[test]
    fn clamp_caps_the_width_at_sixty_percent_of_the_group() {
        let sizes = [px(200.), px(800.)]; // 总宽 1000 → 上限 600
        assert_eq!(SidebarPanel::clamp_to_share(px(600.), &sizes), px(600.));
        assert_eq!(SidebarPanel::clamp_to_share(px(601.), &sizes), px(600.));
        assert_eq!(SidebarPanel::clamp_to_share(px(900.), &sizes), px(600.));
    }

    /// 上限跟着窗口宽走：总宽 400 → 上限约 240。
    ///
    /// 不与 `px(240.)` 做 `assert_eq!`：`400.0f32 * 0.6f32` 的精确值是 `240.0000153…`
    /// （0.6 在二进制下不精确），把它写死成 `240.` 会得到一条**假红**。这里断言的是
    /// 「落在 60% 上限附近（容差 0.5px 吸收 f32 舍入）且被削过」，与实现是否换算法无关。
    #[test]
    fn clamp_follows_a_narrower_window() {
        let sizes = [px(100.), px(300.)]; // 总宽 400
        let width = SidebarPanel::clamp_to_share(px(280.), &sizes);
        assert!(
            width > px(239.5) && width <= px(240.5),
            "总宽 400 的上限应约 240，实得 {width:?}"
        );
        assert!(width < px(280.), "280 超过上限，必须被削");
    }
}
