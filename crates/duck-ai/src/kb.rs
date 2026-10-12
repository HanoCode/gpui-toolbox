//! 本地知识库：SQLite **FTS5** 索引与检索。
//!
//! 背景与选型见 `docs/ai-design.md` §4.7。本模块解决三件事：
//! 把目录扫成文本、把文本切成块、把块喂给 FTS5 并按 BM25 取回带出处的片段。
//!
//! ## 为什么不用默认分词器（**实测结论，别省这一步**）
//!
//! SQLite 的 `unicode61` 把 CJK 表意文字当作 letter 类，于是**一整段连续汉字
//! 会变成一个 token**。实测（`sqlite3` 3.54）：
//!
//! ```text
//! INSERT INTO t(body) VALUES ('数据库连接失败怎么办');
//! SELECT term FROM fts5vocab(t);
//!   → 数据库连接失败怎么办        ← 整段一个 token
//! SELECT * FROM t WHERE t MATCH '数据库';   → 0 行
//! SELECT * FROM t WHERE t MATCH '连接失败'; → 0 行
//! ```
//!
//! 也就是说**默认配置下中文检索完全失效**（英文好好的，所以很容易漏测）。
//! `tokenize='trigram'` 也救不了：三元组要求查询串至少 3 个字符，
//! 「连接」这种两字查询同样 0 行。
//!
//! 采用的方案：**索引前与查询前都做同一套 CJK 预分词**——把相邻的两个 CJK
//! 字符之间插入一个空格，让每个汉字成为独立 token；`unicode61` 照常工作，
//! 英文单词不受影响。展示时用 [`despace_cjk`] 还原。
//!
//! 代价（诚实记录）：① 索引体积略增（汉字间多一个空格）；
//! ② 「汉字 + 英文」交界处原本就没有空格时，还原后仍会留一个空格
//! （`abc数据库` → `abc 数据库`）——只影响片段显示，不影响检索正确性。
//!
//! ## 片段自己拼
//!
//! FTS5 的 `snippet()` 会给**每个** token 单独加括号，中文预分词后就是
//! `数 据 库 [连] [接]`——没法看。所以这里只借 FTS5 做「命中 + 排序」，
//! 片段在 Rust 侧按原文字符窗口重切（[`make_snippet`]）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use crate::AiError;

/// 一个索引来源（目录或单个文件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbSource {
    /// 稳定 id。用目录绝对路径的哈希，改名不动索引、换路径自动算新来源。
    pub id: String,
    /// 展示名（用户看得懂的名字，不是路径）。
    pub label: String,
    /// 绝对路径。
    pub path: PathBuf,
}

impl KbSource {
    /// 由路径构造，`label` 取目录名。
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let label = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Self {
            id: source_id(&path),
            label,
            path,
        }
    }
}

/// 来源 id：路径的稳定短哈希。
#[must_use]
pub fn source_id(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 一个可入库的文本块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 标题路径（`# 一级 / ## 二级`），无标题时为空串。
    pub heading: String,
    /// 正文。
    pub body: String,
}

/// 一条检索命中。
#[derive(Debug, Clone, PartialEq)]
pub struct KbHit {
    /// 来源 id。
    pub source_id: String,
    /// 来源展示名。
    pub label: String,
    /// 文件绝对路径（界面上的「出处」，也是「打开原文件」的目标）。
    pub path: String,
    /// 标题路径；可能为空。
    pub heading: String,
    /// 命中处的正文片段（已还原中文、已截断）。
    pub snippet: String,
    /// BM25 得分。**越负越相关**（FTS5 约定），原样带出供排序与调试。
    pub score: f64,
}

impl KbHit {
    /// 「出处」一行文案：`label · heading`（标题为空时只留 label）。
    #[must_use]
    pub fn origin(&self) -> String {
        let file = Path::new(&self.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.clone());
        if self.heading.is_empty() {
            format!("{} · {}", self.label, file)
        } else {
            format!("{} · {} · {}", self.label, file, self.heading)
        }
    }
}

