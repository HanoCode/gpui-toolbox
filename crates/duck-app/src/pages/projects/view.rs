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
    segmented_tabs, segmented_tabs_with_icons, selectable_text, selectable_text_at,
    sidebar_navigation_button, tabular_numbers, toolbar_text_dropdown_button,
};
use duck_project::{
    CalendarDate, LogKind, Priority, Project, ProjectStatus, Quadrant, Tag, Task, TaskSort,
    TaskStatus, Weekday, WeeklyPlanStatus,
};
use duck_theme::{active_theme, color_from_hex, tr, trf, trn};

use super::ProjectsPage;
use super::archive::render_archive_center;
use super::prefs::{CalendarMode, GroupMode, ReportRange, ViewMode};
use super::report::{self, ReportStatus};
use super::state::{
    BoardScope, LOG_HINT_THRESHOLD, ProjectsState, SidebarCounts, SmartFilter, local_date,
};
use super::widgets::calendar_menu::calendar_menu;
use super::widgets::kanban_column::kanban_column;
use super::widgets::priority_dot::{priority_color, priority_dot, priority_label};
use super::widgets::progress_bar::task_progress;
use super::widgets::quadrant_badge::quadrant_badge;
use super::widgets::status_label;
use super::widgets::tag_chip::tag_chip;
use super::widgets::task_detail_panel::task_detail_panel;
use super::widgets::task_menu::task_menu;
use super::widgets::{due_label, is_double_click, weekday_label};

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

/// 筛选面板里每组标题（「四象限」「优先级」「标签」）占的固定宽度。
///
/// 固定而不是按文字自适应：三组标题字数不同，自适应会让三行的 chips 起跑线错开
/// 一格，「同一个面板里的三行」看着像三张表。
const FILTER_GROUP_WIDTH: f32 = 48.;

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
        //
        // 右键菜单挂外层 `div`（同 [`project_row`]）：收集箱是「没有项目」时的唯一
        // 落点，新建任务本就是它的高频动作。
        let selected = state.scope() == BoardScope::Inbox;
        nav = nav.child(
            div()
                .id("projects-inbox-row")
                .w_full()
                .context_menu({
                    let view = view.clone();
                    move |menu, _, cx| {
                        menu.item(PopupMenuItem::label(tr(cx, "未归入项目")))
                            .item(
                                PopupMenuItem::new(tr(cx, "只看收集箱"))
                                    .icon(Icon::new(DuckIcon::Inbox))
                                    .on_click({
                                        let view = view.clone();
                                        move |_, _, cx| {
                                            let _ = view.update(cx, |page, cx| {
                                                page.set_scope(BoardScope::Inbox, cx)
                                            });
                                        }
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(tr(cx, "新建任务（进收集箱）"))
                                    .icon(Icon::new(DuckIcon::Plus))
                                    .on_click({
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |page, cx| {
                                                page.open_spotlight_in(None, window, cx)
                                            });
                                        }
                                    }),
                            )
                    }
                })
                .child(indent(
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
                            let _ =
                                view.update(cx, |page, cx| page.set_scope(BoardScope::Inbox, cx));
                        }
                    }),
                )),
        );
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
                    .child(selectable_text(SharedString::from(value.to_string()))),
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
        // 右键：新建项目（与段标题那颗「+」同一入口）+ 展开 / 收起。左栏的行都该
        // 右键有反应 —— 这里是唯一没有项目行兜底的「容器行」，断了习惯的就是它。
        .context_menu({
            let view = view.clone();
            move |menu, _, cx| {
                menu.item(
                    PopupMenuItem::new(tr(cx, "新建项目"))
                        .icon(Icon::new(DuckIcon::Plus))
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |page, cx| page.create_project(cx));
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new(tr(cx, if collapsed { "展开" } else { "收起" }))
                        .icon(Icon::new(if collapsed {
                            DuckIcon::ChevronDown
                        } else {
                            DuckIcon::ChevronRight
                        }))
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ =
                                    view.update(cx, |page, cx| page.toggle_projects_collapsed(cx));
                            }
                        }),
                )
            }
        })
        .into_any_element()
}

/// 左栏一行标签：色点 + 名称 + 用到它的任务数。点一下按它筛，再点一下取消。
///
/// 右键菜单挂在外层 `div` 上（同 [`project_row`] 的取舍：`Button` 没有
/// `context_menu`）—— 标签的收藏开关目前只住在「管理标签」弹窗里，右键给了
/// 一步直达，不必为拨一个开关开整只弹窗。
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

    div()
        .id(SharedString::from(format!("projects-tag-row-{}", tag.id)))
        .w_full()
        .context_menu({
            let view = view.clone();
            let id = tag.id;
            let name = SharedString::from(tag.name.clone());
            let favorite = tag.is_favorite;
            move |menu, _, cx| {
                // 标题行（同项目菜单）：右键弹出的菜单不自带方向，写明点的是哪一个。
                menu.item(PopupMenuItem::label(trf(cx, "标签「{}」", &[&name])))
                    .item(
                        PopupMenuItem::new(tr(
                            cx,
                            if selected {
                                "取消筛选"
                            } else {
                                "只看这个标签"
                            },
                        ))
                        .icon(Icon::new(DuckIcon::ListFilter))
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |page, cx| page.toggle_tag(id, cx));
                            }
                        }),
                    )
                    .item(
                        PopupMenuItem::new(tr(cx, if favorite { "取消收藏" } else { "收藏" }))
                            .icon(Icon::new(DuckIcon::Star))
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    let _ = view.update(cx, |page, cx| {
                                        page.set_tag_favorite(id, !favorite, cx)
                                    });
                                }
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new(tr(cx, "管理标签…"))
                            .icon(Icon::new(DuckIcon::Tag))
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view
                                        .update(cx, |page, cx| page.open_tag_manager(window, cx));
                                }
                            }),
                    )
            }
        })
        .child(
            sidebar_navigation_button(
                SharedString::from(format!("projects-tag-{}", tag.id)),
                SharedString::from(tag.name.clone()),
                // 收藏的标签在色点右侧给一颗小星（顺序本身就是内核的「收藏在前」，
                // 星标只是让「为什么它排前面」一眼可读）。
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        div()
                            .size(extended.icon.md)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().size(extended.icon.md / 2.).rounded_full().bg(color)),
                    )
                    .when(tag.is_favorite, |row| {
                        row.child(
                            Icon::new(DuckIcon::Star)
                                .size(extended.icon.sm)
                                .text_color(theme.extended().colors.warning),
                        )
                    }),
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
            }),
        )
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
                .child(selectable_text(SharedString::from(
                    counts.open().to_string(),
                ))),
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
        .child(selectable_text(SharedString::from(count.to_string())))
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
        // 筛选面板：点工具条「筛选」才出现，展开时压在列表上方（不遮挡数据）。
        .children(
            state
                .filters_open()
                .then(|| render_filter_panel(state, view, cx)),
        )
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
        .child(render_status_bar(state, view, cx))
        .into_any_element()
}

/// 工具条：视图切换 + 范围 chips +（筛选 / 排序 / 分组 / 显示已完成）+ 导入导出 + 两个新建按钮。
///
/// 与「筛选面板」分成两行是**刻意的**：一行塞不下（5 个 chip + 6 档视图 +
/// 6 档排序 + 4 档分组 + 4 个动作），而按宽度硬挤会让窄窗口下的控件互相截断。
/// 两行各司其职 —— 这一行管「怎么看」，下面那行（[`render_filter_panel`]，点「筛选」
/// 才出现）管「看哪些」。
///
/// 排法与原型 `renderToolbar`（`docs/projects-mock.html:1131`）逐段对应：
/// 左侧 seg + 范围 chips，右侧一条 `.right` 段放筛选 / 排序 / 分组 / 显示已完成 /
/// 分隔 / 导入 / 导出 / 快速新建 / 新建任务。
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
        .child(filter_button(state, view, cx))
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

/// 工具条上的「筛选」按钮：点一下展开 / 收起下面那条筛选面板。
///
/// 文案里带生效条数（`筛选 · 2`）而不是另挂一枚圆形徽标：工具条是一排 chip，
/// 中间突然冒出一个 16px 的圆点会被读成「有人在等我看消息」。条数为 0 时只写
/// 「筛选」，与旁边几个 chip 的「一个词」节奏一致。
fn filter_button(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let extended = theme.extended();
    let count = state.filter_count();
    let open = state.filters_open();
    let label = if count == 0 {
        SharedString::from(tr(cx, "筛选"))
    } else {
        SharedString::from(trf(cx, "筛选 · {}", &[&count.to_string()]))
    };
    let icon = Icon::new(DuckIcon::ListFilter)
        .size(extended.icon.sm)
        .text_color(if open || count > 0 {
            theme.tokens().colors.primary
        } else {
            theme.tokens().colors.muted_foreground
        });
    chip(
        "projects-filter",
        Some(icon.into_any_element()),
        label,
        open || count > 0,
        cx,
    )
    .on_click({
        let view = view.clone();
        move |_, _window, cx| {
            let _ = view.update(cx, |page, cx| page.toggle_filters(cx));
        }
    })
    .into_any_element()
}

/// 筛选面板：四象限 / 优先级 / 标签三组 chips + 重置。
///
/// 做成一整条**常驻在工具条下面**的行（而不是浮在按钮上的弹层），理由是这里的
/// 三组都是**多选**：浮层点一下就得收起来，勾三个条件要开三次；摊成一行则每次
/// 勾选都看得见「现在一共勾了几个」，也不会有「弹层盖住列表」的问题。
///
/// 三组之间是 AND、组内是 OR（口径写在 `ProjectsState::query` 里，与报表筛选条
/// 同一套直觉）。所以「勾了 Q1 和 Q2、又勾了 P0」= `(Q1|Q2) 且 P0`。
fn render_filter_panel(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let quadrants = state.filter_quadrants();
    let priorities = state.filter_priorities();
    let tags = state.filter_tags();

    let mut groups = v_flex().w_full().gap(tokens.spacing.xs);

    // 四象限
    let mut quadrant_row = h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(tokens.spacing.xs);
    quadrant_row = quadrant_row.child(filter_group_label(tr(cx, "四象限"), cx));
    for quadrant in Quadrant::ALL {
        let selected = quadrants.contains(&quadrant);
        quadrant_row = quadrant_row.child(
            chip(
                SharedString::from(format!("projects-filter-{}", quadrant.as_str())),
                Some(quadrant_badge(quadrant, cx)),
                SharedString::from(filter_quadrant_label(quadrant, cx)),
                selected,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.toggle_filter_quadrant(quadrant, cx));
                }
            }),
        );
    }

    // 优先级
    let mut priority_row = h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(tokens.spacing.xs);
    priority_row = priority_row.child(filter_group_label(tr(cx, "优先级"), cx));
    for priority in Priority::ALL {
        let selected = priorities.contains(&priority);
        priority_row = priority_row.child(
            chip(
                SharedString::from(format!("projects-filter-p{}", priority.as_i64())),
                Some(priority_dot(priority, cx)),
                SharedString::from(priority_label(priority, cx)),
                selected,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.toggle_filter_priority(priority, cx));
                }
            }),
        );
    }

    groups = groups.child(quadrant_row).child(priority_row);

    // 标签：库里一个都没建时整组不出现（空的「标签」标题只会让人以为加载失败）
    if !state.tags().is_empty() {
        let mut tag_row = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap(tokens.spacing.xs);
        tag_row = tag_row.child(filter_group_label(tr(cx, "标签"), cx));
        for tag in state.tags() {
            let selected = tags.contains(&tag.id);
            let dot = color_from_hex(&tag.color).unwrap_or(tokens.colors.muted_foreground);
            tag_row = tag_row.child(
                chip(
                    SharedString::from(format!("projects-filter-tag-{}", tag.id)),
                    Some(
                        div()
                            .flex_none()
                            .size(extended.icon.sm / 2.)
                            .rounded_full()
                            .bg(dot)
                            .into_any_element(),
                    ),
                    SharedString::from(tag.name.clone()),
                    selected,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    let id = tag.id;
                    move |_, _window, cx| {
                        let _ = view.update(cx, |page, cx| page.toggle_filter_tag(id, cx));
                    }
                }),
            );
        }
        groups = groups.child(tag_row);
    }

    h_flex()
        .flex_none()
        .w_full()
        .items_start()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.muted)
        .child(div().flex_1().min_w_0().child(groups))
        // 「重置」只在真有条件时出现：平时摆一个点了没反应的按钮，比不摆更糟。
        .when(state.filter_count() > 0, |row| {
            row.child(
                chip(
                    "projects-filter-reset",
                    None,
                    SharedString::from(tr(cx, "重置")),
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _window, cx| {
                        let _ = view.update(cx, |page, cx| page.clear_filters(cx));
                    }
                }),
            )
        })
        .into_any_element()
}

