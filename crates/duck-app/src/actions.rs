//! 应用动作清单：**在这里登记一次，四个输出面自动出现**。
//!
//! 每条 [`ActionMeta`] 说明「这个动作叫什么、长什么样、出现在哪里、默认什么键位」；
//! 具体做什么由处理器决定。快捷键、原生菜单、命令面板、活动栏按钮都由 `duck-command`
//! 从这份清单派生（见 `duck_command::bind_all` / `refresh_menus` / `palette_groups`）。

use gpui::{App, SharedString, Window};
use gpui_component::Icon;

use duck_command::{ActionMeta, ActionSlot, install_app_handler, install_handler};
use duck_components::DuckIcon;
use duck_theme::{Appearance, ThemeMode, ThemePreference, active_theme, apply, tr};

use crate::pages::open_settings;

/// 动作 id 常量（命令面板 / 菜单 / 快捷键共用同一套 id）。
pub mod ids {
    /// 打开命令面板。
    pub const PALETTE_TOGGLE: &str = "palette.toggle";
    /// 切到概览页。
    pub const ROUTE_OVERVIEW: &str = "route.overview";
    /// 切到工具箱页。
    pub const ROUTE_TOOLBOX: &str = "route.toolbox";
    /// 切到 SSH 终端页。
    pub const ROUTE_SSH: &str = "route.ssh";
    /// 切到数据库页。
    pub const ROUTE_DATABASE: &str = "route.database";
    /// 切到项目管理页。
    pub const ROUTE_PROJECTS: &str = "route.projects";
    /// 切到 AI 助手页。
    pub const ROUTE_ASSISTANT: &str = "route.assistant";
    /// 收起 / 展开侧栏。
    pub const VIEW_TOGGLE_SIDEBAR: &str = "view.toggle-sidebar";
    /// 明暗外观切换。
    pub const APPEARANCE_TOGGLE: &str = "appearance.toggle";
    /// 打开设置。
    pub const SETTINGS_OPEN: &str = "settings.open";
    /// 主题选择器（活动栏 Popover）。
    pub const THEME_PICK: &str = "theme.pick";
    /// 刷新当前工具列表（演示）。
    pub const TOOLBOX_REFRESH: &str = "toolbox.refresh";
    /// 检查更新。
    pub const UPDATE_CHECK: &str = "update.check";
    /// 在文件管理器中显示日志。
    pub const HELP_OPEN_LOGS: &str = "help.open-logs";
    /// 退出应用。
    pub const APP_QUIT: &str = "app.quit";

    // —— 标准菜单项：只出现在菜单栏的固定位置，不进命令面板 / 活动栏 ——
    //
    // id 直接引用 `duck_command` 里的常量：菜单项在那边静态搭出来（位置由 macOS 习惯
    // 定死），这里只负责登记处理器与键位 —— 两处用同一个常量，不会写岔。
    /// 关于 Duck Toolbox（打开设置的「关于」页）。
    pub const APP_ABOUT: &str = duck_command::ABOUT_COMMAND;
    /// 隐藏应用。
    pub const WINDOW_HIDE: &str = duck_command::HIDE_COMMAND;
    /// 隐藏其他应用。
    pub const WINDOW_HIDE_OTHERS: &str = duck_command::HIDE_OTHERS_COMMAND;
    /// 显示全部应用。
    pub const WINDOW_SHOW_ALL: &str = duck_command::SHOW_ALL_COMMAND;
    /// 最小化当前窗口。
    pub const WINDOW_MINIMIZE: &str = duck_command::MINIMIZE_COMMAND;
    /// 缩放当前窗口。
    pub const WINDOW_ZOOM: &str = duck_command::ZOOM_COMMAND;
    /// 关闭当前窗口。
    pub const WINDOW_CLOSE: &str = duck_command::CLOSE_COMMAND;
}

