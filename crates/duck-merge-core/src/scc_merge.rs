//! SCC 客户包合并（`SccMerge.vue`）：按客户号 / 行业 / 增值包裁剪标准包内容，
//! 支持跨大版本合包。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/scc_merge.rs`（555 行）。
//! 接线与 `non_sp_merge` / `ifms6_merge` 同一套：`emit_*` → [`crate::MergeReporter`]
//! （三个函数体换了实现，参数名 `app` 保留以便逐行 `diff`），
//! `start_merge`（spawn + `emit_done`）→ [`merge`]（同步返回结果）。
//!
//! 源仓的 `emit_done` 在这里删掉：结果直接当返回值给插件，由插件拼结果正文。

// 复刻件的 clippy 偏差逐条列名（理由同 `ifms6_merge`）：这些是参考仓的既有写法。
#![allow(
    clippy::collapsible_if,
    clippy::needless_return,
    clippy::manual_range_contains,
    clippy::needless_borrows_for_generic_args,
    clippy::op_ref,
    clippy::useless_borrows_in_formatting,
    dead_code,
    unused_variables
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::MergeReporter;

static CANCEL_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
fn cancel_flag() -> &'static Arc<AtomicBool> { CANCEL_FLAG.get_or_init(|| Arc::new(AtomicBool::new(false))) }
pub fn cancel() { cancel_flag().store(true, Ordering::SeqCst); }
pub fn reset_cancel() { cancel_flag().store(false, Ordering::SeqCst); }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SccMergeOptions {
    pub src_dir: String,
    pub dst_dir: String,
    pub current_version: String,
    pub target_version: String,
    pub customer_no: String,
    pub db_type: String,
    pub industry: Option<String>,
    pub value_added: Option<String>,
    pub services: Vec<String>,
    pub tech_services: Vec<String>,
    pub stack_enabled: Option<bool>,
    pub is_new_customer: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SccMergeResult {
    pub success: bool,
    pub output_path: String,
    pub statistics: SccMergeStats,
}
#[derive(Debug, Clone, Serialize)]
pub struct SccMergeStats {
    pub packages: usize,
    pub services_merged: usize,
    pub cross_version: bool,
    pub total_duration_ms: u64,
}

// 源仓三条 `app.emit("scc-merge-*", …)`：载荷逐字段搬到 [`MergeReporter`] 上。
// `progress` 只带 `stage / percent / message`，`current/total` 填 0（源仓没有这两格）。
fn emit_progress(app: &dyn MergeReporter, s: &str, pct: u32, msg: Option<&str>) {
    app.progress(s, pct.min(100), 0, 0, msg.map(|m| m.to_string()));
}
fn emit_log(app: &dyn MergeReporter, lvl: &str, msg: &str, dur: Option<u64>) {
    app.log(lvl, msg, dur);
}

fn unzip_to(zip: &Path, dest: &Path) -> Result<usize, String> {
    let f = std::fs::File::open(zip).map_err(|e| format!("打开ZIP失败 {}", e))?;
    let mut a = zip::ZipArchive::new(f).map_err(|e| format!("读ZIP失败 {}", e))?; let mut n = 0;
    for i in 0..a.len() {
        let mut e = match a.by_index(i) { Ok(e) => e, _ => continue };
        if e.is_dir() { let _ = std::fs::create_dir_all(dest.join(e.name())); continue; }
        let en = e.name().to_string(); if en.contains("..") { continue; }
        let fp = dest.join(&en); if let Some(p) = fp.parent() { std::fs::create_dir_all(p).ok(); }
        let mut o = std::fs::File::create(&fp).map_err(|e| format!("创建文件 {}", e))?;
        std::io::copy(&mut e, &mut o).map_err(|e| format!("写入 {}", e))?; n += 1;
    }
    Ok(n)
}
fn rmdir(p: &Path) -> Result<(), String> { if p.exists() { std::fs::remove_dir_all(p).map_err(|e| format!("清理 {}", e))?; } Ok(()) }
fn cpdir(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.exists() { return Ok(()); }
    for e in std::fs::read_dir(src).map_err(|e| format!("读目录 {}", e))?.flatten() {
        let p = e.path(); let r = p.strip_prefix(src).unwrap_or(&p); let t = dst.join(r);
        if p.is_dir() { std::fs::create_dir_all(&t).ok(); cpdir(&p, &t)?; }
        else { if let Some(par) = t.parent() { std::fs::create_dir_all(par).ok(); } std::fs::copy(&p, &t).map_err(|e| format!("拷贝 {}", e))?; }
    }
    Ok(())
}
fn mvfile(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(par) = dst.parent() { std::fs::create_dir_all(par).ok(); }
    std::fs::rename(src, dst).or_else(|_| { std::fs::copy(src, dst).map(|_| ()) }).map_err(|e| format!("移动 {}", e))
}
fn read_file(p: &Path) -> Result<String, String> { std::fs::read_to_string(p).map_err(|e| format!("读文件 {}", e)) }
fn write_file(p: &Path, c: &str) -> Result<(), String> { std::fs::write(p, c).map_err(|e| format!("写文件 {}", e)) }
fn delete_file(p: &Path) { let _ = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }; }

