//! SQL 编辑器的**导航层**：跳行（`⌘⌥G`）、Cmd+点击表名跳转、悬停看表结构。
//!
//! 三件事全部挂在编辑器内核**已有**的通道上，不自造一套命中测试：
//!
//! - **跳行**：`InputBaseState::set_selected_range` 内部走 `move_to` → `scroll_to`，
//!   所以「行号 → 字节偏移」之后不必再碰滚动句柄。
//! - **Cmd+点击表名**：`gpui-base` 只在 **Cmd+悬停** 时问 `DefinitionProvider`
//!   （见 `input/editor/lsp/hover.rs`），命中后编辑器给符号画下划线 + 挂可点 hitbox；
//!   落地那一步走 `show_document` 钩子 —— 这是宿主唯一能在「跳转」上插手的口子
//!   （`GoToDefinition` 的默认行为是**在本文件内**移光标，对表名毫无意义）。
//!   落点是那张表的**结构**标签：列 / 索引 / DDL，与「字段详情」是同一件事。
//! - **悬停看表结构**：普通悬停问 `HoverProvider`，结果由 `gpui-component` 的
//!   `HoverPopover` 渲染成一张 markdown 卡片 —— 卡片不用自己画，连定位都不用管。
//!
//! 与「快速打开」的分工：那边是「先想找什么，再跳」，这边是「盯着代码里的名字，
//! 就地跳过去」。两边共用同一份目录（`Session::catalog`），没有第二套表名索引。
//!
//! 本文件是 `impl DatabasePage` 的一个切片 + 三个 trait 实现，方法定义在子模块后
//! 每个都标 `pub(super)`；类型定义留在本文件（与 `csv_import` / `quick_open` 同口径）。

use std::cell::RefCell;

use super::*;
use gpui_component::input::{DefinitionProvider, HoverProvider, RopeExt as _, ShowDocumentHandler};

/// 跳行浮层卡片宽度（够放四位数行号 + 一句提示，不至于摆成一个大弹窗）。
const GOTO_LINE_WIDTH: f32 = 320.;

/// 悬停预览最多列多少列（几十列的表得有个头，卡片的用处是「这是不是我要那张」）。
const HOVER_COLUMN_CAP: usize = 24;

/// 跳行浮层：一个输入框，回车把光标送到那一行。
///
/// 没有独立视图实体：整层由页面按 [`DatabasePage::goto_line`] 决定画不画，与 CSV
/// 导入向导同一种做法（浮层在页面上、不在窗口弹窗层 —— 页面 `cx.notify()` 就能
/// 带动它重绘）。
pub(super) struct GotoLine {
    /// 行号输入框。
    input: Entity<InputState>,
    /// 回车 / 失焦两条订阅（随浮层一起 drop）。
    _subscriptions: Vec<Subscription>,
}

/// 编辑器导航的 LSP 侧：表名解析 + 跳转回执 + 悬停预览。
///
/// 一个实例同时当 `DefinitionProvider` / `HoverProvider` 用（同一个 `Rc` 克隆三分），
/// 于是「这是哪张表」的判定只有一份代码 —— 悬停看到的和点下去跳到的必然是同一张。
pub(super) struct SqlNavigator {
    /// 自引用：解析要读当前会话的目录。
    view: WeakEntity<DatabasePage>,
    /// 跳转目标。
    ///
    /// 为什么不把表名编进 `LocationLink` 的 URI：`lsp_types::Url` 只接受合法 URL，
    /// 表名里可能有空格 / `#` / `?`，编码再解码要另写一层易错的转义；而
    /// 「悬停求位置 → 点击」是同一段交互的两次回调，中间没有别的写入者
    ///（`hover_definition.is_same(offset)` 钉住了「点的就是高亮着的那一个符号」）。
    target: Rc<RefCell<Option<TableRef>>>,
    /// 悬停预览的列缓存（键 = 小写限定名）。
    ///
    /// 悬停会在鼠标划过时反复触发，每次都往数据库问一遍列元信息是白花的往返；
    /// 表结构在一次会话里几乎不变，缓存到会话结束即可（`Rc` 共享，后台任务写回来
    /// 的也是同一张表）。
    columns: Rc<RefCell<HashMap<String, Vec<Column>>>>,
}

