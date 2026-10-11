//! 独立的命令面板浮窗 —— 全局唤起键的落点。
//!
//! 与主窗内的浮层（`ShellView::render_palette`）是**同一个面板的两种挂载方式**：
//! 卡片都来自 `duck_components::command_palette::palette_card`，本模块只管窗口本身
//! —— 无边框、置顶、不可缩放、失焦即收。
//!
//! # 为什么值得单开一个窗口
//!
//! 唤起键是 `⌥Space`，紧挨 `⌘Space`（Spotlight），这个键位自带**启动器**语义：
//! 用户按它期待的是一个盖在当前工作之上的浮层，而不是把 Duck 整个切到前台。原先的
//! 落点（`duck_app::app::summon_main_window`）会把主窗抬出来再叠一个面板 ——
//! 行为没错，但与键位的暗示不符。
//!
//! # 窗口形态（各平台分别是什么）
//!
//! [`WindowKind::PopUp`]：
//!
//! - **macOS** → `NSPanel` 子类 `GPUIPanel`，外加
//!   `NSWindowStyleMaskNonactivatingPanel` + `NSPopUpWindowLevel`(101) +
//!   `CanJoinAllSpaces | FullScreenAuxiliary`。非激活面板**不抢前台**（菜单栏与 Dock
//!   保持原来的应用），但 `GPUIPanel` 把 `canBecomeKeyWindow` 覆写成了 YES，
//!   所以**键盘照样收得到**；这也让「失焦即收」有信号可用（激活态就是 `isKeyWindow`）。
//! - **Windows** → `WS_EX_TOOLWINDOW | WS_EX_TOPMOST` + `dwstyle = 0`：
//!   无边框、置顶、不进任务栏。
//!
//! # 顶部没有「隐形标题栏」吗？
//!
//! `titlebar: None` 时 gpui 的 style mask 仍含 `NSTitledWindowMask`，系统会在窗口顶部
//! 摆一个看不见的标题栏容器。若放任不管（`app_owns_titlebar_drag: false`），它会拦截
//! 顶部约 28pt 的点击——搜索框与右上角关闭按钮都会失灵。修法与主窗自绘标题栏同源：
//! 设 [`WindowOptions::app_owns_titlebar_drag`] = `true`，gpui 向 AppKit 声明整个内容
//! 视图都是应用自有的标题栏内容，点击全部落进 gpui，浮窗自己又不拖动（`is_movable:
//! false`），条带就彻底不存在了。

use std::rc::Rc;

use duck_components::command_palette::{palette_card, palette_footer};
use duck_theme::{active_theme, tr};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement, Pixels,
    Render, SharedString, Size, Styled as _, Subscription, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use gpui_component::{
    IndexPath,
    command::{Command, CommandGroup, CommandState},
    v_flex,
};

use crate::view::{PaletteEmptySource, PaletteGroupsSource};

/// 面板卡片的目标尺寸；也就是浮窗本身的尺寸。
const CARD_SIZE: Size<Pixels> = size(px(620.), px(420.));

/// 浮窗纵向位置：屏高的这个比例处（偏上，贴近 Spotlight 的习惯而不在正中央）。
const VERTICAL_BIAS: f32 = 0.28;

/// 浮窗尺寸 = 卡片尺寸。
fn window_size() -> Size<Pixels> {
    CARD_SIZE
}

/// 浮窗位置：主屏水平居中、纵向在 [`VERTICAL_BIAS`] 处。
///
/// 不做「跟随鼠标所在屏」：唤起键是键盘操作，鼠标可能停在任意位置，跟着鼠标跳反而
/// 让面板出现在难以预料的地方。取主屏是唯一稳定的选择。
#[must_use]
pub fn palette_window_bounds(cx: &App) -> WindowBounds {
    let window_size = window_size();
    let Some(display) = cx.primary_display() else {
        // 拿不到屏幕信息（无头 / 启动极早期）时交给上游按默认规则居中。
        return WindowBounds::centered(window_size, cx);
    };
    let screen = display.bounds();
    WindowBounds::Windowed(Bounds::new(
        point(
            screen.origin.x + (screen.size.width - window_size.width) / 2.,
            screen.origin.y + (screen.size.height - window_size.height) * VERTICAL_BIAS,
        ),
        window_size,
    ))
}

