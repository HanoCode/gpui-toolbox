//! AI 助手可调用的工具（M2）。
//!
//! 内核（`duck-ai::tool`）只定义协议与注册表，**实现留在这一层**——因为工具要读本机
//! 文件、要开知识库连接，那些都不该进一个「零依赖内核」。
//!
//! # 本轮只交付三个只读工具
//!
//! | 工具 | 风险 | 说明 |
//! | --- | --- | --- |
//! | `read_file` | 只读 | 读白名单目录内的 UTF-8 文本，支持行区间 |
//! | `list_dir` | 只读 | 列白名单目录内的条目 |
//! | `search_kb` | 只读 | 检索本地知识库（FTS5），带出处 |
//!
//! 设计文档 §4.6 里的第四个工具 `run_sql_readonly` **本轮不交付**，原因写在
//! `docs/ai-design.md` §10.3：它的「只读就自动执行」需要一个**活着的数据库会话**，
//! 而会话由 `pages/db.rs` 持有——那个文件正在被另一会话改写（§4.9 的同一条阻塞）。
//! 与其先发一个永远回「未连接数据库」的空壳（本项目明令禁止的演示项），
//! 不如等 §4.9 收敛时和 `duck-db::ai` 一起接。
//!
//! # 路径沙箱
//!
//! 两条规则，与 `duck-plugin-js` 的 `readFile` 同源：
//!
//! 1. **拒绝任何含 `..` 的路径**（`Component::ParentDir`），不管它最后能不能归一化；
//! 2. 解析结果必须 `canonicalize()` 后落在某个允许根之内。
//!
//! 第 2 条才是真正的防线：`canonicalize` 会解开符号链接，所以「根目录里放一个
//! 指向 `/etc` 的软链」这种绕过会被拦住——只做字符串前缀比较是拦不住的。
//!
//! 白名单根由用户显式配置（设置 → AI → 工具）。**默认关闭**：本项目既有口径是
//! 「涉及本机数据的开关由用户自己打开」（`redis.rs` 的 `require_confirm_writes` 同源）。

use std::path::{Component, Path, PathBuf};

use duck_ai::kb::KbIndex;
use duck_ai::tool::{Tool, ToolDetail, ToolOutcome, ToolRegistry, ToolRisk, ToolSpec};
use duck_ai::wire::ToolCall;
use gpui::App;

use crate::ai;

/// 单次读取的字符上限（超过就直说，而不是把上下文塞爆）。
const READ_MAX_CHARS: usize = 60_000;
/// 单次读取的最大字节数（超过直接拒绝，不读进来）。
const READ_MAX_BYTES: u64 = 1024 * 1024;
/// `read_file` 默认返回的行数。
const READ_DEFAULT_LINES: usize = 400;
/// `read_file` 一次最多返回的行数。
const READ_MAX_LINES: usize = 2000;
/// `list_dir` 一次最多返回的条目数。
const LIST_MAX_ENTRIES: usize = 300;
/// `search_kb` 默认返回的片段数。
const KB_DEFAULT_HITS: usize = 5;
/// `search_kb` 一次最多返回的片段数。
const KB_MAX_HITS: usize = 20;

// ---------------------------------------------------------------------------
// 路径沙箱
// ---------------------------------------------------------------------------

/// 允许访问的根目录集合。空集合 = 所有文件类工具都不可用。
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    roots: Vec<PathBuf>,
}

