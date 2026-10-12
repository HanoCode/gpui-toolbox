//! 应用装配：全局状态、主窗口打开、路由与活动栏动作。

use duck_command::{ActionSlot, primary_modifier};
use duck_components::DuckIcon;
use duck_shell::{
    PaletteWindow, RouteId, ShellAction, ShellRoute, ShellView, main_window_options,
    palette_window_options,
};
use duck_theme::{DuckThemeState, active_theme, tr, trf};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Global, IntoElement as _, KeyBinding,
    ParentElement as _, SharedString, Styled as _, WeakEntity, Window, WindowBounds, WindowHandle,
    WindowId, point, px, size,
};
use gpui_component::{Icon, Root};

use crate::pages::{
    ASSISTANT_ROUTE, AssistantPage, Clear, CloseTab, CopySelection, DATABASE_ROUTE, DatabasePage,
    DeleteToLineEnd, DeleteToLineStart, DeleteWordForward, EMAIL_ROUTE, MailPage, MoveToLineEnd,
    MoveToLineStart, NewTab, OVERVIEW_ROUTE, OverviewPage, PROJECTS_ROUTE, PasteFromClipboard,
    PlaceholderPage, REDIS_ROUTE, RedisPage, SSH_ROUTE, ScrollLineDown, ScrollLineUp,
    ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop, SelectAll, SendCtrlC, SendTab,
    SendTabPrev, SplitPane, TOOLBOX_ROUTE, TerminalPage, ToolboxPage, ZoomIn, ZoomOut, ZoomReset,
    open_licenses, open_logs, open_plugin_center, open_settings,
};
use crate::settings::AuxWindow;
use crate::top_bar::TopBarSearch;
use crate::{actions, settings, theme_picker};

/// 主窗口初始尺寸。
const MAIN_WINDOW_SIZE: gpui::Size<gpui::Pixels> = size(px(1180.), px(760.));
/// 主窗口初始位置（屏幕坐标）。
const MAIN_WINDOW_ORIGIN: gpui::Point<gpui::Pixels> = point(px(160.), px(120.));

/// 应用级全局状态。
pub struct Desktop {
    main_window: Option<WindowHandle<Root>>,
    settings_window: Option<WindowHandle<Root>>,
    logs_window: Option<WindowHandle<Root>>,
    /// 插件中心窗口句柄，与设置窗口同一条路子。
    plugin_center_window: Option<WindowHandle<Root>>,
    /// 开源许可窗口句柄，与日志窗口同一条路子。
    licenses_window: Option<WindowHandle<Root>>,
    /// 首启引导窗口句柄。它只在该窗口开着时有效——引导关掉就是真的关了。
    onboarding_window: Option<WindowHandle<Root>>,
    /// 命令面板浮窗（全局唤起键的落点）。
    ///
    /// 它自己关自己（失焦 / Esc / 确认），所以这个句柄会**变陈旧**；
    /// 判「还开着没」一律走 `update(..).is_ok()`，与其它窗口同一条路子。
    palette_window: Option<WindowHandle<Root>>,
    /// 设置窗口的待处理跳转：`(分类 key, 选中的工具 id)`。
    ///
    /// 窗口句柄的类型是 `Root`，从外部拿不到窗口里的内容视图，所以「让已打开的设置
    /// 窗口切页」不走视图直连，而是把目标投递到这里，由设置窗口渲染时取走。
    settings_pending: Option<(&'static str, Option<String>)>,
    /// 主窗里两个关键视图的弱引用（主窗重建时刷新）。
    ///
    /// 深链接、命令行唤起、文件拖放都从**窗口之外**触发，需要一个能直达
    /// 「切路由」与「打开工具」的入口；没有它，这些入口各自还得再存一套句柄。
    /// 存弱引用而不是强引用：主窗关掉之后它们应当自然失效，而不是让窗口活不下来。
    main_views: Option<MainViews>,
    /// 主窗还没开时先记下的工具 id，窗口就绪后补跳一次。
    ///
    /// 与 [`Self::settings_pending`] 同一个形状：调用方可能比窗口先到
    /// （`on_open_urls` 在启动早期就可能触发）。
    pending_tool: Option<String>,
}

/// 主窗内两个关键视图的弱引用。
#[derive(Clone)]
struct MainViews {
    shell: WeakEntity<ShellView>,
    toolbox: WeakEntity<ToolboxPage>,
}

impl Global for Desktop {}

impl Desktop {
    /// 一份空的全局状态：还没开任何窗口时的初值。
    ///
    /// [`install`] 与单元测试共用——字段一多，两处各写一遍必然漂移。
    fn empty() -> Self {
        Self {
            main_window: None,
            settings_window: None,
            logs_window: None,
            plugin_center_window: None,
            licenses_window: None,
            onboarding_window: None,
            palette_window: None,
            settings_pending: None,
            main_views: None,
            pending_tool: None,
        }
    }

    /// 读取全局状态；必须先执行 [`install`]。
    #[must_use]
    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    fn global_mut(cx: &mut App) -> &mut Self {
        if !cx.has_global::<Self>() {
            install(cx);
        }
        cx.global_mut::<Self>()
    }

    /// 主窗口句柄；没开过（或已经关掉）时为 `None`。
    ///
    /// 托盘要用它：菜单命令走的是「先把窗口弄出来，再对着它派发动作」，而窗口句柄
    /// 只有这里存着。用 `try_global` 而不是 `global`——托盘能在窗口之前建起来，
    /// 这条路径上不该有 panic。
    #[must_use]
    pub(crate) fn main_window(cx: &App) -> Option<WindowHandle<Root>> {
        cx.try_global::<Self>()
            .and_then(|desktop| desktop.main_window)
    }

    /// 设置窗口句柄的存取：设置页打开 / 聚焦时复用。
    pub(crate) fn settings_window(cx: &mut App) -> &mut Option<WindowHandle<Root>> {
        Self::aux_window(cx, AuxWindow::Settings)
    }

    /// 日志窗口句柄的存取，与设置窗口同一条路子。
    pub(crate) fn logs_window(cx: &mut App) -> &mut Option<WindowHandle<Root>> {
        Self::aux_window(cx, AuxWindow::Logs)
    }

    /// 插件中心窗口句柄的存取，与设置窗口同一条路子。
    pub(crate) fn plugin_center_window(cx: &mut App) -> &mut Option<WindowHandle<Root>> {
        Self::aux_window(cx, AuxWindow::PluginCenter)
    }

