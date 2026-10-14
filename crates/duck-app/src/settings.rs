//! 应用设置与窗口几何的持久化接线。
//!
//! 三件事：把 [`duck_theme::Appearance`] 落盘 / 读盘（经 [`duck_theme::AppearanceWire`]）、
//! 窗口位置尺寸的存取、以及**窗口会话**的存取（主窗落在哪条路由、右栏是否展开、
//! 退出时开着哪些辅助窗口）。
//!
//! 分界：几何是「每窗口一行的结构化数据」，落在 `windows` 表；会话是「上次长什么样」
//! 的一小段状态，落在 `kv` 表。把会话混进 `windows` 会把那张表的语义撑散。
//!
//! 全部写入都是去抖的（见 [`duck_store`]），退出时由 `app::install` 注册的
//! `on_app_quit` 落盘 —— 不依赖 `Store::drop`，它在 `process::exit` 路径上不保证跑到。

use gpui::{App, Bounds, Pixels, Size, Window, WindowBounds, point, px, size};

use duck_components::notify;
use duck_shell::RouteId;
use duck_store::Store;
use duck_theme::{Appearance, AppearanceWire, active_theme, apply};

use crate::hotkey::HotkeySlot;
use crate::pages::{
    ASSISTANT_ROUTE, DATABASE_ROUTE, EMAIL_ROUTE, OVERVIEW_ROUTE, PROJECTS_ROUTE, REDIS_ROUTE,
    SSH_ROUTE, TOOLBOX_ROUTE,
};

/// 外观设置在 `kv` 表里的键。
const KEY_APPEARANCE: &str = duck_store::KEY_APPEARANCE;
/// 主窗口几何在 `windows` 表里的键。
const WINDOW_MAIN: &str = "main";
/// 主窗口上次落在哪条路由（`kv`）。
const KEY_ROUTE: &str = "window_route";
/// 主窗口活跃路由的右栏上次是否展开（`kv`）。
const KEY_RIGHT_SIDEBAR: &str = "window_right_sidebar_open";
/// 上次退出时开着的辅助窗口（`kv`，JSON 数组）。
const KEY_AUX_OPEN: &str = "window_aux_open";
/// 「启动时恢复上次窗口」开关（`kv`；缺省**关**）。
const KEY_RESTORE_WINDOWS: &str = "restore_windows_on_launch";
/// 「唤起快捷键顺带打开主窗口」开关（`kv`；缺省**关**）。
const KEY_LAUNCHER_MAIN_WINDOW: &str = "launcher_opens_main_window";

/// 「卸载插件前弹确认」在 `kv` 表里的键（设置 → 行为）。
const KEY_CONFIRM_UNINSTALL: &str = "confirm_uninstall";

/// 最近若干次「进程启动 → 主窗就绪」的耗时（毫秒，JSON 数组，新值在前）。
const KEY_STARTUP_MS: &str = "startup_durations_ms";
/// 启动耗时保留的样本数：够看趋势，又不至于让一处老慢数据赖着不走。
const STARTUP_SAMPLES: usize = 10;

/// 全部**已登记**路由的稳定 id（存档校验用）。
///
/// 直接引用路由常量而不是抄一份字面量：路由增删时这里自动跟上，而存档里的旧 id
/// 也就自动变成「不认识」→ 回落首页，不会把窗口开到空白页。清单必须与
/// `app::build_main_window` 登记的 `ShellRoute` 保持一致（含功能占位路由）。
const KNOWN_ROUTES: [RouteId; 8] = [
    OVERVIEW_ROUTE,
    TOOLBOX_ROUTE,
    SSH_ROUTE,
    DATABASE_ROUTE,
    REDIS_ROUTE,
    PROJECTS_ROUTE,
    ASSISTANT_ROUTE,
    EMAIL_ROUTE,
];

// ── 外观 ────────────────────────────────────────────────────────────────────

/// 从磁盘恢复外观设置；没有存档或解析失败时返回 `false`（保持默认外观）。
pub fn load_appearance(cx: &mut App) -> bool {
    let Some(json) = Store::global(cx).and_then(|store| store.kv_get(KEY_APPEARANCE)) else {
        return false;
    };
    match serde_json::from_str::<AppearanceWire>(&json) {
        Ok(wire) => {
            apply(Appearance::from_wire(wire), cx);
            true
        }
        Err(error) => {
            tracing::warn!(%error, "failed to parse appearance, falling back to defaults");
            false
        }
    }
}

