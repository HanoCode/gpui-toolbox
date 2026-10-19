//! SQL 合并（多版本包 → 按分组归并，SPSQL / SPSQL2-ZIP / 标准 三模式）。
//!
//! **本文件逐字复刻**自参考仓 `ifm_ai_tools/src-tauri/src/sql_merge.rs`
//! （2,101 行，已在生产使用中验证）。移植原则见 `docs/ifm-plugin-port-plan.md`
//! 与用户口径「复刻不复研」。改动清单（全部是**接线**，没有一处逻辑）：
//!
//! 1. 去掉 `use tauri::{AppHandle, Emitter}`；三个 `emit_*` 的载荷不变，
//!    出口从 Tauri 广播换成 [`MergeReporter`] 的三个同名方法。
//! 2. 签名上的 `&AppHandle` 一维换成 `&dyn MergeReporter`（`merge_group` /
//!    `run_sql_merge` 各一处，调用点零改动）。
//! 3. 入口 `start_sql_merge(app, options) -> Result<(), String>`（内部
//!    `thread::spawn`）改为 [`merge_sql`]（同步 + 直接返回结果）——插件本身
//!    已在独立进程里，线程包装只会把错误多绕一道。
//!
//! 源仓自带的 16 条单测原样保留（`mod tests` 在文件末尾），它们覆盖的就是
//! 这批纯函数，等于免费继承上游的行为锁。
//!
//! 下面的 clippy 豁免逐条列名、**只为本文件服务**：这些都是「改写法才消失」的建议，
//! 而改写法正是移植要避免的成本（`collapsible_if` 的正确写法要 Rust 1.88 的
//! let_chains，本仓 MSRV 是 1.85）。
#![allow(
    clippy::collapsible_if,
    clippy::double_ended_iterator_last,
    clippy::for_kv_map,
    clippy::too_many_arguments,
    clippy::unnecessary_map_or
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use encoding_rs::GBK;
use regex::Regex;
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

/// 合并过程的对外回执。
///
/// 源仓用 Tauri 的 `AppHandle::emit` 把三类事件广播给 WebView；这里换成 trait，
/// 因为 `duck-sql-core` 是零 IO 的内核 crate——**事件怎么落地是插件的事**
/// （process 轨写 stdout 的 `log`/`progress` 事件，还要先过译文表）。
/// 三个方法与源仓 `emit_progress` / `emit_log` / `emit_done` 的载荷一一对应，
/// 一个字段都不多不少。
pub trait MergeReporter {
    /// 阶段进度。`stage` 是源仓的英文阶段名（`scanning` / `gathering` / …）。
    fn progress(
        &self,
        stage: &str,
        percent: u32,
        current: usize,
        total: usize,
        current_file: Option<String>,
    );
    /// 一行过程日志。`level`：`info` / `warn` / `error` / `success`。
    fn log(&self, level: &str, message: &str, timestamp: String, duration_ms: Option<u64>);
    /// 完成回执（成功时给最终统计）。
    fn done(&self, result: &SqlMergeResult);
}

// ── Global Cancel Flag ──

static CANCEL_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn cancel_flag() -> &'static Arc<AtomicBool> {
    CANCEL_FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

pub fn cancel() {
    cancel_flag().store(true, Ordering::SeqCst);
}

pub fn reset_cancel() {
    cancel_flag().store(false, Ordering::SeqCst);
}

// ── Log Collector ──

#[derive(Debug, Clone, Serialize)]
struct LogEntryRecord {
    timestamp: String,
    level: String,
    message: String,
    duration_ms: Option<u64>,
}

thread_local! {
    static MERGE_LOG: std::cell::RefCell<Vec<LogEntryRecord>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn collect_log_entry(level: &str, message: &str, timestamp: &str, duration_ms: Option<u64>) {
    MERGE_LOG.with(|log| {
        log.borrow_mut().push(LogEntryRecord {
            timestamp: timestamp.to_string(),
            level: level.to_string(),
            message: message.to_string(),
            duration_ms,
        });
    });
}

fn clear_log() {
    MERGE_LOG.with(|log| log.borrow_mut().clear());
}

fn drain_log() -> Vec<LogEntryRecord> {
    MERGE_LOG.with(|log| log.borrow_mut().drain(..).collect())
}

thread_local! {
    static NOTICE_ITEMS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn clear_notices() {
    NOTICE_ITEMS.with(|n| n.borrow_mut().clear());
}

fn add_notice(msg: String) {
    NOTICE_ITEMS.with(|n| n.borrow_mut().push(msg));
}

fn drain_notices() -> Vec<String> {
    NOTICE_ITEMS.with(|n| n.borrow_mut().drain(..).collect())
}

/// 读取文件并自动检测编码（UTF-8 BOM → UTF-8 → GBK）
/// 参考 CharseToGBK.java 的 BOM + 字节模式检测逻辑
fn read_file_auto_encoding(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("读取文件失败 {}: {}", path.display(), e))?;
    if bytes.is_empty() {
        return Ok(String::new());
    }

    // Step 1: BOM detection
    if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        // UTF-8 BOM — strip and parse as UTF-8
        return String::from_utf8(bytes[3..].to_vec())
            .map_err(|_| format!("UTF-8 BOM 文件解码失败: {}", path.display()));
    }
    if bytes.len() >= 2 {
        if bytes[0] == 0xFF && bytes[1] == 0xFE {
            // UTF-16LE BOM
            let u16_chars: Vec<u16> = bytes[2..].chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            return String::from_utf16(&u16_chars)
                .map_err(|_| format!("UTF-16LE 文件解码失败: {}", path.display()));
        }
        if bytes[0] == 0xFE && bytes[1] == 0xFF {
            // UTF-16BE BOM
            let u16_chars: Vec<u16> = bytes[2..].chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            return String::from_utf16(&u16_chars)
                .map_err(|_| format!("UTF-16BE 文件解码失败: {}", path.display()));
        }
    }

    // Step 2: Try UTF-8 decode directly
    if let Ok(s) = std::str::from_utf8(&bytes) {
        return Ok(s.to_string());
    }

    // Step 3: Byte pattern analysis (CharseToGBK.java style)
    // Scan for valid UTF-8 multi-byte sequences
    let mut is_utf8 = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            i += 1;
            continue;
        }
        if b >= 0xF0 {
            // 4-byte sequence — not commonly used in SQL files, break
            is_utf8 = false;
            break;
        }
        if b < 0xC0 {
            // Standalone continuation byte 0x80-0xBF → not UTF-8
            is_utf8 = false;
            break;
        }
        if b < 0xE0 {
            // 2-byte sequence: 0xC0-0xDF followed by 0x80-0xBF
            if i + 1 >= bytes.len() || bytes[i + 1] & 0xC0 != 0x80 {
                is_utf8 = false;
                break;
            }
            i += 2;
            continue;
        }
        // 3-byte sequence: 0xE0-0xEF followed by two 0x80-0xBF
        if i + 2 >= bytes.len() || bytes[i + 1] & 0xC0 != 0x80 || bytes[i + 2] & 0xC0 != 0x80 {
            is_utf8 = false;
            break;
        }
        is_utf8 = true;
        i += 3;
    }

    if is_utf8 {
        // Found valid 3-byte UTF-8 sequence — decode lossily
        let (cow, _had_errors) = encoding_rs::UTF_8.decode_without_bom_handling(&bytes);
        return Ok(cow.to_string());
    }

    // Step 4: Fallback to GBK (default encoding for IFMS SQL files)
    let (cow, _encoding, _had_errors) = GBK.decode(&bytes);
    Ok(cow.to_string())
}

fn last_line_is_sql_comment(content: &str) -> bool {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .last()
        .is_some_and(|l| {
            let t = l.trim();
            t.starts_with("--") || t.starts_with("/*") || t == "*/"
        })
}

