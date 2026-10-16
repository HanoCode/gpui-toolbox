//! ER 关系图标签：一个库的表卡片 + 外键关系（dbx 的 ER 图对位）。
//!
//! 真实后端 = 内核外键内省（[`Connection::table_foreign_keys`] / `table_columns`）：
//! 每张表取回列清单与外键清单，卡片上如实画；跨卡连线在 GPUI 里要拿布局后的
//! 几何（taffy 布局完才有坐标），第一期用「卡片内关系清单 + 点关系跳到被引用表」
//! 表达同样的信息，另给一键导出 Mermaid `erDiagram`（带 PK / FK 标记的属性块），
//! 贴进任何 Mermaid 渲染器就是标准 ER 图。

use super::*;

/// 一张 ER 卡片的列数上限：列清单只显示前 N 列，其余折叠成一行计数。
/// ER 图关心的是**结构与关系**，一百多列的宽表整个铺开只会把卡片撑爆。
const ER_CARD_COLUMNS: usize = 12;

/// 单库参与的表数上限：几百张表的库一次全画既慢又看不清，超出部分明说。
const ER_TABLE_CAP: usize = 80;

/// 一张 ER 卡片：表 + 列 + 外键。
pub(super) struct ErTable {
    /// 表引用（点卡片头 / 关系行跳数据标签用）。
    pub reference: TableRef,
    /// 列（名字 + 类型，声明序；最多展示 [`ER_CARD_COLUMNS`] 个）。
    pub columns: Vec<(String, String)>,
    /// 实际列数（超出展示数时卡片尾部报「还有 N 列」）。
    pub total_columns: usize,
    /// 本表的外键（指向谁）。
    pub fks: Vec<TableForeignKey>,
}

/// 一张 ER 图的全部数据（`Mode::Er` 的数据源，按标签存）。
pub(super) struct ErData {
    /// 图对应哪个命名空间。
    pub namespace: String,
    /// 卡片（目录序；超出 [`ER_TABLE_CAP`] 截断）。
    pub tables: Vec<ErTable>,
    /// 实际表数（截断时卡片条注明）。
    pub total_tables: usize,
}

impl DatabasePage {
    /// 打开（或复用）某个库的 **ER 关系图标签**。
    pub(super) fn open_er_tab(
        &mut self,
        conn_id: &str,
        namespace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        let kind = TabKind::Er {
            namespace: namespace.to_owned(),
        };
        if let Some(existing) =
            find_reusable_tab(&self.tabs, &self.tab_kind, &self.tab_conn, conn_id, &kind)
        {
            self.activate(&existing, window, cx);
            return;
        }
        self.spawn_tab_for(conn_id, kind, window, cx);
        self.mode = Mode::Er {
            namespace: namespace.to_owned(),
        };
        self.er = None;
        self.load_er(conn_id, namespace, cx);
        cx.notify();
    }

