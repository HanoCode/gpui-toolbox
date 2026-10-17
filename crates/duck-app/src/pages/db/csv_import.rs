//! CSV 导入向导：**先看清再落库**。
//!
//! 原来的导入是「选完文件立刻 INSERT」——首行当表头、空串当空串、列名对不上就整体失败，
//! 用户在数据进库之前看不到任何东西。向导把这一步拆成三段：
//!
//! 1. **预览**：前 50 行，够看清分隔符、编码、前几列的内容；
//! 2. **策略**：首行是不是表头、空串算 NULL 还是空串 —— 这两条几乎每个真实文件都要调；
//! 3. **对齐结论**：每列落到表里哪一列、哪几列会被忽略、一共要写多少行（写完给回执）。
//!
//! 口径：**只按列名对齐**，不提供逐列手动映射。理由是「CSV 列序 ≠ 表列序」时按名字对
//! 才是对的，而按名字对不上时（改了表头 / 换了语言）手动映射也只是把同一件事再做一遍；
//! 与其加二十个下拉，不如把「哪几列被忽略」在预览里写清楚，让用户回去改表头。
//! 表里不在 CSV 里的列交给数据库默认值（与表节点右键「导入数据」同一条规矩）。
//!
//! 为什么是**页面内浮层**而不是 `window.open_dialog`：向导里的勾选会改动预览（列名、
//! 对齐结论都要跟着变），而 dialog 的内容闭包由窗口层驱动重绘，页面 `cx.notify()`
//! 并不保证它会重跑。浮层挂在页面根上，状态一变就重绘，行为可预期。
//!
//! 数据只在**执行那一刻**进库，且仍是单事务（`Connection::insert_rows`）：中途失败
//! 整体回滚，不会留下半份数据。

use super::*;

/// 向导浮层（页面字段 `csv_import`；`Some` = 开着）。
pub(super) struct CsvImport {
    /// 源文件（标题里显示名字）。
    path: std::path::PathBuf,
    /// 目标连接 id。
    conn: String,
    /// 目标表。
    table: TableRef,
    /// `parse_csv` 的原样输出（第一行是不是表头取决于 `has_header`）。
    raw: Vec<Vec<Option<String>>>,
    /// 表里真实的列（打开时读一次；空 = 读不到，预览里所有列都会标「忽略」）。
    table_columns: Vec<String>,
    /// 第一行是不是表头。
    has_header: bool,
    /// 空串是否按 NULL 写入。
    empty_as_null: bool,
    /// 当前策略的结论（切换选项时重算）。
    plan: CsvPlan,
}

/// 一次导入的**结论**：列怎么对、写多少行、能不能写。
#[derive(Default)]
struct CsvPlan {
    /// CSV 侧的列名（有表头取首行，否则 `列 1..n`）。
    columns: Vec<String>,
    /// 每一列落到表里的哪一列（`None` = 忽略）。
    mapping: Vec<Option<String>>,
    /// 实际要写的数据行数。
    data_rows: usize,
    /// 命中的列数。
    hit: usize,
    /// 被忽略的列数。
    skipped: usize,
    /// 致命问题；`Some` 时「开始导入」禁用。
    issue: Option<CsvIssue>,
}

/// 「不能导入」的两种原因（渲染时按当前语言取词）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CsvIssue {
    /// 一列都没对上表里的列名。
    NoColumnMatch,
    /// 表头有重复列名（对齐会指向同一列）。
    DuplicateHeader,
}

impl CsvIssue {
    fn label(self, cx: &App) -> SharedString {
        match self {
            Self::NoColumnMatch => tr(cx, "CSV 的表头一列都没对上表的列名，请先改表头。").into(),
            Self::DuplicateHeader => tr(cx, "表头有重复列名，请先改成唯一列名。").into(),
        }
    }
}

/// 预览每列宽度（像素；宽表靠横向滚动，不压缩文字）。
const CSV_PREVIEW_COL_WIDTH: f32 = 132.;
/// 预览最多显示多少行数据。
const CSV_PREVIEW_ROWS: usize = 50;
/// 预览区最大高度。
const CSV_PREVIEW_MAX_HEIGHT: f32 = 280.;
/// 卡片宽度（比命令面板宽：要摆下预览表）。
const CSV_DIALOG_WIDTH: f32 = 720.;

