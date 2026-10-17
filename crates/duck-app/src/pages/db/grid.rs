use super::*;
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

/// 列模式（转置视图）的列宽拖拽载荷：只带**列下标**（0 = 名称，1 = 值，2 = 类型）。
///
/// 行模式网格的同类载荷（原先的 `ColResizeDrag`）已经随网格一起搬进公共数据表组件；
/// 列模式仍是页面自己画的，所以它这份留在这里 —— 起拖的指针位置与起始列宽另存在
/// [`DatabasePage::columns_mode_resize`]，因为那两样在 `on_drag` 的构造回调里拿不到
/// 稳定值（它给的是元素内偏移，列宽一变就跟着变，用来算宽度会自激）。
#[derive(Clone)]
struct ColumnsModeResizeDrag {
    col: usize,
}

impl Render for ColumnsModeResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // 拖动期间跟手的是光标形状（`CursorStyle::ResizeColumn`），不需要拖影。
        Empty
    }
}

/// 右键「在结果中搜索此值」时，最多拿单元格文本的前多少个**字符**当搜索词。
///
/// 256 是个「普通值全部装得下、异常值一定被拦住」的量级：电话号码 / 邮箱 / UUID /
/// 短备注都在几十字符内；而 TEXT / JSON 列可以是整篇文档，那种长度当搜索词既
/// 没意义（没人按 1 MB 的串匹配），又会在**每一行**上做一次子串匹配。截断之后
/// 语义变成「按这段文本的开头过滤」—— 短值下与「按整个值过滤」完全一样。
const SEARCH_VALUE_MAX_CHARS: usize = 256;

/// 右键列头调**列序**的四种动作：都是显示顺序上的相对移动，不动数据。
///
/// 为什么用菜单而不是拖列头：列头文字是**可拖选 / 可 ⌘C** 的（`selectable_text`，
/// 见 `tests/column_header_detail_copy.rs`），在同一个元素上再挂一个拖拽源会与
/// 文本选择抢同一次「按下 + 移动」。菜单零手势冲突，且一眼能看出结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnNudge {
    Left,
    Right,
    First,
    Last,
}

