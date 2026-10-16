//! 构建器的右栏面板（**渲染层**：只读状态 + 派发动作，不拼 SQL）。
//!
//! 版式按「右栏是窄栏」这条事实排：一条行数据摊成**两小行**（列 / 取值），不是把
//! 四五个控件挤成一排 —— 右栏宽度通常在 300~400pt，横向塞五个控件的结果是每个都
//! 窄到读不出自己填了什么。
//!
//! 分区顺序照 DBFlux：语句模式 → 目标表 → （UPDATE）赋值 → （SELECT）列 → 条件 →
//! （SELECT）排序与限制 → 预览 → 动作。**预览必须在动作之上**：这一屏的动作是「执行」，
//! 而执行的是预览里那条语句，先看见再点，顺序本身就是一道确认。
//!
//! 几个重复出现的小件（输入框槽 / 分区标题 / 行外壳 / × / 插入列 / 添加）是**自由函数**
//! 而不是闭包：返回闭包的闭包在 `move` 语义下会把捕获的 `view` / `tokens` 搬走，
//! 第二个闭包拿到的是空壳；自由函数每次调用各自持有参数，没有这层纠缠。

use super::*;

use gpui::Div;
use gpui_component::{
    button::{Button, ButtonVariants as _},
    menu::DropdownMenu as _,
};

/// 面板里每个控件都要的唯一标识源（同一屏里同名 id 会串）。
const ROW_ID_PREFIX: &str = "db-builder";

