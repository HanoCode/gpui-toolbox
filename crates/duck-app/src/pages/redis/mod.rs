//! Redis 页：连接管理（左栏）+ 键空间浏览（主区）+ 键详情与值查看（右栏）。
//!
//! 布局与交互范式沿用本仓 `DatabasePage`（`db.rs`）与 `TerminalPage`：
//! 左栏走页面内 [`SidebarPanel`]，右栏走 shell 级 `with_right_sidebar`。
//! 新建 / 编辑连接走**模态弹窗**（`duck_components::dialog` 一族），分页签
//! 常规 / SSL/TLS / SSH 隧道 / 高级 / 安全 / 键——信息架构对齐 zedis 的
//! 「添加新服务器」表单（`temp/zedis/src/views/servers/form.rs`）。
//!
//! **本增量范围**（`docs/redis-design.md` §6 的 M1.1 + M1.2）：
//! 连接档案 CRUD 与分组、连通测试、服务端版本、`SCAN` 键空间浏览（分页 / 过滤 /
//! 虚拟滚动 / 命名空间树）、键详情（类型 / TTL / 大小）、六种类型的值查看、
//! 字符串三档解码（文本 / JSON / Hex）、**五类结构化值编辑**（字符串 / 哈希字段 /
//! 列表元素 / 集合成员 / 有序集合成员）、设置与移除 TTL、重命名键、删除键，
//! 以及**分级锁写闸门**的全链路接入（灰显 + 说明原因 + 限时解锁）。
//! **未做**：破坏性命令的「输入连接名」二次确认（M1.3 收尾）、
//! 服务端面板族（M1.4~M1.6）——见设计文档 §6「当前进度」。
//!
//! 弹窗里**尚未接线**的档位一律以禁用控件 + 「M1.x 接入」说明呈现：
//! TLS 证书族与隧道建立（M1.5）、运行模式 / 哨兵与集群（M1.6）。
//!
//! # 两条实现纪律
//!
//! 1. **线程**：`duck-redis` 全部是阻塞 API，本页每条 Redis 操作都经
//!    [`Self::spawn_redis`] 丢进 `background_executor`，结果回主线程后再 `cx.notify()`；
//!    渲染路径上不做任何网络调用。
//! 2. **`SCAN` 不取每键类型**：逐键 `TYPE` 会在一个 500 键的页面上产生 500 次往返。
//!    所以列表里的类型图标**只在启用了类型过滤时**显示（此时 `SCAN TYPE` 已保证类型），
//!    其余情况用中性键图标；精确类型在右栏选中后取（一次往返）。
//!
//! # 文件划分（P3-3，2026-10-14 拆自单文件 `redis.rs`，零行为变更）
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `mod.rs` | 模块文档 / use / 常量 / 全部类型 / `RedisPage` / `impl Render` / 页面壳方法 |
//! | `helpers.rs` | 自由函数：锁、拒绝文案、连接档案读写、面板骨架、时间与标签 |
//! | `connect.rs` | 连接生命周期：新建 / 编辑 / 测试 / 激活 / 写锁 / 持久化 |
//! | `keys.rs` | 键浏览与键级操作：分页 / 树 / 选中 / 重命名 / TTL / 删除确认 |
//! | `writes.rs` | 五类结构化值的写入与删除 |
//! | `panels.rs` | 服务端面板的数据拉取与渲染（慢日志 / 客户端 / 配置 / 内存 / 监视器 / CLI） |
//! | `form.rs` | 连接编辑弹窗：六页签表单、页签外壳与页脚 |
//! | `browser.rs` | 键浏览主区与结果行渲染、视图切换、状态栏 |
//! | `sidebar.rs` | 左栏连接列表与 shell 级右栏详情 |
//! | `value.rs` | 值详情构造、三档解码、编码切换与值编辑器 |
//!
//! 两条口径（与 `pages/settings/` 相同）：
//! 1. 子模块靠 `use super::*;` 取父模块的私有 `use` 绑定与私有类型，不必复制 use 块；
//! 2. 子模块里 `pub(super)` == 可见于 `redis` 及其全部后代，与原来「模块私有」同一范围。

use std::collections::{HashMap, VecDeque};
use std::ops::RangeInclusive;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duck_components::{
    ButtonVariant, ConfirmHandler, DuckIcon, SidebarPanel, SidebarState, body_text, button,
    caption_heading,
    content::{description_list, description_row, empty_state, neutral_tag},
    dialog_top_offset, field_hint, field_label, form_field, form_gap, form_row, icon_button,
    meta_text, modal_drag, modal_drag_area, modal_offset, modal_title_row, notify, segmented_tabs,
    sidebar_navigation_button, switch_control, toolbar_action_button,
};
use duck_redis::capability::{AccessMode, Capability, DenyReason, Gate};
use duck_redis::config::{
    EnvTag, MAX_DB, RedisConfig, TlsMode, WritePolicy, delete_password, load_password,
    store_password,
};
use duck_redis::decode::{DecodeKind, Message as DecodeMessage, decode as redis_decode};
use duck_redis::scan::{ScanPage, TreeNode};
use duck_redis::value::KeyType;
use duck_redis::{
    AclProbe, ClientInfo, ConfigEntry, ConfirmKind, MonitorSession, RedisConnection, RedisError,
    RedisValue, ServerFeatures, SlowlogEntry, WRITE_UNLOCK_SECS, build_tree_with_depth,
    confirm_kind, is_destructive, parse_info_sections, should_auto_expand,
};
use duck_theme::{UnderMinute, active_theme, tf, tr, tr_relative_time, trf};
use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Div, Entity, InteractiveElement as _,
    IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::{
    Icon, WindowExt as _,
    dialog::{DialogButtonProps, DialogFooter},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};

mod browser;
mod connect;
mod form;
mod helpers;
mod keys;
mod panels;
mod sidebar;
mod value;
mod writes;