impl Sandbox {
    /// 由用户配置的目录构造。**不存在的目录被丢弃**（canonicalize 失败），
    /// 且构造时就归一化，避免每次调用都重算。
    #[must_use]
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut seen: Vec<PathBuf> = Vec::new();
        for root in roots {
            // 只有真实存在的目录才配当根：canonicalize 顺带解开符号链接，
            // 于是 `/tmp` 与 `/private/tmp` 这类同义路径会归一成同一个。
            let Ok(real) = root.canonicalize() else {
                continue;
            };
            if real.is_dir() && !seen.contains(&real) {
                seen.push(real);
            }
        }
        seen.sort();
        Self { roots: seen }
    }

    /// 一个根都没有。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// 根目录清单（只读展示用）。
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 把模型给的路径解析成**真实存在的绝对路径**，且必须落在某个根之内。
    ///
    /// 相对路径按每个根依次尝试；绝对路径只做落点校验。
    /// 失败时返回**给模型读的原因**（英文，进上下文），不是给用户看的文案。
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("path must not be empty".to_owned());
        }
        let candidate = Path::new(trimmed);
        // 第 1 条规则：`..` 一律拒绝。不做「归一化后如果还在根内就放行」的聪明事——
        // 那需要自己实现路径归一化，而自己写的归一化出错的代价是沙箱失效。
        if candidate
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(format!("path {trimmed:?} must not contain '..'"));
        }
        let attempts: Vec<PathBuf> = if candidate.is_absolute() {
            vec![candidate.to_path_buf()]
        } else {
            self.roots.iter().map(|root| root.join(candidate)).collect()
        };
        for attempt in attempts {
            let Ok(real) = attempt.canonicalize() else {
                continue;
            };
            // 第 2 条规则：软链解完还得在根内。用组件级前缀比较，
            // 避免 `/data-extra` 被 `/data` 误判为「在里面」。
            if self
                .roots
                .iter()
                .any(|root| real.starts_with(root) && real != *root)
            {
                return Ok(real);
            }
        }
        Err(format!(
            "path {trimmed:?} is outside the allowed workspace ({})",
            self.describe_roots()
        ))
    }

    /// 允许根的人类可读清单（错误信息里回给模型，让它知道该往哪找）。
    fn describe_roots(&self) -> String {
        if self.roots.is_empty() {
            return "no workspace directory configured".to_owned();
        }
        self.roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

/// 读文件。
pub struct ReadFileTool {
    sandbox: Sandbox,
}

impl ReadFileTool {
    #[must_use]
    pub fn new(sandbox: Sandbox) -> Self {
        Self { sandbox }
    }
}

impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a UTF-8 text file inside the user's workspace. \
                 Use start_line/max_lines to page through long files."
                .to_owned(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, relative to the workspace root."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "1-based first line to return. Defaults to 1."
                    },
                    "max_lines": {
                        "type": "integer",
                        "description": "How many lines to return. Defaults to 400."
                    }
                },
                "required": ["path"]
            }),
            risk: ToolRisk::ReadOnly,
        }
    }

    fn run(
        &self,
        input: &serde_json::Value,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> ToolOutcome {
        let Some(raw) = input.get("path").and_then(serde_json::Value::as_str) else {
            return ToolOutcome::error("missing required argument: path");
        };
        let path = match self.sandbox.resolve(raw) {
            Ok(path) => path,
            Err(reason) => return ToolOutcome::error(reason),
        };
        let Ok(metadata) = std::fs::metadata(&path) else {
            return ToolOutcome::error(format!("cannot stat {raw:?}"));
        };
        if metadata.is_dir() {
            return ToolOutcome::error(format!("{raw:?} is a directory; use list_dir instead"));
        }
        if metadata.len() > READ_MAX_BYTES {
            return ToolOutcome::error(format!(
                "file is {} bytes, above the {READ_MAX_BYTES}-byte read limit",
                metadata.len()
            ));
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return ToolOutcome::error(format!("{raw:?} is not a UTF-8 text file"));
        };

        let start = input
            .get("start_line")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let take = input
            .get("max_lines")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(READ_DEFAULT_LINES as u64)
            .clamp(1, READ_MAX_LINES as u64) as usize;

        let total_lines = text.lines().count();
        let mut body = String::new();
        let mut shown = 0usize;
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            if number < start {
                continue;
            }
            if shown >= take {
                break;
            }
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return ToolOutcome::error("cancelled while reading");
            }
            // 行号帮模型引用具体位置（「第 42 行」比「那段代码」可核对得多）。
            body.push_str(&format!("{number:>6}│ {line}\n"));
            shown += 1;
        }
        if body.chars().count() > READ_MAX_CHARS {
            body = body.chars().take(READ_MAX_CHARS).collect();
        }

        let mut content = format!(
            "{}\n{} line(s) starting at {start} of {total_lines} total\n\n{body}",
            path.display(),
            shown
        );
        if start + shown - 1 < total_lines {
            content.push_str(&format!(
                "\n[more lines available: call again with start_line={}]\n",
                start + shown
            ));
        }
        ToolOutcome::text(content)
    }
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

/// 列目录。
pub struct ListDirTool {
    sandbox: Sandbox,
}

impl ListDirTool {
    #[must_use]
    pub fn new(sandbox: Sandbox) -> Self {
        Self { sandbox }
    }
}

/// 一个目录项。
struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
}

