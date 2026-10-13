//! 应用动作清单：**在这里登记一次，四个输出面自动出现**。
//!
//! 每条 [`ActionMeta`] 说明「这个动作叫什么、长什么样、出现在哪里、默认什么键位」；
//! 具体做什么由处理器决定。快捷键、原生菜单、命令面板、活动栏按钮都由 `duck-command`
//! 从这份清单派生（见 `duck_command::bind_all` / `refresh_menus` / `palette_groups`）。

use gpui::{App, SharedString, Window};
use gpui_component::Icon;

use duck_command::{ActionMeta, ActionSlot, install_app_handler, install_handler};
use duck_components::DuckIcon;
use duck_theme::{Appearance, ThemeMode, ThemePreference, active_theme, apply, tr, trf};

use crate::pages::TerminalPage;
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
    /// 切到 Redis / Valkey 页。
    pub const ROUTE_REDIS: &str = "route.redis";
    /// 切到项目管理页。
    pub const ROUTE_PROJECTS: &str = "route.projects";
    /// 切到 AI 助手页。
    pub const ROUTE_ASSISTANT: &str = "route.assistant";
    /// 切到邮箱页。
    pub const ROUTE_EMAIL: &str = "route.email";
    /// 收起 / 展开侧栏。
    pub const VIEW_TOGGLE_SIDEBAR: &str = "view.toggle-sidebar";
    /// 明暗外观切换。
    pub const APPEARANCE_TOGGLE: &str = "appearance.toggle";
    /// 打开设置。
    pub const SETTINGS_OPEN: &str = "settings.open";
    /// 主题选择器（活动栏 Popover）。
    pub const THEME_PICK: &str = "theme.pick";
    /// 重新扫描插件目录并刷新工具列表。
    pub const TOOLBOX_REFRESH: &str = "toolbox.refresh";
    /// 检查更新。
    pub const UPDATE_CHECK: &str = "update.check";
    /// 在文件管理器中显示日志。
    pub const HELP_OPEN_LOGS: &str = "help.open-logs";
    /// 退出应用。
    pub const APP_QUIT: &str = "app.quit";

    // —— 终端页动作 ——
    //
    // 这些动作的**快捷键**绑在 `"Terminal"` 上下文里（见 `app::install` 的
    // `cx.bind_keys([...])`）。刻意**不**在 `ActionMeta::keys` 里再声明一份：
    // 那份会被 `duck_command::bind_all` 绑到全局上下文，`⌘K`（清屏）会与
    // `palette.toggle` 直接撞车，`⌘T` / `⌘D` 也会在设置页等地方被误触发。
    // 这里只做**命令面板可见性**：让它们能在面板里搜到、并可供菜单引用。
    /// 新建终端标签（本地 shell）。
    pub const TERMINAL_NEW_TAB: &str = "terminal.new-tab";
    /// 分屏 / 取消分屏当前标签。
    pub const TERMINAL_SPLIT_PANE: &str = "terminal.split-pane";
    /// 增大终端字号。
    pub const TERMINAL_ZOOM_IN: &str = "terminal.zoom-in";
    /// 减小终端字号。
    pub const TERMINAL_ZOOM_OUT: &str = "terminal.zoom-out";
    /// 重置终端字号。
    pub const TERMINAL_ZOOM_RESET: &str = "terminal.zoom-reset";
    /// 清空当前窗格。
    pub const TERMINAL_CLEAR: &str = "terminal.clear";
    /// 新建连接（打开连接编辑器）。
    pub const TERMINAL_NEW_CONNECTION: &str = "terminal.new-connection";
    /// 打开 SFTP 面板。
    pub const TERMINAL_OPEN_SFTP: &str = "terminal.open-sftp";

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
            id: ids::ROUTE_REDIS,
            label: |cx| SharedString::from(tr(cx, "Redis")),
            // 关键词全 ASCII，省去 i18n 译条（同上面四条的约定）。
            keywords: &["redis", "valkey", "kv", "cache"],
            group: Some("导航"),
            icon: Some(|_| Icon::new(DuckIcon::DatabaseZap)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", Some("导航"))),
            keys: &["$mod-5"],
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
            // Redis 插在数据库页之后占走了 `⌘5`，本项与 AI 助手顺延一档。
            keys: &["$mod-6"],
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
            keys: &["$mod-7"],
        },
    );
    // 邮箱：真实页面（`MailPage`，见 `app.rs` 的 `duck-route-email`）。
    // 活动栏最末一条，键位接在 AI 助手之后（`⌘8`），现有 ⌘1–⌘7 不动。
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::ROUTE_EMAIL,
            label: |cx| SharedString::from(tr(cx, "邮箱")),
            // 关键词全 ASCII，省去 i18n 译条（同上面几条的约定）。
            keywords: &["email", "mail", "inbox", "imap", "smtp"],
            group: Some("导航"),
            icon: Some(|_| Icon::new(DuckIcon::Mail)),
            icon_color: None,
            slots: &[ActionSlot::Palette, ActionSlot::Menu],
            menu: Some(("视图", Some("导航"))),
            keys: &["$mod-8"],
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

    // —— 终端（SSH 终端页的动作；此前只存在于键盘绑定里）——
    //
    // 关键词一律 ASCII（与上面几条占位路由同一条约定）：中文搜索词若写进
    // `keywords` 就得为每个词各补一条英译，而标签本身已是搜索目标。
    // 设计 §9 的口径是「一处登记，四处输出面同时生效」。这些动作原本只绑在
    // `"Terminal"` 上下文（`app::install`），于是**命令面板里搜不到**，菜单也引用不了。
    // 登记元数据后落进面板；键位仍由 `"Terminal"` 上下文那一份提供（见 `ids` 的说明）。
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::TERMINAL_NEW_TAB,
            label: |cx| SharedString::from(tr(cx, "新建终端标签")),
            keywords: &["new", "tab", "terminal", "shell"],
            group: Some("终端"),
            icon: Some(|_| Icon::new(DuckIcon::Plus)),
            icon_color: None,
            slots: &[ActionSlot::Palette],
            menu: None,
            keys: &[],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::TERMINAL_SPLIT_PANE,
            label: |cx| SharedString::from(tr(cx, "切换分屏")),
            keywords: &["split", "pane", "columns"],
            group: Some("终端"),
            icon: Some(|_| Icon::new(DuckIcon::Columns2)),
            icon_color: None,
            slots: &[ActionSlot::Palette],
            menu: None,
            keys: &[],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::TERMINAL_ZOOM_IN,
            label: |cx| SharedString::from(tr(cx, "增大终端字号")),
            keywords: &["zoom", "font", "larger", "increase"],
            group: Some("终端"),
            icon: Some(|_| Icon::new(DuckIcon::Plus)),
            icon_color: None,
            slots: &[ActionSlot::Palette],
            menu: None,
            keys: &[],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::TERMINAL_ZOOM_OUT,
            label: |cx| SharedString::from(tr(cx, "减小终端字号")),
            keywords: &["zoom", "font", "smaller", "decrease"],
            group: Some("终端"),
            icon: Some(|_| Icon::new(DuckIcon::Minus)),
            icon_color: None,
            slots: &[ActionSlot::Palette],
            menu: None,
            keys: &[],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::TERMINAL_ZOOM_RESET,
            label: |cx| SharedString::from(tr(cx, "重置终端字号")),
            keywords: &["zoom", "font", "reset"],
            group: Some("终端"),
            icon: Some(|_| Icon::new(DuckIcon::RotateCw)),
            icon_color: None,
            slots: &[ActionSlot::Palette],
            menu: None,
            keys: &[],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::TERMINAL_CLEAR,
            label: |cx| SharedString::from(tr(cx, "清屏")),
            keywords: &["clear", "screen", "scrollback"],
            group: Some("终端"),
            icon: Some(|_| Icon::new(DuckIcon::Trash2)),
            icon_color: None,
            slots: &[ActionSlot::Palette],
            menu: None,
            keys: &[],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::TERMINAL_NEW_CONNECTION,
            label: |cx| SharedString::from(tr(cx, "新建连接")),
            keywords: &["new", "connection", "ssh", "host"],
            group: Some("终端"),
            icon: Some(|_| Icon::new(DuckIcon::Server)),
            icon_color: None,
            slots: &[ActionSlot::Palette],
            menu: None,
            keys: &[],
        },
    );
    duck_command::declare(
        cx,
        ActionMeta {
            id: ids::TERMINAL_OPEN_SFTP,
            label: |cx| SharedString::from(tr(cx, "打开 SFTP 面板")),
            keywords: &["sftp", "files", "transfer", "upload"],
            group: Some("终端"),
            icon: Some(|_| Icon::new(DuckIcon::Folders)),
            icon_color: None,
            slots: &[ActionSlot::Palette],
            menu: None,
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
    // 「刷新」是**真动作**（`⌘R` / 菜单「工具」/ 命令面板 / 活动栏同一份声明），不是演示项：
    // 重扫插件目录并重建工具注册表 —— 用户手改了插件目录、或想强制重读清单时走这一条。
    install_handler(cx, ids::TOOLBOX_REFRESH, |window, cx| {
        crate::plugins::rescan(cx);
        let tools = duck_tools::ToolRegistry::listed(cx).len();
        duck_components::notify::info(
            trf(
                cx,
                "已重新扫描插件目录，当前列出 {} 个工具。",
                &[&tools.to_string()],
            ),
            window,
            cx,
        );
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
    install_handler(cx, ids::ROUTE_REDIS, navigate("redis"));
    install_handler(cx, ids::ROUTE_PROJECTS, navigate("projects"));
    install_handler(cx, ids::ROUTE_ASSISTANT, navigate("assistant"));
    install_handler(cx, ids::ROUTE_EMAIL, navigate("email"));

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

/// 安装终端页的动作处理器。
///
/// 这些动作的操作对象是**终端页实体**（不是 shell），所以单独一个函数、在
/// `build_main_window` 里拿到 `TerminalPage` 之后调用。每条都先切到 SSH 路由 ——
/// 用户从命令面板触发「新建终端标签」时，如果当前停在设置页，动作生效了却看不见，
/// 表现就是「点了没反应」。
///
/// 处理器收弱引用：主窗关掉后终端页应当自然失效，而不是被全局注册表钉住不释放。
pub fn install_terminal_handlers(cx: &mut App, terminal: &gpui::WeakEntity<TerminalPage>) {
    install_handler(cx, ids::TERMINAL_NEW_TAB, {
        let terminal = terminal.clone();
        move |_window, cx| {
            crate::app::navigate_to(cx, crate::pages::SSH_ROUTE);
            if let Some(page) = terminal.upgrade() {
                page.update(cx, |page, cx| page.new_local_tab(cx));
            }
        }
    });
    install_handler(cx, ids::TERMINAL_SPLIT_PANE, {
        let terminal = terminal.clone();
        move |_window, cx| {
            crate::app::navigate_to(cx, crate::pages::SSH_ROUTE);
            if let Some(page) = terminal.upgrade() {
                page.update(cx, |page, cx| page.toggle_split(cx));
            }
        }
    });
    install_handler(cx, ids::TERMINAL_ZOOM_IN, {
        let terminal = terminal.clone();
        move |_window, cx| {
            if let Some(page) = terminal.upgrade() {
                page.update(cx, |page, cx| page.nudge_font_size(1., cx));
            }
        }
    });
    install_handler(cx, ids::TERMINAL_ZOOM_OUT, {
        let terminal = terminal.clone();
        move |_window, cx| {
            if let Some(page) = terminal.upgrade() {
                page.update(cx, |page, cx| page.nudge_font_size(-1., cx));
            }
        }
    });
    install_handler(cx, ids::TERMINAL_ZOOM_RESET, {
        let terminal = terminal.clone();
        move |_window, cx| {
            if let Some(page) = terminal.upgrade() {
                page.update(cx, |page, cx| page.reset_font_size(cx));
            }
        }
    });
    install_handler(cx, ids::TERMINAL_CLEAR, {
        let terminal = terminal.clone();
        move |_window, cx| {
            crate::app::navigate_to(cx, crate::pages::SSH_ROUTE);
            if let Some(page) = terminal.upgrade() {
                page.update(cx, |page, cx| page.clear_active(cx));
            }
        }
    });
    install_handler(cx, ids::TERMINAL_NEW_CONNECTION, {
        let terminal = terminal.clone();
        move |window, cx| {
            crate::app::navigate_to(cx, crate::pages::SSH_ROUTE);
            if let Some(page) = terminal.upgrade() {
                page.update(cx, |page, cx| page.open_new_connection(window, cx));
            }
        }
    });
    install_handler(cx, ids::TERMINAL_OPEN_SFTP, {
        let terminal = terminal.clone();
        move |window, cx| {
            crate::app::navigate_to(cx, crate::pages::SSH_ROUTE);
            if let Some(page) = terminal.upgrade() {
                page.update(cx, |page, cx| page.open_sftp_for_current(window, cx));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use duck_command::metas;
    use gpui::TestAppContext;

    /// 「数据库」页必须能从命令面板搜到（B11 的验收点），并且带着它的键位。
    ///
    /// 这条测试同时**钉住「不要往 duck-tools 里再登记一条同名的 ToolMeta」**：
    /// 面板条目完全由动作注册表派生，重复登记会在面板里冒出两个「数据库」。
    #[gpui::test]
    fn database_page_is_searchable_from_the_palette(cx: &mut TestAppContext) {
        cx.update(|cx| {
            duck_command::install(cx);
            declare_all(cx);
        });
        cx.update(|cx| {
            let meta = metas(cx)
                .iter()
                .find(|meta| meta.id == ids::ROUTE_DATABASE)
                .expect("数据库路由必须已声明");
            assert!(
                meta.slots.contains(&ActionSlot::Palette),
                "路由必须进命令面板，否则只能靠点侧栏到达"
            );
            for keyword in ["database", "db", "sql"] {
                assert!(
                    meta.keywords.contains(&keyword),
                    "命令面板要能用 `{keyword}` 搜到数据库页"
                );
            }
            let keys = duck_command::keybindings_for(cx, ids::ROUTE_DATABASE);
            assert!(!keys.is_empty(), "数据库页要有默认键位（设计稿定的是 ⌘4）");
        });
    }

    /// 终端页的动作必须能从命令面板搜到（设计 §9 的「四处输出面同时生效」）。
    ///
    /// 这些动作原本只存在于 `"Terminal"` 上下文的键盘绑定里 —— 键按下去有效，
    /// 但**面板里搜不到、菜单也引用不了**。这条钉住的是「登记进 ActionMeta」这一步
    /// 没有回退（回退的表现是面板里少了一批终端命令，肉眼很难注意到）。
    #[gpui::test]
    fn terminal_actions_are_searchable_from_the_palette(cx: &mut TestAppContext) {
        cx.update(|cx| {
            duck_command::install(cx);
            declare_all(cx);
        });
        cx.update(|cx| {
            let terminal_ids = [
                ids::TERMINAL_NEW_TAB,
                ids::TERMINAL_SPLIT_PANE,
                ids::TERMINAL_ZOOM_IN,
                ids::TERMINAL_ZOOM_OUT,
                ids::TERMINAL_ZOOM_RESET,
                ids::TERMINAL_CLEAR,
                ids::TERMINAL_NEW_CONNECTION,
                ids::TERMINAL_OPEN_SFTP,
            ];
            for id in terminal_ids {
                let meta = metas(cx)
                    .iter()
                    .find(|meta| meta.id == id)
                    .unwrap_or_else(|| panic!("终端动作 {id} 必须已声明"));
                assert!(
                    meta.slots.contains(&ActionSlot::Palette),
                    "{id} 必须进命令面板，否则只能靠按键到达"
                );
                assert!(
                    !meta.keywords.is_empty(),
                    "{id} 要有搜索词，否则面板里只能靠中文全名匹配"
                );
            }
        });
    }
}
