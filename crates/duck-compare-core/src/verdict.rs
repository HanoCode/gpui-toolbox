//! 一次比对的「判决」：两边谁新谁旧、谁少了。
//!
//! 提成 crate 顶层而不是跟着 class 比对走，是因为**指向同一个概念**：class 比对、
//! jar 比对、将来的配置比对说的都是这五件事。共用一份的好处是插件层只写一份文案
//! 与颜色映射，而不是每个工具抄一遍、抄错一个就成了「界面说新增、判定说回退」。

use serde::{Deserialize, Serialize};

/// 比对结论。
///
/// 序列化后就是参考仓用的字符串字面量（`"update"` / `"same"` / `"rollback"` /
/// `"new"` / `"missing"`），落 JSON 时两边可逐字对照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompareVerdict {
    /// 基准侧（新版本）更高 —— 正常升级。
    Update,
    /// 两边一样。
    Same,
    /// 基准侧**更低** —— 版本回退，这是唯一一种「异常」。
    Rollback,
    /// 只在基准侧有 —— 本次新增。
    New,
    /// 只在对比侧有 —— 基准侧缺失（jar 比对的 `missing`；class 比对按源仓口径
    /// 不产生这一种，只在对比环境存在的类不进结果）。
    Missing,
}

impl CompareVerdict {
    /// 与参考仓的字面量一致。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Same => "same",
            Self::Rollback => "rollback",
            Self::New => "new",
            Self::Missing => "missing",
        }
    }

    /// 是否异常。口径照抄源仓：**只有版本回退算异常**，新增/缺失/升级/相同都不算。
    pub fn is_abnormal(self) -> bool {
        matches!(self, Self::Rollback)
    }
}
