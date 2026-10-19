//! 清单提取（`duck.merge.manifest-extract`）。
//!
//! **复刻自** `ifm_ai_tools/src-tauri/src/extract_by_manifest.rs`（503 行，生产已验证）。
//! 逻辑逐字保留：清单解析（`【小节】` / `#` 头、` V1.2.3` 版本后缀）、路径清洗
//! （去前导 `/`、去 `Sources/` 前缀）、四级查找（精确 → `app/` 结构变换 →
//! 全目录同名打分 → 父目录 `walkdir` 兜底）、逐条复制并保留相对路径。
//!
//! **只改了两处接线**（与 `sql_merge` 同款）：
//! 1. `AppHandle + Emitter` 的三个 `emit_*` → [`ExtractReporter`] 的三个方法；
//! 2. `std::thread::spawn` 入口 → 同步函数 [`extract`]（线程由插件进程开）。
//!
//! 取消沿用源仓的进程级静态标志（`cancel` / `reset_cancel`）：宿主点「取消」时
//! 是给进程发信号，模块内的标志只服务于「插件进程自己想停」的场景，与
//! `sql_merge` 完全一致。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// 过程回执（源仓 `emit_progress` / `emit_log` / `emit_done` 的替身）。
pub trait ExtractReporter {
    /// 逐条进度：`current` 从 1 开始，`total` 是清单条目数。
    fn progress(&self, current: usize, total: usize, current_file: &str);
    /// 一行过程日志。`level`：`info` / `warn` / `error`。
    fn log(&self, level: &str, message: &str);
    /// 完成回执（取消也走这里，`success = false`）。
    fn done(&self, result: &ExtractResult);
}

static CANCEL_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn cancel_flag() -> &'static Arc<AtomicBool> {
    CANCEL_FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// 请求取消（源仓同名函数）。
pub fn cancel() {
    cancel_flag().store(true, Ordering::SeqCst);
}

/// 清掉取消标志（源仓同名函数；每次运行开始前调）。
pub fn reset_cancel() {
    cancel_flag().store(false, Ordering::SeqCst);
}

/// 一次提取的输入（源仓 `ExtractOptions`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractOptions {
    pub manifest_path: String,
    pub src_dir: String,
    pub dst_dir: String,
}

/// 单条清单项的结局（与前端 `results` 一一对应）。
#[derive(Debug, Clone, Serialize)]
pub struct ExtractFileResult {
    pub manifest_path: String,
    pub source_path: Option<String>,
    pub found: bool,
    pub error: Option<String>,
}

/// 一次运行的汇总（与前端 `result` 一一对应）。
#[derive(Debug, Clone, Serialize)]
pub struct ExtractResult {
    pub success: bool,
    pub total: usize,
    pub found: usize,
    pub not_found: usize,
    pub results: Vec<ExtractFileResult>,
    pub total_duration_ms: u64,
}

fn is_section_header(line: &str) -> bool {
    line.starts_with('\u{3010}') || line.starts_with("【") || line.starts_with('#')
}

fn strip_version_suffix(line: &str) -> &str {
    if let Some(pos) = line.rfind(" V") {
        let rest = &line[pos + 2..];
        if !rest.is_empty()
            && (rest.starts_with('V') || rest.chars().next().is_some_and(|c| c.is_ascii_digit()))
            && rest.chars().all(|c| c == '.' || c.is_ascii_digit())
        {
            return &line[..pos];
        }
    }
    line
}

fn parse_manifest(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !is_section_header(l))
        .map(|l| strip_version_suffix(l).to_string())
        .collect()
}

fn clean_path(raw: &str) -> String {
    let s = raw.trim_start_matches('/');
    s.strip_prefix("Sources/").unwrap_or(s).to_string()
}

/// Known Java package prefixes used to identify the package path within a file path.
const PKG_PREFIXES: &[&str] = &[
    "com/", "org/", "net/", "javax/", "java/", "sun/", "io/", "cn/",
];

/// Known business module directories in SP merge output.
const BUS_MODULES: &[&str] = &["base", "fund", "lcdx", "bfina", "database"];

