//! 两段文本的逐行差异（参考仓 `file_ops.rs` 的 `compute_diff`，**逐字复刻**）。
//!
//! 与源仓的差别只有两处：
//!
//! 1. 去掉 `#[tauri::command]`（本仓走 process 轨的 stdio 协议，没有命令注册）；
//! 2. 结构体字段改为 `pub`（源仓靠 serde 序列化直接出网，本仓还要在工具层
//!    映射成宿主的 `DiffDoc`，字段不可见就没法映射）。
//!
//! 算法一行没动：`TextDiff::from_lines` 找行块 → `Replace` 段内再按**字符**做一次
//! 内联差异，把「改了哪几个字」标出来（`old_changes` / `new_changes`）。
use serde::Serialize;
use similar::{ChangeTag, TextDiff};

// ── Diff Engine ──

#[derive(Serialize)]
pub struct ChangeSpan {
    pub kind: String,
    pub text: String,
}

#[derive(Serialize)]
pub struct DiffRow {
    pub kind: String,
    pub old_ln: Option<u32>,
    pub new_ln: Option<u32>,
    pub old_text: String,
    pub new_text: String,
    pub old_changes: Vec<ChangeSpan>,
    pub new_changes: Vec<ChangeSpan>,
}

#[derive(Serialize)]
pub struct DiffStats {
    pub added: u32,
    pub removed: u32,
    pub unchanged: u32,
    pub modified: u32,
}

#[derive(Serialize)]
pub struct DiffResult {
    pub rows: Vec<DiffRow>,
    pub stats: DiffStats,
}

fn inline_diff(old: &str, new: &str) -> (Vec<ChangeSpan>, Vec<ChangeSpan>) {
    let diff = TextDiff::from_chars(old, new);
    let mut old_spans = Vec::new();
    let mut new_spans = Vec::new();
    for change in diff.iter_all_changes() {
        let text = change.value().to_string();
        match change.tag() {
            ChangeTag::Equal => {
                old_spans.push(ChangeSpan {
                    kind: "equal".into(),
                    text: text.clone(),
                });
                new_spans.push(ChangeSpan {
                    kind: "equal".into(),
                    text,
                });
            }
            ChangeTag::Delete => old_spans.push(ChangeSpan {
                kind: "remove".into(),
                text,
            }),
            ChangeTag::Insert => new_spans.push(ChangeSpan {
                kind: "add".into(),
                text,
            }),
        }
    }
    (old_spans, new_spans)
}

