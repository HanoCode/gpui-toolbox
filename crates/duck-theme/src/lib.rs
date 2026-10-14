//! Duck Toolbox 主题层。
//!
//! 与 FluxDown 的主题系统同构：一组预设调色板 → Base 语义 token → 按固定规则派生扩展
//! token（chrome / hairline / navSelected / accentText …），再投影到 gpui-component 与
//! gpui-base，业务只读 [`active_theme`] 提供的完整快照。
//!
//! 业务 feature 不在本 crate 中。

mod builtin;
mod color;
mod i18n;
mod manager;
mod syntax;
mod tokens;
mod wire;

pub use builtin::{BuiltinThemeId, base_tokens, button_surface, extended_tokens, foreground_for};
pub use color::{color_from_hex, color_from_rgb};
pub use i18n::{
    Lang, Locale, UnderMinute, bytes, datetime_full, datetime_short, number, relative_time,
    system_lang, t, tf, tn, tr, tr_relative_time, trf, trn,
};
pub use manager::{
    Appearance, DEFAULT_ACCENT_RGB, Density, DuckThemeState, NAV_GAP_RANGE, SidebarSide,
    ThemePreference, ThemeSlots, accent_from_rgb, active_theme, apply, default_accent, init,
    sync_system_theme, update_appearance,
};
pub use tokens::{
    ComponentTokens, DensityTokens, ExtendedColors, ExtendedTokens, FocusRingTokens, IconSizes,
    OpacityTokens, ResolvedTheme, StrokeTokens,
};
pub use wire::{AppearanceWire, UI_SCALE_RANGE};

pub use gpui_base::{
    ColorTokens, RadiusTokens, SemanticThemeTokens, ShadowTokens, SpacingTokens, TextStyleToken,
    TypographyTokens,
};
pub use gpui_component::ThemeMode;

/// 主题基线正文字号（px）。
pub const BASE_FONT_SIZE: f32 = 13.;
/// 主题基线正文行高（px）。
pub const BASE_LINE_HEIGHT: f32 = 18.;
/// 可选正文字号档位（px）。
pub const FONT_SIZES: [f32; 5] = [11., 12., 13., 15., 17.];
/// 正文字号允许范围（px）。
pub const FONT_SIZE_RANGE: (f32, f32) = (11., 17.);
