//! 配对与判定：两边同一个类，版本谁高谁低。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/class_compare/compare_engine.rs`。
//! 判定逻辑一行未改，只做了两件与宿主无关的整理：
//!
//! 1. `compare_type` 从 `String` 换成 [`CompareType`]（四个方面分支编译期收口）；
//! 2. 不生成 `diff_desc`（要翻译的句子归插件层，理由见 [`super::models::CompareResult`]）。
//!
//! 判定口径照抄：`base.version` 与 `target.version` 用
//! `duck_classfile::version_compare::compare` 比（返回 `>0` / `0` / `<0`），
//! **大于**是正常升级、等于相同、小于算回退（异常）。

use std::collections::HashMap;

use duck_classfile::version_compare;

use super::models::{ClassInfo, CompareResult};
use crate::CompareVerdict;

pub struct CompareEngine {
    pub base_list: Vec<ClassInfo>,
    pub target_list: Vec<ClassInfo>,
}

impl CompareEngine {
    /// 对**交集**逐条判定。只存在于基准侧的条目由调用方补 `New`（源仓同此分工）。
    pub fn execute(&self) -> Vec<CompareResult> {
        let target_map: HashMap<String, &ClassInfo> = self
            .target_list
            .iter()
            .map(|info| (Self::build_key(info), info))
            .collect();

        let mut results = Vec::new();
        for base in &self.base_list {
            let key = Self::build_key(base);
            let target = target_map.get(&key).copied();
            results.push(self.compare_pair(base, target));
        }
        results
    }

    /// 配对键与 [`super::scanner::Scanner`] 的 `ClassEntry::key` 同构。
    fn build_key(info: &ClassInfo) -> String {
        format!(
            "{}#{}#{}",
            info.server_name, info.package_name, info.class_name
        )
    }

    fn compare_pair(&self, base: &ClassInfo, target: Option<&ClassInfo>) -> CompareResult {
        let mut result = CompareResult {
            server_name: base.server_name.clone(),
            class_name: base.class_name.clone(),
            package_name: base.package_name.clone(),
            class_path: base.class_path.clone(),
            file_path_base: base.file_path.clone(),
            file_path_target: String::new(),
            version_base: base.version.clone(),
            version_target: String::new(),
            compare_type: CompareVerdict::New,
            is_abnormal: false,
            diff_version_num: 0,
        };

        let Some(target) = target else {
            // 调用方一般已把「基准独有」提前摘出去了，这里仍留一条兜底。
            return result;
        };

        result.file_path_target = target.file_path.clone();
        result.version_target = target.version.clone();
        let ordering = version_compare::compare(&base.version, &target.version);
        result.diff_version_num = version_compare::diff_num(&base.version, &target.version);

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
}
