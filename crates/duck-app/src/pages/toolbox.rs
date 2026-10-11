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
use std::rc::Rc;

use duck_components::content::{
    accordion, description_list, description_row, empty_state, neutral_tag, status_cell,
};
use duck_components::{
    ButtonVariant, DuckIcon, SidebarPanel, SidebarState, beta_badge, body_text, button,
    caption_number, icon_button, meta_text, notify, option_row, settings_section,
    sidebar_navigation_button, status_button, status_button_content, switch_control,
    tabular_numbers, toolbar_action_button,
};
use duck_shell::left_edge_probe;
use duck_theme::{active_theme, t, tf, tr, trf};
use duck_tools::paste::detect;
use duck_tools::settings as tool_settings;
use duck_tools::settings::{SettingValue, ToolSettingKind, ToolSettingSpec};
use duck_tools::{
    HelpSection, PasteInbox, PendingPaste, RegisteredTool, ToolCategory, ToolQuery, ToolRegistry,
};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement, Pixels, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon, h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
    tooltip::Tooltip,
    v_flex,
};

use crate::pages::open_settings_at;
use crate::pages::settings::{CONTENT_PADDING_LEFT, CONTENT_PADDING_RIGHT};
use crate::pages::status_support::{disk_free_bytes, format_bytes, open_number_prompt};

/// 完成后关机延迟预设（分钟），对齐 FluxDown `SHUTDOWN_PRESETS_MIN`。
const SHUTDOWN_PRESETS_MIN: [i64; 3] = [1, 5, 30];

/// 工具箱侧栏默认宽度（px）。
const SIDEBAR_DEFAULT_WIDTH: f32 = 230.;
/// 侧栏可拖拽的宽度范围（px）。
const SIDEBAR_MIN_WIDTH: f32 = 184.;
const SIDEBAR_MAX_WIDTH: f32 = 320.;

/// 路由标签页的最大宽度（名称超出截断，防长名挤掉关闭按钮）。
const ROUTE_TAB_MAX_WIDTH: f32 = 160.;

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

/// 工具箱页。
pub struct ToolboxPage {
    sidebar: Entity<SidebarState>,
    /// 打开的路由（顺序即标签页顺序）。
    routes: Vec<ToolRoute>,
    /// 激活路由的工具 id；`routes` 非空时必有一些。
    active: Option<&'static str>,
    /// 折叠的分类（默认全展开）。
    collapsed: HashSet<ToolCategory>,
    /// 内容区左缘的窗口横坐标（上一帧测得），供顶栏搜索框对齐。
    content_left: Pixels,
}

impl ToolboxPage {
    /// 创建页面：默认展开 230px 侧栏；分类**默认全部折叠**（点开才见工具子行）。
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
            collapsed: ToolCategory::ALL.iter().copied().collect(),
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
    /// 分类默认折叠（见 [`Self::new`]），所以顺手把该工具所属的分类展开，
    /// 别让侧栏选中项藏在折叠区里。命令面板「打开工具」也走这里（见 `app.rs`
    /// 的 `OpenTool` 处理器），保证两条入口行为完全一致。
    pub fn open_route(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if let Some(tool) = snapshot_of(id, cx) {
            self.collapsed.remove(&tool.category);
        }
        if !self.routes.iter().any(|route| route.tool_id == id) {
            self.routes.push(ToolRoute {
                tool_id: id,
                scroll: ScrollHandle::new(),
            });
        }
        self.activate(id, cx);
    }

