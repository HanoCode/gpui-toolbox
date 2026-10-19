//! IFMS6.0 上线包合并（`Ifms6Merge.vue`）：前端 / 服务端 / 库包 / ifmcounter /
//! spconfig 分块合并后重新打包。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/ifms6_merge.rs`（2,037 行）。
//! 接线与 `non_sp_merge` 同一套：`emit_*` → [`crate::MergeReporter`]，
//! `start_merge`（spawn）→ [`merge`]（同步返回）。另外两处因「宿主不能反向调
//! 插件」而必须改：
//!
//! - `#[tauri::command] ifms6_merge_scan_versions` → [`scan_versions`]（普通函数）：
//!   参考页面点「重新扫描」时由前端 invoke 它拿到版本列表，而 process 轨的插件
//!   进程**不能**被界面反向调用。改为：`versions` 参数留空时**插件进程**先调一次
//!   [`scan_versions`] 再进 [`merge`]（`crates/duck-merge-tool::resolve_versions`），
//!   与页面点「重新扫描」等价——内核这层不做隐式扫描，它只认传进来的列表。
//! - `emit_done` 删掉：结果直接当返回值给插件，由插件拼结果正文。

// 复刻件的 clippy 偏差**逐条列名**：这些写法是参考仓的既有风格（两千行的合并流程，
// 重排成 clippy 喜欢的形状就等于重写，逐行 `diff` 会立刻失效）。`dead_code` 与
// `unused_*` 也是同理：上游确有未被调用的辅助函数与未读的参数，保持原样。
#![allow(
    clippy::collapsible_if,
    clippy::too_many_arguments,
    clippy::manual_strip,
    clippy::bind_instead_of_map,
    clippy::collapsible_str_replace,
    clippy::empty_line_after_doc_comments,
    clippy::for_kv_map,
    clippy::if_same_then_else,
    clippy::manual_map,
    clippy::needless_borrows_for_generic_args,
    clippy::ptr_arg,
    clippy::redundant_pattern_matching,
    clippy::unnecessary_filter_map,
    clippy::unnecessary_map_or,
    clippy::useless_format,
    dead_code,
    unused_assignments,
    unused_variables
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use encoding_rs::GBK;
use serde::{Deserialize, Serialize};

use crate::MergeReporter;
use duck_sql_core::sql_merge;

// ── 工程包 Detection ──

/// Check if a version name is a 工程包 (engineering package) by detecting C001 pattern
/// e.g., IFMS6.0V202607-02-008-C001-80149-QLYH
fn is_engineering_package(version_name: &str) -> bool {
    version_name.contains("-C001-")
}

// ── Regression Detection ──

fn class_major_version(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 8 || bytes[0] != 0xCA || bytes[1] != 0xFE || bytes[2] != 0xBA || bytes[3] != 0xBE {
        return None;
    }
    Some(u16::from_be_bytes([bytes[6], bytes[7]]))
}

static V6_REGRESSIONS: OnceLock<Mutex<Vec<(String, u32, u32)>>> = OnceLock::new();
fn v6_regression_list() -> &'static Mutex<Vec<(String, u32, u32)>> {
    V6_REGRESSIONS.get_or_init(|| Mutex::new(Vec::new()))
}

// ── Global Cancel ──

static CANCEL_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
fn cancel_flag() -> &'static Arc<AtomicBool> { CANCEL_FLAG.get_or_init(|| Arc::new(AtomicBool::new(false))) }
pub fn cancel() { cancel_flag().store(true, Ordering::SeqCst); }
pub fn reset_cancel() { cancel_flag().store(false, Ordering::SeqCst); }

// ── Data Structures ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ifms6MergeOptions {
    pub src_dir: String,
    pub dst_dir: String,
    pub customer_name: String,
    pub all_merge_versions: Vec<String>,
    pub current_version: String,
    pub target_version: String,
    pub include_sql_merge: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ifms6MergeResult {
    pub success: bool,
    pub output_path: String,
    pub statistics: Ifms6MergeStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ifms6MergeStats {
    pub versions: usize,
    pub front_merged: bool,
    pub server_merged: bool,
    pub db_merged: bool,
    pub ifmcounter_merged: bool,
    pub spconfig_merged: bool,
    pub total_duration_ms: u64,
}

/// non-SEE 路径的结果（源仓是把这段 JSON 直接 emit 出去的，没有对应结构体）。
///
/// 字段与源仓 `ifms6-non-see-merge-done` 载荷一一对应：**少了** ifmcounter /
/// spconfig（那条路径本来就不合并这两块），多出来的 `success` 保留着，便于与
/// SEE 路径共用同一份结果拼版。
#[derive(Debug, Clone, Serialize)]
pub struct Ifms6NonSeeResult {
    pub success: bool,
    pub output_path: String,
    pub statistics: Ifms6NonSeeStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ifms6NonSeeStats {
    pub versions: usize,
    pub front_merged: bool,
    pub server_merged: bool,
    pub db_merged: bool,
    pub total_duration_ms: u64,
}

// ── Emit Helpers ──

fn emit_progress(app: &dyn MergeReporter, stage: &str, pct: u32, msg: Option<&str>) {
    // 源仓这里 `pct.min(100)`；`current/total` 这条路径没有，填 0。
    app.progress(stage, pct.min(100), 0, 0, msg.map(str::to_owned));
}
fn emit_log(app: &dyn MergeReporter, level: &str, message: &str, dur: Option<u64>) {
    app.log(level, message, dur);
}

// ── ZIP Utils ──

fn decode_entry_name(raw: &[u8]) -> String {
    if let Ok(name) = std::str::from_utf8(raw) {
        name.replace('\\', "/")
    } else {
        let (decoded, _, _) = GBK.decode(raw);
        decoded.replace('\\', "/")
    }
}

fn unzip_to(zip_path: &Path, dest: &Path) -> Result<usize, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("打开ZIP失败 {}", e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("读取ZIP失败 {}", e))?;
    let mut count = 0;
    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) { Ok(e) => e, Err(_) => continue };
        let ename = decode_entry_name(entry.name_raw());
        if entry.is_dir() { let _ = std::fs::create_dir_all(dest.join(&ename)); continue; }
        if ename.contains("..") { continue; }
        let fp = dest.join(&ename);
        if let Some(p) = fp.parent() { std::fs::create_dir_all(p).map_err(|e| format!("创建目录失败 {}", e))?; }
        let mut out = std::fs::File::create(&fp).map_err(|e| format!("创建文件失败 {}", e))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("写入文件失败 {}", e))?;
        count += 1;
    }
    Ok(count)
}

fn remove_dir(path: &Path) -> Result<(), String> {
    if path.exists() { std::fs::remove_dir_all(path).map_err(|e| format!("清理目录失败 {}", e))?; }
    Ok(())
}

// ── Chinese Sort (Java StringUtils.sortByChinese / sortByChineseReverse) ──

fn sort_chinese(list: &mut Vec<String>) {
    list.sort_by(|a, b| {
        let a_bytes = a.as_bytes();
        let b_bytes = b.as_bytes();
        a_bytes.cmp(b_bytes)
    });
}

fn sort_chinese_reverse(list: &mut Vec<String>) {
    list.sort_by(|a, b| {
        let a_bytes = a.as_bytes();
        let b_bytes = b.as_bytes();
        b_bytes.cmp(a_bytes)
    });
}

// ── File Search (Java findFilePathInDirectory) ──

fn find_file_in_dir(dir: &Path, target: &str) -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if let Some(f) = find_file_in_dir(&p, target) { return Some(f); }
            } else if p.file_name().and_then(|n| n.to_str()) == Some(target) {
                return Some(p);
            }
        }
    }
    None
}

fn find_dir_in_dir(dir: &Path, target: &str) -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && entry.file_name().to_string_lossy() == target {
                return Some(p);
            }
        }
    }
    None
}

// ── Directory Finders (Java findFrontParentDir / findServerDir / etc.) ──

fn find_sub_version_dir(zip_dir: &Path) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("IFMS6.0V") && entry.path().is_dir() { return entry.path(); }
        }
    }
    zip_dir.join("front")
}

fn find_server_dir(zip_dir: &Path) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("IFMS6.0V") && entry.path().is_dir() { return entry.path(); }
        }
    }
    zip_dir.join("server")
}

fn find_db_dir(zip_dir: &Path) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("IFMS6.0V") && entry.path().is_dir() { return entry.path(); }
        }
    }
    zip_dir.join("spsql")
}

fn find_spconfig_dir(zip_dir: &Path) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("IFMS6.0V") && entry.path().is_dir() { return entry.path(); }
        }
    }
    zip_dir.join("spsql")
}

fn find_ifmcounter_dir(zip_dir: &Path) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("IFMS6.0V") && entry.path().is_dir() { return entry.path(); }
        }
    }
    zip_dir.join("ifmcounter")
}

fn find_upgrade_dir(zip_dir: &Path) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("IFMS6.0V") && entry.path().is_dir() { return entry.path().join("SP包升级说明"); }
        }
    }
    zip_dir.join("SP包升级说明")
}

fn find_console_dir(dir: &Path) -> Option<PathBuf> {
    if dir.file_name().and_then(|n| n.to_str()) == Some("console") { return Some(dir.to_path_buf()); }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if let Some(f) = find_console_dir(&p) { return Some(f); }
            }
        }
    }
    None
}

fn debug_list_dir(path: &Path) -> String {
    match std::fs::read_dir(path) {
        Ok(dir) => {
            let names: Vec<String> = dir.filter_map(|e| match e {
                Ok(entry) => {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let kind = if entry.path().is_dir() { "d" } else { "f" };
                    Some(format!("{}({})", kind, name))
                }
                Err(err) => Some(format!("ERR:{}", err)),
            }).collect();
            format!("{} entries [{}]", names.len(), names.join(", "))
        }
        Err(e) => format!("read_dir FAILED: {}", e),
    }
}

fn dir_count(path: &Path) -> usize {
    match std::fs::read_dir(path) {
        Ok(dir) => dir.flatten().count(),
        Err(_) => 0,
    }
}

fn is_exist_front(front_parent: &Path) -> Option<PathBuf> {
    let f1 = front_parent.join("front.zip");
    let f2 = front_parent.join("front").join("front.zip");
    if f1.exists() { Some(f1) } else if f2.exists() { Some(f2) } else { None }
}

