//! 插件中心：独立辅助窗口，浏览本机已装与市场可用的插件，并可安装 / 卸载。
//!
//! 结构对齐设置窗口的辅助窗口模式（[`duck_shell::AuxiliaryWindowView`]）：
//! 顶部「搜索 + 状态筛选 + 分类筛选 + 刷新」固定，其下是标签筛选行与**两列网格卡片列表**；
//! 点击卡片进入详情视图（描述 / 来源 / 分类 / 标签 / 执行模型 / 能力声明 / 工具贡献 /
//! 启停开关 / 安装卸载）。数据全部来自 [`crate::market`] 的合并快照，不在渲染期碰网络
//! 或文件系统。
//!
//! 「已安装 / 未安装 / 可更新」这三档来自 [`crate::market::view`] 给出的
//! [`PluginStatus`]——它由「本机发现到的版本」与「远端索引里的版本」共同派生，
//! 所以插件中心必须先有一次成功的市场拉取（或用上一次的缓存）才能把「未安装」列出来。

use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;

use duck_components::{
    ButtonVariant, ConfirmHandler, DuckIcon, body_text, button, card, content, detail_title_text,
    dialog_footer, dialog_top_offset, dropdown_select, field_hint, icon_text_button, meta_text,
    modal_drag, modal_drag_area, modal_offset, modal_title_row, option_row, settings_section,
    switch_control, toolbar_action_button,
};
use duck_plugin::{Capabilities, Capability};
use duck_plugin_registry::PluginStatus;
use duck_shell::AuxiliaryWindowView;
use duck_theme::{Lang, UnderMinute, active_theme, tr, tr_relative_time, trf};
use duck_tools::ToolCategory;
use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement, Radians, Render, SharedString, StatefulInteractiveElement as _,
    Styled, Subscription, Window, div, prelude::FluentBuilder as _, px, size,
};
use gpui_component::{
    Icon, Root, Sizable as _, Size, WindowExt as _,
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    tooltip::Tooltip,
    v_flex,
};

use crate::app::Desktop;
use crate::market::{self, MarketEntry};
use crate::pages::settings::{CONTENT_PADDING_LEFT, CONTENT_PADDING_RIGHT};
use crate::plugins;
use crate::settings::{self, AuxWindow};

/// 插件中心窗口初始尺寸：单列卡片列表用不到设置窗口那么宽。
const PLUGIN_CENTER_WINDOW_SIZE: gpui::Size<gpui::Pixels> = size(px(880.), px(640.));
/// 最小尺寸：搜索 + 下拉 + 刷新一行放下，卡片仍有一行正文的宽度。
const PLUGIN_CENTER_WINDOW_MIN: gpui::Size<gpui::Pixels> = size(px(760.), px(500.));

/// 分类下拉里「全部」的值。
const CATEGORY_ALL: &str = "all";
/// 状态下拉里「全部」的值。
const STATUS_ALL: &str = "all";

/// 安装确认框的宽度：够放下「标签 + 路径」两列，又不必占满窗口。
const INSTALL_DIALOG_W: f32 = 520.;
/// 能力行左侧标签列的固定宽度：读 / 写 / 网络 / 通知 / 进程 五档里最宽的那个。
const CAPABILITY_LABEL_W: f32 = 72.;

/// 打开（或聚焦）插件中心窗口。一律 defer：动作回调处于活动窗口的 update 栈内，
/// 同步打开窗口可能静默失败（对齐设置窗口的处理）。
pub fn open_plugin_center(cx: &mut App) {
    cx.defer(open_or_focus);
}

fn open_or_focus(cx: &mut App) {
    if let Some(handle) = *Desktop::plugin_center_window(cx) {
        if handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            cx.activate(true);
            return;
        }
    }
    *Desktop::plugin_center_window(cx) = None;

    let mut options = duck_shell::auxiliary_window_options(SharedString::from(tr(cx, "插件中心")));
    // 上次的位置尺寸（已夹回可见区域）；没有存档则主屏居中。
    options.window_bounds = Some(settings::aux_window_bounds(
        cx,
        AuxWindow::PluginCenter,
        PLUGIN_CENTER_WINDOW_SIZE,
    ));
    options.window_min_size = Some(PLUGIN_CENTER_WINDOW_MIN);

    match cx.open_window(options, |window, cx| {
        let content = cx.new(|cx| PluginCenterView::new(window, cx));
        let chrome = cx.new(|cx| {
            // 占位标题：真实文案由 with_title_fn 按语言解析，此处不会被渲染。
            AuxiliaryWindowView::new("Plugin Center", content.into(), cx)
                .with_title_fn(|cx| SharedString::from(tr(cx, "插件中心")))
                // 几何变化 → 去抖持久化（与主窗口同形）。
                .on_window_bounds_changed(|window, cx| {
                    settings::save_aux_bounds(AuxWindow::PluginCenter, window, cx);
                })
        });
        crate::app::observe_system_appearance(window);
        cx.new(|cx| Root::new(chrome, window, cx))
    }) {
        Ok(handle) => {
            *Desktop::plugin_center_window(cx) = Some(handle);
            // 记下「这个窗口开着」，供下次启动恢复（见「设置 → 行为」的开关）。
            Desktop::remember_aux_open(cx);
            cx.activate(true);
        }
        Err(error) => crate::app::report_window_failure("插件中心窗口", &error, cx),
    }
}

/// 状态筛选档位。`None` 表示「全部」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusFilter {
    /// 已安装（含可更新）。
    Installed,
    /// 仅市场有、本机没有。
    NotInstalled,
    /// 已装且远端版本更高。
    Updatable,
}

impl StatusFilter {
    /// 下拉里的稳定值。
    const fn wire(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::NotInstalled => "not-installed",
            Self::Updatable => "updatable",
        }
    }

    /// 由稳定值还原。
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "installed" => Some(Self::Installed),
            "not-installed" => Some(Self::NotInstalled),
            "updatable" => Some(Self::Updatable),
            _ => None,
        }
    }

    /// 展示名。
    fn label(self) -> &'static str {
        match self {
            Self::Installed => "已安装",
            Self::NotInstalled => "未安装",
            Self::Updatable => "可更新",
        }
    }

    /// 某个条目是否落在这一档里。
    fn accepts(self, status: PluginStatus) -> bool {
        match self {
            Self::Installed => status.is_installed(),
            Self::NotInstalled => status == PluginStatus::NotInstalled,
            Self::Updatable => status == PluginStatus::Updatable,
        }
    }
}