impl DatabasePage {
    /// 右栏「构建器」分段。
    ///
    /// `pub(crate)`：调用点在 `render_sidebar.rs`（`db` 的子模块），与本文件不同模块。
    pub(crate) fn render_builder_dock(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        // 没有 SQL 这一层的引擎：整段给空态，而不是给一堆点了报错的控件。
        let Some(dialect) = self.builder_dialect() else {
            return empty_state(
                DuckIcon::SlidersHorizontal,
                tr(cx, "这套引擎没有 SQL 构建器"),
                Some(
                    tr(
                        cx,
                        "构建器生成的是 SQL 语句；MongoDB 与 Elasticsearch 走各自的查询语法。",
                    )
                    .into(),
                ),
                None,
                cx,
            )
            .into_any_element();
        };

        // ── 语句模式 + 重置 ─────────────────────────────────────────────
        let mode_select = dropdown_select(
            builder_id("mode"),
            BuilderMode::ALL
                .into_iter()
                .map(|mode| {
                    (
                        SharedString::from(mode.slug()),
                        SharedString::from(mode.label(cx)),
                    )
                })
                .collect::<Vec<_>>(),
            SharedString::from(self.builder.mode.slug()),
            {
                let view = view.clone();
                move |value, _, cx| {
                    let mode = BuilderMode::from_slug(&value);
                    let _ = view.update(cx, |this, cx| {
                        this.builder.mode = mode;
                        cx.notify();
                    });
                }
            },
            cx,
        );

        // ── 目标表 ───────────────────────────────────────────────────────
        //
        // 候选来自当前连接的目录（只有表 / 视图）：选了一个目录里没有的表（例如从结果
        // 标签的「浏览表」带过来的）时，把它**补进候选首位** —— 否则下拉按钮会退回
        // 显示一个内部 slug，用户看到的是乱码一样的名字。
        let mut candidates = self.builder_table_candidates();
        if let Some(table) = self.builder.table.clone() {
            let slug = table_slug(&table);
            if !candidates.iter().any(|(known, _)| *known == slug) {
                candidates.insert(0, (slug, table));
            }
        }
        let table_options: Vec<(SharedString, SharedString)> = candidates
            .iter()
            .map(|(slug, table)| {
                (
                    SharedString::from(slug.clone()),
                    SharedString::from(table.display()),
                )
            })
            .collect();
        let current_table = self
            .builder
            .table
            .as_ref()
            .map_or_else(String::new, table_slug);
        let table_select = dropdown_select(
            builder_id("table"),
            table_options,
            SharedString::from(current_table),
            {
                let view = view.clone();
                let candidates = candidates.clone();
                move |value, window, cx| {
                    let Some((_, table)) = candidates
                        .iter()
                        .find(|(slug, _)| slug.as_str() == value.as_ref())
                    else {
                        return;
                    };
                    let table = table.clone();
                    let _ = view.update(cx, |this, cx| {
                        this.builder_set_table(table, window, cx);
                    });
                }
            },
            cx,
        );

        let columns_hint = if self.builder.table.is_none() {
            tr(cx, "先选一张目标表。")
        } else if self.builder.columns_for == self.builder.table {
            if self.builder.table_columns.is_empty() {
                tr(cx, "这张表读不到列清单（视图或权限不足），直接手输列名。")
            } else {
                tr(cx, "列清单已就绪，「插入列」里可以直接挑。")
            }
        } else {
            tr(cx, "正在取列清单…")
        };

        let mut panel = v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(builder_section(
                        tr(cx, "语句"),
                        vec![
                            button(
                                builder_id("reset"),
                                tr(cx, "重置"),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ =
                                        view.update(cx, |this, cx| this.builder_reset(window, cx));
                                }
                            })
                            .into_any_element(),
                        ],
                        cx,
                    ))
                    .child(mode_select),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(builder_section(tr(cx, "目标表"), Vec::new(), cx))
                    .child(table_select)
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.sm)
                            .child(meta_text(tr(cx, "别名"), cx))
                            .child(builder_field(&self.builder.alias, None, cx)),
                    )
                    .child(meta_text(columns_hint, cx)),
            );

        // 列清单候选（四个分区共用一份；「插入列」菜单是它唯一的去处）。
        let column_options = self.builder_column_candidates();

        // ── UPDATE：赋值区 ───────────────────────────────────────────────
        if self.builder.mode == BuilderMode::Update {
            let mut rows: Vec<AnyElement> = Vec::new();
            for (index, row) in self.builder.assignments.iter().enumerate() {
                rows.push(builder_row_shell(
                    vec![
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .child(builder_field(&row.column, None, cx))
                            .child(builder_remove(
                                builder_id(&format!("assign-remove-{index}")),
                                tr(cx, "移除这条赋值"),
                                RemoveAction::Assignment(index),
                                &view,
                                cx,
                            ))
                            .into_any_element(),
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .child(div().flex_none().child(meta_text(tr(cx, "新值"), cx)))
                            .child(builder_field(&row.value, None, cx))
                            .into_any_element(),
                        {
                            let view = view.clone();
                            check_row(
                                builder_id(&format!("assign-raw-{index}")),
                                row.raw,
                                div().child(tr(cx, "按原样表达式写入")),
                                move |checked, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.builder.assignments[index].raw = checked;
                                        cx.notify();
                                    });
                                },
                                cx,
                            )
                            .into_any_element()
                        },
                    ],
                    cx,
                ));
            }
            panel = panel.child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(builder_section(
                        tr(cx, "赋值"),
                        vec![
                            builder_column_menu(
                                builder_id("assign-cols"),
                                RowKind::Assignment,
                                column_options.clone(),
                                &view,
                                cx,
                            ),
                            builder_add(
                                builder_id("assign-add"),
                                tr(cx, "添加"),
                                RowKind::Assignment,
                                &view,
                                cx,
                            )
                            .into_any_element(),
                        ],
                        cx,
                    ))
                    .children(rows)
                    .children((self.builder.assignments.is_empty()).then(|| {
                        meta_text(tr(cx, "UPDATE 至少要有一条赋值。"), cx).into_any_element()
                    })),
            );
        }

        // ── SELECT：投影区 ───────────────────────────────────────────────
        if self.builder.mode == BuilderMode::Select {
            let mut rows: Vec<AnyElement> = Vec::new();
            for (index, row) in self.builder.columns.iter().enumerate() {
                rows.push(builder_row_shell(
                    vec![
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .child(builder_field(&row.column, None, cx))
                            .child(builder_field(&row.alias, Some(86.), cx))
                            .child(builder_remove(
                                builder_id(&format!("col-remove-{index}")),
                                tr(cx, "移除这一列"),
                                RemoveAction::Column(index),
                                &view,
                                cx,
                            ))
                            .into_any_element(),
                    ],
                    cx,
                ));
            }
            panel = panel.child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(builder_section(
                        tr(cx, "输出列"),
                        vec![
                            builder_column_menu(
                                builder_id("col-cols"),
                                RowKind::Column,
                                column_options.clone(),
                                &view,
                                cx,
                            ),
                            builder_add(
                                builder_id("col-add"),
                                tr(cx, "添加"),
                                RowKind::Column,
                                &view,
                                cx,
                            )
                            .into_any_element(),
                        ],
                        cx,
                    ))
                    .child(dropdown_select(
                        builder_id("projection"),
                        ProjectionMode::ALL
                            .into_iter()
                            .map(|mode| {
                                (
                                    SharedString::from(mode.slug()),
                                    SharedString::from(mode.label(cx)),
                                )
                            })
                            .collect::<Vec<_>>(),
                        SharedString::from(self.builder.projection.slug()),
                        {
                            let view = view.clone();
                            move |value, _, cx| {
                                let projection = ProjectionMode::from_slug(&value);
                                let _ = view.update(cx, |this, cx| {
                                    this.builder.projection = projection;
                                    cx.notify();
                                });
                            }
                        },
                        cx,
                    ))
                    .children(if self.builder.projection == ProjectionMode::Explicit {
                        rows
                    } else {
                        Vec::new()
                    })
                    .children(
                        (self.builder.projection == ProjectionMode::Explicit
                            && self.builder.columns.is_empty())
                        .then(|| {
                            meta_text(tr(cx, "一条列都没填，预览会按「全部列」生成。"), cx)
                                .into_any_element()
                        }),
                    ),
            );
        }

        // ── 条件区 ───────────────────────────────────────────────────────
        let mut predicate_rows: Vec<AnyElement> = Vec::new();
        for (index, row) in self.builder.predicates.iter().enumerate() {
            let op_select = dropdown_select(
                builder_id(&format!("pred-op-{index}")),
                FilterOp::ALL
                    .into_iter()
                    .map(|op| {
                        (
                            SharedString::from(filter_op_key(op)),
                            SharedString::from(filter_op_label(op, cx)),
                        )
                    })
                    .collect::<Vec<_>>(),
                SharedString::from(filter_op_key(row.op)),
                {
                    let view = view.clone();
                    move |value, _, cx| {
                        let op = filter_op_from_key(&value);
                        let _ = view.update(cx, |this, cx| {
                            this.builder.predicates[index].op = op;
                            cx.notify();
                        });
                    }
                },
                cx,
            );
            predicate_rows.push(builder_row_shell(
                vec![
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.xs)
                        .child(builder_field(&row.column, None, cx))
                        .child(builder_remove(
                            builder_id(&format!("pred-remove-{index}")),
                            tr(cx, "移除这条条件"),
                            RemoveAction::Predicate(index),
                            &view,
                            cx,
                        ))
                        .into_any_element(),
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.xs)
                        .child(div().flex_none().child(op_select))
                        .child(builder_field_when(&row.value, row.op.needs_value(), cx))
                        .into_any_element(),
                    {
                        let view = view.clone();
                        check_row(
                            builder_id(&format!("pred-raw-{index}")),
                            row.raw_value,
                            div().child(tr(cx, "值按原样表达式下发")),
                            move |checked, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.builder.predicates[index].raw_value = checked;
                                    cx.notify();
                                });
                            },
                            cx,
                        )
                        .into_any_element()
                    },
                ],
                cx,
            ));
        }

        let mut condition_actions: Vec<AnyElement> = vec![
            builder_column_menu(
                builder_id("pred-cols"),
                RowKind::Predicate,
                column_options.clone(),
                &view,
                cx,
            ),
            builder_add(
                builder_id("pred-add"),
                tr(cx, "添加"),
                RowKind::Predicate,
                &view,
                cx,
            )
            .into_any_element(),
        ];
        // 多条条件才有「并且 / 或者」可谈：一条时下拉是纯噪音。
        if self.builder.predicates.len() > 1 {
            condition_actions.insert(
                0,
                dropdown_select(
                    builder_id("bool-op"),
                    BoolOp::ALL
                        .into_iter()
                        .map(|op| {
                            (
                                SharedString::from(op.slug()),
                                SharedString::from(op.label(cx)),
                            )
                        })
                        .collect::<Vec<_>>(),
                    SharedString::from(self.builder.bool_op.slug()),
                    {
                        let view = view.clone();
                        move |value, _, cx| {
                            let op = BoolOp::from_slug(&value);
                            let _ = view.update(cx, |this, cx| {
                                this.builder.bool_op = op;
                                cx.notify();
                            });
                        }
                    },
                    cx,
                ),
            );
        }

        panel = panel.child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(builder_section(tr(cx, "条件"), condition_actions, cx))
                .children(predicate_rows)
                .children(
                    (self.builder.predicates.is_empty())
                        .then(|| meta_text(tr(cx, "还没有条件。"), cx).into_any_element()),
                ),
        );

        // ── SELECT：排序与限制 ───────────────────────────────────────────
        if self.builder.mode == BuilderMode::Select {
            let mut sort_rows: Vec<AnyElement> = Vec::new();
            for (index, row) in self.builder.sorts.iter().enumerate() {
                let (icon, tip) = if row.desc {
                    (DuckIcon::ArrowDown, tr(cx, "降序（点一下改升序）"))
                } else {
                    (DuckIcon::ArrowUp, tr(cx, "升序（点一下改降序）"))
                };
                sort_rows.push(builder_row_shell(
                    vec![
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .child(builder_field(&row.column, None, cx))
                            .child({
                                let view = view.clone();
                                icon_button_tip(
                                    builder_id(&format!("sort-dir-{index}")),
                                    tip,
                                    Icon::new(icon).size(extended.icon.sm),
                                    SharedString::from(tip),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        let row = &mut this.builder.sorts[index];
                                        row.desc = !row.desc;
                                        cx.notify();
                                    });
                                })
                            })
                            .child(builder_remove(
                                builder_id(&format!("sort-remove-{index}")),
                                tr(cx, "移除这条排序"),
                                RemoveAction::Sort(index),
                                &view,
                                cx,
                            ))
                            .into_any_element(),
                    ],
                    cx,
                ));
            }
            panel = panel.child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(builder_section(
                        tr(cx, "排序与限制"),
                        vec![
                            builder_column_menu(
                                builder_id("sort-cols"),
                                RowKind::Sort,
                                column_options.clone(),
                                &view,
                                cx,
                            ),
                            builder_add(
                                builder_id("sort-add"),
                                tr(cx, "添加"),
                                RowKind::Sort,
                                &view,
                                cx,
                            )
                            .into_any_element(),
                        ],
                        cx,
                    ))
                    .children(sort_rows)
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.sm)
                            .child(meta_text(tr(cx, "上限"), cx))
                            .child(builder_field(&self.builder.limit, Some(72.), cx))
                            .child(meta_text(tr(cx, "偏移"), cx))
                            .child(builder_field(&self.builder.offset, Some(72.), cx)),
                    ),
            );
        }

        // ── 预览 + 读数 ──────────────────────────────────────────────────
        //
        // 预览是**唯一**的执行源：下面那个「执行」按的就是这里显示的文本。所以它必须
        // 在动作之上、必须完整可见（可滚动，不截断）。
        let spec = self.builder_spec(cx);
        let preview = sql::render(&spec, dialect);
        let mut notices: Vec<AnyElement> = Vec::new();
        if let Some(rendered) = preview.as_ref() {
            if rendered.incomplete > 0 {
                notices.push(
                    meta_text(
                        trf(
                            cx,
                            "有 {} 条没填完，没有进 SQL。",
                            &[&rendered.incomplete.to_string()],
                        ),
                        cx,
                    )
                    .into_any_element(),
                );
            }
            if sql::offset_is_ignored(&spec, dialect) {
                let note = match dialect.limit {
                    sql::LimitStyle::LimitOffset => tr(
                        cx,
                        "没填上限时偏移量不会下发（各引擎的「无上限」写法不一致）。",
                    ),
                    sql::LimitStyle::OffsetFetch => {
                        tr(cx, "SQL Server 的偏移必须配排序，先加一条排序才会下发。")
                    }
                };
                notices.push(meta_text(note, cx).into_any_element());
            }
            if spec.mode.needs_where() && spec.predicates.is_empty() {
                notices.push(
                    div()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.destructive)
                        .child(tr(cx, "没有条件 —— 这条语句会对整张表生效。"))
                        .into_any_element(),
                );
            }
        }

        let preview_body = match preview.as_ref() {
            Some(rendered) => card(cx)
                .id(builder_id("preview"))
                .w_full()
                .max_h(px(DETAIL_CARD_MAX_HEIGHT))
                .overflow_y_scroll()
                .px(tokens.spacing.lg)
                .py(tokens.spacing.sm)
                .child(
                    div()
                        .text_size(tokens.typography.mono_md.size)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(rendered.sql.clone())),
                )
                .into_any_element(),
            None => meta_text(tr(cx, "选好目标表就会在这里出现 SQL。"), cx).into_any_element(),
        };

        let can_run = preview.is_some() && !self.busy;
        panel
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(builder_section(tr(cx, "预览"), Vec::new(), cx))
                    .child(preview_body)
                    .children(notices),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(
                        button(
                            builder_id("run"),
                            tr(cx, "执行"),
                            ButtonVariant::Primary,
                            cx,
                        )
                        .disabled(!can_run)
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| this.builder_run(window, cx));
                            }
                        }),
                    )
                    .child(
                        button(
                            builder_id("stage"),
                            tr(cx, "在编辑器中打开"),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .disabled(preview.is_none())
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| this.builder_stage(window, cx));
                            }
                        }),
                    ),
            )
            .into_any_element()
    }
}

