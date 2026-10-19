//! 合并族共用的目录 / ZIP / 修改时间基础能力。
//!
//! **本文件逐字复刻**自参考仓 `ifm_ai_tools/src-tauri/src/merge_common.rs`
//! （639 行，含 11 条上游单测）。与 Tauri 零耦合，所以移植时**一行逻辑都没动**，
//! 只加了这段模块头。
//!
//! 23 个函数的用途分四类：
//! - 目录遍历与筛选（`find_file_recursive` / `find_jarlist_files` /
//!   `filter_customer_dirs` / `filter_db_type_dirs` / `find_dir_limited` /
//!   `resolve_merge_root` / `collect_relative_files`）；
//! - ZIP 条目名归一化（`decode_entry_name`：反斜杠 → `/`、非 UTF-8 回退 GBK）；
//! - 修改时间族（`dos_to_naive` / `file_mtime` / `set_file_mtime` /
//!   `copy_file_keep_mtime` / `newer_mtime`）——同名文件「按日期取最新」的裁决依据；
//! - 目录树合并与 ZIP 打包（`merge_tree_mtime_aware` / `has_any_file` /
//!   `zip_dir_sorted`）。
//!
//! ⚠️ 调用方必须按**固定顺序**遍历包：本模块的「日期相同/一方缺失时保留先落盘者」
//! 让结果可复现，但复现的前提是遍历顺序本身确定。

// clippy 豁免逐条列名、**只为本文件服务**：这些都是「改写法才消失」的建议，而改
// 写法正是移植要避免的成本（`collapsible_if` 的正确写法要 Rust 1.88 的 let_chains，
// 本仓 MSRV 是 1.85）。
#![allow(clippy::collapsible_if)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 递归查找指定目录下匹配文件名的第一个文件
pub fn find_file_recursive(dir: &Path, target: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_recursive(&path, target) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(target) {
            return Some(path);
        }
    }
    None
}

/// 查找匹配 jarlist*.txt 模式的所有文件
pub fn find_jarlist_files(dir: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                results.extend(find_jarlist_files(&path));
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("jarlist") && name.ends_with(".txt") {
                    results.push(path);
                }
            }
        }
    }
    results
}

/// 按客户编号过滤目录：遍历 dir 下的子目录，只保留名称包含 customer_no 的目录
/// 返回被删除的目录数量
pub fn filter_customer_dirs(dir: &Path, customer_no: &str) -> Result<usize, String> {
    let mut deleted = 0usize;
    if !dir.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("读取目录失败 {}: {}", dir.display(), e))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(customer_no) {
            std::fs::remove_dir_all(&path)
                .map_err(|e| format!("删除目录失败 {}: {}", path.display(), e))?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// 按数据库类型过滤目录：只保留名称在 db_types 列表中的子目录
/// 返回被删除的目录数量
pub fn filter_db_type_dirs(dir: &Path, db_types: &[String]) -> Result<usize, String> {
    let mut deleted = 0usize;
    if !dir.exists() || db_types.is_empty() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("读取目录失败 {}: {}", dir.display(), e))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !db_types.contains(&name) {
            std::fs::remove_dir_all(&path)
                .map_err(|e| format!("删除目录失败 {}: {}", path.display(), e))?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// 拷贝 .doc / .docx 文件从 src 到 dest 目录
pub fn copy_doc_files(src: &Path, dest: &Path) {
    fn walk_copy(dir: &Path, dest: &Path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk_copy(&path, dest);
                } else if let Some(ext) = path.extension() {
                    if ext == "doc" || ext == "docx" {
                        if let Some(name) = path.file_name() {
                            let target = dest.join(name);
                            let _ = std::fs::copy(&path, &target);
                        }
                    }
                }
            }
        }
    }
    walk_copy(src, dest);
}

/// 读取文件内容为字符串
pub fn read_text_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|e| format!("读取文件失败 {}: {}", path.display(), e))
}

