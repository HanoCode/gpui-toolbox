//! 内置主题基底与派生 token。
//!
//! 复刻 FluxDown 的 `fluxdown_ui_theme::builtin`：5 组预设调色板 → Base 语义 token，
//! 再由 Base 按固定规则派生扩展色（chrome / hairline / navSelected / accentText …）。
//! 规则与原实现逐条对应，改动时须同步。

use gpui::{FontWeight, Hsla, Rgba, px, rgb};
use gpui_base::{
    ColorTokens, RadiusTokens, SemanticThemeTokens, ShadowTokens, SpacingTokens, TextStyleToken,
    TypographyTokens,
};
use gpui_component::ThemeMode;

use crate::manager::Density;
use crate::tokens::{
    ComponentTokens, DensityTokens, ExtendedColors, ExtendedTokens, FocusRingTokens, IconSizes,
    OpacityTokens, StrokeTokens,
};

/// 主题阴影层的 alpha（46/255），与 gpui-base `ShadowTokens::elevations` 同源。
const SHADOW_ALPHA: f32 = 46.0 / 255.0;
/// Flutter `accentBackground` alpha 的 8-bit 量化：0.10 / 0.15 / 0.18。
const ACCENT_ALPHA_LIGHT: u8 = 26;
const ACCENT_ALPHA_SOFT: u8 = 38;
const ACCENT_ALPHA_DARK: u8 = 46;
/// 主色按钮文字的最低对比度（WCAG AA 正文）。
const MIN_PRIMARY_CONTRAST: f32 = 4.5;
/// 焦点环相对它真正落上去的底色的最低对比度（WCAG 2.1 SC 1.4.11 非文本对比）。
///
/// 焦点环是**图形指示**而非文字，故取 3:1 而不是正文的 4.5:1 —— 但这一档必须由它自己
/// 扛住：键盘用户靠它判断「我现在停在哪」，看不见就等于没有键盘导航。
const MIN_FOCUS_RING_CONTRAST: f32 = 3.0;
/// 文本选区底色相对强调色的透明度。
const TEXT_SELECTION_ALPHA: f32 = 0.3;

/// 内置主题 id；wire 名称即 Dart enum `.name`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinThemeId {
    Adventure,
    AdventureTime,
    Alduin,
    Asciinema,
    AuroraLight,
    AyuDark,
    AyuLight,
    CatppuccinFrappe,
    CatppuccinLatte,
    CatppuccinMacchiato,
    CatppuccinMocha,
    DefaultDark,
    DefaultLight,
    EverforestDark,
    EverforestLight,
    Fahrenheit,
    FlexokiDark,
    FlexokiLight,
    GruvboxDark,
    GruvboxLight,
    Harper,
    HybridDark,
    HybridLight,
    Jellybeans,
    Kibble,
    MacOSClassicDark,
    MacOSClassicLight,
    Matrix,
    MellifluousDark,
    MellifluousLight,
    MidnightBlue,
    MolokaiDark,
    MolokaiLight,
    Nord,
    SolarizedDark,
    SolarizedLight,
    Spaceduck,
    TokyoMoon,
    TokyoNight,
    TokyoStorm,
    Twilight,
    WarmLight,
}

impl BuiltinThemeId {
    /// 全部内置主题（按展示名的字节序排列，主题选择器三列按此切分）。
    pub const ALL: [Self; 42] = [
        Self::Adventure,
        Self::AdventureTime,
        Self::Alduin,
        Self::Asciinema,
        Self::AuroraLight,
        Self::AyuDark,
        Self::AyuLight,
        Self::CatppuccinFrappe,
        Self::CatppuccinLatte,
        Self::CatppuccinMacchiato,
        Self::CatppuccinMocha,
        Self::DefaultDark,
        Self::DefaultLight,
        Self::EverforestDark,
        Self::EverforestLight,
        Self::Fahrenheit,
        Self::FlexokiDark,
        Self::FlexokiLight,
        Self::GruvboxDark,
        Self::GruvboxLight,
        Self::Harper,
        Self::HybridDark,
        Self::HybridLight,
        Self::Jellybeans,
        Self::Kibble,
        Self::MacOSClassicDark,
        Self::MacOSClassicLight,
        Self::Matrix,
        Self::MellifluousDark,
        Self::MellifluousLight,
        Self::MidnightBlue,
        Self::MolokaiDark,
        Self::MolokaiLight,
        Self::Nord,
        Self::SolarizedDark,
        Self::SolarizedLight,
        Self::Spaceduck,
        Self::TokyoMoon,
        Self::TokyoNight,
        Self::TokyoStorm,
        Self::Twilight,
        Self::WarmLight,
    ];

    /// 主题本身的明暗外观。
    #[must_use]
    pub fn appearance(self) -> ThemeMode {
        match self {
            Self::Adventure
            | Self::AdventureTime
            | Self::Alduin
            | Self::Asciinema
            | Self::AyuDark
            | Self::CatppuccinFrappe
            | Self::CatppuccinMacchiato
            | Self::CatppuccinMocha
            | Self::DefaultDark
            | Self::EverforestDark
            | Self::Fahrenheit
            | Self::FlexokiDark
            | Self::GruvboxDark
            | Self::Harper
            | Self::HybridDark
            | Self::Jellybeans
            | Self::Kibble
            | Self::MacOSClassicDark
            | Self::Matrix
            | Self::MellifluousDark
            | Self::MidnightBlue
            | Self::MolokaiDark
            | Self::Nord
            | Self::SolarizedDark
            | Self::Spaceduck
            | Self::TokyoMoon
            | Self::TokyoNight
            | Self::TokyoStorm
            | Self::Twilight => ThemeMode::Dark,
            Self::AuroraLight
            | Self::AyuLight
            | Self::CatppuccinLatte
            | Self::DefaultLight
            | Self::EverforestLight
            | Self::FlexokiLight
            | Self::GruvboxLight
            | Self::HybridLight
            | Self::MacOSClassicLight
            | Self::MellifluousLight
            | Self::MolokaiLight
            | Self::SolarizedLight
            | Self::WarmLight => ThemeMode::Light,
        }
    }

