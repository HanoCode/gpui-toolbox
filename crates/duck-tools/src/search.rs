//! 工具搜索：加权打分匹配。
//!
//! 匹配目标与权重（高 → 低）：名称全等 → 名称前缀 → 首字母前缀 → 全拼前缀 →
//! 名称包含 → 关键词全等 / 前缀 / 包含 → 全拼包含 → 描述包含。
//! 命中同分时保持注册顺序，结果稳定。

use crate::registry::RegisteredTool;

/// 名称完全一致。
const SCORE_NAME_EXACT: i32 = 100;
/// 名称前缀。
const SCORE_NAME_PREFIX: i32 = 80;
/// 首字母前缀（`sptyjx` → 视频统一解析）。
const SCORE_INITIALS_PREFIX: i32 = 70;
/// 全拼前缀。
const SCORE_PLAIN_PREFIX: i32 = 60;
/// 名称包含。
const SCORE_NAME_CONTAINS: i32 = 50;
/// 关键词完全一致。
const SCORE_KEYWORD_EXACT: i32 = 45;
/// 关键词前缀。
const SCORE_KEYWORD_PREFIX: i32 = 40;
/// 全拼包含。
const SCORE_PLAIN_CONTAINS: i32 = 25;
/// 关键词包含。
const SCORE_KEYWORD_CONTAINS: i32 = 20;
/// 描述包含。
const SCORE_DESCRIPTION_CONTAINS: i32 = 10;

/// 单个工具对查询串的匹配得分；`None` 表示不匹配。
///
/// 空查询视为「无条件命中」，得 0 分（调用方按注册顺序原样返回）。
#[must_use]
pub fn score(tool: &RegisteredTool, query: &str) -> Option<i32> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }

    let meta = tool.meta();
    let name = meta.name().to_lowercase();
    let mut best = 0;
    let mut matched = false;

    if name == query {
        best = best.max(SCORE_NAME_EXACT);
        matched = true;
    } else if name.starts_with(&query) {
        best = best.max(SCORE_NAME_PREFIX);
        matched = true;
    } else if name.contains(&query) {
        best = best.max(SCORE_NAME_CONTAINS);
        matched = true;
    }

    if tool.initials().starts_with(&query) {
        best = best.max(SCORE_INITIALS_PREFIX);
        matched = true;
    }
    if tool.plain().starts_with(&query) {
        best = best.max(SCORE_PLAIN_PREFIX);
        matched = true;
    } else if tool.plain().contains(&query) {
        best = best.max(SCORE_PLAIN_CONTAINS);
        matched = true;
    }

    for keyword in meta.keywords() {
        let keyword = keyword.to_lowercase();
        if keyword == query {
            best = best.max(SCORE_KEYWORD_EXACT);
            matched = true;
        } else if keyword.starts_with(&query) {
            best = best.max(SCORE_KEYWORD_PREFIX);
            matched = true;
        } else if keyword.contains(&query) {
            best = best.max(SCORE_KEYWORD_CONTAINS);
            matched = true;
        }
    }

    if meta.description().to_lowercase().contains(&query) {
        best = best.max(SCORE_DESCRIPTION_CONTAINS);
        matched = true;
    }

    matched.then_some(best)
}

/// 对一批工具做搜索，按得分降序（同分保持注册顺序）。
#[must_use]
pub fn search<'a>(tools: &[&'a RegisteredTool], query: &str) -> Vec<&'a RegisteredTool> {
    let mut hits: Vec<(i32, usize, &RegisteredTool)> = tools
        .iter()
        .enumerate()
        .filter_map(|(position, tool)| score(tool, query).map(|value| (value, position, *tool)))
        .collect();
    hits.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    hits.into_iter().map(|(_, _, tool)| tool).collect()
}

#[cfg(test)]
mod tests {
    use super::{score, search};
    use crate::category::ToolCategory;
    use crate::meta::ToolMeta;
    use crate::registry::ToolIndex;
    use duck_components::DuckIcon;

    fn index() -> ToolIndex {
        let mut index = ToolIndex::default();
        index.insert(
            ToolMeta::builder("video.parse", "视频统一解析", DuckIcon::Film)
                .description("CDP 控制本地浏览器嗅探媒体地址")
                .keywords(&["video", "媒体"])
                .category(ToolCategory::Media)
                .build(),
        );
        index.insert(
            ToolMeta::builder("file.rename", "批量重命名", DuckIcon::FileText)
                .keywords(&["rename", "文件名"])
                .category(ToolCategory::Document)
                .build(),
        );
        index
    }

    #[test]
    fn empty_query_keeps_registration_order() {
        let index = index();
        let all: Vec<_> = index.all().iter().collect();
        let hits = search(&all, "   ");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id(), "video.parse");
        assert_eq!(hits[1].id(), "file.rename");
    }

    #[test]
    fn matches_by_initials_pinyin_and_name() {
        let index = index();
        let video = index.get("video.parse").expect("应已登记");
        // 首字母
        assert_eq!(score(video, "sptyjx"), Some(super::SCORE_INITIALS_PREFIX));
        // 全拼
        assert_eq!(score(video, "shipin"), Some(super::SCORE_PLAIN_PREFIX));
        // 名称前缀
        assert_eq!(score(video, "视频"), Some(super::SCORE_NAME_PREFIX));
        // 关键词
        assert_eq!(score(video, "video"), Some(super::SCORE_KEYWORD_EXACT));
        // 描述
        assert!(score(video, "youlan").is_none(), "描述里没有该词");
        assert_eq!(
            score(video, "浏览器"),
            Some(super::SCORE_DESCRIPTION_CONTAINS)
        );
    }

    #[test]
    fn ranking_prefers_exact_name_over_description() {
        let index = index();
        let all: Vec<_> = index.all().iter().collect();
        let hits = search(&all, "批量重命名");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id(), "file.rename");
    }

    #[test]
    fn no_match_returns_empty() {
        let index = index();
        let all: Vec<_> = index.all().iter().collect();
        assert!(search(&all, "zzzz-不存在").is_empty());
    }
}