fn write_log_file(entries: &[LogEntryRecord], path: &Path) {
    let mut content = String::new();
    for entry in entries {
        content.push_str(&format!(
            "[{}] {}: {}\n",
            entry.timestamp,
            entry.level.to_uppercase(),
            entry.message
        ));
    }
    let _ = std::fs::write(path, content);
}

// ── Temp Directory Guard ──

struct TempGuard {
    dirs: Vec<PathBuf>,
}

impl TempGuard {
    fn new() -> Self {
        Self { dirs: Vec::new() }
    }
    fn push(&mut self, dir: PathBuf) {
        self.dirs.push(dir);
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        for dir in self.dirs.drain(..) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

// ── ZIP Extraction Helpers ──

fn create_temp_dir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join("spsql2_merge");
    std::fs::create_dir_all(&base).map_err(|e| format!("创建临时目录失败: {}", e))?;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S_%6f");
    let dir = base.join(ts.to_string());
    std::fs::create_dir(&dir).map_err(|e| format!("创建临时子目录失败: {}", e))?;
    Ok(dir)
}

fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("打开 ZIP 失败 {}: {}", zip_path.display(), e))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| format!("读取 ZIP 失败 {}: {}", zip_path.display(), e))?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)
            .map_err(|e| format!("读取 ZIP 条目 {} 失败: {}", i, e))?;
        let outpath = dest.join(entry.mangled_name());

        if entry.is_dir() {
            let _ = std::fs::create_dir_all(&outpath);
        } else {
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
            }
            let mut outfile = std::fs::File::create(&outpath)
                .map_err(|e| format!("创建文件失败 {}: {}", outpath.display(), e))?;
            std::io::copy(&mut entry, &mut outfile)
                .map_err(|e| format!("写入文件失败 {}: {}", outpath.display(), e))?;
        }
    }
    Ok(())
}

/// Shared SQL merge entry for use by other merge modules (sp_merge, non_sp_merge, ifms6_merge).
/// Recursively finds `database/`, `spsql/`, `sql/` directories under `root`,
/// gathers `.sql` files, groups by normalized relative path, merges with DDL/DML split,
/// writes to `root/merge_sql_{ts}/`. Silent operation — no Tauri event emission.

#[derive(Clone)]
struct SqlEntry {
    rel_path: PathBuf,
    full_path: PathBuf,
}

fn file_mtime_naive(path: &Path) -> Option<chrono::NaiveDateTime> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let dt: chrono::DateTime<chrono::Local> = modified.into();
    Some(dt.naive_local())
}

fn version_tuple_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"V(\d{6})\.(\d{1,3})\.(\d{1,3})").expect("valid regex"))
}

/// 从相对路径中提取最大的版本段 (monthversion, big, mini)，用于同名脚本的时间平局裁决。
fn version_key_of(rel_path: &Path) -> Option<(u32, u32, u32)> {
    let re = version_tuple_regex();
    let name = rel_path.to_string_lossy();
    let mut best: Option<(u32, u32, u32)> = None;
    for caps in re.captures_iter(&name) {
        let t = (
            caps[1].parse::<u32>().ok()?,
            caps[2].parse::<u32>().ok()?,
            caps[3].parse::<u32>().ok()?,
        );
        if best.map_or(true, |b| t > b) {
            best = Some(t);
        }
    }
    best
}

#[derive(Clone)]
struct DedupeKey {
    mtime: Option<chrono::NaiveDateTime>,
    ver: Option<(u32, u32, u32)>,
}

/// 裁决优先级：修改日期新者优先；日期相同/缺失时版本段高者胜；
/// 都无法区分时保留先出现者（调用方已按 rel_path 排序）。
fn prefer_key(new: &DedupeKey, old: &DedupeKey) -> bool {
    use std::cmp::Ordering;
    match (new.mtime, old.mtime) {
        (Some(n), Some(o)) => match n.cmp(&o) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => new.ver > old.ver,
        },
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => new.ver > old.ver,
    }
}

/// 按「文件名 → 修改日期最新」去重：同名脚本只保留日期最新的一份，
/// 不同名脚本仍全部保留（由调用方追加合并）。
fn dedupe_keep_newest_by<F>(entries: Vec<SqlEntry>, mtime_of: F) -> Vec<SqlEntry>
where
    F: Fn(&Path) -> Option<chrono::NaiveDateTime>,
{
    let mut best: std::collections::BTreeMap<String, (DedupeKey, SqlEntry)> =
        std::collections::BTreeMap::new();
    for e in entries {
        let fname = e
            .rel_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let key = DedupeKey {
            mtime: mtime_of(&e.full_path),
            ver: version_key_of(&e.rel_path),
        };
        match best.get_mut(&fname) {
            Some(slot) => {
                if prefer_key(&key, &slot.0) {
                    slot.0 = key;
                    slot.1 = e;
                }
            }
            None => {
                best.insert(fname, (key, e));
            }
        }
    }
    let mut out: Vec<SqlEntry> = best.into_values().map(|(_, e)| e).collect();
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    out
}

