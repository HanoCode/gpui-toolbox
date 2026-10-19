//! 批量改写：**改名（rename）与内容替换（replace）共用一套「预览 → 确认 → 应用 → 撤销」**。
//!
//! 内容替换的匹配与执行逐字复刻参考仓 `batch_replace.rs`（`build_regex` /
//! `expand_group_refs` / `locate_matches` / `read_file_text` / `preview` / `execute`），
//! 包括两条容易写错的细节：
//!
//! 1. **非正则模式下 `$` 要转义成 `$$`**：用户输入 `$5` 是想写字面量，不是分组引用；
//! 2. **正则模式下 `$1` 要补成 `${1}`**：`$1a` 会被正则引擎读成组名 `1a`，
//!    结果整段替换成空 —— 参考仓为此专门写了 `expand_group_refs`。
//!
//! 改名规则来自前端 `BatchRenamer.vue`（纯前端算法，无后端可比，按前端逻辑移植）。
//!
//! # 撤销：参考仓没有，这里补上
//!
//! 参考仓只给「写 `.bak`」这一个选项，撤不撤销得用户自己去文件管理器改回来；
//! 而批量操作一旦上百个文件，`.bak` 反而更乱。这里改成**撤销日志**：
//!
//! - 每次应用都在撤销目录下建一个 `<时间戳>-<序号>/`，里面是 `journal.json` + 原始字节；
//! - 「撤销」＝把原件写回（替换）或逆向改名（改名），成功后删掉那一条日志；
//! - 只保留最近 [`MAX_UNDO`] 条，多的按时间淘汰（磁盘不会无限长）。
//!
//! ⚠️ 插件进程**一次调用一次进程**，所以撤销必须落盘——这也意味着「撤销」是
//! **跨运行**可用的（今天改的，明天还能撤），比内存栈更符合批量工具的实际使用。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ErrorKind, FilesError, FilesResult};
use crate::walk::WalkEntry;

/// 保留的撤销记录条数。
pub const MAX_UNDO: usize = 20;

/// 改写模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchMode {
    /// 改文件名。
    Rename,
    /// 改文件内容。
    Replace,
}

// ── 内容替换（复刻 batch_replace.rs）─────────────────────────────────────

/// 替换规则。
#[derive(Debug, Clone)]
pub struct ReplaceRule {
    pub pattern: String,
    pub replacement: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
}

/// 一处命中（预览用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewMatch {
    pub line: usize,
    pub column: usize,
    /// 命中所在整行（trim 过）。
    pub context: String,
}

/// 单个文件的预览结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplacePreview {
    pub path: String,
    pub match_count: usize,
    pub matches: Vec<PreviewMatch>,
    /// 读文件失败时这里有值，其余字段为空（参考仓口径：**不中断整批**）。
    pub error: Option<String>,
}

/// 单个文件的执行结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplaceResult {
    pub path: String,
    pub ok: bool,
    pub replaced_count: usize,
    pub error: Option<String>,
}

/// 预览每个文件的命中数与前若干处位置。
///
/// 每个文件默认只报前 5 处（参考仓 `locate_matches(text, regex, 5)`）：预览是给人
/// 扫一眼的，一个 3 000 行的日志里 800 处命中全列出来只会让人滚半天。
pub fn preview_replace(
    paths: &[PathBuf],
    rule: &ReplaceRule,
    per_file: usize,
) -> FilesResult<Vec<ReplacePreview>> {
    let regex = build_regex(&rule.pattern, rule.is_regex, rule.case_sensitive)?;
    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        let display = path.display().to_string();
        let text = match read_file_text(&display) {
            Ok(text) => text,
            Err(error) => {
                results.push(ReplacePreview {
                    path: display,
                    match_count: 0,
                    matches: Vec::new(),
                    error: Some(error.to_string()),
                });
                continue;
            }
        };
        let match_count = regex.find_iter(&text).count();
        results.push(ReplacePreview {
            path: display,
            match_count,
            matches: locate_matches(&text, &regex, per_file),
            error: None,
        });
    }
    Ok(results)
}