fn find_new_version_package(zip_dir: &Path) -> bool {
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy() == "server" { return true; }
        }
    }
    false
}

fn find_non_db_zip(zip_dir: &Path) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("IFMS6.0V") && entry.path().is_dir() { return entry.path().join("spsql"); }
        }
    }
    zip_dir.join("spsql")
}

/// Find sqls/ directory inside extracted db-*.zip content.
/// The zip may have a single root subdir (e.g. db/, db-$TACODE/)
/// so we check both the root and one level deep.
fn find_sqls_in_extracted(dir: &Path) -> Option<PathBuf> {
    // Check direct child
    let direct = dir.join("sqls");
    if direct.is_dir() { return Some(direct); }
    // Check one level deep (common: db/, db-$TACODE/ as zip root)
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let nested = p.join("sqls");
                if nested.is_dir() { return Some(nested); }
            }
        }
    }
    None
}

fn find_db_zip_map(zip_dir: &Path, map: &mut HashMap<String, Vec<PathBuf>>) {
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let inner = if name.starts_with("IFMS6.0V") {
                entry.path()
            } else {
                let p1 = zip_dir.join("spsql");
                let p2 = zip_dir.join("sql");
                if p1.exists() { p1 } else { p2 }
            };
            if inner.exists() {
                if let Ok(inner_entries) = std::fs::read_dir(&inner) {
                    for ie in inner_entries.flatten() {
                        let iname = ie.file_name().to_string_lossy().to_string();
                        if iname == "db.zip" || iname.starts_with("db-") || iname.starts_with("db_") {
                            let key = iname.replace('{', "").replace('}', "");
                            map.entry(key).or_default().push(ie.path());
                        }
                    }
                }
            }
        }
    }
}

fn find_see_server_zips(zip_dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(zip_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let file_path = if name.starts_with("IFMS6.0V") {
                entry.path()
            } else if name == "server" {
                entry.path()
            } else { continue };
            if file_path.is_dir() {
                if let Ok(inner) = std::fs::read_dir(&file_path) {
                    for ie in inner.flatten() {
                        let iname = ie.file_name().to_string_lossy().to_string();
                        if iname.starts_with("lcpt") || iname == "lib.zip" || iname == "lib" {
                            result.push(ie.path());
                        }
                    }
                }
            }
        }
    }
    result
}

// ═══════════════════════════════════════════════════════════════
//  1. frontSeePackageMerge
// ═══════════════════════════════════════════════════════════════

