//! 菜单栏常驻图标：一个箱体标记，加一张原生菜单。
//!
//! # 为什么是 `src/tray/` 而不是 `src/platform/tray/`
//!
//! 平台差异只落在 [`mark`] 里一个布尔判断上（AppKit 模板图 vs 明确的墨色），
//! 其余全是 `tray-icon` 自己抹平的。为它开一层目录不值。
//!
//! # 事件怎么进来：没有第二个事件循环
//!
//! 这是最容易做错的一段，所以写下来。
//!
//! `muda` 留了一个全局回调槽。这里装进去的那个回调只做一件事：往
//! `futures_channel` 的 mpsc 上 `unbounded_send`。另有一个常驻的前台 [`Task`] 在
//! `await` 接收端。**没有轮询、没有定时器、没有后台线程、没有每帧 tick，更没有
//! 第二个事件循环。** 队列空时任务就是挂起的；唤醒它走 gpui 自己的调度器回到主线程，
//! 于是工作落在正常的更新周期里。
//!
//! 两点让它是对的而不是碰巧对：
//!
//! - **回调本来就在主线程上跑。** `muda` 从 `NSMenuItem` 的 action 发事件，是主 run
//!   loop 上同步发生的。这条通道不是线程跃迁，而是进 gpui 借用纪律的门。
//! - **回调的约束是 `Fn + Send + Sync`**，所以它抓不住 `AsyncApp`——gpui 的
//!   `ForegroundExecutor` 按构造就是 `!Send`。mpsc 的发送端是 `Send + Sync`，
//!   所以接缝必须是通道，而不是捕获一个上下文。
//!
//! **坑：** 那个槽是 `OnceCell`，发送端做的是 `get_or_init(|| None)`。只要有任何事件
//! 早于安装发生，槽位就被钉死成 `None`，之后所有安装都会被静默忽略，事件则堆在一个
//! 没人持有的通道里。所以 [`install_event_handlers`] 在菜单和状态栏项存在**之前**跑，
//! 而且只跑一次。
//!
//! # 失败从来不致命
//!
//! [`init`] 返回 `bool` 只是为了记一行日志。菜单栏少一个图标不该挡住应用启动——
//! 「没有菜单栏图标但能用」永远好过「起不来」。

mod mark;
mod menu;

use duck_components::DuckIcon;
use duck_theme::DuckThemeState;
use futures_channel::mpsc::{UnboundedReceiver, unbounded};
use futures_util::StreamExt as _;
use gpui::{App, AsyncApp, Global, Subscription, Task, WindowHandle};
use gpui_component::Root;
use tray_icon::menu::MenuEvent;
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::actions::{self, ids};
use crate::app::Desktop;
use crate::pages;
use crate::tray::menu::{TrayCommand, TrayMenu};

/// 托盘句柄。
///
/// **持有 [`TrayIcon`] 才是「图标留在菜单栏上」的原因**：它的 `Drop` 会调
/// `NSStatusBar::removeStatusItem`，于是把图标建在局部变量里的写法只会让图标活到
/// `init` 返回为止。
///
/// 做成 `Global` 的理由和别处一样：随处可读，只在一处写。`TrayIcon` 是 `!Send`
/// 的（里面是 `Rc<RefCell<..>>` 包着 `NSStatusItem`），这没关系——`Global` 只要求
/// `'static`，而 `App` 本来就绑在主线程上。
pub struct Tray {
    /// 只为了它的 `Drop`，没人读它。
    #[allow(dead_code)]
    icon: TrayIcon,
    /// 语言换了要重写标题，所以这个得留着。
    menu: TrayMenu,
    /// 收事件的常驻任务。持有而不是 `detach`，是为了让投递随全局一起结束。
    #[allow(dead_code)]
    events: Task<()>,
    /// 听语言变化。同样持有而不是 `detach`。
    #[allow(dead_code)]
    localization: Subscription,
}

impl Global for Tray {}

