//! 运行时主题装配：解析明暗 / 强调色 / 缩放，投影到 gpui-component 与 gpui-base，
//! 并把完整 token 快照放进全局供业务读取。

use gpui::{App, Global, Hsla, SharedString, Window, rgb};
use gpui_base::SemanticThemeTokens;
use gpui_component::{Theme as ComponentTheme, ThemeMode};
use serde::{Deserialize, Serialize};

use crate::builtin::BuiltinThemeId;
use crate::i18n::{Lang, Locale};
use crate::tokens::{ComponentTokens, DensityTokens, ExtendedTokens, ResolvedTheme};

/// 默认强调色的 RGB 值（`#3B82F6`，与内置亮色预设的蓝一致）。
///
/// 唯一来源：强调色校验（设置页判断「当前是否仍是默认色」）与预设色点都引用它，
/// 避免同一事实多处字面量。
pub const DEFAULT_ACCENT_RGB: u32 = 0x3B82F6;

/// 默认强调色（`#3B82F6`，与内置亮色预设的蓝一致）。
#[must_use]
pub fn default_accent() -> Hsla {
    Hsla::from(rgb(DEFAULT_ACCENT_RGB))
}

/// 用户主题偏好；`System` 在每次安装时解析当前系统明暗模式。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    /// 偏好 wire 值。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// 解析为具体明暗模式。
    #[must_use]
    pub fn resolve(self, cx: &App) -> ThemeMode {
        match self {
            Self::System => cx.window_appearance().into(),
            Self::Light => ThemeMode::Light,
            Self::Dark => ThemeMode::Dark,
        }
    }
}

/// 界面密度档位：同一套 token 派生规则下，控件高度 / 行高 / 间距的三档疏密。
///
/// 与「界面缩放」是两个独立的轴：缩放是等比放大（0.8~1.5），密度是**改变档位关系**
/// （例如紧凑档下侧栏行高比控件高度更紧）。两者叠加生效。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    /// 紧凑：桌面信息密度优先，控件 24 / 行高 24。
    Compact,
    /// 标准：与历史基线一致，控件 28 / 行高 28。
    #[default]
    Normal,
    /// 宽松：触控与低视力友好，控件 32 / 行高 34。
    Comfortable,
}

impl Density {
    /// 全部档位，按设置页下拉的排列顺序。
    pub const ALL: [Self; 3] = [Self::Compact, Self::Normal, Self::Comfortable];

    /// 存档 / 下拉用的稳定 wire 名。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Normal => "normal",
            Self::Comfortable => "comfortable",
        }
    }

    /// 解析 wire 名；不认识的值回落到标准档（旧存档无此键时的行为）。
    #[must_use]
    pub fn from_wire_name(value: &str) -> Self {
        match value {
            "compact" => Self::Compact,
            "comfortable" => Self::Comfortable,
            _ => Self::Normal,
        }
    }

    /// 设置页下拉的中文源串（走 i18n 取词）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Compact => "紧凑",
            Self::Normal => "标准",
            Self::Comfortable => "宽松",
        }
    }
}

/// 右侧栏停靠在哪一侧。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarSide {
    /// 内容左侧（在活动栏与内容之间）。
    Left,
    /// 内容右侧（默认）。
    #[default]
    Right,
}

impl SidebarSide {
    /// 存档 / 下拉用的稳定 wire 名。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    /// 解析 wire 名；不认识的值回落到默认的右侧。
    #[must_use]
    pub fn from_wire_name(value: &str) -> Self {
        match value {
            "left" => Self::Left,
            _ => Self::Right,
        }
    }

    /// 设置页下拉的中文源串（走 i18n 取词）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "左侧",
            Self::Right => "右侧",
        }
    }
}

/// 侧栏项间距允许范围（px）；`0` 表示贴紧。
pub const NAV_GAP_RANGE: (f32, f32) = (0., 24.);

/// 明暗两个槽位各自选中的内置预设（与 FluxDown 的 `lightTheme` / `darkTheme` 两键同构）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeSlots {
    pub dark: BuiltinThemeId,
    pub light: BuiltinThemeId,
}

impl Default for ThemeSlots {
    fn default() -> Self {
        Self {
            dark: BuiltinThemeId::default_for(ThemeMode::Dark),
            light: BuiltinThemeId::default_for(ThemeMode::Light),
        }
    }
}

impl ThemeSlots {
    /// 取某个明暗槽位当前选中的预设。
    #[must_use]
    pub fn for_mode(&self, mode: ThemeMode) -> BuiltinThemeId {
        match mode {
            ThemeMode::Dark => self.dark,
            ThemeMode::Light => self.light,
        }
    }