impl SqlNavigator {
    pub(super) fn new(view: WeakEntity<DatabasePage>) -> Self {
        Self {
            view,
            target: Rc::new(RefCell::new(None)),
            columns: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// 跳转目标的共享句柄（交给 [`jump_handler`]）。
    pub(super) fn target(&self) -> Rc<RefCell<Option<TableRef>>> {
        self.target.clone()
    }

    /// 光标下的表：`(符号字节范围, 表)`。
    fn table_at(
        &self,
        text: &Rope,
        offset: usize,
        cx: &App,
    ) -> Option<(std::ops::Range<usize>, TableRef)> {
        let source = text.to_string();
        let (range, path) = identifier_path_at(&source, offset)?;
        let page = self.view.upgrade()?;
        let table = page.read(cx).resolve_table_path(&path)?;
        Some((range, table))
    }

    /// 把字节范围翻成 LSP 的 `(行, 列)`。
    ///
    /// 编辑器拿到 `origin_selection_range` 之后会用**同一个** `RopeExt` 反算回字节，
    /// 所以这里必须与内核的列口径一致（`character` = 字符数，不是 UTF-16 码元）——
    /// 自己按码元另算一套，会在中文表名上把下划线画歪。
    fn range_to_lsp(&self, text: &Rope, range: std::ops::Range<usize>) -> lsp_types::Range {
        lsp_types::Range::new(
            text.offset_to_position(range.start),
            text.offset_to_position(range.end),
        )
    }
}

impl DefinitionProvider for SqlNavigator {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<lsp_types::LocationLink>>> {
        let Some((range, table)) = self.table_at(text, offset, cx) else {
            return Task::ready(Ok(Vec::new()));
        };
        // 回执里只要有个合法 URL 就够：真正的去处由 `show_document` 接。
        let Ok(uri) = "duck-table://jump".parse::<lsp_types::Uri>() else {
            return Task::ready(Ok(Vec::new()));
        };
        let range = self.range_to_lsp(text, range);
        *self.target.borrow_mut() = Some(table);
        Task::ready(Ok(vec![lsp_types::LocationLink {
            origin_selection_range: Some(range),
            target_uri: uri,
            target_range: range,
            target_selection_range: range,
        }]))
    }
}

impl HoverProvider for SqlNavigator {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Option<lsp_types::Hover>>> {
        let Some((range, table)) = self.table_at(text, offset, cx) else {
            return Task::ready(Ok(None));
        };
        let range = self.range_to_lsp(text, range);
        let key = table.display().to_lowercase();
        if let Some(columns) = self.columns.borrow().get(&key) {
            let hover = hover_card(cx, &table, columns, range);
            return Task::ready(Ok(hover));
        }
        let Some(page) = self.view.upgrade() else {
            return Task::ready(Ok(None));
        };
        let Some(conn) = page.read(cx).active_conn() else {
            return Task::ready(Ok(None));
        };
        let fetch = cx.background_executor().spawn({
            let table = table.clone();
            async move {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.table_columns(&table)
            }
        });
        let cache = self.columns.clone();
        cx.spawn(async move |cx| {
            let columns = fetch.await.map_err(|error| anyhow!(error.to_string()))?;
            cache.borrow_mut().insert(key, columns.clone());
            // 卡片正文要取译文，得回到前台（`&App` 只在页面上下文里拿得到）。
            let hover = page.update(cx, |_, cx| hover_card(cx, &table, &columns, range));
            Ok(hover)
        })
    }
}

/// 悬停卡片的正文：表名 + 列数 + 一列一行（名 / 类型 / 注释）。
///
/// 用「行内代码 + 空格分隔」而不是 markdown 表格：popover 只有两百来像素宽，表格里
/// 长类型名（`character varying(255)`）会被挤成锯齿，一行一列反而好扫。
fn hover_card(
    cx: &App,
    table: &TableRef,
    columns: &[Column],
    range: lsp_types::Range,
) -> Option<lsp_types::Hover> {
    let mut body = format!(
        "**{}** · {}",
        table.display(),
        trf(cx, "{} 列", &[&columns.len().to_string()])
    );
    if !columns.is_empty() {
        body.push_str("\n\n");
        for column in columns.iter().take(HOVER_COLUMN_CAP) {
            body.push_str(&format!("`{}`", column.name));
            if !column.type_name.is_empty() {
                body.push(' ');
                body.push_str(&column.type_name);
            }
            if !column.comment.is_empty() {
                body.push_str(" · ");
                body.push_str(&column.comment);
            }
            body.push('\n');
        }
        if columns.len() > HOVER_COLUMN_CAP {
            body.push_str(&trf(
                cx,
                "…… 还有 {} 列",
                &[&(columns.len() - HOVER_COLUMN_CAP).to_string()],
            ));
            body.push('\n');
        }
        body.push('\n');
        body.push_str(&trf(cx, "{} 点击打开这张表", &[modifier_symbol()]));
    }
    Some(lsp_types::Hover {
        contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: body,
        }),
        range: Some(range),
    })
}

