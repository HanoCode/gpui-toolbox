//! 弹窗（`gpui-component::Dialog`）的 Duck 侧原语。
//!
//! 两条**上游事实**决定了这里必须有一层封装，否则每个调用点都要各踩一次：
//!
//! 1. **`Dialog` 不会自己渲染按钮。** `button_props` 只登记文案与回调（`render_ok`
//!    / `render_cancel` 是 `pub(crate)`，且唯一调用方是 `AlertDialog`），可见按钮
//!    必须由调用方显式 `.footer(...)` 挂上；不挂的结果是「只有一个标题栏 ✕」的
//!    空壳弹窗。`AlertDialog` 虽然会自动渲染 footer，但它的按钮是库自带 `Button`，
//!    几何走 `h_8()`（26px）、配色走库调色板，与本仓 `density.control` /
//!    `ButtonPalette` 都对不上，所以本仓的弹窗一律用 `Dialog` + 本模块的 footer。
//! 2. **弹窗纵向落点由 `DialogProps::margin_top` 决定**，库默认取
//!    `viewport_height / 10` —— 窗口越高弹窗越靠下，窗口一变大就从「距顶一段」
//!    变成「垂到半空」。本仓改用与窗口尺寸无关的固定基线。
//!
//! 底部操作区只做一件事：把「取消 / 确认」两个按钮与一份校验回调接起来。
//! 校验回调（[`ConfirmHandler`]）返回 `false` 时不关窗，于是**键盘 ⏎ 与鼠标点
//! 主按钮走的是同一个判断**，不会出现「按钮能过、回车不能过」这类分叉。
//!
//! # 弹窗恒居中（2026-10-12 起取消「拖动移动」）
//!
//! 本模块曾让标题行当把手、内容区跟踪指针位移，再每帧以 `.ml(dx)` /
//! `.margin_top(base + dy)` 把位移回灌给面板。问题是**自绘位移与库的绝对定位叠加
//! 会漂移**：面板落点由 `Positioner::corner(Anchor::TopLeft, point(x, y))` 算死
//! （`dialog.rs:658-663`，`x` 恒居中、`y` 只认 `margin_top`），拖动只是在外面再叠
//! 一层 margin，窗口缩放 / 快速拖动 / 多层弹窗并存时，算出来的位置与眼睛看到的位置
//! 对不上。
//!
//! 现改为**恒居中**，不留任何手工位移量：
//!
//! 1. [`modal_drag`]：每弹窗一份的几何状态——只记录「上一帧实测的面板高度 + 窗口
//!    内容区高度」；
//! 2. [`modal_title_row`]：自管标题行（替代 `Dialog::title`，外观完全走本仓 token），
//!    行尾关闭按钮。**不再可拖**——抓手光标 / `on_drag` / 拖拽预览全部去掉；
//! 3. [`modal_drag_area`]：包住弹窗内容，尾部挂一个 `absolute + size_full` 的
//!    **量尺**，量出内容盒高度；面板那部分（内边距 / gap / 边框 / 脚注）由
//!    [`panel_chrome`] 按确定性来源补齐——量尺够不到面板，因为 **GPUI 的绝对定位
//!    贴着直接父级解析**（实测，不是 CSS 的「最近定位祖先」），内容区里元素的直接
//!    父级都还在面板内；
//! 4. [`modal_offset`]：每帧读几何状态，给出把面板垂直居中所需的**补差**，调用点
//!    沿用既有写法 `.ml(offset.x).margin_top(dialog_top_offset(cx) + offset.y)`。
//!
//! ⚠️ **调用点必须 `.close_button(false)`**：库的 `Dialog` 默认还会在面板右上角自画一个
//! ✕（`DialogProps::close_button` 缺省 `true`，`dialog/dialog.rs:736-751` 绝对定位渲染
//! `gpui_base::DialogClose`），与 [`modal_title_row`] 的关闭按钮**叠成两个**（2026-10-12
//! 实机截图确认）。关闭入口统一走 [`modal_title_row`]（走本仓 token + 中文无障碍名），
//! 库那个样式不受控，关掉。
//!
//! ⚠️ **调用点必须 `.opacity(offset.opacity)`**（见 [`ModalOffset::opacity`]）：
//! 高度只能等到首帧 prepaint 才量到，所以「打开后的第一帧」一定是在**不知道落点**的
//! 情况下画的——基线落点（窗口顶部）。这一帧必须整块面板不可见，否则就是
//! 「弹窗先从顶部冒出来、再跳到居中」。`Dialog` 实现 `Styled`，`refine_style`
//! 会把 `opacity` 合进面板本身（`dialog.rs:677`），面板的底 / 描边 / 阴影 / 内容
//! 一起压到 0，这是**唯一**能从外面压住面板的办法（面板的绘制在外面，内容区
//! 无论如何改都不行）。
//!
//! 于是首帧流程是：面板在顶部但**全透明**（量不了内容的可见尺寸就别露脸）→
//! prepaint 量到高度 → [`modal_drag_area`] 要一帧重绘 → 第二帧起按居中落点画，
//! 不透明度交给库的下落 / 淡入动画（`dialog.rs:778-790`，`delta` 从 0 起）。
//!
//! ⚠️ 函数名里的 `drag`（[`modal_drag`] / [`modal_drag_area`]）是**历史遗留**：改名
//! 会波及本文件之外 7 处调用点，其中 `db.rs` / `terminal_dialogs.rs` 当时正被并发
//! 会话编辑，改签名会互相覆盖。于是语义先改成「几何状态 / 测量区」，名字暂留。
//!
//! 焦点陷阱 / Esc / 遮罩仍由 `Dialog` 自管，本模块不重复实现。

