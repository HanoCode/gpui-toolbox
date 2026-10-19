//! 目录比对（参考仓 `file_ops.rs` 的 `list_directory` / `compare_directories`，**逐字复刻**）。
//!
//! 判「同一个相对路径在两边的文件是否一样」只看 `size` 与 `mtime`，不读内容——
//! 这是源仓的既定口径（大目录下逐个读内容不可接受），复刻件照旧。
//!
//! 本文件同时提供 `list_directory`：目录树工具与「批量改名」将来都要它。
//!
//! 与源仓的差别：去掉 `#[tauri::command]`、结构体字段改为 `pub`、`Serialize` 由
//! `serde` 直接引入（源仓文件顶部统一 `use serde::Serialize;`）。
//!
//! ## 相对参考页补的两件（参考页没有，属**有意偏离**）
//!
//! 1. **排除规则**（[`is_excluded`]）：参考页只硬编码跳过 `.` 开头的隐藏项，
//!    真实目录里 `node_modules` / `target` / `dist` 会让清单一屏全是噪声。
//! 2. **视图筛选**（[`View`]）：参考页顶部有「全部 / 差异 / 相同」三个按钮，
//!    但只作用于树的显示；工具没有树，筛选必须落在输出上才算同一个能力。
//!
//! 两处都不改判定口径（仍是「同名文件只比 size + mtime」），只是决定**收集谁、
//! 展示谁**。

use serde::Serialize;

// 源仓里两个比对器同在一个文件、共用一份 `DiffStats`；这里分成两个模块后
// 直接复用同一份结构（字段与语义完全一致：added / removed / unchanged / modified）。
use crate::text_diff::DiffStats;

#[derive(Serialize, Debug)]
pub struct FileEntry {
    pub rel_path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: u64,
}

fn collect_dir_entries(
    dir: &std::path::Path,
    base: &std::path::Path,
    excludes: &[String],
    entries: &mut Vec<FileEntry>,
) -> Result<(), String> {
    let read_dir = std::fs::read_dir(dir).map_err(|e| format!("读取目录失败: {}", e))?;
    for entry in read_dir {
        let entry = entry.map_err(|e| format!("读取条目失败: {}", e))?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .map_err(|_| "路径错误".to_string())?;
        let rel_str = rel.to_string_lossy().to_string();
        if rel_str.starts_with('.') {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // 命中排除规则就不再往下走：`node_modules` 这种目录必须**剪枝**，
        // 只过滤结果集等于白扫几万个文件。
        if is_excluded(&name, excludes) {
            continue;
        }
        if path.is_dir() {
            entries.push(FileEntry {
                rel_path: rel_str.clone(),
                is_dir: true,
                size: 0,
                modified: 0,
            });
            collect_dir_entries(&path, base, excludes, entries)?;
        } else if path.is_file() {
            let meta = std::fs::metadata(&path).map_err(|e| format!("读取文件信息失败: {}", e))?;
            entries.push(FileEntry {
                rel_path: rel_str,
                is_dir: false,
                size: meta.len(),
                modified: meta
                    .modified()
                    .ok()
                    .map(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    })
                    .unwrap_or(0),
            });
        }
    }
    Ok(())
}

pub fn list_directory(path: String) -> Result<Vec<FileEntry>, String> {
    list_directory_excluding(path, &[])
}

/// 同 [`list_directory`]，但跳过命中 `excludes` 的目录 / 文件。
pub fn list_directory_excluding(
    path: String,
    excludes: &[String],
) -> Result<Vec<FileEntry>, String> {
    let dir = std::path::Path::new(&path);
    if !dir.is_dir() {
        return Err("路径不是有效目录".to_string());
    }
    let mut entries = Vec::new();
    collect_dir_entries(dir, dir, excludes, &mut entries)?;
    Ok(entries)
}

