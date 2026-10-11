//! Duck 自带图标集。
//!
//! 内嵌 Lucide 1.48.0 的子集（ISC，见 `assets/icons/LICENSE`），线宽统一 1.75，
//! 经 [`DuckIcon`] 与 gpui-component 的 `Icon` / `Button::icon` / `PopupMenuItem::icon`
//! 无缝互用。资源由 [`ComponentAssets`] 提供，composition root 负责并入应用 `AssetSource`。

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_component::IconNamed;

macro_rules! duck_icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        /// Duck 自带图标。
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum DuckIcon {
            $($variant,)*
        }

        impl DuckIcon {
            /// 全部图标（资源枚举与测试用）。
            pub const ALL: &'static [DuckIcon] = &[$(DuckIcon::$variant,)*];

            /// 嵌入资源路径。
            pub const fn asset_path(self) -> &'static str {
                match self {
                    $(Self::$variant => concat!("duck/icons/", $file, ".svg"),)*
                }
            }

            const fn bytes(self) -> &'static [u8] {
                match self {
                    $(Self::$variant => include_bytes!(concat!("../assets/icons/", $file, ".svg")),)*
                }
            }
        }
    };
}

duck_icons! {
    AppWindow => "app-window",
    Archive => "archive",
    ArrowDown => "arrow-down",
    ArrowUp => "arrow-up",
    Bell => "bell",
    Bot => "bot",
    Box => "box",
    Check => "check",
    CheckboxCheck => "checkbox-check",
    CheckboxMinus => "checkbox-minus",
    ChevronDown => "chevron-down",
    ChevronLeft => "chevron-left",
    ChevronRight => "chevron-right",
    CircleAlert => "circle-alert",
    CircleCheck => "circle-check",
    Clipboard => "clipboard",
    Clock => "clock",
    Code => "code",
    Copy => "copy",
    Database => "database",
    Download => "download",
    Ellipsis => "ellipsis",
    ExternalLink => "external-link",
    File => "file",
    FileText => "file-text",
    Film => "film",
    FolderOpen => "folder-open",
    Folders => "folders",
    Gauge => "gauge",
    Globe => "globe",
    HardDrive => "hard-drive",
    Image => "image",
    Info => "info",
    Kanban => "kanban",
    Layers => "layers",
    Library => "library",
    ListFilter => "list-filter",
    Moon => "moon",
    PanelRight => "panel-right",
    Palette => "palette",
    Pause => "pause",
    Play => "play",
    Plus => "plus",
    Power => "power",
    RotateCw => "rotate-cw",
    Search => "search",
    Settings => "settings",
    SlidersHorizontal => "sliders-horizontal",
    SquareTerminal => "square-terminal",
    Sun => "sun",
    Trash2 => "trash-2",
    TriangleAlert => "triangle-alert",
    User => "user",
    X => "x",
    Zap => "zap",
}

impl IconNamed for DuckIcon {
    fn path(self) -> SharedString {
        SharedString::new_static(self.asset_path())
    }
}

/// 应用 logo 的资源路径（非图标：PNG 位图，给「关于」页居中展示用）。
///
/// 与 `assets/AppIcon.icns` / `assets/duck.ico` 同源（都由 `assets/icon-1024.png`
/// 降采样），换 logo 时三处一起换，见 `sbin/` 的打包脚本注释。
pub const LOGO_ASSET_PATH: &str = "duck/logo.png";

/// 组件 crate 一并提供的**非图标**嵌入资源（路径 → 字节）。
///
/// 图标走 [`DuckIcon::ALL`]；这里只放位图等整块资源，避免把 PNG 塞进图标枚举
/// （图标测试断言每个条目都是 SVG）。
const EXTRA_ASSETS: &[(&str, &[u8])] = &[(LOGO_ASSET_PATH, include_bytes!("../assets/logo.png"))];

/// 组件 crate 拥有的嵌入资源（[`DuckIcon`] 全集 + [`EXTRA_ASSETS`]）。
pub struct ComponentAssets;

impl AssetSource for ComponentAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = EXTRA_ASSETS.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        Ok(DuckIcon::ALL
            .iter()
            .find(|icon| icon.asset_path() == path)
            .map(|icon| Cow::Borrowed(icon.bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(EXTRA_ASSETS
            .iter()
            .map(|(name, _)| *name)
            .chain(DuckIcon::ALL.iter().map(|icon| icon.asset_path()))
            .filter(|asset| asset.starts_with(path))
            .map(SharedString::new_static)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use gpui::AssetSource as _;

    use super::{ComponentAssets, DuckIcon, LOGO_ASSET_PATH};

    #[test]
    fn every_icon_path_is_unique_and_loads_svg() -> gpui::Result<()> {
        let mut seen = HashSet::new();
        for icon in DuckIcon::ALL {
            assert!(seen.insert(icon.asset_path()), "duplicate {icon:?}");
            let bytes = ComponentAssets.load(icon.asset_path())?;
            assert!(
                bytes.is_some_and(|bytes| bytes.starts_with(b"<svg")),
                "{icon:?} missing or not an svg"
            );
        }
        assert!(ComponentAssets.load("duck/icons/nope.svg")?.is_none());
        Ok(())
    }

    #[test]
    fn logo_asset_loads_as_png_and_is_listed() -> gpui::Result<()> {
        let bytes = ComponentAssets
            .load(LOGO_ASSET_PATH)?
            .expect("logo 必须能从资源源取到");
        assert!(bytes.starts_with(b"\x89PNG"), "logo 应当是 PNG 位图");
        assert!(
            ComponentAssets
                .list("duck/")?
                .iter()
                .any(|asset| asset.as_ref() == LOGO_ASSET_PATH)
        );
        Ok(())
    }
}
