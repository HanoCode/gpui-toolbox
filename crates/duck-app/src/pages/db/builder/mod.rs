//! 可视化 SQL 构建器（对齐 DBFlux 的 Builder）：入口在结果区顶部 tab 行的
//! **「执行计划」之后**，**容器是右栏 dock 的「构建器」分段** —— DBFlux 也是右侧栏
//! 承载（`docs/zh_Hans/QUERY_BUILDER.md`：「点击构建器，即可打开右侧栏面板」），
//! 所以它**不占结果区**：构建时原结果照看在眼里。
//!
//! 分工（与 `charts.rs` 同一条纪律：纯计算与渲染分开）：
//! - [`sql`]：**纯函数** spec → SQL（引号走 `duck_db::quote_identifier`，值按引擎写字面量，
//!   每条子句都有单测）；
//! - [`panel`]：右栏面板渲染（分区 + 行编辑器 + 实时预览 + 页脚动作）；
//! - 本文件：类型定义、UI 状态、行数据的增删、选项来源（当前连接的表 / 列）、动作落点。
//!
//! **列引用一律是输入框**（不是下拉）：列名可以带别名前缀或写成表达式（`u.id` /
//! `count(*)`），下拉只能选「元信息里有的那几列」；元信息取不到时（视图、权限不足、
//! 还没连库）下拉会变成一条死路，输入框不会。为了不丢可用性，每个分区的标题行另给
//! 一个「列 ▾」菜单：列清单拿得到时点一下就开一行并填好列名，拿不到时它显示一句说明。
//!
//! **本轮范围**（后续轮次见 `docs/database-design.md` §4.19 的分期表）：SELECT 全流程
//! （目标表 / 别名 / 列 / 条件 / 排序与限制 / 预览 / 执行 / 填入编辑器）＋ UPDATE 与
//! DELETE（赋值 / 条件 / 危险提示）；JOIN、分组聚合、已保存查询、schema 感知补全随后。

use super::*;

mod panel;
mod sql;

/// 构建器的三种语句模式（DBFlux 顶部的模式选择器）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum BuilderMode {
    /// 查询（默认）。
    #[default]
    Select,
    /// 更新：需要至少一条赋值才可运行。
    Update,
    /// 删除：只用条件区。
    Delete,
}

impl BuilderMode {
    /// 界面展示顺序。
    pub(super) const ALL: [Self; 3] = [Self::Select, Self::Update, Self::Delete];

    /// 下拉里的文字。
    pub(super) fn label(self, cx: &App) -> &'static str {
        match self {
            Self::Select => tr(cx, "SELECT"),
            Self::Update => tr(cx, "UPDATE"),
            Self::Delete => tr(cx, "DELETE"),
        }
    }

    /// 下拉的稳定值（与文案解耦：切语言不该改变选中项）。
    pub(super) const fn slug(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }

    /// 从 slug 反解（下拉回调用）。
    pub(super) fn from_slug(slug: &str) -> Self {
        match slug {
            "update" => Self::Update,
            "delete" => Self::Delete,
            _ => Self::Select,
        }
    }

    /// 它需要 `WHERE` 吗（DELETE 与 UPDATE 没条件就是**全表**动作，面板要警示）。
    pub(super) const fn needs_where(self) -> bool {
        !matches!(self, Self::Select)
    }
}

/// 投影方式：`SELECT *` 还是显式列清单。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum ProjectionMode {
    /// 全部列（默认）。
    #[default]
    All,
    /// 指定列。
    Explicit,
}

impl ProjectionMode {
    pub(super) const ALL: [Self; 2] = [Self::All, Self::Explicit];

    pub(super) fn label(self, cx: &App) -> &'static str {
        match self {
            Self::All => tr(cx, "全部列（*）"),
            Self::Explicit => tr(cx, "指定列"),
        }
    }

    pub(super) const fn slug(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Explicit => "explicit",
        }
    }

    pub(super) fn from_slug(slug: &str) -> Self {
        if slug == "explicit" {
            Self::Explicit
        } else {
            Self::All
        }
    }
}

/// 条件之间的连接算子（本轮是**顶层**一个；嵌套分组随后）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum BoolOp {
    /// 并且。
    #[default]
    And,
    /// 或者。
    Or,
}