/// 构造浮窗选项。
///
/// 不可缩放、不可最小化、不可拖动：它是一块随按随走的浮层，不是文档窗口。
/// 允许拖动只会制造「被拖到屏幕角落、下次弹出也还在那儿」的困惑。
#[must_use]
pub fn palette_window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(palette_window_bounds(cx)),
        // `None`：不要系统标题栏（面板自带关闭按钮）。
        titlebar: None,
        kind: WindowKind::PopUp,
        is_resizable: false,
        is_minimizable: false,
        is_movable: false,
        // 透明底才能露出卡片自己的圆角与系统阴影。
        window_background: WindowBackgroundAppearance::Transparent,
        // 关键：与主窗自绘标题栏同源（`TitleBar::window_options()` 同款）。不设的话
        // AppKit 会把顶部约 28pt 的隐形标题栏当成系统拖拽区，吃掉搜索框与关闭按钮
        // 的点击；设了之后点击全部落进 gpui 内容——而我们本来就 `is_movable: false`，
        // 不需要系统代管拖动。
        app_owns_titlebar_drag: true,
        focus: true,
        show: true,
        ..WindowOptions::default()
    }
}

/// 关掉浮窗。取消 / 确认 / 右上角关闭按钮三条出口都落在它上面。
///
/// 写成 `fn` 而不是闭包：它不捕获任何东西，而函数项天生是 `Copy`，可以同时喂给
/// `palette_card` 与 `Command::on_cancel` 两处，不必 `.clone()` 来 `.clone()` 去。
fn close_window(window: &mut Window, _: &mut App) {
    window.remove_window();
}

/// 确认（选中条目）后关窗。动作此时已由面板自身派发完毕（见 `Command::on_confirm`）。
fn confirm_and_close(_: IndexPath, window: &mut Window, _: &mut App) {
    window.remove_window();
}

/// 独立浮窗的根视图：只有一张命令面板卡片。
pub struct PaletteWindow {
    state: Entity<CommandState>,
    /// 面板当前查询串。面板的本地过滤是关掉的，命中由 [`Self::groups`] 按它算。
    query: SharedString,
    groups: Option<PaletteGroupsSource>,
    empty: Option<PaletteEmptySource>,
    /// 失焦即收起。首次渲染时才挂 —— `observe_window_activation` 会**立刻**回调一次，
    /// 在构造期挂会拿到一个还没装配完的 `Self`（与 `ShellView` 延迟挂
    /// `observe_window_bounds` 是同一个理由）。
    activation: Option<Subscription>,
    /// 上一次观察到的激活态；只在「由激活转不激活」时收起窗口。
    ///
    /// 不能只看当前值：窗口刚建出来时还不是 key window，直接按「不激活就关」会把
    /// 每一次弹出都立刻关掉。
    active: bool,
}

impl PaletteWindow {
    /// 建浮窗内容。`CommandState` 的构建需要窗口，所以只能在这里建。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| CommandState::new(window, cx));
        Self {
            state,
            query: SharedString::default(),
            groups: None,
            empty: None,
            activation: None,
            active: false,
        }
    }

    /// 注入面板分组数据源（与 `ShellView::with_palette_groups` 同一份实现）。
    #[must_use]
    pub fn with_groups(
        mut self,
        groups: impl Fn(&App, &str) -> Vec<CommandGroup> + 'static,
    ) -> Self {
        self.groups = Some(Rc::new(groups));
        self
    }

    /// 注入空结果态（与 `ShellView::with_palette_empty` 同一份实现）。
    #[must_use]
    pub fn with_empty(
        mut self,
        empty: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        self.empty = Some(Rc::new(empty));
        self
    }

    /// 把键盘焦点交给面板的搜索框。
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.state.update(cx, |state, cx| state.focus(window, cx));
    }

    /// 记录面板查询串（面板的 `on_query` 回调驱动）。
    ///
    /// 查询变了就要重算分组，而分组只在 `render` 里算，所以这里必须 `notify`。
    pub fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.query.as_ref() == query {
            return;
        }
        self.query = SharedString::from(query.to_owned());
        cx.notify();
    }

    /// 收起浮窗（失焦时用）。
    fn dismiss(&mut self, window: &mut Window) {
        window.remove_window();
    }
}

