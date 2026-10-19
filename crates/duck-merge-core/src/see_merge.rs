//! SEE 包合并（`SeeMerge.vue`）：扫描一串 SEE 分包 ZIP，解压 → 内层包去重 →
//! 个性化 SQL（按客户号）→ Slim JAR 瘦身 → 出 MD5 → 重新压缩。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/see_merge.rs`（1,082 行）。
//! 接线与 `sp_merge` 同一套：源仓 progress 载荷本来就是 `stage / percent /
//! current / total / current_file / stage_duration_ms` 六格，所以直接复用
//! [`crate::sp_merge::PkgMergeReporter`]；`start_merge`（spawn + `emit_done`）→
//! [`merge`]（同步返回结果）。
//!
//! 源仓的 `emit_done` 在这里删掉：结果直接当返回值给插件。trait 上的 `done` 方法
//! 收的是 `sp_merge::MergeResult`，形状不同，本模块不调它。

// 复刻件的 clippy 偏差逐条列名（理由同 `ifms6_merge`）：这些是参考仓的既有写法。
// `dead_code` 覆盖源仓自己也没接上的 `slim_package_convert`——**不要顺手接上**，
// 那是改行为，不是移植。
#![allow(
    clippy::collapsible_if,
    clippy::too_many_arguments,
    clippy::manual_range_contains,
    clippy::needless_borrows_for_generic_args,
    clippy::useless_borrows_in_formatting,
    dead_code,
    unused_variables
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use encoding_rs::GBK;
use regex::Regex;
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::sp_merge::PkgMergeReporter;

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

// ── Data Structures ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeeMergeOptions {
    pub src_dir: String,
    pub dst_dir: String,
    pub customer_no: Option<String>,
    pub db_type: Option<String>,
    pub current_version: Option<String>,
    pub target_version: Option<String>,
    pub handle_custom_script: Option<bool>,
    pub slim_mode: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeeMergeResult {
    pub success: bool,
    pub output_path: String,
    pub statistics: SeeMergeStatistics,
    pub extended: Option<SeeMergeExtended>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeeMergeExtended {
    pub personalized_sql_services: usize,
    pub customer_jars_deleted: usize,
    pub slim_jars_removed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeeMergeStatistics {
    pub packages: usize,
    pub files_extracted: usize,
    pub packages_deduplicated: usize,
    pub total_duration_ms: u64,
}

// ── Constants & Regex ──

fn package_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^FS5(?:\.0)?-FINFSS-FSS-V(\d{6})-(\d{2})-(\d{3,})").expect("valid regex")
    })
}

/// Java: getStrPackge = replaceAll(STRREGEX, "").replaceAll("zip", "")
fn get_str_packge(filename: &str) -> String {
    let no_digits: String = filename.chars().filter(|c| !c.is_ascii_digit()).collect();
    no_digits.replace("zip", "")
}

/// Java: replaceAll(NUMREGEX, "")
fn get_str_num(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Java: fiterFiles key = getStrPackge - PACK - PATCH - trailing M
fn extract_letter_key(filename: &str) -> String {
    let key = get_str_packge(filename)
        .replace("PACK", "")
        .replace("PATCH", "");
    key.strip_suffix('M').unwrap_or(&key).to_string()
}

/// Java: getStrPackge(file).endsWith("M")
fn is_m_series(filename: &str) -> bool {
    get_str_packge(filename).ends_with('M')
}

/// Java: M suffix length after last 'M'
fn m_suffix_length(filename: &str) -> usize {
    let name = filename.strip_suffix(".zip").unwrap_or(filename);
    if let Some(pos) = name.rfind('M') { name[pos + 1..].len() } else { 0 }
}

fn is_service_zip(filename: &str) -> bool {
    let svcs = ["UOT-SERVICE", "UPS-SERVICE", "TCS-SERVICE", "CONTROL-SERVICE", "LIQUIDATE-SERVICE"];
    svcs.iter().any(|s| filename.contains(s))
}

// ── Package Parsing ──

#[derive(Debug, Clone)]
struct SeePackage { path: PathBuf, filename: String }

fn parse_package(path: &Path) -> Option<SeePackage> {
    let filename = path.file_name()?.to_str()?;
    if !package_regex().is_match(filename) { return None; }
    Some(SeePackage { path: path.to_path_buf(), filename: filename.to_string() })
}

fn scan_zips(src_dir: &Path) -> Vec<SeePackage> {
    let mut pkgs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(src_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("zip") {
                if let Some(pkg) = parse_package(&path) { pkgs.push(pkg); }
            }
        }
    }
    pkgs
}

// ── Pow computation (Java initPow) ──

fn compute_pow(zips: &[SeePackage]) -> u64 {
    let mut nums: Vec<u64> = zips.iter()
        .filter_map(|p| get_str_num(&p.filename).parse::<u64>().ok())
        .collect();
    if nums.len() < 2 { return 10; }
    nums.sort();
    let exp = nums[nums.len() - 1].to_string().len().saturating_sub(nums[0].to_string().len());
    10u64.pow(exp as u32)
}

// ── M-series version comparison (Java compareBigNum) ──

fn compare_big_num(filename_a: &str, filename_b: &str, pow_val: u64) -> std::cmp::Ordering {
    let num_a: u128 = get_str_num(filename_a).parse().unwrap_or(0);
    let num_b: u128 = get_str_num(filename_b).parse().unwrap_or(0);
    let is_m_a = is_m_series(filename_a);
    let is_m_b = is_m_series(filename_b);
    if !is_m_a && !is_m_b { return num_a.cmp(&num_b); }
    if is_m_a && !is_m_b {
        return num_a.cmp(&(num_b * (pow_val as u128).pow(m_suffix_length(filename_a) as u32)));
    }
    if !is_m_a && is_m_b {
        return (num_a * (pow_val as u128).pow(m_suffix_length(filename_b) as u32)).cmp(&num_b);
    }
    num_a.cmp(&num_b)
}