use std::rc::Rc;

use duck_theme::{active_theme, tr};
use gpui::{
    AnyElement, App, AppContext as _, Div, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, SharedString, Stateful, Styled as _, Window, div, px,
};
use gpui_component::{Icon, WindowExt as _, dialog::DialogFooter, h_flex, window_paddings};

use crate::{ButtonVariant, DuckIcon, button, title_text, toolbar_action_button};

/// 弹窗的确认动作：返回 `false` 表示校验未通过，弹窗保持打开。
///
/// 装进 `Rc` 是必须的——外层 `open_dialog` 闭包是 `Fn`，泛型闭包参数没法被它
/// 「克隆进内层 `move` 闭包」，只能先变成可克隆的句柄。
pub type ConfirmHandler = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

/// 弹窗里的**无返回值动作**：点击后执行一次，不校验、不关窗。
///
/// 与 [`ConfirmHandler`] 只差返回值。抽成别名不只是为了 [clippy 的 `type_complexity`]，
/// 也为了 [`FooterAction`] 的字段有个可读的名字。
///
/// [clippy 的 `type_complexity`]: https://rust-lang.github.io/rust-clippy/master/index.html#type_complexity
pub type ActionHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// 弹窗顶部的**基线**落点。
///
/// 用 `xxl + lg`（默认 48px）而不是库默认的 `viewport_height / 10`：后者随窗口
/// 高度线性增长，最大化窗口时弹窗会掉到屏幕中段。
///
/// ⚠️ 这只是基线：面板真正的落点是 `dialog_top_offset(cx) + modal_offset(..).y`
/// ——后者是「垂直居中」相对本基线的补差，最终结果由它主导。
#[must_use]
pub fn dialog_top_offset(cx: &App) -> Pixels {
    let spacing = active_theme(cx).tokens().spacing;
    spacing.xxl + spacing.lg
}

