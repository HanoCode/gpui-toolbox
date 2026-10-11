//! shell 视图：窗口 chrome、活动栏、路由与内容槽位。

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use duck_components::{
    DuckIcon, SidebarPanel, SidebarState, SlidingHighlight, activity_button, color_transition,
    meta_text, nav_icon_color, toolbar_action_button,
};
use duck_theme::{SidebarSide, active_theme, tr};
use gpui::{
    Anchor, AnyElement, AnyView, App, AppContext as _, Bounds, Context, Div, Entity, ExternalPaths,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, MouseButton, ParentElement, Pixels,
    Render, Role, SharedString, StatefulInteractiveElement as _, Styled, Subscription, WeakEntity,
    Window, canvas, div, percentage, point, px, size,
};
use gpui_component::{
    Icon, IndexPath, TitleBar,
    command::{Command, CommandGroup, CommandState},
    h_flex,
    popover::Popover,
    tooltip::Tooltip,
    v_flex,
};

/// 窗口几何变化回调：由应用注入，用于持久化位置 / 尺寸。
pub type BoundsHook = Rc<dyn Fn(&Window, &mut App)>;

/// 面板分组数据源：由应用注入（例：把工具条目并进动作分组）。
///
/// 第二个参数是**当前查询串**——面板的本地过滤是关掉的（过滤权在宿主手里，
/// 见 `render_palette`），命中与排序都由这个数据源自己算。
///
/// `pub(crate)`：独立弹窗（`palette_window`）要用同一份签名，两处各写一遍迟早分叉。
pub(crate) type PaletteGroupsSource = Rc<dyn Fn(&App, &str) -> Vec<CommandGroup>>;

/// 面板空结果态：由应用注入（例：引导去插件市场）。未注入时用上游默认文案。
pub(crate) type PaletteEmptySource = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// 顶栏内可交互元素的包裹：拦截左键按下冒泡，避免触发标题栏的窗口拖拽 / 双击缩放。
/// 包裹本身是 flex 容器：调用方放开收缩时，子元素的 `flex_shrink` / `min_w` 才能生效。
pub fn interactive(child: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_none()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(child)
}

/// 测量父元素左缘的窗口横坐标，变化时写回 `entity` 并重绘（下一帧生效）。
///
/// 父元素须 `relative()`；测量层显式 `inset_0()` 铺满父元素——只写 `size_full()` 的
/// 绝对定位层会落在静态位置（`ElementExt::on_prepaint` 即如此），原点偏离父元素。
pub fn left_edge_probe<T: 'static>(
    entity: WeakEntity<T>,
    get: fn(&T) -> Pixels,
    set: fn(&mut T, Pixels),
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            let left = bounds.origin.x;
            let changed = entity
                .upgrade()
                .is_some_and(|entity| get(entity.read(cx)) != left);
            if !changed {
                return;
            }
            window.defer(cx, move |_, cx| {
                let Ok(()) = entity.update(cx, |entity, cx| {
                    set(entity, left);
                    cx.notify();
                }) else {
                    // 视图已释放，结束这次回调而不再更新状态。
                    return;
                };
            });
        },
        |_, (), _, _| {},
    )
    .absolute()
    .inset_0()
}

/// shell 路由的稳定标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteId(&'static str);

impl RouteId {
    /// 创建稳定路由标识。
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    /// 稳定 id 的原始字符串（持久化 / 与外部清单比对用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// 路由 / 动作的展示文案：渲染时按当前语言解析（语言切换后窗口刷新即换文案）。
type ShellLabel = Rc<dyn Fn(&App) -> SharedString>;

/// 由应用装配并注入 shell 的路由元数据与页面内容。
pub struct ShellRoute {
    id: RouteId,
    button_id: &'static str,
    label: ShellLabel,
    icon: Icon,
    view: AnyView,
    /// 路由活跃时渲染在统一顶栏右侧的插槽。
    title_bar: Option<AnyView>,
    /// 页面内容区左缘（窗口坐标）；顶栏前导插槽按它缩进，与内容对齐。
    content_left: Option<Rc<dyn Fn(&App) -> Pixels>>,
    /// 页面独立侧栏状态；由 shell 统一提供顶栏收起 / 展开入口。
    sidebar: Option<Entity<SidebarState>>,
    /// 路由右侧栏状态：shell 在 [`ShellView::new`] 里保证每条路由都有一个
    /// （未登记时自动建默认态），顶栏开关因此在所有模块常显。
    right_sidebar: Option<Entity<SidebarState>>,
    /// 路由右侧栏内容（每帧求值，可读实时主题 / 语言）；未登记时 shell 给占位文案。
    right_content: Option<RightSidebarContent>,
}
impl ShellRoute {
    /// 创建一条 shell 路由。文案固定时用本构造器。
    pub fn new(
        id: RouteId,
        button_id: &'static str,
        label: impl Into<SharedString>,
        icon: Icon,
        view: impl Into<AnyView>,
    ) -> Self {
        let label = label.into();
        Self {
            id,
            button_id,
            label: Rc::new(move |_| label.clone()),
            icon,
            view: view.into(),
            title_bar: None,
            content_left: None,
            sidebar: None,
            right_sidebar: None,
            right_content: None,
        }
    }

    /// 让路由名随语言解析（`: 概览 / Overview`）。
    #[must_use]
    pub fn with_label_fn(mut self, label: impl Fn(&App) -> SharedString + 'static) -> Self {
        self.label = Rc::new(label);
        self
    }

