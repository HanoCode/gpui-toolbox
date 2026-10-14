//! Duck Toolbox 桌面入口。
//!
//! 只做四件事：解析外部唤起请求、装配资源、装载主题与组件库、打开主窗口。
//! 页面与窗口内部结构见 [`app`] 与 [`pages`]。

mod actions;
mod ai;
mod ai_tools;
mod app;
mod assets;
mod audit;
mod backup;
mod deeplink;
mod diagnostics;
mod hotkey;
mod http_client;
mod language_switch;
mod market;
mod pages;
mod palette_tools;
mod plugins;
mod settings;
mod theme_picker;
mod tools;
mod top_bar;
mod tray;
mod update;

use futures_channel::mpsc::unbounded;
use futures_util::StreamExt as _;
use gpui::QuitMode;
use gpui_platform::application;

/// 应用身份标识：与打包脚本写入 `Info.plist` 的 `CFBundleIdentifier` 同一串。
///
/// 只用于系统级机制（Windows 的 AppUserModelID），不是 macOS bundle 的替代品。
const APP_IDENTIFIER: &str = "com.duck.toolbox";

/// 产品名的唯一来源：操作系统展示名（系统通知发送方）+ 顶栏品牌区 + 「关于」页。
///
/// 位于 crate 根的私有项对全部子模块可见（`crate::APP_NAME`），
/// 故 `top_bar` / `pages::settings` 直接复用，不再各自定义同名常量。
const APP_NAME: &str = "Duck Toolbox";