// ── fiterFiles (Java-compatible) ──

fn fiter_files(merge_dir: &Path, cancel: &AtomicBool, pow_val: u64) -> Result<usize, String> {
    let mut dedup_count = 0;
    let mut map: HashMap<String, PathBuf> = HashMap::new();
    let entries: Vec<_> = std::fs::read_dir(merge_dir)
        .map_err(|e| format!("读取合并目录失败: {}", e))?
        .filter_map(|e| e.ok()).collect();
    for entry in &entries {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let path = entry.path();
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(), None => continue,
        };
        if !fname.contains(".zip") { continue; }
        let key = extract_letter_key(&fname);
        if let Some(existing_path) = map.remove(&key) {
            let existing_name = existing_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            dedup_count += 1;
            match compare_big_num(&fname, existing_name, pow_val) {
                std::cmp::Ordering::Greater | std::cmp::Ordering::Equal => {
                    let _ = std::fs::remove_file(&existing_path);
                    map.insert(key, path);
                }
                std::cmp::Ordering::Less => {
                    let _ = std::fs::remove_file(&path);
                    map.insert(key, existing_path);
                }
            }
        } else {
            map.insert(key, path);
        }
    }
    Ok(dedup_count)
}

// ── Emit Helpers ──

// 源仓三条 `app.emit("see-merge-*", …)`：载荷逐字段搬到 [`PkgMergeReporter`] 上
// （六格全对得上，见模块头）。`done` 不调——结果走返回值。
fn emit_progress(app: &dyn PkgMergeReporter, stage: &str, percent: u32, current: usize, total: usize,
    current_file: Option<String>, stage_duration: Option<u64>) {
    app.progress(stage, percent.min(100), current, total, current_file, stage_duration);
}

fn emit_log(app: &dyn PkgMergeReporter, level: &str, message: &str, duration_ms: Option<u64>) {
    app.log(level, message, duration_ms);
}

// ── ZIP Extraction ──

fn decode_entry_name(raw: &[u8]) -> String {
    if let Ok(name) = std::str::from_utf8(raw) {
        name.replace('\\', "/")
    } else {
        let (decoded, _, _) = GBK.decode(raw);
        decoded.replace('\\', "/")
    }
}

fn extract_zip_to_dir(zip_path: &Path, dest_dir: &Path, cancel: &AtomicBool) -> Result<usize, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("无法打开文件: {}", e))?;
    let mut archive = ZipArchive::new(file).map_err(|e| format!("无法读取ZIP: {}", e))?;
    let mut extracted = 0usize;
    for i in 0..archive.len() {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let mut entry = match archive.by_index(i) { Ok(e) => e, Err(_) => continue, };
        let entry_name = decode_entry_name(entry.name_raw());
        if entry.is_dir() { let _ = std::fs::create_dir_all(dest_dir.join(&entry_name)); continue; }
        if entry_name.contains("..") { continue; }
        let full_path = dest_dir.join(&entry_name);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
        }
        let mut outfile = std::fs::File::create(&full_path).map_err(|e| format!("创建文件失败: {}", e))?;
        std::io::copy(&mut entry, &mut outfile).map_err(|e| format!("写入文件失败: {}", e))?;
        extracted += 1;
    }
    Ok(extracted)
}

// ── Directory Compression (跨平台一致，委托 merge_common) ──

// ── MD5 hash.txt ──

fn create_md5_file(merge_dir: &Path) -> Result<(), String> {
    let doc_dir = {
        let doc = merge_dir.join("doc");
        let docs = merge_dir.join("docs");
        if doc.exists() { doc } else {
            std::fs::create_dir_all(&docs).map_err(|e| format!("创建 docs 目录失败: {}", e))?;
            docs
        }
    };
    let hash_path = doc_dir.join("hash.txt");
    let mut md5_lines = String::new();
    for entry in std::fs::read_dir(merge_dir).map_err(|e| format!("读取目录失败: {}", e))?.flatten() {
        let path = entry.path();
        if path.is_file() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            md5_lines.push_str(&format!("{}\tmd5:{}\n", name, compute_md5(&path)?));
        }
    }
    std::fs::write(&hash_path, md5_lines.as_bytes()).map_err(|e| format!("写入 hash.txt 失败: {}", e))?;
    Ok(())
}

fn compute_md5(path: &Path) -> Result<String, String> {
    // 参考仓这里写的就是 `use md5::{Digest, Md5}`（`md5` crate）。本仓换成同一套
    // RustCrypto 实现的 `md-5 0.10`（零新增下载，见工作区注释）：**包名是 `md-5`、
    // lib 名仍是 `md5`**，所以这一行与参考仓一字不差。
    use md5::{Digest, Md5};
    let data = std::fs::read(path).map_err(|e| format!("读取文件失败 {}", e))?;
    Ok(format!("{:x}", Md5::digest(&data)))
}

