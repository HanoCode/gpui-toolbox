//! 工具箱页：左侧「分类 → 工具」两级导航，右侧路由标签页与工具工作区。
//!
//! 交互模型：
//! - 侧栏分类是主菜单，工具是挂在分类下的子菜单；点分类折叠 / 展开，点工具打开路由。
//! - 顶部是**路由标签页**：每个打开的工具一个标签，支持右键菜单（关闭 / 关闭其他 /
//!   关闭全部）、中键关闭；工作区滚动位置跟着路由走而不是页面级单例——切走再切
//!   回来，上一个工作区原样保留。
//! - 帮助与设置放在**右侧栏**（顶栏尾随插槽右侧的图标开关，所有路由常显，
//!   布局与开合由 shell 统一管理）：工具信息、启用开关、帮助章节与设置项摘要
//!   合并为单列，不再用 tab 切换，按活跃工具动态渲染；数据库 / AI 助手等路由
//!   落地时各自定制内容。
//! - 没有任何打开路由时内容区给空状态引导。

use std::collections::HashSet;

use duck_components::content::{
    accordion, description_list, description_row, empty_state, neutral_tag, status_cell,
};
use duck_components::{
    ButtonVariant, DuckIcon, SidebarPanel, SidebarState, beta_badge, body_text, button,
    icon_button, meta_text, notify, option_row, selectable_text, settings_section, switch_control,
    tabular_numbers,
};
use duck_shell::left_edge_probe;
use duck_theme::{active_theme, t, tr, trf};
use duck_tools::paste::detect;
use duck_tools::settings as tool_settings;
use duck_tools::settings::{SettingValue, ToolSettingKind, ToolSettingSpec};
use duck_tools::{
    HelpSection, PasteInbox, PendingPaste, RegisteredTool, ToolCategory, ToolQuery, ToolRegistry,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement, Pixels, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon, h_flex,
    menu::{ContextMenuExt as _, PopupMenuItem},
    v_flex,
};

use crate::pages::open_settings_at;
use crate::pages::settings::{CONTENT_PADDING_LEFT, CONTENT_PADDING_RIGHT};
use crate::pages::status_support::{disk_free_bytes, format_bytes};

/// 工具箱侧栏默认宽度（px）：参考项目 `w-60` = 240px。
const SIDEBAR_DEFAULT_WIDTH: f32 = 240.;
/// 侧栏可拖拽的宽度范围（px）。
const SIDEBAR_MIN_WIDTH: f32 = 184.;
const SIDEBAR_MAX_WIDTH: f32 = 320.;

/// 路由标签页的最大宽度（名称超出截断，防长名挤掉关闭按钮）。
const ROUTE_TAB_MAX_WIDTH: f32 = 160.;

/// 侧栏工具行的左缩进（参考项目 `NavItem` 的 `pl-10` = 40px）：
/// 比分组标题的图标再进一级，形成「标题 ／ 子项」的层级。
const SIDEBAR_TOOL_INDENT: f32 = 40.;

/// 「最近使用」最多列几条（参考项目也是短列表，多了就失去「最近」的意义）。
const RECENT_LIMIT: usize = 6;

/// 一个打开的工具路由（一个标签页）。
///
/// 工作区状态跟着路由而不是页面级单例：切走再切回来，上一个工作区
/// （滚动位置）原样保留——这是本次交互的核心承诺。
struct ToolRoute {
    /// 工具稳定 id（标签页 `ElementId` 与关闭目标都以它为准）。
    tool_id: &'static str,
    /// 工作区滚动位置。内容只在激活时渲染，但偏移存在句柄里，
    /// 切回来时由 `track_scroll` 恢复。
    scroll: ScrollHandle,
    /// 插件 io 工具的通用执行页（声明式 `[io]` 才建，懒创建）。
    /// 状态（填过的参数、上次结果）跟着路由走：切走再切回来原样保留。
    run: Option<Entity<crate::pages::tool_run::ToolRunView>>,
}

/// 工具渲染快照（侧栏子项、工作区与右侧栏共用）。
///
/// 渲染时先把注册表 + 持久化状态打成快照，避免在渲染里长期借用 `App`
/// （渲染需要 `&mut Context`，两者不能同时持有）。后四个字段只有工作区与右侧栏读。
struct ToolRow {
    /// 工具稳定 id（`ElementId` 与路由目标都以它为准）。
    id: &'static str,
    /// 展示名（中文源串）。
    name: &'static str,
    /// 图标。
    icon: DuckIcon,
    /// 是否试验功能（显示 BETA 角标）。
    beta: bool,
    /// 当前是否启用。
    enabled: bool,
    /// 累计使用次数。
    runs: u64,
    /// 所属分类。
    category: ToolCategory,
    /// 二级分组（可选）。
    group: Option<&'static str>,
    /// 附加搜索词（中英）。
    keywords: &'static [&'static str],
    /// 帮助内容。
    help: &'static [HelpSection],
    /// 工具级设置 schema。
    settings: &'static [ToolSettingSpec],
}

/// 把一条注册记录摊平成渲染快照。
fn snapshot(tool: &RegisteredTool, cx: &App) -> ToolRow {
    let meta = tool.meta();
    ToolRow {
        id: tool.id(),
        name: meta.name(),
        icon: meta.icon(),
        beta: meta.is_beta(),
        enabled: tool_settings::is_enabled(cx, tool.id())
            && crate::plugins::tool_enabled(cx, tool.id()),
        runs: tool_settings::runs(cx, tool.id()),
        category: meta.category(),
        group: meta.group(),
        keywords: meta.keywords(),
        help: meta.help(),
        settings: meta.settings(),
    }
}

/// 取某个工具的快照；未注册或未列出（如开发者模式关闭后的 dev 工具）返回 `None`。
fn snapshot_of(id: &str, cx: &App) -> Option<ToolRow> {
    ToolRegistry::get(cx, id)
        .filter(|tool| ToolRegistry::is_listed(cx, tool))
        .map(|tool| snapshot(tool, cx))
}

