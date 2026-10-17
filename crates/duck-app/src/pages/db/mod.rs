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
//!
//! # 文件划分
//!
//! 本页原本是**单文件**（`pages/db.rs`，24,100 行 / 497 个函数）。按 AGENTS.md §4.1
//! 的「三件套」范式拆成目录模块（对齐 `pages::terminal` / `pages::redis`）：**本文件只留
//! 类型定义、常量、键位与模块装配**，行为按领域切片进子模块。拆解口径是「**纯搬移**」：
//! 除下文标注的 4 处路径修正外，每个函数的函数体逐字节不变（校验方式见
//! `docs/database-design.md` §3.1）。
//!
//! | 文件 | 行数 | 承担 |
//! |---|---|---|
//! | `mod.rs` | 2,329 | 全部类型定义（60 个结构 / 枚举）、常量、[`bind_keys`]、模块装配 |
//! | `helpers.rs` | 451 | 跨领域自由函数：kv 档案读取、偏好取值、格式化、结果集纯计算 |
//! | `objects.rs` | 115 | 模式对象的图标 / 标签与 DDL 骨架（建表 / 改表 / 删表模板） |
//! | `tunnel.rs` | 314 | 传输四档：解析 / 展开 / 打开 / 探活，以及失败分层与文案 |
//! | `hooks.rs` | 297 | 连接钩子四相位 + 并发闸门 [`DbOpGate`] / [`DbOpPermit`] |
//! | `completion.rs` | 202 | SQL 编辑器补全（`CompletionProvider` 实现，不碰会话） |
//! | `profiles.rs` | 709 | 连接档案 CRUD / 分组 / 隧道档案 / 导入导出 / 新建·编辑弹窗 |
//! | `connect.rs` | 1,910 | 连接表单 [`EditFormView`] 与引擎选择器 [`DriverPickerView`] |
//! | `session.rs` | 865 | 会话与标签生命周期、当前标签 / 当前会话的取值 |
//! | `browse.rs` | 3,149 | 表浏览：分页 / 排序 / 过滤条、单元格详情、左栏对象树 |
//! | `query.rs` | 2,932 | SQL 执行：排队 / 取消 / 超时、编辑器状态、单元格与行编辑、暂存区、导出 |
//! | `grid.rs` | 3,569 | 结果区：格式分发、tab 栏、列宽 / 列序、摘要·消息·计划三档、批量条 |
//! | `charts.rs` | 2,152 | 图表与用户仪表板（12 栏网格） |
//! | `builder/` | 3 文件 | 可视化 SQL 构建器：`sql.rs` 纯函数 spec → SQL、`panel.rs` 右栏面板、`mod.rs` 状态与动作 |
//! | `instance.rs` | 904 | 实例监控四档：总览 / 指标 / 检查器 / 仪表板 |
//! | `ai.rs` | 499 | AI 助手（text-to-SQL）：动作入口、产出落点、助手条与解释面板 |
//! | `render_editor.rs` | 609 | 编辑器工具条 + 诊断条 |
//! | `render_tabs.rs` | 596 | 标签条 + 「历史」抽屉 |
//! | `render_sidebar.rs` | 271 | 右栏（shell 级 dock 的数据库分段） |
//! | `shell.rs` | 1,049 | 页面骨架：`new` / `impl Render` / 主区拼装 / 状态栏 / 浮层提示 |
//! | `tests.rs` | 1,537 | 模块单测（51 个 `#[test]`，全是纯函数口径；守卫扫描会跳过它） |
//!
//! **仅有的语义改动：6 处路径修正，全在同一个成因上**。这几个函数原本用
//! `super::module_settings::…` 指兄弟页 `pages::module_settings`；进了 `pages::db`
//! 的子模块后 `super` 变成 `pages::db`，故补一层 → `super::super::module_settings::…`。
//! 落在 `helpers.rs::db_prefs`、`browse.rs::set_browse_page_size`，以及 `query.rs` 的
//! `toggle_keyword_case` / `toggle_editor_word_wrap` / `toggle_semantic_diagnostics` /
//! `apply_query_page_size`。其余 491 个函数逐字节未动。

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use duck_ai::ProviderProfile;
use duck_components::{
    ButtonVariant, CheckState, ConfirmHandler, DROPDOWN_MIN_WIDTH, DatabaseLogo, DuckIcon,
    SidebarPanel, SidebarState, body_text, button, caption_heading, card, check_mark, check_row,
    content::{
        accent_tag, description_list, description_row, empty_state, neutral_tag, progress_bar,
    },
    dialog_footer, dialog_top_offset, dropdown_select, field_hint, field_label, form_gap,
    icon_button, icon_button_tip, icon_text_button, meta_text, modal_drag, modal_drag_area,
    modal_offset, modal_title_row, notify, segmented_tabs, selectable_text,
    selectable_text_with_id, sidebar_navigation_button, tabular_numbers, title_text,
    toolbar_action_button, toolbar_dropdown_button, toolbar_text_dropdown_button,
};
// 结果网格的行模式：公共数据表组件（行虚拟化 + 固定列头 + 列宽拖拽 + 就地编辑槽）。
// 列宽 / 藏列 / 展示行序 / 滚动句柄归组件；选中 / 勾选 / 排序 / 编辑器实体留在页面
// （见 `duck_components::data_table` 模块文档的「归属边界」）。
use duck_components::data_table::{
    CellKind, CellPos, CellValue, ColumnSpec, DataTableEvent, DataTableHooks, DataTableLabels,
    DataTableState, DataTableView, HeaderMenuHook, RowData, TableModel, data_table,
};
use duck_db::ai::{AiAction, build_action_prompt};
use duck_db::config::{
    AuthMode, ConnectionConfig, DatabaseKind, EnvTag, SshAuthMethod, SshTunnelConfig, TlsMode,
    TransportKind, delete_password, delete_profile_proxy_password,
    delete_profile_ssh_key_passphrase, delete_profile_ssh_password, delete_proxy_password,
    delete_ssh_key_passphrase, delete_ssh_password, load_password, load_profile_proxy_password,
    load_profile_ssh_key_passphrase, load_profile_ssh_password, load_proxy_password,
    load_ssh_key_passphrase, load_ssh_password, store_password, store_profile_proxy_password,
    store_profile_ssh_key_passphrase, store_profile_ssh_password, store_proxy_password,
    store_ssh_key_passphrase, store_ssh_password,
};
use duck_db::drivers::CellEdit;
use duck_db::import::import_third_party;
use duck_db::sqldiag::Severity as DiagSeverity;
use duck_db::sqlintel::{
    ExplainSkip, SqlRisk, TxControl, TxEffect, allow_auto_execute, explain_sql_for,
    risk_of_statements, split_statement_spans, split_statements, tx_effect,
};
use duck_db::sqlwords::{QueryLanguage, SqlWords};
use duck_db::{
    AiProvider, BatchOutcome, BinaryEncoding, CancelHandle, CancelTimer, CatalogTable, Column,
    Connection, ConnectionHook, ConnectionHooks, DEFAULT_HOOK_TIMEOUT_SECS, DatabaseObject,
    DbError, EditTarget, ExecOutcome, FileProgress, FileReport, FilterOp, HookAction, HookContext,
    HookFailure, HookOutcome, HookPhase, HookReport, HookStatus, InsertRow, InstanceMetric,
    InstanceOverview, InstanceTable, InstanceView, OpenAiCompatible, OrderBy, PkValues, PlanNode,
    PlanTree, ProxyConfig, ProxyKind, ProxyTunnel, ProxyTunnelSpec, ResultSet, RowCount,
    SqlFileError, SqlFileOptions, SshTunnel, SshTunnelSpec, SsmConfig, SsmTunnel, SsmTunnelSpec,
    SwitchOutcome, TableFilter, TableForeignKey, TableIndex, TableKind, TableRef, TableTrigger,
    Value, covers_columns, edit_target, execute_sql_file, fingerprint, parse_csv, parse_plan,
    quote_identifier, quote_qualified, run_hook, run_phase, tsv_field,
};
use duck_drivers::registry::DriverRegistry;
use duck_drivers::{
    CancelFlag, DownloadSource, DriverEntry, DriverEvent, DriverStore, EventSink, InstalledDriver,
    ProgressStep,
};
// 选择器里有一张卡是**指路**而不是连库（Redis 有自己的页）：切路由要 `RouteId`。
use duck_shell::RouteId;
use duck_theme::{UnderMinute, active_theme, tr, tr_relative_time, trf, trn};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClickEvent, Context, CursorStyle, DismissEvent,
    Empty, Entity, FocusHandle, Focusable as _, FontWeight, Hsla, InteractiveElement, IntoElement,
    KeyBinding, MouseButton, ParentElement, Pixels, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Task, Toggled, WeakEntity, Window,
    actions, div, prelude::FluentBuilder as _, px, relative, uniform_list,
};
use gpui_component::{
    Icon, ResizableState, Sizable as _, Size, WindowExt as _,
    chart::{AreaChart, BarChart, LineChart, PieChart},
    dialog::{DialogButtonProps, DialogFooter},
    h_flex,
    input::{
        CompletionProvider, Editor, EditorState, Input, InputEvent, InputState, Rope, Textarea,
        TextareaState,
    },
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    resizable_panel,
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    tooltip::Tooltip,
    v_flex, v_resizable,
};
// `CompletionProvider` 的签名直接引用 lsp-types 的类型，实现方必须能具名它们；
// 版本与 gpui-component 经 `gpui_component::input` 转出的 `lsp_types::Position` 同源。
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse};
use serde::{Deserialize, Serialize};

// 审计条目按 `AuditEntry` 的链式构造器拼（语句级事件要带指纹与耗时）。
use crate::audit::AuditEntry;
// Redis 那张卡点下去是**切路由**（`PickerTarget::Page`），本页不接它的表单。
use crate::pages::REDIS_ROUTE;

/// 可搜索下拉（gpui-component `Select`）的选项条目：`value` 是回传的稳定值，
/// `label` 是展示文案（列名清单里两者相同；解耦字段留给未来的展示修饰）。
#[derive(Clone)]
pub(super) struct DbSelectOption {
    pub(super) value: SharedString,
    pub(super) label: SharedString,
}