/// 索引统计（设置页展示）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KbStats {
    /// 来源数量。
    pub sources: usize,
    /// 索引块数量。
    pub chunks: usize,
    /// 最近一次索引时间（unix 秒）；从未索引为 0。
    pub indexed_at: i64,
}

/// 一次索引的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReport {
    /// 本次写入的块数。
    pub chunks: usize,
    /// 扫描到的文件数。
    pub files: usize,
    /// 是否因指纹未变而跳过（增量命中）。
    pub skipped: bool,
}

/// 扫描 / 分块的进度回报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexProgress {
    /// 开始处理某个来源。
    Started { label: String, files: usize },
    /// 已写入若干块。
    Chunks { label: String, chunks: usize },
    /// 某个来源完成。
    Finished { label: String, report: IndexReport },
    /// 出错（记下来继续下一个来源，不是整体失败）。
    Failed { label: String, reason: String },
}

// ── 索引上限（防一次误选整个盘把内存吃完）──────────────────────────────

/// 单块最大字符数。
const MAX_CHUNK_CHARS: usize = 4_000;
/// 单文件最大字节数。
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 单个来源最多索引的文件数。
const MAX_FILES: usize = 4_000;
/// 目录递归深度上限。
const MAX_DEPTH: usize = 8;

/// 允许索引的扩展名（小写比较）。
const TEXT_EXTENSIONS: &[&str] = &[
    "md", "markdown", "txt", "rs", "toml", "json", "js", "mjs", "cjs", "ts", "tsx", "jsx", "py",
    "yaml", "yml", "sql", "sh", "bash", "zsh", "css", "html", "xml", "cfg", "ini", "conf", "go",
    "java", "kt", "c", "h", "cpp", "hpp", "rb", "php", "swift", "lua",
];

/// 遍历时直接跳过的目录名。
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    "target",
    "node_modules",
    "dist",
    "build",
    ".cache",
    ".venv",
    "__pycache__",
];

/// 知识库索引句柄。
///
/// **独立连接**：`AiStore` 那条连接归 UI 线程，索引跑在专用线程上，
/// 两条连接走同一个 WAL 文件互不阻塞（`ai-design.md` §4.7「索引在专用线程」）。
pub struct KbIndex {
    conn: Connection,
}