    /// 指定明暗槽位的默认主题。
    #[must_use]
    pub fn default_for(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::DefaultDark,
            ThemeMode::Light => Self::DefaultLight,
        }
    }

    /// 稳定的 wire 名称（偏好持久化用）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Adventure => "adventure",
            Self::AdventureTime => "adventure_time",
            Self::Alduin => "alduin",
            Self::Asciinema => "asciinema",
            Self::AuroraLight => "aurora_light",
            Self::AyuDark => "ayu_dark",
            Self::AyuLight => "ayu_light",
            Self::CatppuccinFrappe => "catppuccin_frappe",
            Self::CatppuccinLatte => "catppuccin_latte",
            Self::CatppuccinMacchiato => "catppuccin_macchiato",
            Self::CatppuccinMocha => "catppuccin_mocha",
            Self::DefaultDark => "default_dark",
            Self::DefaultLight => "default_light",
            Self::EverforestDark => "everforest_dark",
            Self::EverforestLight => "everforest_light",
            Self::Fahrenheit => "fahrenheit",
            Self::FlexokiDark => "flexoki_dark",
            Self::FlexokiLight => "flexoki_light",
            Self::GruvboxDark => "gruvbox_dark",
            Self::GruvboxLight => "gruvbox_light",
            Self::Harper => "harper",
            Self::HybridDark => "hybrid_dark",
            Self::HybridLight => "hybrid_light",
            Self::Jellybeans => "jellybeans",
            Self::Kibble => "kibble",
            Self::MacOSClassicDark => "macos_classic_dark",
            Self::MacOSClassicLight => "macos_classic_light",
            Self::Matrix => "matrix",
            Self::MellifluousDark => "mellifluous_dark",
            Self::MellifluousLight => "mellifluous_light",
            Self::MidnightBlue => "midnight_blue",
            Self::MolokaiDark => "molokai_dark",
            Self::MolokaiLight => "molokai_light",
            Self::Nord => "nord",
            Self::SolarizedDark => "solarized_dark",
            Self::SolarizedLight => "solarized_light",
            Self::Spaceduck => "spaceduck",
            Self::TokyoMoon => "tokyo_moon",
            Self::TokyoNight => "tokyo_night",
            Self::TokyoStorm => "tokyo_storm",
            Self::Twilight => "twilight",
            Self::WarmLight => "warm_light",
        }
    }

    /// 解析 wire 名称；未知值回退到对应槽位的默认主题。
    #[must_use]
    pub fn from_wire_name(value: &str, fallback: ThemeMode) -> Self {
        Self::ALL
            .into_iter()
            .find(|id| id.wire_name() == value)
            .unwrap_or_else(|| Self::default_for(fallback))
    }

    /// 展示名（主题画廊卡片下方）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Adventure => "Adventure",
            Self::AdventureTime => "Adventure Time",
            Self::Alduin => "Alduin",
            Self::Asciinema => "Asciinema",
            Self::AuroraLight => "Aurora Light",
            Self::AyuDark => "Ayu Dark",
            Self::AyuLight => "Ayu Light",
            Self::CatppuccinFrappe => "Catppuccin Frappe",
            Self::CatppuccinLatte => "Catppuccin Latte",
            Self::CatppuccinMacchiato => "Catppuccin Macchiato",
            Self::CatppuccinMocha => "Catppuccin Mocha",
            Self::DefaultDark => "Default Dark",
            Self::DefaultLight => "Default Light",
            Self::EverforestDark => "Everforest Dark",
            Self::EverforestLight => "Everforest Light",
            Self::Fahrenheit => "Fahrenheit",
            Self::FlexokiDark => "Flexoki Dark",
            Self::FlexokiLight => "Flexoki Light",
            Self::GruvboxDark => "Gruvbox Dark",
            Self::GruvboxLight => "Gruvbox Light",
            Self::Harper => "Harper",
            Self::HybridDark => "Hybrid Dark",
            Self::HybridLight => "Hybrid Light",
            Self::Jellybeans => "Jellybeans",
            Self::Kibble => "Kibble",
            Self::MacOSClassicDark => "macOS Classic Dark",
            Self::MacOSClassicLight => "macOS Classic Light",
            Self::Matrix => "Matrix",
            Self::MellifluousDark => "Mellifluous Dark",
            Self::MellifluousLight => "Mellifluous Light",
            Self::MidnightBlue => "Midnight Blue",
            Self::MolokaiDark => "Molokai Dark",
            Self::MolokaiLight => "Molokai Light",
            Self::Nord => "Nord",
            Self::SolarizedDark => "Solarized Dark",
            Self::SolarizedLight => "Solarized Light",
            Self::Spaceduck => "Spaceduck",
            Self::TokyoMoon => "Tokyo Moon",
            Self::TokyoNight => "Tokyo Night",
            Self::TokyoStorm => "Tokyo Storm",
            Self::Twilight => "Twilight",
            Self::WarmLight => "Warm Light",
        }
    }

    /// 某个明暗槽位可选的内置预设：外观与槽位一致的才放得进去。
    pub fn presets_for(mode: ThemeMode) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |id| id.appearance() == mode)
    }

    /// 主题卡片预览用的基础配色（强调色取预设自身的默认强调色）。
    #[must_use]
    pub fn preview(self) -> ColorTokens {
        let palette = palette(self);
        color_tokens(palette, color(palette.accent))
    }
}

/// Flutter `FluxThemeTokens` 工厂中映射到 Base token 的 Layer0 颜色。
struct Palette {
    accent: u32,
    accent_background_alpha: u8,
    /// 固定的 `accentForeground`（如 Nord）；`None` 按强调色亮度自动取黑 / 白。
    accent_foreground: Option<u32>,
    background: u32,
    surface1: u32,
    surface2: u32,
    text_primary: u32,
    text_secondary: u32,
    border: u32,
    status_error: u32,
}