impl SelectItem for DbSelectOption {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// db 页四个可搜索下拉（过滤列 / 批量列 / 命名空间×2）共用的状态类型。
pub(super) type DbSelectState = SelectState<SearchableVec<DbSelectOption>>;

mod ai;
mod brands;
mod browse;
// 品牌表本体（由 `scripts/gen_brands.py` 生成）。只服务 `brands.rs`，
// 所以不带 `pub`、也不在这里转出名字。
mod brands_gen;
mod builder;
mod charts;
mod charts_plot;
mod compare;
mod completion;
mod connect;
mod drivers;
mod er;
mod grid;
mod helpers;
mod hooks;
mod instance;
mod objects;
mod profiles;
mod query;
mod render_editor;
mod render_sidebar;
mod render_tabs;
mod session;
mod shell;
mod structure;
mod transfer;
mod tunnel;

// 含自由函数的子模块：名字转出到父模块，供各兄弟子模块取用（范式同 `pages::terminal` /
// `pages::redis`）。只有 `impl` 块、没有自由函数的子模块（`ai` / `completion` / `query` /
// `session` / `render_editor` / `render_sidebar` / `render_tabs` / `shell`）无需转出；
// `grid` 的自由函数（执行计划树）只在它自己内部用，也不转出。
use self::brands::*;
use self::browse::*;
use self::builder::*;
use self::charts::*;
use self::charts_plot::*;
use self::compare::*;
use self::connect::*;
use self::drivers::*;
use self::er::*;
use self::helpers::*;
use self::hooks::*;
use self::instance::*;
use self::objects::*;
use self::profiles::*;
use self::transfer::*;
use self::tunnel::*;

/// kv 单键：连接档案数组（结构体 → serde_json，范式同 `appearance`）。
const KEY_CONNECTIONS: &str = "db_connections";
/// 表浏览每页行数的**出厂默认**（可在「设置 → 数据库」里改）。
pub(super) const PAGE_SIZE: usize = 200;
/// 表浏览翻页条上「每页行数」的可选档位（写的是同一个 `db.settings.page_size`）。
///
/// 上限 2000 是刻意的：每页行数落实为**服务器侧 LIMIT**，档位再往上拉就把「分页」
/// 本身抵消了（等于给用户一个按钮把整张表拉进内存）。要一次看很多行就走导出。
const BROWSE_PAGE_CHOICES: [u32; 5] = [100, 200, 500, 1_000, 2_000];
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
/// 表结构页「编辑面」的初始高度。
///
/// 只是一份**首帧起点**：之后高度由 [`ResizableState`] 记着用户拖出来的值（并按容器
/// 伸缩等比调整），见 `render_structure_page` 的上下分栏 —— 与「SQL 编辑器 / 结果区」
/// 同一条机制。取 340px 是把默认天平往 SQL 预览那边压一压：字段网格两三分屏就能
/// 编辑，而预览要放得下整段 DDL / ALTER 才叫「预览」（原来它被压在 `max_h(200)`
/// 里，只剩一条缝）。
const STRUCTURE_EDITOR_INITIAL_HEIGHT: f32 = 340.;
/// 结构页编辑面的可拖拽高度档位（下边框把手）。
const STRUCTURE_EDITOR_HEIGHT_RANGE: std::ops::RangeInclusive<f32> = 160.0..=900.0;
/// 结构页 SQL 预览的可拖拽高度档位（留得住一行语句，也容得下长 DDL）。
const STRUCTURE_PREVIEW_HEIGHT_RANGE: std::ops::RangeInclusive<f32> = 120.0..=700.0;
/// 网格行高：与 `typography.sm` 的 18px 行高同源，行内文字不再压缩。
const GRID_ROW_HEIGHT: f32 = 24.;
/// 网格**列头**行高：两行（列名 + 类型），对齐设计稿的 `colhead`。
const GRID_HEADER_HEIGHT: f32 = 38.;
/// 网格列宽的下限（列模式（转置视图）三列仍用这一档夹宽）。
///
/// 行模式网格的估宽上下限（含「一屏塞得下」的 260 上限）已随网格一起搬进公共数据表
/// 组件（`duck_components::data_table::{MIN_COLUMN_WIDTH, MAX_AUTO_WIDTH}`）。
const COLUMN_MIN_WIDTH: f32 = 72.;
/// **手动拖拽**能拉到的最大宽度：估宽夹在 260 是为了「一屏塞得下」，
/// 但手动拖的人明确要放宽看长文本，就不该被 260 顶住。
const COLUMN_RESIZE_MAX: f32 = 1200.;
/// 列宽拖拽把手的可命中带宽（对齐 gpui-component Table 的 `HANDLE_PADDING`）。
const COLUMN_RESIZE_HANDLE: f32 = 4.;
/// 列模式（转置视图）三列的出厂宽度：名称 / 值 / 类型。
///
/// 双击把手回到这一组；拖拽改出的宽度只留在当前会话（不落盘）。
const COLUMNS_MODE_DEFAULT_WIDTHS: [f32; 3] = [160., 360., 120.];
/// 「消息」tab 执行流水的保留条数（新条目在尾、裁头部）。
const EXEC_LOG_CAP: usize = 200;
/// 破坏性动作确认框的宽度。
///
/// 正文都是一行问句（「将从数据库中删除 N 行，确定继续吗？」），440 与 redis 页的
/// 确认框同档：窄到能让目光一次扫完，宽到问句不必折行。
const CONFIRM_DIALOG_WIDTH: f32 = 440.;
/// SQL 编辑器**首行文本**距编辑器元素顶边的内边距。
///
/// gpui-component 的 `Input` 渲染多行编辑器时会把 `editor_paddings.top` 设成
/// `Size::Medium.input_py()`（input.rs 的 `set_editor_paddings`，Medium 档 = 8px）。
/// 语句槽的 ▶ 用 `scroll_offset().y + 行号 × 行高` 定位，缺了这 8px 整列图标
/// 会整体悬在对应行上方——正是「执行图标和行没对齐」的根因。
const EDITOR_TOP_PADDING: f32 = 8.;
/// 左栏宽度档位。
const SIDEBAR_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 184.0..=340.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 236.;
/// 左栏连接行的品牌 logo 边长。
///
/// 比图标阶梯（`icon.lg` 16px）大一号：品牌图形细节密（海豚 / 大象），塞进 14px
/// 的档位就是一团色渍（用户实测「根本看不清」）；18px 在 `density.navRow` 的行高
/// 里放得下，也和行内 12px 的小图标拉开层级。图片定尺寸不算「裸 px 字号」——
/// 那条禁令管的是文字（要随字号 / 缩放设置走），品牌 logo 与 `card_logo_size`
/// （22px）同例。
const CONN_LOGO_SIZE: f32 = 18.;
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
/// 全局并发上限：同时最多允许多少条数据库操作在跑（`docs/database-design.md` §4.4）。
///
/// 三引擎的连接都是**一条连接一个锁**，真正的并行度受连接数限制；这个闸门挡的是
/// 「多个连接同时压满」以及「同一页反复点触发一串操作把 background_executor 占满」。
const MAX_CONCURRENT_DB_OPS: usize = 4;
/// 「执行 SQL 文件」的超时守护时长（**防呆**，不是日常判据）。
///
/// 单条语句那套超时预算（[`DatabasePage::effective_timeout`]）在这里用不了：一整份
/// 脚本跑十分钟是常态，按 30 秒掐掉会把一次正常的导入打成失败。所以文件执行按
/// **整天**兜底 —— 守护到点仍会送那一枪（引擎侧真卡死时还有救），停止的日常入口是
/// 工具条「停止」（它同时置逐条生效的停止谓词）。
const SQL_FILE_GUARD_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// 文件执行的进度轮询间隔（界面读数刷新频率；后台按 100ms 合流上报，见 `sqlfile`）。
const SQL_FILE_PROGRESS_POLL: Duration = Duration::from_millis(150);
/// 工具条进度条前面那个文件名的宽度上限（超长截断，不把进度条挤没）。
const SQL_FILE_NAME_WIDTH: f32 = 140.;
/// 「新建连接」驱动选择弹窗宽度。
const PICKER_WIDTH: f32 = 720.;
/// 驱动卡片网格列数（弹窗宽度固定，3 列对齐 dbflux 的密度）。
const PICKER_COLUMNS: usize = 3;
/// 驱动选择弹窗**分类侧栏**的宽度：九档中文标签一行放得下的最窄值。
const PICKER_RAIL_WIDTH: f32 = 132.;
/// 连接详情弹窗（picker 的子页）宽度。
const EDIT_DIALOG_WIDTH: f32 = 720.;
/// 「选择显示数据库」弹窗宽度：单列勾选清单，比详情弹窗窄一档。
const VISIBLE_DBS_DIALOG_WIDTH: f32 = 480.;
/// 「选择显示数据库」弹窗清单的最大高度；超出后清单自身滚动（标题与脚注常驻）。
const VISIBLE_DBS_LIST_MAX_HEIGHT: f32 = 360.;
/// 连接表单的四页签（dbx 同形：连接信息 / TLS/SSL / 隧道/代理 / 高级）。
///
/// ⚠️ 顺序即界面顺序；「TLS/SSL」「隧道/代理」两页签**仅网络引擎显示**（文件型
/// 引擎没有这两层），显示数变化时调用方把下标夹回可用范围。
const FORM_TABS: [&str; 4] = ["连接信息", "TLS/SSL", "隧道/代理", "高级"];
/// 详情弹窗正文的最大高度；超出后正文自身滚动（头部与脚注常驻）。
const EDIT_DIALOG_BODY_MAX_HEIGHT: f32 = 540.;
/// 「新建分组」提示弹窗宽度：正文只有一行输入，比常规确认弹窗窄一档。
const GROUP_DIALOG_WIDTH: f32 = 380.;
/// 「新建数据库」弹窗宽度：同为单行输入弹窗，与分组弹窗同档。
const CREATE_DB_DIALOG_WIDTH: f32 = 380.;
/// 「保存到 SQL 库」弹窗宽度：比单输入弹窗宽一档 —— 文件名（`<表名>.sql` 可能不短）
/// 与文件夹两行都要放得下，下面还有一排「已有文件夹」快选。
const SAVE_SQL_DIALOG_WIDTH: f32 = 460.;
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
        /// 全选 / 清空当前结果集的行（网格里 `⌘A` 的语义）：网格聚焦时接住这套键，
        /// 不漏给先前聚焦的 SQL 编辑器（dbx 同款「聚焦的网格拥有剪贴板键」）。
        SelectAllRows,
        /// 剪切选中的那一格（`⌘X`，dbx 同款）：复制格文本 + 原格暂存为 `NULL`。
        /// 走暂存模型 —— `⌘Z` 可退、提交前可整批丢弃。
        CutSelectedCell,
        /// 粘贴到选中的那一格（`⌘V`，dbx 同款）：剪贴板文本暂存为新值。
        /// 就地编辑打开时不接（输入框自己处理原生粘贴）。
        PasteIntoSelectedCell,
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
        // 网格里的 `⌘A` / `⌘X` / `⌘V`（dbx 同款：聚焦的网格拥有剪贴板键）：
        // 焦点在 SQL 编辑器 / 就地编辑输入框里时，更深的编辑器上下文先接管
        // （全选文字 / 剪切 / 粘贴都是原生语义）；焦点在结果网格上时才归网格。
        KeyBinding::new(&format!("{modifier}-a"), SelectAllRows, Some(KEY_CONTEXT)),
        KeyBinding::new(&format!("{modifier}-x"), CutSelectedCell, Some(KEY_CONTEXT)),
        KeyBinding::new(
            &format!("{modifier}-v"),
            PasteIntoSelectedCell,
            Some(KEY_CONTEXT),
        ),
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

// ── 驱动选择器（dbx 同形：分类侧栏 + 品牌卡片网格）────────────────────────────

/// 驱动选择器的分类（**九档，对齐 dbx 与驱动注册表**）。
///
/// ⚠️ 这九档与 `duck_drivers` 清单里的 `category` 是**同一套词表**（清单本来就是
/// 从 dbx 生成的）：侧栏直接按它分档，agent 驱动的归属就不再需要一层映射；
/// 手写卡（内建七档 + 指路 / 未支持）各自固定归一档，见 [`PICKER_DRIVERS`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerSection {
    /// 关系型数据库。
    Sql,
    /// 数仓分析。
    Analytics,
    /// 国产数据库。
    Domestic,
    /// 轻量数据库。
    Lightweight,
    /// 文档 / 缓存 / 检索。
    Document,
    /// 图谱 / 向量。
    GraphAi,
    /// 时序数据库。
    TimeSeries,
    /// 消息队列。
    Mq,
    /// 注册与配置。
    RegistryConfig,
}

/// 分类展示顺序（dbx 同序；无卡的分类不出现在侧栏）。
const PICKER_SECTIONS: &[PickerSection] = &[
    PickerSection::Sql,
    PickerSection::Analytics,
    PickerSection::Domestic,
    PickerSection::Lightweight,
    PickerSection::Document,
    PickerSection::GraphAi,
    PickerSection::TimeSeries,
    PickerSection::Mq,
    PickerSection::RegistryConfig,
];

impl PickerSection {
    fn label(self, cx: &App) -> SharedString {
        let label = match self {
            Self::Sql => tr(cx, "关系型数据库"),
            Self::Analytics => tr(cx, "数仓分析"),
            Self::Domestic => tr(cx, "国产数据库"),
            Self::Lightweight => tr(cx, "轻量数据库"),
            Self::Document => tr(cx, "文档/缓存/检索"),
            Self::GraphAi => tr(cx, "图谱/向量"),
            Self::TimeSeries => tr(cx, "时序数据库"),
            Self::Mq => tr(cx, "消息队列"),
            Self::RegistryConfig => tr(cx, "注册与配置"),
        };
        SharedString::from(label)
    }