/// 插件中心内容视图。
pub struct PluginCenterView {
    /// 搜索框状态（占位文案随语言同步，见 [`Self::sync_placeholders`]）。
    search: Entity<InputState>,
    /// 两个输入框占位文案使用的语言；语言切换后同步刷新（占位只在构建时定型）。
    placeholder_lang: Option<Lang>,
    /// 当前搜索词（小写化后比对）。
    query: String,
    /// 状态筛选；`None` = 全部。
    status: Option<StatusFilter>,
    /// 分类筛选；`None` = 全部分类。
    category: Option<ToolCategory>,
    /// 标签筛选；`None` = 不过滤。
    tag: Option<String>,
    /// 详情视图正在展示的插件 id；`None` = 列表视图。
    detail: Option<String>,
    /// 搜索词变更订阅；持有句柄仅为延长生命周期。
    _subscriptions: Vec<Subscription>,
}

impl PluginCenterView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "搜索插件…")));

        let mut subscriptions = Vec::new();
        subscriptions.push(
            cx.subscribe(&search, move |this, source, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query = source.read(cx).value().to_lowercase();
                    cx.notify();
                }
            }),
        );

        Self {
            search,
            placeholder_lang: Some(active_theme(cx).lang()),
            query: String::new(),
            status: None,
            category: None,
            tag: None,
            detail: None,
            _subscriptions: subscriptions,
        }
    }

    /// 语言切换后同步搜索框的占位文案。
    fn sync_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = active_theme(cx).lang();
        if self.placeholder_lang == Some(lang) {
            return;
        }
        self.placeholder_lang = Some(lang);
        let search_placeholder = tr(cx, "搜索插件…");
        self.search.update(cx, |input, cx| {
            input.set_placeholder(search_placeholder, window, cx)
        });
    }

    /// 当前筛选条件下可见的插件快照。
    fn visible_entries(&self, cx: &App) -> Vec<MarketEntry> {
        market::view(cx)
            .into_iter()
            .filter(|entry| {
                if let Some(status) = self.status {
                    if !status.accepts(entry.status) {
                        return false;
                    }
                }
                if let Some(category) = self.category {
                    if entry.category != category.id() {
                        return false;
                    }
                }
                if let Some(tag) = &self.tag {
                    if !entry.tags.iter().any(|candidate| candidate == tag) {
                        return false;
                    }
                }
                if self.query.is_empty() {
                    return true;
                }
                let haystacks = [
                    entry.name.to_lowercase(),
                    entry.description.to_lowercase(),
                    entry.id.clone().unwrap_or_default().to_lowercase(),
                    entry.tags.join(" ").to_lowercase(),
                    entry.author.to_lowercase(),
                ];
                haystacks
                    .iter()
                    .any(|hay| hay.contains(self.query.as_str()))
            })
            .collect()
    }

    /// 全部插件的标签并集（去重升序），供标签筛选行渲染。
    fn all_tags(entries: &[MarketEntry]) -> Vec<String> {
        entries
            .iter()
            .flat_map(|entry| entry.tags.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    // ── 列表视图 ─────────────────────────────────────────────────────────

    fn render_list(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let entries = self.visible_entries(cx);
        let tags = Self::all_tags(&entries);

        let toolbar = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(self.render_search_box(cx))
            .child(self.render_status_dropdown(cx))
            .child(self.render_category_dropdown(cx))
            .child(div().flex_1())
            .child(self.render_refresh_button(cx));

        let tag_row = (!tags.is_empty()).then(|| {
            h_flex()
                .w_full()
                .flex_none()
                .flex_wrap()
                .gap(tokens.spacing.xs)
                .children(tags.into_iter().map(|tag| self.render_tag_chip(tag, cx)))
        });

        let status_line = self.render_market_status(cx);

        let list: gpui::AnyElement = if entries.is_empty() {
            if self.has_filters() {
                content::empty_state(
                    DuckIcon::Search,
                    tr(cx, "没有匹配的插件"),
                    Some(tr(cx, "调整筛选条件或清空搜索词。").into()),
                    None,
                    cx,
                )
                .into_any_element()
            } else {
                let dir = plugins::primary_plugins_dir();
                content::empty_state(
                    DuckIcon::Box,
                    tr(cx, "未发现插件"),
                    Some(
                        trf(
                            cx,
                            "从市场安装，或将插件放入 {} 后重启应用。",
                            &[dir.display().to_string().as_str()],
                        )
                        .into(),
                    ),
                    None,
                    cx,
                )
                .into_any_element()
            }
        } else {
            let cards: Vec<gpui::AnyElement> = entries
                .into_iter()
                .map(|entry| self.render_plugin_card(entry, cx))
                .collect();
            div()
                .w_full()
                .grid()
                .grid_cols(2)
                .gap(tokens.spacing.md)
                .children(cards)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(tokens.colors.surface)
            .text_color(tokens.colors.foreground)
            .pl(px(CONTENT_PADDING_LEFT))
            .pr(px(CONTENT_PADDING_RIGHT))
            .pt(tokens.spacing.lg)
            .pb(tokens.spacing.lg)
            .gap(tokens.spacing.md)
            .child(toolbar)
            .children(tag_row)
            .child(status_line)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(list),
            )
            .into_any_element()
    }

    /// 是否有任何筛选条件（决定空列表时说「没匹配」还是「没插件」）。
    fn has_filters(&self) -> bool {
        !self.query.is_empty()
            || self.status.is_some()
            || self.category.is_some()
            || self.tag.is_some()
    }

    /// 搜索框：浅色胶囊 + 放大镜图标 + 可清空输入框。
    fn render_search_box(&self, cx: &App) -> gpui::AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let chrome = theme.extended().colors;
        let opacity = theme.extended().opacity;
        let input_focus = self.search.read(cx).focus_handle(cx);

        h_flex()
            .id("plugin-center-search-box")
            .track_focus(&input_focus)
            // 搜索框宽度区间：布局约束（不在 spacing 阶梯上），保留裸值；
            // `.flex_shrink(1.)` + `min_w` 让它在窄窗口里先让位、再兜底可读宽度。
            .w(px(240.))
            .flex_shrink(1.)
            .min_w(px(160.))
            .h(theme.density().control)
            .pl(tokens.spacing.sm)
            .pr(tokens.spacing.xs)
            .gap(tokens.spacing.xs)
            .items_center()
            .rounded(tokens.radius.md)
            .border(theme.extended().stroke.thin)
            .border_color(chrome.hairline)
            .bg(tokens.colors.surface)
            .cursor_text()
            // 悬停给强调色描边 + 薄底；聚焦态由输入框自身光标与描边体现。
            .hover(move |style| {
                style
                    .border_color(tokens.colors.primary.opacity(opacity.muted))
                    .bg(chrome.hover_tint)
            })
            .on_click(move |_, window, cx| window.focus(&input_focus, cx))
            .child(
                Icon::new(DuckIcon::Search)
                    .size(theme.extended().icon.md)
                    .text_color(tokens.colors.muted_foreground),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.search)
                        .appearance(false)
                        .with_size(Size::Small)
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .w_full()
                        .cleanable(true),
                ),
            )
            .into_any_element()
    }

    /// 状态下拉：全部 / 已安装 / 未安装 / 可更新。
    fn render_status_dropdown(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let options: Vec<(SharedString, SharedString)> = std::iter::once((
            SharedString::from(STATUS_ALL),
            SharedString::from(tr(cx, "全部状态")),
        ))
        .chain(
            [
                StatusFilter::Installed,
                StatusFilter::NotInstalled,
                StatusFilter::Updatable,
            ]
            .into_iter()
            .map(|filter| {
                (
                    SharedString::from(filter.wire()),
                    SharedString::from(tr(cx, filter.label())),
                )
            }),
        )
        .collect();
        let current = SharedString::from(self.status.map_or(STATUS_ALL, StatusFilter::wire));

        let view = cx.entity().downgrade();
        dropdown_select(
            "plugin-center-status",
            options,
            current,
            move |value, _, cx| {
                let value: &str = &value;
                if let Some(view) = view.upgrade() {
                    view.update(cx, |this, cx| {
                        this.status = StatusFilter::from_wire(value);
                        cx.notify();
                    });
                }
            },
            cx,
        )
    }

    /// 分类下拉：取当前插件并集里出现过的分类（含「全部」）。
    fn render_category_dropdown(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut present: Vec<ToolCategory> = market::view(cx)
            .iter()
            .filter_map(|entry| {
                ToolCategory::ALL
                    .iter()
                    .copied()
                    .find(|category| category.id() == entry.category)
            })
            .collect();
        present.sort_by_key(|category| {
            ToolCategory::ALL
                .iter()
                .position(|candidate| candidate == category)
        });
        present.dedup();

        let mut options: Vec<(SharedString, SharedString)> = vec![(
            SharedString::from(CATEGORY_ALL),
            SharedString::from(tr(cx, "全部分类")),
        )];
        options.extend(present.iter().map(|category| {
            (
                SharedString::from(category.id()),
                SharedString::from(tr(cx, category.label())),
            )
        }));
        let current = SharedString::from(self.category.map_or(CATEGORY_ALL, ToolCategory::id));

        let view = cx.entity().downgrade();
        dropdown_select(
            "plugin-center-category",
            options,
            current,
            move |value, _, cx| {
                let value: &str = &value;
                if let Some(view) = view.upgrade() {
                    view.update(cx, |this, cx| {
                        this.category = ToolCategory::ALL
                            .iter()
                            .copied()
                            .find(|category| category.id() == value);
                        cx.notify();
                    });
                }
            },
            cx,
        )
    }

    /// 刷新按钮：拉取中禁用，右侧状态行给出结论。
    fn render_refresh_button(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = active_theme(cx);
        let loading = market::phase(cx).is_loading();
        let label = if loading {
            tr(cx, "获取中…")
        } else {
            tr(cx, "刷新")
        };
        let action = toolbar_action_button(
            "plugin-center-refresh",
            label,
            h_flex()
                .items_center()
                .gap(theme.tokens().spacing.xs)
                .child(Icon::new(DuckIcon::RotateCw).size(theme.extended().icon.md))
                .child(
                    div()
                        .text_size(theme.tokens().typography.xs.size)
                        .child(label),
                ),
            false,
            loading,
            cx,
        );
        let node = if loading {
            action
        } else {
            action.on_click(cx.listener(|_, _, _, cx| market::refresh(cx)))
        };
        node.into_any_element()
    }

    /// 市场状态行：拉取中 / 失败原因 / 条目数与上次更新时间。
    ///
    /// 市场源本身（地址 + 编辑）已挪到「设置 → 插件」页；这里只留一行结论，
    /// 不再占一整行放地址与「配置源」按钮。
    fn render_market_status(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let phase = market::phase(cx);
        let age = market::age_seconds(cx);
        let count = market::entries(cx).len();

        let text: SharedString = match &phase {
            market::Phase::Loading => tr(cx, "正在获取插件清单…").into(),
            market::Phase::Failed(detail) => {
                trf(cx, "获取插件清单失败：{}", &[detail.as_str()]).into()
            }
            market::Phase::Idle => tr(cx, "尚未获取插件清单，点「刷新」从市场获取。").into(),
            market::Phase::Ready => {
                let ago = age.map_or_else(
                    || tr(cx, "刚刚").to_string(),
                    |seconds| format_age(seconds, cx),
                );
                trf(
                    cx,
                    "市场共 {} 个插件 · 上次更新：{}",
                    &[&count.to_string(), &ago],
                )
                .into()
            }
        };

        meta_text(text, cx).w_full().flex_none().into_any_element()
    }

    /// 标签筛选 chip：选中态用强调色描边 + 淡强调底；未选中悬停也给强调色反馈。
    fn render_tag_chip(&self, tag: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let chrome = theme.extended().colors;
        let opacity = theme.extended().opacity;
        let selected = self.tag.as_ref() == Some(&tag);
        let accent = tokens.colors.primary;

        let view = cx.entity().downgrade();
        let tag_for_click = tag.clone();
        div()
            .id(SharedString::from(format!("plugin-center-tag-{tag}")))
            .flex_none()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xxs)
            .rounded_full()
            .border(theme.extended().stroke.thin)
            .text_size(tokens.typography.xs.size)
            .line_height(tokens.typography.xs.line_height)
            .map(|this| {
                if selected {
                    this.border_color(accent)
                        .bg(accent.opacity(opacity.tint))
                        .text_color(accent)
                } else {
                    // 未选中：悬停给强调色薄底 + 描边，文字转强调色。
                    this.border_color(chrome.hairline)
                        .text_color(tokens.colors.muted_foreground)
                        .hover(move |style| {
                            style
                                .bg(chrome.hover_tint)
                                .border_color(accent)
                                .text_color(accent)
                        })
                }
            })
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |this, cx| {
                        let next = this.tag.as_ref() != Some(&tag_for_click);
                        this.tag = if next {
                            Some(tag_for_click.clone())
                        } else {
                            None
                        };
                        cx.notify();
                    });
                }
            })
            .child(tag)
            .into_any_element()
    }

    /// 网格卡片：图标块 + 名称/版本/状态 + 描述 + 标签 + 操作（安装 / 更新 / 箭头）。
    fn render_plugin_card(&self, entry: MarketEntry, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let chrome = theme.extended().colors;
        let opacity = theme.extended().opacity;
        let icon = theme.extended().icon;
        let invalid = entry.id.is_none();

        // 图标取第一个工具贡献的声明，缺省回落分类图标，再回落通用箱形图标。
        let plugin_icon = entry
            .id
            .as_deref()
            .and_then(|id| plugins::manifest_of(cx, id))
            .and_then(|manifest| manifest.tools.first().and_then(|tool| tool.icon.as_deref()));
        let fallback_category = ToolCategory::ALL
            .iter()
            .copied()
            .find(|category| category.id() == entry.category);
        let duck_icon = match plugin_icon {
            Some(name) => plugins::icon_of(Some(name)),
            None => fallback_category
                .map(ToolCategory::icon)
                .unwrap_or(DuckIcon::Box),
        };

        let category_label: Option<&'static str> =
            fallback_category.map(|category| tr(cx, category.label()));
        let model_label = entry.model.map(|model| plugins::model_display(model, cx));
        let tools_label = (entry.tools > 0)
            .then(|| trf(cx, "{} 个工具", &[entry.tools.to_string().as_str()]).to_string());

        let mut meta_parts: Vec<String> = Vec::new();
        if let Some(label) = category_label {
            meta_parts.push(label.to_string());
        }
        if let Some(label) = model_label {
            meta_parts.push(label.to_string());
        }
        if let Some(label) = &tools_label {
            meta_parts.push(label.clone());
        }
        if !entry.author.is_empty() {
            meta_parts.push(entry.author.clone());
        }

        let invalid_note = invalid.then(|| {
            format!(
                "{} — {}",
                tr(cx, "清单无效"),
                entry.error.as_deref().unwrap_or("")
            )
        });

        let version_label = match (&entry.installed_version, &entry.remote_version) {
            (Some(local), Some(remote)) if entry.status == PluginStatus::Updatable => {
                Some(format!("v{local} → v{remote}"))
            }
            (Some(local), _) => Some(format!("v{local}")),
            (None, Some(remote)) => Some(format!("v{remote}")),
            (None, None) => None,
        };

        // 状态只靠文字区分：全页 tag 统一走强调色薄底，避免蓝/绿/橙多套语义色打架。
        let status_tag = match entry.status {
            PluginStatus::NotInstalled => content::accent_tag(tr(cx, "未安装"), cx),
            PluginStatus::Updatable => content::accent_tag(tr(cx, "可更新"), cx),
            PluginStatus::Installed | PluginStatus::UpToDate => {
                content::accent_tag(tr(cx, "已安装"), cx)
            }
        };

        let region =
            h_flex()
                .id(SharedString::from(format!(
                    "plugin-center-card-region-{}",
                    entry.id.as_deref().unwrap_or("invalid")
                )))
                .flex_1()
                .min_w_0()
                .items_start()
                .gap(tokens.spacing.lg)
                .child(
                    div()
                        .flex_none()
                        // 插件图标底板边长：布局约束（不在 spacing / 密度阶梯上），保留裸值。
                        .size(px(40.))
                        .rounded(tokens.radius.md)
                        .bg(chrome.nav_hover)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Icon::new(duck_icon)
                                .size(icon.lg)
                                .text_color(tokens.colors.muted_foreground),
                        ),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(tokens.spacing.xs)
                        .child(
                            h_flex()
                                .min_w_0()
                                .items_center()
                                .gap(tokens.spacing.sm)
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(tokens.typography.sm.size)
                                        .line_height(tokens.typography.sm.line_height)
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(tokens.colors.foreground)
                                        .child(entry.name.clone()),
                                )
                                .children(version_label.map(|label| meta_text(label, cx)))
                                .child(status_tag),
                        )
                        .child(div().min_w_0().truncate().child(
                            if let Some(note) = &invalid_note {
                                meta_text(note.clone(), cx).into_any_element()
                            } else {
                                body_text(entry.description.clone(), cx)
                                    .text_color(tokens.colors.muted_foreground)
                                    .into_any_element()
                            },
                        ))
                        .child(
                            h_flex()
                                .min_w_0()
                                .flex_wrap()
                                .gap(tokens.spacing.xs)
                                .children(
                                    (!meta_parts.is_empty())
                                        .then(|| meta_text(meta_parts.join(" · "), cx)),
                                ),
                        ),
                )
                .child(
                    Icon::new(if invalid {
                        DuckIcon::TriangleAlert
                    } else {
                        DuckIcon::ChevronRight
                    })
                    .size(icon.md)
                    .text_color(tokens.colors.muted_foreground),
                );

        // 需要动作的条目（未安装 / 可更新）在卡片右缘给一个按钮；已装且最新的只留箭头。
        let action = entry
            .id
            .clone()
            .filter(|_| entry.status.needs_action())
            .map(|plugin_id| {
                let busy = market::is_busy(cx, &plugin_id);
                let label = if busy {
                    tr(cx, "安装中…")
                } else if entry.status == PluginStatus::Updatable {
                    tr(cx, "更新")
                } else {
                    tr(cx, "安装")
                };
                let id_for_click = plugin_id.clone();
                button(
                    SharedString::from(format!("plugin-center-action-{plugin_id}")),
                    label,
                    ButtonVariant::Primary,
                    cx,
                )
                .on_click(move |_, window, cx| request_install(&id_for_click, window, cx))
                .into_any_element()
            });

        let clickable_id = entry.id.clone();
        let row = h_flex()
            .w_full()
            .items_start()
            .px(tokens.spacing.lg)
            .py(tokens.spacing.md)
            .gap(tokens.spacing.md)
            .child(region)
            .children(action);

        let base = card(cx).w_full();
        match clickable_id {
            Some(plugin_id) => {
                let view = cx.entity().downgrade();
                base.id(SharedString::from(format!(
                    "plugin-center-card-{plugin_id}"
                )))
                .hover(move |style| {
                    style.border_color(tokens.colors.primary.opacity(opacity.hover_border))
                })
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "plugin-center-open-{plugin_id}"
                        )))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            if let Some(view) = view.upgrade() {
                                view.update(cx, |this, cx| {
                                    this.detail = Some(plugin_id.clone());
                                    cx.notify();
                                });
                            }
                        })
                        .child(row),
                )
                .into_any_element()
            }
            None => base
                .cursor_default()
                .opacity(opacity.dim)
                .child(row)
                .into_any_element(),
        }
    }

    // ── 详情视图 ─────────────────────────────────────────────────────────

    fn render_detail(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(plugin_id) = self.detail.clone() else {
            return div().into_any_element();
        };
        let Some(entry) = market::view(cx)
            .into_iter()
            .find(|entry| entry.id.as_deref() == Some(plugin_id.as_str()))
        else {
            // 插件目录被移除等极端情况：退回列表。
            self.detail = None;
            return self.render_list(cx);
        };

        // 主操作先算：它要 `&mut cx`，而下面取主题会把 `cx` 不可变借用保持到函数结束。
        let detail_action = self.render_detail_action(&entry, cx);

        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();

        let header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                icon_text_button(
                    "plugin-center-back",
                    tr(cx, "返回"),
                    // 库内无左箭头资源：chevron-right 旋转 180° 作返回指示。
                    Icon::new(DuckIcon::ChevronRight)
                        .rotate(Radians(std::f32::consts::PI))
                        .size(theme.extended().icon.sm),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.detail = None;
                    cx.notify();
                })),
            )
            .child(
                detail_title_text(entry.name.clone(), cx)
                    .min_w_0()
                    .truncate(),
            )
            .child(match entry.status {
                PluginStatus::NotInstalled => content::accent_tag(tr(cx, "未安装"), cx),
                PluginStatus::Updatable => content::accent_tag(tr(cx, "可更新"), cx),
                PluginStatus::Installed | PluginStatus::UpToDate => {
                    content::accent_tag(tr(cx, "已安装"), cx)
                }
            })
            .child(div().flex_1())
            .child(detail_action);

        let category_label = ToolCategory::ALL
            .iter()
            .copied()
            .find(|category| category.id() == entry.category)
            .map(|category| tr(cx, category.label()).to_string());

        let dash = "—".to_string();
        // 「标签—值」表格里的**值一律 `body_text`（13）**：无论人类可读（版本号、分类）
        // 还是机器标识（安装目录、工具 id、能力项），值就是值，区分靠内容而不是字号。
        // `meta_text`（12）只留给整行级的辅助说明（市场状态行、空态文案、卡片元信息）。
        let overview = settings_section(
            Some(tr(cx, "插件信息").into()),
            [
                option_row(
                    tr(cx, "已装版本"),
                    None,
                    body_text(
                        entry
                            .installed_version
                            .clone()
                            .unwrap_or_else(|| dash.clone()),
                        cx,
                    ),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "市场版本"),
                    None,
                    body_text(
                        entry.remote_version.clone().unwrap_or_else(|| dash.clone()),
                        cx,
                    ),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "分类"),
                    None,
                    body_text(category_label.unwrap_or_else(|| dash.clone()), cx),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "标签"),
                    None,
                    body_text(
                        if entry.tags.is_empty() {
                            dash.clone()
                        } else {
                            entry.tags.join("、")
                        },
                        cx,
                    ),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "执行模型"),
                    None,
                    body_text(
                        entry.model.map_or_else(
                            || dash.clone(),
                            |model| plugins::model_display(model, cx).to_string(),
                        ),
                        cx,
                    ),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "作者"),
                    None,
                    body_text(
                        if entry.author.is_empty() {
                            dash.clone()
                        } else {
                            entry.author.clone()
                        },
                        cx,
                    ),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "包体积"),
                    None,
                    body_text(
                        entry.archive_size.map_or_else(|| dash.clone(), format_size),
                        cx,
                    ),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "安装目录"),
                    None,
                    div().min_w_0().truncate().child(body_text(
                        entry
                            .dir
                            .as_ref()
                            .map_or_else(|| dash.clone(), |dir| dir.display().to_string()),
                        cx,
                    )),
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        );

        // 能力声明：**与安装确认框共用同一套行**。两处各写一套的话，迟早会出现
        // 「详情页说它只要通知、确认框说它还要网络」这种自相矛盾。
        let declared = entry.capabilities.clone();
        let added = market::added_capabilities(entry.installed_capabilities.as_ref(), &declared);
        let rows = capability_rows(&declared, &added);
        // 已安装才给逐项调整：未安装的插件没有「本机授予」可言，勾了也无处安放。
        let grant_target = entry.id.clone().filter(|_| entry.status.is_installed());
        let granted = grant_target
            .as_ref()
            .map(|id| Capability::set_of(&plugins::granted_capabilities(cx, id)));
        let section_rows: Vec<gpui::AnyElement> = if rows.is_empty() {
            vec![meta_text(tr(cx, "未请求任何能力"), cx).into_any_element()]
        } else {
            rows.into_iter()
                .map(|row| {
                    let control = match (&granted, &grant_target) {
                        (Some(allowed), Some(installed_id)) => {
                            let plugin_id = installed_id.clone();
                            let checked = allowed.contains(&row.unit);
                            let key = unit_key(&row.unit);
                            let unit = row.unit;
                            let declared = declared.clone();
                            let display_name = entry.name.clone();
                            div()
                                .flex_none()
                                .child(switch_control(
                                    SharedString::from(format!("plugin-grant-{plugin_id}-{key}")),
                                    checked,
                                    move |value: &bool, window, cx| {
                                        // 每次从**运行时全局**读当前授予再改这一位：
                                        // `kv_put` 是去抖写的，回读会拿到旧值（P2-6 的教训）。
                                        let mut next = Capability::set_of(
                                            &plugins::granted_capabilities(cx, &plugin_id),
                                        );
                                        if *value {
                                            next.insert(unit.clone());
                                        } else {
                                            next.remove(&unit);
                                        }
                                        plugins::set_granted(cx, &plugin_id, &declared, &next);
                                        duck_components::notify::success(
                                            trf(
                                                cx,
                                                "插件「{}」的权限已更新",
                                                &[display_name.as_str()],
                                            ),
                                            window,
                                            cx,
                                        );
                                        cx.refresh_windows();
                                    },
                                ))
                                .into_any_element()
                        }
                        // 未安装：只读展示，点也点不动。
                        _ => capability_value(row.value, row.is_new, cx),
                    };
                    option_row(tr(cx, row.label), None, control, cx).into_any_element()
                })
                .collect()
        };
        // 已安装时标题改成「能力授权」：这一栏不再是「它要什么」，而是「你给了它什么」。
        let capability_title = if grant_target.is_some() {
            tr(cx, "能力授权")
        } else {
            tr(cx, "能力声明")
        };
        let capabilities = settings_section(Some(capability_title.into()), section_rows, cx);

        // 工具贡献。
        let tools = plugins::manifest_of(cx, &plugin_id)
            .map(|manifest| manifest.tools.clone())
            .unwrap_or_default();
        let tool_rows: Vec<gpui::AnyElement> = if tools.is_empty() {
            vec![meta_text(tr(cx, "该插件没有工具贡献"), cx).into_any_element()]
        } else {
            tools
                .iter()
                .map(|tool| {
                    option_row(
                        tool.name.clone(),
                        Some(tool.description.clone().into()),
                        body_text(tool.id.clone(), cx),
                        cx,
                    )
                    .into_any_element()
                })
                .collect()
        };
        let contributions = settings_section(Some(tr(cx, "工具贡献").into()), tool_rows, cx);

        // 启停开关：只对已安装的插件有意义。
        let toggle = entry
            .id
            .clone()
            .filter(|_| entry.status.is_installed())
            .map(|plugin_id| {
                let display_name = entry.name.clone();
                switch_control(
                    SharedString::from(format!("plugin-center-toggle-{plugin_id}")),
                    entry.enabled,
                    move |value: &bool, window, cx| {
                        plugins::set_enabled(cx, &plugin_id, *value);
                        let label = if *value {
                            trf(cx, "插件「{}」已启用", &[display_name.as_str()])
                        } else {
                            trf(cx, "插件「{}」已停用", &[display_name.as_str()])
                        };
                        duck_components::notify::success(label, window, cx);
                        cx.refresh_windows();
                    },
                )
            });

        v_flex()
            .size_full()
            .bg(tokens.colors.surface)
            .text_color(tokens.colors.foreground)
            .pl(px(CONTENT_PADDING_LEFT))
            .pr(px(CONTENT_PADDING_RIGHT))
            .pt(tokens.spacing.lg)
            .pb(tokens.spacing.lg)
            .gap(tokens.spacing.md)
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .w_full()
                            .gap(tokens.spacing.md)
                            .when(!entry.description.is_empty(), |this| {
                                this.child(
                                    body_text(entry.description.clone(), cx)
                                        .text_color(tokens.colors.muted_foreground),
                                )
                            })
                            .when(!entry.homepage.is_empty(), |this| {
                                this.child(
                                    h_flex()
                                        .items_center()
                                        .gap(tokens.spacing.xs)
                                        .child(
                                            Icon::new(DuckIcon::ExternalLink)
                                                .size(theme.extended().icon.sm)
                                                .text_color(tokens.colors.muted_foreground),
                                        )
                                        .child(meta_text(entry.homepage.clone(), cx)),
                                )
                            })
                            .child(toggle.map_or_else(
                                || div().into_any_element(),
                                |toggle| {
                                    option_row(tr(cx, "启用插件"), None, toggle, cx)
                                        .into_any_element()
                                },
                            ))
                            .child(overview)
                            .child(capabilities)
                            .child(contributions),
                    ),
            )
            .into_any_element()
    }

    /// 详情页右上角的主操作：安装 / 更新 / 卸载。
    fn render_detail_action(
        &self,
        entry: &MarketEntry,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(plugin_id) = entry.id.clone() else {
            return div().into_any_element();
        };

        if market::is_busy(cx, &plugin_id) {
            let label = if entry.status == PluginStatus::NotInstalled {
                tr(cx, "安装中…")
            } else {
                tr(cx, "处理中…")
            };
            return button(
                SharedString::from("plugin-center-detail-busy"),
                label,
                ButtonVariant::Secondary,
                cx,
            )
            .into_any_element();
        }

        if entry.status.needs_action() {
            let label = if entry.status == PluginStatus::Updatable {
                tr(cx, "更新")
            } else {
                tr(cx, "安装")
            };
            let id_for_click = plugin_id.clone();
            return button(
                SharedString::from(format!("plugin-center-detail-install-{plugin_id}")),
                label,
                ButtonVariant::Primary,
                cx,
            )
            .on_click(move |_, window, cx| request_install(&id_for_click, window, cx))
            .into_any_element();
        }

        let id_for_click = plugin_id.clone();
        button(
            SharedString::from(format!("plugin-center-detail-uninstall-{plugin_id}")),
            tr(cx, "卸载"),
            ButtonVariant::Destructive,
            cx,
        )
        .on_click(move |_, _, cx| market::uninstall_plugin(cx, &id_for_click))
        .into_any_element()
    }
}