/// 逐行差异。
///
/// `ignore_case` 是参考仓 `TextDiff.vue` 的两个勾选框之一（`忽略大小写`，**默认开**）：
/// 那边在干比较前把每行 `toLowerCase()`。这里同样只影响**比对**——行文本照原文展示，
/// 行数不变，因此 ops 的下标与 `lines_a` / `lines_b` 天然对齐。
pub fn compute_diff(
    text_a: String,
    text_b: String,
    ignore_whitespace: bool,
    ignore_case: bool,
) -> Result<DiffResult, String> {
    let a = if ignore_whitespace {
        text_a
            .lines()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text_a.clone()
    };
    let b = if ignore_whitespace {
        text_b
            .lines()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text_b.clone()
    };

    let lines_a: Vec<&str> = a.split('\n').collect();
    let lines_b: Vec<&str> = b.split('\n').collect();
    // 比对用的副本（`to_lowercase` 不动换行 ⇒ 行数与原文一致）。
    let (a_cmp, b_cmp) = if ignore_case {
        (a.to_lowercase(), b.to_lowercase())
    } else {
        (a.clone(), b.clone())
    };

    let diff = TextDiff::from_lines(&a_cmp, &b_cmp);
    let mut rows = Vec::new();
    let mut added = 0u32;
    let mut removed = 0u32;
    let mut unchanged = 0u32;

    for op in diff.ops() {
        let old_range = op.old_range();
        let new_range = op.new_range();

        match op {
            similar::DiffOp::Equal { .. } => {
                for (i, j) in old_range.zip(new_range) {
                    unchanged += 1;
                    // 两侧各取**自己**那一行：忽略大小写时它们字面不同（`Name` / `name`），
                    // 都拿 A 的行去填会把右侧的原文改掉。关掉大小写折叠时两者本就相同。
                    rows.push(DiffRow {
                        kind: "equal".into(),
                        old_ln: Some((i + 1) as u32),
                        new_ln: Some((j + 1) as u32),
                        old_text: lines_a[i].to_string(),
                        new_text: lines_b[j].to_string(),
                        old_changes: vec![],
                        new_changes: vec![],
                    });
                }
            }
            similar::DiffOp::Delete { .. } => {
                for i in old_range {
                    let line = lines_a[i].to_string();
                    removed += 1;
                    rows.push(DiffRow {
                        kind: "delete".into(),
                        old_ln: Some((i + 1) as u32),
                        new_ln: None,
                        old_text: line,
                        new_text: String::new(),
                        old_changes: vec![],
                        new_changes: vec![],
                    });
                }
            }
            similar::DiffOp::Insert { .. } => {
                for j in new_range {
                    let line = lines_b[j].to_string();
                    added += 1;
                    rows.push(DiffRow {
                        kind: "insert".into(),
                        old_ln: None,
                        new_ln: Some((j + 1) as u32),
                        old_text: String::new(),
                        new_text: line,
                        old_changes: vec![],
                        new_changes: vec![],
                    });
                }
            }
            similar::DiffOp::Replace { .. } => {
                let old_slice: Vec<&str> = old_range.clone().map(|i| lines_a[i]).collect();
                let new_slice: Vec<&str> = new_range.clone().map(|j| lines_b[j]).collect();
                let max = old_slice.len().max(new_slice.len());
                for idx in 0..max {
                    let old_line = old_slice.get(idx).copied().unwrap_or("");
                    let new_line = new_slice.get(idx).copied().unwrap_or("");
                    let (oc, nc) = inline_diff(old_line, new_line);
                    if idx < old_slice.len() {
                        removed += 1;
                    }
                    if idx < new_slice.len() {
                        added += 1;
                    }
                    rows.push(DiffRow {
                        kind: "change".into(),
                        old_ln: old_slice
                            .get(idx)
                            .map(|_| (old_range.start + idx + 1) as u32),
                        new_ln: new_slice
                            .get(idx)
                            .map(|_| (new_range.start + idx + 1) as u32),
                        old_text: old_line.to_string(),
                        new_text: new_line.to_string(),
                        old_changes: oc,
                        new_changes: nc,
                    });
                }
            }
        }
    }

    Ok(DiffResult {
        rows,
        stats: DiffStats {
            added,
            removed,
            unchanged,
            modified: 0,
        },
    })
}

/// 同一份比对的 unified diff 文本（`--- / +++ / @@ …`）。
///
/// 用途：插件把结构化差异发给宿主**渲染**，同时要把一份纯文本放进 `output`——
/// 复制按钮复制的是 `output`，用户拿走的应该是「能贴进邮件 / 缺陷单」的东西，
/// 而不是界面上的行号。所以这里用 `similar` 自带的 unified 打印器再算一遍：
/// 与逐行差异同一份算法，不会出现「画面与复制的行号对不上」。
pub fn unified_diff(
    text_a: &str,
    text_b: &str,
    title_a: &str,
    title_b: &str,
    ignore_whitespace: bool,
    ignore_case: bool,
) -> String {
    let norm = |text: &str| -> String {
        let text = if ignore_whitespace {
            trim_ends(text)
        } else {
            text.to_owned()
        };
        if ignore_case {
            text.to_lowercase()
        } else {
            text
        }
    };
    let (a, b) = (norm(text_a), norm(text_b));
    TextDiff::from_lines(&a, &b)
        .unified_diff()
        .header(title_a, title_b)
        .to_string()
}