// 含自由函数 / 类型别名的子模块，名字要转出到父模块供兄弟模块取用。
use self::browser::*;
use self::form::*;
use self::helpers::*;
use self::value::*;

/// kv 单键：连接档案数组（结构体 → serde_json，范式同 `appearance` / `db_connections`）。
const KEY_CONNECTIONS: &str = "redis_connections";
/// 左栏宽度档位（与 `db.rs` 保持一致，两页切换时侧栏不跳宽）。
const SIDEBAR_WIDTH_RANGE: RangeInclusive<f32> = 184.0..=340.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 236.;
/// 键列表行高：与 `typography.sm` 的 18px 行高同源。
const KEY_ROW_HEIGHT: f32 = 24.;
/// 一次 `SCAN` 的 `COUNT` 的**出厂默认**（也是「继续加载」每次追加的目标量）。
///
/// 用户可在「设置 → Redis」里改这一档（`terminal` 同款：模块偏好单键 JSON）；
/// 连接档案里的 `scan_count` 仍然逐连接优先。
pub(super) const SCAN_PAGE: usize = 500;
/// 集合类值一页取多少元素。
const VALUE_PAGE: usize = 200;
/// 本地最多保留的键数：防止在大键空间上无限堆积（超出后停止追加并提示收窄过滤）。
const KEY_LIST_MAX: usize = 100_000;
/// 大于这个体积的值不自动渲染（避免一次性把几十 MB 文本推给布局）。
const VALUE_RENDER_LIMIT: usize = 1 << 20;

/// `SLOWLOG GET` 一次取多少条。
const SLOWLOG_PAGE: usize = 64;
/// 终端保留的输出行数上限（超出丢最旧的）。
const CLI_LINES_MAX: usize = 500;
/// 终端保留的命令历史条数上限。
const CLI_HISTORY_MAX: usize = 50;
/// `MONITOR` 累积行数上限（超出丢最旧的）——监控流是无限的，必须有界。
const MONITOR_LINES_MAX: usize = 2_000;
/// `MONITOR` 轮询节拍。
///
/// 50 毫秒是「拉」模型下的折中：真正等新行的耗时在 `POLL_TIMEOUT` 里，
/// 这里只是两轮之间的空档。太大会让爆发式输出显得卡顿。
const MONITOR_TICK: Duration = Duration::from_millis(50);
/// `MONITOR` 单轮最多收多少行（防止一次收太多把一帧拖长）。
const MONITOR_BATCH: usize = 500;
/// 面板里「标签」列的宽度（配置项的 name、慢日志的元信息）。
const PANEL_LABEL_WIDTH: f32 = 200.;
/// 慢日志行左侧元信息（序号 / 耗时 / 时间）的宽度。
const SLOWLOG_META_WIDTH: f32 = 220.;
/// 客户端列表里地址列与短列（库 / 空闲）的宽度。
const CLIENT_ADDR_WIDTH: f32 = 180.;
const CLIENT_SHORT_WIDTH: f32 = 72.;
/// 终端历史 chips 最多显示几条、每条最多几个字符。
const CLI_HISTORY_CHIPS: usize = 8;
const CLI_HISTORY_CHIP_CHARS: usize = 20;

/// 一条已建立的会话：阻塞连接包在互斥锁里，由后台线程短暂持有。
struct Session {
    conn: Arc<Mutex<RedisConnection>>,
    /// 服务端版本（连接时取一次）。
    version: String,
    /// ACL 写权限探测结果（决定严格只读）。
    acl: AclProbe,
    /// 生产档解锁窗口的到期时刻（`None` = 未解锁 / 已过期）。
    unlock_until: Option<Instant>,
    /// 能力探测结果（连接时跑一次，见 `RedisConnection::probe_features`）。
    ///
    /// **存在会话上而不是页面上**：切连接时各连接的能力集是独立的，
    /// 放在页面上会让「切到旧连接后仍按新连接的能力灰显面板」。
    features: Option<ServerFeatures>,
}

impl Session {
    /// 解锁窗口的剩余时长；已过期或未解锁返回 `None`。
    fn unlock_remaining(&self) -> Option<Duration> {
        let until = self.unlock_until?;
        until.checked_duration_since(Instant::now())
    }
}

/// 字符串值的解码档位展示名（档位本身住内核 `duck_redis::decode`）。
///
/// 对照 zedis：**显示的是解码后的内容，写回服务端的永远是原始字节**。
/// 所以切档只重算展示态，绝不改动 `KeyDetail::raw`。
///
/// 文案留在界面层：内核 `DecodeKind` 只带 `wire_name()`（也用作元素 id 后缀），
/// 不吐成品中文。
fn decode_label(kind: DecodeKind, cx: &App) -> &'static str {
    match kind {
        DecodeKind::Text => tr(cx, "文本"),
        DecodeKind::Json => tr(cx, "JSON"),
        DecodeKind::Hex => tr(cx, "Hex"),
        DecodeKind::Base64 => tr(cx, "Base64"),
        DecodeKind::Gzip => tr(cx, "GZIP"),
        DecodeKind::Jwt => tr(cx, "JWT"),
        DecodeKind::Timestamp => tr(cx, "时间戳"),
        DecodeKind::Url => tr(cx, "URL"),
        DecodeKind::Php => tr(cx, "PHP"),
    }
}

/// 内核文案（源串 + 参数）→ 当前语言的成品串。
///
/// 内核不吐成品中文，只上交 `(源串, 参数)`；这里按当前语言渲染一次存进展示态，
/// 渲染期不再现算（与「结果算一次存起来」同一条纪律）。
fn decode_message(message: &DecodeMessage, cx: &App) -> String {
    let args: Vec<&str> = message.args.iter().map(String::as_str).collect();
    tf(active_theme(cx).lang(), message.source, &args)
}

