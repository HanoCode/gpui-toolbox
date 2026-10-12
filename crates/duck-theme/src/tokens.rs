//! Duck 自有的扩展语义 token 快照。
//!
//! gpui-base 的 [`SemanticThemeTokens`] 提供 Base 语义 token（颜色 / 圆角 / 间距 / 字体 /
//! 阴影）；本模块补上桌面列表 UI 需要的状态色、三级文字色、细分隔线、chrome 区域色，
//! 以及标题 / 注释字号、图标尺寸、线宽与焦点环、控件密度与组件圆角。
//!
//! 复刻自 FluxDown 的 `fluxdown_ui_theme`：同一套派生规则、同一套密度档位，保证观感一致。

use gpui::{Hsla, Pixels};
use gpui_base::TextStyleToken;

use crate::builtin::BuiltinThemeId;

/// 扩展颜色。派生色（`text_tertiary` 等）默认不透明，叠在选中 / 悬停底色上不会出现
/// 透明度叠加色差。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ExtendedColors {
    /// 成功（完成）状态色：只用于小面积图标或圆点，不做大面积填充。
    pub success: Hsla,
    /// 警示色：需要注意但不是错误的状态。
    pub warning: Hsla,
    /// 提示信息色。
    pub info: Hsla,
    /// 三级文字：计数、时间、分区标题、占位等最弱信息。
    pub text_tertiary: Hsla,
    /// 细分隔线：比 `border` 更弱，只用于保留下来的少数结构线。
    pub hairline: Hsla,
    /// 列表行 / 导航项悬停底色。
    pub row_hover: Hsla,
    /// 侧栏与顶栏等「退后」区域的底色（比内容区 `surface` 低一个层级）。
    pub chrome: Hsla,
    /// `chrome` 上导航项的悬停底色。
    pub nav_hover: Hsla,
    /// 悬停底色（强调色薄层）：按钮、列表行、标签、可点文字的悬停反馈统一用它。
    /// 带透明度，可叠在 `surface` / `chrome` / `muted` 各种底上而不串色。
    pub hover_tint: Hsla,
    /// 按下底色：`hover_tint` 的浓一档，与悬停同色系。
    pub hover_tint_press: Hsla,
    /// `chrome` 上导航项的选中底色（中性，强调色只落在选中图标上）。
    pub nav_selected: Hsla,
    /// 选中导航项的文字色（默认正文色）。
    pub nav_selected_foreground: Hsla,
    /// 选中导航项的图标色：强调色，与 `nav_selected` 对比度 ≥ 3:1。
    pub nav_selected_icon: Hsla,
    /// 强调色文字：链接、生效中的状态、活跃计数；与 `surface` 对比度 ≥ 4.5:1。
    pub accent_text: Hsla,
    /// 太阳色：深色模式下「切换外观」等的太阳图标；只在图标小面积上使用。
    pub sun: Hsla,
    /// 输入框光标。
    pub caret: Hsla,
    /// 文本选区底色。
    pub text_selection: Hsla,
    /// 拖拽指示线。
    pub drag_border: Hsla,
    /// 拖放目标区底色。
    pub drop_target: Hsla,
    /// 任务状态文字：下载中。
    pub status_downloading: Hsla,
    /// 任务状态文字：已完成。
    pub status_completed: Hsla,
    /// 任务状态文字：失败。
    pub status_failed: Hsla,
    /// 任务状态文字：已暂停。
    pub status_paused: Hsla,
    /// 任务状态文字：排队中。
    pub status_queued: Hsla,
    /// 进度条轨道。
    pub progress_track: Hsla,
    /// 进度条填充（下载中）。
    pub progress_fill: Hsla,
}

/// 图标尺寸阶梯：全应用只用这三档。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct IconSizes {
    /// 12px：折叠箭头、行内状态点旁的小图标。
    pub sm: Pixels,
    /// 14px：状态栏、按钮内图标、表格内操作。
    pub md: Pixels,
    /// 16px：导航项、文件类型、工具栏主图标。
    pub lg: Pixels,
}

/// 线宽（不随界面缩放）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StrokeTokens {
    /// 1px：描边与分隔线。
    pub thin: Pixels,
    /// 2px：强调描边（选中框、勾选线）。
    pub strong: Pixels,
}

/// 焦点环（不随界面缩放）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FocusRingTokens {
    /// 环本身的线宽。
    pub width: Pixels,
    /// 环与控件之间的留白。
    pub offset: Pixels,
}