/// 应用替换。命中 0 处的文件算成功（`replaced_count = 0`），不算失败。
///
/// `undo_dir` 为 `None` 时不建撤销记录（只写 `.bak` 也不做——那是参考仓的开关，
/// 这里由「要不要撤销」统一表达：给了目录就有撤销，没给就没有）。
pub fn apply_replace(
    paths: &[PathBuf],
    rule: &ReplaceRule,
    undo_dir: Option<&Path>,
) -> FilesResult<(Vec<ReplaceResult>, Option<String>)> {
    let regex = build_regex(&rule.pattern, rule.is_regex, rule.case_sensitive)?;
    let replacement = if rule.is_regex {
        expand_group_refs(&rule.replacement)
    } else {
        rule.replacement.replace('$', "$$")
    };

    let mut journal = undo_dir.map(|dir| UndoJournal::begin(dir, UndoKind::Replace));
    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        let display = path.display().to_string();
        let text = match read_file_text(&display) {
            Ok(text) => text,
            Err(error) => {
                results.push(ReplaceResult {
                    path: display,
                    ok: false,
                    replaced_count: 0,
                    error: Some(error.to_string()),
                });
                continue;
            }
        };
        let count = regex.find_iter(&text).count();
        if count == 0 {
            results.push(ReplaceResult {
                path: display,
                ok: true,
                replaced_count: 0,
                error: None,
            });
            continue;
        }
        if let Some(error) = stash_text(journal.as_mut(), path, &text) {
            results.push(ReplaceResult {
                path: display,
                replaced_count: 0,
                ok: false,
                error: Some(error),
            });
            continue;
        }
        let new_text = regex.replace_all(&text, &replacement).to_string();
        match std::fs::write(path, &new_text) {
            Ok(()) => results.push(ReplaceResult {
                path: display,
                ok: true,
                replaced_count: count,
                error: None,
            }),
            Err(error) => results.push(ReplaceResult {
                path: display,
                ok: false,
                replaced_count: 0,
                error: Some(format!("write: {error}")),
            }),
        }
    }
    let undo_id = journal.and_then(|journal| journal.commit().ok());
    Ok((results, undo_id))
}

/// 要撤销就先存一份原文；返回 `Some(错误)` 表示**存不下**，此时绝不能改文件。
///
/// 拆成独立函数是因为「有撤销目录」与「存失败」是两层判断，直接嵌套 `if let`
/// 会触发 `clippy::collapsible_if`，而它给的合并写法 `if a && let Some(x) = b`
/// 需要 `let_chains`（Rust 1.88），本仓 MSRV 是 1.85——只能把两层压成一次 `if let`。
fn stash_text(journal: Option<&mut UndoJournal>, path: &Path, text: &str) -> Option<String> {
    journal?
        .stash(path, text.as_bytes())
        .err()
        .map(|error| error.to_string())
}

/// 与参考仓 `build_regex` 等价。
fn build_regex(pattern: &str, is_regex: bool, case_sensitive: bool) -> FilesResult<regex::Regex> {
    let source = if is_regex {
        pattern.to_string()
    } else {
        regex::escape(pattern)
    };
    regex::RegexBuilder::new(&source)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|error| FilesError::with_detail(ErrorKind::BadPattern, error.to_string()))
}

/// 把 `$1` 显式转成 `${1}`，避免组引用后面紧跟字母时被当成组名。
///
/// 参考仓原注释：「避免捕获组引用后紧跟字母时被误解析为组名」。
fn expand_group_refs(replacement: &str) -> String {
    let matcher = regex::Regex::new(r"\$(\d+)").expect("valid regex");
    let mut out = String::with_capacity(replacement.len() + 4);
    let mut last = 0;
    for found in matcher.find_iter(replacement) {
        out.push_str(&replacement[last..found.start()]);
        out.push_str("${");
        out.push_str(&found.as_str()[1..]);
        out.push('}');
        last = found.end();
    }
    out.push_str(&replacement[last..]);
    out
}