/// 把当前外观设置写入存储（去抖，异步落盘）。
pub fn save_appearance(cx: &App) {
    let Some(store) = Store::global(cx) else {
        return;
    };
    let wire = active_theme(cx).appearance().to_wire();
    match serde_json::to_string(&wire) {
        Ok(json) => store.kv_put(KEY_APPEARANCE, json),
        Err(error) => tracing::warn!(%error, "failed to serialize appearance"),
    }
}

// ── 窗口几何 ────────────────────────────────────────────────────────────────

/// 窗口的显示状态（会话恢复用）。
///
/// 只存 `x/y/width/height` 会把「最大化着退出」记成「一个占满屏幕的普通窗口」：
/// 下次启动后它不再随屏幕变化自适应，也不能用双击标题栏还原。
///
/// 判据取自 `Window::is_maximized()` / `is_fullscreen()`（原因见 [`save_bounds`]）。
/// macOS 上 `is_maximized()` 是**尺寸相等**的比较，所以「手动把窗口拉到正好铺满屏幕」
/// 也会被当作最大化 —— 两者在恢复时表现一致（都铺满），不值得为此加区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowState {
    /// 普通窗口。
    Windowed,
    /// 最大化（macOS 的「缩放」）。
    Maximized,
    /// 全屏。
    Fullscreen,
}

impl WindowState {
    /// 存档里的标签。
    const fn label(self) -> &'static str {
        match self {
            Self::Windowed => "windowed",
            Self::Maximized => "maximized",
            Self::Fullscreen => "fullscreen",
        }
    }

    /// 从存档标签还原。认不出的标签按普通窗口处理 —— 存档可能来自旧版本。
    fn parse(value: &str) -> Self {
        match value {
            "maximized" => Self::Maximized,
            "fullscreen" => Self::Fullscreen,
            _ => Self::Windowed,
        }
    }

    /// 拼回 gpui 的窗口选项；`Maximized` / `Fullscreen` 里的矩形是「还原尺寸」。
    const fn to_window_bounds(self, bounds: Bounds<Pixels>) -> WindowBounds {
        match self {
            Self::Windowed => WindowBounds::Windowed(bounds),
            Self::Maximized => WindowBounds::Maximized(bounds),
            Self::Fullscreen => WindowBounds::Fullscreen(bounds),
        }
    }
}

/// 读某个窗口的存档几何与显示状态；没有存档或内容坏了时为 `None`。
#[must_use]
fn load_saved(cx: &App, key: &str) -> Option<(Bounds<Pixels>, WindowState)> {
    let json = Store::global(cx)?.window_get(key)?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let read = |field: &str| value.get(field).and_then(serde_json::Value::as_f64);
    let bounds = Bounds::new(
        point(px(read("x")? as f32), px(read("y")? as f32)),
        size(px(read("width")? as f32), px(read("height")? as f32)),
    );
    // 旧存档没有 `state` 字段：那时还没有记录显示状态，按普通窗口处理。
    let state = value
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map_or(WindowState::Windowed, WindowState::parse);
    Some((bounds, state))
}

/// 记录某个窗口的几何与显示状态（去抖）。
fn save_bounds(key: &str, window: &Window, cx: &App) {
    let Some(store) = Store::global(cx) else {
        return;
    };
    let bounds = window.bounds();
    // 注意：macOS 的 `window_bounds()` **永远不报 `Maximized`** —— 它只区分全屏与普通
    // （见 `gpui_macos::window_bounds`）。所以最大化必须问 `is_maximized()`，别图省事
    // 改回解析 `WindowBounds`：那样最大化会被静默记成普通窗口，下次启动就丢了。
    let state = if window.is_fullscreen() {
        WindowState::Fullscreen
    } else if window.is_maximized() {
        WindowState::Maximized
    } else {
        WindowState::Windowed
    };
    let json = serde_json::json!({
        "x": f32::from(bounds.origin.x),
        "y": f32::from(bounds.origin.y),
        "width": f32::from(bounds.size.width),
        "height": f32::from(bounds.size.height),
        "state": state.label(),
    })
    .to_string();
    store.window_put(key, json, None);
}

/// 主窗口的存档窗口选项；没有存档时为 `None`（调用方取默认几何）。
///
/// 几何先过 [`clamp_bounds`]：存档里的坐标可能落在已经不在的显示器上。
#[must_use]
pub fn restore_main_window(cx: &App) -> Option<WindowBounds> {
    let (bounds, state) = load_saved(cx, WINDOW_MAIN)?;
    Some(state.to_window_bounds(clamp_bounds(bounds, cx)))
}