impl Render for PluginCenterView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_placeholders(window, cx);
        if self.detail.is_some() {
            self.render_detail(cx)
        } else {
            self.render_list(cx)
        }
    }
}

// ── 安装确认 ─────────────────────────────────────────────────────────────

/// 安装 / 更新的**唯一入口**：先看这个插件要什么权限，要了就先把权限摊给用户点头。
///
/// 卡片上的按钮与详情页的按钮都走这里——两条路各自直接调 `install_plugin` 的话，
/// 从卡片安装就会绕过确认框，而卡片恰恰是最常被点到的那条路。
fn request_install(plugin_id: &str, window: &mut Window, cx: &mut App) {
    if market::is_busy(cx, plugin_id) {
        return;
    }
    // 每次都从合并快照现取：点下去的那一刻的声明才算数，不用渲染期的旧副本。
    let Some(entry) = market::view(cx)
        .into_iter()
        .find(|entry| entry.id.as_deref() == Some(plugin_id))
    else {
        return;
    };

    if entry.capabilities.is_empty() {
        // 什么都没请求 → 没有需要点头的东西。空弹窗只会训练用户闭眼点确定。
        market::install_plugin(cx, plugin_id);
        return;
    }

    let updating = entry.status.is_installed();
    let added =
        market::added_capabilities(entry.installed_capabilities.as_ref(), &entry.capabilities);
    // 勾选初值：**已装过就沿用既有授予**——这样「同意并更新」不会顺手把用户之前收回去的
    // 权限又放回来，而本次新加的权限默认不勾，正好对上「本次更新新增 N 项权限，请确认」。
    // 首次安装没有既有授予，声明全给（与「点同意即全给」的旧行为一致，不悄悄少给）。
    let initial = if updating {
        Capability::set_of(&plugins::granted_capabilities(cx, plugin_id))
    } else {
        Capability::set_of(&entry.capabilities)
    };
    open_install_dialog(
        plugin_id.to_string(),
        entry.name.clone(),
        entry.capabilities.clone(),
        added,
        initial,
        updating,
        window,
        cx,
    );
}