/// 不透明度档位：对前景 / 描边 / 衬底做弱化时的统一强度。
///
/// 这些值是按「视觉角色」定档的离散刻度，收敛掉散落在组件里的裸 `opacity(0.x)` 字面量；
/// 数值保持与原字面量一致，避免观感漂移。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OpacityTokens {
    /// 强调色极轻衬底：选中块、标签内铺底（0.08）。
    pub subtle: f32,
    /// 强调色轻衬底：图标底色、卡片点缀（0.12）。
    pub tint: f32,
    /// 模态遮罩层（0.28）。
    pub scrim: f32,
    /// 弱化前景：停用图标、预览示意条（0.35）。
    pub muted: f32,
    /// 次级弱化：次要装饰、示意条（0.4）。
    pub faint: f32,
    /// 悬停描边（强调色中档，0.45）。
    pub hover_border: f32,
    /// 停用控件整体淡出（0.5）。
    pub disabled: f32,
    /// 未选中控件描边（0.55）。
    pub border: f32,
    /// 聚焦描边（0.6）。
    pub focus_border: f32,
    /// 大幅淡出：不可用卡片、占位（0.75）。
    pub dim: f32,
}

/// 扩展 token 快照（已按界面缩放）。
#[derive(Debug, Clone, PartialEq)]
pub struct ExtendedTokens {
    pub colors: ExtendedColors,
    /// 10/13：角标（BETA 标、通知未读数），对应 macOS Caption 1。
    ///
    /// 基线字阶（[`gpui_base::TypographyTokens`]）的最小档是 `xs`(12)；角标要嵌在固定
    /// 尺寸的图形里，比 `xs` 再小一档，故单列在此而不是复用 `xs`。
    pub badge: TextStyleToken,
    /// 26/32：展示级数值（指标卡），对应 macOS Large Title。
    ///
    /// 基线字阶最大档是 `xl`(22)，展示数值需要再大一档；字重由调用方决定
    /// （指标卡用 SEMIBOLD，macOS 的 Large Title 也是「常规 + 强调加粗」）。
    pub display: TextStyleToken,
    pub icon: IconSizes,
    pub stroke: StrokeTokens,
    pub focus_ring: FocusRingTokens,
    pub opacity: OpacityTokens,
}

/// 控件高度阶梯（px，已按界面缩放；`title_bar` 除外）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DensityTokens {
    /// 按钮 / 输入框 / 下拉 / 数字输入统一高度。
    pub control: Pixels,
    /// 侧栏导航行高。
    pub nav_row: Pixels,
    /// 工具栏 / 顶栏图标按钮边长。
    pub toolbar_button: Pixels,
    /// shell 自绘标题栏高度：交通灯按它垂直居中，故不随界面缩放。
    pub title_bar: Pixels,
    /// 状态栏高度。
    pub status_bar: Pixels,
    /// 状态栏内按钮高度。
    pub status_control: Pixels,
    /// 侧栏分区标题行高。
    pub section_header: Pixels,
    /// 任务行高（舒适双行）。
    pub task_row: Pixels,
    /// 任务行高（紧凑单行）。
    pub task_row_compact: Pixels,
    /// 邮件列表行高（发件人 · 主题 · 摘要三行）。
    ///
    /// 单独一档而不是复用 [`DensityTokens::task_row`]：任务行是**双行**（标题 + 说明），
    /// 最紧也要 36；邮件行是三行且带一条时间戳，压在 44 以下会把摘要行裁掉。
    /// 三档 56 / 64 / 76 与设计稿第 ① 屏的 64 对齐（`docs/email-design.md` §4.10）。
    pub mail_row: Pixels,
    /// 复选框边长。
    pub check_mark: Pixels,
    /// 活动栏轨宽（按钮居中 + 左右留白）。
    pub activity_rail: Pixels,
    /// 活动栏每个按钮位所占的行高。
    pub activity_tile: Pixels,
    /// 活动栏按钮边长。
    pub activity_button: Pixels,
}

/// 组件级 token（px，已按界面缩放）。`input` / `dialog` / `menu` / `tooltip` 圆角由
/// gpui-component 库控件直接读取 `radius.md` / `radius.lg`，此处仅供自绘元素对齐。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ComponentTokens {
    pub button_radius: Pixels,
    pub input_radius: Pixels,
    pub dialog_radius: Pixels,
    pub card_radius: Pixels,
    pub badge_radius: Pixels,
    pub nav_item_radius: Pixels,
    pub progress_radius: Pixels,
    pub progress_height: Pixels,
    pub task_row_radius: Pixels,
    pub checkbox_radius: Pixels,
    pub tab_radius: Pixels,
    pub menu_radius: Pixels,
    pub tooltip_radius: Pixels,
}

/// 单个模式完整解析后的运行时主题（已按界面缩放）。
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTheme {
    /// gpui-base Base 语义 token。
    pub base: gpui_base::SemanticThemeTokens,
    pub extended: ExtendedTokens,
    pub density: DensityTokens,
    pub components: ComponentTokens,
}

