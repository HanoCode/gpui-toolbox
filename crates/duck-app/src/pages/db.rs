//! 数据库页：连接管理（左栏）+ SQL 编辑器与结果网格（主区）+ 对象详情（右栏）。
//!
//! 布局对齐 dbflux 的三栏形态；交互范式沿用本仓 `TerminalPage`：
//! 左栏走页面内 [`SidebarPanel`]，右栏走 shell 级 `with_right_sidebar`。
//!
//! **M1 范围**（`docs/database-design.md` §6）：SQLite / PostgreSQL / MySQL 三引擎的
//! 连接管理、表浏览（分页）、SQL 执行（多语句 + 行上限截断）、结果网格与 CSV 导出。
//! 行内编辑、SSH 隧道、PostgreSQL TLS、AI 助手按设计稿分期跟进。
//!
//! 线程纪律：`duck-db` 全部是阻塞 API，本页的每条数据库操作都经
//! [`Self::spawn_db`] 丢进 `background_executor`，结果在 `cx.spawn` 里回投主线程
//! （范式同 `plugins.rs` / `pages/logs.rs`）；渲染路径上不做任何数据库调用，
//! 展示用的列宽与文本都在任务收尾时算好存起来。

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use duck_components::{
    ButtonVariant, CheckState, ConfirmHandler, DatabaseLogo, DuckIcon, SidebarPanel, SidebarState,
    button, caption_heading, card, check_mark,
    content::{accent_tag, description_list, description_row, empty_state, neutral_tag},
    dialog_footer, dialog_top_offset, dropdown_select, field_hint, field_label, form_gap,
    icon_button, icon_text_button, meta_text, modal_drag, modal_drag_area, modal_offset,
    modal_title_row, notify, sidebar_navigation_button, title_text, toolbar_action_button,
};
use duck_db::ai::build_prompt;
use duck_db::config::{
    ConnectionConfig, DatabaseKind, EnvTag, SshTunnelConfig, TlsMode, delete_password,
    load_password, load_ssh_password, store_password, store_ssh_password,
};
use duck_db::drivers::{CellEdit, sqlite::to_csv};
use duck_db::import::import_third_party;
use duck_db::sqlintel::{
    ExplainSkip, SqlRisk, TxEffect, explain_sql, format_sql, risk_of_statements, split_statements,
    tx_effect,
};
use duck_db::{
    AiProvider, BatchOutcome, CancelHandle, CancelTimer, CatalogTable, Column, Connection, DbError,
    EditTarget, ExecOutcome, InsertRow, OpenAiCompatible, OrderBy, PkValues, ResultSet, RowCount,
    SshTunnel, SshTunnelSpec, TableIndex, TableKind, TableRef, covers_columns, edit_target,
    parse_csv,
};
use duck_theme::{active_theme, tr, trf};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FontWeight, Hsla,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Task, Toggled, WeakEntity, Window,
    actions, div, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::{
    Icon, ResizableState, Sizable as _, Size, WindowExt as _,
    dialog::{DialogButtonProps, DialogFooter},
    h_flex,
    input::{CompletionProvider, Editor, EditorState, Input, InputEvent, InputState, Rope},
    menu::{ContextMenuExt as _, PopupMenuItem},
    resizable_panel, v_flex, v_resizable,
};
// `CompletionProvider` 的签名直接引用 lsp-types 的类型，实现方必须能具名它们；
// 版本与 gpui-component 经 `gpui_component::input` 转出的 `lsp_types::Position` 同源。
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse};

/// kv 单键：连接档案数组（结构体 → serde_json，范式同 `appearance`）。
const KEY_CONNECTIONS: &str = "db_connections";
/// kv 单键：AI 助手设置。
const KEY_AI_SETTINGS: &str = "ai_settings";
/// AI API key 的凭据键名。
const AI_SECRET_KEY: &str = "ai:api_key";
/// 表浏览每页行数。
const PAGE_SIZE: usize = 200;
/// SQL 查询结果行数上限（`+1` 探测截断）。
const QUERY_ROW_CAP: usize = 10_000;
/// 一条语句的**全局默认**执行超时：到点由 [`CancelTimer`] 送取消（内核报 [`DbError::Cancelled`]）。
///
/// 30 秒是「OLTP 手敲查询」的经验值——真跑分析型大查询请走表浏览的分页或加 `LIMIT`。
/// 生效值按「单次覆盖 → 连接默认 → 本常量」三级回落，见 [`DatabasePage::effective_timeout`]。
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// 查询超时可接受的秒数上限（挡住 0 / 手滑多打一位数）。
const MAX_TIMEOUT_SECS: u64 = 3600;
/// SQL 编辑器可拖拽的高度档位（下边框把手）。
const SQL_EDITOR_HEIGHT_RANGE: std::ops::RangeInclusive<f32> = 72.0..=520.0;
/// 网格行高：与 `typography.sm` 的 18px 行高同源，行内文字不再压缩。
const GRID_ROW_HEIGHT: f32 = 24.;
/// 网格列宽的上下限（M1 不做拖宽，按内容一次性估宽）。
const COLUMN_MIN_WIDTH: f32 = 72.;
const COLUMN_MAX_WIDTH: f32 = 260.;
/// 结果网格行首「多选」列的宽度（不计入 `column_widths`）。
const SELECT_COLUMN_WIDTH: f32 = 34.;
/// 左栏宽度档位。
const SIDEBAR_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 184.0..=340.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 236.;
/// 分组档案 kv 单键（左栏「新建分组」创建的分组名列表）。
const KEY_GROUPS: &str = "db_groups";
/// 查询历史 kv 单键（最近执行过的 SQL，最新的在前）。
const KEY_HISTORY: &str = "db_query_history";
/// 查询历史最多保留多少条（超出的丢掉最旧的）。
const HISTORY_LIMIT: usize = 50;
/// 查询历史面板的最大高度（超出自己滚，不挤占结果区）。
const HISTORY_PANEL_MAX_HEIGHT: f32 = 180.;
/// 全局并发上限：同时最多允许多少条数据库操作在跑（`docs/database-design.md` §4.4）。
///
/// 三引擎的连接都是**一条连接一个锁**，真正的并行度受连接数限制；这个闸门挡的是
/// 「多个连接同时压满」以及「同一页反复点触发一串操作把 background_executor 占满」。
const MAX_CONCURRENT_DB_OPS: usize = 4;
/// 「新建连接」驱动选择弹窗宽度。
const PICKER_WIDTH: f32 = 720.;
/// 驱动卡片网格列数（弹窗宽度固定，3 列对齐 dbflux 的密度）。
const PICKER_COLUMNS: usize = 3;
/// 驱动选择弹窗卡片网格的最大高度。
const PICKER_GRID_MAX_HEIGHT: f32 = 460.;
/// 连接详情弹窗（picker 的子页）宽度。
const EDIT_DIALOG_WIDTH: f32 = 720.;
/// 详情弹窗正文的最大高度；超出后正文自身滚动（头部与脚注常驻）。
const EDIT_DIALOG_BODY_MAX_HEIGHT: f32 = 540.;
/// 「新建分组」提示弹窗宽度：正文只有一行输入，比常规确认弹窗窄一档。
const GROUP_DIALOG_WIDTH: f32 = 380.;
/// 详情页滚动卡片（DDL / 索引两张相邻卡片共用）的最大高度。
///
/// 两张卡片原先各写各的裸值（260 / 240），20px 的差在视觉上无意义却让同页
/// 出现两种尺寸——UI 规范排查时收敛成同一条。
const DETAIL_CARD_MAX_HEIGHT: f32 = 260.;
/// 详情弹窗头部引擎 logo 边长。
const EDIT_LOGO_SIZE: f32 = 26.;
/// 驱动选择卡片上的引擎 logo 边长（卡片比详情头部矮，故比
/// [`EDIT_LOGO_SIZE`] 小一档）。
const PICKER_LOGO_SIZE: f32 = 22.;
/// 数据库页的键盘上下文。
///
/// 必须比 `"Root"` 更深：gpui 的按键派发**先查绑定再跑 `on_key_down`**，且更深上下文
/// 优先命中——`⌘Z` / `⌘.` 在编辑器（`"Input"` / `"Editor"`）上下文里已有归属
/// （撤销 / 代码操作），页面级键位另开一层才不会互相抢。见
/// `docs/database-design.md` §8 与 `tests/database_keys.rs`。
const KEY_CONTEXT: &str = "Database";

/// SQL 编辑器的高亮语言名。
///
/// 必须与 `gpui-component` 注册表里的名字对上（`Language::Sql => "sql"`，由根
/// `Cargo.toml` 的 `features = ["tree-sitter-sql"]` 打开）；名字写错不会编译报错，
/// 只会静默不高亮 —— 所以 `tests` 里有一条把它钉住。
const SQL_LANGUAGE: &str = "sql";

actions!(
    db,
    [
        /// 执行编辑器里的 SQL；**正在跑时同一个键变成「停止」**（工具条按钮也是这个语义）。
        RunOrStopQuery,
        /// 提交暂存改动（改格 + 新增行，一个事务）。
        CommitPendingEdits,
        /// 丢弃全部暂存改动。
        DiscardPendingEdits,
        /// 复制当前选中的那一行（TSV）—— 网格里 `⌘C` 的自然语义。
        CopySelectedRow,
    ]
);

/// 绑定数据库页的键位（由 `app::install` 调一次）。
///
/// 单独成函数（而不是内联在 `install` 里）是为了让 `actions.rs` 的测试能只做
/// 「绑一次 + 读回键位表」，不必跑整套 `install`（那会建托盘、开主窗口、读用户配置）。
pub(crate) fn bind_keys(cx: &mut App) {
    let modifier = duck_command::primary_modifier();
    cx.bind_keys([
        // 执行 / 停止：`⌘↵`。`gpui_base::Root` 与编辑器的默认键位表里都没有它。
        KeyBinding::new(
            &format!("{modifier}-enter"),
            RunOrStopQuery,
            Some(KEY_CONTEXT),
        ),
        // 提交暂存。编辑器不占 `⌘S`，故焦点在 SQL 编辑器里时这条**照样生效**
        // （gpui 沿焦点链收集绑定、更深的先派发，但没被更深层「认领」的键会继续往上走）。
        KeyBinding::new(
            &format!("{modifier}-s"),
            CommitPendingEdits,
            Some(KEY_CONTEXT),
        ),
        // 丢弃暂存是破坏性动作，用带 `Shift` 的组合降低误触概率。
        // 注意：**不能**指望编辑器的 `⌘⌫` 帮忙拦截 —— gpui-base 绑的是无 `Shift` 的
        // `cmd-backspace`，`cmd-shift-backspace` 是另一个键，仍会冒泡到这一层。
        KeyBinding::new(
            &format!("{modifier}-shift-backspace"),
            DiscardPendingEdits,
            Some(KEY_CONTEXT),
        ),
        // 网格里的 `⌘C`：焦点在 SQL 编辑器 / 输入框里时更深的上下文先接管（复制文字），
        // 只有焦点落在页面本身或结果网格上时才走这条 —— 覆盖 `gpui_base::Root` 的同键绑定。
        KeyBinding::new(&format!("{modifier}-c"), CopySelectedRow, Some(KEY_CONTEXT)),
    ]);
}

// ── 驱动选择器（dbflux 同形：分区 + 品牌卡片网格）────────────────────────────

/// 驱动选择器的分区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerSection {
    Relational,
    Document,
    KeyValue,
    TimeSeriesAndLogs,
    Search,
}

/// 分区展示顺序（dbflux 同序；无卡的分区不出现在界面）。
const PICKER_SECTIONS: &[PickerSection] = &[
    PickerSection::Relational,
    PickerSection::Document,
    PickerSection::KeyValue,
    PickerSection::TimeSeriesAndLogs,
    PickerSection::Search,
];

impl PickerSection {
    fn label(self, cx: &App) -> SharedString {
        let label = match self {
            Self::Relational => tr(cx, "关系型"),
            Self::Document => tr(cx, "文档"),
            Self::KeyValue => tr(cx, "键值"),
            Self::TimeSeriesAndLogs => tr(cx, "时间序列和日志"),
            Self::Search => tr(cx, "搜索"),
        };
        SharedString::from(label)
    }
}

/// 驱动选择器里的一张卡片。
struct PickerDriver {
    id: &'static str,
    name: &'static str,
    logo: DatabaseLogo,
    /// 卡片第二行的短提示（`:5432` / `file`，dbflux 同法）。
    hint: &'static str,
    section: PickerSection,
    /// M1 支持的引擎携带真实的 [`DatabaseKind`]；其余为占位展示。
    kind: Option<DatabaseKind>,
}

const PICKER_DRIVERS: &[PickerDriver] = &[
    PickerDriver {
        id: "postgresql",
        name: "PostgreSQL",
        logo: DatabaseLogo::Postgres,
        hint: ":5432",
        section: PickerSection::Relational,
        kind: Some(DatabaseKind::Postgres),
    },
    PickerDriver {
        id: "mysql",
        name: "MySQL",
        logo: DatabaseLogo::Mysql,
        hint: ":3306",
        section: PickerSection::Relational,
        kind: Some(DatabaseKind::Mysql),
    },
    PickerDriver {
        id: "mariadb",
        name: "MariaDB",
        logo: DatabaseLogo::Mariadb,
        hint: ":3306",
        section: PickerSection::Relational,
        kind: None,
    },
    PickerDriver {
        id: "sqlserver",
        name: "SQL Server",
        logo: DatabaseLogo::Sqlserver,
        hint: ":1433",
        section: PickerSection::Relational,
        kind: Some(DatabaseKind::SqlServer),
    },
    PickerDriver {
        id: "sqlite",
        name: "SQLite",
        logo: DatabaseLogo::Sqlite,
        hint: "file",
        section: PickerSection::Relational,
        kind: Some(DatabaseKind::Sqlite),
    },
    PickerDriver {
        id: "redshift",
        name: "Amazon Redshift",
        logo: DatabaseLogo::Redshift,
        hint: ":5439",
        section: PickerSection::Relational,
        kind: None,
    },
    PickerDriver {
        id: "clickhouse",
        name: "ClickHouse",
        logo: DatabaseLogo::Clickhouse,
        hint: ":8123",
        section: PickerSection::Relational,
        kind: Some(DatabaseKind::ClickHouse),
    },
    PickerDriver {
        id: "oracle",
        name: "Oracle",
        logo: DatabaseLogo::Oracle,
        hint: ":1521",
        section: PickerSection::Relational,
        kind: None,
    },
    PickerDriver {
        id: "duckdb",
        name: "DuckDB",
        logo: DatabaseLogo::Duckdb,
        hint: "file",
        section: PickerSection::Relational,
        kind: None,
    },
    PickerDriver {
        id: "mongodb",
        name: "MongoDB",
        logo: DatabaseLogo::Mongodb,
        hint: ":27017",
        section: PickerSection::Document,
        kind: Some(DatabaseKind::Mongo),
    },
    PickerDriver {
        id: "dynamodb",
        name: "DynamoDB",
        logo: DatabaseLogo::Dynamodb,
        hint: "AWS",
        section: PickerSection::Document,
        kind: None,
    },
    PickerDriver {
        id: "redis",
        name: "Redis",
        logo: DatabaseLogo::Redis,
        hint: ":6379",
        section: PickerSection::KeyValue,
        kind: None,
    },
    PickerDriver {
        id: "influxdb",
        name: "InfluxDB",
        logo: DatabaseLogo::Influxdb,
        hint: ":8086",
        section: PickerSection::TimeSeriesAndLogs,
        kind: None,
    },
    PickerDriver {
        id: "elasticsearch",
        name: "Elasticsearch",
        logo: DatabaseLogo::Elasticsearch,
        hint: ":9200",
        section: PickerSection::Search,
        kind: Some(DatabaseKind::Elasticsearch),
    },
];

/// 卡片是否命中过滤词（名称 / id / 提示，大小写不敏感）。
fn driver_matches_query(driver: &PickerDriver, query: &str) -> bool {
    query.is_empty()
        || driver.name.to_lowercase().contains(query)
        || driver.id.to_lowercase().contains(query)
        || driver.hint.to_lowercase().contains(query)
}

/// 引擎 → 品牌 logo（详情表单头部用）。
fn kind_logo(kind: DatabaseKind) -> DatabaseLogo {
    match kind {
        DatabaseKind::Sqlite => DatabaseLogo::Sqlite,
        DatabaseKind::Postgres => DatabaseLogo::Postgres,
        DatabaseKind::Mysql => DatabaseLogo::Mysql,
        DatabaseKind::ClickHouse => DatabaseLogo::Clickhouse,
        DatabaseKind::SqlServer => DatabaseLogo::Sqlserver,
        DatabaseKind::Mongo => DatabaseLogo::Mongodb,
        DatabaseKind::Elasticsearch => DatabaseLogo::Elasticsearch,
    }
}

/// 主区展示模式：SQL 结果或表浏览。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// SQL 执行结果。
    Query,
    /// 表浏览（分页）。
    Table,
}

/// 一条已建立的会话：阻塞连接包在互斥锁里，由后台线程短暂持有。
struct Session {
    conn: Arc<Mutex<Connection>>,
    /// 服务器版本描述（连接时取一次）。
    version: String,
    /// 当前命名空间的目录（表 / 视图 / 函数 / 序列；连接时与切 schema 时取）。
    catalog: Vec<CatalogTable>,
    /// 可选的命名空间清单（连接时取一次）。
    schemas: Vec<String>,
    /// 当前浏览的命名空间（`None` = 引擎的当前命名空间，即 `catalog` 里那份的来处）。
    schema: Option<String>,
    /// SSH 隧道句柄（有则连接经 127.0.0.1 隧道端口；断开随会话释放）。
    tunnel: Option<Arc<SshTunnel>>,
    /// 是否处于**显式事务**（用户敲过 BEGIN / START TRANSACTION，还没 COMMIT / ROLLBACK）。
    ///
    /// 判据只看用户发过来的语句文本（[`tx_effect`]）：单条执行路径下连接是常驻的，
    /// 语句之间的事务状态由服务器维持，这里只是把它**显示**出来。
    in_tx: bool,
}

/// 正在跑的一件数据库工作（有则工具条把「执行」换成「停止」）。
struct RunningQuery {
    /// 本次运行的令牌：收尾时只清理「自己那次」。
    ///
    /// 表页加载可能被连续点击叠加（后一次覆盖前一次），没有令牌的话，先回来的那次
    /// 收尾会把**后一次**的 `running` 摘掉并撤销它的超时守护。
    token: u64,
    /// 会话 id：结果回来时核对用（关标签 / 切连接后不误判）。
    session_id: String,
    /// 带外取消句柄（不占连接锁，后台线程握着锁也能送「停」）。
    cancel: CancelHandle,
    /// 超时守护；查询正常收工必须 `disarm()`，否则晚到的那一枪打在下一条语句上。
    timer: CancelTimer,
}

/// SQL 执行结果的展示态（跑一次存起来，渲染绝不现算）。
struct QueryView {
    /// 结果集（整段都是写语句时为 `None`）。
    result: Option<ResultSet>,
}

/// 行编辑器（暂存式）。
///
/// 两种形态共用一套输入框：`row_index` 是 `Some(n)` 时改第 `n` 行，`None` 时是
/// **新增行**（提交走 INSERT，没填的列落到数据库 `DEFAULT`）。
struct RowEditor {
    /// 正在编辑的行号（当前结果集内的下标）；`None` = 新增行。
    row_index: Option<usize>,
    /// 每列一个输入框；主键列也可编辑（定位用原值，与输入无关）。
    cells: Vec<RowCell>,
}

/// 行编辑器里的一列。
struct RowCell {
    /// 列名。
    column: String,
    /// 进编辑器时的展示原值（比对判定「已改」与主键定位）。
    original: String,
    /// 进编辑器时这一格是不是 `NULL`（判定「写 NULL」到底是改动还是原样）。
    was_null: bool,
    /// 单行输入框。
    input: Entity<InputState>,
    /// 「这一格写 NULL」开关（`Some("")` 与 `None` 在库里是两件事，必须能分开表达）。
    ///
    /// 用 `Rc<Cell<bool>>` 而不是普通字段：开关的点击回调活在渲染闭包（`'static`）里，
    /// 拿不到 `&mut Self`；写进 `Cell` 后由回调直接改，提交时再读。
    null: Rc<std::cell::Cell<bool>>,
}

/// AI 助手设置（kv 单键 `ai_settings`；api_key 走凭据文件，不进 kv）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AiSettings {
    /// OpenAI 兼容 base_url（不带尾斜杠）。
    #[serde(default = "default_ai_base_url")]
    base_url: String,
    /// 模型名。
    #[serde(default = "default_ai_model")]
    model: String,
}

fn default_ai_base_url() -> String {
    "https://api.openai.com/v1".into()
}

fn default_ai_model() -> String {
    "gpt-4o-mini".into()
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            base_url: default_ai_base_url(),
            model: default_ai_model(),
        }
    }
}

/// 从 kv 读 AI 设置；缺字段 / 解析失败回落默认。
fn load_ai_settings(cx: &App) -> AiSettings {
    duck_store::kv_get(cx, KEY_AI_SETTINGS)
        .and_then(|json| serde_json::from_str::<AiSettings>(&json).ok())
        .unwrap_or_default()
}

/// AI 一次问答的结局。
///
/// ⚠️ 这里**不含执行结果**：模型调用（网络等待）与 SQL 执行是两件事——前者只能靠
/// 它自带的 HTTP 超时，后者才需要「可取消 + 查询超时」。分成两步之后，只读 SQL 的执行
/// 与手敲执行走**同一条** [`DatabasePage::spawn_db_guarded`] 路径。
enum AiOutcome {
    /// 单条只读：可以自动执行（执行交给 [`DatabasePage::execute_ai_sql`]）。
    ReadOnly { sql: String, tokens: u32 },
    /// 写语句 / 多条 / 混合：填入编辑器等人工执行。
    NeedsApproval { sql: String, tokens: u32 },
}

/// 新建 / 编辑连接的表单 —— **独立视图**（弹窗的正文就是它）。
///
/// 两条理由，都与 `redis.rs` 的 `EditForm` 同源：
///
/// 1. **所有权**：弹窗的 `content` 闭包是 `Fn`（可能重排重跑），拿不到 `&mut DatabasePage`，
///    只能克隆 `Entity` 进去读写；
/// 2. **重绘**：弹窗正文由 `WindowState` 的 `render` 驱动。实测**宿主页面 `notify()` 到不了
///    这里** —— 旁证是全仓 5 个页面（db / redis / logs / terminal / plugin-center）都必须显式
///    订阅 `InputEvent::Change → cx.notify()`，输入框驱动的列表才会跟着变。所以「点了要变色的
///    控件」（环境 / SSL 模式 / SSH 隧道展开）必须活在一个视图里，由它自己 `cx.notify()`
///    重画那一层。
///
/// 所有权补充：它只持 `page` 弱引用用于回调，**页面不持它** —— 弹窗是唯一所有者，
/// 关窗即释放（同时避开了「在自身更新期间析构」那类崩溃）。
struct EditFormView {
    /// 回调宿主（保存 / 测试 / 删除 / 返回都要回到页面）。
    page: WeakEntity<DatabasePage>,
    /// `None` = 新建。
    id: Option<String>,
    kind: DatabaseKind,
    tls: TlsMode,
    env_tag: EnvTag,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    database: Entity<InputState>,
    password: Entity<InputState>,
    group: Entity<InputState>,
    /// 该连接的默认查询超时（秒）；留空 = 用全局默认。
    timeout: Entity<InputState>,
    /// 默认命名空间（schema / 库）；留空 = 用引擎的当前命名空间。
    schema: Entity<InputState>,
    /// 连接级**只读档**：建连后立刻把会话切到只读（由服务器兜底拒绝写）。
    read_only: bool,
    ssh_enabled: bool,
    ssh_host: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_remote_host: Entity<InputState>,
    ssh_password: Entity<InputState>,
}

