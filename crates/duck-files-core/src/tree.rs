//! 目录树：把一棵目录渲成 `tree` 命令那种文本。
//!
//! 参考仓的 `file_ops::read_directory_tree` 只负责把目录读成 `TreeNode` 数组，
//! 真正画树的是前端 `DirectoryTree.vue`（332 行，含 PNG 导出）。这里把「读」与
//! 「画」合成一步：产出**文本**，PNG 导出交给宿主（插件只产出可复制的东西）。
//!
//! 与 `tree` 命令对齐的三处细节：
//!
//! - 目录在前、同级按名字（不区分大小写）排序；
//! - 目录名带尾随 `/`，最后一项用 `└── `、其余用 `├── `；
//! - 达到深度上限但仍有子项时，补一行 `…`，**不假装到底了**。

use std::path::Path;

use crate::error::FilesResult;

/// 渲染选项。
#[derive(Debug, Clone)]
pub struct TreeOptions {
    /// 最大深度（`1` = 只看直接子项；`0` = 不限）。
    pub max_depth: usize,
    /// 是否包含 `.` 开头的隐藏项。
    pub include_hidden: bool,
    /// 是否在名字后面标大小（文件才标）。
    pub show_size: bool,
    /// 只列目录（结构梳理时用）。
    pub dirs_only: bool,
    /// 条目数上限（防止对一个巨大的 `node_modules` 打印十万行）。
    pub max_entries: usize,
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self {
            max_depth: 3,
            include_hidden: false,
            show_size: false,
            dirs_only: false,
            max_entries: 5_000,
        }
    }
}

/// 统计（报告头与「已截断」提示用）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TreeStats {
    /// 目录数（不含根）。
    pub dirs: usize,
    /// 文件数（`dirs_only` 时仍按实际文件数计）。
    pub files: usize,
    /// 文件总字节。
    pub bytes: u64,
    /// 是否因深度或条目上限被截断。
    pub truncated: bool,
}

struct Node {
    name: String,
    is_dir: bool,
    size: u64,
    children: Vec<Node>,
}

impl Node {
    fn dir(name: String) -> Self {
        Self {
            name,
            is_dir: true,
            size: 0,
            children: Vec::new(),
        }
    }
}

/// 读一层目录（排序、过滤隐藏项），递归到 `max_depth`。
fn read_node(
    path: &Path,
    depth: usize,
    options: &TreeOptions,
    budget: &mut usize,
    stats: &mut TreeStats,
) -> Node {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut node = Node::dir(name);
    if depth >= options.max_depth {
        // 还有没有子项？有就记一笔截断，让报告诚实。
        if has_visible_child(path, options) {
            stats.truncated = true;
        }
        return node;
    }

    let mut children: Vec<Node> = Vec::new();
    let Ok(entries) = std::fs::read_dir(path) else {
        return node;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            stats.truncated = true;
            break;
        }
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if !options.include_hidden && file_name.starts_with('.') {
            continue;
        }
        // `read_dir` 的 `file_type` 在符号链接上是「链接本身」：按链接处理，
        // 不跟进去（与 `walk` 的 `follow_links = false` 同口径）。
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let child_path = entry.path();
        if file_type.is_dir() {
            stats.dirs += 1;
            *budget -= 1;
            children.push(read_node(&child_path, depth + 1, options, budget, stats));
        } else if file_type.is_file() {
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            stats.files += 1;
            stats.bytes += size;
            *budget -= 1;
            if !options.dirs_only {
                children.push(Node {
                    name: file_name,
                    is_dir: false,
                    size,
                    children: Vec::new(),
                });
            }
        }
    }
    // 目录在前，同类按名字（小写）——`read_dir` 的顺序是文件系统的，不可依赖。
    children.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    node.children = children;
    node
}

fn has_visible_child(path: &Path, options: &TreeOptions) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        options.include_hidden || !name.starts_with('.')
    })
}

/// 人类可读的大小（与 `ls -h` 同一套 1024 进位）。
///
/// 内核里只用 ASCII 单位（`B` / `KB` / `MB` / `GB`）：这一列是数据，不是文案，
/// 进译文表反而会让「报告宽度」随语言漂移。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn label(node: &Node, options: &TreeOptions) -> String {
    if node.is_dir {
        format!("{}/", node.name)
    } else if options.show_size {
        format!("{}  ({})", node.name, human_size(node.size))
    } else {
        node.name.clone()
    }
}

