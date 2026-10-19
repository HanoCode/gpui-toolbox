//! 文件搜索：**按名找 / 按内容找，同一份清单**。
//!
//! 参考仓有两套实现：`file-search`（内容搜索，逐行扫）与 `fast-file-search`
//! （常驻内存索引 + 文件监听守护，1,582 行）。设计稿已把后者裁成「限定目录 +
//! 深度上限」的遍历——插件进程一次调用一次进程，索引与监听器都没有「下一次运行」
//! 可复用，留着只是把启动时间从 30 ms 拖到 3 s。
//!
//! 因此这里只保留**匹配口径**，并把它做成两种模式共用一套遍历：
//!
//! - **文件名**：模式命中文件名或相对路径，每个文件一条命中（`line = 0`）；
//! - **内容**：逐行匹配，命中带行号、列号与该行片段；二进制与超大文件跳过并计数。
//!
//! 上限是**三重的**：全局命中数、每文件命中数、单文件字节数。三者都会在
//! [`SearchOutcome`] 里如实报出来——搜索工具最怕的是「看起来搜完了其实没有」。

use std::path::Path;

use regex::RegexBuilder;

use crate::error::{ErrorKind, FilesError, FilesResult};
use crate::walk::{WalkOptions, slash, walk};

/// 搜索模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// 按文件名 / 相对路径。
    Filename,
    /// 按文件内容。
    Content,
}

/// 搜索参数。
#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub mode: SearchMode,
    /// 关键词或正则。
    pub pattern: String,
    /// 把 `pattern` 当正则（否则按字面量，元字符自动转义）。
    pub is_regex: bool,
    /// 区分大小写。
    pub case_sensitive: bool,
    /// 全词匹配：给模式套一层 `\b(?:…)\b`（参考仓 `file_content_search` 同口径）。
    pub whole_word: bool,
    /// 全局最多返回多少条命中。
    pub max_results: usize,
    /// 每个文件最多报几处（计数仍然是全量）。
    pub max_hits_per_file: usize,
    /// 内容搜索跳过超过这个字节数的文件。
    pub max_file_bytes: u64,
    /// 遍历条件（深度、隐藏项、包含 / 排除）。
    pub walk: WalkOptions,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            mode: SearchMode::Content,
            pattern: String::new(),
            is_regex: false,
            case_sensitive: false,
            whole_word: false,
            max_results: 500,
            max_hits_per_file: 5,
            max_file_bytes: 2 * 1024 * 1024,
            walk: WalkOptions::default(),
        }
    }
}

/// 一条命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// 相对搜索根目录的路径（`/` 分隔）。
    pub rel: String,
    /// 行号（1 起；文件名模式为 0）。
    pub line: usize,
    /// 列号（按字符计，1 起；文件名模式为 0）。
    pub column: usize,
    /// 该行内容（已 trim 并按需截断）。
    pub text: String,
    /// 该文件里的命中总数（文件名模式恒为 1）。
    pub file_hits: usize,
}

/// 搜索结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    /// 遍历到的文件数（含被跳过的）。
    pub files_seen: usize,
    /// 因二进制 / 超大 / 非 UTF-8 而跳过的文件数。
    pub files_skipped: usize,
    /// 因全局上限提前收工。
    pub truncated: bool,
}

/// 单个命中片段的显示宽度上限（字符数）。
const SNIPPET_CHARS: usize = 200;

fn build_regex(
    pattern: &str,
    is_regex: bool,
    case_sensitive: bool,
    whole_word: bool,
) -> FilesResult<regex::Regex> {
    let mut source = if is_regex {
        pattern.to_string()
    } else {
        regex::escape(pattern)
    };
    // 全词匹配 = 套一层词边界（参考仓 `file_content_search` 也是这么拼的）。
    // 空模式不套：`\b(?:)\b` 是零宽断言，会在每个词边界「命中」。
    if whole_word && !source.is_empty() {
        source = format!(r"\b(?:{source})\b");
    }
    RegexBuilder::new(&source)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|error| FilesError::with_detail(ErrorKind::BadPattern, error.to_string()))
}

/// 是不是二进制：前 8 KiB 里出现 NUL 就当二进制。
///
/// 比「按扩展名白名单」可靠得多——`.log` / `.txt` 里塞二进制的情况很常见，
/// 而套接字、图片、编译产物都能被这一条挡住。
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8 * 1024).any(|byte| *byte == 0)
}