fn remove_dir_all(path: &Path) -> Result<(), String> {
    if path.exists() {
        std::fs::remove_dir_all(path).map_err(|e| format!("清理目录失败 {}", e))?;
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
//  Personalized SQL Merge (Java MergePersonalizedSqlFiles)
// ═══════════════════════════════════════════════════════════════

/// Java MergePersonalizedSqlFiles.mergePersonalizedSqlFiles:
/// For each service zip (UOT/UPS/TCS/CONTROL/LIQUIDATE):
///   - extract outer/inner zip
///   - find database/{dbType}/个性化脚本/{customerNo}/
///   - delete other customers' dirs, keep only matching customerNo
///   - copy .doc/.docx files to mergeDir/doc/开通功能手册
fn merge_personalized_sql(merge_dir: &Path, customer_no: &str,
    db_type: Option<&str>, handle_custom_script: bool, cancel: &AtomicBool,
) -> Result<usize, String> {
    let mut processed = 0usize;
    let entries: Vec<_> = std::fs::read_dir(merge_dir)
        .map_err(|e| format!("读取目录失败 {}", e))?
        .filter_map(|e| e.ok()).collect();

    for entry in &entries {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let path = entry.path();
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if !fname.ends_with(".zip") || !is_service_zip(&fname) { continue; }

        // Extract service zip to temp dir
        let tmp_dir = merge_dir.join(format!(".sql_tmp_{}", processed));
        std::fs::create_dir_all(&tmp_dir)
            .map_err(|e| format!("创建临时目录失败: {}", e))?;
        let outer_count = extract_zip_to_dir(&path, &tmp_dir, cancel)?;
        if outer_count == 0 { let _ = remove_dir_all(&tmp_dir); continue; }

        // Determine inner path (Java: flag=true when inner zip has same name as outer)
        let inner_base = if tmp_dir.join(&fname).exists() {
            // Double-zip: inner has same name as outer
            let inner_tmp = tmp_dir.join("_inner");
            std::fs::create_dir_all(&inner_tmp)
                .map_err(|e| format!("创建内层临时目录失败: {}", e))?;
            extract_zip_to_dir(&tmp_dir.join(&fname), &inner_tmp, cancel)?;
            let _ = std::fs::remove_file(&tmp_dir.join(&fname));
            inner_tmp
        } else {
            tmp_dir.clone()
        };

        // Navigate to database/ directory
        let db_path = inner_base.join("database");
        if !db_path.exists() { let _ = remove_dir_all(&tmp_dir); continue; }

        let db_types: Vec<&str> = db_type.map(|s| s.split(',').collect()).unwrap_or_default();
        let db_type_iter: Vec<String> = std::fs::read_dir(&db_path)
            .ok().into_iter().flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        for type_name in &db_type_iter {
            if !db_types.is_empty() && !db_types.contains(&type_name.as_str()) { continue; }
            let type_dir = db_path.join(type_name);
            let first_sub = std::fs::read_dir(&type_dir).ok()
                .and_then(|mut it| it.find_map(|e| e.ok()));
            let sub_dir = match first_sub {
                Some(e) => type_dir.join(e.file_name()),
                None => continue,
            };
            let cust_dir = sub_dir.join("个性化脚本");
            if !cust_dir.exists() { continue; }

            // Iterate customer directories under 个性化脚本
            for cust_entry in std::fs::read_dir(&cust_dir)
                .map_err(|e| format!("读取个性化脚本目录失败 {}", e))?.flatten()
            {
                if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
                let cpath = cust_entry.path();
                let cname = cust_entry.file_name().to_string_lossy().to_string();
                if !cpath.is_dir() { continue; }

                if !cname.contains(customer_no) {
                    // Delete other customers' directories
                    let _ = std::fs::remove_dir_all(&cpath);
                } else {
                    // Keep this customer's directory
                    // Java: ReplaceCustomerSqlUtils.findSqlFiles() → rewrite procedure calls to SQL
                    if handle_custom_script {
                        let _ = duck_sql_core::sql_rewriter::rewrite_sql_dir(&cpath);
                    }
                    // Copy doc files
                    let doc_dir = merge_dir.join("doc").join("开通功能手册");
                    let _ = std::fs::create_dir_all(&doc_dir);
                    copy_doc_files(&cpath, &doc_dir);
                }
            }
            processed += 1;
        }

        // Re-zip the service
        let _ = crate::merge_common::zip_dir_sorted(&tmp_dir, &path);
        let _ = remove_dir_all(&tmp_dir);
    }

    Ok(processed)
}

/// Copy .doc/.docx files from src to dest (Java ZipUtils.moveDocFiles equivalent)
fn copy_doc_files(src: &Path, dest: &Path) {
    crate::merge_common::copy_doc_files(src, dest);
}

// ═══════════════════════════════════════════════════════════════
//  Slim JAR Processing (Java MergeZipsUtils lines 112-228)
// ═══════════════════════════════════════════════════════════════

/// Process slim packages:
///   1. Extract each .zip, read jarlist.txt, collect jar names -> jarContextList
///   2. For V202402+: handle customer.json in UOT-SERVICE
///   3. Delete jars NOT in jarContextList from Stack/artifactory/
///   4. Re-zip Stack/
fn process_slim_jars(merge_dir: &Path, target_version: Option<&str>, customer_no: Option<&str>,
    cancel: &AtomicBool,
) -> Result<(usize, usize), String> {
    let version_str = target_version.unwrap_or("");
    let is_v202402_plus = version_str.contains("V202402") || version_str > "FS5.0-FINFSS.V202401.02.000";
    let scripts_version = get_scripts_version(version_str);

    let mut jar_context_list: Vec<String> = Vec::new();
    let mut service_zips: Vec<PathBuf> = Vec::new();

    // Phase A: extract each .zip, read jarlist, collect jar context
    let entries: Vec<_> = std::fs::read_dir(merge_dir)
        .map_err(|e| format!("读取合并目录失败 {}", e))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file() && e.path().extension().map(|x| x == "zip").unwrap_or(false))
        .collect();

    for entry in &entries {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();

        // Skip Stack.zip, already handled
        if fname.contains("Stack") { continue; }

        service_zips.push(path.clone());

        // Extract outer zip
        let outer_dir = merge_dir.join(format!(".slim_{}", fname));
        std::fs::create_dir_all(&outer_dir)
            .map_err(|e| format!("创建slim临时目录失败 {}", e))?;
        let _ = extract_zip_to_dir(&path, &outer_dir, cancel);

        // Find inner .zip files (Java: for each inner zip, extract and read jarlist)
        for inner_entry in std::fs::read_dir(&outer_dir)
            .ok().into_iter().flatten().filter_map(|e| e.ok())
        {
            let inner_path = inner_entry.path();
            if !inner_path.is_file() || inner_path.extension().map(|x| x == "zip").unwrap_or(false) {
                let inner_name = inner_path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                if !inner_name.ends_with(".zip") || inner_name == fname { continue; }
                let inner_dir = merge_dir.join(format!(".slim_inner_{}", &inner_name));
                std::fs::create_dir_all(&inner_dir)
                    .map_err(|e| format!("创建内层slim目录失败 {}", e))?;
                let _ = extract_zip_to_dir(&inner_path, &inner_dir, cancel);

                // Read jarlist from acmsdk/template/
                let template_dir = inner_dir.join("acmsdk").join("template");
                if template_dir.exists() {
                    for tf in std::fs::read_dir(&template_dir).ok().into_iter().flatten().filter_map(|e| e.ok()) {
                        let tf_name = tf.file_name().to_string_lossy().to_string();
                        if tf_name.contains("jarlist") {
                            if let Ok(content) = std::fs::read_to_string(tf.path()) {
                                if let Ok(list) = serde_json::from_str::<Vec<String>>(&content) {
                                    jar_context_list.extend(list);
                                }
                            }
                        }
                    }
                }
                let _ = remove_dir_all(&inner_dir);
            }
        }
        let _ = remove_dir_all(&outer_dir);
    }

    let total_jars_in_context = jar_context_list.len();
    let mut customer_jars_deleted = 0usize;

    // Phase B: handle customer.json for V202402+
    if is_v202402_plus {
        if let Some(cust_no) = customer_no {
            if !cust_no.is_empty() {
                // Find UOT-SERVICE zip in merge dir
                for entry in &entries {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if fname.starts_with("FS5.0-FINFSS-UOT-SERVICE-") {
                        let result = delete_customer_jars(&entry.path(), cust_no, merge_dir, cancel)?;
                        customer_jars_deleted = result;
                        break;
                    }
                }
            }
        }
    }

    // Phase C: delete jars NOT in jarContextList from Stack/artifactory/
    let stack_artifactory = merge_dir.join("Stack").join("artifactory");
    let mut jars_removed = 0usize;
    if stack_artifactory.exists() {
        // If there's an all-lib-* directory inside, use that; otherwise use artifactory directly
        let lib_dir = if let Some(all_lib) = find_all_lib_dir(&stack_artifactory) {
            all_lib
        } else {
            stack_artifactory.clone()
        };

        if lib_dir.exists() {
            for jar_entry in std::fs::read_dir(&lib_dir)
                .map_err(|e| format!("读取artifactory目录失败 {}", e))?.flatten()
            {
                let jar_path = jar_entry.path();
                if !jar_path.is_file() { continue; }
                let jar_name = jar_entry.file_name().to_string_lossy().to_string();
                // Check if this jar is in the context list (by name or prefix)
                let in_context = jar_context_list.iter().any(|j| {
                    j.starts_with(&jar_name) || jar_name.starts_with(j)
                        || j.trim_end_matches(".jar") == jar_name.trim_end_matches(".jar")
                });
                if !in_context && !jar_name.contains("hash") {
                    let _ = std::fs::remove_file(&jar_path);
                    jars_removed += 1;
                }
            }
        }
    }

    // Re-zip Stack/
    let stack_dir = merge_dir.join("Stack");
    let stack_zip = merge_dir.join("Stack.zip");
    if stack_dir.exists() {
        let _ = std::fs::remove_file(&stack_zip);
        crate::merge_common::zip_dir_sorted(&stack_dir, &stack_zip)?;
        let _ = remove_dir_all(&stack_dir);
    }

    Ok((customer_jars_deleted, jars_removed))
}

fn find_all_lib_dir(artifactory_dir: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(artifactory_dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() && entry.file_name().to_string_lossy().contains("all-lib") {
            return Some(path);
        }
    }
    None
}

/// Java deleteCustomerJars:
///   Extract UOT-SERVICE zip → find customer.json → parse customer_jars_prefix
///   → delete jars for non-matching customers from Stack/artifactory/
///   → update jarlist.txt and deploy.xml
fn delete_customer_jars(uot_zip: &Path, customer_no: &str,
    merge_dir: &Path, cancel: &AtomicBool,
) -> Result<usize, String> {
    let tmp = merge_dir.join(".uot_tmp");
    std::fs::create_dir_all(&tmp)
        .map_err(|e| format!("创建UOT临时目录失败 {}", e))?;
    let _ = extract_zip_to_dir(uot_zip, &tmp, cancel);

    // Find inner zip (FS5.0-FINFSS-UOT-SERVICE-V*)
    let inner_zip = std::fs::read_dir(&tmp).ok().into_iter().flatten()
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_file() && e.file_name().to_string_lossy().contains("UOT-SERVICE-"));
    let inner_zip = match inner_zip {
        Some(e) => e.path(),
        None => { let _ = remove_dir_all(&tmp); return Ok(0); }
    };

    let inner_tmp = merge_dir.join(".uot_inner");
    std::fs::create_dir_all(&inner_tmp)
        .map_err(|e| format!("创建UOT内层目录失败 {}", e))?;
    let _ = extract_zip_to_dir(&inner_zip, &inner_tmp, cancel);

    // Find customer.json
    let customer_json = find_file_recursive(&inner_tmp, "customer.json");
    let result = match customer_json {
        Some(json_path) => {
            process_customer_json(&json_path, customer_no, &inner_tmp, merge_dir, cancel)
        }
        None => 0,
    };

    // Cleanup
    let _ = remove_dir_all(&tmp);
    let _ = remove_dir_all(&inner_tmp);
    Ok(result)
}