/// 登记全部动作元数据。
pub fn declare_all(cx: &mut App) {
    // —— 视图 ——
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::PALETTE_TOGGLE,
            label: |cx| SharedString::from(tr(cx, "命令面板")),
            keywords: &["command", "palette", "命令", "面板"],
            group: Some("导航"),
            icon: Some(|_| Icon::new(DuckIcon::Search)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", None)),
            keys: &["$mod-k", "$mod-shift-p"],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::VIEW_TOGGLE_SIDEBAR,
            label: |cx| SharedString::from(tr(cx, "切换侧栏")),
            keywords: &["sidebar", "侧栏", "面板"],
            group: Some("视图"),
            icon: Some(|_| Icon::new(DuckIcon::PanelRight)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", None)),
            keys: &["$mod-b"],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::ROUTE_OVERVIEW,
            label: |cx| SharedString::from(tr(cx, "概览")),
            keywords: &["overview", "home", "概览", "首页"],
            group: Some("导航"),
            icon: Some(|_| Icon::new(DuckIcon::Gauge)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", Some("导航"))),
            keys: &["$mod-1"],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::ROUTE_TOOLBOX,
            label: |cx| SharedString::from(tr(cx, "工具箱")),
            keywords: &["toolbox", "tools", "工具箱", "工具"],
            group: Some("导航"),
            icon: Some(|_| Icon::new(DuckIcon::Box)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", Some("导航"))),
            keys: &["$mod-2"],
        },
    );
    // 4 条功能占位路由：入口先挂上，页面是「功能正在建设中」的空白页。
    // 关键词全用 ASCII，省去 i18n 译条（标签本身已进 EN 表）。
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::ROUTE_SSH,
            label: |cx| SharedString::from(tr(cx, "SSH 终端")),
            keywords: &["ssh", "terminal", "shell"],
            group: Some("导航"),
            icon: Some(|_| Icon::new(DuckIcon::SquareTerminal)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", Some("导航"))),
            keys: &["$mod-3"],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::ROUTE_DATABASE,
            label: |cx| SharedString::from(tr(cx, "数据库")),
            keywords: &["database", "db", "sql"],
            group: Some("导航"),
            icon: Some(|_| Icon::new(DuckIcon::Database)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", Some("导航"))),
            keys: &["$mod-4"],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::ROUTE_PROJECTS,
            label: |cx| SharedString::from(tr(cx, "项目管理")),
            keywords: &["projects", "project", "kanban", "tasks"],
            group: Some("导航"),
            icon: Some(|_| Icon::new(DuckIcon::Kanban)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", Some("导航"))),
            keys: &["$mod-5"],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::ROUTE_ASSISTANT,
            label: |cx| SharedString::from(tr(cx, "AI 助手")),
            keywords: &["assistant", "ai", "chat"],
            group: Some("导航"),
            icon: Some(|_| Icon::new(DuckIcon::Bot)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", Some("导航"))),
            keys: &["$mod-6"],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::APPEARANCE_TOGGLE,
            label: |cx| SharedString::from(tr(cx, "切换外观")),
            keywords: &[
                "appearance",
                "theme",
                "dark",
                "light",
                "外观",
                "主题",
                "深色",
                "浅色",
            ],
            group: Some("外观"),
            // 浅色显示月亮、深色显示太阳：图标随主题解析。
            icon: Some(|cx| {
                Icon::new(match active_theme(cx).mode() {
                    ThemeMode::Dark => DuckIcon::Sun,
                    ThemeMode::Light => DuckIcon::Moon,
                })
            }),
            // 深色下太阳取暖黄，浅色下月亮用二级文字色。
            icon_color: Some(|cx| {
                let theme = active_theme(cx);
                match theme.mode() {
                    ThemeMode::Dark => theme.extended().colors.sun,
                    ThemeMode::Light => theme.tokens().colors.muted_foreground,
                }
            }),
            slots: &[
                ActionSlot::ActivityBar,
                ActionSlot::Palette,
                ActionSlot::Menu,
            ],
            menu: Some(("视图", Some("外观"))),
            keys: &["$mod-shift-d"],
        },
    );

    // —— 应用菜单（macOS 上偏好设置的传统位置）——
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::SETTINGS_OPEN,
            label: |cx| SharedString::from(tr(cx, "设置")),
            keywords: &["settings", "preferences", "设置", "偏好"],
            group: Some("应用"),
            icon: Some(|_| Icon::new(DuckIcon::Settings)),
            icon_color: None,
            slots: &[
                ActionSlot::ActivityBar,
                ActionSlot::Palette,
                ActionSlot::Menu,
            ],
            menu: Some((duck_command::APP_MENU_NAME, None)),
            keys: &[],
        },
    );
    // —— 主题选择器：只挂活动栏（点击由 Popover 接管，命令面板 / 菜单没有可锚定的按钮）——
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::THEME_PICK,
            label: |cx| SharedString::from(tr(cx, "主题")),
            keywords: &["theme", "palette", "主题", "配色"],
            group: Some("外观"),
            icon: Some(|_| Icon::new(DuckIcon::Palette)),
            icon_color: None,
            slots: &[ActionSlot::ActivityBar],
            menu: None,
            keys: &[],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::APP_QUIT,
            label: |cx| SharedString::from(tr(cx, "退出 Duck Toolbox")),
            keywords: &["quit", "exit", "退出", "关闭"],
            group: Some("应用"),
            icon: Some(|_| Icon::new(DuckIcon::Power)),
            icon_color: None,
            // **不进 `Menu` 槽**：`refresh_menus` 已经在应用菜单末尾钉了一个「退出」
            // （见 `duck_command::menu`），这里再报一次会让那张菜单出现两条。
            // 命令面板要它，键位也要它。
            slots: &[ActionSlot::Palette],
            menu: None,
            // 整张菜单栏是被我们换掉的，AppKit 不再自带 Cmd-Q——得自己绑。
            keys: &["$mod-q"],
        },
    );

    // —— 工具 ——
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::TOOLBOX_REFRESH,
            label: |cx| SharedString::from(tr(cx, "刷新")),
            keywords: &["refresh", "reload", "刷新", "重载"],
            group: Some("工具"),
            icon: Some(|_| Icon::new(DuckIcon::RotateCw)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("工具", None)),
            keys: &["$mod-r"],
        },
    );

    // —— 帮助 ——
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::UPDATE_CHECK,
            label: |cx| SharedString::from(tr(cx, "检查更新")),
            keywords: &["update", "upgrade", "version", "更新", "升级", "版本"],
            group: Some("帮助"),
            icon: Some(|_| Icon::new(DuckIcon::Download)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("帮助", None)),
            keys: &[],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::HELP_OPEN_LOGS,
            label: |cx| SharedString::from(tr(cx, "打开日志")),
            keywords: &["log", "logs", "日志"],
            group: Some("帮助"),
            icon: Some(|_| Icon::new(DuckIcon::FileText)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("帮助", None)),
            keys: &[],
        },
    );

    // —— 标准菜单项（关于 / 隐藏三件套 / 窗口控制）——
    //
    // 只登记元数据与键位：`slots: &[]` + `menu: None` 让它们**不**出现在命令面板与
    // 活动栏里（「隐藏其他应用」不该是一条可搜索命令），菜单栏上的固定位置由
    // `duck_command::refresh_menus` 直接搭出来。
    //
    // 键位仍走注册表，菜单项因此自动带上 ⌘H / ⌘M / ⌘W —— 键位串只有这一处来源。
    macro_rules! standard_item {
        ($id:expr, $label:literal, $keys:expr) => {
            duck_command::declare(
                cx,
                ActionMeta {
                    id: $id,
                    label: |cx| SharedString::from(tr(cx, $label)),
                    keywords: &[],
                    group: None,
                    icon: None,
                    icon_color: None,
                    slots: &[],
                    menu: None,
                    keys: $keys,
                },
            );
        };
    }
    standard_item!(ids::APP_ABOUT, "关于 Duck Toolbox", &[]);
    standard_item!(ids::WINDOW_HIDE, "隐藏 Duck Toolbox", &["$mod-h"]);
    standard_item!(ids::WINDOW_HIDE_OTHERS, "隐藏其他", &["$mod-alt-h"]);
    standard_item!(ids::WINDOW_SHOW_ALL, "显示全部", &[]);
    standard_item!(ids::WINDOW_MINIMIZE, "最小化", &["$mod-m"]);
    standard_item!(ids::WINDOW_ZOOM, "缩放", &[]);
    standard_item!(ids::WINDOW_CLOSE, "关闭", &["$mod-w"]);
}

