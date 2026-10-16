//! 数据传输标签：源库的表批量搬运到目标连接 + 库（dbx 的 Data Transfer 对位）。
//!
//! 真实后端 = 内核写路径：源表 `SELECT *`（行数有上限）读成展示文本，目标侧
//! `apply_batch` 按引擎的类型亲和落库。**不建表**：目标表不存在就跳过并明说
//! （跨引擎建表要搬一套类型系统，界面猜不如让用户先用结构编辑器建好）。
//! 列按**同名交集**搬运：目标多出的列落默认值，目标缺的列被丢弃并在结果里注明。

use super::*;

/// 单表传输的行数上限（与导出同一口径：超出的部分不是不能传，是不该悄悄全传）。
const TRANSFER_ROW_CAP: usize = 100_000;

/// `apply_batch` 的分块大小：一次几百行参数，既不超引擎的语句长度也不频繁往返。
const TRANSFER_CHUNK: usize = 500;

/// 一张表的传输结论。
#[derive(Debug, Clone)]
pub(super) enum TransferStatus {
    /// 还没轮到。
    Pending,
    /// 正在传。
    Transferring,
    /// 传完：落库行数（附读取行数与被丢弃列数，结果行里如实报）。
    Done {
        /// 实际插入的行数（各分块 `BatchOutcome::inserted` 之和）。
        inserted: u64,
        /// 读到的源行数。
        read: usize,
        /// 目标缺列而被丢弃的源列数。
        dropped: usize,
    },
    /// 跳过（目标表不存在等：不是错误，是前置条件不满足）。
    Skipped(String),
    /// 失败（引擎报错）。
    Failed(String),
}

/// 传输清单里的一张表。
pub(super) struct TransferRow {
    /// 源表引用（读数据用；目标表同名）。
    pub table: TableRef,
    /// 结论。
    pub status: TransferStatus,
}

/// 传输标签的全部状态（`Mode::Transfer` 的数据源，按标签存）。
pub(super) struct TransferState {
    /// 传输规格。
    pub spec: TransferSpec,
    /// 逐表结论（打开时按规格定死）。
    pub rows: Vec<TransferRow>,
    /// 是否还在传（逐表串行，一张接一张）。
    pub running: bool,
    /// 目标连接下拉。
    pub conn_select: Entity<DbSelectState>,
    /// 目标库下拉。
    pub ns_select: Entity<DbSelectState>,
    /// 两个下拉的选项签名。
    pub conn_stamp: Option<String>,
    pub ns_stamp: Option<String>,
}

impl DatabasePage {
    /// 打开（或复用）一张**数据传输标签**：源 = 打开菜单所在的连接 + 命名空间，
    /// 表清单 = 源目录里的全部表（比较页的「传输此表」传单张）。
    pub(super) fn open_transfer_tab(
        &mut self,
        conn_id: &str,
        namespace: &str,
        tables: Vec<TableRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
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
        let spec = TransferSpec {
            source_conn: conn_id.to_owned(),
            source_ns: namespace.to_owned(),
            tables,
            target_conn,
            target_ns,
        };
        let kind = TabKind::Transfer(spec.clone());
        if let Some(existing) =
            find_reusable_tab(&self.tabs, &self.tab_kind, &self.tab_conn, conn_id, &kind)
        {
            self.activate(&existing, window, cx);
            return;
        }
        let conn_select = db_select_state(window, cx);
        let ns_select = db_select_state(window, cx);
        self.spawn_tab_for(conn_id, kind, window, cx);
        self._subscriptions.push(cx.subscribe(
            &conn_select,
            |this, _, event: &SelectEvent<SearchableVec<DbSelectOption>>, cx| {
                let SelectEvent::Confirm(value) = event;
                if let Some(state) = this.transfer.as_mut() {
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
                if let Some(state) = this.transfer.as_mut() {
                    state.spec.target_ns =
                        value.as_ref().map(|v| v.to_string()).unwrap_or_default();
                    cx.notify();
                }
            },
        ));
        self.mode = Mode::Transfer(spec.clone());
        self.transfer = Some(TransferState {
            rows: spec
                .tables
                .iter()
                .map(|table| TransferRow {
                    table: table.clone(),
                    status: TransferStatus::Pending,
                })
                .collect(),
            spec,
            running: false,
            conn_select,
            ns_select,
            conn_stamp: None,
            ns_stamp: None,
        });
        cx.notify();
    }

    /// 开始传输：逐表串行（一张传完再传下一张），每张「读源 → 写目标」都在一个
    /// 后台闭包里完成（源锁在读完后立刻释放，不给目标侧的并发操作留死锁口子）。
    pub(super) fn start_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.transfer.as_ref() else {
            return;
        };
        if state.running {
            return;
        }
        if let Some(state) = self.transfer.as_mut() {
            for row in &mut state.rows {
                row.status = TransferStatus::Pending;
            }
            state.running = true;
        }
        self.status = tr(cx, "正在传输数据…").into();
        self.transfer_next(0, cx);
    }