/// 把 ZIP 条目的**原始名字节**解码为归一化路径字符串。
///
/// 做两件事：
/// 1. **分隔符归一化**：把 `\` 一律改为 `/`。包内条目按 ZIP 规范本应为 `/`，
///    但 Windows 侧打包工具会写出 `\`；若不归一化，在 macOS/Linux 上反斜杠是
///    普通字符（不会被 `Path` 切分），会造成「目录层级塌陷、落点错乱」。
/// 2. **编码兜底**：UTF-8 解码失败时按 GBK 解码。`zip` crate 的 `ZipFile::name()`
///    在非 UTF-8 标志位的情况下走 cp437，中文会被解成乱码，因此**必须**用
///    `name_raw()` + 本函数，而不是 `name()`。
///
/// 这是合并类模块的**规范实现**（`non_sp_merge` / `see_merge` / `ifms6_merge`
/// 内各有一份历史私有副本，语义相同，新增代码请统一走这里）。
pub fn decode_entry_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(name) => name.replace('\\', "/"),
        Err(_) => {
            let (decoded, _, _) = encoding_rs::GBK.decode(raw);
            decoded.replace('\\', "/")
        }
    }
}

// ═══════════════════════════════════════════════════════════════
//  同名文件「按修改日期取最新」的共用基础能力
// ═══════════════════════════════════════════════════════════════

/// ZIP 条目的 DOS 时间 → NaiveDateTime。
///
/// DOS 时间不带时区，按本地时间语义直接比较；`month()/day()` 为 0 表示该条目
/// 未写入有效时间（部分打包工具会写出 1979-11-30 之类的占位值），此时返回
/// `None`，由调用方回退到「按包处理顺序覆盖」的旧行为。
pub fn dos_to_naive(dt: zip::DateTime) -> Option<chrono::NaiveDateTime> {
    if dt.month() == 0 || dt.day() == 0 {
        return None;
    }
    let date = chrono::NaiveDate::from_ymd_opt(dt.year() as i32, dt.month() as u32, dt.day() as u32)?;
    date.and_hms_opt(dt.hour() as u32, dt.minute() as u32, dt.second() as u32)
}

/// 读取文件修改时间（本地时间语义）。
pub fn file_mtime(path: &Path) -> Option<chrono::NaiveDateTime> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let dt: chrono::DateTime<chrono::Local> = modified.into();
    Some(dt.naive_local())
}

/// 把文件的修改时间设置为给定值。
///
/// 用于把「ZIP 条目原始修改时间」一路带下去：解压 → 合并复制 → 结果目录，
/// 否则后续 `merge_sql_tree` 之类的按日期去重会因为所有文件时间都是「复制时刻」
/// 而失效。
pub fn set_file_mtime(path: &Path, t: chrono::NaiveDateTime) {
    if let Some(lt) = t.and_local_timezone(chrono::Local).single() {
        let ft = filetime::FileTime::from_unix_time(lt.timestamp(), lt.timestamp_subsec_nanos());
        let _ = filetime::set_file_mtime(path, ft);
    }
}

/// 复制文件并保留其修改时间。
pub fn copy_file_keep_mtime(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::copy(src, dst).map_err(|e| {
        format!("复制文件失败 {} -> {}: {}", src.display(), dst.display(), e)
    })?;
    if let Some(t) = file_mtime(src) {
        set_file_mtime(dst, t);
    }
    Ok(())
}

/// 「取修改日期最新」的通用裁决：返回 true 表示应当用 `new_t` 覆盖 `old_t`。
///
/// 双方都有有效时间时取较新者；时间相同或任一方缺失时返回 false（保留先落盘者），
/// 保证结果可复现（调用方已按固定顺序遍历包与文件）。
pub fn newer_mtime(new_t: Option<chrono::NaiveDateTime>, old_t: Option<chrono::NaiveDateTime>) -> bool {
    matches!((new_t, old_t), (Some(n), Some(o)) if n > o)
}