fn find_file_recursive(dir: &Path, target: &str) -> Option<PathBuf> {
    crate::merge_common::find_file_recursive(dir, target)
}

/// Parse customer.json and delete non-matching customer jars from Stack
fn process_customer_json(json_path: &Path, customer_no: &str,
    service_dir: &Path, merge_dir: &Path, cancel: &AtomicBool,
) -> usize {
    let content = match crate::merge_common::read_text_file(json_path) { Ok(c) => c, Err(_) => return 0 };
    let customers: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(v) => v, Err(_) => return 0,
    };

    // Collect jar prefixes to delete (for non-matching customers)
    let mut delete_prefixes: Vec<String> = Vec::new();
    let mut our_prefixes: Vec<String> = Vec::new();
    for cust in &customers {
        let cust_id = cust["customer_id"].as_str().unwrap_or("");
        if let Some(prefixes) = cust["customer_jars_prefix"].as_array() {
            let target = if cust_id == customer_no { &mut our_prefixes } else { &mut delete_prefixes };
            for p in prefixes {
                if let Some(s) = p.as_str() { target.push(s.trim().to_string()); }
            }
        }
    }
    // Remove our customer's prefixes from delete list (keep our customer's jars)
    for p in &our_prefixes { delete_prefixes.retain(|d| d != p); }
    if delete_prefixes.is_empty() { return 0; }

    // Find jarlist.txt
    let mut jar_list: Vec<String> = Vec::new();
    let jarlist_path = find_file_recursive(service_dir, "jarlist.txt")
        .or_else(|| find_jarlist_files(service_dir).first().cloned());

    if let Some(ref jlp) = jarlist_path {
        if let Ok(content) = std::fs::read_to_string(jlp) {
            jar_list = serde_json::from_str(&content).unwrap_or_default();
        }
    }

    // Find jars to delete from jarlist
    let mut to_remove: Vec<String> = Vec::new();
    for jar in &jar_list {
        for prefix in &delete_prefixes {
            if jar.starts_with(prefix) { to_remove.push(jar.clone()); break; }
        }
    }

    if to_remove.is_empty() { return 0; }

    // Update jarlist.txt
    if let Some(ref jlp) = jarlist_path {
        let new_list: Vec<String> = jar_list.into_iter()
            .filter(|j| !to_remove.contains(j)).collect();
        if let Ok(json) = serde_json::to_string(&new_list) {
            let _ = std::fs::write(jlp, &json);
        }
    }

    // Update deploy.xml: remove jar URL entries
    if let Some(deploy_path) = find_file_recursive(service_dir, "deploy.xml") {
        if let Ok(content) = std::fs::read_to_string(&deploy_path) {
            let mut new_content = content.clone();
            for jar_name in &to_remove {
                let url_tag = format!("./artifactory/{}", jar_name);
                new_content = new_content.replace(&format!("<URL type=\"local\">{}</URL>", url_tag), "");
            }
            let _ = std::fs::write(&deploy_path, &new_content);
        }
    }

    // Delete jar files from Stack/artifactory/
    let stack_artifactory = merge_dir.join("Stack").join("artifactory");
    let lib_dir = find_all_lib_dir(&stack_artifactory).unwrap_or(stack_artifactory);
    if lib_dir.exists() {
        for jar_name in &to_remove {
            let jar_path = lib_dir.join(jar_name);
            if jar_path.exists() { let _ = std::fs::remove_file(&jar_path); }
        }
    }

    to_remove.len()
}