/// Extract the filename stem and extension from a path-like string.
fn path_parts(p: &str) -> (String, String) {
    let path = Path::new(p);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    (stem, ext)
}

/// Try to find a file by exact path with extension adaptation.
fn find_exact(src_dir: &Path, cleaned: &str) -> Option<PathBuf> {
    let full = src_dir.join(cleaned);
    if full.exists() && full.is_file() {
        return Some(full);
    }

    let p = Path::new(cleaned);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let parent = p.parent().unwrap_or(Path::new(""));
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let alt_ext = match ext {
        "java" => Some("class"),
        "class" => Some("java"),
        _ => None,
    };
    if let Some(ae) = alt_ext {
        let alt = src_dir.join(parent).join(format!("{}.{}", stem, ae));
        if alt.exists() && alt.is_file() {
            return Some(alt);
        }
    }

    None
}

/// Search the entire src_dir for a file by matching its stem and trying extensions.
fn find_by_stem_global(
    src_dir: &Path,
    stem: &str,
    exts: &[&str],
    package_hint: &str,
    module_hint: &str,
) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    let mut best_score: i32 = -1;

    for entry in walkdir::WalkDir::new(src_dir)
        .max_depth(15)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let ep = entry.path();
        let entry_stem = ep.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if entry_stem != stem {
            continue;
        }
        let entry_ext = ep.extension().and_then(|s| s.to_str()).unwrap_or("");
        if !exts.contains(&entry_ext) && !exts.is_empty() {
            continue;
        }

        let entry_str = ep.to_string_lossy().replace('\\', "/");

        // Score: higher = better match
        let mut score = 0i32;

        // Prefer paths containing the module hint
        if !module_hint.is_empty() && entry_str.contains(module_hint) {
            score += 10;
        }
        // Prefer paths containing the package hint
        if !package_hint.is_empty() && entry_str.contains(package_hint) {
            score += 20;
        }
        // Prefer WEB-INF/classes for class files
        if entry_ext == "class" && entry_str.contains("WEB-INF/classes") {
            score += 5;
        }
        // Prefer exact match of the tail of the path
        if !package_hint.is_empty() {
            let expected_tail = format!("{}/{}", package_hint.trim_end_matches('/'), stem);
            if entry_str.contains(&expected_tail) {
                score += 30;
            }
        }

        if score > best_score {
            best_score = score;
            best = Some(ep.to_path_buf());
        }
    }

    best
}

/// Build candidate paths from a manifest entry (after cleaning) and test each.
/// The cleaned path format is: "app/{module}/{subpath...}" or "app/spsql/{version}/{module}/..."
fn find_by_path_transforms(src_dir: &Path, cleaned: &str) -> Option<PathBuf> {
    // Split into components
    let normalized = cleaned.replace('\\', "/");

    // Must start with "app/"
    let rest = normalized.strip_prefix("app/")?;

    // Split into module and subpath
    let first_slash = rest.find('/')?;
    let module = &rest[..first_slash];
    let subpath = &rest[first_slash + 1..];

    let ext = &path_parts(subpath).1;

    // Case 1: subpath starts with "WebContent/". Strip it.
    let sub_variants = if let Some(wc_rest) = subpath.strip_prefix("WebContent/") {
        vec![subpath.to_string(), wc_rest.to_string()]
    } else {
        vec![subpath.to_string()]
    };

    // Case 2: Handle spsql -> database mapping
    if module == "spsql" {
        for sub in &sub_variants {
            for bus_mod in BUS_MODULES {
                let candidate = src_dir.join(bus_mod).join(module).join(sub);
                if candidate.exists() && candidate.is_file() {
                    return Some(candidate);
                }
                // Try with extension swap
                if let Some(found) = find_exact(src_dir, &format!("{}/{}/{}", bus_mod, module, sub))
                {
                    return Some(found);
                }
            }
        }
    }

    // General case: try each business module
    for sub in &sub_variants {
        for bus_mod in BUS_MODULES {
            let candidate = src_dir.join(bus_mod).join(module).join(sub);

            // Direct match
            if candidate.exists() && candidate.is_file() {
                return Some(candidate);
            }

            // Extension swap at the direct path
            let candidate_str = format!("{}/{}/{}", bus_mod, module, sub);
            if let Some(found) = find_exact(src_dir, &candidate_str) {
                return Some(found);
            }

            // If the subpath starts with "ifm/", also try "WEB-INF/classes/" for .class files
            if let Some(pkg_rest) = sub.strip_prefix("ifm/") {
                let class_path = format!("{}/{}/WEB-INF/classes/{}", bus_mod, module, pkg_rest);
                let class_p = Path::new(&class_path);
                let class_stem = class_p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let class_parent = class_p.parent().unwrap_or(Path::new(""));

                if ext == "java" || ext == "class" {
                    let alt = src_dir
                        .join(class_parent)
                        .join(format!("{}.class", class_stem));
                    if alt.exists() && alt.is_file() {
                        return Some(alt);
                    }
                    let alt2 = src_dir
                        .join(class_parent)
                        .join(format!("{}.java", class_stem));
                    if alt2.exists() && alt2.is_file() {
                        return Some(alt2);
                    }
                }

                if let Some(found) = find_exact(src_dir, &class_path) {
                    return Some(found);
                }
            }

            // For entries that already look like package paths (contain com/ etc.),
            // also try under WEB-INF/classes/
            if ext.as_str() == "java" || ext.as_str() == "class" {
                let has_pkg = PKG_PREFIXES.iter().any(|p| sub.contains(p));
                if has_pkg {
                    let class_path = format!("{}/{}/WEB-INF/classes/{}", bus_mod, module, sub);
                    if let Some(found) = find_exact(src_dir, &class_path) {
                        return Some(found);
                    }
                }
            }
        }
    }

    None
}