    /// 驱动清单里的分类 → 选择器分类（同一套词表，只兜底未知值）。
    fn of_category(category: &str) -> Self {
        match category {
            "analytics" => Self::Analytics,
            "domestic" => Self::Domestic,
            "lightweight" => Self::Lightweight,
            "document" => Self::Document,
            "graph_ai" => Self::GraphAi,
            "timeseries" => Self::TimeSeries,
            "mq" => Self::Mq,
            "registry_config" => Self::RegistryConfig,
            _ => Self::Sql,
        }
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
    /// 选中它之后进哪条路。
    target: PickerTarget,
}

/// 选中一张卡之后开什么。
///
/// 做成一枚举而不是「`kind: Option<DatabaseKind>` + 另一个 `driver` 字段」：
/// 那两个字段是有**约束**的（驱动键只对 agent 档有意义，其余两档必须为空），
/// 用枚举让编译器替我们守着，比在构造函数里写一句注释可靠。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerTarget {
    /// 内建七档之一。
    ///
    /// 一张卡落在这一档**不代表**它的牌子就是那个变体的名字：MariaDB 是
    /// [`DatabaseKind::Mysql`]、Redshift 是 [`DatabaseKind::Postgres`] —— 它们各自
    /// 拥有同一个线协议，差别在服务端。品牌留在卡片上，档案里存的是引擎档位
    /// （dbx 同法：`profiles/catalog.yaml` 给 MariaDB 的 `dbType` 就是 `mysql`）。
    Builtin(DatabaseKind),
    /// **子进程（agent）档**：档位恒为 [`DatabaseKind::Agent`]，谁是引擎看这个键
    /// （[`duck_drivers::DriverEntry::key`]）。
    Agent(&'static str),
    /// **切到另一条路由**：本页没有它的连接形态，但它在本仓别处有完整的页。
    ///
    /// 只有 Redis 一条（`pages::redis`，⌘5）。它自有一套档案 / 会话 / 命令台，
    /// 而且没有 schema 树（dbx 的 `redis.yaml` 里 `metadataBrowse: false`）——
    /// 那是一棵永远空的树，不是「连上了但没对象」。所以这里**指路**而不是硬塞进
    /// 本页的 SQL 表单：进去了也只会开出一个 `SELECT` 发不出去的空壳。
    Page(RouteId),
    /// **品牌表里的一条**（`brands`）：一个牌子（可能带多个模式）走内建协议或 agent 轨。
    ///
    /// 与 [`Self::Builtin`] / [`Self::Agent`] 的区别是**粒度**：那两个表达「走哪条路」，
    /// 这一档表达「哪个牌子」。`mysql` 协议下站着 12 个牌子（MySQL / MariaDB / TiDB /
    /// OceanBase / Doris / StarRocks …），各自端口与账号都不一样；模式族
    /// （OceanBase 的 MySQL / Oracle 模式）也靠它带出切换器。
    Brand(&'static Brand),
    /// 本应用还不支持的目标：点了只提示「即将到来」，不进表单。
    ///
    /// ⚠️ 这个分支**必须留着**：选择器里那些卡片是照着 dbx 的品牌墙列出来的，
    /// 我们没实现的目标要么老实标「即将到来」，要么就得从墙上删掉 ——
    /// 让它点进去开一个跑不通的连表单才是错的。
    ///
    /// 现在还剩下两条，成因**各不相同**（都在 `PICKER_DRIVERS` 各自的注释里）：
    /// DynamoDB 要 AWS 签名加一份新引擎；InfluxDB 连可下的工件都没有。
    /// （DuckDB 曾经也在这条里 —— 它的 worker 说的是另一套 NDJSON 协议。那套协议
    /// 现已接上，见 `duck-db` 的 `drivers::duckdb`。）
    Unsupported,
}

const PICKER_DRIVERS: &[PickerDriver] = &[
    PickerDriver {
        id: "postgresql",
        name: "PostgreSQL",
        logo: DatabaseLogo::Postgres,
        hint: ":5432",
        section: PickerSection::Sql,
        target: PickerTarget::Builtin(DatabaseKind::Postgres),
    },
    PickerDriver {
        id: "mysql",
        name: "MySQL",
        logo: DatabaseLogo::Mysql,
        hint: ":3306",
        section: PickerSection::Sql,
        target: PickerTarget::Builtin(DatabaseKind::Mysql),
    },
    PickerDriver {
        id: "mariadb",
        name: "MariaDB",
        logo: DatabaseLogo::Mariadb,
        // 端口后面的词是**协议**，不是引擎名：它告诉用户点下去会开哪一档表单。
        hint: ":3306 MySQL",
        section: PickerSection::Sql,
        // MariaDB 是 MySQL 的分支，**同一个线协议** —— 连表单里除了惯用账号没有第二处
        // 与 MySQL 不同。dbx 就这么归（`profiles/catalog.yaml` 里 `id: mariadb` 的
        // `dbType` 是 `mysql`，只换图标与标签），dbflux 更直接：「One crate implements
        // both」（其 `docs/DRIVERS.md`）。所以这张卡**不是**「即将到来」，
        // 它只是同一档引擎的另一个牌子。
        target: PickerTarget::Builtin(DatabaseKind::Mysql),
    },
    PickerDriver {
        id: "sqlserver",
        name: "SQL Server",
        logo: DatabaseLogo::Sqlserver,
        hint: ":1433",
        section: PickerSection::Sql,
        target: PickerTarget::Builtin(DatabaseKind::SqlServer),
    },
    PickerDriver {
        id: "sqlite",
        name: "SQLite",
        logo: DatabaseLogo::Sqlite,
        hint: "file",
        section: PickerSection::Lightweight,
        target: PickerTarget::Builtin(DatabaseKind::Sqlite),
    },
    PickerDriver {
        id: "redshift",
        name: "Amazon Redshift",
        logo: DatabaseLogo::Redshift,
        hint: ":5439 PostgreSQL",
        section: PickerSection::Analytics,
        // Redshift 走 **PostgreSQL 线协议**：dbx 的 `redshift.yaml` 写着
        // `pgLikeStructure: true`（`dialect: Redshift`、端口 5439），dbflux 的说明
        // 也是「Read-only over the PostgreSQL wire protocol」。所以内建那一档直接
        // 就能连。差别只在**能力面**（Redshift 只读、单语句、没有 DDL / 索引），
        // 那是引擎侧该按方言裁的事，不该在「选一张卡」这一步拦人。
        target: PickerTarget::Builtin(DatabaseKind::Postgres),
    },
    PickerDriver {
        id: "clickhouse",
        name: "ClickHouse",
        logo: DatabaseLogo::Clickhouse,
        hint: ":8123",
        section: PickerSection::Analytics,
        target: PickerTarget::Builtin(DatabaseKind::ClickHouse),
    },
    PickerDriver {
        id: "oracle",
        name: "Oracle",
        logo: DatabaseLogo::Oracle,
        hint: ":1521",
        section: PickerSection::Sql,
        // Oracle 走**子进程（agent）轨**的官方驱动进程 —— 本仓没有 Oracle 的原生 Rust
        // 驱动，协议也不是能自己实现的体量（[`duck_db_agent`] 就是为这类库准备的）。
        target: PickerTarget::Agent("oracle"),
    },
    PickerDriver {
        id: "duckdb",
        name: "DuckDB",
        logo: DatabaseLogo::Duckdb,
        hint: "file",
        section: PickerSection::Lightweight,
        // 工件从驱动商店装（dbx 注册表里六平台齐全），但**协议不是 agent 轨那套
        // JSON-RPC**：worker 说 `duckdb_worker_protocol`（`{id, method, params}` →
        // `{id, ok, result|error}`，没有 `jsonrpc` 字段、id 是字符串），动词也小一号
        // （没有 `list_indexes` / `list_triggers`），连的是 `{path}` 而不是 host/port。
        //
        // 所以这一档在**引擎实现**上另立一条（`duck-db` 的 `drivers::duckdb`，不在
        // `agent.rs` 里塞分支），在**档案形态**上仍是 agent 档（驱动键 `duckdb`）：
        // 判据只看驱动键，不看 `runtime_mode`。表单按「文件型引擎」画（只有路径一格）。
        target: PickerTarget::Agent("duckdb"),
    },
    PickerDriver {
        id: "mongodb",
        name: "MongoDB",
        logo: DatabaseLogo::Mongodb,
        hint: ":27017",
        section: PickerSection::Document,
        target: PickerTarget::Builtin(DatabaseKind::Mongo),
    },
    PickerDriver {
        id: "dynamodb",
        name: "DynamoDB",
        logo: DatabaseLogo::Dynamodb,
        hint: "AWS",
        section: PickerSection::Document,
        // 还没做，而且**不是**补个映射就有的那种：它要一套 AWS 签名（region / profile /
        // endpoint）与一份按命令走的引擎（dbflux 用的是 `aws-sdk-dynamodb`，
        // `scan`/`query`/`put`/`update`/`delete`，连查询语言都是它自己的
        // `Custom("DynamoDB")`）。注册表里也没有它的工件 —— 这条路只能自己写。
        target: PickerTarget::Unsupported,
    },
    PickerDriver {
        id: "redis",
        name: "Redis",
        logo: DatabaseLogo::Redis,
        hint: ":6379",
        section: PickerSection::Document,
        // Redis 在本仓**有自己的页**（`pages::redis`，⌘5）和独立内核 `duck-redis`，
        // 所以这张卡点下去是**指路**、不是「即将到来」。不把它接进本页还有一个更硬的
        // 理由：它没有 schema 树（dbx 的 `redis.yaml`：`metadataBrowse: false`），
        // 本页的左栏对它会是一棵永远空的树。
        target: PickerTarget::Page(REDIS_ROUTE),
    },
    PickerDriver {
        id: "influxdb",
        name: "InfluxDB",
        logo: DatabaseLogo::Influxdb,
        hint: ":8086",
        section: PickerSection::TimeSeries,
        // 三条里最「够不着」的一条：dbx 把它挂在 agent 轨上（`influxdb.yaml`：
        // `runtimeMode: agent`、`agentKey: influxdb`），但**同一份 yaml 又写着
        // `driverStoreVisible: false`** —— 它从商店里装不出来，`agents/drivers/` 下也
        // 没有 influxdb 这个 crate，远端注册表的 55 条里同样没有它。
        // 换句话说：没有工件可下，只能自己写一份（dbflux 是用一个 crate 同时做 v1/v2）。
        target: PickerTarget::Unsupported,
    },
    PickerDriver {
        id: "elasticsearch",
        name: "Elasticsearch",
        logo: DatabaseLogo::Elasticsearch,
        hint: ":9200",
        section: PickerSection::Document,
        target: PickerTarget::Builtin(DatabaseKind::Elasticsearch),
    },
];

/// 选择器里的一张卡片 —— **两个来源**。
///
/// 为什么不把手写的那 14 张和驱动清单合并成一份静态表：两者的**来源性质不同**。
/// 手写的那些是「本仓内建七档 + 几个我们明确还不支持的目标」（照着 dbx 的品牌墙排的，
/// 顺序与措辞是界面决定）；清单那些是**数据**（`duck-drivers` 从 dbx 生成，随时会跟着上游变）。
/// 混进一张静态表就得在生成器里维护界面顺序，改一次上游就冲突一次。
enum PickerCard {
    /// 手写的那几张（含 [`PickerTarget::Unsupported`] 的「即将到来」）。
    Curated(&'static PickerDriver),
    /// 驱动清单里的一条：**agent 档**，点进去还要先装驱动。
    Agent(&'static DriverEntry),
    /// 品牌表里的一条：**一个牌子一张卡**（模式族由表单里的切换器展开）。
    ///
    /// 它补的是另外两档都表达不了的粒度 —— 「同一个协议下的不同牌子」
    /// （MySQL / MariaDB / TiDB / OceanBase…）与「同一个牌子的多种模式」。
    Brand(&'static Brand),
}

impl PickerCard {
    /// 元素 id（也是搜索与去重的键）。
    ///
    /// ⚠️ 品牌卡用**族 id**：一族（OceanBase 的 MySQL / Oracle 模式）只占墙上的一格，
    /// 点进去再用切换器换模式。用品牌键会让同一个牌子的两种模式各占一格。
    fn id(&self) -> &'static str {
        match self {
            Self::Curated(driver) => driver.id,
            Self::Agent(entry) => entry.key,
            Self::Brand(brand) => brand.family,
        }
    }

    fn section(&self) -> PickerSection {
        match self {
            Self::Curated(driver) => driver.section,
            Self::Agent(entry) => PickerSection::of_category(entry.category),
            // 品牌卡按它的档位归：内建协议走 `DatabaseKind` 的分类，
            // agent 轨走驱动清单的分类 —— 与另外两档同一套直觉。
            Self::Brand(brand) => match brand.driver().and_then(duck_drivers::catalog::entry) {
                Some(entry) => PickerSection::of_category(entry.category),
                None => match brand.kind() {
                    DatabaseKind::Mongo | DatabaseKind::Elasticsearch => PickerSection::Document,
                    _ => PickerSection::Sql,
                },
            },
        }
    }

    /// 卡片上的名字。手写的那些是 ASCII 品牌名（不进 i18n 表），另外两档是中文源串
    /// （要过 `tr`）—— 两种来源的约定不同，所以在这里收口，`render` 只管拿结果。
    fn name(&self, cx: &App) -> SharedString {
        match self {
            Self::Curated(driver) => SharedString::from(driver.name),
            Self::Agent(entry) => SharedString::from(tr(cx, entry.label)),
            Self::Brand(brand) => SharedString::from(tr(cx, brand.label)),
        }
    }

    /// 第二行的短提示：端口 / `file` / `AWS`。
    ///
    /// agent 档按**缺省端口**给（`:7654`）；清单里没有端口的那几条（云端 REST 端点）
    /// 给分类名 —— 空着一行会让卡片看着像没加载完。
    fn hint(&self, cx: &App) -> SharedString {
        match self {
            Self::Curated(driver) => SharedString::from(driver.hint),
            Self::Agent(entry) => match entry.default_port {
                Some(port) => SharedString::from(format!(":{port}")),
                None => SharedString::from(tr(cx, duck_drivers::category_label_of(entry.category))),
            },
            // 品牌卡：端口 + 模式数（`:2883 · 2 种模式`）—— 后半截是这一档独有的信息，
            // 它告诉用户「这张卡点进去还要选一次」。
            //
            // ⚠️ 占位符只有 `{}` 这一种（`trf` → `fill` 逐个替换 `{}`，不认具名占位符）：
            // 写成 `{port}` / `{n}` 会被**原样打印**出来，而且编译期不报错。
            Self::Brand(brand) => {
                let modes = brands::family_of(brand.key).len();
                match brand.port {
                    Some(port) if modes > 1 => SharedString::from(trf(
                        cx,
                        ":{} · {} 种模式",
                        &[&port.to_string(), &modes.to_string()],
                    )),
                    Some(port) => SharedString::from(format!(":{port}")),
                    None if modes > 1 => {
                        SharedString::from(trf(cx, "{} 种模式", &[&modes.to_string()]))
                    }
                    None => SharedString::from(tr(cx, brand.kind().wire_name())),
                }
            }
        }
    }

    /// 品牌图；`None` = 没有这张图（调用方回落主题单色图标）。
    fn logo(&self) -> Option<DatabaseLogo> {
        match self {
            Self::Curated(driver) => Some(driver.logo),
            Self::Agent(entry) => EngineRef::new(DatabaseKind::Agent, Some(entry.key)).logo(),
            Self::Brand(brand) => brand.logo.or_else(|| {
                brand
                    .driver()
                    .and_then(|key| EngineRef::new(DatabaseKind::Agent, Some(key)).logo())
            }),
        }
    }

    fn target(&self) -> PickerTarget {
        match self {
            // 手写卡的 id 多半**就是品牌键**（`mariadb` / `sqlserver` / `clickhouse` /
            // `mysql` / `sqlite`），对得上就走品牌卡那条路 —— 否则点 MariaDB 会开出一张
            // 标题写着「MySQL」的表单（那是它的线协议，不是它的名字），多模式品牌
            // （Oracle 的 Thin / OCI）也带不出切换器。
            //
            // 对不上号的原样保留：`redshift` 的驱动是 native（本仓没有它的品牌）、
            // `duckdb` / `dynamodb` / `influxdb` / `redis` 各有别的形态。
            Self::Curated(driver) => {
                curated_brand(driver.id).map_or(driver.target, PickerTarget::Brand)
            }
            Self::Agent(entry) => PickerTarget::Agent(entry.key),
            Self::Brand(brand) => PickerTarget::Brand(brand),
        }
    }

    /// 这张卡点下去**能不能真的连上**（墙上要不要弱化它 + 挂「未支持」角标）。
    ///
    /// 判据就是 [`PickerTarget::Unsupported`] 这一档，不另立一个布尔字段：卡片能不能连、
    /// 点下去走哪条路、要不要标未支持，三件事说的是同一件事，分开写迟早分叉。
    /// 走 [`PickerTarget::Page`] 的算支持 —— 它只是**在别处**支持（Redis 有自己的页）。
    fn is_supported(&self) -> bool {
        !matches!(self.target(), PickerTarget::Unsupported)
    }

    /// 命中过滤词（名称 / id / 提示，大小写不敏感）。`query` 由调用点**先小写化**。
    fn matches(&self, query: &str) -> bool {
        match self {
            Self::Curated(driver) => driver_matches_query(driver, query),
            Self::Agent(entry) => {
                query.is_empty()
                    || entry.label.to_lowercase().contains(query)
                    || entry.key.to_lowercase().contains(query)
                    || entry
                        .default_port
                        .is_some_and(|port| port.to_string().contains(query))
            }
            // 品牌卡还要命中原协议名（搜 `mysql` 该搜出 MariaDB 那张卡）。
            Self::Brand(brand) => {
                query.is_empty()
                    || brand.label.to_lowercase().contains(query)
                    || brand.key.to_lowercase().contains(query)
                    || brand.family.to_lowercase().contains(query)
                    || brand.driver.to_lowercase().contains(query)
                    || brand
                        .port
                        .is_some_and(|port| port.to_string().contains(query))
            }
        }
    }
}

/// 手写卡的 id ↔ 品牌表的键：**同一个产品的两种叫法**（`(手写 id, 品牌键)`）。
///
/// 手写卡的 id 是界面习惯（`postgresql`），品牌表的键是 dbx 品牌表里的那一栏
/// （`postgres`）。两者在别处都对得上（`mysql` / `mariadb` / `sqlite` / `oracle` /
/// `mongodb` / `clickhouse` 是同一个词），只有 PostgreSQL 这一对不一致 ——
/// 不加这张表，墙上就会出现两张一模一样的卡片（一张手写、一张品牌），
/// `picker_card_names_are_unique` 就是钉它。
///
/// ⚠️ 加手写卡时如果它的产品在品牌表里另有名字，**来这里补一行**：漏了的症状是
/// 墙上多一张重复卡，而不是编译错误。
const CURATED_BRAND_ALIAS: &[(&str, &str)] = &[("postgresql", "postgres")];

/// 手写卡的 id → 品牌表里的那个牌子（对不上号时 `None`）。
///
/// 手写卡与品牌表是**两个来源**（前者是界面措辞，后者是数据），但两者常常说的是同一个
/// 产品：`mariadb` / `mysql` / `sqlserver` / `sqlite` / `clickhouse` / `oracle` /
/// `mongodb` 两边同名，`postgresql` 在品牌表里叫 `postgres`（见 [`CURATED_BRAND_ALIAS`]）。
/// 认得出来的，手写卡就按品牌卡那条路走 —— 名字用品牌表的中文源串、多模式带出切换器。
///
/// 认不出来的就是**品牌表里没有的那些**：`redshift`（本仓没有它的驱动）、
/// `duckdb` / `dynamodb` / `influxdb`（还不支持）、`redis`（本仓另有专页）。
fn curated_brand(id: &str) -> Option<&'static Brand> {
    brands::brand(id).or_else(|| {
        CURATED_BRAND_ALIAS
            .iter()
            .find(|(card_id, _)| *card_id == id)
            .and_then(|(_, brand_key)| brands::brand(brand_key))
    })
}

/// 选择器要画的**全部**卡片：手写的几张 + 品牌表里该出卡的 + 驱动清单里能接成数据源的。
///
/// 三档各管一段，且**互不重叠**（去重规则见下）：
///
/// 1. **手写卡**（[`PICKER_DRIVERS`]）：内建七档的样板 + 本仓还不支持的那几个目标。
///    措辞与顺序是界面决定，不是数据。
/// 2. **品牌卡**（[`brands`]）：补「同一个协议下的不同牌子」——
///    `mysql` 协议下站着 12 个牌子，档位表达不了，只能一个牌子一张卡。
///    ⚠️ **一族一张**：OceanBase 的 MySQL / Oracle 模式共用一个卡片格，
///    模式由表单里的切换器选（与 dbx 的 `switchOceanbaseMode` 同形）。
/// 3. **agent 卡**（[`duck_drivers::agent_sources`]）：驱动清单里能接成数据源的那些，
///    点进去还要先装驱动。
///
/// ⚠️ **两处必须同源**：第 3 档与驱动管理弹窗（`drivers.rs`）用的是同一个判据，
/// 所以「选择器里点得进去」与「驱动商店里装得出来」一一对应。
///
/// ⚠️ 手写卡的 id 用的是**界面习惯的叫法**，品牌表用的是 **dbx 的品牌键** ——
/// 两边对不上号时同一个产品会被画两张卡。见 [`curated_brand`]。
fn picker_cards() -> Vec<PickerCard> {
    let curated_ids: std::collections::HashSet<&str> =
        PICKER_DRIVERS.iter().map(|driver| driver.id).collect();
    let agent_keys: std::collections::HashSet<&str> = duck_drivers::agent_sources()
        .map(|entry| entry.key)
        .collect();
    // 被手写卡覆盖的**族**：手写卡认得出来的牌子，整族都不再出品牌卡 ——
    // 手写 Oracle 卡覆盖的是整个 Oracle 族（Thin / OCI 由它的切换器给），
    // 手写 PostgreSQL 卡覆盖的是 `postgres` 那个牌子（`postgresql` 的别名）。
    let curated_families: std::collections::HashSet<&str> = PICKER_DRIVERS
        .iter()
        .filter_map(|driver| curated_brand(driver.id))
        .map(|brand| brand.family)
        .collect();

    let mut cards: Vec<PickerCard> = PICKER_DRIVERS.iter().map(PickerCard::Curated).collect();
    // 一族的**代表**是表里第一条（生成时按「缺省模式优先」排过，见 `gen_brands.py`）。
    let mut seen_families: std::collections::HashSet<&str> = std::collections::HashSet::new();
    // 被品牌卡取代的 agent 卡（一族的成员只该出现在族卡的模式切换器里）。
    let mut claimed_drivers: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for brand in brands::all() {
        if !seen_families.insert(brand.family) {
            continue;
        }
        if curated_families.contains(brand.family) {
            continue;
        }
        let via_agent = brand.driver().is_some_and(|key| agent_keys.contains(key));
        let multi_mode = brands::family_of(brand.key).len() > 1;
        // 单模式 + 已有 agent 卡 → 那张卡就是它，不重复画。
        if !multi_mode && via_agent {
            continue;
        }
        // 单模式 + 要驱动但**装不了**（不在数据源清单里）→ 不画：画出来就是
        // 「点得进去、装不出来」。这与 `agent_sources` 的筛选同一条口径。
        if !multi_mode && brand.driver().is_some() {
            continue;
        }
        if multi_mode {
            for member in brands::family_of(brand.key) {
                if let Some(key) = member.driver() {
                    claimed_drivers.insert(key);
                }
            }
        }
        cards.push(PickerCard::Brand(brand));
    }

    for entry in duck_drivers::agent_sources() {
        if curated_ids.contains(entry.key) || claimed_drivers.contains(entry.key) {
            continue;
        }
        cards.push(PickerCard::Agent(entry));
    }
    cards
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

/// 主区展示模式：SQL 结果 / 表浏览 / 表结构 / 实例信息页 / 用户仪表板。
///
/// 不再 `Copy`：`Structure(TableRef)` 带着表引用，快照存取按 `clone()` 走
///（全模式按值使用的只有 `take_editor_state` 那一处）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Mode {
    /// SQL 执行结果。
    #[default]
    Query,
    /// 表浏览（分页）。
    Table,
    /// 表结构编辑器（图形化字段网格；带的是目标表，一个标签一张表）。
    ///
    /// dbx `TableStructureEditor` 对位：点开不进 SQL 编辑器，进一张「字段网格」——
    /// 改名 / 改类型 / 增删列都在网格上完成，SQL 预览随改动实时生成，
    /// 「应用变更」确认后执行。
    Structure(TableRef),
    /// 实例信息页（左栏连接节点下那四个入口，DBFlux 同形）。
    Instance(InstanceTab),
    /// ER 关系图：一个库的表卡片 + 外键关系（dbx 对位；带的是库名 / schema）。
    Er { namespace: String },
    /// 比较数据：源库 vs 目标连接 + 库（规格带在变体里，结果在 `compare` 字段）。
    Compare(CompareSpec),
    /// 数据传输：源库 → 目标连接 + 库（规格带在变体里，结果在 `transfer` 字段）。
    Transfer(TransferSpec),
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

/// 一条**已保存查询**（kv 单键里的一项），也就是「SQL 库」里的一个 `.sql` 文件。
///
/// `id` 与 `name` 分开：名字是给人看的、可以重名、可以改；定位（改名 / 删除 / 元素 id）
/// 一律走 `id`，否则改个名字就会丢掉「正在编辑的是哪一条」这层关系。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedQuery {
    /// 稳定 id（`saved-<递增序号>`，生成时保证不与现有项重复）。
    id: String,
    /// 文件名（用户可改；界面上的「文件名」就是它，默认 `<表名>.sql`）。
    name: String,
    /// 所属文件夹；`None` / 空 = **根文件夹「SQL 库」**（与 dbx 的 `rootFolder` 同口径）。
    ///
    /// `#[serde(default)]`：这一栏是本轮新加的，旧 kv 里的条目没有它 ——
    /// 没有就是根，正好是旧行为（那时根本没有文件夹这一层）。
    #[serde(default)]
    folder: Option<String>,
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

/// 图表类型（对齐 DBFlux：折线 / 柱状 / 面积 / 饼图 / 散点 / 堆叠柱）。
///
/// 前四档由 `gpui-component` 的样式化图表画（`chart::{LineChart, BarChart, AreaChart,
/// PieChart}`），所以只携带「选哪一种」这个标签、没有任何绘制参数。
///
/// **后两档是自己画的**（`charts_plot.rs`）：那边拼不出「只有点没有线」的散点，也没有堆叠
/// 模式，而它们的原语（`canvas` + `PathBuilder` + `paint_quad`）本仓都有 —— 见
/// `docs/database-design.md` §4.16.11。
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
    /// 散点（看分布 / 相关性；一条数值列一张，点不连线）。
    ///
    /// 与折线同一份数据、同一套轴，差别只在**不连线**：线会暗示「两点之间是连续的」，
    /// 而散点要读的是「点在哪儿、有多密」。
    Scatter,
    /// 堆叠柱（看构成；多条数值列**共用一根柱**叠起来）。
    ///
    /// 唯一一档「多列同框」的图：并排柱比的是「谁高」，堆叠柱比的是「合起来多少、各占多少」。
    /// 它因此不走「一列一张」那张小倍数布局（见 `chart_panel` / `stacked_panel`）。
    StackedBar,
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

/// 右键「复制这一行为 …」的**行级 SQL** 三档（dbx `DataGridCopyExtractorId` 里
/// `sql-inserts` / `sql-updates` / `where-clause` 三项的对位）。
///
/// 与 [`ResultFormat`] 分开是刻意的：那边的 `InsertSql` 作用对象是**选区**（勾选行 /
/// 选中行 / 整份结果），这边永远是「**右键指向的那一行**」—— 用户在某一格上右键时，
/// 心里想的是「把这条记录拿走」，不是「把整页结果拿走」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowSqlKind {
    /// `INSERT INTO t (…) VALUES (…)`（整行，含主键）。
    Insert,
    /// `UPDATE t SET … WHERE 主键`（SET 不含主键列）。
    Update,
    /// `WHERE 主键 = …`（只给定位条件，拿去别处核对 / 当删除条件）。
    Where,
}

impl RowSqlKind {
    /// 复制成功后的状态栏回执。
    fn done_note(self, cx: &App) -> SharedString {
        match self {
            Self::Insert => tr(cx, "已复制这一行的 SQL INSERT。").into(),
            Self::Update => tr(cx, "已复制这一行的 SQL UPDATE。").into(),
            Self::Where => tr(cx, "已复制这一行的 WHERE 条件。").into(),
        }
    }

    /// 右键菜单项文案。收进「复制为 ▸」子菜单后与格式档同句式；「（此行）」
    /// 跟选区档的「复制为 INSERT 语句」（勾选行 / 选中行 / 整份结果）区分开。
    fn menu_label(self, cx: &App) -> &'static str {
        match self {
            Self::Insert => tr(cx, "复制为 INSERT 语句（此行）"),
            Self::Update => tr(cx, "复制为 UPDATE 语句（此行）"),
            Self::Where => tr(cx, "复制为 WHERE 条件（此行）"),
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
    /// 当前命名空间里的**索引**（名字 + 建在哪张表上；没有命名空间级索引概念的
    /// 引擎恒空，左栏树据此不画这个分类节点）。
    ///
    /// 与 [`Session::table_indexes`] 那一档不是一回事：这里回答「这个库有哪些索引」，
    /// 那里回答「这张表有哪些索引」。
    indexes: Vec<DatabaseObject>,
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
    /// 这条连接**标识符用什么引号**（`None` = 这条驱动没有引号概念，即「不是 SQL 引擎」）。
    ///
    /// 为什么在会话上存一份而不到处问 `session.conn`：那是 `Arc<Mutex<…>>`，后台线程
    /// 跑长查询时**一直握着它** —— 在 `render` 里 `lock()` 会让整个界面卡到来查询结束。
    /// 而这件事建连时就定死了（agent 档问对端、内建七档看引擎），所以建连时取一次。
    ///
    /// ⚠️ 界面里「这条引擎接不接 SQL」与「拿什么引号拼标识符」**都读这一个字段** ——
    /// 前者看 `is_some()`，后者 `unwrap_or('"')`。别退回 `kind.identifier_quote()`：
    /// 那对 agent 档恒 `None`（它是一整个家族），会把整片 SQL 面一起关掉。
    quote: Option<char>,
    /// 传输通道句柄（有则连接经 `127.0.0.1:{local_port}`；断开随会话释放）。
    transport: Option<Transport>,
    /// 是否处于**显式事务**（用户敲过 BEGIN / START TRANSACTION，还没 COMMIT / ROLLBACK）。
    ///
    /// 判据只看用户发过来的语句文本（[`tx_effect`]）：单条执行路径下连接是常驻的，
    /// 语句之间的事务状态由服务器维持，这里只是把它**显示**出来。
    in_tx: bool,
}

/// 一次内省取回的全部结果（建连 / 刷新 / 切命名空间三处共用同一套取法）。
///
/// 为什么包一个结构：三处都要取「命名空间清单 + 目录 + 触发器 / 事件 / 索引 / 用户」
/// 这七份，每处各自写一遍元组解构，加一份新内省就要追三处（类型、构造、回投）。
///
/// **失败口径**：只有目录（`catalog`）失败会让整次内省失败——它是界面主体；
/// 其余六份全部 `.unwrap_or_default()`：权限不足的账号取不到 `mysql.user`、
/// 老引擎没有 `list_databases`，都不该把整条连接判死（左栏少一个分类节点而已）。
#[derive(Default)]
struct Introspection {
    schemas: Vec<String>,
    databases: Vec<String>,
    catalog: Vec<CatalogTable>,
    triggers: Vec<DatabaseObject>,
    events: Vec<DatabaseObject>,
    indexes: Vec<DatabaseObject>,
    users: Vec<DatabaseObject>,
}

impl Introspection {
    /// 在**已加锁**的连接上按命名空间取齐七份内省结果。
    ///
    /// ⚠️ 索引走 [`Connection::list_indexes`]（**一次**问出整个库的索引，不是逐表问）
    /// —— 几百张表的库逐表 `pragma_index_list` / `information_schema` 就是几百次往返，
    /// 而这一步在三处（建连 / 刷新 / 切库）都会跑。
    fn load(conn: &mut Connection, schema: Option<&str>) -> Result<Self, DbError> {
        Ok(Self {
            // 命名空间清单也跟着重取（刷新要能看见别人新建的库）。
            schemas: conn.list_schemas().unwrap_or_default(),
            databases: conn.list_databases().unwrap_or_default(),
            catalog: conn.catalog(schema)?,
            triggers: conn.list_triggers(schema).unwrap_or_default(),
            events: conn.list_events(schema).unwrap_or_default(),
            indexes: conn.list_indexes(schema).unwrap_or_default(),
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
        session.indexes = self.indexes;
        session.users = self.users;
    }
}

/// 左栏树的一个分类节点（表 / 视图 / 索引 / 存储过程 / 函数 / 触发器 / 事件 /
/// 序列 / 用户与权限）。
///
/// 顺序即展示顺序 —— 与 DBX / DBFlux 的左栏一致（表在最前，权限类在最后）；索引紧跟
/// 视图，因为它是「表这类东西」的伴生对象（存储过程往后的几类都是程序对象，序列是
/// PG / SQL Server 的取号器，跟着事件排在权限前）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeCategory {
    Table,
    View,
    Index,
    Procedure,
    Function,
    Trigger,
    Event,
    Sequence,
    User,
}

impl TreeCategory {
    /// 全部分类，按展示顺序。
    const ALL: [Self; 9] = [
        Self::Table,
        Self::View,
        Self::Index,
        Self::Procedure,
        Self::Function,
        Self::Trigger,
        Self::Event,
        Self::Sequence,
        Self::User,
    ];

    /// 展开状态与元素 id 的稳定键。**不翻译**：它是状态键（存进 `tree_open` 的那一半），
    /// 翻译过的文案换语言就换键，展开状态会跟着丢。
    const fn key(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::Index => "index",
            Self::Procedure => "procedure",
            Self::Function => "function",
            Self::Trigger => "trigger",
            Self::Event => "event",
            Self::Sequence => "sequence",
            Self::User => "user",
        }
    }

    /// 分类标题。
    fn heading(self, cx: &App) -> String {
        match self {
            Self::Table => tr(cx, "表"),
            Self::View => tr(cx, "视图"),
            Self::Index => tr(cx, "索引"),
            Self::Procedure => tr(cx, "存储过程"),
            Self::Function => tr(cx, "函数"),
            Self::Trigger => tr(cx, "触发器"),
            Self::Event => tr(cx, "事件"),
            Self::Sequence => tr(cx, "序列"),
            Self::User => tr(cx, "用户与权限"),
        }
        .to_string()
    }

    /// 分类图标：触发器取闪电、事件取时钟（与 DBX 的取意一致），用户取人形，
    /// 索引取「带序号的清单」（索引就是列的有序清单），序列同取它 —— 与
    /// [`table_kind_icon`] 给序列行的那枚一致（分类图标要跟自己的行同形）。
    const fn icon(self) -> DuckIcon {
        match self {
            Self::Table => DuckIcon::Columns2,
            Self::View => DuckIcon::Layers,
            Self::Index => DuckIcon::ListOrdered,
            Self::Procedure => DuckIcon::SquareTerminal,
            Self::Function => DuckIcon::SquareFunction,
            Self::Trigger => DuckIcon::Zap,
            Self::Event => DuckIcon::Clock,
            Self::Sequence => DuckIcon::ListOrdered,
            Self::User => DuckIcon::User,
        }
    }

    /// 目录条目里的哪一类归到这个分类（索引不是目录条目，恒不匹配）。
    const fn matches(self, kind: TableKind) -> bool {
        matches!(
            (self, kind),
            (Self::Table, TableKind::Table)
                | (Self::View, TableKind::View)
                | (Self::Procedure, TableKind::Procedure)
                | (Self::Function, TableKind::Function)
                | (Self::Sequence, TableKind::Sequence)
        )
    }

    /// 该引擎**有没有这个概念**：有的话清单为空也画分类节点（DBX 同款「视图 0」
    /// —— 数得出「没有」比节点凭空消失更可预期）；没有概念的才不画，免得左栏
    /// 挂一排点开必为空的哑节点。判定与 duck-db 驱动的分派同口径
    /// （`list_events` / `list_users` 等对不支持的引擎恒返回空清单）。
    const fn concept_supported(self, kind: DatabaseKind) -> bool {
        match self {
            Self::Table | Self::View => true,
            // ⚠️ agent 档在这三档上返回 `false`，但**不是**「那些引擎没有索引 / 触发器」——
            // Oracle / 达梦 / DB2 都有。返回 `false` 是因为**协议只有「按表看」那一形态**
            // （`list_indexes` / `list_triggers` 是表级方法），而我们画的是**库级**分类节点，
            // 聚合不出清单来。要打开它得在 `catalog` 里逐表聚合（连接时几十张表各来一次），
            // 那是另一件事；现在宁可少画一个节点，也不给一个点开必为空的哑节点。
            Self::Index | Self::Trigger => matches!(
                kind,
                DatabaseKind::Sqlite
                    | DatabaseKind::Postgres
                    | DatabaseKind::Mysql
                    | DatabaseKind::SqlServer
            ),
            // agent 档的目录是 `list_tables` + `list_objects` 两路取的，后一路正好给
            // 过程 / 函数 / 序列，所以这三档**有清单可画**（不像索引那样聚合不出来）。
            Self::Procedure | Self::Function => matches!(
                kind,
                DatabaseKind::Postgres
                    | DatabaseKind::Mysql
                    | DatabaseKind::SqlServer
                    | DatabaseKind::Agent
            ),
            // 事件是 MySQL 独有的调度器对象，别家（含 agent 那一族）没有这个统一概念。
            Self::Event => matches!(kind, DatabaseKind::Mysql),
            // 序列：PostgreSQL 与 SQL Server 有（后者走 `sys.sequences`）；
            // MySQL / SQLite 用 AUTO_INCREMENT / rowid 顶替，没有独立对象。
            // agent 档跟着 `list_objects` 走（Oracle `SEQUENCE`、达梦 `序列` 都报得出来）。
            Self::Sequence => matches!(
                kind,
                DatabaseKind::Postgres | DatabaseKind::SqlServer | DatabaseKind::Agent
            ),
            Self::User => matches!(
                kind,
                DatabaseKind::Postgres | DatabaseKind::Mysql | DatabaseKind::SqlServer
            ),
        }
    }
}

impl Session {
    /// 某个分类对应的**库级对象**清单；目录类分类（表 / 视图 / 过程 / 函数 / 序列）
    /// 给空。
    fn database_objects(&self, category: TreeCategory) -> &[DatabaseObject] {
        match category {
            TreeCategory::Trigger => &self.triggers,
            TreeCategory::Event => &self.events,
            TreeCategory::Index => &self.indexes,
            TreeCategory::User => &self.users,
            TreeCategory::Table
            | TreeCategory::View
            | TreeCategory::Procedure
            | TreeCategory::Function
            | TreeCategory::Sequence => &[],
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
/// 连接级查询标签之外，左栏点表 / 视图会开**数据标签**、点数据库节点会开**库标签**、
/// 点实例四档（仪表盘 / 实例概览 / 实例指标 / 实例检查器）会开**实例标签**、点用户
/// 仪表板会开**仪表板标签**（对齐 dbx 的 dataTabOpenPolicy：同对象复用并激活，
/// 同连接同身份只留一张；⌥ 点击对数据标签强制新开；「永远新开 / 活动标签复用」
/// 两档不进口径）。身份在打开时定死、关闭时随标签移除，见
/// [`DatabasePage::tab_kind`]。
#[derive(Debug, Clone, PartialEq, Eq)]
enum TabKind {
    /// 连接级查询标签（工具条「＋」与 [`DatabasePage::activate_conn`] 造的都是这种）。
    Query,
    /// 库标签：一个库（schema 层引擎里就是 schema）一个，标题 = 库名。
    Database { name: String },
    /// 数据标签：一张表 / 一个视图一个，标题 = `表名@库`（无 schema 时裸名）。
    Data { object: TableRef },
    /// 结构标签：一张表一个（同连接同表复用），标题 = `结构 表名@库`。
    Structure { object: TableRef },
    /// 新建表标签：dbx `openTableStructure(…, "")` 对位 —— 不弹窗，直接进结构
    /// 编辑器的**建表模式**（表名 / 注释 / 字段网格都现场填）。**永不复用**，
    /// 每点一次新开一个；应用成功后原地转正成 [`TabKind::Structure`]。
    NewTable { schema: Option<String> },
    /// 实例标签：仪表盘 / 实例概览 / 实例指标 / 实例检查器各一个（同连接同档复用）。
    ///
    /// 从「页面级 `Mode` 直切」迁到标签层：直切会把当前标签正在看的内容整个盖掉，
    /// 也回不去 —— 标签化之后每个实例页各自持快照（`mode` + `instance_data` 都在
    /// [`SessionView`] 里），切回查询标签原样恢复。
    Instance(InstanceTab),
    /// ER 关系图标签：一个库一张（同连接同库复用），标题 = `ER 库名`。
    Er { namespace: String },
    /// 比较数据标签：一对规格一张（同规格复用），标题 = `比较 源→目标`。
    Compare(CompareSpec),
    /// 数据传输标签：一对规格一张（同规格复用），标题 = `传输 源→目标`。
    Transfer(TransferSpec),
    /// 仪表板标签：一块用户自建仪表板一个（同连接同板复用），标题 = 板名。
    Dashboard(u32),
}

/// 比较数据的两侧规格：源 / 目标各是「连接 + 命名空间」。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompareSpec {
    /// 源连接（打开菜单所在的连接）。
    source_conn: String,
    /// 源命名空间。
    source_ns: String,
    /// 目标连接（页面上选，默认另一条已连接会话）。
    target_conn: String,
    /// 目标命名空间（页面上选）。
    target_ns: String,
    /// 只比较这些表（表节点右键「比较数据」带单张；**空 = 全库**，库级入口
    /// 与工具条按钮都不带 —— 与 `TransferSpec::tables` 同一条口径）。
    tables: Vec<TableRef>,
}

/// 数据传输的规格：源连接 + 源命名空间的若干表 → 目标连接 + 目标命名空间。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TransferSpec {
    /// 源连接。
    source_conn: String,
    /// 源命名空间。
    source_ns: String,
    /// 要搬的表（打开时按源目录定死；比较页的「传输此表」传单张）。
    tables: Vec<TableRef>,
    /// 目标连接（页面上选）。
    target_conn: String,
    /// 目标命名空间（页面上选）。
    target_ns: String,
}

/// 结果区视图三档（对齐 dbflux 的「网格 / 记录 / JSON」）。
///
/// 三档看的是**同一份结果的同一行**（有选中行用选中行，没有用首行）：
/// 网格 = 常规网格（默认）；记录 = 把那一行转置成「名称 / 值 / 类型」三列；
/// JSON = 那一行序列化成对象。落点在**结果区脚注最前面的切换**（第四档「图表」
/// 不在本枚举里 —— 它切换的是结果区顶部 tab，见 `DatabasePage::result_view_switch`），
/// 不占右键菜单。与 `chart_view` 是两码事：那是「换一种画法」，这是「换一种读一行数据的方式」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ResultViewMode {
    /// 常规网格（默认）：切换器上叫「网格」。
    #[default]
    Grid,
    /// 转置视图：一列一行，附引擎报的类型名。切换器上叫「记录」。
    Columns,
    /// 单行 JSON 预览。
    Json,
}

/// 结果区顶部 tab（dbx 的 `QueryResultViewSwitcher` 同款五档）。
///
/// 「数据表」是网格本体的家；摘要 / 图表 / 消息 / 执行计划各自换掉网格内容，
/// 头条与读数条不动。切 tab 只改**看的方式**，不清结果、不动分页与暂存。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ResultAreaTab {
    /// 数据表（默认）：网格 / 记录 / JSON 三档（[`ResultViewMode`]）在这里生效。脚注
    /// 切换器的第四档「图表」会切到下面的 [`ResultAreaTab::Chart`]（本枚举里没有图表档）。
    #[default]
    Data,
    /// 摘要：结果集的统计概览（行列数、耗时、逐列类型与空值计数）。
    Summary,
    /// 图表：同一份结果的画法（`chart_view` 配置）。
    Chart,
    /// 消息：本次标签的执行流水（成功 / 失败 / 耗时）。
    Messages,
    /// 执行计划：`plan_view` 的计划树（没生成过计划时给指引空态）。
    Plan,
}

/// 「消息」tab 里的**一条执行流水**（dbx 的 messages 同款最小集）。
#[derive(Debug, Clone)]
struct ExecLogEntry {
    /// `true` = 成功；`false` = 失败 / 取消（决定行首图标与配色）。
    ok: bool,
    /// 单行化后的语句（长语句截断显示，完整原文在编辑器 / 历史里）。
    label: String,
    /// 结果读数：行数 / 影响行数，或错误文本。
    detail: String,
    /// 耗时（毫秒）。
    elapsed_ms: u128,
}

// 列宽拖拽的载荷（原先的 `ColResizeDrag`）已随行模式网格一起搬进公共数据表组件
//（`duck_components::data_table::table`）——列宽与它的拖拽态现在都是组件的内部状态，
// 页面不再需要这个类型，也不再需要 `col_resize` 字段。

/// 右栏 dock 的分段（设计稿 `docs/db-ui-mockup.html` 的 `docktabs`）。
///
/// 五段 —— **详情**（连接信息 / 命名空间 / DDL / 表元信息 / 选中行）、**历史**、
/// **单元格**（网格里点 ⓘ 的只读档案）、**构建器**（可视化 SQL 构建器）与
/// **AI 助手**（与模型的一轮对话）。
///
/// 页面级而非按标签存：右栏是 shell 级、一条路由一个（`with_right_sidebar`），
/// 用户选的是「我现在想看哪一栏」，切标签不该把这个选择重置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RightDock {
    /// 详情（默认）：当前连接 / 选中对象 / 选中行的只读档案。
    #[default]
    Detail,
    /// 历史：最近执行的语句与已保存查询，点击**填编辑器不执行**。
    History,
    /// 单元格详情：网格里点 ⓘ 后的只读档案（dbx「单元格详情」对位）。
    Cell,
    /// 构建器：可视化拼 SQL（**不占结果区**，构建时原结果照看在眼里）。
    Builder,
    /// AI 助手：与模型的一轮对话（dbx `AiAssistant` 对位）。
    ///
    /// 曾经是一个「独立右滑浮层」（整块盖在编辑器上方，由 `ai_open` 控制），现在
    /// 并进 dock 分段：助手要能**回看**「刚才问过什么、它答了什么、产出落到哪儿了」，
    /// 一条贴在编辑器上的窄条放不下对话，还会把编辑器挤走；右栏本来就是「与当前
    /// 对象相关的另一块屏」。
    Ai,
}

/// AI 对话里一条消息的**说话人**（决定气泡靠哪边、用哪个底色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AiRole {
    /// 用户：右对齐、强调薄底。
    User,
    /// 助手：左对齐、卡片底。
    Assistant,
}

/// AI 助手对话里的一条消息。
///
/// 只存**看得见的东西**（谁说的 + 正文 + 一行补充说明），不存模型原始响应：
/// 这里是一份对话记录，不是一份 API 日志；要复现问题看 `tracing` 里的
/// `ai executed readonly sql` 那条（带 sql 与 tokens）。
#[derive(Debug, Clone)]
struct AiMessage {
    /// 说话人。
    role: AiRole,
    /// 正文（用户问句 / 助手产出的 SQL / 解释文本）。
    text: String,
    /// 补充说明：tokens 读数与「产出落到哪儿了」（已自动执行 / 已填入编辑器）。
    /// 用户消息恒为 `None` —— 用户自己知道他说了什么。
    note: Option<SharedString>,
}

/// 右栏「历史」分段里的两个数据源。
///
/// 与左栏「已保存查询」共用同一份 kv（[`KEY_SAVED_QUERIES`]），这里不再另存一份 ——
/// 同一个东西在两处各画一份，改一处忘一处就是两套行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum HistorySegment {
    /// 最近执行过的语句（kv `db_query_history`，自动攒、有上限）。
    #[default]
    Recent,
    /// 用户显式保存的查询（kv `db.saved_queries`）。
    Saved,
}