impl EditFormView {
    /// 新建态：按引擎把可预判的字段直接填好（对齐 dbx 的 `defaultForm()`），少让用户手打；
    /// SQLite 走本地文件，三样都不填。
    fn new(
        page: WeakEntity<DatabasePage>,
        kind: DatabaseKind,
        window: &mut Window,
        cx: &mut Context<DatabasePage>,
    ) -> Self {
        // 预填对齐 dbx 的 `defaultForm()`（`ConnectionDialog.vue`）：host=`127.0.0.1`、
        // 端口=引擎缺省数字、用户=引擎惯用账号；SQLite 走本地文件，三样都不填。
        // host 输入框在网络引擎下恒被预填，占位符不出现，留空即可。
        let host = text_input(window, cx, "");
        let port = text_input(window, cx, tr(cx, "端口"));
        let user = text_input(window, cx, tr(cx, "用户名"));
        if kind.needs_host() {
            host.update(cx, |state, cx| state.set_value("127.0.0.1", window, cx));
            user.update(cx, |state, cx| {
                state.set_value(default_user(kind), window, cx)
            });
            if let Some(default_port) = kind.default_port() {
                port.update(cx, |state, cx| {
                    state.set_value(default_port.to_string(), window, cx)
                });
            }
        }
        Self {
            page,
            id: None,
            kind,
            tls: TlsMode::Disable,
            env_tag: EnvTag::None,
            name: text_input(window, cx, tr(cx, "例如：本地开发库")),
            host,
            port,
            user,
            database: text_input(window, cx, tr(cx, "库名（网络引擎可留空）")),
            password: secret_input(window, cx, tr(cx, "留空表示保持既有口令不变")),
            group: text_input(window, cx, tr(cx, "默认分组")),
            timeout: text_input(window, cx, tr(cx, "留空跟随默认")),
            schema: text_input(window, cx, tr(cx, "留空用引擎当前命名空间")),
            read_only: false,
            ssh_enabled: false,
            ssh_host: text_input(window, cx, tr(cx, "跳板机主机")),
            ssh_port: text_input(window, cx, tr(cx, "留空用 22")),
            ssh_user: text_input(window, cx, tr(cx, "跳板机用户")),
            ssh_remote_host: text_input(window, cx, tr(cx, "留空表示跳板机本机")),
            ssh_password: secret_input(window, cx, tr(cx, "留空用密钥认证 / 保持既有")),
        }
    }

    /// 编辑态：用既有档案填表（口令不入表，留空即保持既有）。
    fn from_config(
        page: WeakEntity<DatabasePage>,
        cfg: &ConnectionConfig,
        window: &mut Window,
        cx: &mut Context<DatabasePage>,
    ) -> Self {
        let mut form = Self::new(page, cfg.kind, window, cx);
        form.id = Some(cfg.id.clone());
        form.tls = cfg.tls;
        form.env_tag = cfg.env_tag;
        form.name.update(cx, |state, cx| {
            state.set_value(cfg.name.clone(), window, cx)
        });
        form.host.update(cx, |state, cx| {
            state.set_value(cfg.host.clone(), window, cx)
        });
        if let Some(port) = cfg.port {
            form.port.update(cx, |state, cx| {
                state.set_value(port.to_string(), window, cx)
            });
        }
        form.user.update(cx, |state, cx| {
            state.set_value(cfg.user.clone(), window, cx)
        });
        form.database.update(cx, |state, cx| {
            state.set_value(cfg.database.clone(), window, cx)
        });
        if let Some(group) = cfg.group.clone() {
            form.group
                .update(cx, |state, cx| state.set_value(group, window, cx));
        }
        if let Some(secs) = cfg.default_timeout() {
            form.timeout.update(cx, |state, cx| {
                state.set_value(secs.to_string(), window, cx)
            });
        }
        if let Some(schema) = cfg.schema.clone() {
            form.schema
                .update(cx, |state, cx| state.set_value(schema, window, cx));
        }
        form.read_only = cfg.read_only;
        if let Some(tunnel) = &cfg.ssh_tunnel {
            form.ssh_enabled = true;
            form.ssh_host.update(cx, |state, cx| {
                state.set_value(tunnel.host.clone(), window, cx)
            });
            if let Some(port) = tunnel.port {
                form.ssh_port.update(cx, |state, cx| {
                    state.set_value(port.to_string(), window, cx)
                });
            }
            form.ssh_user.update(cx, |state, cx| {
                state.set_value(tunnel.user.clone(), window, cx)
            });
            form.ssh_remote_host.update(cx, |state, cx| {
                state.set_value(tunnel.remote_host.clone(), window, cx)
            });
        }
        form
    }

    /// 把表单读回结构体；端口解析失败返回可显示的提示。
    fn to_config(&self, cx: &App) -> Result<ConnectionConfig, SharedString> {
        let port_text = self.port.read(cx).value().trim().to_owned();
        let port = if port_text.is_empty() {
            None
        } else {
            match port_text.parse::<u16>() {
                Ok(port) if port > 0 => Some(port),
                _ => return Err(tr(cx, "端口必须是 1-65535 的数字").into()),
            }
        };
        let group_text = self.group.read(cx).value().trim().to_owned();
        let group = (!group_text.is_empty()).then_some(group_text);
        // 查询超时：留空 / `0` 都表示「用全局默认」（写回 `None`，别把 0 存进去）。
        let timeout_text = self.timeout.read(cx).value().trim().to_owned();
        let default_timeout_secs = if timeout_text.is_empty() {
            None
        } else {
            match timeout_text.parse::<u64>() {
                Ok(0) => None,
                Ok(secs) if secs <= MAX_TIMEOUT_SECS => Some(secs),
                _ => {
                    return Err(trf(
                        cx,
                        "查询超时必须是 1-{} 的整数秒",
                        &[&MAX_TIMEOUT_SECS.to_string()],
                    )
                    .into());
                }
            }
        };
        let ssh_tunnel = if self.ssh_enabled {
            let tunnel_port_text = self.ssh_port.read(cx).value().trim().to_owned();
            let tunnel_port = if tunnel_port_text.is_empty() {
                None
            } else {
                match tunnel_port_text.parse::<u16>() {
                    Ok(port) if port > 0 => Some(port),
                    _ => return Err(tr(cx, "SSH 端口必须是 1-65535 的数字").into()),
                }
            };
            Some(SshTunnelConfig {
                host: self.ssh_host.read(cx).value().trim().to_owned(),
                port: tunnel_port,
                user: self.ssh_user.read(cx).value().trim().to_owned(),
                remote_host: self.ssh_remote_host.read(cx).value().trim().to_owned(),
            })
        } else {
            None
        };
        Ok(ConnectionConfig {
            id: self.id.clone().unwrap_or_else(|| {
                format!("conn-{}", id_suffix(self.name.read(cx).value().as_ref()))
            }),
            name: self.name.read(cx).value().trim().to_owned(),
            kind: self.kind,
            host: self.host.read(cx).value().trim().to_owned(),
            port,
            user: self.user.read(cx).value().trim().to_owned(),
            database: self.database.read(cx).value().trim().to_owned(),
            tls: self.tls,
            env_tag: self.env_tag,
            group,
            ssh_tunnel,
            default_timeout_secs,
            read_only: self.read_only,
            schema: {
                let text = self.schema.read(cx).value().trim().to_owned();
                (!text.is_empty()).then_some(text)
            },
        })
    }

    /// 配置 + 解析后的口令：表单里非空用表单值，否则读既有凭据（与保存语义一致）。
    ///
    /// SSH 口令用 `and_then`（隧道缺失得 `None`，而不是 `Some(None)`）——
    /// `probe_connection` 收的是 `Option<String>`（口令本身可空）。
    #[allow(clippy::type_complexity)]
    fn prepared(
        &self,
        cx: &App,
    ) -> Result<(ConnectionConfig, Option<String>, Option<String>), SharedString> {
        let cfg = self.to_config(cx)?;
        let password_text = self.password.read(cx).value().trim().to_owned();
        let password = if password_text.is_empty() {
            load_password(&cfg.id).unwrap_or(None)
        } else {
            Some(password_text)
        };
        let ssh_password = cfg.ssh_tunnel.as_ref().and_then(|_| {
            let text = self.ssh_password.read(cx).value().trim().to_owned();
            if text.is_empty() {
                load_ssh_password(&cfg.id).unwrap_or(None)
            } else {
                Some(text)
            }
        });
        Ok((cfg, password, ssh_password))
    }

    /// 保存：校验 → 口令进凭据文件 → 档案落 kv（经页面）。返回是否成功。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (cfg, password, ssh_password) = match self.prepared(cx) {
            Ok(prepared) => prepared,
            Err(message) => {
                notify::warning(message, window, cx);
                return false;
            }
        };
        if let Err(error) = cfg.validate() {
            notify::warning(SharedString::from(error.to_string()), window, cx);
            return false;
        }
        // 口令：非空才写凭据文件（编辑时留空 = 保持既有口令不变）。
        if let Some(password) = password.filter(|text| !text.is_empty())
            && let Err(error) = store_password(&cfg.id, &password)
        {
            notify::error(
                trf(cx, "口令保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return false;
        }
        if cfg.ssh_tunnel.is_some()
            && let Some(ssh_password) = ssh_password.filter(|text| !text.is_empty())
            && let Err(error) = store_ssh_password(&cfg.id, &ssh_password)
        {
            notify::error(
                trf(cx, "SSH 口令保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return false;
        }
        let _ = self
            .page
            .update(cx, |page, cx| page.upsert_connection(cfg, cx));
        notify::success(tr(cx, "连接已保存。"), window, cx);
        true
    }

    /// 「测试连接」：用表单当前值探活（不保存、不开会话），报告版本或错误。
    fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (cfg, password, ssh_password) = match self.prepared(cx) {
            Ok(prepared) => prepared,
            Err(message) => {
                notify::warning(message, window, cx);
                return;
            }
        };
        if let Err(error) = cfg.validate() {
            notify::warning(SharedString::from(error.to_string()), window, cx);
            return;
        }
        let _ = self.page.update(cx, |page, cx| {
            page.spawn_probe(cfg, password, ssh_password, cx)
        });
    }
}

/// 新建连接的第一步：驱动选择网格 —— **独立视图**（弹窗的正文就是它）。
///
/// 与 [`EditFormView`] 同因（弹窗 `content` 闭包是 `Fn`，拿不到 `&mut DatabasePage`），
/// 但这里还多一条硬理由：**搜索框要边打边重排网格**。全仓 5 个页面（db / redis / logs /
/// terminal / plugin-center）都靠显式订阅 `InputEvent::Change → cx.notify()` 才让列表跟着
/// 输入框变 —— 可见「子视图 notify 会把宿主重画」这条并不成立。正文挂成视图后，订阅与
/// `cx.notify()` 都落在视图自己身上，网格才会随关键字重排。
///
/// 所有权同 [`EditFormView`]：只持 `page` 弱引用，弹窗是唯一所有者，关窗即释放。
struct DriverPickerView {
    /// 回调宿主（选中引擎进详情表单、从文件导入）。
    page: WeakEntity<DatabasePage>,
    /// 过滤网格的搜索框。
    search: Entity<InputState>,
    /// 关键字（小写；订阅里算好，`render` 只读它）。
    query: String,
    /// 搜索框订阅（延长生命周期 —— 丢掉就不再有 `Change` 回调）。
    _subscription: Subscription,
}

impl DriverPickerView {
    fn new(page: WeakEntity<DatabasePage>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = text_input(window, cx, tr(cx, "按名称、驱动或端口筛选"));
        let subscription = cx.subscribe(
            &search,
            |this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    this.query = this.search.read(cx).value().trim().to_lowercase();
                    cx.notify();
                }
            },
        );
        Self {
            page,
            search,
            query: String::new(),
            _subscription: subscription,
        }
    }

    /// 选中一张卡片：M1 支持的引擎关掉 picker、进详情表单；其余只提示。
    fn choose(&self, kind: Option<DatabaseKind>, window: &mut Window, cx: &mut App) {
        let Some(kind) = kind else {
            notify::info(tr(cx, "该数据库类型的支持即将到来。"), window, cx);
            return;
        };
        window.close_dialog(cx);
        let _ = self.page.update(cx, |page, cx| {
            page.start_new_connection(kind, window, cx);
        });
    }
}

impl Render for DriverPickerView {
    /// 头部（提示 + 搜索框）常驻；下方按分区排卡片网格，整体独立滚动。
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let hover_bg = tokens.colors.muted;

        // 标题行（`modal_title_row`）已经写了「新建连接」+ ✕，这里**不再重复标题**：
        // 第二行只放一句提示与搜索框（原实现两行标题叠着，是个肉眼可见的重复）。
        let header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.lg)
            .child(meta_text(tr(cx, "选择要连接的目标。"), cx))
            .child(
                h_flex()
                    .h(theme.density().control)
                    // 驱动选择器的**舒适宽度**：布局约束（不在 spacing 阶梯上），保留裸值。
                    .w(px(240.))
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .px(tokens.spacing.sm)
                    .rounded(tokens.radius.md)
                    .border_1()
                    .border_color(tokens.colors.border)
                    .bg(tokens.colors.background)
                    .child(
                        Icon::new(DuckIcon::Search)
                            .size(theme.extended().icon.md)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .appearance(false)
                                .with_size(Size::Small)
                                .text_size(tokens.typography.sm.size)
                                .w_full(),
                        ),
                    ),
            );

        let visible: Vec<&PickerDriver> = PICKER_DRIVERS
            .iter()
            .filter(|driver| driver_matches_query(driver, &self.query))
            .collect();

        let mut body = v_flex().w_full().min_h_0().gap(tokens.spacing.xs);
        for section in PICKER_SECTIONS {
            let cards: Vec<&PickerDriver> = visible
                .iter()
                .copied()
                .filter(|driver| driver.section == *section)
                .collect();
            if cards.is_empty() {
                continue;
            }
            let mut grid = div()
                .grid()
                .grid_cols(PICKER_COLUMNS as u16)
                .gap(tokens.spacing.xs);
            for driver in cards {
                let name = SharedString::from(driver.name);
                let hint = SharedString::from(driver.hint);
                let logo = driver.logo;
                let kind = driver.kind;
                grid = grid.child(
                    h_flex()
                        .id(SharedString::from(format!("db-driver-{}", driver.id)))
                        .min_w_0()
                        .items_center()
                        .gap(tokens.spacing.sm)
                        .px(tokens.spacing.sm)
                        .py(tokens.spacing.xs)
                        .rounded(tokens.radius.md)
                        .border_1()
                        .border_color(tokens.colors.border)
                        .bg(tokens.colors.background)
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover_bg))
                        .child(logo.element(px(PICKER_LOGO_SIZE)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(tokens.typography.sm.size)
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(tokens.typography.xs.size)
                                        .text_color(tokens.colors.muted_foreground)
                                        .child(hint),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.choose(kind, window, cx);
                        })),
                );
            }
            body = body
                .child(caption_heading(section.label(cx), cx))
                .child(grid);
        }
        if visible.is_empty() {
            body = body.child(
                div()
                    .w_full()
                    .py(tokens.spacing.lg)
                    .flex()
                    .justify_center()
                    .child(meta_text(tr(cx, "没有匹配的数据库类型。"), cx)),
            );
        }

        v_flex()
            .w_full()
            .min_h_0()
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.sm)
                    .border_b_1()
                    .border_color(tokens.colors.border)
                    .child(header),
            )
            .child(
                div()
                    .id("db-picker-grid")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .max_h(px(PICKER_GRID_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.sm)
                    .child(body),
            )
    }
}

/// 连接档案导入失败的原因。
///
/// 内核只给分类（[`duck_db::import::ImportError`]），文案在 [`import_error_message`] 里翻，
/// 所以这个枚举可以安全地穿过后台线程边界（`String` 与 `Copy` 枚举都是 `Send`）。
enum ProfileImportError {
    /// 文件根本没读出来。
    Read(String),
    /// 三种格式（本应用 / DBeaver / TablePlus）都不认。
    ThirdParty(duck_db::import::ImportError),
}

/// 第三方档案的失败原因 → 给用户看的说明。
fn import_error_message(error: duck_db::import::ImportError, cx: &App) -> &'static str {
    use duck_db::import::ImportError;
    match error {
        ImportError::NotJson => tr(cx, "文件内容不是合法的 JSON。"),
        ImportError::Unrecognised => tr(
            cx,
            "认不出这个连接档案；支持本应用导出的档案、DBeaver 的 data-sources.json 与 TablePlus 的 .tableplusconnection。",
        ),
        ImportError::NoSupportedConnections => tr(
            cx,
            "档案里没有本应用支持的引擎（目前支持 SQLite / PostgreSQL / MySQL）。",
        ),
    }
}

/// 表单缺 id 时的兜底：名字 FNV 哈希混入时间戳，唯一即可。
fn id_suffix(seed: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes().chain(nanos.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 建一个带占位符的输入框状态。
///
/// 泛型收 `Context<T>`：页面自己（[`DatabasePage`]）与两个弹窗正文视图
/// （[`DriverPickerView`] / [`EditFormView`]）都从各自的上下文里建输入框。
fn text_input<T>(
    window: &mut Window,
    cx: &mut Context<T>,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

/// 建一个带占位符的口令输入框状态（显示为掩码）。
fn secret_input(
    window: &mut Window,
    cx: &mut Context<DatabasePage>,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .masked(true)
    })
}

/// 从 kv 读连接档案；解析失败回落空表（用户重存即可覆盖）。
fn load_connections(cx: &App) -> Vec<ConnectionConfig> {
    duck_store::kv_get(cx, KEY_CONNECTIONS)
        .and_then(|json| serde_json::from_str::<Vec<ConnectionConfig>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读分组档案；解析失败回落空表。
fn load_groups(cx: &App) -> Vec<String> {
    duck_store::kv_get(cx, KEY_GROUPS)
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读查询历史（解析失败 / 缺键 → 空）。
fn load_history(cx: &App) -> Vec<String> {
    duck_store::kv_get(cx, KEY_HISTORY)
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .unwrap_or_default()
}

/// 把一条 SQL 记进查询历史（去重后置顶，超出 [`HISTORY_LIMIT`] 丢最旧的）。
///
/// 只记**非空且与最近一条不同**的语句：重复执行同一条查询是很常见的动作，
/// 每次都记会把历史刷成同一行。
fn push_history(history: &mut Vec<String>, sql: &str) {
    let sql = sql.trim();
    if sql.is_empty() {
        return;
    }
    history.retain(|entry| entry != sql);
    history.insert(0, sql.to_owned());
    history.truncate(HISTORY_LIMIT);
}

/// 当前结果集的**可编辑来源**（表浏览的表，或可编辑 SELECT 的表）。
///
/// 把「结果集 + 目标表 + 主键」三件一起给出：改格 / 删行 / 批量操作都必须同时知道
/// 「改哪张表」和「拿什么列定位」，只给结果下标是不够的。
struct EditSource<'a> {
    /// 当前展示的结果集。
    result: &'a ResultSet,
    /// 结果集背后的表（限定名）。
    table: &'a TableRef,
    /// 定位行用的主键列。
    pk: &'a [String],
}

/// 列头排序的一档。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SortSpec {
    /// 排序列。
    column: String,
    /// 是否降序。
    descending: bool,
}

impl SortSpec {
    /// 点一次列头的下一步（升 → 降 → 取消）。
    #[must_use]
    fn next(current: Option<&Self>, column: &str) -> Option<Self> {
        match current {
            Some(spec) if spec.column == column && !spec.descending => Some(Self {
                column: column.to_owned(),
                descending: true,
            }),
            Some(spec) if spec.column == column => None,
            _ => Some(Self {
                column: column.to_owned(),
                descending: false,
            }),
        }
    }
}

/// 翻页用的 `ORDER BY` 档位：**用户排序列在前，主键随后兜底**。
///
/// 只按用户那一列排是不够的：同值行之间的先后在 SQL 里是**未定义**的，PG / MySQL
/// 每页各排一次就可能给出不同顺序 → 翻页重复或漏行（`docs/database-design.md` §4.5）。
/// 主键（唯一）随后补位，整条序才是全序。
///
/// 用户排的列**本身就是主键的一部分**时不再重复挂一列（重复列不报错但没意义），
/// 改用它指定的方向——否则「按主键降序」会被后面的升序主键抵消。
fn page_order(sort: Option<&SortSpec>, pk: &[String]) -> Vec<OrderBy> {
    let mut order: Vec<OrderBy> = Vec::with_capacity(pk.len() + 1);
    if let Some(spec) = sort
        && !pk.contains(&spec.column)
    {
        order.push(if spec.descending {
            OrderBy::desc(spec.column.clone())
        } else {
            OrderBy::asc(spec.column.clone())
        });
    }
    order.extend(pk.iter().map(|column| {
        let descending = sort.is_some_and(|spec| spec.column == *column && spec.descending);
        if descending {
            OrderBy::desc(column.clone())
        } else {
            OrderBy::asc(column.clone())
        }
    }));
    order
}

/// 结果集的 TSV 文本（含表头）。
///
/// 用制表符而不是逗号：粘进 Excel / Numbers / 飞书表格会**直接分列**，不用再走一遍
/// 「分列向导」。TSV 没有公认的转义标准，这里按 CSV 的引号规则处理含制表符 / 换行 /
/// 引号的单元格（主流表格软件导入时认这一套）。
fn result_as_tsv(result: &ResultSet) -> String {
    let mut lines = Vec::with_capacity(result.rows.len() + 1);
    lines.push(
        result
            .columns
            .iter()
            .map(|column| tsv_cell(&column.name))
            .collect::<Vec<_>>()
            .join("\t"),
    );
    for row in &result.rows {
        lines.push(
            row.iter()
                .map(|value| tsv_cell(&value.render()))
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    lines.join("\n")
}

/// 一行结果的 TSV 文本（不含表头）—— 「复制整行」用。
fn row_as_tsv(row: &[duck_db::Value]) -> String {
    row.iter()
        .map(|value| tsv_cell(&value.render()))
        .collect::<Vec<_>>()
        .join("\t")
}

/// TSV 单元格：含有分隔符 / 换行 / 引号时按 CSV 的规矩包进双引号。
fn tsv_cell(cell: &str) -> String {
    if cell.contains(['\t', '\n', '\r', '"']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell.to_owned()
    }
}

/// 结果集的列宽估算：采样前 200 行，按最长展示文本估宽再夹在上下限之间。
fn compute_column_widths(result: &ResultSet) -> Vec<f32> {
    let sample = result.rows.len().min(200);
    result
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let mut longest = column.name.chars().count();
            for row in result.rows.iter().take(sample) {
                if let Some(cell) = row.get(index) {
                    longest = longest.max(cell.render().chars().count());
                }
            }
            // 中文按 2 倍宽近似：字符数 × 7px，上下限夹住。
            (longest as f32 * 7.0 + 16.0).clamp(COLUMN_MIN_WIDTH, COLUMN_MAX_WIDTH)
        })
        .collect()
}

/// 结果网格行首的多选复选框（表头全选 / 每行勾选共用）。
///
/// 纯图标没有可访问名，所以名字必须**显式**给（`aria_label`）：AccessKit 的
/// `CheckBox` 只认 `aria_toggled` 与显式名字，读不出 `check_mark` 里那个图标。
fn grid_checkbox(
    id: SharedString,
    state: CheckState,
    label: SharedString,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let hover = theme.extended().colors.hover_tint;
    let press = theme.extended().colors.hover_tint_press;
    let ring = theme.tokens().colors.ring;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(GRID_ROW_HEIGHT))
        .rounded(theme.tokens().radius.sm)
        .cursor_pointer()
        .role(Role::CheckBox)
        .aria_label(label)
        .aria_toggled(match state {
            CheckState::Checked => Toggled::True,
            CheckState::Unchecked => Toggled::False,
            CheckState::Indeterminate => Toggled::Mixed,
        })
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(press))
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.border_1().border_color(ring))
        .on_click(on_click)
        .child(check_mark(state, cx))
        .into_any_element()
}

// ── SQL 补全（CompletionProvider）───────────────────────────────────────────

