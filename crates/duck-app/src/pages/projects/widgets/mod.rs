//! 项目管理页的 9 个领域组件。
//!
//! 拍板 Q4：**页面私有**（整个模块只在 `pages::projects` 内可见），不进
//! `duck-components` 公共库 —— 它们的语义只对看板成立（列 / 卡片 / 优先级点 /
//! 标签片 / 象限徽标 / 进度条 / 详情面板 / 右键菜单 / 日历格菜单），进公共库会让
//! 「通用组件」里混进领域概念。

pub(crate) mod calendar_menu;
pub(crate) mod kanban_column;
pub(crate) mod priority_dot;
pub(crate) mod progress_bar;
pub(crate) mod quadrant_badge;
pub(crate) mod tag_chip;
pub(crate) mod task_card;
pub(crate) mod task_detail_panel;
pub(crate) mod task_menu;

use duck_project::{BoardColumn, CalendarDate, Quadrant, Task, TaskStatus, Weekday};
use duck_theme::{active_theme, tr, trf};
use gpui::{App, ClickEvent, Hsla, SharedString};

/// 看板拖拽的载荷。
///
/// 只带任务 id：落点那边自己去库里取最新值 —— 拖拽期间任务可能被别处改过，
/// 把整条 `Task` 装进载荷会让落点用上一帧的旧值去写。
#[derive(Clone)]
pub(crate) struct TaskDrag {
    /// 被拖的任务。
    pub(crate) task_id: i64,
}

/// 这次点击是不是双击。
///
/// 原型在一个全局 `dblclick` 处理器里对**任何**带 `data-card` 的元素开完整编辑器
/// （`projects-mock.html:3041-3045`），也就是卡片与列表行同一个手势。落在这里而不是
/// 各写一遍：三种 `ClickEvent`（鼠标 / 触摸 / 键盘）的连击字段名各不相同，漏一支就会
/// 在触摸板上变成「双击没反应」。
pub(crate) fn is_double_click(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(mouse) => mouse.up.click_count > 1,
        ClickEvent::Touch(tap) => tap.tap_count > 1,
        ClickEvent::Keyboard(_) => false,
    }
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

/// 截止日文案 + 颜色（原型 `dueLabel`，`docs/projects-mock.html:1041`）。
///
/// 看板卡片 `.pmeta` 与列表「截止」列共用一份：两处都只有一格宽，都该说「还差几天」
/// 而不是印一个 `2026-08-14` 让人自己算今天几号；各写一遍的话，迟早有一处漏掉
/// 「今天到期」这种只有当天才出现的分支。
///
/// 三种「要出事」的情形各自点名（逾期 N 天 / 今天到期 / 明天到期），其余给 `MM-DD`
/// —— 年份在几十行的清单里是噪声。已完成的回落到「`MM-DD` 完成」：那条日期这时是
/// **历史记录**，再按逾期去警告它没有意义。
pub(crate) fn due_label(task: &Task, today: CalendarDate, cx: &App) -> (SharedString, Hsla) {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let normal = tokens.colors.muted_foreground;
    let Some(due) = task.due_date else {
        return (SharedString::from(tr(cx, "未排期")), normal);
    };
    if task.status == TaskStatus::Done {
        return (
            SharedString::from(trf(cx, "{} 完成", &[&mmdd(due)])),
            normal,
        );
    }
    match today.days_until(due) {
        days if days < 0 => (
            SharedString::from(trf(cx, "逾期 {} 天", &[&(-days).to_string()])),
            tokens.colors.destructive,
        ),
        0 => (
            SharedString::from(tr(cx, "今天到期")),
            theme.extended().colors.warning,
        ),
        1 => (
            SharedString::from(tr(cx, "明天到期")),
            theme.extended().colors.warning,
        ),
        _ => (SharedString::from(mmdd(due)), normal),
    }
}

/// 星期的短名（周一起，与内核的 [`Weekday`] 同序）。
///
/// 日历的列头（月视图）与周视图的列头、日期格菜单共用一份：三处各写一遍
/// `match weekday` 的话，迟早有一处把周日排到周一前面 —— 而 `Weekday` 的顺序是
/// 内核定的（`index_from_monday`），界面只能跟着它走。
pub(crate) fn weekday_label(weekday: Weekday, cx: &App) -> &'static str {
    match weekday {
        Weekday::Monday => tr(cx, "周一"),
        Weekday::Tuesday => tr(cx, "周二"),
        Weekday::Wednesday => tr(cx, "周三"),
        Weekday::Thursday => tr(cx, "周四"),
        Weekday::Friday => tr(cx, "周五"),
        Weekday::Saturday => tr(cx, "周六"),
        Weekday::Sunday => tr(cx, "周日"),
    }
}

/// `MM-DD`（原型 `fmtMMDD`）。
fn mmdd(date: CalendarDate) -> String {
    format!("{:02}-{:02}", date.month(), date.day())
}