    /// 写入某个槽位；预设外观与槽位不符时忽略（亮色预设进不了暗色槽）。
    #[must_use]
    pub fn with(self, mode: ThemeMode, id: BuiltinThemeId) -> Self {
        if id.appearance() != mode {
            return self;
        }
        match mode {
            ThemeMode::Dark => Self { dark: id, ..self },
            ThemeMode::Light => Self { light: id, ..self },
        }
    }
}

/// 外观设置：偏好 + 强调色 + 界面缩放 + 字号 + 语言 + 主题槽位 + 密度与布局。
///
/// 「布局」类字段（密度 / 侧栏项间距 / 组件显隐 / 右栏停靠侧）也放在这里：它们与
/// 颜色 / 字号一样是「用户对界面的选择」，且 `language` 早已是本结构里非 token 的
/// 先例。集中一处意味着只有**一份**持久化键、一条读盘路径、一个设置入口。
#[derive(Debug, Clone, PartialEq)]
pub struct Appearance {
    pub preference: ThemePreference,
    /// 强调色（不透明）。
    pub accent: Hsla,
    /// 界面缩放倍率（0.8 ~ 1.5）。
    pub ui_scale: f32,
    /// 正文字号（px）；`None` 跟随主题基线 13px。
    pub font_size: Option<f32>,
    /// 正文字体族；`None` / 缺失字体跟随主题字体（gpui 自动回退）。
    pub font_family: Option<SharedString>,
    /// 界面语言；`System` 跟随系统语言。
    pub language: Locale,
    /// 明暗槽位选中的内置预设。
    pub theme: ThemeSlots,
    /// 界面密度档位（紧凑 / 标准 / 宽松）。
    pub density: Density,
    /// 侧栏列表项之间的间距（px，0 ~ 24）。
    pub nav_gap: f32,
    /// 是否显示左侧活动栏（功能轨）。
    pub show_activity_bar: bool,
    /// 是否在顶栏显示通知中心铃铛。
    pub show_notification_bell: bool,
    /// 是否在顶栏显示附加入口（插件中心 / 语言切换）。
    pub show_top_bar_extras: bool,
    /// 右侧栏停靠侧。
    pub right_sidebar_side: SidebarSide,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            preference: ThemePreference::System,
            accent: default_accent(),
            ui_scale: 1.,
            font_size: None,
            font_family: None,
            language: Locale::System,
            theme: ThemeSlots::default(),
            density: Density::Normal,
            nav_gap: 4.,
            show_activity_bar: true,
            show_notification_bell: true,
            show_top_bar_extras: true,
            right_sidebar_side: SidebarSide::Right,
        }
    }
}

/// 全局主题状态；由 [`init`] 建立，任何视图创建前必须已存在。
#[derive(Clone)]
pub struct DuckThemeState {
    resolved: ResolvedTheme,
    mode: ThemeMode,
    appearance: Appearance,
    builtin: BuiltinThemeId,
    /// 已解析的界面语言（`System` 偏好在这里落成具体语言）。
    lang: Lang,
}

impl Global for DuckThemeState {}

impl DuckThemeState {
    /// gpui-base Base 语义 token。
    #[must_use]
    pub fn tokens(&self) -> &SemanticThemeTokens {
        &self.resolved.base
    }

    /// 扩展 token（chrome / 状态色 / 图标尺寸 …）。
    #[must_use]
    pub fn extended(&self) -> &ExtendedTokens {
        &self.resolved.extended
    }

    /// 控件密度档位。
    #[must_use]
    pub fn density(&self) -> &DensityTokens {
        &self.resolved.density
    }

    /// 组件圆角 / 尺寸档位。
    #[must_use]
    pub fn components(&self) -> &ComponentTokens {
        &self.resolved.components
    }

    /// 当前生效的明暗模式。
    #[must_use]
    pub fn mode(&self) -> ThemeMode {
        self.mode
    }

    /// 当前外观设置。
    #[must_use]
    pub fn appearance(&self) -> &Appearance {
        &self.appearance
    }

    /// 当前内置主题 id。
    #[must_use]
    pub fn builtin(&self) -> BuiltinThemeId {
        self.builtin
    }

    /// 当前生效的界面语言（`System` 已解析成具体语言）。
    #[must_use]
    pub const fn lang(&self) -> Lang {
        self.lang
    }

    /// 完整解析后的主题快照。
    #[must_use]
    pub fn resolved(&self) -> &ResolvedTheme {
        &self.resolved
    }
}

/// 读取当前完整主题状态。
///
/// 调用方必须先执行 [`init`]；桌面 shell 在创建任何视图前建立这一不变量。
#[must_use]
pub fn active_theme(cx: &App) -> &DuckThemeState {
    cx.global::<DuckThemeState>()
}

/// 在 `gpui_component::init` 之后安装默认主题。
pub fn init(cx: &mut App) {
    install(Appearance::default(), None, cx);
}

