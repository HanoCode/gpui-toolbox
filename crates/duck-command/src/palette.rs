//! 命令面板索引：把注册表里标记 [`ActionSlot::Palette`](crate::ActionSlot::Palette) 的动作
//! 投影成 gpui-component 的 [`CommandGroup`] 列表。
//!
//! 面板的渲染与交互由 `gpui_component::command::{Command, CommandState}` 负责；本模块只产出
//! 数据。每个条目挂一个 [`RunCommand`]，因此「键位提示」由面板自行从键盘映射解析。
//!
//! **过滤与排序也在这里**：面板挂 `filterable(false)`，上游把所有条目原样留着，命中的
//! 那一份由本模块按 [`crate::fuzzy`] 的打分产出。组内按分数降序，同分保持注册顺序，
//! 空查询（`score` 恒为 0）于是自然退化成「按注册顺序原样列出」。

use std::collections::HashMap;

use gpui::App;
use gpui_component::command::{CommandGroup, CommandItem};

use duck_theme::tr;

use crate::action::{ActionSlot, RunCommand, metas};
use crate::fuzzy;

/// 未声明分组的动作归入此组。
const DEFAULT_GROUP: &str = "命令";

/// 按注册顺序产出的命令面板分组；`query` 为空时即全量清单。
///
/// 不匹配的动作整条消失，不匹配的动作填不满的组也随之消失。
#[must_use]
pub fn palette_groups(cx: &App, query: &str) -> Vec<CommandGroup> {
    let mut order: Vec<&'static str> = Vec::new();
    let mut buckets: HashMap<&'static str, Vec<(i32, usize, CommandItem)>> = HashMap::new();

    for (position, meta) in metas(cx).iter().enumerate() {
        if !meta.slots.contains(&ActionSlot::Palette) {
            continue;
        }
        let label = (meta.label)(cx);
        let Some(rank) = fuzzy::score(&label, meta.keywords, query) else {
            continue;
        };
        let group = meta.group.unwrap_or(DEFAULT_GROUP);
        if !buckets.contains_key(group) {
            order.push(group);
        }

        let mut item = CommandItem::new().label(label);
        if let Some(icon) = meta.icon {
            item = item.icon(icon(cx));
        }
        if !meta.keywords.is_empty() {
            item = item.keywords(meta.keywords.iter().copied());
        }
        item = item.action(Box::new(RunCommand { id: meta.id.into() }));
        buckets
            .entry(group)
            .or_default()
            .push((rank, position, item));
    }

    order
        .into_iter()
        .filter_map(|group| {
            let mut items = buckets.remove(group)?;
            fuzzy::rank(&mut items);
            let mut builder = CommandGroup::new().label(tr(cx, group));
            for (_, _, item) in items {
                builder = builder.item(item);
            }
            Some(builder)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::palette_groups;
    use crate::action::{ActionMeta, ActionSlot, declare, install};
    use duck_theme::tr;
    use gpui::{App, SharedString, TestAppContext};

    /// 登记一条只进命令面板的动作，挂到指定分组。
    ///
    /// 用宏而不是函数：`ActionMeta::label` 是 **fn 指针**（不能捕获环境），
    /// 把字面量直接写进闭包体里才不捕获。
    macro_rules! declare_action {
        ($cx:expr, $id:literal, $label:literal, $group:literal) => {
            declare(
                $cx,
                ActionMeta {
                    id: $id,
                    label: |_| SharedString::from($label),
                    keywords: &[],
                    group: Some($group),
                    icon: None,
                    icon_color: None,
                    slots: &[ActionSlot::Palette],
                    menu: None,
                    keys: &[],
                },
            )
        };
    }

    /// 结果里的组标题序列（顺序即渲染顺序）。
    ///
    /// 只取标题：每个组里有几条、什么顺序由 `fuzzy::rank` 的测试盯着，
    /// `CommandGroup` 的条目字段是上游私有的，读不到。
    ///
    /// 标题是**译文**（语言跟随系统），所以断言一律拿 `tr(cx, …)` 对照，
    /// 不写裸中文。
    fn headings(cx: &App, query: &str) -> Vec<String> {
        palette_groups(cx, query)
            .iter()
            .map(|group| group.heading().map(ToString::to_string).unwrap_or_default())
            .collect()
    }

    /// 组标题要过 `tr`，而 `tr` 读主题状态——没铺底座会 panic。
    /// 顺序照 `main.rs`：组件库 → 主题。
    fn boot(cx: &mut App) {
        gpui_component::init(cx);
        duck_theme::init(cx);
        install(cx);
    }

    #[gpui::test]
    fn an_empty_query_lists_every_group_in_registration_order(cx: &mut TestAppContext) {
        cx.update(|cx| {
            boot(cx);
            declare_action!(cx, "t.b", "批量重命名", "文档");
            declare_action!(cx, "t.a", "视频统一解析", "媒体");
            declare_action!(cx, "t.c", "归档解压", "压缩");

            assert_eq!(
                headings(cx, ""),
                vec![tr(cx, "文档"), tr(cx, "媒体"), tr(cx, "压缩")]
            );
        });
    }

    #[gpui::test]
    fn a_group_survives_only_while_it_still_has_a_match(cx: &mut TestAppContext) {
        cx.update(|cx| {
            boot(cx);
            declare_action!(cx, "t.a", "视频统一解析", "媒体");
            declare_action!(cx, "t.b", "批量重命名", "文档");

            // 「视频」只命中媒体组：文档组整组消失，媒体组照旧。
            assert_eq!(headings(cx, "视频"), vec![tr(cx, "媒体")]);
            // 「重命名」反过来。
            assert_eq!(headings(cx, "重命名"), vec![tr(cx, "文档")]);
        });
    }

    #[gpui::test]
    fn a_query_matching_nothing_yields_no_group_at_all(cx: &mut TestAppContext) {
        cx.update(|cx| {
            boot(cx);
            declare_action!(cx, "t.a", "视频统一解析", "媒体");
            assert!(headings(cx, "zzz-不存在").is_empty());
        });
    }

    #[gpui::test]
    fn a_gap_of_characters_still_finds_the_action(cx: &mut TestAppContext) {
        cx.update(|cx| {
            boot(cx);
            declare_action!(cx, "t.a", "视频统一解析", "媒体");
            // 「视解」不是连续片段——只有子序列兜底能救回来。
            assert_eq!(headings(cx, "视解"), vec![tr(cx, "媒体")]);
        });
    }
}