/// Extract the Java package path (e.g. "com/hundsun/lcpt/console/...") from a cleaned manifest entry.
fn extract_package_hint(cleaned: &str) -> String {
    let normalized = cleaned.replace('\\', "/");

    // Look for package prefixes in the path
    for prefix in PKG_PREFIXES {
        if let Some(pos) = normalized.find(prefix) {
            // Extract from the package prefix to the end, then remove filename
            let from_pkg = &normalized[pos..];
            let last_slash = from_pkg.rfind('/');
            return match last_slash {
                Some(slash) => from_pkg[..slash].to_string(),
                None => from_pkg.to_string(),
            };
        }
    }

    String::new()
}

/// Extract the app module name from a cleaned manifest entry.
fn extract_module_hint(cleaned: &str) -> String {
    let normalized = cleaned.replace('\\', "/");
    // Format: "app/{module}/..."
    if let Some(rest) = normalized.strip_prefix("app/") {
        if let Some(slash) = rest.find('/') {
            return rest[..slash].to_string();
        }
        return rest.to_string();
    }
    String::new()
}

/// Main fuzzy search: tries multiple strategies in order of precision.
fn find_file_fuzzy(src_dir: &Path, entry: &str) -> Option<PathBuf> {
    let cleaned = clean_path(entry);

    // 1. Exact match (existing behavior)
    if let Some(found) = find_exact(src_dir, &cleaned) {
        return Some(found);
    }

    // 2. Path transformation (new): try structural mappings from manifest to SP output
    //
    // ⚠️ 不要写成 `a && let Some(x) = b`：let chains 是 1.88 的语法，本仓 MSRV 是 1.85，
    // 而 clippy 的 `collapsible_if` 恰好只会给那个建议。这里把「前置条件」并进一条
    // 表达式（`then`），两边都满足。
    let transformed = cleaned
        .starts_with("app/")
        .then(|| find_by_path_transforms(src_dir, &cleaned))
        .flatten();
    if let Some(found) = transformed {
        return Some(found);
    }

    // 3. Extract package hint and filename for global fuzzy search
    let (stem, ext) = path_parts(&cleaned);
    if stem.is_empty() {
        return None;
    }

    let try_exts: &[&str] = match ext.as_str() {
        "java" => &["class", "java"],
        "class" => &["java", "class"],
        "jar" => &["jar"],
        _ => {
            if ext.is_empty() {
                return None;
            }
            &[ext.as_str()]
        }
    };

    let package_hint = extract_package_hint(&cleaned);
    let module_hint = extract_module_hint(&cleaned);

    if let Some(found) = find_by_stem_global(src_dir, &stem, try_exts, &package_hint, &module_hint)
    {
        return Some(found);
    }

    // 4. Final fallback: legacy walkdir in parent
    find_walkdir(src_dir, &cleaned)
}