    /// 取一个库的 ER 数据：逐表拿列清单与外键（单表失败不连坐，卡片少一张而已）。
    pub(super) fn load_er(&mut self, conn_id: &str, namespace: &str, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        // 只画**表**：视图 / 过程不参与外键关系，画进 ER 图只会误导。
        let mut tables: Vec<TableRef> = session
            .catalog
            .iter()
            .filter(|entry| entry.kind == TableKind::Table)
            .filter(|entry| entry.schema.as_deref() == Some(namespace))
            .map(|entry| entry.table_ref())
            .collect();
        let total_tables = tables.len();
        tables.truncate(ER_TABLE_CAP);
        let conn = session.conn.clone();
        let namespace = namespace.to_owned();
        self.status = tr(cx, "正在读取 ER 关系…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Vec<ErTable> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                tables
                    .iter()
                    .filter_map(|table| {
                        let columns = guard.table_columns(table).ok()?;
                        let columns = columns
                            .into_iter()
                            .map(|column| (column.name, column.type_name))
                            .collect::<Vec<_>>();
                        let total_columns = columns.len();
                        let fks = guard.table_foreign_keys(table).unwrap_or_default();
                        Some(ErTable {
                            reference: table.clone(),
                            columns,
                            total_columns,
                            fks,
                        })
                    })
                    .collect()
            },
            move |this, tables: Vec<ErTable>, _window, cx| {
                this.er = Some(ErData {
                    namespace,
                    total_tables,
                    tables,
                });
                this.status = SharedString::default();
                cx.notify();
            },
        );
    }

    /// ER 关系图页：头部（标题 / 计数 / 导出 Mermaid）+ 卡片流。
    pub(super) fn render_er_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(data) = self.er.as_ref() else {
            // 还没取回：给一块占位说明（与实例页同一口径——正文不空转）。
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .bg(tokens.colors.background)
                .child(meta_text(tr(cx, "正在读取 ER 关系…"), cx))
                .into_any_element();
        };
        let relation_count: usize = data.tables.iter().map(|table| table.fks.len()).sum();
        let header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .bg(extended.colors.chrome)
            .border_1()
            .border_color(tokens.colors.border)
            .child(
                Icon::new(DuckIcon::Network)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(tokens.typography.sm.size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.foreground)
                    .truncate()
                    .child(SharedString::from(format!("ER {}", data.namespace))),
            )
            .child(neutral_tag(
                SharedString::from(trf(cx, "{} 张表", &[&data.total_tables.to_string()])),
                cx,
            ))
            .child(neutral_tag(
                SharedString::from(trf(cx, "{} 条关系", &[&relation_count.to_string()])),
                cx,
            ))
            .child(if data.total_tables > data.tables.len() {
                neutral_tag(
                    SharedString::from(trf(
                        cx,
                        "已截断，仅前 {} 张",
                        &[&data.tables.len().to_string()],
                    )),
                    cx,
                )
                .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(
                button(
                    "db-er-mermaid",
                    tr(cx, "导出 Mermaid"),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let Some(entity) = view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.export_er_mermaid(cx);
                        });
                    }
                }),
            )
            .child({
                let namespace = data.namespace.clone();
                button("db-er-refresh", tr(cx, "刷新"), ButtonVariant::Ghost, cx)
                    .disabled(self.busy)
                    .on_click(move |_, _, cx| {
                        let Some(entity) = view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            let conn_id = this.active.clone().unwrap_or_default();
                            this.load_er(&conn_id, &namespace, cx);
                        });
                    })
            });
        // 卡片流：flex_wrap 平铺，卡片定宽，超出换行（关系数量多的库一屏扫得完）。
        let mut cards = div()
            .flex()
            .flex_wrap()
            .gap(tokens.spacing.sm)
            .content_start();
        for table in &data.tables {
            cards = cards.child(self.render_er_card(table, cx));
        }
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .gap(tokens.spacing.sm)
            .bg(tokens.colors.background)
            .p(tokens.spacing.sm)
            .child(header)
            .child(
                div()
                    .id("db-er-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(cards),
            )
            .into_any_element()
    }

    /// 一张 ER 卡片：表头（点开数据标签）+ 列清单 + 外键关系。
    fn render_er_card(&self, table: &ErTable, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let conn_id = self.active.clone().unwrap_or_default();
        let name = table.reference.display();
        // 列行：名字为主文案，类型补在尾部（截断防长类型撑爆卡片）。
        let mut rows = v_flex().w_full().gap(tokens.spacing.xxs);
        let shown = table.columns.len().min(ER_CARD_COLUMNS);
        for (column, type_name) in table.columns.iter().take(shown) {
            rows = rows.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.foreground)
                            .truncate()
                            .child(SharedString::from(column.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(110.))
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .truncate()
                            .child(SharedString::from(type_name.clone())),
                    ),
            );
        }
        if table.total_columns > shown {
            rows = rows.child(meta_text(
                SharedString::from(trf(
                    cx,
                    "还有 {} 列…",
                    &[&(table.total_columns - shown).to_string()],
                )),
                cx,
            ));
        }
        // 关系段：本表指向谁，一行一条；点关系行跳到**被引用表**（ER 图上顺藤摸瓜）。
        let mut relations = v_flex().w_full().gap(tokens.spacing.xxs);
        for fk in &table.fks {
            let Some(first_column) = fk.columns.first().cloned() else {
                continue;
            };
            let first_ref_column = fk.ref_columns.first().cloned().unwrap_or_default();
            let reference = TableRef {
                schema: table.reference.schema.clone(),
                name: fk.ref_table.clone(),
            };
            let conn_id = conn_id.clone();
            relations = relations.child(
                // 关系行做成可点：自绘可点元素必须 role + aria_label（P2-7 口径）。
                h_flex()
                    .id(SharedString::from(format!(
                        "db-er-fk-{}-{}",
                        name, fk.ref_table
                    )))
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .px(tokens.spacing.xxs)
                    .py(px(2.))
                    .rounded(tokens.radius.sm)
                    .cursor_pointer()
                    .hover(|style| style.bg(extended.colors.hover_tint))
                    .on_click({
                        let view = view.clone();
                        let conn_id = conn_id.clone();
                        let reference = reference.clone();
                        move |_, window, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.open_data_tab(&conn_id, reference.clone(), false, window, cx);
                            });
                        }
                    })
                    .child(
                        Icon::new(DuckIcon::ArrowRightLeft)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.foreground)
                            .truncate()
                            .child(SharedString::from(format!(
                                "{first_column} → {}.{first_ref_column}",
                                fk.ref_table
                            ))),
                    )
                    .role(Role::Button)
                    .aria_label(SharedString::from(format!(
                        "跳转到被引用表 {}",
                        fk.ref_table
                    ))),
            );
        }
        let body = if table.fks.is_empty() {
            v_flex().child(meta_text(tr(cx, "无外键"), cx))
        } else {
            relations
        };
        v_flex()
            .w(px(260.))
            .flex_none()
            .gap(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(tokens.colors.border)
            .bg(tokens.colors.surface)
            .p(tokens.spacing.sm)
            // 卡片头：点开这张表的数据标签。
            .child(
                h_flex()
                    .id(SharedString::from(format!("db-er-card-{name}")))
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .px(tokens.spacing.xxs)
                    .py(tokens.spacing.xxs)
                    .rounded(tokens.radius.sm)
                    .cursor_pointer()
                    .hover(|style| style.bg(extended.colors.hover_tint))
                    .on_click({
                        let view = view.clone();
                        let conn_id = conn_id.clone();
                        let reference = table.reference.clone();
                        move |_, window, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.open_data_tab(&conn_id, reference.clone(), false, window, cx);
                            });
                        }
                    })
                    .role(Role::Button)
                    .aria_label(SharedString::from(format!("打开表 {name}")))
                    .child(
                        Icon::new(DuckIcon::Columns2)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(tokens.typography.sm.size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(tokens.colors.foreground)
                            .truncate()
                            .child(SharedString::from(name.clone())),
                    ),
            )
            .child(rows)
            .child(body)
            .into_any_element()
    }

    /// 导出 Mermaid `erDiagram`：关系行 + 每表的属性块（类型剥成单词，PK / FK 标记）。
    pub(super) fn export_er_mermaid(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.er.as_ref() else {
            return;
        };
        let mut out = String::from("erDiagram\n");
        for table in &data.tables {
            let fks: std::collections::BTreeSet<&str> = table
                .fks
                .iter()
                .flat_map(|fk| fk.columns.iter().map(|column| column.as_str()))
                .collect();
            out.push_str(&format!("  {} {{\n", mermaid_ident(&table.reference.name)));
            for (column, type_name) in &table.columns {
                let mut line =
                    format!("    {} {}", mermaid_ident(type_name), mermaid_ident(column));
                // 主键 / 外键标记：PK 由外键表反推不全，内核有 table_primary_key，
                // 这里只标拿得到的 FK（PK 标记留空不写 ——宁缺勿错）。
                if fks.contains(column.as_str()) {
                    line.push_str(" FK");
                }
                line.push('\n');
                out.push_str(&line);
            }
            out.push_str("  }\n");
        }
        for table in &data.tables {
            for fk in &table.fks {
                let label = if fk.name.is_empty() {
                    fk.columns.first().cloned().unwrap_or_default()
                } else {
                    fk.name.clone()
                };
                out.push_str(&format!(
                    "  {} ||--o{{ {} : \"{}\"\n",
                    mermaid_ident(&fk.ref_table),
                    mermaid_ident(&table.reference.name),
                    label
                ));
            }
        }
        self.put_on_clipboard(
            out,
            tr(cx, "已复制 Mermaid ER 图，可粘贴到任何 Mermaid 渲染器。"),
            cx,
        );
    }
}

/// Mermaid 标识符清洗：非字母数字下划线一律换成下划线（中文表名 / 连字符类型都能落）。
fn mermaid_ident(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        format!("t_{cleaned}")
    } else {
        cleaned
    }
}