    /// 开源许可窗口句柄的存取，与日志窗口同一条路子。
    pub(crate) fn licenses_window(cx: &mut App) -> &mut Option<WindowHandle<Root>> {
        Self::aux_window(cx, AuxWindow::Licenses)
    }

    /// 按 [`AuxWindow`] 取对应字段的可写引用。
    ///
    /// 上面四个具名存取器都走这里 —— 字段映射只此一处，加窗口时不会漏掉某条路径。
    pub(crate) fn aux_window(cx: &mut App, aux: AuxWindow) -> &mut Option<WindowHandle<Root>> {
        let desktop = Self::global_mut(cx);
        match aux {
            AuxWindow::Settings => &mut desktop.settings_window,
            AuxWindow::Logs => &mut desktop.logs_window,
            AuxWindow::PluginCenter => &mut desktop.plugin_center_window,
            AuxWindow::Licenses => &mut desktop.licenses_window,
        }
    }

    /// 按 [`AuxWindow`] 取句柄快照（不改动全局状态）。
    fn aux_handle(&self, aux: AuxWindow) -> Option<WindowHandle<Root>> {
        match aux {
            AuxWindow::Settings => self.settings_window,
            AuxWindow::Logs => self.logs_window,
            AuxWindow::PluginCenter => self.plugin_center_window,
            AuxWindow::Licenses => self.licenses_window,
        }
    }

    /// 当前开着的辅助窗口，顺序固定（[`AuxWindow::ALL`]）。
    #[must_use]
    fn open_aux_windows(cx: &App) -> Vec<AuxWindow> {
        let Some(desktop) = cx.try_global::<Self>() else {
            return Vec::new();
        };
        AuxWindow::ALL
            .into_iter()
            .filter(|aux| desktop.aux_handle(*aux).is_some())
            .collect()
    }

    /// 把「当前开着的辅助窗口」写回存储。开窗 / 关窗后都要调一次。
    pub(crate) fn remember_aux_open(cx: &App) {
        settings::save_aux_open(&Self::open_aux_windows(cx), cx);
    }

    /// 首启引导窗口句柄的存取。
    pub(crate) fn onboarding_window(cx: &mut App) -> &mut Option<WindowHandle<Root>> {
        &mut Self::global_mut(cx).onboarding_window
    }

    /// 引导窗口句柄的快照；没开过（或已经关掉）时为 `None`。
    ///
    /// 通知投递要用它把引导窗口从候选里剔掉（见 `update::on_notify_window`）：它是首启
    /// 流程开的居中小向导，提示落进去就是那个小窗的右上角，而不是程序主窗的右上角。
    /// 与 [`Self::main_window`] 同形：`try_global`，全局状态还没装时不 panic。
    #[must_use]
    pub(crate) fn onboarding_handle(cx: &App) -> Option<WindowHandle<Root>> {
        cx.try_global::<Self>()
            .and_then(|desktop| desktop.onboarding_window)
    }

    /// 命令面板浮窗的句柄快照。浮窗自己会关自己，所以这个句柄**可能是陈旧的**——
    /// 判「还开着没」用 `update(..).is_ok()`，与其它窗口同一条路子。
    ///
    /// 与 [`Self::main_window`] 同形：`try_global`，全局状态还没装时不 panic。
    #[must_use]
    pub(crate) fn palette_window(cx: &App) -> Option<WindowHandle<Root>> {
        cx.try_global::<Self>()
            .and_then(|desktop| desktop.palette_window)
    }

    /// 投递一次设置窗口跳转（覆盖上一次未取走的请求）。
    pub(crate) fn set_settings_pending(cx: &mut App, page: &'static str, tool: Option<String>) {
        Self::global_mut(cx).settings_pending = Some((page, tool));
    }

    /// 取走设置窗口跳转；没有则返回 `None`。
    pub(crate) fn take_settings_pending(cx: &mut App) -> Option<(&'static str, Option<String>)> {
        Self::global_mut(cx).settings_pending.take()
    }

    /// 记下主窗里的两个关键视图（每次建主窗时覆盖）。
    ///
    /// 收弱引用：`toolbox` 实体在 routes 里会被 move 走，调用方只能提前取一份弱引用。
    pub(crate) fn remember_main_views(
        cx: &mut App,
        shell: &Entity<ShellView>,
        toolbox: &WeakEntity<ToolboxPage>,
    ) {
        Self::global_mut(cx).main_views = Some(MainViews {
            shell: shell.downgrade(),
            toolbox: toolbox.clone(),
        });
    }

    /// 主窗视图的弱引用快照。
    ///
    /// 返回克隆而非借用：调用方拿到之后要 `update` 它们，那时不能再借住全局状态。
    fn main_views(cx: &App) -> Option<MainViews> {
        cx.try_global::<Self>()?.main_views.clone()
    }

    /// 主窗还没开时记下待跳转的工具。
    pub(crate) fn remember_pending_tool(cx: &mut App, id: &str) {
        tracing::info!(
            tool = id,
            "the main window is not up yet; queueing the jump"
        );
        Self::global_mut(cx).pending_tool = Some(id.to_owned());
    }

