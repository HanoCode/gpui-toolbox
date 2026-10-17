//! 快速打开（`⌘P`）：一个输入框搜「连接 / 库 / 表 / 已保存查询」，回车直达。
//!
//! 与左栏那个「搜索连接与对象…」的分工：左栏是**过滤器**（只缩小树的范围，不动光标），
//! 这里是**跳转器**（选中即动作：建连 / 切库 / 开表 / 载入 SQL）。两者都留，因为
//! 「我想缩小范围看一眼」和「我已经知道要去哪」是两种不同的意图，混成一个框会两头不讨好。
//!
//! 三条口径：
//!
//! 1. **候选在打开那一刻拍一次**（[`DatabasePage::quick_open_items`]）：面板开着的时候
//!    后台可能在刷新内省，候选列表不该在手指底下重排（点下去打到别的东西上）。
//! 2. **只搜当前会话的库与表**：库名 / 表名跨连接会重名，列一条「sales 库」却不知道
//!    是哪台服务器上的，选中了也只能靠猜。跨连接请先搜连接本身。
//! 3. **Enter 的动作可回退**：建连 / 切库 / 开表都走既有入口，没有一条是「面板自己
//!    实现一遍」——所以 ⌘Z、标签、审计流水这些行为与从树里点进去完全一致。

use super::*;
use duck_components::command_palette::{palette_footer, palette_frame};

/// 快速打开面板的状态（页面字段 `quick_open`；`Some` = 面板开着）。
pub(super) struct QuickOpen {
    /// 搜索框。
    input: Entity<InputState>,
    /// 候选（打开时拍下）。
    items: Vec<QuickItem>,
    /// 高亮项：对**过滤后**的下标空间（不是 `items` 的下标）。
    active: usize,
    /// 面板自己的键盘落点：焦点在输入框里，Esc / ↑↓ 靠祖先冒泡到这里。
    focus: FocusHandle,
    /// 回车订阅（随面板一起 drop；收进页面的 `_subscriptions` 会随每次 ⌘P 长一条死条目）。
    _subscriptions: Vec<Subscription>,
}

/// 一条候选。
struct QuickItem {
    /// 主文案。
    label: String,
    /// 右侧灰字（「已连接」「表 · public」之类）。
    detail: SharedString,
    /// 行首图标。
    icon: DuckIcon,
    /// 小写化的匹配串（`label + detail`），打开时算一次 —— 每帧 `to_lowercase`
    /// 上千条会白烧一遍。
    haystack: String,
    /// 点中之后做什么。
    action: QuickAction,
}

/// 候选被选中后的动作。
///
/// 都是**既有入口**的转发（见模块注释第 3 条），这里不新造任何行为。
/// `pub(super)`：它的一个副本存在页面字段 [`DatabasePage::pending_quick_open`] 里
/// （订阅闭包没有窗口，只能先记意图、渲染入口再执行）。
#[derive(Clone)]
pub(super) enum QuickAction {
    /// 切到这条连接：已连接 = 切标签，未连接 = 立刻建连。
    OpenConnection(String),
    /// 切当前会话的命名空间（库 / schema 由引擎决定是哪一层）。
    OpenNamespace(String),
    /// 打开一张表。
    OpenTable { conn: String, table: TableRef },
    /// 载入一条已保存查询（只填编辑器，不执行）。`saved_id` 用来把当前标签与
    /// 该条目链接起来（保存写回同一条，见 [`DatabasePage::load_saved_query`]）。
    LoadQuery {
        sql: String,
        name: String,
        saved_id: String,
    },
}

