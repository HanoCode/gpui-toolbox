//! 工具分类：决定侧栏分组、排序与图标。

use duck_components::DuckIcon;

/// 工具分类。
///
/// 用枚举而非字符串：注册时写错分类会编译失败，而不是运行时归进「其他」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCategory {
    /// 媒体处理（视频 / 音频 / 图片）。
    Media,
    /// 文档转换。
    Document,
    /// 压缩归档。
    Archive,
    /// 网络（网盘直链、请求回放等）。
    Network,
    /// 开发工具。
    Development,
    /// 系统能力。
    System,
    /// 未归类。
    Other,
}

impl ToolCategory {
    /// 全部分类；**数组顺序即侧栏顺序**。
    pub const ALL: &'static [ToolCategory] = &[
        Self::Media,
        Self::Document,
        Self::Archive,
        Self::Network,
        Self::Development,
        Self::System,
        Self::Other,
    ];

    /// 稳定 id：用于 `ElementId`，不随语言变化。
    pub const fn id(self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::Network => "network",
            Self::Development => "development",
            Self::System => "system",
            Self::Other => "other",
        }
    }

    /// 展示名（中文源串，渲染时经 `tr`）。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Media => "媒体处理",
            Self::Document => "文档转换",
            Self::Archive => "压缩归档",
            Self::Network => "网络",
            Self::Development => "开发工具",
            Self::System => "系统",
            Self::Other => "其他",
        }
    }

    /// 分类图标。
    pub const fn icon(self) -> DuckIcon {
        match self {
            Self::Media => DuckIcon::Film,
            Self::Document => DuckIcon::FileText,
            Self::Archive => DuckIcon::Archive,
            Self::Network => DuckIcon::Globe,
            Self::Development => DuckIcon::Code,
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