fn palette(id: BuiltinThemeId) -> &'static Palette {
    match id {
        BuiltinThemeId::DefaultDark => &Palette {
            accent: 0x3B82F6,
            accent_background_alpha: ACCENT_ALPHA_DARK,
            accent_foreground: None,
            background: 0x1C1C1E,
            surface1: 0x2C2C2E,
            surface2: 0x3A3A3C,
            text_primary: 0xF5F5F7,
            text_secondary: 0xA1A1A6,
            border: 0x48484A,
            status_error: 0xEF4444,
        },
        BuiltinThemeId::DefaultLight => &Palette {
            accent: 0x3B82F6,
            accent_background_alpha: ACCENT_ALPHA_LIGHT,
            accent_foreground: None,
            background: 0xF8F9FA,
            surface1: 0xFFFFFF,
            surface2: 0xF1F3F5,
            text_primary: 0x09090B,
            text_secondary: 0x71717A,
            border: 0xE4E4E7,
            status_error: 0xEF4444,
        },
        BuiltinThemeId::MidnightBlue => &Palette {
            accent: 0x60A5FA,
            accent_background_alpha: ACCENT_ALPHA_SOFT,
            accent_foreground: None,
            background: 0x0F172A,
            surface1: 0x1E293B,
            surface2: 0x334155,
            text_primary: 0xF1F5F9,
            text_secondary: 0x94A3B8,
            border: 0x334155,
            status_error: 0xEF4444,
        },
        BuiltinThemeId::Nord => &Palette {
            accent: 0x88C0D0,
            accent_background_alpha: ACCENT_ALPHA_SOFT,
            accent_foreground: Some(0x2E3440),
            background: 0x2E3440,
            surface1: 0x3B4252,
            surface2: 0x434C5E,
            text_primary: 0xECEFF4,
            text_secondary: 0xD8DEE9,
            border: 0x4C566A,
            status_error: 0xBF616A,
        },
        BuiltinThemeId::WarmLight => &Palette {
            accent: 0xE11D48,
            accent_background_alpha: ACCENT_ALPHA_LIGHT,
            accent_foreground: None,
            background: 0xFFFBEB,
            surface1: 0xFFFFFF,
            surface2: 0xFEF3C7,
            text_primary: 0x1C1917,
            text_secondary: 0x78716C,
            border: 0xE7E5E4,
            status_error: 0xDC2626,
        },
        BuiltinThemeId::Adventure => &Palette {
            accent: 0x4384AD,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x040404,
            surface1: 0x003A5B,
            surface2: 0x171717,
            text_primary: 0xFEFFFF,
            text_secondary: 0x5D6165,
            border: 0x282828,
            status_error: 0xD84A33,
        },
        BuiltinThemeId::AdventureTime => &Palette {
            accent: 0x5F72C6,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x1F1D45,
            surface1: 0x003A5B,
            surface2: 0x29274A,
            text_primary: 0xC7C7D4,
            text_secondary: 0x717192,
            border: 0x333150,
            status_error: 0xA02733,
        },
        BuiltinThemeId::Alduin => &Palette {
            accent: 0x458588,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x1C1C1C,
            surface1: 0x282828,
            surface2: 0x262626,
            text_primary: 0x9E9E9E,
            text_secondary: 0x878787,
            border: 0x3A3A3A,
            status_error: 0x8B5F61,
        },
        BuiltinThemeId::Asciinema => &Palette {
            accent: 0x26B0D7,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x121314,
            surface1: 0x181919,
            surface2: 0x1A1B1C,
            text_primary: 0xCCCCCC,
            text_secondary: 0x6D6D6D,
            border: 0x3A3A3A,
            status_error: 0xDD3C69,
        },
        BuiltinThemeId::AuroraLight => &Palette {
            accent: 0x0F172A,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xFFFFFF,
            surface1: 0xF1F5F9,
            surface2: 0xF1F5F9,
            text_primary: 0x020617,
            text_secondary: 0x64748B,
            border: 0xE2E8F0,
            status_error: 0xEF4444,
        },
        BuiltinThemeId::AyuDark => &Palette {
            accent: 0x5AC1FE,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x0D1016,
            surface1: 0x16191F,
            surface2: 0x16191F,
            text_primary: 0xB3B1AD,
            text_secondary: 0x52514F,
            border: 0x292A2C,
            status_error: 0xEF7177,
        },
        BuiltinThemeId::AyuLight => &Palette {
            accent: 0x55B4D3,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xFCFCFC,
            surface1: 0xF3F4F5,
            surface2: 0xF4F4F5,
            text_primary: 0x5C6166,
            text_secondary: 0x99A0A6,
            border: 0xCFD1D2,
            status_error: 0xF07171,
        },
        BuiltinThemeId::CatppuccinFrappe => &Palette {
            accent: 0x8CAAEE,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x232634,
            surface1: 0x414559,
            surface2: 0x2C303F,
            text_primary: 0xC6D0F5,
            text_secondary: 0x979DB5,
            border: 0x3E4255,
            status_error: 0xE78284,
        },
        BuiltinThemeId::CatppuccinLatte => &Palette {
            accent: 0x7287FD,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xE5E9EF,
            surface1: 0xDCE0E8,
            surface2: 0xDCE0E8,
            text_primary: 0x4C4F69,
            text_secondary: 0x9A9DB2,
            border: 0xCCD0DA,
            status_error: 0xD26A53,
        },
        BuiltinThemeId::CatppuccinMacchiato => &Palette {
            accent: 0x8AADF4,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x1E2030,
            surface1: 0x171926,
            surface2: 0x282A3A,
            text_primary: 0xCAD3F5,
            text_secondary: 0xB8C0E0,
            border: 0x494D64,
            status_error: 0xED8796,
        },
        BuiltinThemeId::CatppuccinMocha => &Palette {
            accent: 0x89B4FA,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x181825,
            surface1: 0x302D41,
            surface2: 0x302D41,
            text_primary: 0xCDD6F4,
            text_secondary: 0x6C7086,
            border: 0x313244,
            status_error: 0xF38BA8,
        },
        BuiltinThemeId::EverforestDark => &Palette {
            accent: 0xE69875,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x262E34,
            surface1: 0x2E383B,
            surface2: 0x2E383B,
            text_primary: 0xD3C6AA,
            text_secondary: 0x6D7873,
            border: 0x40484C,
            status_error: 0xE67E80,
        },
        BuiltinThemeId::EverforestLight => &Palette {
            accent: 0xE69875,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xFEFCEE,
            surface1: 0xF9F5E4,
            surface2: 0xF1F0E2,
            text_primary: 0x5F6D75,
            text_secondary: 0x959A9D,
            border: 0xE7E5D4,
            status_error: 0xE67E80,
        },
        BuiltinThemeId::Fahrenheit => &Palette {
            accent: 0x720202,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x000000,
            surface1: 0x1E1E1E,
            surface2: 0x131313,
            text_primary: 0xFFFFCE,
            text_secondary: 0x828282,
            border: 0x252525,
            status_error: 0x723202,
        },
        BuiltinThemeId::FlexokiDark => &Palette {
            accent: 0x24837B,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x100F0F,
            surface1: 0x1C1B1A,
            surface2: 0x1C1B1A,
            text_primary: 0xCECDC3,
            text_secondary: 0x878580,
            border: 0x282726,
            status_error: 0xAF3029,
        },
        BuiltinThemeId::FlexokiLight => &Palette {
            accent: 0x3AA99F,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xFFFCF0,
            surface1: 0xF2F0E5,
            surface2: 0xF2F0E5,
            text_primary: 0x100F0F,
            text_secondary: 0x6F6E69,
            border: 0xE6E4D9,
            status_error: 0xD14D41,
        },
        BuiltinThemeId::GruvboxDark => &Palette {
            accent: 0xD79921,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x1D2021,
            surface1: 0x282828,
            surface2: 0x272828,
            text_primary: 0xEBDBB2,
            text_secondary: 0x928374,
            border: 0x3E3936,
            status_error: 0xF06555,
        },
        BuiltinThemeId::GruvboxLight => &Palette {
            accent: 0xD79921,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xFBF1C7,
            surface1: 0xEBDBB2,
            surface2: 0xECDFB8,
            text_primary: 0x3C3836,
            text_secondary: 0x928374,
            border: 0xD5C4A1,
            status_error: 0xFB4934,
        },
        BuiltinThemeId::Harper => &Palette {
            accent: 0xB196C6,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x010101,
            surface1: 0x003A5B,
            surface2: 0x171717,
            text_primary: 0xA8A49D,
            text_secondary: 0x726E69,
            border: 0x333333,
            status_error: 0xFF5874,
        },
        BuiltinThemeId::HybridDark => &Palette {
            accent: 0x15678A,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x1D1F21,
            surface1: 0x1D1F21,
            surface2: 0x242629,
            text_primary: 0xE8E8E8,
            text_secondary: 0x878787,
            border: 0x34373C,
            status_error: 0x8A1515,
        },
        BuiltinThemeId::HybridLight => &Palette {
            accent: 0x005F87,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xE4E4E4,
            surface1: 0xD0D0D0,
            surface2: 0xD7D7D7,
            text_primary: 0x1C1C1C,
            text_secondary: 0x5F5F5F,
            border: 0xCACACA,
            status_error: 0x5F0000,
        },
        BuiltinThemeId::Jellybeans => &Palette {
            accent: 0x97BEDC,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x151515,
            surface1: 0x151515,
            surface2: 0x242424,
            text_primary: 0xE8E8D3,
            text_secondary: 0x767676,
            border: 0x2E2E2E,
            status_error: 0xE27373,
        },
        BuiltinThemeId::Kibble => &Palette {
            accent: 0x6CE05C,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x0E100A,
            surface1: 0x003A5B,
            surface2: 0x171913,
            text_primary: 0xF7F7F7,
            text_secondary: 0x777777,
            border: 0x292A24,
            status_error: 0xC70231,
        },
        BuiltinThemeId::MacOSClassicDark => &Palette {
            accent: 0x077CFD,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x131313,
            surface1: 0x1C1C1E,
            surface2: 0x202020,
            text_primary: 0xDEDEDE,
            text_secondary: 0x9D9D9D,
            border: 0x303030,
            status_error: 0xFF5257,
        },
        BuiltinThemeId::MacOSClassicLight => &Palette {
            accent: 0x0060DE,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xF9F9F9,
            surface1: 0xFEFEFE,
            surface2: 0xEAEAEA,
            text_primary: 0x000000,
            text_secondary: 0x707070,
            border: 0xD2D2D2,
            status_error: 0xD21F07,
        },
        BuiltinThemeId::Matrix => &Palette {
            accent: 0x00FF00,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x020D02,
            surface1: 0x001900,
            surface2: 0x001900,
            text_primary: 0x88FF88,
            text_secondary: 0x007700,
            border: 0x12410E,
            status_error: 0xFF0000,
        },
        BuiltinThemeId::MellifluousDark => &Palette {
            accent: 0x5A6599,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x1A1A1A,
            surface1: 0x282C34,
            surface2: 0x282828,
            text_primary: 0xABB2BF,
            text_secondary: 0x828997,
            border: 0x444444,
            status_error: 0xC95954,
        },
        BuiltinThemeId::MellifluousLight => &Palette {
            accent: 0x5A6599,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xE7E7E7,
            surface1: 0xFAFAFA,
            surface2: 0xE0E0E0,
            text_primary: 0x383A42,
            text_secondary: 0x828997,
            border: 0xCACACA,
            status_error: 0xC95954,
        },
        BuiltinThemeId::MolokaiDark => &Palette {
            accent: 0xDC1860,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x1B1D1E,
            surface1: 0x1B1D1E,
            surface2: 0x232526,
            text_primary: 0xF8F8F2,
            text_secondary: 0x5B5A54,
            border: 0x393939,
            status_error: 0xDC1860,
        },
        BuiltinThemeId::MolokaiLight => &Palette {
            accent: 0xE14774,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xFEFAF9,
            surface1: 0xFEFAF9,
            surface2: 0xF1EDED,
            text_primary: 0x0A0A0A,
            text_secondary: 0x767676,
            border: 0xE4DEDA,
            status_error: 0xE17047,
        },
        BuiltinThemeId::SolarizedDark => &Palette {
            accent: 0x0D667D,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x002B36,
            surface1: 0x002B36,
            surface2: 0x073642,
            text_primary: 0xFDF6E3,
            text_secondary: 0x839496,
            border: 0x103A44,
            status_error: 0x7D2F0D,
        },
        BuiltinThemeId::SolarizedLight => &Palette {
            accent: 0x586E75,
            accent_background_alpha: 26,
            accent_foreground: None,
            background: 0xFDF6E3,
            surface1: 0xFDF6E3,
            surface2: 0xEEE8D5,
            text_primary: 0x586E75,
            text_secondary: 0x93A1A1,
            border: 0xDCD4BC,
            status_error: 0x755858,
        },
        BuiltinThemeId::Spaceduck => &Palette {
            accent: 0x089CC5,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x0F111B,
            surface1: 0x003A5B,
            surface2: 0x171A28,
            text_primary: 0xA3A49D,
            text_secondary: 0x4B6479,
            border: 0x272C42,
            status_error: 0xE33400,
        },
        BuiltinThemeId::TokyoMoon => &Palette {
            accent: 0x82AAFF,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x222436,
            surface1: 0x2D3149,
            surface2: 0x2D3149,
            text_primary: 0xC0CAF5,
            text_secondary: 0x6E738D,
            border: 0x323752,
            status_error: 0xFF757F,
        },
        BuiltinThemeId::TokyoNight => &Palette {
            accent: 0x7AA2F7,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x1A1B26,
            surface1: 0x292E42,
            surface2: 0x1F212E,
            text_primary: 0xC0CAF5,
            text_secondary: 0x565F89,
            border: 0x292E42,
            status_error: 0xF7768E,
        },
        BuiltinThemeId::TokyoStorm => &Palette {
            accent: 0x7AA2F7,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x24283B,
            surface1: 0x292E42,
            surface2: 0x292E42,
            text_primary: 0xC0CAF5,
            text_secondary: 0x565F89,
            border: 0x3D4462,
            status_error: 0xF7768E,
        },
        BuiltinThemeId::Twilight => &Palette {
            accent: 0xCDA869,
            accent_background_alpha: 46,
            accent_foreground: None,
            background: 0x141414,
            surface1: 0x1E1E1E,
            surface2: 0x222222,
            text_primary: 0xDCDCDC,
            text_secondary: 0x828282,
            border: 0x343434,
            status_error: 0xC06D44,
        },
    }
}

