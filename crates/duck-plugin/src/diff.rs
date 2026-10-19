//! 结构化差异正文：`[io.output] kind = "diff"` 的载荷与其文本回退解析。
//!
//! 为什么差异不能只当一段文本发：宿主要把「哪一行是删、哪一行是增、改动的**哪几个
//! 字**变了」画成 A/B 双栏（参考项目 `DiffView.vue` 就是这样做的）。纯文本只能靠
//! 前缀猜，猜错就是画错——所以进程轨多一条通道（`{"type":"body","chunk":"…"}`，
//! 见 `duck-plugin-process` 的协议），把这份结构作为 JSON 分片送过来。
//!
//! 两条兼容路径：
//!
//! 1. **结构化**（首选）：插件发 body，字段就是本模块的 [`DiffDoc`]；
//! 2. **unified diff**（回退）：插件只发 `output` 文本时，宿主用
//!    [`DiffDoc::from_unified`] 解析 `--- / +++ / @@` 那种标准格式。JS 轨插件没有
//!    body 通道，所以这条回退**是它的唯一路径**，必须保留。
//!
//! `output` 文本始终保留一份（复制按钮复制的是它）：结构化只影响**渲染**，
//! 不影响「用户能把结果带走」。

use serde::{Deserialize, Serialize};

/// 一侧行内的片段。
///
/// `hit = true` 表示这段是该行的改动部分（渲染时上高亮底色）。整行未变、或改动是
/// 整行替换时，一个 [`DiffSpan`] 就够；参考仓的 `inline_diff` 会按字符切开，
/// 只把真正变的字标成 `hit`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSpan {
    /// 是否属于改动部分。
    pub hit: bool,
    /// 片段文本。
    pub text: String,
}

impl DiffSpan {
    /// 未改动的片段。
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            hit: false,
            text: text.into(),
        }
    }

    /// 改动了的片段。
    #[must_use]
    pub fn changed(text: impl Into<String>) -> Self {
        Self {
            hit: true,
            text: text.into(),
        }
    }
}

/// 一行的种类。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffRowKind {
    /// 两侧相同。
    #[default]
    Equal,
    /// 只在左侧（删除）。
    Delete,
    /// 只在右侧（新增）。
    Insert,
    /// 两侧都有、内容不同（行内 `spans` 标出改了哪几个字）。
    Change,
    /// 不连续：unified diff 的 hunk 之间、或被折叠掉的一大段。两侧都空。
    Gap,
}

impl DiffRowKind {
    /// 这一行在左侧有没有内容。
    #[must_use]
    pub fn has_old(self) -> bool {
        matches!(self, Self::Equal | Self::Delete | Self::Change)
    }

    /// 这一行在右侧有没有内容。
    #[must_use]
    pub fn has_new(self) -> bool {
        matches!(self, Self::Equal | Self::Insert | Self::Change)
    }
}

/// 一行差异。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRow {
    /// 行种类。
    #[serde(default = "default_row_kind")]
    pub kind: DiffRowKind,
    /// 左侧行号（1 起；该行不属于左侧时为 `None`）。
    #[serde(default)]
    pub old_ln: Option<u32>,
    /// 右侧行号。
    #[serde(default)]
    pub new_ln: Option<u32>,
    /// 左侧整行文本。
    #[serde(default)]
    pub old_text: String,
    /// 右侧整行文本。
    #[serde(default)]
    pub new_text: String,
    /// 左侧行内片段（空 = 整行一个未改动的片段）。
    #[serde(default)]
    pub old_spans: Vec<DiffSpan>,
    /// 右侧行内片段。
    #[serde(default)]
    pub new_spans: Vec<DiffSpan>,
}

fn default_row_kind() -> DiffRowKind {
    DiffRowKind::Equal
}