pub fn merge_sql_tree(root: &Path, cancel: &AtomicBool) -> Result<usize, String> {
    // Find all SQL root directories at any depth
    let mut sql_roots: Vec<PathBuf> = Vec::new();
    let walker = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    for entry in walker {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "database" || name == "spsql" || name == "sql" || name == "reqsql" {
                sql_roots.push(entry.path().to_path_buf());
            }
        }
    }

    if sql_roots.is_empty() {
        return Ok(0);
    }

    let mut all_entries: Vec<SqlEntry> = Vec::new();
    let mut all_scripts: Vec<SqlEntry> = Vec::new();
    let mut script_dirs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for sql_root in &sql_roots {
        let base_len = sql_root.to_string_lossy().len();
        let walker = walkdir::WalkDir::new(sql_root).follow_links(false).into_iter();
        for entry in walker {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let rel = path.to_string_lossy()[base_len..].to_string();
            let rel = rel.trim_start_matches('/').trim_start_matches('\\');
            if entry.file_type().is_dir() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.ends_with(".sh") {
                    script_dirs.push((PathBuf::from(rel), path.to_path_buf()));
                }
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if ext == Some("sh") {
                all_scripts.push(SqlEntry {
                    rel_path: PathBuf::from(rel),
                    full_path: path.to_path_buf(),
                });
                continue;
            }
            if ext != Some("sql") {
                continue;
            }
            let rel_path = PathBuf::from(rel);
            if is_in_sh_directory(&rel_path) {
                continue;
            }
            all_entries.push(SqlEntry {
                rel_path,
                full_path: path.to_path_buf(),
            });
        }
    }

    if all_entries.is_empty() && all_scripts.is_empty() && script_dirs.is_empty() {
        return Ok(0);
    }

    // Group .sql files by normalized parent directory
    let mut groups: std::collections::BTreeMap<String, Vec<SqlEntry>> = std::collections::BTreeMap::new();
    for entry in &all_entries {
        let parent = entry.rel_path.parent().unwrap_or(Path::new(""));
        let normalized = normalize_dir_path(parent);
        let key = normalized.to_string_lossy().to_string();
        groups.entry(key).or_default().push(entry.clone());
    }

    // 同名脚本去重：规范化路径相同的脚本（不同版本包各带一份）只保留修改日期最新的一份，
    // 不同名脚本仍按 rel_path 顺序全部追加
    for (_key, entries) in groups.iter_mut() {
        entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        *entries = dedupe_keep_newest_by(std::mem::take(entries), file_mtime_naive);
    }

    // Group .sh scripts by normalized parent directory
    let mut script_groups: std::collections::BTreeMap<String, Vec<SqlEntry>> = std::collections::BTreeMap::new();
    for entry in &all_scripts {
        let parent = entry.rel_path.parent().unwrap_or(Path::new(""));
        let normalized = normalize_dir_path(parent);
        let key = normalized.to_string_lossy().to_string();
        script_groups.entry(key).or_default().push(entry.clone());
    }

    // 同名 .sh 脚本同样取修改日期最新的一份
    for (_key, entries) in script_groups.iter_mut() {
        entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        *entries = dedupe_keep_newest_by(std::mem::take(entries), file_mtime_naive);
    }

    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let output_root = root.join(format!("merge_sql_{}", timestamp));
    std::fs::create_dir_all(&output_root)
        .map_err(|e| format!("创建 merge_sql 目录失败: {}", e))?;

    let mut merged_count = 0usize;

    for (group_key, entries) in &groups {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let group_dir = output_root.join(group_key);
        std::fs::create_dir_all(&group_dir)
            .map_err(|e| format!("创建分组目录失败: {}", e))?;

        let mut ddl_content = String::new();
        let mut dml_content = String::new();
        let mut has_ddl = false;
        let mut has_dml = false;

        for entry in entries {
            let fname = entry.full_path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let content = read_file_auto_encoding(&entry.full_path)?;

            let trimmed = content.trim_end();
            let normalized = if trimmed.ends_with(';') || last_line_is_sql_comment(trimmed) {
                format!("{}\n", trimmed)
            } else {
                format!("{};\n", trimmed)
            };

            if is_ddl_file(fname) {
                if has_ddl { ddl_content.push('\n'); }
                ddl_content.push_str(&normalized);
                has_ddl = true;
            } else if !is_create_tablespace(fname) {
                if has_dml { dml_content.push('\n'); }
                dml_content.push_str(&normalized);
                has_dml = true;
            }
        }

        let tag = group_key.split('/').filter(|s| !s.is_empty()).last().unwrap_or("sql");

        if has_ddl {
            let output_name = format!("{}_DDL.sql", tag);
            let output_path = group_dir.join(&output_name);
            std::fs::create_dir_all(output_path.parent().unwrap_or(Path::new("")))
                .map_err(|e| format!("创建 DDL 输出目录失败: {}", e))?;
            std::fs::write(&output_path, &ddl_content)
                .map_err(|e| format!("写入 DDL 文件失败 ({}): {}", output_path.display(), e))?;
            merged_count += 1;
        }

        if has_dml {
            if !dml_content.ends_with('\n') { dml_content.push('\n'); }
            dml_content.push_str("COMMIT;\n");
            let output_name = format!("{}_DML.sql", tag);
            let output_path = group_dir.join(&output_name);
            std::fs::create_dir_all(output_path.parent().unwrap_or(Path::new("")))
                .map_err(|e| format!("创建 DML 输出目录失败: {}", e))?;
            std::fs::write(&output_path, &dml_content)
                .map_err(|e| format!("写入 DML 文件失败 ({}): {}", output_path.display(), e))?;
            merged_count += 1;
        }
    }

    // Copy .sh files to their group output directories
    for (group_key, scripts) in &script_groups {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }
        let group_dir = output_root.join(group_key);
        std::fs::create_dir_all(&group_dir)
            .map_err(|e| format!("创建脚本分组目录失败: {}", e))?;
        for script in scripts {
            let fname = script.full_path.file_name().unwrap_or_default();
            let target = group_dir.join(fname);
            std::fs::copy(&script.full_path, &target)
                .map_err(|e| format!("复制脚本失败 {}: {}", script.full_path.display(), e))?;
        }
    }

    // Copy .sh directories (e.g. zz_fenbiao.sh/) recursively to their group output directories
    for (rel_path, full_path) in &script_dirs {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }
        let parent = rel_path.parent().unwrap_or(Path::new(""));
        let normalized = normalize_dir_path(parent);
        let group_key = normalized.to_string_lossy().to_string();
        let group_dir = output_root.join(&group_key);
        let dirname = rel_path.file_name().unwrap_or_default();
        let target = group_dir.join(dirname);
        copy_dir_recursive(full_path, &target)?;
    }

    Ok(merged_count)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("创建目录失败 {}: {}", dst.display(), e))?;
    let entries = std::fs::read_dir(src)
        .map_err(|e| format!("读取目录失败 {}: {}", src.display(), e))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let target = dst.join(&name);
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)
                .map_err(|e| format!("复制文件失败 {}: {}", path.display(), e))?;
        }
    }
    Ok(())
}

/// Find `sqls/` directory inside a given root directory.
fn find_sqls_dir(root: &Path) -> Result<Option<PathBuf>, String> {
    let direct = root.join("sqls");
    if direct.is_dir() {
        return Ok(Some(direct));
    }
    for entry in std::fs::read_dir(root).map_err(|e| format!("读取目录失败: {}", e))? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.is_dir() {
            let candidate = path.join("sqls");
            if candidate.is_dir() {
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
}

fn extract_zip_tag(filename: &str) -> String {
    // "db-HAXM.zip" → "HAXM", "db.zip" → ""
    let stem = filename.strip_suffix(".zip").unwrap_or(filename);
    if stem == "db" || stem == "db-" {
        String::new()
    } else if let Some(tag) = stem.strip_prefix("db-") {
        tag.to_string()
    } else if let Some(tag) = stem.strip_prefix("db") {
        tag.to_string()
    } else {
        String::new()
    }
}

// ── Data Structures ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlMergeOptions {
    pub src_dir: String,
    pub customer_no: Option<String>,
    pub db_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlMergeResult {
    pub success: bool,
    pub output_dir: String,
    pub statistics: SqlMergeStatistics,
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlMergeStatistics {
    pub packages: usize,
    pub merged_groups: Vec<MergedGroupInfo>,
    pub total_sql_count: usize,
    pub total_duration_secs: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlFileStat {
    pub source_package: String,
    pub source_path: String,
    pub target_path: String,
    pub sql_count: usize,
    pub file_size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergedGroupInfo {
    pub group_name: String,
    pub output_path: String,
    pub source_files: Vec<SqlFileStat>,
    pub total_sql_count: usize,
    pub match_check: bool,
}

// ── IFMS Version Regex ──

fn version_segment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^.{0,30}V\d{6}\.\d{2}\.\d{3}$").expect("valid regex"))
}

// ── SQL Statement Counting ──

fn count_sql_statements(content: &str) -> usize {
    content
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .count()
}

fn is_ddl_file(filename: &str) -> bool {
    let lower = filename.to_lowercase();
    lower.contains("ddl") || lower.contains("dbstruct") || lower.contains("_oracle") || lower.contains(".ora.")
}

fn is_create_tablespace(filename: &str) -> bool {
    filename.to_lowercase().contains("create_tablespace")
}

// ── Path Normalization ──

fn is_version_segment(name: &str) -> bool {
    version_segment_regex().is_match(name)
}

fn is_in_sh_directory(rel: &Path) -> bool {
    rel.parent().map_or(false, |parent| {
        parent.components().any(|c| {
            c.as_os_str().to_string_lossy().ends_with(".sh")
        })
    })
}

fn normalize_dir_path(rel_path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in rel_path.components() {
        if let Some(name) = component.as_os_str().to_str() {
            if is_version_segment(name) {
                continue;
            }
            normalized.push(name);
        } else {
            normalized.push(component);
        }
    }
    normalized
}

// ── Emit Helpers ──

fn emit_progress(
    app: &dyn MergeReporter,
    stage: &str,
    percent: u32,
    current: usize,
    total: usize,
    current_file: Option<String>,
) {
    app.progress(stage, percent, current, total, current_file);
}

fn emit_log(
    app: &dyn MergeReporter,
    level: &str,
    message: &str,
    timestamp: Option<String>,
    duration_ms: Option<u64>,
) {
    let ts = timestamp.unwrap_or_else(|| chrono::Local::now().format("%H:%M:%S").to_string());
    collect_log_entry(level, message, &ts, duration_ms);
    app.log(level, message, ts, duration_ms);
}

fn emit_done(app: &dyn MergeReporter, result: &SqlMergeResult) {
    app.done(result);
}

// ── Scan Packages ──

struct PackageInfo {
    dir: PathBuf,
    name: String,
    container: String,
}

fn scan_packages(src_dir: &Path, cancel: &AtomicBool) -> Result<Vec<PackageInfo>, String> {
    let mut packages = Vec::new();

    let entries = std::fs::read_dir(src_dir)
        .map_err(|e| format!("读取目录失败: {}", e))?;

    for entry in entries.flatten() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let database_dir = path.join("database");
        if !database_dir.is_dir() {
            continue;
        }

        if name.starts_with('.') {
            continue;
        }

        packages.push(PackageInfo { dir: path, name, container: String::new() });
    }

    packages.sort_by(|a, b| a.name.cmp(&b.name));

    if packages.is_empty() {
        return Err("未找到包含 database/ 目录的有效包".to_string());
    }

    Ok(packages)
}

// ── Gather SQL Files ──

#[derive(Clone)]
struct SqlFileInfo {
    full_path: PathBuf,
    rel_to_database: PathBuf,
    pkg_name: String,
}

fn gather_sql_files(pkg: &PackageInfo, cancel: &AtomicBool) -> Result<Vec<SqlFileInfo>, String> {
    let database_dir = pkg.dir.join("database");
    let mut files = Vec::new();

    if !database_dir.is_dir() {
        return Ok(files);
    }

    let walker = walkdir::WalkDir::new(&database_dir)
        .follow_links(false)
        .into_iter();

    for entry in walker {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_lowercase(),
            None => continue,
        };

        if ext != "sql" {
            continue;
        }

        let rel = match path.strip_prefix(&database_dir) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };

        if is_in_sh_directory(&rel) {
            continue;
        }

        files.push(SqlFileInfo {
            full_path: path.to_path_buf(),
            rel_to_database: rel,
            pkg_name: pkg.name.clone(),
        });
    }

    files.sort_by(|a, b| a.rel_to_database.cmp(&b.rel_to_database));
    Ok(files)
}

// ── SPSQL Mode: Scan packages from all containers (pub/, trans/, ...) ──

fn scan_spsql_packages(src_dir: &Path, cancel: &AtomicBool) -> Result<Vec<PackageInfo>, String> {
    // Determine containers: subdirectories of src_dir (pub, trans, etc.)
    // If src_dir has no subdirectories, treat src_dir itself as the container
    let mut containers: Vec<(PathBuf, String)> = Vec::new();
    {
        let entries = std::fs::read_dir(src_dir)
            .map_err(|e| format!("读取目录失败: {}", e))?;
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if name.starts_with('.') {
                continue;
            }
            containers.push((path, name));
        }
    }

    // If no subdirectories found, treat src_dir as flat container
    if containers.is_empty() {
        containers.push((src_dir.to_path_buf(), String::new()));
    }

    let mut packages = Vec::new();
    for (container_dir, container_name) in &containers {
        let entries = std::fs::read_dir(container_dir)
            .map_err(|e| format!("读取 {} 目录失败: {}", container_name, e))?;
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if name.starts_with('.') {
                continue;
            }
            packages.push(PackageInfo {
                dir: path,
                name,
                container: container_name.clone(),
            });
        }
    }

    packages.sort_by(|a, b| a.name.cmp(&b.name));

    if packages.is_empty() {
        return Err("未找到有效的版本目录".to_string());
    }

    Ok(packages)
}

