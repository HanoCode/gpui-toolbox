//! 项目管理页的视图：全部 `render_*` 都是**纯函数**（`&ProjectsState` + `&App` 进，
//! `AnyElement` 出），不持有状态字段 —— 状态在 `state.rs`，装配在 `mod.rs`。

use gpui::{
    Anchor, AnyElement, App, ClickEvent, Entity, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, Toggled, WeakEntity, div, prelude::FluentBuilder as _,
    px, relative,
};
use gpui_component::{
    Icon, h_flex,
    input::{Input, InputState},
    menu::{ContextMenuExt as _, PopupMenuItem},
    v_flex,
};

use duck_components::{
    ButtonVariant, DuckIcon, SegmentItem, button, caption_heading,
    content::{alert_block, empty_state, neutral_tag},
    icon_button, icon_text_button, meta_text, nav_icon_color,
    notify::Tone,
    segmented_tabs_with_icons, sidebar_navigation_button, tabular_numbers,
    toolbar_text_dropdown_button,
};
use duck_project::{
    CalendarDate, LogKind, Priority, Project, ProjectStatus, Tag, Task, TaskSort, TaskStatus,
    Weekday,
};
use duck_theme::{active_theme, color_from_hex, tr, trf};

use super::ProjectsPage;
use super::archive::render_archive_center;
use super::prefs::{GroupMode, ViewMode};
use super::state::{BoardScope, ProjectsState, SidebarCounts, SmartFilter, local_date};
use super::widgets::kanban_column::kanban_column;
use super::widgets::priority_dot::{priority_color, priority_dot, priority_label};
use super::widgets::progress_bar::task_progress;
use super::widgets::status_label;
use super::widgets::tag_chip::tag_chip;
use super::widgets::task_detail_panel::task_detail_panel;
use super::widgets::task_menu::task_menu;
use super::widgets::{due_label, is_double_click};

/// 左栏里项目 / 「未归入项目」相对智能列表右缩进的距离。
const SIDEBAR_INDENT: f32 = 18.;

/// 列表视图各列的宽度（表头与行共用，两处必须是同一份，否则列会错位）。
const LIST_PROJECT_WIDTH: f32 = 120.;
const LIST_PRIORITY_WIDTH: f32 = 88.;
const LIST_STATUS_WIDTH: f32 = 88.;
const LIST_DUE_WIDTH: f32 = 120.;
const LIST_PROGRESS_WIDTH: f32 = 120.;
const LIST_TAGS_WIDTH: f32 = 150.;
/// 勾选列：只放一个 15px 的方框，宽度给到 18 让它在表头与行里都居中。
///
/// 裸值（不是间距阶梯里的一档）：勾选框的**直径**是原型 `.lcheck` 定死的 15px，
/// 这一格只是它的外框，跟着阶梯走反而会让框与行高对不上。
const LIST_CHECK_WIDTH: f32 = 18.;
/// 勾选框直径（原型 `.lcheck{width:15px;height:15px}`）。
const CHECK_BOX_SIZE: f32 = 15.;

/// 左栏：智能列表 + 工作（项目树）+ 标签 + 底部计数。
///
/// 分段与 `docs/projects-mock.html` 的左栏一一对应。**不复用** `duck_components`
/// 的 `nav_list`：那里的滑动高亮块按「行下标 × 固定行高」定位，而这里每两段之间还
/// 夹着段标题与缩进行，下标算不准会滑到错行。
///
/// 范围与智能列表**互斥**（同原型）：两者回答的都是「我现在在看哪一堆」，同时亮两处
/// 高亮只会让人说不清在看什么。
pub(crate) fn render_sidebar(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    filter: &Entity<InputState>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    // 左栏这个框同时管两件事（原型同款）：筛主区任务（走内核 `search`），
    // 以及筛树上的项目 / 标签名。占位文案写的就是「筛选任务与项目」。
    let query = filter.read(cx).value().trim().to_lowercase();
    let counts = state.sidebar_counts();

    let mut nav = v_flex().w_full().gap(tokens.spacing.xxs);
    let mut matched_any_project = false;

    // ---- 智能列表 ----
    nav = nav.child(section_heading(tr(cx, "智能列表"), None, cx));
    for filter in SmartFilter::ALL {
        let selected = state.smart() == Some(filter);
        // 元素 id 用**稳定的 ASCII 键**，不用译文 —— 否则切换语言时 id 变化，
        // gpui 会当成另一个元素，选中态与焦点都会丢。
        nav = nav.child(
            sidebar_navigation_button(
                SharedString::from(smart_id(filter)),
                SharedString::from(smart_label(filter, cx)),
                Icon::new(smart_icon(filter))
                    .size(extended.icon.md)
                    .text_color(nav_icon_color(selected, cx)),
                count_badge(counts.of(filter), cx),
                selected,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.toggle_smart(filter, cx));
                }
            }),
        );
    }

    // ---- 工作 ----
    nav = nav.child(section_heading(
        tr(cx, "工作"),
        Some(
            icon_button(
                "projects-new-project",
                tr(cx, "新建项目"),
                Icon::new(DuckIcon::Plus)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.create_project(cx));
                }
            })
            .into_any_element(),
        ),
        cx,
    ));
    nav = nav.child(collapse_row(state, view, cx));

    if !state.projects_collapsed() {
        // 分组 → 项目。`group_id` 为 `None` 的项目归到单独的「未分组」段，
        // 不为了显示而造一条假分组记录。
        let mut sections: Vec<(Option<String>, Vec<&Project>)> = Vec::new();
        for group in state.groups() {
            let items = state
                .projects()
                .iter()
                .filter(|project| project.group_id == Some(group.id))
                .filter(|project| matches_filter(&project.name, &query))
                .collect::<Vec<_>>();
            if !items.is_empty() {
                sections.push((Some(group.name.clone()), items));
            }
        }
        let loose = state
            .projects()
            .iter()
            .filter(|project| project.group_id.is_none())
            .filter(|project| matches_filter(&project.name, &query))
            .collect::<Vec<_>>();
        if !loose.is_empty() {
            sections.push((None, loose));
        }

        for (heading, items) in sections {
            matched_any_project = true;
            // 只有一个「未分组」段时不加段标题：没有兄弟段可区分，标题只是噪音。
            if let Some(heading) = heading {
                nav = nav.child(section_heading(SharedString::from(heading), None, cx));
            }
            for project in items {
                nav = nav.child(project_row(project, state, view, cx));
            }
        }

        // 「未归入项目」排在项目之后（原型同款）：它不是又一个筛选项，而是「没有归属」
        // 这一堆本身，所以走 [`BoardScope::Inbox`] —— 与「全部任务」并列的另一个范围。
        let selected = state.scope() == BoardScope::Inbox;
        nav = nav.child(indent(
            sidebar_navigation_button(
                "projects-scope-inbox",
                tr(cx, "未归入项目"),
                Icon::new(DuckIcon::Inbox)
                    .size(extended.icon.md)
                    .text_color(nav_icon_color(selected, cx)),
                count_badge(scope_open_count(state, BoardScope::Inbox), cx),
                selected,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.set_scope(BoardScope::Inbox, cx));
                }
            }),
        ));
    }

    if !query.is_empty() && !matched_any_project {
        nav = nav.child(
            div()
                .w_full()
                .py(tokens.spacing.lg)
                .flex()
                .justify_center()
                .child(meta_text(tr(cx, "没有匹配的项目。"), cx)),
        );
    }

    // ---- 标签 ----
    nav = nav.child(section_heading(tr(cx, "标签"), None, cx));
    if state.tags().is_empty() {
        nav = nav.child(
            div()
                .w_full()
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xxs)
                .child(meta_text(tr(cx, "还没有标签。"), cx)),
        );
    }
    for tag in state.tags() {
        if !matches_filter(&tag.name, &query) {
            continue;
        }
        nav = nav.child(tag_row(tag, state, view, cx));
    }
    // 「管理标签…」压在标签段末尾（原型 `data-side="tags"`）。它是个**动作**、不是
    // 又一个筛选档，所以不参与 `state.tag()` 的选中态、也不带计数徽标 —— 摆一个
    // 会亮起来却没有对应范围的导航项，比不摆更让人困惑。
    nav = nav.child(
        sidebar_navigation_button(
            "projects-manage-tags",
            tr(cx, "管理标签…"),
            Icon::new(DuckIcon::Tag)
                .size(extended.icon.md)
                .text_color(tokens.colors.muted_foreground),
            h_flex().flex_none().items_center(),
            false,
            cx,
        )
        .on_click({
            let view = view.clone();
            move |_, window, cx| {
                let _ = view.update(cx, |page, cx| page.open_tag_manager(window, cx));
            }
        }),
    );

    v_flex()
        .size_full()
        .min_h_0()
        .bg(tokens.colors.surface)
        .child(
            h_flex()
                .h(theme.density().section_header)
                .flex_none()
                .items_center()
                .justify_between()
                .px(tokens.spacing.sm)
                .child(caption_heading(tr(cx, "项目管理"), cx))
                .child(
                    icon_button(
                        "projects-collapse-all",
                        tr(cx, "折叠全部"),
                        Icon::new(if state.projects_collapsed() {
                            DuckIcon::ChevronRight
                        } else {
                            DuckIcon::ChevronDown
                        })
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, _window, cx| {
                            let _ = view.update(cx, |page, cx| {
                                page.toggle_projects_collapsed(cx);
                            });
                        }
                    }),
                ),
        )
        .child(
            div()
                .flex_none()
                .px(tokens.spacing.sm)
                .pb(tokens.spacing.xs)
                .child(
                    h_flex()
                        .h(theme.density().control)
                        .w_full()
                        .px(tokens.spacing.sm)
                        .rounded(theme.components().input_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.background)
                        .child(
                            Input::new(filter)
                                .appearance(false)
                                .bordered(false)
                                .w_full(),
                        ),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .id("projects-sidebar-scroll")
                .overflow_y_scroll()
                .child(nav),
        )
        // 归档中心挂在左栏**底部**（§5.6）：它不是「范围」，与上面那三段不同类；
        // 混在「工作」里会让人以为它是又一个筛选项。
        .child(
            div()
                .flex_none()
                .pt(tokens.spacing.xs)
                .px(tokens.spacing.xs)
                .border_t(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(archive_entry(state, view, cx)),
        )
        // 底部三格计数（原型同款）：全局口径，与当前范围 / 筛选无关。
        .child(sidebar_foot(counts, cx))
        .into_any_element()
}

/// 段标题：`xs` 中粗 + 三级文字色，右侧可带一个动作按钮。
fn section_heading(
    label: impl Into<SharedString>,
    action: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    h_flex()
        .w_full()
        .flex_none()
        .items_center()
        .gap(tokens.spacing.xxs)
        .px(tokens.spacing.xs)
        .pt(tokens.spacing.sm)
        .pb(tokens.spacing.xxs)
        .child(caption_heading(label.into(), cx))
        .children(action.map(|action| div().flex_1().min_w_0().flex().justify_end().child(action)))
        .into_any_element()
}

/// 右缩进一档的行（项目与「未归入项目」比智能列表往里收）。
///
/// 在**外层**加左内边距而不是改 `navigation_row` 的内部 padding：后者是全仓共用的
/// 导航行，不该为这一个页面的层级再开一个参数（改它要同步 `nav_list` 的滑动块定位）。
fn indent(child: impl IntoElement) -> AnyElement {
    div()
        .w_full()
        .pl(px(SIDEBAR_INDENT))
        .child(child)
        .into_any_element()
}

/// 左栏底部的三条全局计数（原型 `sb-foot`）：共 / 完成 / 逾期。
fn sidebar_foot(counts: SidebarCounts, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let xs = tokens.typography.xs;

    let stat = |label: &'static str, value: u32, tone: Hsla, cx: &App| {
        h_flex()
            .flex_none()
            .gap(tokens.spacing.xxs)
            .child(
                div()
                    .text_size(xs.size)
                    .line_height(xs.line_height)
                    .text_color(extended.colors.text_tertiary)
                    .child(SharedString::from(tr(cx, label))),
            )
            .child(
                div()
                    .font_features(tabular_numbers())
                    .text_size(xs.size)
                    .line_height(xs.line_height)
                    .text_color(tone)
                    .child(SharedString::from(value.to_string())),
            )
    };

    h_flex()
        .w_full()
        .flex_none()
        .items_center()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_t(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(stat("共", counts.all, tokens.colors.muted_foreground, cx))
        .child(stat(
            "完成",
            counts.done,
            tokens.colors.muted_foreground,
            cx,
        ))
        .child(stat("逾期", counts.overdue, tokens.colors.destructive, cx))
        .into_any_element()
}

/// 「全部项目」折叠头。
///
/// 它不是范围（没有「全部项目」这一档查询口径），所以**不给选中态**，只做展开 / 收起
/// —— 给一个永远亮不起来的选中态，比不给更让人困惑。
fn collapse_row(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let collapsed = state.projects_collapsed();
    let hover_tint = extended.colors.hover_tint;

    h_flex()
        .id("projects-collapse-work")
        .w_full()
        .flex_none()
        .h(theme.density().nav_row)
        .px(tokens.spacing.sm)
        .gap(tokens.spacing.sm)
        .rounded(theme.components().nav_item_radius)
        .cursor_pointer()
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .text_color(tokens.colors.muted_foreground)
        .hover(move |style| style.bg(hover_tint))
        // 自绘的可点行对读屏不存在（a11y 树只收录带 `role` 的节点），角色与名字都得显式给。
        .role(Role::Button)
        .aria_label(tr(cx, "全部项目"))
        .child(
            Icon::new(if collapsed {
                DuckIcon::ChevronRight
            } else {
                DuckIcon::ChevronDown
            })
            .size(extended.icon.sm)
            .text_color(extended.colors.text_tertiary),
        )
        .child(div().truncate().child(tr(cx, "全部项目")))
        .on_click({
            let view = view.clone();
            move |_, _window, cx| {
                let _ = view.update(cx, |page, cx| page.toggle_projects_collapsed(cx));
            }
        })
        .into_any_element()
}

/// 左栏一行标签：色点 + 名称 + 用到它的任务数。点一下按它筛，再点一下取消。
fn tag_row(
    tag: &Tag,
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let selected = state.tag() == Some(tag.id);
    // 标签色和项目色一样是用户数据（`#RRGGBB`），解析失败按中性色画。
    let color = color_from_hex(&tag.color).unwrap_or(tokens.colors.muted_foreground);

    sidebar_navigation_button(
        SharedString::from(format!("projects-tag-{}", tag.id)),
        SharedString::from(tag.name.clone()),
        div()
            .size(extended.icon.md)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .child(div().size(extended.icon.md / 2.).rounded_full().bg(color)),
        count_badge(tag.usage_count, cx),
        selected,
        cx,
    )
    .on_click({
        let view = view.clone();
        let id = tag.id;
        move |_, _window, cx| {
            let _ = view.update(cx, |page, cx| page.toggle_tag(id, cx));
        }
    })
    .into_any_element()
}

/// 智能列表档位的稳定元素 id（理由同项目行：用译文当 id 会在切语言时丢选中态）。
const fn smart_id(filter: SmartFilter) -> &'static str {
    match filter {
        SmartFilter::Today => "projects-smart-today",
        SmartFilter::ThisWeek => "projects-smart-week",
        SmartFilter::Overdue => "projects-smart-overdue",
        SmartFilter::Unscheduled => "projects-smart-unscheduled",
    }
}

