use super::*;

/// 「导出数据」单次读取的行上限：一条 `SELECT *` 一次拿走，超出部分靠 WHERE
/// 缩小范围（流式分块导出是另一个量级的工程，不在这一轮）。
const EXPORT_ROW_CAP: usize = 100_000;

/// SQL 类型名 → Rust 类型草稿（生成模型用；草稿级映射，不是类型系统）。
fn rust_type_of(type_name: &str) -> &'static str {
    let name = type_name.to_lowercase();
    if name.contains("bool") {
        "bool"
    } else if name.contains("bigint")
        || name.contains("int")
        || name.contains("serial")
        || name.contains("year")
    {
        "i64"
    } else if name.contains("float")
        || name.contains("double")
        || name.contains("real")
        || name.contains("decimal")
        || name.contains("numeric")
    {
        "f64"
    } else {
        "String"
    }
}

/// 表结构缓存与 `tree_open` 展开键共用的定位串（`连接 id:限定名`）。
///
/// `tree_open` 的键带 `tbl:` / `tblg:` 前缀区分层级，缓存的键就是这个裸定位串
/// （见 [`DatabasePage::load_table_tree`]）。
fn table_tree_key(conn_id: &str, table: &TableRef) -> String {
    format!("{conn_id}:{}", table.display())
}

/// 表结构分组下的**一条子行**（列 / 索引 / 外键 / 触发器成员）：名称 + 尾部元信息。
///
/// 纯展示、不做成按钮：这些行没有可打开的编辑器，装成可点就是假入口（界面不许
/// 有演示项）。行高与 `sidebar_navigation_button` 对齐（`density.nav_row`），视觉上
/// 才像同一棵树里的兄弟。
fn structure_leaf_row(
    id: String,
    label: String,
    detail: Option<String>,
    icon: DuckIcon,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    h_flex()
        .id(SharedString::from(id))
        .w_full()
        .h(theme.density().nav_row)
        .px(tokens.spacing.sm)
        .gap(tokens.spacing.xs)
        .items_center()
        .child(
            Icon::new(icon)
                .size(extended.icon.sm)
                .text_color(tokens.colors.muted_foreground),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.sm.size)
                .text_color(tokens.colors.foreground)
                .child(label),
        )
        .children(detail.map(|detail| meta_text(detail, cx).min_w_0().truncate()))
        .into_any_element()
}

impl DatabasePage {
    /// 选中一张表并翻到第一页。
    pub(super) fn open_table(
        &mut self,
        table: TableRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_session().is_none() {
            return;
        }
        // 换表清掉上一张表的搜索词（同 `activate`：旧词对新表几乎必然失效）。
        self.clear_result_search(window, cx);
        self.selected_table = Some(table);
        self.table_page = 0;
        self.table_sort = None;
        self.mode = Mode::Table;
        self.selected_row = None;
        self.row_editor = None;
        // 换表：勾选、暂存改动、「批量编辑的目标列」都作废（表与列集合都换了）。
        self.checked_rows.clear();
        self.batch_column = None;
        self.clear_pending();
        // 换表先把上一张表的元信息清干净，免得右栏在新数据回来前展示旧索引；
        // 同时让 `table_meta_for` 失配，强制本次重新取一遍（而不是沿用旧缓存）。
        self.table_indexes.clear();
        self.table_row_count = None;
        self.table_ddl = None;
        self.table_meta_for = None;
        // 「打开一张表」是这十几个读表入口里**唯一**值得进审计流水的用户动作：
        // 翻页 / 排序 / 过滤是同一个浏览动作的延续，写后重载是写的余波 —— 各记一条
        // 只会把「谁在什么时候看了哪张表」淹掉。
        self.browse_open_pending = true;
        self.load_table_page(cx);
    }