/// 安装不依赖视图的处理器。
///
/// 这里全部登记成 **App 级**：动作本身不需要窗口，而窗口全关时（macOS 关窗后应用仍驻留）
/// 菜单栏上的「退出」「打开设置」「打开日志」「检查更新」恰恰最需要能用。
/// 需要弹 toast 的动作（例如刷新）留在窗口级。
pub fn install_stateless_handlers(cx: &mut App) {
    install_app_handler(cx, ids::APPEARANCE_TOGGLE, |cx| {
        toggle_appearance(cx);
    });
    install_app_handler(cx, ids::SETTINGS_OPEN, |cx| {
        open_settings(cx);
    });
    install_app_handler(cx, ids::HELP_OPEN_LOGS, |cx| {
        crate::pages::open_logs(cx);
    });
    install_app_handler(cx, ids::UPDATE_CHECK, |cx| {
        crate::update::check_now(cx);
    });
    install_app_handler(cx, ids::APP_QUIT, quit);
    install_handler(cx, ids::TOOLBOX_REFRESH, |window, cx| {
        duck_components::notify::info(tr(cx, "演示：刷新（未接入工具后端）"), window, cx);
    });

    // 标准菜单项的落点。隐藏 / 隐藏其他 / 显示全部 / 关于都不依赖窗口（窗口全关时
    // 菜单栏仍可点），登记成 App 级；窗口控制三项天然需要窗口。
    install_app_handler(cx, ids::APP_ABOUT, |cx| {
        crate::pages::open_settings_at("about", None, cx);
    });
    install_app_handler(cx, ids::WINDOW_HIDE, |cx| cx.hide());
    install_app_handler(cx, ids::WINDOW_HIDE_OTHERS, |cx| cx.hide_other_apps());
    install_app_handler(cx, ids::WINDOW_SHOW_ALL, |_cx| {
        duck_platform::show_all_apps();
    });
    install_handler(cx, ids::WINDOW_MINIMIZE, |window, _| {
        window.minimize_window();
    });
    install_handler(cx, ids::WINDOW_ZOOM, |window, _| window.zoom_window());
    install_handler(cx, ids::WINDOW_CLOSE, |window, _| window.remove_window());
}

