//! 数据库页：连接管理（左栏）+ SQL 编辑器与结果网格（主区）+ 对象详情（右栏）。
//!
//! 布局对齐 dbflux 的三栏形态；交互范式沿用本仓 `TerminalPage`：
//! 左栏走页面内 [`SidebarPanel`]，右栏走 shell 级 `with_right_sidebar`。
//!
//! **引擎范围**（`docs/database-design.md` §5）：SQLite / PostgreSQL / MySQL（含 MariaDB）/
//! ClickHouse / SQL Server / MongoDB / Elasticsearch **七条引擎**共用同一条链路——
//! 连接管理、表浏览（分页）、SQL 执行（多语句 + 行上限截断）、结果网格与导出。
//! 行内编辑、SSH 隧道、PostgreSQL TLS、AI 助手（text-to-SQL 基础版）均已落地，
//! 见 §4 各期「实施修订」。
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
use duck_ai::ProviderProfile;
use duck_components::{
    ButtonVariant, CheckState, ConfirmHandler, DatabaseLogo, DuckIcon, SidebarPanel, SidebarState,
    button, caption_heading, card, check_mark, check_row,
    content::{accent_tag, description_list, description_row, empty_state, neutral_tag},
    dialog_footer, dialog_top_offset, dropdown_select, field_hint, field_label, form_gap,
    icon_button, icon_text_button, meta_text, modal_drag, modal_drag_area, modal_offset,
    modal_title_row, notify, sidebar_navigation_button, title_text, toolbar_action_button,
    toolbar_dropdown_button,
};
use duck_db::ai::{AiAction, build_action_prompt};
use duck_db::config::{
    ConnectionConfig, DatabaseKind, EnvTag, SshTunnelConfig, TlsMode, TransportKind,
    delete_password, delete_profile_proxy_password, delete_profile_ssh_password,
    delete_proxy_password, delete_ssh_password, load_password, load_profile_proxy_password,
    load_profile_ssh_password, load_proxy_password, load_ssh_password, store_password,
    store_profile_proxy_password, store_profile_ssh_password, store_proxy_password,
    store_ssh_password,
};
use duck_db::drivers::CellEdit;
use duck_db::import::import_third_party;
use duck_db::sqlintel::{
    ExplainSkip, SqlRisk, TxEffect, allow_auto_execute, explain_sql_for, format_sql,
    risk_of_statements, split_statement_spans, split_statements, tx_effect,
};
use duck_db::{
    AiProvider, BatchOutcome, BinaryEncoding, CancelHandle, CancelTimer, CatalogTable, Column,
    Connection, ConnectionHook, ConnectionHooks, DEFAULT_HOOK_TIMEOUT_SECS, DatabaseObject,
    DbError, EditTarget, ExecOutcome, FilterOp, HookAction, HookContext, HookFailure, HookOutcome,
    HookPhase, HookReport, HookStatus, InsertRow, InstanceMetric, InstanceOverview, InstanceTable,
    InstanceView, OpenAiCompatible, OrderBy, PkValues, PlanNode, PlanTree, ProxyConfig, ProxyKind,
    ProxyTunnel, ProxyTunnelSpec, ResultSet, RowCount, SshTunnel, SshTunnelSpec, SsmConfig,
    SsmTunnel, SsmTunnelSpec, SwitchOutcome, TableFilter, TableForeignKey, TableIndex, TableKind,
    TableRef, TableTrigger, Value, covers_columns, edit_target, fingerprint, parse_csv, parse_plan,
    quote_qualified, run_hook, run_phase, tsv_field,
};
use duck_theme::{UnderMinute, active_theme, tr, tr_relative_time, trf, trn};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FontWeight, Hsla,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, Role, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, Task, Toggled, WeakEntity,
    Window, actions, div, point, prelude::FluentBuilder as _, px, relative, uniform_list,
};
use gpui_component::{
    Icon, ResizableState, Sizable as _, Size, WindowExt as _,
    chart::{AreaChart, BarChart, LineChart, PieChart},
    dialog::{DialogButtonProps, DialogFooter},
    h_flex,
    input::{CompletionProvider, Editor, EditorState, Input, InputEvent, InputState, Rope},
    menu::{ContextMenuExt as _, PopupMenuItem},
    resizable_panel,
    scroll::{Scrollbar, ScrollbarMode},
    tooltip::Tooltip,
    v_flex, v_resizable,
};
// `CompletionProvider` 的签名直接引用 lsp-types 的类型，实现方必须能具名它们；
// 版本与 gpui-component 经 `gpui_component::input` 转出的 `lsp_types::Position` 同源。
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse};
use serde::{Deserialize, Serialize};

// 审计条目按 `AuditEntry` 的链式构造器拼（语句级事件要带指纹与耗时）。
use crate::audit::AuditEntry;

/// kv 单键：连接档案数组（结构体 → serde_json，范式同 `appearance`）。
const KEY_CONNECTIONS: &str = "db_connections";
/// 表浏览每页行数的**出厂默认**（可在「设置 → 数据库」里改）。
pub(super) const PAGE_SIZE: usize = 200;
/// SQL 查询结果**每页**行数上限（首屏与每次「加载更多」各取一页）的**出厂默认**
///（可在「设置 → 数据库」里改）。
///
/// 首屏由 [`Connection::execute`] 在 `QUERY_ROW_CAP` 处截断；「加载更多」走
/// [`Connection::execute_paged`]，以**已加载行数**为 `offset` 再取一页。
pub(super) const QUERY_ROW_CAP: usize = 10_000;
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
/// 已保存查询 kv 单键（结构化数组，范式同 `KEY_CONNECTIONS`）。
///
/// 与「查询历史」分工明确：历史是**自动**攒下来的最近执行记录（会被上限截掉），
/// 已保存查询是用户**主动**留的一份，两者的生命周期与用途都不同，所以分开存。
const KEY_SAVED_QUERIES: &str = "db_saved_queries";
/// 已保存图表 kv 单键（结构化数组，与已保存查询同范式、分键存）。
///
/// 单独一键而不是并入 [`KEY_SAVED_QUERIES`]：图表比查询多一层列映射，两者的字段集
/// 不同；混在一个数组里得靠 `Option` 字段区分「这是查询还是图表」，反而更难读。
const KEY_SAVED_CHARTS: &str = "db_saved_charts";
/// 用户仪表板 kv 单键（结构化数组，与已保存图表同范式、分键存）。
///
/// 单独一键而不是并进 [`KEY_SAVED_CHARTS`]：图表是「一份数据 + 一层视图配置」，
/// 仪表板是「一堆面板的排布」，生命周期与编辑动作都不重叠；而且面板引用图表 id，
/// 两者同键会让「删图表」这一步同时改到自己所在的容器。
const KEY_DASHBOARDS: &str = "db_dashboards";
/// 会话内钩子执行记录的上限（环形缓冲，超出丢最旧的）。
///
/// 只留在内存里：钩子的输出可能很长、也可能带服务器信息，而「刚才这几个钩子跑了什么」
/// 是一看即抛的会话内问题。⚠️ 它**不**进审计流水（见 `log_hook_report` 的说明）：
/// 每次连接都把每个钩子跑一遍，成功也各记一条只会把该看的淹掉；历史留给 tracing。
const HOOK_LOG_CAP: usize = 200;
/// 可复用「传输档案」kv 单键（结构化数组，范式同 `KEY_CONNECTIONS`）。
///
/// 单独一键而不是把三档散落在每条连接里：一份档案（比如公司唯一的 SOCKS5 出口、
/// 或是那台跳板机）往往被多条连接引用，改主机 / 换口令只该改一处。
/// 连接档案里只留一个 `tunnel_profile` 引用 id。
const KEY_TUNNEL_PROFILES: &str = "db_tunnel_profiles";
/// 查询历史最多保留多少条的**出厂默认**（可在「设置 → 数据库」里改）。
pub(super) const HISTORY_LIMIT: usize = 50;
/// 查询历史面板的最大高度（超出自己滚，不挤占结果区）。
const HISTORY_PANEL_MAX_HEIGHT: f32 = 180.;
/// AI「解释」面板的最大高度（解释可能很长，超出自己滚，同样不挤占结果区）。
const AI_EXPLAIN_MAX_HEIGHT: f32 = 220.;
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
        /// 撤销一步暂存改动（`⌘Z`）；没有可撤销的一步时是空操作。
        UndoPendingEdits,
        /// 重做一步暂存改动（`⇧⌘Z`）。
        RedoPendingEdits,
        /// 复制当前选中的那一行（TSV）—— 网格里 `⌘C` 的自然语义。
        CopySelectedRow,
        /// 中止正在跑的查询（`⌘.`）；没有查询在跑时是空操作。
        StopQuery,
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
        // 取消查询：`⌘.`（macOS 上「取消当前操作」的惯例键位）。工具条「停止」与它是
        // 同一条路径（[`DatabasePage::stop_running`]）；没有查询在跑时不做事。
        KeyBinding::new(&format!("{modifier}-."), StopQuery, Some(KEY_CONTEXT)),
        // 暂存改动的逐步撤销 / 重做。⚠️ `⌘Z` 在**文本输入上下文**里是上游的
        // `input::Undo`（`gpui-base` 的 `cmd-z` 绑定，见 `input/base/state.rs`），
        // 而焦点链上更深的上下文先派发 —— 所以焦点在 SQL 编辑器 / 行编辑器输入框里时，
        // `⌘Z` 退的是**文本**；焦点在页面或结果网格上时才退暂存改动。
        // 这与 `⌘C` 那条是同一套分层，无键入歧义（同一时刻只有一个上下文认领）。
        KeyBinding::new(
            &format!("{modifier}-z"),
            UndoPendingEdits,
            Some(KEY_CONTEXT),
        ),
        KeyBinding::new(
            &format!("{modifier}-shift-z"),
            RedoPendingEdits,
            Some(KEY_CONTEXT),
        ),
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

/// 一条连接钩子的执行记录（会话内环形缓冲的条目）。
///
/// 记的是「谁、哪个相位、跑哪条、什么结果、什么时候」——「刚才那次连接里钩子到底跑成没有」
/// 问的就是这几样，回答完就可以忘，所以留在内存里（上限 [`HOOK_LOG_CAP`]）。
/// ⚠️ 它**不**进审计流水：理由见 [`log_hook_report`]（钩子成功太频繁，记了没信息量）。
#[derive(Debug, Clone)]
struct HookRecord {
    /// 档案 id（按它筛「这条连接的钩子」）。
    connection_id: String,
    /// 执行结果本体（相位 / 命令 / 状态 / 输出 / 耗时）。
    outcome: HookOutcome,
    /// 什么时候跑的（unix 秒）。
    at: i64,
}

/// 建连后台闭包的产物。
///
/// 为什么不是 `Result<Session, DbError>`：钩子报告要和成功 / 失败**并列**带回来 ——
/// 被钩子阻断时既没有会话、又不是引擎本身报的错，塞进 `DbError` 会把「钩子没放行」
/// 和「引擎连不上」混成一件事（对用户是两种完全不同的处置）。
struct ConnectAttempt {
    /// 「建连前」钩子的报告。
    pre: HookReport,
    /// 「建连后」钩子的报告（没走到那一步就是空表）。
    post: HookReport,
    /// 会话（成功时 `Some`）。
    session: Option<Session>,
    /// 失败原因（成功时 `None`）；带上「断在哪一层」。
    error: Option<StageError>,
}

/// 主区展示模式：SQL 结果 / 表浏览 / 实例信息页 / 用户仪表板。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    /// SQL 执行结果。
    #[default]
    Query,
    /// 表浏览（分页）。
    Table,
    /// 实例信息页（左栏连接节点下那四个入口，DBFlux 同形）。
    Instance(InstanceTab),
    /// 用户仪表板（12 栏网格；带的是仪表板 id，见 [`Dashboard::id`]）。
    Dashboard(u32),
}

/// 实例信息页的四档 —— 顺序即左栏入口顺序。
///
/// 与 [`Mode::Table`] 平级而不是塞进「数据库」那棵树里：这四个页面讲的是**服务器**，
/// 而库 / 表讲的是**数据**（DBFlux 也是这么分的：前两项与后三项在树里同层）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstanceTab {
    /// 仪表盘：这条连接当前的对象构成 + 最近跑过的语句。
    Dashboard,
    /// 实例概览：版本 / 在线时长 / 体量 / 字符集这类服务器级事实。
    Overview,
    /// 实例指标：连接数、事务量、缓冲池命中率这类数字。
    Metrics,
    /// 实例检查器：服务端会话 / 连接现状。
    Inspector,
}

impl InstanceTab {
    /// 四档全量（左栏渲染顺序）。
    const ALL: [Self; 4] = [
        Self::Dashboard,
        Self::Overview,
        Self::Metrics,
        Self::Inspector,
    ];

    /// 左栏入口文案（`tr` 回 `&'static str`，与全站取词一致）。
    fn heading(self, cx: &App) -> &'static str {
        match self {
            Self::Dashboard => tr(cx, "仪表盘"),
            Self::Overview => tr(cx, "实例概览"),
            Self::Metrics => tr(cx, "实例指标"),
            Self::Inspector => tr(cx, "实例检查器"),
        }
    }

    /// 元素 id / 展开键用的稳定英文键（**不翻译**，见 [`TreeCategory::key`] 同一理由）。
    const fn key(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Overview => "overview",
            Self::Metrics => "metrics",
            Self::Inspector => "inspector",
        }
    }

    /// 需要读服务器数据的那几档（仪表盘只用手上已有的数据，不取）。
    const fn fetches_server(self) -> bool {
        !matches!(self, Self::Dashboard)
    }
}

/// 实例信息页入口的图标（左栏与页面标题共用，两处不会画成两个样子）。
fn instance_tab_icon(tab: InstanceTab) -> DuckIcon {
    match tab {
        InstanceTab::Dashboard => DuckIcon::Gauge,
        InstanceTab::Overview => DuckIcon::Server,
        InstanceTab::Metrics => DuckIcon::DatabaseZap,
        InstanceTab::Inspector => DuckIcon::ListOrdered,
    }
}

/// 一条**已保存查询**（kv 单键里的一项）。
///
/// `id` 与 `name` 分开：名字是给人看的、可以重名、可以改；定位（改名 / 删除 / 元素 id）
/// 一律走 `id`，否则改个名字就会丢掉「正在编辑的是哪一条」这层关系。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedQuery {
    /// 稳定 id（`saved-<递增序号>`，生成时保证不与现有项重复）。
    id: String,
    /// 显示名（用户可改）。
    name: String,
    /// SQL 正文（载入编辑器用）。
    sql: String,
}

impl SavedQuery {
    /// 生成一个新 id：按现有条目数递增到不撞车为止（不引入随机数依赖）。
    fn next_id(existing: &[Self]) -> String {
        let mut seq = existing.len() + 1;
        loop {
            let candidate = format!("saved-{seq}");
            if !existing.iter().any(|saved| saved.id == candidate) {
                return candidate;
            }
            seq += 1;
        }
    }
}

/// 图表类型（对齐 DBFlux 的四档：折线 / 柱状 / 面积 / 饼图）。
///
/// 四档都由 `gpui-component` 的样式化图表画（`chart::{LineChart, BarChart, AreaChart,
/// PieChart}`），所以这里只是一个「选哪一种」的标签，不携带任何绘制参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum ChartKind {
    /// 折线（看趋势）。
    #[default]
    Line,
    /// 柱状（比大小）。
    Bar,
    /// 面积（折线 + 填充，看体量）。
    Area,
    /// 饼图（看占比；只吃一个数值列）。
    Pie,
}

impl ChartKind {
    /// 四档全量（菜单顺序）。
    const ALL: [Self; 4] = [Self::Line, Self::Bar, Self::Area, Self::Pie];

    /// 展示文案。
    fn heading(self, cx: &App) -> &'static str {
        match self {
            Self::Line => tr(cx, "折线图"),
            Self::Bar => tr(cx, "柱状图"),
            Self::Area => tr(cx, "面积图"),
            Self::Pie => tr(cx, "饼图"),
        }
    }

    /// 下拉选项 / 持久化用的稳定英文键（**不翻译**，同 [`InstanceTab::key`] 理由）。
    const fn key(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Bar => "bar",
            Self::Area => "area",
            Self::Pie => "pie",
        }
    }

    /// 从稳定键还原（下拉回投用；认不出时给 `None`，不猜）。
    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// 同时能画几条数值序列。
    ///
    /// 饼图只吃**一列**：多列拼进一张饼是「苹果 + 橘子」的和，读不出任何东西，所以
    /// 饼图档下选另一列 = 换掉这一条。其余三档封顶 [`CHART_SERIES_LIMIT`]：再多就
    /// 是一屏碎图，剩下的列留在表里（不删）。
    const fn series_limit(self) -> usize {
        match self {
            Self::Pie => 1,
            Self::Line | Self::Bar | Self::Area => CHART_SERIES_LIMIT,
        }
    }
}

/// 图表视角的配置（结果区改画图时用）。
///
/// `x` / `y` 存的是**列名**而不是列下标：结果重新跑过之后列序可能变（`SELECT *` 加了
/// 一列），按下标存会静默画错列；列名对不上时判为「配置失效」并回落默认映射。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ChartConfig {
    /// 图表类型。
    kind: ChartKind,
    /// 分类 / 横轴列名。
    x: String,
    /// 数值列名（饼图只用第一个；其余类型一列一条序列）。
    y: Vec<String>,
}

impl ChartConfig {
    /// 按结果集给一份能直接画的默认配置（`x` 是第一个非数值列，`y` 是数值列）。
    fn from_result(result: &ResultSet) -> Option<Self> {
        let (x, y) = default_chart_columns(result)?;
        Some(Self {
            kind: ChartKind::Line,
            x,
            y,
        })
    }

    /// 这份配置在当前结果集上还能不能用（列名都还在）。
    fn is_valid_for(&self, result: &ResultSet) -> bool {
        let has = |name: &str| result.columns.iter().any(|column| column.name == name);
        has(&self.x) && !self.y.is_empty() && self.y.iter().all(|name| has(name))
    }

    /// 按类型截断到能画的序列数（饼图只留一条）。
    fn effective_y(&self) -> &[String] {
        let limit = self.kind.series_limit();
        let end = self.y.len().min(limit);
        &self.y[..end]
    }
}

/// 一条**已保存图表**（kv 单键里的一项）。
///
/// 存的是「SQL + 列映射 + 图表类型」三件：重新打开时把 SQL 放回编辑器（**不执行**，
/// 与已保存查询同口径），并用同一份配置去画——这样「保存的图」= 「保存的查询」+ 一层
/// 视图配置，而不是一份会过期的数据快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedChart {
    /// 稳定 id（`chart-<递增序号>`）。
    id: String,
    /// 显示名（用户可改）。
    name: String,
    /// 产出这张图数据的 SQL。
    sql: String,
    /// 图表类型。
    kind: ChartKind,
    /// 分类 / 横轴列名。
    x: String,
    /// 数值列名。
    #[serde(default)]
    y: Vec<String>,
}

impl SavedChart {
    /// 生成一个新 id（范式同 [`SavedQuery::next_id`]）。
    fn next_id(existing: &[Self]) -> String {
        let mut seq = existing.len() + 1;
        loop {
            let candidate = format!("chart-{seq}");
            if !existing.iter().any(|saved| saved.id == candidate) {
                return candidate;
            }
            seq += 1;
        }
    }

    /// 取出视图配置（已保存图表与实时配置共用一份）。
    fn config(&self) -> ChartConfig {
        ChartConfig {
            kind: self.kind,
            x: self.x.clone(),
            y: self.y.clone(),
        }
    }
}

/// 12 栏网格的栏数（对齐 DBFlux 的仪表板网格）。
const DASHBOARD_COLUMNS: u32 = 12;

/// 一块仪表板面板默认占的栏数（半行：两张并排）。
const DASHBOARD_DEFAULT_SPAN: u32 = 6;

/// 仪表板图表面板取数时的行上限。
///
/// 图不靠行数取胜（它要的是**走势 / 对比**），几百行足够画出形状；真正的按量聚合
/// 该写在 SQL 里（`GROUP BY` / `date_trunc`），而不是拉回十万行让界面自己截。
const DASHBOARD_ROW_LIMIT: usize = 500;

/// 仪表板里一张图的高度。
const DASHBOARD_CHART_HEIGHT: f32 = 160.;

/// `span` 字段的 serde 默认值（写不出 `DASHBOARD_DEFAULT_SPAN` 作为属性参数，包一层）。
fn default_panel_span() -> u32 {
    DASHBOARD_DEFAULT_SPAN
}

/// 一条**用户仪表板**：把已保存图表 / 分隔标题 / 实例检查器排在一张 12 栏网格上。
///
/// 与 [`InstanceTab::Dashboard`]（连接节点下那块只读的「仪表盘」）是两件事：那个讲
/// **这台服务器现在什么样**，内容由程序按引擎能力摆好；这个讲**用户自己想盯着看什么**，
/// 面板由用户自己增删排布。前者的数据只用手上已有的，后者会真的去跑面板里的 SQL。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Dashboard {
    /// 稳定 id。
    ///
    /// 用递增整数而不是 [`SavedQuery`] / [`SavedChart`] 那样的字符串：它只在本机这一份
    /// kv 里唯一、也不参与任何跨设备引用。整数让 [`Mode`] 保持 `Copy`——模式是在
    /// `&self` 里被 match 的，带一个 `String` 会让那几十处匹配全改成先克隆。
    id: u32,
    /// 显示名（用户可改）。
    name: String,
    /// 面板（顺序即 12 栏网格里从左到右、从上到下的铺放顺序）。
    #[serde(default)]
    panels: Vec<DashboardPanel>,
}

impl Dashboard {
    /// 生成一个新 id：现有最大 +1（删除后**不复用**，免得旧面板 id 撞上）。
    fn next_id(existing: &[Self]) -> u32 {
        existing.iter().map(|saved| saved.id).max().unwrap_or(0) + 1
    }

    /// 复制一份（id / 名字另起，面板原样搬过去）。
    ///
    /// 名字由调用方给定：新名字里的「副本」二字是要翻译的文案，而**内核侧不产面向用户的
    /// 散文**这条纪律在这里同样适用 —— 本结构不做 `tr`，由界面层拼好再传进来。
    fn duplicate(&self, id: u32, name: String) -> Self {
        Self {
            id,
            name,
            panels: self.panels.clone(),
        }
    }

    /// 把第 `index` 块面板与相邻的 `delta` 位换位（`±1`；越界不动）。
    ///
    /// 12 栏**流式**网格里「左 / 右 / 上 / 下」是同一件事（都只是顺序上前挪或后挪），
    /// 所以只给这一个动作，界面上也就只画两个按钮。
    fn move_panel(&mut self, index: usize, delta: i32) {
        let Some(target) = index.checked_add_signed(delta as isize) else {
            return;
        };
        if target < self.panels.len() {
            self.panels.swap(index, target);
        }
    }

    /// 下一个可用的面板 id（同样不回收已删除的号）。
    fn next_panel_id(&self) -> u32 {
        self.panels.iter().map(|panel| panel.id).max().unwrap_or(0) + 1
    }
}

/// 仪表板上的一块面板。
///
/// 宽窄用**栏数**（1..=12）而不是像素：网格是相对布局，存像素换个窗口尺寸就错位。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DashboardPanel {
    /// 面板 id（在同一块仪表板内唯一；元素 id 与折叠状态都靠它）。
    id: u32,
    /// 在 12 栏网格里占几栏（越界由 [`clamp_span`] 收口）。
    #[serde(default = "default_panel_span")]
    span: u32,
    /// 面板内容（展平进本结构，kv 里读起来是一层而不是 `kind: { kind: … }`）。
    #[serde(flatten)]
    kind: DashboardPanelKind,
}

/// 面板的三种内容 —— 与 DBFlux 的 `DashboardPanelKind` 同形（图表 / 分隔条 / 检查器）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DashboardPanelKind {
    /// 引用一条**已保存图表**（按 id）。存引用而不是快照：图表改了，仪表板跟着变；
    /// 图表被删了就留一个占位（[`PanelFetch::MissingChart`]），不静默少一块。
    Chart { saved_chart_id: String },
    /// 分隔标题：视觉上把面板分组，点一下折起它统领的那一段。
    Divider { title: String },
    /// 实例检查器：直接摆一张服务器事实表（指标 / 会话）。
    Inspector { view: InspectorView },
}

/// 仪表板「实例检查器」面板看哪张表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InspectorView {
    /// 实例指标（连接数 / 事务量…）。
    Metrics,
    /// 服务端会话 / 连接现状。
    Processes,
}

impl InspectorView {
    /// 两档全量（下拉顺序）。
    const ALL: [Self; 2] = [Self::Metrics, Self::Processes];

    /// 下拉选项 / 持久化用的稳定英文键（**不翻译**，同 [`InstanceTab::key`] 理由）。
    const fn key(self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::Processes => "processes",
        }
    }

    /// 从稳定键还原（下拉回投用；认不出给 `None`，不猜）。
    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|view| view.key() == key)
    }

    /// 展示文案。
    fn heading(self, cx: &App) -> &'static str {
        match self {
            Self::Metrics => tr(cx, "实例指标"),
            Self::Processes => tr(cx, "实例检查器"),
        }
    }

    /// 对应的内核取数档位。
    const fn view(self) -> InstanceView {
        match self {
            Self::Metrics => InstanceView::Metrics,
            Self::Processes => InstanceView::Processes,
        }
    }
}

/// 一块仪表板面板取数的结果。
///
/// 分开表达「引用的图表没了」（配置问题）/「语句没有结果集」（句子问题）/「跑失败」
/// （服务器问题）：三种原因该说的话完全不同，混成一个 `Err(String)` 就只剩一句
/// 「失败了」。
enum PanelFetch {
    /// 图表面板：跑通并拿到结果集。
    Chart(ResultSet),
    /// 检查器面板：拿到一张通用表。
    Table(InstanceTable),
    /// 引用的已保存图表已被删除（面板留占位，不消失）。
    MissingChart,
    /// 语句跑通但没有结果集（写语句 / 空结果）。
    NoRows,
    /// 执行 / 读取失败（文案是内核给的英文错误）。
    Failed(String),
}

/// 后台要替仪表板面板做的活（面板 id → 做什么）。
enum DashboardJob {
    /// 跑一条 SQL（图表面板）。
    Sql(String),
    /// 取一张实例检查器表。
    Inspector(InstanceView),
}

/// 12 栏网格的**流式打包**：按面板顺序从左往右铺，铺不下就换行。
///
/// 返回「每行有哪些面板下标」。用流式而不是自由 `(x, y)` 定位：自由定位要么额外做一套
/// 碰撞 / 吸附规则，要么允许重叠；而本页的编辑动作只有「宽窄 / 前移后移」两样，
/// 流式网格下这两样都是确定行为（不会出现两块面板叠在一起这种"看上去排好了"的坏状态）。
fn pack_dashboard_rows(spans: &[u32]) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut row: Vec<usize> = Vec::new();
    let mut used = 0_u32;
    for (index, raw) in spans.iter().copied().enumerate() {
        let span = clamp_span(raw);
        if !row.is_empty() && used + span > DASHBOARD_COLUMNS {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(index);
        used += span;
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// 跨度收口到 1..=12 栏（kv 是外部可改的，越界值在这里拦掉而不是让布局算出负数宽度）。
fn clamp_span(span: u32) -> u32 {
    span.clamp(1, DASHBOARD_COLUMNS)
}

/// 面板在网格里**实际**占的栏数。
///
/// 分隔标题一律整行：它是给面板分组用的横条，半行宽就失去「这一段属于它」的意思。
/// 布局与宽度都走这一个函数，不会出现「存的是 6 栏、画的是 12 栏」这种两处不一致。
fn dashboard_panel_span(panel: &DashboardPanel) -> u32 {
    match panel.kind {
        DashboardPanelKind::Divider { .. } => DASHBOARD_COLUMNS,
        _ => clamp_span(panel.span),
    }
}

/// 一栏宽度占整行的比例（`span / 12`），给相对宽度用。
fn column_fraction(span: u32) -> f32 {
    clamp_span(span) as f32 / DASHBOARD_COLUMNS as f32
}

/// 哪些面板当前可见：被折叠的分隔条把它**下面直到下一个分隔条**的面板挡掉。
///
/// 与 DBFlux 同口径。分隔条自己始终可见 —— 否则折起来之后就没东西可以点开了。
fn visible_dashboard_panels(panels: &[DashboardPanel], collapsed: &BTreeSet<u32>) -> Vec<usize> {
    let mut visible = Vec::new();
    let mut hidden = false;
    for (index, panel) in panels.iter().enumerate() {
        if matches!(panel.kind, DashboardPanelKind::Divider { .. }) {
            // 每个分隔条只掌自己那一段：见到它就按它自己的折叠态重设。
            hidden = collapsed.contains(&panel.id);
            visible.push(index);
            continue;
        }
        if !hidden {
            visible.push(index);
        }
    }
    visible
}

/// 结果集的文本格式。
///
/// **导出落盘**与**复制到剪贴板**两条路共用这一个枚举：以前导出有自己的三档枚举、
/// 剪贴板侧硬编码 TSV，于是「把结果复制成 Markdown 贴进工单」做不到，而想加一档还得
/// 在两处各改一遍。格式的正文一律由内核中性层渲染（[`ResultSet`] 的
/// `to_tsv` / `to_csv` / `to_markdown` / `to_json` / `to_insert_sql`），本枚举只负责
/// 「有哪几档」「叫什么」「要什么额外入参」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultFormat {
    /// 制表符分隔（粘进表格软件直接分列）——复制那条路的默认档。
    Tsv,
    /// 逗号分隔文本，带表头。
    Csv,
    /// 对齐的纯文本表（给人看的：终端 / 工单 / 聊天窗口）。
    Text,
    /// Markdown 管道表格（贴进文档 / 工单）。
    Markdown,
    /// JSON 数组（每行一条记录，便于 grep / diff）。
    Json,
    /// JSON 数组（缩进完整，给人读）。
    JsonPretty,
    /// 二进制转储：字节列给十六进制。
    Hex,
    /// 二进制转储：字节列给 Base64。
    Base64,
    /// 逐行一条 `INSERT`；只有 SQL 引擎给得出（MongoDB / Elasticsearch 无此概念）。
    InsertSql,
    /// **原始字节**落盘（只有「单列且全是二进制」的结果给）；复制侧没有这一档。
    Binary,
}

impl ResultFormat {
    /// 文本档全量（复制菜单按这个顺序排；导出菜单 = 它 + [`Self::Binary`]）。
    const ALL: [Self; 9] = [
        Self::Tsv,
        Self::Csv,
        Self::Text,
        Self::Markdown,
        Self::Json,
        Self::JsonPretty,
        Self::Hex,
        Self::Base64,
        Self::InsertSql,
    ];

    /// 导出菜单全量：文本档 + 原始字节。
    const EXPORT_ALL: [Self; 10] = [
        Self::Tsv,
        Self::Csv,
        Self::Text,
        Self::Markdown,
        Self::Json,
        Self::JsonPretty,
        Self::Hex,
        Self::Base64,
        Self::InsertSql,
        Self::Binary,
    ];

    /// 这一档是不是**原始字节**（落盘走字节路径、复制侧没有）。
    const fn is_binary(self) -> bool {
        matches!(self, Self::Binary)
    }

    /// 保存对话框的默认文件名。
    fn file_name(self) -> &'static str {
        match self {
            Self::Tsv => "result.tsv",
            Self::Csv => "result.csv",
            Self::Text => "result.txt",
            Self::Markdown => "result.md",
            Self::Json => "result.json",
            Self::JsonPretty => "result.pretty.json",
            Self::Hex => "result.hex",
            Self::Base64 => "result.base64",
            Self::InsertSql => "result.sql",
            Self::Binary => "result.bin",
        }
    }

    /// 导出成功后的通知文案。
    fn done_note(self, cx: &App) -> SharedString {
        match self {
            Self::Tsv => tr(cx, "已导出 TSV。").into(),
            Self::Csv => tr(cx, "已导出 CSV。").into(),
            Self::Text => tr(cx, "已导出纯文本。").into(),
            Self::Markdown => tr(cx, "已导出 Markdown。").into(),
            Self::Json => tr(cx, "已导出 JSON。").into(),
            Self::JsonPretty => tr(cx, "已导出美化 JSON。").into(),
            Self::Hex => tr(cx, "已导出十六进制转储。").into(),
            Self::Base64 => tr(cx, "已导出 Base64 转储。").into(),
            Self::InsertSql => tr(cx, "已导出 INSERT 语句。").into(),
            Self::Binary => tr(cx, "已导出原始字节。").into(),
        }
    }

    /// 右键菜单里「复制为 …」那一档的文案。
    ///
    /// 复制与导出的**动词不同**（复制 vs 导出），所以文案各留一份，不拿枚举名拼。
    fn copy_label(self, cx: &App) -> &'static str {
        match self {
            Self::Tsv => tr(cx, "复制为 TSV（含表头）"),
            Self::Csv => tr(cx, "复制为 CSV"),
            Self::Text => tr(cx, "复制为纯文本（对齐）"),
            Self::Markdown => tr(cx, "复制为 Markdown 表格"),
            Self::Json => tr(cx, "复制为 JSON"),
            Self::JsonPretty => tr(cx, "复制为 JSON（美化）"),
            Self::Hex => tr(cx, "复制为十六进制转储"),
            Self::Base64 => tr(cx, "复制为 Base64 转储"),
            Self::InsertSql => tr(cx, "复制为 INSERT 语句"),
            // 原始字节不进剪贴板（是文件载荷，不是文本）。
            Self::Binary => tr(cx, "导出原始字节"),
        }
    }

    /// 导出菜单里那一档的文案。
    ///
    /// 与 [`Self::copy_label`] 分开是刻意的（菜单动词不同：导出 vs 复制），但**档位清单
    /// 只有一个**（[`Self::EXPORT_ALL`] / [`Self::ALL`]）：以前菜单用 `zip` 配一份手写
    /// 文案数组，加一档就整体错位——文案跟着枚举走，错位无从发生。
    fn export_label(self, cx: &App) -> &'static str {
        match self {
            Self::Tsv => tr(cx, "导出 TSV"),
            Self::Csv => tr(cx, "导出 CSV"),
            Self::Text => tr(cx, "导出纯文本"),
            Self::Markdown => tr(cx, "导出 Markdown"),
            Self::Json => tr(cx, "导出 JSON"),
            Self::JsonPretty => tr(cx, "导出 JSON（美化）"),
            Self::Hex => tr(cx, "导出十六进制转储"),
            Self::Base64 => tr(cx, "导出 Base64 转储"),
            Self::InsertSql => tr(cx, "导出 INSERT 语句"),
            Self::Binary => tr(cx, "导出原始字节"),
        }
    }

    /// 这一档此刻能不能用（导出菜单据此置灰）。
    ///
    /// 三条前置：有结果集；`INSERT` 要引擎给得出引号规则；原始字节要结果真的是单列二进制
    /// ——判据与 [`Self::render_bytes`] 同源（都是 `ResultSet::raw_bytes`），不会出现
    /// 「菜单亮着、点了说不行」。
    fn exportable(self, has_result: bool, can_insert: bool, has_raw: bool) -> bool {
        if !has_result {
            return false;
        }
        match self {
            Self::InsertSql => can_insert,
            Self::Binary => has_raw,
            _ => true,
        }
    }

    /// 渲染正文；`InsertSql` 需要表引用与引号字符（都是引擎相关的，由调用方给），
    /// 给不出时返回 `None` —— 「没有这一档」这件事由调用方按场景各自讲（导出侧报错、
    /// 复制侧直接把菜单项禁掉）。[`Self::Binary`] 不是文本档，永远返回 `None`
    /// （它的载荷走 [`Self::render_bytes`]）。
    fn render(self, result: &ResultSet, insert: Option<(&str, char)>) -> Option<String> {
        match self {
            Self::Tsv => Some(result.to_tsv()),
            Self::Csv => Some(result.to_csv()),
            Self::Text => Some(result.to_text()),
            Self::Markdown => Some(result.to_markdown()),
            Self::Json => Some(result.to_json()),
            Self::JsonPretty => Some(result.to_json_pretty()),
            Self::Hex => Some(result.to_binary_dump(BinaryEncoding::Hex)),
            Self::Base64 => Some(result.to_binary_dump(BinaryEncoding::Base64)),
            Self::InsertSql => insert.map(|(table, quote)| result.to_insert_sql(table, quote)),
            Self::Binary => None,
        }
    }

    /// [`Self::Binary`] 的载荷：单列二进制结果的原始字节（其余档返回 `None`）。
    fn render_bytes(self, result: &ResultSet) -> Option<Vec<u8>> {
        match self {
            Self::Binary => result.raw_bytes(),
            _ => None,
        }
    }
}

/// 一条已建立的会话：阻塞连接包在互斥锁里，由后台线程短暂持有。
struct Session {
    conn: Arc<Mutex<Connection>>,
    /// 服务器版本描述（连接时取一次）。
    version: String,
    /// 当前命名空间的目录（表 / 视图 / 存储过程 / 函数 / 序列；连接时与切 schema 时取）。
    catalog: Vec<CatalogTable>,
    /// 当前命名空间里的触发器（名字 + 挂在哪张表上；引擎没有这个概念就是空清单）。
    triggers: Vec<DatabaseObject>,
    /// 当前命名空间里的事件（只有 MySQL 有独立事件调度器，其余恒空）。
    events: Vec<DatabaseObject>,
    /// 服务器级的用户与登录（MySQL / PG / MSSQL 有；SQLite 无用户体系恒空）。
    users: Vec<DatabaseObject>,
    /// 可选的命名空间清单（连接时取一次）。
    schemas: Vec<String>,
    /// 这条连接上有**哪些库**（连接时取一次；Elasticsearch 恒空）。
    ///
    /// 与 [`Self::schemas`] 是两层东西：MySQL / ClickHouse / Mongo / SQLite 里
    /// 「库」就是 schema 那一层（两份清单内容基本相同），而 PG / MSSQL 的
    /// `schemas` 是**库内**的 schema（连接时若没选库，服务端给不出，清单也往往是空的）。
    /// 「连上了但什么都看不到」正是缺了这一层：用户最先想问的是「这台服务器上有哪些
    /// 库」，而不是「当前库里有几张表」。
    databases: Vec<String>,
    /// 当前**绑定在哪个库**上（`None` = 没选定，语句会被服务端回 `No database selected`）。
    ///
    /// 单独一个字段而不是复用 `schema`：PG / MSSQL 的 `schema` 是库内的下一层，
    /// 两层混用一个变量就没法在界面上把「库」和「schema」分开标。
    database: Option<String>,
    /// 当前浏览的命名空间（`None` = 引擎的当前命名空间，即 `catalog` 里那份的来处）。
    schema: Option<String>,
    /// 这条连接是什么引擎。
    ///
    /// 界面必须知道它才能回答「命名空间下拉里那一项是**库**还是**库内的 schema**」——
    /// MySQL / ClickHouse / Mongo / SQLite 里两者同一层，PG / MSSQL 里是两层。
    /// 认错一层，「选中项」就会被当成库名去重连（PG），或者当成 schema 去内省（MySQL），
    /// 两种都表现为「点了没反应」。
    kind: DatabaseKind,
    /// 传输通道句柄（有则连接经 `127.0.0.1:{local_port}`；断开随会话释放）。
    transport: Option<Transport>,
    /// 是否处于**显式事务**（用户敲过 BEGIN / START TRANSACTION，还没 COMMIT / ROLLBACK）。
    ///
    /// 判据只看用户发过来的语句文本（[`tx_effect`]）：单条执行路径下连接是常驻的，
    /// 语句之间的事务状态由服务器维持，这里只是把它**显示**出来。
    in_tx: bool,
}

/// 这个引擎的「命名空间」是不是**就是库本身**？
///
/// 是 → 界面上只有一层（下拉 / 库列表里的每一项都是库，选中即切库）；
/// 否 → PG / MSSQL：库是外层、schema 是库内一层，两者必须分开呈现。
///
/// Elasticsearch 也归「是」：它没有库这一层，`list_databases` / `list_schemas` 都恒空，
/// 归哪边都不影响（界面上不会出现这一组控件）。
fn namespace_is_database(kind: DatabaseKind) -> bool {
    !matches!(kind, DatabaseKind::Postgres | DatabaseKind::SqlServer)
}

/// 「库」清单优先取 [`Session::databases`]；它空（老引擎 / 权限不足 / ES）时退回
/// [`Session::schemas`]。
///
/// 退回而不是返回空：`databases` 拿不到不代表没得选——部分服务端（受限账号、
/// 只读副本）只肯暴露当前库，这时 `schemas` 里的那点信息仍是可用的导航。
fn collect_namespaces(databases: &[String], schemas: &[String]) -> Vec<String> {
    if databases.is_empty() {
        schemas.to_vec()
    } else {
        databases.to_vec()
    }
}

/// 一次内省取回的全部结果（建连 / 刷新 / 切命名空间三处共用同一套取法）。
///
/// 为什么包一个结构：三处都要取「命名空间清单 + 目录 + 触发器 / 事件 / 用户」这六份，
/// 每处各自写一遍元组解构，加一份新内省就要追三处（类型、构造、回投）。
///
/// **失败口径**：只有目录（`catalog`）失败会让整次内省失败——它是界面主体；
/// 其余五份全部 `.unwrap_or_default()`：权限不足的账号取不到 `mysql.user`、
/// 老引擎没有 `list_databases`，都不该把整条连接判死（左栏少一个分类节点而已）。
#[derive(Default)]
struct Introspection {
    schemas: Vec<String>,
    databases: Vec<String>,
    catalog: Vec<CatalogTable>,
    triggers: Vec<DatabaseObject>,
    events: Vec<DatabaseObject>,
    users: Vec<DatabaseObject>,
}

impl Introspection {
    /// 在**已加锁**的连接上按命名空间取齐六份内省结果。
    fn load(conn: &mut Connection, schema: Option<&str>) -> Result<Self, DbError> {
        Ok(Self {
            // 命名空间清单也跟着重取（刷新要能看见别人新建的库）。
            schemas: conn.list_schemas().unwrap_or_default(),
            databases: conn.list_databases().unwrap_or_default(),
            catalog: conn.catalog(schema)?,
            triggers: conn.list_triggers(schema).unwrap_or_default(),
            events: conn.list_events(schema).unwrap_or_default(),
            users: conn.list_users().unwrap_or_default(),
        })
    }

    /// 把结果写回会话（三处回投共用）。
    fn install(self, session: &mut Session) {
        session.schemas = self.schemas;
        session.databases = self.databases;
        session.catalog = self.catalog;
        session.triggers = self.triggers;
        session.events = self.events;
        session.users = self.users;
    }
}

/// 左栏树的一个分类节点（表 / 视图 / 存储过程 / 函数 / 触发器 / 事件 / 用户与权限）。
///
/// 顺序即展示顺序 —— 与 DBX / DBFlux 的左栏一致（表在最前，权限类在最后）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeCategory {
    Table,
    View,
    Procedure,
    Function,
    Trigger,
    Event,
    User,
}

impl TreeCategory {
    /// 全部分类，按展示顺序。
    const ALL: [Self; 7] = [
        Self::Table,
        Self::View,
        Self::Procedure,
        Self::Function,
        Self::Trigger,
        Self::Event,
        Self::User,
    ];

    /// 展开状态与元素 id 的稳定键。**不翻译**：它是状态键（存进 `tree_open` 的那一半），
    /// 翻译过的文案换语言就换键，展开状态会跟着丢。
    const fn key(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::Procedure => "procedure",
            Self::Function => "function",
            Self::Trigger => "trigger",
            Self::Event => "event",
            Self::User => "user",
        }
    }

    /// 分类标题。
    fn heading(self, cx: &App) -> String {
        match self {
            Self::Table => tr(cx, "表"),
            Self::View => tr(cx, "视图"),
            Self::Procedure => tr(cx, "存储过程"),
            Self::Function => tr(cx, "函数"),
            Self::Trigger => tr(cx, "触发器"),
            Self::Event => tr(cx, "事件"),
            Self::User => tr(cx, "用户与权限"),
        }
        .to_string()
    }

    /// 分类图标：触发器取闪电、事件取时钟（与 DBX 的取意一致），用户取人形。
    const fn icon(self) -> DuckIcon {
        match self {
            Self::Table => DuckIcon::Columns2,
            Self::View => DuckIcon::Layers,
            Self::Procedure => DuckIcon::SquareTerminal,
            Self::Function => DuckIcon::SquareFunction,
            Self::Trigger => DuckIcon::Zap,
            Self::Event => DuckIcon::Clock,
            Self::User => DuckIcon::User,
        }
    }

    /// 目录条目里的哪一类归到这个分类。
    const fn matches(self, kind: TableKind) -> bool {
        matches!(
            (self, kind),
            (Self::Table, TableKind::Table)
                | (Self::View, TableKind::View)
                | (Self::Procedure, TableKind::Procedure)
                | (Self::Function, TableKind::Function)
        )
    }
}

impl Session {
    /// 某个分类对应的**库级对象**清单；目录类分类（表 / 视图 / 过程 / 函数）给空。
    fn database_objects(&self, category: TreeCategory) -> &[DatabaseObject] {
        match category {
            TreeCategory::Trigger => &self.triggers,
            TreeCategory::Event => &self.events,
            TreeCategory::User => &self.users,
            TreeCategory::Table
            | TreeCategory::View
            | TreeCategory::Procedure
            | TreeCategory::Function => &[],
        }
    }
}

/// 按连接 id 取到的**只读会话句柄**（`docs/ai-design.md` §10.4 里 `run_sql_readonly` 的落点）。
///
/// 它不是"又一个连接"：内部就是页面那条会话的 `Arc<Mutex<Connection>>` 克隆 ——
/// 同一个连接、同一个事务状态、同一把取消句柄，**只是入口换成只读闸门**
/// （[`duck_db::Connection::execute_read_only`]）。
///
/// 为什么句柄落在页面这一层而不是 `duck-db` 内核：内核里没有"会话"这个概念（只有
/// [`Connection`] 本身），连接的生命周期（建 / 断 / 隧道 / 切命名空间）全由页面按标签
/// 管理 —— 内核给**能力**（只读执行），页面给**寻址**（按 id 找会话）。把会话表塞进内核
/// 只会让内核多出一份与界面生命周期并行的状态，两边迟早对不上。
#[derive(Clone)]
pub(crate) struct ReadOnlySession {
    /// 会话 id（就是连接档案的 id，与标签一一对应）。
    id: String,
    /// 与会话共享的连接句柄。
    conn: Arc<Mutex<Connection>>,
}

impl ReadOnlySession {
    /// 会话 id。
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// 跑一段**只读** SQL；写语句 / 证明不了只读的语句**整体拒绝**（一条都不执行）。
    ///
    /// 阻塞调用：调用方负责把它丢进后台执行器（与页面其它数据库调用同一条纪律）。
    /// 只读是内核保证的，不靠调用方自觉——拿到这个句柄也跑不了写语句。
    pub(crate) fn execute(&self, sql: &str, max_rows: usize) -> Result<ExecOutcome, DbError> {
        let mut guard = self.conn.lock().expect("连接锁不应中毒");
        guard.execute_read_only(sql, max_rows)
    }
}

/// 一条连接标签的编辑区快照：SQL 编辑器文本 + 结果区状态。
///
/// 「每个连接标签各自保留 SQL 与结果集」这条要求落在这里：编辑器实体仍是页面级单实例
/// （GPUI 实体不便宜，也没必要每个标签建一个），但**内容与结果区按标签留存**。
///
/// ⚠️ 它**不放进 [`Session`]**：`Session` 要跨线程送进 `background_executor`
/// （`spawn_db` 的 `T: Send`），而快照里有 gpui 实体（行编辑器的输入框），不是 `Send`。
/// 所以按标签存在页面侧的 `sessions_views` 里，连接本身仍只带 `Send` 的东西。
///
/// ⚠️ 编辑器实体是**页面级共享**的，所以撤销栈也共享：切标签会换掉正文，但 `⌘Z`
/// 有可能退回上一个标签的文本。要彻底分家就得每标签一个 `EditorState`，代价是每个
/// 连接都常驻一个编辑器实体——当前按「内容独立」这一条实现，撤销栈的边界如实写在这里。
/// 标签身份：一个标签「是谁」——决定标签条上的标题，以及「同表复用」怎么匹配。
///
/// 连接级查询标签之外，左栏点表 / 视图会开**数据标签**、点数据库节点会开**库标签**
/// （对齐 dbx 的 dataTabOpenPolicy：同对象复用并激活，⌥ 点击强制新开；「永远新开 /
/// 活动标签复用」两档不进口径）。身份在打开时定死、关闭时随标签移除，见
/// [`DatabasePage::tab_kind`]。
#[derive(Debug, Clone, PartialEq, Eq)]
enum TabKind {
    /// 连接级查询标签（工具条「＋」与 [`DatabasePage::activate_conn`] 造的都是这种）。
    Query,
    /// 库标签：一个库（schema 层引擎里就是 schema）一个，标题 = 库名。
    Database { name: String },
    /// 数据标签：一张表 / 一个视图一个，标题 = `表名@库`（无 schema 时裸名）。
    Data { object: TableRef },
}

/// 「同对象复用」判定：在已开标签里找**同连接 + 同身份**的第一个。
///
/// 纯函数（输入全是借用）：复用策略是界面行为里最容易写歪的部分，拆出来才好单测。
fn find_reusable_tab(
    tabs: &[String],
    kinds: &HashMap<String, TabKind>,
    owners: &HashMap<String, String>,
    conn_id: &str,
    target: &TabKind,
) -> Option<String> {
    tabs.iter()
        .find(|tab| {
            owners.get(*tab).is_some_and(|owner| owner == conn_id)
                && kinds.get(*tab).is_some_and(|kind| kind == target)
        })
        .cloned()
}

#[derive(Default)]
struct SessionEditorState {
    /// 编辑器文本。
    text: String,
    /// 结果区状态。
    view: SessionView,
}

/// 结果区状态（[`SessionEditorState`] 的后半截）。
///
/// 不进快照的是那些**本就跨连接共享**的东西：查询历史、AI 面板与其输入、
/// 单次超时覆盖、左侧连接列表与右栏对象树（它们的数据源是 Session 自身）。
#[derive(Default)]
struct SessionView {
    /// 主区模式（SQL 结果 / 表浏览 / 实例信息页）。
    mode: Mode,
    /// 实例信息页取回的数据（按标签存：切回来不用重取）。
    instance_data: InstanceData,
    /// 当前浏览的表。
    selected_table: Option<TableRef>,
    /// 表浏览页码。
    table_page: usize,
    /// 表页数据。
    table_rows: Option<ResultSet>,
    /// 表页排序。
    table_sort: Option<SortSpec>,
    /// 表页过滤条件（**服务器侧** WHERE；空 = 不过滤）。
    ///
    /// 与 `table_sort` 同为「按表页生效的查询参数」，所以同样按标签存：切回来还在。
    /// 值一律走驱动的绑定参数（见 [`duck_db::TableFilter`]），界面只负责攒条件。
    table_filters: Vec<TableFilter>,
    /// 当前表的主键列。
    table_pk: Vec<String>,
    /// 当前表的 DDL。
    table_ddl: Option<String>,
    /// 当前表的索引。
    table_indexes: Vec<TableIndex>,
    /// 当前表的行数（含口径）。
    table_row_count: Option<RowCount>,
    /// 当前表的列（右栏「列」卡；换表时取一次）。
    table_columns: Vec<Column>,
    /// 当前表的外键（右栏「外键」卡）。
    table_foreign_keys: Vec<TableForeignKey>,
    /// 当前表的触发器（右栏「触发器」卡）。
    table_triggers: Vec<TableTrigger>,
    /// 表级元信息是为哪张表取的。
    table_meta_for: Option<TableRef>,
    /// SQL 结果展示态。
    query_result: Option<QueryView>,
    /// SQL 结果可编辑时的目标表。
    query_target: Option<TableRef>,
    /// 目标表的主键列。
    query_pk: Vec<String>,
    /// 产出当前结果的 SQL。
    query_sql: Option<String>,
    /// 产出当前结果那一次执行的耗时（毫秒；结果区 footer 常驻展示）。
    query_elapsed_ms: Option<u128>,
    /// 当前结果**已加载**的行数（「加载更多」的下一页 `offset`）。
    query_loaded: u64,
    /// 当前结果之后是否还有更多行（决定「加载更多」入口是否出现）。
    query_has_more: bool,
    /// 执行计划态（结果区改显示计划树）；`None` = 不是在看计划。
    plan_view: Option<PlanView>,
    /// 当前结果的列宽。
    column_widths: Vec<f32>,
    /// 选中行。
    selected_row: Option<usize>,
    /// 勾选的行。
    checked_rows: BTreeSet<usize>,
    /// 批量编辑的目标列。
    batch_column: Option<String>,
    /// 暂存的改格。
    pending_edits: Vec<CellEdit>,
    /// 暂存的新增行。
    pending_inserts: Vec<InsertRow>,
    /// 暂存区的**逐步撤销栈**（一步 = 一次「保存」动作；详见 [`PendingSnapshot`]）。
    ///
    /// 与暂存区同生共死：提交 / 换表 / 重跑 / 换库把它一起清掉（见 `clear_pending`），
    /// 切标签随 [`SessionView`] 一起走 —— 撤销栈绝不跨表、跨标签。
    pending_undo: Vec<PendingSnapshot>,
    /// 重做栈（撤销时把「撤销前的状态」压进来；`⇧⌘Z` 反向走一遍）。
    pending_redo: Vec<PendingSnapshot>,
    /// 行编辑器。
    row_editor: Option<RowEditor>,
    /// 结果区的**图表视角**配置；`Some` = 结果区改画图（对齐 DBFlux：同一份结果可在
    /// 表格与图表之间切换）。与 `plan_view` 分开：那是「换一种方式看执行计划」，
    /// 这是「换一种方式看结果数据」，两者不是一回事，也都可能同时存在（计划态优先）。
    chart_view: Option<ChartConfig>,
}

/// 表级元信息包（换表 / 刷新时一次取齐，见 [`SessionView::table_meta_for`]）。
///
/// 五个槽位互相独立：哪个内省失败都不拖垮表页加载——索引 / 列 / 外键 / 触发器
/// 落空清单、行数落 `None`，而不是让整次读取报错。
#[derive(Default)]
struct TableMeta {
    indexes: Vec<TableIndex>,
    row_count: Option<RowCount>,
    columns: Vec<Column>,
    foreign_keys: Vec<TableForeignKey>,
    triggers: Vec<TableTrigger>,
}

/// 实例信息页取回的数据（三档各自独立）。
///
/// 分开存而不是一个大包：三档是**分别点开**的，某一档在权限不足的账号上取不到
/// （PG 的 `pg_stat_activity` 对普通账号可见但 `pg_stat_database` 的部分列可能受限），
/// 不该连带把已经取到的另一档擦掉。`None` = 还没取过。
#[derive(Default)]
struct InstanceData {
    /// 实例概览（`InstanceTab::Overview`）。
    overview: Option<InstanceOverview>,
    /// 实例指标（`InstanceTab::Metrics`）。
    metrics: Option<InstanceTable>,
    /// 实例检查器（`InstanceTab::Inspector`）。
    processes: Option<InstanceTable>,
}

/// 后台取实例数据时**按档位**回投的载荷。
///
/// 用一个枚举而不是 `Option<T>` 的三元组：后台闭包里只能有一个返回类型，而三档各自
/// 是不同结构；回投时按变体落位，也就顺手把「哪一档回来了」这件事写死在了类型里。
enum InstancePayload {
    /// 实例概览。
    Overview(InstanceOverview),
    /// 实例指标：明细表 + 数值序列（序列由表的同一份白名单筛出来，见
    /// [`InstanceTable::numeric`]）。
    Metrics(InstanceTable, Vec<InstanceMetric>),
    /// 实例检查器。
    Processes(InstanceTable),
}

/// 指标时序保留的采样数。
///
/// 与 DBFlux 同口径：**只在本次会话内累积**（不落盘、不回填历史）——服务器指标是
/// 瞬时值，补不出来；界面该讲清楚这一点（写「最近 N 次采样」而不是「最近一小时」）。
const METRIC_SAMPLES: usize = 120;

/// 一条连接的指标时序：最近 [`METRIC_SAMPLES`] 次采样的数值快照。
///
/// 为什么不只存「一个指标一条折线」：采样的单位是**一次服务器往返**（一次刷新取回
/// 全部指标），存成快照才能保证同一时刻的各指标对得上；画哪条线由界面选。
#[derive(Default)]
struct MetricHistory {
    /// `(采样时刻 epoch 秒, 那一轮的全部数值指标)`，最旧在前。
    samples: std::collections::VecDeque<(i64, Vec<InstanceMetric>)>,
}

impl MetricHistory {
    /// 记一次采样（超过上限丢最旧的）。
    fn push(&mut self, at: i64, metrics: Vec<InstanceMetric>) {
        self.samples.push_back((at, metrics));
        while self.samples.len() > METRIC_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// 采了几次。
    fn len(&self) -> usize {
        self.samples.len()
    }

    /// 可画的指标名（按**最近一次**采样的顺序去重）。
    ///
    /// 用最新一轮而不是「所有轮次的并集」：指标清单本身会随引擎版本变（白名单改了），
    /// 并集会把早就不报的项一直挂在图上。
    fn keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some((_, latest)) = self.samples.back() {
            for metric in latest {
                if !keys.contains(&metric.key) {
                    keys.push(metric.key.clone());
                }
            }
        }
        keys
    }

    /// 某个指标的时间序列（`(采样时刻, 值)`）。
    ///
    /// 某一轮没报这个指标就**跳过**那一轮，不补 0：补 0 会在图上画出一段实际不存在的
    /// 「跌到底」，比断开更难解释。
    fn series(&self, key: &str) -> Vec<(i64, f64)> {
        self.samples
            .iter()
            .filter_map(|(at, metrics)| {
                metrics
                    .iter()
                    .find(|metric| metric.key == key)
                    .map(|metric| (*at, metric.value))
            })
            .collect()
    }

    /// 缓冲覆盖的时长（秒；不足两次采样为 0）。
    fn span_secs(&self) -> i64 {
        match (self.samples.front(), self.samples.back()) {
            (Some((first, _)), Some((last, _))) => (last - first).max(0),
            _ => 0,
        }
    }
}

/// 采样时刻（epoch 秒）。时钟漂移只影响横轴间距，不影响数值。
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
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

/// 结果区的执行计划态（`EXPLAIN` 之后才出现）。
///
/// 只有**解析成功**才建这个视图：解析不出来时不进来，结果区照旧显示原始网格
/// （那就是"原样文本展示"，输出一点没丢）。所以 `nodes` 恒非空。
struct PlanView {
    /// 解析出来的计划树节点（顺序即展示顺序）。
    nodes: Vec<PlanNode>,
    /// `true` = 用户切到了「原始输出」档（此时显示网格，头条上能切回计划树）。
    raw: bool,
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

/// 撤销栈里的一格：暂存区的一份**整快照**。
///
/// 为什么是快照而不是「反向操作」：一次「保存」可能同时改十几格，其中若干格还是
/// **覆盖**同一格的前一次改动（`pending_edits` 按 (主键, 列) 去重）。要按反向操作回退，
/// 就得记住「谁被覆盖了、原先排在第几位」——那是最容易写错、也最难测的一类账。
/// 快照把这件事变成赋值：撤销 = 恢复上一份，重做 = 恢复下一份，不可能算错。
///
/// 成本可控：一份快照 = 暂存区当前大小，而暂存区是用户**手工**攒出来的
/// （几十条量级），且栈深有 [`UNDO_DEPTH`] 封顶。
#[derive(Clone, Default)]
struct PendingSnapshot {
    /// 那一刻的改格暂存。
    edits: Vec<CellEdit>,
    /// 那一刻的新增行暂存。
    inserts: Vec<InsertRow>,
}

/// 撤销栈深度上限。
///
/// 快照式撤销的空间是「栈深 × 暂存区大小」，不封顶的话，攒几千条再反复保存会把
/// 内存推到不该有的量级。64 步远超实际需要（一步 = 一次「保存」），且超出后丢的
/// 是最旧的那一步——比不封顶更可控。
const UNDO_DEPTH: usize = 64;

/// 一次执行失败留下的上下文。
///
/// 专为 AI 的 `fix` / `explain` 动作准备：prompt 里要注入的是**真实错误原文 + 原 SQL**，
/// 而不是让模型猜。通知只会消失，这份上下文保留到下一次成功执行。
struct QueryError {
    /// 失败的那条 SQL（原样）。
    sql: String,
    /// 数据库返回的错误原文（未加工）。
    message: String,
}

/// 表单里一条钩子的载体档位（界面上的「命令 / 脚本」）。
///
/// 与内核的 [`HookAction`] 不是一回事：那边是**落库形态**（展平成 tag + 字段），
/// 这边只表示「哪两个输入框是活的」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookMode {
    /// 一行命令（交给平台 shell）。
    Command,
    /// 一个脚本文件（解释器 + 路径）。
    Script,
}

impl HookMode {
    /// 稳定键（元素 id 用；不随界面语言变）。
    const fn key(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Script => "script",
        }
    }

    /// 展示文案。
    fn label(self, cx: &App) -> &'static str {
        match self {
            Self::Command => tr(cx, "命令"),
            Self::Script => tr(cx, "脚本"),
        }
    }
}

/// 表单里的一条钩子 —— 输入框与档位都是**工作副本**，点保存才写回档案。
///
/// 与 [`ConnectionHook`] 分开的理由是「编辑期」与「落库期」的形态本就不同：编辑期要持
/// [`Entity<InputState>`]（输入框是独立实体、由 gpui 自己管），那些实体不该进 kv。
struct HookRow {
    id: String,
    phase: HookPhase,
    enabled: bool,
    mode: HookMode,
    failure: HookFailure,
    /// 命令全文（命令档）。
    line: Entity<InputState>,
    /// 解释器（脚本档）。
    interpreter: Entity<InputState>,
    /// 脚本文件路径（脚本档）。
    path: Entity<InputState>,
    /// 工作目录（两档共用；留空 = 进程默认目录）。
    cwd: Entity<InputState>,
    /// 单条超时（秒；留空 = 默认 30）。
    timeout: Entity<InputState>,
}

impl HookRow {
    /// 建一行：`Some(hook)` 用既有值填，`None` 走新建默认（命令档 / 建连前 / 启用 / 不阻断）。
    fn new<T>(
        id: String,
        hook: Option<&ConnectionHook>,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let (mode, line, interpreter, path) = match hook.map(|hook| &hook.action) {
            Some(HookAction::Command { line, .. }) => (
                HookMode::Command,
                line.clone(),
                String::new(),
                String::new(),
            ),
            Some(HookAction::Script {
                interpreter, path, ..
            }) => (
                HookMode::Script,
                String::new(),
                interpreter.clone(),
                path.clone(),
            ),
            None => (
                HookMode::Command,
                String::new(),
                String::new(),
                String::new(),
            ),
        };
        let cwd = hook
            .and_then(|hook| hook.action.cwd().map(str::to_owned))
            .unwrap_or_default();
        let timeout = hook
            .and_then(|hook| hook.timeout_secs)
            .filter(|secs| *secs > 0)
            .map_or_else(String::new, |secs| secs.to_string());
        let row = Self {
            id,
            phase: hook.map_or(HookPhase::PreConnect, |hook| hook.phase),
            enabled: hook.is_none_or(|hook| hook.enabled),
            mode,
            failure: hook.map_or(HookFailure::Warn, |hook| hook.failure),
            line: text_input(window, cx, tr(cx, "例如：openvpn --config prod.ovpn")),
            interpreter: text_input(window, cx, tr(cx, "解释器，例如 bash / python3")),
            path: text_input(window, cx, tr(cx, "脚本文件的绝对路径")),
            cwd: text_input(window, cx, tr(cx, "留空用进程默认目录")),
            timeout: text_input(
                window,
                cx,
                trf(
                    cx,
                    "留空用默认 {} 秒",
                    &[&DEFAULT_HOOK_TIMEOUT_SECS.to_string()],
                ),
            ),
        };
        let seed = [
            (&row.line, line),
            (&row.interpreter, interpreter),
            (&row.path, path),
            (&row.cwd, cwd),
            (&row.timeout, timeout),
        ];
        for (target, value) in seed {
            if !value.is_empty() {
                target.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
            }
        }
        row
    }

    /// 读回成内核结构（保存时用）。
    fn to_hook(&self, cx: &App) -> ConnectionHook {
        let text = |state: &Entity<InputState>| state.read(cx).value().trim().to_owned();
        let cwd = {
            let value = text(&self.cwd);
            (!value.is_empty()).then_some(value)
        };
        let action = match self.mode {
            HookMode::Command => HookAction::Command {
                line: text(&self.line),
                cwd,
            },
            HookMode::Script => HookAction::Script {
                interpreter: text(&self.interpreter),
                path: text(&self.path),
                cwd,
            },
        };
        // 解析不出来的超时按「没填」处理：它在界面上不是必填项，卡住保存不值当。
        let timeout_secs = text(&self.timeout)
            .parse::<u64>()
            .ok()
            .filter(|secs| *secs > 0);
        ConnectionHook {
            id: self.id.clone(),
            phase: self.phase,
            enabled: self.enabled,
            action,
            failure: self.failure,
            timeout_secs,
        }
    }

    /// 启用的钩子却查不到可跑的命令（保存前拦一道，免得存下一条永远 `spawn failed` 的钩子）。
    fn is_incomplete(&self, cx: &App) -> bool {
        if !self.enabled {
            return false;
        }
        match self.mode {
            HookMode::Command => self.line.read(cx).value().trim().is_empty(),
            HookMode::Script => {
                self.interpreter.read(cx).value().trim().is_empty()
                    || self.path.read(cx).value().trim().is_empty()
            }
        }
    }
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
    /// 代理开关（SOCKS5 / HTTP CONNECT）；关掉即把 `proxy` 写回 `None`。
    proxy_enabled: bool,
    /// 代理协议。
    proxy_kind: ProxyKind,
    proxy_host: Entity<InputState>,
    proxy_port: Entity<InputState>,
    proxy_user: Entity<InputState>,
    proxy_password: Entity<InputState>,
    /// AWS SSM 开关。
    ssm_enabled: bool,
    ssm_instance: Entity<InputState>,
    ssm_remote_host: Entity<InputState>,
    ssm_region: Entity<InputState>,
    ssm_profile: Entity<InputState>,
    /// 可复用传输档案的**快照**（下拉选项与「存为 / 删除」按钮都读它）。
    ///
    /// 快照而不是每次渲染去读页面：弹窗里改完档案要立刻看到下拉变化，
    /// 而页面的变更走的是同一条 `upsert_tunnel_profile`，这里跟着刷新即可。
    tunnel_profiles: Vec<TunnelProfile>,
    /// 当前选中并**正在编辑**的那份档案 id（`None` = 自定义，用连接自己的三档）。
    tunnel_profile: Option<String>,
    /// 档案名（选中档案时是它的名字；自定义时是「存为新档案」要用的名字）。
    profile_name: Entity<InputState>,
    /// 连接钩子（工作副本；保存时才写回档案）。
    hook_rows: Vec<HookRow>,
    /// 「试跑」的结果（按钩子 id），只用于界面提示，不入档案。
    hook_results: std::collections::HashMap<String, HookOutcome>,
}

impl EditFormView {
    /// 新建态：按引擎把可预判的字段直接填好（对齐 dbx 的 `defaultForm()`），少让用户手打；
    /// SQLite 走本地文件，三样都不填。
    fn new(
        page: WeakEntity<DatabasePage>,
        kind: DatabaseKind,
        profiles: Vec<TunnelProfile>,
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
        // 新建时的**预置值**（只读档 / 默认超时 / 环境标签）来自「设置 → 数据库」。
        // 出厂默认仍是「无标签 / 可写 / 跟随引擎」，所以没设置过的话行为与从前一致。
        let prefs = db_prefs(cx);
        let env_tag = prefs
            .new_env_tag
            .as_deref()
            .map_or(EnvTag::None, EnvTag::from_wire);
        let timeout = text_input(window, cx, tr(cx, "留空跟随默认"));
        if let Some(secs) = prefs.new_timeout_secs {
            timeout.update(cx, |state, cx| {
                state.set_value(secs.to_string(), window, cx)
            });
        }
        Self {
            page,
            id: None,
            kind,
            tls: TlsMode::Disable,
            env_tag,
            name: text_input(window, cx, tr(cx, "例如：本地开发库")),
            host,
            port,
            user,
            database: text_input(window, cx, tr(cx, "库名（网络引擎可留空）")),
            password: secret_input(window, cx, tr(cx, "留空表示保持既有口令不变")),
            group: text_input(window, cx, tr(cx, "默认分组")),
            timeout,
            schema: text_input(window, cx, tr(cx, "留空用引擎当前命名空间")),
            read_only: prefs.new_read_only,
            ssh_enabled: false,
            ssh_host: text_input(window, cx, tr(cx, "跳板机主机")),
            ssh_port: text_input(window, cx, tr(cx, "留空用 22")),
            ssh_user: text_input(window, cx, tr(cx, "跳板机用户")),
            ssh_remote_host: text_input(window, cx, tr(cx, "留空表示跳板机本机")),
            ssh_password: secret_input(window, cx, tr(cx, "留空用密钥认证 / 保持既有")),
            // 两档都是**按需**功能：默认关闭，不预置占位值（与钩子同一条口径）。
            proxy_enabled: false,
            proxy_kind: ProxyKind::default(),
            proxy_host: text_input(window, cx, tr(cx, "代理主机")),
            proxy_port: text_input(window, cx, tr(cx, "留空用默认端口")),
            proxy_user: text_input(window, cx, tr(cx, "留空表示不需要认证")),
            proxy_password: secret_input(window, cx, tr(cx, "留空表示保持既有 / 不需要认证")),
            ssm_enabled: false,
            ssm_instance: text_input(window, cx, tr(cx, "实例 id 或实例名（i-… / tag:Name=…）")),
            ssm_remote_host: text_input(window, cx, tr(cx, "留空表示实例本机")),
            ssm_region: text_input(window, cx, tr(cx, "留空用 aws 配置里的区域")),
            ssm_profile: text_input(window, cx, tr(cx, "留空用 aws 默认 profile")),
            // 传输档案：新建态恒为「自定义」（不会替用户挑一份档案去改）。
            tunnel_profiles: profiles,
            tunnel_profile: None,
            profile_name: text_input(window, cx, tr(cx, "例如：公司代理")),
            // 新建态没有钩子：这是个**按需**功能，默认给空列表而不是预置一条占位的。
            hook_rows: Vec::new(),
            hook_results: std::collections::HashMap::new(),
        }
    }

    /// 编辑态：用既有档案填表（口令不入表，留空即保持既有）。
    fn from_config(
        page: WeakEntity<DatabasePage>,
        cfg: &ConnectionConfig,
        profiles: Vec<TunnelProfile>,
        window: &mut Window,
        cx: &mut Context<DatabasePage>,
    ) -> Self {
        let mut form = Self::new(page, cfg.kind, profiles, window, cx);
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
        // 超时**无条件写一遍**（`None` 就写空串）：`new()` 可能按设置页的
        // 「默认超时」预填过数字，留着它会把一条本来「跟随引擎」的既有连接
        // 变成「显式 30 秒」——保存时就把语义改掉了。
        form.timeout.update(cx, |state, cx| {
            state.set_value(
                cfg.default_timeout()
                    .map_or_else(String::new, |secs| secs.to_string()),
                window,
                cx,
            )
        });
        if let Some(schema) = cfg.schema.clone() {
            form.schema
                .update(cx, |state, cx| state.set_value(schema, window, cx));
        }
        form.read_only = cfg.read_only;
        // 引用了传输档案：下面三档显示的是**那份档案的内容**（选中态下三档就是档案本体，
        // 保存时原样写回档案）。展开一次，后面三段填表读的都是它。
        let (transport_cfg, profile) = expand_tunnel_profile(cfg, &form.tunnel_profiles);
        let profile_name = profile.map(|profile| profile.name.clone());
        form.tunnel_profile = cfg.tunnel_profile.clone();
        if let Some(name) = profile_name {
            form.profile_name
                .update(cx, |state, cx| state.set_value(name, window, cx));
        }
        if let Some(tunnel) = &transport_cfg.ssh_tunnel {
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
        if let Some(proxy) = &transport_cfg.proxy {
            form.proxy_enabled = true;
            form.proxy_kind = proxy.kind;
            form.proxy_host.update(cx, |state, cx| {
                state.set_value(proxy.host.clone(), window, cx)
            });
            if let Some(port) = proxy.port {
                form.proxy_port.update(cx, |state, cx| {
                    state.set_value(port.to_string(), window, cx)
                });
            }
            form.proxy_user.update(cx, |state, cx| {
                state.set_value(proxy.username.clone(), window, cx)
            });
        }
        if let Some(ssm) = &transport_cfg.ssm {
            form.ssm_enabled = true;
            form.ssm_instance.update(cx, |state, cx| {
                state.set_value(ssm.instance.clone(), window, cx)
            });
            if let Some(host) = ssm.remote_host.clone() {
                form.ssm_remote_host
                    .update(cx, |state, cx| state.set_value(host, window, cx));
            }
            if let Some(region) = ssm.region.clone() {
                form.ssm_region
                    .update(cx, |state, cx| state.set_value(region, window, cx));
            }
            if let Some(profile) = ssm.profile.clone() {
                form.ssm_profile
                    .update(cx, |state, cx| state.set_value(profile, window, cx));
            }
        }
        form.hook_rows = cfg
            .hooks
            .hooks
            .iter()
            .map(|hook| HookRow::new(hook.id.clone(), Some(hook), window, cx))
            .collect();
        form.hook_results.clear();
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
        let proxy = if self.proxy_enabled {
            let proxy_port_text = self.proxy_port.read(cx).value().trim().to_owned();
            let proxy_port = if proxy_port_text.is_empty() {
                None
            } else {
                match proxy_port_text.parse::<u16>() {
                    Ok(port) if port > 0 => Some(port),
                    _ => return Err(tr(cx, "代理端口必须是 1-65535 的数字").into()),
                }
            };
            Some(ProxyConfig {
                kind: self.proxy_kind,
                host: self.proxy_host.read(cx).value().trim().to_owned(),
                port: proxy_port,
                username: self.proxy_user.read(cx).value().trim().to_owned(),
                // 口令不入档案（`#[serde(skip)]`）：只在保存那一步进凭据文件。
                password: None,
            })
        } else {
            None
        };
        let ssm = if self.ssm_enabled {
            let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
            let optional = |value: String| (!value.is_empty()).then_some(value);
            Some(SsmConfig {
                instance: text(&self.ssm_instance),
                remote_host: optional(text(&self.ssm_remote_host)),
                region: optional(text(&self.ssm_region)),
                profile: optional(text(&self.ssm_profile)),
            })
        } else {
            None
        };
        // 启用的钩子必须先填完：存下一条永远跑不起来的命令，错误会推迟到建连那一刻才暴露。
        if self.hook_rows.iter().any(|row| row.is_incomplete(cx)) {
            return Err(tr(cx, "有一条启用的钩子还没填完（命令或脚本不能为空）。").into());
        }
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
            hooks: ConnectionHooks {
                hooks: self.hook_rows.iter().map(|row| row.to_hook(cx)).collect(),
            },
            proxy,
            ssm,
            // 选中档案时三档照旧读出来：它是**这份档案的内容**，保存那一步会写回档案，
            // 而连接自己存的是 `None` + 引用（见 `EditFormView::save`）。
            tunnel_profile: self.tunnel_profile.clone(),
        })
    }

    /// 配置 + 解析后的口令：表单里非空用表单值，否则读既有凭据（与保存语义一致）。
    fn prepared(&self, cx: &App) -> Result<(ConnectionConfig, Credentials), SharedString> {
        let cfg = self.to_config(cx)?;
        let password_text = self.password.read(cx).value().trim().to_owned();
        let db = if password_text.is_empty() {
            load_password(&cfg.id).unwrap_or(None)
        } else {
            Some(password_text)
        };
        // 三份额外口令同一条语义：表单非空优先，留空回落凭据文件。**档位缺失时得 `None`**
        // （跳过读文件）——`map` 会得到 `Some(None)`，那两个值在提示语里分不出来。
        //
        // ⚠️ 回落的是**谁的**凭据要看模式：选中传输档案时，三档是那份**档案**的内容，
        // 口令自然也在档案自己的键上（`dbprofile:{id}:…`）——从连接的键上读会得到一份
        // 别的口令（或者干脆是 `None`），表现成「档案里的代理明明能通，测试却认证失败」。
        let profile_id = self.tunnel_profile.clone();
        let ssh = if cfg.ssh_tunnel.is_some() {
            let text = self.ssh_password.read(cx).value().trim().to_owned();
            if !text.is_empty() {
                Some(text)
            } else if let Some(profile_id) = profile_id.as_deref() {
                load_profile_ssh_password(profile_id).unwrap_or(None)
            } else {
                load_ssh_password(&cfg.id).unwrap_or(None)
            }
        } else {
            None
        };
        let proxy = if cfg.proxy.is_some() {
            let text = self.proxy_password.read(cx).value().trim().to_owned();
            if !text.is_empty() {
                Some(text)
            } else if let Some(profile_id) = profile_id.as_deref() {
                load_profile_proxy_password(profile_id).unwrap_or(None)
            } else {
                load_proxy_password(&cfg.id).unwrap_or(None)
            }
        } else {
            None
        };
        Ok((cfg, Credentials { db, ssh, proxy }))
    }

    /// 保存：校验 → 口令进凭据文件 → 档案落 kv（经页面）。返回是否成功。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (mut cfg, creds) = match self.prepared(cx) {
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
        // 选中档案时，三档是**档案的内容**：先把它们写回档案（含档案自己的口令），
        // 连接本体只留一个引用。这样「改一处，引用它的五条连接下次建连全都生效」。
        if let Some(profile_id) = self.tunnel_profile.clone() {
            let name = self.profile_name.read(cx).value().trim().to_owned();
            let profile = TunnelProfile {
                id: profile_id.clone(),
                name: if name.is_empty() {
                    tr(cx, "未命名档案").to_owned()
                } else {
                    name
                },
                ssh: cfg.ssh_tunnel.clone(),
                proxy: cfg.proxy.clone(),
                ssm: cfg.ssm.clone(),
            };
            let _ = self
                .page
                .update(cx, |page, cx| page.upsert_tunnel_profile(profile, cx));
            self.refresh_tunnel_profiles(cx);
            // ⚠️ 档案的口令存在档案自己的键上，**不能**顺手写到连接的键：
            // 写过去等于把「这份档案的口令」在每条引用它的连接上各留一份副本。
            if let Some(password) = creds.proxy.clone().filter(|text| !text.is_empty())
                && let Err(error) = store_profile_proxy_password(&profile_id, &password)
            {
                notify::error(
                    trf(cx, "代理口令保存失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                );
                return false;
            }
            if let Some(password) = creds.ssh.clone().filter(|text| !text.is_empty())
                && let Err(error) = store_profile_ssh_password(&profile_id, &password)
            {
                notify::error(
                    trf(cx, "SSH 口令保存失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                );
                return false;
            }
            cfg.tunnel_profile = Some(profile_id);
            cfg.ssh_tunnel = None;
            cfg.proxy = None;
            cfg.ssm = None;
        }
        // 口令：非空才写凭据文件（编辑时留空 = 保持既有口令不变）。
        // 上面那条分支已经把档案口令写完了，这里的 `cfg.*` 都是 `None`，
        // 所以不会重复写——两处的判据都是「这个档位还在不在连接上」。
        if let Some(password) = creds.db.filter(|text| !text.is_empty())
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
            && let Some(ssh_password) = creds.ssh.filter(|text| !text.is_empty())
            && let Err(error) = store_ssh_password(&cfg.id, &ssh_password)
        {
            notify::error(
                trf(cx, "SSH 口令保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return false;
        }
        if cfg.proxy.is_some()
            && let Some(proxy_password) = creds.proxy.filter(|text| !text.is_empty())
            && let Err(error) = store_proxy_password(&cfg.id, &proxy_password)
        {
            notify::error(
                trf(cx, "代理口令保存失败：{}", &[&error.to_string()]),
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

    /// 把页面的档案快照拉回来（下拉选项要立刻反映刚才的新增 / 删除）。
    fn refresh_tunnel_profiles(&mut self, cx: &Context<Self>) {
        let Some(page) = self.page.upgrade() else {
            return;
        };
        self.tunnel_profiles = page.read(cx).tunnel_profiles.clone();
    }

    /// 选中档案：把它的三档读进表单（选中态下三档就是这份档案的内容）。
    fn select_tunnel_profile(
        &mut self,
        profile_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tunnel_profile = profile_id.clone();
        let Some(profile) = profile_id
            .as_deref()
            .and_then(|id| self.tunnel_profiles.iter().find(|profile| profile.id == id))
            .cloned()
        else {
            // 切回「自定义」：三档**保持当前值**不动，用户改完再存成新档案。
            cx.notify();
            return;
        };
        self.profile_name.update(cx, |state, cx| {
            state.set_value(profile.name.clone(), window, cx)
        });
        self.load_transport_into_form(&profile, window, cx);
        cx.notify();
    }

    /// 把一份档案的三档写进表单控件（切档案 / 新建时共用）。
    fn load_transport_into_form(
        &mut self,
        profile: &TunnelProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = |value: &str| value.to_owned();
        self.ssh_enabled = profile.ssh.is_some();
        self.ssh_host.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssh
                    .as_ref()
                    .map_or_else(String::new, |t| text(&t.host)),
                window,
                cx,
            )
        });
        self.ssh_port.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssh
                    .as_ref()
                    .and_then(|t| t.port)
                    .map_or_else(String::new, |port| port.to_string()),
                window,
                cx,
            )
        });
        self.ssh_user.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssh
                    .as_ref()
                    .map_or_else(String::new, |t| text(&t.user)),
                window,
                cx,
            )
        });
        self.ssh_remote_host.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssh
                    .as_ref()
                    .map_or_else(String::new, |t| text(&t.remote_host)),
                window,
                cx,
            )
        });
        self.proxy_enabled = profile.proxy.is_some();
        if let Some(proxy) = profile.proxy.as_ref() {
            self.proxy_kind = proxy.kind;
        }
        self.proxy_host.update(cx, |state, cx| {
            state.set_value(
                profile
                    .proxy
                    .as_ref()
                    .map_or_else(String::new, |p| text(&p.host)),
                window,
                cx,
            )
        });
        self.proxy_port.update(cx, |state, cx| {
            state.set_value(
                profile
                    .proxy
                    .as_ref()
                    .and_then(|p| p.port)
                    .map_or_else(String::new, |port| port.to_string()),
                window,
                cx,
            )
        });
        self.proxy_user.update(cx, |state, cx| {
            state.set_value(
                profile
                    .proxy
                    .as_ref()
                    .map_or_else(String::new, |p| text(&p.username)),
                window,
                cx,
            )
        });
        self.ssm_enabled = profile.ssm.is_some();
        self.ssm_instance.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssm
                    .as_ref()
                    .map_or_else(String::new, |s| text(&s.instance)),
                window,
                cx,
            )
        });
        self.ssm_remote_host.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssm
                    .as_ref()
                    .and_then(|s| s.remote_host.clone())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
        self.ssm_region.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssm
                    .as_ref()
                    .and_then(|s| s.region.clone())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
        self.ssm_profile.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssm
                    .as_ref()
                    .and_then(|s| s.profile.clone())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
    }

    /// 「存为新档案」：用当前三档 + 名称输入框的值建一份新档案，并把连接切到引用它。
    fn add_tunnel_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.profile_name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            notify::warning(tr(cx, "先给档案起个名字。"), window, cx);
            return;
        }
        let cfg = match self.to_config(cx) {
            Ok(cfg) => cfg,
            Err(message) => {
                notify::warning(message, window, cx);
                return;
            }
        };
        if cfg.ssh_tunnel.is_none() && cfg.proxy.is_none() && cfg.ssm.is_none() {
            notify::warning(tr(cx, "三档都空着，存下来会是一份空档案。"), window, cx);
            return;
        }
        // id 从名字派生 + 序号去重：与连接档案同一套做法，改名不会丢定位。
        let existing: Vec<String> = self
            .tunnel_profiles
            .iter()
            .map(|profile| profile.id.clone())
            .collect();
        let base = format!("tunnel-{}", id_suffix(&name));
        let mut id = base.clone();
        let mut seq = 2;
        while existing.contains(&id) {
            id = format!("{base}-{seq}");
            seq += 1;
        }
        let profile = TunnelProfile {
            id: id.clone(),
            name,
            ssh: cfg.ssh_tunnel.clone(),
            proxy: cfg.proxy.clone(),
            ssm: cfg.ssm.clone(),
        };
        let _ = self
            .page
            .update(cx, |page, cx| page.upsert_tunnel_profile(profile, cx));
        self.refresh_tunnel_profiles(cx);
        // 档案自己的口令：把表单里刚打的那份也存到档案的键上（不然「新建档案」之后
        // 第一次建连会因为档案没口令而认证失败，用户还得再进一次表单）。
        let creds = self.prepared(cx).map(|(_, creds)| creds).ok();
        if let Some(creds) = creds {
            if let Some(password) = creds.proxy.filter(|text| !text.is_empty()) {
                let _ = store_profile_proxy_password(&id, &password);
            }
            if let Some(password) = creds.ssh.filter(|text| !text.is_empty()) {
                let _ = store_profile_ssh_password(&id, &password);
            }
        }
        self.tunnel_profile = Some(id);
        notify::success(tr(cx, "传输档案已保存。"), window, cx);
        cx.notify();
    }

    /// 「删除这份档案」：引用它的连接一并清掉引用（页面的 `delete_tunnel_profile` 负责）。
    fn remove_tunnel_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile_id) = self.tunnel_profile.clone() else {
            return;
        };
        let refs = self
            .page
            .upgrade()
            .map_or(0, |page| page.read(cx).tunnel_profile_refs(&profile_id));
        let message = if refs > 0 {
            trf(
                cx,
                "删除后引用它的 {} 条连接会回到「自定义」，确定继续吗？",
                &[&refs.to_string()],
            )
            .to_string()
        } else {
            tr(cx, "确定删除这份传输档案吗？").to_owned()
        };
        if !duck_platform::confirm(tr(cx, "删除传输档案"), &message) {
            return;
        }
        let _ = self
            .page
            .update(cx, |page, cx| page.delete_tunnel_profile(&profile_id, cx));
        self.refresh_tunnel_profiles(cx);
        self.tunnel_profile = None;
        self.profile_name
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        notify::success(tr(cx, "传输档案已删除。"), window, cx);
        cx.notify();
    }

    /// 「测试连接」：用表单当前值探活（不保存、不开会话），报告版本或错误。
    fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (cfg, creds) = match self.prepared(cx) {
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
        let _ = self
            .page
            .update(cx, |page, cx| page.spawn_probe(cfg, creds, cx));
    }

    // ── 连接钩子 ──────────────────────────────────────────────────────

    /// 表单当前的「环境事实」（试跑钩子时喂给子进程；**不含口令**）。
    ///
    /// 用的是**表单里此刻的值**而不是已保存的档案：这样「改完主机顺手试跑一下钩子」
    /// 看到的就是改完之后的 `DUCK_CONN_HOST`，不必先保存再试。
    fn hook_context(&self, cx: &App) -> HookContext {
        let text = |state: &Entity<InputState>| state.read(cx).value().trim().to_owned();
        let port = {
            let raw = text(&self.port);
            if raw.is_empty() {
                self.kind
                    .default_port()
                    .map_or_else(String::new, |port| port.to_string())
            } else {
                raw
            }
        };
        HookContext {
            // 新建态还没有 id：试跑时这个变量会是空串（钩子不该依赖它做判断）。
            connection_id: self.id.clone().unwrap_or_default(),
            connection_name: text(&self.name),
            kind: self.kind.wire_name().to_owned(),
            host: text(&self.host),
            port,
            user: text(&self.user),
            database: text(&self.database),
        }
    }

    /// 加一条空钩子。
    fn add_hook(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing: Vec<String> = self.hook_rows.iter().map(|row| row.id.clone()).collect();
        let mut seq = existing.len() + 1;
        let id = loop {
            let candidate = format!("hook-{seq}");
            if !existing.contains(&candidate) {
                break candidate;
            }
            seq += 1;
        };
        let row = HookRow::new(id, None, window, cx);
        self.hook_rows.push(row);
        cx.notify();
    }

    /// 删一条钩子（连带清掉它的试跑结果）。
    fn remove_hook(&mut self, id: &str, cx: &mut Context<Self>) {
        self.hook_rows.retain(|row| row.id != id);
        self.hook_results.remove(id);
        cx.notify();
    }

    /// 「试跑」一条钩子：**在后台单跑一次**（不建连、不落任何状态，只为了确认它跑得通）。
    fn test_hook(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.hook_rows.get(index) else {
            return;
        };
        if row.is_incomplete(cx) {
            notify::warning(tr(cx, "这条钩子还没填完，先补上命令或脚本。"), window, cx);
            return;
        }
        let hook = row.to_hook(cx);
        let context = self.hook_context(cx);
        let phase = hook.phase;
        let id = hook.id.clone();
        let task = cx
            .background_executor()
            .spawn(async move { run_hook(&hook, phase, &context) });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |this, cx| {
                this.hook_results.insert(id, outcome);
                cx.notify();
            });
        })
        .detach();
    }

    /// HOOKS 分区：列表 + 「添加钩子」。
    fn hooks_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = active_theme(cx).tokens().clone();
        let mut section = v_flex()
            .w_full()
            .gap(form_gap(cx))
            .child(db_section_header(DuckIcon::SquareTerminal, "HOOKS", cx))
            .child(field_hint(
                tr(
                    cx,
                    "在建连 / 断开的前后跑一条本机命令或脚本；失败默认只提示、不阻断。",
                ),
                cx,
            ));
        if self.hook_rows.is_empty() {
            section = section.child(field_hint(tr(cx, "还没有钩子。"), cx));
        }
        for index in 0..self.hook_rows.len() {
            section = section.child(self.render_hook_row(index, cx));
        }
        section
            .child(
                h_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(
                        button(
                            "db-hook-add",
                            tr(cx, "添加钩子"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.add_hook(window, cx))),
                    )
                    .child(field_hint(
                        tr(cx, "钩子只在「连接 / 断开」时跑，不会在测试连接时跑。"),
                        cx,
                    )),
            )
            .into_any_element()
    }

    /// 一行钩子：启用 / 相位 / 载体 / 内容 / 工作目录 / 超时 / 失败策略 / 试跑 / 删除。
    fn render_hook_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let Some(row) = self.hook_rows.get(index) else {
            return div().into_any_element();
        };
        let id = row.id.clone();
        let mode = row.mode;
        let phase = row.phase;
        let failure = row.failure;
        let enabled = row.enabled;
        let entity = cx.entity().downgrade();

        // 相位下拉（四档）。
        let phase_options: Vec<(SharedString, SharedString)> = HookPhase::ALL
            .iter()
            .map(|phase| {
                (
                    SharedString::from(phase.key()),
                    SharedString::from(hook_phase_label(*phase, cx)),
                )
            })
            .collect();
        let phase_select = {
            let entity = entity.clone();
            let key = id.clone();
            dropdown_select(
                SharedString::from(format!("db-hook-phase-{key}")),
                phase_options,
                SharedString::from(phase.key()),
                move |value: SharedString, _window, cx| {
                    let Some(phase) = HookPhase::from_key(&value) else {
                        return;
                    };
                    let _ = entity.update(cx, |this, cx| {
                        if let Some(row) = this.hook_rows.iter_mut().find(|row| row.id == key) {
                            row.phase = phase;
                        }
                        cx.notify();
                    });
                },
                cx,
            )
        };

        // 失败策略下拉（两档）。
        let failure_options = vec![
            (
                SharedString::from("warn"),
                SharedString::from(tr(cx, "失败只提示（继续）")),
            ),
            (
                SharedString::from("block"),
                SharedString::from(tr(cx, "失败则中断本次操作")),
            ),
        ];
        let failure_select = {
            let entity = entity.clone();
            let key = id.clone();
            let current = match failure {
                HookFailure::Warn => "warn",
                HookFailure::Block => "block",
            };
            dropdown_select(
                SharedString::from(format!("db-hook-failure-{key}")),
                failure_options,
                SharedString::from(current),
                move |value: SharedString, _window, cx| {
                    let failure = if value.as_ref() == "block" {
                        HookFailure::Block
                    } else {
                        HookFailure::Warn
                    };
                    let _ = entity.update(cx, |this, cx| {
                        if let Some(row) = this.hook_rows.iter_mut().find(|row| row.id == key) {
                            row.failure = failure;
                        }
                        cx.notify();
                    });
                },
                cx,
            )
        };

        // 载体两档（命令 / 脚本）。
        let mode_toggle = h_flex().flex_none().gap(tokens.spacing.xxs).children(
            [HookMode::Command, HookMode::Script].map(|candidate| {
                let selected = mode == candidate;
                let key = id.clone();
                button(
                    SharedString::from(format!("db-hook-mode-{}-{}", candidate.key(), key)),
                    SharedString::from(candidate.label(cx)),
                    if selected {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    },
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(row) = this.hook_rows.iter_mut().find(|row| row.id == key) {
                        row.mode = candidate;
                    }
                    cx.notify();
                }))
            }),
        );

        // 启用 / 停用。
        let enable_toggle = {
            let key = id.clone();
            button(
                SharedString::from(format!("db-hook-enable-{key}")),
                if enabled {
                    tr(cx, "已启用")
                } else {
                    tr(cx, "已停用")
                },
                if enabled {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(row) = this.hook_rows.iter_mut().find(|row| row.id == key) {
                    row.enabled = !row.enabled;
                }
                cx.notify();
            }))
        };

        // 内容：命令档给一行命令，脚本档给「解释器 + 文件」。
        let content = match mode {
            HookMode::Command => field_row(cx, tr(cx, "命令"), &row.line),
            HookMode::Script => v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(field_row(cx, tr(cx, "解释器"), &row.interpreter))
                .child(field_row(cx, tr(cx, "脚本文件"), &row.path)),
        };

        let status = self
            .hook_results
            .get(&id)
            .map(|outcome| hook_outcome_line(outcome, cx));

        let remove = {
            let key = id.clone();
            toolbar_action_button(
                SharedString::from(format!("db-hook-remove-{key}")),
                tr(cx, "删除钩子"),
                Icon::new(DuckIcon::Trash2).size(extended.icon.sm),
                false,
                false,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.remove_hook(&key, cx)))
        };
        let run = toolbar_action_button(
            SharedString::from(format!("db-hook-test-{id}")),
            tr(cx, "试跑"),
            Icon::new(DuckIcon::Play).size(extended.icon.sm),
            false,
            false,
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| this.test_hook(index, window, cx)));

        v_flex()
            .w_full()
            .gap(form_gap(cx))
            .p(tokens.spacing.sm)
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(if enabled {
                tokens.colors.border
            } else {
                extended.colors.hairline
            })
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .flex_wrap()
                    .gap(tokens.spacing.xs)
                    .child(enable_toggle)
                    .child(phase_select)
                    .child(mode_toggle)
                    .child(div().flex_1())
                    .child(run)
                    .child(remove),
            )
            .child(content)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_row(cx, tr(cx, "工作目录"), &row.cwd))
                    .child(field_row(cx, tr(cx, "超时（秒）"), &row.timeout)),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "失败时"), cx))
                    .child(failure_select),
            )
            .children(status)
            .into_any_element()
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

/// 数据库模块偏好（设置页「数据库」；未设置时是各常量的出厂默认）。
///
/// 每次调用解析一遍 JSON —— 这几处都在「发起一次查询 / 翻一页」的路径上，不是每帧，
/// 相比之下 `serde_json` 的这点开销可以忽略；换来的是没有第二处真源要同步。
fn db_prefs(cx: &App) -> super::module_settings::DbSettings {
    super::module_settings::db_settings(cx)
}

/// 单条查询的结果行数上限（设置页可改）。
fn query_row_cap(cx: &App) -> usize {
    db_prefs(cx).row_cap as usize
}

/// 表浏览 / 分页查询每页的行数（设置页可改）。
fn browse_page_size(cx: &App) -> usize {
    db_prefs(cx).page_size as usize
}

/// 查询历史保留条数（设置页可改）。
fn history_limit(cx: &App) -> usize {
    db_prefs(cx).history_limit as usize
}

/// 指标页最多画几张折线（其余指标留在明细表里，一个不少）。
const METRIC_CHART_LIMIT: usize = 6;

/// 紧凑时长（数字 + `s`/`m`/`h`/`d`）：指标图的横轴档位与「覆盖 {}」共用。
///
/// 刻意**不**进 EN 表：`3m` 是量纲缩写而不是句子，各语言一致；这与「相对时间文案走
/// `tr_relative_time`」那条规则不冲突——那里管的是「3 分钟前」这类成句的文案。
fn compact_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// 纵轴刻度的紧凑数值（`1.5k` / `2.0M` / 整数原样 / 小数留两位）。
///
/// 服务器计数器动辄上亿（`Bytes_sent`），原样铺在纵轴上会把图挤没——k/M/G 是
/// 数字缩写，同样不进 EN 表。
fn compact_number(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1e9 {
        format!("{:.1}G", value / 1e9)
    } else if abs >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1}k", value / 1e3)
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// 默认映射最多取几条数值列（防一张几十列的表把图挤成麻花）。
const CHART_SERIES_LIMIT: usize = 5;
/// 判「一列是不是数值」时最多看几行（表可能几十万行，只看开头就够定性）。
const NUMERIC_PROBE_ROWS: usize = 64;

/// 从结果集挑默认的图表列：`x` 取**第一个非数值列**（全数值就退回第一列），
/// `y` 取前 [`CHART_SERIES_LIMIT`] 个数值列。
///
/// 没有任何数值列时返回 `None`——那说明这张表画不出图，界面该说清楚而不是画一张空图。
fn default_chart_columns(result: &ResultSet) -> Option<(String, Vec<String>)> {
    let numeric: Vec<bool> = (0..result.columns.len())
        .map(|index| column_is_numeric(result, index))
        .collect();
    // X 优先文本列（分类轴）；全是数值时用第一列当轴。
    let x_index = result
        .columns
        .iter()
        .enumerate()
        .find(|(index, _)| !numeric[*index])
        .or_else(|| result.columns.iter().enumerate().next())
        .map(|(index, _)| index)?;
    let x = result.columns[x_index].name.clone();
    let y: Vec<String> = result
        .columns
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != x_index && numeric[*index])
        .map(|(_, column)| column.name.clone())
        .take(CHART_SERIES_LIMIT)
        .collect();
    if y.is_empty() {
        return None;
    }
    Some((x, y))
}

/// 一列是不是数值列。
///
/// 不看引擎报的 `type_name`：各引擎口径不一（SQLite 是动态类型、ClickHouse 一堆
/// `Nullable(...)` 包装），拿它当判据会漏判；**值**才是真凭据。空值跳过（NULL 不代表
/// 非数值），见到第一个非数值非空的值就判否。
fn column_is_numeric(result: &ResultSet, index: usize) -> bool {
    let mut saw_number = false;
    for row in result.rows.iter().take(NUMERIC_PROBE_ROWS) {
        match row.get(index) {
            None | Some(Value::Null) => continue,
            Some(Value::Int(_) | Value::UInt(_) | Value::Float(_)) => saw_number = true,
            Some(Value::Decimal(text)) => {
                if text.trim().parse::<f64>().is_ok() {
                    saw_number = true;
                } else {
                    return false;
                }
            }
            // 布尔画成数是误导（`true` 出 1 还是 0 都有理由），一律不当数值。
            Some(_) => return false,
        }
    }
    saw_number
}

/// 把结果集的两列抽成「分类 → 数值」序列。
///
/// 跳过两类行：分类为 `NULL` 的（分类轴上一个叫「NULL」的桶是噪声，真要看那几行去表格
/// 视图）与数值非数的（画不进坐标系）。**不补 0**：补 0 会在图上画出一段实际不存在的
/// 「跌到底」，比断开更难解释。
///
/// 选中列在当前结果集里不存在时返回空表——调用方据此显示「配置失效」而不是画空图。
fn chart_series(result: &ResultSet, x: &str, y: &str) -> Vec<(String, f64)> {
    let column_index = |name: &str| result.columns.iter().position(|column| column.name == name);
    let (Some(xi), Some(yi)) = (column_index(x), column_index(y)) else {
        return Vec::new();
    };
    result
        .rows
        .iter()
        .filter_map(|row| {
            if matches!(row.get(xi), None | Some(Value::Null)) {
                return None;
            }
            let label = row.get(xi)?.render();
            let value = chart_number(row.get(yi)?)?;
            Some((label, value))
        })
        .collect()
}

/// 一格的数值（能画进坐标系的才算）；非数返回 `None`。
fn chart_number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(v) => Some(*v as f64),
        Value::UInt(v) => Some(*v as f64),
        Value::Float(v) if v.is_finite() => Some(*v),
        Value::Decimal(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 单张图的高度（小倍数一格的高度）。
const CHART_PANEL_HEIGHT: f32 = 220.;

/// 饼图配色：以强调色为起点按**黄金角**旋转色相，切片多了也不容易撞色。
///
/// 不用 gpui-component 自带的 `chart.1..5`：那是它自己主题里的蓝系五档，与 duck 的
/// 主题色不一路；按色相旋转则由本主题的强调色派生，明暗模式都跟着走。
fn chart_palette(base: Hsla, count: usize) -> Vec<Hsla> {
    /// 黄金角（度）：连续切片之间的色相间隔，任意前 n 个都分布均匀。
    const GOLDEN_ANGLE: f32 = 137.508;
    (0..count)
        .map(|index| {
            let hue = (base.h * 360.0 + GOLDEN_ANGLE * index as f32).rem_euclid(360.0);
            Hsla {
                h: hue / 360.0,
                ..base
            }
        })
        .collect()
}

/// 按类型造一张图（四种图的构造差异只集中在这一处）。
///
/// 数据一律 `Vec<(分类, 数值)>`；饼图另外带上下标（配色要按切片顺序取）。
/// 返回 `None` = 这份数据画不出来（饼图全是非正值），调用方据此给一句说明。
fn chart_element(
    kind: ChartKind,
    points: Vec<(String, f64)>,
    key: &str,
    accent: Hsla,
) -> Option<AnyElement> {
    let id = SharedString::from(format!("db-chart-{key}"));
    let element = match kind {
        ChartKind::Line => LineChart::new(points)
            .id(id)
            .x(|(label, _)| label.clone())
            .y(|(_, value)| *value)
            .stroke(accent)
            .grid(true)
            .y_tick_format(compact_number)
            .tooltip_title(|(label, _)| SharedString::from(label.clone()))
            .tooltip_value(|_, value| SharedString::from(compact_number(value)))
            .into_any_element(),
        ChartKind::Bar => BarChart::new(points)
            .id(id)
            .band(|(label, _)| label.clone())
            .value(|(_, value)| *value)
            .fill(move |_, _, _, _| accent)
            .grid(true)
            // 柱状图的「带」就是分类，值轴要自己画刻度（否则只有柱高没有刻度）。
            .value_axis(true)
            .value_tick_format(compact_number)
            .tooltip_title(|(label, _)| SharedString::from(label.clone()))
            .tooltip_value(|_, value| SharedString::from(compact_number(value)))
            .into_any_element(),
        ChartKind::Area => AreaChart::new(points)
            .id(id)
            .x(|(label, _)| label.clone())
            .y(|(_, value)| *value)
            .stroke(accent)
            .fill(accent.opacity(0.22))
            .grid(true)
            .y_tick_format(compact_number)
            .tooltip_title(|(label, _)| SharedString::from(label.clone()))
            .tooltip_value(|_, _, value| SharedString::from(compact_number(value)))
            .into_any_element(),
        ChartKind::Pie => {
            // 饼图画的是占比：非正值（负数 / 零）在饼上没有位置，先滤掉。
            let slices: Vec<(String, f64, usize)> = points
                .into_iter()
                .filter(|(_, value)| *value > 0.0)
                .enumerate()
                .map(|(index, (label, value))| (label, value, index))
                .collect();
            if slices.is_empty() {
                return None;
            }
            let palette = chart_palette(accent, slices.len());
            PieChart::new(slices)
                .id(id)
                .value(|(_, value, _)| *value as f32)
                .color(move |(_, _, index)| palette.get(*index).copied().unwrap_or(accent))
                .label(|(label, _, _)| SharedString::from(label.clone()))
                .label_color(accent)
                .inner_radius(0.5)
                .tooltip_name(|(label, _, _)| SharedString::from(label.clone()))
                .tooltip_value(|_, _, value| SharedString::from(compact_number(f64::from(value))))
                .into_any_element()
        }
    };
    Some(element)
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

/// 从 kv 读传输档案；解析失败回落空表（用户重存即可覆盖）。
fn load_tunnel_profiles(cx: &App) -> Vec<TunnelProfile> {
    duck_store::kv_get(cx, KEY_TUNNEL_PROFILES)
        .and_then(|json| serde_json::from_str::<Vec<TunnelProfile>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读查询历史（解析失败 / 缺键 → 空）。
fn load_history(cx: &App) -> Vec<String> {
    duck_store::kv_get(cx, KEY_HISTORY)
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读已保存查询（解析失败 / 缺键 → 空，用户重存即可覆盖）。
fn load_saved_queries(cx: &App) -> Vec<SavedQuery> {
    duck_store::kv_get(cx, KEY_SAVED_QUERIES)
        .and_then(|json| serde_json::from_str::<Vec<SavedQuery>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读已保存图表（解析失败 / 缺键 → 空，用户重存即可覆盖）。
fn load_saved_charts(cx: &App) -> Vec<SavedChart> {
    duck_store::kv_get(cx, KEY_SAVED_CHARTS)
        .and_then(|json| serde_json::from_str::<Vec<SavedChart>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读用户仪表板（解析失败 / 缺键 → 空，用户重存即可覆盖）。
fn load_dashboards(cx: &App) -> Vec<Dashboard> {
    duck_store::kv_get(cx, KEY_DASHBOARDS)
        .and_then(|json| serde_json::from_str::<Vec<Dashboard>>(&json).ok())
        .unwrap_or_default()
}

/// 把一条 SQL 记进查询历史（去重后置顶，超出 [`HISTORY_LIMIT`] 丢最旧的）。
///
/// 只记**非空且与最近一条不同**的语句：重复执行同一条查询是很常见的动作，
/// 每次都记会把历史刷成同一行。
fn push_history(history: &mut Vec<String>, sql: &str, limit: usize) {
    let sql = sql.trim();
    if sql.is_empty() {
        return;
    }
    history.retain(|entry| entry != sql);
    history.insert(0, sql.to_owned());
    history.truncate(limit.max(1));
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

/// 一行结果的 TSV 文本（不含表头）—— 「复制整行」用。
///
/// 单格转义走内核的 [`tsv_field`]：整表那条路（[`ResultSet::to_tsv`]）用的是同一个
/// 函数，两边各写一份转义规则迟早会漂移——而漂移的那一份会把带换行的备注错开一整行。
fn row_as_tsv(row: &[duck_db::Value]) -> String {
    row.iter()
        .map(|value| tsv_field(&value.render()))
        .collect::<Vec<_>>()
        .join("\t")
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

/// 一份可复用的**传输档案**：SSH / 代理 / SSM 三档里配了哪一档就是哪一档。
///
/// ⚠️ 三档用的是与连接档案**完全同一套结构体**。档案不是「另一种隧道」，它只是
/// 「把这三档挪到外面存、可以被多条连接引用」，展开回连接档案之后走的是同一条
/// [`open_transport`] —— 内核不知道有档案这回事，也就不会有两套行为要同步维护。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TunnelProfile {
    /// 稳定 id（`tunnel-` + 后缀），连接档案与凭据键名都引用它。
    id: String,
    /// 显示名（用户可改）。
    name: String,
    /// SSH 跳板机档。
    #[serde(default)]
    ssh: Option<SshTunnelConfig>,
    /// 代理档（口令在凭据文件 `dbprofile:{id}:proxy`）。
    #[serde(default)]
    proxy: Option<ProxyConfig>,
    /// AWS SSM 档。
    #[serde(default)]
    ssm: Option<SsmConfig>,
}

impl TunnelProfile {
    /// 这份档案配的是哪一档（用于界面标签；三档都没配得 `None`）。
    ///
    /// 优先级与 [`ConnectionConfig::transport`] 一致——界面上的标签与真正生效的
    /// 必须是同一档，否则用户按标签排错会排到错的那条路上。
    fn kind(&self) -> Option<TransportKind> {
        if self.ssm.as_ref().is_some_and(SsmConfig::is_usable) {
            Some(TransportKind::Ssm)
        } else if self
            .ssh
            .as_ref()
            .is_some_and(|tunnel| !tunnel.host.trim().is_empty() && !tunnel.user.trim().is_empty())
        {
            Some(TransportKind::Ssh)
        } else if self
            .proxy
            .as_ref()
            .is_some_and(|proxy| !proxy.host.trim().is_empty())
        {
            Some(TransportKind::Proxy)
        } else {
            None
        }
    }
}

/// 展开「隧道档案」的三档，返回**命中的那份档案**（供调用方取它的凭据）。
///
/// 语义上取的是**整体替换**而不是逐档合并：档案只配了代理、连接自己还留着一条 SSH 时，
/// 「合并」会得到两份都成立的三档，而 [`ConnectionConfig::transport`] 只挑一个 ——
/// 结果是「用户以为在走代理，实际在走 SSH」。整体替换没有这个歧义。
///
/// 引用的档案不在了（被删 / 从别的机器导入的档案）：**回落到连接自己的三档**，
/// 而不是静默直连 —— 「配了隧道却直连」是最危险的那种失败。
fn expand_tunnel_profile<'a>(
    cfg: &ConnectionConfig,
    profiles: &'a [TunnelProfile],
) -> (ConnectionConfig, Option<&'a TunnelProfile>) {
    let Some(profile_id) = cfg.tunnel_profile.as_deref() else {
        return (cfg.clone(), None);
    };
    let Some(profile) = profiles.iter().find(|profile| profile.id == profile_id) else {
        return (cfg.clone(), None);
    };
    let mut out = cfg.clone();
    out.ssh_tunnel = profile.ssh.clone();
    out.proxy = profile.proxy.clone();
    out.ssm = profile.ssm.clone();
    (out, Some(profile))
}

/// 展开档案并**把凭据也换过去**（建连路径用）。
///
/// 档案是独立凭据主体（`dbprofile:{id}:…`）：一份公司 SOCKS5 出口被五条连接引用时，
/// 口令只该存一份。界面判据（只想知道「走哪一档」）用 [`expand_tunnel_profile`]，
/// 不必白读一遍凭据文件。
fn resolve_transport(
    cfg: &ConnectionConfig,
    profiles: &[TunnelProfile],
    mut creds: Credentials,
) -> (ConnectionConfig, Credentials) {
    let (out, profile) = expand_tunnel_profile(cfg, profiles);
    if let Some(profile) = profile {
        creds.ssh = load_profile_ssh_password(&profile.id).unwrap_or(None);
        creds.proxy = load_profile_proxy_password(&profile.id).unwrap_or(None);
    }
    (out, creds)
}

/// 已建立的传输通道（三档互斥，优先级见 [`ConnectionConfig::transport`]）。
///
/// 三个变体对内都只暴露一个本机监听端口，所以调用方只需要 `local_port()`——
/// 换句话说「通道是哪种」只影响界面上那一行字与错误归因。
enum Transport {
    /// `ssh -N -L`（走用户本机 `ssh`）。
    Ssh(Arc<SshTunnel>),
    /// SOCKS5 / HTTP CONNECT（进程内手写握手）。
    Proxy(Arc<ProxyTunnel>),
    /// AWS SSM 端口转发（走用户本机 `aws` CLI）。
    Ssm(Arc<SsmTunnel>),
}

impl Transport {
    /// 本机监听端口（数据库连接实际指向 `127.0.0.1:{port}`）。
    fn local_port(&self) -> u16 {
        match self {
            Self::Ssh(tunnel) => tunnel.local_port(),
            Self::Proxy(tunnel) => tunnel.local_port(),
            Self::Ssm(tunnel) => tunnel.local_port(),
        }
    }

    /// 通道还活着吗（子进程未退出 / 监听线程未停）。
    fn is_alive(&self) -> bool {
        match self {
            Self::Ssh(tunnel) => tunnel.is_alive(),
            Self::Proxy(tunnel) => tunnel.is_alive(),
            Self::Ssm(tunnel) => tunnel.is_alive(),
        }
    }

    /// 界面用的名字（连接信息卡「接入方式」那一行）。
    fn label(&self, cx: &App) -> &'static str {
        match self {
            Self::Ssh(_) => transport_kind_label(TransportKind::Ssh, cx),
            Self::Proxy(_) => transport_kind_label(TransportKind::Proxy, cx),
            Self::Ssm(_) => transport_kind_label(TransportKind::Ssm, cx),
        }
    }
}

/// 建连所需的三份额外口令。
///
/// 单独一个结构体而不是继续往元组里塞：三个 `Option<String>` 并排之后，
/// 光看调用点已经分不清谁是谁，而**送错口令**的后果是「认证失败」这种最难查的错。
#[derive(Debug, Clone, Default)]
struct Credentials {
    /// 数据库口令。
    db: Option<String>,
    /// SSH 跳板机口令。
    ssh: Option<String>,
    /// SOCKS5 / HTTP CONNECT 代理口令。
    proxy: Option<String>,
}

impl Credentials {
    /// 全部从凭据文件读（真实建连路径用）。
    ///
    /// 「测试连接 / 保存」走表单值，见 `EditFormView::prepared`——那里表单非空优先、
    /// 留空再回落到这里同一套凭据文件，两处语义一致。
    fn from_secret_store(id: &str) -> Self {
        Self {
            db: load_password(id).unwrap_or(None),
            ssh: load_ssh_password(id).unwrap_or(None),
            proxy: load_proxy_password(id).unwrap_or(None),
        }
    }
}

/// 按档案建传输通道，并给出**实际该连的主机与端口**。
///
/// 生效档位由 [`ConnectionConfig::transport`] 决定（优先级只写在那里一处），
/// 本函数只负责把它落地成具体的进程 / 监听端口。返回的第二个值是「指向通道入口的
/// 副本」：调用方拿它去建连，原档案不改（否则「关掉代理」要重连才发现主机被改写过）。
fn open_transport(
    cfg: &ConnectionConfig,
    creds: &Credentials,
) -> Result<(Option<Transport>, ConnectionConfig), DbError> {
    let mut effective = cfg.clone();
    // SQLite / 本地文件不走任何通道：起隧道只会白开一个没人用的端口。
    if !cfg.kind.needs_host() {
        return Ok((None, effective));
    }
    let remote_port = cfg
        .port
        .unwrap_or_else(|| cfg.kind.default_port().expect("network kind has a port"));
    let transport = match cfg.transport() {
        TransportKind::Direct => None,
        TransportKind::Ssh => {
            // `transport()` 只在 ssh_tunnel 存在且填全时才说 Ssh，这里不可能落空。
            let tunnel = cfg
                .ssh_tunnel
                .as_ref()
                .expect("ssh transport implies config");
            Some(Transport::Ssh(Arc::new(SshTunnel::open(SshTunnelSpec {
                jump_host: tunnel.host.clone(),
                jump_port: tunnel.port,
                user: tunnel.user.clone(),
                remote_host: tunnel.remote_host.clone(),
                remote_port,
                password: creds.ssh.clone(),
            })?)))
        }
        TransportKind::Proxy => {
            let proxy = cfg.proxy.as_ref().expect("proxy transport implies config");
            Some(Transport::Proxy(Arc::new(ProxyTunnel::open(
                ProxyTunnelSpec {
                    proxy: ProxyConfig {
                        // 口令永远不在档案里（`#[serde(skip)]`），只能从凭据文件补。
                        password: creds.proxy.clone(),
                        ..proxy.clone()
                    },
                    target_host: cfg.host.clone(),
                    target_port: remote_port,
                },
            )?)))
        }
        TransportKind::Ssm => {
            let ssm = cfg.ssm.as_ref().expect("ssm transport implies config");
            Some(Transport::Ssm(Arc::new(SsmTunnel::open(SsmTunnelSpec {
                config: ssm.clone(),
                target_host: cfg.host.clone(),
                target_port: remote_port,
            })?)))
        }
    };
    if let Some(transport) = transport.as_ref() {
        effective.host = "127.0.0.1".into();
        effective.port = Some(transport.local_port());
    }
    Ok((transport, effective))
}

/// 用给定档案探活：建连（含传输通道）并取服务器版本描述；不落任何会话状态。
///
/// **不跑连接钩子**：钩子挂在「建立一次真实会话」这件事上（开 VPN / 切 DNS 之后才该连），
/// 而「测试连接」只是一次探活 —— 它不改任何状态，也不该触发副作用。
/// 探活 / 建连失败发生在**哪一层**。
///
/// 分开这两层是为了审计与提示能说准话：配了 SSH 隧道却统一报「连接失败」时，
/// 用户会去查库名和口令，而真正的问题在隧道那头（密钥、跳板机策略）。
///
/// 判据取自**代码结构**（是哪个 `?` 抛出来的），不是从错误文本里找关键词 ——
/// 后者会随驱动升级失配，而失配的表现是「提示语指错方向」。
#[derive(Debug)]
enum StageError {
    /// 传输通道（SSH 隧道 / 代理 / SSM）没建起来。
    Transport(DbError),
    /// 通道通了，但连库 / 只读档 / 目录内省失败。
    Session(DbError),
}

impl StageError {
    /// 出错的原始错误。
    fn error(&self) -> &DbError {
        match self {
            Self::Transport(error) | Self::Session(error) => error,
        }
    }

    /// 落到审计的哪一类（提示语与流水共用这一个判据，不会两边打架）。
    fn kind(&self) -> crate::audit::AuditKind {
        match self {
            Self::Transport(_) => crate::audit::AuditKind::Transport,
            Self::Session(_) => crate::audit::AuditKind::Connection,
        }
    }

    /// 这一层的中文名（提示语里点明「哪一段断了」）。
    fn label(&self, cx: &App) -> &'static str {
        match self {
            Self::Transport(_) => tr(cx, "传输通道"),
            Self::Session(_) => tr(cx, "连接"),
        }
    }
}

/// 显示成**原始错误**：提示语自己会在外面补上「哪一层」与关联 id
///（`Display` 拿不到 `cx`，译不出层名 —— 所以层名不在这里拼）。
impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error().fmt(f)
    }
}

fn probe_connection(cfg: &ConnectionConfig, creds: &Credentials) -> Result<String, StageError> {
    let (_transport, effective) = open_transport(cfg, creds).map_err(StageError::Transport)?;
    let mut conn =
        Connection::connect(&effective, creds.db.as_deref()).map_err(StageError::Session)?;
    Ok(conn.server_version().unwrap_or_default())
}

/// 建连的实体动作：传输通道 → 连接 → 只读档 → 目录内省。
///
/// 抽成自由函数是为了让钩子能插在这一串动作的两头：写成
/// 「先跑钩子 → 建连 → 再跑钩子」的话，闭包里三层嵌套的缩进会把流程读到看不清。
///
/// 凭据由调用方传进来而不是这里现读：走「传输档案」时口令在档案自己的键上
/// （`dbprofile:{id}:…`），展开动作在 [`resolve_transport`] 里，这里不该再猜一遍。
fn open_session(cfg: &ConnectionConfig, creds: &Credentials) -> Result<Session, StageError> {
    // 传输通道（SQLite 走本地文件，不需要）：先建通道再指回 127.0.0.1。
    let (transport, effective) = open_transport(cfg, creds).map_err(StageError::Transport)?;
    let mut conn =
        Connection::connect(&effective, creds.db.as_deref()).map_err(StageError::Session)?;
    // 连接级只读档：由**服务器**兜底拒绝写（不是界面软拦），建连后立刻生效。
    if effective.read_only {
        conn.set_read_only(true).map_err(StageError::Session)?;
    }
    let version = conn.server_version().unwrap_or_default();
    // 命名空间清单 + 目录 + 库级对象（触发器 / 事件 / 用户）一次取齐；
    // 只有目录失败算真失败，其余取不到就退化成空列表。
    let introspection =
        Introspection::load(&mut conn, effective.schema.as_deref()).unwrap_or_default();
    let database =
        (!effective.database.trim().is_empty()).then(|| effective.database.trim().to_owned());
    Ok(Session {
        conn: Arc::new(Mutex::new(conn)),
        version,
        catalog: introspection.catalog,
        triggers: introspection.triggers,
        events: introspection.events,
        users: introspection.users,
        schemas: introspection.schemas,
        databases: introspection.databases,
        database,
        schema: effective.schema.clone(),
        kind: effective.kind,
        transport,
        in_tx: false,
    })
}

/// 传输档位的中文名（提示与标签用；判据与持久化一律走 [`TransportKind::key`]）。
fn transport_kind_label(kind: TransportKind, cx: &App) -> &'static str {
    match kind {
        TransportKind::Direct => tr(cx, "直连"),
        TransportKind::Ssh => tr(cx, "SSH 隧道"),
        TransportKind::Proxy => tr(cx, "代理"),
        TransportKind::Ssm => tr(cx, "AWS SSM"),
    }
}

/// 「断在哪一层 · 走的是哪条通道」。
///
/// 只在不走直连时补通道名：直连是默认形态，写出来是噪音；而配了通道仍报失败时，
/// 「通道这一层过了没有」正是第一个要分清的事 —— `StageError` 说层，这里补通道。
fn failure_layer(error: &StageError, cfg: &ConnectionConfig, cx: &App) -> String {
    match cfg.transport() {
        TransportKind::Direct => error.label(cx).to_owned(),
        kind => format!("{} · {}", error.label(cx), transport_kind_label(kind, cx)),
    }
}

/// 失败落在哪一层 → 审计动作键（探活与建连共用）。
fn stage_action(kind: crate::audit::AuditKind) -> &'static str {
    match kind {
        crate::audit::AuditKind::Transport => crate::audit::action::OPEN_TRANSPORT,
        _ => crate::audit::action::PROBE,
    }
}

/// 失败提示的统一形状：`<什么失败>（<关联 id>）：<原因>`。
///
/// 关联 id 与审计里那一行**是同一个** —— 用户把提示截图发过来，就能直接定位那条流水；
/// 没有 id（理论上失败一定有）时退化成不带括号的旧形状，不显示空括号。
fn failure_message(cx: &App, what: &str, trace: Option<&str>, reason: &str) -> SharedString {
    match trace {
        Some(trace) => trf(cx, "{}失败（{}）：{}", &[what, trace, reason]).into(),
        None => trf(cx, "{}失败：{}", &[what, reason]).into(),
    }
}

/// 审计摘要里放的那一行 SQL：折掉换行、按字符截断。
///
/// 不放全文：审计是**明文落盘**的一张表（会被读、会被复制出去），一条几百行的脚本就能把
/// 它撑成日志文件；而「一眼认出是哪条语句」只需要开头这一截 —— 要精确比对有**指纹**
/// （同一形状的语句指纹相同，正文里的字面量本来就该被抹掉）。
fn audit_sql_summary(sql: &str) -> String {
    /// 摘要的字符上限（中文按字符算，不是字节）。
    const SUMMARY_LIMIT: usize = 80;
    let flat: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SUMMARY_LIMIT {
        return flat;
    }
    let head: String = flat.chars().take(SUMMARY_LIMIT).collect();
    format!("{head}…")
}

/// 钩子相位的中文名（提示文案里要用；落库与元素 id 走 [`HookPhase::key`]，不用它）。
fn hook_phase_label(phase: HookPhase, cx: &App) -> &'static str {
    match phase {
        HookPhase::PreConnect => tr(cx, "建连前"),
        HookPhase::PostConnect => tr(cx, "建连后"),
        HookPhase::PreDisconnect => tr(cx, "断开前"),
        HookPhase::PostDisconnect => tr(cx, "断开后"),
    }
}

/// 钩子执行状态的中文名（内核的 [`HookStatus`] 是**英文**的，界面这一层才翻译）。
fn hook_status_label(status: &HookStatus, cx: &App) -> String {
    match status {
        HookStatus::Ok => tr(cx, "成功").to_owned(),
        HookStatus::Failed { code: Some(code) } => {
            trf(cx, "失败（退出码 {}）", &[&code.to_string()])
        }
        HookStatus::Failed { code: None } => tr(cx, "失败（被信号终止）").to_owned(),
        HookStatus::TimedOut => tr(cx, "超时").to_owned(),
        HookStatus::SpawnFailed(_) => tr(cx, "起不来（命令或解释器不存在）").to_owned(),
    }
}

/// 一次钩子结果的一行（状态 + 耗时 + 输出首行）。
fn hook_outcome_line(outcome: &HookOutcome, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    // 成功用扩展色板里的 success（它只在扩展色里有），失败用 base 的 destructive ——
    // 与通知的语义色取法是同一条口径。
    let color = if outcome.status.is_ok() {
        extended.colors.success
    } else {
        tokens.colors.destructive
    };
    let mut line = format!(
        "{} · {} ms",
        hook_status_label(&outcome.status, cx),
        outcome.duration_ms
    );
    // 只取**首行非空**输出：整段塞进表单会把行高撑爆，剩下的内容在日志里。
    if let Some(preview) = outcome
        .output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
    {
        line.push_str(" · ");
        line.push_str(preview);
    }
    meta_text(SharedString::from(line), cx)
        .text_color(color)
        .into_any_element()
}

/// 钩子能看到的环境事实（由档案派生；**不含口令**）。
fn hook_facts(cfg: &ConnectionConfig) -> HookContext {
    HookContext {
        connection_id: cfg.id.clone(),
        connection_name: cfg.display_name(),
        kind: cfg.kind.wire_name().to_owned(),
        host: cfg.host.clone(),
        port: cfg
            .port
            .or_else(|| cfg.kind.default_port())
            .map_or_else(String::new, |port| port.to_string()),
        user: cfg.user.clone(),
        database: cfg.database.clone(),
    }
}

/// 把一次相位报告逐条写进 `tracing` 日志（界面提示由调用方另做）。
///
/// ⚠️ 这里**不是**审计流水的落点。钩子每建立 / 拆除一次连接就把每个钩子跑一遍，
/// 全记「成功」只会把真正该看见的东西淹掉；而钩子唯一能改变连接结局的那一次
/// （挡住 / 失败）已经由连接路径记成 `Blocked` / `Failed` 了。想看「刚才那几个钩子
/// 跑出了什么」有内存里的 `hook_log` 面板，想看历史有 tracing。
fn log_hook_report(conn_id: &str, phase: HookPhase, report: &HookReport) {
    for outcome in &report.outcomes {
        tracing::info!(
            connection = %conn_id,
            hook = %outcome.hook_id,
            phase = %phase.key(),
            command = %outcome.command,
            status = %outcome.detail(),
            duration_ms = outcome.duration_ms,
            "connection hook finished"
        );
    }
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
    /// 已建立的会话（键 = 连接 id）。
    sessions: HashMap<String, Session>,
    /// 每个标签的编辑区快照（SQL 文本 + 结果区），切标签时存/取。
    ///
    /// 与 [`Self::sessions`] 平行但**分开存**：这里有 gpui 实体（`Send` 不了），
    /// 而 `sessions` 要送进后台线程。断开标签时两边一起清。
    ///
    /// 键 = **标签 id**（[`Self::tab_conn`] 的键），不是连接 id —— 同一连接可以
    /// 开多个查询标签（dbx 同款），每个标签各存一份编辑区。
    sessions_views: HashMap<String, SessionEditorState>,
    /// 打开的查询标签（顺序即展示顺序；元素是**标签 id**，经 [`Self::tab_conn`] 绑到连接）。
    tabs: Vec<String>,
    /// 标签 id → 连接 id。一个连接可以挂多个查询标签；会话（[`Self::sessions`]）
    /// 仍按连接共享 —— 多个标签只是各拿各的编辑区，底层连接只有一条。
    tab_conn: HashMap<String, String>,
    /// 标签身份（键 = [`Self::tab_conn`] 的键）：打开时定死，随标签关闭移除。
    ///
    /// 不进 [`SessionEditorState`] 快照 —— 身份跟着标签走而不是跟着内容走，
    /// 快照只管「编辑器文本 + 结果区」。
    tab_kind: HashMap<String, TabKind>,
    /// 标签 id 自增序号（会话内递增；标签不持久化，重启后从 1 重来没有影响）。
    tab_seq: usize,
    /// 当前激活的标签 id。
    active_tab: Option<String>,
    /// 当前激活的连接 id（随 [`Self::active_tab`] 派生；页面其余逻辑按连接语义读它）。
    active: Option<String>,
    /// 主区模式与数据。
    mode: Mode,
    /// 实例信息页取回的数据（当前标签的那一份；切标签随快照走）。
    instance_data: InstanceData,
    /// 指标时序（按连接 id 存；只在会话内累积，断开即丢）。
    ///
    /// 与 `instance_data` 分开存：那边是**快照**（切标签随 `SessionView` 走），这边是
    /// **历史**（每次刷新往里追加一条采样），按连接存才不会跟着标签切来切去。
    metric_history: std::collections::HashMap<String, MetricHistory>,
    /// 连接钩子的执行记录（会话内环形缓冲，上限 [`HOOK_LOG_CAP`]）。
    ///
    /// 只留内存：钩子的输出可能很长，而问题只到「刚才那条钩子到底跑成没有」为止——
    /// 当次会话内查得到就够了，不必去翻日志文件。（不进审计流水的理由同 [`HookRecord`]。）
    hook_log: std::collections::VecDeque<HookRecord>,
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
    /// 产出当前结果那一次执行的耗时（毫秒；结果区 footer 常驻展示）。
    query_elapsed_ms: Option<u128>,
    /// 当前结果**已加载**的行数（「加载更多」的下一页 `offset`）。
    query_loaded: u64,
    /// 当前结果之后是否还有更多行（决定「加载更多」入口是否出现）。
    query_has_more: bool,
    /// 执行计划态；见 [`PlanView`]。
    plan_view: Option<PlanView>,
    /// **下一次**装结果时顺手解析一份执行计划（`EXPLAIN` 按钮置位、装完即清）。
    ///
    /// 用一次性标记而不是给 [`Self::execute_sql`] 加参数：EXPLAIN 与普通查询走的是
    /// 同一条执行路径（同一把取消句柄、超时、截断提示），分支只该落在"结果怎么装"
    /// 这一步上，别把整条路径复制一份。
    plan_next: bool,
    /// **这一次**表读取是「用户刚打开一张表」，读回来要记一条「浏览表」流水。
    ///
    /// 与 [`Self::plan_next`] 同一种一次性标记，理由也一样：读表这条路径被
    /// 翻页 / 排序 / 过滤 / 写后重载共用，只有「打开表」这一个入口是值得记的
    /// 用户动作。把标记放这里而不是在 `load_table_page` 里判「是不是第一页」：
    /// 写完之后的重载经常也停在第 0 页，按页号判会把每次写都变成「浏览」。
    browse_open_pending: bool,
    /// 上一次结果是 EXPLAIN，但结构**没解析出来**（状态栏要如实说一句；
    /// 输出仍在网格里，只是没被整理成树）。
    plan_flat: bool,
    /// 当前结果的列宽（任务收尾时算好）。
    column_widths: Vec<f32>,
    /// 结果网格的**横向**滚动句柄（跨帧持久：滚轮 / 拖滚动条都读写同一份偏移；
    /// 常驻滚动条 [`ScrollbarMode::Always`] 也从它读位置）。
    result_x_scroll: ScrollHandle,
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
    /// 暂存区的逐步撤销栈 / 重做栈（见 [`PendingSnapshot`]）。
    pending_undo: Vec<PendingSnapshot>,
    pending_redo: Vec<PendingSnapshot>,
    /// 表浏览的过滤条件（生效中的；见 [`SessionView::table_filters`]）。
    table_filters: Vec<TableFilter>,
    /// 过滤条里「待添加」的草稿：列 / 操作符。
    ///
    /// 草稿不进 [`SessionView`]：与 `batch_value`（批量改值输入框）同一条口径——
    /// 它是一次**未提交的输入**，不是某个标签的结果区状态。
    filter_column: Option<String>,
    filter_op: FilterOp,
    /// 过滤条里「待添加」的值输入框。
    filter_value: Entity<InputState>,
    /// 当前选中表的主键列名（表页加载时内省；空 = 无主键 / 只读）。
    table_pk: Vec<String>,
    /// 当前选中表的 DDL（表页加载时顺带取；视图 / 无 DDL 为 `None`）。
    table_ddl: Option<String>,
    /// 当前选中表的索引（表级元信息，只在换表时取一次）。
    table_indexes: Vec<TableIndex>,
    /// 当前选中表的行数（口径见 [`RowCount`]；视图 / 不可数对象为 `None`）。
    table_row_count: Option<RowCount>,
    /// 当前选中表的列（右栏「列」卡；换表时取一次）。
    table_columns: Vec<Column>,
    /// 当前选中表的外键（右栏「外键」卡）。
    table_foreign_keys: Vec<TableForeignKey>,
    /// 当前选中表的触发器（右栏「触发器」卡）。
    table_triggers: Vec<TableTrigger>,
    /// 表级元信息是为哪张表取的（翻页不重复 `COUNT(*)`）。
    table_meta_for: Option<TableRef>,
    /// 行编辑器（暂存式）：改动只存输入框，点保存才落库。
    row_editor: Option<RowEditor>,
    /// 结果区的图表视角配置（`None` = 看表格）。按标签存（见 [`SessionView`]）。
    chart_view: Option<ChartConfig>,
    /// 查询历史（最近在前；kv 单键持久化）。
    history: Vec<String>,
    /// 已保存查询（用户主动留的 SQL 片段；kv 单键持久化，左栏「已保存查询」入口）。
    saved_queries: Vec<SavedQuery>,
    /// 已保存图表（SQL + 列映射 + 图表类型；kv 单键持久化，左栏「已保存图表」入口）。
    saved_charts: Vec<SavedChart>,
    /// 用户仪表板（12 栏网格；kv 单键持久化，左栏「仪表板」入口）。
    dashboards: Vec<Dashboard>,
    /// 可复用传输档案（SSH / 代理 / SSM 三档；kv 单键持久化）。
    ///
    /// 连接档案里只存一个 `tunnel_profile` 引用 id，**建连前在这里展开**成内联三档
    /// （见 [`resolve_transport`]）—— 所以「改一份档案」对引用它的所有连接下次建连即生效。
    tunnel_profiles: Vec<TunnelProfile>,
    /// 仪表板面板取回的数据。
    ///
    /// 键带**连接 id**：同一块仪表板换条连接打开，跑出来的是另一份数，缓存不能混。
    /// 带**仪表板 id**：面板 id 只在一块板内唯一。
    dashboard_results: HashMap<(String, u32, u32), PanelFetch>,
    /// 折叠起来的分隔条（面板 id）。只在本会话有效 —— 折叠是「此刻我想少看点」，
    /// 不是这块板的固有配置，不该跟着 kv 走。
    dashboard_collapsed: BTreeSet<u32>,
    /// 仪表板是否处于**编辑态**（对齐 DBFlux 的 View / Edit）。
    dashboard_editing: bool,
    /// 查询历史面板是否展开。
    history_open: bool,
    /// AI 助手面板是否展开。
    ai_open: bool,
    /// 选中的 AI 厂商档案 id；`None` = 跟随助手页的默认档案 / 第一条。
    ///
    /// 档案本身（base_url / model / 密文 key）**不落在这里**：一律按需从
    /// [`crate::ai`] 读（助手页与 db 页共用同一份 `kv: ai_providers`），
    /// 避免.db 页缓存一份会和助手页打架的副本。
    ai_provider: Option<String>,
    /// AI 提问输入框。
    ai_question: Entity<InputState>,
    /// AI「解释」动作的正文（`None` = 不展示）；**只展示，不执行**。
    ai_explain: Option<String>,
    /// 最近一次执行失败的上下文（供「AI 修错 / 解释」注入真实错误原文）。
    last_error: Option<QueryError>,
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
    /// 左栏树里**哪些节点是展开的**（库节点与分类节点共用一个集合）。
    ///
    /// 键的拼法：库节点 `db:<连接 id>:<库名>`；分类节点 `cat:<连接 id>:<库名>:<分类键>`。
    /// 只存「展开」，没记录的默认收起 —— 收起是安全的默认值（连上 MySQL 时
    /// `information_schema` / `performance_schema` 各有上百张系统表，全展开等于把
    /// 用户想看的那个库挤到屏幕外面）。
    ///
    /// 展开状态**不进 kv**：它是「这一次看库」的临时视图，不是偏好设置
    /// （与 `Appearance` 里那些密度 / 缩放是两回事）。
    tree_open: std::collections::HashSet<String>,
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
        // 占位符点明**两件事**：这个框既筛连接、也筛对象（对齐 DBFlux 的
        // 「筛选连接和 Schema…」）——只写「搜索连接」的话，没人会想到它能找表。
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "搜索连接与对象…")));
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
        // 编辑器任何 notify（滚动 / 光标 / 文本变化）都让**页面**重渲一次：
        // 左栏「逐条执行」的 ▶ 按 `行号 × 行高 − 滚动偏移` 定位，编辑器内部滚动时
        // 只有这条 observe 能把偏移变化带给页面（InputEvent 里没有滚动事件）。
        subscriptions.push(cx.observe(&sql, |_, _, cx| cx.notify()));
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
            sessions_views: HashMap::new(),
            tabs: Vec::new(),
            tab_conn: HashMap::new(),
            tab_kind: HashMap::new(),
            tab_seq: 0,
            active_tab: None,
            active: None,
            mode: Mode::Query,
            instance_data: InstanceData::default(),
            metric_history: std::collections::HashMap::new(),
            hook_log: std::collections::VecDeque::new(),
            selected_table: None,
            table_page: 0,
            table_rows: None,
            table_sort: None,
            query_result: None,
            query_target: None,
            query_pk: Vec::new(),
            query_sql: None,
            query_elapsed_ms: None,
            query_loaded: 0,
            query_has_more: false,
            plan_view: None,
            plan_next: false,
            browse_open_pending: false,
            plan_flat: false,
            column_widths: Vec::new(),
            result_x_scroll: ScrollHandle::new(),
            selected_row: None,
            checked_rows: BTreeSet::new(),
            batch_column: None,
            batch_value: text_input(window, cx, tr(cx, "要写入该列的新值")),
            batch_null: false,
            pending_edits: Vec::new(),
            pending_inserts: Vec::new(),
            pending_undo: Vec::new(),
            pending_redo: Vec::new(),
            table_filters: Vec::new(),
            filter_column: None,
            filter_op: FilterOp::Eq,
            filter_value: text_input(window, cx, tr(cx, "过滤值…")),
            table_pk: Vec::new(),
            table_ddl: None,
            table_indexes: Vec::new(),
            table_row_count: None,
            table_columns: Vec::new(),
            table_foreign_keys: Vec::new(),
            table_triggers: Vec::new(),
            table_meta_for: None,
            row_editor: None,
            chart_view: None,
            history: load_history(cx),
            saved_queries: load_saved_queries(cx),
            saved_charts: load_saved_charts(cx),
            dashboards: load_dashboards(cx),
            tunnel_profiles: load_tunnel_profiles(cx),
            dashboard_results: HashMap::new(),
            dashboard_collapsed: BTreeSet::new(),
            dashboard_editing: false,
            history_open: false,
            ai_open: false,
            ai_provider: None,
            ai_question: text_input(window, cx, tr(cx, "用自然语言描述要查什么…")),
            ai_explain: None,
            last_error: None,
            status: tr(cx, "就绪").into(),
            busy: false,
            running: None,
            stop_requested: false,
            run_timeout_secs: QUERY_TIMEOUT.as_secs(),
            run_seq: 0,
            query_timeout_override: None,
            query_timeout_input: text_input(window, cx, tr(cx, "留空跟随连接默认")),
            tree_open: std::collections::HashSet::new(),
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
        let form = EditFormView::new(
            self.view.clone(),
            kind,
            self.tunnel_profiles.clone(),
            window,
            cx,
        );
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
    /// 只收「表单已经读好」的两件套：配置 + 口令（口令的取值与回落语义在
    /// [`EditFormView::prepared`] 里，测试与保存共用同一套）。
    fn spawn_probe(&mut self, cfg: ConnectionConfig, creds: Credentials, cx: &mut Context<Self>) {
        self.status = trf(cx, "正在测试 {}…", &[&cfg.display_name()]).into();
        cx.notify();
        // 收工回调里要报名字与 id，而 `cfg` 已经进了后台闭包。
        let done_cfg = cfg.clone();
        self.spawn_db(
            cx,
            move || probe_connection(&cfg, &creds),
            // `move`：回调要把 `done_cfg` 一起带走（它比这次调用活得久）。
            // ⚠️ 下面每个 `record_event` 的明细都先落在局部变量里再传：参数从左往右求值，
            // 写成 `record_event(cx, …, trf(cx, …))` 会「先可变借、再不可变借」同一个 `cx`。
            move |this, result, window, cx| match result {
                Ok(version) => {
                    // 探活也要留痕：它是唯一一条「不建会话就碰了服务器」的路径，
                    // 出问题时这段历史里没有别的线索。
                    let detail = trf(cx, "测试连接 {}：{}", &[&done_cfg.display_name(), &version]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Connection,
                        crate::audit::action::PROBE,
                        crate::audit::AuditOutcome::Ok,
                        &done_cfg.id,
                        detail,
                    );
                    this.status = trf(cx, "测试成功：{}", &[&version]).into();
                    notify::success(trf(cx, "连接成功：{}", &[&version]), window, cx);
                }
                Err(error) => {
                    // 「断在哪一层」由 `StageError` 决定（通道 / 连接），类别与动作键跟着走。
                    let kind = error.kind();
                    let layer = failure_layer(&error, &done_cfg, cx);
                    let detail = trf(
                        cx,
                        "测试连接 {}（{}）失败：{}",
                        &[&done_cfg.display_name(), &layer, &error.error().to_string()],
                    );
                    let reason = error.to_string();
                    let trace = crate::audit::record_event(
                        cx,
                        kind,
                        stage_action(kind),
                        crate::audit::AuditOutcome::Failed,
                        &done_cfg.id,
                        detail,
                    );
                    this.status = trf(cx, "测试失败：{}", &[&reason]).into();
                    notify::error(
                        failure_message(cx, error.label(cx), trace.as_deref(), &reason),
                        window,
                        cx,
                    );
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
        // 顺序要紧：`close_tab` 要从档案里取这条连接的**断开钩子**，档案必须先还在
        // （删档之后再拆会话，钩子就查不到了，等于静默跳过用户配好的收尾动作）。
        self.close_tab(id, window, cx);
        self.connections.retain(|cfg| cfg.id != id);
        self.persist_connections(cx);
        let _ = delete_password(id);
        let _ = delete_ssh_password(id);
        let _ = delete_proxy_password(id);
        notify::success(tr(cx, "连接已删除。"), window, cx);
    }

    /// 把档案数组写回 kv。
    fn persist_connections(&self, cx: &App) {
        match serde_json::to_string(&self.connections) {
            Ok(json) => duck_store::kv_put(cx, KEY_CONNECTIONS, json),
            Err(error) => tracing::warn!(%error, "serialize db connections failed"),
        }
    }

    /// 把传输档案数组写回 kv。
    fn persist_tunnel_profiles(&self, cx: &App) {
        match serde_json::to_string(&self.tunnel_profiles) {
            Ok(json) => duck_store::kv_put(cx, KEY_TUNNEL_PROFILES, json),
            Err(error) => tracing::warn!(%error, "serialize tunnel profiles failed"),
        }
    }

    /// 新增 / 覆盖一份传输档案（同 id 覆盖，否则追加）。返回档案 id。
    ///
    /// 交由页面来做而不是表单自己存：档案与凭据是**页面级**的东西（一份档案被多条连接
    /// 引用），表单只是它的一个编辑入口。
    fn upsert_tunnel_profile(&mut self, profile: TunnelProfile, cx: &mut Context<Self>) -> String {
        let id = profile.id.clone();
        match self
            .tunnel_profiles
            .iter_mut()
            .find(|existing| existing.id == id)
        {
            Some(existing) => *existing = profile,
            None => self.tunnel_profiles.push(profile),
        }
        self.persist_tunnel_profiles(cx);
        cx.notify();
        id
    }

    /// 删一份传输档案，**并清掉引用它的连接上的引用**。
    ///
    /// 不清引用的话，那些连接会一直指着一个不存在的 id（建连时回落到自带三档，
    /// 而用户以为自己在走档案）。一并清掉，界面上就回到「自定义」这个诚实的状态。
    fn delete_tunnel_profile(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        self.tunnel_profiles
            .retain(|profile| profile.id != profile_id);
        self.persist_tunnel_profiles(cx);
        let mut touched = false;
        for cfg in &mut self.connections {
            if cfg.tunnel_profile.as_deref() == Some(profile_id) {
                cfg.tunnel_profile = None;
                touched = true;
            }
        }
        if touched {
            self.persist_connections(cx);
        }
        let _ = delete_profile_ssh_password(profile_id);
        let _ = delete_profile_proxy_password(profile_id);
        cx.notify();
    }

    /// 引用某份传输档案的连接条数（删档案前告知影响面）。
    fn tunnel_profile_refs(&self, profile_id: &str) -> usize {
        self.connections
            .iter()
            .filter(|cfg| cfg.tunnel_profile.as_deref() == Some(profile_id))
            .count()
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

    /// 把已保存查询写回 kv（单键结构化数组，范式同连接档案）。
    fn persist_saved_queries(&self, cx: &App) {
        match serde_json::to_string(&self.saved_queries) {
            Ok(json) => duck_store::kv_put(cx, KEY_SAVED_QUERIES, json),
            Err(error) => tracing::warn!(%error, "serialize db saved queries failed"),
        }
    }

    /// 把编辑器里的 SQL 存成一条已保存查询（打开命名弹窗）。
    fn open_save_query_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql.read(cx).value().trim().to_owned();
        if sql.is_empty() {
            notify::info(tr(cx, "编辑器里还没有 SQL。"), window, cx);
            return;
        }
        // 默认名按内容给一个能改的占位（不是「未命名」这种空话）：取首行前若干字符。
        let suggestion: String = sql
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(24)
            .collect();
        self.open_query_name_dialog(None, suggestion, sql, window, cx);
    }

    /// 给一条已保存查询改名（左栏右键「重命名」）。
    fn open_rename_query_dialog(
        &mut self,
        id: &str,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.saved_queries.iter().find(|saved| saved.id == id) else {
            return;
        };
        let sql = saved.sql.clone();
        self.open_query_name_dialog(Some(id.to_owned()), current.to_owned(), sql, window, cx);
    }

    /// 「命名查询」弹窗（新建与重命名共用；`target` 为 `None` 即新建）。
    fn open_query_name_dialog(
        &mut self,
        target: Option<String>,
        initial: String,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, "查询名称"));
        // 重命名时把原名填进去：清空重打一遍没有道理，改一个字符才是常态。
        if !initial.is_empty() {
            let seed = initial.clone();
            input.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    notify::warning(tr(cx, "请填写查询名称。"), window, cx);
                    return false;
                }
                let sql = sql.clone();
                let target = target.clone();
                let _ = view.update(cx, |this, cx| {
                    match target {
                        // 改名：只动名字，SQL 正文保持原样（用户可能已经改过编辑器里的内容，
                        // 「重命名」的语义不该顺手覆盖正文）。
                        Some(id) => {
                            if let Some(saved) =
                                this.saved_queries.iter_mut().find(|saved| saved.id == id)
                            {
                                saved.name = name;
                            }
                        }
                        None => {
                            let id = SavedQuery::next_id(&this.saved_queries);
                            this.saved_queries.push(SavedQuery { id, name, sql });
                        }
                    }
                    this.persist_saved_queries(cx);
                    this.status = tr(cx, "已保存查询。").into();
                    cx.notify();
                });
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
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
                                "db-saved-query-title",
                                &drag_for_content,
                                tr(cx, "保存查询"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "名称"), &input_for_content)),
                    ))
                })
                // 不带分号：闭包必须返回 `Dialog`。
                .footer(dialog_footer(
                    "db-saved-query",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 把一条已保存查询载入编辑器（左栏点击 / 右键「载入到编辑器」）。
    ///
    /// 只**填**编辑器，不执行：载入之后要不要跑是用户的决定（与「生成 DDL 骨架」
    /// 同口径——写动作都要过人工审批这一关）。
    fn load_saved_query(
        &mut self,
        sql: String,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fill_editor(sql, window, cx);
        self.status = trf(cx, "已载入查询「{}」。", &[name]).into();
        cx.notify();
    }

    /// 把一段 SQL 填进编辑器并切回查询模式（**不执行**）。
    ///
    /// 「载入已保存查询」与「从历史放回编辑器」共用这一步：填进去但不跑是
    /// 同一条口径，状态栏那句话各自不同。
    fn fill_editor(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        self.sql.update(cx, |state, cx| {
            state.set_value(sql, window, cx);
        });
        self.mode = Mode::Query;
        cx.notify();
    }

    /// 删除一条已保存查询（带二次确认：删了取不回来）。
    fn delete_saved_query(
        &mut self,
        id: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmed = duck_platform::confirm(
            tr(cx, "删除已保存查询"),
            &trf(cx, "「{}」将被删除，确定继续吗？", &[name]),
        );
        if !confirmed {
            return;
        }
        self.saved_queries.retain(|saved| saved.id != id);
        self.persist_saved_queries(cx);
        self.status = tr(cx, "已删除。").into();
        notify::success(tr(cx, "已删除该查询。"), window, cx);
        cx.notify();
    }

    /// 把已保存图表写回 kv（单键结构化数组）。
    fn persist_saved_charts(&self, cx: &App) {
        match serde_json::to_string(&self.saved_charts) {
            Ok(json) => duck_store::kv_put(cx, KEY_SAVED_CHARTS, json),
            Err(error) => tracing::warn!(%error, "serialize db saved charts failed"),
        }
    }

    /// 把用户仪表板写回 kv（单键结构化数组）。
    fn persist_dashboards(&self, cx: &App) {
        match serde_json::to_string(&self.dashboards) {
            Ok(json) => duck_store::kv_put(cx, KEY_DASHBOARDS, json),
            Err(error) => tracing::warn!(%error, "serialize db dashboards failed"),
        }
    }

    /// 在表格与图表之间切换结果区。
    ///
    /// 第一次切进图表时按结果集**自动选列**（见 [`ChartConfig::from_result`]）；没有
    /// 可画的数值列就如实说不画，而不是切过去给一张空图。切回表格保留配置（下次切回
    /// 来还是同一张图），配置随 [`SessionView`] 按标签走。
    fn toggle_chart_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chart_view.is_some() {
            self.chart_view = None;
            cx.notify();
            return;
        }
        let Some(result) = self.current_result() else {
            notify::info(tr(cx, "当前没有可画图的结果。"), window, cx);
            return;
        };
        let Some(config) = ChartConfig::from_result(result) else {
            notify::info(tr(cx, "这份结果里没有数值列，画不出图。"), window, cx);
            return;
        };
        self.chart_view = Some(config);
        cx.notify();
    }

    /// 改图表类型（保列映射；饼图只留第一条序列）。
    fn set_chart_kind(&mut self, kind: ChartKind, cx: &mut Context<Self>) {
        if let Some(config) = self.chart_view.as_mut() {
            config.kind = kind;
            let limit = kind.series_limit();
            config.y.truncate(config.y.len().min(limit));
            cx.notify();
        }
    }

    /// 改横轴列。
    ///
    /// 同一列若已经在序列里就把它摘掉（「轴 = 值」画出来是一条对角线，没有意义），
    /// 但**至少留一条序列**：摘光了这份配置就失效，`render_chart` 会整份回落到默认映射，
    /// 用户刚选的轴会被一起抹掉——那比多画一条对角线更难理解。
    fn set_chart_axis(&mut self, column: String, cx: &mut Context<Self>) {
        if let Some(config) = self.chart_view.as_mut() {
            if config.y.len() > 1 {
                config.y.retain(|name| name != &column);
            }
            config.x = column;
            cx.notify();
        }
    }

    /// 切换某个数值列的勾选（序列选择）。
    ///
    /// 三种情形分开：已选 → 摘掉；单选类（饼图）没选 → **换成这一列**；多选类到顶 →
    /// 不动（界面把到顶的未选项画成停用态，用户看得见为什么点不动）。
    fn toggle_chart_series(&mut self, column: &str, cx: &mut Context<Self>) {
        let Some(config) = self.chart_view.as_mut() else {
            return;
        };
        let capacity = config.kind.series_limit();
        if let Some(index) = config.y.iter().position(|name| name == column) {
            config.y.remove(index);
        } else if capacity == 1 {
            config.y = vec![column.to_owned()];
        } else if config.y.len() < capacity {
            config.y.push(column.to_owned());
        } else {
            return;
        }
        cx.notify();
    }

    /// 把当前图表（SQL + 列映射 + 类型）存成一条已保存图表（打开命名弹窗）。
    fn open_save_chart_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.chart_view.clone() else {
            notify::info(tr(cx, "先切到图表视角再保存。"), window, cx);
            return;
        };
        let sql = self.sql.read(cx).value().trim().to_owned();
        if sql.is_empty() {
            notify::info(tr(cx, "编辑器里还没有 SQL。"), window, cx);
            return;
        }
        // 默认名取首行前若干字符（与保存查询同口径：给一个能改的占位，不是「未命名」）。
        let suggestion: String = sql
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(24)
            .collect();
        self.open_chart_name_dialog(None, suggestion, sql, config, window, cx);
    }

    /// 给一条已保存图表改名（左栏右键「重命名」）。
    fn open_rename_chart_dialog(
        &mut self,
        id: &str,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.saved_charts.iter().find(|saved| saved.id == id) else {
            return;
        };
        let sql = saved.sql.clone();
        let config = saved.config();
        self.open_chart_name_dialog(
            Some(id.to_owned()),
            current.to_owned(),
            sql,
            config,
            window,
            cx,
        );
    }

    /// 「命名图表」弹窗（新建与重命名共用；`target` 为 `None` 即新建）。
    fn open_chart_name_dialog(
        &mut self,
        target: Option<String>,
        initial: String,
        sql: String,
        config: ChartConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, "图表名称"));
        if !initial.is_empty() {
            let seed = initial.clone();
            input.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    notify::warning(tr(cx, "请填写图表名称。"), window, cx);
                    return false;
                }
                let sql = sql.clone();
                let config = config.clone();
                let target = target.clone();
                let _ = view.update(cx, |this, cx| {
                    match target {
                        // 改名：只动名字（与「重命名查询」同口径，不覆盖列映射）。
                        Some(id) => {
                            if let Some(saved) =
                                this.saved_charts.iter_mut().find(|saved| saved.id == id)
                            {
                                saved.name = name;
                            }
                        }
                        None => {
                            let id = SavedChart::next_id(&this.saved_charts);
                            this.saved_charts.push(SavedChart {
                                id,
                                name,
                                sql,
                                kind: config.kind,
                                x: config.x,
                                y: config.y,
                            });
                        }
                    }
                    this.persist_saved_charts(cx);
                    this.status = tr(cx, "已保存图表。").into();
                    cx.notify();
                });
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
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
                                "db-saved-chart-title",
                                &drag_for_content,
                                tr(cx, "保存图表"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "名称"), &input_for_content)),
                    ))
                })
                .footer(dialog_footer(
                    "db-saved-chart",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 载入一条已保存图表：SQL 放回编辑器（**不执行**）+ 切到图表视角用同一份配置。
    ///
    /// 与已保存查询的差别只有「顺手把图表视角打开」。SQL 放回但不跑，所以图表要等到
    /// 用户执行之后才有数据——那时配置已经就位，直接出图。
    fn load_saved_chart(
        &mut self,
        saved_id: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.saved_charts.iter().find(|saved| saved.id == saved_id) else {
            return;
        };
        let sql = saved.sql.clone();
        let config = saved.config();
        self.chart_view = Some(config);
        self.fill_editor(sql, window, cx);
        self.status = trf(cx, "已载入图表「{}」，执行后即可看图。", &[name]).into();
        cx.notify();
    }

    /// 删除一条已保存图表（带二次确认：删了取不回来）。
    fn delete_saved_chart(
        &mut self,
        id: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmed = duck_platform::confirm(
            tr(cx, "删除已保存图表"),
            &trf(cx, "「{}」将被删除，确定继续吗？", &[name]),
        );
        if !confirmed {
            return;
        }
        self.saved_charts.retain(|saved| saved.id != id);
        self.persist_saved_charts(cx);
        self.status = tr(cx, "已删除。").into();
        notify::success(tr(cx, "已删除该图表。"), window, cx);
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 用户仪表板（12 栏网格）。
    // ------------------------------------------------------------------

    /// 打开一块仪表板：切模式、收工结果区状态、拉一遍面板数据。
    ///
    /// 进来一律是**查看态**（对齐 DBFlux）：仪表板多半是回来看的，先看；想改再按编辑。
    fn open_dashboard(&mut self, id: u32, cx: &mut Context<Self>) {
        if !self.dashboards.iter().any(|board| board.id == id) {
            return;
        }
        self.mode = Mode::Dashboard(id);
        // 与「切实例页」同口径：离开结果区，选中行 / 暂存改动 / 图表视角一并作废。
        self.selected_row = None;
        self.row_editor = None;
        self.chart_view = None;
        self.dashboard_editing = false;
        self.refresh_dashboard(cx);
    }

    /// 重取当前仪表板上所有要服务器数据的面板。
    ///
    /// 一次 `spawn_db` 跑完所有面板（不是一板一枪）：连接后面是一把锁，并发发枪只会
    /// 排在锁上；攒成一批还能让「引用的图表已被删除」这种**当场可判**的结果和
    /// 真取数的结果一起落位，界面只会闪一次。
    fn refresh_dashboard(&mut self, cx: &mut Context<Self>) {
        let Mode::Dashboard(board_id) = self.mode else {
            return;
        };
        let Some(conn_id) = self.active.clone() else {
            return;
        };
        let Some(board) = self
            .dashboards
            .iter()
            .find(|board| board.id == board_id)
            .cloned()
        else {
            return;
        };
        // 这块板在这个连接上的旧结果先清掉：面板删了 / 换了内容之后不该留着上一份。
        self.dashboard_results
            .retain(|(conn, dash, _), _| conn != &conn_id || *dash != board_id);

        let Some(session) = self.sessions.get(&conn_id) else {
            return;
        };
        let kind = session.kind;
        let conn = session.conn.clone();
        // 折线 / 柱状这些面板的 SQL 是**替用户跑**的，该过只读闸门。但
        // [`duck_db::sqlintel::prove_read_only`] 靠解析 SQL，MongoDB / Elasticsearch 的
        // shell / DSL 语句永远证明不了——一律拒绝等于让这两条引擎的图表面板彻底不能用。
        // 判据复用 `identifier_quote()`（它就是「这条引擎接不接 SQL」的同源口径），
        // 非 SQL 引擎退回与编辑器手动执行**同一条** `execute` 路径：面板的语句本就是
        // 用户自己存下来的，不存在"替用户写库"。
        let sql_engine = kind.identifier_quote().is_some();

        // 先把「要干什么」算成清单：后台闭包不该再读界面状态。
        let mut jobs: Vec<(u32, DashboardJob)> = Vec::new();
        for panel in &board.panels {
            match &panel.kind {
                DashboardPanelKind::Chart { saved_chart_id } => {
                    match self
                        .saved_charts
                        .iter()
                        .find(|chart| &chart.id == saved_chart_id)
                    {
                        Some(chart) => jobs.push((panel.id, DashboardJob::Sql(chart.sql.clone()))),
                        None => {
                            self.dashboard_results.insert(
                                (conn_id.clone(), board_id, panel.id),
                                PanelFetch::MissingChart,
                            );
                        }
                    }
                }
                DashboardPanelKind::Inspector { view } => {
                    jobs.push((panel.id, DashboardJob::Inspector(view.view())));
                }
                DashboardPanelKind::Divider { .. } => {}
            }
        }
        if jobs.is_empty() {
            cx.notify();
            return;
        }
        let board_name = board.name.clone();
        self.spawn_db(
            cx,
            move || -> Vec<(u32, PanelFetch)> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                jobs.into_iter()
                    .map(|(panel_id, job)| {
                        let fetch = match job {
                            DashboardJob::Sql(sql) => {
                                let outcome = if sql_engine {
                                    guard.execute_read_only(&sql, DASHBOARD_ROW_LIMIT)
                                } else {
                                    guard.execute(&sql, DASHBOARD_ROW_LIMIT)
                                };
                                match outcome {
                                    Ok(outcome) => match outcome.result {
                                        Some(result) => PanelFetch::Chart(result),
                                        None => PanelFetch::NoRows,
                                    },
                                    Err(error) => PanelFetch::Failed(error.to_string()),
                                }
                            }
                            DashboardJob::Inspector(view) => match guard.instance_table(view) {
                                Ok(table) => PanelFetch::Table(table),
                                Err(error) => PanelFetch::Failed(error.to_string()),
                            },
                        };
                        (panel_id, fetch)
                    })
                    .collect()
            },
            move |this, fetches, _window, cx| {
                for (panel_id, fetch) in fetches {
                    this.dashboard_results
                        .insert((conn_id.clone(), board_id, panel_id), fetch);
                }
                this.status = trf(cx, "已刷新仪表板「{}」。", &[&board_name]).into();
                cx.notify();
            },
        );
    }

    /// 把一块面板的改动落盘（所有面板级编辑动作共用的收尾）。
    fn commit_dashboard(&self, cx: &App) {
        self.persist_dashboards(cx);
    }

    /// 找一块面板。
    fn dashboard_panel_mut(&mut self, board_id: u32, panel_id: u32) -> Option<&mut DashboardPanel> {
        self.dashboards
            .iter_mut()
            .find(|board| board.id == board_id)?
            .panels
            .iter_mut()
            .find(|panel| panel.id == panel_id)
    }

    /// 往当前仪表板加一块面板（返回新面板 id，调用方接着做「改标题」这类一步动作）。
    ///
    /// 分隔条一律整行（见 [`dashboard_panel_span`]），所以它的 `span` 直接存满栏 ——
    /// 存成别的值也不会生效，不如存成真实生效的那个，kv 读出来不会自相矛盾。
    fn add_dashboard_panel(
        &mut self,
        board_id: u32,
        kind: DashboardPanelKind,
        cx: &mut Context<Self>,
    ) -> Option<u32> {
        let span = match kind {
            DashboardPanelKind::Divider { .. } => DASHBOARD_COLUMNS,
            _ => DASHBOARD_DEFAULT_SPAN,
        };
        let board = self
            .dashboards
            .iter_mut()
            .find(|board| board.id == board_id)?;
        let id = board.next_panel_id();
        board.panels.push(DashboardPanel { id, span, kind });
        self.commit_dashboard(cx);
        // 新面板的数据得先取到手（图表面板 / 检查器面板），否则摆上去是一块空壳。
        self.refresh_dashboard(cx);
        cx.notify();
        Some(id)
    }

    /// 移除一块面板。
    fn remove_dashboard_panel(&mut self, board_id: u32, panel_id: u32, cx: &mut Context<Self>) {
        if let Some(board) = self
            .dashboards
            .iter_mut()
            .find(|board| board.id == board_id)
        {
            board.panels.retain(|panel| panel.id != panel_id);
        }
        // 折叠状态与取回的数据跟着走：面板没了，这两份也不该留。
        self.dashboard_collapsed.remove(&panel_id);
        self.dashboard_results
            .retain(|(_, dash, panel), _| *dash != board_id || *panel != panel_id);
        self.commit_dashboard(cx);
        cx.notify();
    }

    /// 改一块面板的宽度（`delta` 栏；收口在 1..=12）。
    fn set_dashboard_span(
        &mut self,
        board_id: u32,
        panel_id: u32,
        span: u32,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.dashboard_panel_mut(board_id, panel_id) {
            panel.span = clamp_span(span);
        }
        self.commit_dashboard(cx);
        cx.notify();
    }

    /// 前挪 / 后挪一块面板（12 栏流式网格里「左右上下」是同一件事）。
    fn move_dashboard_panel(
        &mut self,
        board_id: u32,
        panel_id: u32,
        delta: i32,
        cx: &mut Context<Self>,
    ) {
        let Some(board) = self
            .dashboards
            .iter_mut()
            .find(|board| board.id == board_id)
        else {
            return;
        };
        let Some(index) = board.panels.iter().position(|panel| panel.id == panel_id) else {
            return;
        };
        board.move_panel(index, delta);
        self.commit_dashboard(cx);
        cx.notify();
    }

    /// 换掉图表面板引用的已保存图表。
    fn set_dashboard_chart(
        &mut self,
        board_id: u32,
        panel_id: u32,
        chart_id: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.dashboard_panel_mut(board_id, panel_id) {
            panel.kind = DashboardPanelKind::Chart {
                saved_chart_id: chart_id,
            };
        }
        self.commit_dashboard(cx);
        // 整板重取（而不是只重取这一块）：连接后面是一把锁，多跑一次往返换不来什么，
        // 而「哪块数据是哪次取的」只留一种情况，界面不会出现新旧混排。
        self.refresh_dashboard(cx);
        cx.notify();
    }

    /// 换掉检查器面板看的那张表。
    fn set_dashboard_inspector(
        &mut self,
        board_id: u32,
        panel_id: u32,
        view: InspectorView,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.dashboard_panel_mut(board_id, panel_id) {
            panel.kind = DashboardPanelKind::Inspector { view };
        }
        self.commit_dashboard(cx);
        self.refresh_dashboard(cx);
        cx.notify();
    }

    /// 折叠 / 展开一条分隔标题（它统领它下面那一段）。
    fn toggle_dashboard_divider(&mut self, panel_id: u32, cx: &mut Context<Self>) {
        if !self.dashboard_collapsed.insert(panel_id) {
            self.dashboard_collapsed.remove(&panel_id);
        }
        cx.notify();
    }

    /// 新建 / 重命名仪表板（`None` = 新建，`Some` = 改名）。
    fn open_dashboard_name_dialog(
        &mut self,
        target: Option<u32>,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, "仪表板名称"));
        if !initial.is_empty() {
            let seed = initial.clone();
            input.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    notify::warning(tr(cx, "请填写仪表板名称。"), window, cx);
                    return false;
                }
                // `target` 是 `Option<u32>`（`Copy`），闭包是 `Fn` 也能直接读。
                let mut created = None;
                let _ = view.update(cx, |this, cx| {
                    match target {
                        // 改名：只动名字（面板与取回的数据都不动）。
                        Some(id) => {
                            if let Some(board) =
                                this.dashboards.iter_mut().find(|board| board.id == id)
                            {
                                board.name = name;
                            }
                        }
                        None => {
                            let id = Dashboard::next_id(&this.dashboards);
                            this.dashboards.push(Dashboard {
                                id,
                                name,
                                panels: Vec::new(),
                            });
                            created = Some(id);
                        }
                    }
                    this.commit_dashboard(cx);
                    this.status = tr(cx, "已保存仪表板。").into();
                    cx.notify();
                });
                // 新建的当场打开：用户按「新建」的意思是「现在就开始摆」。
                if let Some(id) = created {
                    let _ = view.update(cx, |this, cx| this.open_dashboard(id, cx));
                }
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
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
                                "db-dashboard-title",
                                &drag_for_content,
                                tr(cx, "仪表板名称"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "名称"), &input_for_content)),
                    ))
                })
                .footer(dialog_footer(
                    "db-dashboard-name",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 改分隔标题的正文（分隔条的内容就一行字，也走弹窗以免和面板里的按钮抢焦点）。
    fn open_divider_title_dialog(
        &mut self,
        board_id: u32,
        panel_id: u32,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, "分隔标题"));
        if !initial.is_empty() {
            let seed = initial.clone();
            input.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let title = input.read(cx).value().trim().to_owned();
                if title.is_empty() {
                    notify::warning(tr(cx, "请填写分隔标题。"), window, cx);
                    return false;
                }
                let _ = view.update(cx, |this, cx| {
                    if let Some(panel) = this.dashboard_panel_mut(board_id, panel_id) {
                        panel.kind = DashboardPanelKind::Divider { title };
                    }
                    this.commit_dashboard(cx);
                    this.status = tr(cx, "已保存分隔标题。").into();
                    cx.notify();
                });
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
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
                                "db-divider-title",
                                &drag_for_content,
                                tr(cx, "分隔标题"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "标题"), &input_for_content)),
                    ))
                })
                .footer(dialog_footer(
                    "db-divider-name",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 复制一块仪表板（面板照搬，名字加「副本」）。
    fn duplicate_dashboard(&mut self, id: u32, cx: &mut Context<Self>) {
        let Some(source) = self.dashboards.iter().find(|board| board.id == id).cloned() else {
            return;
        };
        let new_id = Dashboard::next_id(&self.dashboards);
        let name = trf(cx, "{} 副本", &[&source.name]);
        self.dashboards.push(source.duplicate(new_id, name));
        self.persist_dashboards(cx);
        self.status = tr(cx, "已复制仪表板。").into();
        cx.notify();
    }

    /// 删除一块仪表板（正在看这块的话退回 SQL 结果区）。
    fn delete_dashboard(
        &mut self,
        id: u32,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmed = duck_platform::confirm(
            tr(cx, "删除仪表板"),
            &trf(cx, "「{}」将被删除，确定继续吗？", &[name]),
        );
        if !confirmed {
            return;
        }
        let panels: Vec<u32> = self
            .dashboards
            .iter()
            .find(|board| board.id == id)
            .map(|board| board.panels.iter().map(|panel| panel.id).collect())
            .unwrap_or_default();
        self.dashboards.retain(|board| board.id != id);
        for panel in panels {
            self.dashboard_collapsed.remove(&panel);
        }
        self.dashboard_results.retain(|(_, dash, _), _| *dash != id);
        // 正在看这块：主区不能留在一个已经不存在的 id 上。
        if self.mode == Mode::Dashboard(id) {
            self.mode = Mode::Query;
            self.dashboard_editing = false;
        }
        self.persist_dashboards(cx);
        self.status = tr(cx, "已删除。").into();
        notify::success(tr(cx, "已删除该仪表板。"), window, cx);
        cx.notify();
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

    /// 打开实例信息页（左栏连接节点下那四个入口）。
    ///
    /// 取数策略：**点开才取**，不在建连时预取——建连那一刻用户要的是「有哪些库」，
    /// 让他多等三条服务器探询语句没有道理。已取过的档位不重复取（`refresh` 会重取）。
    fn open_instance_tab(
        &mut self,
        tab: InstanceTab,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active.clone() else {
            return;
        };
        self.mode = Mode::Instance(tab);
        // 切到实例页 = 离开结果区：选中行 / 暂存改动跟着作废（与换库同口径）。
        self.selected_row = None;
        self.row_editor = None;
        let cached = match tab {
            InstanceTab::Dashboard => true,
            InstanceTab::Overview => self.instance_data.overview.is_some(),
            InstanceTab::Metrics => self.instance_data.metrics.is_some(),
            InstanceTab::Inspector => self.instance_data.processes.is_some(),
        };
        if cached || !tab.fetches_server() {
            cx.notify();
            return;
        }
        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        let conn = session.conn.clone();
        // 历史按连接存：回投闭包里要用，先克隆一份键。
        let history_key = id.clone();
        self.status = trf(cx, "正在读取{}…", &[tab.heading(cx)]).into();
        self.spawn_db(
            cx,
            move || -> Result<InstancePayload, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                Ok(match tab {
                    InstanceTab::Overview => InstancePayload::Overview(guard.instance_overview()?),
                    InstanceTab::Metrics => {
                        let table = guard.instance_table(InstanceView::Metrics)?;
                        // 数值序列从**同一张表**筛出来：指标白名单只有引擎侧一份。
                        let points = table.numeric();
                        InstancePayload::Metrics(table, points)
                    }
                    InstanceTab::Inspector => {
                        InstancePayload::Processes(guard.instance_table(InstanceView::Processes)?)
                    }
                    // 仪表盘不取服务器数据（`fetches_server()` 已挡在前面）。
                    InstanceTab::Dashboard => InstancePayload::Overview(Default::default()),
                })
            },
            move |this, result, _window, cx| match result {
                Ok(payload) => {
                    match payload {
                        InstancePayload::Overview(overview) => {
                            this.instance_data.overview = Some(overview);
                        }
                        InstancePayload::Metrics(table, points) => {
                            this.instance_data.metrics = Some(table);
                            // 每取一轮 = 一次采样。缓冲按连接累积（切库 / 换标签不打断），
                            // 断开连接时随会话一起丢（`close_tab`）。
                            this.metric_history
                                .entry(history_key)
                                .or_default()
                                .push(now_secs(), points);
                        }
                        InstancePayload::Processes(table) => {
                            this.instance_data.processes = Some(table);
                        }
                    }
                    this.status = trf(cx, "已读取{}。", &[tab.heading(cx)]).into();
                }
                Err(error) => {
                    this.status = trf(cx, "读取失败：{}", &[&error.to_string()]).into();
                }
            },
        );
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
            move || -> Result<Introspection, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                Introspection::load(&mut guard, schema.as_deref())
            },
            |this, result, _window, cx| match result {
                Ok(introspection) => {
                    if let Some(session) = this.active_session_mut() {
                        introspection.install(session);
                    }
                    this.status = tr(cx, "已刷新。").into();
                }
                Err(error) => {
                    this.status = trf(cx, "刷新失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 切换**当前库**（左栏库列表 / 右栏下拉在「库即命名空间」的引擎上走这里）。
    ///
    /// 与 [`Self::select_schema`] 的关键差别：这里会**真的切会话**——
    /// MySQL / ClickHouse / SQL Server / Mongo 各自发 `USE` / 改端点 / 记落点，
    /// PostgreSQL 回 `NeedsReconnect` 后由 [`Self::connect_entry_with_database`] 重连。
    ///
    /// 原先只把库名记在界面上（`set_namespace` 对 SQL 引擎是空操作），于是库名留空
    /// 建连的用户点了库、也看到了表，一跑 `SELECT` 却被服务端回 `No database selected`
    /// —— 界面「切了」、查询没切，正是「和 dbx 不一样」的那一处。
    fn select_database(&mut self, name: String, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        // 借用到这里就结束：下面要 `&mut self` 清状态、写档案，不能拖着 `session` 的
        // 不可变借用（E0502）。
        let (conn, already_there) = {
            let Some(session) = self.sessions.get(&id) else {
                return;
            };
            (
                session.conn.clone(),
                session.database.as_deref() == Some(name.as_str()),
            )
        };
        if already_there {
            return;
        }
        // 换库 = 换整套对象与列：选中表、勾选、暂存改动全部作废（与换 schema 同口径）。
        self.selected_table = None;
        self.table_rows = None;
        self.table_page = 0;
        self.mode = Mode::Query;
        self.checked_rows.clear();
        self.clear_pending();
        self.row_editor = None;
        self.selected_row = None;
        self.table_meta_for = None;
        self.table_indexes.clear();
        self.table_row_count = None;
        self.table_ddl = None;
        self.update_connection_field(&id, cx, |cfg| cfg.database = name.clone());
        self.status = trf(cx, "正在切换数据库 {}…", &[&name]).into();
        cx.notify();

        let for_conn = name.clone();
        // 建连闭包要把库名移进去，回投闭包还要用 → 各持一份。
        let switching = name.clone();
        self.spawn_db(
            cx,
            move || -> Result<(SwitchOutcome, Vec<CatalogTable>), DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                let outcome = guard.switch_database(&switching)?;
                // 会话已经是新库了 → 目录按新库取（`None` 即「当前库」，省一次往返）。
                let catalog = match outcome {
                    SwitchOutcome::Applied => guard.catalog(None)?,
                    // 还没切过去（要重连），这份目录属于旧库，取了也不能用。
                    SwitchOutcome::NeedsReconnect => Vec::new(),
                };
                Ok((outcome, catalog))
            },
            move |this, result, window, cx| match result {
                Ok((SwitchOutcome::Applied, catalog)) => {
                    if let Some(session) = this.active_session_mut() {
                        session.catalog = catalog;
                        // 库即命名空间：目录来处就是刚切过去的那个库。
                        session.schema = Some(for_conn.clone());
                        session.database = Some(for_conn.clone());
                    }
                    this.status = trf(cx, "已切到数据库 {}", &[&for_conn]).into();
                }
                Ok((SwitchOutcome::NeedsReconnect, _)) => {
                    // PG：库是建连参数，只能重连。状态栏先说明白，别让人以为卡住了。
                    this.status = trf(cx, "正在重连到数据库 {}…", &[&for_conn]).into();
                    this.connect_entry_with_database(&id, Some(for_conn.clone()), window, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "切换数据库失败：{}", &[&error.to_string()]).into();
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
        self.clear_pending();
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
            move || -> Result<Introspection, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                // 先让**连接自己**记住落点再内省：MongoDB 的「当前库」是客户端概念，
                // 不告诉驱动的话，编辑器里的命令还会落在旧库（详见
                // `Connection::set_namespace`）。
                guard.set_namespace(for_conn.as_deref());
                Introspection::load(&mut guard, for_conn.as_deref())
            },
            move |this, result, _window, cx| match result {
                Ok(introspection) => {
                    if let Some(session) = this.active_session_mut() {
                        introspection.install(session);
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
    ///
    /// 带上 `database` 时按那个库建连（覆盖档案里的值，且不写档案）——
    /// 这是 PostgreSQL 切库的唯一通路：`dbname` 是建连参数，一个会话终身绑死，
    /// 想换库只能重连（见 [`duck_db::SwitchOutcome::NeedsReconnect`]）。
    fn connect_entry(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.connect_entry_with_database(id, None, window, cx);
    }

    fn connect_entry_with_database(
        &mut self,
        id: &str,
        database: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            notify::info(tr(cx, "有数据库操作正在进行，请稍候。"), window, cx);
            return;
        }
        let Some(cfg) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        // 引用的「传输档案」在**这一次建连**里展开成内联三档，**连同档案自己的凭据**
        // 一起换过去（档案是独立凭据主体，口令存在 `dbprofile:{id}:…`）。
        // 展开后走的是同一条 `open_transport`，档案对它是不存在的。
        let (cfg, creds) = resolve_transport(
            &cfg,
            &self.tunnel_profiles,
            Credentials::from_secret_store(&cfg.id),
        );
        // 只在**本次建连**里覆盖库名：档案不动（浏览动作不该悄悄改档案）。
        let cfg = match database {
            Some(database) => ConnectionConfig { database, ..cfg },
            None => cfg,
        };
        self.status = trf(cx, "正在连接 {}…", &[&cfg.display_name()]).into();
        cx.notify();
        let done_cfg = cfg.clone();
        self.spawn_db(
            cx,
            move || -> ConnectAttempt {
                let facts = hook_facts(&cfg);
                // 「建连前」在最前面：开 VPN / 切 DNS 这类前置动作都该发生在 socket 之前。
                let pre = run_phase(&cfg.hooks, HookPhase::PreConnect, &facts);
                log_hook_report(&cfg.id, HookPhase::PreConnect, &pre);
                if pre.blocker().is_some() {
                    // 明确阻断：**连都不连**（不是「连上了又关掉」——那会在服务端留一条认证记录）。
                    return ConnectAttempt {
                        pre,
                        post: HookReport::default(),
                        session: None,
                        error: None,
                    };
                }
                match open_session(&cfg, &creds) {
                    Ok(session) => {
                        // 「建连后」在目录都取完之后：此时这条连接**真的可用**了，
                        // 拿它做「注册到服务注册中心」「写一条审计」才有意义。
                        let post = run_phase(&cfg.hooks, HookPhase::PostConnect, &facts);
                        log_hook_report(&cfg.id, HookPhase::PostConnect, &post);
                        if post.blocker().is_some() {
                            // 阻断 = 这次「建连」整体不算数：把会话丢掉（socket 随之关闭）。
                            return ConnectAttempt {
                                pre,
                                post,
                                session: None,
                                error: None,
                            };
                        }
                        ConnectAttempt {
                            pre,
                            post,
                            session: Some(session),
                            error: None,
                        }
                    }
                    Err(error) => {
                        // 建连本身就失败了：不跑「建连后」钩子（没什么可注册的），
                        // 但「建连前」的结果照旧带回去给用户看。
                        ConnectAttempt {
                            pre,
                            post: HookReport::default(),
                            session: None,
                            error: Some(error),
                        }
                    }
                }
            },
            move |this, attempt, window, cx| {
                let cfg = &done_cfg;
                let ConnectAttempt {
                    pre,
                    post,
                    session,
                    error,
                } = attempt;
                // 先把「哪一段被钩子拦下了」记下来：报告随即被 `note_hook_report` 取走。
                let blocked = if pre.blocked.is_some() {
                    Some(HookPhase::PreConnect)
                } else if post.blocked.is_some() {
                    Some(HookPhase::PostConnect)
                } else {
                    None
                };
                this.note_hook_report(&cfg.id, pre, window, cx);
                this.note_hook_report(&cfg.id, post, window, cx);
                let Some(session) = session else {
                    // 「钩子挡下的」与「引擎连不上的」是两种处置（前者去改钩子，后者去查网络 /
                    // 凭据），提示里必须分得开 —— 所以先看是不是钩子拦的，再谈引擎错误。
                    let message = match (blocked, error) {
                        (Some(phase), _) => {
                            // 钩子阻断是**策略生效**，不是故障：记 `blocked` 而不是 `failed`
                            //（审计的「只看问题」会带上它，但结果列显示「已拦下」而不是「失败」）。
                            let reason = trf(
                                cx,
                                "{}的钩子未通过，已按配置中断连接。",
                                &[hook_phase_label(phase, cx)],
                            );
                            let detail = trf(cx, "连接 {}：{}", &[&cfg.display_name(), &reason]);
                            crate::audit::record_event(
                                cx,
                                crate::audit::AuditKind::Hook,
                                crate::audit::action::RUN_HOOK,
                                crate::audit::AuditOutcome::Blocked,
                                &cfg.id,
                                detail,
                            );
                            reason
                        }
                        (None, Some(error)) => {
                            let kind = error.kind();
                            let layer = failure_layer(&error, cfg, cx);
                            let detail = trf(
                                cx,
                                "连接 {}（{}）失败：{}",
                                &[&cfg.display_name(), &layer, &error.error().to_string()],
                            );
                            let reason = error.to_string();
                            let trace = crate::audit::record_event(
                                cx,
                                kind,
                                stage_action(kind),
                                crate::audit::AuditOutcome::Failed,
                                &cfg.id,
                                detail,
                            );
                            failure_message(cx, error.label(cx), trace.as_deref(), &reason)
                                .to_string()
                        }
                        (None, None) => {
                            let detail = trf(cx, "连接 {} 未能建立。", &[&cfg.display_name()]);
                            crate::audit::record_event(
                                cx,
                                crate::audit::AuditKind::Connection,
                                crate::audit::action::CONNECT,
                                crate::audit::AuditOutcome::Failed,
                                &cfg.id,
                                detail,
                            );
                            tr(cx, "连接失败。").to_owned()
                        }
                    };
                    this.status = SharedString::from(message.clone());
                    notify::error(SharedString::from(message), window, cx);
                    cx.notify();
                    return;
                };
                let id = cfg.id.clone();
                // 走了通道才记一条「通道已建立」：直连没有通道可记，而这一条正是
                // 「这条连接什么时候走的是隧道 / 走的是哪一档」的唯一来源。
                if let Some(transport) = session.transport.as_ref() {
                    let name = cfg.display_name();
                    let port = transport.local_port().to_string();
                    let kind_label = transport.label(cx);
                    let detail = trf(cx, "{} · {}（本地端口 {}）", &[kind_label, &name, &port]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Transport,
                        crate::audit::action::OPEN_TRANSPORT,
                        crate::audit::AuditOutcome::Ok,
                        &id,
                        detail,
                    );
                }
                this.sessions.insert(id.clone(), session);
                // 把这条会话挂进 AI 侧的只读目录，`run_sql_readonly` 据此按 id 找会话
                // （`docs/ai-design.md` §10.4）。目录键取自句柄自己的 id，保证两边
                // 不会因为来源不同而漂移；重连（同 id 再连）会覆盖旧句柄。
                if let Some(readonly) = this.readonly_session(&id) {
                    let session_id = readonly.id().to_owned();
                    let label = cfg.display_name();
                    crate::ai_tools::publish_sql_session(session_id, label, move |sql, rows| {
                        readonly.execute(sql, rows)
                    });
                }
                this.activate_conn(&id, window, cx);
                let name = cfg.display_name();
                let kind_label = transport_kind_label(cfg.transport(), cx);
                let detail = trf(cx, "连接 {}（{}）", &[&name, kind_label]);
                crate::audit::record_event(
                    cx,
                    crate::audit::AuditKind::Connection,
                    crate::audit::action::CONNECT,
                    crate::audit::AuditOutcome::Ok,
                    &id,
                    detail,
                );
                this.status = trf(cx, "已连接 {}", &[&cfg.display_name()]).into();
                notify::success(trf(cx, "已连接 {}。", &[&cfg.display_name()]), window, cx);
                cx.notify();
            },
        );
    }

    /// 派生下一个标签 id。
    fn next_tab_id(&mut self) -> String {
        self.tab_seq += 1;
        format!("t{}", self.tab_seq)
    }

    /// 新开一个绑定 `conn_id` 的查询标签并激活（dbx 的「+」语义）。
    ///
    /// 会话按连接共享，这里只造一个**空的编辑区快照位**；当前标签的编辑区先存回
    /// 它自己的快照位，再切到新标签的空白编辑区。
    fn open_tab(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        self.spawn_tab_for(conn_id, TabKind::Query, window, cx);
        cx.notify();
    }

    /// 造一个新标签并装上空白编辑区（当前标签的现场先存回快照位）。
    ///
    /// 新标签**插到活动标签之后**（dbx 同款），没有活动标签就追加到末尾。
    /// 身份写入 [`Self::tab_kind`]；调用方负责随后把新标签的内容装好
    /// （查询标签留空即可，数据标签接着调 [`Self::open_table`]）。
    fn spawn_tab_for(
        &mut self,
        conn_id: &str,
        kind: TabKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.active_tab.clone() {
            let state = self.take_editor_state(cx);
            self.sessions_views.insert(previous, state);
        }
        let tab = self.next_tab_id();
        self.tab_conn.insert(tab.clone(), conn_id.to_owned());
        self.tab_kind.insert(tab.clone(), kind);
        let at = self
            .tabs
            .iter()
            .position(|existing| Some(existing.as_str()) == self.active_tab.as_deref())
            .map_or(self.tabs.len(), |at| at + 1);
        self.tabs.insert(at, tab.clone());
        self.active_tab = Some(tab.clone());
        self.active = Some(conn_id.to_owned());
        self.install_editor_state(SessionEditorState::default(), window, cx);
    }

    /// 打开（或复用）某张表 / 视图的**数据标签**。
    ///
    /// 复用策略 = 「同对象复用」：同连接 + 同对象的标签已存在就只激活（`activate`
    /// 会把它留存的浏览态整体装回来），`force_new`（⌥ 点击）跳过复用强制新开。
    /// 新标签插到活动标签之后，内容走既有 [`Self::open_table`]（清态 + 首页 + 审计）。
    fn open_data_tab(
        &mut self,
        conn_id: &str,
        table: TableRef,
        force_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        if !force_new {
            let existing = find_reusable_tab(
                &self.tabs,
                &self.tab_kind,
                &self.tab_conn,
                conn_id,
                &TabKind::Data {
                    object: table.clone(),
                },
            );
            if let Some(tab) = existing {
                self.activate(&tab, window, cx);
                return;
            }
        }
        self.spawn_tab_for(
            conn_id,
            TabKind::Data {
                object: table.clone(),
            },
            window,
            cx,
        );
        self.open_table(table, window, cx);
    }

    /// 打开（或复用）某个库 / schema 的**库标签**：点左栏数据库节点的伴随动作。
    ///
    /// 同连接同名复用；新标签是空白查询态，标题带库名。复用已有标签时**不重置**
    /// 里面已输入的 SQL —— 换库清的是表浏览态（[`Self::select_database`]），
    /// 草稿是用户自己的。
    fn open_database_tab(
        &mut self,
        conn_id: &str,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        let kind = TabKind::Database { name: name.clone() };
        if let Some(tab) = find_reusable_tab(
            &self.tabs,
            &self.tab_kind,
            &self.tab_conn,
            conn_id,
            &kind,
        ) {
            self.activate(&tab, window, cx);
            return;
        }
        self.spawn_tab_for(conn_id, kind, window, cx);
    }

    /// 激活「连接 `conn_id` 的某个查询标签」：已有就切到最早的那个，没有就新开。
    ///
    /// 页面其余逻辑（左栏 / 树节点 / 仪表板节点）手里拿的都是**连接 id**，
    /// 统一从这里落到标签层。
    fn activate_conn(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        let existing = self
            .tabs
            .iter()
            .find(|tab| self.tab_conn.get(*tab).is_some_and(|c| c == conn_id))
            .cloned();
        match existing {
            Some(tab) => self.activate(&tab, window, cx),
            None => self.open_tab(conn_id, window, cx),
        }
    }

    /// 切到某个查询标签：把当前编辑区存回上一个标签，再把目标标签的编辑区装回来。
    ///
    /// 这是「每个查询标签各自保留 SQL 与结果集」的落地点（见 [`SessionEditorState`]）。
    /// 重复点同一个标签是**空操作** —— 内容与结果区必须原样留着。
    fn activate(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn_id) = self.tab_conn.get(id).cloned() else {
            return;
        };
        if !self.sessions.contains_key(&conn_id) {
            return;
        }
        if self.active_tab.as_deref() == Some(id) {
            return;
        }
        // 先把当前标签的编辑区存进它的快照位（`take_editor_state` 借 `self`，
        // `sessions_views.insert` 随后借 —— 顺序摆开，互不打架）。
        if let Some(previous) = self.active_tab.clone().filter(|prev| prev.as_str() != id) {
            let state = self.take_editor_state(cx);
            self.sessions_views.insert(previous, state);
        }
        let state = self.sessions_views.remove(id).unwrap_or_default();
        self.active_tab = Some(id.to_owned());
        self.active = Some(conn_id);
        self.install_editor_state(state, window, cx);
        cx.notify();
    }

    /// 把当前编辑区（编辑器文本 + 结果区）整体取出，交给调用方存进标签快照。
    ///
    /// 取值即搬空：调用方必须紧接着把结果装回去（[`Self::install_editor_state`]），
    /// 否则编辑区会停在空态。
    fn take_editor_state(&mut self, cx: &App) -> SessionEditorState {
        let text = self.sql.read(cx).value().to_string();
        let view = SessionView {
            mode: self.mode,
            instance_data: std::mem::take(&mut self.instance_data),
            selected_table: self.selected_table.take(),
            table_page: self.table_page,
            table_rows: self.table_rows.take(),
            table_sort: self.table_sort.take(),
            table_filters: std::mem::take(&mut self.table_filters),
            table_pk: std::mem::take(&mut self.table_pk),
            table_ddl: self.table_ddl.take(),
            table_indexes: std::mem::take(&mut self.table_indexes),
            table_row_count: self.table_row_count.take(),
            table_columns: std::mem::take(&mut self.table_columns),
            table_foreign_keys: std::mem::take(&mut self.table_foreign_keys),
            table_triggers: std::mem::take(&mut self.table_triggers),
            table_meta_for: self.table_meta_for.take(),
            query_result: self.query_result.take(),
            query_target: self.query_target.take(),
            query_pk: std::mem::take(&mut self.query_pk),
            query_sql: self.query_sql.take(),
            query_elapsed_ms: self.query_elapsed_ms.take(),
            query_loaded: self.query_loaded,
            query_has_more: self.query_has_more,
            plan_view: self.plan_view.take(),
            column_widths: std::mem::take(&mut self.column_widths),
            selected_row: self.selected_row.take(),
            checked_rows: std::mem::take(&mut self.checked_rows),
            batch_column: self.batch_column.take(),
            pending_edits: std::mem::take(&mut self.pending_edits),
            pending_inserts: std::mem::take(&mut self.pending_inserts),
            pending_undo: std::mem::take(&mut self.pending_undo),
            pending_redo: std::mem::take(&mut self.pending_redo),
            row_editor: self.row_editor.take(),
            chart_view: self.chart_view.take(),
        };
        SessionEditorState { text, view }
    }

    /// 把一份编辑区快照装进页面（编辑器文本 + 结果区）。
    fn install_editor_state(
        &mut self,
        state: SessionEditorState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SessionEditorState { text, mut view } = state;
        self.sql
            .update(cx, |editor, cx| editor.set_value(text, window, cx));
        self.mode = view.mode;
        self.instance_data = std::mem::take(&mut view.instance_data);
        self.selected_table = view.selected_table.take();
        self.table_page = view.table_page;
        self.table_rows = view.table_rows.take();
        self.table_sort = view.table_sort.take();
        self.table_filters = std::mem::take(&mut view.table_filters);
        self.table_pk = std::mem::take(&mut view.table_pk);
        self.table_ddl = view.table_ddl.take();
        self.table_indexes = std::mem::take(&mut view.table_indexes);
        self.table_row_count = view.table_row_count.take();
        self.table_columns = std::mem::take(&mut view.table_columns);
        self.table_foreign_keys = std::mem::take(&mut view.table_foreign_keys);
        self.table_triggers = std::mem::take(&mut view.table_triggers);
        self.table_meta_for = view.table_meta_for.take();
        self.query_result = view.query_result.take();
        self.query_target = view.query_target.take();
        self.query_pk = std::mem::take(&mut view.query_pk);
        self.query_sql = view.query_sql.take();
        self.query_elapsed_ms = view.query_elapsed_ms.take();
        self.query_loaded = view.query_loaded;
        self.query_has_more = view.query_has_more;
        self.plan_view = view.plan_view.take();
        self.column_widths = std::mem::take(&mut view.column_widths);
        // 快照不带横向偏移，切回标签一律回最左（与换结果集同一口径）。
        self.result_x_scroll.set_offset(point(px(0.), px(0.)));
        self.selected_row = view.selected_row.take();
        self.checked_rows = std::mem::take(&mut view.checked_rows);
        self.batch_column = view.batch_column.take();
        self.pending_edits = std::mem::take(&mut view.pending_edits);
        self.pending_inserts = std::mem::take(&mut view.pending_inserts);
        self.pending_undo = std::mem::take(&mut view.pending_undo);
        self.pending_redo = std::mem::take(&mut view.pending_redo);
        self.row_editor = view.row_editor.take();
        self.chart_view = view.chart_view.take();
    }

    /// 关闭一个查询标签。
    ///
    /// 该连接**还有别的标签**时只关这一个标签（连接保持，其他标签的编辑区原样）；
    /// 已是最后一个标签时语义回到「断开连接」——钩子时序交给
    /// [`Self::disconnect_conn`]，与会话绑定的东西（指标历史 / AI 目录）一起清。
    fn close_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn_id) = self.tab_conn.get(id).cloned() else {
            return;
        };
        let last_of_conn = self
            .tabs
            .iter()
            .filter(|tab| self.tab_conn.get(*tab).is_some_and(|c| c == &conn_id))
            .count()
            <= 1;
        if !last_of_conn {
            self.remove_tab(id, window, cx);
            return;
        }
        self.disconnect_conn(&conn_id, window, cx);
    }

    /// 只移除一个查询标签（不动连接）：丢掉它的编辑区快照，必要的话把激活位
    /// 挪到剩下的第一个标签。
    fn remove_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.sessions_views.remove(id);
        self.tab_conn.remove(id);
        self.tab_kind.remove(id);
        self.tabs.retain(|tab| tab != id);
        if self.active_tab.as_deref() == Some(id) {
            self.active_tab = None;
            self.active = None;
            self.install_editor_state(SessionEditorState::default(), window, cx);
            if let Some(next) = self.tabs.first().cloned() {
                self.activate(&next, window, cx);
            }
        }
        cx.notify();
    }

    /// 断开一条连接（原「关闭标签」的连接语义；连接钩子版的入口）。
    ///
    /// 有「断开」钩子时**先跑钩子再拆会话**：`PreDisconnect` 的语义是「连接还活着时跑」，
    /// 所以它必须在会话被移除之前发生 —— 于是这里把真正的拆除让给
    /// [`Self::teardown_session`]，钩子跑完在回调里再调它。没有钩子（绝大多数情况）
    /// 走同一条同步路径，行为与从前一字不差。
    fn disconnect_conn(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // 没有活跃会话就没有「断开」这回事：钩子不该为一个从未建立（或早掉了）的连接跑。
        if !self.sessions.contains_key(id) {
            self.teardown_session(id, window, cx);
            return;
        }
        let hooks = self
            .connections
            .iter()
            .find(|cfg| cfg.id == id)
            .map(|cfg| cfg.hooks.clone())
            .unwrap_or_default();
        if !hooks.has_phase(HookPhase::PreDisconnect) && !hooks.has_phase(HookPhase::PostDisconnect)
        {
            self.teardown_session(id, window, cx);
            return;
        }
        let facts = self
            .connections
            .iter()
            .find(|cfg| cfg.id == id)
            .map(hook_facts)
            .unwrap_or_default();
        let id_owned = id.to_owned();
        let post_hooks = hooks.clone();
        let post_facts = facts.clone();
        self.spawn_db(
            cx,
            move || run_phase(&hooks, HookPhase::PreDisconnect, &facts),
            move |this, pre, window, cx| {
                this.note_hook_report(&id_owned, pre, window, cx);
                this.teardown_session(&id_owned, window, cx);
                // 「断开后」在会话拆掉之后再跑；界面已经不关心这条连接了，结果只进日志
                // （审计面板会读日志那一份）——所以扔给后台，不占这次操作的收尾。
                let post_id = id_owned.clone();
                cx.background_executor()
                    .spawn(async move {
                        let post = run_phase(&post_hooks, HookPhase::PostDisconnect, &post_facts);
                        log_hook_report(&post_id, HookPhase::PostDisconnect, &post);
                    })
                    .detach();
            },
        );
    }

    /// 真正把一个会话拆掉（移除会话 / 指标历史 / AI 目录 / 标签页）。
    ///
    /// 与 [`Self::close_tab`] 分开：`close_tab` 负责**钩子时序**，这里只做拆除。
    /// 两者混在一起会让「钩子跑几次」与「会话何时消失」纠缠不清。
    fn teardown_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // 只有**真的拆掉了一个会话**才算「断开」：`close_tab` 在没有会话时也会走到这里
        // （见它的第一段），那种情况下记一条「断开」就是凭空多出一条流水。
        let existed = self.sessions.remove(id).is_some();
        if existed {
            let name = self
                .connections
                .iter()
                .find(|cfg| cfg.id == id)
                .map_or_else(|| id.to_owned(), ConnectionConfig::display_name);
            let detail = trf(cx, "断开 {}", &[&name]);
            crate::audit::record_event(
                cx,
                crate::audit::AuditKind::Connection,
                crate::audit::action::DISCONNECT,
                crate::audit::AuditOutcome::Ok,
                id,
                detail,
            );
        }
        // 指标时序是**会话内**的观测历史：连接都没了，攒的采样也没有归属了
        // （下次连上是新会话，从零开始采）。
        self.metric_history.remove(id);
        // 会话已经没了，AI 侧的只读目录也要跟着撤回：否则 `run_sql_readonly` 会列出一个
        // 已经关掉的连接（选了它就直接报未知连接）。
        crate::ai_tools::retract_sql_session(id);
        // 会话没了，它名下的查询标签（可能不止一个）一起拆掉。
        let closed: Vec<String> = self
            .tabs
            .iter()
            .filter(|tab| self.tab_conn.get(*tab).is_some_and(|c| c == id))
            .cloned()
            .collect();
        for tab in &closed {
            self.sessions_views.remove(tab);
            self.tab_conn.remove(tab);
            self.tab_kind.remove(tab);
        }
        self.tabs.retain(|tab| !closed.contains(tab));
        if self
            .active_tab
            .as_deref()
            .is_some_and(|tab| closed.iter().any(|c| c == tab))
        {
            // 关掉的正是当前标签：它的编辑区随快照一起丢弃。换成剩下的第一个标签
            // （没有就留空）——先清空再装，免得旧标签的内容残留到新标签上。
            self.active_tab = None;
            self.active = None;
            self.install_editor_state(SessionEditorState::default(), window, cx);
            if let Some(next) = self.tabs.first().cloned() {
                self.activate(&next, window, cx);
            }
        }
        cx.notify();
    }

    /// 记一次相位报告：进会话内日志 + 失败时给用户一个提示。
    ///
    /// 「失败」有两种：`Block` 会连带把这次建连 / 断开中断（提示按错误档），
    /// `Warn` 只是没跑成（提示按警告档）—— 用户看到的措辞必须对得上后果。
    fn note_hook_report(
        &mut self,
        id: &str,
        report: HookReport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if report.outcomes.is_empty() {
            return;
        }
        // 先把「要不要提示、提示什么」算出来：`outcomes` 随即被搬进日志，之后读不到它。
        let blocked = report.blocker().cloned();
        let failures = report.failures();
        let at = now_secs();
        for outcome in report.outcomes {
            self.hook_log.push_back(HookRecord {
                connection_id: id.to_owned(),
                outcome,
                at,
            });
        }
        while self.hook_log.len() > HOOK_LOG_CAP {
            self.hook_log.pop_front();
        }
        if let Some(blocker) = blocked {
            notify::error(
                trf(
                    cx,
                    "钩子「{}」失败，已中断本次操作：{}",
                    &[&blocker.command, &blocker.detail()],
                ),
                window,
                cx,
            );
        } else if failures > 0 {
            notify::warning(
                trf(
                    cx,
                    "有 {} 条钩子失败（按配置继续）。",
                    &[&failures.to_string()],
                ),
                window,
                cx,
            );
        }
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

    /// 执行**单条**语句（编辑器左栏的逐条 ▶）：与全文执行同一条危险确认 / 执行路径。
    ///
    /// 文本来自 [`duck_db::sqlintel::split_statement_spans`] 的切分——「图标钉在哪一行」
    /// 与「执行的是哪段文本」出自同一次切分，不会漂。
    fn run_statement(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        if sql.trim().is_empty() {
            return;
        }
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

    /// 把当前结果集**整体**按 `format` 复制到剪贴板。
    ///
    /// 与「导出」共用 [`ResultFormat`] 与内核的渲染原语，只是终点不同（系统剪贴板 vs
    /// 落盘）——两条路的格式口径不会各走各的。`InsertSql` 要的表引用与引号字符由
    /// [`Self::insert_context`] 给；拿不到就静默返回（菜单项本来已禁用，走到这里只可能
    /// 是菜单弹开后引擎被换掉了）。
    ///
    /// 刻意**不**在渲染路径上预先拼好文本：见 [`Self::render_result`] 里那段注释。
    fn copy_result_as(&mut self, format: ResultFormat, cx: &mut Context<Self>) {
        let Some(result) = self.current_result() else {
            return;
        };
        let insert = self.insert_context();
        let rendered = format.render(
            result,
            insert
                .as_ref()
                .map(|(table, quote)| (table.as_str(), *quote)),
        );
        let Some(text) = rendered else {
            return;
        };
        self.put_on_clipboard(text, tr(cx, "已复制全部结果。"), cx);
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
    ///
    /// 包装语句**按引擎来**（[`explain_sql_for`]）：SQLite 用 `EXPLAIN QUERY PLAN`、
    /// PG 用 `EXPLAIN (FORMAT JSON)`、MySQL 用 `EXPLAIN FORMAT=JSON` —— 这三条的输出
    /// 能结构化（见 [`duck_db::plan`]），比一坨文本有用得多。
    fn explain_editor_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql.read(cx).value().to_string();
        let Some(kind) = self.active_config().map(|config| config.kind) else {
            notify::info(tr(cx, "请先连接一个数据库。"), window, cx);
            return;
        };
        match explain_sql_for(kind, &sql) {
            Ok(explain) => {
                // 置一次性标记：结果装回来时顺手解析成计划树。
                self.plan_next = true;
                self.execute_sql(explain, window, cx);
            }
            Err(ExplainSkip::NotASingleStatement) => {
                notify::info(tr(cx, "执行计划一次只看一条语句。"), window, cx);
            }
            Err(ExplainSkip::NoPlanForThisStatement) => {
                notify::info(tr(cx, "这条语句没有执行计划。"), window, cx);
            }
        }
    }

    fn execute_sql(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        // 早退的两条分支都要把一次性标记清掉：没跑起来就没有「这一次是 EXPLAIN」，
        // 留着它会让**下一次**普通查询在结果区无端端解析一份计划树，审计里也会被
        // 记成「查看执行计划」—— 标记描述的是「这一次」，不是「最近一次」。
        let Some(session) = self.active_session() else {
            self.plan_next = false;
            notify::info(tr(cx, "请先连接一个数据库。"), window, cx);
            return;
        };
        if self.running.is_some() {
            self.plan_next = false;
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
        // 审计用的几件事实在**发起时**算好：它们只依赖 SQL 文本，放进后台闭包反而要多搬
        // 一份数据出来。类别按**语句风险**分（不是按「结果集有没有行」）——写死了
        // 读路径与写路径的区别，用户拿它筛「这张库被写过什么」才有意义。
        // 执行计划走的是**同一条**执行路径（[`Self::explain_editor_sql`] 只把包装语句
        // 传进来），要在这里把它认出来，否则流水里那条会是一条看不出所以然的 `EXPLAIN …`。
        // 判据用 `plan_next`：它由 `explain_editor_sql` 置上、等结果装回来才消费，此刻还是
        // true。计划只读，所以类别恒为 Query（不参与风险分类）。
        let is_explain = self.plan_next;
        let audit_kind = if is_explain || risk_of_statements(&statements) == SqlRisk::ReadOnly {
            crate::audit::AuditKind::Query
        } else {
            crate::audit::AuditKind::Write
        };
        let audit_action = if is_explain {
            crate::audit::action::EXPLAIN
        } else if audit_kind == crate::audit::AuditKind::Query {
            crate::audit::action::EXECUTE
        } else {
            crate::audit::action::COMMIT
        };
        // 指纹认「语句形状」（字面量被抹掉），摘要给人认「是哪条语句」——
        // 两者都不能少：前者用来归类，后者用来一眼看明白。
        let audit_fingerprint = fingerprint(&sql);
        let audit_summary = audit_sql_summary(&sql);
        // 结果上限在**发起这次查询时**读一次：这条语句跑多久都不影响它。
        let row_cap = query_row_cap(cx);
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
                // 失败也带上耗时：一次「立刻返回的错误」和一次「跑满 30 秒才超时」是完全
                // 不同的两件事，而错误文本里都没有这个信息 —— 审计里不记就只能猜。
                // （类型从 `Result<_, DbError>` 变成 `Result<_, (DbError, u128)>`。）
                let outcome = match guard.execute(&sql, row_cap) {
                    Ok(outcome) => outcome,
                    Err(error) => return Err((error, started.elapsed().as_millis())),
                };
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
                    push_history(&mut this.history, &history_sql, history_limit(cx));
                    this.persist_history(cx);
                    // footer 要的「这一次跑了多久」：结果装进去之前记下（与 status 同源）。
                    this.query_elapsed_ms = Some(elapsed_ms);
                    this.install_query_result(outcome, edit_meta, history_sql);
                    // 计划没解析出结构时如实说一句：输出还在（网格里就是原样文本），
                    // 但界面没能把它整理成树。
                    let mut status = status;
                    if this.plan_flat {
                        status.push_str(" · ");
                        status.push_str(tr(cx, "计划未能结构化，按原样文本展示"));
                    }
                    this.status = status.into();
                    crate::audit::record_entry(
                        cx,
                        AuditEntry::new(
                            audit_kind.key(),
                            audit_action,
                            crate::audit::AuditOutcome::Ok,
                            &session_id,
                            audit_summary.clone(),
                        )
                        .with_fingerprint(&audit_fingerprint)
                        .with_duration(elapsed_ms as i64),
                    );
                }
                Err((error, elapsed_ms)) => {
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
                        // 取消也记：`Cancelled` 在审计里不是「失败」，但「哪条语句被中断过」
                        // 是排查「为什么半截数据进去了」的第一手线索。
                        let detail = trf(cx, "{}（已取消）", &[&audit_summary]);
                        crate::audit::record_entry(
                            cx,
                            AuditEntry::new(
                                audit_kind.key(),
                                audit_action,
                                crate::audit::AuditOutcome::Cancelled,
                                &session_id,
                                detail,
                            )
                            .with_fingerprint(&audit_fingerprint)
                            .with_duration(elapsed_ms as i64),
                        );
                    } else {
                        // 留下真实错误原文 + 原 SQL：给「AI 修错 / 解释」注入上下文
                        // （通知会消失，这份上下文不会）。
                        let reason = error.to_string();
                        this.last_error = Some(QueryError {
                            sql: history_sql,
                            message: reason.clone(),
                        });
                        this.status = trf(cx, "执行失败：{}", &[&reason]).into();
                        let detail = trf(cx, "{}失败：{}", &[&audit_summary, &reason]);
                        let trace = crate::audit::record_entry(
                            cx,
                            AuditEntry::new(
                                audit_kind.key(),
                                audit_action,
                                crate::audit::AuditOutcome::Failed,
                                &session_id,
                                detail,
                            )
                            .with_fingerprint(&audit_fingerprint)
                            .with_duration(elapsed_ms as i64),
                        );
                        notify::error(
                            failure_message(cx, tr(cx, "SQL 执行"), trace.as_deref(), &reason),
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
    fn load_table_page(&mut self, cx: &mut Context<Self>) {
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

    /// 右键列头直接定序（升 / 降各一项），与点列头的三态循环并存。
    ///
    /// 作废集合与翻页口径与 [`Self::toggle_table_sort`] 完全一致——两个入口，
    /// 一份真相。
    fn sort_table_by(&mut self, column: &str, descending: bool, cx: &mut Context<Self>) {
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
    fn draft_filter_column(&mut self, column: &str, cx: &mut Context<Self>) {
        if self.mode != Mode::Table {
            return;
        }
        self.filter_column = Some(column.to_owned());
        cx.notify();
    }

    fn table_prev_page(&mut self, cx: &mut Context<Self>) {
        if self.table_page > 0 {
            self.table_page -= 1;
            self.load_table_page(cx);
        }
    }

    fn table_next_page(&mut self, cx: &mut Context<Self>) {
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

    fn refresh_table_page(&mut self, cx: &mut Context<Self>) {
        // 「刷新」按重来理解：行数与索引一并重取（删改之后行数就变了）。
        self.table_meta_for = None;
        self.load_table_page(cx);
    }

    /// 把一段生成的 SQL 放进编辑器（**只放不跑**）。
    ///
    /// 结构管理（建表 / 改表 / 删表）与 DDL 改写都走这条路：界面拼出语法正确的
    /// 起点，落不落库由用户在编辑器里按「执行」决定 —— 与项目里其它写路径同一条
    /// 「人工审批」纪律。
    ///
    /// **不切模式**：用户可能正在浏览某张表，把语句塞进编辑器不该顺手把结果区挪走。
    fn stage_sql(
        &mut self,
        sql: String,
        note: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sql
            .update(cx, |state, cx| state.set_value(sql, window, cx));
        self.status = note;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 表浏览的过滤条。
    // ------------------------------------------------------------------

    /// 把过滤条上攒的草稿变成一条生效条件，然后重取第一页。
    ///
    /// 校验只做界面能确定的两件事：选了列、该操作符需要的值非空。**值的类型合法性
    /// 不在这里拦** —— 那要问引擎（驱动把值走绑定参数发出去，服务端报错原样透出），
    /// 在界面里猜类型只会猜错。
    fn add_table_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    fn remove_table_filter(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.table_filters.len() {
            self.table_filters.remove(index);
            self.after_filter_changed(cx);
        }
    }

    /// 清空全部生效条件。
    fn clear_table_filters(&mut self, cx: &mut Context<Self>) {
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
    fn after_filter_changed(&mut self, cx: &mut Context<Self>) {
        self.checked_rows.clear();
        self.selected_row = None;
        self.row_editor = None;
        self.table_page = 0;
        self.load_table_page(cx);
        cx.notify();
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
        // 分页状态先算：结果集马上要被搬进 `QueryView`，之后在 `outcome` 上就读不到了。
        //
        // 「可以加载更多」的三个前提缺一不可：
        // ① 语句是**单条且可证明只读**——只有它才能被安全地用 `LIMIT/OFFSET` 重跑；
        // ② 当前引擎支持分页续拉（`DatabaseKind::supports_paged_read`；SQL Server /
        //    MongoDB / Elasticsearch 为假）；
        // ③ 首屏确实被行上限截断了（没截断 = 已经拿全了）。
        let single_read_only = {
            let statements = split_statements(&sql);
            statements.len() == 1 && risk_of_statements(&statements) == SqlRisk::ReadOnly
        };
        // 「能不能续拉」查内核的**单一判据**（`DatabaseKind::supports_paged_read`）——
        // SQL Server / MongoDB / Elasticsearch 都被判为不支持，界面因此不出「加载更多」，
        // 与 `Connection::execute_paged` 的拒绝口径同源。
        let paging_capable = self
            .active_config()
            .is_some_and(|cfg| cfg.kind.supports_paged_read());
        let truncated = outcome
            .result
            .as_ref()
            .is_some_and(|result| result.truncated);
        self.query_loaded = outcome
            .result
            .as_ref()
            .map_or(0, |result| result.row_count() as u64);
        self.query_has_more = single_read_only && paging_capable && truncated;
        // 新语句 = 新结果：列宽变了（列集不同）才把横向位置回零；同一份结构的
        // 「加载更多」不经过这里，不会打断正在看右侧列的人。
        let widths = outcome
            .result
            .as_ref()
            .map(compute_column_widths)
            .unwrap_or_default();
        if widths != self.column_widths {
            self.result_x_scroll.set_offset(point(px(0.), px(0.)));
        }
        self.column_widths = widths;
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
        // 这次执行成功了：上一次的错误上下文与解释面板都作废。
        self.last_error = None;
        self.ai_explain = None;
        self.selected_row = None;
        self.row_editor = None;
        // 结果集换了：旧的勾选按「旧结果集的下标」记，必须清掉；暂存改动同理
        // （它们按旧行号定位，重跑后行号可能指向别的行）。
        self.checked_rows.clear();
        self.clear_pending();
        // 计划视图跟**这一次**的结果绑定：这一步是 EXPLAIN 就解析一份，否则清掉
        // （普通查询不能在结果区留下上一次的计划）。
        let planned = std::mem::take(&mut self.plan_next);
        self.plan_view = planned
            .then(|| {
                self.query_result
                    .as_ref()
                    .and_then(|view| view.result.as_ref())
                    .map(parse_plan)
            })
            .flatten()
            .and_then(|tree| match tree {
                PlanTree::Nodes(nodes) if !nodes.is_empty() => Some(PlanView { nodes, raw: false }),
                // 解析不出来就不进计划态：结果区照旧显示原始网格（这就是「原样文本
                // 展示」，输出一点没丢），只把「没解析出来」这件事记下来给状态栏说。
                PlanTree::Nodes(_) | PlanTree::Raw(_) => None,
            });
        self.plan_flat = planned && self.plan_view.is_none();
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
            // 实例信息页没有可编辑的单元格（它不是结果集，是服务器事实）。
            // 仪表板同理：面板里那些行是**读出来给人看的**，不是可改的暂存区。
            Mode::Instance(_) | Mode::Dashboard(_) => None,
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
            // 实例信息页不可编辑，也就没有「重载来源」这回事；仪表板面板同理。
            Mode::Instance(_) | Mode::Dashboard(_) => {}
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
            self.record_pending();
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
        // 这一步（无论改了几格、是否覆盖了同格的旧改动）在撤销栈里**只算一步**：
        // 用户按一次「保存」，`⌘Z` 就退回按之前。
        self.record_pending();
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

    /// 当前暂存区的一份快照。
    fn pending_snapshot(&self) -> PendingSnapshot {
        PendingSnapshot {
            edits: self.pending_edits.clone(),
            inserts: self.pending_inserts.clone(),
        }
    }

    /// 把暂存区换成一份快照。
    fn restore_pending(&mut self, snapshot: PendingSnapshot) {
        self.pending_edits = snapshot.edits;
        self.pending_inserts = snapshot.inserts;
    }

    /// 在**改动暂存区之前**记一步（撤销栈的落点）。
    ///
    /// 每处会改动暂存区的操作都要先调它：`stage_row_edits`（保存）、`discard_pending`
    /// （丢弃）、以及撤销 / 重做本身。记的是**改动前**的状态——撤销就是把暂存区恢复成它。
    ///
    /// 记完之后**重做栈作废**：走了新的一步，「未来的那一步」就不存在了
    /// （与文本编辑器的撤销口径一致）。
    fn record_pending(&mut self) {
        self.pending_undo.push(self.pending_snapshot());
        if self.pending_undo.len() > UNDO_DEPTH {
            // 丢**最旧**的一步而不是整栈清空：用户最需要的一定是最近几步。
            self.pending_undo.remove(0);
        }
        self.pending_redo.clear();
    }

    /// 清空暂存区**连同撤销 / 重做栈**。
    ///
    /// 用在「暂存区整体作废」的地方：提交成功、换表、换库、结果集被重跑替换。
    /// 这些场景下暂存区里的改动本来就不再指向当前结果集，留着撤销栈只会让 `⌘Z`
    /// 恢复出一批**对不上当前表**的改动——那比不能撤销更坏。
    fn clear_pending(&mut self) {
        self.pending_edits.clear();
        self.pending_inserts.clear();
        self.pending_undo.clear();
        self.pending_redo.clear();
    }

    /// 撤销一步（`⌘Z`）：回退到上一次「保存 / 丢弃」之前的样子。
    ///
    /// 不动 `row_editor`：行编辑器里的输入与暂存区是两份东西，撤销顺手把它关掉等于
    /// 把用户刚敲的字抹了。它留在原地，用户想再存一次就再点一次「保存」。
    fn undo_pending(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.pending_undo.pop() else {
            self.status = tr(cx, "没有可撤销的暂存改动").into();
            cx.notify();
            return;
        };
        // 撤销前的状态压进重做栈，`⇧⌘Z` 才能原样回去。
        self.pending_redo.push(self.pending_snapshot());
        self.restore_pending(snapshot);
        self.status = trf(
            cx,
            "已撤销一步（共 {} 条待提交）",
            &[&self.pending_total().to_string()],
        )
        .into();
        cx.notify();
    }

    /// 重做一步（`⇧⌘Z`）。
    fn redo_pending(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.pending_redo.pop() else {
            self.status = tr(cx, "没有可重做的暂存改动").into();
            cx.notify();
            return;
        };
        self.pending_undo.push(self.pending_snapshot());
        self.restore_pending(snapshot);
        self.status = trf(
            cx,
            "已重做一步（共 {} 条待提交）",
            &[&self.pending_total().to_string()],
        )
        .into();
        cx.notify();
    }

    /// 丢掉全部暂存改动（**可撤销**：撤销栈里留了一步）。
    fn discard_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_total() == 0 {
            // 空暂存区不留一步空快照：否则 `⌘Z` 会「撤销」出一次什么都没发生的操作，
            // 用户按第二下才退到真正想要的那一步。
            notify::info(tr(cx, "没有待提交的改动。"), window, cx);
            return;
        }
        self.record_pending();
        self.pending_edits.clear();
        self.pending_inserts.clear();
        self.status = tr(cx, "已丢弃暂存改动（⌘Z 可撤销）").into();
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
        // 审计要的「改了哪张表」在**发起时**就抽成字符串：`table` 本体要交给后台闭包，
        // 收工回调（`move` 闭包）里已经拿不到它了。
        let audit_target = table.display();
        let edits = self.pending_edits.clone();
        let inserts = self.pending_inserts.clone();
        let planned_updates = u64::try_from(edits.len()).unwrap_or(u64::MAX);
        let planned_inserts = u64::try_from(inserts.len()).unwrap_or(u64::MAX);
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        // 审计的 scope 是**会话**（与 SQL 执行那条路径同一口径），不是连接档案 id：
        // 同一条连接连两次是两个会话，流水要能分开。
        let session_id = self.active.clone().unwrap_or_default();
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
                    // 提交成功：暂存区连同撤销栈一起清（这一步已经落到库里，
                    // 撤销栈里剩下的「撤销到提交前」会指向一批已经写掉、且行号可能
                    // 已变的改动）。
                    this.clear_pending();
                    let BatchOutcome { updated, inserted } = outcome;
                    // 「提交写入」单独成一类动作：它和 `UPDATE` 语句都是写，但「暂存区攒了
                    // 一批改动一起落库」这件事只能从暂存区发生，按 `COMMIT` 记才好把它
                    // 和逐条执行区分开。
                    let detail = trf(
                        cx,
                        "提交 {} · 改 {} 行 · 新增 {} 行",
                        &[&audit_target, &updated.to_string(), &inserted.to_string()],
                    );
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::COMMIT,
                        crate::audit::AuditOutcome::Ok,
                        &session_id,
                        detail,
                    );
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
                    // 整体回滚也记：失败的那一批**没有**落库，这一条告诉后来人
                    // 「这个时刻有过一次没写成的提交」，否则流水里只看得到成功的那些。
                    let detail = trf(cx, "提交失败：{}", &[&error.to_string()]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::COMMIT,
                        crate::audit::AuditOutcome::Failed,
                        &session_id,
                        detail,
                    );
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
        // 审计的「改了什么」= 表 + 列（只写行数看不出改的是哪个字段，而批量改最危险的
        // 就是**改错列**）。`table` 与 `column` 都要交给后台闭包，所以先拼成字符串。
        let audit_target = format!("{}.{}", table.display(), column);
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let session_id = self.active.clone().unwrap_or_default();
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
                    let detail = trf(
                        cx,
                        "修改 {} · {} 行",
                        &[&audit_target, &affected.to_string()],
                    );
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::EDIT_ROW,
                        crate::audit::AuditOutcome::Ok,
                        &session_id,
                        detail,
                    );
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
                    let detail = trf(cx, "批量修改失败：{}", &[&error.to_string()]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::EDIT_ROW,
                        crate::audit::AuditOutcome::Failed,
                        &session_id,
                        detail.clone(),
                    );
                    this.status = detail.clone().into();
                    notify::error(detail, window, cx);
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
        let audit_target = table.display();
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let session_id = self.active.clone().unwrap_or_default();
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
                    // 删行是**最需要留痕**的写：改错了还能改回来，删掉的行只有这条流水
                    // 说得清「什么时候、按什么范围删的」（具体值在确认框里，已随通知消失）。
                    let detail = trf(
                        cx,
                        "删除 {} · {} 行",
                        &[&audit_target, &affected.to_string()],
                    );
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::DELETE_ROWS,
                        crate::audit::AuditOutcome::Ok,
                        &session_id,
                        detail,
                    );
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
                    let detail = trf(cx, "批量删除失败：{}", &[&error.to_string()]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::DELETE_ROWS,
                        crate::audit::AuditOutcome::Failed,
                        &session_id,
                        detail.clone(),
                    );
                    this.status = detail.clone().into();
                    notify::error(detail, window, cx);
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
        let audit_target = table.display();
        let session_id = self.active.clone().unwrap_or_default();
        self.status = tr(cx, "正在删除行…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.delete_rows(&table, &pk_values)
            },
            // 这一处回调本来是**不捕获**任何东西的（所以没写 `move`）；现在它要带
            // `audit_target` / `session_id` 进闭包，加 `move` 才拿得到所有权。
            move |this, result, window, cx| match result {
                Ok(_) => {
                    // 单行删除也用「删除 {} · {} 行」这条文案（行数恒为 1）：
                    // 同一个动作在流水里应该长成同一个样子，别按入口分两种写法。
                    let detail = trf(cx, "删除 {} · {} 行", &[&audit_target, "1"]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::DELETE_ROWS,
                        crate::audit::AuditOutcome::Ok,
                        &session_id,
                        detail,
                    );
                    this.status = tr(cx, "行已删除").into();
                    notify::success(tr(cx, "行已删除。"), window, cx);
                    // 行数一定变了：让表级元信息跟着重取。
                    this.table_meta_for = None;
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    let detail = trf(cx, "删除失败：{}", &[&error.to_string()]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::DELETE_ROWS,
                        crate::audit::AuditOutcome::Failed,
                        &session_id,
                        detail.clone(),
                    );
                    this.status = detail.clone().into();
                    notify::error(detail, window, cx);
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // AI 助手（text-to-SQL；写语句一律退回编辑器人工审批）。
    // ------------------------------------------------------------------

    /// 当前选中的 AI 厂商档案：显式选择 → 助手页默认档案 → 第一条。
    ///
    /// 档案是 kv 的内存快照（[`crate::ai::load_providers`]），读它不落盘也不解密；
    /// **解密**只发生在 [`Self::ai_setup`]（发请求前一次）。
    fn active_ai_provider(&self, cx: &App) -> Option<ProviderProfile> {
        let providers = crate::ai::load_providers(cx);
        if providers.is_empty() {
            return None;
        }
        let wanted = self
            .ai_provider
            .clone()
            .or_else(|| crate::ai::load_settings(cx).default_provider);
        wanted
            .and_then(|id| providers.iter().find(|profile| profile.id == id).cloned())
            .or_else(|| providers.first().cloned())
    }

    /// 备好一次 AI 请求要用的 provider 参数与**明文 key**。
    ///
    /// 走的是助手页**同一条**「按档案 + vault 密文」通道（[`crate::ai::load_api_key`]），
    /// 与 db 页旧的 `ai:api_key` 单键通道已彻底解耦。返回 `Err(文案)` 表示配置不齐——
    /// 文案直接给用户看（提示到「AI 助手」页重新配置），不静默失败。
    fn ai_setup(&self, cx: &App) -> Result<OpenAiCompatible, SharedString> {
        let Some(profile) = self.active_ai_provider(cx) else {
            return Err(tr(
                cx,
                "尚未配置 AI 厂商，请到「AI 助手」页添加厂商档案与 API key。",
            )
            .into());
        };
        let display = crate::ai::profile_display_name(&profile);
        let Some(api_key) =
            crate::ai::load_api_key(cx, &profile.id).filter(|key| !key.trim().is_empty())
        else {
            return Err(trf(
                cx,
                "厂商「{}」还没配置 API key，请到「AI 助手」页配置。",
                &[&display],
            )
            .into());
        };
        Ok(OpenAiCompatible {
            base_url: profile.base_url.clone(),
            api_key,
            model: profile.model.clone(),
            timeout_secs: 60,
        })
    }

    /// DB 场景的 AI 动作入口：`generate` / `fix` / `explain`。
    ///
    /// 动作由**调用点显式传入**（错误条给 `Fix` / `Explain`，AI 助手条给 `Generate`）；
    /// `duck-db` 侧的 `pick_action` 只是给「按上下文规则选动作」留下的纯函数入口，**当前
    /// 生产路径并未调用它**（按钮已经知道要哪个动作，不必再猜一次）。
    /// schema 摘要的静态部分（表名）在主线程备好；列级信息在工作闭包内持锁快取，
    /// 取完即释放——模型调用（网络等待）绝不持连接锁。
    ///
    /// 安全边界（与既有实现一致，不放宽）：`generate` / `fix` 产出的语句过
    /// [`allow_auto_execute`]（单条 + 可证明只读）才自动执行，其余（写 / 多条 / 混合）
    /// 只填编辑器等人工审阅；`explain` 只展示文本，**永不执行**。
    fn run_ai_action(&mut self, action: AiAction, window: &mut Window, cx: &mut Context<Self>) {
        // 1. 上下文：动作决定注入什么。
        let (question, sql_ctx, error_ctx) = match action {
            AiAction::Generate => (
                self.ai_question.read(cx).value().trim().to_owned(),
                None,
                None,
            ),
            AiAction::Fix | AiAction::Explain => {
                let Some(error) = self.last_error.as_ref() else {
                    notify::info(tr(cx, "当前没有可分析的执行错误。"), window, cx);
                    return;
                };
                (
                    String::new(),
                    Some(error.sql.clone()),
                    Some(error.message.clone()),
                )
            }
        };
        if action == AiAction::Generate && question.is_empty() {
            notify::info(tr(cx, "请先输入问题。"), window, cx);
            return;
        }
        // 2. 厂商 + key：配置不齐直接提示，不静默失败。
        let provider = match self.ai_setup(cx) {
            Ok(provider) => provider,
            Err(message) => {
                notify::warning(message, window, cx);
                return;
            }
        };
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
        // 3. schema 摘要的静态部分（表名清单，最多 30 表）。
        let mut schema = String::new();
        for table in session.catalog.iter().take(30) {
            schema.push_str(&table.name);
            schema.push('\n');
        }
        // 选中表必须在目录里，列级信息才值得进 prompt。
        let selected_table = self.selected_table.clone().filter(|table| {
            session
                .catalog
                .iter()
                .any(|entry| entry.table_ref() == *table)
        });
        let conn = session.conn.clone();
        self.status = tr(cx, "正在请求 AI…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<(String, u32), DbError> {
                // 列级 schema（持锁快查）；取完立刻释放，AI 调用不持锁。
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
                // 提示词由**纯函数工厂**组装：generate 注入问题，fix / explain 注入
                // 真实错误原文 + 原 SQL（见 `duck_db::ai::build_action_prompt`）。
                let prompt = build_action_prompt(
                    action,
                    &detail,
                    &question,
                    sql_ctx.as_deref(),
                    error_ctx.as_deref(),
                );
                let completion = provider.complete_with(action.system_prompt(), &prompt)?;
                Ok((completion.text, completion.total_tokens))
            },
            move |this, result, window, cx| match result {
                Ok((text, tokens)) => this.install_ai_result(action, text, tokens, window, cx),
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

    /// 处理一次 AI 调用的产出：按动作决定「自动执行 / 填编辑器 / 只展示」。
    ///
    /// - `generate` / `fix`：单条只读 → 走可取消执行路径；其余 → 只填编辑器（人工审阅）；
    /// - `explain`：正文是**解释文本**，只放进 [`Self::ai_explain`] 展示，永不执行。
    fn install_ai_result(
        &mut self,
        action: AiAction,
        text: String,
        tokens: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            AiAction::Explain => {
                let explanation = text.trim().to_owned();
                if explanation.is_empty() {
                    notify::info(tr(cx, "AI 没有返回解释内容。"), window, cx);
                    return;
                }
                self.ai_explain = Some(explanation);
                self.status = trf(cx, "AI 已给出解释 · {} tokens", &[&tokens.to_string()]).into();
                cx.notify();
            }
            AiAction::Generate | AiAction::Fix => {
                // 闸门是**纯函数**（`duck_db::sqlintel::allow_auto_execute`）：恰好一条且
                // 可证明只读才自动执行；写 / 多条 / 混合（含数据修改型 CTE、`SELECT …
                // INTO`、`EXPLAIN ANALYZE <写>`、赋值型 PRAGMA）一律只填编辑器。
                if allow_auto_execute(&text) {
                    // 单条只读：立刻执行（可取消 + 查询超时），与手敲执行同一条路径。
                    self.execute_ai_sql(text, tokens, window, cx);
                } else {
                    // 写 / 多条 / 混合：只填编辑器，人工审阅后自己跑——**不自动执行写**。
                    self.sql.update(cx, |state, cx| {
                        state.set_value(text, window, cx);
                    });
                    self.status = trf(
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
            }
        }
    }

    /// 执行 AI 产出的**单条只读** SQL —— 与手敲执行共用「可取消 + 查询超时」路径。
    ///
    /// 与 [`Self::execute_sql`] 的差别只有两点：状态文案带 tokens（来源是 AI），
    /// 以及**函数入口处会再跑一遍 [`allow_auto_execute`]**（单条 + 可证明只读 + 无已知
    /// 副作用，不过即拒绝），再走内核 [`Connection::execute_read_only`] 二次复核，不必再走
    /// 危险确认。模型调用本身无法带外取消（只有它自带的 HTTP 超时），所以「可停」只覆盖
    /// 真正在库上跑的那一段。
    fn execute_ai_sql(
        &mut self,
        sql: String,
        tokens: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 闸门由**代码**强制，不靠调用点自觉：走到这里就再跑一遍
        // [`allow_auto_execute`]（单条 + 可证明只读 + 无已知副作用），不过即拒绝。
        // 这样即便将来新增调用点忘了先判，也绕不过闸门（纵深防御）。
        if !allow_auto_execute(&sql) {
            self.status = tr(cx, "该语句未通过只读闸门，已拒绝自动执行。").into();
            notify::warning(
                tr(
                    cx,
                    "该语句未通过只读闸门，已拒绝自动执行（请人工审阅后再手动执行）。",
                ),
                window,
                cx,
            );
            return;
        }
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
        // 与 `execute_sql` 同一条口径：上限在发起时读一次。
        let row_cap = query_row_cap(cx);
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
                // 纵深防御：即便闸门已判只读，执行仍走**内核只读路径**——它会再用
                // `prove_read_only` 复核一遍，证明不了就整体拒绝，绝不退回通用 `execute`。
                guard.execute_read_only(&sql, row_cap).map(|outcome| {
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
                    // 与手敲执行同口径：留下错误原文 + 原 SQL，供「AI 修错 / 解释」用。
                    this.last_error = Some(QueryError {
                        sql: sql_for_log.clone(),
                        message: error.to_string(),
                    });
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

    /// 按连接 id 取一个**只读**会话句柄（`None` = 没有这个连接）。
    ///
    /// 给 AI 的 `run_sql_readonly` 用（`docs/ai-design.md` §10.4）：拿到句柄之后由调用方
    /// 丢进后台线程执行；句柄是 `Send + 'static` 的（`Arc<Mutex<Connection>>` + `String`），
    /// 因此可以直接 `move` 进 `background_executor`。
    ///
    /// ⚠️ 只读由**内核**保证（[`duck_db::Connection::execute_read_only`]），这里不做也不
    /// 依赖任何界面侧的检查 —— 上层拿到句柄同样跑不了写语句。
    /// 调用方是 `ai_tools::run_sql_readonly`（连接成功时发布、断开时撤回）。
    pub(crate) fn readonly_session(&self, id: &str) -> Option<ReadOnlySession> {
        let session = self.sessions.get(id)?;
        Some(ReadOnlySession {
            id: id.to_owned(),
            conn: session.conn.clone(),
        })
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
            // 实例页不是结果集：没有可导出的行。仪表板也不是——它一屏有**好几份**结果集，
            // 导出 / 复制的「当前结果集」在这页没有单一定义（要导出就点开对应图表）。
            Mode::Instance(_) | Mode::Dashboard(_) => None,
        }
    }

    /// 导出当前结果集（[`ResultFormat`] 里任一档）。
    ///
    /// 导出文本由**内核的中性层**生成（见 [`ResultFormat::render`]），这里只管选格式、
    /// 拼表名与落盘——格式逻辑不进 UI，也不属于任何一条驱动。
    fn export_result(&mut self, format: ResultFormat, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self.current_result() else {
            notify::info(tr(cx, "当前没有可导出的结果。"), window, cx);
            return;
        };
        // 原始字节档走字节路径：它不是文本，不能过 `write(&path, text)` 那道口子。
        if format.is_binary() {
            let Some(bytes) = format.render_bytes(result) else {
                notify::info(
                    tr(cx, "原始字节导出要求结果只有一列且全是二进制值。"),
                    window,
                    cx,
                );
                return;
            };
            let Some(path) = duck_platform::save_file(format.file_name()) else {
                return;
            };
            let note = format.done_note(cx);
            self.spawn_db(
                cx,
                move || std::fs::write(&path, bytes).map_err(|err| err.to_string()),
                move |this, result, window, cx| match result {
                    Ok(()) => {
                        notify::success(note, window, cx);
                        this.status = tr(cx, "导出完成").into();
                    }
                    Err(error) => {
                        notify::error(trf(cx, "导出失败：{}", &[&error]), window, cx);
                    }
                },
            );
            return;
        }
        // INSERT 是 SQL 草稿：表名与列名都要按引擎的引号规则拼。MongoDB /
        // Elasticsearch 没有 SQL 标识符概念（菜单里已对它们禁用这一档），
        // 这里再挡一次，避免别处调进来产出坏 SQL。
        let insert = self.insert_context();
        let rendered = format.render(
            result,
            insert
                .as_ref()
                .map(|(table, quote)| (table.as_str(), *quote)),
        );
        let Some(text) = rendered else {
            notify::info(tr(cx, "当前引擎不支持导出 INSERT 语句。"), window, cx);
            return;
        };
        let Some(path) = duck_platform::save_file(format.file_name()) else {
            return;
        };
        let note = format.done_note(cx);
        self.spawn_db(
            cx,
            move || std::fs::write(&path, text).map_err(|err| err.to_string()),
            move |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(note, window, cx);
                    this.status = tr(cx, "导出完成").into();
                }
                Err(error) => {
                    notify::error(trf(cx, "导出失败：{}", &[&error]), window, cx);
                }
            },
        );
    }

    /// 渲染 INSERT 草稿需要的额外入参：表引用文本 + 引擎的引号字符。
    ///
    /// MongoDB / Elasticsearch 没有 SQL 标识符概念（`identifier_quote()` 为 `None`），
    /// 这里就返回 `None` —— 调用方据此把那一档禁掉或回绝。
    fn insert_context(&self) -> Option<(String, char)> {
        let quote = self
            .active_config()
            .and_then(|cfg| cfg.kind.identifier_quote())?;
        Some((self.export_table_name(quote), quote))
    }

    /// 导出 INSERT 时用的表引用文本（按引擎引号拼限定名）。
    ///
    /// 表浏览模式用当前表，SQL 结果模式用 [`Self::query_target`]（可定位单表时）；
    /// 两者都拿不到（任意查询 / 多表 join）时退回占位名 `result`——用户一眼看得出
    /// 需要替换，比编一个像真的表名好。
    fn export_table_name(&self, quote: char) -> String {
        let table = match self.mode {
            Mode::Table => self.selected_table.as_ref(),
            Mode::Query => self.query_target.as_ref(),
            Mode::Instance(_) | Mode::Dashboard(_) => None,
        };
        table
            .and_then(|table| quote_qualified(table, quote).ok())
            .unwrap_or_else(|| "result".to_owned())
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
                                    this.activate_conn(&id, window, cx);
                                } else {
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
                if let Some(session) = self.sessions.get(&id) {
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

    /// 进入编辑表单（弹窗子页）。
    fn edit_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cfg) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        let form = EditFormView::from_config(
            self.view.clone(),
            &cfg,
            self.tunnel_profiles.clone(),
            window,
            cx,
        );
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
        for tab_id in &self.tabs {
            // 标签元素是**标签 id**，经 `tab_conn` 找到所属连接再取展示信息。
            let Some(conn_id) = self.tab_conn.get(tab_id) else {
                continue;
            };
            let Some(cfg) = self.connections.iter().find(|cfg| &cfg.id == conn_id) else {
                continue;
            };
            let active = self.active_tab.as_deref() == Some(tab_id.as_str());
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
                    // 引擎品牌 logo（全彩）：查询标签保持主题中性色，靠 logo 辨识引擎。
                    // 数据 / 库标签改用对象图标（表 / 库），标题也换成对象名 —— dbx 同款
                    // 「表名@库」：点过的表各自一张标签，标题直说这是谁。
                    .child(match &self.tab_kind.get(tab_id) {
                        Some(TabKind::Data { object }) => Icon::new(table_kind_icon(
                            TableKind::Table,
                        ))
                        .size(extended.icon.sm)
                        .into_any_element(),
                        Some(TabKind::Database { .. }) => {
                            Icon::new(DuckIcon::Database)
                                .size(extended.icon.sm)
                                .into_any_element()
                        }
                        _ => kind_logo(cfg.kind).element(extended.icon.sm),
                    })
                    .child(div().min_w_0().truncate().child(match &self.tab_kind.get(tab_id) {
                        Some(TabKind::Data { object }) => match &object.schema {
                            Some(namespace) => format!("{}@{}", object.name, namespace),
                            None => object.name.clone(),
                        },
                        Some(TabKind::Database { name }) => name.clone(),
                        _ => cfg.display_name().to_string(),
                    }))
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
        let extended = theme.extended();
        let view = self.view.clone();

        // ── 左侧：动作图标行（dbx 同款）──
        //
        // 原先这一排是文字按钮，挤占编辑器宽度；对齐 dbx 收成纯图标，
        // 语义全部保留在无障碍标签里。执行/停止仍是同一对互斥按钮。
        let actions = h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            // 跑着就把「执行」换成「停止」：主按钮永远只有一个动作，
            // 用户不会去点一个点了没用的按钮。
            .child(if self.running.is_some() {
                icon_button(
                    "db-stop-query",
                    tr(cx, "停止"),
                    Icon::new(DuckIcon::Square).size(extended.icon.md),
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
                icon_button(
                    "db-run-query",
                    tr(cx, "执行"),
                    Icon::new(DuckIcon::Play).size(extended.icon.md),
                    ButtonVariant::Primary,
                    cx,
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
            // 「执行计划」放最前：它是执行旁边最常按的第二个动作。
            .child(
                icon_button(
                    "db-explain-query",
                    tr(cx, "执行计划"),
                    Icon::new(DuckIcon::ListOrdered).size(extended.icon.md),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.explain_editor_sql(window, cx));
                    }
                }),
            )
            .child(
                icon_button(
                    "db-format-sql",
                    tr(cx, "格式化"),
                    Icon::new(DuckIcon::Code).size(extended.icon.md),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.format_editor_sql(window, cx));
                    }
                }),
            )
            .child(
                icon_button(
                    "db-ai-toggle",
                    tr(cx, "AI 助手"),
                    Icon::new(DuckIcon::Sparkles).size(extended.icon.md),
                    if self.ai_open {
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
                            this.ai_open = !this.ai_open;
                            cx.notify();
                        });
                    }
                }),
            )
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
            .child(
                icon_button(
                    "db-history-toggle",
                    tr(cx, "历史"),
                    Icon::new(DuckIcon::Clock).size(extended.icon.md),
                    if self.history_open {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Ghost
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
            // 「保存查询」：把编辑器里的 SQL 存进左栏「已保存查询」。
            // 编辑器为空时禁用——存一条空语句没有任何意义。
            .child(
                icon_button(
                    "db-save-query",
                    tr(cx, "保存查询"),
                    Icon::new(DuckIcon::FileText).size(extended.icon.md),
                    ButtonVariant::Ghost,
                    cx,
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

        // ── 右侧：模式标签 + 连接下拉 + 库/Schema 下拉（dbx 同款）──
        let mut meta = h_flex().flex_none().items_center().gap(tokens.spacing.xs);
        // 单次覆盖生效时挂个标签：跑慢了才知道「不是连接默认在管」。
        if let Some(secs) = self.query_timeout_override {
            meta = meta.child(neutral_tag(
                SharedString::from(trf(cx, "超时 {} 秒", &[&secs.to_string()])),
                cx,
            ));
        }
        meta = meta.child(match self.mode {
            Mode::Query => neutral_tag(tr(cx, "SQL 结果"), cx).into_any_element(),
            Mode::Table => neutral_tag(tr(cx, "表浏览"), cx).into_any_element(),
            // 实例信息页没有结果集，标出**在看哪一档**（四个入口共用一块正文，
            // 不标的话用户切了档也认不出自己在看哪一页）。
            Mode::Instance(tab) => neutral_tag(tab.heading(cx), cx).into_any_element(),
            // 仪表板走的是另一条正文（`render_body` 里提前分支），这条工具栏
            // 在那一档下压根不画；留这个分支只为让 match 保持穷尽。
            Mode::Dashboard(_) => neutral_tag(tr(cx, "仪表板"), cx).into_any_element(),
        });
        if let Some(picker) = self.namespace_picker("db-toolbar-schema-picker", cx) {
            meta = meta.child(picker);
        }
        if let Some(picker) = self.connection_picker(cx) {
            meta = meta.child(picker);
        }

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .child(actions)
            // 中间弹性带放最近一次操作的结果文案；左右都是 flex_none，挤了先截它。
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(meta_text(self.status.clone(), cx)),
            )
            .child(meta)
            .into_any_element()
    }

    /// 连接切换下拉（编辑器工具条右端，dbx 同款）。
    ///
    /// 列出**打开中的标签所属的连接**（同一连接多个标签只列一次），选中即切到
    /// 该连接最早的一个查询标签。没有打开的标签就不画。
    fn connection_picker(&self, cx: &App) -> Option<AnyElement> {
        let view = self.view.clone();
        let mut seen: Vec<&str> = Vec::new();
        let options: Vec<(SharedString, SharedString)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let conn = self.tab_conn.get(tab)?;
                if seen.contains(&conn.as_str()) {
                    return None;
                }
                seen.push(conn);
                let cfg = self.connections.iter().find(|cfg| &cfg.id == conn)?;
                Some((
                    SharedString::from(conn.clone()),
                    SharedString::from(cfg.display_name()),
                ))
            })
            .collect();
        if options.is_empty() {
            return None;
        }
        let current = self.active.clone()?;
        Some(dropdown_select(
            "db-conn-picker",
            options,
            SharedString::from(current),
            move |value: SharedString, window, cx| {
                let Some(entity) = view.upgrade() else {
                    return;
                };
                entity.update(cx, |this, cx| {
                    this.activate_conn(&value, window, cx);
                });
            },
            cx,
        ))
    }

    /// 命名空间选择下拉（库 / Schema，由引擎分层决定）。
    ///
    /// 编辑器工具条与右栏共用一份构造；`id` 由调用方给（两处不能撞 ElementId）。
    /// 返回 `None` = 引擎没有命名空间层（如 SQLite 单文件），不画。
    fn namespace_picker(&self, id: &'static str, cx: &App) -> Option<AnyElement> {
        let session = self.active_session()?;
        let view = self.view.clone();
        // 下拉里列的是**库**还是**库内的 schema**，由引擎决定（见 `namespace_is_database`）。
        // 库这一层优先用 `databases`（「这条连接上有哪些库」），拿不到才退回 `schemas`。
        let layer_is_database = namespace_is_database(session.kind);
        let names: Vec<String> = if layer_is_database {
            collect_namespaces(&session.databases, &session.schemas)
        } else {
            session.schemas.clone()
        };
        let current = if layer_is_database {
            session.database.clone()
        } else {
            session.schema.clone()
        };
        if names.is_empty() && current.is_none() {
            return None;
        }
        let options: Vec<(SharedString, SharedString)> = names
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
        let current = current.unwrap_or_else(|| {
            tr(
                cx,
                if layer_is_database {
                    "当前数据库"
                } else {
                    "当前命名空间"
                },
            )
            .to_string()
        });
        Some(dropdown_select(
            id,
            options,
            SharedString::from(current),
            move |value: SharedString, window, cx| {
                let Some(entity) = view.upgrade() else {
                    return;
                };
                entity.update(cx, |this, cx| {
                    // 库这一层必须走「真切会话」的那条路；schema 层保持原样。
                    if layer_is_database {
                        this.select_database(value.to_string(), window, cx);
                    } else {
                        this.select_schema(Some(value.to_string()), cx);
                    }
                });
            },
            cx,
        ))
    }

    /// AI 助手条：厂商选择 + 提问输入 + 生成按钮。
    ///
    /// 厂商档案与 API key **不再在 db 页单独维护**：这里只从助手页那份档案列表里
    /// 选一条（[`crate::ai::load_providers`]），key 走**同一条** vault 密文通道
    /// （[`crate::ai::load_api_key`]）。没配过任何厂商时给出去「AI 助手」页配置的提示。
    fn render_ai_bar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();
        let providers = crate::ai::load_providers(cx);
        let active = self.active_ai_provider(cx);
        let options: Vec<(SharedString, SharedString)> = crate::ai::model_options(&providers)
            .into_iter()
            .map(|(id, label)| (SharedString::from(id), SharedString::from(label)))
            .collect();
        let mut bar = v_flex().w_full().gap(tokens.spacing.xs);

        // 厂商行：有档案给下拉，没有就给一句「去助手页配置」的提示。
        if options.is_empty() {
            bar = bar.child(h_flex().w_full().items_center().child(meta_text(
                tr(
                    cx,
                    "尚未配置 AI 厂商，请到「AI 助手」页添加厂商档案与 API key。",
                ),
                cx,
            )));
        } else {
            let current = SharedString::from(
                active
                    .as_ref()
                    .map(|profile| profile.id.clone())
                    .unwrap_or_default(),
            );
            bar = bar.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(dropdown_select(
                        "db-ai-provider",
                        options,
                        current,
                        {
                            let view = view.clone();
                            move |value: SharedString, _window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.ai_provider = Some(value.to_string());
                                    cx.notify();
                                });
                            }
                        },
                        cx,
                    )),
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
                                    this.run_ai_action(AiAction::Generate, window, cx);
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

    // ------------------------------------------------------------------
    // 结果分页（「加载更多」）与错误上下文条。
    // ------------------------------------------------------------------

    /// 「加载更多」：以**已加载行数**为 `offset` 再取一页，**追加**到已有结果集。
    ///
    /// 与「重跑整条查询」的区别：这里不重建视图（不换 `QueryView`、不清勾选 / 暂存改动），
    /// 只把新行接到尾部——滚动位置、已选行、列宽都原样保留。
    /// 走的是可取消 + 超时的读路径（与表页加载同一档）。
    fn load_more_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        if !self.query_has_more {
            return;
        }
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
        // 与 `execute_sql` 同理：取消句柄必须在开工前抓。
        let cancel = conn.lock().expect("连接锁不应中毒").cancel_handle();
        let timeout = self.effective_timeout();
        let offset = self.query_loaded;
        // 与首屏同一条上限：否则「加载更多」的页会比首屏大 / 小一截。
        let row_cap = query_row_cap(cx);
        self.status = tr(cx, "正在加载更多…").into();
        cx.notify();
        self.spawn_db_guarded(
            cx,
            session_id,
            cancel,
            timeout,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.execute_paged(&sql, offset, row_cap)
            },
            move |this, result, window, cx| match result {
                Ok(page) => this.append_query_page(page, cx),
                Err(error) => {
                    if matches!(error, DbError::Cancelled(_)) {
                        let text = this.cancelled_status(cx);
                        this.status = text.clone();
                        notify::info(text, window, cx);
                        return;
                    }
                    this.status = trf(cx, "加载更多失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "加载更多失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 把一页新结果**追加**到当前结果集尾部（不重建视图）。
    fn append_query_page(&mut self, page: ResultSet, cx: &mut Context<Self>) {
        let added = page.row_count();
        let more = page.truncated;
        let Some(view) = self.query_result.as_mut() else {
            return;
        };
        let Some(existing) = view.result.as_mut() else {
            return;
        };
        // 列集合不变：追加行即可（列宽 / 勾选 / 暂存改动都按原下标继续有效）。
        existing.rows.extend(page.rows);
        existing.truncated = more;
        self.query_loaded += added as u64;
        self.query_has_more = more;
        self.status = if more {
            trf(
                cx,
                "已加载 {} 行，可继续加载",
                &[&self.query_loaded.to_string()],
            )
            .into()
        } else {
            trf(
                cx,
                "已加载 {} 行（已到末页）",
                &[&self.query_loaded.to_string()],
            )
            .into()
        };
        cx.notify();
    }

    /// 「加载更多」条：结果被行上限截断、且能续拉时，出现在网格下方。
    fn render_load_more_bar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                button(
                    "db-load-more",
                    tr(cx, "加载更多"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.load_more_query(window, cx));
                    }
                }),
            )
            .child(meta_text(
                SharedString::from(trf(
                    cx,
                    "已加载 {} 行；结果被行上限截断",
                    &[&self.query_loaded.to_string()],
                )),
                cx,
            ))
            .into_any_element()
    }

    /// 执行失败条：错误摘要 + 「AI 修错 / 解释」+「关闭」。
    fn render_error_bar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(error) = self.last_error.as_ref() else {
            return div().into_any_element();
        };
        let message = error.message.clone();
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
            .child(div().flex_1().min_w_0().truncate().child(meta_text(
                SharedString::from(trf(cx, "执行失败：{}", &[&message])),
                cx,
            )))
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
                button("db-error-dismiss", tr(cx, "关闭"), ButtonVariant::Ghost, cx).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.last_error = None;
                            this.ai_explain = None;
                            cx.notify();
                        });
                    }
                }),
            )
            .into_any_element()
    }

    /// AI 解释面板：展示 `explain` 动作的正文；**只展示，绝不执行**。
    fn render_explain_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(text) = self.ai_explain.clone() else {
            return div().into_any_element();
        };
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
                    .child(caption_heading(tr(cx, "AI 解释"), cx))
                    .child(
                        button(
                            "db-ai-explain-close",
                            tr(cx, "关闭"),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.ai_explain = None;
                                    cx.notify();
                                });
                            }
                        }),
                    ),
            )
            // 解释可能很长：面板有高度上限，超出自己滚，不把结果区挤没。
            .child(
                div()
                    .id("db-ai-explain-body")
                    .w_full()
                    .max_h(px(AI_EXPLAIN_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.foreground)
                    .child(SharedString::from(text)),
            )
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
        // 实例信息页走另一条正文（不是「编辑器 + 结果区」）：它没有 SQL 可编辑，
        // 也不产出结果集，硬塞进结果区会让人以为「查询返回了这些」。
        if let Mode::Instance(tab) = self.mode {
            return self.render_instance_page(tab, cx);
        }
        // 仪表板同理走另一条正文：它一屏是**好几块**面板，没有「当前结果集」这东西，
        // 塞进结果区既摆不下也会让人以为「查询返回了这些」。
        if let Mode::Dashboard(board_id) = self.mode {
            return self.render_dashboard_page(board_id, cx);
        }
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let height_range = px(*SQL_EDITOR_HEIGHT_RANGE.start())..px(*SQL_EDITOR_HEIGHT_RANGE.end());
        // 逐条 ▶ 的定位数据：语句起始行 × 行高 − 编辑器滚动偏移（行高读编辑器
        // 实际布局值，未布局完退回 mono_md 档；偏移由 `cx.observe(&sql)` 带动重渲跟随）。
        let sql_text = self.sql.read(cx).value().to_string();
        let editor_line_height = self
            .sql
            .read(cx)
            .line_height()
            .unwrap_or(tokens.typography.mono_md.line_height);
        let editor_scroll_y = self.sql.read(cx).scroll_offset().y;
        let statements: Vec<(usize, String)> = split_statement_spans(&sql_text)
            .iter()
            .map(|span| {
                let line = sql_text.as_bytes()[..span.start]
                    .iter()
                    .filter(|&&byte| byte == b'\n')
                    .count();
                (line, span.text.clone())
            })
            .collect();

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
            // 执行失败提示条 + AI 解释面板：都挂在编辑器上方（flex_none、各自有上限，
            // 不挤压结果区）。错误条给「AI 修错 / 解释」一个稳定入口——通知会消失，
            // 错误上下文不会。
            .when(self.last_error.is_some(), |body| {
                body.child(self.render_error_bar(cx))
            })
            .when(self.ai_explain.is_some(), |body| {
                body.child(self.render_explain_panel(cx))
            })
            .child(
                div().flex_1().min_h_0().min_w_0().child(
                    v_resizable("db-sql-split")
                        .with_state(&self.sql_split)
                        .child(
                            resizable_panel().size_range(height_range).child(
                                // ⚠️ 这里**不能用 `h_flex()`**：它的 cross-axis 规则是
                                // 「居中」，编辑器会被收成一行内容高。要用显式
                                // `.flex().flex_row()` 拿默认的 stretch——左右两列
                                // 才都能撑满面板高度（执行列靠 `pt` 把 ▶ 顶到第一行）。
                                div()
                                    .flex()
                                    .flex_row()
                                    .size_full()
                                    .border_b(extended.stroke.thin)
                                    .border_color(extended.colors.hairline)
                                    // 左侧执行列（dbx 同款）：每条**完整语句**一个 ▶，
                                    // 钉在语句首行外侧、只执行那一条（与全文同一条危险
                                    // 确认 / 执行路径）；执行全文走工具条「执行」或 ⌘↵。
                                    .child(
                                        div()
                                            .relative()
                                            .flex_none()
                                            .w(theme.density().control + tokens.spacing.xs)
                                            .overflow_hidden()
                                            .children(statements.iter().enumerate().map(
                                                |(index, (line, text))| {
                                                    let button_height = theme.density().control;
                                                    let top = editor_line_height * *line as f32
                                                        - editor_scroll_y
                                                        + (editor_line_height - button_height)
                                                            / 2.0;
                                                    let statement = text.clone();
                                                    icon_button(
                                                        SharedString::from(format!(
                                                            "db-stmt-run-{index}"
                                                        )),
                                                        SharedString::from(trf(
                                                            cx,
                                                            "执行第 {} 条语句",
                                                            &[&(*line + 1).to_string()],
                                                        )),
                                                        Icon::new(DuckIcon::Play)
                                                            .size(extended.icon.sm),
                                                        ButtonVariant::Ghost,
                                                        cx,
                                                    )
                                                    .disabled(self.busy)
                                                    .on_click({
                                                        let view = view.clone();
                                                        move |_, window, cx| {
                                                            let _ = view.update(cx, |this, cx| {
                                                                this.run_statement(
                                                                    statement.clone(),
                                                                    window,
                                                                    cx,
                                                                )
                                                            });
                                                        }
                                                    })
                                                    // absolute 子项：top 已含滚动修正，
                                                    // 相对本列定位（列自身 overflow_hidden 裁剪）。
                                                    .absolute()
                                                    .top(top)
                                                    .left_0()
                                                    .into_any_element()
                                                },
                                            )),
                                    )
                                    .child(
                                        div().flex_1().min_w_0().child(
                                            Editor::new(&self.sql)
                                                .bordered(false)
                                                .p_0()
                                                .h_full()
                                                .w_full()
                                                .text_size(tokens.typography.mono_md.size),
                                        ),
                                    ),
                            ),
                        )
                        .child(resizable_panel().child(self.render_result(cx))),
                ),
            )
            .into_any_element()
    }

    /// 实例信息页（左栏连接节点的四个入口）。
    ///
    /// 四档共用一套骨架：页头（图标 + 标题 + 刷新）+ 内容区。内容区按档位给不同的卡，
    /// 取数状态（还没取 / 取失败）由**同一句提示**表达——三档都一样，不必各写一遍。
    fn render_instance_page(&self, tab: InstanceTab, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let subtitle: SharedString = match self.active_config() {
            Some(cfg) => {
                SharedString::from(format!("{} · {}", kind_label(cfg.kind), cfg.display_name()))
            }
            None => SharedString::from(tr(cx, "未连接")),
        };
        let mut header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                Icon::new(instance_tab_icon(tab))
                    .size(extended.icon.lg)
                    .text_color(extended.colors.accent_text),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(tokens.typography.md.size)
                            .line_height(tokens.typography.md.line_height)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(tab.heading(cx))),
                    )
                    .child(meta_text(subtitle, cx)),
            );
        if tab.fetches_server() {
            header = header.child(
                icon_text_button(
                    "db-instance-refresh",
                    tr(cx, "刷新"),
                    Icon::new(DuckIcon::RotateCw)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.busy)
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.refresh_instance_tab(tab, window, cx);
                    });
                }),
            );
        }

        let mut column = v_flex()
            .w_full()
            .gap(tokens.spacing.md)
            .child(header)
            .child(match tab {
                InstanceTab::Dashboard => self.render_instance_dashboard(cx),
                InstanceTab::Overview => self.render_instance_overview(cx),
                InstanceTab::Metrics => self.render_instance_metrics(cx),
                InstanceTab::Inspector => self.render_instance_inspector(cx),
            });
        // 还没取到的档位：把「正在读」写在内容区，而不是留一片空白让人以为坏了。
        if tab.fetches_server() && !self.instance_tab_loaded(tab) {
            column = column.child(meta_text(
                if self.busy {
                    tr(cx, "正在读取实例信息…")
                } else {
                    tr(cx, "还没有读取实例信息，点右上角「刷新」重试。")
                },
                cx,
            ));
        }
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .id("db-instance-page")
            .overflow_y_scroll()
            .children(vec![
                div()
                    .w_full()
                    .p(tokens.spacing.md)
                    .child(column)
                    .into_any_element(),
            ])
            .into_any_element()
    }

    /// 这一档的服务器数据到手了没有。
    fn instance_tab_loaded(&self, tab: InstanceTab) -> bool {
        match tab {
            InstanceTab::Dashboard => true,
            InstanceTab::Overview => self.instance_data.overview.is_some(),
            InstanceTab::Metrics => self.instance_data.metrics.is_some(),
            InstanceTab::Inspector => self.instance_data.processes.is_some(),
        }
    }

    /// 丢弃某一档的缓存后重取（页头「刷新」）。
    fn refresh_instance_tab(
        &mut self,
        tab: InstanceTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match tab {
            InstanceTab::Dashboard => {}
            InstanceTab::Overview => self.instance_data.overview = None,
            InstanceTab::Metrics => self.instance_data.metrics = None,
            InstanceTab::Inspector => self.instance_data.processes = None,
        }
        // 仪表盘没有服务器数据可重取，但它的对象计数来自会话内省 ——
        // 顺手把目录也刷一遍，省得用户跑去左栏点第二个刷新按钮。
        if tab == InstanceTab::Dashboard {
            self.refresh_state(window, cx);
            return;
        }
        self.open_instance_tab(tab, window, cx);
    }

    // ------------------------------------------------------------------
    // 用户仪表板（12 栏网格）—— 渲染。
    // ------------------------------------------------------------------

    /// 仪表板页：页头（查看 / 编辑、刷新、加面板）+ 12 栏网格。
    ///
    /// 网格按**流式**铺：每行的栏数加起来不超过 12（[`pack_dashboard_rows`]），面板宽窄
    /// 用相对宽度表示（`span / 12`），所以换个窗口宽度不会错位。面板之间的空隙由每块
    /// 面板自己的内边距让出来 —— 而不是给行加 `gap`，那样一行的总宽会超过 100%。
    fn render_dashboard_page(&self, board_id: u32, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(board) = self.dashboards.iter().find(|board| board.id == board_id) else {
            return instance_hint(tr(cx, "这块仪表板已经不在了。"), cx);
        };
        let conn = self.active.clone().unwrap_or_default();
        let editing = self.dashboard_editing;

        let mut header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                Icon::new(DuckIcon::Columns2)
                    .size(extended.icon.lg)
                    .text_color(extended.colors.accent_text),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(tokens.typography.md.size)
                            .line_height(tokens.typography.md.line_height)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(board.name.clone())),
                    )
                    .child(meta_text(
                        if editing {
                            tr(cx, "编辑中：面板可以改宽、前挪后挪、换内容。")
                        } else {
                            tr(cx, "12 栏网格")
                        },
                        cx,
                    )),
            );

        // 「添加面板」只在编辑态出现：查看态不该长出一排编辑按钮。
        if editing {
            let chart_ids: Vec<(String, String)> = self
                .saved_charts
                .iter()
                .map(|chart| (chart.id.clone(), chart.name.clone()))
                .collect();
            let supports_instance = self
                .active_session()
                .is_some_and(|session| session.kind.supports_instance_insight());
            header = header.child(toolbar_dropdown_button(
                "db-dashboard-add",
                tr(cx, "添加面板"),
                Icon::new(DuckIcon::Plus)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.muted_foreground),
                Anchor::BottomRight,
                {
                    let view = view.clone();
                    move |menu, _, cx| {
                        // 图表面板默认落在**第一条**已保存图表上：加完面板在面板自己的
                        // 下拉里换（那条下拉列全量），比在这里再套一层选择器少一次点击。
                        // 一条都没有时这一项禁用 —— 「点了才知道不行」不如直接不给点。
                        let add_chart = chart_ids.first().map(|(id, _)| id.clone());
                        let chart_view = view.clone();
                        let divider_view = view.clone();
                        let inspector_view = view.clone();
                        menu.item(
                            PopupMenuItem::new(tr(cx, "图表面板"))
                                .disabled(add_chart.is_none())
                                .on_click(move |_, _, cx| {
                                    let Some(chart_id) = add_chart.clone() else {
                                        return;
                                    };
                                    let _ = chart_view.update(cx, |this, cx| {
                                        this.add_dashboard_panel(
                                            board_id,
                                            DashboardPanelKind::Chart {
                                                saved_chart_id: chart_id,
                                            },
                                            cx,
                                        );
                                    });
                                }),
                        )
                        .item(PopupMenuItem::new(tr(cx, "分隔标题")).on_click(
                            move |_, window, cx| {
                                let _ = divider_view.update(cx, |this, cx| {
                                    let panel_id = this.add_dashboard_panel(
                                        board_id,
                                        DashboardPanelKind::Divider {
                                            title: tr(cx, "分组").to_owned(),
                                        },
                                        cx,
                                    );
                                    // 加完直接让用户命名：占位名「分组」只是为了让面板
                                    // 先立住（不命名的话框里是空的，像坏了）。
                                    if let Some(panel_id) = panel_id {
                                        this.open_divider_title_dialog(
                                            board_id,
                                            panel_id,
                                            String::new(),
                                            window,
                                            cx,
                                        );
                                    }
                                });
                            },
                        ))
                        // 引擎没这个能力就禁用（与左栏那三个入口同一判据）：画一个点开
                        // 只会报「不支持」的面板，和画一个点不动的按钮一样糟。
                        .item(
                            PopupMenuItem::new(tr(cx, "实例检查器"))
                                .disabled(!supports_instance)
                                .on_click(move |_, _, cx| {
                                    let _ = inspector_view.update(cx, |this, cx| {
                                        this.add_dashboard_panel(
                                            board_id,
                                            DashboardPanelKind::Inspector {
                                                view: InspectorView::Metrics,
                                            },
                                            cx,
                                        );
                                    });
                                }),
                        )
                    }
                },
                cx,
            ));
        }

        header = header.child(
            icon_text_button(
                "db-dashboard-refresh",
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
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.refresh_dashboard(cx));
                }
            }),
        );
        header = header.child(if editing {
            button(
                "db-dashboard-view",
                tr(cx, "完成编辑"),
                ButtonVariant::Primary,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.dashboard_editing = false;
                        cx.notify();
                    });
                }
            })
        } else {
            button(
                "db-dashboard-edit",
                tr(cx, "编辑"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.dashboard_editing = true;
                        cx.notify();
                    });
                }
            })
        });

        // ── 网格 ──
        let visible = visible_dashboard_panels(&board.panels, &self.dashboard_collapsed);
        let spans: Vec<u32> = visible
            .iter()
            .map(|index| dashboard_panel_span(&board.panels[*index]))
            .collect();
        let mut grid = v_flex().w_full().gap(tokens.spacing.xs);
        if visible.is_empty() {
            grid = grid.child(
                empty_state(
                    DuckIcon::Columns2,
                    tr(cx, "这块仪表板还是空的"),
                    Some(
                        tr(
                            cx,
                            "点右上角「编辑」，再点「添加面板」摆上图表、分隔标题或实例检查器。",
                        )
                        .into(),
                    ),
                    None,
                    cx,
                )
                .w_full()
                .into_any_element(),
            );
        }
        for row in pack_dashboard_rows(&spans) {
            let mut line = h_flex().w_full().items_stretch();
            for slot in row {
                let panel = &board.panels[visible[slot]];
                line = line.child(
                    div()
                        .w(relative(column_fraction(dashboard_panel_span(panel))))
                        .p(tokens.spacing.xs)
                        .child(self.render_dashboard_panel(&conn, board, panel, cx)),
                );
            }
            grid = grid.child(line);
        }

        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .id("db-dashboard-page")
            .overflow_y_scroll()
            .children(vec![
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.md)
                    .p(tokens.spacing.md)
                    .child(header)
                    .child(grid)
                    .into_any_element(),
            ])
            .into_any_element()
    }

    /// 一块面板：标题条（编辑态带控件）+ 内容。
    fn render_dashboard_panel(
        &self,
        conn: &str,
        board: &Dashboard,
        panel: &DashboardPanel,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let editing = self.dashboard_editing;
        let fetch = self
            .dashboard_results
            .get(&(conn.to_owned(), board.id, panel.id));

        // ── 分隔标题：整行横条，点一下折起它统领的那一段 ──
        if let DashboardPanelKind::Divider { title } = &panel.kind {
            let panel_id = panel.id;
            let collapsed = self.dashboard_collapsed.contains(&panel_id);
            let mut strip = h_flex()
                .id(("db-dash-divider", panel_id))
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .px(tokens.spacing.xs)
                .py(tokens.spacing.xxs)
                .rounded(tokens.radius.sm)
                .cursor_pointer()
                .role(Role::Button)
                .aria_label(SharedString::from(if collapsed {
                    trf(cx, "展开「{}」这一段", &[title])
                } else {
                    trf(cx, "折起「{}」这一段", &[title])
                }))
                .hover(move |style| style.bg(extended.colors.hover_tint))
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.toggle_dashboard_divider(panel_id, cx);
                        });
                    }
                })
                .child(
                    Icon::new(if collapsed {
                        DuckIcon::ChevronRight
                    } else {
                        DuckIcon::ChevronDown
                    })
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.colors.foreground)
                        .truncate()
                        .child(SharedString::from(title.clone())),
                )
                .child(div().flex_1());
            if editing {
                strip = strip.child(self.dashboard_divider_controls(board, panel, cx));
            }
            return strip.into_any_element();
        }

        // ── 其余两类：卡片 = 标题条 + 内容 ──
        let (icon, title) = match &panel.kind {
            DashboardPanelKind::Chart { saved_chart_id } => (
                DuckIcon::ChartLine,
                self.saved_charts
                    .iter()
                    .find(|chart| &chart.id == saved_chart_id)
                    .map(|chart| chart.name.clone())
                    .unwrap_or_else(|| tr(cx, "已删除的图表").to_owned()),
            ),
            DashboardPanelKind::Inspector { view: tab } => (
                // 与左栏那两个入口共用同一套图标：面板上的「实例指标」和左栏的
                // 「实例指标」是同一份数据，不该画成两个样子。
                instance_tab_icon(match tab {
                    InspectorView::Metrics => InstanceTab::Metrics,
                    InspectorView::Processes => InstanceTab::Inspector,
                }),
                tab.heading(cx).to_owned(),
            ),
            // 分隔标题已在上面 return。
            DashboardPanelKind::Divider { .. } => (DuckIcon::Columns2, String::new()),
        };
        let mut head = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(
                Icon::new(icon)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(tokens.colors.muted_foreground)
                    .child(SharedString::from(title)),
            );
        if editing {
            head = head.child(self.dashboard_panel_controls(board, panel, cx));
        }

        let content = match &panel.kind {
            DashboardPanelKind::Chart { .. } => self.render_dashboard_chart_body(panel, fetch, cx),
            DashboardPanelKind::Inspector { .. } => match fetch {
                Some(PanelFetch::Table(table)) => self.instance_table_card(table, cx),
                Some(PanelFetch::Failed(message)) => meta_text(
                    SharedString::from(trf(cx, "读取失败：{}", &[message.as_str()])),
                    cx,
                )
                .into_any_element(),
                _ => meta_text(tr(cx, "还没有数据，点右上角「刷新」。"), cx).into_any_element(),
            },
            DashboardPanelKind::Divider { .. } => div().into_any_element(),
        };

        v_flex()
            .size_full()
            .min_w_0()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .rounded(tokens.radius.md)
            .bg(tokens.colors.surface)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(head)
            .child(content)
            .into_any_element()
    }

    /// 图表面板的内容：按面板引用的那条已保存图表的列映射出图。
    fn render_dashboard_chart_body(
        &self,
        panel: &DashboardPanel,
        fetch: Option<&PanelFetch>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let DashboardPanelKind::Chart { saved_chart_id } = &panel.kind else {
            return div().into_any_element();
        };
        let Some(PanelFetch::Chart(result)) = fetch else {
            return match fetch {
                Some(PanelFetch::MissingChart) => {
                    meta_text(tr(cx, "引用的已保存图表已被删除；在编辑态里换一条。"), cx)
                        .into_any_element()
                }
                Some(PanelFetch::NoRows) => {
                    meta_text(tr(cx, "这条语句没有返回结果集。"), cx).into_any_element()
                }
                Some(PanelFetch::Failed(message)) => meta_text(
                    SharedString::from(trf(cx, "读取失败：{}", &[message.as_str()])),
                    cx,
                )
                .into_any_element(),
                _ => meta_text(tr(cx, "还没有数据，点右上角「刷新」。"), cx).into_any_element(),
            };
        };
        // 面板引用的图表**可能已被删**（那时面板还能立着，但配置没了），列映射对不上
        // 就回落默认映射 —— 与结果区的图表视角同一条口径。
        let stored = self
            .saved_charts
            .iter()
            .find(|chart| &chart.id == saved_chart_id)
            .map(|chart| chart.config());
        let config = match stored {
            Some(config) if config.is_valid_for(result) => config,
            _ => match ChartConfig::from_result(result) {
                Some(fallback) => fallback,
                None => {
                    return meta_text(tr(cx, "这份结果里没有数值列，画不出图。"), cx)
                        .into_any_element();
                }
            },
        };
        let mut column = v_flex().w_full().min_w_0().gap(tokens.spacing.xs);
        for series in config.effective_y() {
            column =
                column.child(self.chart_panel(result, &config, series, DASHBOARD_CHART_HEIGHT, cx));
        }
        column.into_any_element()
    }

    /// 编辑态下**普通面板**标题条右侧那排控件：换内容 / 前挪后挪 / 改宽 / 移除。
    fn dashboard_panel_controls(
        &self,
        board: &Dashboard,
        panel: &DashboardPanel,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let board_id = board.id;
        let panel_id = panel.id;
        let index = board
            .panels
            .iter()
            .position(|candidate| candidate.id == panel_id)
            .unwrap_or_default();
        let last = board.panels.len().saturating_sub(1);

        let mut row = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);

        // 换内容：图表面板列出全部已保存图表，检查器面板列出两档表。
        match &panel.kind {
            DashboardPanelKind::Chart { saved_chart_id } => {
                if !self.saved_charts.is_empty() {
                    let options: Vec<(SharedString, SharedString)> = self
                        .saved_charts
                        .iter()
                        .map(|chart| {
                            (
                                SharedString::from(chart.id.clone()),
                                SharedString::from(chart.name.clone()),
                            )
                        })
                        .collect();
                    row = row.child(dropdown_select(
                        SharedString::from(format!("db-dash-chart-{panel_id}")),
                        options,
                        SharedString::from(saved_chart_id.clone()),
                        {
                            let view = view.clone();
                            move |value: SharedString, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.set_dashboard_chart(
                                        board_id,
                                        panel_id,
                                        value.to_string(),
                                        cx,
                                    );
                                });
                            }
                        },
                        cx,
                    ));
                }
            }
            DashboardPanelKind::Inspector { view: current } => {
                let options: Vec<(SharedString, SharedString)> = InspectorView::ALL
                    .into_iter()
                    .map(|tab| {
                        (
                            SharedString::from(tab.key()),
                            SharedString::from(tab.heading(cx)),
                        )
                    })
                    .collect();
                row = row.child(dropdown_select(
                    SharedString::from(format!("db-dash-inspector-{panel_id}")),
                    options,
                    SharedString::from(current.key()),
                    {
                        let view = view.clone();
                        move |value: SharedString, _, cx| {
                            let Some(tab) = InspectorView::from_key(&value) else {
                                return;
                            };
                            let _ = view.update(cx, |this, cx| {
                                this.set_dashboard_inspector(board_id, panel_id, tab, cx);
                            });
                        }
                    },
                    cx,
                ));
            }
            DashboardPanelKind::Divider { .. } => {}
        }

        // 前挪 / 后挪。
        for (tag, icon, label, delta, disabled) in [
            (
                "move-prev",
                DuckIcon::ChevronLeft,
                tr(cx, "前挪"),
                -1_i32,
                index == 0,
            ),
            (
                "move-next",
                DuckIcon::ChevronRight,
                tr(cx, "后挪"),
                1_i32,
                index >= last,
            ),
        ] {
            row = row.child(
                icon_button(
                    SharedString::from(format!("db-dash-{tag}-{panel_id}")),
                    label,
                    Icon::new(icon)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(disabled)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.move_dashboard_panel(board_id, panel_id, delta, cx);
                        });
                    }
                }),
            );
        }

        // 改宽：−1 / +1 栏 + 整行。
        for (tag, icon, label, span, disabled) in [
            (
                "narrow",
                DuckIcon::Minus,
                tr(cx, "变窄"),
                panel.span.saturating_sub(1),
                panel.span <= 1,
            ),
            (
                "widen",
                DuckIcon::Plus,
                tr(cx, "变宽"),
                panel.span + 1,
                panel.span >= DASHBOARD_COLUMNS,
            ),
            (
                "full",
                DuckIcon::Columns2,
                tr(cx, "整行"),
                DASHBOARD_COLUMNS,
                panel.span >= DASHBOARD_COLUMNS,
            ),
        ] {
            row = row.child(
                icon_button(
                    SharedString::from(format!("db-dash-{tag}-{panel_id}")),
                    label,
                    Icon::new(icon)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(disabled)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.set_dashboard_span(board_id, panel_id, span, cx);
                        });
                    }
                }),
            );
        }

        row = row.child(
            icon_button(
                SharedString::from(format!("db-dash-remove-{panel_id}")),
                tr(cx, "移除面板"),
                Icon::new(DuckIcon::X)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.remove_dashboard_panel(board_id, panel_id, cx);
                    });
                }
            }),
        );
        row.into_any_element()
    }

    /// 编辑态下**分隔标题**右侧的控件：只有「改标题」与「移除」。
    ///
    /// 分隔条不做改宽 / 挪位以外的排布动作：它本来就是整行横条，宽度没有意义。
    fn dashboard_divider_controls(
        &self,
        board: &Dashboard,
        panel: &DashboardPanel,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let board_id = board.id;
        let panel_id = panel.id;
        let title = match &panel.kind {
            DashboardPanelKind::Divider { title } => title.clone(),
            _ => String::new(),
        };
        h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .child(
                icon_button(
                    SharedString::from(format!("db-dash-rename-{panel_id}")),
                    tr(cx, "改标题"),
                    Icon::new(DuckIcon::Pencil)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.open_divider_title_dialog(
                                board_id,
                                panel_id,
                                title.clone(),
                                window,
                                cx,
                            );
                        });
                    }
                }),
            )
            .child(
                icon_button(
                    SharedString::from(format!("db-dash-remove-panel-{panel_id}")),
                    tr(cx, "移除面板"),
                    Icon::new(DuckIcon::X)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.remove_dashboard_panel(board_id, panel_id, cx);
                        });
                    }
                }),
            )
            .into_any_element()
    }

    /// 仪表盘：这条连接的**构成**与**最近做过的事**（全用界面已有的数据）。
    fn render_instance_dashboard(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(session) = self.active_session() else {
            return instance_hint(tr(cx, "先连上一条连接。"), cx);
        };
        let mut cards = v_flex().w_full().gap(tokens.spacing.md);

        // ── 连接信息 ──
        if let Some(cfg) = self.active_config() {
            let mut rows: Vec<(SharedString, SharedString)> = vec![
                ("engine".into(), kind_label(cfg.kind).into()),
                ("host".into(), cfg.host.clone().into()),
                (
                    "port".into(),
                    cfg.port
                        .or_else(|| cfg.kind.default_port())
                        .map(|port| port.to_string())
                        .unwrap_or_default()
                        .into(),
                ),
                ("database".into(), cfg.database.clone().into()),
                ("user".into(), cfg.user.clone().into()),
            ];
            if let Some(group) = cfg.group.as_deref().filter(|group| !group.is_empty()) {
                rows.insert(0, ("group".into(), group.to_owned().into()));
            }
            rows.push(("server_version".into(), session.version.clone().into()));
            cards = cards.child(self.instance_kv_card(tr(cx, "连接信息"), rows, cx));
        }

        // ── 对象构成：左栏树上的那些分类，这里给个数 ──
        let mut objects: Vec<(SharedString, SharedString)> = TreeCategory::ALL
            .iter()
            .filter_map(|category| {
                let count = session
                    .catalog
                    .iter()
                    .filter(|entry| category.matches(entry.kind))
                    .count()
                    + session.database_objects(*category).len();
                // 计数为 0 的分类不列：这一页是「这里有什么」，不是「什么没有」。
                (count > 0).then(|| {
                    (
                        SharedString::from(category.heading(cx)),
                        SharedString::from(count.to_string()),
                    )
                })
            })
            .collect();
        objects.insert(
            0,
            (
                tr(cx, "命名空间").into(),
                collect_namespaces(&session.databases, &session.schemas)
                    .len()
                    .to_string()
                    .into(),
            ),
        );
        cards = cards.child(self.instance_kv_card(tr(cx, "对象构成"), objects, cx));

        // ── 最近查询：历史里最近几条，点一条载入编辑器 ──
        let mut recent = v_flex().w_full().gap(tokens.spacing.xxs);
        if self.history.is_empty() {
            recent = recent.child(meta_text(tr(cx, "还没有执行过语句。"), cx));
        }
        for sql in self.history.iter().take(5) {
            let view = self.view.clone();
            let load = sql.clone();
            recent = recent.child(
                sidebar_navigation_button(
                    SharedString::from(format!("db-dash-history-{sql}")),
                    SharedString::from(one_line(sql)),
                    Icon::new(DuckIcon::Clock)
                        .size(theme.extended().icon.md)
                        .text_color(tokens.colors.muted_foreground),
                    div().flex_none(),
                    false,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let Some(entity) = view.upgrade() else {
                        return;
                    };
                    let sql = load.clone();
                    entity.update(cx, |this, cx| {
                        this.fill_editor(sql, window, cx);
                        // 与历史面板同口径：放回编辑器不执行，状态栏也照那边说。
                        this.status = tr(cx, "已从历史放回编辑器。").into();
                        cx.notify();
                    });
                }),
            );
        }
        cards
            .child(instance_card(tr(cx, "最近查询"), recent, cx))
            .child(self.dashboard_transport_card(cx))
            .child(self.dashboard_hook_card(cx))
            .into_any_element()
    }

    /// 仪表盘上的「传输通道」卡：这条连接经什么出去、通道还活着吗。
    ///
    /// 为什么放在这一页：隧道 / 代理 / SSM 都是**外部进程或本机端口**，档案里那几栏
    /// 只说明「配了什么」，回答不了「现在到底通不通」。而通道一死，表现全是「查询超时」——
    /// 用户会去查库、查网络，最后才想到「ssh 早就退出了」。这一卡把这个弯路截掉。
    fn dashboard_transport_card(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let active = self.active.clone().unwrap_or_default();
        let transport = self
            .sessions
            .get(&active)
            .and_then(|session| session.transport.as_ref());
        // 判据要按**展开后的**档案算：只引用了一份代理档案的连接，`active_config()` 上
        // 三档全是 `None`，直接算会显示「直连」。
        let expanded = self
            .active_config()
            .map(|cfg| expand_tunnel_profile(cfg, &self.tunnel_profiles).0);
        let configured = expanded
            .as_ref()
            .map_or(TransportKind::Direct, ConnectionConfig::transport);
        let mut body = v_flex().w_full().gap(tokens.spacing.xxs);
        match transport {
            Some(transport) => {
                let alive = transport.is_alive();
                body = body
                    .child(description_list(vec![
                        description_row(tr(cx, "接入方式"), transport.label(cx)),
                        description_row(tr(cx, "本机端口"), transport.local_port().to_string()),
                    ]))
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .child(
                                Icon::new(if alive {
                                    DuckIcon::CircleCheck
                                } else {
                                    DuckIcon::CircleAlert
                                })
                                .size(extended.icon.md)
                                .text_color(if alive {
                                    extended.colors.success
                                } else {
                                    tokens.colors.destructive
                                }),
                            )
                            .child(meta_text(
                                if alive {
                                    tr(cx, "通道正常。")
                                } else {
                                    tr(cx, "通道已断开，请重新连接。")
                                },
                                cx,
                            )),
                    );
            }
            None => {
                body = body.child(meta_text(
                    if configured == TransportKind::Direct {
                        tr(cx, "直连：没有配置隧道 / 代理。")
                    } else {
                        tr(cx, "档案配了传输通道，但当前会话还没建立。")
                    },
                    cx,
                ));
            }
        }
        // 配了但填不全的档位**不会生效**：不说出来的话，用户会以为自己已经在走代理。
        if expanded
            .as_ref()
            .is_some_and(ConnectionConfig::has_incomplete_transport)
        {
            body = body.child(meta_text(tr(cx, "有一档传输配置没填完，不会生效。"), cx));
        }
        instance_card(tr(cx, "传输通道"), body, cx)
    }

    /// 仪表盘上的「连接钩子」卡：这条连接最近的钩子执行（成功 / 失败一眼可见）。
    ///
    /// 为什么放在这一页：钩子是**静默**跑的（失败默认也不拦），平时没有任何界面会提到它；
    /// 而这一页的定位正是「这条连接的构成与最近做过的事」，钩子恰好属于后者。
    fn dashboard_hook_card(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let active = self.active.clone().unwrap_or_default();
        let records: Vec<&HookRecord> = self
            .hook_log
            .iter()
            .rev()
            .filter(|record| record.connection_id == active)
            .take(5)
            .collect();
        let configured = self
            .active_config()
            .is_some_and(|cfg| !cfg.hooks.is_empty());
        let mut body = v_flex().w_full().gap(tokens.spacing.xxs);
        if records.is_empty() {
            // 空态分两种：配了但还没跑（连上就会跑），和压根没配（去编辑档案加）。
            body = body.child(meta_text(
                if configured {
                    tr(cx, "这条连接配了钩子，还没跑过。")
                } else {
                    tr(cx, "这条连接没有配置钩子。")
                },
                cx,
            ));
        }
        let now = now_secs();
        for record in records {
            let ok = record.outcome.status.is_ok();
            body = body.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(
                        Icon::new(if ok {
                            DuckIcon::CircleCheck
                        } else {
                            DuckIcon::CircleAlert
                        })
                        .size(extended.icon.md)
                        .text_color(if ok {
                            extended.colors.success
                        } else {
                            tokens.colors.destructive
                        }),
                    )
                    .child(div().flex_1().min_w_0().child(meta_text(
                        SharedString::from(format!(
                            "{} · {} · {} · {}",
                            hook_phase_label(record.outcome.phase, cx),
                            one_line(&record.outcome.command),
                            hook_status_label(&record.outcome.status, cx),
                            tr_relative_time(cx, now - record.at, UnderMinute::JustNow),
                        )),
                        cx,
                    ))),
            );
        }
        instance_card(tr(cx, "连接钩子"), body, cx)
    }

    /// 实例概览：服务器级事实（版本 / 在线时长 / 体量 / 字符集…）。
    fn render_instance_overview(&self, cx: &App) -> AnyElement {
        let Some(overview) = self.instance_data.overview.as_ref() else {
            return div().into_any_element();
        };
        let mut rows: Vec<(SharedString, SharedString)> =
            vec![("version".into(), overview.version.clone().into())];
        if !overview.database.is_empty() {
            rows.push(("database".into(), overview.database.clone().into()));
        }
        if !overview.user.is_empty() {
            rows.push(("user".into(), overview.user.clone().into()));
        }
        if let Some(uptime) = overview.uptime.clone() {
            rows.push(("uptime".into(), uptime.into()));
        }
        for fact in &overview.facts {
            rows.push((
                SharedString::from(fact.name.clone()),
                SharedString::from(fact.value.clone()),
            ));
        }
        self.instance_kv_card(tr(cx, "实例概览"), rows, cx)
    }

    /// 实例指标：时序折线（本会话内累积的采样）+ 明细表。
    ///
    /// 两张东西的分工：折线看**走势**（需要历史），明细表看**此刻的值**（含画不进坐标系的
    /// 文案项，如体量 `8493 kB`）。只有一次采样时折线还画不出走势，那时如实说清楚
    /// 「多刷几次就有」，而不是画一条只有一个点的线让人以为指标是常数。
    fn render_instance_metrics(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(table) = self.instance_data.metrics.as_ref() else {
            return div().into_any_element();
        };
        let mut column = v_flex().w_full().gap(tokens.spacing.md);
        if let Some(history) = self
            .active
            .as_deref()
            .and_then(|id| self.metric_history.get(id))
        {
            let keys = history.keys();
            // 图上限：指标白名单有十几项，全铺出来是一屏碎图；取前几张（顺序即引擎的
            // 白名单顺序，最要紧的排前面）——剩下的在明细表里一个不少。
            let charts: Vec<String> = keys.into_iter().take(METRIC_CHART_LIMIT).collect();
            if charts.is_empty() || history.len() < 2 {
                column = column.child(meta_text(
                    if charts.is_empty() {
                        tr(cx, "这台服务器没报出可画的数值指标。")
                    } else {
                        tr(cx, "再刷新几次就会出现趋势（当前只有一次采样）。")
                    },
                    cx,
                ));
            } else {
                column = column.child(meta_text(
                    SharedString::from(trf(
                        cx,
                        "最近 {} 次采样 · 覆盖 {}",
                        &[
                            &history.len().to_string(),
                            &compact_duration(history.span_secs()),
                        ],
                    )),
                    cx,
                ));
                for chunk in charts.chunks(2) {
                    let mut row = h_flex().w_full().gap(tokens.spacing.md);
                    for key in chunk {
                        row = row.child(self.metric_chart_card(key, &history.series(key), cx));
                    }
                    // 奇数张时补一个空位，免得最后一张被拉成整行宽（两张图一行的节奏）。
                    if chunk.len() == 1 {
                        row = row.child(div().flex_1());
                    }
                    column = column.child(row);
                }
            }
        }
        column
            .child(self.instance_table_card(table, cx))
            .into_any_element()
    }

    /// 一张指标折线卡：标题 + 折线（横轴是「距现在多久」，纵轴是该指标的值）。
    fn metric_chart_card(&self, key: &str, series: &[(i64, f64)], cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        // 采样点：横轴文案用「距今」的紧凑时长（数字 + s/m/h，无需翻译），
        // 纵轴直接用值。`series` 为空时给一张只有标题的卡（界面不至于开天窗）。
        let points: Vec<(String, f64)> = series
            .iter()
            .map(|(at, value)| (compact_duration((now_secs() - at).max(0)), *value))
            .collect();
        let stroke = extended.colors.accent_text;
        let mut card = v_flex()
            .flex_1()
            .min_w_0()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .rounded(tokens.radius.md)
            .bg(tokens.colors.surface)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .truncate()
                    .child(SharedString::from(key.to_owned())),
            );
        if points.is_empty() {
            return card
                .child(meta_text(tr(cx, "没有采到值。"), cx))
                .into_any_element();
        }
        // 数据点全部交给图表（形状不丢）：抽稀是它自己的事。
        card = card.child(
            div().w_full().h(px(120.)).child(
                LineChart::new(points)
                    .id(SharedString::from(format!("db-metric-{key}")))
                    .x(|(label, _)| label.clone())
                    .y(|(_, value)| *value)
                    .stroke(stroke)
                    .x_axis(false)
                    .grid(true)
                    .y_tick_format(compact_number)
                    .tooltip_title(|(label, _)| SharedString::from(label.clone()))
                    .tooltip_value({
                        let name = SharedString::from(key.to_owned());
                        move |_, value| {
                            SharedString::from(format!("{name}: {}", compact_number(value)))
                        }
                    }),
            ),
        );
        card.into_any_element()
    }

    /// 实例检查器：服务端会话 / 连接现状（SQLite 是本连接挂载的数据库）。
    fn render_instance_inspector(&self, cx: &App) -> AnyElement {
        let Some(table) = self.instance_data.processes.as_ref() else {
            return div().into_any_element();
        };
        let mut column = v_flex().w_full().gap(active_theme(cx).tokens().spacing.sm);
        // 引擎口径的解释由**界面**给（内核只出数据，不出面向用户的散文）：
        // SQLite 是进程内数据库，没有服务端会话列表，能检查的只有挂载了哪些文件。
        if self
            .active_config()
            .is_some_and(|cfg| cfg.kind == DatabaseKind::Sqlite)
        {
            column = column.child(meta_text(
                tr(
                    cx,
                    "SQLite 是进程内数据库，没有服务端会话列表；下面是本连接挂载的数据库文件。",
                ),
                cx,
            ));
        }
        if table.rows.is_empty() {
            column = column.child(meta_text(tr(cx, "当前没有会话。"), cx));
        } else {
            column = column.child(self.instance_table_card(table, cx));
        }
        column.into_any_element()
    }

    /// 一张「名字 — 值」卡（实例概览与仪表盘共用）。
    ///
    /// 走全站既有的 `description_list` 而不是自绘行：右栏「连接信息」、行详情卡都是它，
    /// 标签列宽与行距才不会一处一个样。
    fn instance_kv_card(
        &self,
        title: impl Into<SharedString>,
        rows: impl IntoIterator<Item = (SharedString, SharedString)>,
        cx: &App,
    ) -> AnyElement {
        let items = rows
            .into_iter()
            .map(|(name, value)| description_row(name, value))
            .collect::<Vec<_>>();
        instance_card(title, description_list(items), cx)
    }

    /// 一张**通用表**卡（指标 / 检查器共用；列数与列名全由内核给）。
    fn instance_table_card(&self, table: &InstanceTable, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let mut grid = v_flex().w_full();
        // 表头
        let mut head = h_flex().w_full().py(tokens.spacing.xs);
        for column in &table.columns {
            head = head.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(tokens.colors.muted_foreground)
                    .child(div().truncate().child(SharedString::from(column.clone()))),
            );
        }
        grid = grid.child(head);
        for row in &table.rows {
            grid = grid.child(
                div()
                    .w_full()
                    .h(px(1.))
                    .pl(tokens.spacing.lg)
                    .bg(extended.colors.hairline),
            );
            let mut line = h_flex().w_full().py(tokens.spacing.xs);
            for cell in row {
                line = line.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .text_color(tokens.colors.foreground)
                        .child(div().truncate().child(SharedString::from(cell.clone()))),
                );
            }
            grid = grid.child(line);
        }
        instance_card(
            SharedString::from(trf(cx, "明细（{} 行）", &[&table.rows.len().to_string()])),
            grid,
            cx,
        )
    }

    /// 结果区的**图表视角**：配置条 + 图。
    ///
    /// 为什么是「小倍数」而不是叠线：`gpui-component` 的样式化图表一次只画**一条序列**
    /// （折线 / 面积内部就是一块单线 `Plot`，柱状是一带一值）。要叠几条线得自绘 `Plot`，
    /// 而把量纲不同的列（`count` 与 `amount`）塞进同一条 Y 轴本身就是错的。所以多选几列
    /// 时**每列一张小图**并排铺开——这是可视化里的标准做法（small multiples），也正好
    /// 是这块原语擅长的事。
    fn render_chart(&self, result: &ResultSet, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let Some(stored) = self.chart_view.clone() else {
            return div().into_any_element();
        };
        // 列名对不上（重跑后列变了）→ 回落默认映射；连数值列都没有就如实说。
        let config = if stored.is_valid_for(result) {
            stored
        } else if let Some(fallback) = ChartConfig::from_result(result) {
            fallback
        } else {
            return empty_state(
                DuckIcon::ChartLine,
                tr(cx, "这份结果里没有数值列"),
                Some(tr(cx, "图表至少要有一列数值；切回表格看原始结果。").into()),
                None,
                cx,
            )
            .flex_1()
            .into_any_element();
        };

        // ── 配置条：类型 / 横轴 / 数值列 / 保存 ──
        let kind_options: Vec<(SharedString, SharedString)> = ChartKind::ALL
            .into_iter()
            .map(|kind| {
                (
                    SharedString::from(kind.key()),
                    SharedString::from(kind.heading(cx)),
                )
            })
            .collect();
        let axis_options: Vec<(SharedString, SharedString)> = result
            .columns
            .iter()
            .map(|column| {
                (
                    SharedString::from(column.name.clone()),
                    SharedString::from(column.name.clone()),
                )
            })
            .collect();
        // 数值列勾选：到顶的未选项画成停用态（点不动看得见原因，不静默吞掉点击）。
        let capacity = config.kind.series_limit();
        let mut series_row = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(field_label(tr(cx, "数值列"), cx));
        let mut numeric_columns = 0_usize;
        for (index, column) in result.columns.iter().enumerate() {
            if !column_is_numeric(result, index) {
                continue;
            }
            numeric_columns += 1;
            let name = column.name.clone();
            let selected = config.y.iter().any(|picked| picked == &name);
            let at_capacity = !selected && config.y.len() >= capacity;
            let row = check_row(
                SharedString::from(format!("db-chart-series-{name}")),
                selected,
                div().child(SharedString::from(name.clone())),
                {
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.toggle_chart_series(&name, cx));
                    }
                },
                cx,
            );
            series_row = series_row.child(if at_capacity {
                row.opacity(extended.opacity.disabled)
            } else {
                row
            });
        }
        if numeric_columns == 0 {
            series_row = series_row.child(meta_text(tr(cx, "没有数值列。"), cx));
        }

        let controls = v_flex()
            .w_full()
            .flex_none()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .rounded(tokens.radius.md)
            .bg(tokens.colors.surface)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(field_label(tr(cx, "图表类型"), cx))
                    .child(dropdown_select(
                        "db-chart-kind",
                        kind_options,
                        SharedString::from(config.kind.key()),
                        {
                            let view = view.clone();
                            move |value: SharedString, _window, cx| {
                                let Some(kind) = ChartKind::from_key(&value) else {
                                    return;
                                };
                                let _ = view.update(cx, |this, cx| this.set_chart_kind(kind, cx));
                            }
                        },
                        cx,
                    ))
                    .child(field_label(tr(cx, "横轴"), cx))
                    .child(dropdown_select(
                        "db-chart-axis",
                        axis_options,
                        SharedString::from(config.x.clone()),
                        {
                            let view = view.clone();
                            move |value: SharedString, _window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.set_chart_axis(value.to_string(), cx);
                                });
                            }
                        },
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(
                        button(
                            "db-save-chart",
                            tr(cx, "保存图表"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(config.y.is_empty())
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.open_save_chart_dialog(window, cx);
                                });
                            }
                        }),
                    ),
            )
            .child(series_row);

        // ── 图：每列一张，两列一行 ──
        let series: Vec<String> = config.effective_y().to_vec();
        let mut body = v_flex().w_full().gap(tokens.spacing.md);
        if series.is_empty() {
            body = body.child(meta_text(
                tr(cx, "还没选数值列；勾一个上面列出的数值列就会出图。"),
                cx,
            ));
        }
        for chunk in series.chunks(2) {
            let mut row = h_flex().w_full().items_start().gap(tokens.spacing.md);
            for column in chunk {
                row = row.child(self.chart_panel(result, &config, column, CHART_PANEL_HEIGHT, cx));
            }
            // 奇数张时补一个空位，免得最后一张被拉成整行宽（两张一行的节奏）。
            if chunk.len() == 1 {
                row = row.child(div().flex_1());
            }
            body = body.child(row);
        }

        v_flex()
            .id("db-chart-body")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .gap(tokens.spacing.md)
            .p(tokens.spacing.md)
            .overflow_y_scroll()
            .child(controls)
            .child(body)
            .into_any_element()
    }

    /// 一张小图：标题（`数值列 · 横轴`）+ 图。
    ///
    /// `height` 由调用方给：结果区的图表视角一屏只放两张，图给得高（[`CHART_PANEL_HEIGHT`]）；
    /// 仪表板一块面板里可能叠好几张，得矮一些（[`DASHBOARD_CHART_HEIGHT`]）。除了高度，
    /// 两条路画的是**同一张图**（同一套 [`chart_element`] 与列映射口径）。
    fn chart_panel(
        &self,
        result: &ResultSet,
        config: &ChartConfig,
        column: &str,
        height: f32,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let points = chart_series(result, &config.x, column);
        let panel = v_flex()
            .flex_1()
            .min_w_0()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .rounded(tokens.radius.md)
            .bg(tokens.colors.surface)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .truncate()
                    .child(SharedString::from(format!("{column} · {}", config.x))),
            );
        if points.is_empty() {
            return panel
                .child(meta_text(tr(cx, "这一列没有可画的数值。"), cx))
                .into_any_element();
        }
        let Some(chart) = chart_element(config.kind, points, column, extended.colors.accent_text)
        else {
            return panel
                .child(meta_text(tr(cx, "饼图只吃正值，这一列全是 0 或负数。"), cx))
                .into_any_element();
        };
        panel
            .child(div().w_full().h(px(height)).child(chart))
            .into_any_element()
    }

    /// 结果网格（含表浏览分页条与截断提示）。
    /// 结果区头条（dbx 同款）：当前结果属于哪个对象 + 元信息 + DDL 入口。
    ///
    /// 表浏览 = 正在看的表；SQL 结果 = 可编辑时的目标表，否则就是「SQL 结果」。
    /// DDL 入口只在表浏览且当前表取到了 DDL 时出现（与右栏 DDL 卡的「放进编辑器」
    /// 同一条 `stage_sql` 路径——右栏可能被折叠，头条是常驻入口）。
    fn render_result_header(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let object: SharedString = match self.mode {
            Mode::Table => self
                .selected_table
                .as_ref()
                .map(|table| match table.schema.as_deref() {
                    Some(schema) => SharedString::from(format!("{schema}.{}", table.name)),
                    None => SharedString::from(table.name.clone()),
                })
                .unwrap_or_else(|| tr(cx, "表浏览").into()),
            _ => self
                .query_target
                .as_ref()
                .map(|table| match table.schema.as_deref() {
                    Some(schema) => SharedString::from(format!("{schema}.{}", table.name)),
                    None => SharedString::from(table.name.clone()),
                })
                .unwrap_or_else(|| tr(cx, "SQL 结果").into()),
        };
        let (columns, rows) = self
            .current_result()
            .map(|result| (result.columns.len(), result.rows.len()))
            .unwrap_or((0, 0));
        let meta = trf(
            cx,
            "{} 列 · {} 行",
            &[&columns.to_string(), &rows.to_string()],
        );
        let ddl_ready = self.mode == Mode::Table && self.table_ddl.is_some();

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xxs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                Icon::new(if self.mode == Mode::Table {
                    DuckIcon::Table
                } else {
                    DuckIcon::Database
                })
                .size(extended.icon.sm)
                .text_color(extended.colors.accent_text),
            )
            .child(
                div()
                    .min_w_0()
                    .max_w(px(320.))
                    .text_size(tokens.typography.sm.size)
                    .font_weight(tokens.typography.sm.weight)
                    .truncate()
                    .child(object),
            )
            .child(meta_text(meta, cx))
            .child(div().flex_1().min_w_0())
            .when(ddl_ready, |bar| {
                bar.child(
                    button(
                        "db-result-ddl",
                        tr(cx, "在编辑器中修改 DDL"),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
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
            })
            .into_any_element()
    }

    /// 结果区脚注（dbx 同款）：行数 · 耗时 · 产生结果的 SQL + 导出入口。
    ///
    /// 全部是「这一次结果」的**常驻**事实——工具条中部那份 status 是易逝的操作
    /// 反馈，翻完页 / 切完标签就没了；这里跟结果共存亡。
    fn render_result_footer(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let rows = self
            .current_result()
            .map(|result| result.rows.len())
            .unwrap_or(0);
        let count = trf(cx, "共 {} 行", &[&rows.to_string()]);

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xxs)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(meta_text(count, cx))
            .when(
                self.current_result().is_some_and(|result| result.truncated),
                |bar| bar.child(neutral_tag(tr(cx, "已达上限"), cx)),
            )
            .when_some(self.query_elapsed_ms, |bar, ms| {
                bar.child(meta_text(
                    SharedString::from(trf(cx, "{} ms", &[&ms.to_string()])),
                    cx,
                ))
            })
            .child(
                div().flex_1().min_w_0().truncate().children(
                    self.query_sql
                        .as_deref()
                        .map(|sql| meta_text(SharedString::from(sql.to_owned()), cx)),
                ),
            )
            .child(self.export_dropdown(cx))
            .into_any_element()
    }

    /// 导出结果下拉（五档：TSV / CSV / Markdown / JSON / INSERT）。
    ///
    /// 档位清单与结果区右键的「复制为 …」同源（[`ResultFormat::ALL`]），这里只是
    /// 多一层落盘文件名与保存对话框。「导出 INSERT」只对 SQL 引擎开放：MongoDB /
    /// Elasticsearch 没有带引号的表名，导不出能直接执行的语句。
    fn export_dropdown(&self, cx: &App) -> AnyElement {
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
        toolbar_dropdown_button(
            "db-export",
            tr(cx, "导出结果"),
            Icon::new(DuckIcon::Download).size(extended.icon.md),
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
        let plan_bar = self.plan_view.as_ref().map(|_| self.render_plan_bar(cx));
        if self.plan_view.as_ref().is_some_and(|plan| !plan.raw) {
            let mut panel = v_flex().flex_1().min_h_0().min_w_0();
            if let Some(bar) = plan_bar {
                panel = panel.child(bar);
            }
            let nodes = self
                .plan_view
                .as_ref()
                .map(|plan| plan.nodes.clone())
                .unwrap_or_default();
            return panel.child(plan_tree(nodes, cx)).into_any_element();
        }

        // 图表视角：结果区改画图（头条上的「看表格」切回网格）。计划态优先于图表态
        // （前者是「这一次执行的计划」，后者是「这份结果的另一种看法」，同时存在时
        // 刚跑出来的计划更该占正文）。
        if self.chart_view.is_some() {
            return self.render_chart(result, cx);
        }

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
        // 对象头条（dbx 同款）：现在看的是哪张表 / 哪段 SQL 的结果 + DDL 入口。
        panel = panel.child(self.render_result_header(cx));
        // 计划头条（「原始输出」档才走到这里；树档在上面已经返回了）。
        if let Some(bar) = plan_bar {
            panel = panel.child(bar);
        }
        // 编辑条：结果集可编辑（单表 + 主键齐全）时才出来，承载「新增行 / 提交 / 丢弃」。
        if self.edit_source().is_some() {
            panel = panel.child(self.render_edit_bar(cx));
        }
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
        if self.mode == Mode::Table {
            panel = panel.child(self.render_pager(cx));
        }
        // 脚注（dbx 同款）：行数 · 耗时 · 产生结果的 SQL + 导出（常驻，跟结果共存亡）。
        panel = panel.child(self.render_result_footer(cx));
        // 结果区整体右键：一次拿走全部行 —— 文本档按 [`ResultFormat::ALL`] 给满
        //（原始字节不进剪贴板：那是文件载荷）。
        //
        // 这里刻意**不**预先把文本拼出来（以前为了「复制全部」在渲染路径上每次都拼一份
        // TSV，一万行的结果集等于每帧白白分配几百 KB）：点哪一档才在那一刻按当前结果集
        // 渲染。菜单项是 `'static` 闭包，所以只把 `Format` 这个小 `Copy` 值带进去。
        let can_insert = self.insert_context().is_some();
        // 同导出菜单：文案按当前语言在外面取好，闭包里只搬「档位 + 文案」对。
        let copy_items: Vec<(ResultFormat, &'static str)> = ResultFormat::ALL
            .iter()
            .map(|format| (*format, format.copy_label(cx)))
            .collect();
        panel
            .id("db-result-panel")
            .context_menu({
                let view = result_view.clone();
                move |menu, _, _| {
                    copy_items.iter().fold(menu, |menu, (format, label)| {
                        let enabled = format.exportable(true, can_insert, false);
                        menu.item(PopupMenuItem::new(*label).disabled(!enabled).on_click({
                            let view = view.clone();
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

    /// 表浏览的过滤条：攒一条草稿（列 + 操作符 + 值），点「添加条件」才生效。
    ///
    /// 生效中的条件以标签形式列在右侧，每条一个 ✕；条件一变就重取第一页
    /// （见 [`Self::after_filter_changed`]）。
    ///
    /// **值的类型合法性不在这里拦**：驱动把值走绑定参数发给引擎，服务端的报错原样
    /// 透出——在界面里猜类型（"这列看着像数字"）只会猜错，还把真正的错误信息换成了
    /// 一句含糊的本地提示。
    fn render_filter_bar(&self, cx: &Context<Self>) -> AnyElement {
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
    fn render_plan_bar(&self, cx: &Context<Self>) -> AnyElement {
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

    /// 表浏览分页条。
    fn render_pager(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        // 与取数、翻页同一口径（设置页改过就跟着变）。
        let page_size = browse_page_size(cx);
        let can_next = self
            .table_rows
            .as_ref()
            .is_some_and(|rows| rows.row_count() >= page_size);
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
                        SharedString::from(if self.table_filters.is_empty() {
                            trf(
                                cx,
                                "第 {} 页 · 每页 {} 行",
                                &[&(self.table_page + 1).to_string(), &page_size.to_string()],
                            )
                        } else {
                            // 过滤生效时把条件数摆出来：翻页的人得知道「下一页」是在
                            // 哪个集合里翻，否则会以为行数对不上是界面出错。
                            trf(
                                cx,
                                "第 {} 页 · 每页 {} 行 · 已应用 {} 个条件",
                                &[
                                    &(self.table_page + 1).to_string(),
                                    &page_size.to_string(),
                                    &self.table_filters.len().to_string(),
                                ],
                            )
                        }),
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

/// 过滤操作符在下拉里的**稳定标识**（ASCII，只做下拉的 value，不进 i18n）。
///
/// 用名字而不是「在 [`duck_db::FilterOp::ALL`] 里的下标」：下标会随着清单增删而漂移，
/// 名字不会。
fn filter_op_key(op: FilterOp) -> &'static str {
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
fn filter_op_from_key(key: &str) -> FilterOp {
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
fn filter_op_label(op: FilterOp, cx: &App) -> &'static str {
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
fn filter_chip_text(filter: &TableFilter, cx: &App) -> SharedString {
    let op = filter_op_label(filter.op, cx);
    match filter.value.as_deref() {
        Some(value) => SharedString::from(format!("{} {op} {value}", filter.column)),
        None => SharedString::from(format!("{} {op}", filter.column)),
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
        // 存储过程 / 函数 / 序列：这三类对象**不能当表浏览**（见 [`TableKind::is_relation`]），
        // 树里只给出图标与名字，点开走 «生成引用骨架» 那条路。
        TableKind::Procedure => DuckIcon::SquareTerminal,
        TableKind::Function => DuckIcon::SquareFunction,
        TableKind::Sequence => DuckIcon::ListOrdered,
    }
}

/// 目录对象种类的中文名（左栏 tooltip / 补全详情共用）。
fn table_kind_label(kind: TableKind, cx: &App) -> String {
    match kind {
        TableKind::Table => tr(cx, "表"),
        TableKind::View => tr(cx, "视图"),
        TableKind::Procedure => tr(cx, "存储过程"),
        TableKind::Function => tr(cx, "函数"),
        TableKind::Sequence => tr(cx, "序列"),
    }
    .to_string()
}

/// 存储过程 / 函数 / 序列点开时放进编辑器的**引用骨架**。
///
/// 这几类对象不能像表那样翻页读，但也不能「点了没反应」——它们各自有唯一一种
/// 在 SQL 里被引用的写法，直接给出来最省事（名字已按引擎的限定名规则拼好）。
fn object_snippet(kind: TableKind, engine: DatabaseKind, name: &str) -> String {
    match kind {
        TableKind::Table | TableKind::View => format!("SELECT * FROM {name} LIMIT 100;"),
        // 存储过程一律按过程调用（`CALL`）；函数按表函数（`SELECT * FROM fn()`），
        // SQL Server 的标量函数要 `SELECT dbo.fn()` 才跑得起来，这里按最通用的写法给。
        TableKind::Procedure => format!("CALL {name}();"),
        TableKind::Function => match engine {
            DatabaseKind::Mysql => format!("SELECT {name}();"),
            _ => format!("SELECT * FROM {name}();"),
        },
        // 序列只有 PostgreSQL 有（`nextval` 是它的取号函数）。
        TableKind::Sequence => format!("SELECT nextval('{name}');"),
    }
}

/// 引擎有没有 SQL 的 DDL 这一层（建表 / 改表 / 删表）。
///
/// 判据只有一条：引擎有没有 SQL 标识符概念（[`DatabaseKind::identifier_quote`]）。
/// MongoDB 走 JSON 命令、Elasticsearch 走 REST 映射，都没有「带引号的表名」，
/// 结构管理对它们**不给入口**（菜单项禁用），而不是给了再报错：
/// 「不支持」与「执行失败」在界面上必须分得开。
fn supports_ddl(engine: DatabaseKind) -> bool {
    engine.identifier_quote().is_some()
}

/// 该引擎的列类型占位（`(主键列, 普通列)`）；`None` = 没有 SQL 类型系统。
fn column_type_placeholders(engine: DatabaseKind) -> Option<(&'static str, &'static str)> {
    Some(match engine {
        DatabaseKind::Sqlite => ("INTEGER", "TEXT"),
        DatabaseKind::Postgres => ("serial", "text"),
        DatabaseKind::Mysql => ("BIGINT", "VARCHAR(255)"),
        DatabaseKind::ClickHouse => ("Int64", "String"),
        DatabaseKind::SqlServer => ("BIGINT", "NVARCHAR(255)"),
        DatabaseKind::Mongo | DatabaseKind::Elasticsearch => return None,
    })
}

/// 建表语句骨架（`qualified` 已按引擎规则拼好限定名）。
///
/// 结构管理走「生成 DDL → 填进编辑器 → 用户确认后执行」这一条统一路径，**不做**
/// 可视化建表向导：列类型 / 约束在真实项目里千变万化，表单化编辑器只会把「能表达
/// 的形状」做成上限（比要建的表更简单都做不到）；而在编辑器里改一行比在表单里
/// 找那个字段快得多。界面负责给一个**语法正确**的起点。
fn create_table_template(engine: DatabaseKind, qualified: &str) -> Option<String> {
    let (id_type, name_type) = column_type_placeholders(engine)?;
    Some(format!(
        "CREATE TABLE {qualified} (\n    id {id_type} NOT NULL,\n    name {name_type},\n    PRIMARY KEY (id)\n);"
    ))
}

/// 改表语句骨架：给一条能直接跑的 `ADD COLUMN` 起点。
///
/// 不生成「完整的」`ALTER TABLE`：改结构有类型 / 默认值 / 约束 / 重整表几十种分支，
/// 界面猜一种而已。一条**语法正确**的骨架 + 编辑器，比一条猜错的完整语句好。
fn alter_table_template(engine: DatabaseKind, qualified: &str) -> Option<String> {
    let (_, name_type) = column_type_placeholders(engine)?;
    Some(format!(
        "ALTER TABLE {qualified} ADD COLUMN new_column {name_type};"
    ))
}

/// 删表语句骨架。**破坏性**：只生成语句，跑不跑由用户在编辑器里点「执行」决定。
fn drop_table_template(engine: DatabaseKind, qualified: &str) -> Option<String> {
    // 判据与建表同源（`column_type_placeholders`），两者一致由单测钉住 ——
    // 「引号有没有」与「类型表有没有」这两个集合将来不许分叉。
    let _ = column_type_placeholders(engine)?;
    Some(format!("DROP TABLE IF EXISTS {qualified};"))
}

/// 对象树筛选：`needle` 由调用点**先小写化**，名字按子串匹配、忽略大小写。
///
/// 空 `needle` 一律通过 —— 「没填筛选词」不等于「筛掉全部」。
fn object_matches(name: &str, needle: &str) -> bool {
    needle.is_empty() || name.to_lowercase().contains(needle)
}

impl DatabasePage {
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
    fn sidebar_toggle_row(
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
    /// 主区正停在这一档时给选中态（否则用户点完不知道自己在看哪一页）。
    fn sidebar_instance_row(&self, conn_id: &str, tab: InstanceTab, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let selected = self.mode == Mode::Instance(tab) && self.active.as_deref() == Some(conn_id);
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
                this.open_instance_tab(tab, window, cx);
            });
        })
        .into_any_element()
    }

    /// 左栏「已保存查询」入口 + 它的子级（逐条列出，点一下载入编辑器）。
    ///
    /// 与「数据库」同层、同在连接节点下：它是**按连接浏览时的取用入口**，不是全局
    /// 设置项（设置页那边放的是「保留多少条历史」这类偏好）。
    fn sidebar_saved_queries_section(&self, conn_id: &str, query: &str, cx: &App) -> AnyElement {
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
    fn sidebar_dashboards_section(&self, conn_id: &str, query: &str, cx: &App) -> AnyElement {
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
        let mut section = v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .child(self.sidebar_toggle_row(
                format!("db-section-boards-{conn_id}"),
                key,
                DuckIcon::Columns2,
                tr(cx, "仪表板"),
                Some(hits.len()),
                matches!(self.mode, Mode::Dashboard(_)),
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
                    self.mode == Mode::Dashboard(id),
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let Some(entity) = click_view.upgrade() else {
                        return;
                    };
                    // 先激活**这条**连接：点的是连接节点下的一块板，意思就是「用这条连接看」，
                    // 不能拿另一条活跃连接的会话去跑面板里的 SQL（与库节点行同口径）。
                    entity.update(cx, |this, cx| {
                        this.activate_conn(&click_conn, window, cx);
                        this.open_dashboard(id, cx);
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
                                this.activate_conn(&open_conn, window, cx);
                                this.open_dashboard(id, cx);
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
    fn sidebar_saved_charts_section(&self, conn_id: &str, query: &str, cx: &App) -> AnyElement {
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
    fn render_connection_tree(
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
            .child(self.sidebar_toggle_row(
                format!("db-section-databases-{id}"),
                db_key,
                DuckIcon::Database,
                tr(cx, "数据库"),
                Some(namespace_count),
                !matches!(self.mode, Mode::Instance(_)),
                db_open,
                cx,
            ));
        if db_open {
            column = column.child(namespace_tree);
        }
        // 实例三档：只有读服务器数据的引擎才画（SQLite 也支持——它的「服务器」就是文件）。
        if session.kind.supports_instance_insight() {
            for tab in InstanceTab::ALL
                .into_iter()
                .filter(|tab| tab.fetches_server())
            {
                column = column.child(self.sidebar_instance_row(id, tab, cx));
            }
        }
        column.into_any_element()
    }

    fn render_tree_category(
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
    fn sidebar_object_row(&self, conn_id: &str, entry: CatalogTable, cx: &App) -> AnyElement {
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
                    let force_new = event.modifiers().alt();
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
    fn sidebar_database_object_row(
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
            move |menu, _, _| {
                let mut menu = menu;
                if let Some(reference) = jump.clone() {
                    let jump_view = view.clone();
                    let jump_conn = conn_id.to_owned();
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
    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
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
        // 只要引擎有命名空间这一层就**总是**给这个控件（构造与编辑器工具条共用
        // [`Self::namespace_picker`]）。
        //
        // 曾经写成「`schemas.len() > 1` 才给」，理由是「只能选一项的下拉是噪音」。
        // 代价出现在用户的实际路径上：不填库名连上 MySQL / PostgreSQL 之后，
        // 左栏对象树是空的（还没有任何命名空间被选中），而这个**唯一能选出库的入口
        // 也一起消失了** —— 界面表现就是「连接成功了，但什么都看不到」。
        // 单库时它至少充当「你现在在哪个库里」的显式读数，不是噪音。
        let schema_picker = self.namespace_picker("db-schema-picker", cx);

        // ── 结构管理入口：新建表 ──
        //
        // 只对**有 SQL DDL 的引擎**给按钮（MongoDB / Elasticsearch 没有这一层，
        // 不给入口而不是给了再报错）。生成语句的表名落点跟随当前命名空间，
        // 与用户在树里看到的那一层保持一致。
        let new_table_button =
            self.active_config()
                .map(|cfg| cfg.kind)
                .filter(|engine| supports_ddl(*engine))
                .and_then(|engine| {
                    let quote = engine.identifier_quote()?;
                    let namespace = self
                        .selected_table
                        .as_ref()
                        .and_then(|table| table.schema.clone())
                        .or_else(|| session.schema.clone())
                        .or_else(|| session.database.clone());
                    let target = match namespace {
                        Some(schema) => TableRef::in_schema(schema, "new_table"),
                        None => TableRef::bare("new_table"),
                    };
                    let qualified = quote_qualified(&target, quote).ok()?;
                    let template = create_table_template(engine, &qualified)?;
                    let entry = view.clone();
                    Some(
                        button("db-new-table", tr(cx, "新建表"), ButtonVariant::Ghost, cx)
                            .on_click(move |_, window, cx| {
                                let Some(entity) = entry.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.stage_sql(
                                        template.clone(),
                                        tr(cx, "已生成建表骨架，改好表名与列后执行。").into(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
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
                            .child(description_list({
                                let mut rows = vec![
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
                        // 「在编辑器中修改」= 把当前定义送进编辑器当起点（改完由用户
                        // 自己按执行）。视图 / 无 DDL 的引擎不会有这张卡，无需额外判据。
                        let stage_ddl = ddl.clone();
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
                                            tr(cx, "在编辑器中修改"),
                                            ButtonVariant::Ghost,
                                            cx,
                                        )
                                        .on_click(
                                            move |_, window, cx| {
                                                let Some(entity) = view.upgrade() else {
                                                    return;
                                                };
                                                entity.update(cx, |this, cx| {
                                                    this.stage_sql(
                                                        stage_ddl.clone(),
                                                        tr(
                                                            cx,
                                                            "已把当前 DDL 放进编辑器，改好后执行。",
                                                        )
                                                        .into(),
                                                        window,
                                                        cx,
                                                    );
                                                });
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

    /// 右栏「表级元信息」：列 + 行数 + 索引 + 外键 + 触发器。
    ///
    /// 数据来自 [`Self::load_table_page`]（只在换表 / 刷新时取一次），这里纯展示，
    /// 渲染路径上没有数据库调用。清单为空的卡照画（显示空态文案）——「这表没有」
    /// 与「引擎没这个概念」（MongoDB / Elasticsearch 的外键与触发器恒空）在界面上
    /// 同形是**有意的**：不为引擎做 UI 特判。
    fn render_table_meta(&self, cx: &App) -> AnyElement {
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
            .on_action(cx.listener(|this, _: &UndoPendingEdits, _window, cx| {
                this.undo_pending(cx);
            }))
            .on_action(cx.listener(|this, _: &RedoPendingEdits, _window, cx| {
                this.redo_pending(cx);
            }))
            .on_action(cx.listener(|this, _: &CopySelectedRow, _window, cx| {
                this.copy_selected_row(cx);
            }))
            .on_action(cx.listener(|this, _: &StopQuery, window, cx| {
                this.stop_running(window, cx);
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
            // 错误徽标：有**还没看过**的失败就挂在这里。它是状态栏里唯一一个「有事发生」
            // 的信号 —— 通知会自己消失，而这个不会，直到用户真的去看了一眼流水。
            //
            // 点它带着**最近那次的关联 id** 跳审计页：那是用户唯一能对上号的东西
            //（提示语里带的是同一个 id）。
            .when_some(crate::audit::pending_failure(cx), |bar, pending| {
                let trace = pending.trace().map(ToOwned::to_owned);
                let tip = SharedString::from(trf(cx, "最近一次：{}", &[pending.summary()]));
                bar.child(
                    icon_text_button(
                        "db-status-audit-errors",
                        SharedString::from(trn(cx, "{} 条待查看错误", pending.count() as i64)),
                        Icon::new(DuckIcon::CircleAlert)
                            .size(extended.icon.sm)
                            .text_color(extended.colors.status_failed),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        // 先落盘再跳：审计的写是去抖的，不 flush 的话刚记的那条还没进库，
                        // 用户点进来看到的会是一张缺了「刚刚那次」的列表。
                        duck_store::flush(cx);
                        crate::audit::clear_failure(cx);
                        crate::audit::open_audit(cx, trace.clone());
                        cx.notify();
                        window.refresh();
                    })),
                )
            })
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

/// 实例页的一张卡：小标题 + 卡片正文（与右栏「索引 / 外键 / 触发器」卡同形）。
///
/// `title` 收 `Into<SharedString>`：仪表盘与概览传中文（进 EN 表），指标 / 检查器传
/// `trf` 拼出来的计数文案（不带方括号的静态串 → 用 `&'static str` 反而要编常量）。
fn instance_card(title: impl Into<SharedString>, body: impl IntoElement, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    v_flex()
        .w_full()
        .gap(tokens.spacing.xs)
        .child(caption_heading(title, cx))
        .child(
            card(cx)
                .w_full()
                .px(tokens.spacing.lg)
                .py(tokens.spacing.sm)
                .child(body),
        )
        .into_any_element()
}

/// 实例页的一句提示（空态 / 取数中；比整块 `empty_state` 轻）。
fn instance_hint(message: &'static str, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    card(cx)
        .w_full()
        .px(tokens.spacing.lg)
        .py(tokens.spacing.sm)
        .child(meta_text(message, cx))
        .into_any_element()
}

/// 把一段 SQL 压成**单行**并截断（仪表盘「最近查询」的行标签用）。
///
/// 不直接把多行 SQL 塞进导航行：行高是固定的，多行文本会被裁成半截，看起来像乱码。
fn one_line(sql: &str) -> String {
    let mut out = String::new();
    for word in sql.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= 60 {
            out.push('…');
            break;
        }
    }
    out
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

            // 代理：协议两档按钮 + 主机 / 端口 / 认证。口令与 SSH 同一条纪律（进凭据文件）。
            let proxy_toggle = button(
                "db-proxy-toggle",
                if self.proxy_enabled {
                    tr(cx, "经代理")
                } else {
                    tr(cx, "不经代理")
                },
                if self.proxy_enabled {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.proxy_enabled = !this.proxy_enabled;
                cx.notify();
            }));
            let proxy_kind_switch = h_flex()
                .flex_wrap()
                .items_center()
                .gap(tokens.spacing.xxs)
                .children(ProxyKind::ALL.map(|kind| {
                    let selected = self.proxy_kind == kind;
                    let label = match kind {
                        ProxyKind::Socks5 => tr(cx, "SOCKS5"),
                        ProxyKind::HttpConnect => tr(cx, "HTTP CONNECT"),
                    };
                    button(
                        SharedString::from(format!("db-proxy-kind-{}", kind.key())),
                        SharedString::from(label),
                        if selected {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Secondary
                        },
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.proxy_kind = kind;
                        cx.notify();
                    }))
                }));
            let mut proxy_block = v_flex().w_full().gap(form_gap(cx)).child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "代理"), cx))
                    .child(proxy_toggle)
                    .child(proxy_kind_switch),
            );
            if self.proxy_enabled {
                proxy_block = proxy_block
                    .child(field_row(cx, tr(cx, "代理主机"), &self.proxy_host))
                    .child(field_row(cx, tr(cx, "代理端口"), &self.proxy_port))
                    .child(field_row(cx, tr(cx, "代理用户"), &self.proxy_user))
                    .child(field_row(cx, tr(cx, "代理口令"), &self.proxy_password))
                    .child(field_hint(
                        tr(cx, "只有需要认证的代理才要填用户名与口令。"),
                        cx,
                    ));
            }

            // AWS SSM：命令全部交给用户本机已装的 aws CLI，本项目不存任何 AWS 凭据。
            let ssm_toggle = button(
                "db-ssm-toggle",
                if self.ssm_enabled {
                    tr(cx, "经 AWS SSM")
                } else {
                    tr(cx, "不用 SSM")
                },
                if self.ssm_enabled {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.ssm_enabled = !this.ssm_enabled;
                cx.notify();
            }));
            let mut ssm_block = v_flex().w_full().gap(form_gap(cx)).child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "AWS SSM"), cx))
                    .child(ssm_toggle),
            );
            if self.ssm_enabled {
                ssm_block = ssm_block
                    .child(field_row(cx, tr(cx, "目标实例"), &self.ssm_instance))
                    .child(field_row(cx, tr(cx, "远端主机"), &self.ssm_remote_host))
                    .child(field_row(cx, tr(cx, "区域"), &self.ssm_region))
                    .child(field_row(cx, tr(cx, "AWS profile"), &self.ssm_profile))
                    .child(field_hint(
                        tr(
                            cx,
                            "需要本机已装 aws CLI 并完成过 aws sso login / configure。",
                        ),
                        cx,
                    ));
            }

            // 传输档案：一份可被多条连接引用的三档配置。选中之后，下面三档**就是这份
            // 档案的内容**（而不是连接自己的）——保存在 `save()` 里原样写回档案。
            let mut profile_options = vec![(
                SharedString::default(),
                SharedString::from(tr(cx, "自定义（用下面的配置）")),
            )];
            profile_options.extend(self.tunnel_profiles.iter().map(|profile| {
                let label = match profile.kind() {
                    Some(kind) => format!("{} · {}", profile.name, transport_kind_label(kind, cx)),
                    None => profile.name.clone(),
                };
                (
                    SharedString::from(profile.id.clone()),
                    SharedString::from(label),
                )
            }));
            let profile_entity = cx.entity().downgrade();
            let profile_select = dropdown_select(
                "db-tunnel-profile",
                profile_options,
                SharedString::from(self.tunnel_profile.clone().unwrap_or_default()),
                move |value: SharedString, window, cx| {
                    let id = value.to_string();
                    let _ = profile_entity.update(cx, |this, cx| {
                        this.select_tunnel_profile((!id.is_empty()).then_some(id), window, cx);
                    });
                },
                cx,
            );
            let mut profile_block = v_flex().w_full().gap(form_gap(cx)).child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "传输档案"), cx))
                    .child(div().flex_1().min_w_0().child(profile_select)),
            );
            match self.tunnel_profile.is_some() {
                true => {
                    profile_block = profile_block
                        .child(field_row(cx, tr(cx, "档案名称"), &self.profile_name))
                        .child(field_hint(
                            tr(
                                cx,
                                "下面的三档就是这份档案的内容；保存连接时会一并存回档案。",
                            ),
                            cx,
                        ))
                        .child(
                            h_flex().w_full().gap(tokens.spacing.xs).child(
                                button(
                                    "db-tunnel-profile-delete",
                                    tr(cx, "删除这份档案"),
                                    ButtonVariant::Secondary,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.remove_tunnel_profile(window, cx),
                                )),
                            ),
                        );
                }
                false => {
                    // 「自定义」态才给「存为新档案」：选中档案时再存一份，等于把档案复制一遍，
                    // 想要那份语义请先切到自定义。少一个按钮，少一种误解。
                    profile_block = profile_block
                        .child(field_row(cx, tr(cx, "档案名称"), &self.profile_name))
                        .child(
                            h_flex().w_full().gap(tokens.spacing.xs).child(
                                button(
                                    "db-tunnel-profile-add",
                                    tr(cx, "存为新档案"),
                                    ButtonVariant::Secondary,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.add_tunnel_profile(window, cx),
                                )),
                            ),
                        );
                }
            }

            // 三档是**互斥**的：同时开着只说优先级，不说「叠加」——界面上说清楚，
            // 免得用户以为「SSH 出去再走代理」会生效。
            let active_count = usize::from(self.ssh_enabled)
                + usize::from(self.proxy_enabled)
                + usize::from(self.ssm_enabled);
            let overlap_hint = (active_count > 1).then(|| {
                field_hint(
                    tr(
                        cx,
                        "同时启用多档时，只有优先级最高的一档生效：SSM → SSH → 代理。",
                    ),
                    cx,
                )
            });

            v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(db_section_header(DuckIcon::Globe, "TRANSPORT", cx))
                .child(profile_block)
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.lg)
                        .child(field_label(tr(cx, "SSL 模式"), cx))
                        .child(tls_switch),
                )
                .child(ssh_block)
                .child(proxy_block)
                .child(ssm_block)
                .children(overlap_hint)
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
                            .children(transport_section)
                            .child(self.hooks_section(cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_db::Value;
    use gpui::TestAppContext;

    /// 「同对象复用」判定：同连接同对象才复用；跨连接、跨对象、身份缺失都不算。
    ///
    /// 这是 [`find_reusable_tab`] 的纯函数验收——复用策略写歪的最常见形态是
    /// 「忘了比对连接」（两个连接的同名表互相抢标签）。
    #[test]
    fn data_tab_reuse_requires_same_connection_and_object() {
        let tabs = vec!["t1".to_owned(), "t2".to_owned(), "t3".to_owned()];
        let kinds: HashMap<String, TabKind> = [
            ("t1".to_owned(), TabKind::Query),
            (
                "t2".to_owned(),
                TabKind::Data {
                    object: TableRef::in_schema("wordpress", "ah_posts"),
                },
            ),
            (
                "t3".to_owned(),
                TabKind::Data {
                    object: TableRef::bare("ah_posts"),
                },
            ),
        ]
        .into_iter()
        .collect();
        let owners: HashMap<String, String> = [
            ("t1".to_owned(), "c1".to_owned()),
            ("t2".to_owned(), "c1".to_owned()),
            ("t3".to_owned(), "c2".to_owned()),
        ]
        .into_iter()
        .collect();

        // 同连接 + 同库同表 → 复用 t2。
        assert_eq!(
            find_reusable_tab(
                &tabs,
                &kinds,
                &owners,
                "c1",
                &TabKind::Data {
                    object: TableRef::in_schema("wordpress", "ah_posts"),
                },
            )
            .as_deref(),
            Some("t2"),
        );
        // 同表名但不同 schema → 不复用（裸名与限定名是两个对象）。
        assert_eq!(
            find_reusable_tab(
                &tabs,
                &kinds,
                &owners,
                "c1",
                &TabKind::Data {
                    object: TableRef::bare("ah_posts"),
                },
            ),
            None,
        );
        // 同对象但归属另一条连接 → 不复用。
        assert_eq!(
            find_reusable_tab(
                &tabs,
                &kinds,
                &owners,
                "c2",
                &TabKind::Data {
                    object: TableRef::in_schema("wordpress", "ah_posts"),
                },
            ),
            None,
        );
        // 查询标签不参与对象复用。
        assert_eq!(
            find_reusable_tab(&tabs, &kinds, &owners, "c1", &TabKind::Query).as_deref(),
            Some("t1"),
        );
    }

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

    /// 本页的页面级键位必须**真的**绑在 `"Database"` 上下文上（B10 的接线验收）。
    ///
    /// 盯的是最容易各写各的三处：动作由 `actions!` 声明、上下文常量 `KEY_CONTEXT` 在
    /// 同一模块、绑定由 [`bind_keys`] 做，而 `app::install` 只负责调一次。任何一处漂了，
    /// 界面照样编译、键位却悄悄失效 —— 只有把键位表**读回来**才能发现，所以这里不测行为、
    /// 测绑定本身。
    ///
    /// 不断言「本页键位不与别处相撞」：那是 `duck_command::bind_all` 的职责（会打
    /// `keybinding conflict` 警告）；这几条选键的依据是 `gpui_base::Root` 的默认表里
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
            let undo = UndoPendingEdits;
            let redo = RedoPendingEdits;
            let copy_row = CopySelectedRow;
            let stop = StopQuery;
            let bindings = cx.key_bindings();
            let keymap = bindings.borrow();

            let expected: [(&dyn gpui::Action, &str, bool); 7] = [
                (&run, "enter", false),
                (&commit, "s", false),
                (&discard, "backspace", true),
                (&undo, "z", false),
                (&redo, "z", true),
                (&copy_row, "c", false),
                (&stop, ".", false),
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
    ///
    /// 单格转义本身在内核（`duck-db`）测；这里盯的是**接线**：整表那条路必须走
    /// [`ResultSet::to_tsv`]、「复制整行」那条路必须走同一个 [`tsv_field`]，
    /// 两条路的口径不能各走各的。
    #[test]
    fn tsv_copy_escapes_separators_newlines_and_quotes() {
        assert_eq!(tsv_field("plain"), "plain");
        assert_eq!(tsv_field("a\tb"), "\"a\tb\"");
        assert_eq!(tsv_field("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(tsv_field("say \"hi\""), "\"say \"\"hi\"\"\"");

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
        // 表头 + 每行一条，行式文本以换行收尾（与内核 `to_csv` 同口径）；
        // NULL 走展示文本 `NULL`（与网格里看到的一致）。
        assert_eq!(result.to_tsv(), "id\tnote\n1\tNULL\n2\t\"a\tb\"\n");
        // 整行拆分出来的每一段，和整表渲染出来的对应段逐字相同。
        assert_eq!(row_as_tsv(&result.rows[0]), "1\tNULL");
        assert_eq!(row_as_tsv(&result.rows[1]), "2\t\"a\tb\"");
        for (index, row) in result.rows.iter().enumerate() {
            let line = result
                .to_tsv()
                .lines()
                .nth(index + 1)
                .unwrap_or("")
                .to_owned();
            assert_eq!(line, row_as_tsv(row), "第 {index} 行两条路必须同源");
        }
    }

    /// `ResultFormat` 的正文渲染：每一档都要出得来，且 `InsertSql` 缺表引用时给 `None`
    /// 而不是硬拼一条坏 SQL；`Binary`（原始字节）**永远不给文本**，它的载荷走
    /// [`ResultFormat::render_bytes`]。
    #[test]
    fn every_result_format_renders_or_declines() {
        let result = ResultSet {
            columns: vec![Column {
                name: "id".to_owned(),
                type_name: "INTEGER".to_owned(),
            }],
            rows: vec![vec![Value::Int(1)]],
            truncated: false,
        };

        assert_eq!(
            ResultFormat::Tsv.render(&result, None),
            Some("id\n1\n".to_owned())
        );
        assert_eq!(
            ResultFormat::Csv.render(&result, None),
            Some("id\n1\n".to_owned())
        );
        assert_eq!(
            ResultFormat::Text.render(&result, None),
            Some("id\n1\n".to_owned())
        );
        assert_eq!(
            ResultFormat::Markdown.render(&result, None),
            Some("| id |\n| --- |\n| 1 |\n".to_owned())
        );
        assert_eq!(
            ResultFormat::Json.render(&result, None),
            Some("[\n  {\"id\": 1}\n]\n".to_owned())
        );
        assert_eq!(
            ResultFormat::JsonPretty.render(&result, None),
            Some("[\n  {\n    \"id\": 1\n  }\n]\n".to_owned())
        );
        assert_eq!(
            ResultFormat::Hex.render(&result, None),
            Some("1\n".to_owned())
        );
        assert_eq!(
            ResultFormat::Base64.render(&result, None),
            Some("1\n".to_owned())
        );
        // 没有表引用 + 引号（MongoDB / Elasticsearch）：不渲染。
        assert_eq!(ResultFormat::InsertSql.render(&result, None), None);
        // 给了就渲染成可执行草稿。
        assert_eq!(
            ResultFormat::InsertSql.render(&result, Some(("t", '"'))),
            Some("INSERT INTO t (\"id\") VALUES (1);\n".to_owned())
        );
        // 原始字节档不是文本档：文本渲染一律 `None`，单列整数结果也给不出字节。
        assert_eq!(ResultFormat::Binary.render(&result, None), None);
        assert_eq!(ResultFormat::Binary.render_bytes(&result), None);

        // 每档的落盘文件名各不相同（同一次导出不会互相覆盖）。
        let names = ResultFormat::EXPORT_ALL.map(ResultFormat::file_name);
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "文件名必须两两不同");
        // 复制菜单是导出菜单的子集（原始字节不进剪贴板），且顺序一致。
        let export: Vec<_> = ResultFormat::EXPORT_ALL.to_vec();
        let copy: Vec<_> = ResultFormat::ALL.to_vec();
        assert_eq!(copy, export[..copy.len()].to_vec(), "两个清单必须同源同序");
        assert_eq!(
            export.last().copied(),
            Some(ResultFormat::Binary),
            "原始字节排在最末（它是导出专属档）"
        );
    }

    /// 原始字节档的可用性判据与渲染同源：单列二进制 → 可用且给字节；其余 → 不可用。
    #[test]
    fn binary_format_enables_only_for_raw_payloads() {
        let single_binary = ResultSet {
            columns: vec![Column {
                name: "blob".to_owned(),
                type_name: "BLOB".to_owned(),
            }],
            rows: vec![vec![Value::Bytes(vec![1, 2, 3])]],
            truncated: false,
        };
        assert!(ResultFormat::Binary.exportable(true, false, true));
        assert_eq!(
            ResultFormat::Binary.render_bytes(&single_binary),
            Some(vec![1, 2, 3])
        );
        // 引擎给不出引号规则也不影响原始字节档（它不拼 SQL）。
        assert!(ResultFormat::Binary.exportable(true, false, true));
        // 没有结果集 / 不是单列二进制：置灰。
        assert!(!ResultFormat::Binary.exportable(false, true, true));
        assert!(!ResultFormat::Binary.exportable(true, true, false));
        // 文本档只要「有结果」，不受 INSERT 与字节判据影响。
        assert!(ResultFormat::Csv.exportable(true, false, false));
        assert!(!ResultFormat::Csv.exportable(false, true, true));
    }

    /// 结构管理的三条骨架：有 DDL 的引擎给语句，MongoDB / Elasticsearch 一律 `None`
    /// （界面据此**不给入口**，而不是给了再报错）。
    #[test]
    fn ddl_templates_cover_sql_engines_only() {
        for engine in DatabaseKind::ALL {
            let create = create_table_template(engine, "t");
            let alter = alter_table_template(engine, "t");
            let drop = drop_table_template(engine, "t");
            if supports_ddl(engine) {
                let create = create.expect("有 DDL 的引擎必须给建表骨架");
                assert!(
                    create.starts_with("CREATE TABLE t ("),
                    "{engine:?}: {create}"
                );
                // 骨架要能直接跑起来：主键列自带 `NOT NULL`，不留待填的空壳。
                assert!(create.contains("PRIMARY KEY (id)"), "{engine:?}: {create}");
                let alter = alter.expect("有 DDL 的引擎必须给改表骨架");
                assert!(
                    alter.starts_with("ALTER TABLE t ADD COLUMN"),
                    "{engine:?}: {alter}"
                );
                assert_eq!(
                    drop.expect("有 DDL 的引擎必须给删表骨架"),
                    "DROP TABLE IF EXISTS t;"
                );
            } else {
                assert!(
                    create.is_none() && alter.is_none() && drop.is_none(),
                    "{engine:?}"
                );
            }
        }
    }

    /// 「支持 DDL」的两个判据（引号有没有 / 类型占位表有没有）必须同源。
    #[test]
    fn ddl_support_judgements_agree() {
        for engine in DatabaseKind::ALL {
            assert_eq!(
                supports_ddl(engine),
                column_type_placeholders(engine).is_some(),
                "{engine:?}"
            );
        }
    }

    /// 对象树筛选：大小写不敏感的子串匹配；空筛选词**通过全部**（不是筛掉全部）。
    ///
    /// `needle` 的约定是**调用点先小写化**，所以这里只断言「名字侧大小写无关」。
    #[test]
    fn object_filter_matches_case_insensitively() {
        assert!(object_matches("Users", ""));
        assert!(object_matches("Users", "user"));
        assert!(object_matches("USERS", "user"));
        assert!(object_matches("用户表", "用户"));
        assert!(!object_matches("users", "orders"));
    }

    /// 已保存查询的新 id：不与现有条目撞车（删过几条之后 `len()+1` 会撞上旧号）。
    #[test]
    fn saved_query_ids_never_collide() {
        let mk = |id: &str| SavedQuery {
            id: id.to_owned(),
            name: id.to_owned(),
            sql: "SELECT 1".to_owned(),
        };
        assert_eq!(SavedQuery::next_id(&[]), "saved-1");
        assert_eq!(
            SavedQuery::next_id(&[mk("saved-1"), mk("saved-2")]),
            "saved-3"
        );
        // 删掉中间那条后再新建：`len()+1` 会算出 `saved-3`（已存在），必须顺延。
        assert_eq!(
            SavedQuery::next_id(&[mk("saved-1"), mk("saved-3")]),
            "saved-4"
        );
    }

    /// 实例四档：`ALL` 是左栏渲染顺序，且只有仪表盘不取服务器数据。
    #[test]
    fn instance_tabs_split_by_server_fetch() {
        assert_eq!(InstanceTab::ALL.len(), 4);
        assert_eq!(InstanceTab::ALL[0], InstanceTab::Dashboard);
        assert!(!InstanceTab::Dashboard.fetches_server());
        for tab in InstanceTab::ALL
            .into_iter()
            .filter(|tab| tab.fetches_server())
        {
            assert_ne!(tab, InstanceTab::Dashboard);
        }
        // 元素 id 用的英文键两两不同（同一连接下四个入口不能共用一个 id）。
        let keys: std::collections::BTreeSet<_> =
            InstanceTab::ALL.iter().map(|tab| tab.key()).collect();
        assert_eq!(keys.len(), InstanceTab::ALL.len());
    }

    /// 指标时序只保留最近 [`METRIC_SAMPLES`] 次采样（内存有界）。
    #[test]
    fn metric_history_keeps_a_bounded_window() {
        let mut history = MetricHistory::default();
        for at in 0..(METRIC_SAMPLES as i64 + 10) {
            history.push(at, vec![metric("cpu", at as f64)]);
        }
        assert_eq!(history.len(), METRIC_SAMPLES);
        // 丢的是最旧的：留下的第一条是第 10 个采样（at == 10）。
        let series = history.series("cpu");
        assert_eq!(series.first().map(|(at, _)| *at), Some(10));
        assert_eq!(series.len(), METRIC_SAMPLES);
    }

    /// 某一轮没报某指标时，序列**跳过**那一轮而不是补 0（补 0 会画出不存在的「跌到底」）。
    #[test]
    fn metric_history_series_skips_missing_points() {
        let mut history = MetricHistory::default();
        history.push(0, vec![metric("cpu", 1.0), metric("mem", 2.0)]);
        history.push(1, vec![metric("mem", 3.0)]); // 这轮没报 cpu
        history.push(2, vec![metric("cpu", 5.0), metric("mem", 4.0)]);

        let cpu = history.series("cpu");
        assert_eq!(cpu, vec![(0, 1.0), (2, 5.0)], "cpu 序列应跳过第 1 轮");
        let mem = history.series("mem");
        assert_eq!(mem.len(), 3);
        assert!(history.series("nope").is_empty());
    }

    /// 可画的指标名以**最近一轮**为准（指标清单随引擎版本变，不能用并集）。
    #[test]
    fn metric_history_keys_follow_the_latest_sample() {
        let mut history = MetricHistory::default();
        history.push(
            0,
            vec![metric("a", 1.0), metric("b", 1.0), metric("a", 9.0)],
        );
        assert_eq!(history.keys(), vec!["a".to_owned(), "b".to_owned()]);
        history.push(1, vec![metric("b", 1.0), metric("c", 1.0)]);
        assert_eq!(history.keys(), vec!["b".to_owned(), "c".to_owned()]);
    }

    /// 覆盖时长：不足两次采样为 0（界面据此显示「只有一次采样」而不是「覆盖 0s」）。
    #[test]
    fn metric_history_span_needs_two_samples() {
        let mut history = MetricHistory::default();
        assert_eq!(history.span_secs(), 0);
        history.push(100, vec![metric("cpu", 1.0)]);
        assert_eq!(history.span_secs(), 0);
        history.push(160, vec![metric("cpu", 1.0)]);
        assert_eq!(history.span_secs(), 60);
    }

    /// 紧凑时长逐档进位（横轴刻度与「覆盖 {}」共用）。
    #[test]
    fn compact_duration_steps_through_units() {
        assert_eq!(compact_duration(0), "0s");
        assert_eq!(compact_duration(59), "59s");
        assert_eq!(compact_duration(60), "1m");
        assert_eq!(compact_duration(3_599), "59m");
        assert_eq!(compact_duration(3_600), "1h");
        assert_eq!(compact_duration(86_399), "23h");
        assert_eq!(compact_duration(86_400), "1d");
        // 负数（时钟漂移）按 0 处理，不出现「-3s」。
        assert_eq!(compact_duration(-5), "0s");
    }

    /// 紧凑数值：整数原样、小数两位、千/百万/十亿缩写（纵轴刻度用）。
    #[test]
    fn compact_number_abbreviates_large_magnitudes() {
        assert_eq!(compact_number(0.0), "0");
        assert_eq!(compact_number(42.0), "42");
        assert_eq!(compact_number(1.5), "1.50");
        assert_eq!(compact_number(1_500.0), "1.5k");
        assert_eq!(compact_number(2_000_000.0), "2.0M");
        assert_eq!(compact_number(1_000_000_000.0), "1.0G");
        // 绝对值判档，负计数（速率差）同样缩写。
        assert_eq!(compact_number(-1_500.0), "-1.5k");
    }

    /// 造一个指标项。
    fn metric(key: &str, value: f64) -> InstanceMetric {
        InstanceMetric {
            key: key.to_owned(),
            value,
        }
    }

    /// 造一个结果集（列名 + 行）。
    fn result_set(columns: &[&str], rows: Vec<Vec<Value>>) -> ResultSet {
        ResultSet {
            columns: columns
                .iter()
                .map(|name| Column {
                    name: (*name).to_owned(),
                    type_name: String::new(),
                })
                .collect(),
            rows,
            ..ResultSet::default()
        }
    }

    /// 数值列判据看**值**而不是声明类型（SQLite 动态类型 / CH 的 `Nullable(...)` 包装都
    /// 会让类型名不可靠）。
    #[test]
    fn numeric_columns_are_judged_by_value_not_declared_type() {
        let result = result_set(
            &["name", "amount", "note", "flag", "mixed", "empty"],
            vec![
                vec![
                    Value::Text("a".into()),
                    Value::Int(1),
                    Value::Decimal("2.50".into()),
                    Value::Bool(true),
                    Value::Int(5),
                    Value::Null,
                ],
                vec![
                    Value::Text("b".into()),
                    Value::Float(2.5),
                    Value::Decimal("-3".into()),
                    Value::Bool(false),
                    Value::Text("boom".into()),
                    Value::Null,
                ],
            ],
        );
        let numeric = |name: &str| {
            let index = result
                .columns
                .iter()
                .position(|column| column.name == name)
                .expect("列存在");
            column_is_numeric(&result, index)
        };
        assert!(!numeric("name"), "文本列不是数值");
        assert!(numeric("amount"), "整数 / 浮点算数值");
        assert!(numeric("note"), "能 parse 的 Decimal 算数值");
        assert!(!numeric("flag"), "布尔画成数是误导，不算数值");
        assert!(!numeric("mixed"), "只要有一格非数就整列不算");
        assert!(!numeric("empty"), "整列 NULL 不当数值（没东西可画）");
    }

    /// 默认映射：X 取第一个非数值列，Y 取数值列（封顶）。
    #[test]
    fn default_chart_columns_pick_a_label_axis_and_numeric_series() {
        let result = result_set(
            &["created_at", "amount", "qty"],
            vec![vec![
                Value::Text("2026-01-01".into()),
                Value::Int(10),
                Value::Int(2),
            ]],
        );
        let (x, y) = default_chart_columns(&result).expect("有数值列就该给出映射");
        assert_eq!(x, "created_at");
        assert_eq!(y, vec!["amount".to_owned(), "qty".to_owned()]);

        // 全是数值 → 拿第一列当轴，其余当序列。
        let all_numeric = result_set(&["a", "b"], vec![vec![Value::Int(1), Value::Int(2)]]);
        let (x, y) = default_chart_columns(&all_numeric).expect("有数值列");
        assert_eq!(x, "a");
        assert_eq!(y, vec!["b".to_owned()]);

        // 一个数值列都没有 → 不硬编一份空映射。
        let no_numeric = result_set(
            &["a", "b"],
            vec![vec![Value::Text("x".into()), Value::Null]],
        );
        assert!(default_chart_columns(&no_numeric).is_none());

        // 数值列多于封顶时只取前 CHART_SERIES_LIMIT 条。
        let wide: Vec<String> = (0..(CHART_SERIES_LIMIT + 3))
            .map(|i| format!("c{i}"))
            .collect();
        let names: Vec<&str> = wide.iter().map(String::as_str).collect();
        let row: Vec<Value> = (0..wide.len()).map(|i| Value::Int(i as i64)).collect();
        let crowded = result_set(&names, vec![row]);
        let (_, y) = default_chart_columns(&crowded).expect("有数值列");
        assert_eq!(
            y.len(),
            CHART_SERIES_LIMIT,
            "序列数按 CHART_SERIES_LIMIT 封顶"
        );
    }

    /// 序列抽取：分类缺失 / 数值非数的行跳过（不补 0）。
    #[test]
    fn chart_series_skips_rows_that_cannot_be_plotted() {
        let result = result_set(
            &["label", "value"],
            vec![
                vec![Value::Text("a".into()), Value::Int(1)],
                vec![Value::Null, Value::Int(2)],
                vec![Value::Text("c".into()), Value::Text("nope".into())],
                vec![Value::Text("d".into()), Value::Decimal("4.5".into())],
            ],
        );
        let series = chart_series(&result, "label", "value");
        assert_eq!(series, vec![("a".to_owned(), 1.0), ("d".to_owned(), 4.5)]);

        // 列名不在结果里 → 空表（调用方据此提示「配置失效」）。
        assert!(chart_series(&result, "nope", "value").is_empty());
        assert!(chart_series(&result, "label", "nope").is_empty());

        // 非有限的浮点进不了坐标系。
        assert_eq!(chart_number(&Value::Float(f64::NAN)), None);
        assert_eq!(chart_number(&Value::Float(f64::INFINITY)), None);
        assert_eq!(chart_number(&Value::Bool(true)), None);
    }

    /// 配置失效判据：列名对不上就判失效（重跑后列变了不能按老配置画）。
    #[test]
    fn chart_config_validity_follows_column_names() {
        let result = result_set(
            &["label", "value"],
            vec![vec![Value::Text("a".into()), Value::Int(1)]],
        );
        let good = ChartConfig {
            kind: ChartKind::Line,
            x: "label".to_owned(),
            y: vec!["value".to_owned()],
        };
        assert!(good.is_valid_for(&result));

        let gone_axis = ChartConfig {
            x: "renamed".to_owned(),
            ..good.clone()
        };
        assert!(!gone_axis.is_valid_for(&result), "轴列没了即失效");

        let gone_series = ChartConfig {
            y: vec!["value".to_owned(), "dropped".to_owned()],
            ..good.clone()
        };
        assert!(!gone_series.is_valid_for(&result), "任一条序列没了即失效");

        let empty_series = ChartConfig {
            y: Vec::new(),
            ..good
        };
        assert!(!empty_series.is_valid_for(&result), "没有序列就画不出图");
    }

    /// 图表类型：稳定键可往返；饼图只吃一条序列，其余封顶。
    #[test]
    fn chart_kinds_round_trip_and_bound_their_series() {
        for kind in ChartKind::ALL {
            assert_eq!(ChartKind::from_key(kind.key()), Some(kind));
        }
        assert_eq!(ChartKind::from_key("nope"), None);
        assert_eq!(ChartKind::Pie.series_limit(), 1);
        for kind in [ChartKind::Line, ChartKind::Bar, ChartKind::Area] {
            assert_eq!(kind.series_limit(), CHART_SERIES_LIMIT);
        }
        // 键两两不同（同一连接下几张图的元素 id 不能撞）。
        let keys: BTreeSet<&str> = ChartKind::ALL.iter().map(|kind| kind.key()).collect();
        assert_eq!(keys.len(), ChartKind::ALL.len());

        // `effective_y` 按类型截断。
        let config = ChartConfig {
            kind: ChartKind::Pie,
            x: "label".to_owned(),
            y: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        };
        assert_eq!(config.effective_y().to_vec(), vec!["a".to_owned()]);
        let line = ChartConfig {
            kind: ChartKind::Line,
            ..config
        };
        assert_eq!(line.effective_y().len(), 3);
    }

    /// 饼图配色按黄金角旋转色相：条数够多也不撞色，且第一条就是基色。
    #[test]
    fn pie_palette_rotates_hue_from_the_base_color() {
        let base = Hsla {
            h: 0.5,
            s: 0.8,
            l: 0.5,
            a: 1.0,
        };
        let palette = chart_palette(base, 6);
        assert_eq!(palette.len(), 6);
        assert_eq!(palette[0], base, "第一条就是基色");
        for pair in palette.windows(2) {
            assert_ne!(pair[0].h, pair[1].h, "相邻切片色相不同");
        }
        // 色相恒落在 [0, 1)（旋转后不会被 rem_euclid 甩到区间外）。
        for color in &palette {
            assert!((0.0..1.0).contains(&color.h), "色相越界: {}", color.h);
        }
    }

    /// 已保存图表的 id 序列不与现有条目撞车。
    #[test]
    fn saved_chart_ids_never_collide() {
        let mk = |id: &str| SavedChart {
            id: id.to_owned(),
            name: id.to_owned(),
            sql: String::new(),
            kind: ChartKind::Line,
            x: String::new(),
            y: Vec::new(),
        };
        assert_eq!(SavedChart::next_id(&[]), "chart-1");
        assert_eq!(
            SavedChart::next_id(&[mk("chart-1"), mk("chart-2")]),
            "chart-3"
        );
        // 中间被删过（只有 1 与 3）→ 补到 4，不重用已存在过的号。
        assert_eq!(
            SavedChart::next_id(&[mk("chart-1"), mk("chart-3")]),
            "chart-4"
        );
        // 落盘往返：类型 / 列映射都得原样回来。
        let saved = SavedChart {
            id: "chart-9".to_owned(),
            name: "按天下单量".to_owned(),
            sql: "SELECT day, amount FROM orders".to_owned(),
            kind: ChartKind::Bar,
            x: "day".to_owned(),
            y: vec!["amount".to_owned()],
        };
        let json = serde_json::to_string(&saved).expect("可序列化");
        let back: SavedChart = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back.kind, ChartKind::Bar);
        assert_eq!(back.x, "day");
        assert_eq!(back.config().y, ["amount".to_owned()]);
    }

    /// 12 栏流式打包：一行最多 12 栏，装不下就换行；跨度越界收口。
    #[test]
    fn dashboard_rows_pack_into_twelve_columns() {
        // 两个半行 = 一行。
        assert_eq!(pack_dashboard_rows(&[6, 6]), vec![vec![0, 1]]);
        // 半行 + 半行 + 半行 → 第三块换行（6+6 已满 12）。
        assert_eq!(pack_dashboard_rows(&[6, 6, 6]), vec![vec![0, 1], vec![2]]);
        // 整行独占一行，后面的从新行开始（哪怕前一行还剩 1 栏）。
        assert_eq!(
            pack_dashboard_rows(&[5, 12, 7]),
            vec![vec![0], vec![1], vec![2]]
        );
        // 5 + 7 = 12 正好一行；再来的 1 换行。
        assert_eq!(pack_dashboard_rows(&[5, 7, 1]), vec![vec![0, 1], vec![2]]);
        // 越界收口：0 当 1 栏、99 当 12 栏（kv 是可以被外部改的）。
        assert_eq!(pack_dashboard_rows(&[0, 99]), vec![vec![0], vec![1]]);
        assert_eq!(clamp_span(0), 1);
        assert_eq!(clamp_span(99), DASHBOARD_COLUMNS);
        // 空表 → 没有行（调用方据此画空态，而不是画一条空行）。
        assert!(pack_dashboard_rows(&[]).is_empty());
    }

    /// 一栏的宽度占比 = 栏数 / 12。
    #[test]
    fn column_fraction_is_span_over_twelve() {
        assert!((column_fraction(6) - 0.5).abs() < f32::EPSILON);
        assert!((column_fraction(12) - 1.0).abs() < f32::EPSILON);
        // 越界同样收口（否则会算出宽度 > 1 的越界面板）。
        assert!((column_fraction(0) - 1.0 / 12.0).abs() < f32::EPSILON);
        assert!((column_fraction(99) - 1.0).abs() < f32::EPSILON);
    }

    /// 折叠的分隔条只挡它**下面那一段**（直到下一个分隔条），分隔条自己始终可见。
    #[test]
    fn collapsed_divider_hides_only_its_own_section() {
        let board = Dashboard {
            id: 1,
            name: "生产监控".to_owned(),
            panels: vec![
                panel(
                    1,
                    6,
                    DashboardPanelKind::Chart {
                        saved_chart_id: "chart-1".into(),
                    },
                ),
                panel(
                    2,
                    12,
                    DashboardPanelKind::Divider {
                        title: "上半场".into(),
                    },
                ),
                panel(
                    3,
                    6,
                    DashboardPanelKind::Chart {
                        saved_chart_id: "chart-2".into(),
                    },
                ),
                panel(
                    4,
                    12,
                    DashboardPanelKind::Divider {
                        title: "下半场".into(),
                    },
                ),
                panel(
                    5,
                    6,
                    DashboardPanelKind::Inspector {
                        view: InspectorView::Metrics,
                    },
                ),
            ],
        };
        // 都不折：五块全在，顺序不变。
        assert_eq!(
            visible_dashboard_panels(&board.panels, &BTreeSet::new()),
            vec![0, 1, 2, 3, 4]
        );
        // 折第一条：它统领的第 3 块藏起来，第二条分隔条与它后面的照旧。
        let collapsed: BTreeSet<u32> = [2].into_iter().collect();
        assert_eq!(
            visible_dashboard_panels(&board.panels, &collapsed),
            vec![0, 1, 3, 4]
        );
        // 折第二条：只影响第 5 块（它自己的那一段）。
        let collapsed: BTreeSet<u32> = [4].into_iter().collect();
        assert_eq!(
            visible_dashboard_panels(&board.panels, &collapsed),
            vec![0, 1, 2, 3]
        );
        // 两条都折：被折的两块都藏起来，两条分隔条自己还在。
        let collapsed: BTreeSet<u32> = [2, 4].into_iter().collect();
        let visible = visible_dashboard_panels(&board.panels, &collapsed);
        assert!(visible.contains(&1) && visible.contains(&3));
        assert_eq!(visible, vec![0, 1, 3]);
    }

    /// 分隔条一律整行：它的 `span` 存满栏，布局也按满栏算。
    #[test]
    fn divider_panels_always_span_the_full_row() {
        let divider = panel(
            1,
            6,
            DashboardPanelKind::Divider {
                title: "分组".into(),
            },
        );
        assert_eq!(dashboard_panel_span(&divider), DASHBOARD_COLUMNS);
        let chart = panel(
            2,
            6,
            DashboardPanelKind::Chart {
                saved_chart_id: "chart-1".into(),
            },
        );
        assert_eq!(dashboard_panel_span(&chart), 6);
        // 越界的非分隔面板同样收口。
        let odd = panel(
            3,
            40,
            DashboardPanelKind::Inspector {
                view: InspectorView::Processes,
            },
        );
        assert_eq!(dashboard_panel_span(&odd), DASHBOARD_COLUMNS);
    }

    /// 面板 id 不回收：删掉中间那块之后，新加的不会撞上还活着的 id。
    #[test]
    fn dashboard_panel_ids_are_never_reused() {
        let mut board = Dashboard {
            id: 1,
            name: "看板".to_owned(),
            panels: vec![
                panel(
                    1,
                    6,
                    DashboardPanelKind::Divider {
                        title: "一".into()
                    },
                ),
                panel(
                    3,
                    6,
                    DashboardPanelKind::Divider {
                        title: "二".into()
                    },
                ),
            ],
        };
        assert_eq!(board.next_panel_id(), 4, "取最大值 +1，不复用 2");
        board.panels.push(panel(
            board.next_panel_id(),
            6,
            DashboardPanelKind::Divider {
                title: "三".into()
            },
        ));
        assert_eq!(board.next_panel_id(), 5);
        assert_eq!(Dashboard::next_id(&[]), 1);
        assert_eq!(Dashboard::next_id(&[board]), 2);
    }

    /// 前挪 / 后挪：与相邻面板换位，越界不动（不 panic、不绕回）。
    #[test]
    fn moving_a_panel_swaps_it_with_its_neighbour() {
        let mut board = Dashboard {
            id: 1,
            name: "看板".to_owned(),
            panels: vec![
                panel(
                    1,
                    6,
                    DashboardPanelKind::Divider {
                        title: "一".into()
                    },
                ),
                panel(
                    2,
                    6,
                    DashboardPanelKind::Divider {
                        title: "二".into()
                    },
                ),
                panel(
                    3,
                    6,
                    DashboardPanelKind::Divider {
                        title: "三".into()
                    },
                ),
            ],
        };
        let ids = |board: &Dashboard| board.panels.iter().map(|p| p.id).collect::<Vec<_>>();
        board.move_panel(0, 1);
        assert_eq!(ids(&board), vec![2, 1, 3]);
        board.move_panel(2, -1);
        assert_eq!(ids(&board), vec![2, 3, 1]);
        // 两个边界：第一块往前、最后一块往后都不动。
        board.move_panel(0, -1);
        assert_eq!(ids(&board), vec![2, 3, 1]);
        board.move_panel(2, 1);
        assert_eq!(ids(&board), vec![2, 3, 1]);
    }

    /// 落盘往返：面板的「类型 + 跨度」两件事都得原样回来（面板是展平进外层结构的）。
    #[test]
    fn dashboards_round_trip_through_json() {
        let board = Dashboard {
            id: 7,
            name: "生产监控".to_owned(),
            panels: vec![
                panel(
                    1,
                    12,
                    DashboardPanelKind::Divider {
                        title: "上半场".into(),
                    },
                ),
                panel(
                    2,
                    6,
                    DashboardPanelKind::Chart {
                        saved_chart_id: "chart-2".into(),
                    },
                ),
                panel(
                    3,
                    6,
                    DashboardPanelKind::Inspector {
                        view: InspectorView::Processes,
                    },
                ),
            ],
        };
        let json = serde_json::to_string(&board).expect("可序列化");
        // 展平：面板里直接读得到 `kind`，不是 `kind: { kind: … }` 两层。
        assert!(json.contains(r#""kind":"divider""#), "{json}");
        assert!(json.contains(r#""kind":"chart""#), "{json}");
        assert!(json.contains(r#""kind":"inspector""#), "{json}");
        let back: Dashboard = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back.id, 7);
        assert_eq!(back.panels.len(), 3);
        assert_eq!(back.panels[0].span, 12);
        assert_eq!(
            back.panels[1].kind,
            DashboardPanelKind::Chart {
                saved_chart_id: "chart-2".into()
            }
        );
        assert_eq!(
            back.panels[2].kind,
            DashboardPanelKind::Inspector {
                view: InspectorView::Processes
            }
        );
        // 老 kv（没有 `span` / 没有 `panels`）也得读得进来 —— 缺字段走 serde 默认。
        let legacy: Dashboard =
            serde_json::from_str(r#"{"id":1,"name":"旧板"}"#).expect("缺字段要能回落");
        assert!(legacy.panels.is_empty());
        let no_span: Dashboard = serde_json::from_str(
            r#"{"id":1,"name":"旧板","panels":[{"id":1,"kind":"divider","title":"段"}]}"#,
        )
        .expect("缺 span 要能回落");
        assert_eq!(no_span.panels[0].span, DASHBOARD_DEFAULT_SPAN);
    }

    /// 检查器两档的稳定键往返（下拉回投靠它）。
    #[test]
    fn inspector_view_keys_round_trip() {
        for view in InspectorView::ALL {
            assert_eq!(InspectorView::from_key(view.key()), Some(view));
        }
        assert_eq!(InspectorView::from_key("bogus"), None);
        // 两档的键两两不同（同一个下拉里不能有两个同值项）。
        let keys: std::collections::BTreeSet<&str> =
            InspectorView::ALL.iter().map(|view| view.key()).collect();
        assert_eq!(keys.len(), InspectorView::ALL.len());
    }

    /// 复制一块仪表板：面板原样搬过去（id 另起，免得两块板的面板 id 系列对不上）。
    #[test]
    fn duplicating_a_dashboard_copies_its_panels() {
        let board = Dashboard {
            id: 1,
            name: "生产监控".to_owned(),
            panels: vec![panel(
                1,
                6,
                DashboardPanelKind::Chart {
                    saved_chart_id: "chart-1".into(),
                },
            )],
        };
        let copy = board.duplicate(9, "生产监控 副本".to_owned());
        assert_eq!(copy.id, 9);
        assert_eq!(copy.name, "生产监控 副本");
        assert_eq!(copy.panels.len(), 1);
        assert_eq!(copy.panels[0].id, 1);
        assert_eq!(copy.panels[0].kind, board.panels[0].kind);
    }

    /// 造一块面板（测试排版用；`kind` 直接给，省得写一串字段名）。
    fn panel(id: u32, span: u32, kind: DashboardPanelKind) -> DashboardPanel {
        DashboardPanel { id, span, kind }
    }

    // ── 连接钩子（#19） ──────────────────────────────────────────────

    /// 视图类测试的前置：按 `main.rs` 的顺序装组件与主题全局。
    ///
    /// `InputState::new` 会读主题（占位符配色 / 字阶），没装全局就直接 panic
    /// （报的是「no state of type DuckThemeState / ThemeRegistry exists」，
    /// 与钩子本身无关）。
    fn init_theme(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            duck_theme::init(cx);
        });
    }

    /// 载体两档的稳定键两两不同（元素 id 靠它，撞了两个按钮就成了同一个）。
    #[test]
    fn hook_mode_keys_are_distinct() {
        let keys: std::collections::BTreeSet<&str> = [HookMode::Command, HookMode::Script]
            .iter()
            .map(|mode| mode.key())
            .collect();
        assert_eq!(keys.len(), 2);
        assert_eq!(HookMode::Command.key(), "command");
        assert_eq!(HookMode::Script.key(), "script");
    }

    /// 新建一行钩子的默认值是**明确**的：命令档 / 建连前 / 启用 / 不阻断 / 不单独设超时。
    ///
    /// 这四条合起来决定了「用户刚加一条钩子、还没填命令」时的语义，任何一条漂了都会
    /// 变成「刚加就挡住建连」这类意外。
    #[gpui::test]
    fn a_new_hook_row_defaults_to_an_enabled_pre_connect_command(cx: &mut TestAppContext) {
        init_theme(cx);
        cx.add_window(|window, cx| {
            let row = cx.new(|cx| HookRow::new("hook-1".to_owned(), None, window, cx));
            let hook = row.read(cx).to_hook(cx);
            assert_eq!(hook.id, "hook-1");
            assert_eq!(hook.phase, HookPhase::PreConnect);
            assert!(hook.enabled);
            assert_eq!(
                hook.failure,
                HookFailure::Warn,
                "新建的钩子不该默认阻断建连"
            );
            assert_eq!(hook.timeout_secs, None);
            assert!(matches!(hook.action, HookAction::Command { .. }));
            // 空命令 = 没填完；只有**启用**的才拦（停用那行留空不该挡住保存）。
            assert!(row.read(cx).is_incomplete(cx));
            let row_entity = row.clone();
            row_entity.update(cx, |row, cx| {
                row.enabled = false;
                cx.notify();
            });
            assert!(!row.read(cx).is_incomplete(cx));
            SqlEditorProbe
        });
    }

    /// 脚本档读回的是「解释器 + 文件」，并且**两端空白都去掉**、空的工作目录变 `None`。
    #[gpui::test]
    fn script_hook_rows_read_back_into_core_scripts(cx: &mut TestAppContext) {
        init_theme(cx);
        cx.add_window(|window, cx| {
            let row = cx.new(|cx| HookRow::new("hook-2".to_owned(), None, window, cx));
            let (interpreter, path, cwd, timeout) = {
                let row = row.read(cx);
                (
                    row.interpreter.clone(),
                    row.path.clone(),
                    row.cwd.clone(),
                    row.timeout.clone(),
                )
            };
            for (state, value) in [
                (&interpreter, "  bash  "),
                (&path, " /tmp/hook.sh "),
                // 只填空格的工作目录等于没填（`current_dir("")` 会直接失败）。
                (&cwd, "   "),
                (&timeout, "5"),
            ] {
                state.update(cx, |state, cx| state.set_value(value, window, cx));
            }
            row.update(cx, |row, _| row.mode = HookMode::Script);
            let hook = row.read(cx).to_hook(cx);
            match &hook.action {
                HookAction::Script {
                    interpreter,
                    path,
                    cwd,
                } => {
                    assert_eq!(interpreter, "bash");
                    assert_eq!(path, "/tmp/hook.sh");
                    assert_eq!(*cwd, None);
                }
                other => panic!("脚本档读回的不是脚本：{other:?}"),
            }
            assert_eq!(hook.timeout_secs, Some(5));
            assert!(!row.read(cx).is_incomplete(cx), "填全了就不该算没填完");

            // 解析不出数字的超时按「没填」处理，不卡住保存。
            timeout.update(cx, |state, cx| state.set_value("很久", window, cx));
            assert_eq!(row.read(cx).to_hook(cx).timeout_secs, None);
            // 解释器清空 → 又算没填完。
            interpreter.update(cx, |state, cx| state.set_value("  ", window, cx));
            assert!(row.read(cx).is_incomplete(cx));
            SqlEditorProbe
        });
    }

    /// 已有档案填充表单时，相位 / 载体 / 失败策略 / 超时都要原样回来（编辑不该悄悄改配置）。
    #[gpui::test]
    fn an_existing_hook_fills_the_row_verbatim(cx: &mut TestAppContext) {
        init_theme(cx);
        cx.add_window(|window, cx| {
            let existing = ConnectionHook {
                id: "hook-7".to_owned(),
                phase: HookPhase::PostDisconnect,
                enabled: false,
                action: HookAction::Script {
                    interpreter: "python3".to_owned(),
                    path: "/tmp/bye.py".to_owned(),
                    cwd: Some("/tmp".to_owned()),
                },
                failure: HookFailure::Block,
                timeout_secs: Some(12),
            };
            let row = cx.new(|cx| HookRow::new("hook-7".to_owned(), Some(&existing), window, cx));
            assert_eq!(row.read(cx).to_hook(cx), existing);
            // 停用的行不被「没填完」检查拦下（它有内容，但语义上本就不跑）。
            assert!(!row.read(cx).is_incomplete(cx));
            SqlEditorProbe
        });
    }

    /// 造一份只填了主机的代理档。
    fn test_proxy_at(host: &str) -> ProxyConfig {
        ProxyConfig {
            kind: ProxyKind::Socks5,
            host: host.into(),
            port: None,
            username: String::new(),
            password: None,
        }
    }

    /// 造一份只填了实例 id 的 SSM 档。
    fn test_ssm_at(instance: &str) -> SsmConfig {
        SsmConfig {
            instance: instance.into(),
            remote_host: None,
            region: None,
            profile: None,
        }
    }

    /// 四档传输的中文名两两不同（下拉标签与状态卡都读它，撞名就分不清在走哪条路）。
    #[gpui::test]
    fn transport_labels_are_distinct(cx: &mut TestAppContext) {
        init_theme(cx);
        cx.update(|cx| {
            let labels: std::collections::BTreeSet<&str> = [
                TransportKind::Direct,
                TransportKind::Ssh,
                TransportKind::Proxy,
                TransportKind::Ssm,
            ]
            .iter()
            .map(|kind| transport_kind_label(*kind, cx))
            .collect();
            assert_eq!(labels.len(), 4, "四档标签不能有重名：{labels:?}");
        });
    }

    /// 档案「是哪一档」的判据与内核同源：**填不全的档位不算数**，且优先级一致。
    #[test]
    fn a_profile_reports_only_fully_filled_transports() {
        assert_eq!(TunnelProfile::default().kind(), None);

        let mut profile = TunnelProfile {
            id: "tunnel-1".into(),
            name: "档案".into(),
            ssh: Some(SshTunnelConfig {
                host: "  ".into(),
                user: "duck".into(),
                ..SshTunnelConfig::default()
            }),
            ..TunnelProfile::default()
        };
        assert_eq!(profile.kind(), None, "主机空白 = 没填完，不该说它是 SSH");

        profile.ssh = Some(SshTunnelConfig {
            host: "bastion".into(),
            user: "duck".into(),
            ..SshTunnelConfig::default()
        });
        assert_eq!(profile.kind(), Some(TransportKind::Ssh));

        profile.proxy = Some(test_proxy_at("proxy.local"));
        assert_eq!(
            profile.kind(),
            Some(TransportKind::Ssh),
            "SSH 优先于代理（与内核 transport() 同一顺序）"
        );

        profile.ssm = Some(test_ssm_at("i-0123456789abcdef0"));
        assert_eq!(profile.kind(), Some(TransportKind::Ssm));
    }

    /// 展开档案：**整体替换**三档；档案不在时回落连接自己的三档（绝不静默直连）。
    #[test]
    fn a_referenced_profile_replaces_all_three_transports() {
        let mut cfg = ConnectionConfig {
            kind: DatabaseKind::Postgres,
            host: "db.local".into(),
            ssh_tunnel: Some(SshTunnelConfig {
                host: "own-bastion".into(),
                user: "duck".into(),
                ..SshTunnelConfig::default()
            }),
            ..ConnectionConfig::default()
        };
        // 没引用档案：原样返回，`hit` 为空。
        let (out, hit) = expand_tunnel_profile(&cfg, &[]);
        assert!(hit.is_none());
        assert_eq!(out.transport(), TransportKind::Ssh);

        // 引用的档案不存在：**回落连接自己的三档**，而不是变成直连。
        cfg.tunnel_profile = Some("tunnel-missing".into());
        let (out, hit) = expand_tunnel_profile(&cfg, &[]);
        assert!(hit.is_none());
        assert_eq!(out.transport(), TransportKind::Ssh);
        assert!(out.ssh_tunnel.is_some());

        // 命中档案：三档整体换成档案的——连接自己的 SSH 必须让位。
        cfg.tunnel_profile = Some("tunnel-1".into());
        let profile = TunnelProfile {
            id: "tunnel-1".into(),
            name: "公司代理".into(),
            proxy: Some(test_proxy_at("proxy.corp")),
            ..TunnelProfile::default()
        };
        let (out, hit) = expand_tunnel_profile(&cfg, std::slice::from_ref(&profile));
        assert_eq!(hit.map(|hit| hit.id.as_str()), Some("tunnel-1"));
        assert!(
            out.ssh_tunnel.is_none(),
            "档案里没有 SSH，连接上那条不能留下（否则两档都成立、只有一档生效）"
        );
        assert_eq!(out.transport(), TransportKind::Proxy);
        // 引用本身留在展开结果上，保存时才知道该写回哪份档案。
        assert_eq!(out.tunnel_profile.as_deref(), Some("tunnel-1"));
    }
}