    /// 加载当前表页（后台）。
    pub(super) fn load_table_page(&mut self, cx: &mut Context<Self>) {
        // 一次性标记**在这里**就取走（而不是在回调里）：下面两个早退分支只要发生，
        // 这一次的「打开表」就不成立，标记不能留给下一次读表去冒领。
        let announce = std::mem::take(&mut self.browse_open_pending);
        let Some(session) = self.active_session() else {
            return;
        };
        let Some(table) = self.selected_table.clone() else {
            return;
        };
        let page = self.table_page;
        let sort = self.table_sort.clone();
        // 过滤条件随任务带走：它是**服务器侧** WHERE，与排序同样按当前页重新发起。
        let filters = self.table_filters.clone();
        let filtered = !filters.is_empty();
        let session_id = self.active.clone().unwrap_or_default();
        // 回调里还要读一次会话 id（审计的 scope），而 `session_id` 本体要交给守卫。
        let audit_scope = session_id.clone();
        let conn = session.conn.clone();
        // 表级元信息（索引 / 行数）只在**换表**时取一次：翻页时重复 `COUNT(*)`
        // 在大表上是纯浪费（`table_meta_for` 记住已经为哪张表取过）。
        let wants_meta = self.table_meta_for.as_ref() != Some(&table);
        let table_for_work = table.clone();
        // 读表也是「可能很久」的路径（大表 + 远端库），同样给取消与超时。
        let cancel = conn.lock().expect("连接锁不应中毒").cancel_handle();
        let timeout = self.effective_timeout();
        // 每页行数在发起这次读取时读一次（设置页可改，翻页时自然跟着变）。
        let page_size = browse_page_size(cx);
        self.status = tr(cx, "正在读取表数据…").into();
        cx.notify();
        self.spawn_db_guarded(
            cx,
            session_id,
            cancel,
            timeout,
            move || {
                // 工作闭包拿走一份克隆：`table` 本体要留给下面 `done` 里的元信息记账。
                let table = table_for_work;
                let mut guard = conn.lock().expect("连接锁不应中毒");
                let pk = guard.table_primary_key(&table).unwrap_or_default();
                let order = page_order(sort.as_ref(), &pk);
                let ddl = guard.table_ddl(&table).ok();
                let meta = wants_meta.then(|| {
                    // 元信息失败不该拖垮表页：各槽位独立兜底（索引 / 列 / 外键 /
                    // 触发器落空清单，行数 `.ok()`——视图 / 不可数对象给不出数字）。
                    TableMeta {
                        indexes: guard.table_indexes(&table).unwrap_or_default(),
                        row_count: guard.table_row_count(&table).ok(),
                        columns: guard.table_columns(&table).unwrap_or_default(),
                        foreign_keys: guard.table_foreign_keys(&table).unwrap_or_default(),
                        triggers: guard.table_triggers(&table).unwrap_or_default(),
                    }
                });
                guard
                    .table_page(
                        &table,
                        page_size,
                        page as u64 * page_size as u64,
                        &order,
                        &filters,
                    )
                    .map(|result| (result, pk, ddl, meta))
            },
            move |this, result, window, cx| match result {
                Ok((mut result, pk, ddl, meta)) => {
                    let rows = result.row_count();
                    // 列注释（表头第三行 / 悬停详情卡）与**类型原文**都从**表内省**回填：
                    // 查询结果的列元数据来自游标——不知道注释，类型也常常只剩裸名
                    // （MySQL 游标给 `VARCHAR`，没有 `(50)`）；同一张表的完整类型
                    // （`varchar(50)`，带长度修饰）只有 `table_columns` 有。按列名对齐
                    // ——`SELECT *` 展开与内省同序，别名 / 表达式对不上就留原样
                    //（宁可少给，不猜）。翻页时本次不重取内省（`wants_meta` 只在换表
                    // 时真），回落到**页面上留存的**那份列清单——同一张表翻页后
                    // 表头不该一会儿有长度一会儿没有。
                    let introspected = meta
                        .as_ref()
                        .map(|meta| &meta.columns)
                        .or(Some(&this.table_columns));
                    if let Some(introspected) = introspected {
                        let by_name: HashMap<&str, &duck_db::Column> = introspected
                            .iter()
                            .map(|column| (column.name.as_str(), column))
                            .collect();
                        for column in &mut result.columns {
                            let Some(full) = by_name.get(column.name.as_str()) else {
                                continue;
                            };
                            column.comment = full.comment.clone();
                            if !full.type_name.is_empty() {
                                column.type_name = full.type_name.clone();
                            }
                        }
                    }
                    // 用户刚打开一张表：记一条「浏览表」。翻页 / 排序 / 写后重载都不记
                    //（见 `browse_open_pending` 的说明）—— 否则一次写会连着记两条，
                    // 流水里全是页号在变、没有信息量的行。
                    if announce {
                        let detail = trf(cx, "浏览表 {}", &[&table.display()]);
                        crate::audit::record_event(
                            cx,
                            crate::audit::AuditKind::Query,
                            crate::audit::action::BROWSE,
                            crate::audit::AuditOutcome::Ok,
                            &audit_scope,
                            detail,
                        );
                    }
                    // 列宽没变就保留横向位置：翻页 / 排序不该把人拽回最左列。
                    // 判据交给数据表组件（`set_model` 比**列名序列**）：这比原先
                    // 「估宽向量不等」更贴近本意 —— 同一列集换了数据时不该回零。
                    this.table_rows = Some(result);
                    this.table_pk = pk;
                    this.table_ddl = ddl;
                    if let Some(meta) = meta {
                        this.table_indexes = meta.indexes;
                        this.table_row_count = meta.row_count;
                        this.table_columns = meta.columns;
                        this.table_foreign_keys = meta.foreign_keys;
                        this.table_triggers = meta.triggers;
                        this.table_meta_for = Some(table.clone());
                    }
                    this.row_editor = None;
                    this.selected_row = None;
                    // 换了结果集：列头的本地筛选与格式化跟着旧列名走已经没有意义，
                    // 一并清掉（列级状态只对「看着的这份结果」负责）。
                    this.local_filters.clear();
                    this.column_formats.clear();
                    // 翻页 / 换表 / 刷新换了结果集：勾选按旧下标记，必须清掉。
                    this.checked_rows.clear();
                    // 行数口径要如实：过滤生效时「本页 N 行」是**过滤后**的 N，
                    // 不加后缀会让人以为整表只有这么多行。
                    this.status = if filtered {
                        trf(
                            cx,
                            "第 {} 页 · 本页 {} 行 · 已过滤",
                            &[&(this.table_page + 1).to_string(), &rows.to_string()],
                        )
                    } else {
                        trf(
                            cx,
                            "第 {} 页 · 本页 {} 行",
                            &[&(this.table_page + 1).to_string(), &rows.to_string()],
                        )
                    }
                    .into();
                    // 新一页装好了：把结果推给数据表组件（表模型 / 行序 / 藏列）。
                    // 表浏览是**服务器侧分页**，整页即一页，所以展示行序就是全量行。
                    this.sync_data_table(cx);
                }
                Err(error) => {
                    if matches!(error, DbError::Cancelled(_)) {
                        let text = this.cancelled_status(cx);
                        this.status = text.clone();
                        notify::info(text, window, cx);
                        return;
                    }
                    this.status = trf(cx, "读取失败：{}", &[&error.to_string()]).into();
                    // 读失败**每次**都记，不看 `announce`：它罕见，而且是「这个页面为什么
                    // 是空的」的第一手线索。取消（上面那条分支）反而**不记** —— 翻页点快了
                    // 就会产生一次取消，那是导航噪声不是事件（对照组：SQL 执行的取消会记，
                    // 因为「一条跑了一半的写语句被中断」是真事件）。
                    let what = trf(cx, "浏览表 {}", &[&table.display()]);
                    let detail = trf(cx, "{}失败：{}", &[&what, &error.to_string()]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Query,
                        crate::audit::action::BROWSE,
                        crate::audit::AuditOutcome::Failed,
                        &audit_scope,
                        detail,
                    );
                    notify::error(
                        trf(cx, "读取表数据失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 点列头：升序 → 降序 → 取消，然后重新取这一页。
    pub(super) fn toggle_table_sort(&mut self, column: &str, cx: &mut Context<Self>) {
        if self.mode != Mode::Table {
            return;
        }
        self.table_sort = SortSpec::next(self.table_sort.as_ref(), column);
        // 排序换了，行号的含义也换了：勾选与选中行都作废。
        self.checked_rows.clear();
        self.selected_row = None;
        self.row_editor = None;
        // 回第一页：排序变了还停在第 5 页没有意义。
        self.table_page = 0;
        self.load_table_page(cx);
    }

    /// 右键列头直接定序（升 / 降各一项），与点列头的三态循环并存。
    ///
    /// 作废集合与翻页口径与 [`Self::toggle_table_sort`] 完全一致——两个入口，
    /// 一份真相。
    pub(super) fn sort_table_by(&mut self, column: &str, descending: bool, cx: &mut Context<Self>) {
        if self.mode != Mode::Table {
            return;
        }
        self.table_sort = Some(SortSpec {
            column: column.to_owned(),
            descending,
        });
        self.checked_rows.clear();
        self.selected_row = None;
        self.row_editor = None;
        self.table_page = 0;
        self.load_table_page(cx);
    }

    /// 右键列头把过滤条的「列」预选成这一列（**草稿态**：仍要点「添加」才生效）。
    pub(super) fn draft_filter_column(&mut self, column: &str, cx: &mut Context<Self>) {
        if self.mode != Mode::Table {
            return;
        }
        self.filter_column = Some(column.to_owned());
        cx.notify();
    }

    pub(super) fn table_next_page(&mut self, cx: &mut Context<Self>) {
        // 与取数时同一口径：满一页才认为后面还有。
        let page_size = browse_page_size(cx);
        let full = self
            .table_rows
            .as_ref()
            .is_some_and(|rows| rows.row_count() >= page_size);
        if full {
            self.table_page += 1;
            self.load_table_page(cx);
        }
    }

    /// 翻页条「跳转到第 N 页」：解析输入框里的页码（**1 起**），回车后跳到那一页。
    ///
    /// 空 / 非数字 / 0 一律当没输入：静默跳到第 1 页会让人以为「按了回车页面乱跳」。
    /// 查询模式是**客户端**分页（结果已整批在手），改序号即刻生效；表浏览是**服务器
    /// 侧**分页，得重新按新 `OFFSET` 取一页。越界由上界那一侧的取数兜住（表浏览取到
    /// 空页就是到头了，与「下一页」同一套判据）。
    pub(super) fn jump_to_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.page_jump.read(cx).value().trim().to_owned();
        // 1 起的页码 → 0 起的下标；空 / 非数字 / 0 都收敛成 `None`（当没输入）。
        let target = raw
            .parse::<usize>()
            .ok()
            .filter(|page| *page > 0)
            .map(|page| page - 1);
        if let Some(target) = target {
            match self.mode {
                Mode::Query => {
                    self.query_page = target;
                    self.sync_data_table_rows(cx);
                    cx.notify();
                }
                Mode::Table => {
                    if !self.busy && self.table_page != target {
                        self.table_page = target;
                        self.load_table_page(cx);
                    }
                }
                // 实例页 / 仪表板没有「页」的概念，跳页输入框在这两处也不出现。
                // 结构标签同理（它没有结果集）。
                Mode::Instance(_)
                | Mode::Dashboard(_)
                | Mode::Structure(_)
                | Mode::Er { .. }
                | Mode::Compare(_)
                | Mode::Transfer(_) => {}
            }
        }
        // 页码框是**受控显示**（页码是状态、框只是它的显示）：输入被采纳也好、
        // 是空 / 非法也罢，一并回写成真实页码（越界时显示的是夹住之后的那一页，
        // 与脚注读数一致）。这里不能走 `sync_page_jump` 的「聚焦则不动」——
        // 按下回车时输入框必然还聚焦着，正是要覆盖它的那一刻。
        self.write_page_jump(window, cx);
    }

    pub(super) fn refresh_table_page(&mut self, cx: &mut Context<Self>) {
        // 「刷新」按重来理解：行数与索引一并重取（删改之后行数就变了）。
        self.table_meta_for = None;
        self.load_table_page(cx);
    }

    /// 落盘「每页行数」并按新页大小重取（回到第 1 页）。
    ///
    /// 改的是**服务器侧取数**的大小（`LIMIT` + `OFFSET`），所以页码语义会整体位移：
    /// 「第 2 页 · 每页 200」改成每页 500 之后，第 2 页对应的是完全另一批行。与其替
    /// 用户换算一个他没见过的页码，不如回到第 1 页并说清楚——页码是给人定位用的，
    /// 悄悄换掉比换掉后说明更难收拾。
    pub(super) fn set_browse_page_size(&mut self, size: u32, cx: &mut Context<Self>) {
        if browse_page_size(cx) as u32 == size {
            return;
        }
        super::super::module_settings::set_db_page_size(cx, size);
        self.table_page = 0;
        self.status = trf(
            cx,
            "已改为每页 {} 行，并回到第 1 页。",
            &[&size.to_string()],
        )
        .into();
        // 只有正看着一张表才重取；别的模式下改的是「下次打开表时用多大页」。
        if self.mode == Mode::Table && self.selected_table.is_some() {
            self.table_meta_for = None;
            self.load_table_page(cx);
        }
        cx.notify();
    }

    /// 翻页条上的「N 行/页」下拉：就地改 `db.settings.page_size`。
    ///
    /// 与设置页的那一项是**同一个键**（[`module_settings::set_db_page_size`]），
    /// 差别只是入口：翻页的人正在看行数，让他为此跑一趟设置页没有道理。
    ///
    /// 版式与查询侧（[`DatabasePage::query_page_size_dropdown`]）同款：预设 +
    /// 分隔线 + 内联的「自定义行数」区。**表浏览没有「仅本次」这一层** —— 这里的
    /// 页大小直接就是服务器侧取数的 `LIMIT`，落不落盘都是同一个值，所以只给
    /// 「设为默认」一条路（`session_label = None`，输入框旁的 ✓ 也直接写默认）。
    pub(super) fn browse_page_size_dropdown(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let extended = theme.extended();
        let view = self.view.clone();
        let current = browse_page_size(cx) as u32;
        toolbar_text_dropdown_button(
            "db-browse-page-size",
            SharedString::from(trf(cx, "{} 行/页", &[&current.to_string()])),
            Icon::new(DuckIcon::ListOrdered).size(extended.icon.sm),
            Anchor::TopRight,
            move |menu, window, cx| {
                // 弹层要装下内联的「自定义行数」表单，比普通菜单宽一档。
                let menu = menu.min_w(px(260.));
                let menu_entity = cx.entity();
                // 输入框在**弹层构建时**建一次（builder 每次打开才跑，
                // 弹层存活着就复用同一份 —— 输入中的值不会丢）。
                let input = text_input(window, cx, tr(cx, "行数"));
                input.update(cx, |state, cx| {
                    state.set_value(current.to_string(), window, cx)
                });
                let on_apply: PageSizeApply = {
                    let view = view.clone();
                    Rc::new(move |size, _persist, _window, cx| {
                        // 表浏览只有「写默认」一条路：`persist` 入参在这里恒为 true。
                        let _ = view.update(cx, |this, cx| this.set_browse_page_size(size, cx));
                    })
                };
                let close: Rc<dyn Fn(&mut App)> = {
                    let menu_entity = menu_entity.clone();
                    Rc::new(move |cx| {
                        menu_entity.update(cx, |_, cx| cx.emit(DismissEvent));
                    })
                };
                let menu = BROWSE_PAGE_CHOICES.iter().fold(menu, |menu, size| {
                    let view = view.clone();
                    let size = *size;
                    let label = SharedString::from(trf(cx, "{} 行/页", &[&size.to_string()]));
                    menu.item(PopupMenuItem::new(label).checked(size == current).on_click(
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.set_browse_page_size(size, cx);
                            });
                        },
                    ))
                });
                menu.separator().item(PopupMenuItem::element(move |_, cx| {
                    page_size_custom_section(
                        &input,
                        None,
                        None,
                        on_apply.clone(),
                        close.clone(),
                        cx,
                    )
                }))
            },
            cx,
        )
    }

    /// 把过滤条上攒的草稿变成一条生效条件，然后重取第一页。
    ///
    /// 校验只做界面能确定的两件事：选了列、该操作符需要的值非空。**值的类型合法性
    /// 不在这里拦** —— 那要问引擎（驱动把值走绑定参数发出去，服务端报错原样透出），
    /// 在界面里猜类型只会猜错。
    pub(super) fn add_table_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Table {
            return;
        }
        let Some(column) = self.filter_column.clone() else {
            notify::info(tr(cx, "请先选择要过滤的列。"), window, cx);
            return;
        };
        let op = self.filter_op;
        let filter = if op.needs_value() {
            let value = self.filter_value.read(cx).value().to_string();
            if value.is_empty() {
                notify::info(
                    tr(cx, "请填写过滤值（要匹配空值请用「为空」。）"),
                    window,
                    cx,
                );
                return;
            }
            TableFilter::new(column, op, value)
        } else {
            TableFilter::null(column, op)
        };
        self.table_filters.push(filter);
        // 加完就把草稿清掉：留着上一条值，下一次点「添加」很容易误加一条重复条件。
        self.filter_value
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.after_filter_changed(cx);
    }

    /// 摘掉第 `index` 条生效条件。
    pub(super) fn remove_table_filter(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.table_filters.len() {
            self.table_filters.remove(index);
            self.after_filter_changed(cx);
        }
    }

    /// 清空全部生效条件。
    pub(super) fn clear_table_filters(&mut self, cx: &mut Context<Self>) {
        if self.table_filters.is_empty() {
            return;
        }
        self.table_filters.clear();
        self.after_filter_changed(cx);
    }

    /// 过滤条件一变就重新取页。
    ///
    /// 与 [`Self::toggle_table_sort`] 同一套善后：条件换了 → 行号的含义也换了
    /// （勾选 / 选中行 / 行编辑器全部作废）、页码回第一页。
    /// ⚠️ **不清暂存改动**：改格是按主键定位的，与「这一页显示哪些行」无关
    /// （与排序同口径）。
    pub(super) fn after_filter_changed(&mut self, cx: &mut Context<Self>) {
        self.checked_rows.clear();
        self.selected_row = None;
        self.row_editor = None;
        self.table_page = 0;
        self.load_table_page(cx);
        cx.notify();
    }

    pub(super) fn open_cell_detail(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        self.cell_detail = Some((row, col));
        self.reveal_right_dock(RightDock::Cell, cx);
    }

    /// 让单元格详情的**值编辑器**跟上「现在指向的是哪一格」。
    ///
    /// 在**渲染期**做（`Render::render` 入口），理由与 [`Self::sync_json_editor`]
    /// 同一条：构造 `InputState` 要 `&mut Window`，渲染是页面唯一同时握着窗口与
    /// `&mut self` 的时机。指纹 = (行, 列, 原值)：换格 / 原值变了（提交后重取回来）
    /// 才重建，行没换就不动 —— 用户正在输入框里敲的字不被下一帧抹掉。
    ///
    /// 不可编辑（多表 / 无主键 / 列越界）时清掉编辑器：右栏回落到只读展示。
    pub(super) fn sync_cell_detail_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, col)) = self.cell_detail else {
            self.cell_detail_editor = None;
            return;
        };
        let spec = self
            .row_editor_specs(row)
            .and_then(|specs| specs.into_iter().nth(col));
        let Some((column, original, was_null)) = spec else {
            self.cell_detail_editor = None;
            return;
        };
        if self.cell_detail_editor.as_ref().is_some_and(|editor| {
            editor.row == row && editor.col == col && editor.cell.original == original
        }) {
            return;
        }
        let input = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        if !was_null {
            input.update(cx, |state, cx| {
                state.set_value(original.clone(), window, cx);
            });
        }
        self.cell_detail_editor = Some(CellDetailEditor {
            row,
            col,
            cell: RowCell {
                column,
                original,
                was_null,
                input,
                // 与行编辑器同款：开关初始**不勾**（勾不勾由用户点），「已改」的
                // 判据把 `was_null` 算进去 —— NULL 格一打开就显示「已改」是刻意的
                //（对 NULL 格来说，写什么都算改动）。
                null: Rc::new(std::cell::Cell::new(false)),
            },
        });
    }

    /// 把某个 dock 分段送到右栏，**右栏收着时替用户展开**它。
    ///
    /// 「内容落在右栏」的入口都走这里：右栏是 shell 级的（`with_right_sidebar`），
    /// 页面拿不到它的展开态 —— 不展开的话用户点了按钮什么都看不见（此前只能往
    /// 状态栏写一句「已切到右侧栏」兜底，用户仍然看不到内容）。展开的确来自页面时
    /// 记一笔 [`Self::right_dock_auto`]，供内容失效时判断该不该自动收起。
    pub(super) fn reveal_right_dock(&mut self, dock: RightDock, cx: &mut Context<Self>) {
        self.right_dock = dock;
        if !crate::app::right_sidebar_open(cx).unwrap_or(false) {
            // 只有**真的替用户展开**了才算 auto：本来就开着的不动标记，
            // 免得把用户手动展开的状态误判成页面展开的（那会导致内容失效时误收起）。
            self.right_dock_auto = crate::app::set_right_sidebar_open(cx, true);
        }
        cx.notify();
    }

    /// 单元格详情失效时（清结果 / 换结果标签 / 关会话）收起右栏。
    ///
    /// 只在「右栏**正显示单元格详情**、且它是页面替用户展开的」时才收：右栏正显示
    /// 别的分段（历史 / 详情）、或用户自己从顶栏展开的，都不动 —— 页面绝不关用户
    /// 手动开的右栏，也不该因为一次查询清空了格子就把正在看的历史一起收走。
    pub(super) fn dismiss_cell_dock_if_auto(&mut self, cx: &mut Context<Self>) {
        if self.right_dock != RightDock::Cell || !self.right_dock_auto {
            return;
        }
        self.right_dock_auto = false;
        crate::app::set_right_sidebar_open(cx, false);
    }

    /// 订阅活跃路由右栏的开合变化 —— 工具条「历史」按钮的高亮要跟着右栏走。
    ///
    /// 页面是 `AnyView`，shell 重绘不连带重绘它：只有页面自己在展开 / 收起的那一刻
    /// 重绘，按钮才会跟着变。订阅一次即可 —— 路由与右栏实体都是常驻的，重复订阅
    /// 只会堆回调。
    pub(super) fn ensure_right_dock_subscribed(&mut self, cx: &mut Context<Self>) {
        if self.right_dock_subscribed {
            return;
        }
        let Some(state) = crate::app::right_sidebar_state(cx) else {
            return;
        };
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        self.right_dock_subscribed = true;
    }

    /// 左栏：搜索 + 分组连接列表 + 新建入口。
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let searching = !query.is_empty();

        // 分组展示序：用户创建的分组（kv 顺序，空组也显示）→ 连接档案里的新分组名
        // → 默认分组（无分组标签的连接）。
        let mut groups: Vec<(String, Vec<&ConnectionConfig>)> = self
            .groups
            .iter()
            .map(|name| (name.clone(), Vec::new()))
            .collect();
        for cfg in &self.connections {
            let label = match (&cfg.group, cfg.env_tag) {
                (Some(group), _) if !group.is_empty() => group.clone(),
                _ => tr(cx, "默认分组").to_owned(),
            };
            match groups.iter_mut().find(|(name, _)| *name == label) {
                Some((_, items)) => items.push(cfg),
                None => groups.push((label, vec![cfg])),
            }
        }
        for (_, items) in &mut groups {
            items.retain(|cfg| {
                if !searching {
                    return true;
                }
                if cfg.display_name().to_lowercase().contains(&query)
                    || format!("{} {}", cfg.host, cfg.database)
                        .to_lowercase()
                        .contains(&query)
                {
                    return true;
                }
                // 搜索框按 DBFlux 的口径是「筛选连接**和对象**」：打了表名时，
                // 装着这张表的连接必须留着——否则连接被筛掉、命中项跟着消失，
                // 用户看到的是「明明有这张表却说没有」。
                self.sessions.get(&cfg.id).is_some_and(|session| {
                    session
                        .catalog
                        .iter()
                        .any(|entry| object_matches(&entry.name, &query))
                        || [
                            TreeCategory::Trigger,
                            TreeCategory::Event,
                            TreeCategory::Index,
                            TreeCategory::User,
                        ]
                        .iter()
                        .any(|category| {
                            session
                                .database_objects(*category)
                                .iter()
                                .any(|object| object_matches(&object.name, &query))
                        })
                })
            });
        }
        // 搜索时空组不占位；非搜索态只有用户创建的组（kv 里有名）才允许为空。
        groups.retain(|(label, items)| {
            !items.is_empty() || (!searching && self.groups.contains(label))
        });

        let mut nav = v_flex().w_full().gap(tokens.spacing.xxs);
        for (label, items) in &groups {
            nav = nav.child(
                h_flex()
                    .w_full()
                    .px(tokens.spacing.sm)
                    .pt(tokens.spacing.sm)
                    .pb(tokens.spacing.xxs)
                    .child(caption_heading(label.clone(), cx)),
            );
            for cfg in items {
                let id = cfg.id.clone();
                let connected = self.sessions.contains_key(&id);
                // 在途（点了连接、还没回话）：圆点给「进行中」色（与驱动下载同一条读数），
                // 让「点了没动静」变成「这条正在连」。此时行点击仍走 `connect_entry`，
                // 那里按 id 静默忽略重复。
                let connecting = self.connecting.contains(&id);
                // ── 右键菜单的「弹出时事实」在渲染期算好（闭包是 `Fn`，只借克隆）──
                let menu_name = cfg.display_name();
                let host_info = (!cfg.host.trim().is_empty()).then(|| match cfg.port {
                    Some(port) => format!("{}:{}", cfg.host.trim(), port),
                    None => cfg.host.trim().to_owned(),
                });
                let user_info = (!cfg.user.trim().is_empty()).then(|| cfg.user.trim().to_owned());
                let database_info =
                    (!cfg.database.trim().is_empty()).then(|| cfg.database.trim().to_owned());
                // 「这是哪个引擎」只问 [`EngineRef`] 一处 —— agent 档的展示名 / 图标
                // 都在驱动清单里，按 `kind` 手写 `match` 会漏。
                // `tr` 对 agent 档的中文源串出译文；内建七档那些 ASCII 品牌名不在表里，
                // 原样返回（两种来源的约定不同，见 `EngineRef::label`）。
                let engine = EngineRef::from_config(cfg);
                let conn_summary = {
                    let mut parts = vec![tr(cx, engine.label()).to_owned()];
                    if let Some(host) = &host_info {
                        parts.push(host.clone());
                    }
                    if let Some(user) = &user_info {
                        parts.push(user.clone());
                    }
                    if let Some(database) = &database_info {
                        parts.push(database.clone());
                    }
                    parts.join(" · ")
                };
                // 「用户与权限」跳的是**当前命名空间**的分类节点：目录只对内省过的
                // 命名空间有效，且服务器没有用户体系（SQLite）时整项不画。
                let users_ns = self.sessions.get(&id).and_then(|session| {
                    if session.users.is_empty() {
                        return None;
                    }
                    let namespace = if namespace_is_database(session.kind) {
                        session.database.clone()
                    } else {
                        session.schema.clone()
                    }?;
                    (!namespace.trim().is_empty()).then_some(namespace)
                });
                // 进程列表（实例检查器）只对读得到服务器数据的引擎画。
                let can_inspect = connected && cfg.kind.supports_instance_insight();
                // 「新建数据库」只对 SQL 引擎画：SQLite 文件即库，Mongo / ES 没有
                // 带引号的库名概念。判据是**这条连接**的引号（`self.quote_of(id)`）而不是
                // `cfg.kind`：agent 档是一整个家族，kind 那层答不出它接不接 SQL。
                let can_create_db =
                    connected && self.quote_of(&id).is_some() && cfg.kind != DatabaseKind::Sqlite;
                // 连接子树的展开态（dbx 同款行首箭头）：搜索态一律展开——命中项
                // 必须看得见；连接成功的瞬间顺手置为展开，子树跟着出现不突兀。
                let conn_key = format!("conn:{id}");
                let expanded = searching || self.tree_open.contains(&conn_key);
                // 行首 = 箭头（仅已连时，未连没有子树可折）+ 品牌 logo。两者拼成
                // 一块塞进 `icon` 槽：箭头是行内嵌套按钮，要点切换就得在这里截停
                // 冒泡（同库节点的展开箭头）。
                let leading = {
                    let mut leading = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
                    if connected {
                        let toggle_view = view.clone();
                        let toggle_key = conn_key.clone();
                        let was_closed = !self.tree_open.contains(&conn_key);
                        let chevron = if was_closed {
                            DuckIcon::ChevronRight
                        } else {
                            DuckIcon::ChevronDown
                        };
                        leading = leading.child(
                            icon_button(
                                SharedString::from(format!("db-conn-toggle-{id}")),
                                if was_closed {
                                    tr(cx, "展开")
                                } else {
                                    tr(cx, "收起")
                                },
                                Icon::new(chevron)
                                    .size(extended.icon.sm)
                                    .text_color(tokens.colors.muted_foreground),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click(move |_, _, cx| {
                                let Some(entity) = toggle_view.upgrade() else {
                                    return;
                                };
                                let key = toggle_key.clone();
                                entity.update(cx, |this, cx| {
                                    if !this.tree_open.remove(&key) {
                                        this.tree_open.insert(key);
                                    }
                                    cx.notify();
                                });
                                cx.stop_propagation();
                            }),
                        );
                    }
                    leading.child(engine_logo(engine, px(CONN_LOGO_SIZE), cx))
                };
                let trailing = h_flex()
                    .flex_none()
                    .gap(tokens.spacing.xxs)
                    .children(env_tag_label(cfg.env_tag, cx).map(|tag| neutral_tag(tag, cx)))
                    .child(
                        div()
                            .flex_none()
                            .text_size(tokens.typography.xs.size)
                            .text_color(extended.colors.text_tertiary)
                            .child(tr(cx, engine.label())),
                    )
                    // 连接状态圆点：行首图标的着色之外再给一个不依赖颜色记忆的信号
                    // （DBX / TablePlus 同款）：绿=已连，灰=未连。
                    .child(
                        div()
                            .flex_none()
                            .size(px(8.))
                            .rounded_full()
                            .bg(if connected {
                                extended.colors.status_completed
                            } else if connecting {
                                extended.colors.status_downloading
                            } else {
                                tokens.colors.muted_foreground
                            }),
                    )
                    .child(
                        icon_button(
                            SharedString::from(format!("db-edit-{id}")),
                            tr(cx, "编辑连接"),
                            Icon::new(DuckIcon::Settings)
                                .size(extended.icon.sm)
                                .text_color(tokens.colors.muted_foreground),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            let id = id.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.edit_connection(&id, window, cx);
                                });
                                // 同库节点的箭头：齿轮嵌在连接行按钮里，不截停冒泡
                                // 的话点齿轮还会顺手激活/连接这条连接。
                                cx.stop_propagation();
                            }
                        }),
                    );
                nav = nav.child(
                    sidebar_navigation_button(
                        SharedString::from(format!("db-conn-{id}")),
                        SharedString::from(cfg.display_name()),
                        leading,
                        trailing,
                        connected,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        let id = id.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                if this.sessions.contains_key(&id) {
                                    this.activate_conn(&id, window, cx);
                                    // 点行名 = 「进去看看」：折叠着就顺手铺开，
                                    // 不然激活了也看不出主区为什么换了内容。
                                    this.tree_open.insert(format!("conn:{id}"));
                                } else {
                                    // 连接成功后子树才有内容：顺手置为展开，免得
                                    // 「连上了却看不出多了什么」。
                                    this.tree_open.insert(format!("conn:{id}"));
                                    this.connect_entry(&id, window, cx);
                                }
                            });
                        }
                    })
                    // 连接节点右键（dbx 同款全量入口，每项都有真实后端）：
                    // 断开 / 复制名称 / 复制连接信息 ▸ / 新建查询 / 查看资源对象 /
                    // AI 助手 / 历史查询 / 用户与权限 / 进程列表 / 服务器仪表盘 /
                    // 刷新 / 编辑 / 复制 / 移至新分组 / 删除。
                    // 与右上角的编辑齿轮重复是**有意的**：右键是第一入口，不用去找小图标。
                    .context_menu({
                        let view = view.clone();
                        let menu_id = id.clone();
                        let is_connected = connected;
                        let menu_name = menu_name.clone();
                        let host_info = host_info.clone();
                        let user_info = user_info.clone();
                        let database_info = database_info.clone();
                        let conn_summary = conn_summary.clone();
                        let users_ns = users_ns.clone();
                        let disconnect_label = tr(cx, "断开连接");
                        let copy_name_label = tr(cx, "复制名称");
                        let copied_name_note = tr(cx, "已复制名称。");
                        let copy_info_label = tr(cx, "复制连接信息");
                        let copied_info_note = tr(cx, "已复制连接信息。");
                        let host_label = tr(cx, "主机与端口");
                        let user_label = tr(cx, "用户名");
                        let database_label = tr(cx, "默认数据库");
                        let all_info_label = tr(cx, "连接信息（全部）");
                        let new_query_label = tr(cx, "新建查询");
                        let browse_objects_label = tr(cx, "查看资源对象");
                        let ai_label = tr(cx, "AI 助手");
                        let history_label = tr(cx, "历史查询");
                        let users_label = tr(cx, "用户与权限");
                        let processes_label = tr(cx, "进程列表");
                        let dashboard_label = tr(cx, "服务器仪表盘");
                        let refresh_label = tr(cx, "刷新");
                        let edit_label = tr(cx, "编辑连接");
                        let duplicate_label = tr(cx, "复制连接");
                        let move_group_label = tr(cx, "移至新分组");
                        let delete_label = tr(cx, "删除连接");
                        let create_db_label = tr(cx, "新建数据库");
                        let run_file_label = tr(cx, "执行 SQL 文件");
                        move |mut menu, window, cx| {
                            let view = view.clone();
                            // ── 连接 ──
                            if is_connected {
                                let disconnect_view = view.clone();
                                let disconnect_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(disconnect_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = disconnect_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.close_tab(&disconnect_id, window, cx);
                                        });
                                    },
                                ));
                            }
                            // ── 复制 ──
                            {
                                let copy_view = view.clone();
                                let name = menu_name.clone();
                                menu = menu.item(PopupMenuItem::new(copy_name_label).on_click(
                                    move |_, _, cx| {
                                        let Some(entity) = copy_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.put_on_clipboard(
                                                name.clone(),
                                                copied_name_note,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            {
                                // 「复制连接信息 ▸」：把连接串拆成单独可取的字段（dbx
                                // 同款子菜单）；网络引擎才有的字段不画（主机为空 =
                                // SQLite 这类本地引擎）。
                                let info_view = view.clone();
                                let host = host_info.clone();
                                let user = user_info.clone();
                                let database = database_info.clone();
                                let summary = conn_summary.clone();
                                // `copied_info_note` 是 `&'static str`（Copy）：每个
                                // `move` 闭包各捕获一份副本即可，无需 clone。
                                let info_menu = PopupMenu::build(window, cx, move |menu, _, _| {
                                    let mut menu = menu;
                                    if let Some(host) = host.clone() {
                                        let info_view = info_view.clone();
                                        menu = menu.item(PopupMenuItem::new(host_label).on_click(
                                            move |_, _, cx| {
                                                let Some(entity) = info_view.upgrade() else {
                                                    return;
                                                };
                                                entity.update(cx, |this, cx| {
                                                    this.put_on_clipboard(
                                                        host.clone(),
                                                        copied_info_note,
                                                        cx,
                                                    );
                                                });
                                            },
                                        ));
                                    }
                                    if let Some(user) = user.clone() {
                                        let info_view = info_view.clone();
                                        menu = menu.item(PopupMenuItem::new(user_label).on_click(
                                            move |_, _, cx| {
                                                let Some(entity) = info_view.upgrade() else {
                                                    return;
                                                };
                                                entity.update(cx, |this, cx| {
                                                    this.put_on_clipboard(
                                                        user.clone(),
                                                        copied_info_note,
                                                        cx,
                                                    );
                                                });
                                            },
                                        ));
                                    }
                                    if let Some(database) = database.clone() {
                                        let info_view = info_view.clone();
                                        menu =
                                            menu.item(PopupMenuItem::new(database_label).on_click(
                                                move |_, _, cx| {
                                                    let Some(entity) = info_view.upgrade() else {
                                                        return;
                                                    };
                                                    entity.update(cx, |this, cx| {
                                                        this.put_on_clipboard(
                                                            database.clone(),
                                                            copied_info_note,
                                                            cx,
                                                        );
                                                    });
                                                },
                                            ));
                                    }
                                    menu = menu.separator().item(
                                        PopupMenuItem::new(all_info_label).on_click(
                                            move |_, _, cx| {
                                                let Some(entity) = info_view.upgrade() else {
                                                    return;
                                                };
                                                entity.update(cx, |this, cx| {
                                                    this.put_on_clipboard(
                                                        summary.clone(),
                                                        copied_info_note,
                                                        cx,
                                                    );
                                                });
                                            },
                                        ),
                                    );
                                    menu
                                });
                                menu =
                                    menu.item(PopupMenuItem::submenu(copy_info_label, info_menu));
                            }
                            // ── 查询 ──
                            if is_connected {
                                let query_view = view.clone();
                                let query_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(new_query_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = query_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.activate_conn(&query_id, window, cx);
                                            this.open_tab(&query_id, window, cx);
                                        });
                                    },
                                ));
                            }
                            {
                                // 查看资源对象：激活这条连接并铺开它的树（与点行名同口径，
                                // 只是不切主区焦点）。
                                let browse_view = view.clone();
                                let browse_id = menu_id.clone();
                                menu =
                                    menu.item(PopupMenuItem::new(browse_objects_label).on_click(
                                        move |_, window, cx| {
                                            let Some(entity) = browse_view.upgrade() else {
                                                return;
                                            };
                                            entity.update(cx, |this, cx| {
                                                this.activate_conn(&browse_id, window, cx);
                                                this.tree_open.insert(format!("conn:{browse_id}"));
                                                cx.notify();
                                            });
                                        },
                                    ));
                            }
                            {
                                let ai_view = view.clone();
                                menu = menu.item(PopupMenuItem::new(ai_label).on_click(
                                    move |_, _, cx| {
                                        let Some(entity) = ai_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.reveal_right_dock(RightDock::Ai, cx);
                                        });
                                    },
                                ));
                            }
                            {
                                let history_view = view.clone();
                                menu = menu.item(PopupMenuItem::new(history_label).on_click(
                                    move |_, _, cx| {
                                        let Some(entity) = history_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.reveal_right_dock(RightDock::History, cx);
                                        });
                                    },
                                ));
                            }
                            if let Some(namespace) = users_ns.clone() {
                                // 用户与权限：铺开当前命名空间的「用户与权限」分类。
                                let users_view = view.clone();
                                let users_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(users_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = users_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.activate_conn(&users_id, window, cx);
                                            this.tree_open.insert(format!("conn:{users_id}"));
                                            this.tree_open
                                                .insert(format!("cat:{users_id}:{namespace}:user"));
                                            cx.notify();
                                        });
                                    },
                                ));
                            }
                            // ── 服务器 ──
                            if can_inspect {
                                let inspect_view = view.clone();
                                let inspect_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(processes_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = inspect_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.activate_conn(&inspect_id, window, cx);
                                            this.open_instance_tab(
                                                &inspect_id,
                                                InstanceTab::Inspector,
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            if is_connected {
                                let dashboard_view = view.clone();
                                let dashboard_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(dashboard_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = dashboard_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.activate_conn(&dashboard_id, window, cx);
                                            this.open_instance_tab(
                                                &dashboard_id,
                                                InstanceTab::Dashboard,
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            // ── 管理 ──
                            if is_connected {
                                let refresh_view = view.clone();
                                let refresh_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(refresh_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = refresh_view.upgrade() else {
                                            return;
                                        };
                                        // 刷新的是**这条**连接：先把会话激活再刷新
                                        //（`refresh_state` 只认活跃连接）。
                                        entity.update(cx, |this, cx| {
                                            this.activate_conn(&refresh_id, window, cx);
                                            this.refresh_state(window, cx);
                                        });
                                    },
                                ));
                            }
                            if can_create_db {
                                let createdb_view = view.clone();
                                let createdb_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(create_db_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = createdb_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.open_create_database_dialog(
                                                &createdb_id,
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            if is_connected {
                                // 执行 SQL 文件：复用 `run_sql_file` 的流式批处理路径
                                //（一次加锁 + 双阈值刷批 + 可停止），先激活这条连接。
                                let runfile_view = view.clone();
                                let runfile_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(run_file_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = runfile_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.activate_conn(&runfile_id, window, cx);
                                            this.run_sql_file(window, cx);
                                        });
                                    },
                                ));
                            }
                            {
                                let edit_view = view.clone();
                                let edit_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(edit_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = edit_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.edit_connection(&edit_id, window, cx);
                                        });
                                    },
                                ));
                            }
                            {
                                let duplicate_view = view.clone();
                                let duplicate_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(duplicate_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = duplicate_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.duplicate_connection(&duplicate_id, window, cx);
                                        });
                                    },
                                ));
                            }
                            {
                                let group_view = view.clone();
                                let group_id = menu_id.clone();
                                menu = menu.item(PopupMenuItem::new(move_group_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = group_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.open_move_to_group_dialog(&group_id, window, cx);
                                        });
                                    },
                                ));
                            }
                            {
                                let delete_view = view.clone();
                                let delete_id = menu_id.clone();
                                menu.item(PopupMenuItem::new(delete_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = delete_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.delete_connection(&delete_id, window, cx);
                                        });
                                    },
                                ))
                            }
                        }
                    }),
                );

                // ── 连接节点下的功能入口（DBFlux 同形）──
                //
                // DBFlux 把「仪表盘 / 已保存图表 / 数据库 / 实例概览 / 实例指标 /
                // 实例检查器」平铺在连接节点下；这里同形，只是第四项按本项目的
                // **真实功能**改成了「已保存查询」（没有图表渲染后端，就不摆一个
                // 点开是空壳的「图表」）。
                //
                // 未连接的档案不列：入口里的数据全来自会话与内省结果，没会话就没有
                // 内容可列（连接节点自己那行的右键菜单才是未连接时的操作入口）。
                // 展开态 gate 在这里：折叠的连接不渲染整棵子树（dbx 同款）。
                // `filter` 而不是嵌套 `if`：嵌套会被 clippy 建议合并成 let-chain，
                // 而 let-chain 要 Rust 1.88，本仓 MSRV 是 1.85。
                if let Some(session) = self.sessions.get(&id).filter(|_| expanded) {
                    nav = nav.child(self.render_connection_tree(&id, session, &query, cx));
                }
            }
        }
        if searching && groups.is_empty() {
            nav = nav.child(
                div()
                    .w_full()
                    .py(tokens.spacing.lg)
                    .flex()
                    .justify_center()
                    .child(meta_text(tr(cx, "没有匹配的连接。"), cx)),
            );
        }

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
                    .child(
                        // dbx 同形三按钮：新建分组 / 刷新 / 新建连接。
                        h_flex()
                            .gap(tokens.spacing.xxs)
                            .child(
                                icon_button(
                                    "db-new-group",
                                    tr(cx, "新建分组"),
                                    Icon::new(DuckIcon::FolderPlus)
                                        .size(extended.icon.sm)
                                        .text_color(tokens.colors.muted_foreground),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .on_click({
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.open_new_group_dialog(window, cx);
                                        });
                                    }
                                }),
                            )
                            .child(
                                icon_button(
                                    "db-refresh",
                                    tr(cx, "刷新"),
                                    Icon::new(DuckIcon::RotateCw)
                                        .size(extended.icon.sm)
                                        .text_color(tokens.colors.muted_foreground),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .disabled(self.busy)
                                .on_click({
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.refresh_state(window, cx);
                                        });
                                    }
                                }),
                            )
                            .child(
                                icon_button(
                                    "db-new-connection-icon",
                                    tr(cx, "新建连接"),
                                    Icon::new(DuckIcon::Plug)
                                        .size(extended.icon.sm)
                                        .text_color(tokens.colors.muted_foreground),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .on_click({
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.open_new_connection_picker(window, cx);
                                        });
                                    }
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .px(tokens.spacing.sm)
                    .pb(tokens.spacing.xs)
                    .child(
                        h_flex()
                            .h(theme.density().control)
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
                    .id("db-connection-nav")
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
                    .child(
                        h_flex()
                            .w_full()
                            .gap(tokens.spacing.xs)
                            .child(
                                button(
                                    "db-export-connections",
                                    tr(cx, "导出连接"),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .flex_1()
                                .on_click({
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.export_connections(window, cx)
                                        });
                                    }
                                }),
                            )
                            .child(
                                button(
                                    "db-import-connections",
                                    tr(cx, "导入连接"),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .flex_1()
                                .on_click({
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.import_connections(window, cx)
                                        });
                                    }
                                }),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .justify_center()
                            .child(meta_text(tr(cx, "点击连接，铅笔进入编辑"), cx)),
                    ),
            )
            .into_any_element()
    }

    /// 「新增行」入口（tab 栏动作组）：按当前结果集的列清单开一个空行编辑器。
    pub(super) fn open_row_editor_for_insert(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 只读借用阶段取列清单，再构造输入框实体（与点行开编辑器同形）。
        let specs = self.new_row_specs();
        let cells = specs.map(|columns| {
            columns
                .into_iter()
                .map(|(column, original, was_null)| RowCell {
                    column,
                    original,
                    was_null,
                    input: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
                    null: Rc::new(std::cell::Cell::new(false)),
                })
                .collect()
        });
        self.selected_row = None;
        self.row_editor = cells.map(|cells| RowEditor {
            row_index: None,
            cells,
        });
        if self.row_editor.is_none() {
            // 兜底：列清单取不到（结果集不可编辑 / 没有结果）就什么都不做 ——
            // 「新增行」这个入口本身已置灰，这里只是防漏，别让状态栏替用户说一句
            // 「已就绪」而右栏打开后是空的。
            return;
        }
        // ⚠️ 新增行**没有对应的表格行**：它是一份待填的草稿（`row_index: None`，提交时
        // 走 INSERT），网格里不会凭空多出一行 —— 那是「结果集」该有的东西，而结果集
        // 是数据库回来的，不能由界面伪造。所以「定位到这一行」在编辑卡形态下就是
        // **把承载它的右栏展开到「详情」分段 + 光标落到第一格**：此前只挂 `row_editor`
        // 而不展开右栏，右栏收着时点「新增行」什么都看不见（用户实测反馈）。
        self.reveal_right_dock(RightDock::Detail, cx);
        if let Some(first) = self
            .row_editor
            .as_ref()
            .and_then(|editor| editor.cells.first())
        {
            first.input.update(cx, |state, cx| state.focus(window, cx));
        }
        self.status = tr(
            cx,
            "新增行已就绪：在右栏「选中行」卡里填写，点「加入暂存」。",
        )
        .into();
        cx.notify();
    }

    /// 左栏树：一个分类节点（`表 (14)` 这种），展开时把对象叶子挂在下面。
    ///
    /// 计数**跟着筛选词走**：界面上那个数字与看得见的行永远一致（筛到 3 张表就写 3）。
    /// 空分类整棵不画 —— `information_schema` 那种库里没有触发器时，画一个「触发器 (0)」
    /// 只是噪音。
    /// 左栏一个**可折叠入口**行（数据库 / 已保存查询共用）。
    ///
    /// 点整行只切折叠状态：这些入口不是「当前对象」，选中态留给真正在看的东西
    /// （当前库 / 当前表），与[`Self::render_tree_category`]的分类行同口径。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn sidebar_toggle_row(
        &self,
        element_id: String,
        open_key: String,
        icon: DuckIcon,
        // `&'static str` → `Into<SharedString>`：文件夹名是**运行时数据**（用户起的名字），
        // 不是编译期字面量，收窄成 `'static` 就没法给文件夹画这一行了。
        label: impl Into<SharedString>,
        count: Option<usize>,
        selected: bool,
        open: bool,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let trailing = h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .children(count.map(|count| meta_text(count.to_string(), cx)))
            .child(
                Icon::new(if open {
                    DuckIcon::ChevronDown
                } else {
                    DuckIcon::ChevronRight
                })
                .size(extended.icon.sm)
                .text_color(tokens.colors.muted_foreground),
            );
        sidebar_navigation_button(
            SharedString::from(element_id),
            // 这里**不能再包一层** `SharedString::from`：`label` 已经是
            // `impl Into<SharedString>`，再包就成了 `impl Into<SharedString>` → `SharedString`
            // 的二次转换，编译器认不出（E0277）。
            label,
            Icon::new(icon)
                .size(extended.icon.md)
                .text_color(tokens.colors.muted_foreground),
            trailing,
            selected,
            cx,
        )
        .on_click(move |_, _, cx| {
            let Some(entity) = view.upgrade() else {
                return;
            };
            let key = open_key.clone();
            entity.update(cx, |this, cx| {
                if !this.tree_open.remove(&key) {
                    this.tree_open.insert(key);
                }
                cx.notify();
            });
        })
        .into_any_element()
    }

    /// 左栏一个**实例信息页入口**行（仪表盘 / 实例概览 / 实例指标 / 实例检查器）。
    ///
    /// 活动标签正停在这一档（且属于这条连接）时给选中态（否则用户点完不知道
    /// 自己在看哪一页）。
    pub(super) fn sidebar_instance_row(
        &self,
        conn_id: &str,
        tab: InstanceTab,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        // 闭包是 `'static`：连接 id 得自己持一份（不能借用方法参数）。
        let connection = conn_id.to_owned();
        let selected = self.active_tab_kind(conn_id) == Some(&TabKind::Instance(tab));
        sidebar_navigation_button(
            SharedString::from(format!("db-instance-{conn_id}-{}", tab.key())),
            SharedString::from(tab.heading(cx)),
            Icon::new(instance_tab_icon(tab))
                .size(extended.icon.md)
                .text_color(if selected {
                    extended.colors.accent_text
                } else {
                    tokens.colors.muted_foreground
                }),
            div().flex_none(),
            selected,
            cx,
        )
        .on_click(move |_, window, cx| {
            let Some(entity) = view.upgrade() else {
                return;
            };
            entity.update(cx, |this, cx| {
                this.open_instance_tab(&connection, tab, window, cx);
            });
        })
        .into_any_element()
    }

    /// 左栏「实例指标」段：可展开，子级逐项列**数值指标**（DBFlux 同形）。
    ///
    /// 还没采样（本会话没拉过指标）时退回扁平行：点了直接进实例指标页拉第一轮，
    /// 有采样后才展开出指标子项——空展开节点比没有节点更让人困惑。指标名是
    /// 引擎口径原文（`Threads_connected` 之类），照抄不翻译，与图表卡标题同源。
    pub(super) fn sidebar_metrics_section(
        &self,
        conn_id: &str,
        query: &str,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let keys: Vec<String> = self
            .metric_history
            .get(conn_id)
            .map(|history| history.keys())
            .unwrap_or_default();
        if keys.is_empty() {
            return self.sidebar_instance_row(conn_id, InstanceTab::Metrics, cx);
        }
        let hits: Vec<String> = keys
            .into_iter()
            .filter(|key| object_matches(key, query))
            .collect();
        let key = format!("sec:metrics:{conn_id}");
        // 搜索态一律展开：命中项要看得见（与库节点 / 分类节点同口径）。
        let open = !query.is_empty() || self.tree_open.contains(&key);
        let selected =
            self.active_tab_kind(conn_id) == Some(&TabKind::Instance(InstanceTab::Metrics));
        let section = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .child(self.sidebar_toggle_row(
                format!("db-section-metrics-{conn_id}"),
                key,
                DuckIcon::DatabaseZap,
                tr(cx, "实例指标"),
                Some(hits.len()),
                selected,
                open,
                cx,
            ));
        if !open {
            return section.into_any_element();
        }
        let mut children = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .pl(tokens.spacing.md);
        if hits.is_empty() {
            children = children.child(
                div()
                    .w_full()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(meta_text(tr(cx, "没有匹配的指标。"), cx)),
            );
        }
        for metric in hits {
            // 尾巴挂**最近一次采样的值**：不展开子级也能扫一眼当前读数（DBFlux
            // 的子项带迷你走势图，这边先用数值占位，与明细表同一来源）。
            let latest = self
                .metric_history
                .get(conn_id)
                .and_then(|history| history.series(&metric).pop())
                .map(|(_, value)| compact_number(value));
            let trailing = h_flex()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.xxs)
                .children(latest.map(|value| meta_text(value, cx)));
            let selected = self.active_tab_kind(conn_id)
                == Some(&TabKind::Instance(InstanceTab::Metrics))
                && self.focused_metric.as_deref() == Some(metric.as_str());
            let row_view = view.clone();
            let connection = conn_id.to_owned();
            let target = metric.clone();
            children = children.child(
                sidebar_navigation_button(
                    SharedString::from(format!("db-metric-{conn_id}-{metric}")),
                    SharedString::from(metric),
                    Icon::new(DuckIcon::ChartLine)
                        .size(extended.icon.md)
                        .text_color(if selected {
                            extended.colors.accent_text
                        } else {
                            tokens.colors.muted_foreground
                        }),
                    trailing,
                    selected,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let Some(entity) = row_view.upgrade() else {
                        return;
                    };
                    entity.update(cx, |this, cx| {
                        // `open_instance_tab` 自己会复用激活 / 新开（激活会把该标签
                        // 快照装回来），所以**聚焦指标要在开完之后置**：先置的话会被
                        // 快照里的旧值盖掉，「点子项聚焦卡片」就失灵了。
                        this.open_instance_tab(&connection, InstanceTab::Metrics, window, cx);
                        this.focused_metric = Some(target.clone());
                        cx.notify();
                    });
                }),
            );
        }
        section.child(children).into_any_element()
    }

    /// 左栏「实例检查器」段：可展开，子级是**进程列表**（DBFlux 同形）。
    ///
    /// 内核目前只有一张会话表（`InstanceView::Processes`），所以只有一个子项——
    /// 不是空壳入口：点进去就是检查器页那张表。SQLite 没有服务端会话列表，
    /// 「进程列表」的口径解释由检查器页自己给（那边按引擎拼说明文案）。
    pub(super) fn sidebar_inspector_section(&self, conn_id: &str, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let key = format!("sec:inspector:{conn_id}");
        let open = self.tree_open.contains(&key);
        let selected =
            self.active_tab_kind(conn_id) == Some(&TabKind::Instance(InstanceTab::Inspector));
        let mut section = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .child(self.sidebar_toggle_row(
                format!("db-section-inspector-{conn_id}"),
                key,
                DuckIcon::ListOrdered,
                tr(cx, "实例检查器"),
                None,
                selected,
                open,
                cx,
            ));
        if !open {
            return section.into_any_element();
        }
        let row_view = view.clone();
        let connection = conn_id.to_owned();
        section = section.child(
            sidebar_navigation_button(
                SharedString::from(format!("db-processes-{conn_id}")),
                SharedString::from(tr(cx, "进程列表")),
                Icon::new(DuckIcon::Table)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.muted_foreground),
                div().flex_none(),
                false,
                cx,
            )
            .on_click(move |_, window, cx| {
                let Some(entity) = row_view.upgrade() else {
                    return;
                };
                entity.update(cx, |this, cx| {
                    this.open_instance_tab(&connection, InstanceTab::Inspector, window, cx);
                    cx.notify();
                });
            }),
        );
        section.into_any_element()
    }

    /// 左栏「查询」入口 + 它的子级（保存的查询，按文件夹分组，点一下载入编辑器）。
    ///
    /// 挂在**库节点**下、排在分类（表 / 视图 / 索引 / …）之后：这是参考实现的摆法
    /// （清单里最后一项就是「查询」），也与「点开一个库想干点什么」的动线一致。
    ///
    /// 分组只有两层，且**文件与文件夹平级**（未归文件夹的排在最前）：给每个查询都
    /// 套一个「SQL 库」根节点，对绝大多数人就是白白多一层缩进。文件夹本身从查询推导
    /// （见 `helpers::saved_query_folders`）—— 不另存一份会跟查询打架的清单。
    ///
    /// ⚠️ 保存的查询是**跨连接 / 跨库共享**的（kv 单键 [`KEY_SAVED_QUERIES`]，没有归属
    /// 字段），所以搬进库节点只是**摆位**变了，内容仍是那一份全局清单。
    pub(super) fn sidebar_saved_queries_section(
        &self,
        conn_id: &str,
        query: &str,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let key = format!("sec:saved:{conn_id}");
        let open = !query.is_empty() || self.tree_open.contains(&key);
        let hits: Vec<&SavedQuery> = self
            .saved_queries
            .iter()
            .filter(|saved| object_matches(&saved.name, query))
            .collect();
        // 缩进一格：这一段与分类节点（表 / 索引 / …）**同层**挂在库节点下，少这层
        // 缩进就会比分类行凸出去一列（层级全丢）。
        let mut section = v_flex()
            .w_full()
            .pl(tokens.spacing.md)
            .gap(tokens.spacing.xxs)
            .child(self.sidebar_toggle_row(
                format!("db-section-saved-{conn_id}"),
                key,
                DuckIcon::Star,
                tr(cx, "查询"),
                Some(hits.len()),
                false,
                open,
                cx,
            ));
        if !open {
            return section.into_any_element();
        }
        let mut children = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .pl(tokens.spacing.md);
        if hits.is_empty() {
            children = children.child(
                div()
                    .w_full()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(meta_text(tr(cx, "还没有保存的查询。"), cx)),
            );
        }
        // 未归文件夹的排最前：新存的都落在这儿，位置稳定，不用每次去树里找。
        for saved in hits
            .iter()
            .filter(|saved| saved_query_folder(saved).is_none())
        {
            children = children.child(self.sidebar_saved_query_row(saved, cx));
        }
        // 再按文件夹分组。有筛选词时文件夹**默认展开**（与根段同口径）：搜索命中的
        // 结果藏在折叠的文件夹里，等于搜不到；被筛空的文件夹整棵不画 —— 一串
        // 「文件夹 (0)」只是噪音。
        for folder in saved_query_folders(&self.saved_queries) {
            let in_folder: Vec<&SavedQuery> = hits
                .iter()
                .copied()
                .filter(|saved| saved_query_folder(saved) == Some(folder.as_str()))
                .collect();
            if in_folder.is_empty() {
                continue;
            }
            let folder_key = format!("sec:saved-folder:{conn_id}:{folder}");
            let folder_open = !query.is_empty() || self.tree_open.contains(&folder_key);
            children = children.child(self.sidebar_toggle_row(
                format!("db-saved-folder-{conn_id}-{folder}"),
                folder_key,
                DuckIcon::Folders,
                SharedString::from(folder.clone()),
                Some(in_folder.len()),
                false,
                folder_open,
                cx,
            ));
            if folder_open {
                let mut inner = v_flex()
                    .w_full()
                    .gap(tokens.spacing.xxs)
                    .pl(tokens.spacing.md);
                for saved in in_folder {
                    inner = inner.child(self.sidebar_saved_query_row(saved, cx));
                }
                children = children.child(inner);
            }
        }
        section = section.child(children);
        section.into_any_element()
    }

    /// 左栏「SQL 库」里**一条查询**的行：点一下载入编辑器，右键载入 / 重命名 / 删除。
    ///
    /// 从 `sidebar_saved_queries_section` 里抽出来：分组之后同一条查询要能在两个位置
    /// （根部 / 文件夹内）出现，行本身只该有一份实现。
    fn sidebar_saved_query_row(&self, saved: &SavedQuery, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let id = saved.id.clone();
        let name = saved.name.clone();
        let sql = saved.sql.clone();
        // 两类闭包各持一份句柄：`on_click` 与 `context_menu` 的闭包都会把捕获值
        // **move** 走，共用一份第二个就没得用了（E0382）。
        let click_view = view.clone();
        let menu_view = view.clone();
        let click_sql = sql.clone();
        let click_name = name.clone();
        let menu_sql = sql;
        let menu_name = name;
        let menu_id = id.clone();
        let load_label = tr(cx, "载入到编辑器");
        let rename_label = tr(cx, "重命名");
        let delete_label = tr(cx, "删除");
        sidebar_navigation_button(
            SharedString::from(format!("db-saved-{id}")),
            SharedString::from(menu_name.clone()),
            Icon::new(DuckIcon::FileText)
                .size(extended.icon.md)
                .text_color(tokens.colors.muted_foreground),
            div().flex_none(),
            false,
            cx,
        )
        .on_click(move |_, window, cx| {
            let Some(entity) = click_view.upgrade() else {
                return;
            };
            let sql = click_sql.clone();
            let name = click_name.clone();
            entity.update(cx, |this, cx| {
                this.load_saved_query(sql, &name, window, cx);
            });
        })
        // 右键三件事与「点一下」不重复：点 = 载入（最常用），
        // 改名 / 删除是低频但要有的维护动作。
        .context_menu(move |menu, _, _| {
            // 外层是 `Fn`（每次重绘都重跑）：每个内层闭包用到的值都得**当场克隆**，
            // 从捕获的环境里搬走一次之后就没得搬了。
            let load_view = menu_view.clone();
            let load_sql = menu_sql.clone();
            let load_name = menu_name.clone();
            let rename_view = menu_view.clone();
            let rename_id = menu_id.clone();
            let rename_name = menu_name.clone();
            let delete_view = menu_view.clone();
            let delete_id = menu_id.clone();
            let delete_name = menu_name.clone();
            menu.item(
                PopupMenuItem::new(load_label).on_click(move |_, window, cx| {
                    let Some(entity) = load_view.upgrade() else {
                        return;
                    };
                    entity.update(cx, |this, cx| {
                        this.load_saved_query(load_sql.clone(), &load_name, window, cx);
                    });
                }),
            )
            .item(
                PopupMenuItem::new(rename_label).on_click(move |_, window, cx| {
                    let Some(entity) = rename_view.upgrade() else {
                        return;
                    };
                    entity.update(cx, |this, cx| {
                        this.open_rename_query_dialog(&rename_id, &rename_name, window, cx);
                    });
                }),
            )
            .item(
                PopupMenuItem::new(delete_label).on_click(move |_, window, cx| {
                    let Some(entity) = delete_view.upgrade() else {
                        return;
                    };
                    entity.update(cx, |this, cx| {
                        this.delete_saved_query(&delete_id, &delete_name, window, cx);
                    });
                }),
            )
        })
        .into_any_element()
    }

    /// 左栏「仪表板」分区：一项 = 一块 12 栏网格 + 它引用的面板。
    ///
    /// 点一下 = 打开（主区切到网格），右键 = 重命名 / 复制 / 删除。与「已保存图表」并列
    /// 而不是互相替代：图表是**一块**看法，仪表板是**一屏**写照。
    pub(super) fn sidebar_dashboards_section(
        &self,
        conn_id: &str,
        query: &str,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let key = format!("sec:boards:{conn_id}");
        let open = !query.is_empty() || self.tree_open.contains(&key);
        let hits: Vec<&Dashboard> = self
            .dashboards
            .iter()
            .filter(|board| object_matches(&board.name, query))
            .collect();
        let mut section = v_flex().w_full().gap(tokens.spacing.xxs).child(
            self.sidebar_toggle_row(
                format!("db-section-boards-{conn_id}"),
                key,
                DuckIcon::Columns2,
                tr(cx, "仪表板"),
                Some(hits.len()),
                self.active_tab_kind(conn_id)
                    .is_some_and(|kind| matches!(kind, TabKind::Dashboard(_))),
                open,
                cx,
            ),
        );
        if !open {
            return section.into_any_element();
        }
        let mut children = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .pl(tokens.spacing.md);

        // 「新建」放在列表最前：它是这一段的入口动作，混在条目里会看漏。
        children = children.child(
            sidebar_navigation_button(
                SharedString::from(format!("db-dashboard-new-{conn_id}")),
                SharedString::from(tr(cx, "新建仪表板")),
                Icon::new(DuckIcon::Plus)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.muted_foreground),
                div().flex_none(),
                false,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let Some(entity) = view.upgrade() else {
                        return;
                    };
                    entity.update(cx, |this, cx| {
                        this.open_dashboard_name_dialog(None, String::new(), window, cx);
                    });
                }
            }),
        );
        if hits.is_empty() {
            children = children.child(
                div()
                    .w_full()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(meta_text(tr(cx, "还没有仪表板。"), cx)),
            );
        }
        for board in hits {
            let id = board.id;
            let name = board.name.clone();
            let count = board.panels.len();
            // 三类闭包各持一份句柄：`on_click` 与 `context_menu` 都会把捕获值 move 走。
            let click_view = view.clone();
            let menu_view = view.clone();
            let rename_name = name.clone();
            let delete_name = name.clone();
            // 连接 id 也要进闭包（点开时先激活这条连接），同样得自己持一份。
            let click_conn = conn_id.to_owned();
            let open_conn = conn_id.to_owned();
            let open_label = tr(cx, "打开");
            let rename_label = tr(cx, "重命名");
            let duplicate_label = tr(cx, "复制");
            let delete_label = tr(cx, "删除");
            children = children.child(
                sidebar_navigation_button(
                    SharedString::from(format!("db-board-{id}")),
                    SharedString::from(name.clone()),
                    Icon::new(DuckIcon::Columns2)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
                    div()
                        .flex_none()
                        .child(meta_text(SharedString::from(count.to_string()), cx)),
                    self.active_tab_kind(conn_id) == Some(&TabKind::Dashboard(id)),
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let Some(entity) = click_view.upgrade() else {
                        return;
                    };
                    // 先激活**这条**连接：点的是连接节点下的一块板，意思就是「用这条连接看」，
                    // 不能拿另一条活跃连接的会话去跑面板里的 SQL（与库节点行同口径）。
                    // `open_dashboard` 自己会复用激活 / 新开仪表板标签。
                    entity.update(cx, |this, cx| {
                        this.open_dashboard(&click_conn, id, window, cx);
                    });
                })
                .context_menu(move |menu, _, _| {
                    // 外层是 `Fn`（每次重绘都重跑）：用到的值都得**当场克隆**，
                    // 从捕获的环境里搬走一次之后就没得搬了。
                    let open_view = menu_view.clone();
                    let rename_view = menu_view.clone();
                    let duplicate_view = menu_view.clone();
                    let delete_view = menu_view.clone();
                    let open_conn = open_conn.clone();
                    let rename_name = rename_name.clone();
                    let delete_name = delete_name.clone();
                    menu.item(
                        PopupMenuItem::new(open_label).on_click(move |_, window, cx| {
                            let Some(entity) = open_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.open_dashboard(&open_conn, id, window, cx);
                            });
                        }),
                    )
                    .item(
                        PopupMenuItem::new(rename_label).on_click(move |_, window, cx| {
                            let Some(entity) = rename_view.upgrade() else {
                                return;
                            };
                            let name = rename_name.clone();
                            entity.update(cx, |this, cx| {
                                this.open_dashboard_name_dialog(Some(id), name, window, cx);
                            });
                        }),
                    )
                    .item(
                        PopupMenuItem::new(duplicate_label).on_click(move |_, _, cx| {
                            let Some(entity) = duplicate_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| this.duplicate_dashboard(id, cx));
                        }),
                    )
                    .item(
                        PopupMenuItem::new(delete_label).on_click(move |_, window, cx| {
                            let Some(entity) = delete_view.upgrade() else {
                                return;
                            };
                            let name = delete_name.clone();
                            entity.update(cx, |this, cx| {
                                this.delete_dashboard(id, &name, window, cx);
                            });
                        }),
                    )
                }),
            );
        }
        section = section.child(children);
        section.into_any_element()
    }

    /// 左栏「已保存图表」分区：一项 = 一条 SQL + 一套列映射。
    ///
    /// 点一下 = 载入（SQL 放回编辑器 + 切到图表视角），右键 = 重命名 / 删除。与「已保存
    /// 查询」是并列的两份清单而不是互相替代：查询给的是**数据**，图表给的是**看法**。
    pub(super) fn sidebar_saved_charts_section(
        &self,
        conn_id: &str,
        query: &str,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let key = format!("sec:charts:{conn_id}");
        let open = !query.is_empty() || self.tree_open.contains(&key);
        let hits: Vec<&SavedChart> = self
            .saved_charts
            .iter()
            .filter(|saved| object_matches(&saved.name, query))
            .collect();
        let mut section = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .child(self.sidebar_toggle_row(
                format!("db-section-charts-{conn_id}"),
                key,
                DuckIcon::ChartLine,
                tr(cx, "已保存图表"),
                Some(hits.len()),
                false,
                open,
                cx,
            ));
        if !open {
            return section.into_any_element();
        }
        let mut children = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .pl(tokens.spacing.md);
        if hits.is_empty() {
            children = children.child(
                div()
                    .w_full()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(meta_text(tr(cx, "还没有保存的图表。"), cx)),
            );
        }
        for saved in hits {
            let id = saved.id.clone();
            let name = saved.name.clone();
            let kind = saved.kind;
            // 两类闭包各持一份句柄（与已保存查询同因：`on_click` 与 `context_menu` 都会
            // 把捕获值 move 走）。
            let click_view = view.clone();
            let menu_view = view.clone();
            let click_id = id.clone();
            let click_name = name.clone();
            let menu_id = id.clone();
            let menu_name = name.clone();
            let load_label = tr(cx, "载入图表");
            let rename_label = tr(cx, "重命名");
            let delete_label = tr(cx, "删除");
            children = children.child(
                sidebar_navigation_button(
                    SharedString::from(format!("db-chart-{id}")),
                    SharedString::from(name.clone()),
                    Icon::new(DuckIcon::ChartLine)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
                    div().flex_none().child(meta_text(kind.heading(cx), cx)),
                    false,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let Some(entity) = click_view.upgrade() else {
                        return;
                    };
                    let id = click_id.clone();
                    let name = click_name.clone();
                    entity.update(cx, |this, cx| {
                        this.load_saved_chart(&id, &name, window, cx);
                    });
                })
                .context_menu(move |menu, _, _| {
                    let load_view = menu_view.clone();
                    let load_id = menu_id.clone();
                    let load_name = menu_name.clone();
                    let rename_view = menu_view.clone();
                    let rename_id = menu_id.clone();
                    let rename_name = menu_name.clone();
                    let delete_view = menu_view.clone();
                    let delete_id = menu_id.clone();
                    let delete_name = menu_name.clone();
                    menu.item(
                        PopupMenuItem::new(load_label).on_click(move |_, window, cx| {
                            let Some(entity) = load_view.upgrade() else {
                                return;
                            };
                            let id = load_id.clone();
                            let name = load_name.clone();
                            entity.update(cx, |this, cx| {
                                this.load_saved_chart(&id, &name, window, cx);
                            });
                        }),
                    )
                    .item(
                        PopupMenuItem::new(rename_label).on_click(move |_, window, cx| {
                            let Some(entity) = rename_view.upgrade() else {
                                return;
                            };
                            let id = rename_id.clone();
                            let name = rename_name.clone();
                            entity.update(cx, |this, cx| {
                                this.open_rename_chart_dialog(&id, &name, window, cx);
                            });
                        }),
                    )
                    .item(
                        PopupMenuItem::new(delete_label).on_click(move |_, window, cx| {
                            let Some(entity) = delete_view.upgrade() else {
                                return;
                            };
                            let id = delete_id.clone();
                            let name = delete_name.clone();
                            entity.update(cx, |this, cx| {
                                this.delete_saved_chart(&id, &name, window, cx);
                            });
                        }),
                    )
                }),
            );
        }
        section = section.child(children);
        section.into_any_element()
    }

    /// 左栏「连接节点下」的整棵子树：功能入口 + 数据库树。
    ///
    /// 形态对齐 DBFlux（仪表盘 / 已保存查询 / 已保存图表 / 数据库 / 实例概览 /
    /// 实例指标 / 实例检查器）。后三项按 [`DatabaseKind::supports_instance_insight`]
    /// 决定画不画：引擎没这个能力就不给入口，而不是画一个点开报「不支持」的按钮
    /// ——「不支持」和「执行失败」必须分得开。
    pub(super) fn render_connection_tree(
        &self,
        id: &str,
        session: &Session,
        query: &str,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        // ── 数据库：命名空间树（库 → 分类 → 对象）就是它的子级 ──
        let mut names = collect_namespaces(&session.databases, &session.schemas);
        // 「选择显示数据库」的白名单：设了就只显示勾选的那些。只作用于**侧栏树**
        // —— 命名空间下拉 / 数据传输 / 对比那些入口是「换个库干活」的工具，
        // 不该被浏览偏好顺手藏掉。
        let whitelist = self
            .connections
            .iter()
            .find(|cfg| cfg.id == id)
            .and_then(|cfg| cfg.visible_databases.as_ref());
        if let Some(whitelist) = whitelist {
            names.retain(|name| whitelist.contains(name));
        }
        // 计数要在循环把它 move 走之前取：下面「数据库」那行的尾巴挂的是这个数。
        let namespace_count = names.len();
        let namespace_tree = {
            let layer_is_database = namespace_is_database(session.kind);
            let current = if layer_is_database {
                session.database.clone()
            } else {
                session.schema.clone()
            };
            let mut databases = v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                // 缩进一格：视觉上是连接节点的子级（与图标色一起承担层级）。
                .pl(tokens.spacing.md);
            for name in names {
                let selected = current.as_deref() == Some(name.as_str());
                // 搜索态一律展开：命中项要看得见（与分类节点的口径一致）。
                let node_key = format!("db:{id}:{name}");
                let open = !query.is_empty() || self.tree_open.contains(&node_key);
                let row = view.clone();
                let connection = id.to_owned();
                let target = name.clone();
                // 右键菜单自用的一份（`on_click` 会把上面两份 move 走）。
                let menu_connection = id.to_owned();
                let menu_target = name.clone();
                let menu_node_key = node_key.clone();
                let toggle_view = view.clone();
                let toggle_key = node_key.clone();
                let toggle_connection = id.to_owned();
                let toggle_target = name.clone();
                let chevron = if open {
                    DuckIcon::ChevronDown
                } else {
                    DuckIcon::ChevronRight
                };
                let trailing = h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        icon_button(
                            SharedString::from(format!("db-namespace-toggle-{id}-{name}")),
                            if open {
                                tr(cx, "收起")
                            } else {
                                tr(cx, "展开")
                            },
                            Icon::new(chevron)
                                .size(extended.icon.sm)
                                .text_color(tokens.colors.muted_foreground),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        // 箭头只切换折叠，**不切库**：展开另一个库看一眼结构
                        // 不该把当前会话搬过去（真正切库是点整行）。
                        .on_click(move |_, window, cx| {
                            let Some(entity) = toggle_view.upgrade() else {
                                return;
                            };
                            let key = toggle_key.clone();
                            let connection = toggle_connection.clone();
                            let target = toggle_target.clone();
                            let was_closed = !entity.read(cx).tree_open.contains(&key);
                            entity.update(cx, |this, cx| {
                                if !this.tree_open.remove(&key) {
                                    this.tree_open.insert(key);
                                }
                                // 首次展开 + 不是当前库：顺手切过去，否则树里
                                // 没有这个库的目录可显示（目录只属于当前命名空间）。
                                if was_closed && layer_is_database {
                                    this.activate_conn(&connection, window, cx);
                                    this.select_database(target, window, cx);
                                } else if was_closed {
                                    this.activate_conn(&connection, window, cx);
                                    this.select_schema(Some(target), cx);
                                }
                                cx.notify();
                            });
                            // 箭头是行内**嵌套按钮**：gpui 的 click 监听在冒泡期派发且
                            // 不自动截停，不截停的话外层行按钮也会收到这次点击——
                            // 它无条件 `tree_open.insert(node_key)`，刚收起的库又被
                            // 铺开（「展开后无法折叠」的根因）。
                            cx.stop_propagation();
                        }),
                    )
                    .children(selected.then(|| {
                        Icon::new(DuckIcon::Check)
                            .size(extended.icon.sm)
                            .text_color(extended.colors.accent_text)
                    }));
                databases = databases.child(
                    sidebar_navigation_button(
                        SharedString::from(format!("db-namespace-{id}-{name}")),
                        SharedString::from(name.clone()),
                        Icon::new(DuckIcon::Layers)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                        trailing,
                        selected,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let Some(entity) = row.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            // 切库前先确保操作的是**这条**连接的会话：
                            // 左栏可能在浏览另一条连接的库列表。
                            this.activate_conn(&connection, window, cx);
                            // 每个库一张**库标签**（同库复用、不重置已有草稿）：
                            // 「点几个库就多几个 tab」从这里兑现。
                            this.open_database_tab(&connection, target.clone(), window, cx);
                            if layer_is_database {
                                this.select_database(target.clone(), window, cx);
                            } else {
                                this.select_schema(Some(target.clone()), cx);
                            }
                            // 点整行 = 选中 + 铺开：用户点库名的意思就是「进去看看」，
                            // 不该再要求他去点右边那个小箭头。
                            this.tree_open.insert(node_key.clone());
                            cx.notify();
                        });
                    })
                    // 库节点右键（dbx 同款，每项真实后端）：断开 / 复制名称 /
                    // 查看资源对象 / 新建查询 / 设为默认数据库 / 刷新。
                    .context_menu({
                        let view = view.clone();
                        let menu_conn = menu_connection.clone();
                        let menu_target = menu_target.clone();
                        let menu_key = menu_node_key.clone();
                        let is_selected = selected;
                        let is_database_layer = layer_is_database;
                        let disconnect_label = tr(cx, "断开连接");
                        let copy_name_label = tr(cx, "复制名称");
                        let copied_name_note = tr(cx, "已复制名称。");
                        let browse_label = tr(cx, "查看资源对象");
                        let new_query_label = tr(cx, "新建查询");
                        let default_label = tr(cx, "设为默认数据库");
                        let refresh_label = tr(cx, "刷新");
                        // dbx 对位的三张工具页：ER 关系图 / 比较数据 / 数据传输，
                        // 全部以「这个库」为源打开。
                        let er_label = tr(cx, "ER 关系图");
                        let compare_label = tr(cx, "比较数据…");
                        let transfer_label = tr(cx, "数据传输…");
                        // ── 库节点管理（dbx 对位）──
                        // 建库 / 删库只给四家 SQL 引擎：SQLite 的库就是文件（没有这条
                        // DDL），Mongo / ES / agent 轨没有这条概念（agent 档故意不做
                        // DDL）；导出库结构另含 SQLite（五家的表 DDL 都有出口）。
                        let ns_kind = session.kind;
                        let ns_ddl_kind = matches!(
                            ns_kind,
                            DatabaseKind::Mysql
                                | DatabaseKind::Postgres
                                | DatabaseKind::SqlServer
                                | DatabaseKind::ClickHouse
                        );
                        let ns_export_kind =
                            ns_ddl_kind || ns_kind == DatabaseKind::Sqlite;
                        let has_clipboard = self.table_clipboard.is_some();
                        let ns_menu_any =
                            ns_ddl_kind || ns_export_kind || has_clipboard;
                        let create_ns_label = if is_database_layer {
                            tr(cx, "新建数据库…")
                        } else {
                            tr(cx, "新建 Schema…")
                        };
                        let ns_name_label = if is_database_layer {
                            tr(cx, "库名")
                        } else {
                            tr(cx, "Schema 名")
                        };
                        let drop_ns_label = if is_database_layer {
                            tr(cx, "删除数据库…")
                        } else {
                            tr(cx, "删除 Schema…")
                        };
                        // 删的就是**这个节点自己的名字**：语句在渲染期拼好（名字走
                        // 引擎自己的引号）；引号拿不到（未连上）就不给项。
                        let drop_ns_sql = match (ns_ddl_kind, session.quote) {
                            (true, Some(q)) => duck_db::drivers::quote_identifier(
                                menu_target.as_str(),
                                q,
                            )
                            .ok()
                            .map(|ident| {
                                if is_database_layer {
                                    format!("DROP DATABASE {ident};")
                                } else {
                                    format!("DROP SCHEMA {ident};")
                                }
                            }),
                            _ => None,
                        };
                        let ns_quote = session.quote.unwrap_or('"');
                        let export_ns_label = tr(cx, "导出库结构");
                        let paste_label = tr(cx, "粘贴表");
                        let drop_ns_note = tr(cx, "已生成删除语句；确认无误后再执行。");
                        move |mut menu, _window, _cx| {
                            let view = view.clone();
                            {
                                let disconnect_view = view.clone();
                                let disconnect_id = menu_conn.clone();
                                menu = menu.item(PopupMenuItem::new(disconnect_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = disconnect_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.close_tab(&disconnect_id, window, cx);
                                        });
                                    },
                                ));
                            }
                            menu = menu.separator();
                            {
                                let copy_view = view.clone();
                                let name = menu_target.clone();
                                menu = menu.item(PopupMenuItem::new(copy_name_label).on_click(
                                    move |_, _, cx| {
                                        let Some(entity) = copy_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.put_on_clipboard(
                                                name.clone(),
                                                copied_name_note,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            {
                                // 查看资源对象 = 铺开这个库节点（与点行名的铺开一致，
                                // 但不切库 —— 看别的库的结构不该搬走当前会话）。
                                let browse_view = view.clone();
                                let browse_key = menu_key.clone();
                                menu = menu.item(PopupMenuItem::new(browse_label).on_click(
                                    move |_, _, cx| {
                                        let Some(entity) = browse_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.tree_open.insert(browse_key.clone());
                                            cx.notify();
                                        });
                                    },
                                ));
                            }
                            menu = menu.separator();
                            {
                                let query_view = view.clone();
                                let query_conn = menu_conn.clone();
                                let query_target = menu_target.clone();
                                let query_layer = is_database_layer;
                                menu = menu.item(PopupMenuItem::new(new_query_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = query_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.activate_conn(&query_conn, window, cx);
                                            // 先落进这个库再开查询：新标签的语句就该
                                            // 打在这个库里。
                                            if query_layer {
                                                this.select_database(
                                                    query_target.clone(),
                                                    window,
                                                    cx,
                                                );
                                            } else {
                                                this.select_schema(Some(query_target.clone()), cx);
                                            }
                                            this.open_tab(&query_conn, window, cx);
                                        });
                                    },
                                ));
                            }
                            {
                                // 设为默认数据库 = 把选中态持久化（`select_database` /
                                // `select_schema` 会写回档案的默认库 / schema 字段，
                                // 下次连接直接落在这里）。已经是默认就置灰。
                                let default_view = view.clone();
                                let default_conn = menu_conn.clone();
                                let default_target = menu_target.clone();
                                let default_layer = is_database_layer;
                                menu = menu.item(
                                    PopupMenuItem::new(default_label)
                                        .disabled(is_selected)
                                        .on_click(move |_, window, cx| {
                                            let Some(entity) = default_view.upgrade() else {
                                                return;
                                            };
                                            entity.update(cx, |this, cx| {
                                                this.activate_conn(&default_conn, window, cx);
                                                if default_layer {
                                                    this.select_database(
                                                        default_target.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                } else {
                                                    this.select_schema(
                                                        Some(default_target.clone()),
                                                        cx,
                                                    );
                                                }
                                            });
                                        }),
                                );
                            }
                            menu = menu.separator();
                            {
                                // ER 关系图：源就是这个库。
                                let er_view = view.clone();
                                let er_conn = menu_conn.clone();
                                let er_target = menu_target.clone();
                                menu = menu.item(PopupMenuItem::new(er_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = er_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.open_er_tab(&er_conn, &er_target, window, cx);
                                        });
                                    },
                                ));
                            }
                            {
                                // 比较数据：源 = 当前连接 + 这个库；目标在页面上选。
                                let compare_view = view.clone();
                                let compare_conn = menu_conn.clone();
                                let compare_target = menu_target.clone();
                                menu = menu.item(PopupMenuItem::new(compare_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = compare_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.open_compare_tab(
                                                &compare_conn,
                                                &compare_target,
                                                Vec::new(),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            {
                                // 数据传输：源 = 当前连接 + 这个库的全部表；目标在页面上选。
                                let transfer_view = view.clone();
                                let transfer_conn = menu_conn.clone();
                                let transfer_target = menu_target.clone();
                                menu = menu.item(PopupMenuItem::new(transfer_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = transfer_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            // 表清单当场从源目录拍下来（只搬基础表）。
                                            let tables: Vec<TableRef> = this
                                                .sessions
                                                .get(&transfer_conn)
                                                .map(|session| {
                                                    session
                                                        .catalog
                                                        .iter()
                                                        .filter(|entry| {
                                                            entry.kind == TableKind::Table
                                                        })
                                                        .filter(|entry| {
                                                            entry.schema.as_deref()
                                                                == Some(transfer_target.as_str())
                                                        })
                                                        .map(|entry| entry.table_ref())
                                                        .collect()
                                                })
                                                .unwrap_or_default();
                                            this.open_transfer_tab(
                                                &transfer_conn,
                                                &transfer_target,
                                                tables,
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            menu = menu.separator();
                            if ns_menu_any {
                                // 新建数据库 / Schema：弹窗取名 → CREATE 语句进编辑器
                                // 过人眼（与重命名 / 克隆同一条仓库纪律）。
                                if ns_ddl_kind {
                                    let create_view = view.clone();
                                    let create_conn = menu_conn.clone();
                                    let create_title = create_ns_label;
                                    let create_field = ns_name_label;
                                    let create_layer = is_database_layer;
                                    let create_quote = ns_quote;
                                    menu = menu.item(
                                        PopupMenuItem::new(create_ns_label).on_click(
                                            move |_, window, cx| {
                                                let Some(entity) = create_view.upgrade() else {
                                                    return;
                                                };
                                                entity.update(cx, |this, cx| {
                                                    this.activate_conn(
                                                        &create_conn,
                                                        window,
                                                        cx,
                                                    );
                                                    this.open_text_prompt_dialog(
                                                        create_title,
                                                        create_field,
                                                        tr(cx, "生成语句"),
                                                        Rc::new(
                                                            move |name: &str,
                                                                  this: &mut DatabasePage,
                                                                  window: &mut Window,
                                                                  cx: &mut Context<DatabasePage>| {
                                                                let Ok(ident) = duck_db::drivers::quote_identifier(name, create_quote) else {
                                                                    notify::warning(
                                                                        tr(cx, "名称包含非法字符。"),
                                                                        window,
                                                                        cx,
                                                                    );
                                                                    return false;
                                                                };
                                                                let sql = if create_layer {
                                                                    format!("CREATE DATABASE {ident};")
                                                                } else {
                                                                    format!("CREATE SCHEMA {ident};")
                                                                };
                                                                this.stage_sql(
                                                                    sql,
                                                                    tr(cx, "已生成创建语句；确认无误后再执行。").into(),
                                                                    window,
                                                                    cx,
                                                                );
                                                                true
                                                            },
                                                        ),
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            },
                                        ),
                                    );
                                    // 删除这个库 / Schema：DROP 语句进编辑器过人眼，
                                    // 不直接执行。
                                    if let Some(drop_sql) = drop_ns_sql.clone() {
                                        let drop_view = view.clone();
                                        let drop_conn = menu_conn.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(drop_ns_label).on_click(
                                                move |_, window, cx| {
                                                    let Some(entity) = drop_view.upgrade()
                                                    else {
                                                        return;
                                                    };
                                                    entity.update(cx, |this, cx| {
                                                        this.activate_conn(
                                                            &drop_conn,
                                                            window,
                                                            cx,
                                                        );
                                                        this.stage_sql(
                                                            drop_sql.clone(),
                                                            drop_ns_note.into(),
                                                            window,
                                                            cx,
                                                        );
                                                    });
                                                },
                                            ),
                                        );
                                    }
                                }
                                // 导出库结构：全库基础表的 DDL 拼成一份 .sql 落盘。
                                if ns_export_kind {
                                    let export_view = view.clone();
                                    let export_conn = menu_conn.clone();
                                    let export_target = menu_target.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(export_ns_label).on_click(
                                            move |_, _, cx| {
                                                let Some(entity) = export_view.upgrade() else {
                                                    return;
                                                };
                                                entity.update(cx, |this, cx| {
                                                    this.export_namespace_ddl(
                                                        &export_conn,
                                                        &export_target,
                                                        cx,
                                                    );
                                                });
                                            },
                                        ),
                                    );
                                }
                                // 粘贴表：表剪贴板里有东西才给项（表节点「复制表」存的）。
                                if has_clipboard {
                                    let paste_view = view.clone();
                                    let paste_conn = menu_conn.clone();
                                    let paste_target = menu_target.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(paste_label).on_click(
                                            move |_, window, cx| {
                                                let Some(entity) = paste_view.upgrade() else {
                                                    return;
                                                };
                                                entity.update(cx, |this, cx| {
                                                    this.paste_table(
                                                        &paste_conn,
                                                        &paste_target,
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            },
                                        ),
                                    );
                                }
                            }
                            menu = menu.separator();
                            {
                                let refresh_view = view.clone();
                                let refresh_id = menu_conn.clone();
                                menu = menu.item(PopupMenuItem::new(refresh_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = refresh_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.activate_conn(&refresh_id, window, cx);
                                            this.refresh_state(window, cx);
                                        });
                                    },
                                ));
                            }
                            menu
                        }
                    }),
                );
                // 展开且**是当前命名空间**才铺分类：目录只对内省过的那个命名空间
                // 有效，拿别的库的目录去渲染它的分类会张冠李戴。
                if open && selected {
                    for category in TreeCategory::ALL {
                        if let Some(node) =
                            self.render_tree_category(id, &name, session, category, query, cx)
                        {
                            databases = databases.child(node);
                        }
                    }
                    // 「查询」：保存的查询（SQL 库）排在分类之后，与「表 / 视图 / …」
                    // 同层挂在**库节点**下（清单里最后一项就是它）。
                    databases = databases.child(self.sidebar_saved_queries_section(id, query, cx));
                }
            }
            databases
        };

        let db_key = format!("sec:db:{id}");
        // 搜索态一律展开：命中项要在树上看得见（与库节点 / 分类节点同口径）。
        let db_open = !query.is_empty() || self.tree_open.contains(&db_key);

        let mut column = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            // 缩进一格：视觉上是连接节点的子级（与图标色一起承担层级）。
            .pl(tokens.spacing.md)
            // 仪表盘：只用手上已有的数据（对象构成 + 最近查询），不额外打服务器。
            .child(self.sidebar_instance_row(id, InstanceTab::Dashboard, cx))
            .child(self.sidebar_saved_charts_section(id, query, cx))
            .child(self.sidebar_dashboards_section(id, query, cx))
            .child(
                self.sidebar_toggle_row(
                    format!("db-section-databases-{id}"),
                    db_key,
                    DuckIcon::Database,
                    tr(cx, "数据库"),
                    Some(namespace_count),
                    // 活动标签属于这条连接、且不是实例标签时算选中：实例页讲的是服务器，
                    // 库 / 表讲的是数据（查询 / 库 / 数据 / 仪表板标签都算「在看数据」）。
                    !self
                        .active_tab_kind(id)
                        .is_some_and(|kind| matches!(kind, TabKind::Instance(_))),
                    db_open,
                    cx,
                ),
            );
        if db_open {
            column = column.child(namespace_tree);
        }
        // 实例三档：只有读服务器数据的引擎才画（SQLite 也支持——它的「服务器」就是文件）。
        // 概览是单页事实表，保持扁平行；指标与检查器照 DBFlux 摆成可展开段，
        // 子级分别是各数值指标与进程列表（见 `sidebar_metrics_section` 的注释）。
        if session.kind.supports_instance_insight() {
            for tab in InstanceTab::ALL
                .into_iter()
                .filter(|tab| tab.fetches_server())
            {
                match tab {
                    InstanceTab::Metrics => {
                        column = column.child(self.sidebar_metrics_section(id, query, cx));
                    }
                    InstanceTab::Inspector => {
                        column = column.child(self.sidebar_inspector_section(id, cx));
                    }
                    _ => column = column.child(self.sidebar_instance_row(id, tab, cx)),
                }
            }
        }
        column.into_any_element()
    }

    pub(super) fn render_tree_category(
        &self,
        conn_id: &str,
        namespace: &str,
        session: &Session,
        category: TreeCategory,
        needle: &str,
        cx: &App,
    ) -> Option<AnyElement> {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let objects: Vec<&CatalogTable> = session
            .catalog
            .iter()
            .filter(|entry| category.matches(entry.kind))
            .filter(|entry| object_matches(&entry.name, needle))
            .collect();
        // 置顶优先：被钉住的对象排在本分类最前，其余保持目录原序（稳定排序）。
        let mut objects = objects;
        if !self.pinned_tables.is_empty() {
            objects.sort_by_key(|entry| {
                !self.pinned_tables.contains(&format!(
                    "{conn_id}:{}:{}",
                    entry.schema.as_deref().unwrap_or(""),
                    entry.name
                ))
            });
        }
        let database_objects: Vec<&DatabaseObject> = session
            .database_objects(category)
            .iter()
            .filter(|object| object_matches(&object.name, needle))
            .collect();
        let count = objects.len() + database_objects.len();
        // 概念存在但清单为空的分类也画（DBX 同款「视图 0」）：数得出「没有」比节点
        // 凭空消失更可预期。引擎根本没有这个概念的（PG 的事件、SQLite 的用户…）
        // 才不画 —— 那是点开必为空的哑节点（判定见 `TreeCategory::concept_supported`）。
        if count == 0 && !category.concept_supported(session.kind) {
            return None;
        }
        let key = format!("cat:{conn_id}:{namespace}:{}", category.key());
        // 搜索态一律展开：用户打了词就是想看见命中项，还得再点一次「展开」是多余动作。
        let open = !needle.is_empty() || self.tree_open.contains(&key);

        let toggle_view = self.view.clone();
        let toggle_key = key.clone();
        let chevron = if open {
            DuckIcon::ChevronDown
        } else {
            DuckIcon::ChevronRight
        };
        let mut children = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .pl(tokens.spacing.md);
        for entry in objects {
            children = children.child(self.sidebar_object_row(conn_id, entry.clone(), cx));
        }
        for object in database_objects {
            children = children.child(self.sidebar_database_object_row(
                object.clone(),
                category,
                conn_id,
                namespace,
                cx,
            ));
        }

        // 计数 + 展开箭头一起放行尾：数字告诉你这一分类有多少个对象，箭头告诉你
        // **能展开**（分类图标留给「这是哪一类」，两者不抢同一个位置）。
        let trailing = h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(meta_text(count.to_string(), cx))
            .child(
                Icon::new(chevron)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
            );

        // 缩进一格：分类是库节点的子级，不缩会跟库名排在同一列（层级全丢）。
        let mut node = v_flex()
            .w_full()
            .pl(tokens.spacing.md)
            .gap(tokens.spacing.xxs)
            .child(
                // 分类行故意**不**用选中态：它不是「当前对象」，只是一个折叠头。
                sidebar_navigation_button(
                    SharedString::from(format!(
                        "db-category-{conn_id}-{namespace}-{}",
                        category.key()
                    )),
                    SharedString::from(category.heading(cx)),
                    Icon::new(category.icon())
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    trailing,
                    false,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let Some(entity) = toggle_view.upgrade() else {
                        return;
                    };
                    let key = toggle_key.clone();
                    entity.update(cx, |this, cx| {
                        if !this.tree_open.remove(&key) {
                            this.tree_open.insert(key);
                        }
                        cx.notify();
                    });
                })
                // 分类节点右键（dbx 同款，每项真实后端）：表分类给「新建表 / 导入
                // 数据」，视图分类给「新建视图」，全部分类都有展开 / 过滤器 / 刷新。
                .context_menu({
                    let view = self.view.clone();
                    let menu_conn = conn_id.to_owned();
                    let menu_ns = namespace.to_owned();
                    let menu_key = key.clone();
                    let is_open = open;
                    let is_table = category == TreeCategory::Table;
                    let is_view = category == TreeCategory::View;
                    // 骨架与导入只对带标识符引号的 SQL 引擎有意义。
                    let has_sql_ident = session.quote.is_some();
                    // 表分类：**直接开「新建表」标签**（dbx 同款的可视化网格建表），
                    // 不再生成骨架；视图与其余七类仍走 CREATE 骨架。命名空间按层
                    // 挂进去（SQLite 没有这层概念）。
                    let create_schema: Option<String> = (session.kind != DatabaseKind::Sqlite
                        && !namespace.is_empty())
                    .then(|| namespace.to_owned());
                    let opens_new_table = is_table && has_sql_ident;
                    // CREATE 骨架：表名 / 列名都走引擎自己的引号（`quote_identifier`），
                    // 命名空间按层挂进去 —— 生成出来就能直接执行。
                    let create_sql: Option<String> = if is_table {
                        None
                    } else {
                        (has_sql_ident && is_view)
                            .then(|| {
                                let quote = session.quote.unwrap_or('"');
                                let Ok(qualified) = duck_db::drivers::quote_qualified(
                                    &TableRef {
                                        schema: create_schema.clone(),
                                        name: "new_view".to_owned(),
                                    },
                                    quote,
                                ) else {
                                    return None;
                                };
                                Some(format!("CREATE VIEW {qualified} AS\nSELECT 1;"))
                            })
                            .flatten()
                            // 表 / 视图之外的七类（索引 / 过程 / 函数 / 触发器 / 事件 /
                            // 序列 / 用户）：每类一条语法正确的 CREATE 骨架；引擎给不出的
                            // （事件只有 MySQL、序列只有 PG / SQL Server、用户体系 SQLite /
                            // ClickHouse 没有）返回 None，菜单不给项。
                            .or_else(|| create_category_skeleton(category, session.kind))
                    };
                    let toggle_label = if is_open {
                        tr(cx, "收起")
                    } else {
                        tr(cx, "展开全部")
                    };
                    let create_label = match category {
                        TreeCategory::Table => tr(cx, "新建表"),
                        TreeCategory::View => tr(cx, "新建视图"),
                        TreeCategory::Index => tr(cx, "新建索引"),
                        TreeCategory::Procedure => tr(cx, "新建存储过程"),
                        TreeCategory::Function => tr(cx, "新建函数"),
                        TreeCategory::Trigger => tr(cx, "新建触发器"),
                        TreeCategory::Event => tr(cx, "新建事件"),
                        TreeCategory::Sequence => tr(cx, "新建序列"),
                        TreeCategory::User => tr(cx, "新建用户"),
                    };
                    let create_note = if is_view {
                        tr(cx, "已把建视图骨架放进编辑器。")
                    } else {
                        tr(cx, "已把创建骨架放进编辑器。")
                    };
                    let import_label = tr(cx, "导入数据…");
                    // 「粘贴表」：表剪贴板里有东西才给项（表节点「复制表」存的）。
                    let paste_label = tr(cx, "粘贴表");
                    let has_clipboard = self.table_clipboard.is_some();
                    let filter_label = tr(cx, "过滤器…");
                    let refresh_label = tr(cx, "刷新");
                    move |mut menu, _window, _cx| {
                        let view = view.clone();
                        // 表分类：开「新建表」标签（永不复用，每点一次新开一个草稿）。
                        if opens_new_table {
                            let create_view = view.clone();
                            let create_conn = menu_conn.clone();
                            let create_ns = create_schema.clone();
                            menu = menu.item(PopupMenuItem::new(create_label).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = create_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.open_new_table_tab(
                                            &create_conn,
                                            create_ns.clone(),
                                            window,
                                            cx,
                                        );
                                    });
                                },
                            ));
                        } else if let Some(sql) = create_sql.clone() {
                            let create_view = view.clone();
                            let note = create_note;
                            menu = menu.item(PopupMenuItem::new(create_label).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = create_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.stage_sql(sql.clone(), note.into(), window, cx);
                                    });
                                },
                            ));
                        }
                        if is_table && has_sql_ident {
                            // 导入数据：先选文件（异步面板），选完才触发导入 ——
                            // 面板是异步的，回调里不再回读树上的状态。
                            let import_view = view.clone();
                            let import_conn = menu_conn.clone();
                            let import_ns = menu_ns.clone();
                            menu = menu.item(PopupMenuItem::new(import_label).on_click(
                                move |_, _, cx| {
                                    let Some(entity) = import_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |_this, cx| {
                                        let import_conn = import_conn.clone();
                                        let import_ns = import_ns.clone();
                                        crate::file_dialogs::open_file(
                                            cx,
                                            tr(cx, "选择要导入的 CSV 文件"),
                                            move |path, this, cx| {
                                                this.import_csv_new_table(
                                                    &import_conn,
                                                    &import_ns,
                                                    path,
                                                    cx,
                                                );
                                            },
                                        );
                                    });
                                },
                            ));
                        }
                        if is_table && has_clipboard {
                            // 粘贴表：同连接弹窗取名生成 CTAS 过人眼，跨连接开数据传输。
                            let paste_view = view.clone();
                            let paste_conn = menu_conn.clone();
                            let paste_ns = menu_ns.clone();
                            menu = menu.item(PopupMenuItem::new(paste_label).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = paste_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.paste_table(&paste_conn, &paste_ns, window, cx);
                                    });
                                },
                            ));
                        }
                        menu = menu.separator();
                        {
                            let toggle_view = view.clone();
                            let toggle_key = menu_key.clone();
                            menu = menu.item(PopupMenuItem::new(toggle_label).on_click(
                                move |_, _, cx| {
                                    let Some(entity) = toggle_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.tree_open.insert(toggle_key.clone());
                                        cx.notify();
                                    });
                                },
                            ));
                        }
                        {
                            // 过滤器：聚焦左栏搜索框（它同时筛连接与对象）。
                            let filter_view = view.clone();
                            menu = menu.item(PopupMenuItem::new(filter_label).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = filter_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.search.update(cx, |state, cx| {
                                            state.focus(window, cx);
                                        });
                                    });
                                },
                            ));
                        }
                        {
                            let refresh_view = view.clone();
                            let refresh_id = menu_conn.clone();
                            menu = menu.item(PopupMenuItem::new(refresh_label).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = refresh_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.activate_conn(&refresh_id, window, cx);
                                        this.refresh_state(window, cx);
                                    });
                                },
                            ));
                        }
                        menu
                    }
                }),
            );
        // 收起时不画子节点：树里那一堆对象行是**元素**，不是数据显示——留在树上
        // 只是看不见，白白占布局与命中测试。
        if open {
            node = node.child(children);
        }
        Some(node.into_any_element())
    }

    /// 单输入弹窗的**通用骨架**（重命名 / 克隆 / 自增起始值共用）：标题 + 一行
    /// 输入 + 确认。`on_confirm` 返回 `false` = 校验没过，弹窗不关（与「新建分组」
    /// 同一口径）；返回 `true` 关闭。
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub(super) fn open_text_prompt_dialog(
        &mut self,
        title: &'static str,
        field_label: &'static str,
        confirm_label: &'static str,
        on_confirm: Rc<dyn Fn(&str, &mut DatabasePage, &mut Window, &mut Context<Self>) -> bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, field_label));
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let value = input.read(cx).value().trim().to_owned();
                if value.is_empty() {
                    notify::warning(tr(cx, "请填写内容。"), window, cx);
                    return false;
                }
                let mut accepted = true;
                let _ = view.update(cx, |this, cx| {
                    accepted = on_confirm(&value, this, window, cx);
                });
                accepted
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let confirm = Rc::clone(&confirm);
            let input_for_content = input.clone();
            dialog
                .w(px(GROUP_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .gap(form_gap(cx))
                            .child(modal_title_row(
                                "db-text-prompt-title",
                                &drag_for_content,
                                tr(cx, title),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, field_label), &input_for_content)),
                    ))
                })
                .footer(dialog_footer(
                    "db-text-prompt",
                    tr(cx, confirm_label),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |state, cx| state.focus(window, cx));
    }

    /// 「查看 DDL」：真取这张表的建表语句（`Connection::table_ddl`，MySQL / PG /
    /// MSSQL / ClickHouse / SQLite 各有出口）并放进编辑器。
    pub(super) fn view_table_ddl(
        &mut self,
        conn_id: &str,
        table: TableRef,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let conn = session.conn.clone();
        let label = table.display();
        self.status = trf(cx, "正在读取 {} 的 DDL…", &[&label]).into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<String, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.table_ddl(&table)
            },
            move |this, result, window, cx| match result {
                Ok(ddl) => {
                    this.stage_sql(ddl, tr(cx, "已把 DDL 放进编辑器。").into(), window, cx);
                }
                Err(error) => {
                    let reason = error.to_string();
                    this.status = trf(cx, "读取 DDL 失败：{}", &[&reason]).into();
                    notify::error(trf(cx, "读取 DDL 失败：{}", &[&reason]), window, cx);
                }
            },
        );
    }

    /// 「查看定义」：按引擎跑一条定义原文查询（[`definition_query`] /
    /// [`db_object_definition_query`] 生成的元数据 SQL），取结果第一行里**最长的
    /// 单元格** —— 定义体总是那一行最长的文本（列名 / 列序各引擎不同，不按列位猜）
    /// —— 然后放进编辑器。放进去的就是可执行的 CREATE 语句，改完直接执行，
    /// 「查看」与「编辑」在编辑器里是同一步。
    pub(super) fn stage_object_definition(
        &mut self,
        conn_id: &str,
        sql: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let conn = session.conn.clone();
        self.status = tr(cx, "正在读取对象定义…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<String, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                let outcome = guard.execute(&sql, 1)?;
                let definition = outcome
                    .result
                    .and_then(|set| set.rows.into_iter().next())
                    .map(|row| {
                        row.into_iter()
                            .map(|value| value.render())
                            .max_by_key(|text| text.chars().count())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                if definition.trim().is_empty() {
                    return Err(DbError::Query("object definition is empty".into()));
                }
                Ok(definition)
            },
            move |this, result, window, cx| match result {
                Ok(definition) => {
                    this.stage_sql(
                        definition,
                        tr(cx, "已把对象定义放进编辑器，改好后执行。").into(),
                        window,
                        cx,
                    );
                }
                Err(error) => {
                    let reason = error.to_string();
                    this.status = trf(cx, "读取对象定义失败：{}", &[&reason]).into();
                    notify::error(trf(cx, "读取对象定义失败：{}", &[&reason]), window, cx);
                }
            },
        );
    }

    /// 「导出表结构」：选定落盘路径后取 DDL 写文件。
    ///
    /// 路径先定、DDL 后取：文件面板是异步的，但取 DDL 不依赖界面状态（只有表引用
    /// 在开面板前就冻住了），先后顺序反过来反而会让用户等一次服务器往返才见到面板。
    pub(super) fn export_table_ddl(
        &mut self,
        conn_id: &str,
        table: TableRef,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let conn = session.conn.clone();
        let default_name = format!("{}.sql", table.name);
        crate::file_dialogs::save_file(cx, &default_name, move |path, this, cx| {
            this.spawn_db(
                cx,
                move || -> Result<(String, std::path::PathBuf), DbError> {
                    let mut guard = conn.lock().expect("连接锁不应中毒");
                    Ok((guard.table_ddl(&table)?, path))
                },
                move |this, result, window, cx| match result {
                    Ok((ddl, path)) => match std::fs::write(&path, ddl) {
                        Ok(()) => {
                            let detail =
                                trf(cx, "表结构已导出到 {}。", &[&path.display().to_string()]);
                            this.status = detail.clone().into();
                            notify::success(detail, window, cx);
                        }
                        Err(error) => {
                            let reason = error.to_string();
                            this.status = trf(cx, "写入文件失败：{}", &[&reason]).into();
                            notify::error(trf(cx, "写入文件失败：{}", &[&reason]), window, cx);
                        }
                    },
                    Err(error) => {
                        let reason = error.to_string();
                        this.status = trf(cx, "读取 DDL 失败：{}", &[&reason]).into();
                        notify::error(trf(cx, "读取 DDL 失败：{}", &[&reason]), window, cx);
                    }
                },
            );
        });
    }

    /// 「粘贴表」（dbx 的表剪贴板对位：表节点「复制表」→ 库 / 表分类节点「粘贴表」）。
    ///
    /// 同一条连接且是 SQL 引擎 → 弹窗取名后生成 `CREATE TABLE … AS SELECT * FROM …`
    /// （SQL Server 没有 CTAS，用 `SELECT * INTO`）进编辑器过一次人眼；跨连接（或
    /// 目标不是 SQL 引擎）→ 开数据传输标签，真正把数据搬过去（目标连接在页面上可改）。
    pub(super) fn paste_table(
        &mut self,
        target_conn: &str,
        namespace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((source_conn, source_table)) = self.table_clipboard.clone() else {
            return;
        };
        // 语句级 CTAS 只在同一条连接上成立；其余一律走数据传输（跨引擎也靠它）。
        let same_sql = source_conn == target_conn
            && self
                .sessions
                .get(target_conn)
                .and_then(|session| session.quote)
                .is_some();
        if !same_sql {
            let source_ns = source_table.schema.clone().unwrap_or_default();
            self.open_transfer_tab(&source_conn, &source_ns, vec![source_table], window, cx);
            return;
        }
        let Some(session) = self.sessions.get(target_conn) else {
            return;
        };
        let quote = session.quote.unwrap_or('"');
        let kind = session.kind;
        let namespace = namespace.to_owned();
        let conn_id = target_conn.to_owned();
        self.open_text_prompt_dialog(
            tr(cx, "粘贴表"),
            tr(cx, "新表名"),
            tr(cx, "生成语句"),
            Rc::new(
                move |new_name: &str,
                      this: &mut DatabasePage,
                      window: &mut Window,
                      cx: &mut Context<DatabasePage>| {
                    // SQLite 没有命名空间；其余引擎把新表挂进目标库 / schema。
                    let schema = (kind != DatabaseKind::Sqlite && !namespace.is_empty())
                        .then(|| namespace.clone());
                    let (Ok(new_qualified), Ok(old_qualified)) = (
                        quote_qualified(
                            &TableRef {
                                schema,
                                name: new_name.to_owned(),
                            },
                            quote,
                        ),
                        quote_qualified(&source_table, quote),
                    ) else {
                        notify::warning(tr(cx, "表名包含非法字符。"), window, cx);
                        return false;
                    };
                    let sql = if kind == DatabaseKind::SqlServer {
                        format!("SELECT * INTO {new_qualified} FROM {old_qualified};")
                    } else {
                        format!("CREATE TABLE {new_qualified} AS SELECT * FROM {old_qualified};")
                    };
                    this.activate_conn(&conn_id, window, cx);
                    this.stage_sql(
                        sql,
                        tr(cx, "已生成粘贴语句；确认无误后再执行。").into(),
                        window,
                        cx,
                    );
                    true
                },
            ),
            window,
            cx,
        );
    }

    /// 「导出库结构」：把目标库 / schema 下**全部基础表**的 DDL 依次读出，拼成
    /// 一份 `.sql` 落盘（`-- 限定名` 分隔头）。单表读失败就跳过（部分引擎对个别
    /// 表不给 DDL），一份都没有才算没有。
    ///
    /// 与「导出表结构」同一条纪律：路径先定、DDL 后取（文件面板是异步的，DDL
    /// 不依赖界面状态，只有表清单在开面板前就冻住了）。
    pub(super) fn export_namespace_ddl(
        &mut self,
        conn_id: &str,
        namespace: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let conn = session.conn.clone();
        let kind = session.kind;
        // 与数据传输同一条口径：这个命名空间下的基础表。SQLite 没有命名空间
        // （目录条目不带 schema），整个目录就是这份文件的内容。
        let tables: Vec<TableRef> = session
            .catalog
            .iter()
            .filter(|entry| entry.kind == TableKind::Table)
            .filter(|entry| {
                kind == DatabaseKind::Sqlite || entry.schema.as_deref() == Some(namespace)
            })
            .map(|entry| entry.table_ref())
            .collect();
        let default_name = format!("{}.sql", namespace.replace(['/', '\\'], "_"));
        crate::file_dialogs::save_file(cx, &default_name, move |path, this, cx| {
            this.spawn_db(
                cx,
                move || -> Result<String, DbError> {
                    let mut guard = conn.lock().expect("连接锁不应中毒");
                    let mut out = String::new();
                    for table in &tables {
                        if let Ok(ddl) = guard.table_ddl(table) {
                            if !out.is_empty() {
                                out.push_str("\n\n");
                            }
                            out.push_str(&format!("-- {}\n{ddl}", table.display()));
                        }
                    }
                    Ok(out)
                },
                move |this, result, window, cx| match result {
                    Ok(ddl) if ddl.trim().is_empty() => {
                        notify::warning(tr(cx, "这个库没有可导出的表结构。"), window, cx);
                    }
                    Ok(ddl) => match std::fs::write(&path, ddl) {
                        Ok(()) => {
                            let detail =
                                trf(cx, "库结构已导出到 {}。", &[&path.display().to_string()]);
                            this.status = detail.clone().into();
                            notify::success(detail, window, cx);
                        }
                        Err(error) => {
                            let reason = error.to_string();
                            this.status = trf(cx, "写入文件失败：{}", &[&reason]).into();
                            notify::error(trf(cx, "写入文件失败：{}", &[&reason]), window, cx);
                        }
                    },
                    Err(error) => {
                        let reason = error.to_string();
                        this.status = trf(cx, "读取 DDL 失败：{}", &[&reason]).into();
                        notify::error(trf(cx, "读取 DDL 失败：{}", &[&reason]), window, cx);
                    }
                },
            );
        });
    }

    /// 「导出数据」：按选定格式把整表读出（单次大页，上限 `EXPORT_ROW_CAP`）
    /// 渲染后写文件。CSV / JSON / INSERT SQL 三档共用这一条出口 —— 渲染走
    /// [`ResultFormat::render`]（INSERT 档要表引用与引擎引号，就在这里给）。
    pub(super) fn export_table_data_as(
        &mut self,
        conn_id: &str,
        table: TableRef,
        format: ResultFormat,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let Some(quote) = session.quote else {
            return;
        };
        let Ok(qualified) = quote_qualified(&table, quote) else {
            return;
        };
        let conn = session.conn.clone();
        // INSERT 档的表引用按引擎引号拼好（与结果区导出同一条口径）。
        let insert_args = (format == ResultFormat::InsertSql).then(|| (qualified.clone(), quote));
        let extension = format
            .file_name()
            .rsplit('.')
            .next()
            .unwrap_or("csv")
            .to_owned();
        let default_name = format!("{}.{}", table.name, extension);
        let label = table.display();
        crate::file_dialogs::save_file(cx, &default_name, move |path, this, cx| {
            let sql = format!("SELECT * FROM {qualified}");
            let display = label.clone();
            this.spawn_db(
                cx,
                move || -> Result<(String, usize, std::path::PathBuf), DbError> {
                    let mut guard = conn.lock().expect("连接锁不应中毒");
                    let outcome = guard.execute(&sql, EXPORT_ROW_CAP)?;
                    let result = outcome.result.unwrap_or_default();
                    let count = result.row_count();
                    let payload = format
                        .render(&result, insert_args.as_ref().map(|(t, q)| (t.as_str(), *q)))
                        .ok_or_else(|| {
                            DbError::Config("this format cannot render the table data".into())
                        })?;
                    Ok((payload, count, path))
                },
                move |this, result, window, cx| match result {
                    Ok((payload, count, path)) => {
                        match std::fs::write(&path, payload) {
                            Ok(()) => {
                                let detail = trf(
                                    cx,
                                    "已导出 {} 行（{}）。上限 {} 行，超出的部分请用 WHERE 缩小范围。",
                                    &[&count.to_string(), &path.display().to_string(), &EXPORT_ROW_CAP.to_string()],
                                );
                                this.status = detail.clone().into();
                                notify::success(detail, window, cx);
                            }
                            Err(error) => {
                                let reason = error.to_string();
                                this.status = trf(cx, "写入文件失败：{}", &[&reason]).into();
                                notify::error(trf(cx, "写入文件失败：{}", &[&reason]), window, cx);
                            }
                        }
                        let _ = display;
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        this.status = trf(cx, "导出数据失败：{}", &[&reason]).into();
                        notify::error(trf(cx, "导出数据失败：{}", &[&reason]), window, cx);
                    }
                },
            );
        });
    }

    /// 「复制表结构为」：列清单（列名 / 类型 / 是否主键）转 TSV 或 Markdown 进剪贴板
    /// （dbx 的 copy structure as 对位；数据字典导出的**剪贴板版**，同一套拼法）。
    pub(super) fn copy_table_structure(
        &mut self,
        conn_id: &str,
        table: TableRef,
        markdown: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let conn = session.conn.clone();
        let title = table.display();
        // 三段分开取词：写死 `"列名\t类型\t主键"` 一整串会让 i18n 守卫提取出**字面
        // 反斜杠**形式，与运行时（真 tab）键永远对不上。取词要在**进后台闭包之前**
        // 完成——`tr` 要 `&App`，闭包（要求 Send）捕获不得。
        let header = (tr(cx, "列名"), tr(cx, "类型"), tr(cx, "主键"));
        self.spawn_db(
            cx,
            move || -> Result<String, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                let columns = guard.table_columns(&table)?;
                let pk = guard.table_primary_key(&table).unwrap_or_default();
                use std::fmt::Write as _;
                let mut text = String::new();
                if markdown {
                    let _ = writeln!(text, "# {}", title);
                    let _ = writeln!(text);
                    let _ = writeln!(text, "| 列名 | 类型 | 主键 |");
                    let _ = writeln!(text, "| --- | --- | --- |");
                    for column in &columns {
                        let is_pk = pk.iter().any(|name| name == &column.name);
                        let _ = writeln!(
                            text,
                            "| {} | {} | {} |",
                            column.name.replace('|', "\\|"),
                            column.type_name.replace('|', "\\|"),
                            if is_pk { "✓" } else { "" }
                        );
                    }
                } else {
                    let _ = writeln!(text, "{}\t{}\t{}", header.0, header.1, header.2);
                    for column in &columns {
                        let is_pk = pk.iter().any(|name| name == &column.name);
                        let _ = writeln!(
                            text,
                            "{}\t{}\t{}",
                            column.name.replace(['\t', '\n'], " "),
                            column.type_name.replace(['\t', '\n'], " "),
                            if is_pk { "✓" } else { "" }
                        );
                    }
                }
                Ok(text)
            },
            move |this, result, _window, cx| match result {
                Ok(text) => {
                    let note = if markdown {
                        tr(cx, "已复制表结构（Markdown）。")
                    } else {
                        tr(cx, "已复制表结构（TSV）。")
                    };
                    this.put_on_clipboard(text, note, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "读取表结构失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 「导出数据字典」：列清单（列名 / 类型 / 是否主键）写成 Markdown 表格落盘。
    pub(super) fn export_data_dictionary(
        &mut self,
        conn_id: &str,
        table: TableRef,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let conn = session.conn.clone();
        let default_name = format!("{}.md", table.name);
        let title = table.display();
        crate::file_dialogs::save_file(cx, &default_name, move |path, this, cx| {
            this.spawn_db(
                cx,
                move || -> Result<(String, std::path::PathBuf), DbError> {
                    let mut guard = conn.lock().expect("连接锁不应中毒");
                    let columns = guard.table_columns(&table)?;
                    let pk = guard.table_primary_key(&table).unwrap_or_default();
                    use std::fmt::Write as _;
                    let mut md = String::new();
                    let _ = writeln!(md, "# {}", title);
                    let _ = writeln!(md);
                    let _ = writeln!(md, "| 列名 | 类型 | 主键 |");
                    let _ = writeln!(md, "| --- | --- | --- |");
                    for column in &columns {
                        let is_pk = pk.iter().any(|name| name == &column.name);
                        let _ = writeln!(
                            md,
                            "| {} | {} | {} |",
                            column.name.replace('|', "\\|"),
                            column.type_name.replace('|', "\\|"),
                            if is_pk { "✓" } else { "" }
                        );
                    }
                    Ok((md, path))
                },
                move |this, result, window, cx| match result {
                    Ok((md, path)) => match std::fs::write(&path, md) {
                        Ok(()) => {
                            let detail =
                                trf(cx, "数据字典已导出到 {}。", &[&path.display().to_string()]);
                            this.status = detail.clone().into();
                            notify::success(detail, window, cx);
                        }
                        Err(error) => {
                            let reason = error.to_string();
                            this.status = trf(cx, "写入文件失败：{}", &[&reason]).into();
                            notify::error(trf(cx, "写入文件失败：{}", &[&reason]), window, cx);
                        }
                    },
                    Err(error) => {
                        let reason = error.to_string();
                        this.status = trf(cx, "读取列信息失败：{}", &[&reason]).into();
                        notify::error(trf(cx, "读取列信息失败：{}", &[&reason]), window, cx);
                    }
                },
            );
        });
    }

    /// 「生成模型」：按目录列清单生成 Rust 结构体草稿到剪贴板（类型映射是**草稿级**
    /// 的：int 系 → `i64`，浮点系 → `f64`，布尔 → `bool`，其余一律 `String`，
    /// 让用户拿到起点而不是成品）。
    pub(super) fn generate_model(
        &mut self,
        conn_id: &str,
        table: TableRef,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let conn = session.conn.clone();
        let struct_name = table
            .name
            .split(['_', '-', '.'])
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().collect::<String>()
                }) + &chars.as_str().to_lowercase()
            })
            .collect::<String>();
        let display = table.display();
        self.status = trf(cx, "正在读取 {} 的列…", &[&display]).into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<(String, String), DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                use std::fmt::Write as _;
                let columns = guard.table_columns(&table)?;
                let mut body = String::new();
                for column in &columns {
                    let field = column
                        .name
                        .split(['_', '-', '.'])
                        .filter(|part| !part.is_empty())
                        .map(|part| part.to_lowercase())
                        .collect::<Vec<_>>()
                        .join("_");
                    writeln!(
                        body,
                        "    pub {}: {},",
                        if field.is_empty() { "field" } else { &field },
                        rust_type_of(&column.type_name)
                    )
                    .ok();
                }
                Ok((
                    format!("pub struct {struct_name} {{\n{body}}}"),
                    struct_name,
                ))
            },
            move |this, result, window, cx| match result {
                Ok((code, struct_name)) => {
                    let note = trf(cx, "已生成 {} 的 Rust 模型草稿。", &[&struct_name]);
                    this.put_on_clipboard(code, note, cx);
                }
                Err(error) => {
                    let reason = error.to_string();
                    this.status = trf(cx, "读取列信息失败：{}", &[&reason]).into();
                    notify::error(trf(cx, "读取列信息失败：{}", &[&reason]), window, cx);
                }
            },
        );
    }

    /// 左栏树里的**目录对象叶子**（表 / 视图 / 存储过程 / 函数 / 序列）。
    ///
    /// 从右栏对象树迁过来的：入口与右键菜单对同一个目录条目做的事没变，
    /// 变的只是「谁负责分组」——分类节点成了树的一层，右栏只留详情。
    pub(super) fn sidebar_object_row(
        &self,
        conn_id: &str,
        entry: CatalogTable,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let kind = entry.kind;
        let reference = entry.table_ref();
        let selected = self.selected_table.as_ref() == Some(&reference);
        // 表 / 视图：开**数据标签**（同表复用，⌥ 点击强制新开）。存储过程 / 函数 / 序列：
        // 不能当表读，往编辑器里放一段引用骨架（这是它们在界面上的**真实功能**，不是占位）。
        let browsable = kind.is_relation();
        let conn_owned = conn_id.to_owned();
        let engine = self
            .active_config()
            .map_or(DatabaseKind::Sqlite, |cfg| cfg.kind);
        let snippet = object_snippet(kind, engine, &reference.display());
        let browse_label = tr(cx, "浏览数据");
        let select_label = tr(cx, "生成 SELECT 语句");
        let copy_label = tr(cx, "复制对象名");
        let refresh_label = tr(cx, "刷新");
        let copied_note = tr(cx, "已复制对象名。");
        // 「生成删除语句」（只给**表**）：语句生成到编辑器，不在这里执行。
        // 视图 / 函数 / 序列各有各的改法（`CREATE OR REPLACE` / `ALTER FUNCTION`
        // / `ALTER SEQUENCE`），不在口径里，给了反而是半个按钮。
        // 「修改结构」不再要模板：直接开结构编辑器标签，引擎能力闸门由编辑器
        // 自己把（拿不到 DDL 能力的引擎画口径空态，见 `structure_capabilities`）。
        let drop_sql: Option<String> = (kind == TableKind::Table)
            .then(|| {
                // 引号取**会话的真值**（agent 档只有连上才知道是 `"` 还是 `` ` ``）。
                let quote = self.active_quote()?;
                let qualified = quote_qualified(&reference, quote).ok()?;
                drop_table_template(engine, &qualified)
            })
            .flatten();
        let alter_label = tr(cx, "修改结构");
        let drop_label = tr(cx, "生成删除语句");
        let drop_note = tr(cx, "已生成删表语句；确认无误后再执行。");
        // 非表对象（视图 / 过程 / 函数 / 序列）的删除说明文案与删表分开：界面上
        // 「删表」与「删视图」不是一回事。
        let drop_object_note = tr(cx, "已生成删除语句；确认无误后再执行。");
        // dbx 对位的新增项文案（菜单分组见 context_menu）。
        let newtab_label = tr(cx, "新标签页查看");
        let view_ddl_label = tr(cx, "查看 DDL");
        let definition_label = tr(cx, "查看定义");
        let exec_label = tr(cx, "生成执行语句");
        let setval_label = tr(cx, "设置当前值…");
        let gen_sql_label = tr(cx, "生成 SQL");
        let insert_label = tr(cx, "生成 INSERT 语句");
        let update_label = tr(cx, "生成 UPDATE 语句");
        let delete_label = tr(cx, "生成 DELETE 语句");
        let skeleton_note = tr(cx, "已把 SQL 骨架放进编辑器。");
        let model_label = tr(cx, "生成模型");
        let history_label = tr(cx, "历史查询");
        let pin_label = tr(cx, "置顶");
        let rename_label = tr(cx, "重命名");
        let clone_label = tr(cx, "克隆为新表");
        let autoinc_label = tr(cx, "设置自增起始值");
        let export_data_label = tr(cx, "导出数据");
        let export_ddl_label = tr(cx, "导出表结构");
        let dict_label = tr(cx, "导出数据字典");
        // 表剪贴板（dbx 对位）：「复制表」存进页内剪贴板，目标库 / 表分类「粘贴表」落地。
        let copy_table_label = tr(cx, "复制表");
        let copy_table_note = tr(cx, "已复制表；在目标库或表分类上右键「粘贴表」。");
        // 「补齐 dbx 对位」的新增项文案。
        let ai_label = tr(cx, "加入 AI");
        let dependency_label = tr(cx, "查看依赖");
        let dependency_note = tr(cx, "已生成引用查询：执行后列出引用此表的表与列。");
        let compare_label = tr(cx, "比较数据…");
        let import_into_label = tr(cx, "导入数据…");
        let copy_structure_label = tr(cx, "复制表结构为");
        let truncate_label = tr(cx, "截断表");
        let truncate_note = tr(cx, "已生成 TRUNCATE 语句；不可回滚，确认无误后再执行。");
        let clear_label = tr(cx, "清空数据");
        let clear_note = tr(cx, "已生成清空语句；建议放进事务里执行。");
        let vacuum_label = tr(cx, "整理空间（VACUUM）");
        let vacuum_note = tr(cx, "已生成 VACUUM 语句；确认无误后再执行。");

        // 表行**可展开**（DBX 同款：列 / 索引 / 外键 / 触发器四个子分组）；视图 /
        // 过程 / 函数 / 序列仍是叶子 —— 它们没有「表结构」可铺。展开状态挂在
        // `tree_open`（键 = `tbl:<连接>:<限定名>`），首次展开由 chevron 的回调
        // 触发一次结构内省（`load_table_tree`），之后命中缓存。
        let expandable = kind == TableKind::Table;
        let node_key = format!("tbl:{conn_id}:{}", reference.display());
        let open = expandable && self.tree_open.contains(&node_key);
        let trailing: AnyElement = if expandable {
            let toggle_view = view.clone();
            let toggle_key = node_key.clone();
            let toggle_conn = conn_owned.clone();
            let toggle_ref = reference.clone();
            h_flex()
                .flex_none()
                .child(
                    icon_button(
                        SharedString::from(format!(
                            "db-table-toggle-{conn_id}-{}",
                            reference.display()
                        )),
                        if open {
                            tr(cx, "收起")
                        } else {
                            tr(cx, "展开")
                        },
                        Icon::new(if open {
                            DuckIcon::ChevronDown
                        } else {
                            DuckIcon::ChevronRight
                        })
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    // 箭头只切换折叠 + 首次展开顺手取结构，**不开数据标签**：
                    // 与库节点的箭头同一条纪律（点整行才是「浏览数据」）。
                    .on_click(move |_, _, cx| {
                        let Some(entity) = toggle_view.upgrade() else {
                            return;
                        };
                        let key = toggle_key.clone();
                        let conn = toggle_conn.clone();
                        let table = toggle_ref.clone();
                        entity.update(cx, |this, cx| {
                            if !this.tree_open.remove(&key) {
                                this.tree_open.insert(key);
                                this.load_table_tree(&conn, &table, cx);
                            }
                            cx.notify();
                        });
                        // 箭头是行内**嵌套按钮**：click 在冒泡期派发且不自动截停，
                        // 不截停的话外层行按钮也会收到这次点击——它无条件切到这张表
                        // 的数据标签（「点箭头看一眼结构」变成「跳走」）。
                        cx.stop_propagation();
                    }),
                )
                .into_any_element()
        } else {
            div().flex_none().into_any_element()
        };

        let row = sidebar_navigation_button(
            SharedString::from(format!("db-object-{}", reference.display())),
            SharedString::from(entry.name.clone()),
            Icon::new(table_kind_icon(kind))
                .size(extended.icon.md)
                .text_color(if selected {
                    extended.colors.accent_text
                } else {
                    tokens.colors.muted_foreground
                }),
            // 行尾**不挂「表 / 视图 / 存储过程」标签**：叶子挂在同名分类节点下面
            // （`表 (14)` → 一串 `表`），标签只是把分类标题重复一遍；种类由行首图标
            // 表意（`table_kind_icon`，格子 / 图层 / 终端 / 函数 / 序列各不相同）。
            // 名字因此占满整行，长表名不容易被标签挤断。表行的行尾是展开箭头。
            trailing,
            selected,
            cx,
        )
        .on_click({
            let reference = reference.clone();
            let snippet = snippet.clone();
            // 闭包是 `Fn`：外层这个 `view` 后面 context_menu 还要用，这里 clone 一份，
            // 否则 `move` 把原始那个搬走了（E0382）。
            let view = view.clone();
            let conn_owned = conn_owned.clone();
            move |event, window, cx| {
                let Some(entity) = view.upgrade() else {
                    return;
                };
                if browsable {
                    // ⌥ 点击 = 强制新开（dbx 的 modifier-only open-in-new-tab）。
                    let force_new = event.modifiers().alt;
                    entity.update(cx, |this, cx| {
                        this.open_data_tab(&conn_owned, reference.clone(), force_new, window, cx);
                    });
                } else {
                    entity.update(cx, |this, cx| {
                        this.sql.update(cx, |state, cx| {
                            state.set_value(snippet.clone(), window, cx);
                        });
                        this.status = tr(cx, "已把引用骨架放进编辑器。").into();
                        cx.notify();
                    });
                }
            }
        })
        // 对象行右键（dbx 同款全量分组，每项真实后端）：查看 / 生成 / 复制·置顶 /
        // 结构 / 导出 / 刷新·删除。「生成 SELECT」只对表 / 视图开放（存储过程与
        // 函数的骨架是 CALL / SELECT，序列是 nextval，都不叫 SELECT）；结构管理与
        // 导出只对表开放（视图 / 函数 / 序列各有各的改法，不在口径里）。
        .context_menu({
            let view = view.clone();
            let object_name = reference.display();
            let browse_ref = reference.clone();
            let snippet = snippet.clone();
            let menu_engine = engine;
            // 右键菜单里所有「拼限定名」的地方都用这一个引号：会话的真值（agent 档靠它）。
            // 一次取好往下传，而不是在几个 `move` 闭包里各取一次（那里没有 `self`）。
            let menu_quote = self.active_quote();
            let menu_namespace = reference.schema.clone().unwrap_or_default();
            let pin_key = format!(
                "{conn_id}:{}:{}",
                reference.schema.as_deref().unwrap_or(""),
                entry.name
            );
            let is_pinned = self.pinned_tables.contains(&pin_key);
            // 生成 SQL ▸ 的骨架：限定名按引擎引号拼好，列位留占位 —— 用户补列名。
            // 只给**表**：视图不可写（INSERT / UPDATE / DELETE 对视图通常无效），
            // 视图的子菜单只有 SELECT 一项。
            let sql_skeletons: Option<(String, String, String)> = (kind == TableKind::Table)
                .then(|| {
                    let quote = menu_quote?;
                    let qualified = quote_qualified(&reference, quote).ok()?;
                    let q = quote;
                    Some((
                        format!("INSERT INTO {qualified} ({q}列{q}) VALUES (…);"),
                        format!("UPDATE {qualified} SET {q}列{q} = … WHERE {q}主键{q} = …;"),
                        format!("DELETE FROM {qualified} WHERE {q}主键{q} = …;"),
                    ))
                })
                .flatten();
            // 定义原文查询（视图 / 过程 / 函数有出口，序列与无 SQL 引擎没有）：
            // 先算好那条元数据 SQL，点「查看定义」时才去连接上执行。
            let definition_sql = definition_query(kind, engine, &menu_namespace, &entry.name);
            // 「查看依赖」的引用查询：**谁引用这张表**（外键反向）。只有三家有
            // 信息架构可查（SQLite 要遍历整库 pragma，不在右键路径上做）。
            let dependency_sql: Option<String> = (kind == TableKind::Table)
                .then(|| {
                let escaped = entry.name.replace('\'', "''");
                match engine {
                    DatabaseKind::Mysql => Some(format!(
                        "SELECT TABLE_NAME AS referencing_table, COLUMN_NAME AS referencing_column, \
                         REFERENCED_COLUMN_NAME, CONSTRAINT_NAME\nFROM information_schema.KEY_COLUMN_USAGE\n\
                         WHERE TABLE_SCHEMA = DATABASE() AND REFERENCED_TABLE_NAME = '{escaped}';"
                    )),
                    DatabaseKind::Postgres => Some(format!(
                        "SELECT tc.table_schema, tc.table_name, kcu.column_name, \
                         ccu.table_schema AS referenced_schema, ccu.table_name AS referenced_table\n\
                         FROM information_schema.table_constraints tc\n\
                         JOIN information_schema.key_column_usage kcu\n  \
                         ON tc.constraint_name = kcu.constraint_name\n\
                         JOIN information_schema.constraint_column_usage ccu\n  \
                         ON tc.constraint_name = ccu.constraint_name\n\
                         WHERE tc.constraint_type = 'FOREIGN KEY' AND ccu.table_name = '{escaped}';"
                    )),
                    DatabaseKind::SqlServer => Some(format!(
                        "SELECT tp.name AS referencing_table, cp.name AS referencing_column, \
                         tr.name AS referenced_table, cr.name AS referenced_column\n\
                         FROM sys.foreign_keys fk\n\
                         JOIN sys.foreign_key_columns fkc ON fk.object_id = fkc.constraint_object_id\n\
                         JOIN sys.tables tp ON fkc.parent_object_id = tp.object_id\n\
                         JOIN sys.columns cp ON fkc.parent_object_id = cp.object_id \
                         AND fkc.parent_column_id = cp.column_id\n\
                         JOIN sys.tables tr ON fkc.referenced_object_id = tr.object_id\n\
                         JOIN sys.columns cr ON fkc.referenced_object_id = cr.object_id \
                         AND fkc.referenced_column_id = cr.column_id\n\
                         WHERE tr.name = '{escaped}';"
                    )),
                    _ => None,
                }
            })
            .flatten();
            // 非表对象的删除语句：限定名按引擎引号拼好再进模板（表走上面的
            // `drop_sql` 那条路，这里只管视图 / 过程 / 函数 / 序列）。
            let drop_object_sql: Option<String> = (kind != TableKind::Table)
                .then(|| {
                    let quote = menu_quote?;
                    let qualified = quote_qualified(&reference, quote).ok()?;
                    drop_catalog_sql(kind, &qualified)
                })
                .flatten();
            move |mut menu, window, cx| {
                let view = view.clone();
                // ── 查看 ──
                {
                    let browse_view = view.clone();
                    let browse_ref = browse_ref.clone();
                    let browse_conn = conn_owned.clone();
                    menu = menu.item(
                        PopupMenuItem::new(browse_label)
                            .disabled(!browsable)
                            .on_click(move |_, window, cx| {
                                let Some(entity) = browse_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.open_data_tab(
                                        &browse_conn,
                                        browse_ref.clone(),
                                        false,
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    );
                }
                {
                    let newtab_view = view.clone();
                    let newtab_ref = browse_ref.clone();
                    let newtab_conn = conn_owned.clone();
                    menu = menu.item(
                        PopupMenuItem::new(newtab_label)
                            .disabled(!browsable)
                            .on_click(move |_, window, cx| {
                                let Some(entity) = newtab_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.open_data_tab(
                                        &newtab_conn,
                                        newtab_ref.clone(),
                                        true,
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    );
                }
                if kind == TableKind::Table {
                    let ddl_view = view.clone();
                    let ddl_ref = browse_ref.clone();
                    let ddl_conn = conn_owned.clone();
                    menu = menu.item(PopupMenuItem::new(view_ddl_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = ddl_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.view_table_ddl(&ddl_conn, ddl_ref.clone(), window, cx);
                            });
                        },
                    ));
                }
                // 「查看依赖」：生成「谁引用我」的元数据查询进编辑器（MySQL / PG /
                // SQL Server；SQLite 要遍历整库 pragma，不给这项）。只读查询，
                // 用户自己执行 —— 与「查看定义」同一条节奏。
                if let Some(query) = dependency_sql.clone() {
                    let dep_view = view.clone();
                    menu = menu.item(PopupMenuItem::new(dependency_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = dep_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.stage_sql(query.clone(), dependency_note.into(), window, cx);
                            });
                        },
                    ));
                }
                // 查看定义（视图 / 过程 / 函数）：按引擎跑定义原文查询，取回的
                // CREATE 语句直接放进编辑器 —— 改完就能执行，即「编辑视图定义」。
                if let Some(query) = definition_sql.clone() {
                    let def_view = view.clone();
                    let def_conn = conn_owned.clone();
                    menu = menu.item(PopupMenuItem::new(definition_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = def_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.stage_object_definition(
                                    &def_conn,
                                    query.clone(),
                                    window,
                                    cx,
                                );
                            });
                        },
                    ));
                }
                menu = menu.separator();
                // ── 生成 ──
                if browsable {
                    let gen_view = view.clone();
                    let gen_note = skeleton_note;
                    let select_sql = snippet.clone();
                    let skeletons = sql_skeletons.clone();
                    let sql_menu = PopupMenu::build(window, cx, move |mut menu, _, _| {
                        menu = menu.item(PopupMenuItem::new(select_label).on_click({
                            let gen_view = gen_view.clone();
                            let select_sql = select_sql.clone();
                            move |_, window, cx| {
                                let Some(entity) = gen_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.stage_sql(select_sql.clone(), gen_note.into(), window, cx);
                                });
                            }
                        }));
                        // INSERT / UPDATE / DELETE 骨架只给**表**：视图不可写，
                        // 对视图生成写语句会诱导用户执行必错的 SQL。
                        if let Some((insert_sql, update_sql, delete_sql)) = skeletons {
                            menu = menu.separator()
                                .item(PopupMenuItem::new(insert_label).on_click({
                                    let gen_view = gen_view.clone();
                                    move |_, window, cx| {
                                        let Some(entity) = gen_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.stage_sql(
                                                insert_sql.clone(),
                                                gen_note.into(),
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }))
                                .item(PopupMenuItem::new(update_label).on_click({
                                    let gen_view = gen_view.clone();
                                    move |_, window, cx| {
                                        let Some(entity) = gen_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.stage_sql(
                                                update_sql.clone(),
                                                gen_note.into(),
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }))
                                .item(PopupMenuItem::new(delete_label).on_click({
                                    let gen_view = gen_view.clone();
                                    move |_, window, cx| {
                                        let Some(entity) = gen_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.stage_sql(
                                                delete_sql.clone(),
                                                gen_note.into(),
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }));
                        }
                        menu
                    });
                    menu = menu.item(PopupMenuItem::submenu(gen_sql_label, sql_menu));
                }
                // 执行骨架（过程 / 函数 / 序列）：CALL / 表函数 / nextval —— 与左键
                // 点行放进编辑器的是同一段（`object_snippet`），补一个菜单入口。
                if !browsable {
                    let exec_view = view.clone();
                    let exec_sql = snippet.clone();
                    let exec_note = skeleton_note;
                    menu = menu.item(PopupMenuItem::new(exec_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = exec_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.stage_sql(exec_sql.clone(), exec_note.into(), window, cx);
                            });
                        },
                    ));
                }
                if kind == TableKind::Table {
                    let model_view = view.clone();
                    let model_ref = browse_ref.clone();
                    let model_conn = conn_owned.clone();
                    menu = menu.item(PopupMenuItem::new(model_label).on_click(
                        move |_, _, cx| {
                            let Some(entity) = model_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.generate_model(&model_conn, model_ref.clone(), cx);
                            });
                        },
                    ));
                }
                {
                    let history_view = view.clone();
                    menu = menu.item(PopupMenuItem::new(history_label).on_click(
                        move |_, _, cx| {
                            let Some(entity) = history_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.reveal_right_dock(RightDock::History, cx);
                            });
                        },
                    ));
                }
                menu = menu.separator();
                // ── 复制 / 置顶 ──
                {
                    let copy_view = view.clone();
                    let name = object_name.clone();
                    menu = menu.item(PopupMenuItem::new(copy_label).on_click(move |_, _, cx| {
                        let Some(entity) = copy_view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.put_on_clipboard(name.clone(), copied_note, cx);
                        });
                    }));
                }
                if kind == TableKind::Table {
                    // 「复制表」：存进**页内表剪贴板**（系统剪贴板放的是对象名，两者
                    // 不抢）；目标库 / 表分类上「粘贴表」把它落地。
                    let copy_view = view.clone();
                    let copy_conn = conn_owned.clone();
                    let copy_ref = browse_ref.clone();
                    menu = menu.item(PopupMenuItem::new(copy_table_label).on_click(
                        move |_, _, cx| {
                            let Some(entity) = copy_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.table_clipboard = Some((copy_conn.clone(), copy_ref.clone()));
                                this.toast(notify::Tone::Success, copy_table_note, cx);
                            });
                        },
                    ));
                }
                {
                    let pin_view = view.clone();
                    let pin_key = pin_key.clone();
                    menu = menu.item(
                        PopupMenuItem::new(pin_label)
                            .checked(is_pinned)
                            .on_click(move |_, _, cx| {
                                let Some(entity) = pin_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    if !this.pinned_tables.remove(&pin_key) {
                                        this.pinned_tables.insert(pin_key.clone());
                                    }
                                    cx.notify();
                                });
                            }),
                    );
                }
                // 「加入 AI」：打开右栏 AI 分段并预填带表名的提问（dbx 的 addToAi
                // 对位；本仓 AI 的表上下文在发请求时按**选中表**组装 —— 先把选中
                // 表切到这一张再开面板，用户接着输入问题就行）。
                if kind.is_relation() {
                    let ai_view = view.clone();
                    let ai_ref = browse_ref.clone();
                    let ai_name = object_name.clone();
                    menu = menu.item(PopupMenuItem::new(ai_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = ai_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.selected_table = Some(ai_ref.clone());
                                this.reveal_right_dock(RightDock::Ai, cx);
                                let question = this.ai_question.clone();
                                let prompt =
                                    trf(cx, "请分析表 {} 的结构与数据：", &[&ai_name]);
                                question.update(cx, |state, cx| {
                                    state.set_value(prompt, window, cx);
                                    state.focus(window, cx);
                                });
                                cx.notify();
                            });
                        },
                    ));
                }
                menu = menu.separator();
                // ── 结构 ──
                if kind == TableKind::Table {
                    // 「修改结构」直接打开（或复用）这张表的结构编辑器标签 ——
                    // 不再生成 `ALTER` 骨架：可视化编辑器覆盖的口径更全，生成的
                    // 语句也逐条可核（同右栏 DDL 卡的「修改表结构」一条路径）。
                    let alter_view = view.clone();
                    let alter_conn = conn_owned.clone();
                    let alter_ref = browse_ref.clone();
                    menu = menu.item(
                        PopupMenuItem::new(alter_label).on_click(move |_, window, cx| {
                            let Some(entity) = alter_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.open_structure_tab(
                                    &alter_conn,
                                    alter_ref.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            });
                        }),
                    );
                    // 重命名 / 克隆为新表：弹窗取名 → 生成对应方言的语句到编辑器
                    //（不直接执行 —— 结构变更一律过一次人眼）。
                    let rename_view = view.clone();
                    let rename_ref = browse_ref.clone();
                    let rename_engine = menu_engine;
                    let rename_ns = menu_namespace.clone();
                    menu = menu.item(PopupMenuItem::new(rename_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = rename_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                let table = rename_ref.clone();
                                let engine = rename_engine;
                                let namespace = rename_ns.clone();
                                this.open_text_prompt_dialog(
                                    tr(cx, "重命名表"),
                                    tr(cx, "新表名"),
                                    tr(cx, "重命名"),
                                    Rc::new(
                                        move |new_name: &str,
                                              this: &mut DatabasePage,
                                              window: &mut Window,
                                              cx: &mut Context<DatabasePage>| {
                                            let Ok(quoted_new) =
                                                duck_db::drivers::quote_identifier(new_name, menu_quote.unwrap_or('"'))
                                            else {
                                                notify::warning(
                                                    tr(cx, "表名包含非法字符。"),
                                                    window,
                                                    cx,
                                                );
                                                return false;
                                            };
                                            let old_qualified = match quote_qualified(
                                                &table,
                                                menu_quote.unwrap_or('"'),
                                            ) {
                                                Ok(qualified) => qualified,
                                                Err(_) => table.display(),
                                            };
                                            let sql = match engine {
                                                DatabaseKind::Mysql | DatabaseKind::ClickHouse => {
                                                    format!("RENAME TABLE {old_qualified} TO {quoted_new};")
                                                }
                                                DatabaseKind::SqlServer => {
                                                    format!("EXEC sp_rename '{old_qualified}', '{new_name}';")
                                                }
                                                _ => {
                                                    format!("ALTER TABLE {old_qualified} RENAME TO {quoted_new};")
                                                }
                                            };
                                            let _ = namespace;
                                            this.stage_sql(
                                                sql,
                                                tr(cx, "已生成重命名语句；确认无误后再执行。").into(),
                                                window,
                                                cx,
                                            );
                                            true
                                        },
                                    ),
                                    window,
                                    cx,
                                );
                            });
                        },
                    ));
                    let clone_view = view.clone();
                    let clone_ref = browse_ref.clone();
                    let clone_engine = menu_engine;
                    menu = menu.item(PopupMenuItem::new(clone_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = clone_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                let table = clone_ref.clone();
                                let engine = clone_engine;
                                this.open_text_prompt_dialog(
                                    tr(cx, "克隆为新表"),
                                    tr(cx, "新表名"),
                                    tr(cx, "生成语句"),
                                    Rc::new(
                                        move |new_name: &str,
                                              this: &mut DatabasePage,
                                              window: &mut Window,
                                              cx: &mut Context<DatabasePage>| {
                                            let quote = menu_quote.unwrap_or('"');
                                            let (Ok(new_qualified), Ok(old_qualified)) = (
                                                quote_qualified(
                                                    &TableRef {
                                                        schema: table.schema.clone(),
                                                        name: new_name.to_owned(),
                                                    },
                                                    quote,
                                                ),
                                                quote_qualified(&table, quote),
                                            ) else {
                                                notify::warning(
                                                    tr(cx, "表名包含非法字符。"),
                                                    window,
                                                    cx,
                                                );
                                                return false;
                                            };
                                            // SQL Server 没有 CTAS，用 SELECT INTO。
                                            let sql = if engine == DatabaseKind::SqlServer {
                                                format!("SELECT * INTO {new_qualified} FROM {old_qualified};")
                                            } else {
                                                format!("CREATE TABLE {new_qualified} AS SELECT * FROM {old_qualified};")
                                            };
                                            this.stage_sql(
                                                sql,
                                                tr(cx, "已生成克隆语句；确认无误后再执行。").into(),
                                                window,
                                                cx,
                                            );
                                            true
                                        },
                                    ),
                                    window,
                                    cx,
                                );
                            });
                        },
                    ));
                    // 自增起始值：只有 MySQL（`AUTO_INCREMENT`）与 SQLite
                    //（`sqlite_sequence`）有直接出口，其余引擎不给这项。
                    if menu_engine == DatabaseKind::Mysql || menu_engine == DatabaseKind::Sqlite {
                        let auto_view = view.clone();
                        let auto_ref = browse_ref.clone();
                        let auto_engine = menu_engine;
                        menu = menu.item(PopupMenuItem::new(autoinc_label).on_click(
                            move |_, window, cx| {
                                let Some(entity) = auto_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    let table = auto_ref.clone();
                                    let engine = auto_engine;
                                    this.open_text_prompt_dialog(
                                        tr(cx, "设置自增起始值"),
                                        tr(cx, "起始值"),
                                        tr(cx, "生成语句"),
                                        Rc::new(
                                            move |value: &str,
                                                  this: &mut DatabasePage,
                                                  window: &mut Window,
                                                  cx: &mut Context<DatabasePage>| {
                                                let Ok(next) = value.parse::<u64>() else {
                                                    notify::warning(
                                                        tr(cx, "请输入一个非负整数。"),
                                                        window,
                                                        cx,
                                                    );
                                                    return false;
                                                };
                                                let quote = menu_quote.unwrap_or('"');
                                                let Ok(qualified) = quote_qualified(&table, quote) else {
                                                    return false;
                                                };
                                                let sql = if engine == DatabaseKind::Mysql {
                                                    format!("ALTER TABLE {qualified} AUTO_INCREMENT = {next};")
                                                } else {
                                                    format!(
                                                        "UPDATE sqlite_sequence SET seq = {next} WHERE name = '{}';",
                                                        table.name.replace('\'', "''")
                                                    )
                                                };
                                                this.stage_sql(
                                                    sql,
                                                    tr(cx, "已生成自增起始值语句；确认无误后再执行。").into(),
                                                    window,
                                                    cx,
                                                );
                                                true
                                            },
                                        ),
                                        window,
                                        cx,
                                    );
                                });
                            },
                        ));
                    }
                    // 「生成删除语句」仍要方言模板（引号 / 引擎子句），给不出不给项。
                    if let Some(drop) = drop_sql.as_ref() {
                        let drop = drop.clone();
                        let drop_view = view.clone();
                        menu = menu.item(
                            PopupMenuItem::new(drop_label).on_click(move |_, window, cx| {
                                let Some(entity) = drop_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.stage_sql(drop.clone(), drop_note.into(), window, cx);
                                });
                            }),
                        );
                    }
                    // ── 危险操作（生成语句版：一律进编辑器过一次人眼，不直接执行
                    // —— 与重命名 / 克隆同一条仓库纪律；dbx 的 Truncate / Empty /
                    // Vacuum 对位）。截断只有吃 `TRUNCATE` 的四家给（SQLite 没有，
                    // 「清空数据」覆盖该场景）；VACUUM 只有 PG / SQLite 有这个概念
                    // （MySQL 的 OPTIMIZE TABLE 会锁表，不冒充）。
                    if let Some(quote) = menu_quote
                        && let Ok(qualified) = quote_qualified(&browse_ref, quote)
                    {
                        if matches!(
                            engine,
                            DatabaseKind::Mysql
                                | DatabaseKind::Postgres
                                | DatabaseKind::SqlServer
                                | DatabaseKind::ClickHouse
                        ) {
                            let sql = format!("TRUNCATE TABLE {qualified};");
                            let truncate_view = view.clone();
                            menu = menu.item(PopupMenuItem::new(truncate_label).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = truncate_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.stage_sql(
                                            sql.clone(),
                                            truncate_note.into(),
                                            window,
                                            cx,
                                        );
                                    });
                                },
                            ));
                        }
                        {
                            let sql = format!("DELETE FROM {qualified};");
                            let clear_view = view.clone();
                            menu = menu.item(PopupMenuItem::new(clear_label).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = clear_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.stage_sql(
                                            sql.clone(),
                                            clear_note.into(),
                                            window,
                                            cx,
                                        );
                                    });
                                },
                            ));
                        }
                        match engine {
                            DatabaseKind::Postgres => {
                                let sql = format!("VACUUM (ANALYZE) {qualified};");
                                let vacuum_view = view.clone();
                                menu = menu.item(PopupMenuItem::new(vacuum_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = vacuum_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.stage_sql(
                                                sql.clone(),
                                                vacuum_note.into(),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            DatabaseKind::Sqlite => {
                                let sql = "VACUUM;".to_owned();
                                let vacuum_view = view.clone();
                                menu = menu.item(PopupMenuItem::new(vacuum_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = vacuum_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.stage_sql(
                                                sql.clone(),
                                                vacuum_note.into(),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                // 非表对象的结构段：序列给「设置当前值」，其余给「生成删除语句」
                // （视图 / 过程 / 函数 / 序列各有各的 DROP，引擎给不出的不给项）。
                if kind != TableKind::Table {
                    if kind == TableKind::Sequence && engine == DatabaseKind::Postgres {
                        let setval_view = view.clone();
                        let setval_schema = menu_namespace.clone();
                        let setval_name = entry.name.clone();
                        menu = menu.item(PopupMenuItem::new(setval_label).on_click(
                            move |_, window, cx| {
                                let Some(entity) = setval_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    let schema = setval_schema.clone();
                                    let name = setval_name.clone();
                                    this.open_text_prompt_dialog(
                                        tr(cx, "设置当前值…"),
                                        tr(cx, "当前值"),
                                        tr(cx, "生成语句"),
                                        Rc::new(
                                            move |value: &str,
                                                  this: &mut DatabasePage,
                                                  window: &mut Window,
                                                  cx: &mut Context<DatabasePage>| {
                                                let Ok(next) = value.parse::<u64>() else {
                                                    notify::warning(
                                                        tr(cx, "请输入一个非负整数。"),
                                                        window,
                                                        cx,
                                                    );
                                                    return false;
                                                };
                                                let sql = sequence_setval_sql(
                                                    &schema,
                                                    &name,
                                                    next,
                                                );
                                                this.stage_sql(
                                                    sql,
                                                    tr(cx, "已生成 setval 语句；确认无误后再执行。")
                                                        .into(),
                                                    window,
                                                    cx,
                                                );
                                                true
                                            },
                                        ),
                                        window,
                                        cx,
                                    );
                                });
                            },
                        ));
                    }
                    if let Some(sql) = drop_object_sql.clone() {
                        let drop_object_view = view.clone();
                        menu = menu.item(
                            PopupMenuItem::new(drop_label).on_click(move |_, window, cx| {
                                let Some(entity) = drop_object_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.stage_sql(
                                        sql.clone(),
                                        drop_object_note.into(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                        );
                    }
                }
                // ── 导出 ──（表 / 视图都能导数据 —— `SELECT *` 对两者都合法；
                // 表结构 / 数据字典只有表有，视图没有列元数据口径之外的列说明。）
                // 表级「比较数据」：比较页的规格直接带上这一张（CompareSpec::tables）。
                if kind == TableKind::Table {
                    let compare_view = view.clone();
                    let compare_ref = browse_ref.clone();
                    let compare_conn = conn_owned.clone();
                    let compare_ns = menu_namespace.clone();
                    menu = menu.item(PopupMenuItem::new(compare_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = compare_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.open_compare_tab(
                                    &compare_conn,
                                    &compare_ns,
                                    vec![compare_ref.clone()],
                                    window,
                                    cx,
                                );
                            });
                        },
                    ));
                    // 表级「导入数据」：CSV 按列名对齐追加进这张表（分组级那项
                    // 是「导入成新表」，两条路不重合）。
                    let import_view = view.clone();
                    let import_ref = browse_ref.clone();
                    let import_conn = conn_owned.clone();
                    menu = menu.item(PopupMenuItem::new(import_into_label).on_click(
                        move |_, _, cx| {
                            let Some(entity) = import_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |_this, cx| {
                                let import_conn = import_conn.clone();
                                let import_ref = import_ref.clone();
                                crate::file_dialogs::open_file(
                                    cx,
                                    tr(cx, "选择要导入的 CSV 文件"),
                                    move |path, this, cx| {
                                        this.import_csv_append(
                                            &import_conn,
                                            import_ref.clone(),
                                            path,
                                            cx,
                                        );
                                    },
                                );
                            });
                        },
                    ));
                }
                if kind.is_relation() && menu_quote.is_some() {
                    // 导出数据 ▸：CSV / JSON / INSERT SQL 三档（同一份整表读取，
                    // 渲染走 ResultFormat——dbx 的导出子菜单对位）。INSERT INTO 对
                    // 视图通常无效，这一档只给表；标签在 build 前算好（builder 是
                    // `Fn`，不能在里面动 `cx`）。
                    let data_formats: Vec<(ResultFormat, &'static str)> =
                        [(ResultFormat::Csv, tr(cx, "导出 CSV")),
                         (ResultFormat::Json, tr(cx, "导出 JSON")),
                         (ResultFormat::InsertSql, tr(cx, "导出 INSERT 语句"))]
                            .into_iter()
                            .filter(|(format, _)| {
                                *format != ResultFormat::InsertSql
                                    || kind == TableKind::Table
                            })
                            .collect();
                    let data_menu = PopupMenu::build(window, cx, |sub, _, _| {
                        data_formats.iter().fold(sub, |sub, (format, label)| {
                            let item_view = view.clone();
                            let item_ref = browse_ref.clone();
                            let item_conn = conn_owned.clone();
                            let format = *format;
                            sub.item(
                                PopupMenuItem::new(*label).on_click(move |_, _, cx| {
                                    let Some(entity) = item_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.export_table_data_as(
                                            &item_conn,
                                            item_ref.clone(),
                                            format,
                                            cx,
                                        );
                                    });
                                }),
                            )
                        })
                    });
                    menu = menu.item(PopupMenuItem::submenu(export_data_label, data_menu));
                    if kind == TableKind::Table {
                        let ddl_export_view = view.clone();
                        let ddl_export_ref = browse_ref.clone();
                        let ddl_export_conn = conn_owned.clone();
                        menu = menu.item(PopupMenuItem::new(export_ddl_label).on_click(
                            move |_, _, cx| {
                                let Some(entity) = ddl_export_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.export_table_ddl(
                                        &ddl_export_conn,
                                        ddl_export_ref.clone(),
                                        cx,
                                    );
                                });
                            },
                        ));
                        let dict_view = view.clone();
                        let dict_ref = browse_ref.clone();
                        let dict_conn = conn_owned.clone();
                        menu = menu.item(PopupMenuItem::new(dict_label).on_click(
                            move |_, _, cx| {
                                let Some(entity) = dict_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.export_data_dictionary(
                                        &dict_conn,
                                        dict_ref.clone(),
                                        cx,
                                    );
                                });
                            },
                        ));
                        // 复制表结构为 ▸（TSV / Markdown）：数据字典的剪贴板版，
                        // 粘进文档 / IM 里讨论用（dbx 的 copy structure as 对位）。
                        let structure_formats = [
                            (false, tr(cx, "TSV")),
                            (true, tr(cx, "Markdown")),
                        ];
                        let structure_menu = PopupMenu::build(window, cx, |sub, _, _| {
                            structure_formats.iter().fold(sub, |sub, (markdown, label)| {
                                let item_view = view.clone();
                                let item_ref = browse_ref.clone();
                                let item_conn = conn_owned.clone();
                                let markdown = *markdown;
                                sub.item(PopupMenuItem::new(*label).on_click(
                                    move |_, _, cx| {
                                        let Some(entity) = item_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.copy_table_structure(
                                                &item_conn,
                                                item_ref.clone(),
                                                markdown,
                                                cx,
                                            );
                                        });
                                    },
                                ))
                            })
                        });
                        menu = menu.item(PopupMenuItem::submenu(
                            copy_structure_label,
                            structure_menu,
                        ));
                    }
                    menu = menu.separator();
                }
                // ── 刷新 / 删除 ──
                {
                    let refresh_view = view.clone();
                    let refresh_id = conn_owned.clone();
                    menu = menu.item(PopupMenuItem::new(refresh_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = refresh_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.activate_conn(&refresh_id, window, cx);
                                this.refresh_state(window, cx);
                            });
                        },
                    ));
                }
                menu
            }
        })
        .into_any_element();
        // 展开的表行下面铺结构分组（列 / 索引 / 外键 / 触发器）；收起与非表对象
        // 就只有这一行。分组是行的**兄弟**而不是行内子级 —— 再包一层缩进会让
        // 收起状态的表行比其他叶子深一列。
        if open {
            let mut node = v_flex().w_full().gap(tokens.spacing.xxs).child(row);
            node = node.child(self.sidebar_table_structure(conn_id, &reference, cx));
            node.into_any_element()
        } else {
            row
        }
    }

    /// 展开表节点时**惰性**取一次结构（列 / 索引 / 外键 / 触发器一次取齐）。
    ///
    /// 已缓存或在取的表直接返回：展开收起再展开不打服务器。四份内省各自兜底
    /// 落空清单 —— 结构取不全不该把整个节点卡死（与表页 [`TableMeta`] 同口径）。
    pub(super) fn load_table_tree(
        &mut self,
        conn_id: &str,
        table: &TableRef,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let key = table_tree_key(conn_id, table);
        if self.table_tree.contains_key(&key) || !self.table_tree_loading.insert(key.clone()) {
            return;
        }
        let conn = session.conn.clone();
        let table_for_work = table.clone();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                TableTreeMeta {
                    columns: guard.table_columns(&table_for_work).unwrap_or_default(),
                    indexes: guard.table_indexes(&table_for_work).unwrap_or_default(),
                    foreign_keys: guard
                        .table_foreign_keys(&table_for_work)
                        .unwrap_or_default(),
                    triggers: guard.table_triggers(&table_for_work).unwrap_or_default(),
                }
            },
            move |this, meta, _window, cx| {
                this.table_tree_loading.remove(&key);
                this.table_tree.insert(key.clone(), meta);
                cx.notify();
            },
        );
    }

    /// 表节点展开后的**结构分组**（列 / 索引 / 外键 / 触发器，DBX 同款四个分组）。
    ///
    /// 结构还没取回（首次展开、后台在取）时给一条「正在读取」说明行；取回后四个
    /// 分组各自可折叠，子级是纯展示行（名称 + 类型 / 组成列 / 指向等元信息）。
    /// 外键与触发器分组只给**有这两个概念**的引擎（与 `table_foreign_keys` /
    /// `table_triggers` 的分派同口径：ClickHouse / MongoDB / Elasticsearch 恒空，
    /// 挂出来就是两个永远为 0 的死节点）。
    fn sidebar_table_structure(&self, conn_id: &str, table: &TableRef, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let key = table_tree_key(conn_id, table);
        let Some(meta) = self.table_tree.get(&key) else {
            // 还在取：给一条说明行，别让人以为点了没反应。
            return div()
                .w_full()
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .child(meta_text(tr(cx, "正在读取表结构…"), cx))
                .into_any_element();
        };
        let engine = self
            .sessions
            .get(conn_id)
            .map_or(DatabaseKind::Sqlite, |session| session.kind);
        // 外键与触发器在引擎分派上**同一个集合**（`table_foreign_keys` 与
        // `table_triggers` 只认 SQLite / PG / MySQL / SQL Server），所以一个判据管两组：
        // ClickHouse / MongoDB / Elasticsearch 挂出来就是两个永远为 0 的死节点。
        let has_keys_and_triggers = matches!(
            engine,
            DatabaseKind::Sqlite
                | DatabaseKind::Postgres
                | DatabaseKind::Mysql
                | DatabaseKind::SqlServer
        );
        // 分组的四元组：稳定键 / 图标 / 标题源串 / 该引擎有没有这个概念。
        let groups = [
            ("columns", DuckIcon::Rows3, "列", true),
            ("indexes", DuckIcon::ListOrdered, "索引", true),
            ("fkeys", DuckIcon::Network, "外键", has_keys_and_triggers),
            ("triggers", DuckIcon::Zap, "触发器", has_keys_and_triggers),
        ];
        let mut node = v_flex()
            .w_full()
            .pl(tokens.spacing.md)
            .gap(tokens.spacing.xxs);
        for (group, icon, heading, shown) in groups {
            if !shown {
                continue;
            }
            let group_key = format!("tblg:{conn_id}:{}:{group}", table.display());
            let group_open = self.tree_open.contains(&group_key);
            let rows: Vec<AnyElement> = match group {
                "columns" => meta
                    .columns
                    .iter()
                    .map(|column| {
                        structure_leaf_row(
                            format!("db-col-{conn_id}-{}-{}", table.display(), column.name),
                            column.name.clone(),
                            (!column.type_name.is_empty()).then(|| column.type_name.clone()),
                            DuckIcon::Rows3,
                            cx,
                        )
                    })
                    .collect(),
                "indexes" => meta
                    .indexes
                    .iter()
                    .map(|index| {
                        structure_leaf_row(
                            format!("db-idx-{conn_id}-{}-{}", table.display(), index.name),
                            index.name.clone(),
                            (!index.columns.is_empty()).then(|| index.columns.join(", ")),
                            DuckIcon::ListOrdered,
                            cx,
                        )
                    })
                    .collect(),
                "fkeys" => meta
                    .foreign_keys
                    .iter()
                    .enumerate()
                    .map(|(ordinal, fkey)| {
                        // SQLite 的外键没有约束名：落空串时用「列 → 被引用列」当行名。
                        let label = if fkey.name.is_empty() {
                            format!(
                                "{} → {}",
                                fkey.columns.join(", "),
                                fkey.ref_columns.join(", ")
                            )
                        } else {
                            fkey.name.clone()
                        };
                        // 元素 id 用**序号**而不是约束名兜底：SQLite 的外键名恒空，
                        // 同一张表的多条外键会撞成同一个 id（引擎要求 id 逐元素唯一，
                        // 撞了就是后注册覆盖先注册、先那份静默失效）。
                        structure_leaf_row(
                            format!("db-fk-{conn_id}-{}-{ordinal}", table.display()),
                            label,
                            Some(fkey.ref_table.clone()),
                            DuckIcon::Network,
                            cx,
                        )
                    })
                    .collect(),
                "triggers" => meta
                    .triggers
                    .iter()
                    .map(|trigger| {
                        let detail = format!("{} {}", trigger.timing, trigger.event)
                            .trim()
                            .to_owned();
                        structure_leaf_row(
                            format!("db-trg-{conn_id}-{}-{}", table.display(), trigger.name),
                            trigger.name.clone(),
                            (!detail.is_empty()).then_some(detail),
                            DuckIcon::Zap,
                            cx,
                        )
                    })
                    .collect(),
                _ => Vec::new(),
            };
            node = node.child(self.sidebar_toggle_row(
                format!("db-structure-{conn_id}-{}-{group}", table.display()),
                group_key,
                icon,
                tr(cx, heading),
                Some(rows.len()),
                false,
                group_open,
                cx,
            ));
            if group_open {
                node = node.child(
                    v_flex()
                        .w_full()
                        .pl(tokens.spacing.md)
                        .gap(tokens.spacing.xxs)
                        .children(rows),
                );
            }
        }
        node.into_any_element()
    }

    /// 左栏树里的**库级对象叶子**：触发器 / 事件 / 用户与权限。
    ///
    /// 触发器点开跳到它挂的那张表（`detail` 就是表名）——这是它在这套界面里唯一
    /// 有意义的动作；事件与用户没有可打开的编辑器，右键给「复制名称」与「刷新」。
    pub(super) fn sidebar_database_object_row(
        &self,
        object: DatabaseObject,
        category: TreeCategory,
        conn_id: &str,
        namespace: &str,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let name = object.name.clone();
        let detail = object.detail.clone();
        let copy_label = tr(cx, "复制名称");
        let refresh_label = tr(cx, "刷新");
        let copied_note = tr(cx, "已复制名称。");
        // 文案在**闭包外**取好：`context_menu` 的闭包要活到元素被丢掉那一刻，
        // 捕获 `&App` 会让返回值的生命周期缩到方法体内（编译不过）。
        let jump_label = tr(cx, "打开所属表");
        // 触发器跳表：表名与 namespace 拼限定名（触发器属于**当前**库，所以用当前那一层）。
        let jump = (category == TreeCategory::Trigger).then(|| {
            let table = detail.clone()?;
            Some(TableRef {
                schema: Some(namespace.to_owned()),
                name: table,
            })
        });
        let jump = jump.flatten();
        // 引擎取自会话：库级对象菜单的方言差异全靠它。会话不在了按 SQLite 兜底，
        // 各 helper 会因引擎给不出出口而让对应菜单项自然隐藏。
        let engine = self
            .sessions
            .get(conn_id)
            .map_or(DatabaseKind::Sqlite, |session| session.kind);
        let definition_label = tr(cx, "查看定义");
        let drop_label = tr(cx, "生成删除语句");
        let grants_label = tr(cx, "查看权限");
        let drop_note = tr(cx, "已生成删除语句；确认无误后再执行。");
        let grants_note = tr(cx, "已生成权限查询语句，执行后可查看该用户的授权。");
        // 定义原文查询（触发器 / 事件 / 索引有出口）与删除语句（破坏性：只生成，
        // 跑不跑由用户决定）。MySQL 的索引删除必须带 `ON 表名`，detail 缺失就不给。
        let definition_sql = db_object_definition_query(category, engine, namespace, &name);
        let drop_sql = drop_db_object_sql(category, engine, &name, detail.as_deref());
        // 用户权限查询：MySQL 按 'user'@'host'（detail 就是 host），PG 查
        // role_table_grants；生成的是一条可执行的查询语句，编辑器里跑就是结果。
        let grants_sql = (category == TreeCategory::User)
            .then(|| user_grants_sql(engine, &name, detail.as_deref()))
            .flatten();

        let mut row = sidebar_navigation_button(
            SharedString::from(format!(
                "db-object-{conn_id}-{namespace}-{}-{}",
                category.key(),
                name
            )),
            SharedString::from(name.clone()),
            Icon::new(category.icon())
                .size(extended.icon.md)
                .text_color(tokens.colors.muted_foreground),
            // ⚠️ 行尾说明文字（索引 = 挂在哪张表、用户 = host…）必须**可被挤 +
            // 省略号**：它是行里唯一没有伸缩约束的元素，原始宽度一超过行宽就把
            // 前面「图标 + 名称」整组挤塌 —— lead 组是 `min_w_0`，能缩到 0；
            // 名称随之整条消失，`flex_shrink_0` 的图标从 0 宽的框里溢出来画在
            // 说明文字上（实机表现：索引分类里「图标压在文字上」，其实压的是
            // 表名，索引名已经被挤没了）。
            detail.as_ref().map_or_else(
                || div().flex_none().into_any_element(),
                |detail| {
                    meta_text(detail.clone(), cx)
                        .min_w_0()
                        .truncate()
                        .into_any_element()
                },
            ),
            false,
            cx,
        );
        // 触发器点开跳它挂的那张表：先挂 `on_click` 再挂 `context_menu` —— 后者返回的是
        // `ContextMenu<_>` 包装，它身上没有 `on_click`（挂了就编译不过）。
        if let Some(reference) = jump.as_ref() {
            let reference = reference.clone();
            let view = view.clone();
            let conn_owned = conn_id.to_owned();
            row = row.on_click(move |_, window, cx| {
                let Some(entity) = view.upgrade() else {
                    return;
                };
                entity.update(cx, |this, cx| {
                    this.open_data_tab(&conn_owned, reference.clone(), false, window, cx);
                });
            });
        }
        row.context_menu({
            let view = view.clone();
            let name = name.clone();
            let jump = jump.clone();
            // 闭包是 'static：`conn_id` 是借用，先在方法体里落成owned再进闭包。
            let conn_owned = conn_id.to_owned();
            move |menu, _, _| {
                let mut menu = menu;
                if let Some(reference) = jump.clone() {
                    let jump_view = view.clone();
                    let jump_conn = conn_owned.clone();
                    menu = menu.item(PopupMenuItem::new(jump_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = jump_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.open_data_tab(
                                    &jump_conn,
                                    reference.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            });
                        },
                    ));
                }
                // 查看定义（触发器 / 事件 / 索引）：跑引擎的定义原文查询，
                // 取回的 CREATE 语句放进编辑器。
                if let Some(query) = definition_sql.clone() {
                    let def_view = view.clone();
                    let def_conn = conn_owned.clone();
                    menu = menu.item(PopupMenuItem::new(definition_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = def_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.stage_object_definition(&def_conn, query.clone(), window, cx);
                            });
                        },
                    ));
                }
                // 查看权限（用户）：生成该用户的授权查询语句 —— MySQL 的
                // SHOW GRANTS / PG 的 role_table_grants，编辑器里执行就是结果。
                if let Some(sql) = grants_sql.clone() {
                    let grants_view = view.clone();
                    menu = menu.item(PopupMenuItem::new(grants_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = grants_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.stage_sql(sql.clone(), grants_note.into(), window, cx);
                            });
                        },
                    ));
                }
                // 生成删除语句（触发器 / 事件 / 索引）：破坏性动作只生成，
                // 确认无误后由用户执行。
                if let Some(sql) = drop_sql.clone() {
                    let drop_view = view.clone();
                    menu = menu.item(PopupMenuItem::new(drop_label).on_click(
                        move |_, window, cx| {
                            let Some(entity) = drop_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.stage_sql(sql.clone(), drop_note.into(), window, cx);
                            });
                        },
                    ));
                }
                let copy_view = view.clone();
                let copy_name = name.clone();
                let refresh_view = view.clone();
                menu.item(PopupMenuItem::new(copy_label).on_click(move |_, _, cx| {
                    let Some(entity) = copy_view.upgrade() else {
                        return;
                    };
                    let name = copy_name.clone();
                    entity.update(cx, |this, cx| {
                        this.put_on_clipboard(name, copied_note, cx);
                    });
                }))
                .item(
                    PopupMenuItem::new(refresh_label).on_click(move |_, window, cx| {
                        let Some(entity) = refresh_view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.refresh_state(window, cx);
                        });
                    }),
                )
            }
        })
        .into_any_element()
    }

    /// 右栏：连接信息 + 选中行详情。
    /// 右栏 dock 的分段头（详情 / 历史）。
    ///
    /// 设计稿 `docs/db-ui-mockup.html` 的 `.docktabs`：一条 hairlines 底边 + 两个 pill。
    /// **不做**设计稿里那个「收起右栏」的 `»`：收起 / 展开由 shell 的右栏按钮管
    /// （`duck-shell` 的 `render_right_sidebar_toggle`），页面里再摆一个就得能真的收起它，
    /// 否则就是个点了没反应的演示项（P3-6 判据）。
    pub(super) fn render_right_dock_tabs(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        // 取成局部 `Copy` 值再进闭包：闭包是 `move`，直接捕获 `tokens` / `theme`
        // 会把它们搬走，闭包外构造这一行时就没得用了。
        let tab_height = theme.density().control;
        let tab_px = tokens.spacing.sm;
        let tab_radius = tokens.radius.md;
        let tab_text = tokens.typography.sm.size;
        let tab_fg = tokens.colors.foreground;
        let tab_muted = tokens.colors.muted_foreground;
        let tab_selected = extended.colors.nav_selected;
        let tab_hover = extended.colors.hover_tint;

        let tab = {
            // 闭包是 `Fn`（两个分段各调一次）：句柄在外面克隆一份独占，
            // 每次调用再克隆给内层 `move` 闭包。
            let view = view.clone();
            move |label: &'static str, target: RightDock, active: bool| {
                let view = view.clone();
                div()
                    .id(SharedString::from(format!("db-dock-tab-{label}")))
                    .h(tab_height)
                    .px(tab_px)
                    .flex()
                    .items_center()
                    .rounded(tab_radius)
                    .cursor_pointer()
                    .text_size(tab_text)
                    .font_weight(if active {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if active { tab_fg } else { tab_muted })
                    .when(active, |el| el.bg(tab_selected))
                    .hover(move |style| style.bg(tab_hover))
                    .role(Role::Tab)
                    .aria_selected(active)
                    .aria_label(SharedString::from(label))
                    .on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            // 用户在右栏里主动挑分段：从这一刻起右栏归用户 —— 页面不再
                            // 因为它（可能曾是自己展开的）在内容失效时自动收起它。
                            this.right_dock_auto = false;
                            this.right_dock = target;
                            cx.notify();
                        });
                    })
                    .child(label)
            }
        };

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(tab(
                tr(cx, "详情"),
                RightDock::Detail,
                self.right_dock == RightDock::Detail,
            ))
            .child(tab(
                tr(cx, "历史"),
                RightDock::History,
                self.right_dock == RightDock::History,
            ))
            .child(tab(
                tr(cx, "单元格"),
                RightDock::Cell,
                self.right_dock == RightDock::Cell,
            ))
            .child(tab(
                tr(cx, "构建器"),
                RightDock::Builder,
                self.right_dock == RightDock::Builder,
            ))
            .child(tab(
                tr(cx, "AI 助手"),
                RightDock::Ai,
                self.right_dock == RightDock::Ai,
            ))
            .into_any_element()
    }

    /// 右栏「单元格」分段：dbx「单元格详情」对位 —— 这格是什么、值是什么、一键复制。
    ///
    /// 列的 NULL 约束 / 长度 / 注释元信息内核没有（[`duck_db::result::Column`] 只有
    /// 名字与类型名），有啥画啥，不冒充；`NULL` 一行画的是**这个值**是不是 NULL。
    pub(super) fn render_cell_detail_dock(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let Some((row, col)) = self.cell_detail else {
            return empty_state(
                DuckIcon::Info,
                tr(cx, "还没有选中的单元格"),
                Some(tr(cx, "在结果网格里点一格，或点行尾的 ⓘ。").into()),
                None,
                cx,
            )
            .into_any_element();
        };
        let Some(result) = self.current_result() else {
            return empty_state(DuckIcon::Info, tr(cx, "没有结果集"), None, None, cx)
                .into_any_element();
        };
        let Some(cell) = result.rows.get(row).and_then(|row| row.get(col)) else {
            // 指向的行 / 列在当前结果里越界了（重跑换结果后旧的指向不跟着改）。
            return empty_state(
                DuckIcon::Info,
                tr(cx, "这一格已经不在当前结果里"),
                Some(tr(cx, "重跑查询或重新点一格。").into()),
                None,
                cx,
            )
            .into_any_element();
        };
        let column = result.columns.get(col);
        let value = cell.render().to_string();
        let null_cell = cell.is_null();
        let view = self.view.clone();
        let value_for_copy = value.clone();
        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(caption_heading(tr(cx, "单元格详情"), cx))
            .child(
                card(cx)
                    .w_full()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.sm)
                    .child(description_list(vec![
                        description_row(
                            tr(cx, "列名"),
                            SharedString::from(
                                column.map(|column| column.name.clone()).unwrap_or_default(),
                            ),
                        ),
                        description_row(tr(cx, "行号"), SharedString::from((row + 1).to_string())),
                        description_row(
                            tr(cx, "类型"),
                            SharedString::from(
                                column
                                    .map(|column| column.type_name.clone())
                                    .filter(|name| !name.is_empty())
                                    .unwrap_or_else(|| tr(cx, "未知").to_string()),
                            ),
                        ),
                        description_row(
                            tr(cx, "NULL"),
                            SharedString::from(if null_cell {
                                tr(cx, "是")
                            } else {
                                tr(cx, "否")
                            }),
                        ),
                    ])),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.sm)
                    .child(caption_heading(tr(cx, "值"), cx))
                    .children(
                        match self
                            .cell_detail_editor
                            .as_ref()
                            .filter(|editor| editor.row == row && editor.col == col)
                        {
                            // 可编辑：输入框 + NULL 开关，「已改」点亮后「暂存更改」可点
                            //（改动进**暂存区**，与行编辑器 / JSON 表单同一条 `pending_edits`
                            // 路径 —— 点结果区上方的「提交」才落库，一处撤销栈不分裂）。
                            Some(editor) => {
                                let cell = &editor.cell;
                                let null_now = cell.null.get();
                                let text = cell.input.read(cx).value().to_string();
                                let changed = if null_now {
                                    !cell.was_null
                                } else {
                                    cell.was_null || text != cell.original
                                };
                                let null_cell = cell.null.clone();
                                // 「NULL 写不写」的开关位要被**两个**闭包读（切换与提交），
                                // 各持一份克隆 —— Rc 不可 Copy，先动的那个会把原值搬走。
                                let null_for_submit = cell.null.clone();
                                let toggle_view = view.clone();
                                let editor_row = editor.row;
                                let editor_col = editor.col;
                                let input_entity = cell.input.clone();
                                let submit_view = view.clone();
                                // 统一成 AnyElement：三件东西不是同一类型，Vec 会推成 Div。
                                vec![
                                    h_flex()
                                        .w_full()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            h_flex()
                                                .items_center()
                                                .gap(tokens.spacing.xs)
                                                .children(
                                                    changed
                                                        .then(|| neutral_tag(tr(cx, "已改"), cx)),
                                                ),
                                        )
                                        .child(
                                            h_flex()
                                                .flex_none()
                                                .items_center()
                                                .gap(tokens.spacing.xxs)
                                                .child(grid_checkbox(
                                                    SharedString::from("db-cell-detail-null"),
                                                    if null_now {
                                                        CheckState::Checked
                                                    } else {
                                                        CheckState::Unchecked
                                                    },
                                                    SharedString::from(trf(
                                                        cx,
                                                        "把「{}」这一格写成 NULL",
                                                        &[&cell.column],
                                                    )),
                                                    move |_, _, cx| {
                                                        null_cell.set(!null_cell.get());
                                                        let _ = toggle_view
                                                            .update(cx, |_, cx| cx.notify());
                                                    },
                                                    cx,
                                                ))
                                                .child(meta_text(tr(cx, "NULL"), cx)),
                                        )
                                        .into_any_element(),
                                    div()
                                        .w_full()
                                        .min_w_0()
                                        .when(null_now, |box_| {
                                            box_.opacity(theme.extended().opacity.disabled)
                                        })
                                        .child(
                                            Input::new(&cell.input)
                                                .h(theme.density().control)
                                                .text_size(tokens.typography.sm.size)
                                                .w_full(),
                                        )
                                        .into_any_element(),
                                    button(
                                        "db-cell-detail-stage",
                                        tr(cx, "暂存更改"),
                                        ButtonVariant::Primary,
                                        cx,
                                    )
                                    .disabled(self.busy || !changed)
                                    .on_click(move |_, _, cx| {
                                        let Some(entity) = submit_view.upgrade() else {
                                            return;
                                        };
                                        let null_now = null_for_submit.get();
                                        let text = input_entity.read(cx).value().to_string();
                                        entity.update(cx, |this, cx| {
                                            this.stage_cell_edit(
                                                editor_row,
                                                editor_col,
                                                if null_now { None } else { Some(text) },
                                                cx,
                                            );
                                        });
                                    })
                                    .into_any_element(),
                                ]
                            }
                            // 只读（多表 / 无主键 / 不可编辑结果）：照旧展示，不冒充能改。
                            None => {
                                vec![
                            div()
                                .min_h(px(40.))
                                .text_size(tokens.typography.sm.size)
                                .when(null_cell, |text| {
                                    text.text_color(extended.colors.text_tertiary)
                                })
                                .child(selectable_text(SharedString::from(value)))
                                .into_any_element(),
                            meta_text(
                                tr(cx, "这一格所在的结果集不可编辑（需要单表 + 主键），只能查看。"),
                                cx,
                            )
                            .into_any_element(),
                        ]
                            }
                        },
                    )
                    .child(
                        button(
                            "db-cell-copy-value",
                            tr(cx, "复制值"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(move |_, _, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.put_on_clipboard(
                                    value_for_copy.clone(),
                                    tr(cx, "已复制单元格。"),
                                    cx,
                                );
                            });
                        }),
                    ),
            )
            .into_any_element()
    }

    /// 右栏「表级元信息」：列 + 行数 + 索引 + 外键 + 触发器。
    ///
    /// 数据来自 [`Self::load_table_page`]（只在换表 / 刷新时取一次），这里纯展示，
    /// 渲染路径上没有数据库调用。清单为空的卡照画（显示空态文案）——「这表没有」
    /// 与「引擎没这个概念」（MongoDB / Elasticsearch 的外键与触发器恒空）在界面上
    /// 同形是**有意的**：不为引擎做 UI 特判。
    pub(super) fn render_table_meta(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();

        // 行数：口径直接写进文案（精确 vs 统计估算），别让用户以为估算值是实数。
        let row_count_text: SharedString = match self.table_row_count {
            Some(RowCount::Exact(count)) => trf(cx, "{} 行", &[&count.to_string()]).into(),
            // PG 从未 `ANALYZE` 过时 `reltuples` 是 -1（内核落成 0）。
            Some(RowCount::Estimate(0)) => tr(cx, "未知（未统计）").into(),
            Some(RowCount::Estimate(count)) => trf(cx, "约 {} 行", &[&count.to_string()]).into(),
            None => tr(cx, "未知").into(),
        };

        // ── 列卡：列名（+ 主键徽标）/ 类型，行右键复制列名 ──
        let mut column_list = v_flex().w_full().gap(tokens.spacing.sm);
        for column in &self.table_columns {
            let is_pk = self.table_pk.contains(&column.name);
            let copy_label = tr(cx, "复制列名");
            let copied_note = tr(cx, "已复制列名。");
            let column_name = column.name.clone();
            // 每轮一份：闭包是 `move`，不克隆会把下一轮的 `view` 一起搬走。
            let view = view.clone();
            column_list = column_list.child(
                h_flex()
                    .id(SharedString::from(format!("db-column-{}", column.name)))
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .justify_between()
                    .gap(tokens.spacing.xs)
                    .child(
                        h_flex()
                            .min_w_0()
                            .items_center()
                            .gap(tokens.spacing.xxs)
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(tokens.typography.sm.size)
                                    .child(selectable_text(SharedString::from(
                                        column.name.clone(),
                                    ))),
                            )
                            .children(is_pk.then(|| accent_tag(tr(cx, "主键"), cx))),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(selectable_text(SharedString::from(
                                column.type_name.clone(),
                            ))),
                    )
                    .context_menu(move |menu, _, _| {
                        let view = view.clone();
                        let column_name = column_name.clone();
                        let copied_note = copied_note;
                        menu.item(PopupMenuItem::new(copy_label).on_click(move |_, _, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.put_on_clipboard(column_name.clone(), copied_note, cx);
                            });
                        }))
                    }),
            );
        }
        if self.table_columns.is_empty() {
            column_list = column_list.child(meta_text(tr(cx, "没有列信息。"), cx));
        }

        // ── 索引卡：行右键复制名称 / 复制列清单 ──
        let mut index_list = v_flex().w_full().gap(tokens.spacing.sm);
        for index in &self.table_indexes {
            let columns: SharedString = if index.columns.is_empty() {
                // PG 的表达式索引取不到列名（`attnum = 0`）。
                tr(cx, "(表达式)").into()
            } else {
                SharedString::from(index.columns.join(", "))
            };
            let mut tags = h_flex().items_center().gap(tokens.spacing.xxs);
            if index.primary {
                tags = tags.child(accent_tag(tr(cx, "主键"), cx));
            }
            if index.unique && !index.primary {
                tags = tags.child(neutral_tag(tr(cx, "唯一"), cx));
            }
            let copy_name_label = tr(cx, "复制索引名");
            let copy_columns_label = tr(cx, "复制索引列");
            let copied_note = tr(cx, "已复制。");
            let index_name = index.name.clone();
            let index_columns = index.columns.clone();
            // 每轮一份（本行菜单有两个动作都要 `WeakEntity`）。
            let view = view.clone();
            index_list = index_list.child(
                v_flex()
                    .id(SharedString::from(format!("db-index-{}", index.name)))
                    .w_full()
                    .gap(tokens.spacing.xxs)
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .items_center()
                            .justify_between()
                            .gap(tokens.spacing.xs)
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(tokens.typography.sm.size)
                                    .child(selectable_text(SharedString::from(index.name.clone()))),
                            )
                            .child(tags),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(columns),
                    )
                    .context_menu(move |menu, _, _| {
                        let name_view = view.clone();
                        let columns_view = view.clone();
                        let index_name = index_name.clone();
                        let index_columns = index_columns.clone();
                        let copied_note = copied_note;
                        menu.item(
                            PopupMenuItem::new(copy_name_label).on_click(move |_, _, cx| {
                                let Some(entity) = name_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.put_on_clipboard(index_name.clone(), copied_note, cx);
                                });
                            }),
                        )
                        .item(
                            PopupMenuItem::new(copy_columns_label).on_click(move |_, _, cx| {
                                let Some(entity) = columns_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    let text = index_columns.join(", ");
                                    this.put_on_clipboard(text, copied_note, cx);
                                });
                            }),
                        )
                    }),
            );
        }
        if self.table_indexes.is_empty() {
            index_list = index_list.child(meta_text(tr(cx, "该表没有索引。"), cx));
        }

        // ── 外键卡：约束名（SQLite 没有约束名 → 以指向的表为主文案）+ 引用关系 ──
        let mut fk_list = v_flex().w_full().gap(tokens.spacing.sm);
        for fk in &self.table_foreign_keys {
            let mut tags = h_flex().flex_wrap().items_center().gap(tokens.spacing.xxs);
            // 动作只在有信息量时亮出来（`NO ACTION` / 空是缺省，不占地方）。
            for (label, action) in [
                (tr(cx, "更新"), &fk.on_update),
                (tr(cx, "删除"), &fk.on_delete),
            ] {
                if !action.is_empty() && action != "NO ACTION" {
                    tags = tags.child(neutral_tag(
                        SharedString::from(format!("{label} {action}")),
                        cx,
                    ));
                }
            }
            let copy_label = tr(cx, "复制外键定义");
            let copied_note = tr(cx, "已复制外键定义。");
            // 每轮一份：闭包是 `move`，不克隆会把下一轮的 `view` 一起搬走。
            let view = view.clone();
            let definition = format!(
                "{} -> {}({})",
                fk.columns.join(", "),
                fk.ref_table,
                fk.ref_columns.join(", ")
            );
            let display_name = if fk.name.is_empty() {
                SharedString::from(format!("→ {}", fk.ref_table))
            } else {
                SharedString::from(fk.name.clone())
            };
            fk_list = fk_list.child(
                v_flex()
                    .id(SharedString::from(format!(
                        "db-fk-{}-{}",
                        fk.name, fk.ref_table
                    )))
                    .w_full()
                    .gap(tokens.spacing.xxs)
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .items_center()
                            .justify_between()
                            .gap(tokens.spacing.xs)
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(tokens.typography.sm.size)
                                    .child(display_name),
                            )
                            .child(tags),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(selectable_text(SharedString::from(definition.clone()))),
                    )
                    .context_menu(move |menu, _, _| {
                        let view = view.clone();
                        let definition = definition.clone();
                        let copied_note = copied_note;
                        menu.item(PopupMenuItem::new(copy_label).on_click(move |_, _, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.put_on_clipboard(definition.clone(), copied_note, cx);
                            });
                        }))
                    }),
            );
        }
        if self.table_foreign_keys.is_empty() {
            fk_list = fk_list.child(meta_text(tr(cx, "该表没有外键。"), cx));
        }

        // ── 触发器卡：名称 + 时机徽标 / 事件 ──
        let mut trigger_list = v_flex().w_full().gap(tokens.spacing.sm);
        for trigger in &self.table_triggers {
            let copy_label = tr(cx, "复制触发器名");
            let copied_note = tr(cx, "已复制触发器名。");
            let trigger_name = trigger.name.clone();
            // 每轮一份：闭包是 `move`，不克隆会把下一轮的 `view` 一起搬走。
            let view = view.clone();
            let timing: SharedString = if trigger.timing.is_empty() {
                tr(cx, "触发器").into()
            } else {
                SharedString::from(trigger.timing.clone())
            };
            trigger_list = trigger_list.child(
                v_flex()
                    .id(SharedString::from(format!("db-trigger-{}", trigger.name)))
                    .w_full()
                    .gap(tokens.spacing.xxs)
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .items_center()
                            .justify_between()
                            .gap(tokens.spacing.xs)
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(tokens.typography.sm.size)
                                    .child(selectable_text(SharedString::from(
                                        trigger.name.clone(),
                                    ))),
                            )
                            .child(neutral_tag(timing, cx)),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(selectable_text(SharedString::from(trigger.event.clone()))),
                    )
                    .context_menu(move |menu, _, _| {
                        let view = view.clone();
                        let trigger_name = trigger_name.clone();
                        let copied_note = copied_note;
                        menu.item(PopupMenuItem::new(copy_label).on_click(move |_, _, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.put_on_clipboard(trigger_name.clone(), copied_note, cx);
                            });
                        }))
                    }),
            );
        }
        if self.table_triggers.is_empty() {
            trigger_list = trigger_list.child(meta_text(tr(cx, "该表没有触发器。"), cx));
        }

        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "列"), cx))
                    .child(
                        card(cx)
                            .id("db-columns-card")
                            .w_full()
                            .max_h(px(DETAIL_CARD_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.sm)
                            .child(column_list),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "行数"), cx))
                    .child(
                        card(cx)
                            .id("db-row-count-card")
                            .w_full()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.sm)
                            .child(description_list(vec![description_row(
                                tr(cx, "行数"),
                                row_count_text,
                            )])),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "索引"), cx))
                    .child(
                        card(cx)
                            .id("db-index-card")
                            .w_full()
                            // 同上：与 DDL 卡片共用同一条最大高度（原先 240 vs 260，已收敛）。
                            .max_h(px(DETAIL_CARD_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.sm)
                            .child(index_list),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "外键"), cx))
                    .child(
                        card(cx)
                            .id("db-fk-card")
                            .w_full()
                            .max_h(px(DETAIL_CARD_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.sm)
                            .child(fk_list),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "触发器"), cx))
                    .child(
                        card(cx)
                            .id("db-trigger-card")
                            .w_full()
                            .max_h(px(DETAIL_CARD_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.sm)
                            .child(trigger_list),
                    ),
            )
            .into_any_element()
    }

    /// 只读行详情卡：列名 → 展示值。
    pub(super) fn readonly_row_detail(&self, cx: &App) -> Option<AnyElement> {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let index = self.selected_row?;
        let result = self.current_result()?;
        let row = result.rows.get(index)?;
        let items: Vec<_> = row
            .iter()
            .enumerate()
            .map(|(cell_index, cell)| {
                let name = result
                    .columns
                    .get(cell_index)
                    .map(|column| SharedString::from(column.name.clone()))
                    .unwrap_or_else(|| SharedString::from(format!("col{cell_index}")));
                description_row(name, cell.render())
            })
            .collect();
        Some(
            card(cx)
                .w_full()
                .px(tokens.spacing.lg)
                .py(tokens.spacing.sm)
                .child(description_list(items))
                .into_any_element(),
        )
    }

    /// 编辑表单的**字段区**：每列一行「列名 + 当前值输入框 + 写 NULL 开关 + 已改标记」。
    ///
    /// 两个宿主共用：右栏行编辑卡（`slug = "row"`）与 JSON 档的字段表单（`slug = "json"`）。
    ///
    /// ⚠️ `slug` 唯一的用途是**错开元素 id**（`db-row-null-1` / `db-json-null-1`）：
    /// 同一个窗口里 ElementId 就是状态键，两份表单同时在场（右栏开着编辑卡 + 结果区停在
    /// JSON 档）时共用一格，勾选态会互相串。文案 / 版式一个字都不分叉。
    pub(super) fn row_editor_fields(&self, cells: &[RowCell], slug: &str, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();
        let mut fields = v_flex().w_full().gap(tokens.spacing.xs);
        for (index, cell) in cells.iter().enumerate() {
            let null_now = cell.null.get();
            // 「已改」的判据要把 NULL 开关算进来：`Some("")` 与 `None` 在库里是两件事，
            // 只比文本会把「把非空值改成 NULL」漏判成没改。
            let changed = if null_now {
                !cell.was_null
            } else {
                cell.was_null || cell.input.read(cx).value() != cell.original
            };

            let null_cell = cell.null.clone();
            let toggle_view = view.clone();
            let null_toggle = h_flex()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.xxs)
                .child(grid_checkbox(
                    SharedString::from(format!("db-{slug}-null-{index}")),
                    if null_now {
                        CheckState::Checked
                    } else {
                        CheckState::Unchecked
                    },
                    SharedString::from(trf(cx, "把「{}」这一格写成 NULL", &[&cell.column])),
                    move |_, _, cx| {
                        null_cell.set(!null_cell.get());
                        let _ = toggle_view.update(cx, |_, cx| cx.notify());
                    },
                    cx,
                ))
                .child(meta_text(tr(cx, "NULL"), cx));

            let mut marks = h_flex().items_center().gap(tokens.spacing.xs);
            if changed {
                marks = marks.child(neutral_tag(tr(cx, "已改"), cx));
            }
            marks = marks.child(null_toggle);

            let mut input_box = div().w_full().min_w_0().child(
                Input::new(&cell.input)
                    .h(theme.density().control)
                    .text_size(tokens.typography.sm.size)
                    .w_full(),
            );
            if null_now {
                // 开关打开时输入框只是淡出（值会被忽略），不真禁用：
                // 用户可能只是先勾了再看一眼原值，真禁掉会让内容不可见。
                input_box = input_box.opacity(theme.extended().opacity.disabled);
            }

            fields = fields
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .child(field_label(cell.column.clone(), cx))
                        .child(marks),
                )
                .child(input_box);
        }
        fields.into_any_element()
    }

    /// 行编辑卡（暂存式）：每列一个输入框，保存 / 丢弃 / 删行。
    pub(super) fn render_row_editor(&self, editor: &RowEditor, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();
        // 能不能改由**结果集**说了算（表浏览的当前表 / 可编辑 SELECT 背后的表），
        // 不再看 `table_pk`：那个字段只在表浏览路径上被填过。
        let editable = self.edit_source().is_some();
        let is_new = editor.row_index.is_none();

        let fields = self.row_editor_fields(&editor.cells, "row", cx);

        let row_index = editor.row_index;
        let mut footer = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(
                button(
                    "db-row-save",
                    if is_new {
                        tr(cx, "加入暂存")
                    } else {
                        tr(cx, "暂存更改")
                    },
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(self.busy || !editable)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.stage_row_edits(window, cx));
                    }
                }),
            );
        if !is_new {
            footer = footer.child(
                button(
                    "db-row-delete",
                    tr(cx, "删除行"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(self.busy || !editable)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let Some(row_index) = row_index else {
                            return;
                        };
                        let _ = view.update(cx, |this, cx| {
                            this.delete_current_row(row_index, window, cx);
                        });
                    }
                }),
            );
        }
        footer = footer.child(
            button("db-row-discard", tr(cx, "关闭"), ButtonVariant::Ghost, cx)
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.row_editor = None;
                            this.selected_row = None;
                            cx.notify();
                        });
                    }
                }),
        );

        let mut panel = v_flex().w_full().gap(tokens.spacing.sm).child(fields);
        panel = panel.child(meta_text(
            if is_new {
                tr(cx, "没填的列会落到数据库默认值。")
            } else {
                tr(cx, "改动先暂存，点结果区上方的「提交」才落库。")
            },
            cx,
        ));
        if !editable {
            panel = panel.child(meta_text(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键齐全）。"),
                cx,
            ));
        }
        panel = panel.child(footer);
        card(cx)
            .id("db-row-editor")
            .w_full()
            .min_h_0()
            .overflow_y_scroll()
            .p(tokens.spacing.lg)
            .child(panel)
            .into_any_element()
    }
}

