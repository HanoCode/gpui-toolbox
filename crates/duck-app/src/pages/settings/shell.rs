//! 设置窗口的侧栏与窗口壳辅助构造。

use super::*;

/// 设置窗口标题栏左侧的侧栏折叠按钮。
///
/// 与主窗口 shell 的那个按钮同一规格（同一图标与旋转、同一提示语、同样拦住标题栏拖动）：
/// 同一个动作在两个窗口里不该长得不一样。
pub(super) fn sidebar_toggle(view: WeakEntity<SettingsWindowView>, cx: &App) -> AnyElement {
    let label = SharedString::from(sidebar_label(cx, sidebar_collapsed(&view, cx)));
    let state = view.clone();
    div()
        .id("settings-sidebar-toggle-tooltip")
        .flex_none()
        // 标题栏整条是拖动区：不拦住左键按下，点按钮就变成拖窗口（对齐 shell 的处理）。
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        // 提示语在悬停时才求值：chrome 不随折叠状态重绘，取词不能定格在构建那一刻。
        .tooltip(move |window, cx| {
            Tooltip::new(SharedString::from(sidebar_label(
                cx,
                sidebar_collapsed(&view, cx),
            )))
            .build(window, cx)
        })
        .child(
            toolbar_action_button(
                "settings-sidebar-toggle",
                label,
                Icon::new(DuckIcon::PanelRight)
                    .size(active_theme(cx).extended().icon.md)
                    .rotate(percentage(0.5)),
                false,
                false,
                cx,
            )
            .on_click(move |_, _, cx| {
                if let Ok(()) = state.update(cx, |view, cx| {
                    view.sidebar_collapsed = !view.sidebar_collapsed;
                    cx.notify();
                }) {}
            }),
        )
        .into_any_element()
}

/// 读折叠状态（视图已释放时按展开算——那时按钮也没机会点了）。
pub(super) fn sidebar_collapsed(view: &WeakEntity<SettingsWindowView>, cx: &App) -> bool {
    view.upgrade()
        .is_some_and(|view| view.read(cx).sidebar_collapsed)
}

/// 折叠按钮的提示语：收起时提示「展开侧栏」，反之亦然（源串即键）。
pub(super) fn sidebar_label(cx: &App, collapsed: bool) -> &'static str {
    tr(
        cx,
        if collapsed {
            "展开侧栏"
        } else {
            "收起侧栏"
        },
    )
}