fn find_jarlist_files(dir: &Path) -> Vec<PathBuf> {
    crate::merge_common::find_jarlist_files(dir)
}

/// Java getScriptsVersion: FS5.0-FINFSS.V202201.06.022 → "8.6.22.0"
fn get_scripts_version(version: &str) -> String {
    if let Some(v_pos) = version.find('V') {
        let ver_part = &version[v_pos + 1..];
        let parts: Vec<&str> = ver_part.split('.').collect();
        if parts.len() >= 3 {
            let base_ver = parts[0].trim_end_matches(|c: char| !c.is_ascii_digit());
            let jx = (base_ver.parse::<i32>().unwrap_or(2022) - 2022 + 8).to_string();
            let zl = parts[1].parse::<i32>().unwrap_or(0).to_string();
            let bd_part = parts[2];
            if let Some(m_pos) = bd_part.find('M') {
                let bd = bd_part[..m_pos].parse::<i32>().unwrap_or(0).to_string();
                let bdbd = bd_part[m_pos + 1..].parse::<i32>().unwrap_or(0).to_string();
                return format!("{}.{}.{}.{}", jx, zl, bd, bdbd);
            } else {
                let bd = bd_part.parse::<i32>().unwrap_or(0).to_string();
                return format!("{}.{}.{}.0", jx, zl, bd);
            }
        }
    }
    "8.0.0.0".to_string()
}

// ═══════════════════════════════════════════════════════════════
//  SlimPackageUtils.convert (Java SlimPackageUtils.changeSlimPackage)
// ═══════════════════════════════════════════════════════════════