impl KbIndex {
    /// 打开（或创建）指定路径的库，并保证表结构就绪。
    pub fn open(path: &Path) -> Result<Self, AiError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AiError::Storage(format!("create kb dir failed: {error}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|error| AiError::Storage(format!("open kb index failed: {error}")))?;
        Self::from_connection(conn)
    }

    /// 打开应用数据目录下的 `ai.db`（与会话同库、异表）。
    pub fn open_default() -> Result<Self, AiError> {
        Self::open(&crate::store::default_path())
    }

    /// 内存库，仅测试用。
    pub fn open_in_memory() -> Result<Self, AiError> {
        let conn = Connection::open_in_memory()
            .map_err(|error| AiError::Storage(format!("open kb memory db failed: {error}")))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, AiError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kb_sources (
                 id TEXT PRIMARY KEY,
                 label TEXT NOT NULL,
                 path TEXT NOT NULL,
                 fingerprint TEXT NOT NULL DEFAULT '',
                 files INTEGER NOT NULL DEFAULT 0,
                 chunks INTEGER NOT NULL DEFAULT 0,
                 indexed_at INTEGER NOT NULL DEFAULT 0
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS kb_chunks USING fts5(
                 source_id UNINDEXED,
                 path UNINDEXED,
                 heading,
                 body,
                 tokenize = 'unicode61 remove_diacritics 2'
             );
             CREATE TABLE IF NOT EXISTS kb_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .map_err(|error| AiError::Storage(format!("create kb schema failed: {error}")))?;
        Ok(Self { conn })
    }

    /// 建表版本号，供将来迁移用。
    pub fn schema_version(&self) -> Result<i64, AiError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM kb_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .ok();
        Ok(raw.and_then(|value| value.parse().ok()).unwrap_or(1))
    }

    /// 当前索引统计。
    pub fn stats(&self) -> Result<KbStats, AiError> {
        let sources: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM kb_sources", [], |row| row.get(0))
            .map_err(storage("count kb_sources failed"))?;
        let chunks: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM kb_chunks", [], |row| row.get(0))
            .map_err(storage("count kb_chunks failed"))?;
        let indexed_at: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(indexed_at), 0) FROM kb_sources",
                [],
                |row| row.get(0),
            )
            .map_err(storage("read kb indexed_at failed"))?;
        Ok(KbStats {
            sources: sources.max(0) as usize,
            chunks: chunks.max(0) as usize,
            indexed_at,
        })
    }

    /// 全部来源（按加入顺序即路径序返回给设置页）。
    pub fn sources(&self) -> Result<Vec<KbSource>, AiError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, label, path FROM kb_sources ORDER BY path")
            .map_err(storage("prepare kb_sources failed"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(KbSource {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    path: PathBuf::from(row.get::<_, String>(2)?),
                })
            })
            .map_err(storage("query kb_sources failed"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(storage("read kb_sources row failed"))
    }

    /// 移除一个来源，连同它的全部块。
    pub fn remove_source(&self, id: &str) -> Result<(), AiError> {
        self.conn
            .execute("DELETE FROM kb_chunks WHERE source_id = ?1", params![id])
            .map_err(storage("delete kb_chunks failed"))?;
        self.conn
            .execute("DELETE FROM kb_sources WHERE id = ?1", params![id])
            .map_err(storage("delete kb_source failed"))?;
        Ok(())
    }

    /// 清空整个索引（保留来源清单）。设置页「重建索引」的第一步。
    pub fn clear_chunks(&self) -> Result<(), AiError> {
        self.conn
            .execute("DELETE FROM kb_chunks", [])
            .map_err(storage("clear kb_chunks failed"))?;
        self.conn
            .execute("UPDATE kb_sources SET fingerprint = '', chunks = 0", [])
            .map_err(storage("reset kb fingerprint failed"))?;
        Ok(())
    }

    /// 索引一个来源。
    ///
    /// **增量**：先只走 metadata 算指纹（不读文件内容），与库里记录一致就整体跳过。
    /// 指纹变了就整源重建——块级增量需要 per-file 表，而知识库本身
    /// **可重建且不参与备份**（§4.7），整源重扫的代价远低于维护第二张表的复杂度。
    pub fn index_source(
        &mut self,
        source: &KbSource,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<IndexReport, AiError> {
        let files = collect_files(&source.path, cancel);
        if cancel.load(Ordering::Relaxed) {
            return Ok(IndexReport {
                skipped: true,
                ..IndexReport::default()
            });
        }
        if files.is_empty() {
            return Err(AiError::Storage(format!(
                "no indexable text file under {}",
                source.path.display()
            )));
        }

        let fingerprint = fingerprint_of(&files);
        let recorded: Option<String> = self
            .conn
            .query_row(
                "SELECT fingerprint FROM kb_sources WHERE id = ?1",
                params![source.id],
                |row| row.get(0),
            )
            .ok();
        if recorded.as_deref() == Some(fingerprint.as_str()) {
            return Ok(IndexReport {
                files: files.len(),
                chunks: 0,
                skipped: true,
            });
        }

        progress(IndexProgress::Started {
            label: source.label.clone(),
            files: files.len(),
        });

        // 整源重建：先删旧块，再写新块。写在一个事务里——中途取消不会留下半个源。
        let tx = self
            .conn
            .transaction()
            .map_err(storage("begin kb transaction failed"))?;
        tx.execute(
            "DELETE FROM kb_chunks WHERE source_id = ?1",
            params![source.id],
        )
        .map_err(storage("delete stale kb_chunks failed"))?;

        let mut written = 0usize;
        // 进度节流。**不用 `written % 200 == 0`**：clippy 会建议换成
        // `is_multiple_of()`，而那个方法稳定于 Rust 1.87，**高于本仓 MSRV 1.85**。
        // （`crates/duck-ai/Cargo.toml` 没声明 `rust-version.workspace`，
        // 所以 clippy 不知道 MSRV 就发了这条建议——照改会破 MSRV 门。）
        let mut since_report = 0usize;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO kb_chunks (source_id, path, heading, body) VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(storage("prepare kb insert failed"))?;
            for file in &files {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(text) = std::fs::read_to_string(&file.path) else {
                    // 非 UTF-8 / 权限不足：跳过而不是中断整个来源。
                    continue;
                };
                for chunk in chunk_text(&text) {
                    insert
                        .execute(params![
                            source.id,
                            file.path.to_string_lossy(),
                            tokenize_cjk(&chunk.heading),
                            tokenize_cjk(&chunk.body),
                        ])
                        .map_err(storage("insert kb_chunk failed"))?;
                    written += 1;
                    since_report += 1;
                }
                if since_report >= 200 {
                    since_report = 0;
                    progress(IndexProgress::Chunks {
                        label: source.label.clone(),
                        chunks: written,
                    });
                }
            }
        }

        tx.execute(
            "INSERT INTO kb_sources (id, label, path, fingerprint, files, chunks, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 label = excluded.label,
                 path = excluded.path,
                 fingerprint = excluded.fingerprint,
                 files = excluded.files,
                 chunks = excluded.chunks,
                 indexed_at = excluded.indexed_at",
            params![
                source.id,
                source.label,
                source.path.to_string_lossy(),
                fingerprint,
                files.len() as i64,
                written as i64,
                now_secs(),
            ],
        )
        .map_err(storage("upsert kb_source failed"))?;
        tx.commit()
            .map_err(storage("commit kb transaction failed"))?;

        let report = IndexReport {
            chunks: written,
            files: files.len(),
            skipped: false,
        };
        progress(IndexProgress::Finished {
            label: source.label.clone(),
            report: report.clone(),
        });
        Ok(report)
    }

    /// 重建全部来源。`sources` 为空表示按库里已登记的来源重建。
    pub fn rebuild(
        &mut self,
        sources: &[KbSource],
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<KbStats, AiError> {
        let list: Vec<KbSource> = if sources.is_empty() {
            self.sources()?
        } else {
            sources.to_vec()
        };
        self.clear_chunks()?;
        for source in &list {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            if let Err(error) = self.index_source(source, cancel, progress) {
                // 单个来源失败不拖垮整体：设置页要能看到「哪个目录没索引上」。
                progress(IndexProgress::Failed {
                    label: source.label.clone(),
                    reason: error.to_string(),
                });
            }
        }
        self.stats()
    }

    /// 检索。`query` 是用户原话（不是 FTS5 语法），内部统一转义。
    ///
    /// 只读：本方法绝不写索引。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<KbHit>, AiError> {
        let Some(expression) = build_match(query) else {
            return Ok(Vec::new());
        };
        // ⚠️ FTS5 的表**不要起别名**：`MATCH` 与 `bm25()` 都按表名解析，
        // 一旦写成 `FROM kb_chunks c` 再 `WHERE c MATCH …`，多数版本会报
        // `no such column`。要联表就用「表名 + 别名」两套并存的写法。
        let mut stmt = self
            .conn
            .prepare(
                "SELECT kb_chunks.source_id, kb_chunks.path, kb_chunks.heading, kb_chunks.body,
                        bm25(kb_chunks) AS score, COALESCE(s.label, '')
                 FROM kb_chunks
                 LEFT JOIN kb_sources s ON s.id = kb_chunks.source_id
                 WHERE kb_chunks MATCH ?1
                 ORDER BY score
                 LIMIT ?2",
            )
            .map_err(storage("prepare kb search failed"))?;
        let terms = query_terms(query);
        let rows = stmt
            .query_map(params![expression, limit as i64], |row| {
                let heading: String = row.get(2)?;
                let body: String = row.get(3)?;
                Ok(KbHit {
                    source_id: row.get(0)?,
                    path: row.get(1)?,
                    heading: despace_cjk(&heading),
                    label: row.get(5)?,
                    snippet: make_snippet(&despace_cjk(&body), &terms, 160),
                    score: row.get(4)?,
                })
            })
            .map_err(storage("search kb_chunks failed"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(storage("read kb search row failed"))
    }
}

fn storage(context: &'static str) -> impl Fn(rusqlite::Error) -> AiError {
    move |error| AiError::Storage(format!("{context}: {error}"))
}

/// 当前 unix 秒。
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0)
}