/// 命中位置（行 / 列 / 所在行）。
fn locate_matches(text: &str, regex: &regex::Regex, limit: usize) -> Vec<PreviewMatch> {
    let mut matches = Vec::new();
    for found in regex.find_iter(text) {
        let start = found.start();
        let line = text[..start].matches('\n').count() + 1;
        let line_start = text[..start]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let column = text[line_start..start].chars().count() + 1;
        let line_end = text[start..]
            .find('\n')
            .map(|index| start + index)
            .unwrap_or(text.len());
        matches.push(PreviewMatch {
            line,
            column,
            context: text[line_start..line_end].trim().to_string(),
        });
        if matches.len() >= limit {
            break;
        }
    }
    matches
}

/// 按 UTF-8 读文本；不是 UTF-8 就报错（参考仓口径：不做编码嗅探）。
fn read_file_text(path: &str) -> FilesResult<String> {
    crate::error::validate_path(path)?;
    let bytes = std::fs::read(path).map_err(FilesError::io)?;
    String::from_utf8(bytes).map_err(|_| FilesError::new(ErrorKind::NotUtf8))
}

// ── 批量改名 ────────────────────────────────────────────────────────────

/// 改名规则（移植 `BatchRenamer.vue` 的 `applyRename`）。
///
/// 参考仓的规则比「正则替换」多三样东西，缺一个都会让用户觉得「这工具改名不听话」：
///
/// 1. **只改主名**，扩展名原样保留（`.tar.gz` 只认最后一个点，`archive.tar` 才是主名）；
/// 2. **前缀 / 后缀**：批量改名最常用的其实是「加个日期前缀」，不是正则；
/// 3. **占位符** `{name}` / `{index}` / `{ext}`：`{index}` 补零到 3 位，排序不会乱。
#[derive(Debug, Clone)]
pub struct RenameRule {
    /// 查找内容（空 = 不做查找替换）。
    pub pattern: String,
    /// 替换内容（正则模式下支持 `$1`）。
    pub replacement: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
    /// 主名前缀。
    pub prefix: String,
    /// 主名后缀。
    pub suffix: String,
}

/// 预览里的一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenamePlanRow {
    /// 原名（相对目录）。
    pub from: PathBuf,
    /// 新名（相对目录）。
    pub to: PathBuf,
    /// 这一行能不能应用。
    pub ok: bool,
    /// 不能应用的原因码（`bad_name` / `duplicate_target` / `target_exists`）。
    pub reason: Option<&'static str>,
}

/// 按参考仓规则算出一个新文件名。
///
/// `index` 是这一批里的序号（0 起），只给 `{index}` 用。
fn rename_one(name: &str, index: usize, rule: &RenameRule) -> String {
    // `lastIndexOf('.') > 0`：`.bashrc` 这种只有前导点的名字整串都是主名。
    let dot = name.rfind('.').filter(|position| *position > 0);
    let (stem, ext) = match dot {
        Some(position) => (&name[..position], &name[position..]),
        None => (name, ""),
    };

    let mut base = stem.to_string();
    // 模式为空 / 写错了 → 只加前缀后缀、不改主名（参考仓 `if (this.pattern)` 同义）。
    // 空串必须挡掉：`Regex::new("")` 是能编过的，替换下去会把主名变成「替换串 + 原名」。
    match build_regex(&rule.pattern, rule.is_regex, rule.case_sensitive) {
        Ok(regex) if !rule.pattern.is_empty() => {
            let replacement = if rule.is_regex {
                expand_group_refs(&rule.replacement)
            } else {
                rule.replacement.replace('$', "$$")
            };
            base = regex.replace_all(&base, &replacement).to_string();
        }
        _ => {}
    }

    let serial = format!("{:03}", index + 1);
    // ⚠️ 与前端的一处**有意的**差异：参考仓只在主名上展开占位符，`前缀/suffix` 里写的
    // `{index}` 会原样留下（静默失效）。这里等整串拼好再展开一次，三种位置都生效——
    // 「后缀加序号」是批量改名最常见的用法，让它失效没有道理。
    let composed = format!("{}{base}{}{ext}", rule.prefix, rule.suffix);
    composed
        .replace("{name}", stem)
        .replace("{index}", &serial)
        .replace("{ext}", ext)
}