/// Convert a merged package to SLIM format:
///   - Read stack-deploy.json for version
///   - Compute hashCode from zip filenames
///   - Extract each service, move JARs from WEB-INF/lib/ to public all-lib
///   - Write jarlist.txt, update deploy.xml
///   - Re-zip services (now without JARs) and the final package
fn slim_package_convert(merge_zip: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let parent = merge_zip.parent().unwrap_or(Path::new("."));
    let fname = merge_zip.file_stem().and_then(|n| n.to_str()).unwrap_or("merge");
    let slim_dir_name = if fname.contains("FS50-FINFSS-FSS") {
        fname.replace("FS50-FINFSS-FSS", "FS5.0-FINFSS-FSS-SLIM")
    } else {
        format!("{}-SLIM", fname)
    };
    let slim_dir = parent.join(&slim_dir_name);

    if slim_dir.exists() { let _ = remove_dir_all(&slim_dir); }
    std::fs::create_dir_all(&slim_dir)
        .map_err(|e| format!("创建SLIM目录失败 {}", e))?;
    let _ = extract_zip_to_dir(merge_zip, &slim_dir, cancel);

    // Read stack-deploy.json for version
    let stack_json = slim_dir.join("stack-deploy.json");
    let mut stack_version = String::from("1.0.0.0");
    if stack_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&stack_json) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                stack_version = v["version"].as_str().unwrap_or("1.0.0.0").to_string();
            }
        }
    }

    // Compute hashCode from zip filenames (Java: concat non-digit parts, get hash)
    let hash_string: String = std::fs::read_dir(&slim_dir)
        .ok().into_iter().flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file() && e.path().extension().map(|x| x == "zip").unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(|s| {
            s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>()
        }))
        .collect();
    let hash_code = if hash_string.is_empty() {
        "0".to_string()
    } else {
        let hash = hash_string.as_bytes();
        let mut h: i32 = 0;
        for &b in hash { h = h.wrapping_mul(31).wrapping_add(b as i32); }
        h.abs().to_string()
    };

    let all_lib_dir = slim_dir.join("Stack").join("artifactory")
        .join(format!("all-lib-{}-{}", stack_version, hash_code));
    std::fs::create_dir_all(&all_lib_dir)
        .map_err(|e| format!("创建all-lib目录失败 {}", e))?;

    // Process each service zip
    for entry in std::fs::read_dir(&slim_dir)
        .map_err(|e| format!("读取SLIM目录失败 {}", e))?.flatten()
    {
        if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
        let path = entry.path();
        if !path.is_file() || path.extension().map(|x| x == "zip").unwrap_or(false) { continue; }
        let service_name = entry.file_name().to_string_lossy().to_string();
        if service_name.contains("REPORT") || service_name.contains("APP") { continue; }

        // Extract service outer zip
        let outer_dir = slim_dir.join(format!(".sl_{}", &service_name));
        std::fs::create_dir_all(&outer_dir)
            .map_err(|e| format!("创建服务临时目录失败 {}", e))?;
        let _ = extract_zip_to_dir(&path, &outer_dir, cancel);

        // Find inner zip (service inner)
        let inner_zip = std::fs::read_dir(&outer_dir).ok().into_iter().flatten()
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_file() && e.file_name().to_string_lossy() == service_name);
        let inner_zip_path = match inner_zip { Some(e) => e.path(), None => continue };

        let inner_dir = outer_dir.join("_inner");
        std::fs::create_dir_all(&inner_dir)
            .map_err(|e| format!("创建服务内层目录失败 {}", e))?;
        let _ = extract_zip_to_dir(&inner_zip_path, &inner_dir, cancel);

        // Find WAR file in application/
        let app_dir = inner_dir.join("application");
        if !app_dir.exists() { continue; }

        let war_file = std::fs::read_dir(&app_dir).ok().into_iter().flatten()
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_file() && e.path().extension().map(|x| x == "war").unwrap_or(false));

        if let Some(war) = war_file {
            let war_path = war.path();
            let war_dir = outer_dir.join("_war");
            std::fs::create_dir_all(&war_dir)
                .map_err(|e| format!("创建WAR临时目录失败 {}", e))?;
            let _ = extract_zip_to_dir(&war_path, &war_dir, cancel);
            let _ = std::fs::remove_file(&war_path);

            // Read jars from WEB-INF/lib/
            let lib_dir_path = war_dir.join("WEB-INF").join("lib");
            if lib_dir_path.exists() {
                // Write jarlist.txt
                let jarlist_path = inner_dir.join("acmsdk").join("template").join("jarlist.txt");
                std::fs::create_dir_all(jarlist_path.parent().ok_or_else(|| format!("路径没有父目录: {}", jarlist_path.display()))?)
                    .map_err(|e| format!("创建template目录失败 {}", e))?;

                let jar_names: Vec<String> = std::fs::read_dir(&lib_dir_path)
                    .ok().into_iter().flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_file())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect();

                if let Ok(json) = serde_json::to_string(&jar_names) {
                    let _ = std::fs::write(&jarlist_path, &json);
                }

                // Move JARs to public all-lib
                for jar_entry in std::fs::read_dir(&lib_dir_path)
                    .ok().into_iter().flatten().filter_map(|e| e.ok())
                {
                    let jar_path = jar_entry.path();
                    if jar_path.is_file() {
                        let target = all_lib_dir.join(jar_entry.file_name());
                        let _ = std::fs::rename(&jar_path, &target);
                    }
                }
            }

            // Re-zip as war
            let _ = crate::merge_common::zip_dir_sorted(&war_dir, &war_path);
            let _ = remove_dir_all(&war_dir);
        }

        // Update deploy.xml with all-lib path (Java TemplateUtils.replaceDeploy)
        let deploy_path = inner_dir.join("acmsdk").join("deploy.xml");
        if deploy_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&deploy_path) {
                let repo_tag = format!("<repository><URL type=\"local\">./artifactory/all-lib-{}-{}.zip</URL></repository>",
                    stack_version, hash_code);
                let new_content = content.replace("<repository/>", &repo_tag);
                let _ = std::fs::write(&deploy_path, &new_content);
            }
        }

        // Re-zip inner, then outer
        let _ = crate::merge_common::zip_dir_sorted(&inner_dir, &inner_zip_path);
        let _ = remove_dir_all(&inner_dir);
        let _ = crate::merge_common::zip_dir_sorted(&outer_dir, &path);
        let _ = remove_dir_all(&outer_dir);
    }

    // Re-zip all-lib
    let _ = crate::merge_common::zip_dir_sorted(&all_lib_dir, &all_lib_dir.with_extension("zip"));
    let _ = remove_dir_all(&all_lib_dir);

    // Re-zip Stack
    let stack_dir = slim_dir.join("Stack");
    if stack_dir.exists() {
        let _ = crate::merge_common::zip_dir_sorted(&stack_dir, &stack_dir.with_extension("zip"));
        let _ = remove_dir_all(&stack_dir);
    }

    // Final slim zip
    let slim_zip = slim_dir.with_extension("zip");
    let _ = crate::merge_common::zip_dir_sorted(&slim_dir, &slim_zip);
    let _ = remove_dir_all(&slim_dir);

    Ok(slim_zip.to_string_lossy().to_string())
}

