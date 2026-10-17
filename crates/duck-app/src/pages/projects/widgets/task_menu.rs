//! 任务卡片的右键菜单 —— 原型 `cardMenu()`（`docs/projects-mock.html:2682`）的逐条对应。
//!
//! 看板卡片与列表行**共用**这一份：原型里两者都挂 `data-card`，全局 `contextmenu`
//! 处理器一视同仁地弹同一个菜单（`projects-mock.html:3045-3052`），所以这里也收成
//! 一个函数，而不是各写一份 —— 两处各写一遍，迟早只有一处跟着原型改。
//!
//! # 与原型的差异
//!
//! 1. **「删除任务」走二次确认**：内核 `delete_task` 是**物理删除**（子任务随
//!    `ON DELETE CASCADE` 一起没），和原型那个「从数组里挪走 + 底部提示条 7 秒
//!    撤销」的软删除不是一回事。本仓对不可逆动作的既有口径就是「先弹确认」（§5.6），
//!    看板的 `⌫` 与归档中心的彻底删除都这么做，这里复用它
//!    （`ConfirmScope::DeleteTask`）而不是另开一条不设防的路径。
//! 2. **回执不带「撤销」按钮**：同一条理由 —— 写进库里的动作靠二次确认拦在前面，
//!    而不是靠「先做了再撤回」。原型里 `pushUndo` 那条 7 秒撤销带子在本仓没有对应物。
//!
//! # 子菜单怎么建
//!
//! `ContextMenuExt::context_menu` 只给一个「菜单值 + 上下文」，没有 `Entity`，
//! 所以子菜单用 [`PopupMenu::build`] 先建成实体，再交给
//! `PopupMenuItem::submenu`。`gpui-component` 明确支持这条路径：`PopupMenu::render`
//! 会给「构造时还没接上 parent」的子菜单补 parent 与层级优先级
//! （`menu/popup_menu.rs:1429-1445`），它自带的 `native_menu::fallback` 也是这么用的。

use gpui::{Context, Entity, SharedString, WeakEntity, Window};
use gpui_component::{
    Icon,
    menu::{PopupMenu, PopupMenuItem},
};

use duck_components::DuckIcon;
use duck_project::{BoardColumn, Priority, Project, Quadrant, Task, TaskStatus};
use duck_theme::tr;

use super::super::ProjectsPage;
use super::column_title;
use super::priority_dot::priority_label;
use super::quadrant_label;