/// 摊开能力声明的确认框：**逐条勾选**要放行的能力，取消勾选的当场就不给。
///
/// 文案刻意分两句：授权范围（这个插件要什么）与后果（不给会怎样、怎么收回）。
/// 只报权限不说后果，用户没法判断该不该点同意。
///
/// 授予结果落 `kv: plugin_caps::<id>`（见 [`crate::plugins::set_granted`]），
/// 运行时生效的是「清单声明 ∩ 这份授予」——**只勾不记等于没勾**，所以两个动作必须成对。
#[allow(clippy::too_many_arguments)]
fn open_install_dialog(
    plugin_id: String,
    name: String,
    capabilities: Capabilities,
    added: HashSet<Capability>,
    initial: HashSet<Capability>,
    updating: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let added_count = added.len();
    let selection = cx.new(|_| GrantSelection::new(initial));
    // 确认动作放函数级 `Rc`：键盘 ⏎ 与底部「同意并安装 / 更新」按钮**共用同一份**
    // 落库逻辑。先记授权再装——装完才写的话，安装过程中弹出的工具已经按旧授予跑过了。
    // 安装失败时这条记录会留着：下次打开确认框正好接着上次的勾选，比「失败即清空、
    // 用户得重勾一遍」更贴近意图。
    let confirm: ConfirmHandler = {
        let selection = selection.clone();
        let id = plugin_id.clone();
        let declared = capabilities.clone();
        Rc::new(move |_window, cx| {
            let allowed = selection.read(cx).snapshot();
            plugins::set_granted(cx, &id, &declared, &allowed);
            market::install_plugin(cx, &id);
            true
        })
    };
    let title_key = if updating {
        "更新插件「{}」"
    } else {
        "安装插件「{}」"
    };
    let ok_key = if updating {
        "同意并更新"
    } else {
        "同意并安装"
    };
    let lead_key = if updating {
        "更新后，本插件会请求访问以下资源："
    } else {
        "安装后，本插件会请求访问以下资源："
    };

    let drag = modal_drag(cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        // 内容闭包可能被多次调用（重排 / 重绘），所以每一轮都从克隆出来的数据重建。
        let capabilities = capabilities.clone();
        let added = added.clone();
        // 逐条勾选要用的一份；`Entity` 克隆只是引用计数。
        let selection_for_rows = selection.clone();
        // 确认动作：`on_ok`（键盘 ⏎）与底部主按钮各取一份 `Rc`。
        let confirm = Rc::clone(&confirm);
        // 勾选框 id 要用一份专用的：`content` 也是 `Fn`，直接捕获 `plugin_id` 会被移走。
        let id_for_rows = plugin_id.clone();
        let title = trf(cx, title_key, &[name.as_str()]);
        let lead = tr(cx, lead_key);
        let ok_text = tr(cx, ok_key);
        let cancel_text = tr(cx, "取消");
        let note = (added_count > 0).then(|| {
            trf(
                cx,
                "本次更新新增 {} 项权限，请确认。",
                &[&added_count.to_string()],
            )
        });
        let footer = tr(cx, "未在本机授予的能力一律被拒绝；卸载插件即收回全部权限。");
        let grant_hint = tr(
            cx,
            "勾选要放行的能力；取消勾选即拒绝，之后可在插件中心逐项调整。",
        );
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();

        dialog
            .w(px(INSTALL_DIALOG_W))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    // 按钮文案按弹窗渲染时的语言解析（弹窗开着时不跟随切换）。
                    .ok_text(ok_text)
                    .cancel_text(cancel_text)
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                let tokens = active_theme(cx).tokens().clone();
                let rows: Vec<gpui::AnyElement> = capability_rows(&capabilities, &added)
                    .into_iter()
                    .map(|row| {
                        let key = unit_key(&row.unit);
                        let toggle_id =
                            SharedString::from(format!("install-grant-{id_for_rows}-{key}"));
                        let checked = selection_for_rows.read(cx).allows(&row.unit);
                        let unit = row.unit.clone();
                        let selection = selection_for_rows.clone();
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.md)
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(CAPABILITY_LABEL_W))
                                    .text_size(tokens.typography.sm.size)
                                    .line_height(tokens.typography.sm.line_height)
                                    .text_color(tokens.colors.muted_foreground)
                                    .child(tr(cx, row.label)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(capability_value(row.value, row.is_new, cx)),
                            )
                            .child(div().flex_none().child(switch_control(
                                toggle_id,
                                checked,
                                move |value: &bool, _, cx| {
                                    selection.update(cx, |state, _| state.set(&unit, *value));
                                },
                            )))
                            .into_any_element()
                    })
                    .collect();

                content.child(modal_drag_area(
                    &drag_for_content,
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .child(modal_title_row(
                            "plugin-install-title",
                            &drag_for_content,
                            // `content` 闭包是 `Fn`（重排时会重跑），捕获的值只能借不能移。
                            title.clone(),
                            cx,
                        ))
                        .child(body_text(lead, cx))
                        .children(
                            note.clone()
                                .map(|note| body_text(note, cx).text_color(tokens.colors.primary)),
                        )
                        .child(v_flex().w_full().gap(tokens.spacing.xs).children(rows))
                        .child(field_hint(grant_hint, cx))
                        .child(field_hint(footer, cx)),
                ))
            })
            .on_ok({
                let confirm = Rc::clone(&confirm);
                move |_, window, cx| confirm(window, cx)
            })
            .footer(dialog_footer(
                "plugin-install",
                ok_text,
                ButtonVariant::Primary,
                confirm,
                cx,
            ))
    });
}