impl BoolOp {
    pub(super) const ALL: [Self; 2] = [Self::And, Self::Or];

    pub(super) fn label(self, cx: &App) -> &'static str {
        match self {
            Self::And => tr(cx, "并且"),
            Self::Or => tr(cx, "或者"),
        }
    }

    pub(super) const fn slug(self) -> &'static str {
        match self {
            Self::And => "and",
            Self::Or => "or",
        }
    }

    pub(super) fn from_slug(slug: &str) -> Self {
        if slug == "or" { Self::Or } else { Self::And }
    }
}

/// 能「按列开一行」的四个分区（「列 ▾」菜单的落点）。
///
/// 收成一个枚举而不是四份闭包：菜单本身只关心「插到哪个分区」，而闭包里要克隆的
/// 句柄只有一份 —— 四份的话每加一个分区就多一份要同步的样板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowKind {
    /// 输出列（SELECT）。
    Column,
    /// 条件。
    Predicate,
    /// 排序（SELECT）。
    Sort,
    /// 赋值（UPDATE）。
    Assignment,
}

/// 一条投影列（`sql` 侧用的纯数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ColRow {
    pub column: String,
    /// 输出别名（空 = 不写 `AS`）。
    pub alias: String,
}

/// 一条筛选条件（纯数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PredicateRow {
    pub column: String,
    pub op: FilterOp,
    pub value: String,
    /// 值按**原样表达式**下发（函数 / 算术 / 方言专有写法）。
    pub raw_value: bool,
}

/// 一条排序（纯数据：列名 + 方向）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SortRow {
    pub column: String,
    pub desc: bool,
}

/// 一条 `SET` 赋值（纯数据；UPDATE 用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AssignRow {
    pub column: String,
    pub value: String,
    /// 值按原样表达式下发（`now()` / `amount + 1` 这类）。
    pub raw: bool,
}

/// 构建器产出一份**规范**（spec）：`sql::render` 的唯一输入。
///
/// 面板每次渲染从输入框现读现建一份，**不缓存** —— 缓存就会出现「界面上改了、
/// 预览还是旧的」这类最难查的偏差。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct BuilderSpec {
    pub mode: BuilderMode,
    /// 目标表（`None` = 还没选）。
    pub table: Option<TableRef>,
    /// 表别名（默认取表名；与表名相同则不写 `AS`）。
    pub alias: String,
    pub projection: ProjectionMode,
    pub columns: Vec<ColRow>,
    pub bool_op: BoolOp,
    pub predicates: Vec<PredicateRow>,
    pub sorts: Vec<SortRow>,
    pub limit: Option<u64>,
    pub offset: u64,
    pub assignments: Vec<AssignRow>,
}

/// 一条投影行的界面状态：列 + 别名输入框。
pub(super) struct ColRowUi {
    pub column: Entity<InputState>,
    pub alias: Entity<InputState>,
}

/// 一条条件行的界面状态：列输入框 + 算子 + 值输入框 + 原样开关。
pub(super) struct PredicateRowUi {
    pub column: Entity<InputState>,
    pub op: FilterOp,
    pub value: Entity<InputState>,
    pub raw_value: bool,
}

/// 一条排序行的界面状态。
pub(super) struct SortRowUi {
    pub column: Entity<InputState>,
    pub desc: bool,
}

/// 一条赋值行的界面状态。
pub(super) struct AssignRowUi {
    pub column: Entity<InputState>,
    pub value: Entity<InputState>,
    pub raw: bool,
}