/// 侧栏分组标题行（参考项目 `NavSection` 的按钮）：
/// 折叠箭头（12px，展开时朝下）+ 图标（14px）+ 名称（14px / 中等字重），
/// 右侧是可选的计数角标。参 `cx` 只为取主题，交互由调用方挂。
fn sidebar_section_header(
    id: SharedString,
    label: SharedString,
    icon: DuckIcon,
    count: Option<usize>,
    expanded: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let chevron = if expanded {
        DuckIcon::ChevronDown
    } else {
        DuckIcon::ChevronRight
    };
    h_flex()
        .id(id)
        .h(theme.density().nav_row)
        .w_full()
        .flex_none()
        .items_center()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.md)
        .rounded(theme.components().nav_item_radius)
        .cursor_pointer()
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .font_weight(FontWeight::MEDIUM)
        .text_color(tokens.colors.muted_foreground)
        .hover(|style| style.bg(extended.colors.nav_hover))
        // 箭头在**最左边**（参考项目如此），展开时朝下、收起时朝右。
        .child(
            Icon::new(chevron)
                .size(extended.icon.sm)
                .text_color(extended.colors.text_tertiary),
        )
        // 图标与文字同色（参考项目用 `currentColor`）。
        .child(
            Icon::new(icon)
                .size(extended.icon.md)
                .text_color(tokens.colors.muted_foreground),
        )
        .child(div().flex_1().min_w_0().truncate().child(label))
        .when_some(count, |row, count| {
            row.child(sidebar_count_badge(count, cx))
        })
}

/// 分组标题右侧的计数角标（参考项目 `NavSection` 的 `count`）。
fn sidebar_count_badge(count: usize, cx: &App) -> gpui::Div {
    let theme = active_theme(cx);
    let extended = theme.extended();
    div()
        .flex_none()
        .px(px(4.))
        .rounded(theme.components().badge_radius)
        .bg(extended.colors.nav_hover)
        .text_size(extended.badge.size)
        .line_height(extended.badge.line_height)
        .text_color(extended.colors.text_tertiary)
        .child(count.to_string())
}

/// 空分组提示（参考项目「暂无收藏」/「暂无记录」那一行）。
fn sidebar_hint(text: SharedString, cx: &App) -> gpui::Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .w_full()
        .px(tokens.spacing.md)
        .py(tokens.spacing.xs)
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(theme.extended().colors.text_tertiary)
        .child(text)
}

/// 分组之间的分隔线（参考项目 `my-2 mx-3 h-px`）。
fn sidebar_divider(cx: &App) -> gpui::Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .flex_none()
        .h(px(1.))
        .mx(tokens.spacing.md)
        .my(tokens.spacing.xs)
        .bg(theme.extended().colors.hairline)
}

/// 侧栏工具行（参考项目 `NavItem`）：
/// 图标（14px，未选中用最弱文字色）+ 名称（12px，截断）+ BETA 角标 + 收藏星标。
///
/// 星标默认 `opacity(0)`、悬停整行时浮现（参考项目 `group-hover:opacity-100`），
/// 已收藏则恒亮；点击星标只切收藏、不打开工具（`stop_propagation`）。
fn sidebar_tool_row(
    view: &gpui::WeakEntity<ToolboxPage>,
    row: &ToolRow,
    label: SharedString,
    selected: bool,
    favorite: bool,
    show_star: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let group = SharedString::from(format!("toolbox-row-{}", row.id));
    let star_group = group.clone();
    let icon_color = if selected {
        extended.colors.nav_selected_icon
    } else {
        extended.colors.text_tertiary
    };
    let star_color = if favorite {
        extended.colors.warning
    } else {
        extended.colors.text_tertiary
    };

    let mut el = h_flex()
        .id(SharedString::from(format!("toolbox-tool-{}", row.id)))
        // 悬停组名逐行唯一：星标的显隐跟着**这一行**，不能串到邻行。
        .group(group)
        .h(theme.density().nav_row)
        .w_full()
        .flex_none()
        .items_center()
        .gap(tokens.spacing.sm)
        .pl(px(SIDEBAR_TOOL_INDENT))
        .pr(tokens.spacing.md)
        .rounded(theme.components().nav_item_radius)
        .cursor_pointer()
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(if selected {
            extended.colors.nav_selected_foreground
        } else {
            tokens.colors.muted_foreground
        });
    el = if selected {
        el.bg(extended.colors.nav_selected)
    } else {
        el.hover(|style| style.bg(extended.colors.nav_hover))
    };
    el = el
        .child(
            Icon::new(row.icon)
                .size(extended.icon.md)
                .text_color(icon_color),
        )
        .child(div().flex_1().min_w_0().truncate().child(label));
    if row.beta {
        el = el.child(beta_badge(cx));
    }
    if show_star {
        el = el.child(
            h_flex()
                .id(SharedString::from(format!("toolbox-fav-{}", row.id)))
                .flex_none()
                .items_center()
                .gap(px(2.))
                // 未收藏时整块透明、悬停浮现；已收藏恒亮（参考项目同款处理）。
                .opacity(if favorite { 1. } else { 0. })
                .group_hover(star_group, |style| style.opacity(1.))
                .child(
                    Icon::new(DuckIcon::Star)
                        .size(extended.icon.sm)
                        .text_color(star_color),
                )
                .child(
                    div()
                        .text_size(extended.badge.size)
                        .line_height(extended.badge.line_height)
                        .text_color(star_color)
                        .child(tr(cx, if favorite { "已收藏" } else { "收藏" })),
                )
                .on_click({
                    let view = view.clone();
                    let tool_id = row.id;
                    move |_, _, cx| {
                        cx.stop_propagation();
                        ToolRegistry::toggle_favorite(cx, tool_id);
                        if let Ok(()) = view.update(cx, |_, cx| cx.notify()) {}
                    }
                }),
        );
    }
    el.on_click({
        let view = view.clone();
        let tool_id = row.id;
        move |_, _, cx| {
            if let Ok(()) = view.update(cx, |this, cx| {
                this.open_route(tool_id, cx);
            }) {}
        }
    })
}

