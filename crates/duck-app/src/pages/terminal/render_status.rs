//! 状态栏与 shell 级右栏会话详情的渲染。

use super::*;
use crate::pages::terminal::keys::{KeyScope, shortcuts};

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

        // 路径格：宽度封顶 + 省略号收口 + 点一下就复制。
        //
        // 以前两条完整路径都往左段里塞：`/usr/local/share/doc/nginx/changelog.gz`
        // 这种路径一条就能挤掉右段的版本号，而它**没有截断**，长到把布局撑破。
        // 「看一眼完整路径」与「复制它」是同一件事的两种表达，所以点击即复制。
        let path_cell = |icon: DuckIcon, label: &'static str, path: String| {
            let copy = path.clone();
            h_flex()
                .id(SharedString::from(format!("terminal-status-path-{label}")))
                .flex_none()
                .min_w_0()
                .max_w(px(STATUS_PATH_MAX_WIDTH))
                .items_center()
                .gap(spacing.xxs)
                .rounded(tokens.radius.sm)
                .px(spacing.xxs)
                .cursor_pointer()
                .role(Role::Button)
                .aria_label(SharedString::from(path.clone()))
                .hover(move |style| style.bg(extended.colors.hover_tint))
                .on_click(move |_, window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()));
                    notify::success(trf(cx, "已复制路径：{}", &[&copy]), window, cx);
                })
                .child(Icon::new(icon).size(icon_size).text_color(muted))
                .child(div().min_w_0().truncate().child(format!("{label} {path}")))
        };

        // 左段可被右段挤：`min_w_0` 让出空间，`overflow_x_hidden` 收口 ——
        // 窄窗口时读数格子依次被裁，版本号（右段）永远完整可见。
        let mut left = h_flex()
            .min_w_0()
            .overflow_x_hidden()
            .items_center()
            .gap(spacing.md)
            .child(
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
            // 只报**当前那一侧**的路径。
            //
            // 以前两栏的路径都往状态栏塞：两段完整路径加起来必然撑破左段，而状态栏
            // 里读者只关心「我正在看的那一栏在哪」。想复制另一栏的路径，
            // 点那一栏就会切过来（`focused_side`）。
            let side = self.focused_side;
            let path = self.pane(side).path.clone();
            let shown = if path.trim().is_empty() {
                // 远端路径为空 = 还没列举出来，显示「—」比显示空白清楚。
                "—".to_owned()
            } else {
                path
            };
            let icon = if side.is_local() {
                DuckIcon::HardDrive
            } else {
                DuckIcon::Folders
            };
            left = left.child(path_cell(icon, tr(cx, side.title()), shown));
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
                    .child(selectable_text(trf(
                        cx,
                        "v{}",
                        &[env!("CARGO_PKG_VERSION")],
                    ))),
            )
            .into_any_element()
    }

    /// 右栏该「审视」的那条档案：选中的连接，且它**没有活着的会话**。
    ///
    /// 三条排除：没选中过、选中项已不在档案表里（被删了）、以及内置的本地 shell
    ///（它不是一台远端主机，详情没有意义）。
    fn inspectable_profile(&self) -> Option<String> {
        let owner = self.selected_profile.as_deref()?;
        if owner == data::LOCAL_SHELL_ID || self.profile(owner).is_none() {
            return None;
        }
        // 连着的那条不抢右栏：那时用户看的是会话本身。
        (self.link_status(owner) != LinkStatus::Connected).then(|| owner.to_owned())
    }

    /// 右栏的「连接档案详情」：一台**还没连上**的机器在看什么、怎么连。
    fn render_profile_detail(&self, owner: &str, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let lang = theme.lang();
        let view = self.view.clone();
        let Some(entry) = self.connection_entry(owner) else {
            return v_flex().size_full().into_any_element();
        };

        let info = vec![
            description_row(t(lang, "名称"), body_text(entry.label.clone(), cx)),
            description_row(t(lang, "主机"), body_text(entry.endpoint(), cx)),
            description_row(t(lang, "端口"), body_text(entry.port.clone(), cx)),
            description_row(
                t(lang, "认证方式"),
                body_text(
                    format!("{} · {}", t(lang, entry.auth.label()), entry.auth_detail),
                    cx,
                ),
            ),
            description_row(
                t(lang, "状态"),
                status_cell(entry.status.color(cx), t(lang, entry.status.label()), cx),
            ),
        ];

        let open = {
            let id = owner.to_owned();
            let view_open = view.clone();
            icon_text_button(
                "terminal-profile-open",
                tr(cx, "连接"),
                Icon::new(DuckIcon::ArrowRightLeft)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Primary,
                cx,
            )
            .on_click(move |_, window, cx| {
                let _ = view_open.update(cx, |this, cx| this.open_connection(&id, window, cx));
            })
        };
        let edit = {
            let id = owner.to_owned();
            let view_edit = view.clone();
            icon_text_button(
                "terminal-profile-edit",
                tr(cx, "编辑连接"),
                Icon::new(DuckIcon::Pencil)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, window, cx| {
                let _ = view_edit.update(cx, |this, cx| {
                    this.open_edit_connection(&id, window, cx);
                });
            })
        };

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
                    .child(caption_heading(tr(cx, "连接详情"), cx))
                    .child(
                        card(cx)
                            .w_full()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.sm)
                            .child(description_list(info)),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap(tokens.spacing.xs)
                            .flex_wrap()
                            .child(open)
                            .child(edit),
                    ),
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
            // 没有活动会话：如果用户选中过某条档案（左栏单击 / 右键），
            // 就显示**这条档案**的详情。以前这里恒定是「打开一个会话后…」，
            // 于是「这台机器到底连的是哪个端口」在连上之前无处可看。
            if let Some(owner) = self.inspectable_profile() {
                return self.render_profile_detail(&owner, cx);
            }
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

        // 选中一条**当前没连着**的档案时，右栏让位给这条档案的详情：
        // 「我正在看的那台机器」优先于「我上一条会话」。连着的那条不抢 ——
        // 那时用户显然在看会话本身。
        if let Some(owner) = self.inspectable_profile() {
            return self.render_profile_detail(&owner, cx);
        }

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
            // 「删除连接」**只留在左栏的右键菜单里**。
            // 详情面板里放一个红色按钮，等于把「我刚点开这台机器」和
            // 「永久删掉它」放在同一块 40px 高的区域里 —— 那个误点的代价太大，
            // 而删除本身又不是一个需要「顺手」的动作。
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
                    .gap(tokens.spacing.xxs)
                    .child(caption_heading(tr(cx, "快捷键"), cx))
                    // 这一段**从绑定表生成**（`terminal/keys.rs`），这里不写任何键位。
                    // 此前是六行手写文案，其中「新建标签 ⌘T / 关闭标签 ⌘W / 分屏 ⌘D」
                    // 三条一条都没绑 —— 教用户按三个没反应的键，比不教更糟。
                    .children(shortcut_rows(cx)),
            )
            .into_any_element()
    }
}

/// 右栏的快捷键清单：按上下文分两组，行内「说明 ─ 键位」。
///
/// 键位用等宽族、右对齐、次要色：它是**参数**而不是标题，读的时候眼睛扫的是左边
/// 那列说明。整块内容与绑定表同源，加一条快捷键只需改 `terminal/keys.rs`。
fn shortcut_rows(cx: &App) -> Vec<AnyElement> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let mut rows: Vec<AnyElement> = Vec::new();
    for scope in [KeyScope::Canvas, KeyScope::Page] {
        rows.push(
            div()
                .w_full()
                .pt(tokens.spacing.xs)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(tr(cx, scope.label()))
                .into_any_element(),
        );
        for shortcut in shortcuts()
            .iter()
            .filter(|shortcut| shortcut.scope() == scope && shortcut.shown())
        {
            rows.push(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap(tokens.spacing.sm)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(tr(cx, shortcut.label())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(tokens.typography.mono.clone())
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(tokens.colors.muted_foreground)
                            .child(shortcut.display()),
                    )
                    .into_any_element(),
            );
        }
    }
    rows
}
