use super::*;

impl DatabasePage {
    /// 摘要「执行过程」里的**来源语句 chip**（dbx 的「SQL 结果」tag 同款）：单行化显示 + 左键复制。
    ///
    /// dbx 那个 tag 就是「点一下把这条语句拿走、悬停看全」——本仓的 `query_sql` 恰好
    /// 恒等于**当前结果标签**的语句（多语句批次切标签时跟着换），所以这里显示与复制的
    /// 天然是「这一份结果对应的那条语句」。
    ///
    /// 截断只影响**显示**：复制拿到的是原文（含换行与缩进），粘回编辑器能直接用；
    /// 完整语句也在悬停浮层里。
    pub(super) fn result_sql_chip(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let sql = self.query_sql.as_deref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        // 换行与缩进压成一个空格：读数条只放得下一行。
        let one_line = one_line_sql(sql);
        let tip = SharedString::from(one_line.clone());
        let view = self.view.clone();
        let copied = sql.to_owned();
        Some(
            div()
                .id("db-result-sql-chip")
                .min_w_0()
                .truncate()
                .rounded(tokens.radius.sm)
                .bg(extended.colors.row_hover)
                .px(tokens.spacing.xs)
                .py(px(2.))
                .text_size(tokens.typography.xs.size)
                .text_color(tokens.colors.muted_foreground)
                .cursor_pointer()
                .hover(move |style| style.bg(extended.colors.hover_tint))
                .role(Role::Button)
                .aria_label(tr(cx, "复制 SQL"))
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .on_click(move |_, _, cx| {
                    let Some(entity) = view.upgrade() else {
                        return;
                    };
                    let sql = copied.clone();
                    entity.update(cx, |this, cx| {
                        this.put_on_clipboard(sql, tr(cx, "已复制该 SQL。"), cx);
                    });
                })
                .child(SharedString::from(one_line))
                .into_any_element(),
        )
    }

    /// 一条结果标签的显示名：**目标表 + 「-结果」**。
    ///
    /// 与 dbx 的 `resultTabNamingMode = source`（默认档）同一条口径 —— 标签说的是
    /// 「这份结果是哪张表 / 哪条语句出来的」，而不是它在批次里的序号；dbx 的
    /// `resultNamingLabel` 也是先取 SQL 解析出来的来源表（`FROM` / 写目标），
    /// 解析不出来才回落「结果 N」（`tabs.resultN`）。
    ///
    /// - 目标表名取 `ResultTab::target`（执行时由 SQL 解析出的可编辑目标表，
    ///   与结果集来源同一份解析），用**裸表名**：`schema` 不进标签，标签格宽有限。
    /// - `multiple = true` 时补序号：同一条 SQL 跑两遍会有两个同名标签，光看名字
    ///   分不出谁是谁。
    /// - 解析不出目标表（纯 `SELECT 1`、多表连接取不到唯一源、DDL 等）时回落
    ///   「结果 N」——名字退化了，标签本身不能因此消失。
    pub(super) fn result_tab_label(
        &self,
        index: usize,
        multiple: bool,
        cx: &Context<Self>,
    ) -> SharedString {
        let ordinal = (index + 1).to_string();
        let table = self
            .result_tabs
            .get(index)
            .and_then(|tab| tab.target.as_ref())
            .map(|target| target.name.clone());
        match (table, multiple) {
            (Some(name), true) => SharedString::from(trf(cx, "{}-结果{}", &[&name, &ordinal])),
            (Some(name), false) => SharedString::from(trf(cx, "{}-结果", &[&name])),
            (None, _) => SharedString::from(trf(cx, "结果 {}", &[&ordinal])),
        }
    }