/// Base 语义 token：调色板 + 用户强调色（`ensure_primary_contrast` 保证主色按钮文字可读）。
#[must_use]
pub fn base_tokens(builtin: BuiltinThemeId, accent: Hsla) -> SemanticThemeTokens {
    let palette = palette(builtin);
    let mut colors = color_tokens(palette, accent);
    // 主色按钮与其文字必须达到 WCAG AA（正文 4.5:1）。这一守卫**明暗两档都要跑**：
    // 此前只在亮色分支执行，暗色下默认强调色 `#3B82F6` 配白字实测约 3.68:1（低于 4.5）。
    // 焦点环的种子取强调色**原值**（`color_tokens` 已把它写进 `ring`），且必须在主色收敛
    // 之前取走：主色接下来会被压到能配按钮文字，而焦点环要对底色负责，起点不该先被那一次
    // 「为文字服务」的收敛污染。
    let accent_seed = colors.ring;
    colors.primary = button_surface(colors.primary, colors.primary_foreground);
    // 焦点环与按钮底色**不再同源**：`primary` 只对按钮文字负责，`ring` 对底色负责。
    colors.ring = focus_ring(
        accent_seed,
        builtin.appearance(),
        [
            colors.background,
            colors.surface,
            colors.secondary,
            colors.muted,
            colors.border,
        ],
    );
    colors.accent = colors.primary.alpha(colors.accent.a);
    colors.accent_foreground = colors.primary;
    colors.selection = colors.primary.alpha(TEXT_SELECTION_ALPHA);

    // 字阶**逐档显式定义**，不再依赖 `..Default::default()` 的隐式尺寸——上游默认是
    // 12/14/16/18/20，与本项目的 13 基线拼在一起会生成「12 与 13 只差 1px」这类伪档位，
    // 且上游一旦调默认值，这里会静默跟着漂移。`..Default::default()` 只留给字体族
    // （`sans` / `mono`），尺寸字段一个都不许省。
    //
    // 取值严格对齐 **macOS HIG 内建文本样式**（桌面正文基线 13pt）：
    //   Callout 12 · Body 13 · Title 3 15 · Title 2 17 · Title 1 22
    // 因此 12→13 的相邻 1px 正是 macOS 官方的 Callout→Body 关系，属规范内；
    // 真正的冗余（两个 token 表达同一角色）在扩展 token 那一侧，见 `extended_tokens`。
    // 行高：`sm` 取 18（gpui-component 的 rem 基准，同时是全站行高节奏），其余按 4px 网格
    // 取整（macOS 官方 Callout 12/15、Body 13/16，此处各放宽 1–2px 以适配桌面列表密度）。
    let typography = TypographyTokens {
        xs: TextStyleToken {
            size: px(12.),
            line_height: px(16.),
            weight: FontWeight::NORMAL,
        },
        sm: TextStyleToken {
            size: px(13.),
            line_height: px(18.),
            weight: FontWeight::NORMAL,
        },
        md: TextStyleToken {
            size: px(15.),
            line_height: px(20.),
            weight: FontWeight::NORMAL,
        },
        lg: TextStyleToken {
            size: px(17.),
            line_height: px(22.),
            weight: FontWeight::NORMAL,
        },
        xl: TextStyleToken {
            size: px(22.),
            line_height: px(26.),
            weight: FontWeight::NORMAL,
        },
        // 等宽正文档：与 `sm` 同号，行高放宽一档，供需要逐行比对的长文本使用。
        // 日志 / 许可正文当前取 `xs`（12/16）换取更紧凑的列表密度，因此**本档暂无消费方**；
        // 它是上游 `TypographyTokens` 的必填字段，保留完整阶梯而不置零。
        mono_md: TextStyleToken {
            size: px(13.),
            line_height: px(20.),
            weight: FontWeight::NORMAL,
        },
        ..TypographyTokens::default()
    };

    SemanticThemeTokens {
        colors,
        radius: RadiusTokens::default(),
        spacing: SpacingTokens::default(),
        typography,
        shadow: ShadowTokens::elevations(Hsla {
            h: 0.,
            s: 0.,
            l: 0.,
            a: SHADOW_ALPHA,
        }),
    }
}

