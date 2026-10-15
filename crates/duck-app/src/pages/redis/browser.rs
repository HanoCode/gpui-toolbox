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

// ── 顶栏档位的文案与图标 ────────────────────────────────────────────────
//
// 三个下拉（查询模式 / 过期时间 / 排序）都是「枚举 → 文案」的映射，没有第二份
// 清单可以走偏：档位表一律取自内核的 `ALL`（`QueryMode::ALL` / `TtlFilter::ALL` /
// `KeySort::ALL`），这里只负责起名字。
//
// ⚠️ 文案自带「这一档在管什么」的前缀（如「过期时间：全部」）：下拉按钮的
// 可见文字同时也是它的无障碍名与悬停提示（`toolbar_text_dropdown_button` 只有
// 一个文案参数），只写「全部」会让人分不清它管的是类型还是过期时间。

/// 查询模式的中文名。
fn query_mode_label(mode: QueryMode, cx: &App) -> &'static str {
    match mode {
        QueryMode::All => tr(cx, "子串匹配"),
        QueryMode::Prefix => tr(cx, "前缀匹配"),
        QueryMode::Exact => tr(cx, "精确匹配"),
    }
}

/// 查询模式的图标（`*` 子串 / `^` 前缀 / `=` 精确，与 zedis 的符号同义）。
fn query_mode_icon(mode: QueryMode) -> DuckIcon {
    match mode {
        QueryMode::All => DuckIcon::Search,
        QueryMode::Prefix => DuckIcon::CornerLeftUp,
        QueryMode::Exact => DuckIcon::Check,
    }
}

/// 过期时间过滤档的中文名。
fn ttl_filter_label(filter: TtlFilter, cx: &App) -> &'static str {
    match filter {
        TtlFilter::All => tr(cx, "过期时间：全部"),
        TtlFilter::NoTtl => tr(cx, "过期时间：永不过期"),
        TtlFilter::Expiring => tr(cx, "过期时间：即将过期"),
        TtlFilter::Lt1h => tr(cx, "过期时间：1 小时内"),
        TtlFilter::Lt1d => tr(cx, "过期时间：1 天内"),
        TtlFilter::Lt7d => tr(cx, "过期时间：7 天内"),
        TtlFilter::Gte7d => tr(cx, "过期时间：7 天以上"),
    }
}

