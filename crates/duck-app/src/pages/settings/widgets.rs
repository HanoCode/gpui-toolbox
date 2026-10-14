//! 跨页复用的控件构造（自由函数，不持有状态）。

use super::*;

/// 按 schema 渲染一项工具设置的控件。
///
/// 类型不符（schema 改过 / 落盘值来自旧版本）时不 panic，回落成只读文本——
/// 设置界面崩掉比少一个控件糟糕得多。
pub(super) fn tool_setting_control(
    tool_id: &'static str,
    spec: &'static ToolSettingSpec,
    cx: &App,
) -> AnyElement {
    let spec = *spec;
    let key = spec.key();
    let tokens = active_theme(cx).tokens();
    match (spec.kind(), tool_settings::current(cx, tool_id, &spec)) {
        (ToolSettingKind::Toggle { .. }, SettingValue::Toggle(on)) => switch_control(
            SharedString::from(format!("settings-tool-switch-{tool_id}-{key}")),
            on,
            move |value, _, cx| {
                tool_settings::apply(cx, tool_id, &spec, SettingValue::Toggle(*value));
                cx.refresh_windows();
            },
        )
        .into_any_element(),
        (ToolSettingKind::Select { options, .. }, SettingValue::Select(current)) => {
            let items = options.iter().map(|option| {
                (
                    SharedString::from(option.value()),
                    SharedString::from(tr(cx, option.label())),
                )
            });
            dropdown_select(
                SharedString::from(format!("settings-tool-select-{tool_id}-{key}")),
                items,
                SharedString::from(current),
                move |value, _, cx| {
                    // 下拉回传的是稳定值；按 schema 校验一次，非法值（理论上不会）直接丢弃。
                    let picked = options
                        .iter()
                        .map(tool_settings::SelectOption::value)
                        .find(|option| *option == value.as_ref());
                    let Some(picked) = picked else {
                        return;
                    };
                    tool_settings::apply(cx, tool_id, &spec, SettingValue::Select(picked));
                    cx.refresh_windows();
                },
                cx,
            )
        }
        (ToolSettingKind::Text { placeholder, .. }, SettingValue::Text(text)) => {
            let shown = if text.is_empty() {
                meta_text(tr(cx, "（未设置）"), cx).into_any_element()
            } else {
                body_text(text, cx).into_any_element()
            };
            h_flex()
                .items_center()
                // 只读值 ↔ 「编辑」按钮之间：两块独立控件，取「独立表面」档间距。
                .gap(tokens.spacing.sm)
                .child(shown)
                .child(
                    button(
                        SharedString::from(format!("settings-tool-text-{tool_id}-{key}")),
                        tr(cx, "编辑"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let on_confirm: TextConfirm = Rc::new(move |value, _, cx| {
                            tool_settings::apply(
                                cx,
                                tool_id,
                                &spec,
                                SettingValue::Text(value.to_owned()),
                            );
                            cx.refresh_windows();
                        });
                        open_text_prompt(
                            window,
                            cx,
                            spec.label(),
                            spec.label(),
                            placeholder,
                            None,
                            on_confirm,
                        );
                    }),
                )
                .into_any_element()
        }
        (
            ToolSettingKind::Number {
                min,
                max: _,
                default: _,
                unit,
            },
            SettingValue::Number(value),
        ) => {
            let shown = if unit.is_empty() {
                value.to_string()
            } else {
                format!("{value} {unit}")
            };
            h_flex()
                .items_center()
                // 同上：数值只读展示 ↔ 「编辑」按钮，取「独立表面」档间距。
                .gap(tokens.spacing.sm)
                .child(body_text(shown, cx))
                .child(
                    button(
                        SharedString::from(format!("settings-tool-number-{tool_id}-{key}")),
                        tr(cx, "编辑"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let on_confirm: NumberConfirm = Rc::new(move |value, _, cx| {
                            tool_settings::apply(cx, tool_id, &spec, SettingValue::Number(value));
                            cx.refresh_windows();
                        });
                        open_number_prompt(
                            window,
                            cx,
                            spec.label(),
                            spec.label(),
                            unit,
                            None,
                            min,
                            on_confirm,
                        );
                    }),
                )
                .into_any_element()
        }
        (_, other) => body_text(setting_value_display(&other), cx).into_any_element(),
    }
}

