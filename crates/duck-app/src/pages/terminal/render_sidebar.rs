//! 左栏连接列表的渲染。

use super::*;

impl TerminalPage {
    /// 左栏：搜索 + 分组连接列表。行规格与 [`sidebar_navigation_button`] 一致。
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        // 分组名在这里是**已就绪的显示名**（内置组在 `data::group_views` 里翻过），
        // 左栏不再需要 `lang` 自己翻译。
        let control = theme.density().control;
        let view = self.view.clone();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let searching = !query.is_empty();

        let new_connection = button(
            "terminal-new-connection",
            tr(cx, "新建连接"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click({
            let view = view.clone();
            move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.open_new_connection(window, cx));
            }
        });

        let header_new = icon_button(
            "terminal-new-connection-icon",
            tr(cx, "新建连接"),
            // 图标走 `icon.sm`：与数据库 / Redis 左栏 section header 同位置的方形图标按钮一致
            // （`icon_button` 框恒为 `density.control`，档位取 sm 是这三页的共同约定）。
            Icon::new(DuckIcon::Plus)
                .size(extended.icon.sm)
                .text_color(tokens.colors.muted_foreground),
            ButtonVariant::Ghost,
            cx,
        )
        // 与同行搜索框等高：曾为了「视觉轻一点」矮过一个 `xs`，实测加号小到难点中，
        // 而且它是左栏唯一的「新建」入口 —— 可点性优先于视觉轻重。
        .h(control)
        .on_click({
            let view = view.clone();
            move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.open_new_connection(window, cx));
            }
        });

        let mut nav = v_flex().w_full().gap(tokens.spacing.xxs);
        let mut matched = 0_usize;
        // 视图每次渲染现算：连接表是唯一真源（`profiles`），
        // 多存一份 `Vec<Group>` 就意味着「改了这份忘了那份」。
        let groups = self.group_views(cx);
        // 连接拖拽进行中（拖拽源登记 + 确有活跃拖拽双保险：松手哪怕落在
        // 投放区之外，下一帧空分组与高亮也会收回去）。
        let conn_dragging = self.dragging_conn.is_some() && cx.has_active_drag();

        for group in &groups {
            let items: Vec<&Connection> = group
                .items
                .iter()
                .filter(|item| {
                    !searching
                        || item.label.to_lowercase().contains(&query)
                        || item.endpoint().to_lowercase().contains(&query)
                })
                .collect();
            // 空分组平时不渲染；唯独拖连接时要在 —— 不然把某组最后一条
            // 连接拖走后这个组就消失了，那份连接永远拖不回去。拖拽期间以
            // 「虚线投放位」的弱化形态出现（见下），不会看着像凭空冒出的文件夹。
            if items.is_empty() && !conn_dragging {
                continue;
            }
            if !items.is_empty() {
                matched += items.len();
            }

            // 搜索时临时展开命中的分组（不然「搜到了却看不见」）；平时只看用户点开的。
            // 空分组没有内容可展开，永远按折叠画。
            let expanded = !items.is_empty() && (searching || self.expanded.contains(&group.id));
            let chevron = if expanded {
                DuckIcon::ChevronDown
            } else {
                DuckIcon::ChevronRight
            };
            let group_id = group.id.clone();
            let conn_hovered =
                conn_dragging && self.conn_drop_hover.as_deref() == Some(group_id.as_str());

            // 分组整块是投放区：悬停亮主题专用语义色（`drag_border` 描边、
            // `drop_target` 悬停底色），与画布分屏投放区同一套拖放视觉。
            // 常态描边透明（宽度不变），高亮出现时不抖布局。
            let mut section = v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .id(SharedString::from(format!(
                    "terminal-group-drop-{group_id}"
                )))
                .rounded(tokens.radius.md)
                .border_1()
                .border_color(if conn_hovered {
                    extended.colors.drag_border
                } else {
                    gpui::transparent_black()
                })
                .when(conn_hovered, |section| {
                    section.bg(extended.colors.drop_target)
                })
                .on_drag_move::<ConnDrag>({
                    let view = view.clone();
                    let group_id = group_id.clone();
                    move |event: &DragMoveEvent<ConnDrag>, _window, cx| {
                        // ⚠️ gpui 的 `on_drag_move` 在 Capture 阶段**全局**分发
                        //（div.rs 只对载荷类型、不做命中检测），每个分组都会在
                        // 每次指针移动时收到回调，注册顺序靠后的会覆盖靠前的
                        // ——不做这一道几何自检，高亮与落组会永远钉在最后一组。
                        // 与分屏投放区（`with_split_drop_target`）同一套自救。
                        if !event.bounds.contains(&event.event.position) {
                            return;
                        }
                        let _ = view.update(cx, |this, cx| {
                            // `group_id` 是 `String`（自定义分组没有静态 id），
                            // 而处理器是 `Fn`（每次指针移动都重跑）——只能克隆比较。
                            if this.conn_drop_hover.as_deref() != Some(group_id.as_str()) {
                                this.conn_drop_hover = Some(group_id.clone());
                                cx.notify();
                            }
                        });
                    }
                })
                .on_drop::<ConnDrag>({
                    let view = view.clone();
                    move |drag: &ConnDrag, _window, cx| {
                        let _ = view.update(cx, |this, cx| this.finish_conn_drop(drag, cx));
                    }
                });

            // 空分组（只会在拖拽期间出现）画成虚线投放位：弱化的图标 + 名字，
            // 一眼能看出「这是拖拽用的临时槽位」，而不是凭空多出来的文件夹。
            // 投放逻辑（悬停高亮 / `on_drop`）由外层 `section` 统一提供，
            // 占位块自己不用接任何事件。
            if items.is_empty() {
                section = section.child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .px(tokens.spacing.md)
                        .py(tokens.spacing.xs)
                        .rounded(tokens.radius.md)
                        .border_1()
                        .border_dashed()
                        .border_color(if conn_hovered {
                            extended.colors.drag_border
                        } else {
                            extended.colors.hairline
                        })
                        .text_size(tokens.typography.sm.size)
                        .text_color(extended.colors.text_tertiary)
                        .child(
                            Icon::new(group.icon)
                                .size(extended.icon.md)
                                .text_color(extended.colors.text_tertiary),
                        )
                        .child(group.label.clone()),
                );
            } else {
                section = section.child(
                    sidebar_navigation_button(
                        SharedString::from(format!("terminal-group-{group_id}")),
                        SharedString::from(group.label.clone()),
                        Icon::new(group.icon).size(extended.icon.lg),
                        Icon::new(chevron)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                        false,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ =
                                view.update(cx, |this, cx| this.toggle_group(group_id.clone(), cx));
                        }
                    }),
                );

                if expanded {
                    for item in items {
                        let id = item.id.clone();
                        // 选中态跟随**当前激活标签**：激活的是这条连接（或它的 SFTP 标签）才亮。
                        // 之前是「开过就算选中」，三个标签一开就满栏高亮，跟当前在哪没关系。
                        let selected =
                            self.active_tab()
                                .map(|tab| tab.id.as_str())
                                .is_some_and(|active| {
                                    active == id
                                        || active.strip_suffix("/sftp") == Some(id.as_str())
                                });
                        let mut row = sidebar_navigation_button(
                            SharedString::from(format!("terminal-conn-{id}")),
                            SharedString::from(item.label.clone()),
                            Icon::new(DuckIcon::Server)
                                .size(extended.icon.md)
                                .text_color(item.status.color(cx)),
                            div()
                                .flex_none()
                                .text_size(tokens.typography.xs.size)
                                .line_height(tokens.typography.xs.line_height)
                                .text_color(extended.colors.text_tertiary)
                                .child(item.endpoint()),
                            selected,
                            cx,
                        )
                        .pl(tokens.spacing.md)
                        .on_click({
                            let id = id.clone();
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.open_connection(&id, window, cx);
                                });
                            }
                        });
                        // 拖拽源：只有用户档案能拖（本地 shell 是内置入口，不属于任何
                        // 分组），搜索过滤视图里也不拖 —— 列表是筛出来的，落组语义混乱。
                        // 必须挂在 context_menu 之前：`.context_menu()` 返回
                        // `ContextMenu<Button>`，上面没有拖拽方法。
                        if !searching && !item.builtin {
                            row = row.on_drag(
                                ConnDrag {
                                    profile_id: id.clone(),
                                },
                                {
                                    let view = view.clone();
                                    let label = item.label.clone();
                                    move |drag: &ConnDrag, _point, _window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            if this.dragging_conn.as_deref()
                                                != Some(drag.profile_id.as_str())
                                            {
                                                this.dragging_conn = Some(drag.profile_id.clone());
                                                this.conn_drop_hover = None;
                                                // 互清另一类拖拽的残留：上一轮若在空白处松手
                                                //（没有 drop 目标接住），旧值会留到下一次拖拽，
                                                // 让「空分组显示 / 分屏投放区」错档亮起。
                                                this.dragging_tab = None;
                                                this.split_drop_hover = false;
                                                cx.notify();
                                            }
                                        });
                                        cx.new(|_| TabDragGhost {
                                            icon: DuckIcon::Server,
                                            label: label.clone(),
                                        })
                                    }
                                },
                            );
                        }
                        let row = row.context_menu({
                            // 菜单闭包是 `Fn`（每次打开都重跑），id 每项一份、
                            // 每个 on_click 再各自克隆。
                            let id = id.clone();
                            let builtin = item.builtin;
                            let view = view.clone();
                            move |menu, _, cx| {
                                let mut menu = menu
                                    .item(PopupMenuItem::new(tr(cx, "打开")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_connection(&id, window, cx);
                                            });
                                        }
                                    }))
                                    // 把当前标签分屏到这条连接（本地 shell 与远端主机都行）：
                                    // 分屏右窗格连谁由这里显式指定，不再是「标签自己的连接」。
                                    .item(PopupMenuItem::new(tr(cx, "在分屏中打开")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, _, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_split_with(&id, cx)
                                            });
                                        }
                                    }));
                                if builtin {
                                    return menu;
                                }
                                menu = menu
                                    .item(PopupMenuItem::new(tr(cx, "打开 SFTP 面板")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_sftp_tab(&id, window, cx);
                                            });
                                        }
                                    }))
                                    .item(PopupMenuItem::new(tr(cx, "编辑连接")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_edit_connection(&id, window, cx);
                                            });
                                        }
                                    }))
                                    // 克隆紧挨「编辑」：都是「拿这条连接再派生一份」的动作，
                                    // 区别只是克隆不问参数、直接复制一份可改的副本。
                                    .item(PopupMenuItem::new(tr(cx, "克隆连接")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.clone_connection(&id, window, cx);
                                            });
                                        }
                                    }));
                                menu.item(PopupMenuItem::new(tr(cx, "删除连接")).on_click({
                                    let id = id.clone();
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.delete_connection(&id, window, cx);
                                        });
                                    }
                                }))
                            }
                        });
                        section = section.child(row);
                    }
                }
            }

            nav = nav.child(section);
        }

        if searching && matched == 0 {
            nav = nav.child(
                div()
                    .w_full()
                    .py(tokens.spacing.lg)
                    .flex()
                    .justify_center()
                    .child(meta_text(tr(cx, "没有匹配的连接。"), cx)),
            );
        }

        let total: usize = groups.iter().map(|group| group.items.len()).sum();
        let connection_count = meta_text(
            SharedString::from(trf(cx, "{} 个连接", &[&total.to_string()])),
            cx,
        );

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
                    .child(caption_heading(tr(cx, "连接"), cx))
                    .child(header_new),
            )
            .child(
                div()
                    .flex_none()
                    .px(tokens.spacing.sm)
                    .pb(tokens.spacing.xs)
                    .child(
                        h_flex()
                            .h(control)
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .px(tokens.spacing.sm)
                            .rounded(tokens.radius.md)
                            .border_1()
                            .border_color(tokens.colors.border)
                            .bg(tokens.colors.background)
                            .child(
                                Icon::new(DuckIcon::Search)
                                    .size(extended.icon.md)
                                    .text_color(tokens.colors.muted_foreground),
                            )
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.search)
                                        .appearance(false)
                                        .with_size(Size::Small)
                                        .text_size(tokens.typography.sm.size)
                                        .line_height(tokens.typography.sm.line_height)
                                        .w_full(),
                                ),
                            ),
                    ),
            )
            .child(
                div()
                    .id("terminal-connection-nav")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(nav),
            )
            .child(
                v_flex()
                    .flex_none()
                    .gap(tokens.spacing.xs)
                    .px(tokens.spacing.sm)
                    .pb(tokens.spacing.sm)
                    .child(new_connection)
                    .child(h_flex().w_full().justify_center().child(connection_count)),
            )
            .into_any_element()
    }
}