/// 与 `compute_diff` 同一口径的行尾去空白（源码里是 `trim_end` 后重新拼行）。
fn trim_ends(text: &str) -> String {
    text.lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐行差异的行种类与统计：这是差异渲染的唯一数据来源，画错就是「多了一行/少了一行」。
    #[test]
    fn row_kinds_and_stats_match_the_text() {
        let result =
            compute_diff("a\nb\nc\n".into(), "a\nB\nc\n".into(), false, false).expect("diff");
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0].kind, "equal");
        assert_eq!(result.rows[1].kind, "change");
        assert_eq!(result.rows[2].kind, "equal");
        assert_eq!(result.stats.unchanged, 2);
        assert_eq!(result.stats.added, 1);
        assert_eq!(result.stats.removed, 1);
    }

    #[test]
    fn inserted_and_deleted_lines_are_not_paired() {
        let result = compute_diff("a\n".into(), "a\nb\n".into(), false, false).expect("diff");
        assert_eq!(result.rows.last().expect("row").kind, "insert");
        assert_eq!(result.stats.added, 1);
        assert_eq!(result.stats.removed, 0);
    }

    /// 忽略行尾空白：两侧只差行尾空格时应当「完全一致」。
    #[test]
    fn ignore_whitespace_treats_trailing_spaces_as_equal() {
        let strict = compute_diff("a  \n".into(), "a\n".into(), false, false).expect("diff");
        assert_eq!(strict.stats.unchanged, 0);
        let loose = compute_diff("a  \n".into(), "a\n".into(), true, false).expect("diff");
        assert_eq!(loose.stats.unchanged, 1);
        assert_eq!(loose.stats.added, 0);
    }

    /// 忽略大小写（参考仓默认开）：只有大小写不同的一行算「一致」，
    /// 但**展示的仍是原文**——大小写折叠只发生在比对那一侧。
    #[test]
    fn ignore_case_compares_folded_but_displays_the_original() {
        let strict =
            compute_diff("Name: Duck\n".into(), "name: duck\n".into(), false, false).expect("diff");
        assert_eq!(strict.stats.unchanged, 0);
        assert_eq!(strict.rows[0].kind, "change");

        let loose =
            compute_diff("Name: Duck\n".into(), "name: duck\n".into(), false, true).expect("diff");
        assert_eq!(loose.stats.unchanged, 1);
        assert_eq!(loose.rows[0].kind, "equal");
        // 两侧都保留原始大小写。
        assert_eq!(loose.rows[0].old_text, "Name: Duck");
        assert_eq!(loose.rows[0].new_text, "name: duck");
    }

    /// 行内片段：把改了的那几个字标出来（渲染器据此上高亮底色）。
    #[test]
    fn changed_rows_carry_inline_spans() {
        let result = compute_diff("version=1.0\n".into(), "version=2.0\n".into(), false, false)
            .expect("diff");
        let row = &result.rows[0];
        assert_eq!(row.kind, "change");
        let removed: String = row
            .old_changes
            .iter()
            .filter(|span| span.kind == "remove")
            .map(|span| span.text.as_str())
            .collect();
        let added: String = row
            .new_changes
            .iter()
            .filter(|span| span.kind == "add")
            .map(|span| span.text.as_str())
            .collect();
        assert_eq!((removed.as_str(), added.as_str()), ("1", "2"));
    }

    /// 复制走的那份 unified diff 必须与逐行结果同源（同一个 hunk、同样的 +/-）。
    #[test]
    fn unified_diff_text_matches_the_rows() {
        let text = unified_diff("a\nb\n", "a\nc\n", "A", "B", false, false);
        assert!(text.starts_with("--- A\n+++ B\n"), "{text}");
        assert!(text.contains("-b"), "{text}");
        assert!(text.contains("+c"), "{text}");
        assert!(text.contains(" a"), "{text}");
    }
}
