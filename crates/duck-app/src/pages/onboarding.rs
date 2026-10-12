//! 首启引导：三步向导窗口（欢迎 → 外观 → 插件市场）。
//!
//! # 只在第一次开，但随时能再打开
//!
//! 「见过没有」是一个 `kv` 键（[`KEY_DONE`]），默认没有 = 没见过。写它只在
//! 「完成」与「跳过」两处发生——**中途关掉窗口不算见过**，下次启动还会出来。
//!
//! # 为什么是独立窗口而不是盖在主窗口上的蒙层
//!
//! 主窗口的 shell 由 `duck-shell` 布局（活动栏 + 顶栏 + 路由），在它上面盖一层
//! 蒙层要动 shell 的渲染链；而辅助窗口这条路已经有三处先例（设置 / 日志 / 插件中心），
//! 句柄收在 [`Desktop`] 里，开关与复用都是既有机制。向导本身与「从哪个窗口进来的」无关，
//! 天然适合独立窗口。
//!
//! 它**不替代主窗口**：先把主窗口开起来，再把向导开在它上面。这样首启时
//! 「上次异常退出」的通知（投给主窗口）与更新检查都照常走，不需要为引导特判。

use duck_components::{
    ButtonVariant, LOGO_ASSET_PATH, body_text, button, dropdown_select, meta_text, option_group,
    option_row, segmented_tabs,
};
use duck_shell::AuxiliaryWindowView;
use duck_theme::{Locale, ThemePreference, active_theme, t, tr, trf, update_appearance};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Div, InteractiveElement as _, IntoElement,
    ParentElement, Render, SharedString, Styled, Window, WindowBounds, div, img, px, size,
};
use gpui_component::{Root, h_flex, scroll::ScrollableElement as _, v_flex};

use crate::app::Desktop;

/// 引导是否已完成的 `kv` 键。
///
/// 只有 [`mark_done`] 会写它，只有 [`done`] 会读它。空字符串与缺失都算「没见过」。
pub const KEY_DONE: &str = "onboarding_done";

/// 向导步数（与 [`OnboardingView::step_body`] 的分支数同源）。
const STEPS: usize = 3;

/// 窗口初始尺寸。
const WINDOW_SIZE: gpui::Size<gpui::Pixels> = size(px(680.), px(520.));
/// 窗口最小尺寸。
const WINDOW_MIN: gpui::Size<gpui::Pixels> = size(px(560.), px(440.));
/// 内容区最大宽度：向导是「一次读一件事」，不该在大屏上摊成一行长文。
const CONTENT_MAX_WIDTH: f32 = 520.;
/// 品牌图标尺寸。
const LOGO_SIZE: f32 = 72.;
/// 品牌图标圆角。
const LOGO_RADIUS: f32 = 16.;
/// 步骤指示点的直径。
const DOT_SIZE: f32 = 6.;

/// 是否已完成过引导。
#[must_use]
pub fn done(cx: &App) -> bool {
    duck_store::kv_get(cx, KEY_DONE).as_deref() == Some("true")
}

/// 标记引导已完成（「完成」与「跳过」两处调用）。
///
/// 写完立即 `flush`：`kv` 是 400 ms 去抖落盘的，而「点完完成就退出应用」是
/// 正常操作——不 flush 的话这次写会跟着进程一起消失，下次开又弹一遍。
fn mark_done(cx: &App) {
    duck_store::kv_put(cx, KEY_DONE, "true");
    duck_store::flush(cx);
}

/// 打开（或聚焦）引导窗口。
///
/// 「设置 → 关于」的「重新查看引导」也走这里：它不区分「第一次」与「再看一遍」，
/// 两者的行为本来就是同一个。
pub fn open_onboarding(cx: &mut App) {
    cx.defer(open_or_focus);
}

/// 窗口已开着则前置激活，否则新建（路径与设置 / 日志 / 许可窗口一致）。
fn open_or_focus(cx: &mut App) {
    if focus_open(cx) {
        return;
    }

    let mut options = duck_shell::auxiliary_window_options(SharedString::from(tr(cx, "引导")));
    options.window_bounds = Some(WindowBounds::Windowed(Bounds::centered(
        None,
        WINDOW_SIZE,
        cx,
    )));
    options.window_min_size = Some(WINDOW_MIN);

    match cx.open_window(options, |window, cx| {
        let content = cx.new(|_| OnboardingView::new());
        let chrome = cx.new(|cx| {
            // 占位标题：真实文案由 with_title_fn 按语言解析，此处不会被渲染。
            AuxiliaryWindowView::new("Welcome", content.into(), cx)
                .with_title_fn(|cx| SharedString::from(tr(cx, "引导")))
        });
        crate::app::observe_system_appearance(window);
        cx.new(|cx| Root::new(chrome, window, cx))
    }) {
        Ok(handle) => {
            *Desktop::onboarding_window(cx) = Some(handle);
            cx.activate(true);
        }
        Err(error) => crate::app::report_window_failure("引导窗口", &error, cx),
    }
}