/// Fallback: recursively search in the parent directory for a filename match
fn find_walkdir(src_dir: &Path, cleaned: &str) -> Option<PathBuf> {
    let p = Path::new(cleaned);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let parent = p.parent().unwrap_or(Path::new(""));
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let search_exts: &[&str] = match ext {
        "java" | "class" => &["class", "java"],
        "jar" => &["jar"],
        _ => &[ext],
    };

    let search_root = src_dir.join(parent);
    if !search_root.exists() {
        return None;
    }

    for entry in walkdir::WalkDir::new(&search_root)
        .max_depth(5)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let ep = Path::new(&*name);
        let e_stem = ep.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let e_ext = ep.extension().and_then(|s| s.to_str()).unwrap_or("");
        if e_stem == stem && search_exts.contains(&e_ext) {
            return Some(entry.path().to_path_buf());
        }
    }
    None
}

/// 清单提取主流程（源仓 `start_extract`，去掉线程与 `AppHandle`）。
///
/// # Errors
/// 入参缺失、源目录不存在、目标目录建不出来、清单读不出来、清单为空——
/// 都是**派发前**就能判定的事，返回源仓同一批错误文案。
pub fn extract(
    reporter: &dyn ExtractReporter,
    options: &ExtractOptions,
) -> Result<ExtractResult, String> {
    if options.manifest_path.is_empty() || options.src_dir.is_empty() || options.dst_dir.is_empty()
    {
        return Err("清单文件、源目录和目标目录不能为空".to_string());
    }

    let src_dir = PathBuf::from(&options.src_dir);
    if !src_dir.exists() {
        return Err(format!("源目录不存在: {}", options.src_dir));
    }
    let dst_dir = PathBuf::from(&options.dst_dir);
    std::fs::create_dir_all(&dst_dir).map_err(|e| format!("创建目标目录失败: {}", e))?;

    let manifest_raw = std::fs::read_to_string(&options.manifest_path)
        .map_err(|e| format!("读取清单文件失败: {}", e))?;
    let entries = parse_manifest(&manifest_raw);
    if entries.is_empty() {
        return Err("清单文件中没有有效的文件条目".to_string());
    }

    reset_cancel();

    let start = Instant::now();
    let total = entries.len();
    let mut results = Vec::with_capacity(total);
    let mut found_count = 0usize;

    for (i, entry) in entries.iter().enumerate() {
        if cancel_flag().load(Ordering::Relaxed) {
            let elapsed = start.elapsed().as_millis() as u64;
            let result = ExtractResult {
                success: false,
                total,
                found: found_count,
                not_found: total - found_count,
                results,
                total_duration_ms: elapsed,
            };
            reporter.done(&result);
            return Ok(result);
        }

        reporter.progress(i + 1, total, entry);

        // Skip directory entries (paths ending with /)
        if entry.ends_with('/') {
            reporter.log("info", &format!("跳过目录条目: {}", entry));
            results.push(ExtractFileResult {
                manifest_path: entry.clone(),
                source_path: None,
                found: false,
                error: Some("目录条目，已跳过".to_string()),
            });
            continue;
        }

        let source = find_file_fuzzy(&src_dir, entry);

        match source {
            Some(sp) => {
                let rel = sp.strip_prefix(&src_dir).unwrap_or(&sp);
                let dest = dst_dir.join(rel);

                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::copy(&sp, &dest) {
                    Ok(_) => {
                        found_count += 1;
                        results.push(ExtractFileResult {
                            manifest_path: entry.clone(),
                            source_path: Some(sp.to_string_lossy().to_string()),
                            found: true,
                            error: None,
                        });
                    }
                    Err(e) => {
                        reporter.log("error", &format!("复制失败 {}: {}", entry, e));
                        results.push(ExtractFileResult {
                            manifest_path: entry.clone(),
                            source_path: Some(sp.to_string_lossy().to_string()),
                            found: true,
                            error: Some(format!("复制失败: {}", e)),
                        });
                    }
                }
            }
            None => {
                reporter.log("warn", &format!("未找到: {}", entry));
                results.push(ExtractFileResult {
                    manifest_path: entry.clone(),
                    source_path: None,
                    found: false,
                    error: Some("未找到匹配文件".to_string()),
                });
            }
        }
    }

    let elapsed = start.elapsed().as_millis() as u64;
    let result = ExtractResult {
        success: true,
        total,
        found: found_count,
        not_found: total - found_count,
        results,
        total_duration_ms: elapsed,
    };
    reporter.done(&result);
    Ok(result)
}