// ── SPSQL Mode: Gather SQL files from version directory ──

fn gather_spsql_sql_files(pkg: &PackageInfo, cancel: &AtomicBool) -> Result<Vec<SqlFileInfo>, String> {
    let mut files = Vec::new();

    // Packages ending with .sh (e.g. zz_fenbiao.sh) should be copied as-is, not merged
    if pkg.name.ends_with(".sh") {
        return Ok(files);
    }

    let pkg_dir = &pkg.dir;

    if !pkg_dir.is_dir() {
        return Ok(files);
    }

    let walker = walkdir::WalkDir::new(pkg_dir)
        .follow_links(false)
        .into_iter();

    for entry in walker {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_lowercase(),
            None => continue,
        };

        if ext != "sql" {
            continue;
        }

        let rel = match path.strip_prefix(pkg_dir) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };

        if is_in_sh_directory(&rel) {
            continue;
        }

        let rel_with_container = if pkg.container.is_empty() {
            rel
        } else {
            Path::new(&pkg.container).join(&rel)
        };

        files.push(SqlFileInfo {
            full_path: path.to_path_buf(),
            rel_to_database: rel_with_container,
            pkg_name: pkg.name.clone(),
        });
    }

    files.sort_by(|a, b| a.rel_to_database.cmp(&b.rel_to_database));
    Ok(files)
}

// ── SPSQL2 Mode: Scan packages from db-*.zip files ──

fn scan_spsql2_packages(
    src_dir: &Path,
    cancel: &AtomicBool,
    temp_guard: &mut TempGuard,
) -> Result<Vec<PackageInfo>, String> {
    let mut packages = Vec::new();

    let entries = std::fs::read_dir(src_dir)
        .map_err(|e| format!("读取目录失败: {}", e))?;

    for entry in entries.flatten() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        if !filename.starts_with("db") || !filename.ends_with(".zip") {
            continue;
        }

        let tag = extract_zip_tag(&filename);

        // Extract to temp dir
        let temp_dir = create_temp_dir()?;
        temp_guard.push(temp_dir.clone());
        extract_zip(&path, &temp_dir)?;

        // Find sqls/ inside extracted content
        let sqls_dir = match find_sqls_dir(&temp_dir)? {
            Some(d) => d,
            None => continue,
        };

        // Scan containers (pub, trans, etc.) inside sqls/
        let container_entries = std::fs::read_dir(&sqls_dir)
            .map_err(|e| format!("读取 sqls 子目录失败: {}", e))?;

        for ce in container_entries.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }

            let container_path = ce.path();
            if !container_path.is_dir() {
                continue;
            }
            let container_name = match container_path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if container_name.starts_with('.') {
                continue;
            }

            let full_container = if tag.is_empty() {
                container_name.clone()
            } else {
                format!("{}/{}", tag, container_name)
            };

            // Scan version directories inside this container
            let version_entries = std::fs::read_dir(&container_path)
                .map_err(|e| format!("读取 {} 目录失败: {}", container_name, e))?;

            for ve in version_entries.flatten() {
                if cancel.load(Ordering::Relaxed) {
                    return Err("用户取消".to_string());
                }

                let version_path = ve.path();
                if !version_path.is_dir() {
                    continue;
                }
                let version_name = match version_path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                if version_name.starts_with('.') {
                    continue;
                }

                packages.push(PackageInfo {
                    dir: version_path,
                    name: version_name,
                    container: full_container.clone(),
                });
            }
        }
    }

    packages.sort_by(|a, b| a.name.cmp(&b.name));

    if packages.is_empty() {
        return Err("未找到有效的 db-*.zip 文件".to_string());
    }

    Ok(packages)
}

// ── Read and Count ──

struct FileContent {
    content: String,
    sql_count: usize,
}

fn read_and_count(path: &Path) -> Result<FileContent, String> {
    let content = read_file_auto_encoding(path)?;

    let sql_count = count_sql_statements(&content);
    Ok(FileContent { content, sql_count })
}

// ── Merge Group ──