fn get_str_num(s: &str) -> i32 { s.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0) }

fn check_cross_big_version(cur: &str, tgt: &str) -> bool {
    if cur == "000" || cur.len() < 13 || tgt.len() < 13 { return false; }
    &cur[..13] != &tgt[..13]
}

fn get_scripts_version(ver: &str) -> String {
    if let Some(vp) = ver.find('V') {
        let p: Vec<&str> = ver[vp+1..].split('.').collect();
        if p.len() >= 3 {
            let jx = (p[0].chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse::<i32>().unwrap_or(2022) - 2022 + 8).to_string();
            let zl = p[1].parse::<i32>().unwrap_or(0).to_string();
            if let Some(mp) = p[2].find('M') {
                return format!("{}.{}.{}.{}", jx, zl, p[2][..mp].parse::<i32>().unwrap_or(0), p[2][mp+1..].parse::<i32>().unwrap_or(0));
            }
            return format!("{}.{}.{}.0", jx, zl, p[2].parse::<i32>().unwrap_or(0));
        }
    }
    "8.0.0.0".to_string()
}

fn validate_script_version(ver: &str, cur_sv: &str) -> bool {
    let vn = get_str_num(ver); let cn = get_str_num(cur_sv);
    vn >= cn
}

const HSWEALTH_SCC: &str = "hswealth-scc";
const VOLATILE: &str = "Volatile";
const INDUSTRY: &str = "10_Industry";
const VALUE_ADDED: &str = "b20_ValueAdded";
const CUSTOMER_FOLDER: &str = "d40_Customer";

