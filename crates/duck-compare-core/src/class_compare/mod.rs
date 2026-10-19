//! class 比对：两个应用目录里同名类的版本高低。
//!
//! 整族来自参考仓 `ifm_ai_tools/src-tauri/src/class_compare/`，三个文件各自逐字复刻
//! （[`models`] / [`scanner`] / [`engine`]）。本文件是源仓 `class_compare/mod.rs` 里
//! `run_scan_and_compare` 的**编排部分**，剥掉三样与 Tauri 强绑定的东西：
//!
//! | 源仓 | 这里 |
//! | --- | --- |
//! | `app.emit("scan_progress", …)` 推前端 | [`Progress`] 回调，由插件层转成 process 协议的 `progress` 事件 |
//! | `app.emit("compare_log", …)` 打日志 | 不要（进度里已含阶段与计数） |
//! | `CompareStore` 落 SQLite + 历史记录表 | 不要（见下） |
//!
//! **`store` 不迁**的理由：源仓那套表是为「扫描后台跑、前端分页查结果、隔天再看历史」
//! 设计的——扫描线程先把几万条结果灌进 SQLite，前端再 `get_compare_results` 分页读。
//! 本仓的插件进程是**一次调用一次进程**：扫完就要把结论交给宿主画出来，进程随即退出，
//! 落库既无读者也无下一次运行（历史归宿主层的 KV，见计划 W5）。所以结果直接在内存里
//! 交给调用方，少一层 schema 维护，也少一份「库里的旧结果和这次不一样」的困惑。
//!
//! 同样不迁的还有 `compute_source_diff` / `get_class_source`：它们要 `java -jar cfr.jar`
//! 反编译两边的 `.class` 再比源码。插件进程没有 JRE、也不该带一个 20 MB 的反编译器；
//! 「看两段文本差在哪」已经由 `duck.compare.text-diff` 覆盖。

pub mod engine;
pub mod models;
pub mod scanner;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub use crate::progress::{Progress, Stage};
use engine::CompareEngine;
pub use models::{ClassCompareReport, ClassEntry, ClassInfo, CompareResult};
use scanner::Scanner;

use crate::CompareVerdict;
use crate::progress::scan_in_batches;

/// 一次扫描的输入。
#[derive(Debug, Clone)]
pub struct CompareSpec {
    /// 基准环境名（只用于报告措辞，源仓也是这么用的）。
    pub base_env: String,
    pub target_env: String,
    pub base_dir: PathBuf,
    pub target_dir: PathBuf,
    /// 连 `WEB-INF/lib/*.jar` 一起比（按大小 + 修改时间当指纹）。
    pub compare_lib: bool,
    /// 跳过的 JAR 名，支持通配。
    pub jar_exclude: Vec<String>,
}

