//! 外观设置的持久化表示（wire）。
//!
//! [`Appearance`] 里带 `Hsla` 与 `SharedString`，都不适合直接序列化；本模块用一个纯数据的
//! [`AppearanceWire`] 做转换，落盘 / 读盘都走它。反序列化时所有字段都做了范围与合法性兜底，
//! 因此**旧版本写下的 JSON 或被人手改坏的 JSON 都不会让应用起不来**。

use gpui::{Hsla, SharedString};
use serde::{Deserialize, Serialize};

use crate::builtin::BuiltinThemeId;
use crate::i18n::Locale;
use crate::manager::{
    Appearance, Density, NAV_GAP_RANGE, SidebarSide, ThemePreference, ThemeSlots,
};
use crate::{FONT_SIZE_RANGE, ThemeMode};

/// 界面缩放允许范围（与 [`crate::manager`] 的文档一致）。
pub const UI_SCALE_RANGE: (f32, f32) = (0.8, 1.5);

/// 可序列化的外观快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppearanceWire {
    /// 明暗偏好。
    #[serde(default)]
    pub preference: ThemePreference,
    /// 强调色 `[h, s, l, a]`；缺失时回落到默认强调色。
    #[serde(default)]
    pub accent: Option<[f32; 4]>,
    /// 界面缩放。
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
    /// 正文字号（px）。
    #[serde(default)]
    pub font_size: Option<f32>,
    /// 正文字体族。
    #[serde(default)]
    pub font_family: Option<String>,
    /// 界面语言。
    #[serde(default)]
    pub language: Locale,
    /// 暗色槽位选中的内置主题 wire 名。
    #[serde(default)]
    pub dark_theme: Option<String>,
    /// 亮色槽位选中的内置主题 wire 名。
    #[serde(default)]
    pub light_theme: Option<String>,
    /// 界面密度档位 wire 名（`compact` / `normal` / `comfortable`）。
    #[serde(default)]
    pub density: Option<String>,
    /// 侧栏项间距（px）。
    #[serde(default)]
    pub nav_gap: Option<f32>,
    /// 是否显示活动栏；缺失按默认（显示）。
    #[serde(default)]
    pub show_activity_bar: Option<bool>,
    /// 是否显示通知铃铛；缺失按默认（显示）。
    #[serde(default)]
    pub show_notification_bell: Option<bool>,
    /// 是否显示顶栏附加入口；缺失按默认（显示）。
    #[serde(default)]
    pub show_top_bar_extras: Option<bool>,
    /// 右侧栏停靠侧 wire 名（`left` / `right`）。
    #[serde(default)]
    pub right_sidebar_side: Option<String>,
}

fn default_ui_scale() -> f32 {
    1.
}

impl Appearance {
    /// 转成可落盘的表示。
    #[must_use]
    pub fn to_wire(&self) -> AppearanceWire {
        AppearanceWire {
            preference: self.preference,
            accent: Some([self.accent.h, self.accent.s, self.accent.l, self.accent.a]),
            ui_scale: self.ui_scale,
            font_size: self.font_size,
            font_family: self.font_family.as_ref().map(ToString::to_string),
            language: self.language,
            dark_theme: Some(self.theme.dark.wire_name().to_owned()),
            light_theme: Some(self.theme.light.wire_name().to_owned()),
            density: Some(self.density.wire_name().to_owned()),
            nav_gap: Some(self.nav_gap),
            show_activity_bar: Some(self.show_activity_bar),
            show_notification_bell: Some(self.show_notification_bell),
            show_top_bar_extras: Some(self.show_top_bar_extras),
            right_sidebar_side: Some(self.right_sidebar_side.wire_name().to_owned()),
        }
    }

    /// 从落盘表示还原；任何非法值回落到默认值而非报错。
    #[must_use]
    pub fn from_wire(wire: AppearanceWire) -> Self {
        let fallback = Self::default();

        let accent = wire
            .accent
            .filter(|parts| parts.iter().all(|value| value.is_finite()))
            .map_or(fallback.accent, |[h, s, l, a]| Hsla { h, s, l, a });

        let dark = wire
            .dark_theme
            .as_deref()
            .map_or(fallback.theme.dark, |value| {
                BuiltinThemeId::from_wire_name(value, ThemeMode::Dark)
            });
        let light = wire
            .light_theme
            .as_deref()
            .map_or(fallback.theme.light, |value| {
                BuiltinThemeId::from_wire_name(value, ThemeMode::Light)
            });

        let ui_scale = if wire.ui_scale.is_finite() {
            wire.ui_scale.clamp(UI_SCALE_RANGE.0, UI_SCALE_RANGE.1)
        } else {
            fallback.ui_scale
        };

        let font_size = wire
            .font_size
            .filter(|size| size.is_finite())
            .map(|size| size.clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1));

        let font_family = wire
            .font_family
            .filter(|family| !family.trim().is_empty())
            .map(SharedString::from);

