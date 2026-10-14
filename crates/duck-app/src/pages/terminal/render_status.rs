//! 状态栏与 shell 级右栏会话详情的渲染。

use super::*;

impl TerminalPage {
    /// 底部状态栏：左侧连接信息，右侧传输速率。SFTP 标签改报目录信息。
    pub(super) fn render_status_bar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let spacing = tokens.spacing;
        let muted = tokens.colors.muted_foreground;
        let meta_size = tokens.typography.xs.size;
        let meta_line_height = tokens.typography.xs.line_height;
        let icon_size = extended.icon.sm;

        let tab = self.active_tab();
        let (status, host) = tab.map_or((LinkStatus::Disconnected, "—".to_owned()), |tab| {
            let host = self
                .connection_entry(&tab.id)
                .map_or_else(|| "localhost".to_owned(), |item| item.endpoint());
            (tab.status, host)
        });
        let is_sftp = tab.is_some_and(|tab| tab.kind == TabKind::Sftp);

        let cell = move |icon: DuckIcon, text: String| {
            h_flex()
                .flex_none()
                .items_center()
                .gap(spacing.xxs)
                .child(Icon::new(icon).size(icon_size).text_color(muted))
                .child(text)
        };

        let mut left = h_flex().min_w_0().items_center().gap(spacing.md).child(
            h_flex()
                .flex_none()
                .items_center()
                .gap(spacing.xs)
                .child(
                    div()
                        .size(px(STATUS_DOT_SIZE))
                        .flex_none()
                        .rounded_full()
                        .bg(status.color(cx)),
                )
                .child(tr(cx, status.label()))
                .child(host),
        );
        if is_sftp {
            left = left
                .child(cell(
                    DuckIcon::HardDrive,
                    self.pane(Side::Local).path.clone(),
                ))
                .child(cell(
                    DuckIcon::Folders,
                    // 远端路径为空 = 还没列举出来，显示「—」比显示空白清楚。
                    if self.pane(Side::Remote).path.trim().is_empty() {
                        "—".to_owned()
                    } else {
                        self.pane(Side::Remote).path.clone()
                    },
                ));
        } else {
            // 网格与回滚行数都是**真实读数**：前者来自内核的 `Dims`，
            // 后者取自起会话用的那份设置（「设置 → 终端」的「回滚行数」）
            // —— 都不是写死展示的数字。
            let (columns, rows) = tab
                .and_then(|tab| self.sessions.get(&tab.id))
                .map_or((0, 0), TerminalSession::grid);
            let scrollback = super::super::module_settings::terminal_settings(cx).scrollback;
            left = left.child(cell(DuckIcon::Globe, tr(cx, "UTF-8").to_owned()));
            if columns > 0 {
                left = left.child(cell(DuckIcon::Columns2, format!("{columns}×{rows}")));
            }
            left = left.child(cell(
                DuckIcon::Layers,
                trf(cx, "回滚 {} 行", &[&scrollback.to_string()]),
            ));
        }

