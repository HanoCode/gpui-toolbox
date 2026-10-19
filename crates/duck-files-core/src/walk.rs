//! 目录遍历：**一套口径，三个工具共用**。
//!
//! 搜索、目录树、批量改写都要回答同一个问题——「这个目录下有哪些文件」——只是
//! 过滤条件不同。参考仓把这件事分散在三处（`file_ops::list_files_recursive`、
//! `fast_file_search` 的索引器、前端各自的拖拽列表），口径互不一致：一个跳过
//! 隐藏文件、另一个不跳；一个按深度截断、另一个不限。合成一份之后，三个工具的
//! 「同一目录为什么结果不同」这类问题从根上消失。
//!
//! 三条硬规则：
//!
//! 1. **`max_entries` 是硬上限**（缺省 20 000）：搜索一个误选的 `/` 不该把内存吃光，
//!    被截断时 [`WalkOutcome::truncated`] 会说清「还有更多」。
//! 2. **不跟随符号链接**（`follow_links = false`）：`~/Library` 里有大量指回自身的
//!    链接，跟进去就是无限深。
//! 3. **隐藏项按名前缀判断**（`.` 开头），与 `ls` 一致；`.git` 因此天然被跳过。

use std::path::{Path, PathBuf};

use crate::error::{ErrorKind, FilesError, FilesResult};

/// 一次遍历的过滤条件。
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// 最大深度（`0` = 不限；`1` = 只看直接子项）。
    pub max_depth: usize,
    /// 是否包含 `.` 开头的隐藏项。
    pub include_hidden: bool,
    /// 包含模式（空 = 全部）。命中**文件名**或**相对路径**任一条即算命中。
    pub include: Vec<String>,
    /// 排除模式（优先于 `include`）。
    pub exclude: Vec<String>,
    /// 条目数硬上限。
    pub max_entries: usize,
    /// 是否跟随符号链接。
    pub follow_links: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_depth: 0,
            include_hidden: false,
            include: Vec::new(),
            exclude: Vec::new(),
            max_entries: 20_000,
            follow_links: false,
        }
    }
}

/// 一个条目（目录也在内：目录树要它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
    /// 绝对 / 原始路径。
    pub path: PathBuf,
    /// 相对根目录的路径（渲染与报告都用它，避免把 `/Users/xxx` 打给用户）。
    pub rel: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

impl WalkEntry {
    /// 文件名（末段）。
    pub fn name(&self) -> String {
        self.rel
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// 相对路径，统一用 `/` 分隔（报告里跨平台一致）。
    pub fn rel_slash(&self) -> String {
        slash(&self.rel)
    }
}

/// 遍历汇总。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WalkOutcome {
    /// 已产出的条目数。
    pub entries: usize,
    /// 命中上限被截断。
    pub truncated: bool,
    /// 因权限 / 读目录失败而跳过的目录数。
    pub skipped_dirs: usize,
}

/// 把路径渲成 `/` 分隔的字符串。
pub fn slash(path: &Path) -> String {
    let text = path.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        text.into_owned()
    } else {
        text.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

/// 通配匹配：`*` 任意串（不含分隔符）、`?` 单字符、其余字面量。
///
/// 只用得上这两种元字符：参考仓的 JAR 排除写的是 `spring-*`、`*-sources.jar`，
/// 引 `globset` 会为一个 `*` 拖进一整套正则自动机（还有 MSRV 风险）。
/// 回溯实现是 O(n·m) 最坏，但模式串都是人写的短串。
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    // 记住最近一个 `*` 的位置：失配时只回溯到那里，不做指数级枚举。
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// 模式串列表里是否有一条命中「文件名」或「相对路径」。
pub fn any_match(patterns: &[String], name: &str, rel: &str) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim();
        !pattern.is_empty() && (glob_match(pattern, name) || glob_match(pattern, rel))
    })
}

