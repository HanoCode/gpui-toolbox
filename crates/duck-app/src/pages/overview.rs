//! 概览页：页面标题 + 指标卡 + 快速入口 + 运行环境分组。
//!
//! **页面上没有编出来的数字**：三张指标卡分别读工具注册表、工具使用记录与启动耗时
//! 实测值，读不到就显示空态文案（`—` + 一句说明），而不是摆一个好看的假数。

use duck_components::{
    ButtonVariant, DuckIcon, button, caption_heading, card, option_group, option_row,
    selectable_text, title_text,
};
use duck_theme::{ThemePreference, active_theme, tr, trf};
use gpui::{
    App, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement, Render,
    StatefulInteractiveElement as _, Styled, Window, div,
};
use gpui_component::{h_flex, v_flex};

use crate::settings;
use duck_tools::{ToolRegistry, settings as tool_settings};

/// 「最近使用」的统计窗口（天）。
const RECENT_WINDOW_DAYS: i64 = 7;

/// 项目主页：文档就落在这里（README 与 `docs/`）。
///
/// 与 `git remote` 一致（`HanoCode/gpui-toolbox`）—— 改仓库名/迁移时这里要一起改。
const DOCS_URL: &str = "https://github.com/HanoCode/gpui-toolbox#readme";

/// 概览页。
pub struct OverviewPage;

impl OverviewPage {
    /// 创建概览页。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// 指标卡：标签 + 数值 + 说明。
fn stat_card(label: &str, value: &str, delta: &str, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    card(cx)
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.sm)
        .p(tokens.spacing.md)
        .child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(label.to_owned()),
        )
        .child(
            div()
                .text_size(extended.display.size)
                .line_height(extended.display.line_height)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.colors.foreground)
                .child(value.to_owned()),
        )
        .child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(extended.colors.accent_text)
                .child(delta.to_owned()),
        )
}

/// 分区标题。
fn section_title(text: &str, cx: &App) -> Div {
    caption_heading(text, cx)
}

impl Render for OverviewPage {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let muted = tokens.colors.muted_foreground;

        let header = v_flex()
            .gap(tokens.spacing.xs)
            .child(title_text(tr(cx, "概览"), cx))
            .child(
                div()
                    .text_size(tokens.typography.sm.size)
                    .line_height(tokens.typography.sm.line_height)
                    .text_color(muted)
                    .child(selectable_text(tr(
                        cx,
                        "Duck Toolbox 的示例外壳：自绘标题栏、活动栏、可拖拽侧栏、工具栏与状态栏。",
                    ))),
            );

        // ── 指标卡：三个数都来自真实记录 ──────────────────────────────────
        let tools = ToolRegistry::visible(cx);
        let tool_count = tools.len();
        let category_count = ToolRegistry::present_categories(cx).len();

        // 「累计执行」= 各工具使用次数之和（注册表里没有的工具不计）。
        let runs: u64 = tools
            .iter()
            .map(|tool| tool_settings::runs(cx, tool.id()))
            .sum();
        // 「近 7 天用过」以 `recent` 表的时间戳为准：数的是**动过的工具数**，不是次数
        // （那张表每工具一条，见 `duck_store::Store::recent_since` 的说明）。
        let recent_count = ToolRegistry::recent_within_days(cx, RECENT_WINDOW_DAYS).len();

        let recent_note = if recent_count == 0 {
            trf(
                cx,
                "{} 天内还没用过任何工具",
                &[&RECENT_WINDOW_DAYS.to_string()],
            )
        } else {
            trf(
                cx,
                "{} 天内用过 {} 个",
                &[&RECENT_WINDOW_DAYS.to_string(), &recent_count.to_string()],
            )
        };

        // 启动耗时：没有样本时摆 `—` 与一句说明，绝不摆一个编出来的毫秒数。
        let (startup_value, startup_note) = match settings::startup_stats(cx) {
            Some((samples, average)) => (
                format!("{average} ms"),
                trf(cx, "最近 {} 次启动的平均值", &[&samples.to_string()]),
            ),
            None => ("—".to_owned(), tr(cx, "本次启动后开始记录").to_owned()),
        };

