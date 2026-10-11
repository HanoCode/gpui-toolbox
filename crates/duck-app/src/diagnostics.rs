//! 诊断：把「出了问题从哪儿查」收口到一处。
//!
//! 平台层负责**如实记录**崩溃（[`duck_platform::install_panic_hook`]），本模块负责
//! **解读**它：把累计崩溃次数与本地水位线（`kv` 的 `crash_seen`）比较，得出有没有
//! 用户还没看过的崩溃，并在启动时提示一次。
//!
//! 水位线放 `kv` 而不是平台层：平台层不依赖持久层（见 `duck-platform` 的 crate 文档），
//! 而「看没看过」本来就是应用级状态。

use std::path::PathBuf;

use duck_components::notify::{self, Tone};
use duck_theme::{tr, trf};
use gpui::App;

/// 「已查看过的崩溃数」水位线（`kv`）。
pub const KEY_CRASH_SEEN: &str = "crash_seen";

/// 启动提示的键：同键替换，反复启动不会堆一列（见 `notify::sticky` 的语义）。
const KEY_CRASH_NOTICE: &str = "diagnostics.crash";

/// 尽早安装崩溃钩子（`main` 里紧跟日志初始化调用）。
pub fn install_hook() {
    duck_platform::install_panic_hook(&log_dir());
}

/// 日志目录：运行日志与崩溃现场都在这里。
#[must_use]
pub fn log_dir() -> PathBuf {
    duck_platform::log_dir()
}

/// 累计崩溃次数（跨日志清理长期保留）。
#[must_use]
pub fn crash_total() -> u64 {
    duck_platform::crash_count(&log_dir())
}

/// 用户尚未查看过的崩溃数。
#[must_use]
pub fn crash_unseen(cx: &App) -> u64 {
    crash_total().saturating_sub(seen(cx))
}

/// 本次启动是否该提示「上次异常退出」；命中时把水位线推到当前值，因此只提示一次。
pub fn take_unseen_crash(cx: &App) -> bool {
    let total = crash_total();
    if total <= seen(cx) {
        return false;
    }
    duck_store::kv_put(cx, KEY_CRASH_SEEN, total.to_string());
    true
}

/// 平台与架构标识（诊断文本用；值是标识符，不参与本地化）。
#[must_use]
pub fn platform() -> String {
    format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH)
}

/// 已装载的插件数。
#[must_use]
pub fn plugin_count(cx: &App) -> usize {
    crate::plugins::index(cx).map_or(0, |index| index.loaded_count())
}

/// 诊断文本：出问题时用户能整份复制给开发者，比截图有用。
#[must_use]
pub fn report(cx: &App) -> String {
    let version = duck_update::compiled_version();
    let mut text = format!("Duck Toolbox {version}\n");
    text.push_str(&format!("platform: {}\n", platform()));
    text.push_str(&format!("plugins: {}\n", plugin_count(cx)));
    text.push_str(&format!("crashes: {}\n", crash_total()));
    text.push_str(&format!("data: {}\n", duck_platform::db_path().display()));
    text.push_str(&format!("logs: {}\n", log_dir().display()));
    text
}

/// 复制诊断文本到剪贴板，并给一次成功 / 失败反馈。
pub fn copy_report(cx: &mut App) {
    let text = report(cx);
    match duck_platform::write_text(&text) {
        Ok(()) => crate::update::toast(cx, tr(cx, "已复制诊断信息"), Tone::Success),
        Err(error) => {
            tracing::warn!(%error, "failed to copy diagnostics to the clipboard");
            crate::update::toast(cx, tr(cx, "复制诊断信息失败"), Tone::Error);
        }
    }
}

/// 有未查看的崩溃时推一条**可替换**的提示。
pub fn notify_unseen_crash(cx: &mut App) {
    let count = crash_unseen(cx);
    let message = trf(
        cx,
        "检测到 {} 次未查看的崩溃，可在「设置 → 关于」页查看诊断信息。",
        &[&count.to_string()],
    );
    crate::update::on_notify_window(cx, move |window, cx| {
        notify::sticky(KEY_CRASH_NOTICE, Tone::Warning, message, window, cx);
    });
}

/// 读水位线；读不到（从未提示过）时当 0。
fn seen(cx: &App) -> u64 {
    duck_store::kv_get(cx, KEY_CRASH_SEEN)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}
