//! 上线包（SP / IFMS6）合并：扫描 → 解析 → 排序 → 分组 → 解压合并 → 服务合并 →
//! SQL 合并 → upd 合并 → 版本回退核对。
//!
//! **本文件逐字复刻**自参考仓 `ifm_ai_tools/src-tauri/src/sp_merge.rs`
//! （1,460 行，含 12 条上游单测，已在生产使用中验证）。移植只改**接线**，
//! 逻辑一行未动——改动清单如下（与 `duck-sql-core/src/sql_merge.rs` 同一套规矩）：
//!
//! 1. 去掉 `use tauri::{AppHandle, Emitter}`；三个 `emit_*` 的载荷不变，出口从
//!    Tauri 广播换成 [`PkgMergeReporter`] 的三个同名方法。
//! 2. 签名上的 `&AppHandle` 一维换成 `&dyn PkgMergeReporter`（`run_merge` 一处，
//!    调用点零改动）。
//! 3. 入口 `start_merge(app, options) -> Result<(), String>`（内部
//!    `thread::spawn`）改为 [`merge`]（同步 + 直接返回 [`MergeResult`]）——插件
//!    本身已在独立进程里，线程包装只会把错误多绕一道。
//! 4. **一处 bug 修复**（唯一的逻辑改动，用户口径「复刻不复研」的例外，理由见下）：
//!    `app/Documents` ↔ `app/documents` 的搬移与「清理残留」在**大小写不敏感**的
//!    文件系统（macOS 默认 APFS、Windows NTFS）上是同一个目录，源仓在那里会自己
//!    删自己——实测 macOS 上包里的 `documents/` 整块消失。改为先判 `same_dir()`：
//!    同一目录时只做「仅改大小写的改名」，清理残留也跳过。判据见 [`same_dir`]。
//!    Linux（ext4，大小写敏感）上两条路径分支都与源仓完全一致。
//!
//! ⚠️ **取消**：源仓用 `CANCEL_FLAG` 全局原子 + Tauri 命令置位。本仓的取消走
//! 宿主侧进程管理（`duck-plugin-process::CancelHandle`：写 `{"type":"cancel"}`，
//! 2 秒不退就 `SIGTERM`）。`CANCEL_FLAG` 与 `cancel()` / `reset_cancel()` 原样
//! 保留——插件进程收不到 cancel 行时由 `SIGTERM` 终止，行为等价；将来若给插件
//! 进程加控制行监听，这里直接能用，不必再改内核。

// clippy 豁免逐条列名、**只为本文件服务**：这些都是「改写法才消失」的建议，而改
// 写法正是移植要避免的成本（`collapsible_if` 的正确写法要 Rust 1.88 的 let_chains，
// 本仓 MSRV 是 1.85）。
#![allow(clippy::collapsible_if,
    clippy::if_same_then_else,
    clippy::too_many_arguments)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use chrono::NaiveDate;
use regex::Regex;
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::merge_common;
use crate::upd_merge;
use duck_sql_core::sql_merge;

/// 合并过程的对外回执（源仓 `emit_progress` / `emit_log` / `emit_done` 的替身）。
///
/// 三个方法逐字段对应源仓的 Tauri 事件载荷，一个都不多不少：
/// [`progress`](Self::progress) 的 `stage_duration` 是源仓 `stage_duration_ms`。
/// 事件怎么落地是插件的事（process 轨写 stdout 的 `log` / `progress`，还要先过
/// 译文表），所以内核这层只留 trait。
pub trait PkgMergeReporter {
    /// 阶段进度。`stage` 是源仓的英文阶段名（`scanning` / `parsing` / `merging` /
    /// `service_merge` / `sql_merge` / `upd_merge` / `done`）。
    fn progress(
        &self,
        stage: &str,
        percent: u32,
        current: usize,
        total: usize,
        current_file: Option<String>,
        stage_duration_ms: Option<u64>,
    );
    /// 一行过程日志。`level`：`info` / `warn` / `error` / `success`。
    fn log(&self, level: &str, message: &str, duration_ms: Option<u64>);
    /// 完成回执（成功与失败都会调，失败时 `success = false`）。
    fn done(&self, result: &MergeResult);
}

// ── Global Cancel Flag ──

static CANCEL_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn cancel_flag() -> &'static Arc<AtomicBool> {
    CANCEL_FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

// Re-export for Tauri commands in lib.rs
pub fn cancel() {
    cancel_flag().store(true, Ordering::SeqCst);
}

pub fn reset_cancel() {
    cancel_flag().store(false, Ordering::SeqCst);
}

// ── Data Structures ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeOptions {
    pub src_dir: String,
    pub dst_dir: String,
    pub version_filter: String,
    /// 是否启用 `database/` 的「保持原结构 + 追加合并」：为真时 database/ 下的
    /// 同名文件以**先落盘者为准**，后续包不再覆盖（默认 true）。
    pub include_database: bool,
    /// 是否启用 `documents/` 的「保持原结构 + 追加合并」，并额外把源目录的
    /// `documents/` 合并进输出（默认 true）。
    pub include_documents: bool,
    pub include_sql_merge: Option<bool>,
    pub include_service_merge: Option<bool>,
    /// 是否合并 reqsql 下的 `.upd` 说明文件（按包顺序追加为 `merge_upd.xml`）
    pub include_upd_merge: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeResult {
    pub success: bool,
    pub output_dir: String,
    pub statistics: MergeStatistics,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeStatistics {
    pub packages: usize,
    pub files_copied: usize,
    pub files_overwritten: usize,
    pub total_duration_ms: u64,
}

#[derive(Debug, Clone)]
struct SpPackage {
    path: PathBuf,
    filename: String,
    product: String,
    version: String,
    monthversion: String,
    bigversion: u8,
    miniversion: u16,
    push_date: NaiveDate,
    /// Entry prefix to strip from ZIP entries, e.g. "IFMS3.0V202501.08.028/"
    entry_prefix: String,
    is_v6: bool,
    /// V6 module name (METAL, PUB, DXCP, etc.), empty for 3.0/4.0
    v6_module: String,
}

// ── Constants ──

// 业务子模块目录：这些顶层目录只作「壳」，其内容会被剥掉一层并归入输出的 app/ 下。
// trust（信托）与 base/fund 等同为业务子模块，其下同样是 ifm* 应用目录，
// 剥壳后与其它业务模块的同名服务在 app/ 下聚合。
const BUSINESS_DIRS: &[&str] = &[
    "base",
    "bfina",
    "fund",
    "fina",
    "lcdx",
    "pm",
    "am",
    "transfer",
    "trust",
];
const SERVICE_DIRS_3X: &[&str] = &["ifmbatch", "ifmcounter", "ifmmanage", "ifmonline"];

fn package_regex_v3() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([A-Z]+)([\d\.]+)V(\d{6})-(\d{2})-(\d{3})-(\d{8})\.zip$").expect("valid regex")
    })
}

fn package_regex_v6() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^IFMS6\.0-([A-Z]+)-V(\d{6})-(\d{2})-(\d{3})\.zip$").expect("valid regex")
    })
}

fn package_regex_v6_simple() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^IFMS6\.0V(\d{6})-(\d{2})-(\d{3})\.zip$").expect("valid regex")
    })
}

// ── Package Parsing ──