/// SQL 关键字候选（label 即插入文本，大写为 SQL 惯例）。
const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "CREATE TABLE",
    "CREATE INDEX",
    "CREATE VIEW",
    "DROP TABLE",
    "DROP INDEX",
    "DROP VIEW",
    "ALTER TABLE",
    "TRUNCATE TABLE",
    "JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "INNER JOIN",
    "FULL JOIN",
    "CROSS JOIN",
    "ON",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION",
    "UNION ALL",
    "DISTINCT",
    "AS",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS NULL",
    "IS NOT NULL",
    "IN",
    "NOT IN",
    "LIKE",
    "NOT LIKE",
    "BETWEEN",
    "EXISTS",
    "CASE WHEN",
    "THEN",
    "ELSE",
    "END",
    "ASC",
    "DESC",
    "VALUES",
    "SET",
    "PRIMARY KEY",
    "FOREIGN KEY",
    "REFERENCES",
    "DEFAULT",
    "CONSTRAINT",
    "UNIQUE",
    "CHECK",
    "CASCADE",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "EXPLAIN",
];

/// SQL 常用函数（label 即插入文本）。
const SQL_FUNCTIONS: &[&str] = &[
    "COUNT(*)", "COUNT", "SUM", "AVG", "MIN", "MAX", "LOWER", "UPPER", "LENGTH", "SUBSTR", "TRIM",
    "COALESCE", "NULLIF", "CAST", "ROUND", "ABS", "IFNULL", "CONCAT", "RANDOM", "DATE", "STRFTIME",
];

/// 补全候选条数上限（关键字+函数+表一次给全也就 90 余条，截断防极端卡顿）。
const COMPLETION_LIMIT: usize = 80;

/// SQL 编辑器的补全来源：关键字 / 函数 + 当前会话目录的表名；点号后取该表的列名。
///
/// 列名要查库（`table_columns` 是阻塞 API）：经后台执行器跑、主线程 Task 里写缓存。
/// 每表只查一次，缓存随弹层 / 会话生命周期走（provider 挂在编辑器状态上）。
struct SqlCompletionProvider {
    view: WeakEntity<DatabasePage>,
    columns: Rc<std::cell::RefCell<HashMap<String, Vec<Column>>>>,
}

impl SqlCompletionProvider {
    fn new(view: WeakEntity<DatabasePage>) -> Self {
        Self {
            view,
            columns: Rc::new(std::cell::RefCell::new(HashMap::new())),
        }
    }

    /// 表名补全候选（含视图）。
    ///
    /// 收「目录切片」而非 `&DatabasePage`：调用点在 `completions` 里只能对页面持
    /// 临时不可变借用，而本函数还要用 `cx` 取译文——持借用会让可变 / 不可变借用
    /// 撞车。目录先按值取一份（≤200 项、两个字段），借用随即释放。
    fn table_items(&self, catalog: &[CatalogTable], query: &str, cx: &App) -> Vec<CompletionItem> {
        catalog
            .iter()
            .filter(|table| matches_query(&table.name, query))
            .take(COMPLETION_LIMIT)
            .map(|table| CompletionItem {
                label: table.name.clone(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some(table_kind_label(table.kind, cx)),
                ..Default::default()
            })
            .collect()
    }

    /// 取（必要时后台查）点号前那张表的列。
    fn column_completions(
        &self,
        table: &str,
        query: &str,
        cx: &App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let table_key = table.to_lowercase();
        if let Some(columns) = self.columns.borrow().get(&table_key) {
            return Task::ready(Ok(CompletionResponse::Array(
                columns
                    .iter()
                    .filter(|column| matches_query(&column.name, query))
                    .take(COMPLETION_LIMIT)
                    .map(|column| CompletionItem {
                        label: column.name.clone(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: Some(column.type_name.clone()),
                        ..Default::default()
                    })
                    .collect(),
            )));
        }
        let Some(page) = self.view.upgrade() else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };
        let Some(conn) = page.read(cx).active_conn() else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };
        let table_owned = table.to_owned();
        let query_owned = query.to_owned();
        let cache = self.columns.clone();
        let fetch = cx.background_executor().spawn(async move {
            let mut guard = conn.lock().expect("连接锁不应中毒");
            // 补全拿到的只有「用户敲的那段名字」，没有 schema 上下文 → 裸名引用，
            // 让引擎按自己的 search_path / 当前库解析。
            guard.table_columns(&TableRef::bare(table_owned))
        });
        cx.spawn(async move |_| {
            let columns = fetch.await.map_err(|error| anyhow!(error.to_string()))?;
            let items: Vec<CompletionItem> = columns
                .iter()
                .filter(|column| matches_query(&column.name, &query_owned))
                .take(COMPLETION_LIMIT)
                .map(|column| CompletionItem {
                    label: column.name.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(column.type_name.clone()),
                    ..Default::default()
                })
                .collect();
            cache.borrow_mut().insert(table_key, columns);
            Ok(CompletionResponse::Array(items))
        })
    }
}

impl CompletionProvider for SqlCompletionProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let query = trigger.trigger_character.unwrap_or_default();
        let Some(page) = self.view.upgrade() else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };

        // 光标前的词：标识符字符与点号。带点号说明在写 `table.`，转列补全。
        let prefix = text.to_string();
        let word_before = prefix
            .get(..offset.min(prefix.len()))
            .unwrap_or_default()
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();

        if let Some((table, column_query)) = word_before.rsplit_once('.') {
            if !table.is_empty() {
                return self.column_completions(table, column_query, cx);
            }
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        }

        // 目录按值取一份再释放借用，避免与随后用 `cx` 取译文撞借用。
        let catalog: Vec<CatalogTable> = page
            .read(cx)
            .active_session()
            .map(|session| session.catalog.clone())
            .unwrap_or_default();
        let mut items: Vec<CompletionItem> = self.table_items(&catalog, &query, cx);
        items.extend(
            SQL_KEYWORDS
                .iter()
                .filter(|keyword| matches_query(keyword, &query))
                .map(|keyword| CompletionItem {
                    label: (*keyword).to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: Some(tr(cx, "关键字").to_string()),
                    ..Default::default()
                }),
        );
        items.extend(
            SQL_FUNCTIONS
                .iter()
                .filter(|function| matches_query(function, &query))
                .map(|function| CompletionItem {
                    label: (*function).to_string(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    detail: Some(tr(cx, "函数").to_string()),
                    ..Default::default()
                }),
        );
        items.truncate(COMPLETION_LIMIT);
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        // 标识符字符与点号触发；整段粘贴（>2 字符）不打断输入。
        new_text.chars().count() <= 2
            && new_text
                .chars()
                .last()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}

/// 候选是否命中查询词（大小写不敏感：前缀命中排前由调用方排序，这里只判命中）。
fn matches_query(candidate: &str, query: &str) -> bool {
    query.is_empty() || candidate.to_lowercase().contains(&query.to_lowercase())
}

/// 用给定档案探活：建连（含 SSH 隧道）并取服务器版本描述；不落任何会话状态。
fn probe_connection(
    cfg: &ConnectionConfig,
    password: Option<String>,
    ssh_password: Option<String>,
) -> Result<String, DbError> {
    let tunnel = match cfg.ssh_tunnel.as_ref() {
        Some(tunnel_cfg) if cfg.kind.needs_host() => {
            Some(Arc::new(SshTunnel::open(SshTunnelSpec {
                jump_host: tunnel_cfg.host.clone(),
                jump_port: tunnel_cfg.port,
                user: tunnel_cfg.user.clone(),
                remote_host: tunnel_cfg.remote_host.clone(),
                remote_port: cfg
                    .port
                    .unwrap_or_else(|| cfg.kind.default_port().expect("network kind has a port")),
                password: ssh_password,
            })?))
        }
        _ => None,
    };
    let mut effective = cfg.clone();
    if let Some(tunnel) = tunnel.as_ref() {
        effective.host = "127.0.0.1".into();
        effective.port = Some(tunnel.local_port());
    }
    let mut conn = Connection::connect(&effective, password.as_deref())?;
    Ok(conn.server_version().unwrap_or_default())
}

/// 全局并发闸门（[`MAX_CONCURRENT_DB_OPS`] 的落地点）。
///
/// 为什么等待放在 **async** 里、而不是「后台线程阻塞等名额」：后台执行器的线程数有限，
/// 阻塞等会把线程全占在等待上（最坏自我饿死：4 个线程都在等，谁也没在跑）。
/// 异步等待的 future 只占一个队列项。
///
/// 为什么不是「主线程排队」：待跑的闭包类型各异（每个 `spawn_db` 的 `T` 都不同），
/// 没法塞进同一个队列；只在**取值时**排队就不用搬运闭包。
struct DbOpGate {
    state: Mutex<GateState>,
}

struct GateState {
    /// 正在跑的操作数（被放行给等待者时**不减**，直接转交）。
    in_flight: usize,
    /// 排队的等待者（FIFO；被唤醒时名额已转交到自己手上）。
    waiters: VecDeque<futures_channel::oneshot::Sender<()>>,
}

impl DbOpGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                in_flight: 0,
                waiters: VecDeque::new(),
            }),
        }
    }

    /// 取一个名额；满了就排队，等某位持有者 [`Self::release`] 时把名额转交过来。
    async fn acquire(self: Arc<Self>) -> DbOpPermit {
        // 锁的临界区里不 await，也不搬动 `self`：先决定「直接拿走」还是「登记排队」。
        let waiting = {
            let mut state = self.state.lock().expect("db op gate lock poisoned");
            if state.in_flight < MAX_CONCURRENT_DB_OPS {
                state.in_flight += 1;
                None
            } else {
                let (tx, rx) = futures_channel::oneshot::channel();
                state.waiters.push_back(tx);
                Some(rx)
            }
        };
        if let Some(rx) = waiting {
            // 被唤醒 = 名额已转交（`in_flight` 不变）；发送端被丢弃说明闸门没了，也照跑。
            let _ = rx.await;
        }
        DbOpPermit { gate: self }
    }

    /// 放行：优先把名额**直接交给**排在最前的等待者，没人等才减计数。
    ///
    /// 等待者可能已经放弃（页面关了 / future 被取消），那个 `send` 会失败——
    /// 这时必须接着找下一个，**绝不能吞掉名额**（吞掉就等于并发上限凭空变小一格）。
    fn release(&self) {
        let mut state = self.state.lock().expect("db op gate lock poisoned");
        while let Some(next) = state.waiters.pop_front() {
            if next.send(()).is_ok() {
                return;
            }
        }
        state.in_flight = state.in_flight.saturating_sub(1);
    }
}

/// 名额凭证：`drop` 即归还（持到工作闭包跑完为止）。
struct DbOpPermit {
    gate: Arc<DbOpGate>,
}

impl Drop for DbOpPermit {
    fn drop(&mut self) {
        self.gate.release();
    }
}

/// 进程内唯一的闸门（多个页面 / 多个窗口共用同一份上限）。
fn db_op_gate() -> Arc<DbOpGate> {
    static GATE: OnceLock<Arc<DbOpGate>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(DbOpGate::new())).clone()
}

/// 数据库页。
pub struct DatabasePage {
    /// 左栏折叠 / 展开状态（shell 顶栏的侧栏开关也读它）。
    sidebar: Entity<SidebarState>,
    /// 连接搜索框。
    search: Entity<InputState>,
    /// SQL 编辑器（代码编辑器模式）。
    sql: Entity<EditorState>,
    /// 「编辑器 / 结果区」的竖向分栏状态（拖下边框调编辑器高度；尺寸由组件自管）。
    sql_split: Entity<ResizableState>,
    /// 自引用：右栏 / 网格渲染闭包要把点击送回来。
    view: WeakEntity<Self>,
    /// 订阅句柄（延长生命周期）。
    _subscriptions: Vec<Subscription>,

    /// 连接档案（kv 读入；保存时写回）。
    connections: Vec<ConnectionConfig>,
    /// 左栏「新建分组」创建的分组名（kv 单键，展示序即存储序）。
    groups: Vec<String>,
    /// 已建立的会话。
    sessions: HashMap<String, Session>,
    /// 打开的连接标签（顺序即展示顺序）。
    tabs: Vec<String>,
    /// 当前激活的连接 id。
    active: Option<String>,
    /// 主区模式与数据。
    mode: Mode,
    /// 当前浏览的表（限定名：schema + 表名）。
    selected_table: Option<TableRef>,
    table_page: usize,
    table_rows: Option<ResultSet>,
    /// 表浏览的列头排序；`None` = 只按主键的稳定序。
    ///
    /// 点列头来回切「升 → 降 → 取消」（[`SortSpec::next`]）；排序走**服务器侧
    /// ORDER BY**（不是只排当前页，那会让「第 2 页」变成第一页的重排）。
    table_sort: Option<SortSpec>,
    query_result: Option<QueryView>,
    /// 当前 SQL 结果可编辑时的目标表（[`duck_db::edit_target`] 判定 + 主键覆盖核验）。
    query_target: Option<TableRef>,
    /// 上面那张表的主键列名。
    query_pk: Vec<String>,
    /// 产出当前结果的那段 SQL（结果集上做编辑后要重跑一遍才看得到新值）。
    query_sql: Option<String>,
    /// 当前结果的列宽（任务收尾时算好）。
    column_widths: Vec<f32>,
    /// 选中行（两种模式共用一份索引）。
    selected_row: Option<usize>,
    /// 结果网格里**勾选**的行（表浏览的多选；批量改 / 批量删的输入）。
    ///
    /// 与 `selected_row` 是两件事：选中行 = 「正在看 / 正在编辑这一行」，
    /// 勾选 = 「要把这一批一起处理」。翻页 / 换表 / 重新执行都会清空。
    checked_rows: BTreeSet<usize>,
    /// 批量编辑的目标列（`None` = 还没选，应用按钮禁用）。
    batch_column: Option<String>,
    /// 批量编辑的新值输入框。
    batch_value: Entity<InputState>,
    /// 批量编辑是否写 `NULL`（勾上时忽略输入框内容）。
    batch_null: bool,
    /// 暂存的改格（`pending_edits` 模型）：攒着不落库，点「提交」时**一个事务**写下去。
    pending_edits: Vec<CellEdit>,
    /// 暂存的新增行（同上）。
    pending_inserts: Vec<InsertRow>,
    /// 当前选中表的主键列名（表页加载时内省；空 = 无主键 / 只读）。
    table_pk: Vec<String>,
    /// 当前选中表的 DDL（表页加载时顺带取；视图 / 无 DDL 为 `None`）。
    table_ddl: Option<String>,
    /// 当前选中表的索引（表级元信息，只在换表时取一次）。
    table_indexes: Vec<TableIndex>,
    /// 当前选中表的行数（口径见 [`RowCount`]；视图 / 不可数对象为 `None`）。
    table_row_count: Option<RowCount>,
    /// 表级元信息是为哪张表取的（翻页不重复 `COUNT(*)`）。
    table_meta_for: Option<TableRef>,
    /// 行编辑器（暂存式）：改动只存输入框，点保存才落库。
    row_editor: Option<RowEditor>,
    /// 查询历史（最近在前；kv 单键持久化）。
    history: Vec<String>,
    /// 查询历史面板是否展开。
    history_open: bool,
    /// AI 助手设置（kv 读入）。
    ai_settings: AiSettings,
    /// AI 助手面板是否展开。
    ai_open: bool,
    /// AI 设置行是否展开。
    ai_settings_open: bool,
    /// AI 提问输入框。
    ai_question: Entity<InputState>,
    /// AI 设置：base_url / model / api_key 输入框。
    ai_base_url: Entity<InputState>,
    ai_model: Entity<InputState>,
    ai_api_key: Entity<InputState>,
    /// 状态栏文案（连接 / 执行的即时反馈）。
    status: SharedString,
    /// 有后台数据库操作在跑：期间禁用会触发同类操作的按钮。
    busy: bool,
    /// 正在跑的**可取消**工作（有则工具条显示「停止」，见 [`RunningQuery`]）。
    running: Option<RunningQuery>,
    /// 本次运行是否被用户点过「停止」：决定 [`DbError::Cancelled`] 的文案
    /// （没点过就是超时守护开的枪）。
    stop_requested: bool,
    /// 本次运行的超时秒数（错误文案用；随 [`Self::spawn_db_guarded`] 的设置走）。
    run_timeout_secs: u64,
    /// 运行令牌自增源（见 [`RunningQuery::token`]）。
    run_seq: u64,
    /// **单次覆盖**的查询超时（秒）；`None` = 跟随连接默认 / 全局默认。
    ///
    /// 只活在本次页面会话里（不落盘、不进档案）：落盘的「连接默认」在连接详情表单的
    /// QUERY 分节。生效值三级回落见 [`Self::effective_timeout`]。
    query_timeout_override: Option<u64>,
    /// 「查询选项」弹窗里的超时输入框（弹窗每次打开时用当前生效值回填）。
    query_timeout_input: Entity<InputState>,
}