/// 智能列表档位的中文名。
fn smart_label(filter: SmartFilter, cx: &App) -> &'static str {
    match filter {
        SmartFilter::Today => tr(cx, "今天"),
        SmartFilter::ThisWeek => tr(cx, "本周"),
        SmartFilter::Overdue => tr(cx, "逾期"),
        SmartFilter::Unscheduled => tr(cx, "未排期"),
    }
}

/// 智能列表档位的图标。
///
/// 图标集里没有日历族（`CalendarToday` / `CalendarWeek` 都不存在），所以「今天 /
/// 本周」先用语义最近的 `Clock` / `Layers` 顶上 —— 为了两个图标往 `duck_components`
/// 里塞资源，代价大于收益。
const fn smart_icon(filter: SmartFilter) -> DuckIcon {
    match filter {
        SmartFilter::Today => DuckIcon::Clock,
        SmartFilter::ThisWeek => DuckIcon::Layers,
        SmartFilter::Overdue => DuckIcon::TriangleAlert,
        SmartFilter::Unscheduled => DuckIcon::Inbox,
    }
}

/// 左栏底部的「归档中心」入口。
fn archive_entry(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let extended = theme.extended();
    let selected = state.view() == ViewMode::Archive;

    sidebar_navigation_button(
        "projects-scope-archive",
        tr(cx, "归档中心"),
        Icon::new(DuckIcon::Archive)
            .size(extended.icon.md)
            .text_color(nav_icon_color(selected, cx)),
        count_badge(state.archive_counts().total(), cx),
        selected,
        cx,
    )
    .on_click({
        let view = view.clone();
        move |_, _window, cx| {
            let _ = view.update(cx, |page, cx| page.set_view(ViewMode::Archive, cx));
        }
    })
    .into_any_element()
}

/// 一行项目。
///
/// 右键菜单挂在外层 `div` 上（有 `id` 的 `div` 才是 `Stateful`），而不是挂在
/// `Button` 上 —— 后者没有 `context_menu`。
fn project_row(
    project: &Project,
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let selected = state.scope().is_project(project.id);
    // 项目色只作圆点：它和标签色一样是用户数据（`#RRGGBB`），解析失败按中性色画。
    let color = color_from_hex(&project.color).unwrap_or(tokens.colors.muted_foreground);
    let counts = state.project_counts(project.id);
    let paused = project.status == ProjectStatus::Paused;

    let trailing = h_flex()
        .flex_none()
        .gap(tokens.spacing.xxs)
        .children(paused.then(|| neutral_tag(tr(cx, "已搁置"), cx)))
        .child(
            div()
                .flex_none()
                .font_features(tabular_numbers())
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(extended.colors.text_tertiary)
                .child(SharedString::from(counts.open().to_string())),
        );

    div()
        .id(SharedString::from(format!(
            "projects-project-row-{}",
            project.id
        )))
        .w_full()
        .context_menu({
            let view = view.clone();
            let id = project.id;
            let name = SharedString::from(project.name.clone());
            move |menu, _, cx| {
                menu
                    // 标题行（原型 `{ title: '项目「X」' }`）：右键弹出来的菜单不自带
                    // 方向 —— 不写明点的是哪一个，项目一多就很容易改错。
                    .item(PopupMenuItem::label(trf(cx, "项目「{}」", &[&name])))
                    .item(
                        PopupMenuItem::new(tr(cx, "只看这个项目"))
                            .icon(Icon::new(DuckIcon::ListFilter))
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    let _ = view.update(cx, |page, cx| {
                                        page.set_scope(BoardScope::Project(id), cx);
                                    });
                                }
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr(cx, "新建任务并归入"))
                            .icon(Icon::new(DuckIcon::Plus))
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |page, cx| {
                                        page.open_spotlight_in(Some(id), window, cx);
                                    });
                                }
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new(tr(cx, "项目设置…"))
                            .icon(Icon::new(DuckIcon::Settings))
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |page, cx| {
                                        page.open_project_editor(Some(id), window, cx);
                                    });
                                }
                            }),
                    )
                    // 与「归档项目」分开：归档可恢复，删除不可逆，中间那根分隔线就是
                    // 这两档之间唯一的视觉边界（「归档项目」是本仓特有的一档，原型
                    // 的软删除在这里换成了归档中心，见 `docs/projects-design.md`）。
                    .separator()
                    .item(
                        PopupMenuItem::new(tr(cx, "归档项目"))
                            .icon(Icon::new(DuckIcon::Archive))
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    let _ =
                                        view.update(cx, |page, cx| page.archive_project(id, cx));
                                }
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr(cx, "删除项目"))
                            .icon(Icon::new(DuckIcon::Trash2))
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |page, cx| {
                                        page.confirm_delete_project(id, window, cx);
                                    });
                                }
                            }),
                    )
            }
        })
        .child(
            sidebar_navigation_button(
                SharedString::from(format!("projects-project-{}", project.id)),
                SharedString::from(project.name.clone()),
                div()
                    .size(extended.icon.md)
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().size(extended.icon.md / 2.).rounded_full().bg(color)),
                trailing,
                selected,
                cx,
            )
            .on_click({
                let view = view.clone();
                let id = project.id;
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.set_scope(BoardScope::Project(id), cx));
                }
            }),
        )
        .into_any_element()
}

/// 项目名；查不到（刚被归档）回落一个中性词。
fn project_name(state: &ProjectsState, id: i64, cx: &App) -> SharedString {
    state
        .projects()
        .iter()
        .find(|project| project.id == id)
        .map_or_else(
            || SharedString::from(tr(cx, "项目")),
            |project| SharedString::from(project.name.clone()),
        )
}

/// 某个范围内的未完成任务数。
fn scope_open_count(state: &ProjectsState, scope: BoardScope) -> u32 {
    match scope {
        BoardScope::All => state.stats().total.saturating_sub(state.stats().done),
        BoardScope::Inbox => state
            .tasks()
            .iter()
            .filter(|task| task.project_id.is_none() && !task.status.is_settled())
            .count() as u32,
        BoardScope::Project(id) => state.project_counts(id).open(),
    }
}

/// 等宽数字的计数徽标。
pub(super) fn count_badge(count: u32, cx: &App) -> impl IntoElement {
    let theme = active_theme(cx);
    div()
        .flex_none()
        .font_features(tabular_numbers())
        .text_size(theme.tokens().typography.xs.size)
        .line_height(theme.tokens().typography.xs.line_height)
        .text_color(theme.extended().colors.text_tertiary)
        .child(SharedString::from(count.to_string()))
}

fn matches_filter(name: &str, query: &str) -> bool {
    query.is_empty() || name.to_lowercase().contains(query)
}

/// 主区：工具条 + 看板 / 列表 + 状态栏。
///
/// 归档中心**整块换掉**（它自带工具条：搜索 + 清空），不叠在看板工具条下面 ——
/// 「看板 / 列表」这两个 tab 对归档区没有意义。
///
/// 搜索框不在这里：它挪到了左栏（原型同款，一个框同时筛任务与项目名），所以主区
/// 工具条只剩「视图 + 范围 + 排序 + 分组 + 显示已完成 + 导入导出 + 两个新建入口」。
pub(crate) fn render_main(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    archive_search: &Entity<InputState>,
    cx: &App,
) -> AnyElement {
    if state.view().is_archive() {
        return render_archive_center(state, view, archive_search, cx);
    }

    let body = match state.view() {
        ViewMode::List => render_list(state, view, cx),
        ViewMode::Calendar => render_calendar(state, view, cx),
        ViewMode::Timeline => render_timeline(state, view, cx),
        ViewMode::Report => render_report(state, view, cx),
        ViewMode::Log => render_log(state, view, cx),
        // `Board` 与 `Archive` 都落到这儿，但 `Archive` 在上面已经 return 了。
        _ => render_board(state, view, cx),
    };

    v_flex()
        .size_full()
        .min_h_0()
        .min_w_0()
        .child(render_toolbar(state, view, cx))
        // 错误横幅：内核打不开时把**原始**英文错误显示出来，不翻译也不吞掉。
        .children(state.error().map(|message| {
            alert_block(
                "projects-error",
                Tone::Warning,
                SharedString::from(message.to_owned()),
                cx,
            )
        }))
        .child(body)
        .child(render_status_bar(state, cx))
        .into_any_element()
}

/// 工具条：视图切换 + 范围 chips +（排序 / 分组 / 显示已完成）+ 导入导出 + 两个新建按钮。
///
/// 与下面那条「筛选条」分成两行是**刻意的**：一行塞不下（5 个 chip + 6 档视图 +
/// 6 档排序 + 4 档分组 + 4 个动作），而按宽度硬挤会让窄窗口下的控件互相截断。
/// 两行各司其职 —— 这一行管「怎么看」，下一行管「看哪些」。
///
/// 排法与原型 `renderToolbar`（`docs/projects-mock.html:1131`）逐段对应：
/// 左侧 seg + 范围 chips，右侧一条 `.right` 段放排序 / 分组 / 显示已完成 / 分隔 /
/// 导入 / 导出 / 快速新建 / 新建任务。
fn render_toolbar(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    h_flex()
        .flex_none()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(tokens.spacing.xs)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(view_switcher(state, view, cx))
        .child(toolbar_separator(cx))
        // 范围 chips：全部任务 + 每个项目（带色点与未完成数）。与左栏的项目行是同一档
        // 范围，只是换了个入口 —— 原型两处都有，看板页面里伸手就能换项目。
        .child(scope_chips(state, view, cx))
        .child(div().flex_1().min_w_0())
        .child(sort_switcher(state, view, cx))
        .child(group_switcher(state, view, cx))
        .child(show_done_toggle(state, view, cx))
        .child(toolbar_separator(cx))
        .child(toolbar_import(view, cx))
        .child(toolbar_export(view, cx))
        .child(quick_create_button(view, cx))
        .child(new_task_button(view, cx))
        .into_any_element()
}