/// 筛选面板里每组前面的小标题（「四象限」「优先级」「标签」）。
fn filter_group_label(label: &'static str, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    div()
        .flex_none()
        .w(px(FILTER_GROUP_WIDTH))
        .text_size(theme.tokens().typography.xs.size)
        .line_height(theme.tokens().typography.xs.line_height)
        .text_color(theme.extended().colors.text_tertiary)
        .child(SharedString::from(label))
        .into_any_element()
}

/// 四象限的短名（筛选 chips 用）。
///
/// 与 [`quadrant_label`] 的差别只是**不给处置建议**：chips 一行要摆四个，
/// 「Q1 重要且紧急 · 立即处理」这种长度会把行撑爆；建议留在详情面板里说。
fn filter_quadrant_label(quadrant: Quadrant, cx: &App) -> &'static str {
    match quadrant {
        Quadrant::Q1 => tr(cx, "重要且紧急"),
        Quadrant::Q2 => tr(cx, "重要不紧急"),
        Quadrant::Q3 => tr(cx, "紧急不重要"),
        Quadrant::Q4 => tr(cx, "都不紧急"),
    }
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
pub(super) fn chip(
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

/// 主区空态：按「有没有在筛」分四种说法。
///
/// 一律说「这里还没有任务」会让人以为数据丢了 —— 明明只是筛选条件太窄（或搜索词
/// 打错一个字）。说清是哪一种，用户才知道该清哪个框。看板与列表共用。
///
/// 第四种（工具条筛选面板勾太窄）**带一个退路按钮**：前面三种的退路都在别处
/// （清搜索框、再点一下左栏），而面板里的条件在别的行上，空态里不给按钮的话
/// 用户得先想起来「我刚才在工具条点过筛选」。
fn main_empty_state(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
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
    if state.filter_count() > 0 {
        let clear = {
            let view = view.clone();
            icon_text_button(
                "projects-empty-clear-filters",
                tr(cx, "清空筛选"),
                Icon::new(DuckIcon::ListFilter).size(active_theme(cx).extended().icon.sm),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, _window, cx| {
                let _ = view.update(cx, |page, cx| page.clear_filters(cx));
            })
            .into_any_element()
        };
        return empty_state(
            DuckIcon::ListFilter,
            trf(
                cx,
                "没有同时满足这 {} 个条件的任务",
                &[&state.filter_count().to_string()],
            ),
            Some(SharedString::from(tr(
                cx,
                "工具条的「筛选」里勾得太多，清掉几项再看看。",
            ))),
            Some(clear),
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
        return main_empty_state(state, view, cx);
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
pub(crate) fn check_box(
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
        .child(selectable_text(SharedString::from(trf(
            cx,
            "{} 项已选中",
            &[&count.to_string()],
        ))))
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
                        super::dialogs::open_task_editor(
                            page,
                            Some(id),
                            None,
                            status,
                            None,
                            window,
                            cx,
                        )
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
                        .child(selectable_text(SharedString::from(task.title.clone()))),
                ),
        )
        // 项目名。
        .child(
            cell(LIST_PROJECT_WIDTH)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(project.map_or_else(
                    || SharedString::from("—"),
                    |project| SharedString::from(project.name.clone()),
                ))),
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
                .child(selectable_text(text))
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
        return main_empty_state(state, view, cx);
    }

    let today = state.today();
    let selected = state.selected();
    // 批量勾选：看板与列表共用同一份（`checked_ids` 是两个视图的公共事实）。
    let checked = state.checked_ids();
    // 卡片上的项目色点要项目表：这一层是取数的地方（同右栏），组件自己不碰状态。
    let projects = state.projects();
    // 搁置卡没有自己的列（`DEFAULT_BOARD` 只有四态），挂在待办列底部的一段里。
    // 这里取一次给所有列：只有待办列会用，别的列传进去也是同一条空切片。
    let deferred = state.deferred_tasks();
    let deferred_open = state.deferred_open();
    let columns: Vec<AnyElement> = state
        .columns()
        .iter()
        .map(|column| {
            let tasks = state.tasks_in_status(column.status_key);
            kanban_column(
                column,
                &tasks,
                &deferred,
                deferred_open,
                projects,
                today,
                selected,
                &checked,
                view,
                cx,
            )
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
fn render_status_bar(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
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
        .child(selectable_text(SharedString::from(trf(
            cx,
            "范围：{}",
            &[scope.as_str()],
        ))))
        .child(selectable_text(SharedString::from(trf(
            cx,
            "本视图 {} 条",
            &[&state.task_total().to_string()],
        ))))
        .child(selectable_text(SharedString::from(trf(
            cx,
            "共 {}",
            &[&counts.all.to_string()],
        ))))
        .child(selectable_text(SharedString::from(trf(
            cx,
            "完成 {}",
            &[&counts.done.to_string()],
        ))))
        .child(
            div()
                .flex_none()
                .text_color(tokens.colors.destructive)
                .child(selectable_text(SharedString::from(trf(
                    cx,
                    "逾期 {}",
                    &[&counts.overdue.to_string()],
                )))),
        );

    let term = state.search().trim();
    if !term.is_empty() {
        row = row.child(selectable_text(SharedString::from(trf(
            cx,
            "搜索：{}",
            &[term],
        ))));
    }

    // 日报轻提示：今天做完 3 条以上、今天还没写日报时，在状态栏里点一句。
    //
    // 刻意**不做成弹窗/通知**：它要在用户刚完成第 3 条、注意力还在任务上的那一刻出现，
    // 但绝不能打断（不抢焦点、不遮挡、不消失倒计时）。状态栏是唯一「一直看得见又
    // 永远不会挡住东西」的位置。点它就开今天那篇日报。
    if state.done_today() >= LOG_HINT_THRESHOLD && !state.has_log_today() {
        let view = view.clone();
        let today = state.today();
        let label = SharedString::from(trf(
            cx,
            "今天已完成 {} 条 · 写日报",
            &[&state.done_today().to_string()],
        ));
        row = row.child(
            chip(
                "projects-status-log-hint",
                Some(
                    Icon::new(DuckIcon::FileText)
                        .size(extended.icon.sm)
                        .into_any_element(),
                ),
                label,
                false,
                cx,
            )
            .on_click(move |_, window, cx| {
                let _ = view.update(cx, |page, cx| {
                    page.open_log_editor(today, LogKind::Daily, window, cx)
                });
            }),
        );
    }

    row.child(div().flex_1().min_w_0())
        .child(
            div()
                .flex_none()
                .text_color(extended.colors.text_tertiary)
                .child(SharedString::from(tr(
                    cx,
                    "N 快速新建 · ⌘N 新建任务 · / 搜索 · F 筛选 · ⌘K 命令面板 · 右键卡片有菜单",
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

/// 日历月网格恒为 6 行 × 7 列（42 格）。
///
/// 行数随月份变（4~6 行）会让每翻一页网格高度就跳一次，鼠标下的格子会挪位；
/// 恒 6 行则「下个月」永远在同一个位置。
const CALENDAR_CELLS: i64 = 42;
/// 周网格：一行 7 格（与 [`CALENDAR_CELLS`] 同族，两者一起决定网格形状）。
const CALENDAR_WEEK_CELLS: i64 = 7;
/// 日历格高（不属间距阶梯、也不是字号，故留裸值）：容得下日号 + 两行任务。
const CALENDAR_CELL_HEIGHT: f32 = 92.;
/// 月视图每格最多列几条任务（多出来的收成 `+N 更多`）。
const CALENDAR_MONTH_DETAIL: usize = 2;
/// 周视图每格最多列几条 —— 格子撑满整块高度，放得下一天的安排。
const CALENDAR_WEEK_DETAIL: usize = 6;

/// 日历两种画法的几何与容量。
///
/// 收成一个结构体而不是在渲染里到处 `if mode == Week`：格数 / 高度 / 每格容量必须
/// **一起**变，分开写迟早出现「周视图按 42 格算」或「周视图只列 2 条」这种半改。
#[derive(Clone, Copy)]
struct CalendarLayout {
    /// 画法本身（格子要按画法决定「跨月是否淡化」这类细节）。
    mode: CalendarMode,
    /// 网格格数（月 42 = 6×7；周 7）。
    cells: i64,
    /// 每格最多列几条任务。
    detail: usize,
    /// 每格固定高度；`None` = 高度由所在行决定（周视图一行撑满的看板式布局）。
    height: Option<f32>,
}

impl CalendarLayout {
    const fn of(mode: CalendarMode) -> Self {
        match mode {
            CalendarMode::Month => Self {
                mode,
                cells: CALENDAR_CELLS,
                detail: CALENDAR_MONTH_DETAIL,
                height: Some(CALENDAR_CELL_HEIGHT),
            },
            CalendarMode::Week => Self {
                mode,
                cells: CALENDAR_WEEK_CELLS,
                detail: CALENDAR_WEEK_DETAIL,
                height: None,
            },
        }
    }
}

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

/// 日历视图：月网格或周网格（按截止日归位，已完成按完成日归位）。
///
/// 两种画法共用**同一个锚**（`state.calendar_anchor`）与同一份格子渲染：差别只在格数、
/// 高度、每格容量（[`CalendarLayout`]）与首行表头（月视图是星期、周视图是「星期 + 日期」）。
fn render_calendar(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let today = state.today();
    let anchor = state.calendar_anchor().unwrap_or(today);
    let layout = CalendarLayout::of(state.calendar_mode());
    // 月视图从「当月 1 号」起画（那一格的日号才是 1，右端跨月格才跟着对）；
    // 周视图直接从锚定日所在那一周的周一起画 —— 它没有「月初」这个概念。
    let first = match layout.mode {
        CalendarMode::Month => {
            let Some(first) = CalendarDate::new(anchor.year(), anchor.month(), 1) else {
                return main_empty_state(state, view, cx);
            };
            first
        }
        CalendarMode::Week => anchor,
    };
    let grid_start = first.start_of_week();
    let tasks = state.tasks();

    // 周视图只有一行，让它连带格子一起撑满这整块高度：格子够高才装得下一天的安排。
    let stretch = layout.height.is_none();
    let mut grid = if stretch {
        v_flex().w_full().flex_1().min_h_0()
    } else {
        v_flex().w_full().flex_none()
    };
    for row in 0..(layout.cells / 7) {
        let mut line = div().flex().flex_row().items_stretch().w_full();
        line = if stretch {
            line.flex_1().min_h_0()
        } else {
            line.flex_none()
        };
        for column in 0..7 {
            let day = grid_start.add_days(row * 7 + column);
            line = line.child(calendar_cell(day, anchor, today, tasks, layout, view, cx));
        }
        grid = grid.child(line);
    }

    let weekday_head = match layout.mode {
        CalendarMode::Month => weekday_row(cx),
        CalendarMode::Week => week_header_row(grid_start, today, cx),
    };

    v_flex()
        .size_full()
        .min_h_0()
        .min_w_0()
        .child(calendar_header(anchor, layout.mode, view, cx))
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
                        .child(weekday_head)
                        .child(grid),
                ),
        )
        .into_any_element()
}

/// 日历工具条：翻页 + 回到今天 + 月/周切换 + 标题 + 口径说明。
///
/// 箭头与「今天」的文案随画法变（「上个月」/「上一周」）：同一个位置的动作在两种画法下
/// 翻的量不一样，文案跟着说清楚比让人试一次要省事。
fn calendar_header(
    anchor: CalendarDate,
    mode: CalendarMode,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let icon_size = extended.icon.sm;
    // 月视图给「2026 年 10 月」，周视图给这一周的起止日 —— 周视图下只写月份，
    // 用户没法知道现在翻到哪一周了。
    let title = match mode {
        CalendarMode::Month => SharedString::from(trf(
            cx,
            "{} 年 {} 月",
            &[&anchor.year().to_string(), &anchor.month().to_string()],
        )),
        CalendarMode::Week => week_range_label(anchor.start_of_week(), cx),
    };
    let (prev_label, next_label) = match mode {
        CalendarMode::Month => (tr(cx, "上个月"), tr(cx, "下个月")),
        CalendarMode::Week => (tr(cx, "上一周"), tr(cx, "下一周")),
    };

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
            let _ = view.update(cx, |page, cx| page.step_calendar(Some(delta), cx));
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
            prev_label,
            DuckIcon::ChevronLeft,
            -1,
            cx,
        ))
        .child(step(
            "projects-calendar-next",
            next_label,
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
                let _ = view.update(cx, |page, cx| page.step_calendar(None, cx));
            })
        })
        .child(calendar_mode_switcher(mode, view, cx))
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(selectable_text(title)),
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
        // 独立日历浮窗的入口：把这块月历「摘出去」摆在桌面上（⌥⌘C 也能唤起）。
        .child({
            icon_button(
                "projects-calendar-float",
                tr(cx, "日历浮窗"),
                Icon::new(DuckIcon::ExternalLink).size(icon_size),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click(move |_, _, cx| {
                // 只动 shell 层的窗口注册表，不碰页面状态 —— 不必经页面实体转手。
                crate::app::open_calendar_float(cx);
            })
        })
        .into_any_element()
}

/// 星期行（周一起，与内核的 [`Weekday`] 同序）——月视图的列头。
///
/// 文案走 [`weekday_label`]（即 `tr`）：这几格是**界面文案**，英文界面下该显示
/// `Mon` 而不是「一」。
fn weekday_row(cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .flex()
        .flex_row()
        .w_full()
        .flex_none()
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .children(Weekday::ALL.into_iter().map(|weekday| {
            div()
                .flex_1()
                .min_w_0()
                .py(tokens.spacing.xxs)
                .flex()
                .justify_center()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if weekday.is_weekend() {
                    extended.colors.text_tertiary
                } else {
                    tokens.colors.muted_foreground
                })
                .child(SharedString::from(weekday_label(weekday, cx)))
        }))
        .into_any_element()
}

/// 周视图的列头：星期 + 日期（设计文档 §5.3「7 列看板式，列头为星期 + 日期」）。
///
/// 周视图的格子里**不画日号**（见 `calendar_cell`）：日期已经在列头上，格子里再写一遍
/// 等于同一行字出现两次。今天的列头用强调色，与月视图里今天那格的强调色是同一套语义。
fn week_header_row(grid_start: CalendarDate, today: CalendarDate, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .flex()
        .flex_row()
        .w_full()
        .flex_none()
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .children((0..7).map(|offset| {
            let day = grid_start.add_days(offset);
            let is_today = day == today;
            let weekday = weekday_label(day.weekday(), cx);
            div()
                .flex_1()
                .min_w_0()
                .py(tokens.spacing.xxs)
                .flex()
                .flex_col()
                .items_center()
                .gap(tokens.spacing.xxs)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if is_today {
                    tokens.colors.primary
                } else if day.weekday().is_weekend() {
                    extended.colors.text_tertiary
                } else {
                    tokens.colors.muted_foreground
                })
                .child(SharedString::from(weekday))
                .child(
                    div()
                        .text_size(tokens.typography.sm.size)
                        .child(SharedString::from(trf(
                            cx,
                            "{} 月 {} 日",
                            &[&day.month().to_string(), &day.day().to_string()],
                        ))),
                )
        }))
        .into_any_element()
}

/// 周视图标题：这一周的起止日。
///
/// 跨年那一周（比如 12 月 29 日 – 1 月 4 日）把结束日的年份也写出来 —— 只写
/// 「12 月 29 日 – 1 月 4 日」在跨年时读起来像倒着走。
fn week_range_label(start: CalendarDate, cx: &App) -> SharedString {
    let end = start.add_days(6);
    let label = if start.year() == end.year() {
        trf(
            cx,
            "{} 年 {} 月 {} 日 – {} 月 {} 日",
            &[
                &start.year().to_string(),
                &start.month().to_string(),
                &start.day().to_string(),
                &end.month().to_string(),
                &end.day().to_string(),
            ],
        )
    } else {
        trf(
            cx,
            "{} 年 {} 月 {} 日 – {} 年 {} 月 {} 日",
            &[
                &start.year().to_string(),
                &start.month().to_string(),
                &start.day().to_string(),
                &end.year().to_string(),
                &end.month().to_string(),
                &end.day().to_string(),
            ],
        )
    };
    SharedString::from(label)
}

/// 日历画法切换（月 / 周）。
///
/// 与工具条上的视图切换同一个组件（`segmented_tabs`）：两处都是「同一批数据的两种画法」，
/// 外观与交互不该长得不一样。
fn calendar_mode_switcher(
    mode: CalendarMode,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let labels: Vec<SharedString> = CalendarMode::ALL
        .iter()
        .map(|candidate| SharedString::from(calendar_mode_label(*candidate, cx)))
        .collect();
    let selected = CalendarMode::ALL
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0);
    // 与 `report_range_switcher` 同理：闭包要求 `'static`，先把引用克隆成拥有所有权的实体。
    let view = view.clone();
    segmented_tabs(
        "projects-calendar-mode",
        labels,
        selected,
        move |index, _window, cx| {
            let Some(mode) = CalendarMode::ALL.get(index).copied() else {
                return;
            };
            let _ = view.update(cx, |page, cx| page.set_calendar_mode(mode, cx));
        },
        cx,
    )
    .flex_none()
    .into_any_element()
}

/// 画法名（切换条与日期格菜单共用一份）。
fn calendar_mode_label(mode: CalendarMode, cx: &App) -> &'static str {
    match mode {
        CalendarMode::Month => tr(cx, "月视图"),
        CalendarMode::Week => tr(cx, "周视图"),
    }
}