/// 计算改名计划：**只算，不动盘**。
///
/// 三类冲突全部拦在计划里（应用时再撞上就晚了）：
/// 1. 新名字为空 / 含路径分隔符（改名不可能跨目录）；
/// 2. 同一批里两行改到同一个名字；
/// 3. 目标文件已存在，且它自己不在这一批里。
pub fn plan_rename(
    root: &Path,
    entries: &[WalkEntry],
    rule: &RenameRule,
) -> FilesResult<Vec<RenamePlanRow>> {
    // 规则里的正则写错时**整批拒绝**：半批改名比不改更糟。
    if !rule.pattern.is_empty() {
        build_regex(&rule.pattern, rule.is_regex, rule.case_sensitive)?;
    }

    let mut planned: Vec<RenamePlanRow> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let name = entry.name();
        let new_name = rename_one(&name, index, rule);
        let mut row = RenamePlanRow {
            from: entry.rel.clone(),
            to: entry.rel.with_file_name(&new_name),
            ok: true,
            reason: None,
        };
        if new_name.trim().is_empty() || new_name.contains('/') || new_name.contains('\\') {
            row.ok = false;
            row.reason = Some("bad_name");
        } else if new_name == name {
            // 规则没命中：不报错，也不进应用清单（`from == to`）。
            row.to = entry.rel.clone();
        }
        planned.push(row);
    }

    // 冲突 2：批内重名。
    let mut seen: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    for row in &planned {
        if row.from != row.to {
            *seen.entry(row.to.clone()).or_insert(0) += 1;
        }
    }
    for row in &mut planned {
        if row.ok && row.from != row.to && seen.get(&row.to).copied().unwrap_or(0) > 1 {
            row.ok = false;
            row.reason = Some("duplicate_target");
        }
    }

    // 冲突 3：目标已存在（同一批里会被改走的那个不算冲突）。
    let moving: std::collections::HashSet<PathBuf> =
        planned.iter().map(|row| root.join(&row.from)).collect();
    for row in &mut planned {
        if !row.ok || row.from == row.to {
            continue;
        }
        let target = root.join(&row.to);
        let source = root.join(&row.from);
        if !target.exists() {
            continue;
        }
        // macOS / Windows 的文件系统大小写不敏感：`a.txt → A.txt` 会让 `exists()` 为真，
        // 但它其实是同一个文件，属于「只改大小写」的合法改名。用 canonicalize 比对
        // 真实身份，比按平台 `cfg` 分支更准（Linux 上大小写不同就是两个文件）。
        let same_file = match (
            std::fs::canonicalize(&target),
            std::fs::canonicalize(&source),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !same_file && !moving.contains(&target) {
            row.ok = false;
            row.reason = Some("target_exists");
        }
    }
    Ok(planned)
}

/// 应用结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub applied: usize,
    /// `(路径, 错误码)`：失败不中断整批。
    pub failed: Vec<(String, String)>,
    /// 撤销记录 id（没有记录时为 `None`）。
    pub undo_id: Option<String>,
}

