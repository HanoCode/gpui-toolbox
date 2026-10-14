//! 设置页「关于」。

use super::*;

impl SettingsWindowView {
    // ── 关于 ─────────────────────────────────────────────────────────────
    /// 「关于」＝ 居中品牌头 + 更新 + 版本发布记录 + 信息。
    ///
    /// 更新相关的整块（检查 / 下载 / 安装 / 自动检查 / 更新源）原先是独立分类页，
    /// 现并入这里：用户想知道「我装的是哪一版、源上有没有新版、改了什么」，
    /// 只该有一个去处，而不是在两页之间来回找。
    pub(super) fn about_body(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let colors = tokens.colors;
        let muted = colors.muted_foreground;
        let appearance = theme.appearance().clone();
        let mode = theme.mode();
        let version = duck_update::compiled_version().to_string();

        // —— 品牌头：logo / 名称 / 版本徽章 / 一句话说明，整块居中 ——
        let header = v_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.md)
            .pt(tokens.spacing.lg)
            .pb(tokens.spacing.xl)
            .child(
                div()
                    .size(px(ABOUT_LOGO_SIZE))
                    .rounded(px(ABOUT_LOGO_RADIUS))
                    .overflow_hidden()
                    .child(img(LOGO_ASSET_PATH).size_full()),
            )
            .child(
                div()
                    .text_size(tokens.typography.xl.size)
                    .line_height(tokens.typography.xl.line_height)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.foreground)
                    .child(crate::APP_NAME),
            )
            .child(
                h_flex()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xxs)
                    .rounded_full()
                    .bg(colors.muted)
                    .child(meta_text(format!("v{version}"), cx)),
            )
            .child(
                div()
                    .max_w(px(ABOUT_DESCRIPTION_MAX_WIDTH))
                    .text_center()
                    .child(
                        body_text(
                            tr(
                                cx,
                                "Duck Toolbox 是一个基于 GPUI 构建的桌面工具箱外壳：把常用小工具收进同一个窗口，能力由插件扩展。",
                            ),
                            cx,
                        )
                        .text_color(muted),
                    ),
            );

        // —— 更新：原「更新」分类页整块并入（版本号已见上方徽章，不再重复一行）——
        let phase = update::phase(cx);
        let running = phase.is_running();

        let configured = update::source(cx);
        let source_row = option_row(
            tr(cx, "更新源"),
            Some(tr(cx, "静态 JSON manifest 的 https 地址。").into()),
            h_flex()
                .min_w_0()
                .gap(tokens.spacing.sm)
                .child(
                    div().min_w_0().truncate().child(
                        body_text(
                            if configured.trim().is_empty() {
                                tr(cx, "未配置")
                            } else {
                                configured.as_ref()
                            },
                            cx,
                        )
                        .text_color(muted),
                    ),
                )
                .child(
                    button(
                        "duck-settings-update-source",
                        tr(cx, "修改"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, window, cx| {
                        // 文案先取出来：`tr` 借 `cx`，而 `open_text_prompt` 要 `&mut`。
                        let title = tr(cx, "更新源");
                        let hint = tr(cx, "留空即用内置默认源（GitHub Releases）。");
                        let on_confirm: TextConfirm =
                            Rc::new(|value, _, cx| update::set_source(value.trim().to_owned(), cx));
                        open_text_prompt(
                            window,
                            cx,
                            title,
                            title,
                            update::default_source(),
                            Some(hint),
                            on_confirm,
                        );
                    })),
                ),
            cx,
        );

        let auto_row = option_row(
            tr(cx, "启动后自动检查更新"),
            None,
            switch_control(
                "duck-settings-update-auto",
                update::auto_check(cx),
                cx.listener(|_, next: &bool, _, cx| update::set_auto_check(*next, cx)),
            ),
            cx,
        );

        let mut status = v_flex().w_full().gap(tokens.spacing.sm);
        if let Some(text) = update::describe(&phase, cx) {
            status = status.child(body_text(text, cx).text_color(muted));
        }
        if let update::Phase::Manual { reason, .. } = &phase {
            status = status.child(meta_text(update::manual_reason(*reason, cx), cx));
        }
        if let update::Phase::Downloading { downloaded, total } = phase {
            let ratio = if total == 0 {
                0.
            } else {
                downloaded as f32 / total as f32
            };
            status = status.child(content::progress_bar(
                "duck-settings-update-progress",
                ratio,
                cx,
            ));
        }

        let mut actions = h_flex().gap(tokens.spacing.sm).flex_wrap();
        if running {
            actions = actions.child(
                button(
                    "duck-settings-update-cancel",
                    tr(cx, "取消"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|_, _, _, cx| update::cancel(cx))),
            );
        } else {
            actions = actions.child(
                button(
                    "duck-settings-update-check",
                    tr(cx, "检查更新"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|_, _, _, cx| update::check_now(cx))),
            );
            if matches!(phase, update::Phase::Available(_)) {
                actions = actions.child(
                    button(
                        "duck-settings-update-install",
                        tr(cx, "下载并安装"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| update::install_now(cx))),
                );
            }
            if matches!(phase, update::Phase::Installed) {
                actions = actions.child(
                    button(
                        "duck-settings-update-restart",
                        tr(cx, "重启并应用更新"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| update::restart(cx))),
                );
            }
            if matches!(phase, update::Phase::RolledBack) {
                actions = actions.child(
                    button(
                        "duck-settings-update-restart-rollback",
                        tr(cx, "重启并运行上一版本"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| update::restart(cx))),
                );
            }
            // 只要上一版还在盘上，回退就是一个**正常的**操作，而不是失败之后的补救出口——
            // 所以它是个常驻的次要按钮，跟「检查更新」并排，而不是藏在错误提示里。
            if update::can_rollback() && !matches!(phase, update::Phase::RolledBack) {
                actions = actions.child(
                    button(
                        "duck-settings-update-rollback",
                        tr(cx, "回退到上一版本"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| update::rollback_now(cx))),
                );
            }
        }
        status = status.child(actions);

        let status_row = option_row_stacked(tr(cx, "更新状态"), None, status, cx);

        let update_section = settings_section(
            Some(tr(cx, "更新").into()),
            [
                status_row.into_any_element(),
                auto_row.into_any_element(),
                source_row.into_any_element(),
            ],
            cx,
        );

        // —— 版本发布记录：内容来自更新源 manifest，紧跟更新块（它就是检查的产物）——
        let notes: AnyElement = match update::release_notes(cx) {
            Some(notes) => v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .children(notes.lines().map(|line| {
                    // 只做换行还原：manifest 里的 Markdown 标题按原样当正文读，
                    // 不引一套 Markdown 渲染（渲染器会把 `##` 之类直接漏给用户看）。
                    if line.trim().is_empty() {
                        div().h(tokens.spacing.xs).into_any_element()
                    } else {
                        body_text(line.to_string(), cx)
                            .text_color(muted)
                            .into_any_element()
                    }
                }))
                .into_any_element(),
            None => meta_text(
                tr(cx, "配置更新源并检查更新后，这里会显示源上版本的发布说明。"),
                cx,
            )
            .into_any_element(),
        };
        let notes_section = settings_section(
            Some(tr(cx, "版本发布记录").into()),
            [option_row_stacked(tr(cx, "最新发布"), None, notes, cx).into_any_element()],
            cx,
        );

        // —— 信息：版本明细与当前生效主题 ——
        let info_section = settings_section(
            Some(tr(cx, "信息").into()),
            [
                option_row(
                    tr(cx, "当前版本"),
                    Some(tr(cx, "取自 workspace 版本号，与打包产物一致。").into()),
                    body_text(version, cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "当前生效主题"),
                    None,
                    body_text(appearance.theme.for_mode(mode).label(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "使用引导"),
                    Some(tr(cx, "第一次启动时的三步向导，随时可以再看一遍。").into()),
                    button(
                        "duck-settings-onboarding",
                        tr(cx, "重新查看引导"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(
                        cx.listener(|_, _, _, cx| super::super::onboarding::open_onboarding(cx)),
                    ),
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        );

        // —— 诊断：出问题时最该先看的一处（排在「信息」之后，平时不打扰）——
        let diagnostics_actions = h_flex()
            .gap(tokens.spacing.sm)
            .flex_wrap()
            .child(
                button(
                    "duck-settings-diagnostics-copy",
                    tr(cx, "复制诊断信息"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|_, _, _, cx| diagnostics::copy_report(cx))),
            )
            .child(
                button(
                    "duck-settings-diagnostics-logs",
                    tr(cx, "打开日志目录"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|_, _, _, _| {
                    duck_platform::reveal_in_finder(&diagnostics::log_dir())
                })),
            );

        let diagnostics_section = settings_section(
            Some(tr(cx, "诊断信息").into()),
            [
                option_row(
                    tr(cx, "运行平台"),
                    None,
                    body_text(diagnostics::platform(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "插件数量"),
                    None,
                    body_text(diagnostics::plugin_count(cx).to_string(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "累计崩溃"),
                    None,
                    body_text(diagnostics::crash_total().to_string(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "数据文件"),
                    None,
                    body_text(duck_platform::db_path().display().to_string(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "日志目录"),
                    None,
                    body_text(diagnostics::log_dir().display().to_string(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row_stacked(tr(cx, "诊断操作"), None, diagnostics_actions, cx)
                    .into_any_element(),
            ],
            cx,
        );

        // —— 法律信息：发行合规要求「许可可查」，全文随程序内嵌（见 `pages::licenses`）——
        let license_summary = super::super::licenses::stats().map_or_else(
            || SharedString::from(tr(cx, "第三方组件许可汇总")),
            |stats| {
                SharedString::from(trf(
                    cx,
                    "{} 个组件 · {} 份许可全文",
                    &[&stats.components.to_string(), &stats.texts.to_string()],
                ))
            },
        );
        let legal_section = settings_section(
            Some(tr(cx, "法律信息").into()),
            [
                option_row(
                    tr(cx, "本程序许可"),
                    Some(tr(cx, "MIT 许可，版权归 HanoCode。").into()),
                    button(
                        "duck-settings-licenses",
                        tr(cx, "查看开源许可"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| super::super::licenses::open_licenses(cx))),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "第三方组件"),
                    Some(tr(cx, "由 Cargo.lock 汇总，随程序内置，离线可查。").into()),
                    body_text(license_summary, cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        );

        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(header)
            .child(update_section)
            .child(notes_section)
            .child(info_section)
            .child(diagnostics_section)
            .child(legal_section)
    }
}