#[cfg(test)]
mod tests {
    //! 源仓没有单测，这里补的是**移植正确性**的锚点：清单解析的三条规则
    //! （小节头 / 版本后缀 / 空行）与「保留相对路径复制」这一条主路径。

    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        logs: Mutex<Vec<(String, String)>>,
        done: Mutex<Option<ExtractResult>>,
    }

    impl ExtractReporter for Recorder {
        fn progress(&self, _current: usize, _total: usize, _current_file: &str) {}
        fn log(&self, level: &str, message: &str) {
            self.logs
                .lock()
                .expect("lock")
                .push((level.to_owned(), message.to_owned()));
        }
        fn done(&self, result: &ExtractResult) {
            *self.done.lock().expect("lock") = Some(result.clone());
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-merge-extract-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn manifest_parsing_skips_headers_blank_lines_and_version_suffix() {
        let manifest = "【核心系统】\n# 注释\n\n  app/base/ifm/A.java V1.2.3\napp/fund/B.java\n";
        assert_eq!(
            parse_manifest(manifest),
            vec!["app/base/ifm/A.java", "app/fund/B.java"]
        );
    }

    #[test]
    fn clean_path_drops_leading_slash_and_sources_prefix() {
        assert_eq!(clean_path("/Sources/app/x/Y.java"), "app/x/Y.java");
        assert_eq!(clean_path("app/x/Y.java"), "app/x/Y.java");
    }

    #[test]
    fn copies_found_files_and_reports_missing_ones() {
        let root = temp_dir("copy");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("base/ifm")).expect("建源目录");
        std::fs::write(src.join("base/ifm/A.java"), "class A {}").expect("写源文件");
        let dst = root.join("dst");
        let manifest = root.join("list.txt");
        std::fs::write(&manifest, "base/ifm/A.java\nbase/ifm/Missing.java\n").expect("写清单");

        let recorder = Recorder::default();
        let result = extract(
            &recorder,
            &ExtractOptions {
                manifest_path: manifest.to_string_lossy().to_string(),
                src_dir: src.to_string_lossy().to_string(),
                dst_dir: dst.to_string_lossy().to_string(),
            },
        )
        .expect("提取成功");

        assert_eq!(result.total, 2);
        assert_eq!(result.found, 1);
        assert_eq!(result.not_found, 1);
        // 保留相对路径，而不是全塞进目标目录根下。
        assert!(dst.join("base/ifm/A.java").is_file());
        let logs = recorder.logs.lock().expect("lock").clone();
        assert!(
            logs.iter()
                .any(|(level, message)| level == "warn" && message.contains("Missing.java")),
            "未找到的条目要留一条 warn 日志：{logs:?}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn empty_manifest_is_an_error_not_a_silent_success() {
        let root = temp_dir("empty");
        let manifest = root.join("list.txt");
        std::fs::write(&manifest, "# 只有注释\n\n").expect("写清单");
        let error = extract(
            &Recorder::default(),
            &ExtractOptions {
                manifest_path: manifest.to_string_lossy().to_string(),
                src_dir: root.to_string_lossy().to_string(),
                dst_dir: root.join("dst").to_string_lossy().to_string(),
            },
        )
        .expect_err("空清单应当报错");
        assert_eq!(error, "清单文件中没有有效的文件条目");
        std::fs::remove_dir_all(root).ok();
    }
}
