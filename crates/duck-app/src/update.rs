//! 更新器的应用侧接线：配置落盘、后台检查、进度、安装与重启。
//!
//! 逻辑层在 `duck-update`（不依赖 gpui，全部可单测）；这里只负责三件事：
//!
//! 1. **配置与状态的落点** —— 更新源地址与「启动后自动检查」进 `duck-store` 的 kv 表，
//!    检查 / 下载进度进一个 `Global`（[`Updater`]）。
//! 2. **线程边界** —— HTTP 与哈希都跑在 gpui 的后台执行器上，主线程只接收事件。
//!    [`Updater`] 只能从主线程读写，所以进度经一条无界通道从后台搬过来。
//! 3. **交互** —— 「装好了 → 现在重启？」用原生确认框；其余结果用应用内 toast。
//!    没有窗口（例如从托盘触发且窗口已关）时不弹任何东西，只把状态留在 [`Updater`] 里。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use duck_components::notify;
use duck_store as store;
use duck_theme::trf;
use duck_update::{Check, Flow, ManualReason, UpdateConfig, UpdateError, UpdateEvent, UpdateInfo};
use futures_channel::mpsc::{UnboundedReceiver, unbounded};
use futures_util::StreamExt as _;
use gpui::{AnyWindowHandle, App, Global, SharedString, Task};

/// 更新源地址的 kv 键。
///
/// 存的是**用户的覆盖值**；空串表示「用内置默认源」（见 [`DEFAULT_SOURCE`]）。
const KEY_SOURCE: &str = "update.source_url";
/// 「启动后自动检查」的 kv 键。
const KEY_AUTO: &str = "update.check_on_launch";

/// 内置的默认更新源。
///
/// 用 `releases/latest/download/` 而不是带版本号的地址，是因为它**版本无关**：升到
/// 0.3.0 时不必改这个常量，还在跑旧版的客户端也永远取得到最新那份 manifest。
/// `latest` 天然跳过预发布，正是稳定通道要的语义。
///
/// 它只是一份几 KB 的 JSON——真正大的是归档，而那些地址写在 manifest 里、可以指向
/// 镜像。所以「manifest 留在 GitHub、只把归档分流到 CDN」是最省事的加速组合：
/// 用户什么都不用配。
const DEFAULT_SOURCE: &str =
    "https://github.com/HanoCode/gpui-toolbox/releases/latest/download/update.json";

/// 提示去重键：一次「检查」只留最新一条结论（反复点不堆叠）。
const KEY_TOAST_CHECK: &str = "update.toast.check";
/// 提示去重键：一次「安装」只留最新一条结论。
const KEY_TOAST_INSTALL: &str = "update.toast.install";
/// 提示去重键：重启失败只留最新一条。
const KEY_TOAST_RESTART: &str = "update.toast.restart";
/// 提示去重键：回退失败只留最新一条。
const KEY_TOAST_ROLLBACK: &str = "update.toast.rollback";

/// 检查 / 安装的当前阶段。
///
/// 失败变体持 [`UpdateError`] 而不是一句话：文案在渲染时现取，切语言才会跟着变。
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// 没在跑，也没结果。
    Idle,
    /// 没有可用的更新源。
    ///
    /// 正常构建里到不了这里——[`DEFAULT_SOURCE`] 不为空，用户清空输入框也只是回到默认。
    /// 留着是因为它是「真的没有源」这一分支的兜底，而不是一条摆设。
    NoSource,
    /// 正在取 manifest。
    Checking,
    /// 已是最新。
    UpToDate {
        /// 更新源上的版本串。
        latest: String,
    },
    /// 有新版本。
    Available(UpdateInfo),
    /// 正在下载。
    Downloading {
        /// 已收字节。
        downloaded: u64,
        /// 总字节。
        total: u64,
    },
    /// 正在校验。
    Verifying,
    /// 正在替换安装。
    Installing,
    /// 装好了，等重启。
    Installed,
    /// 已回退到上一版本，等重启。
    ///
    /// 与 [`Installed`](Self::Installed) 分开而不是复用：界面上这是两件不同的事——
    /// 「更新好了」和「回到了从前」——而两者都同样需要一次重启。
    RolledBack,
    /// 归档已下好，但这台机器无法就地更新。
    Manual {
        /// 原因。
        reason: ManualReason,
        /// 归档位置。
        archive: PathBuf,
    },
    /// 失败。
    Failed(UpdateError),
}