/// 已开着的引导窗口前置激活；没有、或已经关掉了则返回 `false`（顺手清掉死句柄）。
///
/// 形状照 `pages::logs::focus_open_logs`（`let ... else` + 独立 `if`，
/// 而不是会被 clippy 判为「可折叠」的 `if let { if ... }`）。
fn focus_open(cx: &mut App) -> bool {
    let Some(handle) = *Desktop::onboarding_window(cx) else {
        return false;
    };
    if handle
        .update(cx, |_, window, _| window.activate_window())
        .is_ok()
    {
        cx.activate(true);
        true
    } else {
        *Desktop::onboarding_window(cx) = None;
        false
    }
}

/// 向导视图：只持有「当前第几步」，其余都从全局状态读。
struct OnboardingView {
    /// 当前步骤下标（`0..STEPS`）。
    step: usize,
}

impl OnboardingView {
    fn new() -> Self {
        Self { step: 0 }
    }

    /// 收尾：标记完成并关掉自己的窗口。
    ///
    /// 「完成」与「跳过」都落到这里——**跳过也要写标记**，否则下次启动还会弹，
    /// 那不叫跳过，叫稍后再说。
    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        mark_done(cx);
        tracing::info!(step = self.step, "onboarding finished");
        // 关自己：句柄留在 `Desktop` 里，下次 `open_or_focus` 会走「已关闭」分支重开。
        window.remove_window();
    }

    /// 第一步：欢迎。
    fn welcome_body(cx: &App) -> AnyElement {
        let tokens = active_theme(cx).tokens().clone();
        let bullets = [
            "工具箱：把常用小工具收进同一个窗口，启停与设置都在「设置 → 工具」。",
            "命令面板：用键盘完成导航、切换与常用动作。",
            "插件：在插件市场挑选并安装新工具，装完就在工具箱里。",
        ];
        v_flex()
            .w_full()
            .gap(tokens.spacing.md)
            .children(bullets.map(|line| {
                h_flex()
                    .items_start()
                    .gap(tokens.spacing.sm)
                    .child(
                        // 圆点用正文色的强调版：不抢眼，但让三条读起来是三条。
                        div()
                            .flex_none()
                            // 相对首行行盒垂直居中（`notify.rs` 同款算法）——是字形几何校正，
                            // 不属 spacing 阶梯，所以写成派生式而非裸 px(6)。
                            .mt((tokens.typography.sm.line_height - px(DOT_SIZE)) / 2.)
                            .size(px(DOT_SIZE))
                            .rounded_full()
                            .bg(tokens.colors.muted_foreground),
                    )
                    .child(body_text(tr(cx, line), cx))
                    .into_any_element()
            }))
            .into_any_element()
    }

    /// 第二步：语言与明暗。
    ///
    /// 两个控件都直接写 `Appearance`（`update_appearance` 会重建主题、刷新全部窗口、
    /// 落盘），不设中间状态——所以在这一步里改完，主窗口当场就变。
    fn appearance_body(cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let lang = theme.lang();
        let appearance = theme.appearance().clone();

        let current_locale = appearance.language;
        let locale_labels: Vec<SharedString> = Locale::ALL
            .iter()
            .map(|locale| SharedString::from(t(lang, locale.label())))
            .collect();
        let locale_index = Locale::ALL
            .iter()
            .position(|locale| *locale == current_locale)
            .unwrap_or(0);

        let language_row = option_row(
            tr(cx, "语言"),
            Some(tr(cx, "跟随系统时随系统语言切换。").into()),
            segmented_tabs(
                "onboarding-language",
                locale_labels,
                locale_index,
                |index, _, cx| {
                    if let Some(locale) = Locale::ALL.get(index) {
                        let locale = *locale;
                        update_appearance(cx, move |appearance| appearance.language = locale);
                    }
                },
                cx,
            ),
            cx,
        );

        // 选项值与「设置 → 外观」同一个 wire 名，避免同一件事两套字面量。
        let mode_options = [
            ("system", tr(cx, "跟随系统")),
            ("light", tr(cx, "亮色")),
            ("dark", tr(cx, "暗色")),
        ]
        .map(|(value, label)| (SharedString::from(value), SharedString::from(label)));
        let mode_row = option_row(
            tr(cx, "主题模式"),
            None,
            dropdown_select(
                "onboarding-theme-mode",
                mode_options,
                appearance.preference.wire_name(),
                |value, _, cx| {
                    let preference = match value.as_ref() {
                        "light" => ThemePreference::Light,
                        "dark" => ThemePreference::Dark,
                        _ => ThemePreference::System,
                    };
                    update_appearance(cx, move |appearance| appearance.preference = preference);
                },
                cx,
            ),
            cx,
        );

        option_group(
            [language_row.into_any_element(), mode_row.into_any_element()],
            cx,
        )
        .into_any_element()
    }

    /// 第三步：插件市场。
    fn market_body(cx: &App) -> AnyElement {
        let tokens = active_theme(cx).tokens().clone();
        v_flex()
            .w_full()
            .gap(tokens.spacing.md)
            .child(body_text(
                tr(
                    cx,
                    "内置工具已下线，能力全部来自插件。装好的插件会出现在工具箱的侧栏分类里。",
                ),
                cx,
            ))
            .child(
                h_flex().child(
                    button(
                        "onboarding-open-market",
                        tr(cx, "打开插件中心"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(|_, _, cx| crate::pages::plugin_center::open_plugin_center(cx)),
                ),
            )
            .into_any_element()
    }

    /// 当前步骤的正文。
    fn step_body(&self, cx: &App) -> AnyElement {
        match self.step {
            1 => Self::appearance_body(cx),
            2 => Self::market_body(cx),
            _ => Self::welcome_body(cx),
        }
    }

    /// 当前步骤的标题与说明。
    fn step_heading(&self, cx: &App) -> (&'static str, &'static str) {
        match self.step {
            1 => (
                tr(cx, "选择外观"),
                tr(cx, "语言与明暗都能随时在「设置 → 外观」里改。"),
            ),
            2 => (
                tr(cx, "从插件市场开始"),
                tr(cx, "这一步是可选的，之后可以在左边的插件中心随时回来。"),
            ),
            _ => (
                tr(cx, "欢迎使用 Duck Toolbox"),
                tr(
                    cx,
                    "Duck Toolbox 是一个基于 GPUI 构建的桌面工具箱外壳：把常用小工具收进同一个窗口，能力由插件扩展。",
                ),
            ),
        }
    }

    /// 页脚：左侧步骤指示与「跳过」，右侧前进 / 返回。
    fn footer(&self, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();
        let colors = tokens.colors;
        let accent = active_theme(cx).appearance().accent;
        let last = self.step + 1 == STEPS;

        let dots = h_flex()
            .gap(tokens.spacing.xs)
            .children((0..STEPS).map(|index| {
                div()
                    .size(px(DOT_SIZE))
                    .rounded_full()
                    .bg(if index == self.step {
                        accent
                    } else {
                        colors.border
                    })
            }));

        let mut right = h_flex().gap(tokens.spacing.sm);
        if self.step > 0 {
            right = right.child(
                button(
                    "onboarding-back",
                    tr(cx, "上一步"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.step = view.step.saturating_sub(1);
                    cx.notify();
                })),
            );
        }
        right = right.child(
            button(
                "onboarding-next",
                if last {
                    tr(cx, "完成")
                } else {
                    tr(cx, "下一步")
                },
                ButtonVariant::Primary,
                cx,
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                if last {
                    view.finish(window, cx);
                } else {
                    view.step = (view.step + 1).min(STEPS - 1);
                    cx.notify();
                }
            })),
        );

        h_flex()
            .w_full()
            .flex_none()
            .justify_between()
            .items_center()
            .px(tokens.spacing.lg)
            .py(tokens.spacing.md)
            .border_t_1()
            .border_color(active_theme(cx).extended().colors.hairline)
            .child(
                h_flex()
                    .gap(tokens.spacing.md)
                    .items_center()
                    .child(dots)
                    .child(meta_text(
                        trf(
                            cx,
                            "第 {} 步，共 {} 步",
                            &[&(self.step + 1).to_string(), &STEPS.to_string()],
                        ),
                        cx,
                    ))
                    .child(
                        button("onboarding-skip", tr(cx, "跳过"), ButtonVariant::Ghost, cx)
                            .on_click(cx.listener(|view, _, window, cx| view.finish(window, cx))),
                    ),
            )
            .child(right)
    }
}

impl Render for OnboardingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let colors = tokens.colors;
        let (title, description) = self.step_heading(cx);

        let mut content = v_flex().w_full().gap(tokens.spacing.lg);
        // 整段引导只有第一屏需要「这是谁」，所以品牌图标只挂在第一步。
        if self.step == 0 {
            content = content.child(
                div()
                    .size(px(LOGO_SIZE))
                    .rounded(px(LOGO_RADIUS))
                    .overflow_hidden()
                    .child(img(LOGO_ASSET_PATH).size_full()),
            );
        }
        content = content
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(
                        div()
                            .text_size(tokens.typography.xl.size)
                            .line_height(tokens.typography.xl.line_height)
                            .text_color(colors.foreground)
                            .child(title),
                    )
                    .child(body_text(description, cx).text_color(colors.muted_foreground)),
            )
            .child(self.step_body(cx));

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(colors.surface)
            .child(
                div()
                    .id("onboarding-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .justify_center()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.xl)
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .child(content),
                    ),
            )
            .child(self.footer(cx))
    }
}
