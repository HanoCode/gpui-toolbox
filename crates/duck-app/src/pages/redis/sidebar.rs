//! 左右两栏：连接列表（页面内 SidebarPanel）与 shell 级右栏详情。

use super::*;

impl RedisPage {
    // ------------------------------------------------------------------
    // 左栏：连接列表。
    // ------------------------------------------------------------------

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let searching = !query.is_empty();

        // 按档案里的分组名分组（无分组归到「默认分组」）。
        let mut groups: Vec<(String, Vec<&RedisConfig>)> = Vec::new();
        for cfg in &self.connections {
            let label = cfg
                .group_name()
                .map(str::to_owned)
                .unwrap_or_else(|| tr(cx, "默认分组").to_owned());
            match groups.iter_mut().find(|(name, _)| *name == label) {
                Some((_, items)) => items.push(cfg),
                None => groups.push((label, vec![cfg])),
            }
        }
        for (_, items) in &mut groups {
            items.retain(|cfg| {
                !searching
                    || cfg.display_name().to_lowercase().contains(&query)
                    || format!("{}:{}", cfg.host, cfg.effective_port())
                        .to_lowercase()
                        .contains(&query)
            });
        }
        groups.retain(|(_, items)| !items.is_empty());

        let mut nav = v_flex().w_full().gap(tokens.spacing.xxs);
        for (label, items) in &groups {
            nav = nav.child(
                h_flex()
                    .w_full()
                    .px(tokens.spacing.sm)
                    .pt(tokens.spacing.sm)
                    .pb(tokens.spacing.xxs)
                    .child(caption_heading(label.clone(), cx)),
            );
            for cfg in items {
                let id = cfg.id.clone();
                let connected = self.sessions.contains_key(&id);
                let is_active = self.active.as_deref() == Some(id.as_str());
                let trailing = h_flex()
                    .flex_none()
                    .gap(tokens.spacing.xxs)
                    .children(env_tag_label(cfg.env_tag, cx).map(|tag| neutral_tag(tag, cx)))
                    .child(
                        div()
                            .flex_none()
                            .text_size(tokens.typography.xs.size)
                            .text_color(extended.colors.text_tertiary)
                            .child(SharedString::from(format!("db{}", cfg.db))),
                    )
                    .child(
                        icon_button(
                            SharedString::from(format!("redis-edit-{id}")),
                            tr(cx, "编辑连接"),
                            Icon::new(DuckIcon::Settings)
                                .size(extended.icon.sm)
                                .text_color(tokens.colors.muted_foreground),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            let id = id.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.edit_connection(&id, window, cx);
                                });
                            }
                        }),
                    );
                nav = nav.child(
                    sidebar_navigation_button(
                        SharedString::from(format!("redis-conn-{id}")),
                        SharedString::from(cfg.display_name()),
                        Icon::new(DuckIcon::DatabaseZap)
                            .size(extended.icon.md)
                            .text_color(if connected {
                                extended.colors.status_completed
                            } else {
                                extended.colors.status_paused
                            }),
                        trailing,
                        is_active,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        let id = id.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.connect_entry(&id, window, cx);
                            });
                        }
                    }),
                );
            }
        }
        if searching && groups.is_empty() {
            nav = nav.child(
                div()
                    .w_full()
                    .py(tokens.spacing.lg)
                    .flex()
                    .justify_center()
                    .child(meta_text(tr(cx, "没有匹配的连接。"), cx)),
            );
        }

        v_flex()
            .size_full()
            .min_h_0()
            .bg(tokens.colors.surface)
            .child(
                h_flex()
                    .h(theme.density().section_header)
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .px(tokens.spacing.sm)
                    .child(caption_heading(tr(cx, "Redis 连接"), cx))
                    .child(
                        icon_button(
                            "redis-new-connection-icon",
                            tr(cx, "新建连接"),
                            Icon::new(DuckIcon::Plus)
                                .size(extended.icon.sm)
                                .text_color(tokens.colors.muted_foreground),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.new_connection(window, cx);
                                });
                            }
                        }),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .px(tokens.spacing.sm)
                    .pb(tokens.spacing.xs)
                    .child(
                        h_flex()
                            .h(theme.density().control)
                            .w_full()
                            .px(tokens.spacing.sm)
                            .rounded(theme.components().input_radius)
                            .border(extended.stroke.thin)
                            .border_color(extended.colors.hairline)
                            .bg(tokens.colors.background)
                            .child(
                                Input::new(&self.search)
                                    .appearance(false)
                                    .bordered(false)
                                    .w_full(),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .id("redis-connections-scroll")
                    .overflow_y_scroll()
                    .child(nav),
            )
            .child(
                div()
                    .flex_none()
                    .p(tokens.spacing.sm)
                    .border_t(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .child(
                        button(
                            "redis-new-connection",
                            tr(cx, "新建连接"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.new_connection(window, cx);
                                });
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 主区：键空间浏览器。
    // ------------------------------------------------------------------

    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let Some(detail) = &self.detail else {
            return v_flex()
                .size_full()
                .id("redis-right-sidebar")
                .items_center()
                .justify_center()
                .p(tokens.spacing.lg)
                .child(meta_text(
                    if self.active.is_some() {
                        tr(cx, "选择一个键查看详情。")
                    } else {
                        tr(cx, "尚未连接。")
                    },
                    cx,
                ))
                .into_any_element();
        };

        let gate = self.gate();
        let can_delete = gate
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::DeleteKey));
        let can_write = gate
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::WriteKey));

        let ttl_text: SharedString = match detail.ttl {
            -1 => SharedString::from(tr(cx, "永久")),
            -2 => SharedString::from(tr(cx, "键不存在")),
            seconds => trf(cx, "{} 秒", &[&seconds.to_string()]).into(),
        };

        let meta = description_list([
            description_row(
                tr(cx, "键名"),
                body_text(SharedString::from(detail.key.clone()), cx),
            ),
            description_row(
                tr(cx, "类型"),
                body_text(key_type_label(detail.key_type, cx), cx),
            ),
            description_row(tr(cx, "过期时间"), body_text(ttl_text, cx)),
            description_row(
                tr(cx, "元素数"),
                body_text(
                    SharedString::from(if detail.truncated {
                        format!("{}（已截断）", detail.size)
                    } else {
                        detail.size.to_string()
                    }),
                    cx,
                ),
            ),
        ]);

        let mut panel = v_flex()
            .size_full()
            .id("redis-right-sidebar")
            .gap(tokens.spacing.sm)
            .p(tokens.spacing.sm)
            .child(caption_heading(tr(cx, "键详情"), cx))
            .child(meta);

        // 值区域：标题右侧放解码档位（只有字符串值有这三档）。
        panel = panel.child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(caption_heading(tr(cx, "值"), cx))
                .children(decode_chips(detail.key_type, self.decode, &view, cx)),
        );
        let value_body: AnyElement = match &detail.view {
            ValueView::Text {
                rendered,
                notice,
                bytes,
            } => v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .child(
                    div()
                        .w_full()
                        // 滚动面板的**最大高度**是布局约束（撑住值面板的可视区），
                        // 不属 spacing 阶梯，保留裸值。
                        .max_h(px(240.))
                        .id("redis-value-text")
                        .overflow_y_scroll()
                        .p(tokens.spacing.xs)
                        .rounded(theme.components().input_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.background)
                        .text_size(tokens.typography.mono_md.size)
                        .line_height(tokens.typography.mono_md.line_height)
                        .child(SharedString::from(rendered.clone())),
                )
                // 解码档位的说明行（GZIP 解压出多少字节、时间戳按哪个单位解读……）：
                // 没有它，用户不知道 GZIP 档显示的这段字是从哪来的。
                .children(
                    notice
                        .clone()
                        .map(|text| meta_text(SharedString::from(text), cx)),
                )
                .child(meta_text(trf(cx, "{} 字节", &[&bytes.to_string()]), cx))
                .into_any_element(),
            ValueView::Binary { hex, bytes } => v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .child(
                    div()
                        .w_full()
                        // 同上：值面板的可视区上限，与 `redis-value-text` 取同一高度。
                        .max_h(px(240.))
                        .id("redis-value-hex")
                        .overflow_y_scroll()
                        .p(tokens.spacing.xs)
                        .rounded(theme.components().input_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.background)
                        .text_size(tokens.typography.mono_md.size)
                        .line_height(tokens.typography.mono_md.line_height)
                        .child(SharedString::from(hex.clone())),
                )
                .child(meta_text(
                    trf(cx, "二进制内容，共 {} 字节", &[&bytes.to_string()]),
                    cx,
                ))
                .into_any_element(),
            ValueView::TooLarge { bytes } => meta_text(
                trf(cx, "值过大（{} 字节），未自动加载。", &[&bytes.to_string()]),
                cx,
            )
            .into_any_element(),
            ValueView::JsonFailed { error, bytes } => v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .child(
                    div()
                        .w_full()
                        .p(tokens.spacing.xs)
                        .rounded(theme.components().input_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.background)
                        .text_size(tokens.typography.sm.size)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(trf(
                            cx,
                            "不是合法 JSON：{}",
                            &[error.as_str()],
                        ))),
                )
                .child(meta_text(
                    trf(
                        cx,
                        "共 {} 字节，可切到「文本」或「Hex」档查看原文。",
                        &[&bytes.to_string()],
                    ),
                    cx,
                ))
                .into_any_element(),
            ValueView::Hash(pairs) => hash_rows(&detail.key, pairs, &view, cx),
            ValueView::ZSet(pairs) => zset_rows(&detail.key, pairs, &view, cx),
            ValueView::List(items) => list_rows(&detail.key, items, self.list_editing, &view, cx),
            ValueView::Set(items) => set_rows(&detail.key, items, &view, cx),
            ValueView::Stream(entries) => {
                let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
                for (id, fields) in entries {
                    list = list.child(
                        v_flex()
                            .w_full()
                            .gap(tokens.spacing.xxs)
                            .p(tokens.spacing.xs)
                            .rounded(theme.components().input_radius)
                            .border(extended.stroke.thin)
                            .border_color(extended.colors.hairline)
                            .child(
                                div()
                                    .text_size(tokens.typography.xs.size)
                                    .text_color(tokens.colors.muted_foreground)
                                    .child(SharedString::from(id.clone())),
                            )
                            .children(fields.iter().map(|(field, value)| {
                                div()
                                    .text_size(tokens.typography.sm.size)
                                    .text_color(tokens.colors.foreground)
                                    .child(SharedString::from(format!("{field}: {value}")))
                            })),
                    );
                }
                list.into_any_element()
            }
            ValueView::Missing => meta_text(
                tr(cx, "键已不存在（可能在选中之后被其他客户端删掉了）。"),
                cx,
            )
            .into_any_element(),
            ValueView::Unsupported(key_type) => meta_text(
                trf(
                    cx,
                    "「{}」类型的专用查看器尚未实现（计划在后续增量补上）。",
                    &[key_type_label(*key_type, cx)],
                ),
                cx,
            )
            .into_any_element(),
        };
        panel = panel.child(
            div()
                .w_full()
                // 整个值区（含标题、格式化切换）的滚动上限，布局约束、非间距。
                .max_h(px(320.))
                .id("redis-value-scroll")
                .overflow_y_scroll()
                .child(value_body),
        );

        // 按类型给编辑入口（字符串 / 哈希 / 列表 / 集合 / 有序集合各一套字段）。
        panel = panel.child(value_editor(self, detail.key_type, &view, can_write, cx));

        if !can_write || !can_delete {
            panel = panel.child(meta_text(
                gate.as_ref()
                    .and_then(|gate| gate.check(Capability::WriteKey).err())
                    .map(|reason| deny_message(reason, cx))
                    .unwrap_or_else(|| tr(cx, "当前连接为只读。")),
                cx,
            ));
        }

        // 键管理（TTL / 重命名）与删除键同属「危险操作区」，排在一起。
        panel = panel.child(key_ops(self, &view, can_write, cx));

        panel = panel.child(
            button(
                "redis-delete-key",
                tr(cx, "删除此键"),
                ButtonVariant::Destructive,
                cx,
            )
            .disabled(!can_delete)
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.delete_selected_key(window, cx);
                    });
                }
            }),
        );

        panel.into_any_element()
    }

    // ------------------------------------------------------------------
    // 状态栏。
    // ------------------------------------------------------------------
}