/// 单条排除规则是否命中 `name`（大小写不敏感）。
///
/// 三种写法（参考页没有这个能力，这是本仓补的，口径尽量少而可解释）：
///
/// | 写法 | 含义 | 例 |
/// |---|---|---|
/// | 不带点 | 目录 / 文件**同名** | `node_modules` |
/// | 带点 | **后缀**匹配（扩展名） | `.log` / `*.log` |
/// | `*` 开头 | **后缀**匹配 | `*.min.js` |
///
/// 只拿 `name` 判定、不看相对路径：规则是「叫这个名字的都跳过」，
/// 这样在任意层级都一致，也不用教用户写路径通配。
pub fn is_excluded(name: &str, excludes: &[String]) -> bool {
    if excludes.is_empty() {
        return false;
    }
    let name = name.to_lowercase();
    excludes.iter().any(|pattern| {
        let pattern = pattern.trim().to_lowercase();
        if pattern.is_empty() {
            return false;
        }
        match pattern.strip_prefix('*') {
            Some(suffix) => name.ends_with(suffix),
            // 带点的一律按后缀看：`.log` 也能命中 `a.log`。
            None if pattern.contains('.') => name.ends_with(&pattern),
            None => name == pattern,
        }
    })
}

/// 视图筛选（对齐参考页顶部 `全部 / 差异 / 相同`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    All,
    /// 只要新增 / 删除 / 修改（参考页的「差异」）。
    Diff,
    /// 只要未变（参考页的「相同」）。
    Same,
}

impl View {
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "all" => Self::All,
            "same" => Self::Same,
            _ => Self::Diff,
        }
    }

    /// `kind` 是 [`DirDiffEntry::kind`] 的四档之一。
    pub fn keeps(self, kind: &str) -> bool {
        match self {
            Self::All => true,
            Self::Diff => kind != "equal",
            Self::Same => kind == "equal",
        }
    }
}

fn collect_file_paths(
    dir: &std::path::Path,
    base: &std::path::Path,
    paths: &mut Vec<String>,
) -> Result<(), String> {
    let read_dir = std::fs::read_dir(dir).map_err(|e| format!("读取目录失败: {}", e))?;
    for entry in read_dir {
        let entry = entry.map_err(|e| format!("读取条目失败: {}", e))?;
        let path = entry.path();
        if path.is_dir() {
            collect_file_paths(&path, base, paths)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(base)
                .map_err(|_| "路径错误".to_string())?;
            paths.push(rel.to_string_lossy().to_string());
        }
    }
    Ok(())
}

pub fn list_files_recursive(dir_path: String) -> Result<Vec<String>, String> {
    let dir = std::path::Path::new(&dir_path);
    if !dir.is_dir() {
        return Err("路径不是有效目录".to_string());
    }
    let mut paths = Vec::new();
    collect_file_paths(dir, dir, &mut paths)?;
    paths.sort();
    Ok(paths)
}

#[derive(Serialize)]
pub struct TreeNode {
    pub name: String,
    pub path: String,
    #[serde(rename = "isDir")]
    pub is_dir: bool,
    pub children: Option<Vec<TreeNode>>,
}

fn collect_tree(
    dir: &std::path::Path,
    depth: u32,
    max_depth: u32,
    show_hidden: bool,
) -> Result<Vec<TreeNode>, String> {
    let read_dir = std::fs::read_dir(dir).map_err(|e| format!("读取目录失败: {}", e))?;
    let mut nodes = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|e| format!("读取条目失败: {}", e))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = path.is_dir();
        if is_dir && depth < max_depth {
            let children = collect_tree(&path, depth + 1, max_depth, show_hidden)?;
            nodes.push(TreeNode {
                name,
                path: path.to_string_lossy().to_string(),
                is_dir,
                children: Some(children),
            });
        } else {
            nodes.push(TreeNode {
                name,
                path: path.to_string_lossy().to_string(),
                is_dir,
                children: None,
            });
        }
    }
    nodes.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            if a.is_dir {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        } else {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
    });
    Ok(nodes)
}

pub fn read_directory_tree(
    dir_path: String,
    max_depth: u32,
    show_hidden: bool,
) -> Result<Vec<TreeNode>, String> {
    let dir = std::path::Path::new(&dir_path);
    if !dir.is_dir() {
        return Err("路径不是有效目录".to_string());
    }
    collect_tree(dir, 0, max_depth, show_hidden)
}