/// 工具条上的「快速新建」（原型 `data-open="quick"`）：开 Spotlight 语义输入。
fn quick_create_button(view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let icon = Icon::new(DuckIcon::Sparkles).size(active_theme(cx).extended().icon.sm);
    let view = view.clone();
    icon_text_button(
        "projects-quick-create",
        tr(cx, "快速新建"),
        icon,
        ButtonVariant::Secondary,
        cx,
    )
    .on_click(move |_, window, cx| {
        let _ = view.update(cx, |page, cx| page.open_spotlight(window, cx));
    })
    .into_any_element()
}

/// 工具条上的「新建任务」（原型 `data-open="task"`）：开任务编辑大表单。
fn new_task_button(view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let icon = Icon::new(DuckIcon::Plus).size(active_theme(cx).extended().icon.sm);
    let view = view.clone();
    icon_text_button(
        "projects-new-task",
        tr(cx, "新建任务"),
        icon,
        ButtonVariant::Primary,
        cx,
    )
    .on_click(move |_, window, cx| {
        let _ = view.update(cx, |page, cx| page.new_task_in_scope(window, cx));
    })
    .into_any_element()
}

/// 工具条上的「从文本导入」（原型 `data-side="import"`）。
///
/// 只给图标：工具条右段已经有六个控件，再摆两个文字按钮会把范围 chips 挤成一团。
/// 悬停提示由 [`icon_button`] 自己挂（**调用方不要重复挂**，二次 `.tooltip()` 会 panic）。
fn toolbar_import(view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let icon = Icon::new(DuckIcon::Upload).size(active_theme(cx).extended().icon.sm);
    let view = view.clone();
    icon_button(
        "projects-import",
        tr(cx, "从文本导入"),
        icon,
        ButtonVariant::Ghost,
        cx,
    )
    .on_click(move |_, window, cx| {
        let _ = view.update(cx, |page, cx| page.open_import(window, cx));
    })
    .into_any_element()
}

/// 工具条上的「导出」（原型 `data-side="export"`）。
fn toolbar_export(view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let icon = Icon::new(DuckIcon::Download).size(active_theme(cx).extended().icon.sm);
    let view = view.clone();
    icon_button(
        "projects-export",
        tr(cx, "导出"),
        icon,
        ButtonVariant::Ghost,
        cx,
    )
    .on_click(move |_, window, cx| {
        let _ = view.update(cx, |page, cx| page.open_export(window, cx));
    })
    .into_any_element()
}

/// 工具条上的竖线分隔（原型 `.tbsep`）。
fn toolbar_separator(cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .flex_none()
        .w(px(1.))
        .h(tokens.spacing.md)
        .bg(theme.extended().colors.hairline)
        .into_any_element()
}

/// 范围 chips：`全部任务` + 每个项目。
///
/// 与左栏的项目行是**同一档**范围（点当前项 = 收回全部，原型同款），所以两处的高亮
/// 永远同步 —— 状态只有 `BoardScope` 一份。
fn scope_chips(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let all_selected = state.scope() == BoardScope::All;
    let gap = active_theme(cx).tokens().spacing.xs;
    let mut row = h_flex()
        .flex_none()
        .flex_wrap()
        .items_center()
        .gap(gap)
        .child(
            chip(
                "projects-scope-all",
                None,
                SharedString::from(trf(cx, "全部任务 {}", &[&state.stats().total.to_string()])),
                all_selected,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.set_scope(BoardScope::All, cx));
                }
            }),
        );

    for project in state.projects() {
        let selected = state.scope().is_project(project.id);
        let tokens = active_theme(cx).tokens();
        let color = color_from_hex(&project.color).unwrap_or(tokens.colors.muted_foreground);
        let id = project.id;
        let label = SharedString::from(format!(
            "{} {}",
            project.name,
            state.project_counts(project.id).open()
        ));
        row = row.child(
            chip(
                SharedString::from(format!("projects-scope-chip-{id}")),
                Some(
                    div()
                        .flex_none()
                        .size(tokens.spacing.xs)
                        .rounded_full()
                        .bg(color)
                        .into_any_element(),
                ),
                label,
                selected,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.set_scope(BoardScope::Project(id), cx));
                }
            }),
        );
    }

    row.into_any_element()
}

/// 「显示已完成」开关（原型 `data-toggle="done"`）。
///
/// 它是**独立的一条轴**，不是智能列表的第五档：四档日期口径的 `WHERE` 都排除了
/// `done`，把「已完成」塞进去只会得到一个必然为空的组合。
fn show_done_toggle(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let on = state.show_done();
    let theme = active_theme(cx);
    let icon = Icon::new(DuckIcon::CheckboxCheck)
        .size(theme.extended().icon.sm)
        .text_color(if on {
            theme.tokens().colors.primary
        } else {
            theme.tokens().colors.muted_foreground
        });
    chip(
        "projects-show-done",
        Some(icon.into_any_element()),
        SharedString::from(if on {
            tr(cx, "显示已完成")
        } else {
            tr(cx, "已隐藏完成")
        }),
        on,
        cx,
    )
    .on_click({
        let view = view.clone();
        move |_, _window, cx| {
            let _ = view.update(cx, |page, cx| page.toggle_show_done(cx));
        }
    })
    .into_any_element()
}

/// 视图切换：`segmented_tabs_with_icons`（看板 / 列表 / 日历 / 时间线 / 报表 / 工作日志）。
///
/// 六档一起摆出来（原型 `VIEWS` 同款）而不是只摆做得了的两个：六个视图在本轮
/// 全部落地了，不再有「点了没反应」的占位档（P3-6 判据）。
fn view_switcher(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let current = state.view();
    let selected = ViewMode::TABS
        .iter()
        .position(|mode| *mode == current)
        .unwrap_or(0);
    let items: Vec<SegmentItem> = ViewMode::TABS
        .iter()
        .map(|mode| {
            SegmentItem::new(SharedString::from(view_tab_label(*mode, cx)))
                .icon(view_tab_icon(*mode))
        })
        .collect();
    segmented_tabs_with_icons(
        "projects-view-tabs",
        items,
        selected,
        {
            let view = view.clone();
            move |index, _window, cx| {
                let Some(mode) = ViewMode::TABS.get(index).copied() else {
                    return;
                };
                let _ = view.update(cx, |page, cx| page.set_view(mode, cx));
            }
        },
        cx,
    )
    .into_any_element()
}

/// 视图 tab 的中文名（与原型 `VIEWS` 逐条对应）。
fn view_tab_label(mode: ViewMode, cx: &App) -> &'static str {
    match mode {
        ViewMode::Board => tr(cx, "看板"),
        ViewMode::List => tr(cx, "列表"),
        ViewMode::Calendar => tr(cx, "日历"),
        ViewMode::Timeline => tr(cx, "时间线"),
        ViewMode::Report => tr(cx, "报表"),
        ViewMode::Log => tr(cx, "工作日志"),
        // 归档中心有它自己的左栏入口，不在 segmented tabs 里（`ViewMode::TABS` 也不含它）。
        ViewMode::Archive => tr(cx, "归档"),
    }
}

/// 视图 tab 的图标（原型 `VIEWS[].icon`）。
///
/// 原型用的是 Lucide 的 `kanban` / `list` / `calendar` / `gantt` / `chart` / `file-text`；
/// 本仓图标集里没有 `calendar` 与 `gantt`，各取语义最近的一个：日历用 `Clock`
/// （日期与时刻同族），时间线用 `Columns2`（分段条）；换名字会同时改到 tab 与
/// [`render_empty_*`] 的空态图标，所以集中在这一处。
const fn view_tab_icon(mode: ViewMode) -> DuckIcon {
    match mode {
        ViewMode::Board => DuckIcon::Kanban,
        ViewMode::List => DuckIcon::Table,
        ViewMode::Calendar => DuckIcon::Clock,
        ViewMode::Timeline => DuckIcon::Columns2,
        ViewMode::Report => DuckIcon::ChartLine,
        ViewMode::Log => DuckIcon::FileText,
        ViewMode::Archive => DuckIcon::Archive,
    }
}

/// 排序切换器（`TaskSort::ALL` 档位，当前档带勾）。
fn sort_switcher(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let extended = theme.extended();
    let current = state.sort();
    // 先取一份**拥有所有权**的克隆：下面 `build` 闭包是 `move` 且要求 `'static`，
    // 直接捕获 `&WeakEntity` 这个引用会让借用逃出函数体（E0521）。
    let view = view.clone();
    let items: Vec<(TaskSort, SharedString)> = TaskSort::ALL
        .iter()
        .map(|sort| (*sort, SharedString::from(sort_label(*sort, cx))))
        .collect();
    toolbar_text_dropdown_button(
        "projects-sort",
        SharedString::from(sort_label(current, cx)),
        Icon::new(DuckIcon::ListOrdered).size(extended.icon.sm),
        Anchor::TopRight,
        move |menu, _, _| {
            items.iter().fold(menu, |menu, (sort, label)| {
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .checked(*sort == current)
                        .on_click({
                            let view = view.clone();
                            let sort = *sort;
                            move |_, _, cx| {
                                let _ = view.update(cx, |page, cx| page.set_sort(sort, cx));
                            }
                        }),
                )
            })
        },
        cx,
    )
}

/// 排序档位的中文名。
///
/// 与内核的 [`TaskSort::ALL`] 一一对应（`TaskSort::ALL` 是唯一真源，这里只做
/// 「枚举 → 界面文案」的映射，没有第二份清单可以走偏）。
///
/// 原型菜单是四档（按截止日 / 按优先级 / 按进度 / 按标题）；本表比它多出「手动顺序 /
/// 截止日最远 / 最近创建」——这三档是内核本就有的真实能力，删掉只会让功能倒退，
/// 故保留为超集。
fn sort_label(sort: TaskSort, cx: &App) -> &'static str {
    match sort {
        TaskSort::Manual => tr(cx, "手动顺序"),
        TaskSort::DueAsc => tr(cx, "按截止日"),
        TaskSort::DueDesc => tr(cx, "截止日最远"),
        TaskSort::PriorityFirst => tr(cx, "按优先级"),
        TaskSort::ProgressDesc => tr(cx, "按进度"),
        TaskSort::CreatedDesc => tr(cx, "最近创建"),
        TaskSort::TitleAsc => tr(cx, "按标题"),
    }
}

/// 分组轴下拉（原型 `data-menu="group"`）。
///
/// 只在列表视图里真的改变画面（其余视图各有自己的骨架），但**照原型一样常驻**：
/// 按钮忽隐忽现比一个暂时没效果的按钮更让人迷惑，而且切回列表时还能看到上次选的是哪档。
fn group_switcher(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let extended = theme.extended();
    let current = state.group();
    // 理由同 `sort_switcher`：`build` 闭包要求 `'static`，先把引用克隆成拥有所有权的实体。
    let view = view.clone();
    let items: Vec<(GroupMode, SharedString)> = GroupMode::ALL
        .iter()
        .map(|mode| (*mode, SharedString::from(group_label(*mode, cx))))
        .collect();
    toolbar_text_dropdown_button(
        "projects-group",
        SharedString::from(group_label(current, cx)),
        Icon::new(DuckIcon::Layers).size(extended.icon.sm),
        Anchor::TopRight,
        move |menu, _, _| {
            items.iter().fold(menu, |menu, (mode, label)| {
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .checked(*mode == current)
                        .on_click({
                            let view = view.clone();
                            let mode = *mode;
                            move |_, _, cx| {
                                let _ = view.update(cx, |page, cx| page.set_group(mode, cx));
                            }
                        }),
                )
            })
        },
        cx,
    )
}