/// 右栏值面板的展示态（取一次存起来，渲染绝不现算）。
enum ValueView {
    /// 文本形态（UTF-8 可解码）。`notice` 是解码档位附带的说明行（如「GZIP 解压出
    /// 1234 字节」），没有该档说明时为 `None`。
    Text {
        rendered: String,
        notice: Option<String>,
        bytes: usize,
    },
    /// 二进制形态（hex 预览）。
    Binary { hex: String, bytes: usize },
    /// JSON 档解析失败（明确报错，而不是悄悄退回原文）。
    JsonFailed { error: String, bytes: usize },
    /// 体积过大，未自动加载。
    TooLarge { bytes: usize },
    /// 哈希表。
    Hash(Vec<(String, String)>),
    /// 列表。
    List(Vec<String>),
    /// 集合。
    Set(Vec<String>),
    /// 有序集合。
    ZSet(Vec<(String, String)>),
    /// 流。
    Stream(Vec<(String, Vec<(String, String)>)>),
    /// 键已不在（选中后到读取之间被删掉了）。
    Missing,
    /// 该类型还没有专用查看器（模块类型要 `DUMP`，计划 M1.6 做）。
    Unsupported(KeyType),
}

/// 右栏的完整状态。
struct KeyDetail {
    /// 键名。
    key: String,
    /// 精确类型（选中后取一次）。
    key_type: KeyType,
    /// 剩余 TTL 秒数（`-1` 无过期，`-2` 不存在）。
    ttl: i64,
    /// 元素条数 / 字节数（按类型含义不同）。
    size: usize,
    /// 是否被截断（一页装不下）。
    truncated: bool,
    /// 字符串值的**原始字节**：切解码档位时用它重算展示态，避免多一次网络往返。
    /// 非字符串类型为空——集合类没有「解码」这回事。
    raw: Vec<u8>,
    /// 值展示态。
    view: ValueView,
}

/// 弹窗页签（顺序对齐 zedis 的「添加新服务器」表单）。
const FORM_TABS: [&str; 6] = ["常规", "SSL/TLS", "SSH 隧道", "高级", "安全", "键"];
/// 弹窗宽度：六个页签 + 两列表单的最小舒适宽度。
const FORM_DIALOG_WIDTH: f32 = 680.;

/// 二次确认弹窗宽度：正文只有一两句「要动什么」，比表单弹窗窄得多。
const CONFIRM_DIALOG_WIDTH: f32 = 440.;

/// 新建 / 编辑连接的弹窗状态。
///
/// 挂成 [`Entity`] 而不是页面字段：`open_dialog` 的内容闭包是 `Fn`（重排会重跑），
/// 拿不到 `&mut RedisPage`，但可以克隆 `Entity` 自由读写。
struct EditForm {
    /// `None` = 新建。
    id: Option<String>,
    /// 当前页签下标。
    tab: usize,

    // ── 常规 ────────────────────────────────────────────────────────────
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    acl_user: Entity<InputState>,
    password: Entity<InputState>,
    description: Entity<InputState>,
    group: Entity<InputState>,

    // ── SSL/TLS（M1 仅档位可选，证书族 M1.5 接入）────────────────────────
    tls: TlsMode,

    // ── SSH 隧道（档案可先填，M1.5 前建连会明确报错）──────────────────────
    ssh_enabled: bool,
    ssh_host: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_password: Entity<InputState>,
    ssh_remote_host: Entity<InputState>,

    // ── 高级 ────────────────────────────────────────────────────────────
    db: Entity<InputState>,

    // ── 安全 ────────────────────────────────────────────────────────────
    env_tag: EnvTag,
    write_policy: WritePolicy,
    require_confirm: bool,

    // ── 键 ──────────────────────────────────────────────────────────────
    separator: Entity<InputState>,
    scan_count: Entity<InputState>,
    /// 键树最大层数（空 = 内核缺省）。
    tree_depth: Entity<InputState>,
    /// 键树自动展开阈值（空 / 0 = 不自动展开）。
    auto_expand: Entity<InputState>,
}