    /// 取走待跳转的工具。
    pub(crate) fn take_pending_tool(cx: &mut App) -> Option<String> {
        Self::global_mut(cx).pending_tool.take()
    }
}

/// 切到某条路由；主窗不可用时返回 `false`。
pub fn navigate_to(cx: &mut App, route: RouteId) -> bool {
    let Some(views) = Desktop::main_views(cx) else {
        return false;
    };
    views
        .shell
        .update(cx, |shell, cx| shell.navigate_by_id(route.as_str(), cx))
        .is_ok()
}

/// 打开一个工具：先切到工具箱路由，再与侧栏点击同路打开标签页。
///
/// 命令面板、深链接、命令行唤起、文件拖放共用这一条路——四处各写一遍
/// 「先切路由再开工具」迟早会分叉。
///
/// 返回 `false` 表示没做成：工具没注册，或者主窗还没开（此时工具 id 会被记下，
/// 窗口就绪后补跳，见 [`Desktop::take_pending_tool`]）。
pub fn open_tool(cx: &mut App, id: &str) -> bool {
    // 借注册表把外部带来的 id 还原成 `'static`（`ToolRoute` 存 `&'static str`）。
    // 没注册的工具直接失败：打开一个空标签页比什么都不做更让人困惑。
    let Some(tool) = duck_tools::ToolRegistry::get(cx, id).map(|tool| tool.id()) else {
        return false;
    };
    if !navigate_to(cx, TOOLBOX_ROUTE) {
        // 主窗没开（唤起键只开浮窗、用户从浮窗里选了工具）：先把主窗叫出来，
        // 跳转由 `build_main_window` 末尾那段补上 —— 那条路径本来就是为
        // 「调用方比窗口先到」准备的（见 `on_open_urls`）。不叫的话，这一步之后
        // 屏幕上是「什么都没发生」，而工具 id 只能一直躺在待办里。
        Desktop::remember_pending_tool(cx, id);
        open_main_window(cx);
        return false;
    }
    let Some(views) = Desktop::main_views(cx) else {
        return false;
    };
    views
        .toolbox
        .update(cx, |page, cx| page.open_route(tool, cx))
        .is_ok()
}

/// 建立应用级全局状态（在创建任何视图之前调用）。
pub fn install(cx: &mut App) {
    cx.set_global(Desktop::empty());

    // 交互骨架：注册表 → 快捷键 / 菜单 / 命令面板 / 活动栏。
    duck_command::install(cx);
    actions::declare_all(cx);
    duck_command::bind_all(cx);
    // 终端画布的键位必须绑在更深的 `"Terminal"` 上下文里：gpui 的按键派发**先查
    // action 绑定**，全部未命中才轮到 `on_key_down`；而 `gpui_base::Root` 在 `"Root"`
    // 上下文占着 `"tab"` / `"shift-tab"` / `"cmd-c"`（macOS）/ `"ctrl-c"`（非 macOS）
    // （`root.rs:15`）。不在这里用同键动作盖掉，终端就收不到这些键。
    // 动作定义与处理见 `pages::terminal_canvas`；这套清单对齐
    // `temp/zed/assets/keymaps/default-macos.json` 的 `"Terminal"` 上下文。
    let modifier = primary_modifier();
    cx.bind_keys([
        // —— 全局冲突键 ——
        KeyBinding::new("tab", SendTab, Some("Terminal")),
        KeyBinding::new("shift-tab", SendTabPrev, Some("Terminal")),
        // `Ctrl-C`（SIGINT）：非 macOS 上 Root 把它绑给了 `Copy`，不盖掉就是死的。
        // macOS 上显式绑也无害 —— 发出去的正是终端本来就该收到的 `0x03`。
        KeyBinding::new("ctrl-c", SendCtrlC, Some("Terminal")),
        // —— 字号 / 清屏 ——
        // `=` / `+` 都绑（美式布局 `+` 在 `=` 上，事件里两种键名都会出现）。
        KeyBinding::new(&format!("{modifier}-="), ZoomIn, Some("Terminal")),
        KeyBinding::new(&format!("{modifier}-+"), ZoomIn, Some("Terminal")),
        KeyBinding::new(&format!("{modifier}--"), ZoomOut, Some("Terminal")),
        KeyBinding::new(&format!("{modifier}-0"), ZoomReset, Some("Terminal")),
        KeyBinding::new(&format!("{modifier}-k"), Clear, Some("Terminal")),
        // 分屏 / 取消分屏（与工具条那个图标同一条路径）。
        KeyBinding::new(&format!("{modifier}-d"), SplitPane, Some("Terminal")),
        // —— 视口滚动：只挪视口，不向 PTY 写字节 ——
        KeyBinding::new("shift-up", ScrollLineUp, Some("Terminal")),
        KeyBinding::new("shift-down", ScrollLineDown, Some("Terminal")),
        KeyBinding::new("shift-pageup", ScrollPageUp, Some("Terminal")),
        KeyBinding::new("shift-pagedown", ScrollPageDown, Some("Terminal")),
        KeyBinding::new(&format!("{modifier}-up"), ScrollPageUp, Some("Terminal")),
        KeyBinding::new(
            &format!("{modifier}-down"),
            ScrollPageDown,
            Some("Terminal"),
        ),
        KeyBinding::new("shift-home", ScrollToTop, Some("Terminal")),
        KeyBinding::new("shift-end", ScrollToBottom, Some("Terminal")),
        KeyBinding::new(&format!("{modifier}-home"), ScrollToTop, Some("Terminal")),
        KeyBinding::new(&format!("{modifier}-end"), ScrollToBottom, Some("Terminal")),
        // —— 行编辑（发控制字节，靠 shell 的行编辑生效；对齐 zed 的 SendKeystroke）——
        KeyBinding::new(
            &format!("{modifier}-left"),
            MoveToLineStart,
            Some("Terminal"),
        ),
        KeyBinding::new(
            &format!("{modifier}-right"),
            MoveToLineEnd,
            Some("Terminal"),
        ),
        KeyBinding::new(
            &format!("{modifier}-backspace"),
            DeleteToLineStart,
            Some("Terminal"),
        ),
        KeyBinding::new(
            &format!("{modifier}-delete"),
            DeleteToLineEnd,
            Some("Terminal"),
        ),
        KeyBinding::new("alt-delete", DeleteWordForward, Some("Terminal")),
        // —— 剪贴板 / 选区 ——
        // `⌘C` 在 `"Root"` 上下文被绑给了 `Copy`、`⌘A` 被绑给了 `SelectAll`
        //（`gpui-base/src/root.rs`）：不在更深的 `"Terminal"` 上下文用同键动作盖掉，
        // 终端里按 ⌘C 永远不会是「复制选中的文本」。
        KeyBinding::new(&format!("{modifier}-c"), CopySelection, Some("Terminal")),
        KeyBinding::new(
            &format!("{modifier}-v"),
            PasteFromClipboard,
            Some("Terminal"),
        ),
        KeyBinding::new(&format!("{modifier}-a"), SelectAll, Some("Terminal")),
        // —— 标签 ——
        // 主流终端的一致键位；此前只写在设置页的快捷键说明里，一条都没绑。
        KeyBinding::new(&format!("{modifier}-t"), NewTab, Some("Terminal")),
        KeyBinding::new(&format!("{modifier}-w"), CloseTab, Some("Terminal")),
    ]);
    // —— 数据库页 ——
    // 绑在页面自己的 `"Database"` 上下文（`pages::db::bind_keys`）：焦点在 SQL 编辑器里时
    // 上下文栈更深的是 `Editor`，它占着 `⌘Z`（撤销）与 `⌘.`，所以只挑它没占的
    // `⌘↵` / `⌘S` / `⌘⇧⌫`。三条键在 `gpui_base::Root` 的默认表里都不存在
    // （见 `gpui-base/src/input` 的 `KeyBinding` 清单，已逐条核对）。
    crate::pages::bind_db_keys(cx);
    duck_command::refresh_menus(cx);
    actions::install_stateless_handlers(cx);

    // 工具目录：工具箱的「菜单表」，列表 / 分类 / 搜索 / 工具级设置都由它派生。
    duck_tools::ToolRegistry::install(cx);
    // 顶栏搜索词与智能粘贴负载：搜索框写、工具箱列表 / 横幅读。
    duck_tools::ToolQuery::install(cx);
    duck_tools::PasteInbox::install(cx);
    crate::tools::register_all(cx);

    // 插件发现与装配：插件声明的工具贡献注册进同一张注册表，派生管线共用。
    crate::plugins::init(cx);

    // 插件市场：读源地址与上一次的索引缓存（离线也能列出「未安装」）。
    // 联网刷新排在窗口就绪之后，见 `main.rs`。
    crate::market::init(cx);

    // 更新器状态：更新源与阶段。后台顺带清扫上一次安装留下的旧 bundle。
    crate::update::init(cx);

    // 外观变化（含语言切换）→ 落盘 + 重建菜单文案。
    cx.observe_global::<DuckThemeState>(|cx| {
        settings::save_appearance(cx);
        duck_command::refresh_menus(cx);
    })
    .detach();

    // 窗口关闭 → 更新「上次开着哪些窗口」。只注册一次：回调按窗口 id 比对 `Desktop`
    // 里存的句柄，命中哪个辅助窗口就清掉哪个。
    cx.on_window_closed(on_window_closed).detach();

    // 退出前把去抖队列落盘。**不能依赖 `Drop for Store`** —— 平台退出走的是
    // `process::exit`，全局值不保证被析构，而「刚改的设置没保存」正是最招人烦的一类 bug。
    //
    // 落盘语义本身由 `duck_store` 的 `flush_is_a_batch_boundary` 守着（写 → flush → 读得到）；
    // 这条日志守的是另一半：这个钩子**真的被平台调用过**。退出若变慢，看它的时间戳。
    cx.on_app_quit(|cx| {
        tracing::info!("flushing pending store writes before exit");
        duck_store::flush(cx);
        async {}
    })
    .detach();
}

/// 窗口关闭后的收尾：清句柄 + 刷新会话记录（见 [`Desktop::open_aux_windows`]）。
///
/// 主窗也要清。留着过期句柄不会 panic（每条路径都先 `update` 探活），但 `main_views`
/// 里那两个弱引用是**死的**，而「主窗还开着吗」会一直得到 `Some` —— 托盘、深链接、
/// 全局唤起键据此判断都会绕远路（白等一次 `update` 失败），日志里也看不出差异。
fn on_window_closed(cx: &mut App, id: WindowId) {
    let Some(desktop) = cx.try_global::<Desktop>() else {
        return;
    };
    let closed_main = desktop
        .main_window
        .is_some_and(|handle| AnyWindowHandle::from(handle).window_id() == id);
    let closed_aux = AuxWindow::ALL.into_iter().find(|aux| {
        desktop
            .aux_handle(*aux)
            .is_some_and(|handle| AnyWindowHandle::from(handle).window_id() == id)
    });
    if closed_main {
        // 主窗不进「上次开着哪些窗口」那份记录：它不在恢复列表里（辅窗才在）。
        let desktop = Desktop::global_mut(cx);
        desktop.main_window = None;
        desktop.main_views = None;
        return;
    }
    if let Some(aux) = closed_aux {
        *Desktop::aux_window(cx, aux) = None;
        Desktop::remember_aux_open(cx);
    }
}

/// 按上次退出时的记录重开辅助窗口。
///
/// 每个窗口各自走「已开则聚焦、否则新建」的那条路径；这里只管调用顺序。
/// 调用方要在排完它们之后再把主窗口抬回前台，否则最后一个辅窗会抢走焦点。
pub fn restore_aux_windows(cx: &mut App) {
    let remembered = settings::load_aux_open(cx);
    if remembered.is_empty() {
        return;
    }
    tracing::info!(count = remembered.len(), "restoring auxiliary windows");
    for aux in remembered {
        match aux {
            AuxWindow::Settings => open_settings(cx),
            AuxWindow::Logs => open_logs(cx),
            AuxWindow::PluginCenter => open_plugin_center(cx),
            AuxWindow::Licenses => open_licenses(cx),
        }
    }
}

/// 打开主窗口；已存在则前置激活。
pub fn open_main_window(cx: &mut App) {
    // `is_ok()` 为假说明句柄过期了（窗口已经被关掉）：那就往下走，重新开一个。
    if let Some(handle) = Desktop::global(cx).main_window
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        cx.activate(true);
        return;
    }