/// 一个日历格：日号（仅月视图）+ 最多 N 条任务 + 「+N 更多」。
///
/// # ⚠️ 一格里的两个右键菜单为什么不直接叠在同一个元素上
///
/// 格子上要同时有两个右键菜单：落在**任务条**上是 [`task_menu`]（同一条任务，换个视图
/// 不该换一套动作），落在**空白处**是这一天的 [`calendar_menu`]（新建 / 看日志 / 换画法）。
/// 但 `ContextMenuExt::context_menu` 的触发是每个元素各挂一份**气泡阶段**的窗口监听，
/// 判据为 `hitbox.is_hovered()` —— 而该判据默认把鼠标下**所有**命中框都算作 hovered
/// （`Window::hit_test` 只在遇到 `HitboxBehavior::BlockMouse` 时才截断），并且
/// `ContextMenu` 自己的命中框是**画完子元素之后**才插入的（= 在子元素前面）。
/// 于是「外层日期格 + 内层任务条」两个触发会**同时命中**，两个菜单一起弹出来。
///
/// 所以日期菜单挂在**一层铺满格子的绝对定位背景**上（[`day_backdrop`]），任务条自己
/// `.occlude()`（= `BlockMouse`）：任务条盖住的那块地方会把背景的命中框截断，日期菜单
/// 不跟着弹；空白处则只有背景命中。`duck-components` 的列宽拖拽手柄
/// （`data_table/table.rs:250`）用的是同一套机制。
fn calendar_cell(
    day: CalendarDate,
    anchor: CalendarDate,
    today: CalendarDate,
    tasks: &[Task],
    layout: CalendarLayout,
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

    // 跨月淡化只对月视图有意义：周视图里「11 月 1 日」就是这一周的一天，
    // 没有「不在本月」这回事。
    let other_month = layout.mode == CalendarMode::Month && day.month() != anchor.month();
    let is_today = day == today;
    let shown = items.len().min(layout.detail);

    let mut cell = v_flex()
        .id(SharedString::from(format!(
            "projects-calendar-{}",
            day.to_iso()
        )))
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.xxs)
        .p(tokens.spacing.xxs)
        .border_r(extended.stroke.thin)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .overflow_hidden()
        // 整格读成「某年某月某日，N 条任务」：网格里几十个格子长得很像，
        // 读屏软件（以及「按元素找今天」这类审计）只能靠这句话分辨它们。
        .role(Role::Cell)
        .aria_label(day_cell_aria(day, items.len(), cx));
    if let Some(height) = layout.height {
        cell = cell.h(px(height));
    }
    // 今天的格子用强调色描边（设计文档 §5.3）——月视图里这是「哪个格子是今天」的
    // 最强信号；周视图的列头已经用强调色写了日期，格子这层不用再来一道。
    if is_today && layout.mode == CalendarMode::Month {
        cell = cell.border_color(tokens.colors.primary);
    }
    // ⚠️ 必须第一个 add：它要画在最底层（见函数头），并且靠 `.occlude()` 的
    // 任务条把它按块截断。
    cell = cell.child(day_backdrop(day, layout.mode, view, cx));

    let mut head = h_flex().flex_none().w_full().gap(tokens.spacing.xxs);
    if layout.mode == CalendarMode::Month {
        // 日号：今天加强调色圆底，跨月的日子淡化。
        head = head.child(
            div()
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
                .child(SharedString::from(day.day().to_string())),
        );
    }
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

    for task in items.iter().take(shown) {
        cell = cell.child(calendar_task_pill(task, view, cx));
    }
    if items.len() > shown {
        cell = cell.child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(trf(
                    cx,
                    "+{} 更多",
                    &[&(items.len() - shown).to_string()],
                ))),
        );
    }

    cell.when(other_month, |this| this.opacity(extended.opacity.muted))
        .into_any_element()
}

