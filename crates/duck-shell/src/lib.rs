//! Duck Toolbox 桌面窗口外壳。
//!
//! 本 crate 只提供窗口 chrome、活动栏、路由和内容槽位；业务页面由 app 创建后以
//! [`ShellRoute`] 注入。结构对应 FluxDown `fluxdown_ui_shell`。

// 模块名刻意不叫 `aux`：`aux` 是 Windows 保留设备名（与 `CON` / `NUL` / `COM1` 同类），
// 文件一旦叫 `aux.rs`，Windows 上 `git checkout` 直接报
// `error: invalid path 'crates/duck-shell/src/aux.rs'`，CI 的 Windows 腿在**检出阶段**
// 就红，而本地 macOS 全程无感。现在文件名与对外 API 名（`auxiliary_window_options` /
// `AuxiliaryWindowView`）也对齐了。守卫见 `duck-platform/tests/windows_safe_paths.rs`。
mod auxiliary;
mod notifications;
mod palette_window;
mod view;

pub use auxiliary::*;
pub use palette_window::*;

use duck_theme::active_theme;
use gpui::{
    App, Pixels, Point, SharedString, Window, WindowDecorations, WindowOptions, point, px, size,
};
use gpui_component::TitleBar;

pub use view::*;

/// 交通灯对齐的顶栏高度（逻辑像素）：窗口创建时写入平台窗口选项，是窗口级而非主题值。
const TITLE_BAR_HEIGHT_PX: f32 = 40.;
/// macOS 交通灯按钮框高度（AppKit 标准窗口按钮 14×16 的高）。
const TRAFFIC_LIGHT_BUTTON_HEIGHT_PX: f32 = 16.;
/// 交通灯纵向偏移：按钮框在顶栏内垂直居中；横向与纵向留白一致。
const TRAFFIC_LIGHT_INSET_PX: f32 = (TITLE_BAR_HEIGHT_PX - TRAFFIC_LIGHT_BUTTON_HEIGHT_PX) / 2.;

/// 交通灯在 [`TITLE_BAR_HEIGHT_PX`] 高的标题栏内垂直居中的位置。
fn traffic_light_position() -> Point<Pixels> {
    point(px(TRAFFIC_LIGHT_INSET_PX), px(TRAFFIC_LIGHT_INSET_PX))
}

/// 标题栏内控件（`density.control` 高）上下各留的空白：默认 28 + 6 + 6 恰为 40。
const TITLE_BAR_CONTROL_PADDING_PX: f32 = 6.;

/// shell 窗口默认最小尺寸：辅助窗口（设置 / 日志 / 插件中心）各自覆盖，主窗口沿用此值。
const WINDOW_MIN_SIZE: gpui::Size<Pixels> = size(px(720.), px(520.));

/// 标题栏实际高度：不低于 `density.title_bar`，文字放大使 `density.control` 变高时随之撑高。
#[must_use]
pub fn title_bar_height(cx: &App) -> Pixels {
    let density = active_theme(cx).density();
    density
        .title_bar
        .max(density.control + px(TITLE_BAR_CONTROL_PADDING_PX * 2.))
}

/// 交通灯纵向偏移随标题栏高度居中（macOS）；`applied` 记录已应用的值，未变化时不触碰 AppKit。
#[cfg(target_os = "macos")]
pub(crate) fn sync_traffic_light(window: &Window, height: Pixels, applied: &mut Pixels) {
    let inset_y = (height - px(TRAFFIC_LIGHT_BUTTON_HEIGHT_PX)) / 2.;
    if *applied != inset_y {
        *applied = inset_y;
        window.set_traffic_light_position(point(px(TRAFFIC_LIGHT_INSET_PX), inset_y));
    }
}

/// 非 macOS 无交通灯，无需同步。
#[cfg(not(target_os = "macos"))]
pub(crate) fn sync_traffic_light(_window: &Window, _height: Pixels, _applied: &mut Pixels) {}

/// 交通灯初始纵向偏移（默认 40px 标题栏下的居中值）。
pub(crate) const fn initial_traffic_light_y() -> Pixels {
    px(TRAFFIC_LIGHT_INSET_PX)
}

/// 以 gpui-component `TitleBar` 窗口选项为基础，交通灯改为对齐 shell 标题栏高度。
fn shell_window_options() -> WindowOptions {
    let mut options = TitleBar::window_options();
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.traffic_light_position = Some(traffic_light_position());
    }
    options.window_min_size = Some(WINDOW_MIN_SIZE);
    options.window_decorations = Some(WindowDecorations::Client);
    options
}

/// 产品名（品牌）：主窗口系统级标题与顶栏品牌区共用，不随语言变化。
const MAIN_WINDOW_TITLE: &str = "Duck Toolbox";

/// 构造主窗口选项。
#[must_use]
pub fn main_window_options() -> WindowOptions {
    auxiliary_window_options(MAIN_WINDOW_TITLE)
}

/// 构造使用 Duck 自定义标题栏的辅助窗口选项。
#[must_use]
pub fn auxiliary_window_options(title: impl Into<SharedString>) -> WindowOptions {
    let mut options = shell_window_options();
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some(title.into());
    }
    options
}
