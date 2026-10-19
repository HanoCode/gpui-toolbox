//! JAR 配对与判定。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/jar_compare/compare_engine.rs`。
//! 与源仓一致的两点：配对键是**相对路径**（不是文件名）；只在对比侧存在的 JAR 也进
//! 结果（判 `Missing`），这一点与 class 比对相反——class 比对会丢掉「只在对比侧」的类
//! （它们本来就在对比环境里，不属于本轮上线内容），而 JAR 侧少了就是少了，必须报。

use std::collections::{HashMap, HashSet};

use duck_classfile::version_compare;

use super::models::{JarCompareResult, JarInfo};
use crate::CompareVerdict;

pub struct JarCompareEngine {
    pub base_jars: Vec<JarInfo>,
    pub target_jars: Vec<JarInfo>,
}

impl JarCompareEngine {
    pub fn execute(&self) -> Vec<JarCompareResult> {
        let target_map: HashMap<&String, &JarInfo> = self
            .target_jars
            .iter()
            .map(|info| (&info.jar_rel_path, info))
            .collect();

        let mut results = Vec::new();
        for base in &self.base_jars {
            let target = target_map.get(&base.jar_rel_path).copied();
            results.push(self.compare_jar_pair(base, target));
        }

        let base_rel_paths: HashSet<&String> = self
            .base_jars
            .iter()
            .map(|info| &info.jar_rel_path)
            .collect();
        for target in &self.target_jars {
            if !base_rel_paths.contains(&target.jar_rel_path) {
                results.push(Self::missing_in_base(target));
            }
        }
        results
    }

    fn compare_jar_pair(&self, base: &JarInfo, target: Option<&JarInfo>) -> JarCompareResult {
        let mut result = JarCompareResult {
            jar_name: base.jar_rel_path.clone(),
            server_name: base.server_name.clone(),
            jar_path_base: base.jar_path.clone(),
            jar_path_target: String::new(),
            class_count_base: base.classes.len(),
            class_count_target: 0,
            version_base: base.jar_version.clone(),
            version_target: String::new(),
            compare_type: CompareVerdict::New,
            is_abnormal: false,
        };

        let Some(target) = target else {
            return result;
        };

        result.jar_path_target = target.jar_path.clone();
        result.class_count_target = target.classes.len();
        result.version_target = target.jar_version.clone();

        let ordering = version_compare::compare(&base.jar_version, &target.jar_version);
        if ordering > 0 {
            result.compare_type = CompareVerdict::Update;
        } else if ordering == 0 {
            result.compare_type = CompareVerdict::Same;
        } else {
            result.compare_type = CompareVerdict::Rollback;
            result.is_abnormal = true;
        }
        result
    }

    fn missing_in_base(target: &JarInfo) -> JarCompareResult {
        JarCompareResult {
            jar_name: target.jar_rel_path.clone(),
            server_name: target.server_name.clone(),
            jar_path_base: String::new(),
            jar_path_target: target.jar_path.clone(),
            class_count_base: 0,
            class_count_target: target.classes.len(),
            version_base: String::new(),
            version_target: target.jar_version.clone(),
            compare_type: CompareVerdict::Missing,
            is_abnormal: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jar(rel_path: &str, version: &str, class_count: usize) -> JarInfo {
        JarInfo {
            env_name: "env".to_string(),
            jar_name: rel_path.rsplit('/').next().unwrap_or(rel_path).to_string(),
            jar_path: format!("/base/{rel_path}"),
            jar_rel_path: rel_path.to_string(),
            server_name: "lib".to_string(),
            file_size: 1,
            classes: Vec::new(),
            jar_version: version.to_string(),
        }
        .tap_classes(class_count)
    }

    /// 只为了带上 class 数量，`classes` 的内容不参与判定。
    impl JarInfo {
        fn tap_classes(mut self, count: usize) -> Self {
            self.classes = (0..count)
                .map(|index| crate::jar_compare::models::JarClassInfo {
                    class_path: format!("com.demo.C{index}"),
                    version: self.jar_version.clone(),
                    class_name: format!("C{index}"),
                    package_name: "com.demo".to_string(),
                })
                .collect();
            self
        }
    }

    /// 配对键是相对路径：同名 JAR 在不同服务下互不相干。
    #[test]
    fn matches_by_relative_path_not_file_name() {
        let engine = JarCompareEngine {
            base_jars: vec![
                jar("server/lcpt-a/lib/foo.jar", "1.2.0.4", 3),
                jar("server/lcpt-b/lib/foo.jar", "1.2.0.3", 3),
            ],
            target_jars: vec![
                jar("server/lcpt-a/lib/foo.jar", "1.2.0.3", 2),
                jar("server/lcpt-b/lib/foo.jar", "1.2.0.3", 3),
            ],
        };
        let results = engine.execute();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].compare_type, CompareVerdict::Update);
        assert_eq!(results[0].class_count_target, 2);
        assert_eq!(results[1].compare_type, CompareVerdict::Same);
    }

    /// 只在对比侧存在的 JAR 判 `Missing`（与 class 比对相反：JAR 少了必须报）。
    #[test]
    fn target_only_jar_is_missing() {
        let engine = JarCompareEngine {
            base_jars: Vec::new(),
            target_jars: vec![jar("lib/gone.jar", "9.9.9", 1)],
        };
        let results = engine.execute();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].compare_type, CompareVerdict::Missing);
        assert!(results[0].jar_path_base.is_empty());
        assert!(!results[0].is_abnormal);
    }

    /// 基准侧版本更低 = 回退，也是唯一一种异常。
    #[test]
    fn rollback_is_the_only_abnormal_verdict() {
        let engine = JarCompareEngine {
            base_jars: vec![jar("lib/foo.jar", "1.2.0.3", 1)],
            target_jars: vec![jar("lib/foo.jar", "1.2.0.4", 1)],
        };
        let results = engine.execute();
        assert_eq!(results[0].compare_type, CompareVerdict::Rollback);
        assert!(results[0].is_abnormal);
    }
}