/// 弹窗底部操作区：次要「取消」+ 主「确认」。
///
/// `id_prefix` 用来派生两个稳定的 `ElementId`（`{id_prefix}-cancel` /
/// `{id_prefix}-ok`）——一个窗口里可能同时挂着多层弹窗，id 撞车会让焦点与命中测试串台。
/// 因此**不要**用页面下标之类会重复的字符串当 `id_prefix`。
///
/// 确认按钮的语义色由 `ok_variant` 指定：普通表单用 `Primary`，删除类不可逆动作
/// 用 `Destructive`（`Primary` 只留给「决策区里唯一的默认提交」）。
pub fn dialog_footer(
    id_prefix: &'static str,
    ok_text: impl Into<SharedString>,
    ok_variant: ButtonVariant,
    confirm: ConfirmHandler,
    cx: &App,
) -> DialogFooter {
    DialogFooter::new().children(cancel_and_ok(id_prefix, ok_text, ok_variant, confirm, cx))
}

/// 操作区里的一个**旁路动作**：点击后**不关窗**，只执行一次。
///
/// 用来放「测试连接」这类「先验一下，再决定要不要按确认」的入口。它排在操作区
/// **最左**，与「取消 / 确认」之间隔一段空白 —— 它是这一行里唯一不关窗的按钮，
/// 紧挨着确认键会让人误以为「点它等于提交」。
pub struct FooterAction {
    /// 按钮的稳定 `ElementId`：同一窗口里多层弹窗并存时别重名。
    pub id: SharedString,
    /// 按钮文案。
    pub text: SharedString,
    /// 点击回调。装 `Rc` 的原因同 [`ConfirmHandler`]：外层 `open_dialog` 闭包是 `Fn`，
    /// 会被反复调用，普通 `Box<dyn FnOnce>` 活不过第一轮。
    pub on_click: ActionHandler,
}

/// 底部操作区：**左侧旁路动作** + 次要「取消」+ 主「确认」——[`dialog_footer`] 的三按钮形态。
///
/// 旁路动作与确认键之间插一个 `flex_1` 的空盒把两者顶开：`DialogFooter` 自身是
/// `justify_end` 的 `h_flex`（`dialog/footer.rs:51-57`），不撑开的话三个按钮会挤成一组，
/// 「测试连接」就贴在确认键左边了。
pub fn dialog_footer_with_action(
    id_prefix: &'static str,
    action: FooterAction,
    ok_text: impl Into<SharedString>,
    ok_variant: ButtonVariant,
    confirm: ConfirmHandler,
    cx: &App,
) -> DialogFooter {
    DialogFooter::new()
        .child(
            button(action.id, action.text, ButtonVariant::Secondary, cx)
                .on_click(move |_, window, cx| (action.on_click)(window, cx)),
        )
        .child(div().flex_1())
        .children(cancel_and_ok(id_prefix, ok_text, ok_variant, confirm, cx))
}