/// 单元格右键菜单在**弹出那一刻**读齐的事实。
///
/// 菜单构建闭包是 `Fn`（每次右键 rebuild 一次），拿不到 `&DatabasePage`：弹出时
/// 一次读完、之后只借克隆。顺带把「一万行也不在渲染路径上白拼几百 KB」这条纪律
/// 落成结构 —— 文本只在真弹菜单时才算。
///
/// 每一项「能不能点」都在这里判完，**不在点击回调里兜底**：菜单里出现一个点了
/// 只弹「不可用」的项，比不放这一项更让人困惑。点击路径上的守卫（
/// [`DatabasePage::toggle_column_hidden`] 一类）仍然保留 —— 那是并发 / 竞态下的
/// 第二道闸，不是第一道。
struct CellMenuFacts {
    /// 这一格的列名（越界 = `None`）。
    column: Option<String>,
    /// 这一格的文本（[`Value::render`]，与网格里显示的一致）。
    cell_text: String,
    /// 整行一条 TSV（粘进表格软件就是一整行单元格）。
    row_text: String,
    /// 「复制 ▸」父项文案（跟着选区走，见菜单里的说明）。
    submenu_label: SharedString,
    /// 拖出多格选区时的「复制选区 N 格」文案；`None` = 只有一格 / 没选区。
    selection_label: Option<SharedString>,
    /// 结果集可写 → 「复制 ▸」里的 INSERT 档可用（与
    /// [`ResultFormat::exportable`] 的第二个入参同源）。
    can_insert: bool,
    /// 结果集可编辑（单表 + 主键齐全）→ 「编辑此单元格」可用。
    editable: bool,
    /// 这一行能按主键删（有主键且本行能定位 —— 与
    /// [`DatabasePage::delete_current_row`] 的守卫同源）。
    deletable: bool,
    /// 有列被藏着 → 菜单里给「全部显示」。
    any_hidden: bool,
    /// 列头三态排序在这一模式下有意义（表浏览 / SQL 结果各有一套落地）。
    sortable: bool,
    /// 当前生效的排序 → 决定两个方向项的勾选态与「清除排序」要不要出现。
    sort: Option<SortSpec>,
}

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
                .child(selectable_text(SharedString::from(one_line)))
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
            || self.active_sort().is_some()
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
                // 标签区可**横向滚动**：一条 SQL 一个标签，跑多了必然放不下。
                // `overflow_x_scroll` 前先 `.id(...)`；滚动条由外层相对容器叠加
                //（gpui 不画系统滚动条，不叠就没有可拖动的东西）。
                let mut tabs = h_flex()
                    .id("db-result-tabs")
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.result_tabs_x_scroll);
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
                bar.child(
                    // 标签区宿主：相对容器承托横向滚动条轨道（顶部标签条 / 结果
                    // 网格同款形态）；**悬停才现身**（与标签条同口径：常驻太吵）。
                    h_flex().flex_1().min_w_0().relative().child(tabs).child(
                        div().absolute().inset_0().child(
                            Scrollbar::horizontal(&self.result_tabs_x_scroll)
                                .id("db-result-tabs-x-scrollbar")
                                .mode(ScrollbarMode::Hover)
                                .viewport_from_layout(),
                        ),
                    ),
                )
                .child(
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
            // 排序读数（原编辑条迁来）：按列排过之后如实说一句，不然「顺序不对」
            // 没有任何线索。表浏览与 SQL 结果集各读自己那一份（[`Self::active_sort`]）。
            //
            // ⚠️ 只在**这一列真的还在结果里**时读数：客户端排序是**按列名**记的
            //（`query_sort`），同一标签里换一条语句重跑之后，旧列名可能已经不存在
            // —— 那时排序根本没生效，读数却还挂着「按 X 升序」，是一句假话。
            // 排序读数：多级排序时按优先级列出（`A 升序 · B 降序`），
            // 只列**当前结果里真的还有这一列**的键 —— 换语句重跑后旧列名可能
            // 已经不存在，那时排序没生效，读数也不能挂着。
            .when_some(
                self.current_result().and_then(|result| {
                    let keys: Vec<String> = self
                        .sort_keys_spec()
                        .into_iter()
                        .filter(|spec| {
                            result
                                .columns
                                .iter()
                                .any(|column| column.name == spec.column)
                        })
                        .map(|spec| {
                            format!(
                                "{} {}",
                                spec.column,
                                if spec.descending {
                                    tr(cx, "降序")
                                } else {
                                    tr(cx, "升序")
                                }
                            )
                        })
                        .collect();
                    (!keys.is_empty()).then(|| keys.join(" · "))
                }),
                |bar, keys| {
                    bar.child(neutral_tag(
                        SharedString::from(trf(cx, "按 {}", &[keys.as_str()])),
                        cx,
                    ))
                },
            )
            // 弹性带把读数与右端入口分开；来源语句 chip 已撤（摘要「执行过程」
            // 里的语句可复制，历史与右栏详情也能看全文）。⚠️ 有结果标签行时
            // 标签区自己就是 `flex_1`（放不下就横向滚动）—— 这里再给一个
            // `flex_1` 会跟它**对半分宽**，标签区最多只能占半条栏；所以这一档
            // 退成零宽占位，只有「没有标签行」时才需要它撑开。
            .child(if has_tabs {
                div().min_w_0()
            } else {
                div().flex_1().min_w_0()
            })
            // 字段筛选 / 跳转列已并入 tab 栏动作组（回滚之后）。
            // DDL 入口（原对象头条迁来）：只在表浏览且当前表取到了 DDL 时出现，
            // 与右栏 DDL 卡的「修改表结构」同一条 `open_structure_tab` 路径 ——
            // 右栏可能被折叠，这里是常驻入口。
            .when(
                self.mode == Mode::Table && self.table_ddl.is_some(),
                |bar| {
                    bar.child(
                        icon_text_button(
                            "db-result-ddl",
                            tr(cx, "修改表结构"),
                            Icon::new(DuckIcon::Pencil)
                                .size(extended.icon.sm)
                                .text_color(tokens.colors.muted_foreground),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = self.view.clone();
                            let target = self.selected_table.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    let Some(conn_id) = this.active.clone() else {
                                        return;
                                    };
                                    let Some(target) = target.clone() else {
                                        return;
                                    };
                                    this.open_structure_tab(&conn_id, target, false, window, cx);
                                });
                            }
                        }),
                    )
                },
            )
            .into_any_element()
    }

    /// 结果区脚注（设计稿 `grid-foot`）：视图三档切换 + 行数 / 耗时读数；右端
    /// 行数下拉与（表浏览的）翻页组。
    ///
    /// 全部是「这一次结果」的**常驻**事实——工具条中部那份 status 是易逝的操作
    /// 反馈，翻完页 / 切完标签就没了；这里跟结果共存亡。来源语句挪去了读数条
    /// （设计稿位置），脚注只留读数与动作。
    /// 表浏览工具条的「自动刷新」下拉（5 / 10 / 30 / 60 / 300 秒，或关闭）。
    ///
    /// 文案走「{}s 自动刷新」而不是只显示秒数：这排按钮全是动作，只写 `30s`
    /// 看不出它是开关还是读数。关闭态写「自动刷新」，一眼知道这里能开。
    pub(super) fn auto_refresh_dropdown(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let extended = theme.extended();
        let view = self.view.clone();
        let current = self.auto_refresh_secs;
        let label: SharedString = match current {
            Some(secs) => SharedString::from(trf(cx, "{}s 自动刷新", &[&secs.to_string()])),
            None => tr(cx, "自动刷新").into(),
        };
        toolbar_text_dropdown_button(
            "db-auto-refresh",
            label,
            Icon::new(DuckIcon::Clock).size(extended.icon.md),
            Anchor::TopRight,
            move |menu, _, cx| {
                let view_off = view.clone();
                let mut menu = menu.item(
                    PopupMenuItem::new(tr(cx, "关闭自动刷新"))
                        .checked(current.is_none())
                        .on_click(move |_, _, cx| {
                            let _ = view_off.update(cx, |this, cx| this.set_auto_refresh(None, cx));
                        }),
                );
                for secs in Self::AUTO_REFRESH_CHOICES {
                    let view_on = view.clone();
                    let item = SharedString::from(trf(cx, "{} 秒", &[&secs.to_string()]));
                    menu = menu.item(
                        PopupMenuItem::new(item)
                            .checked(current == Some(secs))
                            .on_click(move |_, _, cx| {
                                let _ = view_on.update(cx, |this, cx| {
                                    this.set_auto_refresh(Some(secs), cx);
                                });
                            }),
                    );
                }
                menu
            },
            cx,
        )
    }

    pub(super) fn render_result_footer(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let result = self.current_result();
        let rows = result.as_ref().map(|result| result.rows.len()).unwrap_or(0);
        let count = trf(cx, "{} 行", &[&rows.to_string()]);
        // 矩形选区的读数（dbx grid-foot 同款）：格子数，有数值就补合计 / 平均。
        // 只在真拖出选区时算 —— 一格都没有时这一段不占位。
        let selection_note: Option<SharedString> = self
            .active_cells_selection()
            .zip(result)
            .filter(|(range, _)| range.cell_count() > 1)
            .map(|(range, result)| {
                let columns = self.selection_columns(result, range);
                let stats = selection_stats(&result.rows, range, &columns);
                let cells = stats.cells.to_string();
                if stats.numeric == 0 {
                    // 全是文本 / NULL：只报格子数（合计 0 会让人以为数据是 0）。
                    trf(cx, "选中 {} 格", &[&cells]).into()
                } else {
                    let sum = format_number(stats.sum);
                    let average = format_number(stats.average);
                    trf(
                        cx,
                        "选中 {} 格 · 合计 {} · 平均 {}",
                        &[&cells, &sum, &average],
                    )
                    .into()
                }
            });

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
            .when_some(selection_note, |bar, note| bar.child(meta_text(note, cx)))
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
            // dbx grid-foot 同款的右端两件套：「N 行/页」下拉 → 紧凑翻页组
            // （« ‹ [页码] › »）。翻页组**常驻**（只有一页时按钮置灰，不再整组
            // 消失 —— dbx 同款，空一页也是页）。
            //
            // ⚠️ 「导出」**不再在这里出现**：tab 栏那份和这里那份用的是同一个元素
            // id（`db-export`），而同一个窗口里 ElementId 就是状态键 —— 两处共用
            // 一格状态，点一次会把两份弹层一起叫出来（用户实测「出现两个弹窗」）。
            // 同一个动作在一个窗口里只留一个入口：tab 栏那份与写动作组同排，
            // 离结果最近。
            .when(matches!(self.mode, Mode::Query | Mode::Table), |bar| {
                bar.child(match self.mode {
                    Mode::Query => self.query_page_size_dropdown(cx),
                    _ => self.browse_page_size_dropdown(cx),
                })
                .child(self.render_pager_compact(cx))
            })
            .into_any_element()
    }

    /// footer 的**紧凑翻页组**（dbx grid-foot 同款：`« ‹ [页码] › »`）。
    ///
    /// 两种模式共用一条版式：查询 = 客户端分页（总页数已知，五个钮都能判
    /// 置灰）；表浏览 = 服务器侧分页，总页数按**表级元信息的行数**估（带筛选
    /// 时不给，见 `Mode::Table` 臂）。「»」与「最后一页」只在总页数已知时判
    /// 置灰，未知时永远置灰 —— 装作知道再点空是骗人。**常驻**：只有一页时
    /// 按钮置灰但整条还在（dbx 同款，整组消失会让人找不到页码输入框）。
    /// 「跳至某页」回车即跳，见 [`Self::jump_to_page`]（两种模式同一份输入框）。
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
                    // 查询 = 客户端分页：改序号即刻生效（展示行序在组件那边，改完推一次）。
                    Mode::Query => {
                        this.query_page = target;
                        this.sync_data_table_rows(cx);
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
                    Mode::Instance(_)
                    | Mode::Dashboard(_)
                    | Mode::Structure(_)
                    | Mode::Er { .. } => {}
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
                // 总页数：表级元信息里的行数（SQLite 是精确 `COUNT(*)`；InnoDB / PG
                // 是统计估算，估算页数也比没有强 —— 用户要的是「还有多远到头」的量级）。
                // **带筛选时不给**：筛选后的行数服务器没报，拿全表行数凑总数是骗人。
                let pages = self
                    .table_filters
                    .is_empty()
                    .then_some(self.table_row_count)
                    .flatten()
                    .map(|count| count.value().div_ceil(page_size as u64).max(1) as usize);
                (self.table_page, pages, Some(can_next))
            }
            Mode::Instance(_) | Mode::Dashboard(_) | Mode::Structure(_) | Mode::Er { .. } => {
                (0, Some(1), Some(false))
            }
        };
        let can_prev = page > 0;
        let can_next = can_next.unwrap_or_else(|| pages.is_some_and(|pages| page + 1 < pages));
        let can_last = pages.is_some_and(|pages| page + 1 < pages);
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
            // 页码框 + 总页数（用户指定版式：`[页码] / N`；不再出「第 n / N 页」
            // 那条静态读数）。框里显示的是**当前页**，由 [`Self::sync_page_jump`]
            // 在渲染入口回写；键盘输入仍是「回车即跳」。
            //
            // 总页数：查询侧按结果整批 `div_ceil`；表浏览按**表级元信息的行数**估
            //（SQLite 精确、InnoDB/PG 统计估算；带筛选时拿不到筛选后的行数，不硬凑）。
            // 两边都拿不到（表行数没取回）时只留页码框。
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        div()
                            .id("db-page-jump")
                            .w(px(56.))
                            .flex_none()
                            // 同上：脚注挂在结果面板下，不拦住鼠标按下这一格就永远进不了焦点。
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                // 带框（`appearance(true)`）：这一格是**可编辑的页码**，
                                // 不画边框就只剩一个跟着内容浮动的数字，看不出能点。
                                Input::new(&self.page_jump)
                                    .appearance(true)
                                    .with_size(Size::Small)
                                    .text_size(tokens.typography.sm.size)
                                    .w_full(),
                            ),
                    )
                    .children(pages.map(|pages| {
                        div()
                            .flex_none()
                            .line_height(tokens.typography.sm.line_height)
                            .text_size(tokens.typography.sm.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(format!("/{pages}")))
                    })),
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
            .into_any_element()
    }

    /// 当前展示的页码（**0 起**）。
    ///
    /// 查询侧要按总页数夹一下：`query_page` 可能落在结果变短之后的界外（重跑
    /// 一条返回更少行的语句、改页大小），而脚注画的是夹住之后的那一页。
    /// 表浏览是服务器侧分页，页码就是本地那一个。「实例 / 仪表板」没有页的概念。
    fn current_page(&self, cx: &App) -> usize {
        match self.mode {
            Mode::Query => {
                let total = self.current_result().map_or(0, |result| result.rows.len());
                let size = self.query_page_size(cx).max(1);
                let pages = total.div_ceil(size).max(1);
                self.query_page.min(pages - 1)
            }
            Mode::Table => self.table_page,
            Mode::Instance(_) | Mode::Dashboard(_) | Mode::Structure(_) | Mode::Er { .. } => 0,
        }
    }

    /// 把页码框回写成**当前页**（受控显示：页码是状态，框只是它的显示）。
    ///
    /// 为什么在这里统一写：改页码的路径有十几处（翻页 / 跳页 / 搜索重排 / 换表 /
    /// 改页大小 / 换结果标签），其中大半拿不到 `Window`，而写输入框必须给窗口 ——
    /// 所以收在**渲染入口**同步一次。`set_value` 不发 `InputEvent::Change`，本页也
    /// 没有 observe 这个输入框，因此这一写不会反过来触发本页重绘（值相同时直接
    /// 跳过，稳态下零开销）。
    ///
    /// ⚠️ 输入框**聚焦时跳过**：用户正在打字，把值按回去会把输入吞掉；那会儿框里
    /// 的文本本来就是用户意图，回车走 [`Self::jump_to_page`] 自己回写。
    pub(super) fn sync_page_jump(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page_jump.focus_handle(cx).is_focused(window) {
            return;
        }
        self.write_page_jump(window, cx);
    }

    /// 无条件把页码框写成当前页（跳过「聚焦则不动」那条守卫）。
    ///
    /// 给 [`Self::jump_to_page`] 用：按下回车时输入框必然聚焦，但此时要的是
    /// **真实**页码（输入越界时显示的是被夹住的那一页，与脚注一致）。
    pub(super) fn write_page_jump(&self, window: &mut Window, cx: &mut Context<Self>) {
        let text = (self.current_page(cx) + 1).to_string();
        if self.page_jump.read(cx).value().as_ref() == text {
            return;
        }
        self.page_jump
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    /// 结果视图四档切换（网格 / 记录 / JSON / 图表），落在结果 footer **最前面**。
    ///
    /// 版式照结果区顶部 tab 栏（dbflux 底部切换同款）：图标 + 文字、12pt 小字
    /// （比普通按钮的 13pt 小一档）、选中档 `nav_selected` 底 + 正文色，其余
    /// `muted_foreground`、悬停 `hover_tint`。
    ///
    /// 前三档换的是「同一份结果怎么读」：网格 = 常规网格（默认），记录 = 选中行的
    /// 转置视图（名称 / 值 / 类型），JSON = 该行序列化。三档共用 `result_view`。
    ///
    /// **图表不是第四种读法**：它联动结果区顶部 tab（[`ResultAreaTab::Chart`]）——
    /// 选中态按 `result_area_tab` 判定，点它 = `set_result_area_tab(Chart)`；点前三档
    /// 除了换 `result_view` 还要把顶部 tab 切回**数据表**，否则「点了网格、画面还是图表」。
    pub(super) fn result_view_switch(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let current = self.result_view;
        let in_chart = self.result_area_tab == ResultAreaTab::Chart;
        let has_result = self.current_result().is_some();
        let hover_tint = extended.colors.hover_tint;
        let disabled_opacity = extended.opacity.disabled;

        /// 一档点了之后去哪。
        #[derive(Clone, Copy)]
        enum Target {
            /// 换读法：切数据表 tab + 换 `result_view`。
            Mode(ResultViewMode),
            /// 图表：切结果区顶部 tab（图表的配置 / 画法与网格无关）。
            Chart,
        }

        let item = |id: &'static str,
                    label: &'static str,
                    icon: AnyElement,
                    active: bool,
                    enabled: bool,
                    target: Target| {
            let mut el = div()
                .id(SharedString::from(id))
                .h(theme.density().control)
                .px(tokens.spacing.sm)
                .flex()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.xxs)
                .rounded(tokens.radius.sm)
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
                .child(icon)
                .child(label);
            if enabled {
                let view = view.clone();
                el = el
                    .cursor_pointer()
                    .role(Role::Button)
                    .aria_label(label)
                    .hover(move |style| style.bg(hover_tint))
                    .on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| match target {
                            Target::Chart => this.set_result_area_tab(ResultAreaTab::Chart, cx),
                            Target::Mode(mode) => {
                                this.result_view = mode;
                                // 从图表 / 摘要等 tab 点回来时顶部 tab 也得跟着回数据表；
                                // 已在数据表时 `set_result_area_tab` 直接 return（不 notify）。
                                this.set_result_area_tab(ResultAreaTab::Data, cx);
                                cx.notify();
                            }
                        });
                    });
            } else {
                // 没有结果可画时「图表」置灰但**留在原位**（与顶部 tab 同一口径），
                // 工具条布局不跳，用户也看得到还有哪些视图。
                el = el.opacity(disabled_opacity);
            }
            el.into_any_element()
        };

        // JSON 档的「图标」就是字面的 `{}`（dbflux 同款，不另画 SVG）：与图标同档字号
        // （`icon.sm` = 12 = `typography.xs`），四档的图标槽宽度才不会一档一个样。
        let braces = div()
            .text_size(tokens.typography.xs.size)
            .line_height(tokens.typography.xs.line_height)
            .font_weight(FontWeight::MEDIUM)
            .child("{}")
            .into_any_element();

        h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .child(item(
                "db-result-view-grid",
                tr(cx, "网格"),
                Icon::new(DuckIcon::Table)
                    .size(extended.icon.sm)
                    .into_any_element(),
                !in_chart && current == ResultViewMode::Grid,
                true,
                Target::Mode(ResultViewMode::Grid),
            ))
            .child(item(
                "db-result-view-columns",
                tr(cx, "记录"),
                Icon::new(DuckIcon::Rows3)
                    .size(extended.icon.sm)
                    .into_any_element(),
                !in_chart && current == ResultViewMode::Columns,
                true,
                Target::Mode(ResultViewMode::Columns),
            ))
            .child(item(
                "db-result-view-json",
                "JSON",
                braces,
                !in_chart && current == ResultViewMode::Json,
                true,
                Target::Mode(ResultViewMode::Json),
            ))
            .child(item(
                "db-result-view-chart",
                tr(cx, "图表"),
                Icon::new(DuckIcon::ChartLine)
                    .size(extended.icon.sm)
                    .into_any_element(),
                in_chart,
                has_result,
                Target::Chart,
            ))
            .into_any_element()
    }

    /// 当前行（列 / JSON 视图看的那一行）：有选中行用选中行，否则用首行。
    ///
    /// 越界一律回落首行——「翻到第 3 页后结果变短」这种情形下，`selected_row`
    /// 可能指向已不存在的行，落到空态比 panic 好。
    pub(super) fn current_row<'a>(&self, result: &'a ResultSet) -> Option<&'a [Value]> {
        let index = self.current_row_index(result.rows.len())?;
        result.rows.get(index).map(Vec::as_slice)
    }

    /// 当前行的**下标**（与 [`Self::current_row`] 是同一判据的两种取用方式）。
    ///
    /// JSON 档要按下标去建编辑表单（[`Self::sync_json_editor`]），只拿切片不够；
    /// 判据写在**一处** —— 越界回落首行、空结果 `None`，两处口径不会漂。
    pub(super) fn current_row_index(&self, rows: usize) -> Option<usize> {
        if rows == 0 {
            return None;
        }
        Some(self.selected_row.filter(|index| *index < rows).unwrap_or(0))
    }

    /// 切换某一列的显示 / 隐藏（结果区「字段筛选」）。
    ///
    /// 藏列同时影响**两处**：组件那边要重排列头与行（`set_hidden_names`），页面这边
    /// 要按新的可见列重算搜索命中集 —— 两件事都收在 [`Self::sync_data_table_rows`] 里。
    pub(super) fn toggle_column_hidden(&mut self, column: &str, cx: &mut Context<Self>) {
        if self.hidden_columns.remove(column) {
            self.sync_data_table_rows(cx);
            self.store_grid_layout(cx);
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
        self.sync_data_table_rows(cx);
        self.store_grid_layout(cx);
        cx.notify();
    }

    /// 把结果网格横向滚到某一列（结果区「跳转列」）。
    ///
    /// 横向偏移 = 行首两根固定柱（行号 + 多选）+ 它前面各列的宽度之和 —— 前缀和与
    /// 滚动**都归组件**（[`DataTableState::scroll_to_column`] 用的是同一份列宽，
    /// 不会与列头实际布局漂移）。纵向不动：这块区域只允许横向滚动。
    pub(super) fn jump_to_column(&mut self, index: usize, cx: &mut Context<Self>) {
        // 借用先结束再写 `self`：`current_result()` 借的是 `&self`，与后面的
        // `data_table.update` 不能重叠（虽然那只是 `&self`，但列名要活着带走）。
        let name = {
            let Some(result) = self.current_result() else {
                return;
            };
            match result.columns.get(index) {
                Some(column) => column.name.clone(),
                None => return,
            }
        };
        self.data_table
            .update(cx, |state, cx| state.scroll_to_column(index, cx));
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
                        let _ = view.update(cx, |this, cx| this.show_all_columns(cx));
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
    ///
    /// 清空走的是 `set_value`，那是输入框自己的事件 —— 页面这条订阅里
    /// `InputEvent::Change` 会把展示行序重推一遍（见 `shell.rs` 的 `result_search`
    /// 订阅），所以这里不必也不该再推一次（推两次会白算两遍命中集）。
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
    /// 搜索是**客户端**的：命中集只搜可见列，算出来的是展示行序的输入 ——
    /// 输入框变字由订阅推一次 [`Self::sync_data_table_rows`]，不在渲染路径上现算。
    /// 有词时在框尾给一枚清除按钮——搜索词藏在小框里，没有一键清除就得全选删除。
    pub(super) fn result_search_box(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let has_query = !self.result_search_text(cx).is_empty();
        h_flex()
            .id("db-result-search")
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            // ⚠️ 整块必须拦住鼠标按下：结果面板外层挂了 `track_focus(grid_focus)` +
            // `on_mouse_down → focus(网格)`（「聚焦的网格拥有剪贴板键」那条）。不拦的话
            // 输入框刚拿到焦点就被父级抢回网格 —— 表现就是「搜索框点不动、也搜不了」。
            // 同一份修法见 `render_pager_compact` / 批量条 / 过滤条的输入框。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
                                .child(selectable_text(SharedString::from(trf(
                                    cx,
                                    "执行失败：{}",
                                    &[&message],
                                )))),
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
                        .child(selectable_text(SharedString::from(sql_line))),
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
                .on_drag(ColumnsModeResizeDrag { col }, |drag, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| drag.clone())
                })
                .on_drag_move::<ColumnsModeResizeDrag>({
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
        //
        // 两层滚动：**外层**横向滚（表头与行区都是它的子项，天然共享同一份横向
        // 偏移，不会错位），**内层**只有行区纵向滚 —— 表头固定不动（行模式网格
        // 同款语义）。`restrict_scroll_to_axis` 锁轴：竖向滚轮归行区、横向手势归
        // 外层，互不串扰（网格横向滚动区同款）。
        let mut rows_list = v_flex()
            .id("db-result-columns")
            .flex_1()
            .min_h_0()
            // 宽度钉成内容总宽：行区与表头同宽，外层按子项边界算可滚距离。
            .w(px(total_width))
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            // 纵向滚动条从这里读位置（句柄跨帧持久，见字段注释）。
            .track_scroll(&self.result_columns_y_scroll);
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
            let submenu_label = tr(cx, "复制为");
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
                        .child(selectable_text(SharedString::from(cell_value.clone())))
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
                        entity.update(cx, |this, cx| {
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
            rows_list = rows_list.child(
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
                            .child(selectable_text(SharedString::from(column.name.clone()))),
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
                            .child(selectable_text(SharedString::from(
                                column.type_name.clone(),
                            ))),
                    ),
            );
        }
        // 外层横向滚动区：装下「表头 + 行区」，两者同一份横向偏移；表头 `flex_none`
        // 不参与纵向滚动，行区在内层自己滚自己的。
        //
        // ⚠️ gpui 的滚动容器不画系统滚动条，必须自己叠 `Scrollbar`（字段网格 /
        // data_table 同款形态），否则超宽 / 超高的内容没有可拖动的东西。句柄由
        // 页面状态持有（跨帧持久），absolute 兄弟节点叠加、外壳承托。
        // ⚠️ 外壳必须是 **flex 容器**（`v_flex`）：块级 `div()` 不给子元素确定高度，
        // 内层 x 滚动区的 `flex_1` 失效 ⇒ 行区纵向 `overflow_y_scroll` 永不触发。
        v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .relative()
            .child(
                v_flex()
                    .id("db-result-columns-x")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.result_columns_x_scroll)
                    .child(header)
                    .child(rows_list),
            )
            // 横向常驻（无溢出时 Scrollbar 自己隐藏）；`viewport_from_layout`
            // 让轨道按内容实际占用区域计算。
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::horizontal(&self.result_columns_x_scroll)
                        .id("db-result-columns-x-scrollbar")
                        .mode(ScrollbarMode::Always)
                        .viewport_from_layout(),
                ),
            )
            // 纵向滚动时现身、停下淡出（行区同档）。
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::vertical(&self.result_columns_y_scroll)
                        .id("db-result-columns-y-scrollbar")
                        .mode(ScrollbarMode::Scrolling),
                ),
            )
            .into_any_element()
    }

    /// JSON 模式：把当前行铺成一份**可直接编辑**的 JSON 代码编辑器。
    ///
    /// 复用内核的 [`ResultSet::to_json_pretty`]（与导出 JSON 同一套序列化口径）做
    /// 编辑器初值——不另写一份「值 → JSON」的手工拼接，避免两处口径漂移。高亮、
    /// 行号、软换行由编辑器内建（Redis 值编辑器同款，见 [`JsonEditor`]）；
    /// 改动经「暂存更改」进与网格 / 右栏同一条暂存路径。
    ///
    /// ⚠️ 旧版「字段表单 + 只读预览」两层已收成这一层：嵌套结构 / 数组的真实形状
    /// 只有用 JSON 表达得清楚，能看清就能直接改。
    pub(super) fn render_result_json(
        &self,
        // 内容与可编辑性都由渲染入口同步好的 [`Self::json_editor`] 承载（它按
        // `current_result()` 取数，与此处传入的结果集是同一份）；参数留着保持
        // 与 `render_result_grid` / `render_result_columns` 同一档位签名。
        _result: &ResultSet,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let Some(form) = self.json_editor.as_ref() else {
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
        let view = self.view.clone();
        let row = form.row;
        let editable = form.editable;
        // 标题行：档位名 + 两个动作（暂存 / 删行）收在右端，编辑器占满剩余空间。
        let header_row = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .child(caption_heading(tr(cx, "JSON 预览"), cx))
            .child(
                h_flex()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(
                        button(
                            "db-json-save",
                            tr(cx, "暂存更改"),
                            ButtonVariant::Primary,
                            cx,
                        )
                        .disabled(self.busy || !editable)
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ =
                                    view.update(cx, |this, cx| this.stage_json_edits(window, cx));
                            }
                        }),
                    )
                    .child(
                        button(
                            "db-json-delete",
                            tr(cx, "删除行"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(self.busy || !editable)
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.delete_current_row(row, window, cx);
                                });
                            }
                        }),
                    ),
            );
        v_flex()
            .id("db-result-json")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .p(tokens.spacing.sm)
            .gap(tokens.spacing.xs)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(header_row)
            // ⚠️ 编辑器必须挂在 **flex 容器**下（`v_flex`）：包一层 `display:block`
            // 的 div 它的 `flex_1` 量不到高度（Redis 页同名注释的坑）。
            .child(
                v_flex().w_full().flex_1().min_h_0().min_w_0().child(
                    Editor::new(&form.input)
                        .appearance(false)
                        .bordered(false)
                        // 只读不是灰掉：外观不变、照常选中复制，只是拒绝改动。
                        .readonly(!editable)
                        .flex_1()
                        .min_h_0()
                        .text_size(tokens.typography.mono_md.size),
                ),
            )
            .child(meta_text(
                tr(cx, "改动先暂存，点结果区上方的「提交」才落库。"),
                cx,
            ))
            .into_any_element()
    }

    /// 让 JSON 档的编辑器跟上「现在显示的是哪一行」。
    ///
    /// 在**渲染期**做（`Render::render` 的头一行）：构造 `EditorState` 要
    /// `&mut Window`，而渲染是页面唯一同时握着窗口与 `&mut self` 的时机（双击开行
    /// 编辑器是同一个理由）。这里只改页面自己的字段、**不 notify** —— 渲染中发
    /// 通知就是死循环。
    ///
    /// 只有指纹对不上（[`specs_stamp`]）才整份重建：行没换就原样留着，用户正在
    /// 编辑器里敲的字不会被下一帧抹掉。
    pub(super) fn sync_json_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 只有 JSON 档要这份编辑器；切走时留着（切回来接着改），但**不再同步**。
        if self.result_view != ResultViewMode::Json {
            return;
        }
        let Some(rows) = self.current_result().map(|result| result.rows.len()) else {
            self.json_editor = None;
            return;
        };
        // 空结果没有可显示的行：编辑器跟着消失，由 `render_result_json` 的空态如实说明。
        let Some(row) = self.current_row_index(rows) else {
            self.json_editor = None;
            return;
        };
        // 可编辑与否不影响编辑器本身（只读时照常选中复制），specs 拿不到就空着
        // ——暂存路径自己还有「结果集可编辑」的兜底。
        let specs = self.row_editor_specs(row).unwrap_or_default();
        let stamp = specs_stamp(&specs);
        let editable = self.edit_source().is_some();
        if self
            .json_editor
            .as_ref()
            .is_some_and(|form| form.stamp == stamp)
        {
            return;
        }
        // 编辑器初值 = 当前行（裁掉藏列）的美化 JSON，与旧只读预览同一序列化口径。
        // 按**原始**下标同时裁列与裁值，否则列名与值会错位半格（JSON 是「名字对值」
        // 的映射，错位比少一列严重得多）。
        let text = self
            .current_result()
            .and_then(|result| {
                let values = self.current_row(result)?;
                let mut columns = Vec::new();
                let mut row_values = Vec::new();
                for (index, column) in result.columns.iter().enumerate() {
                    if self.hidden_columns.contains(&column.name) {
                        continue;
                    }
                    columns.push(column.clone());
                    if let Some(value) = values.get(index) {
                        row_values.push(value.clone());
                    }
                }
                Some(
                    ResultSet {
                        columns,
                        rows: vec![row_values],
                        truncated: false,
                    }
                    .to_json_pretty(),
                )
            })
            .unwrap_or_default();
        let input = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(true)
                .soft_wrap(true)
        });
        input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
        });
        self.json_editor = Some(JsonEditor {
            row,
            stamp,
            editable,
            specs,
            input,
        });
    }

    /// 导出结果下拉（五档：TSV / CSV / Markdown / JSON / INSERT）+「导入 CSV…」。
    ///
    /// 档位清单与结果区右键的「复制为 …」同源（[`ResultFormat::ALL`]），这里只是
    /// 多一层落盘文件名与保存对话框。「导出 INSERT」只对 SQL 引擎开放：MongoDB /
    /// Elasticsearch 没有带引号的表名，导不出能直接执行的语句。
    ///
    /// 「导入 CSV」**并进这个下拉**，不再单独占一枚工具条图标：它与导出共用同一批
    /// 文件格式，摆在一起是「进出这份结果的文件口子」；单独立一枚图标时它藏在最右端，
    /// 与相邻的图表切换按钮没有任何关系，反而看不出是干嘛的（需求原话：最右侧那枚
    /// 导入图标去掉）。可用性判据不变：结果集可编辑（单表 + 主键）且当前不忙。
    pub(super) fn export_dropdown(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let extended = theme.extended();
        let view = self.view.clone();
        let has_result = self.current_result().is_some();
        let can_insert = self.insert_context().is_some();
        let has_raw = self
            .current_result()
            .is_some_and(|result| result.raw_bytes().is_some());
        let import_ready = self.edit_source().is_some() && !self.busy;
        // 菜单构建闭包是 `Fn` 且 `'static`：文案必须在外面按当前语言取好（`tr`
        // 要 `&App`，闭包里拿不到），所以这里先结成「档位 + 文案」对——
        // 一份清单派生一对，不会像手写文案数组那样加档错位。
        let export_items: Vec<(ResultFormat, &'static str)> = ResultFormat::EXPORT_ALL
            .iter()
            .map(|format| (*format, format.export_label(cx)))
            .collect();
        let import_label = tr(cx, "导入 CSV…");
        // 导出范围（[`ExportScope`]）：与格式正交的第二根轴。摆成子菜单而不是在
        // 每个格式后面再乘一遍——10 档格式 × 4 档范围会变成 40 项。
        let scope = self.export_scope;
        let table_mode = self.mode == Mode::Table;
        let has_selected = !self.checked_rows.is_empty() || self.selected_row.is_some();
        let has_selection = self.active_cells_selection().is_some();
        let scope_items: Vec<(ExportScope, &'static str)> = [
            ExportScope::Page,
            ExportScope::Selected,
            ExportScope::Selection,
            ExportScope::WholeTable,
        ]
        .iter()
        .map(|choice| (*choice, choice.label(cx)))
        .collect();
        let scope_label: SharedString = trf(cx, "范围：{}", &[scope.label(cx)]).into();
        // 文字按钮（设计稿「⤓ 导出 ∨」）：纯图标版让人猜这个下载箭头是干嘛的。
        let title = tr(cx, "导出 / 导入");
        toolbar_text_dropdown_button(
            "db-export",
            title,
            Icon::new(DuckIcon::Download).size(extended.icon.sm),
            Anchor::TopRight,
            move |menu, _, _| {
                // 范围四档平铺在顶部（勾选态 = 当前档），再做一节分隔：菜单里
                // 第 1 节是「装哪些行」，第 2 节才是「装成什么格式」。首项是禁用的
                // 分区标题，写清当前范围——不然只看勾选态得自己数。
                let menu = menu.item(PopupMenuItem::new(scope_label.clone()).disabled(true));
                let menu = scope_items.iter().fold(menu, |menu, item| {
                    let range_view = view.clone();
                    let (choice, label) = *item;
                    menu.item(
                        PopupMenuItem::new(label)
                            .checked(choice == scope)
                            .disabled(!choice.available(table_mode, has_selected, has_selection))
                            .on_click(move |_, _, cx| {
                                let _ = range_view.update(cx, |this, cx| {
                                    this.export_scope = choice;
                                    cx.notify();
                                });
                            }),
                    )
                });
                let menu = export_items
                    .iter()
                    .fold(menu.separator(), |menu, (format, label)| {
                        let enabled = format.exportable(has_result, can_insert, has_raw);
                        menu.item(PopupMenuItem::new(*label).disabled(!enabled).on_click({
                            let view = view.clone();
                            let format = *format;
                            move |_, window, cx| {
                                let _ = view
                                    .update(cx, |this, cx| this.export_result(format, window, cx));
                            }
                        }))
                    });
                let import_view = view.clone();
                menu.separator().item(
                    PopupMenuItem::new(import_label)
                        .disabled(!import_ready)
                        .on_click(move |_, window, cx| {
                            let _ = import_view.update(cx, |this, cx| this.import_csv(window, cx));
                        }),
                )
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
            // 「构建器」**不是结果区的档位**（它开在右栏，结果照看在眼里），所以不并进
            // `ResultAreaTab`；但它是这一排里的入口，版式照抄 tab 格，另加一道竖分隔线
            // 把它与「切结果视图」那几个 tab 分开。选中态 = 右栏开着**且**正停在构建器
            // 那一栏：亮着＝现在看的就是它，与 tab 的选中语义一致。
            .child(
                div()
                    .flex_none()
                    .w(px(1.))
                    .h(tokens.spacing.lg)
                    .bg(extended.colors.hairline),
            )
            .child({
                let active = self.right_dock == RightDock::Builder
                    && crate::app::right_sidebar_open(cx).unwrap_or(false);
                let builder_view = view.clone();
                div()
                    .id("db-area-tab-builder")
                    .flex_none()
                    .h(theme.density().control)
                    .pl(tokens.spacing.sm)
                    .pr(tokens.spacing.xs)
                    .flex()
                    .items_center()
                    .gap(tokens.spacing.xxs)
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
                    .role(Role::Button)
                    .aria_label(tr(cx, "构建器"))
                    .on_click(move |_, window, cx| {
                        let _ = builder_view.update(cx, |this, cx| this.open_builder(window, cx));
                    })
                    .child(Icon::new(DuckIcon::SlidersHorizontal).size(extended.icon.sm))
                    .child(tr(cx, "构建器"))
            })
            .child(div().flex_1().min_w_0())
            // 结果内搜索（自读数条上移）：找行是「看着结果」时的动作，跟档位与写
            // 动作同一排；读数条只留事实读数。词不进快照，仍是临时态。
            .child(self.result_search_box(cx))
            .child(refresh)
            // 自动刷新只对表浏览出现：查询结果没有「重新取一页」——
            // 定时重跑语句会改结果集、冲掉撤销栈，是用户没要求过的行为。
            .when(self.mode == Mode::Table, |bar| {
                bar.child(self.auto_refresh_dropdown(cx))
            })
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
            // 同上：flex 容器，子项的 `flex_1` 才生效（摘要 / 消息 / 图表正文）。
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(content),
            );
        if with_footer {
            panel = panel.child(self.render_result_footer(cx));
        }
        panel.into_any_element()
    }

    // 列宽的写回 / 自适应 / 拖拽（原先的 `set_column_width` / `auto_fit_column` /
    // `begin_col_resize` / `drag_col_resize` / `end_col_resize`）已整体搬进公共数据表
    // 组件（`DataTableState::set_width` / `begin_resize` / `drag_resize` / `end_resize`）。
    // 页面不再持有列宽 —— 组件在**拖拽收尾**时经 `DataTableEvent::ColumnResized` 报
    // 最终宽度（宿主将来要落盘时接这个事件即可）。

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
        let list =
            uniform_list("db-summary-cols", list_columns.len(), move |range, _, _| {
                range
                    .map(|index| {
                        let column = &list_columns[index];
                        let nulls = null_counts[index];
                        h_flex()
                            .id(SharedString::from(format!("db-summary-col-{index}")))
                            .h(px(GRID_ROW_HEIGHT))
                            // uniform_list 的行是 taffy 根节点：flex 根不会自动撑满可用
                            // 宽度（只有 block 根会），不写 w_full 行就收缩成内容宽，
                            // 固定列宽（类型 150 / 空值 90）与 `flex_1` 全部失效。
                            .w_full()
                            .px(row_pad)
                            .items_center()
                            .gap(col_gap)
                            .border_b(thin)
                            .border_color(hairline)
                            .text_size(cell_text)
                            .child(
                                div().flex_1().min_w_0().truncate().text_color(fg).child(
                                    selectable_text(SharedString::from(column.name.clone())),
                                ),
                            )
                            .child(
                                div()
                                    .w(px(150.))
                                    .flex_none()
                                    .truncate()
                                    .text_color(muted)
                                    .child(selectable_text(SharedString::from(
                                        column.type_name.clone(),
                                    ))),
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
                        // 同 `db-summary-cols`：taffy flex 根不自动撑满，缺 w_full
                        // 会让右侧耗时列跟着内容跑。
                        .w_full()
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
                                .child(selectable_text(SharedString::from(entry.label.clone()))),
                        )
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(360.))
                                .truncate()
                                .text_color(muted)
                                .child(selectable_text(SharedString::from(entry.detail.clone()))),
                        )
                        .child(div().flex_none().text_color(muted).child(selectable_text(
                            SharedString::from(format!("{} ms", entry.elapsed_ms)),
                        )))
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
            // ⚠️ 这层包裹必须是 **flex 容器**（`div()` 默认 Block）：里面的滚动容器
            // （列模式 / JSON）靠 `flex_1` 吃满剩余高度，Block 下它不生效 —— 内容
            // 有多高容器就有多高，永远不溢出 = 永远不滚动，行还会直接画过脚注。
            panel = panel.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(content),
            );
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

        let truncated = result.truncated;
        // 网格主体（列切片 + 列头 / 列宽把手 + 行列表）已单独成方法 `render_result_grid`：
        // 本函数只剩「守卫 + 面板拼装」两段。
        let content = self.render_result_grid(result, cx);

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
        // 列宽之和经常超过视口（宽表 / 窄窗口 / 多列结果集）：表头与行区装进**同一个**
        // 横向滚动容器、共用一份横向偏移，滚动时不会错位。
        //
        // ⚠️ 这一层（横向滚动容器 + 常驻横向滚动条 + 滚动态纵向滚动条 + 固定列头 +
        // 行虚拟化）现在**整体由公共数据表组件提供**（`duck_components::data_table`），
        // 页面只要把 `content` 交过去 —— 再包一层就是两层横向滚动，滚动条会打架，
        // 表头也会跟着滚走。过滤条 / 编辑条 / 批量条 / 翻页条仍留在滚动区外（固定，
        // 同主流数据库客户端），见下面几行。
        panel = panel.child(content);
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
        // 的却是外层的「复制为 TSV」，整页结果进了剪贴板）。「复制为」已并入单元格
        // 菜单（`ResultFormat::Tsv` 一档），其余格式走脚注「导出」落盘。
        panel.into_any_element()
    }

    /// 当前**有效**的矩形选区：越界就当没有（结果集换了、行变少、翻页之后）。
    ///
    /// 选区存的是**当前结果集里的下标**，换结果集 / 翻页都可能让旧下标指向别的
    /// 数据。渲染、底栏读数、选区复制**一律经这里取**，三处口径才不会各走各的
    ///（否则会出现「格子没高亮，但底栏还在报合计」这种自相矛盾的画面）。
    pub(super) fn active_cells_selection(&self) -> Option<CellRange> {
        let range = self.cells_selection?;
        let result = self.current_result()?;
        let (_, row_end, _, col_end) = range.bounds();
        (row_end < result.rows.len() && col_end < result.columns.len()).then_some(range)
    }

    /// 选区统计 / 复制要用的列：**可见列里落在选区列区间内的那些下标**。
    ///
    /// 隐藏列不参与：用户看不见它，却让「N 格」多出两格、合计里多出两列数，
    /// 只会让人以为读数算错了。
    pub(super) fn selection_columns(&self, result: &ResultSet, range: CellRange) -> Vec<usize> {
        let (_, _, col_start, col_end) = range.bounds();
        visible_column_indexes(&result.columns, &self.hidden_columns)
            .into_iter()
            .filter(|col| (col_start..=col_end).contains(col))
            .collect()
    }

    /// 右键列头调列序：交给组件改展示顺序（它自己会报 `ColumnOrderChanged`，
    /// 这里再顺手落一次盘 —— 事件是异步回来的，但 `store_grid_layout` 幂等）。
    fn reorder_column(&mut self, col: usize, nudge: ColumnNudge, cx: &mut Context<Self>) {
        self.data_table.update(cx, |state, cx| match nudge {
            ColumnNudge::Left => state.nudge_column(col, -1, cx),
            ColumnNudge::Right => state.nudge_column(col, 1, cx),
            ColumnNudge::First => state.send_column_to_edge(col, false, cx),
            ColumnNudge::Last => state.send_column_to_edge(col, true, cx),
        });
    }

    /// 只有一个可见列时列序无从可调（菜单据此置灰）。
    fn has_multiple_columns(&self, cx: &App) -> bool {
        self.data_table.read(cx).visible_columns().nth(1).is_some()
    }

    /// 结果网格主体：把宿主的展示态交给**公共数据表组件**
    ///（`duck_components::data_table`）。
    ///
    /// 这里只组装三样东西就返回 —— 几何、虚拟化、两条滚动条、固定列头都在组件里：
    ///
    /// | 传入 | 内容 |
    /// | --- | --- |
    /// | [`DataTableView`] | 宿主真源：选中行 / 勾选集 / 排序态 / 就地编辑器 / 空态提示 |
    /// | [`DataTableLabels`] | 界面文案（组件自身不含文案，`tr` 的结果在这里注入） |
    /// | [`DataTableHooks`] | 只有「要窗口的交互」走闭包：右键菜单内容、双击开编辑器 |
    ///
    /// 表模型 / 列宽 / 藏列 / 展示行序早就在 [`Self::sync_data_table`] 里推给组件了
    ///（结果、搜索、分页、藏列一变各推一次），所以这条渲染路径上**没有任何逐帧计算**
    /// —— 这正是「行再多也不卡」的口径：每帧只碰可见的几十行。
    pub(super) fn render_result_grid(&self, result: &ResultSet, cx: &Context<Self>) -> AnyElement {
        // 组件里的模型必须正是这一次要画的结果：不同步 = 画面与数据对不上。
        // 开发期约束，不是用户可见文案（panic 消息写英文）。
        debug_assert!(
            self.current_result()
                .is_some_and(|current| std::ptr::eq(current, result)),
            "data table component was not synced to the result being rendered"
        );
        // 本帧要用的宿主真源：都是小值 / 借用（`'static` 那份由组件自己拷）。
        let sort = self.active_sort();
        let editing = self.cell_editor.as_ref();
        let view = DataTableView {
            selected_row: self.selected_row,
            checked: Some(&self.checked_rows),
            // 两种模式都可点列头排序，但**排的地方不同**：表浏览是服务器侧
            // `ORDER BY`（重取一页、数据真的换了序），SQL 结果集是客户端重排
            //（结果整批在手，只改展示顺序，不动语句、不重新查库）。
            // 后者不会与用户语句里的 `ORDER BY` 打架：界面排的是「拿到手的这批行」，
            // 用户写法决定的仍是「取到哪批行、按什么顺序取」。
            sortable: matches!(self.mode, Mode::Table | Mode::Query),
            sort_column: sort.map(|spec| spec.column.as_str()),
            sort_descending: sort.is_some_and(|spec| spec.descending),
            editing: editing.map(|editor| CellPos::new(editor.row, editor.col)),
            editing_input: editing.map(|editor| editor.input.clone()),
            selection: self.active_cells_selection(),
            empty_hint: self.result_empty_hint(cx),
        };
        let labels = self.data_table_labels(cx);
        let hooks = self.data_table_hooks();
        data_table(
            "db-result-grid",
            &self.data_table,
            &view,
            &labels,
            &hooks,
            cx,
        )
    }

    /// 「搜了个词却一行没中」的提示文案。
    ///
    /// 组件只在**展示行序为空**时渲染它，所以这里只要如实回答「有没有在搜」——
    /// 绝不在渲染路径上重算命中集（那是 O(行 × 列)，正是要消灭的那种逐帧开销）。
    fn result_empty_hint(&self, cx: &App) -> Option<SharedString> {
        let text = self.result_search_text(cx);
        (!text.is_empty()).then(|| trf(cx, "没有命中「{}」的行。", &[&text]).into())
    }

    /// 组件里的用户可见文案。
    ///
    /// `duck-components` 的口径是**组件不含任何界面文案**（i18n 守卫扫得到），所以
    /// `tr` 留在宿主；动态部分用 `{}` 模板交进去，组件按出现顺序填 —— 行勾选框的名字
    /// 就是「第 {} 行」+ 行号（这比原来自拼的 `format!` 强：那个串永远只有中文）。
    fn data_table_labels(&self, cx: &App) -> DataTableLabels {
        DataTableLabels::new(
            tr(cx, "勾选全部行"),
            tr(cx, "第 {} 行"),
            tr(cx, "按 {} 排序"),
            tr(cx, "提交"),
            tr(cx, "单元格详情"),
            tr(cx, "{} 的列操作"),
            tr(cx, "列名"),
            tr(cx, "类型"),
            tr(cx, "注释"),
            tr(cx, "复制详情"),
            // 详情卡复制出来的三行里标签与值之间的分隔符：中文全角冒号、英文
            // 冒号加空格 —— 是文案不是标点常量，所以在这儿注入。
            tr(cx, "："),
        )
    }

    /// 只有「**要窗口**的交互」才走闭包。
    ///
    /// 事件订阅只给 `&mut Context<Self>`（没有窗口），而两件事离不开窗口：
    ///
    /// 1. **右键菜单内容** —— gpui-component 的菜单是声明式的（`.context_menu()` 挂在
    ///    元素上、构建闭包在弹出时才跑），没有「弹出时再问宿主」的机会；
    /// 2. **双击开就地编辑 / 行编辑器** —— `InputState::new` 必须要 `&mut Window`。
    ///
    /// 其余交互（选中 / 勾选 / 排序 / 提交 / 详情）都走 [`DataTableEvent`]。
    fn data_table_hooks(&self) -> DataTableHooks {
        let menu_view = self.view.clone();
        let cell_dbl_view = self.view.clone();
        let row_dbl_view = self.view.clone();
        DataTableHooks {
            // 单元格右键菜单（网格**唯一**一层）：复制 / 搜索 / 藏列 / 排序 / 写动作 /
            // 详情。行级菜单也并到这里 —— 组件库的 `ContextMenu` 不截停冒泡，行与格
            // 两层嵌套会各弹一份叠在一起（见 `git log` 里那一轮修复）。
            //
            // 内容照 dbx 的宽度摆：一次右键把「看着这一格能做的事」都摆上来，别为了
            // 藏一列 / 排一次序还得跑回工具条。同一件事的**多个目标**（这一格 / 这一行
            // / 整份结果）都在，但一律只作用于**当前选区**（见下面「复制 ▸」的说明）。
            cell_menu: Some(Rc::new(move |menu, pos, window, cx| {
                let view = menu_view.clone();
                let Some(entity) = view.upgrade() else {
                    return menu;
                };
                // 弹出这一刻的事实**一次读齐**（只读借用页面）：文本与选区口径都
                // 弹出时才算 —— 一万行也不在渲染路径上白拼几百 KB。
                let facts = {
                    let page = entity.read(cx);
                    let Some(result) = page.current_result() else {
                        return menu;
                    };
                    let cell_text = result
                        .rows
                        .get(pos.row)
                        .and_then(|row| row.get(pos.col))
                        .map(Value::render)
                        .unwrap_or_default();
                    // 整行一条 TSV，粘进表格软件就是一整行单元格。
                    let row_text = result
                        .rows
                        .get(pos.row)
                        .map(|row| row_as_tsv(row))
                        .unwrap_or_default();
                    // 「复制 ▸」的父项文案跟着选区走（dbx 同款：复制永远作用于选区）
                    // —— 勾选行优先，其次单击选中的行，都没有才整份结果。文案让目标
                    // 一眼可辨，不再有「选中一行却复制了整页」的错位。
                    let checked_count = page.checked_rows.len();
                    let submenu_label: SharedString = if checked_count > 0 {
                        trf(cx, "复制所选 {} 行", &[&checked_count.to_string()]).into()
                    } else if page.selected_row.is_some() {
                        tr(cx, "复制选中行").into()
                    } else {
                        tr(cx, "复制为").into()
                    };
                    // 多格选区单独给一项：「复制 ▸」的几档按**行**作用域走，拖出来的
                    // 那块格子得有自己的出口（否则只能 ⌘C，菜单里找不到）。
                    let selection_label: Option<SharedString> = page
                        .active_cells_selection()
                        .filter(|range| range.cell_count() > 1)
                        .map(|range| {
                            let columns = page.selection_columns(result, range);
                            let cells = selection_stats(&result.rows, range, &columns).cells;
                            SharedString::from(trf(cx, "复制选区 {} 格", &[&cells.to_string()]))
                        });
                    CellMenuFacts {
                        column: result
                            .columns
                            .get(pos.col)
                            .map(|column| column.name.clone()),
                        cell_text,
                        row_text,
                        submenu_label,
                        selection_label,
                        can_insert: page.insert_context().is_some(),
                        editable: page.edit_source().is_some(),
                        deletable: page.pk_values_for_row(pos.row).is_some(),
                        any_hidden: !page.hidden_columns.is_empty(),
                        sortable: matches!(page.mode, Mode::Table | Mode::Query),
                        sort: page.active_sort().cloned(),
                    }
                };
                let CellMenuFacts {
                    column,
                    cell_text,
                    row_text,
                    submenu_label,
                    selection_label,
                    can_insert,
                    editable,
                    deletable,
                    any_hidden,
                    sortable,
                    sort,
                } = facts;
                // 菜单构建闭包是 `Fn`（每次右键 rebuild 一次）：文案只许借出克隆。
                let copy_cell_label = tr(cx, "复制单元格");
                let copied_cell_note = tr(cx, "已复制单元格。");
                let copy_row_label = tr(cx, "复制整行");
                let copied_row_note = tr(cx, "已复制该行。");
                let copy_column_label = tr(cx, "复制列名");
                let copied_column_note = tr(cx, "已复制列名。");
                let detail_label = tr(cx, "单元格详情");
                let search_label = tr(cx, "在结果中搜索此值");
                let hide_label = tr(cx, "隐藏此列");
                let show_all_label = tr(cx, "全部显示");
                let clear_sort_label = tr(cx, "清除排序");
                let edit_label = tr(cx, "编辑此单元格");
                let delete_label = tr(cx, "删除该行");
                // 「按 X 升序 / 降序」的文案要带列名：整份菜单里只有这一组是**指着
                // 某一列**说的，不带名字就成了「按什么排序？」。
                let (asc_label, desc_label) = match column.as_ref() {
                    Some(name) => (
                        SharedString::from(trf(cx, "按 {} 升序", &[name])),
                        SharedString::from(trf(cx, "按 {} 降序", &[name])),
                    ),
                    None => (tr(cx, "升序").into(), tr(cx, "降序").into()),
                };
                // 当前排序落没落在这一列上（决定两个方向项的勾选态）。
                let sorted_here = column
                    .as_ref()
                    .is_some_and(|name| sort.as_ref().is_some_and(|spec| spec.column == *name));
                let sorted_descending = sort.as_ref().is_some_and(|spec| spec.descending);
                // 「复制 ▸」子菜单：一次拿走**全部**行。文本在点某档的那一刻才按当前
                // 结果集渲染（一万行也不在渲染路径上白白拼几百 KB）。
                let copy_items: Vec<(ResultFormat, &'static str)> = ResultFormat::ALL
                    .iter()
                    .map(|format| (*format, format.copy_label(cx)))
                    .collect();
                // 行级 SQL 三档也收进这个子菜单（用户口径：所有「复制为 X」集中在
                // 一处，别在顶层再占一段）：与上面各档的作用域互补 —— 格式档拿的是
                // **选区**（勾选行 / 选中行 / 整份结果），这三档永远是「**右键指向的
                // 那一行**」。可用性同一道闸门：INSERT 只要有引擎的引号规则就能给；
                // UPDATE / WHERE 要主键定位（与「编辑此单元格」同门槛），没有就整组
                // 不画，留着置灰项只会让人猜「为什么这一行复制不出来」。
                let sql_row_items: Vec<(RowSqlKind, &'static str)> = [
                    (RowSqlKind::Insert, can_insert),
                    (RowSqlKind::Update, editable),
                    (RowSqlKind::Where, editable),
                ]
                .into_iter()
                .filter(|(_, available)| *available)
                .map(|(kind, _)| (kind, kind.menu_label(cx)))
                .collect();
                let all_view = view.clone();
                let sql_menu_row = pos.row;
                let copy_menu = PopupMenu::build(window, cx, move |menu, _, _| {
                    let menu = copy_items.into_iter().fold(menu, |menu, (format, label)| {
                        let enabled = format.exportable(true, can_insert, false);
                        let all_view = all_view.clone();
                        menu.item(PopupMenuItem::new(label).disabled(!enabled).on_click(
                            move |_, _, cx| {
                                let Some(entity) = all_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.copy_result_as(format, cx);
                                });
                            },
                        ))
                    });
                    if sql_row_items.is_empty() {
                        return menu;
                    }
                    let sql_view = all_view.clone();
                    let menu = menu.separator();
                    sql_row_items.into_iter().fold(menu, |menu, (kind, label)| {
                        let sql_view = sql_view.clone();
                        menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                            let Some(entity) = sql_view.upgrade() else {
                                return;
                            };
                            let row = sql_menu_row;
                            entity.update(cx, |this, cx| {
                                this.copy_row_as_sql(kind, row, cx);
                            });
                        }))
                    })
                });
                // 菜单项按**用途分组**攒进 `Vec` 再一次性拼上：分组之间那句
                // `separator()` 只有在**这一组真的有东西**时才该出现（空组 + 分隔线
                // 会在菜单里留下一段莫名其妙的横线）。链式 `.item()` 做不到「有条件地
                // 加分隔线」——攒起来 Fold 才做得到。
                let mut items: Vec<PopupMenuItem> = Vec::with_capacity(14);
                // ── 第一组：把这一格 / 这一行 / 这一列的名字拿走 ──
                //
                // 文案与文本**一个闭包一份克隆**：菜单项各是一个 `'static` 闭包，
                // 共享一份 `String` 就得让先建的那个改成借用，反而绕。
                let copy_cell_text = cell_text.clone();
                let cell_view = view.clone();
                // 多格选区在**最前面**：「复制这几格」是对当前选区最直接的那件事，
                // 排在逐格 / 整行复制之前才顺手。
                if let Some(label) = selection_label {
                    let selection_view = view.clone();
                    items.push(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        let Some(entity) = selection_view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.copy_cells_selection(cx);
                        });
                    }));
                }
                items.push(
                    PopupMenuItem::new(copy_cell_label).on_click(move |_, _, cx| {
                        let Some(entity) = cell_view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.put_on_clipboard(copy_cell_text.clone(), copied_cell_note, cx);
                        });
                    }),
                );
                let row_view = view.clone();
                items.push(
                    PopupMenuItem::new(copy_row_label).on_click(move |_, _, cx| {
                        let Some(entity) = row_view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.put_on_clipboard(row_text.clone(), copied_row_note, cx);
                        });
                    }),
                );
                if let Some(name) = column.clone() {
                    let column_view = view.clone();
                    items.push(
                        PopupMenuItem::new(copy_column_label).on_click(move |_, _, cx| {
                            let Some(entity) = column_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.put_on_clipboard(name.clone(), copied_column_note, cx);
                            });
                        }),
                    );
                }
                items.push(PopupMenuItem::submenu(submenu_label, copy_menu));
                // （行级 SQL 三档已并入上面的「复制为」子菜单：所有「复制为 X」集中
                // 一处，顶层不再单独占一段。）
                // ── 第二组：把这一列从视野里收掉 / 放回来，或按这一格的值过滤 ──
                //
                // 「搜索此值」写的是**结果内搜索框**（不是服务器侧 WHERE）：与「在结果
                // 中搜索」那块输入框是同一份状态，所以过滤完用户能在框里看到词、接着改。
                let search_view = view.clone();
                items.push(
                    PopupMenuItem::new(search_label).on_click(move |_, window, cx| {
                        let Some(entity) = search_view.upgrade() else {
                            return;
                        };
                        let value = cell_text.clone();
                        entity.update(cx, |this, cx| {
                            this.search_in_result(&value, window, cx);
                        });
                    }),
                );
                if let Some(name) = column.clone() {
                    let hide_view = view.clone();
                    items.push(PopupMenuItem::new(hide_label).on_click(move |_, _, cx| {
                        let Some(entity) = hide_view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.toggle_column_hidden(&name, cx);
                        });
                    }));
                }
                if any_hidden {
                    let show_view = view.clone();
                    items.push(
                        PopupMenuItem::new(show_all_label).on_click(move |_, _, cx| {
                            let Some(entity) = show_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| this.show_all_columns(cx));
                        }),
                    );
                }
                // ── 第三组：按这一列定序（表浏览与 SQL 结果各有一套落地） ──
                //
                // 三态与点列头同一套：右键定序是**直接指定方向**（不像点列头要轮转），
                // 所以两个方向项各一项、当前生效的那个打勾；「清除排序」只在真有排序时给。
                if sortable {
                    items.push(PopupMenuItem::separator());
                    let asc_view = view.clone();
                    let asc_name = column.clone();
                    items.push(
                        PopupMenuItem::new(asc_label)
                            .checked(sorted_here && !sorted_descending)
                            .on_click(move |_, _, cx| {
                                let Some(entity) = asc_view.upgrade() else {
                                    return;
                                };
                                let Some(name) = asc_name.clone() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| this.sort_by(&name, false, cx));
                            }),
                    );
                    let desc_view = view.clone();
                    let desc_name = column.clone();
                    items.push(
                        PopupMenuItem::new(desc_label)
                            .checked(sorted_here && sorted_descending)
                            .on_click(move |_, _, cx| {
                                let Some(entity) = desc_view.upgrade() else {
                                    return;
                                };
                                let Some(name) = desc_name.clone() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| this.sort_by(&name, true, cx));
                            }),
                    );
                    if sort.is_some() {
                        let clear_view = view.clone();
                        items.push(PopupMenuItem::new(clear_sort_label).on_click(
                            move |_, _, cx| {
                                let Some(entity) = clear_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| this.clear_sort(cx));
                            },
                        ));
                    }
                }
                // ── 第四组：写动作（只在结果集可编辑时出现） ──
                if editable || deletable {
                    items.push(PopupMenuItem::separator());
                }
                if editable {
                    let edit_view = view.clone();
                    items.push(
                        PopupMenuItem::new(edit_label).on_click(move |_, window, cx| {
                            let Some(entity) = edit_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.begin_cell_edit(pos, window, cx);
                            });
                        }),
                    );
                }
                if deletable {
                    let delete_view = view.clone();
                    items.push(
                        PopupMenuItem::new(delete_label).on_click(move |_, window, cx| {
                            let Some(entity) = delete_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.delete_current_row(pos.row, window, cx);
                            });
                        }),
                    );
                }
                // ── 第五组：这一格的完整内容（长文本 / 二进制在这里看） ──
                items.push(PopupMenuItem::separator());
                let detail_view = view.clone();
                items.push(PopupMenuItem::new(detail_label).on_click(move |_, _, cx| {
                    let Some(entity) = detail_view.upgrade() else {
                        return;
                    };
                    entity.update(cx, |this, cx| {
                        this.open_cell_detail(pos.row, pos.col, cx);
                    });
                }));
                items.into_iter().fold(menu, |menu, item| menu.item(item))
            })),
            // 列头右键：直接定序 / 预选过滤列 / 藏列 / 复制列名（DBX / TablePlus 的
            // 标准列头菜单；「筛选」只是把草稿列填上，仍要点「添加」生效）。
            //
            // ⚠️ 表浏览与 SQL 结果**都要给**：排序两边都有落地（服务器侧 `ORDER BY`
            // vs 客户端重排，见 [`Self::active_sort`]），藏列 / 复制列名更是两种模式
            // 都成立。差别只在「筛选」——那是表页动作，只有表浏览给。
            header_menu: matches!(self.mode, Mode::Table | Mode::Query).then(|| {
                let view = self.view.clone();
                let hook: HeaderMenuHook = Rc::new(move |menu, col, _window, cx| {
                    let view = view.clone();
                    let Some(entity) = view.upgrade() else {
                        return menu;
                    };
                    // 弹出这一刻一次读齐（与单元格菜单同一条纪律）：列名、模式、
                    // 当前排序、藏列集合都只读一次，之后只借克隆。
                    let (name, table_mode, sort, multi_column) = {
                        let page = entity.read(cx);
                        let Some(result) = page.current_result() else {
                            return menu;
                        };
                        match result.columns.get(col) {
                            Some(column) => (
                                column.name.clone(),
                                page.mode == Mode::Table,
                                page.active_sort().cloned(),
                                page.has_multiple_columns(cx),
                            ),
                            None => return menu,
                        }
                    };
                    let asc_label = trf(cx, "按 {} 升序", &[&name]).to_string();
                    let desc_label = trf(cx, "按 {} 降序", &[&name]).to_string();
                    let filter_label = trf(cx, "筛选「{}」", &[&name]).to_string();
                    let copy_label = tr(cx, "复制列名");
                    let copied_note = tr(cx, "已复制列名。");
                    let hide_label = tr(cx, "隐藏此列");
                    let clear_sort_label = tr(cx, "清除排序");
                    let sorted_here = sort.as_ref().is_some_and(|spec| spec.column == name);
                    let sorted_descending = sort.as_ref().is_some_and(|spec| spec.descending);
                    let asc_view = view.clone();
                    let desc_view = view.clone();
                    let filter_view = view.clone();
                    let copy_view = view.clone();
                    let hide_view = view.clone();
                    let clear_view = view.clone();
                    let asc_name = name.clone();
                    let desc_name = name.clone();
                    let filter_name = name.clone();
                    let hide_name = name.clone();
                    let mut menu = menu
                        .item(
                            PopupMenuItem::new(asc_label)
                                .checked(sorted_here && !sorted_descending)
                                .on_click(move |_, _, cx| {
                                    let Some(entity) = asc_view.upgrade() else {
                                        return;
                                    };
                                    entity
                                        .update(cx, |this, cx| this.sort_by(&asc_name, false, cx));
                                }),
                        )
                        .item(
                            PopupMenuItem::new(desc_label)
                                .checked(sorted_here && sorted_descending)
                                .on_click(move |_, _, cx| {
                                    let Some(entity) = desc_view.upgrade() else {
                                        return;
                                    };
                                    entity
                                        .update(cx, |this, cx| this.sort_by(&desc_name, true, cx));
                                }),
                        );
                    // 「筛选」是**表页**动作（服务器侧 WHERE）：SQL 结果集没有表页可筛，
                    // 不给这一项，否则点了只会填上一个不会生效的草稿列。
                    if table_mode {
                        menu = menu.item(PopupMenuItem::new(filter_label).on_click(
                            move |_, _, cx| {
                                let Some(entity) = filter_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.draft_filter_column(&filter_name, cx)
                                });
                            },
                        ));
                    }
                    menu = menu
                        .separator()
                        .item(PopupMenuItem::new(copy_label).on_click(move |_, _, cx| {
                            let Some(entity) = copy_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.put_on_clipboard(name.clone(), copied_note, cx);
                            });
                        }))
                        .item(PopupMenuItem::new(hide_label).on_click(move |_, _, cx| {
                            let Some(entity) = hide_view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| this.toggle_column_hidden(&hide_name, cx));
                        }));
                    if sort.is_some() {
                        menu = menu.item(PopupMenuItem::new(clear_sort_label).on_click(
                            move |_, _, cx| {
                                let Some(entity) = clear_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| this.clear_sort(cx));
                            },
                        ));
                    }
                    // ── 列顺序（显示顺序，不动数据）─────────────────────────
                    // 用菜单而不是「拖列头」：列头文字是可拖选 / 可 ⌘C 的，同一元素
                    // 上再挂拖拽源会与文本选择抢同一次「按下 + 移动」（见
                    // [`ColumnNudge`] 的说明）。只有一个可见列时整组不画。
                    if multi_column {
                        let order_label = tr(cx, "列顺序");
                        let left_label = tr(cx, "左移一列");
                        let right_label = tr(cx, "右移一列");
                        let first_label = tr(cx, "移到最左");
                        let last_label = tr(cx, "移到最右");
                        let order_menu = PopupMenu::build(_window, cx, move |menu, _, _| {
                            [
                                (left_label, ColumnNudge::Left),
                                (right_label, ColumnNudge::Right),
                                (first_label, ColumnNudge::First),
                                (last_label, ColumnNudge::Last),
                            ]
                            .into_iter()
                            .fold(menu, |menu, (label, nudge)| {
                                let view = view.clone();
                                menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                                    let Some(entity) = view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.reorder_column(col, nudge, cx);
                                    });
                                }))
                            })
                        });
                        menu = menu
                            .separator()
                            .item(PopupMenuItem::submenu(order_label, order_menu));
                    }
                    menu
                });
                hook
            }),
            // 列头**下拉**操作台（左键点 ∨，锚在按钮下方）：只收「这一列怎么显示、
            // 怎么筛」的四件事，与右键菜单（复制 / 藏列 / 写动作）是两条入口。每一项
            // 都落真实动作：格式化改模型预算、本地筛选改显示行、数据库筛选走表浏览
            // 的 WHERE 草稿、清除按列摘干净 —— 摆占位菜单等于让人猜。
            header_dropdown: matches!(self.mode, Mode::Table | Mode::Query).then(|| {
                let view = self.view.clone();
                let hook: HeaderMenuHook = Rc::new(move |menu, col, window, cx| {
                    let view = view.clone();
                    let Some(entity) = view.upgrade() else {
                        return menu;
                    };
                    // 弹出这一刻一次读齐（与右键菜单同一条纪律）：列名、数值性、
                    // 有没有选中格、各档开关都只读一次，之后只借克隆。
                    let (
                        name,
                        numeric,
                        has_selection,
                        local_active,
                        format_active,
                        sorted_here,
                        db_filterable,
                    ) = {
                        let page = entity.read(cx);
                        let Some(result) = page.current_result() else {
                            return menu;
                        };
                        let Some(column) = result.columns.get(col) else {
                            return menu;
                        };
                        let name = column.name.clone();
                        (
                            name.clone(),
                            super::helpers::result_column_is_numeric(&name, result),
                            page.selected_row.is_some(),
                            page.local_filters.contains_key(&name),
                            page.column_formats.contains_key(&name),
                            page.active_sort().is_some_and(|spec| spec.column == name),
                            page.mode == Mode::Table,
                        )
                    };
                    let format_label = tr(cx, "列格式化");
                    let thousands_label = tr(cx, "千分位");
                    let raw_label = tr(cx, "原始值");
                    let local_label = tr(cx, "本地值筛选");
                    let db_label = tr(cx, "数据库值筛选");
                    let clear_label = tr(cx, "清除");
                    let menu_view = view.clone();
                    let mut menu = menu;
                    // 「列格式化」只对数值列给：千分位有真实落点（模型预算时改显示
                    // 文本），文本 / 日期挑格式没有可用的展示层模板，给一条灰的只会
                    // 让人猜。子菜单两档的勾选态就是当前档位。
                    if numeric {
                        let thousands_view = menu_view.clone();
                        let raw_view = menu_view.clone();
                        let thousands_name = name.clone();
                        let raw_name = name.clone();
                        let submenu = PopupMenu::build(window, cx, move |menu, _, _| {
                            menu.item(
                                PopupMenuItem::new(thousands_label)
                                    .checked(format_active)
                                    .on_click(move |_, _, cx| {
                                        let Some(entity) = thousands_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.set_column_format(
                                                &thousands_name,
                                                Some(ColumnFormat::Thousands),
                                                cx,
                                            );
                                        });
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(raw_label)
                                    .checked(!format_active)
                                    .on_click(move |_, _, cx| {
                                        let Some(entity) = raw_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.set_column_format(&raw_name, None, cx);
                                        });
                                    }),
                            )
                        });
                        menu = menu.item(
                            PopupMenuItem::submenu(format_label, submenu)
                                .icon(Icon::new(DuckIcon::Code)),
                        );
                    }
                    // 「本地值筛选」以选中格的值为等值条件：没有选中格就没有条件可落，
                    // 置灰而不是弹出「请先选中」的提示（菜单项点了才解释是死路）。
                    menu = menu.item(
                        PopupMenuItem::new(local_label)
                            .icon(Icon::new(DuckIcon::ListFilter))
                            .checked(local_active)
                            .disabled(!has_selection)
                            .on_click({
                                let view = menu_view.clone();
                                let name = name.clone();
                                move |_, _, cx| {
                                    let Some(entity) = view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.toggle_local_filter_from_selection(&name, cx);
                                    });
                                }
                            }),
                    );
                    // 「数据库值筛选」是表页动作（服务器侧 WHERE）：SQL 结果集没有
                    // 表页可筛，不给这一项，否则点了只会填上一个不会生效的草稿列。
                    if db_filterable {
                        menu = menu.item(
                            PopupMenuItem::new(db_label)
                                .icon(Icon::new(DuckIcon::Database))
                                .on_click({
                                    let view = menu_view.clone();
                                    let name = name.clone();
                                    move |_, window, cx| {
                                        let Some(entity) = view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.db_filter_from_selection(&name, window, cx);
                                        });
                                    }
                                }),
                        );
                    }
                    if local_active || format_active || sorted_here {
                        menu = menu.separator().item(
                            PopupMenuItem::new(clear_label)
                                .icon(Icon::new(DuckIcon::Eraser))
                                .on_click({
                                    let view = menu_view.clone();
                                    let name = name.clone();
                                    move |_, _, cx| {
                                        let Some(entity) = view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.clear_column_effects(&name, cx);
                                        });
                                    }
                                }),
                        );
                    }
                    menu
                });
                hook
            }),
            // 双击单元格 = 进就地编辑（单击已经在组件里处理成「选中这一格」——
            // 选中与编辑是两步，单击就开编辑框等于把浏览打断成编辑）。
            on_cell_double_click: Some(Rc::new(move |pos, window, cx| {
                let Some(entity) = cell_dbl_view.upgrade() else {
                    return;
                };
                entity.update(cx, |this, cx| this.begin_cell_edit(pos, window, cx));
            })),
            // 双击行空白处（行号 / 多选列一带）= 开行编辑器（暂存式：改动只存输入框，
            // 点保存才落库）。
            on_row_double_click: Some(Rc::new(move |row, window, cx| {
                let Some(entity) = row_dbl_view.upgrade() else {
                    return;
                };
                // 只读借用阶段取数据，随后构造输入框实体。
                let specs = entity.read(cx).row_editor_specs(row);
                let cells = specs.map(|columns| {
                    columns
                        .into_iter()
                        .map(|(column, original, was_null)| {
                            let input = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
                            if !was_null {
                                input.update(cx, |state, cx| {
                                    state.set_value(original.clone(), window, cx);
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
                entity.update(cx, |this, cx| {
                    this.selected_row = Some(row);
                    this.row_editor = cells.map(|cells| RowEditor {
                        row_index: Some(row),
                        cells,
                    });
                    cx.notify();
                });
            })),
        }
    }

    /// 选中某一格：选中行 + 单元格详情指向一起落。
    ///
    /// 组件点单元格时 `RowSelected` 与 `CellSelected` 会各发一次（顺序行在前），
    /// 这条是「双击进编辑」那条路上的同一落点 —— 两处共用一份，免得一处的口径漂了。
    fn select_cell(&mut self, pos: CellPos, cx: &mut Context<Self>) {
        self.selected_row = Some(pos.row);
        self.cell_detail = Some((pos.row, pos.col));
        cx.notify();
    }

    /// 把当前结果推给数据表组件（换结果 / 换标签 / 清结果 / 换库时调用）。
    ///
    /// 只做两件事：换**表模型**（组件内部判列名序列要不要重估列宽并清横向位置），
    /// 再换**展示行序 + 藏列**（见 [`Self::sync_data_table_rows`]）。没有结果就推一份
    /// 空表 —— 组件恒有一份模型，渲染路径上因此不必判 `Option`。
    pub(super) fn sync_data_table(&mut self, cx: &mut Context<Self>) {
        let model = self
            .current_result()
            .map(|result| Arc::new(build_table_model(result, &self.column_formats)))
            .unwrap_or_default();
        self.data_table
            .update(cx, |state, cx| state.set_model(model, cx));
        self.sync_data_table_rows(cx);
        // 新结果落地后恢复这张表上次的列宽 / 隐藏列（查询模式下 key 为 `None`，
        // 这一步是空操作）。
        self.apply_grid_layout(cx);
    }

    /// 推「展示哪些行」和「藏了哪些列」给组件。
    ///
    /// 展示行序 = 搜索命中（客户端过滤，只搜可见列）× 客户端分页切片，两边都按
    /// **原始行下标**记（选中 / 勾选仍用原始下标，翻译只此一次）。表浏览是服务器侧
    /// 分页，整页即一页，不做切片。
    ///
    /// 这三件事（搜索词变 / 翻页 / 藏列变）以前都是**渲染路径上现算**的 —— 现在收成
    /// 「变化时推一次」。
    pub(super) fn sync_data_table_rows(&mut self, cx: &mut Context<Self>) {
        let (rows, offset) = self.visible_row_indexes(cx).unwrap_or_default();
        let hidden = self.hidden_columns.clone();
        self.data_table.update(cx, |state, cx| {
            state.set_hidden_names(&hidden, cx);
            state.set_view_rows(rows, offset, cx);
        });
    }

    /// 当前模式生效的排序：表浏览读服务器侧的 `table_sort`，SQL 结果读客户端的
    /// `query_sort`。
    ///
    /// 两个字段**刻意分开**（见各自的定义）：它们不只是「谁排的」不同 —— 一个是
    /// 数据库真的按它取数、另一个只是换个顺序看这批已到手的行。任何要「显示当前
    /// 排序」的地方（列头箭头、读数 chip、三态轮转）都从这里取，免得两处各判一遍。
    pub(super) fn active_sort(&self) -> Option<&SortSpec> {
        match self.mode {
            Mode::Table => self.table_sort.as_ref(),
            Mode::Query => self.query_sort.as_ref(),
            Mode::Instance(_) | Mode::Dashboard(_) | Mode::Structure(_) | Mode::Er { .. } => None,
        }
    }

    /// 当前生效的**全部**排序键（主键在前，追加键按加入顺序）。
    ///
    /// 只给「真的要用多键」的两处用：表浏览拼 `ORDER BY`、查询结果重排展示序。
    /// 列头箭头 / 右键 checked / 读数条仍读 [`Self::active_sort`]（只关心主排序）。
    pub(super) fn active_sort_keys(&self) -> Vec<(String, bool)> {
        self.sort_keys_spec()
            .into_iter()
            .map(|spec| (spec.column, spec.descending))
            .collect()
    }

    /// [`Self::active_sort_keys`] 的 `SortSpec` 版（写回 / 轮转的中间形态）。
    pub(super) fn sort_keys_spec(&self) -> Vec<SortSpec> {
        let (main, extra) = match self.mode {
            Mode::Table => (self.table_sort.as_ref(), &self.table_sort_extra),
            Mode::Query => (self.query_sort.as_ref(), &self.query_sort_extra),
            Mode::Instance(_) | Mode::Dashboard(_) | Mode::Structure(_) | Mode::Er { .. } => {
                return Vec::new();
            }
        };
        main.cloned()
            .into_iter()
            .chain(extra.iter().cloned())
            .collect()
    }

    /// 写回排序键：第一键进 `*_sort`，其余进 `*_sort_extra`，然后按模式刷新。
    ///
    /// 作废口径沿用单键时代的规矩：排序换了 → 行号含义变了 → 勾选 / 选中 / 行编辑器
    /// 一并作废（矩形选区同理：那块格子按行下标记，重排之后就指向别的数据了），
    /// 表浏览回第 1 页并重取（查询结果只重排展示序，不重跑语句）。
    pub(super) fn set_sort_keys(&mut self, keys: Vec<SortSpec>, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Table => {
                let mut rest = keys.into_iter();
                self.table_sort = rest.next();
                self.table_sort_extra = rest.collect();
                self.checked_rows.clear();
                self.selected_row = None;
                self.row_editor = None;
                self.cells_selection = None;
                self.table_page = 0;
                self.load_table_page(cx);
            }
            Mode::Query => {
                let mut rest = keys.into_iter();
                self.query_sort = rest.next();
                self.query_sort_extra = rest.collect();
                self.cells_selection = None;
                self.query_page = 0;
                self.sync_data_table_rows(cx);
                cx.notify();
            }
            Mode::Instance(_) | Mode::Dashboard(_) | Mode::Structure(_) | Mode::Er { .. } => {}
        }
    }

    /// 列头三态轮转（普通点击 = 替换，`Shift+点击` = 追加一级）。
    ///
    /// 两套落地见 [`Self::set_sort_keys`]：表浏览 = 服务器侧重取一页（数据真的换了
    /// 序），SQL 结果集 = 只把**已到手的行**重排一遍（不必、也不该重新查库）。
    /// 点列头与右键定序是同一个动作的两种入口，都汇到这里，免得哪一天漂开。
    pub(super) fn toggle_sort_key(&mut self, column: &str, additive: bool, cx: &mut Context<Self>) {
        let mut keys = self.sort_keys_spec();
        toggle_sort_keys_in(&mut keys, column, additive);
        self.set_sort_keys(keys, cx);
    }

    /// 右键定序：**直接指定方向**（不像点列头那样三态轮转），两套落地按模式分派。
    ///
    /// 点列头（[`Self::on_data_table_event`] 的 `SortRequested`）与右键菜单是同一个
    /// 动作的两种入口 —— 都在这里汇合，免得「右键按升序」和「点头一次」哪一天漂开。
    pub(super) fn sort_by(&mut self, column: &str, descending: bool, cx: &mut Context<Self>) {
        // 右键「按此列升 / 降序」是**替换**语义：清掉追加键，只留这一列。
        self.set_sort_keys(
            vec![SortSpec {
                column: column.to_owned(),
                descending,
            }],
            cx,
        );
    }

    /// 取消当前排序（右键「清除排序」）。
    ///
    /// 善后**复用** [`Self::set_sort_keys`]（空键表 = 不排序）：取消排序同样是「行号
    /// 的含义变了」，勾选 / 选中行 / 行编辑器 / 矩形选区一套作废、页码回第一页 ——
    /// 只清 `table_sort` 会把它们留在旧行号上，下一次删行就删错。这里只多一句
    /// 「本来就没排序就别白跑一次取数」。
    pub(super) fn clear_sort(&mut self, cx: &mut Context<Self>) {
        let sorted = match self.mode {
            Mode::Table => self.table_sort.is_some() || !self.table_sort_extra.is_empty(),
            Mode::Query => self.query_sort.is_some() || !self.query_sort_extra.is_empty(),
            Mode::Instance(_) | Mode::Dashboard(_) | Mode::Structure(_) | Mode::Er { .. } => false,
        };
        if !sorted {
            return;
        }
        self.set_sort_keys(Vec::new(), cx);
    }

    /// 把藏起来的列全部放出来（工具条「字段筛选」与右键菜单「全部显示」共用）。
    pub(super) fn show_all_columns(&mut self, cx: &mut Context<Self>) {
        if self.hidden_columns.is_empty() {
            return;
        }
        self.hidden_columns.clear();
        self.sync_data_table_rows(cx);
        self.store_grid_layout(cx);
        cx.notify();
    }

    /// 列头下拉「列格式化 ▸」：改该列的格式档（`None` = 还原原始值）。
    ///
    /// ⚠️ display 文本在模型里**预算**（`Arc<str>` 一次算好，见 `build_table_model`），
    /// 改档必须重推模型，渲染层消化不了。
    pub(super) fn set_column_format(
        &mut self,
        column: &str,
        format: Option<ColumnFormat>,
        cx: &mut Context<Self>,
    ) {
        match format {
            Some(format) => {
                self.column_formats.insert(column.to_owned(), format);
            }
            None => {
                self.column_formats.remove(column);
            }
        }
        self.sync_data_table(cx);
    }

    /// 列头下拉「本地值筛选」：以**当前选中格**该列的展示文本为等值条件，开关本地过滤。
    ///
    /// 只动显示行（`visible_row_indexes` 里夹一层），服务器数据不动；同一值再点一次
    /// = 解除。值按展示文本记（NULL 照 `render` 的文本算），与格子看见的口径一致。
    pub(super) fn toggle_local_filter_from_selection(
        &mut self,
        column: &str,
        cx: &mut Context<Self>,
    ) {
        let value_text = {
            let Some(result) = self.current_result() else {
                return;
            };
            let Some(col) = result.columns.iter().position(|c| c.name == column) else {
                return;
            };
            self.selected_row
                .and_then(|row| result.rows.get(row))
                .and_then(|row| row.get(col))
                .map(Value::render)
                .unwrap_or_else(|| "NULL".to_owned())
        };
        {
            let set = self.local_filters.entry(column.to_owned()).or_default();
            // 没有这个值 = 命中它（加进白名单）；已经有 = 再点一次解除。
            if !set.remove(&value_text) {
                set.insert(value_text);
            }
        }
        // 集合空了顺手摘键：空集合在过滤里等于「一行都留不下」，留着只会骗人。
        if self
            .local_filters
            .get(column)
            .is_some_and(BTreeSet::is_empty)
        {
            self.local_filters.remove(column);
        }
        self.sync_data_table_rows(cx);
    }

    /// 列头下拉「数据库值筛选」：把「该列 = 当前格的值」直接落成一条**服务器侧**条件。
    ///
    /// 只有表浏览有服务器侧筛选可挂。有选中格就一步加条件（`add_table_filter`）；
    /// 没有就只把筛选条草稿的列填上，操作符与值留给用户挑。NULL 格用「为空」
    /// 操作符——拿字符串 `"NULL"` 去等值永远查不出东西。
    pub(super) fn db_filter_from_selection(
        &mut self,
        column: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode != Mode::Table {
            return;
        }
        self.filter_column = Some(column.to_owned());
        let cell = {
            let Some(result) = self.current_result() else {
                return;
            };
            let col = result.columns.iter().position(|c| c.name == column);
            let row = self.selected_row;
            match (row, col) {
                (Some(row), Some(col)) => result.rows.get(row).and_then(|r| r.get(col)).cloned(),
                _ => None,
            }
        };
        match cell {
            Some(cell) if !cell.is_null() => {
                self.filter_op = FilterOp::Eq;
                let text = cell.render();
                self.filter_value
                    .update(cx, |state, cx| state.set_value(text, window, cx));
                self.add_table_filter(window, cx);
            }
            Some(_) => {
                self.filter_op = FilterOp::IsNull;
                self.after_filter_changed(cx);
            }
            None => cx.notify(),
        }
    }

    /// 列头下拉「清除」：摘掉该列的排序 / 本地筛选 / 格式化（有一件算一件）。
    pub(super) fn clear_column_effects(&mut self, column: &str, cx: &mut Context<Self>) {
        let had_filter = self.local_filters.remove(column).is_some();
        let had_format = self.column_formats.remove(column).is_some();
        if self.active_sort().is_some_and(|spec| spec.column == column) {
            // 排序有自己的刷新路径（表浏览重取页 / 查询重推行序），让它走完。
            self.clear_sort(cx);
            return;
        }
        if had_format {
            self.sync_data_table(cx);
        } else if had_filter {
            self.sync_data_table_rows(cx);
        }
    }

    /// 把这一格的值填进结果内搜索框（右键「在结果中搜索此值」）。
    ///
    /// 与 [`Self::clear_result_search`] 同一条纪律：写的是**输入框自己的值**，展示
    /// 行序由 `shell.rs` 里那条 `InputEvent::Change` 订阅重推 —— 这里不再推一遍
    ///（推两次会白算两遍命中集）。
    ///
    /// ⚠️ 取值**截到** [`SEARCH_VALUE_MAX_CHARS`]：TEXT / JSON 列没有长度上限，一格
    /// 可能就是整篇文档。拿整篇当搜索词既没意义（没人按 1 MB 的串匹配），又要在
    /// **每一行**上做一次子串匹配。截断后语义是「按这段文本的开头过滤」，普通短值
    /// 完全不受影响。
    pub(super) fn search_in_result(
        &mut self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value: String = value.chars().take(SEARCH_VALUE_MAX_CHARS).collect();
        self.result_search.update(cx, |state, cx| {
            state.set_value(&value, window, cx);
        });
        // 回执用**固定文案**，不把值拼进去：一格可能是整篇文档，拼进通知就是一条
        // 占满屏幕的横幅。搜索框自己已经显示了这个词，回执只需说一句「做了」。
        self.toast(notify::Tone::Info, tr(cx, "已按该值过滤结果。"), cx);
        cx.notify();
    }

    /// 开始就地编辑某一格（双击与右键菜单**共用**这一条路）。
    ///
    /// 两个入口的守卫、原值回填、聚焦全选完全同一份 —— 分开写迟早只有一处被改动
    ///（比如「结果集不可编辑时给提示」只加在双击那条路上，右键进来就是个空框）。
    pub(super) fn begin_cell_edit(
        &mut self,
        pos: CellPos,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 只读借用阶段取「原值 + 是否 NULL」，随后才构造输入框实体。
        let Some((original, was_null)) = self.cell_editor_prep(pos.row, pos.col) else {
            self.select_cell(pos, cx);
            return;
        };
        // 结果集不可编辑就明说，不摆空编辑框。
        if self.edit_source().is_none() {
            notify::info(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        }
        let input = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        // 原值回填：编辑器的起点就是「库里现在是什么」。
        if !was_null {
            let seed = original.clone();
            input.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        // dbx / dbflux 同款：进编辑即聚焦 + **全选** —— 高亮选区本身就是
        // 「输入框已接管这一格」的视觉信号，直接打字即可整格覆盖。
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
        self.select_cell(pos, cx);
        self.install_cell_editor(pos.row, pos.col, original, was_null, input, cx);
        cx.notify();
    }

    /// 当前应在网格里显示的行：`(原始行下标, 行号起点)`；没有结果时 `None`。
    fn visible_row_indexes(&self, cx: &App) -> Option<(Vec<usize>, usize)> {
        let result = self.current_result()?;
        let visible = visible_columns(&result.columns, &self.hidden_columns);
        let mut matched = matched_row_indexes(&result.rows, &visible, &self.result_search_text(cx));
        // 列头下拉的**本地值筛选**夹在「搜索命中」与「客户端排序」之间：只留
        // 「每个筛选列都命中任一允许值」的行。值按展示文本比（与格子里看见的一致），
        // 服务器数据不动——这是查询结果的筛选，别和表浏览的 WHERE 草稿混了。
        if !self.local_filters.is_empty() {
            let column_pos: Vec<(usize, &BTreeSet<String>)> = self
                .local_filters
                .iter()
                .filter_map(|(name, values)| {
                    result
                        .columns
                        .iter()
                        .position(|column| &column.name == name)
                        .map(|pos| (pos, values))
                })
                .collect();
            matched.retain(|&row| {
                column_pos.iter().all(|(pos, values)| {
                    result
                        .rows
                        .get(row)
                        .and_then(|cells| cells.get(*pos))
                        .is_some_and(|cell| values.contains(cell.render().as_str()))
                })
            });
        }
        // 客户端排序夹在「搜索命中」与「分页切片」之间：先定序、再切片，
        // 否则每一页各自排序，「第 2 页」就成了第 1 页的重排（表浏览那条注释同款）。
        // 多级排序：主键 + 追加键逐级比较。列名认不出的键跳过（结果集换了列后，
        // 旧排序键自然失效，不在这里报错）。
        let sort_columns: Vec<(usize, bool)> = self
            .query_sort
            .iter()
            .cloned()
            .chain(self.query_sort_extra.iter().cloned())
            .filter_map(|spec| {
                result
                    .columns
                    .iter()
                    .position(|column| column.name == spec.column)
                    .map(|position| (position, spec.descending))
            })
            .collect();
        if !sort_columns.is_empty() {
            sort_row_indexes_multi(&result.rows, &mut matched, &sort_columns);
        }
        if self.mode != Mode::Query {
            // 表浏览 = 服务器侧分页，整页即一页。
            return Some((matched, 0));
        }
        // 查询 = 客户端分页：按「每页行数」切片展示（切片存原始行下标）。
        let page_size = self.query_page_size(cx);
        let start = (self.query_page * page_size).min(matched.len());
        let end = (start + page_size).min(matched.len());
        Some((matched[start..end].to_vec(), start))
    }

    /// 组件报回来的交互（[`DataTableEvent`]）。
    ///
    /// 这里只接「靠 `cx` 就能落地」的动作；要窗口的（双击开编辑器、右键菜单内容）
    /// 走 [`Self::data_table_hooks`]。注意 `RowSelected` / `CellSelected` 是**两件事**：
    /// 前者 = 「正在看 / 正在编辑这一行」（行编辑器、右栏详情），后者 = 「单元格详情
    /// 指向这一格」—— 点单元格时组件两个都发，顺序是行在前。
    pub(super) fn on_data_table_event(&mut self, event: &DataTableEvent, cx: &mut Context<Self>) {
        match event {
            DataTableEvent::RowSelected { row } => {
                self.selected_row = Some(*row);
                cx.notify();
            }
            DataTableEvent::CellSelected(pos) => {
                self.cell_detail = Some((pos.row, pos.col));
                cx.notify();
            }
            // 拖选一块矩形选区：起选落锚点，之后只挪焦点（往回拖也是同一块，
            // [`CellRange::bounds`] 会规整方向）。
            DataTableEvent::SelectionStarted(pos) => {
                self.cells_selection = Some(CellRange::new(*pos, *pos));
                cx.notify();
            }
            DataTableEvent::SelectionExtended(pos) => {
                // 拖动事件在**任何**按下-移动里都会来（包括拖列宽 / 拖滚动条）：
                // 没有起选就没有选区可延展，静默丢掉。
                if let Some(range) = self.cells_selection {
                    self.cells_selection = Some(CellRange::new(range.anchor, *pos));
                    cx.notify();
                }
            }
            DataTableEvent::RowCheckedToggled { row, checked } => {
                // 事件带的是**目标态**（不是「翻一下」）：勾选集合按它直接写，免得
                // 与组件那一刻画出来的态出现一帧偏差。
                if *checked {
                    self.checked_rows.insert(*row);
                } else {
                    self.checked_rows.remove(row);
                }
                cx.notify();
            }
            DataTableEvent::AllCheckedToggled { .. } => {
                // 表头三态只有「全勾 / 不全勾」两档可点，语义与既有命令同一份实现
                //（`ToggleAllChecked` 也走它）。
                self.toggle_all_checked(cx);
            }
            DataTableEvent::SortRequested { column, additive } => {
                // 同一个动作、两套落地（见 [`Self::active_sort`] 的说明）。
                // `additive` = 按着 Shift：追加一级排序而不是替换。
                let name = column.to_string();
                self.toggle_sort_key(&name, *additive, cx);
            }
            // 列宽由组件自管（拖拽 / 双击自适应都在组件里）；这条事件是列宽
            // **落盘**的接缝：收尾时把整份布局写回 kv（含隐藏列）。
            DataTableEvent::ColumnResized { .. } => self.store_grid_layout(cx),
            // 列序变了同理：整份布局（宽度 + 藏列 + 列序）一次写回 kv。
            DataTableEvent::ColumnOrderChanged => self.store_grid_layout(cx),
            DataTableEvent::CellEditSubmitRequested(_) => self.commit_cell_edit(cx),
            DataTableEvent::CellDetailRequested(pos) => {
                self.open_cell_detail(pos.row, pos.col, cx);
            }
        }
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
        // 「列」是可搜索下拉：选项与选中值由 `sync_select_pickers` 按指纹同步
        //（列多时下拉顶上有搜索框，见 `DbSelectState`）；确认走 `SelectEvent` 订阅。

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
            // 同过滤条：Select 根是 size_full，定宽容器套外面才压得住。
            .child(
                div().flex_none().w(px(DROPDOWN_MIN_WIDTH)).child(
                    Select::new(&self.batch_column_select)
                        .placeholder(tr(cx, "选择列"))
                        .h(theme.density().control),
                ),
            )
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
                    .id("db-batch-value")
                    .min_w(px(160.))
                    .max_w(px(260.))
                    .flex_1()
                    // 批量条的输入框同样在结果面板下（见 `result_search_box` 的 ⚠️）。
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
            // 「列」是可搜索下拉：选项与选中值由 `sync_select_pickers` 按指纹同步
            //（列多时下拉顶上有搜索框，见 `DbSelectState`）；确认走 `SelectEvent` 订阅。
            // Select 的 `.w(...)` 只 refine 到**内部输入框**上，外层根仍是
            // `size_full` —— 不套一层定宽容器，工具条剩余空间照旧被占满
            //（后面的组件被推到最右），下拉弹层也按整条宽度展开。
            .child(
                div().flex_none().w(px(DROPDOWN_MIN_WIDTH)).child(
                    Select::new(&self.filter_column_select)
                        .placeholder(tr(cx, "选择列"))
                        .h(theme.density().control),
                ),
            )
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
                    .id("db-filter-value")
                    // 值输入框的宽度区间：布局约束（不在 spacing 阶梯上），保留裸值；
                    // 与批量条同口径（`min_w` 保底可读、`max_w` 防长值撑爆）。
                    .min_w(px(160.))
                    .max_w(px(260.))
                    .flex_1()
                    // 过滤条的输入框同样在结果面板下（见 `result_search_box` 的 ⚠️）。
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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

// ── 网格布局持久化（列宽 / 隐藏列）──────────────────────────────────────────

/// 单张表的网格布局：列宽与隐藏列。
///
/// **键 = 连接 + 限定的表名**，只对表浏览生效。查询结果不落盘：同一标签重跑
/// 另一条语句后列集合会整批换掉，存下来的宽度下次一列都对不上（而且查询结果的
/// 列宽更像「这一屏我想怎么看」的临时调整，见 `columns_mode_widths` 的同款理由）。
///
/// 全部字段 `#[serde(default)]`：以后加字段时旧存档照样能读。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct GridLayout {
    /// 列名 → 宽度（像素）。按**列名**存而不是下标：表加了一列之后下标会整体错位。
    columns: std::collections::BTreeMap<String, f32>,
    /// 隐藏的列名。
    hidden: Vec<String>,
    /// 列的**展示顺序**（列名序列；空 = 原序）。结果集的列集换过之后，对不上的
    /// 名字在组件里被忽略、新列按原序接在后面（见 `set_order_names`）。
    order: Vec<String>,
}

impl DatabasePage {
    /// 当前表浏览的布局键；不是表浏览 / 没有选中表时 `None`。
    fn grid_layout_key(&self) -> Option<String> {
        if self.mode != Mode::Table {
            return None;
        }
        let table = self.selected_table.as_ref()?;
        let conn = self.active.as_ref()?;
        let qualified = match &table.schema {
            Some(schema) => format!("{schema}.{}", table.name),
            None => table.name.clone(),
        };
        Some(format!("db_grid_layout:{conn}:{qualified}"))
    }

    fn load_grid_layout(&self, cx: &App) -> GridLayout {
        self.grid_layout_key()
            .and_then(|key| duck_store::kv_get(cx, &key))
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// 把组件当前的列宽 + 页面当前的藏列写回 kv。
    ///
    /// 写的是**全量**而不是单个被拖的列：组件里所有列都有宽度（估算值也算），
    /// 一起存下来最省事，恢复时也不必猜「哪几列是用户拖过的」。
    pub(super) fn store_grid_layout(&self, cx: &App) {
        let Some(key) = self.grid_layout_key() else {
            return;
        };
        let state = self.data_table.read(cx);
        let model = state.model();
        let mut layout = GridLayout {
            hidden: self.hidden_columns.iter().cloned().collect(),
            order: state.order_names(),
            ..GridLayout::default()
        };
        for (index, column) in model.columns().iter().enumerate() {
            layout
                .columns
                .insert(column.name.to_string(), state.column_width(index));
        }
        if let Ok(text) = serde_json::to_string(&layout) {
            duck_store::kv_put(cx, key, text);
        }
    }

    /// 恢复当前表的布局（换表 / 新结果落地后调用一次）。
    ///
    /// 先设宽度再设藏列：`set_hidden_names` 只影响可见列集合，与宽度互不依赖；
    /// 顺序反过来也不会错，只是这里按「先几何、后可见性」读起来顺。
    pub(super) fn apply_grid_layout(&mut self, cx: &mut Context<Self>) {
        let layout = self.load_grid_layout(cx);
        if layout.columns.is_empty() && layout.hidden.is_empty() && layout.order.is_empty() {
            return;
        }
        // 列序先套：它是「先画哪一列」，与宽度 / 藏列互不依赖，但先摆好列序，
        // 后面按名字查宽度那一步读起来才不别扭。
        if !layout.order.is_empty() {
            let order = layout.order.clone();
            self.data_table
                .update(cx, |state, cx| state.set_order_names(&order, cx));
        }
        let column_indexes: Vec<(usize, f32)> = {
            let state = self.data_table.read(cx);
            state
                .model()
                .columns()
                .iter()
                .enumerate()
                .filter_map(|(index, column)| {
                    layout
                        .columns
                        .get(column.name.as_ref())
                        .map(|width| (index, *width))
                })
                .collect()
        };
        if !column_indexes.is_empty() {
            self.data_table.update(cx, |state, cx| {
                for (index, width) in column_indexes {
                    state.set_width(index, width, cx);
                }
            });
        }
        if !layout.hidden.is_empty() {
            self.hidden_columns = layout.hidden.into_iter().collect();
            self.sync_data_table_rows(cx);
        }
    }
}
