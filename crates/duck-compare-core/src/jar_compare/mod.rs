//! jar 比对：两个应用目录里同名 JAR 的版本高低。
//!
//! 来源与 class 比对同一族（参考仓 `ifm_ai_tools/src-tauri/src/jar_compare/`），
//! [`models`] / [`scanner`] / [`engine`] 逐字复刻。本文件是源仓 `run_compare_jars`
//! 的编排部分，剥掉的东西与 [`super::class_compare`] 的模块头一致（emit / store /
//! 历史记录）：插件进程一次调用一次进程，结果直接交给宿主。
//!
//! 与 class 比对的两处**实质差异**（源仓如此，不是我们改的）：
//!
//! 1. 扫描范围由 `compare_lib` 反过来决定：`true` = 全扫，`false` = 只看
//!    `lcpt*` 服务目录 + `applib`。参数名叫 `compare_lib` 但语义是「范围放开」，
//!    容易读错，这里保留了原名以免与源仓对不上，另加 [`JarCompareSpec::scan_all`]
//!    这个别名式注释。
//! 2. 批大小下限是 5（class 比对是 50）：一个 JAR 要开 ZIP 读几百个 class，
//!    单批就慢，批小一点进度才动得起来。

pub mod engine;
pub mod models;
pub mod scanner;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub use crate::progress::{Progress, Stage};
use engine::JarCompareEngine;
pub use models::{JarClassInfo, JarCompareReport, JarCompareResult, JarInfo};
use scanner::{JarEntry, JarScanner};

use crate::progress::scan_in_batches;

/// 一次 jar 比对的输入。
#[derive(Debug, Clone)]
pub struct JarCompareSpec {
    pub base_env: String,
    pub target_env: String,
    pub base_dir: PathBuf,
    pub target_dir: PathBuf,
    /// 名字沿用源仓；语义是「放开扫描范围」：`true` 扫全部 JAR，`false` 只扫
    /// `lcpt*` 服务目录与 `applib`。
    pub scan_all: bool,
    /// 跳过的 JAR 名，支持通配。
    pub jar_exclude: Vec<String>,
}

impl JarCompareSpec {
    /// 源仓那套 `compare_lib` → `service_pattern` 的换算。
    fn service_pattern(&self) -> &'static str {
        if self.scan_all { "" } else { "lcpt+applib" }
    }
}