/// 修改当前外观设置中的若干字段并立即应用（UI 侧的统一切换入口）。
pub fn update_appearance(cx: &mut App, edit: impl FnOnce(&mut Appearance)) {
    let mut appearance = active_theme(cx).appearance().clone();
    edit(&mut appearance);
    apply(appearance, cx);
}

/// 应用一组外观设置（幂等；与当前状态一致时不重装）。
pub fn apply(appearance: Appearance, cx: &mut App) {
    if cx
        .try_global::<DuckThemeState>()
        .is_some_and(|state| state.appearance == appearance)
    {
        return;
    }
    install(appearance, None, cx);
}

/// 系统外观变化时按当前偏好重新解析 `System`；显式亮 / 暗偏好保持不变。
pub fn sync_system_theme(cx: &mut App) {
    let Some(state) = cx.try_global::<DuckThemeState>() else {
        return;
    };
    if state.appearance.preference == ThemePreference::System
        && ThemePreference::System.resolve(cx) != state.mode
    {
        let appearance = state.appearance.clone();
        install(appearance, None, cx);
    }
}

fn install(appearance: Appearance, _window: Option<&mut Window>, cx: &mut App) {
    let mode = appearance.preference.resolve(cx);
    let builtin = appearance.theme.for_mode(mode);
    let mut theme = ResolvedTheme::build(
        builtin,
        appearance.accent,
        appearance.ui_scale,
        appearance.font_size,
        appearance.density,
    );
    // 本机字体覆盖：缺失的字体由 gpui 按回退链解析，不必校验已安装。
    if let Some(family) = appearance
        .font_family
        .as_deref()
        .filter(|family| !family.trim().is_empty())
    {
        theme.base.typography.sans = SharedString::from(family.to_owned());
    }

    ComponentTheme::change(mode, None, cx);
    project_onto_kit(ComponentTheme::global_mut(cx), &theme);
    ComponentTheme::sync_base(cx);
    {
        let base_theme = gpui_base::Theme::global_mut(cx);
        base_theme.appearance = match mode {
            ThemeMode::Dark => gpui_base::ThemeAppearance::Dark,
            ThemeMode::Light => gpui_base::ThemeAppearance::Light,
        };
        base_theme.tokens = theme.base.clone();
        // 面板分隔把手与其他结构线一致用 hairline。
        base_theme.resizable.handle = Some(theme.extended.colors.hairline);
    }

    // `gpui-component` 自带的那几十条文案（编辑器查找替换条上的「替换 / 全部替换」等）
    // 走它自己的 `rust_i18n` 实例，本仓的英译表碰不到；它公开了 `set_locale`，所以在
    // 这里跟本仓语言一起切。**只切这两种**：它的 `ui.yml` 也只有 en / zh-* 有全文，
    // 别的 tag 会把英文界面切成半截中文。
    gpui_component::set_locale(match appearance.language.resolve() {
        Lang::Zh => "zh-CN",
        Lang::En => "en",
    });

    cx.set_global(DuckThemeState {
        resolved: theme,
        mode,
        lang: appearance.language.resolve(),
        appearance,
        builtin,
    });

    for handle in cx.windows() {
        let _ = handle.update(cx, |_, window, _| window.refresh());
    }
}

/// 同时写 gpui-component 的 legacy 颜色字段与同名 `tokens` 字段（控件分别读两者）。
macro_rules! paint {
    ($kit:ident, $($field:ident = $value:expr),+ $(,)?) => {
        $(
            let value: gpui::Hsla = $value;
            $kit.$field = value;
            $kit.tokens.$field = value.into();
        )+
    };
}

