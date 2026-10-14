//! 项目管理页的 7 个领域组件。
//!
//! 拍板 Q4：**页面私有**（整个模块只在 `pages::projects` 内可见），不进
//! `duck-components` 公共库 —— 它们的语义只对看板成立（列 / 卡片 / 优先级点 /
//! 标签片 / 进度条 / 详情面板 / 右键菜单），进公共库会让「通用组件」里混进领域概念。

pub(crate) mod kanban_column;
pub(crate) mod priority_dot;
pub(crate) mod progress_bar;
pub(crate) mod tag_chip;
pub(crate) mod task_card;
pub(crate) mod task_detail_panel;
pub(crate) mod task_menu;

use duck_project::{BoardColumn, Quadrant, TaskStatus};
use duck_theme::tr;
use gpui::{App, SharedString};

/// 看板拖拽的载荷。
///
/// 只带任务 id：落点那边自己去库里取最新值 —— 拖拽期间任务可能被别处改过，
/// 把整条 `Task` 装进载荷会让落点用上一帧的旧值去写。
#[derive(Clone)]
pub(crate) struct TaskDrag {
    /// 被拖的任务。
    pub(crate) task_id: i64,
}

/// 看板列 / 状态的中文名。
///
/// 列名优先用用户自己改的（`BoardColumn::name`），没改过才回落这档内置文案 ——
/// 内核存的是 `NULL`，界面文案必须留在界面层（内核不许有中文字面量）。
///
/// `Blocked` 用「阻塞」而不是「受阻」：内核 `store/columns.rs` 的模块头写的默认
/// 四列就是「待办 / 进行中 / 阻塞 / 已完成」，两处一个词，界面上再叫「受阻」等于
/// 给同一档状态起了两个名字。
pub(crate) fn status_label(status: TaskStatus, cx: &App) -> &'static str {
    match status {
        TaskStatus::Todo => tr(cx, "待办"),
        TaskStatus::Doing => tr(cx, "进行中"),
        TaskStatus::Blocked => tr(cx, "阻塞"),
        TaskStatus::Done => tr(cx, "已完成"),
        TaskStatus::Deferred => tr(cx, "已搁置"),
        TaskStatus::Cancelled => tr(cx, "已取消"),
    }
}

/// 一列的显示名。
///
/// 列头与卡片菜单的「状态…」子菜单共用一份：两处各写一遍 `match column.name`，
/// 迟早有一处漏掉「用户改过名」这条分支。
pub(crate) fn column_title(column: &BoardColumn, cx: &App) -> SharedString {
    match &column.name {
        Some(name) => SharedString::from(name.clone()),
        None => SharedString::from(status_label(column.status_key, cx)),
    }
}

/// 四象限的名字 + 处置建议（原型 `QUAD`，`projects-mock.html:955`）。
///
/// 连建议一起给：只写「Q2」的话，用的人得先想起来 Q2 是「重要不紧急」才敢点。
pub(crate) fn quadrant_label(quadrant: Quadrant, cx: &App) -> &'static str {
    match quadrant {
        Quadrant::Q1 => tr(cx, "Q1 重要且紧急 · 立即处理"),
        Quadrant::Q2 => tr(cx, "Q2 重要不紧急 · 尽快安排或委派"),
        Quadrant::Q3 => tr(cx, "Q3 紧急不重要 · 计划安排"),
        Quadrant::Q4 => tr(cx, "Q4 都不紧急 · 有空再处理"),
    }
}