impl DatabasePage {
    /// 创建页面：读连接档案、展开左栏。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| {
            SidebarState::new(
                px(SIDEBAR_DEFAULT_WIDTH),
                px(*SIDEBAR_WIDTH_RANGE.start())..px(*SIDEBAR_WIDTH_RANGE.end()),
                cx,
            )
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "搜索连接…")));
        let sql = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(SQL_LANGUAGE)
                .placeholder(tr(cx, "输入 SQL，多条语句用分号分隔…"))
        });
        // SQL 补全：关键字 / 函数 / 表名（点号后取列名，查库走后台线程）。
        let page = cx.entity();
        sql.update(cx, |state, _cx| {
            state.lsp_mut().completion_provider =
                Some(Rc::new(SqlCompletionProvider::new(page.downgrade())));
        });
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(
            &search,
            |_this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            },
        ));

        Self {
            sidebar,
            search,
            sql,
            sql_split: cx.new(|_| ResizableState::default()),
            view: cx.entity().downgrade(),
            _subscriptions: subscriptions,
            connections: load_connections(cx),
            groups: load_groups(cx),
            sessions: HashMap::new(),
            tabs: Vec::new(),
            active: None,
            mode: Mode::Query,
            selected_table: None,
            table_page: 0,
            table_rows: None,
            table_sort: None,
            query_result: None,
            query_target: None,
            query_pk: Vec::new(),
            query_sql: None,
            column_widths: Vec::new(),
            selected_row: None,
            checked_rows: BTreeSet::new(),
            batch_column: None,
            batch_value: text_input(window, cx, tr(cx, "要写入该列的新值")),
            batch_null: false,
            pending_edits: Vec::new(),
            pending_inserts: Vec::new(),
            table_pk: Vec::new(),
            table_ddl: None,
            table_indexes: Vec::new(),
            table_row_count: None,
            table_meta_for: None,
            row_editor: None,
            history: load_history(cx),
            history_open: false,
            ai_settings: load_ai_settings(cx),
            ai_open: false,
            ai_settings_open: false,
            ai_question: text_input(window, cx, tr(cx, "用自然语言描述要查什么…")),
            ai_base_url: text_input(window, cx, tr(cx, "OpenAI 兼容接口地址")),
            ai_model: text_input(window, cx, tr(cx, "模型名")),
            ai_api_key: text_input(window, cx, tr(cx, "留空表示保持既有 key 不变")),
            status: tr(cx, "就绪").into(),
            busy: false,
            running: None,
            stop_requested: false,
            run_timeout_secs: QUERY_TIMEOUT.as_secs(),
            run_seq: 0,
            query_timeout_override: None,
            query_timeout_input: text_input(window, cx, tr(cx, "留空跟随连接默认")),
        }
    }

    /// 生效的查询超时：**单次覆盖 → 连接默认 → 全局 [`QUERY_TIMEOUT`]** 三级回落。
    fn effective_timeout(&self) -> Duration {
        self.query_timeout_override
            .or_else(|| {
                self.active_config()
                    .and_then(ConnectionConfig::default_timeout)
            })
            .map_or(QUERY_TIMEOUT, Duration::from_secs)
    }

    /// 应用 / 清除单次超时覆盖（查询选项弹窗的落点）。
    fn set_query_timeout(&mut self, secs: Option<u64>, cx: &mut Context<Self>) {
        self.query_timeout_override = secs;
        let text: SharedString = match secs {
            Some(secs) => trf(cx, "本次查询超时已设为 {} 秒", &[&secs.to_string()]).into(),
            None => tr(cx, "查询超时已恢复连接默认").into(),
        };
        self.status = text;
        cx.notify();
    }

    /// 「查询选项」弹窗：设本次会话的超时覆盖（留空 / 0 = 跟随连接默认）。
    ///
    /// 正文只有静态字段 + 一个输入框，所以**不需要**像连接详情那样挂视图实体：
    /// 输入框的正文由 `EditorState` 自己的视图重绘，弹窗不参与打字时的重排。
    fn open_query_options(&self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let input = self.query_timeout_input.clone();
        let override_secs = self.query_timeout_override;
        let connection_default = self
            .active_config()
            .and_then(ConnectionConfig::default_timeout);
        // 打开时回填：有覆盖填覆盖值，否则留空（占位符说明将跟随谁）。
        let seed = override_secs
            .map(|secs| secs.to_string())
            .unwrap_or_default();
        input.update(cx, |state, cx| state.set_value(seed, window, cx));

        let fallback_hint: SharedString = match connection_default {
            Some(secs) => trf(cx, "该连接默认 {} 秒。", &[&secs.to_string()]).into(),
            None => trf(
                cx,
                "该连接未设默认，回落全局 {} 秒。",
                &[&QUERY_TIMEOUT.as_secs().to_string()],
            )
            .into(),
        };
        let active_hint: SharedString = if override_secs.is_some() {
            tr(cx, "当前使用单次覆盖，清除后回到连接默认。").into()
        } else {
            tr(cx, "当前跟随连接默认。").into()
        };

        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let input_for_apply = input.clone();
            let input_for_content = input.clone();
            let view_for_apply = view.clone();
            let view_for_clear = view.clone();
            // 外层闭包是 `Fn`（可能被反复调用），捕获的提示语只能逐帧克隆进正文闭包。
            let active_for_content = active_hint.clone();
            let fallback_for_content = fallback_hint.clone();

            let footer = DialogFooter::new()
                .child(
                    button(
                        "db-query-options-clear",
                        tr(cx, "清除覆盖"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(override_secs.is_none())
                    .on_click({
                        let view = view_for_clear;
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.set_query_timeout(None, cx));
                            window.close_dialog(cx);
                        }
                    }),
                )
                .child(div().flex_1())
                .child(
                    button(
                        "db-query-options-cancel",
                        tr(cx, "取消"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    button(
                        "db-query-options-apply",
                        tr(cx, "应用"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click({
                        let view = view_for_apply;
                        move |_, window, cx| {
                            let text = input_for_apply.read(cx).value().trim().to_owned();
                            // 空 / 0 = 跟随连接默认（与连接表单里的口径一致）。
                            let parsed = if text.is_empty() {
                                Some(None)
                            } else {
                                match text.parse::<u64>() {
                                    Ok(0) => Some(None),
                                    Ok(secs) if secs <= MAX_TIMEOUT_SECS => Some(Some(secs)),
                                    _ => None,
                                }
                            };
                            let Some(secs) = parsed else {
                                notify::warning(
                                    trf(
                                        cx,
                                        "查询超时必须是 1-{} 的整数秒",
                                        &[&MAX_TIMEOUT_SECS.to_string()],
                                    ),
                                    window,
                                    cx,
                                );
                                return;
                            };
                            let _ = view.update(cx, |this, cx| this.set_query_timeout(secs, cx));
                            window.close_dialog(cx);
                        }
                    }),
                );

            dialog
                .w(px(EDIT_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(DialogButtonProps::default())
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .min_h_0()
                            .gap(form_gap(cx))
                            .child(modal_title_row(
                                "db-query-options-title",
                                &drag_for_content,
                                tr(cx, "查询选项"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "超时（秒）"), &input_for_content))
                            .child(field_hint(active_for_content.clone(), cx))
                            .child(field_hint(fallback_for_content.clone(), cx)),
                    ))
                })
                // 不带分号：`open_dialog` 的闭包必须**返回** `Dialog`。
                .footer(footer)
        });
    }

    /// 供 shell 挂左栏折叠态。
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
    }

    // ------------------------------------------------------------------
    // 新建连接流程：驱动选择弹窗 → 详情表单。
    // ------------------------------------------------------------------

    /// 打开「新建连接」驱动选择弹窗（dbflux 同形：分区卡片网格 + 搜索 + 底部操作）。
    fn open_new_connection_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.view.clone();
        let picker = cx.new(|cx| DriverPickerView::new(page, window, cx));
        self.open_picker_dialog(picker, window, cx);
    }

    /// 「新建连接」弹窗外壳：标题行 + 视图正文 + 底部「从文件导入… / 取消」。
    ///
    /// 正文只挂 [`DriverPickerView`] 实体 —— 搜索框要边打边重排网格，而弹窗的 `content`
    /// 闭包拿不到 `&mut DatabasePage`，实测也够不到宿主的 `notify()`（见该结构体的说明）。
    fn open_picker_dialog(
        &self,
        picker: Entity<DriverPickerView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.view.clone();
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let view_for_import = view.clone();
            let picker_for_content = picker.clone();
            let drag_for_content = drag.clone();

            let footer = DialogFooter::new()
                .child(
                    button(
                        "db-picker-import",
                        tr(cx, "从文件导入…"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let _ = view_for_import.update(cx, |this, cx| {
                            this.import_connections(window, cx);
                        });
                    }),
                )
                .child(div().flex_1())
                .child(
                    button(
                        "db-picker-cancel",
                        tr(cx, "取消"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                );

            dialog
                .w(px(PICKER_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(DialogButtonProps::default())
                .content(move |content, _, cx| {
                    // 正文是**视图实体**，逐帧克隆挂进来即可 —— `content` 闭包是 `Fn`，
                    // 元素树搬进来只够用一次，`Entity` 可以反复克隆。
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .min_h_0()
                            .child(modal_title_row(
                                "db-picker-title",
                                &drag_for_content,
                                tr(cx, "新建连接"),
                                cx,
                            ))
                            .child(picker_for_content.clone()),
                    ))
                })
                // 不带分号：`open_dialog` 的闭包必须**返回** `Dialog`。
                .footer(footer)
        });
    }

    /// 连接详情弹窗（picker 的**子页**，dbx 同形）。
    ///
    /// 表单不再占主区：picker 选中引擎（或列表铅笔）→ 关掉当前弹窗 → 同一个窗口里
    /// 开出详情弹窗，正文就是 [`EditFormView`] 这个视图实体，返回键回到 picker。
    /// 脚注「测试连接（编辑态带删除）｜取消 / 保存连接」由弹窗外壳挂。
    ///
    /// ⚠️ 两条硬约束（都试错过）：
    /// 1. 正文必须挂成视图实体（理由见 [`EditFormView`] 的说明）：环境 / SSL / SSH 这些
    ///    点了要变色的控件靠视图自己 `notify` 重绘，宿主页面的 `notify()` 到不了弹窗正文；
    /// 2. 页面**不持有**表单实体，弹窗是唯一所有者 —— 关窗即释放，同时避开
    ///    「在自身更新期间析构」那类崩溃。
    fn open_edit_dialog(
        &self,
        form: Entity<EditFormView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = self.view.clone();
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let form_for_test = form.clone();
            let form_for_content = form.clone();
            let form_for_save = form.clone();
            let page_for_delete = page.clone();

            // 删除按钮只在编辑态出现。**先建好再按需挂**：`when_some` 的闭包若要
            // `move` 进 `cx`（`&mut App` 不 Copy），后面的 `tr(cx, …)` 会被借用检查拦下。
            let delete_slot = form.read(cx).id.clone().map(|id| {
                icon_text_button(
                    "db-edit-delete",
                    tr(cx, "删除连接"),
                    Icon::new(DuckIcon::Trash2).size(active_theme(cx).extended().icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = page_for_delete
                        .update(cx, |page, cx| page.delete_connection(&id, window, cx));
                    window.close_dialog(cx);
                })
                .into_any_element()
            });

            let footer = DialogFooter::new()
                .child(
                    icon_text_button(
                        "db-edit-test",
                        tr(cx, "测试连接"),
                        Icon::new(DuckIcon::Zap).size(active_theme(cx).extended().icon.sm),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        form_for_test.update(cx, |form, cx| form.test(window, cx));
                    }),
                )
                .when_some(delete_slot, |footer, button| footer.child(button))
                .child(div().flex_1())
                .child(
                    button(
                        "db-edit-cancel",
                        tr(cx, "取消"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    button(
                        "db-edit-save",
                        tr(cx, "保存连接"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        // 校验不过时 `save` 返回 false（提示贴在字段上），弹窗留下；
                        // 只有真的写盘成功才关。
                        let saved = form_for_save.update(cx, |form, cx| form.save(window, cx));
                        if saved {
                            window.close_dialog(cx);
                        }
                    }),
                );

            dialog
                .w(px(EDIT_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(DialogButtonProps::default())
                .content(move |content, _, _cx| {
                    // 每帧把视图实体挂进来（`Fn` 闭包，只能逐帧克隆）。
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex().w_full().min_h_0().child(form_for_content.clone()),
                    ))
                })
                // 不带分号：`open_dialog` 的闭包必须**返回** `Dialog`。
                .footer(footer)
        });
    }

    /// 选定引擎后进入详情表单（同窗切换为表单子页，不离开弹窗）。
    fn start_new_connection(
        &mut self,
        kind: DatabaseKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = EditFormView::new(self.view.clone(), kind, window, cx);
        let form = cx.new(|_| form);
        self.open_edit_dialog(form, window, cx);
    }

    // ------------------------------------------------------------------
    // 后台任务包装：所有阻塞数据库调用都从这里走。
    // ------------------------------------------------------------------

    /// 把阻塞调用丢进后台执行器，完成后回主线程跑 `done`（拿到窗口与上下文）。
    ///
    /// 真正的活先过全局并发闸门（[`db_op_gate`]，上限 [`MAX_CONCURRENT_DB_OPS`]）：
    /// 超限时**排队**而不是拒绝，界面看到的仍是「一次点了就一定会跑」。
    /// 排队时间计入调用方那套超时预算——与「等连接互斥锁」同性质，都不另开表。
    fn spawn_db<T, F, D>(&mut self, cx: &mut Context<Self>, work: F, done: D)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    {
        self.busy = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            // 名额凭证活到闭包返回（`let _permit = ...` 不是 `let _ = ...`：
            // 后者会立刻析构，名额当场就还回去了）。
            let _permit = db_op_gate().acquire().await;
            work()
        });
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let value = task.await;
            cx.update(move |cx| {
                crate::update::on_notify_window(cx, move |window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.busy = false;
                        done(this, value, window, cx);
                        cx.notify();
                    });
                });
            });
        })
        .detach();
    }

    /// 跑一件**可取消 + 带超时**的数据库工作（读路径专用）。
    ///
    /// 与 [`Self::spawn_db`] 同形，差别只在：开工前抓一条**带外**取消句柄并挂超时守护
    /// （工具条据此把「执行」换成「停止」），收工前撤销守护。
    ///
    /// ⚠️ 只给**读路径**（SQL 执行、表页加载）用：写路径中途取消会留下半截数据，
    /// 不提供取消入口（设计稿 §4.4 的取舍）。
    fn spawn_db_guarded<T, F, D>(
        &mut self,
        cx: &mut Context<Self>,
        session_id: String,
        cancel: CancelHandle,
        timeout: Duration,
        work: F,
        done: D,
    ) where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    {
        let timer = CancelTimer::arm(&cancel, timeout);
        self.run_seq = self.run_seq.wrapping_add(1);
        let token = self.run_seq;
        self.running = Some(RunningQuery {
            token,
            session_id,
            cancel,
            timer,
        });
        self.stop_requested = false;
        self.run_timeout_secs = timeout.as_secs();
        self.spawn_db(cx, work, move |this, value, window, cx| {
            // 先收工（撤销守护 + 摘掉 running）再交结果：
            // ① 晚到的那一枪绝不能打到下一条语句上；
            // ② `done` 里可能立刻发起下一次运行（如保存后刷新表页），
            //    此时 `running` 应已清空，否则会被这里的清理误伤。
            // **只清理自己那次**：叠加运行（连续点表页）时，先回来的那次不许动后来者的守护。
            // 「这次是用户停的还是超时停的」记在 `stop_requested` / `run_timeout_secs`，
            // 它们保留到下一次 `arm` 覆盖，`done` 里读得到。
            if this
                .running
                .as_ref()
                .is_some_and(|running| running.token == token)
            {
                if let Some(running) = this.running.as_mut() {
                    running.timer.disarm();
                }
                this.running = None;
            }
            done(this, value, window, cx);
        });
    }

    /// 工具条「停止」：把取消送到正在跑的那条语句（不占连接锁，后台线程正握着也没关系）。
    fn stop_running(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(running) = self.running.as_ref() else {
            return;
        };
        tracing::info!(session = %running.session_id, "db query cancel requested");
        if let Err(error) = running.cancel.cancel() {
            self.status = trf(cx, "停止失败：{}", &[&error.to_string()]).into();
            cx.notify();
            return;
        }
        self.stop_requested = true;
        self.status = tr(cx, "正在停止…").into();
        notify::info(tr(cx, "已向数据库发送取消请求。"), window, cx);
        cx.notify();
    }

    /// 取消类错误的中文文案：点过「停止」就是用户停的，否则是超时守护开的枪。
    fn cancelled_status(&self, cx: &App) -> SharedString {
        if self.stop_requested {
            tr(cx, "已停止。").into()
        } else {
            trf(
                cx,
                "查询超时（{} 秒）已被中止。",
                &[&self.run_timeout_secs.to_string()],
            )
            .into()
        }
    }

    // ------------------------------------------------------------------
    // 连接档案 CRUD。
    // ------------------------------------------------------------------

    /// 保存连接档案（表单视图校验通过后调）：按 id 更新或追加，再落 kv。
    fn upsert_connection(&mut self, cfg: ConnectionConfig, cx: &mut Context<Self>) {
        let id = cfg.id.clone();
        match self
            .connections
            .iter_mut()
            .find(|existing| existing.id == id)
        {
            Some(existing) => *existing = cfg,
            None => self.connections.push(cfg),
        }
        self.persist_connections(cx);
        cx.notify();
    }

    /// 「测试连接」：用表单当前值探活（不保存、不开会话），报告版本或错误。
    ///
    /// 只收「表单已经读好」的三件套：配置 + 口令 + SSH 口令（口令的取值与回落
    /// 语义在 [`EditFormView::prepared`] 里，测试与保存共用同一套）。
    fn spawn_probe(
        &mut self,
        cfg: ConnectionConfig,
        password: Option<String>,
        ssh_password: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.status = trf(cx, "正在测试 {}…", &[&cfg.display_name()]).into();
        cx.notify();
        self.spawn_db(
            cx,
            move || probe_connection(&cfg, password, ssh_password),
            |this, result, window, cx| match result {
                Ok(version) => {
                    this.status = trf(cx, "测试成功：{}", &[&version]).into();
                    notify::success(trf(cx, "连接成功：{}", &[&version]), window, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "测试失败：{}", &[&error.to_string()]).into();
                    notify::error(trf(cx, "连接失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 删除连接（档案 + 凭据文件里的口令 + 已开会话）。
    fn delete_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let confirmed = duck_platform::confirm(
            tr(cx, "删除连接"),
            tr(cx, "连接档案与其保存的口令都会被删除，确定继续吗？"),
        );
        if !confirmed {
            return;
        }
        self.connections.retain(|cfg| cfg.id != id);
        self.persist_connections(cx);
        let _ = delete_password(id);
        self.close_tab(id, cx);
        notify::success(tr(cx, "连接已删除。"), window, cx);
    }

    /// 把档案数组写回 kv。
    fn persist_connections(&self, cx: &App) {
        match serde_json::to_string(&self.connections) {
            Ok(json) => duck_store::kv_put(cx, KEY_CONNECTIONS, json),
            Err(error) => tracing::warn!(%error, "serialize db connections failed"),
        }
    }

    /// 把分组数组写回 kv。
    fn persist_groups(&self, cx: &App) {
        match serde_json::to_string(&self.groups) {
            Ok(json) => duck_store::kv_put(cx, KEY_GROUPS, json),
            Err(error) => tracing::warn!(%error, "serialize db groups failed"),
        }
    }

    /// 把查询历史写回 kv。
    fn persist_history(&self, cx: &App) {
        match serde_json::to_string(&self.history) {
            Ok(json) => duck_store::kv_put(cx, KEY_HISTORY, json),
            Err(error) => tracing::warn!(%error, "serialize db query history failed"),
        }
    }

    /// 把某条档案的某个字段改掉并落盘（当前用在「记住上次浏览的命名空间」）。
    ///
    /// 走「改 `self.connections` 再整份持久化」而不是直接写 kv：页面里那份数组是
    /// 唯一的真源，绕过它会出现「界面上是 A、磁盘上是 B」。
    fn update_connection_field(
        &mut self,
        id: &str,
        cx: &App,
        edit: impl FnOnce(&mut ConnectionConfig),
    ) {
        let mut touched = false;
        for cfg in &mut self.connections {
            if cfg.id == id {
                edit(cfg);
                touched = true;
                break;
            }
        }
        if touched {
            self.persist_connections(cx);
        }
    }

    /// 左栏「新建分组」：弹窗输入分组名，创建后空分组也显示在左栏。
    fn open_new_group_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = text_input(window, cx, tr(cx, "分组名"));
        let drag = modal_drag(cx);
        let view = self.view.clone();
        // `open_dialog` 的闭包是 `Fn + 'static`：`input` 会被整个搬进去，因此外面再留
        // 一份句柄用于开窗后聚焦（实体是 `Rc` 语义，两份指向同一个）。
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            // 形参显式标注：闭包体里 `input.read(cx)` 只要求 `&App`，若不标注，推断会
            // 把 `cx` 定成 `&App`，随后 `notify::warning(.., cx)` 需要 `&mut App` 就报
            // 「types differ in mutability」。
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    notify::warning(tr(cx, "请填写分组名。"), window, cx);
                    return false;
                }
                let _ = view.update(cx, |this, cx| {
                    if !this.groups.contains(&name) {
                        this.groups.push(name);
                        this.persist_groups(cx);
                    }
                    cx.notify();
                });
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            // 外层闭包是 `Fn`（每次重绘都会重跑）：`confirm` 与 `input` 都只能**克隆**
            // 取用，不能把捕获值搬走 —— 搬一次之后第二次调用就没得搬了。
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
                                "db-group-title",
                                &drag_for_content,
                                tr(cx, "新建分组"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "分组名"), &input_for_content)),
                    ))
                })
                // 不带分号：闭包必须返回 `Dialog`。
                .footer(dialog_footer(
                    "db-group",
                    tr(cx, "创建"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |state, cx| state.focus(window, cx));
    }

    /// 左栏「刷新」：重读 kv 档案（含外部改动），活跃会话重取目录。
    ///
    /// 不接 `Window`：刷新本身不弹通知（结论落在状态栏），目录重取走
    /// [`Self::spawn_db`] 的回投，回调自带窗口。
    fn refresh_state(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.connections = load_connections(cx);
        self.groups = load_groups(cx);
        let Some(session) = self.active_session() else {
            self.status = tr(cx, "已刷新。").into();
            cx.notify();
            return;
        };
        let conn = session.conn.clone();
        // 重新内省的是**当前浏览的命名空间**（不是引擎缺省的那个）：用户在 schema
        // 下拉里切到别的库之后刷新，不该被弹回缺省库。
        let schema = session.schema.clone();
        self.spawn_db(
            cx,
            move || -> Result<(Vec<String>, Vec<CatalogTable>), DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                // 命名空间清单也跟着重取（刷新要能看见别人新建的库）。
                let schemas = guard.list_schemas().unwrap_or_default();
                let catalog = guard.catalog(schema.as_deref())?;
                Ok((schemas, catalog))
            },
            |this, result, _window, cx| match result {
                Ok((schemas, catalog)) => {
                    if let Some(session) = this.active_session_mut() {
                        session.schemas = schemas;
                        session.catalog = catalog;
                    }
                    this.status = tr(cx, "已刷新。").into();
                }
                Err(error) => {
                    this.status = trf(cx, "刷新失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 切换当前会话浏览的命名空间：重取目录并回第一页。
    ///
    /// 选中的命名空间写回档案（`ConnectionConfig::schema`），下次连接直接落在这里。
    fn select_schema(&mut self, schema: Option<String>, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if session.schema == schema {
            return;
        }
        session.schema = schema.clone();
        let conn = session.conn.clone();
        // 换库 = 换整套对象与列：选中表、勾选、暂存改动全部作废。
        self.selected_table = None;
        self.table_rows = None;
        self.table_page = 0;
        self.mode = Mode::Query;
        self.checked_rows.clear();
        self.pending_edits.clear();
        self.pending_inserts.clear();
        self.row_editor = None;
        self.selected_row = None;
        self.table_meta_for = None;
        self.table_indexes.clear();
        self.table_row_count = None;
        self.table_ddl = None;
        self.update_connection_field(&id, cx, |cfg| cfg.schema = schema.clone());
        self.status = tr(cx, "正在切换命名空间…").into();
        cx.notify();
        let for_conn = schema.clone();
        self.spawn_db(
            cx,
            move || -> Result<Vec<CatalogTable>, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                // 先让**连接自己**记住落点再内省：MongoDB 的「当前库」是客户端概念，
                // 不告诉驱动的话，编辑器里的命令还会落在旧库（详见
                // `Connection::set_namespace`）。
                guard.set_namespace(for_conn.as_deref());
                guard.catalog(for_conn.as_deref())
            },
            move |this, result, _window, cx| match result {
                Ok(catalog) => {
                    if let Some(session) = this.active_session_mut() {
                        session.catalog = catalog;
                    }
                    this.status = tr(cx, "已切换命名空间。").into();
                }
                Err(error) => {
                    this.status = trf(cx, "切换命名空间失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // 连接生命周期。
    // ------------------------------------------------------------------

    /// 连接一条档案（后台建连 + 版本 + 目录）。
    fn connect_entry(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            notify::info(tr(cx, "有数据库操作正在进行，请稍候。"), window, cx);
            return;
        }
        let Some(cfg) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        self.status = trf(cx, "正在连接 {}…", &[&cfg.display_name()]).into();
        cx.notify();
        let done_cfg = cfg.clone();
        self.spawn_db(
            cx,
            move || -> Result<Session, DbError> {
                let password = load_password(&cfg.id).unwrap_or(None);
                // SSH 隧道（SQLite 走本地文件，不需要）：先建隧道再指回 127.0.0.1。
                let tunnel = match cfg.ssh_tunnel.as_ref() {
                    Some(tunnel_cfg) if cfg.kind.needs_host() => {
                        let ssh_password = load_ssh_password(&cfg.id).unwrap_or(None);
                        Some(Arc::new(SshTunnel::open(SshTunnelSpec {
                            jump_host: tunnel_cfg.host.clone(),
                            jump_port: tunnel_cfg.port,
                            user: tunnel_cfg.user.clone(),
                            remote_host: tunnel_cfg.remote_host.clone(),
                            remote_port: cfg.port.unwrap_or_else(|| {
                                cfg.kind.default_port().expect("network kind has a port")
                            }),
                            password: ssh_password,
                        })?))
                    }
                    _ => None,
                };
                let mut effective = cfg.clone();
                if let Some(tunnel) = tunnel.as_ref() {
                    effective.host = "127.0.0.1".into();
                    effective.port = Some(tunnel.local_port());
                }
                let mut conn = Connection::connect(&effective, password.as_deref())?;
                // 连接级只读档：由**服务器**兜底拒绝写（不是界面软拦），建连后立刻生效。
                if effective.read_only {
                    conn.set_read_only(true)?;
                }
                let version = conn.server_version().unwrap_or_default();
                // 命名空间清单与目录各取一次；目录落在档案记住的那个命名空间上。
                let schemas = conn.list_schemas().unwrap_or_default();
                let catalog = conn
                    .catalog(effective.schema.as_deref())
                    .unwrap_or_default();
                Ok(Session {
                    conn: Arc::new(Mutex::new(conn)),
                    version,
                    catalog,
                    schemas,
                    schema: effective.schema.clone(),
                    tunnel,
                    in_tx: false,
                })
            },
            move |this, result, window, cx| {
                let cfg = &done_cfg;
                match result {
                    Ok(session) => {
                        let id = cfg.id.clone();
                        this.sessions.insert(id.clone(), session);
                        if !this.tabs.contains(&id) {
                            this.tabs.push(id.clone());
                        }
                        this.activate(&id, cx);
                        this.status = trf(cx, "已连接 {}", &[&cfg.display_name()]).into();
                        notify::success(trf(cx, "已连接 {}。", &[&cfg.display_name()]), window, cx);
                    }
                    Err(error) => {
                        this.status = trf(cx, "连接失败：{}", &[&error.to_string()]).into();
                        notify::error(
                            trf(
                                cx,
                                "连接 {} 失败：{}",
                                &[&cfg.display_name(), &error.to_string()],
                            ),
                            window,
                            cx,
                        );
                    }
                }
            },
        );
    }

    /// 激活某个已连接标签：切换视图数据源。
    fn activate(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.sessions.contains_key(id) {
            return;
        }
        self.active = Some(id.to_owned());
        self.mode = Mode::Query;
        self.selected_table = None;
        self.table_page = 0;
        self.table_rows = None;
        self.table_sort = None;
        self.query_result = None;
        self.query_target = None;
        self.query_pk.clear();
        self.query_sql = None;
        self.column_widths = Vec::new();
        self.table_pk = Vec::new();
        self.table_ddl = None;
        self.table_indexes.clear();
        self.table_row_count = None;
        self.table_meta_for = None;
        // 换连接 = 换整套对象：勾选与暂存改动都按**旧库的下标 / 主键**记，必须作废。
        self.checked_rows.clear();
        self.batch_column = None;
        self.pending_edits.clear();
        self.pending_inserts.clear();
        self.row_editor = None;
        self.selected_row = None;
        cx.notify();
    }

    /// 断开并关闭一个标签。
    fn close_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        self.sessions.remove(id);
        self.tabs.retain(|tab| tab != id);
        if self.active.as_deref() == Some(id) {
            self.active = self.tabs.first().cloned();
            // 关掉的正是当前标签：视图数据整体作废，交给 `activate` 之外这里清一遍。
            self.mode = Mode::Query;
            self.selected_table = None;
            self.table_rows = None;
            self.table_sort = None;
            self.query_result = None;
            self.query_target = None;
            self.query_pk.clear();
            self.query_sql = None;
            self.column_widths = Vec::new();
            self.table_meta_for = None;
            self.table_indexes.clear();
            self.table_row_count = None;
            self.table_ddl = None;
            self.checked_rows.clear();
            self.pending_edits.clear();
            self.pending_inserts.clear();
            self.row_editor = None;
            self.selected_row = None;
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 查询与浏览。
    // ------------------------------------------------------------------

    /// 执行 SQL 编辑器里的内容（工具条「执行」）。
    fn run_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql.read(cx).value().trim().to_owned();
        if sql.is_empty() {
            notify::info(tr(cx, "请先输入 SQL。"), window, cx);
            return;
        }
        // 危险语句（删库 / 全表清改）先确认；普通写语句由用户直接执行。
        let risk = risk_of_statements(&split_statements(&sql));
        if risk == SqlRisk::Dangerous
            && !duck_platform::confirm(
                tr(cx, "执行危险 SQL"),
                tr(cx, "这段 SQL 包含删库 / 全表清改类语句，确定继续吗？"),
            )
        {
            return;
        }
        self.execute_sql(sql, window, cx);
    }

    /// 真正执行一段 SQL：**可取消 + 超时**，收尾时顺带更新事务状态与可编辑目标。
    ///
    /// 工具条「执行」与状态栏的「提交 / 回滚」共用这一条路径，所以「谁在跑、怎么停、
    /// 事务还在不在」只有一份真相。
    /// 复制当前选中行为 TSV（没有选中行时静默——与系统 ⌘C 在空选区上的行为一致）。
    fn copy_selected_row(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.selected_row else {
            return;
        };
        let Some(result) = self.current_result() else {
            return;
        };
        let Some(row) = result.rows.get(index) else {
            return;
        };
        self.put_on_clipboard(row_as_tsv(row), tr(cx, "已复制该行。"), cx);
    }

    /// 写剪贴板并把结果报到状态栏（复制类动作共用的收尾）。
    fn put_on_clipboard(&mut self, text: String, note: &'static str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.status = note.into();
        cx.notify();
    }

    /// 把编辑器里的 SQL 重新排版（关键字大写、空白收敛）。
    ///
    /// 排版本身在 [`duck_db::format_sql`] 里：解析得了的换成规范形式，解析不了的**原样
    /// 留下**并在状态栏报出数量 —— 绝不静默改写用户手上跑得通的半截语句。
    /// 一字未改时不回写编辑器（避免无谓地移动光标与撤销栈）。
    fn format_editor_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let original = self.sql.read(cx).value().to_string();
        let formatted = format_sql(&original);
        let changed = formatted.text != original;
        if changed {
            self.sql.update(cx, |state, cx| {
                state.set_value(formatted.text.clone(), window, cx);
            });
        }
        let skipped = formatted.kept_verbatim.len();
        self.status = if skipped == 0 {
            if changed {
                tr(cx, "已格式化。").into()
            } else {
                tr(cx, "已格式化（没有变化）。").into()
            }
        } else {
            trf(
                cx,
                "已格式化；{} 段解析不了，保持原样。",
                &[&skipped.to_string()],
            )
            .into()
        };
        cx.notify();
    }

    /// 对编辑器里的单条语句求执行计划（`EXPLAIN`）。
    ///
    /// 走的是与普通查询**同一条**执行路径（同一把取消句柄、超时、结果网格与截断提示），
    /// 但不动编辑器内容：按一次看一眼计划，光标与语句都还在原处。
    fn explain_editor_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql.read(cx).value().to_string();
        match explain_sql(&sql) {
            Ok(explain) => self.execute_sql(explain, window, cx),
            Err(ExplainSkip::NotASingleStatement) => {
                notify::info(tr(cx, "执行计划一次只看一条语句。"), window, cx);
            }
            Err(ExplainSkip::NoPlanForThisStatement) => {
                notify::info(tr(cx, "这条语句没有执行计划。"), window, cx);
            }
        }
    }

    fn execute_sql(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            notify::info(tr(cx, "请先连接一个数据库。"), window, cx);
            return;
        };
        if self.running.is_some() {
            notify::info(
                tr(cx, "上一条语句还在跑，先等它结束或点「停止」。"),
                window,
                cx,
            );
            return;
        }
        let session_id = self.active.clone().unwrap_or_default();
        let conn = session.conn.clone();
        // 取消句柄必须在开工前抓：跑起来之后连接锁在后台线程手里。
        let cancel = conn.lock().expect("连接锁不应中毒").cancel_handle();
        let statements = split_statements(&sql);
        let timeout = self.effective_timeout();
        // 只有「单条、单表、无分组」的 SELECT 才有资格被当作可编辑结果集；
        // 具体能不能改还要等结果回来核主键覆盖（`covers_columns`）。
        let target = if statements.len() == 1 {
            edit_target(&sql).map(|EditTarget { table, schema }| TableRef {
                schema,
                name: table,
            })
        } else {
            None
        };
        let history_sql = sql.clone();
        self.status = tr(cx, "正在执行…").into();
        cx.notify();
        self.spawn_db_guarded(
            cx,
            session_id.clone(),
            cancel,
            timeout,
            move || {
                let started = Instant::now();
                let mut guard = conn.lock().expect("连接锁不应中毒");
                let outcome = guard.execute(&sql, QUERY_ROW_CAP)?;
                // 可编辑目标：表有主键，且主键列**真的在结果里**（`SELECT name FROM t`
                // 单表合法，但改完没法定回哪一行）。
                let edit_meta = match (&target, outcome.result.as_ref()) {
                    (Some(table), Some(result)) => {
                        let pk = guard.table_primary_key(table).unwrap_or_default();
                        let projected: Vec<String> = result
                            .columns
                            .iter()
                            .map(|column| column.name.clone())
                            .collect();
                        (!pk.is_empty() && covers_columns(&projected, &pk))
                            .then(|| (table.clone(), pk))
                    }
                    _ => None,
                };
                Ok((outcome, started.elapsed().as_millis(), edit_meta))
            },
            move |this, result, window, cx| match result {
                Ok((outcome, elapsed_ms, edit_meta)) => {
                    // 事务状态：`BEGIN` 挂上、`COMMIT` / `ROLLBACK` 摘掉（状态栏据此显示）。
                    if let Some(session) = this.sessions.get_mut(&session_id) {
                        for stmt in &statements {
                            match tx_effect(stmt) {
                                Some(TxEffect::Begin) => session.in_tx = true,
                                Some(TxEffect::End) => session.in_tx = false,
                                None => {}
                            }
                        }
                    }
                    let status = match &outcome.result {
                        Some(result) => {
                            let mut text = trf(
                                cx,
                                "{} 行 · {} 条语句 · {} ms",
                                &[
                                    &result.row_count().to_string(),
                                    &outcome.statements_run.to_string(),
                                    &elapsed_ms.to_string(),
                                ],
                            );
                            if result.truncated {
                                text.push_str(" · ");
                                text.push_str(tr(cx, "已达上限，结果被截断"));
                            }
                            text
                        }
                        None => trf(
                            cx,
                            "影响 {} 行 · {} 条语句 · {} ms",
                            &[
                                &outcome.affected.to_string(),
                                &outcome.statements_run.to_string(),
                                &elapsed_ms.to_string(),
                            ],
                        ),
                    };
                    push_history(&mut this.history, &history_sql);
                    this.persist_history(cx);
                    this.install_query_result(outcome, edit_meta, history_sql);
                    this.status = status.into();
                }
                Err(error) => {
                    // 「被取消」分两种来源：用户点了停止，或超时守护到点。
                    if matches!(error, DbError::Cancelled(_)) {
                        let mut text = this.cancelled_status(cx).to_string();
                        if statements.len() > 1 {
                            // 多语句批次是逐条跑的：被中断的那条之后没跑，但之前的已经生效了。
                            text.push_str(tr(cx, "更早的语句可能已生效。"));
                        }
                        let text = SharedString::from(text);
                        this.status = text.clone();
                        notify::info(text, window, cx);
                    } else {
                        this.status = trf(cx, "执行失败：{}", &[&error.to_string()]).into();
                        notify::error(
                            trf(cx, "SQL 执行失败：{}", &[&error.to_string()]),
                            window,
                            cx,
                        );
                    }
                }
            },
        );
    }

    /// 选中一张表并翻到第一页。
    fn open_table(&mut self, table: TableRef, _window: &mut Window, cx: &mut Context<Self>) {
        if self.active_session().is_none() {
            return;
        }
        self.selected_table = Some(table);
        self.table_page = 0;
        self.table_sort = None;
        self.mode = Mode::Table;
        self.selected_row = None;
        self.row_editor = None;
        // 换表：勾选、暂存改动、「批量编辑的目标列」都作废（表与列集合都换了）。
        self.checked_rows.clear();
        self.batch_column = None;
        self.pending_edits.clear();
        self.pending_inserts.clear();
        // 换表先把上一张表的元信息清干净，免得右栏在新数据回来前展示旧索引；
        // 同时让 `table_meta_for` 失配，强制本次重新取一遍（而不是沿用旧缓存）。
        self.table_indexes.clear();
        self.table_row_count = None;
        self.table_ddl = None;
        self.table_meta_for = None;
        self.load_table_page(cx);
    }

    /// 加载当前表页（后台）。
    fn load_table_page(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let Some(table) = self.selected_table.clone() else {
            return;
        };
        let page = self.table_page;
        let sort = self.table_sort.clone();
        let session_id = self.active.clone().unwrap_or_default();
        let conn = session.conn.clone();
        // 表级元信息（索引 / 行数）只在**换表**时取一次：翻页时重复 `COUNT(*)`
        // 在大表上是纯浪费（`table_meta_for` 记住已经为哪张表取过）。
        let wants_meta = self.table_meta_for.as_ref() != Some(&table);
        let table_for_work = table.clone();
        // 读表也是「可能很久」的路径（大表 + 远端库），同样给取消与超时。
        let cancel = conn.lock().expect("连接锁不应中毒").cancel_handle();
        let timeout = self.effective_timeout();
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
                    // 元信息失败不该拖垮表页：索引 `.unwrap_or_default()`，
                    // 行数 `.ok()`（视图 / 不可数对象给不出数字）。
                    (
                        guard.table_indexes(&table).unwrap_or_default(),
                        guard.table_row_count(&table).ok(),
                    )
                });
                guard
                    .table_page(&table, PAGE_SIZE, page as u64 * PAGE_SIZE as u64, &order)
                    .map(|result| (result, pk, ddl, meta))
            },
            move |this, result, window, cx| match result {
                Ok((result, pk, ddl, meta)) => {
                    let rows = result.row_count();
                    this.column_widths = compute_column_widths(&result);
                    this.table_rows = Some(result);
                    this.table_pk = pk;
                    this.table_ddl = ddl;
                    if let Some((indexes, row_count)) = meta {
                        this.table_indexes = indexes;
                        this.table_row_count = row_count;
                        this.table_meta_for = Some(table.clone());
                    }
                    this.row_editor = None;
                    this.selected_row = None;
                    // 翻页 / 换表 / 刷新换了结果集：勾选按旧下标记，必须清掉。
                    this.checked_rows.clear();
                    this.status = trf(
                        cx,
                        "第 {} 页 · 本页 {} 行",
                        &[&(this.table_page + 1).to_string(), &rows.to_string()],
                    )
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
    fn toggle_table_sort(&mut self, column: &str, cx: &mut Context<Self>) {
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

    fn table_prev_page(&mut self, cx: &mut Context<Self>) {
        if self.table_page > 0 {
            self.table_page -= 1;
            self.load_table_page(cx);
        }
    }

    fn table_next_page(&mut self, cx: &mut Context<Self>) {
        let full = self
            .table_rows
            .as_ref()
            .is_some_and(|rows| rows.row_count() >= PAGE_SIZE);
        if full {
            self.table_page += 1;
            self.load_table_page(cx);
        }
    }

    fn refresh_table_page(&mut self, cx: &mut Context<Self>) {
        // 「刷新」按重来理解：行数与索引一并重取（删改之后行数就变了）。
        self.table_meta_for = None;
        self.load_table_page(cx);
    }

    /// 装配查询结果展示态（SQL 执行与 AI 自动执行共用）。
    ///
    /// `edit_meta` 为 `Some` 时这份结果集可以在网格上直接改（单表 + 主键齐全）；
    /// `sql` 是产出它的语句，编辑后重跑用。
    fn install_query_result(
        &mut self,
        outcome: ExecOutcome,
        edit_meta: Option<(TableRef, Vec<String>)>,
        sql: String,
    ) {
        self.mode = Mode::Query;
        self.column_widths = outcome
            .result
            .as_ref()
            .map(compute_column_widths)
            .unwrap_or_default();
        self.query_result = Some(QueryView {
            result: outcome.result,
        });
        match edit_meta {
            Some((table, pk)) => {
                self.query_target = Some(table);
                self.query_pk = pk;
            }
            None => {
                self.query_target = None;
                self.query_pk.clear();
            }
        }
        // 只有单条读语句才记 SQL：多语句批次重跑会把写语句再跑一遍。
        self.query_sql = Some(sql);
        self.selected_row = None;
        self.row_editor = None;
        // 结果集换了：旧的勾选按「旧结果集的下标」记，必须清掉；暂存改动同理
        // （它们按旧行号定位，重跑后行号可能指向别的行）。
        self.checked_rows.clear();
        self.pending_edits.clear();
        self.pending_inserts.clear();
    }

    /// 当前结果集的**可编辑来源**；不可编辑（无表 / 无主键 / 列对不上）返回 `None`。
    fn edit_source(&self) -> Option<EditSource<'_>> {
        match self.mode {
            Mode::Table => Some(EditSource {
                result: self.table_rows.as_ref()?,
                table: self.selected_table.as_ref()?,
                pk: &self.table_pk,
            }),
            Mode::Query => Some(EditSource {
                result: self.query_result.as_ref()?.result.as_ref()?,
                table: self.query_target.as_ref()?,
                pk: &self.query_pk,
            }),
        }
    }

    /// 编辑落库后重建当前视图：表浏览重取这一页，SQL 结果重跑那条语句。
    ///
    /// 重跑是安全的：能走到编辑这条路的语句必然是单表只读 SELECT（[`edit_target`] 的
    /// 前提），不可能是带副作用的语句。
    fn reload_edit_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Table => {
                self.table_meta_for = None;
                self.load_table_page(cx);
            }
            Mode::Query => {
                if let Some(sql) = self.query_sql.clone() {
                    self.execute_sql(sql, window, cx);
                }
            }
        }
    }

    /// 指定行的编辑器数据源：列名 + 展示原值 + 是否 NULL；不可编辑时返回 `None`。
    fn row_editor_specs(&self, row_index: usize) -> Option<Vec<(String, String, bool)>> {
        let source = self.edit_source()?;
        let row = source.result.rows.get(row_index)?;
        Some(
            source
                .result
                .columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let cell = row.get(index);
                    let original = cell
                        .map(|cell| cell.render().to_string())
                        .unwrap_or_default();
                    let is_null = cell.is_some_and(duck_db::Value::is_null);
                    (column.name.clone(), original, is_null)
                })
                .collect(),
        )
    }

    /// 新增行的编辑器数据源：所有列，值全空（不填的列提交时落到数据库 `DEFAULT`）。
    fn new_row_specs(&self) -> Option<Vec<(String, String, bool)>> {
        let _source = self.edit_source()?;
        let columns = self
            .current_result()?
            .columns
            .iter()
            .map(|column| (column.name.clone(), String::new(), false))
            .collect();
        Some(columns)
    }

    /// 当前行（指定下标）的主键定位值；无主键或列对不上返回 `None`。
    fn pk_values_for_row(&self, row_index: usize) -> Option<PkValues> {
        let source = self.edit_source()?;
        if source.pk.is_empty() {
            return None;
        }
        let row = source.result.rows.get(row_index)?;
        let mut pk_values = Vec::with_capacity(source.pk.len());
        for name in source.pk {
            let index = source
                .result
                .columns
                .iter()
                .position(|column| &column.name == name)?;
            pk_values.push((name.clone(), row.get(index)?.render().to_string()));
        }
        Some(pk_values)
    }

    /// 把行编辑器的改动**暂存**起来（不落库）。
    ///
    /// 这是 `pending_edits` 模型：编辑只进暂存区，点「提交」时成一个事务写下去。
    /// 每列一次判定：`null` 开关打开 → 写 `NULL`；否则只在文本**真的变了**时暂存。
    fn stage_row_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.row_editor.as_ref() else {
            return;
        };
        // 兜一道「结果集可编辑」：无表 / 无主键 / 主键列不在结果里时，暂存毫无意义
        // （点了提交也定位不到行）。正常路径上提交按钮已置灰，这里是防漏。
        if self.edit_source().is_none() {
            return;
        }
        // 新增行：把填过的列攒成一条 INSERT（空且未勾 NULL 的列不提交，让 DEFAULT 生效）。
        if editor.row_index.is_none() {
            let mut columns = Vec::new();
            let mut values = Vec::new();
            for cell in &editor.cells {
                let is_null = cell.null.get();
                let text = cell.input.read(cx).value().to_string();
                if is_null {
                    columns.push(cell.column.clone());
                    values.push(None);
                } else if !text.is_empty() {
                    columns.push(cell.column.clone());
                    values.push(Some(text));
                }
            }
            if columns.is_empty() {
                notify::info(
                    tr(cx, "新增行至少要填一列（空白列会落到数据库默认值）。"),
                    window,
                    cx,
                );
                return;
            }
            self.pending_inserts.push(InsertRow { columns, values });
            self.status = trf(
                cx,
                "已暂存 1 条新增行（共 {} 条待提交）",
                &[&self.pending_total().to_string()],
            )
            .into();
            self.row_editor = None;
            cx.notify();
            return;
        }
        let row_index = editor.row_index.expect("new-row branch returns above");
        let Some(pk_values) = self.pk_values_for_row(row_index) else {
            notify::error(tr(cx, "该表没有主键，无法定位行。"), window, cx);
            return;
        };
        let mut staged = Vec::new();
        for cell in &editor.cells {
            let is_null = cell.null.get();
            let text = cell.input.read(cx).value().to_string();
            let value = if is_null { None } else { Some(text) };
            // 「原值」的等价形式：`NULL` 就是「没有值」，不能拿展示文本 `"NULL"` 去比
            // （否则「NULL → NULL」会被判成改动，而「把 NULL 改成空串」又判不出来）。
            let original: Option<&str> = (!cell.was_null).then_some(cell.original.as_str());
            let unchanged = match (value.as_deref(), original) {
                // 两边都没值。
                (None, None) => true,
                // 都有值：文本一致才算没改。
                (Some(text), Some(original)) => text == original,
                // 原为 NULL、输入框仍是空的 = 没碰过（NULL 格进编辑器时输入框本就是空的）。
                (Some(text), None) => text.is_empty(),
                // 有值 → NULL：是改动。
                (None, Some(_)) => false,
            };
            if unchanged {
                continue;
            }
            staged.push(CellEdit {
                pk: pk_values.clone(),
                column: cell.column.clone(),
                value,
            });
        }
        if staged.is_empty() {
            notify::info(tr(cx, "没有改动。"), window, cx);
            return;
        }
        let count = staged.len();
        // 同一格改两次：后一次覆盖前一次（旧的那条按 (pk, 列) 摘掉）。
        for edit in staged {
            self.pending_edits
                .retain(|old| !(old.pk == edit.pk && old.column == edit.column));
            self.pending_edits.push(edit);
        }
        self.status = trf(
            cx,
            "已暂存 {} 处改动（共 {} 条待提交）",
            &[&count.to_string(), &self.pending_total().to_string()],
        )
        .into();
        self.row_editor = None;
        cx.notify();
    }

    /// 暂存区里一共有多少条待提交（改动 + 新增行）。
    fn pending_total(&self) -> usize {
        self.pending_edits.len() + self.pending_inserts.len()
    }

    /// 丢掉全部暂存改动。
    fn discard_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_edits.clear();
        self.pending_inserts.clear();
        self.status = tr(cx, "已丢弃暂存改动").into();
        notify::info(tr(cx, "暂存改动已丢弃。"), window, cx);
        cx.notify();
    }

    /// **提交**暂存区：改格 + 新增行在一个事务里写下去。
    fn commit_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_total() == 0 {
            notify::info(tr(cx, "没有待提交的改动。"), window, cx);
            return;
        }
        let Some(source) = self.edit_source() else {
            notify::error(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        let table = source.table.clone();
        let edits = self.pending_edits.clone();
        let inserts = self.pending_inserts.clone();
        let planned_updates = u64::try_from(edits.len()).unwrap_or(u64::MAX);
        let planned_inserts = u64::try_from(inserts.len()).unwrap_or(u64::MAX);
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.status = trf(
            cx,
            "正在提交 {} 条改动…",
            &[&self.pending_total().to_string()],
        )
        .into();
        cx.notify();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.apply_batch(&table, &edits, &inserts)
            },
            move |this, result, window, cx| match result {
                Ok(outcome) => {
                    this.pending_edits.clear();
                    this.pending_inserts.clear();
                    let BatchOutcome { updated, inserted } = outcome;
                    this.status = trf(
                        cx,
                        "已提交：改 {} 行 · 新增 {} 行",
                        &[&updated.to_string(), &inserted.to_string()],
                    )
                    .into();
                    notify::success(
                        trf(
                            cx,
                            "已提交：改 {} 行 · 新增 {} 行。",
                            &[&updated.to_string(), &inserted.to_string()],
                        ),
                        window,
                        cx,
                    );
                    if updated < planned_updates || inserted < planned_inserts {
                        // 少命中：多半是这些行已被他人改动（主键展示文本对不上）。
                        notify::warning(
                            tr(
                                cx,
                                "有改动未命中目标行（该行可能已被他人修改），请刷新后重试。",
                            ),
                            window,
                            cx,
                        );
                    }
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "提交失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "提交失败（已整体回滚）：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // 多选与批量操作（结果网格的行首复选框）。
    // ------------------------------------------------------------------

    /// 勾选 / 取消勾选一行。
    fn toggle_row_checked(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.checked_rows.remove(&index) {
            self.checked_rows.insert(index);
        }
        cx.notify();
    }

    /// 表头全选 / 全不选（范围 = 当前页；勾满时点一下就是清空）。
    fn toggle_all_checked(&mut self, cx: &mut Context<Self>) {
        let total = self.current_result().map_or(0, ResultSet::row_count);
        if total > 0 && self.checked_rows.len() == total {
            self.checked_rows.clear();
        } else {
            self.checked_rows = (0..total).collect();
        }
        cx.notify();
    }

    /// 清空多选。**翻页 / 换表 / 重新执行都必须调**：行号是「当前结果集里的下标」，
    /// 结果集一换，旧的勾选就指向别的行了。
    fn clear_checked_rows(&mut self) {
        self.checked_rows.clear();
    }

    /// 勾选行的主键定位值；任一行定位不了（无主键 / 下标越界）返回 `None`。
    ///
    /// 整批要么都能定位、要么一个都不动 —— 「删了一半」这种中间态最难收拾。
    fn checked_pk_values(&self) -> Option<Vec<PkValues>> {
        if self.checked_rows.is_empty() {
            return None;
        }
        let mut keys = Vec::with_capacity(self.checked_rows.len());
        for index in &self.checked_rows {
            keys.push(self.pk_values_for_row(*index)?);
        }
        Some(keys)
    }

    /// 批量编辑：把「选中列 + 新值」应用到所有勾选行（**单事务**）。
    ///
    /// 表浏览与可编辑 SELECT 结果共用同一条路（[`Self::edit_source`] 决定改哪张表）。
    fn apply_batch_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(column) = self.batch_column.clone() else {
            notify::info(tr(cx, "请先选择要修改的列。"), window, cx);
            return;
        };
        if self.batch_null {
            // 勾了「写 NULL」就以它为准，输入框内容忽略。
        }
        let value = if self.batch_null {
            None
        } else {
            Some(self.batch_value.read(cx).value().to_string())
        };
        let Some(keys) = self.checked_pk_values() else {
            notify::info(
                tr(
                    cx,
                    "请先勾选要处理的行；当前结果集不可编辑（需要单表 + 主键）。",
                ),
                window,
                cx,
            );
            return;
        };
        let Some(table) = self.edit_source().map(|source| source.table.clone()) else {
            notify::info(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        let edits: Vec<CellEdit> = keys
            .into_iter()
            .map(|pk| CellEdit {
                pk,
                column: column.clone(),
                value: value.clone(),
            })
            .collect();
        let planned = u64::try_from(edits.len()).unwrap_or(u64::MAX);
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.status = trf(cx, "正在批量修改 {} 行…", &[&planned.to_string()]).into();
        cx.notify();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.update_cells(&table, &edits)
            },
            move |this, result, window, cx| match result {
                Ok(affected) => {
                    this.status = trf(cx, "已批量修改 {} 行", &[&affected.to_string()]).into();
                    notify::success(
                        trf(cx, "已修改 {} 处。", &[&affected.to_string()]),
                        window,
                        cx,
                    );
                    if affected < planned {
                        notify::warning(
                            tr(
                                cx,
                                "有改动未命中目标行（行可能已被他人修改），请刷新后重试。",
                            ),
                            window,
                            cx,
                        );
                    }
                    this.clear_checked_rows();
                    // 行数没变，但值变了：表浏览的元信息（索引 / 行数）可以复用。
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "批量修改失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "批量修改失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 批量删除勾选的行（**单事务**）。
    fn apply_batch_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(keys) = self.checked_pk_values() else {
            notify::info(tr(cx, "请先勾选要处理的行，或该表没有主键。"), window, cx);
            return;
        };
        let Some(table) = self.edit_source().map(|source| source.table.clone()) else {
            notify::info(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        let question = trf(
            cx,
            "将从数据库中删除 {} 行，确定继续吗？",
            &[&keys.len().to_string()],
        );
        if !duck_platform::confirm(tr(cx, "批量删除行"), &question) {
            return;
        }
        let planned = u64::try_from(keys.len()).unwrap_or(u64::MAX);
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.status = trf(cx, "正在删除 {} 行…", &[&planned.to_string()]).into();
        cx.notify();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.delete_row_set(&table, &keys)
            },
            move |this, result, window, cx| match result {
                Ok(affected) => {
                    this.status = trf(cx, "已删除 {} 行", &[&affected.to_string()]).into();
                    notify::success(
                        trf(cx, "已删除 {} 行。", &[&affected.to_string()]),
                        window,
                        cx,
                    );
                    if affected < planned {
                        notify::warning(
                            tr(cx, "有行未命中（可能已被他人删除），请刷新后重试。"),
                            window,
                            cx,
                        );
                    }
                    this.clear_checked_rows();
                    // 行数变了：让表级元信息跟着重取。
                    this.table_meta_for = None;
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "批量删除失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "批量删除失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 删除编辑中的行（确认后按主键删）。
    fn delete_current_row(
        &mut self,
        row_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pk_values) = self.pk_values_for_row(row_index) else {
            notify::error(tr(cx, "该表没有主键，无法定位行。"), window, cx);
            return;
        };
        let Some(table) = self.edit_source().map(|source| source.table.clone()) else {
            notify::error(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        if !duck_platform::confirm(
            tr(cx, "删除行"),
            tr(cx, "这一行将从数据库中删除，确定继续吗？"),
        ) {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.status = tr(cx, "正在删除行…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.delete_rows(&table, &pk_values)
            },
            |this, result, window, cx| match result {
                Ok(_) => {
                    this.status = tr(cx, "行已删除").into();
                    notify::success(tr(cx, "行已删除。"), window, cx);
                    // 行数一定变了：让表级元信息跟着重取。
                    this.table_meta_for = None;
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "删除失败：{}", &[&error.to_string()]).into();
                    notify::error(trf(cx, "删除失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // AI 助手（text-to-SQL；写语句一律退回编辑器人工审批）。
    // ------------------------------------------------------------------

    /// 保存 AI 设置：base_url / model 写 kv，api_key 非空才写凭据文件。
    fn save_ai_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let base_url = self.ai_base_url.read(cx).value().trim().to_owned();
        let model = self.ai_model.read(cx).value().trim().to_owned();
        self.ai_settings = AiSettings {
            base_url: if base_url.is_empty() {
                default_ai_base_url()
            } else {
                base_url
            },
            model: if model.is_empty() {
                default_ai_model()
            } else {
                model
            },
        };
        match serde_json::to_string(&self.ai_settings) {
            Ok(json) => duck_store::kv_put(cx, KEY_AI_SETTINGS, json),
            Err(error) => tracing::warn!(%error, "serialize ai settings failed"),
        }
        let api_key = self.ai_api_key.read(cx).value().trim().to_owned();
        if !api_key.is_empty()
            && let Err(error) = duck_platform::secret::set(AI_SECRET_KEY, &api_key)
        {
            notify::error(
                trf(cx, "AI key 保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return;
        }
        notify::success(tr(cx, "AI 设置已保存。"), window, cx);
    }

    /// AI 问答：schema 摘要 → 模型产 SQL → 风险分级 → 只读自动执行 / 其余退回编辑器。
    fn run_ai_question(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let question = self.ai_question.read(cx).value().trim().to_owned();
        if question.is_empty() {
            notify::info(tr(cx, "请先输入问题。"), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            notify::info(tr(cx, "请先连接一个数据库。"), window, cx);
            return;
        };
        let settings = self.ai_settings.clone();
        let selected_table = self.selected_table.clone();
        let conn = session.conn.clone();
        // schema 摘要的静态部分（表名清单）在主线程备好；列级信息在工作闭包里取。
        let mut schema = String::new();
        for table in session.catalog.iter().take(30) {
            schema.push_str(&table.name);
            schema.push('\n');
        }
        // 选中表必须在目录里，列级信息才值得进 prompt。
        let selected_table = selected_table.filter(|table| {
            session
                .catalog
                .iter()
                .any(|entry| entry.table_ref() == *table)
        });
        self.status = tr(cx, "正在请求 AI…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<AiOutcome, DbError> {
                // 1. 列级 schema（持锁快查，最多 30 表）。
                let mut detail = schema;
                {
                    let mut guard = conn.lock().expect("连接锁不应中毒");
                    if let Some(table) = selected_table.as_ref() {
                        let columns = guard.table_columns(table).unwrap_or_default();
                        let columns_text = columns
                            .iter()
                            .map(|column| format!("{} {}", column.name, column.type_name))
                            .collect::<Vec<_>>()
                            .join(", ");
                        detail.push_str(&format!("{}({columns_text})\n", table.display()));
                    }
                }
                // 2. AI 调用（不持锁）。
                let api_key = duck_platform::secret::get(AI_SECRET_KEY)
                    .unwrap_or(None)
                    .unwrap_or_default();
                let provider = OpenAiCompatible {
                    base_url: settings.base_url.clone(),
                    api_key,
                    model: settings.model.clone(),
                    timeout_secs: 60,
                };
                let completion = provider.complete(&build_prompt(&detail, &question))?;
                // 3. 风险分级：**单条只读**交给调用方自动执行。
                //    执行不走这里（这里握着的是「模型调用」，不是查询），
                //    而是回到主线程再走一次可取消的执行路径 —— 见 `execute_ai_sql`。
                let statements = split_statements(&completion.sql);
                let tokens = completion.total_tokens;
                if statements.len() == 1 && risk_of_statements(&statements) == SqlRisk::ReadOnly {
                    Ok(AiOutcome::ReadOnly {
                        sql: completion.sql,
                        tokens,
                    })
                } else {
                    Ok(AiOutcome::NeedsApproval {
                        sql: completion.sql,
                        tokens,
                    })
                }
            },
            |this, result, window, cx| match result {
                Ok(AiOutcome::ReadOnly { sql, tokens }) => {
                    // 模型给的是单条只读 SQL：立刻执行（可取消 + 查询超时），
                    // 用户看到的是「AI 直接给了结果」，不是「还得自己点一次执行」。
                    this.execute_ai_sql(sql, tokens, window, cx);
                }
                Ok(AiOutcome::NeedsApproval { sql, tokens }) => {
                    this.sql.update(cx, |state, cx| {
                        state.set_value(sql, window, cx);
                    });
                    this.status = trf(
                        cx,
                        "AI 产出的写 / 多条语句已填入编辑器（{} tokens），请审阅后执行",
                        &[&tokens.to_string()],
                    )
                    .into();
                    notify::info(
                        tr(cx, "AI 产出的语句需要人工审阅，已填入 SQL 编辑器。"),
                        window,
                        cx,
                    );
                }
                Err(error) => {
                    this.status = trf(cx, "AI 失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "AI 请求失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 执行 AI 产出的**单条只读** SQL —— 与手敲执行共用「可取消 + 查询超时」路径。
    ///
    /// 与 [`Self::execute_sql`] 的差别只有两点：状态文案带 tokens（来源是 AI），
    /// 以及语句已由 `risk_of_statements` 证明是单条只读（不必再走危险确认）。
    /// 模型调用本身无法带外取消（只有它自带的 HTTP 超时），所以「可停」只覆盖
    /// 真正在库上跑的那一段。
    fn execute_ai_sql(
        &mut self,
        sql: String,
        tokens: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.active_session() else {
            return;
        };
        if self.running.is_some() {
            notify::info(
                tr(cx, "上一条语句还在跑，先等它结束或点「停止」。"),
                window,
                cx,
            );
            return;
        }
        let session_id = self.active.clone().unwrap_or_default();
        let conn = session.conn.clone();
        // 与 `execute_sql` 同理：取消句柄必须在开工前抓（跑起来锁在后台线程手里）。
        let cancel = conn.lock().expect("连接锁不应中毒").cancel_handle();
        let timeout = self.effective_timeout();
        let sql_for_log = sql.clone();
        // AI 只可能是单条只读 SELECT，同样按可编辑判据过一遍（`edit_target` 是纯函数，
        // 在主线程算完带进去；结果回来后再核主键覆盖）。
        let target = edit_target(&sql).map(|EditTarget { table, schema }| TableRef {
            schema,
            name: table,
        });
        self.status = tr(cx, "正在执行 AI 生成的 SQL…").into();
        cx.notify();
        self.spawn_db_guarded(
            cx,
            session_id,
            cancel,
            timeout,
            move || {
                let started = Instant::now();
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.execute(&sql, QUERY_ROW_CAP).map(|outcome| {
                    let edit_meta = match (&target, outcome.result.as_ref()) {
                        (Some(table), Some(result)) => {
                            let pk = guard.table_primary_key(table).unwrap_or_default();
                            let projected: Vec<String> = result
                                .columns
                                .iter()
                                .map(|column| column.name.clone())
                                .collect();
                            (!pk.is_empty() && covers_columns(&projected, &pk))
                                .then(|| (table.clone(), pk))
                        }
                        _ => None,
                    };
                    (outcome, started.elapsed().as_millis(), edit_meta)
                })
            },
            move |this, result, window, cx| match result {
                Ok((outcome, elapsed_ms, edit_meta)) => {
                    this.install_query_result(outcome, edit_meta, sql_for_log.clone());
                    this.status =
                        trf(cx, "AI 已执行只读 SQL · {} tokens", &[&tokens.to_string()]).into();
                    notify::success(
                        trf(
                            cx,
                            "AI 生成的 SQL 已执行（{} ms）。",
                            &[&elapsed_ms.to_string()],
                        ),
                        window,
                        cx,
                    );
                    tracing::info!(sql = %sql_for_log, tokens, "ai executed readonly sql");
                }
                Err(error) => {
                    if matches!(error, DbError::Cancelled(_)) {
                        let text = this.cancelled_status(cx);
                        this.status = text.clone();
                        notify::info(text, window, cx);
                        return;
                    }
                    this.status = trf(cx, "AI 执行失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "AI 生成的 SQL 执行失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    fn active_session(&self) -> Option<&Session> {
        self.sessions.get(self.active.as_deref()?)
    }

    fn active_session_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.active.as_deref()?)
    }

    /// 活跃会话的连接句柄（SQL 补全的后台取列用）。
    fn active_conn(&self) -> Option<Arc<Mutex<Connection>>> {
        self.active_session().map(|session| session.conn.clone())
    }

    /// 当前展示的结果集（SQL 结果或表页）。
    fn current_result(&self) -> Option<&ResultSet> {
        match self.mode {
            Mode::Query => self
                .query_result
                .as_ref()
                .and_then(|view| view.result.as_ref()),
            Mode::Table => self.table_rows.as_ref(),
        }
    }

    /// 导出当前结果集为 CSV。
    fn export_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self.current_result() else {
            notify::info(tr(cx, "当前没有可导出的结果。"), window, cx);
            return;
        };
        let csv = to_csv(result);
        let Some(path) = duck_platform::save_file("result.csv") else {
            return;
        };
        self.spawn_db(
            cx,
            move || std::fs::write(&path, csv).map_err(|err| err.to_string()),
            |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(tr(cx, "已导出 CSV。"), window, cx);
                    this.status = tr(cx, "导出完成").into();
                }
                Err(error) => {
                    notify::error(trf(cx, "导出失败：{}", &[&error]), window, cx);
                }
            },
        );
    }

    /// 从 CSV 文件导入数据到当前浏览的表（首行为列名，按列名对位）。
    fn import_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            notify::info(tr(cx, "有数据库操作正在进行，请稍候。"), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        // 导入目标与改格 / 删行同源：表浏览的当前表，或可编辑 SELECT 背后的表。
        let Some(table) = self.edit_source().map(|source| source.table.clone()) else {
            notify::info(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        let Some(path) = duck_platform::open_file() else {
            return;
        };
        let conn = session.conn.clone();
        self.status = tr(cx, "正在导入 CSV…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<u64, String> {
                let text = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
                let mut rows = parse_csv(&text);
                if rows.is_empty() {
                    return Err("file is empty".into());
                }
                let header = rows.remove(0);
                let columns: Vec<String> = header
                    .into_iter()
                    .map(|cell| cell.unwrap_or_default())
                    .collect();
                if columns.iter().any(String::is_empty) {
                    return Err("header contains an empty column name".into());
                }
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard
                    .insert_rows(&table, &columns, &rows)
                    .map_err(|error| error.to_string())
            },
            |this, inserted, window, cx| match inserted {
                Ok(count) => {
                    notify::success(trf(cx, "已导入 {} 行。", &[&count.to_string()]), window, cx);
                    // 行数变了：表级元信息（行数）也跟着重取。
                    this.table_meta_for = None;
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "导入失败：{}", &[&error]).into();
                    notify::error(trf(cx, "导入失败：{}", &[&error]), window, cx);
                }
            },
        );
    }

    /// 导出连接档案为 JSON 文件（口令不导出——它们只存凭据文件）。
    fn export_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connections.is_empty() {
            notify::info(tr(cx, "当前没有可导出的连接。"), window, cx);
            return;
        }
        let Some(path) = duck_platform::save_file("duck-connections.json") else {
            return;
        };
        let json = match serde_json::to_vec_pretty(&self.connections) {
            Ok(json) => json,
            Err(error) => {
                notify::error(trf(cx, "导出失败：{}", &[&error.to_string()]), window, cx);
                return;
            }
        };
        self.spawn_db(
            cx,
            move || std::fs::write(&path, json).map_err(|err| err.to_string()),
            |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(tr(cx, "已导出连接档案。"), window, cx);
                    this.status = tr(cx, "导出完成").into();
                }
                Err(error) => {
                    notify::error(trf(cx, "导出失败：{}", &[&error]), window, cx);
                }
            },
        );
    }

    /// 从 JSON 文件导入连接档案。
    ///
    /// 两种来源都收：本应用自己导出的（`Vec<ConnectionConfig>`）与第三方客户端的档案
    /// （DBeaver 的 `data-sources.json`、TablePlus 的 `.tableplusconnection`，见
    /// [`duck_db::import`]）。按 id 去重、已存在的跳过；口令一律要重新录入。
    fn import_connections(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = duck_platform::open_file() else {
            return;
        };
        self.spawn_db(
            cx,
            move || -> Result<Vec<ConnectionConfig>, ProfileImportError> {
                let text = std::fs::read_to_string(&path)
                    .map_err(|err| ProfileImportError::Read(err.to_string()))?;
                // 先按自家档案读；读不出来再试第三方格式。
                if let Ok(profiles) = serde_json::from_str::<Vec<ConnectionConfig>>(&text) {
                    return Ok(profiles);
                }
                import_third_party(&text)
                    .map(|(_source, connections)| connections)
                    .map_err(ProfileImportError::ThirdParty)
            },
            |this, result, window, cx| match result {
                Ok(incoming) => {
                    let existing: std::collections::HashSet<String> =
                        this.connections.iter().map(|cfg| cfg.id.clone()).collect();
                    let mut added = 0usize;
                    let mut skipped = 0usize;
                    for (index, mut cfg) in incoming.into_iter().enumerate() {
                        // 第三方档案不带我们的 id（`import` 层刻意留空），在这里统一补。
                        if cfg.id.trim().is_empty() {
                            cfg.id =
                                format!("conn-{}", id_suffix(&format!("{}-{index}", cfg.name)));
                        }
                        if existing.contains(&cfg.id) || cfg.validate().is_err() {
                            skipped += 1;
                        } else {
                            this.connections.push(cfg);
                            added += 1;
                        }
                    }
                    this.persist_connections(cx);
                    cx.notify();
                    if added > 0 {
                        notify::success(
                            trf(
                                cx,
                                "已导入 {} 条连接，跳过 {} 条。",
                                &[&added.to_string(), &skipped.to_string()],
                            ),
                            window,
                            cx,
                        );
                    } else {
                        notify::info(tr(cx, "没有新增连接（id 全部重复或无效）。"), window, cx);
                    }
                }
                Err(error) => {
                    let message = match error {
                        ProfileImportError::Read(error) => trf(cx, "读取文件失败：{}", &[&error]),
                        ProfileImportError::ThirdParty(error) => {
                            import_error_message(error, cx).to_owned()
                        }
                    };
                    notify::error(trf(cx, "导入失败：{}", &[&message]), window, cx);
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // 渲染。
    // ------------------------------------------------------------------

    /// 左栏：搜索 + 分组连接列表 + 新建入口。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
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
                !searching
                    || cfg.display_name().to_lowercase().contains(&query)
                    || format!("{} {}", cfg.host, cfg.database)
                        .to_lowercase()
                        .contains(&query)
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
                            }
                        }),
                    );
                nav = nav.child(
                    sidebar_navigation_button(
                        SharedString::from(format!("db-conn-{id}")),
                        SharedString::from(cfg.display_name()),
                        Icon::new(DuckIcon::Database)
                            .size(extended.icon.md)
                            .text_color(if connected {
                                extended.colors.status_completed
                            } else {
                                extended.colors.status_paused
                            }),
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
                                    this.activate(&id, cx);
                                } else {
                                    this.connect_entry(&id, window, cx);
                                }
                            });
                        }
                    }),
                );
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

    /// 进入编辑表单（弹窗子页）。
    fn edit_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cfg) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        let form = EditFormView::from_config(self.view.clone(), &cfg, window, cx);
        let form = cx.new(|_| form);
        self.open_edit_dialog(form, window, cx);
    }

    /// 主区顶部的连接标签条。
    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let mut row = h_flex()
            .min_w_0()
            .flex_1()
            .gap(tokens.spacing.xxs)
            .overflow_x_hidden();
        for id in &self.tabs {
            let active = self.active.as_deref() == Some(id.as_str());
            let Some(cfg) = self.connections.iter().find(|cfg| &cfg.id == id) else {
                continue;
            };
            let (foreground, weight) = if active {
                (tokens.colors.foreground, FontWeight::MEDIUM)
            } else {
                (tokens.colors.muted_foreground, FontWeight::NORMAL)
            };
            let close = toolbar_action_button(
                SharedString::from(format!("db-tab-close-{id}")),
                tr(cx, "断开连接"),
                // 标签页内嵌的关闭 ✕ 有意比 `icon.md` 小一档（三个页面的标签条都取 sm）；
                // 代价是按钮 26×28 不方 —— 与终端标签条同款，已知并接受。
                Icon::new(DuckIcon::X).size(extended.icon.sm),
                false,
                false,
                cx,
            )
            .on_click({
                let view = view.clone();
                let id = id.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.close_tab(&id, cx));
                }
            });
            row = row.child(
                h_flex()
                    .id(SharedString::from(format!("db-tab-{id}")))
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
                        let id = id.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.activate(&id, cx));
                        }
                    })
                    .child(div().min_w_0().truncate().child(cfg.display_name()))
                    .child(close),
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
            .child(row)
            .child(
                toolbar_action_button(
                    "db-export-csv",
                    tr(cx, "导出 CSV"),
                    Icon::new(DuckIcon::Download).size(extended.icon.md),
                    false,
                    self.current_result().is_none(),
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.export_csv(window, cx));
                    }
                }),
            )
            .child(
                toolbar_action_button(
                    "db-import-csv",
                    tr(cx, "导入 CSV"),
                    Icon::new(DuckIcon::Upload).size(extended.icon.md),
                    false,
                    self.edit_source().is_none() || self.busy,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.import_csv(window, cx));
                    }
                }),
            )
            .into_any_element()
    }

    /// 查询历史面板：最近执行过的 SQL（最新的在前，点一条放回编辑器）。
    ///
    /// 只放**执行过**的语句：编辑器里正在敲的半截内容不算历史。同一句重复执行只留
    /// 一条并置顶（见 [`push_history`]），否则历史会被同一句刷满。
    fn render_history_panel(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let hover = extended.colors.hover_tint;

        let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
        for (index, sql) in self.history.iter().enumerate() {
            let statement = sql.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("db-history-{index}")))
                    .w_full()
                    .min_w_0()
                    .px(tokens.spacing.xs)
                    .py(tokens.spacing.xxs)
                    .rounded(tokens.radius.sm)
                    .cursor_pointer()
                    .role(Role::Button)
                    .aria_label(SharedString::from(trf(
                        cx,
                        "把第 {} 条历史放回编辑器",
                        &[&(index + 1).to_string()],
                    )))
                    .text_size(tokens.typography.mono_md.size)
                    .text_color(tokens.colors.muted_foreground)
                    .hover(move |style| style.bg(hover))
                    .on_click({
                        let view = view.clone();
                        // 闭包拿走自己那份，`statement` 本体留给下面那行展示文本。
                        let statement = statement.clone();
                        move |_, window, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            let sql = statement.clone();
                            entity.update(cx, |this, cx| {
                                this.sql.update(cx, |state, cx| {
                                    state.set_value(sql.clone(), window, cx);
                                });
                                // 放回即收起：面板挡着编辑器，留着反而是干扰。
                                this.history_open = false;
                                this.status = tr(cx, "已从历史放回编辑器。").into();
                                cx.notify();
                            });
                        }
                    })
                    // 多行 SQL 折成一行展示：列表是「认句子」用的，不是阅读器。
                    .child(div().truncate().child(SharedString::from(
                        statement.split_whitespace().collect::<Vec<_>>().join(" "),
                    ))),
            );
        }

        v_flex()
            .w_full()
            .flex_none()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .bg(tokens.colors.surface)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(caption_heading(tr(cx, "查询历史"), cx))
                    .child(
                        button("db-history-clear", tr(cx, "清空"), ButtonVariant::Ghost, cx)
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.history.clear();
                                        this.history_open = false;
                                        this.persist_history(cx);
                                        this.status = tr(cx, "查询历史已清空。").into();
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            )
            // 面板最高只占这些高度，超了自己滚——历史是辅助信息，不能把结果区挤没。
            .child(
                div()
                    .id("db-history-list")
                    .w_full()
                    .max_h(px(HISTORY_PANEL_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    /// 编辑器工具条：执行按钮 + 状态 + 模式标签。
    fn render_editor_toolbar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();
        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    // 跑着就把「执行」换成「停止」：主按钮永远只有一个动作，
                    // 用户不会去点一个点了没用的按钮。
                    .child(if self.running.is_some() {
                        button(
                            "db-stop-query",
                            tr(cx, "停止"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| this.stop_running(window, cx));
                            }
                        })
                        .into_any_element()
                    } else {
                        button("db-run-query", tr(cx, "执行"), ButtonVariant::Primary, cx)
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
                        div()
                            .min_w_0()
                            .truncate()
                            .child(meta_text(self.status.clone(), cx)),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    // 「执行计划」放最前：它是执行旁边最常按的第二个动作。
                    .child(
                        button(
                            "db-explain-query",
                            tr(cx, "执行计划"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(self.busy)
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ =
                                    view.update(cx, |this, cx| this.explain_editor_sql(window, cx));
                            }
                        }),
                    )
                    .child(
                        button(
                            "db-format-sql",
                            tr(cx, "格式化"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ =
                                    view.update(cx, |this, cx| this.format_editor_sql(window, cx));
                            }
                        }),
                    )
                    .child(
                        button(
                            "db-ai-toggle",
                            tr(cx, "AI 助手"),
                            if self.ai_open {
                                ButtonVariant::Primary
                            } else {
                                ButtonVariant::Secondary
                            },
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.ai_open = !this.ai_open;
                                    if !this.ai_open {
                                        this.ai_settings_open = false;
                                    }
                                    cx.notify();
                                });
                            }
                        }),
                    )
                    .child(
                        button(
                            "db-query-options",
                            tr(cx, "查询选项"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ =
                                    view.update(cx, |this, cx| this.open_query_options(window, cx));
                            }
                        }),
                    )
                    .child(
                        button(
                            "db-history-toggle",
                            tr(cx, "历史"),
                            if self.history_open {
                                ButtonVariant::Primary
                            } else {
                                ButtonVariant::Secondary
                            },
                            cx,
                        )
                        .disabled(self.history.is_empty())
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.history_open = !this.history_open;
                                    cx.notify();
                                });
                            }
                        }),
                    )
                    // 单次覆盖生效时挂个标签：跑慢了才知道「不是连接默认在管」。
                    .when_some(self.query_timeout_override, |row, secs| {
                        row.child(neutral_tag(
                            SharedString::from(trf(cx, "超时 {} 秒", &[&secs.to_string()])),
                            cx,
                        ))
                    })
                    .child(match self.mode {
                        Mode::Query => neutral_tag(tr(cx, "SQL 结果"), cx).into_any_element(),
                        Mode::Table => neutral_tag(tr(cx, "表浏览"), cx).into_any_element(),
                    }),
            )
            .into_any_element()
    }

    /// AI 助手条：提问输入 + 生成按钮；（展开时）设置行。
    fn render_ai_bar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();
        let mut bar = v_flex().w_full().gap(tokens.spacing.xs);
        if self.ai_settings_open {
            bar = bar
                .child(field_row(cx, tr(cx, "接口地址"), &self.ai_base_url))
                .child(field_row(cx, tr(cx, "模型"), &self.ai_model))
                .child(field_row(cx, tr(cx, "API Key"), &self.ai_api_key))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.xs)
                        .child(
                            button(
                                "db-ai-save-settings",
                                tr(cx, "保存 AI 设置"),
                                ButtonVariant::Primary,
                                cx,
                            )
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.save_ai_settings(window, cx);
                                    });
                                }
                            }),
                        )
                        .child(
                            button(
                                "db-ai-hide-settings",
                                tr(cx, "收起设置"),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.ai_settings_open = false;
                                        cx.notify();
                                    });
                                }
                            }),
                        ),
                );
        }
        bar = bar.child(
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&self.ai_question)
                            .h(theme.density().control)
                            .text_size(tokens.typography.sm.size)
                            .w_full(),
                    ),
                )
                .child(
                    button("db-ai-run", tr(cx, "生成"), ButtonVariant::Primary, cx)
                        .disabled(self.busy)
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.run_ai_question(window, cx);
                                });
                            }
                        }),
                )
                .child(
                    button(
                        "db-ai-settings",
                        tr(cx, "设置"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.ai_settings_open = !this.ai_settings_open;
                                cx.notify();
                            });
                        }
                    }),
                ),
        );
        card(cx)
            .id("db-ai-bar")
            .w_full()
            .flex_none()
            .p(tokens.spacing.lg)
            .child(bar)
            .into_any_element()
    }

    /// 主区：编辑器 + 结果网格（或空态）。
    ///
    /// 编辑器与结果区之间是一条**可拖拽的下边框**。两个面板都**不传初始尺寸**：
    /// `ResizablePanel` 在 `initial_size` 为 None 时走 `flex_grow_1 + flex_shrink_1`，
    /// 两块**默认各占一半**；首帧 prepaint 把实测尺寸记进 `ResizableState`，此后拖拽
    /// 与容器伸缩都由组件按比例自管（`adjust_to_container_size`）。把手本体
    /// （hairline + hover 胶囊 + 光标）由 gpui-component 画，本页只给编辑器面板
    /// `SQL_EDITOR_HEIGHT_RANGE` 的拖拽档位。
    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let height_range = px(*SQL_EDITOR_HEIGHT_RANGE.start())..px(*SQL_EDITOR_HEIGHT_RANGE.end());

        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .bg(tokens.colors.background)
            .child(self.render_editor_toolbar(cx))
            .when(self.history_open, |body| {
                body.child(self.render_history_panel(cx))
            })
            .when(self.ai_open, |body| body.child(self.render_ai_bar(cx)))
            .child(
                div().flex_1().min_h_0().min_w_0().child(
                    v_resizable("db-sql-split")
                        .with_state(&self.sql_split)
                        .child(
                            resizable_panel().size_range(height_range).child(
                                div()
                                    .size_full()
                                    .border_b(extended.stroke.thin)
                                    .border_color(extended.colors.hairline)
                                    .child(
                                        Editor::new(&self.sql)
                                            .bordered(false)
                                            .p_0()
                                            .h_full()
                                            .w_full()
                                            .text_size(tokens.typography.mono_md.size),
                                    ),
                            ),
                        )
                        .child(resizable_panel().child(self.render_result(cx))),
                ),
            )
            .into_any_element()
    }

    /// 结果网格（含表浏览分页条与截断提示）。
    fn render_result(&self, cx: &Context<Self>) -> AnyElement {
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
            return empty_state(
                DuckIcon::Database,
                tr(cx, "执行 SQL 后在这里查看结果"),
                Some(tr(cx, "支持多条语句；读语句结果最多展示 1000 行。").into()),
                None,
                cx,
            )
            .flex_1()
            .into_any_element();
        };

        let columns = result.columns.clone();
        let rows = result.rows.clone();
        let widths = if self.column_widths.len() == columns.len() {
            self.column_widths.clone()
        } else {
            compute_column_widths(result)
        };
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

        let header = h_flex()
            .flex_none()
            .h(px(GRID_ROW_HEIGHT))
            .bg(tokens.colors.surface)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .overflow_x_hidden()
            .child(
                div()
                    .w(px(SELECT_COLUMN_WIDTH))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(grid_checkbox(
                        SharedString::from("db-select-all"),
                        header_state,
                        SharedString::from(tr(cx, "勾选本页全部行")),
                        {
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.toggle_all_checked(cx));
                            }
                        },
                        cx,
                    )),
            )
            .children(columns.iter().enumerate().map(|(index, column)| {
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
                let cell = div()
                    .id(SharedString::from(format!("db-head-{}", column.name)))
                    .w(px(widths.get(index).copied().unwrap_or(COLUMN_MIN_WIDTH)))
                    .flex_none()
                    .px(tokens.spacing.xs)
                    .flex()
                    .items_center()
                    .overflow_x_hidden()
                    .text_size(tokens.typography.xs.size)
                    .line_height(px(GRID_ROW_HEIGHT))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.muted_foreground)
                    .child(div().truncate().child(match arrow {
                        Some(arrow) => SharedString::from(format!("{}{arrow}", column.name)),
                        None => SharedString::from(column.name.clone()),
                    }));
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
                    .into_any_element()
            }));

        // `view` 下面要被 `uniform_list` 的 `move` 闭包拿走，结果区的右键菜单留一份自己的。
        let result_view = view.clone();
        let grid = uniform_list(
            "db-result-grid",
            row_count.max(1),
            move |range, _window, cx| {
                range
                    .filter_map(|index| {
                        let row = rows.get(index)?;
                        let selected = selected_row == Some(index);
                        let checked = checked_rows.contains(&index);
                        // 右键菜单闭包 `'static`：整行文本与文案都在这里备好。
                        let row_text = row_as_tsv(row);
                        let copy_row_label = tr(cx, "复制整行");
                        let copied_row_note = tr(cx, "已复制该行。");
                        Some(
                            h_flex()
                                .id(SharedString::from(format!("db-row-{index}")))
                                .h(px(GRID_ROW_HEIGHT))
                                .w_full()
                                .flex_none()
                                .items_center()
                                .overflow_x_hidden()
                                .when(selected, |row| row.bg(selected_bg))
                                .hover(move |style| style.bg(hover))
                                .on_click({
                                    let view = view.clone();
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let Some(entity) = view.upgrade() else {
                                            return;
                                        };
                                        // 只读借用阶段取数据，随后构造输入框实体。
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
                                .child(
                                    div()
                                        .w(px(SELECT_COLUMN_WIDTH))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
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
                                // 行级右键：整行一条 TSV，粘进表格软件就是一整行单元格。
                                .context_menu({
                                    let view = view.clone();
                                    move |menu, _, _| {
                                        let view = view.clone();
                                        let text = row_text.clone();
                                        menu.item(PopupMenuItem::new(copy_row_label).on_click(
                                            move |_, _, cx| {
                                                let Some(entity) = view.upgrade() else {
                                                    return;
                                                };
                                                entity.update(cx, |this, cx| {
                                                    this.put_on_clipboard(
                                                        text.clone(),
                                                        copied_row_note,
                                                        cx,
                                                    );
                                                });
                                            },
                                        ))
                                    }
                                })
                                .children(row.iter().enumerate().map(|(cell_index, cell)| {
                                    // 右键菜单的闭包是 `'static`：单元格文本先取出来带走。
                                    let cell_value = cell.render();
                                    let null_cell = cell.is_null();
                                    let copy_cell_label = tr(cx, "复制单元格");
                                    let copied_cell_note = tr(cx, "已复制单元格。");
                                    div()
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
                                        .child(
                                            div()
                                                .truncate()
                                                .when(null_cell, |text| text.text_color(tertiary))
                                                .child(cell_value.clone()),
                                        )
                                        .context_menu({
                                            let view = view.clone();
                                            move |menu, _, _| {
                                                let view = view.clone();
                                                let text = cell_value.clone();
                                                menu.item(
                                                    PopupMenuItem::new(copy_cell_label).on_click(
                                                        move |_, _, cx| {
                                                            let Some(entity) = view.upgrade()
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
                                                        },
                                                    ),
                                                )
                                            }
                                        })
                                })),
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .flex_1()
        .min_h_0();

        let mut panel = v_flex().flex_1().min_h_0().min_w_0();
        // 编辑条：结果集可编辑（单表 + 主键齐全）时才出来，承载「新增行 / 提交 / 丢弃」。
        if self.edit_source().is_some() {
            panel = panel.child(self.render_edit_bar(cx));
        }
        // 多选条只在勾了行时出现，且不限模式（SQL 结果集同样可以批量改 / 删）。
        if !self.checked_rows.is_empty() {
            panel = panel.child(self.render_batch_bar(cx));
        }
        panel = panel.child(header).child(grid);
        if truncated {
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
        if self.mode == Mode::Table {
            panel = panel.child(self.render_pager(cx));
        }
        // 结果区整体右键：一次拿走全部行（含表头）。
        let copy_all_label = tr(cx, "复制全部（含表头）");
        let copied_all_note = tr(cx, "已复制全部结果。");
        let all_text = result_as_tsv(result);
        panel
            .id("db-result-panel")
            .context_menu({
                let view = result_view.clone();
                move |menu, _, _| {
                    let view = view.clone();
                    let text = all_text.clone();
                    menu.item(
                        PopupMenuItem::new(copy_all_label).on_click(move |_, _, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            entity.update(cx, |this, cx| {
                                this.put_on_clipboard(text.clone(), copied_all_note, cx);
                            });
                        }),
                    )
                }
            })
            .into_any_element()
    }

    /// 编辑条：`pending_edits` 模型的界面落点（新增行 + 待提交计数 + 提交 / 丢弃）。
    ///
    /// 与「多选工具条」分开排：多选条是**临时的**（勾了才有），编辑条只要结果集可编辑
    /// 就常驻——「我这一批改动攒着没提交」这件事必须随时看得见。
    fn render_edit_bar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let pending = self.pending_total();

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
            .child(
                button(
                    "db-row-insert",
                    tr(cx, "新增行"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(self.busy || self.row_editor.is_some())
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let Some(entity) = view.upgrade() else {
                            return;
                        };
                        // 只读借用阶段取列清单，再构造输入框实体（与点行开编辑器同形）。
                        let specs = entity.read(cx).new_row_specs();
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
                        entity.update(cx, |this, cx| {
                            this.selected_row = None;
                            this.row_editor = cells.map(|cells| RowEditor {
                                row_index: None,
                                cells,
                            });
                            cx.notify();
                        });
                    }
                }),
            );
        if let Some(spec) = self.table_sort.as_ref() {
            bar = bar.child(neutral_tag(
                SharedString::from(trf(
                    cx,
                    "按 {} {}",
                    &[&spec.column, if spec.descending { "降序" } else { "升序" }],
                )),
                cx,
            ));
        }
        bar = bar.child(div().flex_1());
        if pending > 0 {
            bar = bar
                .child(accent_tag(
                    SharedString::from(trf(cx, "{} 条待提交", &[&pending.to_string()])),
                    cx,
                ))
                .child(
                    button(
                        "db-pending-commit",
                        tr(cx, "提交"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(self.busy)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.commit_pending(window, cx));
                        }
                    }),
                )
                .child(
                    button(
                        "db-pending-discard",
                        tr(cx, "丢弃"),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .disabled(self.busy)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.discard_pending(window, cx));
                        }
                    }),
                );
        }
        bar.into_any_element()
    }

    /// 多选工具条：勾了行才出现（盖在网格表头之上）。
    ///
    /// 两个写动作都落在**单事务**内核接口上（批量改 → [`Connection::update_cells`]，
    /// 批量删 → [`Connection::delete_row_set`]）：「要么这一批全成、要么全不动」是
    /// 跨行操作的底线——半截成功比失败更难收拾。
    fn render_batch_bar(&self, cx: &Context<Self>) -> AnyElement {
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

    /// 表浏览分页条。
    fn render_pager(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let can_next = self
            .table_rows
            .as_ref()
            .is_some_and(|rows| rows.row_count() >= PAGE_SIZE);
        h_flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(
                        button(
                            "db-page-prev",
                            tr(cx, "上一页"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(self.busy || self.table_page == 0)
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.table_prev_page(cx));
                            }
                        }),
                    )
                    .child(
                        button(
                            "db-page-next",
                            tr(cx, "下一页"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(self.busy || !can_next)
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.table_next_page(cx));
                            }
                        }),
                    )
                    .child(meta_text(
                        SharedString::from(trf(
                            cx,
                            "第 {} 页 · 每页 {} 行",
                            &[&(self.table_page + 1).to_string(), &PAGE_SIZE.to_string()],
                        )),
                        cx,
                    )),
            )
            .child(
                button("db-refresh-page", tr(cx, "刷新"), ButtonVariant::Ghost, cx)
                    .disabled(self.busy)
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.refresh_table_page(cx));
                        }
                    }),
            )
            .into_any_element()
    }
}