    /// 激活某个路由（保留其工作区状态）。
    fn activate(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.active != Some(id) {
            self.active = Some(id);
            cx.notify();
        }
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

    /// 左侧两级导航：分类行（可折叠）+ 工具子行（选中即激活路由）。
    ///
    /// 行规格与 [`sidebar_navigation_button`] 一致（`density.navRow` 高、
    /// `nav_item_radius` 圆角、状态层交叉淡入）；子行整体缩进一级。
    /// 顶栏搜索词非空时进入过滤形态：只显示命中的工具，折叠态忽略。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let lang = theme.lang();
        let query = ToolQuery::get(cx);
        let searching = !query.trim().is_empty();

        let categories = ToolRegistry::present_categories(cx);
        let view = cx.entity().downgrade();

        // 子行缩进：对齐父行的图标（行内边距 + 图标 + 图标与文字的间距）。
        let indent = tokens.spacing.sm + extended.icon.lg + tokens.spacing.sm;

        // 侧栏项间距：可在「设置 → 外观 → 布局 → 侧栏项间距」调（0 ~ 24px，0 为贴紧）。
        // `Pixels` 是 Copy，取值后不延长对 `theme` 的借用。
        let nav_gap = theme.appearance().nav_gap;

        let mut nav = v_flex().w_full().gap(px(nav_gap));
        let mut any_match = false;

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
            let chevron_icon = if expanded {
                DuckIcon::ChevronDown
            } else {
                DuckIcon::ChevronRight
            };
            let category_id = *category;

            nav = nav.child(
                sidebar_navigation_button(
                    SharedString::from(format!("toolbox-category-{}", category.id())),
                    SharedString::from(t(lang, category.label())),
                    // 图标不设色，继承行的文字色：悬停/选中随文字一起转强调色。
                    Icon::new(category.icon()).size(extended.icon.lg),
                    Icon::new(chevron_icon)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    false,
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
                    let selected = self.active == Some(row.id);
                    nav = nav.child(
                        sidebar_navigation_button(
                            SharedString::from(format!("toolbox-tool-{}", row.id)),
                            SharedString::from(t(lang, row.name)),
                            // 图标不设色，继承行的文字色：悬停/选中随文字一起转强调色。
                            Icon::new(row.icon).size(extended.icon.md),
                            div(),
                            selected,
                            cx,
                        )
                        .pl(indent)
                        .on_click({
                            let view = view.clone();
                            let tool_id = row.id;
                            move |_, _, cx| {
                                if let Ok(()) = view.update(cx, |this, cx| {
                                    this.open_route(tool_id, cx);
                                }) {}
                            }
                        }),
                    );
                }
            }
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
            .bg(tokens.colors.surface)
            .child(
                h_flex()
                    .h(theme.density().section_header)
                    .flex_none()
                    .items_center()
                    .px(tokens.spacing.sm)
                    .child(
                        div()
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(extended.colors.text_tertiary)
                            .child(t(lang, "分类")),
                    ),
            )
            .child(
                div()
                    .id("toolbox-sidebar-nav")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(nav),
            )
            .child(
                h_flex()
                    .flex_none()
                    .px(tokens.spacing.md)
                    .pb(tokens.spacing.sm)
                    .child(caption_number(
                        SharedString::from(tf(
                            lang,
                            "{} 个工具",
                            &[&ToolRegistry::listed(cx).len().to_string()],
                        )),
                        cx,
                    )),
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
                Some(tool) => (tool.icon, SharedString::from(t(lang, tool.name)), tool.beta),
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

        self.render_workspace_body(&tool, &scroll, cx)
    }

    /// 工作区内容区：该工具的操作区域，滚动容器挂在路由自己的句柄上，
    /// 切走再切回来偏移原样恢复。
    fn render_workspace_body(
        &self,
        tool: &ToolRow,
        scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let canvas = self.render_workspace_canvas(tool, cx);
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

    /// 状态栏里的小图标按钮（全部暂停 / 全部开始）。
    fn render_status_action(
        &self,
        id: &'static str,
        label: &'static str,
        icon: DuckIcon,
        message: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = active_theme(cx);
        let icon_size = theme.extended().icon.sm;
        let control_size = theme.density().status_control;
        let tooltip_label = SharedString::from(label);
        div()
            .id(SharedString::from(format!("{id}-tooltip")))
            .flex_none()
            .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
            .child(
                toolbar_action_button(id, label, Icon::new(icon).size(icon_size), false, false, cx)
                    .size(control_size)
                    .on_click(move |_, window, cx| {
                        duck_components::notify::info(message, window, cx);
                    }),
            )
    }

    /// 完成后关机控件：电源图标 + 预设下拉；本工程无宿主电源能力，选择后仅提示。
    fn render_shutdown_control(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let lang = active_theme(cx).lang();
        let title = t(lang, "完成后关机");

        status_button("shutdown-trigger", cx)
            .min_w(active_theme(cx).density().status_control)
            .child(status_button_content(Some(DuckIcon::Power), None, None, cx))
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                let mut menu = menu.item(PopupMenuItem::new(t(lang, "立即")).on_click(
                    move |_, window, cx| {
                        duck_components::notify::warning(
                            t(lang, "演示：完成后关机 · 立即（未接入宿主电源能力）"),
                            window,
                            cx,
                        );
                    },
                ));
                for minutes in SHUTDOWN_PRESETS_MIN {
                    menu = menu.item(
                        PopupMenuItem::new(SharedString::from(tf(
                            lang,
                            "{} 分钟",
                            &[&minutes.to_string()],
                        )))
                        .on_click(move |_, window, cx| {
                            duck_components::notify::warning(
                                tf(
                                    lang,
                                    "演示：完成后关机 · {} 分钟（未接入宿主电源能力）",
                                    &[&minutes.to_string()],
                                ),
                                window,
                                cx,
                            );
                        }),
                    );
                }
                menu.separator()
                    .item(PopupMenuItem::new(t(lang, "自定义...")).on_click(
                        move |_, window, cx| {
                            open_number_prompt(
                                window,
                                cx,
                                title,
                                t(lang, "自定义延迟"),
                                t(lang, "分钟"),
                                None,
                                1,
                                Rc::new(move |minutes, window, cx| {
                                    duck_components::notify::warning(
                                        tf(
                                            lang,
                                            "演示：完成后关机 · {} 分钟（未接入宿主电源能力）",
                                            &[&minutes.to_string()],
                                        ),
                                        window,
                                        cx,
                                    );
                                }),
                            );
                        },
                    ))
            })
    }

