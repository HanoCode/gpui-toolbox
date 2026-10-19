//! 非 SEE 上线包合并（`NonSpMerge.vue`）：解压 → 服务目录归并 → SQL 合并 →
//! 需求单合并 → 冲突报表。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/non_sp_merge.rs`（1,936 行，
//! 含上游 10 条单测）。改动只有接线，与 `sp_merge` 同一套：
//!
//! 1. `use tauri::{AppHandle, Emitter}` → 去掉；三个 `emit_*` 的出入口换成
//!    [`crate::MergeReporter`]（`app` 这个**参数名**故意留着，见下）。
//! 2. `&AppHandle` → `&dyn MergeReporter`：`run_merge` 的签名一行。
//! 3. `start_merge`（内部 `thread::spawn` + 事件回执）→ [`merge`]（同步返回
//!    [`NonSpMergeResult`]）。
//! 4. 版本号工具从参考仓的 `class_compare` 改为独立的 `duck-classfile` crate
//!    （同一份代码，见那个 crate 的文档）。
//!
//! **为什么参数还叫 `app`**：本文件有 60 多处 `emit_x(app, …)` 调用，改名会让
//! 与参考仓的逐行 `diff` 全部错位。类型已经是 `&dyn MergeReporter`，名字保留
//! 只是为了让「除了签名那一行，其余一行未改」这件事一眼可验。

#![allow(clippy::collapsible_if,
    clippy::too_many_arguments,
    clippy::manual_strip,
    // 报告表格的那段 doc 注释是复刻件原样带过来的换行方式，重排会让逐行 diff 失效。
    clippy::doc_lazy_continuation)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use encoding_rs::GBK;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use zip::ZipArchive;

use crate::MergeReporter;
use crate::merge_common;
use crate::upd_merge;
use duck_classfile::version_compare;
use duck_classfile::version_extractor::VersionExtractor;
use duck_sql_core::sql_merge;

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

pub fn scan_packages(src_dir: &str) -> Result<Vec<String>, String> {
    let dir = Path::new(src_dir);
    if !dir.is_dir() {
        return Err("目录不存在".to_string());
    }
    let mut zip_files: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("读取目录失败: {}", e))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("zip") {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                zip_files.push(name.to_string());
            }
        }
    }
    zip_files.sort();
    Ok(zip_files)
}

// ── Data Structures ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NonSpMergeOptions {
    pub src_dir: String,
    pub dst_dir: String,
    pub include_sql_merge: Option<bool>,
    pub include_upd_merge: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NonSpMergeResult {
    pub success: bool,
    pub output_dir: String,
    pub statistics: NonSpMergeStatistics,
}

#[derive(Debug, Clone, Serialize)]
pub struct NonSpMergeStatistics {
    pub packages: usize,
    pub files_copied: usize,
    pub total_duration_ms: u64,
}

// ── Emit Helpers ──
//
// 源仓这里直接 `app.emit("non-sp-merge-progress", json)`。载荷字段一一对应搬到
// [`MergeReporter`] 上；落地方式（写 stdout 事件流、过译文表）归插件。

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

fn emit_log(app: &dyn MergeReporter, level: &str, message: &str) {
    app.log(level, message, None);
}

// ── ZIP Handling ──

fn decode_entry_name(raw: &[u8]) -> String {
    if let Ok(name) = std::str::from_utf8(raw) {
        name.replace('\\', "/")
    } else {
        let (decoded, _, _) = GBK.decode(raw);
        decoded.replace('\\', "/")
    }
}

fn unzip_to(zip_path: &Path, dest_dir: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("无法打开文件 {}: {}", zip_path.display(), e))?;
    let mut archive =
        ZipArchive::new(file).map_err(|e| format!("无法读取ZIP {}: {}", zip_path.display(), e))?;

    for i in 0..archive.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let entry_name = decode_entry_name(entry.name_raw());

        if entry_name.contains("..") {
            continue;
        }

        let dest_path = dest_dir.join(&entry_name);

        // ZIP 条目的原始修改时间：解压后会写回磁盘 mtime，作为后续
        // 「同名文件按修改日期取最新」裁决的唯一数据源（否则全是解压时刻）。
        let entry_mtime = entry.last_modified().and_then(merge_common::dos_to_naive);

        if entry.is_dir() {
            std::fs::create_dir_all(&dest_path)
                .map_err(|e| format!("创建目录失败 {}: {}", dest_path.display(), e))?;
        } else {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
            }
            let mut outfile = std::fs::File::create(&dest_path)
                .map_err(|e| format!("创建文件失败 {}: {}", dest_path.display(), e))?;
            std::io::copy(&mut entry, &mut outfile)
                .map_err(|e| format!("写入文件失败 {}: {}", dest_path.display(), e))?;
            drop(outfile);
            if let Some(t) = entry_mtime {
                merge_common::set_file_mtime(&dest_path, t);
            }
        }
    }

    Ok(())
}

// ── Version Extraction from JSP Files ──

/// Extract version from JSP file header comment.
/// Looks for pattern: @version X.X.X.X in the JSP comment block.
/// Example:
/// <%--      @system  综合理财平台
///     @version 3.0.0.13
///     @2019-12-27
/// --%>
fn extract_jsp_version(content: &str) -> Option<String> {
    // Extract the JSP header comment block (<%-- ... --%>)
    // Look for the first comment block at the start of the file
    let header_block = extract_jsp_header_comment(content);

    for line in header_block.lines() {
        let trimmed = line.trim();
        if let Some(idx) = trimmed.find("@version") {
            let version_str = &trimmed[idx + 8..];
            let version = version_str.trim();
            // Validate it looks like a version number (contains digits and dots)
            if !version.is_empty()
                && version.chars().any(|c| c.is_ascii_digit())
                && version
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.')
            {
                return Some(version.to_string());
            }
        }
    }
    None
}

/// Extract the JSP header comment block (<%-- ... --%>) from the beginning of the file.
/// Returns the content between <%-- and --%>, or the first 2000 chars if no closing tag found.
fn extract_jsp_header_comment(content: &str) -> &str {
    // Find opening tag
    let start = match content.find("<%--") {
        Some(pos) => pos + 4, // Skip past "<%--"
        None => return &content[..content.len().min(2000)], // No JSP comment found, return beginning
    };

    // Find closing tag
    match content[start..].find("--%>") {
        Some(pos) => &content[start..start + pos],
        None => {
            // No closing tag, return up to 2000 chars from start of comment
            let end = (start + 2000).min(content.len());
            &content[start..end]
        }
    }
}

// ── 同名文件冲突裁决 ──

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClassConflict {
    rel_path: String,
    /// 冲突类别：Class / JSP/MV / 其他
    kind: String,
    existing_version: String,
    incoming_version: String,
    /// 实际生效的裁决依据：版本号 / 修改日期
    rule: String,
    winner: String,
    source_pkg: String,
}

/// ifmpub 公库条目。需求单包中的 ifmpub 目录下同时存在两种形态：
/// 1. 独立 JAR 文件（lib 包），如 `com.hundsun.lcpt.constant-1.0.0.1.jar`；
/// 2. 解压后的 JAR 目录（lib 解压包），如 `com.hundsun.lcpt.business.trust-1.0.0.1/`。
/// 两者都要按统一规则分发到每个 ifm* 服务的 `WEB-INF/lib/` 下。
#[derive(Debug, Clone)]
struct PubEntry {
    name: String,
    src: PathBuf,
    is_dir: bool,
    source_pkg: String,
}

/// 完整服务清单。ifmpub 公库必须落到以下每一个服务下；
/// 需求单包通常只包含其中一两个服务，缺失的由本工具自动创建补齐。
const CANONICAL_IFM_SERVICES: [&str; 4] = ["ifmmanage", "ifmonline", "ifmbatch", "ifmcounter"];

/// 公库分发目标服务：ifm* 服务本身，排除 ifmpub（公库自身）以及
/// reqsql / modal / banksql 这类纯 SQL、配置目录。
fn is_pub_target_service(name: &str) -> bool {
    name.starts_with("ifm") && name != "ifmpub"
}