/// 一条能力声明的渲染行：标签 / 值 / 是否本次新增，以及它对应的**能力单元**。
///
/// `unit` 是勾选框的键。`Capability::set_of`（声明 → 单元）与 `Capabilities::only`
/// （单元 → 声明）是一对互逆映射，行里少写一个字段，界面上就会多一条「勾了不生效」。
struct CapabilityRow {
    label: &'static str,
    value: String,
    is_new: bool,
    unit: Capability,
}

/// 能力声明摊成**逐条**的行。
///
/// 不按类合并成一串：合并之后就没法给「这次新加的那一条」打标，而更新确认框
/// 真正要说的恰恰只有那一条。标签是中文源串，`tr` 留给渲染时取（本函数不碰 `cx`，
/// 于是可以单独测住）。
fn capability_rows(caps: &Capabilities, added: &HashSet<Capability>) -> Vec<CapabilityRow> {
    let mut rows: Vec<CapabilityRow> = Vec::new();
    let mut push = |label: &'static str, value: String, unit: Capability| {
        rows.push(CapabilityRow {
            label,
            value,
            is_new: added.contains(&unit),
            unit,
        });
    };

    for pattern in &caps.filesystem_read {
        push(
            "读取文件",
            pattern.clone(),
            Capability::FilesystemRead(pattern.clone()),
        );
    }
    for pattern in &caps.filesystem_write {
        push(
            "写入文件",
            pattern.clone(),
            Capability::FilesystemWrite(pattern.clone()),
        );
    }
    for host in &caps.network_hosts {
        push("网络", host.clone(), Capability::NetworkHost(host.clone()));
    }
    if caps.notifications {
        push("通知", "✓".to_string(), Capability::Notifications);
    }
    for command in &caps.process_exec {
        push(
            "进程执行",
            command.clone(),
            Capability::ProcessExec(command.clone()),
        );
    }
    rows
}

