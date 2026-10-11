//! 托盘菜单：一张条目表，和唯一一张「点哪一项 → 做什么」的路由表。
//!
//! # 为什么每项都写死 id
//!
//! `muda` 允许 `MenuItem::new` 自动派 id，但那样路由就只能靠**比对持有的句柄**——
//! 于是 [`command_for`] 必须先有一个 `Menu` 才成立，而 `muda::Menu::new` 在非主线程上
//! 是直接 panic 的。写死 id 之后路由退化成一次字符串 `match`：可以脱离 AppKit 单测，
//! 菜单也可以随时重建而不影响派发。
//!
//! 句柄仍然要留着——[`TrayMenu::relabel`] 要往它们上面写新文案。**持有即存活**：
//! `muda` 的条目是 `Rc` 的，丢掉句柄等于把菜单一起丢掉，状态栏项就点不出东西了。

use duck_theme::tr;
use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};

/// 菜单项 id。字符串常量集中在这里，[`command_for`] 与构建处共用同一份。
mod ids {
    /// 显示主窗口并切到工具箱页。
    pub const OPEN: &str = "tray.open";
    /// 显示主窗口并查一次更新。
    pub const CHECK_UPDATE: &str = "tray.check-update";
    /// 打开（或前置）设置窗口。
    pub const SETTINGS: &str = "tray.settings";
    /// 在文件管理器中显示日志目录。
    pub const LOGS: &str = "tray.logs";
    /// 结束进程。
    pub const QUIT: &str = "tray.quit";
}

/// 点一下托盘菜单项意味着什么。
///
/// 只在托盘内部流转：别处不派发它，也就没有理由让它们能叫出这些名字。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    /// 显示主窗口（必要时重建）并切到工具箱页。
    OpenToolbox,
    /// 显示主窗口并查一次更新——没有窗口的话结论没有地方显示。
    CheckUpdate,
    /// 打开（或前置）设置窗口。
    OpenSettings,
    /// 在文件管理器中显示日志目录。
    OpenLogs,
    /// 结束进程。托盘常驻之后，这是「关掉窗口还剩什么」的答案之一。
    Quit,
}

/// 一个菜单项 id 对应的命令；不是自己人则 `None`。
///
/// 纯函数：路由规则可以单测，不需要先把菜单建出来。
#[must_use]
pub fn command_for(id: &MenuId) -> Option<TrayCommand> {
    Some(match id.0.as_str() {
        ids::OPEN => TrayCommand::OpenToolbox,
        ids::CHECK_UPDATE => TrayCommand::CheckUpdate,
        ids::SETTINGS => TrayCommand::OpenSettings,
        ids::LOGS => TrayCommand::OpenLogs,
        ids::QUIT => TrayCommand::Quit,
        _ => return None,
    })
}

/// 菜单本体，以及 [`TrayMenu::relabel`] 需要改标题的那几项。
///
/// 两条分隔线的句柄不留：没有文案，也就没人再碰它们。
pub struct TrayMenu {
    menu: Menu,
    open: MenuItem,
    check_update: MenuItem,
    settings: MenuItem,
    logs: MenuItem,
    quit: MenuItem,
}

impl TrayMenu {
    /// 按当前语言建菜单。
    ///
    /// 必须在主线程上调用：`muda::Menu::new` 在非主线程上是 panic 而不是报错。
    /// 唯一的调用点是 [`crate::tray::init`]，而它在 `App::run` 的回调里。
    pub fn new(cx: &gpui::App) -> Self {
        let this = Self {
            menu: Menu::new(),
            open: item(ids::OPEN, tr(cx, "打开工具箱")),
            check_update: item(ids::CHECK_UPDATE, tr(cx, "检查更新")),
            settings: item(ids::SETTINGS, tr(cx, "设置")),
            logs: item(ids::LOGS, tr(cx, "打开日志")),
            quit: item(ids::QUIT, tr(cx, "退出 Duck Toolbox")),
        };
        this.assemble();
        this
    }