/// 分组档位的中文名。与 [`GroupMode::ALL`] 一一对应（真源只有那一个数组）。
fn group_label(mode: GroupMode, cx: &App) -> &'static str {
    match mode {
        GroupMode::Priority => tr(cx, "按优先级分组"),
        GroupMode::Project => tr(cx, "按项目分组"),
        GroupMode::Status => tr(cx, "按状态分组"),
        GroupMode::None => tr(cx, "不分组"),
    }
}

/// 工具条 chip（原型 `.chip`）：选中 = 强调色薄底 + 强调色描边与文字。
///
/// `leading` 是文字**之前**的小挂件（色点 / 图标）；用参数而不是让调用方 `.child()`
/// 追加，是因为追加只会在文字**之后**，而「色点在项目名左边」是原型定死的排法。
///
/// 元素 id 用稳定 ASCII 键（理由同项目行：用译文当 id 会在切语言时被当成另一个元素，
/// 选中态与焦点都会丢）。调用方负责挂 `.on_click`。
fn chip(
    id: impl Into<SharedString>,
    leading: Option<AnyElement>,
    label: SharedString,
    selected: bool,
    cx: &App,
) -> Stateful<gpui::Div> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let accent = tokens.colors.primary;
    let hover_tint = extended.colors.hover_tint;

    div()
        .id(id.into())
        .flex_none()
        .flex()
        .items_center()
        .gap(tokens.spacing.xxs)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xxs)
        .rounded_full()
        .border(extended.stroke.thin)
        .border_color(if selected {
            accent
        } else {
            extended.colors.hairline
        })
        .bg(if selected {
            accent.opacity(extended.opacity.tint)
        } else {
            tokens.colors.surface
        })
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(if selected {
            accent
        } else {
            tokens.colors.muted_foreground
        })
        .cursor_pointer()
        .hover(move |style| style.bg(hover_tint))
        // 自绘的可点 div 对读屏不存在（a11y 树只收录带 `role` 的节点），
        // 所以角色与名字都得显式给。
        .role(Role::Button)
        .aria_label(label.clone())
        .children(leading)
        .child(label)
}

/// 主区空态：按「有没有在筛」分三种说法。
///
/// 一律说「这里还没有任务」会让人以为数据丢了 —— 明明只是筛选条件太窄（或搜索词
/// 打错一个字）。说清是哪一种，用户才知道该清哪个框。看板与列表共用。
fn main_empty_state(state: &ProjectsState, cx: &App) -> AnyElement {
    let term = state.search().trim();
    if !term.is_empty() {
        return empty_state(
            DuckIcon::Search,
            trf(cx, "没有匹配「{}」的任务", &[term]),
            Some(SharedString::from(tr(
                cx,
                "换个关键词，或清掉右上角的搜索框再看看。",
            ))),
            None,
            cx,
        )
        .into_any_element();
    }
    if let Some(filter) = state.smart() {
        return empty_state(
            DuckIcon::ListFilter,
            trf(cx, "没有「{}」的任务", &[smart_label(filter, cx)]),
            Some(SharedString::from(tr(
                cx,
                "再点一下左栏那一项即可取消筛选。",
            ))),
            None,
            cx,
        )
        .into_any_element();
    }
    if let Some(id) = state.tag() {
        let name = state.tags().iter().find(|tag| tag.id == id).map_or_else(
            || SharedString::from(tr(cx, "标签")),
            |tag| SharedString::from(tag.name.clone()),
        );
        return empty_state(
            DuckIcon::Tag,
            trf(cx, "没有「{}」的任务", &[&name]),
            Some(SharedString::from(tr(
                cx,
                "再点一下左栏那个标签即可取消筛选。",
            ))),
            None,
            cx,
        )
        .into_any_element();
    }
    empty_state(
        DuckIcon::Kanban,
        tr(cx, "这里还没有任务"),
        Some(SharedString::from(tr(
            cx,
            "点工具条的「新建任务」，或按 N 用一句话快速新建一条。",
        ))),
        None,
        cx,
    )
    .into_any_element()
}

/// 列表视图：表头 + 若干行（按当前分组轴摆成几摞）。
///
/// 与看板读的是**同一份** `state.tasks()`（口径在 `ProjectsState::query`），所以两个
/// 视图下「范围 + 筛选 + 搜索 + 排序」的结果逐条一致 —— 切视图不该改变看到的东西。
/// 分组只改变**摆法**（同样的那些行换个分堆方式），一条都不增不减。
fn render_list(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    if state.tasks().is_empty() {
        return main_empty_state(state, cx);
    }

    let today = state.today();
    let selected = state.selected();
    let mut body = v_flex().w_full();
    // 批量条压在表格上方（原型同款）：它讲的是「这一屏选了几条」，离被选中的行越近越好。
    if state.checked_count() > 0 {
        body = body.child(bulk_bar(state, view, cx));
    }
    body = body.child(list_header(state, view, cx));
    for group in grouped_tasks(state) {
        if let Some(head) = group_header(&group, state, cx) {
            body = body.child(head);
        }
        // 状态分组下空组也保留（它顺带把看板列当成了分组表头），得给个占位行，
        // 否则表头下面直接接下一个表头，看着像漏渲染了。
        if group.tasks.is_empty() {
            body = body.child(group_empty_row(group.key, cx));
            continue;
        }
        for &task in &group.tasks {
            body = body.child(list_row(task, state, today, selected, view, cx));
        }
    }

    div()
        .id("projects-list")
        .flex_1()
        .min_h_0()
        .min_w_0()
        .w_full()
        .overflow_y_scroll()
        .p(tokens.spacing.sm)
        .child(
            v_flex()
                .w_full()
                .rounded(theme.components().card_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .bg(tokens.colors.surface)
                .overflow_hidden()
                .child(body),
        )
        .into_any_element()
}

/// 列表分组的一档（原型 `vList` 里 `groups` 的构造，一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKey {
    /// 不分组：一整张表，只有表头。
    All,
    /// 按优先级。
    Priority(Priority),
    /// 按项目；`None` = 未归入项目。
    Project(Option<i64>),
    /// 按状态（看板列序）。
    Status(TaskStatus),
}

/// 列表里的一摞。
struct TaskGroup<'a> {
    key: GroupKey,
    tasks: Vec<&'a Task>,
}

/// 按当前分组轴把任务摆成几摞。
///
/// 组**内**顺序就是 `state.tasks()` 自己的顺序（已经是排序口径出来的）；组**间**顺序
/// 由分组轴决定：
///
/// - 优先级：P0 → P3（[`Priority::ALL`] 的顺序）；
/// - 项目：左栏项目表的顺序，「未归入项目」放**最后**（原型同款）；
/// - 状态：看板列序 —— **用户自定义过的列序也算**，照抄 `state.columns()`；
/// - 不分组：只有一摞、也不画组头。
///
/// 除「状态」外空组一律丢掉（原型 `groups.filter(...)`）：一个「P3 低 · 0 项」的表头
/// 占位置又没内容。状态是例外 —— 四列都摆出来，用户才知道「搁置的现在一条都没有」。
fn grouped_tasks(state: &ProjectsState) -> Vec<TaskGroup<'_>> {
    let tasks = state.tasks();
    match state.group() {
        GroupMode::None => vec![TaskGroup {
            key: GroupKey::All,
            tasks: tasks.iter().collect(),
        }],
        GroupMode::Priority => Priority::ALL
            .iter()
            .map(|priority| TaskGroup {
                key: GroupKey::Priority(*priority),
                tasks: tasks
                    .iter()
                    .filter(|task| task.priority == *priority)
                    .collect(),
            })
            .filter(|group| !group.tasks.is_empty())
            .collect(),
        GroupMode::Project => {
            let mut groups: Vec<TaskGroup<'_>> = state
                .projects()
                .iter()
                .map(|project| TaskGroup {
                    key: GroupKey::Project(Some(project.id)),
                    tasks: tasks
                        .iter()
                        .filter(|task| task.project_id == Some(project.id))
                        .collect(),
                })
                .filter(|group| !group.tasks.is_empty())
                .collect();
            let inbox: Vec<&Task> = tasks
                .iter()
                .filter(|task| task.project_id.is_none())
                .collect();
            if !inbox.is_empty() {
                groups.push(TaskGroup {
                    key: GroupKey::Project(None),
                    tasks: inbox,
                });
            }
            groups
        }
        GroupMode::Status => state
            .columns()
            .iter()
            .map(|column| TaskGroup {
                key: GroupKey::Status(column.status_key),
                tasks: tasks
                    .iter()
                    .filter(|task| task.status == column.status_key)
                    .collect(),
            })
            .collect(),
    }
}

/// 一摞的组头（原型 `.lgroup`）：色点 + 名字 + 「n 项」。
///
/// 「不分组」没有组头（[`GroupKey::All`]）—— 只有一张表的时候，一个「全部 · n 项」
/// 的表头纯属多一行。
fn group_header(group: &TaskGroup<'_>, state: &ProjectsState, cx: &App) -> Option<AnyElement> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let (dot, label) = match group.key {
        GroupKey::All => return None,
        GroupKey::Priority(priority) => (
            priority_color(priority, cx),
            SharedString::from(priority_label(priority, cx)),
        ),
        GroupKey::Project(Some(id)) => (
            state
                .projects()
                .iter()
                .find(|project| project.id == id)
                .map_or(tokens.colors.muted_foreground, |project| {
                    color_from_hex(&project.color).unwrap_or(tokens.colors.muted_foreground)
                }),
            project_name(state, id, cx),
        ),
        GroupKey::Project(None) => (
            extended.colors.text_tertiary,
            SharedString::from(tr(cx, "未归入项目")),
        ),
        GroupKey::Status(status) => (
            status_color(status, cx),
            SharedString::from(status_label(status, cx)),
        ),
    };

    Some(
        h_flex()
            .w_full()
            .flex_none()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .bg(tokens.colors.background)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(div().flex_none().size(px(8.)).rounded_full().bg(dot))
            .child(
                div()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.foreground)
                    .child(label),
            )
            .child(count_badge(group.tasks.len() as u32, cx))
            .into_any_element(),
    )
}

/// 空分组的占位行（只有「按状态分组」会出现，见 [`grouped_tasks`]）。
fn group_empty_row(key: GroupKey, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .w_full()
        .flex_none()
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_b(theme.extended().stroke.thin)
        .border_color(theme.extended().colors.hairline)
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(theme.extended().colors.text_tertiary)
        .child(SharedString::from(match key {
            GroupKey::Status(_) => tr(cx, "这一列下暂无任务"),
            _ => tr(cx, "此分组暂无任务"),
        }))
        .into_any_element()
}

/// 状态色点：与时间线条带同一套语义（已完成 → 成功、受阻 → 警告、进行中 → 强调、
/// 待办 → 弱化）。
/// 状态 → 颜色（原型 `.cdot.todo/.doing/.blocked/.done`）。
///
/// 列表分组头与看板列头共用：同一个状态在两处必须是同一个颜色，各写一份迟早对不上。
pub(super) fn status_color(status: TaskStatus, cx: &App) -> Hsla {
    let theme = active_theme(cx);
    match status {
        TaskStatus::Done => theme.extended().colors.success,
        TaskStatus::Blocked => theme.extended().colors.warning,
        TaskStatus::Doing => theme.tokens().colors.primary,
        TaskStatus::Todo => theme.tokens().colors.muted_foreground,
        TaskStatus::Deferred | TaskStatus::Cancelled => theme.extended().colors.text_tertiary,
    }
}

/// 列表最左边那个勾（原型 `.lcheck`）。
///
/// 15px 的圆角方块，勾上时整块转强调色底 + 勾号。勾选必须是**独立的手势**：
/// 整行点下去是「选中 → 右栏看详情」，顺手把勾也翻过来会让人不敢点行。
/// 所以它在 `on_mouse_down` 上 `stop_propagation`，把这次点击从行的处理器那里截住。
fn check_box(
    id: SharedString,
    checked: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<gpui::Div> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let accent = tokens.colors.primary;

    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(CHECK_BOX_SIZE))
        // 4px 圆角取自原型 `.lcheck`，阶梯里没有这一档。
        .rounded(px(4.))
        .border(extended.stroke.thin)
        .border_color(if checked {
            accent
        } else {
            tokens.colors.border
        })
        .bg(if checked {
            accent
        } else {
            tokens.colors.surface
        })
        .cursor_pointer()
        .role(Role::CheckBox)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
        .children(checked.then(|| {
            Icon::new(DuckIcon::Check)
                .size(px(11.))
                .text_color(tokens.colors.primary_foreground)
        }))
}