fn front_see_package_merge(save_path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    // Collect all version zip dirs
    let mut zip_names: Vec<String> = Vec::new();
    let mut zip_map: HashMap<String, PathBuf> = HashMap::new();
    let mut dir_map: HashMap<String, PathBuf> = HashMap::new();

    if let Ok(entries) = std::fs::read_dir(save_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            let p = entry.path();
            if p.is_dir() {
                zip_names.push(name.clone());
                zip_map.insert(name, p.clone());
            } else if p.is_file() && p.extension().map(|x| x == "zip").unwrap_or(false) {
                zip_names.push(name.clone());
                zip_map.insert(name, p);
            }
        }
    }

    sort_chinese_reverse(&mut zip_names);
    if zip_names.is_empty() { return Ok(()); }
    let big_version_name = zip_names[0].clone();

    // Unzip all version dirs
    for vname in &zip_names {
        let p = zip_map.get(vname).ok_or_else(|| format!("未找到版本: {}", vname))?;
        if p.is_file() && p.extension().map(|x| x == "zip").unwrap_or(false) {
            let dir_name = vname.trim_end_matches(".zip");
            let dir_path = save_path.join(dir_name);
            std::fs::create_dir_all(&dir_path).map_err(|e| format!("创建目录 {} 失败: {}", dir_path.display(), e))?;
            unzip_to(p, &dir_path)?;
            let _ = std::fs::remove_file(p);
            dir_map.insert(vname.clone(), dir_path);
        } else {
            dir_map.insert(vname.clone(), p.clone());
        }
    }

    let big_ver_dir = dir_map.get(&big_version_name).ok_or_else(|| format!("未找到大版本目录: {}", big_version_name))?;
    let front_parent = find_sub_version_dir(big_ver_dir);
    let big_front_path = is_exist_front(&front_parent);

    // Collect front module names from the latest version
    let mut front_modules: Vec<String> = Vec::new();
    let mut has_console = false;
    let is_two_zip = big_front_path.is_some();
    // Track deploy.xml from 工程包 (engineering package with C001 pattern)
    let mut engineering_deploy_xml: Option<PathBuf> = None;

    // Check if big version is a 工程包
    let big_is_eng = is_engineering_package(&big_version_name);

    if let Some(ref bfp) = big_front_path {
        let front_dir = bfp.parent().unwrap_or(big_ver_dir);
        unzip_to(bfp, front_dir).map_err(|e| format!("解压最新版本 front.zip 失败: {}", e))?;
        let _ = std::fs::remove_file(bfp);

        // If big version is a 工程包, save the deploy.xml path
        if big_is_eng {
            let deploy_xml = front_dir.join("front").join("front").join("deploy.xml");
            if deploy_xml.exists() {
                engineering_deploy_xml = Some(deploy_xml);
            }
        }

        // Try inner front.zip in front/front/
        let inner_front = front_dir.join("front").join("front.zip");
        if inner_front.exists() {
            let inner_dir = front_dir.join("front").join("front");
            std::fs::create_dir_all(&inner_dir).ok();
            unzip_to(&inner_front, &inner_dir).map_err(|e| format!("解压内部 front.zip 失败: {}", e))?;
            let _ = std::fs::remove_file(&inner_front);
            if let Some(console) = find_console_dir(&inner_dir) {
                if let Ok(ce) = std::fs::read_dir(&console) {
                    for cf in ce.flatten() {
                        let cn = cf.file_name().to_string_lossy().to_string();
                        if cn.ends_with("-vue") { front_modules.push(cn); }
                    }
                }
                has_console = true;
            }
        } else {
            // New version format: no inner front.zip
            // Collect vue modules from the already-extracted console directory
            if let Some(console) = find_console_dir(front_dir) {
                if let Ok(ce) = std::fs::read_dir(&console) {
                    for cf in ce.flatten() {
                        let cn = cf.file_name().to_string_lossy().to_string();
                        if cn.ends_with("-vue") { front_modules.push(cn); }
                    }
                }
                has_console = true;
            }
        }

        // Also process console-xxx-vue.zip files in front/ directory
        if let Ok(entries) = std::fs::read_dir(front_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with("console-") || !name.ends_with("-vue.zip") { continue; }
                let module_name = name.trim_end_matches(".zip").to_string();
                if front_modules.contains(&module_name) { continue; }

                let temp_dir = save_path.join(format!(".front_tmp_big_{}", module_name));
                std::fs::create_dir_all(&temp_dir).ok();
                if let Err(_) = unzip_to(&entry.path(), &temp_dir) { continue; }

                if let Some(console) = find_console_dir(&temp_dir) {
                    if let Ok(ce) = std::fs::read_dir(&console) {
                        for cf in ce.flatten() {
                            let cn = cf.file_name().to_string_lossy().to_string();
                            if cn.ends_with("-vue") && !front_modules.contains(&cn) {
                                front_modules.push(cn.clone());
                                let target = find_target_path(&front_parent, &temp_dir, &cn, is_two_zip);
                                if let Some(t) = target {
                                    copy_dir_all(&cf.path(), &t)?;
                                }
                            }
                        }
                    }
                }
                let _ = remove_dir(&temp_dir);
            }
        }
    }

    // Merge front modules from older versions (zip metadata scan, skip full extraction when possible)
    for vname in &zip_names {
        if vname == &big_version_name { continue; }
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let dir = match dir_map.get(vname) { Some(d) => d, None => continue };
        let fp = find_sub_version_dir(dir);
        let front_path = is_exist_front(&fp);

        // Check if this version is a 工程包 (engineering package with C001 pattern)
        let is_eng_pkg = is_engineering_package(vname);

        if let Some(ref path) = front_path {
            // Case 1: front.zip exists (original format with console/xxx-vue inside)
            let file = match std::fs::File::open(path) {
                Ok(f) => f, Err(_) => { let _ = std::fs::remove_file(path); continue; }
            };
            let Ok(mut archive) = zip::ZipArchive::new(file) else { let _ = std::fs::remove_file(path); continue; };

            let mut has_inner = false;
            let mut scan_modules: Vec<String> = Vec::new();
            for i in 0..archive.len() {
                if let Ok(entry) = archive.by_index(i) {
                    let ename = decode_entry_name(entry.name_raw());
                    if ename == "front/front.zip" { has_inner = true; }
                    if !has_inner {
                        let parts: Vec<&str> = ename.split('/').collect();
                        for (j, part) in parts.iter().enumerate() {
                            if *part == "console" && j + 1 < parts.len() && parts[j+1].ends_with("-vue") {
                                let m = parts[j+1].to_string();
                                if !front_modules.contains(&m) && !scan_modules.contains(&m) {
                                    scan_modules.push(m);
                                }
                            }
                        }
                    }
                }
            }
            drop(archive);

            if has_console && !has_inner && scan_modules.is_empty() {
                let _ = std::fs::remove_file(path);
                continue;
            }

            let temp_dir = save_path.join(format!(".front_tmp_{}", vname));
            std::fs::create_dir_all(&temp_dir).ok();
            unzip_to(path, &temp_dir).map_err(|e| format!("前端合并: 解压 {} (版本 {}) 失败: {}", path.display(), vname, e))?;
            let _ = std::fs::remove_file(path);

            // If this is a 工程包, save the deploy.xml path
            if is_eng_pkg {
                let deploy_xml = temp_dir.join("front").join("front").join("deploy.xml");
                if deploy_xml.exists() {
                    engineering_deploy_xml = Some(deploy_xml);
                }
            }

            let inner = temp_dir.join("front").join("front.zip");
            if inner.exists() {
                let inner_dir = temp_dir.join("front").join("front");
                std::fs::create_dir_all(&inner_dir).ok();
                unzip_to(&inner, &inner_dir).map_err(|e| format!("解压内部 front.zip 失败: {}", e))?;
                let _ = std::fs::remove_file(&inner);
            }

            let console_dir = find_console_dir(&temp_dir);
            if let Some(ref cd) = console_dir {
                if !has_console {
                    copy_dir_all(&temp_dir, &front_parent)?;
                    has_console = true;
                } else if let Ok(ce) = std::fs::read_dir(cd) {
                    // Determine target console dir for non-vue files
                    let target_console = if is_two_zip {
                        find_console_dir(&front_parent)
                    } else {
                        Some(front_parent.join("front").join("front").join("console"))
                    };
                    for cf in ce.flatten() {
                        let cn = cf.file_name().to_string_lossy().to_string();
                        if cn.ends_with("-vue") {
                            if !front_modules.contains(&cn) {
                                front_modules.push(cn.clone());
                                let target = find_target_path(&front_parent, &temp_dir, &cn, is_two_zip);
                                if let Some(t) = target {
                                    copy_dir_all(&cf.path(), &t)?;
                                }
                            }
                        } else if let Some(ref tc) = target_console {
                            // Copy non-vue files/dirs (common/, frame/, static/, unifiedLogin/, *.js, *.css, *.html)
                            let t = tc.join(&cn);
                            if !t.exists() {
                                if cf.path().is_dir() {
                                    std::fs::create_dir_all(&t).ok();
                                    copy_dir_all(&cf.path(), &t)?;
                                } else {
                                    if let Some(parent) = t.parent() { std::fs::create_dir_all(parent).ok(); }
                                    let _ = std::fs::copy(cf.path(), &t);
                                }
                            }
                        }
                    }
                }
            }
            let _ = remove_dir(&temp_dir);

            // Also process console-xxx-vue.zip files in front/ directory for this version
            // fp = <version>/front/, zip files are at fp/ level (e.g. fp/console-dxcp-vue.zip)
            if let Ok(entries) = std::fs::read_dir(&fp) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with("console-") || !name.ends_with("-vue.zip") { continue; }
                    let module_name = name.trim_end_matches(".zip").to_string();
                    if front_modules.contains(&module_name) { continue; }

                    let temp_dir = save_path.join(format!(".front_tmp_{}_{}", vname, module_name));
                    std::fs::create_dir_all(&temp_dir).ok();
                    if let Err(_) = unzip_to(&entry.path(), &temp_dir) {
                        let _ = remove_dir(&temp_dir);
                        continue;
                    }

                    if let Some(console) = find_console_dir(&temp_dir) {
                        if let Ok(ce) = std::fs::read_dir(&console) {
                            for cf in ce.flatten() {
                                let cn = cf.file_name().to_string_lossy().to_string();
                                if cn.ends_with("-vue") && !front_modules.contains(&cn) {
                                    front_modules.push(cn.clone());
                                    let target = find_target_path(&front_parent, &temp_dir, &cn, is_two_zip);
                                    if let Some(t) = target {
                                        copy_dir_all(&cf.path(), &t)?;
                                    }
                                }
                            }
                        }
                    }
                    let _ = remove_dir(&temp_dir);
                }
            }

            // Also copy fp/front/ directory content (deploy.xml, scripts, template) if present
            let inner_front = fp.join("front");
            if inner_front.exists() && inner_front.is_dir() {
                let target_base = if is_two_zip {
                    front_parent.join("front")
                } else {
                    front_parent.join("front").join("front")
                };
                if let Ok(ie) = std::fs::read_dir(&inner_front) {
                    for item in ie.flatten() {
                        let cn = item.file_name().to_string_lossy().to_string();
                        if cn == "console" { continue; }
                        let t = target_base.join(&cn);
                        if !t.exists() {
                            if item.path().is_dir() {
                                std::fs::create_dir_all(&t).ok();
                                copy_dir_all(&item.path(), &t)?;
                            } else {
                                if let Some(parent) = t.parent() { std::fs::create_dir_all(parent).ok(); }
                                let _ = std::fs::copy(item.path(), &t);
                            }
                        }
                    }
                }
            }
        } else {
            // Case 2: console-xxx-vue.zip files directly in front/ (new modular format)
            // fp = <version>/front/, zip files are at fp/ level
            if let Ok(entries) = std::fs::read_dir(&fp) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with("console-") || !name.ends_with("-vue.zip") { continue; }
                    let module_name = name.trim_end_matches(".zip").to_string();
                    if front_modules.contains(&module_name) { continue; }

                    let temp_dir = save_path.join(format!(".front_tmp_{}_{}", vname, module_name));
                    std::fs::create_dir_all(&temp_dir).ok();
                    if let Err(e) = unzip_to(&entry.path(), &temp_dir) {
                        let _ = remove_dir(&temp_dir);
                        continue;
                    }

                    let console_dir = find_console_dir(&temp_dir);
                    if let Some(ref cd) = console_dir {
                        if !has_console {
                            copy_dir_all(&temp_dir, &front_parent)?;
                            has_console = true;
                        } else {
                            let target_console = if is_two_zip {
                                find_console_dir(&front_parent)
                            } else {
                                Some(front_parent.join("front").join("front").join("console"))
                            };
                            if let Ok(ce) = std::fs::read_dir(cd) {
                                for cf in ce.flatten() {
                                    let cn = cf.file_name().to_string_lossy().to_string();
                                    if cn.ends_with("-vue") {
                                        if !front_modules.contains(&cn) {
                                            front_modules.push(cn.clone());
                                            let target = find_target_path(&front_parent, &temp_dir, &cn, is_two_zip);
                                            if let Some(t) = target {
                                                copy_dir_all(&cf.path(), &t)?;
                                            }
                                        }
                                    } else if let Some(ref tc) = target_console {
                                        let t = tc.join(&cn);
                                        if !t.exists() {
                                            if cf.path().is_dir() {
                                                std::fs::create_dir_all(&t).ok();
                                                copy_dir_all(&cf.path(), &t)?;
                                            } else {
                                                if let Some(parent) = t.parent() { std::fs::create_dir_all(parent).ok(); }
                                                let _ = std::fs::copy(cf.path(), &t);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let _ = remove_dir(&temp_dir);
                }
            }

            // Also copy fp/front/ directory content (deploy.xml, scripts, template) if present
            let inner_front = fp.join("front");
            if inner_front.exists() && inner_front.is_dir() {
                let target_base = if is_two_zip {
                    front_parent.join("front")
                } else {
                    front_parent.join("front").join("front")
                };
                if let Ok(ie) = std::fs::read_dir(&inner_front) {
                    for item in ie.flatten() {
                        let cn = item.file_name().to_string_lossy().to_string();
                        if cn == "console" { continue; }
                        let t = target_base.join(&cn);
                        if !t.exists() {
                            if item.path().is_dir() {
                                std::fs::create_dir_all(&t).ok();
                                copy_dir_all(&item.path(), &t)?;
                            } else {
                                if let Some(parent) = t.parent() { std::fs::create_dir_all(parent).ok(); }
                                let _ = std::fs::copy(item.path(), &t);
                            }
                        }
                    }
                }
            }
        }
    }

    // Copy deploy.xml from 工程包 if found
    if let Some(ref deploy_xml) = engineering_deploy_xml {
        let target_deploy = if is_two_zip {
            front_parent.join("front").join("deploy.xml")
        } else {
            front_parent.join("front").join("front").join("deploy.xml")
        };
        if let Some(parent) = target_deploy.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let _ = std::fs::copy(deploy_xml, &target_deploy);
    }

    // Re-zip front (preserve front/ prefix in zip entries)
    if has_console {
        if front_parent.join("front").join("front").exists() {
            // Inner front: wrap content in front/ prefix so zip has front/console/...
            let inner_dir = front_parent.join("front").join("front");
            let rezip = save_path.join(".front_inner_rezip");
            std::fs::create_dir_all(&rezip.join("front")).ok();
            let _ = copy_dir_all(&inner_dir, &rezip.join("front"));
            crate::merge_common::zip_dir_sorted(&rezip, &front_parent.join("front").join("front.zip"))?;
            let _ = std::fs::remove_dir_all(&rezip);
            let _ = remove_dir(&inner_dir);
        }
        let front_dir = front_parent.join("front");
        if front_dir.exists() {
            // Outer front: wrap in front/ prefix so outer zip has front/front.zip
            let rezip = save_path.join(".front_outer_rezip");
            std::fs::create_dir_all(&rezip.join("front")).ok();
            let _ = copy_dir_all(&front_dir, &rezip.join("front"));
            crate::merge_common::zip_dir_sorted(&rezip, &front_parent.join("front.zip"))?;
            let _ = std::fs::remove_dir_all(&rezip);
            let _ = remove_dir(&front_dir);
        } else if front_parent.exists() {
            crate::merge_common::zip_dir_sorted(&front_parent, &front_parent.with_extension("zip"))?;
            remove_dir(&front_parent)?;
        }
    }

    Ok(())
}

fn find_target_path(front_parent: &Path, temp_dir: &Path, module: &str, is_two_zip: bool) -> Option<PathBuf> {
    if is_two_zip {
        if let Some(console) = find_console_dir(front_parent) {
            Some(console.join(module))
        } else {
            None
        }
    } else {
        // New format: front_parent/front/front/console/...
        Some(front_parent.join("front").join("front").join("console").join(module))
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.exists() { return Ok(()); }
    if src.is_file() {
        if let Some(name) = src.file_name() {
            let target = dst.join(name);
            if let Some(p) = target.parent() { std::fs::create_dir_all(p).ok(); }
            std::fs::copy(src, &target).map_err(|e| format!("拷贝文件失败 {}", e))?;
        }
        return Ok(());
    }
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let p = entry.path();
            let rel = p.strip_prefix(src).unwrap_or(&p);
            let target = dst.join(rel);
            if p.is_dir() {
                std::fs::create_dir_all(&target).ok();
                copy_dir_all(&p, &target)?;
            } else {
                if let Some(parent) = target.parent() { std::fs::create_dir_all(parent).ok(); }
                std::fs::copy(&p, &target).map_err(|e| format!("拷贝文件失败 {}", e))?;
            }
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
//  2. serverSeePackageMerge
// ═══════════════════════════════════════════════════════════════

fn server_see_package_merge(save_path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut zip_names: Vec<String> = Vec::new();
    let mut zip_map: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(save_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            zip_names.push(name.clone());
            zip_map.insert(name, entry.path());
        }
    }
    sort_chinese(&mut zip_names);
    if zip_names.is_empty() { return Ok(()); }
    let big_name = zip_names.last().cloned().unwrap_or_default();

    // Group service dirs by server name.
    // Extract each server zip IN PLACE (like Java reference):
    //   server/lcpt-xxx.zip → extract to server/lcpt-xxx/ → delete zip
    // This way merged results live directly in the big version's server dir.
    let mut file_map: HashMap<String, Vec<(PathBuf, String)>> = HashMap::new();
    for vname in &zip_names {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let dir = match zip_map.get(vname) { Some(d) => d, None => continue };
        let server_zips = find_see_server_zips(dir);
        for sz in &server_zips {
            let fname = sz.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            // Extract IN PLACE: next to the original zip, then delete the zip
            let out_dir = sz.with_extension("");
            std::fs::create_dir_all(&out_dir).ok();
            unzip_to(sz, &out_dir).map_err(|e| format!("服务端合并: 解压 {} (版本 {}) 失败: {}", sz.display(), vname, e))?;
            let _ = std::fs::remove_file(sz);

            // Check for inner zip (two-zip format: outer/outer.zip)
            let inner_zip = out_dir.join(&fname);
            let svr_dir = if inner_zip.exists() {
                let inner_unzip = out_dir.join("_inner");
                std::fs::create_dir_all(&inner_unzip).ok();
                unzip_to(&inner_zip, &inner_unzip).map_err(|e| format!("解压内部服务端 zip 失败: {}", e))?;
                let _ = std::fs::remove_file(&inner_zip);
                let inner_name = fname.trim_end_matches(".zip");
                // Structure: out_dir/_inner/{inner_name}/
                let candidate = inner_unzip.join(inner_name);
                if candidate.exists() { candidate } else { inner_unzip.clone() }
            } else {
                // Single-zip format: use out_dir to preserve full structure
                // (deploy.xml, scripts/, template/, and the app subdirectory)
                out_dir.clone()
            };

            if svr_dir.exists() {
                let svr_name = svr_dir.file_name().and_then(|n| n.to_str()).unwrap_or(&fname).to_string();
                file_map.entry(svr_name).or_default().push((svr_dir, vname.clone()));
            }
        }
    }

    // Merge services: for each service, merge from oldest → newest
    for (_svr_name, dir_list) in &file_map {
        if dir_list.len() > 1 {
            for i in 0..dir_list.len() - 1 {
                if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
                let (src, _) = &dir_list[i];
                let (tgt, _) = &dir_list[i + 1];
                merge_dirs_two_phase(src, tgt)?;
            }
        }
    }

    // Ensure all merged service dirs exist in the big version's server dir.
    // Services that exist in earlier versions but NOT in the latest (big) version
    // would otherwise be lost during re-zip (which only looks at big version's server dir).
    // Matches Java reference's `targetMap` logic in IFMS6ServiceImpl.serverSeePackageMerge.
    if let Some(big_dir) = zip_map.get(&big_name) {
        let big_server_dir = find_server_dir(big_dir);
        std::fs::create_dir_all(&big_server_dir).ok();
        for (svr_name, dir_list) in &file_map {
            if dir_list.is_empty() { continue; }
            let (last_dir, _) = dir_list.last().ok_or_else(|| format!("目录列表为空: {}", svr_name))?;
            let target_in_big = big_server_dir.join(svr_name);
            if !target_in_big.exists() {
                copy_dir_all(last_dir, &target_in_big)?;
            }
        }
    }

    // Re-zip all service directories in the big version's server dir
    // Zip should contain {app_name}/* structure (e.g. lcpt-datax.zip → lcpt-datax/deploy.xml, lcpt-datax/lib/...)
    if let Some(big_dir) = zip_map.get(&big_name) {
        let big_server_dir = find_server_dir(big_dir);
        if big_server_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&big_server_dir) {
                for entry in entries.flatten() {
                    if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
                    let p = entry.path();
                    if p.is_dir() {
                        let dir_name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                        // Check if directory already has app name subdirectory (from big version extraction)
                        // e.g. lcpt-dxcp-pub-batch/ contains lcpt-dxcp-pub-batch/ inside
                        let already_has_structure = p.join(&dir_name).is_dir();
                        if already_has_structure {
                            // Already has correct outer structure: zip directly (contents become zip entries)
                            crate::merge_common::zip_dir_sorted(&p, &p.with_extension("zip"))?;
                        } else {
                            // Only has inner app files (from older version): wrap before zipping
                            let rezip = save_path.join(format!(".svr_rezip_{}", dir_name));
                            std::fs::create_dir_all(&rezip.join(&dir_name)).ok();
                            let _ = copy_dir_all(&p, &rezip.join(&dir_name));
                            crate::merge_common::zip_dir_sorted(&rezip, &p.with_extension("zip"))?;
                            let _ = std::fs::remove_dir_all(&rezip);
                        }
                        remove_dir(&p)?;
                    }
                }
            }
        }
    }

    Ok(())
}

fn merge_dirs_non_overwrite(src: &Path, tgt: &Path) -> Result<(), String> {
    if !src.exists() { return Ok(()); }
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let p = entry.path();
            let rel = p.strip_prefix(src).unwrap_or(&p);
            let target = tgt.join(rel);
            if p.is_dir() {
                if !target.exists() { std::fs::create_dir_all(&target).ok(); }
                merge_dirs_non_overwrite(&p, &target)?;
            } else {
                let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let is_delete_file = fname == "del_file_list.txt" || fname == "deleteFiles.txt";
                let file_exists = target.exists();
                if !file_exists {
                    if let Some(parent) = target.parent() { std::fs::create_dir_all(parent).ok(); }
                    std::fs::copy(&p, &target).map_err(|e| format!("拷贝文件失败 {}", e))?;
                    // Sync del_file_list.txt → scripts/deleteFiles.txt
                    if is_delete_file {
                        sync_delete_file_to_scripts(&target)?;
                    }
                }
                // Check version regression for existing .class files (non-overwrite → skip new one)
                if file_exists && !is_delete_file && fname.ends_with(".class") {
                    let old_bytes = match std::fs::read(&target) { Ok(b) => b, Err(_) => return Ok(()) };
                    let new_bytes = match std::fs::read(&p) { Ok(b) => b, Err(_) => return Ok(()) };
                    let (ov, nv) = (class_major_version(&old_bytes), class_major_version(&new_bytes));
                    if let (Some(ov), Some(nv)) = (ov, nv) {
                        if nv < ov {
                            let _ = v6_regression_list().lock().map(|mut list| {
                                list.push((target.to_string_lossy().to_string(), ov as u32, nv as u32));
                            });
                        }
                    }
                }
                if file_exists && is_delete_file {
                    // Merge delete lists: combine dedup, source entries take precedence
                    let src_lines = read_lines_or_empty(&p);
                    let tgt_lines = read_lines_or_empty(&target);
                    let mut merged: Vec<String> = src_lines.clone();
                    for line in &tgt_lines {
                        if !merged.contains(line) { merged.push(line.clone()); }
                    }
                    write_lines(&p, &merged)?;
                    // Delete files listed in source from target tree
                    delete_files_listed(&p, tgt)?;
                    sync_delete_file_to_scripts(&target)?;
                }
            }
        }
    }
    Ok(())
}