/// Flutter Layer0 → Base 语义 token 的固定映射；`accent` 为用户强调色。
fn color_tokens(palette: &Palette, accent: Hsla) -> ColorTokens {
    let accent = if palette.accent_foreground.is_none() {
        accent
    } else {
        // Nord 固定前景，强调色仍按预设走。
        color(palette.accent)
    };
    ColorTokens {
        background: color(palette.background),
        foreground: color(palette.text_primary),
        surface: color(palette.surface1),
        surface_foreground: color(palette.text_primary),
        primary: accent,
        primary_foreground: palette
            .accent_foreground
            .map_or_else(|| foreground_for(accent), color),
        secondary: color(palette.surface2),
        secondary_foreground: color(palette.text_primary),
        muted: color(palette.surface2),
        muted_foreground: color(palette.text_secondary),
        accent: accent.alpha(f32::from(palette.accent_background_alpha) / 255.),
        accent_foreground: accent,
        destructive: color(palette.status_error),
        destructive_foreground: color(0xFFFFFF),
        border: color(palette.border),
        input: color(palette.border),
        ring: accent,
        selection: accent.alpha(TEXT_SELECTION_ALPHA),
    }
}

/// 由 Base token 派生扩展 token（规则见模块注释）。
#[must_use]
pub fn extended_tokens(base: &SemanticThemeTokens, mode: ThemeMode) -> ExtendedTokens {
    let c = base.colors;
    let nav_selected = match mode {
        ThemeMode::Dark => mix(c.muted, c.surface, 0.15),
        ThemeMode::Light => mix(c.muted, c.border, 0.65),
    };
    let chrome = match mode {
        ThemeMode::Dark => mix(c.background, c.surface, 0.35),
        ThemeMode::Light => mix(c.background, c.muted, 0.35),
    };
    let text_tertiary = mix(c.muted_foreground, c.surface, 0.38);
    let hairline = match mode {
        ThemeMode::Dark => mix(c.border, c.surface, 0.35),
        ThemeMode::Light => mix(c.border, c.surface, 0.4),
    };
    let row_hover = match mode {
        ThemeMode::Dark => mix(c.muted, c.surface, 0.45),
        ThemeMode::Light => mix(c.muted, c.surface, 0.4),
    };
    let accent_text = with_min_contrast(c.primary, c.surface, 4.5);
    // 悬停 / 按下底色改为强调色薄层：悬停即见主题色，深浅两档都弱于选中态
    // （`nav_selected` 是不透明的中性块），不会与选中态混淆。
    let hover_tint = match mode {
        ThemeMode::Dark => c.primary.alpha(0.16),
        ThemeMode::Light => c.primary.alpha(0.10),
    };
    let hover_tint_press = match mode {
        ThemeMode::Dark => c.primary.alpha(0.26),
        ThemeMode::Light => c.primary.alpha(0.18),
    };

    ExtendedTokens {
        colors: ExtendedColors {
            success: match mode {
                ThemeMode::Dark => color(0x30D158),
                ThemeMode::Light => color(0x1F9D55),
            },
            warning: match mode {
                ThemeMode::Dark => color(0xFFB340),
                ThemeMode::Light => color(0xC77C02),
            },
            info: c.primary,
            text_tertiary,
            hairline,
            row_hover,
            chrome,
            nav_hover: mix(chrome, nav_selected, 0.55),
            hover_tint,
            hover_tint_press,
            nav_selected,
            nav_selected_foreground: c.foreground,
            nav_selected_icon: with_min_contrast(c.primary, nav_selected, 3.),
            accent_text,
            // 太阳色：深色下取暖黄（与 warning 同源），浅色下未用但保持可读的深琥珀。
            sun: match mode {
                ThemeMode::Dark => color(0xFFB340),
                ThemeMode::Light => color(0xC77C02),
            },
            caret: accent_text,
            text_selection: c.primary.alpha(TEXT_SELECTION_ALPHA),
            drag_border: c.primary.alpha(0.65),
            drop_target: c.primary.alpha(0.2),
            status_downloading: c.primary,
            status_completed: text_tertiary,
            status_failed: c.destructive,
            status_paused: c.muted_foreground,
            status_queued: text_tertiary,
            progress_track: c.muted_foreground.alpha(0.16),
            progress_fill: c.primary,
        },
        // 角标档：macOS Caption 1。比 `typography.xs`（Callout）再小一档，只用于嵌在
        // 固定尺寸图形里的小角标（BETA 标、通知未读数），不参与正文层级。
        badge: TextStyleToken {
            size: px(10.),
            line_height: px(13.),
            weight: FontWeight::NORMAL,
        },
        // 展示档：macOS Large Title。指标卡数值这类「一眼看数」的场合，字重由调用方定。
        display: TextStyleToken {
            size: px(26.),
            line_height: px(32.),
            weight: FontWeight::NORMAL,
        },
        icon: IconSizes {
            sm: px(12.),
            md: px(14.),
            lg: px(16.),
        },
        stroke: StrokeTokens {
            thin: px(1.),
            strong: px(2.),
        },
        focus_ring: FocusRingTokens {
            width: px(2.),
            offset: px(1.),
        },
        opacity: OpacityTokens {
            subtle: 0.08,
            tint: 0.12,
            scrim: 0.28,
            muted: 0.35,
            faint: 0.4,
            hover_border: 0.45,
            disabled: 0.5,
            border: 0.55,
            focus_border: 0.6,
            dim: 0.75,
        },
    }
}