/// 在 `menu` 上挂出这条任务的全部操作。
///
/// `task` / `projects` / `columns` 由调用方**在菜单要弹的那一刻**现读（见
/// `ProjectsPage::menu_data`）：菜单闭包是 `'static` 的，若改成渲染时捕获，一屏
/// 几十张卡片每帧都要克隆一份全项目表。
pub(crate) fn task_menu(
    mut menu: PopupMenu,
    task: &Task,
    projects: &[Project],
    columns: &[BoardColumn],
    view: &WeakEntity<ProjectsPage>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let id = task.id;
    let status = task.status;
    let starred = task.starred;

    // ---- 详情 ----
    menu = menu
        .item(PopupMenuItem::label(tr(cx, "任务操作")))
        .item(
            PopupMenuItem::new(tr(cx, "在右栏查看详情"))
                .icon(Icon::new(DuckIcon::FileText))
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |page, cx| page.select_task(Some(id), cx));
                    }
                }),
        )
        .item(
            PopupMenuItem::new(tr(cx, "打开完整编辑器"))
                .icon(Icon::new(DuckIcon::Pencil))
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        // `preset_project` / `preset_status` / `preset_due` 只对**新建**
                        // 有意义，编辑既有任务时内核自己会把原值读回来，所以这里传 `None`
                        // 加当前状态即可。
                        let _ = view.update(cx, |page, cx| {
                            page.open_task_editor(Some(id), None, status, None, window, cx);
                        });
                    }
                }),
        )
        .item(
            PopupMenuItem::new(if starred {
                tr(cx, "取消置顶")
            } else {
                tr(cx, "置顶任务")
            })
            .icon(Icon::new(DuckIcon::Star))
            .on_click({
                let view = view.clone();
                let next = !starred;
                move |_, _, cx| {
                    let _ = view.update(cx, |page, cx| page.set_starred(id, next, cx));
                }
            }),
        )
        .separator();

    // ---- 状态：一个三态开关 + 两个单向动作（原型 2694-2700 行）----
    // `done` 与 `cancelled` 都是「已收尾」的态，内核只放行回 `todo`
    // （`TaskStatus::can_transition_to`：`cancelled` 只许去 `todo`），故两者共用一条。
    let (label, icon, next) = match status {
        TaskStatus::Done | TaskStatus::Cancelled => {
            (tr(cx, "重新打开"), DuckIcon::RotateCw, TaskStatus::Todo)
        }
        TaskStatus::Doing => (tr(cx, "暂停"), DuckIcon::Pause, TaskStatus::Todo),
        _ => (tr(cx, "开始"), DuckIcon::Play, TaskStatus::Doing),
    };
    menu = menu.item(status_action(label, icon, id, next, view));

    // 能不能做交给内核的状态机说（`can_transition_to`），不在这里另抄一份规则；
    // 「已经是这一档」由 `!=` 挡掉 —— 内核允许 `done -> done`，留着只会让人点了没反应。
    if status != TaskStatus::Done && status.can_transition_to(TaskStatus::Done) {
        menu = menu.item(status_action(
            tr(cx, "标记完成"),
            DuckIcon::Check,
            id,
            TaskStatus::Done,
            view,
        ));
    }
    if status != TaskStatus::Blocked && status.can_transition_to(TaskStatus::Blocked) {
        menu = menu.item(status_action(
            tr(cx, "标记为阻塞"),
            DuckIcon::CircleAlert,
            id,
            TaskStatus::Blocked,
            view,
        ));
    }

    menu = menu.separator();
    menu = menu.item(
        PopupMenuItem::submenu(tr(cx, "优先级…"), priority_submenu(task, view, window, cx))
            .icon(Icon::new(DuckIcon::Flag)),
    );
    menu = menu.item(
        PopupMenuItem::submenu(
            tr(cx, "状态…"),
            status_submenu(task, columns, view, window, cx),
        )
        .icon(Icon::new(DuckIcon::SlidersHorizontal)),
    );
    menu = menu.item(
        PopupMenuItem::submenu(
            tr(cx, "移入项目…"),
            project_submenu(task, projects, view, window, cx),
        )
        .icon(Icon::new(DuckIcon::FolderOpen)),
    );
    menu = menu.item(
        PopupMenuItem::submenu(tr(cx, "四象限…"), quadrant_submenu(task, view, window, cx))
            .icon(Icon::new(DuckIcon::Columns2)),
    );

    // ---- 收尾：复制（本仓口径：真建一条新任务）+ 归档 + 删除（不可逆，走二次确认）----
    menu.separator()
        .item(
            PopupMenuItem::new(tr(cx, "复制任务"))
                .icon(Icon::new(DuckIcon::Copy))
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |page, cx| page.duplicate_task(id, cx));
                    }
                }),
        )
        .item(
            PopupMenuItem::new(tr(cx, "归档任务"))
                .icon(Icon::new(DuckIcon::Archive))
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |page, cx| page.archive_task(id, cx));
                    }
                }),
        )
        .item(
            PopupMenuItem::new(tr(cx, "删除任务"))
                .icon(Icon::new(DuckIcon::Trash2))
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ =
                            view.update(cx, |page, cx| page.confirm_delete_task(id, window, cx));
                    }
                }),
        )
}

/// 一条「把状态改成 `next`」的菜单项。
fn status_action(
    label: &'static str,
    icon: DuckIcon,
    id: i64,
    next: TaskStatus,
    view: &WeakEntity<ProjectsPage>,
) -> PopupMenuItem {
    PopupMenuItem::new(label).icon(Icon::new(icon)).on_click({
        let view = view.clone();
        move |_, _, cx| {
            let _ = view.update(cx, |page, cx| page.set_task_status(id, next, cx));
        }
    })
}