/// 判断一个字符是否属于 CJK / 假名 / 谚文表意文字区。
///
/// 覆盖范围刻意保守（不做完整 Unicode 脚本判定）：错了会多插空格，
/// 只会让片段多一个空格，不会漏检索。
#[must_use]
pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF      // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 基本区
        | 0xF900..=0xFAFF    // CJK 兼容表意
        | 0x3040..=0x30FF    // 平假名 / 片假名
        | 0xAC00..=0xD7AF    // 谚文音节
        | 0x20000..=0x2FA1F  // CJK 扩展 B~F / 兼容补充
    )
}

/// 索引前 / 查询前的 CJK 预分词：相邻两个 CJK 字符之间插一个空格。
///
/// 英文与数字原样保留（`unicode61` 本来就会按空白与标点切词）。
#[must_use]
pub fn tokenize_cjk(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut previous_cjk = false;
    for ch in text.chars() {
        let cjk = is_cjk(ch);
        if cjk && previous_cjk {
            out.push(' ');
        }
        out.push(ch);
        previous_cjk = cjk;
    }
    out
}

/// [`tokenize_cjk`] 的逆操作：去掉**夹在两个 CJK 字符之间**的那个空格。
///
/// 只处理「两边都是 CJK」的情形，所以 `redis 缓存` 这类正常的
/// 中英混排空格不会被吃掉。
#[must_use]
pub fn despace_cjk(text: &str) -> String {
    if !text.contains(' ') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    // 待决空格：先记下，看到下一个字符才知道该留还是该丢。
    let mut pending_space = false;
    let mut last_cjk = false;
    for ch in text.chars() {
        if ch == ' ' {
            pending_space = true;
            continue;
        }
        if pending_space {
            let cjk = is_cjk(ch);
            if !(last_cjk && cjk) {
                out.push(' ');
            }
            pending_space = false;
        }
        out.push(ch);
        last_cjk = is_cjk(ch);
    }
    if pending_space {
        out.push(' ');
    }
    out
}