    /// 结果区**结果标签行**（设计稿 `resbar`）：一排可切的结果标签（一条语句一个
    /// 标签，单结果也摆）+ 已隐藏列 / 排序读数 + DDL 入口。
    ///
    /// 「结果 · N 行 / 耗时 / 来源语句」不再上这排 —— 摘要的「执行过程」区块在说
    /// 同一件事，两处重复（用户实测反馈）；隐藏列与排序是这排独有的读数，留下。
    /// 「字段筛选 / 跳转列」已迁去档位行的动作组（排在「回滚」之后）。
    pub(super) fn render_result_resbar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        // 这一条是结果区的**第一行**，没有内容时不能留一条空行 + 一条悬空的分隔线。
        // 标签（查询模式且已有结果）与读数（藏列 / 排序 / DDL）两边都没有就整条让位 ——
        // 表浏览下「没排过序、没藏列、没取到 DDL」是最常见的情形，别在那儿摆一条空带。
        let has_tabs = self.mode == Mode::Query && !self.result_tabs.is_empty();
        let has_readings = !self.hidden_columns.is_empty()
            || self.table_sort.is_some()
            || (self.mode == Mode::Table && self.table_ddl.is_some());
        if !has_tabs && !has_readings {
            return div().into_any_element();
        }

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xxs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            // **一条语句一个结果标签**（dbx 同款：dbx 的 `ResultSetNavigator` +
            // `resultTabNamingMode = source`）。单结果也摆 —— 点「执行」之后必须看得见
            // 结果落在哪条语句上，只有一个标签的切换条不是噪音，而是**这份结果的身份**
            // （标签名见 `result_tab_label`）。限定查询模式：标签装的是语句结果，表浏览下
            // 点了也不该换（`current_result()` 那时给的是表行）。
            .when(has_tabs, |bar| {
                let multi = self.result_tabs.len() > 1;
                let mut tabs = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
                for index in 0..self.result_tabs.len() {
                    let active = index == self.active_result_tab;
                    let view = self.view.clone();
                    // 关标签的 ✕ 是标签格里的**独立点击目标**：不截停冒泡的话点它
                    // 会先把「切到这条标签」触发一遍再关掉，白闪一下。
                    let close_view = self.view.clone();
                    let label = self.result_tab_label(index, multi, cx);
                    tabs = tabs.child(
                        div()
                            .id(SharedString::from(format!("db-result-tab-{index}")))
                            .h(theme.density().control)
                            .pl(tokens.spacing.sm)
                            .pr(tokens.spacing.xs)
                            .flex()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .rounded(tokens.radius.md)
                            .cursor_pointer()
                            .text_size(tokens.typography.xs.size)
                            .font_weight(if active {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if active {
                                tokens.colors.foreground
                            } else {
                                tokens.colors.muted_foreground
                            })
                            .when(active, |el| el.bg(extended.colors.nav_selected))
                            .hover(move |style| style.bg(extended.colors.hover_tint))
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.switch_result_tab(index, cx);
                                });
                            })
                            .child(label)
                            .child(
                                div()
                                    .id(SharedString::from(format!("db-result-tab-close-{index}")))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .p(tokens.spacing.xxs)
                                    .rounded(tokens.radius.sm)
                                    .cursor_pointer()
                                    .aria_label(tr(cx, "关闭结果"))
                                    .role(Role::Button)
                                    .hover(move |style| style.bg(extended.colors.hover_tint))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        let _ = close_view.update(cx, |this, cx| {
                                            this.close_result_tab(index, cx);
                                        });
                                    })
                                    .child(
                                        Icon::new(DuckIcon::X)
                                            .size(extended.icon.sm)
                                            .text_color(tokens.colors.muted_foreground),
                                    ),
                            ),
                    );
                }
                bar.child(tabs).child(
                    div()
                        .w(px(1.))
                        .h(px(14.))
                        .flex_none()
                        .bg(extended.colors.hairline),
                )
            })
            // 藏过列时给个读数：网格凭空少了几列而界面没有别的线索，用户会以为
            // 查询少选了字段。
            .when(!self.hidden_columns.is_empty(), |bar| {
                bar.child(meta_text(
                    trf(
                        cx,
                        "已隐藏 {} 列",
                        &[&self.hidden_columns.len().to_string()],
                    ),
                    cx,
                ))
            })
            // 排序读数（原编辑条迁来）：表浏览按列排序后如实说一句，不然「顺序
            // 不对」没有任何线索。排序只在表浏览存在，查询结果不受它影响。
            .when_some(self.table_sort.as_ref(), |bar, spec| {
                bar.child(neutral_tag(
                    SharedString::from(trf(
                        cx,
                        "按 {} {}",
                        &[&spec.column, if spec.descending { "降序" } else { "升序" }],
                    )),
                    cx,
                ))
            })
            // 弹性带把读数与右端入口分开；来源语句 chip 已撤（摘要「执行过程」
            // 里的语句可复制，历史与右栏详情也能看全文）。
            .child(div().flex_1().min_w_0())
            // 字段筛选 / 跳转列已并入 tab 栏动作组（回滚之后）。
            // DDL 入口（原对象头条迁来）：只在表浏览且当前表取到了 DDL 时出现，
            // 与右栏 DDL 卡的「放进编辑器」同一条 `stage_sql` 路径 —— 右栏可能
            // 被折叠，这里是常驻入口。
            .when(
                self.mode == Mode::Table && self.table_ddl.is_some(),
                |bar| {
                    bar.child(
                        button(
                            "db-result-ddl",
                            tr(cx, "在编辑器中修改 DDL"),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = self.view.clone();
                            let ddl = self.table_ddl.clone().unwrap_or_default();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.stage_sql(
                                        ddl.clone(),
                                        tr(cx, "已把当前 DDL 放进编辑器，改好后执行。").into(),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        }),
                    )
                },
            )
            .into_any_element()
    }

    /// 结果区脚注（设计稿 `grid-foot`）：视图三档切换 + 行数 / 耗时读数；右端
    /// 导出与（表浏览的）翻页组。
    ///
    /// 全部是「这一次结果」的**常驻**事实——工具条中部那份 status 是易逝的操作
    /// 反馈，翻完页 / 切完标签就没了；这里跟结果共存亡。来源语句挪去了读数条
    /// （设计稿位置），脚注只留读数与动作。
    pub(super) fn render_result_footer(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let result = self.current_result();
        let rows = result.as_ref().map(|result| result.rows.len()).unwrap_or(0);
        let count = trf(cx, "{} 行", &[&rows.to_string()]);

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xxs)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            // 视图三档切换放**最前面**（第三轮指定位置）：先定「怎么看这份结果」，
            // 再读行数 / 耗时这些读数。
            .child(self.result_view_switch(cx))
            .child(meta_text(count, cx))
            .when_some(self.query_elapsed_ms, |bar, ms| {
                bar.child(meta_text(
                    SharedString::from(trf(cx, "{} ms", &[&ms.to_string()])),
                    cx,
                ))
            })
            // 搜索生效时补一个命中读数：网格少了一片行而脚注没说，用户会以为
            // 查询本身只返回了这么点。
            .when_some(
                result
                    .as_ref()
                    .filter(|_| !self.result_search_text(cx).is_empty())
                    .map(|result| {
                        let visible = visible_columns(&result.columns, &self.hidden_columns);
                        matched_row_indexes(&result.rows, &visible, &self.result_search_text(cx))
                            .len()
                    }),
                |bar, hits| {
                    bar.child(meta_text(
                        SharedString::from(trf(cx, "命中 {} 行", &[&hits.to_string()])),
                        cx,
                    ))
                },
            )
            .when(
                self.current_result().is_some_and(|result| result.truncated),
                |bar| bar.child(neutral_tag(tr(cx, "已达上限"), cx)),
            )
            .child(div().flex_1().min_w_0())
            // dbx grid-foot 同款的右端三件套：「N 行/页」下拉 → 紧凑翻页组
            // （« ‹ [页码] › »）→ 导出菜单。翻页组**常驻**（只有一页时按钮
            // 置灰，不再整组消失 —— dbx 同款，空一页也是页）。
            // 「保存结果」导出在 tab 栏（QueryResultToolbarActions 同款位置）
            // 也有一份 —— dbx 两处都有，footer 这份照用户给的截图落位。
            .when(matches!(self.mode, Mode::Query | Mode::Table), |bar| {
                bar.child(match self.mode {
                    Mode::Query => self.query_page_size_dropdown(cx),
                    _ => self.browse_page_size_dropdown(cx),
                })
                .child(self.render_pager_compact(cx))
                .child(self.export_dropdown(cx))
            })
            .into_any_element()
    }

    /// footer 的**紧凑翻页组**（dbx grid-foot 同款：`« ‹ [页码] › »`）。
    ///
    /// 两种模式共用一条版式：查询 = 客户端分页（总页数已知，五个钮都能判
    /// 置灰）；表浏览 = 服务器侧分页（总页数未知，「»」永远置灰 —— 装作知道
    /// 再点空是骗人）。**常驻**：只有一页时按钮置灰但整条还在（dbx 同款，
    /// 整组消失会让人找不到页码输入框）。「跳至某页」回车即跳，见
    /// [`Self::jump_to_page`]（两种模式同一份输入框）。
    pub(super) fn render_pager_compact(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        // ⚠️ 点击闭包必须写全参数类型：省略会让 `&ClickEvent` 绑到单一生命周期，
        // 编译器报「implementation of Fn is not general enough」。
        let goto = |target: usize| {
            let view = view.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let Some(entity) = view.upgrade() else {
                    return;
                };
                entity.update(cx, |this, cx| match this.mode {
                    // 查询 = 客户端分页：改序号即刻生效。
                    Mode::Query => {
                        this.query_page = target;
                        cx.notify();
                    }
                    // 表浏览 = 服务器侧分页：**只走本页 / 下一页的既有路径**
                    // （它们带着「本页满页才前进」的守卫）；往前跳直接设页码重取，
                    // 与「跳至某页」同一口径。
                    Mode::Table => match target.cmp(&this.table_page) {
                        std::cmp::Ordering::Less => {
                            this.table_page = target;
                            this.load_table_page(cx);
                        }
                        std::cmp::Ordering::Equal => {}
                        std::cmp::Ordering::Greater => {
                            while this.table_page < target {
                                this.table_next_page(cx);
                            }
                        }
                    },
                    Mode::Instance(_) | Mode::Dashboard(_) => {}
                });
            }
        };
        // 置灰与页码读数按模式分开算：查询知道总页数（可判「»」），表浏览
        // 只能判「下一页还有没有货」（本页行数 ≥ 页大小）。
        let (page, pages, can_next) = match self.mode {
            Mode::Query => {
                let total = self
                    .current_result()
                    .map(|result| result.rows.len())
                    .unwrap_or(0);
                let page_size = self.query_page_size(cx);
                let pages = total.div_ceil(page_size).max(1);
                (self.query_page.min(pages - 1), Some(pages), None)
            }
            Mode::Table => {
                let page_size = browse_page_size(cx);
                let can_next = self
                    .table_rows
                    .as_ref()
                    .is_some_and(|rows| rows.row_count() >= page_size);
                (self.table_page, None, Some(can_next))
            }
            Mode::Instance(_) | Mode::Dashboard(_) => (0, Some(1), Some(false)),
        };
        let can_prev = page > 0;
        let can_next = can_next.unwrap_or_else(|| pages.is_some_and(|pages| page + 1 < pages));
        let can_last = pages.is_some_and(|pages| page + 1 < pages);
        let page_text = SharedString::from((page + 1).to_string());
        h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .child(
                icon_button(
                    "db-page-first",
                    tr(cx, "第一页"),
                    Icon::new(DuckIcon::ChevronsLeft).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(!can_prev)
                .on_click(goto(0)),
            )
            .child(
                icon_button(
                    "db-page-prev",
                    tr(cx, "上一页"),
                    Icon::new(DuckIcon::ChevronLeft).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(!can_prev)
                .on_click(goto(page.saturating_sub(1))),
            )
            // 页码框（dbx 同款：居中窄框，回车即跳；当前页常显在框里是死值
            // —— 输入框是空的，占位才是当前页）。
            .child(
                div().w(px(56.)).flex_none().child(
                    Input::new(&self.page_jump)
                        .appearance(false)
                        .with_size(Size::Small)
                        .text_size(tokens.typography.sm.size)
                        .w_full(),
                ),
            )
            .child(
                icon_button(
                    "db-page-next",
                    tr(cx, "下一页"),
                    Icon::new(DuckIcon::ChevronRight).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(!can_next)
                .on_click(goto(page + 1)),
            )
            .child(
                icon_button(
                    "db-page-last",
                    tr(cx, "最后一页"),
                    Icon::new(DuckIcon::ChevronsRight).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(!can_last)
                .on_click(goto(pages.map_or(page, |pages| pages.saturating_sub(1)))),
            )
            .children(pages.map(|pages| {
                meta_text(
                    SharedString::from(trf(cx, "第 {} / {} 页", &[&page_text, &pages.to_string()])),
                    cx,
                )
            }))
            .into_any_element()
    }

    /// 结果视图三档切换（行 / 列 / JSON），落在结果 footer **最前面**。
    ///
    /// 选中档用主色按钮、其余用 ghost —— 三个都是**动作**（点了切视图），不是标签，
    /// 所以用按钮而不是 tag：tag 不表达可点。
    pub(super) fn result_view_switch(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();
        let current = self.result_view;
        let modes = [
            (ResultViewMode::Grid, "grid", tr(cx, "行模式")),
            (ResultViewMode::Columns, "columns", tr(cx, "列模式")),
            (ResultViewMode::Json, "json", "JSON"),
        ];
        h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .children(modes.into_iter().map(|(mode, slug, label)| {
                let selected = mode == current;
                button(
                    SharedString::from(format!("db-result-view-{slug}")),
                    label,
                    if selected {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Ghost
                    },
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.result_view = mode;
                            cx.notify();
                        });
                    }
                })
                .into_any_element()
            }))
            .into_any_element()
    }

    /// 当前行（列 / JSON 视图看的那一行）：有选中行用选中行，否则用首行。
    ///
    /// 越界一律回落首行——「翻到第 3 页后结果变短」这种情形下，`selected_row`
    /// 可能指向已不存在的行，落到空态比 panic 好。
    pub(super) fn current_row<'a>(&self, result: &'a ResultSet) -> Option<&'a [Value]> {
        let index = self
            .selected_row
            .filter(|index| *index < result.rows.len())
            .unwrap_or(0);
        result.rows.get(index).map(Vec::as_slice)
    }

    /// 当前结果每一列的宽度（缓存命中就用缓存，否则按内容现算）。
    ///
    /// 「字段筛选」与「跳转列」都要按**同一份**宽度算横向偏移，所以口径收在一处：
    /// 两处各写一遍那句 `column_widths.len() == columns.len()` 的判断，漏一处就是
    /// 「跳转到的位置差一列」这种只在特定结果集上才出现的 bug。
    pub(super) fn result_widths(&self, result: &ResultSet) -> Vec<f32> {
        if self.column_widths.len() == result.columns.len() {
            self.column_widths.clone()
        } else {
            compute_column_widths(result)
        }
    }

    /// 切换某一列的显示 / 隐藏（结果区「字段筛选」）。
    pub(super) fn toggle_column_hidden(&mut self, column: &str, cx: &mut Context<Self>) {
        if self.hidden_columns.remove(column) {
            cx.notify();
            return;
        }
        // 全藏光会让网格变成空壳（只剩行首多选列）：至少留一列可见。
        let total = self
            .current_result()
            .map_or(0, |result| result.columns.len());
        if total > 0 && self.hidden_columns.len() + 1 >= total {
            self.status = tr(cx, "至少要保留一列可见。").into();
            cx.notify();
            return;
        }
        self.hidden_columns.insert(column.to_owned());
        cx.notify();
    }

    /// 把结果网格横向滚到某一列（结果区「跳转列」）。
    ///
    /// 横向偏移 = 行首多选列宽 + 它前面各列的宽度之和（`widths` 不含多选列，
    /// 见 [`SELECT_COLUMN_WIDTH`]）。纵向不动：这块区域只允许横向滚动
    /// （`restrict_scroll_to_axis`），纵向那个句柄归行列表。
    pub(super) fn jump_to_column(&mut self, index: usize, cx: &mut Context<Self>) {
        // 借用先结束再写 `self` 的字段：`current_result()` 借的是 `&self`，
        // 与后面的写入不能重叠。
        let (offset, name) = {
            let Some(result) = self.current_result() else {
                return;
            };
            let Some(column) = result.columns.get(index) else {
                return;
            };
            let widths = self.result_widths(result);
            (
                // 网格里数据列之前有两根固定柱：行号 + 多选框（见 `render_result`）。
                ROWNUM_WIDTH + SELECT_COLUMN_WIDTH + widths.iter().take(index).sum::<f32>(),
                column.name.clone(),
            )
        };
        self.result_x_scroll.set_offset(point(px(offset), px(0.)));
        self.status = trf(cx, "已定位到列「{}」。", &[&name]).into();
        cx.notify();
    }

    /// 结果区「字段筛选」下拉：勾选哪些列可见（只影响显示，
    /// 见 [`SessionView::hidden_columns`]）。
    pub(super) fn column_filter_dropdown(&self, cx: &App) -> Option<AnyElement> {
        let theme = active_theme(cx);
        let extended = theme.extended();
        let view = self.view.clone();
        let result = self.current_result()?;
        // 菜单构建闭包是 `Fn`（每次打开重跑）：列名与勾选态先在这边取好，别让闭包
        // 去碰 `self`。
        let items: Vec<(String, bool)> = result
            .columns
            .iter()
            .map(|column| {
                (
                    column.name.clone(),
                    !self.hidden_columns.contains(&column.name),
                )
            })
            .collect();
        let any_hidden = items.iter().any(|(_, shown)| !*shown);
        let show_all = tr(cx, "全部显示");
        Some(toolbar_dropdown_button(
            "db-col-filter",
            tr(cx, "字段筛选"),
            Icon::new(DuckIcon::ListFilter).size(extended.icon.sm),
            Anchor::TopRight,
            move |menu, _, _| {
                let menu = items.iter().fold(menu, |menu, (name, shown)| {
                    let view = view.clone();
                    let name = name.clone();
                    menu.item(
                        PopupMenuItem::new(SharedString::from(name.clone()))
                            .checked(*shown)
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.toggle_column_hidden(&name, cx);
                                });
                            }),
                    )
                });
                // 一列没藏过时，「全部显示」是个点了没反应的项 —— 不给。
                if !any_hidden {
                    return menu;
                }
                let view = view.clone();
                menu.item(PopupMenuItem::separator())
                    .item(PopupMenuItem::new(show_all).on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.hidden_columns.clear();
                            cx.notify();
                        });
                    }))
            },
            cx,
        ))
    }

    /// 结果区「跳转列」下拉：点一列就把网格横向滚到它。
    pub(super) fn column_jump_dropdown(&self, cx: &App) -> Option<AnyElement> {
        let theme = active_theme(cx);
        let extended = theme.extended();
        let view = self.view.clone();
        let result = self.current_result()?;
        // 藏起来的列不当跳转目标：滚过去也在屏幕上找不到它。
        let items: Vec<(usize, String)> = result
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| !self.hidden_columns.contains(&column.name))
            .map(|(index, column)| (index, column.name.clone()))
            .collect();
        Some(toolbar_dropdown_button(
            "db-col-jump",
            tr(cx, "跳转列"),
            Icon::new(DuckIcon::Columns2).size(extended.icon.sm),
            Anchor::TopRight,
            move |menu, _, _| {
                items.iter().fold(menu, |menu, (index, name)| {
                    let view = view.clone();
                    let index = *index;
                    menu.item(
                        PopupMenuItem::new(SharedString::from(name.clone())).on_click(
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.jump_to_column(index, cx));
                            },
                        ),
                    )
                })
            },
            cx,
        ))
    }

    /// 结果内搜索的当前词（trim 过；空串 = 没在搜）。
    pub(super) fn result_search_text(&self, cx: &App) -> String {
        self.result_search.read(cx).value().trim().to_owned()
    }

    /// 清空结果内搜索（切标签 / 换表时调用：旧词对新结果几乎必然失效）。
    pub(super) fn clear_result_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.result_search_text(cx).is_empty() {
            return;
        }
        self.result_search.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        cx.notify();
    }

    /// 结果区 tab 栏右端的「在结果中搜索」框（dbx resSearch 对位）。
    ///
    /// 过滤是**客户端**的（[`matched_row_indexes`] 在渲染路径上现算命中集），
    /// 输入框变字由订阅推一次重渲（`InputEvent::Change → cx.notify()`）。
    /// 有词时在框尾给一枚清除按钮——搜索词藏在小框里，没有一键清除就得全选删除。
    pub(super) fn result_search_box(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let has_query = !self.result_search_text(cx).is_empty();
        h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .child(
                h_flex()
                    .w(px(180.))
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .px(tokens.spacing.xs)
                    .py(px(2.))
                    .rounded(tokens.radius.md)
                    .border_1()
                    .border_color(tokens.colors.border)
                    .bg(tokens.colors.background)
                    .child(
                        Icon::new(DuckIcon::Search)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.result_search)
                                .appearance(false)
                                .with_size(Size::Small)
                                .text_size(tokens.typography.sm.size)
                                .w_full(),
                        ),
                    ),
            )
            .when(has_query, |bar| {
                bar.child(
                    icon_button(
                        "db-result-search-clear",
                        tr(cx, "清除结果搜索"),
                        Icon::new(DuckIcon::X).size(extended.icon.sm),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.clear_result_search(window, cx);
                            });
                        }
                    }),
                )
            })
            .into_any_element()
    }

    /// 结果区顶部的**执行报错条**：`last_error` 常驻到下一次成功执行或用户关闭。
    ///
    /// 这是失败信息**唯一**的常驻落点（编辑器上方不再重复画一条，见 `render_body`
    /// 的说明）；「AI 修错 / 解释 / 关闭」三个动作也从旧编辑器条并到了这里 ——
    /// 看着错误的地方就有出口，不用再回编辑器上方找。
    pub(super) fn render_result_error_banner(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let error = self.last_error.as_ref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = cx.entity().downgrade();
        let message = error.message.clone();
        let sql_line = one_line_sql(&error.sql);
        let sql_tip = SharedString::from(error.sql.clone());
        Some(
            v_flex()
                .w_full()
                .flex_none()
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .gap(tokens.spacing.xxs)
                .bg(tokens.colors.surface)
                .border_b(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap(tokens.spacing.xs)
                        .child(
                            Icon::new(DuckIcon::TriangleAlert)
                                .size(extended.icon.sm)
                                .text_color(extended.colors.status_failed),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .text_color(extended.colors.status_failed)
                                .child(SharedString::from(trf(cx, "执行失败：{}", &[&message]))),
                        ),
                )
                .child(
                    div()
                        .id("db-result-error-sql")
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.muted_foreground)
                        .tooltip(move |window, cx| Tooltip::new(sql_tip.clone()).build(window, cx))
                        .child(SharedString::from(sql_line)),
                )
                // 动作行（旧编辑器报错条的三件套原样搬来）：修错 / 解释注入真实
                // 错误原文；「关闭」只收这条横幅，`last_error` 留给 AI 注入用
                // （下一次成功执行才整体作废）。
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_end()
                        .gap(tokens.spacing.xs)
                        .child(
                            button("db-ai-fix", tr(cx, "AI 修错"), ButtonVariant::Secondary, cx)
                                .disabled(self.busy)
                                .on_click({
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.run_ai_action(AiAction::Fix, window, cx);
                                        });
                                    }
                                }),
                        )
                        .child(
                            button(
                                "db-ai-explain-error",
                                tr(cx, "AI 解释"),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .disabled(self.busy)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.run_ai_action(AiAction::Explain, window, cx);
                                    });
                                }
                            }),
                        )
                        .child(
                            button("db-error-dismiss", tr(cx, "关闭"), ButtonVariant::Ghost, cx)
                                .on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.last_error = None;
                                            this.ai_explain = None;
                                            cx.notify();
                                        });
                                    }
                                }),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 列模式：把当前行转置成「名称 / 值 / 类型」三列（dbx 转置查看对位）。
    ///
    /// 三列宽度可拖（把手贴列头右缘，双击把手回出厂宽度）；值格支持**双击就地编辑**
    /// 与**右键菜单**（复制 / 复制为 ▸ / 单元格详情）—— 编辑走与网格同一条
    /// `pending_edits` 暂存路径，提交口径完全一致。
    pub(super) fn render_result_columns(
        &self,
        result: &ResultSet,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(row) = self.current_row(result) else {
            return empty_state(
                DuckIcon::Columns2,
                tr(cx, "这份结果没有可转置的行"),
                Some(tr(cx, "查询返回了 0 行。").into()),
                None,
                cx,
            )
            .flex_1()
            .into_any_element();
        };
        // 当前转置行的下标（与 [`Self::current_row`] 同一条口径：选中行优先，否则第 0 行）。
        let row_index = self
            .selected_row
            .filter(|index| *index < result.rows.len())
            .unwrap_or(0);
        let widths = self.columns_mode_widths;
        let total_width: f32 = widths.iter().sum();
        // 就地编辑中的格子（行, 列, 输入框）：闭包拿不到 `self`，借用阶段先拷出来。
        let cell_editing = self
            .cell_editor
            .as_ref()
            .map(|editor| (editor.row, editor.col, editor.input.clone()));
        // 闭包跨入 'static，用到的量先拷成 `Copy` 值带进去。
        let hairline = extended.colors.hairline;
        let thin = extended.stroke.thin;
        let hover_tint = extended.colors.hover_tint;
        let surface_bg = tokens.colors.surface;
        let header_h = px(GRID_HEADER_HEIGHT);
        let head_pad = tokens.spacing.xs;
        let head_text = tokens.typography.xs.size;
        let body_text = tokens.typography.sm.size;
        let mono_text = tokens.typography.mono_md.size;
        let fg = tokens.colors.foreground;
        let muted = tokens.colors.muted_foreground;
        let tertiary = extended.colors.text_tertiary;

        // 列头：三格固定宽 + 右缘拖拽把手（把手绝对定位在列头行上，见网格同款注释：
        // 列头格 `overflow_x_hidden` 会裁掉嵌进去的把手）。
        let mut resize_offset = px(0.);
        let resize_handles = (0..3).map(|col| {
            let left = resize_offset + px(widths[col]) - px(COLUMN_RESIZE_HANDLE / 2.);
            resize_offset += px(widths[col]);
            let view = view.clone();
            div()
                .id(SharedString::from(format!("db-colmode-resize-{col}")))
                .absolute()
                .top_0()
                .left(left)
                .w(px(COLUMN_RESIZE_HANDLE))
                .h(header_h)
                .occlude()
                .cursor(CursorStyle::ResizeColumn)
                .on_mouse_down(MouseButton::Left, {
                    let view = view.clone();
                    move |event, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.begin_columns_mode_resize(
                                col,
                                event.position,
                                event.click_count,
                                cx,
                            );
                        });
                    }
                })
                .on_drag(ColResizeDrag { col }, |drag, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| drag.clone())
                })
                .on_drag_move::<ColResizeDrag>({
                    let view = view.clone();
                    move |event, _, cx| {
                        let dragged = event.drag(cx).col;
                        let x = f32::from(event.event.position.x);
                        let _ = view.update(cx, |this, cx| {
                            this.drag_columns_mode_resize(dragged, x, cx);
                        });
                    }
                })
                .on_mouse_up_out(MouseButton::Left, {
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, _| this.end_columns_mode_resize());
                    }
                })
        });
        let head = |text: SharedString, width: f32| {
            div()
                .w(px(width))
                .h(header_h)
                .flex_none()
                .px(head_pad)
                .flex()
                .items_center()
                .overflow_x_hidden()
                .border_r(thin)
                .border_color(hairline)
                .truncate()
                .text_size(head_text)
                .font_weight(FontWeight::MEDIUM)
                .text_color(muted)
                .child(text)
        };
        let header = h_flex()
            .relative()
            .w(px(total_width))
            .flex_none()
            .bg(surface_bg)
            .border_b(thin)
            .border_color(hairline)
            .child(head(tr(cx, "名称").into(), widths[0]))
            .child(head(tr(cx, "值").into(), widths[1]))
            .child(head(tr(cx, "类型").into(), widths[2]))
            // 把手最后追加：绘制顺序靠后 = 优先接到拖拽。
            .children(resize_handles);

        // 「字段筛选」藏起来的列不转置：网格 / 列 / JSON 三个视图看的是同一份结果，
        // 显示口径必须一致（否则「藏了一列，切到列模式它又冒出来」）。
        let mut list = v_flex()
            .id("db-result-columns")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_x_scroll()
            .overflow_y_scroll()
            .child(header);
        for (index, column) in visible_columns(&result.columns, &self.hidden_columns) {
            let cell = row
                .get(index)
                .map(Value::render)
                .unwrap_or_else(|| "NULL".to_owned());
            let null_cell = row.get(index).is_some_and(Value::is_null);
            // 右键菜单 / 双击编辑的闭包都是 'static'：先在外面备好。
            let cell_value = cell.clone();
            let copy_items: Vec<(ResultFormat, &'static str)> = ResultFormat::ALL
                .iter()
                .map(|format| (*format, format.copy_label(cx)))
                .collect();
            let copy_cell_label = tr(cx, "复制单元格");
            let copied_cell_note = tr(cx, "已复制单元格。");
            let submenu_label = tr(cx, "复制全部");
            let detail_label = tr(cx, "单元格详情");
            let editing_input = cell_editing
                .as_ref()
                .and_then(|(edit_row, edit_col, input)| {
                    (*edit_row == row_index && *edit_col == index).then(|| input.clone())
                });
            let value_cell = div()
                .id(SharedString::from(format!("db-colmode-cell-{index}")))
                .w(px(widths[1]))
                .flex_none()
                .px(head_pad)
                .flex()
                .items_center()
                .overflow_x_hidden()
                .border_r(thin)
                .border_color(hairline)
                .hover(move |style| style.bg(hover_tint))
                .child(match editing_input {
                    // 就地编辑中：正文换成输入框（回车 / 失焦提交，
                    // 订阅挂在 `install_cell_editor` 里）。
                    Some(input) => div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Input::new(&input)
                                .appearance(false)
                                .with_size(Size::Small)
                                .flex_1()
                                .min_w_0(),
                        )
                        .into_any_element(),
                    None => div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(mono_text)
                        .text_color(if null_cell { tertiary } else { fg })
                        .child(SharedString::from(cell_value.clone()))
                        .into_any_element(),
                })
                // 单击选中行；**双击**就地编辑（dbx 同款，与网格同一条提交路径）。
                .on_click({
                    let view = view.clone();
                    move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                        cx.stop_propagation();
                        let Some(entity) = view.upgrade() else {
                            return;
                        };
                        let double = match event {
                            ClickEvent::Mouse(mouse) => mouse.up.click_count > 1,
                            ClickEvent::Touch(tap) => tap.tap_count > 1,
                            ClickEvent::Keyboard(_) => false,
                        };
                        if !double {
                            return;
                        }
                        let Some((original, was_null)) =
                            entity.read(cx).cell_editor_prep(row_index, index)
                        else {
                            return;
                        };
                        // 结果集不可编辑就明说，不摆空编辑框。
                        if entity.read(cx).edit_source().is_none() {
                            notify::info(
                                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                                window,
                                cx,
                            );
                            return;
                        }
                        let input = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
                        if !was_null {
                            let seed = original.clone();
                            input.update(cx, |state, cx| {
                                state.set_value(seed, window, cx);
                            });
                        }
                        // dbx / dbflux 同款：进编辑即聚焦 + **全选**。
                        input.update(cx, |state, cx| {
                            state.focus(window, cx);
                            state.select_all(window, cx);
                        });
                        let _ = entity.update(cx, |this, cx| {
                            this.selected_row = Some(row_index);
                            this.cell_detail = Some((row_index, index));
                            this.install_cell_editor(
                                row_index, index, original, was_null, input, cx,
                            );
                            cx.notify();
                        });
                    }
                })
                // 右键菜单：复制该值 / 复制为 ▸（整份结果）/ 单元格详情。
                // 一层菜单（组件库 ContextMenu 不截停冒泡，别再叠外层）。
                .context_menu({
                    let view = view.clone();
                    let cell_text = cell.clone();
                    let copy_items = copy_items.clone();
                    move |menu, window, cx| {
                        let copy_view = view.clone();
                        let copy_menu = PopupMenu::build(window, cx, |menu, _, _| {
                            copy_items.iter().fold(menu, |menu, (format, label)| {
                                menu.item(PopupMenuItem::new(*label).on_click({
                                    let view = copy_view.clone();
                                    let format = *format;
                                    move |_, _, cx| {
                                        let Some(entity) = view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.copy_result_as(format, cx);
                                        });
                                    }
                                }))
                            })
                        });
                        menu.item(PopupMenuItem::new(copy_cell_label).on_click({
                            let view = view.clone();
                            let text = cell_text.clone();
                            move |_, _, cx| {
                                let Some(entity) = view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.put_on_clipboard(text.clone(), copied_cell_note, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::submenu(submenu_label, copy_menu))
                        .item(PopupMenuItem::new(detail_label).on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let Some(entity) = view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.open_cell_detail(row_index, index, cx);
                                });
                            }
                        }))
                    }
                });
            list = list.child(
                h_flex()
                    .w(px(total_width))
                    .flex_none()
                    .items_center()
                    .py(tokens.spacing.xxs)
                    .border_b(thin)
                    .border_color(hairline)
                    .hover(move |style| style.bg(hover_tint))
                    // 名称 / 类型是只读描述列；高度随行（flex 默认 stretch）。
                    .child(
                        div()
                            .w(px(widths[0]))
                            .flex_none()
                            .px(head_pad)
                            .truncate()
                            .text_size(body_text)
                            .text_color(fg)
                            .child(SharedString::from(column.name.clone())),
                    )
                    .child(value_cell)
                    .child(
                        div()
                            .w(px(widths[2]))
                            .flex_none()
                            .px(head_pad)
                            .truncate()
                            .text_size(body_text)
                            .text_color(tertiary)
                            .child(SharedString::from(column.type_name.clone())),
                    ),
            );
        }
        list.into_any_element()
    }

    /// JSON 模式：把当前行序列化成对象预览。
    ///
    /// 复用内核的 [`ResultSet::to_json_pretty`]（与导出 JSON 同一套序列化口径），
    /// 只把行裁成一行——不另写一份「值 → JSON」的手工拼接，避免两处口径漂移。
    pub(super) fn render_result_json(&self, result: &ResultSet, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let Some(row) = self.current_row(result) else {
            return empty_state(
                DuckIcon::Code,
                tr(cx, "这份结果没有可预览的行"),
                Some(tr(cx, "查询返回了 0 行。").into()),
                None,
                cx,
            )
            .flex_1()
            .into_any_element();
        };
        // 与网格 / 列模式同一口径：藏起来的列不进 JSON。按**原始**下标同时裁列与裁值，
        // 否则列名与值会错位半格（JSON 是「名字对值」的映射，错位比少一列严重得多）。
        let mut columns = Vec::new();
        let mut values = Vec::new();
        for (index, column) in result.columns.iter().enumerate() {
            if self.hidden_columns.contains(&column.name) {
                continue;
            }
            columns.push(column.clone());
            if let Some(value) = row.get(index) {
                values.push(value.clone());
            }
        }
        let single = ResultSet {
            columns,
            rows: vec![values],
            truncated: false,
        };
        div()
            .id("db-result-json")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .p(tokens.spacing.sm)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .text_color(tokens.colors.foreground)
                    .child(SharedString::from(single.to_json_pretty())),
            )
            .into_any_element()
    }

    /// 导出结果下拉（五档：TSV / CSV / Markdown / JSON / INSERT）。
    ///
    /// 档位清单与结果区右键的「复制为 …」同源（[`ResultFormat::ALL`]），这里只是
    /// 多一层落盘文件名与保存对话框。「导出 INSERT」只对 SQL 引擎开放：MongoDB /
    /// Elasticsearch 没有带引号的表名，导不出能直接执行的语句。
    pub(super) fn export_dropdown(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let extended = theme.extended();
        let view = self.view.clone();
        let has_result = self.current_result().is_some();
        let can_insert = self.insert_context().is_some();
        let has_raw = self
            .current_result()
            .is_some_and(|result| result.raw_bytes().is_some());
        // 菜单构建闭包是 `Fn` 且 `'static`：文案必须在外面按当前语言取好（`tr`
        // 要 `&App`，闭包里拿不到），所以这里先结成「档位 + 文案」对——
        // 一份清单派生一对，不会像手写文案数组那样加档错位。
        let export_items: Vec<(ResultFormat, &'static str)> = ResultFormat::EXPORT_ALL
            .iter()
            .map(|format| (*format, format.export_label(cx)))
            .collect();
        // 文字按钮（设计稿「⤓ 导出 ∨」）：纯图标版让人猜这个下载箭头是干嘛的。
        toolbar_text_dropdown_button(
            "db-export",
            tr(cx, "导出"),
            Icon::new(DuckIcon::Download).size(extended.icon.sm),
            Anchor::TopRight,
            move |menu, _, _| {
                export_items.iter().fold(menu, |menu, (format, label)| {
                    let enabled = format.exportable(has_result, can_insert, has_raw);
                    menu.item(PopupMenuItem::new(*label).disabled(!enabled).on_click({
                        let view = view.clone();
                        let format = *format;
                        move |_, window, cx| {
                            let _ =
                                view.update(cx, |this, cx| this.export_result(format, window, cx));
                        }
                    }))
                })
            },
            cx,
        )
    }

    /// 结果区顶部 tab 栏 + 常驻动作组。
    ///
    /// 行首是**对象名**（原对象头条迁来：正在看哪张表 / 哪条查询），随后五档 tab
    /// 换掉网格本体（摘要 / 图表 / 消息 / 执行计划）、「在结果中搜索」，右侧动作组把
    /// 原「编辑条」的写操作提到常驻位置：刷新 / 新增行 / 删除行 / 提交 / 回滚 +
    /// 「保存结果」导出。dbx 的语义是「这一列动作常在」，不可用**置灰**而不是消失
    /// —— 按钮忽隐忽现会让工具条自身的位置也跟着跳。
    pub(super) fn render_result_tabbar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let has_result = self.current_result().is_some();
        let pending = self.pending_total();

        // tab 格：与读数条的结果标签同一版式（圆角、选中 nav_selected、悬停 hover_tint）。
        let tab = |id: &'static str,
                   label: &'static str,
                   icon: DuckIcon,
                   area: ResultAreaTab,
                   enabled: bool| {
            let active = self.result_area_tab == area;
            let tab_view = view.clone();
            let mut el = div()
                .id(SharedString::from(id))
                .h(theme.density().control)
                .pl(tokens.spacing.sm)
                .pr(tokens.spacing.xs)
                .flex()
                .items_center()
                .gap(tokens.spacing.xxs)
                .rounded(tokens.radius.md)
                .text_size(tokens.typography.xs.size)
                .font_weight(if active {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if active {
                    tokens.colors.foreground
                } else {
                    tokens.colors.muted_foreground
                })
                .when(active, |el| el.bg(extended.colors.nav_selected))
                .child(Icon::new(icon).size(extended.icon.sm))
                .child(label);
            if enabled {
                el = el
                    .cursor_pointer()
                    .role(Role::Button)
                    .aria_label(label)
                    .hover(move |style| style.bg(extended.colors.hover_tint))
                    .on_click(move |_, _, cx| {
                        let _ = tab_view.update(cx, |this, cx| this.set_result_area_tab(area, cx));
                    });
            } else {
                // 不可用的档位置灰但**留在原位**：工具条布局不跳，用户也看得到还有哪些视图。
                el = el.opacity(extended.opacity.disabled);
            }
            el.into_any_element()
        };

        // 动作钮：纯图标 + 悬停提示（dbx 工具条同款）。提示由组件自己挂（`icon_button`
        // 用 label 当提示、`icon_button_tip` 另给一句），这里**不再挂第二次** ——
        // gpui 对同一元素重复 `.tooltip()` 会 panic 秒退。
        let act = |id: &'static str, label: &'static str, icon: DuckIcon| {
            icon_button(
                id,
                label,
                Icon::new(icon).size(extended.icon.md),
                ButtonVariant::Ghost,
                cx,
            )
        };
        // 提示要比按钮名多说一句时走这个（如「提交」的暂存条数、行数随状态变的读数）。
        let act_tip = |id: &'static str, label: &'static str, icon: DuckIcon, tip: SharedString| {
            icon_button_tip(
                id,
                label,
                Icon::new(icon).size(extended.icon.md),
                tip,
                ButtonVariant::Ghost,
                cx,
            )
        };

        // 刷新：表浏览 = 重取当前页；查询 = 重新执行来源语句（dbx 刷新同款语义）。
        let refresh = if self.mode == Mode::Table {
            act("db-area-refresh", tr(cx, "刷新当前页"), DuckIcon::RotateCw).on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.refresh_table_page(cx));
                }
            })
        } else {
            act(
                "db-area-refresh",
                tr(cx, "重新执行这条语句"),
                DuckIcon::RotateCw,
            )
            .disabled(self.busy || self.query_sql.is_none())
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        let Some(sql) = this.query_sql.clone() else {
                            return;
                        };
                        this.execute_sql(sql, None, window, cx);
                    });
                }
            })
        };

        // 删除行：勾选了行就批量删（单事务），否则删当前选中行（与多选条同源）。
        let deletable = self.edit_source().is_some()
            && !self.busy
            && (self.selected_row.is_some() || !self.checked_rows.is_empty());
        let mut delete_btn = act(
            "db-area-delete-row",
            if self.checked_rows.is_empty() {
                tr(cx, "删除当前行")
            } else {
                tr(cx, "删除选中行")
            },
            DuckIcon::Trash2,
        )
        .disabled(!deletable);
        if deletable {
            delete_btn = delete_btn.on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        if !this.checked_rows.is_empty() {
                            this.apply_batch_delete(window, cx);
                        } else if let Some(row) = this.selected_row {
                            this.delete_current_row(row, window, cx);
                        }
                    });
                }
            });
        }

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xxs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(tab(
                "db-area-tab-data",
                tr(cx, "数据表"),
                DuckIcon::Table,
                ResultAreaTab::Data,
                true,
            ))
            .child(tab(
                "db-area-tab-summary",
                tr(cx, "摘要"),
                DuckIcon::ListOrdered,
                ResultAreaTab::Summary,
                has_result,
            ))
            .child(tab(
                "db-area-tab-chart",
                tr(cx, "图表"),
                DuckIcon::ChartLine,
                ResultAreaTab::Chart,
                has_result,
            ))
            .child(tab(
                "db-area-tab-messages",
                tr(cx, "消息"),
                DuckIcon::MessageSquare,
                ResultAreaTab::Messages,
                true,
            ))
            .child(tab(
                "db-area-tab-plan",
                tr(cx, "执行计划"),
                DuckIcon::Network,
                ResultAreaTab::Plan,
                true,
            ))
            .child(div().flex_1().min_w_0())
            // 结果内搜索（自读数条上移）：找行是「看着结果」时的动作，跟档位与写
            // 动作同一排；读数条只留事实读数。词不进快照，仍是临时态。
            .child(self.result_search_box(cx))
            .child(refresh)
            .child(self.export_dropdown(cx))
            .child(
                act("db-area-insert-row", tr(cx, "新增行"), DuckIcon::Plus)
                    .disabled(
                        self.busy || self.row_editor.is_some() || self.edit_source().is_none(),
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.open_row_editor_for_insert(window, cx);
                            });
                        }
                    }),
            )
            .child(delete_btn)
            .child(
                act_tip(
                    "db-area-commit",
                    tr(cx, "提交"),
                    DuckIcon::Check,
                    if pending > 0 {
                        SharedString::from(trf(cx, "提交 {} 条暂存改动", &[&pending.to_string()]))
                    } else {
                        SharedString::from(tr(cx, "没有待提交的改动"))
                    },
                )
                .disabled(self.busy || pending == 0)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.commit_pending(window, cx));
                    }
                }),
            )
            .child(
                act_tip(
                    "db-area-rollback",
                    tr(cx, "回滚"),
                    DuckIcon::CornerLeftUp,
                    SharedString::from(tr(cx, "丢弃全部暂存改动")),
                )
                .disabled(self.busy || pending == 0)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.discard_pending(window, cx));
                    }
                }),
            )
            // 字段筛选 / 跳转列（自结果标签行迁来）：都是「看着结果」时用的网格
            // 工具，跟刷新 / 删除排在同一条；悬停提示由 `toolbar_dropdown_button`
            // 内部挂（与无障碍名同一份文案）。
            .children(self.column_filter_dropdown(cx))
            .children(self.column_jump_dropdown(cx))
            .into_any_element()
    }

    /// 结果 tab 面板的公共骨架：结果标签行 + 档位行 + 报错条（若有）+ 内容
    /// （+ 可选脚注）。
    ///
    /// 顺序与网格路径一致：**结果标签行在最上**（「现在看的是哪条语句的结果」是这块
    /// 区域的标题），档位行（数据表 / 摘要 / 图表 / 消息 / 执行计划 + 动作组）跟在
    /// 其后。摘要 / 消息 / 图表三档也照摆标签行 —— 不然切进摘要就看不见（也切不了）
    /// 结果标签，多结果批次会变成「进去容易出来难」。
    pub(super) fn result_tab_panel(
        &self,
        content: AnyElement,
        with_footer: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut panel = v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(self.render_result_resbar(cx))
            .child(self.render_result_tabbar(cx))
            .children(self.render_result_error_banner(cx))
            .child(div().flex_1().min_h_0().min_w_0().child(content));
        if with_footer {
            panel = panel.child(self.render_result_footer(cx));
        }
        panel.into_any_element()
    }

    /// 拖拽后的列宽写回缓存（按原始列下标；缓存没对齐当前结果集时先按内容补齐）。
    pub(super) fn set_column_width(&mut self, col: usize, width: f32, cx: &mut Context<Self>) {
        if self.column_widths.len() <= col {
            self.column_widths = self
                .current_result()
                .map(compute_column_widths)
                .unwrap_or_default();
        }
        if let Some(slot) = self.column_widths.get_mut(col)
            && (*slot - width).abs() > f32::EPSILON
        {
            *slot = width;
            cx.notify();
        }
    }

    /// 双击列宽把手：按内容自适应该列（与初始估宽同一套采样口径）。
    pub(super) fn auto_fit_column(&mut self, col: usize, cx: &mut Context<Self>) {
        let Some(result) = self.current_result() else {
            return;
        };
        if let Some(width) = compute_column_widths(result).get(col).copied() {
            self.set_column_width(col, width, cx);
        }
    }

    /// 列宽把手按下：记下起点（双击则改走「自适应」，不进入拖动态）。
    pub(super) fn begin_col_resize(
        &mut self,
        col: usize,
        position: gpui::Point<gpui::Pixels>,
        click_count: usize,
        cx: &mut Context<Self>,
    ) {
        if click_count >= 2 {
            self.col_resize = None;
            self.auto_fit_column(col, cx);
            return;
        }
        let start_width = self
            .column_widths
            .get(col)
            .copied()
            .unwrap_or(COLUMN_MIN_WIDTH);
        self.col_resize = Some((col, f32::from(position.x), start_width));
    }

    /// 拖动中：按位移改列宽（夹在上下限之间）。
    pub(super) fn drag_col_resize(&mut self, col: usize, x: f32, cx: &mut Context<Self>) {
        let Some((active, start_x, start_width)) = self.col_resize else {
            return;
        };
        if active != col {
            return;
        }
        let width = (start_width + (x - start_x)).clamp(COLUMN_MIN_WIDTH, COLUMN_RESIZE_MAX);
        self.set_column_width(col, width, cx);
    }

    /// 松手 / 拖出：结束拖动态（列宽已经逐帧写进缓存，这里只清标记）。
    pub(super) fn end_col_resize(&mut self) {
        self.col_resize = None;
    }

    /// 列模式：开始拖第 `col` 列（0 = 名称，1 = 值，2 = 类型）。
    pub(super) fn begin_columns_mode_resize(
        &mut self,
        col: usize,
        position: gpui::Point<gpui::Pixels>,
        click_count: usize,
        cx: &mut Context<Self>,
    ) {
        if click_count >= 2 {
            self.columns_mode_resize = None;
            self.columns_mode_widths[col] = COLUMNS_MODE_DEFAULT_WIDTHS[col];
            cx.notify();
            return;
        }
        self.columns_mode_resize =
            Some((col, f32::from(position.x), self.columns_mode_widths[col]));
    }

    /// 列模式：拖动中按位移改列宽。
    pub(super) fn drag_columns_mode_resize(&mut self, col: usize, x: f32, cx: &mut Context<Self>) {
        let Some((active, start_x, start_width)) = self.columns_mode_resize else {
            return;
        };
        if active != col {
            return;
        }
        self.columns_mode_widths[col] =
            (start_width + (x - start_x)).clamp(COLUMN_MIN_WIDTH, COLUMN_RESIZE_MAX);
        cx.notify();
    }

    /// 列模式：拖拽收尾。
    pub(super) fn end_columns_mode_resize(&mut self) {
        self.columns_mode_resize = None;
    }

    /// 摘要 tab 正文：**执行过程**（状态 / 耗时 / 规模 / 来源语句）+ **列统计**清单。
    ///
    /// 「执行过程」回答「这次执行发生了什么」（dbx 摘要同款语义）：状态恒为「成功」
    /// —— 失败不产出结果，走结果区顶部的报错条与「消息」流水。逐列的空值计数只对
    /// **当前渲染到的列**现算（`uniform_list` 按可视区物化）：全量 10k 行 × 宽列的
    /// 预计算放渲染路径上不值当。
    pub(super) fn render_summary_body(&self, result: &ResultSet, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let rows_n = result.rows.len();
        let columns = result.columns.clone();
        let elapsed = self
            .query_elapsed_ms
            .map(|ms| SharedString::from(trf(cx, "{} ms", &[&ms.to_string()])))
            .unwrap_or_else(|| tr(cx, "—").into());

        let stat = |label: &'static str, value: SharedString| {
            h_flex()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.xxs)
                .child(meta_text(label, cx))
                .child(
                    div()
                        .text_size(tokens.typography.sm.size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.colors.foreground)
                        .child(value),
                )
        };
        let stats = h_flex()
            .w_full()
            .flex_none()
            .flex_wrap()
            .items_center()
            .gap(tokens.spacing.lg)
            .px(tokens.spacing.sm)
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(meta_text(tr(cx, "状态"), cx))
                    .child(
                        Icon::new(DuckIcon::CircleCheck)
                            .size(extended.icon.sm)
                            .text_color(extended.colors.success),
                    )
                    .child(
                        div()
                            .text_size(tokens.typography.sm.size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(tokens.colors.foreground)
                            .child(tr(cx, "成功")),
                    ),
            )
            .child(stat("耗时", elapsed))
            .child(stat("行数", SharedString::from(rows_n.to_string())))
            .child(stat("列数", SharedString::from(columns.len().to_string())))
            .when(result.truncated, |row| {
                row.child(neutral_tag(tr(cx, "已达上限"), cx))
            });

        // 逐列清单表头。
        let head = h_flex()
            .flex_none()
            .h(px(GRID_ROW_HEIGHT))
            .px(tokens.spacing.sm)
            .items_center()
            .gap(tokens.spacing.lg)
            .bg(extended.colors.chrome)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .text_size(tokens.typography.xs.size)
            .font_weight(FontWeight::MEDIUM)
            .text_color(tokens.colors.muted_foreground)
            .child(div().flex_1().min_w_0().child(tr(cx, "列名")))
            .child(
                div()
                    .w(px(150.))
                    .flex_none()
                    .truncate()
                    .child(tr(cx, "类型")),
            )
            .child(div().w(px(90.)).flex_none().child(tr(cx, "空值")));
        let list_columns = columns.clone();
        // 空值计数一次性预算：只做「是不是 NULL」的枚举判断（1 万行 × 20 列 ≈ 20 万次
        // 分支，微秒级），别放渲染闭包里逐帧算。
        let null_counts: Vec<usize> = list_columns
            .iter()
            .enumerate()
            .map(|(index, _)| {
                result
                    .rows
                    .iter()
                    .filter(|row| row.get(index).is_some_and(|value| value.is_null()))
                    .count()
            })
            .collect();
        // 网格闭包跨入 'static，拿不到 `tokens` / `extended`，用到的量先拷出来。
        let row_pad = tokens.spacing.sm;
        let col_gap = tokens.spacing.lg;
        let cell_text = tokens.typography.sm.size;
        let fg = tokens.colors.foreground;
        let muted = tokens.colors.muted_foreground;
        let hairline = extended.colors.hairline;
        let thin = extended.stroke.thin;
        let list = uniform_list("db-summary-cols", list_columns.len(), move |range, _, _| {
            range
                .map(|index| {
                    let column = &list_columns[index];
                    let nulls = null_counts[index];
                    h_flex()
                        .id(SharedString::from(format!("db-summary-col-{index}")))
                        .h(px(GRID_ROW_HEIGHT))
                        .px(row_pad)
                        .items_center()
                        .gap(col_gap)
                        .border_b(thin)
                        .border_color(hairline)
                        .text_size(cell_text)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(fg)
                                .child(SharedString::from(column.name.clone())),
                        )
                        .child(
                            div()
                                .w(px(150.))
                                .flex_none()
                                .truncate()
                                .text_color(muted)
                                .child(SharedString::from(column.type_name.clone())),
                        )
                        .child(
                            div()
                                .w(px(90.))
                                .flex_none()
                                .text_color(muted)
                                .child(SharedString::from(nulls.to_string())),
                        )
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .flex_1();
        v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            // —— 执行过程 ——（状态 / 耗时 / 规模 + 来源语句；语句过长截断，
            // 完整原文在悬停浮层与右栏详情里。）
            .child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .pb(tokens.spacing.sm)
                    .border_b(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .child(
                        caption_heading(tr(cx, "执行过程"), cx)
                            .px(tokens.spacing.sm)
                            .pt(tokens.spacing.sm)
                            .pb(tokens.spacing.xs),
                    )
                    .child(stats)
                    .when_some(self.query_sql.clone(), |block, _| {
                        block.child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .items_center()
                                .gap(tokens.spacing.sm)
                                .px(tokens.spacing.sm)
                                .pt(tokens.spacing.xs)
                                .child(meta_text(tr(cx, "语句"), cx))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .items_center()
                                        .children(self.result_sql_chip(cx)),
                                ),
                        )
                    }),
            )
            // —— 列统计 ——
            .child(
                caption_heading(tr(cx, "列统计"), cx)
                    .px(tokens.spacing.sm)
                    .pt(tokens.spacing.sm)
                    .pb(tokens.spacing.xs),
            )
            .child(head)
            .child(list)
            .into_any_element()
    }

    /// 消息 tab 正文：执行流水（新在前；空态给指引而不是一片空白）。
    pub(super) fn render_messages_body(&self, cx: &Context<Self>) -> AnyElement {
        if self.exec_log.is_empty() {
            return empty_state(
                DuckIcon::MessageSquare,
                tr(cx, "暂无执行消息"),
                Some(tr(cx, "执行 SQL 后这里会记录每次执行的读数与错误。").into()),
                None,
                cx,
            )
            .flex_1()
            .into_any_element();
        }
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let entries: Vec<ExecLogEntry> = self.exec_log.iter().rev().cloned().collect();
        // 闭包跨入 'static，用到的量先拷成 `Copy` 值带进去。
        let row_pad = tokens.spacing.sm;
        let row_gap = tokens.spacing.sm;
        let cell_text = tokens.typography.sm.size;
        let fg = tokens.colors.foreground;
        let muted = tokens.colors.muted_foreground;
        let hairline = extended.colors.hairline;
        let thin = extended.stroke.thin;
        let icon_sm = extended.icon.sm;
        let ok_color = extended.colors.success;
        let fail_color = extended.colors.status_failed;
        uniform_list("db-exec-log", entries.len(), move |range, _, _| {
            range
                .map(|index| {
                    let entry = &entries[index];
                    let (icon, color) = if entry.ok {
                        (DuckIcon::CircleCheck, ok_color)
                    } else {
                        (DuckIcon::TriangleAlert, fail_color)
                    };
                    h_flex()
                        .id(SharedString::from(format!("db-exec-log-{index}")))
                        .h(px(GRID_ROW_HEIGHT))
                        .px(row_pad)
                        .items_center()
                        .gap(row_gap)
                        .border_b(thin)
                        .border_color(hairline)
                        .text_size(cell_text)
                        .child(Icon::new(icon).size(icon_sm).text_color(color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(fg)
                                .child(SharedString::from(entry.label.clone())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(360.))
                                .truncate()
                                .text_color(muted)
                                .child(SharedString::from(entry.detail.clone())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(muted)
                                .child(SharedString::from(format!("{} ms", entry.elapsed_ms))),
                        )
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .into_any_element()
    }

    pub(super) fn render_result(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let hover = extended.colors.hover_tint;
        let selected_bg = extended.colors.nav_selected;
        let tertiary = extended.colors.text_tertiary;

        if self.mode == Mode::Table && self.selected_table.is_none() {
            return empty_state(
                DuckIcon::Columns2,
                tr(cx, "从右侧对象树选择一张表"),
                Some(tr(cx, "表浏览以分页方式读取数据，每页 200 行。").into()),
                None,
                cx,
            )
            .flex_1()
            .into_any_element();
        }
        let Some(result) = self.current_result() else {
            // 执行失败且没有可展示的结果：报错条必须出现在结果区，不能只靠状态栏
            // 一句话（会被下一次操作顶掉）——「结果区空着」最像没执行过。tab 栏
            // 照常给，「消息」里还有完整流水可看。
            if self.last_error.is_some() {
                let body = v_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .children(self.render_result_error_banner(cx))
                    .child(
                        empty_state(
                            DuckIcon::CircleAlert,
                            tr(cx, "没有可展示的结果"),
                            Some(tr(cx, "上一次执行失败了；修正 SQL 后重新执行。").into()),
                            None,
                            cx,
                        )
                        .flex_1(),
                    );
                return self.result_tab_panel(body.into_any_element(), false, cx);
            }
            return empty_state(
                DuckIcon::Database,
                tr(cx, "执行 SQL 后在这里查看结果"),
                Some(
                    tr(
                        cx,
                        "支持多条语句；读语句结果按 10000 行一页展示，可逐页加载更多。",
                    )
                    .into(),
                ),
                None,
                cx,
            )
            .flex_1()
            .into_any_element();
        };

        // 执行计划态：解析成树时结果区换成缩进树；切到「原始输出」档则照旧显示网格，
        // 头条留在最上面（随时切回树）。
        let mut plan_bar = self.plan_view.as_ref().map(|_| self.render_plan_bar(cx));

        // 结果区 tab 分发（dbx `QueryResultViewSwitcher` 同款）：摘要 / 消息 / 图表 /
        // 计划树各自换掉网格本体；「数据表」与「执行计划 · 原始输出」共用下面的网格
        // 流程 —— 原样输出本来就在网格里，plan_bar 照旧挂在头条下（随时切回树）。
        let tabbed = match self.result_area_tab {
            ResultAreaTab::Plan if self.plan_view.as_ref().is_some_and(|plan| !plan.raw) => {
                let mut panel = v_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.render_result_resbar(cx))
                    .child(self.render_result_tabbar(cx));
                if let Some(bar) = plan_bar.take() {
                    panel = panel.child(bar);
                }
                let nodes = self
                    .plan_view
                    .as_ref()
                    .map(|plan| plan.nodes.clone())
                    .unwrap_or_default();
                Some(panel.child(plan_tree(nodes, cx)).into_any_element())
            }
            ResultAreaTab::Summary => {
                Some(self.result_tab_panel(self.render_summary_body(result, cx), true, cx))
            }
            ResultAreaTab::Messages => {
                Some(self.result_tab_panel(self.render_messages_body(cx), false, cx))
            }
            ResultAreaTab::Chart => {
                let body = if self.chart_view.is_some() {
                    self.render_chart(result, cx)
                } else {
                    empty_state(
                        DuckIcon::ChartLine,
                        tr(cx, "这份结果还没有图表"),
                        Some(tr(cx, "没有可画的数值列；切回「数据表」看原始结果。").into()),
                        None,
                        cx,
                    )
                    .flex_1()
                    .into_any_element()
                };
                Some(self.result_tab_panel(body, self.chart_view.is_some(), cx))
            }
            ResultAreaTab::Data | ResultAreaTab::Plan => None,
        };
        if let Some(panel) = tabbed {
            return panel;
        }

        // 结果视图三档（footer 最前面的切换）：计划态 / 图表态优先，其余落到这里。
        //
        // 列模式 / JSON 只**替换网格内容**，tab 栏与脚注照旧 —— 尤其脚注里那三个
        // 切换按钮：早返回掉了就等于「切进去再也切不回来」。翻页 / 加载更多也保留，
        // 否则表浏览切到列模式后就没法再翻页了。
        if self.result_view != ResultViewMode::Grid {
            let content = match self.result_view {
                ResultViewMode::Columns => self.render_result_columns(result, cx),
                ResultViewMode::Json => self.render_result_json(result, cx),
                // 开发期约束，不是用户可见文案：`Grid` 档在上面就被筛掉了，
                // 所以 panic 消息写英文（`every_chinese_literal_is_translated`
                // 守卫要求界面里的中文字面量必须进 EN 表，开发者消息不占一个键）。
                ResultViewMode::Grid => unreachable!("Grid was filtered out above"),
            };
            let mut panel = v_flex().flex_1().min_h_0().min_w_0();
            panel = panel.child(self.render_result_resbar(cx));
            panel = panel.child(self.render_result_tabbar(cx));
            if let Some(bar) = plan_bar {
                panel = panel.child(bar);
            }
            panel = panel.child(div().flex_1().min_h_0().min_w_0().child(content));
            if self.mode == Mode::Query && self.query_has_more {
                panel = panel.child(self.render_load_more_bar(cx));
            } else if result.truncated {
                panel = panel.child(
                    div()
                        .flex_none()
                        .px(tokens.spacing.sm)
                        .py(tokens.spacing.xxs)
                        .child(meta_text(
                            tr(
                                cx,
                                "结果已达行数上限，仅展示前一部分；可加 LIMIT 或导出 CSV。",
                            ),
                            cx,
                        )),
                );
            }
            panel = panel.child(self.render_result_footer(cx));
            return panel.into_any_element();
        }

        let columns = result.columns.clone();
        let rows = result.rows.clone();
        let widths = self.result_widths(result);
        // 「字段筛选」藏起来的列不进列头与行渲染。**只有渲染这一步过滤**：`widths`
        // 与行数据仍按原始列序对齐，所以单元格取宽要用原始下标。
        let visible = visible_columns(&columns, &self.hidden_columns);
        // 结果内搜索：**客户端**过滤，只影响网格显示（多选 / 行编辑 / 导出仍按完整
        // 行集，与「字段筛选」同一条口径）。命中集存**原始行下标**，网格按它取行。
        let search_text = self.result_search_text(cx);
        let matched = matched_row_indexes(&rows, &visible, &search_text);
        // 客户端分页（查询模式，设计稿 grid-foot 的翻页组）：结果已整批加载，
        // 按「每页行数」切片展示；表浏览本身是服务器侧分页，整页即一页。
        // 切片存原始行下标——选中 / 勾选仍按原始下标，翻译只此一次。
        let (page_rows, page_start) = if self.mode == Mode::Query {
            let page_size = self.query_page_size(cx);
            let start = (self.query_page * page_size).min(matched.len());
            let end = (start + page_size).min(matched.len());
            (matched[start..end].to_vec(), start)
        } else {
            (matched.clone(), 0)
        };
        let matched_count = page_rows.len();
        let no_match = !search_text.is_empty() && matched.is_empty();
        let truncated = result.truncated;
        let view = self.view.clone();
        let selected_row = self.selected_row;
        let row_count = rows.len();
        // 多选态：表头三态由「勾了几行」推出来，行内是二态。
        let checked_rows = self.checked_rows.clone();
        let checked_count = checked_rows.len();
        let all_checked = row_count > 0 && checked_count == row_count;
        let header_state = if checked_count == 0 {
            CheckState::Unchecked
        } else if all_checked {
            CheckState::Checked
        } else {
            CheckState::Indeterminate
        };
        // 网格闭包拿不到 `tokens`（跨入 'static），把用到的量提前拷出来。
        let cell_pad = tokens.spacing.xs;
        let cell_text = tokens.typography.sm.size;
        let rownum_bg = extended.colors.chrome;
        // 框线（dbx 同款：列间竖线 + 行间横线）与图标尺寸先拷成 `Copy` 值，
        // uniform_list 的 `'static` 闭包拿不到 `extended`。
        let hairline = extended.colors.hairline;
        let thin = extended.stroke.thin;
        let icon_sm = extended.icon.sm;
        // 悬停 ⓘ / 就地编辑 ✓ 还要用：底色、圆角、间距、强调与成功色。
        let surface_bg = tokens.colors.surface;
        let radius_sm = tokens.radius.sm;
        let cell_gap = tokens.spacing.xxs;
        let hover_tint = extended.colors.hover_tint;
        let success = extended.colors.success;
        // 就地编辑态（dbx / dbflux 同款）：2px 强调描边 + 焦点环色、编辑行的
        // 行号用「文本选区底色 + 强调色文字」点亮。先拷成 `Copy` 值带进
        // `uniform_list` 的 `'static` 闭包（同上面 thin / hover_tint 的原因）。
        let edit_strong = extended.stroke.strong;
        let edit_ring = tokens.colors.ring;
        let selection_bg = tokens.colors.selection;
        let accent_text = extended.colors.accent_text;
        // 就地编辑中的格子（行, 列, 输入框）：闭包拿不到 `self`，借用阶段先拷出来。
        let cell_editing = self
            .cell_editor
            .as_ref()
            .map(|editor| (editor.row, editor.col, editor.input.clone()));

        // 列宽把手：贴着每列右缘的一条竖带（dbx / gpui-component Table 同款）。
        //
        // 绝对定位挂在**列头行**上，而不是塞进列头格：列头格自己有
        // `overflow_x_hidden`（负责列名截断），嵌进去的把手会被它裁掉一半而够不着。
        // 这组把手在 `.children()` 里**最后**追加 —— 绘制顺序靠后即优先接到拖拽，
        // 否则列头格自己的「点一下排序」会把按下的那一下吃掉。
        let header_view = view.clone();
        let handle_widths = widths.clone();
        let mut resize_offset = ROWNUM_WIDTH + SELECT_COLUMN_WIDTH;
        let resize_handles = visible.iter().map(move |(index, column)| {
            let width = handle_widths
                .get(*index)
                .copied()
                .unwrap_or(COLUMN_MIN_WIDTH);
            let left = resize_offset + width - COLUMN_RESIZE_HANDLE / 2.;
            resize_offset += width;
            let col = *index;
            let view = header_view.clone();
            div()
                .id(SharedString::from(format!("db-col-resize-{}", column.name)))
                .absolute()
                .top_0()
                .left(px(left))
                .w(px(COLUMN_RESIZE_HANDLE))
                .h(px(GRID_HEADER_HEIGHT))
                .occlude()
                .cursor(CursorStyle::ResizeColumn)
                .on_mouse_down(MouseButton::Left, {
                    let view = view.clone();
                    move |event, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.begin_col_resize(col, event.position, event.click_count, cx);
                        });
                    }
                })
                .on_drag(ColResizeDrag { col }, |drag, _, _, cx| {
                    // 别让这次按下再冒泡成「点列头排序」。
                    cx.stop_propagation();
                    cx.new(|_| drag.clone())
                })
                .on_drag_move::<ColResizeDrag>({
                    let view = view.clone();
                    move |event, _, cx| {
                        // 拖拽载荷里带的是**真正在拖的那一列**：多列把手各自注册了
                        // 同一个监听，用载荷的列号而不是闭包捕获的列号，才不会被
                        // 光标掠过的邻列抢走。
                        let dragged = event.drag(cx).col;
                        let x = f32::from(event.event.position.x);
                        let _ = view.update(cx, |this, cx| this.drag_col_resize(dragged, x, cx));
                    }
                })
                .on_mouse_up_out(MouseButton::Left, {
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, _| this.end_col_resize());
                    }
                })
        });

        let header = h_flex()
            .relative()
            .flex_none()
            .h(px(GRID_HEADER_HEIGHT))
            .bg(tokens.colors.surface)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .overflow_x_hidden()
            // 行号列头（设计稿 rownum：`#`，底色比网格面深一档）。
            .child(
                div()
                    .w(px(ROWNUM_WIDTH))
                    .h(px(GRID_HEADER_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rownum_bg)
                    .border_r(thin)
                    .border_color(hairline)
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child("#"),
            )
            .child(
                div()
                    .w(px(SELECT_COLUMN_WIDTH))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_r(thin)
                    .border_color(hairline)
                    .child(grid_checkbox(
                        SharedString::from("db-select-all"),
                        header_state,
                        // 查询模式的客户端分页下勾的是**全部已加载行**（跨页），
                        // 不再说「本页」——表浏览的整页即全部已加载，两种模式都成立。
                        SharedString::from(tr(cx, "勾选全部行")),
                        {
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.toggle_all_checked(cx));
                            }
                        },
                        cx,
                    )),
            )
            .children(visible.iter().map(|(index, column)| {
                let sortable = self.mode == Mode::Table;
                let arrow = if sortable {
                    match self.table_sort.as_ref() {
                        Some(spec) if spec.column == column.name && spec.descending => Some(" ↓"),
                        Some(spec) if spec.column == column.name => Some(" ↑"),
                        _ => None,
                    }
                } else {
                    None
                };
                // 两行列头（设计稿 colhead）：上行列名（含排序箭头），下行类型。
                let cell = div()
                    .id(SharedString::from(format!("db-head-{}", column.name)))
                    .w(px(widths.get(*index).copied().unwrap_or(COLUMN_MIN_WIDTH)))
                    .flex_none()
                    .px(tokens.spacing.xs)
                    .flex()
                    .flex_col()
                    .justify_center()
                    .overflow_x_hidden()
                    .border_r(thin)
                    .border_color(hairline)
                    .text_size(tokens.typography.xs.size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.muted_foreground)
                    .child(div().flex_none().truncate().child(match arrow {
                        Some(arrow) => SharedString::from(format!("{}{arrow}", column.name)),
                        None => SharedString::from(column.name.clone()),
                    }))
                    // 类型行：引擎报什么画什么；拿不到类型就只画一行，别摆空档。
                    .when(!column.type_name.is_empty(), |cell| {
                        cell.child(
                            div()
                                .flex_none()
                                .truncate()
                                .font_weight(FontWeight::NORMAL)
                                .text_color(tertiary)
                                .child(SharedString::from(column.type_name.clone())),
                        )
                    });
                if !sortable {
                    // SQL 结果集的列头不可排：那块内容由用户自己的 ORDER BY 决定，
                    // 界面上再加一层排序会与语句里的写法打架。
                    return cell.into_any_element();
                }
                // 列头点一次「升 → 降 → 取消」；排序在服务器侧做（见 [`page_order`]）。
                let name = column.name.clone();
                cell.cursor_pointer()
                    .role(Role::Button)
                    .aria_label(SharedString::from(trf(cx, "按 {} 排序", &[&column.name])))
                    .hover(move |style| style.bg(hover))
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let name = name.clone();
                            let _ = view.update(cx, |this, cx| this.toggle_table_sort(&name, cx));
                        }
                    })
                    // 列头右键：直接定序 / 预选过滤列 / 复制列名（DBX / TablePlus 的
                    // 标准列头菜单；「筛选」只是把草稿列填上，仍要点「添加」生效）。
                    .context_menu({
                        let view = view.clone();
                        let menu_name = column.name.clone();
                        let asc_label = trf(cx, "按 {} 升序", &[&column.name]).to_string();
                        let desc_label = trf(cx, "按 {} 降序", &[&column.name]).to_string();
                        let filter_label = trf(cx, "筛选「{}」", &[&column.name]).to_string();
                        let copy_label = tr(cx, "复制列名");
                        let copied_note = tr(cx, "已复制列名。");
                        move |menu, _, _| {
                            let asc_view = view.clone();
                            let asc_name = menu_name.clone();
                            // 菜单闭包是 `Fn`（右键一次构建一次）：String 标签只许
                            // 借出克隆，不许搬走（`&'static str` 的文案是 Copy，无此约束）。
                            let mut menu = menu
                                .item(PopupMenuItem::new(asc_label.clone()).on_click(
                                    move |_, _, cx| {
                                        let Some(entity) = asc_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.sort_table_by(&asc_name, false, cx);
                                        });
                                    },
                                ))
                                .item(PopupMenuItem::new(desc_label.clone()).on_click({
                                    let view = view.clone();
                                    let desc_name = menu_name.clone();
                                    move |_, _, cx| {
                                        let Some(entity) = view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.sort_table_by(&desc_name, true, cx);
                                        });
                                    }
                                }));
                            let filter_view = view.clone();
                            let filter_name = menu_name.clone();
                            menu = menu
                                .item(PopupMenuItem::new(filter_label.clone()).on_click(
                                    move |_, _, cx| {
                                        let Some(entity) = filter_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.draft_filter_column(&filter_name, cx);
                                        });
                                    },
                                ))
                                .separator();
                            let copy_view = view.clone();
                            let copy_name = menu_name.clone();
                            menu.item(PopupMenuItem::new(copy_label).on_click(move |_, _, cx| {
                                let Some(entity) = copy_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.put_on_clipboard(copy_name.clone(), copied_note, cx);
                                });
                            }))
                        }
                    })
                    .into_any_element()
            }))
            // 列宽把手最后追加：绘制顺序靠后 = 优先接到拖拽。
            .children(resize_handles);

        // 单元格右键菜单（网格**唯一**一层）的「复制 ▸」档位：文案与可插入判定都在
        // 渲染期取好 —— `cx` 与 `self` 都进不了 `'static` 菜单闭包，闭包里只搬
        // 「档位 + 文案」对与两个 `Copy` 值。
        let copy_items: Vec<(ResultFormat, &'static str)> = ResultFormat::ALL
            .iter()
            .map(|format| (*format, format.copy_label(cx)))
            .collect();
        let can_insert = self.insert_context().is_some();

        let grid = uniform_list(
            "db-result-grid",
            // 搜索 + 客户端分页后的**本页行**：`page_rows` 把「列表下标」翻译回原始行
            // 下标（选中 / 勾选都仍用原始下标，闭包里不许再换算一次）。
            matched_count.max(1),
            move |range, _window, cx| {
                range
                    .filter_map(|vis_index| {
                        let index = *page_rows.get(vis_index)?;
                        let row = rows.get(index)?;
                        let selected = selected_row == Some(index);
                        let checked = checked_rows.contains(&index);
                        // 这一行是否正在就地编辑（行号格要跟着点亮，见下面 rownum）。
                        let editing_row =
                            cell_editing.as_ref().map(|(row, _, _)| *row) == Some(index);
                        // 右键菜单闭包 `'static`：整行文本在这里备好（单元格菜单的
                        // 「复制整行」用；行级菜单已并入单元格菜单，见下面）。
                        let row_text = row_as_tsv(row);
                        Some(
                            h_flex()
                                .id(SharedString::from(format!("db-row-{index}")))
                                .h(px(GRID_ROW_HEIGHT))
                                .w_full()
                                .flex_none()
                                .items_center()
                                .overflow_x_hidden()
                                // 行框线（dbx 同款：每行一条底线，格与行不再糊成一片）。
                                .border_b(thin)
                                .border_color(hairline)
                                .when(selected, |row| row.bg(selected_bg))
                                .hover(move |style| style.bg(hover))
                                .on_click({
                                    let view = view.clone();
                                    move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let Some(entity) = view.upgrade() else {
                                            return;
                                        };
                                        // 单击 = 选中这一行；**双击**才进编辑（dbx 同款：
                                        // 选中与编辑是两步，单击开弹窗等于把浏览打断成编辑）。
                                        // 单元格上的点击在内层已 `stop_propagation`，走不到这。
                                        let clicks = match event {
                                            ClickEvent::Mouse(mouse) => mouse.up.click_count,
                                            ClickEvent::Touch(tap) => tap.tap_count,
                                            ClickEvent::Keyboard(_) => 1,
                                        };
                                        if clicks <= 1 {
                                            let _ = view.update(cx, |this, cx| {
                                                this.selected_row = Some(index);
                                                cx.notify();
                                            });
                                            return;
                                        }
                                        // 双击：只读借用阶段取数据，随后构造输入框实体。
                                        let specs = entity.read(cx).row_editor_specs(index);
                                        let cells = specs.map(|columns| {
                                            columns
                                                .into_iter()
                                                .map(|(column, original, was_null)| {
                                                    let input = cx.new(|cell_cx| {
                                                        InputState::new(&mut *window, cell_cx)
                                                    });
                                                    // 原值回填：编辑器的起点就是「库里现在是什么」。
                                                    if !was_null {
                                                        input.update(cx, |state, cx| {
                                                            state.set_value(
                                                                original.clone(),
                                                                window,
                                                                cx,
                                                            );
                                                        });
                                                    }
                                                    RowCell {
                                                        column,
                                                        original,
                                                        was_null,
                                                        input,
                                                        null: Rc::new(std::cell::Cell::new(false)),
                                                    }
                                                })
                                                .collect()
                                        });
                                        let _ = view.update(cx, |this, cx| {
                                            this.selected_row = Some(index);
                                            this.row_editor = cells.map(|cells| RowEditor {
                                                row_index: Some(index),
                                                cells,
                                            });
                                            cx.notify();
                                        });
                                    }
                                })
                                // 行号列（设计稿 rownum）：显示**结果里的绝对序号**——
                                // 客户端分页翻到第 2 页从 201 起编，跟「第几行数据」一致。
                                // 正在就地编辑的行，行号跟着点亮（dbx 同款：编辑态
                                // 不止落在格子上，行号也参与指位）。
                                .child(
                                    div()
                                        .w(px(ROWNUM_WIDTH))
                                        .h(px(GRID_ROW_HEIGHT))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_end()
                                        .px(cell_pad)
                                        .bg(if editing_row {
                                            selection_bg
                                        } else {
                                            rownum_bg
                                        })
                                        .border_r(thin)
                                        .border_color(hairline)
                                        .text_size(cell_text)
                                        .text_color(if editing_row {
                                            accent_text
                                        } else {
                                            tertiary
                                        })
                                        .child(SharedString::from(
                                            (page_start + vis_index + 1).to_string(),
                                        )),
                                )
                                .child(
                                    div()
                                        .w(px(SELECT_COLUMN_WIDTH))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .border_r(thin)
                                        .border_color(hairline)
                                        .child(grid_checkbox(
                                            SharedString::from(format!("db-select-{index}")),
                                            if checked {
                                                CheckState::Checked
                                            } else {
                                                CheckState::Unchecked
                                            },
                                            SharedString::from(format!("第 {} 行", index + 1)),
                                            {
                                                let view = view.clone();
                                                move |_, _, cx| {
                                                    // 勾选框是行内的独立点击目标：
                                                    // 不拦一下，点它会把「选中这一行」
                                                    // （右侧行编辑器）也一起触发。
                                                    cx.stop_propagation();
                                                    let _ = view.update(cx, |this, cx| {
                                                        this.toggle_row_checked(index, cx);
                                                    });
                                                }
                                            },
                                            cx,
                                        )),
                                )
                                // 行级右键菜单已并入**单元格**菜单（复制单元格 / 复制整行 /
                                // 复制全部 / 单元格详情）：gpui-component 的 `ContextMenu`
                                // 右键处理不截停冒泡，行与格两层嵌套会各弹一份叠在一起 ——
                                // 网格里只留最内一层（行号 / 勾选列右键无菜单，dbx 同款语义
                                // 是「菜单只挂在数据格上」）。
                                .children(visible.iter().filter_map(|(cell_index, _column)| {
                                    // 藏起来的列直接跳过；取宽仍用**原始**列下标
                                    //（`widths` 按完整列序对齐，见上面 `visible` 的定义）。
                                    let cell_index = *cell_index;
                                    let cell = row.get(cell_index)?;
                                    // 右键菜单的闭包是 `'static`：单元格文本先取出来带走。
                                    let cell_value = cell.render();
                                    let null_cell = cell.is_null();
                                    let copy_cell_label = tr(cx, "复制单元格");
                                    let copied_cell_note = tr(cx, "已复制单元格。");
                                    // 行级菜单并入单元格菜单后多出来的文案。
                                    let copy_row_label = tr(cx, "复制整行");
                                    let copied_row_note = tr(cx, "已复制该行。");
                                    // 「复制 ▸」的父项文案跟着选区走（dbx 同款：复制永远
                                    // 作用于选区）—— 勾选行优先，其次单击选中的行，都没有
                                    // 才整份结果。文案让目标一眼可辨，不再有「选中一行却
                                    // 复制了整页」的错位。
                                    let submenu_label: SharedString = if !checked_rows.is_empty()
                                    {
                                        trf(
                                            cx,
                                            "复制所选 {} 行",
                                            &[&checked_rows.len().to_string()],
                                        )
                                        .into()
                                    } else if selected_row.is_some() {
                                        tr(cx, "复制选中行").into()
                                    } else {
                                        tr(cx, "复制全部").into()
                                    };
                                    let detail_label = tr(cx, "单元格详情");
                                    // 就地编辑中的这一格：正文换成输入框（双击进入，
                                    // 回车提交 —— 订阅在 `install_cell_editor` 里挂着）。
                                    let editing_input = cell_editing.as_ref().and_then(
                                        |(edit_row, edit_col, input)| {
                                            (*edit_row == index
                                                && *edit_col == cell_index)
                                                .then(|| input.clone())
                                        },
                                    );
                                    Some(
                                        div()
                                            .id(SharedString::from(format!(
                                                "db-cell-{index}-{cell_index}"
                                            )))
                                            .w(px(widths
                                                .get(cell_index)
                                                .copied()
                                                .unwrap_or(COLUMN_MIN_WIDTH)))
                                            .flex_none()
                                            .px(cell_pad)
                                            .flex()
                                            .items_center()
                                            .overflow_x_hidden()
                                            .text_size(cell_text)
                                            .line_height(px(GRID_ROW_HEIGHT))
                                            // 格框线（dbx 同款：列与列之间一条竖线）。
                                            .border_r(thin)
                                            .border_color(hairline)
                                            .when_some(editing_input.clone(), |cell, input| {
                                                // 就地编辑的**整格**视觉（dbx / dbflux 同款）：
                                                // 2px 强调描边框住整格 + 提亮底色 —— 输入框
                                                // 本身 appearance(false)，没有这层框的话编辑态
                                                // 和普通格几乎没差别，用户看不出「已经进编辑」。
                                                // 后写的 `.border` 覆盖上面按边的 `border_r`。
                                                let view = view.clone();
                                                cell.bg(surface_bg)
                                                    .border(edit_strong)
                                                    .border_color(edit_ring)
                                                    .child(
                                                    h_flex()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .gap(cell_gap)
                                                        .child(
                                                            Input::new(&input)
                                                                .appearance(false)
                                                                .with_size(Size::Small)
                                                                .flex_1()
                                                                .min_w_0(),
                                                        )
                                                        .child(
                                                            div()
                                                                .id(SharedString::from(format!(
                                                                    "db-cell-commit-{index}-{cell_index}"
                                                                )))
                                                                .flex_none()
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .w(px(18.))
                                                                .h(px(18.))
                                                                .rounded(radius_sm)
                                                                .cursor_pointer()
                                                                .role(Role::Button)
                                                                .aria_label(tr(cx, "提交"))
                                                                .hover(move |style| style.bg(hover_tint))
                                                                .on_click({
                                                                    let view = view.clone();
                                                                    move |_, _, cx| {
                                                                        cx.stop_propagation();
                                                                        let Some(entity) =
                                                                            view.upgrade()
                                                                        else {
                                                                            return;
                                                                        };
                                                                        entity.update(cx, |this, cx| {
                                                                            this.commit_cell_edit(cx);
                                                                        });
                                                                    }
                                                                })
                                                                .child(
                                                                    Icon::new(DuckIcon::Check)
                                                                        .size(icon_sm)
                                                                        .text_color(success),
                                                                ),
                                                        ),
                                                )
                                            })
                                            .when(editing_input.is_none(), |cell| {
                                                // 悬停 ⓘ（dbx 同款）：详情入口**贴在每格上**，
                                                // 平时隐形、悬停所在格才现身（0 / 1 是显隐
                                                // 开关，不是透明档位；`group` 与 `group_hover`
                                                // 必须成对，组名带行列号防串格）。
                                                let group = SharedString::from(format!(
                                                    "db-cell-group-{index}-{cell_index}"
                                                ));
                                                cell.group(group.clone())
                                                    .relative()
                                                    .child(
                                                        div()
                                                            .truncate()
                                                            .when(null_cell, |text| {
                                                                text.text_color(tertiary)
                                                            })
                                                            .child(cell_value.clone()),
                                                    )
                                                    .child(
                                                        div()
                                                            .id(SharedString::from(format!(
                                                                "db-cell-info-{index}-{cell_index}"
                                                            )))
                                                            .absolute()
                                                            .right(px(2.))
                                                            .top(px(0.))
                                                            .bottom(px(0.))
                                                            .flex()
                                                            .items_center()
                                                            .cursor_pointer()
                                                            .opacity(0.)
                                                            .group_hover(group, |style| {
                                                                style.opacity(1.)
                                                            })
                                                            .role(Role::Button)
                                                            .aria_label(detail_label)
                                                            .tooltip({
                                                                let tip = SharedString::from(detail_label);
                                                                move |window, cx| {
                                                                    Tooltip::new(tip.clone())
                                                                        .build(window, cx)
                                                                }
                                                            })
                                                            .on_click({
                                                                let view = view.clone();
                                                                move |_, _, cx| {
                                                                    cx.stop_propagation();
                                                                    let Some(entity) = view.upgrade()
                                                                    else {
                                                                        return;
                                                                    };
                                                                    entity.update(cx, |this, cx| {
                                                                        this.open_cell_detail(
                                                                            index, cell_index, cx,
                                                                        );
                                                                    });
                                                                }
                                                            })
                                                            .child(
                                                                div()
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .w(px(18.))
                                                                    .h(px(18.))
                                                                    .rounded(radius_sm)
                                                                    .bg(surface_bg)
                                                                    .child(
                                                                        Icon::new(DuckIcon::Info)
                                                                            .size(icon_sm)
                                                                            .text_color(tertiary),
                                                                    ),
                                                            ),
                                                    )
                                            })
                                            // 单击选中这一格（ⓘ 的详情指向）；双击就地编辑。
                                            // 不拦一下，点击会先落到「选中这一行」上。
                                            .on_click({
                                                let view = view.clone();
                                                move |event: &ClickEvent,
                                                      window: &mut Window,
                                                      cx: &mut App| {
                                                    cx.stop_propagation();
                                                    let Some(entity) = view.upgrade() else {
                                                        return;
                                                    };
                                                    let double = match event {
                                                        ClickEvent::Mouse(mouse) => {
                                                            mouse.up.click_count > 1
                                                        }
                                                        ClickEvent::Touch(tap) => tap.tap_count > 1,
                                                        ClickEvent::Keyboard(_) => false,
                                                    };
                                                    let prep = if double {
                                                        entity.read(cx).cell_editor_prep(
                                                            index, cell_index,
                                                        )
                                                    } else {
                                                        None
                                                    };
                                                    let Some((original, was_null)) = prep else {
                                                        let _ = view.update(cx, |this, cx| {
                                                            this.selected_row = Some(index);
                                                            this.cell_detail =
                                                                Some((index, cell_index));
                                                            cx.notify();
                                                        });
                                                        return;
                                                    };
                                                    // 结果集不可编辑就明说，不摆空编辑框。
                                                    if entity.read(cx).edit_source().is_none() {
                                                        notify::info(
                                                            tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                                                            window,
                                                            cx,
                                                        );
                                                        return;
                                                    }
                                                    let input = cx.new(|cell_cx| {
                                                        InputState::new(&mut *window, cell_cx)
                                                    });
                                                    if !was_null {
                                                        let seed = original.clone();
                                                        input.update(cx, |state, cx| {
                                                            state.set_value(seed, window, cx);
                                                        });
                                                    }
                                                    // dbx / dbflux 同款：进编辑即聚焦 + **全选**
                                                    // ——高亮选区本身就是「输入框已接管这一格」
                                                    // 的视觉信号，直接打字即可整格覆盖。
                                                    input.update(cx, |state, cx| {
                                                        state.focus(window, cx);
                                                        state.select_all(window, cx);
                                                    });
                                                    let _ = view.update(cx, |this, cx| {
                                                        this.selected_row = Some(index);
                                                        this.cell_detail = Some((index, cell_index));
                                                        this.install_cell_editor(
                                                            index, cell_index, original, was_null,
                                                            input, cx,
                                                        );
                                                        cx.notify();
                                                    });
                                                }
                                            })
                                            .context_menu({
                                                // **网格唯一的一层右键菜单**（dbx 同款：一个弹层
                                                // 入口，内容按目标合成）。行级菜单并到了这里，
                                                // 面板层的「复制为 …」收进「复制 ▸」子菜单 ——
                                                // 组件库的 ContextMenu 不截停冒泡，两层嵌套
                                                // 会各弹一份叠在一起（外层还盖在内层上面）。
                                                let view = view.clone();
                                                let row_text = row_text.clone();
                                                let copy_items = copy_items.clone();
                                                move |menu, window, cx| {
                                                    let view = view.clone();
                                                    let text = cell_value.clone();
                                                    let row_text = row_text.clone();
                                                    let all_view = view.clone();
                                                    let detail_view = view.clone();
                                                    let cell_view = view.clone();
                                                    let items = copy_items.clone();
                                                    // 「复制 ▸」子菜单：一次拿走**全部**行。文本
                                                    // 在点某档的那一刻才按当前结果集渲染（一万行
                                                    // 也不在渲染路径上白白拼几百 KB）。
                                                    let copy_menu = PopupMenu::build(
                                                        window,
                                                        cx,
                                                        |menu, _, _| {
                                                            items.iter().fold(menu, |menu, (format, label)| {
                                                                let enabled = format.exportable(
                                                                    true, can_insert, false,
                                                                );
                                                                menu.item(
                                                                    PopupMenuItem::new(*label)
                                                                        .disabled(!enabled)
                                                                        .on_click({
                                                                            let view =
                                                                                all_view.clone();
                                                                            let format = *format;
                                                                            move |_, _, cx| {
                                                                                let Some(entity) =
                                                                                    view.upgrade()
                                                                                else {
                                                                                    return;
                                                                                };
                                                                                entity.update(
                                                                                    cx, |this, cx| {
                                                                                        this.copy_result_as(
                                                                                            format, cx,
                                                                                        );
                                                                                    },
                                                                                );
                                                                            }
                                                                        }),
                                                                )
                                                            })
                                                        },
                                                    );
                                                    menu.item(
                                                        PopupMenuItem::new(copy_cell_label)
                                                            .on_click(move |_, _, cx| {
                                                                let Some(entity) = cell_view.upgrade()
                                                                else {
                                                                    return;
                                                                };
                                                                entity.update(cx, |this, cx| {
                                                                    this.put_on_clipboard(
                                                                        text.clone(),
                                                                        copied_cell_note,
                                                                        cx,
                                                                    );
                                                                });
                                                            }),
                                                    )
                                                    // 整行一条 TSV，粘进表格软件就是一整行单元格。
                                                    .item(
                                                        PopupMenuItem::new(copy_row_label)
                                                            .on_click({
                                                                let view = view.clone();
                                                                move |_, _, cx| {
                                                                    let Some(entity) = view.upgrade()
                                                                    else {
                                                                        return;
                                                                    };
                                                                    entity.update(cx, |this, cx| {
                                                                        this.put_on_clipboard(
                                                                            row_text.clone(),
                                                                            copied_row_note,
                                                                            cx,
                                                                        );
                                                                    });
                                                                }
                                                            }),
                                                    )
                                                    .item(PopupMenuItem::submenu(
                                                        // 菜单闭包是 `Fn`（每次右键重建一次）：
                                                        // 标题只许借出克隆，不许搬走。
                                                        submenu_label.clone(),
                                                        copy_menu,
                                                    ))
                                                    .item(
                                                        PopupMenuItem::new(detail_label).on_click(
                                                            move |_, _, cx| {
                                                                let Some(entity) =
                                                                    detail_view.upgrade()
                                                                else {
                                                                    return;
                                                                };
                                                                entity.update(cx, |this, cx| {
                                                                    this.open_cell_detail(
                                                                        index, cell_index, cx,
                                                                    );
                                                                });
                                                            },
                                                        ),
                                                    )
                                                }
                                            }),
                                    )
                                }))
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .flex_1()
        .min_h_0()
        // 纵向滚动句柄交给页面持有（跨帧持久）：纵向滚动条从它读位置 ——
        // 滚动时现身、停下淡出（[`ScrollbarMode::Scrolling`]，见 overlay 处）。
        .track_scroll(&self.result_v_scroll);

        let mut panel = v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            // dbx 同款「聚焦的网格拥有剪贴板键」：点进结果区就把焦点挪到网格，
            // `⌘C / ⌘A / ⌘X / ⌘V` 才会派发到页面上下文 —— 否则焦点仍留在
            // 先前聚焦的 SQL 编辑器里，这些键全被它更深一层抢走。
            .track_focus(&self.grid_focus)
            .on_mouse_down(MouseButton::Left, {
                let focus = self.grid_focus.clone();
                move |_, window, cx| focus.focus(window, cx)
            });
        // **结果标签行压在结果区第一行**（用户反馈：tab 这行上移一行放顶上）：
        // 标签说的是「现在看的是哪条语句的结果」，是这块区域的标题；档位行（数据表 /
        // 摘要 / 图表 / 消息 / 执行计划）与刷新 / 写动作组跟在它后面。dbx 同款 ——
        // 结果集标签在工具条最左，视图切换器在其后。
        panel = panel.child(self.render_result_resbar(cx));
        panel = panel.child(self.render_result_tabbar(cx));
        // 计划头条（「原始输出」档才走到这里；树档在上面已经返回了）。
        if let Some(bar) = plan_bar {
            panel = panel.child(bar);
        }
        // 执行报错条：失败后「看结果的地方」也要看得见（编辑器上方那条带 AI 动作，
        // 这条纯展示；随下一次成功执行消失 —— `last_error` 就地清空）。
        panel = panel.children(self.render_result_error_banner(cx));
        // 多选条只在勾了行时出现，且不限模式（SQL 结果集同样可以批量改 / 删）。
        if !self.checked_rows.is_empty() {
            panel = panel.child(self.render_batch_bar(cx));
        }
        // 过滤条只在表浏览模式出现：过滤是「按这张表的列下条件重取这一页」，
        // SQL 结果集没有这层语义（要过滤就在 SQL 里写 WHERE）。
        if self.mode == Mode::Table {
            panel = panel.child(self.render_filter_bar(cx));
        }
        // 列宽之和经常超过视口（宽表 / 窄窗口 / 多列结果集）：把「列头 + 行网格」
        // 装进同一个横向滚动区，两者共用一份横向偏移，滚动时不会错位。行与单元格
        // 各自仍有 `overflow_x_hidden`（负责单元格内截断），所以溢出必须在这里被
        // 滚动消化掉，否则右侧列会被整块裁掉、连滚动条都没有。
        //
        // 滚动条**常驻**（`ScrollbarMode::Always`）：gpui-component 的
        // `overflow_x_scrollbar` 包装器不透出 mode，档位走主题投影 = Hover
        // （悬停才现身）——数据网格里等于没有可发现的滚动条。Scrollbar 自己会在
        // 无溢出时隐藏（gpui-base scrollbar.rs「Hide scrollbar, if the scroll area
        // is smaller than the container」），不会摆一条死轨道。
        //
        // `restrict_scroll_to_axis` 锁轴：竖向滚轮只归行列表纵向滚动，不会被映射
        // 成横向滚动；横向靠触控板横扫、Shift+滚轮或拖这条滚动条。
        // 过滤条 / 编辑条 / 批量条 / 翻页条留在滚动区外，保持固定（同主流数据库客户端）。
        let content = v_flex()
            .flex_none()
            .w_auto()
            .min_w_full()
            .child(header)
            // 搜了个词却一行没中：必须明说，否则「空网格」与「查询返回了 0 行」
            // 分不清（前者清掉搜索词就有数据）。
            .when(no_match, |content| {
                content.child(
                    div()
                        .flex_none()
                        .px(tokens.spacing.sm)
                        .py(tokens.spacing.xs)
                        .child(meta_text(
                            trf(cx, "没有命中「{}」的行。", &[&search_text]),
                            cx,
                        )),
                )
            })
            .child(grid);
        panel = panel.child(
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .relative()
                .child(
                    div()
                        .id("db-result-x-area")
                        .size_full()
                        .flex()
                        .flex_row()
                        .overflow_x_scroll()
                        .restrict_scroll_to_axis()
                        .track_scroll(&self.result_x_scroll)
                        .child(content),
                )
                .child(
                    div().absolute().inset_0().child(
                        Scrollbar::horizontal(&self.result_x_scroll)
                            .id("db-result-x-scrollbar")
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    ),
                )
                // 纵向滚动条（用户反馈：侧边滚动条要在滚动时显示）：档位
                // [`ScrollbarMode::Scrolling`] = 滚动时现身、停下淡出 —— 数据
                // 网格行多，常驻太重、悬停才现身又等于不可发现，滚动态是
                // dbx / 主流客户端的折衷。句柄是 `uniform_list` 自己的
                // （见上面 `track_scroll`），无溢出时 Scrollbar 自己隐藏。
                .child(
                    div().absolute().inset_0().child(
                        Scrollbar::vertical(self.result_v_scroll.as_ref())
                            .id("db-result-v-scrollbar")
                            .mode(ScrollbarMode::Scrolling),
                    ),
                ),
        );
        // 截断提示与「加载更多」互斥，口径一致：能续拉就给按钮，不能续拉才如实说「只展示了前一部分」。
        if self.mode == Mode::Query && self.query_has_more {
            panel = panel.child(self.render_load_more_bar(cx));
        } else if truncated {
            panel = panel.child(
                div()
                    .flex_none()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xxs)
                    .child(meta_text(
                        tr(
                            cx,
                            "结果已达行数上限，仅展示前一部分；可加 LIMIT 或导出 CSV。",
                        ),
                        cx,
                    )),
            );
        }
        // 脚注（设计稿 grid-foot）：读数 + 导出 +（表浏览）翻页，常驻、跟结果共存亡。
        panel = panel.child(self.render_result_footer(cx));
        // ⚠️ 面板层**不再挂右键菜单**：gpui-component 的 `ContextMenu` 右键处理不截停
        // 冒泡，面板与单元格两层嵌套会各弹一份叠在一起（用户点「复制整行」实际看到
        // 的却是外层的「复制为 TSV」，整页结果进了剪贴板）。「复制全部」已并入单元格
        // 菜单（`ResultFormat::Tsv` 一档），其余格式走脚注「导出」落盘。
        panel.into_any_element()
    }

    /// 编辑条：`pending_edits` 模型的界面落点（新增行 + 待提交计数 + 提交 / 丢弃）。
    ///
    /// 与「多选工具条」分开排：多选条是**临时的**（勾了才有），编辑条只要结果集可编辑
    /// 就常驻——「我这一批改动攒着没提交」这件事必须随时看得见。
    /// 多选工具条：勾了行才出现（盖在网格表头之上）。
    ///
    /// 两个写动作都落在**单事务**内核接口上（批量改 → [`Connection::update_cells`]，
    /// 批量删 → [`Connection::delete_row_set`]）：「要么这一批全成、要么全不动」是
    /// 跨行操作的底线——半截成功比失败更难收拾。
    pub(super) fn render_batch_bar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let selected = self.checked_rows.len();
        // 「能不能批量改」与「能不能改这一格」同源：结果集可编辑（单表 + 主键齐全）。
        let has_pk = self.edit_source().is_some();
        let columns = self
            .current_result()
            .map(|result| result.columns.clone())
            .unwrap_or_default();
        let options: Vec<(SharedString, SharedString)> = columns
            .iter()
            .map(|column| {
                let name = SharedString::from(column.name.clone());
                (name.clone(), name)
            })
            .collect();
        let current: SharedString = self
            .batch_column
            .clone()
            .map_or_else(|| SharedString::from(tr(cx, "选择列")), SharedString::from);
        let on_select = {
            let view = view.clone();
            move |value: SharedString, _window: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |this, cx| {
                    this.batch_column = Some(value.to_string());
                    cx.notify();
                });
            }
        };

        let mut bar = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .bg(tokens.colors.surface)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(accent_tag(
                SharedString::from(trf(cx, "已选 {} 行", &[&selected.to_string()])),
                cx,
            ))
            .child(dropdown_select(
                "db-batch-column",
                options,
                current,
                on_select,
                cx,
            ))
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(grid_checkbox(
                        SharedString::from("db-batch-null"),
                        if self.batch_null {
                            CheckState::Checked
                        } else {
                            CheckState::Unchecked
                        },
                        SharedString::from(tr(cx, "把选中列整列写成 NULL")),
                        {
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.batch_null = !this.batch_null;
                                    cx.notify();
                                });
                            }
                        },
                        cx,
                    ))
                    .child(meta_text(tr(cx, "写 NULL"), cx)),
            )
            .child(
                // 批量改值输入框的宽度区间：布局约束（不在 spacing 阶梯上），保留裸值；
                // `min_w` 保底可读、`max_w` 防止长值把整行撑爆，中间 `flex_1` 自适应。
                div()
                    .min_w(px(160.))
                    .max_w(px(260.))
                    .flex_1()
                    .when(self.batch_null, |slot| {
                        // 勾了「写 NULL」时输入框淡出（内容会被忽略）——与行编辑器的
                        // NULL 开关同一处理，不真禁用（值仍可读）。
                        slot.opacity(extended.opacity.disabled)
                    })
                    .child(
                        Input::new(&self.batch_value)
                            .h(theme.density().control)
                            .text_size(tokens.typography.sm.size)
                            .w_full(),
                    ),
            )
            .child(
                button(
                    "db-batch-apply",
                    tr(cx, "应用到选中行"),
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(self.busy || !has_pk || self.batch_column.is_none())
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.apply_batch_edit(window, cx));
                    }
                }),
            )
            .child(
                button(
                    "db-batch-delete",
                    tr(cx, "删除选中行"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(self.busy || !has_pk)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.apply_batch_delete(window, cx));
                    }
                }),
            );
        if !has_pk {
            bar = bar.child(meta_text(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键齐全）。"),
                cx,
            ));
        }
        bar.child(div().flex_1())
            .child(
                button(
                    "db-batch-clear",
                    tr(cx, "清空选择"),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.clear_checked_rows();
                            cx.notify();
                        });
                    }
                }),
            )
            .into_any_element()
    }

    /// 表浏览的过滤条：攒一条草稿（列 + 操作符 + 值），点「添加条件」才生效。
    ///
    /// 生效中的条件以标签形式列在右侧，每条一个 ✕；条件一变就重取第一页
    /// （见 [`Self::after_filter_changed`]）。
    ///
    /// **值的类型合法性不在这里拦**：驱动把值走绑定参数发给引擎，服务端的报错原样
    /// 透出——在界面里猜类型（"这列看着像数字"）只会猜错，还把真正的错误信息换成了
    /// 一句含糊的本地提示。
    pub(super) fn render_filter_bar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let columns = self
            .current_result()
            .map(|result| result.columns.clone())
            .unwrap_or_default();
        let options: Vec<(SharedString, SharedString)> = columns
            .iter()
            .map(|column| {
                let name = SharedString::from(column.name.clone());
                (name.clone(), name)
            })
            .collect();
        let current_column: SharedString = self
            .filter_column
            .clone()
            .map_or_else(|| SharedString::from(tr(cx, "选择列")), SharedString::from);

        let mut bar = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .bg(tokens.colors.surface)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(meta_text(tr(cx, "过滤"), cx))
            .child(dropdown_select(
                "db-filter-column",
                options,
                current_column,
                {
                    let view = view.clone();
                    move |value, _window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.filter_column = Some(value.to_string());
                            cx.notify();
                        });
                    }
                },
                cx,
            ))
            .child(dropdown_select(
                "db-filter-op",
                FilterOp::ALL
                    .into_iter()
                    .map(|op| {
                        (
                            SharedString::from(filter_op_key(op)),
                            SharedString::from(filter_op_label(op, cx)),
                        )
                    })
                    .collect::<Vec<_>>(),
                SharedString::from(filter_op_key(self.filter_op)),
                {
                    let view = view.clone();
                    move |value, _window, cx| {
                        let op = filter_op_from_key(&value);
                        let _ = view.update(cx, |this, cx| {
                            this.filter_op = op;
                            cx.notify();
                        });
                    }
                },
                cx,
            ))
            .child(
                div()
                    // 值输入框的宽度区间：布局约束（不在 spacing 阶梯上），保留裸值；
                    // 与批量条同口径（`min_w` 保底可读、`max_w` 防长值撑爆）。
                    .min_w(px(160.))
                    .max_w(px(260.))
                    .flex_1()
                    .when(!self.filter_op.needs_value(), |slot| {
                        // 「为空 / 不为空」不吃值：淡出而不禁用（值仍可读、仍可改，
                        // 与批量条的「写 NULL」同一处理）。
                        slot.opacity(extended.opacity.disabled)
                    })
                    .child(
                        Input::new(&self.filter_value)
                            .h(theme.density().control)
                            .text_size(tokens.typography.sm.size)
                            .w_full(),
                    ),
            )
            .child(
                button(
                    "db-filter-add",
                    tr(cx, "添加条件"),
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(self.busy || self.filter_column.is_none())
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.add_table_filter(window, cx));
                    }
                }),
            )
            .child(div().flex_1());
        if !self.table_filters.is_empty() {
            // 行数口径要如实：右侧对象的行数是**整表**口径，过滤只改「这一页取哪些行」。
            bar = bar
                .child(meta_text(tr(cx, "行数统计仍是整表口径"), cx))
                .children(
                    self.table_filters
                        .iter()
                        .enumerate()
                        .map(|(index, filter)| {
                            h_flex()
                                .flex_none()
                                .items_center()
                                .gap(tokens.spacing.xxs)
                                .child(neutral_tag(filter_chip_text(filter, cx), cx))
                                .child(
                                    icon_button(
                                        SharedString::from(format!("db-filter-remove-{index}")),
                                        tr(cx, "移除这条条件"),
                                        Icon::new(DuckIcon::X).size(extended.icon.sm),
                                        ButtonVariant::Ghost,
                                        cx,
                                    )
                                    .on_click({
                                        let view = view.clone();
                                        move |_, _, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.remove_table_filter(index, cx);
                                            });
                                        }
                                    }),
                                )
                                .into_any_element()
                        }),
                )
                .child(
                    button(
                        "db-filter-clear",
                        tr(cx, "清空条件"),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .disabled(self.busy)
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.clear_table_filters(cx));
                        }
                    }),
                );
        }
        bar.into_any_element()
    }

    /// 执行计划头条：节点数 + 「计划树 / 原始输出」切换。
    ///
    /// 两份输出**都留着**：树是整理的成果，原始文本是保底（解析总有认不出的形态，
    /// 而且有人就爱读原文）。所以这里的切换不是"要不要保留输出"，只是"看哪一份"。
    pub(super) fn render_plan_bar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let raw = self.plan_view.as_ref().is_some_and(|plan| plan.raw);
        let count = self
            .plan_view
            .as_ref()
            .map_or(0, |plan| plan.nodes.len())
            .to_string();
        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .bg(tokens.colors.surface)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(accent_tag(tr(cx, "执行计划"), cx))
            .child(meta_text(
                SharedString::from(trf(cx, "{} 步", &[&count])),
                cx,
            ))
            .child(div().flex_1())
            .child(
                button(
                    "db-plan-toggle",
                    if raw {
                        tr(cx, "计划树")
                    } else {
                        tr(cx, "原始输出")
                    },
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if let Some(plan) = this.plan_view.as_mut() {
                                plan.raw = !plan.raw;
                            }
                            cx.notify();
                        });
                    }
                }),
            )
            .into_any_element()
    }
}