/// 「取消 + 确认」两按钮，两种 footer 形态共用。
///
/// 确认按钮的语义色由 `ok_variant` 指定：普通表单用 `Primary`，删除类不可逆动作
/// 用 `Destructive`（`Primary` 只留给「决策区里唯一的默认提交」）。
fn cancel_and_ok(
    id_prefix: &'static str,
    ok_text: impl Into<SharedString>,
    ok_variant: ButtonVariant,
    confirm: ConfirmHandler,
    cx: &App,
) -> [AnyElement; 2] {
    [
        button(
            format!("{id_prefix}-cancel"),
            tr(cx, "取消"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(|_, window, cx| window.close_dialog(cx))
        .into_any_element(),
        button(format!("{id_prefix}-ok"), ok_text, ok_variant, cx)
            .on_click(move |_, window, cx| {
                // 校验不过就什么都不做：弹窗留着、错误提示贴在字段上。
                if confirm(window, cx) {
                    window.close_dialog(cx);
                }
            })
            .into_any_element(),
    ]
}

// ── 弹窗几何：恒居中 ─────────────────────────────────────────────────────────

/// 判定「面板高度变了」的阈值（px）。
///
/// prepaint 监听器每帧都会回调，无条件 `window.refresh()` 会自激成死循环；
/// 而亚像素抖动足以让 `!=` 永远为真，所以必须带容差。
const HEIGHT_EPSILON: f32 = 0.5;

/// 库给面板的上下内边距（`gpui-component/src/dialog/dialog.rs:589`
/// `Edges::all(px(16.))`）。本仓 7 处弹窗都没覆盖过 `padding`，故可按常量算。
const PANEL_PADDING: f32 = 16.;
/// 库给面板画的 1px 边框（`dialog/dialog.rs:670` `.border_1()`），上下各一条。
const PANEL_BORDER: f32 = 1.;

/// 面板比「内容盒」多出来的高度（px）。
///
/// 量尺只能量到**内容盒**——GPUI 的绝对定位贴着**直接父级**解析（实测；不是 CSS 的
/// 「最近定位祖先」），所以内容区里的量尺够不到外面那层面板。面板高度 = 内容盒高度 +
/// 本函数，每一项都有确定性来源，不是拍脑袋的魔数：
///
/// | 项 | 来源 |
/// | --- | --- |
/// | 上下内边距 ×2 | 库 `Edges::all(px(16.))` |
/// | 内边距与脚注之间的 gap | 库 `gap(paddings.top.max(px(8.)))` → 16 |
/// | 上下边框 ×2 | 库 `.border_1()` |
/// | 脚注 | `DialogFooter` 自身无内边距，高度就是一行按钮 = `density.control`（`lib.rs` 的 `text_button_frame` 里 `.h(theme.density().control)`） |
///
/// 实测校验（1000×800 窗口、正文 200px）：内容盒 228 + 78 = 306，与面板实测高度一致。
///
/// ⚠️ 前提：① 调用点没有覆盖 `Dialog` 的 padding；② 弹窗挂了一行按钮脚注。本仓 7 处
/// 弹窗都满足；日后若出现无脚注的弹窗，需要在这里减掉 gap + 脚注高。
fn panel_chrome(cx: &App) -> f32 {
    let tokens = active_theme(cx);
    PANEL_PADDING * 3. + PANEL_BORDER * 2. + f32::from(tokens.density().control)
}

/// 一个弹窗的几何状态：上一帧实测的内容盒高度与窗口内容区高度。
///
/// 每个弹窗一份；关闭弹窗后实体随调用方一起丢弃，不跨弹窗记忆。
pub struct ModalDrag {
    /// 面板高度（px，含边框）= 实测内容盒 + [`panel_chrome`]。测量点见 [`modal_drag_area`]。
    panel_height: f32,
    /// 窗口内容区高度（px，已扣掉窗口留白），与库算 `margin_top` 的口径一致。
    content_height: f32,
}

impl ModalDrag {
    /// 高度是否已经量到（两个输入都非零）。
    ///
    /// 与 [`centered_top`] 的守卫是**同一个判据**：那里退回 0 也是因为还没量到。
    /// 落点与显隐绑在一处判断，才不会出现「按旧落点画了、却以为已经量到」。
    fn measured(&self) -> bool {
        self.panel_height > 0. && self.content_height > 0.
    }
}

/// 新建一份弹窗几何状态。
#[must_use]
pub fn modal_drag(cx: &mut App) -> Entity<ModalDrag> {
    cx.new(|_| ModalDrag {
        panel_height: 0.,
        content_height: 0.,
    })
}

/// 弹窗面板的**落点 + 显隐**，由 [`modal_offset`] 每帧算一次。
///
/// 拆成结构体而不是 `Point<Pixels>`：`y` 与 `opacity` 出自同一份几何状态，
/// 调用点若能只取一个（例如只抄 `y`），就会出现「按没量到的落点把面板画出来」。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModalOffset {
    /// 水平补差。库自己居中（`dialog/dialog.rs:583`，`x = (view_size.width - width) / 2`），
    /// 所以**恒为 0**；留着是为了调用点不用改写法。
    pub x: Pixels,
    /// 相对基线 [`dialog_top_offset`] 的垂直补差。
    pub y: Pixels,
    /// 面板不透明度：**高度量到之前是 0**。
    ///
    /// 见模块头：首帧画面板时落点还未知，那一帧必须整块不可见。调用点写成
    /// `.opacity(offset.opacity)`（`Dialog` 实现 `Styled`，会合进面板本身）。
    pub opacity: f32,
}

/// 返回把面板在窗口内容区里**垂直居中**所需的偏移与显隐。
///
/// 调用点沿用既有写法 `.ml(offset.x).margin_top(dialog_top_offset(cx) + offset.y)`，
/// 所以这里给的是「相对基线 [`dialog_top_offset`] 的补差」。
#[must_use]
pub fn modal_offset(drag: &Entity<ModalDrag>, cx: &App) -> ModalOffset {
    let state = drag.read(cx);
    ModalOffset {
        x: px(0.),
        y: centered_top(state.panel_height, state.content_height) - dialog_top_offset(cx),
        opacity: if state.measured() { 1. } else { 0. },
    }
}

/// 面板在窗口内容区里的理想顶部。
///
/// 两个输入都来自**上一帧**的测量：任一为 0（首帧还没测到）就退回 0——面板先落到
/// 窗口顶部。此时调用点拿到的 [`ModalOffset::opacity`] 是 0（面板整块透明），
/// 所以这一帧不会有人看见；下一帧即为准确值。
fn centered_top(panel_height: f32, content_height: f32) -> Pixels {
    if panel_height <= 0. || content_height <= 0. {
        return px(0.);
    }
    // 库把面板钳在 `view_size.height - 2 * lg` 之内，正常永远比窗口矮；万一测到的
    // 比窗口还高，说明量尺落到了错误的包含块上——掐到窗口高度，别把弹窗顶出屏幕。
    let height = panel_height.min(content_height);
    px(((content_height - height) / 2.).max(0.))
}

/// 弹窗标题行：整行是标题，行尾关闭按钮。
///
/// `id` 用来派生关闭按钮的 `ElementId`；多层弹窗并存时不要重名。
///
/// ⚠️ 调用点必须同时给 `Dialog` 加 **`.close_button(false)`**——库默认还会在面板右上角
/// 自画一个 ✕，不加就会出现两个关闭按钮（见模块头）。
///
/// ⚠️ 第二个形参**已不再使用**（原先是拖拽把手的状态）。保留形参只为不动
/// `db.rs` / `terminal_dialogs.rs` 等 7 处调用点——它们当时正被并发会话编辑，
/// 改签名会互相覆盖。测量已移到 [`modal_drag_area`]。
pub fn modal_title_row(
    id: &'static str,
    _geometry: &Entity<ModalDrag>,
    title: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let icon_size = theme.extended().icon.md;
    let close_label = tr(cx, "关闭");

    h_flex()
        .id(id)
        .w_full()
        .flex_none()
        .items_center()
        .justify_between()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.lg)
        .child(div().min_w_0().truncate().child(title_text(title, cx)))
        .child(
            toolbar_action_button(
                format!("{id}-close"),
                close_label,
                Icon::new(DuckIcon::X).size(icon_size),
                false,
                false,
                cx,
            )
            .on_click(|_, window, cx| window.close_dialog(cx)),
        )
}