fn parse_package(path: &Path) -> Option<SpPackage> {
    let filename = path.file_name()?.to_str()?;
    // Try V3/4 format first
    if let Some(caps) = package_regex_v3().captures(filename) {
        let product = caps[1].to_string();
        let version = caps[2].to_string();
        let monthversion = caps[3].to_string();
        let bigversion: u8 = caps[4].parse().ok()?;
        let miniversion: u16 = caps[5].parse().ok()?;
        let push_date = NaiveDate::parse_from_str(&caps[6], "%Y%m%d").ok()?;
        let entry_prefix = format!(
            "{}{}V{}.{:02}.{:03}/",
            product, version, monthversion, bigversion, miniversion
        );
        return Some(SpPackage {
            path: path.to_path_buf(),
            filename: filename.to_string(),
            product,
            version,
            monthversion,
            bigversion,
            miniversion,
            push_date,
            entry_prefix,
            is_v6: false,
            v6_module: String::new(),
        });
    }
    // Try V6 format (with module name, e.g. IFMS6.0-PUB-V202607-00-000.zip)
    if let Some(caps) = package_regex_v6().captures(filename) {
        let v6_module = caps[1].to_string();
        let monthversion = caps[2].to_string();
        let bigversion: u8 = caps[3].parse().ok()?;
        let miniversion: u16 = caps[4].parse().ok()?;
        let entry_prefix = format!("IFMS6.0V{}.{:02}.{:03}/", monthversion, bigversion, miniversion);
        return Some(SpPackage {
            path: path.to_path_buf(),
            filename: filename.to_string(),
            product: "IFMS".into(),
            version: "6.0".into(),
            monthversion,
            bigversion,
            miniversion,
            push_date: NaiveDate::from_ymd_opt(9999, 12, 31).expect("valid date"),
            entry_prefix,
            is_v6: true,
            v6_module,
        });
    }
    // Try V6 simple format (no module name, e.g. IFMS6.0V202506-06-010.zip)
    if let Some(caps) = package_regex_v6_simple().captures(filename) {
        let monthversion = caps[1].to_string();
        let bigversion: u8 = caps[2].parse().ok()?;
        let miniversion: u16 = caps[3].parse().ok()?;
        let entry_prefix = format!("IFMS6.0V{}.{:02}.{:03}/", monthversion, bigversion, miniversion);
        return Some(SpPackage {
            path: path.to_path_buf(),
            filename: filename.to_string(),
            product: "IFMS".into(),
            version: "6.0".into(),
            monthversion,
            bigversion,
            miniversion,
            push_date: NaiveDate::from_ymd_opt(9999, 12, 31).expect("valid date"),
            entry_prefix,
            is_v6: true,
            v6_module: String::new(),
        });
    }
    None
}

// ── Sorting ──

impl Ord for SpPackage {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.monthversion
            .cmp(&other.monthversion)
            .then(self.bigversion.cmp(&other.bigversion))
            .then(self.miniversion.cmp(&other.miniversion))
            .then(self.push_date.cmp(&other.push_date))
    }
}

impl PartialOrd for SpPackage {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SpPackage {
    fn eq(&self, other: &Self) -> bool {
        self.monthversion == other.monthversion
            && self.bigversion == other.bigversion
            && self.miniversion == other.miniversion
            && self.push_date == other.push_date
    }
}

impl Eq for SpPackage {}

// ── Scan ──

pub fn scan_versions(src_dir: &str) -> Result<Vec<String>, String> {
    let dir = Path::new(src_dir);
    if !dir.is_dir() {
        return Err("目录不存在".to_string());
    }
    let mut pkgs = scan_packages(dir);
    pkgs.sort();
    Ok(pkgs.into_iter().map(|p| p.filename).collect())
}

fn scan_packages(src_dir: &Path) -> Vec<SpPackage> {
    let mut pkgs = Vec::new();
    let mut stack = vec![src_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("zip") {
                    if let Some(pkg) = parse_package(&path) {
                        pkgs.push(pkg);
                    }
                }
            }
        }
    }
    pkgs
}

// ── Path Resolution ──

fn resolve_output(entry_name: &str, keep_dirs: &[&str]) -> PathBuf {
    let clean = entry_name.trim_start_matches("./").trim_start_matches('/');
    let entry = Path::new(clean);

    let first = entry
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .unwrap_or("");

    if BUSINESS_DIRS.contains(&first) {
        let rest: PathBuf = entry.components().skip(1).collect();
        return Path::new("app").join(rest);
    }

    if keep_dirs.contains(&first) {
        return entry.to_path_buf();
    }

    entry.to_path_buf()
}

// ── Emit Helpers ──

fn emit_progress(
    reporter: &dyn PkgMergeReporter,
    stage: &str,
    percent: u32,
    current: usize,
    total: usize,
    current_file: Option<String>,
    stage_duration: Option<u64>,
) {
    reporter.progress(stage, percent, current, total, current_file, stage_duration);
}

fn emit_log(reporter: &dyn PkgMergeReporter, level: &str, message: &str, duration_ms: Option<u64>) {
    reporter.log(level, message, duration_ms);
}

fn emit_done(reporter: &dyn PkgMergeReporter, result: &MergeResult) {
    reporter.done(result);
}

// ── Modification-Date Based Conflict Resolution ──
// 版本号相同的包之间，"在原版本上直接改"的场景不叠版本号，
// 同名文件以 ZIP 条目的修改日期为准，取日期最新的。

/// ZIP 条目的 DOS 时间 → NaiveDateTime（DOS 时间无时区，按本地时间语义比较）。
/// 解析失败返回 None（调用方回退为"按包版本顺序覆盖"的旧行为）。
fn dos_to_naive(dt: zip::DateTime) -> Option<chrono::NaiveDateTime> {
    if dt.month() == 0 || dt.day() == 0 {
        return None;
    }
    let date = chrono::NaiveDate::from_ymd_opt(
        dt.year() as i32,
        dt.month() as u32,
        dt.day() as u32,
    )?;
    date.and_hms_opt(dt.hour() as u32, dt.minute() as u32, dt.second() as u32)
}

/// 同名文件冲突裁决：双方均有有效修改时间时取较新者；
/// 任一方缺时间信息则回退为"后包覆盖"（版本顺序，兼容旧行为）。
fn should_overwrite(new_t: Option<chrono::NaiveDateTime>, old_t: Option<chrono::NaiveDateTime>) -> bool {
    match (new_t, old_t) {
        (Some(n), Some(o)) => n > o,
        _ => true,
    }
}

/// 将解压出的文件 mtime 保留为 ZIP 条目的原始修改时间，
/// 供后续 SQL 脚本合并（merge_sql_tree）按日期去重使用。
fn preserve_entry_mtime(path: &Path, t: chrono::NaiveDateTime) {
    if let Some(lt) = t.and_local_timezone(chrono::Local).single() {
        let ft = filetime::FileTime::from_unix_time(lt.timestamp(), lt.timestamp_subsec_nanos());
        let _ = filetime::set_file_mtime(path, ft);
    }
}

// ── Version Regression Detection ──
// During extraction, check if overwritten .class files have a LOWER version → regression.

fn class_major_version(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 8 || bytes[0] != 0xCA || bytes[1] != 0xFE || bytes[2] != 0xBA || bytes[3] != 0xBE {
        return None;
    }
    Some(u16::from_be_bytes([bytes[6], bytes[7]]))
}

static REGRESSIONS: OnceLock<Mutex<Vec<(String, u16, u16)>>> = OnceLock::new();
fn regression_list() -> &'static Mutex<Vec<(String, u16, u16)>> {
    REGRESSIONS.get_or_init(|| Mutex::new(Vec::new()))
}