/// 主修饰键在**给人看**的文案里的写法。
///
/// `duck_command::primary_modifier` 给的是 gpui 键位串里的名字（`cmd` / `ctrl`），
/// 直接摆到界面上没人认得，所以这里另给一份符号。
fn modifier_symbol() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    }
}

/// `show_document` 钩子：把「跳表」这件事接到页面上。
///
/// 落点是**结构标签**（列 / 索引 / 外键 / DDL），不是数据浏览：在 SQL 里盯着一
/// 张表点下去，问的是「这张表长什么样」；翻数据是左栏点表的语义（`open_table`），
/// 两个入口不该都挤到同一条路上。
///
/// 返回 `true` 表示这一步已由宿主消化 —— 编辑器于是不再走默认的「在本文件内移动
/// 光标」，也就不会把光标挪到表名自己身上。
pub(super) fn jump_handler(
    view: WeakEntity<DatabasePage>,
    target: Rc<RefCell<Option<TableRef>>>,
) -> ShowDocumentHandler {
    Rc::new(move |params: &lsp_types::ShowDocumentParams, window, cx| {
        if params.uri.scheme().map(|scheme| scheme.as_str()) != Some("duck-table") {
            return false;
        }
        if let Some(page) = view.upgrade() {
            page.update(cx, |this, cx| {
                let table = target.borrow_mut().take();
                // 表是从**当前会话**的目录里解析出来的，所以「哪条连接」就是活跃那条。
                if let (Some(table), Some(conn)) = (table, this.active.clone()) {
                    this.open_structure_tab(&conn, table, false, window, cx);
                }
            });
        }
        // 取不到目标也算「已处理」：默认行为只会把光标挪到符号上，那不是用户
        // 点下去想要的任何一件事。
        true
    })
}

/// 文档的**真实行数**（`lines_len` 会把结尾换行之后那个空行也算一行）。
fn line_count(text: &Rope) -> usize {
    let lines = text.lines_len();
    let trailing_break = text
        .char_at(text.len().saturating_sub(1))
        .is_some_and(|ch| ch == '\n');
    if trailing_break {
        lines.saturating_sub(1)
    } else {
        lines
    }
    .max(1)
}

impl DatabasePage {
    /// 标识符路径 → 当前会话里那张表。
    ///
    /// 两步都按**不区分大小写**比（PG 把未加引号的名字折成小写，MySQL 视平台而定，
    /// 用户写 `Users` 指的是同一张 `users`）：
    ///
    /// 1. 两段及以上先按 `(schema, name)` 精确对 —— `public.users` 不该落到别的
    ///    schema 里那张同名表上；
    /// 2. 再逐段退让：`users.name` 里的 `users` 也算（限定列引用比限定表名更常见）。
    pub(super) fn resolve_table_path(&self, path: &str) -> Option<TableRef> {
        let catalog = &self.active_session()?.catalog;
        let segments: Vec<&str> = path.split('.').filter(|part| !part.is_empty()).collect();
        if segments.len() >= 2 {
            let schema = segments[segments.len() - 2];
            let name = segments[segments.len() - 1];
            if let Some(table) = catalog.iter().find(|table| {
                table.name.eq_ignore_ascii_case(name)
                    && table
                        .schema
                        .as_deref()
                        .is_some_and(|value| value.eq_ignore_ascii_case(schema))
            }) {
                return Some(table.table_ref());
            }
        }
        segments
            .iter()
            .find_map(|segment| {
                catalog
                    .iter()
                    .find(|table| table.name.eq_ignore_ascii_case(segment))
            })
            .map(CatalogTable::table_ref)
    }