impl Phase {
    /// 有没有一个后台任务正在推进。
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading { .. } | Self::Verifying | Self::Installing
        )
    }
}

/// 更新器的应用级状态。
pub struct Updater {
    source: SharedString,
    auto_check: bool,
    phase: Phase,
    /// 最近一次检查拿到的发布说明（源上那个版本的），「关于」页用。
    ///
    /// 单独缓存而不是从 [`Phase`] 现取：「已是最新」时阶段里没有这份文本，
    /// 而用户恰恰最需要在那时看到「我装的是什么版本、改了什么」。
    release_notes: Option<SharedString>,
    /// 置位即要求后台任务尽快停下（每收一块检查一次）。
    cancel: Arc<AtomicBool>,
    /// 正在跑的检查 / 安装任务。
    ///
    /// 持有而不是丢弃：换一次检查时旧任务会随之被放弃，也不会再回到主线程改状态。
    task: Option<Task<()>>,
}

impl Global for Updater {}

/// 建立更新器状态，并在后台清扫上次安装留下的旧 bundle。
pub fn init(cx: &mut App) {
    let stored = store::kv_get(cx, KEY_SOURCE).unwrap_or_default();
    // 缺省是「开」；只有显式存过 `off` 才是关。
    let auto_check = store::kv_get(cx, KEY_AUTO).as_deref() != Some("off");
    cx.set_global(Updater {
        source: effective_source(&stored),
        auto_check,
        phase: Phase::Idle,
        release_notes: None,
        cancel: Arc::new(AtomicBool::new(false)),
        task: None,
    });

    // 上一次安装把旧 `.app` **改名留了下来**（那是回退的落点，不清）。这里只清理
    // 失败的安装留下的暂存目录。扔到独立线程：删一个几十 MB 的目录不该拖慢第一帧。
    std::thread::spawn(|| duck_update::platform_installer().sweep_stale());
}

/// 把 kv 里存的值折算成**实际要用的**更新源。
///
/// 空（从没配过，或用户把输入框清空表示「恢复默认」）就回落到 [`DEFAULT_SOURCE`]。
/// 于是「没配更新源」这件事在正常的构建里不再存在，用户开箱就能检查更新。
fn effective_source(stored: &str) -> SharedString {
    let trimmed = stored.trim();
    if trimmed.is_empty() {
        DEFAULT_SOURCE.into()
    } else {
        trimmed.into()
    }
}

/// 内置的默认更新源（设置页拿它当输入框的占位提示）。
#[must_use]
pub const fn default_source() -> &'static str {
    DEFAULT_SOURCE
}

/// 更新源地址。
#[must_use]
pub fn source(cx: &App) -> SharedString {
    cx.global::<Updater>().source.clone()
}

/// 改写更新源并落盘。
///
/// 传空串表示「恢复内置默认源」——落盘的也是空串，而不是把默认地址存进 kv：
/// 两者混为一谈的话，将来换默认源就换不动已经发出去的那些客户端了。
pub fn set_source(value: String, cx: &mut App) {
    let next = effective_source(&value);
    {
        let updater = cx.global_mut::<Updater>();
        if updater.source.as_ref() == next.as_ref() {
            return;
        }
        updater.source = next;
        // 换了源，上一次的结论就不作数了（发布说明同样失效）。
        updater.phase = Phase::Idle;
        updater.release_notes = None;
    }
    store::kv_put(cx, KEY_SOURCE, value.trim().to_owned());
    cx.refresh_windows();
}

/// 是否在启动后自动检查。
#[must_use]
pub fn auto_check(cx: &App) -> bool {
    cx.global::<Updater>().auto_check
}

/// 打开 / 关闭「启动后自动检查」并落盘。
pub fn set_auto_check(on: bool, cx: &mut App) {
    {
        let updater = cx.global_mut::<Updater>();
        if updater.auto_check == on {
            return;
        }
        updater.auto_check = on;
    }
    store::kv_put(cx, KEY_AUTO, if on { "on" } else { "off" });
    cx.refresh_windows();
}

/// 当前阶段。
#[must_use]
pub fn phase(cx: &App) -> Phase {
    cx.global::<Updater>().phase.clone()
}

