//! 设置页「插件」：清单与条目。

use super::*;

/// 「插件」页的一行：展示名 / 模型·版本·工具数·能力摘要 / 启停开关。
/// 清单无效的插件保留展示（回落目录名 + 英文错误说明），开关换成警示图标——
/// 让投放错的包可诊断，而不是静默消失。
pub(super) fn plugin_row(entry: crate::plugins::PluginEntry, cx: &App) -> AnyElement {
    let description: SharedString = match entry.model {
        Some(model) => {
            let model_label = crate::plugins::model_display(model, cx);
            let tools_label = trf(cx, "{} 个工具", &[entry.tools.to_string().as_str()]);
            let mut parts = vec![
                model_label.to_string(),
                format!("v{}", entry.version.as_deref().unwrap_or("?")),
                tools_label,
            ];
            if let Some(caps) = &entry.capabilities {
                let mut labels: Vec<&str> = Vec::new();
                if !caps.filesystem_read.is_empty() || !caps.filesystem_write.is_empty() {
                    labels.push(tr(cx, "文件访问"));
                }
                if !caps.network_hosts.is_empty() {
                    labels.push(tr(cx, "网络"));
                }
                if caps.notifications {
                    labels.push(tr(cx, "通知"));
                }
                if !caps.process_exec.is_empty() {
                    labels.push(tr(cx, "进程执行"));
                }
                if !labels.is_empty() {
                    parts.push(labels.join(" / "));
                }
                // 授予概况：清单里声明的 ≠ 本机放行的。不带这一行的话，用户在确认框里
                // 收回去的权限在这页上完全看不出来（详情页能改，但先得知道要去看）。
                if let Some(plugin_id) = entry.id.as_deref() {
                    let (granted, declared) = crate::plugins::grant_summary(cx, plugin_id, caps);
                    if declared > 0 {
                        parts.push(trf(
                            cx,
                            "已授予 {} / {} 项能力",
                            &[&granted.to_string(), &declared.to_string()],
                        ));
                    }
                }
            }
            SharedString::from(parts.join(" · "))
        }
        None => SharedString::from(format!(
            "{} — {}",
            tr(cx, "清单无效"),
            entry.error.as_deref().unwrap_or("")
        )),
    };

    let control = match entry.id.clone() {
        Some(plugin_id) => {
            let display_name = entry.name.clone();
            switch_control(
                SharedString::from(format!("settings-plugins-toggle-{plugin_id}")),
                entry.enabled,
                move |value: &bool, window, cx| {
                    crate::plugins::set_enabled(cx, &plugin_id, *value);
                    let label = if *value {
                        trf(cx, "插件「{}」已启用", &[display_name.as_str()])
                    } else {
                        trf(cx, "插件「{}」已停用", &[display_name.as_str()])
                    };
                    duck_components::notify::success(label, window, cx);
                    cx.refresh_windows();
                },
            )
            .into_any_element()
        }
        // 显式给尺寸：`Icon` 不设 `.size()` 时按库的规则（`gpui-component/src/icon.rs`）
        // 继承所在行的文字字号，落成 ≈13px，越出 `icon.sm/md/lg` 的 12/14/16 阶梯。
        None => Icon::new(DuckIcon::TriangleAlert)
            .size(active_theme(cx).extended().icon.md)
            .into_any_element(),
    };

    option_row(entry.name, Some(description), control, cx).into_any_element()
}

impl SettingsWindowView {
    // ── 插件：市场源 + 发现快照 + 逐插件启停 ─────────────────────────────
    pub(super) fn plugins_body(&self, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();
        let entries = crate::plugins::PluginEntry::entries(cx);
        let rows: Vec<AnyElement> = if entries.is_empty() {
            let dir = crate::plugins::primary_plugins_dir();
            vec![
                body_text(tr(cx, "未发现插件"), cx).into_any_element(),
                meta_text(
                    trf(
                        cx,
                        "将插件放入 {} 后重启应用。",
                        &[dir.display().to_string().as_str()],
                    ),
                    cx,
                )
                .into_any_element(),
            ]
        } else {
            entries
                .into_iter()
                .map(|entry| plugin_row(entry, cx))
                .collect()
        };

        // 市场源：原先在插件中心顶部占一整行，挪到这里（那边只留一行状态结论）。
        let source_row = option_row_stacked(
            tr(cx, "市场源"),
            Some(
                tr(
                    cx,
                    "插件清单的获取地址；留空回落默认源，保存后立即重新获取。",
                )
                .into(),
            ),
            h_flex()
                .w_full()
                .gap(tokens.spacing.sm)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&self.market_source).control(cx).w_full()),
                )
                .child(
                    button(
                        "duck-settings-market-source-save",
                        tr(cx, "保存并刷新"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        let value = this.market_source.read(cx).value().to_string();
                        crate::market::set_source(cx, value);
                        crate::market::refresh(cx);
                    })),
                ),
            cx,
        );

        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(settings_section(
                Some(tr(cx, "插件市场源").into()),
                [source_row.into_any_element()],
                cx,
            ))
            .child(settings_section(
                Some(tr(cx, "已安装插件").into()),
                rows,
                cx,
            ))
    }
}