/// 构建器的界面状态（页面级：右栏是 shell 级、一条路由一个，切标签不重置）。
pub(super) struct BuilderState {
    pub mode: BuilderMode,
    /// 目标表（`None` = 还没选）。
    pub table: Option<TableRef>,
    /// 表别名输入框（选表时填表名）。
    pub alias: Entity<InputState>,
    /// 列清单是**给哪张表**取的（表一换就重新取；`None` = 还没取过）。
    pub columns_for: Option<TableRef>,
    /// 目标表的列（拿不到就是空 —— 只影响「列 ▾」菜单，手输照样能拼 SQL）。
    pub table_columns: Vec<Column>,
    pub projection: ProjectionMode,
    pub columns: Vec<ColRowUi>,
    pub bool_op: BoolOp,
    pub predicates: Vec<PredicateRowUi>,
    pub sorts: Vec<SortRowUi>,
    pub limit: Entity<InputState>,
    pub offset: Entity<InputState>,
    pub assignments: Vec<AssignRowUi>,
    /// 预览的 SQL 编辑器（**高亮 + 可编辑**）：内容 = 由构建器实时生成的语句，
    /// 或用户手改后的文本。与结构编辑器的预览同一套语义。
    pub preview_editor: Entity<EditorState>,
    /// 预览正文是否处于编辑态（`readonly(false)`）。
    pub preview_editing: bool,
    /// 预览被手改过：执行 / 「在编辑器中打开」都用编辑器里那段，不再用生成结果。
    pub preview_manual: bool,
}

impl BuilderState {
    /// 初始态：除模式外全空，输入框在这里一次建好（选表时只改值，不重建实体）。
    pub(super) fn new(window: &mut Window, cx: &mut Context<DatabasePage>) -> Self {
        Self {
            mode: BuilderMode::Select,
            table: None,
            alias: text_input(window, cx, tr(cx, "表别名")),
            columns_for: None,
            table_columns: Vec::new(),
            projection: ProjectionMode::All,
            columns: Vec::new(),
            bool_op: BoolOp::And,
            predicates: Vec::new(),
            sorts: Vec::new(),
            limit: text_input(window, cx, tr(cx, "留空＝不限")),
            offset: text_input(window, cx, "0"),
            assignments: Vec::new(),
            // 预览编辑器：SQL 高亮 + 可用；内容由预览渲染时同步（构建器一格没填
            // 时它就是空的，不额外播种）。
            preview_editor: cx.new(|editor_cx| {
                EditorState::new(&mut *window, editor_cx)
                    .language(SQL_LANGUAGE)
                    .soft_wrap(true)
            }),
            preview_editing: false,
            preview_manual: false,
        }
    }
}

/// 目标表在下拉里的**稳定值**：`schema` 与名字用一个不可能出现在标识符里的分隔符拼。
pub(super) fn table_slug(table: &TableRef) -> String {
    match &table.schema {
        Some(schema) => format!("{schema}\u{1}{}", table.name),
        None => table.name.clone(),
    }
}