/// 过滤操作符在下拉里的**稳定标识**（ASCII，只做下拉的 value，不进 i18n）。
///
/// 用名字而不是「在 [`duck_db::FilterOp::ALL`] 里的下标」：下标会随着清单增删而漂移，
/// 名字不会。
pub(super) fn filter_op_key(op: FilterOp) -> &'static str {
    match op {
        FilterOp::Eq => "eq",
        FilterOp::NotEq => "ne",
        FilterOp::Gt => "gt",
        FilterOp::Gte => "ge",
        FilterOp::Lt => "lt",
        FilterOp::Lte => "le",
        FilterOp::Like => "like",
        FilterOp::IsNull => "null",
        FilterOp::IsNotNull => "notnull",
    }
}

/// 从下拉回读操作符；认不出的标识回落到 `=`（下拉项是自家人，但兜底好过崩）。
pub(super) fn filter_op_from_key(key: &str) -> FilterOp {
    FilterOp::ALL
        .into_iter()
        .find(|op| filter_op_key(*op) == key)
        .unwrap_or(FilterOp::Eq)
}

/// 操作符的展示文案。
///
/// 符号类（`=` / `!=` / `>` …）是**语言无关**的，直接给符号、不进 i18n 表；
/// 词类的两条（为空 / 不为空）才登记。`LIKE` 照抄 SQL 里的操作符名——翻成「包含」
/// 反而会误导（`a%b` 是模式，不是子串匹配）。
pub(super) fn filter_op_label(op: FilterOp, cx: &App) -> &'static str {
    match op {
        FilterOp::Eq => "=",
        FilterOp::NotEq => "!=",
        FilterOp::Gt => ">",
        FilterOp::Gte => ">=",
        FilterOp::Lt => "<",
        FilterOp::Lte => "<=",
        FilterOp::Like => "LIKE",
        FilterOp::IsNull => tr(cx, "为空"),
        FilterOp::IsNotNull => tr(cx, "不为空"),
    }
}

/// 一条生效条件的标签文本：`列 操作符 值`；`为空` / `不为空` 没有值段。
pub(super) fn filter_chip_text(filter: &TableFilter, cx: &App) -> SharedString {
    let op = filter_op_label(filter.op, cx);
    match filter.value.as_deref() {
        Some(value) => SharedString::from(format!("{} {op} {value}", filter.column)),
        None => SharedString::from(format!("{} {op}", filter.column)),
    }
}