    /// 按顺序装进菜单。
    ///
    /// 每一条 append 都是可失败的，而失败不值得让整个菜单建不出来：少一行仍然是
    /// 能用的菜单，而且退出就在同一张菜单里。失败只记一条日志。
    fn assemble(&self) {
        // 三条线：名词（打开）、动词（查更新 / 设置 / 日志）、退出，各成一组。
        let separator = PredefinedMenuItem::separator();
        let items: [&dyn tray_icon::menu::IsMenuItem; 7] = [
            &self.open,
            &separator,
            &self.check_update,
            &self.settings,
            &self.logs,
            &separator,
            &self.quit,
        ];
        for entry in items {
            if let Err(error) = self.menu.append(entry) {
                tracing::warn!(%error, "could not add a tray menu entry");
            }
        }
    }

    /// 交给 `TrayIconBuilder::with_menu`。
    pub fn as_context_menu(&self) -> Box<dyn tray_icon::menu::ContextMenu> {
        Box::new(self.menu.clone())
    }

    /// 按当前语言重写全部标题。
    ///
    /// `NSMenu` 不是 gpui 窗口：切语言时重新渲染所有窗口够不着菜单栏，托盘得自己听。
    /// 由 [`crate::tray::init`] 里那个 `observe_global::<DuckThemeState>` 触发。
    ///
    /// **每一项都先比后写。** 那个观察者对**任何**外观改动都会响——拖一下强调色色点
    /// 就是几十次——而菜单栏标题只在语言变了才需要动。比一次字符串比写一次
    /// `NSMenuItem.title` 便宜得多，而且让这个函数天然幂等。
    pub fn relabel(&self, cx: &gpui::App) {
        retitle(&self.open, tr(cx, "打开工具箱"));
        retitle(&self.check_update, tr(cx, "检查更新"));
        retitle(&self.settings, tr(cx, "设置"));
        retitle(&self.logs, tr(cx, "打开日志"));
        retitle(&self.quit, tr(cx, "退出 Duck Toolbox"));
    }
}

/// 标题不同才写回去。
fn retitle(item: &MenuItem, text: &str) {
    if item.text() != text {
        item.set_text(text);
    }
}

/// 建一个启用、无快捷键的条目。
///
/// 不给快捷键：菜单栏项本来就靠鼠标点开，而 `muda` 的 accelerator 会全局抢键——
/// 同一份动作在应用菜单里已经有键位了（见 `duck_command::refresh_menus`）。
fn item(id: &str, text: &str) -> MenuItem {
    MenuItem::with_id(id, text, true, None)
}

#[cfg(test)]
mod tests {
    use super::{TrayCommand, command_for, ids};
    use tray_icon::menu::MenuId;

    #[test]
    fn every_tray_id_routes_to_exactly_one_command() {
        let table = [
            (ids::OPEN, TrayCommand::OpenToolbox),
            (ids::CHECK_UPDATE, TrayCommand::CheckUpdate),
            (ids::SETTINGS, TrayCommand::OpenSettings),
            (ids::LOGS, TrayCommand::OpenLogs),
            (ids::QUIT, TrayCommand::Quit),
        ];
        for (id, expected) in table {
            assert_eq!(command_for(&MenuId::new(id)), Some(expected));
        }
    }

    /// id 是手写的字符串，所以「两条项撞 id」是能发生的错误；这里把它挡住。
    #[test]
    fn the_ids_are_distinct() {
        let all = [
            ids::OPEN,
            ids::CHECK_UPDATE,
            ids::SETTINGS,
            ids::LOGS,
            ids::QUIT,
        ];
        for (index, id) in all.iter().enumerate() {
            assert!(
                !all[index + 1..].contains(id),
                "duplicate tray menu id: {id}"
            );
        }
    }

    /// 事件也会从别的 `muda` 菜单过来（gpui 自己那套除外，它是另一条链）；不是自己人就该放手。
    #[test]
    fn a_foreign_id_is_not_mine() {
        assert_eq!(command_for(&MenuId::new("menu.copy")), None);
    }
}