impl DatabasePage {
    /// 打开快速打开面板（`⌘P`）。
    pub(super) fn open_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr(cx, "搜索连接、库、表、已保存查询…"))
        });
        // Enter 走输入框自己的提交事件：单行输入框会**消费** Enter，挂在卡片上的
        // `on_key_down` 收不到它（↑↓ / Esc 则不消费，仍会冒泡上去）。
        let subscriptions = vec![cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.arm_quick_open(cx);
            }
        })];
        let items = self.quick_open_items(cx);
        self.quick_open = Some(QuickOpen {
            input: input.clone(),
            items,
            active: 0,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        });
        window.focus(&input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// 关闭面板（不动作）。
    pub(super) fn close_quick_open(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_open.take().is_some() {
            cx.notify();
        }
    }

    /// 把「当前高亮项」记到 [`Self::pending_quick_open`]，由渲染入口带着 `&mut Window` 执行。
    ///
    /// 订阅闭包（Enter）拿不到窗口，而目标动作全都要它（开表要 load、载查询要 replace_all），
    /// 所以只在这里留个意图 —— 与 `pending_namespace_pick` 同一套办法。
    fn arm_quick_open(&mut self, cx: &mut Context<Self>) {
        let matches = self.quick_open_matches(cx);
        let Some(state) = self.quick_open.as_ref() else {
            return;
        };
        let Some(item) = matches
            .get(state.active)
            .and_then(|index| state.items.get(*index))
        else {
            return;
        };
        self.pending_quick_open = Some(item.action.clone());
        cx.notify();
    }

    /// 渲染入口取走 `pending_quick_open` 并执行（需要窗口）。
    pub(super) fn apply_pending_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.pending_quick_open.take() else {
            return;
        };
        self.quick_open_run(action, window, cx);
    }

    /// 执行一条动作：先关面板，再走既有入口。
    pub(super) fn quick_open_run(
        &mut self,
        action: QuickAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_quick_open(window, cx);
        match action {
            QuickAction::OpenConnection(id) => {
                if self.sessions.contains_key(&id) {
                    self.activate_conn(&id, window, cx);
                } else {
                    self.connect_entry(&id, window, cx);
                }
            }
            QuickAction::OpenNamespace(name) => {
                let layer_is_database = self
                    .active_session()
                    .is_some_and(|session| namespace_is_database(session.kind));
                if layer_is_database {
                    self.select_database(name, window, cx);
                } else {
                    self.select_schema(Some(name), cx);
                }
            }
            QuickAction::OpenTable { conn, table } => {
                // 表属于别的连接时先切过去：`open_table` 读的是**活动会话**的目录，
                // 不切就会在 A 连接上打开一张 B 连接的表名（查出来是错的或空的）。
                if self.active.as_deref() != Some(conn.as_str()) {
                    self.activate_conn(&conn, window, cx);
                }
                self.open_table(table, window, cx);
            }
            QuickAction::LoadQuery {
                sql,
                name,
                saved_id,
            } => {
                self.load_saved_query(sql, &name, &saved_id, window, cx);
            }
        }
        cx.notify();
    }

    /// 当前查询词命中的**候选下标**（按 `items` 的原序）。
    ///
    /// 多词是「与」：`订单 prod` 能命中「prod 库里的订单表」这种 label 与 detail 跨字段
    /// 的组合 —— 只按整串子串匹配的话，用户得先知道这两个词挨着才能搜到。
    pub(super) fn quick_open_matches(&self, cx: &App) -> Vec<usize> {
        let Some(state) = self.quick_open.as_ref() else {
            return Vec::new();
        };
        let query = state.input.read(cx).value().to_string().to_lowercase();
        let tokens: Vec<&str> = query.split_whitespace().collect();
        state
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| tokens.iter().all(|token| item.haystack.contains(token)))
            .map(|(index, _)| index)
            .collect()
    }

    /// 打开面板时的候选快照：连接（全部档案）→ 当前会话的库 / schema → 表 → 已保存查询。
    ///
    /// 排序就是「越常用越靠前」：连接在最上（跨连接是第一步），其次是库，再是表；
    /// 已保存查询放最后（它是「回到我写过的东西」，通常已经知道名字，直接打名字即可）。
    fn quick_open_items(&self, cx: &App) -> Vec<QuickItem> {
        let mut items = Vec::new();
        for config in &self.connections {
            let connected = self.sessions.contains_key(&config.id);
            let detail: SharedString = if connected {
                tr(cx, "已连接").into()
            } else {
                tr(cx, "未连接").into()
            };
            let label = config.display_name();
            items.push(QuickItem {
                haystack: format!("{label} {detail}").to_lowercase(),
                label,
                detail,
                icon: DuckIcon::Database,
                action: QuickAction::OpenConnection(config.id.clone()),
            });
        }
        if let Some(session) = self.active_session() {
            let namespaces = if namespace_is_database(session.kind) {
                &session.databases
            } else {
                &session.schemas
            };
            for name in namespaces {
                let detail: SharedString = tr(cx, "命名空间").into();
                items.push(QuickItem {
                    haystack: format!("{name} {detail}").to_lowercase(),
                    label: name.clone(),
                    detail,
                    icon: DuckIcon::Folders,
                    action: QuickAction::OpenNamespace(name.clone()),
                });
            }
            for table in &session.catalog {
                let kind: SharedString = match table.kind {
                    TableKind::Table => tr(cx, "表").into(),
                    TableKind::View => tr(cx, "视图").into(),
                    _ => tr(cx, "对象").into(),
                };
                let detail: SharedString = match &table.schema {
                    Some(schema) if !schema.is_empty() => {
                        SharedString::from(format!("{kind} · {schema}"))
                    }
                    _ => kind,
                };
                items.push(QuickItem {
                    haystack: format!("{} {detail}", table.name).to_lowercase(),
                    label: table.name.clone(),
                    detail,
                    icon: DuckIcon::Table,
                    action: QuickAction::OpenTable {
                        conn: self.active.clone().unwrap_or_default(),
                        table: table.table_ref(),
                    },
                });
            }
        }
        for saved in &self.saved_queries {
            let detail: SharedString = tr(cx, "已保存查询").into();
            items.push(QuickItem {
                haystack: format!("{} {detail}", saved.name).to_lowercase(),
                label: saved.name.clone(),
                detail,
                icon: DuckIcon::FileText,
                action: QuickAction::LoadQuery {
                    sql: saved.sql.clone(),
                    name: saved.name.clone(),
                    saved_id: saved.id.clone(),
                },
            });
        }
        items
    }

    /// 浮层本体（遮罩 + 卡片）。面板没开时返回 `None`。
    pub(super) fn render_quick_open(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let state = self.quick_open.as_ref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let matches = self.quick_open_matches(cx);
        // 高亮夹到命中范围内：输入继续变长时命中会变少，旧下标不能越界。
        let active = state.active.min(matches.len().saturating_sub(1));
        let view = self.view.clone();
        let hover = extended.colors.hover_tint;
        let selected = extended.colors.nav_selected;

        let rows = matches
            .iter()
            .take(QUICK_OPEN_MAX_ROWS)
            .enumerate()
            .map(|(position, index)| {
                let Some(item) = state.items.get(*index) else {
                    return div().into_any_element();
                };
                let action = item.action.clone();
                let page = view.clone();
                let is_active = position == active;
                div()
                    .id(SharedString::from(format!("db-quick-{position}")))
                    .flex()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.md)
                    .py(tokens.spacing.xs)
                    .rounded(tokens.radius.md)
                    .cursor_pointer()
                    .when(is_active, |row| row.bg(selected))
                    .when(!is_active, |row| row.hover(move |style| style.bg(hover)))
                    .on_click(move |_, window, cx| {
                        let action = action.clone();
                        let _ = page.update(cx, |this, cx| {
                            this.quick_open_run(action.clone(), window, cx);
                        });
                    })
                    .child(
                        Icon::new(item.icon)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .text_color(tokens.colors.foreground)
                            .child(item.label.clone()),
                    )
                    .children(
                        (!item.detail.is_empty())
                            .then(|| meta_text(item.detail.clone(), cx).min_w_0().truncate()),
                    )
                    .into_any_element()
            });

        let empty_hint = matches.is_empty().then(|| {
            div()
                .px(tokens.spacing.md)
                .py(tokens.spacing.sm)
                .text_size(tokens.typography.xs.size)
                .text_color(extended.colors.text_tertiary)
                .child(tr(cx, "没有匹配的项"))
                .into_any_element()
        });

        let card = v_flex()
            .w_full()
            .child(
                h_flex()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .px(tokens.spacing.md)
                    .py(tokens.spacing.sm)
                    .border_b_1()
                    .border_color(extended.colors.hairline)
                    .child(
                        Icon::new(DuckIcon::Search)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&state.input)
                                .appearance(false)
                                .with_size(Size::Small)
                                .w_full(),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .id("db-quick-list")
                    .max_h(px(QUICK_OPEN_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .p(tokens.spacing.xs)
                    .gap(tokens.spacing.xxs)
                    .children(empty_hint)
                    .children(rows),
            )
            .child(palette_footer(matches.len(), cx));

        let dismiss_view = self.view.clone();
        Some(palette_frame(
            card.track_focus(&state.focus).on_key_down({
                let key_view = self.view.clone();
                move |event: &gpui::KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    if key == "escape" {
                        cx.stop_propagation();
                        let _ = key_view.update(cx, |this, cx| this.close_quick_open(window, cx));
                        return;
                    }
                    // Tab 也算上下（多一个不离开输入框就能挪的方法）；Shift 反向。
                    if !matches!(key, "up" | "down" | "tab") {
                        return;
                    }
                    let down = key == "down" || (key == "tab" && !event.keystroke.modifiers.shift);
                    cx.stop_propagation();
                    let _ = key_view.update(cx, |this, cx| {
                        let len = this.quick_open_matches(cx).len();
                        if len == 0 {
                            return;
                        }
                        if let Some(state) = this.quick_open.as_mut() {
                            state.active = if down {
                                (state.active + 1) % len
                            } else {
                                (state.active + len - 1) % len
                            };
                        }
                        cx.notify();
                    });
                }
            }),
            move |window, cx| {
                let _ = dismiss_view.update(cx, |this, cx| this.close_quick_open(window, cx));
            },
            cx,
        ))
    }
}

/// 面板一次最多画多少行（命中的其余部分靠滚动看）。与 `QUICK_OPEN_MAX_HEIGHT`
/// 一起把浮层高度钉在「一屏内」——列表再长，卡片也不该长到盖住整个窗口。
const QUICK_OPEN_MAX_ROWS: usize = 200;
/// 列表区最大高度（像素；滚动条由 `overflow_y_scroll` 给）。
const QUICK_OPEN_MAX_HEIGHT: f32 = 340.;
