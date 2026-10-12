//! 代码编辑器（SQL / 终端旁的源码块）的语法高亮配色。
//!
//! gpui-component 自带一套 Zed 风格的默认 `HighlightTheme`，但它的色相是按库默认
//! 调色板挑的，与本仓预设（强调色、状态色、三级文字）不同源：换主题后会出现在
//! 「深色自定义底 + 库默认浅色关键字」这类串色。
//!
//! 这里把 **Duck 自己的语义 token** 投影成语法角色色，换主题 / 换强调色时高亮跟着走，
//! 不再为编辑器单列一套硬编码色板。
//!
//! 实现约束：gpui-component 的 `ThemeStyle` 字段是私有的、只导出 serde，改色只能经
//! JSON 往返（与 dbflux 的 `recolored_style` 同法）。角色名与 tree-sitter 的 capture
//! 一一对应，SQL 语法用的是 `tree-sitter-sequel`，它的 capture 集合见
//! `queries/highlights.scm`（keyword / keyword.operator / function.call / string /
//! number / float / boolean / comment / operator / punctuation.bracket / type /
//! variable / conditional / storageclass / parameter / field）。
//!
//! ⚠️ 上游 `SyntaxColors` 只认固定的 41 个角色名，名字不在表里就走「第一个点号前缀」
//! 回退（`keyword.operator` → `keyword`、`type.builtin` → `type`）；`conditional` /
//! `storageclass` / `field` / `parameter` 无对应角色，落在默认前景色上——这是上游限制，
//! 不是漏配。

use std::sync::Arc;

use gpui::Hsla;
use gpui_component::Theme as ComponentTheme;
use gpui_component::highlighter::{HighlightTheme, SyntaxColors};

use crate::tokens::ResolvedTheme;

/// 把 `theme` 的语义色写进 `kit.highlight_theme`（编辑器底色 + 语法角色色）。
pub fn apply(kit: &mut ComponentTheme, theme: &ResolvedTheme) {
    let colors = theme.base.colors;
    let extended = theme.extended.colors;
    let mut highlight = (*kit.highlight_theme).clone();

    // 编辑器 chrome：底色与内容区一致，行号用三级文字，当前行一条极浅的 `muted` 带。
    highlight.style.editor_background = Some(colors.background);
    highlight.style.editor_foreground = Some(colors.foreground);
    highlight.style.editor_gutter_background = Some(colors.surface);
    highlight.style.editor_active_line = Some(colors.muted);
    highlight.style.editor_line_number = Some(extended.text_tertiary);
    highlight.style.editor_active_line_number = Some(colors.foreground);
    highlight.style.editor_invisible = Some(extended.text_tertiary);

    let syntax = roles(&colors, &extended, &highlight.style.syntax);
    highlight.style.syntax = syntax;
    kit.highlight_theme = Arc::new(HighlightTheme {
        name: highlight.name.clone(),
        appearance: highlight.appearance,
        style: highlight.style,
    });
}

