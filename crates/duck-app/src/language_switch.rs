//! 顶栏语言切换：地球图标 + 当前模式文案 + 下拉（跟随系统 / 简体中文 / English）。
//!
//! 挂在 shell 的顶栏右侧插槽（[`duck_shell::ShellView::with_title_bar_trailing`]），
//! 所有路由共用一份。取舍与设置页「语言」一致：直接读写 `Appearance.language`
//! （`update_appearance` 会重建主题、刷新全部窗口并落盘），不另设中间状态。
//!
//! 插槽是**闭包**（每帧求值），所以模式文案、下拉的勾选位与提示语都跟着实时语言走。

use duck_components::{DuckIcon, toolbar_dropdown_button};
use duck_theme::{Locale, active_theme, t, tr, update_appearance};
use gpui::{
    Anchor, AnyElement, App, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};
use gpui_component::{Icon, h_flex, menu::PopupMenuItem, tooltip::Tooltip};

/// 语言切换按钮的稳定 id（顶栏插槽与提示语共用）。
const ID: &str = "shell-language-switch";

/// 当前语言模式的文案：`自动`（跟随系统）/ `中文` / `英文`。
///
/// 按钮上直接显示完整词（跟随界面语言翻译），与下拉里的
/// 「跟随系统 / 简体中文 / English」是两套写法：下拉要的是可选项的全称，
/// 按钮要的是当前模式的简称，所以不直接复用 [`Locale::label`]。
fn mode_text(current: Locale, lang: duck_theme::Lang) -> &'static str {
    match current {
        Locale::System => t(lang, "自动"),
        Locale::ZhHans => t(lang, "中文"),
        Locale::En => t(lang, "英文"),
    }
}

/// 顶栏右侧的语言切换控件。
pub fn language_switch(cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let lang = theme.lang();
    let current = theme.appearance().language;
    let label = SharedString::from(tr(cx, "语言"));
    let mode = mode_text(current, lang);

    div()
        .id("shell-language-switch-tooltip")
        .flex_none()
        // 顶栏整条是拖动区：不拦下鼠标按下，点一下会变成拖窗口。
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .tooltip({
            let label = label.clone();
            move |window, cx| Tooltip::new(label.clone()).build(window, cx)
        })
        .child(toolbar_dropdown_button(
            ID,
            label,
            h_flex()
                .items_center()
                .gap(px(3.))
                .child(Icon::new(DuckIcon::Globe).size(theme.extended().icon.md))
                // 模式文案用 xs 档：与图标等高协调，色随按钮的 muted_foreground。
                .child(
                    div()
                        .text_size(theme.tokens().typography.xs.size)
                        .child(mode),
                ),
            // 菜单锚在按钮右下角：顶栏贴窗口上缘，向上弹会出界。
            Anchor::BottomRight,
            move |menu, _, _| {
                Locale::ALL.iter().fold(menu, |menu, locale| {
                    let locale = *locale;
                    menu.item(
                        PopupMenuItem::new(t(lang, locale.label()))
                            .checked(locale == current)
                            .on_click(move |_, _, cx| {
                                update_appearance(cx, move |appearance| {
                                    appearance.language = locale;
                                });
                            }),
                    )
                })
            },
            cx,
        ))
        .into_any_element()
}