/// 表头格子的骨架：字阶 / 颜色 / 宽度（`None` = 撑满剩余）。
fn header_cell_frame(width: Option<f32>, cx: &App) -> gpui::Div {
    let tokens = active_theme(cx).tokens();
    let mut cell = div()
        .flex_none()
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.muted_foreground);
    if let Some(width) = width {
        cell = cell.w(px(width));
    } else {
        cell = cell.flex_1().min_w_0().truncate();
    }
    cell
}

/// 不可排序的表头格（「项目 / 状态 / 标签」—— 内核的 [`TaskSort`] 里没有这三档）。
fn plain_cell(width: Option<f32>, label: &'static str, cx: &App) -> AnyElement {
    header_cell_frame(width, cx)
        .child(SharedString::from(tr(cx, label)))
        .into_any_element()
}

/// 可排序的表头格（原型 `.lsort`）：点一下按这一列排，再点一下换方向。
///
/// `sorts` 是这一列的档位链：单档（标题 / 优先级 / 进度）点了就停在它上面，
/// 双档（截止）在升 / 降之间来回 —— 链的长度就是「有几个方向」。
/// 当前档落在这条链上时才画箭头，免得每一列表头都挂一个不生效的箭头。
///
/// `id_key` 用显式 ASCII 键而不是列名：列名是中文源串（会被翻译），元素 id 一旦
/// 跟着语言变，gpui 就当成另一个元素，焦点与悬停态都会丢。
fn sort_cell(
    id_key: &'static str,
    width: Option<f32>,
    label: &'static str,
    sorts: &'static [TaskSort],
    current: TaskSort,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    // 颜色是 `Copy` 的，先取出来再 move 进 `'static` 闭包（闭包借不到 `cx`）。
    let accent = tokens.colors.primary;
    let foreground = tokens.colors.foreground;
    let active = sorts.iter().position(|sort| *sort == current);

    let mut cell = header_cell_frame(width, cx)
        .id(SharedString::from(format!("projects-sort-{id_key}")))
        .flex()
        .flex_row()
        .items_center()
        .gap(tokens.spacing.xxs)
        .cursor_pointer()
        // 自绘的可点元素对读屏不存在，角色与名字都得显式给。
        .role(Role::Button)
        .aria_label(SharedString::from(tr(cx, label)))
        .hover(move |style| style.text_color(foreground))
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                // 当前档在链上就往后挪一格（到头绕回），否则进入这条链的第一档。
                let next = match sorts.iter().position(|sort| *sort == current) {
                    Some(index) => sorts[(index + 1) % sorts.len()],
                    None => sorts[0],
                };
                let _ = view.update(cx, |page, cx| page.set_sort(next, cx));
            }
        })
        .child(SharedString::from(tr(cx, label)));
    if let Some(index) = active {
        // 方向箭头用 `ArrowUp/Down` 而不是 `Chevron*`：图标集里**没有** `ChevronUp`
        // （只有 Down / Left / Right），而排序的方向语义本来就该是箭头。
        cell = cell.child(
            Icon::new(if index == 0 {
                DuckIcon::ArrowUp
            } else {
                DuckIcon::ArrowDown
            })
            .size(extended.icon.sm)
            .text_color(accent),
        );
    }
    cell.into_any_element()
}

/// 列表表头：全选勾 + 七列（其中四列可点排序）。
fn list_header(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let current = state.sort();

    h_flex()
        .w_full()
        .flex_none()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .child(
            div()
                .flex_none()
                .w(px(LIST_CHECK_WIDTH))
                .flex()
                .justify_center()
                .child(
                    check_box(
                        SharedString::from("projects-list-check-all"),
                        state.all_checked(),
                        {
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |page, cx| page.toggle_check_all(cx));
                            }
                        },
                        cx,
                    )
                    .aria_label(tr(cx, "全选")),
                ),
        )
        .child(sort_cell(
            "title",
            None,
            "任务",
            &[TaskSort::TitleAsc],
            current,
            view,
            cx,
        ))
        .child(plain_cell(Some(LIST_PROJECT_WIDTH), "项目", cx))
        .child(sort_cell(
            "priority",
            Some(LIST_PRIORITY_WIDTH),
            "优先级",
            &[TaskSort::PriorityFirst],
            current,
            view,
            cx,
        ))
        .child(plain_cell(Some(LIST_STATUS_WIDTH), "状态", cx))
        .child(sort_cell(
            "due",
            Some(LIST_DUE_WIDTH),
            "截止",
            &[TaskSort::DueAsc, TaskSort::DueDesc],
            current,
            view,
            cx,
        ))
        .child(sort_cell(
            "progress",
            Some(LIST_PROGRESS_WIDTH),
            "进度",
            &[TaskSort::ProgressDesc],
            current,
            view,
            cx,
        ))
        .child(plain_cell(Some(LIST_TAGS_WIDTH), "标签", cx))
        .into_any_element()
}

/// 批量条（原型 `.bulkbar`）：`N 项已选中` + 六个动作。
///
/// 只在有勾选时出现，压在表格**上方**：它讲的是「这一屏选了几条」，离被选中的行越近越好。
fn bulk_bar(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let count = state.checked_count();

    // 三个「选一个值」的动作走下拉（与工具条的排序 / 分组同一套），
    // 两个直接生效，一个不可逆的走二次确认。
    let status_menu = {
        let view = view.clone();
        let items: Vec<(TaskStatus, SharedString)> = TaskStatus::ALL
            .iter()
            .map(|status| (*status, SharedString::from(status_label(*status, cx))))
            .collect();
        toolbar_text_dropdown_button(
            "projects-bulk-status",
            SharedString::from(tr(cx, "改状态")),
            Icon::new(DuckIcon::SlidersHorizontal).size(extended.icon.sm),
            Anchor::TopLeft,
            move |menu, _, _cx| {
                items.iter().fold(menu, |menu, (status, label)| {
                    let status = *status;
                    menu.item(PopupMenuItem::new(label.clone()).on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |page, cx| page.bulk_status(status, cx));
                        }
                    }))
                })
            },
            cx,
        )
    };

    let priority_menu = {
        let view = view.clone();
        let items: Vec<(Priority, SharedString)> = Priority::ALL
            .iter()
            .map(|priority| {
                (
                    *priority,
                    SharedString::from(format!(
                        "P{} {}",
                        priority.as_i64(),
                        priority_label(*priority, cx)
                    )),
                )
            })
            .collect();
        toolbar_text_dropdown_button(
            "projects-bulk-priority",
            SharedString::from(tr(cx, "改优先级")),
            Icon::new(DuckIcon::Flag).size(extended.icon.sm),
            Anchor::TopLeft,
            move |menu, _, _cx| {
                items.iter().fold(menu, |menu, (priority, label)| {
                    let priority = *priority;
                    menu.item(PopupMenuItem::new(label.clone()).on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |page, cx| page.bulk_priority(priority, cx));
                        }
                    }))
                })
            },
            cx,
        )
    };

    let project_menu =
        {
            let view = view.clone();
            let items: Vec<(Option<i64>, SharedString)> =
                std::iter::once((None, SharedString::from(tr(cx, "未归入项目"))))
                    .chain(state.projects().iter().map(|project| {
                        (Some(project.id), SharedString::from(project.name.clone()))
                    }))
                    .collect();
            toolbar_text_dropdown_button(
                "projects-bulk-project",
                SharedString::from(tr(cx, "移入项目")),
                Icon::new(DuckIcon::FolderOpen).size(extended.icon.sm),
                Anchor::TopLeft,
                move |menu, _, _cx| {
                    items.iter().fold(menu, |menu, (target, label)| {
                        let target = *target;
                        menu.item(PopupMenuItem::new(label.clone()).on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |page, cx| page.bulk_project(target, cx));
                            }
                        }))
                    })
                },
                cx,
            )
        };

    h_flex()
        .w_full()
        .flex_none()
        .gap(tokens.spacing.xs)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .mb(tokens.spacing.sm)
        .rounded(theme.components().card_radius)
        .border(extended.stroke.thin)
        .border_color(tokens.colors.primary)
        .bg(tokens.colors.primary.opacity(extended.opacity.tint))
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .text_color(tokens.colors.primary)
        .child(SharedString::from(trf(
            cx,
            "{} 项已选中",
            &[&count.to_string()],
        )))
        .child(div().flex_1())
        .child(
            icon_text_button(
                "projects-bulk-done",
                tr(cx, "标记完成"),
                Icon::new(DuckIcon::Check).size(extended.icon.sm),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |page, cx| {
                        page.bulk_status(duck_project::TaskStatus::Done, cx);
                    });
                }
            }),
        )
        .child(status_menu)
        .child(priority_menu)
        .child(project_menu)
        .child(
            icon_text_button(
                "projects-bulk-delete",
                tr(cx, "删除"),
                Icon::new(DuckIcon::Trash2).size(extended.icon.sm),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |page, cx| page.confirm_delete_checked(window, cx));
                }
            }),
        )
        .child(
            icon_text_button(
                "projects-bulk-clear",
                tr(cx, "取消选择"),
                Icon::new(DuckIcon::X).size(extended.icon.sm),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |page, cx| page.clear_checked(cx));
                }
            }),
        )
        .into_any_element()
}

/// 列表的一行。
fn list_row(
    task: &Task,
    state: &ProjectsState,
    today: duck_project::CalendarDate,
    selected: Option<i64>,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let is_selected = selected == Some(task.id);
    let checked = state.is_checked(task.id);
    let done = task.status == duck_project::TaskStatus::Done;

    let project = task
        .project_id
        .and_then(|id| state.projects().iter().find(|entry| entry.id == id));
    let dot_color = project
        .map(|project| color_from_hex(&project.color).unwrap_or(tokens.colors.muted_foreground))
        .unwrap_or(tokens.colors.muted_foreground);

    let cell = |width: f32| div().flex_none().w(px(width)).min_w_0().truncate();

    div()
        .id(SharedString::from(format!("projects-list-row-{}", task.id)))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(SharedString::from(task.title.clone()))
        .when(is_selected, |row| row.bg(extended.colors.nav_selected))
        .hover(move |style| style.bg(extended.colors.hover_tint))
        .on_click({
            let view = view.clone();
            let id = task.id;
            let status = task.status;
            move |event: &ClickEvent, window, cx| {
                // 双击开完整编辑器（原型全局 `dblclick` → `openTaskEditor`）：与看板
                // 卡片同一个手势，判据见 `widgets::is_double_click`。
                if is_double_click(event) {
                    let _ = view.update(cx, |page, cx| {
                        super::dialogs::open_task_editor(page, Some(id), None, status, window, cx)
                    });
                    return;
                }
                let _ = view.update(cx, |page, cx| page.select_task(Some(id), cx));
            }
        })
        // 列表行与看板卡片共用同一个右键菜单（原型里两者都挂 `data-card`，
        // 全局 `contextmenu` 处理器一视同仁），见 `widgets::task_menu`。
        .context_menu({
            let view = view.clone();
            let id = task.id;
            move |menu, window, cx| {
                let Some((task, projects, columns)) = view
                    .read_with(cx, |page, cx| page.menu_data(id, cx))
                    .ok()
                    .flatten()
                else {
                    return menu;
                };
                task_menu(menu, &task, &projects, &columns, &view, window, cx)
            }
        })
        // 勾选列（原型 `.lcell.ck`）：整行点击已经是「选中 → 右栏」，
        // 勾选是另一个手势，所以在 `on_mouse_down` 上截住冒泡。
        .child(
            div()
                .flex_none()
                .w(px(LIST_CHECK_WIDTH))
                .flex()
                .justify_center()
                .child(
                    check_box(
                        SharedString::from(format!("projects-list-check-{}", task.id)),
                        checked,
                        {
                            let view = view.clone();
                            let id = task.id;
                            move |_, _, cx| {
                                let _ = view.update(cx, |page, cx| page.toggle_checked(id, cx));
                            }
                        },
                        cx,
                    )
                    .aria_label(SharedString::from(task.title.clone())),
                ),
        )
        // 任务：项目色点 + 标题（已完成加删除线）。
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap(tokens.spacing.xs)
                .child(
                    div()
                        .flex_none()
                        .size(tokens.typography.xs.size / 1.2)
                        .rounded_full()
                        .bg(dot_color),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .text_color(if done {
                            extended.colors.text_tertiary
                        } else {
                            tokens.colors.foreground
                        })
                        .when(done, |title| title.line_through())
                        .child(SharedString::from(task.title.clone())),
                ),
        )
        // 项目名。
        .child(
            cell(LIST_PROJECT_WIDTH)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(project.map_or_else(
                    || SharedString::from("—"),
                    |project| SharedString::from(project.name.clone()),
                )),
        )
        // 优先级。
        .child(
            cell(LIST_PRIORITY_WIDTH)
                .flex()
                .flex_row()
                .items_center()
                .gap(tokens.spacing.xxs)
                .child(priority_dot(task.priority, cx)),
        )
        // 状态。
        .child(
            cell(LIST_STATUS_WIDTH)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(status_label(task.status, cx))),
        )
        // 截止。
        .child({
            // 与看板卡片 `.pmeta` 共用一份口径（原型 `dueLabel`）：同一张卡在两种视图里
            // 对同一个日期给两种说法，是最容易被当成 bug 的那种不一致。
            let (text, color) = due_label(task, today, cx);
            cell(LIST_DUE_WIDTH)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(color)
                .child(text)
        })
        // 进度。
        .child(
            cell(LIST_PROGRESS_WIDTH)
                .flex()
                .flex_row()
                .items_center()
                .gap(tokens.spacing.xxs)
                .child(div().flex_1().min_w_0().child(task_progress(
                    SharedString::from(format!("projects-list-progress-{}", task.id)),
                    task.progress,
                    cx,
                ))),
        )
        // 标签。
        .child(
            cell(LIST_TAGS_WIDTH)
                .flex()
                .flex_row()
                .items_center()
                .gap(tokens.spacing.xxs)
                .children(task.tags.iter().take(2).map(|tag| tag_chip(tag, cx))),
        )
        .into_any_element()
}

