//! 命令面板的数据源：把工具注册表与设置分类并入面板，并处理「打开工具」
//! 「跳到设置分类」两个动作。
//!
//! **过滤与排序也在这里**。面板挂着 `filterable(false)`，上游把所有条目原样留着，
//! 命中的那一份由本模块按 [`duck_command::fuzzy`] 的打分产出——所以这里产出的分组
//! 顺序、组内顺序就是最终呈现顺序。
//!
//! 分组顺序：**最近使用 → 动作（导航 / 视图 / …）→ 设置 → 工具（按分类）**。
//! 「最近使用」置顶但**不从分类组里摘掉**（同一工具会出现在两处）——想按分类浏览时
//! 列表不会因为最近项被抽走而残缺。

use duck_command::fuzzy;
use duck_components::{ButtonVariant, body_text, button, meta_text};
use duck_theme::{active_theme, t, tr};
use duck_tools::{RegisteredTool, ToolCategory, ToolRegistry};
use gpui::{
    AnyElement, App, IntoElement as _, ParentElement as _, SharedString, Styled as _, Window, px,
};
use gpui_component::{
    Icon,
    command::{CommandGroup, CommandItem},
    v_flex,
};

use crate::actions::ids;
use crate::pages::{open_plugin_center, palette_page_key, palette_pages};

/// 「最近使用」组的条数上限。
const RECENT_LIMIT: usize = 5;
/// 「最近使用」组的标题。
const RECENT_GROUP: &str = "最近使用";
/// 「设置」组的标题。
const SETTINGS_GROUP: &str = "设置";

/// 打开工具：命令面板确认后派发；落点与工具箱侧栏点击完全同路（处理器见 `app.rs`）。
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = duck, no_json)]
pub struct OpenTool {
    /// 目标工具稳定 id。
    pub id: SharedString,
}

/// 打开设置窗口并定位到某个分类（面板「设置」分组）。
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = duck, no_json)]
pub struct OpenSettingsPage {
    /// 分类稳定 key，见 `pages::settings` 的分类清单。
    pub page: SharedString,
}

/// 面板完整数据源：最近使用 → 动作 → 设置 → 工具（按分类）。
pub fn palette_groups(cx: &App, query: &str) -> Vec<CommandGroup> {
    let mut groups = Vec::new();
    groups.extend(recent_group(cx, query));
    groups.extend(duck_command::palette_groups(cx, query));
    groups.extend(settings_group(cx, query));
    groups.extend(tool_groups(cx, query));
    groups
}