/// 环境标签的展示文本（`None` 不展示）。
fn env_tag_label(tag: EnvTag, cx: &App) -> Option<SharedString> {
    let label = match tag {
        EnvTag::None => return None,
        EnvTag::Dev => tr(cx, "开发"),
        EnvTag::Test => tr(cx, "测试"),
        EnvTag::Staging => tr(cx, "预发布"),
        EnvTag::Prod => tr(cx, "生产"),
    };
    Some(SharedString::from(label))
}

/// 引擎展示名（源串是 ASCII，不进 i18n 表）。
fn kind_label(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => "SQLite",
        DatabaseKind::Postgres => "PostgreSQL",
        DatabaseKind::Mysql => "MySQL",
        DatabaseKind::ClickHouse => "ClickHouse",
        DatabaseKind::SqlServer => "SQL Server",
        DatabaseKind::Mongo => "MongoDB",
        DatabaseKind::Elasticsearch => "Elasticsearch",
    }
}

/// 引擎惯用的管理账号（新建连接时预填，省掉手打）：MySQL 系用 `root`，
/// PostgreSQL 系用 `postgres`，ClickHouse 用它的默认账号 `default`，
/// SQL Server 用内建管理员 `sa`；SQLite 无账号概念，返回空串。
///
/// Elasticsearch 预填内建超级用户 `elastic`：8.x 起安全模块**默认开启**，
/// 内建账号就是它。安全性没开的集群**不会**因此报错——`xpack.security.enabled:
/// false` 时 Security 模块整个不加载，凭证字段被直接忽略（驱动那边也只是
/// 把它拼成 `Authorization` 头，服务端不看）。真要让出空账号，表单里清掉即可。
fn default_user(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => "",
        DatabaseKind::Postgres => "postgres",
        DatabaseKind::Mysql => "root",
        DatabaseKind::ClickHouse => "default",
        DatabaseKind::SqlServer => "sa",
        DatabaseKind::Mongo => "admin",
        DatabaseKind::Elasticsearch => "elastic",
    }
}

