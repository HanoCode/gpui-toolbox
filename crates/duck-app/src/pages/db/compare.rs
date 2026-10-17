//! 比较数据标签：源库 vs 目标连接 + 库，逐表对比结构与行数（dbx 的 Compare 对位）。
//!
//! 真实后端 = 两侧分别内省 + `COUNT(*)` + **主键级行差**：两张表两侧都在、主键同名时，
//! 各拉一份主键值在内存里做差，报出「目标缺失 N 行 / 目标多出 N 行」和主键样例；
//! 差异行可以一键生成补数 INSERT（拉回缺失行的完整数据）或直接开一张单表传输标签。
//! 单元格级的逐列对比不在第一期口径（那要对齐两侧类型系统），界面上如实注明。

use super::*;

/// 参与比较的表数上限：两侧逐表跑 `COUNT(*)` 与主键查询，几百张表已经是分钟级。
const COMPARE_TABLE_CAP: usize = 200;

/// 主键值清单的拉取上限：主键 diff 只在两侧各 5 万键以内做，超出就明说「未比对」。
const COMPARE_PK_CAP: usize = 50_000;

/// 补数 SQL 一次生成的行数上限：缺失行再多也先补前 500 行，剩下交给传输。
const COMPARE_BACKFILL_CAP: usize = 500;

/// 一张表的比较结论。
pub(super) enum CompareRowState {
    /// 只在源有（目标缺整张表）。
    OnlySource {
        /// 源侧行数。
        rows: u64,
    },
    /// 只在目标有（源缺整张表）。
    OnlyTarget {
        /// 目标侧行数。
        rows: u64,
    },
    /// 两侧都有：行数 / 列名 / 主键级行差。
    Both {
        /// 源侧行数。
        source_rows: u64,
        /// 目标侧行数。
        target_rows: u64,
        /// 只在源有的列名（目标缺列）。
        columns_missing_in_target: Vec<String>,
        /// 只在目标有的列名。
        columns_missing_in_source: Vec<String>,
        /// 目标缺失的主键值（源有目标无；主键可比对时才有）。
        missing_in_target: Vec<String>,
        /// 目标多出的主键值。
        missing_in_source: Vec<String>,
        /// 主键列名（空 = 两侧都取不到主键）。
        pk: Vec<String>,
        /// 主键级 diff 是否真的跑过（键数超上限或主键不一致时为 false）。
        pk_compared: bool,
    },
}

/// 比较报告的一行（一张表）。
pub(super) struct CompareRow {
    /// 源侧表引用（补数 / 传输都从源拉）。
    pub table: TableRef,
    /// 结论。
    pub state: CompareRowState,
}

/// 比较报告整体。
pub(super) struct CompareReport {
    /// 逐表结论（按表名排序）。
    pub rows: Vec<CompareRow>,
}

/// 比较标签的运行状态。
pub(super) enum CompareStatus {
    /// 刚打开，还没跑。
    Idle,
    /// 跑着。
    Loading,
    /// 出了报告。
    Ready(CompareReport),
    /// 整体失败（连不上 / 目录都取不到）。
    Failed(String),
}

/// 比较标签的全部状态（`Mode::Compare` 的数据源，按标签存）。
pub(super) struct CompareState {
    /// 两侧规格（目标连接 / 库在页面上选）。
    pub spec: CompareSpec,
    /// 运行状态。
    pub status: CompareStatus,
    /// 目标连接下拉。
    pub conn_select: Entity<DbSelectState>,
    /// 目标库下拉。
    pub ns_select: Entity<DbSelectState>,
    /// 两个下拉的选项签名（变了才重灌，见 `sync_select_pickers` 同款）。
    pub conn_stamp: Option<String>,
    pub ns_stamp: Option<String>,
}

