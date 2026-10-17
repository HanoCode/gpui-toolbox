//! 标签栏与页面主体骨架（含空态）的渲染。

use super::*;

use duck_term::Encoding;

impl TerminalPage {
    /// 会话标签页：图标 + 名称（+ OSC 副标题）+ 状态点 / 关闭；激活标签以内容区底色「贴」在下方。
    pub(super) fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let mut row = h_flex()
            .id("terminal-tab-row")
            .min_w_0()
            .flex_1()
            .gap(tokens.spacing.xxs)
            .overflow_x_hidden()
            // 双击标签条空白处 = 新建本地终端（浏览器 / 访达的同一个手势）。
            // 挂在 `row` 而不是整条标签栏上：右侧那排按钮不该被这个手势波及，
            // 而 `row` 恰好只装标签、且 `flex_1` 吃满空白区。
            .on_click({
                let view = view.clone();
                move |event: &ClickEvent, _, cx| {
                    let double =
                        matches!(event, ClickEvent::Mouse(mouse) if mouse.down.click_count > 1);
                    if !double {
                        return;
                    }
                    let _ = view.update(cx, |this, cx| this.new_local_tab(cx));
                }
            });

        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active;
            let hovered = self.hovered_tab.as_deref() == Some(tab.id.as_str());
            // 休眠标签（上次退出时恢复出来的、还没点过）用**三级**文字色：
            // 它与「未选中」是两件事 —— 未选中是「在，但不在前台」，休眠是
            // 「还没连上」。同一档灰会让用户以为那条标签已经连着了。
            let (foreground, weight) = if active {
                (tokens.colors.foreground, FontWeight::MEDIUM)
            } else if tab.dormant {
                (extended.colors.text_tertiary, FontWeight::NORMAL)
            } else {
                (tokens.colors.muted_foreground, FontWeight::NORMAL)
            };
            // 编码菜单要标出「当前是哪一档」，而菜单闭包是 `'static` 且每次打开重跑 ——
            // 值在这里（渲染期）取好，闭包里只捕获这个 `Copy` 枚举。
            let tab_encoding = self
                .sessions
                .get(tab.id.as_str())
                .map_or_else(Encoding::default, |live| live.encoding());

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
            // 宽度钉死：未悬停时那个占位必须与它**同宽**，否则整排标签会在指针
            // 移上移下时左右跳一下。
            .w(px(TAB_CLOSE_SLOT))
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    // 关闭按钮在标签内部：不掐断冒泡就会顺带触发「切到这个标签」。
                    cx.stop_propagation();
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
                    // **不设 `flex_none`**：标签条是唯一要「让」的地方 —— 窗口变窄时
                    // 标签按 flex-shrink 均匀收缩、文字省略号收口，而不是被
                    // `overflow_x_hidden` 整个裁掉最后一个。内容（序号/图标/关闭槽）
                    // 各自 `flex_none`，收缩全部由标题承担。
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
                    // 读屏要能报出「这是第几个标签」：`⌘1..9` 是按**位置**切的，
                    // 位置就是它的名字，缺了这个数字，那九条快捷键对读屏用户等于不存在。
                    .aria_label(SharedString::from(if tab.dormant {
                        trf(
                            cx,
                            "第 {} 个标签：{}（未连接，点击连接）",
                            &[&(index + 1).to_string(), tab.title.as_str()],
                        )
                    } else {
                        trf(
                            cx,
                            "第 {} 个标签：{}",
                            &[&(index + 1).to_string(), tab.title.as_str()],
                        )
                    }))
                    .when(active, |this| this.bg(tokens.colors.surface))
                    .hover(move |style| {
                        style
                            .bg(extended.colors.hover_tint)
                            .text_color(extended.colors.accent_text)
                    })
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            // 标签行的空白处挂着重双击新建：点标签本身不能再冒泡过去。
                            cx.stop_propagation();
                            let _ = view.update(cx, |this, cx| this.activate(index, cx));
                        }
                    })
                    // 中键关闭（浏览器标签的共识手势）：不用先瞄准那个小 ✕。
                    .on_mouse_down(gpui::MouseButton::Middle, {
                        let view = view.clone();
                        move |_, _, cx| {
                            cx.stop_propagation();
                            let _ = view.update(cx, |this, cx| this.close_tab_at(index, cx));
                        }
                    })
                    // 悬停才出关闭按钮：一个藏起来的目标不会被误点。
                    .on_hover({
                        let view = view.clone();
                        let id = tab.id.clone();
                        move |hovered: &bool, _, cx| {
                            let hovered = *hovered;
                            let _ = view.update(cx, |this, cx| {
                                let next = hovered.then(|| id.clone());
                                if this.hovered_tab != next {
                                    this.hovered_tab = next;
                                    cx.notify();
                                }
                            });
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
                                                // 与画布上那条横幅的按钮走同一个入口
                                                //（`reconnect_session`）：手动重连要能重放
                                                // 当初的 argv，且不留下两套「重连」语义。
                                                this.reconnect_session(&target, cx);
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
                            // 克隆这条标签所属的连接到**新标签页**：再开一条同主机的
                            // 会话（与 `⌘T` 同一入口），不是复制档案 —— 复制档案
                            // 在左栏连接的右键菜单里，两处「克隆」语义有意分开。
                            // 本地 shell 没有主机可言，不给。
                            if let Some(owner) = reconnect_target {
                                menu = menu.item(
                                    PopupMenuItem::new(tr(cx, "再开一条会话")).on_click({
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_second_session(&owner, window, cx);
                                            });
                                        }
                                    }),
                                );
                            }
                            // 编码：GBK / GB2312 的输出**与输入**都要客户端转码，否则
                            // 文件名、`cat gbk.txt`、vim 提示全变成替换符。菜单没有
                            // 勾选位，用行首的 ✓ 标当前档（占两位宽，两行左边对齐）。
                            // SFTP 标签没有 PTY 输出，不给这两项。
                            if tab_kind != TabKind::Sftp {
                                for encoding in Encoding::ALL {
                                    let mark = if encoding == tab_encoding {
                                        "✓ "
                                    } else {
                                        "  "
                                    };
                                    let name: &str = &module_settings::encoding_label(cx, encoding);
                                    let label = trf(cx, "{}编码：{}", &[mark, name]);
                                    // 闭包是 `'static`：只能碰早已克隆出来的 `tab_id`，
                                    // 不能借循环里的 `tab`（借了会以整条标签的生命周期外逃）。
                                    let id = tab_id.clone();
                                    let view = view.clone();
                                    menu = menu.item(PopupMenuItem::new(label).on_click(
                                        move |_, _, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.set_tab_encoding(&id, encoding, cx);
                                            });
                                        },
                                    ));
                                }
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
                    // 序号：`⌘1..9` 按位置切标签，位置写在标签上才学习得到。
                    // 只画前九个 —— 第 10 个之后没有直达键，画个数字反而是假的承诺。
                    .when(index < 9, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .font_features(tabular_numbers())
                                .text_size(tokens.typography.xs.size)
                                .text_color(if active {
                                    tokens.colors.foreground
                                } else {
                                    tokens.colors.muted_foreground
                                })
                                .child(SharedString::from((index + 1).to_string())),
                        )
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
                    .child(
                        // 名称 + 副标题（OSC 标题）：名称是档案名，永远不变；
                        // 副标题是远端程序（`vim` / `top`）自报的那一行，弱化显示。
                        h_flex()
                            .min_w_0()
                            .items_baseline()
                            .gap(tokens.spacing.xxs)
                            .child(div().min_w_0().truncate().child(tab.title.clone()))
                            .when_some(tab.subtitle.clone(), |this, subtitle| {
                                this.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(tokens.typography.xs.size)
                                        .text_color(tokens.colors.muted_foreground)
                                        .child(subtitle),
                                )
                            }),
                    )
                    .child(if hovered {
                        // 同一个槽位：悬停换成 ✕，不悬停仍是状态点。
                        // 两者占位同宽，指针进出时不会把邻居挤走。
                        div()
                            .w(px(TAB_CLOSE_SLOT))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .child(close)
                    } else {
                        div()
                            .w(px(TAB_CLOSE_SLOT))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .size(px(STATUS_DOT_SIZE))
                                    .rounded_full()
                                    .bg(tab.status.color(cx)),
                            )
                    }),
            );
        }

        // 右侧工具按钮：未分屏时跟在标签后面；分屏时贴整条栏的最右
        //（分区分支里再包一层留出内边距，见下）。
        let buttons = h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .child(
                toolbar_action_button(
                    "terminal-new-tab",
                    tr(cx, "新建连接（搜索 / 直接连 / 新建）"),
                    Icon::new(DuckIcon::Plus).size(extended.icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.open_quick_connect(window, cx));
                    }
                }),
            )
            .child({
                // 只对终端会话有意义：SFTP 标签的内容是一对文件面板，没有 PTY
                // 可分。禁用时把原因写进标签（悬停提示与读屏都会念到它）。
                let split_label = match self.active_tab() {
                    None => tr(cx, "分屏"),
                    Some(tab) if tab.kind == TabKind::Sftp => tr(cx, "分屏（仅限终端会话）"),
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
                // 终端工具条的**可见入口**：工具条本体是浮层，唤出全靠画布
                // 顶部那条看不见的热区 —— 图标给「调节」一个看得见的把手，
                // 点它 = 钉住 / 收起（与浮层上的旗子是同一个状态）。
                let pinned = self.toolbar_pinned;
                let label = if pinned {
                    tr(cx, "取消固定")
                } else {
                    tr(cx, "固定工具条")
                };
                toolbar_action_button(
                    "terminal-toolbar-toggle",
                    label,
                    Icon::new(DuckIcon::SlidersHorizontal).size(extended.icon.md),
                    pinned,
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.toolbar_pinned = !this.toolbar_pinned;
                            this.toolbar_hovered = false;
                            cx.notify();
                        });
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
            });

        // 分屏时标签栏**按窗格分区**：左区装原有会话标签（宽度 = 左窗格同比例），
        // 右区装分屏窗格标签（[`Self::render_pane_tab`]），中间留一条与隔离栏同宽
        // 的占位。三个区块用与画布**完全相同的弹性结构**（`relative(ratio)` 左 +
        // `sm` 隔条 + `flex_1` 右），窗格标签才与右窗格上下对齐 —— 所以这条分支
        // 刻意不带整栏的 `px` / `gap`（它们会把隔离线的横向位置顶偏），内边距
        // 改由左右两区各自承担。
        // `theme` / `tokens` / `extended` 都借着 `cx`，先用不了再还：分支里要
        // 可变借 `cx`（`render_pane_tab`），所以标量在这里提前取好。
        let bar_h = theme.density().control + tokens.spacing.sm;
        if self.active_tab().is_some_and(|tab| tab.split.is_some()) {
            let bar_sm = tokens.spacing.sm;
            let bar_xs = tokens.spacing.xs;
            let (chrome, hairline) = (extended.colors.chrome, extended.colors.hairline);
            let stroke = extended.stroke.thin;
            let pane_tab = self.render_pane_tab(cx);
            return h_flex()
                .w_full()
                .h(bar_h)
                .flex_none()
                .items_center()
                .bg(chrome)
                .border_b(stroke)
                .border_color(hairline)
                .child(
                    div()
                        .h_full()
                        .min_w_0()
                        .w(relative(self.pane_ratio()))
                        .flex_none()
                        .pl(bar_sm)
                        .child(row),
                )
                .child(div().w(bar_sm).h_full().flex_none())
                .child(
                    h_flex()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .items_center()
                        .children(pane_tab),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .h_full()
                        .items_center()
                        .pl(bar_xs)
                        .pr(bar_sm)
                        .child(buttons),
                )
                .into_any_element();
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
            .child(buttons)
            .into_any_element()
    }

    /// 内容区：按激活标签的类型分派（终端画布 / SFTP 面板 / 空态）。
    pub(super) fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.active_tab() {
            // 休眠标签（上次退出时恢复的）：还没连，画布上没有会话可画 ——
            // 直接走 `render_terminal` 只会得到一块空白，像是卡住了。
            Some(tab) if tab.dormant => self.render_dormant(cx),
            Some(tab) => match tab.kind {
                TabKind::Sftp => self.render_sftp(cx),
                TabKind::Local | TabKind::Ssh => self.render_terminal(cx),
            },
            None => self.render_empty(cx),
        }
    }

    /// 休眠标签的占位：说清楚「这是上次留下的标签，还没连」，并给一键连上。
    ///
    /// 不自动连的理由见 [`SessionTab::dormant`]；但**也不能只是灰着不给出口** ——
    /// 恢复出来的标签正好是当前标签时，用户看到的会是一整块空白。
    pub(super) fn render_dormant(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = self.view.clone();
        let index = self.active;
        let title = self
            .tabs
            .get(index)
            .map_or_else(String::new, |tab| tab.title.clone());
        v_flex()
            .size_full()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap(active_theme(cx).tokens().spacing.sm)
            .bg(active_theme(cx).tokens().colors.surface)
            .child(
                empty_state(
                    DuckIcon::SquareTerminal,
                    trf(cx, "「{}」还没连接", &[title.as_str()]),
                    Some(SharedString::from(tr(
                        cx,
                        "这是上次退出时开着的标签。点「连接」现在就连，不会在你没点之前自己拨号。",
                    ))),
                    None,
                    cx,
                )
                .border_0(),
            )
            .child(
                button(
                    "terminal-wake-tab",
                    tr(cx, "连接"),
                    ButtonVariant::Primary,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.activate(index, cx));
                }),
            )
            .into_any_element()
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