/// 应用改名计划。
///
/// **两阶段**：先把每个源文件改成临时名，再从临时名改成目标名。这样 `a→b` 与
/// `b→c` 这种「链式改名」不会因为顺序问题互相覆盖（单阶段实现必须做拓扑排序，
/// 而且环（`a→b, b→a`）根本排不出来；两阶段天然没有这个问题）。
pub fn apply_rename(
    root: &Path,
    plan: &[RenamePlanRow],
    undo_dir: Option<&Path>,
) -> FilesResult<ApplyOutcome> {
    let mut outcome = ApplyOutcome::default();
    let actionable: Vec<&RenamePlanRow> = plan
        .iter()
        .filter(|row| row.ok && row.from != row.to)
        .collect();
    if actionable.is_empty() {
        return Ok(outcome);
    }

    let mut journal = undo_dir.map(|dir| UndoJournal::begin(dir, UndoKind::Rename));
    // 阶段一：源 → 临时名。临时名带前缀 `.duck-tmp-`，避开用户文件与隐藏项规则。
    let mut staged: Vec<(&RenamePlanRow, PathBuf)> = Vec::with_capacity(actionable.len());
    for (index, row) in actionable.iter().enumerate() {
        let from = root.join(&row.from);
        let temp = root.join(format!(
            ".duck-tmp-{index}-{}",
            row.from
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        match std::fs::rename(&from, &temp) {
            Ok(()) => staged.push((row, temp)),
            Err(error) => outcome
                .failed
                .push((row.from.display().to_string(), error.to_string())),
        }
    }
    // 阶段二：临时名 → 目标名。失败的回滚回原名（能回就回，回不了就报出来）。
    for (row, temp) in staged {
        let to = root.join(&row.to);
        match std::fs::rename(&temp, &to) {
            Ok(()) => {
                outcome.applied += 1;
                if let Some(journal) = journal.as_mut() {
                    journal.record_move(&root.join(&row.from), &root.join(&row.to));
                }
            }
            Err(error) => {
                let _ = std::fs::rename(&temp, root.join(&row.from));
                outcome
                    .failed
                    .push((row.to.display().to_string(), error.to_string()));
            }
        }
    }
    outcome.undo_id = journal.and_then(|journal| journal.commit().ok());
    Ok(outcome)
}

// ── 撤销日志 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum UndoKind {
    Replace,
    Rename,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MoveRecord {
    from: String,
    to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupRecord {
    path: String,
    /// 备份文件名（相对日志目录）。
    backup: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    id: String,
    kind: UndoKind,
    created_at: u64,
    #[serde(default)]
    files: Vec<BackupRecord>,
    #[serde(default)]
    moves: Vec<MoveRecord>,
}

/// 一条待撤销记录（列表用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UndoEntryInfo {
    pub id: String,
    /// `replace` / `rename`。
    pub kind: String,
    pub created_at: u64,
    /// 涉及的文件数。
    pub items: usize,
}

/// 撤销结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UndoReport {
    pub id: String,
    pub kind: String,
    pub restored: usize,
    /// `(路径, 错误)`。
    pub failed: Vec<(String, String)>,
}

/// 撤销期间的日志句柄。
struct UndoJournal {
    dir: PathBuf,
    journal: Journal,
    committed: bool,
}

impl UndoJournal {
    fn begin(root: &Path, kind: UndoKind) -> Self {
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        // id 里带序号：同一秒内连点两次「应用」也不会撞目录名。
        let mut sequence = 0u32;
        let dir = loop {
            let candidate = root.join(format!("{created_at}-{sequence:02}"));
            if !candidate.exists() {
                break candidate;
            }
            sequence += 1;
        };
        let _ = std::fs::create_dir_all(&dir);
        let id = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            dir,
            journal: Journal {
                id,
                kind,
                created_at,
                files: Vec::new(),
                moves: Vec::new(),
            },
            committed: false,
        }
    }

    fn stash(&mut self, path: &Path, bytes: &[u8]) -> FilesResult<()> {
        let name = format!("{:04}.bin", self.journal.files.len());
        std::fs::write(self.dir.join(&name), bytes).map_err(FilesError::io)?;
        self.journal.files.push(BackupRecord {
            path: path.display().to_string(),
            backup: name,
        });
        Ok(())
    }

    /// 记录一对改名。⚠️ 必须存**绝对路径**：撤销会在下一次运行（另一个进程、
    /// 另一个工作目录）里发生，相对路径那时候指到哪里就说不清了。
    fn record_move(&mut self, from: &Path, to: &Path) {
        self.journal.moves.push(MoveRecord {
            from: from.display().to_string(),
            to: to.display().to_string(),
        });
    }

    /// 落 `journal.json` 并返回 id；什么都没记就删掉空目录（不留噪声）。
    fn commit(mut self) -> FilesResult<String> {
        self.committed = true;
        if self.journal.files.is_empty() && self.journal.moves.is_empty() {
            let _ = std::fs::remove_dir_all(&self.dir);
            return Ok(self.journal.id.clone());
        }
        let json = serde_json::to_string_pretty(&self.journal)
            .map_err(|error| FilesError::with_detail(ErrorKind::Io, error.to_string()))?;
        std::fs::write(self.dir.join("journal.json"), json).map_err(FilesError::io)?;
        let _ = prune(self.dir.parent().unwrap_or(Path::new(".")));
        Ok(self.journal.id.clone())
    }
}

