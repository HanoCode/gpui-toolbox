//! composition root 的资源源：把组件库图标并入 gpui-component 自带资源。

use std::borrow::Cow;

use duck_components::ComponentAssets;
use gpui::{AssetSource, Result, SharedString};

/// 应用唯一的 [`AssetSource`]。
pub struct DesktopAssets;

impl AssetSource for DesktopAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(asset) = ComponentAssets.load(path)? {
            return Ok(Some(asset));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut assets = gpui_component_assets::Assets.list(path)?;
        assets.extend(ComponentAssets.list(path)?);
        Ok(assets)
    }
}

#[cfg(test)]
mod tests {
    use duck_components::DuckIcon;
    use gpui::AssetSource as _;

    use super::DesktopAssets;

    #[test]
    fn desktop_assets_serve_component_icons() -> gpui::Result<()> {
        for icon in DuckIcon::ALL {
            assert!(
                DesktopAssets.load(icon.asset_path())?.is_some(),
                "{icon:?} not reachable through DesktopAssets"
            );
        }
        Ok(())
    }
}