fn read_lines_or_empty(path: &Path) -> Vec<String> {
    if !path.exists() { return Vec::new(); }
    std::fs::read_to_string(path).ok()
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default()
}

fn write_lines(path: &Path, lines: &[String]) -> Result<(), String> {
    let content = lines.join("\n") + "\n";
    std::fs::write(path, &content).map_err(|e| format!("写文件失败 {}", e))
}

fn delete_files_listed(del_file: &Path, base_dir: &Path) -> Result<(), String> {
    if !del_file.exists() { return Ok(()); }
    let lines = read_lines_or_empty(del_file);
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let target = base_dir.join(trimmed);
        if target.exists() {
            let _ = std::fs::remove_file(&target);
        }
    }
    Ok(())
}

fn sync_delete_file_to_scripts(delete_file_path: &Path) -> Result<(), String> {
    let lines = read_lines_or_empty(delete_file_path);
    if lines.is_empty() { return Ok(()); }
    let parent_name = delete_file_path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Walk up to find scripts/deleteFiles.txt
    let scripts_path = find_scripts_delete_txt(delete_file_path);
    if let Some(sp) = scripts_path {
        let prefixed: Vec<String> = lines.iter()
            .map(|l| format!("/{}{}", parent_name, l))
            .collect();
        write_lines(&sp, &prefixed)?;
    }
    Ok(())
}

fn find_scripts_delete_txt(start: &Path) -> Option<PathBuf> {
    let mut dir = start.parent()?;
    for _ in 0..8 {
        if dir.join("scripts").join("deleteFiles.txt").exists() {
            return Some(dir.join("scripts").join("deleteFiles.txt"));
        }
        if dir.join("scripts").is_dir() {
            if let Ok(entries) = std::fs::read_dir(dir.join("scripts")) {
                for entry in entries.flatten() {
                    let n = entry.file_name().to_string_lossy().to_string();
                    if n.starts_with("deleteFiles") && n.ends_with(".txt") {
                        return Some(entry.path());
                    }
                }
            }
        }
        dir = dir.parent()?;
    }
    None
}

/// Two-phase merge (Java FileCopyUtils2.mergeServer equivalent):
/// Phase 1: reverse copy target→source with deletion handling
/// Phase 2: forward copy source→target non-overwrite
fn merge_dirs_two_phase(src: &Path, tgt: &Path) -> Result<(), String> {
    if !src.exists() { return Ok(()); }
    // Phase 1: copy from newer (tgt) to older (src) with deletion handling
    copy_dirs_with_deletions(tgt, src)?;
    // Phase 2: copy from older (src) to newer (tgt) non-overwrite
    merge_dirs_non_overwrite(src, tgt)?;
    Ok(())
}