        let stats = h_flex()
            .w_full()
            .gap(tokens.spacing.md)
            .child(stat_card(
                tr(cx, "已安装工具"),
                &tool_count.to_string(),
                &trf(cx, "分布在 {} 个分类", &[&category_count.to_string()]),
                cx,
            ))
            .child(stat_card(
                tr(cx, "累计执行"),
                &runs.to_string(),
                &recent_note,
                cx,
            ))
            .child(stat_card(
                tr(cx, "平均启动耗时"),
                &startup_value,
                &startup_note,
                cx,
            ));

        let quick_actions = v_flex()
            .gap(tokens.spacing.sm)
            .child(section_title(tr(cx, "快速入口"), cx))
            .child(
                h_flex()
                    .gap(tokens.spacing.sm)
                    .child(
                        button(
                            "overview-import",
                            tr(cx, "导入配置"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        // 以前这里只弹一句「尚未接入业务逻辑」。数据页落地后它就是真入口，
                        // 直接跳过去——一个能用的按钮比一句解释好。
                        .on_click(|_, _, cx| crate::pages::open_settings_at("data", None, cx)),
                    )
                    .child(
                        button("overview-docs", tr(cx, "查看文档"), ButtonVariant::Link, cx)
                            .on_click(|_, window, cx| {
                                // 打不开就如实说：静默失败的按钮和假按钮是一回事。
                                if let Err(error) = duck_platform::open_url(DOCS_URL) {
                                    duck_components::notify::error(
                                        trf(cx, "打开文档失败：{}", &[&error.to_string()]),
                                        window,
                                        cx,
                                    );
                                }
                            }),
                    ),
            );

        // ── 运行环境：三行都读当前真实状态，不写死 ────────────────────────
        let appearance = theme.appearance();
        let scale_percent = (appearance.ui_scale * 100.).round();
        let preference = match appearance.preference {
            ThemePreference::System => tr(cx, "跟随系统"),
            ThemePreference::Light => tr(cx, "浅色"),
            ThemePreference::Dark => tr(cx, "深色"),
        };
        let preference_detail = match appearance.preference {
            // 「跟随系统」时把当前实际落到的模式一并说出来，否则用户看不出现在到底是哪种。
            ThemePreference::System => trf(
                cx,
                "跟随系统，当前生效的是{}",
                &[match theme.mode() {
                    gpui_component::ThemeMode::Dark => tr(cx, "深色"),
                    gpui_component::ThemeMode::Light => tr(cx, "浅色"),
                }],
            ),
            ThemePreference::Light | ThemePreference::Dark => {
                trf(cx, "内置主题 {}", &[theme.builtin().label()])
            }
        };
        let icon_count = DuckIcon::ALL.len();

        let environment = v_flex()
            .gap(tokens.spacing.sm)
            .child(section_title(tr(cx, "运行环境"), cx))
            .child(option_group(
                [
                    option_row(
                        tr(cx, "界面缩放"),
                        Some(
                            tr(
                                cx,
                                "按 0.8 ~ 1.5 倍缩放全部尺寸 token，并同步重算密度档位。",
                            )
                            .into(),
                        ),
                        div()
                            .text_size(tokens.typography.sm.size)
                            .text_color(muted)
                            .child(selectable_text(trf(
                                cx,
                                "{}%",
                                &[&format!("{scale_percent:.0}")],
                            ))),
                        cx,
                    )
                    .into_any_element(),
                    option_row(
                        tr(cx, "主题"),
                        Some(preference_detail.into()),
                        div()
                            .text_size(tokens.typography.sm.size)
                            .text_color(muted)
                            .child(preference),
                        cx,
                    )
                    .into_any_element(),
                    option_row(
                        tr(cx, "图标集"),
                        Some(tr(cx, "内嵌 Lucide 子集，线宽统一 1.75").into()),
                        div()
                            .text_size(tokens.typography.sm.size)
                            .text_color(muted)
                            .child(selectable_text(trf(
                                cx,
                                "{} 个",
                                &[&icon_count.to_string()],
                            ))),
                        cx,
                    )
                    .into_any_element(),
                ],
                cx,
            ));

        div()
            .id("overview-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .bg(tokens.colors.surface)
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xl)
                    .px(tokens.spacing.xl)
                    .py(tokens.spacing.xl)
                    .child(header)
                    .child(stats)
                    .child(quick_actions)
                    .child(environment),
            )
    }
}