/// 建立托盘图标。
///
/// **不收返回值**，因为「图标没出来」没有可做的事：菜单栏少一个图标不该挡住应用启动，
/// 失败只在 `tracing` 里留一句。真需要判断的话，看 `cx.has_global::<Tray>()`。
pub fn init(cx: &mut App) {
    // 先装回调，再建菜单和图标——理由见模块文档里那段 `OnceCell`。
    let receiver = install_event_handlers();

    // 主机的答案整条链上只问一次，往下传。
    let host = mark::TrayHost::current();
    let menu = TrayMenu::new(cx);

    let mut builder = TrayIconBuilder::new()
        .with_menu(menu.as_context_menu())
        // 应用名只有一个，不翻译。
        .with_tooltip(duck_command::APP_MENU_NAME)
        // macOS 靠这个标志把位图当模板图（AppKit 只读 alpha）。别的平台忽略它。
        .with_icon_as_template(mark::is_template(host))
        // 左右键都开菜单：这个图标没有别的「单击」含义。
        .with_menu_on_left_click(true);

    match mark::render(DuckIcon::Box, host, cx) {
        Ok(icon) => builder = builder.with_icon(icon),
        // 没有图的状态栏项是一条很窄的空白点击区——不理想，但菜单还在，退出就在里面。
        Err(detail) => tracing::warn!(%detail, "no menu bar mark; the tray item will be blank"),
    }

    let icon = match builder.build() {
        Ok(icon) => icon,
        Err(error) => {
            tracing::warn!(%error, "could not create the menu bar item; continuing without one");
            return;
        }
    };

    let events = cx.spawn(async move |cx| drain(receiver, cx).await);
    // 语言切换整条链是 `update_appearance` → 重建 → 各窗口 `refresh()`，而 `NSMenu`
    // 不是 gpui 窗口，够不着。托盘得自己听一次。外观的其它改动也会吵醒这个观察者，
    // 所以 `relabel` 自己按当前标题去重（见 `TrayMenu::relabel`）。
    let localization = cx.observe_global::<DuckThemeState>(|cx| {
        if let Some(tray) = cx.try_global::<Tray>() {
            tray.menu.relabel(cx);
        }
    });

    cx.set_global(Tray {
        icon,
        menu,
        events,
        localization,
    });
}

/// 把 `muda` 的全局回调槽指向一条通道。
///
/// **在有任何东西能发事件之前调用**——见模块文档里那段 `OnceCell`。只从 [`init`] 调一次。
fn install_event_handlers() -> UnboundedReceiver<MenuEvent> {
    let (sender, receiver) = unbounded::<MenuEvent>();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        // 接收端没了说明应用正在退出；原生回调里不值得再记一条日志。
        let _ = sender.unbounded_send(event);
    }));
    receiver
}

/// 唯一的常驻监听者。队列空时挂起；发送端或任务没了就结束。
async fn drain(mut receiver: UnboundedReceiver<MenuEvent>, cx: &mut AsyncApp) {
    while let Some(event) = receiver.next().await {
        cx.update(|cx| {
            // 别的 `muda` 菜单也会往这条槽里发；不是自己人就放手。
            if let Some(command) = menu::command_for(&event.id) {
                run(command, cx);
            }
        });
    }
}

/// 执行一条托盘命令。
fn run(command: TrayCommand, cx: &mut App) {
    match command {
        TrayCommand::OpenToolbox => {
            if let Some(window) = show_main_window(cx) {
                // 走注册表里的动作，而不是直接找 shell：路由的落点只有一处定义。
                let _ = window.update(cx, |_, window, cx| {
                    duck_command::dispatch(ids::ROUTE_TOOLBOX, window, cx);
                });
            }
        }
        TrayCommand::CheckUpdate => {
            // 先要窗口：检查结论是一条 toast，没有窗口的话它就静默落进更新器状态里了。
            show_main_window(cx);
            crate::update::check_now(cx);
        }
        TrayCommand::OpenSettings => pages::open_settings(cx),
        TrayCommand::OpenLogs => pages::open_logs(cx),
        TrayCommand::Quit => actions::quit(cx),
    }
}

/// 保证主窗口真的在屏幕上，并把它的句柄交回来。
///
/// 走 [`crate::app::reveal_main_window`] 而不是直接 `open_main_window`：后者只做
/// `activate_window()`，而 macOS 上 `makeKeyAndOrderFront:` **不解除最小化**，应用被
/// `⌘H` 隐藏时也不会被带出来——用户看到的就是「点了菜单栏图标没反应」。多一步
/// `restore_visible()` 才把这两种状态一起收掉。这里不开命令面板（那是唤起键的语义）。
fn show_main_window(cx: &mut App) -> Option<WindowHandle<Root>> {
    crate::app::reveal_main_window(cx);
    Desktop::main_window(cx)
}