impl EditForm {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            id: None,
            tab: 0,
            name: text_input(window, cx, tr(cx, "输入名称")),
            host: text_input(window, cx, tr(cx, "host、host:port、… 或 /path/to.sock")),
            port: text_input(window, cx, tr(cx, "6379")),
            acl_user: text_input(window, cx, tr(cx, "输入用户名，仅用于 Redis 6.0+")),
            password: text_input(window, cx, tr(cx, "输入密码")),
            description: text_input(window, cx, tr(cx, "输入描述信息")),
            group: text_input(window, cx, tr(cx, "例如 团队 A / 个人 / 客户 X")),
            tls: TlsMode::Disable,
            ssh_enabled: false,
            ssh_host: text_input(window, cx, tr(cx, "输入 SSH 地址")),
            ssh_port: text_input(window, cx, tr(cx, "22")),
            ssh_user: text_input(window, cx, tr(cx, "输入 SSH 用户名")),
            ssh_password: text_input(window, cx, tr(cx, "输入 SSH 密码")),
            ssh_remote_host: text_input(window, cx, tr(cx, "留空则连跳板机本机")),
            db: text_input(window, cx, tr(cx, "留空则打开 0 号库")),
            env_tag: EnvTag::None,
            write_policy: WritePolicy::default(),
            require_confirm: false,
            separator: text_input(window, cx, tr(cx, "留空则使用 :")),
            scan_count: text_input(window, cx, tr(cx, "留空则使用设置中的默认值")),
            tree_depth: text_input(window, cx, tr(cx, "留空则使用 8 层")),
            auto_expand: text_input(window, cx, tr(cx, "留空或 0 表示不自动展开")),
        }
    }

    /// 由既有档案填表（编辑态）。口令永远留空——读回口令会把它带进内存与界面。
    fn from_config(cfg: &RedisConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tunnel = cfg.ssh_tunnel.clone().unwrap_or_default();
        Self {
            id: Some(cfg.id.clone()),
            tab: 0,
            name: text_input(window, cx, cfg.name.clone()),
            host: text_input(window, cx, cfg.host.clone()),
            port: text_input(
                window,
                cx,
                cfg.port.map(|p| p.to_string()).unwrap_or_default(),
            ),
            acl_user: text_input(window, cx, cfg.acl_user.clone()),
            password: text_input(window, cx, tr(cx, "留空表示保持既有口令不变")),
            description: text_input(window, cx, cfg.description.clone().unwrap_or_default()),
            group: text_input(window, cx, cfg.group.clone().unwrap_or_default()),
            tls: cfg.tls,
            ssh_enabled: cfg.ssh_tunnel.is_some(),
            ssh_host: text_input(window, cx, tunnel.host.clone()),
            ssh_port: text_input(
                window,
                cx,
                tunnel.port.map(|p| p.to_string()).unwrap_or_default(),
            ),
            ssh_user: text_input(window, cx, tunnel.user.clone()),
            ssh_password: text_input(window, cx, tr(cx, "留空表示保持既有口令不变")),
            ssh_remote_host: text_input(window, cx, tunnel.remote_host.clone()),
            db: text_input(window, cx, cfg.db.to_string()),
            env_tag: cfg.env_tag,
            write_policy: cfg.write_policy,
            require_confirm: cfg.require_confirm_writes,
            separator: text_input(window, cx, cfg.key_separator.clone()),
            scan_count: text_input(
                window,
                cx,
                cfg.scan_count.map(|n| n.to_string()).unwrap_or_default(),
            ),
            tree_depth: text_input(
                window,
                cx,
                cfg.max_key_tree_depth
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
            ),
            auto_expand: text_input(
                window,
                cx,
                cfg.auto_expand_threshold
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
            ),
        }
    }

    fn input_value(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_owned()
    }

    /// 可选数字输入：空 → `None`，非数字 → 错误（端口 u16、扫描数 u32 各取所需）。
    fn optional_num<T: std::str::FromStr>(
        input: &Entity<InputState>,
        cx: &App,
    ) -> Result<Option<T>, String> {
        let text = Self::input_value(input, cx);
        if text.is_empty() {
            return Ok(None);
        }
        text.parse::<T>()
            .map(Some)
            .map_err(|_| "not a number".into())
    }

    /// 表单 → 档案。解析失败返回给用户看的英文原因（内核错误文案是英文）。
    fn to_config(&self, cx: &App) -> Result<RedisConfig, String> {
        let port = Self::optional_num::<u16>(&self.port, cx)?;
        let db = Self::input_value(&self.db, cx);
        let db = if db.is_empty() {
            0
        } else {
            db.parse::<u32>()
                .map_err(|_| "db is not a number".to_owned())?
        };
        let scan_count = Self::optional_num::<u32>(&self.scan_count, cx)?;
        let ssh_tunnel = if self.ssh_enabled {
            Some(duck_redis::SshTunnelConfig {
                host: Self::input_value(&self.ssh_host, cx),
                port: Self::optional_num::<u16>(&self.ssh_port, cx)?,
                user: Self::input_value(&self.ssh_user, cx),
                remote_host: Self::input_value(&self.ssh_remote_host, cx),
            })
        } else {
            None
        };
        let separator = Self::input_value(&self.separator, cx);
        Ok(RedisConfig {
            id: self.id.clone().unwrap_or_else(new_connection_id),
            name: Self::input_value(&self.name, cx),
            host: Self::input_value(&self.host, cx),
            port,
            db,
            acl_user: Self::input_value(&self.acl_user, cx),
            description: {
                let description = Self::input_value(&self.description, cx);
                (!description.is_empty()).then_some(description)
            },
            tls: self.tls,
            env_tag: self.env_tag,
            write_policy: self.write_policy,
            scan_count: scan_count.filter(|count| *count > 0),
            require_confirm_writes: self.require_confirm,
            key_separator: if separator.is_empty() {
                ":".to_owned()
            } else {
                separator
            },
            // 两档都做「0 = 用内核缺省」的归一：深度 0 会让树退化，展开阈值 0
            // 的含义本来就是「不自动展开」，与「没填」同义，都落成 `None` 更干净。
            max_key_tree_depth: Self::optional_num::<u32>(&self.tree_depth, cx)?
                .filter(|depth| *depth > 0),
            auto_expand_threshold: Self::optional_num::<u32>(&self.auto_expand, cx)?
                .filter(|threshold| *threshold > 0),
            group: {
                let group = Self::input_value(&self.group, cx);
                (!group.is_empty()).then_some(group)
            },
            ssh_tunnel,
        })
    }
}

/// Redis 页。
pub struct RedisPage {
    /// 左栏折叠 / 展开状态（shell 顶栏的侧栏开关也读它）。
    sidebar: Entity<SidebarState>,
    /// 连接搜索框。
    search: Entity<InputState>,
    /// 键名过滤（`SCAN MATCH` 的模式串）。
    pattern: Entity<InputState>,
    /// 字符串值编辑缓冲。
    edit_buffer: Entity<InputState>,
    /// 字符串值的解码档位（文本 / JSON / Hex）。
    decode: DecodeKind,

    /// 哈希字段编辑：字段名。
    hash_field: Entity<InputState>,
    /// 哈希字段编辑：字段值。
    hash_value: Entity<InputState>,
    /// 列表元素编辑缓冲。
    list_value: Entity<InputState>,
    /// 集合成员编辑缓冲。
    set_member: Entity<InputState>,
    /// 有序集合成员编辑缓冲。
    zset_member: Entity<InputState>,
    /// 有序集合分数编辑缓冲。
    zset_score: Entity<InputState>,
    /// 设置 TTL 的秒数输入。
    ttl_input: Entity<InputState>,
    /// 重命名键的目标键名输入。
    rename_input: Entity<InputState>,
    /// 列表底部表单当前在改哪一行（`Some` = 「按位置改」态，`None` = 「追加」态）。
    list_editing: Option<i64>,