/// 最近一次检查拿到的发布说明（「关于 → 版本发布记录」）。
///
/// 只有检查过更新源才有；没配源或还没检查时为 `None`，由页面显示占位提示。
#[must_use]
pub fn release_notes(cx: &App) -> Option<SharedString> {
    cx.global::<Updater>().release_notes.clone()
}

/// 启动后自动检查：只在配了更新源、开了自动检查、且没有任务在跑时才动。
pub fn check_on_launch(cx: &mut App) {
    if !auto_check(cx) || source(cx).trim().is_empty() {
        return;
    }
    check_now(cx);
}

/// 查一次更新。
pub fn check_now(cx: &mut App) {
    if phase(cx).is_running() {
        return;
    }

    let Some(config) = config(cx) else {
        set_phase(cx, Phase::NoSource);
        let text = trf(cx, "尚未配置更新源。", &[]);
        sticky(cx, KEY_TOAST_CHECK, text, notify::Tone::Warning);
        return;
    };

    set_phase(cx, Phase::Checking);
    let task = cx.spawn(async move |cx| {
        let checked = cx
            .background_executor()
            .spawn(async move { duck_update::check(&config) })
            .await;

        cx.update(|cx| {
            // 发布说明在成功结论里（两个变体都带）；失败时保留上一次的，不清空。
            set_release_notes(cx, checked.as_ref().ok().and_then(Check::notes));
            match checked {
                Ok(Check::UpToDate { latest, .. }) => {
                    let version = latest.to_string();
                    set_phase(
                        cx,
                        Phase::UpToDate {
                            latest: version.clone(),
                        },
                    );
                    let text = trf(cx, "已是最新版本 {}。", &[&version]);
                    toast(cx, text, notify::Tone::Success);
                }
                Ok(Check::Available(info)) => {
                    let version = info.version().to_string();
                    set_phase(cx, Phase::Available(info));
                    let text = trf(cx, "发现新版本 {}。", &[&version]);
                    toast(cx, text, notify::Tone::Info);
                }
                Err(error) => {
                    let detail = error.to_string();
                    set_phase(cx, Phase::Failed(error));
                    let text = trf(cx, "检查更新失败：{}", &[&detail]);
                    sticky(cx, KEY_TOAST_CHECK, text, notify::Tone::Error);
                }
            }
        });
    });
    store_task(cx, task);
}

/// 下载并安装刚查到的更新。
///
/// 只该在 [`Phase::Available`] 上调用；阶段不对时静默返回。
pub fn install_now(cx: &mut App) {
    let Phase::Available(info) = phase(cx) else {
        return;
    };
    let Some(config) = config(cx) else {
        set_phase(cx, Phase::NoSource);
        return;
    };

    set_phase(
        cx,
        Phase::Downloading {
            downloaded: 0,
            total: info.size(),
        },
    );

    let cancel = begin_cancel(cx);
    let task = cx.spawn(async move |cx| {
        let (sender, receiver) = unbounded::<UpdateEvent>();
        let sink = move |event: UpdateEvent| {
            if cancel.load(Ordering::Relaxed) {
                return Flow::Abort;
            }
            // 接收端没了说明主线程那侧已经不关心了；把传输停掉比下完更对。
            if sender.unbounded_send(event).is_err() {
                return Flow::Abort;
            }
            Flow::Continue
        };

        let installer = duck_update::platform_installer();
        let done = cx
            .background_executor()
            .spawn(async move { duck_update::install(&config, &info, installer.as_ref(), &sink) });

        let progress = drain(receiver, cx);
        let ((), outcome) = futures_util::future::join(progress, done).await;

        cx.update(|cx| finish(outcome, cx));
    });
    store_task(cx, task);
}

/// 中止正在跑的检查 / 下载。
pub fn cancel(cx: &mut App) {
    if !phase(cx).is_running() {
        return;
    }
    cx.global::<Updater>().cancel.store(true, Ordering::Relaxed);
    set_phase(cx, Phase::Idle);
    let text = trf(cx, "已取消更新。", &[]);
    toast(cx, text, notify::Tone::Info);
}