/// 安装依赖 shell 的处理器（窗口创建后调用）。
pub fn install_shell_handlers(cx: &mut App, shell: &gpui::Entity<duck_shell::ShellView>) {
    let weak = shell.downgrade();

    let navigate = |id: &'static str| {
        let weak = weak.clone();
        move |_window: &mut Window, cx: &mut App| {
            if let Some(shell) = weak.upgrade() {
                shell.update(cx, |shell, cx| shell.navigate_by_id(id, cx));
            }
        }
    };
    install_handler(cx, ids::ROUTE_OVERVIEW, navigate("overview"));
    install_handler(cx, ids::ROUTE_TOOLBOX, navigate("toolbox"));
    install_handler(cx, ids::ROUTE_SSH, navigate("ssh"));
    install_handler(cx, ids::ROUTE_DATABASE, navigate("database"));
    install_handler(cx, ids::ROUTE_PROJECTS, navigate("projects"));
    install_handler(cx, ids::ROUTE_ASSISTANT, navigate("assistant"));

    install_handler(cx, ids::VIEW_TOGGLE_SIDEBAR, {
        let weak = weak.clone();
        move |_window, cx| {
            if let Some(shell) = weak.upgrade() {
                shell.update(cx, |shell, cx| shell.toggle_active_sidebar(cx));
            }
        }
    });

    install_handler(cx, ids::PALETTE_TOGGLE, move |window, cx| {
        if let Some(shell) = weak.upgrade() {
            shell.update(cx, |shell, cx| shell.toggle_palette(window, cx));
        }
    });
}

/// 退出应用：先把待写数据落盘，再结束事件循环。
///
/// 抽成函数而不是写在处理器里，是因为托盘也要走这条路——托盘菜单里那个「退出」
/// 和命令面板里那个必须是同一件事，包括「先 flush」这一步。
pub fn quit(cx: &mut App) {
    duck_store::flush(cx);
    cx.quit();
}

/// 在当前明暗之间切换（保留其余外观设置）。
fn toggle_appearance(cx: &mut App) {
    let current = active_theme(cx).appearance().clone();
    let preference = match active_theme(cx).mode() {
        ThemeMode::Dark => ThemePreference::Light,
        ThemeMode::Light => ThemePreference::Dark,
    };
    apply(
        Appearance {
            preference,
            ..current
        },
        cx,
    );
}