/// 格子的读屏描述：「10 月 13 日 · 周二 · 3 个任务」。
///
/// 写成一句完整的话而不是「13 3」这种拼出来的字符串：格子之间只有这句话不同，
/// 它得自己站得住。没有任务的日子也要能读出来（「没有任务」不是「没说」）。
///
/// 数量那一段走 `trn`（不是 `trf`）：英文按 1 / N 取单复数，`1 tasks` 是错的。
fn day_cell_aria(day: CalendarDate, count: usize, cx: &App) -> SharedString {
    let date = trf(
        cx,
        "{} 月 {} 日",
        &[&day.month().to_string(), &day.day().to_string()],
    );
    let weekday = weekday_label(day.weekday(), cx);
    let label = if count == 0 {
        trf(cx, "{} {} · 没有任务", &[&date, weekday])
    } else {
        trf(
            cx,
            "{} {} · {}",
            &[&date, weekday, &trn(cx, "{} 个任务", count as i64)],
        )
    };
    SharedString::from(label)
}

/// 铺满格子的「空白处」层：只负责承接**这一天的右键菜单**（见 [`calendar_cell`] 头）。
///
/// ⚠️ 两个不能省的细节：
///
/// 1. **带 `.hover(...)`**：gpui 的 `Div` 只在「有交互性」时才插入命中框
///    （`Div::should_insert_hitbox`），而 `ContextMenuExt` 正是靠命中框判 hover 的
///    —— 不挂 hover 样式就没有命中框，菜单永远不弹。（顺带也是「这块地方能右键」
///    的视觉提示，符合「悬停一律强调色薄底」。）
/// 2. **`absolute` + `inset_0`**：它是格子里的第一个子元素（先画 = 最底层），
///    正常流布局会让它占掉一行高度、把任务条挤下去，所以必须绝对定位。
fn day_backdrop(
    day: CalendarDate,
    mode: CalendarMode,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let view = view.clone();
    div()
        .id(SharedString::from(format!(
            "projects-calendar-day-{}",
            day.to_iso()
        )))
        .absolute()
        .inset_0()
        .rounded(tokens.spacing.xxs)
        .hover(move |style| style.bg(extended.colors.hover_tint))
        .context_menu(move |menu, _window, cx| calendar_menu(menu, day, mode, &view, cx))
        .into_any_element()
}

/// 日历格里的一条任务条：点击选中、双击开编辑器、右键给任务菜单。
///
/// 与看板卡片同一套手势（`is_double_click` + [`task_menu`]）：同一批任务在哪个视图里
/// 都该是同一套操作。
fn calendar_task_pill(task: &Task, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let id = task.id;
    let status = task.status;
    let view = view.clone();
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
        // ⚠️ `occlude` 必须在 `context_menu` **之前**：后者返回 `ContextMenu` 包装，
        // 不再是可配 `hitbox_behavior` 的元素（同 `task_card` 里 `.on_drag` 的位置约束）。
        .occlude()
        .child(priority_dot(task.priority, cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if status == TaskStatus::Done {
                    tokens.colors.muted_foreground
                } else {
                    tokens.colors.foreground
                })
                .child(selectable_text(SharedString::from(task.title.clone()))),
        )
        .on_click({
            let view = view.clone();
            move |event: &ClickEvent, window, cx| {
                if is_double_click(event) {
                    let _ = view.update(cx, |page, cx| {
                        super::dialogs::open_task_editor(
                            page,
                            Some(id),
                            None,
                            status,
                            None,
                            window,
                            cx,
                        )
                    });
                    return;
                }
                let _ = view.update(cx, |page, cx| page.select_task(Some(id), cx));
            }
        })
        .context_menu({
            let view = view.clone();
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
                        .child(selectable_text(SharedString::from(task.title.clone()))),
                ),
        )
        .child(track)
        .into_any_element()
}

/// 报表视图：筛选条 + KPI + 完成趋势 + 按项目 / 标签分布 + 逾期清单 + 完成明细。
///
/// 数字全部**现算**（[`report::build`] 一次遍历），不额外落统计表 —— 统计表要跟着
/// 每一次写维护，早晚会有一次忘掉，然后就出现「报表和看板对不上」。
///
/// 每一块的口径写在面板右上角。筛选条上那三组筛选各自作用于哪一块，见 `report.rs`
/// 的模块头：状态 / 标签筛**整份**报表，时间范围只筛时间轴上的两块（趋势与明细）。
fn render_report(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let today = state.today();
    let report = report::build(
        state.tasks(),
        today,
        &state.report_filters(),
        state.projects(),
        state.tags(),
    );

    let kpis = h_flex()
        .flex_none()
        .w_full()
        .gap(tokens.spacing.sm)
        .children(
            report_kpis(&report, cx)
                .into_iter()
                .map(|kpi| report_kpi(kpi, cx)),
        );

    // 完成趋势：一根柱 = 一桶（近 7 / 30 天按日、近 90 天按周），柱高按整段最大值归一。
    let peak = report
        .trend
        .iter()
        .map(|bar| bar.count)
        .max()
        .unwrap_or(0)
        .max(1);
    let mut bars = h_flex()
        .flex_none()
        .w_full()
        .items_end()
        .gap(tokens.spacing.xs)
        .h(px(TREND_CHART_HEIGHT));
    for bar in &report.trend {
        let ratio = bar.count as f32 / peak as f32;
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
                        .child(selectable_text(SharedString::from(if bar.count == 0 {
                            String::new()
                        } else {
                            bar.count.to_string()
                        }))),
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
                        .child(SharedString::from(if bar.labelled {
                            bar.label.clone()
                        } else {
                            // 柱子太密时留空：`report.rs` 已经算好哪几根要写字，
                            // 这里不自己数下标（两处各数一遍迟早对不上）。
                            String::new()
                        })),
                ),
        );
    }

    // 按项目分布：一行一个项目（含收集箱）。
    let mut by_project = v_flex().w_full().flex_none().gap(tokens.spacing.xs);
    for row in &report.by_project {
        by_project = by_project.child(distribution_row(
            SharedString::from(row.name.clone()),
            row.color
                .as_deref()
                .and_then(color_from_hex)
                .unwrap_or(tokens.colors.muted_foreground),
            row.done,
            row.total,
            cx,
        ));
    }
    by_project = by_project.child(distribution_row(
        SharedString::from(tr(cx, "未归入项目")),
        tokens.colors.muted_foreground,
        report.inbox.done,
        report.inbox.total,
        cx,
    ));

    // 按标签分布：只列当前筛选下真的挂着标签的那些（没有就直说，不留一块空白）。
    let mut by_tag = v_flex().w_full().flex_none().gap(tokens.spacing.xs);
    if report.by_tag.is_empty() {
        by_tag = by_tag.child(report_empty_line("当前筛选下没有带标签的任务。", cx));
    } else {
        for row in &report.by_tag {
            by_tag = by_tag.child(distribution_row(
                SharedString::from(row.name.clone()),
                row.color
                    .as_deref()
                    .and_then(color_from_hex)
                    .unwrap_or(tokens.colors.muted_foreground),
                row.done,
                row.total,
                cx,
            ));
        }
    }

    let mut overdue_list = v_flex().w_full().flex_none().gap(tokens.spacing.xxs);
    if report.overdue.is_empty() {
        overdue_list = overdue_list.child(report_empty_line("当前没有逾期任务。", cx));
    } else {
        for task in &report.overdue {
            overdue_list = overdue_list.child(report_overdue_row(task, today, state, view, cx));
        }
    }

    let mut completed_list = v_flex().w_full().flex_none().gap(tokens.spacing.xxs);
    if report.completed.is_empty() {
        completed_list =
            completed_list.child(report_empty_line("当前筛选下这段时间没有完成的任务。", cx));
    } else {
        for row in &report.completed {
            completed_list = completed_list.child(report_completed_row(row, state, view, cx));
        }
    }

    // 明细分块的右上角：没截断就报真实条数，截断了就把上限一起说清。
    let completed_hint = match report.detail_truncated_note() {
        Some((total, limit)) => trf(
            cx,
            "共 {} 项 · 仅列最近 {}",
            &[&total.to_string(), &limit.to_string()],
        ),
        None => trf(cx, "{} 项", &[&report.completed.len().to_string()]),
    };
    let distribution_note = trf(
        cx,
        "平均进度（未完成项）{}%",
        &[&report.avg_progress.to_string()],
    );

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
                .child(report_filter_bar(state, view, cx))
                .child(kpis)
                .child(report_panel(
                    SharedString::from(trf(
                        cx,
                        "近 {} 天完成趋势",
                        &[&report::window_days(state.report_range()).to_string()],
                    )),
                    SharedString::from(if report.bucket_days > 1 {
                        trf(
                            cx,
                            "口径：按完成日计 · 每 {} 天一根柱",
                            &[&report.bucket_days.to_string()],
                        )
                    } else {
                        tr(cx, "口径：按完成日计").to_owned()
                    }),
                    bars.into_any_element(),
                    cx,
                ))
                .child(report_panel(
                    SharedString::from(tr(cx, "按项目分布")),
                    SharedString::from(trf(
                        cx,
                        "完成 / 总数 · {} 项",
                        &[&report.scoped.to_string()],
                    )),
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .child(by_project)
                        .child(report_note_line(distribution_note, cx))
                        .into_any_element(),
                    cx,
                ))
                .child(report_panel(
                    SharedString::from(tr(cx, "按标签分布")),
                    SharedString::from(tr(cx, "完成 / 总数")),
                    by_tag.into_any_element(),
                    cx,
                ))
                .child(report_panel(
                    SharedString::from(tr(cx, "逾期清单")),
                    SharedString::from(trf(cx, "{} 项", &[&report.overdue.len().to_string()])),
                    overdue_list.into_any_element(),
                    cx,
                ))
                .child(report_panel(
                    SharedString::from(tr(cx, "完成明细")),
                    SharedString::from(completed_hint),
                    completed_list.into_any_element(),
                    cx,
                )),
        )
        .into_any_element()
}

/// 趋势柱的高度（几何量，留裸值并写明来由：7 根柱 + 上下标签刚好排满一屏的三分之一）。
const TREND_CHART_HEIGHT: f32 = 150.;