/// 重启以运行新版本。
///
/// 只该在 [`Phase::Installed`] 或 [`Phase::RolledBack`] 上调用：先让新进程起来，
/// 再退出自己。两者都是「磁盘上的东西已经换过了，重启才生效」。
pub fn restart(cx: &mut App) {
    if !matches!(phase(cx), Phase::Installed | Phase::RolledBack) {
        return;
    }
    match duck_update::platform_installer().relaunch() {
        Ok(()) => cx.quit(),
        Err(error) => {
            let detail = error.to_string();
            set_phase(cx, Phase::Failed(error));
            let text = trf(cx, "无法重启应用：{}", &[&detail]);
            sticky(cx, KEY_TOAST_RESTART, text, notify::Tone::Error);
        }
    }
}

/// 磁盘上还留着上一版吗。
///
/// 「关于」页拿它决定「回退到上一版本」这个入口该不该出现——一个点了只会报错的按钮
/// 比没有更糟。它只查一条路径，可以在每次渲染时问。
#[must_use]
pub fn can_rollback() -> bool {
    duck_update::platform_installer().has_previous()
}

/// 把上一版换回来；重启后生效（与安装同一个约定）。
///
/// 没有上一版时会明确报出来——而不是静默地什么都不做，那会让人以为按钮坏了。
pub fn rollback_now(cx: &mut App) {
    // 正在下载 / 安装途中不动自己的目录：两个方向同时改同一批路径，结果谁也说不准。
    if phase(cx).is_running() {
        return;
    }

    match duck_update::platform_installer().rollback() {
        Ok(()) => {
            set_phase(cx, Phase::RolledBack);
            let text = trf(cx, "已回退到上一版本，重启后生效。", &[]);
            toast(cx, text, notify::Tone::Success);
            // 与装好之后同样处理：原生确认框是模态阻塞调用，得用 `defer` 挪出当前栈。
            cx.defer(|cx| {
                let title = trf(cx, "回退已就绪", &[]);
                let body = trf(cx, "重启后运行上一版本。现在重启？", &[]);
                if duck_platform::confirm(&title, &body) {
                    restart(cx);
                }
            });
        }
        Err(error) => {
            // **不**改阶段：回退失败不该把「已是最新」这种好状态冲成「更新失败」。
            let detail = error.to_string();
            let text = trf(cx, "回退失败：{}", &[&detail]);
            sticky(cx, KEY_TOAST_ROLLBACK, text, notify::Tone::Error);
        }
    }
}

/// 当前阶段的一句话描述。`None` 表示没什么可说（空闲）。
#[must_use]
pub fn describe(phase: &Phase, cx: &App) -> Option<SharedString> {
    let text = match phase {
        Phase::Idle => return None,
        Phase::NoSource => trf(cx, "尚未配置更新源。", &[]),
        Phase::Checking => trf(cx, "正在检查更新…", &[]),
        Phase::UpToDate { latest } => trf(cx, "已是最新版本 {}。", &[latest]),
        Phase::Available(info) => {
            let version = info.version().to_string();
            trf(cx, "发现新版本 {}。", &[&version])
        }
        Phase::Downloading { downloaded, total } => {
            let done = duck_theme::bytes(*downloaded);
            let all = duck_theme::bytes(*total);
            trf(cx, "正在下载更新… {} / {}", &[&done, &all])
        }
        Phase::Verifying => trf(cx, "正在校验更新…", &[]),
        Phase::Installing => trf(cx, "正在安装更新…", &[]),
        Phase::Installed => trf(cx, "更新已安装，重启后生效。", &[]),
        Phase::RolledBack => trf(cx, "已回退到上一版本，重启后生效。", &[]),
        Phase::Manual { archive, .. } => {
            let path = archive.display().to_string();
            trf(cx, "已下载并校验，安装包在 {}。", &[&path])
        }
        Phase::Failed(error) => {
            let detail = error.to_string();
            trf(cx, "更新失败：{}", &[&detail])
        }
    };
    Some(text.into())
}

/// 人工安装的原因（[`Phase::Manual`] 里那句「为什么不能就地装」）。
#[must_use]
pub fn manual_reason(reason: ManualReason, cx: &App) -> SharedString {
    match reason {
        ManualReason::NotABundle => trf(cx, "当前运行的不是打包后的 .app。", &[]),
        ManualReason::NotWritable => trf(cx, "当前位置没有写入权限。", &[]),
        ManualReason::ReadOnlyLocation => trf(cx, "当前位置是只读的。", &[]),
        ManualReason::Unsupported => trf(cx, "当前平台不支持就地更新。", &[]),
    }
    .into()
}