impl DatabasePage {
    /// 打开（或复用）一张**比较数据标签**：源 = 打开菜单所在的连接 + 命名空间，
    /// 目标默认挑另一条已连接会话（没有就源自己，页面上可改）。
    pub(super) fn open_compare_tab(
        &mut self,
        conn_id: &str,
        namespace: &str,
        // 表节点右键「比较数据」带单张；库级入口 / 工具条传空 = 全库。
        tables: Vec<TableRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        // 目标连接默认 = 第一条**别的**已连接会话；没有就源自己（同库比较也有意义：
        // 两个命名空间的数据对账在 dbx 里就是这么跑的 —— 这里源库固定，目标库可换）。
        let target_conn = self
            .sessions
            .keys()
            .find(|other| other.as_str() != conn_id)
            .cloned()
            .unwrap_or_else(|| conn_id.to_owned());
        let target_ns = self
            .sessions
            .get(&target_conn)
            .and_then(|session| {
                if namespace_is_database(session.kind) {
                    session
                        .databases
                        .first()
                        .cloned()
                        .or_else(|| session.schema.clone())
                } else {
                    session.schema.clone()
                }
            })
            .unwrap_or_default();
        let spec = CompareSpec {
            source_conn: conn_id.to_owned(),
            source_ns: namespace.to_owned(),
            target_conn,
            target_ns,
            tables,
        };
        let kind = TabKind::Compare(spec.clone());
        if let Some(existing) =
            find_reusable_tab(&self.tabs, &self.tab_kind, &self.tab_conn, conn_id, &kind)
        {
            self.activate(&existing, window, cx);
            return;
        }
        // 两个下拉实体 + 订阅：确认事件改**当前标签**的 spec（`self.compare` 就是
        // 活动标签那份 —— 用户只能点到活动标签上的下拉）。
        let conn_select = db_select_state(window, cx);
        let ns_select = db_select_state(window, cx);
        self.spawn_tab_for(conn_id, kind, window, cx);
        self._subscriptions.push(cx.subscribe(
            &conn_select,
            |this, _, event: &SelectEvent<SearchableVec<DbSelectOption>>, cx| {
                let SelectEvent::Confirm(value) = event;
                if let Some(state) = this.compare.as_mut() {
                    state.spec.target_conn =
                        value.as_ref().map(|v| v.to_string()).unwrap_or_default();
                    state.spec.target_ns.clear();
                    state.ns_stamp = None;
                    cx.notify();
                }
            },
        ));
        self._subscriptions.push(cx.subscribe(
            &ns_select,
            |this, _, event: &SelectEvent<SearchableVec<DbSelectOption>>, cx| {
                let SelectEvent::Confirm(value) = event;
                if let Some(state) = this.compare.as_mut() {
                    state.spec.target_ns =
                        value.as_ref().map(|v| v.to_string()).unwrap_or_default();
                    cx.notify();
                }
            },
        ));
        self.mode = Mode::Compare(spec.clone());
        self.compare = Some(CompareState {
            spec,
            status: CompareStatus::Idle,
            conn_select,
            ns_select,
            conn_stamp: None,
            ns_stamp: None,
        });
        cx.notify();
    }