fn merge_group(
    app: &dyn MergeReporter,
    group_key: &str,
    _packages: &[PackageInfo],
    all_sql_files: &[(String, Vec<SqlFileInfo>)],
    output_root: &Path,
    first_pkg_name: &str,
    last_pkg_name: &str,
    cancel: &AtomicBool,
    suffix: &str,
) -> Result<MergedGroupInfo, String> {
    let merged_filename = format!("{}～{}{}.sql", first_pkg_name, last_pkg_name, suffix);
    let output_dir = output_root.join(group_key);
    let output_path = output_dir.join(&merged_filename);

    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("创建输出目录失败 {}: {}", output_dir.display(), e))?;

    let mut merged_content = String::new();
    let mut source_files = Vec::new();
    let mut total_sql_count: usize = 0;

    for (pkg_name, files) in all_sql_files {
        for sql_file in files {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }

            let FileContent { content, sql_count } = read_and_count(&sql_file.full_path)?;

            let file_size = std::fs::metadata(&sql_file.full_path)
                .map(|m| m.len())
                .unwrap_or(0);

            // Check trailing semicolon
            let trimmed_content = content.trim_end();
            let is_comment_last = trimmed_content.is_empty() || last_line_is_sql_comment(trimmed_content);
            let ends_with_semicolon = trimmed_content.is_empty() || trimmed_content.ends_with(';');
            if !ends_with_semicolon && !is_comment_last {
                // Extract last meaningful SQL statement for context
                let last_stmt = trimmed_content
                    .rsplit(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .lines()
                    .filter(|l| !l.trim().is_empty() && !l.trim().starts_with("--"))
                    .last()
                    .unwrap_or("")
                    .trim();
                let excerpt = if last_stmt.len() > 60 {
                    format!("...{}", &last_stmt[last_stmt.len().saturating_sub(57)..])
                } else {
                    last_stmt.to_string()
                };
                let warn_msg = format!(
                    "[{}] {} 文件末尾缺少分号，已自动补全\n      └─ 末句: {}",
                    pkg_name,
                    sql_file.rel_to_database.display(),
                    excerpt,
                );
                emit_log(app, "warn", &format!("  ⚠️ {}", warn_msg), None, None);
                add_notice(format!("文件末尾缺少分号: {} ({}) 末句: {}", sql_file.rel_to_database.display(), pkg_name, excerpt));
            }

            // Log: source file being merged
            let pkg_display = &sql_file.pkg_name;
            emit_log(
                app,
                "info",
                &format!(
                    "  📄 [{}] {} → {} ({} 条SQL)",
                    pkg_display,
                    sql_file.rel_to_database.display(),
                    merged_filename,
                    sql_count,
                ),
                None,
                None,
            );

            source_files.push(SqlFileStat {
                source_package: pkg_name.clone(),
                source_path: sql_file.rel_to_database.to_string_lossy().to_string(),
                target_path: merged_filename.clone(),
                sql_count,
                file_size,
            });

            total_sql_count += sql_count;

            // Normalize: ensure every file content ends with ;\n for correct counting
            let normalized = {
                let trimmed = content.trim_end();
                if trimmed.ends_with(';') || last_line_is_sql_comment(trimmed) {
                    format!("{}\n", trimmed)
                } else {
                    format!("{};\n", trimmed)
                }
            };

            if !merged_content.is_empty() {
                merged_content.push('\n');
            }
            merged_content.push_str(&normalized);
        }
    }

    // Append COMMIT for DML files
    if suffix == "_DML" {
        merged_content.push_str("COMMIT;\n");
    }

    // Write merged file
    let merged_sql_count = count_sql_statements(&merged_content);
    let match_check = merged_sql_count == total_sql_count
        || (suffix == "_DML" && merged_sql_count == total_sql_count + 1);

    std::fs::write(&output_path, &merged_content)
        .map_err(|e| format!("写入合并文件失败 {}: {}", output_path.display(), e))?;

    let merged_file_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let check_mark = if match_check { "✅" } else { "⚠️" };
    emit_log(
        app,
        if match_check { "success" } else { "warn" },
        &format!(
            "  {} 合并完成: {} (源 {} 条SQL → 合并 {} 条SQL, 大小 {} bytes)",
            check_mark,
            merged_filename,
            total_sql_count,
            merged_sql_count,
            merged_file_size,
        ),
        None,
        None,
    );

    Ok(MergedGroupInfo {
        group_name: group_key.to_string(),
        output_path: output_path.to_string_lossy().to_string(),
        source_files,
        total_sql_count: merged_sql_count,
        match_check,
    })
}

// ── Customer / DB Type Filtering ──

/// Filter SQL files by customer number and database type.
/// - customer_no: only keep paths containing this string
/// - db_types: only keep paths under matching db type directories
fn filter_sql_files(files: Vec<SqlFileInfo>, customer_no: &str, db_types: &[String]) -> Vec<SqlFileInfo> {
    files.into_iter().filter(|f| {
        let path_str = f.rel_to_database.to_string_lossy();
        // Customer filter: path must contain customer_no
        if !customer_no.is_empty() && !path_str.contains(customer_no) {
            return false;
        }
        // DB type filter: path must be under one of the db types
        if !db_types.is_empty() {
            let under_valid_db = db_types.iter().any(|db| {
                path_str.starts_with(db) || path_str.contains(&format!("/{}/", db))
            });
            if !under_valid_db {
                return false;
            }
        }
        true
    }).collect()
}

// ── Auto-detect Mode ──

fn has_db_zip_files(dir: &Path) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("db") && name.ends_with(".zip") {
                return true;
            }
        }
    }
    false
}

fn detect_merge_mode(src_dir: &Path) -> &'static str {
    if has_db_zip_files(src_dir) {
        "spsql2"
    } else if src_dir.join("pub").is_dir() {
        "spsql"
    } else {
        "standard"
    }
}

// ── Main Merge Logic ──