// ═══════════════════════════════════════════════════════════════
//  源码合并：合并基准定位 + 「按修改日期取最新」的目录树合并
// ═══════════════════════════════════════════════════════════════

/// 源码包内定位 `app/` 骨架目录的搜索深度上限
/// （包根 → 包目录 → Sources → app）
pub const APP_SEARCH_DEPTH: usize = 6;

/// 在 `dir` 下（含自身）限深查找名为 `target` 的目录，返回第一个命中项。
///
/// 按「同级字典序 + 深度优先」推进，保证同一批包始终得到同一结果，不依赖
/// `read_dir` 的平台遍历顺序。
pub fn find_dir_limited(dir: &Path, target: &str, max_depth: usize) -> Option<PathBuf> {
    if max_depth == 0 {
        return None;
    }
    if dir.file_name().and_then(|n| n.to_str()) == Some(target) {
        return Some(dir.to_path_buf());
    }
    let mut subdirs: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
        Err(_) => return None,
    };
    subdirs.sort();
    for sub in subdirs {
        if let Some(found) = find_dir_limited(&sub, target, max_depth - 1) {
            return Some(found);
        }
    }
    None
}

/// 解析一个已解压包目录的「合并基准目录」。
///
/// IFMS3.0 源码包的结构是 `<包目录>/Sources/app/<服务>/...`，基准取 `app/`，
/// 这样多个包的同名路径天然对齐、可直接合并（与 SP 合并输出的 `app/{svc}/...`
/// 骨架同构）。找不到 `app/` 时回退到「只有单一子目录则下钻一层」的旧行为，
/// 最后退回包目录自身。
pub fn resolve_merge_root(pkg_dir: &Path, app_search_depth: usize) -> PathBuf {
    if let Some(app) = find_dir_limited(pkg_dir, "app", app_search_depth) {
        return app;
    }
    let mut subdirs: Vec<PathBuf> = match std::fs::read_dir(pkg_dir) {
        Ok(entries) => entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
        Err(_) => Vec::new(),
    };
    subdirs.sort();
    if subdirs.len() == 1 {
        return subdirs.into_iter().next().expect("single element");
    }
    pkg_dir.to_path_buf()
}

/// 目录树合并的计数结果。
#[derive(Debug, Default, Clone, Copy)]
pub struct MergeCounters {
    pub copied: usize,
    pub overwritten: usize,
    pub kept_older: usize,
}

/// 把 `src` 目录的内容递归合并进 `dst`，同名文件按修改日期取最新。
///
/// - 目标不存在：复制并保留源 mtime；
/// - 目标已存在：仅当源 mtime 更新时才覆盖（时间相同或任一方缺失则保留先落盘者），
///   与 SP 合并、需求单合并的裁决规则一致。
///
/// 调用方须按固定顺序遍历包，以保证结果可复现。
pub fn merge_tree_mtime_aware(
    src: &Path,
    dst: &Path,
    counters: &mut MergeCounters,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst).map_err(|e| format!("创建目录失败 {}: {}", dst.display(), e))?;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(src)
        .map_err(|e| format!("读取目录失败 {}: {}", src.display(), e))?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }
        let name = match p.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let target = dst.join(name);
        if p.is_dir() {
            merge_tree_mtime_aware(&p, &target, counters, cancel)?;
        } else if p.is_file() {
            if target.exists() {
                if newer_mtime(file_mtime(&p), file_mtime(&target)) {
                    copy_file_keep_mtime(&p, &target)?;
                    counters.overwritten += 1;
                } else {
                    counters.kept_older += 1;
                }
            } else {
                copy_file_keep_mtime(&p, &target)?;
                counters.copied += 1;
            }
        }
    }
    Ok(())
}

/// 判断目录树内是否存在至少一个文件。
pub fn has_any_file(dir: &Path) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() {
            return true;
        }
        if p.is_dir() && has_any_file(&p) {
            return true;
        }
    }
    false
}