/// 按条件遍历 `root`，每命中一个条目调一次 `visit`；返回 `Some(false)` 可提前收工
/// （搜索在够数之后就是这么停的）。
///
/// 遍历顺序：目录先于其内容（深度优先）、同层按名字排序——报告因此是稳定的，
/// 两次跑同一目录得到同样的行序，测试才能断言全文。
pub fn walk(
    root: &Path,
    options: &WalkOptions,
    mut visit: impl FnMut(&WalkEntry) -> Option<bool>,
) -> FilesResult<WalkOutcome> {
    if !root.exists() {
        return Err(FilesError::with_detail(
            ErrorKind::PathMissing,
            root.display(),
        ));
    }
    if !root.is_dir() {
        return Err(FilesError::with_detail(
            ErrorKind::NotADirectory,
            root.display(),
        ));
    }

    let mut walker = walkdir::WalkDir::new(root)
        .follow_links(options.follow_links)
        .sort_by(|a, b| {
            // 目录在前，同类按名字（不区分大小写）——与 `tree` 的观感一致。
            b.file_type()
                .is_dir()
                .cmp(&a.file_type().is_dir())
                .then_with(|| {
                    a.file_name()
                        .to_string_lossy()
                        .to_lowercase()
                        .cmp(&b.file_name().to_string_lossy().to_lowercase())
                })
        });
    if options.max_depth > 0 {
        walker = walker.max_depth(options.max_depth);
    }

    let mut outcome = WalkOutcome::default();
    // ⚠️ `filter_entry` 而不是循环里 `continue`：后者只跳过**目录本身**，子树照样
    // 往下走——`exclude = ["target"]` 会排除 `target/` 这一个条目，却把里面几万个
    // 文件全带回来。`filter_entry` 的返回值直接决定「要不要进这一层」。
    let entries = walker.into_iter().filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !options.include_hidden && name.starts_with('.') {
            return false;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        !any_match(&options.exclude, &name, &slash(rel))
    });

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            // 权限不足之类的单点失败不该中断整次遍历：记下，继续。
            Err(_) => {
                outcome.skipped_dirs += 1;
                continue;
            }
        };
        // 根目录自身不算条目。
        if entry.depth() == 0 {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_path_buf();
        let rel_slash = slash(&rel);
        let is_dir = entry.file_type().is_dir();

        // `include` 只筛文件：目录必须留着，否则永远走不到里面的文件。
        if !is_dir && !options.include.is_empty() && !any_match(&options.include, &name, &rel_slash)
        {
            continue;
        }

        let item = WalkEntry {
            path: entry.path().to_path_buf(),
            rel,
            is_dir,
            size: entry.metadata().map(|meta| meta.len()).unwrap_or(0),
        };
        outcome.entries += 1;
        if visit(&item) == Some(false) {
            outcome.truncated = true;
            return Ok(outcome);
        }
        if outcome.entries >= options.max_entries.max(1) {
            outcome.truncated = true;
            return Ok(outcome);
        }
    }
    Ok(outcome)
}

/// 只要文件的便捷版（搜索与批量改写都不关心目录）。
pub fn collect_files(
    root: &Path,
    options: &WalkOptions,
) -> FilesResult<(Vec<WalkEntry>, WalkOutcome)> {
    let mut files = Vec::new();
    let outcome = walk(root, options, |entry| {
        if !entry.is_dir {
            files.push(entry.clone());
        }
        Some(true)
    })?;
    Ok((files, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::temp_dir;

    #[test]
    fn glob_supports_star_and_question() {
        assert!(glob_match("*.jar", "foo.jar"));
        assert!(glob_match("spring-*", "spring-core"));
        assert!(glob_match("*-sources.jar", "a-sources.jar"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("*.jar", "foo.war"));
        // 连续 `*` 与结尾 `*` 都要收敛，否则回溯会卡住。
        assert!(glob_match("**a**", "xax"));
        assert!(glob_match("a*", "a"));
        assert!(!glob_match("a*b", "a"));
    }

    #[test]
    fn walk_skips_hidden_and_honours_depth() {
        let root = temp_dir("walk-depth");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir");
        std::fs::create_dir_all(root.join("src/deep/deeper")).expect("mkdir");
        std::fs::write(root.join(".hidden"), b"x").expect("write");
        std::fs::write(root.join("src/main.rs"), b"fn main() {}").expect("write");
        std::fs::write(root.join("src/deep/deeper/x.rs"), b"x").expect("write");

        let (files, _) = collect_files(&root, &WalkOptions::default()).expect("walk");
        let names: Vec<String> = files.iter().map(|file| file.rel_slash()).collect();
        assert_eq!(names, vec!["src/deep/deeper/x.rs", "src/main.rs"]);

        let shallow = WalkOptions {
            max_depth: 2,
            ..WalkOptions::default()
        };
        let (files, _) = collect_files(&root, &shallow).expect("walk");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rel_slash(), "src/main.rs");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn walk_filters_by_extension_and_reports_truncation() {
        let root = temp_dir("walk-filter");
        for index in 0..10 {
            std::fs::write(root.join(format!("f{index}.txt")), b"x").expect("write");
            std::fs::write(root.join(format!("f{index}.md")), b"x").expect("write");
        }
        let options = WalkOptions {
            include: vec!["*.md".to_string()],
            max_entries: 3,
            ..WalkOptions::default()
        };
        let (files, outcome) = collect_files(&root, &options).expect("walk");
        assert_eq!(files.len(), 3);
        assert!(outcome.truncated, "上限必须显式报出来");
        assert!(files.iter().all(|file| file.rel_slash().ends_with(".md")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn exclude_beats_include() {
        let root = temp_dir("walk-exclude");
        std::fs::create_dir_all(root.join("target")).expect("mkdir");
        std::fs::write(root.join("target/a.rs"), b"x").expect("write");
        std::fs::write(root.join("b.rs"), b"x").expect("write");
        let options = WalkOptions {
            exclude: vec!["target".to_string()],
            ..WalkOptions::default()
        };
        let (files, _) = collect_files(&root, &options).expect("walk");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rel_slash(), "b.rs");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_a_file_as_root() {
        let root = temp_dir("walk-file-root");
        let file = root.join("a.txt");
        std::fs::write(&file, b"x").expect("write");
        let error = collect_files(&file, &WalkOptions::default()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotADirectory);
        let _ = std::fs::remove_dir_all(root);
    }
}