/// 看板本体。
fn render_board(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();

    if state.task_total() == 0 {
        return main_empty_state(state, cx);
    }

    let today = state.today();
    let selected = state.selected();
    // 卡片上的项目色点要项目表：这一层是取数的地方（同右栏），组件自己不碰状态。
    let projects = state.projects();
    let columns: Vec<AnyElement> = state
        .columns()
        .iter()
        .map(|column| {
            let tasks = state.tasks_in_status(column.status_key);
            kanban_column(column, &tasks, projects, today, selected, view, cx)
        })
        .collect();

    div()
        .id("projects-board")
        .flex_1()
        .min_h_0()
        .min_w_0()
        .w_full()
        .overflow_x_scroll()
        .p(tokens.spacing.sm)
        .child(
            // 满高并排行必须用 `div().flex().flex_row()`：`h_flex()` 是 cross-axis
            // 居中，列会被压成内容高度而不是撑满。
            div()
                .flex()
                .flex_row()
                .h_full()
                .gap(tokens.spacing.sm)
                .children(columns),
        )
        .into_any_element()
}

/// 状态栏：`范围：X · 本视图 N 条 · 共 N · 完成 N · 逾期 N [· 搜索：词]`，右侧一行快捷键提示。
///
/// 「本视图」是当前这一屏的条数，「共 / 完成 / 逾期」走左栏那一份**全局**计数
/// （原型同款）—— 两个口径同时摆出来，才看得出筛选收窄了多少。
fn render_status_bar(state: &ProjectsState, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let counts = state.sidebar_counts();
    let scope = scope_text(state, cx);

    let mut row = h_flex()
        .flex_none()
        .w_full()
        .h(theme.density().section_header)
        .gap(tokens.spacing.md)
        .px(tokens.spacing.sm)
        .border_t(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(SharedString::from(trf(cx, "范围：{}", &[scope.as_str()])))
        .child(SharedString::from(trf(
            cx,
            "本视图 {} 条",
            &[&state.task_total().to_string()],
        )))
        .child(SharedString::from(trf(
            cx,
            "共 {}",
            &[&counts.all.to_string()],
        )))
        .child(SharedString::from(trf(
            cx,
            "完成 {}",
            &[&counts.done.to_string()],
        )))
        .child(
            div()
                .flex_none()
                .text_color(tokens.colors.destructive)
                .child(SharedString::from(trf(
                    cx,
                    "逾期 {}",
                    &[&counts.overdue.to_string()],
                ))),
        );

    let term = state.search().trim();
    if !term.is_empty() {
        row = row.child(SharedString::from(trf(cx, "搜索：{}", &[term])));
    }

    row.child(div().flex_1().min_w_0())
        .child(
            div()
                .flex_none()
                .text_color(extended.colors.text_tertiary)
                .child(SharedString::from(tr(
                    cx,
                    "N 快速新建 · ⌘N 新建任务 · ⌘K 命令面板 · 右键卡片有菜单",
                ))),
        )
        .into_any_element()
}

/// 状态栏「范围：」后面那一段。
///
/// 范围 / 智能列表 / 标签三者互斥、且状态里只有一份，所以这里按优先级取其一即可，
/// 不存在「同时看着两处范围」的情况。
fn scope_text(state: &ProjectsState, cx: &App) -> SharedString {
    if let Some(filter) = state.smart() {
        return SharedString::from(trf(cx, "智能列表 · {}", &[smart_label(filter, cx)]));
    }
    if let Some(id) = state.tag() {
        let name = state.tags().iter().find(|tag| tag.id == id).map_or_else(
            || SharedString::from(tr(cx, "标签")),
            |tag| SharedString::from(tag.name.clone()),
        );
        return SharedString::from(trf(cx, "标签 · {}", &[name.as_str()]));
    }
    match state.scope() {
        BoardScope::All => SharedString::from(tr(cx, "全部任务")),
        BoardScope::Inbox => SharedString::from(tr(cx, "未归入项目")),
        BoardScope::Project(id) => project_name(state, id, cx),
    }
}

// ---------------------------------------------------------------------------
// 视图：日历 / 时间线 / 报表 / 工作日志
// ---------------------------------------------------------------------------
//
// 四个视图读的都是**同一份** `state.tasks()`（口径在 `ProjectsState::query`），
// 所以「范围 + 筛选 + 搜索 + 排序」的结果与看板逐条一致 —— 切视图不该改变看到的东西。

/// 日历网格恒为 6 行 × 7 列（42 格）。
///
/// 行数随月份变（4~6 行）会让每翻一页网格高度就跳一次，鼠标下的格子会挪位；
/// 恒 6 行则「下个月」永远在同一个位置。
const CALENDAR_CELLS: i64 = 42;
/// 日历格高（不属间距阶梯、也不是字号，故留裸值）：容得下日号 + 两行任务。
const CALENDAR_CELL_HEIGHT: f32 = 92.;

/// 时间线的窗口长度（天）、每行高度、左侧任务名宽度。
const TIMELINE_DAYS: i64 = 28;
const TIMELINE_ROW_HEIGHT: f32 = 26.;
const TIMELINE_LABEL_WIDTH: f32 = 180.;

/// 「进行中」的 WIP 上限（与原型一致：超过就提示超限，**不做硬拦**）。
const WIP_LIMIT: u32 = 5;

/// 任务「归到哪一天」：已完成按完成日，其余按截止日（原型 `dayOf` 同口径）。
///
/// 没有日期的任务**不显示**在日历上 —— 日历的意义就是「日子 × 事情」，
/// 硬塞进某一天（比如今天）只会让那天的格子骗人。
fn calendar_day(task: &Task) -> Option<CalendarDate> {
    if task.status == TaskStatus::Done {
        task.completed_at.map(local_date).or(task.due_date)
    } else {
        task.due_date
    }
}

/// 日历视图：月网格（按截止日归位，已完成按完成日归位）。
fn render_calendar(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let today = state.today();
    let anchor = state.calendar_month().unwrap_or(today);
    let Some(first) = CalendarDate::new(anchor.year(), anchor.month(), 1) else {
        return main_empty_state(state, cx);
    };
    let grid_start = first.start_of_week();
    let tasks = state.tasks();

    let mut grid = v_flex().w_full().flex_none();
    for row in 0..(CALENDAR_CELLS / 7) {
        let mut line = div().flex().flex_row().items_stretch().w_full().flex_none();
        for column in 0..7 {
            let day = grid_start.add_days(row * 7 + column);
            line = line.child(calendar_cell(day, anchor, today, tasks, view, cx));
        }
        grid = grid.child(line);
    }

    v_flex()
        .size_full()
        .min_h_0()
        .min_w_0()
        .child(calendar_header(anchor, view, cx))
        .child(
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .p(tokens.spacing.sm)
                .child(
                    v_flex()
                        .w_full()
                        .flex_1()
                        .min_h_0()
                        .rounded(theme.components().card_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.surface)
                        .overflow_hidden()
                        .child(weekday_row(cx))
                        .child(grid),
                ),
        )
        .into_any_element()
}

/// 日历工具条：翻月 + 回到今天 + 月份标题 + 口径说明。
fn calendar_header(anchor: CalendarDate, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let icon_size = extended.icon.sm;
    let month_label = SharedString::from(trf(
        cx,
        "{} 年 {} 月",
        &[&anchor.year().to_string(), &anchor.month().to_string()],
    ));

    let step = |id: &'static str, label: &'static str, icon: DuckIcon, delta: i32, cx: &App| {
        let view = view.clone();
        icon_button(
            id,
            label,
            Icon::new(icon).size(icon_size),
            ButtonVariant::Ghost,
            cx,
        )
        .on_click(move |_, _, cx| {
            let _ = view.update(cx, |page, cx| page.step_calendar_month(Some(delta), cx));
        })
    };

    h_flex()
        .flex_none()
        .w_full()
        .gap(tokens.spacing.xs)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(step(
            "projects-calendar-prev",
            tr(cx, "上个月"),
            DuckIcon::ChevronLeft,
            -1,
            cx,
        ))
        .child(step(
            "projects-calendar-next",
            tr(cx, "下个月"),
            DuckIcon::ChevronRight,
            1,
            cx,
        ))
        .child({
            let view = view.clone();
            button(
                "projects-calendar-today",
                tr(cx, "今天"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.step_calendar_month(None, cx));
            })
        })
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(month_label),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(tr(
                    cx,
                    "按截止日归位 · 已完成按完成日归位 · 无日期任务不显示",
                ))),
        )
        .child(div().flex_1().min_w_0())
        .into_any_element()
}

/// 星期行（周一起，与内核的 [`Weekday`] 同序）。
fn weekday_row(cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let labels = ["一", "二", "三", "四", "五", "六", "日"];
    div()
        .flex()
        .flex_row()
        .w_full()
        .flex_none()
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .children(labels.iter().enumerate().map(|(index, label)| {
            div()
                .flex_1()
                .min_w_0()
                .py(tokens.spacing.xxs)
                .flex()
                .justify_center()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if index >= 5 {
                    extended.colors.text_tertiary
                } else {
                    tokens.colors.muted_foreground
                })
                .child(SharedString::from(*label))
        }))
        .into_any_element()
}

/// 一个日历格：日号 + 最多两条任务 + 「+N 更多」。
fn calendar_cell(
    day: CalendarDate,
    anchor: CalendarDate,
    today: CalendarDate,
    tasks: &[Task],
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let mut items: Vec<&Task> = tasks
        .iter()
        .filter(|task| calendar_day(task) == Some(day))
        .collect();
    items.sort_by_key(|task| task.priority.as_i64());

    let other_month = day.month() != anchor.month();
    let is_today = day == today;

    let mut cell = v_flex()
        .id(SharedString::from(format!(
            "projects-calendar-{}",
            day.to_iso()
        )))
        .flex_1()
        .min_w_0()
        .h(px(CALENDAR_CELL_HEIGHT))
        .gap(tokens.spacing.xxs)
        .p(tokens.spacing.xxs)
        .border_r(extended.stroke.thin)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .overflow_hidden();

    // 日号：今天加强调色圆底，跨月的日子淡化。
    let day_number = div()
        .flex_none()
        .size(tokens.spacing.lg)
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(if is_today {
            tokens.colors.primary_foreground
        } else if other_month {
            extended.colors.text_tertiary
        } else {
            tokens.colors.foreground
        })
        .when(is_today, |this| this.bg(tokens.colors.primary))
        .child(SharedString::from(day.day().to_string()));

    let mut head = h_flex()
        .flex_none()
        .w_full()
        .gap(tokens.spacing.xxs)
        .child(day_number);
    if !items.is_empty() {
        head = head.child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(items.len().to_string())),
        );
    }
    cell = cell.child(head);

    for task in items.iter().take(2) {
        let id = task.id;
        let view = view.clone();
        cell = cell.child(
            h_flex()
                .id(SharedString::from(format!("projects-calendar-task-{id}")))
                .flex_none()
                .w_full()
                .min_w_0()
                .gap(tokens.spacing.xxs)
                .px(tokens.spacing.xxs)
                .rounded(tokens.spacing.xxs)
                .cursor_pointer()
                .hover(move |style| style.bg(extended.colors.hover_tint))
                .role(Role::Button)
                .aria_label(SharedString::from(task.title.clone()))
                .child(priority_dot(task.priority, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(if task.status == TaskStatus::Done {
                            tokens.colors.muted_foreground
                        } else {
                            tokens.colors.foreground
                        })
                        .child(SharedString::from(task.title.clone())),
                )
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |page, cx| page.select_task(Some(id), cx));
                }),
        );
    }
    if items.len() > 2 {
        cell = cell.child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(trf(
                    cx,
                    "+{} 更多",
                    &[&(items.len() - 2).to_string()],
                ))),
        );
    }

    cell.when(other_month, |this| this.opacity(extended.opacity.muted))
        .into_any_element()
}

