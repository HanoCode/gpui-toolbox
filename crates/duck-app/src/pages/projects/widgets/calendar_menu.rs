//! 日历格的右键菜单 —— 一格 = 一天，菜单的动作全部围绕「这一天」。
//!
//! # 为什么不是 `task_menu`
//!
//! 日历上右键有两种意思，取决于落点：
//!
//! - 落在**任务条**上 → 用户想动的是那条任务 → 复用看板卡片那份
//!   [`super::task_menu::task_menu`]（同一条任务，换个视图不该换一套动作）；
//! - 落在**格子空白处** → 用户想动的是**这一天**（新建 / 看日志 / 换个粒度看它）
//!   → 本菜单。
//!
//! 两套菜单挂在不同的元素上，事件按元素就近命中，所以格子里既有任务又能右键空白。
//!
//! # 三项动作的落点
//!
//! 1. **在这天新建任务**：编辑器照常开，只是截止日预填成这一天
//!    （`ProjectsPage::new_task_on`）—— 在某一格上点「新建」，期望的第一件事就是
//!    「截止日已经是这天」；
//! 2. **打开这天的日报**：`LogKind::Daily` 的工作日志编辑器（与工作日志视图同一个弹窗）；
//! 3. **换个粒度看这一天**：月视图里看这周 / 周视图里看这月 —— 它同时挪锚与切画法
//!    （`ProjectsPage::show_day_as`），否则切完还停在原来的月份上，像点了没反应。
//!
//! 外加一条「复制日期」（`YYYY-MM-DD`）：日期文本是唯一不随语言变的东西，粘进
//! 命令行 / SQL / 别的工具都得用它，与「复制任务」同一个出口（`copy_export`）。

use gpui::{Context, SharedString, WeakEntity};
use gpui_component::{
    Icon,
    menu::{PopupMenu, PopupMenuItem},
};

use duck_components::DuckIcon;
use duck_project::{CalendarDate, LogKind};
use duck_theme::{tr, trf};

use super::super::ProjectsPage;
use super::super::prefs::CalendarMode;
use super::weekday_label;

/// 在 `menu` 上挂出这一天的全部动作。
///
/// `day` / `mode` 由调用方在**右键那一刻**给出（菜单闭包是 `'static` 的，格子里的
/// `CalendarDate` 是个把 `Copy` 类型，直接搬进去）。
pub(crate) fn calendar_menu(
    mut menu: PopupMenu,
    day: CalendarDate,
    mode: CalendarMode,
    view: &WeakEntity<ProjectsPage>,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    menu = menu
        .item(PopupMenuItem::label(SharedString::from(trf(
            cx,
            "{} 月 {} 日 · {}",
            &[
                &day.month().to_string(),
                &day.day().to_string(),
                weekday_label(day.weekday(), cx),
            ],
        ))))
        .item(
            PopupMenuItem::new(tr(cx, "在这天新建任务"))
                .icon(Icon::new(DuckIcon::Plus))
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |page, cx| page.new_task_on(day, window, cx));
                    }
                }),
        )
        .item(
            PopupMenuItem::new(tr(cx, "打开这天的日报"))
                .icon(Icon::new(DuckIcon::FileText))
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |page, cx| {
                            page.open_log_editor(day, LogKind::Daily, window, cx)
                        });
                    }
                }),
        );

    // 「换个粒度看这一天」的文案随当前画法翻转：在月视图里它给的是「看这周」，
    // 在周视图里给的是「看这月」—— 同一枚入口，永远指向**另一个**画法。
    let (label, icon, target) = match mode {
        CalendarMode::Month => (
            tr(cx, "在周视图里查看这周"),
            DuckIcon::Rows3,
            CalendarMode::Week,
        ),
        CalendarMode::Week => (
            tr(cx, "在月视图里查看这个月"),
            DuckIcon::Kanban,
            CalendarMode::Month,
        ),
    };
    menu.separator()
        .item(PopupMenuItem::new(label).icon(Icon::new(icon)).on_click({
            let view = view.clone();
            move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.show_day_as(target, day, cx));
            }
        }))
        .item(
            PopupMenuItem::new(tr(cx, "复制日期"))
                .icon(Icon::new(DuckIcon::Copy))
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |page, cx| page.copy_date(day, cx));
                    }
                }),
        )
}