/// 把一个用户查询拆成检索用的词条（已还原、已去重、顺序保留）。
///
/// CJK 逐字；ASCII 按连续字母数字成词（长度 ≥ 2 才保留，单字母噪声太大）。
#[must_use]
pub fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut ascii = String::new();
    let flush = |ascii: &mut String, terms: &mut Vec<String>| {
        if ascii.chars().count() >= 2 {
            let word = ascii.to_lowercase();
            if !terms.contains(&word) {
                terms.push(word);
            }
        }
        ascii.clear();
    };
    for ch in query.chars() {
        if is_cjk(ch) {
            flush(&mut ascii, &mut terms);
            let term = ch.to_string();
            if !terms.contains(&term) {
                terms.push(term);
            }
        } else if ch.is_alphanumeric() {
            ascii.push(ch);
        } else {
            flush(&mut ascii, &mut terms);
        }
    }
    flush(&mut ascii, &mut terms);
    terms
}

/// 把查询编成 FTS5 的 `MATCH` 表达式。
///
/// **每个词条都用双引号包住**：FTS5 的查询语法认 `*`、`-`、`^`、`NEAR`、`OR`
/// 这些元字符，用户随口一句「怎么 - 删除」会直接变成语法错误。引号化之后
/// 用户输入永远只能当字面量，注入面为零。空查询返回 `None`（不查，而不是查全部）。
#[must_use]
pub fn build_match(query: &str) -> Option<String> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return None;
    }
    let quoted: Vec<String> = terms
        .iter()
        .map(|term| format!("\"{}\"", term.replace('"', "")))
        .collect();
    Some(quoted.join(" "))
}