    /// 自引用：右栏 / 列表渲染闭包要把点击送回来。
    view: WeakEntity<Self>,
    /// 订阅句柄（延长生命周期）。
    _subscriptions: Vec<Subscription>,

    /// 连接档案（kv 读入；保存时写回）。
    connections: Vec<RedisConfig>,
    /// 正在新建 / 编辑的连接弹窗（`None` = 没开着）。
    editing: Option<Entity<EditForm>>,
    /// 已建立的会话。
    sessions: HashMap<String, Session>,
    /// 当前激活的连接 id。
    active: Option<String>,

    /// 已扫描到的键名。
    keys: Vec<String>,
    /// `SCAN` 游标（`0` = 遍历结束）。
    scan_cursor: u64,
    /// 是否已遍历完整个键空间。
    scan_done: bool,
    /// 当前库的键总数（`DBSIZE`）。
    dbsize: u64,
    /// 类型过滤（`None` = 不过滤）。
    type_filter: Option<KeyType>,
    /// 选中的键下标。
    selected: Option<usize>,
    /// 右栏键详情。
    detail: Option<KeyDetail>,
    /// 状态栏文案。
    status: SharedString,
    /// 有后台操作在跑。
    busy: bool,
    /// 本页已提示过「停止追加」——同一条提示只说一次。
    limit_announced: bool,
    /// 键列表形态：`true` = 命名空间树，`false` = 平铺列表。
    tree_mode: bool,
    /// 树节点的展开状态**覆盖**：键 = 节点路径。
    ///
    /// 只记用户显式切过的节点；没记过的一律按连接档案的「自动展开阈值」推导
    /// （见 [`should_auto_expand`]）。这样改阈值能立刻反映到没碰过的节点上。
    tree_overrides: HashMap<String, bool>,
    /// 树模式的展平行缓存。
    ///
    /// **只在键集 / 展开状态 / 树参数变化时重建，渲染路径只读它**——
    /// 百万键下每次重绘都重算一遍展平会把滚动拖垮（设计文档 §4.5 的性能判据）。
    tree_rows: Vec<TreeRow>,

    // ── 服务端观测（M1.4） ──────────────────────────────────────────────
    /// 连接内主视图（数据 / 服务端）。
    main_view: MainView,
    /// 当前服务端面板。
    panel: ServerPanel,
    /// 面板数据。
    panel_data: PanelData,
    /// 面板正在拉取（刷新按钮据此置灰）。
    panel_busy: bool,
    /// 配置过滤串（`CONFIG GET <pattern>`）。
    config_pattern: Entity<InputState>,
    /// 待写入的配置项名（点某一行时回填）。
    config_name: Entity<InputState>,
    /// 待写入的配置项值。
    config_value: Entity<InputState>,
    /// 内存占用查询的键名（`MEMORY USAGE`）。
    memory_key: Entity<InputState>,
    /// 终端输入行。
    cli_input: Entity<InputState>,
    /// 终端输出（只增，上限 [`CLI_LINES_MAX`]）。
    cli_lines: Vec<CliLine>,
    /// 终端命令历史（最近 [`CLI_HISTORY_MAX`] 条，去重后置顶）。
    cli_history: Vec<String>,
    /// 多行输入拆出来的待执行队列（逐条串行，避免共用连接上交错）。
    cli_queue: VecDeque<String>,
    /// `MONITOR` 的停止位。**只置位、不丢任务**：任务是阻塞读的，drop 会把
    /// 那条专用连接留在半路；置位后读循环自己走完 `break` 并正常析构连接。
    monitor_stop: Option<Arc<AtomicBool>>,
    /// `MONITOR` 轮询循环的生命周期轮次。每次开启自增；
    /// 循环收工时只在「自己那一轮仍是当前轮」的才回落状态，
    /// 免得一次快速「停→开」后，旧循环把新会话的「运行中」抹掉。
    monitor_generation: u64,
    /// `MONITOR` 累积的行（上限 [`MONITOR_LINES_MAX`]）。
    monitor_lines: VecDeque<String>,
    /// `MONITOR` 是否在跑（决定按钮形态）。
    monitor_running: bool,
    /// `MONITOR` 轮询任务句柄；丢掉即停止循环。
    monitor_task: Option<Task<()>>,
    /// `MONITOR` 因错误中断时的原因（面板上要说明，不能只是静默停了）。
    monitor_error: Option<String>,
    /// 终端输出的滚动句柄（新行进来要贴底）。
    cli_scroll: ScrollHandle,
    /// 监控输出的滚动句柄（同上）。
    monitor_scroll: ScrollHandle,
}

/// 需要二次确认的破坏性动作（连接档案开启「写入确认」时生效）。
///
/// 只在 `require_confirm_writes` 为真时介入；关掉时直接执行——**确认是用户开的
/// 开关，不是我们强加的门槛**。参数在构造时就被固定下来（尤其 `RenameKey` 的目标
/// 键名），确认期间不再回读输入框：否则用户改完输入框再点确认，动的就不是他看到的那个。
#[derive(Clone, PartialEq, Eq)]
enum PendingAction {
    /// 删除键。
    DeleteKey { key: String },
    /// 重命名键。
    RenameKey { from: String, to: String },
    /// 移除键的过期时间。
    ClearTtl { key: String },
    /// 删哈希字段。
    DeleteHashField { key: String, field: String },
    /// 删列表元素（按位置）。
    DeleteListItem { key: String, index: i64 },
    /// 删集合成员。
    DeleteSetMember { key: String, member: String },
    /// 删有序集合成员。
    DeleteZSetMember { key: String, member: String },
    /// 执行一条原始命令（redis-cli 终端）。
    RawCommand { line: String },
    /// 写一个服务端配置项（`CONFIG SET`）。
    ConfigSet { name: String, value: String },
    /// 清空慢日志（`SLOWLOG RESET`）。
    SlowlogReset,
    /// 强制断开一条客户端连接（`CLIENT KILL ADDR`）。
    ClientKill { addr: String },
}