        h_flex()
            .w_full()
            .h(theme.density().status_bar)
            .flex_none()
            .items_center()
            .justify_between()
            .gap(spacing.md)
            .px(spacing.sm)
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .text_size(meta_size)
            .line_height(meta_line_height)
            .font_features(tabular_numbers())
            .text_color(muted)
            .child(left)
            // 右侧只放**确定的事实**：应用版本来自编译期。
            // 此前这里挂的是「1.2 MB/s / 486 KB/s」两个写死的速率读数 ——
            // 终端没有可用的真实速率来源，宁可空着，也不摆两个会误导人的数字。
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(spacing.md)
                    .child(trf(cx, "v{}", &[env!("CARGO_PKG_VERSION")])),
            )
            .into_any_element()
    }

    /// 右侧栏内容：会话详情 + 传输队列摘要 + 快捷键。
    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let lang = theme.lang();
        let view = self.view.clone();

        let Some(tab) = self.active_tab() else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .px(tokens.spacing.lg)
                .child(meta_text(
                    tr(cx, "打开一个会话后，这里显示连接详情与传输队列。"),
                    cx,
                ))
                .into_any_element();
        };

        let connection = self.connection_entry(&tab.id);

        // 连接时长是**真实计时**：从会话起来的时刻算起（没有会话就是 `—`）。
        let uptime = self.sessions.get(&tab.id).map_or_else(
            || "—".to_owned(),
            |live| human_duration(live.elapsed(), lang),
        );

        let mut info = vec![
            description_row(t(lang, "会话"), body_text(tab.title.clone(), cx)),
            description_row(t(lang, "类型"), body_text(t(lang, tab.kind.label()), cx)),
            description_row(
                t(lang, "主机"),
                body_text(
                    connection
                        .as_ref()
                        .map_or_else(|| "—".to_owned(), |item| item.endpoint()),
                    cx,
                ),
            ),
            description_row(
                t(lang, "端口"),
                body_text(
                    connection
                        .as_ref()
                        .map_or_else(|| "—".to_owned(), |item| item.port.clone()),
                    cx,
                ),
            ),
            description_row(
                t(lang, "认证方式"),
                body_text(
                    connection.as_ref().map_or_else(
                        || t(lang, "—").to_owned(),
                        |item| format!("{} · {}", t(lang, item.auth.label()), item.auth_detail),
                    ),
                    cx,
                ),
            ),
            description_row(t(lang, "已连接"), body_text(uptime, cx)),
            description_row(
                t(lang, "状态"),
                status_cell(tab.status.color(cx), t(lang, tab.status.label()), cx),
            ),
        ];
        if tab.kind == TabKind::Sftp {
            info.push(description_row(
                t(lang, "本地目录"),
                body_text(self.pane(Side::Local).path.clone(), cx),
            ));
            info.push(description_row(
                t(lang, "远端目录"),
                body_text(self.pane(Side::Remote).path.clone(), cx),
            ));
        }

        let transfers = v_flex()
            .w_full()
            .children(self.transfers.iter().map(|transfer| {
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(
                        Icon::new(if transfer.upload {
                            DuckIcon::ArrowUp
                        } else {
                            DuckIcon::ArrowDown
                        })
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(transfer.name.clone()),
                    )
                    .child(meta_text(tr(cx, transfer.state.label()), cx))
            }));

        // 会话对应的连接 id（SFTP 标签取它的父连接）。
        let owner = tab
            .id
            .strip_suffix("/sftp")
            .unwrap_or(tab.id.as_str())
            .to_owned();
        // 内置的本地 shell 没有档案可编辑（`builtin` 就是为这句话存在的）。
        let editable = self
            .connection_entry(&owner)
            .is_some_and(|entry| !entry.builtin);

        let mut actions = h_flex().w_full().gap(tokens.spacing.xs).flex_wrap();
        // 只有远程会话才给「打开 SFTP 面板」：本地外壳没有远端文件可传。
        if tab.kind != TabKind::Local {
            let view = view.clone();
            let owner = owner.clone();
            actions = actions.child(
                icon_text_button(
                    "terminal-right-open-sftp",
                    tr(cx, "打开 SFTP 面板"),
                    Icon::new(DuckIcon::Folders)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.open_sftp_tab(&owner, window, cx));
                }),
            );
        }
        if editable {
            let view_edit = view.clone();
            let owner_edit = owner.clone();
            actions = actions.child(
                icon_text_button(
                    "terminal-right-edit",
                    tr(cx, "编辑连接"),
                    Icon::new(DuckIcon::Pencil)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = view_edit.update(cx, |this, cx| {
                        this.open_edit_connection(&owner_edit, window, cx);
                    });
                }),
            );
            let view_delete = view.clone();
            let owner_delete = owner.clone();
            actions = actions.child(
                icon_text_button(
                    "terminal-right-delete",
                    tr(cx, "删除连接"),
                    Icon::new(DuckIcon::Trash2)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.destructive),
                    ButtonVariant::Destructive,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = view_delete.update(cx, |this, cx| {
                        this.delete_connection(&owner_delete, window, cx);
                    });
                }),
            );
        }

        v_flex()
            .id("terminal-right-sidebar")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .gap(tokens.spacing.lg)
            .px(tokens.spacing.lg)
            .py(tokens.spacing.md)
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.sm)
                    .child(caption_heading(tr(cx, "会话详情"), cx))
                    .child(
                        card(cx)
                            .w_full()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.sm)
                            .child(description_list(info)),
                    )
                    .child(actions),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "传输队列"), cx))
                    .child(transfers),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "快捷键"), cx))
                    // 这里只列**真的绑了**的键。此前写的是「新建标签 ⌘T · 关闭标签 ⌘W ·
                    // 分屏 ⌘D」—— 三个一条都没绑（`app::install` 的 `"Terminal"` 上下文里
                    // 只有下面这些），等于教用户按三个没反应的键。
                    // 2026-10-12 补绑 `⌘T` / `⌘W` / `⌘C` / `⌘V` / `⌘A` 之后，
                    // 这一组与 `app::install` 的 `"Terminal"` 绑定表**逐条对应**。
                    .child(meta_text(tr(cx, "新建标签 ⌘T · 关闭标签 ⌘W"), cx))
                    .child(meta_text(
                        tr(cx, "复制 ⌘C（拖选文字后）· 粘贴 ⌘V · 全选 ⌘A"),
                        cx,
                    ))
                    .child(meta_text(
                        tr(cx, "清屏 ⌘K · 翻页 Shift+PageUp / Shift+PageDown"),
                        cx,
                    ))
                    .child(meta_text(tr(cx, "分屏 ⌘D（左右各一条独立会话）"), cx))
                    .child(meta_text(tr(cx, "行首 / 行尾 ⌘← / ⌘→"), cx))
                    .child(meta_text(tr(cx, "字号 ⌘+ / ⌘- / ⌘0"), cx))
                    .child(meta_text(tr(cx, "文件：单击选中 · 双击进入目录"), cx)),
            )
            .into_any_element()
    }
}
