//! Redis / Valkey 模块内核：连接档案、分级锁写闸门、SCAN 键树与值模型。
//!
//! 分层（`docs/redis-design.md`）：本 crate 是**内核层**，不依赖 gpui；
//! 对外 API 全部同步——`redis` 走**阻塞**客户端，调用方（`duck-app` 的页面）
//! 把调用丢进 gpui 的 `background_executor`，本 crate 不开线程、不建 runtime、
//! **不引 tokio**（`redis` 关掉了 aio / tokio-comp / smol-comp 三个特性）。
//!
//! ```text
//! duck-app/pages/redis/（UI，gpui）
//!   │  background_executor().spawn(async move { 阻塞调用 })
//!   ▼
//! duck-redis（本 crate：config / capability / writegate / conn / scan / query /
//! sort / ttl / tags / trash / value / decode / json_tree / server / monitor）
//!   ▼
//! duck-platform::secret（凭据）  redis（阻塞客户端）
//! ```
//!
//! # 与其他 crate 的分工
//!
//! - **不依赖 `duck-db`**：两者是平行的引擎内核（关系型 vs 键值），
//!   连接档案、凭据键名与错误类型各自独立。理由见设计文档 §1.1。
//! - 连接档案的**持久化**不在这里：`duck-app` 把 [`config::RedisConfig`] 序列化成
//!   JSON 存进 `duck-store` 的 kv 单键（`redis_connections`），沿用 `appearance` 的
//!   「结构体 → serde_json → 单键」范式。本 crate 只定义结构与校验。
//! - **凭据**只走 [`config`] 里的薄封装 → `duck-platform::secret`：kv 里只存
//!   `secret_ref` 语义的键名，口令本体绝不落盘。
//!
//! # 安全基座
//!
//! [`capability`] 决定「**能不能**做」（访问模式：只读开关 / 生产档锁写 / ACL 只读），
//! [`writegate`] 决定「做的时候要**怎么确认**」（破坏性命令的二次确认形式）。
//! **两者都是纯函数**，可普通 `#[test]` 覆盖，UI 不自己判断环境。
//!
//! # 两条硬纪律（改代码前先读）
//!
//! 1. **不用 `KEYS`** 遍历键空间，一律 [`conn::RedisConnection::scan`]（增量、不阻塞服务端）。
//! 2. **不整读大集合**（`HGETALL` / `SMEMBERS` / `ZRANGE 全量`），一律游标分页或窗口读取。

pub mod capability;
pub mod config;
pub mod conn;
pub mod decode;
pub mod error;
pub mod json_tree;
pub mod jsonpath;
pub mod monitor;
pub mod pubsub;
pub mod query;
pub mod scan;
pub mod server;
pub mod sort;
pub mod tags;
pub mod trash;
pub mod ttl;
pub mod value;
pub mod writegate;

pub use capability::{AccessMode, Capability, DenyReason, Gate, WRITE_UNLOCK_SECS};
pub use config::{
    DEFAULT_PORT, EnvTag, MAX_DB, RedisConfig, SshTunnelConfig, TlsMode, WritePolicy,
};
pub use conn::{
    AclProbe, BATCH_CHUNK, KeyMeta, KeyObjectInfo, META_CHUNK_KEYS, RedisConnection, info_field,
    object_info_from, parse_info_pairs, parse_info_sections, parse_meta_replies,
    split_command_args,
};
pub use decode::{DecodeKind, Decoded, HEX_PREVIEW_BYTES, Message, decode, hex_dump};
pub use error::RedisError;
pub use json_tree::{
    JSON_TREE_MAX_CHILDREN, JSON_TREE_MAX_NODES, JSON_TREE_PREVIEW_CHARS, JsonNodeKind,
    JsonTreeNode, JsonTreeRow, default_expanded, flatten, json_tree, parse_document, toggle_path,
    tree_of,
};
pub use jsonpath::{JsonPathHit, JsonPathQuery, run as jsonpath_run};
pub use monitor::{MonitorSession, POLL_TIMEOUT};
pub use pubsub::{PubSubMessage, PubSubSession};
pub use query::{QueryMode, ScanPlan, escape_glob, scan_plan};
pub use scan::{
    AUTO_EXPAND_DEFAULT, ScanPage, TreeNode, build_tree, build_tree_sorted, build_tree_with_depth,
    prefix_pattern, should_auto_expand, split_namespace, stamp_folder_tags,
};
pub use server::{
    ClientInfo, ConfigEntry, DbKeyStats, MemoryOverview, ServerFeatures, SlowlogEntry,
    StatusOverview, memory_overview, parse_client_list, parse_config_entries, parse_keyspace,
    parse_memory_stats, parse_slowlog, status_overview_from,
};
pub use sort::{KeySort, Sortable, ttl_rank};
pub use tags::{
    KeyMetadata, KeyTags, TAG_COLOR_COUNT, TAG_STORE_VERSION, TagColor, TagFilter, TagHistogram,
    TagStore, resolve_folder_tag,
};
pub use trash::{
    TRASH_MAX_ENTRIES, TRASH_MAX_PAYLOAD, TRASH_MAX_TOTAL_BYTES, TRASH_MAX_VALUE_MEMORY,
    TRASH_RETENTION_MS, TRASH_STORE_VERSION, TrashEntry, TrashMeta, TrashStore, now_millis,
    payload_fits, should_stash,
};
pub use ttl::{
    TTL_EXPIRING_THRESHOLD_SECS, TtlChipKind, TtlFilter, format_ttl_chip, ttl_chip_kind,
};
pub use value::{FieldPair, KeyType, RedisValue, StreamEntry, format_reply};
pub use writegate::{ConfirmKind, confirm_kind, is_destructive, is_write_command};
