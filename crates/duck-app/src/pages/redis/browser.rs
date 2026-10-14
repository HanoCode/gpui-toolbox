//! 键浏览主区与结果行渲染：视图切换、状态栏、各类值的行构造。

use super::*;

/// 类型 → 图标（列表里按类型过滤时用）。
pub(super) fn type_icon(key_type: KeyType) -> DuckIcon {
    match key_type {
        KeyType::String => DuckIcon::FileText,
        KeyType::List => DuckIcon::ListFilter,
        KeyType::Set => DuckIcon::Folders,
        KeyType::ZSet => DuckIcon::Layers,
        KeyType::Hash => DuckIcon::Columns2,
        KeyType::Stream => DuckIcon::Film,
        KeyType::Module | KeyType::Missing => DuckIcon::File,
    }
}

/// 行内的小动作按钮（删除 / 修改共用）。
///
/// `label` 走 i18n 的 `tr`，同时兼作无障碍名——图标按钮**必须**有名字，
/// 否则读屏只能念出一个「按钮」（见 `AGENTS.md` §3.6 的 a11y 条文）。
pub(super) fn row_icon_button(
    id: String,
    label: &'static str,
    icon: DuckIcon,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let extended = theme.extended();
    icon_button(
        SharedString::from(id),
        label,
        Icon::new(icon)
            .size(extended.icon.sm)
            .text_color(theme.tokens().colors.muted_foreground),
        ButtonVariant::Ghost,
        cx,
    )
    .on_click(on_click)
    .into_any_element()
}

/// 行尾删除回调（哈希 / 有序集合用：入参是被删项的**主键**）。
pub type PairDelete = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// 两列 + 行尾删除的表格（哈希 / 有序集合共用）。
///
/// `on_delete` 的入参是**左列的值**——哈希的字段名、有序集合的成员名，
/// 两者都是该项在服务端的主键，按它删不会误删同值的其他项。
pub(super) fn pair_rows(
    id_prefix: &str,
    key: &str,
    left_head: &'static str,
    right_head: &'static str,
    pairs: &[(String, String)],
    on_delete: PairDelete,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let head = |text: &'static str| {
        div()
            .flex_1()
            .min_w_0()
            .text_size(tokens.typography.xs.size)
            .text_color(tokens.colors.muted_foreground)
            .child(SharedString::from(tr(cx, text)))
    };
    let mut table = v_flex().w_full().gap(tokens.spacing.xxs).child(
        h_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .child(head(left_head))
            .child(head(right_head)),
    );
    for (index, (left_value, right_value)) in pairs.iter().enumerate() {
        let cell = |value: &str| {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.sm.size)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(value.to_owned()))
        };
        let target = left_value.clone();
        table = table.child(
            h_flex()
                .id(SharedString::from(format!("{id_prefix}-{key}-{index}")))
                .w_full()
                .gap(tokens.spacing.xs)
                .child(cell(left_value))
                .child(cell(right_value))
                .child(row_icon_button(
                    format!("{id_prefix}-del-{index}"),
                    tr(cx, "删除此项"),
                    DuckIcon::Trash2,
                    {
                        let on_delete = on_delete.clone();
                        move |_, window, cx| on_delete(target.clone(), window, cx)
                    },
                    cx,
                )),
        );
    }
    table.into_any_element()
}

/// 哈希：字段 / 值 / 删除。
pub(super) fn hash_rows(
    key: &str,
    pairs: &[(String, String)],
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> AnyElement {
    let on_delete: PairDelete = {
        let view = view.clone();
        Rc::new(move |field, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.delete_hash_field(field.clone(), window, cx);
            });
        })
    };
    pair_rows("redis-hash", key, "字段", "值", pairs, on_delete, cx)
}

/// 有序集合：成员 / 分数 / 删除。
pub(super) fn zset_rows(
    key: &str,
    pairs: &[(String, String)],
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> AnyElement {
    let on_delete: PairDelete = {
        let view = view.clone();
        Rc::new(move |member, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.delete_zset_member(member.clone(), window, cx);
            });
        })
    };
    pair_rows("redis-zset", key, "成员", "分数", pairs, on_delete, cx)
}