        // 布局类字段：缺键 → 默认；非法数值 → clamp / 回落默认。旧存档里没有这些键，
        // 走 `None` 分支即可得到与升级前完全一致的外观。
        let density = wire
            .density
            .as_deref()
            .map_or(fallback.density, Density::from_wire_name);
        let nav_gap = wire
            .nav_gap
            .filter(|gap| gap.is_finite())
            .map_or(fallback.nav_gap, |gap| {
                gap.clamp(NAV_GAP_RANGE.0, NAV_GAP_RANGE.1)
            });
        let right_sidebar_side = wire
            .right_sidebar_side
            .as_deref()
            .map_or(fallback.right_sidebar_side, SidebarSide::from_wire_name);

        Self {
            preference: wire.preference,
            accent,
            ui_scale,
            font_size,
            font_family,
            language: wire.language,
            theme: ThemeSlots { dark, light },
            density,
            nav_gap,
            show_activity_bar: wire.show_activity_bar.unwrap_or(fallback.show_activity_bar),
            show_notification_bell: wire
                .show_notification_bell
                .unwrap_or(fallback.show_notification_bell),
            show_top_bar_extras: wire
                .show_top_bar_extras
                .unwrap_or(fallback.show_top_bar_extras),
            right_sidebar_side,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AppearanceWire;
    use crate::manager::{Appearance, ThemePreference};

    #[test]
    fn round_trip_preserves_appearance() {
        let appearance = Appearance {
            preference: ThemePreference::Dark,
            ui_scale: 1.25,
            font_size: Some(15.),
            ..Appearance::default()
        };

        let restored = Appearance::from_wire(appearance.to_wire());
        assert_eq!(restored.preference, ThemePreference::Dark);
        assert!((restored.ui_scale - 1.25).abs() < f32::EPSILON);
        assert_eq!(restored.font_size, Some(15.));
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let wire = AppearanceWire {
            ui_scale: 9.,
            font_size: Some(99.),
            accent: None,
            preference: ThemePreference::System,
            font_family: Some("   ".to_owned()),
            language: Default::default(),
            dark_theme: Some("nonsense".to_owned()),
            light_theme: None,
            nav_gap: Some(999.),
            density: Some("nonsense".to_owned()),
            right_sidebar_side: Some("nonsense".to_owned()),
            show_activity_bar: None,
            show_notification_bell: None,
            show_top_bar_extras: None,
        };
        let appearance = Appearance::from_wire(wire);
        assert!((appearance.ui_scale - 1.5).abs() < f32::EPSILON);
        assert_eq!(appearance.font_size, Some(crate::FONT_SIZE_RANGE.1));
        assert!(appearance.font_family.is_none());
        // 布局类：数值 clamp、枚举回落默认、缺键用回默认。
        assert!((appearance.nav_gap - crate::manager::NAV_GAP_RANGE.1).abs() < f32::EPSILON);
        assert_eq!(appearance.density, crate::manager::Density::Normal);
        assert_eq!(
            appearance.right_sidebar_side,
            crate::manager::SidebarSide::Right
        );
        assert!(appearance.show_activity_bar);
        assert!(appearance.show_notification_bell);
        assert!(appearance.show_top_bar_extras);
    }

    /// 布局类设置能完整往返（本轮新增字段与旧字段走同一条 wire 通道）。
    #[test]
    fn layout_preferences_round_trip() {
        let appearance = Appearance {
            density: crate::manager::Density::Comfortable,
            nav_gap: 12.,
            show_activity_bar: false,
            show_notification_bell: false,
            show_top_bar_extras: false,
            right_sidebar_side: crate::manager::SidebarSide::Left,
            ..Appearance::default()
        };

        let restored = Appearance::from_wire(appearance.to_wire());
        assert_eq!(restored, appearance);
    }

    /// 旧版本存档（没有布局类键）读出来必须是升级前的默认外观。
    ///
    /// 旧 JSON 里这些键缺失 → `#[serde(default)]` 得到 `None`，所以直接构造 `None`
    /// 即等价于旧存档；不必为此在 duck-theme 里引入 JSON 解析依赖。
    #[test]
    fn legacy_wire_without_layout_keys_keeps_defaults() {
        let legacy = AppearanceWire {
            preference: ThemePreference::Dark,
            accent: None,
            ui_scale: 1.0,
            font_size: None,
            font_family: None,
            language: Default::default(),
            dark_theme: None,
            light_theme: None,
            density: None,
            nav_gap: None,
            show_activity_bar: None,
            show_notification_bell: None,
            show_top_bar_extras: None,
            right_sidebar_side: None,
        };
        let appearance = Appearance::from_wire(legacy);

        assert_eq!(appearance.preference, ThemePreference::Dark);
        assert_eq!(appearance.density, crate::manager::Density::Normal);
        assert!((appearance.nav_gap - 4.).abs() < f32::EPSILON);
        assert!(appearance.show_activity_bar);
        assert!(appearance.show_notification_bell);
        assert!(appearance.show_top_bar_extras);
        assert_eq!(
            appearance.right_sidebar_side,
            crate::manager::SidebarSide::Right
        );
    }
}