/// 语句槽（编辑器左侧逐条 ▶ 那一列）的执行状态标记（设计稿 gutter marker）。
///
/// 不在 `stmt_states` 里 = 还没跑过 = 画 ▶（可点）。跑过之后落在这里：
/// 设计稿同款 —— 运行中转圈、成功绿点、失败红点，跑没跑过一眼可辨。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StmtRunState {
    /// 已发起还没回来（工具条同一时刻只放行一条，所以最多一个 Running）。
    Running,
    /// 执行成功。
    Ok,
    /// 执行失败（错误条与状态栏有详情）。
    Failed,
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
    /// 实例指标页**当前聚焦**的指标（左栏指标子项点进来的高亮，按标签存）。
    focused_metric: Option<String>,
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
    /// 表结构编辑器的草稿（`Mode::Structure` 的数据源；`None` = 还没取回列清单）。
    ///
    /// 按标签存：两个结构标签各改各的，切回来草稿原样。
    structure: Option<StructureDraft>,
    /// ER 关系图的数据（`Mode::Er` 的数据源；`None` = 还没取回）。
    ///
    /// 按标签存：两个 ER 标签各看各的库，切回来原样。
    er: Option<ErData>,
    /// 比较数据的状态（`Mode::Compare` 的数据源；`None` = 还没开跑）。
    compare: Option<CompareState>,
    /// 数据传输的状态（`Mode::Transfer` 的数据源；`None` = 还没开跑）。
    transfer: Option<TransferState>,
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
    /// 查询结果**客户端分页**的当前页（0 起；`Mode::Query` 的网格按它切片）。
    query_page: usize,
    /// 查询结果的**客户端排序**（`None` = 原始返回顺序）。
    ///
    /// 与表浏览的 `table_sort` 是两套，别混：那边是**服务器侧** `ORDER BY`（重取一页，
    /// 数据本身换了序），这边结果已整批在手，排的是**展示顺序**（同「结果内搜索」
    /// 「客户端分页」一层语义 —— 导出 / 编辑 / 多选仍按原始行集走）。
    query_sort: Option<SortSpec>,
    /// 结果标签（一条语句一个）；`query_result` 恒等于激活标签的视图。
    result_tabs: Vec<ResultTab>,
    /// 激活的结果标签下标。
    active_result_tab: usize,
    /// 执行计划态（结果区改显示计划树）；`None` = 不是在看计划。
    plan_view: Option<PlanView>,
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
    /// 结果区视图档位（行 / 列 / JSON 三选一，见 [`ResultViewMode`]）。
    ///
    /// 与 `chart_view` 并存且互不覆盖：切到图表看画法，切回表格仍停在原来的行 / 列 / JSON 档。
    result_view: ResultViewMode,
    /// 结果区顶部 tab（数据表 / 摘要 / 图表 / 消息 / 执行计划，见 [`ResultAreaTab`]）。
    result_area_tab: ResultAreaTab,
    /// 「消息」tab 的执行流水（每标签一份；新条目在尾部，超过 [`EXEC_LOG_CAP`] 裁头部）。
    exec_log: Vec<ExecLogEntry>,
    /// 结果网格里**被隐藏**的列名（「字段筛选」勾掉的那些列）。
    ///
    /// 记**列名**而不是下标：下标在换结果集后必然错位，而同名列跨语句保持隐藏
    /// 更符合直觉（`SELECT * FROM t` 换张表，藏掉的 `id` 应当继续藏着）。
    /// 代价是 `SELECT a, a` 这种同名列会一起隐藏——已知并接受。
    ///
    /// 只影响**显示**：导出 / 复制 / 编辑仍按完整列集走。隐藏是「我不想看」，
    /// 不是「这列不存在」。
    hidden_columns: BTreeSet<String>,
}