    let mut options = main_window_options();
    // 恢复上次的位置、尺寸与显示状态，并把几何夹回可见区域（外接屏拔掉后不能把窗口
    // 丢在屏幕外）；首次运行取默认几何。
    options.window_bounds = Some(settings::restore_main_window(cx).unwrap_or_else(|| {
        WindowBounds::Windowed(Bounds::new(MAIN_WINDOW_ORIGIN, MAIN_WINDOW_SIZE))
    }));

    match cx.open_window(options, build_main_window) {
        Ok(handle) => {
            Desktop::global_mut(cx).main_window = Some(handle);
            cx.activate(true);
        }
        Err(error) => report_window_failure("主窗口", &error, cx),
    }
}

/// 窗口建不出来时的统一收尾：记一条 error 日志，并在还能找到的窗口上弹一条用户可见的提示。
///
/// 这里原本是 `eprintln!`：release 包没有 stderr 可看（带 bundle 启动时连终端都没有），
/// 用户能观察到的只有「点了没反应」。提示文案不带原始错误——那是日志的活；toast 只说清
/// 「哪个窗口没开起来」，能让用户自己判断要不要重启或看日志。
pub(crate) fn report_window_failure(
    what: &'static str,
    error: &impl std::fmt::Display,
    cx: &mut App,
) {
    tracing::error!(window = what, %error, "could not open the window");
    let message = trf(cx, "无法打开{}，详见日志。", &[tr(cx, what)]);
    crate::update::on_notify_window(cx, move |window, cx| {
        duck_components::notify::error(message, window, cx);
    });
}