/// 记录主窗口几何与显示状态（去抖）。
pub fn save_window_bounds(window: &Window, cx: &App) {
    save_bounds(WINDOW_MAIN, window, cx);
}

/// 「随手看」悬浮窗在 `windows` 表里的键。
///
/// 与辅助窗口分开一格（它们共用 `window_aux_open` 那份**会话**记录，但几何还是各存各的）：
/// 悬浮窗不在「上次开着哪些窗口」的恢复列表里（缺省不自动现身，见「不打扰」原则），
/// 但它一旦被打开就该回到上次待的地方。
const WINDOW_TASK_FLOAT: &str = "task_float";

/// 悬浮窗上次待的位置；没有存档时为 `None`（调用方取默认位置）。
///
/// 只取**原点**：悬浮窗不可缩放（尺寸是常量），也没有全屏 / 最大化这回事 ——
/// 存档里那几个字段是 [`save_bounds`] 顺带写的，回读时不该当真。
/// 位置同样过 [`clamp_bounds`]：外接屏拔掉之后不能把它丢在屏幕外。
#[must_use]
pub fn restore_task_float(cx: &App) -> Option<gpui::Point<Pixels>> {
    let (bounds, _state) = load_saved(cx, WINDOW_TASK_FLOAT)?;
    Some(clamp_bounds(bounds, cx).origin)
}

/// 记录悬浮窗位置（去抖）。
///
/// 拖动时会被连续调用：`window_put` 本身有 400ms 去抖（见 `duck-store`），
/// 主窗走的是同一条路。
pub fn save_task_float_bounds(window: &Window, cx: &App) {
    save_bounds(WINDOW_TASK_FLOAT, window, cx);
}

// ── 显示器夹取 ──────────────────────────────────────────────────────────────

/// 窗口至少要有这么大比例的面积落在某块屏幕内，才算「用户看得见」。
///
/// 取 1/4 是有意的：它把「窗口整体跑到屏幕外」与「用户刻意把窗口停成一部分探出屏幕」
/// 分开。后者是**合法的桌面布局**（macOS 本来也允许），把它拉回来等于每次启动都替
/// 用户挪窗口 —— 实测里正是这条规则先咬到了自己：一个停在 (553,134) 的窗口
/// （右、下各探出一点）被「纠正」成 (312,92) 并写了回去。
const MIN_VISIBLE_FRACTION: f32 = 0.25;

/// 把窗口几何夹回可见区域。
///
/// 只在窗口**基本看不见**时才动它 —— 外接屏拔掉、分辨率调小、显示器重排会让存档坐标
/// 落在任何屏幕之外，那时用户看到的是「应用启动了但没窗口」，比启动失败更难排查。
///
/// 动的时候优先塞回**重叠面积最大**的那块屏（保留用户原本的相对位置）；一块都不挨着
/// 就用主屏。看得见就原样返回，见 [`MIN_VISIBLE_FRACTION`]。
#[must_use]
pub fn clamp_bounds(bounds: Bounds<Pixels>, cx: &App) -> Bounds<Pixels> {
    let displays: Vec<Bounds<Pixels>> = cx
        .displays()
        .iter()
        .map(|display| display.visible_bounds())
        .collect();
    if displays.is_empty() {
        // 拿不到显示器信息（无头 / 平台不支持）：原样返回，不要凭空猜。
        return bounds;
    }
    let primary = cx.primary_display().map(|display| display.visible_bounds());
    clamp_to_displays(bounds, &displays, primary)
}