fn check_class_regression(full_path: &Path, new_bytes: &[u8]) {
    let old_bytes = match std::fs::read(full_path) {
        Ok(b) => b,
        Err(_) => return,
    };
    let old_ver = match class_major_version(&old_bytes) {
        Some(v) => v,
        None => return,
    };
    let new_ver = match class_major_version(new_bytes) {
        Some(v) => v,
        None => return,
    };
    if new_ver < old_ver {
        let _ = regression_list().lock().map(|mut list| {
            list.push((full_path.to_string_lossy().to_string(), old_ver, new_ver));
        });
    }
}

fn write_regression_report(output_root: &Path) -> Result<(), String> {
    let list = regression_list().lock().map_err(|e| e.to_string())?;
    if list.is_empty() { return Ok(()); }
    let report = output_root.join("版本回退核对清单.txt");
    let mut content = String::from("文件路径\t原版本\t新版本\n");
    for (path, old, new) in list.iter() {
        content.push_str(&format!("{}\t{}\t{}\n", path, old, new));
    }
    std::fs::write(&report, &content)
        .map_err(|e| format!("写入回退清单失败: {}", e))?;
    Ok(())
}

// ── Extract Single Package ──

/// `.upd` 说明文件判定：扩展名大小写不敏感，且路径中不得含以 `.` 开头的段
/// （跳过 macOS 资源分叉 `__MACOSX/._x.upd`、编辑器临时文件等）。
fn is_upd_path(path: &Path) -> bool {
    let is_upd = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("upd"));
    if !is_upd {
        return false;
    }
    !path
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
}

/// 登记一个 `.upd` 说明文件条目。
///
/// **正文随登记即入内存**：`content` 就是刚从 ZIP 条目里读出来的字节，因此条目
/// 的有效性与「输出目录里的这个文件后来是否被搬走/删除」无关 —— 服务合并阶段
/// 会把 `database/`、`documents/` 搬进 `app/`，若在合并阶段才回读路径，
/// 就会报「N 个 upd 源文件不存在」。
///
/// **同一相对路径只保留一份**：跨包重复投递/返工时按修改日期取最新，与
/// `extract_package` 对落盘文件的覆盖裁决（`should_overwrite`）保持同一口径，
/// 二者要么都替换、要么都不替换。
fn push_upd_entry(
    upd_out: &mut Vec<upd_merge::UpdEntry>,
    dest_path: &Path,
    source_pkg: &str,
    full_path: &Path,
    content: &[u8],
    mtime: Option<chrono::NaiveDateTime>,
) {
    if !is_upd_path(dest_path) {
        return;
    }
    let display = dest_path.to_string_lossy().replace('\\', "/");
    match upd_out.iter_mut().find(|e| e.display_path == display) {
        Some(prev) => {
            if should_overwrite(mtime, prev.mtime) {
                prev.src = full_path.to_path_buf();
                prev.source_pkg = source_pkg.to_string();
                prev.mtime = mtime;
                prev.content = content.to_vec();
            }
        }
        None => upd_out.push(upd_merge::UpdEntry {
            display_path: display,
            source_pkg: source_pkg.to_string(),
            src: full_path.to_path_buf(),
            mtime,
            content: content.to_vec(),
        }),
    }
}

fn extract_package(
    pkg: &SpPackage,
    output_root: &Path,
    keep_dirs: &[&str],
    cancel: &AtomicBool,
    // For V6: if true, extract front/ as well; if false, skip front/ (only take from latest module package)
    extract_front: bool,
    // 已写入条目的修改时间表（按输出相对路径），用于同名文件按日期取最新
    mtime_map: &mut std::collections::HashMap<PathBuf, chrono::NaiveDateTime>,
    // 是否收集 `reqsql` 下的 `.upd` 说明文件（供可选的 upd 合并阶段使用）
    collect_upd: bool,
    // 已收集的 `.upd` 条目（按包处理顺序累积）
    upd_out: &mut Vec<upd_merge::UpdEntry>,
) -> Result<(usize, usize, usize), String> {
    let pkg_source = pkg.filename.trim_end_matches(".zip").to_string();
    let file = std::fs::File::open(&pkg.path).map_err(|e| format!("无法打开包文件: {}", e))?;
    let mut archive =
        ZipArchive::new(file).map_err(|e| format!("无法读取ZIP: {}", e))?;

    let mut copied = 0usize;
    let mut overwritten = 0usize;
    let mut kept_older = 0usize;

    // Strip the version directory prefix from ZIP entries, e.g. "IFMS3.0V202501.08.028/"
    let prefix = &pkg.entry_prefix;

    for i in 0..archive.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.is_dir() {
            continue;
        }

        // 条目名统一走 merge_common 的归一化：反斜杠 → `/`（Windows 侧打包可能
        // 写出 `\`），且非 UTF-8 时按 GBK 解码（`entry.name()` 会走 cp437 出乱码）。
        let entry_name = merge_common::decode_entry_name(entry.name_raw());

        if entry_name.contains("..") {
            continue;
        }

        let stripped = entry_name.strip_prefix(prefix).unwrap_or(&entry_name);

        // V6 front/ is full-release: only extract from the latest package of each module
        if pkg.is_v6 && !extract_front && (stripped.starts_with("front/") || stripped == "front") {
            continue;
        }

        // V6: extract flat (no resolve_output), with version-prefix handling
        let dest_path = if pkg.is_v6 {
            // If stripping produced no change (no version prefix in zip),
            // or what remains still starts with the version dir, extract as-is
            let as_path = Path::new(stripped);
            let first = as_path.components().next()
                .and_then(|c| c.as_os_str().to_str()).unwrap_or("");
            // Check if the entry still starts with a version-like dir (nested prefix case)
            if first.starts_with("IFMS") && first.contains('V') {
                // Double-nested: IFMS6.0-PUB-V202607-00-000/IFMS6.0V202607.00.000/front/...
                // Strip one more level
                let rest: PathBuf = as_path.components().skip(1).collect();
                rest
            } else {
                as_path.to_path_buf()
            }
        } else {
            resolve_output(stripped, keep_dirs)
        };
        let full_path = output_root.join(&dest_path);

        let is_dedup = keep_dirs
            .iter()
            .any(|d| Some(*d) == dest_path.components().next().and_then(|c| c.as_os_str().to_str()));

        let exists = full_path.exists();

        if exists && is_dedup {
            continue;
        }

        let entry_mtime = entry.last_modified().and_then(dos_to_naive);

        if exists && !is_dedup {
            // 同名文件：修改日期新的才覆盖，旧的跳过（版本相同的包可能存在
            // "在原版本上直接改、不叠版本号"的更新代码，不能只按包版本顺序）
            if should_overwrite(entry_mtime, mtime_map.get(&dest_path).copied()) {
                overwritten += 1;
            } else {
                kept_older += 1;
                continue;
            }
        }

        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
        }

        // Read all entry bytes, then check regression for .class files
        let mut entry_bytes = Vec::new();
        entry.read_to_end(&mut entry_bytes)
            .map_err(|e| format!("读取ZIP条目失败: {}", e))?;

        if exists && entry_name.ends_with(".class") {
            check_class_regression(&full_path, &entry_bytes);
        }

        let mut outfile =
            std::fs::File::create(&full_path).map_err(|e| format!("创建文件失败: {}", e))?;
        std::io::copy(&mut entry_bytes.as_slice(), &mut outfile)
            .map_err(|e| format!("写入文件失败: {}", e))?;
        drop(outfile);

        // 保留 ZIP 条目原始修改时间，供后续 SQL 脚本按日期去重
        if let Some(t) = entry_mtime {
            preserve_entry_mtime(&full_path, t);
            mtime_map.insert(dest_path.clone(), t);
        }
        copied += 1;

        // 收集 reqsql 下的 .upd 说明文件，供可选的 upd 合并阶段按包顺序追加。
        // 正文此刻就在手上（`entry_bytes`），直接入内存，不再让后续阶段依赖
        // 「输出目录里这个路径还在不在」。
        if collect_upd {
            push_upd_entry(upd_out, &dest_path, &pkg_source, &full_path, &entry_bytes, entry_mtime);
        }
    }

    Ok((copied, overwritten, kept_older))
}