/// 唤起快捷键的**旧行为**：主窗不在前台就把它叫出来并打开主窗内嵌的面板浮层；
/// 已经在前台就收起来。
///
/// 现在是 [`toggle_launcher`] 在「设置 → 行为 → 唤起」开启时转交过来的路径
/// （见那里的说明）。托盘菜单与深链接都只做「叫出来」，只有这里与 `toggle_launcher`
/// 是**切换**——因为只有它们是同一个键按第二遍，而用户按第二遍时的意思必然是「收起来」。
pub fn toggle_main_window(cx: &mut App) {
    let frontmost = Desktop::main_window(cx)
        .and_then(|handle| {
            handle
                .update(cx, |_, window, _| window.is_window_active())
                .ok()
        })
        .unwrap_or(false);
    if frontmost {
        put_away_main_window(cx);
        return;
    }
    summon_main_window(cx);
}

/// 唤起主窗：隐藏 / 最小化 / 已经关掉，三种状态都走这一条。
///
/// 顺序不能换：先 [`duck_platform::restore_visible`]（macOS 上解除应用隐藏、
/// 把被最小化的窗口还原），再 [`open_main_window`]（激活 + `makeKeyAndOrderFront`）。
/// 反过来的话，在「应用被隐藏」的状态下抬窗口不会把应用带出来。**只抬窗口、不开面板**：
/// 托盘菜单、深链接、菜单栏命令要的只是窗口回来。
pub fn reveal_main_window(cx: &mut App) {
    duck_platform::restore_visible();
    open_main_window(cx);
}

/// 唤起主窗并打开**主窗内嵌**的面板浮层 —— [`toggle_main_window`] 的「唤起」那一半。
///
/// 与 [`reveal_main_window`] 分开：只有它是同一个键按第二遍，用户按第二遍时的意思
/// 必然是「收起来」；而「唤起」这一半顺带开面板，是「我按快捷键就是想找东西」的默认答案。
pub fn summon_main_window(cx: &mut App) {
    reveal_main_window(cx);
    let Some(views) = Desktop::main_views(cx) else {
        return;
    };
    let Some(handle) = Desktop::main_window(cx) else {
        return;
    };
    let _ = handle.update(cx, |_, window, cx| {
        // `open_palette` 而不是 `toggle_palette`：面板本来就开着时也不该被关掉。
        let _ = views
            .shell
            .update(cx, |shell, cx| shell.open_palette(window, cx));
    });
}

/// 把主窗收起来（唤起键按第二遍时）。
///
/// 「收起来」两端不同：macOS 隐藏**整个应用**（`[NSApp hide:]`，与 `⌘H` 同款，
/// 辅助窗口一起走，恢复时由 `restore_visible` 解除隐藏）；Windows / Linux 只最小化
/// 主窗 —— **不能照搬 macOS 的 `cx.hide()`**：它在 Windows 上是空实现
/// （`gpui_windows/src/platform.rs` 里的 `fn hide(&self) {}`），照搬会变成
/// 「按了没反应」；反过来 macOS 上最小化更糟，`activate_window` 走的是
/// `makeKeyAndOrderFront:`，而 AppKit 里 `orderFront:` 不解除最小化，
/// 于是窗口会永远留在 Dock 里回不来。
fn put_away_main_window(cx: &mut App) {
    if cfg!(target_os = "macos") {
        cx.hide();
        return;
    }
    if let Some(handle) = Desktop::main_window(cx) {
        // 最小化失败（窗口刚好被关掉）不值得补救：下一次按键会走 `summon` 那条路。
        let _ = handle.update(cx, |_, window, _| window.minimize_window());
    }
}

/// 全局唤起快捷键的落点：**开 / 收命令面板浮窗**。
///
/// 默认只开浮窗。键位是 `⌥Space`，紧挨 `⌘Space`（Spotlight），自带**启动器**语义：
/// 按它期待的是一个盖在当前工作之上的浮层，而不是把 Duck 整个切到前台。
///
/// 「设置 → 行为 → 唤起」里的开关能把**完整旧行为**要回来（转交 [`toggle_main_window`]：
/// 唤起主窗 + 主窗内嵌面板浮层）。为什么是整条转交、而不是「主窗 + 浮窗」叠加：
///
/// - 一次按键屏幕上只该有**一个**面板。主窗里再叠一个浮窗就是两个面板同时存在。
/// - 那条旧路径是独立浮窗存在之前就一直在跑的，窗口激活语义与中文输入法都不必重新赌
///   一遍 —— 于是这个开关天然成为**独立浮窗出问题时的兜底路径**（见 `palette_window`）。
///
/// 第二遍按同一个键是**收起来** —— 与 [`toggle_main_window`] 同一套直觉：同一个键
/// 按第二遍，用户的意思必然是收起来。
pub fn toggle_launcher(cx: &mut App) {
    if settings::launcher_opens_main_window(cx) {
        toggle_main_window(cx);
        return;
    }
    if palette_window_is_open(cx) {
        close_palette_window(cx);
        return;
    }
    open_palette_window(cx);
}

/// 浮窗是否开着；句柄陈旧（它自己关掉了）时返回 `false`。
///
/// `handle.update` 要 `&mut App`（与 [`navigate_to`] 判主窗存活同一条路子），所以这里的
/// 参数不能是 `&App` —— 空闭包只为问一句「窗口还在不在」。
fn palette_window_is_open(cx: &mut App) -> bool {
    Desktop::palette_window(cx).is_some_and(|handle| handle.update(cx, |_, _, _| {}).is_ok())
}

/// 开浮窗；已经开着则只把它抬到前面并**保持打开**。
///
/// 与 `ShellView::open_palette` 同一套语义：按唤起键想要的永远是「现在开始打字」，
/// 不是「把我上次开着的面板关掉」。不重建已经开着的那个，是因为重建会把用户
/// 刚打进去的查询丢掉。
pub fn open_palette_window(cx: &mut App) {
    if let Some(handle) = Desktop::palette_window(cx)
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    match cx.open_window(palette_window_options(cx), |window, cx| {
        let palette = cx.new(|cx| {
            PaletteWindow::new(window, cx)
                // 与主窗内浮层共用同一份数据源，两处条目顺序才会一致。
                .with_groups(crate::palette_tools::palette_groups)
                .with_empty(crate::palette_tools::palette_empty)
        });
        // 系统外观变化 → 「跟随系统」时重解析主题（与其它窗口同形）。
        observe_system_appearance(window);
        cx.new(|cx| Root::new(palette, window, cx))
    }) {
        Ok(handle) => Desktop::global_mut(cx).palette_window = Some(handle),
        Err(error) => report_window_failure("命令面板", &error, cx),
    }
}