/// Java copyDirectoryNonOverwriteOnlyIncludingDeleteFile equivalent:
/// Copy from src to tgt, but when encountering del_file_list.txt in src:
/// read the file list and delete those files from tgt.
/// If both have del_file_list.txt: merge them (source takes precedence).
fn copy_dirs_with_deletions(src: &Path, tgt: &Path) -> Result<(), String> {
    if !src.exists() { return Ok(()); }
    if !tgt.exists() { std::fs::create_dir_all(tgt).ok(); }
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let p = entry.path();
            let rel = p.strip_prefix(src).unwrap_or(&p);
            let target = tgt.join(rel);
            if p.is_dir() {
                copy_dirs_with_deletions(&p, &target)?;
            } else {
                let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let is_delete_file = fname == "del_file_list.txt" || fname == "deleteFiles.txt";
                if !target.exists() {
                    if is_delete_file {
                        // Target doesn't have this delete list → read from source, delete listed files from target
                        delete_files_listed(&p, tgt)?;
                    }
                    if let Some(parent) = target.parent() { std::fs::create_dir_all(parent).ok(); }
                    std::fs::copy(&p, &target).map_err(|e| format!("拷贝文件失败 {}", e))?;
                } else if is_delete_file {
                    // Both have delete files: merge (source entries take precedence)
                    let src_lines = read_lines_or_empty(&p);
                    let tgt_lines = read_lines_or_empty(&target);
                    // Delete files from source list in target directory
                    delete_files_listed(&p, tgt)?;
                    let mut merged: Vec<String> = src_lines.clone();
                    for line in &tgt_lines {
                        if !merged.contains(line) { merged.push(line.clone()); }
                    }
                    write_lines(&p, &merged)?;
                }
            }
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
//  Non-SEE merge functions (IFMS6 non-SEE package)
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NonSeeMergeOptions {
    pub src_dir: String,
    pub dst_dir: String,
    pub all_merge_versions: Vec<String>,
}

fn front_non_see_package_merge(save_path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut zip_names: Vec<String> = Vec::new();
    let mut zip_map: HashMap<String, PathBuf> = HashMap::new();
    let mut dir_map: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(save_path) {
        for entry in entries.flatten() {
            let p = entry.path(); let n = entry.file_name().to_string_lossy().to_string();
            if p.is_file() && p.extension().map(|x| x == "zip").unwrap_or(false) {
                zip_names.push(n.clone()); zip_map.insert(n, p);
            }
        }
    }
    sort_chinese_reverse(&mut zip_names);
    if zip_names.is_empty() { return Ok(()); }
    for vname in &zip_names {
        let p = zip_map.get(vname).ok_or_else(|| format!("未找到版本: {}", vname))?;
        let d = save_path.join(vname); std::fs::create_dir_all(&d).ok();
        let _ = unzip_to(p, &d); let _ = std::fs::remove_file(p);
        dir_map.insert(vname.clone(), d);
    }
    for i in 0..zip_names.len().saturating_sub(1) {
        let src = find_sub_version_dir(&dir_map[&zip_names[i]]);
        let tgt = find_sub_version_dir(&dir_map[&zip_names[i + 1]]);
        if src.exists() && tgt.exists() { merge_dirs_non_overwrite(&src, &tgt)?; }
    }
    Ok(())
}

fn server_non_see_package_merge(save_path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut zip_names: Vec<String> = Vec::new();
    let mut zip_map: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(save_path) {
        for entry in entries.flatten() {
            let n = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() { zip_names.push(n.clone()); zip_map.insert(n, entry.path()); }
        }
    }
    sort_chinese(&mut zip_names);
    for i in 0..zip_names.len().saturating_sub(1) {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let src = find_server_dir(&zip_map[&zip_names[i]]);
        let tgt = find_server_dir(&zip_map[&zip_names[i + 1]]);
        if src.exists() && tgt.exists() { merge_dirs_non_overwrite(&src, &tgt)?; }
    }
    Ok(())
}

fn db_non_see_package_merge(save_path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut zip_names: Vec<String> = Vec::new();
    let mut zip_map: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(save_path) {
        for entry in entries.flatten() {
            let n = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() { zip_names.push(n.clone()); zip_map.insert(n, entry.path()); }
        }
    }
    sort_chinese(&mut zip_names);
    for i in 0..zip_names.len().saturating_sub(1) {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let src = find_non_db_zip(&zip_map[&zip_names[i]]);
        let tgt = find_non_db_zip(&zip_map[&zip_names[i + 1]]);
        if src.exists() && tgt.exists() { merge_dirs_non_overwrite(&src, &tgt)?; }
    }
    Ok(())
}

/// IFMS6 **non-SEE** 包合并（参考仓里与 [`merge`] 并列的第二个入口，界面上是
/// `SpMerge.vue` 的另一个模式）。
///
/// 源仓 `start_non_see_merge` 用的是另一族事件（`ifms6-non-see-merge-*`）与
/// `thread::spawn`；这里照同一套规矩接到 [`crate::MergeReporter`] 上并同步返回。
/// 取消语义与源仓一致：置 `cancel_flag()` 后清掉工作目录、立刻返回。
pub fn non_see_merge(
    app: &dyn MergeReporter,
    options: &NonSeeMergeOptions,
) -> Result<Ifms6NonSeeResult, String> {
    reset_cancel();
    let _ = v6_regression_list().lock().map(|mut list| list.clear());
    let cancel = cancel_flag().clone();
    let start = Instant::now();
    let src_dir = Path::new(&options.src_dir);
    let dst_dir = Path::new(&options.dst_dir);
    if !src_dir.is_dir() {
        // 源仓这里发一条 success=false 的 done 事件就结束；同步版直接报错。
        return Err(format!("源目录不存在: {}", options.src_dir));
    }
    let work_dir_name = format!("IFMS6-non-see-merge-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let work_dir = dst_dir.join(&work_dir_name);
    let _ = std::fs::create_dir_all(&work_dir);

    // Copy version dirs
    ns_emit("copy", 5, Some("复制版本包到工作目录..."), app);
    if let Ok(entries) = std::fs::read_dir(src_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = entry.file_name();
            let target = work_dir.join(&name);
            if p.is_dir() { let _ = copy_dir_all(&p, &target); }
        }
    }

    ns_emit("front", 20, Some("前端合并 (non-SEE)..."), app);
    let front_merged = front_non_see_package_merge(&work_dir, &cancel).is_ok();
    if cancel.load(Ordering::Relaxed) { let _ = remove_dir(&work_dir); return Err("用户取消".to_string()); }
    ns_emit("server", 45, Some("服务端合并 (non-SEE)..."), app);
    let server_merged = server_non_see_package_merge(&work_dir, &cancel).is_ok();
    if cancel.load(Ordering::Relaxed) { let _ = remove_dir(&work_dir); return Err("用户取消".to_string()); }
    ns_emit("db", 70, Some("数据库合并 (non-SEE)..."), app);
    let db_merged = db_non_see_package_merge(&work_dir, &cancel).is_ok();
    if cancel.load(Ordering::Relaxed) { let _ = remove_dir(&work_dir); return Err("用户取消".to_string()); }
    ns_emit("final", 90, Some("最终打包..."), app);

    // Zip the entire work_dir content
    let prefix = options.all_merge_versions.first().cloned().unwrap_or_default();
    let suffix = options.all_merge_versions.last().cloned().unwrap_or_default();
    let merge_name = format!("{}-{}.zip", prefix, suffix);
    let output_path = dst_dir.join(&merge_name);
    let _ = std::fs::remove_file(&output_path);
    let _ = crate::merge_common::zip_dir_sorted(&work_dir, &output_path);
    let _ = remove_dir(&work_dir);

    ns_emit("done", 100, Some("合并完成"), app);
    Ok(Ifms6NonSeeResult {
        success: true,
        output_path: output_path.to_string_lossy().to_string(),
        statistics: Ifms6NonSeeStats {
            versions: options.all_merge_versions.len(),
            front_merged,
            server_merged,
            db_merged,
            total_duration_ms: start.elapsed().as_millis() as u64,
        },
    })
}

fn ns_emit(stage: &str, pct: u32, msg: Option<&str>, app: &dyn MergeReporter) {
    app.progress(stage, pct, 0, 0, msg.map(str::to_owned));
}

// ═══════════════════════════════════════════════════════════════
//  3. dbSeePackageMerge
// ═══════════════════════════════════════════════════════════════

fn db_see_package_merge(save_path: &Path, cancel: &AtomicBool) -> Result<usize, String> {
    let mut zip_names: Vec<String> = Vec::new();
    let mut zip_map: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(save_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            zip_names.push(name.clone());
            zip_map.insert(name.clone(), entry.path());
        }
    }
    sort_chinese(&mut zip_names);
    let big_name = zip_names.last().cloned().unwrap_or_default();

    // Collect db zip groups
    let mut db_zip_map: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for vname in &zip_names {
        if let Some(dir) = zip_map.get(vname) {
            find_db_zip_map(dir, &mut db_zip_map);
        }
    }

    let mut merged_count = 0usize;
    for (key, files) in &db_zip_map {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        if files.len() > 1 {
            let mut dir_list: Vec<PathBuf> = Vec::new();
            for f in files {
                let d = save_path.join(format!(".db_{}_{}", key, merged_count));
                std::fs::create_dir_all(&d).ok();
                unzip_to(f, &d).map_err(|e| format!("数据库合并: 解压 {} 失败: {}", f.display(), e))?;
                dir_list.push(d);
            }
            for i in 0..dir_list.len() - 1 {
                let src_sql = find_file_in_dir(&dir_list[i], "sqls");
                let tgt_sql = find_file_in_dir(&dir_list[i + 1], "sqls");
                match (&src_sql, &tgt_sql) {
                    (Some(s), Some(t)) => {
                        merge_dirs_non_overwrite(s.parent().unwrap_or(s), t.parent().unwrap_or(t))?;
                    }
                    (Some(s), None) => {
                        let src_str = s.to_string_lossy().to_string();
                        let src_base = dir_list[i].to_string_lossy().to_string();
                        let suffix = src_str.strip_prefix(&src_base).unwrap_or("");
                        let tgt_path = dir_list[i + 1].join(suffix.trim_start_matches('/'));
                        if let Some(parent) = tgt_path.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        merge_dirs_non_overwrite(s.parent().unwrap_or(s), tgt_path.parent().unwrap_or(&tgt_path))?;
                    }
                    _ => {}
                }
                let template_path = dir_list[i + 1].join(key.trim_end_matches(".zip")).join("template");
                std::fs::create_dir_all(&template_path).ok();
            }
            // Re-zip the last one
            let last_dir = dir_list.last().ok_or_else(|| format!("目录列表为空: {}", key))?;
            let last_zip = last_dir.with_extension("zip");
            let _ = crate::merge_common::zip_dir_sorted(last_dir, &last_zip);
            let _ = remove_dir(last_dir);

            // Copy to latest version (use original key as filename, not temp name)
            if !last_zip.to_string_lossy().contains(&big_name) {
                if let Some(big_dir) = zip_map.get(&big_name) {
                    let big_db_dir = find_db_dir(big_dir);
                    std::fs::create_dir_all(&big_db_dir).ok();
                    let db_filename = if key.ends_with(".zip") { key.clone() } else { format!("{}.zip", key) };
                    let _ = std::fs::copy(&last_zip, big_db_dir.join(&db_filename));
                }
            }
            let _ = std::fs::remove_file(&last_zip);
            for d in &dir_list { let _ = remove_dir(d); }
        } else if files.len() == 1 {
            // Copy single db zip to latest version
            if let Some(big_dir) = zip_map.get(&big_name) {
                let big_db_dir = find_db_dir(big_dir);
                if !files[0].to_string_lossy().contains(&big_name) {
                    std::fs::create_dir_all(&big_db_dir).ok();
                    if let Some(fname) = files[0].file_name() {
                        let _ = std::fs::copy(&files[0], big_db_dir.join(fname));
                    }
                }
            }
        }
        merged_count += 1;
    }
    Ok(merged_count)
}