impl DiffRow {
    /// 构造一条「两侧相同」的行。
    #[must_use]
    pub fn equal(old_ln: u32, new_ln: u32, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            kind: DiffRowKind::Equal,
            old_ln: Some(old_ln),
            new_ln: Some(new_ln),
            old_text: text.clone(),
            new_text: text,
            ..Self::default()
        }
    }

    /// 构造一条「只在左侧」的行。
    #[must_use]
    pub fn delete(old_ln: u32, text: impl Into<String>) -> Self {
        Self {
            kind: DiffRowKind::Delete,
            old_ln: Some(old_ln),
            old_text: text.into(),
            ..Self::default()
        }
    }

    /// 构造一条「只在右侧」的行。
    #[must_use]
    pub fn insert(new_ln: u32, text: impl Into<String>) -> Self {
        Self {
            kind: DiffRowKind::Insert,
            new_ln: Some(new_ln),
            new_text: text.into(),
            ..Self::default()
        }
    }

    /// 一条不连续标记。
    #[must_use]
    pub fn gap() -> Self {
        Self {
            kind: DiffRowKind::Gap,
            ..Self::default()
        }
    }

    /// 左侧要画的片段：没给 `old_spans` 就退回整行文本。
    #[must_use]
    pub fn old_segments(&self) -> Vec<DiffSpan> {
        segments_or_whole(&self.old_spans, &self.old_text)
    }

    /// 右侧要画的片段。
    #[must_use]
    pub fn new_segments(&self) -> Vec<DiffSpan> {
        segments_or_whole(&self.new_spans, &self.new_text)
    }
}

fn segments_or_whole(spans: &[DiffSpan], text: &str) -> Vec<DiffSpan> {
    if spans.is_empty() {
        return vec![DiffSpan::plain(text)];
    }
    spans.to_vec()
}

/// 一份文件的差异。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    /// 左侧标题（文件名 / 「版本 A」；可空）。
    #[serde(default)]
    pub title_a: String,
    /// 右侧标题。
    #[serde(default)]
    pub title_b: String,
    /// 逐行差异。
    #[serde(default)]
    pub rows: Vec<DiffRow>,
}

/// 一次差异结果的全文（可含多个文件）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffDoc {
    /// 各文件的差异，按出现顺序。
    #[serde(default)]
    pub files: Vec<DiffFile>,
}

/// 差异计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStats {
    /// 新增行（`Insert` + 有右侧的 `Change`）。
    pub added: usize,
    /// 删除行（`Delete` + 有左侧的 `Change`）。
    pub removed: usize,
    /// 改动行（两侧都在）。
    pub modified: usize,
    /// 未变行。
    pub unchanged: usize,
}

impl DiffStats {
    /// 两份内容是否完全一样（没有任何增删改）。
    #[must_use]
    pub fn is_identical(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.modified == 0
    }
}

impl DiffDoc {
    /// 单文件形态：只有一份且没有标题时，渲染器不必画文件头。
    #[must_use]
    pub fn single_file(&self) -> bool {
        self.files.len() == 1
    }

