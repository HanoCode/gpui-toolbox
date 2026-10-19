//! class 比对的数据结构。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/class_compare/models.rs`，
//! 差异只有一处（见 [`CompareResult`] 的文档）：源仓的 `diff_desc` 是**界面文案**，
//! 这里挪到了插件层——中间层不该产出要翻译的句子。

use serde::{Deserialize, Serialize};

use crate::CompareVerdict;

/// 一个类的指纹：Phase 2 扫出来的东西。
///
/// `version` 有两种来源（源仓口径，见 [`super::scanner::Scanner::scan_entries`]）：
/// 普通 `.class` 从字节码里抽 `getVersion()` 的返回串；`WEB-INF/lib/*.jar` 这种
/// JAR 级条目拿不到字节码，用 `jar:<大小>:<修改时间>` 当指纹。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassEntry {
    /// 配对键：`服务名#包名#类名`。
    pub key: String,
    pub env_name: String,
    pub server_name: String,
    pub class_name: String,
    pub package_name: String,
    pub class_path: String,
    pub file_path: String,
    /// `true` = 这一条代表整个 JAR（`WEB-INF/lib` 下的包），不是单个类。
    pub is_jar_level: bool,
}

/// Phase 2 的产物：带版本的类条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassInfo {
    pub env_name: String,
    pub server_name: String,
    pub class_name: String,
    pub package_name: String,
    pub class_path: String,
    pub file_path: String,
    pub version: String,
}

/// 一条比对结论。
///
/// 与源仓的差别：**没有** `diff_desc` / `diff_percent` / `diff_lines` / `engine_used`。
///
/// - `diff_percent` / `diff_lines` / `engine_used` 属于「反编译两边的源码再比」那一步，
///   它要 `java -jar cfr.jar`，插件进程没有 JRE 也没有 jar（见 [`super`] 的模块头）；
/// - `diff_desc` 是一句给人看的话（`【基准】版本 1.0 → 2.0，正常升级(差异2级)`）。
///   内核若把它生成成中文句子，i18n 就只能靠「反查模板」这种脆弱手段，改一个字就失配。
///   现在内核只给结构化字段（[`CompareType`] + 两个版本号 + [`Self::diff_version_num`]），
///   由插件层用 `locales/*.toml` 的模板拼句子。
///
/// 判定结果本身（`compare_type` / `is_abnormal` / `diff_version_num`）与源仓一致。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompareResult {
    pub server_name: String,
    pub class_name: String,
    pub package_name: String,
    pub class_path: String,
    pub file_path_base: String,
    pub file_path_target: String,
    pub version_base: String,
    pub version_target: String,
    pub compare_type: CompareVerdict,
    /// 「异常」口径照抄源仓：只有**版本回退**算异常，新增/升级/相同都不算。
    pub is_abnormal: bool,
    /// 版本差几级（`compare` 风格的整数差，见 `duck_classfile::version_compare`）。
    pub diff_version_num: i32,
}

/// 一次扫描比对的全部产出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassCompareReport {
    pub results: Vec<CompareResult>,
    /// 基准侧清单里的类（条目，不是去重后的键）总数。
    pub base_listed: usize,
    /// 对比侧清单总数。
    pub target_listed: usize,
    /// 两边都有的键数 —— 只有这部分才做版本提取（Phase 2）。
    pub compared: usize,
}

impl ClassCompareReport {
    /// 各结论的条数，供报告头与「异常」计数用。
    pub fn count(&self, verdict: CompareVerdict) -> usize {
        self.results
            .iter()
            .filter(|result| result.compare_type == verdict)
            .count()
    }

    /// 异常条数（版本回退）。
    pub fn abnormal(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.is_abnormal)
            .count()
    }
}