/// 计算公库分发目标服务列表，返回 `(服务名, 是否为补齐新建)`。
///
/// 先把 4 个标准服务按固定顺序全部纳入（包内缺失的标记为「补齐」），
/// 再追加包内出现的其他 ifm* 服务（未来新增服务的兜底，不算补齐）。
/// 调用方据此创建缺失服务目录并分发公库，避免只含一两个服务的需求单包
/// 导致其余服务的 `WEB-INF/lib` 缺少公库。
fn resolve_pub_target_services(discovered: &[String]) -> Vec<(String, bool)> {
    let mut result: Vec<(String, bool)> = CANONICAL_IFM_SERVICES
        .iter()
        .map(|name| {
            let present = discovered.iter().any(|d| d.as_str() == *name);
            (name.to_string(), !present)
        })
        .collect();

    for name in discovered {
        if is_pub_target_service(name) && !result.iter().any(|(s, _)| s == name) {
            result.push((name.clone(), false));
        }
    }

    result
}

/// 同名文件冲突裁决结果。
struct ConflictDecision {
    /// true = 用新包文件覆盖已合并文件
    overwrite: bool,
    /// 实际生效的裁决依据："版本号" / "修改日期"
    rule: &'static str,
}

/// 冲突类别（用于报告分组与可读性）。
fn conflict_kind(ext: &str) -> &'static str {
    match ext {
        "class" => "Class",
        "jsp" | "mv" => "JSP/MV",
        _ => "其他",
    }
}

/// 宽松解码文本（UTF-8 优先，回退 GBK）。
///
/// 交付包中的 JSP/MV 常见 GBK 编码；若直接 `read_to_string` 会返回 Err 并中断
/// 整个合并流程，这里改为「尽力解码，解不出即视为无版本号」。
fn decode_text_lossy(bytes: &[u8]) -> Option<String> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Some(s.to_string());
    }
    let (decoded, _, _) = GBK.decode(bytes);
    Some(decoded.to_string())
}

/// 提取文件的版本号：Class 取字节码常量池；JSP / MV 取文件头 `@version` 注释；
/// 其余类型没有版本号概念，返回 None。
fn version_of_file(path: &Path, ext: &str) -> Option<String> {
    match ext {
        "class" => {
            let bytes = std::fs::read(path).ok()?;
            let ver = VersionExtractor::extract(&bytes);
            if ver == "-1" {
                None
            } else {
                Some(ver)
            }
        }
        "jsp" | "mv" => {
            let bytes = std::fs::read(path).ok()?;
            let text = decode_text_lossy(&bytes)?;
            extract_jsp_version(&text)
        }
        _ => None,
    }
}

fn fmt_mtime(t: Option<chrono::NaiveDateTime>) -> String {
    match t {
        Some(v) => v.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => "-".to_string(),
    }
}

/// 同名文件冲突裁决（SP 合并 / 需求单合并统一规则）：
/// 1. 双方都能提取到版本号且不同 → 保留版本号更高的一方；
/// 2. 版本号相同，或**双方都没有版本号** → 按文件修改日期取最新；
/// 3. 日期缺失或相同、无法区分 → 保留先合并的一方（结果可复现）。
fn decide_conflict(src: &Path, dst: &Path, ext: &str) -> ConflictDecision {
    let src_ver = version_of_file(src, ext);
    let dst_ver = version_of_file(dst, ext);

    if let (Some(s), Some(d)) = (&src_ver, &dst_ver) {
        let cmp = version_compare::compare(s, d);
        if cmp != 0 {
            return ConflictDecision {
                overwrite: cmp > 0,
                rule: "版本号",
            };
        }
    }

    ConflictDecision {
        overwrite: merge_common::newer_mtime(
            merge_common::file_mtime(src),
            merge_common::file_mtime(dst),
        ),
        rule: "修改日期",
    }
}

/// 合并单个文件：目标不存在直接复制；已存在则按 `decide_conflict` 裁决并记录
/// 一条冲突明细。返回是否真正写入了目标文件。
fn merge_with_conflict_check(
    src_file: &Path,
    dst_file: &Path,
    rel_path: &str,
    conflicts: &mut Vec<ClassConflict>,
    source_pkg: &str,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("用户取消".to_string());
    }

    // If target doesn't exist, just copy
    if !dst_file.exists() {
        if let Some(parent) = dst_file.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
        }
        merge_common::copy_file_keep_mtime(src_file, dst_file)?;
        return Ok(true);
    }

    let ext = src_file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let src_ver = version_of_file(src_file, &ext);
    let dst_ver = version_of_file(dst_file, &ext);
    let src_time = merge_common::file_mtime(src_file);
    let dst_time = merge_common::file_mtime(dst_file);
    let decision = decide_conflict(src_file, dst_file, &ext);

    // 展示值跟随实际生效的依据：按版本号裁决展示版本号，按日期裁决展示日期
    let by_version = decision.rule == "版本号";
    let existing_repr = if by_version {
        dst_ver.unwrap_or_else(|| "-".to_string())
    } else {
        fmt_mtime(dst_time)
    };
    let incoming_repr = if by_version {
        src_ver.unwrap_or_else(|| "-".to_string())
    } else {
        fmt_mtime(src_time)
    };

    conflicts.push(ClassConflict {
        rel_path: rel_path.to_string(),
        kind: conflict_kind(&ext).to_string(),
        existing_version: existing_repr.clone(),
        incoming_version: incoming_repr.clone(),
        rule: decision.rule.to_string(),
        winner: if decision.overwrite {
            format!("新包({})", incoming_repr)
        } else {
            format!("已合并({})", existing_repr)
        },
        source_pkg: source_pkg.to_string(),
    });

    if decision.overwrite {
        merge_common::copy_file_keep_mtime(src_file, dst_file)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ── Service Merge Logic ──

fn merge_service(
    src_service_dir: &Path,
    dst_service_dir: &Path,
    conflicts: &mut Vec<ClassConflict>,
    source_pkg: &str,
    cancel: &AtomicBool,
    rel_prefix: Option<&str>,
) -> Result<usize, String> {
    let mut count = 0usize;

    for entry in WalkDir::new(src_service_dir)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        if !entry.file_type().is_file() {
            continue;
        }

        let relative = match entry.path().strip_prefix(src_service_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let rel_str = match rel_prefix {
            Some(prefix) => format!("{}/{}", prefix, relative.to_string_lossy()),
            None => relative.to_string_lossy().to_string(),
        };
        let dst_path = dst_service_dir.join(relative);

        // 全部同名文件（不限 class/jsp）统一走「版本号优先 → 修改日期兜底」裁决，
        // 覆盖 class/jsp/mv/sql/upd/sh/xls/xlsx/xml/jar 等所有类型。
        let copied = merge_with_conflict_check(
            entry.path(),
            &dst_path,
            &rel_str,
            conflicts,
            source_pkg,
            cancel,
        )?;
        if copied {
            count += 1;
        }
    }

    Ok(count)
}

// ── Find T* directories inside the extracted package ──

fn find_t_dirs(dir: &Path, cancel: &AtomicBool) -> Result<Vec<PathBuf>, String> {
    let entries: Vec<walkdir::DirEntry> = WalkDir::new(dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .collect();

    let sub_dirs: Vec<PathBuf> = entries
        .into_iter()
        .filter(|e| e.path() != dir)
        .map(|e| e.path().to_path_buf())
        .collect();

    if sub_dirs.is_empty() {
        return Ok(Vec::new());
    }

    // If exactly one subdirectory, look for T* inside it (Java convention)
    if sub_dirs.len() == 1 {
        let mut t_dirs: Vec<PathBuf> = Vec::new();
        let inner_entries = std::fs::read_dir(&sub_dirs[0])
            .map_err(|e| format!("读取目录失败 {}: {}", sub_dirs[0].display(), e))?;

        for entry in inner_entries.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with('T') {
                        t_dirs.push(entry.path());
                    }
                }
            }
        }
        return Ok(t_dirs);
    }

    // Multiple subdirectories: filter T* directly at this level
    let t_dirs: Vec<PathBuf> = sub_dirs
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('T'))
                .unwrap_or(false)
        })
        .collect();

    Ok(t_dirs)
}