impl PendingAction {
    /// 确认框标题。
    fn title(&self, cx: &App) -> &'static str {
        match self {
            Self::DeleteKey { .. } => tr(cx, "删除键"),
            Self::RenameKey { .. } => tr(cx, "重命名键"),
            Self::ClearTtl { .. } => tr(cx, "移除过期时间"),
            Self::DeleteHashField { .. } => tr(cx, "删除哈希字段"),
            Self::DeleteListItem { .. } => tr(cx, "删除列表元素"),
            Self::DeleteSetMember { .. } => tr(cx, "删除集合成员"),
            Self::DeleteZSetMember { .. } => tr(cx, "删除有序集合成员"),
            Self::RawCommand { .. } => tr(cx, "执行原始命令"),
            Self::ConfigSet { .. } => tr(cx, "修改服务端配置"),
            Self::SlowlogReset => tr(cx, "清空慢日志"),
            Self::ClientKill { .. } => tr(cx, "断开客户端连接"),
        }
    }

    /// 确认框正文：把「要动什么」写清楚，别让用户凭记忆确认。
    fn message(&self, cx: &App) -> String {
        match self {
            Self::DeleteKey { key } => trf(cx, "将永久删除键 {}，此操作不可撤销。", &[key]),
            Self::RenameKey { from, to } => trf(
                cx,
                "将把 {} 重命名为 {}；若目标键已存在，它会被覆盖。",
                &[from, to],
            ),
            Self::ClearTtl { key } => trf(cx, "将移除 {} 的过期时间，该键会永不过期。", &[key]),
            Self::DeleteHashField { key, field } => {
                trf(cx, "将从哈希 {} 中删除字段 {}。", &[key, field])
            }
            Self::DeleteListItem { key, index } => trf(
                cx,
                "将删除列表 {} 中第 {} 个元素。",
                &[key, &index.to_string()],
            ),
            Self::DeleteSetMember { key, member } => {
                trf(cx, "将从集合 {} 中删除成员 {}。", &[key, member])
            }
            Self::DeleteZSetMember { key, member } => {
                trf(cx, "将从有序集合 {} 中删除成员 {}。", &[key, member])
            }
            Self::RawCommand { line } => trf(cx, "将向服务端发送命令：{}", &[line]),
            Self::ConfigSet { name, value } => trf(
                cx,
                "将把服务端配置 {} 改为 {}；多数参数立即生效并影响所有客户端。",
                &[name, value],
            ),
            Self::SlowlogReset => tr(cx, "将清空服务端的慢日志记录。").to_owned(),
            Self::ClientKill { addr } => trf(cx, "将强制断开客户端 {} 的连接。", &[addr]),
        }
    }
}

// ----------------------------------------------------------------------
// 连接内主视图与服务端观测（M1.4）
// ----------------------------------------------------------------------

/// 连接内的主视图：数据浏览 ↔ 服务端观测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainView {
    /// 键空间浏览与值编辑（M1.2 / M1.3）。
    Data,
    /// 服务端观测面板族（M1.4）。
    Server,
}

/// 服务端观测的具体面板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerPanel {
    /// `INFO` 分节原始浏览器。
    Info,
    /// `SLOWLOG GET` 慢日志。
    Slowlog,
    /// `CLIENT LIST` 客户端连接。
    Clients,
    /// `CONFIG GET` / `CONFIG SET` 配置。
    Config,
    /// `MEMORY DOCTOR` / `MEMORY STATS` 内存。
    Memory,
    /// `MONITOR` 实时命令流（专用连接）。
    Monitor,
    /// redis-cli 终端（任意命令）。
    Cli,
}

impl ServerPanel {
    /// 全部面板（页签顺序即此顺序）。
    const ALL: [Self; 7] = [
        Self::Info,
        Self::Slowlog,
        Self::Clients,
        Self::Config,
        Self::Memory,
        Self::Monitor,
        Self::Cli,
    ];

    /// 页签文案。
    fn label(self, cx: &App) -> &'static str {
        match self {
            Self::Info => tr(cx, "概览"),
            Self::Slowlog => tr(cx, "慢日志"),
            Self::Clients => tr(cx, "客户端"),
            Self::Config => tr(cx, "配置"),
            Self::Memory => tr(cx, "内存"),
            Self::Monitor => tr(cx, "实时监控"),
            Self::Cli => tr(cx, "终端"),
        }
    }

    /// 该面板依赖的服务端命令。
    ///
    /// 能力探测（[`ServerFeatures`]）说这里任何一条不存在，面板就**灰显并说明原因**
    /// ——这是设计文档 §4.7 的硬口径：不做品牌特判，只按「缺什么命令」判。
    /// 终端与实时监控没有依赖清单：终端本身就是「有什么用什么」。
    fn required_commands(self) -> &'static [&'static str] {
        match self {
            Self::Info | Self::Cli => &[],
            Self::Slowlog => &["slowlog"],
            Self::Clients => &["client"],
            Self::Config => &["config"],
            Self::Memory => &["memory"],
            Self::Monitor => &["monitor"],
        }
    }
}