/// 控件密度档位（`density` 为档位、`scale` 为界面缩放；`title_bar` 不缩放）。
///
/// 三档不是等比的：紧凑档把「行高」压得比控件更紧（信息密度优先），宽松档反而让行高
/// 略高于控件（点选更从容）。活动栏三值随档位一起走，避免活动栏与内容区节奏脱节。
#[must_use]
pub fn density_tokens(density: Density, scale: f32) -> DensityTokens {
    let s = |value: f32| px(value * scale);
    // (control, nav_row, toolbar_button, title_bar, status_bar, status_control,
    //  section_header, task_row, task_row_compact, check_mark, rail, tile, button)
    let base = match density {
        Density::Compact => [
            24., 24., 24., 36., 24., 20., 22., 36., 26., 14., 44., 28., 28.,
        ],
        Density::Normal => [
            28., 28., 28., 40., 28., 22., 24., 44., 30., 16., 48., 32., 32.,
        ],
        Density::Comfortable => [
            32., 34., 32., 44., 32., 26., 28., 52., 36., 18., 52., 36., 36.,
        ],
    };
    let [
        control,
        nav_row,
        toolbar_button,
        title_bar,
        status_bar,
        status_control,
        section_header,
        task_row,
        task_row_compact,
        check_mark,
        activity_rail,
        activity_tile,
        activity_button,
    ] = base;
    DensityTokens {
        control: s(control),
        nav_row: s(nav_row),
        toolbar_button: s(toolbar_button),
        // 交通灯按标题栏高度垂直居中，故它只随密度档变、不随界面缩放变。
        title_bar: px(title_bar),
        status_bar: s(status_bar),
        status_control: s(status_control),
        section_header: s(section_header),
        task_row: s(task_row),
        task_row_compact: s(task_row_compact),
        check_mark: s(check_mark),
        activity_rail: s(activity_rail),
        activity_tile: s(activity_tile),
        activity_button: s(activity_button),
    }
}

/// 组件圆角 / 尺寸档位（`scale` 为界面缩放）。
#[must_use]
pub fn component_tokens(scale: f32) -> ComponentTokens {
    let md = px(6. * scale);
    let lg = px(8. * scale);
    ComponentTokens {
        button_radius: md,
        input_radius: md,
        dialog_radius: lg,
        card_radius: lg,
        badge_radius: px(9999.),
        nav_item_radius: md,
        progress_radius: px(9999.),
        progress_height: px(4. * scale),
        task_row_radius: md,
        checkbox_radius: px(4. * scale),
        tab_radius: md,
        menu_radius: md,
        tooltip_radius: md,
    }
}

/// 在 sRGB 空间把 `from` 向 `to` 混合 `amount`（0 = from，1 = to）。
fn mix(from: Hsla, to: Hsla, amount: f32) -> Hsla {
    let a = Rgba::from(from);
    let b = Rgba::from(to);
    let lerp = |x: f32, y: f32| x + (y - x) * amount;
    Hsla::from(Rgba {
        r: lerp(a.r, b.r),
        g: lerp(a.g, b.g),
        b: lerp(a.b, b.b),
        a: lerp(a.a, b.a),
    })
}

/// 相对亮度的黑白分界：背景亮于它时与黑色的对比度更高。
const CONTRAST_PIVOT: f32 = 0.179_13;
/// 对比度派生每步调整的 HSL 亮度。
const CONTRAST_STEP: f32 = 0.01;

/// WCAG 对比度（参数为相对亮度，顺序无关）。
fn contrast_ratio(a: f32, b: f32) -> f32 {
    let (light, dark) = if a > b { (a, b) } else { (b, a) };
    (light + 0.05) / (dark + 0.05)
}

/// 保持色相 / 饱和度 / alpha，沿亮度远离 `against`，直到对比度 ≥ `min`。
fn with_min_contrast(color: Hsla, against: Hsla, min: f32) -> Hsla {
    let background = relative_luminance(against);
    let darken = background > CONTRAST_PIVOT;
    let mut color = color;
    loop {
        if contrast_ratio(relative_luminance(color), background) >= min {
            return color;
        }
        let next = if darken {
            (color.l - CONTRAST_STEP).max(0.)
        } else {
            (color.l + CONTRAST_STEP).min(1.)
        };
        if next == color.l {
            return color;
        }
        color.l = next;
    }
}