/// 一行命中摘要：在**原文**里找第一个词条，取它前后各若干字符。
///
/// 不用 FTS5 的 `snippet()`：中文预分词后它会给每个汉字单独加括号（见模块头）。
#[must_use]
pub fn make_snippet(body: &str, terms: &[String], budget: usize) -> String {
    let chars: Vec<char> = body.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let hit = find_first(&chars, terms).unwrap_or(0);
    let back = budget / 3;
    let start = hit.saturating_sub(back);
    let end = (start + budget).min(chars.len());
    let mut snippet: String = chars[start..end].iter().collect();
    if start > 0 {
        snippet.insert(0, '…');
    }
    if end < chars.len() {
        snippet.push('…');
    }
    // 片段里保留换行会让卡片排版散架：压成单行。
    snippet.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 找出第一个词条出现的位置（ASCII 大小写不敏感）。返回字符下标。
fn find_first(chars: &[char], terms: &[String]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for term in terms {
        let needle: Vec<char> = term.chars().collect();
        if needle.is_empty() || needle.len() > chars.len() {
            continue;
        }
        for start in 0..=(chars.len() - needle.len()) {
            let matched = needle
                .iter()
                .enumerate()
                .all(|(offset, want)| chars[start + offset].to_lowercase().eq(want.to_lowercase()));
            if matched {
                best = Some(best.map_or(start, |current| current.min(start)));
                break;
            }
        }
    }
    best
}

/// 按 Markdown 标题切块；无标题时按 [`MAX_CHUNK_CHARS`] 定长切。
#[must_use]
pub fn chunk_text(text: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut body = String::new();

    for line in text.lines() {
        if let Some((level, title)) = parse_heading(line) {
            push_chunk(&mut chunks, &stack, &mut body);
            while stack.last().is_some_and(|(depth, _)| *depth >= level) {
                stack.pop();
            }
            stack.push((level, title));
            continue;
        }
        body.push_str(line);
        body.push('\n');
        // 超长正文（压缩过的 JSON、没有换行的日志）在这里断开，避免出现巨型块。
        if body.chars().count() >= MAX_CHUNK_CHARS {
            push_chunk(&mut chunks, &stack, &mut body);
        }
    }
    push_chunk(&mut chunks, &stack, &mut body);
    chunks
}

/// 识别 `# 标题` 形式的行，返回（层级，标题）。
fn parse_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let title = trimmed[level..].trim();
    if title.is_empty() {
        return None;
    }
    Some((level, title.to_owned()))
}

/// 把累积的正文按 [`MAX_CHUNK_CHARS`] 切开落进 `chunks`，标题取当前标题栈。
fn push_chunk(chunks: &mut Vec<Chunk>, stack: &[(usize, String)], body: &mut String) {
    let trimmed = body.trim();
    if !trimmed.is_empty() {
        let heading = stack
            .iter()
            .map(|(_, title)| title.as_str())
            .collect::<Vec<_>>()
            .join(" / ");
        let chars: Vec<char> = trimmed.chars().collect();
        for slice in chars.chunks(MAX_CHUNK_CHARS) {
            chunks.push(Chunk {
                heading: heading.clone(),
                body: slice.iter().collect(),
            });
        }
    }
    body.clear();
}

/// 扫描到的一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScannedFile {
    path: PathBuf,
    mtime: i64,
    size: u64,
}

/// 只走 metadata 递归收集可索引文件（不读内容）。
fn collect_files(root: &Path, cancel: &AtomicBool) -> Vec<ScannedFile> {
    let mut out = Vec::new();
    walk(root, 0, cancel, &mut out);
    out.sort_by(|left, right| left.path.cmp(&right.path));
    out
}