/// 包住弹窗内容，并顺带量出内容盒高度（据此推算面板高度）。
///
/// 内容区尾部挂一个 `absolute + size_full` 的空盒子当量尺，它的包含块就是这层包装
/// （`div().w_full()`）——于是量尺高度 = 包装高度 = 内容的自然高度。绝对定位元素不参与
/// 父级尺寸计算，所以这层包装完全不改变原有布局。
///
/// ⚠️ 量不到面板本身：GPUI 的绝对定位贴着**直接父级**解析，而内容区里任何元素的直接
/// 父级都在面板**内**。面板那部分高度由 [`panel_chrome`] 按确定性来源补上。
///
/// 量到的高度是「**下一帧**才生效」的：本帧的 render 早于 prepaint，所以首帧一定用
/// 的是零值（见 [`ModalOffset::opacity`]）。因此量到之后必须**主动要一帧**，
/// 否则没人重绘，面板会停在窗口顶部直到别的输入把窗口弄脏。
///
/// 要帧只能走 [`Window::on_next_frame`]：prepaint 阶段直接 `window.refresh()` 是**空操作**
/// ——`Window::refresh` 整个函数体裹在 `invalidator.not_drawing()` 里
/// （`gpui-pre/src/window.rs:2315-2321`），而 prepaint 时 phase 是 `Prepaint`，
/// 于是刷新被静默跳过（曾经就是这么写的，2026-10-12 实机表现为「弹窗先贴在窗口顶部，
/// 鼠标动一下才跳到居中」）。下一帧回调里 phase 已回到 `None`，`refresh()` 才真的
/// 置脏 + 置 `refreshing`，让这一帧绕开 prepaint 缓存、把新落点重算一遍。
///
/// 只在高度真的变了（超过 [`HEIGHT_EPSILON`]）时才要帧，避免自激成每帧重绘。
pub fn modal_drag_area(drag: &Entity<ModalDrag>, child: impl IntoElement) -> Div {
    let drag = drag.clone();
    div().w_full().child(child).child(
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_children_prepainted(move |bounds, window, cx| {
                let Some(probe) = bounds.first() else {
                    return;
                };
                let panel_height = f32::from(probe.size.height) + panel_chrome(cx);
                // 与库算 `margin_top` / `max_height` 的口径对齐：扣掉窗口留白。
                let paddings = window_paddings(window);
                let content_height = f32::from(window.viewport_size().height)
                    - f32::from(paddings.top + paddings.bottom);
                let changed = drag.update(cx, |state, _| {
                    let changed = (state.panel_height - panel_height).abs() > HEIGHT_EPSILON
                        || (state.content_height - content_height).abs() > HEIGHT_EPSILON;
                    state.panel_height = panel_height;
                    state.content_height = content_height;
                    changed
                });
                if changed {
                    window.on_next_frame(|window, _| window.refresh());
                }
            })
            .child(div().size_full()),
    )
}

