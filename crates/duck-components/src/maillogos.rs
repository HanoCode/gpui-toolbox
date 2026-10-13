//! 邮箱品牌 logo（全彩 SVG，与 `dblogos` 同一套机制）。
//!
//! 品牌标志有固有色（Gmail 红、iCloud 蓝…），**不能**走 `DuckIcon` 的单色
//! `IconNamed` 通道（会被 `currentColor` 渲染成主题前景色）；这里走
//! `gpui::img(Resource::Embedded(path))`——`svg_renderer` 渲染时保留 SVG 内的
//! `fill` 颜色，品牌色随资源走。
//!
//! # 来源与许可
//!
//! - Gmail / QQ / iCloud：Simple Icons（CC0，路径 `assets/maillogos/`），
//!   单色品牌字形 + 官方品牌色填充。
//! - Outlook / Yahoo：selfh.st 图标集（MIT，经 Iconify 分发）。
//! - 网易 163 / 126 与「其他邮箱」：Simple Icons 已无对应品牌标（微软系与
//!   雅虎系商标曾因品牌方要求整体下架，网易从未收录），这里手绘**品牌色
//!   瓦片 + 白色信封**补位——不冒充官方 logo，但与卡片网格里的全彩图同重。
//!
//! 品牌商标归各自权利人所有，本仓库只做「指代该服务商」的引用性使用。
//!
//! 资源由 [`ComponentAssets`] 一并并入应用 `AssetSource`（同 `dblogos` 的路径）。

use gpui::{IntoElement, Pixels, Styled as _, img};

/// 一家邮箱服务商的品牌 logo。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MailLogo {
    Custom,
    Gmail,
    Icloud,
    Netease126,
    Netease163,
    Outlook,
    Qq,
    Yahoo,
}

impl MailLogo {
    /// 全部 logo（资源枚举与测试用）。
    pub const ALL: &'static [MailLogo] = &[
        Self::Custom,
        Self::Gmail,
        Self::Icloud,
        Self::Netease126,
        Self::Netease163,
        Self::Outlook,
        Self::Qq,
        Self::Yahoo,
    ];

    /// 嵌入资源路径。
    pub const fn asset_path(self) -> &'static str {
        match self {
            Self::Custom => "duck/maillogos/custom.svg",
            Self::Gmail => "duck/maillogos/gmail.svg",
            Self::Icloud => "duck/maillogos/icloud.svg",
            Self::Netease126 => "duck/maillogos/netease126.svg",
            Self::Netease163 => "duck/maillogos/netease163.svg",
            Self::Outlook => "duck/maillogos/outlook.svg",
            Self::Qq => "duck/maillogos/qq.svg",
            Self::Yahoo => "duck/maillogos/yahoo.svg",
        }
    }

    pub(crate) const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Custom => include_bytes!("../assets/maillogos/custom.svg"),
            Self::Gmail => include_bytes!("../assets/maillogos/gmail.svg"),
            Self::Icloud => include_bytes!("../assets/maillogos/icloud.svg"),
            Self::Netease126 => include_bytes!("../assets/maillogos/netease126.svg"),
            Self::Netease163 => include_bytes!("../assets/maillogos/netease163.svg"),
            Self::Outlook => include_bytes!("../assets/maillogos/outlook.svg"),
            Self::Qq => include_bytes!("../assets/maillogos/qq.svg"),
            Self::Yahoo => include_bytes!("../assets/maillogos/yahoo.svg"),
        }
    }

    /// 渲染为 `size` 见方的全彩品牌图（选择卡 / 步骤条用）。
    #[must_use]
    pub fn element(self, size: Pixels) -> impl IntoElement {
        img(self.asset_path()).size(size)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use gpui::AssetSource as _;

    use crate::ComponentAssets;
    use crate::maillogos::MailLogo;

    #[test]
    fn every_logo_path_is_unique_and_loads_svg() -> gpui::Result<()> {
        let mut seen = HashSet::new();
        for logo in MailLogo::ALL {
            assert!(seen.insert(logo.asset_path()), "duplicate {logo:?}");
            let bytes = ComponentAssets.load(logo.asset_path())?;
            assert!(
                bytes
                    .is_some_and(|bytes| bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml")),
                "{logo:?} missing or not an svg"
            );
        }
        assert!(ComponentAssets.load("duck/maillogos/nope.svg")?.is_none());
        Ok(())
    }

    #[test]
    fn logos_are_listed_under_duck_maillogos() -> gpui::Result<()> {
        let listed = ComponentAssets.list("duck/maillogos/")?;
        assert_eq!(listed.len(), MailLogo::ALL.len());
        Ok(())
    }
}