impl DatabasePage {
    /// 打开 CSV 导入向导（`import_csv` 选完文件之后进来）。
    ///
    /// 解析与「表有哪些列」都在这里同步做完：文件已经选好，点开面板就该看到预览。
    /// 读的是文本文件（`read_to_string`），几十 MB 的 CSV 会占用同量级内存 —— 与
    /// 「导入本来就要把行装进一个事务」同量级，不额外放大。
    ///
    /// 没有 `&mut Window`：调用点在文件面板的异步回调里。失败只落状态栏 —— 刚选过
    /// 的路径读不出来，几乎只有「文件被删 / 没有权限」两种，够定位。
    pub(super) fn open_csv_import(
        &mut self,
        conn: String,
        table: TableRef,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let raw = match std::fs::read_to_string(&path) {
            Ok(text) => duck_db::parse_csv(&text),
            Err(error) => {
                self.status = trf(cx, "读取文件失败：{}", &[&error.to_string()]).into();
                cx.notify();
                return;
            }
        };
        if raw.is_empty() {
            self.status = tr(cx, "文件是空的，没有可导入的行。").into();
            cx.notify();
            return;
        }
        // 表列清单：读不到就退化成空清单（预览里所有列都会标「忽略」），而不是把
        // 整个向导关掉 —— 用户至少能看见文件解析得对不对。
        let table_columns = self
            .sessions
            .get(&conn)
            .and_then(|session| {
                let mut guard = session.conn.lock().expect("连接锁不应中毒");
                guard.table_columns(&table).ok().map(|columns| {
                    columns
                        .into_iter()
                        .map(|column| column.name)
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        let mut draft = CsvImport {
            path,
            conn,
            table,
            raw,
            table_columns,
            has_header: true,
            empty_as_null: true,
            plan: CsvPlan::default(),
        };
        draft.replan(cx);
        self.csv_import = Some(draft);
        cx.notify();
    }

    /// 关闭向导（不导入）。
    pub(super) fn close_csv_import(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.csv_import.take().is_some() {
            cx.notify();
        }
    }

    /// 按当前策略真正落库。
    pub(super) fn commit_csv_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.csv_import.as_ref() else {
            return;
        };
        if draft.plan.issue.is_some() {
            return;
        }
        let table = draft.table.clone();
        let columns = draft.plan.columns.clone();
        let rows = draft.projected_rows();
        let row_count = rows.len();
        let Some(session) = self.sessions.get(&draft.conn) else {
            return;
        };
        let conn = session.conn.clone();
        let label = table.display();
        self.close_csv_import(window, cx);
        self.status = trf(cx, "正在导入 {} 行…", &[&row_count.to_string()]).into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<u64, String> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard
                    .insert_rows(&table, &columns, &rows)
                    .map(|count| count as u64)
                    .map_err(|error| error.to_string())
            },
            move |this, result, window, cx| match result {
                Ok(count) => {
                    let detail = trf(cx, "已导入 {} 行到 {}。", &[&count.to_string(), &label]);
                    this.status = detail.clone().into();
                    notify::success(detail, window, cx);
                    // 行数变了：表级元信息（行数）跟着重取，表格重载。
                    this.table_meta_for = None;
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "导入失败：{}", &[&error]).into();
                    notify::error(trf(cx, "导入失败：{}", &[&error]), window, cx);
                }
            },
        );
    }

    /// 向导浮层。没开时返回 `None`。
    pub(super) fn render_csv_import(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let draft = self.csv_import.as_ref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let plan = &draft.plan;
        let file_name = draft
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| draft.path.display().to_string());
        let target = trf(cx, "导入到 {}", &[&draft.table.display()]);

