//! 设置页「工具」与「行为」。

use super::*;

impl SettingsWindowView {
    // ── 工具：左侧工具列 + 右侧该工具的设置 ────────────────────────────
    pub(super) fn tools_body(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let colors = tokens.colors;
        let lang = theme.lang();

        // 当前列出的工具（含已停用，不含未开开发者模式的 dev 工具）。
        let listed: Vec<ToolPick> = ToolRegistry::listed(cx)
            .into_iter()
            .map(|tool| {
                let meta = tool.meta();
                ToolPick {
                    id: tool.id(),
                    name: meta.name(),
                    icon: meta.icon(),
                    beta: meta.is_beta(),
                    settings: meta.settings(),
                }
            })
            .collect();

        // 选中项失效（工具被停用 / 开发者模式关掉）时回落列表首项。
        let active_id: &'static str = self
            .selected_tool
            .as_ref()
            .and_then(|id| listed.iter().find(|pick| pick.id == id.as_ref()))
            .map_or_else(|| listed.first().map_or("", |pick| pick.id), |pick| pick.id);

        // 左列：可点选的工具列表。
        let this = cx.entity().downgrade();
        let tool_count = listed.len();
        let picker: Vec<AnyElement> =
            listed
                .iter()
                .enumerate()
                .map(|(index, pick)| {
                    let id = pick.id;
                    let selected = id == active_id;
                    let this = this.clone();
                    // 焦点环色要 `move` 进闭包 → 先拷出来（`Hsla` 是 `Copy`）。
                    let row_ring = colors.ring;
                    // 可访问名用**显式** `aria_label`、复用下面那句可见文案：一是别让读屏
                    // 依赖「从子节点文字派生名字」这条本仓库无从验证的假设，二是
                    // `gpui_base::test_support` 的 `label()` 只读节点上的显式 label
                    // （`test_support.rs:318`），不显式给就断言不了。
                    let tool_label = SharedString::from(t(lang, pick.name));
                    h_flex()
                        .id(SharedString::from(format!("settings-tool-{id}")))
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.xs)
                        .px(tokens.spacing.sm)
                        .py(tokens.spacing.xs)
                        .rounded(theme.components().button_radius)
                        // 无障碍：GPUI 的 a11y 树**只收录带 role 的节点**，没有 role 就等于
                        // 对读屏完全不存在。这一列与 `kit::segmented_tabs` / `kit::nav_list`
                        // 同构（一组「选一个」的导航项），角色对齐 `Tab`，容器在渲染处补
                        // `TabList`。名字见上面 `tool_label`；位置/总数让读屏能播报
                        // 「第 M / N 项」。
                        .role(Role::Tab)
                        .aria_selected(selected)
                        .aria_label(tool_label.clone())
                        .aria_position_in_set(index + 1)
                        .aria_size_of_set(tool_count)
                        .focusable()
                        .tab_stop(true)
                        // 焦点用**描边**而不是底色（与 `navigation_row` / `activity_button`
                        // 同款）。先占 1px 透明边，描边出现时不引起位移。
                        .border_1()
                        .border_color(gpui::transparent_black())
                        .focus_visible(move |style| style.border_color(row_ring))
                        .cursor_pointer()
                        .when(selected, |row| row.bg(colors.muted))
                        .when(!selected, |row| {
                            row.hover(|style| style.bg(extended.colors.hover_tint))
                        })
                        .on_click(move |_, _, cx| {
                            let next = SharedString::from(id);
                            if let Ok(()) = this.update(cx, |this, cx| {
                                if this.selected_tool.as_ref() != Some(&next) {
                                    this.selected_tool = Some(next);
                                    cx.notify();
                                }
                            }) {}
                        })
                        .child(Icon::new(pick.icon).size(extended.icon.md).text_color(
                            if selected {
                                colors.primary
                            } else {
                                colors.muted_foreground
                            },
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .text_color(colors.foreground)
                                .child(tool_label),
                        )
                        .when(pick.beta, |row| row.child(beta_badge(cx)))
                        .into_any_element()
                })
                .collect();

        // 右列：选中工具的通用项 + 该工具的设置项。
        let mut sections: Vec<AnyElement> = Vec::new();
        if let Some(pick) = listed.iter().find(|pick| pick.id == active_id) {
            let tool_id = pick.id;
            let enabled = tool_settings::is_enabled(cx, tool_id);
            sections.push(
                settings_section(
                    Some(tr(cx, "通用").into()),
                    [
                        option_row(
                            tr(cx, "启用"),
                            Some(tr(cx, "停用后从工具箱列表隐藏，设置项保留。").into()),
                            switch_control(
                                SharedString::from(format!("settings-tool-enabled-{tool_id}")),
                                enabled,
                                move |value, _, cx| {
                                    tool_settings::set_enabled(cx, tool_id, *value);
                                    cx.refresh_windows();
                                },
                            ),
                            cx,
                        )
                        .into_any_element(),
                        option_row(
                            tr(cx, "使用次数"),
                            Some(tr(cx, "每次从工具箱运行该工具后累加。").into()),
                            body_text(tool_settings::runs(cx, tool_id).to_string(), cx)
                                .into_any_element(),
                            cx,
                        )
                        .into_any_element(),
                    ],
                    cx,
                )
                .into_any_element(),
            );

            let rows: Vec<AnyElement> = pick
                .settings
                .iter()
                .map(|spec| {
                    let control = tool_setting_control(tool_id, spec, cx);
                    let description = (!spec.description().is_empty())
                        .then(|| SharedString::from(t(lang, spec.description())));
                    // 下拉自带宽度，横排在窄窗口下会被挤扁，统一走竖排。
                    let stacked = matches!(spec.kind(), ToolSettingKind::Select { .. });
                    if stacked {
                        option_row_stacked(t(lang, spec.label()), description, control, cx)
                    } else {
                        option_row(t(lang, spec.label()), description, control, cx)
                    }
                    .into_any_element()
                })
                .collect();

            sections.push(
                if rows.is_empty() {
                    settings_section(
                        Some(t(lang, "设置项").into()),
                        [meta_text(tr(cx, "该工具暂无独立设置。"), cx).into_any_element()],
                        cx,
                    )
                } else {
                    settings_section(Some(t(lang, "设置项").into()), rows, cx)
                }
                .into_any_element(),
            );
        } else {
            sections.push(meta_text(tr(cx, "当前没有可配置的工具。"), cx).into_any_element());
        }

        // 开发者模式是全局开关，与具体工具无关，单独一节放在最后。
        let dev_mode = ToolRegistry::dev_mode(cx);
        sections.push(
            settings_section(
                Some(tr(cx, "开发者").into()),
                [option_row(
                    tr(cx, "开发者模式"),
                    Some(tr(cx, "显示标记为开发者的工具，仅在排查问题时使用。").into()),
                    switch_control("settings-tool-dev-mode", dev_mode, move |value, _, cx| {
                        ToolRegistry::set_dev_mode(cx, *value);
                        cx.refresh_windows();
                    }),
                    cx,
                )
                .into_any_element()],
                cx,
            )
            .into_any_element(),
        );

        h_flex()
            .w_full()
            .items_start()
            .gap(tokens.spacing.lg)
            .child(
                v_flex()
                    .id("settings-tool-list")
                    // 行按 `Role::Tab` 报，容器就要是 `TabList`（与 `kit::segmented_tabs`
                    // / `kit::nav_list` 一致），读屏才会把它当一组标签而不是一串孤立按钮。
                    .role(Role::TabList)
                    .flex_none()
                    .w(px(TOOL_PICKER_WIDTH))
                    .gap(tokens.spacing.xxs)
                    .children(picker),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(tokens.spacing.lg)
                    .children(sections),
            )
    }