// ═══════════════════════════════════════════════════════════════
//  4. ifmcounterSeePackageMerge
// ═══════════════════════════════════════════════════════════════

fn ifmcounter_see_package_merge(save_path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut zip_names: Vec<String> = Vec::new();
    let mut zip_map: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(save_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            zip_names.push(name.clone());
            zip_map.insert(name, entry.path());
        }
    }
    sort_chinese(&mut zip_names);
    let big_name = zip_names.last().cloned().unwrap_or_default();

    // Find ifmcounter.zip in each version (only search within version dirs, skip temp)
    let mut counter_dirs: Vec<PathBuf> = Vec::new();
    for vname in &zip_names {
        if let Some(dir) = zip_map.get(vname) {
            if let Some(path) = find_file_in_dir(dir, "ifmcounter.zip") {
                counter_dirs.push(path);
            }
        }
    }

    if counter_dirs.is_empty() { return Ok(()); }

    if counter_dirs.len() == 1 {
        // Single counter: copy to latest version
        let big_dir = zip_map.get(&big_name).ok_or_else(|| format!("未找到大版本: {}", big_name))?;
        let big_counter_dir = find_ifmcounter_dir(big_dir);
        std::fs::create_dir_all(&big_counter_dir).ok();
        let _ = std::fs::copy(&counter_dirs[0], big_counter_dir.join("ifmcounter.zip"));
    } else {
        let mut dir_list: Vec<PathBuf> = Vec::new();
        let mut has_inner: Vec<bool> = Vec::new();
        for cf in &counter_dirs {
            let d = save_path.join(format!(".ctr_{}", dir_list.len()));
            std::fs::create_dir_all(&d).ok();
            unzip_to(cf, &d).map_err(|e| format!("ifmcounter 合并: 解压 {} 失败: {}", cf.display(), e))?;
            let _ = std::fs::remove_file(cf);

            // Handle inner ifmcounter.zip
            let inner = find_file_in_dir(&d, "ifmcounter.zip");
            let (target_dir, is_inner) = if let Some(ref ip) = inner {
                let id = d.join("_inner");
                std::fs::create_dir_all(&id).ok();
                unzip_to(ip, &id).map_err(|e| format!("解压内部 ifmcounter.zip 失败: {}", e))?;
                let _ = std::fs::remove_file(ip);
                (id.join("ifmcounter"), true)
            } else {
                (d.clone(), false)
            };
            if target_dir.exists() {
                dir_list.push(target_dir);
                has_inner.push(is_inner);
            }
        }

        for i in 0..dir_list.len().saturating_sub(1) {
            merge_dirs_two_phase(&dir_list[i], &dir_list[i + 1])?;
        }

        if let Some((last, &is_inner)) = dir_list.last().zip(has_inner.last()) {
            // 有内层 zip → 压缩 d/_inner（last 的 parent）
            // 无内层 zip → 压缩 d 本身（last 就是 d）
            let dir_to_zip = if is_inner {
                last.parent().unwrap_or(last).to_path_buf()
            } else {
                last.to_path_buf()
            };
            let _ = crate::merge_common::zip_dir_sorted(&dir_to_zip, &dir_to_zip.with_extension("zip"));
            let _ = remove_dir(&dir_to_zip);

            let big_dir = zip_map.get(&big_name).ok_or_else(|| format!("未找到大版本: {}", big_name))?;
            let big_ctr_dir = find_ifmcounter_dir(big_dir);
            std::fs::create_dir_all(&big_ctr_dir).ok();
            let _ = std::fs::copy(&dir_to_zip.with_extension("zip"), big_ctr_dir.join("ifmcounter.zip"));
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
//  Non-SEE modes (simple directory-level merge)
// ═══════════════════════════════════════════════════════════════

/// Java frontNonSeePackageMerge: unzip all, sort reverse, copy front dirs sequentially


// ═══════════════════════════════════════════════════════════════
//  5. otherPackageMerge (升级说明等)
// ═══════════════════════════════════════════════════════════════

fn other_package_merge(save_path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut zip_names: Vec<String> = Vec::new();
    let mut zip_map: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(save_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            zip_names.push(name.clone());
            zip_map.insert(name, entry.path());
        }
    }
    sort_chinese(&mut zip_names);
    let big_name = zip_names.last().cloned().unwrap_or_default();

    // Find "SP包升级说明" dirs
    let mut upgrade_dirs: Vec<PathBuf> = Vec::new();
    for vname in &zip_names {
        if let Some(dir) = zip_map.get(vname) {
            let upgrade_dir = find_upgrade_dir(dir);
            if upgrade_dir.exists() { upgrade_dirs.push(upgrade_dir); }
        }
    }

    if upgrade_dirs.len() == 1 {
        let big_dir = zip_map.get(&big_name).ok_or_else(|| format!("未找到大版本: {}", big_name))?;
        if !upgrade_dirs[0].to_string_lossy().contains(&big_name) {
            copy_dir_all(&upgrade_dirs[0], big_dir)?;
        }
    } else if upgrade_dirs.len() > 1 {
        for i in 0..upgrade_dirs.len() - 1 {
            merge_dirs_non_overwrite(&upgrade_dirs[i], &upgrade_dirs[i + 1])?;
        }
        // Ensure merged upgrade content lands in big version
        let last_upgrade = upgrade_dirs.last().ok_or_else(|| "升级目录列表为空".to_string())?;
        if !last_upgrade.to_string_lossy().contains(&big_name) {
            if let Some(big_dir) = zip_map.get(&big_name) {
                let big_upgrade_dir = find_upgrade_dir(big_dir);
                if !big_upgrade_dir.exists() {
                    let _ = std::fs::create_dir_all(&big_upgrade_dir);
                    let _ = copy_dir_all(last_upgrade, &big_upgrade_dir);
                }
            }
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
//  6. spconfigZipPackageMerge
// ═══════════════════════════════════════════════════════════════

fn spconfig_zip_package_merge(save_path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut zip_names: Vec<String> = Vec::new();
    let mut zip_map: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(save_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            zip_names.push(name.clone());
            zip_map.insert(name, entry.path());
        }
    }
    sort_chinese(&mut zip_names);
    let big_name = zip_names.last().cloned().unwrap_or_default();

    // Find spconfig.zip or spconfig_wis.zip
    let mut sp_dirs: Vec<PathBuf> = Vec::new();
    for vname in &zip_names {
        if let Some(dir) = zip_map.get(vname) {
            let sp = find_file_in_dir(dir, "spconfig.zip").or_else(|| find_file_in_dir(dir, "spconfig_wis.zip"));
            if let Some(p) = sp { sp_dirs.push(p); }
        }
    }

    if sp_dirs.is_empty() { return Ok(()); }

    if sp_dirs.len() == 1 {
        let big_dir = zip_map.get(&big_name).ok_or_else(|| format!("未找到大版本: {}", big_name))?;
        if !sp_dirs[0].to_string_lossy().contains(&big_name) {
            let big_sp_dir = find_spconfig_dir(big_dir);
            std::fs::create_dir_all(&big_sp_dir).ok();
            if let Some(fname) = sp_dirs[0].file_name() {
                let _ = std::fs::copy(&sp_dirs[0], big_sp_dir.join(fname));
            }
        }
    } else {
        let mut dir_list: Vec<PathBuf> = Vec::new();
        for sp in &sp_dirs {
            let d = save_path.join(format!(".sp_{}", dir_list.len()));
            std::fs::create_dir_all(&d).ok();
            unzip_to(sp, &d).map_err(|e| format!("spconfig 合并: 解压 {} 失败: {}", sp.display(), e))?;
            let _ = std::fs::remove_file(sp);
            dir_list.push(d);
        }
        for i in 0..dir_list.len() - 1 {
            merge_dirs_non_overwrite(&dir_list[i], &dir_list[i + 1])?;
        }
        if let Some(last) = dir_list.last() {
            let _ = crate::merge_common::zip_dir_sorted(last, &last.with_extension("zip"));
            let _ = remove_dir(last);
            let big_dir = zip_map.get(&big_name).ok_or_else(|| format!("未找到大版本: {}", big_name))?;
            if !last.with_extension("zip").to_string_lossy().contains(&big_name) {
                let big_sp_dir = find_spconfig_dir(big_dir);
                std::fs::create_dir_all(&big_sp_dir).ok();
                let _ = std::fs::copy(&last.with_extension("zip"), big_sp_dir.join("spconfig.zip"));
            }
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
//  7. finalZipPackageMerge
// ═══════════════════════════════════════════════════════════════

fn final_zip_package_merge(save_path: &Path, merge_name: &str) -> Result<PathBuf, String> {
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(save_path) {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            if name.starts_with("merge_sql") { continue; }
            let p = entry.path();
            if p.is_dir() {
                let display_name = name.strip_suffix(".zip").unwrap_or(&name).to_string();
                entries.push((display_name, p));
            }
        }
    }
    if entries.is_empty() {
        return Err("未找到最新版本目录".to_string());
    }
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let (_, latest_dir) = entries.last().ok_or_else(|| "条目列表为空".to_string())?;

    let tmp_zip = save_path.join(format!(".final_{}", merge_name));
    let _ = std::fs::remove_file(&tmp_zip);
    crate::merge_common::zip_dir_sorted(latest_dir, &tmp_zip)?;
    let _ = remove_dir(latest_dir);

    let result_path = save_path.join(merge_name);
    let _ = std::fs::remove_file(&result_path);
    std::fs::rename(&tmp_zip, &result_path)
        .map_err(|e| format!("重命名失败 {}", e))?;
    Ok(result_path)
}

// ═══════════════════════════════════════════════════════════════
//  Main Merge Logic
// ═══════════════════════════════════════════════════════════════

fn timed_step<F>(app: &dyn MergeReporter, label: &str, f: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
{
    let start = Instant::now();
    let r = f();
    let ms = start.elapsed().as_millis() as u64;
    emit_log(app, "info", &format!("[{}] {} ({}ms)", if r.is_ok() { "OK" } else { "FAIL" }, label, ms), Some(ms));
    r
}

fn run_merge(app: &dyn MergeReporter, src_dir: &Path, dst_dir: &Path,
    options: &Ifms6MergeOptions, cancel: &AtomicBool,
) -> Result<Ifms6MergeResult, String> {
    let start = Instant::now();
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");

    // Create output dir inside dst_dir
    let output_dir = dst_dir.join(format!("merge_{}", ts));
    std::fs::create_dir_all(&output_dir).map_err(|e| format!("创建输出目录失败 {}", e))?;

    // Create temp working dir inside output dir (same filesystem = no cross-device rename issues)
    let work_dir = output_dir.join(format!("_work_{}", ts));
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("创建工作目录失败 {}", e))?;

    // Extract version entries directly from src_dir into work_dir (skip copy, src dir is read-only)
    let extract_start = Instant::now();
    let name_set: std::collections::HashSet<String> = options.all_merge_versions.iter()
        .map(|v| v.strip_suffix(".zip").unwrap_or(v).to_string())
        .collect();
    let entries = std::fs::read_dir(src_dir).map_err(|e| format!("读取源目录失败: {}", e))?;
    let mut extracted = 0u32;
    for entry in entries {
        let entry = entry.map_err(|e| format!("读取源目录条目失败: {}", e))?;
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let name_noext = name.strip_suffix(".zip").unwrap_or(&name).to_string();
        if !name_set.contains(&name_noext) { continue; }
        emit_log(app, "info", &format!("  提取 {}{}", name, if p.is_dir() { "" } else { " (解压)" }), None);
        if p.is_dir() {
            let target = work_dir.join(&name);
            copy_dir_all(&p, &target)?;
            extracted += 1;
        } else if p.is_file() && name.ends_with(".zip") {
            let target = work_dir.join(&name_noext);
            std::fs::create_dir_all(&target).map_err(|e| format!("创建目录 {} 失败: {}", target.display(), e))?;
            unzip_to(&p, &target)?;
            extracted += 1;
        }
    }
    emit_log(app, "info", &format!("[OK] 提取版本 {} 个 ({}ms)", extracted, extract_start.elapsed().as_millis() as u64), Some(extract_start.elapsed().as_millis() as u64));
    if extracted != name_set.len() as u32 {
        emit_log(app, "warn", &format!("请求 {} 个版本, 实际提取 {} 个 — 请检查源目录中版本包是否完整", name_set.len(), extracted), None);
    }

    let mut front_ok = false;
    let mut server_ok = false;
    let mut db_ok = false;
    let mut counter_ok = false;
    let mut sp_ok = false;
    let version_count = options.all_merge_versions.len();

    fn debug_dir_state(app: &dyn MergeReporter, label: &str, path: &Path) {
        let overview = debug_list_dir(path);
        let mut details = String::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let n = entry.file_name().to_string_lossy().to_string();
                if n.starts_with('.') { continue; }
                let p = entry.path();
                if p.is_dir() {
                    let subs: Vec<String> = std::fs::read_dir(&p).ok().into_iter().flatten()
                        .filter_map(|e| e.ok())
                        .map(|e| {
                            let en = e.file_name().to_string_lossy().to_string();
                            if e.path().is_dir() { format!("d:{}", en) } else { format!("f:{}", en) }
                        }).collect();
                    details.push_str(&format!(" [{}]:{{{}}}", n, subs.join(",")));
                }
            }
        }
        emit_log(app, "debug", &format!("[合并状态] {}: {}", label, overview), None);
        if !details.is_empty() {
            emit_log(app, "debug", &format!("[合并状态] {} 详情:{}", label, details), None);
        }
    }

    // IFMS6 包合并 (SEE mode): module-level incremental merge
    debug_dir_state(app, "前端合并前", &work_dir);
    timed_step(app, "前端包合并", || front_see_package_merge(&work_dir, cancel))?;
    front_ok = true;
    debug_dir_state(app, "前端合并后", &work_dir);

    if cancel.load(Ordering::Relaxed) { let _ = remove_dir(&work_dir); return Err("用户取消".to_string()); }
    debug_dir_state(app, "服务端合并前", &work_dir);
    timed_step(app, "服务端包合并", || server_see_package_merge(&work_dir, cancel))?;
    server_ok = true;
    debug_dir_state(app, "服务端合并后", &work_dir);

    if cancel.load(Ordering::Relaxed) { let _ = remove_dir(&work_dir); return Err("用户取消".to_string()); }
    emit_progress(app, "db_merge", 55, Some("数据库包合并..."));
    let db_start = Instant::now();
    debug_dir_state(app, "数据库合并前", &work_dir);
    let db_count = db_see_package_merge(&work_dir, cancel)?;
    db_ok = db_count > 0;
    emit_log(app, "info", &format!("[OK] 数据库包合并 ({}ms, {}分组)", db_start.elapsed().as_millis() as u64, db_count), Some(db_start.elapsed().as_millis() as u64));
    debug_dir_state(app, "数据库合并后", &work_dir);

    if cancel.load(Ordering::Relaxed) { let _ = remove_dir(&work_dir); return Err("用户取消".to_string()); }
    debug_dir_state(app, "ifmcounter合并前", &work_dir);
    timed_step(app, "ifmcounter 合并", || ifmcounter_see_package_merge(&work_dir, cancel))?;
    counter_ok = true;
    debug_dir_state(app, "ifmcounter合并后", &work_dir);

    if cancel.load(Ordering::Relaxed) { let _ = remove_dir(&work_dir); return Err("用户取消".to_string()); }
    debug_dir_state(app, "其他文件合并前", &work_dir);
    timed_step(app, "其他文件合并", || other_package_merge(&work_dir, cancel))?;
    debug_dir_state(app, "其他文件合并后", &work_dir);

    if cancel.load(Ordering::Relaxed) { let _ = remove_dir(&work_dir); return Err("用户取消".to_string()); }
    debug_dir_state(app, "spconfig合并前", &work_dir);
    timed_step(app, "spconfig 合并", || spconfig_zip_package_merge(&work_dir, cancel))?;
    sp_ok = true;
    debug_dir_state(app, "spconfig合并后", &work_dir);

    let prefix = options.all_merge_versions.first().cloned().unwrap_or_default();
    let prefix_stripped = prefix.strip_suffix(".zip").unwrap_or(&prefix);
    let target_stripped = options.target_version.strip_suffix(".zip").unwrap_or(&options.target_version);

    // SQL merge (optional) — use work_dir content (all version dirs still present), write to output_dir
    if options.include_sql_merge.unwrap_or(false) {
        emit_log(app, "info", "开始 SQL 文件合并...", None);
        let sql_start = Instant::now();
        let sql_work = work_dir.join(".sql_merge_work");
        let _ = std::fs::create_dir_all(&sql_work);
        let mut db_found = 0usize;
        let mut ver_found = 0usize;
        let mut sqls_renamed = 0usize;
        // Scan each version dir in work_dir for spsql/db-*.zip
        if let Ok(entries) = std::fs::read_dir(&work_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                if name.starts_with("merge_sql") { continue; }
                let p = entry.path();
                if !p.is_dir() { continue; }
                ver_found += 1;
                let spsql_dir = find_non_db_zip(&p);
                emit_log(app, "debug", &format!("  SQL: 检查版本 {} spsql={} exist={}", name, spsql_dir.display(), spsql_dir.is_dir()), None);
                if !spsql_dir.is_dir() { continue; }
                if let Ok(db_entries) = std::fs::read_dir(&spsql_dir) {
                    for db_entry in db_entries.flatten() {
                        let db_path = db_entry.path();
                        let db_name = db_entry.file_name().to_string_lossy().to_string();
                        if !(db_name.starts_with("db") && db_name.ends_with(".zip")) { continue; }
                        db_found += 1;
                        emit_log(app, "debug", &format!("  SQL: 找到 db zip {}", db_path.display()), None);
                        let ex = sql_work.join(format!("{}_{}", name, db_name));
                        std::fs::create_dir_all(&ex).ok();
                        let extracted = unzip_to(&db_path, &ex).unwrap_or(0);
                        emit_log(app, "debug", &format!("  SQL: 解压 {} 个文件到 {}", extracted, ex.display()), None);
                        // Find sqls/ — may be at root or inside a single subdir (db/, db-$TACODE/)
                        let sqls_dir = find_sqls_in_extracted(&ex);
                        if let Some(ref sqls) = sqls_dir {
                            let sp = ex.join("spsql");
                            let _ = std::fs::remove_dir_all(&sp);
                            let _ = std::fs::rename(sqls, &sp);
                            sqls_renamed += 1;
                            emit_log(app, "debug", &format!("  SQL: 找到 sqls 并重命名→spsql {}", sp.display()), None);
                        } else {
                            emit_log(app, "debug", &format!("  SQL: 未找到 sqls 目录"), None);
                        }
                    }
                }
            }
        }
        emit_log(app, "info", &format!("SQL: 版本={} db-zip={} sqls→spsql={}", ver_found, db_found, sqls_renamed), None);
        if let Ok(out) = std::fs::read_dir(&sql_work) {
            let names: Vec<String> = out.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string())).collect();
            emit_log(app, "info", &format!("SQL: sql_work 内容 {:?}", names), None);
        }
        let count = sql_merge::merge_sql_tree(&sql_work, cancel)?;
        if let Ok(sql_out) = std::fs::read_dir(&sql_work) {
            for sql_entry in sql_out.flatten() {
                let n = sql_entry.file_name().to_string_lossy().to_string();
                if n.starts_with("merge_sql") && sql_entry.path().is_dir() {
                    let target = output_dir.join(&n);
                    let _ = std::fs::remove_dir_all(&target);
                    // Try rename first; fall back to copy+delete across filesystems
                    if std::fs::rename(sql_entry.path(), &target).is_err() {
                        let _ = copy_dir_all(&sql_entry.path(), &target);
                        let _ = std::fs::remove_dir_all(sql_entry.path());
                    }
                    // Rename SQL files inside to include version range prefix
                    let version_label = format!("{}-{}", prefix_stripped, target_stripped);
                    for sql_file in walkdir::WalkDir::new(&target)
                        .into_iter().filter_map(|e| e.ok())
                        .filter(|e| e.file_type().is_file() && e.path().extension().map_or(false, |x| x == "sql"))
                    {
                        let path = sql_file.path().to_path_buf();
                        let dir = path.parent().ok_or_else(|| format!("路径没有父目录: {}", path.display()))?.to_path_buf();
                        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                        if fname.starts_with(&version_label) { continue; }
                        let new_fname = format!("{}_{}", version_label, fname);
                        let new_path = dir.join(&new_fname);
                        let _ = std::fs::rename(&path, &new_path);
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&sql_work);
        let sql_elapsed = sql_start.elapsed().as_millis() as u64;
        emit_log(app, "info", &format!("SQL 合并: {} 个分组, 耗时 {:.1}s", count, sql_elapsed as f64 / 1000.0), Some(sql_elapsed));
    }

    // Final packaging (common to both modes) — runs after SQL merge so all version dirs are still in work_dir
    if cancel.load(Ordering::Relaxed) { let _ = remove_dir(&work_dir); return Err("用户取消".to_string()); }
    emit_progress(app, "final_pack", 95, Some("最终打包..."));
    let final_start = Instant::now();
    let merge_name = format!("{}-{}.zip", prefix_stripped, target_stripped);

    emit_log(app, "info", &format!("打包前: {}", debug_list_dir(&work_dir)), None);

    // Debug: show what's in each version directory
    if let Ok(entries) = std::fs::read_dir(&work_dir) {
        for entry in entries.flatten() {
            let n = entry.file_name().to_string_lossy().to_string();
            if n.starts_with('.') || n.starts_with("merge_sql") { continue; }
            let p = entry.path();
            if p.is_dir() {
                let subs: Vec<String> = std::fs::read_dir(&p).ok().into_iter().flatten()
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let ename = e.file_name().to_string_lossy().to_string();
                        if e.path().is_dir() { format!("d:{}", ename) } else { format!("f:{}", ename) }
                    }).collect();
                emit_log(app, "info", &format!("  目录内容 {}: [{}]", n, subs.join(", ")), None);
            }
        }
    }

    emit_log(app, "info", &format!("最终打包: {}", merge_name), None);
    let result_path = final_zip_package_merge(&work_dir, &merge_name)?;
    emit_log(app, "info", &format!("[OK] 最终打包 ({}ms)", final_start.elapsed().as_millis() as u64), Some(final_start.elapsed().as_millis() as u64));
    emit_log(app, "info", &format!("合并完成: {}", result_path.display()), None);

    // Move final zip to output dir
    let final_path = output_dir.join(&merge_name);
    let _ = std::fs::remove_file(&final_path);
    std::fs::rename(&result_path, &final_path)
        .or_else(|_| {
            std::fs::copy(&result_path, &final_path).and_then(|_| {
                std::fs::remove_file(&result_path).ok();
                Ok(())
            })
        })
        .map_err(|e| format!("移动最终文件失败: {}", e))?;
    emit_log(app, "info", &format!("结果文件: {}", final_path.display()), None);

    // Write regression report
    if let Ok(list) = v6_regression_list().lock() {
        if !list.is_empty() {
            let report = output_dir.join("版本回退核对清单.txt");
            let mut content = String::from("文件路径\t原版本\t新版本\n");
            for (path, old, new) in list.iter() {
                content.push_str(&format!("{}\t{}\t{}\n", path, old, new));
            }
            let _ = std::fs::write(&report, &content);
        }
    }

    let _ = remove_dir(&work_dir);

    let elapsed = start.elapsed().as_millis() as u64;
    Ok(Ifms6MergeResult {
        success: true,
        output_path: final_path.to_string_lossy().to_string(),
        statistics: Ifms6MergeStats {
            versions: version_count,
            front_merged: front_ok,
            server_merged: server_ok,
            db_merged: db_ok,
            ifmcounter_merged: counter_ok,
            spconfig_merged: sp_ok,
            total_duration_ms: elapsed,
        },
    })
}

// ── Version Scanning ──

/// 扫源目录下的 IFMS6 版本条目（目录或 `.zip`），按中文序排序。
///
/// 源仓这是 `#[tauri::command]`：参考页面点「重新扫描」时由前端 invoke 拿列表，
/// 填进「待合并版本列表」文本框。process 轨的插件进程不能被界面反向调用，所以
/// 这里退成普通函数，由 [`merge`] 在 `all_merge_versions` 为空时自己调它——
/// 界面上等价于「留空 = 直接扫全部版本」。
pub fn scan_versions(src_dir: &str) -> Result<Vec<String>, String> {
    let path = PathBuf::from(&src_dir);
    if !path.exists() {
        return Err("目录不存在".to_string());
    }
    let mut versions = Vec::new();
    for entry in std::fs::read_dir(&path).map_err(|e| format!("读取目录失败: {}", e))? {
        let entry = entry.map_err(|e| format!("读取条目失败: {}", e))?;
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_version = name.starts_with("IFMS6.0V") || name.starts_with("IFMS6");
        if !is_version { continue; }
        // Accept both directories and .zip files
        if p.is_dir() || (p.is_file() && name.ends_with(".zip")) {
            versions.push(name);
        }
    }
    sort_chinese(&mut versions);
    Ok(versions)
}

// ── Mode Detection ──

/// Peek inside a version's zip to determine SEE (server/ has zips) or Non-SEE (server/ has dirs).
/// Also works on already-extracted directories.
pub fn detect_mode_from_entry(src_dir: &str, version_name: &str) -> Result<bool, String> {
    let src = PathBuf::from(src_dir);

    // Try as directory first (extracted package)
    let dir_path = src.join(version_name);
    if dir_path.is_dir() {
        let server_dir = dir_path.join("server");
        if !server_dir.exists() {
            return Err("版本目录中未找到 server/ 子目录".to_string());
        }
        let mut has_zip = false;
        let mut has_dir = false;
        if let Ok(entries) = std::fs::read_dir(&server_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let n = entry.file_name().to_string_lossy().to_string();
                if p.is_file() && n.ends_with(".zip") {
                    has_zip = true;
                } else if p.is_dir() {
                    has_dir = true;
                }
            }
        }
        return Ok(has_zip && !has_dir);
    }

    // Try as .zip file
    let zip_path = src.join(format!("{}.zip", version_name));
    if !zip_path.is_file() {
        return Err(format!("在目录中未找到版本条目: {} (目录或 .zip 文件)", version_name));
    }
    let file = std::fs::File::open(&zip_path).map_err(|e| format!("打开ZIP失败: {}", e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("读取ZIP失败: {}", e))?;

    let prefix = "server/";
    let prefix_len = prefix.len();
    let mut has_server_zip = false;
    let mut has_server_dir = false;

    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|_| "读取ZIP条目失败".to_string())?;
        let name = decode_entry_name(entry.name_raw());
        if !name.starts_with(prefix) { continue; }
        let rest = &name[prefix_len..];
        if rest.is_empty() || rest == "/" { continue; }
        // If this is a file entry (not ending with /) and ends with .zip → SEE
        // If this is a dir entry (ending with /) → Non-SEE
        if rest.ends_with('/') {
            has_server_dir = true;
        } else if rest.ends_with(".zip") {
            has_server_zip = true;
        }
    }

    if has_server_zip && !has_server_dir {
        Ok(true)
    } else {
        Ok(false)
    }
}