fn walk(path: &Path, depth: usize, cancel: &AtomicBool, out: &mut Vec<ScannedFile>) {
    if cancel.load(Ordering::Relaxed) || out.len() >= MAX_FILES || depth > MAX_DEPTH {
        return;
    }
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.is_file() {
        if is_indexable(path) && metadata.len() <= MAX_FILE_BYTES {
            out.push(ScannedFile {
                path: path.to_path_buf(),
                mtime: file_mtime(&metadata),
                size: metadata.len(),
            });
        }
        return;
    }
    if !metadata.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        if cancel.load(Ordering::Relaxed) || out.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let child = entry.path();
        if child.is_dir() && SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        walk(&child, depth + 1, cancel, out);
    }
}

/// 扩展名是否在白名单内。
fn is_indexable(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return false;
    };
    TEXT_EXTENSIONS.contains(&extension.to_lowercase().as_str())
}

fn file_mtime(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0)
}

/// 文件清单的指纹：路径 + mtime + 大小三者一起哈希。
///
/// 只看目录 mtime 是不够的——**深层文件改动不会更新上层目录的 mtime**
/// （POSIX 只更新直接父目录），那样「改了子目录里的文档却索引不动」。
/// 只哈希 metadata，所以指纹计算本身不读文件内容，代价是若干次 `stat`。
///
/// ⚠️ `DefaultHasher` 的取值不保证跨 Rust 版本稳定；版本一变指纹就不匹配，
/// 后果只是**多重建一次索引**，没有正确性风险，因此不引第三方哈希。
fn fingerprint_of(files: &[ScannedFile]) -> String {
    let mut hasher = DefaultHasher::new();
    for file in files {
        file.path.to_string_lossy().hash(&mut hasher);
        file.mtime.hash(&mut hasher);
        file.size.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_tokenization_splits_han_characters() {
        assert_eq!(tokenize_cjk("数据库"), "数 据 库");
        // 英文词不动，中文逐字，混排只在需要的地方插空格。
        assert_eq!(tokenize_cjk("redis 缓存穿透"), "redis 缓 存 穿 透");
    }

    #[test]
    fn despace_restores_the_original_text() {
        for original in ["数据库连接失败", "redis 缓存穿透", "纯英文没有空格"] {
            assert_eq!(despace_cjk(&tokenize_cjk(original)), original);
        }
    }

    #[test]
    fn despace_keeps_the_space_between_ascii_and_han() {
        // 中英之间的空格是原文本就有的语义，不能被吃掉。
        assert_eq!(despace_cjk("abc 数据"), "abc 数据");
    }

    #[test]
    fn query_terms_keep_han_chars_and_drop_single_letters() {
        assert_eq!(
            query_terms("数据库 redis a"),
            vec!["数", "据", "库", "redis"]
        );
        // 重复字只留一个，避免同一条命中被重复计权。
        assert_eq!(query_terms("连接连接"), vec!["连", "接"]);
    }

    #[test]
    fn match_expression_is_quoted_against_injection() {
        let expression = build_match("连接* OR 删除").unwrap();
        // `*` 与 `OR` 只是普通字符，不会变成 FTS5 语法。
        assert!(expression.starts_with('"'));
        assert!(!expression.contains("OR "));
        assert!(build_match("   ").is_none());
    }

    #[test]
    fn match_expression_never_leaks_a_bare_quote() {
        // 双引号被剥掉，不会提前闭合引号串。
        let expression = build_match("he said \"hi\"").unwrap();
        assert_eq!(expression.matches('"').count() % 2, 0);
    }

    #[test]
    fn chunking_tracks_the_heading_path() {
        let source = "# 一级\n正文甲\n## 二级\n正文乙\n### 三级\n正文丙\n";
        let chunks = chunk_text(source);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].heading, "一级");
        assert!(chunks[0].body.contains("正文甲"));
        assert_eq!(chunks[1].heading, "一级 / 二级");
        assert_eq!(chunks[2].heading, "一级 / 二级 / 三级");
    }

    #[test]
    fn chunking_splits_oversized_bodies() {
        let source = "字".repeat(MAX_CHUNK_CHARS + 100);
        let chunks = chunk_text(&source);
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.body.chars().count() <= MAX_CHUNK_CHARS)
        );
    }

    #[test]
    fn snippet_centres_on_the_first_hit() {
        let body = format!("{}连接失败{}", "甲".repeat(500), "乙".repeat(500));
        let snippet = make_snippet(&body, &["连".to_owned(), "接".to_owned()], 100);
        assert!(snippet.contains("连接失败"));
        assert!(snippet.starts_with('…'));
        // 按**字符**数断言：汉字一字三字节，用 `len()` 会得出三倍假结论。
        assert!(
            snippet.chars().count() <= 104,
            "{}",
            snippet.chars().count()
        );
    }

    #[test]
    fn snippet_without_a_hit_falls_back_to_the_head() {
        let snippet = make_snippet("hello world", &["missing".to_owned()], 40);
        assert_eq!(snippet, "hello world");
    }

    fn seeded_index() -> KbIndex {
        let mut index = KbIndex::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("duck-kb-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("guide.md"),
            "# 连接\n数据库连接失败时先看网络。\n# 缓存\nredis 缓存穿透要加布隆过滤器。\n",
        )
        .unwrap();
        let source = KbSource::new(&dir);
        index
            .index_source(&source, &AtomicBool::new(false), &mut |_| {})
            .unwrap();
        index
    }

    #[test]
    fn search_finds_chinese_substrings() {
        // 这是本模块存在的理由：默认分词器下这条断言会失败（见模块头实测）。
        let index = seeded_index();
        let hits = index.search("连接失败", 5).unwrap();
        assert!(!hits.is_empty(), "中文分词没生效");
        assert!(hits[0].snippet.contains("数据库连接失败"));
        assert!(hits[0].heading.contains("连接"));
    }

    #[test]
    fn search_finds_ascii_words_case_insensitively() {
        let index = seeded_index();
        let hits = index.search("Redis", 5).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].snippet.contains("redis"));
    }

    #[test]
    fn empty_query_returns_nothing_instead_of_everything() {
        let index = seeded_index();
        assert!(index.search("   ", 5).unwrap().is_empty());
    }

    #[test]
    fn incremental_index_skips_unchanged_sources() {
        let mut index = KbIndex::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("duck-kb-inc-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "# 主题\n内容内容\n").unwrap();
        let source = KbSource::new(&dir);
        let cancel = AtomicBool::new(false);

        let first = index.index_source(&source, &cancel, &mut |_| {}).unwrap();
        assert!(!first.skipped);
        assert!(first.chunks > 0);

        let second = index.index_source(&source, &cancel, &mut |_| {}).unwrap();
        assert!(second.skipped, "指纹没变却重扫了");
        // 跳过不等于重复写入：块数不能翻倍。
        assert_eq!(index.stats().unwrap().chunks, first.chunks);
    }

    #[test]
    fn removing_a_source_drops_its_chunks() {
        let index = seeded_index();
        let sources = index.sources().unwrap();
        assert_eq!(sources.len(), 1);
        index.remove_source(&sources[0].id).unwrap();
        assert_eq!(index.stats().unwrap().chunks, 0);
        assert!(index.search("连接失败", 5).unwrap().is_empty());
    }

    #[test]
    fn indexing_an_empty_directory_is_an_error_not_a_silent_success() {
        let mut index = KbIndex::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("duck-kb-empty-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let outcome =
            index.index_source(&KbSource::new(&dir), &AtomicBool::new(false), &mut |_| {});
        assert!(outcome.is_err());
    }

    #[test]
    fn origin_line_mentions_the_file() {
        let hit = KbHit {
            source_id: "s".to_owned(),
            label: "文档".to_owned(),
            path: "/tmp/docs/ai-design.md".to_owned(),
            heading: "工具系统".to_owned(),
            snippet: String::new(),
            score: -1.0,
        };
        assert_eq!(hit.origin(), "文档 · ai-design.md · 工具系统");
    }

    #[test]
    fn heading_only_sources_still_index_the_body() {
        let chunks = chunk_text("没有标题的一段话。\n第二行。");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].heading.is_empty());
        assert!(chunks[0].body.contains("第二行"));
    }
}