fn run_merge(app: &dyn MergeReporter, src_dir: &Path, dst_dir: &Path, opts: &SccMergeOptions, cancel: &AtomicBool) -> Result<SccMergeResult, String> {
    let start = Instant::now();
    let wn = format!("SCC-merge-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let work = dst_dir.join(&wn);
    std::fs::create_dir_all(&work).map_err(|e| format!("创建工作目录 {}", e))?;

    // Copy packages
    if let Ok(entries) = std::fs::read_dir(src_dir) {
        for e in entries.flatten() { let p = e.path(); let n = e.file_name(); let t = work.join(&n); if p.is_dir() { cpdir(&p, &t)?; } else { let _ = std::fs::copy(&p, &t); } }
    }

    let merge_name = if opts.is_new_customer.unwrap_or(false) {
        format!("{}-merge", opts.target_version)
    } else {
        format!("{}-{}-merge", opts.current_version, opts.target_version)
    };
    let merge_dir = work.join(&merge_name);
    std::fs::create_dir_all(&merge_dir).map_err(|e| format!("创建合并目录 {}", e))?;

    let all_services: Vec<&str> = opts.services.iter().map(|s| s.as_str()).chain(opts.tech_services.iter().map(|s| s.as_str())).collect();
    let cross = check_cross_big_version(&opts.current_version, &opts.target_version);
    let sv = get_scripts_version(&opts.current_version);
    let script_list: Vec<String> = {
        let mut sl = Vec::new();
        if let Some(ref ind) = opts.industry { sl.extend(ind.split(',').map(|s| s.trim().to_string())); }
        if let Some(ref va) = opts.value_added { sl.extend(va.split(',').map(|s| s.trim().to_string())); }
        sl
    };
    let db_types: Vec<&str> = opts.db_type.split(',').map(|s| s.trim()).collect();
    let cust_no = &opts.customer_no;

    emit_progress(app, "extracting", 10, Some("遍历解压包..."));

    // Phase 1: Extract and filter
    let mut pkg_count = 0usize;
    if let Ok(entries) = std::fs::read_dir(&work) {
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
            let p = entry.path();
            if !p.is_file() || p.extension().map(|x| x != "zip").unwrap_or(true) { continue; }
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.contains("-merge") { continue; }

            if fname.starts_with("SCC") && fname.ends_with(".zip") {
                let tmp = work.join(format!(".scc_{}", pkg_count));
                std::fs::create_dir_all(&tmp).ok();
                let _ = unzip_to(&p, &tmp);
                if let Ok(inner) = std::fs::read_dir(&tmp) {
                    for f in inner.flatten() {
                        let fn2 = f.file_name().to_string_lossy().to_string();
                        if opts.tech_services.iter().any(|t| fn2.contains(t)) {
                            let _ = std::fs::copy(f.path(), merge_dir.join(&fn2));
                        }
                        if opts.stack_enabled.unwrap_or(false) && fn2.ends_with(".json") {
                            let _ = std::fs::copy(f.path(), merge_dir.join(&fn2));
                        }
                    }
                }
                let _ = rmdir(&tmp);
            } else {
                let _ = std::fs::copy(&p, merge_dir.join(&fname));
            }
            pkg_count += 1;
        }
    }

    // Phase 2: Script merge
    emit_progress(app, "script_merge", 30, Some("脚本合并..."));
    let mut microservice_map: HashMap<String, PathBuf> = HashMap::new();

    if let Ok(entries) = std::fs::read_dir(&merge_dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(".zip") && n.starts_with(HSWEALTH_SCC) && n.contains("svr") {
                microservice_map.insert(n.clone(), e.path());
            }
        }
    }

    if cross {
        // Process cross-version: find newest version, incrementally copy
        let mut sorted: Vec<String> = microservice_map.keys().cloned().collect();
        sorted.sort_by(|a, b| b.as_bytes().cmp(a.as_bytes()));

        // Get the first (newest) as base
        if let Some(big_key) = sorted.first() {
            if let Some(big_path) = microservice_map.get(big_key) {
                let big_dir = merge_dir.join(format!(".ms_{}", big_key));
                std::fs::create_dir_all(&big_dir).ok();
                let _ = unzip_to(big_path, &big_dir);
                let _ = std::fs::remove_file(big_path);
                let csdk = big_dir.join("core_sdk");
                if csdk.exists() { let _ = unzip_to(&csdk.with_extension("zip"), &csdk); let _ = std::fs::remove_file(&csdk.with_extension("zip")); }

                for key in &sorted {
                    if key == big_key { continue; }
                    if let Some(path) = microservice_map.get(key) {
                        let tmp = merge_dir.join(format!(".ms_tmp_{}", key));
                        std::fs::create_dir_all(&tmp).ok();
                        let _ = unzip_to(path, &tmp);
                        let _ = std::fs::remove_file(path);
                        let tcsdk = tmp.join("core_sdk");
                        if tcsdk.exists() { let _ = unzip_to(&tcsdk.with_extension("zip"), &tcsdk); let _ = std::fs::remove_file(&tcsdk.with_extension("zip")); }

                        // Copy processZips for versions >= current
                        let tproc = tcsdk.join("processZips");
                        let bproc = big_dir.join("core_sdk").join("processZips");
                        if tproc.exists() {
                            if let Ok(cats) = std::fs::read_dir(&tproc) {
                                for cat in cats.flatten() {
                                    let cat_path = cat.path();
                                    if !cat_path.is_dir() { continue; }
                                    if let Ok(vers) = std::fs::read_dir(&cat_path) {
                                        for ver in vers.flatten() {
                                            let vp = ver.path();
                                            if vp.is_dir() && validate_script_version(&ver.file_name().to_string_lossy(), &sv) {
                                                let target = bproc.join(cat.file_name()).join(ver.file_name());
                                                if !target.exists() { cpdir(&vp, &target)?; }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Copy sqls
                        let tsql = tcsdk.join("sqls");
                        let bsql = big_dir.join("core_sdk").join("sqls");
                        if tsql.exists() {
                            if let Ok(db_entries) = std::fs::read_dir(&tsql) {
                                for dbe in db_entries.flatten() {
                                    let db_path = dbe.path();
                                    if !db_path.is_dir() || !db_types.contains(&db_path.file_name().and_then(|n| n.to_str()).unwrap_or("")) { continue; }
                                    if let Ok(svc_entries) = std::fs::read_dir(&db_path) {
                                        for svc in svc_entries.flatten() {
                                            let svc_path = svc.path();
                                            if !svc_path.is_dir() { continue; }
                                            if let Ok(vers) = std::fs::read_dir(&svc_path) {
                                                for ver in vers.flatten() {
                                                    let vp = ver.path();
                                                    if vp.is_dir() && validate_script_version(&ver.file_name().to_string_lossy(), &sv) {
                                                        let target = bsql.join(dbe.file_name()).join(svc.file_name()).join(ver.file_name());
                                                        if !target.exists() { cpdir(&vp, &target)?; }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Filter processZips: remove entries < current version
                        if bproc.exists() {
                            if let Ok(cats) = std::fs::read_dir(&bproc) {
                                for cat in cats.flatten() {
                                    let cp = cat.path();
                                    if !cp.is_dir() { continue; }
                                    if let Ok(vers) = std::fs::read_dir(&cp) {
                                        for ver in vers.flatten() {
                                            let vp = ver.path();
                                            if vp.is_dir() && !validate_script_version(&ver.file_name().to_string_lossy(), &sv) {
                                                delete_file(&vp);
                                            } else if vp.is_dir() {
                                                filter_customer_dirs(&vp, cust_no, &script_list, &opts.industry, &opts.value_added)?;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Filter sqls
                        if bsql.exists() {
                            if let Ok(dbes) = std::fs::read_dir(&bsql) {
                                for dbe in dbes.flatten() {
                                    let dbp = dbe.path();
                                    if !dbp.is_dir() || !db_types.contains(&dbp.file_name().and_then(|n| n.to_str()).unwrap_or("")) { delete_file(&dbp); continue; }
                                    if let Ok(svc_entries) = std::fs::read_dir(&dbp) {
                                        for svc in svc_entries.flatten() {
                                            let sp = svc.path();
                                            if !sp.is_dir() { continue; }
                                            if let Ok(vers) = std::fs::read_dir(&sp) {
                                                for ver in vers.flatten() {
                                                    let vp = ver.path();
                                                    if vp.is_dir() && !validate_script_version(&ver.file_name().to_string_lossy(), &sv) { delete_file(&vp); }
                                                    else if vp.is_dir() {
                                                        let vol = vp.join(VOLATILE);
                                                        if vol.exists() { filter_volatile_dir(&vol, &script_list, cust_no, &opts.value_added)?; }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            remove_empty_dirs(&bsql);
                        }
                        let _ = rmdir(&tmp);
                    }
                }

                // Re-zip core_sdk
                let csdk_path = big_dir.join("core_sdk");
                if csdk_path.exists() { let _ = crate::merge_common::zip_dir_sorted(&csdk_path, &csdk_path.with_extension("zip")); let _ = rmdir(&csdk_path); }
                let _ = crate::merge_common::zip_dir_sorted(&big_dir, &merge_dir.join(big_key));
                let _ = rmdir(&big_dir);
            }
        }
    } else {
        // Non-cross version: process each microservice
        let ms_keys: Vec<String> = microservice_map.keys().cloned().collect();
        for key in &ms_keys {
            if cancel.load(Ordering::Relaxed) { return Err("用户取消".to_string()); }
            if let Some(path) = microservice_map.get(key) {
                let ms_dir = merge_dir.join(format!(".ms_{}", key));
                std::fs::create_dir_all(&ms_dir).ok();
                let _ = unzip_to(path, &ms_dir);
                let _ = std::fs::remove_file(path);
                let csdk = ms_dir.join("core_sdk");
                if csdk.exists() { let _ = unzip_to(&csdk.with_extension("zip"), &csdk); let _ = std::fs::remove_file(&csdk.with_extension("zip")); }

                // Filter processZips
                let proc_dir = csdk.join("processZips");
                if proc_dir.exists() {
                    if let Ok(cats) = std::fs::read_dir(&proc_dir) {
                        for cat in cats.flatten() {
                            let cp = cat.path(); if !cp.is_dir() { continue; }
                            if let Ok(vers) = std::fs::read_dir(&cp) {
                                for ver in vers.flatten() {
                                    let vp = ver.path(); if !vp.is_dir() { continue; }
                                    if !opts.is_new_customer.unwrap_or(false) && !validate_script_version(&ver.file_name().to_string_lossy(), &sv) {
                                        delete_file(&vp);
                                    } else {
                                        filter_customer_dirs(&vp, cust_no, &script_list, &opts.industry, &opts.value_added)?;
                                    }
                                }
                            }
                        }
                    }
                }
                remove_empty_dirs(&proc_dir);

                // Filter sqls
                let sql_dir = csdk.join("sqls");
                if sql_dir.exists() {
                    if let Ok(dbes) = std::fs::read_dir(&sql_dir) {
                        for dbe in dbes.flatten() {
                            let dbp = dbe.path(); if !dbp.is_dir() { continue; }
                            if !db_types.contains(&dbp.file_name().and_then(|n| n.to_str()).unwrap_or("")) { delete_file(&dbp); continue; }
                            if let Ok(svc_entries) = std::fs::read_dir(&dbp) {
                                for svc in svc_entries.flatten() {
                                    let sp = svc.path(); if !sp.is_dir() { continue; }
                                    if let Ok(vers) = std::fs::read_dir(&sp) {
                                        for ver in vers.flatten() {
                                            let vp = ver.path(); if !vp.is_dir() { continue; }
                                            if !opts.is_new_customer.unwrap_or(false) && get_str_num(&ver.file_name().to_string_lossy()) < get_str_num(&sv) {
                                                delete_file(&vp);
                                            } else {
                                                let vol = vp.join(VOLATILE);
                                                if vol.exists() { filter_volatile_dir(&vol, &script_list, cust_no, &opts.value_added)?; }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    remove_empty_dirs(&sql_dir);
                }

                // Filter database by db_type
                let db_dir = csdk.join("database");
                if db_dir.exists() {
                    if let Ok(dbes) = std::fs::read_dir(&db_dir) {
                        for dbe in dbes.flatten() {
                            let n = dbe.file_name().to_string_lossy().to_string();
                            if !db_types.contains(&n.as_str()) { delete_file(&dbe.path()); }
                        }
                    }
                }

                // Re-zip core_sdk
                if csdk.exists() { let _ = crate::merge_common::zip_dir_sorted(&csdk, &csdk.with_extension("zip")); let _ = rmdir(&csdk); }
                let _ = crate::merge_common::zip_dir_sorted(&ms_dir, &merge_dir.join(key));
                let _ = rmdir(&ms_dir);
            }
        }
    }

    // Phase 3: fiterFiles (filter by service list)
    emit_progress(app, "filtering", 60, Some("服务过滤..."));
    if let Ok(entries) = std::fs::read_dir(&merge_dir) {
        for entry in entries.flatten() {
            let n = entry.file_name().to_string_lossy().to_string();
            if n.ends_with(".zip") {
                let keep = all_services.iter().any(|s| n.contains(s));
                if !keep { delete_file(&entry.path()); }
            }
        }
    }

    // Phase 4: Handle stack-deploy.json
    emit_progress(app, "stack_json", 75, Some("处理 Stack JSON..."));
    if opts.stack_enabled.unwrap_or(false) {
        // Process stack-deploy.json: update version and techStack
        let stack_json = merge_dir.join("stack-deploy.json");
        if stack_json.exists() {
            if let Ok(content) = read_file(&stack_json) {
                if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) {
                    json["version"] = serde_json::Value::String(merge_name.trim_end_matches("-merge").to_string());
                    if let Some(tech) = json["techStack"].as_array_mut() {
                        let mut new_tech = Vec::new();
                        for item in tech.iter() {
                            let id = item["id"].as_str().unwrap_or("").to_string();
                            let mut keep = true;
                            if let Ok(entries) = std::fs::read_dir(&merge_dir) {
                                keep = entries.flatten().any(|e| {
                                    let en = e.file_name().to_string_lossy().to_lowercase();
                                    en.contains(&id.to_lowercase()) && en.ends_with(".zip")
                                });
                            }
                            if keep { new_tech.push(item.clone()); }
                        }
                        json["techStack"] = serde_json::Value::Array(new_tech);
                    }
                    if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                        let _ = write_file(&stack_json, &pretty);
                    }
                }
            }
        }
    } else {
        let _ = std::fs::remove_file(merge_dir.join("stack-deploy.json"));
    }

    // Phase 5: Organize doc files
    let doc_dir = merge_dir.join("doc");
    std::fs::create_dir_all(&doc_dir).ok();
    if let Ok(entries) = std::fs::read_dir(&merge_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let n = entry.file_name().to_string_lossy().to_string();
            if n != "stack-deploy.json" && !n.ends_with(".zip") && p.is_file() {
                let dst = doc_dir.join(&n);
                let _ = mvfile(&p, &dst);
            }
        }
    }

    // Phase 6: Final compress
    emit_progress(app, "compressing", 90, Some("最终压缩..."));
    let out_name = format!("{}.zip", &merge_name);
    let out_path = work.join(&out_name);
    let _ = crate::merge_common::zip_dir_sorted(&merge_dir, &out_path);
    let _ = rmdir(&merge_dir);

    let final_out = dst_dir.join(&out_name);
    let _ = std::fs::copy(&out_path, &final_out);
    let _ = rmdir(&work);

    let elapsed = start.elapsed().as_millis() as u64;
    emit_log(app, "success", &format!("SCC 合包完成: {}", final_out.display()), Some(elapsed));
    Ok(SccMergeResult {
        success: true, output_path: final_out.to_string_lossy().to_string(),
        statistics: SccMergeStats { packages: pkg_count, services_merged: microservice_map.len(), cross_version: cross, total_duration_ms: elapsed },
    })
}

fn filter_customer_dirs(dir: &Path, cust_no: &str, script_list: &[String], industry: &Option<String>, value_added: &Option<String>) -> Result<(), String> {
    if !dir.exists() { return Ok(()); }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path(); if !p.is_dir() { continue; }
            let n = entry.file_name().to_string_lossy().to_string();
            // Customer dirs: keep if contains customer_no OR is in script_list
            let keep = n.contains(cust_no) || script_list.iter().any(|s| n.contains(s));
            if !keep { delete_file(&p); }
        }
    }
    Ok(())
}

fn filter_volatile_dir(vol: &Path, script_list: &[String], cust_no: &str, value_added: &Option<String>) -> Result<(), String> {
    if !vol.exists() { return Ok(()); }
    if let Ok(entries) = std::fs::read_dir(vol) {
        for entry in entries.flatten() {
            let p = entry.path(); if !p.is_dir() { continue; }
            let n = entry.file_name().to_string_lossy().to_string();
            if p.to_string_lossy().contains(VALUE_ADDED) {
                if let Ok(subs) = std::fs::read_dir(&p) {
                    for sub in subs.flatten() {
                        let sn = sub.file_name().to_string_lossy().to_string();
                        let keep = sn.contains(cust_no) || script_list.iter().any(|s| sn.contains(s));
                        if !keep { delete_file(&sub.path()); }
                    }
                }
            } else {
                let keep = n.contains(cust_no) || script_list.iter().any(|s| n.contains(s));
                if !keep { delete_file(&p); }
            }
        }
    }
    Ok(())
}

fn remove_empty_dirs(dir: &Path) {
    fn walk(d: &Path) -> bool {
        if let Ok(entries) = std::fs::read_dir(d) {
            let mut has_content = false;
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() { if !walk(&p) { delete_file(&p); } else { has_content = true; } }
                else { has_content = true; }
            }
            has_content
        } else { false }
    }
    walk(dir);
}

/// 同步入口（源仓 `start_merge`：校验 → `thread::spawn` → 三条事件）。
///
/// 线程由插件进程自己开（`crates/duck-merge-tool`），这里只做「校验 → 干活 →
/// 回报」三件事；失败信息随 `Err` 返回，源仓那条 `success:false` 的 `emit_done`
/// 就由 `Err` 承担。
pub fn merge(app: &dyn MergeReporter, options: SccMergeOptions) -> Result<SccMergeResult, String> {
    if options.src_dir.is_empty() || options.dst_dir.is_empty() { return Err("源目录和目标目录不能为空".to_string()); }
    let src = PathBuf::from(&options.src_dir); if !src.exists() { return Err(format!("源目录不存在: {}", options.src_dir)); }
    let dst = PathBuf::from(&options.dst_dir); std::fs::create_dir_all(&dst).map_err(|e| format!("创建目标目录失败: {}", e))?;
    reset_cancel();
    let s = Instant::now();
    let r = run_merge(app, &src, &dst, &options, cancel_flag());
    match r {
        Ok(st) => { let el = s.elapsed().as_millis() as u64; emit_log(app, "success", &format!("完成 {:.1}s", el as f64/1000.0), Some(el)); emit_progress(app, "done", 100, None); Ok(st) }
        Err(e) => { emit_log(app, "error", &e, None); Err(e) }
    }
}
