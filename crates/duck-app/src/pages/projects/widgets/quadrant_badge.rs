//! 四象限徽标（原型 `.qd`，`docs/projects-mock.html:249-251`）。
//!
//! 原型的 `.qd` 是**实底彩色小方块 + 字母**：卡片 `.ctop` 里 19px、详情概览 18px、
//! 属性下拉 14px。这里做成一格，卡片与详情概览共用同一档尺寸 —— 同一个象限在两个
//! 地方长得不一样，只会让人以为它们是两件事。
//!
//! 底色与优先级色**同源**（原型里 `--q1..--q4` 与 `--p0..--p3` 逐字相等，顺序也一致：
//! Q1 重要且紧急 → 最重），所以这里和 [`super::priority_dot::priority_color`] 用同一套
//! 语义色，主题切换时两者不会各走各的。
//!
//! 字色**不写死白**：深色主题的 Q2 是 `#FFB340`、Q4 是灰，白字压上去读不出来。走
//! [`duck_theme::foreground_for`] 按亮度取近黑或白，与主按钮「底色/字色配对」同一套规则。

use duck_project::Quadrant;
use duck_theme::{active_theme, foreground_for};
use gpui::{
    AnyElement, App, FontWeight, Hsla, IntoElement, ParentElement, SharedString, Styled, div,
};

/// 象限色。
pub(crate) fn quadrant_color(quadrant: Quadrant, cx: &App) -> Hsla {
    let theme = active_theme(cx);
    match quadrant {
        Quadrant::Q1 => theme.tokens().colors.destructive,
        Quadrant::Q2 => theme.extended().colors.warning,
        Quadrant::Q3 => theme.extended().colors.info,
        Quadrant::Q4 => theme.extended().colors.text_tertiary,
    }
}

/// 徽标本体：`Q1`…`Q4`。
pub(crate) fn quadrant_badge(quadrant: Quadrant, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let color = quadrant_color(quadrant, cx);

    div()
        .flex_none()
        // 边长从图标档位推出来（16 × 1.25 = 20，原型 19）：用它而不是写死 px，
        // 「设置 → 外观 → 界面缩放」一动它跟着动，不会在大字号下变成半截方块。
        .size(extended.icon.lg * 1.25)
        .flex()
        .items_center()
        .justify_center()
        // 方角取按钮档（6，原型 5）：`badge_radius` 是 9999 的胶囊，套在方块上会成圆形。
        .rounded(theme.components().button_radius)
        .bg(color)
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .font_weight(FontWeight::BOLD)
        .text_color(foreground_for(color))
        .child(SharedString::from(quadrant.as_str()))
        .into_any_element()
}