// ── ifmpub 公库（lib 包 + lib 解压包）处理 ──

/// 收集某个需求单包中 ifmpub 目录下的公库条目。
/// - 文件（独立 JAR）按名称去重，**同名取修改日期最新的**；
/// - 目录（解压包）全部保留，后续按同名冲突规则递归合并。
fn collect_pub_entries(
    ifmpub_dir: &Path,
    source_pkg: &str,
    entries: &mut Vec<PubEntry>,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    let mut added = 0usize;

    let children = std::fs::read_dir(ifmpub_dir)
        .map_err(|e| format!("读取目录失败 {}: {}", ifmpub_dir.display(), e))?;

    for child in children.flatten() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let name = child.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "__MACOSX" {
            continue;
        }

        let is_dir = child.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let src = child.path();

        if is_dir {
            entries.push(PubEntry {
                name,
                src,
                is_dir: true,
                source_pkg: source_pkg.to_string(),
            });
        } else if let Some(prev) = entries.iter_mut().find(|e| !e.is_dir && e.name == name) {
            // 同名独立 JAR：按修改日期取最新（解压时已把 ZIP 条目原始时间写回磁盘）；
            // 日期相同或任一方缺失时保留先处理的需求单。
            if merge_common::newer_mtime(
                merge_common::file_mtime(&src),
                merge_common::file_mtime(&prev.src),
            ) {
                prev.src = src;
                prev.source_pkg = source_pkg.to_string();
            }
        } else {
            entries.push(PubEntry {
                name,
                src,
                is_dir: false,
                source_pkg: source_pkg.to_string(),
            });
        }

        added += 1;
    }

    Ok(added)
}

/// 统计目录下的文件总数（递归）。
fn count_files(dir: &Path) -> usize {
    WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count()
}

/// 把公库条目按统一规则合并到暂存目录，保证每个服务最终拿到完全一致的 lib 内容。
/// 解压包复用 `merge_service`（同名文件走「版本号优先 → 修改日期兜底」裁决），
/// 独立 JAR 直接复制（同名已在收集阶段按修改日期取最新）。
/// 返回暂存目录中实际落盘的文件数。
fn stage_pub_entries(
    entries: &[PubEntry],
    stage_dir: &Path,
    conflicts: &mut Vec<ClassConflict>,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    std::fs::create_dir_all(stage_dir)
        .map_err(|e| format!("创建暂存目录失败 {}: {}", stage_dir.display(), e))?;

    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let dst = stage_dir.join(&entry.name);

        if entry.is_dir {
            let prefix = format!("ifmpub/{}", entry.name);
            merge_service(
                &entry.src,
                &dst,
                conflicts,
                &entry.source_pkg,
                cancel,
                Some(&prefix),
            )?;
        } else {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
            }
            merge_common::copy_file_keep_mtime(&entry.src, &dst).map_err(|e| {
                format!("复制公库文件失败: {}", e)
            })?;
        }
    }

    Ok(count_files(stage_dir))
}

/// 递归复制目录内容到目标目录（同名覆盖），返回复制文件数。
fn copy_tree_overwrite(src: &Path, dest: &Path, cancel: &AtomicBool) -> Result<usize, String> {
    if !src.is_dir() {
        return Ok(0);
    }

    std::fs::create_dir_all(dest)
        .map_err(|e| format!("创建目录失败 {}: {}", dest.display(), e))?;

    let mut count = 0usize;

    for entry in std::fs::read_dir(src)
        .map_err(|e| format!("读取目录失败 {}: {}", src.display(), e))?
        .flatten()
    {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if path.is_dir() {
            count += copy_tree_overwrite(&path, &dest_path, cancel)?;
        } else {
            merge_common::copy_file_keep_mtime(&path, &dest_path)?;
            count += 1;
        }
    }

    Ok(count)
}

/// 把暂存的公库内容分发到各目标服务的 `WEB-INF/lib/`，返回 (服务名, 文件数)。
fn distribute_pub_stage(
    stage_dir: &Path,
    apps_dir: &Path,
    services: &[String],
    cancel: &AtomicBool,
) -> Result<Vec<(String, usize)>, String> {
    let mut result: Vec<(String, usize)> = Vec::with_capacity(services.len());

    for service_name in services {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let lib_dir = apps_dir.join(service_name).join("WEB-INF").join("lib");
        let copied = copy_tree_overwrite(stage_dir, &lib_dir, cancel)?;
        result.push((service_name.clone(), copied));
    }

    Ok(result)
}

/// 按显示宽度（CJK 记 2 列）右侧补空格，保证报告里的中英混排列对齐。
fn pad_display(s: &str, width: usize) -> String {
    let w: usize = s
        .chars()
        .map(|c| if (c as u32) > 0x2E7F { 2 } else { 1 })
        .sum();
    if w >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - w))
    }
}

fn push_conflict_table(lines: &mut Vec<String>, items: &[&ClassConflict]) {
    let header = format!(
        "  {} {} {} {} {} {}",
        pad_display("来源包", 24),
        pad_display("文件路径", 58),
        pad_display("裁决依据", 10),
        pad_display("已合并", 20),
        pad_display("新包", 20),
        "保留",
    );
    lines.push(header);
    lines.push(format!("  {}", "-".repeat(150)));
    for c in items {
        lines.push(format!(
            "  {} {} {} {} {} {}",
            pad_display(&c.source_pkg, 24),
            pad_display(&c.rel_path, 58),
            pad_display(&c.rule, 10),
            pad_display(&c.existing_version, 20),
            pad_display(&c.incoming_version, 20),
            c.winner,
        ));
    }
}