/// 工具箱页。
pub struct ToolboxPage {
    sidebar: Entity<SidebarState>,
    /// 打开的路由（顺序即标签页顺序）。
    routes: Vec<ToolRoute>,
    /// 激活路由的工具 id；`routes` 非空时必有一些。
    active: Option<&'static str>,
    /// 已折叠的分类（默认**全部折叠**，点标题展开）。
    collapsed: HashSet<ToolCategory>,
    /// 「收藏夹」分组是否折叠（默认折叠，照参考项目）。
    favorites_collapsed: bool,
    /// 「最近使用」分组是否折叠（默认折叠，照参考项目）。
    recent_collapsed: bool,
    /// 内容区左缘的窗口横坐标（上一帧测得），供顶栏搜索框对齐。
    content_left: Pixels,
}

impl ToolboxPage {
    /// 创建页面：默认展开 240px 侧栏；分类**默认全部折叠**——首屏只列一级分类，
    /// 点标题展开二级工具行（与「收藏夹 / 最近使用」两个分组的默认态一致）。
    /// 右侧栏由 shell 统一持有（顶栏开关常显），本页只提供内容。
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            sidebar: cx.new(|cx| {
                SidebarState::new(
                    px(SIDEBAR_DEFAULT_WIDTH),
                    px(SIDEBAR_MIN_WIDTH)..px(SIDEBAR_MAX_WIDTH),
                    cx,
                )
            }),
            routes: Vec::new(),
            active: None,
            // 分类默认**全部折叠**：侧栏首屏只有一级标题，点开才见工具。
            collapsed: ToolCategory::ALL.iter().copied().collect(),
            favorites_collapsed: true,
            recent_collapsed: true,
            content_left: px(0.),
        }
    }

    /// 内容区左缘的窗口横坐标（顶栏搜索框对齐用）。
    pub(crate) fn content_left(&self) -> Pixels {
        self.content_left
    }

    /// 供 shell 顶栏共享的侧栏状态。
    #[must_use]
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
    }

    /// 展开 / 折叠一个分类。
    fn toggle_expanded(&mut self, category: ToolCategory, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&category) {
            self.collapsed.insert(category);
        }
        cx.notify();
    }

    /// 打开某个工具的路由：已有则只激活（保留其工作区状态），没有则新建。
    ///
    /// 顺手把该工具所属的分类展开（用户可能手动收起过），别让侧栏选中项藏在
    /// 折叠区里。命令面板「打开工具」也走这里（见 `app.rs` 的 `OpenTool` 处理器），
    /// 保证两条入口行为完全一致。
    pub fn open_route(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if let Some(tool) = snapshot_of(id, cx) {
            self.collapsed.remove(&tool.category);
        }
        if !self.routes.iter().any(|route| route.tool_id == id) {
            self.routes.push(ToolRoute {
                tool_id: id,
                scroll: ScrollHandle::new(),
                run: None,
            });
        }
        // 「最近使用」分组的唯一写入点：**打开工具**（不是执行）。
        // 参考项目同样是「选中即记一条」。用 `mark_used` 而不是 `record_use`：
        // 后者会顺带累加使用次数，而这里只是打开了标签、没真跑。
        ToolRegistry::mark_used(cx, id);
        self.activate(id, cx);
    }

    /// 激活某个路由（保留其工作区状态）。
    fn activate(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.active != Some(id) {
            self.active = Some(id);
            cx.notify();
        }
    }

    /// 为激活路由懒建插件 io 工具的执行页（`Render::render` 入口调用）。
    ///
    /// `ToolRunView` 的输入框状态要 `&mut Window` 才能建，而渲染层只有这里
    /// 拿得到窗口，所以在页面渲染最前面补建。非插件 io 工具（`run_context`
    /// 返回 `None`）保持 `None`，继续走占位画布。
    fn ensure_run_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self
            .active
            .or_else(|| self.routes.last().map(|route| route.tool_id));
        let Some(active) = active else {
            return;
        };
        let Some(route) = self.routes.iter_mut().find(|route| route.tool_id == active) else {
            return;
        };
        if route.run.is_some() {
            return;
        }
        let Some(ctx) = crate::plugins::run_context(cx, active) else {
            return;
        };
        route.run = Some(cx.new(|cx| crate::pages::tool_run::ToolRunView::new(ctx, window, cx)));
    }

    /// 关闭一个路由；关的是激活路由时，优先落到原位置的下一个标签，
    /// 没有就退到上一个（此时即最后一个）。
    fn close_route(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(index) = self.routes.iter().position(|route| route.tool_id == id) else {
            return;
        };
        self.routes.remove(index);
        if self.active == Some(id) {
            self.active = self
                .routes
                .get(index)
                .or_else(|| self.routes.last())
                .map(|route| route.tool_id);
        }
        cx.notify();
    }

    /// 关闭除指定路由外的全部。
    fn close_others(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.routes.retain(|route| route.tool_id == id);
        self.active = Some(id);
        cx.notify();
    }

    /// 关闭全部路由（内容区回到空状态引导）。
    fn close_all(&mut self, cx: &mut Context<Self>) {
        self.routes.clear();
        self.active = None;
        cx.notify();
    }

    /// 左侧导航，**布局与参考项目 `Sidebar.vue` 一一对应**：
    /// 收藏夹 → 最近使用 → 分隔线 → 各分类（一级标题 + 二级工具行）。
    ///
    /// 几条照抄参考项目的口径：
    /// - 三个分组都可折叠；收藏夹 / 最近使用**默认折叠**（参考仓
    ///   `favCollapsed = ref(true)` / `recentCollapsed = ref(true)`）；
    /// - 分类同样默认折叠：首屏只有一级菜单，点开才见工具行；
    /// - 分组标题 `px-3` + 14px 中等字重、左侧折叠箭头；工具行缩进 `pl-10`（40px）
    ///   + 12px 常规字重（Tailwind `px-3` / `text-sm` / `pl-10` / `text-xs` 直译）；
    /// - 收藏星标默认透明、悬停行时浮现（`group-hover:opacity-100`），已收藏恒亮；
    /// - 顶栏搜索词非空时进入过滤形态：只显示命中的工具，分组与折叠态忽略。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let lang = theme.lang();
        let query = ToolQuery::get(cx);
        let searching = !query.trim().is_empty();

        let categories = ToolRegistry::present_categories(cx);
        let view = cx.entity().downgrade();

        // 分组间距沿用「设置 → 外观 → 布局 → 侧栏项间距」（0 ~ 24px）。
        // `Pixels` 是 Copy，取值后不延长对 `theme` 的借用。
        let nav_gap = theme.appearance().nav_gap;
        let mut nav = v_flex().w_full().gap(px(nav_gap));
        let mut any_match = false;

        // ── 收藏夹 / 最近使用 ──
        // 只在非搜索态出现：参考项目的搜索走独立弹窗，侧栏不参与过滤；
        // 我们的搜索框在顶栏，过滤时把这两组藏掉，避免「命中了什么」说不清。
        if !searching {
            let favorites: Vec<&RegisteredTool> = ToolRegistry::favorites(cx);
            let mut section = v_flex().w_full().gap(px(2.));
            section = section.child(
                sidebar_section_header(
                    SharedString::from("toolbox-section-favorites"),
                    SharedString::from(tr(cx, "收藏夹")),
                    DuckIcon::Star,
                    Some(favorites.len()),
                    !self.favorites_collapsed,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        if let Ok(()) = view.update(cx, |this, cx| {
                            this.favorites_collapsed = !this.favorites_collapsed;
                            cx.notify();
                        }) {}
                    }
                }),
            );
            if !self.favorites_collapsed && favorites.is_empty() {
                section = section.child(sidebar_hint(SharedString::from(tr(cx, "暂无收藏")), cx));
            } else if !self.favorites_collapsed {
                for tool in favorites {
                    let row = snapshot(tool, cx);
                    section = section.child(sidebar_tool_row(
                        &view,
                        &row,
                        SharedString::from(crate::plugins::plugin_text_of_tool(
                            cx, lang, row.id, row.name,
                        )),
                        self.active == Some(row.id),
                        // 收藏夹里的每一条本身就是收藏，但参考项目这一组**不画星标**
                        // （`NavItem` 没传 `show-fav`）—— 去收藏得回分类里点。
                        false,
                        false,
                        cx,
                    ));
                }
            }
            nav = nav.child(section);

            let recent: Vec<&RegisteredTool> = ToolRegistry::recent(cx, RECENT_LIMIT)
                .into_iter()
                .filter(|tool| ToolRegistry::is_listed(cx, tool))
                .collect();
            let mut section = v_flex().w_full().gap(px(2.));
            section = section.child(
                sidebar_section_header(
                    SharedString::from("toolbox-section-recent"),
                    SharedString::from(tr(cx, "最近使用")),
                    DuckIcon::Clock,
                    Some(recent.len()),
                    !self.recent_collapsed,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        if let Ok(()) = view.update(cx, |this, cx| {
                            this.recent_collapsed = !this.recent_collapsed;
                            cx.notify();
                        }) {}
                    }
                }),
            );
            if !self.recent_collapsed {
                if recent.is_empty() {
                    section =
                        section.child(sidebar_hint(SharedString::from(tr(cx, "暂无记录")), cx));
                } else {
                    for tool in recent {
                        let row = snapshot(tool, cx);
                        section = section.child(sidebar_tool_row(
                            &view,
                            &row,
                            SharedString::from(crate::plugins::plugin_text_of_tool(
                                cx, lang, row.id, row.name,
                            )),
                            self.active == Some(row.id),
                            ToolRegistry::is_favorite(cx, row.id),
                            true,
                            cx,
                        ));
                    }
                }
            }
            nav = nav.child(section);
            nav = nav.child(sidebar_divider(cx));
        }

        // ── 分类（一级标题 + 二级工具行）──
        for category in &categories {
            let tools_in_category = ToolRegistry::in_category(cx, Some(*category));
            let tools = if searching {
                duck_tools::search(&tools_in_category, &query)
            } else {
                tools_in_category
            };
            if searching && tools.is_empty() {
                continue;
            }
            any_match |= !tools.is_empty();

            let expanded = searching || !self.collapsed.contains(category);
            let category_id = *category;

            let mut section = v_flex().w_full().gap(px(2.));
            section = section.child(
                sidebar_section_header(
                    SharedString::from(format!("toolbox-category-{}", category.id())),
                    SharedString::from(t(lang, category.label())),
                    category.icon(),
                    None,
                    expanded,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        if let Ok(()) = view.update(cx, |this, cx| {
                            this.toggle_expanded(category_id, cx);
                        }) {}
                    }
                }),
            );

            if expanded {
                for tool in tools {
                    let row = snapshot(tool, cx);
                    section = section.child(sidebar_tool_row(
                        &view,
                        &row,
                        // 插件工具走「插件表 → 宿主表 → 源串」；内置工具直接走宿主表。
                        SharedString::from(crate::plugins::plugin_text_of_tool(
                            cx, lang, row.id, row.name,
                        )),
                        self.active == Some(row.id),
                        ToolRegistry::is_favorite(cx, row.id),
                        true,
                        cx,
                    ));
                }
            }
            nav = nav.child(section);
        }

        if searching && !any_match {
            nav = nav.child(
                div()
                    .w_full()
                    .py(tokens.spacing.lg)
                    .flex()
                    .justify_center()
                    .child(meta_text(tr(cx, "没有匹配的工具。"), cx)),
            );
        }

        v_flex()
            .size_full()
            .min_h_0()
            .flex_col()
            .bg(extended.colors.chrome)
            .child(
                div()
                    .id("toolbox-sidebar-nav")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    // 参考项目 `px-2 py-3`：左右 8、上下 12。
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.md)
                    .child(nav),
            )
    }

    /// 路由标签页：每个打开的工具一个标签（图标 + 名称 + 关闭按钮），
    /// 激活标签底部一条主色下划线。支持右键菜单与中键关闭。
    fn render_route_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let lang = theme.lang();
        let view = cx.entity().downgrade();

        // 右键菜单文案在渲染期取好（闭包里只有 `Context<PopupMenu>`，但菜单
        // 只该说当前语言的一句话，取词留在渲染里更直白）。
        let close_label = SharedString::from(tr(cx, "关闭"));
        let close_others_label = SharedString::from(tr(cx, "关闭其他"));
        let close_all_label = SharedString::from(tr(cx, "关闭全部"));
        let close_tab_label = SharedString::from(tr(cx, "关闭标签页"));

        let mut tabs_row = h_flex().min_w_0().gap(tokens.spacing.xxs);
        for route in &self.routes {
            // 工具被移除（或 dev 过滤）时标签退化成裸 id，至少还能关掉它。
            let (tool_icon, tool_name, tool_beta) = match snapshot_of(route.tool_id, cx) {
                Some(tool) => (
                    tool.icon,
                    SharedString::from(crate::plugins::plugin_text_of_tool(
                        cx,
                        lang,
                        route.tool_id,
                        tool.name,
                    )),
                    tool.beta,
                ),
                None => (DuckIcon::Box, SharedString::from(route.tool_id), false),
            };
            let active = self.active == Some(route.tool_id);
            let id = route.tool_id;

            let (foreground, weight) = if active {
                (tokens.colors.foreground, FontWeight::MEDIUM)
            } else {
                (tokens.colors.muted_foreground, FontWeight::NORMAL)
            };

            let tab = h_flex()
                .id(SharedString::from(format!("toolbox-route-{id}")))
                .relative()
                .h(theme.density().control)
                .min_w_0()
                .max_w(px(ROUTE_TAB_MAX_WIDTH))
                .flex_none()
                .items_center()
                .gap(tokens.spacing.xs)
                .px(tokens.spacing.sm)
                .rounded(tokens.radius.md)
                .cursor_pointer()
                .text_color(foreground)
                .hover(move |style| {
                    style
                        .bg(extended.colors.hover_tint)
                        .text_color(extended.colors.accent_text)
                })
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        if let Ok(()) = view.update(cx, |this, cx| this.activate(id, cx)) {}
                    }
                })
                // 中键关闭：与浏览器标签一致。
                .on_mouse_down(MouseButton::Middle, {
                    let view = view.clone();
                    move |_, _, cx| {
                        if let Ok(()) = view.update(cx, |this, cx| this.close_route(id, cx)) {}
                    }
                })
                .child(
                    Icon::new(tool_icon)
                        .size(extended.icon.sm)
                        .flex_none()
                        .text_color(if active {
                            extended.colors.nav_selected_icon
                        } else {
                            tokens.colors.muted_foreground
                        }),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .font_weight(weight)
                        .child(tool_name),
                )
                .when(tool_beta, |tab| tab.child(beta_badge(cx)))
                .child(
                    div()
                        .flex_none()
                        // 拦下左键冒泡：点关闭不该同时也激活标签。
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            icon_button(
                                SharedString::from(format!("toolbox-route-close-{id}")),
                                close_tab_label.clone(),
                                Icon::new(DuckIcon::X)
                                    .size(extended.icon.sm)
                                    .text_color(tokens.colors.muted_foreground),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    if let Ok(()) =
                                        view.update(cx, |this, cx| this.close_route(id, cx))
                                    {
                                    }
                                }
                            }),
                        ),
                )
                .when(active, |tab| {
                    tab.child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            // 选中标签的下缘指示条：2px 视觉标记（不是间距步进），
                            // 全仓仅此一处、粗细应保持恒定，故不用 spacing 档。
                            .h(px(2.))
                            .rounded_full()
                            .bg(tokens.colors.primary),
                    )
                })
                .context_menu({
                    // 每次迭代都要一份自己的标签（闭包被 `OnceCell` 持有，跨迭代移动）。
                    let close_label = close_label.clone();
                    let close_others_label = close_others_label.clone();
                    let close_all_label = close_all_label.clone();
                    let view = view.clone();
                    move |menu, _, _| {
                        menu.item(PopupMenuItem::new(close_label.clone()).on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                if let Ok(()) = view.update(cx, |this, cx| this.close_route(id, cx))
                                {
                                }
                            }
                        }))
                        .item(PopupMenuItem::new(close_others_label.clone()).on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                if let Ok(()) =
                                    view.update(cx, |this, cx| this.close_others(id, cx))
                                {
                                }
                            }
                        }))
                        .separator()
                        .item(
                            PopupMenuItem::new(close_all_label.clone()).on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    if let Ok(()) = view.update(cx, |this, cx| this.close_all(cx)) {
                                    }
                                }
                            }),
                        )
                    }
                });
            tabs_row = tabs_row.child(tab);
        }

        h_flex()
            .id("toolbox-route-tabbar")
            .w_full()
            .flex_none()
            .min_w_0()
            .items_end()
            .gap(tokens.spacing.sm)
            .pl(px(CONTENT_PADDING_LEFT))
            .pr(px(CONTENT_PADDING_RIGHT))
            .pt(tokens.spacing.xs)
            .bg(tokens.colors.surface)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            // 标签行自己横向滚动；智能粘贴入口钉在行尾，不随滚动走。
            .child(
                h_flex()
                    .id("toolbox-route-tabs-scroll")
                    .flex_1()
                    .min_w_0()
                    .items_end()
                    .overflow_x_scroll()
                    .gap(tokens.spacing.sm)
                    .child(tabs_row),
            )
            .child(smart_paste_button(cx))
            .into_any_element()
    }

    /// 没有任何打开路由时的空状态引导。
    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(
                empty_state(
                    DuckIcon::Layers,
                    tr(cx, "从左侧选择工具"),
                    Some(SharedString::from(tr(
                        cx,
                        "打开的工具会以标签页保留在这里。",
                    ))),
                    // 空态下没有标签栏，智能粘贴入口挪进引导区。
                    Some(smart_paste_button(cx).into_any_element()),
                    cx,
                )
                .border_0(),
            )
            .into_any_element()
    }

    /// 主内容：智能粘贴横幅 + 空状态 / 路由标签页 + 激活的工作区。
    fn render_main(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut main = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .children(self.render_paste_banner(cx));
        if self.routes.is_empty() {
            main = main.child(self.render_empty_state(cx));
        } else {
            main = main
                .child(self.render_route_tabs(cx))
                .child(self.render_active_workspace(cx));
        }
        main
    }

    /// 激活路由的工作区：操作区域（帮助与设置在右侧栏）。
    ///
    /// 工具可能被移除或不再列出（dev 过滤）：给一句解释的空态，路由本身保留，
    /// 用户随时可以关掉这个标签。
    fn render_active_workspace(&self, cx: &mut Context<Self>) -> AnyElement {
        let route = self
            .routes
            .iter()
            .find(|route| Some(route.tool_id) == self.active)
            .or_else(|| self.routes.first());
        let Some(route) = route else {
            return self.render_empty_state(cx);
        };
        let scroll = route.scroll.clone();
        let tool_id = route.tool_id;

        let Some(tool) = snapshot_of(tool_id, cx) else {
            return v_flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .child(
                    empty_state(
                        DuckIcon::CircleAlert,
                        tr(cx, "工具不可用"),
                        Some(SharedString::from(tr(
                            cx,
                            "该工具已被移除，或开发者模式已关闭。",
                        ))),
                        None,
                        cx,
                    )
                    .border_0(),
                )
                .into_any_element();
        };

        self.render_workspace_body(
            &tool,
            &scroll,
            self.workspace_canvas_for(route, &tool, cx),
            cx,
        )
    }

    /// 工作区画布分流：插件 io 工具走通用执行页（状态挂在路由上），
    /// 其余工具走占位说明 + 运行入口。
    fn workspace_canvas_for(
        &self,
        route: &ToolRoute,
        tool: &ToolRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(view) = &route.run {
            return view.clone().into_any_element();
        }
        self.render_workspace_canvas(tool, cx)
    }

    /// 工作区内容区：该工具的操作区域，滚动容器挂在路由自己的句柄上，
    /// 切走再切回来偏移原样恢复。
    fn render_workspace_body(
        &self,
        tool: &ToolRow,
        scroll: &ScrollHandle,
        canvas: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = active_theme(cx).tokens();

        div()
            .id(SharedString::from(format!(
                "toolbox-workspace-body-{}",
                tool.id
            )))
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(scroll)
            .pl(px(CONTENT_PADDING_LEFT))
            .pr(px(CONTENT_PADDING_RIGHT))
            .pt(tokens.spacing.md)
            .pb(tokens.spacing.xl)
            .child(canvas)
            .into_any_element()
    }

    /// 工具的操作区域（帮助与设置在右侧栏）。
    ///
    /// 后端尚未接入时给占位说明 + 运行入口（标题区已移除，运行收敛到操作区）；
    /// 各工具后端就绪后，这里替换成自己的操作界面（届时操作状态同样挂在
    /// [`ToolRoute`] 里，跟路由保持）。停用的工具不给运行入口——先在右侧栏打开开关。
    fn render_workspace_canvas(&self, tool: &ToolRow, cx: &mut Context<Self>) -> AnyElement {
        let id = tool.id;
        let run = tool.enabled.then(|| {
            button(
                SharedString::from(format!("toolbox-workspace-run-{id}")),
                tr(cx, "运行"),
                ButtonVariant::Primary,
                cx,
            )
            .on_click(cx.listener(move |_, _, window, cx| {
                if ToolRegistry::invoke(id, window, cx) {
                    cx.notify();
                }
            }))
            .into_any_element()
        });

        v_flex()
            .size_full()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(
                empty_state(
                    tool.icon,
                    tr(cx, "工作区"),
                    Some(SharedString::from(tr(
                        cx,
                        "该工具的操作区域尚未接入，后端就绪后在此展示。",
                    ))),
                    run,
                    cx,
                )
                .border_0(),
            )
            .into_any_element()
    }

    /// 右侧栏内容：工具信息、关键词、启用开关、帮助章节与设置项摘要合并为
    /// 单列（无 tab），纵向滚动。
    ///
    /// 右侧栏的布局与开合由 shell 统一管理（每条路由都有，顶栏开关常显），
    /// 本页只通过 `with_right_sidebar` 提供这个内容闭包，按活跃工具动态渲染；
    /// 数据库 / AI 助手等路由落地时各自定制自己的内容。没有打开路由时给一句
    /// 占位提示。
    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        let tool = self
            .routes
            .iter()
            .find(|route| Some(route.tool_id) == self.active)
            .or_else(|| self.routes.first())
            .and_then(|route| snapshot_of(route.tool_id, cx));
        let Some(tool) = tool else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px(px(CONTENT_PADDING_LEFT))
                .child(meta_text(
                    tr(cx, "从左侧选择工具后，这里显示该工具的帮助与设置。"),
                    cx,
                ))
                .into_any_element();
        };

        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let lang = theme.lang();
        let id = tool.id;
        let status_color = if tool.enabled {
            extended.colors.status_completed
        } else {
            extended.colors.status_paused
        };

        let mut info = vec![description_row(
            t(lang, "分类"),
            body_text(t(lang, tool.category.label()), cx),
        )];
        if let Some(group) = tool.group {
            info.push(description_row(
                t(lang, "分组"),
                body_text(t(lang, group), cx),
            ));
        }
        info.push(description_row(
            t(lang, "使用次数"),
            body_text(tool.runs.to_string(), cx),
        ));
        info.push(description_row(
            t(lang, "状态"),
            status_cell(
                status_color,
                t(
                    lang,
                    if tool.enabled {
                        "已启用"
                    } else {
                        "已停用"
                    },
                ),
                cx,
            ),
        ));

        // 上游 `DescriptionList` 自身只有 4px 行内边距，直接贴卡片内壁；
        // 这里补上与 `option_row` 一致的 `px(lg) / py(sm)`，卡片内壁才对齐主题规格。
        let info_card = div()
            .w_full()
            .px(tokens.spacing.lg)
            .py(tokens.spacing.sm)
            .child(description_list(info));

        let mut body = v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(settings_section(
                Some(tr(cx, "工具信息").into()),
                [info_card.into_any_element()],
                cx,
            ));

        if !tool.keywords.is_empty() {
            body = body.child(settings_section(
                Some(tr(cx, "关键词").into()),
                [option_row(
                    tr(cx, "关键词"),
                    None,
                    h_flex()
                        .flex_wrap()
                        .justify_end()
                        .gap(tokens.spacing.xxs)
                        .children(
                            tool.keywords
                                .iter()
                                .copied()
                                .map(|keyword| neutral_tag(t(lang, keyword), cx)),
                        )
                        .into_any_element(),
                    cx,
                )
                .into_any_element()],
                cx,
            ));
        }

        body = body.child(settings_section(
            Some(tr(cx, "通用").into()),
            [option_row(
                tr(cx, "启用"),
                Some(tr(cx, "停用后从工具箱列表隐藏，设置项保留。").into()),
                switch_control(
                    SharedString::from(format!("toolbox-right-sidebar-enabled-{id}")),
                    tool.enabled,
                    move |value, _, cx| {
                        tool_settings::set_enabled(cx, id, *value);
                        cx.refresh_windows();
                    },
                ),
                cx,
            )
            .into_any_element()],
            cx,
        ));

        // 帮助章节铺成默认全展开的折叠面板；空帮助不占位。
        if !tool.help.is_empty() {
            let sections = tool.help.iter().map(|section| {
                let paragraphs = v_flex()
                    .w_full()
                    .gap(tokens.spacing.sm)
                    .children(
                        section
                            .paragraphs()
                            .iter()
                            .copied()
                            .map(|paragraph| body_text(t(lang, paragraph), cx)),
                    )
                    .into_any_element();
                // 帮助是要读的内容，默认全展开（收起只是留给按需折叠的余地）。
                (
                    SharedString::from(t(lang, section.title())),
                    paragraphs,
                    true,
                )
            });
            body = body.child(accordion(
                SharedString::from(format!("toolbox-right-sidebar-help-{id}")),
                sections,
                cx,
            ));
        }

        // 设置项摘要（只读列出）；空设置不占位，写入口统一在设置窗口。
        if !tool.settings.is_empty() {
            let rows: Vec<AnyElement> = tool
                .settings
                .iter()
                .map(|spec| {
                    let description = (!spec.description().is_empty())
                        .then(|| SharedString::from(t(lang, spec.description())));
                    option_row(
                        t(lang, spec.label()),
                        description,
                        body_text(setting_value_text(cx, id, spec), cx),
                        cx,
                    )
                    .into_any_element()
                })
                .collect();
            body = body
                .child(settings_section(Some(tr(cx, "设置项").into()), rows, cx))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .gap(tokens.spacing.md)
                        .child(meta_text(
                            tr(cx, "工具设置项在设置窗口中统一调整，改动即时生效。"),
                            cx,
                        ))
                        .child(
                            button(
                                "toolbox-right-sidebar-open-settings",
                                tr(cx, "在设置中打开"),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .on_click(move |_, _, cx| open_settings_at("tools", Some(id), cx)),
                        ),
                );
        }

        div()
            .id(SharedString::from(format!(
                "toolbox-right-sidebar-body-{id}"
            )))
            .size_full()
            .overflow_y_scroll()
            .pl(px(CONTENT_PADDING_LEFT))
            .pr(px(CONTENT_PADDING_RIGHT))
            .pt(tokens.spacing.md)
            .pb(tokens.spacing.xl)
            .child(body)
            .into_any_element()
    }

    /// 状态栏：只放**有真实来源**的读数。
    ///
    /// 此前这里是一整套下载管理器风格的控件（上下行速率、全部暂停 / 全部开始、完成后
    /// 关机），源码注释写着「对齐 FluxDown」。本应用没有下载引擎、没有全局任务队列、
    /// 也没有宿主电源能力 —— 那几个控件点下去只能弹一句「演示：…」，而那两个速率读数
    /// 更是恒为 `0 B/s` 的假数字。全部删除：一个**点下去不会有结果**的按钮比没有它更糟。
    ///
    /// 现在左边是插件与工具的**真实计数**（与侧栏读同一份注册表），右边是家目录所在卷
    /// 的剩余空间。两样都拿不到时整条不渲染，不留一条空栏占位置。
    fn render_status_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let spacing = tokens.spacing;
        let muted = tokens.colors.muted_foreground;
        let hairline = extended.colors.hairline;
        let stroke = extended.stroke.thin;
        let bar_height = theme.density().status_bar;
        let meta_size = tokens.typography.xs.size;
        let meta_line_height = tokens.typography.xs.line_height;
        let icon_size = extended.icon.sm;

        let tools = ToolRegistry::listed(cx).len();
        let plugins = crate::plugins::index(cx).map_or(0, |index| index.loaded_count());
        let disk = disk_free_bytes();
        if tools == 0 && plugins == 0 && disk.is_none() {
            return None;
        }

        let icon_cell = move |icon: DuckIcon, text: String| {
            h_flex()
                .flex_none()
                .items_center()
                .gap(spacing.xxs)
                .child(Icon::new(icon).size(icon_size).text_color(muted))
                .child(selectable_text(text))
        };

        Some(
            h_flex()
                .w_full()
                .h(bar_height)
                .flex_none()
                .items_center()
                .justify_between()
                .gap(spacing.md)
                .px(spacing.sm)
                .bg(extended.colors.chrome)
                .border_t(stroke)
                .border_color(hairline)
                .text_size(meta_size)
                .line_height(meta_line_height)
                .font_features(tabular_numbers())
                .text_color(muted)
                .child(
                    h_flex()
                        .min_w_0()
                        .items_center()
                        .gap(spacing.md)
                        .child(icon_cell(
                            DuckIcon::Plug,
                            trf(
                                cx,
                                "{} 个插件 · {} 个工具",
                                &[&plugins.to_string(), &tools.to_string()],
                            ),
                        )),
                )
                .children(disk.map(|bytes| {
                    h_flex()
                        .min_w_0()
                        .items_center()
                        .gap(spacing.xs)
                        .child(icon_cell(
                            DuckIcon::HardDrive,
                            trf(cx, "剩余 {}", &[&format_bytes(bytes)]),
                        ))
                        .px(spacing.xs)
                }))
                .into_any_element(),
        )
    }

    /// 智能粘贴横幅：顶栏搜索框粘进来一段可识别内容时出现，确认后交给对应工具。
    ///
    /// 返回 `None` 表示当前没有待处理内容（不占位，内容直接上移）。
    fn render_paste_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = PasteInbox::peek(cx)?;
        // 工具已注册才显示；被 `dev` 过滤掉或尚未注册就静默丢弃，避免出现「认不出是谁」的横幅。
        let tool_id = pending.tool_id();
        let tool_name = ToolRegistry::get(cx, tool_id).map(|tool| tool.meta().name())?;

        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let colors = tokens.colors;
        let lang = theme.lang();

        Some(
            h_flex()
                .w_full()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.md)
                .py(tokens.spacing.sm)
                .bg(colors.primary.opacity(extended.opacity.subtle))
                .border_b(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(
                    Icon::new(DuckIcon::Zap)
                        .size(extended.icon.lg)
                        .text_color(colors.primary),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        // 同一段文案的两行（主体 + 补充说明）：最紧一档间距。
                        .gap(tokens.spacing.xxs)
                        .child(
                            div()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(colors.foreground)
                                .child(selectable_text(trf(
                                    cx,
                                    "已识别为「{}」",
                                    &[t(lang, tool_name)],
                                ))),
                        )
                        .child(
                            div()
                                .text_size(tokens.typography.xs.size)
                                .line_height(tokens.typography.xs.line_height)
                                .text_color(colors.muted_foreground)
                                .truncate()
                                .child(pending.text().to_owned()),
                        ),
                )
                .child(
                    button(
                        "toolbox-paste-dismiss",
                        tr(cx, "忽略"),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| {
                        PasteInbox::clear(cx);
                        cx.notify();
                    })),
                )
                .child(
                    button(
                        "toolbox-paste-accept",
                        trf(cx, "用「{}」处理", &[t(lang, tool_name)]),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(cx.listener(move |_, _, window, cx| {
                        // 工具后端尚未接入时 `invoke` 返回 `false`，此时保留负载让用户再试。
                        if ToolRegistry::invoke(tool_id, window, cx) {
                            PasteInbox::clear(cx);
                        }
                        cx.notify();
                    })),
                )
                .into_any_element(),
        )
    }
}