/// 把命中行裁到 [`SNIPPET_CHARS`] 以内，尽量保留命中点附近的上下文。
fn snippet(line: &str, start: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= SNIPPET_CHARS {
        return line.trim().to_string();
    }
    // 命中点按字符下标定位：`start` 是字节偏移，先换算成字符数。
    let hit_char = line[..start].chars().count();
    let half = SNIPPET_CHARS / 2;
    let from = hit_char.saturating_sub(half);
    let to = (from + SNIPPET_CHARS).min(chars.len());
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.extend(&chars[from..to]);
    if to < chars.len() {
        out.push('…');
    }
    out.trim().to_string()
}

/// 搜索入口。
pub fn search(root: &Path, options: &SearchOptions) -> FilesResult<SearchOutcome> {
    let pattern = options.pattern.trim();
    if pattern.is_empty() {
        return Err(FilesError::new(ErrorKind::EmptyInput));
    }
    let regex = build_regex(
        pattern,
        options.is_regex,
        options.case_sensitive,
        options.whole_word,
    )?;
    let mut outcome = SearchOutcome::default();

    let walk_outcome = walk(root, &options.walk, |entry| {
        if entry.is_dir {
            return Some(true);
        }
        outcome.files_seen += 1;
        let rel = entry.rel_slash();

        if options.mode == SearchMode::Filename {
            if regex.is_match(&entry.name()) || regex.is_match(&rel) {
                outcome.hits.push(SearchHit {
                    rel,
                    line: 0,
                    column: 0,
                    text: entry.name(),
                    file_hits: 1,
                });
            }
            return Some(outcome.hits.len() < options.max_results);
        }

        if entry.size > options.max_file_bytes {
            outcome.files_skipped += 1;
            return Some(true);
        }
        let bytes = match std::fs::read(&entry.path) {
            Ok(bytes) => bytes,
            Err(_) => {
                outcome.files_skipped += 1;
                return Some(true);
            }
        };
        if looks_binary(&bytes) {
            outcome.files_skipped += 1;
            return Some(true);
        }
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                outcome.files_skipped += 1;
                return Some(true);
            }
        };

        let mut file_hits = 0usize;
        for (index, line) in text.lines().enumerate() {
            for found in regex.find_iter(line) {
                file_hits += 1;
                if file_hits > options.max_hits_per_file {
                    continue;
                }
                outcome.hits.push(SearchHit {
                    rel: rel.clone(),
                    line: index + 1,
                    column: line[..found.start()].chars().count() + 1,
                    text: snippet(line, found.start()),
                    file_hits,
                });
            }
        }
        // `file_hits` 是全量计数，回填到该文件的每一条命中上（报告里显示「共 N 处」）。
        if file_hits > 0 {
            let start = outcome.hits.len() - file_hits.min(options.max_hits_per_file);
            for hit in &mut outcome.hits[start..] {
                hit.file_hits = file_hits;
            }
        }
        Some(outcome.hits.len() < options.max_results)
    })?;

    outcome.truncated = walk_outcome.truncated || outcome.hits.len() >= options.max_results;
    Ok(outcome)
}