/// 把进度事件搬进主线程。
async fn drain(mut receiver: UnboundedReceiver<UpdateEvent>, cx: &gpui::AsyncApp) {
    while let Some(event) = receiver.next().await {
        cx.update(|cx| apply_event(event, cx));
    }
}

/// 一个更新事件对 [`Phase`] 的影响。
fn apply_event(event: UpdateEvent, cx: &mut App) {
    let next = match event {
        UpdateEvent::Downloading { downloaded, total } => Phase::Downloading { downloaded, total },
        UpdateEvent::Verifying => Phase::Verifying,
        UpdateEvent::Installing => Phase::Installing,
        UpdateEvent::Installed => Phase::Installed,
        UpdateEvent::Manual { reason, archive } => Phase::Manual { reason, archive },
    };
    set_phase(cx, next);
}

/// 安装流程收尾：报结果，并在装好后问一句要不要现在重启。
fn finish(outcome: Result<duck_update::InstallOutcome, UpdateError>, cx: &mut App) {
    match outcome {
        Ok(duck_update::InstallOutcome::Installed) => {
            set_phase(cx, Phase::Installed);
            let text = trf(cx, "更新已安装，重启后生效。", &[]);
            toast(cx, text, notify::Tone::Success);
            // 原生确认框是模态阻塞调用，只该在动作处理器里跑；用 `defer` 挪出当前的
            // 更新栈，避免在 gpui 借用期间开一个嵌套事件循环。
            cx.defer(|cx| {
                let title = trf(cx, "更新已就绪", &[]);
                let body = trf(cx, "重启后运行新版本。现在重启？", &[]);
                if duck_platform::confirm(&title, &body) {
                    restart(cx);
                }
            });
        }
        Ok(duck_update::InstallOutcome::Manual { reason, archive }) => {
            set_phase(cx, Phase::Manual { reason, archive });
            let text = describe(&phase(cx), cx).unwrap_or_default();
            sticky(cx, KEY_TOAST_INSTALL, text, notify::Tone::Warning);
        }
        Err(UpdateError::Cancelled) => set_phase(cx, Phase::Idle),
        Err(error) => {
            let detail = error.to_string();
            set_phase(cx, Phase::Failed(error));
            let text = trf(cx, "更新失败：{}", &[&detail]);
            sticky(cx, KEY_TOAST_INSTALL, text, notify::Tone::Error);
        }
    }
}

/// 更新配置：没配更新源时返回 `None`。
fn config(cx: &App) -> Option<UpdateConfig> {
    let url = source(cx);
    if url.trim().is_empty() {
        return None;
    }
    Some(UpdateConfig::new(
        url.as_ref(),
        duck_platform::cache_dir().join("updates"),
    ))
}

/// 写入阶段并催一次重绘。
fn set_phase(cx: &mut App, phase: Phase) {
    cx.global_mut::<Updater>().phase = phase;
    cx.refresh_windows();
}

/// 记下这次检查带回的发布说明；`None`（结论里没写）时保持原值不动。
fn set_release_notes(cx: &mut App, notes: Option<&str>) {
    let Some(notes) = notes else {
        return;
    };
    cx.global_mut::<Updater>().release_notes = Some(SharedString::from(notes));
}

/// 换一个新的取消标志，并把它交给即将启动的任务。
fn begin_cancel(cx: &mut App) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    cx.global_mut::<Updater>().cancel = flag.clone();
    flag
}

/// 记住正在跑的任务（旧任务随之被放弃）。
fn store_task(cx: &mut App, task: Task<()>) {
    cx.global_mut::<Updater>().task = Some(task);
}