/// 能力单元的稳定键（勾选框的 `ElementId` 用）。
///
/// **不用行下标**：声明一变顺序就变，下标一旦错位，界面上的勾会落到别人身上
/// （AGENTS 4 对「领域派生的 `ElementId`」的要求）。前缀按能力类别分叉：读 `a` 与写 `a`
/// 是两条不同的行，键撞上就会让两个开关共用一个 `ElementId`（同窗口两个同 id 会串）。
fn unit_key(unit: &Capability) -> String {
    match unit {
        Capability::FilesystemRead(pattern) => format!("read:{pattern}"),
        Capability::FilesystemWrite(pattern) => format!("write:{pattern}"),
        Capability::NetworkHost(host) => format!("net:{host}"),
        Capability::Notifications => "notify".to_owned(),
        Capability::ProcessExec(command) => format!("exec:{command}"),
    }
}

/// 安装确认框里「勾哪些能力」的勾选状态。
///
/// 必须是实体：`Dialog::content` 的闭包是 `Fn`，会被反复调用（重排 / 重绘），局部变量
/// 活不过一轮，而同一次弹窗里的多个开关要共享同一份勾选结果。
struct GrantSelection {
    allowed: HashSet<Capability>,
}

impl GrantSelection {
    fn new(initial: HashSet<Capability>) -> Self {
        Self { allowed: initial }
    }