impl ResolvedTheme {
    /// 从内置预设 + 强调色构造完整主题快照。
    ///
    /// - `scale`（界面缩放）作用于尺寸类 token：圆角 / 间距 / 阴影 / 密度 / 组件圆角 / 图标；
    /// - `font_size` 是正文的绝对 px，换算成相对基线 [`crate::BASE_FONT_SIZE`] 的比例后，
    ///   套用到**整个字阶**（Base 的 xs…xl 与扩展 token 的 badge / display）；
    /// - `density`（界面密度）先决定密度 token 的**基线档位**，再乘 `scale`；
    /// - 三者叠加：文字的最终倍率是 `font_size / 基线 × scale`，与设置页「叠加生效」一致；
    /// - 线宽与焦点环有意**不**缩放（见 [`StrokeTokens`] / [`FocusRingTokens`] 注释）。
    #[must_use]
    pub fn build(
        builtin: BuiltinThemeId,
        accent: Hsla,
        scale: f32,
        font_size: Option<f32>,
        density: crate::manager::Density,
    ) -> Self {
        let mode = builtin.appearance();
        let mut base = crate::builtin::base_tokens(builtin, accent);
        // 扩展 token 的派生色只依赖 Base 的**颜色**，与尺寸缩放无关，故可先算。
        let mut extended = crate::builtin::extended_tokens(&base, mode);
        let scale = if scale > 0. { scale } else { 1. };

        let font_ratio = font_size.map_or(1., |size| {
            let (min, max) = crate::FONT_SIZE_RANGE;
            size.clamp(min, max) / crate::BASE_FONT_SIZE
        });
        let text_scale = font_ratio * scale;
        if (text_scale - 1.).abs() > f32::EPSILON {
            scale_typography(&mut base.typography, text_scale);
            scale_extended_text(&mut extended, text_scale);
        }
        if (scale - 1.).abs() > f32::EPSILON {
            scale_sizes(&mut base, scale);
            scale_extended_icons(&mut extended, scale);
        }

        let mut theme = Self {
            base,
            extended,
            density: crate::builtin::density_tokens(density, scale),
            components: crate::builtin::component_tokens(scale),
        };
        // Base `selection` 与扩展 `colors.textSelection` 同义，统一由后者投影，
        // 保证 Base 选区与 gpui-component 选区同色。
        theme.base.colors.selection = theme.extended.colors.text_selection;
        theme
    }
}

/// 按比例缩放 Base token 中的**尺寸**项（圆角 / 间距 / 阴影）；「全圆」圆角不缩放。
///
/// 文字不在这里：它按 `font_size × ui_scale` 的合成倍率走 [`scale_typography`]。
fn scale_sizes(base: &mut gpui_base::SemanticThemeTokens, scale: f32) {
    let radius = &mut base.radius;
    for value in [
        &mut radius.none,
        &mut radius.sm,
        &mut radius.md,
        &mut radius.lg,
        &mut radius.xl,
    ] {
        *value *= scale;
    }
    let spacing = &mut base.spacing;
    for value in [
        &mut spacing.xxs,
        &mut spacing.xs,
        &mut spacing.sm,
        &mut spacing.md,
        &mut spacing.lg,
        &mut spacing.xl,
        &mut spacing.xxl,
    ] {
        *value *= scale;
    }
    let shadow = &mut base.shadow;
    for layers in [&mut shadow.sm, &mut shadow.md, &mut shadow.lg] {
        for layer in layers.iter_mut() {
            layer.offset.x *= scale;
            layer.offset.y *= scale;
            layer.blur_radius *= scale;
            layer.spread_radius *= scale;
        }
    }
}

/// 按比例缩放 Base 的整条字阶（含等宽档）。
///
/// 之前这里只改 `typography.sm`，导致「正文放大、标题 / 注释 / 等宽不动」的比例失真。
fn scale_typography(typography: &mut gpui_base::TypographyTokens, ratio: f32) {
    for text in [
        &mut typography.xs,
        &mut typography.sm,
        &mut typography.md,
        &mut typography.lg,
        &mut typography.xl,
        &mut typography.mono_md,
    ] {
        text.size *= ratio;
        text.line_height *= ratio;
    }
}

/// 缩放扩展 token 里的文字档（badge / display）——它们不在 Base 字阶内，必须单独跟着走。
fn scale_extended_text(extended: &mut ExtendedTokens, ratio: f32) {
    for text in [&mut extended.badge, &mut extended.display] {
        text.size *= ratio;
        text.line_height *= ratio;
    }
}

/// 缩放扩展 token 的图标档——与文字同比例，避免「字大了图标还是 12/14/16」。
fn scale_extended_icons(extended: &mut ExtendedTokens, scale: f32) {
    for icon in [
        &mut extended.icon.sm,
        &mut extended.icon.md,
        &mut extended.icon.lg,
    ] {
        *icon *= scale;
    }
}