/// 报表上的一枚 KPI：标题、数值（已格式化）、附注。
///
/// 单独立一个结构而不是让渲染层自己拼：**导出报表**要的是同一批文字（`report_markdown`
/// 里那几行「指标 | 数值」就是它），两处各拼一遍必然有一处先改漏。数值里的附注由
/// [`report_kpis`] 拼好，导出时直接把 `value` 与 `note` 合成一格。
struct ReportKpi {
    kind: KpiKind,
    label: &'static str,
    value: String,
    note: String,
}

/// KPI 的标识：决定元素 id 与数值颜色（颜色映射见 [`kpi_color`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KpiKind {
    Done,
    Doing,
    Blocked,
    Overdue,
    Cycle,
}

impl KpiKind {
    /// 稳定 ASCII 元素 id（用译文当 id 会在切语言时被当成另一个元素）。
    const fn id(self) -> &'static str {
        match self {
            Self::Done => "projects-kpi-done",
            Self::Doing => "projects-kpi-doing",
            Self::Blocked => "projects-kpi-blocked",
            Self::Overdue => "projects-kpi-overdue",
            Self::Cycle => "projects-kpi-cycle",
        }
    }

    const fn icon(self) -> DuckIcon {
        match self {
            Self::Done => DuckIcon::CircleCheck,
            Self::Doing => DuckIcon::Play,
            Self::Blocked => DuckIcon::Pause,
            Self::Overdue => DuckIcon::TriangleAlert,
            Self::Cycle => DuckIcon::Clock,
        }
    }
}

/// 五枚 KPI 的文字（唯一来源：界面与导出都读它）。
///
/// 「平均周期」是这一批新加的：参考实现的报表有「平均耗时」，而本仓原本只有「进度」。
/// 口径见 `report::build` —— 只看已完成任务，创建日 → 完成日。
fn report_kpis(report: &report::Report<'_>, cx: &App) -> Vec<ReportKpi> {
    let kpi = |kind, label, value: String, note: String| ReportKpi {
        kind,
        label,
        value,
        note,
    };
    vec![
        kpi(
            KpiKind::Done,
            tr(cx, "已完成"),
            report.done.to_string(),
            trf(cx, "范围内完成 {} 个", &[&report.done_in_range.to_string()]),
        ),
        kpi(
            KpiKind::Doing,
            tr(cx, "进行中"),
            report.doing.to_string(),
            doing_note(report.doing as u32, cx),
        ),
        kpi(
            KpiKind::Blocked,
            tr(cx, "阻塞中"),
            report.blocked.to_string(),
            tr(cx, "均需外部输入才能推进").to_owned(),
        ),
        kpi(
            KpiKind::Overdue,
            tr(cx, "逾期"),
            report.overdue.len().to_string(),
            trf(
                cx,
                "最久逾期 {} 天",
                &[&report.max_overdue_days.to_string()],
            ),
        ),
        kpi(
            KpiKind::Cycle,
            tr(cx, "平均周期"),
            // 没有样本时说「—」而不是「0 天」：0 天读起来像「当天就完成了」。
            report
                .avg_cycle_days
                .map_or_else(|| "—".to_owned(), |days| days.to_string()),
            match report.avg_cycle_days {
                Some(_) => trf(
                    cx,
                    "按创建日 → 完成日 · {} 个样本",
                    &[&report.cycle_samples.to_string()],
                ),
                None => tr(cx, "暂无已完成的样本").to_owned(),
            },
        ),
    ]
}

/// KPI 数值的颜色。
///
/// 只有真正带情绪的两枚上色（阻塞 = 警告、逾期 = 危险），其余用正文色 —— 一排五张
/// 卡全上色等于一张都没强调。
fn kpi_color(kind: KpiKind, cx: &App) -> Hsla {
    let theme = active_theme(cx);
    match kind {
        KpiKind::Blocked => theme.extended().colors.warning,
        KpiKind::Overdue => theme.tokens().colors.destructive,
        KpiKind::Done | KpiKind::Doing | KpiKind::Cycle => theme.tokens().colors.foreground,
    }
}

/// 报表的一张 KPI 卡。
fn report_kpi(kpi: ReportKpi, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    v_flex()
        .id(kpi.kind.id())
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
                .child(Icon::new(kpi.kind.icon()).size(tokens.typography.xs.size))
                .child(SharedString::from(kpi.label)),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xl.size)
                .line_height(tokens.typography.xl.line_height)
                .text_color(kpi_color(kpi.kind, cx))
                .child(selectable_text(SharedString::from(kpi.value))),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(SharedString::from(kpi.note))),
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

/// 报表的一张面板（标题 + 右上角口径 + 内容）。
///
/// `title` 收 `impl Into<SharedString>` 而不是 `&'static str`：趋势面板的标题要带
/// 当前范围（「近 30 天完成趋势」），那是一句拼出来的文字。
fn report_panel(
    title: impl Into<SharedString>,
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

/// 分布面板里的一行（「按项目分布」「按标签分布」共用）：色点 + 名称 + 进度条 + `完成 / 总数`。
fn distribution_row(
    name: SharedString,
    color: Hsla,
    done: u32,
    total: u32,
    cx: &App,
) -> AnyElement {
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
                        .child(selectable_text(name)),
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

// ---------------------------------------------------------------------------
// 报表：筛选条
// ---------------------------------------------------------------------------

/// 报表筛选条：时间范围 + 状态 + 标签（多选）+ 导出。
///
/// 放在**报表内部**而不是工具条上：这三组筛选只作用于报表这一个视图，摆进全局工具条
/// 会让人以为看板也跟着筛。工具条上的「范围 / 排序 / 分组」才是全局口径。
///
/// 标签那一组只在库里真有标签时出现（没标签却摆一排空 chip 只会占地方）；`flex_wrap`
/// 让标签多起来时自然折行，而不是把导出按钮挤出屏幕。
fn report_filter_bar(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    h_flex()
        .flex_none()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(tokens.spacing.sm)
        .p(tokens.spacing.sm)
        .rounded(theme.components().card_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.surface)
        .child(report_filter_label(tr(cx, "时间范围"), cx))
        .child(report_range_switcher(state, view, cx))
        .child(report_filter_label(tr(cx, "状态"), cx))
        .child(report_status_switcher(state, view, cx))
        .children(report_tag_filter(state, view, cx))
        .child(div().flex_1().min_w_0())
        .child(report_export_button(view, cx))
        .into_any_element()
}

/// 筛选条上那一枚小标签（「时间范围」「状态」「标签」）。
fn report_filter_label(text: &'static str, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    div()
        .flex_none()
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(SharedString::from(text))
        .into_any_element()
}

/// 时间范围档（三档 segmented）。
fn report_range_switcher(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let current = state.report_range();
    let labels: Vec<SharedString> = ReportRange::ALL
        .iter()
        .map(|range| SharedString::from(report_range_label(*range, cx)))
        .collect();
    let selected = ReportRange::ALL
        .iter()
        .position(|range| *range == current)
        .unwrap_or(0);
    // 与 `sort_switcher` 同理：闭包要求 `'static`，先把引用克隆成拥有所有权的实体。
    let view = view.clone();
    segmented_tabs(
        "projects-report-range",
        labels,
        selected,
        move |index, _window, cx| {
            let Some(range) = ReportRange::ALL.get(index).copied() else {
                return;
            };
            let _ = view.update(cx, |page, cx| page.set_report_range(range, cx));
        },
        cx,
    )
    .into_any_element()
}

/// 状态档（四档 segmented）。
fn report_status_switcher(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let current = state.report_status();
    let labels: Vec<SharedString> = ReportStatus::ALL
        .iter()
        .map(|status| SharedString::from(report_status_label(*status, cx)))
        .collect();
    let selected = ReportStatus::ALL
        .iter()
        .position(|status| *status == current)
        .unwrap_or(0);
    let view = view.clone();
    segmented_tabs(
        "projects-report-status",
        labels,
        selected,
        move |index, _window, cx| {
            let Some(status) = ReportStatus::ALL.get(index).copied() else {
                return;
            };
            let _ = view.update(cx, |page, cx| page.set_report_status(status, cx));
        },
        cx,
    )
    .into_any_element()
}

/// 标签档：一个「标签」小标题 + 一枚「全部」+ 每个标签一枚 chip（**可多选**）。
///
/// 「全部」在没有任何标签被勾中时是亮的 —— 它表达的是「不限」，不是「第五个标签」。
fn report_tag_filter(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> Option<AnyElement> {
    if state.tags().is_empty() {
        return None;
    }
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let selected = state.report_tags();
    let mut row = h_flex()
        .flex_none()
        .flex_wrap()
        .items_center()
        .gap(tokens.spacing.xs)
        .child(report_filter_label(tr(cx, "标签"), cx))
        .child({
            let view = view.clone();
            chip(
                "projects-report-tag-all",
                None,
                SharedString::from(tr(cx, "全部")),
                selected.is_empty(),
                cx,
            )
            .on_click(move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.clear_report_tags(cx));
            })
        });
    for tag in state.tags() {
        let is_on = selected.contains(&tag.id);
        let dot = div()
            .flex_none()
            .size(tokens.spacing.xs)
            .rounded_full()
            .bg(color_from_hex(&tag.color).unwrap_or(tokens.colors.muted_foreground))
            .into_any_element();
        let view = view.clone();
        let id = tag.id;
        row = row.child(
            chip(
                SharedString::from(format!("projects-report-tag-{id}")),
                Some(dot),
                SharedString::from(tag.name.clone()),
                is_on,
                cx,
            )
            .on_click(move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.toggle_report_tag(id, cx));
            }),
        );
    }
    Some(row.into_any_element())
}

/// 筛选条右侧的「导出报表」。
///
/// 点开的是**弹窗**（`dialogs::open_report_export`），里面两个出口：复制到剪贴板 /
/// 另存为。与「导出任务」完全一致 —— 两条导出路径各有一套出口，用户就得分别记。
fn report_export_button(view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let icon = Icon::new(DuckIcon::Download).size(active_theme(cx).extended().icon.sm);
    let view = view.clone();
    icon_text_button(
        "projects-report-export",
        tr(cx, "导出报表"),
        icon,
        ButtonVariant::Secondary,
        cx,
    )
    .on_click(move |_, window, cx| {
        let _ = view.update(cx, |page, cx| page.open_report_export(window, cx));
    })
    .into_any_element()
}

/// 时间范围档的中文名。
fn report_range_label(range: ReportRange, cx: &App) -> &'static str {
    match range {
        ReportRange::Days7 => tr(cx, "近 7 天"),
        ReportRange::Days30 => tr(cx, "近 30 天"),
        ReportRange::Days90 => tr(cx, "近 90 天"),
    }
}

/// 状态档的中文名。
fn report_status_label(status: ReportStatus, cx: &App) -> &'static str {
    match status {
        ReportStatus::All => tr(cx, "全部"),
        ReportStatus::Open => tr(cx, "未完成"),
        ReportStatus::Done => tr(cx, "已完成"),
        ReportStatus::Cancelled => tr(cx, "已取消"),
    }
}

