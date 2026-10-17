//! 标签条与「历史」抽屉：主区顶部的连接标签、右键菜单（关其他 / 关右侧 / 关全部），
//! 以及右栏历史分段的语句列表与分页。
//!
//! 标签的**状态**在 `session.rs`（开关策略收敛到 `remove_tab`），这里只管画。
//!
//! 本文件是 `impl DatabasePage` 的一个切片，方法定义在子模块后每个都标 `pub(super)`
//!（与 `pages::terminal` 同一口径）；类型定义仍在 `mod.rs`。

use super::*;
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

/// 标签拖拽重排的载荷：只带标签 id（**不带下标**）。
///
/// 带下标会在拖动过程中失效（前面的标签被移走，下标就变了）；按 id 现查位置，
/// 与结构编辑器行重排同一条口径（`StructureRowDrag`）。
#[derive(Clone)]
struct TabDrag {
    id: String,
}

impl Render for TabDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // 跟手的是光标形状，不需要拖影（与列宽把手、结构行重排一致）。
        gpui::Empty
    }
}

impl DatabasePage {
    /// 主区顶部的连接标签条。
    pub(super) fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        // 基础标题（数据标签 `表名@库` / 库标签库名 / 查询标签连接名）先算齐，再做重名去重：
        // 同名标签补 ` n` 后缀——「两张同名表分属不同库」「同一连接开两个查询」都会撞名，
        // 不补后缀就分不清点的是哪一个。
        let base_titles: Vec<(String, String)> = self
            .tabs
            .iter()
            .filter_map(|tab_id| {
                self.tab_base_title(tab_id, cx)
                    .map(|title| (tab_id.clone(), title))
            })
            .collect();
        let deduped = dedupe_titles(
            &base_titles
                .iter()
                .map(|(_, title)| title.clone())
                .collect::<Vec<_>>(),
        );
        let titles: HashMap<String, String> = base_titles
            .into_iter()
            .map(|(tab_id, _)| tab_id)
            .zip(deduped)
            .collect();
        let tab_count = self.tabs.len();

        // 右键菜单文案：菜单闭包是 `'static`（拿不到 `&mut Context<Self>`），先取好再进闭包。
        let lbl_close = tr(cx, "关闭");
        let lbl_close_others = tr(cx, "关闭其他");
        let lbl_close_right = tr(cx, "关闭右侧");
        let lbl_close_all = tr(cx, "关闭全部");
        let lbl_copy_name = tr(cx, "复制名称");
        let note_copied_name = tr(cx, "已复制名称。");

