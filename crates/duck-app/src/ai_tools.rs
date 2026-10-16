//! AI 助手可调用的工具（M2）。
//!
//! 内核（`duck-ai::tool`）只定义协议与注册表，**实现留在这一层**——因为工具要读本机
//! 文件、要开知识库连接，那些都不该进一个「零依赖内核」。
//!
//! # 四个只读工具
//!
//! | 工具 | 风险 | 说明 |
//! | --- | --- | --- |
//! | `read_file` | 只读 | 读白名单目录内的 UTF-8 文本，支持行区间 |
//! | `list_dir` | 只读 | 列白名单目录内的条目 |
//! | `search_kb` | 只读 | 检索本地知识库（FTS5），带出处 |
//! | `run_sql_readonly` | 只读 | 在**用户已打开**的数据库连接上跑一段**可证明只读**的 SQL |
//!
//! `run_sql_readonly`（`docs/ai-design.md` §4.6 的第四个工具）此前暂缓，原因是「会话句柄在
//! `DatabasePage` 内部，没有可查的会话注册表」。现在数据库侧的只读入口已就绪
//! （`DatabasePage::readonly_session` + `duck_db::Connection::execute_read_only`），
//! 于是本模块用一份**进程级只读会话目录**把「页面持有的会话」暴露给工具：
//! 连接成功时页面 [`publish_sql_session`]，断开时 [`retract_sql_session`]。
//!
//! **它不是新连接**：发布的句柄就是页面那条活跃会话的克隆（同一事务状态），
//! 且闸门在**内核**里（`execute_read_only` 只放行证明得了只读的语句，一条不达标就整体拒绝），
//! 所以拿到句柄也跑不了写语句——风险档因此定为 [`ToolRisk::ReadOnly`]（见工具实现处的说明）。
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

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use duck_ai::kb::KbIndex;
use duck_ai::tool::{Tool, ToolDetail, ToolOutcome, ToolRegistry, ToolRisk, ToolSpec};
use duck_ai::wire::ToolCall;
use duck_db::{DbError, ExecOutcome};
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
/// `run_sql_readonly` 默认返回的行数。
const SQL_DEFAULT_ROWS: usize = 100;
/// `run_sql_readonly` 一次最多返回的行数（外层还会按上下文档位截断文本）。
const SQL_MAX_ROWS: usize = 1_000;

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
                        duck_theme::bytes(item.size)
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
// run_sql_readonly 的会话目录
// ---------------------------------------------------------------------------

/// 「跑一段只读 SQL」这一件事的句柄。
///
/// 刻意用**闭包**而不是 `ReadOnlySession` 类型：那个类型住在本 crate 的 `pages::db`
/// 私有模块里，工具层不该为了一个工具去抬它的可见性。闭包把「怎么执行」封装好，
/// 工具层只认「给我 SQL 与行上限，还我一个 `ExecOutcome` 或错误」。
type SqlRunner = Arc<dyn Fn(&str, usize) -> Result<ExecOutcome, DbError> + Send + Sync + 'static>;

/// 目录里一条已发布的只读会话。
struct CatalogEntry {
    /// 给模型看的连接名（`ConnectionConfig::display_name`）。
    label: String,
    run: SqlRunner,
}

/// 进程级只读会话目录。
///
/// 为什么是 `OnceLock<Mutex<..>>` 而不是 gpui `Global`：工具在**后台线程**里 `run`，
/// 那里没有 `cx`，拿不到 Global；发布方（`DatabasePage`）与消费方（工具）都必须能在
/// 无 `cx` 处访问。同仓 `db.rs` 的 `DbOpGate` 正是这套写法。
fn catalog() -> &'static Mutex<BTreeMap<String, CatalogEntry>> {
    static CATALOG: OnceLock<Mutex<BTreeMap<String, CatalogEntry>>> = OnceLock::new();
    CATALOG.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// 发布（或更新）一条只读会话，供 `run_sql_readonly` 使用。
///
/// 由数据库页在**连接成功**时调用；`run` 通常是
/// `move |sql, rows| session.execute(sql, rows)`。重复 id 覆盖旧句柄（重连场景）。
pub fn publish_sql_session(
    id: impl Into<String>,
    label: impl Into<String>,
    run: impl Fn(&str, usize) -> Result<ExecOutcome, DbError> + Send + Sync + 'static,
) {
    let entry = CatalogEntry {
        label: label.into(),
        run: Arc::new(run),
    };
    if let Ok(mut catalog) = catalog().lock() {
        catalog.insert(id.into(), entry);
    }
}

/// 撤回一条只读会话（断开标签时）。不存在的 id 是空操作。
pub fn retract_sql_session(id: &str) {
    if let Ok(mut catalog) = catalog().lock() {
        catalog.remove(id);
    }
}