    /// 全文是否空（没有任何文件或没有任何行）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|file| file.rows.is_empty())
    }

    /// 行数合计。
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.files.iter().map(|file| file.rows.len()).sum()
    }

    /// 统计：从行本身数出来，**不读插件自报的数字**。
    ///
    /// 自报统计与逐行内容一旦对不上（插件少发了几行、或把 `replace` 数成 1 条），
    /// 界面就会同时显示「+3」和三条新增行——两处各有各的道理，用户只会觉得哪里不对。
    /// 以行为准，两者永远一致。
    #[must_use]
    pub fn stats(&self) -> DiffStats {
        let mut stats = DiffStats::default();
        for row in self.files.iter().flat_map(|file| &file.rows) {
            match row.kind {
                DiffRowKind::Equal => stats.unchanged += 1,
                DiffRowKind::Delete => stats.removed += 1,
                DiffRowKind::Insert => stats.added += 1,
                DiffRowKind::Change => {
                    stats.modified += 1;
                    stats.removed += 1;
                    stats.added += 1;
                }
                DiffRowKind::Gap => {}
            }
        }
        stats
    }

    /// 把 unified diff 文本解析成结构化差异；**不是** unified diff 时返回 `None`。
    ///
    /// # 判据
    /// 必须出现 `@@` hunk 头，或成对的 `--- ` / `+++ ` 文件头。不能拿「有 `+` / `-`
    /// 开头的行」当判据：普通文本里以 `- ` 开头的列表项太常见，那样会把一段
    /// 普通输出误判成差异、画成一堆红色删除行。
    ///
    /// # 行号
    /// 行号来自 hunk 头（`@@ -12,3 +12,4 @@`）。两段 hunk 之间插一条
    /// [`DiffRowKind::Gap`]：中间那些没显示的行不是「未变」，不能让行号悄悄连上。
    #[must_use]
    pub fn from_unified(text: &str) -> Option<Self> {
        let mut doc = Self::default();
        let mut current: Option<DiffFile> = None;
        let mut old_no = 0u32;
        let mut new_no = 0u32;
        let mut seen_diff = false;
        let mut minus_title: Option<String> = None;
        let mut pending_hunk = false;

        for raw in text.lines() {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if let Some(rest) = line.strip_prefix("--- ") {
                minus_title = Some(clean_title(rest));
                continue;
            }
            if let Some(rest) = line.strip_prefix("+++ ") {
                seen_diff = true;
                let title_b = clean_title(rest);
                let title_a = minus_title.take().unwrap_or_default();
                // `if let … && …` 是 let-chain（Rust 1.88），MSRV 1.85 用不了，
                // 所以把「有内容」折进 `filter`，保持单层 `if let`。
                if let Some(file) = current.take().filter(|file| !file.rows.is_empty()) {
                    doc.files.push(file);
                }
                current = Some(DiffFile {
                    title_a,
                    title_b,
                    rows: Vec::new(),
                });
                old_no = 0;
                new_no = 0;
                continue;
            }
            if let Some(rest) = line.strip_prefix("@@") {
                seen_diff = true;
                if let Some((a, b)) = parse_hunk_header(rest) {
                    old_no = a;
                    new_no = b;
                }
                let file = current.get_or_insert_with(DiffFile::default);
                // 第一条 hunk 之前不插 Gap（文件头下面直接就是内容）。
                if !file.rows.is_empty() {
                    file.rows.push(DiffRow::gap());
                }
                pending_hunk = true;
                continue;
            }
            if !pending_hunk {
                // hunk 之外的行（`diff --git a/x b/x`、`index 1234..5678` 之类）：
                // 只有 `diff --git` 能补一份文件标题，其余丢弃。
                if let Some(rest) = line.strip_prefix("diff --git ") {
                    if current.as_ref().is_none_or(|file| !file.rows.is_empty()) {
                        if let Some(file) = current.take() {
                            doc.files.push(file);
                        }
                        current = Some(DiffFile::default());
                    }
                    if let Some((a, b)) = split_git_header(rest) {
                        let file = current.get_or_insert_with(DiffFile::default);
                        if file.title_a.is_empty() {
                            file.title_a = a;
                            file.title_b = b;
                        }
                    }
                }
                continue;
            }
            let file = current.get_or_insert_with(DiffFile::default);
            if let Some(rest) = line.strip_prefix('\\') {
                // `\ No newline at end of file`：不是内容行。
                let _ = rest;
                continue;
            }
            match line.as_bytes().first() {
                Some(b'+') => {
                    let text = line[1..].to_owned();
                    file.rows.push(DiffRow::insert(new_no.max(1), text));
                    new_no += 1;
                }
                Some(b'-') => {
                    let text = line[1..].to_owned();
                    file.rows.push(DiffRow::delete(old_no.max(1), text));
                    old_no += 1;
                }
                Some(b' ') => {
                    let text = line[1..].to_owned();
                    file.rows
                        .push(DiffRow::equal(old_no.max(1), new_no.max(1), text));
                    old_no += 1;
                    new_no += 1;
                }
                // 空行（有的工具会把「上下文空行」写成完全空的一行）
                None => {
                    file.rows
                        .push(DiffRow::equal(old_no.max(1), new_no.max(1), String::new()));
                    old_no += 1;
                    new_no += 1;
                }
                _ => {}
            }
        }

        if let Some(file) = current.take().filter(|file| !file.rows.is_empty()) {
            doc.files.push(file);
        }
        if !seen_diff || doc.files.is_empty() {
            return None;
        }
        Some(doc)
    }
}

/// 去掉 `a/` / `b/` 前缀（git 惯用）与 `/dev/null` 之外的引号。
fn clean_title(raw: &str) -> String {
    let title = raw.trim();
    // `"a/x y.txt"` 这种带引号（路径含空格）的写法。
    let title = title
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(title);
    let title = title
        .strip_prefix("a/")
        .or_else(|| title.strip_prefix("b/"))
        .unwrap_or(title);
    if title == "/dev/null" {
        String::new()
    } else {
        title.to_owned()
    }
}

/// `diff --git a/x b/y` → `(x, y)`。
fn split_git_header(rest: &str) -> Option<(String, String)> {
    let rest = rest.trim();
    let (a, b) = rest.rsplit_once(' ')?;
    Some((clean_title(a), clean_title(b)))
}