// ═══════════════════════════════════════════════════════════════
//  Main Merge Logic (Java MergeZipsUtils.mergeZipsMethod flow)
// ═══════════════════════════════════════════════════════════════

struct MergeStats {
    output_path: String,
    packages: usize,
    files_extracted: usize,
    packages_deduplicated: usize,
    sql_services_processed: usize,
    customer_jars_deleted: usize,
    slim_jars_removed: usize,
}

fn run_merge(app: &dyn PkgMergeReporter, src_dir: &Path, dst_dir: &Path,
    options: &SeeMergeOptions, cancel: &AtomicBool,
) -> Result<MergeStats, String> {
    let start_total = Instant::now();

    // ── Phase 1: Scan ──
    emit_progress(app, "scanning", 3, 0, 0, None, None);
    emit_log(app, "info", &format!("开始扫描目录: {}", src_dir.display()), None);
    let pkgs = scan_zips(src_dir);
    if pkgs.is_empty() {
        return Err("未找到有效的 SEE 包（需要匹配 FS5.0-FINFSS-FSS-* 命名规范）".to_string());
    }
    emit_log(app, "info", &format!("扫描到 {} 个 ZIP 包", pkgs.len()), None);
    let pow_val = compute_pow(&pkgs);

    // ── Phase 2: Create merge dir ──
    let merge_dir_name = format!("SEE-merge-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let merge_dir = dst_dir.join(&merge_dir_name);
    std::fs::create_dir_all(&merge_dir)
        .map_err(|e| format!("创建合并目录失败: {}", e))?;

    // ── Phase 3: Extract all packages into single merge dir (Java last-write-wins) ──
    let pkg_count = pkgs.len();
    let mut total_extracted = 0usize;
    for (idx, pkg) in pkgs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) { let _ = remove_dir_all(&merge_dir); return Err("用户取消".to_string()); }
        let pct = 10 + ((idx as u32) * 50 / pkg_count as u32);
        emit_progress(app, "extracting", pct, idx + 1, pkg_count, Some(pkg.filename.clone()), None);
        let step_start = Instant::now();
        match extract_zip_to_dir(&pkg.path, &merge_dir, cancel) {
            Ok(count) => {
                total_extracted += count;
                let elapsed = step_start.elapsed().as_millis() as u64;
                emit_log(app, "info", &format!("解压 {}/{}: {} ({} 文件, {:.1}s)",
                    idx + 1, pkg_count, pkg.filename, count, elapsed as f64 / 1000.0), Some(elapsed));
            }
            Err(e) => emit_log(app, "warn", &format!("解压失败 {}: {}", pkg.filename, e), None),
        }
        // Java: Stack.zip extraction for 2024 versions
        if options.slim_mode.unwrap_or(false) {
            for entry in std::fs::read_dir(&merge_dir).ok().into_iter().flatten().filter_map(|e| e.ok()) {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.contains("Stack.zip") {
                    let stack_dir = merge_dir.join("Stack");
                    std::fs::create_dir_all(&stack_dir)
                        .map_err(|e| format!("创建 Stack 目录失败: {}", e))?;
                    let _ = extract_zip_to_dir(&entry.path(), &stack_dir, cancel);
                    let _ = std::fs::remove_file(&entry.path());
                    emit_log(app, "info", "已提取 Stack.zip → Stack/", None);
                }
            }
        }
    }

    // ── Phase 4: fiterFiles (Java: inner .zip dedup) ──
    emit_progress(app, "dedup", 55, pkg_count, pkg_count, None, None);
    let dedup_count = fiter_files(&merge_dir, cancel, pow_val)?;
    if dedup_count > 0 {
        emit_log(app, "info", &format!("内层包去重: 清理 {} 个重复包", dedup_count), None);
    }

    // ── Phase 5: Personalized SQL merge (Java MergePersonalizedSqlFiles) ──
    let mut sql_services = 0usize;
    if let Some(ref cust_no) = options.customer_no {
        if !cust_no.is_empty() {
            emit_progress(app, "sql_merge", 65, 0, 0, None, None);
            emit_log(app, "info", &format!("开始个性化 SQL 合并 (客户: {})", cust_no), None);
            let sql_start = Instant::now();
            match merge_personalized_sql(&merge_dir, cust_no,
                options.db_type.as_deref(), options.handle_custom_script.unwrap_or(false), cancel)
            {
                Ok(count) => {
                    sql_services = count;
                    let elapsed = sql_start.elapsed().as_millis() as u64;
                    emit_log(app, "info", &format!("个性化 SQL 处理完成，处理 {} 个服务",
                        count), Some(elapsed));
                }
                Err(e) => emit_log(app, "warn", &format!("个性化 SQL 处理异常: {}", e), None),
            }
        }
    }

    // ── Phase 6: Slim JAR processing (Java slim processing for 2024+ versions) ──
    let mut cust_jars_del = 0usize;
    let mut slim_jars_rem = 0usize;
    if options.slim_mode.unwrap_or(false) {
        emit_progress(app, "slim_jars", 75, 0, 0, None, None);
        emit_log(app, "info", "开始 Slim JAR 处理...", None);
        let slim_start = Instant::now();
        match process_slim_jars(&merge_dir, options.target_version.as_deref(),
            options.customer_no.as_deref(), cancel)
        {
            Ok((cjd, sjr)) => {
                cust_jars_del = cjd;
                slim_jars_rem = sjr;
                let elapsed = slim_start.elapsed().as_millis() as u64;
                let mut parts = Vec::new();
                if cjd > 0 { parts.push(format!("客户个性化JAR删除: {}", cjd)); }
                if sjr > 0 { parts.push(format!("冗余JAR清理: {}", sjr)); }
                let msg = if parts.is_empty() { "Slim JAR 处理完成，无需清理".to_string() }
                    else { format!("Slim JAR 处理完成: {}", parts.join(", ")) };
                emit_log(app, "info", &msg, Some(elapsed));
            }
            Err(e) => emit_log(app, "warn", &format!("Slim JAR 处理异常: {}", e), None),
        }
    }

    // ── Phase 7: MD5 hash.txt ──
    emit_progress(app, "md5", 85, pkg_count, pkg_count, None, None);
    if let Err(e) = create_md5_file(&merge_dir) {
        emit_log(app, "warn", &format!("MD5 文件生成失败: {}", e), None);
    } else {
        emit_log(app, "info", "hash.txt MD5 校验文件已生成", None);
    }

    // ── Phase 8: Final compression ──
    let output_zip_name = format!("{}.zip", &merge_dir_name);
    let output_path = dst_dir.join(&output_zip_name);
    emit_progress(app, "compressing", 90, pkg_count, pkg_count, None, None);
    emit_log(app, "info", "开始压缩合并结果...", None);
    let compress_start = Instant::now();
    crate::merge_common::zip_dir_sorted(&merge_dir, &output_path)?;
    let compress_elapsed = compress_start.elapsed().as_millis() as u64;
    emit_log(app, "info", &format!("压缩完成 (耗时 {:.1}s)", compress_elapsed as f64 / 1000.0), Some(compress_elapsed));
    let _ = remove_dir_all(&merge_dir);
    let total_elapsed = start_total.elapsed().as_millis() as u64;

    Ok(MergeStats {
        output_path: output_path.to_string_lossy().to_string(),
        packages: pkg_count,
        files_extracted: total_extracted,
        packages_deduplicated: dedup_count,
        sql_services_processed: sql_services,
        customer_jars_deleted: cust_jars_del,
        slim_jars_removed: slim_jars_rem,
    })
}