/// 时间线视图：近 4 周，一行一个任务（有开始或截止日期的才排得上）。
///
/// 只画 16 行（原型同口径）：时间线是「扫一眼排期」，不是全量清单；要全量去列表。
fn render_timeline(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let today = state.today();
    // 窗口从「本周一往前一周」开始，正好把过去一周与未来三周都罩进来。
    let window_start = today.start_of_week().add_days(-7);

    let rows: Vec<&Task> = state
        .tasks()
        .iter()
        .filter(|task| task.start_date.is_some() || task.due_date.is_some())
        .take(16)
        .collect();

    let mut body = v_flex().w_full().flex_none();
    if rows.is_empty() {
        body = body.child(
            div()
                .w_full()
                .p(tokens.spacing.lg)
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(tr(
                    cx,
                    "没有带日期的任务 —— 时间线只展示有开始或截止日期的任务。",
                ))),
        );
    } else {
        for task in rows {
            body = body.child(timeline_row(task, window_start, today, view, cx));
        }
    }

    v_flex()
        .size_full()
        .min_h_0()
        .min_w_0()
        .child(timeline_header(window_start, today, cx))
        .child(
            div()
                .id("projects-timeline")
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .p(tokens.spacing.sm)
                .child(
                    v_flex()
                        .w_full()
                        .rounded(theme.components().card_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.surface)
                        .overflow_hidden()
                        .child(body),
                ),
        )
        .into_any_element()
}

/// 时间线的刻度头：左侧留白 + 28 天刻度（只标周一，否则糊成一片）。
fn timeline_header(window_start: CalendarDate, today: CalendarDate, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    h_flex()
        .flex_none()
        .w_full()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(
            div()
                .flex_none()
                .w(px(TIMELINE_LABEL_WIDTH))
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(tr(cx, "任务 · 近 4 周"))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .children((0..TIMELINE_DAYS).map(|offset| {
                    let day = window_start.add_days(offset);
                    let monday = day.weekday() == Weekday::Monday;
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .justify_center()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(if day == today {
                            tokens.colors.primary
                        } else if day.weekday().is_weekend() {
                            extended.colors.text_tertiary
                        } else {
                            tokens.colors.muted_foreground
                        })
                        .child(if monday {
                            SharedString::from(trf(
                                cx,
                                "{}/{}",
                                &[&day.month().to_string(), &day.day().to_string()],
                            ))
                        } else {
                            SharedString::default()
                        })
                })),
        )
        .into_any_element()
}

/// 时间线一行：任务名 + 起止条（只有截止日的画一个里程碑方块）。
fn timeline_row(
    task: &Task,
    window_start: CalendarDate,
    today: CalendarDate,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let id = task.id;

    let start = task.start_date.unwrap_or(today);
    let end = task.due_date.unwrap_or(start);
    let i0 = start.days_since_epoch() - window_start.days_since_epoch();
    let i1 = end.days_since_epoch() - window_start.days_since_epoch();

    // 颜色：已完成 → 成功色，逾期 → 危险色，受阻 → 警告色，其余 → 强调色。
    let tone = if task.status == TaskStatus::Done {
        extended.colors.success
    } else if task.is_overdue(today) {
        tokens.colors.destructive
    } else if task.status == TaskStatus::Blocked {
        extended.colors.warning
    } else {
        tokens.colors.primary
    };

    let track = {
        let mut track = div()
            .flex_1()
            .min_w_0()
            .h(px(TIMELINE_ROW_HEIGHT))
            .relative()
            .overflow_hidden();

        // 底纹：周末一个浅色块，让「周」在横向可数。
        let mut backdrop = div().flex().flex_row().size_full().absolute();
        for offset in 0..TIMELINE_DAYS {
            let weekend = window_start.add_days(offset).weekday().is_weekend();
            backdrop = backdrop.child(
                div()
                    .flex_1()
                    .h_full()
                    .when(weekend, |this| this.bg(extended.colors.hover_tint)),
            );
        }
        track = track.child(backdrop);

        let has_span = task.start_date.is_some()
            && task.due_date.is_some()
            && task.due_date != task.start_date;
        if has_span {
            // 有起止：横条。左缘按起始日、宽度按跨度；两端都夹进窗口内。
            let clipped_start = i0.max(0);
            let clipped_end = i1.min(TIMELINE_DAYS - 1);
            let left = clipped_start as f32 / TIMELINE_DAYS as f32;
            let span = (clipped_end - clipped_start + 1).max(1) as f32;
            let width = (span / TIMELINE_DAYS as f32).max(0.032);
            track = track.child(
                div()
                    .absolute()
                    .left(relative(left))
                    .top(px(6.))
                    .w(relative(width))
                    .h(px(TIMELINE_ROW_HEIGHT - 12.))
                    .rounded_full()
                    .bg(tone)
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .text_color(tokens.colors.primary_foreground)
                    .child(if span >= 3. {
                        SharedString::from(trf(cx, "{}%", &[&task.progress.to_string()]))
                    } else {
                        SharedString::default()
                    }),
            );
        } else if (0..TIMELINE_DAYS).contains(&i1) {
            // 只有截止日：一个里程碑方块，落在那一天的中线上。
            let left = (i1 as f32 + 0.5) / TIMELINE_DAYS as f32;
            track = track.child(
                div()
                    .absolute()
                    .left(relative(left))
                    .top(px(TIMELINE_ROW_HEIGHT / 2. - 4.))
                    .size(px(8.))
                    .rounded(px(2.))
                    .bg(tone),
            );
        }

        // 今天那条竖线（窗口外面就不画）。
        let today_left = (today.days_since_epoch() - window_start.days_since_epoch()) as f32 + 0.5;
        if (0. ..TIMELINE_DAYS as f32).contains(&today_left) {
            track = track.child(
                div()
                    .absolute()
                    .left(relative(today_left / TIMELINE_DAYS as f32))
                    .top_0()
                    .h_full()
                    .w(px(1.))
                    .bg(tokens.colors.destructive),
            );
        }
        track
    };

    h_flex()
        .id(SharedString::from(format!("projects-timeline-row-{id}")))
        .flex_none()
        .w_full()
        .h(px(TIMELINE_ROW_HEIGHT))
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(SharedString::from(task.title.clone()))
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.select_task(Some(id), cx));
            }
        })
        .child(
            h_flex()
                .flex_none()
                .w(px(TIMELINE_LABEL_WIDTH))
                .min_w_0()
                .gap(tokens.spacing.xs)
                .child(priority_dot(task.priority, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(task.title.clone())),
                ),
        )
        .child(track)
        .into_any_element()
}

/// 报表视图：KPI + 近 7 天完成趋势 + 按项目分布 + 逾期清单。
///
/// 数字全部**现算**（`state.tasks()` 一次遍历），不额外落统计表 —— 统计表要跟着
/// 每一次写维护，早晚会有一次忘掉，然后就出现「报表和看板对不上」。
/// 口径与看板一致：报的是**当前范围**（状态栏那条「范围：…」）里的事。
fn render_report(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let today = state.today();
    let tasks = state.tasks();

    let count = |status: TaskStatus| tasks.iter().filter(|task| task.status == status).count();
    let open: Vec<&Task> = tasks
        .iter()
        .filter(|task| !task.status.is_settled())
        .collect();
    let avg_progress = if open.is_empty() {
        0
    } else {
        open.iter()
            .map(|task| u32::from(task.progress))
            .sum::<u32>()
            / open.len() as u32
    };
    let mut overdue: Vec<&Task> = tasks.iter().filter(|task| task.is_overdue(today)).collect();
    overdue.sort_by_key(|task| task.due_date.map_or(0, CalendarDate::days_since_epoch));
    let max_overdue = overdue
        .iter()
        .filter_map(|task| task.due_date)
        .map(|due| today.days_until(due).unsigned_abs())
        .max()
        .unwrap_or(0);

    let kpis = h_flex()
        .flex_none()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(report_kpi(
            "projects-kpi-done",
            DuckIcon::CircleCheck,
            tr(cx, "已完成"),
            SharedString::from(count(TaskStatus::Done).to_string()),
            tokens.colors.foreground,
            SharedString::from(trf(
                cx,
                "近 7 天完成 {} 个",
                &[&completed_within(tasks, today, 7).to_string()],
            )),
            cx,
        ))
        .child(report_kpi(
            "projects-kpi-doing",
            DuckIcon::Play,
            tr(cx, "进行中"),
            SharedString::from(count(TaskStatus::Doing).to_string()),
            tokens.colors.foreground,
            SharedString::from(doing_note(count(TaskStatus::Doing) as u32, cx)),
            cx,
        ))
        .child(report_kpi(
            "projects-kpi-blocked",
            DuckIcon::Pause,
            tr(cx, "阻塞中"),
            SharedString::from(count(TaskStatus::Blocked).to_string()),
            extended.colors.warning,
            SharedString::from(tr(cx, "均需外部输入才能推进")),
            cx,
        ))
        .child(report_kpi(
            "projects-kpi-overdue",
            DuckIcon::TriangleAlert,
            tr(cx, "逾期"),
            SharedString::from(overdue.len().to_string()),
            tokens.colors.destructive,
            SharedString::from(trf(cx, "最久逾期 {} 天", &[&max_overdue.to_string()])),
            cx,
        ));

    // 近 7 天完成趋势：一天一根柱，柱高按当周最大值归一。
    let series: Vec<(CalendarDate, usize)> = (0..7)
        .rev()
        .map(|back| {
            let day = today.add_days(-back);
            let done = tasks
                .iter()
                .filter(|task| {
                    task.status == TaskStatus::Done
                        && task.completed_at.map(local_date) == Some(day)
                })
                .count();
            (day, done)
        })
        .collect();
    let peak = series
        .iter()
        .map(|(_, done)| *done)
        .max()
        .unwrap_or(0)
        .max(1);
    let mut bars = h_flex()
        .flex_none()
        .w_full()
        .items_end()
        .gap(tokens.spacing.xs)
        .h(px(TREND_CHART_HEIGHT));
    for (day, done) in &series {
        let ratio = *done as f32 / peak as f32;
        bars = bars.child(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .justify_end()
                .gap(tokens.spacing.xxs)
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .flex()
                        .justify_center()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(tokens.colors.muted_foreground)
                        .child(SharedString::from(if *done == 0 {
                            String::new()
                        } else {
                            done.to_string()
                        })),
                )
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .h(px((ratio * (TREND_CHART_HEIGHT - 34.)).max(4.)))
                        .rounded_t(tokens.spacing.xxs)
                        .bg(tokens.colors.primary),
                )
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .flex()
                        .justify_center()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(tokens.colors.muted_foreground)
                        .child(SharedString::from(trf(
                            cx,
                            "{}/{}",
                            &[&day.month().to_string(), &day.day().to_string()],
                        ))),
                ),
        );
    }

    // 按项目分布：一行一个项目，进度条按「完成 / 总数」。
    let mut distribution = v_flex().w_full().flex_none().gap(tokens.spacing.xs);
    for project in state.projects() {
        let scope = state.project_counts(project.id);
        distribution = distribution.child(project_bar(
            SharedString::from(project.name.clone()),
            color_from_hex(&project.color).unwrap_or(tokens.colors.muted_foreground),
            scope.done,
            scope.total,
            cx,
        ));
    }
    let inbox: Vec<&Task> = tasks
        .iter()
        .filter(|task| task.project_id.is_none())
        .collect();
    distribution = distribution.child(project_bar(
        SharedString::from(tr(cx, "未归入项目")),
        tokens.colors.muted_foreground,
        inbox
            .iter()
            .filter(|task| task.status == TaskStatus::Done)
            .count() as u32,
        inbox.len() as u32,
        cx,
    ));

    // 逾期清单。
    let mut overdue_list = v_flex().w_full().flex_none().gap(tokens.spacing.xxs);
    if overdue.is_empty() {
        overdue_list = overdue_list.child(
            div()
                .w_full()
                .p(tokens.spacing.sm)
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(tr(cx, "当前没有逾期任务。"))),
        );
    } else {
        for task in &overdue {
            overdue_list = overdue_list.child(report_overdue_row(task, today, state, view, cx));
        }
    }

    div()
        .id("projects-report")
        .flex_1()
        .min_h_0()
        .min_w_0()
        .w_full()
        .overflow_y_scroll()
        .p(tokens.spacing.sm)
        .child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.sm)
                .child(kpis)
                .child(report_panel(
                    tr(cx, "近 7 天完成趋势"),
                    tr(cx, "口径：按完成日计"),
                    bars.into_any_element(),
                    cx,
                ))
                .child(report_panel(
                    tr(cx, "按项目分布"),
                    tr(cx, "完成 / 总数"),
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .child(distribution)
                        .child(
                            div()
                                .flex_none()
                                .text_size(tokens.typography.xs.size)
                                .line_height(tokens.typography.xs.line_height)
                                .text_color(tokens.colors.muted_foreground)
                                .child(SharedString::from(trf(
                                    cx,
                                    "平均进度（未完成项）{}%",
                                    &[&avg_progress.to_string()],
                                ))),
                        )
                        .into_any_element(),
                    cx,
                ))
                .child(report_panel(
                    tr(cx, "逾期清单"),
                    trf(cx, "{} 项", &[&overdue.len().to_string()]),
                    overdue_list.into_any_element(),
                    cx,
                )),
        )
        .into_any_element()
}