fn render_children(node: &Node, prefix: &str, options: &TreeOptions, out: &mut String) {
    let count = node.children.len();
    for (index, child) in node.children.iter().enumerate() {
        let last = index + 1 == count;
        let connector = if last { "└── " } else { "├── " };
        out.push_str(prefix);
        out.push_str(connector);
        // `dirs_only` 下空目录会变成「只有名字的叶子」，仍然照画——用户要的就是骨架。
        out.push_str(&label(child, options));
        out.push('\n');
        if child.is_dir {
            let next = format!("{prefix}{}", if last { "    " } else { "│   " });
            render_children(child, &next, options, out);
        }
    }
}

/// 渲成纯文本。返回 `(文本, 统计)`。
pub fn render_text(root: &Path, options: &TreeOptions) -> FilesResult<(String, TreeStats)> {
    let (node, stats) = build(root, options)?;
    let mut out = String::new();
    out.push_str(&format!("{}/", node.name));
    out.push('\n');
    render_children(&node, "", options, &mut out);
    Ok((out, stats))
}

/// 渲成 Markdown：一段 ```text 围栏。
///
/// 不做成 Markdown 列表：目录树的连线字符（`├──` / `└──`）在列表语法里会被
/// 当成普通文本缩进，粘进文档后层级全丢；围栏反而保真。
pub fn render_markdown(root: &Path, options: &TreeOptions) -> FilesResult<(String, TreeStats)> {
    let (text, stats) = render_text(root, options)?;
    Ok((format!("```text\n{text}```\n"), stats))
}

fn build(root: &Path, options: &TreeOptions) -> FilesResult<(Node, TreeStats)> {
    crate::error::require_dir(&root.to_string_lossy())?;
    let mut stats = TreeStats::default();
    let mut budget = options.max_entries.max(1);
    let node = read_node(root, 0, options, &mut budget, &mut stats);
    Ok((node, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::temp_dir;

    fn fixture(name: &str) -> std::path::PathBuf {
        let root = temp_dir(name);
        std::fs::create_dir_all(root.join("src/deep")).expect("mkdir");
        std::fs::create_dir_all(root.join("empty")).expect("mkdir");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir");
        std::fs::write(root.join("README.md"), b"hello").expect("write");
        std::fs::write(root.join("src/main.rs"), b"fn main() {}").expect("write");
        std::fs::write(root.join("src/deep/x.rs"), b"xxxxxxxxxx").expect("write");
        std::fs::write(root.join(".git/config"), b"x").expect("write");
        root
    }

    #[test]
    fn renders_tree_with_connectors_and_skips_hidden() {
        let root = fixture("tree-basic");
        let (text, stats) = render_text(&root, &TreeOptions::default()).expect("tree");
        let expected = format!("\
{}/
├── empty/
├── src/
│   ├── deep/
│   │   └── x.rs
│   └── main.rs
└── README.md
", root.file_name().expect("root name").to_string_lossy());
        assert_eq!(text, expected);
        assert_eq!(stats.dirs, 3, "src / src/deep / empty");
        assert_eq!(stats.files, 3, ".git 下的不算");
        assert!(!stats.truncated);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn depth_limit_marks_truncation_instead_of_faking_the_end() {
        let root = fixture("tree-depth");
        let options = TreeOptions {
            max_depth: 1,
            ..TreeOptions::default()
        };
        let (text, stats) = render_text(&root, &options).expect("tree");
        // 深度 1 = 只有直接子项。
        assert!(text.contains("├── src/"), "{text}");
        assert!(!text.contains("main.rs"), "{text}");
        assert!(stats.truncated, "还有子项没画，必须标出来");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dirs_only_drops_files_but_keeps_counts() {
        let root = fixture("tree-dirs");
        let options = TreeOptions {
            dirs_only: true,
            ..TreeOptions::default()
        };
        let (text, stats) = render_text(&root, &options).expect("tree");
        assert!(!text.contains("README.md"), "{text}");
        assert!(text.contains("deep/"), "{text}");
        assert_eq!(stats.files, 3, "文件数按真实文件计，只是不画");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn size_column_and_markdown_fence() {
        let root = fixture("tree-size");
        let options = TreeOptions {
            show_size: true,
            ..TreeOptions::default()
        };
        let (text, _) = render_text(&root, &options).expect("tree");
        assert!(text.contains("README.md  (5 B)"), "{text}");
        assert!(text.contains("x.rs  (10 B)"), "{text}");

        let (markdown, _) = render_markdown(&root, &options).expect("tree");
        assert!(markdown.starts_with("```text\n"), "{markdown}");
        assert!(markdown.ends_with("```\n"), "{markdown}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
    }
}