/// 构建器面板里控件的唯一 id。
fn builder_id(suffix: &str) -> SharedString {
    SharedString::from(format!("{ROW_ID_PREFIX}-{suffix}"))
}

/// 一个文本输入框槽（列 / 值 / 别名通用）；`width` 给定时固定宽，否则吃掉剩余宽度。
fn builder_field(input: &Entity<InputState>, width: Option<f32>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let mut slot = div().min_w_0().flex_1();
    if let Some(width) = width {
        slot = slot.flex_none().w(px(width));
    }
    slot.child(
        Input::new(input)
            .h(theme.density().control)
            .text_size(theme.tokens().typography.sm.size)
            .w_full(),
    )
}

/// 输入框槽，但「这一档不吃值」时整槽淡出（**不禁用**：值仍可读、仍可改）。
fn builder_field_when(input: &Entity<InputState>, enabled: bool, cx: &App) -> Div {
    let slot = builder_field(input, None, cx);
    if enabled {
        slot
    } else {
        slot.opacity(active_theme(cx).extended().opacity.disabled)
    }
}

/// 分区标题行：左标题、右动作。
fn builder_section(label: &'static str, actions: Vec<AnyElement>, cx: &App) -> Div {
    let tokens = active_theme(cx).tokens();
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap(tokens.spacing.sm)
        .child(caption_heading(label, cx))
        .child(
            h_flex()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.xxs)
                .children(actions),
        )
}