/// 夹取的纯几何核心（与 gpui 解耦，便于单测）。
///
/// `displays` 是各显示器**可见区域**；`primary` 是「完全不在任何屏幕内」时的落点。
#[must_use]
pub fn clamp_to_displays(
    bounds: Bounds<Pixels>,
    displays: &[Bounds<Pixels>],
    primary: Option<Bounds<Pixels>>,
) -> Bounds<Pixels> {
    let Some(first) = displays.first().copied() else {
        return bounds;
    };
    let (visible, host) = displays
        .iter()
        .map(|area| (overlap_area(bounds, *area), *area))
        .max_by(|(left, _), (right, _)| {
            left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((0., first));
    let area = f32::from(bounds.size.width) * f32::from(bounds.size.height);
    if visible > 0. && visible >= area * MIN_VISIBLE_FRACTION {
        return bounds;
    }
    let target = if visible > 0. {
        host
    } else {
        primary.unwrap_or(first)
    };
    fit_into(bounds, target)
}

/// 两块矩形的重叠面积；不相交为 `0`。
fn overlap_area(left: Bounds<Pixels>, right: Bounds<Pixels>) -> f32 {
    let width = (left.origin.x + left.size.width).min(right.origin.x + right.size.width)
        - left.origin.x.max(right.origin.x);
    let height = (left.origin.y + left.size.height).min(right.origin.y + right.size.height)
        - left.origin.y.max(right.origin.y);
    if width <= px(0.) || height <= px(0.) {
        0.
    } else {
        f32::from(width) * f32::from(height)
    }
}

/// 把 `bounds` 塞进 `area`：尺寸先收到放得下，再把左上角推回范围内。
fn fit_into(bounds: Bounds<Pixels>, area: Bounds<Pixels>) -> Bounds<Pixels> {
    let size = size(
        bounds.size.width.min(area.size.width),
        bounds.size.height.min(area.size.height),
    );
    let max_x = area.origin.x + area.size.width - size.width;
    let max_y = area.origin.y + area.size.height - size.height;
    Bounds::new(
        point(
            bounds.origin.x.max(area.origin.x).min(max_x),
            bounds.origin.y.max(area.origin.y).min(max_y),
        ),
        size,
    )
}

// ── 辅助窗口 ────────────────────────────────────────────────────────────────

/// 会被记住几何 / 会被恢复的辅助窗口。
///
/// 引导窗口**不在其中**：它是一次性流程，记住几何没有意义，重开更是打扰。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuxWindow {
    /// 设置。
    Settings,
    /// 日志。
    Logs,
    /// 插件中心。
    PluginCenter,
    /// 开源许可。
    Licenses,
}

impl AuxWindow {
    /// 全部辅助窗口。顺序固定 —— 「上次开着哪些窗口」的存档顺序依赖它。
    pub const ALL: [Self; 4] = [
        Self::Settings,
        Self::Logs,
        Self::PluginCenter,
        Self::Licenses,
    ];

    /// `windows` 表里的键。
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Logs => "logs",
            Self::PluginCenter => "plugin-center",
            Self::Licenses => "licenses",
        }
    }
}

/// 辅助窗口的存档窗口选项；没有存档时为 `None`（调用方取默认几何）。
#[must_use]
pub fn restore_aux_window(cx: &App, aux: AuxWindow) -> Option<WindowBounds> {
    let (bounds, state) = load_saved(cx, aux.key())?;
    Some(state.to_window_bounds(clamp_bounds(bounds, cx)))
}

/// 记录辅助窗口几何与显示状态（去抖）。
pub fn save_aux_bounds(aux: AuxWindow, window: &Window, cx: &App) {
    save_bounds(aux.key(), window, cx);
}

/// 辅助窗口的开窗几何：有存档就用存档（已夹回可见区域），否则按 `fallback`
/// 在主屏居中。四个辅助窗口共用这一条，免得各自抄一遍「存档 or 居中」。
#[must_use]
pub fn aux_window_bounds(cx: &App, aux: AuxWindow, fallback: Size<Pixels>) -> WindowBounds {
    restore_aux_window(cx, aux)
        .unwrap_or_else(|| WindowBounds::Windowed(Bounds::centered(None, fallback, cx)))
}

/// 上次退出时开着的辅助窗口（按 [`AuxWindow::ALL`] 的顺序）。
///
/// 存档里认不出的键直接丢弃 —— 那多半是旧版本留下的窗口名。
#[must_use]
pub fn load_aux_open(cx: &App) -> Vec<AuxWindow> {
    let Some(stored) = Store::global(cx).and_then(|store| store.kv_get(KEY_AUX_OPEN)) else {
        return Vec::new();
    };
    let Ok(keys) = serde_json::from_str::<Vec<String>>(&stored) else {
        tracing::warn!("failed to parse the remembered auxiliary windows; ignoring");
        return Vec::new();
    };
    AuxWindow::ALL
        .into_iter()
        .filter(|aux| keys.iter().any(|key| key == aux.key()))
        .collect()
}

/// 记录当前开着的辅助窗口（去抖）。
pub fn save_aux_open(open: &[AuxWindow], cx: &App) {
    let keys: Vec<&str> = open.iter().map(|aux| aux.key()).collect();
    match serde_json::to_string(&keys) {
        Ok(json) => duck_store::kv_put(cx, KEY_AUX_OPEN, json),
        Err(error) => tracing::warn!(%error, "failed to serialize the open auxiliary windows"),
    }
}