/// 把结果渲成纯文本报告（`rel:line:col  text`，与 grep 的输出对齐，便于二次处理）。
///
/// 头部三行是「搜了什么 / 扫了多少 / 有没有截断」——缺任何一条，用户都会误判
/// 「没搜到」与「没搜完」为同一件事。
pub fn render_report(root: &Path, options: &SearchOptions, outcome: &SearchOutcome) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n", slash(root)));
    out.push_str(&format!(
        "pattern={:?} mode={:?} files={} skipped={} hits={}\n",
        options.pattern,
        match options.mode {
            SearchMode::Filename => "filename",
            SearchMode::Content => "content",
        },
        outcome.files_seen,
        outcome.files_skipped,
        outcome.hits.len()
    ));
    if outcome.truncated {
        out.push_str("truncated=true\n");
    }
    out.push('\n');
    for hit in &outcome.hits {
        if hit.line == 0 {
            out.push_str(&format!("{}\n", hit.rel));
        } else {
            out.push_str(&format!(
                "{}:{}:{}  {}\n",
                hit.rel, hit.line, hit.column, hit.text
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{temp_dir, write, write_bytes};

    fn fixture(name: &str) -> std::path::PathBuf {
        let root = temp_dir(name);
        write(&root, "src/main.rs", "fn main() {\n    let token = 1;\n}\n");
        write(&root, "src/lib.rs", "pub fn helper() {}\n");
        write(&root, "README.md", "# token 说明\n");
        write(&root, "server.log", "token\ntoken\ntoken\n");
        write_bytes(&root, "blob.bin", &[0u8, 1, 2, 3, 0]);
        root
    }

    #[test]
    fn content_search_reports_line_and_column() {
        let root = fixture("search-content");
        let options = SearchOptions {
            pattern: "token".to_string(),
            ..SearchOptions::default()
        };
        let outcome = search(&root, &options).expect("search");
        // README 1 + main 1 + log 3 = 5；blob 被当二进制跳过。
        assert_eq!(outcome.hits.len(), 5, "{:?}", outcome.hits);
        assert_eq!(outcome.files_skipped, 1);

        let main = outcome
            .hits
            .iter()
            .find(|hit| hit.rel == "src/main.rs")
            .expect("main.rs hit");
        assert_eq!(main.line, 2);
        assert_eq!(
            main.column, 9,
            "`    let token` 里 token 在第 9 列（按字符计）"
        );
        assert_eq!(main.text, "let token = 1;");
        assert_eq!(main.file_hits, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn whole_word_only_matches_standalone_occurrences() {
        let root = temp_dir("search-whole-word");
        write(&root, "a.txt", "token\ntokens\nmytoken\nsub token x\n");
        let loose = SearchOptions {
            pattern: "token".to_string(),
            ..SearchOptions::default()
        };
        assert_eq!(search(&root, &loose).expect("loose").hits.len(), 4);
        // `\btoken\b` 不该命中 `tokens` / `mytoken`：词边界要求两侧都不是词字符。
        let strict = SearchOptions {
            whole_word: true,
            ..loose
        };
        assert_eq!(search(&root, &strict).expect("strict").hits.len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn per_file_limit_caps_rows_but_keeps_the_count() {
        let root = fixture("search-per-file");
        let options = SearchOptions {
            pattern: "token".to_string(),
            max_hits_per_file: 2,
            ..SearchOptions::default()
        };
        let outcome = search(&root, &options).expect("search");
        let log_hits: Vec<&SearchHit> = outcome
            .hits
            .iter()
            .filter(|hit| hit.rel == "server.log")
            .collect();
        assert_eq!(log_hits.len(), 2, "最多报 2 行");
        assert_eq!(log_hits[0].file_hits, 3, "但计数要如实说 3 处");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn filename_mode_matches_name_and_ignores_case_when_asked() {
        let root = fixture("search-name");
        let options = SearchOptions {
            mode: SearchMode::Filename,
            pattern: "MAIN".to_string(),
            ..SearchOptions::default()
        };
        let outcome = search(&root, &options).expect("search");
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].rel, "src/main.rs");
        assert_eq!(outcome.hits[0].line, 0);

        let strict = SearchOptions {
            mode: SearchMode::Filename,
            pattern: "MAIN".to_string(),
            case_sensitive: true,
            ..SearchOptions::default()
        };
        assert!(search(&root, &strict).expect("search").hits.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn regex_mode_and_literal_mode_differ_on_metacharacters() {
        let root = temp_dir("search-regex");
        write(&root, "a.txt", "a.b\naxb\n");
        let literal = SearchOptions {
            pattern: "a.b".to_string(),
            ..SearchOptions::default()
        };
        assert_eq!(search(&root, &literal).expect("search").hits.len(), 1);

        let as_regex = SearchOptions {
            pattern: "a.b".to_string(),
            is_regex: true,
            ..SearchOptions::default()
        };
        assert_eq!(search(&root, &as_regex).expect("search").hits.len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_pattern_and_bad_regex_are_distinguishable() {
        let root = fixture("search-errors");
        let empty = SearchOptions::default();
        assert_eq!(
            search(&root, &empty).unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
        let bad = SearchOptions {
            pattern: "a(".to_string(),
            is_regex: true,
            ..SearchOptions::default()
        };
        assert_eq!(
            search(&root, &bad).unwrap_err().kind(),
            ErrorKind::BadPattern
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn global_limit_marks_truncated() {
        let root = fixture("search-limit");
        let options = SearchOptions {
            pattern: "token".to_string(),
            max_results: 2,
            ..SearchOptions::default()
        };
        let outcome = search(&root, &options).expect("search");
        assert_eq!(outcome.hits.len(), 2);
        assert!(outcome.truncated);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn report_lists_hits_and_flags() {
        let root = fixture("search-report");
        let options = SearchOptions {
            pattern: "token".to_string(),
            max_results: 2,
            ..SearchOptions::default()
        };
        let outcome = search(&root, &options).expect("search");
        let report = render_report(&root, &options, &outcome);
        // 命中数一到上限，遍历就停了：`files` 因此是 4 而不是 5（第 5 个没扫到）。
        assert!(report.contains("files=4"), "{report}");
        assert!(report.contains("skipped=1"), "{report}");
        assert!(report.contains("truncated=true"), "{report}");
        assert!(report.contains("src/main.rs:2:9"), "{report}");
        let _ = std::fs::remove_dir_all(root);
    }
}