impl Tool for ListDirTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".to_owned(),
            description: "List files and sub-directories inside the user's workspace. \
                 Directories are listed first."
                .to_owned(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path relative to the workspace root. \
                                        Empty means the root itself."
                    }
                }
            }),
            risk: ToolRisk::ReadOnly,
        }
    }

    fn run(
        &self,
        input: &serde_json::Value,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> ToolOutcome {
        let raw = input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        // 空路径 = 根目录本身。`Sandbox::resolve` 不允许空串（那在 read_file 里是错的），
        // 所以这里对根目录走一条单独的、同样受控的路径。
        let directory = if raw.trim().is_empty() {
            match self.sandbox.roots().first() {
                Some(root) => root.clone(),
                None => {
                    return ToolOutcome::error(
                        "no workspace directory is configured, so nothing can be listed",
                    );
                }
            }
        } else {
            match self.sandbox.resolve(raw) {
                Ok(path) => path,
                Err(reason) => return ToolOutcome::error(reason),
            }
        };
        if !directory.is_dir() {
            return ToolOutcome::error(format!("{:?} is not a directory", display(&directory)));
        }

        let Ok(entries) = std::fs::read_dir(&directory) else {
            return ToolOutcome::error(format!("cannot read directory {:?}", display(&directory)));
        };
        let mut items: Vec<Entry> = Vec::new();
        for entry in entries.flatten() {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return ToolOutcome::error("cancelled while listing");
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // 隐藏文件不进清单：`.git` / `.DS_Store` 只会挤掉有用的条目。
            if name.starts_with('.') {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            items.push(Entry {
                name,
                is_dir: metadata.is_dir(),
                size: metadata.len(),
            });
        }
        items.sort_by(|left, right| {
            right
                .is_dir
                .cmp(&left.is_dir)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        let truncated = items.len() > LIST_MAX_ENTRIES;
        items.truncate(LIST_MAX_ENTRIES);

        if items.is_empty() {
            return ToolOutcome::text(format!("{}\n(empty)", display(&directory)));
        }

        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|item| {
                vec![
                    item.name.clone(),
                    if item.is_dir { "dir" } else { "file" }.to_owned(),
                    if item.is_dir {
                        "—".to_owned()
                    } else {
                        human_size(item.size)
                    },
                ]
            })
            .collect();
        let mut content = format!("{}\n", display(&directory));
        for row in &rows {
            content.push_str(&format!("{}  {}  {}\n", row[0], row[1], row[2]));
        }
        if truncated {
            content.push_str(&format!("[truncated at {LIST_MAX_ENTRIES} entries]\n"));
        }

        ToolOutcome::text(content).with_detail(ToolDetail::Table {
            columns: vec!["name".to_owned(), "kind".to_owned(), "size".to_owned()],
            rows,
        })
    }
}

// ---------------------------------------------------------------------------
// search_kb
// ---------------------------------------------------------------------------

/// 检索本地知识库。
#[derive(Default)]
pub struct SearchKbTool {
    /// 索引库路径；`None` = 应用数据目录下的默认 `ai.db`。
    path: Option<PathBuf>,
}

impl SearchKbTool {
    /// 指向指定的索引库。
    ///
    /// 生产路径只有一个（应用数据目录下的默认库），所以这条构造器目前**只有测试在用**。
    /// 标 `cfg(test)` 而不是留一个永不被调用的 `pub fn`：bin crate 的 `pub` 不参与
    /// 死代码豁免，留着就是一条常驻警告。
    #[cfg(test)]
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    fn open(&self) -> Result<KbIndex, duck_ai::AiError> {
        match &self.path {
            Some(path) => KbIndex::open(path),
            None => KbIndex::open_default(),
        }
    }
}

impl Tool for SearchKbTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_kb".to_owned(),
            description:
                "Full-text search over the user's local knowledge base (indexed documents). \
                 Returns the best matching excerpts together with their source document, \
                 so you can cite where an answer came from."
                    .to_owned(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search keywords. Chinese and English both work."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many excerpts to return. Defaults to 5."
                    }
                },
                "required": ["query"]
            }),
            risk: ToolRisk::ReadOnly,
        }
    }

    fn run(
        &self,
        input: &serde_json::Value,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> ToolOutcome {
        let Some(query) = input.get("query").and_then(serde_json::Value::as_str) else {
            return ToolOutcome::error("missing required argument: query");
        };
        let limit = input
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(KB_DEFAULT_HITS as u64)
            .clamp(1, KB_MAX_HITS as u64) as usize;

        let index = match self.open() {
            Ok(index) => index,
            Err(error) => return ToolOutcome::error(format!("cannot open the index: {error}")),
        };
        match index.stats() {
            Ok(stats) if stats.chunks == 0 => {
                // 说清楚「还没索引」而不是「没找到」：后者会让模型去改关键词，
                // 白转几轮还是空手。
                return ToolOutcome::error(
                    "the knowledge base is empty; no document has been indexed yet. \
                     Tell the user to add source directories in Settings → AI → Tools.",
                );
            }
            Ok(_) => {}
            Err(error) => return ToolOutcome::error(format!("cannot read the index: {error}")),
        }

        let hits = match index.search(query, limit) {
            Ok(hits) => hits,
            Err(error) => return ToolOutcome::error(format!("search failed: {error}")),
        };
        if hits.is_empty() {
            return ToolOutcome::text(format!("no excerpt matched {query:?}"));
        }

        let mut content = format!("{} hit(s) for {query:?}\n", hits.len());
        for (position, hit) in hits.iter().enumerate() {
            content.push_str(&format!(
                "\n[{}] {}\n{}\n",
                position + 1,
                hit.origin(),
                hit.snippet
            ));
        }
        ToolOutcome::text(content).with_detail(ToolDetail::Table {
            columns: vec!["source".to_owned(), "excerpt".to_owned()],
            rows: hits
                .iter()
                .map(|hit| vec![hit.origin(), hit.snippet.clone()])
                .collect(),
        })
    }
}

