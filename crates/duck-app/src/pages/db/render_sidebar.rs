//! 右栏（shell 级 dock 的数据库分段）：内容一律是「dock 分段头 + 可滚动的分段体」两层，
//! 头固定、体自己滚。历史分段与会话无关（历史跨连接），所以先于「还没连库」那条早返回
//! 分流 —— 否则用户在最需要翻旧语句的时候反而看不到它。
//!
//! 本文件是 `impl DatabasePage` 的一个切片，方法定义在子模块后每个都标 `pub(super)`
//!（与 `pages::terminal` 同一口径）；类型定义仍在 `mod.rs`。

use super::*;

impl DatabasePage {
    pub(crate) fn render_right_sidebar(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();

        // 右栏内容一律是「dock 分段头 + 可滚动的分段体」两层：头固定，体自己滚。
        // 历史分段与会话**无关**（历史跨连接），所以先分流 —— 否则「还没连库」那条
        // 早返回会把历史一起挡掉，用户在最需要翻旧语句的时候反而看不到它。
        //
        // 尺寸先取成局部 `Copy` 值再进闭包：闭包是 `move`，直接捕获 `tokens`
        // 会把它搬走，详情分支还要用。
        let body_px = tokens.spacing.lg;
        let body_py = tokens.spacing.md;
        // `scroll` 决定分段体是「内容自然高度 + 外层滚动」还是「自己占满、内部自理」：
        // 详情 / 历史 / 单元格 / 构建器都是前一种（内容有多长就多长，超出由外层滚）；
        // AI 助手是后一种（对话区自己滚、输入区钉在底部），套进外层滚动会被滚走。
        // 分段头（`tabs`）由调用方先算好再传进来：`dock_shell` 不捕获 `self`/`cx`，
        // 否则 `&mut App` 会被 move 进闭包，后面那些分支就没法再用它了。
        let dock_shell = |tabs: AnyElement, body: AnyElement, scroll: bool| {
            v_flex()
                .id("db-right-sidebar")
                .size_full()
                .min_h_0()
                .min_w_0()
                .child(tabs)
                .child(
                    div()
                        .id("db-right-sidebar-body")
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .when(scroll, |el| el.overflow_y_scroll())
                        .px(body_px)
                        .py(body_py)
                        .child(body),
                )
                .into_any_element()
        };

        // AI 助手：不套滚动壳（见上），且与会话**无关** —— 还没连库时也该能打开它，
        // 看到欢迎语与「去哪配置厂商」那条提示。
        if self.right_dock == RightDock::Ai {
            return dock_shell(
                self.render_right_dock_tabs(cx),
                self.render_ai_dock(cx),
                false,
            );
        }
        if self.right_dock == RightDock::History {
            return dock_shell(
                self.render_right_dock_tabs(cx),
                self.render_history_dock(cx),
                true,
            );
        }
        // 单元格详情也**不依赖会话分支**（依赖的是当前结果集）：先于「未连接」
        // 早返回分流，否则「网格里刚点过 ⓘ、连接断了一下」详情就跟着消失。
        if self.right_dock == RightDock::Cell {
            return dock_shell(
                self.render_right_dock_tabs(cx),
                self.render_cell_detail_dock(cx),
                true,
            );
        }
        // 构建器同样**不依赖会话分支**：右栏停在构建器上时连接断了，已经拼了一半的
        // 条件不该跟着消失（构建器自己会说明「先选一张目标表」/ 列清单取不到）。
        if self.right_dock == RightDock::Builder {
            return dock_shell(
                self.render_right_dock_tabs(cx),
                self.render_builder_dock(window, cx),
                true,
            );
        }

        let Some(session) = self.active_session() else {
            return dock_shell(
                self.render_right_dock_tabs(cx),
                empty_state(
                    DuckIcon::Database,
                    tr(cx, "尚未连接数据库"),
                    Some(tr(cx, "在左栏选择一条连接，或新建一条。").into()),
                    None,
                    cx,
                )
                .into_any_element(),
                true,
            );
        };

        // ── 命名空间选择器（多 schema / 多库）──
        //
        // 只要引擎有命名空间这一层就**总是**给这个控件（构造与编辑器工具条共用
        // [`Self::namespace_picker`]）。
        //
        // 曾经写成「`schemas.len() > 1` 才给」，理由是「只能选一项的下拉是噪音」。
        // 代价出现在用户的实际路径上：不填库名连上 MySQL / PostgreSQL 之后，
        // 左栏对象树是空的（还没有任何命名空间被选中），而这个**唯一能选出库的入口
        // 也一起消失了** —— 界面表现就是「连接成功了，但什么都看不到」。
        // 单库时它至少充当「你现在在哪个库里」的显式读数，不是噪音。
        let schema_picker = self.namespace_picker(&self.dock_schema_select, cx);

        // ── 结构管理入口：新建表 ──
        //
        // 只对**有 SQL DDL 的引擎**给按钮（MongoDB / Elasticsearch 没有这一层，
        // 不给入口而不是给了再报错）。落点跟随当前命名空间，与用户在树里看到的
        // 那一层保持一致。点击直接开「新建表」标签（dbx 同款：可视化网格建表，
        // 不再往编辑器塞骨架）。
        let new_table_button = self
            .active_config()
            .map(|cfg| cfg.kind)
            .filter(|engine| supports_ddl(*engine))
            .filter(|_| self.active_quote().is_some())
            .map(|_| {
                let namespace = self
                    .selected_table
                    .as_ref()
                    .and_then(|table| table.schema.clone())
                    .or_else(|| session.schema.clone())
                    .or_else(|| session.database.clone());
                let entry = view.clone();
                button("db-new-table", tr(cx, "新建表"), ButtonVariant::Ghost, cx).on_click(
                    move |_, window, cx| {
                        let Some(entity) = entry.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            let Some(conn_id) = this.active.clone() else {
                                return;
                            };
                            this.open_new_table_tab(&conn_id, namespace.clone(), window, cx);
                        });
                    },
                )
            });

        // 对象树已经**整体迁到左栏**（连接 → 库 → 分类 → 对象，对齐 DBFlux / DBX）：
        // 右栏只承担「选中对象的详情」（DDL / 表元信息 / 行）与连接信息 —— 同一个
        // 东西在两处各画一份，改一处忘一处就是两套行为。
        // 选中行：有编辑器就出暂存编辑卡，否则出只读详情。
        //
        // 不再按 `Mode` 分支：可编辑 SELECT 的结果集也能改（`edit_source` 说了算），
        // 表浏览与 SQL 结果在编辑这件事上没有区别。
        let row_panel = match self.row_editor.as_ref() {
            Some(editor) => Some(self.render_row_editor(editor, cx)),
            None => self.readonly_row_detail(cx),
        };

        dock_shell(
            self.render_right_dock_tabs(cx),
            v_flex()
                .w_full()
                .gap(tokens.spacing.lg)
                .child(
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .child(caption_heading(tr(cx, "连接信息"), cx))
                        .child(
                            card(cx)
                                .w_full()
                                .px(tokens.spacing.lg)
                                .py(tokens.spacing.sm)
                                .child(description_list({
                                    let mut rows = vec![
                                        description_row(
                                            tr(cx, "引擎"),
                                            // 引擎名走 `EngineRef`：agent 档的真名在驱动清单里。
                                            self.active_config().map_or_else(
                                                // 没档案时按 SQLite 兜底（与 `builder_engine` 同一口径）。
                                                || {
                                                    EngineRef::new(DatabaseKind::Sqlite, None)
                                                        .label()
                                                },
                                                |cfg| tr(cx, EngineRef::from_config(cfg).label()),
                                            ),
                                        ),
                                        description_row(tr(cx, "版本"), session.version.clone()),
                                        description_row(
                                            tr(cx, "接入方式"),
                                            session
                                                .transport
                                                .as_ref()
                                                .map_or_else(|| tr(cx, "直连"), |t| t.label(cx)),
                                        ),
                                    ];
                                    // 通道进程死了但会话还挂着：**必须显式说**。
                                    // 不说的话表现是「查询一直超时」，而用户会去查库、查网络，
                                    // 永远查不到「ssh 早就退出了」这件事上。
                                    if session
                                        .transport
                                        .as_ref()
                                        .is_some_and(|transport| !transport.is_alive())
                                    {
                                        rows.push(description_row(
                                            tr(cx, "通道状态"),
                                            tr(cx, "已断开（请重新连接）"),
                                        ));
                                    }
                                    rows
                                })),
                        ),
                )
                .child(
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(
                            h_flex()
                                .w_full()
                                .items_center()
                                .justify_between()
                                .child(caption_heading(tr(cx, "命名空间"), cx))
                                .children(new_table_button),
                        )
                        .children(schema_picker),
                )
                .children(
                    (self.mode == Mode::Table)
                        .then(|| self.table_ddl.clone())
                        .flatten()
                        .map(|ddl| {
                            // 「修改表结构」= 新开**结构标签**（dbx 的图形化字段网格
                            // 对位）：改名 / 改类型 / 增删列都在网格上完成，SQL 预览
                            // 实时生成、确认后执行 —— 把 DDL 送进编辑器只是它的
                            // 兜底（标签里随时可以「复制 SQL」拿走全文）。
                            let target = self.selected_table.clone();
                            let view = view.clone();
                            v_flex()
                                .w_full()
                                .gap(tokens.spacing.xs)
                                .child(
                                    h_flex()
                                        .w_full()
                                        .items_center()
                                        .justify_between()
                                        .child(caption_heading(tr(cx, "DDL"), cx))
                                        .child(
                                            button(
                                                "db-edit-ddl",
                                                tr(cx, "修改表结构"),
                                                ButtonVariant::Ghost,
                                                cx,
                                            )
                                            .on_click(
                                                {
                                                    move |_, window, cx| {
                                                        let Some(entity) = view.upgrade() else {
                                                            return;
                                                        };
                                                        let Some(target) = target.clone() else {
                                                            return;
                                                        };
                                                        entity.update(cx, |this, cx| {
                                                            let Some(conn_id) = this.active.clone()
                                                            else {
                                                                return;
                                                            };
                                                            this.open_structure_tab(
                                                                &conn_id, target, false, window, cx,
                                                            );
                                                        });
                                                    }
                                                },
                                            ),
                                        ),
                                )
                                .child(
                                    card(cx)
                                        .id("db-ddl-card")
                                        .w_full()
                                        // 滚动卡片的最大高度：布局约束（非 spacing 阶梯），
                                        // 与「索引」卡片共用 [`DETAIL_CARD_MAX_HEIGHT`]。
                                        .max_h(px(DETAIL_CARD_MAX_HEIGHT))
                                        .overflow_y_scroll()
                                        .px(tokens.spacing.lg)
                                        .py(tokens.spacing.sm)
                                        .child(
                                            div()
                                                .text_size(tokens.typography.mono_md.size)
                                                .text_color(tokens.colors.muted_foreground)
                                                .child(selectable_text(SharedString::from(ddl))),
                                        ),
                                )
                        }),
                )
                .children((self.mode == Mode::Table).then(|| self.render_table_meta(cx)))
                .children(row_panel.map(|panel| {
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(caption_heading(tr(cx, "选中行"), cx))
                        .child(panel)
                }))
                .into_any_element(),
            true,
        )
    }
}