impl Render for PaletteWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.activation.is_none() {
            // 首帧做两件事，都只做一次。
            //
            // ① 焦点交给搜索框：浮窗的全部意义就是「按一下就能打字」。**用 `defer_in`
            //    推到本轮 effect 之后**——渲染期直接改焦点会被 gpui 挡下，而这一刻窗口
            //    视图自己也才刚建出来。顺带清空查询，免得复用实体时留着上一轮的串。
            cx.defer_in(window, |this, window, cx| {
                this.state.update(cx, |state, cx| {
                    state.set_query("", window, cx);
                    state.focus(window, cx);
                });
            });
            // ② 挂激活观察（见 `active` 字段注释：构造期挂会拿到还没装配完的 `Self`，
            //    而且它注册时会**立刻**回调一次）。
            self.activation = Some(cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                let was = std::mem::replace(&mut this.active, active);
                // 只在「本来是激活的、现在不激活了」时收 —— 点别的应用才算失焦。
                if was && !active {
                    this.dismiss(window);
                    cx.notify();
                }
            }));
        }

        let theme = active_theme(cx);
        let colors = theme.tokens().colors;
        let inner_radius = theme.components().dialog_radius - theme.extended().stroke.thin;
        let surface = colors.surface;

        let state = self.state.clone();
        let mut command = Command::new(&state)
            .bordered(false)
            .placeholder(tr(cx, "搜索工具、命令…"))
            // 过滤权交给宿主，理由与主窗浮层一致：上游的匹配是标签 / 关键词的
            // `contains`，撑不起「跳跃命中」与跨来源排序。
            .filterable(false)
            // 上游把列表 `max_h` 默认钉在 18.75rem（约 300px），而卡片（见下）在弹窗里
            // 要撑满整窗 —— 不放开这个帽子，列表到 300px 就截断，卡片下面会露出一条
            // 窗口底色，看起来像「卡片浮在另一层上」。帽子提到窗高即可，实际仍由
            // flex 布局收到「窗高 − 搜索行 − 底条」。
            .max_h(CARD_SIZE.height)
            // 在卡片的定高列里撑满剩余空间（卡片自身 `h_full`，见 `palette_card`）。
            .flex_1()
            .on_query({
                let this = cx.entity().downgrade();
                move |query, _window, cx| {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |this, cx| this.set_query(query, cx));
                    }
                }
            })
            .on_cancel(close_window)
            .on_confirm(confirm_and_close)
            .rounded(inner_radius)
            .bg(surface)
            .overflow_hidden()
            .footer(|state, _, cx| palette_footer(state.matched_count(), cx));
        if let Some(empty) = self.empty.clone() {
            command = command.empty(move |_, window, cx| empty(window, cx));
        }
        let groups = match &self.groups {
            Some(groups) => groups(cx, &self.query),
            None => duck_command::palette_groups(cx, &self.query),
        };
        for group in groups {
            command = command.group(group);
        }

        // 卡片撑满整窗：窗口尺寸就是卡片尺寸（见 `window_size`），顶部不再让位——
        // `app_owns_titlebar_drag` 已把隐形标题栏的点击交还给内容视图。
        v_flex()
            .size_full()
            .child(palette_card(command, close_window, cx))
    }
}
