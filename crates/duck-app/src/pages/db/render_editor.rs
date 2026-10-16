//! SQL 编辑器工具条与诊断条：执行 / 停止、超时与分页设置、模式标签（关键字大小写、
//! 自动换行、语义诊断）、格式化与压缩入口，以及编辑器下方的诊断列表。
//!
//! 编辑器的**内容与历史**不在这里 —— 工具条只发号施令，落状态的是 `query.rs`。
//!
//! 本文件是 `impl DatabasePage` 的一个切片，方法定义在子模块后每个都标 `pub(super)`
//!（与 `pages::terminal` 同一口径）；类型定义仍在 `mod.rs`。

use super::*;

impl DatabasePage {
    /// 编辑器工具条：执行按钮 + 状态 + 模式标签。
    /// SQL 语义诊断**问题条**（编辑器面板底部，dbx 语义诊断的折衷版式）。
    ///
    /// gpui-base 编辑器没有装饰通道（画不了下划线 / 波浪线），诊断折衷为一条
    /// 列表：解析报错与未知表各占一行，位置 + 内核英文消息原样给出 —— 消息
    /// 不翻壳（`Unknown table "x"` 是让用户**照着搜**的，翻成中文反而搜不到）。
    /// 干净时不摆任何东西（dbx 同款：没消息就是好消息，一条绿条纯属噪音）。
    pub(super) fn render_sql_diagnostics_bar(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.sql_diagnostics.is_empty() || !db_prefs(cx).semantic_diagnostics {
            return None;
        }
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let total = self.sql_diagnostics.len();
        let shown = self
            .sql_diagnostics
            .iter()
            .take(3)
            .map(|diagnostic| {
                let (icon, color) = match diagnostic.severity {
                    DiagSeverity::Error => (DuckIcon::CircleAlert, tokens.colors.destructive),
                    DiagSeverity::Warning => (DuckIcon::TriangleAlert, extended.colors.warning),
                };
                // 位置解不出（line == 0）就只给消息 —— 别摆一个「第 0 行」出来。
                let location = if diagnostic.line == 0 {
                    None
                } else if diagnostic.column == 0 {
                    Some(SharedString::from(trf(
                        cx,
                        "第 {} 行",
                        &[&diagnostic.line.to_string()],
                    )))
                } else {
                    Some(SharedString::from(trf(
                        cx,
                        "第 {} 行第 {} 列",
                        &[&diagnostic.line.to_string(), &diagnostic.column.to_string()],
                    )))
                };
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(Icon::new(icon).size(extended.icon.sm).text_color(color))
                    .children(location.map(|location| {
                        div()
                            .flex_none()
                            .text_color(tokens.colors.muted_foreground)
                            .child(location)
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(diagnostic.message.clone())),
                    )
            })
            .collect::<Vec<_>>();
        Some(
            v_flex()
                .id("db-sql-diag-bar")
                .flex_none()
                .max_h(px(96.))
                .overflow_y_scroll()
                .gap(tokens.spacing.xxs)
                .border_t(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xxs)
                .bg(tokens.colors.background)
                .children(shown)
                .when(total > 3, |bar| {
                    bar.child(
                        div()
                            .flex_none()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(trf(
                                cx,
                                "共 {} 个问题",
                                &[&total.to_string()],
                            ))),
                    )
                })
                .into_any_element(),
        )
    }

    pub(super) fn render_editor_toolbar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        // ── 左侧：动作按钮行（设计稿 toolbar 同款，全部带文字）──
        //
        // 曾经为了省宽度收成纯图标，但设计稿定稿是文字按钮（图标行的语义只能
        // 靠悬停猜）；恢复文字，主行动作「运行」保持主色实底。
        let sep = || {
            // 设计稿 `.tb-sep`：一条 1px 竖线把「改 SQL 的动作」和「管 SQL 的动作」分开。
            div()
                .w(px(1.))
                .h(px(16.))
                .flex_none()
                .bg(extended.colors.hairline)
                .mx(tokens.spacing.xxs)
        };