/// 设置值的朴素展示（仅用于类型不符的兜底分支）。
pub(super) fn setting_value_display(value: &SettingValue) -> String {
    match value {
        SettingValue::Toggle(on) => on.to_string(),
        SettingValue::Select(option) => (*option).to_owned(),
        SettingValue::Text(text) => text.clone(),
        SettingValue::Number(number) => number.to_string(),
    }
}

/// 页面标题 + 一行描述（[`title_text`] + xs 二级文字）。
pub(super) fn page_heading(title: &str, description: &str, cx: &App) -> Div {
    let tokens = active_theme(cx).tokens().clone();
    v_flex()
        .w_full()
        .gap(tokens.spacing.xxs)
        .child(title_text(title, cx))
        .child(meta_text(description, cx))
}

/// 「AI」页分区里的一条**纯说明**行（没有控件）。
///
/// 存在的理由：空态与「本版只支持 OpenAI 兼容协议」这类事实需要说清楚，
/// 而 `option_row*` 都要求给一个控件——挂一个空 `div` 会多出一段无意义的留白。
/// 「AI」页的分区说明行（整行、无控件）。
///
/// 收 `impl Into<SharedString>` 而不是 `&'static str`：带占位符的说明（`trf` 的产物）
/// 是运行时拼出来的 `String`，写死 `&'static str` 会让那些地方退化成「先泄漏再传」。
pub(super) fn ai_hint_row(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens().clone();
    div()
        .w_full()
        .px(tokens.spacing.lg)
        .py(tokens.spacing.md)
        .child(meta_text(text, cx))
        .into_any_element()
}

/// 知识库索引的等宽族名。
///
/// 进度日志里全是文件路径，比例字体会让它们读起来一截一截的。
/// `DuckThemeState` 没有 `fonts()`（本仓 `logs.rs` / `licenses.rs` 同款处置），
/// 族名只能在用到的地方各写一份。
#[cfg(target_os = "macos")]
pub(super) const MONO_FAMILY: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
pub(super) const MONO_FAMILY: &str = "Consolas";

/// 一条索引进度对应的界面文案。
///
/// 内核的 [`IndexProgress`] 只带数据、不带文案（`duck-ai` 是零依赖内核，不吐成品中文），
/// 换成哪种语言由这里决定。
pub(super) fn kb_progress_line(progress: &IndexProgress, cx: &App) -> String {
    match progress {
        IndexProgress::Started { label, files } => {
            trf(cx, "开始索引 {}（{} 个文件）", &[label, &files.to_string()])
        }
        IndexProgress::Chunks { label, chunks } => {
            trf(cx, "{}：已写入 {} 块", &[label, &chunks.to_string()])
        }
        // 「跳过」和「写了 0 块」是两回事：前者表示指纹没变（增量命中），
        // 后者表示扫到了文件但一块都没切出来。合成一句会让人以为索引坏了。
        IndexProgress::Finished { label, report } if report.skipped => {
            trf(cx, "{}：内容未变，跳过", &[label])
        }
        IndexProgress::Finished { label, report } => trf(
            cx,
            "{}：写入 {} 块（{} 个文件）",
            &[label, &report.chunks.to_string(), &report.files.to_string()],
        ),
        IndexProgress::Failed { label, reason } => trf(cx, "{} 索引失败：{}", &[label, reason]),
    }
}

/// 「AI」页厂商行的凭据状态文案。
///
/// 几种状态刻意分开说：**平台没后端**（无解，改用本地端点）与**还没配过**
/// （去填一个 key 就行）是完全不同的处置，合成一句「未配置」会让人白折腾。
pub(super) fn ai_credential_label(
    profile: &duck_ai::ProviderProfile,
    has_key: bool,
    secure: bool,
    cx: &App,
) -> String {
    if profile.is_local {
        tr(cx, "本地端点，无需密钥").to_owned()
    } else if !secure {
        tr(cx, "本机凭据存储不可用").to_owned()
    } else if has_key {
        tr(cx, "密钥已加密保存").to_owned()
    } else {
        tr(cx, "尚未配置密钥").to_owned()
    }
}