/// 计划树的每层缩进（布局约束，不在 spacing 阶梯上），保留裸值。
const PLAN_INDENT: f32 = 14.;

/// 渲染执行计划树：一步一行，按层级缩进，明细跟在同行后面。
///
/// 不用 `uniform_list`：一份计划通常几十步，虚拟化省下的那点开销抵不上它的复杂度
/// —— 何况这里的行高并不一致（明细长起来会折行）。
fn plan_tree(nodes: Vec<PlanNode>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let label_color = extended.colors.accent_text;
    let detail_color = extended.colors.text_tertiary;
    let label_size = tokens.typography.sm.size;
    let detail_size = tokens.typography.xs.size;
    v_flex()
        .id("db-plan-tree")
        .flex_1()
        .min_h_0()
        .w_full()
        .gap(tokens.spacing.xxs)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.sm)
        .overflow_y_scroll()
        .children(nodes.into_iter().enumerate().map(|(index, node)| {
            // 明细拼成一行 `键: 值 · 键: 值`：计划里的明细是**陪衬**，一行读得完比
            // 铺成两列表格更省地方（也就少一层横向滚动）。
            let detail = node
                .details
                .iter()
                .map(|(key, value)| format!("{key}: {value}"))
                .collect::<Vec<_>>()
                .join(" · ");
            h_flex()
                .id(SharedString::from(format!("db-plan-{index}")))
                .items_center()
                .gap(tokens.spacing.xs)
                // 缩进用「撑开的空盒子」而不是左边距：层级是**相对**的，一个定宽的
                // 前导块比逐层累加的边距更好对齐（也与结果网格里的列宽同一种做法）。
                .child(div().flex_none().w(px(node.depth as f32 * PLAN_INDENT)))
                .child(
                    div()
                        .flex_none()
                        .text_size(label_size)
                        .text_color(label_color)
                        .child(node.label),
                )
                .when(!detail.is_empty(), |row| {
                    row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(detail_size)
                            .text_color(detail_color)
                            .child(detail),
                    )
                })
                .into_any_element()
        }))
        .into_any_element()
}