#[derive(Serialize, Debug)]
pub struct DirDiffEntry {
    pub rel_path: String,
    pub is_dir: bool,
    pub kind: String,
    pub size_a: u64,
    pub size_b: u64,
    pub modified_a: u64,
    pub modified_b: u64,
}

#[derive(Serialize)]
pub struct DirDiffResult {
    pub entries: Vec<DirDiffEntry>,
    pub stats: DiffStats,
}

pub fn compare_directories(dir_a: String, dir_b: String) -> Result<DirDiffResult, String> {
    compare_directories_excluding(dir_a, dir_b, &[])
}

/// 同 [`compare_directories`]，两侧都按 `excludes` 剪枝。
pub fn compare_directories_excluding(
    dir_a: String,
    dir_b: String,
    excludes: &[String],
) -> Result<DirDiffResult, String> {
    let entries_a = list_directory_excluding(dir_a, excludes)?;
    let entries_b = list_directory_excluding(dir_b, excludes)?;

    let map_a: std::collections::HashMap<String, &FileEntry> =
        entries_a.iter().map(|e| (e.rel_path.clone(), e)).collect();
    let map_b: std::collections::HashMap<String, &FileEntry> =
        entries_b.iter().map(|e| (e.rel_path.clone(), e)).collect();

    let mut all_paths: Vec<String> = map_a.keys().cloned().collect();
    for k in map_b.keys() {
        if !all_paths.contains(k) {
            all_paths.push(k.clone());
        }
    }
    all_paths.sort();

    let mut result = Vec::new();
    let mut added = 0u32;
    let mut removed = 0u32;
    let mut unchanged = 0u32;
    let mut modified = 0u32;

    for path in &all_paths {
        let a = map_a.get(path);
        let b = map_b.get(path);
        let kind = match (a, b) {
            (Some(_), None) => {
                removed += 1;
                "removed".to_string()
            }
            (None, Some(_)) => {
                added += 1;
                "added".to_string()
            }
            (Some(a), Some(b)) if a.is_dir && b.is_dir => {
                unchanged += 1;
                "equal".to_string()
            }
            (Some(a), Some(b)) if a.size == b.size && a.modified == b.modified => {
                unchanged += 1;
                "equal".to_string()
            }
            (Some(_), Some(_)) => {
                modified += 1;
                "modified".to_string()
            }
            _ => "equal".to_string(),
        };
        result.push(DirDiffEntry {
            rel_path: path.clone(),
            is_dir: a.map(|e| e.is_dir).or(b.map(|e| e.is_dir)).unwrap_or(false),
            kind: kind.to_string(),
            size_a: a.map(|e| e.size).unwrap_or(0),
            size_b: b.map(|e| e.size).unwrap_or(0),
            modified_a: a.map(|e| e.modified).unwrap_or(0),
            modified_b: b.map(|e| e.modified).unwrap_or(0),
        });
    }

    Ok(DirDiffResult {
        entries: result,
        stats: DiffStats {
            added,
            removed,
            unchanged,
            modified,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-dir-compare-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// 新增 / 删除 / 未变三种情形各来一条；目录本身也算一条。
    #[test]
    fn compares_two_directory_listings() {
        let a = temp_dir("a");
        let b = temp_dir("b");
        std::fs::write(a.join("same.txt"), "x").expect("write");
        std::fs::write(b.join("same.txt"), "x").expect("write");
        std::fs::write(a.join("only_a.txt"), "x").expect("write");
        std::fs::write(b.join("only_b.txt"), "y").expect("write");
        std::fs::create_dir_all(a.join("sub")).expect("mkdir");

        let result = compare_directories(
            a.to_string_lossy().to_string(),
            b.to_string_lossy().to_string(),
        )
        .expect("compare");

        let kind_of = |path: &str| {
            result
                .entries
                .iter()
                .find(|entry| entry.rel_path == path)
                .map(|entry| entry.kind.as_str())
                .unwrap_or("<missing>")
        };
        assert_eq!(kind_of("same.txt"), "equal");
        assert_eq!(kind_of("only_a.txt"), "removed");
        assert_eq!(kind_of("only_b.txt"), "added");
        assert!(result.entries.iter().any(|entry| entry.is_dir));
        assert_eq!(result.stats.added, 1);
        // 只在 A 侧存在的**目录**也计一条 removed —— 这是源仓 `compare_directories`
        // 的口径（`(Some(_), None) => removed += 1`，不区分文件与目录）。
        assert_eq!(result.stats.removed, 2);

        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(b);
    }

    /// 不是目录时给中文错误（英文界面由插件表翻）。
    #[test]
    fn non_directory_is_rejected() {
        let dir = temp_dir("not-dir");
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").expect("write");
        let error = list_directory(file.to_string_lossy().to_string()).expect_err("must fail");
        assert_eq!(error, "路径不是有效目录");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 排除规则三种写法的判定表（`node_modules` 同名 / `.log` 扩展名 / `*.min.js` 后缀）。
    #[test]
    fn exclude_patterns_match_three_ways() {
        let rules: Vec<String> = ["node_modules", ".log", "*.min.js"]
            .iter()
            .map(|rule| rule.to_string())
            .collect();
        assert!(is_excluded("node_modules", &rules));
        assert!(is_excluded("Node_Modules", &rules), "大小写不敏感");
        assert!(is_excluded("app.log", &rules));
        assert!(is_excluded("APP.LOG", &rules));
        assert!(is_excluded("app.min.js", &rules));
        assert!(!is_excluded("app.js", &rules));
        assert!(
            !is_excluded("my_node_modules_backup", &rules),
            "同名不是包含"
        );
        assert!(!is_excluded("node_modules", &[]), "空规则不排除任何东西");
    }

    /// 命中排除规则的**目录要剪枝**：里面即使有差异也不该出现在清单里。
    #[test]
    fn excluded_directory_is_pruned_not_just_filtered() {
        let a = temp_dir("prune-a");
        let b = temp_dir("prune-b");
        std::fs::create_dir_all(a.join("node_modules")).expect("mkdir");
        std::fs::create_dir_all(b.join("node_modules")).expect("mkdir");
        std::fs::write(a.join("node_modules/left.txt"), "x").expect("write");
        std::fs::write(a.join("keep.txt"), "x").expect("write");
        std::fs::write(b.join("keep.txt"), "x").expect("write");

        let plain = compare_directories(
            a.to_string_lossy().to_string(),
            b.to_string_lossy().to_string(),
        )
        .expect("compare");
        assert!(
            plain
                .entries
                .iter()
                .any(|e| e.rel_path.starts_with("node_modules"))
        );

        let rules = vec!["node_modules".to_string()];
        let pruned = compare_directories_excluding(
            a.to_string_lossy().to_string(),
            b.to_string_lossy().to_string(),
            &rules,
        )
        .expect("compare");
        assert!(
            !pruned
                .entries
                .iter()
                .any(|e| e.rel_path.starts_with("node_modules")),
            "排除目录下的条目一条都不该在清单里"
        );
        assert_eq!(pruned.stats.unchanged, 1, "只剩 keep.txt");
        assert_eq!(
            pruned.stats.added + pruned.stats.removed + pruned.stats.modified,
            0
        );

        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(b);
    }

    /// 视图筛选：`diff` 挡掉 equal，`same` 只留 equal。
    #[test]
    fn view_filters_entries() {
        assert_eq!(View::parse("all"), View::All);
        assert_eq!(View::parse("same"), View::Same);
        assert_eq!(View::parse(""), View::Diff, "缺省是「只看差异」");
        assert!(View::All.keeps("equal"));
        assert!(!View::Diff.keeps("equal"));
        assert!(View::Diff.keeps("modified"));
        assert!(View::Same.keeps("equal"));
        assert!(!View::Same.keeps("added"));
    }
}