/// 「启动时恢复上次窗口」是否开启；缺省为关。
#[must_use]
pub fn restore_windows_on_launch(cx: &App) -> bool {
    Store::global(cx)
        .and_then(|store| store.kv_get(KEY_RESTORE_WINDOWS))
        .is_some_and(|value| value == "true")
}

/// 设置「启动时恢复上次窗口」（去抖）。
pub fn set_restore_windows_on_launch(cx: &App, on: bool) {
    duck_store::kv_put(cx, KEY_RESTORE_WINDOWS, if on { "true" } else { "false" });
}

/// 「唤起快捷键顺带打开主窗口」是否开启；缺省为**关**。
///
/// 缺省关是有意的：唤起键是 `⌥Space`，紧挨 `⌘Space`（Spotlight），这个键位自带
/// **启动器**语义 —— 按它期待的是一个浮层盖在当前工作之上，而不是把 Duck 整个切到
/// 前台。把主窗也叫出来是**旧行为**，留给「我把 Duck 当工具箱切来切去」这种用法。
#[must_use]
pub fn launcher_opens_main_window(cx: &App) -> bool {
    Store::global(cx)
        .and_then(|store| store.kv_get(KEY_LAUNCHER_MAIN_WINDOW))
        .is_some_and(|value| value == "true")
}

/// 设置「唤起快捷键顺带打开主窗口」（去抖）。
pub fn set_launcher_opens_main_window(cx: &App, on: bool) {
    duck_store::kv_put(
        cx,
        KEY_LAUNCHER_MAIN_WINDOW,
        if on { "true" } else { "false" },
    );
}

// ── 全局唤起快捷键 ──────────────────────────────────────────────────────────

/// 某个槽位的「全局唤起快捷键」是否开启；缺省为**开**。
///
/// 缺省开是有意的：这些键存在的意义就是「随手把 Duck 叫出来」，默认关掉等于
/// 让这个能力永远没人发现。代价是第一次运行时确实会占掉几个组合键 —— 所以
/// 设置页那一行会显示它**到底登记上没有**，登记不上还会弹一次提示（见 `main.rs`）。
///
/// 槽位与 `kv` 键名的对应见 [`HotkeySlot::keys`]（键名不是三处各写一遍的）。
#[must_use]
pub fn hotkey_enabled(cx: &App, slot: HotkeySlot) -> bool {
    let (enabled_key, _, _) = slot.keys();
    Store::global(cx)
        .and_then(|store| store.kv_get(enabled_key))
        .is_none_or(|value| value == "true")
}

/// 设置某个槽位的「全局唤起快捷键」开关（去抖）。
pub fn set_hotkey_enabled(cx: &App, slot: HotkeySlot, on: bool) {
    let (enabled_key, _, _) = slot.keys();
    duck_store::kv_put(cx, enabled_key, if on { "true" } else { "false" });
}

/// 某个槽位的热键规范串；没有存档（或存档读不出）时为 `None`。
///
/// 只做「读出来」这一步，**不在这里解析**：解析（与逐条错误）在
/// [`duck_platform::Hotkey::parse`]，调用方拿到 `None` 时回落到该槽位的默认串再解析。
/// 这样「存档坏了」的表现是「回到默认键位」，而不是「功能静默失效」。
#[must_use]
pub fn hotkey_spec(cx: &App, slot: HotkeySlot) -> Option<String> {
    let (_, spec_key, _) = slot.keys();
    let stored = Store::global(cx)?.kv_get(spec_key)?;
    let stored = stored.trim().to_owned();
    (!stored.is_empty()).then_some(stored)
}

/// 记录某个槽位的热键规范串（去抖）。
pub fn set_hotkey_spec(cx: &App, slot: HotkeySlot, spec: &str) {
    let (_, spec_key, _) = slot.keys();
    duck_store::kv_put(cx, spec_key, spec);
}

/// 某个槽位已经把「登记不上」提示过的键位串。
///
/// 同一个键位每次启动都失败只提示一次 —— 每次开机弹同一句是打扰；但换了个键位
/// 又失败，就值得再说一次（用户刚改过什么，正是最容易对上号的时候）。
#[must_use]
pub fn hotkey_announced(cx: &App, slot: HotkeySlot) -> Option<String> {
    let (_, _, announced_key) = slot.keys();
    Store::global(cx)?.kv_get(announced_key)
}

/// 记下「这个键位登记不上，已经提示过了」（去抖）。
pub fn set_hotkey_announced(cx: &App, slot: HotkeySlot, spec: &str) {
    let (_, _, announced_key) = slot.keys();
    duck_store::kv_put(cx, announced_key, spec);
}