/// 目录快照：`(id, label)`，按 id 升序。
fn catalog_listing() -> Vec<(String, String)> {
    catalog()
        .lock()
        .map(|catalog| {
            catalog
                .iter()
                .map(|(id, entry)| (id.clone(), entry.label.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// 取某条会话的执行句柄。
fn catalog_runner(id: &str) -> Option<SqlRunner> {
    catalog()
        .lock()
        .ok()
        .and_then(|catalog| catalog.get(id).map(|entry| Arc::clone(&entry.run)))
}

/// 目录为空时给模型的口径（引导它去让用户先连一个库，而不是干猜连接名）。
const NO_SESSION_HINT: &str = "No database connection is currently open. \
     Ask the user to open the Database page and connect first.";

/// 列出可选连接的文本（模型据此选择 `connection`）。
fn listing_text() -> String {
    let listing = catalog_listing();
    if listing.is_empty() {
        return NO_SESSION_HINT.to_owned();
    }
    let mut text = String::from("Open database connections:\n");
    for (id, label) in &listing {
        text.push_str(&format!("- {id}  ({label})\n"));
    }
    text.push_str("\nPass one id as `connection`, plus a read-only `sql` statement.");
    text
}

/// 把执行产出折成「回灌文本 + 结果表」。
fn render_exec_outcome(outcome: &ExecOutcome) -> (String, Option<ToolDetail>) {
    let Some(result) = outcome.result.as_ref() else {
        // 只读语句也可能不返回行（如 `PRAGMA` / `SHOW`）；如实说明，别假装没执行。
        return (
            format!(
                "statement(s) completed; no rows returned ({} statement(s) run)",
                outcome.statements_run
            ),
            None,
        );
    };
    let columns: Vec<String> = result
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    let rows: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(duck_db::Value::render).collect())
        .collect();

    let mut content = format!("{} row(s), {} column(s)\n", rows.len(), columns.len());
    content.push_str(&columns.join(" | "));
    content.push('\n');
    for row in &rows {
        content.push_str(&row.join(" | "));
        content.push('\n');
    }
    if result.truncated {
        // 行被上限截断要明说（与本模块「绝不静默丢内容」同一条纪律）。
        content.push_str("[row cap reached; more rows exist]\n");
    }
    (content, Some(ToolDetail::Table { columns, rows }))
}

// ---------------------------------------------------------------------------
// run_sql_readonly
// ---------------------------------------------------------------------------

/// 在用户**已打开**的连接上跑只读 SQL。
///
/// # 风险档：`ReadOnly`（自动执行），理由
///
/// 设计 §4.6 的口径是「看语句 → 证明为只读 → 自动执行」。**证明在执行内核里**
/// （`duck_db::Connection::execute_read_only`）：任何一条语句证明不了只读就整体拒绝、
/// 一条都不跑。所以这个工具**没有**做写操作的能力——把它标成 `NeedsApproval`
/// 只会让每次安全的 `SELECT` 都白弹一次确认，而真正的防线（内核闸门）一点没动。
///
/// 「连哪个库、查什么」这类**知情权**不靠审批满足：只有用户**主动打开**的连接才会
/// 出现在目录里（[`publish_sql_session`] 的调用方是页面本身），模型也无法凭空开新连接；
/// 而它执行的 SQL 与结果都原样显示在工具卡片上，用户看得到。
#[derive(Default)]
pub struct RunSqlReadonlyTool;

impl Tool for RunSqlReadonlyTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_sql_readonly".to_owned(),
            description: "Run a read-only SQL query against one of the user's currently open \
                 database connections. Only statements that can be proven read-only will run; \
                 anything that might write is refused by the engine and must be reviewed by the \
                 user. Call this with no arguments to list the available connections and their ids."
                .to_owned(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "connection": {
                        "type": "string",
                        "description": "Connection id from the listing (omit to list connections)."
                    },
                    "sql": {
                        "type": "string",
                        "description": "A read-only SQL query (e.g. SELECT)."
                    },
                    "max_rows": {
                        "type": "integer",
                        "description": "Row cap. Defaults to 100."
                    }
                }
            }),
            risk: ToolRisk::ReadOnly,
        }
    }

    fn run(&self, input: &serde_json::Value, cancel: &AtomicBool) -> ToolOutcome {
        let connection = input
            .get("connection")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(connection) = connection else {
            // 没点名连接 = 想看清单。这不是错误：模型需要知道能查什么。
            return ToolOutcome::text(listing_text());
        };
        let Some(sql) = input.get("sql").and_then(serde_json::Value::as_str) else {
            return ToolOutcome::error(format!(
                "missing required argument: sql (target connection {connection:?})"
            ));
        };
        if sql.trim().is_empty() {
            return ToolOutcome::error("sql must not be empty");
        }
        if cancel.load(Ordering::Relaxed) {
            return ToolOutcome::error("cancelled before execution");
        }

        let Some(run) = catalog_runner(connection) else {
            let available: Vec<String> = catalog_listing().into_iter().map(|(id, _)| id).collect();
            return ToolOutcome::error(format!(
                "unknown or closed connection {connection:?}; available: {}",
                if available.is_empty() {
                    "(none)".to_owned()
                } else {
                    available.join(", ")
                }
            ));
        };

        let max_rows = input
            .get("max_rows")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(SQL_DEFAULT_ROWS as u64)
            .clamp(1, SQL_MAX_ROWS as u64) as usize;

        // 同步阻塞调用：本工具总是在后台线程里跑（注册表随线程转移所有权），不卡 UI。
        match run(sql, max_rows) {
            Ok(outcome) => {
                let (content, detail) = render_exec_outcome(&outcome);
                let outcome = ToolOutcome::text(content);
                match detail {
                    Some(detail) => outcome.with_detail(detail),
                    None => outcome,
                }
            }
            // 写语句 / 证明不了只读：内核整体拒绝。这是**预期内的拒绝**，
            // 把原因原样回灌，模型据此改口径（而不是反复重试同一条语句）。
            Err(error) => ToolOutcome::error(format!("read-only execution refused: {error}")),
        }
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
/// | 配置 | 注册的工具 |
/// | --- | --- |
/// | 工具未开启 | 一个都没有（连 `tools` 字段都不下发） |
/// | 开启但没配工作目录 | `search_kb` + `run_sql_readonly`（都不依赖工作目录） |
/// | 开启且配了目录 | 四个 |
///
/// `run_sql_readonly` 即便此刻没有打开的连接也照常注册：它「只列清单」时不报错，
/// 模型据此知道有这个能力、要去让用户连库；等连接发布进目录后同一个注册表就能执行。
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
    registry.register(RunSqlReadonlyTool);
    registry
}