/// 服务端面板的已加载数据（按需拉取，拉完存这里；**渲染路径不现算**）。
#[derive(Default)]
struct PanelData {
    /// `INFO` 的分节结构。
    info_sections: Vec<(String, Vec<(String, String)>)>,
    /// 慢日志条目。
    slowlog: Vec<SlowlogEntry>,
    /// 慢日志总条数（`SLOWLOG LEN`）。
    slowlog_len: u64,
    /// 客户端连接列表。
    clients: Vec<ClientInfo>,
    /// 配置项列表。
    config: Vec<ConfigEntry>,
    /// 内存诊断报告（`MEMORY DOCTOR`）。
    memory_doctor: String,
    /// 内存统计（`MEMORY STATS`）。
    memory_stats: Vec<(String, String)>,
    /// 单个键的内存占用查询结果（`MEMORY USAGE`）。
    memory_usage: Option<(String, Option<u64>)>,
}

/// 终端输出的一行。
#[derive(Clone, PartialEq, Eq)]
struct CliLine {
    /// `true` = 用户输入的命令行；`false` = 服务端应答。
    command: bool,
    /// `true` = 失败应答（标红）。
    error: bool,
    /// 正文。
    text: String,
}

/// 树模式下一行（展平后，供 `uniform_list` 使用）。
#[derive(Clone)]
struct TreeRow {
    /// 缩进层数（0 = 顶层）。
    depth: usize,
    /// 显示名。
    label: String,
    /// 节点路径（展开状态的键）。
    path: String,
    /// 该节点子树下的键总数。
    key_count: usize,
    /// 是否可折叠（有子节点）。
    has_children: bool,
    /// 当前是否展开。
    expanded: bool,
    /// 该行对应的实体键名（纯中间节点为 `None`，点击只折叠不选中）。
    leaf_key: Option<String>,
}

/// 把命名空间树展平成行列表（深度优先，只展开处于展开态的节点）。
///
/// 纯函数：`overrides` 是用户显式的展开 / 收起记录，未记录时按
/// `auto_expand` 阈值推导（[`should_auto_expand`]）。
fn flatten_tree(
    nodes: &[TreeNode],
    depth: usize,
    prefix: &str,
    separator: &str,
    overrides: &HashMap<String, bool>,
    auto_expand: usize,
    out: &mut Vec<TreeRow>,
) {
    for node in nodes {
        let path = if prefix.is_empty() {
            node.label.clone()
        } else {
            format!("{prefix}{separator}{}", node.label)
        };
        let has_children = !node.children.is_empty();
        let expanded = overrides
            .get(&path)
            .copied()
            .unwrap_or_else(|| should_auto_expand(node, auto_expand));
        // 有键：每个键成一行（限深时同一节点下可能挤着多个键）。
        // 折叠三角只挂在第一行上，否则同一节点会出现两个三角。
        let multi = node.keys.len() > 1;
        for (index, key) in node.keys.iter().enumerate() {
            let first = index == 0;
            out.push(TreeRow {
                depth,
                label: if multi {
                    key.clone()
                } else {
                    node.label.clone()
                },
                path: if first {
                    path.clone()
                } else {
                    format!("{path}\u{1}{index}")
                },
                key_count: 1,
                has_children: has_children && first,
                expanded: expanded && first,
                leaf_key: Some(key.clone()),
            });
        }
        // 无键的纯中间节点也要一行，否则这个命名空间在界面上不存在。
        if node.keys.is_empty() {
            out.push(TreeRow {
                depth,
                label: node.label.clone(),
                path: path.clone(),
                key_count: node.total_keys(),
                has_children,
                expanded,
                leaf_key: None,
            });
        }
        if has_children && expanded {
            flatten_tree(
                &node.children,
                depth + 1,
                &path,
                separator,
                overrides,
                auto_expand,
                out,
            );
        }
    }
}

impl Render for RedisPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = self.render_browser(cx);

        let mut panel = SidebarPanel::new("redis-content", &self.sidebar, content, window, cx);
        if panel.is_sidebar_visible() {
            panel = panel.sidebar(self.render_sidebar(cx));
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(div().flex_1().min_h_0().min_w_0().child(panel))
            .child(self.render_status_bar(cx))
    }
}