// ── 卸载确认 ────────────────────────────────────────────────────────────────

/// 「卸载插件前弹确认」是否开启；缺省为**开**。
///
/// 缺省开：卸载会把插件目录删掉（[`crate::market::uninstall_plugin`]），误点一次就得
/// 重新去市场装回来，而多一次确认的代价只是再点一下。想连点卸载的人可以关掉它。
#[must_use]
pub fn confirm_uninstall(cx: &App) -> bool {
    Store::global(cx)
        .and_then(|store| store.kv_get(KEY_CONFIRM_UNINSTALL))
        .is_none_or(|value| value != "false")
}

/// 设置「卸载插件前弹确认」（去抖）。
pub fn set_confirm_uninstall(cx: &App, on: bool) {
    duck_store::kv_put(cx, KEY_CONFIRM_UNINSTALL, if on { "true" } else { "false" });
}

// ── 通知通道 ────────────────────────────────────────────────────────────────

/// 应用内横幅的抑制档位（`kv`；缺省「全部」）。
const KEY_NOTIFY_BANNERS: &str = "notify_banners";
/// 是否投递系统通知（`kv`；缺省**开**）。
const KEY_NOTIFY_SYSTEM: &str = "notify_system";

/// 读取通知通道设置。
///
/// **这是唯一的事实来源**：`kv` 里的值经 [`apply_notify_channels`] 投影给
/// `duck_components::notify` 的 App 级全局，通知模块自己不碰存储（它不依赖 `duck_store`）。
#[must_use]
pub fn notify_channels(cx: &App) -> notify::Channels {
    let value = Store::global(cx).map(|store| {
        let banners = store
            .kv_get(KEY_NOTIFY_BANNERS)
            .and_then(|value| notify::BannerLevel::from_wire(&value))
            .unwrap_or_default();
        let system = store
            .kv_get(KEY_NOTIFY_SYSTEM)
            .is_none_or(|value| value != "false");
        notify::Channels { banners, system }
    });
    value.unwrap_or_default()
}

/// 把 `kv` 里的通知通道设置推给通知模块。
///
/// **启动时要调一次**（`main.rs`）：不调的话，启动早期生产的那几条通知（崩溃提示、
/// 插件清单刷新失败……）会按默认档全弹，用户明明关过。
pub fn apply_notify_channels(cx: &mut App) {
    let channels = notify_channels(cx);
    notify::set_channels(channels, cx);
}

/// 设置应用内横幅档位（立刻生效 + 去抖落盘）。
///
/// **不从 `kv` 回读新值**：`kv_put` 是去抖落盘的（400ms，见 `duck_store::DEBOUNCE`），
/// 紧接着 `kv_get` 拿到的还是旧值 —— 那样这一档要等下一次落盘才生效，中间设置页
/// 显示的又是旧档，用户看到的就是「点了没反应」。所以运行时那份从
/// [`notify::channels`]（当前生效值）出发只改这一位；`kv` 只负责让它活过重启。
pub fn set_notify_banners(cx: &mut App, banners: notify::BannerLevel) {
    duck_store::kv_put(cx, KEY_NOTIFY_BANNERS, banners.wire());
    let mut channels = notify::channels(cx);
    channels.banners = banners;
    notify::set_channels(channels, cx);
}

/// 设置是否投递系统通知（立刻生效 + 去抖落盘；理由同 [`set_notify_banners`]）。
pub fn set_notify_system(cx: &mut App, on: bool) {
    duck_store::kv_put(cx, KEY_NOTIFY_SYSTEM, if on { "true" } else { "false" });
    let mut channels = notify::channels(cx);
    channels.system = on;
    notify::set_channels(channels, cx);
}

// ── 启动耗时 ────────────────────────────────────────────────────────────────

/// 记一次启动耗时（毫秒），保留最近 [`STARTUP_SAMPLES`] 次。
///
/// 概览页那张「平均启动耗时」卡就靠它 —— 与其摆一个编出来的数字，不如从第一次启动
/// 起就没记录，攒够样本再说。读数只写不读别的用途，所以不做去抖：一次启动只写一次。
pub fn record_startup(cx: &App, millis: u64) {
    let mut samples = startup_samples(cx);
    samples.insert(0, millis);
    samples.truncate(STARTUP_SAMPLES);
    if let Ok(json) = serde_json::to_string(&samples) {
        duck_store::kv_put(cx, KEY_STARTUP_MS, json);
    }
}