/// 焦点环：由强调色派生，但对**底色**收敛，而不是对按钮文字收敛。
///
/// `ring` 曾经直接继承 `colors.primary`，而那次收敛只保证「主色按钮文字 4.5:1」。焦点环
/// 从不画在按钮文字上 —— 它画在页面底、卡片底、选中行底、活动栏底上。于是深色主题的强调
/// 色被压暗到能配白字之后，落在同样暗的底上就几乎没有对比度：实测 420 个组合里 80 个低于
/// 3:1（最低约 2.0:1），键盘焦点环在这些主题上基本看不见。
///
/// 这里保留色相与饱和度、只把亮度推离底色，直到 ≥ [`MIN_FOCUS_RING_CONTRAST`]。副作用是
/// 焦点环不再与强调色同色 —— 但那恰好也解掉「选中态边框已经是主色、焦点环落上去等于没画」
/// 的碰撞。收敛性由 [`with_min_contrast`] 的「亮度不再变化即返回」保证。
///
/// **只需对一个底色收敛**：`chrome` 与 `nav_selected` 都由这些底色经 [`mix`] 混合而来
/// （见 [`extended_tokens`]），sRGB 逐通道插值下亮度必落在参混两端之间；而焦点环在明暗两档
/// 都被推到该区间之外，故它与区间内任意混合色的对比度都不低于与「最难那一端」的对比度。
/// 最难的一端 = 暗色主题里**最亮**的底色（环要够亮才压得过它）、亮色主题里**最暗**的底色。
fn focus_ring(accent: Hsla, mode: ThemeMode, surfaces: [Hsla; 5]) -> Hsla {
    let [first, rest @ ..] = surfaces;
    let hardest = rest.iter().fold(first, |best, &candidate| {
        let (best_luminance, candidate_luminance) =
            (relative_luminance(best), relative_luminance(candidate));
        let candidate_is_harder = match mode {
            ThemeMode::Dark => candidate_luminance > best_luminance,
            ThemeMode::Light => candidate_luminance < best_luminance,
        };
        if candidate_is_harder { candidate } else { best }
    });
    with_min_contrast(accent, hardest, MIN_FOCUS_RING_CONTRAST)
}

/// 把「按钮底色」沿亮度远离其前景文字，直到对比度达到 AA 正文（4.5:1）。
///
/// 明暗两档共用：前景比底色亮就压暗底色（暗色模式的白字按钮），反之提亮
/// （浅色强调色配深字）。只改亮度，色相 / 饱和度 / alpha 保持不变，因此表现为
/// 同一色系的深浅微调，而不是换一种颜色。收敛性由 [`with_min_contrast`] 的
/// 「亮度不再变化即返回」保证（亮度空间有界，不会死循环）。
#[must_use]
pub fn button_surface(background: Hsla, foreground: Hsla) -> Hsla {
    with_min_contrast(background, foreground, MIN_PRIMARY_CONTRAST)
}

/// 强调色相对亮度 > 0.5 取近黑，否则取白。
#[must_use]
pub fn foreground_for(accent: Hsla) -> Hsla {
    if relative_luminance(accent) > 0.5 {
        color(0x09090B)
    } else {
        color(0xFFFFFF)
    }
}

/// WCAG 相对亮度。
fn relative_luminance(value: Hsla) -> f32 {
    fn linear(channel: f32) -> f32 {
        if channel <= 0.039_28 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }
    let rgba = Rgba::from(value);
    0.2126 * linear(rgba.r) + 0.7152 * linear(rgba.g) + 0.0722 * linear(rgba.b)
}