        // 勾选项：一个可点的胶囊（勾上 = 主色边框 + 深一档底色）。
        let toggle_row = |id: &'static str, label: &'static str, on: bool| -> AnyElement {
            let view = self.view.clone();
            let tip = if on {
                tr(cx, "已开启（点击关闭）")
            } else {
                tr(cx, "已关闭（点击开启）")
            };
            let border = if on {
                tokens.colors.primary
            } else {
                extended.colors.hairline
            };
            let icon = if on {
                DuckIcon::CheckboxCheck
            } else {
                DuckIcon::Square
            };
            div()
                .id(SharedString::from(id))
                .flex()
                .items_center()
                .gap(tokens.spacing.xs)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xxs)
                .rounded(tokens.radius.md)
                .border_1()
                .border_color(border)
                .cursor_pointer()
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        if let Some(draft) = this.csv_import.as_mut() {
                            if id == "db-csv-header" {
                                draft.has_header = !draft.has_header;
                            } else {
                                draft.empty_as_null = !draft.empty_as_null;
                            }
                            draft.replan(cx);
                        }
                        cx.notify();
                    });
                })
                .child(Icon::new(icon).size(extended.icon.sm).text_color(border))
                .child(
                    div()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.foreground)
                        .child(label),
                )
                .into_any_element()
        };

        // 表头：列名 + 「→ 表列」或「（忽略）」。
        let header_cells = plan.columns.iter().enumerate().map(|(index, name)| {
            let mapped = plan.mapping.get(index).and_then(|slot| slot.clone());
            let color = if mapped.is_some() {
                tokens.colors.foreground
            } else {
                extended.colors.text_tertiary
            };
            let hint: SharedString = match &mapped {
                Some(column) => SharedString::from(format!("→ {column}")),
                None => tr(cx, "（忽略）").into(),
            };
            v_flex()
                .w(px(CSV_PREVIEW_COL_WIDTH))
                .flex_none()
                .gap(tokens.spacing.xxs)
                .px(tokens.spacing.xs)
                .py(tokens.spacing.xxs)
                .child(
                    div()
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .text_color(color)
                        .child(name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .text_color(extended.colors.text_tertiary)
                        .child(hint),
                )
                .into_any_element()
        });
        let preview_rows = draft.raw_rows().take(CSV_PREVIEW_ROWS).map(|row| {
            let cells = plan.columns.iter().enumerate().map(|(index, _)| {
                let text = row
                    .get(index)
                    .and_then(|cell| cell.clone())
                    .unwrap_or_default();
                div()
                    .w(px(CSV_PREVIEW_COL_WIDTH))
                    .flex_none()
                    .truncate()
                    .px(tokens.spacing.xs)
                    .py(tokens.spacing.xxs)
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(if text.is_empty() {
                        tr(cx, "（空）").to_owned()
                    } else {
                        text
                    })
                    .into_any_element()
            });
            h_flex()
                .flex_none()
                .border_t_1()
                .border_color(extended.colors.hairline)
                .children(cells)
                .into_any_element()
        });

        let (summary_text, summary_color): (SharedString, Hsla) = match plan.issue {
            Some(issue) => (issue.label(cx), extended.colors.status_failed),
            None => (
                trf(
                    cx,
                    "将导入 {} 行 · 命中 {} 列 · 忽略 {} 列",
                    &[
                        &plan.data_rows.to_string(),
                        &plan.hit.to_string(),
                        &plan.skipped.to_string(),
                    ],
                )
                .into(),
                extended.colors.text_tertiary,
            ),
        };

        let cancel_view = self.view.clone();
        let run_view = self.view.clone();
        let run_enabled = plan.issue.is_none();
        let card = v_flex()
            .w(px(CSV_DIALOG_WIDTH))
            .max_h(px(CSV_DIALOG_WIDTH * 0.9))
            .gap(tokens.spacing.sm)
            .p(tokens.spacing.lg)
            .rounded(theme.components().dialog_radius)
            .border_1()
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.surface)
            .shadow_lg()
            // 卡片上的按下不冒泡到遮罩（否则点内容就关面板）。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                v_flex()
                    .gap(tokens.spacing.xxs)
                    .child(
                        div()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .font_weight(FontWeight::MEDIUM)
                            .child(file_name),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(extended.colors.text_tertiary)
                            .child(target),
                    ),
            )
            .child(
                h_flex()
                    .gap(tokens.spacing.xs)
                    .child(toggle_row(
                        "db-csv-header",
                        tr(cx, "首行是表头"),
                        draft.has_header,
                    ))
                    .child(toggle_row(
                        "db-csv-null",
                        tr(cx, "空串按 NULL"),
                        draft.empty_as_null,
                    )),
            )
            .child(
                v_flex()
                    .id("db-csv-preview")
                    .max_h(px(CSV_PREVIEW_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(extended.colors.hairline)
                    .rounded(tokens.radius.md)
                    .child(h_flex().flex_none().children(header_cells))
                    .children(preview_rows),
            )
            .child(
                div()
                    .text_size(tokens.typography.xs.size)
                    .text_color(summary_color)
                    .child(summary_text),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap(tokens.spacing.xs)
                    .child(
                        button(
                            "db-csv-cancel",
                            tr(cx, "取消"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(move |_, window, cx| {
                            let _ = cancel_view
                                .update(cx, |this, cx| this.close_csv_import(window, cx));
                        }),
                    )
                    .child(
                        button("db-csv-run", tr(cx, "开始导入"), ButtonVariant::Primary, cx)
                            .disabled(!run_enabled)
                            .on_click(move |_, window, cx| {
                                let _ = run_view
                                    .update(cx, |this, cx| this.commit_csv_import(window, cx));
                            }),
                    ),
            );

        let dismiss_view = self.view.clone();
        Some(
            div()
                .id("db-csv-import")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_center()
                .bg(Hsla {
                    h: 0.,
                    s: 0.,
                    l: 0.,
                    a: extended.opacity.scrim,
                })
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    let _ = dismiss_view.update(cx, |this, cx| this.close_csv_import(window, cx));
                })
                .on_key_down({
                    let key_view = self.view.clone();
                    move |event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            let _ =
                                key_view.update(cx, |this, cx| this.close_csv_import(window, cx));
                        }
                    }
                })
                .child(card)
                .into_any_element(),
        )
    }
}