impl Drop for UndoJournal {
    /// 半途而废（`?` 提前返回）时别留半截目录：没有 `journal.json` 的记录本来也
    /// 撤不了，留着只会让「撤销」列表里多出一条点不动的。
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// 只保留最近 [`MAX_UNDO`] 条（按目录名里的时间戳排序，不读文件）。
fn prune(root: &Path) -> FilesResult<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(FilesError::io)?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect();
    if entries.len() <= MAX_UNDO {
        return Ok(());
    }
    entries.sort_by(|a, b| {
        let key = |path: &PathBuf| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        };
        key(a).cmp(&key(b))
    });
    for stale in &entries[..entries.len() - MAX_UNDO] {
        let _ = std::fs::remove_dir_all(stale);
    }
    Ok(())
}

fn read_journal(dir: &Path) -> FilesResult<Journal> {
    let text = std::fs::read_to_string(dir.join("journal.json")).map_err(|_| {
        FilesError::with_detail(
            ErrorKind::UndoUnavailable,
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    })?;
    serde_json::from_str(&text)
        .map_err(|error| FilesError::with_detail(ErrorKind::UndoUnavailable, error.to_string()))
}

/// 列出可撤销的记录（新的在前）。
pub fn list_undo(dir: &Path) -> FilesResult<Vec<UndoEntryInfo>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<UndoEntryInfo> = std::fs::read_dir(dir)
        .map_err(FilesError::io)?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| read_journal(&entry.path()).ok())
        .map(|journal| UndoEntryInfo {
            id: journal.id,
            kind: match journal.kind {
                UndoKind::Replace => "replace".to_string(),
                UndoKind::Rename => "rename".to_string(),
            },
            created_at: journal.created_at,
            items: journal.files.len() + journal.moves.len(),
        })
        .collect();
    entries.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(entries)
}