impl Render for ToolboxPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 激活路由若是插件 io 工具，先把通用执行页懒建出来（控件状态需要
        // `window` 建输入框，只有这里拿得到）。
        self.ensure_run_view(window, cx);
        let content = self.render_main(cx);
        let mut panel = SidebarPanel::new("toolbox-content", &self.sidebar, content, window, cx);
        if panel.is_sidebar_visible() {
            let sidebar = self.render_sidebar(cx);
            panel = panel.sidebar(sidebar);
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .relative()
                    .child(panel)
                    // 测内容区左缘：顶栏主按钮与其对齐（侧栏宽度变化时跟随）。
                    .child(left_edge_probe(
                        cx.entity().downgrade(),
                        |page| page.content_left,
                        |page, left| page.content_left = left,
                    )),
            )
            .children(self.render_status_bar(cx))
    }
}

/// 智能粘贴按钮：标签栏尾随与空态引导共用同一外观与行为。
fn smart_paste_button(cx: &App) -> duck_components::Button {
    let theme = active_theme(cx);
    icon_button(
        "toolbox-smart-paste",
        tr(cx, "识别粘贴"),
        Icon::new(DuckIcon::Clipboard)
            .size(theme.extended().icon.sm)
            .text_color(theme.tokens().colors.muted_foreground),
        ButtonVariant::Ghost,
        cx,
    )
    .on_click(|_, window, cx| ingest_clipboard(window, cx))
}