fn run_sql_merge(
    app: &dyn MergeReporter,
    src_dir: &Path,
    options: &SqlMergeOptions,
    cancel: &AtomicBool,
) -> Result<SqlMergeResult, String> {
    let overall_start = Instant::now();
    clear_log();
    clear_notices();

    let mode = detect_merge_mode(src_dir);
    let mut _temp_guard = TempGuard::new();

    // Phase 1: Scan packages
    emit_progress(app, "scanning", 5, 0, 0, None);
    emit_log(app, "info", &format!("开始扫描包目录: {}", src_dir.display()), None, None);

    let packages = if mode == "spsql2" {
        emit_log(app, "info", "检测到 db-*.zip 文件，自动切换 ZIP 合并模式", None, None);
        scan_spsql2_packages(src_dir, cancel, &mut _temp_guard)?
    } else if mode == "spsql" {
        emit_log(app, "info", "检测到 SPSQL 参考库结构，按容器分组合并", None, None);
        scan_spsql_packages(src_dir, cancel)?
    } else {
        scan_packages(src_dir, cancel)?
    };
    emit_log(
        app,
        "info",
        &format!("扫描到 {} 个包: {:?}", packages.len(), packages.iter().map(|p| &p.name).collect::<Vec<_>>()),
        None,
        None,
    );

    // Phase 2: Gather SQL files
    emit_progress(app, "gathering", 20, 0, packages.len(), None);
    emit_log(app, "info", "开始收集 SQL 文件...", None, None);

    let mut package_files: Vec<(String, Vec<SqlFileInfo>)> = Vec::new();
    for (idx, pkg) in packages.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let files = if mode == "spsql2" || mode == "spsql" {
            gather_spsql_sql_files(pkg, cancel)?
        } else {
            gather_sql_files(pkg, cancel)?
        };
        emit_log(
            app,
            "info",
            &format!(
                "  📦 {}: 找到 {} 个 SQL 文件",
                pkg.name,
                files.len(),
            ),
            None,
            None,
        );

        // Apply customer/db filtering if specified
        let files = if options.customer_no.is_some() || options.db_type.is_some() {
            let cust = options.customer_no.as_deref().unwrap_or("");
            let db_types: Vec<String> = options.db_type.as_ref()
                .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                .unwrap_or_default();
            filter_sql_files(files, cust, &db_types)
        } else {
            files
        };

        for f in &files {
            emit_log(
                app,
                "debug",
                &format!(
                    "      {}",
                    f.rel_to_database.display(),
                ),
                None,
                None,
            );
        }

        package_files.push((pkg.name.clone(), files));
        emit_progress(
            app,
            "gathering",
            20 + ((idx + 1) * 20 / packages.len()) as u32,
            idx + 1,
            packages.len(),
            Some(pkg.name.clone()),
        );
    }

    // Phase 3: Group files by normalized directory
    emit_progress(app, "grouping", 45, 0, 0, None);
    emit_log(app, "info", "开始分组...", None, None);

    // Groups: group_key -> Vec<(pkg_name, SqlFileInfo)>
    let mut dir_groups: std::collections::BTreeMap<String, Vec<(String, SqlFileInfo)>> =
        std::collections::BTreeMap::new();

    for (pkg_name, files) in &package_files {
        for sql_file in files {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }

            let parent = sql_file.rel_to_database.parent().unwrap_or(Path::new(""));
            let normalized = normalize_dir_path(parent);
            let group_key = normalized.to_string_lossy().to_string();

            dir_groups
                .entry(group_key)
                .or_default()
                .push((pkg_name.clone(), sql_file.clone()));
        }
    }

    emit_log(
        app,
        "info",
        &format!("分组完成: {} 个目录组", dir_groups.len()),
        None,
        None,
    );

    // Phase 4: Merge
    emit_progress(app, "merging", 50, 0, 0, None);
    emit_log(app, "info", "开始合并...", None, None);

    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let output_root = src_dir.join(format!("merge_sql_{}", timestamp));

    let mut merged_groups = Vec::new();
    let total_groups = dir_groups.len();

    for (group_idx, (group_key, files)) in dir_groups.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let pkg_start = Instant::now();
        emit_log(
            app,
            "info",
            &format!("▶ 合并组 [{}/{}]: {}", group_idx + 1, total_groups, group_key),
            None,
            None,
        );

        // Re-organize files by package for merge_group
        let mut by_pkg: Vec<(String, Vec<SqlFileInfo>)> = Vec::new();
        for (pkg_name, sql_file) in files {
            if let Some(last) = by_pkg.last_mut() {
                if last.0 == *pkg_name {
                    last.1.push(sql_file.clone());
                    continue;
                }
            }
            by_pkg.push((pkg_name.clone(), vec![sql_file.clone()]));
        }

        let first_pkg_in_group = by_pkg.first().map(|(n, _)| n.as_str()).unwrap_or("");
        let last_pkg_in_group = by_pkg.last().map(|(n, _)| n.as_str()).unwrap_or("");

        // Split into DDL and DML
        let mut ddl_by_pkg: Vec<(String, Vec<SqlFileInfo>)> = Vec::new();
        let mut dml_by_pkg: Vec<(String, Vec<SqlFileInfo>)> = Vec::new();
        for (pkg_name, pkg_files) in &by_pkg {
            let mut ddl_files = Vec::new();
            let mut dml_files = Vec::new();
            for f in pkg_files {
                let fname = f.full_path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if is_ddl_file(fname) {
                    ddl_files.push(f.clone());
                } else if !is_create_tablespace(fname) {
                    dml_files.push(f.clone());
                }
            }
            if !ddl_files.is_empty() {
                ddl_by_pkg.push((pkg_name.clone(), ddl_files));
            }
            if !dml_files.is_empty() {
                dml_by_pkg.push((pkg_name.clone(), dml_files));
            }
        }

        let mut group_merged = 0;

        if !ddl_by_pkg.is_empty() {
            emit_log(app, "info", "  └─ DDL 文件合并...", None, None);
            let gi = merge_group(
                app, group_key, &packages, &ddl_by_pkg, &output_root,
                first_pkg_in_group, last_pkg_in_group, cancel, "_DDL",
            )?;
            merged_groups.push(gi);
            group_merged += 1;
        }

        if !dml_by_pkg.is_empty() {
            emit_log(app, "info", "  └─ DML 文件合并...", None, None);
            let gi = merge_group(
                app, group_key, &packages, &dml_by_pkg, &output_root,
                first_pkg_in_group, last_pkg_in_group, cancel, "_DML",
            )?;
            merged_groups.push(gi);
            group_merged += 1;
        }

        let elapsed = pkg_start.elapsed();
        emit_log(
            app,
            "success",
            &format!("  ✅ 组完成: {} ({} 个合并文件, 耗时 {:.1}s)", group_key, group_merged, elapsed.as_secs_f64()),
            None,
            Some(elapsed.as_millis() as u64),
        );
        emit_progress(
            app,
            "merging",
            50 + ((group_idx + 1) * 45 / total_groups) as u32,
            group_idx + 1,
            total_groups,
            Some(group_key.clone()),
        );
    }

    // Phase 4b: Copy .sh scripts and .sh directories to their group output directories
    emit_progress(app, "scripts", 95, 0, 0, None);
    let mut script_count = 0usize;
    let all_containers: Vec<PathBuf> = if mode == "spsql" {
        packages.iter().map(|p| p.dir.clone()).collect()
    } else {
        packages.iter().map(|p| p.dir.join("database")).collect()
    };
    let mut sh_files: std::collections::BTreeMap<String, Vec<PathBuf>> = std::collections::BTreeMap::new();
    let mut sh_dirs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for base_dir in &all_containers {
        if !base_dir.is_dir() { continue; }
        let walker = walkdir::WalkDir::new(base_dir).follow_links(false).into_iter();
        for entry in walker {
            if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
            let entry = match entry { Ok(e) => e, Err(_) => continue };
            let path = entry.path();
            let rel = path.strip_prefix(base_dir).unwrap_or(Path::new(""));
            let parent = rel.parent().unwrap_or(Path::new(""));
            let normalized = normalize_dir_path(parent);
            let key = normalized.to_string_lossy().to_string();
            if entry.file_type().is_dir() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.ends_with(".sh") {
                    sh_dirs.push((rel.to_path_buf(), path.to_path_buf()));
                }
            } else if entry.file_type().is_file() {
                if path.extension().and_then(|e| e.to_str()) == Some("sh") {
                    sh_files.entry(key).or_default().push(path.to_path_buf());
                }
            }
        }
    }
    for (group_key, scripts) in &sh_files {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let group_dir = output_root.join(group_key);
        std::fs::create_dir_all(&group_dir)
            .map_err(|e| format!("创建脚本分组目录失败: {}", e))?;
        for script_path in scripts {
            let fname = script_path.file_name().unwrap_or_default();
            let target = group_dir.join(fname);
            std::fs::copy(script_path, &target)
                .map_err(|e| format!("复制脚本失败 {}: {}", script_path.display(), e))?;
            script_count += 1;
        }
    }
    for (rel_path, full_path) in &sh_dirs {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let parent = rel_path.parent().unwrap_or(Path::new(""));
        let normalized = normalize_dir_path(parent);
        let group_key = normalized.to_string_lossy().to_string();
        let group_dir = output_root.join(&group_key);
        let dirname = rel_path.file_name().unwrap_or_default();
        let target = group_dir.join(dirname);
        copy_dir_recursive(full_path, &target)?;
        script_count += 1;
    }
    // Copy entire .sh package directories (e.g. zz_fenbiao.sh/) to their group output directories
    for pkg in &packages {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        if !pkg.name.ends_with(".sh") { continue; }
        let group_key = normalize_dir_path(Path::new(&pkg.container));
        let group_dir = output_root.join(&group_key);
        let target = group_dir.join(&pkg.name);
        if target.exists() {
            std::fs::remove_dir_all(&target)
                .map_err(|e| format!("删除已存在的脚本目录失败 {}: {}", target.display(), e))?;
        }
        copy_dir_recursive(&pkg.dir, &target)?;
        script_count += 1;
    }

    if script_count > 0 {
        emit_log(app, "info", &format!("已复制 {} 个 Shell 脚本/目录到对应分组目录", script_count), None, None);
    }

    // Phase 5: Done
    let total_sql: usize = merged_groups.iter().map(|g| g.total_sql_count).sum();
    let total_duration = overall_start.elapsed();

    emit_progress(app, "done", 100, total_groups, total_groups, None);
    emit_log(
        app,
        "success",
        &format!(
            "🎉 合并完成！共 {} 个合并组, {} 条SQL, 输出到: {}",
            merged_groups.len(),
            total_sql,
            output_root.display(),
        ),
        None,
        Some(total_duration.as_millis() as u64),
    );

    // Write log file
    let log_entries = drain_log();
    let log_path = output_root.join("merge.log");
    write_log_file(&log_entries, &log_path);

    // Write notice file if there are warnings
    let notice_items = drain_notices();
    if !notice_items.is_empty() {
        let notice_content = format!(
            "注意事项 - 请人工核对以下项目\n\
            ================================\n\
            {}\n",
            notice_items.join("\n"),
        );
        let notice_path = output_root.join("注意事项.txt");
        let _ = std::fs::write(&notice_path, &notice_content);
        emit_log(
            app,
            "warn",
            &format!(
                "⚠️ 存在 {} 个注意事项，请查看: {}",
                notice_items.len(),
                notice_path.display(),
            ),
            None,
            None,
        );
    }

    Ok(SqlMergeResult {
        success: true,
        output_dir: output_root.to_string_lossy().to_string(),
        statistics: SqlMergeStatistics {
            packages: packages.len(),
            merged_groups,
            total_sql_count: total_sql,
            total_duration_secs: total_duration.as_secs_f64(),
        },
    })
}