// ── Grouping ──

fn group_packages(pkgs: &[SpPackage]) -> Vec<(String, Vec<SpPackage>)> {
    let mut groups: std::collections::BTreeMap<String, Vec<SpPackage>> =
        std::collections::BTreeMap::new();
    for pkg in pkgs.iter().cloned() {
        // V6: all modules merge into one output dir (like Java mergeV6)
        let key = if pkg.is_v6 {
            format!("{}-{}", pkg.product, pkg.version)
        } else {
            format!("{}-{}", pkg.product, pkg.version)
        };
        groups.entry(key).or_default().push(pkg);
    }
    groups.into_iter().collect()
}

// ── Service Merge ──
// After resolve_output(), everything is in app/{svc} or at root (database/documents).
// The final structure should be:
//   app/{ifmbatch,ifmcounter,ifmmanage,ifmonline,database,documents}/
//   ifmpub/jres → copied into ALL services' WEB-INF/lib/

fn merge_services(output_dir: &Path, cancel: &AtomicBool) -> Result<usize, String> {
    let app_dir = output_dir.join("app");
    if !app_dir.is_dir() {
        return Ok(0);
    }

    let mut total = 0usize;

    // 1. Copy contents of ifmpub into each service's WEB-INF/lib/
    //    (目录本身不复制，只复制里面的内容)
    let ifmpub_dir = app_dir.join("ifmpub");
    if ifmpub_dir.is_dir() {
        for svc in SERVICE_DIRS_3X {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }
            let svc_dir = app_dir.join(svc);
            if !svc_dir.is_dir() {
                continue;
            }
            let lib_dir = svc_dir.join("WEB-INF").join("lib");
            std::fs::create_dir_all(&lib_dir)
                .map_err(|e| format!("创建 lib 目录失败: {}", e))?;
            copy_contents_to(&ifmpub_dir, &lib_dir, cancel)
                .map_err(|e| format!("复制 ifmpub 到 {} 失败: {}", svc, e))?;
            total += 1;
        }
        std::fs::remove_dir_all(&ifmpub_dir)
            .map_err(|e| format!("删除 ifmpub 失败: {}", e))?;
    }

    // 2. Copy jres/lib/* and jres/lib-V2/* into each service's WEB-INF/lib/
    //    (jres 本身可能含 lib/ 和 lib-V2/ 子目录，只取里面的 jar 到 lib/)
    let jres_dir = app_dir.join("jres");
    if jres_dir.is_dir() {
        for jres_sub in &["lib", "lib-V2"] {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }
            let jres_sub_dir = jres_dir.join(jres_sub);
            if !jres_sub_dir.is_dir() {
                continue;
            }
            for svc in SERVICE_DIRS_3X {
                let svc_dir = app_dir.join(svc);
                if !svc_dir.is_dir() {
                    continue;
                }
                let lib_dir = svc_dir.join("WEB-INF").join("lib");
                std::fs::create_dir_all(&lib_dir)
                    .map_err(|e| format!("创建 lib 目录失败: {}", e))?;
                copy_contents_to(&jres_sub_dir, &lib_dir, cancel)
                    .map_err(|e| format!("复制 jres/{}/ 到 {} 失败: {}", jres_sub, svc, e))?;
                total += 1;
            }
        }
        std::fs::remove_dir_all(&jres_dir)
            .map_err(|e| format!("删除 jres 失败: {}", e))?;
    }

    // 2. Move database/ and documents/ from output root into app/ (merge if exists)
    for keep in &["database", "documents"] {
        let src = output_dir.join(keep);
        if src.is_dir() {
            let target = app_dir.join(keep);
            if target.exists() {
                copy_contents_to(&src, &target, cancel)
                    .map_err(|e| format!("合并 {} 到 app/ 失败: {}", keep, e))?;
                std::fs::remove_dir_all(&src)
                    .map_err(|e| format!("删除源 {} 失败: {}", keep, e))?;
            } else {
                std::fs::rename(&src, &target)
                    .map_err(|e| format!("移动 {} 到 app/ 失败: {}", keep, e))?;
            }
            total += 1;
        }
    }

    // 3. Remove leftover app/Documents/ if still empty (from base module doc extraction)
    //
    // ⚠️ 与源仓的**唯一一处**行为差异（bug 修复，见模块头）：这一步在大小写不敏感的
    // 文件系统上是**数据丢失**——`app/Documents` 与上一步刚搬进来的 `app/documents`
    // 是同一条记录，`remove_dir_all` 会把 documents/ 整块删掉（实测 macOS 上
    // `documents/说明.docx` 消失）。所以只有两者确实是两个不同目录时才清残留。
    let docs_remnant = app_dir.join("Documents");
    let docs_kept = app_dir.join("documents");
    if !same_dir(&docs_remnant, &docs_kept) && docs_remnant.is_dir() {
        let _ = std::fs::remove_dir_all(&docs_remnant);
    }

    Ok(total)
}

/// 两个路径是否指向**同一个目录**。
///
/// 用途只有一个：识别「大小写不敏感的文件系统上 `a/Documents` == `a/documents`」
/// 这种同记录异写法的情形（macOS 默认 APFS、Windows NTFS 都是）。任何一方不存在
/// 时返回 `false`——那种情况下调用方自己的 `is_dir()` 判断会接管。
fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Copy the *contents* of src into dest (not src itself).
fn copy_contents_to(src: &Path, dest: &Path, cancel: &AtomicBool) -> Result<(), String> {
    if !src.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(src)
        .map_err(|e| format!("读取目录失败 {}: {}", src.display(), e))?
        .flatten()
    {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }
        let path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dest.join(&file_name);

        if path.is_dir() {
            if dest_path.exists() && dest_path.is_dir() {
                // Merge sub-directory contents
                copy_contents_to(&path, &dest_path, cancel)?;
            } else {
                copy_dir_all(&path, &dest_path)?;
            }
        } else if !dest_path.exists() {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
            }
            std::fs::copy(&path, &dest_path)
                .map_err(|e| format!("复制文件失败 {} -> {}: {}", path.display(), dest_path.display(), e))?;
        }
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest)
        .map_err(|e| format!("创建目录失败 {}: {}", dest.display(), e))?;
    for entry in std::fs::read_dir(src)
        .map_err(|e| format!("读取目录失败 {}: {}", src.display(), e))?
        .flatten()
    {
        let path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dest.join(&file_name);
        if path.is_dir() {
            copy_dir_all(&path, &dest_path)?;
        } else {
            std::fs::copy(&path, &dest_path)
                .map_err(|e| format!("复制文件失败 {} -> {}: {}", path.display(), dest_path.display(), e))?;
        }
    }
    Ok(())
}

