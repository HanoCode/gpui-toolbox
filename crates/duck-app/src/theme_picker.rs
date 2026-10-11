//! 活动栏主题选择器：调色板图标 + 三列主题清单 Popover。
//!
//! 交互对照 `temp/verve` 的 `rail.rs`（用户定稿的样式）：点击活动栏按钮弹出
//! 三列并排的主题名清单（列宽 140px），当前主题高亮，点击即切。清单按
//! [`duck_theme::BuiltinThemeId::ALL`] 的展示名顺序排列，三列按 `div_ceil` 切分。
//!
//! 取舍与语言切换一致：直接写 `Appearance.theme` 槽位（`update_appearance`
//! 会重建主题、刷新全部窗口并落盘）；**用户强调色保持不动**——主题只换
//! 底色 / 文字 / 边框 / 状态色，主色仍来自用户设置（2026-10-10 定稿）。

use duck_theme::{BuiltinThemeId, ThemeMode, ThemePreference, active_theme, update_appearance};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement, ParentElement as _, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Toggled, div, px,
};
use gpui_component::{h_flex, v_flex};

/// 三列清单里单列的宽度（verve `rail.rs` 同值）。
const COLUMN_WIDTH: f32 = 140.;
/// 一列内主题行之间的间隙。
const ITEM_GAP: f32 = 1.;
/// 主题行内边距（verve `rail.rs` 同值：水平 8 / 垂直 4）。
const ITEM_PADDING_X: f32 = 8.;
const ITEM_PADDING_Y: f32 = 4.;
/// 清单列之间的间隙。
const COLUMN_GAP: f32 = 4.;
/// 清单四周内边距。
const LIST_PADDING: f32 = 4.;

/// 主题选择器的 Popover 内容：三列主题名清单。
pub fn popover_content(cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let radius = theme.components().nav_item_radius;
    let current = theme.builtin();

    // 主题清单至少要装得下两个，三分列这条路才永远成立（见下面的 `chunks`）。
    // 用编译期断言而不是注释：清单是个随时可能被删减的常量。
    const _: () = assert!(BuiltinThemeId::ALL.len() >= 2);

    let per = BuiltinThemeId::ALL.len().div_ceil(3);
    let columns = h_flex()
        // 一组「只能选一个」的选项：容器报 `RadioGroup`，行报 `RadioButton`，
        // 读屏才会播报「第几项 / 共几项 / 已选中」。
        .id("theme-picker-list")
        .role(Role::RadioGroup)
        .gap(px(COLUMN_GAP))
        // 用 `chunks` 而不是手写三段下标：手写的 `[per * 2..ALL.len()]` 在主题数只剩一个时
        // `start` 会越过 `end`，直接 panic —— 而这是个纯粹的展示层切片，不该有崩的可能。
        .children(BuiltinThemeId::ALL.chunks(per).map(|column| {
            v_flex()
                .w(px(COLUMN_WIDTH))
                .gap(px(ITEM_GAP))
                .children(column.iter().map(|id| theme_item(*id, current, radius, cx)))
        }))
        .p(px(LIST_PADDING));

    // 底色由 Popover 自带外观提供（跟随主题），这里只负责行内文字与悬停 / 选中态。
    columns.into_any_element()
}

/// 单个主题行：当前主题选中底 + 高亮文字，其余悬停浮现导航悬停色。
fn theme_item(
    id: BuiltinThemeId,
    current: BuiltinThemeId,
    radius: gpui::Pixels,
    cx: &App,
) -> AnyElement {
    let selected = id == current;
    let theme = active_theme(cx);
    let extended = theme.extended().colors;
    let item_text = theme.tokens().typography.xs;
    let text_color = if selected {
        extended.nav_selected_foreground
    } else {
        theme.tokens().colors.muted_foreground
    };
    let hover_color = extended.hover_tint;
    let hover_text = extended.accent_text;
    let ring = theme.tokens().colors.ring;
    let label = SharedString::from(id.label());
    let element_id = SharedString::from(format!("theme-picker-{}", id.wire_name()));

    div()
        .id(element_id)
        // 无障碍：没有 role 的节点**根本不会进 a11y 树**（GPUI 只报有 role 的节点），
        // 所以这一整排主题行此前对读屏是完全不存在的。可访问名必须**显式**给：
        // `gpui_base::test_support` 的 `label()` 只读 `node.label()`
        // （`test_support.rs:318`），**不认**从子节点文字派生的名字 —— 复用下面那句
        // 既有可见文案（主题展示名），不另造串。
        .role(Role::RadioButton)
        .aria_toggled(if selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .aria_label(label.clone())
        .w_full()
        .px(px(ITEM_PADDING_X))
        .py(px(ITEM_PADDING_Y))
        .rounded(radius)
        // 焦点用描边；先占 1px 透明边，描边出现时不产生位移。
        .border_1()
        .border_color(gpui::transparent_black())
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.border_color(ring))
        .text_size(item_text.size)
        .line_height(item_text.line_height)
        .text_color(text_color)
        .when(selected, |row| row.bg(extended.nav_selected))
        // 未选中项悬停：底色薄层 + 文字转强调色（选中项保持自身配色）。
        .when(!selected, |row| {
            row.hover(move |style| style.bg(hover_color).text_color(hover_text))
        })
        .cursor_pointer()
        .child(label)
        .on_click(move |_, _, cx| {
            let mode = id.appearance();
            // 同明暗：只换槽位，保留「跟随系统」；
            // 跨明暗：连偏好一起固定——`install` 只按 `preference` 解析明暗，
            // 不固定的话画面根本不会切换（点了 dark 主题却停在亮色）。
            let resolved = active_theme(cx).mode();
            tracing::debug!(theme = id.wire_name(), ?mode, ?resolved, "theme picked");
            update_appearance(cx, move |appearance| {
                appearance.theme = appearance.theme.with(mode, id);
                if mode != resolved {
                    appearance.preference = match mode {
                        ThemeMode::Dark => ThemePreference::Dark,
                        ThemeMode::Light => ThemePreference::Light,
                    };
                }
            });
        })
        .into_any_element()
}