// ── Public API ──

/// 一次合并的入口：校验 → 跑主流程 → 回执。
///
/// 与源仓 `start_sql_merge` 的差别只有**执行方式**：源仓在 Tauri 里必须
/// `std::thread::spawn`，否则长任务会把 WebView 的 IPC 堵住；这里的代码本来就
/// 跑在独立的插件进程里（process 轨），再开线程只会让结果与错误多绕一道。
/// 参数校验、`reset_cancel()`、成功/失败两条分支的文案与源仓逐行一致。
///
/// # Errors
/// 源目录为空 / 不存在、扫描或写入失败、用户取消——原样返回源仓的原因串。
pub fn merge_sql(
    app: &dyn MergeReporter,
    options: SqlMergeOptions,
) -> Result<SqlMergeResult, String> {
    if options.src_dir.is_empty() {
        return Err("源目录不能为空".to_string());
    }

    let src_dir = PathBuf::from(&options.src_dir);
    if !src_dir.exists() {
        return Err(format!("源目录不存在: {}", options.src_dir));
    }

    reset_cancel();

    let start = Instant::now();
    match run_sql_merge(app, &src_dir, &options, cancel_flag()) {
        Ok(stats) => {
            let result = SqlMergeResult {
                success: true,
                output_dir: stats.output_dir.clone(),
                statistics: SqlMergeStatistics {
                    total_duration_secs: start.elapsed().as_secs_f64(),
                    ..stats.statistics
                },
            };
            emit_done(app, &result);
            Ok(result)
        }
        Err(e) => {
            emit_log(app, "error", &e, None, None);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn count_sql_statements_basic() {
        assert_eq!(count_sql_statements("select 1; select 2;"), 2);
        assert_eq!(count_sql_statements("select 1"), 1);
        assert_eq!(count_sql_statements("  ;  ; "), 0);
    }

    #[test]
    fn ddl_and_tablespace_detection() {
        assert!(is_ddl_file("schema_ddl.sql"));
        assert!(is_ddl_file("dbstruct_mysql.sql"));
        assert!(is_ddl_file("patch_oracle.sql"));
        assert!(!is_ddl_file("data_dml.sql"));
        assert!(is_create_tablespace("01_create_tablespace.sql"));
        assert!(!is_create_tablespace("create_table.sql"));
    }

    #[test]
    fn extract_zip_tag_parses_db_prefix() {
        assert_eq!(extract_zip_tag("db-HAXM.zip"), "HAXM");
        assert_eq!(extract_zip_tag("db.zip"), "");
        assert_eq!(extract_zip_tag("db-.zip"), "");
        assert_eq!(extract_zip_tag("other.zip"), "");
    }

    #[test]
    fn normalize_dir_path_skips_version_segment() {
        let p = Path::new("mysql/IFMS3.0V202501.01.001/script");
        let n = normalize_dir_path(p);
        assert_eq!(n, PathBuf::from("mysql/script"));
    }

    #[test]
    fn last_line_sql_comment() {
        assert!(last_line_is_sql_comment("select 1;\n-- done\n"));
        assert!(!last_line_is_sql_comment("select 1;\ncommit;\n"));
    }

    #[test]
    fn detect_merge_mode_standard_empty_dir() {
        let dir = std::env::temp_dir().join(format!("sql_merge_mode_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(detect_merge_mode(&dir), "standard");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_version_segment_matches_ifms_pattern() {
        assert!(is_version_segment("IFMS3.0V202501.01.001"));
        assert!(!is_version_segment("mysql"));
    }

    fn naive_day(d: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2025, 8, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    fn entry(dir: &str, name: &str) -> SqlEntry {
        SqlEntry {
            rel_path: PathBuf::from(format!("{}/{}", dir, name)),
            full_path: PathBuf::from(format!("/tmp/{}/{}/{}", dir, name, name)),
        }
    }

    #[test]
    fn version_key_extracts_max_version_segment() {
        let p = Path::new("mysql/IFMS3.0V202501.01.001/x.sql");
        assert_eq!(version_key_of(p), Some((202501, 1, 1)));
        let p2 = Path::new("IFMS3.0V202501.01.002/IFMS3.0V202501.01.010/a.sql");
        assert_eq!(version_key_of(p2), Some((202501, 1, 10)));
        let p3 = Path::new("plain/dir/a.sql");
        assert_eq!(version_key_of(p3), None);
    }

    #[test]
    fn dedupe_keeps_newest_mtime_same_name_only() {
        // 同名脚本：修改日期新的（002）保留，旧的（001）去重丢弃
        let out = dedupe_keep_newest_by(
            vec![entry("V202501.01.001", "x.sql"), entry("V202501.01.002", "x.sql")],
            |p| {
                if p.to_string_lossy().contains("001") {
                    Some(naive_day(1))
                } else {
                    Some(naive_day(2))
                }
            },
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].rel_path.to_string_lossy().contains("002"));

        // 不同名脚本全部保留（追加合并语义不变）
        let out2 = dedupe_keep_newest_by(
            vec![
                entry("V202501.01.001", "a.sql"),
                entry("V202501.01.002", "b.sql"),
            ],
            |_| None,
        );
        assert_eq!(out2.len(), 2);
    }

    #[test]
    fn dedupe_mtime_tie_falls_back_to_higher_version() {
        // mtime 全部缺失（None）：版本段高者胜
        let out = dedupe_keep_newest_by(
            vec![entry("V202501.01.001", "x.sql"), entry("V202501.01.002", "x.sql")],
            |_| None,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].rel_path.to_string_lossy().contains("002"));

        // mtime 相同：同样版本段高者胜
        let out2 = dedupe_keep_newest_by(
            vec![entry("V202501.01.001", "x.sql"), entry("V202501.01.002", "x.sql")],
            |_| Some(naive_day(1)),
        );
        assert_eq!(out2.len(), 1);
        assert!(out2[0].rel_path.to_string_lossy().contains("002"));
    }

    #[test]
    fn dedupe_mtime_overrides_version_order() {
        // 日期优先于版本号：低版本包里修改日期更新的脚本胜出（"原版本上直接改"场景）
        let out = dedupe_keep_newest_by(
            vec![entry("V202501.01.001", "x.sql"), entry("V202501.01.002", "x.sql")],
            |p| {
                if p.to_string_lossy().contains("001") {
                    Some(naive_day(9))
                } else {
                    Some(naive_day(1))
                }
            },
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].rel_path.to_string_lossy().contains("001"));
    }

    // ─────────────────────────────────────────────────────────────
    //  SP 包合并 SQL 丢失「某个版本的语句」——真实代码路径复现
    //  布局模拟合并后的输出：database/spsql/<模块>/<版本目录>/<脚本>.sql
    // ─────────────────────────────────────────────────────────────

    fn write_sql(path: &Path, body: &str, unix_secs: i64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        let ft = filetime::FileTime::from_unix_time(unix_secs, 0);
        filetime::set_file_mtime(path, ft).unwrap();
    }

    fn collect_merged_sql(root: &Path) -> String {
        // 只统计合并产物（merge_sql_*），不要误把原始脚本算进来
        let mut out = String::new();
        let mut outs: Vec<PathBuf> = Vec::new();
        for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("merge_sql") && e.path().is_dir() {
                outs.push(e.path());
            }
        }
        for dir in outs {
            for e in walkdir::WalkDir::new(&dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if e.file_type().is_file() && e.path().extension().map_or(false, |x| x == "sql") {
                    out.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
                    out.push('\n');
                }
            }
        }
        out
    }

    const MINIS: [&str; 10] = ["003", "004", "005", "006", "007", "008", "009", "010", "011", "012"];

    fn versioned_body(mini: &str) -> String {
        format!(
            "delete from tbsysversion where system_version = 'IFMS6.0V202607.02.{}';\n\
             insert into tbsysversion (system_version) VALUES('IFMS6.0V202607.02.{}');\n\
             commit;\n",
            mini, mini
        )
    }

    fn kept_versions(merged: &str) -> Vec<String> {
        MINIS
            .iter()
            .filter(|m| merged.contains(&format!("IFMS6.0V202607.02.{}", m)))
            .map(|m| m.to_string())
            .collect()
    }

    /// 缺陷复现 1：**同名**脚本放在各自版本目录下（模块 fina 场景）
    /// → 版本目录被 normalize_dir_path 剥掉 ⇒ 同一分组 + 同名 ⇒ 只保留 1 份
    #[test]
    fn repro_same_named_script_across_versions_is_collapsed_to_one() {
        let root = std::env::temp_dir().join(format!("sql_repro_same_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (i, m) in MINIS.iter().enumerate() {
            let p = root
                .join("database/spsql/fina")
                .join(format!("IFMS6.0V202607.02.{}", m))
                .join("tbsysversion.sql");
            write_sql(&p, &versioned_body(m), 1_700_000_000 + i as i64 * 86_400);
        }
        let cancel = AtomicBool::new(false);
        let _ = merge_sql_tree(&root, &cancel).unwrap();
        let kept = kept_versions(&collect_merged_sql(&root));
        println!("[repro-same] 保留版本 = {:?}（共 {} 份）", kept, kept.len());
        assert_eq!(
            kept.len(),
            1,
            "当前行为：跨版本同名脚本被去重成 1 份（这就是丢版本的机制）"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 对照：脚本名**各不相同**（模块 pub 场景）→ 10 份全部保留，与你手上那份 pub 产物一致
    #[test]
    fn control_distinct_names_keep_all_versions() {
        let root = std::env::temp_dir().join(format!("sql_repro_distinct_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (i, m) in MINIS.iter().enumerate() {
            let p = root
                .join("database/spsql/pub")
                .join(format!("IFMS6.0V202607.02.{}", m))
                .join(format!("tbsysversion_{}.sql", m));
            write_sql(&p, &versioned_body(m), 1_700_000_000 + i as i64 * 86_400);
        }
        let cancel = AtomicBool::new(false);
        let _ = merge_sql_tree(&root, &cancel).unwrap();
        let kept = kept_versions(&collect_merged_sql(&root));
        println!("[control-distinct] 保留版本 = {:?}（共 {} 份）", kept, kept.len());
        assert_eq!(kept.len(), 10, "不同名脚本应全部保留");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 缺陷复现 2：留哪一份由 **文件 mtime** 决定 —— 与版本号无关，肉眼极难发现。
    /// 这里只把 003 的 mtime 调到最新，被保留的就变成 003 而不是 012。
    #[test]
    fn repro_survivor_is_decided_by_mtime_not_version() {
        let root = std::env::temp_dir().join(format!("sql_repro_mtime_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (i, m) in MINIS.iter().enumerate() {
            let p = root
                .join("database/spsql/fina")
                .join(format!("IFMS6.0V202607.02.{}", m))
                .join("tbsysversion.sql");
            // 除 003 外 mtime 递增；003 设为最新
            let secs = if *m == "003" {
                1_900_000_000
            } else {
                1_700_000_000 + i as i64 * 86_400
            };
            write_sql(&p, &versioned_body(m), secs);
        }
        let cancel = AtomicBool::new(false);
        let _ = merge_sql_tree(&root, &cancel).unwrap();
        let kept = kept_versions(&collect_merged_sql(&root));
        println!("[repro-mtime] 保留版本 = {:?}", kept);
        assert_eq!(kept, vec!["003".to_string()], "胜出者取决于 zip 内 mtime");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 缺陷复现 3：与用户反馈完全同形——**只有部分版本同名**时，丢的正是较旧的那个版本。
    /// 这里 003 与 004 的 fina 脚本同名（004 的 mtime 更新），其余版本各带版本后缀：
    /// 结果保留 004~012、丢失 003，即「fina 里没有 03 版本的语句，但 pub 里有」。
    #[test]
    fn repro_partial_name_collision_drops_only_the_older_version() {
        let root = std::env::temp_dir().join(format!("sql_repro_partial_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (i, m) in MINIS.iter().enumerate() {
            let fname = if *m == "003" || *m == "004" {
                "tbsysversion.sql".to_string()
            } else {
                format!("tbsysversion_{}.sql", m)
            };
            let p = root
                .join("database/spsql/fina")
                .join(format!("IFMS6.0V202607.02.{}", m))
                .join(fname);
            write_sql(&p, &versioned_body(m), 1_700_000_000 + i as i64 * 86_400);
        }
        let cancel = AtomicBool::new(false);
        let _ = merge_sql_tree(&root, &cancel).unwrap();
        let kept = kept_versions(&collect_merged_sql(&root));
        println!("[repro-partial] 保留版本 = {:?}", kept);
        assert_eq!(
            kept[0], "004",
            "003 与 004 同名 → 保留 mtime 较新的 004，003 被静默丢弃"
        );
        assert!(!kept.contains(&"003".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 附带的两个**静默跳过**点（同样会让某个版本的语句整段消失，且日志里什么都没有）：
    /// ① 扩展名大写 `.SQL` —— merge_sql_tree 的大小写敏感判断会直接 continue；
    /// ② 文件名含 `create_tablespace` —— 既不入 DDL 也不入 DML，被整份丢掉。
    #[test]
    fn silent_skip_vectors_uppercase_ext_and_tablespace() {
        let root = std::env::temp_dir().join(format!("sql_repro_skip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let base = root.join("database/spsql/fina");
        // 003 扩展名大写 → 被 continue 掉
        write_sql(
            &base.join("IFMS6.0V202607.02.003/tbsysversion.SQL"),
            &versioned_body("003"),
            1_700_000_000,
        );
        // 004 命中 create_tablespace 规则 → 两个桶都不进
        write_sql(
            &base.join("IFMS6.0V202607.02.004/01_create_tablespace.sql"),
            &versioned_body("004"),
            1_700_000_100,
        );
        // 005 起正常命名（名字各不相同，避免彼此去重）
        for (i, m) in MINIS.iter().filter(|m| **m > "004").enumerate() {
            write_sql(
                &base
                    .join(format!("IFMS6.0V202607.02.{}", m))
                    .join(format!("tbsysversion_{}.sql", m)),
                &versioned_body(m),
                1_700_000_200 + i as i64 * 86_400,
            );
        }
        let cancel = AtomicBool::new(false);
        let _ = merge_sql_tree(&root, &cancel).unwrap();
        let kept = kept_versions(&collect_merged_sql(&root));
        println!("[repro-skip] 保留版本 = {:?}", kept);
        assert_eq!(kept, vec!["005", "006", "007", "008", "009", "010", "011", "012"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