    /// 为该路由挂载顶栏插槽：路由活跃时 shell 在标题栏右侧渲染它。
    pub fn with_title_bar(mut self, view: impl Into<AnyView>) -> Self {
        self.title_bar = Some(view.into());
        self
    }

    /// 登记页面内容区左缘的来源（页面内测得）；顶栏前导插槽据此与内容对齐。
    /// 未登记（或测得的值为 0）时插槽贴顶栏左缘。
    #[must_use]
    pub fn with_content_left(mut self, read: impl Fn(&App) -> Pixels + 'static) -> Self {
        self.content_left = Some(Rc::new(read));
        self
    }

    /// 为有侧边菜单的路由登记共享侧栏状态。
    pub fn with_sidebar(mut self, sidebar: Entity<SidebarState>) -> Self {
        self.sidebar = Some(sidebar);
        self
    }

    /// 登记路由右侧栏的内容（状态由 shell 统一持有，顶栏开关在所有模块常显）。
    /// 未登记的路由展开后显示 shell 的占位文案。
    pub fn with_right_sidebar(mut self, content: impl Fn(&App) -> AnyElement + 'static) -> Self {
        self.right_content = Some(Rc::new(content));
        self
    }
}

type ShellActionHandler = Rc<dyn Fn(&mut Window, &mut App)>;
/// 动作图标：按当前主题解析，每次渲染重新求值（例：浅色月亮 / 深色太阳）。
type ShellActionIcon = Rc<dyn Fn(&App) -> Icon>;
/// 动作图标颜色：按当前主题解析；未设置时用活动栏统一的弱化前景色。
type ShellActionColor = Rc<dyn Fn(&App) -> Hsla>;
/// 动作弹层内容：按当前主题解析，每次渲染重新求值（例：主题选择器的三列清单）。
type ShellPopoverContent = Rc<dyn Fn(&App) -> AnyElement>;

/// 路由右侧栏内容：按当前主题 / 语言每帧求值（例：工具箱按活跃工具渲染帮助与设置）。
type RightSidebarContent = Rc<dyn Fn(&App) -> AnyElement>;

/// 由应用装配并注入 shell 的窗口级活动栏动作。
pub struct ShellAction {
    button_id: &'static str,
    label: ShellLabel,
    icon: ShellActionIcon,
    icon_color: Option<ShellActionColor>,
    handler: ShellActionHandler,
    /// 点击按钮弹出 Popover 而不是执行 `handler`（例：主题选择器）。
    popover: Option<ShellPopoverContent>,
}

impl ShellAction {
    /// 创建不切换主内容路由的活动栏动作。
    pub fn new(
        button_id: &'static str,
        label: impl Into<SharedString>,
        icon: Icon,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        let label = label.into();
        Self {
            button_id,
            label: Rc::new(move |_| label.clone()),
            icon: Rc::new(move |_| icon.clone()),
            icon_color: None,
            handler: Rc::new(handler),
            popover: None,
        }
    }

    /// 让文案随语言解析（例：`: 切换外观 / Toggle appearance`）。
    #[must_use]
    pub fn with_label_fn(mut self, label: impl Fn(&App) -> SharedString + 'static) -> Self {
        self.label = Rc::new(label);
        self
    }

    /// 让图标随主题解析（例：浅色显示月亮、深色显示太阳）。
    #[must_use]
    pub fn with_icon_fn(mut self, icon: impl Fn(&App) -> Icon + 'static) -> Self {
        self.icon = Rc::new(icon);
        self
    }

    /// 覆盖图标颜色，同样随主题解析。
    #[must_use]
    pub fn with_icon_color(mut self, color: impl Fn(&App) -> Hsla + 'static) -> Self {
        self.icon_color = Some(Rc::new(color));
        self
    }