/// 「优先级…」子菜单（原型 `PRIO` 表：`P0 紧急` … `P3 低`）。
fn priority_submenu(
    task: &Task,
    view: &WeakEntity<ProjectsPage>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> Entity<PopupMenu> {
    let id = task.id;
    let current = task.priority;
    let view = view.clone();
    // 子菜单标题行（原型 `popMenu` 的 `{ title: '设为' }`）：列出来的都是「可选项」，
    // 顶上给一句话说明这一组在选什么，比让人从选项反推要快。
    let heading = tr(cx, "设为");
    PopupMenu::build(window, cx, move |menu, _, cx| {
        Priority::ALL
            .iter()
            .fold(menu.label(heading), |menu, priority| {
                let priority = *priority;
                menu.item(
                    PopupMenuItem::new(SharedString::from(format!(
                        "P{} {}",
                        priority.as_i64(),
                        priority_label(priority, cx)
                    )))
                    .icon(Icon::new(DuckIcon::Flag))
                    .checked(priority == current)
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |page, cx| page.set_priority(id, priority, cx));
                        }
                    }),
                )
            })
    })
}

/// 「状态…」子菜单：目标是**看板列**（原型 `DATA.cols`），列名用户可改。
fn status_submenu(
    task: &Task,
    columns: &[BoardColumn],
    view: &WeakEntity<ProjectsPage>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> Entity<PopupMenu> {
    let id = task.id;
    let status = task.status;
    let view = view.clone();
    let entries: Vec<(SharedString, TaskStatus)> = columns
        .iter()
        .map(|column| (column_title(column, cx), column.status_key))
        .collect();
    let heading = tr(cx, "移动到");
    PopupMenu::build(window, cx, move |menu, _, _cx| {
        entries
            .iter()
            .fold(menu.label(heading), |menu, (label, target)| {
                let target = *target;
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .icon(Icon::new(DuckIcon::SlidersHorizontal))
                        // 从 `cancelled` 出发的非法迁移直接置灰：内核会拒，置灰比点了
                        // 再弹一条错误更早把话说清楚。
                        .disabled(!status.can_transition_to(target))
                        .checked(target == status)
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view
                                    .update(cx, |page, cx| page.set_task_status(id, target, cx));
                            }
                        }),
                )
            })
    })
}

/// 「移入项目…」子菜单：第一项是「未归入项目」（`None`）。
fn project_submenu(
    task: &Task,
    projects: &[Project],
    view: &WeakEntity<ProjectsPage>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> Entity<PopupMenu> {
    let id = task.id;
    let current = task.project_id;
    let view = view.clone();
    let entries: Vec<(SharedString, Option<i64>)> =
        std::iter::once((SharedString::from(tr(cx, "未归入项目")), None))
            .chain(
                projects
                    .iter()
                    .map(|project| (SharedString::from(project.name.clone()), Some(project.id))),
            )
            .collect();
    let heading = tr(cx, "所属项目");
    PopupMenu::build(window, cx, move |menu, _, _cx| {
        entries
            .iter()
            .fold(menu.label(heading), |menu, (label, target)| {
                let target = *target;
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .icon(Icon::new(if target.is_none() {
                            DuckIcon::Inbox
                        } else {
                            DuckIcon::FolderOpen
                        }))
                        .checked(target == current)
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view
                                    .update(cx, |page, cx| page.set_task_project(id, target, cx));
                            }
                        }),
                )
            })
    })
}

/// 「四象限…」子菜单。
fn quadrant_submenu(
    task: &Task,
    view: &WeakEntity<ProjectsPage>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> Entity<PopupMenu> {
    let id = task.id;
    let current = task.quadrant;
    let view = view.clone();
    let entries: Vec<(SharedString, Quadrant)> = Quadrant::ALL
        .iter()
        .map(|quadrant| (SharedString::from(quadrant_label(*quadrant, cx)), *quadrant))
        .collect();
    let heading = tr(cx, "四象限定位");
    PopupMenu::build(window, cx, move |menu, _, _cx| {
        entries
            .iter()
            .fold(menu.label(heading), |menu, (label, quadrant)| {
                let quadrant = *quadrant;
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .icon(Icon::new(DuckIcon::Columns2))
                        .checked(quadrant == current)
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ =
                                    view.update(cx, |page, cx| page.set_quadrant(id, quadrant, cx));
                            }
                        }),
                )
            })
    })
}