    /// 状态栏：左 = 全局速度 + 全部暂停 / 全部开始；右 = 限速、代理、完成后关机、剩余空间。
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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

        let pause_all = self.render_status_action(
            "status-pause-all",
            tr(cx, "全部暂停"),
            DuckIcon::Pause,
            tr(cx, "演示：全部暂停（未接入任务后端）"),
            cx,
        );
        let resume_all = self.render_status_action(
            "status-resume-all",
            tr(cx, "全部开始"),
            DuckIcon::Play,
            tr(cx, "演示：全部开始（未接入任务后端）"),
            cx,
        );
        let shutdown_control = self.render_shutdown_control(cx);
        let icon_cell = move |icon: DuckIcon, text: String| {
            h_flex()
                .flex_none()
                .items_center()
                .gap(spacing.xxs)
                .child(Icon::new(icon).size(icon_size).text_color(muted))
                .child(text)
        };

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
                        DuckIcon::ArrowDown,
                        format!("{}/s", format_bytes(0)),
                    ))
                    .child(icon_cell(
                        DuckIcon::ArrowUp,
                        format!("{}/s", format_bytes(0)),
                    ))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(spacing.xxs)
                            .child(pause_all)
                            .child(resume_all),
                    ),
            )
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap(spacing.xs)
                    .child(shutdown_control)
                    .children(
                        // 与左侧按钮内边距对齐，使右侧控件与磁盘信息的视觉间距一致。
                        disk_free_bytes().map(|bytes| {
                            icon_cell(
                                DuckIcon::HardDrive,
                                trf(cx, "剩余 {}", &[&format_bytes(bytes)]),
                            )
                            .px(spacing.xs)
                        }),
                    ),
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
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(colors.foreground)
                                .child(trf(cx, "已识别为「{}」", &[t(lang, tool_name)])),
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
            .child(self.render_status_bar(cx))
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