    /// 打开跳行浮层（已经开着时再按一次是收起，与快速打开同一条规矩）。
    pub(super) fn open_goto_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.goto_line.is_some() {
            self.close_goto_line(window, cx);
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "输入行号…")));
        // 回车即跳。`subscribe_in`（而不是 `subscribe`）：跳完要把焦点交还编辑器，
        // 而 `window.focus` 要窗口 —— 用 `subscribe` 就只能再记一个待办字段。
        //
        // 订阅**挂在浮层自己身上**（而不是页面的 `_subscriptions`）：浮层开一次就多两条
        // 订阅，收进页面那个 vec 里就会随「按一次 ⌘⌥G」线性增长，永远是死条目。
        let mut subscriptions = vec![cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _source, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.commit_goto_line(window, cx);
                }
            },
        )];
        // 失焦即收起：点别处 / 切标签 / 编辑器自己拿到焦点都走这条路，省掉
        //「浮层一直挂在屏幕上」那种半开状态。
        subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _source, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Blur) {
                    this.goto_line = None;
                    cx.notify();
                }
            },
        ));
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.goto_line = Some(GotoLine {
            input,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    /// 收起跳行浮层并把焦点交还编辑器。
    pub(super) fn close_goto_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.goto_line.take().is_none() {
            return;
        }
        let handle = self.sql.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// 回车：解析行号并把光标送过去（越界夹到末行，不报错）。
    fn commit_goto_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(overlay) = self.goto_line.as_ref() else {
            return;
        };
        let value = overlay.input.read(cx).value().to_string();
        let Ok(line) = value.trim().parse::<usize>() else {
            self.toast(notify::Tone::Warning, tr(cx, "行号要是正整数。"), cx);
            return;
        };
        if line == 0 {
            self.toast(notify::Tone::Warning, tr(cx, "行号从 1 起。"), cx);
            return;
        }
        let sql = self.sql.clone();
        sql.update(cx, |state, cx| {
            let total = line_count(state.text());
            let offset = state.text().line_start_offset(line.min(total) - 1);
            state.set_selected_range(offset..offset, cx);
        });
        self.close_goto_line(window, cx);
    }

    /// 跳行浮层（页面内浮层；`None` = 没开）。
    pub(super) fn render_goto_line(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let overlay = self.goto_line.as_ref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let total = line_count(self.sql.read(cx).text());
        let view = self.view.clone();
        let card = v_flex()
            .w(px(GOTO_LINE_WIDTH))
            .gap(tokens.spacing.sm)
            .p(tokens.spacing.lg)
            .rounded(theme.components().dialog_radius)
            .border_1()
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.surface)
            .shadow_lg()
            // 卡片上的按下不冒泡到遮罩（否则点输入框就把浮层关了）。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .text_size(tokens.typography.sm.size)
                    .font_weight(FontWeight::MEDIUM)
                    .child(tr(cx, "跳到行")),
            )
            .child(Input::new(&overlay.input))
            .child(
                div()
                    .text_size(tokens.typography.xs.size)
                    .text_color(extended.colors.text_tertiary)
                    .child(trf(
                        cx,
                        "共 {} 行 · 回车跳转，Esc 取消",
                        &[&total.to_string()],
                    )),
            );
        let dismiss = self.view.clone();
        Some(
            div()
                .id("db-goto-line")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(88.))
                .bg(Hsla {
                    h: 0.,
                    s: 0.,
                    l: 0.,
                    a: extended.opacity.scrim,
                })
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    let _ = dismiss.update(cx, |this, cx| this.close_goto_line(window, cx));
                })
                .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        let _ = view.update(cx, |this, cx| this.close_goto_line(window, cx));
                    }
                })
                .child(card)
                .into_any_element(),
        )
    }
}