/// 行的外壳：窄栏里一行数据 = 一张薄卡（描边 + 圆角），行与行之间才分得清。
fn builder_row_shell(children: Vec<AnyElement>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    v_flex()
        .w_full()
        .gap(tokens.spacing.xxs)
        .p(tokens.spacing.sm)
        .rounded(tokens.radius.md)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .children(children)
        .into_any_element()
}

/// 行尾的「×」：删掉这一行。
fn builder_remove(
    id: SharedString,
    label: &'static str,
    action: RemoveAction,
    view: &WeakEntity<DatabasePage>,
    cx: &App,
) -> duck_components::Button {
    let view = view.clone();
    icon_button(
        id,
        label,
        Icon::new(DuckIcon::X).size(active_theme(cx).extended().icon.sm),
        ButtonVariant::Ghost,
        cx,
    )
    .on_click(move |_, _, cx| {
        let _ = view.update(cx, |this, cx| action.run(this, cx));
    })
}

/// 「插入列」菜单：列清单拿得到时点一下即开一行并填好列名；拿不到时退成一句说明
/// （**不是**一个空菜单 —— 空菜单看起来像坏了）。
fn builder_column_menu(
    id: SharedString,
    kind: RowKind,
    options: Vec<(SharedString, SharedString)>,
    view: &WeakEntity<DatabasePage>,
    cx: &App,
) -> AnyElement {
    if options.is_empty() {
        return meta_text(tr(cx, "列清单未取到，直接手输列名"), cx).into_any_element();
    }
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let view = view.clone();
    // 列多时限高滚动（与 kit `dropdown_select` 同一口径）：PopupMenu 只在
    // `scrollable` 时才套 `max_h`（默认 min(半窗高, 450px)）。
    let long_list = options.len() > 12;
    Button::new(id)
        .ghost()
        .with_size(Size::XSmall)
        .h(theme.density().control)
        .px(tokens.spacing.xs)
        .text_size(tokens.typography.xs.size)
        .label(tr(cx, "插入列"))
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            let menu = menu.scrollable(long_list);
            options.iter().fold(menu, |menu, (value, label)| {
                let value = value.to_string();
                let view = view.clone();
                menu.item(
                    PopupMenuItem::new(label.clone()).on_click(move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.builder_insert_row(kind, value.clone(), window, cx);
                        });
                    }),
                )
            })
        })
        .into_any_element()
}

/// 「添加」按钮：开一条空行（列名自己填）。
fn builder_add(
    id: SharedString,
    label: &'static str,
    kind: RowKind,
    view: &WeakEntity<DatabasePage>,
    cx: &App,
) -> duck_components::Button {
    let view = view.clone();
    button(id, label, ButtonVariant::Ghost, cx).on_click(move |_, window, cx| {
        let _ = view.update(cx, |this, cx| {
            this.builder_insert_row(kind, String::new(), window, cx);
        });
    })
}

/// 「×」要删哪一行 —— 把「按下标删」收成一个值，四个闭包里不必各写一遍分派。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoveAction {
    Column(usize),
    Predicate(usize),
    Sort(usize),
    Assignment(usize),
}

impl RemoveAction {
    fn run(self, page: &mut DatabasePage, cx: &mut Context<DatabasePage>) {
        match self {
            Self::Column(index) => page.builder_remove_column(index, cx),
            Self::Predicate(index) => page.builder_remove_predicate(index, cx),
            Self::Sort(index) => page.builder_remove_sort(index, cx),
            Self::Assignment(index) => page.builder_remove_assignment(index, cx),
        }
    }
}
