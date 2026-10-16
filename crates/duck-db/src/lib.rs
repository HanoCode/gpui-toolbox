//! 数据库模块内核：连接档案、驱动分发、SQL 智能与统一结果模型。
//!
//! 分层（`docs/database-design.md`）：本 crate 是**内核层**，不依赖 gpui；
//! 对外 API 全部同步——SQLite / PostgreSQL / MySQL 用阻塞驱动，ClickHouse 走
//! HTTP 接口（`ureq` 也是阻塞的）。调用方（`duck-app` 的页面）把调用丢进 gpui 的
//! `background_executor`，本 crate 不开线程、不建 runtime。异步-only 驱动
//! （MongoDB / SQL Server，M2）在驱动内部自管 current-thread runtime `block_on`，
//! tokio 的 Future 与类型都**不出本 crate**。
//!
//! ```text
//! duck-app/pages/db.rs（UI，gpui）
//!   │  background_executor().spawn(阻塞调用)
//!   ▼
//! duck-db（本 crate：config / drivers / sqlintel / result）
//!   ▼
//! duck-platform::secret（凭据）  rusqlite / postgres / mysql / ureq（阻塞驱动）
//! ```
//!
//! # 与其他 crate 的分工
//!
//! - 连接档案的**持久化**不在这里：`duck-app` 把 [`config::ConnectionConfig`] 序列化成
//!   JSON 存进 `duck-store` 的 kv（沿用 `appearance` 的「结构体 → serde_json → 单键」
//!   范式）。本 crate 只定义结构与校验。
//! - **凭据**只走 [`config`] 里的薄封装 → `duck-platform::secret`：kv 里只存
//!   `secret_ref` 语义的键名，口令本体绝不落 SQLite。
//! - **SQL 智能层**（[`sqlintel`]）是纯函数：语句拆分 / 分类 / 风险分级 / 只读证明，
//!   供执行器与未来的 AI 闸门共用。

pub mod ai;
pub mod batch;
pub mod cancel;
pub mod config;
pub mod csv;
pub mod drivers;
pub mod error;
pub mod hooks;
pub mod plan;
pub mod proxy;
pub mod result;
pub mod sqldiag;
pub mod sqlfile;
pub mod sqlintel;
pub mod sqlwords;
pub mod ssm;

/// 第三方连接档案导入（DBeaver / TablePlus）。
pub mod import;
pub mod tunnel;

pub use ai::{AiProvider, OpenAiCompatible};
pub use batch::{
    BatchReport, BatchTransaction, SqlBatchOptions, StatementOutcome, StatementStatus,
    execute_batch,
};
pub use cancel::{CancelHandle, CancelTimer};
pub use config::{ConnectionConfig, DatabaseKind, EnvTag, SshTunnelConfig, TlsMode};
pub use csv::parse_csv;
pub use drivers::{
    BatchOutcome, CatalogTable, CellEdit, Connection, DatabaseObject, FilterOp, InsertRow,
    InstanceFact, InstanceMetric, InstanceOverview, InstanceTable, InstanceView, OrderBy, PkValues,
    RowCount, SwitchOutcome, TableColumnMeta, TableFilter, TableForeignKey, TableIndex, TableKind,
    TableRef, TableTrigger, quote_identifier, quote_qualified,
};
pub use error::DbError;
pub use hooks::{
    ConnectionHook, ConnectionHooks, DEFAULT_HOOK_TIMEOUT_SECS, HookAction, HookContext,
    HookFailure, HookOutcome, HookPhase, HookReport, HookStatus, run_hook, run_phase,
};
pub use plan::{PlanNode, PlanTree, parse_plan};
pub use proxy::{ProxyConfig, ProxyKind, ProxyTunnel, ProxyTunnelSpec};
pub use result::{BinaryEncoding, Column, ExecOutcome, ResultSet, Value, tsv_field};
pub use sqldiag::{Severity as SqlSeverity, SqlDiagnostic, diagnose_sql};
pub use sqlfile::{
    FileProgress, FileReport, FileStatementError, SqlFileEncoding, SqlFileError, SqlFileOptions,
    execute_sql_file,
};
pub use sqlintel::{
    EditTarget, ExplainSkip, FormattedSql, SqlFormatOptions, SqlKeywordCase, StatementScanner,
    covers_columns, edit_target, explain_sql, explain_sql_for, fingerprint, format_sql,
    format_sql_with, normalize_for_fingerprint, risk_of_statements, split_statements,
};
pub use sqlwords::{QueryLanguage, SqlWords};
pub use ssm::{SsmConfig, SsmTunnel, SsmTunnelSpec};
pub use tunnel::{SshTunnel, SshTunnelSpec};