// ── Public API ──

/// 同步入口（源仓 `start_merge`：校验 → `thread::spawn` → 三条事件）。
///
/// 线程由插件进程自己开（`crates/duck-merge-tool`）。源仓那条 `success:false` 的
/// `emit_done` 由 `Err` 承担，插件据此出错误正文。
pub fn merge(app: &dyn MergeReporter, options: Ifms6MergeOptions) -> Result<Ifms6MergeResult, String> {
    if options.src_dir.is_empty() || options.dst_dir.is_empty() {
        return Err("源目录和目标目录不能为空".to_string());
    }
    let src_dir = PathBuf::from(&options.src_dir);
    if !src_dir.exists() { return Err(format!("源目录不存在: {}", options.src_dir)); }
    // Safety: dst_dir must differ from src_dir to prevent any write on source
    if src_dir.canonicalize().ok() == PathBuf::from(&options.dst_dir).canonicalize().ok() {
        return Err("源目录和目标目录不能相同，请选择不同的目录".to_string());
    }
    let dst_dir = PathBuf::from(&options.dst_dir);
    std::fs::create_dir_all(&dst_dir).map_err(|e| format!("创建目标目录失败: {}", e))?;

    reset_cancel();
    let _ = v6_regression_list().lock().map(|mut list| list.clear());
    let start = Instant::now();
    match run_merge(app, &src_dir, &dst_dir, &options, cancel_flag()) {
        Ok(stats) => {
            let elapsed = start.elapsed().as_millis() as u64;
            emit_log(app, "success", &format!("IFMS6 合包完成，耗时 {:.1}s", elapsed as f64 / 1000.0), Some(elapsed));
            emit_progress(app, "done", 100, Some("合并完成"));
            Ok(Ifms6MergeResult { success: true, ..stats })
        }
        Err(e) => {
            emit_log(app, "error", &e, None);
            Err(e)
        }
    }
}
