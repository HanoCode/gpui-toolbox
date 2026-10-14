//! 标签栏与页面主体骨架（含空态）的渲染。

use super::*;

impl TerminalPage {
    /// 会话标签页：图标 + 名称 + 状态点 + 关闭；激活标签以内容区底色「贴」在下方。
    pub(super) fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let mut row = h_flex()
            .min_w_0()
            .flex_1()
            .gap(tokens.spacing.xxs)
            .overflow_x_hidden();

        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active;
            let (foreground, weight) = if active {
                (tokens.colors.foreground, FontWeight::MEDIUM)
            } else {
                (tokens.colors.muted_foreground, FontWeight::NORMAL)
            };

            let close = toolbar_action_button(
                SharedString::from(format!("terminal-tab-close-{}", tab.id)),
                tr(cx, "关闭标签页"),
                // 标签页内嵌的关闭 ✕ 有意比 `icon.md` 小一档（三个页面的标签条都取 sm，
                // 含工具箱用 `icon_button` 画的那处）；代价是按钮 26×28 不方 —— 已知并接受。
                Icon::new(DuckIcon::X).size(extended.icon.sm),
                false,
                false,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.close_tab_at(index, cx));
                }
            });

            row = row.child(
                h_flex()
                    .id(SharedString::from(format!("terminal-tab-{}", tab.id)))
                    .relative()
                    .h(theme.density().control)
                    .min_w_0()
                    .max_w(px(TAB_MAX_WIDTH))
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .pl(tokens.spacing.sm)
                    .pr(tokens.spacing.xs)
                    .rounded(tokens.radius.md)
                    .cursor_pointer()
                    .text_color(foreground)
                    .text_size(tokens.typography.sm.size)
                    .font_weight(weight)
                    // 会话标签是「一组选一个」：显式给读屏 Tab 角色与选中态
                    //（自绘 on_click div 缺 role 时对辅助技术等于不存在）。
                    .role(Role::Tab)
                    .aria_selected(active)
                    .when(active, |this| this.bg(tokens.colors.surface))
                    .hover(move |style| {
                        style
                            .bg(extended.colors.hover_tint)
                            .text_color(extended.colors.accent_text)
                    })
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.activate(index, cx));
                        }
                    })
                    // 拖拽源：把标签拖到画布右半屏可并入分屏（见 `render_terminal_canvas`）。
                    // 幽灵视图在拖拽开始时构造一次；顺手记下「在拖谁」，让画布显示投放区。
                    .on_drag(
                        TabDrag {
                            tab_id: tab.id.clone(),
                        },
                        {
                            let view = view.clone();
                            let title = tab.title.clone();
                            move |drag: &TabDrag, _point, _window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    if this.dragging_tab.as_deref() != Some(drag.tab_id.as_str()) {
                                        this.dragging_tab = Some(drag.tab_id.clone());
                                        this.split_drop_hover = false;
                                        // 互清连接拖拽的残留，理由同连接拖拽源
                                        //（`has_active_drag()` 只答「有没有拖拽」，
                                        // 两类载荷共用它做渲染闸，残留旧值会串档）。
                                        this.dragging_conn = None;
                                        this.conn_drop_hover = None;
                                        cx.notify();
                                    }
                                });
                                cx.new(|_| TabDragGhost {
                                    icon: DuckIcon::SquareTerminal,
                                    label: title.clone(),
                                })
                            }
                        },
                    )
                    .context_menu({
                        // 菜单闭包是 `'static`（且每次打开都重跑）→ 不能借循环里的 `tab`，
                        // 字段先取出来；`on_click` 各自再克隆一份。
                        let tab_id = tab.id.clone();
                        let tab_kind = tab.kind;
                        let view = view.clone();
                        move |menu, _, cx| {
                            // SFTP 标签没有自己的会话，「重新连接」落在它的父连接上
                            //（SFTP 面板依赖同一条 ControlMaster 物理连接）。
                            let reconnect_target = match tab_kind {
                                TabKind::Ssh => Some(tab_id.clone()),
                                TabKind::Sftp => Some(
                                    tab_id
                                        .strip_suffix("/sftp")
                                        .map_or_else(|| tab_id.clone(), str::to_owned),
                                ),
                                TabKind::Local => None,
                            };
                            let mut menu = menu;
                            // `reconnect_target` 下面还要给「克隆连接」用一次，
                            // 这一处先克隆（它只是个短字符串，不值得为省一次克隆改结构）。
                            if let Some(target) = reconnect_target.clone() {
                                menu =
                                    menu.item(PopupMenuItem::new(tr(cx, "重新连接")).on_click({
                                        let view = view.clone();
                                        move |_, _, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.reconnect(&target, cx);
                                            });
                                        }
                                    }));
                            }
                            // 只有 SSH 标签有「远端文件」可看：本地 shell 不是一台
                            // 远端主机，SFTP 标签已经在那儿了 —— 两种都不给这一项。
                            if tab_kind == TabKind::Ssh {
                                menu = menu.item(
                                    PopupMenuItem::new(tr(cx, "打开 SFTP 面板")).on_click({
                                        let id = tab_id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_sftp_tab(&id, window, cx);
                                            });
                                        }
                                    }),
                                );
                            }
                            // 克隆这条标签所属的连接：与连接列表右键菜单是**同一个动作**
                            //（复制一份可改的档案，不自动连）。本地 shell 没有档案，不给。
                            if let Some(owner) = reconnect_target {
                                menu =
                                    menu.item(PopupMenuItem::new(tr(cx, "克隆连接")).on_click({
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.clone_connection(&owner, window, cx);
                                            });
                                        }
                                    }));
                            }
                            menu.separator()
                                .item(PopupMenuItem::new(tr(cx, "关闭标签页")).on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.close_tab_at(index, cx);
                                        });
                                    }
                                }))
                                .item(PopupMenuItem::new(tr(cx, "关闭其他标签页")).on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.close_other_tabs(index, cx);
                                        });
                                    }
                                }))
                                .item(PopupMenuItem::new(tr(cx, "关闭全部标签页")).on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| this.close_all_tabs(cx));
                                    }
                                }))
                        }
                    })
                    .child(
                        Icon::new(tab.kind.icon())
                            .size(extended.icon.md)
                            .text_color(if active {
                                extended.colors.accent_text
                            } else {
                                tokens.colors.muted_foreground
                            }),
                    )
                    .child(div().min_w_0().truncate().child(tab.title.clone()))
                    .child(
                        div()
                            .size(px(STATUS_DOT_SIZE))
                            .flex_none()
                            .rounded_full()
                            .bg(tab.status.color(cx)),
                    )
                    .child(close),
            );
        }

        h_flex()
            .w_full()
            // 标签条是 chrome：比控件高一个 `sm`，让标签底部与内容区之间留出呼吸位。
            .h(theme.density().control + tokens.spacing.sm)
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(row)
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        toolbar_action_button(
                            "terminal-new-tab",
                            tr(cx, "新建标签页"),
                            Icon::new(DuckIcon::Plus).size(extended.icon.md),
                            false,
                            false,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.new_local_tab(cx));
                            }
                        }),
                    )
                    .child({
                        // 只对终端会话有意义：SFTP 标签的内容是一对文件面板，没有 PTY
                        // 可分。禁用时把原因写进标签（悬停提示与读屏都会念到它）。
                        let split_label = match self.active_tab() {
                            None => tr(cx, "分屏"),
                            Some(tab) if tab.kind == TabKind::Sftp => {
                                tr(cx, "分屏（仅限终端会话）")
                            }
                            Some(tab) if tab.split.is_some() => tr(cx, "取消分屏"),
                            Some(_) => tr(cx, "分屏"),
                        };
                        let can_split = self
                            .active_tab()
                            .is_some_and(|tab| tab.kind != TabKind::Sftp);
                        toolbar_action_button(
                            "terminal-split",
                            split_label,
                            Icon::new(DuckIcon::Columns2).size(extended.icon.md),
                            false,
                            !can_split,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.toggle_split(cx));
                            }
                        })
                    })
                    .child({
                        // 目标是「当前标签所属的连接」或「上次开过 SFTP 的连接」；
                        // 两个都没有（一台远端主机都还没连过）才置灰，并把原因写进标签。
                        let owner = self.sftp_owner();
                        let label = if owner.is_some() {
                            tr(cx, "打开 SFTP 面板")
                        } else {
                            tr(cx, "打开 SFTP 面板（需先连接远程主机）")
                        };
                        toolbar_action_button(
                            "terminal-open-sftp-tab",
                            label,
                            Icon::new(DuckIcon::Folders).size(extended.icon.md),
                            false,
                            owner.is_none(),
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    let Some(id) = this.sftp_owner() else {
                                        return;
                                    };
                                    this.open_sftp_tab(&id, window, cx);
                                });
                            }
                        })
                    }),
            )
            .into_any_element()
    }

    /// 内容区：按激活标签的类型分派（终端画布 / SFTP 面板 / 空态）。
    pub(super) fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.active_tab() {
            Some(tab) => match tab.kind {
                TabKind::Sftp => self.render_sftp(cx),
                TabKind::Local | TabKind::Ssh => self.render_terminal(cx),
            },
            None => self.render_empty(cx),
        }
    }

    /// 空态：没有打开任何会话时引导去左栏选连接。
    pub(super) fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .min_h_0()
            .items_center()
            .justify_center()
            .bg(active_theme(cx).tokens().colors.surface)
            .child(
                empty_state(
                    DuckIcon::SquareTerminal,
                    tr(cx, "暂无打开的会话"),
                    Some(SharedString::from(tr(
                        cx,
                        "从左侧选择一个连接，或新建连接后回车打开。",
                    ))),
                    None,
                    cx,
                )
                .border_0(),
            )
            .into_any_element()
    }
}