/// 推一条应用内提示；没有可用窗口时静默丢弃。
///
/// 优先投给**当前活跃窗口**：用户多半是在「设置 → 关于」里点的检查，设置窗口
/// 就是前台窗口，toast 必须落在它上面——投给最早创建的窗口（主窗口）会被设置
/// 窗口整个盖住，看起来像「没反应」或「提示丢了」。
///
/// 没有活跃窗口时（从托盘触发且应用不在前台）退一步取**窗口栈最顶层**的那个：
/// `windows()` 只是创建顺序，栈顶才是用户真正看得见的那一个。Windows 未实现
/// `window_stack`（返回 `None`），自然回落到创建顺序上的第一个窗口。
///
/// 一个窗口都没有时什么也不弹，状态仍记在 [`Updater`] 里，下次打开「设置 → 关于」
/// 就能看到——比为了弹一条 toast 去开一个窗口克制。
pub(crate) fn toast(cx: &mut App, message: impl Into<SharedString>, tone: notify::Tone) {
    let message = message.into();
    on_notify_window(cx, move |window, cx| match tone {
        notify::Tone::Info => notify::info(message, window, cx),
        notify::Tone::Success => notify::success(message, window, cx),
        notify::Tone::Warning => notify::warning(message, window, cx),
        notify::Tone::Error => notify::error(message, window, cx),
    });
}

/// 推一条**可替换**的应用内提示：同 `key` 的上一条会被替换掉。
///
/// 更新检查这类动作可能被反复触发（用户等不及会连点），每条通知的默认 id 是
/// 随机 uuid，每点一次堆一张的话窗右上角很快就是一列红条；固定 key 让同一动作
/// 只占一位（自动关闭后仍会被新结论替换）。
fn sticky(cx: &mut App, key: &'static str, message: impl Into<SharedString>, tone: notify::Tone) {
    let message = message.into();
    on_notify_window(cx, move |window, cx| {
        notify::sticky(key, tone, message, window, cx)
    });
}

/// 在「当前最该收到提示的窗口」上投递；没有窗口就什么都不做。
///
/// 候选按「用户看得见的程度」排：**活跃窗口 → 窗口栈（前→后）→ 创建最早的窗口**。
/// 用户多半是在当前那个窗口里点的动作，提示必须落在它上面——投给最早创建的主窗口会被
/// 别的窗口整个盖住，看起来像「没反应」或「提示丢了」。插件市场也用这一条。
///
/// **引导窗口是唯一被剔掉的例外**（见 [`notify_target`]）：它是首启流程开的一个居中小
/// 向导，没有顶栏、也没有通知历史那个铃铛入口，关掉就没了。而它在启动最末打开、默认拿走
/// 焦点，于是 `active_window()` 返回的正是它——首启期间任何一条应用级提示（更新检查、
/// 插件清单刷新、窗口建不起来……）都会落在那个小窗的右上角，而不是程序主窗的右上角。
/// 剔掉它之后提示回到主窗或别的辅助窗口上；只有引导窗口还开着时提示不弹（仍进历史），
/// 与「一个窗口都没有时什么都不弹」同一个态度。
pub(crate) fn on_notify_window(cx: &mut App, post: impl FnOnce(&mut gpui::Window, &mut App)) {
    let Some(handle) = notify_target(cx) else {
        return;
    };
    let _ = handle.update(cx, |_, window, cx| post(window, cx));
}

/// 挑一个能承接提示的窗口；全都不可用时返回 `None`。
///
/// **逐个跳过引导窗口**，而不是「第一个是它就算了」：引导窗口在前时，后面的候选仍然
/// 可用，丢掉它们等于凭白吞掉一条提示。三个来源的先后顺序见 [`on_notify_window`]；
/// `window_stack` 在 Windows 上返回 `None`（未实现），自然落到 `windows()` 的创建顺序上。
fn notify_target(cx: &App) -> Option<AnyWindowHandle> {
    let wizard = crate::app::Desktop::onboarding_handle(cx).map(AnyWindowHandle::from);
    cx.active_window()
        .into_iter()
        .chain(cx.window_stack().unwrap_or_default())
        .chain(cx.windows())
        .find(|handle| Some(*handle) != wizard)
}

#[cfg(test)]
mod tests {
    use super::Phase;
    use duck_update::UpdateError;

    #[test]
    fn only_the_busy_phases_count_as_running() {
        assert!(Phase::Checking.is_running());
        assert!(Phase::Verifying.is_running());
        assert!(Phase::Installing.is_running());
        assert!(
            Phase::Downloading {
                downloaded: 1,
                total: 2
            }
            .is_running()
        );
        assert!(!Phase::Idle.is_running());
        assert!(!Phase::Installed.is_running());
        assert!(!Phase::Failed(UpdateError::Cancelled).is_running());
    }
}