/// 智能粘贴：读剪贴板识别归属工具，命中就写进待处理负载（本页横幅确认）。
///
/// 顶栏搜索框改为命令面板入口后，识别动作从「往搜索框里粘贴」移到这里：
/// 标签栏尾随按钮与空态引导共用这一条路。
fn ingest_clipboard(window: &mut Window, cx: &mut App) {
    let text = cx.read_from_clipboard().and_then(|item| item.text());
    let Some(hit) = text.as_deref().and_then(detect) else {
        notify::info(tr(cx, "剪贴板内容未识别到可处理的工具"), window, cx);
        return;
    };
    // 工具已注册才谈得上「交给它处理」；拿不到就当作没识别。
    let Some(tool_name) = ToolRegistry::get(cx, hit.tool_id()).map(|tool| tool.meta().name())
    else {
        notify::info(tr(cx, "剪贴板内容未识别到可处理的工具"), window, cx);
        return;
    };

    PasteInbox::put(cx, PendingPaste::new(hit.tool_id(), hit.detail()));
    notify::success(
        trf(cx, "已识别为「{}」，到工具箱确认处理", &[tool_name]),
        window,
        cx,
    );
    // 横幅渲染在本页顶部：刷新把横幅带出来。
    cx.refresh_windows();
}

/// 把某项工具设置的当前值格式化成一行只读文本（工作区「设置」页签的摘要）。
///
/// 只展示、不编辑：设置窗口是唯一的写入口，这里避免第二套控件状态。
fn setting_value_text(cx: &App, tool_id: &str, spec: &ToolSettingSpec) -> String {
    let lang = active_theme(cx).lang();
    match tool_settings::current(cx, tool_id, spec) {
        SettingValue::Toggle(on) => t(lang, if on { "开" } else { "关" }).to_owned(),
        SettingValue::Select(value) => match spec.kind() {
            // 落盘值是稳定键，展示要翻成 label；schema 改过导致查不到时回落落盘值本身。
            ToolSettingKind::Select { options, .. } => options
                .iter()
                .find(|option| option.value() == value)
                .map_or_else(
                    || value.to_owned(),
                    |option| t(lang, option.label()).to_owned(),
                ),
            _ => value.to_owned(),
        },
        SettingValue::Text(text) => text,
        SettingValue::Number(number) => match spec.kind() {
            ToolSettingKind::Number { unit, .. } => {
                let unit = t(lang, unit);
                if unit.is_empty() {
                    number.to_string()
                } else {
                    format!("{number} {unit}")
                }
            }
            _ => number.to_string(),
        },
    }
}