/// 启动耗时统计：`(样本数, 平均毫秒)`；一次都没记录过时返回 `None`。
#[must_use]
pub fn startup_stats(cx: &App) -> Option<(usize, u64)> {
    let samples = startup_samples(cx);
    if samples.is_empty() {
        return None;
    }
    // 样本数在这里必然 ≥ 1（上面刚判过空），`try_from` 的兜底值取不到。
    let count = u64::try_from(samples.len()).unwrap_or(1);
    Some((samples.len(), samples.iter().sum::<u64>() / count))
}

/// 读回启动耗时样本；存档缺失或坏掉时按「没有样本」处理。
fn startup_samples(cx: &App) -> Vec<u64> {
    Store::global(cx)
        .and_then(|store| store.kv_get(KEY_STARTUP_MS))
        .and_then(|json| serde_json::from_str::<Vec<u64>>(&json).ok())
        .unwrap_or_default()
}

// ── 主窗口会话 ──────────────────────────────────────────────────────────────

/// 上次退出时主窗口所在的路由；没有记录或 id 已失效时为 `None`。
#[must_use]
pub fn load_window_route(cx: &App) -> Option<&'static str> {
    let stored = Store::global(cx)?.kv_get(KEY_ROUTE)?;
    KNOWN_ROUTES
        .into_iter()
        .find(|id| id.as_str() == stored)
        .map(RouteId::as_str)
}

/// 记录主窗口当前路由（去抖）。
pub fn save_window_route(route: &'static str, cx: &App) {
    duck_store::kv_put(cx, KEY_ROUTE, route);
}

