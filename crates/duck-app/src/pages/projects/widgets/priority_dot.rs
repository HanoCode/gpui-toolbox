//! 优先级色点。
//!
//! 口径按拍板修正：**P0 红 / P1 橙 / P2 蓝 / P3 灰**。参考项目里色点与文字映射是
//! 相反的（`TaskCard.vue:288-295` vs `:527-540`），这里统一。

use duck_project::Priority;
use duck_theme::{active_theme, tr};
use gpui::{AnyElement, App, Hsla, IntoElement, Styled, div};

/// 全部优先级，顺序即下拉与筛选的展示顺序。
///
/// S2 的卡片与详情只按单个任务的优先级取色，还没有「遍历全部优先级」的入口
/// （优先级下拉与筛选 chips 属 S3，设计文档 §7）。
#[allow(dead_code)] // S3 优先级下拉 / 筛选 chips 用
pub(crate) const ALL: [Priority; 4] = [Priority::P0, Priority::P1, Priority::P2, Priority::P3];

/// 优先级色。
///
/// 三档取自语义色（`destructive` / `warning` / `info`），最弱那档取三级文字色 ——
/// 它们本来就是「需要注意 → 一般」这条强度梯度，另造一套色会让主题切换时不一致。
pub(crate) fn priority_color(priority: Priority, cx: &App) -> Hsla {
    let theme = active_theme(cx);
    match priority {
        Priority::P0 => theme.tokens().colors.destructive,
        Priority::P1 => theme.extended().colors.warning,
        Priority::P2 => theme.extended().colors.info,
        Priority::P3 => theme.extended().colors.text_tertiary,
    }
}

/// 优先级名称。
pub(crate) fn priority_label(priority: Priority, cx: &App) -> &'static str {
    match priority {
        Priority::P0 => tr(cx, "紧急"),
        Priority::P1 => tr(cx, "高"),
        Priority::P2 => tr(cx, "中"),
        Priority::P3 => tr(cx, "低"),
    }
}

/// 色点本身。
///
/// 卡片上只有面积很小的一点，读屏拿不到任何信息 —— 所以调用方必须把
/// [`priority_label`] 放进卡片的无障碍标签里，这里不重复挂文案。
pub(crate) fn priority_dot(priority: Priority, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    // 尺寸从图标档位取半档，不写裸 px：图标档位是随「设置 → 外观 → 字号」缩放的。
    let size = theme.extended().icon.sm / 2.;
    div()
        .size(size)
        .flex_none()
        .rounded_full()
        .bg(priority_color(priority, cx))
        .into_any_element()
}