fn color(value: u32) -> Hsla {
    Hsla::from(rgb(value))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::manager::Density;

    use super::{
        BuiltinThemeId, base_tokens, button_surface, color, contrast_ratio, density_tokens,
        extended_tokens, relative_luminance,
    };

    /// 每个预设的 wire 名必须能在 42 个预设里唯一解析回它自己。
    ///
    /// 重名 / 漏 arm 会让「已选主题」在落盘恢复后静默变成另一个主题（表现为
    /// 「点了没反应」或「重启后主题变了」），这里把它钉死。
    #[test]
    fn every_theme_round_trips_through_its_wire_name() {
        let mut seen = HashSet::new();
        for id in BuiltinThemeId::ALL {
            let wire = id.wire_name();
            assert!(seen.insert(wire), "duplicate wire name: {wire}");
            assert_eq!(
                BuiltinThemeId::from_wire_name(wire, id.appearance()),
                id,
                "{wire} did not resolve back to itself"
            );
        }
        assert_eq!(seen.len(), BuiltinThemeId::ALL.len());
    }

    /// 展示名不能重复：主题画廊卡片与选择器清单都按它显示。
    #[test]
    fn theme_labels_are_unique() {
        let mut seen = HashSet::new();
        for id in BuiltinThemeId::ALL {
            assert!(seen.insert(id.label()), "duplicate label: {}", id.label());
        }
    }

    /// 未知 wire 名回落到该明暗槽位的默认主题，不 panic。
    #[test]
    fn unknown_wire_name_falls_back_to_the_slot_default() {
        use gpui_component::ThemeMode;
        assert_eq!(
            BuiltinThemeId::from_wire_name("nonsense", ThemeMode::Dark),
            BuiltinThemeId::default_for(ThemeMode::Dark)
        );
        assert_eq!(
            BuiltinThemeId::from_wire_name("nonsense", ThemeMode::Light),
            BuiltinThemeId::default_for(ThemeMode::Light)
        );
    }

    /// 悬停底色必须是**强调色薄层**：色相与主色一致、透明度落在可见但不抢选中态的区间。
    ///
    /// 这条规范此前被绕过：`hover_tint` 若退回中性灰（或透明度归零），界面上「悬停没有主题色」
    /// 的回归不会报错、只在人眼里。深浅两档都钉住。
    #[test]
    fn hover_tint_is_a_thin_accent_layer() {
        use gpui_component::ThemeMode;

        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let builtin = BuiltinThemeId::default_for(mode);
            let base = base_tokens(builtin, builtin.preview().primary);
            let extended = extended_tokens(&base, mode);
            let primary = base.colors.primary;

            for (name, color) in [
                ("hover_tint", extended.colors.hover_tint),
                ("hover_tint_press", extended.colors.hover_tint_press),
            ] {
                assert!(
                    color.a > 0.05 && color.a <= 0.3,
                    "{mode:?} 的 {name} 透明度应在 (0.05, 0.3]：{color:?}"
                );
                assert!(
                    (color.h - primary.h).abs() < 1e-4,
                    "{mode:?} 的 {name} 色相应与主色一致（primary={primary:?}, {name}={color:?}）"
                );
            }
            assert!(
                extended.colors.hover_tint.a < extended.colors.hover_tint_press.a,
                "{mode:?} 按下档应比悬停档更浓"
            );
        }
    }

    /// 不透明度阶梯必须全部落在 (0, 1) 且按声明顺序严格递增。
    ///
    /// 这些档位被各处按「越弱越小」使用：一旦某个值被改成 0 或越界，弱化效果会静默
    /// 消失（禁用图标仍全亮、示意条仍实心），只在人眼里；顺序错乱则语义反了。
    #[test]
    fn opacity_scale_ascends_within_valid_range() {
        use gpui_component::ThemeMode;

        let builtin = BuiltinThemeId::default_for(ThemeMode::Dark);
        let base = base_tokens(builtin, builtin.preview().primary);
        let opacity = extended_tokens(&base, ThemeMode::Dark).opacity;

        let scale = [
            ("subtle", opacity.subtle),
            ("tint", opacity.tint),
            ("scrim", opacity.scrim),
            ("muted", opacity.muted),
            ("faint", opacity.faint),
            ("hover_border", opacity.hover_border),
            ("disabled", opacity.disabled),
            ("border", opacity.border),
            ("focus_border", opacity.focus_border),
            ("dim", opacity.dim),
        ];
        for (name, value) in scale {
            assert!(
                value > 0. && value < 1.,
                "{name} 应落在开区间 (0, 1) 内：{value}"
            );
        }
        for window in scale.windows(2) {
            let (prev_name, prev) = window[0];
            let (next_name, next) = window[1];
            assert!(
                prev < next,
                "不透明度阶梯应递增：{prev_name}({prev}) 不应 ≥ {next_name}({next})"
            );
        }
    }

    /// 主色按钮对文字必须在**明暗两档**都达 WCAG AA（正文 4.5:1）。
    ///
    /// 这条此前只在亮色成立：守卫被写进 `if mode == ThemeMode::Light` 分支，暗色下
    /// 默认强调色 `#3B82F6` 配白字实测约 3.68:1。逐预设 × 极端强调色把它钉死。
    #[test]
    fn primary_button_text_meets_aa_in_both_modes() {
        let accents = [
            0x3B82F6u32, // 默认蓝
            0xEF4444,    // 红
            0x22C55E,    // 绿
            0xFDE047,    // 亮黄（走「深字」分支）
            0x111827,    // 近黑
            0xE5E7EB,    // 近白
        ];
        for id in BuiltinThemeId::ALL {
            for value in accents {
                let base = base_tokens(id, color(value));
                let ratio = contrast_ratio(
                    relative_luminance(base.colors.primary),
                    relative_luminance(base.colors.primary_foreground),
                );
                assert!(
                    ratio >= 4.5 - 1e-3,
                    "{id:?} + #{value:06X}: 主色按钮文字对比度 {ratio:.2} 低于 AA 4.5"
                );
            }
        }
    }

    /// 暗色默认主题 + 默认强调色是这条回归的原始反例，单列一条并同时验证
    /// 危险色按钮底色也能被收敛（它此前恒为纯白前景 + 原色红底，约 3.76:1）。
    #[test]
    fn default_dark_theme_button_surfaces_are_readable() {
        use gpui_component::ThemeMode;

        let builtin = BuiltinThemeId::default_for(ThemeMode::Dark);
        let base = base_tokens(builtin, crate::default_accent());
        let colors = base.colors;

        for (name, ratio) in [
            (
                "primary",
                contrast_ratio(
                    relative_luminance(colors.primary),
                    relative_luminance(colors.primary_foreground),
                ),
            ),
            (
                "destructive",
                contrast_ratio(
                    relative_luminance(button_surface(
                        colors.destructive,
                        colors.destructive_foreground,
                    )),
                    relative_luminance(colors.destructive_foreground),
                ),
            ),
        ] {
            assert!(
                ratio >= 4.5 - 1e-3,
                "暗色默认主题的 {name} 按钮文字对比度 {ratio:.2} < 4.5"
            );
        }
    }

    /// 密度三档必须**逐字段严格递增**，且 `title_bar` 只随档、不随缩放。
    ///
    /// 两个真回归面：① 任一尺寸在三档间塌成同高，「紧凑 / 宽松」在界面上就没有区别；
    /// ② `title_bar` 一旦跟着 `scale` 走，交通灯就脱离标题栏垂直居中（macOS 上是可见偏移），
    /// 所以它必须**只**由密度档决定。
    #[test]
    fn density_tiers_are_ordered_and_the_title_bar_ignores_scale() {
        let c = density_tokens(Density::Compact, 1.);
        let n = density_tokens(Density::Normal, 1.);
        let f = density_tokens(Density::Comfortable, 1.);

        for (name, compact, normal, comfortable) in [
            ("control", c.control, n.control, f.control),
            ("nav_row", c.nav_row, n.nav_row, f.nav_row),
            (
                "toolbar_button",
                c.toolbar_button,
                n.toolbar_button,
                f.toolbar_button,
            ),
            ("title_bar", c.title_bar, n.title_bar, f.title_bar),
            ("status_bar", c.status_bar, n.status_bar, f.status_bar),
            (
                "status_control",
                c.status_control,
                n.status_control,
                f.status_control,
            ),
            (
                "section_header",
                c.section_header,
                n.section_header,
                f.section_header,
            ),
            ("task_row", c.task_row, n.task_row, f.task_row),
            (
                "task_row_compact",
                c.task_row_compact,
                n.task_row_compact,
                f.task_row_compact,
            ),
            ("check_mark", c.check_mark, n.check_mark, f.check_mark),
            (
                "activity_rail",
                c.activity_rail,
                n.activity_rail,
                f.activity_rail,
            ),
            (
                "activity_tile",
                c.activity_tile,
                n.activity_tile,
                f.activity_tile,
            ),
            (
                "activity_button",
                c.activity_button,
                n.activity_button,
                f.activity_button,
            ),
        ] {
            assert!(
                compact < normal && normal < comfortable,
                "{name}: 紧凑 {compact:?} < 标准 {normal:?} < 宽松 {comfortable:?} 必须严格递增"
            );
        }

        for density in Density::ALL {
            assert_eq!(
                density_tokens(density, 1.).title_bar,
                density_tokens(density, 2.).title_bar,
                "{density:?} 的 title_bar 不能随界面缩放变"
            );
        }
    }

    /// 除 `title_bar` 外，其余尺寸一律按 `scale` 等比缩放。
    ///
    /// 「界面缩放」与「界面密度」是两条独立轴：缩放是等比放大，密度改档位关系；两者叠加。
    /// 这条钉住前者的等比性——若某个尺寸漏乘 `scale`，叠加后它就不会跟着界面缩放走。
    #[test]
    fn density_scale_grows_every_size_but_the_title_bar() {
        let one = density_tokens(Density::Normal, 1.);
        let two = density_tokens(Density::Normal, 2.);

        for (name, a, b) in [
            ("control", one.control, two.control),
            ("nav_row", one.nav_row, two.nav_row),
            ("status_bar", one.status_bar, two.status_bar),
            ("section_header", one.section_header, two.section_header),
            ("task_row", one.task_row, two.task_row),
            ("check_mark", one.check_mark, two.check_mark),
            ("activity_rail", one.activity_rail, two.activity_rail),
        ] {
            assert_eq!(b, a * 2., "{name} 应随界面缩放翻倍");
        }
        assert_eq!(two.title_bar, one.title_bar, "title_bar 不随界面缩放变");
    }
}