/// 单元格详情的**值编辑器**（右栏「单元格」分段）。
///
/// 一框一开关一件事：`cell` 是编辑本体（列名 / 原值 / NULL 位），`row` / `col` 记住
/// 它属于哪一格 —— 指纹比对用（换格 / 原值变了才重建输入框，用户正在敲的字不被
/// 下一帧抹掉）。可编辑性与行编辑器同一道闸（[`DatabasePage::edit_source`]）。
struct CellDetailEditor {
    /// 指向的行（`cell_detail` 的第一维）。
    row: usize,
    /// 指向的列（`cell_detail` 的第二维）。
    col: usize,
    /// 编辑本体。
    cell: RowCell,
}

/// 表结构编辑器里**一列**的草稿（`Mode::Structure` 的行）。
struct StructureColumnDraft {
    /// 原列名；`None` = 本轮新增（库里还没有这一列，SQL 预览走 `ADD COLUMN`）。
    original: Option<String>,
    /// 打开时内核报的**基础类型名**（判「类型改没改」的基准；新增列为空串）。
    /// 长度已拆到 [`Self::original_length`]：`varchar(50)` → `varchar` + `50`。
    original_type: String,
    /// 打开时内核报的**长度修饰**（`50` / `14,3`；空串 = 无长度）。PG 的
    /// `numeric(10,2)` 与 MySQL 的 `decimal(14,3)` 都落在这一格，原样回填。
    original_length: String,
    /// 打开时读到的可空（新增列为 `true`；内核拿不到的引擎按打开值原样保留——
    /// 不改就不生成语句，改了由「该属性对引擎是否可编辑」的闸门拦住）。
    original_nullable: bool,
    /// 字段名输入框（受控显示：当前生效的列名在这份草稿里，框只是显示）。
    name: Entity<InputState>,
    /// 类型输入框（**基础类型名**：`bigint` / `varchar`；长度在 [`Self::length`]）。
    /// 与类型下拉共用：下拉只往这个框里写候选值，框仍可自由输入任意类型名
    /// （PG 的 `timestamptz` / `uuid`、SQLite 的 `INTEGER` 都塞不进固定候选表）。
    kind: Entity<InputState>,
    /// 长度输入框（`50` / `14,3`；空 = 不带括号）。
    length: Entity<InputState>,
    /// 可空勾选（新增列默认可空；主键勾上时联动关掉）。
    nullable: bool,
    /// 打开时读到的默认值（空串 = 无默认值；新增列为空串）。
    original_default: String,
    /// 默认值输入框（`CURRENT_TIMESTAMP` / `0` / `'x'` 整段文本）。
    default: Entity<InputState>,
    /// 打开时读到的注释（空串 = 无注释）。
    original_comment: String,
    /// 注释输入框。
    comment: Entity<InputState>,
    /// 打开时读到的字符集（空串 = 表默认；只有 MySQL 有）。
    original_charset: String,
    /// 字符集输入框。
    charset: Entity<InputState>,
    /// 打开时读到的排序规则（空串 = 字符集默认）。
    original_collation: String,
    /// 排序规则输入框。
    collation: Entity<InputState>,
    /// 是不是主键成员（打开时从内核读的真值；MySQL 上可改，其余引擎只展示）。
    primary: bool,
    /// 本轮标记删除：行淡化、操作位给「撤销删除」，SQL 预览出 `DROP COLUMN`。
    dropped: bool,
}

/// 字段网格的列数（序号+操作 / 字段名 / 类型 / 长度 / 可为空 / 主键 / 默认值 /
/// 注释 / 字符集 / 排序规则）。
pub(super) const STRUCTURE_COLUMNS: usize = 10;

/// 字段网格十列的默认宽度（对齐 dbx `TableStructureEditor` 的 `th` 宽度；注释列
/// 吃掉余量，故与其余九列之和正好凑够 `min-width: 1360px`）。列宽可拖拽，
/// 拖出来的宽度存在 [`StructureDraft::column_widths`]。
pub(super) const STRUCTURE_DEFAULT_WIDTHS: [f32; STRUCTURE_COLUMNS] = [
    120.0, 160.0, 140.0, 90.0, 64.0, 64.0, 130.0, 372.0, 100.0, 120.0,
];

/// 表结构编辑器里**一个索引**的草稿。
///
/// 内联编辑：名称 / 唯一 / 组成列都能改，改动在「应用变更」时生成
/// `CREATE INDEX` / `DROP INDEX`（重建 = 先 DROP 再 CREATE）。
/// `system` 是引擎自建的索引（MySQL 的 `PRIMARY`、SQLite 的 `sqlite_autoindex_*`）：
/// 名称与列都不可改，只能看——删了它等于删主键。
struct StructureIndexDraft {
    /// 原索引名；`None` = 本轮新增。
    original: Option<String>,
    /// 打开时的组成列（判「改没改」的基准）。
    original_columns: Vec<String>,
    /// 打开时的唯一性（判「改没改」的基准）。
    original_unique: bool,
    /// 引擎自建（主键索引 / SQLite 的 `sqlite_autoindex_*` 约束隐式索引）：
    /// 名称与列只读——删它等于删主键 / 约束。
    system: bool,
    /// 索引名输入框。
    name: Entity<InputState>,
    /// 唯一索引开关。
    unique: bool,
    /// 组成列（勾选顺序即索引内列序）。
    columns: Vec<String>,
    /// 「重建」已排入本次变更：即使一个字没改也要出 `DROP INDEX` + `CREATE INDEX`
    /// （重建的真实用途是刷碎片 / 换存储参数）。
    rebuild: bool,
    /// 本轮标记删除：生成 `DROP INDEX`。
    dropped: bool,
}