/// 库名留空时落到哪里 —— **文案住在界面层**。
///
/// 内核只给「是不是必填」这类事实（`DatabaseKind::needs_database`），不吐成品中文：
/// 零依赖的 `duck-db` 不参与 i18n，字面量也不该出现在那边。而这件事**偏偏只有
/// 界面能说清**——「不填库名」在五个网络引擎上是五种语义（见各驱动模块文档），
/// 泛泛写一句「可留空」等于没说。
fn database_hint(kind: DatabaseKind, cx: &App) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => tr(
            cx,
            "SQLite 这里填数据库文件路径（:memory: 也允许），不可留空。",
        ),
        DatabaseKind::Postgres => tr(cx, "可留空。留空时 PostgreSQL 连到用户名同名的库。"),
        DatabaseKind::Mysql => tr(
            cx,
            "可留空。留空即不选库：连上后在命名空间下拉里选一个，或把表名写成 db.table。",
        ),
        DatabaseKind::ClickHouse => tr(cx, "可留空。留空用惯例库 default。"),
        DatabaseKind::SqlServer => tr(cx, "可留空。留空落入登录名的默认库（通常 master）。"),
        DatabaseKind::Mongo => tr(cx, "可留空。留空即不选库，连上后在命名空间下拉里选一个。"),
        DatabaseKind::Elasticsearch => tr(
            cx,
            "可留空。这里填默认索引或别名（支持 logs-* 这类通配），留空即全部索引。",
        ),
    }
}