        // 工具条动作钮：**整排统一纯图标**（图标/文字混排会让这排忽宽忽窄，也不成
        // 体系 —— dbx 顶部就是全图标）。`label` 只进无障碍名，`tip` 为 dbx 同款的
        // 悬停提示（动作 + 快捷键），由 `icon_button_tip` 在组件内部挂；图标用 14px
        // 档（token 注释：按钮内图标），与右侧两个开关、查询选项同一规格。
        let tb = |id: &'static str,
                  label: &'static str,
                  icon: DuckIcon,
                  variant: ButtonVariant,
                  tip: &'static str| {
            let btn_icon = Icon::new(icon).size(extended.icon.md);
            if tip.is_empty() {
                icon_button(id, label, btn_icon, variant, cx)
            } else {
                icon_button_tip(id, label, btn_icon, tip, variant, cx)
            }
        };

        // 开关类工具条项（dbx 顶部「使用小写关键字 / 自动换行」同款）：纯图标 + 悬停
        // 提示，状态由按钮底色体现 —— 开 = 次级面（有实体感），关 = 幽灵（融进底）。
        // 文字标签只进无障碍名与提示，不占横向宽度：这一行已经有十来个按钮了。
        //
        // ⚠️ 提示一律由 `icon_button_tip` 在组件内部挂（`icon_button*` 自己带提示），
        // **不能再在返回值上加 `.tooltip()`** —— gpui 对同一元素二次挂载直接 panic。
        let opt =
            |id: &'static str, label: &'static str, icon: DuckIcon, on: bool, tip: &'static str| {
                let variant = if on {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Ghost
                };
                icon_button_tip(
                    id,
                    label,
                    Icon::new(icon).size(extended.icon.md),
                    tip,
                    variant,
                    cx,
                )
            };

        // 「运行」与「执行计划」的提示都随选区变（dbx 同款：主按钮自己判断作用于
        // 哪一段，没有同伴按钮）。无选区时与可见文字差别不大，所以给「全部语句」
        // 这层信息；有选区时明确说「只执行 / 只分析选中的」，让用户知道这一下不会
        // 跑整篇 —— 删掉独立的「选中执行」按钮之后，这层提示是唯一的口径说明。
        let has_selection = self.editor_selection(cx).is_some();
        let run_tip = if has_selection {
            tr(cx, "只执行选中的语句")
        } else {
            tr(cx, "执行全部语句（⌘↵）")
        };
        let explain_tip = if has_selection {
            tr(cx, "只分析选中的语句")
        } else {
            tr(cx, "查看执行计划")
        };
        // 跑着就把「运行」换成「停止」：主按钮永远只有一个动作，
        // 用户不会去点一个点了没用的按钮。
        let actions = h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            // 跑着就把「执行」换成「停止」：主按钮永远只有一个动作，
            // 用户不会去点一个点了没用的按钮。
            .child(if self.running.is_some() {
                tb(
                    "db-stop-query",
                    tr(cx, "停止"),
                    DuckIcon::Square,
                    ButtonVariant::Secondary,
                    tr(cx, "停止执行（⌘.）"),
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.stop_running(window, cx));
                    }
                })
                .into_any_element()
            } else {
                tb(
                    "db-run-query",
                    tr(cx, "运行"),
                    DuckIcon::Play,
                    ButtonVariant::Primary,
                    run_tip,
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.run_query(window, cx));
                    }
                })
                .into_any_element()
            })
            .child(
                tb(
                    "db-format-sql",
                    tr(cx, "格式化"),
                    DuckIcon::AlignLeft,
                    ButtonVariant::Ghost,
                    tr(cx, "格式化 SQL"),
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.format_editor_sql(window, cx));
                    }
                }),
            )
            // 编辑器选项（dbx 顶部同款，纯图标 + 悬停提示）：紧跟着「格式化」排，
            // 因为「小写关键字」改的就是格式化时的关键字大小写。
            .child(
                opt(
                    "db-toggle-lower-keywords",
                    tr(cx, "小写关键字"),
                    DuckIcon::CaseLower,
                    db_prefs(cx).sql_keyword_case() == duck_db::SqlKeywordCase::Lower,
                    tr(cx, "格式化时关键字使用小写"),
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.toggle_keyword_case(cx));
                    }
                }),
            )
            .child(
                opt(
                    "db-toggle-word-wrap",
                    tr(cx, "自动换行"),
                    DuckIcon::WrapText,
                    db_prefs(cx).editor_word_wrap,
                    tr(cx, "长语句自动换行"),
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.toggle_editor_word_wrap(window, cx);
                        });
                    }
                }),
            )
            // 「SQL 语义诊断」开关（dbx 顶部同款）：提示文案随状态翻转 ——
            // 「已开启（点击关闭）」，用户不用试点就知道这一下会干什么。
            .child(
                opt(
                    "db-toggle-semantic-diag",
                    tr(cx, "SQL 语义诊断"),
                    DuckIcon::SpellCheck,
                    db_prefs(cx).semantic_diagnostics,
                    if db_prefs(cx).semantic_diagnostics {
                        tr(cx, "SQL 语义诊断：已开启（点击关闭）")
                    } else {
                        tr(cx, "SQL 语义诊断：已关闭（点击开启）")
                    },
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.toggle_semantic_diagnostics(cx);
                        });
                    }
                }),
            )
            // 「注释 / 压缩」（设计稿 `commentBtn` / `compactBtn`）：注释按整行切、
            // 压缩只动注释与空白。编辑器为空时两个都是点了没意义，禁用。
            .child(
                tb(
                    "db-comment-sql",
                    tr(cx, "注释"),
                    DuckIcon::MessageSquare,
                    ButtonVariant::Ghost,
                    tr(cx, "注释 / 取消注释"),
                )
                .disabled(self.sql.read(cx).value().trim().is_empty())
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.toggle_comment_editor_sql(window, cx);
                        });
                    }
                }),
            )
            .child(
                tb(
                    "db-minify-sql",
                    tr(cx, "压缩"),
                    DuckIcon::Minimize2,
                    ButtonVariant::Ghost,
                    tr(cx, "压缩 SQL"),
                )
                .disabled(self.sql.read(cx).value().trim().is_empty())
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.minify_editor_sql(window, cx));
                    }
                }),
            )
            // 「执行计划」收在动作组末尾：设计稿顺序 = 运行 / 选中执行 / 格式化 /
            // 注释 / 压缩 / 执行计划，改 SQL 的动作在前、看数据库的动作在后。
            .child(
                tb(
                    "db-explain-query",
                    tr(cx, "执行计划"),
                    DuckIcon::Network,
                    ButtonVariant::Ghost,
                    explain_tip,
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.explain_editor_sql(window, cx));
                    }
                }),
            )
            // 「执行文件」：跑一份 `.sql` 脚本（批处理路径，不走结果标签）。
            // 摆在运行组末尾 —— 它和「运行」是同一件事的两种输入（编辑器内容 / 磁盘上的
            // 文件），中间不该插进改 SQL 的动作；分隔线右侧仍是「管 SQL 的动作」。
            .child(
                tb(
                    "db-run-sql-file",
                    tr(cx, "执行文件"),
                    DuckIcon::FileText,
                    ButtonVariant::Ghost,
                    tr(cx, "选择 .sql 文件并执行"),
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.run_sql_file(window, cx));
                    }
                }),
            )
            .child(sep())
            // 「保存查询」：把编辑器里的 SQL 存进左栏「已保存查询」。
            // 编辑器为空时禁用——存一条空语句没有任何意义。
            .child(
                tb(
                    "db-save-query",
                    tr(cx, "保存查询"),
                    DuckIcon::Save,
                    ButtonVariant::Ghost,
                    tr(cx, "保存到已保存查询"),
                )
                .disabled(self.sql.read(cx).value().trim().is_empty())
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.open_save_query_dialog(window, cx);
                        });
                    }
                }),
            )
            .child(
                tb(
                    "db-history-toggle",
                    tr(cx, "历史"),
                    DuckIcon::Clock,
                    // 高亮 = 「右栏开着 **且** 停在这一栏」。右栏收起后必须灭掉，否则
                    // 按钮在说「你正在看历史」而右栏根本没显示 —— `right_dock` 只表达
                    // 「上次看哪一栏」（收起后不清，下次展开还看它），开合是另一回事。
                    if self.right_dock == RightDock::History
                        && crate::app::right_sidebar_open(cx).unwrap_or(false)
                    {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Ghost
                    },
                    tr(cx, "查询历史与已保存查询"),
                )
                // 两栏都空才禁用：已保存查询也是「历史」这一栏的内容。
                .disabled(self.history.is_empty() && self.saved_queries.is_empty())
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            // 这是个开关，但「关」的含义是**收起右栏**（对齐 IDE 工具窗
                            // 习惯），不是把右栏内容切回详情 —— 想换内容走右栏顶部的
                            // 分段 tab。右栏收着、或当前停在别的分段时则展开并切到历史。
                            let open = crate::app::right_sidebar_open(cx).unwrap_or(false);
                            if this.right_dock == RightDock::History && open {
                                this.right_dock_auto = false;
                                crate::app::set_right_sidebar_open(cx, false);
                            } else {
                                this.reveal_right_dock(RightDock::History, cx);
                            }
                            cx.notify();
                        });
                    }
                }),
            )
            .child(sep())
            .child(
                icon_button(
                    "db-query-options",
                    tr(cx, "查询选项"),
                    Icon::new(DuckIcon::SlidersHorizontal).size(extended.icon.md),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.open_query_options(window, cx));
                    }
                }),
            )
            // 结果区开合（dbx 同款「收起 / 展开结果面板」）：收起后编辑器独占主区。
            // 状态由按钮档位体现 —— 收起时点亮（Primary）提醒「结果正藏着」，
            // 展开时融进底（Ghost，与旁边的选项按钮一致）。
            .child({
                let results_view = view.clone();
                let tip = if self.results_visible {
                    tr(cx, "隐藏查询结果")
                } else {
                    tr(cx, "显示查询结果")
                };
                icon_button_tip(
                    "db-toggle-results",
                    tr(cx, "查询结果"),
                    Icon::new(DuckIcon::PanelBottom).size(extended.icon.md),
                    tip,
                    if self.results_visible {
                        ButtonVariant::Ghost
                    } else {
                        ButtonVariant::Primary
                    },
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let _ = results_view.update(cx, |this, cx| {
                        this.results_visible = !this.results_visible;
                        cx.notify();
                    });
                })
            })
            // 输入框开合：与「结果区开合」配成一对，但**是另一条轴** —— 结果占满主区
            // 看数据（收起输入框）与让 SQL 占满主区（收起结果）是两种不同的需要。
            // 按钮语义写「要做的那件事」：开着时是「隐藏查询输入框」，收起后变「显示…」。
            .child({
                let editor_view = view.clone();
                let tip = if self.editor_visible {
                    tr(cx, "隐藏查询输入框")
                } else {
                    tr(cx, "显示查询输入框")
                };
                icon_button_tip(
                    "db-toggle-editor",
                    tr(cx, "查询输入框"),
                    Icon::new(DuckIcon::Rows3).size(extended.icon.md),
                    tip,
                    if self.editor_visible {
                        ButtonVariant::Ghost
                    } else {
                        ButtonVariant::Primary
                    },
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let _ = editor_view.update(cx, |this, cx| {
                        this.editor_visible = !this.editor_visible;
                        cx.notify();
                    });
                })
            })
            // 表浏览才有「重取当前页」这一说：SQL 结果的刷新就是重新执行。
            .when(
                self.mode == Mode::Table && self.selected_table.is_some(),
                |row| {
                    row.child(
                        icon_button(
                            "db-refresh-table",
                            tr(cx, "刷新"),
                            Icon::new(DuckIcon::RotateCw).size(extended.icon.md),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .disabled(self.busy)
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.refresh_table_page(cx));
                            }
                        }),
                    )
                },
            );

        // ── 右侧：连接下拉 + 库/Schema 下拉（dbx 同款）──
        // 「SQL 结果 / 表浏览」模式标签不再画：模式由结果区 tab 栏行首的对象名
        // 与图标自明，顶部再挂一个不可点的徽标是纯噪音（用户实测反馈）。
        let mut meta = h_flex().flex_none().items_center().gap(tokens.spacing.xs);
        // 单次覆盖生效时挂个标签：跑慢了才知道「不是连接默认在管」。
        if let Some(secs) = self.query_timeout_override {
            meta = meta.child(neutral_tag(
                SharedString::from(trf(cx, "超时 {} 秒", &[&secs.to_string()])),
                cx,
            ));
        }
        // 实例信息页没有结果集，标出**在看哪一档**（四个入口共用一块正文，
        // 不标的话用户切了档也认不出自己在看哪一页）。
        if let Mode::Instance(tab) = self.mode {
            meta = meta.child(neutral_tag(tab.heading(cx), cx));
        }
        // Tx 控件（dbx 同款三件套）：**档位徽标**（Tx:A = 自动提交、Tx:M = 手动事务，
        // 橙底高亮）+ **提交 ✓** + **回滚 ↺**。徽标可点：无事务时 = BEGIN、事务中 =
        // COMMIT（最常用的两个动作）。
        //
        // ⚠️ 提交 / 回滚图标**只在事务中渲染**（`if in_tx`），不是「渲染出来再置灰」：
        // 自动提交档位下没有可提交 / 可回滚的东西 —— 徽标自己写着「点一下开事务」，
        // 旁边再摆两个永远点不动的出口纯粹是噪音（用户实测反馈）。置灰态本来也骗人：
        // 「能不能点」要用户去猜为什么。
        //
        // 语句按**引擎**造（[`TxControl::statement`]）：MySQL 用 `START TRANSACTION`，
        // 其余用 `BEGIN`。造出来的语句仍由执行路径按 `tx_effect` 认状态，两者闭环 ——
        // 认不出来的话点一下 `in_tx` 不会变，界面就卡在旧态（内核有测试钉死这一点）。
        //
        // 摆在连接 / 库下拉**前面**（设计稿右组顺序 = Tx → 连接 → 库）。
        if let Some(session) = self.active_session() {
            let in_tx = session.in_tx;
            let kind = session.kind;
            let control = if in_tx {
                TxControl::Commit
            } else {
                TxControl::Begin
            };
            let badge_tip = SharedString::from(if in_tx {
                tr(cx, "手动事务 · 点击提交")
            } else {
                tr(cx, "自动提交 · 点击开启事务")
            });
            let badge_view = view.clone();
            meta = meta.child(
                div()
                    .id("db-tx-badge")
                    .flex_none()
                    .h(theme.density().control)
                    .px(tokens.spacing.sm)
                    .flex()
                    .items_center()
                    .rounded(tokens.radius.sm)
                    .cursor_pointer()
                    .text_size(tokens.typography.xs.size)
                    .font_weight(FontWeight::SEMIBOLD)
                    .bg(if in_tx {
                        extended.colors.warning
                    } else {
                        extended.colors.chrome
                    })
                    .text_color(if in_tx {
                        gpui::white()
                    } else {
                        tokens.colors.muted_foreground
                    })
                    .when(!in_tx, |badge| {
                        badge.hover(move |style| style.bg(extended.colors.hover_tint))
                    })
                    .tooltip(move |window, cx| Tooltip::new(badge_tip.clone()).build(window, cx))
                    .child(SharedString::from(if in_tx { "Tx: M" } else { "Tx: A" }))
                    .on_click(move |_, window, cx| {
                        let statement = control.statement(kind).to_owned();
                        let _ = badge_view.update(cx, |this, cx| {
                            this.execute_sql(statement, None, window, cx);
                        });
                    }),
            );
            // 提交 / 回滚只在**事务中**存在（自动提交档位下这两个出口没有意义）。
            if in_tx {
                let tx_view = view.clone();
                meta = meta.child(
                    icon_button_tip(
                        "db-tx-commit",
                        tr(cx, "提交事务"),
                        Icon::new(DuckIcon::Check)
                            .size(extended.icon.md)
                            .text_color(extended.colors.success),
                        tr(cx, "提交事务（COMMIT）"),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .disabled(self.busy)
                    .on_click(move |_, window, cx| {
                        let _ = tx_view.update(cx, |this, cx| {
                            this.execute_sql("COMMIT".to_owned(), None, window, cx);
                        });
                    }),
                );
                let rollback_view = view.clone();
                meta = meta.child(
                    icon_button_tip(
                        "db-tx-rollback",
                        tr(cx, "回滚事务"),
                        Icon::new(DuckIcon::RotateCcw)
                            .size(extended.icon.md)
                            .text_color(tokens.colors.destructive),
                        tr(cx, "回滚事务（ROLLBACK）"),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .disabled(self.busy)
                    .on_click(move |_, window, cx| {
                        let _ = rollback_view.update(cx, |this, cx| {
                            this.execute_sql("ROLLBACK".to_owned(), None, window, cx);
                        });
                    }),
                );
            }
        }
        if let Some(picker) = self.connection_picker(cx) {
            meta = meta.child(picker);
        }
        if let Some(picker) = self.namespace_picker(&self.toolbar_schema_select, cx) {
            meta = meta.child(picker);
        }

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            // 底部一条 hairline 把工具条与下面的 SQL 输入区分开（dbx 同款）：
            // 否则长 SQL 滚起来时按钮与首行文字糊在一起。
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(actions)
            // 中间只留弹性带：结果读数（N 行 · N 条语句 · 耗时）不再上顶部 ——
            // 摘要的「执行过程」与底部状态栏都在说这件事，三处重复（用户实测反馈）。
            // `status` 的操作反馈仍在底部状态栏与右上角通知，不丢。
            //
            // 唯一的例外是**执行 SQL 文件**的进度：那是分钟级的活，读数只在状态栏出现
            // 就太远；它在这条带上占据中段，跑完即消失（只在 `sql_file` 有值时渲染）。
            .child({
                let mut band = div().flex_1().min_w_0();
                if let Some(job) = self.sql_file.as_ref() {
                    // 总大小量不到（非普通文件）时只报条数，不编一个百分比出来。
                    let readout = if job.now.total_bytes == 0 {
                        trf(cx, "已执行 {} 条", &[&job.now.statements.to_string()])
                    } else {
                        let percent = (job.now.fraction() * 100.).round().to_string();
                        trf(
                            cx,
                            "{} 条 · {}%",
                            &[&job.now.statements.to_string(), &percent],
                        )
                    };
                    band = band.child(
                        h_flex()
                            .id("db-sql-file-progress")
                            .w_full()
                            .min_w_0()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .child(
                                Icon::new(DuckIcon::FileText)
                                    .size(extended.icon.sm)
                                    .text_color(extended.colors.accent_text),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(px(SQL_FILE_NAME_WIDTH))
                                    .truncate()
                                    .text_size(tokens.typography.sm.size)
                                    .line_height(tokens.typography.sm.line_height)
                                    .text_color(tokens.colors.foreground)
                                    .child(job.name.clone()),
                            )
                            .child(div().flex_1().min_w_0().child(progress_bar(
                                "db-sql-file-progress-bar",
                                job.now.fraction(),
                                cx,
                            )))
                            .child(
                                div()
                                    .flex_none()
                                    .font_features(tabular_numbers())
                                    .text_size(tokens.typography.xs.size)
                                    .line_height(tokens.typography.xs.line_height)
                                    .text_color(tokens.colors.muted_foreground)
                                    .child(SharedString::from(readout)),
                            ),
                    );
                }
                band
            })
            .child(meta)
            .into_any_element()
    }
}