/// 行内额外动作的构造器（列表放「改」，集合没有）。
pub type RowExtra = Rc<dyn Fn(usize, &str, &App) -> Option<AnyElement>>;
/// 行尾删除的回调：收到 `(行下标, 行内容)`。
pub type RowDelete = Rc<dyn Fn(usize, String, &mut Window, &mut App)>;

/// 单列 + 行尾动作的表格（列表 / 集合共用）。
///
/// `on_delete` 收到 `(行下标, 行内容)`：**集合按值删**（值即集合的成员标识），
/// **列表按位置删**（值可能重复，按值删会删错行）。
pub(super) fn single_rows(
    id_prefix: &str,
    key: &str,
    items: &[String],
    extra: &RowExtra,
    on_delete: &RowDelete,
    delete_label: &'static str,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let mut table = v_flex().w_full().gap(tokens.spacing.xxs);
    for (index, item) in items.iter().enumerate() {
        let target = item.clone();
        table = table.child(
            h_flex()
                .id(SharedString::from(format!("{id_prefix}-{key}-{index}")))
                .w_full()
                .gap(tokens.spacing.xs)
                .child(
                    div()
                        .flex_none()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.muted_foreground)
                        .child(SharedString::from(index.to_string())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.sm.size)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(item.clone())),
                )
                .children(extra(index, item.as_str(), cx))
                .child(row_icon_button(
                    format!("{id_prefix}-del-{index}"),
                    delete_label,
                    DuckIcon::Trash2,
                    {
                        let on_delete = on_delete.clone();
                        move |_, window, cx| on_delete(index, target.clone(), window, cx)
                    },
                    cx,
                )),
        );
    }
    table.into_any_element()
}

/// 集合：值 / 删除（按值删，值即成员标识）。
pub(super) fn set_rows(
    key: &str,
    items: &[String],
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> AnyElement {
    let on_delete: RowDelete = {
        let view = view.clone();
        Rc::new(move |_index, member, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.delete_set_member(member.clone(), window, cx);
            });
        })
    };
    let extra: RowExtra = Rc::new(|_, _, _| None);
    single_rows(
        "redis-set",
        key,
        items,
        &extra,
        &on_delete,
        tr(cx, "删除此成员"),
        cx,
    )
}

/// 列表：序号 / 值 / 改 / 删（按位置删，值可能重复）。
pub(super) fn list_rows(
    key: &str,
    items: &[String],
    editing: Option<i64>,
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> AnyElement {
    let extra: RowExtra = {
        let view = view.clone();
        Rc::new(move |index, value: &str, app: &App| {
            // 正在编辑的那一行不再给入口：再点一次只是把同一个值重填一遍。
            if editing == Some(index as i64) {
                return None;
            }
            let value = value.to_owned();
            let view = view.clone();
            Some(row_icon_button(
                format!("redis-list-edit-{index}"),
                tr(app, "修改此行"),
                DuckIcon::Code,
                move |_, window, cx| {
                    let value = value.clone();
                    let _ = view.update(cx, |this, cx| {
                        this.edit_list_item(index as i64, value.clone(), window, cx);
                    });
                },
                app,
            ))
        })
    };
    let on_delete: RowDelete = {
        let view = view.clone();
        Rc::new(move |index, _value, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.delete_list_item(index as i64, window, cx);
            });
        })
    };
    single_rows(
        "redis-list",
        key,
        items,
        &extra,
        &on_delete,
        tr(cx, "删除此元素"),
        cx,
    )
}

// ── 右栏的值编辑器与键管理（M1.2）───────────────────────────────────────