// ---------------------------------------------------------------------------
// 装配
// ---------------------------------------------------------------------------

/// 按当前配置装配注册表。
///
/// **配置读取都走 `duck_store::kv`（内存快照），不碰磁盘**——这个函数在页面构造与
/// 配置变更时各调一次，不在渲染路径上。
///
/// 三档结果：
///
/// | 配置 | 注册的工具 |
/// | --- | --- |
/// | 工具未开启 | 一个都没有（连 `tools` 字段都不下发） |
/// | 开启但没配工作目录 | 只有 `search_kb`（知识库不依赖工作目录） |
/// | 开启且配了目录 | 三个 |
#[must_use]
pub fn build_registry(cx: &App) -> ToolRegistry {
    let settings = ai::load_settings(cx);
    let mut registry = ToolRegistry::new();
    if !settings.tools_enabled {
        return registry;
    }

    let sandbox = Sandbox::new(
        settings
            .workspace_dir
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>(),
    );
    if !sandbox.is_empty() {
        registry.register(ReadFileTool::new(sandbox.clone()));
        registry.register(ListDirTool::new(sandbox));
    }
    registry.register(SearchKbTool::default());
    registry
}

/// 工具调用的一句话摘要（卡片标题右侧）。
///
/// 从入参里挑最有信息量的那个字段：`path` / `query`，都没有就退回整段 JSON
/// （截断到 80 字符，卡片放不下更长的）。
#[must_use]
pub fn call_summary(call: &ToolCall) -> String {
    let arguments = call.parsed_arguments();
    for key in ["path", "query"] {
        if let Some(value) = arguments.get(key).and_then(serde_json::Value::as_str)
            && !value.trim().is_empty()
        {
            return value.to_owned();
        }
    }
    let raw = arguments.to_string();
    raw.chars().take(80).collect()
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// 人类可读的字节数。
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|delta| delta.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!("duck-ai-tools-{tag}-{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn never_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn sandbox_rejects_parent_dir_escapes() {
        let root = temp_dir("escape");
        let sandbox = Sandbox::new([root.clone()]);
        assert!(sandbox.resolve("../../etc/passwd").is_err());
        assert!(sandbox.resolve("sub/../../etc/passwd").is_err());
        assert!(sandbox.resolve("").is_err());
    }

    #[test]
    fn sandbox_rejects_absolute_paths_outside_the_roots() {
        let root = temp_dir("outside");
        let sandbox = Sandbox::new([root]);
        let error = sandbox.resolve("/etc/hosts").unwrap_err();
        assert!(error.contains("outside the allowed workspace"), "{error}");
    }

    #[test]
    fn sandbox_accepts_files_inside_and_absolute_paths() {
        let root = temp_dir("inside");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/note.md"), "hello").unwrap();
        let sandbox = Sandbox::new([root.clone()]);
        assert!(sandbox.resolve("nested/note.md").is_ok());
        assert!(
            sandbox
                .resolve(&root.join("nested/note.md").to_string_lossy())
                .is_ok()
        );
        // 根目录自身不算「里面的文件」——`resolve` 的契约是解析到一个可读目标。
        assert!(sandbox.resolve("nested").is_ok());
    }

    #[test]
    fn sandbox_drops_roots_that_do_not_exist() {
        let sandbox = Sandbox::new([PathBuf::from("/definitely/not/here/duck")]);
        assert!(sandbox.is_empty());
    }

    #[test]
    fn sandbox_never_matches_a_sibling_with_a_shared_prefix() {
        // `/data-extra` 不能被当成 `/data` 里面。
        let root = temp_dir("prefix");
        let sibling = PathBuf::from(format!("{}-extra", root.to_string_lossy()));
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("secret.txt"), "x").unwrap();
        let sandbox = Sandbox::new([root]);
        assert!(
            sandbox
                .resolve(&sibling.join("secret.txt").to_string_lossy())
                .is_err()
        );
        std::fs::remove_dir_all(&sibling).ok();
    }

    #[test]
    fn read_file_returns_numbered_lines() {
        let root = temp_dir("read");
        std::fs::write(root.join("a.txt"), "first\nsecond\nthird\n").unwrap();
        let tool = ReadFileTool::new(Sandbox::new([root]));
        let outcome = tool.run(&serde_json::json!({"path": "a.txt"}), &never_cancel());
        assert!(outcome.ok, "{}", outcome.content);
        assert!(outcome.content.contains("     1│ first"));
        assert!(outcome.content.contains("     3│ third"));
        assert!(outcome.content.contains("3 line(s)"));
    }

    #[test]
    fn read_file_pages_with_start_line() {
        let root = temp_dir("read-page");
        let body: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        std::fs::write(root.join("big.txt"), body).unwrap();
        let tool = ReadFileTool::new(Sandbox::new([root]));
        let outcome = tool.run(
            &serde_json::json!({"path": "big.txt", "start_line": 40, "max_lines": 5}),
            &never_cancel(),
        );
        assert!(outcome.content.contains("    40│ line 40"));
        assert!(outcome.content.contains("5 line(s) starting at 40 of 50"));
        // 还有剩就要明说怎么继续，否则模型会以为文件到底了。
        assert!(outcome.content.contains("start_line=45"));
    }

    #[test]
    fn read_file_refuses_directories_and_binary() {
        let root = temp_dir("read-bad");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        let tool = ReadFileTool::new(Sandbox::new([root]));
        let directory = tool.run(&serde_json::json!({"path": "sub"}), &never_cancel());
        assert!(!directory.ok);
        assert!(directory.content.contains("list_dir"));
        let binary = tool.run(&serde_json::json!({"path": "bin.dat"}), &never_cancel());
        assert!(!binary.ok);
        assert!(binary.content.contains("UTF-8"));
    }

    #[test]
    fn read_file_requires_the_path_argument() {
        let root = temp_dir("read-missing");
        let tool = ReadFileTool::new(Sandbox::new([root]));
        let outcome = tool.run(&serde_json::json!({}), &never_cancel());
        assert!(!outcome.ok);
        assert!(outcome.content.contains("path"));
    }

    #[test]
    fn list_dir_puts_directories_first_and_skips_hidden() {
        let root = temp_dir("list");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("readme.md"), "x").unwrap();
        std::fs::write(root.join(".hidden"), "x").unwrap();
        let tool = ListDirTool::new(Sandbox::new([root]));
        let outcome = tool.run(&serde_json::json!({}), &never_cancel());
        assert!(outcome.ok, "{}", outcome.content);
        let rows = match outcome.detail {
            Some(ToolDetail::Table { rows, .. }) => rows,
            other => panic!("expected a table detail, got {other:?}"),
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            vec!["src".to_owned(), "dir".to_owned(), "—".to_owned()]
        );
        assert_eq!(rows[1][0], "readme.md");
    }

    #[test]
    fn list_dir_without_a_workspace_says_so() {
        let tool = ListDirTool::new(Sandbox::new(Vec::new()));
        let outcome = tool.run(&serde_json::json!({}), &never_cancel());
        assert!(!outcome.ok);
        assert!(outcome.content.contains("no workspace"));
    }

    #[test]
    fn search_kb_reports_an_empty_index_instead_of_no_match() {
        let file = std::env::temp_dir().join(format!(
            "duck-ai-kb-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let tool = SearchKbTool::at(&file);
        let outcome = tool.run(&serde_json::json!({"query": "任何"}), &never_cancel());
        // 「还没索引」和「没找到」是两件事：前者要引导用户去加来源。
        assert!(!outcome.ok);
        assert!(outcome.content.contains("empty"), "{}", outcome.content);
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn call_summary_prefers_the_path_then_the_query() {
        let call = |arguments: &str| ToolCall {
            id: "c".to_owned(),
            name: "t".to_owned(),
            arguments: arguments.to_owned(),
        };
        assert_eq!(
            call_summary(&call(r#"{"path":"src/main.rs"}"#)),
            "src/main.rs"
        );
        assert_eq!(call_summary(&call(r#"{"query":"连接失败"}"#)), "连接失败");
        // 半截 JSON 不该 panic，退回原样截断。
        assert_eq!(call_summary(&call(r#"{"path":"#)), "null");
    }

    #[test]
    fn human_size_reads_naturally() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