/// 表结构编辑器里**一条外键**的草稿（内联编辑：本表列 / 引用表 / 引用列 /
/// 更新删除动作）。
struct StructureForeignKeyDraft {
    /// 原约束名；`None` = 本轮新增。SQLite 没有约束名，落空串。
    original: Option<String>,
    /// 打开时的本表列（判「改没改」的基准）。
    original_columns: Vec<String>,
    /// 打开时的被引用表。
    original_ref_table: String,
    /// 打开时的被引用列。
    original_ref_columns: Vec<String>,
    /// 打开时的更新动作。
    original_on_update: String,
    /// 打开时的删除动作。
    original_on_delete: String,
    /// 约束名输入框（MySQL / PG / T-SQL 的 `ADD CONSTRAINT <name>` 用；留空则由
    /// 引擎自动命名）。
    name: Entity<InputState>,
    /// 本表引用列（勾选顺序即声明顺序）。
    columns: Vec<String>,
    /// 被引用的表（`schema.table` 文本）。
    ref_table: String,
    /// 被引用的列（与 [`Self::columns`] 按序对齐——复合外键要求两边列数一致，
    /// 生成前校验）。
    ref_columns: Vec<String>,
    /// 更新动作（`NO ACTION` / `CASCADE` / `SET NULL` / `RESTRICT` / `SET DEFAULT`）。
    on_update: String,
    /// 删除动作（同上）。
    on_delete: String,
    /// 本轮标记删除：生成 `DROP CONSTRAINT`。
    dropped: bool,
}

/// 表结构编辑器里**一条新增触发器**的草稿。
///
/// 只有**新增**的可编辑（已有触发器只读清单，见 [`StructureDraft::triggers`]）：
/// 生成 `CREATE TRIGGER` 是单向动作，改 / 删已有触发器要按引擎补齐整套
/// `DROP TRIGGER` / `CREATE OR REPLACE` 口径，没这部分能力就不冒充。
struct StructureTriggerDraft {
    /// 触发器名输入框。
    name: Entity<InputState>,
    /// 时机（`BEFORE` / `AFTER`）。
    timing: &'static str,
    /// 事件（`INSERT` / `UPDATE` / `DELETE`）。
    event: &'static str,
    /// 触发体：MySQL / SQLite 填内联 SQL 语句（`FOR EACH ROW` 后跟的语句）；
    /// PG 填函数名（生成时补 `EXECUTE FUNCTION …()`）。
    statement: Entity<InputState>,
}

/// 「字段注释放大编辑」弹窗的草稿（一次编辑一行；`row` 指向字段草稿的下标）。
struct StructureCommentDraft {
    /// 目标字段行。
    row: usize,
    /// 弹窗里显示的目标字段名（打开时快照，字段名改了也不影响它——只是标题）。
    field: String,
    /// 多行注释输入框。
    text: Entity<TextareaState>,
}

/// 结构编辑器网格的行密度（dbx 同款三档；页内记忆，刷新 / 换表回落「标准」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructureDensity {
    Compact,
    Standard,
    Comfortable,
}

/// 「复制其他表字段」弹窗的草稿：源表名 + 候选字段（勾选态在本地）。
/// `Clone`：弹窗内容闭包里要从页面状态快照一份去渲染。
#[derive(Clone)]
struct CopyFieldsDraft {
    /// 源表（限定名文本，仅展示）。
    table: String,
    /// 候选字段（顺序即源表列序；`selected` 初始全勾——复制本就为了要它们）。
    fields: Vec<(String, String, bool)>,
}

/// 表结构编辑器的标签（dbx `TableStructureEditor` 的 Tabs 对位）。
///
/// 按内核内省能力只开五个：约束、分区内核没有对应读取，**不冒充**（dbx 同款：
/// 能力之外的标签直接不显示）。字段 / 索引 / 外键三个标签都能编辑（各自生成
/// 引擎支持的 DDL）；DDL 与触发器只读——触发器没有生成 DDL 的能力，
/// 摆一个改不动的编辑器只会让人误会。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructureTab {
    /// 只读展示打开时读到的建表 DDL + 复制入口。
    Ddl,
    /// 字段网格（本编辑器的主编辑面：改名 / 改类型 / 增删列）。
    Columns,
    /// 索引卡片（内联编辑 + 新增 / 重建 / 删除）。
    Indexes,
    /// 外键卡片（内联编辑 + 新增 / 删除）。
    ForeignKeys,
    /// 只读触发器清单。
    Triggers,
}

/// 表结构编辑器的整份草稿（一个标签一张表）。
struct StructureDraft {
    /// 目标表。
    table: TableRef,
    /// **建表模式**（`TabKind::NewTable` 的标签）：服务器上还没有这张表——
    /// 表名现场填（[`Self::new_table_name`]），字段从零加，「应用变更」生成
    /// `CREATE TABLE` 而不是 `ALTER`（dbx 的 `isCreateMode` 对位）。
    is_create: bool,
    /// 建表模式的表名输入框（编辑模式没有 —— 表名是打开时定死的）。
    new_table_name: Option<Entity<InputState>>,
    /// 当前激活的标签（打开 / 刷新时回落「字段」；页内记忆，切标签不丢）。
    active_tab: StructureTab,
    /// 字段搜索框（客户端过滤字段名 / 类型；`Change` 订阅在
    /// [`DatabasePage::_subscriptions`]，见 `build_structure_draft`）。
    search: Entity<InputState>,
    /// 打开时读到的建表 DDL（判「DDL 页编辑过没有」的基准；读取失败落 `None`，
    /// 只影响 DDL 标签显示空态，不拖垮编辑器）。
    ddl: Option<String>,
    /// DDL 标签的可编辑 SQL 编辑器（高亮 + 行号，见 `build_structure_draft`）；
    /// 打开时用 [`Self::ddl`] 播种。`None` = 引擎给不出 DDL → 标签显示空态。
    ddl_editor: Option<Entity<EditorState>>,
    /// 表注释输入框 / 打开时读到的表注释（`None` = 该引擎拿不到 → 表级属性行不显示）。
    /// 只有 MySQL 的 `SHOW CREATE TABLE` 尾部带 `COMMENT='…'`，从 DDL 里解析。
    table_comment: Option<Entity<InputState>>,
    original_table_comment: Option<String>,
    /// 存储引擎（打开时读到的值 + 下拉候选；`None` = 不是 MySQL → 不显示这一行）。
    table_engine: Option<String>,
    original_table_engine: Option<String>,
    engine_options: Vec<String>,
    /// 索引 / 外键草稿（可编辑；当前引擎生成不了 DDL 时落空清单，对应标签
    /// 只显示口径提示，不摆改不动的编辑器）。
    indexes: Vec<StructureIndexDraft>,
    foreign_keys: Vec<StructureForeignKeyDraft>,
    /// 索引 / 外键对该引擎可不可生成 DDL（闸门见 `structure_capabilities`）。
    index_editable: bool,
    foreign_key_editable: bool,
    /// 引用表 → 列候选缓存（选「引用表」时后台取一次该表列清单；键为表名）。
    ref_columns: BTreeMap<String, Vec<String>>,
    /// 字段网格十列的当前宽度（列宽可拖拽；刷新 / 换表回落默认）。
    column_widths: [Pixels; STRUCTURE_COLUMNS],
    /// SQL 预览卡是否折叠（dbx 的 `▴ / ▾`，页内记忆）。
    preview_collapsed: bool,
    /// SQL 预览正文的编辑器（**高亮 + 可编辑**）：内容 = 由草稿实时生成的语句，
    /// 或用户手改后的文本。只在有 `window` 的地方建（打开草稿时），与 DDL 页
    /// 的 `ddl_editor` 同一套组件。
    preview_editor: Option<Entity<EditorState>>,
    /// 预览正文是否处于**编辑态**（`Editor::readonly(false)`）。非编辑态下
    /// 生成文本变了就同步进来，让预览始终是草稿的投影。
    preview_editing: bool,
    /// 预览文本被**手改**过：此后「应用变更」/「复制 SQL」都用编辑器里的这段，
    /// 不再用生成结果（网格再改动不会覆盖手改 —— 要回到生成态按「重新生成」）。
    preview_manual: bool,
    /// 已有触发器清单（**只读**：改 / 删已有触发器要按引擎补齐 DROP / REPLACE
    /// 口径，没有就不冒充；新增走 [`Self::new_triggers`]）。
    triggers: Vec<TableTrigger>,
    /// 本轮新增的触发器草稿（可编辑；应用时生成 `CREATE TRIGGER`，引擎可不可
    /// 生成见 [`Self::trigger_editable`]）。
    new_triggers: Vec<StructureTriggerDraft>,
    /// 触发器可不可新增：MySQL / SQLite 内联语句、PG 函数名都能生成
    /// `CREATE TRIGGER`；其余引擎（T-SQL 等方言差异大）不给入口，不冒充。
    trigger_editable: bool,
    /// 「复制其他表字段」的来源表清单（打开时的库内表快照，不含当前表；按名字排序）。
    source_tables: Vec<String>,
    /// 打开时的原列序（仅原列名；判「顺序变没变」——MySQL 生成物理重排 `AFTER`，
    /// 其余引擎只在本地显示序里生效）。
    original_order: Vec<String>,
    /// 打开时的主键列（判「主键变没变」；MySQL 才生成 `ADD/DROP PRIMARY KEY`）。
    original_primary: Vec<String>,
    /// 主键可不可改：只有 MySQL 生成得了主键 DDL；其余引擎改主键 = 重建表，
    /// 勾选框禁用并给出口径提示，不冒充。
    primary_editable: bool,
    /// 列级元数据哪个引擎拿不到哪些属性——按 [`DatabaseKind`] 现查（见
    /// `StructureMeta::supports`），不逐列存。
    kind: DatabaseKind,
    /// 「复制其他表字段」弹窗的草稿（`None` = 没开）；挂在页面（[`DatabasePage`]）
    /// 而不是这份草稿上：弹窗确认时目标列要追加进 `columns`，生命周期跟着弹窗走。
    /// ——— 放在 [`DatabasePage::copy_fields`]。
    /// 网格行密度（dbx 同款三档；刷新 / 换表回落「标准」）。
    density: StructureDensity,
    /// 列草稿（顺序即表列序；新增列追加尾部）。
    columns: Vec<StructureColumnDraft>,
}

/// 列头下拉「列格式化」的格式档（挂在 [`DatabasePage::column_formats`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ColumnFormat {
    /// 数值千分位：`1234567` 显示成 `1,234,567`。只对数值列的**显示**生效，
    /// 复制 / 编辑 / SQL 生成的仍是原值。
    Thousands,
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