/// 收起浮窗（唤起键按第二遍）。
///
/// 只调 `remove_window`，**不清理** `Desktop` 里那个句柄：句柄保持陈旧即可，所有
/// 判定都走 `update(..).is_ok()`。这样「窗口自己关自己」（失焦 / Esc / 确认）与
/// 「外面把它关掉」两条路径的行为完全一致，不需要各自记得再清一次状态 ——
/// 少一处状态就少一处会忘记同步的地方。
fn close_palette_window(cx: &mut App) {
    if let Some(handle) = Desktop::palette_window(cx) {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
}

/// 让某个窗口跟随系统外观：系统在浅色 / 深色之间切换时，若偏好是「跟随系统」
/// 就重新解析主题并重绘全部窗口。
///
/// gpui 的外观观察是**窗口级**的，也没有「新窗口」全局钩子 —— 所以每个窗口都要挂一次。
/// 不挂的话 `duck_theme::sync_system_theme` 就是个死函数：偏好为「跟随系统」时只在
/// 安装主题那一刻取一次系统外观，用户在系统设置里切换后界面（尤其是自绘的标题栏）
/// 不会跟着变。多挂几次也无害：`sync_system_theme` 在模式未变时是空操作。
pub(crate) fn observe_system_appearance(window: &Window) {
    window
        .observe_window_appearance(|_, cx| duck_theme::sync_system_theme(cx))
        .detach();
}

/// 构造 shell：概览 / 工具箱 / SSH 终端 / 数据库 / Redis / AI 助手六条已实现路由
/// + 2 条功能占位路由（项目管理 / 邮箱），以及活动栏底部的外观切换与设置动作。
///
/// 路由一旦登记，活动栏按钮、`⌘n` 键位、原生菜单项、命令面板条目会**同时**出现
/// （四输出面派生自同一份动作清单）。占位路由曾按 P2-4 整体移除，现按用户要求
/// 恢复入口：点开是「功能正在建设中」的空白页，各模块落地后换掉视图即可
/// （SSH 终端与数据库页即按这条路径从占位页换成了真实页面）。
fn build_main_window(window: &mut Window, cx: &mut App) -> Entity<Root> {
    let overview = cx.new(|_| OverviewPage::new());
    let toolbox = cx.new(ToolboxPage::new);
    let ssh = cx.new(|cx| TerminalPage::new(window, cx));
    let database = cx.new(|cx| DatabasePage::new(window, cx));
    let redis = cx.new(|cx| RedisPage::new(window, cx));
    let projects = cx.new(|_| PlaceholderPage::new("项目管理", DuckIcon::Kanban));
    // 邮箱：已接上 `duck-mail` 内核（M1：账户 CRUD、IMAP 拉取、增量同步、正文按需下载）。
    // 构造时会在后台打开库并拉账户，所以需要 `window`（同数据库 / Redis 页）。
    let email = cx.new(|cx| MailPage::new(window, cx));
    // AI 助手：占位页已换成真实页面（`pages/assistant.rs`，见 `docs/ai-design.md`）。
    let assistant = cx.new(|cx| AssistantPage::new(window, cx));

    // 顶栏搜索入口是窗口级 chrome：所有路由共用同一实例，切换路由时状态保留。
    let search = cx.new(|_| TopBarSearch::new());
    let toolbox_sidebar = toolbox.read(cx).sidebar_state();
    // 右侧栏（帮助与设置）由 shell 统一布局；toolbox 只提供按活跃工具渲染的内容。
    let toolbox_right_view = toolbox.clone();
    // SSH 终端：左栏（连接列表）状态同样由 shell 顶栏的侧栏开关共享，
    // 右栏（会话详情 / 传输队列）内容由本页按活跃标签渲染。
    let ssh_sidebar = ssh.read(cx).sidebar_state();
    let ssh_right_view = ssh.clone();
    // 数据库：左栏（连接列表）与右栏（连接信息 / 对象树 / 选中行详情）同 SSH 的挂法。
    let db_sidebar = database.read(cx).sidebar_state();
    let db_right_view = database.clone();
    // Redis：与数据库页同款挂法（左栏连接列表 / 右栏连接信息 + 键详情）。
    let redis_sidebar = redis.read(cx).sidebar_state();
    let redis_right_view = redis.clone();
    // AI 助手：左栏会话历史、右栏会话参数，挂法与数据库 / Redis 页一致。
    let assistant_sidebar = assistant.read(cx).sidebar_state();
    let assistant_right_view = assistant.clone();
    // 邮箱：只有左栏（账户与文件夹树）。**没有右栏**——前一版的右栏「AI 面板」
    // （摘要 / 回复建议 / 问答）按 P3-6 删除了：邮件 AI 后端在 M2，没有后端可接的
    // 面板只能靠演示文案撑着。详见 `pages/mail.rs` 模块头的处置表。
    let email_sidebar = email.read(cx).sidebar_state();

    // 顶栏前导留白按活跃路由登记的内容区左缘算：概览整页通栏（不登记，搜索框贴顶栏左缘），
    // 工具箱登记自己的内容区左缘（随侧栏宽度变化），搜索框与它对齐。
    let toolbox_layout = toolbox.clone();
    // 工具箱实体的弱引用：`routes` 会把实体 move 走，而窗口就绪后还要靠它跳工具
    // （深链接 / 拖放，见 `open_tool`）。
    let toolbox_for_jump = toolbox.downgrade();

    let routes = vec![
        ShellRoute::new(
            OVERVIEW_ROUTE,
            "duck-route-overview",
            // 占位：真实文案由下方 `with_label_fn` 按语言解析，此处不会被渲染。
            "Overview",
            Icon::new(DuckIcon::Gauge),
            overview,
        )
        // 路由名随语言解析：切换语言后 shell 重新渲染即换文案。
        .with_label_fn(|cx| SharedString::from(tr(cx, "概览"))),
        ShellRoute::new(
            TOOLBOX_ROUTE,
            "duck-route-toolbox",
            // 占位：真实文案由下方 `with_label_fn` 按语言解析，此处不会被渲染。
            "Toolbox",
            Icon::new(DuckIcon::Box),
            toolbox,
        )
        .with_label_fn(|cx| SharedString::from(tr(cx, "工具箱")))
        .with_sidebar(toolbox_sidebar)
        .with_right_sidebar(move |cx| toolbox_right_view.read(cx).render_right_sidebar(cx))
        .with_content_left(move |cx| toolbox_layout.read(cx).content_left()),
        ShellRoute::new(
            SSH_ROUTE,
            "duck-route-ssh",
            // 占位：真实文案由下方 `with_label_fn` 按语言解析，此处不会被渲染。
            "SSH",
            Icon::new(DuckIcon::SquareTerminal),
            ssh,
        )
        .with_label_fn(|cx| SharedString::from(tr(cx, "SSH 终端")))
        .with_sidebar(ssh_sidebar)
        .with_right_sidebar(move |cx| ssh_right_view.read(cx).render_right_sidebar(cx)),
        ShellRoute::new(
            DATABASE_ROUTE,
            "duck-route-database",
            // 占位：真实文案由下方 `with_label_fn` 按语言解析，此处不会被渲染。
            "Database",
            Icon::new(DuckIcon::Database),
            database,
        )
        .with_label_fn(|cx| SharedString::from(tr(cx, "数据库")))
        .with_sidebar(db_sidebar)
        .with_right_sidebar(move |cx| db_right_view.read(cx).render_right_sidebar(cx)),
        ShellRoute::new(
            REDIS_ROUTE,
            "duck-route-redis",
            // 占位：真实文案由下方 `with_label_fn` 按语言解析，此处不会被渲染。
            "Redis",
            Icon::new(DuckIcon::DatabaseZap),
            redis,
        )
        .with_label_fn(|cx| SharedString::from(tr(cx, "Redis")))
        .with_sidebar(redis_sidebar)
        .with_right_sidebar(move |cx| redis_right_view.read(cx).render_right_sidebar(cx)),
        ShellRoute::new(
            PROJECTS_ROUTE,
            "duck-route-projects",
            // 占位：真实文案由下方 `with_label_fn` 按语言解析，此处不会被渲染。
            "Projects",
            Icon::new(DuckIcon::Kanban),
            projects,
        )
        .with_label_fn(|cx| SharedString::from(tr(cx, "项目管理"))),
        ShellRoute::new(
            ASSISTANT_ROUTE,
            "duck-route-assistant",
            // 占位：真实文案由下方 `with_label_fn` 按语言解析，此处不会被渲染。
            "AI Assistant",
            Icon::new(DuckIcon::Bot),
            assistant,
        )
        .with_label_fn(|cx| SharedString::from(tr(cx, "AI 助手")))
        .with_sidebar(assistant_sidebar)
        .with_right_sidebar(move |cx| assistant_right_view.read(cx).render_right_sidebar(cx)),
        // 邮箱：活动栏最末一条（`⌘8`），现有 ⌘1–⌘7 不动。
        ShellRoute::new(
            EMAIL_ROUTE,
            "duck-route-email",
            // 占位：真实文案由下方 `with_label_fn` 按语言解析，此处不会被渲染。
            "Email",
            Icon::new(DuckIcon::Mail),
            email,
        )
        .with_label_fn(|cx| SharedString::from(tr(cx, "邮箱")))
        .with_sidebar(email_sidebar),
    ];

    let actions = activity_actions(cx);
    // 会话恢复的两项输入：上次所在路由、该路由右栏的展开态（见 `settings` 会话小节）。
    let initial_route = settings::load_window_route(cx);
    let right_sidebar_open = settings::load_right_sidebar_open(cx);

    // 搜索框挂在 shell 上：任何路由（含概览）都渲染同一个实例。
    let shell = cx.new(|cx| {
        let mut shell = ShellView::new(routes, actions, cx)
            // 上次的路由；存档里的 id 已失效时 shell 自己回落首条路由。
            .with_initial_route(initial_route)
            .with_title_bar_leading(search)
            // 面板数据源：最近使用置顶 → 动作分组 → 设置分类 → 工具分组（按分类，含拼音检索词）。
            .with_palette_groups(crate::palette_tools::palette_groups)
            // 面板空结果态：面板只列得出已装的工具，这里给条去插件市场的明路。
            .with_palette_empty(crate::palette_tools::palette_empty)
            // 尾随插槽：插件中心按钮在语言切换左侧；传闭包，每帧求值才能读到
            // 实时外观（图标颜色、语言模式文案的勾选位）。
            .with_title_bar_trailing(title_bar_trailing)
            // 窗口几何变化 → 去抖持久化。
            .on_window_bounds_changed(|window, cx| settings::save_window_bounds(window, cx))
            // 路由 / 右栏变化 → 去抖持久化，供下次启动恢复。
            .on_route_changed(|route, cx| settings::save_window_route(route, cx))
            .on_right_sidebar_changed(|open, cx| settings::save_right_sidebar_open(open, cx))
            // 文件拖放：落到工具箱的智能粘贴横幅，与顶栏那个「识别粘贴」按钮同一条路。
            .on_files_dropped(crate::deeplink::handle_dropped_files);
        // 右栏展开态在建窗时就位：等第一帧再切会让用户看到「先弹出、再收起」。
        if let Some(open) = right_sidebar_open {
            shell.set_active_right_sidebar_collapsed(!open, cx);
        }
        // 落回哪条路由记一条：存档失效时的回落是**静默**的，只有日志能区分
        // 「按存档恢复」与「回落首页」——出问题时不必靠肉眼比对界面。
        tracing::info!(route = shell.active_route_id(), "main window opened");
        shell
    });
    // 依赖 shell 的动作（面板 / 路由 / 侧栏）在窗口视图就绪后挂上处理器。
    actions::install_shell_handlers(cx, &shell);
    // 登记主窗内两个关键视图：深链接 / 拖放 / 命令行唤起都从窗口之外触发，
    // 靠它们才能「切路由」与「打开工具」（见 `navigate_to` / `open_tool`）。
    Desktop::remember_main_views(cx, &shell, &toolbox_for_jump);
    // 系统外观变化 → 「跟随系统」时重解析主题（见本函数上方说明）。
    observe_system_appearance(window);

    // 命令面板「打开工具」：动作由面板直接派发（`window.dispatch_action`），
    // 这里用全局监听承接。落点与深链接 / 拖放完全同路。
    cx.on_action(
        move |action: &crate::palette_tools::OpenTool, cx: &mut App| {
            if !open_tool(cx, &action.id) {
                tracing::warn!(tool = %action.id, "the palette asked for a tool that is not available");
            }
        },
    );

    // 命令面板「设置」分组：打开设置窗口并定位到该分类。
    // 与面板里的工具条目同形——`on_action` 承接面板直接派发的动作。
    cx.on_action(
        move |action: &crate::palette_tools::OpenSettingsPage, cx: &mut App| {
            if let Some(page) = crate::palette_tools::settings_page_of(action) {
                crate::pages::open_settings_at(page, None, cx);
            }
        },
    );

    let root = cx.new(|cx| Root::new(shell, window, cx));
    // 建窗之前积压的跳转在这里补上：`on_open_urls` 可能比主窗先到。
    // 排一次 `defer` 而不是立刻执行——此刻窗口视图刚建出来，还没渲染过。
    if let Some(id) = Desktop::take_pending_tool(cx) {
        cx.defer(move |cx| {
            open_tool(cx, &id);
        });
    }
    root
}

/// 顶栏尾随插槽：插件中心按钮 + 语言切换横排组合。
///
/// 插槽每帧求值，两个控件都能读到实时外观（语言模式文案、图标颜色随主题走）。
/// 用户可在「设置 → 外观 → 界面」把这一组整体关掉；关掉后顶栏只剩搜索框与通知铃铛。
fn title_bar_trailing(cx: &App) -> gpui::AnyElement {
    if !active_theme(cx).appearance().show_top_bar_extras {
        return gpui::div().into_any_element();
    }
    // 按钮带文字后需要明显间隔，用间距 token 拉开。
    let gap = active_theme(cx).tokens().spacing.md;
    gpui_component::h_flex()
        .flex_none()
        .gap(gap)
        .child(crate::pages::plugin_center::title_bar_button(cx))
        .child(crate::language_switch::language_switch(cx))
        .into_any_element()
}

/// 从动作注册表派生活动栏按钮：与快捷键 / 菜单 / 命令面板同一份清单。
fn activity_actions(cx: &App) -> Vec<ShellAction> {
    duck_command::metas(cx)
        .iter()
        .filter(|meta| meta.slots.contains(&ActionSlot::ActivityBar))
        .filter_map(|meta| meta.icon.map(|icon| (meta, icon)))
        .map(|(meta, icon)| {
            let id = meta.id;
            let label = meta.label;
            let mut action = ShellAction::new(id, label(cx), icon(cx), move |window, cx| {
                duck_command::dispatch(id, window, cx);
            })
            .with_label_fn(label)
            .with_icon_fn(icon);
            if let Some(color) = meta.icon_color {
                action = action.with_icon_color(color);
            }
            // 主题选择器：点击弹出三列主题清单，不走动作处理器。
            if id == actions::ids::THEME_PICK {
                action = action.with_popover(theme_picker::popover_content);
            }
            action
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::Desktop;
    use gpui::{
        AnyWindowHandle, App, AppContext as _, Context, IntoElement, ParentElement as _, Render,
        Styled as _, TestAppContext, VisualTestContext, Window, WindowHandle, div,
    };
    use gpui_component::{Root, WindowExt as _, init as component_init, v_flex};

    /// 最简内容根：通知挂在窗口的 `Root` 上，与内容树无关。
    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex().size_full().child(div())
        }
    }

    /// 建一个挂着 `Root` 的窗口——通知（以及 dialog / sheet）都要求窗口的根视图是它。
    fn root_window(cx: &mut TestAppContext) -> WindowHandle<Root> {
        cx.add_window(|window, cx| Root::new(cx.new(|_| Blank), window, cx))
    }

    /// 某个窗口里当前的应用内通知条数。
    fn toasts(cx: &TestAppContext, handle: AnyWindowHandle) -> usize {
        VisualTestContext::from_window(handle, cx)
            .update(|window, cx| window.notifications(cx).len())
    }

    /// 走投递口推一条应用级提示（与托盘、更新检查、窗口建不起来同一条路）。
    fn push_app_toast(cx: &mut App) {
        crate::update::on_notify_window(cx, |window, cx| {
            duck_components::notify::info("首启期间的提示", window, cx);
        });
    }

    /// 应用级提示**永远不落在引导窗口上**，而是回到主窗口。
    ///
    /// 首启时引导向导是启动最末打开的窗口，macOS 上它顺手拿走焦点——`active_window()`
    /// 返回的正是它。投递口若照单全收，提示就落在那 680×520 居中小窗的右上角，也就是
    /// 「引导弹窗的右上角」，而不是程序主窗的右上角。这条用例钉住那个例外。
    #[gpui::test]
    fn a_toast_never_lands_on_the_onboarding_wizard(cx: &mut TestAppContext) {
        cx.update(|cx| {
            component_init(cx);
            duck_theme::init(cx);
        });

        let main = root_window(cx);
        let wizard = root_window(cx);
        // 把引导窗口抬成活跃窗口：这就是 `pages::onboarding::open_or_focus` 建窗后的效果。
        VisualTestContext::from_window(AnyWindowHandle::from(wizard), cx)
            .update(|window, _| window.activate_window());

        cx.update(|cx| {
            cx.set_global(Desktop::empty());
            *Desktop::onboarding_window(cx) = Some(wizard);
            push_app_toast(cx);
        });
        assert_eq!(
            toasts(cx, AnyWindowHandle::from(main)),
            1,
            "提示该落在主窗口上"
        );
        assert_eq!(
            toasts(cx, AnyWindowHandle::from(wizard)),
            0,
            "引导窗口不该承接提示"
        );

        // 对照组：撤掉登记（等于「这不是引导窗口」），同一条路径就该照常投给活跃窗口。
        // 少了这一半，上面的「0 条」也可能只是通知本来就推不出去。
        cx.update(|cx| {
            *Desktop::onboarding_window(cx) = None;
            push_app_toast(cx);
        });
        assert_eq!(
            toasts(cx, AnyWindowHandle::from(wizard)),
            1,
            "对照：撤掉登记后就照常承接"
        );
        assert_eq!(
            toasts(cx, AnyWindowHandle::from(main)),
            1,
            "主窗口那条不该被追加第二条"
        );
    }
}