/// Rename SQL files in merge_sql_{ts}/ directories to include version range.
/// e.g. base_DDL.sql → 202501-00-000_202501-03-000-DDL-base.sql
fn rename_merge_sql_files(output_dir: &Path, prefix: &str, cancel: &AtomicBool) -> Result<(), String> {
    for entry in std::fs::read_dir(output_dir)
        .map_err(|e| format!("读取输出目录失败: {}", e))?
        .flatten()
    {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("merge_sql") || !path.is_dir() {
            continue;
        }
        // Walk merge_sql subdirectories for .sql files
        rename_sql_in_dir(&path, prefix, cancel)?;
    }
    Ok(())
}

fn rename_sql_in_dir(dir: &Path, prefix: &str, cancel: &AtomicBool) -> Result<(), String> {
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("读取 SQL 目录失败: {}", e))?
        .flatten()
    {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            rename_sql_in_dir(&path, prefix, cancel)?;
        } else if name.ends_with(".sql") && !name.starts_with(prefix) {
            let ext = if name.contains("DDL") { "DDL" } else if name.contains("DML") { "DML" } else { "SQL" };
            let tag = name.strip_suffix("_DDL.sql").or_else(|| name.strip_suffix("_DML.sql"))
                .or_else(|| name.strip_suffix(".sql")).unwrap_or(&name);
            let new_name = format!("{}-{}-{}.sql", prefix, ext, tag);
            let new_path = path.with_file_name(&new_name);
            if new_path.exists() {
                let _ = std::fs::remove_file(&new_path);
            }
            std::fs::rename(&path, &new_path)
                .map_err(|e| format!("重命名 SQL 文件失败 {} → {}: {}", path.display(), new_path.display(), e))?;
        }
    }
    Ok(())
}

// ── Main Merge Logic ──

struct MergeStats {
    output_dir: String,
    packages: usize,
    files_copied: usize,
    files_overwritten: usize,
}