/// 扫两边、比 JAR 版本。
///
/// # Errors
///
/// 目录不存在或不是目录时返回中文错误串（复刻件原样，插件层翻）。
pub fn compare_jars(
    spec: &JarCompareSpec,
    progress: &mut dyn FnMut(Progress),
) -> Result<JarCompareReport, String> {
    for dir in [&spec.base_dir, &spec.target_dir] {
        if !dir.is_dir() {
            return Err("路径不是有效目录".to_string());
        }
    }

    let pattern = spec.service_pattern().to_string();
    let scanner_base = JarScanner::new(
        spec.base_env.clone(),
        spec.base_dir.clone(),
        pattern.clone(),
        spec.jar_exclude.clone(),
    );
    let scanner_target = JarScanner::new(
        spec.target_env.clone(),
        spec.target_dir.clone(),
        pattern,
        spec.jar_exclude.clone(),
    );

    // Phase 1：只列路径，不打开 ZIP。
    progress(Progress {
        percent: 0,
        stage: Stage::BaseListing,
    });
    let base_entries = scanner_base.list_jars();
    progress(Progress {
        percent: 15,
        stage: Stage::BaseListed {
            count: base_entries.len(),
        },
    });

    let target_entries = scanner_target.list_jars();
    progress(Progress {
        percent: 30,
        stage: Stage::TargetListed {
            count: target_entries.len(),
        },
    });

    // 只对交集开 ZIP：读一个 JAR 的全部 class 是重活，只在基准侧的没必要读
    // （它的结论必然是「新增」，清单已经足够）。
    let base_keys: HashSet<&String> = base_entries.iter().map(|entry| &entry.key).collect();
    let target_keys: HashSet<&String> = target_entries.iter().map(|entry| &entry.key).collect();
    let intersection: HashSet<&String> = base_keys.intersection(&target_keys).copied().collect();

    let base_intersection: Vec<JarEntry> = base_entries
        .iter()
        .filter(|entry| intersection.contains(&entry.key))
        .cloned()
        .collect();
    let target_intersection: Vec<JarEntry> = target_entries
        .iter()
        .filter(|entry| intersection.contains(&entry.key))
        .cloned()
        .collect();

    let base_batch = (base_intersection.len() / 10).max(5);
    let mut base_jars = scan_in_batches(
        &base_intersection,
        base_batch,
        30,
        50,
        |chunk| scanner_base.scan_jars(chunk),
        |batch, batches, scanned| Stage::BaseVersions {
            batch,
            batches,
            scanned,
        },
        progress,
    );

    let target_batch = (target_intersection.len() / 10).max(5);
    let target_jars = scan_in_batches(
        &target_intersection,
        target_batch,
        50,
        65,
        |chunk| scanner_target.scan_jars(chunk),
        |batch, batches, scanned| Stage::TargetVersions {
            batch,
            batches,
            scanned,
        },
        progress,
    );

    progress(Progress {
        percent: 90,
        stage: Stage::Comparing,
    });

    // 基准侧独有的 JAR：补一条「空壳」进基准列表（没有 class、没有版本），
    // 判定时它自然落成 `New`。源仓原话：只在对比侧的那些跳过——它们本来就在对比
    // 环境里，不属于本轮上线内容。
    let base_entry_map: HashMap<&String, &JarEntry> = base_entries
        .iter()
        .map(|entry| (&entry.key, entry))
        .collect();
    for key in base_keys.difference(&target_keys) {
        let entry = base_entry_map
            .get(key)
            .expect("key came from base_keys difference");
        base_jars.push(JarInfo {
            env_name: entry.env_name.clone(),
            jar_name: entry.jar_name.clone(),
            jar_path: entry.jar_path.to_string_lossy().to_string(),
            jar_rel_path: entry.jar_rel_path.clone(),
            server_name: entry.server_name.clone(),
            file_size: entry.file_size,
            classes: Vec::new(),
            jar_version: String::new(),
        });
    }

    let engine = JarCompareEngine {
        base_jars,
        target_jars,
    };
    Ok(JarCompareReport {
        results: engine.execute(),
        base_listed: base_entries.len(),
        target_listed: target_entries.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CompareVerdict;
    use crate::testkit::{fake_class, write_zip};

    fn temp_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-jar-compare-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    /// 造一个含一个 `.class` 的 JAR，版本号写在该 class 的常量池里。
    fn jar(root: &std::path::Path, rel: &str, version: &str) {
        let class = fake_class(version);
        write_zip(&root.join(rel), &[("com/demo/A.class", class.as_slice())]);
    }

    fn spec(base: &std::path::Path, target: &std::path::Path) -> JarCompareSpec {
        JarCompareSpec {
            base_env: "基准".to_string(),
            target_env: "对比".to_string(),
            base_dir: base.to_path_buf(),
            target_dir: target.to_path_buf(),
            scan_all: true,
            jar_exclude: Vec::new(),
        }
    }

    /// 三种结论：升级 / 相同 / 只在基准侧新增；只在对比侧的不进结果（源仓口径）。
    #[test]
    fn compares_jars_and_skips_target_only() {
        let root = temp_root("verdicts");
        let base = root.join("base");
        let target = root.join("target");
        jar(&base, "lib/up.jar", "1.2.4");
        jar(&target, "lib/up.jar", "1.2.3");
        jar(&base, "lib/same.jar", "1.0.0");
        jar(&target, "lib/same.jar", "1.0.0");
        jar(&base, "lib/new.jar", "2.0.0");
        jar(&target, "lib/gone.jar", "3.0.0");

        let report = compare_jars(&spec(&base, &target), &mut |_| {}).expect("compare");
        let by_name = |name: &str| {
            report
                .results
                .iter()
                .find(|result| result.jar_name == name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };

        assert_eq!(by_name("lib/up.jar").compare_type, CompareVerdict::Update);
        assert_eq!(by_name("lib/same.jar").compare_type, CompareVerdict::Same);
        let new = by_name("lib/new.jar");
        assert_eq!(new.compare_type, CompareVerdict::New);
        // 新增的 JAR 没开过包：没有 class 数、没有版本（源仓口径的「空壳」）。
        assert_eq!(new.class_count_base, 0);
        assert!(new.version_base.is_empty());

        assert!(
            !report
                .results
                .iter()
                .any(|result| result.jar_name == "lib/gone.jar")
        );
        assert_eq!(report.base_listed, 3);
        assert_eq!(report.target_listed, 3);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `scan_all = false` 只看 `lcpt*` 与 `applib`，`lib` 下的包被漏掉是预期行为。
    #[test]
    fn narrow_scope_skips_lib_directory() {
        let root = temp_root("scope");
        let base = root.join("base");
        let target = root.join("target");
        for dir in [&base, &target] {
            jar(dir, "lib/plain.jar", "1.0.0");
            jar(dir, "lcpt-a/lib/svc.jar", "1.0.0");
        }

        let mut narrow = spec(&base, &target);
        narrow.scan_all = false;
        let report = compare_jars(&narrow, &mut |_| {}).expect("compare");
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].jar_name, "lcpt-a/lib/svc.jar");
        let _ = std::fs::remove_dir_all(root);
    }

    /// 目录不存在要报错。
    #[test]
    fn rejects_missing_directory() {
        let root = temp_root("missing");
        let error = compare_jars(&spec(&root.join("nope"), &root), &mut |_| {}).expect_err("fail");
        assert_eq!(error, "路径不是有效目录");
        let _ = std::fs::remove_dir_all(root);
    }
}
