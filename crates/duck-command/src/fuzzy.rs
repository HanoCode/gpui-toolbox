//! 命令面板的模糊匹配打分器：**纯字符串、零依赖**。
//!
//! 面板的过滤权在宿主手里（`Command::filterable(false)` 上游默认把所有条目留着），
//! 本模块就是那个「外部答案」。三类候选（动作 / 工具 / 设置项）都归约成
//! 「一个标签 + 若干附加词」，所以这里只认字符串，不认业务类型。
//!
//! 档位从高到低：标签全等 → 标签前缀 → 附加词全等 → 标签包含 → 附加词前缀 →
//! 附加词包含 → 子序列（跳跃命中）。**子序列一律低于任何连续命中**——否则
//! 「解析」的两次跳跃会盖过真正的包含命中。
//!
//! 附加词（拼音首字母 / 全拼 / 别名）**不参与子序列**：拼音串动辄十几个字母，
//! 两个字母的查询几乎必然构成子序列，会淹没有效结果。跳跃命中只认标签。

/// 标签完全一致。
pub const SCORE_EXACT: i32 = 1000;
/// 标签前缀。
pub const SCORE_PREFIX: i32 = 900;
/// 附加词完全一致（拼音 / 别名）。
pub const SCORE_WORD_EXACT: i32 = 860;
/// 标签包含。
pub const SCORE_CONTAINS: i32 = 800;
/// 附加词前缀。
pub const SCORE_WORD_PREFIX: i32 = 760;
/// 附加词包含。
pub const SCORE_WORD_CONTAINS: i32 = 700;
/// 子序列命中的基准分。
const SCORE_SUBSEQUENCE_BASE: i32 = 200;
/// 子序列命中的上界（下界是 1，0 留给「空查询无条件命中」）。
pub const SCORE_SUBSEQUENCE_MAX: i32 = 600;

/// 档位阶梯的**编译期**校验：文档注释里写的顺序一旦对不上就编不过。
///
/// 这条不变量是打分器的全部意义所在（「连续命中永远压过跳跃命中」），
/// 用运行期测试守反而会在改错档位时先绿后红。
const _: () = {
    assert!(SCORE_EXACT > SCORE_PREFIX);
    assert!(SCORE_PREFIX > SCORE_WORD_EXACT);
    assert!(SCORE_WORD_EXACT > SCORE_CONTAINS);
    assert!(SCORE_CONTAINS > SCORE_WORD_PREFIX);
    assert!(SCORE_WORD_PREFIX > SCORE_WORD_CONTAINS);
    assert!(SCORE_WORD_CONTAINS > SCORE_SUBSEQUENCE_MAX);
    assert!(SCORE_SUBSEQUENCE_MAX > 0);
    assert!(SCORE_SUBSEQUENCE_BASE > 0);
};

/// 对一条候选打分：`None` 表示不匹配。
///
/// 空查询（或纯空白）视为「无条件命中」，返回 `Some(0)`，让调用方按原顺序原样列出。
#[must_use]
pub fn score(label: &str, words: &[&str], query: &str) -> Option<i32> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }

    let label = label.to_lowercase();
    let mut best = 0;
    let mut matched = false;
    let mut note = |value: i32| {
        if value > best {
            best = value;
        }
    };

    // 标签三档。
    if label == query {
        note(SCORE_EXACT);
        matched = true;
    } else if label.starts_with(&query) {
        note(SCORE_PREFIX);
        matched = true;
    } else if label.contains(&query) {
        note(SCORE_CONTAINS);
        matched = true;
    }

    // 附加词三档。空串会命中一切查询（`contains("")` 恒真），必须剔除。
    for word in words.iter().map(|word| word.to_lowercase()) {
        if word.is_empty() {
            continue;
        }
        if word == query {
            note(SCORE_WORD_EXACT);
            matched = true;
        } else if word.starts_with(&query) {
            note(SCORE_WORD_PREFIX);
            matched = true;
        } else if word.contains(&query) {
            note(SCORE_WORD_CONTAINS);
            matched = true;
        }
    }

    // 兜底：标签上的跳跃命中。
    if let Some(jump) = subsequence(&label, &query) {
        note(jump);
        matched = true;
    }

    matched.then_some(best)
}

/// 子序列命中：`query` 的字符在 `text` 中按顺序出现（不要求连续）。
///
/// 分值 = 基准 + 连续命中奖励 − 跨度松弛惩罚 − 起始位置惩罚，压在
/// `1..=SCORE_SUBSEQUENCE_MAX`，因此**永远低于任何连续命中**。
fn subsequence(text: &str, query: &str) -> Option<i32> {
    let haystack: Vec<char> = text.chars().collect();
    let needle: Vec<char> = query.chars().collect();
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }

    let mut hits: Vec<usize> = Vec::with_capacity(needle.len());
    let mut cursor = 0;
    for ch in &needle {
        let offset = haystack.get(cursor..)?.iter().position(|item| item == ch)?;
        let index = cursor + offset;
        hits.push(index);
        cursor = index + 1;
    }

    let first = *hits.first()?;
    let last = *hits.last()?;
    let adjacent = hits
        .windows(2)
        .filter(|pair| pair[1] == pair[0] + 1)
        .count();
    let slack = last - first + 1 - needle.len();

    let raw =
        SCORE_SUBSEQUENCE_BASE + (adjacent as i32) * 30 - (slack as i32) * 10 - (first as i32) * 3;
    Some(raw.clamp(1, SCORE_SUBSEQUENCE_MAX))
}