/// 语法角色表：`(角色名, 颜色, 加粗, 斜体)`。
///
/// 只取语义 token，不引新色：关键字用强调色、字符串用成功色、数字用警示色、
/// 注释用三级文字、类型用信息色。`base` 是当前模式的上游默认色板，未被覆盖的角色
/// 保持它原值（改主题不改色相家族）。
fn roles(
    colors: &gpui_base::ColorTokens,
    extended: &crate::tokens::ExtendedColors,
    base: &SyntaxColors,
) -> SyntaxColors {
    let roles: [(&str, Hsla, bool, bool); 21] = [
        ("keyword", colors.primary, true, false),
        ("function", extended.accent_text, true, false),
        ("type", extended.info, false, false),
        ("constructor", extended.info, false, false),
        ("enum", extended.info, false, false),
        ("attribute", extended.info, false, false),
        ("variant", extended.info, false, false),
        ("string", extended.success, false, false),
        ("string.escape", extended.success, false, false),
        ("string.regex", extended.success, false, false),
        ("string.special", extended.success, false, false),
        ("string.special.symbol", extended.success, false, false),
        ("text.literal", extended.success, false, false),
        ("number", extended.warning, false, false),
        ("boolean", extended.warning, false, false),
        ("constant", extended.warning, false, false),
        ("comment", extended.text_tertiary, false, true),
        ("comment.doc", extended.text_tertiary, false, true),
        ("operator", colors.muted_foreground, false, false),
        ("punctuation", colors.muted_foreground, false, false),
        ("variable", colors.foreground, false, false),
    ];

    // 标点族共享一个色：括号 / 分隔符 / 特殊标点各自独立成键，逐个写。
    let punctuation = [
        ("punctuation.bracket", colors.muted_foreground),
        ("punctuation.delimiter", colors.muted_foreground),
        ("punctuation.special", colors.muted_foreground),
        ("punctuation.list_marker", colors.muted_foreground),
        ("variable.special", colors.foreground),
        ("property", colors.foreground),
        ("primary", colors.foreground),
        ("title", colors.foreground),
        ("label", colors.foreground),
        ("predictive", extended.text_tertiary),
        ("hint", extended.info),
        ("preproc", colors.primary),
        ("tag", colors.primary),
        ("tag.doctype", extended.text_tertiary),
        ("emphasis", colors.foreground),
        ("emphasis.strong", colors.foreground),
    ];

    let mut overrides: Vec<(&str, Hsla, bool, bool)> = roles.to_vec();
    overrides.extend(
        punctuation
            .iter()
            .map(|(name, color)| (*name, *color, false, false)),
    );
    recolor(base, &overrides)
}

/// 在 `base` 上按 `overrides` 改写颜色（可选加粗 / 斜体）。
///
/// `ThemeStyle` 字段私有、只导出 serde，故经 JSON 往返；未列出的角色保持原值。
fn recolor(base: &SyntaxColors, overrides: &[(&str, Hsla, bool, bool)]) -> SyntaxColors {
    let mut value = match serde_json::to_value(base) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "serialize syntax colors failed");
            return base.clone();
        }
    };

    if let Some(object) = value.as_object_mut() {
        for &(name, color, bold, italic) in overrides {
            let mut style = serde_json::Map::new();
            match serde_json::to_value(color) {
                Ok(color) => {
                    style.insert("color".to_string(), color);
                }
                Err(error) => {
                    tracing::warn!(%error, role = name, "serialize syntax color failed");
                    continue;
                }
            }
            if bold {
                style.insert("font_weight".to_string(), 700.into());
            }
            if italic {
                style.insert("font_style".to_string(), "italic".into());
            }
            object.insert(name.to_string(), serde_json::Value::Object(style));
        }
    }

    match serde_json::from_value(value) {
        Ok(colors) => colors,
        Err(error) => {
            tracing::warn!(%error, "deserialize syntax colors failed");
            base.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::recolor;
    use gpui_component::highlighter::HighlightTheme;

    /// 上游默认语法色板：作为测试里「未被覆盖的角色」的基准。
    fn roles_default() -> gpui_component::highlighter::SyntaxColors {
        HighlightTheme::default_light().style.syntax.clone()
    }

    /// 覆盖写进去的角色必须能被 `HighlightStyleResolver` 取到——名字写错时
    /// `SyntaxColors::style` 只会静默返回 `None`（高亮「看起来没生效」）。
    #[test]
    fn overridden_roles_resolve_by_name() {
        let base = roles_default();
        let color = gpui::rgba(0xff00_00ff).into();
        let overridden = recolor(&base, &[("keyword", color, true, false)]);

        let style = overridden.style("keyword").expect("keyword 角色必须有色");
        assert_eq!(style.color, Some(color), "关键字色应取覆盖值");

        // 名字不在表里时上游按前缀回退，`keyword.operator` 命中 `keyword`。
        assert_eq!(
            overridden.style("keyword.operator").map(|style| style.color),
            Some(Some(color))
        );
    }

    /// 未覆盖的角色保持上游默认值，不能被整体清空。
    #[test]
    fn untouched_roles_keep_the_defaults() {
        let base = roles_default();
        let color = gpui::rgba(0x00ff_00ff).into();
        let overridden = recolor(&base, &[("comment", color, false, true)]);

        let default_type = base.style("type");
        assert_eq!(overridden.style("type").map(|s| s.color), default_type.map(|s| s.color));
    }
}