#[cfg(test)]
mod tests {
    use gpui::px;

    use super::centered_top;

    /// 首帧还没测到高度时退回 0（面板先落窗口顶部），**不能**算成负偏移把面板顶出去。
    ///
    /// 两个输入是分两次填的：窗口尺寸先在首帧 prepaint 拿到、内容盒高度要等同一次测量，
    /// 任一为 0 都必须稳住。
    #[test]
    fn centered_top_gives_up_before_the_first_measurement() {
        assert_eq!(centered_top(0., 0.), px(0.));
        assert_eq!(centered_top(306., 0.), px(0.));
        assert_eq!(centered_top(0., 800.), px(0.));
    }

    /// 常规情况：上下留白相等。用实测过的一组真值（1000×800 窗口、正文 200px）。
    #[test]
    fn centered_top_splits_the_remaining_space_evenly() {
        assert_eq!(centered_top(306., 800.), px(247.));
        // 面板正好与内容区等高 → 没有余量，贴着顶部。
        assert_eq!(centered_top(800., 800.), px(0.));
    }

    /// 面板比窗口还高时掐到 0（贴顶），而不是回一个负的 `margin_top`。
    ///
    /// 这条同时是量尺落错包含块时的保底：万一量到的高度超过窗口，弹窗也不会被顶出屏幕。
    #[test]
    fn centered_top_never_pushes_the_panel_off_screen() {
        assert_eq!(centered_top(1506., 800.), px(0.));
    }
}