/// 扫两个目录、比版本，返回结论。
///
/// 阶段划分与源仓一致（清单 → 只对交集抽版本 → 判定），因为这一步的顺序决定了
/// 大环境下的耗时：两边的类可能有几万个，而交集往往只有几百。
///
/// # Errors
///
/// 目录不存在或不是目录时返回中文错误串（复刻件原样，插件层翻）。
pub fn scan_and_compare(
    spec: &CompareSpec,
    progress: &mut dyn FnMut(Progress),
) -> Result<ClassCompareReport, String> {
    for dir in [&spec.base_dir, &spec.target_dir] {
        if !dir.is_dir() {
            return Err("路径不是有效目录".to_string());
        }
    }

    let scanner_base = Scanner::new(
        spec.base_env.clone(),
        spec.base_dir.clone(),
        String::new(),
        spec.compare_lib,
        spec.jar_exclude.clone(),
    );
    let scanner_target = Scanner::new(
        spec.target_env.clone(),
        spec.target_dir.clone(),
        String::new(),
        spec.compare_lib,
        spec.jar_exclude.clone(),
    );

    // Phase 1：两边各列一遍清单（不读文件内容）。
    progress(Progress {
        percent: 0,
        stage: Stage::BaseListing,
    });
    let base_entries = scanner_base.list_paths();
    progress(Progress {
        percent: 15,
        stage: Stage::BaseListed {
            count: base_entries.len(),
        },
    });

    let target_entries = scanner_target.list_paths();
    progress(Progress {
        percent: 30,
        stage: Stage::TargetListed {
            count: target_entries.len(),
        },
    });

    // 只比交集：只在基准侧的算「新增」，只在对比侧的不进本次结果
    // （它们本来就在对比环境里，不属于这一轮要上线的内容——源仓原话）。
    let base_keys: HashSet<&String> = base_entries.iter().map(|entry| &entry.key).collect();
    let target_keys: HashSet<&String> = target_entries.iter().map(|entry| &entry.key).collect();
    let intersection: HashSet<&String> = base_keys.intersection(&target_keys).copied().collect();

    let base_intersection: Vec<ClassEntry> = base_entries
        .iter()
        .filter(|entry| intersection.contains(&entry.key))
        .cloned()
        .collect();
    let target_intersection: Vec<ClassEntry> = target_entries
        .iter()
        .filter(|entry| intersection.contains(&entry.key))
        .cloned()
        .collect();

    // Phase 2：只给交集抽版本。分批是为了能报进度；批大小照抄源仓（`len/10`，下限 50）。
    let base_batch = (base_intersection.len() / 10).max(50);
    let base_full = scan_in_batches(
        &base_intersection,
        base_batch,
        30,
        50,
        |chunk| scanner_base.scan_entries(chunk),
        |batch, batches, scanned| Stage::BaseVersions {
            batch,
            batches,
            scanned,
        },
        progress,
    );

    let target_batch = (target_intersection.len() / 10).max(50);
    let target_full = scan_in_batches(
        &target_intersection,
        target_batch,
        50,
        65,
        |chunk| scanner_target.scan_entries(chunk),
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

    let engine = CompareEngine {
        base_list: base_full,
        target_list: target_full,
    };
    let mut results = engine.execute();

    // 基准侧独有的类 = 「新增」。按清单顺序追加，保证同一份输入两次跑结果一致。
    let base_entry_map: HashMap<&String, &ClassEntry> = base_entries
        .iter()
        .map(|entry| (&entry.key, entry))
        .collect();
    for key in base_keys.difference(&target_keys) {
        let entry = base_entry_map
            .get(key)
            .expect("key came from base_keys difference");
        results.push(CompareResult {
            server_name: entry.server_name.clone(),
            class_name: entry.class_name.clone(),
            package_name: entry.package_name.clone(),
            class_path: entry.class_path.clone(),
            file_path_base: entry.file_path.clone(),
            file_path_target: String::new(),
            version_base: String::new(),
            version_target: String::new(),
            compare_type: CompareVerdict::New,
            is_abnormal: false,
            diff_version_num: 0,
        });
    }

    Ok(ClassCompareReport {
        results,
        base_listed: base_entries.len(),
        target_listed: target_entries.len(),
        compared: intersection.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::fake_class;

    fn write_class(path: &std::path::Path, version: &str) {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("create dirs");
        std::fs::write(path, fake_class(version)).expect("write class");
    }

    fn service_class(root: &std::path::Path, service: &str, class: &str, version: &str) {
        let mut path = root.join(service).join("WEB-INF/classes");
        for part in class.split('/') {
            path.push(part);
        }
        path.set_extension("class");
        write_class(&path, version);
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-class-compare-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn spec(base: &std::path::Path, target: &std::path::Path) -> CompareSpec {
        CompareSpec {
            base_env: "基准".to_string(),
            target_env: "对比".to_string(),
            base_dir: base.to_path_buf(),
            target_dir: target.to_path_buf(),
            compare_lib: false,
            jar_exclude: Vec::new(),
        }
    }

    /// 四种结论各来一条，外加「只在对比侧」的必须被丢掉。
    #[test]
    fn flags_upgrade_rollback_same_and_new() {
        let root = temp_root("verdicts");
        let base = root.join("base");
        let target = root.join("target");

        service_class(&base, "svc-a", "com/demo/Up", "1.2.4");
        service_class(&target, "svc-a", "com/demo/Up", "1.2.3");
        service_class(&base, "svc-a", "com/demo/Down", "1.2.3");
        service_class(&target, "svc-a", "com/demo/Down", "1.2.4");
        service_class(&base, "svc-a", "com/demo/Same", "1.0.0");
        service_class(&target, "svc-a", "com/demo/Same", "1.0.0");
        service_class(&base, "svc-a", "com/demo/OnlyBase", "3.0.0");
        service_class(&target, "svc-a", "com/demo/OnlyTarget", "3.0.0");

        let mut seen = Vec::new();
        let report = scan_and_compare(&spec(&base, &target), &mut |progress| {
            seen.push(progress.percent)
        })
        .expect("compare");

        let by_name = |name: &str| {
            report
                .results
                .iter()
                .find(|result| result.class_name == name)
                .unwrap_or_else(|| panic!("missing {name} in results"))
        };

        // 基准侧版本更高 = 正常升级（源仓口径：`base` 是新版本一侧）。
        let up = by_name("Up");
        assert_eq!(up.compare_type, CompareVerdict::Update);
        assert!(!up.is_abnormal);
        assert_eq!(up.version_base, "1.2.0.4");
        assert_eq!(up.version_target, "1.2.0.3");

        // 反过来就是回退，也是唯一的「异常」。
        let down = by_name("Down");
        assert_eq!(down.compare_type, CompareVerdict::Rollback);
        assert!(down.is_abnormal);

        assert_eq!(by_name("Same").compare_type, CompareVerdict::Same);

        // 只在基准侧 = 新增，且版本留空（源仓口径：没读到就没有版本）。
        let new = by_name("OnlyBase");
        assert_eq!(new.compare_type, CompareVerdict::New);
        assert!(new.version_base.is_empty());
        assert!(new.file_path_target.is_empty());

        // 只在对比侧的不进结果。
        assert!(
            !report
                .results
                .iter()
                .any(|result| result.class_name == "OnlyTarget")
        );

        assert_eq!(report.compared, 3);
        assert_eq!(report.count(CompareVerdict::New), 1);
        assert_eq!(report.abnormal(), 1);

        // 进度必须一路推到「判定中」，不能停在中途。
        assert!(seen.contains(&90), "progress stopped at {seen:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// 钉住「批次百分比越界就 `break`」那个源仓 bug：51 个类（批大小 50、2 批）
    /// 在源仓会只扫第一批，第二批整批丢掉。
    #[test]
    fn scans_every_batch_even_when_percent_would_overshoot() {
        let root = temp_root("batches");
        let base = root.join("base");
        let target = root.join("target");
        for index in 0..51 {
            let class = format!("com/demo/C{index:02}");
            service_class(&base, "svc-a", &class, "1.0.0");
            service_class(&target, "svc-a", &class, "1.0.0");
        }

        let report = scan_and_compare(&spec(&base, &target), &mut |_| {}).expect("compare");
        assert_eq!(report.compared, 51);
        assert_eq!(
            report.results.len(),
            51,
            "第二批被丢掉了（正是源仓 `if pct > 50 {{ break }}` 的症状）"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// 配对键含服务名：不同服务下的同名同类不算同一个东西。
    #[test]
    fn same_class_in_different_services_is_not_a_pair() {
        let root = temp_root("services");
        let base = root.join("base");
        let target = root.join("target");
        service_class(&base, "svc-a", "com/demo/Shared", "1.0.0");
        service_class(&target, "svc-b", "com/demo/Shared", "2.0.0");

        let report = scan_and_compare(&spec(&base, &target), &mut |_| {}).expect("compare");
        assert_eq!(report.compared, 0);
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].compare_type, CompareVerdict::New);
        let _ = std::fs::remove_dir_all(root);
    }

    /// 目录不存在要报错（不是静默给空结果）。
    #[test]
    fn rejects_missing_directory() {
        let root = temp_root("missing");
        let error =
            scan_and_compare(&spec(&root.join("nope"), &root), &mut |_| {}).expect_err("must fail");
        assert_eq!(error, "路径不是有效目录");
        let _ = std::fs::remove_dir_all(root);
    }
}