    /// 传输第 `index` 张表（递归入口：回调里排下一张）。
    fn transfer_next(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(state) = self.transfer.as_ref() else {
            return;
        };
        if index >= state.rows.len() {
            if let Some(state) = self.transfer.as_mut() {
                state.running = false;
            }
            self.status = tr(cx, "传输结束。").into();
            cx.notify();
            return;
        }
        let Some(source_session) = self.sessions.get(&state.spec.source_conn) else {
            return;
        };
        let Some(target_session) = self.sessions.get(&state.spec.target_conn) else {
            return;
        };
        let source_conn = source_session.conn.clone();
        let target_conn = target_session.conn.clone();
        let (source_ns, target_ns) = (state.spec.source_ns.clone(), state.spec.target_ns.clone());
        let source_ref = TableRef {
            schema: Some(source_ns),
            name: state.rows[index].table.name.clone(),
        };
        let target_ref = TableRef {
            schema: Some(target_ns),
            name: state.rows[index].table.name.clone(),
        };
        if let Some(state) = self.transfer.as_mut() {
            state.rows[index].status = TransferStatus::Transferring;
        }
        cx.notify();
        // 「跳过」原因在主线程译好带进后台（tr 需要 &App，后台线程拿不到）。
        let skipped_label = tr(cx, "目标表不存在").to_owned();
        self.spawn_db(
            cx,
            move || -> (usize, Result<TransferOutcome, TransferFailure>) {
                let run = || -> Result<TransferOutcome, TransferFailure> {
                    // 读源：列清单 + 全部行（展示文本）。
                    let (columns, rows) = {
                        let mut guard = source_conn.lock().expect("连接锁不应中毒");
                        let quote = guard.kind().identifier_quote().unwrap_or('"');
                        let qualified =
                            quote_qualified(&source_ref, quote).map_err(TransferFailure::Error)?;
                        let columns = guard
                            .table_columns(&source_ref)
                            .map_err(TransferFailure::Error)?;
                        let outcome = guard
                            .execute(&format!("SELECT * FROM {qualified}"), TRANSFER_ROW_CAP)
                            .map_err(TransferFailure::Error)?;
                        let rows = outcome.result.unwrap_or_default().rows;
                        (columns, rows)
                    }; // 源锁在这里释放：写目标时不再持有它，两个方向的传输不会互锁。
                    // 写目标：目标表必须已存在（不建表），列按同名交集搬运。
                    let mut guard = target_conn.lock().expect("连接锁不应中毒");
                    let target_columns = guard
                        .table_columns(&target_ref)
                        .map_err(|_| TransferFailure::Skipped(skipped_label))?;
                    let common: Vec<String> = columns
                        .iter()
                        .map(|column| column.name.clone())
                        .filter(|name| target_columns.iter().any(|column| column.name == *name))
                        .collect();
                    let dropped = columns.len() - common.len();
                    let indexes: Vec<usize> = columns
                        .iter()
                        .enumerate()
                        .filter(|(_, column)| common.contains(&column.name))
                        .map(|(index, _)| index)
                        .collect();
                    let mut inserted = 0u64;
                    for chunk_start in (0..rows.len()).step_by(TRANSFER_CHUNK) {
                        let chunk =
                            &rows[chunk_start..(chunk_start + TRANSFER_CHUNK).min(rows.len())];
                        let inserts: Vec<InsertRow> = chunk
                            .iter()
                            .map(|row| InsertRow {
                                columns: common.clone(),
                                values: indexes
                                    .iter()
                                    .map(|index| match &row[*index] {
                                        duck_db::Value::Null => None,
                                        value => Some(value.render()),
                                    })
                                    .collect(),
                            })
                            .collect();
                        if inserts.is_empty() {
                            break;
                        }
                        let outcome = guard
                            .apply_batch(&target_ref, &[], &inserts)
                            .map_err(TransferFailure::Error)?;
                        inserted += outcome.inserted;
                    }
                    Ok(TransferOutcome {
                        inserted,
                        rows_read: rows.len(),
                        dropped_columns: dropped,
                    })
                };
                (index, run())
            },
            move |this,
                  (index, result): (usize, Result<TransferOutcome, TransferFailure>),
                  _window,
                  cx| {
                if let Some(state) = this.transfer.as_mut()
                    && let Some(row) = state.rows.get_mut(index)
                {
                    row.status = match result {
                        Ok(outcome) => TransferStatus::Done {
                            inserted: outcome.inserted,
                            read: outcome.rows_read,
                            dropped: outcome.dropped_columns,
                        },
                        Err(TransferFailure::Skipped(reason)) => TransferStatus::Skipped(reason),
                        Err(TransferFailure::Error(error)) => {
                            TransferStatus::Failed(error.to_string())
                        }
                    };
                }
                this.transfer_next(index + 1, cx);
            },
        );
    }