    /// 开跑比较：源表清单来自源会话目录，其余全部在后台闭包里对两侧连接跑。
    pub(super) fn run_compare(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.compare.as_ref() else {
            return;
        };
        let spec = state.spec.clone();
        let Some(source_session) = self.sessions.get(&spec.source_conn) else {
            return;
        };
        let Some(target_session) = self.sessions.get(&spec.target_conn) else {
            return;
        };
        // 源表清单：表级入口（spec 里带了表）直接用 —— 哪怕它已不在目录快照里
        //（刚建的表还没重新内省也照样能比）；库级入口才从源会话的目录现过滤。
        let mut source_tables: Vec<TableRef> = if spec.tables.is_empty() {
            source_session
                .catalog
                .iter()
                .filter(|entry| entry.kind == TableKind::Table)
                .filter(|entry| entry.schema.as_deref() == Some(spec.source_ns.as_str()))
                .map(|entry| entry.table_ref())
                .collect()
        } else {
            spec.tables.clone()
        };
        source_tables.truncate(COMPARE_TABLE_CAP);
        let source_conn = source_session.conn.clone();
        let target_conn = target_session.conn.clone();
        let target_ns = spec.target_ns.clone();
        if let Some(state) = self.compare.as_mut() {
            state.status = CompareStatus::Loading;
        }
        self.status = tr(cx, "正在比较数据…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<CompareReport, DbError> {
                // 目标侧目录：拿目标库的表名集合（Introspection::load 一次取齐，
                // 与建连同一条内省路径）。
                let target_names: std::collections::BTreeSet<String> = {
                    let mut guard = target_conn.lock().expect("连接锁不应中毒");
                    Introspection::load(&mut guard, Some(&target_ns))?
                        .catalog
                        .iter()
                        .filter(|entry| entry.kind == TableKind::Table)
                        .map(|entry| entry.name.clone())
                        .collect()
                };
                let source_names: std::collections::BTreeSet<String> = source_tables
                    .iter()
                    .map(|table| table.name.clone())
                    .collect();
                let mut report_rows: Vec<CompareRow> = Vec::new();
                // 逐源表比对：不存在 → OnlySource；都在 → 行数 / 列 / 主键级 diff。
                for table in &source_tables {
                    if !target_names.contains(&table.name) {
                        let rows = {
                            let mut guard = source_conn.lock().expect("连接锁不应中毒");
                            count_rows(&mut guard, table)?
                        };
                        report_rows.push(CompareRow {
                            table: table.clone(),
                            state: CompareRowState::OnlySource { rows },
                        });
                        continue;
                    }
                    let target_ref = TableRef {
                        schema: Some(target_ns.clone()),
                        name: table.name.clone(),
                    };
                    // 两侧各取行数 / 列名 / 主键（哪侧失败都让整次比较失败：
                    // 半份报告比没有报告更害人）。
                    let (source_rows, source_columns, source_pk) = {
                        let mut guard = source_conn.lock().expect("连接锁不应中毒");
                        let rows = count_rows(&mut guard, table)?;
                        let columns = guard
                            .table_columns(table)?
                            .into_iter()
                            .map(|column| column.name)
                            .collect::<Vec<_>>();
                        let pk = guard.table_primary_key(table).unwrap_or_default();
                        (rows, columns, pk)
                    };
                    let (target_rows, target_columns, target_pk) = {
                        let mut guard = target_conn.lock().expect("连接锁不应中毒");
                        let rows = count_rows(&mut guard, &target_ref)?;
                        let columns = guard
                            .table_columns(&target_ref)?
                            .into_iter()
                            .map(|column| column.name)
                            .collect::<Vec<_>>();
                        let pk = guard.table_primary_key(&target_ref).unwrap_or_default();
                        (rows, columns, pk)
                    };
                    let columns_missing_in_target: Vec<String> = source_columns
                        .iter()
                        .filter(|column| !target_columns.contains(column))
                        .cloned()
                        .collect();
                    let columns_missing_in_source: Vec<String> = target_columns
                        .iter()
                        .filter(|column| !source_columns.contains(column))
                        .cloned()
                        .collect();
                    // 主键级行差：主键同名且非空才跑；键数超上限就明说未比对。
                    let pk_usable = !source_pk.is_empty() && source_pk == target_pk;
                    let (missing_in_target, missing_in_source, pk_compared) = if pk_usable {
                        let source_keys =
                            fetch_pk_values(&source_conn, table, &source_pk, COMPARE_PK_CAP)?;
                        let target_keys =
                            fetch_pk_values(&target_conn, &target_ref, &source_pk, COMPARE_PK_CAP)?;
                        if source_keys.len() >= COMPARE_PK_CAP
                            || target_keys.len() >= COMPARE_PK_CAP
                        {
                            (Vec::new(), Vec::new(), false)
                        } else {
                            (
                                source_keys.difference(&target_keys).cloned().collect(),
                                target_keys.difference(&source_keys).cloned().collect(),
                                true,
                            )
                        }
                    } else {
                        (Vec::new(), Vec::new(), false)
                    };
                    report_rows.push(CompareRow {
                        table: table.clone(),
                        state: CompareRowState::Both {
                            source_rows,
                            target_rows,
                            columns_missing_in_target,
                            columns_missing_in_source,
                            missing_in_target,
                            missing_in_source,
                            pk: source_pk,
                            pk_compared,
                        },
                    });
                }
                // 目标独有表：目标目录里有、源目录没有的。
                for name in &target_names {
                    if source_names.contains(name) {
                        continue;
                    }
                    let target_ref = TableRef {
                        schema: Some(target_ns.clone()),
                        name: name.clone(),
                    };
                    let rows = {
                        let mut guard = target_conn.lock().expect("连接锁不应中毒");
                        count_rows(&mut guard, &target_ref)?
                    };
                    report_rows.push(CompareRow {
                        table: TableRef {
                            schema: None,
                            name: name.clone(),
                        },
                        state: CompareRowState::OnlyTarget { rows },
                    });
                }
                report_rows.sort_by(|left, right| left.table.name.cmp(&right.table.name));
                Ok(CompareReport { rows: report_rows })
            },
            move |this, result: Result<CompareReport, DbError>, window, cx| match result {
                Ok(report) => {
                    if let Some(state) = this.compare.as_mut() {
                        state.status = CompareStatus::Ready(report);
                    }
                    this.status = SharedString::default();
                }
                Err(error) => {
                    let reason = error.to_string();
                    if let Some(state) = this.compare.as_mut() {
                        state.status = CompareStatus::Failed(reason.clone());
                    }
                    this.status = trf(cx, "比较失败：{}", &[&reason]).into();
                    notify::error(trf(cx, "比较失败：{}", &[&reason]), window, cx);
                }
            },
        );
    }

    /// 生成缺失行的**补数 INSERT**：把目标缺失的主键行从源库拉回来（前
    /// [`COMPARE_BACKFILL_CAP`] 行），按目标方言拼 INSERT 放进编辑器。
    pub(super) fn generate_backfill_sql(
        &mut self,
        table: TableRef,
        pk: Vec<String>,
        keys: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.compare.as_ref() else {
            return;
        };
        let spec = state.spec.clone();
        let Some(session) = self.sessions.get(&spec.source_conn) else {
            return;
        };
        let conn = session.conn.clone();
        let quote = session.quote.unwrap_or('"');
        let Ok(qualified) = quote_qualified(&table, quote) else {
            return;
        };
        let column_list = pk
            .iter()
            .filter_map(|column| duck_db::drivers::quote_identifier(column, quote).ok())
            .collect::<Vec<_>>()
            .join(", ");
        self.status = tr(cx, "正在读取缺失行…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<(String, usize), DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                let selected = keys
                    .into_iter()
                    .take(COMPARE_BACKFILL_CAP)
                    .collect::<Vec<_>>();
                if selected.is_empty() {
                    return Ok((String::new(), 0));
                }
                // 主键值清单转成 IN 列表：值是展示文本（字符串带引号），逐个转义。
                let in_list = selected
                    .iter()
                    .map(|key| format!("'{}'", key.replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(", ");
                let outcome = guard.execute(
                    &format!("SELECT * FROM {qualified} WHERE ({column_list}) IN ({in_list});"),
                    COMPARE_BACKFILL_CAP,
                )?;
                let result = outcome.result.unwrap_or_default();
                let columns: Vec<String> = result
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect();
                let mut statements = String::new();
                let mut count = 0;
                for row in &result.rows {
                    let values = row
                        .iter()
                        .map(|value| match value {
                            duck_db::Value::Null => "NULL".to_owned(),
                            value => format!("'{}'", value.render().replace('\'', "''")),
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let columns_sql = columns
                        .iter()
                        .filter_map(|column| duck_db::drivers::quote_identifier(column, quote).ok())
                        .collect::<Vec<_>>()
                        .join(", ");
                    statements.push_str(&format!(
                        "INSERT INTO {qualified} ({columns_sql}) VALUES ({values});\n"
                    ));
                    count += 1;
                }
                Ok((statements, count))
            },
            move |this, result: Result<(String, usize), DbError>, window, cx| match result {
                Ok((sql, count)) => {
                    if sql.is_empty() {
                        notify::info(tr(cx, "没有需要补的行。"), window, cx);
                    } else {
                        this.stage_sql(
                            sql,
                            trf(
                                cx,
                                "已生成 {} 条补数 INSERT，确认无误后再执行。",
                                &[&count.to_string()],
                            )
                            .into(),
                            window,
                            cx,
                        );
                    }
                    this.status = SharedString::default();
                }
                Err(error) => {
                    let reason = error.to_string();
                    this.status = trf(cx, "读取缺失行失败：{}", &[&reason]).into();
                    notify::error(trf(cx, "读取缺失行失败：{}", &[&reason]), window, cx);
                }
            },
        );
    }

    /// 比较数据页：规格条（源 → 目标 + 开跑）+ 报告列表。
    ///
    /// 两个目标下拉的选项 / 选中值在 [`Self::sync_compare_pickers`]（渲染入口）同步，
    /// 这里只读状态画。
    pub(super) fn render_compare_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(state) = self.compare.as_ref() else {
            return div().into_any_element();
        };
        let spec = &state.spec;
        let source_label = self
            .connections
            .iter()
            .find(|cfg| cfg.id == spec.source_conn)
            .map(|cfg| cfg.display_name())
            .unwrap_or_else(|| spec.source_conn.clone());
        let can_run = !spec.target_conn.is_empty() && !spec.target_ns.is_empty() && !self.busy;
        // 规格条：源（只读展示）→ 目标（两个下拉）+ 开跑按钮。
        let bar = h_flex()
            .w_full()
            .flex_none()
            .flex_wrap()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .bg(extended.colors.chrome)
            .border_1()
            .border_color(tokens.colors.border)
            .child(
                Icon::new(DuckIcon::ArrowRightLeft)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
            )
            .child(neutral_tag(
                SharedString::from(format!("{source_label}:{}", spec.source_ns)),
                cx,
            ))
            .child(
                Icon::new(DuckIcon::ChevronRight)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
            )
            .child(
                Select::new(&state.conn_select)
                    .placeholder(tr(cx, "目标连接"))
                    .h(theme.density().control)
                    .min_w(px(160.)),
            )
            .child(
                Select::new(&state.ns_select)
                    .placeholder(tr(cx, "目标库"))
                    .h(theme.density().control)
                    .min_w(px(160.)),
            )
            .child(div().flex_1())
            .child(match &state.status {
                CompareStatus::Loading => {
                    neutral_tag(SharedString::from(tr(cx, "比较中…")), cx).into_any_element()
                }
                _ => div().into_any_element(),
            })
            .child(
                button(
                    "db-compare-run",
                    tr(cx, "开始比较"),
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(!can_run)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let Some(entity) = view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.run_compare(cx);
                        });
                    }
                }),
            );
        // 报告列表。
        let list: AnyElement = match &state.status {
            CompareStatus::Idle => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(meta_text(tr(cx, "选好目标连接与库，点「开始比较」。"), cx))
                .into_any_element(),
            CompareStatus::Loading => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(meta_text(tr(cx, "正在比较数据…"), cx))
                .into_any_element(),
            CompareStatus::Failed(reason) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(meta_text(
                    SharedString::from(format!("比较失败：{reason}")),
                    cx,
                ))
                .into_any_element(),
            CompareStatus::Ready(report) => {
                let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
                for row in &report.rows {
                    list = list.child(self.render_compare_row(row, cx));
                }
                div()
                    .id("db-compare-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex().w_full().gap(tokens.spacing.xs).child(
                            div()
                                .rounded(tokens.radius.md)
                                .border_1()
                                .border_color(tokens.colors.border)
                                .bg(tokens.colors.surface)
                                .p(tokens.spacing.sm)
                                .child(list),
                        ),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .gap(tokens.spacing.sm)
            .bg(tokens.colors.background)
            .p(tokens.spacing.sm)
            .child(bar)
            .child(list)
            .into_any_element()
    }

    /// 报告的一行：表名 + 结论 chip + 差异明细 + 动作（补数 SQL / 传输此表）。
    fn render_compare_row(&self, row: &CompareRow, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let (chip, detail, state_label) = match &row.state {
            CompareRowState::OnlySource { rows } => (
                accent_tag(SharedString::from(tr(cx, "仅源有")), cx),
                meta_text(
                    SharedString::from(trf(cx, "{} 行", &[&rows.to_string()])),
                    cx,
                )
                .into_any_element(),
                "only-source",
            ),
            CompareRowState::OnlyTarget { rows } => (
                accent_tag(SharedString::from(tr(cx, "仅目标有")), cx),
                meta_text(
                    SharedString::from(trf(cx, "{} 行", &[&rows.to_string()])),
                    cx,
                )
                .into_any_element(),
                "only-target",
            ),
            CompareRowState::Both {
                source_rows,
                target_rows,
                columns_missing_in_target,
                columns_missing_in_source,
                missing_in_target,
                missing_in_source,
                pk,
                pk_compared,
            } => {
                let rows_equal = source_rows == target_rows;
                let columns_equal =
                    columns_missing_in_target.is_empty() && columns_missing_in_source.is_empty();
                let chip = if rows_equal && columns_equal {
                    neutral_tag(SharedString::from(tr(cx, "一致")), cx)
                } else {
                    accent_tag(SharedString::from(tr(cx, "有差异")), cx)
                };
                let mut parts: Vec<String> = Vec::new();
                parts.push(trf(
                    cx,
                    "行数 源 {} / 目标 {}",
                    &[&source_rows.to_string(), &target_rows.to_string()],
                ));
                if !columns_missing_in_target.is_empty() {
                    parts.push(trf(
                        cx,
                        "目标缺列 {}",
                        &[&columns_missing_in_target.join(", ")],
                    ));
                }
                if !columns_missing_in_source.is_empty() {
                    parts.push(trf(
                        cx,
                        "源缺列 {}",
                        &[&columns_missing_in_source.join(", ")],
                    ));
                }
                if *pk_compared {
                    if !missing_in_target.is_empty() {
                        let sample: Vec<String> =
                            missing_in_target.iter().take(5).cloned().collect();
                        parts.push(trf(
                            cx,
                            "目标缺失 {} 行（如 {}…）",
                            &[&missing_in_target.len().to_string(), &sample.join(", ")],
                        ));
                    }
                    if !missing_in_source.is_empty() {
                        parts.push(trf(
                            cx,
                            "目标多出 {} 行",
                            &[&missing_in_source.len().to_string()],
                        ));
                    }
                    if missing_in_target.is_empty() && missing_in_source.is_empty() {
                        parts.push(tr(cx, "主键级行集一致").to_owned());
                    }
                } else if !pk.is_empty() {
                    parts.push(tr(cx, "主键级行集未比对（键数超上限）").to_owned());
                } else {
                    parts.push(tr(cx, "无主键，未做行级比对").to_owned());
                }
                (
                    chip,
                    meta_text(SharedString::from(parts.join("；")), cx).into_any_element(),
                    "both",
                )
            }
        };
        // 动作：补数 SQL（有目标缺失行时）与传输此表（两侧都有且行数不一致时）。
        let mut actions = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        if let CompareRowState::Both {
            missing_in_target,
            pk,
            ..
        } = &row.state
            && !missing_in_target.is_empty()
            && !pk.is_empty()
        {
            let pk = pk.clone();
            let keys = missing_in_target.clone();
            let table = row.table.clone();
            actions = actions.child(
                button(
                    SharedString::from(format!(
                        "db-compare-backfill-{state_label}-{}",
                        row.table.name
                    )),
                    tr(cx, "生成补数 SQL"),
                    ButtonVariant::Link,
                    cx,
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let Some(entity) = view.upgrade() else {
                            return;
                        };
                        let (table, pk, keys) = (table.clone(), pk.clone(), keys.clone());
                        entity.update(cx, |this, cx| {
                            this.generate_backfill_sql(table, pk, keys, cx);
                        });
                    }
                }),
            );
        }
        if matches!(
            row.state,
            CompareRowState::Both { .. } | CompareRowState::OnlySource { .. }
        ) {
            let table = TableRef {
                schema: row.table.schema.clone(),
                name: row.table.name.clone(),
            };
            actions = actions.child(
                button(
                    SharedString::from(format!(
                        "db-compare-transfer-{state_label}-{}",
                        row.table.name
                    )),
                    tr(cx, "传输此表"),
                    ButtonVariant::Link,
                    cx,
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let Some(entity) = view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            let (conn_id, ns) = match &this.compare {
                                Some(state) => {
                                    (state.spec.source_conn.clone(), state.spec.source_ns.clone())
                                }
                                None => return,
                            };
                            this.open_transfer_tab(&conn_id, &ns, vec![table.clone()], window, cx);
                        });
                    }
                }),
            );
        }
        h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.xs)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.sm)
            .hover(|style| style.bg(extended.colors.hover_tint))
            .child(
                div()
                    .min_w_0()
                    .flex_none()
                    .max_w(px(280.))
                    .text_size(tokens.typography.sm.size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.foreground)
                    .truncate()
                    .child(selectable_text(SharedString::from(row.table.display()))),
            )
            .child(chip)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(tokens.typography.xs.size)
                    .child(detail),
            )
            .child(actions)
            .into_any_element()
    }

    /// 比较页的两个目标下拉：选项与选中值按签名同步（渲染入口调用）。
    pub(super) fn sync_compare_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.compare.as_ref() else {
            return;
        };
        // 目标连接 = 全部**已连接**的会话（未连接的没有会话，跑了也是失败）。
        let conn_options: Vec<DbSelectOption> = self
            .sessions
            .keys()
            .filter_map(|conn_id| {
                self.connections
                    .iter()
                    .find(|cfg| &cfg.id == conn_id)
                    .map(|cfg| DbSelectOption {
                        value: SharedString::from(conn_id.clone()),
                        label: SharedString::from(cfg.display_name()),
                    })
            })
            .collect();
        let conn_signature = conn_options
            .iter()
            .map(|option| option.value.to_string())
            .collect::<Vec<_>>()
            .join("\u{1}");
        if state.conn_stamp.as_deref() != Some(conn_signature.as_str())
            && let Some(state) = self.compare.as_mut()
        {
            state.conn_stamp = Some(conn_signature);
            state.conn_select.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(conn_options), window, cx);
            });
        }
        let Some(state) = self.compare.as_ref() else {
            return;
        };
        let selected_conn = state.conn_select.read(cx).selected_value().cloned();
        let selected_conn =
            selected_conn.unwrap_or_else(|| SharedString::from(state.spec.target_conn.clone()));
        // 目标库 = 目标会话的命名空间清单（库层 / schema 层按引擎定）。
        let ns_options: Vec<DbSelectOption> = self
            .sessions
            .get(selected_conn.as_ref())
            .map(|session| {
                let names = if namespace_is_database(session.kind) {
                    collect_namespaces(&session.databases, &session.schemas)
                } else {
                    session.schemas.clone()
                };
                names
                    .into_iter()
                    .map(|name| DbSelectOption {
                        value: SharedString::from(name.clone()),
                        label: SharedString::from(name),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let ns_signature = ns_options
            .iter()
            .map(|option| option.value.to_string())
            .collect::<Vec<_>>()
            .join("\u{1}");
        if state.ns_stamp.as_deref() != Some(ns_signature.as_str())
            && let Some(state) = self.compare.as_mut()
        {
            state.ns_stamp = Some(ns_signature);
            state.ns_select.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(ns_options), window, cx);
            });
        }
        // 选中值回填（下拉显示 ≠ spec 时以 spec 为准）。
        let Some(state) = self.compare.as_ref() else {
            return;
        };
        for (select, target) in [
            (
                &state.conn_select,
                Some(SharedString::from(state.spec.target_conn.clone())),
            ),
            (
                &state.ns_select,
                Some(SharedString::from(state.spec.target_ns.clone())),
            ),
        ] {
            let selected = select.read(cx).selected_value().cloned();
            if selected == target {
                continue;
            }
            let target = target.unwrap_or_default();
            select.update(cx, |select, cx| {
                select.set_selected_value(&target, window, cx);
            });
        }
    }
}