fn run_merge(
    reporter: &dyn PkgMergeReporter,
    src_dir: &Path,
    dst_dir: &Path,
    options: &MergeOptions,
    cancel: &AtomicBool,
) -> Result<MergeStats, String> {
    // Phase 1: Scan
    emit_progress(reporter, "scanning", 3, 0, 0, None, None);
    emit_log(reporter, "info", &format!("开始扫描目录: {}", src_dir.display()), None);

    let mut pkgs = scan_packages(src_dir);
    if pkgs.is_empty() {
        return Err("未找到有效的 SP 包文件".to_string());
    }
    emit_log(reporter, "info", &format!("扫描到 {} 个 ZIP 文件", pkgs.len()), None);

    // Filter by version
    if options.version_filter != "all" {
        pkgs.retain(|p| p.version == options.version_filter);
    }
    if pkgs.is_empty() {
        return Err("未找到匹配版本的有效包文件".to_string());
    }

    // Phase 2: Parse
    emit_progress(reporter, "parsing", 10, pkgs.len(), pkgs.len(), None, None);
    emit_log(reporter, "info", &format!("解析完成，有效包: {}", pkgs.len()), None);

    // Phase 3: Sort
    pkgs.sort();

    let first = &pkgs[0];
    let last = &pkgs[pkgs.len() - 1];
    emit_log(
        reporter,
        "info",
        &format!(
            "排序完成，版本范围: {}-{:02}-{:03} → {}-{:02}-{:03}",
            first.monthversion,
            first.bigversion,
            first.miniversion,
            last.monthversion,
            last.bigversion,
            last.miniversion,
        ),
        None,
    );

    // Phase 4: Group
    let groups = group_packages(&pkgs);
    emit_log(reporter, "info", &format!("分组完成，共 {} 个分组", groups.len()), None);

    // Reset regression list for this merge run
    let _ = regression_list().lock().map(|mut list| list.clear());

    let mut all_outputs: Vec<PathBuf> = Vec::new();
    let mut group_ranges: Vec<String> = Vec::new();
    let mut total_copied = 0usize;
    let mut total_overwritten = 0usize;

    // 「保持原结构 + 追加合并（保留所有文件）」的目录：这些目录下的文件一旦落盘，
    // 后续包的同名文件不再覆盖（见 extract_package 的 is_dedup 分支）。
    // 依据设计文档《P包解析与合并插件》§3.3：KEEP_DIRS = ["database", "documents"]。
    let mut keep_dirs: Vec<&str> = Vec::new();
    if options.include_database {
        keep_dirs.push("database");
    }
    if options.include_documents {
        keep_dirs.push("documents");
    }

    // 各分组收集到的 `.upd` 说明文件（按包处理顺序累积），供可选的 upd 合并阶段使用
    let mut group_upd_entries: Vec<(PathBuf, Vec<upd_merge::UpdEntry>)> = Vec::new();

    for (group_idx, (group_key, group_pkgs)) in groups.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let group_progress = 15 + ((group_idx as u32) * 80 / groups.len() as u32);

        emit_progress(reporter, "merging", group_progress, group_idx + 1, groups.len(), None, None);
        emit_log(
            reporter,
            "info",
            &format!("开始合并分组 {} ({} 个包)", group_key, group_pkgs.len()),
            None,
        );

        let first_pkg = &group_pkgs[0];
        let last_pkg = &group_pkgs[group_pkgs.len() - 1];
        let range = format!(
            "{}-{:02}-{:03}_{}-{:02}-{:03}",
            first_pkg.monthversion,
            first_pkg.bigversion,
            first_pkg.miniversion,
            last_pkg.monthversion,
            last_pkg.bigversion,
            last_pkg.miniversion,
        );
        group_ranges.push(range.clone());
        let output_dir_name = format!("{}_{}-{}", group_key, first_pkg.miniversion, last_pkg.miniversion);
        let output_path = dst_dir.join(&output_dir_name);

        if output_path.exists() {
            std::fs::remove_dir_all(&output_path)
                .map_err(|e| format!("清理旧输出目录失败: {}", e))?;
        }
        std::fs::create_dir_all(&output_path)
            .map_err(|e| format!("创建输出目录失败: {}", e))?;

        // For V6 front/ is full-release: only extract front/ from the latest package per module
        let mut v6_module_last_idx: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (idx, pkg) in group_pkgs.iter().enumerate() {
            if pkg.is_v6 {
                v6_module_last_idx.insert(&pkg.v6_module, idx);
            }
        }

        let mut mtime_map: std::collections::HashMap<PathBuf, chrono::NaiveDateTime> =
            std::collections::HashMap::new();
        let mut group_kept_older = 0usize;

        // 本分组内收集的 `.upd` 说明文件（按包处理顺序），供可选的 upd 合并阶段使用
        let collect_upd = options.include_upd_merge.unwrap_or(false);
        let mut upd_entries: Vec<upd_merge::UpdEntry> = Vec::new();

        for (pkg_idx, pkg) in group_pkgs.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }

            let pkg_progress = group_progress
                + ((pkg_idx as u32) * 80 / (groups.len() as u32 * group_pkgs.len() as u32).max(1));

            emit_progress(
                reporter,
                "merging",
                pkg_progress.min(95),
                pkg_idx + 1,
                group_pkgs.len(),
                Some(pkg.filename.clone()),
                None,
            );

            let extract_front = if pkg.is_v6 {
                v6_module_last_idx.get(pkg.v6_module.as_str()) == Some(&pkg_idx)
            } else {
                true
            };

        let pkg_start = Instant::now();
            match extract_package(
                pkg,
                &output_path,
                &keep_dirs,
                cancel,
                extract_front,
                &mut mtime_map,
                collect_upd,
                &mut upd_entries,
            ) {
                Ok((copied, overwritten, kept_older)) => {
                    total_copied += copied;
                    total_overwritten += overwritten;
                    group_kept_older += kept_older;
                    let elapsed_ns = pkg_start.elapsed().as_nanos();
                    let elapsed_ms = (elapsed_ns / 1_000_000) as u64;
                    emit_log(
                        reporter,
                        "info",
                        &format!(
                            "处理包 {}/{}: {} (耗时 {:.1}s)",
                            pkg_idx + 1,
                            group_pkgs.len(),
                            pkg.filename,
                            elapsed_ms as f64 / 1000.0,
                        ),
                        Some(elapsed_ms),
                    );
                }
                Err(e) => {
                    emit_log(reporter, "warn", &format!("跳过包 {}: {}", pkg.filename, e), None);
                }
            }
        }

        if group_kept_older > 0 {
            emit_log(
                reporter,
                "info",
                &format!(
                    "分组 {} 同名文件按修改日期裁决：跳过 {} 个日期较旧的，保留日期最新的",
                    group_key, group_kept_older
                ),
                None,
            );
        }

        if collect_upd {
            if !upd_entries.is_empty() {
                emit_log(
                    reporter,
                    "info",
                    &format!(
                        "分组 {} 收集到 {} 个 .upd 说明文件",
                        group_key,
                        upd_entries.len()
                    ),
                    None,
                );
            }
            group_upd_entries.push((output_path.clone(), std::mem::take(&mut upd_entries)));
        }

        emit_log(
            reporter,
            "info",
            &format!("分组 {} 合并完毕，输出目录: {}", group_key, output_path.display()),
            None,
        );

        all_outputs.push(output_path);
    }

    // ── Copy source documents/ and move base/Documents/ to each output ──
    for output_path in &all_outputs {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        // 1. Copy loose source documents/ to output
        if options.include_documents {
            let src_docs = src_dir.join("documents");
            if src_docs.is_dir() {
                let target = output_path.join("documents");
                if target.exists() {
                    copy_contents_to(&src_docs, &target, cancel)
                        .map_err(|e| format!("合并 documents 失败: {}", e))?;
                } else {
                    copy_dir_all(&src_docs, &target)
                        .map_err(|e| format!("复制 documents 失败: {}", e))?;
                }
                emit_log(reporter, "info", &format!("源目录 documents/ 已合并到 {}", output_path.display()), None);
            }
        }

        // 2. Move base/Documents/ from app/ to app/documents/ if it exists
        //
        // ⚠️ 与源仓的**唯一一处**行为差异（bug 修复，见模块头）：大小写不敏感的文件
        // 系统（macOS 默认 APFS、Windows NTFS）上 `app/Documents` 与 `app/documents`
        // **是同一条目录记录**，源仓的「先删目标 → 再复制 → 再删源」三步在这里是
        // 自己删自己：先 `remove_dir_all(app/documents)` 把刚解压出来的文档删掉，
        // 再 `copy_dir_all(自身, 自身)` 因为源已空而什么也复制不到，最后再删一次。
        // 实测 macOS 上 `base/Documents/` 整块消失。改成「只改大小写的原地改名」。
        let docs_src = output_path.join("app").join("Documents");
        if docs_src.is_dir() {
            let docs_dst = output_path.join("app").join("documents");
            if same_dir(&docs_src, &docs_dst) {
                // 同一个目录：把名字改成小写即可（APFS / NTFS 都支持仅改大小写的改名）。
                let _ = std::fs::rename(&docs_src, &docs_dst);
            } else {
                if docs_dst.exists() {
                    std::fs::remove_dir_all(&docs_dst)
                        .map_err(|e| format!("删除旧 app/documents/ 失败: {}", e))?;
                }
                if let Some(parent) = docs_dst.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("创建 app/ 目录失败: {}", e))?;
                }
                copy_dir_all(&docs_src, &docs_dst)
                    .map_err(|e| format!("复制 Documents 到 documents 失败: {}", e))?;
                std::fs::remove_dir_all(&docs_src)
                    .map_err(|e| format!("删除源 Documents 失败: {}", e))?;
            }
        }
    }

    let main_output = all_outputs
        .last()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // ── Phase 5: Service Merge (optional, 3.0/4.0) ──
    if options.include_service_merge.unwrap_or(false) {
        emit_progress(reporter, "service_merge", 85, 1, 1, None, None);
        emit_log(reporter, "info", "开始服务目录合并 (ifmbatch/ifmcounter/ifmmanage/ifmonline)...", None);
        let svc_start = Instant::now();

        for (g_idx, output_path) in all_outputs.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
            let count = merge_services(output_path, cancel)?;
            let range = group_ranges.get(g_idx).map(|r| r.as_str()).unwrap_or("");
            emit_log(reporter, "info", &format!("  服务合并 {}: {} 个目录已处理", range, count), None);
        }

        let svc_elapsed = svc_start.elapsed().as_millis() as u64;
        emit_log(reporter, "info", &format!("服务目录合并完成，耗时 {:.1}s", svc_elapsed as f64 / 1000.0), Some(svc_elapsed));
    }

    // ── Phase 6: SQL Merge (optional) ──
    if options.include_sql_merge.unwrap_or(false) {
        emit_progress(reporter, "sql_merge", 95, 1, 1, None, None);
        emit_log(reporter, "info", "开始合并 SQL 文件...", None);
        let sql_start = Instant::now();

        for (g_idx, output_path) in all_outputs.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
            let count = sql_merge::merge_sql_tree(output_path, cancel)?;
            emit_log(reporter, "info", &format!("  SQL 合并: {} 个合并组", count), None);

            // Rename SQL files with version range: 起始包-终止包-DDL/DML.sql
            if count > 0 {
                let range = group_ranges.get(g_idx).map(|r| r.as_str()).unwrap_or("");
                if !range.is_empty() {
                    let prefix = range.replace('_', "-");
                    rename_merge_sql_files(output_path, &prefix, cancel)?;
                }
            }
        }

        let sql_elapsed = sql_start.elapsed().as_millis() as u64;
        emit_log(reporter, "info", &format!("SQL 文件合并完成，耗时 {:.1}s", sql_elapsed as f64 / 1000.0), Some(sql_elapsed));
    }

    // ── Phase 7: UPD Merge (optional) ──
    // 把各分组内收集到的 `.upd` 说明文件按**包处理顺序**追加合并为
    // `<分组输出目录>/merge_upd.xml`：每段正文前后写 beg/end 注释标记，正文原样
    // 保留，编码统一转 UTF-8。
    //
    // 正文在解压阶段就已随条目读入内存（见 `push_upd_entry`），本阶段**不读盘**，
    // 因此放在服务合并/SQL 合并之后是安全的：`database/`、`documents/` 被搬进
    // `app/`、`unzip/` 之类的临时目录被清理都不会让本阶段失败。
    //
    // 历史缺陷（勿回退）：早期版本在这里按路径回读源文件，而 Phase 5
    // `merge_services()` 会把输出根的 `database/`、`documents/` 搬进 `app/`
    // 并删掉 `app/ifmpub`、`app/jres`，于是 `reqsql` 落在这些子树里的 `.upd`
    // 全部「不存在」，报错与操作系统无关（macOS 上同样会中招，只是多为直接
    // 在 Windows 上跑这批包才暴露出来）。
    if options.include_upd_merge.unwrap_or(false) {
        emit_progress(reporter, "upd_merge", 97, 1, 1, None, None);
        let upd_start = Instant::now();
        let mut total_upd_files = 0usize;

        for (output_path, entries) in &group_upd_entries {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }

            let group_name = output_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            if entries.is_empty() {
                emit_log(
                    reporter,
                    "info",
                    &format!("  {}: 未发现 .upd 说明文件，跳过", group_name),
                    None,
                );
                continue;
            }

            let output_file = output_path.join("merge_upd.xml");
            let summary = upd_merge::merge_upd_entries(entries, &output_file, cancel)?;

            for r in &summary.records {
                if r.encoding != "UTF-8" {
                    emit_log(
                        reporter,
                        "info",
                        &format!("  编码转换: {} ({} → UTF-8)", r.display_path, r.encoding),
                        None,
                    );
                }
            }

            total_upd_files += summary.files;
            emit_log(
                reporter,
                "info",
                &format!(
                    "  {}: {} 个 .upd → merge_upd.xml ({:.1} KB)",
                    group_name,
                    summary.files,
                    summary.total_bytes as f64 / 1024.0
                ),
                None,
            );
        }

        let upd_elapsed = upd_start.elapsed().as_millis() as u64;
        emit_log(
            reporter,
            "info",
            &format!(
                "UPD 说明文件合并完成: 共 {} 个文件，耗时 {:.1}s",
                total_upd_files,
                upd_elapsed as f64 / 1000.0
            ),
            Some(upd_elapsed),
        );
    }

    // ── Phase 8: Version Regression Report ──
    for output_path in &all_outputs {
        if let Err(e) = write_regression_report(output_path) {
            emit_log(reporter, "warn", &format!("写入版本回退清单失败: {}", e), None);
        }
    }

    Ok(MergeStats {
        output_dir: main_output,
        packages: pkgs.len(),
        files_copied: total_copied,
        files_overwritten: total_overwritten,
    })
}