    // ── 行为 ─────────────────────────────────────────────────────────────
    pub(super) fn behavior_body(&self, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();
        let confirm_row = option_row(
            tr(cx, "卸载插件前弹确认"),
            Some(tr(cx, "关掉后点「卸载」会直接开始，不再询问。").into()),
            switch_control(
                "duck-settings-confirm-uninstall",
                self.confirm_uninstall,
                cx.listener(|this, next: &bool, _, cx| {
                    this.confirm_uninstall = *next;
                    settings::set_confirm_uninstall(cx, *next);
                    cx.notify();
                }),
            ),
            cx,
        );
        let startup_section = settings_section(
            Some(tr(cx, "启动").into()),
            [
                option_row(
                    tr(cx, "开机自动启动"),
                    Some(tr(cx, "登录系统后自动运行 Duck。").into()),
                    switch_control(
                        "duck-settings-autostart",
                        self.autostart_enabled,
                        cx.listener(|this, next: &bool, window, cx| {
                            // 先按目标值落状态，失败再按系统真实值回摆——开关不闪两次。
                            this.autostart_enabled = *next;
                            if let Err(error) = duck_platform::set_launch_at_login(*next) {
                                this.autostart_enabled = duck_platform::launch_at_login_enabled();
                                duck_components::notify::error(
                                    trf(cx, "开机自启设置失败：{}", &[&error.to_string()]),
                                    window,
                                    cx,
                                );
                            }
                            cx.notify();
                        }),
                    ),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "启动时恢复上次窗口"),
                    Some(
                        tr(
                            cx,
                            "把上次退出时开着的设置、日志等窗口一并打开，各自回到原来的位置。",
                        )
                        .into(),
                    ),
                    switch_control(
                        "duck-settings-restore-windows",
                        self.restore_windows,
                        cx.listener(|this, next: &bool, _, cx| {
                            this.restore_windows = *next;
                            settings::set_restore_windows_on_launch(cx, *next);
                            cx.notify();
                        }),
                    ),
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        );
        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(settings_section(
                Some(tr(cx, "行为").into()),
                [confirm_row.into_any_element()],
                cx,
            ))
            .child(self.hotkey_section(cx))
            .child(startup_section)
    }
}
