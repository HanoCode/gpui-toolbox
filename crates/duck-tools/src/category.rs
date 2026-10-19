//! 工具分类：决定侧栏分组、排序与图标。

use duck_components::DuckIcon;

/// 工具分类。
///
/// 用枚举而非字符串：注册时写错分类会编译失败，而不是运行时归进「其他」。
///
/// **分类名照抄参考项目** `ifm_ai_tools/src/composables/tools.ts` 的 `category`
/// 字段（老用户从参考项目迁过来，侧栏分组必须同名同序，不能有学习成本）。
/// 参考项目有 15 个分类，这里只落**当前插件已有的**那些；没工具的分类不注册
/// （`ToolCategory::ALL` 的遍历会把它过滤掉，加进来只会是死代码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCategory {
    /// 代码开发（格式化、正则、SQL 检查/改写）。
    Development,
    /// 数据管理（DDL 生成、CSV / 表结构类）。
    Data,
    /// 网络（网盘直链、请求回放等）。
    Network,
    /// 代码比对（class / jar / 目录 / 文本）。
    Compare,
    /// 上线合并（各类包合并、清单提取）。
    Merge,
    /// 效率工具（Cron、时间戳、命名转换、随机数据、SQL 分类抽取）。
    Productivity,
    /// 文件工具（加解密、编码转换、批量改写、搜索、目录树、文档转换）。
    Files,
    /// 帮助中心（速查手册）。
    Help,
    /// 媒体处理（视频 / 音频 / 图片）。
    Media,
    /// 文档转换。
    Document,
    /// 压缩归档。
    Archive,
    /// 系统能力。
    System,
    /// 未归类。
    Other,
}

impl ToolCategory {
    /// 全部分类；**数组顺序即侧栏顺序**。
    ///
    /// 前八个是参考项目的原始顺序（代码开发 → 数据管理 → 网络调试 → 代码比对 →
    /// 上线合并 → 效率工具 → 文件工具 → 帮助中心），后面五个是宿主原有的
    /// 内置工具分类，排在迁移工具之后，不抢首屏。
    pub const ALL: &'static [ToolCategory] = &[
        Self::Development,
        Self::Data,
        Self::Network,
        Self::Compare,
        Self::Merge,
        Self::Productivity,
        Self::Files,
        Self::Help,
        Self::Media,
        Self::Document,
        Self::Archive,
        Self::System,
        Self::Other,
    ];

    /// 稳定 id：用于 `ElementId` 与插件清单的 `category` 字段，不随语言变化。
    pub const fn id(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Data => "data",
            Self::Network => "network",
            Self::Compare => "compare",
            Self::Merge => "merge",
            Self::Productivity => "productivity",
            Self::Files => "files",
            Self::Help => "help",
            Self::Media => "media",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::System => "system",
            Self::Other => "other",
        }
    }

    /// 展示名（中文源串，渲染时经 `tr`）；与参考项目 `category` 逐字一致。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Development => "代码开发",
            Self::Data => "数据管理",
            Self::Network => "网络调试",
            Self::Compare => "代码比对",
            Self::Merge => "上线合并",
            Self::Productivity => "效率工具",
            Self::Files => "文件工具",
            Self::Help => "帮助中心",
            Self::Media => "媒体处理",
            Self::Document => "文档转换",
            Self::Archive => "压缩归档",
            Self::System => "系统",
            Self::Other => "其他",
        }
    }

    /// 分类图标（参考项目用 lucide 名，这里取语义最接近的 Duck 图标）。
    pub const fn icon(self) -> DuckIcon {
        match self {
            Self::Development => DuckIcon::Code,
            Self::Data => DuckIcon::Database,
            Self::Network => DuckIcon::Globe,
            // 双栏：比对结果的形态就是左右对照。
            Self::Compare => DuckIcon::Columns2,
            // 多层：多层包叠成一份。
            Self::Merge => DuckIcon::Layers,
            Self::Productivity => DuckIcon::Zap,
            Self::Files => DuckIcon::Folders,
            Self::Help => DuckIcon::Library,
            Self::Media => DuckIcon::Film,
            Self::Document => DuckIcon::FileText,
            Self::Archive => DuckIcon::Archive,
            Self::System => DuckIcon::HardDrive,
            Self::Other => DuckIcon::Box,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ToolCategory;

    #[test]
    fn ids_are_unique_and_stable() {
        let ids: Vec<&str> = ToolCategory::ALL.iter().map(|c| c.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "分类 id 必须唯一");
    }

    #[test]
    fn every_category_has_a_label() {
        for category in ToolCategory::ALL {
            assert!(!category.label().is_empty(), "{:?} 缺展示名", category.id());
        }
    }
}