// ── Start Merge ──

/// 同步执行一次合并，返回最终统计（失败时**也**已经通过回执把错误播出去了）。
///
/// 源仓这里是 `start_merge(app, options) -> Result<(), String>` + 内部
/// `thread::spawn`：WebView 侧只能靠事件知道结果。插件进程本身就是一次性的
/// 子进程，线程包装没有收益，所以改成同步返回——**校验顺序、事件顺序、载荷
/// 全都没变**（校验 → `reset_cancel` → `run_merge` → success/error 日志 →
/// `done` 回执）。
///
/// # Errors
/// 参数校验失败、目录不可用、或合并中途出错（`Ok` 分支的统计来自
/// [`run_merge`]）。出错时 `done` 回执里是 `success = false` 的空统计。
pub fn merge(
    reporter: &dyn PkgMergeReporter,
    options: &MergeOptions,
) -> Result<MergeResult, String> {
    let result = merge_inner(reporter, options);
    if let Err(error) = &result {
        // 与源仓一致：失败也要写一条 error 日志 + 一帧 failed 回执。
        emit_log(reporter, "error", error, None);
        emit_done(
            reporter,
            &MergeResult {
                success: false,
                output_dir: String::new(),
                statistics: MergeStatistics {
                    packages: 0,
                    files_copied: 0,
                    files_overwritten: 0,
                    total_duration_ms: 0,
                },
            },
        );
    }
    result
}

