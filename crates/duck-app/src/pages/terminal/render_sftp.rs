//! SFTP 双栏与传输队列的渲染。

use super::*;

impl TerminalPage {
    /// SFTP 双栏：两侧各自的路径栏 + 文件列表，底部传输队列。
    pub(super) fn render_sftp(&self, cx: &mut Context<Self>) -> AnyElement {
        // 两侧面板先取（需要 `&mut`）：主题引用一旦借上就借到函数末尾，见 `render_terminal`。
        let left_pane = self.render_file_pane(Side::Local, cx);
        let right_pane = self.render_file_pane(Side::Remote, cx);
        let queue = self.render_transfer_queue(cx);
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let stroke = extended.stroke.thin;
        let view = self.view.clone();
        let can_upload = self.pane(Side::Local).selected.is_some();
        let can_download = self.pane(Side::Remote).selected.is_some();
        let title = self
            .active_tab()
            .map_or_else(|| "sftp".to_owned(), |tab| tab.title.clone());

        let toolbar = h_flex()
            .w_full()
            .h(theme.density().section_header)
            .flex_none()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_b(stroke)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        Icon::new(DuckIcon::Folders)
                            .size(extended.icon.md)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(body_text(title, cx))
                    .child(meta_text(tr(cx, "双击目录进入 · 单击选中文件"), cx)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        toolbar_action_button(
                            "terminal-sftp-upload",
                            tr(cx, "上传选中项"),
                            Icon::new(DuckIcon::ArrowUp).size(extended.icon.md),
                            false,
                            !can_upload,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view
                                    .update(cx, |this, cx| this.queue_transfer(true, window, cx));
                            }
                        }),
                    )
                    .child(
                        toolbar_action_button(
                            "terminal-sftp-download",
                            tr(cx, "下载选中项"),
                            Icon::new(DuckIcon::ArrowDown).size(extended.icon.md),
                            false,
                            !can_download,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view
                                    .update(cx, |this, cx| this.queue_transfer(false, window, cx));
                            }
                        }),
                    )
                    .child(
                        toolbar_action_button(
                            "terminal-sftp-refresh-both",
                            tr(cx, "刷新两侧"),
                            Icon::new(DuckIcon::RotateCw).size(extended.icon.md),
                            false,
                            false,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.reload_pane(Side::Local, cx);
                                    this.reload_pane(Side::Remote, cx);
                                });
                            }
                        }),
                    ),
            );

        v_flex()
            .size_full()
            .min_h_0()
            .bg(tokens.colors.background)
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    // 左栏宽度 = 记忆占比（与分屏双终端共用同一条比例，见
                    // `render_pane_divider`）；右栏 `flex_1` 吃剩余空间。
                    .child(
                        div()
                            .h_full()
                            .min_w_0()
                            .min_h_0()
                            .w(relative(self.pane_ratio()))
                            .flex_none()
                            .child(left_pane),
                    )
                    .child(self.render_pane_divider(cx))
                    .child(right_pane)
                    // 拖动中比例实时跟手；松手清「正在拖动」标记。
                    .on_drag_move::<PaneDrag>(cx.listener(Self::on_pane_divider_drag))
                    .on_drop::<PaneDrag>(cx.listener(|this, _, _window, cx| {
                        if this.resizing_pane {
                            this.resizing_pane = false;
                            cx.notify();
                        }
                    })),
            )
            .child(queue)
            .into_any_element()
    }

    /// 单侧文件面板：面板标题 + 路径栏（上级 / 主目录 / 面包屑 / 打开目录 / 刷新）+ 列表。
    pub(super) fn render_file_pane(&self, side: Side, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let pane = self.pane(side);
        let can_up = data::parent(&pane.path).is_some();
        let crumbs = data::crumbs(&pane.path);
        // 只展示最后 3 段，更长的用省略号收口：长路径不能把右侧按钮挤出视口。
        let skip = crumbs.len().saturating_sub(3);

        let mut trail = h_flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(tokens.spacing.xxs)
            .overflow_x_hidden();
        if skip > 0 {
            trail = trail.child(meta_text(tr(cx, "…"), cx)).child(
                Icon::new(DuckIcon::ChevronRight)
                    .size(extended.icon.sm)
                    .text_color(extended.colors.text_tertiary),
            );
        }
        for (index, (path, label)) in crumbs.iter().enumerate().skip(skip) {
            let current = index + 1 == crumbs.len();
            if index > 0 {
                trail = trail.child(
                    Icon::new(DuckIcon::ChevronRight)
                        .size(extended.icon.sm)
                        .text_color(extended.colors.text_tertiary),
                );
            }
            let mut crumb = div()
                .id(SharedString::from(format!(
                    "terminal-crumb-{}-{index}",
                    side.index()
                )))
                .flex_none()
                .px(tokens.spacing.xs)
                .py(tokens.spacing.xxs)
                .rounded(tokens.radius.sm)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .child(label.clone());
            if current {
                crumb = crumb
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.foreground);
            } else {
                // 中间段可点：直接跳到那一级（比一路点「上一级」快）。
                let target = path.clone();
                crumb = crumb
                    .cursor_pointer()
                    .text_color(tokens.colors.muted_foreground)
                    .hover(|style| {
                        style
                            .bg(extended.colors.hover_tint)
                            .text_color(extended.colors.accent_text)
                    })
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ =
                                view.update(cx, |this, cx| this.navigate(side, target.clone(), cx));
                        }
                    });
            }
            trail = trail.child(crumb);
        }

        let path_bar = h_flex()
            .w_full()
            .h(theme.density().control)
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .px(tokens.spacing.sm)
            .child(
                toolbar_action_button(
                    SharedString::from(format!("terminal-up-{}", side.index())),
                    tr(cx, "上一级目录"),
                    Icon::new(DuckIcon::CornerLeftUp).size(extended.icon.md),
                    false,
                    !can_up,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            let path = this.pane(side).path.clone();
                            if let Some(parent) = data::parent(&path) {
                                this.navigate(side, parent, cx);
                            }
                        });
                    }
                }),
            )
            .child(
                toolbar_action_button(
                    SharedString::from(format!("terminal-home-{}", side.index())),
                    tr(cx, "主目录"),
                    Icon::new(DuckIcon::House).size(extended.icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            let home = side.home();
                            this.navigate(side, home, cx);
                        });
                    }
                }),
            )
            .child(
                // 竖分隔线：宽度取 hairline token（1px、不随缩放变粗），
                // 高度取最大档图标高——与同排按钮的视觉高度对齐。
                div()
                    .w(extended.stroke.thin)
                    .h(extended.icon.lg)
                    .flex_none()
                    .mx(tokens.spacing.xxs)
                    .bg(extended.colors.hairline),
            )
            .child(trail)
            .child(
                toolbar_action_button(
                    SharedString::from(format!("terminal-open-dir-{}", side.index())),
                    tr(cx, "打开目录"),
                    Icon::new(DuckIcon::FolderOpen).size(extended.icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.open_directory(side, window, cx));
                    }
                }),
            )
            .child(
                toolbar_action_button(
                    SharedString::from(format!("terminal-refresh-{}", side.index())),
                    tr(cx, "刷新"),
                    Icon::new(DuckIcon::RotateCw).size(extended.icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.reload_pane(side, cx));
                    }
                }),
            )
            // 远端才有的三档改动动作：新建目录 / 重命名 / 删除。
            //
            // 只给远端：本地面板是「上传的取材区」，改本机文件不是这个页面的职责
            //（要动本机文件用户有访达）；而这三个动作是 `duck_ssh` 的 mkdir / mv / rm，
            // 目标就是远端。放在路径栏而非顶部工具栏，是因为它们作用于**这一侧**。
            .when(!side.is_local(), |this| {
                let has_selection = pane.selected.is_some();
                this.child(
                    toolbar_action_button(
                        SharedString::from(format!("terminal-new-dir-{}", side.index())),
                        tr(cx, "新建文件夹"),
                        Icon::new(DuckIcon::FolderPlus).size(extended.icon.md),
                        false,
                        false,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ =
                                view.update(cx, |this, cx| this.new_remote_directory(window, cx));
                        }
                    }),
                )
                .child(
                    toolbar_action_button(
                        SharedString::from(format!("terminal-rename-{}", side.index())),
                        tr(cx, "重命名"),
                        Icon::new(DuckIcon::Pencil).size(extended.icon.md),
                        false,
                        !has_selection,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                if let Some(name) = this.pane(Side::Remote).selected.clone() {
                                    this.rename_remote_entry(name, window, cx);
                                }
                            });
                        }
                    }),
                )
                .child(
                    toolbar_action_button(
                        SharedString::from(format!("terminal-delete-{}", side.index())),
                        tr(cx, "删除"),
                        Icon::new(DuckIcon::Trash2).size(extended.icon.md),
                        true,
                        !has_selection,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                if let Some(name) = this.pane(Side::Remote).selected.clone() {
                                    this.delete_remote_entry(name, window, cx);
                                }
                            });
                        }
                    }),
                )
            });

        let column_header = h_flex()
            .w_full()
            .h(theme.density().section_header)
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(caption_heading(tr(cx, "名称"), cx)),
            )
            .child(
                div()
                    .w(px(SIZE_COLUMN_WIDTH))
                    .flex_none()
                    .child(caption_heading(tr(cx, "大小"), cx)),
            )
            .child(
                div()
                    .w(px(MODIFIED_COLUMN_WIDTH))
                    .flex_none()
                    .child(caption_heading(tr(cx, "修改时间"), cx)),
            );

        let rows = v_flex().w_full().children(pane.entries.iter().map(|entry| {
            let selected = pane.selected.as_deref() == Some(entry.name.as_str());
            let name = entry.name.clone();
            let is_dir = entry.dir;
            let row = h_flex()
                .id(SharedString::from(format!(
                    "terminal-file-{}-{name}",
                    side.index()
                )))
                .w_full()
                .h(theme.density().nav_row)
                .flex_none()
                .items_center()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.sm)
                .cursor_pointer()
                .rounded(tokens.radius.sm)
                // 文件行是「可激活的条目」：给读屏一个显式角色与名字
                //（自绘 on_click div 缺 role 时对辅助技术等于不存在）。
                .role(Role::ListItem)
                .aria_label(SharedString::from(entry.name.clone()))
                .when(selected, |this| {
                    this.bg(extended.colors.nav_selected)
                        .text_color(extended.colors.nav_selected_foreground)
                })
                .when(!selected, |this| {
                    this.hover(|style| style.bg(extended.colors.hover_tint))
                })
                .on_click({
                    let view = view.clone();
                    // `entry.name` 还要给下面的右键菜单用：按值交给点击闭包之前先克隆一份。
                    let click_name = name.clone();
                    move |event: &ClickEvent, window, cx| {
                        // 双击进入目录：`down.click_count` 是平台给的连击计数（第二次按下为 2）。
                        let double =
                            matches!(event, ClickEvent::Mouse(mouse) if mouse.down.click_count > 1);
                        let name = click_name.clone();
                        let _ = view.update(cx, |this, cx| {
                            this.click_entry(side, name, double, window, cx);
                        });
                    }
                })
                .child(
                    Icon::new(if is_dir {
                        DuckIcon::FolderOpen
                    } else {
                        DuckIcon::File
                    })
                    .size(extended.icon.md)
                    .text_color(if selected {
                        extended.colors.nav_selected_icon
                    } else {
                        tokens.colors.muted_foreground
                    }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .child(entry.name.clone()),
                )
                .child(
                    div()
                        .w(px(SIZE_COLUMN_WIDTH))
                        .flex_none()
                        .font_features(tabular_numbers())
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(if selected {
                            extended.colors.nav_selected_foreground
                        } else {
                            tokens.colors.muted_foreground
                        })
                        .child(entry.size.clone()),
                )
                .child(
                    div()
                        .w(px(MODIFIED_COLUMN_WIDTH))
                        .flex_none()
                        .font_features(tabular_numbers())
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(if selected {
                            extended.colors.nav_selected_foreground
                        } else {
                            extended.colors.text_tertiary
                        })
                        .child(entry.modified.clone()),
                );
            // 右键菜单只挂远端行：本地面板没有对应的远端动作（见路径栏那三档的注释）。
            if side.is_local() {
                row.into_any_element()
            } else {
                row.context_menu({
                    let view = view.clone();
                    let menu_name = name.clone();
                    // 菜单闭包是 `Fn`（每次打开都重跑），每一项各自克隆。
                    move |menu, _, cx| {
                        let menu = menu
                            .item(PopupMenuItem::new(tr(cx, "重命名")).on_click({
                                let name = menu_name.clone();
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.rename_remote_entry(name.clone(), window, cx);
                                    });
                                }
                            }))
                            .item(PopupMenuItem::new(tr(cx, "删除")).on_click({
                                let name = menu_name.clone();
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.delete_remote_entry(name.clone(), window, cx);
                                    });
                                }
                            }));
                        menu.separator()
                            .item(PopupMenuItem::new(tr(cx, "新建文件夹")).on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.new_remote_directory(window, cx);
                                    });
                                }
                            }))
                    }
                })
                .into_any_element()
            }
        }));

        // 空目录 / 读失败 / 未连接：说明**就写在列表位置**，不留一片空白让人以为界面坏了。
        let list: AnyElement = if pane.entries.is_empty() {
            v_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .py(tokens.spacing.xl)
                // 有说明就只说说明 ——「尚未连接」「没有权限读取」比「空目录」准确得多；
                // 没有说明时，才是真的空目录。
                .when(pane.notice.is_none(), |this| {
                    this.child(meta_text(tr(cx, "这个目录里没有可显示的条目。"), cx))
                })
                .children(
                    pane.notice
                        .as_ref()
                        .map(|notice| meta_text(notice.render(cx), cx)),
                )
                .into_any_element()
        } else {
            v_flex()
                .w_full()
                .when_some(pane.notice.as_ref(), |this, notice| {
                    this.child(
                        div()
                            .w_full()
                            .px(tokens.spacing.sm)
                            .py(tokens.spacing.xs)
                            .child(meta_text(notice.render(cx), cx)),
                    )
                })
                .child(rows)
                .into_any_element()
        };

        let pane_root = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0()
            .bg(tokens.colors.surface)
            .child(
                h_flex()
                    .w_full()
                    .h(theme.density().section_header)
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.sm)
                    .child(caption_heading(tr(cx, side.title()), cx))
                    .child(meta_text(
                        SharedString::from(trf(cx, "{} 项", &[&pane.entries.len().to_string()])),
                        cx,
                    )),
            )
            .child(path_bar)
            .child(column_header)
            .child(
                div()
                    .id(SharedString::from(format!(
                        "terminal-file-list-{}",
                        side.index()
                    )))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py(tokens.spacing.xxs)
                    .child(list),
            );

        // 访达拖入只挂在**远端**面板上：拖进来即上传到当前远端目录，拖到本地面板
        // 什么都不发生（本地面板是取材区，没有「传到哪里」的语义）。
        // 平台把文件拖放翻成一次以 `ExternalPaths` 为负载的内部拖拽（见
        // `Window::dispatch_event`），所以 `drag_over` / `on_drop` 就够。
        if side.is_local() {
            pane_root.into_any_element()
        } else {
            let tint = extended.colors.hover_tint;
            let drop_view = view.clone();
            pane_root
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(tint))
                .on_drop(move |paths: &ExternalPaths, window, cx| {
                    let files = paths.paths().to_vec();
                    let _ = drop_view.update(cx, |this, cx| {
                        this.upload_dropped_files(files, window, cx);
                    });
                })
                .into_any_element()
        }
    }

    /// 传输队列：一行一条，含方向图标、进度条与状态标签。
    pub(super) fn render_transfer_queue(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let running = self
            .transfers
            .iter()
            .filter(|transfer| transfer.state.is_active())
            .count();

        let rows = v_flex()
            .w_full()
            .children(self.transfers.iter().map(|transfer| {
                let active = transfer.state.is_active();
                // 读数来自**真实字节数**（落点大小 / 总大小，见 `set_transfer_progress`）。
                // 总大小量不到时（远端没有 `du` / `wc`）显示「—」而不是编一个百分比。
                let number = match transfer.state {
                    data::TransferState::Done => "100%".to_owned(),
                    data::TransferState::Active if transfer.total.is_some() => {
                        format!("{}%", (transfer.progress * 100.).round() as u32)
                    }
                    data::TransferState::Active | data::TransferState::Cancelling => "—".to_owned(),
                    _ => "0%".to_owned(),
                };
                h_flex()
                    .id(SharedString::from(format!(
                        "terminal-transfer-{}",
                        transfer.id
                    )))
                    .w_full()
                    .h(theme.density().nav_row)
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.sm)
                    .child(
                        Icon::new(if transfer.upload {
                            DuckIcon::ArrowUp
                        } else {
                            DuckIcon::ArrowDown
                        })
                        .size(extended.icon.md)
                        .text_color(if active {
                            extended.colors.accent_text
                        } else {
                            tokens.colors.muted_foreground
                        }),
                    )
                    .child(
                        div()
                            .w(px(TRANSFER_NAME_WIDTH))
                            .flex_none()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(transfer.name.clone()),
                    )
                    .child(div().flex_1().min_w_0().child(progress_bar(
                        SharedString::from(format!("terminal-progress-{}", transfer.name)),
                        transfer.progress,
                        cx,
                    )))
                    .child(
                        div()
                            .w(px(READOUT_WIDTH))
                            .flex_none()
                            .font_features(tabular_numbers())
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(tokens.colors.muted_foreground)
                            .child(number),
                    )
                    .child(div().flex_none().child(match transfer.state {
                        data::TransferState::Done => {
                            accent_tag(tr(cx, "已完成"), cx).into_any_element()
                        }
                        data::TransferState::Failed => {
                            // 失败时把原因带出来：一句「失败」什么也没说，
                            // 而 `scp` 的原文指得出下一步（权限？磁盘满？路径不对？）。
                            let reason = transfer
                                .reason
                                .clone()
                                .unwrap_or_else(|| tr(cx, "传输失败").to_owned());
                            neutral_tag(reason, cx).into_any_element()
                        }
                        other => neutral_tag(tr(cx, other.label()), cx).into_any_element(),
                    }))
                    // 取消：只在真正进行中（Active）时给，点了立刻 `kill` 进程；
                    // `Cancelling` 期间不给（换成置灰的说明），避免连点与「点了没反应」。
                    .when(transfer.state == data::TransferState::Active, |this| {
                        let id = transfer.id;
                        let view = self.view.clone();
                        this.child(
                            icon_text_button(
                                SharedString::from(format!("terminal-transfer-cancel-{id}")),
                                tr(cx, "取消"),
                                Icon::new(DuckIcon::Pause)
                                    .size(extended.icon.md)
                                    .text_color(tokens.colors.muted_foreground),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .on_click(move |_, _window, cx| {
                                let _ = view.update(cx, |this, cx| this.cancel_transfer(id, cx));
                            }),
                        )
                    })
                    // 重试：只在失败后给 —— 拿冻结的输入快照再跑一遍。
                    .when(transfer.state == data::TransferState::Failed, |this| {
                        let id = transfer.id;
                        let view = self.view.clone();
                        this.child(
                            icon_text_button(
                                SharedString::from(format!("terminal-transfer-retry-{id}")),
                                tr(cx, "重试"),
                                Icon::new(DuckIcon::RotateCw)
                                    .size(extended.icon.md)
                                    .text_color(tokens.colors.muted_foreground),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .on_click(move |_, _window, cx| {
                                let _ = view.update(cx, |this, cx| this.retry_transfer(id, cx));
                            }),
                        )
                    })
            }));

        v_flex()
            .w_full()
            .flex_none()
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .w_full()
                    .h(theme.density().section_header)
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .px(tokens.spacing.sm)
                    .child(caption_heading(tr(cx, "传输队列"), cx))
                    .child(meta_text(
                        SharedString::from(trf(
                            cx,
                            "{}/{} 个任务",
                            &[&running.to_string(), &self.transfers.len().to_string()],
                        )),
                        cx,
                    )),
            )
            .child(rows)
            .into_any_element()
    }
}