/// `@@ -12,3 +12,4 @@ …` → `(12, 12)`（两侧起始行号）。
fn parse_hunk_header(rest: &str) -> Option<(u32, u32)> {
    let mut parts = rest.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    Some((head_number(old), head_number(new)))
}

/// `12,3` → `12`（只取起始行号）。
fn head_number(spec: &str) -> u32 {
    spec.split(',').next().unwrap_or("1").parse().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_becomes_rows_with_line_numbers() {
        let text = "\
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 same
-old line
+new line
 tail
";
        let doc = DiffDoc::from_unified(text).expect("is a unified diff");
        assert!(doc.single_file());
        let file = &doc.files[0];
        assert_eq!(file.title_a, "a.txt");
        assert_eq!(file.title_b, "a.txt");
        assert_eq!(
            file.rows.iter().map(|row| row.kind).collect::<Vec<_>>(),
            vec![
                DiffRowKind::Equal,
                DiffRowKind::Delete,
                DiffRowKind::Insert,
                DiffRowKind::Equal
            ]
        );
        // 删除行只有左侧行号，新增行只有右侧行号。
        assert_eq!(file.rows[1].old_ln, Some(2));
        assert_eq!(file.rows[1].new_ln, None);
        assert_eq!(file.rows[2].old_ln, None);
        assert_eq!(file.rows[2].new_ln, Some(2));
        assert_eq!(file.rows[3].old_ln, Some(3));
        assert_eq!(file.rows[3].new_ln, Some(3));
        let stats = doc.stats();
        assert_eq!((stats.added, stats.removed, stats.unchanged), (1, 1, 2));
    }

    #[test]
    fn two_hunks_get_a_gap_row_so_line_numbers_do_not_lie() {
        let text = "\
@@ -1,1 +1,1 @@
 a
@@ -40,1 +40,1 @@
-b
+B
";
        let doc = DiffDoc::from_unified(text).expect("is a unified diff");
        let rows = &doc.files[0].rows;
        assert_eq!(rows[1].kind, DiffRowKind::Gap);
        // 第二条 hunk 的行号从头里取，不接着上一段数。
        assert_eq!(rows[2].old_ln, Some(40));
        assert_eq!(rows[3].new_ln, Some(40));
    }

    #[test]
    fn plain_prose_with_dash_bullets_is_not_a_diff() {
        // 列表项以 `- ` 开头：绝不能把这些行画成删除。
        let text = "- 第一项\n- 第二项\n+ 不是新增，是正文\n";
        assert!(DiffDoc::from_unified(text).is_none());
    }

    #[test]
    fn git_header_supplies_titles_when_there_are_no_dash_headers() {
        let text = "\
diff --git a/src/x.rs b/src/x.rs
index 1234567..89abcde 100644
@@ -1,1 +1,1 @@
-old
+new
";
        let doc = DiffDoc::from_unified(text).expect("is a unified diff");
        assert_eq!(doc.files[0].title_a, "src/x.rs");
        assert_eq!(doc.files[0].title_b, "src/x.rs");
    }

    #[test]
    fn no_newline_marker_is_skipped() {
        let text = "\
--- a/x
+++ b/x
@@ -1 +1 @@
-a
\\ No newline at end of file
+b
\\ No newline at end of file
";
        let doc = DiffDoc::from_unified(text).expect("is a unified diff");
        assert_eq!(doc.files[0].rows.len(), 2);
    }

    #[test]
    fn stats_count_a_change_row_on_both_sides() {
        let doc = DiffDoc {
            files: vec![DiffFile {
                rows: vec![
                    DiffRow {
                        kind: DiffRowKind::Change,
                        old_ln: Some(1),
                        new_ln: Some(1),
                        ..DiffRow::default()
                    },
                    DiffRow::insert(2, "x"),
                ],
                ..DiffFile::default()
            }],
        };
        let stats = doc.stats();
        assert_eq!(stats.modified, 1);
        assert_eq!(stats.added, 2);
        assert_eq!(stats.removed, 1);
        assert!(!stats.is_identical());
    }

    #[test]
    fn empty_spans_fall_back_to_the_whole_line() {
        let row = DiffRow::insert(1, "hello");
        assert_eq!(row.new_segments(), vec![DiffSpan::plain("hello")]);
        assert!(row.old_segments()[0].text.is_empty());
    }
}