fn write_report(
    dst_dir: &Path,
    conflicts: &[ClassConflict],
    packages: &[String],
    padded_services: &[String],
) -> Result<(), String> {
    let report_path = dst_dir.join("合并检查结果.txt");

    let mut lines = Vec::new();
    lines.push("============================================".to_string());
    lines.push("          需求单合并检查结果             ".to_string());
    lines.push("============================================".to_string());
    lines.push(String::new());
    lines.push(format!("合并时间: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
    lines.push(format!("合并包数: {}", packages.len()));
    lines.push(String::new());
    lines.push("已合并的需求单包:".to_string());
    for pkg in packages {
        lines.push(format!("  - {}", pkg));
    }
    lines.push(String::new());

    if !padded_services.is_empty() {
        lines.push("自动补齐的服务 (需求单包中不存在，已创建并分发公库):".to_string());
        for s in padded_services {
            lines.push(format!("  - {}", s));
        }
        lines.push("  注: 这些服务目录下仅有公库 WEB-INF/lib，".to_string());
        lines.push("      如需完整内容请补充对应的需求单包。".to_string());
        lines.push(String::new());
    }

    let class_conflicts: Vec<&ClassConflict> =
        conflicts.iter().filter(|c| c.kind == "Class").collect();
    let jsp_conflicts: Vec<&ClassConflict> =
        conflicts.iter().filter(|c| c.kind == "JSP/MV").collect();
    let other_conflicts: Vec<&ClassConflict> =
        conflicts.iter().filter(|c| c.kind == "其他").collect();

    let by_version = conflicts.iter().filter(|c| c.rule == "版本号").count();
    let by_mtime = conflicts.iter().filter(|c| c.rule == "修改日期").count();
    lines.push(format!("同名文件冲突合计: {}", conflicts.len()));
    lines.push(format!("  - 按版本号裁决: {}", by_version));
    lines.push(format!("  - 按修改日期裁决: {}", by_mtime));
    lines.push(String::new());

    lines.push("============================================".to_string());
    lines.push("          Class 文件冲突检查              ".to_string());
    lines.push("============================================".to_string());
    lines.push(String::new());

    if class_conflicts.is_empty() {
        lines.push("  未发现 Class 文件冲突。".to_string());
    } else {
        lines.push(format!("  发现 {} 个 Class 冲突文件:", class_conflicts.len()));
        lines.push(String::new());
        push_conflict_table(&mut lines, &class_conflicts);
    }

    lines.push(String::new());
    lines.push("============================================".to_string());
    lines.push("          JSP / MV 文件冲突检查           ".to_string());
    lines.push("============================================".to_string());
    lines.push(String::new());

    if jsp_conflicts.is_empty() {
        lines.push("  未发现 JSP / MV 文件冲突。".to_string());
    } else {
        lines.push(format!("  发现 {} 个 JSP / MV 冲突文件:", jsp_conflicts.len()));
        lines.push(String::new());
        push_conflict_table(&mut lines, &jsp_conflicts);
    }

    lines.push(String::new());
    lines.push("============================================".to_string());
    lines.push("          其他文件冲突检查                ".to_string());
    lines.push("============================================".to_string());
    lines.push(String::new());

    if other_conflicts.is_empty() {
        lines.push("  未发现其他文件冲突。".to_string());
    } else {
        lines.push(format!(
            "  发现 {} 个其他文件冲突 (sql/upd/sh/xls/xlsx/xml/jar 等):",
            other_conflicts.len()
        ));
        lines.push(String::new());
        push_conflict_table(&mut lines, &other_conflicts);
    }

    lines.push(String::new());
    lines.push("============================================".to_string());
    lines.push("  注: 同名文件冲突裁决规则               ".to_string());
    lines.push("  适用范围: class/jsp/mv/sql/upd/sh/      ".to_string());
    lines.push("            xls/xlsx/xml/jar 等全部同名文件".to_string());
    lines.push("  1. 先比版本号: Class 从字节码常量池提取,".to_string());
    lines.push("     JSP/MV 从文件头注释 @version 提取   ".to_string());
    lines.push("  2. 版本号不同 → 保留版本号更高的一方    ".to_string());
    lines.push("  3. 版本号相同, 或双方都没有版本号 →     ".to_string());
    lines.push("     按文件修改日期, 保留日期更新的一方  ".to_string());
    lines.push("     日期取自 ZIP 条目原始修改时间       ".to_string());
    lines.push("  4. 日期相同/缺失无法判定时, 保留先合并  ".to_string());
    lines.push("     的一方 (保证结果可复现)             ".to_string());
    lines.push("============================================".to_string());
    lines.push(String::new());
    lines.push("============================================".to_string());
    lines.push("  注: ifmpub 公库分发规则                 ".to_string());
    lines.push("  1. ifmpub 下的独立 JAR 与解压包目录统一".to_string());
    lines.push("     分发到每个 ifm* 服务的 WEB-INF/lib/  ".to_string());
    lines.push("  2. 解压包目录保留原目录名              ".to_string());
    lines.push("  3. 同名独立 JAR 按修改日期取最新       ".to_string());
    lines.push("  4. 同名解压包按上述同名冲突规则递归合并".to_string());
    lines.push("  5. 包内缺失的标准服务自动创建，并同样  ".to_string());
    lines.push("     拿到公库内容 (ifmmanage/ifmonline/   ".to_string());
    lines.push("     ifmbatch/ifmcounter)                ".to_string());
    lines.push("============================================".to_string());

    std::fs::write(&report_path, lines.join("\n"))
        .map_err(|e| format!("写入报告文件失败: {}", e))?;

    Ok(())
}

// ── Main Merge Logic ──

fn run_merge(
    app: &dyn MergeReporter,
    src_dir: &Path,
    dst_dir: &Path,
    options: &NonSpMergeOptions,
    cancel: &AtomicBool,
) -> Result<NonSpMergeStatistics, String> {
    // Phase 1: Scan
    emit_progress(app, "scanning", 3, 0, 0, None);
    emit_log(app, "info", &format!("开始扫描目录: {}", src_dir.display()));

    let apps_dir = dst_dir.join("apps");
    std::fs::create_dir_all(&apps_dir)
        .map_err(|e| format!("创建 apps 目录失败: {}", e))?;

    let mut zip_files: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(src_dir).map_err(|e| format!("读取目录失败: {}", e))?;

    for entry in entries.flatten() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("zip") {
            zip_files.push(path);
        }
    }

    if zip_files.is_empty() {
        return Err("未找到 ZIP 文件".to_string());
    }

    zip_files.sort_by(|a, b| {
        let aname = a.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let bname = b.file_name().and_then(|n| n.to_str()).unwrap_or("");
        aname.cmp(bname)
    });

    emit_log(app, "info", &format!("扫描到 {} 个 ZIP 文件", zip_files.len()));

    // Phase 2: Extract & merge by service
    let unzip_dir = dst_dir.join("unzip");
    std::fs::create_dir_all(&unzip_dir)
        .map_err(|e| format!("创建 unzip 目录失败: {}", e))?;

    emit_progress(app, "extracting", 10, 0, zip_files.len(), None);

    let mut total_copied = 0usize;
    let mut all_conflicts: Vec<ClassConflict> = Vec::new();
    // ifmpub 公库条目：独立 JAR（lib 包）+ 解压后的 JAR 目录（lib 解压包）
    let mut ifmpub_entries: Vec<PubEntry> = Vec::new();
    let mut discovered_services: Vec<String> = Vec::new();
    // .upd 说明文件条目：按包处理顺序累积，供可选的 upd 合并阶段使用
    let mut upd_entries: Vec<upd_merge::UpdEntry> = Vec::new();

    for (idx, zip_path) in zip_files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let filename = zip_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let source_pkg = filename.trim_end_matches(".zip").to_string();

        emit_log(
            app,
            "info",
            &format!("处理包 {}/{}: {}", idx + 1, zip_files.len(), filename),
        );

        let progress = 10 + ((idx as u32) * 80 / zip_files.len() as u32);
        emit_progress(
            app,
            "extracting",
            progress,
            idx + 1,
            zip_files.len(),
            Some(filename.to_string()),
        );

        // Unzip to unzip/{filename_without_ext}/
        let pkg_unzip_dir = unzip_dir.join(&source_pkg);
        unzip_to(zip_path, &pkg_unzip_dir, cancel)?;

        // Find T* directories
        let t_dirs = find_t_dirs(&pkg_unzip_dir, cancel)?;

        if t_dirs.is_empty() {
            emit_log(
                app,
                "warn",
                &format!("未找到 T* 目录，跳过包: {}", filename),
            );
            continue;
        }

        emit_log(
            app,
            "info",
            &format!("找到 {} 个 T* 目录", t_dirs.len()),
        );

        // Process each T* directory
        for t_dir in &t_dirs {
            if cancel.load(Ordering::Relaxed) {
                return Err("用户取消".to_string());
            }

            let t_name = t_dir.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");

            // 收集该 T* 目录下的 .upd 说明文件（通常位于 reqsql/<用户>/<需求单号>.upd），
            // 按包顺序累积，供后续可选的 upd 合并阶段按序追加。
            if options.include_upd_merge.unwrap_or(false) {
                let collected =
                    upd_merge::collect_upd_entries(t_dir, &source_pkg, &mut upd_entries, cancel)?;
                if collected > 0 {
                    emit_log(
                        app,
                        "info",
                        &format!("  {}: 收集 {} 个 .upd 文件", t_name, collected),
                    );
                }
            }

            // Scan for service sub-directories inside this T* dir
            let service_dirs = std::fs::read_dir(t_dir)
                .map_err(|e| format!("读取目录失败 {}: {}", t_dir.display(), e))?;

            for service_entry in service_dirs.flatten() {
                if cancel.load(Ordering::Relaxed) {
                    return Err("用户取消".to_string());
                }

                if !service_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }

                let service_name = service_entry
                    .file_name()
                    .to_string_lossy()
                    .to_string();

                // Skip hidden directories like .DS_Store
                if service_name.starts_with('.') {
                    continue;
                }

                // ifmpub 是公库目录：其中的独立 JAR（lib 包）与解压后的 JAR 目录
                // （lib 解压包，如 com.hundsun.lcpt.business.trust-1.0.0.1/）都要收集，
                // 统一分发到每个 ifm* 服务的 WEB-INF/lib/ 下。
                if service_name == "ifmpub" {
                    let collected = collect_pub_entries(
                        &service_entry.path(),
                        &source_pkg,
                        &mut ifmpub_entries,
                        cancel,
                    )?;

                    let jar_count = ifmpub_entries.iter().filter(|e| !e.is_dir).count();
                    let dir_count = ifmpub_entries.len() - jar_count;

                    emit_log(
                        app,
                        "info",
                        &format!(
                            "  {}: 收集 ifmpub 公库 {} 项 (累计 JAR {} / 解压包 {})",
                            t_name, collected, jar_count, dir_count
                        ),
                    );
                    continue;
                }

                // Record this service
                if !discovered_services.contains(&service_name) {
                    discovered_services.push(service_name.clone());
                }

                // Merge this service directory into apps/{service_name}/
                let dst_service = apps_dir.join(&service_name);
                let copied = merge_service(
                    &service_entry.path(),
                    &dst_service,
                    &mut all_conflicts,
                    &source_pkg,
                    cancel,
                    None,
                )?;
                total_copied += copied;

                emit_log(
                    app,
                    "info",
                    &format!(
                        "  {} → {}: 复制 {} 个文件",
                        t_name, service_name, copied
                    ),
                );
            }
        }
    }

    // Phase 3: Distribute ifmpub payload (lib 包 + lib 解压包) to every ifm* service's WEB-INF/lib
    // 目标服务 = 4 个标准服务（包内缺失的自动创建补齐）∪ 包内出现的其他 ifm* 服务
    let pub_targets: Vec<(String, bool)> = resolve_pub_target_services(&discovered_services);
    let pub_target_services: Vec<String> = pub_targets.iter().map(|(s, _)| s.clone()).collect();
    let padded_services: Vec<String> = pub_targets
        .iter()
        .filter(|(_, padded)| *padded)
        .map(|(s, _)| s.clone())
        .collect();

    if !ifmpub_entries.is_empty() {
        let jar_count = ifmpub_entries.iter().filter(|e| !e.is_dir).count();
        let dir_count = ifmpub_entries.len() - jar_count;

        if !padded_services.is_empty() {
            emit_log(
                app,
                "warn",
                &format!(
                    "需求单包中未包含以下服务，将自动创建并补齐公库: {}",
                    padded_services.join(", ")
                ),
            );
        }

        emit_log(
            app,
            "info",
            &format!(
                "开始分发 ifmpub 公库 (JAR {} / 解压包 {}) 到 {} 个 ifm* 服务的 WEB-INF/lib ...",
                jar_count,
                dir_count,
                pub_target_services.len()
            ),
        );

        // 先按统一规则合并到暂存目录，保证每个服务拿到完全一致的 lib 内容，
        // 同时避免同类冲突被重复写入 N 次报告。
        let pub_stage_dir = unzip_dir.join("_ifmpub");
        let staged = stage_pub_entries(
            &ifmpub_entries,
            &pub_stage_dir,
            &mut all_conflicts,
            cancel,
        )?;

        emit_log(
            app,
            "info",
            &format!("公库暂存合并完成: {} 个文件", staged),
        );

        let distributed =
            distribute_pub_stage(&pub_stage_dir, &apps_dir, &pub_target_services, cancel)?;
        let distributed_total: usize = distributed.iter().map(|(_, n)| *n).sum();
        total_copied += distributed_total;

        for (service_name, count) in &distributed {
            emit_log(
                app,
                "info",
                &format!("  → {}/WEB-INF/lib: {} 个文件", service_name, count),
            );
        }

        emit_log(
            app,
            "info",
            &format!(
                "ifmpub 公库分发完成: JAR {} / 解压包 {} × {} 个服务 (共 {} 个文件)",
                jar_count,
                dir_count,
                pub_target_services.len(),
                distributed_total
            ),
        );

        if !padded_services.is_empty() {
            emit_log(
                app,
                "info",
                &format!(
                    "自动补齐服务 {}: 目录下仅含公库 WEB-INF/lib，如需完整内容请补充对应需求单包",
                    padded_services.join(", ")
                ),
            );
        }
    }

    // Phase 4: Write conflict report
    emit_log(app, "info", "生成合并检查报告...");
    let pkg_names: Vec<String> = zip_files
        .iter()
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.trim_end_matches(".zip").to_string())
        })
        .collect();
    write_report(dst_dir, &all_conflicts, &pkg_names, &padded_services)?;
    emit_log(
        app,
        "info",
        &format!("合并检查结果.txt 已生成 ({} 条冲突记录)", all_conflicts.len()),
    );

    // ── Phase 5: UPD Merge (optional) ──
    // 把各包 reqsql 下的 .upd 说明文件按包顺序追加合并为单个 merge_upd.xml。
    //
    // 顺序说明：`.upd` 正文在收集阶段（Phase 2 解压时的 collect_upd_entries）
    // 就已读入内存，本阶段不读盘，因此**与 Phase 6 清理临时目录的先后无关**。
    // 仍排在清理之前，只是为了让日志顺序贴合「先合并、后清理」的直觉。
    // （SQL 合并（Phase 7）从结果目录 `apps/` 取数，同样与临时目录无关。）
    if options.include_upd_merge.unwrap_or(false) {
        let upd_start = Instant::now();

        if upd_entries.is_empty() {
            emit_log(app, "warn", "未收集到任何 .upd 文件，跳过 upd 合并");
        } else {
            emit_log(
                app,
                "info",
                &format!("开始 upd 合并: {} 个文件", upd_entries.len()),
            );

            let output_file = dst_dir.join("merge_upd.xml");
            let summary = upd_merge::merge_upd_entries(&upd_entries, &output_file, cancel)?;

            for r in &summary.records {
                if r.encoding != "UTF-8" {
                    emit_log(
                        app,
                        "info",
                        &format!("  编码转换: {} ({} → UTF-8)", r.display_path, r.encoding),
                    );
                }
            }

            emit_log(
                app,
                "info",
                &format!(
                    "upd 合并完成: {} 个文件 → {} ({:.1} KB)",
                    summary.files,
                    summary.output_file,
                    summary.total_bytes as f64 / 1024.0
                ),
            );
            emit_log(
                app,
                "info",
                &format!(
                    "upd 合并耗时 {:.1}s",
                    upd_start.elapsed().as_millis() as f64 / 1000.0
                ),
            );
        }
    }

    // Phase 6: Clean up unzip directory
    // 注意：此后再不得读取 `unzip/` 下的任何内容。
    emit_log(app, "info", "清理临时目录...");
    if unzip_dir.exists() {
        std::fs::remove_dir_all(&unzip_dir)
            .map_err(|e| format!("清理临时目录失败: {}", e))?;
    }
    emit_log(app, "info", "临时目录已清理");

    // ── Phase 7: SQL Merge (optional) ──
    // 数据源为已合并的结果目录 `apps/`，与临时目录生命周期无关。
    if options.include_sql_merge.unwrap_or(false) {
        emit_log(app, "info", "开始 SQL 文件合并...");
        let sql_start = Instant::now();
        let apps_dir = dst_dir.join("apps");
        if apps_dir.is_dir() {
            let count = sql_merge::merge_sql_tree(&apps_dir, cancel)?;
            emit_log(app, "info", &format!("SQL 合并: {} 个合并组", count));
        }
        let sql_elapsed = sql_start.elapsed().as_millis() as u64;
        emit_log(
            app,
            "info",
            &format!("SQL 文件合并完成，耗时 {:.1}s", sql_elapsed as f64 / 1000.0),
        );
    }

    emit_progress(app, "done", 100, zip_files.len(), zip_files.len(), None);

    Ok(NonSpMergeStatistics {
        packages: zip_files.len(),
        files_copied: total_copied,
        total_duration_ms: 0,
    })
}