// ── Start Merge (public API) ──

/// 同步入口（源仓 `start_merge`：校验 → `thread::spawn` → 三条事件）。
///
/// 线程由插件进程自己开（`crates/duck-merge-tool`）。源仓那条 `success:false` 的
/// `emit_done` 由 `Err` 承担。
pub fn merge(app: &dyn PkgMergeReporter, options: SeeMergeOptions) -> Result<SeeMergeResult, String> {
    if options.src_dir.is_empty() || options.dst_dir.is_empty() {
        return Err("源目录和目标目录不能为空".to_string());
    }
    let src_dir = PathBuf::from(&options.src_dir);
    if !src_dir.exists() { return Err(format!("源目录不存在: {}", options.src_dir)); }
    let dst_dir = PathBuf::from(&options.dst_dir);
    std::fs::create_dir_all(&dst_dir).map_err(|e| format!("创建目标目录失败: {}", e))?;

    reset_cancel();
    let start = Instant::now();
    match run_merge(app, &src_dir, &dst_dir, &options, cancel_flag()) {
        Ok(stats) => {
            let elapsed = start.elapsed().as_millis() as u64;
            let ext = if stats.sql_services_processed > 0 || stats.customer_jars_deleted > 0 || stats.slim_jars_removed > 0 {
                Some(SeeMergeExtended {
                    personalized_sql_services: stats.sql_services_processed,
                    customer_jars_deleted: stats.customer_jars_deleted,
                    slim_jars_removed: stats.slim_jars_removed,
                })
            } else { None };
            let result = SeeMergeResult {
                success: true,
                output_path: stats.output_path.clone(),
                statistics: SeeMergeStatistics {
                    packages: stats.packages, files_extracted: stats.files_extracted,
                    packages_deduplicated: stats.packages_deduplicated,
                    total_duration_ms: elapsed,
                },
                extended: ext,
            };
            emit_log(app, "success", &format!("合并完成！总耗时: {:.1}s，输出: {}",
                elapsed as f64 / 1000.0, stats.output_path), Some(elapsed));
            emit_progress(app, "done", 100, stats.packages, stats.packages, None, Some(elapsed));
            Ok(result)
        }
        Err(e) => {
            emit_log(app, "error", &e, None);
            Err(e)
        }
    }
}