/// 收集目录树内所有文件（不含目录），返回相对 `root` 的 POSIX 路径列表（字典序）。
///
/// 用于生成「代码清单」：逐行列举每个具体文件及其相对路径。
/// `skip_dirs` 中的顶层目录名会被整棵跳过（例如 `del`）。
pub fn collect_relative_files(root: &Path, skip_dirs: &[&str]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    collect_relative_files_into(root, root, skip_dirs, &mut files);
    files.sort();
    files
}

fn collect_relative_files_into(
    root: &Path,
    dir: &Path,
    skip_dirs: &[&str],
    out: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    // 排序保证遍历顺序跨平台一致（文件与目录混排时也稳定）
    paths.sort();
    for p in paths {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if p.is_dir() {
            if dir == root && skip_dirs.contains(&name.as_str()) {
                continue;
            }
            collect_relative_files_into(root, &p, skip_dirs, out);
        } else if p.is_file() {
            let rel = p.strip_prefix(root).unwrap_or(&p)
                .to_string_lossy().replace('\\', "/");
            out.push(rel);
        }
    }
}

/// 写入字符串到文件
pub fn write_text_file(path: &Path, content: &str) -> Result<(), String> {
    std::fs::write(path, content)
        .map_err(|e| format!("写入文件失败 {}: {}", path.display(), e))
}

// ═══════════════════════════════════════════════════════════════
//  跨平台一致的 ZIP 目录压缩
// ═══════════════════════════════════════════════════════════════