    /// 点击按钮时在按钮旁弹出 Popover（内容随主题每帧求值），不再执行动作处理器。
    #[must_use]
    pub fn with_popover(mut self, content: impl Fn(&App) -> AnyElement + 'static) -> Self {
        self.popover = Some(Rc::new(content));
        self
    }
}

/// 活动栏图标比 `extended.icon.lg` 大的余量。
///
/// 轨宽 / 按钮位行高 / 按钮边长都不在这里：它们是**密度 token**
/// （`density().activity_rail` / `activity_tile` / `activity_button`），随「界面密度」
/// 与「界面缩放」两轴变化。此余量是图标相对 `icon.lg` 的溢出量，不是布局尺寸，
/// 故仍作常量。
const ACTIVITY_ICON_EXTRA: Pixels = px(2.);

/// shell 兜底的右侧栏默认宽度与可拖拽范围（路由未登记内容时使用）。
const DEFAULT_RIGHT_SIDEBAR_WIDTH: Pixels = px(300.);
const DEFAULT_RIGHT_SIDEBAR_RANGE: Range<Pixels> = px(260.)..px(400.);

/// 顶栏右侧插槽：`&App → 元素`，每帧求值（见 [`ShellView::with_title_bar_trailing`]）。
type TrailingSource = Rc<dyn Fn(&App) -> AnyElement>;

/// GPUI 窗口外壳：只负责窗口 chrome、活动栏、路由与内容槽位。
pub struct ShellView {
    active_route: Option<RouteId>,
    routes: Vec<ShellRoute>,
    actions: Vec<ShellAction>,
    /// 所有路由共用的顶栏前导插槽（例：全局搜索框）。
    title_bar_leading: Option<AnyView>,
    /// 所有路由共用的顶栏尾随插槽（例：语言切换），渲染在最右侧。
    ///
    /// 是闭包而不是元素：它每帧求值，所以里面的控件能读到**实时**外观状态
    /// （下拉的勾选位随当前语言走），不会定格在 shell 构建那一刻。
    title_bar_trailing: Option<TrailingSource>,
    /// 前导插槽自身左缘的窗口横坐标（上一帧 prepaint 测得），用于算前导留白。
    title_slot_left: Pixels,
    /// 已应用到 macOS 交通灯的纵向偏移；标题栏随文字放大撑高时重新居中。
    traffic_light_y: Pixels,
    /// 命令面板状态；首次打开时才创建（`CommandState` 构建需要窗口）。
    palette: Option<Entity<CommandState>>,
    /// 面板当前是否展开。
    palette_open: bool,
    /// 面板分组数据源（由应用注入）；未注入时回落到动作注册表投影。
    palette_groups: Option<PaletteGroupsSource>,
    /// 面板空结果态（由应用注入）；未注入时用上游默认文案。
    palette_empty: Option<PaletteEmptySource>,
    /// 面板当前查询串。面板的本地过滤关着，命中由 [`Self::palette_groups`] 按它算，
    /// 所以这里必须留一份——上游只管把查询回报过来。
    palette_query: SharedString,
    /// 窗口几何变化回调（由应用注入，用于持久化位置 / 尺寸）。
    bounds_hook: Option<BoundsHook>,
    /// 活跃路由变化回调（由应用注入，用于持久化会话）。
    ///
    /// 只回报**结果**（稳定 id / 折叠态），不让回调反过来读 shell —— 钩子在
    /// `Context<Self>` 借用期间触发，回调里再 `update` 同一个实体会重入 panic。
    route_hook: Option<RouteHook>,
    /// 活跃路由**右栏**折叠态变化回调（由应用注入，用于持久化会话）。
    right_sidebar_hook: Option<RightSidebarHook>,
    /// 外部文件拖放回调（由应用注入）。
    ///
    /// shell 不认识「一个文件能拿去做什么」——那是应用的事。它只负责把平台事件翻成
    /// 「一批路径」，外加拖拽悬停时的视觉反馈。
    files_drop_hook: Option<FilesDropHook>,
    /// 几何观察订阅；首次渲染时建立一次。
    bounds_subscription: Option<Subscription>,
}

/// 活跃路由变化：`&'static str` 是路由稳定 id。
type RouteHook = Rc<dyn Fn(&'static str, &mut App)>;
/// 活跃路由右栏折叠态变化：`bool` 为「是否展开」。
type RightSidebarHook = Rc<dyn Fn(bool, &mut App)>;
/// 外部文件落入窗口：参数是这一批被拖入的路径。
pub type FilesDropHook = Rc<dyn Fn(&[PathBuf], &mut Window, &mut App)>;

impl ShellView {
    /// 创建 shell，并使用传入顺序的首条路由作为初始页面。
    ///
    /// 每条路由都保证有右侧栏状态（未登记内容的路由用默认态兜底），
    /// 顶栏右侧栏开关因此在所有模块常显、点击都有响应。
    pub fn new(routes: Vec<ShellRoute>, actions: Vec<ShellAction>, cx: &mut Context<Self>) -> Self {
        let active_route = routes.first().map(|route| route.id);
        let routes: Vec<ShellRoute> = routes
            .into_iter()
            .map(|mut route| {
                if route.right_sidebar.is_none() {
                    route.right_sidebar = Some(cx.new(|cx| {
                        let mut state = SidebarState::new(
                            DEFAULT_RIGHT_SIDEBAR_WIDTH,
                            DEFAULT_RIGHT_SIDEBAR_RANGE.clone(),
                            cx,
                        );
                        state.set_collapsed(true, cx);
                        state
                    }));
                }
                route
            })
            .collect();
        for route in &routes {
            let route_id = route.id;
            // 左侧栏：只负责让活跃路由重绘（它的折叠态是会话内状态，不跨重启）。
            if let Some(sidebar) = &route.sidebar {
                cx.observe(sidebar, move |this, _, cx| {
                    if this.active_route == Some(route_id) {
                        cx.notify();
                    }
                })
                .detach();
            }
            // 右侧栏：除重绘外还要回报折叠态 —— 它要跨重启保留（见 `on_right_sidebar_changed`）。
            if let Some(right) = &route.right_sidebar {
                cx.observe(right, move |this, right, cx| {
                    if this.active_route != Some(route_id) {
                        return;
                    }
                    cx.notify();
                    let open = !right.read(cx).is_collapsed();
                    if let Some(hook) = this.right_sidebar_hook.clone() {
                        hook(open, cx);
                    }
                })
                .detach();
            }
        }
        Self {
            active_route,
            routes,
            actions,
            title_bar_leading: None,
            title_bar_trailing: None,
            title_slot_left: px(0.),
            traffic_light_y: crate::initial_traffic_light_y(),
            palette: None,
            palette_open: false,
            palette_groups: None,
            palette_empty: None,
            palette_query: SharedString::default(),
            bounds_hook: None,
            route_hook: None,
            right_sidebar_hook: None,
            files_drop_hook: None,
            bounds_subscription: None,
        }
    }

    /// 挂载「窗口几何变化」回调（用于持久化位置 / 尺寸）。
    #[must_use]
    pub fn on_window_bounds_changed(mut self, hook: impl Fn(&Window, &mut App) + 'static) -> Self {
        self.bounds_hook = Some(Rc::new(hook));
        self
    }

    /// 指定首屏路由（会话恢复用）。
    ///
    /// **未知 id 一律回落到第一条路由**：存档里的路由可能来自旧版本（模块被删或改名），
    /// 那时打开的应该是一个能用的窗口，而不是空白页或 panic。
    #[must_use]
    pub fn with_initial_route(mut self, id: Option<&'static str>) -> Self {
        let restored = id.and_then(|id| {
            let wanted = RouteId::new(id);
            self.routes
                .iter()
                .find(|route| route.id == wanted)
                .map(|route| route.id)
        });
        if let Some(route) = restored {
            self.active_route = Some(route);
        }
        self
    }

    /// 挂载「活跃路由变化」回调（用于持久化会话）。
    #[must_use]
    pub fn on_route_changed(mut self, hook: impl Fn(&'static str, &mut App) + 'static) -> Self {
        self.route_hook = Some(Rc::new(hook));
        self
    }

    /// 挂载「活跃路由右栏折叠态变化」回调（用于持久化会话）。
    ///
    /// 只回报活跃路由的右栏：非活跃路由的右栏在窗口关闭时即丢弃，没有保留价值。
    #[must_use]
    pub fn on_right_sidebar_changed(mut self, hook: impl Fn(bool, &mut App) + 'static) -> Self {
        self.right_sidebar_hook = Some(Rc::new(hook));
        self
    }

    /// 接住拖进窗口的外部文件。
    ///
    /// 注入后才开启拖放：没注入时窗口完全不受理文件拖拽，**也不会**出现拖拽悬停的底色
    /// ——给一个「能拖但拖了没反应」的视觉承诺比不支持更糟。
    ///
    /// 平台把文件拖放翻成了一次以 [`ExternalPaths`] 为负载的内部拖拽
    /// （见 `Window::dispatch_event`），所以这里用 `drag_over` / `on_drop` 就够，
    /// 不需要自己维护「正在拖拽」的状态。
    #[must_use]
    pub fn on_files_dropped(
        mut self,
        hook: impl Fn(&[PathBuf], &mut Window, &mut App) + 'static,
    ) -> Self {
        self.files_drop_hook = Some(Rc::new(hook));
        self
    }

    /// 挂载所有路由共用的顶栏前导插槽（渲染在侧栏开关之后、路由插槽之前）。
    /// 路由切换不会重建它，所以插槽内的状态（例：搜索查询）跨页面保留。
    #[must_use]
    pub fn with_title_bar_leading(mut self, view: impl Into<AnyView>) -> Self {
        self.title_bar_leading = Some(view.into());
        self
    }

    /// 挂载所有路由共用的顶栏右侧插槽（渲染在活跃路由插槽之后，贴窗口右缘）。
    /// 路由切换不会重建它；传闭包是为了里面的控件每帧读实时状态。
    #[must_use]
    pub fn with_title_bar_trailing(
        mut self,
        trailing: impl Fn(&App) -> AnyElement + 'static,
    ) -> Self {
        self.title_bar_trailing = Some(Rc::new(trailing));
        self
    }

    /// 注入面板分组数据源（例：应用层把工具条目并进动作分组）。
    ///
    /// 回调收到当前查询串；面板的本地过滤是关掉的，命中与排序都归它算。
    /// 未注入时面板只展示动作注册表里的条目（同样按查询串过滤）。
    #[must_use]
    pub fn with_palette_groups(
        mut self,
        groups: impl Fn(&App, &str) -> Vec<CommandGroup> + 'static,
    ) -> Self {
        self.palette_groups = Some(Rc::new(groups));
        self
    }

    /// 注入面板的空结果态（例：引导去插件市场）。
    /// 未注入时用上游默认的「没有匹配项」文案。
    #[must_use]
    pub fn with_palette_empty(
        mut self,
        empty: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        self.palette_empty = Some(Rc::new(empty));
        self
    }

    /// 切换到指定路由（宿主导航入口）。
    pub fn navigate(&mut self, route: RouteId, cx: &mut Context<Self>) {
        if self.active_route != Some(route) {
            self.active_route = Some(route);
            // 持久化回调先于 `cx.notify()`：钩子只写存储、不回读 shell（见字段注释）。
            if let Some(hook) = self.route_hook.clone() {
                hook(route.as_str(), cx);
            }
            cx.notify();
        }
    }

    /// 按稳定 id 切换路由（动作 / 菜单用：`navigate_by_id("toolbox", cx)`）。
    pub fn navigate_by_id(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.navigate(RouteId::new(id), cx);
    }

    /// 当前活跃路由的稳定 id；没有任何路由时为 `None`。
    #[must_use]
    pub fn active_route_id(&self) -> Option<&'static str> {
        self.active_route.map(RouteId::as_str)
    }

    /// 设置活跃路由右栏的折叠态（会话恢复用）；没有活跃路由时什么都不做。
    pub fn set_active_right_sidebar_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let Some(state) = self
            .active_route
            .and_then(|id| self.routes.iter().find(|route| route.id == id))
            .and_then(|route| route.right_sidebar.clone())
        else {
            return;
        };
        state.update(cx, |state, cx| state.set_collapsed(collapsed, cx));
    }

    /// 收起 / 展开当前路由的侧栏；该路由没有侧栏时什么都不做。
    pub fn toggle_active_sidebar(&mut self, cx: &mut Context<Self>) {
        if let Some(sidebar) = self.active_sidebar() {
            sidebar.update(cx, |state, cx| state.toggle(cx));
        }
    }

    /// 收起 / 展开当前路由的右侧栏（每条路由都有状态，点击必有响应）。
    pub fn toggle_active_right_sidebar(&mut self, cx: &mut Context<Self>) {
        self.active_right_sidebar()
            .update(cx, |state, cx| state.toggle(cx));
    }

    /// 当前活跃路由的侧栏状态。
    fn active_sidebar(&self) -> Option<Entity<SidebarState>> {
        self.active_route
            .and_then(|id| self.routes.iter().find(|route| route.id == id))
            .and_then(|route| route.sidebar.clone())
    }

    /// 当前活跃路由的右侧栏状态（每条路由都保证有）。
    fn active_right_sidebar(&self) -> Entity<SidebarState> {
        self.active_route
            .and_then(|id| self.routes.iter().find(|route| route.id == id))
            .and_then(|route| route.right_sidebar.clone())
            .expect("every route has a right sidebar state")
    }

    /// 打开 / 关闭命令面板（由动作处理器调用）。
    pub fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.set_palette_open(false, window, cx);
            return;
        }
        self.open_palette(window, cx);
    }

    /// 打开命令面板；已经开着时**保持打开**，只把查询清空、焦点交回搜索框。
    ///
    /// 与 [`Self::toggle_palette`] 分开是因为调用方的意图不同：命令面板的取消 /
    /// 确认回调、以及应用内键位走的是「切换」，而全局唤起键走的是「打开」——
    /// 用户按下唤起键时想要的永远是「现在开始打字」，不是「把我上次开着的面板关掉」。
    pub fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = match self.palette.clone() {
            Some(state) => state,
            None => {
                let state = cx.new(|cx| CommandState::new(window, cx));
                self.palette = Some(state.clone());
                state
            }
        };
        // 每次打开都清空上次的查询，并把焦点交给面板的搜索框。
        state.update(cx, |state, cx| {
            state.set_query("", window, cx);
            state.focus(window, cx);
        });
        // 上游的 `set_query` 是程序化写入，未必回报给 `on_query`；这里显式归零，
        // 免得上一轮的过滤结果留在 `palette_query` 里。
        self.palette_query = SharedString::default();
        if !self.palette_open {
            self.palette_open = true;
            cx.notify();
        }
    }

    /// 记录面板查询串（面板的 `on_query` 回调驱动）。
    ///
    /// 查询变了就要重算分组，而分组只在 `render` 里算，所以这里必须 `notify`。
    pub fn set_palette_query(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.palette_query.as_ref() == query {
            return;
        }
        self.palette_query = SharedString::from(query.to_owned());
        cx.notify();
    }

    /// 设置面板开合状态（面板自身的取消 / 确认回调调用）。
    pub fn set_palette_open(&mut self, open: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open != open {
            self.palette_open = open;
            cx.notify();
        }
    }

    /// 活动栏按钮位尺寸（自绘动作视图按此尺寸绘制以与图标按钮对齐）。
    ///
    /// 取自密度 token，故不再是 `const`：它随「界面密度」与「界面缩放」两轴变化。
    #[must_use]
    pub fn action_button_size(cx: &App) -> Pixels {
        active_theme(cx).density().activity_button
    }

    /// 统一顶栏：macOS 交通灯 / 其他平台品牌区 → 侧栏开关 → 全局前导插槽（搜索框）
    /// → 活跃路由的插槽。
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let spacing = theme.tokens().spacing;
        let extended = theme.extended().colors;
        let is_macos = cfg!(target_os = "macos");
        let branch_color = theme.tokens().colors.muted_foreground;
        let leading = (!is_macos).then(|| {
            h_flex()
                .h_full()
                .flex_none()
                .items_center()
                .gap(spacing.sm)
                .pl(spacing.md)
                .child(
                    Icon::new(DuckIcon::Box)
                        .size(theme.extended().icon.lg)
                        .text_color(branch_color),
                )
                .child(
                    div()
                        .text_size(theme.tokens().typography.sm.size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.tokens().colors.foreground)
                        .child(crate::MAIN_WINDOW_TITLE),
                )
        });
        let slot: Option<AnyElement> = self.active_title_bar().map(|view| view.into_any_element());
        // 前导留白：前导插槽左缘 = 活跃页内容区左缘 + 一档留白（由容器 flex gap 提供）；
        // 页面未登记内容左缘 / 顶栏前导区（交通灯、品牌区、侧栏开关）更宽时贴插槽左缘。
        let content_left = self.active_content_left(cx);
        let leading_inset = if content_left > px(0.) {
            (content_left - self.title_slot_left).max(px(0.))
        } else {
            px(0.)
        };
        let sidebar_toggle = self
            .active_sidebar()
            .as_ref()
            .filter(|sidebar| sidebar.read(cx).is_available())
            .map(|sidebar| self.render_sidebar_toggle(sidebar, cx));
        // 右侧栏开关挂在尾随插槽（语言切换等）之后，贴窗口右缘；每条路由都有
        // 右侧栏状态，开关在所有模块常显。
        let right_sidebar_toggle = Some(self.render_right_sidebar_toggle(cx));
        let title_bar = TitleBar::new();
        #[cfg(not(target_os = "macos"))]
        let title_bar = title_bar.pl(spacing.sm);
        let probe = left_edge_probe(
            cx.entity().downgrade(),
            |view| view.title_slot_left,
            |view, left| view.title_slot_left = left,
        );

        title_bar
            .h(crate::title_bar_height(cx))
            .bg(extended.chrome)
            .border_color(extended.hairline)
            .child(
                // 绝对定位铺满拖动区：标题行不参与拖动区的内容宽度，窗口控制按钮不会被挤出。
                h_flex()
                    .absolute()
                    .inset_0()
                    .items_center()
                    .gap(spacing.sm)
                    .pr(if is_macos { spacing.md } else { spacing.sm })
                    .children(leading)
                    .children(sidebar_toggle)
                    .child(
                        h_flex()
                            .relative()
                            .h_full()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap(spacing.sm)
                            // 前导留白远先于前导插槽收缩（高收缩权重）；空白处保持窗口拖拽。
                            .child(div().flex_shrink(100.).min_w_0().w(leading_inset))
                            .children(self.title_bar_leading.clone())
                            .child(div().flex_1().min_w_0())
                            .children(slot)
                            .children(
                                self.title_bar_trailing
                                    .as_ref()
                                    .map(|trailing| trailing(cx)),
                            )
                            // 通知中心铃铛：尾随插槽（语言切换等）之后、右侧栏开关之前。
                            // 可在「设置 → 外观 → 界面」关掉；关掉后历史仍照常记录，
                            // 只是顶栏不再有入口。
                            .children(
                                active_theme(cx)
                                    .appearance()
                                    .show_notification_bell
                                    .then(|| crate::notifications::bell(cx)),
                            )
                            .children(right_sidebar_toggle)
                            .child(probe),
                    ),
            )
    }

    /// 当前活跃路由登记的内容区左缘（窗口坐标）；未登记时为 0。
    fn active_content_left(&self, cx: &App) -> Pixels {
        self.active_route
            .and_then(|active| self.routes.iter().find(|route| route.id == active))
            .and_then(|route| route.content_left.as_ref())
            .map_or(px(0.), |read| read(cx))
    }

    fn render_sidebar_toggle(
        &self,
        sidebar: &Entity<SidebarState>,
        cx: &App,
    ) -> impl IntoElement + use<> {
        let label = if sidebar.read(cx).is_collapsed() {
            SharedString::from(tr(cx, "展开侧栏"))
        } else {
            SharedString::from(tr(cx, "收起侧栏"))
        };
        let state = sidebar.clone();
        div()
            .id("shell-sidebar-toggle-tooltip")
            .flex_none()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .tooltip({
                let label = label.clone();
                move |window, cx| Tooltip::new(label.clone()).build(window, cx)
            })
            .child(
                toolbar_action_button(
                    "shell-sidebar-toggle",
                    label,
                    Icon::new(DuckIcon::PanelRight)
                        .size(active_theme(cx).extended().icon.md)
                        .rotate(percentage(0.5)),
                    false,
                    false,
                    cx,
                )
                .on_click(move |_, _, cx| state.update(cx, |state, cx| state.toggle(cx))),
            )
    }

    /// 右侧栏开关：与左侧开关同构，图标用原方向（缺口朝右），挂在尾随插槽之后。
    fn render_right_sidebar_toggle(&self, cx: &App) -> impl IntoElement + use<> {
        let sidebar = self.active_right_sidebar();
        let label = if sidebar.read(cx).is_collapsed() {
            SharedString::from(tr(cx, "展开右侧栏"))
        } else {
            SharedString::from(tr(cx, "收起右侧栏"))
        };
        div()
            .id("shell-right-sidebar-toggle-tooltip")
            .flex_none()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .tooltip({
                let label = label.clone();
                move |window, cx| Tooltip::new(label.clone()).build(window, cx)
            })
            .child(
                toolbar_action_button(
                    "shell-right-sidebar-toggle",
                    label,
                    Icon::new(DuckIcon::PanelRight).size(active_theme(cx).extended().icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    sidebar.update(cx, |state, cx| state.toggle(cx));
                }),
            )
    }

    /// 当前活跃路由挂载的顶栏插槽。
    fn active_title_bar(&self) -> Option<AnyView> {
        self.active_route
            .and_then(|active| self.routes.iter().find(|route| route.id == active))
            .and_then(|route| route.title_bar.clone())
    }

    fn route_button(
        &self,
        route: &ShellRoute,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.active_route == Some(route.id);
        let theme = active_theme(cx);
        let icon_size = theme.extended().icon.lg + ACTIVITY_ICON_EXTRA;
        let rail_width = theme.density().activity_rail;
        let tile_height = theme.density().activity_tile;
        let button_size = theme.density().activity_button;
        let icon_color =
            color_transition(route.button_id, nav_icon_color(selected, cx), window, cx);
        let label = (route.label)(cx);
        let tooltip_label = label.clone();
        let route_id = route.id;

        div()
            .id(route.button_id)
            .w(rail_width)
            .h(tile_height)
            .flex()
            .items_center()
            .justify_center()
            .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
            .child(
                activity_button(
                    route.button_id,
                    label,
                    route.icon.clone().size(icon_size).text_color(icon_color),
                    Role::Tab,
                    selected,
                    button_size,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.active_route != Some(route_id) {
                        this.active_route = Some(route_id);
                        cx.notify();
                    }
                })),
            )
            .into_any_element()
    }

    /// 路由按钮列：选中底是一块随选中项滑动的底块（按钮自身不画选中底）。
    fn route_buttons(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let gap = theme.tokens().spacing.xs;
        let radius = theme.components().nav_item_radius;
        let selected_color = theme.extended().colors.nav_selected;
        // 活动栏三个尺寸取自密度 token（Pixels 是 Copy，取值后即可放开对 cx 的借用）。
        let rail_width = theme.density().activity_rail;
        let button_size = theme.density().activity_button;
        let tile_height = theme.density().activity_tile;
        let highlight = self
            .routes
            .iter()
            .position(|route| self.active_route == Some(route.id))
            .map(|index| {
                let inset = (rail_width - button_size) / 2.;
                let top = (tile_height + gap) * index as f32 + (tile_height - button_size) / 2.;
                SlidingHighlight::new(
                    "shell-activity-selection",
                    Bounds::new(point(inset, top), size(button_size, button_size)),
                    selected_color,
                )
                .radius(radius)
            });
        let buttons: Vec<AnyElement> = self
            .routes
            .iter()
            .map(|route| self.route_button(route, window, cx))
            .collect();
        v_flex()
            .id("shell-activity-routes")
            // 路由按钮按 `Role::Tab` 报（见 `activity_button`），容器就应当是 `TabList`：
            // 没有它，读屏只会挨个念图标名，不会播报「共 N 项、第 M 项」。
            // 装饰性的滑动底块没有 role，不会进 a11y 树。
            .role(Role::TabList)
            .relative()
            .gap(gap)
            .children(highlight)
            .children(buttons)
    }

    fn action_button(&self, action: &ShellAction, cx: &mut Context<Self>) -> AnyElement {
        let label = (action.label)(cx);
        let tooltip_label = label.clone();
        let theme = active_theme(cx);
        let icon_size = theme.extended().icon.lg + ACTIVITY_ICON_EXTRA;
        let rail_width = theme.density().activity_rail;
        let tile_height = theme.density().activity_tile;
        let button_size = theme.density().activity_button;
        let icon_color = action
            .icon_color
            .as_ref()
            .map_or_else(|| theme.tokens().colors.muted_foreground, |color| color(cx));
        let button_id = action.button_id;
        let icon = (action.icon)(cx);
        let button = activity_button(
            button_id,
            label,
            icon.size(icon_size).text_color(icon_color),
            // 活动栏的命令按钮是**动作**不是标签页，角色取 `Button`（容器是 `Toolbar`）。
            Role::Button,
            false,
            button_size,
            cx,
        );
        // 弹层动作：点击由 Popover 接管（左键开合），不再走动作处理器。
        let child = match &action.popover {
            Some(content) => {
                // BasePopover 的状态按 id 键控在外层 div 之外，别与按钮位 id 撞名。
                let popover_id = SharedString::from(format!("{}-popover", button_id));
                Popover::new(popover_id)
                    .anchor(Anchor::LeftCenter)
                    .offset(px(6.))
                    .trigger(button)
                    .child(content(cx))
                    .into_any_element()
            }
            None => {
                let handler = Rc::clone(&action.handler);
                button
                    .on_click(move |_, window, cx| handler(window, cx))
                    .into_any_element()
            }
        };

        div()
            .w(rail_width)
            .h(tile_height)
            .flex()
            .items_center()
            .justify_center()
            .id(button_id)
            .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
            .child(child)
            .into_any_element()
    }

    fn action_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let gap = active_theme(cx).tokens().spacing.xs;
        v_flex()
            .id("shell-activity-actions")
            // 活动栏下部的命令按钮（插件中心 / 主题 / 设置…）：一组命令 → `Toolbar`，
            // 子项按 `Role::Button` 报（见 `action_button`）。
            .role(Role::Toolbar)
            .gap(gap)
            .children(
                self.actions
                    .iter()
                    .map(|action| self.action_button(action, cx)),
            )
    }

    /// 是否存在至少一个路由或动作；活动栏仅在此为真时渲染。
    fn has_activity_items(&self) -> bool {
        !self.routes.is_empty() || !self.actions.is_empty()
    }

    fn render_activity_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<Div> {
        // 用户可在「设置 → 外观 → 界面」关掉整条活动栏（功能轨）；关掉后内容区占满。
        if !self.has_activity_items() || !active_theme(cx).appearance().show_activity_bar {
            return None;
        }
        let theme = active_theme(cx);
        let spacing = theme.tokens().spacing;
        let extended = theme.extended();
        let rail_width = theme.density().activity_rail;
        Some(
            v_flex()
                .h_full()
                .w(rail_width)
                .flex_none()
                .justify_between()
                .bg(extended.colors.chrome)
                .border_r(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .pt(spacing.sm)
                .pb(spacing.sm)
                .child(self.route_buttons(window, cx))
                .child(self.action_buttons(cx)),
        )
    }

    fn active_content(&self) -> AnyElement {
        self.active_route
            .and_then(|active| self.routes.iter().find(|route| route.id == active))
            .map_or_else(
                || div().into_any_element(),
                |route| route.view.clone().into_any_element(),
            )
    }

    /// 命令面板浮层：展开时覆盖整个窗口（在根 flex 里排最后，绘制在最上层）。
    fn render_palette(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.palette_open {
            return None;
        }
        let state = self.palette.clone()?;
        let weak = cx.entity().downgrade();

        // 遮罩点击 → 关闭。
        let dismiss = {
            let weak = weak.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some(shell) = weak.upgrade() {
                    shell.update(cx, |shell, cx| shell.set_palette_open(false, window, cx));
                }
            }
        };
        // 确认（动作已由面板派发）→ 关闭。
        let confirmed = {
            let weak = weak.clone();
            move |_: IndexPath, window: &mut Window, cx: &mut App| {
                if let Some(shell) = weak.upgrade() {
                    shell.update(cx, |shell, cx| shell.set_palette_open(false, window, cx));
                }
            }
        };

        // 圆角与底色直接铺在 Command 自身：gpui-fast 的溢出裁剪不贴圆角，
        // Command 内部的方形底色会盖掉外层卡片的圆角；底色统一用 surface，
        // 避免 gpui-component 默认 popover 色和本主题串色。半径按卡片边框
        // 宽度内收，避免外内两层圆角叠出厚边。
        let theme = active_theme(cx);
        let inner_radius = theme.components().dialog_radius - theme.extended().stroke.thin;
        let surface = theme.tokens().colors.surface;

        let mut command = Command::new(&state)
            .bordered(false)
            .placeholder(tr(cx, "搜索工具、命令…"))
            // 过滤权交给宿主：上游的匹配是标签 / 关键词的 `contains`，撑不起
            // 「跳跃命中」和跨来源排序。关掉本地过滤后上游把所有条目原样留着，
            // 查询照旧从 `on_query` 回报，高亮复位到第一条（也就是我们排的第一名）。
            .filterable(false)
            .on_query({
                let weak = weak.clone();
                move |query, _window, cx| {
                    if let Some(shell) = weak.upgrade() {
                        shell.update(cx, |shell, cx| shell.set_palette_query(query, cx));
                    }
                }
            })
            .on_cancel(dismiss.clone())
            .on_confirm(confirmed)
            .rounded(inner_radius)
            .bg(surface)
            .overflow_hidden()
            .footer(|state, _, cx| {
                duck_components::command_palette::palette_footer(state.matched_count(), cx)
            });
        if let Some(empty) = self.palette_empty.clone() {
            command = command.empty(move |_, window, cx| empty(window, cx));
        }
        let groups = match &self.palette_groups {
            Some(groups) => groups(cx, &self.palette_query),
            None => duck_command::palette_groups(cx, &self.palette_query),
        };
        for group in groups {
            command = command.group(group);
        }

        Some(duck_components::command_palette::palette_frame(
            command, dismiss, cx,
        ))
    }
}