/// 上次退出时主窗口右栏是否展开；没有记录时为 `None`（保持默认收起）。
#[must_use]
pub fn load_right_sidebar_open(cx: &App) -> Option<bool> {
    Store::global(cx)?
        .kv_get(KEY_RIGHT_SIDEBAR)
        .and_then(|value| match value.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
}

/// 记录主窗口活跃路由的右栏展开态（去抖）。
pub fn save_right_sidebar_open(open: bool, cx: &App) {
    duck_store::kv_put(cx, KEY_RIGHT_SIDEBAR, if open { "true" } else { "false" });
}

#[cfg(test)]
mod tests {
    use super::{clamp_to_displays, overlap_area, set_notify_banners, set_notify_system};
    use duck_components::notify;
    use gpui::{Bounds, Pixels, TestAppContext, point, px, size};

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn overlap_area_only_counts_real_intersections() {
        assert!(overlap_area(rect(0., 0., 10., 10.), rect(5., 5., 10., 10.)) > 0.);
        // 只在边界上相接不算重叠：否则「贴着屏幕边缘」会被当成屏幕内。
        assert_eq!(
            overlap_area(rect(0., 0., 10., 10.), rect(10., 0., 10., 10.)),
            0.
        );
        assert_eq!(
            overlap_area(rect(0., 0., 10., 10.), rect(20., 20., 5., 5.)),
            0.
        );
        assert_eq!(
            overlap_area(rect(0., 0., 10., 10.), rect(2., 2., 4., 4.)),
            16.
        );
    }

    /// 窗口被拖到屏幕外一点点——**只要还看得见就别动它**。
    ///
    /// 这是实测里踩到的那个位置：屏幕上 1512×982、可见区下沿 892，窗口停在
    /// (553,134) 时右下各探出一点（1753 > 1512、934 > 892）。系统允许这么停，
    /// 用户的意图就该被保留 —— 早期版本会把它「纠正」成 (312,92)。
    #[test]
    fn a_window_still_visible_keeps_its_partially_off_screen_spot() {
        let display = rect(0., 33., 1512., 859.);
        let bounds = rect(553., 134., 1200., 800.);
        assert_eq!(clamp_to_displays(bounds, &[display], Some(display)), bounds);
    }

    /// 被挤到只剩一条边（外接屏拔掉后常见的残影位置）就必须救回来，只动越界的那一轴。
    #[test]
    fn a_window_reduced_to_a_sliver_is_pulled_back() {
        let display = rect(0., 33., 1512., 859.);
        let fixed = clamp_to_displays(rect(1450., 50., 1200., 800.), &[display], Some(display));
        assert_eq!(f32::from(fixed.origin.x), 312., "右侧被推回屏内");
        assert_eq!(f32::from(fixed.origin.y), 50., "没越界的那一轴不要动");
    }

    /// 在屏幕内的窗口必须原样保留 —— 夹取不能变成「每次都挪一下」。
    #[test]
    fn a_window_fully_inside_is_left_alone() {
        let display = rect(0., 0., 1440., 900.);
        let fixed = clamp_to_displays(rect(300., 200., 800., 600.), &[display], Some(display));
        assert_eq!(fixed, rect(300., 200., 800., 600.));
    }

    /// 外接屏被拔掉：存档坐标落在任何屏幕之外 → 落到主屏上，尺寸不变。
    #[test]
    fn a_window_on_a_removed_display_lands_on_the_primary() {
        let primary = rect(0., 33., 1512., 859.);
        let fixed = clamp_to_displays(rect(2000., 300., 900., 700.), &[primary], Some(primary));
        assert!(overlap_area(fixed, primary) > 0., "必须落到还存在的屏幕上");
        assert_eq!(f32::from(fixed.size.width), 900.);
        assert_eq!(f32::from(fixed.size.height), 700., "放得下就别改尺寸");
    }

    /// 救回来时按重叠面积挑归属屏，而不是「列表里第一个碰到的」。
    #[test]
    fn the_display_with_the_largest_overlap_wins() {
        let left = rect(0., 0., 1440., 900.);
        let right = rect(1440., 0., 1920., 1080.);
        // 又高又宽，横跨两屏且整体露出极少 —— 右边那屏重叠远大于左边。
        let fixed = clamp_to_displays(rect(1300., 300., 1200., 4000.), &[left, right], Some(left));
        assert_eq!(f32::from(fixed.origin.x), 1440., "应当被拉进重叠更多的右屏");
        assert_eq!(overlap_area(fixed, left), 0.);
    }

    /// 比屏幕还大的存档尺寸要收小，否则「推回范围内」和「放得下」会互相打架。
    #[test]
    fn an_oversized_window_shrinks_to_fit() {
        let display = rect(0., 0., 1280., 800.);
        let fixed = clamp_to_displays(rect(0., 0., 4000., 3000.), &[display], Some(display));
        assert_eq!(f32::from(fixed.size.width), 1280.);
        assert_eq!(f32::from(fixed.size.height), 800.);
        assert_eq!(f32::from(fixed.origin.x), 0.);
    }

    /// 一块屏幕都拿不到时不做任何猜测（无头 / 平台不支持）。
    #[test]
    fn with_no_displays_the_bounds_survive_untouched() {
        let bounds = rect(300., 200., 800., 600.);
        assert_eq!(clamp_to_displays(bounds, &[], None), bounds);
    }

    /// 改通知档位必须**立刻**生效，不能等 `kv` 去抖落盘（400ms）。
    ///
    /// 这是真踩过的坑：早先的写法是「`kv_put` 之后调 `apply_notify_channels`」，而后者
    /// 从 `kv` 回读 —— 去抖窗口里读到的还是旧值，于是运行时档位压根没变，设置页下一次
    /// 重绘又弹回旧档，用户看到的就是「点了没反应」。这里**连 `Store` 都不装**，专门钉住
    /// 「不依赖存储也要立刻生效」这条契约。
    #[gpui::test]
    fn changing_the_banner_level_takes_effect_without_the_store(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert_eq!(
                notify::channels(cx).banners,
                notify::BannerLevel::All,
                "缺省全开"
            );

            set_notify_banners(cx, notify::BannerLevel::WarningsAndErrors);
            assert_eq!(
                notify::channels(cx).banners,
                notify::BannerLevel::WarningsAndErrors
            );

            set_notify_banners(cx, notify::BannerLevel::Off);
            assert_eq!(notify::channels(cx).banners, notify::BannerLevel::Off);
        });
    }

    /// 两条通道各改各的：改横幅档位不该动系统开关，反之亦然。
    #[gpui::test]
    fn the_two_channels_do_not_drag_each_other(cx: &mut TestAppContext) {
        cx.update(|cx| {
            set_notify_banners(cx, notify::BannerLevel::Off);
            set_notify_system(cx, false);
            assert!(!notify::channels(cx).system);

            set_notify_banners(cx, notify::BannerLevel::All);
            assert_eq!(notify::channels(cx).banners, notify::BannerLevel::All);
            assert!(!notify::channels(cx).system, "改横幅档位不该把系统通道带开");

            set_notify_system(cx, true);
            assert!(notify::channels(cx).system);
            assert_eq!(
                notify::channels(cx).banners,
                notify::BannerLevel::All,
                "改系统开关不该把横幅档位带走"
            );
        });
    }
}