/// 面板空结果态：面板只列得出**已装**的工具，装新的得去插件市场，所以这里给条明路。
pub fn palette_empty(_window: &mut Window, cx: &mut App) -> AnyElement {
    v_flex()
        .w_full()
        .items_center()
        .gap(px(10.))
        .py(px(26.))
        .child(body_text(tr(cx, "没有匹配的项"), cx))
        .child(meta_text(
            tr(cx, "换个关键词试试，或到插件市场安装更多工具。"),
            cx,
        ))
        .child(
            button(
                "palette-empty-plugins",
                tr(cx, "打开插件中心"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(|_, window, cx| {
                open_plugin_center(cx);
                // 引导按钮把用户送去别处了，面板留着没意义——顺手关掉。
                duck_command::dispatch(ids::PALETTE_TOGGLE, window, cx);
            }),
        )
        .into_any_element()
}

/// 「最近使用」组：注册表里最近被打开过的工具，置顶。
///
/// 空查询时按**时间**倒序（`recent` 的原始顺序）——这正是「接着上次」的语义；
/// 一旦有查询串，名次比时间更贴题，改按打分排。
fn recent_group(cx: &App, query: &str) -> Option<CommandGroup> {
    let mut hits = Vec::new();
    for (position, tool) in ToolRegistry::recent(cx, RECENT_LIMIT)
        .into_iter()
        .enumerate()
    {
        if !ToolRegistry::is_listed(cx, tool) {
            continue;
        }
        let Some((rank, item)) = tool_item(cx, tool, query) else {
            continue;
        };
        hits.push((rank, position, item));
    }
    if !query.trim().is_empty() {
        fuzzy::rank(&mut hits);
    }
    build_group(cx, RECENT_GROUP, hits)
}

/// 「设置」组：每个设置分类一条，确认后打开设置窗口并定位过去。
///
/// 匹配词铺得比标签宽：分类 key（拉丁）、中英「设置」、标题与描述的**两种语言**。
/// 用户搜「主题」要能找到「外观」页，搜 `theme` 同样要能找到。
fn settings_group(cx: &App, query: &str) -> Option<CommandGroup> {
    let lang = active_theme(cx).lang();
    let mut hits = Vec::new();
    for (position, (key, title, description, icon)) in palette_pages().enumerate() {
        let label = tr(cx, title);
        let terms = [
            key,
            SETTINGS_GROUP,
            "settings",
            title,
            t(lang, title),
            description,
            t(lang, description),
        ];
        let Some(rank) = fuzzy::score(label, &terms, query) else {
            continue;
        };
        let item = CommandItem::new()
            .label(label)
            .icon(Icon::new(icon))
            .action(Box::new(OpenSettingsPage {
                page: SharedString::from(key),
            }));
        hits.push((rank, position, item));
    }
    fuzzy::rank(&mut hits);
    build_group(cx, SETTINGS_GROUP, hits)
}

/// 工具分组：按分类聚合**可列出**的工具（dev 过滤 / 停用插件贡献照常剔除），
/// 分类顺序与工具箱侧栏一致；空分类不产生分组。
fn tool_groups(cx: &App, query: &str) -> Vec<CommandGroup> {
    ToolCategory::ALL
        .iter()
        .filter_map(|category| {
            let mut hits = Vec::new();
            for (position, tool) in ToolRegistry::all(cx).iter().enumerate() {
                if tool.meta().category() != *category || !ToolRegistry::is_listed(cx, tool) {
                    continue;
                }
                let Some((rank, item)) = tool_item(cx, tool, query) else {
                    continue;
                };
                hits.push((rank, position, item));
            }
            fuzzy::rank(&mut hits);
            build_group(cx, category.label(), hits)
        })
        .collect()
}

/// 一条工具条目：不匹配查询串时返回 `None`。
fn tool_item(cx: &App, tool: &RegisteredTool, query: &str) -> Option<(i32, CommandItem)> {
    let meta = tool.meta();
    // 匹配词：声明关键词 + 拼音。空串会命中一切查询，必须剔除。
    let mut terms: Vec<&str> = meta.keywords().to_vec();
    for term in [tool.initials(), tool.plain()] {
        if !term.is_empty() {
            terms.push(term);
        }
    }

    let label = tr(cx, meta.name());
    let rank = fuzzy::score(label, &terms, query)?;

    let mut item = CommandItem::new()
        .label(label)
        .icon(Icon::new(meta.icon()))
        .action(Box::new(OpenTool {
            id: SharedString::from(tool.id()),
        }));
    if !terms.is_empty() {
        item = item.keywords(terms);
    }
    Some((rank, item))
}

/// 组装一个分组；一条命中都没有时返回 `None`（组标题不该空挂着）。
///
/// 标题必须是 `'static`——它同时是 `tr` 的查表键。
fn build_group(
    cx: &App,
    title: &'static str,
    hits: Vec<(i32, usize, CommandItem)>,
) -> Option<CommandGroup> {
    if hits.is_empty() {
        return None;
    }
    let mut builder = CommandGroup::new().label(tr(cx, title));
    for (_, _, item) in hits {
        builder = builder.item(item);
    }
    Some(builder)
}

/// 把面板条目携带的分类 key 还原成静态 key（处理器用）。
///
/// 认不出来就返回 `None`——条目快照可能来自旧版本。
#[must_use]
pub fn settings_page_of(action: &OpenSettingsPage) -> Option<&'static str> {
    palette_page_key(&action.page)
}

#[cfg(test)]
mod tests {
    use super::palette_groups;
    use crate::actions;
    use duck_components::DuckIcon;
    use duck_theme::tr;
    use duck_tools::{ToolCategory, ToolMeta, ToolRegistry};
    use gpui::{App, TestAppContext};
    use std::path::PathBuf;

    /// 测试工具的 id（够特别，不会和真实注册表撞名）。
    const TOOL_ID: &str = "palette.test.only";

    /// 铺底座，顺序照 `main.rs`：组件库 → 主题。
    ///
    /// 再挂上动作注册表与工具注册表——本模块的三个数据源都要它们才出得来条目。
    fn boot(cx: &mut App) {
        gpui_component::init(cx);
        duck_theme::init(cx);
        duck_command::install(cx);
        ToolRegistry::install(cx);
        actions::declare_all(cx);
    }

    /// 登记一个只属于本测试的工具，归到「媒体处理」分类。
    fn register_tool(cx: &mut App, name: &'static str) {
        ToolRegistry::register(
            cx,
            ToolMeta::builder(TOOL_ID, name, DuckIcon::Film)
                .category(ToolCategory::Media)
                .build(),
        );
    }

    /// 装一个**临时**库。
    ///
    /// 不用 `Store::install`——那会写到真实用户数据目录去，测试不该碰那里。
    fn install_temp_store(cx: &mut App, tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("duck-palette-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = duck_store::Store::open(path.clone()).expect("临时库应能打开");
        cx.set_global(store);
        path
    }

    /// 组标题序列（顺序即渲染顺序）。
    ///
    /// 标题是**译文**（语言跟随系统），断言一律拿 `tr(cx, …)` 对照。
    fn headings(cx: &App, query: &str) -> Vec<String> {
        palette_groups(cx, query)
            .iter()
            .map(|group| group.heading().map(ToString::to_string).unwrap_or_default())
            .collect()
    }

    fn contains(cx: &App, groups: &[String], text: &'static str) -> bool {
        groups.iter().any(|group| group == tr(cx, text))
    }

    #[gpui::test]
    fn settings_group_sits_between_the_actions_and_the_tools(cx: &mut TestAppContext) {
        cx.update(|cx| {
            boot(cx);
            register_tool(cx, "单元测试专用工具");

            let groups = headings(cx, "");
            assert_eq!(
                groups.last().map(String::as_str),
                Some(tr(cx, ToolCategory::Media.label())),
                "工具分类组排在最后"
            );
            assert_eq!(
                groups[groups.len() - 2],
                tr(cx, "设置"),
                "设置组紧挨在工具分类组之前"
            );
            assert!(
                !contains(cx, &groups, "最近使用"),
                "没用过任何工具时不该冒出「最近使用」组"
            );
        });
    }

    #[gpui::test]
    fn a_tool_hit_lands_in_its_own_category_group(cx: &mut TestAppContext) {
        cx.update(|cx| {
            boot(cx);
            register_tool(cx, "单元测试专用工具");

            // 这个查询串只可能命中工具：动作与设置项的文案里都没有这两字。
            assert_eq!(
                headings(cx, "单元测试"),
                vec![tr(cx, ToolCategory::Media.label())]
            );
        });
    }

    #[gpui::test]
    fn the_recent_group_leads_the_list_once_a_tool_has_been_used(cx: &mut TestAppContext) {
        cx.update(|cx| {
            boot(cx);
            register_tool(cx, "单元测试专用工具");
            let path = install_temp_store(cx, "recent");
            ToolRegistry::mark_used(cx, TOOL_ID);
            // 写是 400ms 去抖的，读快照前必须 flush——否则「最近」还是空的。
            duck_store::flush(cx);

            let groups = headings(cx, "");
            assert_eq!(
                groups.first().map(String::as_str),
                Some(tr(cx, "最近使用")),
                "最近使用组置顶"
            );
            assert!(
                contains(cx, &groups, ToolCategory::Media.label()),
                "同一工具在分类组里照旧保留（允许重复出现）"
            );

            let _ = std::fs::remove_file(path);
        });
    }

    #[gpui::test]
    fn a_query_matching_nothing_yields_no_group_at_all(cx: &mut TestAppContext) {
        cx.update(|cx| {
            boot(cx);
            register_tool(cx, "单元测试专用工具");
            assert!(headings(cx, "zzzz-不存在").is_empty());
        });
    }
}