    fn allows(&self, unit: &Capability) -> bool {
        self.allowed.contains(unit)
    }

    fn set(&mut self, unit: &Capability, allowed: bool) {
        if allowed {
            self.allowed.insert(unit.clone());
        } else {
            self.allowed.remove(unit);
        }
    }

    fn snapshot(&self) -> HashSet<Capability> {
        self.allowed.clone()
    }
}

/// 一条能力的值：值本身 + 可选「新增」小标。
fn capability_value(value: String, is_new: bool, cx: &App) -> gpui::AnyElement {
    h_flex()
        .items_center()
        .gap(active_theme(cx).tokens().spacing.sm)
        .child(body_text(value, cx))
        .children(is_new.then(|| content::accent_tag(tr(cx, "新增"), cx)))
        .into_any_element()
}

/// 把字节数说成「512 B / 4.0 KB / 1.2 MB」。
fn format_size(bytes: u64) -> String {
    let value = bytes as f64;
    if value >= 1024.0 * 1024.0 {
        format!("{:.1} MB", value / (1024.0 * 1024.0))
    } else if value >= 1024.0 {
        format!("{:.0} KB", value / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// 把秒数说成「刚刚 / N 分钟前 / N 小时前 / N 天前」。
///
/// 分档逻辑已下沉到 `duck-theme` 的格式化层（[`tr_relative_time`]）——本仓原先在
/// 这里、`duck-shell::notifications`、`redis.rs` 各写了一遍，且三处的英文复数都是错的
/// （`1 minutes ago`）。「不满一分钟」取「刚刚」档，与通知中心口径一致。
fn format_age(seconds: u64, cx: &App) -> String {
    tr_relative_time(cx, seconds as i64, UnderMinute::JustNow)
}

/// 顶栏入口按钮：语言切换左侧的插件中心入口（图标 + 文字，样式对齐 language_switch）。
pub(crate) fn title_bar_button(cx: &App) -> gpui::AnyElement {
    let theme = active_theme(cx);
    let label = SharedString::from(tr(cx, "插件中心"));
    let tooltip_label = label.clone();
    div()
        .id("shell-plugin-center-tooltip")
        .flex_none()
        // 顶栏整条是拖动区：不拦下鼠标按下，点一下会变成拖窗口。
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
        .child(
            toolbar_action_button(
                "shell-plugin-center",
                label.clone(),
                // 图标 + 文字横排：xs 档字号，色随按钮的 muted_foreground。
                h_flex()
                    .items_center()
                    // 图标与文案属「同一控件的两部分」→ spacing.xs（原裸值 3，off-scale）。
                    .gap(theme.tokens().spacing.xs)
                    .child(Icon::new(DuckIcon::Layers).size(theme.extended().icon.md))
                    .child(
                        div()
                            .text_size(theme.tokens().typography.xs.size)
                            .child(label),
                    ),
                false,
                false,
                cx,
            )
            .on_click(|_, _, cx| open_plugin_center(cx)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{capability_rows, unit_key};
    use duck_plugin::{Capabilities, Capability};
    use std::collections::HashSet;

    /// 五类能力各来一条，外加两条路径，确认**逐条**摊开而不是按类合并。
    #[test]
    fn every_declared_unit_gets_its_own_row() {
        let caps = Capabilities {
            filesystem_read: vec!["data/*".into()],
            filesystem_write: vec!["out/*".into(), "last-report.txt".into()],
            network_hosts: vec!["api.example.com".into()],
            notifications: true,
            process_exec: vec!["ffmpeg".into()],
        };

        let rows = capability_rows(&caps, &HashSet::new());
        let labels: Vec<&str> = rows.iter().map(|row| row.label).collect();
        assert_eq!(
            labels,
            [
                "读取文件",
                "写入文件",
                "写入文件",
                "网络",
                "通知",
                "进程执行"
            ]
        );
        assert_eq!(rows[2].value, "last-report.txt");
        assert!(rows.iter().all(|row| !row.is_new));
    }

    /// 不声明就一行都不出——尤其 `notifications = false` 不该冒出一行「通知 ✓」。
    #[test]
    fn nothing_declared_means_no_rows() {
        assert!(capability_rows(&Capabilities::default(), &HashSet::new()).is_empty());
    }

    /// 「新增」只打在真正多出来的那一条上，同类的其他条不受牵连。
    #[test]
    fn only_the_added_unit_is_flagged() {
        let caps = Capabilities {
            network_hosts: vec!["api.example.com".into(), "exfil.example.com".into()],
            notifications: true,
            ..Capabilities::default()
        };
        let added: HashSet<Capability> = [Capability::NetworkHost("exfil.example.com".into())]
            .into_iter()
            .collect();

        let rows = capability_rows(&caps, &added);
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].is_new, "the pre-existing host must not be flagged");
        assert!(rows[1].is_new, "the newly requested host must be flagged");
        assert!(!rows[2].is_new);
    }

    /// 每一行的 `unit` 必须与「声明 → 单元」的映射一一对上：勾一行就只放行那一行。
    ///
    /// 这条是授权界面的地基。`capability_rows` 漏带一个 `unit`（例如 `process_exec`
    /// 忘了写），界面上就多一条「勾了不生效」的行——而它是**静默**的，从界面上看不出来。
    #[test]
    fn every_row_unit_round_trips_through_only() {
        let caps = Capabilities {
            filesystem_read: vec!["data/*".into()],
            filesystem_write: vec!["out/*".into(), "last-report.txt".into()],
            network_hosts: vec!["api.example.com".into()],
            notifications: true,
            process_exec: vec!["ffmpeg".into()],
        };
        let rows = capability_rows(&caps, &HashSet::new());

        for row in &rows {
            let just_this: HashSet<Capability> = [row.unit.clone()].into_iter().collect();
            let kept = Capability::set_of(&caps.only(&just_this));
            assert_eq!(kept.len(), 1, "只勾一行应当只留一个单元：{:?}", row.unit);
            assert!(kept.contains(&row.unit));
        }

        // 另一头：全勾回去必须等于声明本身（少一条就说明有行是白勾的）。
        let all: HashSet<Capability> = rows.iter().map(|row| row.unit.clone()).collect();
        assert_eq!(caps.only(&all), caps);
    }

    /// 单元键两两不同：读 `a` 与写 `a` 是两条行，撞键会让两个开关共用同一个 `ElementId`。
    #[test]
    fn unit_keys_are_distinct() {
        let caps = Capabilities {
            filesystem_read: vec!["same".into()],
            filesystem_write: vec!["same".into()],
            network_hosts: vec!["same".into()],
            notifications: true,
            process_exec: vec!["same".into()],
        };
        let rows = capability_rows(&caps, &HashSet::new());
        let keys: Vec<String> = rows.iter().map(|row| unit_key(&row.unit)).collect();
        let unique: HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "unit keys collided: {keys:?}");
    }
}