// ── Public Entry Point ──

/// 同步入口（源仓 `start_merge`：校验 → `thread::spawn` → 三条事件）。
///
/// 校验与统计口径一字未改，只是把「事件里回去」换成「返回值带回去」：插件进程
/// 本身已经是独立进程，再包一层线程只会让错误多绕一道（与 `sp_merge::merge` 同）。
pub fn merge(
    app: &dyn MergeReporter,
    options: &NonSpMergeOptions,
) -> Result<NonSpMergeResult, String> {
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
    let stats = run_merge(app, &src_dir, &dst_dir, options, cancel_flag())?;
    let elapsed = start.elapsed().as_millis() as u64;
    let result = NonSpMergeResult {
        success: true,
        output_dir: dst_dir.join("apps").to_string_lossy().to_string(),
        statistics: NonSpMergeStatistics {
            packages: stats.packages,
            files_copied: stats.files_copied,
            total_duration_ms: elapsed,
        },
    };
    emit_log(
        app,
        "success",
        &format!("合并完成！总耗时: {:.1}s", elapsed as f64 / 1000.0),
    );
    emit_progress(app, "done", 100, stats.packages, stats.packages, None);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn scan_packages_lists_zips() {
        let dir = std::env::temp_dir().join(format!("non_sp_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.zip"), b"x").unwrap();
        std::fs::write(dir.join("b.zip"), b"y").unwrap();
        std::fs::write(dir.join("readme.txt"), b"z").unwrap();
        let list = scan_packages(dir.to_str().unwrap()).unwrap();
        assert_eq!(list, vec!["a.zip".to_string(), "b.zip".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_packages_missing_dir() {
        assert!(scan_packages("/tmp/definitely-missing-non-sp-dir-xyz").is_err());
    }

    #[test]
    fn decode_entry_name_utf8_and_slash() {
        assert_eq!(decode_entry_name(b"path\\to\\file.txt"), "path/to/file.txt");
        assert_eq!(decode_entry_name(b"ok/name.txt"), "ok/name.txt");
    }

    #[test]
    fn find_t_dirs_nested_convention() {
        let dir = std::env::temp_dir().join(format!("non_sp_tdirs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("pkg");
        std::fs::create_dir_all(root.join("T001")).unwrap();
        std::fs::create_dir_all(root.join("T002")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        let cancel = AtomicBool::new(false);
        let found = find_t_dirs(&dir, &cancel).unwrap();
        // single subdirectory "pkg" → look inside for T*
        assert_eq!(found.len(), 2);
        let names: Vec<_> = found.iter().filter_map(|p| p.file_name()?.to_str()).collect();
        assert!(names.contains(&"T001"));
        assert!(names.contains(&"T002"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_jsp_version_basic() {
        let jsp_content = r#"<%--      @system  综合理财平台
    @version 3.0.0.13
    @2019-12-27
    @新增
    @修改记录
          程序版本            修改序号                      修改人员               修改说明
    V3.0.0.1    20181018-01      wuxs        新增代销理财产品时段证明M201810171227
 --%>"#;
        assert_eq!(extract_jsp_version(jsp_content), Some("3.0.0.13".to_string()));
    }

    #[test]
    fn extract_jsp_version_different_format() {
        let jsp_content = r#"<%--
    @version 1.2.3
    @author test
--%>"#;
        assert_eq!(extract_jsp_version(jsp_content), Some("1.2.3".to_string()));
    }

    #[test]
    fn extract_jsp_version_no_version() {
        let jsp_content = r#"<%--
    @system test
    @author test
--%>"#;
        assert_eq!(extract_jsp_version(jsp_content), None);
    }

    #[test]
    fn extract_jsp_version_empty() {
        assert_eq!(extract_jsp_version(""), None);
    }

    #[test]
    fn extract_jsp_version_in_middle_of_comment() {
        // Version tag in the middle of a large comment block should be found
        let mut jsp_content = "<%--\n".to_string();
        // Add many lines before version
        for _ in 0..100 {
            jsp_content.push_str("This is a comment line with some description.\n");
        }
        jsp_content.push_str("@version 5.6.7.8\n");
        for _ in 0..50 {
            jsp_content.push_str("More comments here.\n");
        }
        jsp_content.push_str("--%>\n<html>...</html>");
        assert_eq!(extract_jsp_version(&jsp_content), Some("5.6.7.8".to_string()));
    }

    #[test]
    fn extract_jsp_version_no_closing_tag() {
        // Version tag in unclosed comment should still be found (limited to 2000 chars)
        let jsp_content = "<%--\n@version 2.3.4\nSome text\n";
        assert_eq!(extract_jsp_version(jsp_content), Some("2.3.4".to_string()));
    }

    #[test]
    fn extract_jsp_version_text_after_comment() {
        // Version in text outside comment should NOT be found
        let jsp_content = "<%-- no version here --%>\n@version 9.9.9\n<html>";
        assert_eq!(extract_jsp_version(jsp_content), None);
    }

    // ── ifmpub 公库（lib 包 + lib 解压包） ──

    #[test]
    fn pub_target_service_accepts_ifm_only() {
        assert!(is_pub_target_service("ifmonline"));
        assert!(is_pub_target_service("ifmbatch"));
        assert!(is_pub_target_service("ifmmanage"));
        assert!(is_pub_target_service("ifmcounter"));
        assert!(!is_pub_target_service("ifmpub"));
        assert!(!is_pub_target_service("reqsql"));
        assert!(!is_pub_target_service("banksql"));
        assert!(!is_pub_target_service("modal"));
    }

    /// 4 个标准服务必须全部在列；包内缺失的标记为「补齐」。
    #[test]
    fn resolve_pub_targets_pads_missing_canonical_services() {
        let discovered = vec!["ifmonline".to_string(), "reqsql".to_string()];

        let targets = resolve_pub_target_services(&discovered);
        let names: Vec<&str> = targets.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(
            names,
            vec!["ifmmanage", "ifmonline", "ifmbatch", "ifmcounter"]
        );

        let padded: Vec<&str> = targets
            .iter()
            .filter(|(_, p)| *p)
            .map(|(s, _)| s.as_str())
            .collect();
        assert_eq!(padded, vec!["ifmmanage", "ifmbatch", "ifmcounter"]);
        assert!(!targets.iter().find(|(s, _)| s == "ifmonline").unwrap().1);
    }

    /// 包内出现的非标准 ifm* 服务要被追加（不算补齐），且不产生重复项。
    #[test]
    fn resolve_pub_targets_appends_extra_ifm_services() {
        let discovered = vec![
            "ifmonline".to_string(),
            "ifmfuture".to_string(),
            "ifmpub".to_string(),
            "ifmonline".to_string(),
        ];

        let targets = resolve_pub_target_services(&discovered);
        let names: Vec<&str> = targets.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "ifmmanage",
                "ifmonline",
                "ifmbatch",
                "ifmcounter",
                "ifmfuture"
            ]
        );
        assert!(!targets.iter().find(|(s, _)| s == "ifmfuture").unwrap().1);
    }

    /// 4 个服务全都存在时，不应有任何「补齐」。
    #[test]
    fn resolve_pub_targets_no_padding_when_all_present() {
        let discovered: Vec<String> = CANONICAL_IFM_SERVICES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let targets = resolve_pub_target_services(&discovered);
        assert_eq!(targets.len(), CANONICAL_IFM_SERVICES.len());
        assert!(targets.iter().all(|(_, padded)| !*padded));
    }

    fn mkdir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
    }

    /// 真实场景：ifmpub 下只有解压包目录（无 JAR 文件）时，也必须被收集并分发。
    #[test]
    fn collect_pub_entries_picks_dirs_and_files() {
        let root = std::env::temp_dir().join(format!("non_sp_pub_collect_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let ifmpub = root.join("T1").join("ifmpub");
        let exploded = ifmpub.join("com.hundsun.lcpt.business.trust-1.0.0.1");
        mkdir(&exploded.join("com").join("hundsun"));
        std::fs::write(
            exploded
                .join("com")
                .join("hundsun")
                .join("IQualifiedInvestorsService.class"),
            b"\xca\xfe\xba\xbe",
        )
        .unwrap();
        std::fs::write(ifmpub.join("com.hundsun.lcpt.constant-1.0.0.1.jar"), b"jar").unwrap();
        std::fs::write(ifmpub.join(".DS_Store"), b"junk").unwrap();

        let cancel = AtomicBool::new(false);
        let mut entries: Vec<PubEntry> = Vec::new();
        collect_pub_entries(&ifmpub, "PKG_A", &mut entries, &cancel).unwrap();

        assert_eq!(entries.len(), 2, "解压包目录 + JAR 都要收集，隐藏文件要跳过");
        let dir_entry = entries.iter().find(|e| e.is_dir).expect("必须收集解压包目录");
        assert_eq!(dir_entry.name, "com.hundsun.lcpt.business.trust-1.0.0.1");
        let jar_entry = entries.iter().find(|e| !e.is_dir).expect("必须收集 JAR");
        assert_eq!(jar_entry.name, "com.hundsun.lcpt.constant-1.0.0.1.jar");
        assert!(entries.iter().all(|e| e.name != ".DS_Store"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 同名 JAR 跨需求单按「修改日期最新」保留。
    #[test]
    fn collect_pub_entries_same_named_jar_keeps_newest_mtime() {
        fn at(y: i32, m: u32, d: u32) -> chrono::NaiveDateTime {
            chrono::NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        }

        let root = std::env::temp_dir().join(format!("non_sp_pub_dup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let a = root.join("A").join("ifmpub");
        let b = root.join("B").join("ifmpub");
        mkdir(&a);
        mkdir(&b);
        std::fs::write(a.join("dup-1.0.0.1.jar"), b"old").unwrap();
        std::fs::write(b.join("dup-1.0.0.1.jar"), b"new").unwrap();

        let cancel = AtomicBool::new(false);

        // 先处理日期较旧的包（A 在前），后处理日期较新的包（B）：应保留 B
        merge_common::set_file_mtime(&a.join("dup-1.0.0.1.jar"), at(2026, 1, 1));
        merge_common::set_file_mtime(&b.join("dup-1.0.0.1.jar"), at(2026, 6, 1));

        let mut entries: Vec<PubEntry> = Vec::new();
        collect_pub_entries(&a, "PKG_OLD", &mut entries, &cancel).unwrap();
        collect_pub_entries(&b, "PKG_NEW", &mut entries, &cancel).unwrap();

        assert_eq!(entries.len(), 1, "同名 JAR 只保留一条");
        assert_eq!(entries[0].source_pkg, "PKG_NEW", "日期最新的包胜出");
        assert_eq!(std::fs::read(&entries[0].src).unwrap(), b"new");

        // 反向：日期较旧的包后处理，也不能覆盖日期较新的已收集条目
        let mut entries2: Vec<PubEntry> = Vec::new();
        collect_pub_entries(&b, "PKG_NEW", &mut entries2, &cancel).unwrap();
        collect_pub_entries(&a, "PKG_OLD", &mut entries2, &cancel).unwrap();
        assert_eq!(entries2.len(), 1);
        assert_eq!(entries2[0].source_pkg, "PKG_NEW", "后处理的旧日期包不得覆盖");
        assert_eq!(std::fs::read(&entries2[0].src).unwrap(), b"new");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 同名解压包跨需求单必须做并集合并，不能只保留其中一个。
    #[test]
    fn collect_pub_entries_same_named_dir_keeps_all() {
        let root = std::env::temp_dir().join(format!("non_sp_pub_dirdup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cancel = AtomicBool::new(false);

        let jar_name = "com.hundsun.lcpt.business-1.0.0.1";
        let a = root.join("A").join("ifmpub").join(jar_name);
        let b = root.join("B").join("ifmpub").join(jar_name);
        mkdir(&a);
        mkdir(&b);
        std::fs::write(a.join("OnlyInOld.class"), b"\xca\xfe\xba\xbe").unwrap();
        std::fs::write(b.join("OnlyInNew.class"), b"\xca\xfe\xba\xbe").unwrap();

        let mut entries: Vec<PubEntry> = Vec::new();
        collect_pub_entries(
            &root.join("A").join("ifmpub"),
            "PKG_OLD",
            &mut entries,
            &cancel,
        )
        .unwrap();
        collect_pub_entries(
            &root.join("B").join("ifmpub"),
            "PKG_NEW",
            &mut entries,
            &cancel,
        )
        .unwrap();

        assert_eq!(entries.len(), 2, "同名解压包两条都要保留，供递归合并");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 端到端：暂存合并 + 分发到所有 ifm* 服务的 WEB-INF/lib。
    #[test]
    fn pub_payload_distributes_dir_and_jar_to_every_ifm_service() {
        let root = std::env::temp_dir().join(format!("non_sp_pub_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // 构造两个需求单包的 ifmpub
        let pkg_old = root.join("unzip").join("PKG_OLD").join("ifmpub");
        let pkg_new = root.join("unzip").join("PKG_NEW").join("ifmpub");
        let exploded_old = pkg_old.join("com.hundsun.lcpt.business-1.0.0.1");
        let exploded_new = pkg_new.join("com.hundsun.lcpt.business-1.0.0.1");
        mkdir(&exploded_old.join("business"));
        mkdir(&exploded_new.join("business"));
        std::fs::write(
            exploded_old.join("business").join("VolCheck.class"),
            b"\xca\xfe\xba\xbe",
        )
        .unwrap();
        std::fs::write(
            exploded_new.join("business").join("InvestAmt.class"),
            b"\xca\xfe\xba\xbe",
        )
        .unwrap();
        std::fs::write(pkg_old.join("com.hundsun.lcpt.constant-1.0.0.1.jar"), b"jar").unwrap();

        let cancel = AtomicBool::new(false);
        let mut entries: Vec<PubEntry> = Vec::new();
        collect_pub_entries(&pkg_old, "PKG_OLD", &mut entries, &cancel).unwrap();
        collect_pub_entries(&pkg_new, "PKG_NEW", &mut entries, &cancel).unwrap();
        assert!(entries
            .iter()
            .any(|e| e.is_dir && e.name == "com.hundsun.lcpt.business-1.0.0.1"));

        let stage = root.join("unzip").join("_ifmpub");
        let mut conflicts: Vec<ClassConflict> = Vec::new();
        let staged = stage_pub_entries(&entries, &stage, &mut conflicts, &cancel).unwrap();
        assert_eq!(staged, 3, "JAR + 两个解压包文件都要进入暂存");

        // 服务目录（含非 webapp 的 reqsql / banksql 作为干扰项）
        // 需求单包中只有 ifmbatch / ifmonline，其余标准服务应被自动创建补齐
        let apps = root.join("apps");
        let services = vec![
            "ifmbatch".to_string(),
            "ifmonline".to_string(),
            "reqsql".to_string(),
            "banksql".to_string(),
        ];
        for svc in &services {
            mkdir(&apps.join(svc));
        }

        let resolved = resolve_pub_target_services(&services);
        let targets: Vec<String> = resolved.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(
            targets,
            vec![
                "ifmmanage".to_string(),
                "ifmonline".to_string(),
                "ifmbatch".to_string(),
                "ifmcounter".to_string()
            ],
            "缺失的标准服务必须被补齐"
        );
        let padded: Vec<String> = resolved
            .iter()
            .filter(|(_, p)| *p)
            .map(|(s, _)| s.clone())
            .collect();
        assert_eq!(
            padded,
            vec!["ifmmanage".to_string(), "ifmcounter".to_string()]
        );
        assert!(!apps.join("ifmmanage").exists(), "补齐服务分发前不应存在");
        assert!(!apps.join("ifmcounter").exists(), "补齐服务分发前不应存在");

        let distributed = distribute_pub_stage(&stage, &apps, &targets, &cancel).unwrap();
        assert_eq!(distributed.len(), targets.len());
        assert_eq!(
            distributed.iter().map(|(_, n)| *n).sum::<usize>(),
            staged * targets.len()
        );
        assert!(
            distributed.iter().all(|(_, n)| *n == staged),
            "每个服务拿到的文件数必须完全一致"
        );

        for svc in &targets {
            let lib = apps.join(svc).join("WEB-INF").join("lib");
            let unpacked = lib.join("com.hundsun.lcpt.business-1.0.0.1").join("business");
            assert!(
                unpacked.join("VolCheck.class").is_file(),
                "{} 缺少解压包内容",
                svc
            );
            assert!(
                unpacked.join("InvestAmt.class").is_file(),
                "{} 缺少另一需求单的解压包内容（未做并集合并）",
                svc
            );
            assert!(
                lib.join("com.hundsun.lcpt.constant-1.0.0.1.jar").is_file(),
                "{} 缺少 ifmpub JAR",
                svc
            );
        }

        // 非 webapp 目录不得被创建 WEB-INF/lib
        assert!(!apps.join("reqsql").join("WEB-INF").exists());
        assert!(!apps.join("banksql").join("WEB-INF").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 同名文件「版本号优先 → 修改日期兜底」裁决 ──

    fn naivedt(y: i32, m: u32, d: u32, h: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, 0, 0)
            .unwrap()
    }

    fn write_at(path: &Path, bytes: &[u8], t: chrono::NaiveDateTime) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
        merge_common::set_file_mtime(path, t);
    }

    fn conflict_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "non_sp_conflict_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// 版本号不同 → 版本高者胜，与修改日期无关（新包日期更旧也必须被覆盖）。
    #[test]
    fn decide_conflict_version_wins_over_date() {
        let root = conflict_root("ver");
        let src = root.join("new.jsp");
        let dst = root.join("old.jsp");
        write_at(&src, b"<%--\n@version 3.0.0.6\n--%>\n<html>", naivedt(2026, 1, 1, 9));
        write_at(&dst, b"<%--\n@version 3.0.0.5\n--%>\n<html>", naivedt(2026, 6, 1, 9));

        let d = decide_conflict(&src, &dst, "jsp");
        assert!(d.overwrite, "版本号更高的一方必须胜出");
        assert_eq!(d.rule, "版本号");

        let d2 = decide_conflict(&dst, &src, "jsp");
        assert!(!d2.overwrite, "版本号较低的一方即使日期更新也不得覆盖");
        assert_eq!(d2.rule, "版本号");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 版本号相同 → 按修改日期取最新。
    #[test]
    fn decide_conflict_same_version_falls_back_to_mtime() {
        let root = conflict_root("samever");
        let src = root.join("new.jsp");
        let dst = root.join("old.jsp");
        let body = b"<%--\n@version 3.0.0.5\n--%>\n<html>".as_slice();
        write_at(&src, body, naivedt(2026, 6, 1, 9));
        write_at(&dst, body, naivedt(2026, 1, 1, 9));

        let d = decide_conflict(&src, &dst, "jsp");
        assert!(d.overwrite);
        assert_eq!(d.rule, "修改日期");

        let d2 = decide_conflict(&dst, &src, "jsp");
        assert!(!d2.overwrite, "日期较旧的一方不得覆盖");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 没有版本号 → 按修改日期取最新；覆盖 class/jsp/mv/sql/upd/sh/xls/xlsx/xml/jar。
    #[test]
    fn decide_conflict_without_version_falls_back_to_mtime() {
        let root = conflict_root("nover");

        // 1) 伪 class（常量池里提不到版本号）
        let src = root.join("a.class");
        let dst = root.join("b.class");
        let class_bytes = b"\xca\xfe\xba\xbe\x00\x00\x00\x34".as_slice();
        write_at(&src, class_bytes, naivedt(2026, 6, 1, 9));
        write_at(&dst, class_bytes, naivedt(2026, 1, 1, 9));
        let d = decide_conflict(&src, &dst, "class");
        assert!(d.overwrite, "无版本号的 class 应按日期取最新");
        assert_eq!(d.rule, "修改日期");
        assert!(!decide_conflict(&dst, &src, "class").overwrite);

        // 1b) 被截断/损坏的 class 必须安全降级为「无版本号」，不得 panic 中断合并
        let broken = root.join("broken.class");
        write_at(&broken, b"\xca\xfe\xba\xbe", naivedt(2026, 6, 1, 9));
        assert_eq!(version_of_file(&broken, "class"), None);

        // 2) 其余类型同样遵循「日期最新」
        for ext in ["jsp", "mv", "sql", "upd", "sh", "xls", "xlsx", "xml", "jar"] {
            let s = root.join(format!("s.{ext}"));
            let o = root.join(format!("o.{ext}"));
            write_at(&s, b"x", naivedt(2026, 6, 1, 9));
            write_at(&o, b"x", naivedt(2026, 1, 1, 9));
            let dec = decide_conflict(&s, &o, ext);
            assert!(dec.overwrite, ".{ext} 应按修改日期取最新");
            assert_eq!(dec.rule, "修改日期", ".{ext} 应走日期裁决");
            assert!(!decide_conflict(&o, &s, ext).overwrite, ".{ext} 旧日期不得覆盖");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 双方日期相同/缺失 → 保留先合并的一方（结果可复现）。
    #[test]
    fn decide_conflict_tie_keeps_existing() {
        let root = conflict_root("tie");
        let src = root.join("s.xml");
        let dst = root.join("o.xml");
        write_at(&src, b"x", naivedt(2026, 3, 1, 9));
        write_at(&dst, b"x", naivedt(2026, 3, 1, 9));

        let d = decide_conflict(&src, &dst, "xml");
        assert!(!d.overwrite, "日期相同应保留已合并的一方");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// GBK 编码的 JSP 也要能提取版本号，不能因读取失败中断合并。
    #[test]
    fn version_of_file_reads_gbk_jsp() {
        let root = conflict_root("gbk");
        let p = root.join("gbk.jsp");
        let (bytes, _, _) = GBK.encode("<%--\n@version 3.0.0.9\n--%>\n<html>中文</html>");
        write_at(&p, &bytes, naivedt(2026, 6, 1, 9));
        assert_eq!(version_of_file(&p, "jsp"), Some("3.0.0.9".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 端到端：按日期覆盖后，目标文件内容与修改时间都与源文件一致。
    #[test]
    fn merge_with_conflict_check_overwrites_and_keeps_mtime() {
        let root = conflict_root("e2e");
        let src = root.join("src").join("a.xml");
        let dst = root.join("dst").join("a.xml");
        write_at(&src, b"NEW", naivedt(2026, 6, 1, 9));
        write_at(&dst, b"OLD", naivedt(2026, 1, 1, 9));

        let cancel = AtomicBool::new(false);
        let mut conflicts: Vec<ClassConflict> = Vec::new();
        let wrote =
            merge_with_conflict_check(&src, &dst, "a.xml", &mut conflicts, "PKG", &cancel).unwrap();
        assert!(wrote);
        assert_eq!(std::fs::read(&dst).unwrap(), b"NEW");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].rule, "修改日期");
        assert_eq!(conflicts[0].kind, "其他");
        assert_eq!(
            merge_common::file_mtime(&dst),
            Some(naivedt(2026, 6, 1, 9)),
            "目标文件修改时间应保留为源文件时间，供下一包比较"
        );

        // 再处理一个日期更旧的包：不得覆盖
        let older = root.join("src2").join("a.xml");
        write_at(&older, b"OLDER", naivedt(2026, 2, 1, 9));
        let mut c2: Vec<ClassConflict> = Vec::new();
        let wrote2 =
            merge_with_conflict_check(&older, &dst, "a.xml", &mut c2, "PKG2", &cancel).unwrap();
        assert!(!wrote2);
        assert_eq!(std::fs::read(&dst).unwrap(), b"NEW");
        assert!(c2[0].winner.starts_with("已合并"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