/// 组内排序：分数降序，同分回到注册顺序。
///
/// `usize` 是候选的注册位置。对载荷泛型只是为了能被单测直接喂进去——
/// `CommandGroup` 的条目是上游私有字段，从外面读不到顺序，所以这条规则必须能在
/// 不碰上游类型的前提下验证。
pub fn rank<T>(items: &mut [(i32, usize, T)]) {
    // 显式带上位置，免得日后换成不稳定排序时同分行为悄悄改变。
    items.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
}

#[cfg(test)]
mod tests {
    use super::{
        SCORE_CONTAINS, SCORE_EXACT, SCORE_PREFIX, SCORE_SUBSEQUENCE_MAX, SCORE_WORD_CONTAINS,
        SCORE_WORD_EXACT, SCORE_WORD_PREFIX, rank, score,
    };

    /// 打分器只认字符串——标签用中文，附加词用拼音，与面板里的真实形状一致。
    fn video() -> (&'static str, [&'static str; 2]) {
        ("视频统一解析", ["sptyjx", "video"])
    }

    #[test]
    fn ranking_is_score_descending_then_registration_order() {
        let mut items = vec![
            (800, 0, "先注册但分低"),
            (1000, 1, "分最高"),
            (800, 2, "后注册且同分"),
        ];
        rank(&mut items);
        assert_eq!(
            items.iter().map(|item| item.2).collect::<Vec<_>>(),
            vec!["分最高", "先注册但分低", "后注册且同分"]
        );
    }

    #[test]
    fn empty_query_matches_everything_with_zero() {
        let (label, words) = video();
        assert_eq!(score(label, &words, ""), Some(0));
        assert_eq!(score(label, &words, "   "), Some(0));
    }

    #[test]
    fn label_exact_beats_prefix_beats_contains() {
        let (_, words) = video();
        assert_eq!(
            score("视频统一解析", &words, "视频统一解析"),
            Some(SCORE_EXACT)
        );
        assert_eq!(score("视频统一解析", &words, "视频"), Some(SCORE_PREFIX));
        assert_eq!(score("视频统一解析", &words, "统一"), Some(SCORE_CONTAINS));
    }

    #[test]
    fn each_tier_lands_on_its_own_constant() {
        let words = ["sptyjx", "video"];
        assert_eq!(score("video", &words, "video"), Some(SCORE_EXACT));
        assert_eq!(score("video 转换", &words, "video"), Some(SCORE_PREFIX));
        assert_eq!(score("更新 视频", &words, "视频"), Some(SCORE_CONTAINS));
        assert_eq!(score("媒体处理", &words, "video"), Some(SCORE_WORD_EXACT));
        assert_eq!(score("媒体处理", &words, "vid"), Some(SCORE_WORD_PREFIX));
        assert_eq!(score("媒体处理", &words, "ideo"), Some(SCORE_WORD_CONTAINS));
    }

    #[test]
    fn a_gap_in_the_label_still_matches() {
        let (label, words) = video();
        let hit = score(label, &words, "视解");
        let inside = hit.expect("跳跃命中应算匹配");
        assert!(inside > 0, "跳跃命中必须为正分，0 是空查询专用");
        assert!(
            inside < SCORE_CONTAINS,
            "跳跃命中({inside}) 必须低于任何连续命中({SCORE_CONTAINS})"
        );
    }

    #[test]
    fn a_continuous_hit_always_beats_a_gap() {
        let (label, words) = video();
        let gap = score(label, &words, "视解").expect("跳跃命中");
        let continuous = score(label, &words, "统一").expect("包含命中");
        assert!(continuous > gap, "连续({continuous}) 必须高于跳跃({gap})");
    }

    #[test]
    fn a_compact_gap_beats_a_scattered_one() {
        let label = "abcdef";
        let near = score(label, &[], "abc").expect("前缀");
        let compact = score(label, &[], "acd").expect("紧凑跳跃");
        let scattered = score(label, &[], "aef").expect("零散跳跃");
        assert!(
            compact > scattered,
            "紧凑({compact}) 应高于零散({scattered})"
        );
        assert!(near > compact, "连续({near}) 应高于紧凑跳跃({compact})");
    }

    #[test]
    fn pinyin_words_never_jump_match() {
        let (label, words) = video();
        // `sy` 在拼音串里不是连续片段；若允许附加词参与子序列，它会命中一切长拼音。
        assert_eq!(score(label, &words, "sy"), None);
        assert_eq!(score(label, &words, "vtj"), None);
    }

    #[test]
    fn unrelated_query_does_not_match() {
        let (label, words) = video();
        assert_eq!(score(label, &words, "zzz-不存在"), None);
    }

    #[test]
    fn a_longer_query_than_the_label_never_matches() {
        assert_eq!(score("短", &[], "非常长的查询串"), None);
    }

    #[test]
    fn a_jump_hit_is_capped_below_every_continuous_tier() {
        // 与标签全等的查询拿到顶档，作为对照。
        assert_eq!(score("ab", &[], "ab"), Some(SCORE_EXACT));
        // 构造一个极有利的跳跃（全部靠近起点），仍必须被压在上界内。
        let jump = score("a-x-b", &[], "ab").expect("跳跃");
        assert!(jump <= SCORE_SUBSEQUENCE_MAX);
        assert!(jump < SCORE_WORD_CONTAINS);
    }
}