/// 撤销最近一条记录。
pub fn undo_last(dir: &Path) -> FilesResult<UndoReport> {
    let newest = list_undo(dir)?.into_iter().next().ok_or_else(|| {
        FilesError::with_detail(ErrorKind::UndoUnavailable, dir.display().to_string())
    })?;
    let path = dir.join(&newest.id);
    let journal = read_journal(&path)?;
    let mut report = UndoReport {
        id: journal.id.clone(),
        kind: newest.kind.clone(),
        restored: 0,
        failed: Vec::new(),
    };

    match journal.kind {
        UndoKind::Replace => {
            for record in &journal.files {
                let bytes = match std::fs::read(path.join(&record.backup)) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        report.failed.push((record.path.clone(), error.to_string()));
                        continue;
                    }
                };
                match std::fs::write(&record.path, bytes) {
                    Ok(()) => report.restored += 1,
                    Err(error) => report.failed.push((record.path.clone(), error.to_string())),
                }
            }
        }
        UndoKind::Rename => {
            // 与正向一样分两阶段：先全部退到临时名，再落回原名。
            //
            // 直接逆序 `to → from` 在链式改名上会丢数据：正向 `a→b, b→c` 之后
            // `b` 里装的是 a 的内容，逆序第一步 `c→b` 就把它覆盖掉了。两阶段
            // 天然没有这个问题（也顺带解决了 `a↔b` 这种环）。
            let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
            for (index, record) in journal.moves.iter().enumerate().rev() {
                let from = PathBuf::from(&record.to);
                let temp = from.with_file_name(format!(
                    ".duck-undo-{index}-{}",
                    from.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ));
                match std::fs::rename(&from, &temp) {
                    Ok(()) => staged.push((temp, PathBuf::from(&record.from))),
                    Err(error) => report.failed.push((record.to.clone(), error.to_string())),
                }
            }
            for (temp, target) in staged {
                match std::fs::rename(&temp, &target) {
                    Ok(()) => report.restored += 1,
                    Err(error) => report
                        .failed
                        .push((target.display().to_string(), error.to_string())),
                }
            }
        }
    }

    // 只删成功的记录：有失败项就留着，用户可以修好路径再撤一次。
    if report.failed.is_empty() {
        let _ = std::fs::remove_dir_all(&path);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{temp_dir, write};

    fn rule(pattern: &str, replacement: &str) -> ReplaceRule {
        ReplaceRule {
            pattern: pattern.to_string(),
            replacement: replacement.to_string(),
            is_regex: false,
            case_sensitive: true,
        }
    }

    #[test]
    fn preview_counts_and_locate_matches() {
        let root = temp_dir("batch-preview");
        let file = write(&root, "a.txt", "alpha beta\nbeta beta\n");
        let preview =
            preview_replace(std::slice::from_ref(&file), &rule("beta", "B"), 2).expect("preview");
        assert_eq!(preview[0].match_count, 3);
        assert_eq!(preview[0].matches.len(), 2, "per_file 上限生效");
        assert_eq!(preview[0].matches[0].line, 1);
        assert_eq!(preview[0].matches[0].column, 7);
        assert_eq!(preview[0].matches[0].context, "alpha beta");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn non_regex_mode_escapes_metacharacters_and_dollar() {
        let root = temp_dir("batch-literal");
        let file = write(&root, "a.txt", "a.b $1\n");
        let replaced = apply_replace(&[file], &rule("a.b", "$1"), None).expect("apply");
        assert_eq!(replaced.0[0].replaced_count, 1);
        let text = std::fs::read_to_string(root.join("a.txt")).expect("read");
        // 字面量 `$1` 必须原样写进去，不能被当成「第一组」。
        assert_eq!(text, "$1 $1\n");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn regex_mode_expands_group_refs_before_letters() {
        // `$1x` 会被正则引擎读成组名 `1x`；expand_group_refs 把它补成 `${1}x`。
        assert_eq!(expand_group_refs("$1x"), "${1}x");
        assert_eq!(expand_group_refs("pre$12post"), "pre${12}post");
        assert_eq!(expand_group_refs("no refs"), "no refs");

        let root = temp_dir("batch-group");
        let file = write(&root, "a.txt", "foo=1 bar=2\n");
        let rule = ReplaceRule {
            pattern: r"(\w+)=(\d)".to_string(),
            replacement: "$2:$1;".to_string(),
            is_regex: true,
            case_sensitive: true,
        };
        apply_replace(&[file], &rule, None).expect("apply");
        let text = std::fs::read_to_string(root.join("a.txt")).expect("read");
        assert_eq!(text, "1:foo; 2:bar;\n");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replace_then_undo_restores_original_bytes() {
        let root = temp_dir("batch-undo-replace");
        let file = write(&root, "a.txt", "one one one\n");
        let undo = root.join(".undo");
        let (results, undo_id) =
            apply_replace(std::slice::from_ref(&file), &rule("one", "1"), Some(&undo))
                .expect("apply");
        assert_eq!(results[0].replaced_count, 3);
        assert!(undo_id.is_some());
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "1 1 1\n");

        let report = undo_last(&undo).expect("undo");
        assert_eq!(report.restored, 1);
        assert!(report.failed.is_empty());
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "one one one\n"
        );
        // 撤完这条记录就消失，再点一次撤销没有可撤的东西。
        assert!(list_undo(&undo).expect("list").is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replace_skips_missing_file_without_aborting_the_batch() {
        let root = temp_dir("batch-missing");
        let good = write(&root, "good.txt", "x\n");
        let missing = root.join("missing.txt");
        let (results, _) = apply_replace(&[missing, good], &rule("x", "y"), None).expect("apply");
        assert!(!results[0].ok);
        assert!(results[1].ok);
        let _ = std::fs::remove_dir_all(root);
    }

    fn entry(root: &Path, rel: &str) -> WalkEntry {
        let path = root.join(rel);
        WalkEntry {
            path: path.clone(),
            rel: PathBuf::from(rel),
            is_dir: false,
            size: 0,
        }
    }

    fn rename_rule(pattern: &str, replacement: &str) -> RenameRule {
        RenameRule {
            pattern: pattern.to_string(),
            replacement: replacement.to_string(),
            is_regex: false,
            case_sensitive: true,
            prefix: String::new(),
            suffix: String::new(),
        }
    }

    #[test]
    fn rename_plan_catches_duplicates_and_existing_targets() {
        let root = temp_dir("batch-rename-plan");
        write(&root, "a.txt", "a");
        write(&root, "b.txt", "b");
        write(&root, "taken.txt", "t");

        let rule = rename_rule("a", "A");
        let entries = vec![entry(&root, "a.txt"), entry(&root, "b.txt")];
        let plan = plan_rename(&root, &entries, &rule).expect("plan");
        assert_eq!(plan[0].to, PathBuf::from("A.txt"));
        assert_eq!(plan[1].to, PathBuf::from("b.txt"), "没命中就原地不动");
        assert!(plan.iter().all(|row| row.ok), "{plan:?}");

        // 两个文件改到同一个名字：固定替换、不带 `{index}`。
        let fixed = RenameRule {
            pattern: "a|b".to_string(),
            replacement: "same".to_string(),
            is_regex: true,
            case_sensitive: true,
            prefix: String::new(),
            suffix: String::new(),
        };
        let plan = plan_rename(&root, &entries, &fixed).expect("plan");
        assert_eq!(plan[0].reason, Some("duplicate_target"));

        // 目标已存在，且不在本批里。
        let taken = rename_rule("a", "taken");
        let plan = plan_rename(&root, &[entry(&root, "a.txt")], &taken).expect("plan");
        assert_eq!(plan[0].reason, Some("target_exists"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rename_keeps_extension_and_expands_placeholders() {
        // 只改主名：`archive.tar.gz` 的扩展名是 `.gz`。
        assert_eq!(
            rename_one("archive.tar.gz", 0, &rename_rule("archive", "backup")),
            "backup.tar.gz"
        );
        // 前缀 / 后缀 / 占位符。
        let rule = RenameRule {
            pattern: String::new(),
            replacement: String::new(),
            is_regex: false,
            case_sensitive: true,
            prefix: "20-".to_string(),
            suffix: "-{index}".to_string(),
        };
        assert_eq!(rename_one("report.csv", 0, &rule), "20-report-001.csv");
        assert_eq!(rename_one("report.csv", 41, &rule), "20-report-042.csv");
        // `{name}` 是**原始**主名（替换之后的主名不会再被 `{name}` 取到）。
        let with_name = rename_one(
            "a.csv",
            0,
            &RenameRule {
                pattern: "a".to_string(),
                replacement: "b".to_string(),
                is_regex: false,
                case_sensitive: true,
                prefix: String::new(),
                suffix: "-{name}-{ext}".to_string(),
            },
        );
        assert_eq!(
            with_name, "b-a-.csv.csv",
            "与前端同一口径（占位符在替换之后展开）"
        );
        // 只有前导点的名字整串都算主名。
        assert_eq!(
            rename_one(".bashrc", 0, &rename_rule("bashrc", "zshrc")),
            ".zshrc"
        );
    }

    #[test]
    fn chained_renames_do_not_clobber_each_other() {
        let root = temp_dir("batch-rename-chain");
        write(&root, "a.txt", "a");
        write(&root, "b.txt", "b");
        // a→b 与 b→c：单阶段实现按顺序做会把 a.txt 覆盖掉。
        let entries = vec![entry(&root, "a.txt"), entry(&root, "b.txt")];
        let plan = vec![
            RenamePlanRow {
                from: PathBuf::from("a.txt"),
                to: PathBuf::from("b.txt"),
                ok: true,
                reason: None,
            },
            RenamePlanRow {
                from: PathBuf::from("b.txt"),
                to: PathBuf::from("c.txt"),
                ok: true,
                reason: None,
            },
        ];
        // 手工构造的计划（`plan_rename` 会把 a→b 判成 target_exists，这里测的是两阶段本身）。
        let _ = entries;
        let outcome = apply_rename(&root, &plan, Some(&root.join(".undo"))).expect("apply");
        assert_eq!(outcome.applied, 2);
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).expect("b"), "a");
        assert_eq!(std::fs::read_to_string(root.join("c.txt")).expect("c"), "b");
        assert!(!root.join("a.txt").exists());

        let report = undo_last(&root.join(".undo")).expect("undo");
        assert_eq!(report.restored, 2);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("a"), "a");
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).expect("b"), "b");
        assert!(!root.join("c.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn undo_without_journal_reports_a_typed_error() {
        let root = temp_dir("batch-undo-empty");
        let error = undo_last(&root.join("nope")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UndoUnavailable);
        let _ = std::fs::remove_dir_all(root);
    }
}