impl RedisPage {
    /// 创建页面：读连接档案、展开左栏。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| {
            SidebarState::new(
                px(SIDEBAR_DEFAULT_WIDTH),
                px(*SIDEBAR_WIDTH_RANGE.start())..px(*SIDEBAR_WIDTH_RANGE.end()),
                cx,
            )
        });
        let search = text_input(window, cx, tr(cx, "搜索连接…"));
        let pattern = text_input(window, cx, tr(cx, "过滤键名，如 user:* （留空表示全部）"));
        let edit_buffer = text_input(window, cx, tr(cx, "字符串值"));
        let hash_field = text_input(window, cx, tr(cx, "字段名"));
        let hash_value = text_input(window, cx, tr(cx, "字段值"));
        let list_value = text_input(window, cx, tr(cx, "元素值"));
        let set_member = text_input(window, cx, tr(cx, "成员"));
        let zset_member = text_input(window, cx, tr(cx, "成员"));
        let zset_score = text_input(window, cx, tr(cx, "分数，如 1 或 1.5"));
        let ttl_input = text_input(window, cx, tr(cx, "秒数，如 3600"));
        let rename_input = text_input(window, cx, tr(cx, "新的键名"));
        let config_pattern = text_input(window, cx, tr(cx, "过滤参数名，如 maxmemory*"));
        let config_name = text_input(window, cx, tr(cx, "参数名"));
        let config_value = text_input(window, cx, tr(cx, "新值"));
        let memory_key = text_input(window, cx, tr(cx, "键名"));
        let cli_input = text_input(window, cx, tr(cx, "输入命令，如 INFO server"));

        // 搜索 / 过滤框内容变化都要重绘（列表按它们过滤）。
        let mut subscriptions = Vec::new();
        for input in [&search, &pattern] {
            subscriptions.push(cx.subscribe(
                input,
                |_this: &mut Self,
                 _source: Entity<InputState>,
                 event: &InputEvent,
                 cx: &mut Context<Self>| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ));
        }
        // 终端里回车即执行。订阅回调拿不到 `Window`，所以借 `on_notify_window`
        // 找当前窗口（与 `spawn_redis` 同一条路），再回头调 `submit_cli`。
        subscriptions.push(cx.subscribe(
            &cli_input,
            |_this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let entity = cx.entity();
                    crate::update::on_notify_window(cx, move |window, cx| {
                        let _ = entity.update(cx, |this, cx| this.submit_cli(window, cx));
                    });
                }
            },
        ));

        Self {
            sidebar,
            search,
            pattern,
            edit_buffer,
            decode: DecodeKind::default(),
            hash_field,
            hash_value,
            list_value,
            set_member,
            zset_member,
            zset_score,
            ttl_input,
            rename_input,
            list_editing: None,
            view: cx.entity().downgrade(),
            _subscriptions: subscriptions,
            connections: load_connections(cx),
            editing: None,
            sessions: HashMap::new(),
            active: None,
            keys: Vec::new(),
            scan_cursor: 0,
            scan_done: true,
            dbsize: 0,
            type_filter: None,
            selected: None,
            detail: None,
            status: tr(cx, "就绪").into(),
            busy: false,
            limit_announced: false,
            tree_mode: false,
            tree_overrides: HashMap::new(),
            tree_rows: Vec::new(),
            main_view: MainView::Data,
            panel: ServerPanel::Info,
            panel_data: PanelData::default(),
            panel_busy: false,
            config_pattern,
            config_name,
            config_value,
            memory_key,
            cli_input,
            cli_lines: Vec::new(),
            cli_history: Vec::new(),
            cli_queue: VecDeque::new(),
            monitor_stop: None,
            monitor_generation: 0,
            monitor_lines: VecDeque::new(),
            monitor_running: false,
            monitor_task: None,
            monitor_error: None,
            cli_scroll: ScrollHandle::new(),
            monitor_scroll: ScrollHandle::new(),
        }
    }

    /// 供 shell 挂左栏折叠态。
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
    }

    // ------------------------------------------------------------------
    // 后台任务包装：所有阻塞 Redis 调用都从这里走。
    // ------------------------------------------------------------------

    /// 取激活会话的连接句柄（渲染路径上只用它判「有没有连上」，不发命令）。
    fn active_session(&self) -> Option<&Session> {
        let id = self.active.as_ref()?;
        self.sessions.get(id)
    }

    /// 取激活连接的档案。
    fn active_config(&self) -> Option<&RedisConfig> {
        let id = self.active.as_ref()?;
        self.connections.iter().find(|cfg| &cfg.id == id)
    }

    /// 当前的分级锁写闸门；没有激活连接时为 `None`。
    fn gate(&self) -> Option<Gate> {
        let cfg = self.active_config()?;
        let session = self.active_session()?;
        Some(Gate::from_policy(
            cfg.write_policy,
            cfg.env_tag,
            session.acl.acl_read_only(),
            session.unlock_remaining(),
        ))
    }

    /// 当前访问模式（状态栏展示用）。
    fn access_mode(&self) -> AccessMode {
        self.gate()
            .map_or(AccessMode::ReadWrite, |gate| gate.mode())
    }

    /// 本次会话的 `SCAN COUNT`：连接档案里的覆盖值优先，否则用设置页里的全局档
    ///（未设置时回落到 [`SCAN_PAGE`]）。
    fn scan_page_size(&self, cx: &App) -> usize {
        self.active_config()
            .and_then(|cfg| cfg.scan_count)
            .map_or_else(
                || super::module_settings::redis_settings(cx).scan_page as usize,
                |count| count as usize,
            )
    }

    // ------------------------------------------------------------------
    // 连接档案 CRUD。
    // ------------------------------------------------------------------

    /// 激活连接的能力探测结果（未连上时为 `None`）。
    fn features(&self) -> Option<&ServerFeatures> {
        let id = self.active.as_ref()?;
        self.sessions.get(id)?.features.as_ref()
    }

    /// 某个面板在本连接上不可用时，返回**缺失的命令名**（空 = 可用）。
    ///
    /// 没有探测结果（还没连上 / 探测失败）时一律返回空——「不知道」不是「不支持」，
    /// 这是 [`ServerFeatures`] 定下的取舍，界面必须照办，否则老服务端会被全面禁掉。
    fn panel_missing(&self, panel: ServerPanel) -> Vec<&'static str> {
        let Some(features) = self.features() else {
            return Vec::new();
        };
        panel
            .required_commands()
            .iter()
            .copied()
            .filter(|command| !features.supports(command))
            .collect()
    }

    /// 切主视图；切入服务端视图时按需拉一次当前面板的数据。
    fn set_main_view(&mut self, view: MainView, window: &mut Window, cx: &mut Context<Self>) {
        if self.main_view == view {
            return;
        }
        // 离开服务端视图就停掉监控流：一条专用连接不该在看不见的视图里白跑。
        if self.main_view == MainView::Server {
            self.stop_monitor(cx);
        }
        self.main_view = view;
        if view == MainView::Server {
            self.load_panel(window, cx);
        }
        cx.notify();
    }

    /// 切服务端面板。
    fn set_server_panel(
        &mut self,
        panel: ServerPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.panel == panel {
            return;
        }
        // 面板级的「专用连接」只在它自己的面板里活着。
        if self.panel == ServerPanel::Monitor {
            self.stop_monitor(cx);
        }
        self.panel = panel;
        self.load_panel(window, cx);
        cx.notify();
    }

    /// 检查一个动作能不能做（分级锁写闸门）。
    fn check(&self, capability: Capability) -> Result<(), DenyReason> {
        match self.gate() {
            Some(gate) => gate.check(capability),
            // 没有会话时所有动作都无处执行；报「未连接」由调用点处理。
            None => Err(DenyReason::ConnectionReadOnly),
        }
    }
}