impl Render for ShellView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 首次渲染时建立窗口几何观察（`observe_window_bounds` 需要窗口）。
        if self.bounds_subscription.is_none()
            && let Some(hook) = self.bounds_hook.clone()
        {
            self.bounds_subscription =
                Some(cx.observe_window_bounds(window, move |_shell, window, cx| hook(window, cx)));
        }
        crate::sync_traffic_light(
            window,
            crate::title_bar_height(cx),
            &mut self.traffic_light_y,
        );
        let colors = active_theme(cx).tokens().colors;
        let mut root = v_flex()
            .size_full()
            .relative()
            .bg(colors.background)
            .text_color(colors.foreground);

        // 文件拖放挂在根容器上：整片窗口都是落点，用户不必瞄准某个控件。
        // 拖拽悬停铺一层强调色薄底（与悬停同一族语义，用 `hover_tint`），
        // 放下时把路径交出去——`drag_over` 只改样式、`on_drop` 只收路径，都不带状态。
        if let Some(hook) = self.files_drop_hook.clone() {
            let tint = active_theme(cx).extended().colors.hover_tint;
            root = root
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(tint))
                .on_drop(move |paths: &ExternalPaths, window, cx| {
                    hook(paths.paths(), window, cx);
                });
        }

        root.child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .children(self.render_activity_bar(window, cx))
                    .child(self.render_route_area(window, cx)),
            )
            .children(self.render_palette(cx))
    }
}