/// 表 / 视图的小图标。
fn table_kind_icon(kind: TableKind) -> DuckIcon {
    match kind {
        TableKind::Table => DuckIcon::Columns2,
        TableKind::View => DuckIcon::Layers,
        // 函数 / 序列：这两类对象**不能当表浏览**（见 [`TableKind::is_relation`]），
        // 右栏只给出图标与名字，点开走 «生成查询骨架» 那条路。
        TableKind::Routine => DuckIcon::SquareFunction,
        TableKind::Sequence => DuckIcon::ListOrdered,
    }
}

/// 目录对象种类的中文名（左栏 tooltip / 补全详情共用）。
fn table_kind_label(kind: TableKind, cx: &App) -> String {
    match kind {
        TableKind::Table => tr(cx, "表"),
        TableKind::View => tr(cx, "视图"),
        TableKind::Routine => tr(cx, "函数"),
        TableKind::Sequence => tr(cx, "序列"),
    }
    .to_string()
}

/// 函数 / 序列点开时放进编辑器的**引用骨架**。
///
/// 这两类对象不能像表那样翻页读，但也不能「点了没反应」——它们各自有唯一一种
/// 在 SQL 里被引用的写法，直接给出来最省事（名字已按引擎的限定名规则拼好）。
fn object_snippet(kind: TableKind, engine: DatabaseKind, name: &str) -> String {
    match kind {
        TableKind::Table | TableKind::View => format!("SELECT * FROM {name} LIMIT 100;"),
        // 函数与存储过程在 `information_schema.routines` 里同处一张表，分不出是哪种：
        // MySQL 侧按过程调用（`CALL`），其余引擎按表函数（`SELECT * FROM fn()`)。
        TableKind::Routine => match engine {
            DatabaseKind::Mysql => format!("CALL {name}();"),
            _ => format!("SELECT * FROM {name}();"),
        },
        // 序列只有 PostgreSQL 有（`nextval` 是它的取号函数）。
        TableKind::Sequence => format!("SELECT nextval('{name}');"),
    }
}

impl DatabasePage {
    /// 右栏：连接信息 + 对象树 + 选中行详情。
    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let Some(session) = self.active_session() else {
            return v_flex()
                .id("db-right-sidebar")
                .size_full()
                .min_h_0()
                .overflow_y_scroll()
                .px(tokens.spacing.lg)
                .py(tokens.spacing.md)
                .child(empty_state(
                    DuckIcon::Database,
                    tr(cx, "尚未连接数据库"),
                    Some(tr(cx, "在左栏选择一条连接，或新建一条。").into()),
                    None,
                    cx,
                ))
                .into_any_element();
        };

        // ── 命名空间选择器（多 schema / 多库）──
        //
        // 只有一套命名空间时（SQLite 的 `main`、单库 MySQL）不给控件：多一个只能选
        // 一项的下拉是纯噪音。
        let mut schema_picker = None;
        if session.schemas.len() > 1 {
            let options: Vec<(SharedString, SharedString)> = session
                .schemas
                .iter()
                .map(|name| {
                    (
                        SharedString::from(name.clone()),
                        SharedString::from(name.clone()),
                    )
                })
                .collect();
            // 当前值可能不在清单里（引擎缺省命名空间没被 `list_schemas` 报出来）→
            // 原样展示，选别的才会触发切换。
            let current = session
                .schema
                .clone()
                .unwrap_or_else(|| tr(cx, "当前命名空间").to_string());
            schema_picker = Some(dropdown_select(
                "db-schema-picker",
                options,
                SharedString::from(current),
                {
                    let view = view.clone();
                    move |value: SharedString, _window, cx| {
                        let Some(entity) = view.upgrade() else {
                            return;
                        };
                        entity.update(cx, |this, cx| {
                            this.select_schema(Some(value.to_string()), cx);
                        });
                    }
                },
                cx,
            ));
        }