    /// 传输页：规格条（源 → 目标 + 开跑）+ 逐表结果清单。
    pub(super) fn render_transfer_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(state) = self.transfer.as_ref() else {
            return div().into_any_element();
        };
        let spec = &state.spec;
        let source_label = self
            .connections
            .iter()
            .find(|cfg| cfg.id == spec.source_conn)
            .map(|cfg| cfg.display_name())
            .unwrap_or_else(|| spec.source_conn.clone());
        let done = state
            .rows
            .iter()
            .filter(|row| {
                !matches!(
                    row.status,
                    TransferStatus::Pending | TransferStatus::Transferring
                )
            })
            .count();
        let transferred: u64 = state
            .rows
            .iter()
            .filter_map(|row| match &row.status {
                TransferStatus::Done { inserted, .. } => Some(*inserted),
                _ => None,
            })
            .sum();
        let can_run = !spec.target_conn.is_empty()
            && !spec.target_ns.is_empty()
            && !state.running
            && !self.busy;
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
                Icon::new(DuckIcon::Upload)
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
            .child(neutral_tag(
                SharedString::from(trf(
                    cx,
                    "{}/{} 张",
                    &[&done.to_string(), &state.rows.len().to_string()],
                )),
                cx,
            ))
            .child(if state.running {
                neutral_tag(SharedString::from(tr(cx, "传输中…")), cx).into_any_element()
            } else if transferred > 0 {
                neutral_tag(
                    SharedString::from(trf(cx, "已传输 {} 行", &[&transferred.to_string()])),
                    cx,
                )
                .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(
                button(
                    "db-transfer-run",
                    tr(cx, "开始传输"),
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
                            this.start_transfer(cx);
                        });
                    }
                }),
            );
        let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
        for row in &state.rows {
            list = list.child(self.render_transfer_row(row, cx));
        }
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .gap(tokens.spacing.sm)
            .bg(tokens.colors.background)
            .p(tokens.spacing.sm)
            .child(bar)
            .child(
                div()
                    .id("db-transfer-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
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

    /// 结果清单的一行：表名 + 状态 chip + 说明。
    fn render_transfer_row(&self, row: &TransferRow, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let (chip, detail) = match &row.status {
            TransferStatus::Pending => (
                neutral_tag(SharedString::from(tr(cx, "待传输")), cx),
                div().into_any_element(),
            ),
            TransferStatus::Transferring => (
                accent_tag(SharedString::from(tr(cx, "传输中")), cx),
                div().into_any_element(),
            ),
            TransferStatus::Done {
                inserted,
                read,
                dropped,
            } => (
                neutral_tag(SharedString::from(tr(cx, "完成")), cx),
                meta_text(
                    SharedString::from(if *dropped > 0 {
                        trf(
                            cx,
                            "读 {} 行 · 已插入 {} 行（丢弃 {} 列）",
                            &[
                                &read.to_string(),
                                &inserted.to_string(),
                                &dropped.to_string(),
                            ],
                        )
                    } else {
                        trf(
                            cx,
                            "读 {} 行 · 已插入 {} 行",
                            &[&read.to_string(), &inserted.to_string()],
                        )
                    }),
                    cx,
                )
                .into_any_element(),
            ),
            TransferStatus::Skipped(reason) => (
                neutral_tag(SharedString::from(tr(cx, "跳过")), cx),
                meta_text(SharedString::from(reason.clone()), cx).into_any_element(),
            ),
            TransferStatus::Failed(reason) => (
                accent_tag(SharedString::from(tr(cx, "失败")), cx),
                meta_text(SharedString::from(reason.clone()), cx).into_any_element(),
            ),
        };
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
                    .child(SharedString::from(row.table.name.clone())),
            )
            .child(chip)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(tokens.typography.xs.size)
                    .child(detail),
            )
            .into_any_element()
    }
    /// 传输页的两个目标下拉：选项与选中值按签名同步（渲染入口调用）。
    pub(super) fn sync_transfer_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.transfer.as_ref() else {
            return;
        };
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
            && let Some(state) = self.transfer.as_mut()
        {
            state.conn_stamp = Some(conn_signature);
            state.conn_select.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(conn_options), window, cx);
            });
        }
        let Some(state) = self.transfer.as_ref() else {
            return;
        };
        let selected_conn = state.conn_select.read(cx).selected_value().cloned();
        let selected_conn =
            selected_conn.unwrap_or_else(|| SharedString::from(state.spec.target_conn.clone()));
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
            && let Some(state) = self.transfer.as_mut()
        {
            state.ns_stamp = Some(ns_signature);
            state.ns_select.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(ns_options), window, cx);
            });
        }
        let Some(state) = self.transfer.as_ref() else {
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

/// 单表传输的后台产物。
struct TransferOutcome {
    /// 实际插入行数。
    inserted: u64,
    /// 读到的源行数（上限内）。
    rows_read: usize,
    /// 因目标缺列被丢弃的列数。
    dropped_columns: usize,
}

/// 单表传输的失败：跳过（前置条件不满足）或引擎错误。
enum TransferFailure {
    Skipped(String),
    Error(DbError),
}