impl RedisPage {
    pub(super) fn render_browser(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        if self.active_session().is_none() {
            return empty_state(
                DuckIcon::DatabaseZap,
                tr(cx, "未连接"),
                Some(tr(cx, "从左栏选择一条连接，或在左栏底部新建一条。").into()),
                Some(
                    button(
                        "redis-empty-new-connection",
                        tr(cx, "新建连接"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.new_connection(window, cx);
                            });
                        }
                    })
                    .into_any_element(),
                ),
                cx,
            )
            .into_any_element();
        }

        // 主视图切换（M1.4）：数据浏览 ↔ 服务端观测。
        // 服务端视图整屏接管主区（自带页签行），不叠加数据浏览的工具条。
        if self.main_view == MainView::Server {
            return v_flex()
                .size_full()
                .min_w_0()
                .min_h_0()
                .child(self.render_view_switch(cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(self.render_server_view(cx)),
                )
                .into_any_element();
        }

        // 类型过滤条：全部 + 六个可被服务端过滤的类型。
        //
        // `cx` 在这里收成 `&App`：本函数上文已经借走了 `theme`（`active_theme` 返回
        // 全局引用），闭包再要 `&mut Context` 就会与它撞车。只读闭包一律用 `&App`。
        let type_chip = |key_type: Option<KeyType>, cx: &App| {
            let selected = self.type_filter == key_type;
            let label = match key_type {
                None => tr(cx, "全部"),
                Some(key_type) => key_type_label(key_type, cx),
            };
            let id_suffix = key_type
                .map(|key_type| key_type.server_name().unwrap_or("all").to_owned())
                .unwrap_or_else(|| "all".to_owned());
            button(
                SharedString::from(format!("redis-type-{id_suffix}")),
                label,
                if selected {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.type_filter = key_type;
                        this.refresh_keys(window, cx);
                    });
                }
            })
        };

        // 键列表形态切换：平铺（一行一键）↔ 命名空间树（按分隔符分组）。
        let layout_chip = |tree: bool, cx: &App| {
            button(
                if tree {
                    "redis-layout-tree"
                } else {
                    "redis-layout-flat"
                },
                if tree {
                    tr(cx, "树形")
                } else {
                    tr(cx, "平铺")
                },
                if self.tree_mode == tree {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        if this.tree_mode != tree {
                            this.toggle_tree_mode(cx);
                        }
                    });
                }
            })
        };

        let toolbar = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(theme.density().control)
                    .px(tokens.spacing.sm)
                    .rounded(theme.components().input_radius)
                    .border(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .bg(tokens.colors.background)
                    .child(
                        Input::new(&self.pattern)
                            .appearance(false)
                            .bordered(false)
                            .w_full(),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(tokens.spacing.xxs)
                    .child(layout_chip(false, cx))
                    .child(layout_chip(true, cx)),
            )
            .child(
                toolbar_action_button(
                    "redis-refresh",
                    tr(cx, "重新扫描"),
                    // `toolbar_action_button` 的方形内边距按 `icon.md` 算（见其文档与
                    // `toolbar_dropdown_button` 同款公式），传别的档位会让按钮不再是正方形、
                    // 图标也比相邻 chrome 按钮小一档 —— 这里是普通工具栏动作，走 md。
                    Icon::new(DuckIcon::RotateCw).size(extended.icon.md),
                    false,
                    self.busy,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.refresh_keys(window, cx));
                    }
                }),
            );

        let mut type_row = h_flex()
            .flex_none()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap(tokens.spacing.xxs)
            .px(tokens.spacing.sm)
            .pt(tokens.spacing.xs);
        type_row = type_row.child(type_chip(None, cx));
        for key_type in KeyType::SCANNABLE {
            type_row = type_row.child(type_chip(Some(key_type), cx));
        }

        // 键列表（虚拟滚动）。
        let keys = std::sync::Arc::new(self.keys.clone());
        let selected = self.selected;
        let filtered = self.type_filter;
        let row_count = keys.len();
        // `uniform_list` 的闭包要求 `'static`，拿不到借自 `cx` 的 `tokens` / `extended`
        // （同 `db.rs` 网格闭包的做法）：把用到的量提前拷出来。
        let row_gap = tokens.spacing.xs;
        let row_pad = tokens.spacing.sm;
        let row_text = tokens.typography.sm.size;
        let row_line = tokens.typography.sm.line_height;
        let row_fg = tokens.colors.foreground;
        let row_meta = tokens.colors.muted_foreground;
        let row_meta_size = tokens.typography.xs.size;
        let row_selected_bg = extended.colors.nav_selected;
        let row_hover_bg = extended.colors.hover_tint;
        let row_icon = extended.icon.sm;
        // 列表闭包是 `move`：`view` 会被它吃掉，末尾的「继续加载」还要用，先留一份。
        let list_view = view.clone();
        let list = if row_count == 0 {
            empty_state(
                DuckIcon::Search,
                if self.scan_done {
                    tr(cx, "没有键")
                } else {
                    tr(cx, "尚未加载")
                },
                Some(
                    if self.busy {
                        tr(cx, "正在扫描…")
                    } else {
                        tr(cx, "调整过滤串或点「重新扫描」。")
                    }
                    .into(),
                ),
                None,
                cx,
            )
            .into_any_element()
        } else if self.tree_mode {
            // 命名空间树：行来自缓存（`rebuild_tree`），渲染只读不现算。
            let rows = std::sync::Arc::new(self.tree_rows.clone());
            let tree_view = view.clone();
            uniform_list("redis-key-tree", rows.len(), move |range, _window, _cx| {
                range
                    .filter_map(|index| {
                        let row = rows.get(index)?;
                        // 折叠三角：**独立可点**。行本身负责「选中实体键」，
                        // 两者共用一次点击会让「既有键又有子节点」的行没法两者兼顾。
                        // `on_click` 只挂在有 `ElementId` 的元素上（本仓约定），
                        // 所以三角也必须先 `.id()`——裸 `div()` 挂不上。
                        let mut marker = div()
                            .id(SharedString::from(format!("redis-tree-toggle-{index}")))
                            .flex_none()
                            .w(row_icon)
                            .flex()
                            .items_center()
                            .justify_center();
                        if row.has_children {
                            let path = row.path.clone();
                            let expanded = row.expanded;
                            marker = marker
                                .cursor_pointer()
                                .on_click({
                                    let tree_view = tree_view.clone();
                                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                                        let path = path.clone();
                                        let _ = tree_view.update(cx, |this, cx| {
                                            this.toggle_tree_node(path.clone(), !expanded, cx);
                                        });
                                    }
                                })
                                .child(
                                    Icon::new(if row.expanded {
                                        DuckIcon::ChevronDown
                                    } else {
                                        DuckIcon::ChevronRight
                                    })
                                    .size(row_icon)
                                    .text_color(row_meta),
                                );
                        } else {
                            marker = marker.child(
                                Icon::new(DuckIcon::File)
                                    .size(row_icon)
                                    .text_color(row_meta),
                            );
                        }

                        // 跨进 `on_click` 闭包（要求 `'static`）的值必须先拷出来：
                        // `row` 借自 `rows`，生命周期只在本次 `filter_map` 内。
                        let leaf = row.leaf_key.clone();
                        let has_children = row.has_children;
                        let row_expanded = row.expanded;
                        let row_path = row.path.clone();
                        let label = row.label.clone();
                        let indent = row_gap * (row.depth as f32 + 1.0);
                        let count = row.key_count;

                        Some(
                            h_flex()
                                .id(SharedString::from(format!("redis-tree-{index}")))
                                .h(px(KEY_ROW_HEIGHT))
                                .w_full()
                                .flex_none()
                                .items_center()
                                .gap(row_gap)
                                // 缩进按层数乘基础的间距 token，不写裸 px(字号)。
                                .pl(indent)
                                .pr(row_pad)
                                .role(Role::ListItem)
                                .aria_label(SharedString::from(label.clone()))
                                .cursor_pointer()
                                .hover(move |style| style.bg(row_hover_bg))
                                .on_click({
                                    let tree_view = tree_view.clone();
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let _ = tree_view.update(cx, |this, cx| {
                                            match leaf.clone() {
                                                // 挂着实体键 ➜ 选中它。
                                                Some(key) => {
                                                    if let Some(index) = this
                                                        .keys
                                                        .iter()
                                                        .position(|existing| existing == &key)
                                                    {
                                                        this.select_key(index, window, cx);
                                                    }
                                                }
                                                // 纯中间节点 ➜ 折叠 / 展开。
                                                None if has_children => {
                                                    this.toggle_tree_node(
                                                        row_path.clone(),
                                                        !row_expanded,
                                                        cx,
                                                    );
                                                }
                                                None => {}
                                            }
                                        });
                                    }
                                })
                                .child(marker)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(row_text)
                                        .line_height(row_line)
                                        .text_color(row_fg)
                                        .child(SharedString::from(label)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(row_meta_size)
                                        .text_color(row_meta)
                                        .child(SharedString::from(count.to_string())),
                                ),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .flex_1()
            .min_h_0()
            .into_any_element()
        } else {
            uniform_list("redis-key-list", row_count, move |range, _window, _cx| {
                range
                    .filter_map(|index| {
                        let key = keys.get(index)?.clone();
                        Some(
                            h_flex()
                                .id(SharedString::from(format!("redis-key-{index}")))
                                .h(px(KEY_ROW_HEIGHT))
                                .w_full()
                                .flex_none()
                                .items_center()
                                .gap(row_gap)
                                .px(row_pad)
                                .role(Role::ListItem)
                                .aria_label(SharedString::from(key.clone()))
                                .cursor_pointer()
                                .when(selected == Some(index), |row| row.bg(row_selected_bg))
                                .hover(move |style| style.bg(row_hover_bg))
                                .on_click({
                                    let view = list_view.clone();
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.select_key(index, window, cx);
                                        });
                                    }
                                })
                                .child(
                                    Icon::new(match filtered {
                                        Some(key_type) => type_icon(key_type),
                                        None => DuckIcon::File,
                                    })
                                    .size(row_icon)
                                    .text_color(row_meta),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(row_text)
                                        .line_height(row_line)
                                        .text_color(row_fg)
                                        .child(SharedString::from(key)),
                                ),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        let footer = if !self.scan_done {
            h_flex()
                .flex_none()
                .w_full()
                .justify_center()
                .py(tokens.spacing.sm)
                .border_t(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(
                    button(
                        "redis-load-more",
                        tr(cx, "继续加载"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.load_more_keys(window, cx));
                        }
                    }),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(self.render_view_switch(cx))
            .child(toolbar)
            .child(type_row)
            .child(div().flex_1().min_h_0().child(list))
            .child(footer)
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 服务端观测（M1.4）：视图切换条、页签、七个面板。
    // ------------------------------------------------------------------

    /// 主视图切换条（数据 / 服务端）+ 右侧服务端版本。
    pub(super) fn render_view_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let selected = match self.main_view {
            MainView::Data => 0,
            MainView::Server => 1,
        };
        let version = self
            .active_session()
            .map(|session| SharedString::from(format!("Redis {}", session.version)));

        h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(segmented_tabs(
                "redis-main-view",
                [
                    SharedString::from(tr(cx, "数据")),
                    SharedString::from(tr(cx, "服务端")),
                ],
                selected,
                move |index, window, cx| {
                    let target = if index == 0 {
                        MainView::Data
                    } else {
                        MainView::Server
                    };
                    let _ = view.update(cx, |this, cx| this.set_main_view(target, window, cx));
                },
                cx,
            ))
            .child(div().flex_1())
            .children(version.map(|version| meta_text(version, cx)))
            .into_any_element()
    }

    pub(super) fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let mode = self.access_mode();
        let locked = self.gate().is_some_and(|gate| gate.is_locked());

        let mut bar = h_flex()
            .flex_none()
            .h(theme.density().status_bar)
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(self.status.clone()),
            );

        if let Some(session) = self.active_session() {
            bar = bar.child(neutral_tag(SharedString::from(session.version.clone()), cx));
        }
        bar = bar.child(meta_text(mode_label(mode, cx), cx));
        if self.active.is_some() {
            bar = bar.child(meta_text(
                SharedString::from(trf(
                    cx,
                    "{} / {} 键",
                    &[&self.keys.len().to_string(), &self.dbsize.to_string()],
                )),
                cx,
            ));
        }

        if locked {
            bar = bar.child(
                button(
                    "redis-unlock-writes",
                    trf(
                        cx,
                        "解锁写操作（{} 分钟）",
                        &[&(WRITE_UNLOCK_SECS / 60).to_string()],
                    ),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.unlock_writes(window, cx));
                    }
                }),
            );
        } else if self.active.is_some() {
            bar = bar.child(
                button(
                    "redis-disconnect",
                    tr(cx, "断开"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.disconnect_active(window, cx));
                    }
                }),
            );
        }

        bar.into_any_element()
    }
}
