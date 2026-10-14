use super::*;

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
                Ok((result, pk, ddl, meta)) => {
                    let rows = result.row_count();
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
                    let widths = compute_column_widths(&result);
                    if widths != this.column_widths {
                        this.result_x_scroll.set_offset(point(px(0.), px(0.)));
                    }
                    this.column_widths = widths;
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
        // 用完即清：这一格是「一次性输入」，留着旧页码下次按回车会显得莫名其妙。
        self.page_jump
            .update(cx, |state, cx| state.set_value("", window, cx));
        let Ok(page) = raw.parse::<usize>() else {
            return;
        };
        if page == 0 {
            return;
        }
        let target = page - 1;
        match self.mode {
            Mode::Query => {
                self.query_page = target;
                cx.notify();
            }
            Mode::Table => {
                if self.busy || self.table_page == target {
                    return;
                }
                self.table_page = target;
                self.load_table_page(cx);
            }
            // 实例页 / 仪表板没有「页」的概念，跳页输入框在这两处也不出现。
            Mode::Instance(_) | Mode::Dashboard(_) => {}
        }
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

    /// 翻页条上的「每页行数」下拉：就地改 `db.settings.page_size`。
    ///
    /// 与设置页的那一项是**同一个键**（[`module_settings::set_db_page_size`]），
    /// 差别只是入口：翻页的人正在看行数，让他为此跑一趟设置页没有道理。
    pub(super) fn browse_page_size_dropdown(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let extended = theme.extended();
        let view = self.view.clone();
        let current = browse_page_size(cx) as u32;
        toolbar_dropdown_button(
            "db-browse-page-size",
            SharedString::from(trf(cx, "{} 行/页", &[&current.to_string()])),
            Icon::new(DuckIcon::ListOrdered).size(extended.icon.sm),
            Anchor::TopRight,
            move |menu, _, _| {
                BROWSE_PAGE_CHOICES.iter().fold(menu, |menu, size| {
                    let view = view.clone();
                    let size = *size;
                    menu.item(
                        PopupMenuItem::new(SharedString::from(size.to_string()))
                            .checked(size == current)
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.set_browse_page_size(size, cx);
                                });
                            }),
                    )
                })
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
                    leading.child(kind_logo(cfg.kind).element(px(CONN_LOGO_SIZE)))
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
                            .child(kind_label(cfg.kind)),
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
                    // 连接节点右键：编辑 / 断开（仅已连时） / 删除（自带二次确认）。
                    // 与右上角的编辑齿轮重复是**有意的**：DBX / TablePlus 的连接节点
                    // 右键都是第一入口，不用去找小图标。
                    .context_menu({
                        let view = view.clone();
                        let menu_id = id.clone();
                        let is_connected = connected;
                        let edit_label = tr(cx, "编辑连接");
                        let disconnect_label = tr(cx, "断开连接");
                        let delete_label = tr(cx, "删除连接");
                        move |menu, _, _| {
                            let edit_view = view.clone();
                            let edit_id = menu_id.clone();
                            let mut menu = menu.item(PopupMenuItem::new(edit_label).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = edit_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.edit_connection(&edit_id, window, cx);
                                    });
                                },
                            ));
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
        label: &'static str,
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
            SharedString::from(label),
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

    /// 左栏「已保存查询」入口 + 它的子级（逐条列出，点一下载入编辑器）。
    ///
    /// 与「数据库」同层、同在连接节点下：它是**按连接浏览时的取用入口**，不是全局
    /// 设置项（设置页那边放的是「保留多少条历史」这类偏好）。
    pub(super) fn sidebar_saved_queries_section(
        &self,
        conn_id: &str,
        query: &str,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let key = format!("sec:saved:{conn_id}");
        let open = !query.is_empty() || self.tree_open.contains(&key);
        let hits: Vec<&SavedQuery> = self
            .saved_queries
            .iter()
            .filter(|saved| object_matches(&saved.name, query))
            .collect();
        let mut section = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .child(self.sidebar_toggle_row(
                format!("db-section-saved-{conn_id}"),
                key,
                DuckIcon::Star,
                tr(cx, "已保存查询"),
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
        for saved in hits {
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
            children = children.child(
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
                }),
            );
        }
        section = section.child(children);
        section.into_any_element()
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
        let names = collect_namespaces(&session.databases, &session.schemas);
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
            .child(self.sidebar_saved_queries_section(id, query, cx))
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
        let database_objects: Vec<&DatabaseObject> = session
            .database_objects(category)
            .iter()
            .filter(|object| object_matches(&object.name, needle))
            .collect();
        let count = objects.len() + database_objects.len();
        if count == 0 {
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

        let mut node = v_flex().w_full().gap(tokens.spacing.xxs).child(
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
            }),
        );
        // 收起时不画子节点：树里那一堆对象行是**元素**，不是数据显示——留在树上
        // 只是看不见，白白占布局与命中测试。
        if open {
            node = node.child(children);
        }
        Some(node.into_any_element())
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
        // 结构管理（只给**表**）：语句生成到编辑器，不在这里执行。
        // 视图 / 函数 / 序列各有各的改法（`CREATE OR REPLACE` / `ALTER FUNCTION`
        // / `ALTER SEQUENCE`），不在口径里，给了反而是半个按钮。
        let structure_sql: Option<(String, String)> = (kind == TableKind::Table)
            .then(|| {
                let quote = engine.identifier_quote()?;
                let qualified = quote_qualified(&reference, quote).ok()?;
                Some((
                    alter_table_template(engine, &qualified)?,
                    drop_table_template(engine, &qualified)?,
                ))
            })
            .flatten();
        let alter_label = tr(cx, "修改结构");
        let alter_note = tr(cx, "已生成改表骨架，改好后执行。");
        let drop_label = tr(cx, "生成删除语句");
        let drop_note = tr(cx, "已生成删表语句；确认无误后再执行。");

        sidebar_navigation_button(
            SharedString::from(format!("db-object-{}", reference.display())),
            SharedString::from(entry.name.clone()),
            Icon::new(table_kind_icon(kind))
                .size(extended.icon.md)
                .text_color(if selected {
                    extended.colors.accent_text
                } else {
                    tokens.colors.muted_foreground
                }),
            neutral_tag(table_kind_label(kind, cx), cx),
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
        // 对象行右键：四个常用动作，都不必先选中对象。
        // 「生成 SELECT」只对表 / 视图开放（存储过程与函数的骨架是 CALL / SELECT，
        // 序列是 nextval，都不叫 SELECT）。
        .context_menu({
            let view = view.clone();
            let object_name = reference.display();
            let browse_ref = reference.clone();
            let snippet = snippet.clone();
            move |menu, _, _| {
                let browse_view = view.clone();
                let browse_ref = browse_ref.clone();
                let browse_conn = conn_owned.clone();
                let select_view = view.clone();
                let copy_view = view.clone();
                let refresh_view = view.clone();
                let snippet = snippet.clone();
                let object_name = object_name.clone();
                let mut menu = menu
                    .item(
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
                    )
                    .item(
                        PopupMenuItem::new(select_label)
                            .disabled(!browsable)
                            .on_click(move |_, window, cx| {
                                let Some(entity) = select_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.sql.update(cx, |state, cx| {
                                        state.set_value(snippet.clone(), window, cx);
                                    });
                                    this.status = tr(cx, "已把引用骨架放进编辑器。").into();
                                    cx.notify();
                                });
                            }),
                    )
                    .item(PopupMenuItem::new(copy_label).on_click(move |_, _, cx| {
                        let Some(entity) = copy_view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.put_on_clipboard(object_name.clone(), copied_note, cx);
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
                    );
                // 结构管理两项放最后并加分隔线：它们与上面四个「只看不改」的
                // 动作不是一类（尤其删表），混在一起会点错。
                if let Some((alter, drop)) = structure_sql.as_ref() {
                    let alter = alter.clone();
                    let drop = drop.clone();
                    let alter_view = view.clone();
                    let drop_view = view.clone();
                    menu = menu
                        .separator()
                        .item(
                            PopupMenuItem::new(alter_label).on_click(move |_, window, cx| {
                                let Some(entity) = alter_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.stage_sql(alter.clone(), alter_note.into(), window, cx);
                                });
                            }),
                        )
                        .item(
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
                menu
            }
        })
        .into_any_element()
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
            detail.as_ref().map_or_else(
                || div().into_any_element(),
                |detail| meta_text(detail.clone(), cx).into_any_element(),
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
                    .child(
                        card(cx)
                            .w_full()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.sm)
                            .child(
                                div()
                                    .min_h(px(40.))
                                    .text_size(tokens.typography.sm.size)
                                    .when(null_cell, |text| {
                                        text.text_color(extended.colors.text_tertiary)
                                    })
                                    .child(SharedString::from(value)),
                            ),
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
                                    .child(SharedString::from(column.name.clone())),
                            )
                            .children(is_pk.then(|| accent_tag(tr(cx, "主键"), cx))),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(column.type_name.clone())),
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
                                    .child(SharedString::from(index.name.clone())),
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
                            .child(SharedString::from(definition.clone())),
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
                                    .child(SharedString::from(trigger.name.clone())),
                            )
                            .child(neutral_tag(timing, cx)),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(trigger.event.clone())),
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

    /// 行编辑卡（暂存式）：每列一个输入框，保存 / 丢弃 / 删行。
    pub(super) fn render_row_editor(&self, editor: &RowEditor, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();
        // 能不能改由**结果集**说了算（表浏览的当前表 / 可编辑 SELECT 背后的表），
        // 不再看 `table_pk`：那个字段只在表浏览路径上被填过。
        let editable = self.edit_source().is_some();
        let is_new = editor.row_index.is_none();

        let mut fields = v_flex().w_full().gap(tokens.spacing.xs);
        for (index, cell) in editor.cells.iter().enumerate() {
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
                    SharedString::from(format!("db-row-null-{index}")),
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
