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
                    .flex_1()
                    .min_h_0()
                    .id("redis-connections-scroll")
                    .overflow_y_scroll()
                    .child(nav),
            )
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 主区：键空间浏览器。
    // ------------------------------------------------------------------

    /// shell 级右栏：**关于这个键**（属性 + 危险操作区）。
    ///
    /// ⚠️ 值**不在这里**——它在主区右栏的值编辑器里（`value::render_value_editor`）。
    /// 这块面板回答「这个键是什么、能对它做什么」，值编辑器回答「它的值长什么样」；
    /// 拆开之后两边各拿自己需要的宽度，值再宽也挤不到键详情。
    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
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
        let can_write = gate
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::WriteKey));
        let can_delete = gate
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::DeleteKey));

        let ttl_text: SharedString = match detail.ttl {
            -1 => SharedString::from(tr(cx, "永久")),
            -2 => SharedString::from(tr(cx, "键不存在")),
            seconds => trf(cx, "{} 秒", &[&seconds.to_string()]).into(),
        };
        let size_text = if detail.truncated {
            trf(cx, "{}（已截断）", &[&detail.size.to_string()])
        } else {
            detail.size.to_string()
        };

        let mut rows = vec![
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
                body_text(SharedString::from(size_text), cx),
            ),
        ];
        // `OBJECT` 拿不到就**整行不画**：这两条本来就只是锦上添花，画一行「未知」
        // 等于把「服务端没说」编成了一个值（见 `KeyObjectInfo`）。
        if let Some(encoding) = &detail.encoding {
            rows.push(description_row(
                tr(cx, "内部编码"),
                body_text(SharedString::from(encoding.clone()), cx),
            ));
        }
        if let Some(secs) = detail.idletime_secs {
            rows.push(description_row(
                tr(cx, "空闲时间"),
                body_text(trf(cx, "{} 秒", &[&secs.to_string()]), cx),
            ));
        }

        let mut panel = v_flex()
            .size_full()
            .id("redis-right-sidebar")
            .gap(tokens.spacing.sm)
            .p(tokens.spacing.sm)
            .child(caption_heading(tr(cx, "键详情"), cx))
            .child(description_list(rows))
            // 标注区排在「键详情」之后、危险操作区之前：它是**本地**动作
            // （写 kv，不碰服务端），与下面的 TTL / 重命名 / 删除不是一类东西，
            // 所以既不进灰显闸门，也不跟危险操作挨在一起。
            .child(tag_editor(
                &detail.key,
                self.active_tags(),
                &self.note_input,
                &view,
                cx,
            ));

        if !can_write {
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

/// 右栏「标签与备注」区：色块行 + 收藏开关 + 备注框。
///
/// # 为什么这三样不受锁写闸门管
///
/// 它们改的是**本机**的一份备忘（落 kv），服务端收不到任何命令。只读连接、
/// 生产库、ACL 未授权——都不影响「我给自己记一笔」。反过来，如果把它接到
/// `Capability::WriteKey` 上，只读连接的右栏会整块灰掉，而这恰恰是标注最有用
/// 的场合（读库时记「这条数据有问题」）。
///
/// # 交互取舍
///
/// - **色块行**：第一个是「无」（清掉标色），后面六档按内核 `TagColor::ALL` 的
///   顺序。选中档用前景色描一圈粗边——比「加个对勾」省地方，也比只靠填充色
///   更能在深色主题上分辨。
/// - **备注框**：与上面那排输入框不同，**没有「保存」按钮**——输入即落盘
///   （见 `mod.rs` 里那条订阅）。写了就存，是标注这种轻量数据该有的手感。
/// - 收藏是**独立开关**，不和标色混成一个动作：一个键完全可以「收藏但不上色」。
fn tag_editor(
    key: &str,
    tags: &KeyTags,
    note_input: &Entity<InputState>,
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let dark = matches!(theme.mode(), duck_theme::ThemeMode::Dark);
    let current = tags.tag_of(key);
    let favorite = tags.is_favorite(key);

    // 色块：`None` 是「无标签」那一个。
    let swatch = |color: Option<TagColor>| -> AnyElement {
        let selected = current == color;
        let fill = color.map_or(tokens.colors.background, |color| tag_color(color, dark));
        let label = match color {
            Some(color) => tag_color_text(color, cx).to_owned(),
            None => tr(cx, "无标签").to_owned(),
        };
        div()
            .id(SharedString::from(format!(
                "redis-tag-{}",
                color.map_or("none", TagColor::as_str)
            )))
            .flex_none()
            .w(px(TAG_SWATCH))
            .h(px(TAG_SWATCH))
            .rounded(tokens.radius.sm)
            .bg(fill)
            .border(px(if selected { 2. } else { 1. }))
            .border_color(if selected {
                tokens.colors.foreground
            } else {
                extended.colors.hairline
            })
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(SharedString::from(label))
            .on_click({
                let view = view.clone();
                let key = key.to_owned();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.set_key_tag(&key, color, cx));
                }
            })
            .into_any_element()
    };

    let mut swatches = h_flex()
        .flex_none()
        .gap(tokens.spacing.xs)
        .child(swatch(None));
    for color in TagColor::ALL {
        swatches = swatches.child(swatch(Some(color)));
    }

    // 收藏：独立开关。收藏态用金星，未收藏用弱化色（**不是**灰掉——灰掉看起来
    // 像「不能点」，而它随时可点）。
    let star = icon_button(
        "redis-toggle-favorite",
        if favorite {
            tr(cx, "取消收藏")
        } else {
            tr(cx, "收藏这个键")
        },
        Icon::new(DuckIcon::Star)
            .size(extended.icon.sm)
            .text_color(if favorite {
                duck_theme::color_from_rgb(if dark { 0xd8b45c } else { 0xa8801a })
            } else {
                tokens.colors.muted_foreground
            }),
        ButtonVariant::Ghost,
        cx,
    )
    .on_click({
        let view = view.clone();
        let key = key.to_owned();
        move |_, _window, cx| {
            let _ = view.update(cx, |this, cx| this.toggle_key_favorite(&key, cx));
        }
    });

    v_flex()
        .w_full()
        .gap(tokens.spacing.xs)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(caption_heading(tr(cx, "标签与备注"), cx))
                .child(star),
        )
        .child(swatches)
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
                    Input::new(note_input)
                        .appearance(false)
                        .bordered(false)
                        .w_full(),
                ),
        )
        .into_any_element()
}