/// 工具调用的一句话摘要（卡片标题右侧）。
///
/// 从入参里挑最有信息量的那个字段：`path` / `query` / `sql` / `connection`，
/// 都没有就退回整段 JSON（截断到 80 字符，卡片放不下更长的）。
#[must_use]
pub fn call_summary(call: &ToolCall) -> String {
    let arguments = call.parsed_arguments();
    for key in ["path", "query", "sql", "connection"] {
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

    // ── run_sql_readonly（会话目录 + 工具）───────────────────────────

    use duck_db::{Column, ResultSet, Value};

    /// 造一份带行的执行产出。
    fn outcome_with_rows(columns: &[&str], rows: Vec<Vec<Value>>) -> ExecOutcome {
        ExecOutcome {
            result: Some(ResultSet {
                columns: columns
                    .iter()
                    .map(|name| Column {
                        name: (*name).to_owned(),
                        type_name: String::new(),
                        comment: String::new(),
                    })
                    .collect(),
                rows,
                truncated: false,
            }),
            affected: 0,
            statements_run: 1,
        }
    }

    /// 每个测试用互不相同的连接 id，避免共享的进程级目录互相串味。
    fn unique_id(tag: &str) -> String {
        format!(
            "sql-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn run_sql_readonly_lists_connections_when_no_target_is_given() {
        let id = unique_id("list");
        publish_sql_session(&id, "演示库", |_sql, _rows| {
            Ok(outcome_with_rows(&["a"], vec![]))
        });
        let tool = RunSqlReadonlyTool;
        let outcome = tool.run(&serde_json::json!({}), &never_cancel());
        assert!(outcome.ok, "{}", outcome.content);
        assert!(outcome.content.contains(&id));
        assert!(outcome.content.contains("演示库"));
        retract_sql_session(&id);
    }

    #[test]
    fn run_sql_readonly_returns_a_structured_table() {
        let id = unique_id("table");
        publish_sql_session(&id, "demo", |_sql, _rows| {
            Ok(outcome_with_rows(
                &["id", "name"],
                vec![
                    vec![Value::Int(1), Value::Text("a".into())],
                    vec![Value::Int(2), Value::Null],
                ],
            ))
        });
        let tool = RunSqlReadonlyTool;
        let outcome = tool.run(
            &serde_json::json!({"connection": id, "sql": "SELECT 1"}),
            &never_cancel(),
        );
        assert!(outcome.ok, "{}", outcome.content);
        assert!(outcome.content.contains("2 row(s), 2 column(s)"));
        let rows = match outcome.detail {
            Some(ToolDetail::Table { columns, rows }) => {
                assert_eq!(columns, vec!["id".to_owned(), "name".to_owned()]);
                rows
            }
            other => panic!("expected a table detail, got {other:?}"),
        };
        assert_eq!(rows[0], vec!["1".to_owned(), "a".to_owned()]);
        // NULL 渲染成字面 "NULL"，不是空串。
        assert_eq!(rows[1][1], "NULL");
        retract_sql_session(&id);
    }

    #[test]
    fn run_sql_readonly_surfaces_the_read_only_refusal() {
        let id = unique_id("refuse");
        publish_sql_session(&id, "demo", |_sql, _rows| {
            Err(DbError::Unsupported(
                "read-only execution refused a statement that is not provably read-only: DELETE"
                    .into(),
            ))
        });
        let tool = RunSqlReadonlyTool;
        let outcome = tool.run(
            &serde_json::json!({"connection": id, "sql": "DELETE FROM t"}),
            &never_cancel(),
        );
        // 拒绝是**预期内**的结果：ok=false，但原因要能读，模型据此换方案。
        assert!(!outcome.ok);
        assert!(outcome.content.contains("refused"), "{}", outcome.content);
        retract_sql_session(&id);
    }

    #[test]
    fn run_sql_readonly_rejects_unknown_and_retracted_connections() {
        let tool = RunSqlReadonlyTool;
        let unknown = tool.run(
            &serde_json::json!({"connection": "definitely-not-open", "sql": "SELECT 1"}),
            &never_cancel(),
        );
        assert!(!unknown.ok);
        assert!(unknown.content.contains("unknown or closed"));

        let id = unique_id("retract");
        publish_sql_session(&id, "demo", |_sql, _rows| {
            Ok(outcome_with_rows(&["a"], vec![]))
        });
        retract_sql_session(&id);
        let after = tool.run(
            &serde_json::json!({"connection": id, "sql": "SELECT 1"}),
            &never_cancel(),
        );
        assert!(!after.ok, "撤回后不该还能执行");
    }

    #[test]
    fn run_sql_readonly_clamps_max_rows_and_reports_no_rows() {
        let id = unique_id("rows");
        let seen = Arc::new(Mutex::new(Vec::<usize>::new()));
        let recorder = Arc::clone(&seen);
        publish_sql_session(&id, "demo", move |_sql, rows| {
            recorder.lock().unwrap().push(rows);
            // 只读但无返回行（如 PRAGMA）：如实说明，不假装没执行。
            Ok(ExecOutcome {
                result: None,
                affected: 0,
                statements_run: 1,
            })
        });
        let tool = RunSqlReadonlyTool;
        // 0 → 抬到 1；10_000 → 压到 SQL_MAX_ROWS。
        let _ = tool.run(
            &serde_json::json!({"connection": id, "sql": "PRAGMA x", "max_rows": 0}),
            &never_cancel(),
        );
        let _ = tool.run(
            &serde_json::json!({"connection": id, "sql": "PRAGMA x", "max_rows": 10_000}),
            &never_cancel(),
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.as_slice(), &[1, SQL_MAX_ROWS]);
        retract_sql_session(&id);

        let no_rows = tool.run(
            &serde_json::json!({"connection": id, "sql": "PRAGMA x"}),
            &never_cancel(),
        );
        // 会话已撤回 → 走「未知连接」分支；验证无行文本用另一条。
        assert!(!no_rows.ok);
        let id2 = unique_id("norows");
        publish_sql_session(&id2, "demo", |_sql, _rows| {
            Ok(ExecOutcome {
                result: None,
                affected: 0,
                statements_run: 1,
            })
        });
        let text = tool.run(
            &serde_json::json!({"connection": id2, "sql": "PRAGMA x"}),
            &never_cancel(),
        );
        assert!(text.ok);
        assert!(
            text.content.contains("no rows returned"),
            "{}",
            text.content
        );
        retract_sql_session(&id2);
    }

    #[test]
    fn run_sql_readonly_needs_sql_and_honours_cancel() {
        let id = unique_id("args");
        publish_sql_session(&id, "demo", |_sql, _rows| {
            Ok(outcome_with_rows(&["a"], vec![]))
        });
        let tool = RunSqlReadonlyTool;
        let missing = tool.run(&serde_json::json!({"connection": id}), &never_cancel());
        assert!(!missing.ok);
        assert!(missing.content.contains("sql"));

        let cancelled = tool.run(
            &serde_json::json!({"connection": id, "sql": "SELECT 1"}),
            &AtomicBool::new(true),
        );
        assert!(!cancelled.ok);
        assert!(cancelled.content.contains("cancelled"));
        retract_sql_session(&id);
    }
}