/// 报表里一句「这里没有东西」。
///
/// `#[track_caller]` + `selectable_text_at`：本函数被三处调用、传的是不同的句子，若在
/// 函数体内直接调 `selectable_text`，三处的「调用点」会全部落在这一行上（见
/// `duck_components` 的 id 规则）。取调用方的位置是仓库里所有文本 helper 的统一写法。
#[track_caller]
fn report_empty_line(text: &str, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    div()
        .w_full()
        .p(tokens.spacing.sm)
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(selectable_text_at(
            std::panic::Location::caller(),
            SharedString::from(text.to_owned()),
        ))
        .into_any_element()
}

/// 面板底下那行小字（「平均进度（未完成项）40%」）。
#[track_caller]
fn report_note_line(text: String, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    div()
        .flex_none()
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(selectable_text_at(
            std::panic::Location::caller(),
            SharedString::from(text),
        ))
        .into_any_element()
}

/// 「完成明细」里的一行：任务 + 项目 + 完成日 + 周期。
///
/// 点一行就选中那条任务（与逾期清单同一套动作）—— 报表上看到一条眼生的，下一步
/// 总是「打开看看」。
fn report_completed_row(
    row: &report::CompletedRow,
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let id = row.task_id;
    let project_color = row
        .project_color
        .as_deref()
        .and_then(color_from_hex)
        .unwrap_or(tokens.colors.muted_foreground);
    let project = match row.project_id {
        Some(project_id) => project_name(state, project_id, cx),
        None => SharedString::from(tr(cx, "收集箱")),
    };
    let cycle = row.cycle_days.map_or_else(
        || "—".to_owned(),
        |days| trf(cx, "{} 天", &[&days.to_string()]),
    );

    h_flex()
        .id(SharedString::from(format!("projects-completed-{id}")))
        .flex_none()
        .w_full()
        .gap(tokens.spacing.xs)
        .px(tokens.spacing.xs)
        .py(tokens.spacing.xxs)
        .rounded(tokens.spacing.xxs)
        .hover(move |style| style.bg(extended.colors.hover_tint))
        .role(Role::Button)
        .aria_label(SharedString::from(row.title.clone()))
        .cursor_pointer()
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
                .child(selectable_text(SharedString::from(row.title.clone()))),
        )
        .child(
            div()
                .flex_none()
                .w(px(REPORT_NAME_WIDTH))
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(project)),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(SharedString::from(row.completed.to_iso()))),
        )
        .child(
            div()
                .flex_none()
                .w(px(REPORT_CYCLE_WIDTH))
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(SharedString::from(cycle))),
        )
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.select_task(Some(id), cx));
            }
        })
        .into_any_element()
}

/// 「周期」那一列的宽度（几何量：容得下「1234 天」再留一点余量，让完成日对齐）。
const REPORT_CYCLE_WIDTH: f32 = 64.;

// ---------------------------------------------------------------------------
// 报表：导出（Markdown）
// ---------------------------------------------------------------------------

/// 把当前报表拼成一份 Markdown（「导出报表」弹窗走这里）。
///
/// 两个出口共用这一份文字：复制到剪贴板与另存为（见 [`ProjectsPage::open_report_export`]）。
/// 这里只负责把文字拼出来 —— 口径完全照屏幕上的那份（同一个 [`report::build`]、同一批
/// [`report_kpis`] 文案），于是「导出」永远不会和「屏幕上看到的」各说各话。
pub(super) fn report_markdown(state: &ProjectsState, cx: &App) -> String {
    let today = state.today();
    let filters = state.report_filters();
    let report = report::build(
        state.tasks(),
        today,
        &filters,
        state.projects(),
        state.tags(),
    );
    let export = report::ReportExport {
        labels: report_labels(cx),
        scope_line: report_scope_line(state, &report, cx),
        kpi_rows: report_kpis(&report, cx)
            .into_iter()
            .map(|kpi| {
                (
                    kpi.label.to_owned(),
                    // 数值与附注合成一格（「12（范围内完成 3 个）」）：表头就只要两列，
                    // 也少一套要翻译的列名。
                    if kpi.note.is_empty() {
                        kpi.value
                    } else {
                        format!("{}（{}）", kpi.value, kpi.note)
                    },
                )
            })
            .collect(),
        distribution_note: trf(
            cx,
            "平均进度（未完成项）{}%",
            &[&report.avg_progress.to_string()],
        ),
        completed_note: report.detail_truncated_note().map(|(total, limit)| {
            trf(
                cx,
                "共 {} 项 · 仅列最近 {}",
                &[&total.to_string(), &limit.to_string()],
            )
        }),
        inbox_name: tr(cx, "未归入项目").to_owned(),
        projects: state.projects(),
    };
    report::to_markdown(&report, today, &export)
}

/// 导出那份报表的表头文案（全在 `tr` 里取，`report.rs` 不认识界面语言）。
fn report_labels(cx: &App) -> report::ReportLabels<'_> {
    report::ReportLabels {
        title: tr(cx, "任务报表"),
        scope: tr(cx, "口径"),
        overview: tr(cx, "概览"),
        metric: tr(cx, "指标"),
        value: tr(cx, "数值"),
        trend: tr(cx, "完成趋势"),
        date: tr(cx, "日期"),
        count: tr(cx, "完成数"),
        by_project: tr(cx, "按项目分布"),
        by_tag: tr(cx, "按标签分布"),
        name: tr(cx, "名称"),
        done: tr(cx, "完成"),
        total: tr(cx, "总数"),
        overdue: tr(cx, "逾期清单"),
        completed: tr(cx, "完成明细"),
        task: tr(cx, "任务"),
        project: tr(cx, "项目"),
        overdue_days: tr(cx, "逾期天数"),
        completed_on: tr(cx, "完成日"),
        cycle_days: tr(cx, "周期（天）"),
        empty: tr(cx, "（无）"),
    }
}

/// 「口径」那一行：当前三组筛选 + 命中条数。
///
/// 导出出去的文字脱离了界面，筛选条上亮着的 chips 都看不见了 —— 所以这一行是**必须**
/// 的：少掉它，收到报表的人看到「已完成 12」时无从知道这 12 是哪些里数出来的。
fn report_scope_line(state: &ProjectsState, report: &report::Report<'_>, cx: &App) -> String {
    let tags = match state.report_tags() {
        [] => tr(cx, "不限").to_owned(),
        selected => selected
            .iter()
            .filter_map(|id| {
                state
                    .tags()
                    .iter()
                    .find(|tag| tag.id == *id)
                    .map(|tag| tag.name.clone())
            })
            .collect::<Vec<_>>()
            .join(" / "),
    };
    trf(
        cx,
        "状态「{}」· 标签「{}」· 时间范围「{}」· 共 {} 项",
        &[
            report_status_label(state.report_status(), cx),
            &tags,
            report_range_label(state.report_range(), cx),
            &report.scoped.to_string(),
        ],
    )
}

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
                .child(selectable_text(SharedString::from(task.title.clone()))),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.destructive)
                .child(selectable_text(SharedString::from(trf(
                    cx,
                    "逾期 {} 天",
                    &[&days.to_string()],
                )))),
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
///
/// 「周计划」档是**另一块内容**（数据源也不同：`weekly_plans` 表，不是 `work_logs`），
/// 所以直接转给 [`render_plan`]：两档共用一段渲染代码只会让每一行都要写一次
/// `if kind == Plan`，而它们连左清单列的东西都不一样（一个是日期、一个是周）。
fn render_log(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let kind = state.log_kind();
    if kind == LogKind::Plan {
        return render_plan(state, view, cx);
    }
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
            // 行内标签摘要：日期之后跟一串 `#标签`，长了就截断（完整的在右栏）。
            let tag_summary: Option<SharedString> = if log.tags.is_empty() {
                None
            } else {
                Some(SharedString::from(
                    log.tags
                        .iter()
                        .map(|tag| format!("#{tag}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                ))
            };
            list = list.child(
                div()
                    .id(SharedString::from(format!("projects-log-row-{id}")))
                    .w_full()
                    .min_w_0()
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
                    .child(selectable_text(SharedString::from(log.date.to_iso())))
                    .when_some(tag_summary, |this, tags| {
                        this.child(
                            div()
                                .w_full()
                                .min_w_0()
                                .truncate()
                                .text_size(tokens.typography.xs.size)
                                .line_height(tokens.typography.xs.line_height)
                                .text_color(tokens.colors.muted_foreground)
                                .child(selectable_text(tags)),
                        )
                    })
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
        Some(log) if !log.content_md.trim().is_empty() => v_flex()
            .id("projects-log-body")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .gap(tokens.spacing.sm)
            .when(!log.tags.is_empty(), |this| {
                this.child(log_tag_row(&log.tags, cx))
            })
            .child(
                div()
                    .text_size(tokens.typography.sm.size)
                    .line_height(tokens.typography.sm.line_height)
                    .text_color(tokens.colors.foreground)
                    .child(selectable_text(SharedString::from(log.content_md.clone()))),
            )
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

/// 一篇日志的标签行（右栏正文上方）。
///
/// 标签在这里是**内容的一部分**（用户自己打上去的分类词），所以文字走
/// `selectable_text`；与列表行的摘要不同，这里给全部标签，不截断。
fn log_tag_row(tags: &[String], cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let mut row = h_flex()
        .w_full()
        .min_w_0()
        .flex_wrap()
        .gap(tokens.spacing.xxs);
    for tag in tags {
        row = row.child(log_tag_chip(tag, cx));
    }
    row.into_any_element()
}

/// 日志标签片。
///
/// 与 [`tag_chip`]（任务的标签实体）分开：任务标签在库里有 id 与用户选的颜色，
/// 日志标签是手写的分类词，两者只有「看起来都像胶囊」这一点相同。
/// 硬套那一套就得给每个词编一个颜色。
fn log_tag_chip(tag: &str, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .flex_none()
        .px(tokens.spacing.xs)
        .py(tokens.spacing.xxs)
        .rounded_full()
        .bg(tokens.colors.muted)
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(selectable_text(SharedString::from(format!("#{tag}"))))
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
                .child(selectable_text(SharedString::from(
                    date.map_or_else(String::new, |day| day.to_iso()),
                ))),
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

// ---------------------------------------------------------------------------
// 周计划（工作日志视图的「周计划」档）
// ---------------------------------------------------------------------------

/// 周计划视图：周选择器 + 计划 vs 实际两栏。
///
/// # 三个数据源，一次说清
///
/// | 区块 | 来源 | 为什么不是别的 |
/// | --- | --- | --- |
/// | 左清单（最近写过的周计划） | `weekly_plans` 全表（最近 [`PLAN_LIST_LIMIT`] 周） | 「我以前排过什么」——只列有计划的周，空周不占位 |
/// | 左栏「在计划里」 | 计划里的 `task_ids` → **按 id 取任务现状** | 计划是历史事实：任务可能已被归档、甚至彻底删掉 |
/// | 右栏「这一周到期」 | 内核 `DueFilter::ThisWeek`（= 周一到周日到期且未完成） | 复用内核口径，不在这里另写一条 `BETWEEN` |
///
/// 「完成率」按**当前**计划里已完成的比例算（分母是 `task_ids` 的长度），不是按
/// `original_task_ids` —— 用户看这一栏想知道的是「这周排的活干完了多少」，
/// 把后来加进去的排除在外会让他觉得数字一直在骗人。
fn render_plan(state: &ProjectsState, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let anchor = state.plan_anchor();
    let monday = anchor.start_of_week();

    // 左清单：最近写过的周计划。
    let current_key = state.plan_week_key();
    let mut list = v_flex().w_full().flex_none().gap(tokens.spacing.xxs);
    if state.plans().is_empty() {
        list = list.child(
            div()
                .w_full()
                .p(tokens.spacing.sm)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(tr(cx, "还没有排过周计划。"))),
        );
    } else {
        for plan in state.plans() {
            list = list.child(plan_list_row(
                &plan.week_key,
                plan.task_ids.len(),
                plan.is_confirmed(),
                plan.week_key == current_key,
                view,
                cx,
            ));
        }
    }

    // 右栏：正文 + 两栏任务。
    let planned_rows: Vec<AnyElement> = state
        .plan_tasks()
        .iter()
        .map(|task| plan_task_row(task, PlanRowAction::Remove, view, cx))
        .collect();
    let addable_rows: Vec<AnyElement> = state
        .addable_tasks()
        .into_iter()
        .map(|task| plan_task_row(task, PlanRowAction::Add, view, cx))
        .collect();

    let content_block: Option<AnyElement> = state
        .current_plan()
        .map(|plan| plan.content_md.trim())
        .filter(|text| !text.is_empty())
        .map(|text| {
            div()
                .id("projects-plan-body")
                .w_full()
                .flex_none()
                .max_h(px(PLAN_BODY_MAX_HEIGHT))
                .overflow_y_scroll()
                .p(tokens.spacing.sm)
                .rounded(theme.components().card_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .bg(tokens.colors.surface)
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(selectable_text(SharedString::from(text.to_owned())))
                .into_any_element()
        });

    let empty_hint: Option<AnyElement> = if state.has_plan() {
        None
    } else {
        Some(
            div()
                .w_full()
                .flex_none()
                .p(tokens.spacing.sm)
                .rounded(theme.components().card_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(tr(
                    cx,
                    "这一周还没排计划 —— 点「编辑计划」写点什么，或从右边把任务加进来。",
                )))
                .into_any_element(),
        )
    };

    v_flex()
        .size_full()
        .min_h_0()
        .min_w_0()
        .child(plan_header(anchor, state, view, cx))
        .children(schedule_panel(anchor, state.today(), state, cx))
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
                        .id("projects-plan-list")
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
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .p(tokens.spacing.sm)
                        .gap(tokens.spacing.sm)
                        .children(empty_hint)
                        .children(content_block)
                        .child(
                            h_flex()
                                .flex_1()
                                .min_h_0()
                                .w_full()
                                .gap(tokens.spacing.sm)
                                .child(plan_column(
                                    "projects-plan-column-planned",
                                    SharedString::from(tr(cx, "在计划里")),
                                    SharedString::from(tr(cx, "点任务行看详情，双击打开编辑器。")),
                                    planned_rows,
                                    SharedString::from(tr(cx, "左边还没有任务。")),
                                    cx,
                                ))
                                .child(plan_column(
                                    "projects-plan-column-week",
                                    SharedString::from(trf(
                                        cx,
                                        "{} 到期",
                                        &[week_range_label(monday, cx).as_ref()],
                                    )),
                                    SharedString::from(tr(cx, "本周到期且还没完成的任务。")),
                                    addable_rows,
                                    SharedString::from(tr(cx, "这一周没有到期的任务。")),
                                    cx,
                                )),
                        ),
                ),
        )
        .into_any_element()
}

/// 周计划的正文块最大高度。
///
/// 与两栏任务列表**抢**同一块纵向空间，所以要有个上限：正文是「这周的说明」，
/// 通常几行；不限高的话一段长文字会把两栏压成一条缝，而真正要操作的东西在下面。
const PLAN_BODY_MAX_HEIGHT: f32 = 160.;

/// 周计划的左清单一行：周键 + 任务数 + 状态。
fn plan_list_row(
    week_key: &str,
    task_count: usize,
    confirmed: bool,
    active: bool,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let key = week_key.to_owned();
    let label = SharedString::from(week_key.to_owned());
    div()
        .id(SharedString::from(format!("projects-plan-row-{week_key}")))
        .w_full()
        .min_w_0()
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
        .aria_label(label.clone())
        .hover(move |style| style.bg(extended.colors.hover_tint))
        .child(selectable_text(label))
        .child(
            div()
                .w_full()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(SharedString::from(trf(
                    cx,
                    "{} · {} 项",
                    &[plan_status_label(confirmed, cx), &task_count.to_string()],
                )))),
        )
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.show_plan_week(&key, cx));
            }
        })
        .into_any_element()
}