impl ShellView {
    /// 活跃路由内容区 + 侧栏：侧栏由 shell 统一布局（全高，宽度可拖拽），停靠侧由
    /// 「设置 → 外观 → 布局 → 右栏位置」决定（左 / 右）。每条路由都有自己的状态，
    /// 未登记内容的路由展开显示占位文案。
    fn render_route_area(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let content = self.active_content();
        let state = self.active_right_sidebar();
        let route = self
            .active_route
            .and_then(|id| self.routes.iter().find(|route| route.id == id));
        // 侧别是构造期参数（见 `SidebarPanel::new_right`），必须在这里分支，不能放到
        // 构造之后的 builder 上。
        let mut panel = match active_theme(cx).appearance().right_sidebar_side {
            SidebarSide::Left => {
                SidebarPanel::new("shell-right-sidebar", &state, content, window, cx)
            }
            SidebarSide::Right => {
                SidebarPanel::new_right("shell-right-sidebar", &state, content, window, cx)
            }
        };
        if panel.is_sidebar_visible() {
            panel = panel.sidebar(self.render_right_sidebar_content(route, cx));
        }
        div()
            .h_full()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(panel)
            .into_any_element()
    }

    /// 活跃路由的右侧栏内容；路由未登记时给一句占位提示。
    fn render_right_sidebar_content(&self, route: Option<&ShellRoute>, cx: &App) -> AnyElement {
        if let Some(content) = route.and_then(|route| route.right_content.as_ref()) {
            return content(cx);
        }
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .px(px(24.))
            .child(meta_text(tr(cx, "此模块暂未提供侧栏内容。"), cx))
            .into_any_element()
    }
}