/// 趋势柱的高度（几何量，留裸值并写明来由：7 根柱 + 上下标签刚好排满一屏的三分之一）。
const TREND_CHART_HEIGHT: f32 = 150.;

/// 报表的一张 KPI 卡。
fn report_kpi(
    id: &'static str,
    icon: DuckIcon,
    label: &'static str,
    value: SharedString,
    value_color: Hsla,
    note: SharedString,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    v_flex()
        .id(id)
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.xxs)
        .p(tokens.spacing.sm)
        .rounded(theme.components().card_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.surface)
        .child(
            h_flex()
                .flex_none()
                .gap(tokens.spacing.xs)
                .text_color(tokens.colors.muted_foreground)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .child(Icon::new(icon).size(tokens.typography.xs.size))
                .child(SharedString::from(label)),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xl.size)
                .line_height(tokens.typography.xl.line_height)
                .text_color(value_color)
                .child(value),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(note),
        )
        .into_any_element()
}

/// 「进行中」那句附注（超 WIP 上限就点出来）。
fn doing_note(doing: u32, cx: &App) -> String {
    if doing > WIP_LIMIT {
        trf(
            cx,
            "WIP 上限 {} · 超 {} 个",
            &[&WIP_LIMIT.to_string(), &(doing - WIP_LIMIT).to_string()],
        )
    } else {
        trf(cx, "WIP 上限 {} · 未超限", &[&WIP_LIMIT.to_string()])
    }
}

/// 近 `days` 天内完成的任务数（按完成日算）。
fn completed_within(tasks: &[Task], today: CalendarDate, days: i64) -> usize {
    let from = today.add_days(-(days - 1));
    tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Done)
        .filter(|task| {
            task.completed_at
                .map(local_date)
                .is_some_and(|day| day >= from)
        })
        .count()
}

/// 报表的一张面板（标题 + 右上角口径 + 内容）。
fn report_panel(
    title: &'static str,
    hint: impl Into<SharedString>,
    body: AnyElement,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let hint = hint.into();
    v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .p(tokens.spacing.sm)
        .rounded(theme.components().card_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.surface)
        .child(
            h_flex()
                .flex_none()
                .w_full()
                .justify_between()
                .child(caption_heading(title, cx))
                .child(
                    div()
                        .flex_none()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(tokens.colors.muted_foreground)
                        .child(hint),
                ),
        )
        .child(body)
        .into_any_element()
}

/// 按项目分布的横向进度条（`完成 / 总数`）。
fn project_bar(name: SharedString, color: Hsla, done: u32, total: u32, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let ratio = if total == 0 {
        0.
    } else {
        done as f32 / total as f32
    };
    h_flex()
        .flex_none()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(
            h_flex()
                .flex_none()
                .w(px(REPORT_NAME_WIDTH))
                .min_w_0()
                .gap(tokens.spacing.xs)
                .child(
                    div()
                        .flex_none()
                        .size(tokens.spacing.xs)
                        .rounded_full()
                        .bg(color),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .text_color(tokens.colors.foreground)
                        .child(name),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h(tokens.spacing.xs)
                .rounded_full()
                .bg(theme.extended().colors.hairline)
                .overflow_hidden()
                .child(div().h_full().w(relative(ratio)).bg(color)),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(format!("{done} / {total}"))),
        )
        .into_any_element()
}

/// 按项目分布里项目名的宽度（几何量：容得下 8 个中文字符再省略）。
const REPORT_NAME_WIDTH: f32 = 160.;

/// 逾期清单里的一行。
fn report_overdue_row(
    task: &Task,
    today: CalendarDate,
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let id = task.id;
    let days = task
        .due_date
        .map_or(0, |due| today.days_until(due).unsigned_abs());
    let project_color = task
        .project_id
        .and_then(|project_id| state.projects().iter().find(|p| p.id == project_id))
        .and_then(|project| color_from_hex(&project.color))
        .unwrap_or(tokens.colors.muted_foreground);

    h_flex()
        .id(SharedString::from(format!("projects-overdue-{id}")))
        .flex_none()
        .w_full()
        .gap(tokens.spacing.xs)
        .px(tokens.spacing.xs)
        .py(tokens.spacing.xxs)
        .rounded(tokens.spacing.xxs)
        .hover(move |style| style.bg(extended.colors.hover_tint))
        .role(Role::Button)
        .aria_label(SharedString::from(task.title.clone()))
        .cursor_pointer()
        .child(priority_dot(task.priority, cx))
        .child(
            div()
                .flex_none()
                .size(tokens.spacing.xs)
                .rounded_full()
                .bg(project_color),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(task.title.clone())),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.destructive)
                .child(SharedString::from(trf(
                    cx,
                    "逾期 {} 天",
                    &[&days.to_string()],
                ))),
        )
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.select_task(Some(id), cx));
            }
        })
        .into_any_element()
}

/// 工作日志视图：左清单 + 右正文，三档 tab。
fn render_log(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let kind = state.log_kind();
    let rows = state.logs_of_kind(kind);
    let selected = state.selected_log();

    let mut list = v_flex().w_full().flex_none().gap(tokens.spacing.xxs);
    if rows.is_empty() {
        list = list.child(
            div()
                .w_full()
                .p(tokens.spacing.sm)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(trf(
                    cx,
                    "最近 30 天还没有{}。",
                    &[super::dialogs::log_kind_label(kind, cx)],
                ))),
        );
    } else {
        for log in &rows {
            let id = log.id;
            let active = selected.is_some_and(|current| current.id == id);
            list = list.child(
                div()
                    .id(SharedString::from(format!("projects-log-row-{id}")))
                    .w_full()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .rounded(tokens.spacing.xs)
                    .cursor_pointer()
                    .text_size(tokens.typography.sm.size)
                    .line_height(tokens.typography.sm.line_height)
                    .text_color(if active {
                        tokens.colors.primary
                    } else {
                        tokens.colors.foreground
                    })
                    .when(active, |this| this.bg(extended.colors.hover_tint))
                    .role(Role::Button)
                    .aria_label(SharedString::from(log.date.to_iso()))
                    .hover(move |style| style.bg(extended.colors.hover_tint))
                    .child(SharedString::from(log.date.to_iso()))
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |page, cx| page.select_log(id, cx));
                        }
                    }),
            );
        }
    }

    let body = match selected {
        Some(log) if !log.content_md.trim().is_empty() => div()
            .id("projects-log-body")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .text_size(tokens.typography.sm.size)
            .line_height(tokens.typography.sm.line_height)
            .text_color(tokens.colors.foreground)
            .child(SharedString::from(log.content_md.clone()))
            .into_any_element(),
        Some(_) => div()
            .flex_1()
            .min_w_0()
            .text_size(tokens.typography.sm.size)
            .line_height(tokens.typography.sm.line_height)
            .text_color(tokens.colors.muted_foreground)
            .child(SharedString::from(tr(
                cx,
                "这篇还是空的 —— 点「编辑」写点什么。",
            )))
            .into_any_element(),
        None => div()
            .flex_1()
            .min_w_0()
            .text_size(tokens.typography.sm.size)
            .line_height(tokens.typography.sm.line_height)
            .text_color(tokens.colors.muted_foreground)
            .child(SharedString::from(tr(
                cx,
                "左侧选一篇，或点「新建」写下今天的事。",
            )))
            .into_any_element(),
    };

    v_flex()
        .size_full()
        .min_h_0()
        .min_w_0()
        .child(log_header(kind, selected.map(|log| log.date), view, cx))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .w_full()
                .flex()
                .flex_row()
                .child(
                    div()
                        .id("projects-log-list")
                        .flex_none()
                        .w(px(LOG_LIST_WIDTH))
                        .h_full()
                        .overflow_y_scroll()
                        .p(tokens.spacing.sm)
                        .border_r(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .child(list),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .p(tokens.spacing.sm)
                        .child(body),
                ),
        )
        .into_any_element()
}

/// 工作日志左侧清单的宽度。
const LOG_LIST_WIDTH: f32 = 200.;

/// 工作日志工具条：三档 tab + 日期 + 右动作。
fn log_header(
    kind: LogKind,
    date: Option<CalendarDate>,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let items: Vec<SegmentItem> = LogKind::ALL
        .iter()
        .map(|kind| {
            SegmentItem::new(SharedString::from(super::dialogs::log_kind_label(
                *kind, cx,
            )))
        })
        .collect();
    let selected = LogKind::ALL.iter().position(|k| *k == kind).unwrap_or(0);

    let tabs = segmented_tabs_with_icons(
        "projects-log-tabs",
        items,
        selected,
        {
            let view = view.clone();
            move |index, _window, cx| {
                let Some(kind) = LogKind::ALL.get(index).copied() else {
                    return;
                };
                let _ = view.update(cx, |page, cx| page.set_log_kind(kind, cx));
            }
        },
        cx,
    );

    h_flex()
        .flex_none()
        .w_full()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(tabs)
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(
                    date.map_or_else(String::new, |day| day.to_iso()),
                )),
        )
        .child(div().flex_1().min_w_0())
        .child({
            let view = view.clone();
            button(
                "projects-log-edit",
                tr(cx, "编辑"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, window, cx| {
                let _ = view.update(cx, |page, cx| page.open_today_log(window, cx));
            })
        })
        .into_any_element()
}

/// 右栏。
pub(crate) fn render_right_sidebar(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    // 归档中心的选中行是「一条归档记录」，与看板卡片不是一回事（见 `archive.rs`）。
    if state.view().is_archive() {
        return super::archive::render_archive_right_sidebar(state, view, cx);
    }
    let tokens = active_theme(cx).tokens();
    let Some(task) = state.selected_task() else {
        return v_flex()
            .size_full()
            .id("projects-right-sidebar")
            .items_center()
            .justify_center()
            .p(tokens.spacing.lg)
            .child(meta_text(tr(cx, "选中一张卡片查看详情。"), cx))
            .into_any_element();
    };
    // 右栏要用「项目表」与「这条任务的里程碑」：两样都在状态层之外查不到（项目表在
    // `state` 里、里程碑要走库），所以在这里取好交给组件，组件自己不碰状态。
    let milestones = state.task_milestones(task.id);
    task_detail_panel(task, state.projects(), &milestones, state.today(), view, cx)
}