/// 「已确认 / 草稿」。
fn plan_status_label(confirmed: bool, cx: &App) -> &'static str {
    if confirmed {
        tr(cx, "已确认")
    } else {
        tr(cx, "草稿")
    }
}

/// 周计划的工具条：周选择器 + 本周 + 状态 + 四个动作。
fn plan_header(
    anchor: CalendarDate,
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let monday = anchor.start_of_week();
    let week_key = SharedString::from(anchor.week_key());
    let range = week_range_label(monday, cx);
    let stats = state.plan_stats();
    let has_plan = state.has_plan();
    let confirmed = state.current_plan().is_some_and(|plan| plan.is_confirmed());

    let nav = |delta: Option<i32>, icon: DuckIcon, label: &'static str, id: &'static str| {
        let view = view.clone();
        icon_button(
            SharedString::from(id),
            SharedString::from(label),
            Icon::new(icon).size(extended.icon.md),
            ButtonVariant::Ghost,
            cx,
        )
        .on_click(move |_, _, cx| {
            let _ = view.update(cx, |page, cx| page.step_plan_week(delta, cx));
        })
    };

    let mut actions = h_flex().flex_none().gap(tokens.spacing.xs);
    // 「本周」只在离开本周时才有意义 —— 停在本周时它是个点着没反应的按钮。
    if state.plan_week().is_some() {
        actions = actions.child({
            let view = view.clone();
            button(
                "projects-plan-today",
                tr(cx, "本周"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.step_plan_week(None, cx));
            })
        });
    }
    // 「带入上周未完成」：只在真有东西可带时出现。上周没排计划、或上周全做完了，
    // 这个按钮就不该在 —— 摆了却点了没反应，比不摆更让人怀疑功能坏了。
    if !state.carryover_tasks().is_empty() {
        actions = actions.child({
            let view = view.clone();
            let count = state.carryover_tasks().len().to_string();
            button(
                "projects-plan-carryover",
                trf(cx, "带入上周未完成（{}）", &[&count]),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.carry_over_last_week(cx));
            })
        });
    }
    if has_plan {
        // 「确认 / 改回草稿」是一个开关：文案随当前状态取反，点一下切过去。
        actions = actions.child({
            let view = view.clone();
            let (id, label, next) = if confirmed {
                (
                    "projects-plan-unconfirm",
                    tr(cx, "改回草稿"),
                    WeeklyPlanStatus::Draft,
                )
            } else {
                (
                    "projects-plan-confirm",
                    tr(cx, "确认"),
                    WeeklyPlanStatus::Confirmed,
                )
            };
            button(id, label, ButtonVariant::Secondary, cx).on_click(move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.set_plan_status(next, cx));
            })
        });
    }
    actions = actions.child({
        let view = view.clone();
        button(
            "projects-plan-edit",
            tr(cx, "编辑计划"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(move |_, window, cx| {
            let _ = view.update(cx, |page, cx| page.open_plan_editor(window, cx));
        })
    });
    if has_plan {
        actions = actions.child({
            let view = view.clone();
            button(
                "projects-plan-delete",
                tr(cx, "删除"),
                ButtonVariant::Destructive,
                cx,
            )
            .on_click(move |_, window, cx| {
                let _ = view.update(cx, |page, cx| page.confirm_delete_plan(window, cx));
            })
        });
    }

    h_flex()
        .flex_none()
        .w_full()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(
            h_flex()
                .flex_none()
                .gap(tokens.spacing.xxs)
                .child(nav(
                    Some(-1),
                    DuckIcon::ChevronLeft,
                    "上一周",
                    "projects-plan-prev",
                ))
                .child(
                    div()
                        .flex_none()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .text_color(tokens.colors.foreground)
                        .child(selectable_text(week_key)),
                )
                .child(nav(
                    Some(1),
                    DuckIcon::ChevronRight,
                    "下一周",
                    "projects-plan-next",
                )),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(range)),
        )
        .child(
            div()
                .flex_none()
                .px(tokens.spacing.xs)
                .rounded_full()
                .bg(if confirmed {
                    tokens.colors.primary.opacity(extended.opacity.tint)
                } else {
                    tokens.colors.muted
                })
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if confirmed {
                    tokens.colors.primary
                } else {
                    tokens.colors.muted_foreground
                })
                .child(SharedString::from(plan_status_label(confirmed, cx))),
        )
        .child(plan_summary(stats, cx))
        .child(div().flex_1().min_w_0())
        .child(actions)
        .into_any_element()
}