/// `SELECT COUNT(*)` 一问一答（比较与传输共用）。
fn count_rows(conn: &mut Connection, table: &TableRef) -> Result<u64, DbError> {
    let quote = conn.identifier_quote().unwrap_or('"');
    let qualified = quote_qualified(table, quote)?;
    let outcome = conn.execute(&format!("SELECT COUNT(*) FROM {qualified}"), 1)?;
    let count = outcome
        .result
        .and_then(|set| set.rows.into_iter().next())
        .and_then(|row| row.into_iter().next())
        .map(|value| match value {
            duck_db::Value::Int(value) => value.max(0) as u64,
            duck_db::Value::UInt(value) => value,
            value => value.render().parse::<u64>().unwrap_or(0),
        })
        .unwrap_or(0);
    Ok(count)
}

/// 拉一张表的主键值清单（多列主键按序拼接成一个键串），上限 `cap` 行。
fn fetch_pk_values(
    conn: &std::sync::Arc<std::sync::Mutex<Connection>>,
    table: &TableRef,
    pk: &[String],
    cap: usize,
) -> Result<std::collections::BTreeSet<String>, DbError> {
    let mut guard = conn.lock().expect("连接锁不应中毒");
    let quote = guard.identifier_quote().unwrap_or('"');
    let qualified = quote_qualified(table, quote)?;
    let columns = pk
        .iter()
        .filter_map(|column| duck_db::drivers::quote_identifier(column, quote).ok())
        .collect::<Vec<_>>()
        .join(", ");
    let outcome = guard.execute(&format!("SELECT {columns} FROM {qualified}"), cap)?;
    let keys = outcome
        .result
        .map(|set| {
            set.rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| match value {
                            duck_db::Value::Null => String::from("\u{0}NULL"),
                            value => value.render(),
                        })
                        .collect::<Vec<_>>()
                        .join("\u{1}")
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(keys)
}