/// 排序档的中文名。
fn key_sort_label(sort: KeySort, cx: &App) -> &'static str {
    match sort {
        KeySort::NameAsc => tr(cx, "排序：名称升序"),
        KeySort::NameDesc => tr(cx, "排序：名称降序"),
        KeySort::TtlAsc => tr(cx, "排序：TTL 升序"),
        KeySort::TtlDesc => tr(cx, "排序：TTL 降序"),
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
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.type_filter = key_type;
                        this.refresh_keys(cx);
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

        // 顶栏三个档位。查询模式改的是**服务端 `MATCH` 串**（换档即重扫）；
        // 过期时间与排序是**纯本地**的（只读已缓存的 `key_meta`，不发任何往返）。
        let query_options: Vec<(QueryMode, SharedString)> = QueryMode::ALL
            .iter()
            .map(|mode| (*mode, SharedString::from(query_mode_label(*mode, cx))))
            .collect();
        let query_switcher = toolbar_switcher(
            "redis-query-mode",
            // 工具条最左端：菜单向右展开。
            Anchor::BottomLeft,
            &query_options,
            self.query_mode,
            Icon::new(query_mode_icon(self.query_mode)).size(extended.icon.sm),
            {
                let view = view.clone();
                move |mode, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.set_query_mode(mode, cx));
                }
            },
            cx,
        );
        let ttl_options: Vec<(TtlFilter, SharedString)> = TtlFilter::ALL
            .iter()
            .map(|filter| (*filter, SharedString::from(ttl_filter_label(*filter, cx))))
            .collect();
        let ttl_switcher = toolbar_switcher(
            "redis-ttl-filter",
            // 类型行右端：菜单向左展开，否则会顶出窗口右缘。
            Anchor::BottomRight,
            &ttl_options,
            self.ttl_filter,
            Icon::new(DuckIcon::Clock).size(extended.icon.sm),
            {
                let view = view.clone();
                move |filter, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.set_ttl_filter(filter, cx));
                }
            },
            cx,
        );
        let sort_options: Vec<(KeySort, SharedString)> = KeySort::ALL
            .iter()
            .map(|sort| (*sort, SharedString::from(key_sort_label(*sort, cx))))
            .collect();
        let sort_switcher = toolbar_switcher(
            "redis-sort",
            // 工具条最右端：菜单向左展开。
            Anchor::BottomRight,
            &sort_options,
            self.sort,
            Icon::new(DuckIcon::ListOrdered).size(extended.icon.sm),
            {
                let view = view.clone();
                move |sort, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.set_sort(sort, cx));
                }
            },
            cx,
        );

        // 标签档过滤：与过期时间 / 排序同一层（**纯本地**，零往返）。
        // 放在类型过滤旁边（对齐 zedis：标签过滤与类型 / TTL 排在同一行），
        // 因为它回答的是同一类问题——「这一列里我只想看哪一部分」。
        let tag_options: Vec<(TagFilter, SharedString)> = TagFilter::ALL
            .iter()
            .map(|filter| (*filter, SharedString::from(tag_filter_text(*filter, cx))))
            .collect();
        let tag_switcher = toolbar_switcher(
            "redis-tag-filter",
            // 类型行右段：菜单向左展开。
            Anchor::BottomRight,
            &tag_options,
            self.tag_filter,
            Icon::new(DuckIcon::Tag).size(extended.icon.sm),
            {
                let view = view.clone();
                move |filter, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.set_tag_filter(filter, cx));
                }
            },
            cx,
        );

        let toolbar = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(query_switcher)
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
                // 回收站入口：和「重新扫描」并列（都是对着整个键空间的动作）。
                // 有条目时把条数缀进标题——否则用户无从知道里面攒没攒东西。
                toolbar_action_button(
                    "redis-trash",
                    if self.active_trash_len() == 0 {
                        SharedString::from(tr(cx, "回收站"))
                    } else {
                        SharedString::from(trf(
                            cx,
                            "回收站（{}）",
                            &[&self.active_trash_len().to_string()],
                        ))
                    },
                    Icon::new(DuckIcon::Trash2).size(extended.icon.md),
                    false,
                    self.busy || self.active.is_none(),
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.open_trash_dialog(window, cx));
                    }
                }),
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
                    move |_, _window, cx| {
                        let _ = view.update(cx, |this, cx| this.refresh_keys(cx));
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
        // 本地两档靠右：与左边的**类型**过滤在语义上不同（类型会让服务端重扫，
        // 这两个只在已加载的键上筛），拉开一点距离免得看起来是同一组。
        type_row = type_row
            .child(div().flex_1().min_w_0())
            .child(tag_switcher)
            .child(ttl_switcher)
            .child(sort_switcher);

        // 键列表（虚拟滚动）。
        //
        // 平铺形态的行序不再是 `keys` 的原序：`flat_rows` 存的是 `keys` 的**下标**
        // （已按 TTL 档过滤 + 按排序档排）。选中态与点击落点仍然用原始下标——
        // 换排序不该让「谁被选中」跟着变。
        let keys = std::sync::Arc::new(self.keys.clone());
        let flat = std::sync::Arc::new(self.flat_rows.clone());
        let selected = self.selected;
        let filtered = self.type_filter;
        let row_count = flat.len();
        // 当前形态下有没有可显示的行（两种形态的行数来源不同）。
        let visible_empty = if self.tree_mode {
            self.tree_rows.is_empty()
        } else {
            row_count == 0
        };
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
        let list = if visible_empty {
            // 三种「空」要分开说：扫完真的没有键 / 还在扫 / 有键但被过滤挡光了。
            // 混成一句「没有键」会让「过滤太严」看起来像「库是空的」。
            let (title, hint) = if !self.keys.is_empty() {
                (
                    tr(cx, "没有符合筛选的键"),
                    tr(cx, "把「过期时间」过滤调回「全部」。"),
                )
            } else if self.scan_done {
                (
                    tr(cx, "没有键"),
                    tr(cx, "换个查询模式或过滤串，或点「重新扫描」。"),
                )
            } else if self.busy {
                (tr(cx, "尚未加载"), tr(cx, "正在扫描…"))
            } else {
                (
                    tr(cx, "尚未加载"),
                    tr(cx, "换个查询模式或过滤串，或点「重新扫描」。"),
                )
            };
            empty_state(DuckIcon::Search, title, Some(hint.into()), None, cx).into_any_element()
        } else if self.tree_mode {
            // 命名空间树：行来自缓存（`rebuild_views`），渲染只读不现算。
            //
            // 行形态对齐 zedis（`temp/zedis/src/views/key_tree/delegate.rs`）：
            // `[缩进][三角][文件夹图标][类型徽标固定宽列][键名 …][右列]`，
            // **文件夹行右列 = 键数，键行右列 = TTL 芯片**；
            // 键行不画文件夹图标、也不画「1」这种无信息量的计数。
            let rows = std::sync::Arc::new(self.tree_rows.clone());
            let tree_view = view.clone();
            let dark = matches!(theme.mode(), duck_theme::ThemeMode::Dark);
            let folder_icon_color = row_fg.opacity(0.7);
            let folder_open_icon_color = row_fg.opacity(0.9);
            let folder_label_color = row_fg.opacity(0.85);
            // 收藏星的颜色：金色。不用主题的强调色——强调色是「这里是焦点」的语义，
            // 而金色在这个界面里只表示收藏，两套语义不打架。
            let star_color = duck_theme::color_from_rgb(if dark { 0xd8b45c } else { 0xa8801a });
            let badge_size = extended.badge.size;
            let badge_line = extended.badge.line_height;
            let badge_col = px(TREE_TYPE_COL_WIDTH);
            let ttl_col = px(TREE_TTL_COL_WIDTH);
            // 行闭包要 `'static`：借自 `cx` 的 `extended` 不能带进去，
            // 这里先把它压成三个 `Copy` 的色值（见 `TtlChipPalette`）。
            let ttl_palette = TtlChipPalette::of(extended);
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
                        }

                        // 跨进 `on_click` 闭包（要求 `'static`）的值必须先拷出来：
                        // `row` 借自 `rows`，生命周期只在本次 `filter_map` 内。
                        let leaf = row.leaf_key.clone();
                        let has_children = row.has_children;
                        let row_expanded = row.expanded;
                        let row_path = row.path.clone();
                        let label = row.label.clone();
                        // 缩进按层数乘基础步长 —— zedis 是每层 16px；
                        // 这里用 spacing token 的步长，跟随界面缩放。
                        let indent = row_gap * (row.depth as f32 * 2. + 1.);
                        let count = row.key_count;
                        let is_folder = row.leaf_key.is_none();
                        // 本地标注（标签色 / 收藏 / 备注）：都是 `Copy`，拷出来备用。
                        let row_tag = row.tag;
                        let row_tag_mixed = row.tag_mixed;
                        let row_favorite = row.favorite;
                        let row_has_note = row.has_note;
                        // 左缘色条：有标签用标签色，没有就用**全透明**（照样占宽度，
                        // 否则同一列键名会随「这个键有没有标签」左右跳一像素）。
                        let bar_color = row_tag
                            .map(|color| tag_color(color, dark))
                            .unwrap_or_else(|| row_fg.opacity(0.));

                        // 左列：文件夹行是文件夹图标（开 / 合两态），
                        // 键行留出与徽标列同宽的空白槽（对齐用，不画文件图标）。
                        let lead = if is_folder {
                            Icon::new(if row.expanded {
                                DuckIcon::FolderOpen
                            } else {
                                DuckIcon::Folders
                            })
                            .size(row_icon)
                            .text_color(if row.expanded {
                                folder_open_icon_color
                            } else {
                                folder_icon_color
                            })
                            .into_any_element()
                        } else {
                            div().flex_none().w(row_icon).into_any_element()
                        };

                        // 类型徽标：固定宽列 + 去饱和类型色（zedis 的 plain 形态，
                        // 不是填充胶囊——键树里重复上千次，填充会变成噪音色带）。
                        let badge = match row.key_type {
                            Some(key_type) => div()
                                .flex_none()
                                .w(badge_col)
                                .child(
                                    div()
                                        .text_size(badge_size)
                                        .line_height(badge_line)
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(key_type_color(key_type, dark))
                                        .child(SharedString::from(key_type.badge_code())),
                                )
                                .into_any_element(),
                            None => div().flex_none().w(badge_col).into_any_element(),
                        };

                        // 右列：**文件夹 → 键数；键 → TTL 芯片**。
                        //
                        // 判据必须是 `is_folder` 而**不是**「有没有 TTL」：键行在元数据
                        // 尚未取回时 `ttl_secs` 也是 `None`，若按 TTL 有无分支，那一瞬间
                        // 会从文件夹分支漏出计数 `1`（叶子只有一个键），闪一下再变芯片。
                        //
                        // `-1`（永久）与 `-2`（SCAN 与 TTL 之间键已消失）**都不画芯片**：
                        // 永久键若也画「∞」，多数库的每一行都会挂上同一个符号，
                        // 反而把少数真会过期的键埋掉（zedis 的原话，见 delegate.rs）。
                        // 不画时仍占同宽的空白槽，避免整列左右抖动。
                        let base_right: gpui::AnyElement = if is_folder {
                            div()
                                .flex_none()
                                .text_size(row_meta_size)
                                .line_height(row_line)
                                .text_color(row_meta)
                                .child(SharedString::from(count.to_string()))
                                .into_any_element()
                        } else {
                            // 只有「真的会过期」才画芯片：`ttl_chip_kind` 把
                            // 永久（`-1`）与未知（`-2`）都收成 `None`，
                            // 元数据未到时 `ttl_secs` 本身也是 `None` —— 三者共用空白槽。
                            let chip = row
                                .ttl_secs
                                .filter(|secs| ttl_chip_kind(*secs).is_some())
                                .and_then(format_ttl_chip);
                            match chip {
                                Some(text) => div()
                                    .flex_none()
                                    .w(ttl_col)
                                    .text_size(badge_size)
                                    .line_height(badge_line)
                                    .text_right()
                                    .text_color(ttl_palette.pick(row.ttl_secs.unwrap_or(0)))
                                    .child(SharedString::from(text))
                                    .into_any_element(),
                                None => div().flex_none().w(ttl_col).into_any_element(),
                            }
                        };

                        // 右列的**标注徽记**：收藏（实心星）与备注（铅笔），
                        // 都只在「真的有」的时候画——没有的行右列与从前逐字一致，
                        // 这是「不给每一行都挂一个空槽」的取舍（右列本来就窄）。
                        //
                        // 不进左缘色条那一列：色条只有一个通道（一个标签色），
                        // 而收藏与备注是**两个互相独立的布尔**，叠上去会变成
                        // 「红星 = 收藏的红键还是红键+收藏？」说不清。
                        let mut marks: Vec<gpui::AnyElement> = Vec::new();
                        if row_favorite {
                            marks.push(
                                Icon::new(DuckIcon::Star)
                                    .size(row_icon)
                                    .text_color(star_color)
                                    .into_any_element(),
                            );
                        }
                        if row_has_note {
                            marks.push(
                                Icon::new(DuckIcon::Pencil)
                                    .size(row_icon)
                                    .text_color(row_meta)
                                    .into_any_element(),
                            );
                        }
                        let right: gpui::AnyElement = if is_folder {
                            base_right
                        } else {
                            h_flex()
                                .flex_none()
                                .items_center()
                                .gap(row_gap)
                                .children(marks)
                                .child(base_right)
                                .into_any_element()
                        };

                        Some(
                            h_flex()
                                .id(SharedString::from(format!("redis-tree-{index}")))
                                .h(px(KEY_ROW_HEIGHT))
                                .w_full()
                                .flex_none()
                                .items_center()
                                .gap(row_gap)
                                // 左缘标签色条（对齐 zedis：标签是行左边那条竖色，
                                // 不是键名旁边的圆点）。未标色的行**也占这个宽度**，
                                // 用全透明——否则同一列键名会随有没有标签左右抖。
                                .border_l(px(TREE_TAG_BAR_WIDTH))
                                .border_color(bar_color)
                                // 混色（文件夹里不止一种标签色）画成虚线：
                                // 实线说「这个文件夹就是红的」，虚线说「以红为主，还有别的」。
                                .when(row_tag_mixed, |row| row.border_dashed())
                                // 缩进按层数乘基础步长，不写裸 px(字号)。
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
                                .child(lead)
                                .child(badge)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(row_text)
                                        .line_height(row_line)
                                        .text_color(if is_folder && !row_expanded {
                                            folder_label_color
                                        } else {
                                            row_fg
                                        })
                                        .child(SharedString::from(label)),
                                )
                                .child(right),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .flex_1()
            .min_h_0()
            .into_any_element()
        } else {
            // 平铺行没有像 `tree_rows` 那样的缓存行结构（行就是 `keys` 的一个下标），
            // 所以标注得在渲染期查。为此把当前连接的标注**快照成 `Arc`** 带进闭包：
            // 一次 `clone` 的开销只跟「用户标过多少条」有关（几十到几百），
            // 与键数无关——不是把整本标注表按键数复制一遍。
            let flat_tags = std::sync::Arc::new(self.active_tags().clone());
            let flat_dark = matches!(theme.mode(), duck_theme::ThemeMode::Dark);
            let flat_star = duck_theme::color_from_rgb(if flat_dark { 0xd8b45c } else { 0xa8801a });
            uniform_list("redis-key-list", row_count, move |range, _window, _cx| {
                range
                    .filter_map(|index| {
                        // 可见行 → `keys` 下标：行序由 `flat` 决定，
                        // 选中态与点击落点用原始下标（`selected` 的坐标系不变）。
                        let source = *flat.get(index)?;
                        let key = keys.get(source)?.clone();
                        let tag = flat_tags.tag_of(&key);
                        let favorite = flat_tags.is_favorite(&key);
                        let has_note = !flat_tags.note_of(&key).is_empty();
                        let bar_color = tag
                            .map(|color| tag_color(color, flat_dark))
                            .unwrap_or_else(|| row_fg.opacity(0.));
                        let mut marks: Vec<gpui::AnyElement> = Vec::new();
                        if favorite {
                            marks.push(
                                Icon::new(DuckIcon::Star)
                                    .size(row_icon)
                                    .text_color(flat_star)
                                    .into_any_element(),
                            );
                        }
                        if has_note {
                            marks.push(
                                Icon::new(DuckIcon::Pencil)
                                    .size(row_icon)
                                    .text_color(row_meta)
                                    .into_any_element(),
                            );
                        }
                        Some(
                            h_flex()
                                .id(SharedString::from(format!("redis-key-{index}")))
                                .h(px(KEY_ROW_HEIGHT))
                                .w_full()
                                .flex_none()
                                .items_center()
                                .gap(row_gap)
                                .px(row_pad)
                                // 与树模式同一套：左缘标签色条（未标色时全透明占位）。
                                // 平铺的键恒是叶子，所以没有「混色」这一档。
                                .border_l(px(TREE_TAG_BAR_WIDTH))
                                .border_color(bar_color)
                                .role(Role::ListItem)
                                .aria_label(SharedString::from(key.clone()))
                                .cursor_pointer()
                                .when(selected == Some(source), |row| row.bg(row_selected_bg))
                                .hover(move |style| style.bg(row_hover_bg))
                                .on_click({
                                    let view = list_view.clone();
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.select_key(source, window, cx);
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
                                )
                                // 标注徽记排在键名之后（右对齐）：键名占 `flex_1`，
                                // 所以这两个图标自然贴到行尾，不会把键名挤歪。
                                .children(marks),
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

        // ── 主区两栏：左边键列表（+「继续加载」），右边值编辑器 ──────────────
        //
        // 对齐 zedis：值不是「另一个面板里的只读展示」，而是和键空间并排的
        // **编辑区**。两栏之间可拖（`h_resizable`），宽度由 `value_pane`
        // 这个实体自己记着，页面不另存一份。
        //
        // ⚠️ `uniform_list` 必须**直接挂在 flex 容器（`v_flex`）下**：包一层普通
        // `div()`（默认 display:block）会让它在 gpui-pre 0.3.8 下量不出高度、
        // **整表不渲染**（键数照常计数、无任何报错，界面一片空白）。
        // 无头回归：`tests/redis_key_list_render.rs`。所以左栏这里是一个
        // `v_flex`，`list` 是它的直接子节点，而不是塞进别的壳里。
        let left = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(list)
            .child(footer);

        let split = h_resizable("redis-main-split")
            .with_state(&self.value_pane)
            .child(resizable_panel().child(left))
            .child(
                resizable_panel()
                    // ⚠️ `.size()` 只是**创建时的初始宽度**；拖动之后的宽度由
                    // `ResizableState` 记着（这就是它作为实体留在页面上的原因）。
                    .size(px(VALUE_PANE_DEFAULT))
                    .size_range(px(*VALUE_PANE_RANGE.start())..px(*VALUE_PANE_RANGE.end()))
                    // 定宽栏：不参与余量分配，拖动改的是它、余量给左边。
                    .flex_none()
                    .child(self.render_value_editor(cx)),
            );

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(self.render_view_switch(cx))
            .child(toolbar)
            .child(type_row)
            // ⚠️ 分栏组**没有** `flex_1`（它只实现 `RenderOnce` + `size`，而
            // `size` 按文档是「cross axis」，横向组上就是高度、不是弹性分配），
            // 所以套一层 `flex_1` 的壳把余量给它。组自己已经是 `size_full`，
            // 壳有确定高度后它就能拿满。
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .id("redis-main-split-wrap")
                    .child(split),
            )
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
}