fn main() {
    // 「进程启动 → 主窗就绪」的起点，在装上日志之前 —— 用户感知的等待从点图标那一刻算起。
    let started = std::time::Instant::now();
    // 日志最先装：其后的一切（含持久化）都能记录。
    let log_file = duck_platform::init_logging();
    tracing::info!(file = %log_file.display(), "Duck Toolbox starting");
    // 崩溃钩子紧跟日志：此后任何 panic 都会在日志目录留下现场与计数。
    diagnostics::install_hook();

    // 本轮启动携带的请求（命令行路径 / 深链接）。**先解析再决定让不让位**——
    // 让位之前得知道用户这轮想打开什么，否则 `duck duck://tool/x` 在应用已运行时
    // 会被默默丢掉，用户看到的是「什么都没发生」。
    let requests = duck_platform::OpenRequest::from_args(std::env::args_os());

    // 转发通道的两端。发送端有两份：IPC 监听给第二实例用，`on_open_urls` 给
    // 系统深链接用；接收端在 `run` 里由一条主线程任务消费。
    let (requests_tx, mut requests_rx) = unbounded();

    // 单实例：拿到锁的进程才是「正主」，锁一直持到进程结束。
    // 取不到锁不阻断启动（没它只是可能开出第二个窗口），被别人占了才让位。
    let _instance = match duck_platform::acquire_instance_lock("duck") {
        Ok(duck_platform::LockOutcome::Acquired(guard)) => Some(guard),
        Ok(duck_platform::LockOutcome::Busy { path }) => {
            // 点 Dock 图标不会走到这里（macOS 走 `on_reopen`，不新起进程）；
            // 命中的只有命令行 / 深链接 / 脚本重复拉起。把手里的请求交给正主再让位。
            tracing::warn!(lock = %path.display(), "another instance is already running; exiting");
            hand_over(&requests);
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "failed to acquire the single-instance lock; continuing");
            None
        }
    };

    // 受理第二实例的转发。走到这里说明锁在手（或锁不可用），所以本进程就是该监听的那个。
    // 句柄必须活到进程结束，交给 `_listener` 持有——它不是「立刻丢弃」的 `let _`。
    let _listener: Option<duck_platform::IpcListener> = {
        let sender = requests_tx.clone();
        match duck_platform::listen_for_requests(move |handed_over| {
            // 这个回调跑在后台线程上：`App` 不是 `Send`，碰不得，只能把请求投进通道，
            // 由 `run` 里那条主线程任务取出来执行。
            let _ = sender.unbounded_send(handed_over);
        }) {
            Ok(listener) => {
                tracing::info!(
                    endpoint = %listener.endpoint().display(),
                    "listening for hand-offs from later launches"
                );
                Some(listener)
            }
            Err(duck_platform::IpcError::AlreadyListening) => {
                // 通道被占 = 已经有一个能用的实例在跑。这在 Windows 上是**唯一**判据
                // （那边没有 flock，锁恒成功），在 Unix 上则是「旧进程还没退干净」。
                // 两种情况下，转发过去再让位都比硬开第二个实例好。
                tracing::warn!("another instance is already listening; exiting");
                hand_over(&requests);
                return;
            }
            Err(error) => {
                // 通道用不了不该拦着应用启动：代价只是「已运行时再唤起不会跳转」。
                tracing::warn!(%error, "the hand-off channel is unusable; external opens will be dropped");
                None
            }
        }
    };

    let application = application()
        .with_assets(assets::DesktopAssets)
        // gpui 的桌面装配路径默认 `NullHttpClient`（见 `http_client` 模块头）：
        // 不接上它，正文里的远程图片会全部加载失败。
        .with_http_client(std::sync::Arc::new(http_client::UreqHttpClient::new()))
        .with_quit_mode(QuitMode::Explicit);

    // 深链接：应用**已经在跑**时，系统把 URL 直接送到这里，不会再起一个进程。
    // 回调签名是 `FnMut(Vec<String>)`——拿不到 `App`，所以同样只往通道里投。
    {
        let sender = requests_tx.clone();
        application.on_open_urls(move |urls| {
            let requests = urls
                .into_iter()
                .map(duck_platform::OpenRequest::Url)
                .collect::<Vec<_>>();
            let _ = sender.unbounded_send(requests);
        });
    }
    // 发送端到此为止：多的那一个留在手里只会让 `receiver` 永远等不到「全部关闭」，
    // 而那个「等不到」正是这条任务的正常状态。两个真正的发送端分别由监听线程与
    // `on_open_urls` 持有。
    drop(requests_tx);

    // macOS 关闭全部窗口后应用仍驻留：点 Dock 图标走 reopen，重新打开主窗口。
    application.on_reopen(app::open_main_window);
    application.run(move |cx| {
        // 应用身份：macOS 上由 Info.plist 提供，这里只在 Windows 上有实际作用——
        // gpui 用它注册 AppUserModelID，而便携 exe 没有 MSIX 包身份，缺了它
        // **系统通知会被静默丢弃**（gpui 只记一条 warn，界面毫无反应）。
        cx.set_app_identity(APP_IDENTIFIER, APP_NAME);
        // 深链接与文件关联的登记：macOS 认 bundle 清单（打包时写进 Info.plist），
        // Windows 认注册表，所以只有后者在这里做。失败只记日志——注册不上不影响
        // 应用自己跑，代价只是外部唤起不工作。
        duck_platform::register_open_handlers();
        gpui_component::init(cx);
        duck_theme::init(cx);
        // 存储失败不阻断启动：没有它只是「不记住设置」，界面仍可用。
        if let Err(error) = duck_store::Store::install(cx) {
            tracing::error!(%error, "failed to initialize persistent store; running without persistence");
        }
        // 崩溃水位线要在存储之后读：水位线本身存在 `kv` 里。
        let crashed_before = diagnostics::take_unseen_crash(cx);
        // 日志保留：按用户设置（默认 7 天）清一次旧日志，并让它在后台每天再清一次。
        // 排在存储之后——保留天数本身存在 `kv` 里。
        tracing::info!(count = pages::prune_now(cx), "log retention applied on launch");
        pages::spawn_pruner(cx);
        // 先恢复用户上次的外观，再装配（装配时的图标 / 菜单已按恢复后的主题解析）。
        settings::load_appearance(cx);
        // 通知通道同样要在任何窗口出现之前生效：下面几步会产生通知（保留清理、
        // 快捷键登记失败、崩溃提示、更新检查），按默认档全弹就把用户关过的开关当没关。
        settings::apply_notify_channels(cx);
        app::install(cx);
        // 托盘排在恢复语言之后：菜单文案在构建时定型（之后切语言由它自己听）。
        // 失败只是菜单栏少一个图标，不值得分支——`tray::init` 自己记日志。
        tray::init(cx);
        // 全局唤起快捷键：起投递服务 + 按设置登记。排在存储之后（键位与开关都在
        // `kv` 里），排在窗口之前——落点自己能处理「窗口还不存在」。
        // **不弹窗提示**：这会儿还没有窗口可投；失败提示推迟到主窗就绪之后，
        // 见下面那句 `announce_startup_failure`。
        hotkey::install(cx);
        app::open_main_window(cx);
        // 启动耗时：到这里主窗已经开了，概览页那张卡读的就是这一档数据。
        // 只记真实测得的数 —— 没记过就是「没有样本」，界面显示 `—` 而不是编一个。
        let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(millis, "main window ready");
        settings::record_startup(cx, millis);
        // 恢复上次开着的辅助窗口（默认关，见「设置 → 行为」）。这里排的是两次
        // `defer`：先让各辅窗按「已开则聚焦、否则新建」开出来，再由后一个把主窗口
        // 抬回前台 —— 否则最后开出来的那个辅窗会抢走焦点。
        if settings::restore_windows_on_launch(cx) {
            app::restore_aux_windows(cx);
            cx.defer(app::open_main_window);
        }
        // 上次异常退出的提示排在窗口就绪之后：否则会投给一个还不存在的窗口。
        if crashed_before {
            diagnostics::notify_unseen_crash(cx);
        }
        // 快捷键登记不上的提示同理（`hotkey::install` 时窗口还没开），而且它比崩溃
        // 更「静默」——按了没反应，用户只会以为功能没做。同键替换，只提示一次。
        hotkey::announce_startup_failure(cx);
        // 首启引导排在最末：它**盖在主窗口之上**而不是替代主窗口，所以上面的提示
        // 仍然落在主窗口上。只在该标记缺失时开（见 `pages::onboarding`）。
        if !pages::onboarding_done(cx) {
            tracing::info!("first run: opening the onboarding wizard");
            pages::open_onboarding(cx);
        }
        // 自动检查放在窗口就绪之后：结论要么进「设置 → 更新」，要么推成一条提示。
        update::check_on_launch(cx);
        // 插件清单同样排在窗口就绪之后；只在缓存过期（默认 6 小时）时才联网，
        // 失败静默——刚打开应用就被一条网络错误打扰是不划算的。
        market::refresh_if_stale(cx);
        // 外部唤起：本轮启动带来的命令行参数 / 深链接。排在主窗之后——它们要切路由。
        deeplink::dispatch(&requests, cx);
        // 第二实例转发过来的、以及系统在运行期送来的深链接。这条任务与进程同寿，
        // 所以只管 `detach`，不保留句柄。
        cx.spawn(async move |cx| {
            while let Some(handed_over) = requests_rx.next().await {
                cx.update(|cx| deeplink::dispatch(&handed_over, cx));
            }
        })
        .detach();
    });
}

/// 把本轮启动的请求交给正在运行的实例，然后让位。
///
/// 没有请求（用户只是又点了一次图标、或脚本重复拉起）就安静退出——
/// 为这个弹一句「已经在运行了」是打扰。
fn hand_over(requests: &[duck_platform::OpenRequest]) {
    if requests.is_empty() {
        return;
    }
    match duck_platform::forward_to_running_instance(requests) {
        Ok(()) => tracing::info!(
            count = requests.len(),
            "handed the launch request to the running instance"
        ),
        Err(error) => tracing::warn!(%error, "failed to hand the launch request over; exiting"),
    }
}