        // 标签多到放不下时**横向滚动**（`overflow_x_scroll` 前必须先 `.id(...)`，
        // 与结果区横向滚动 `db-result-columns-x` 同一先例）；`restrict_scroll_to_axis`
        // 防止标签条把纵向滚轮也吞掉。滚动条本体由外层相对容器叠加（下方）——
        // gpui 不画系统滚动条，不叠 `Scrollbar` 用户就没有可拖动的东西。
        let mut row = h_flex()
            .id("db-tab-strip")
            .flex_1()
            .min_w_0()
            .gap(tokens.spacing.xxs)
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.tab_strip_x_scroll);
        for (index, tab_id) in self.tabs.iter().enumerate() {
            // 标签元素是**标签 id**，经 `tab_conn` 找到所属连接再取展示信息。
            let Some(conn_id) = self.tab_conn.get(tab_id) else {
                continue;
            };
            let Some(cfg) = self.connections.iter().find(|cfg| &cfg.id == conn_id) else {
                continue;
            };
            let title = titles
                .get(tab_id)
                .cloned()
                .unwrap_or_else(|| cfg.display_name().to_string());
            let active = self.active_tab.as_deref() == Some(tab_id.as_str());
            let is_last = index + 1 >= tab_count;
            let (foreground, weight) = if active {
                (tokens.colors.foreground, FontWeight::MEDIUM)
            } else {
                (tokens.colors.muted_foreground, FontWeight::NORMAL)
            };
            let close = toolbar_action_button(
                SharedString::from(format!("db-tab-close-{tab_id}")),
                tr(cx, "关闭标签"),
                // 标签页内嵌的关闭 ✕ 有意比 `icon.md` 小一档（三个页面的标签条都取 sm）；
                // 代价是按钮 26×28 不方 —— 与终端标签条同款，已知并接受。
                Icon::new(DuckIcon::X).size(extended.icon.sm),
                false,
                false,
                cx,
            )
            .on_click({
                let view = view.clone();
                let tab_id = tab_id.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.close_tab(&tab_id, window, cx));
                }
            });
            row = row.child(
                h_flex()
                    .id(SharedString::from(format!("db-tab-{tab_id}")))
                    .h(theme.density().control)
                    .min_w_0()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .px(tokens.spacing.sm)
                    .rounded(tokens.radius.md)
                    .cursor_pointer()
                    .text_color(foreground)
                    .text_size(tokens.typography.sm.size)
                    .font_weight(weight)
                    .role(Role::Tab)
                    .aria_selected(active)
                    .when(active, |tab| tab.bg(tokens.colors.surface))
                    .hover(move |style| {
                        style
                            .bg(extended.colors.hover_tint)
                            .text_color(extended.colors.accent_text)
                    })
                    .on_click({
                        let view = view.clone();
                        let tab_id = tab_id.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.activate(&tab_id, window, cx));
                        }
                    })
                    // 拖拽重排：拖到哪张标签上就插到它的位置。载荷只带 id ——
                    // 拖动过程中下标会变，按 id 现查才稳。
                    .on_drag(TabDrag { id: tab_id.clone() }, |drag, _, _, cx| {
                        // 别让这次按下再冒泡成「激活标签」。
                        cx.stop_propagation();
                        cx.new(|_| drag.clone())
                    })
                    .on_drag_move::<TabDrag>({
                        let view = view.clone();
                        move |event, _, cx| {
                            let dragged = event.drag(cx).id.clone();
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.move_tab_to(&dragged, index, cx);
                            });
                        }
                    })
                    // 中键关闭（浏览器 / 编辑器通用手势），语义与点 ✕ 一致。
                    .on_mouse_down(MouseButton::Middle, {
                        let view = view.clone();
                        let tab_id = tab_id.clone();
                        move |_, window, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| this.close_tab(&tab_id, window, cx));
                        }
                    })
                    // 引擎品牌 logo（全彩）：查询标签保持主题中性色，靠 logo 辨识引擎。
                    // 数据 / 库标签改用对象图标（表 / 库），标题也换成对象名 —— dbx 同款
                    // 「表名@库」：点过的表各自一张标签，标题直说这是谁。
                    .child(match &self.tab_kind.get(tab_id) {
                        Some(TabKind::Data { .. }) => Icon::new(table_kind_icon(TableKind::Table))
                            .size(extended.icon.sm)
                            .into_any_element(),
                        Some(TabKind::Database { .. }) => Icon::new(DuckIcon::Database)
                            .size(extended.icon.sm)
                            .into_any_element(),
                        Some(TabKind::Instance(tab)) => Icon::new(instance_tab_icon(*tab))
                            .size(extended.icon.sm)
                            .into_any_element(),
                        // 工具页：ER 关系图取「网络」形。
                        Some(TabKind::Er { .. }) => Icon::new(DuckIcon::Network)
                            .size(extended.icon.sm)
                            .into_any_element(),
                        Some(TabKind::Dashboard(_)) => Icon::new(DuckIcon::Columns2)
                            .size(extended.icon.sm)
                            .into_any_element(),
                        _ => engine_logo(EngineRef::from_config(cfg), extended.icon.sm, cx),
                    })
                    .child(div().min_w_0().truncate().child(title.clone()))
                    .child(close)
                    // 标签右键菜单：关闭 / 关闭其它 / 关闭右侧 / 全部关闭 / 复制名称。
                    // 关闭动作都走 `close_tab`——「某连接的最后一张标签」等价于断开该连接，
                    // 与点 ✕ 的语义保持一致（不在这里另造一套）。
                    .context_menu({
                        let view = view.clone();
                        let tab_id = tab_id.clone();
                        let title = title.clone();
                        move |menu, _, _| {
                            let close_view = view.clone();
                            let close_id = tab_id.clone();
                            let others_view = view.clone();
                            let others_id = tab_id.clone();
                            let right_view = view.clone();
                            let right_id = tab_id.clone();
                            let all_view = view.clone();
                            let copy_view = view.clone();
                            let copy_title = title.clone();
                            menu.item(PopupMenuItem::new(lbl_close).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = close_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.close_tab(&close_id, window, cx);
                                    });
                                },
                            ))
                            .item(
                                PopupMenuItem::new(lbl_close_others)
                                    .disabled(tab_count <= 1)
                                    .on_click(move |_, window, cx| {
                                        let Some(entity) = others_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.close_other_tabs(&others_id, window, cx);
                                        });
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(lbl_close_right)
                                    .disabled(is_last)
                                    .on_click(move |_, window, cx| {
                                        let Some(entity) = right_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.close_tabs_right_of(&right_id, window, cx);
                                        });
                                    }),
                            )
                            .item(PopupMenuItem::separator())
                            .item(PopupMenuItem::new(lbl_close_all).on_click(
                                move |_, window, cx| {
                                    let Some(entity) = all_view.upgrade() else {
                                        return;
                                    };
                                    entity.update(cx, |this, cx| {
                                        this.close_all_tabs(window, cx);
                                    });
                                },
                            ))
                            .item(
                                PopupMenuItem::new(lbl_copy_name).on_click(move |_, _, cx| {
                                    let Some(entity) = copy_view.upgrade() else {
                                        return;
                                    };
                                    let title = copy_title.clone();
                                    entity.update(cx, |this, cx| {
                                        this.put_on_clipboard(title, note_copied_name, cx);
                                    });
                                }),
                            )
                        }
                    }),
            );
        }

        h_flex()
            .w_full()
            .h(theme.density().control + tokens.spacing.sm)
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            // 标签条宿主：相对容器承托滚动条轨道（`grid.rs` 结果网格同款形态）。
            // **悬停才现身**（用户实测反馈：常驻太吵）——拖到标签条上滚动条才出现。
            .child(
                h_flex().flex_1().min_w_0().relative().child(row).child(
                    div().absolute().inset_0().child(
                        Scrollbar::horizontal(&self.tab_strip_x_scroll)
                            .id("db-tab-strip-x-scrollbar")
                            .mode(ScrollbarMode::Hover)
                            .viewport_from_layout(),
                    ),
                ),
            )
            .child({
                // 「+」：在当前连接上再开一个空白查询标签（dbx 同款）——同一连接
                // 可以并排多个标签，各拿各的 SQL 与结果区。没有活跃会话时禁用。
                icon_button(
                    "db-tab-new",
                    tr(cx, "新建查询标签"),
                    Icon::new(DuckIcon::Plus).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.active_session().is_none())
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if let Some(conn) = this.active.clone() {
                                this.open_tab(&conn, window, cx);
                            }
                        });
                    }
                })
            })
            .child({
                // 表格 / 图表切换：只要**有结果**就给入口。画不出图（没有数值列）时不在这里
                // 预先禁用——那会让用户对着灰按钮猜原因；点了由 `toggle_chart_view` 说明。
                let has_result = self.current_result().is_some();
                let on_chart = self.chart_view.is_some();
                toolbar_action_button(
                    "db-toggle-chart",
                    if on_chart {
                        tr(cx, "看表格")
                    } else {
                        tr(cx, "看图表")
                    },
                    Icon::new(if on_chart {
                        DuckIcon::Table
                    } else {
                        DuckIcon::ChartLine
                    })
                    .size(extended.icon.md),
                    on_chart,
                    !has_result,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.toggle_chart_view(window, cx));
                    }
                })
            })
            .into_any_element()
    }

    /// 右栏「历史」分段：`最近`（自动攒的执行记录）/ `已保存`（用户存下的查询）。
    ///
    /// 两栏都是**点一下填编辑器、不执行**——与左栏「已保存查询」同一口径
    /// （写动作一律只填编辑器，见 [`Self::fill_editor`] / [`Self::load_saved_query`]）：
    /// 用户总有机会先改再跑。
    ///
    /// 最近那栏只放**执行过**的语句：编辑器里正在敲的半截内容不算历史。同一句重复执行，
    /// [`push_history`] 只留一条并置顶，否则历史会被同一句刷满。
    pub(super) fn render_history_dock(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let hover = extended.colors.hover_tint;

        // ── 子栏切换（最近 / 已保存）──
        //
        // 凹槽底色用 `row_hover`、选中项用 `surface`：「选中项浮在槽上」的语义与设计稿
        // 一致，也不引入第二套中性色（设计稿的 `--surface2` 在本仓没有对应 token）。
        //
        // 用到的 token 先取成局部 `Copy` 值再进闭包：闭包是 `move`，直接捕获
        // `theme` / `tokens` 会把它们搬走，闭包外（header / 列表）就没得用了。
        let seg_height = theme.density().control;
        let seg_radius = tokens.radius.sm;
        let seg_back = tokens.radius.md;
        let seg_gap = tokens.spacing.xxs;
        let seg_text = tokens.typography.xs.size;
        let seg_fg = tokens.colors.foreground;
        let seg_muted = tokens.colors.muted_foreground;
        let seg_surface = tokens.colors.surface;
        let segment = {
            // 闭包是 `Fn`（两个子栏各调一次）：句柄在外面克隆一份独占，每次调用再克隆
            // 给内层 `move` 闭包。
            let view = view.clone();
            move |label: &'static str, target: HistorySegment, active: bool| {
                let view = view.clone();
                div()
                    .id(SharedString::from(format!("db-history-seg-{label}")))
                    .flex_1()
                    .min_w_0()
                    .h(seg_height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(seg_radius)
                    .cursor_pointer()
                    .text_size(seg_text)
                    .font_weight(if active {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if active { seg_fg } else { seg_muted })
                    .when(active, |el| el.bg(seg_surface))
                    .hover(move |style| style.bg(hover))
                    .role(Role::Tab)
                    .aria_selected(active)
                    .aria_label(SharedString::from(label))
                    .on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.history_segment = target;
                            cx.notify();
                        });
                    })
                    .child(label)
            }
        };

        let mut header = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(seg_gap)
                    .p(seg_gap)
                    .rounded(seg_back)
                    .bg(extended.colors.row_hover)
                    .child(segment(
                        tr(cx, "最近"),
                        HistorySegment::Recent,
                        self.history_segment == HistorySegment::Recent,
                    ))
                    .child(segment(
                        tr(cx, "已保存"),
                        HistorySegment::Saved,
                        self.history_segment == HistorySegment::Saved,
                    )),
            );
        // 「清空」只给最近那栏：已保存是用户自己攒下的东西，删除走条目右键（与左栏同款），
        // 不该摆一个「一键删光我存过的全部查询」的入口。
        if self.history_segment == HistorySegment::Recent && !self.history.is_empty() {
            header = header.child(
                button("db-history-clear", tr(cx, "清空"), ButtonVariant::Ghost, cx).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.history.clear();
                            this.persist_history(cx);
                            this.status = tr(cx, "查询历史已清空。").into();
                            cx.notify();
                        });
                    }
                }),
            );
        }

        // ── 列表 ──
        let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
        match self.history_segment {
            HistorySegment::Recent => {
                if self.history.is_empty() {
                    list = list.child(meta_text(tr(cx, "还没有执行过语句。"), cx));
                }
                for (index, sql) in self.history.iter().enumerate() {
                    let card_view = view.clone();
                    let statement = sql.clone();
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("db-history-{index}")))
                            .w_full()
                            .min_w_0()
                            .px(tokens.spacing.sm)
                            .py(tokens.spacing.xs)
                            .rounded(tokens.radius.md)
                            .cursor_pointer()
                            .role(Role::Button)
                            .aria_label(SharedString::from(trf(
                                cx,
                                "把第 {} 条历史放回编辑器",
                                &[&(index + 1).to_string()],
                            )))
                            .text_size(tokens.typography.mono_md.size)
                            .text_color(tokens.colors.foreground)
                            .hover(move |style| style.bg(hover))
                            .on_click(move |_, window, cx| {
                                let Some(entity) = card_view.upgrade() else {
                                    return;
                                };
                                let sql = statement.clone();
                                entity.update(cx, |this, cx| {
                                    this.fill_editor(sql, window, cx);
                                    this.status = tr(cx, "已从历史放回编辑器。").into();
                                    cx.notify();
                                });
                            })
                            // 多行 SQL 折成一行展示：列表是「认句子」用的，不是阅读器。
                            .child(div().truncate().child(selectable_text(SharedString::from(
                                sql.split_whitespace().collect::<Vec<_>>().join(" "),
                            )))),
                    );
                }
            }
            HistorySegment::Saved => {
                if self.saved_queries.is_empty() {
                    list = list.child(meta_text(tr(cx, "还没有保存的查询。"), cx));
                }
                for saved in &self.saved_queries {
                    // 显示文本与两个闭包各持一份：`move` 闭包会把捕获值搬走，
                    // 共用一份第二个就没得用了（E0382）。
                    let id = saved.id.clone();
                    let name = saved.name.clone();
                    let sql = saved.sql.clone();
                    let click_view = view.clone();
                    let menu_view = view.clone();
                    let card_id = id.clone();
                    let display_name = name.clone();
                    let display_sql = sql.clone();
                    let click_sql = sql.clone();
                    let click_name = name.clone();
                    let menu_sql = sql;
                    let menu_name = name;
                    let load_label = tr(cx, "载入到编辑器");
                    let delete_label = tr(cx, "删除");
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("db-dock-saved-{card_id}")))
                            .w_full()
                            .min_w_0()
                            .px(tokens.spacing.sm)
                            .py(tokens.spacing.xs)
                            .rounded(tokens.radius.md)
                            .cursor_pointer()
                            .role(Role::Button)
                            .aria_label(SharedString::from(load_label))
                            .hover(move |style| style.bg(hover))
                            .on_click(move |_, window, cx| {
                                let Some(entity) = click_view.upgrade() else {
                                    return;
                                };
                                let sql = click_sql.clone();
                                let name = click_name.clone();
                                let saved_id = card_id.clone();
                                entity.update(cx, |this, cx| {
                                    this.load_saved_query(sql, &name, &saved_id, window, cx);
                                });
                            })
                            // 右键只放「载入」与「删除」：重命名是低频维护动作，留在左栏
                            // 那棵树里；两处各画一套菜单，改一处忘一处就是两套行为。
                            .context_menu(move |menu, _, _| {
                                let load_view = menu_view.clone();
                                let delete_view = menu_view.clone();
                                let load_sql = menu_sql.clone();
                                let load_name = menu_name.clone();
                                let load_id = id.clone();
                                let delete_id = id.clone();
                                let delete_name = menu_name.clone();
                                menu.item(PopupMenuItem::new(load_label).on_click(
                                    move |_, window, cx| {
                                        let Some(entity) = load_view.upgrade() else {
                                            return;
                                        };
                                        entity.update(cx, |this, cx| {
                                            this.load_saved_query(
                                                load_sql.clone(),
                                                &load_name,
                                                &load_id,
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ))
                                .item(PopupMenuItem::separator())
                                .item(
                                    PopupMenuItem::new(delete_label).on_click(
                                        move |_, window, cx| {
                                            let Some(entity) = delete_view.upgrade() else {
                                                return;
                                            };
                                            entity.update(cx, |this, cx| {
                                                this.delete_saved_query(
                                                    &delete_id,
                                                    &delete_name,
                                                    window,
                                                    cx,
                                                );
                                            });
                                        },
                                    ),
                                )
                            })
                            .child(
                                v_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap(tokens.spacing.xxs)
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(tokens.typography.sm.size)
                                            .child(selectable_text(SharedString::from(
                                                display_name,
                                            ))),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(tokens.typography.mono_md.size)
                                            .text_color(tokens.colors.muted_foreground)
                                            .child(selectable_text(SharedString::from(
                                                display_sql
                                                    .split_whitespace()
                                                    .collect::<Vec<_>>()
                                                    .join(" "),
                                            ))),
                                    ),
                            ),
                    );
                }
            }
        }

        v_flex()
            .w_full()
            .gap(tokens.spacing.md)
            .child(header)
            // 右栏整体已经 `overflow_y_scroll`（见 [`Self::render_right_sidebar`]）：
            // 列表自己不再设高度上限，也不再套一层滚动区。
            .child(list)
            .into_any_element()
    }
}