/// 排期负载（设计文档 F29）：锚定周七天各压了多少剩余工时，外加几句结论。
///
/// # 为什么挂在周计划里
///
/// 负载回答的是「**这一周**排得下排不下」，只在对着某一周时才有意义。单独开一页会让
/// 人先挑一次周，而周计划页本来就锚定在一周上 —— 两块看的是同一周，就该挨在一起。
///
/// # 读哪一批任务
///
/// **库里全部顶层任务**（与 [`super::dialogs`] 里生成 AI 草稿同一个口径）：
/// 这一周排不排得下，不该随左栏筛选条变来变去。全量读失败时回落到当前视图那一批 ——
/// 少算几条也只是把柱子画矮一点，好过整块面板消失。
///
/// # 什么时候不画
///
/// `schedule::ScheduleAdvice::has_findings` 为假时返回 `None`：一周排得整整齐齐，再摆一块
/// 「一切正常」的卡片，只会挤掉真正要看的两栏任务。
fn schedule_panel(
    anchor: CalendarDate,
    today: CalendarDate,
    state: &ProjectsState,
    cx: &App,
) -> Option<AnyElement> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let capacity = super::dialogs::HOURS_PER_WORKDAY as f64;
    let tasks = state.all_tasks().unwrap_or_else(|_| state.tasks().to_vec());
    let advice = super::schedule::analyze(&tasks, anchor, capacity);
    if !advice.has_findings() {
        return None;
    }
    let peak = advice.peak();
    // 柱子的满格高度。裸值而非间距阶梯：它是一根**量尺**，跟着密度变会让同一周
    // 在两种密度下看起来负载不同（这正是这张图最不该撒的谎）。
    let bar_height = 34.0_f32;

    let mut bars = h_flex()
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.xs)
        .items_end();
    for day in &advice.days {
        let over = day.is_overloaded(capacity);
        let fill = if peak > 0.0 {
            ((day.hours / peak) as f32 * bar_height).clamp(0.0, bar_height)
        } else {
            0.0
        };
        // 「有活但极少」也要看得见：0.2 小时按比例只有 1px，与「这天没活」分不开。
        let fill = if day.hours > 0.0 { fill.max(2.0) } else { 0.0 };
        let color = if over {
            extended.colors.warning
        } else {
            tokens.colors.primary
        };
        let weight = if day.date == today {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };
        let hours_color = if over {
            extended.colors.warning
        } else if day.hours <= 0.0 {
            tokens.colors.muted_foreground
        } else {
            tokens.colors.foreground
        };

        bars = bars.child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    div()
                        .w_full()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .font_features(tabular_numbers())
                        .text_color(hours_color)
                        .child(SharedString::from(if day.hours > 0.0 {
                            hours_text(day.hours)
                        } else {
                            String::new()
                        })),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(bar_height))
                        .flex()
                        .flex_col()
                        .justify_end()
                        .rounded(px(3.))
                        .bg(tokens.colors.muted)
                        .child(div().w_full().h(px(fill)).rounded(px(3.)).bg(color)),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .font_weight(weight)
                        .text_color(if day.date == today {
                            tokens.colors.foreground
                        } else {
                            tokens.colors.muted_foreground
                        })
                        .child(SharedString::from(weekday_label(day.date.weekday(), cx))),
                ),
        );
    }

    // 结论一句一行地说：数字全挤在一行读不过来，而这几句恰恰是这块面板存在的理由。
    let mut lines: Vec<String> = Vec::new();
    lines.push(trf(
        cx,
        "本周排期 {} 小时，每天上限 {} 小时",
        &[&hours_text(advice.scheduled_hours), &hours_text(capacity)],
    ));
    let mut findings: Vec<String> = Vec::new();
    if advice.overloaded_days > 0 {
        findings.push(trf(
            cx,
            "{} 天超出上限",
            &[&advice.overloaded_days.to_string()],
        ));
    }
    if advice.overdue_tasks > 0 {
        findings.push(trf(
            cx,
            "另有 {} 项已逾期，欠 {} 小时",
            &[
                &advice.overdue_tasks.to_string(),
                &hours_text(advice.overdue_hours),
            ],
        ));
    }
    if advice.unplanned_tasks > 0 {
        findings.push(trf(
            cx,
            "{} 项没有截止日，排不进负载",
            &[&advice.unplanned_tasks.to_string()],
        ));
    }
    if advice.unsized_tasks > 0 {
        findings.push(trf(
            cx,
            "{} 项没填预估工时，没算进去",
            &[&advice.unsized_tasks.to_string()],
        ));
    }
    if !findings.is_empty() {
        lines.push(findings.join(" · "));
    }

    let mut text = v_flex().flex_1().min_w_0().gap(px(2.));
    for line in lines {
        text = text.child(
            div()
                .w_full()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(SharedString::from(line))),
        );
    }

    Some(
        h_flex()
            .flex_none()
            .w_full()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.surface)
            .child(div().flex_none().w(px(240.)).child(text))
            .child(bars)
            .into_any_element(),
    )
}

/// 工时的显示文本：整点不带 `.0`（`8` 而不是 `8.0`），半点保留一位（`2.5`）。
///
/// 浮点比较用区间而不是 `==`：`0.1 + 0.2` 那套尾巴在累加里一定会出现。
fn hours_text(hours: f64) -> String {
    if (hours.fract()).abs() < 0.05 {
        format!("{hours:.0}")
    } else {
        format!("{hours:.1}")
    }
}

/// 「计划 N 项 · 已完成 M · 后加 K · 完成率 P%」。
fn plan_summary(stats: super::state::PlanStats, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let text = trf(
        cx,
        "计划 {} 项 · 已完成 {} · 后加 {} · 完成率 {}%",
        &[
            &stats.total.to_string(),
            &stats.done.to_string(),
            &stats.added.to_string(),
            &stats.completion_percent().to_string(),
        ],
    );
    div()
        .flex_none()
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(selectable_text(SharedString::from(text)))
        .into_any_element()
}

/// 一栏任务（标题 + 说明 + 计数 + 可滚动的行）。
///
/// `id` 由调用方给而不是从 `heading` 里拼：`ElementId` 必须**逐元素唯一**，
/// 而两栏的标题都是界面文案，拼出来会随语言变；给静态串最稳。
fn plan_column(
    id: &'static str,
    heading: SharedString,
    hint: SharedString,
    rows: Vec<AnyElement>,
    empty: SharedString,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let count = rows.len();
    let mut body = v_flex().w_full().flex_none().gap(tokens.spacing.xxs);
    if rows.is_empty() {
        body = body.child(
            div()
                .w_full()
                .py(tokens.spacing.xs)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(empty)),
        );
    } else {
        for row in rows {
            body = body.child(row);
        }
    }
    v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .gap(tokens.spacing.xxs)
        .child(
            h_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(caption_heading(heading, cx))
                .child(
                    div()
                        .flex_none()
                        .font_features(tabular_numbers())
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(tokens.colors.muted_foreground)
                        .child(selectable_text(SharedString::from(count.to_string()))),
                ),
        )
        .child(
            div()
                .w_full()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(hint)),
        )
        .child(
            div()
                .id(id)
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .rounded(theme.components().card_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .p(tokens.spacing.xs)
                .child(body),
        )
        .into_any_element()
}

/// 「在计划里 / 这一周到期」两栏里的一行。
///
/// 行内动作按钮必须 `stop_propagation`：否则点「加入」会先把行选中一次
/// （与归档中心的行同一条纪律，见 `archive.rs` 模块头）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanRowAction {
    /// 从「这一周到期」加进计划。
    Add,
    /// 从「在计划里」移出计划。
    Remove,
}

fn plan_task_row(
    task: &Task,
    action: PlanRowAction,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let id = task.id;
    let status = task.status;
    let archived = task.is_archived();
    let done = status == TaskStatus::Done;
    let title = SharedString::from(task.title.clone());
    let action_label = match action {
        PlanRowAction::Add => tr(cx, "加入"),
        PlanRowAction::Remove => tr(cx, "移出"),
    };

    let mut row = h_flex()
        .id(SharedString::from(format!("projects-plan-task-{id}")))
        .flex_none()
        .w_full()
        .min_w_0()
        .gap(tokens.spacing.xs)
        .px(tokens.spacing.xs)
        .py(tokens.spacing.xxs)
        .rounded(tokens.spacing.xxs)
        .cursor_pointer()
        .hover(move |style| style.bg(extended.colors.hover_tint))
        .role(Role::Button)
        .aria_label(title.clone())
        .child(priority_dot(task.priority, cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(if done || archived {
                    tokens.colors.muted_foreground
                } else {
                    tokens.colors.foreground
                })
                .child(selectable_text(title)),
        );
    // 归档的任务仍在计划里（计划是历史事实），但要看得出来它已经归档了。
    if archived {
        row = row.child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(SharedString::from(tr(cx, "已归档")))),
        );
    }
    row.child(
        div()
            .flex_none()
            .text_size(tokens.typography.xs.size)
            .line_height(tokens.typography.xs.line_height)
            .text_color(if done {
                extended.colors.success
            } else {
                tokens.colors.muted_foreground
            })
            .child(selectable_text(SharedString::from(status_label(
                status, cx,
            )))),
    )
    .child(
        icon_button(
            SharedString::from(format!("projects-plan-action-{id}")),
            SharedString::from(action_label),
            Icon::new(match action {
                PlanRowAction::Add => DuckIcon::Plus,
                PlanRowAction::Remove => DuckIcon::X,
            })
            .size(extended.icon.md),
            ButtonVariant::Ghost,
            cx,
        )
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                // 内层动作先掐断传播（见本函数文档）。
                cx.stop_propagation();
                let _ = view.update(cx, |page, cx| match action {
                    PlanRowAction::Add => page.add_task_to_plan(id, cx),
                    PlanRowAction::Remove => page.remove_task_from_plan(id, cx),
                });
            }
        }),
    )
    .on_click({
        let view = view.clone();
        move |event: &ClickEvent, window, cx| {
            if is_double_click(event) {
                let _ = view.update(cx, |page, cx| {
                    super::dialogs::open_task_editor(page, Some(id), None, status, None, window, cx)
                });
                return;
            }
            let _ = view.update(cx, |page, cx| page.select_task(Some(id), cx));
        }
    })
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 月视图恒 6 行 × 7 列、周视图恒一行 —— 两条一起钉。
    ///
    /// 月视图为什么恒 6 行见 [`CALENDAR_CELLS`] 的注释（行数随月份变会让网格高度跳）；
    /// 这里钉的是「格数 / 行数 / 高度」三件必须一起改的东西：只改格数会把网格改成
    /// 「5 行 + 7 格」这种既不整除、也画不满的排法，而它在界面上只会表现为
    /// 「最后一行少了一块」，肉眼很难联想到是常量的问题。
    #[test]
    fn the_layout_matches_each_calendar_mode() {
        let month = CalendarLayout::of(CalendarMode::Month);
        assert_eq!(month.cells % 7, 0, "格数必须是整行");
        assert_eq!(month.cells / 7, 6, "月视图恒 6 行");
        assert!(month.height.is_some(), "月视图每格定高，翻页时网格不跳");

        let week = CalendarLayout::of(CalendarMode::Week);
        assert_eq!(week.cells, 7, "周视图只有一行");
        assert!(week.height.is_none(), "周视图的格子由行高决定（撑满整块）");
    }

    /// 周视图的格子比月视图高得多，能列的条数也必须更多。
    ///
    /// 反过来说：如果两档容量相同，周视图就只剩「格子变大」，把「一天看得到多少事」
    /// 这半件好处丢了 —— 而这一条是几何常量（`CALENDAR_WEEK_DETAIL`）在管，改错了
    /// 界面上只会显得「周视图也没多显示什么」，不像 bug。
    #[test]
    fn the_week_grid_shows_more_tasks_per_day() {
        let month = CalendarLayout::of(CalendarMode::Month).detail;
        let week = CalendarLayout::of(CalendarMode::Week).detail;
        assert!(week > month, "周视图每格 {week} 条 vs 月视图 {month} 条");
    }

    /// 排期面板的工时文本：整点不带 `.0`、半点留一位、浮点尾巴不显形。
    ///
    /// 这一条钉的是**显示**而不是计算：`schedule::analyze` 已经把每天四舍五入到一位
    /// 小数，但七天相加又会长出 `15.500000000000002` 这种尾巴；直接 `{}` 打到界面上，
    /// 用户看到的是「本周排期 15.500000000000002 小时」。
    #[test]
    fn hours_text_keeps_at_most_one_decimal() {
        assert_eq!(hours_text(8.0), "8");
        assert_eq!(hours_text(2.5), "2.5");
        assert_eq!(hours_text(0.0), "0");
        // `0.1 + 0.2` 的尾巴：落在半点的区间里，四舍五入后仍是 `0.3`。
        assert_eq!(hours_text(0.1 + 0.2), "0.3");
        assert_eq!(hours_text(15.500_000_000_000_002), "15.5");
    }
}
