//! 示例页面：概览 / 工具箱 / 功能占位页，以及挂到 shell 顶栏的插槽视图与独立设置窗口。

mod assistant;
mod data;
mod db;
mod licenses;
mod logs;
mod mail;
mod mail_data;
mod onboarding;
mod overview;
mod placeholder;
pub(crate) mod plugin_center;
mod redis;
mod settings;
mod status_support;
mod terminal;
mod terminal_canvas;
mod terminal_data;
mod terminal_dialogs;
mod toolbox;

pub use assistant::AssistantPage;
pub use db::DatabasePage;
// 数据库页的键位在自己的上下文里绑，`app::install` 只调这一句；
// 键位表的读回校验由 `pages::db` 自己的测试做（同模块，不必把动作类型再导出一次）。
pub(crate) use db::bind_keys as bind_db_keys;
pub use licenses::open_licenses;
pub use logs::{open_logs, prune_now, spawn_pruner};
pub use mail::MailPage;
pub use onboarding::{done as onboarding_done, open_onboarding};
pub use overview::OverviewPage;
pub use placeholder::PlaceholderPage;
pub use plugin_center::open_plugin_center;
pub use redis::RedisPage;
pub use settings::{open_settings, open_settings_at};
pub use terminal::TerminalPage;
// 键位注册在 `app::install`（`cx.bind_keys`），动作类型要从这里跨出去给 app.rs 用。
pub(crate) use terminal_canvas::{
    Clear, CloseTab, CopySelection, DeleteToLineEnd, DeleteToLineStart, DeleteWordForward,
    MoveToLineEnd, MoveToLineStart, NewTab, PasteFromClipboard, ScrollLineDown, ScrollLineUp,
    ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop, SelectAll, SendCtrlC, SendTab,
    SendTabPrev, SplitPane, ZoomIn, ZoomOut, ZoomReset,
};
pub use toolbox::ToolboxPage;

/// 命令面板的「设置」分组用：分类清单与 key 还原（见 `palette_tools`）。
pub(crate) use settings::{palette_page_key, palette_pages};

use duck_shell::RouteId;

/// 概览页路由。
pub const OVERVIEW_ROUTE: RouteId = RouteId::new("overview");
/// 工具箱页路由。
pub const TOOLBOX_ROUTE: RouteId = RouteId::new("toolbox");
/// SSH 终端页路由。
pub const SSH_ROUTE: RouteId = RouteId::new("ssh");
/// 数据库页路由。
pub const DATABASE_ROUTE: RouteId = RouteId::new("database");
/// Redis / Valkey 页路由。
pub const REDIS_ROUTE: RouteId = RouteId::new("redis");
/// 项目管理页路由。
pub const PROJECTS_ROUTE: RouteId = RouteId::new("projects");
/// AI 助手页路由。
pub const ASSISTANT_ROUTE: RouteId = RouteId::new("assistant");
/// 邮箱页路由。
pub const EMAIL_ROUTE: RouteId = RouteId::new("email");
