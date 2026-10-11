//! 示例页面：概览 / 工具箱 / 功能占位页，以及挂到 shell 顶栏的插槽视图与独立设置窗口。

mod data;
mod licenses;
mod logs;
mod onboarding;
mod overview;
mod placeholder;
pub(crate) mod plugin_center;
mod settings;
mod status_support;
mod toolbox;

pub use licenses::open_licenses;
pub use logs::{open_logs, prune_now, spawn_pruner};
pub use onboarding::{done as onboarding_done, open_onboarding};
pub use overview::OverviewPage;
pub use placeholder::PlaceholderPage;
pub use plugin_center::open_plugin_center;
pub use settings::{open_settings, open_settings_at};
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
/// 项目管理页路由。
pub const PROJECTS_ROUTE: RouteId = RouteId::new("projects");
/// AI 助手页路由。
pub const ASSISTANT_ROUTE: RouteId = RouteId::new("assistant");