/// 把解析后的 Duck token 投影到 gpui-component 主题。
///
/// `ComponentTheme::change` 按库默认主题推导全部派生色，`apply_semantic_tokens` 只覆盖 17 个
/// Base 色；凡是库控件会读、又应跟随本主题的派生色都必须在这里重写，否则停留在库默认的黑 / 白 / 灰。
fn project_onto_kit(kit: &mut ComponentTheme, theme: &ResolvedTheme) {
    let tokens = &theme.base;
    let colors = tokens.colors;
    let extended = theme.extended.colors;
    kit.apply_semantic_tokens(tokens);
    kit.focus_ring = false;
    // 代码编辑器的语法高亮角色色：走本仓 token，不留在库默认色板上。
    crate::syntax::apply(kit, theme);
    // gpui-component 的 `text_base` 取 `font_size`（默认映射到 md=16px），对桌面密度偏大；
    // 与 13px 正文基线对齐取 `sm`。
    kit.font_size = tokens.typography.sm.size;

    let primary_hover = shift_toward_contrast(colors.primary, 0.08);
    let primary_active = shift_toward_contrast(colors.primary, 0.13);
    let secondary_hover = shift_toward_contrast(colors.secondary, 0.05);
    let secondary_active = shift_toward_contrast(colors.secondary, 0.09);
    // 危险色**按钮底色**同样要满足 AA：原样 `#EF4444` 配白字只有约 3.76:1。
    // 这里只收敛底色，**不动** `colors.destructive` —— 它还被当文字色用
    // （通知的错误色、状态「失败」色），压暗会削弱它在暗背景上的可读性。
    let danger_surface = crate::button_surface(colors.destructive, colors.destructive_foreground);
    let danger_hover = shift_toward_contrast(danger_surface, 0.08);
    let danger_active = shift_toward_contrast(danger_surface, 0.13);
    // 列表不画网格：行分隔线与表头竖线取透明，只靠悬停 / 选中底色区分行。
    let no_line = colors.surface.opacity(0.);

    paint!(
        kit,
        background = colors.surface,
        title_bar = extended.chrome,
        title_bar_border = extended.hairline,
        sidebar = colors.surface,
        sidebar_foreground = colors.surface_foreground,
        sidebar_border = colors.border,
        sidebar_accent = colors.accent,
        sidebar_accent_foreground = colors.accent_foreground,
        sidebar_primary = colors.primary,
        sidebar_primary_foreground = colors.primary_foreground,
        primary_hover = primary_hover,
        primary_active = primary_active,
        secondary_hover = secondary_hover,
        secondary_active = secondary_active,
        danger_hover = danger_hover,
        danger_active = danger_active,
        button_primary = colors.primary,
        button_primary_foreground = colors.primary_foreground,
        button_primary_hover = primary_hover,
        button_primary_active = primary_active,
        button_secondary = colors.secondary,
        button_secondary_foreground = colors.secondary_foreground,
        button_secondary_hover = secondary_hover,
        button_secondary_active = secondary_active,
        button_danger = danger_surface,
        button_danger_foreground = colors.destructive_foreground,
        button_danger_hover = danger_hover,
        button_danger_active = danger_active,
        link = extended.accent_text,
        link_hover = extended.accent_text,
        link_active = extended.accent_text,
        caret = extended.caret,
        selection = extended.text_selection,
        list_active = colors.accent,
        list_active_border = colors.primary,
        progress_bar = extended.progress_fill,
        slider_bar = colors.primary,
        slider_thumb = colors.primary_foreground,
        drag_border = extended.drag_border,
        drop_target = extended.drop_target,
        table = colors.surface,
        table_active = colors.accent,
        table_active_border = colors.primary,
        table_even = colors.surface,
        table_head = colors.surface,
        table_head_foreground = extended.text_tertiary,
        table_hover = extended.hover_tint,
        // 库控件的列表 / 菜单悬停：默认回落 `accent.opacity(0.6)`（过浓），
        // 与全应用统一到同一档薄层。
        list_hover = extended.hover_tint,
        table_row_border = no_line,
    );
}

/// 向对比方向偏移亮度：亮色变暗、暗色变亮（hover / active 派生）。
fn shift_toward_contrast(color: Hsla, amount: f32) -> Hsla {
    let delta = if color.l >= 0.5 { -amount } else { amount };
    Hsla {
        l: (color.l + delta).clamp(0., 1.),
        ..color
    }
}

/// 便利：默认强调色的 ARGB 字面量（`0xRRGGBB`）。
#[must_use]
pub fn accent_from_rgb(value: u32) -> Hsla {
    Hsla::from(rgb(value))
}

#[cfg(test)]
mod tests {
    use super::{BuiltinThemeId, ThemeSlots};
    use gpui_component::ThemeMode;

    /// 明暗两个槽位各自独立：写暗色槽不影响亮色槽（跨明密切换的基础）。
    #[test]
    fn slots_track_each_mode_independently() {
        let slots = ThemeSlots::default()
            .with(ThemeMode::Dark, BuiltinThemeId::HybridDark)
            .with(ThemeMode::Light, BuiltinThemeId::CatppuccinLatte);

        assert_eq!(slots.dark, BuiltinThemeId::HybridDark);
        assert_eq!(slots.light, BuiltinThemeId::CatppuccinLatte);
        assert_eq!(slots.for_mode(ThemeMode::Dark), BuiltinThemeId::HybridDark);
        assert_eq!(
            slots.for_mode(ThemeMode::Light),
            BuiltinThemeId::CatppuccinLatte
        );
    }

    /// 不合槽位的预设被忽略：亮色预设写不进暗色槽（反之亦然）。
    #[test]
    fn slots_reject_mismatched_appearance() {
        let slots = ThemeSlots::default();
        let attempted = slots
            .with(ThemeMode::Dark, BuiltinThemeId::DefaultLight)
            .with(ThemeMode::Light, BuiltinThemeId::DefaultDark);
        assert_eq!(attempted, slots);
    }
}