/// 左栏树里**一张表**展开后的四份子清单（列 / 索引 / 外键 / 触发器）。
///
/// 与 [`TableMeta`] 不是一份状态：那是**表页**的元信息（跟着选中表走、翻页不重取），
/// 这是**树上每个表节点**自己的子级（一个库几百张表，只在用户展开节点时才取一次，
/// 取完缓存住 —— 再展开直接命中，不重复打服务器）。四个槽位互相独立兜底，
/// 某个内省失败落空清单，不拖垮其余三份。
#[derive(Default)]
struct TableTreeMeta {
    columns: Vec<Column>,
    indexes: Vec<TableIndex>,
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

/// 正在跑的「执行 SQL 文件」任务（见 [`DatabasePage::sql_file`]）。
///
/// 三样跨线程的东西都是 `Arc` 包裹的原子量 / 互斥量，而不是页面字段 —— 页面拿不到
/// 后台线程手里的 `Context`，只能靠它们通信。
struct SqlFileJob {
    /// 脚本文件名（状态栏、进度条与执行流水都用它）。
    name: SharedString,
    /// 停止标志：`should_stop` 谓词读它，工具条「停止」置它。
    ///
    /// 与 `running.cancel` 的分工：那一枪打断**正在跑的那条语句**，这个标志决定
    /// **后面还跑不跑**（`execute_batch` 每条发起前问一次）。两个都置才是完整的
    /// 「停止」。
    stop: Arc<AtomicBool>,
    /// 最近一次读回的进度（**渲染只读它**，不在 `render` 里碰锁）。
    ///
    /// 进度快照本体（`Arc<Mutex<..>>`）不在这里：它只被轮询任务与后台线程两端持有
    /// （各自一份 `Arc`），页面从不读它 —— 页面读的是这份已经搬回主线程的拷贝。
    now: FileProgress,
}

/// SQL 执行结果的展示态（跑一次存起来，渲染绝不现算）。
#[derive(Clone, Default)]
struct QueryView {
    /// 结果集（整段都是写语句时为 `None`）。
    result: Option<ResultSet>,
}

/// 结果区的一个**结果标签**（一条语句一个，dbx 同款）。
///
/// 标签持有自己的视图与可编辑目标；页面级的 `query_result` 恒等于**激活标签**
/// 的视图 —— 既有渲染路径（网格 / 导出 / 编辑）不用感知标签的存在，切标签
/// 就是把标签里的东西装回去。
#[derive(Default)]
struct ResultTab {
    /// 产出这份结果的语句（读数条展示；将来「按标签重跑」也用它）。
    sql: String,
    /// 结果展示态。
    view: QueryView,
    /// 可编辑目标表（`None` = 不可编辑）。
    target: Option<TableRef>,
    /// 目标表主键。
    pk: Vec<String>,
    /// 这次执行的耗时（毫秒；footer 读数）。
    elapsed_ms: Option<u128>,
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

/// JSON 档（[`ResultViewMode::Json`]）的**行 JSON 编辑器**（Redis 值编辑器同款）。
///
/// 一份带语法高亮（tree-sitter json）的代码编辑器，内容是当前行的美化 JSON——
/// 直接在里面改，改动经「暂存更改」进与网格 / 右栏**同一条** `pending_edits`
/// 暂存路径（[`DatabasePage::stage_json_edits`]）。
///
/// 由渲染期同步（[`DatabasePage::sync_json_editor`]）维护：构造 `EditorState`
/// 需要 `&mut Window`，而渲染是唯一拿得到它的时机。
struct JsonEditor {
    /// 这份编辑器服务的那一行（当前结果集内的下标）。
    row: usize,
    /// 那一行的指纹（列名 + 展示原值 + NULL 位，见 [`specs_stamp`]）。对不上就整份
    /// 重建 —— 结果重取 / 翻页 / 换表之后「同一下标」已经是另一行数据了。
    stamp: u64,
    /// 这一行能不能改（单表 + 主键齐全）：不能改时编辑器只读，照常选中复制。
    editable: bool,
    /// 每列的「展示原值 + NULL 位」（[`row_editor_specs`] 口径）：暂存时拿编辑器里
    /// 解析出的 JSON 值与它逐列 diff，只暂存**真的变了**的列。藏列不在编辑器
    /// 文本里，天然不参与 diff。
    specs: Vec<(String, String, bool)>,
    /// JSON 代码编辑器（行号 + 软换行 + 高亮）。
    input: Entity<EditorState>,
}

/// 单元格就地编辑器（双击进入）：一行输入框，回车提交。
///
/// 提交走与行编辑器**同一条**暂存路径（`pending_edits` 按 (主键, 列) 去重、
/// 「提交」时一个事务写下去），只是入口从弹窗换成了网格内。
struct CellEditor {
    /// 行下标（当前结果集内）。
    row: usize,
    /// 列下标（原始列序）。
    col: usize,
    /// 进编辑器时的展示原值（比对判定「改没改」）。
    original: String,
    /// 这一格是不是 `NULL`（`NULL` 格的输入框留空 = 没碰过，不算改动）。
    was_null: bool,
    /// 单行输入框。
    input: Entity<InputState>,
    /// 回车提交的订阅（编辑器关掉时随结构体一起摘掉）。
    _sub: Subscription,
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
    /// **agent 档**具体是哪条驱动（[`duck_drivers::DriverEntry::key`]）；内建档位恒 `None`。
    ///
    /// 表单一开就定死、**不给改成别的驱动**：换驱动 = 换一个引擎（缺省端口、品牌名、
    /// 协议都不一样），那是「重新选一次引擎」，回到选择器即可。做成只读之后，
    /// 这里就能安全地拿它回答「缺省端口 / 惯用账号 / 叫什么」。
    driver: Option<&'static str>,
    /// **牌子**（品牌表 [`brands::brand`] 的键）。`None` = 推不出牌子。
    ///
    /// 与 `driver` 一样是只读的：它是「选引擎」那一步的结果，表单一开就定死。
    /// 唯一能改它的入口是**模式族切换器**（`EngineRef::variants` 多于一条时画出来）——
    /// OceanBase 从 MySQL 模式切到 Oracle 模式换的是整条路（内建 mysql → agent），
    /// 那不是「改一个字段」，而是「重新选一次引擎」，所以由切换器整体换掉。
    profile: Option<&'static str>,
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
    /// 当前 Tab（[`FORM_TABS`] 的下标；dbx 同形四页签）。
    tab: usize,
    /// 认证方式（网络引擎才有；引擎不认得的档位在 `new` 里就落回口令）。
    auth_mode: AuthMode,
    /// 连接串模式的串（`auth_mode == ConnectionString` 时显示并生效）。
    connection_string: Entity<InputState>,
    /// **URL 附加参数**（JDBC URL 里 `?` 之后那截）。
    ///
    /// 品牌自己的缺省值由品牌表给（星环 Argo `auth=noSasl`、Impala 同、Kyuubi `auth=NONE`）——
    /// 那几个引擎**少了它就拒绝连接**，所以这一格不是「高级选项」，是必填项之一。
    /// 只有 agent 轨消费它；内建驱动的 URL 由各自的驱动拼。
    url_params: Entity<InputState>,
    /// **服务名**（GBase 8s 的 GBASESQLI / Informix 的 `INFORMIXSERVER` 双用途）。
    ///
    /// 只有 `EngineRef::server_name` 认得的两个 agent 引擎才显示与写入（各归各家的
    /// 档案字段）；其余引擎这一格不画、恒存 `None`。
    server_name: Entity<InputState>,
    /// 自签 CA 证书路径（TLS/SSL 分区；PG / MySQL 驱动真消费）。
    ca_cert_path: Entity<InputState>,
    /// 客户端证书路径（双向 TLS）。
    client_cert_path: Entity<InputState>,
    /// 客户端私钥路径。
    client_key_path: Entity<InputState>,
    /// 侧栏可见库白名单（`None` = 不过滤；「选择显示数据库」写回）。
    visible_databases: Option<Vec<String>>,
    ssh_enabled: bool,
    ssh_host: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_remote_host: Entity<InputState>,
    /// SSH 认证方式（口令 / 私钥 / 私钥 + 口令 / agent）。
    ssh_auth_method: SshAuthMethod,
    /// SSH 私钥路径（私钥两档才显示）。
    ssh_key_path: Entity<InputState>,
    /// SSH 私钥口令（掩码；只有「私钥 + 口令」档显示）。
    ssh_key_passphrase: Entity<InputState>,
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

/// 「选择显示数据库」弹窗的正文 —— **独立视图**，理由同 [`DriverPickerView`]：
/// 搜索框要边打边筛勾选列表，弹窗 `content` 闭包拿不到宿主的 `notify()`。
///
/// 交互对齐 dbx：用当前表单配置**临时连一次**拿库清单，用户勾选哪些库显示在侧栏；
/// 确认时把白名单写回表单的 `visible_databases`（勾满全部或全空 = 清空，不存死快照，
/// 之后新建的库才还能自动出现）。
struct VisibleDatabasesView {
    /// 回调宿主（确认时把勾选写回表单）。
    form: WeakEntity<EditFormView>,
    /// 临时连接拉到的库清单（顺序即弹窗展示顺序）。
    names: Vec<String>,
    /// 当前勾选。
    selected: std::collections::HashSet<String>,
    /// 过滤搜索框。
    search: Entity<InputState>,
    /// 搜索框订阅（延长生命周期，丢掉就不再有 `Change` 回调）。
    _subscription: Subscription,
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
    /// 当前分类（侧栏选中项；搜索态下右侧跨分类显示命中）。
    active_section: PickerSection,
    /// 单击选中的卡片 id（`None` = 还没选；「下一步」据此禁用与放行）。
    ///
    /// 存 id 而不是 `PickerTarget`：target 里带着引用，而 id 是卡片去重与搜索的
    /// **同一个键** —— 存它一份，`proceed` 时再按 id 找回卡片即可。
    selected: Option<&'static str>,
    /// 搜索框订阅（延长生命周期 —— 丢掉就不再有 `Change` 回调）。
    _subscription: Subscription,
}

/// 驱动商店（驱动管理弹窗的正文）—— **独立视图**，理由同 [`DriverPickerView`]：
/// 搜索框要边打边筛、安装进度要自己重绘，而弹窗 `content` 闭包是 `Fn`、
/// 拿不到 `&mut DatabasePage`。
///
/// # 状态从哪来
///
/// 三份事实各自有源，这里只**缓存**不重算：
///
/// | 事实 | 源 |
/// | --- | --- |
/// | 有哪些驱动、叫什么、属于哪个分类 | [`duck_drivers::catalog`]（编进二进制的清单，恒可用） |
/// | 每条的版本 / 工件 | 远端注册表（`registry::registry`，**要联网**，故可为 `None`） |
/// | 装了哪些、哪条有新版 | 本地状态（`DriverStore::state` / `installed`） |
///
/// 清单与本地状态是同步的、随时可读；注册表是阻塞网络调用，开弹窗时丢到后台线程
/// 去拉（[`RegistryState`] 记它的进度）。**注册表没拉到时列表照常能画** ——
/// 只是版本栏与「安装」按钮变成禁用态，而不是整页空着。
struct DriverManagerView {
    /// 归属页面（装完之后叫它刷一下，连接表单里的「去装驱动」提示才会消失）。
    page: WeakEntity<DatabasePage>,
    /// 过滤列表的搜索框。
    search: Entity<InputState>,
    /// 关键字（小写；订阅里算好，`render` 只读它）。
    query: String,
    /// 分类筛选（[`duck_drivers::DriverCategory::key`]；`None` = 全部）。
    category: Option<&'static str>,
    /// 只显示**已安装**的那几条。
    only_installed: bool,
    /// 远端注册表（`None` = 还没拉到 / 拉失败）。
    registry: Option<Arc<DriverRegistry>>,
    /// 拉注册表的状态。
    registry_state: RegistryState,
    /// 正在跑的安装：键 → 最近一步（界面据此画进度文案并禁用按钮）。
    running: HashMap<String, ProgressStep>,
    /// 批量安装的进度（第几个 / 共几个）。
    batch: Option<(u32, u32)>,
    /// 最近一条结果提示。
    notice: Option<DriverNotice>,
    /// 事件接收端；**只在有活儿在跑时是 `Some`**（收工置回 `None` 让轮询任务退出）。
    events: Option<std::sync::mpsc::Receiver<DriverEvent>>,
    /// 取消标志（有活儿在跑时挂上；内核每个步骤都会 `check()`）。
    cancel: Option<CancelFlag>,
    /// 事件轮询任务是否已经起来（避免每条驱动都起一个）。
    pumping: bool,
    /// 磁盘用量（收工时算一次，不在 `render` 里 `du`）。
    bytes_installed: u64,
    /// 搜索框订阅（延长生命周期 —— 丢掉就不再有 `Change` 回调）。
    _subscription: Subscription,
}

/// 远端注册表的拉取状态。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistryState {
    /// 还没开始（弹窗刚开）。
    Idle,
    /// 正在拉。
    Loading,
    /// 拉到了（`Ok` 的载荷在视图的 `registry` 字段里）。
    Ready,
    /// 拉失败（英文原文来自内核，展示时包一层中文外壳）。
    Failed(String),
}

/// 驱动管理弹窗里的一条结果提示（成功 / 失败）。
///
/// 与 `notify` 的气泡**不同**：那是「此刻发生了一件事」的一次性提示，这是一条
/// **留在弹窗里**的状态 —— 用户点完「安装」会接着往下看列表，气泡早消失了。
struct DriverNotice {
    /// 是不是失败（决定配色）。
    failed: bool,
    /// 文案（已经是成品中文，展示层拼好）。
    text: String,
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

/// 指标页最多画几张折线（其余指标留在明细表里，一个不少）。
const METRIC_CHART_LIMIT: usize = 6;

/// 默认映射最多取几条数值列（防一张几十列的表把图挤成麻花）。
const CHART_SERIES_LIMIT: usize = 5;
/// 判「一列是不是数值」时最多看几行（表可能几十万行，只看开头就够定性）。
const NUMERIC_PROBE_ROWS: usize = 64;

/// 单张图的高度（小倍数一格的高度）。
const CHART_PANEL_HEIGHT: f32 = 220.;

/// 一张图最多画几个点；超了就走 LTTB 抽稀（[`charts::decimate_lttb`]）。
///
/// 取值口径是**数出来的**，不是拍的：结果区一屏画两张图，每张宽约半屏（≈ 600pt），
/// 横轴还要留出 Y 轴刻度的那条沟（`VALUE_AXIS_GAP` ≈ 32pt），真正落在数据上的像素
/// 也就 500 出头。画比像素更多的点，多出来的点只会叠在相邻像素上 —— 除了让绘制变慢、
/// 让折线糊成一条实心带，没有任何信息增益。所以封在 400：比像素略少，
/// 相邻点在屏幕上一定分得开，「峰在哪一行」这种判读才不会丢。
///
/// ⚠️ 与本仓既有的 `point_count` 不是一回事：`gpui-component` 的 `point_count` 只做
/// `max(数据长度)`（见其 `chart::axis_point_count`），**只能拉长不能抽稀**。
const CHART_MAX_POINTS: usize = 400;

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

/// 单类候选的条数上限（表 / 关键字 / 函数**各算一份**）。
///
/// 分类限额而不是只给总量：词表按引擎展开之后光「关键字」就有一百多条，按总量
/// 截断会让「函数」这一桶整桶消失 —— 用户敲 `count` 反而什么都补不出来。
const COMPLETION_LIMIT: usize = 80;

/// 一次补全的候选总量上限（三类之和的兜底；正常前缀过滤后远达不到）。
const COMPLETION_TOTAL_LIMIT: usize = 240;

/// SQL 编辑器的补全来源：关键字 / 函数 + 当前会话目录的表名；点号后取该表的列名。
///
/// 列名要查库（`table_columns` 是阻塞 API）：经后台执行器跑、主线程 Task 里写缓存。
/// 每表只查一次，缓存随弹层 / 会话生命周期走（provider 挂在编辑器状态上）。
struct SqlCompletionProvider {
    view: WeakEntity<DatabasePage>,
    columns: Rc<std::cell::RefCell<HashMap<String, Vec<Column>>>>,
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

/// 建连所需的三份额外口令。
///
/// 单独一个结构体而不是继续往元组里塞：三个 `Option<String>` 并排之后，
/// 光看调用点已经分不清谁是谁，而**送错口令**的后果是「认证失败」这种最难查的错。
#[derive(Debug, Clone, Default)]
struct Credentials {
    /// 数据库口令（ES 的 API Key 模式也走这一格：值是 key 原文）。
    db: Option<String>,
    /// SSH 跳板机口令。
    ssh: Option<String>,
    /// SSH 私钥口令（加密私钥的 passphrase；「私钥 + 口令」档用）。
    ssh_key: Option<String>,
    /// SOCKS5 / HTTP CONNECT 代理口令。
    proxy: Option<String>,
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

/// 名额凭证：`drop` 即归还（持到工作闭包跑完为止）。
struct DbOpPermit {
    gate: Arc<DbOpGate>,
}

/// 底部居中 toast 的存活时长（dbx 同款约 2 秒自动消失）。
///
/// 足以看清一句话，又不会在连点几次时堆成一列 —— 后来者直接替换前者（见
/// [`DatabasePage::toast`] 的序号判据），永远只有一枚。
const TOAST_LIFETIME: Duration = Duration::from_millis(2000);

/// 一条底部居中 toast（页面级动作回执）。
struct Toast {
    /// 正文。
    message: SharedString,
    /// 语义级别：决定图标与图标色。
    tone: notify::Tone,
    /// 序号：定时器到点时只有「还是这一条」才清（见 [`DatabasePage::toast`]）。
    seq: u64,
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
    /// 「表结构：编辑面 / SQL 预览」的竖向分栏状态（同 [`Self::sql_split`] 的机制）。
    ///
    /// 与 SQL 分栏**分开两份**：两处的合理比例不一样（那边是「输入 SQL / 看结果」，
    /// 这边是「改字段 / 看将执行的 DDL」），共用一个状态会把对方的拖拽结果吃过去。
    structure_split: Entity<ResizableState>,
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
    /// 实例指标页**当前聚焦**的指标（左栏指标子项点进来的高亮；切标签随快照走）。
    focused_metric: Option<String>,
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
    /// 查询结果**客户端分页**的当前页（0 起；`Mode::Query` 的网格按它切片）。
    query_page: usize,
    /// 查询结果的**客户端排序**（`None` = 原始返回顺序）。
    ///
    /// 与 `table_sort` 是**两套**：表浏览那套是服务器侧 `ORDER BY`（改的是取数），
    /// 这套只重排**已到手的这批行**（同「结果内搜索」「客户端分页」一层）。
    query_sort: Option<SortSpec>,
    /// 结果标签（一条语句一个）；`query_result` 恒等于激活标签的视图。
    result_tabs: Vec<ResultTab>,
    /// 激活的结果标签下标。
    active_result_tab: usize,
    /// 下一次结果落地时**替换**激活标签而不是追加（编辑提交后的重跑：
    /// 保存改的是这一条语句的结果，不该多出一条一模一样的标签）。
    ///
    /// 页面级瞬时旗标：`reload_edit_source` 置位、`install_query_result` 消费，
    /// 执行失败时也清掉（否则下一次普通运行会被误替换）。
    result_replace_active: bool,
    /// 多语句批次里**还没跑到**的语句队列（(语句槽下标, 文本)）。
    ///
    /// 页面级（不进快照）：批次的生命周期只到「跑完 / 失败 / 停止」，切标签
    /// 换的是结果展示，不是正在跑的批次。
    stmt_queue: VecDeque<(usize, String)>,
    /// 语句槽的执行状态（编辑器左侧逐条 ▶ 那一列；键 = 语句序号）。
    ///
    /// 页面级单例：编辑器文本一变（`new` 里的 observe）或全文重跑就清空 ——
    /// 语句序号随文本漂移，跨编辑留着只会指错行。
    stmt_states: HashMap<usize, StmtRunState>,
    /// `stmt_states` 对应的编辑器文本快照（observe 里比对用，变了才清）。
    stmt_states_text: String,
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
    /// 结果网格的**组件状态**：表模型（`Arc<TableModel>`）、列宽 + 前缀和、藏列、
    /// 展示行序（搜索命中 + 客户端分页）、两个滚动句柄、列宽拖拽态。
    ///
    /// 原先这些是页面拿 `column_widths` / `col_resize` / `result_x_scroll` /
    /// `result_v_scroll` 四个字段自己手搓的，现在整体归公共数据表组件
    /// （`duck_components::data_table`）。页面只在**结果 / 搜索 / 分页 / 藏列**变化时
    /// 调 [`Self::sync_data_table`] 推一次，交互经 [`DataTableEvent`] 订阅回来。
    ///
    /// 页面级而非按标签存：换标签时用 `set_model` 推新模型，**列名序列**没变就沿用
    /// 现有列宽（同结构的两个标签切来切去不会把宽度重置），变了才重新估宽。
    data_table: Entity<DataTableState>,
    /// 列模式（转置视图）三列的宽度：名称 / 值 / 类型。
    ///
    /// 页面级（不进标签快照）：转置视图的列宽是「这一屏我想怎么看」的临时调整，
    /// 与结果集无关，不值得跟着标签切来切去。
    columns_mode_widths: [f32; 3],
    /// 列模式正在拖的列宽：`(列下标, 起拖时指针 x, 起拖时列宽)`。
    columns_mode_resize: Option<(usize, f32, f32)>,
    /// 结果区是否展开（工具条 `panel-bottom` 开关；`false` = 编辑器独占主区）。
    ///
    /// 一次**未提交的查看**：不进标签快照 —— 收起是「这一屏想让 SQL 大点」，
    /// 切标签回来还照旧展开，不让用户每个标签都要再点一次。
    results_visible: bool,
    /// SQL 编辑器（查询输入框）是否展开（工具条 `rows-3` 开关；`false` = 只留结果区）。
    ///
    /// 与 [`Self::results_visible`] 是**两条独立的轴**：看结果时把编辑器收起来
    /// （结果占满主区）与只看 SQL 时把结果收起来是两种不同的需要，用同一个开关
    /// 表达不了。与它同口径，也不进标签快照。
    editor_visible: bool,
    /// 结果网格里被隐藏的列名（见 [`SessionView::hidden_columns`]：按标签存，
    /// 只影响显示）。渲染时推进 [`Self::data_table`] 的 `set_hidden_names`。
    hidden_columns: BTreeSet<String>,
    /// SQL 语义诊断结果（编辑器内容变化时重算；见 [`duck_db::sqldiag`]）。
    sql_diagnostics: Vec<duck_db::SqlDiagnostic>,
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
    /// 过滤条「列」可搜索下拉（列多时顶上有搜索框；选项 / 选中值由
    /// `sync_select_pickers` 在渲染入口按指纹同步，确认走 `SelectEvent` 订阅）。
    filter_column_select: Entity<DbSelectState>,
    /// 过滤列选项的签名（列名清单拼接）：没变就不动 `SelectState`，
    /// 用户正在敲的搜索词不被下一帧抹掉。
    filter_column_stamp: Option<String>,
    /// 批量条「列」可搜索下拉（选项与过滤列同一份结果集列名）。
    batch_column_select: Entity<DbSelectState>,
    batch_column_stamp: Option<String>,
    /// 命名空间（库 / Schema）可搜索下拉：编辑器工具条与右栏**各一份实体**
    ///（两处可能同时可见，`Select` 的 ElementId 挂在实体上不能共用）。
    toolbar_schema_select: Entity<DbSelectState>,
    dock_schema_select: Entity<DbSelectState>,
    /// 命名空间选项签名（活跃连接 + 名单 + 当前值）。
    schema_stamp: Option<String>,
    /// 命名空间下拉确认后待处理的切换：订阅闭包拿不到 `Window`，
    /// 攒到渲染入口（那里有）再调 [`Self::select_database`] / [`Self::select_schema`]。
    pending_namespace_pick: Option<String>,
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
    /// 表剪贴板（dbx 的复制表 / 粘贴表对位）：树上「复制表」记下源连接与表，
    /// 在别的库 / 表分类节点上「粘贴表」—— 同连接生成 CTAS 语句，跨连接开
    /// 数据传输标签（源 = 剪贴板那张表，目标在页面上落到粘贴处）。
    table_clipboard: Option<(String, TableRef)>,
    /// 建表标签的**落库回执旗标**：确认执行 `CREATE TABLE` 时置上（带着新表名），
    /// 执行成功由 `install_query_result` 消费 —— 标签原地转正成这张表的结构
    /// 编辑器（dbx `onStructureEditorSaved` 的 `createdTable` 分支同口径）。
    /// 失败在 `execute_sql` 的失败分支清掉，不污染下一次普通查询。
    create_applied: Option<TableRef>,
    /// 表结构编辑器的草稿（当前标签的那一份；`None` = 列清单还没取回）。按标签存。
    structure: Option<StructureDraft>,
    /// 字段网格的横向 / 纵向滚动句柄（滚动条从它们读位置、拖拽写回偏移）。
    ///
    /// **跨帧持久**是硬要求：每帧新建会把滚轮写进来的偏移下一帧就丢
    /// （`tests/db_grid_horizontal_scroll.rs` 的注释钉过这条）。页面级即可——
    /// 偏移跟着滚动容器走，切标签重建草稿时容器还在。
    structure_grid_x_scroll: ScrollHandle,
    structure_grid_y_scroll: ScrollHandle,
    /// 结果区「列」视图（转置）的横向 / 纵向滚动句柄：口径同
    /// [`Self::structure_grid_x_scroll`] —— `Scrollbar` 要跨帧持久的句柄才读得到
    /// 偏移、拖得出位置。
    result_columns_x_scroll: ScrollHandle,
    result_columns_y_scroll: ScrollHandle,
    /// ER 关系图的数据（当前标签的那一份；`None` = 还没取回）。按标签存。
    er: Option<ErData>,
    /// 比较数据的状态（当前标签的那一份；`None` = 不是比较标签）。按标签存。
    compare: Option<CompareState>,
    /// 数据传输的状态（当前标签的那一份；`None` = 不是传输标签）。按标签存。
    transfer: Option<TransferState>,
    /// 「复制其他表字段」弹窗的草稿（`None` = 没开；见 [`CopyFieldsDraft`]）。
    /// **不随标签走**：弹窗是一次「从源表挑字段追加进当前草稿」的临时态。
    copy_fields: Option<CopyFieldsDraft>,
    /// 「字段注释放大编辑」弹窗的草稿（`None` = 没开）；与 `copy_fields` 同口径，
    /// 页面级、不随标签走（弹窗确认时要把长注释写回字段草稿）。
    structure_comment: Option<StructureCommentDraft>,
    /// 「复制全部列名」弹窗是否打开（列名清单现读当前草稿，不另存快照）。
    structure_copy_names: bool,
    /// 行编辑器（暂存式）：改动只存输入框，点保存才落库。
    row_editor: Option<RowEditor>,
    /// JSON 档的行 JSON 编辑器（见 [`JsonEditor`]）。**不随标签走**：它是一次
    /// 「看着 JSON 改这一行」的临时态，渲染期按当前行同步（`filter_value` /
    /// `batch_value` 同一条口径 —— 未提交的输入不是某个标签的结果区状态）。
    json_editor: Option<JsonEditor>,
    /// 结果区的图表视角配置（`None` = 看表格）。按标签存（见 [`SessionView`]）。
    chart_view: Option<ChartConfig>,
    /// 结果区视图档位（行 / 列 / JSON 三选一，见 [`ResultViewMode`]）。按标签存。
    result_view: ResultViewMode,
    /// 结果区顶部 tab（数据表 / 摘要 / 图表 / 消息 / 执行计划）。按标签存。
    result_area_tab: ResultAreaTab,
    /// 「消息」tab 的执行流水（当前标签的那一份）。按标签存。
    exec_log: Vec<ExecLogEntry>,
    /// 翻页条的「跳转到第 N 页」输入框（查询 / 表浏览两套翻页组共用一个框；
    /// 切标签 / 换模式时清空即可，跳页是一次性输入，不值得各存一份）。
    page_jump: Entity<InputState>,
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
    /// 右栏 dock 停在哪个分段（详情 / 历史 / 单元格 / 构建器）。
    ///
    /// 取代原先的 `history_open`：历史从「编辑器上方的横向面板」升级为
    /// **右侧常驻分段**（第五轮定稿），所以这不再是「开 / 关」，而是「看哪一栏」。
    right_dock: RightDock,
    /// 右栏这次展开是不是**页面替用户做的**（[`Self::reveal_right_dock`] 置位）。
    ///
    /// 只用来决定「内容失效时该不该自动收起」：页面自己展开的才收；用户从顶栏
    /// 手动展开的则原地留空态 —— 不跟用户对抗。
    right_dock_auto: bool,
    /// 是否已订阅活跃路由右栏的开合变化（一次性；见 [`Self::ensure_right_dock_subscribed`]）。
    ///
    /// 工具条「历史」按钮的高亮 = 「右栏开着 **且** 停在这一栏」，而右栏开合归 shell
    /// 管 —— 不订阅的话，用户从顶栏收起右栏后按钮会一直亮着（页面是 `AnyView`，
    /// shell 重绘不连带重绘它）。
    right_dock_subscribed: bool,
    /// 结果区的焦点柄（dbx 同款「聚焦的网格拥有剪贴板键」）。
    ///
    /// 点击结果区任意处就把焦点挪到这里：`⌘C / ⌘A / ⌘X / ⌘V` 的绑定挂在页面
    /// `"Database"` 上下文上，而 gpui 的按键派发沿**焦点链**收集 —— 不聚焦的话，
    /// 点完网格焦点还留在 SQL 编辑器里，这些键全部被编辑器更深一层抢走
    /// （用户看到的就是「结果区不支持系统快捷键」）。
    grid_focus: FocusHandle,
    /// 右栏「历史」分段里的子栏（最近 / 已保存）。
    history_segment: HistorySegment,
    /// 单元格详情指向的格子（行下标, 原始列下标）；右栏「单元格」分段的数据源。
    ///
    /// 一次**未提交的查看**，不是某个标签的结果区状态 —— 不进快照，切标签保留。
    cell_detail: Option<(usize, usize)>,
    /// 单元格详情的值编辑器（`None` = 这一格只读展示；可编辑性与行编辑器同一道闸）。
    ///
    /// 与 `cell_detail` 同口径：页面级、不进标签快照。
    cell_detail_editor: Option<CellDetailEditor>,
    /// 可视化 SQL 构建器的状态（右栏「构建器」分段的数据源）。
    ///
    /// 与 `cell_detail` 同口径：**页面级、不进标签快照** —— 右栏是 shell 级、一条路由
    /// 一个，切标签把「正拼到一半的条件」清掉才是意外。
    builder: BuilderState,
    /// 单元格就地编辑器（双击进入；`None` = 没在编辑）。
    cell_editor: Option<CellEditor>,
    /// 查询结果客户端分页的**仅本次**每页行数覆盖（`None` = 跟随默认设置）。
    query_page_size_override: Option<u32>,
    /// 选中的 AI 厂商档案 id；`None` = 跟随助手页的默认档案 / 第一条。
    ///
    /// 档案本身（base_url / model / 密文 key）**不落在这里**：一律按需从
    /// [`crate::ai`] 读（助手页与 db 页共用同一份 `kv: ai_providers`），
    /// 避免.db 页缓存一份会和助手页打架的副本。
    ai_provider: Option<String>,
    /// AI 提问输入框。
    ai_question: Entity<InputState>,
    /// AI 助手对话里的消息（页面级，跨标签保留：用户问的是「这个库」的问题，
    /// 切到另一个标签再切回来还该看到刚才那轮）。
    ai_messages: Vec<AiMessage>,
    /// AI 对话消息区的滚动句柄：每追加一条就滚到底（新消息永远可见）。
    ai_scroll: ScrollHandle,
    /// 最近一次执行失败的上下文（供「AI 修错 / 解释」注入真实错误原文）。
    last_error: Option<QueryError>,
    /// 状态栏文案（连接 / 执行的即时反馈）。
    status: SharedString,
    /// 底部居中 toast（dbx 同款**动作回执**，约 [`TOAST_LIFETIME`] 后自消）；`None` = 不显示。
    ///
    /// 与 `status` 的分工：`status` 是**常驻读数**（正在执行… / 查询完成 / 连接状态），
    /// 这条是**动作回执**（已复制、已格式化……）—— 看完就走，不必长期占着状态栏，也就
    /// 不会被下一条读数顶掉。dbx 的复制 / 保存反馈都是这种底部居中 toast。
    toast: Option<Toast>,
    /// toast 序号源：定时器到点时只有「还是自己那一条」才清 —— 否则连点两次会把
    /// 后来的新 toast 一起抹掉，第二条等于没弹。
    toast_seq: u64,
    /// 有后台数据库操作在跑：期间禁用会触发同类操作的按钮。
    ///
    /// ⚠️ **建连不走这里**（见 [`Self::connecting`]）：连接是「等远端回话」，等着的时候
    /// 用户理应能去点别的档案、跑别的查询 —— 拿全局 `busy` 拦它，就成了「一次只能
    /// 连一个」的假排队。
    busy: bool,
    /// 正在建立连接的档案 id 集合（**并发建连**的在途标记）。
    ///
    /// 只有两件事：① 同一条在途时再点忽略（不弹 toast —— 行首圆点已经是琥珀色，
    /// 再弹一条「正在连接」是噪音）；② 左栏圆点据此显示「连的是哪几条」。
    /// 连接完成后（无论成败）由 [`DbShell::spawn_connect`] 摘除。
    connecting: std::collections::HashSet<String>,
    /// 正在跑的**可取消**工作（有则工具条显示「停止」，见 [`RunningQuery`]）。
    running: Option<RunningQuery>,
    /// 本次运行是否被用户点过「停止」：决定 [`DbError::Cancelled`] 的文案
    /// （没点过就是超时守护开的枪）。
    stop_requested: bool,
    /// 本次运行的超时秒数（错误文案用；随 [`Self::spawn_db_guarded`] 的设置走）。
    run_timeout_secs: u64,
    /// 运行令牌自增源（见 [`RunningQuery::token`]）。
    run_seq: u64,
    /// 正在跑的「执行 SQL 文件」任务（`None` = 没在跑，见 [`SqlFileJob`]）。
    ///
    /// 与 [`Self::running`] **并存**：文件执行确实占着一条 `running`（可取消、有守护），
    /// 但它另有一条「读到多少字节 / 跑了多少条」的进度轴，以及一个**逐条**生效的停止
    /// 谓词（`running.cancel` 只打断手上这一条，谓词决定后面还跑不跑）——两样都不属于
    /// 单条查询，塞进 `RunningQuery` 只会让那条路背上看不懂的状态。
    sql_file: Option<SqlFileJob>,
    /// 列头下拉的**本地值筛选**：列名 → 允许的展示值集合（只过滤当前结果集的显示行，
    /// 服务器数据不动；「清除」按列摘掉）。
    ///
    /// 值按**展示文本**记（与格子里看见的一致，NULL 也照 `render` 的文本算）；
    /// 重跑查询 / 换标签时随结果一起失效（见 [`Self::sync_data_table`] 的调用点）。
    local_filters: BTreeMap<String, BTreeSet<String>>,
    /// 列头下拉的**列格式化**：列名 → 格式档（当前只有数值千分位）。
    ///
    /// 展示文本在 `build_table_model` 预算时应用（`Arc<str>` 一次算好），所以改档
    /// 必须重推模型（`sync_data_table`），不能只在渲染层过滤。
    column_formats: BTreeMap<String, ColumnFormat>,
    /// **单次覆盖**的查询超时（秒）；`None` = 跟随连接默认 / 全局默认。
    ///
    /// 只活在本次页面会话里（不落盘、不进档案）：落盘的「连接默认」在连接详情表单的
    /// QUERY 分节。生效值三级回落见 [`Self::effective_timeout`]。
    query_timeout_override: Option<u64>,
    /// 「查询选项」弹窗里的超时输入框（弹窗每次打开时用当前生效值回填）。
    query_timeout_input: Entity<InputState>,
    /// 结果区「在结果中搜索」的输入框（**页面级单例**）。
    ///
    /// 搜索词不进 [`SessionView`]（不按标签留存）：搜索是「看着这份结果时」的临时
    /// 操作，换一个标签 = 换一份结果，旧词几乎必然失效 —— [`Self::activate`] 里
    /// 直接清空，比「每个标签留一份输入框实体再在快照间搬」便宜且不易错。
    result_search: Entity<InputState>,
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
    /// 左栏树里被**置顶**的对象（键 = `conn:namespace:名称`；会话内有效，不进 kv
    /// —— 与 `tree_open` 同口径：是「这一次看库」的视图，不是偏好设置）。
    /// 排序规则：置顶的排在本分类最前，其余保持目录原序。
    pinned_tables: std::collections::BTreeSet<String>,
    /// 表节点展开后的**结构缓存**（键 = [`table_tree_key`]；会话内有效，不进 kv）。
    /// 只在节点首次展开时取一次（见 [`DatabasePage::load_table_tree`]），之后命中缓存。
    table_tree: std::collections::HashMap<String, TableTreeMeta>,
    /// 正在取结构的表（同键口径）：加载中再点展开不重复发起，回来后摘除。
    table_tree_loading: std::collections::HashSet<String>,
}

#[cfg(test)]
mod tests;