impl DatabasePage {
    /// 入口动作：**先选表（没选就按当前浏览表）→ 再展开右栏的构建器分段**。
    ///
    /// 顺序不能反也不必反：右栏展开是 shell 级副作用，选表只改页面状态；但「展开却空着」
    /// 会让用户以为坏了，所以先在同一个 `cx` 里把表定下来再展开。
    pub(super) fn open_builder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 这里用 let-chain：本仓的 `rust-version = "1.85"` 只对**依赖准入**实检，
        // 语言特性没有任何作业在查（`duck-mail/src/db/repo/drafts.rs:486` 有完整说明），
        // 且 db 模块内早有先例（`grid.rs` / `query.rs`）。故不是违规，按本模块风格写。
        if self.builder.table.is_none()
            && let Some(table) = self.selected_table.clone()
        {
            self.builder_set_table(table, window, cx);
        }
        self.reveal_right_dock(RightDock::Builder, cx);
    }

    /// 切换目标表：默认别名、清掉**引用旧表列**的行、后台重取列清单。
    ///
    /// 清行不是「体贴」，是**正确性**：列名是上一张表的，留着会生成一条跑不通的 SQL
    /// 而用户看不出为什么。
    pub(super) fn builder_set_table(
        &mut self,
        table: TableRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.builder.table.as_ref() == Some(&table) {
            return;
        }
        self.builder.table = Some(table.clone());
        self.builder.columns_for = None;
        self.builder.table_columns.clear();
        self.builder.columns.clear();
        self.builder.predicates.clear();
        self.builder.sorts.clear();
        self.builder.assignments.clear();
        self.builder.alias.update(cx, |state, cx| {
            state.set_value(table.name.clone(), window, cx);
        });
        self.builder_load_columns(cx);
        cx.notify();
    }

    /// 后台取目标表的列清单（阻塞 API ⇒ 必须离开渲染线程，范式同表浏览）。
    ///
    /// 取不到就留空：**不报错、不打断** —— 它只是「插入列」菜单的候选，用户仍可手输列名，
    /// 而「构建器因为元信息取不到就整个不能用」是本末倒置。
    pub(super) fn builder_load_columns(&mut self, cx: &mut Context<Self>) {
        let Some(table) = self.builder.table.clone() else {
            return;
        };
        let Some(conn) = self.active_session().map(|session| session.conn.clone()) else {
            return;
        };
        // 换表时把上一张表的列**立刻**清掉：留着的话「插入列」菜单会拿旧表的列往新表的
        // SQL 里填，而用户点的是新表 —— 这种错配比菜单空着难查得多。
        self.builder.table_columns.clear();
        let table_for_work = table.clone();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.table_columns(&table_for_work).unwrap_or_default()
            },
            move |this, columns, _, cx| {
                // 回来时目标表可能已经换成别的（连点两张表）：只认「还是这张表」的那一份。
                if this.builder.table.as_ref() != Some(&table) {
                    return;
                }
                this.builder.table_columns = columns;
                this.builder.columns_for = Some(table);
                cx.notify();
            },
        );
    }

    /// 当前引擎（构建器的方言来源）；未连接时按 SQLite 兜底只为能渲染预览，
    /// 真正的执行路径自己有「请先连接」的守卫）。
    fn builder_engine(&self) -> DatabaseKind {
        self.active_config()
            .map(|config| config.kind)
            .unwrap_or(DatabaseKind::Sqlite)
    }

    /// 用户点「执行」/「在编辑器中打开」时的方言；`None` = 这套引擎没有 SQL 这一层。
    ///
    /// ⚠️ **刻意保持私有**：`sql::BuilderDialect` 是 `pub(super)`（= 仅 `builder` 内可见），
    /// 方法若写成 `pub(super)`（= `pages::db` 可见）就会触发 `private_interfaces`
    /// ——「返回值类型比方法本身更私密」。子模块 `panel` 是本模块的后代，
    /// 私有项对它天然可见，所以这里不需要放宽可见性。
    fn builder_dialect(&self) -> Option<sql::BuilderDialect> {
        let engine = self.builder_engine();
        // 优先用**会话里的真值**（agent 档的引号只有连上才知道）；还没连上时退回引擎的
        // 静态引号 —— 那时 `builder_engine()` 本来就是 SQLite 兜底，只为能渲染预览。
        let quote = self.active_quote().or_else(|| engine.identifier_quote());
        sql::dialect_for(engine, quote)
    }

    /// 目标表候选：当前连接里可查询的关系（表 / 视图），值 = [`table_slug`]。
    ///
    /// 只取「关系」：存储过程 / 函数 / 序列不能 `SELECT * FROM`，放进候选就是给用户
    /// 一条点了必然报错的入口。
    pub(super) fn builder_table_candidates(&self) -> Vec<(String, TableRef)> {
        let Some(session) = self.active_session() else {
            return Vec::new();
        };
        session
            .catalog
            .iter()
            .filter(|entry| entry.kind.is_relation())
            .map(|entry| {
                let table = entry.table_ref();
                (table_slug(&table), table)
            })
            .collect()
    }

    /// 「列 ▾」菜单的候选：目标表的列名（值＝文案＝列名）。
    pub(super) fn builder_column_candidates(&self) -> Vec<(SharedString, SharedString)> {
        self.builder
            .table_columns
            .iter()
            .map(|column| {
                let name = SharedString::from(column.name.clone());
                (name.clone(), name)
            })
            .collect()
    }

    /// 从界面状态现读现建一份 spec（预览与执行共用，二者不可能不一致）。
    pub(super) fn builder_spec(&self, cx: &App) -> BuilderSpec {
        let text_of = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        BuilderSpec {
            mode: self.builder.mode,
            table: self.builder.table.clone(),
            alias: text_of(&self.builder.alias),
            projection: self.builder.projection,
            columns: self
                .builder
                .columns
                .iter()
                .map(|row| ColRow {
                    column: text_of(&row.column),
                    alias: text_of(&row.alias),
                })
                .collect(),
            bool_op: self.builder.bool_op,
            predicates: self
                .builder
                .predicates
                .iter()
                .map(|row| PredicateRow {
                    column: text_of(&row.column),
                    op: row.op,
                    // 值**不 trim**：前后空格在字符串字面量里可能是用户有意要的。
                    value: row.value.read(cx).value().to_string(),
                    raw_value: row.raw_value,
                })
                .collect(),
            sorts: self
                .builder
                .sorts
                .iter()
                .map(|row| SortRow {
                    column: text_of(&row.column),
                    desc: row.desc,
                })
                .collect(),
            limit: text_of(&self.builder.limit)
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0),
            offset: text_of(&self.builder.offset).parse::<u64>().unwrap_or(0),
            assignments: self
                .builder
                .assignments
                .iter()
                .map(|row| AssignRow {
                    column: text_of(&row.column),
                    value: row.value.read(cx).value().to_string(),
                    raw: row.raw,
                })
                .collect(),
        }
    }

    /// 实时预览：`None` = 还没选表或这套引擎没有 SQL。
    ///
    /// 与 [`Self::builder_dialect`] 同理，**刻意私有**（类型比方法更私密会报
    /// `private_interfaces`）。
    fn builder_preview(&self, cx: &App) -> Option<sql::RenderedSql> {
        let dialect = self.builder_dialect()?;
        sql::render(&self.builder_spec(cx), dialect)
    }

    /// 预览**实际要用**的 SQL：手改优先（编辑器里那段），否则用生成结果。
    ///
    /// 「执行」与「在编辑器中打开」都走这里 —— 预览显示什么就执行什么，两者不分叉。
    fn builder_apply_sql(&self, cx: &App) -> Option<String> {
        let draft = &self.builder;
        let manual = draft.preview_editing || draft.preview_manual;
        if manual {
            let text = draft.preview_editor.read(cx).value().trim().to_owned();
            if !text.is_empty() {
                return Some(text);
            }
        }
        self.builder_preview(cx).map(|rendered| rendered.sql)
    }

    // ── 行的增删 ─────────────────────────────────────────────────────────
    //
    // 每加一行都要建输入框实体（`InputState` 需要 `window`），所以这些方法都带
    // `window`：与「弹窗里加一条」同一条范式。参数 `column` 是「列 ▾」菜单的预填值，
    // 普通「添加」按钮传空串。

    pub(super) fn builder_add_column(
        &mut self,
        column: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.builder.columns.push(ColRowUi {
            column: text_input_with(window, cx, tr(cx, "列名或表达式"), &column),
            alias: text_input(window, cx, tr(cx, "别名（可空）")),
        });
        cx.notify();
    }

    pub(super) fn builder_remove_column(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.builder.columns.len() {
            self.builder.columns.remove(index);
            cx.notify();
        }
    }

    pub(super) fn builder_add_predicate(
        &mut self,
        column: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.builder.predicates.push(PredicateRowUi {
            column: text_input_with(window, cx, tr(cx, "列名或表达式"), &column),
            op: FilterOp::Eq,
            value: text_input(window, cx, tr(cx, "值")),
            raw_value: false,
        });
        cx.notify();
    }

    pub(super) fn builder_remove_predicate(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.builder.predicates.len() {
            self.builder.predicates.remove(index);
            cx.notify();
        }
    }

    pub(super) fn builder_add_sort(
        &mut self,
        column: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.builder.sorts.push(SortRowUi {
            column: text_input_with(window, cx, tr(cx, "列名或表达式"), &column),
            desc: false,
        });
        cx.notify();
    }

    pub(super) fn builder_remove_sort(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.builder.sorts.len() {
            self.builder.sorts.remove(index);
            cx.notify();
        }
    }

    pub(super) fn builder_add_assignment(
        &mut self,
        column: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.builder.assignments.push(AssignRowUi {
            column: text_input_with(window, cx, tr(cx, "列名或表达式"), &column),
            value: text_input(window, cx, tr(cx, "新值")),
            raw: false,
        });
        cx.notify();
    }

    pub(super) fn builder_remove_assignment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.builder.assignments.len() {
            self.builder.assignments.remove(index);
            cx.notify();
        }
    }

    /// 在某个分区里新开一行、把列名预填好（「列 ▾」菜单的落点）；空串＝普通「添加」。
    pub(super) fn builder_insert_row(
        &mut self,
        kind: RowKind,
        column: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match kind {
            RowKind::Column => self.builder_add_column(column, window, cx),
            RowKind::Predicate => self.builder_add_predicate(column, window, cx),
            RowKind::Sort => self.builder_add_sort(column, window, cx),
            RowKind::Assignment => self.builder_add_assignment(column, window, cx),
        }
    }

    /// 重置：保留目标表，清掉模式之外的一切（DBFlux 的重置同款语义）。
    pub(super) fn builder_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.builder.mode = BuilderMode::Select;
        self.builder.projection = ProjectionMode::All;
        self.builder.bool_op = BoolOp::And;
        self.builder.columns.clear();
        self.builder.predicates.clear();
        self.builder.sorts.clear();
        self.builder.assignments.clear();
        self.builder.limit.update(cx, |state, cx| {
            state.set_value(String::new(), window, cx);
        });
        self.builder.offset.update(cx, |state, cx| {
            state.set_value(String::new(), window, cx);
        });
        // 重置同时丢掉预览手改：控件全空了，预览还留着一段旧 SQL 会让人以为它算数。
        self.builder.preview_editing = false;
        self.builder.preview_manual = false;
        self.builder
            .preview_editor
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        self.status = tr(cx, "构建器已重置。").into();
        cx.notify();
    }

    /// 预览编辑态开关（「编辑 SQL」↔「完成」）：退出时与生成结果比对定稿。
    pub(super) fn set_builder_preview_editing(
        &mut self,
        editing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.builder.preview_editing = editing;
        if editing {
            self.builder
                .preview_editor
                .update(cx, |state, cx| state.focus(window, cx));
        } else {
            let generated = self
                .builder_preview(cx)
                .map(|rendered| rendered.sql)
                .unwrap_or_default();
            let text = self
                .builder
                .preview_editor
                .read(cx)
                .value()
                .trim_end()
                .to_owned();
            self.builder.preview_manual = text != generated.trim_end();
        }
        cx.notify();
    }

    /// 「重新生成」：丢掉预览手改，回到「构建器投影」态。
    pub(super) fn regenerate_builder_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.builder.preview_manual = false;
        self.builder.preview_editing = false;
        let generated = self
            .builder_preview(cx)
            .map(|rendered| rendered.sql)
            .unwrap_or_default();
        self.builder
            .preview_editor
            .update(cx, |state, cx| state.set_value(generated, window, cx));
        cx.notify();
    }

    // ── 动作落点 ─────────────────────────────────────────────────────────

    /// 执行：把预览里的 SQL 交给既有的执行链路（排队 / 取消 / 超时 / 结果标签全复用）。
    ///
    /// 走的**不是**编辑器内容 —— 构建器产出的是它自己那条语句，用户没点「在编辑器中打开」
    /// 就不该动编辑器里正在写的东西。
    ///
    /// 危险语句（全表 `DELETE` / `UPDATE`）走**与编辑器同一条闸门**（`guard_dangerous`）：
    /// 同一句问话、同一个判据（`risk_of_statements`）——「这条是构建器拼的」不是少问一次的理由。
    pub(super) fn builder_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sql) = self.builder_apply_sql(cx) else {
            notify::info(tr(cx, "先选一张目标表。"), window, cx);
            return;
        };
        let to_run = sql.clone();
        let run = Rc::new(
            move |this: &mut DatabasePage, window: &mut Window, cx: &mut Context<DatabasePage>| {
                this.execute_sql(to_run.clone(), None, window, cx);
            },
        );
        self.guard_dangerous(&sql, run, window, cx);
    }

    /// 在编辑器中打开：把预览的 SQL 填进编辑器（**不执行**，交给用户确认）。
    pub(super) fn builder_stage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sql) = self.builder_apply_sql(cx) else {
            notify::info(tr(cx, "先选一张目标表。"), window, cx);
            return;
        };
        self.stage_sql(
            sql,
            tr(cx, "已把构建器生成的 SQL 放进编辑器。").into(),
            window,
            cx,
        );
    }
}
