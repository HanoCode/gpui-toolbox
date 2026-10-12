//! 工具目录：工具箱的注册入口。
//!
//! 内置工具已全部下线——侧栏分类、列表、搜索索引与工具级设置界面由
//! **插件贡献动态注册**（见 [`crate::plugins::init`]）：每个清单有效的插件
//! 把自己的工具贡献登记进 [`ToolRegistry`]，派生管线自动跟上。
//!
//! 本文件只保留两件事：
//! - [`run_placeholder`]：未接入运行轨（`wasm` / `process`）的统一占位执行入口；
//! - [`register_all`]：安装注册表并登记开发者诊断工具（仅开发者模式可见）。
//!
//! 智能粘贴的识别逻辑（[`duck_tools::paste`]）与具体工具解耦：目标工具未注册时
//! 识别结果被丢弃、退回普通粘贴，因此这里不注册任何粘贴目标。

use duck_components::DuckIcon;
use duck_theme::{tr, trf};
use duck_tools::{ToolMeta, ToolRegistry};
use gpui::{App, Window};

use crate::pages::DATABASE_ROUTE;

/// 统一的占位执行入口：记录使用次数，并提示该工具后端尚未接入。
///
/// 插件贡献的工具若运行轨尚未接入（`wasm` / `process`），复用同一提示。
pub(crate) fn run_placeholder(tool_id: &str, window: &mut Window, cx: &mut App) {
    let name = ToolRegistry::get(cx, tool_id).map_or("工具", |tool| tool.meta().name());
    let name = tr(cx, name);
    duck_components::notify::info(trf(cx, "「{}」后端尚未接入", &[name]), window, cx);
}

/// 「数据库」在工具目录里的执行入口（设计稿 §4.7）。
///
/// 这是个**导航型**条目，不是演示项：点开就是数据库页。导航语义不在工具里另写一份 ——
/// 走 `RunCommand` 转交给 `ids::ROUTE_DATABASE` 那条命令，与命令面板 / 原生菜单 / `⌘4`
/// 完全同一条路径（`window.dispatch_action` 内部已 `cx.defer`，不在派发期直接动 window）。
fn open_database(_tool_id: &str, window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        Box::new(duck_command::RunCommand {
            id: DATABASE_ROUTE.as_str().into(),
        }),
        cx,
    );
}

/// 安装注册表并登记开发者诊断工具。
pub fn register_all(cx: &mut App) {
    ToolRegistry::install(cx);

    // 数据库页本身是导航目标而非插件工具，但设计稿要求它出现在工具目录里
    // （列表 + 搜索索引一并派生），这样「搜 sql / 数据库」也能到得了。
    ToolRegistry::register(
        cx,
        ToolMeta::builder("nav.database", "数据库", DuckIcon::Database)
            .description("连接 SQLite / PostgreSQL / MySQL，浏览表、执行 SQL、编辑数据")
            .keywords(&["database", "db", "sql", "数据库", "查询"])
            .category(duck_tools::ToolCategory::Development)
            .run(open_database)
            .build(),
    );

    // 开发者工具：默认不出现，开启开发者模式（设置 → 工具）后才进列表。
    ToolRegistry::register(
        cx,
        ToolMeta::builder("debug.registry", "注册表检查", DuckIcon::Code)
            .description("列出全部已注册工具与其派生索引")
            .keywords(&["debug", "注册表", "registry", "开发者"])
            .category(duck_tools::ToolCategory::Development)
            .dev()
            .run(run_placeholder)
            .build(),
    );
}