impl CsvImport {
    /// 按当前选项重算列名、对齐与行数。
    fn replan(&mut self, cx: &App) {
        let width = self.raw.first().map(|row| row.len()).unwrap_or_default();
        let (columns, skip) = if self.has_header {
            let header = self.raw.first().cloned().unwrap_or_default();
            let names: Vec<String> = (0..width)
                .map(
                    |index| match header.get(index).and_then(|cell| cell.as_deref()) {
                        Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
                        _ => unnamed_column(index, cx),
                    },
                )
                .collect();
            (names, 1)
        } else {
            (
                (0..width).map(|index| unnamed_column(index, cx)).collect(),
                0,
            )
        };
        let mapping: Vec<Option<String>> = columns
            .iter()
            .map(|name| {
                self.table_columns
                    .iter()
                    .find(|column| column.as_str() == name.as_str())
                    .cloned()
            })
            .collect();
        let hit = mapping.iter().filter(|slot| slot.is_some()).count();
        let duplicate = {
            let mut seen = std::collections::BTreeSet::new();
            !columns.iter().all(|name| seen.insert(name.clone()))
        };
        let issue = if hit == 0 {
            Some(CsvIssue::NoColumnMatch)
        } else if duplicate {
            Some(CsvIssue::DuplicateHeader)
        } else {
            None
        };
        self.plan = CsvPlan {
            data_rows: self.raw.len().saturating_sub(skip),
            columns,
            mapping,
            hit,
            skipped: width.saturating_sub(hit),
            issue,
        };
    }

    /// 数据行（跳过表头）。
    fn raw_rows(&self) -> impl Iterator<Item = &Vec<Option<String>>> {
        let skip = usize::from(self.has_header);
        self.raw.iter().skip(skip)
    }

    /// 按「命中的列」投影之后的行 —— 直接喂给 `insert_rows`。
    fn projected_rows(&self) -> Vec<Vec<Option<String>>> {
        let keep: Vec<usize> = self
            .plan
            .mapping
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_some())
            .map(|(index, _)| index)
            .collect();
        self.raw_rows()
            .map(|row| {
                keep.iter()
                    .map(|index| {
                        let cell = row.get(*index).and_then(|value| value.clone());
                        match cell {
                            // 空串按 NULL：CSV 里「没有值」与「空字符串」本来就不可分，
                            // 统一按 NULL 更贴近「这个格子没有值」的原意。
                            Some(value) if value.is_empty() && self.empty_as_null => None,
                            other => other,
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

/// 无表头时第 `index` 列的名字（`列 1` 起；走 `trf` 所以英文界面是 `Column 1`）。
fn unnamed_column(index: usize, cx: &App) -> String {
    trf(cx, "列 {}", &[&(index + 1).to_string()])
}
