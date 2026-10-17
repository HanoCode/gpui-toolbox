//! 项目管理模块内核。
//!
//! 设计文档：`docs/projects-design.md`。分层：
//!
//! ```text
//! duck-app/pages/projects/（UI，gpui，三件套 State / View / App）
//!   │  background_executor().spawn(阻塞调用) + cx.spawn 回投
//!   ▼
//! duck-project（本 crate：models / query / db / store）
//!   ▼
//! duck-platform::{paths, ensure_dir}   rusqlite（bundled）
//! ```
//!
//! # 硬约束
//!
//! 1. **零 gpui**：本 crate 不依赖 `gpui` / `duck-components` / `duck-store` / `duck-db`
//!    —— 与 `duck-db` / `duck-redis` / `duck-ai` / `duck-mail` 同纪律，内核可被 CLI、
//!    独立悬浮窗或插件宿主复用。
//! 2. **全同步**：公共 API 上没有 `async`，也不出现 `tokio::`。写到磁盘的调用是阻塞的，
//!    线程由调用方决定（本 crate 自己不开线程、不建 runtime）。
//! 3. **写路径单事务**：多语句写（归档的级联、标签合并、看板列整表替换）一律一个
//!    `Connection::transaction`，不留「改了一半」的中间态。
//! 4. **归档是正交维度**（不是状态）：`archived_at IS NOT NULL` 即已归档，
//!    常规查询一律隐式附加 `archived_at IS NULL`，见 [`ArchiveScope`]。
//! 5. **内核不含界面文案**：所有面向用户的字面量在 `duck-app` 侧取词；本 crate 的错误
//!    详情一律英文（i18n 守卫按「源码里的中文字面量必须登记」扫描整个 `crates/`）。
//!
//! # 两种时间表示（不是随手混用）
//!
//! - **时刻** `created_at` / `updated_at` / `completed_at` / `archived_at` → `i64` Unix **秒**，
//!   与 `duck-store` / `duck-ai` / `duck-mail` 一致。
//! - **日期** `start_date` / `due_date` → [`CalendarDate`]，落库为 `YYYY-MM-DD` 文本。
//!   日期是**日历事实**而非时刻：把「10-18 截止」存成 Unix 秒，在 UTC-5 的机器上读出来是
//!   10-17。文本日期不跨时区漂移，且字典序即时间序（SQLite 可直接 `ORDER BY` 与 `<` 比较）。
//!
//! 内核**不知道用户在哪**：`今天 / 本周 / 逾期` 这类口径所需的「今天」由调用方传入
//! （[`TaskQuery::today`]），内核因此可无环境依赖地单测。
//!
//! # 模块地图
//!
//! | 模块 | 职责 |
//! | --- | --- |
//! | [`error`] | [`ProjectError`] / [`Result`] |
//! | [`time`] | Unix 秒、[`CalendarDate`] 与历法换算（手写，无日期库依赖） |
//! | [`models`] | 领域枚举与记录：状态 / 优先级 / 四象限、分组 / 项目 / 任务 / 标签 / 里程碑 / 看板列 |
//! | [`query`] | [`TaskQuery`] 构建器与 `ARCHIVE` 可见性口径（纯函数，可单测） |
//! | [`db`] | 打开连接、PRAGMA 与版本化迁移 |
//! | [`store`] | [`ProjectStore`]：CRUD、归档、统计（按实体分文件） |

pub mod db;
pub mod error;
pub mod models;
pub mod query;
pub mod store;
pub mod time;

pub use error::{ProjectError, Result};
pub use models::{
    ArchiveRoot, BoardColumn, ColumnSpec, DEFAULT_PROJECT_COLOR, FileRef, Group, LogKind,
    Milestone, NewGroup, NewMilestone, NewProject, NewTag, NewTask, Priority, Project,
    ProjectPatch, ProjectStatus, Quadrant, SubtaskProgress, Tag, Task, TaskContext, TaskCounts,
    TaskPatch, TaskStatus, WeeklyPlan, WeeklyPlanStatus, WorkLog,
};
pub use query::{ArchiveScope, DueFilter, TaskQuery, TaskSort};
pub use store::{
    ArchiveCounts, ArchivedProject, ArchivedTask, BoardStats, ClearScope, ProjectStore,
    ProjectTree, TagUsage, default_column_specs, normalize_tags,
};
pub use time::{CalendarDate, Weekday, now_seconds};