fn merge_inner(
    reporter: &dyn PkgMergeReporter,
    options: &MergeOptions,
) -> Result<MergeResult, String> {
    if options.src_dir.is_empty() || options.dst_dir.is_empty() {
        return Err("源目录和目标目录不能为空".to_string());
    }

    let src_dir = PathBuf::from(&options.src_dir);
    if !src_dir.exists() {
        return Err(format!("源目录不存在: {}", options.src_dir));
    }

    let dst_dir = PathBuf::from(&options.dst_dir);
    if !dst_dir.exists() {
        std::fs::create_dir_all(&dst_dir)
            .map_err(|e| format!("创建目标目录失败: {}", e))?;
    }

    reset_cancel();

    let start = Instant::now();
    let stats = run_merge(reporter, &src_dir, &dst_dir, options, cancel_flag())?;
    let elapsed = start.elapsed().as_millis() as u64;
    let result = MergeResult {
        success: true,
        output_dir: stats.output_dir.clone(),
        statistics: MergeStatistics {
            packages: stats.packages,
            files_copied: stats.files_copied,
            files_overwritten: stats.files_overwritten,
            total_duration_ms: elapsed,
        },
    };
    emit_log(
        reporter,
        "success",
        &format!("合并完成！总耗时: {:.1}s", elapsed as f64 / 1000.0),
        Some(elapsed),
    );
    emit_progress(reporter, "done", 100, stats.packages, stats.packages, None, Some(elapsed));
    emit_done(reporter, &result);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_package_v3() {
        let path = PathBuf::from("/tmp/IFMS3.0V202501-01-001-20250115.zip");
        let pkg = parse_package(&path).expect("v3 parse");
        assert_eq!(pkg.product, "IFMS");
        assert_eq!(pkg.version, "3.0");
        assert_eq!(pkg.monthversion, "202501");
        assert_eq!(pkg.bigversion, 1);
        assert_eq!(pkg.miniversion, 1);
        assert!(!pkg.is_v6);
    }

    #[test]
    fn parse_package_v6_with_module() {
        let path = PathBuf::from("/tmp/IFMS6.0-PUB-V202607-00-000.zip");
        let pkg = parse_package(&path).expect("v6 parse");
        assert!(pkg.is_v6);
        assert_eq!(pkg.v6_module, "PUB");
        assert_eq!(pkg.monthversion, "202607");
    }

    #[test]
    fn parse_package_v6_simple() {
        let path = PathBuf::from("/tmp/IFMS6.0V202506-06-010.zip");
        let pkg = parse_package(&path).expect("v6 simple");
        assert!(pkg.is_v6);
        assert_eq!(pkg.v6_module, "");
        assert_eq!(pkg.miniversion, 10);
    }

    #[test]
    fn parse_package_rejects_noise() {
        assert!(parse_package(Path::new("/tmp/readme.txt")).is_none());
        assert!(parse_package(Path::new("/tmp/random.zip")).is_none());
    }

    #[test]
    fn class_major_version_from_cafe_babe() {
        let mut bytes = vec![0u8; 8];
        bytes[0] = 0xCA;
        bytes[1] = 0xFE;
        bytes[2] = 0xBA;
        bytes[3] = 0xBE;
        bytes[6] = 0x00;
        bytes[7] = 0x37; // 55
        assert_eq!(class_major_version(&bytes), Some(55));
        assert_eq!(class_major_version(&[0, 1, 2]), None);
    }

    #[test]
    fn resolve_output_maps_business_and_keep_dirs() {
        let p = resolve_output("fund/lib/a.jar", &["docs"]);
        assert_eq!(p, PathBuf::from("app/lib/a.jar"));
        let p2 = resolve_output("docs/readme.txt", &["docs"]);
        assert_eq!(p2, PathBuf::from("docs/readme.txt"));
        let p3 = resolve_output("./docs/a.txt", &["docs"]);
        assert_eq!(p3, PathBuf::from("docs/a.txt"));
    }

    /// trust 子模块与 base/fund 等业务目录同规则：剥掉 trust 壳后归入 app/，
    /// 使 trust 下的 ifm* 应用与其它业务模块的同名服务在 app/ 下聚合，
    /// 并被后续服务合并（ifmpub/jres 公库分发）一并处理。
    #[test]
    fn resolve_output_strips_trust_module_shell() {
        let p = resolve_output("trust/ifmcounter/WEB-INF/web.xml", &[]);
        assert_eq!(p, PathBuf::from("app/ifmcounter/WEB-INF/web.xml"));

        let p2 = resolve_output("./trust/ifmbatch/com/hundsun/A.class", &[]);
        assert_eq!(p2, PathBuf::from("app/ifmbatch/com/hundsun/A.class"));

        // 非 trust 的其它未登记顶层目录仍保持原样（不误伤）
        let p3 = resolve_output("other/x.txt", &[]);
        assert_eq!(p3, PathBuf::from("other/x.txt"));
    }

    fn naive(y: i32, m: u32, d: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    #[test]
    fn should_overwrite_prefers_newer_mtime() {
        let older = naive(2025, 8, 1);
        let newer = naive(2025, 8, 2);
        assert!(should_overwrite(Some(newer), Some(older)));
        assert!(!should_overwrite(Some(older), Some(newer)));
        // 任一方缺时间信息 → 回退为按包版本顺序覆盖（旧行为）
        assert!(should_overwrite(Some(older), None));
        assert!(should_overwrite(None, Some(newer)));
        assert!(should_overwrite(None, None));
    }

    #[test]
    fn dos_to_naive_parses_and_rejects_garbage() {
        let dt = zip::DateTime::default();
        assert_eq!(dos_to_naive(dt), Some(naive(1980, 1, 1)));
    }

    #[test]
    fn is_upd_path_accepts_upd_and_rejects_hidden() {
        assert!(is_upd_path(Path::new("reqsql/lfyh/a.upd")));
        assert!(is_upd_path(Path::new("app/reqsql/A.UPD")));
        assert!(!is_upd_path(Path::new("reqsql/lfyh/a.sql")));
        assert!(!is_upd_path(Path::new("reqsql/lfyh/note.txt")));
        // macOS 资源分叉与隐藏目录内的 .upd 一律不收
        assert!(!is_upd_path(Path::new("__MACOSX/._a.upd")));
        assert!(!is_upd_path(Path::new("reqsql/.tmp/a.upd")));
    }

    #[test]
    fn push_upd_entry_carries_content_and_keeps_newest() {
        let root = PathBuf::from("/tmp/sp_upd_entry");
        let mut out: Vec<upd_merge::UpdEntry> = Vec::new();
        let older = naive(2026, 1, 1);
        let newer = naive(2026, 6, 1);

        push_upd_entry(&mut out, Path::new("reqsql/lfyh/a.upd"), "PKG_A", &root.join("a.upd"), b"AAA", Some(older));
        push_upd_entry(&mut out, Path::new("reqsql/lfyh/b.upd"), "PKG_A", &root.join("b.upd"), b"BBB", Some(older));
        // 非 .upd 文件不入列
        push_upd_entry(&mut out, Path::new("reqsql/lfyh/c.sql"), "PKG_B", &root.join("c.sql"), b"CCC", Some(newer));

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].display_path, "reqsql/lfyh/a.upd");
        assert_eq!(out[1].display_path, "reqsql/lfyh/b.upd");
        assert_eq!(out[0].content, b"AAA", "正文必须随登记即入内存");

        // 同一相对路径、修改日期更新 → 一并替换正文与来源包
        // （与 extract_package 对落盘文件的 should_overwrite 裁决同一口径）
        push_upd_entry(&mut out, Path::new("reqsql/lfyh/a.upd"), "PKG_B", &root.join("a.upd"), b"AAA-NEW", Some(newer));
        assert_eq!(out.len(), 2, "同一相对路径仍只保留一条");
        assert_eq!(out[0].source_pkg, "PKG_B");
        assert_eq!(out[0].content, b"AAA-NEW");

        // 同一相对路径、修改日期更旧 → 保留先落盘的那一份
        push_upd_entry(
            &mut out,
            Path::new("reqsql/lfyh/a.upd"),
            "PKG_C",
            &root.join("a.upd"),
            b"AAA-OLD",
            Some(naive(2025, 1, 1)),
        );
        assert_eq!(out[0].source_pkg, "PKG_B");
        assert_eq!(out[0].content, b"AAA-NEW");
    }
    // ── 以下为本仓新增（源仓没有这条用例）────────────────────────────────
    //
    // 回归锁：`app/Documents` ↔ `app/documents` 在大小写不敏感的文件系统上是同一条
    // 记录。源仓的「删目标 → 复制 → 删源」与「清理残留」两步在那里会把自己删空，
    // 于是包里的 `documents/` 整块消失（macOS 实测）。这条用例造一个同时含
    // `documents/` 与 `base/Documents/` 的包，跑完整合并后要求两边的文档都在。

    struct Silent;

    impl PkgMergeReporter for Silent {
        fn progress(&self, _stage: &str, _percent: u32, _current: usize, _total: usize, _file: Option<String>, _ms: Option<u64>) {}
        fn log(&self, _level: &str, _message: &str, _ms: Option<u64>) {}
        fn done(&self, _result: &MergeResult) {}
    }

    #[test]
    fn merge_keeps_documents_on_case_insensitive_filesystems() {
        use std::io::Write as _;

        let root = std::env::temp_dir().join(format!("sp_merge_docs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let pkgs = root.join("pkgs");
        let out = root.join("out");
        std::fs::create_dir_all(&pkgs).unwrap();

        // 一个包，带三样：根 `documents/`（走 keep_dirs）、`base/Documents/`（走
        // 剥壳 → app/Documents → 改名）、`database/`（服务合并阶段搬进 app/）。
        let prefix = "IFMS3.0V202501.01.001/";
        let file = std::fs::File::create(pkgs.join("IFMS3.0V202501-01-001-20250115.zip")).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in [
            (format!("{prefix}documents/说明.docx"), "DOC-NEW"),
            (format!("{prefix}base/Documents/readme.txt"), "DOC-BASE"),
            (format!("{prefix}database/oracle/01_create.sql"), "CREATE TABLE t (id INT);"),
            (format!("{prefix}base/ifmonline/WEB-INF/web.xml"), "<web-app/>"),
        ] {
            zw.start_file(name, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        zw.finish().unwrap();

        let options = MergeOptions {
            src_dir: pkgs.to_string_lossy().to_string(),
            dst_dir: out.to_string_lossy().to_string(),
            version_filter: "all".into(),
            include_database: true,
            include_documents: true,
            include_sql_merge: Some(false),
            include_service_merge: Some(true),
            include_upd_merge: Some(false),
        };
        let result = merge(&Silent, &options).expect("合并应当成功");
        assert!(result.success);

        let merged = out.join("IFMS-3.0_1-1");
        // ① 包内的 documents/ 必须还在（曾被服务合并的「清理残留」整块删掉）
        assert!(
            merged.join("app/documents/说明.docx").is_file(),
            "包内 documents/ 被删了：{:?}",
            std::fs::read_dir(merged.join("app")).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
        );
        // ② base/Documents/ 应当改名为 documents/ 并保留正文
        assert_eq!(
            std::fs::read_to_string(merged.join("app/documents/readme.txt")).unwrap(),
            "DOC-BASE"
        );
        // ③ 根 database/ 照旧搬进 app/
        assert!(merged.join("app/database/oracle/01_create.sql").is_file());

        let _ = std::fs::remove_dir_all(&root);
    }
}