/// 收集目录下所有条目（文件和子目录），返回排序后的 (相对路径, 是否目录) 列表
fn collect_sorted_entries(base: &Path, dir: &Path) -> Result<Vec<(String, bool)>, String> {
    let mut entries: Vec<(String, bool)> = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("读取目录失败 {}: {}", dir.display(), e))?
        .flatten()
    {
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path)
            .to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            entries.push((format!("{}/", rel), true));
            let mut sub = collect_sorted_entries(base, &path)?;
            entries.append(&mut sub);
        } else if path.is_file() {
            entries.push((rel, false));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// 跨平台一致的 ZIP 目录压缩。
///
/// - 路径统一为 POSIX `/` 分隔符（消除 macOS `\` vs Windows `\` 差异）
/// - 条目按字典序排列（消除 `read_dir` 遍历顺序的平台差异）
/// - 使用 Deflated 压缩
pub fn zip_dir_sorted(src_dir: &Path, output_path: &Path) -> Result<(), String> {
    let file = std::fs::File::create(output_path)
        .map_err(|e| format!("创建ZIP文件失败 {}: {}", output_path.display(), e))?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = zip::write::FileOptions::<()>::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let entries = collect_sorted_entries(src_dir, src_dir)?;
    for (rel, is_dir) in &entries {
        if *is_dir {
            zw.add_directory(rel, opts)
                .map_err(|e| format!("添加目录到ZIP失败: {}", e))?;
        } else {
            let full_path = src_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            zw.start_file(rel, opts)
                .map_err(|e| format!("添加文件到ZIP失败: {}", e))?;
            let mut f = std::fs::File::open(&full_path)
                .map_err(|e| format!("打开文件失败 {}: {}", full_path.display(), e))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)
                .map_err(|e| format!("读取文件失败 {}: {}", full_path.display(), e))?;
            zw.write_all(&buf)
                .map_err(|e| format!("写入ZIP失败: {}", e))?;
        }
    }

    zw.finish().map_err(|e| format!("ZIP写入完成失败: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn decode_entry_name_normalizes_backslash_and_falls_back_to_gbk() {
        // 反斜杠一律归一为 `/`：Windows 侧打包写出的条目名在 macOS 上不会被
        // `Path` 切分，属于「静默落点错乱」的隐患，必须在这里抹平。
        assert_eq!(decode_entry_name(b"a\\b\\c.txt"), "a/b/c.txt");
        assert_eq!(decode_entry_name(b"a/b/c.txt"), "a/b/c.txt");

        // 非 UTF-8 时按 GBK 解码（zip 的 name() 会走 cp437 出乱码）
        let gbk: Vec<u8> = encoding_rs::GBK
            .encode("database\\需求单\\说明.upd")
            .0
            .into_owned();
        assert_eq!(decode_entry_name(&gbk), "database/需求单/说明.upd");
    }

    #[test]
    fn find_file_recursive_and_jarlist() {
        let dir = std::env::temp_dir().join(format!("merge_common_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/b/target.txt"), "x").unwrap();
        fs::write(dir.join("a/jarlist1.txt"), "j").unwrap();
        fs::write(dir.join("a/other.txt"), "o").unwrap();

        assert_eq!(
            find_file_recursive(&dir, "target.txt").unwrap(),
            dir.join("a/b/target.txt")
        );
        let lists = find_jarlist_files(&dir);
        assert_eq!(lists.len(), 1);
        assert!(lists[0].ends_with("jarlist1.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn filter_customer_and_db_type_dirs() {
        let base = std::env::temp_dir().join(format!("merge_filter_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let cust = base.join("customers");
        let dbs = base.join("dbs");
        fs::create_dir_all(cust.join("cust_001")).unwrap();
        fs::create_dir_all(cust.join("cust_002")).unwrap();
        fs::create_dir_all(dbs.join("mysql")).unwrap();
        fs::create_dir_all(dbs.join("oracle")).unwrap();

        let deleted = filter_customer_dirs(&cust, "001").unwrap();
        assert_eq!(deleted, 1);
        assert!(cust.join("cust_001").exists());
        assert!(!cust.join("cust_002").exists());

        let deleted_db = filter_db_type_dirs(&dbs, &["mysql".into()]).unwrap();
        assert_eq!(deleted_db, 1);
        assert!(dbs.join("mysql").exists());
        assert!(!dbs.join("oracle").exists());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn read_write_text_file() {
        let path = std::env::temp_dir().join(format!("merge_rw_{}.txt", std::process::id()));
        write_text_file(&path, "hello").unwrap();
        assert_eq!(read_text_file(&path).unwrap(), "hello");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn collect_relative_files_lists_every_file_sorted_and_skips_del() {
        let base = std::env::temp_dir().join(format!("merge_codelist_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("ifmonline/com/hundsun/x")).unwrap();
        fs::create_dir_all(base.join("ifmbatch/y")).unwrap();
        fs::create_dir_all(base.join("del")).unwrap();
        fs::write(base.join("ifmonline/com/hundsun/x/B.java"), "b").unwrap();
        fs::write(base.join("ifmonline/com/hundsun/x/A.java"), "a").unwrap();
        fs::write(base.join("ifmbatch/y/C.java"), "c").unwrap();
        fs::write(base.join("del/pkg.txt"), "should be skipped").unwrap();

        let files = collect_relative_files(&base, &["del"]);
        assert_eq!(
            files,
            vec![
                "ifmbatch/y/C.java".to_string(),
                "ifmonline/com/hundsun/x/A.java".to_string(),
                "ifmonline/com/hundsun/x/B.java".to_string(),
            ]
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn find_dir_limited_locates_app_and_respects_depth() {
        let base = std::env::temp_dir().join(format!("merge_app_root_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("pkg/Sources/app/ifmonline")).unwrap();
        fs::write(base.join("pkg/Sources/app/ifmonline/A.java"), "a").unwrap();

        assert_eq!(
            find_dir_limited(&base.join("pkg"), "app", APP_SEARCH_DEPTH).unwrap(),
            base.join("pkg/Sources/app")
        );
        // 深度不足（app 在第 3 层，只给 2 层）时不命中
        assert!(find_dir_limited(&base.join("pkg"), "app", 2).is_none());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_merge_root_prefers_app_then_single_subdir() {
        let base = std::env::temp_dir().join(format!("merge_root_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);

        // 源码包形态：命中 app/
        fs::create_dir_all(base.join("src/Sources/app/ifmbatch")).unwrap();
        assert_eq!(
            resolve_merge_root(&base.join("src"), APP_SEARCH_DEPTH),
            base.join("src/Sources/app")
        );

        // 无 app：单一子目录则下钻一层
        fs::create_dir_all(base.join("plain/only")).unwrap();
        assert_eq!(
            resolve_merge_root(&base.join("plain"), APP_SEARCH_DEPTH),
            base.join("plain/only")
        );

        // 无 app 且多子目录：退回包目录自身
        fs::create_dir_all(base.join("multi/a")).unwrap();
        fs::create_dir_all(base.join("multi/b")).unwrap();
        assert_eq!(
            resolve_merge_root(&base.join("multi"), APP_SEARCH_DEPTH),
            base.join("multi")
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn merge_tree_keeps_newer_mtime_and_counts() {
        let base = std::env::temp_dir().join(format!("merge_tree_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let old_dir = base.join("old");
        let new_dir = base.join("new");
        let dst = base.join("dst");
        fs::create_dir_all(old_dir.join("svc/T1")).unwrap();
        fs::create_dir_all(new_dir.join("svc/T1")).unwrap();
        fs::create_dir_all(new_dir.join("svc/T2")).unwrap();
        fs::write(old_dir.join("svc/T1/Same.java"), "old-same").unwrap();
        fs::write(new_dir.join("svc/T1/Same.java"), "new-same").unwrap();
        fs::write(new_dir.join("svc/T2/New.java"), "new-only").unwrap();
        // 显式把旧文件的 mtime 拨早，避免依赖写入间隔的纳秒差
        let old_ts = chrono::NaiveDate::from_ymd_opt(2020, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        set_file_mtime(&old_dir.join("svc/T1/Same.java"), old_ts);

        let cancel = AtomicBool::new(false);
        let mut c = MergeCounters::default();
        merge_tree_mtime_aware(&old_dir, &dst, &mut c, &cancel).unwrap();
        merge_tree_mtime_aware(&new_dir, &dst, &mut c, &cancel).unwrap();

        assert_eq!(c.copied, 2);
        assert_eq!(c.overwritten, 1);
        assert_eq!(c.kept_older, 0);
        assert_eq!(fs::read_to_string(dst.join("svc/T1/Same.java")).unwrap(), "new-same");
        assert!(dst.join("svc/T2/New.java").exists());
        assert!(has_any_file(&dst));
        assert!(!has_any_file(&base.join("nonexistent")));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn merge_tree_keeps_existing_when_incoming_is_older() {
        let base = std::env::temp_dir().join(format!("merge_tree_older_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let older_dir = base.join("older");
        let newer_dir = base.join("newer");
        let dst = base.join("dst");
        fs::create_dir_all(&older_dir).unwrap();
        fs::create_dir_all(&newer_dir).unwrap();
        fs::write(older_dir.join("A.java"), "incoming-older").unwrap();
        fs::write(newer_dir.join("A.java"), "already-newer").unwrap();
        let old_ts = chrono::NaiveDate::from_ymd_opt(2020, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        set_file_mtime(&older_dir.join("A.java"), old_ts);

        let cancel = AtomicBool::new(false);
        let mut c = MergeCounters::default();
        merge_tree_mtime_aware(&newer_dir, &dst, &mut c, &cancel).unwrap();
        merge_tree_mtime_aware(&older_dir, &dst, &mut c, &cancel).unwrap();

        assert_eq!(c.copied, 1);
        assert_eq!(c.overwritten, 0);
        assert_eq!(c.kept_older, 1);
        assert_eq!(fs::read_to_string(dst.join("A.java")).unwrap(), "already-newer");

        let _ = fs::remove_dir_all(&base);
    }
}