        // ── 对象树：按种类分组（表 / 视图 / 函数 / 序列），组内按名字排 ──
        //
        // 分组而不是一锅端：PG 库里函数与序列动辄几十个，混在表中间会把表淹掉。
        let mut objects = v_flex().w_full().gap(tokens.spacing.sm);
        let mut object_total = 0usize;
        for (kind, heading) in [
            (TableKind::Table, "表"),
            (TableKind::View, "视图"),
            (TableKind::Routine, "函数"),
            (TableKind::Sequence, "序列"),
        ] {
            let mut entries: Vec<&CatalogTable> = session
                .catalog
                .iter()
                .filter(|entry| entry.kind == kind)
                .collect();
            if entries.is_empty() {
                continue;
            }
            entries.sort_by(|left, right| left.name.cmp(&right.name));
            object_total += entries.len();
            let mut group = v_flex().w_full().gap(tokens.spacing.xxs).child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(caption_heading(tr(cx, heading), cx))
                    .child(meta_text(entries.len().to_string(), cx)),
            );
            for entry in entries {
                let name = entry.name.clone();
                let reference = entry.table_ref();
                let selected = self.selected_table.as_ref() == Some(&reference);
                group = group.child(
                    sidebar_navigation_button(
                        SharedString::from(format!("db-object-{}", reference.display())),
                        SharedString::from(name.clone()),
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
                        let view = view.clone();
                        let reference = reference.clone();
                        move |_, window, cx| {
                            let Some(entity) = view.upgrade() else {
                                return;
                            };
                            // 表 / 视图：翻表页浏览。函数 / 序列：不能当表读，
                            // 往编辑器里放一段引用骨架（这是它们在界面上的**真实功能**，
                            // 不是占位）。
                            let browsable = kind.is_relation();
                            let engine = entity
                                .read(cx)
                                .active_config()
                                .map_or(DatabaseKind::Sqlite, |cfg| cfg.kind);
                            if browsable {
                                entity.update(cx, |this, cx| {
                                    this.open_table(reference.clone(), window, cx);
                                });
                            } else {
                                let snippet = object_snippet(kind, engine, &reference.display());
                                entity.update(cx, |this, cx| {
                                    this.sql.update(cx, |state, cx| {
                                        state.set_value(snippet, window, cx);
                                    });
                                    this.status = tr(cx, "已把引用骨架放进编辑器。").into();
                                    cx.notify();
                                });
                            }
                        }
                    }),
                );
            }
            objects = objects.child(group);
        }
        if object_total == 0 {
            objects = objects.child(
                div()
                    .py(tokens.spacing.sm)
                    // 库名可留空：什么都没选时列表当然是空的，得说清下一步，别让人
                    // 以为「这个库是空的」。
                    .child(meta_text(
                        tr(
                            cx,
                            "当前库没有表或视图。若还没选定库，请在上方的命名空间下拉里选一个。",
                        ),
                        cx,
                    )),
            );
        }

        // 选中行：有编辑器就出暂存编辑卡，否则出只读详情。
        //
        // 不再按 `Mode` 分支：可编辑 SELECT 的结果集也能改（`edit_source` 说了算），
        // 表浏览与 SQL 结果在编辑这件事上没有区别。
        let row_panel = match self.row_editor.as_ref() {
            Some(editor) => Some(self.render_row_editor(editor, cx)),
            None => self.readonly_row_detail(cx),
        };

        v_flex()
            .id("db-right-sidebar")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .gap(tokens.spacing.lg)
            .px(tokens.spacing.lg)
            .py(tokens.spacing.md)
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
                            .child(description_list(vec![
                                description_row(
                                    tr(cx, "引擎"),
                                    kind_label(
                                        self.active_config()
                                            .map(|cfg| cfg.kind)
                                            .unwrap_or(DatabaseKind::Sqlite),
                                    ),
                                ),
                                description_row(tr(cx, "版本"), session.version.clone()),
                                description_row(
                                    tr(cx, "接入方式"),
                                    if session.tunnel.is_some() {
                                        tr(cx, "SSH 隧道")
                                    } else {
                                        tr(cx, "直连")
                                    },
                                ),
                            ])),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "对象"), cx))
                    .children(schema_picker)
                    .child(objects),
            )
            .children(
                (self.mode == Mode::Table)
                    .then(|| self.table_ddl.clone())
                    .flatten()
                    .map(|ddl| {
                        v_flex()
                            .w_full()
                            .gap(tokens.spacing.xs)
                            .child(caption_heading(tr(cx, "DDL"), cx))
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
                                            .child(SharedString::from(ddl)),
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
            .into_any_element()
    }

    fn active_config(&self) -> Option<&ConnectionConfig> {
        let id = self.active.as_deref()?;
        self.connections.iter().find(|cfg| cfg.id == id)
    }

    /// 右栏「表级元信息」：行数 + 索引。
    ///
    /// 数据来自 [`Self::load_table_page`]（只在换表 / 刷新时取一次），这里纯展示，
    /// 渲染路径上没有数据库调用。
    fn render_table_meta(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();

        // 行数：口径直接写进文案（精确 vs 统计估算），别让用户以为估算值是实数。
        let row_count_text: SharedString = match self.table_row_count {
            Some(RowCount::Exact(count)) => trf(cx, "{} 行", &[&count.to_string()]).into(),
            // PG 从未 `ANALYZE` 过时 `reltuples` 是 -1（内核落成 0）。
            Some(RowCount::Estimate(0)) => tr(cx, "未知（未统计）").into(),
            Some(RowCount::Estimate(count)) => trf(cx, "约 {} 行", &[&count.to_string()]).into(),
            None => tr(cx, "未知").into(),
        };

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
            index_list = index_list.child(
                v_flex()
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
                    ),
            );
        }
        if self.table_indexes.is_empty() {
            index_list = index_list.child(meta_text(tr(cx, "该表没有索引。"), cx));
        }

        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
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
            .into_any_element()
    }

    /// 只读行详情卡：列名 → 展示值。
    fn readonly_row_detail(&self, cx: &App) -> Option<AnyElement> {
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
    fn render_row_editor(&self, editor: &RowEditor, cx: &App) -> AnyElement {
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

impl Render for DatabasePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 连接的新建 / 编辑走弹窗子页（`open_edit_dialog`），主区始终是
        // 「标签条 + 编辑器 / 结果」——不再有「主区变身表单」这一态。
        let content = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(self.render_tabs(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.render_body(cx)),
            )
            .into_any_element();

        let mut panel = SidebarPanel::new("db-content", &self.sidebar, content, window, cx);
        if panel.is_sidebar_visible() {
            panel = panel.sidebar(self.render_sidebar(cx));
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            // 键位落在页面根上：焦点在 SQL 编辑器里时，上下文栈是
            // `Editor → … → Database → Root`，页面级的 ⌘↵ / ⌘S 在更浅的一层生效，
            // 编辑器的 ⌘Z / ⌘. 仍归编辑器自己（更深优先）。
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &RunOrStopQuery, window, cx| {
                if this.running.is_some() {
                    this.stop_running(window, cx);
                } else {
                    this.run_query(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CommitPendingEdits, window, cx| {
                this.commit_pending(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DiscardPendingEdits, window, cx| {
                this.discard_pending(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopySelectedRow, _window, cx| {
                this.copy_selected_row(cx);
            }))
            .child(div().flex_1().min_h_0().min_w_0().child(panel))
            .child(self.render_status_bar(cx))
    }
}

impl DatabasePage {
    /// 底部状态栏。
    fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        h_flex()
            .flex_none()
            .h(theme.density().status_bar)
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(div().truncate().child(self.status.clone())),
            )
            // 事务状态：`BEGIN` 挂着的时候给一个显眼的档位 + 两个出口（提交 / 回滚）。
            // 连接是常驻的，语句之间的事务由服务器维持——这里只负责把它显示出来。
            .when(
                self.active_session().is_some_and(|session| session.in_tx),
                |bar| {
                    let commit_view = self.view.clone();
                    let rollback_view = self.view.clone();
                    bar.child(accent_tag(tr(cx, "事务中"), cx))
                        .child(
                            button("db-tx-commit", tr(cx, "提交"), ButtonVariant::Secondary, cx)
                                .disabled(self.busy)
                                .on_click(move |_, window, cx| {
                                    let _ = commit_view.update(cx, |this, cx| {
                                        this.execute_sql("COMMIT".to_owned(), window, cx);
                                    });
                                }),
                        )
                        .child(
                            button(
                                "db-tx-rollback",
                                tr(cx, "回滚"),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .disabled(self.busy)
                            .on_click(move |_, window, cx| {
                                let _ = rollback_view.update(cx, |this, cx| {
                                    this.execute_sql("ROLLBACK".to_owned(), window, cx);
                                });
                            }),
                        )
                },
            )
            // 待提交档位：暂存了改动但还没提交时一直挂着——「攒着没落库」这件事
            // 必须在关窗口之前看得见（关掉页面暂存就丢了）。
            .when(self.pending_total() > 0, |bar| {
                bar.child(accent_tag(
                    SharedString::from(trf(
                        cx,
                        "待提交 {} 条",
                        &[&self.pending_total().to_string()],
                    )),
                    cx,
                ))
            })
            .child(meta_text(
                SharedString::from(trf(cx, "{} 个连接", &[&self.connections.len().to_string()])),
                cx,
            ))
            .into_any_element()
    }
}

/// 「标签 + 输入框」一行（表单专用；标签列宽与设置页的 `option_row` 对齐）。
fn field_row(cx: &App, label: impl Into<SharedString>, input: &Entity<InputState>) -> gpui::Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    h_flex()
        .w_full()
        .items_center()
        .gap(tokens.spacing.lg)
        .child(field_label(label, cx))
        .child(
            div().flex_1().min_w_0().child(
                Input::new(input)
                    .h(theme.density().control)
                    .text_size(tokens.typography.sm.size)
                    .w_full(),
            ),
        )
}

/// 详情页分区标题：图标 + 大写字距标题 + 通栏 hairline（dbx 的 SERVER / AUTHENTICATION 同形）。
fn db_section_header(icon: DuckIcon, label: &'static str, cx: &App) -> gpui::Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    h_flex()
        .w_full()
        .flex_none()
        .items_center()
        .gap(tokens.spacing.xs)
        .pb(tokens.spacing.xs)
        .border_b_1()
        .border_color(tokens.colors.border)
        .child(
            Icon::new(icon)
                .size(theme.extended().icon.sm)
                .text_color(tokens.colors.muted_foreground),
        )
        .child(
            // 英文分区名走 ASCII，无需进 i18n 表（与「BETA」角标同一口径）。
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.colors.muted_foreground)
                .child(label),
        )
}

/// 环境标签的语义色圆点：生产=危险、预发布=警告、测试=强调、开发=信息、无=二级文字。
fn env_dot_color(tag: EnvTag, cx: &App) -> Hsla {
    let theme = active_theme(cx);
    let colors = theme.tokens().colors;
    let extended = theme.extended().colors;
    match tag {
        EnvTag::Prod => colors.destructive,
        EnvTag::Staging => extended.warning,
        EnvTag::Test => extended.accent_text,
        EnvTag::Dev => extended.info,
        EnvTag::None => colors.muted_foreground,
    }
}

/// [`field_row`] 的必填变体：标签落危险色（与「标红的字段为必填项」呼应）。
fn field_row_required(
    cx: &App,
    label: impl Into<SharedString>,
    input: &Entity<InputState>,
) -> gpui::Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    h_flex()
        .w_full()
        .items_center()
        .gap(tokens.spacing.lg)
        .child(field_label(label, cx).text_color(tokens.colors.destructive))
        .child(
            div().flex_1().min_w_0().child(
                Input::new(input)
                    .h(theme.density().control)
                    .text_size(tokens.typography.sm.size)
                    .w_full(),
            ),
        )
}

impl Render for EditFormView {
    /// dbx 同形的详情正文：头部（返回 + 引擎 logo + 标题 / 副标题 + 连接名称 + 关闭）
    /// 常驻，三分区（SERVER / AUTHENTICATION / TRANSPORT）独立滚动。
    ///
    /// 脚注（测试连接 / 删除 / 取消 / 保存连接）挂在弹窗外壳上 —— 它不属于表单，
    /// 也不需要跟着表单状态重绘。
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let icon_md = theme.extended().icon.md;
        let is_new = self.id.is_none();
        let network = self.kind.needs_host();

        // ── 头部：返回 + 引擎 logo + 标题 / 副标题 + 连接名称 + 关闭 ──
        let header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                toolbar_action_button(
                    "db-edit-back",
                    tr(cx, "返回"),
                    Icon::new(DuckIcon::ChevronLeft).size(icon_md),
                    false,
                    false,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    // 返回 = 回到 picker：新建态回上一步，编辑态直接关窗。
                    window.close_dialog(cx);
                    if is_new {
                        let _ = this
                            .page
                            .update(cx, |page, cx| page.open_new_connection_picker(window, cx));
                    }
                })),
            )
            .child(kind_logo(self.kind).element(px(EDIT_LOGO_SIZE)))
            .child(
                v_flex()
                    .min_w_0()
                    .gap(tokens.spacing.xxs)
                    .child(title_text(
                        trf(
                            cx,
                            if is_new {
                                "新建 {} 连接"
                            } else {
                                "编辑 {} 连接"
                            },
                            &[kind_label(self.kind)],
                        ),
                        cx,
                    ))
                    .child(meta_text(tr(cx, "标红的字段为必填项。"), cx)),
            )
            .child(div().flex_1())
            .child(
                // 连接名输入框宽度：布局约束（不在 spacing 阶梯上），保留裸值。
                div().w(px(196.)).child(
                    Input::new(&self.name)
                        .h(theme.density().control)
                        .text_size(tokens.typography.sm.size)
                        .w_full(),
                ),
            )
            .child(
                toolbar_action_button(
                    "db-edit-close",
                    tr(cx, "关闭"),
                    Icon::new(DuckIcon::X).size(icon_md),
                    false,
                    false,
                    cx,
                )
                .on_click(|_, window, cx| window.close_dialog(cx)),
            );

        // ── 环境：带语义色圆点的 pill，选中即改档案标签 ──
        let env_pills = h_flex()
            .flex_wrap()
            .items_center()
            .gap(tokens.spacing.xxs)
            .children(EnvTag::ALL.map(|tag| {
                let selected = self.env_tag == tag;
                let label = env_tag_label(tag, cx).unwrap_or_else(|| tr(cx, "无标签").into());
                let dot = div()
                    // 环境色点直径：视觉标记（不是间距步进），阶梯里没有对应档。
                    .size(px(8.))
                    .flex_none()
                    .rounded_full()
                    .bg(env_dot_color(tag, cx));
                icon_text_button(
                    SharedString::from(format!("db-env-{}", tag.wire_name())),
                    label,
                    dot,
                    if selected {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    },
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.env_tag = tag;
                    cx.notify();
                }))
            }));

        // ── SERVER：主机（必填）+ 端口、数据库、分组；SQLite 只有文件与分组 ──
        let mut server_fields = v_flex().w_full().gap(form_gap(cx)).child(db_section_header(
            DuckIcon::Server,
            "SERVER",
            cx,
        ));
        if network {
            server_fields = server_fields
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.lg)
                        .child(
                            // 必填标签落危险色（与副标题「标红的字段为必填项」呼应）。
                            field_label(tr(cx, "主机"), cx).text_color(tokens.colors.destructive),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&self.host)
                                    .h(theme.density().control)
                                    .text_size(tokens.typography.sm.size)
                                    .w_full(),
                            ),
                        )
                        .child(
                            // 端口输入框宽度：布局约束（4 位数足够，再宽浪费），保留裸值。
                            div().w(px(104.)).child(
                                Input::new(&self.port)
                                    .h(theme.density().control)
                                    .text_size(tokens.typography.sm.size)
                                    .w_full(),
                            ),
                        ),
                )
                .child(field_row(cx, tr(cx, "数据库"), &self.database))
                .child(field_hint(database_hint(self.kind, cx), cx))
                .child(field_row(cx, tr(cx, "分组"), &self.group));
        } else {
            server_fields = server_fields
                .child(field_row(cx, tr(cx, "数据库文件"), &self.database))
                .child(field_hint(database_hint(self.kind, cx), cx))
                .child(field_row(cx, tr(cx, "分组"), &self.group));
        }

        // ── QUERY：默认命名空间 + 连接级只读档 + 该连接的默认查询超时 ──
        //
        // 三项都是「连接级」而非「某次执行」的设置，与 SERVER / AUTHENTICATION 的
        // 物理连接参数分开排。读写档放这里而不是 TRANSPORT：它管的是**权限**而非传输。
        let read_only_toggle = button(
            "db-readonly-toggle",
            if self.read_only {
                tr(cx, "只读")
            } else {
                tr(cx, "可写")
            },
            if self.read_only {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Secondary
            },
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.read_only = !this.read_only;
            cx.notify();
        }));
        let query_section = v_flex()
            .w_full()
            .gap(form_gap(cx))
            .child(db_section_header(DuckIcon::Gauge, "QUERY", cx))
            .child(field_row(cx, tr(cx, "默认命名空间"), &self.schema))
            .child(field_hint(
                tr(cx, "连接后默认浏览的 schema / 库；留空用引擎当前命名空间。"),
                cx,
            ))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "读写档"), cx))
                    .child(read_only_toggle),
            )
            .child(field_hint(
                tr(
                    cx,
                    "只读档由服务器兜底拒绝写语句（不是界面软拦）；切到只读不需要重连。",
                ),
                cx,
            ))
            .child(field_row(cx, tr(cx, "查询超时（秒）"), &self.timeout))
            .child(field_hint(
                tr(
                    cx,
                    "只作用于读路径（执行 SQL、读表、AI 只读执行）；留空用全局默认 30 秒。",
                ),
                cx,
            ));

        // ── AUTHENTICATION：用户名（必填）+ 口令（掩码）。仅网络引擎。 ──
        let auth_section = network.then(|| {
            v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(db_section_header(DuckIcon::User, "AUTHENTICATION", cx))
                .child(field_row_required(cx, tr(cx, "用户名"), &self.user))
                .child(field_row(cx, tr(cx, "密码"), &self.password))
        });

        // ── TRANSPORT：SSL 模式 + SSH 隧道。仅网络引擎。 ──
        let transport_section = network.then(|| {
            let tls_switch = h_flex()
                .flex_wrap()
                .items_center()
                .gap(tokens.spacing.xxs)
                .children(TlsMode::ALL.map(|mode| {
                    let selected = self.tls == mode;
                    let label = match mode {
                        TlsMode::Disable => tr(cx, "不用 TLS"),
                        TlsMode::Trust => tr(cx, "校验证书"),
                        TlsMode::DangerAcceptInvalid => tr(cx, "接受自签"),
                    };
                    button(
                        SharedString::from(format!("db-tls-{}", mode.wire_name())),
                        SharedString::from(label),
                        if selected {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Secondary
                        },
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tls = mode;
                        cx.notify();
                    }))
                }));
            let ssh_toggle = button(
                "db-ssh-toggle",
                if self.ssh_enabled {
                    tr(cx, "经 SSH 隧道")
                } else {
                    tr(cx, "直连")
                },
                if self.ssh_enabled {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.ssh_enabled = !this.ssh_enabled;
                cx.notify();
            }));
            let mut ssh_block = v_flex().w_full().gap(form_gap(cx)).child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "SSH 隧道"), cx))
                    .child(ssh_toggle),
            );
            if self.ssh_enabled {
                ssh_block = ssh_block
                    .child(field_row(cx, tr(cx, "跳板机"), &self.ssh_host))
                    .child(field_row(cx, tr(cx, "SSH 端口"), &self.ssh_port))
                    .child(field_row(cx, tr(cx, "SSH 用户"), &self.ssh_user))
                    .child(field_row(cx, tr(cx, "转发目标"), &self.ssh_remote_host))
                    .child(field_row(cx, tr(cx, "SSH 口令"), &self.ssh_password));
            }
            v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(db_section_header(DuckIcon::Globe, "TRANSPORT", cx))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.lg)
                        .child(field_label(tr(cx, "SSL 模式"), cx))
                        .child(tls_switch),
                )
                .child(ssh_block)
        });

        // 头部常驻、正文独立滚动：长表单（SSH 展开）不会把脚注挤出弹窗。
        div()
            .w_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.sm)
                    .border_b_1()
                    .border_color(tokens.colors.border)
                    .child(header),
            )
            .child(
                div()
                    .id("db-edit-body")
                    .w_full()
                    .min_h_0()
                    .max_h(px(EDIT_DIALOG_BODY_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.md)
                    .child(
                        v_flex()
                            .w_full()
                            .gap(tokens.spacing.lg)
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_start()
                                    .gap(tokens.spacing.lg)
                                    .child(field_label(tr(cx, "环境"), cx))
                                    .child(
                                        v_flex().gap(tokens.spacing.xxs).child(env_pills).child(
                                            field_hint(tr(cx, "标明此连接指向的环境。"), cx),
                                        ),
                                    ),
                            )
                            .child(server_fields)
                            .child(query_section)
                            .children(auth_section)
                            .children(transport_section),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_db::Value;
    use gpui::TestAppContext;

    /// `add_window` 要求构建函数返回一个 `Render` 类型；这里只用它的空壳承接断言。
    struct SqlEditorProbe;
    impl Render for SqlEditorProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// SQL 编辑器必须真的挂上「已注册的」高亮语言（B8 的接线验收）。
    ///
    /// 上游对认不出的语言名是**静默不报**（取不到 `LanguageRegistry` 条目只在渲染时记一条
    /// warn，界面照旧出字），所以「把 `"sql"` 敲成 `"sq"`」这类错误编译期与运行期都不会有人
    /// 喊 —— 只有把注册表与编辑器状态两头对一次才拦得住。这一条同时守住
    /// `LanguageRegistry` 里确实有这个名字（即根 `Cargo.toml` 的
    /// `features = ["tree-sitter-sql"]` 没被人顺手删掉）。
    #[gpui::test]
    fn the_sql_editor_is_wired_to_a_registered_language(cx: &mut TestAppContext) {
        let registry = gpui_component::highlighter::LanguageRegistry::singleton();
        assert!(
            registry
                .languages()
                .iter()
                .any(|name| name.as_ref() == SQL_LANGUAGE),
            "`{SQL_LANGUAGE}` 没进 LanguageRegistry：`tree-sitter-sql` feature 可能被关了"
        );

        // `EditorState::new` 需要 `&mut Window`，所以只能在窗口构建里建、也就地读回。
        cx.add_window(|window, cx| {
            let editor = cx.new(|cx| EditorState::new(window, cx).language(SQL_LANGUAGE));
            assert_eq!(
                editor.read(cx).language_name().as_ref(),
                SQL_LANGUAGE,
                "编辑器记下的语言名与常量不一致"
            );
            SqlEditorProbe
        });
    }

    /// 三个页面级键位必须**真的**绑在 `"Database"` 上下文上（B10 的接线验收）。
    ///
    /// 盯的是最容易各写各的三处：动作由 `actions!` 声明、上下文常量 `KEY_CONTEXT` 在
    /// 同一模块、绑定由 [`bind_keys`] 做，而 `app::install` 只负责调一次。任何一处漂了，
    /// 界面照样编译、键位却悄悄失效 —— 只有把键位表**读回来**才能发现，所以这里不测行为、
    /// 测绑定本身。
    ///
    /// 不断言「本页键位不与别处相撞」：那是 `duck_command::bind_all` 的职责（会打
    /// `keybinding conflict` 警告）；这三条选键的依据是 `gpui_base::Root` 的默认表里
    /// 没有它们，属「选型」而非「接线」。
    #[gpui::test]
    fn page_keys_are_bound_in_the_database_context(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        cx.update(|cx| {
            // 声明顺序有讲究：`bindings_for_action` 要求 `&self` 与 `&dyn Action` 同寿，
            // 而 `Ref` 必须比它借的 `Rc` 先析构 —— 动作实例在最前、`Rc` 次之、`Ref` 最后。
            let run = RunOrStopQuery;
            let commit = CommitPendingEdits;
            let discard = DiscardPendingEdits;
            let copy_row = CopySelectedRow;
            let bindings = cx.key_bindings();
            let keymap = bindings.borrow();

            let expected: [(&dyn gpui::Action, &str, bool); 4] = [
                (&run, "enter", false),
                (&commit, "s", false),
                (&discard, "backspace", true),
                (&copy_row, "c", false),
            ];
            let mut strokes_seen = std::collections::BTreeSet::new();
            for (action, key, shift) in expected {
                let binding = keymap
                    .bindings_for_action(action)
                    .next()
                    .unwrap_or_else(|| panic!("`{}` 没有绑定键位", action.name()));
                assert_eq!(
                    binding
                        .predicate()
                        .map(|predicate| predicate.to_string())
                        .as_deref(),
                    Some(KEY_CONTEXT),
                    "`{}` 必须绑在页面上下文里，否则焦点稍深一点就失效",
                    action.name()
                );
                let strokes = binding.keystrokes();
                assert_eq!(strokes.len(), 1, "`{}` 应当是单笔画键位", action.name());
                let stroke = &strokes[0];
                assert_eq!(stroke.key(), key, "`{}` 的键名不对", action.name());
                assert!(
                    stroke.modifiers().secondary(),
                    "`{}` 必须带平台主修饰键（⌘ / Ctrl）",
                    action.name()
                );
                assert_eq!(
                    stroke.modifiers().shift,
                    shift,
                    "`{}` 的 Shift 档位不对",
                    action.name()
                );
                assert!(
                    strokes_seen.insert(stroke.to_string()),
                    "`{}` 与另一个动作挤在同一个键上（重复键不会编译报错，只能在这里拦）",
                    action.name()
                );
            }
        });
    }

    /// 复制出来的 TSV 必须是「能直接粘进表格」的：分隔符、换行、引号都要处理好，
    /// 否则一行有换行的备注就会把整张表错开一行（错位比少复制更难发现）。
    #[test]
    fn tsv_copy_escapes_separators_newlines_and_quotes() {
        assert_eq!(tsv_cell("plain"), "plain");
        assert_eq!(tsv_cell("a\tb"), "\"a\tb\"");
        assert_eq!(tsv_cell("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(tsv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");

        let result = ResultSet {
            columns: vec![
                Column {
                    name: "id".to_owned(),
                    type_name: "INTEGER".to_owned(),
                },
                Column {
                    name: "note".to_owned(),
                    type_name: "TEXT".to_owned(),
                },
            ],
            rows: vec![
                vec![Value::Int(1), Value::Null],
                vec![Value::Int(2), Value::Text("a\tb".to_owned())],
            ],
            truncated: false,
        };
        // 表头 + 每行一条；NULL 走展示文本 `NULL`（与网格里看到的一致）。
        assert_eq!(result_as_tsv(&result), "id\tnote\n1\tNULL\n2\t\"a\tb\"");
        assert_eq!(row_as_tsv(&result.rows[0]), "1\tNULL");
        assert_eq!(row_as_tsv(&result.rows[1]), "2\t\"a\tb\"");
    }
}
