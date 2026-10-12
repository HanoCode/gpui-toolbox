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
use duck_theme::{active_theme, tf, tr, trf};
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

/// kv 单键：连接档案数组（结构体 → serde_json，范式同 `appearance` / `db_connections`）。
const KEY_CONNECTIONS: &str = "redis_connections";
/// 左栏宽度档位（与 `db.rs` 保持一致，两页切换时侧栏不跳宽）。
const SIDEBAR_WIDTH_RANGE: RangeInclusive<f32> = 184.0..=340.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 236.;
/// 键列表行高：与 `typography.sm` 的 18px 行高同源。
const KEY_ROW_HEIGHT: f32 = 24.;
/// 一次 `SCAN` 的 `COUNT`（也是「继续加载」每次追加的目标量）。
const SCAN_PAGE: usize = 500;
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

    /// 把阻塞调用丢进后台执行器，完成后回主线程跑 `done`。
    fn spawn_redis<T, F, D>(&mut self, cx: &mut Context<Self>, work: F, done: D)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    {
        self.busy = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move { work() });
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

    /// 本次会话的 `SCAN COUNT`：连接档案里的覆盖值优先，否则用界面默认档。
    fn scan_page_size(&self) -> usize {
        self.active_config()
            .and_then(|cfg| cfg.scan_count)
            .map_or(SCAN_PAGE, |count| count as usize)
    }

    // ------------------------------------------------------------------
    // 连接档案 CRUD。
    // ------------------------------------------------------------------

    /// 保存弹窗里的连接档案；返回 `false` 表示校验未过（弹窗保持打开）。
    fn save_form(
        &mut self,
        form: &Entity<EditForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let cfg = match form.read(cx).to_config(cx) {
            Ok(cfg) => cfg,
            Err(message) => {
                notify::warning(SharedString::from(message), window, cx);
                return false;
            }
        };
        if let Err(error) = cfg.validate() {
            notify::warning(SharedString::from(error.to_string()), window, cx);
            return false;
        }
        // 口令：非空才写凭据文件（编辑时留空 = 保持既有口令不变）。
        let password = EditForm::input_value(&form.read(cx).password, cx);
        if !password.is_empty()
            && let Err(error) = store_password(&cfg.id, &password)
        {
            notify::error(
                trf(cx, "口令保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return false;
        }
        // SSH 口令单独一条凭据记录；开关没开就删掉旧的，别留死凭据。
        if form.read(cx).ssh_enabled {
            let ssh_password = EditForm::input_value(&form.read(cx).ssh_password, cx);
            if !ssh_password.is_empty()
                && let Err(error) = duck_redis::config::store_ssh_password(&cfg.id, &ssh_password)
            {
                notify::error(
                    trf(cx, "口令保存失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                );
                return false;
            }
        } else {
            let _ = duck_redis::config::delete_ssh_password(&cfg.id);
        }
        match self.connections.iter_mut().find(|item| item.id == cfg.id) {
            Some(existing) => *existing = cfg,
            None => self.connections.push(cfg),
        }
        self.persist_connections(cx);
        self.editing = None;
        // 档案里改的可能正是树参数（分隔符 / 深度 / 展开阈值），树要跟着重建。
        self.rebuild_tree();
        notify::success(tr(cx, "连接已保存"), window, cx);
        cx.notify();
        true
    }

    /// 删除一条连接档案（连带凭据文件里的口令）。
    fn delete_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.connections.retain(|cfg| cfg.id != id);
        self.sessions.remove(id);
        let _ = delete_password(id);
        let _ = duck_redis::config::delete_ssh_password(id);
        if self.active.as_deref() == Some(id) {
            self.active = None;
            self.keys.clear();
            self.detail = None;
            self.selected = None;
            self.rebuild_tree();
        }
        self.persist_connections(cx);
        self.editing = None;
        notify::info(tr(cx, "连接已删除"), window, cx);
        cx.notify();
    }

    /// 打开「添加新服务器」弹窗。
    fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = cx.new(|cx| EditForm::new(window, cx));
        self.open_connection_dialog(form, window, cx);
    }

    /// 打开「编辑服务器」弹窗。
    fn edit_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cfg) = self.connections.iter().find(|cfg| cfg.id == id).cloned() else {
            return;
        };
        let form = cx.new(|cx| EditForm::from_config(&cfg, window, cx));
        self.open_connection_dialog(form, window, cx);
    }

    /// 用弹窗里的**当前表单值**发一次真实建连 + `PING`，只报结果不建会话。
    fn test_connection(
        &mut self,
        form: &Entity<EditForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cfg = match form.read(cx).to_config(cx) {
            Ok(cfg) => cfg,
            Err(message) => {
                notify::warning(SharedString::from(message), window, cx);
                return;
            }
        };
        if let Err(error) = cfg.validate() {
            notify::warning(SharedString::from(error.to_string()), window, cx);
            return;
        }
        // 弹窗里填了口令就用它；编辑旧连接留空则用凭据文件里的既有口令。
        let typed = EditForm::input_value(&form.read(cx).password, cx);
        let password = if typed.is_empty() {
            load_password(&cfg.id).ok().flatten()
        } else {
            Some(typed)
        };
        self.spawn_redis(
            cx,
            move || -> Result<String, RedisError> {
                let mut conn = RedisConnection::connect(&cfg, password.as_deref())?;
                conn.ping()?;
                conn.server_version()
            },
            move |this, result, window, cx| match result {
                Ok(version) => {
                    notify::success(trf(cx, "连接成功（{}）", &[&version]), window, cx);
                }
                Err(error) => {
                    notify::error(trf(cx, "连接失败：{}", &[&error.to_string()]), window, cx);
                    this.status = trf(cx, "连接失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 打开连接编辑弹窗（页签 / 底部动作与 zedis 的「添加新服务器」同构）。
    fn open_connection_dialog(
        &mut self,
        form: Entity<EditForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_new = form.read(cx).id.is_none();
        let title = if is_new {
            tr(cx, "添加新服务器").to_owned()
        } else {
            tr(cx, "编辑服务器").to_owned()
        };
        self.editing = Some(form.clone());
        let view = self.view.clone();
        let drag = modal_drag(cx);

        // 键盘 ⏎ 与底部主按钮共用同一份保存判断（`ConfirmHandler` 口径）。
        let confirm: ConfirmHandler = {
            let form = form.clone();
            let view = view.clone();
            Rc::new(move |window, cx| {
                view.update(cx, |this, cx| this.save_form(&form, window, cx))
                    .unwrap_or(true)
            })
        };

        window.open_dialog(cx, move |dialog, _, cx| {
            let title = title.clone();
            let confirm = Rc::clone(&confirm);
            let form_for_content = form.clone();
            let drag_for_content = drag.clone();
            let is_new = form.read(cx).id.is_none();
            let ok_text = tr(cx, if is_new { "添加" } else { "保存" });
            let offset = modal_offset(&drag, cx);

            dialog
                .w(px(FORM_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                // 库默认还会在面板右上角自画一个 ✕（`Dialog::close_button` 缺省 true），
                // 关闭入口统一走 `modal_title_row`，这里必须关掉，否则出现两个关闭按钮。
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(ok_text)
                        .cancel_text(tr(cx, "取消"))
                        .show_cancel(true),
                )
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "redis-form-title",
                                &drag_for_content,
                                title.clone(),
                                cx,
                            ))
                            .child(render_form_tabs(&form_for_content, cx)),
                    ))
                })
                .on_ok(move |_, window, cx| confirm(window, cx))
                .footer(redis_form_footer(&form, &view, is_new, cx))
        });
    }

    fn persist_connections(&self, cx: &App) {
        match serde_json::to_string(&self.connections) {
            Ok(json) => duck_store::kv_put(cx, KEY_CONNECTIONS, json),
            Err(error) => tracing::warn!(%error, "serialize redis connections failed"),
        }
    }

    // ------------------------------------------------------------------
    // 连接生命周期。
    // ------------------------------------------------------------------

    /// 连接一条档案（后台建连 + 版本 + ACL 探测）。
    fn connect_entry(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cfg) = self.connections.iter().find(|cfg| cfg.id == id).cloned() else {
            return;
        };
        let Some(session) = self.sessions.get(id) else {
            // 完成回调要跨线程存活，`id` 必须先脱掉借用（`&str` 不能进 `'static` 闭包）。
            let id = id.to_owned();
            let password = load_password(&id).ok().flatten();
            self.status = trf(cx, "正在连接 {}…", &[&cfg.display_name()]).into();
            self.spawn_redis(
                cx,
                move || -> Result<(RedisConnection, String, AclProbe, ServerFeatures), RedisError> {
                    let mut conn = RedisConnection::connect(&cfg, password.as_deref())?;
                    let version = conn.server_version()?;
                    let acl = conn.acl_probe();
                    // 能力探测是**只读**的，失败也不能拦住连接（见 `probe_features` 的口径：
                    // 探测不出来时按「可尝试」处理，而不是把面板全灰掉）。
                    let features = conn
                        .probe_features()
                        .unwrap_or_else(|_| ServerFeatures::unprobed(Some(version.clone())));
                    Ok((conn, version, acl, features))
                },
                move |this, result, window, cx| match result {
                    Ok((conn, version, acl, features)) => {
                        let target = conn.target().to_owned();
                        this.sessions.insert(
                            id.clone(),
                            Session {
                                conn: Arc::new(Mutex::new(conn)),
                                version: version.clone(),
                                acl,
                                unlock_until: None,
                                features: Some(features),
                            },
                        );
                        this.active = Some(id.clone());
                        // 换了连接：面板数据与监控流都作废（旧服务端的数字挂在新连接上没有意义）。
                        this.panel_data = PanelData::default();
                        this.stop_monitor(cx);
                        this.status = trf(cx, "已连接 {}（{}）", &[&target, &version]).into();
                        notify::success(tr(cx, "连接成功"), window, cx);
                        // 连上就取一次键总数并扫第一页。
                        this.refresh_keys(window, cx);
                    }
                    Err(error) => {
                        this.status = trf(cx, "连接失败：{}", &[&error.to_string()]).into();
                        notify::error(trf(cx, "连接失败：{}", &[&error.to_string()]), window, cx);
                    }
                },
            );
            return;
        };
        // 已连上：只切激活连接（会话保留，不重连）。
        let _ = session;
        self.activate(id, window, cx);
    }

    /// 切换激活连接并按新的档案重扫键空间。
    fn activate(&mut self, id: &str, _window: &mut Window, cx: &mut Context<Self>) {
        if self.active.as_deref() != Some(id) {
            self.active = Some(id.to_owned());
            self.keys.clear();
            self.detail = None;
            self.selected = None;
            self.scan_cursor = 0;
            self.scan_done = true;
            self.limit_announced = false;
            // 换了连接就是换了一个键空间：展开状态（按节点路径记的）全部作废，
            // 留着会以旧路径误展开新键空间里同名但不同层的节点。
            self.tree_overrides.clear();
            self.rebuild_tree();
            // 面板数据与监控流同理作废：它们反映的是**另一台服务端**的现状。
            self.panel_data = PanelData::default();
            self.stop_monitor(cx);
        }
        if let Some(cfg) = self.active_config() {
            self.status = trf(cx, "当前连接：{}", &[&cfg.display_name()]).into();
        }
        cx.notify();
    }

    /// 断开激活连接。
    fn disconnect_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        self.sessions.remove(&id);
        self.keys.clear();
        self.detail = None;
        self.selected = None;
        self.scan_done = true;
        self.rebuild_tree();
        self.status = tr(cx, "已断开").into();
        notify::info(tr(cx, "已断开连接"), window, cx);
        cx.notify();
    }

    /// 生产档解锁（限时窗口，见设计文档 §4.4）。
    fn unlock_writes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.unlock_until = Some(Instant::now() + Duration::from_secs(WRITE_UNLOCK_SECS));
        let minutes = WRITE_UNLOCK_SECS / 60;
        self.status = trf(cx, "写操作已解锁 {} 分钟", &[&minutes.to_string()]).into();
        notify::warning(
            trf(
                cx,
                "写操作已解锁 {} 分钟，到期自动上锁",
                &[&minutes.to_string()],
            ),
            window,
            cx,
        );
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 服务端观测（M1.4）：视图切换、能力探测、面板拉取、终端、实时监控。
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

    /// 拉取当前面板的数据。
    ///
    /// `Monitor` / `Cli` 不走批量拉取：前者是流、后者每次输入现发一条。
    fn load_panel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let panel = self.panel;
        if matches!(panel, ServerPanel::Monitor | ServerPanel::Cli) {
            return;
        }
        let conn = session.conn.clone();
        // 配置面板的过滤串是**服务端 glob**（`CONFIG GET <pattern>`），交给服务端做；
        // 别的面板用不到，空串归一成 `*`。
        let pattern = {
            let text = self.config_pattern.read(cx).value().trim().to_owned();
            if text.is_empty() {
                "*".to_owned()
            } else {
                text
            }
        };
        self.panel_busy = true;
        cx.notify();
        self.spawn_redis(
            cx,
            move || -> Result<PanelData, RedisError> {
                let mut conn = lock(&conn)?;
                let mut data = PanelData::default();
                match panel {
                    ServerPanel::Info => {
                        let text = conn.info(None)?;
                        data.info_sections = parse_info_sections(&text);
                    }
                    ServerPanel::Slowlog => {
                        data.slowlog = conn.slowlog_get(SLOWLOG_PAGE)?;
                        data.slowlog_len = conn.slowlog_len().unwrap_or(0);
                    }
                    ServerPanel::Clients => data.clients = conn.client_list()?,
                    ServerPanel::Config => data.config = conn.config_get(&pattern)?,
                    ServerPanel::Memory => {
                        // 诊断报告与统计表分开取、各自兜底：老服务端可能没有 DOCTOR，
                        // 那不该把有数据的统计表一起拖垮。
                        data.memory_doctor = conn.memory_doctor().unwrap_or_default();
                        data.memory_stats = conn.memory_stats()?;
                    }
                    ServerPanel::Monitor | ServerPanel::Cli => {}
                }
                Ok(data)
            },
            move |this, result, window, cx| {
                this.panel_busy = false;
                match result {
                    Ok(data) => this.merge_panel_data(panel, data),
                    Err(error) => notify::error(
                        trf(cx, "读取服务端信息失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    ),
                }
                cx.notify();
            },
        );
    }

    /// 把一次拉取的结果并进 `panel_data`。
    ///
    /// **只覆盖本面板的字段**：拉取期间用户可能已经切到别的面板，
    /// 整块赋值会把已经加载好的数据清掉。
    fn merge_panel_data(&mut self, panel: ServerPanel, data: PanelData) {
        match panel {
            ServerPanel::Info => self.panel_data.info_sections = data.info_sections,
            ServerPanel::Slowlog => {
                self.panel_data.slowlog = data.slowlog;
                self.panel_data.slowlog_len = data.slowlog_len;
            }
            ServerPanel::Clients => self.panel_data.clients = data.clients,
            ServerPanel::Config => self.panel_data.config = data.config,
            ServerPanel::Memory => {
                self.panel_data.memory_doctor = data.memory_doctor;
                self.panel_data.memory_stats = data.memory_stats;
            }
            ServerPanel::Monitor | ServerPanel::Cli => {}
        }
    }

    /// 把列表里的一项回填到配置编辑框。
    fn pick_config(&mut self, entry: &ConfigEntry, window: &mut Window, cx: &mut Context<Self>) {
        let name: SharedString = entry.name().to_owned().into();
        let value: SharedString = entry.value().to_owned().into();
        self.config_name
            .update(cx, |input, cx| input.set_value(name, window, cx));
        self.config_value
            .update(cx, |input, cx| input.set_value(value, window, cx));
        cx.notify();
    }

    /// 提交配置修改（`CONFIG SET`，受 `Capability::ConfigWrite` 约束）。
    fn save_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.config_name.read(cx).value().trim().to_owned();
        let value = self.config_value.read(cx).value().to_string();
        if name.is_empty() {
            notify::warning(tr(cx, "请先填参数名（点列表中一项可回填）"), window, cx);
            return;
        }
        self.confirm_or_run(
            Capability::ConfigWrite,
            PendingAction::ConfigSet { name, value },
            window,
            cx,
        );
    }

    /// 真正写入配置项（`*_at` 族：调用方之外的入口也不能绕过闸门）。
    fn set_config_at(
        &mut self,
        name: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(reason) = self.check(Capability::ConfigWrite) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let echo = name.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                conn.config_set(&name, &value)
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(trf(cx, "已修改配置 {}", &[&echo]), window, cx);
                    // 改完立刻重读：服务端可能对取值做了归一化（`1gb` → 字节数），
                    // 继续显示用户填的原值会让下一次编辑以错值为基础。
                    this.load_panel(window, cx);
                }
                Err(error) => notify::error(
                    trf(cx, "修改配置失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                ),
            },
        );
    }

    /// 清空慢日志（`SLOWLOG RESET`）。
    ///
    /// 走 `ServerAdmin` 档**并强制二次确认**：慢日志是排查线上事故的唯一线索，
    /// 误清一次就没了（这一点与「用户自己关掉写入确认」的开关无关）。
    fn reset_slowlog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_or_run(
            Capability::ServerAdmin,
            PendingAction::SlowlogReset,
            window,
            cx,
        );
    }

    /// 真正清空慢日志。
    fn reset_slowlog_at(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(reason) = self.check(Capability::ServerAdmin) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                conn.slowlog_reset()
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(tr(cx, "慢日志已清空"), window, cx);
                    this.load_panel(window, cx);
                }
                Err(error) => notify::error(
                    trf(cx, "清空慢日志失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                ),
            },
        );
    }

    /// 强制断开一条客户端连接（`CLIENT KILL ADDR`）。
    fn kill_client(&mut self, addr: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_or_run(
            Capability::ServerAdmin,
            PendingAction::ClientKill { addr },
            window,
            cx,
        );
    }

    /// 真正断开一条客户端连接。
    fn kill_client_at(&mut self, addr: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(reason) = self.check(Capability::ServerAdmin) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let echo = addr.clone();
        self.spawn_redis(
            cx,
            move || -> Result<bool, RedisError> {
                let mut conn = lock(&conn)?;
                conn.client_kill_addr(&addr)
            },
            move |this, result, window, cx| match result {
                Ok(true) => {
                    notify::success(trf(cx, "已断开 {}", &[&echo]), window, cx);
                    this.load_panel(window, cx);
                }
                Ok(false) => {
                    notify::warning(trf(cx, "未找到客户端 {}", &[&echo]), window, cx);
                    this.load_panel(window, cx);
                }
                Err(error) => notify::error(
                    trf(cx, "断开客户端失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                ),
            },
        );
    }

    /// 查询单个键的内存占用（`MEMORY USAGE`）。
    fn query_memory_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.memory_key.read(cx).value().trim().to_owned();
        if key.is_empty() {
            notify::warning(tr(cx, "请填写键名"), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let echo = key.clone();
        self.spawn_redis(
            cx,
            move || -> Result<Option<u64>, RedisError> {
                let mut conn = lock(&conn)?;
                conn.memory_usage(&key)
            },
            move |this, result, window, cx| match result {
                Ok(bytes) => {
                    this.panel_data.memory_usage = Some((echo, bytes));
                    cx.notify();
                }
                Err(error) => notify::error(
                    trf(cx, "查询内存占用失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                ),
            },
        );
    }

    /// 提交终端输入（多行会按行拆开、依次执行）。
    fn submit_cli(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.cli_input.read(cx).value().to_string();
        let lines: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if lines.is_empty() {
            return;
        }
        self.cli_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        for line in &lines {
            // 历史去重后置顶：同一条命令反复执行只占一行。
            self.cli_history.retain(|existing| existing != line);
            self.cli_history.push(line.clone());
        }
        while self.cli_history.len() > CLI_HISTORY_MAX {
            self.cli_history.remove(0);
        }
        self.cli_queue.extend(lines);
        self.pump_cli_queue(window, cx);
    }

    /// 串行泵出终端队列里的一条命令。
    ///
    /// **一次一条**：所有命令共用同一条阻塞连接，并发下发会让应答交错，
    /// 终端上就分不清哪条回复对应哪条命令了。
    fn pump_cli_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return; // 上一条还在跑；它回主线程时会再泵一次
        }
        let Some(line) = self.cli_queue.pop_front() else {
            return;
        };
        // 闸门：`RawCommand` 是写动作，SafeMode 下一律拒。
        // 终端在客户端**无法判定**一条任意命令是否只读，所以整条通道按写口径管。
        if let Err(reason) = self.check(Capability::RawCommand) {
            self.cli_lines.push(CliLine {
                command: true,
                error: false,
                text: line,
            });
            self.cli_lines.push(CliLine {
                command: false,
                error: true,
                text: deny_message(reason, cx).to_owned(),
            });
            self.trim_cli_lines();
            cx.notify();
            return;
        }
        // `KEYS` 不被拦，但要提醒它有 `SCAN` 这个更安全的替代。
        if confirm_kind(&line) == Some(ConfirmKind::PreferScan) {
            notify::warning(
                tr(cx, "KEYS 会遍历整个键空间，可能阻塞服务端；建议改用 SCAN。"),
                window,
                cx,
            );
        }
        self.cli_lines.push(CliLine {
            command: true,
            error: false,
            text: line.clone(),
        });
        self.trim_cli_lines();
        // 不可逆的破坏性命令（`FLUSHALL` / `SHUTDOWN` / `CONFIG SET` …）**无条件**二次确认：
        // 这是终端与结构化编辑最大的区别——后者只动用户看得见的那一个键。
        let confirm = is_destructive(&line)
            || self
                .active_config()
                .is_some_and(|cfg| cfg.require_confirm_writes);
        if confirm {
            self.confirm_or_run(
                Capability::RawCommand,
                PendingAction::RawCommand { line },
                window,
                cx,
            );
            cx.notify();
            return;
        }
        self.run_cli_line_at(line, window, cx);
    }

    /// 真正下发一条原始命令。
    ///
    /// `confirm_or_run` 之外的入口（含未来的）也不能绕过锁写约束，所以这里**再过一次闸门**。
    fn run_cli_line_at(&mut self, line: String, _window: &mut Window, cx: &mut Context<Self>) {
        if let Err(reason) = self.check(Capability::RawCommand) {
            self.cli_lines.push(CliLine {
                command: false,
                error: true,
                text: deny_message(reason, cx).to_owned(),
            });
            self.trim_cli_lines();
            cx.notify();
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<String, RedisError> {
                let mut conn = lock(&conn)?;
                conn.raw_command_text(&line)
            },
            move |this, result, window, cx| {
                match result {
                    Ok(text) => this.cli_lines.push(CliLine {
                        command: false,
                        error: false,
                        text,
                    }),
                    Err(error) => this.cli_lines.push(CliLine {
                        command: false,
                        error: true,
                        text: error.to_string(),
                    }),
                }
                this.trim_cli_lines();
                // 队列里还有就接着来（`spawn_redis` 已把 `busy` 降下来）。
                this.pump_cli_queue(window, cx);
                cx.notify();
            },
        );
    }

    /// 终端输出裁剪到上限（丢最旧的），并把视口贴到底部。
    fn trim_cli_lines(&mut self) {
        if self.cli_lines.len() > CLI_LINES_MAX {
            let excess = self.cli_lines.len() - CLI_LINES_MAX;
            self.cli_lines.drain(0..excess);
        }
        self.cli_scroll.scroll_to_bottom();
    }

    /// 开启 `MONITOR`。
    ///
    /// 建连与读循环都在**专用连接**上（设计文档 §4.7）：`MONITOR` 一旦开启，
    /// 该连接上再也不能发别的命令，所以绝不能复用主连接。
    fn start_monitor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.monitor_running {
            return;
        }
        let Some(cfg) = self.active_config().cloned() else {
            return;
        };
        // 探测明确说没有 `MONITOR` 就别开——省得开出来一个永远空的面板。
        if let Some(features) = self.features()
            && !features.supports("monitor")
        {
            notify::warning(tr(cx, "本服务端不支持 MONITOR。"), window, cx);
            return;
        }
        self.monitor_error = None;
        self.monitor_lines.clear();
        self.monitor_running = true;
        self.monitor_generation = self.monitor_generation.wrapping_add(1);
        let stop = Arc::new(AtomicBool::new(false));
        self.monitor_stop = Some(stop.clone());
        self.monitor_task = Some(Self::spawn_monitor_poll(
            cx,
            cfg,
            stop,
            self.monitor_generation,
        ));
        self.status = tr(cx, "实时监控已开启").into();
        cx.notify();
    }

    /// 停止 `MONITOR`（只置停止位，读循环自己收工）。
    fn stop_monitor(&mut self, cx: &mut Context<Self>) {
        if let Some(stop) = self.monitor_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if self.monitor_running {
            self.monitor_running = false;
            self.status = tr(cx, "实时监控已停止").into();
            cx.notify();
        }
    }

    /// 清空监控输出（不停会话）。
    fn clear_monitor(&mut self, cx: &mut Context<Self>) {
        self.monitor_lines.clear();
        self.monitor_error = None;
        cx.notify();
    }

    /// `MONITOR` 的轮询循环。
    ///
    /// 结构是「前台节拍 + 后台批读」：阻塞读丢进 `background_executor`，
    /// 会话所有权往返一次（`(session, batch)`）——这样读阻塞不会冻住界面线程，
    /// 也不需要另开线程或共享锁。
    fn spawn_monitor_poll(
        cx: &mut Context<Self>,
        cfg: RedisConfig,
        stop: Arc<AtomicBool>,
        generation: u64,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            // 建连本身是阻塞的（TCP + AUTH + MONITOR 握手），同样丢后台。
            let started = cx
                .background_executor()
                .spawn(async move {
                    let password = load_password(&cfg.id).ok().flatten();
                    MonitorSession::start(&cfg, password.as_deref())
                })
                .await;
            let mut session = match started {
                Ok(session) => session,
                Err(error) => {
                    let message = error.to_string();
                    let _ = this.update(cx, |page, cx| {
                        if page.monitor_generation == generation {
                            page.monitor_running = false;
                        }
                        page.monitor_error = Some(message);
                        cx.notify();
                    });
                    return;
                }
            };

            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                cx.background_executor().timer(MONITOR_TICK).await;
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // 批读（最多 `POLL_TIMEOUT`）挪到后台；会话所有权往返。
                let (back, batch) = cx
                    .background_executor()
                    .spawn(async move {
                        let batch = session.poll(MONITOR_BATCH);
                        (session, batch)
                    })
                    .await;
                session = back;
                match batch {
                    Ok(lines) => {
                        if lines.is_empty() {
                            continue;
                        }
                        let alive = this
                            .update(cx, |page, cx| {
                                page.monitor_lines.extend(lines);
                                while page.monitor_lines.len() > MONITOR_LINES_MAX {
                                    page.monitor_lines.pop_front();
                                }
                                page.monitor_scroll.scroll_to_bottom();
                                cx.notify();
                            })
                            .is_ok();
                        if !alive {
                            return; // 视图销毁，任务随之结束
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = this.update(cx, |page, cx| {
                            if page.monitor_generation == generation {
                                page.monitor_running = false;
                            }
                            page.monitor_error = Some(message);
                            cx.notify();
                        });
                        return;
                    }
                }
            }

            // 正常收工（用户停止 / 切走视图）：只在仍属本轮时回落状态。
            let _ = this.update(cx, |page, cx| {
                if page.monitor_generation == generation && page.monitor_running {
                    page.monitor_running = false;
                    cx.notify();
                }
            });
        })
    }

    // ------------------------------------------------------------------
    // 键空间。
    // ------------------------------------------------------------------

    /// 从零重扫：取 `DBSIZE` 与第一页。
    fn refresh_keys(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let pattern = self.pattern.read(cx).value().trim().to_owned();
        let pattern = if pattern.is_empty() {
            None
        } else {
            Some(pattern)
        };
        let type_filter = self.type_filter;
        let page_size = self.scan_page_size();
        self.keys.clear();
        self.selected = None;
        self.detail = None;
        self.scan_cursor = 0;
        self.scan_done = false;
        self.limit_announced = false;
        self.rebuild_tree();

        self.status = tr(cx, "正在扫描键空间…").into();
        self.spawn_redis(
            cx,
            move || -> Result<(u64, ScanPage), RedisError> {
                let mut conn = lock(&conn)?;
                let total = conn.dbsize()?;
                let page = conn.scan(0, pattern.as_deref(), page_size, type_filter)?;
                Ok((total, page))
            },
            move |this, result, window, cx| match result {
                Ok((total, page)) => {
                    this.dbsize = total;
                    this.apply_page(page, cx);
                    this.status = trf(
                        cx,
                        "已加载 {} / {} 个键",
                        &[&this.keys.len().to_string(), &total.to_string()],
                    )
                    .into();
                    cx.notify();
                }
                Err(error) => {
                    this.scan_done = true;
                    notify::error(trf(cx, "扫描失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 「继续加载」：游标续扫下一批。
    fn load_more_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scan_done || self.busy {
            return;
        }
        if self.keys.len() >= KEY_LIST_MAX {
            if !self.limit_announced {
                self.limit_announced = true;
                notify::warning(
                    trf(
                        cx,
                        "已加载 {} 个键，暂停追加；请用过滤串收窄范围",
                        &[&KEY_LIST_MAX.to_string()],
                    ),
                    window,
                    cx,
                );
            }
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let cursor = self.scan_cursor;
        let pattern = self.pattern.read(cx).value().trim().to_owned();
        let pattern = if pattern.is_empty() {
            None
        } else {
            Some(pattern)
        };
        let type_filter = self.type_filter;
        let page_size = self.scan_page_size();

        self.spawn_redis(
            cx,
            move || -> Result<ScanPage, RedisError> {
                let mut conn = lock(&conn)?;
                conn.scan(cursor, pattern.as_deref(), page_size, type_filter)
            },
            move |this, result, window, cx| match result {
                Ok(page) => {
                    this.apply_page(page, cx);
                    this.status = trf(
                        cx,
                        "已加载 {} / {} 个键",
                        &[&this.keys.len().to_string(), &this.dbsize.to_string()],
                    )
                    .into();
                    cx.notify();
                }
                Err(error) => {
                    this.scan_done = true;
                    notify::error(trf(cx, "加载失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 把一页 `SCAN` 结果并入本地列表（去重 + 上限）。
    fn apply_page(&mut self, page: ScanPage, cx: &mut Context<Self>) {
        self.scan_cursor = page.cursor;
        self.scan_done = page.is_finished();
        for key in page.keys {
            if self.keys.len() >= KEY_LIST_MAX {
                break;
            }
            // 服务端允许同一键在相邻批次重复出现，本地去重（列表要稳定）。
            if !self.keys.iter().any(|existing| existing == &key) {
                self.keys.push(key);
            }
        }
        // 键集变了，树的展平行必须跟着重建（缓存不能过期）。
        self.rebuild_tree();
        cx.notify();
    }

    /// 重建树模式的展平行缓存。
    ///
    /// 调用点：键集变化（`apply_page` / 清空）、连接切换、切形态、切展开状态、
    /// 树参数（分隔符 / 深度 / 阈值）变化。**渲染路径绝不调它**。
    fn rebuild_tree(&mut self) {
        if !self.tree_mode {
            self.tree_rows.clear();
            return;
        }
        let (separator, depth, auto_expand) = match self.active_config() {
            Some(cfg) => (
                cfg.effective_separator().to_owned(),
                cfg.effective_tree_depth(),
                cfg.effective_auto_expand(),
            ),
            // 没有连接时用内核缺省：冒号分隔、8 层、不自动展开。
            None => (":".to_owned(), duck_redis::scan::MAX_TREE_DEPTH, 0),
        };
        let tree = build_tree_with_depth(self.keys.iter().map(String::as_str), &separator, depth);
        let mut rows = Vec::new();
        flatten_tree(
            &tree,
            0,
            "",
            &separator,
            &self.tree_overrides,
            auto_expand,
            &mut rows,
        );
        self.tree_rows = rows;
    }

    /// 切键列表形态（平铺 ↔ 命名空间树）。
    fn toggle_tree_mode(&mut self, cx: &mut Context<Self>) {
        self.tree_mode = !self.tree_mode;
        // 从平铺切到树时，展平行还是空的——必须立刻建一次，否则界面一片空白。
        self.rebuild_tree();
        cx.notify();
    }

    /// 展开 / 收起一个树节点。
    fn toggle_tree_node(&mut self, path: String, expanded: bool, cx: &mut Context<Self>) {
        self.tree_overrides.insert(path, expanded);
        self.rebuild_tree();
        cx.notify();
    }

    /// 选中一个键并取它的详情与首屏值。
    fn select_key(&mut self, index: usize, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.keys.get(index).cloned() else {
            return;
        };
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.selected = Some(index);
        self.detail = None;

        // 两个闭包都要键名：一个拿它去查，一个拿它装配详情。
        let detail_key = key.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(KeyType, i64, RedisValue), RedisError> {
                let mut conn = lock(&conn)?;
                let key_type = conn.key_type(&key)?;
                let ttl = conn.ttl(&key)?;
                // 模块类型没有通用读法（要 `DUMP`）：不要白跑一次 `load_value` 换一个
                // 英文错误，直接回 `Missing` 占位，装配阶段会换成「没有查看器」的说明。
                if key_type == KeyType::Module {
                    return Ok((key_type, ttl, RedisValue::Missing));
                }
                let value = conn.load_value(&key, key_type, VALUE_PAGE)?;
                Ok((key_type, ttl, value))
            },
            move |this, result, window, cx| match result {
                Ok((key_type, ttl, value)) => {
                    let decode = this.decode;
                    let detail = build_detail(detail_key, key_type, ttl, value, decode, cx);
                    // 字符串值顺手填进编辑框：选中即能改，少一步复制粘贴。
                    if let ValueView::Text { rendered, .. } = &detail.view {
                        this.edit_buffer.update(cx, |input, cx| {
                            input.set_value(rendered.clone(), window, cx);
                        });
                    }
                    this.detail = Some(detail);
                    cx.notify();
                }
                Err(error) => {
                    notify::error(trf(cx, "读取键失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 删除当前选中的键（受 `Capability::DeleteKey` 约束）。
    fn delete_selected_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail_key) = self.detail.as_ref().map(|detail| detail.key.clone()) else {
            return;
        };
        self.confirm_or_run(
            Capability::DeleteKey,
            PendingAction::DeleteKey { key: detail_key },
            window,
            cx,
        );
    }

    /// 真正执行删除键（闸门已过、确认已给）。
    fn delete_key_at(&mut self, detail_key: String, window: &mut Window, cx: &mut Context<Self>) {
        // 第二道保险：直接调用本方法时也必须过闸门。
        if let Err(reason) = self.check(Capability::DeleteKey) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let key = detail_key.clone();
        self.spawn_redis(
            cx,
            move || -> Result<u64, RedisError> {
                let mut conn = lock(&conn)?;
                conn.del_key(&key)
            },
            move |this, result, window, cx| match result {
                Ok(deleted) => {
                    if deleted > 0 {
                        this.keys.retain(|existing| existing != &detail_key);
                        this.detail = None;
                        this.selected = None;
                        this.rebuild_tree();
                        this.status = trf(cx, "已删除键 {}", &[&detail_key]).into();
                        notify::success(tr(cx, "键已删除"), window, cx);
                    } else {
                        notify::warning(tr(cx, "键已不存在"), window, cx);
                    }
                    cx.notify();
                }
                Err(error) => {
                    notify::error(trf(cx, "删除失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // M1.3：破坏性动作的二次确认。
    //
    // 确认本身是**连接档案上的开关**（`require_confirm_writes`），不是我们强加的
    // 门槛：关掉时直接执行——否则「高级用户」每删一个哈希字段都要挨一次弹窗。
    // ------------------------------------------------------------------

    /// 闸门 →（开关打开时）确认框 → 真正执行。**所有破坏性动作的唯一入口。**
    ///
    /// 动作参数（尤其 `RenameKey` 的目标键名）在构造 `PendingAction` 那一刻就冻结，
    /// 确认期间**不回读输入框**：否则用户改完输入框再点确认，动的不是他看到的那个。
    fn confirm_or_run(
        &mut self,
        capability: Capability,
        action: PendingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(reason) = self.check(capability) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let requires_confirm = self
            .active_config()
            .is_some_and(|cfg| cfg.require_confirm_writes);
        if !requires_confirm {
            self.run_pending(action, window, cx);
            return;
        }

        let view = self.view.clone();
        let title = action.title(cx);
        let message = action.message(cx);
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _, cx| {
            let offset = modal_offset(&drag, cx);
            // 外层闭包是 `Fn`（每次重排都重跑）→ 每一轮都得重新克隆一份交给内层。
            let action_for_ok = action.clone();
            let view_for_ok = view.clone();
            let drag_for_content = drag.clone();
            let message_for_content = message.clone();

            dialog
                .w(px(CONFIRM_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(tr(cx, "确认执行"))
                        .cancel_text(tr(cx, "取消"))
                        .show_cancel(true),
                )
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "redis-confirm-title",
                                &drag_for_content,
                                title,
                                cx,
                            ))
                            .child(body_text(message_for_content.clone(), cx)),
                    ))
                })
                .on_ok(move |_, window, cx| {
                    // 确认后才真正动手；视图已销毁（`update` 返回 `Err`）也照样关弹窗，
                    // 不让一个点不掉的确认框挂在屏幕上。
                    let _ = view_for_ok.update(cx, |this, cx| {
                        this.run_pending(action_for_ok.clone(), window, cx);
                    });
                    true
                })
        });
    }

    /// 把 `PendingAction` 分派到对应的**真正执行**方法（`*_at` 族）。
    ///
    /// 每个 `*_at` 内部仍会再过一次闸门——`confirm_or_run` 之外的调用点（测试、
    /// 未来新增入口）也不会绕过锁写约束。
    fn run_pending(&mut self, action: PendingAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            PendingAction::DeleteKey { key } => self.delete_key_at(key, window, cx),
            PendingAction::RenameKey { from, to } => {
                self.rename_key_from_to(from, to, window, cx);
            }
            PendingAction::ClearTtl { key } => self.clear_ttl_at(key, window, cx),
            PendingAction::DeleteHashField { key, field } => {
                self.delete_hash_field_at(key, field, window, cx);
            }
            PendingAction::DeleteListItem { key, index } => {
                self.delete_list_item_at(key, index, window, cx);
            }
            PendingAction::DeleteSetMember { key, member } => {
                self.delete_set_member_at(key, member, window, cx);
            }
            PendingAction::DeleteZSetMember { key, member } => {
                self.delete_zset_member_at(key, member, window, cx);
            }
            PendingAction::RawCommand { line } => self.run_cli_line_at(line, window, cx),
            PendingAction::ConfigSet { name, value } => {
                self.set_config_at(name, value, window, cx);
            }
            PendingAction::SlowlogReset => self.reset_slowlog_at(window, cx),
            PendingAction::ClientKill { addr } => self.kill_client_at(addr, window, cx),
        }
    }

    /// 保存字符串值编辑（受 `Capability::WriteKey` 约束）。
    fn save_string_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = &self.detail else {
            return;
        };
        if !matches!(detail.view, ValueView::Text { .. }) {
            notify::warning(tr(cx, "只有字符串值可直接编辑"), window, cx);
            return;
        }
        if let Err(reason) = self.check(Capability::WriteKey) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let key = detail.key.clone();
        let value = self.edit_buffer.read(cx).value().to_string();
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        // 后台闭包吃掉 `key` / `value`，回主线程那一步要用副本（更新右栏与状态栏）。
        let key_echo = key.clone();
        let value_echo = value.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                // KEEPTTL：只改值，别把键上原有的过期时间顺手抹掉。
                conn.set_string(&key, value.as_bytes(), true)
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    let len = value_echo.len();
                    if let Some(detail) = &mut this.detail {
                        detail.view = ValueView::Text {
                            rendered: value_echo,
                            notice: None,
                            bytes: len,
                        };
                        detail.size = len;
                    }
                    this.status = trf(cx, "已保存键 {}", &[&key_echo]).into();
                    notify::success(tr(cx, "值已保存"), window, cx);
                    cx.notify();
                }
                Err(error) => {
                    notify::error(trf(cx, "保存失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // 结构化的值级编辑（M1.2）：哈希字段 / 列表元素 / 集合成员 / 有序集合成员。
    //
    // 全部走内核的**参数化**写方法（`hset` / `lset` / `sadd` / `zadd` …），
    // 不拼命令行——键名与元素里带空格或非 UTF-8 字节时，命令行切词会拆坏参数。
    // ------------------------------------------------------------------

    /// 写操作放行检查；被拒时提示原因并返回 `false`（调用方直接 `return`）。
    fn allow_write(
        &self,
        capability: Capability,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.check(capability) {
            Ok(()) => true,
            Err(reason) => {
                notify::warning(deny_message(reason, cx), window, cx);
                false
            }
        }
    }

    /// 取当前选中键的 `(键名, 类型)`；没选中返回 `None`。
    fn current_key(&self) -> Option<(String, KeyType)> {
        self.detail
            .as_ref()
            .map(|detail| (detail.key.clone(), detail.key_type))
    }

    /// 写成功后重读当前键。
    ///
    /// 集合类值改一笔就重新拉一页：本地拼装容易与服务端实际内容（顺序、
    /// 去重、分数）不一致，宁可多一次往返也不显示一个「看起来对」的假状态。
    fn reload_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.select_key(index, window, cx);
        }
    }

    /// 集合类写操作的统一外壳：闸门 → 后台执行 → 成功后重读当前键。
    ///
    /// `work` 拿到连接与键名（旧键名），返回 `Ok(())` 即视为成功。
    /// 写失败一律 `notify::error`，不静默吞掉——用户按下按钮就必须有反馈。
    fn spawn_write<W>(
        &mut self,
        capability: Capability,
        key: String,
        success: &'static str,
        work: W,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        W: FnOnce(&mut RedisConnection, &str) -> Result<(), RedisError> + Send + 'static,
    {
        if !self.allow_write(capability, window, cx) {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                work(&mut conn, &key)
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(success, window, cx);
                    this.reload_selected(window, cx);
                }
                Err(error) => {
                    notify::error(trf(cx, "写入失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 写一个哈希字段（新增或覆盖）。
    fn save_hash_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::Hash {
            return;
        }
        let field = self.hash_field.read(cx).value().to_string();
        let value = self.hash_value.read(cx).value().to_string();
        if field.is_empty() {
            notify::warning(tr(cx, "字段名不能为空"), window, cx);
            return;
        }
        let field_bytes = field.into_bytes();
        let value_bytes = value.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "字段已写入"),
            move |conn, key| conn.hset(key, &field_bytes, &value_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 删一个哈希字段。
    fn delete_hash_field(&mut self, field: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::Hash {
            return;
        }
        self.confirm_or_run(
            Capability::WriteKey,
            PendingAction::DeleteHashField { key, field },
            window,
            cx,
        );
    }

    /// 真正执行删除哈希字段。
    fn delete_hash_field_at(
        &mut self,
        key: String,
        field: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field_bytes = field.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "字段已删除"),
            move |conn, key| conn.hdel(key, &field_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 写一个有序集合成员（新增 / 改分数 / 改名）。
    ///
    /// `previous` 非空表示改的是既有成员（改名时内核会先加后删，见 `zset_put`）。
    fn save_zset_member(
        &mut self,
        previous: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::ZSet {
            return;
        }
        let member = self.zset_member.read(cx).value().to_string();
        let score_text = self.zset_score.read(cx).value().to_string();
        if member.is_empty() {
            notify::warning(tr(cx, "成员不能为空"), window, cx);
            return;
        }
        let Ok(score) = score_text.trim().parse::<f64>() else {
            notify::warning(tr(cx, "分数必须是数字"), window, cx);
            return;
        };
        let member_bytes = member.into_bytes();
        let previous_bytes = previous.map(String::into_bytes);
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "成员已写入"),
            move |conn, key| {
                conn.zset_put(key, score, &member_bytes, previous_bytes.as_deref())
                    .map(|_| ())
            },
            window,
            cx,
        );
    }

    /// 删一个有序集合成员。
    fn delete_zset_member(&mut self, member: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::ZSet {
            return;
        }
        self.confirm_or_run(
            Capability::WriteKey,
            PendingAction::DeleteZSetMember { key, member },
            window,
            cx,
        );
    }

    /// 真正执行删除有序集合成员。
    fn delete_zset_member_at(
        &mut self,
        key: String,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let member_bytes = member.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "成员已删除"),
            move |conn, key| conn.zset_remove(key, &member_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 加一个集合成员。
    fn add_set_member(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::Set {
            return;
        }
        let member = self.set_member.read(cx).value().to_string();
        if member.is_empty() {
            notify::warning(tr(cx, "成员不能为空"), window, cx);
            return;
        }
        let member_bytes = member.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "成员已加入"),
            move |conn, key| conn.set_add(key, &member_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 删一个集合成员。
    fn delete_set_member(&mut self, member: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::Set {
            return;
        }
        self.confirm_or_run(
            Capability::WriteKey,
            PendingAction::DeleteSetMember { key, member },
            window,
            cx,
        );
    }

    /// 真正执行删除集合成员。
    fn delete_set_member_at(
        &mut self,
        key: String,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let member_bytes = member.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "成员已删除"),
            move |conn, key| conn.set_remove(key, &member_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 列表写入：底部表单处于「改某一行」态就 `LSET`，否则按 `front` 推入。
    fn save_list_value(&mut self, front: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::List {
            return;
        }
        let value = self.list_value.read(cx).value().to_string();
        let value_bytes = value.into_bytes();
        let editing = self.list_editing;
        let (capability, success) = if editing.is_some() {
            (Capability::WriteKey, tr(cx, "元素已更新"))
        } else {
            (Capability::WriteKey, tr(cx, "元素已推入"))
        };
        self.spawn_write(
            capability,
            key,
            success,
            move |conn, key| match editing {
                // 注意 `list_editing` 存的是**列表下标**（0 起，与界面显示的序号一致）。
                Some(index) => conn.list_set(key, index, &value_bytes),
                None => conn.list_push(key, &value_bytes, front).map(|_| ()),
            },
            window,
            cx,
        );
    }

    /// 把某一行填进底部表单，进入「按位置改」态。
    fn edit_list_item(
        &mut self,
        index: i64,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.list_editing = Some(index);
        self.list_value.update(cx, |input, cx| {
            input.set_value(value, window, cx);
        });
        cx.notify();
    }

    /// 退出「按位置改」态，回到追加态。
    fn cancel_list_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list_editing = None;
        self.list_value.update(cx, |input, cx| {
            input.set_value(String::new(), window, cx);
        });
        cx.notify();
    }

    /// 删掉列表里指定位置的元素。
    fn delete_list_item(&mut self, index: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::List {
            return;
        }
        self.confirm_or_run(
            Capability::WriteKey,
            PendingAction::DeleteListItem { key, index },
            window,
            cx,
        );
    }

    /// 真正执行删除列表元素。
    fn delete_list_item_at(
        &mut self,
        key: String,
        index: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 若正在改这一行，先退出编辑态——否则下标会在删除后错位。
        if self.list_editing == Some(index) {
            self.list_editing = None;
        }
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "元素已删除"),
            move |conn, key| conn.list_remove_at(key, index).map(|_| ()),
            window,
            cx,
        );
    }

    /// 设置键的过期时间（秒）。
    fn set_ttl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, _)) = self.current_key() else {
            return;
        };
        let text = self.ttl_input.read(cx).value().to_string();
        let Ok(seconds) = text.trim().parse::<i64>() else {
            notify::warning(tr(cx, "请填写整数秒数"), window, cx);
            return;
        };
        if seconds <= 0 {
            // 0 在服务端语义里是「立即删除」，比「设成永不过期」危险得多：
            // 想永不过期该用「移除过期时间」，别让用户在 TTL 框里打出 0 就把键删了。
            notify::warning(
                tr(cx, "秒数必须大于 0；要永不过期请用「移除过期时间」"),
                window,
                cx,
            );
            return;
        }
        if !self.allow_write(Capability::SetTtl, window, cx) {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<bool, RedisError> {
                let mut conn = lock(&conn)?;
                conn.expire(&key, seconds)
            },
            move |this, result, window, cx| match result {
                Ok(true) => {
                    if let Some(detail) = &mut this.detail {
                        detail.ttl = seconds;
                    }
                    this.status = trf(cx, "已为 {} 设置过期时间", &[&seconds.to_string()]).into();
                    notify::success(tr(cx, "过期时间已设置"), window, cx);
                    cx.notify();
                }
                Ok(false) => notify::warning(tr(cx, "键已不存在"), window, cx),
                Err(error) => {
                    notify::error(trf(cx, "设置失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 移除键的过期时间（改为永不过期）。
    fn clear_ttl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, _)) = self.current_key() else {
            return;
        };
        self.confirm_or_run(
            Capability::SetTtl,
            PendingAction::ClearTtl { key },
            window,
            cx,
        );
    }

    /// 真正执行移除过期时间。
    fn clear_ttl_at(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.allow_write(Capability::SetTtl, window, cx) {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<bool, RedisError> {
                let mut conn = lock(&conn)?;
                conn.persist(&key)
            },
            move |this, result, window, cx| match result {
                Ok(true) => {
                    if let Some(detail) = &mut this.detail {
                        detail.ttl = -1;
                    }
                    notify::success(tr(cx, "已改为永不过期"), window, cx);
                    cx.notify();
                }
                // 本来就没有过期时间：不是错误，但要说清楚「什么都没变」。
                Ok(false) => notify::warning(tr(cx, "该键本来就没有过期时间"), window, cx),
                Err(error) => {
                    notify::error(trf(cx, "设置失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 重命名当前键。
    ///
    /// ⚠️ `RENAME` 会**静默覆盖**同名目标键。界面上的说法只能是「已重命名」，
    /// 无法承诺「没覆盖任何东西」——所以文案里带上目标键名，让用户自己确认。
    fn rename_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, _)) = self.current_key() else {
            return;
        };
        let new_key = self.rename_input.read(cx).value().to_string();
        let new_key = new_key.trim().to_owned();
        if new_key.is_empty() {
            notify::warning(tr(cx, "新的键名不能为空"), window, cx);
            return;
        }
        if new_key == key {
            notify::warning(tr(cx, "新键名与原名相同"), window, cx);
            return;
        }
        self.confirm_or_run(
            Capability::RenameKey,
            PendingAction::RenameKey {
                from: key,
                to: new_key,
            },
            window,
            cx,
        );
    }

    /// 真正执行重命名。
    ///
    /// ⚠️ 目标键名在**点按钮那一刻**就固定下来了（`PendingAction::RenameKey`），
    /// 确认期间不再回读输入框——否则用户改完输入框再点确认，动的不是他看到的那个。
    fn rename_key_from_to(
        &mut self,
        old_key: String,
        new_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = old_key.clone();
        if !self.allow_write(Capability::RenameKey, window, cx) {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let renamed = new_key.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                conn.rename(&old_key, &renamed)
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    // 本地同步：键列表里的旧名换成新名；详情与选中项跟着走。
                    if let Some(slot) = this.keys.iter_mut().find(|existing| **existing == key) {
                        *slot = new_key.clone();
                    }
                    if let Some(detail) = &mut this.detail {
                        detail.key = new_key.clone();
                    }
                    this.status = trf(cx, "已重命名为 {}", &[&new_key]).into();
                    // 键名变了，命名空间树的分组也跟着变（`user:1` → `app:1` 会换层）。
                    this.rebuild_tree();
                    notify::success(tr(cx, "键已重命名"), window, cx);
                    cx.notify();
                }
                Err(error) => {
                    notify::error(trf(cx, "重命名失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 切字符串值的解码档位。
    ///
    /// 只用已缓存的原始字节重算，**不重发 `GET`**——切档是纯展示行为，
    /// 让它产生网络流量既慢又可能与服务端内容产生时间差。
    fn switch_decode(&mut self, mode: DecodeKind, cx: &mut Context<Self>) {
        if self.decode == mode {
            return;
        }
        self.decode = mode;
        if let Some(detail) = &mut self.detail
            && detail.key_type == KeyType::String
        {
            detail.view = decode_view(&detail.raw, mode, cx);
        }
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

/// 会话互斥锁的取用：毒化时返回连接错误（不 panic——UI 线程不该被后台线程的崩溃带走）。
fn lock(
    conn: &Arc<Mutex<RedisConnection>>,
) -> Result<std::sync::MutexGuard<'_, RedisConnection>, RedisError> {
    conn.lock()
        .map_err(|_| RedisError::Command("connection lock poisoned".into()))
}

/// 拒绝原因 → 用户可读文案。
///
/// 返回 `&'static str`（`tr` 的返回类型）：这些文案只进 `notify` / `meta_text`，
/// 不必各自分配一个 `SharedString`。
fn deny_message(reason: DenyReason, cx: &App) -> &'static str {
    match reason {
        DenyReason::ConnectionReadOnly => tr(cx, "该连接的写入策略是「只读」，请在连接设置里改"),
        DenyReason::WriteLocked => tr(cx, "连接当前锁写，请先在状态栏解锁（限时 15 分钟）"),
        DenyReason::AclDenied => tr(cx, "服务端 ACL 未授予写权限，解锁也无法写入"),
    }
}

/// 连接 id：`redis-` + 时间戳后缀（稳定且不依赖随机数源）。
fn new_connection_id() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    format!("redis-{stamp}")
}

fn text_input<C: AppContext>(
    window: &mut Window,
    cx: &mut C,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

/// 从 kv 读连接档案；解析失败回落空表（用户重存即可覆盖）。
///
/// 2026-10-12 换档迁移：旧 wire 是 `read_only: bool`，新 wire 是 `write_policy`
/// 枚举（serde 读不回旧字段）。这里从原始 JSON 把旧布尔的 `true` 抢救成
/// `WritePolicy::ReadOnly`——只对**没有** `write_policy` 字段的条目生效。
fn load_connections(cx: &App) -> Vec<RedisConfig> {
    let raw = duck_store::kv_get(cx, KEY_CONNECTIONS);
    let legacy_read_only: HashMap<String, bool> = raw
        .as_ref()
        .and_then(|json| serde_json::from_str::<Vec<serde_json::Value>>(json).ok())
        .unwrap_or_default()
        .iter()
        .filter(|entry| entry.get("write_policy").is_none())
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            let read_only = entry
                .get("read_only")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            Some((id, read_only))
        })
        .collect();
    raw.as_ref()
        .and_then(|json| serde_json::from_str::<Vec<RedisConfig>>(json).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|mut cfg| {
            if cfg.write_policy == WritePolicy::FollowTag
                && legacy_read_only.get(&cfg.id).copied().unwrap_or(false)
            {
                cfg.write_policy = WritePolicy::ReadOnly;
            }
            cfg
        })
        .collect()
}

/// 把一次读取的结果装配成右栏详情。字符串按 `decode` 档位渲染。
fn build_detail(
    key: String,
    key_type: KeyType,
    ttl: i64,
    value: RedisValue,
    decode: DecodeKind,
    cx: &App,
) -> KeyDetail {
    let size = value.len();
    let truncated = size >= VALUE_PAGE;
    // 字符串的原始字节留一份：切档只用它重算，不再跑一次 `GET`。
    let raw = match &value {
        RedisValue::String(bytes) => bytes.clone(),
        _ => Vec::new(),
    };
    // 模块类型没有通用读法（要 `DUMP`，见设计文档 M1.6）：明确说「没有查看器」，
    // 而不是让内核抛一个英文错误出去。
    let view = if key_type == KeyType::Module {
        ValueView::Unsupported(key_type)
    } else {
        match &value {
            RedisValue::String(bytes) => decode_view(bytes, decode, cx),
            RedisValue::Hash(pairs) => ValueView::Hash(
                pairs
                    .iter()
                    .map(|(field, val)| (lossy(field), lossy(val)))
                    .collect(),
            ),
            RedisValue::List(items) => {
                ValueView::List(items.iter().map(|item| lossy(item)).collect())
            }
            RedisValue::Set(items) => {
                ValueView::Set(items.iter().map(|item| lossy(item)).collect())
            }
            RedisValue::ZSet(pairs) => ValueView::ZSet(
                pairs
                    .iter()
                    .map(|(member, score)| {
                        (lossy(member), duck_redis::value::format_double(*score))
                    })
                    .collect(),
            ),
            RedisValue::Stream(entries) => ValueView::Stream(
                entries
                    .iter()
                    .map(|(id, fields)| {
                        (
                            lossy(id),
                            fields
                                .iter()
                                .map(|(field, val)| (lossy(field), lossy(val)))
                                .collect(),
                        )
                    })
                    .collect(),
            ),
            RedisValue::Missing => ValueView::Missing,
        }
    };
    KeyDetail {
        key,
        key_type,
        ttl,
        size,
        truncated,
        raw,
        view,
    }
}

/// 字节 → 展示态：**按显式档位**解码。
///
/// 解码本身全在内核（`duck_redis::decode`，九档：文本 / JSON / Hex / Base64 /
/// GZIP / JWT / 时间戳 / URL / PHP），这里只负责「档位结果 → 右栏展示态」的适配：
///
/// - 解出的内容能当文本看 → `Text`（附内核给的说明行）；
/// - 解出的是二进制（hex 预览）→ `Binary`；
/// - 解码失败 → `JsonFailed` 复用同一个错误框（文案由内核源串渲染，不写死）；
/// - 体积超过渲染上限 → `TooLarge`，连解码都不做。
fn decode_view(bytes: &[u8], kind: DecodeKind, cx: &App) -> ValueView {
    if bytes.len() > VALUE_RENDER_LIMIT {
        return ValueView::TooLarge { bytes: bytes.len() };
    }
    match redis_decode(kind, bytes) {
        Ok(decoded) => {
            let notice = decoded
                .notice
                .as_ref()
                .map(|message| decode_message(message, cx));
            // 内核明确告诉我们这次解出的是文本还是 hex 预览（`binary`），
            // 界面不靠「猜内容像不像 hex」来判断。
            if decoded.binary {
                ValueView::Binary {
                    hex: decoded.text,
                    bytes: bytes.len(),
                }
            } else {
                ValueView::Text {
                    rendered: decoded.text,
                    notice,
                    bytes: bytes.len(),
                }
            }
        }
        Err(message) => ValueView::JsonFailed {
            error: decode_message(&message, cx),
            bytes: bytes.len(),
        },
    }
}

/// 有损 UTF-8（键名与元素都是二进制安全的，展示时替换坏字节）。
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ----------------------------------------------------------------------
// 服务端观测面板的渲染辅助（M1.4）
// ----------------------------------------------------------------------

/// 面板正文的统一滚动容器。
fn panel_scroll(id: &'static str, body: AnyElement, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    div()
        .size_full()
        .min_w_0()
        .min_h_0()
        .id(id)
        .overflow_y_scroll()
        .p(tokens.spacing.sm)
        .child(body)
        .into_any_element()
}

/// 面板空态（还没拉数据 / 拉回来是空的 / 明确不支持）。
fn panel_empty(message: impl Into<SharedString>, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(tokens.spacing.lg)
        .child(meta_text(message.into(), cx))
        .into_any_element()
}

/// 面板里的输入框（外观与数据浏览的过滤框同源）。
fn panel_input_box(input: &Entity<InputState>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .flex_1()
        .min_w_0()
        .h(theme.density().control)
        .px(tokens.spacing.sm)
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .child(Input::new(input).appearance(false).bordered(false).w_full())
}

/// 当前 Unix 时间戳（秒）。
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 把 Unix 时间戳渲染成「多久以前」。
///
/// **刻意不做绝对时间**：本仓不引时区库，硬凑一个 UTC 字符串会让人误以为是本地时间。
/// 而「这条慢日志是刚发生的还是陈年老账」才是真正要回答的问题，相对时间更直接。
fn relative_age(timestamp: i64, now: i64, cx: &App) -> String {
    let delta = now.saturating_sub(timestamp).max(0);
    if delta < 60 {
        trf(cx, "{} 秒前", &[&delta.to_string()])
    } else if delta < 3_600 {
        trf(cx, "{} 分钟前", &[&(delta / 60).to_string()])
    } else if delta < 86_400 {
        trf(cx, "{} 小时前", &[&(delta / 3_600).to_string()])
    } else {
        trf(cx, "{} 天前", &[&(delta / 86_400).to_string()])
    }
}

/// 字节数的人类可读形态（内存面板用）。
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// 环境标签的展示文案（`None` 档不显示标签）。
fn env_tag_label(tag: EnvTag, cx: &App) -> Option<&'static str> {
    match tag {
        EnvTag::None => None,
        EnvTag::Dev => Some(tr(cx, "开发")),
        EnvTag::Test => Some(tr(cx, "测试")),
        EnvTag::Staging => Some(tr(cx, "预发布")),
        EnvTag::Prod => Some(tr(cx, "生产")),
    }
}

/// 键类型的展示文案。
fn key_type_label(key_type: KeyType, cx: &App) -> &'static str {
    match key_type {
        KeyType::String => tr(cx, "字符串"),
        KeyType::List => tr(cx, "列表"),
        KeyType::Set => tr(cx, "集合"),
        KeyType::ZSet => tr(cx, "有序集合"),
        KeyType::Hash => tr(cx, "哈希"),
        KeyType::Stream => tr(cx, "流"),
        KeyType::Module => tr(cx, "模块类型"),
        KeyType::Missing => tr(cx, "不存在"),
    }
}

/// 访问模式的展示文案。
fn mode_label(mode: AccessMode, cx: &App) -> &'static str {
    match mode {
        AccessMode::ReadWrite => tr(cx, "读写"),
        AccessMode::SafeMode => tr(cx, "安全模式（锁写）"),
        AccessMode::StrictReadOnly => tr(cx, "严格只读（ACL 限制）"),
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
    // ------------------------------------------------------------------
    // 左栏：连接列表。
    // ------------------------------------------------------------------

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let searching = !query.is_empty();

        // 按档案里的分组名分组（无分组归到「默认分组」）。
        let mut groups: Vec<(String, Vec<&RedisConfig>)> = Vec::new();
        for cfg in &self.connections {
            let label = cfg
                .group_name()
                .map(str::to_owned)
                .unwrap_or_else(|| tr(cx, "默认分组").to_owned());
            match groups.iter_mut().find(|(name, _)| *name == label) {
                Some((_, items)) => items.push(cfg),
                None => groups.push((label, vec![cfg])),
            }
        }
        for (_, items) in &mut groups {
            items.retain(|cfg| {
                !searching
                    || cfg.display_name().to_lowercase().contains(&query)
                    || format!("{}:{}", cfg.host, cfg.effective_port())
                        .to_lowercase()
                        .contains(&query)
            });
        }
        groups.retain(|(_, items)| !items.is_empty());

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
                let is_active = self.active.as_deref() == Some(id.as_str());
                let trailing = h_flex()
                    .flex_none()
                    .gap(tokens.spacing.xxs)
                    .children(env_tag_label(cfg.env_tag, cx).map(|tag| neutral_tag(tag, cx)))
                    .child(
                        div()
                            .flex_none()
                            .text_size(tokens.typography.xs.size)
                            .text_color(extended.colors.text_tertiary)
                            .child(SharedString::from(format!("db{}", cfg.db))),
                    )
                    .child(
                        icon_button(
                            SharedString::from(format!("redis-edit-{id}")),
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
                        SharedString::from(format!("redis-conn-{id}")),
                        SharedString::from(cfg.display_name()),
                        Icon::new(DuckIcon::DatabaseZap)
                            .size(extended.icon.md)
                            .text_color(if connected {
                                extended.colors.status_completed
                            } else {
                                extended.colors.status_paused
                            }),
                        trailing,
                        is_active,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        let id = id.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.connect_entry(&id, window, cx);
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
                    .child(caption_heading(tr(cx, "Redis 连接"), cx))
                    .child(
                        icon_button(
                            "redis-new-connection-icon",
                            tr(cx, "新建连接"),
                            Icon::new(DuckIcon::Plus)
                                .size(extended.icon.sm)
                                .text_color(tokens.colors.muted_foreground),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.new_connection(window, cx);
                                });
                            }
                        }),
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
                            .px(tokens.spacing.sm)
                            .rounded(theme.components().input_radius)
                            .border(extended.stroke.thin)
                            .border_color(extended.colors.hairline)
                            .bg(tokens.colors.background)
                            .child(
                                Input::new(&self.search)
                                    .appearance(false)
                                    .bordered(false)
                                    .w_full(),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .id("redis-connections-scroll")
                    .overflow_y_scroll()
                    .child(nav),
            )
            .child(
                div()
                    .flex_none()
                    .p(tokens.spacing.sm)
                    .border_t(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .child(
                        button(
                            "redis-new-connection",
                            tr(cx, "新建连接"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.new_connection(window, cx);
                                });
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 主区：键空间浏览器。
    // ------------------------------------------------------------------

    fn render_browser(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        if self.active_session().is_none() {
            return empty_state(
                DuckIcon::DatabaseZap,
                tr(cx, "未连接"),
                Some(tr(cx, "从左栏选择一条连接，或在左栏底部新建一条。").into()),
                Some(
                    button(
                        "redis-empty-new-connection",
                        tr(cx, "新建连接"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.new_connection(window, cx);
                            });
                        }
                    })
                    .into_any_element(),
                ),
                cx,
            )
            .into_any_element();
        }

        // 主视图切换（M1.4）：数据浏览 ↔ 服务端观测。
        // 服务端视图整屏接管主区（自带页签行），不叠加数据浏览的工具条。
        if self.main_view == MainView::Server {
            return v_flex()
                .size_full()
                .min_w_0()
                .min_h_0()
                .child(self.render_view_switch(cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(self.render_server_view(cx)),
                )
                .into_any_element();
        }

        // 类型过滤条：全部 + 六个可被服务端过滤的类型。
        //
        // `cx` 在这里收成 `&App`：本函数上文已经借走了 `theme`（`active_theme` 返回
        // 全局引用），闭包再要 `&mut Context` 就会与它撞车。只读闭包一律用 `&App`。
        let type_chip = |key_type: Option<KeyType>, cx: &App| {
            let selected = self.type_filter == key_type;
            let label = match key_type {
                None => tr(cx, "全部"),
                Some(key_type) => key_type_label(key_type, cx),
            };
            let id_suffix = key_type
                .map(|key_type| key_type.server_name().unwrap_or("all").to_owned())
                .unwrap_or_else(|| "all".to_owned());
            button(
                SharedString::from(format!("redis-type-{id_suffix}")),
                label,
                if selected {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.type_filter = key_type;
                        this.refresh_keys(window, cx);
                    });
                }
            })
        };

        // 键列表形态切换：平铺（一行一键）↔ 命名空间树（按分隔符分组）。
        let layout_chip = |tree: bool, cx: &App| {
            button(
                if tree {
                    "redis-layout-tree"
                } else {
                    "redis-layout-flat"
                },
                if tree {
                    tr(cx, "树形")
                } else {
                    tr(cx, "平铺")
                },
                if self.tree_mode == tree {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        if this.tree_mode != tree {
                            this.toggle_tree_mode(cx);
                        }
                    });
                }
            })
        };

        let toolbar = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(theme.density().control)
                    .px(tokens.spacing.sm)
                    .rounded(theme.components().input_radius)
                    .border(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .bg(tokens.colors.background)
                    .child(
                        Input::new(&self.pattern)
                            .appearance(false)
                            .bordered(false)
                            .w_full(),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(tokens.spacing.xxs)
                    .child(layout_chip(false, cx))
                    .child(layout_chip(true, cx)),
            )
            .child(
                toolbar_action_button(
                    "redis-refresh",
                    tr(cx, "重新扫描"),
                    // `toolbar_action_button` 的方形内边距按 `icon.md` 算（见其文档与
                    // `toolbar_dropdown_button` 同款公式），传别的档位会让按钮不再是正方形、
                    // 图标也比相邻 chrome 按钮小一档 —— 这里是普通工具栏动作，走 md。
                    Icon::new(DuckIcon::RotateCw).size(extended.icon.md),
                    false,
                    self.busy,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.refresh_keys(window, cx));
                    }
                }),
            );

        let mut type_row = h_flex()
            .flex_none()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap(tokens.spacing.xxs)
            .px(tokens.spacing.sm)
            .pt(tokens.spacing.xs);
        type_row = type_row.child(type_chip(None, cx));
        for key_type in KeyType::SCANNABLE {
            type_row = type_row.child(type_chip(Some(key_type), cx));
        }

        // 键列表（虚拟滚动）。
        let keys = std::sync::Arc::new(self.keys.clone());
        let selected = self.selected;
        let filtered = self.type_filter;
        let row_count = keys.len();
        // `uniform_list` 的闭包要求 `'static`，拿不到借自 `cx` 的 `tokens` / `extended`
        // （同 `db.rs` 网格闭包的做法）：把用到的量提前拷出来。
        let row_gap = tokens.spacing.xs;
        let row_pad = tokens.spacing.sm;
        let row_text = tokens.typography.sm.size;
        let row_line = tokens.typography.sm.line_height;
        let row_fg = tokens.colors.foreground;
        let row_meta = tokens.colors.muted_foreground;
        let row_meta_size = tokens.typography.xs.size;
        let row_selected_bg = extended.colors.nav_selected;
        let row_hover_bg = extended.colors.hover_tint;
        let row_icon = extended.icon.sm;
        // 列表闭包是 `move`：`view` 会被它吃掉，末尾的「继续加载」还要用，先留一份。
        let list_view = view.clone();
        let list = if row_count == 0 {
            empty_state(
                DuckIcon::Search,
                if self.scan_done {
                    tr(cx, "没有键")
                } else {
                    tr(cx, "尚未加载")
                },
                Some(
                    if self.busy {
                        tr(cx, "正在扫描…")
                    } else {
                        tr(cx, "调整过滤串或点「重新扫描」。")
                    }
                    .into(),
                ),
                None,
                cx,
            )
            .into_any_element()
        } else if self.tree_mode {
            // 命名空间树：行来自缓存（`rebuild_tree`），渲染只读不现算。
            let rows = std::sync::Arc::new(self.tree_rows.clone());
            let tree_view = view.clone();
            uniform_list("redis-key-tree", rows.len(), move |range, _window, _cx| {
                range
                    .filter_map(|index| {
                        let row = rows.get(index)?;
                        // 折叠三角：**独立可点**。行本身负责「选中实体键」，
                        // 两者共用一次点击会让「既有键又有子节点」的行没法两者兼顾。
                        // `on_click` 只挂在有 `ElementId` 的元素上（本仓约定），
                        // 所以三角也必须先 `.id()`——裸 `div()` 挂不上。
                        let mut marker = div()
                            .id(SharedString::from(format!("redis-tree-toggle-{index}")))
                            .flex_none()
                            .w(row_icon)
                            .flex()
                            .items_center()
                            .justify_center();
                        if row.has_children {
                            let path = row.path.clone();
                            let expanded = row.expanded;
                            marker = marker
                                .cursor_pointer()
                                .on_click({
                                    let tree_view = tree_view.clone();
                                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                                        let path = path.clone();
                                        let _ = tree_view.update(cx, |this, cx| {
                                            this.toggle_tree_node(path.clone(), !expanded, cx);
                                        });
                                    }
                                })
                                .child(
                                    Icon::new(if row.expanded {
                                        DuckIcon::ChevronDown
                                    } else {
                                        DuckIcon::ChevronRight
                                    })
                                    .size(row_icon)
                                    .text_color(row_meta),
                                );
                        } else {
                            marker = marker.child(
                                Icon::new(DuckIcon::File)
                                    .size(row_icon)
                                    .text_color(row_meta),
                            );
                        }

                        // 跨进 `on_click` 闭包（要求 `'static`）的值必须先拷出来：
                        // `row` 借自 `rows`，生命周期只在本次 `filter_map` 内。
                        let leaf = row.leaf_key.clone();
                        let has_children = row.has_children;
                        let row_expanded = row.expanded;
                        let row_path = row.path.clone();
                        let label = row.label.clone();
                        let indent = row_gap * (row.depth as f32 + 1.0);
                        let count = row.key_count;

                        Some(
                            h_flex()
                                .id(SharedString::from(format!("redis-tree-{index}")))
                                .h(px(KEY_ROW_HEIGHT))
                                .w_full()
                                .flex_none()
                                .items_center()
                                .gap(row_gap)
                                // 缩进按层数乘基础的间距 token，不写裸 px(字号)。
                                .pl(indent)
                                .pr(row_pad)
                                .role(Role::ListItem)
                                .aria_label(SharedString::from(label.clone()))
                                .cursor_pointer()
                                .hover(move |style| style.bg(row_hover_bg))
                                .on_click({
                                    let tree_view = tree_view.clone();
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let _ = tree_view.update(cx, |this, cx| {
                                            match leaf.clone() {
                                                // 挂着实体键 ➜ 选中它。
                                                Some(key) => {
                                                    if let Some(index) = this
                                                        .keys
                                                        .iter()
                                                        .position(|existing| existing == &key)
                                                    {
                                                        this.select_key(index, window, cx);
                                                    }
                                                }
                                                // 纯中间节点 ➜ 折叠 / 展开。
                                                None if has_children => {
                                                    this.toggle_tree_node(
                                                        row_path.clone(),
                                                        !row_expanded,
                                                        cx,
                                                    );
                                                }
                                                None => {}
                                            }
                                        });
                                    }
                                })
                                .child(marker)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(row_text)
                                        .line_height(row_line)
                                        .text_color(row_fg)
                                        .child(SharedString::from(label)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(row_meta_size)
                                        .text_color(row_meta)
                                        .child(SharedString::from(count.to_string())),
                                ),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .flex_1()
            .min_h_0()
            .into_any_element()
        } else {
            uniform_list("redis-key-list", row_count, move |range, _window, _cx| {
                range
                    .filter_map(|index| {
                        let key = keys.get(index)?.clone();
                        Some(
                            h_flex()
                                .id(SharedString::from(format!("redis-key-{index}")))
                                .h(px(KEY_ROW_HEIGHT))
                                .w_full()
                                .flex_none()
                                .items_center()
                                .gap(row_gap)
                                .px(row_pad)
                                .role(Role::ListItem)
                                .aria_label(SharedString::from(key.clone()))
                                .cursor_pointer()
                                .when(selected == Some(index), |row| row.bg(row_selected_bg))
                                .hover(move |style| style.bg(row_hover_bg))
                                .on_click({
                                    let view = list_view.clone();
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.select_key(index, window, cx);
                                        });
                                    }
                                })
                                .child(
                                    Icon::new(match filtered {
                                        Some(key_type) => type_icon(key_type),
                                        None => DuckIcon::File,
                                    })
                                    .size(row_icon)
                                    .text_color(row_meta),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(row_text)
                                        .line_height(row_line)
                                        .text_color(row_fg)
                                        .child(SharedString::from(key)),
                                ),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .flex_1()
            .min_h_0()
            .into_any_element()
        };

        let footer = if !self.scan_done {
            h_flex()
                .flex_none()
                .w_full()
                .justify_center()
                .py(tokens.spacing.sm)
                .border_t(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(
                    button(
                        "redis-load-more",
                        tr(cx, "继续加载"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.load_more_keys(window, cx));
                        }
                    }),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(self.render_view_switch(cx))
            .child(toolbar)
            .child(type_row)
            .child(div().flex_1().min_h_0().child(list))
            .child(footer)
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 服务端观测（M1.4）：视图切换条、页签、七个面板。
    // ------------------------------------------------------------------

    /// 主视图切换条（数据 / 服务端）+ 右侧服务端版本。
    fn render_view_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let selected = match self.main_view {
            MainView::Data => 0,
            MainView::Server => 1,
        };
        let version = self
            .active_session()
            .map(|session| SharedString::from(format!("Redis {}", session.version)));

        h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(segmented_tabs(
                "redis-main-view",
                [
                    SharedString::from(tr(cx, "数据")),
                    SharedString::from(tr(cx, "服务端")),
                ],
                selected,
                move |index, window, cx| {
                    let target = if index == 0 {
                        MainView::Data
                    } else {
                        MainView::Server
                    };
                    let _ = view.update(cx, |this, cx| this.set_main_view(target, window, cx));
                },
                cx,
            ))
            .child(div().flex_1())
            .children(version.map(|version| meta_text(version, cx)))
            .into_any_element()
    }

    /// 服务端观测：面板页签 + 面板动作 + 面板正文。
    fn render_server_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let labels: Vec<SharedString> = ServerPanel::ALL
            .iter()
            .map(|panel| SharedString::from(panel.label(cx)))
            .collect();
        let selected = ServerPanel::ALL
            .iter()
            .position(|panel| *panel == self.panel)
            .unwrap_or(0);

        let tabs = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(segmented_tabs(
                "redis-server-panels",
                labels,
                selected,
                move |index, window, cx| {
                    let Some(panel) = ServerPanel::ALL.get(index).copied() else {
                        return;
                    };
                    let _ = view.update(cx, |this, cx| this.set_server_panel(panel, window, cx));
                },
                cx,
            ))
            .child(div().flex_1())
            .children(self.render_panel_actions(cx));

        // 设计文档 §4.7 的硬口径：缺什么命令就灰显并说明原因，不做品牌特判。
        let missing = self.panel_missing(self.panel);
        let body: AnyElement = if missing.is_empty() {
            match self.panel {
                ServerPanel::Info => self.render_panel_info(cx),
                ServerPanel::Slowlog => self.render_panel_slowlog(cx),
                ServerPanel::Clients => self.render_panel_clients(cx),
                ServerPanel::Config => self.render_panel_config(cx),
                ServerPanel::Memory => self.render_panel_memory(cx),
                ServerPanel::Monitor => self.render_panel_monitor(cx),
                ServerPanel::Cli => self.render_panel_cli(cx),
            }
        } else {
            let commands = missing.join(" / ");
            panel_empty(
                trf(
                    cx,
                    "本服务端没有 {} 命令，无法显示「{}」面板。",
                    &[&commands, self.panel.label(cx)],
                ),
                cx,
            )
        };

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(tabs)
            .child(div().flex_1().min_h_0().min_w_0().child(body))
            .into_any_element()
    }

    /// 面板级动作（刷新 / 开始停止 / 清空）。
    fn render_panel_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = active_theme(cx);
        let icons = theme.extended().icon;
        let view = self.view.clone();
        let mut actions: Vec<AnyElement> = Vec::new();

        match self.panel {
            ServerPanel::Monitor => {
                let running = self.monitor_running;
                actions.push(
                    button(
                        "redis-monitor-toggle",
                        if running {
                            tr(cx, "停止")
                        } else {
                            tr(cx, "开始")
                        },
                        if running {
                            ButtonVariant::Secondary
                        } else {
                            ButtonVariant::Primary
                        },
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                if this.monitor_running {
                                    this.stop_monitor(cx);
                                } else {
                                    this.start_monitor(window, cx);
                                }
                            });
                        }
                    })
                    .into_any_element(),
                );
                actions.push(
                    button(
                        "redis-monitor-clear",
                        tr(cx, "清空"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(self.monitor_lines.is_empty())
                    .on_click({
                        let view = view.clone();
                        move |_, _window, cx| {
                            let _ = view.update(cx, |this, cx| this.clear_monitor(cx));
                        }
                    })
                    .into_any_element(),
                );
            }
            ServerPanel::Cli => {
                actions.push(
                    button(
                        "redis-cli-clear",
                        tr(cx, "清空"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(self.cli_lines.is_empty())
                    .on_click({
                        let view = view.clone();
                        move |_, _window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.cli_lines.clear();
                                this.cli_scroll.scroll_to_bottom();
                                cx.notify();
                            });
                        }
                    })
                    .into_any_element(),
                );
            }
            _ => {
                actions.push(
                    toolbar_action_button(
                        "redis-panel-refresh",
                        tr(cx, "刷新"),
                        Icon::new(DuckIcon::RotateCw).size(icons.md),
                        false,
                        self.panel_busy,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.load_panel(window, cx));
                        }
                    })
                    .into_any_element(),
                );
            }
        }
        actions
    }

    /// 「概览」面板：`INFO` 分节浏览。
    fn render_panel_info(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        if self.panel_data.info_sections.is_empty() {
            return panel_empty(tr(cx, "点右上角「刷新」读取 INFO。"), cx);
        }
        let mut body = v_flex().w_full().gap(tokens.spacing.sm);
        for (section, pairs) in &self.panel_data.info_sections {
            let title = if section.is_empty() {
                tr(cx, "其它")
            } else {
                section.as_str()
            };
            let mut card = v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .p(tokens.spacing.sm)
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(caption_heading(title, cx));
            for (name, value) in pairs {
                card = card.child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap(tokens.spacing.sm)
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(PANEL_LABEL_WIDTH))
                                .truncate()
                                .text_size(tokens.typography.xs.size)
                                .text_color(tokens.colors.muted_foreground)
                                .child(SharedString::from(name.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .text_color(tokens.colors.foreground)
                                .child(SharedString::from(value.clone())),
                        ),
                );
            }
            body = body.child(card);
        }
        panel_scroll("redis-panel-info", body.into_any_element(), cx)
    }

    /// 「慢日志」面板：`SLOWLOG GET`。
    fn render_panel_slowlog(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let can_reset = self
            .gate()
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::ServerAdmin));

        if self.panel_data.slowlog.is_empty() {
            return panel_empty(tr(cx, "没有慢日志记录。"), cx);
        }
        let now = now_secs();
        let mut body = v_flex().w_full().gap(tokens.spacing.xs).child(meta_text(
            trf(
                cx,
                "共 {} 条，显示最近 {} 条。",
                &[
                    &self.panel_data.slowlog_len.to_string(),
                    &self.panel_data.slowlog.len().to_string(),
                ],
            ),
            cx,
        ));
        for entry in &self.panel_data.slowlog {
            body = body.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.xs)
                    .py(tokens.spacing.xxs)
                    .rounded(theme.components().input_radius)
                    .border(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(SLOWLOG_META_WIDTH))
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(format!(
                                "#{}  {} ms  {}",
                                entry.id(),
                                entry.duration_ms(),
                                relative_age(entry.timestamp(), now, cx),
                            ))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(entry.command_line())),
                    ),
            );
        }
        body = body.child(
            h_flex().w_full().justify_end().child(
                button(
                    "redis-slowlog-reset",
                    tr(cx, "清空慢日志"),
                    ButtonVariant::Destructive,
                    cx,
                )
                .disabled(!can_reset)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.reset_slowlog(window, cx));
                    }
                }),
            ),
        );
        panel_scroll("redis-panel-slowlog", body.into_any_element(), cx)
    }

    /// 「客户端」面板：`CLIENT LIST` + `CLIENT KILL`。
    fn render_panel_clients(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let can_kill = self
            .gate()
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::ServerAdmin));

        if self.panel_data.clients.is_empty() {
            return panel_empty(tr(cx, "没有客户端连接。"), cx);
        }
        let mut body = v_flex().w_full().gap(px(1.));
        for client in &self.panel_data.clients {
            let addr = client.addr().to_owned();
            body = body.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.xs)
                    .py(tokens.spacing.xxs)
                    .rounded(theme.components().input_radius)
                    .border_b(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(CLIENT_ADDR_WIDTH))
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(addr.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(CLIENT_SHORT_WIDTH))
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(client.user().to_owned())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(CLIENT_SHORT_WIDTH))
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(trf(cx, "库 {}", &[&client.db().to_string()])),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(CLIENT_SHORT_WIDTH))
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(trf(cx, "空闲 {} 秒", &[&client.idle().to_string()])),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(client.cmd().to_owned())),
                    )
                    .child(
                        button(
                            SharedString::from(format!("redis-client-kill-{}", client.id())),
                            tr(cx, "断开"),
                            ButtonVariant::Destructive,
                            cx,
                        )
                        .disabled(!can_kill)
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let addr = addr.clone();
                                let _ =
                                    view.update(cx, |this, cx| this.kill_client(addr, window, cx));
                            }
                        }),
                    ),
            );
        }
        panel_scroll("redis-panel-clients", body.into_any_element(), cx)
    }

    /// 「配置」面板：`CONFIG GET` 列表 + `CONFIG SET` 编辑。
    fn render_panel_config(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let can_write = self
            .gate()
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::ConfigWrite));

        let filter_row = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(panel_input_box(&self.config_pattern, cx))
            .child(
                button(
                    "redis-config-filter",
                    tr(cx, "过滤"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.load_panel(window, cx));
                    }
                }),
            );

        let edit_row = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(panel_input_box(&self.config_name, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(panel_input_box(&self.config_value, cx)),
            )
            .child(
                button(
                    "redis-config-save",
                    tr(cx, "写入"),
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(!can_write)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.save_config(window, cx));
                    }
                }),
            );

        let mut list = v_flex().w_full().gap(px(1.));
        if self.panel_data.config.is_empty() {
            list = list.child(meta_text(tr(cx, "没有匹配的配置项。"), cx));
        }
        for entry in &self.panel_data.config {
            let name = entry.name().to_owned();
            let click_target = entry.clone();
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("redis-config-{name}")))
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.xxs)
                    .py(px(1.))
                    .rounded(theme.components().input_radius)
                    .role(Role::Button)
                    .aria_label(SharedString::from(trf(cx, "把 {} 回填到编辑框", &[&name])))
                    .cursor_pointer()
                    .hover(move |style| style.bg(extended.colors.hover_tint))
                    .on_click({
                        let view = view.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let entry = click_target.clone();
                            let _ =
                                view.update(cx, |this, cx| this.pick_config(&entry, window, cx));
                        }
                    })
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(PANEL_LABEL_WIDTH))
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(name)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(entry.value().to_owned())),
                    ),
            );
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .child(filter_row)
            .child(edit_row)
            .child(
                div()
                    .id("redis-config-list")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    /// 「内存」面板：`MEMORY DOCTOR` / `MEMORY STATS` / `MEMORY USAGE`。
    fn render_panel_memory(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let doctor = if self.panel_data.memory_doctor.trim().is_empty() {
            tr(cx, "服务端没有给出诊断报告。").to_owned()
        } else {
            self.panel_data.memory_doctor.clone()
        };

        let usage_row = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(panel_input_box(&self.memory_key, cx))
            .child(
                button(
                    "redis-memory-usage",
                    tr(cx, "查询占用"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.query_memory_usage(window, cx));
                    }
                }),
            );

        let mut body = v_flex().w_full().gap(tokens.spacing.sm).child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .p(tokens.spacing.sm)
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(caption_heading(tr(cx, "诊断"), cx))
                .child(body_text(SharedString::from(doctor), cx)),
        );
        body = body.child(usage_row);
        if let Some((key, bytes)) = &self.panel_data.memory_usage {
            let text = match bytes {
                Some(bytes) => trf(cx, "{} 占用 {}", &[key, &human_bytes(*bytes)]),
                None => trf(cx, "{} 不存在。", &[key]),
            };
            body = body.child(meta_text(text, cx));
        }
        if !self.panel_data.memory_stats.is_empty() {
            let mut stats = v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .p(tokens.spacing.sm)
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(caption_heading(tr(cx, "统计"), cx));
            for (name, value) in &self.panel_data.memory_stats {
                stats = stats.child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap(tokens.spacing.sm)
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(PANEL_LABEL_WIDTH))
                                .truncate()
                                .text_size(tokens.typography.xs.size)
                                .text_color(tokens.colors.muted_foreground)
                                .child(SharedString::from(name.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .text_color(tokens.colors.foreground)
                                .child(SharedString::from(value.clone())),
                        ),
                );
            }
            body = body.child(stats);
        }
        panel_scroll("redis-panel-memory", body.into_any_element(), cx)
    }

    /// 「实时监控」面板：`MONITOR` 专用连接上的增量日志。
    fn render_panel_monitor(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        if !self.monitor_running && self.monitor_lines.is_empty() {
            let hint = match &self.monitor_error {
                Some(error) => SharedString::from(error.clone()),
                None => {
                    SharedString::from(tr(cx, "点右上角「开始」，在一条专用连接上开启实时命令流。"))
                }
            };
            return panel_empty(hint, cx);
        }

        let mut lines = v_flex().w_full().gap(px(1.));
        if let Some(error) = &self.monitor_error {
            lines = lines.child(
                div()
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.destructive)
                    .child(SharedString::from(error.clone())),
            );
        }
        for line in &self.monitor_lines {
            lines = lines.child(
                div()
                    .w_full()
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .text_color(tokens.colors.foreground)
                    .child(SharedString::from(line.clone())),
            );
        }

        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .id("redis-monitor-output")
            .overflow_y_scroll()
            .track_scroll(&self.monitor_scroll)
            .p(tokens.spacing.sm)
            .rounded(theme.components().input_radius)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.background)
            .child(lines)
            .into_any_element()
    }

    /// 「终端」面板：redis-cli 式的任意命令。
    fn render_panel_cli(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let mut output = v_flex().w_full().gap(px(1.));
        if self.cli_lines.is_empty() {
            output = output.child(meta_text(
                tr(
                    cx,
                    "输入命令后回车执行；破坏性命令（FLUSHALL 等）会先弹确认框。",
                ),
                cx,
            ));
        }
        for line in &self.cli_lines {
            let color = if line.error {
                tokens.colors.destructive
            } else if line.command {
                tokens.colors.muted_foreground
            } else {
                tokens.colors.foreground
            };
            let text = if line.command {
                format!("> {}", line.text)
            } else {
                line.text.clone()
            };
            output = output.child(
                div()
                    .w_full()
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .text_color(color)
                    .child(SharedString::from(text)),
            );
        }

        let output_box = div()
            .id("redis-cli-output")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.cli_scroll)
            .p(tokens.spacing.sm)
            .rounded(theme.components().input_radius)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.background)
            .child(output);

        // 历史：最近若干条做成可点 chips，点一下回填输入框（比手打快得多）。
        let mut history_row = h_flex()
            .flex_none()
            .w_full()
            .flex_wrap()
            .gap(tokens.spacing.xxs);
        for (index, command) in self
            .cli_history
            .iter()
            .rev()
            .take(CLI_HISTORY_CHIPS)
            .enumerate()
        {
            let label: String = command.chars().take(CLI_HISTORY_CHIP_CHARS).collect();
            let fill = command.clone();
            history_row = history_row.child(
                button(
                    SharedString::from(format!("redis-cli-history-{index}")),
                    SharedString::from(label),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let fill = fill.clone();
                        let _ = view.update(cx, |this, cx| {
                            this.cli_input
                                .update(cx, |input, cx| input.set_value(fill, window, cx));
                        });
                    }
                }),
            );
        }

        let input_row = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(panel_input_box(&self.cli_input, cx))
            .child(
                button("redis-cli-run", tr(cx, "执行"), ButtonVariant::Primary, cx).on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.submit_cli(window, cx));
                    }
                }),
            );

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .child(output_box)
            .children((!self.cli_history.is_empty()).then(|| history_row))
            .child(input_row)
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 右栏：键详情与值。
    // ------------------------------------------------------------------

    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let Some(detail) = &self.detail else {
            return v_flex()
                .size_full()
                .id("redis-right-sidebar")
                .items_center()
                .justify_center()
                .p(tokens.spacing.lg)
                .child(meta_text(
                    if self.active.is_some() {
                        tr(cx, "选择一个键查看详情。")
                    } else {
                        tr(cx, "尚未连接。")
                    },
                    cx,
                ))
                .into_any_element();
        };

        let gate = self.gate();
        let can_delete = gate
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::DeleteKey));
        let can_write = gate
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::WriteKey));

        let ttl_text: SharedString = match detail.ttl {
            -1 => SharedString::from(tr(cx, "永久")),
            -2 => SharedString::from(tr(cx, "键不存在")),
            seconds => trf(cx, "{} 秒", &[&seconds.to_string()]).into(),
        };

        let meta = description_list([
            description_row(
                tr(cx, "键名"),
                body_text(SharedString::from(detail.key.clone()), cx),
            ),
            description_row(
                tr(cx, "类型"),
                body_text(key_type_label(detail.key_type, cx), cx),
            ),
            description_row(tr(cx, "过期时间"), body_text(ttl_text, cx)),
            description_row(
                tr(cx, "元素数"),
                body_text(
                    SharedString::from(if detail.truncated {
                        format!("{}（已截断）", detail.size)
                    } else {
                        detail.size.to_string()
                    }),
                    cx,
                ),
            ),
        ]);

        let mut panel = v_flex()
            .size_full()
            .id("redis-right-sidebar")
            .gap(tokens.spacing.sm)
            .p(tokens.spacing.sm)
            .child(caption_heading(tr(cx, "键详情"), cx))
            .child(meta);

        // 值区域：标题右侧放解码档位（只有字符串值有这三档）。
        panel = panel.child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(caption_heading(tr(cx, "值"), cx))
                .children(decode_chips(detail.key_type, self.decode, &view, cx)),
        );
        let value_body: AnyElement = match &detail.view {
            ValueView::Text {
                rendered,
                notice,
                bytes,
            } => v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .child(
                    div()
                        .w_full()
                        // 滚动面板的**最大高度**是布局约束（撑住值面板的可视区），
                        // 不属 spacing 阶梯，保留裸值。
                        .max_h(px(240.))
                        .id("redis-value-text")
                        .overflow_y_scroll()
                        .p(tokens.spacing.xs)
                        .rounded(theme.components().input_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.background)
                        .text_size(tokens.typography.mono_md.size)
                        .line_height(tokens.typography.mono_md.line_height)
                        .child(SharedString::from(rendered.clone())),
                )
                // 解码档位的说明行（GZIP 解压出多少字节、时间戳按哪个单位解读……）：
                // 没有它，用户不知道 GZIP 档显示的这段字是从哪来的。
                .children(
                    notice
                        .clone()
                        .map(|text| meta_text(SharedString::from(text), cx)),
                )
                .child(meta_text(trf(cx, "{} 字节", &[&bytes.to_string()]), cx))
                .into_any_element(),
            ValueView::Binary { hex, bytes } => v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .child(
                    div()
                        .w_full()
                        // 同上：值面板的可视区上限，与 `redis-value-text` 取同一高度。
                        .max_h(px(240.))
                        .id("redis-value-hex")
                        .overflow_y_scroll()
                        .p(tokens.spacing.xs)
                        .rounded(theme.components().input_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.background)
                        .text_size(tokens.typography.mono_md.size)
                        .line_height(tokens.typography.mono_md.line_height)
                        .child(SharedString::from(hex.clone())),
                )
                .child(meta_text(
                    trf(cx, "二进制内容，共 {} 字节", &[&bytes.to_string()]),
                    cx,
                ))
                .into_any_element(),
            ValueView::TooLarge { bytes } => meta_text(
                trf(cx, "值过大（{} 字节），未自动加载。", &[&bytes.to_string()]),
                cx,
            )
            .into_any_element(),
            ValueView::JsonFailed { error, bytes } => v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .child(
                    div()
                        .w_full()
                        .p(tokens.spacing.xs)
                        .rounded(theme.components().input_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.background)
                        .text_size(tokens.typography.sm.size)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(trf(
                            cx,
                            "不是合法 JSON：{}",
                            &[error.as_str()],
                        ))),
                )
                .child(meta_text(
                    trf(
                        cx,
                        "共 {} 字节，可切到「文本」或「Hex」档查看原文。",
                        &[&bytes.to_string()],
                    ),
                    cx,
                ))
                .into_any_element(),
            ValueView::Hash(pairs) => hash_rows(&detail.key, pairs, &view, cx),
            ValueView::ZSet(pairs) => zset_rows(&detail.key, pairs, &view, cx),
            ValueView::List(items) => list_rows(&detail.key, items, self.list_editing, &view, cx),
            ValueView::Set(items) => set_rows(&detail.key, items, &view, cx),
            ValueView::Stream(entries) => {
                let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
                for (id, fields) in entries {
                    list = list.child(
                        v_flex()
                            .w_full()
                            .gap(tokens.spacing.xxs)
                            .p(tokens.spacing.xs)
                            .rounded(theme.components().input_radius)
                            .border(extended.stroke.thin)
                            .border_color(extended.colors.hairline)
                            .child(
                                div()
                                    .text_size(tokens.typography.xs.size)
                                    .text_color(tokens.colors.muted_foreground)
                                    .child(SharedString::from(id.clone())),
                            )
                            .children(fields.iter().map(|(field, value)| {
                                div()
                                    .text_size(tokens.typography.sm.size)
                                    .text_color(tokens.colors.foreground)
                                    .child(SharedString::from(format!("{field}: {value}")))
                            })),
                    );
                }
                list.into_any_element()
            }
            ValueView::Missing => meta_text(
                tr(cx, "键已不存在（可能在选中之后被其他客户端删掉了）。"),
                cx,
            )
            .into_any_element(),
            ValueView::Unsupported(key_type) => meta_text(
                trf(
                    cx,
                    "「{}」类型的专用查看器尚未实现（计划在后续增量补上）。",
                    &[key_type_label(*key_type, cx)],
                ),
                cx,
            )
            .into_any_element(),
        };
        panel = panel.child(
            div()
                .w_full()
                // 整个值区（含标题、格式化切换）的滚动上限，布局约束、非间距。
                .max_h(px(320.))
                .id("redis-value-scroll")
                .overflow_y_scroll()
                .child(value_body),
        );

        // 按类型给编辑入口（字符串 / 哈希 / 列表 / 集合 / 有序集合各一套字段）。
        panel = panel.child(value_editor(self, detail.key_type, &view, can_write, cx));

        if !can_write || !can_delete {
            panel = panel.child(meta_text(
                gate.as_ref()
                    .and_then(|gate| gate.check(Capability::WriteKey).err())
                    .map(|reason| deny_message(reason, cx))
                    .unwrap_or_else(|| tr(cx, "当前连接为只读。")),
                cx,
            ));
        }

        // 键管理（TTL / 重命名）与删除键同属「危险操作区」，排在一起。
        panel = panel.child(key_ops(self, &view, can_write, cx));

        panel = panel.child(
            button(
                "redis-delete-key",
                tr(cx, "删除此键"),
                ButtonVariant::Destructive,
                cx,
            )
            .disabled(!can_delete)
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.delete_selected_key(window, cx);
                    });
                }
            }),
        );

        panel.into_any_element()
    }

    // ------------------------------------------------------------------
    // 状态栏。
    // ------------------------------------------------------------------

    fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let mode = self.access_mode();
        let locked = self.gate().is_some_and(|gate| gate.is_locked());

        let mut bar = h_flex()
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
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(self.status.clone()),
            );

        if let Some(session) = self.active_session() {
            bar = bar.child(neutral_tag(SharedString::from(session.version.clone()), cx));
        }
        bar = bar.child(meta_text(mode_label(mode, cx), cx));
        if self.active.is_some() {
            bar = bar.child(meta_text(
                SharedString::from(trf(
                    cx,
                    "{} / {} 键",
                    &[&self.keys.len().to_string(), &self.dbsize.to_string()],
                )),
                cx,
            ));
        }

        if locked {
            bar = bar.child(
                button(
                    "redis-unlock-writes",
                    trf(
                        cx,
                        "解锁写操作（{} 分钟）",
                        &[&(WRITE_UNLOCK_SECS / 60).to_string()],
                    ),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.unlock_writes(window, cx));
                    }
                }),
            );
        } else if self.active.is_some() {
            bar = bar.child(
                button(
                    "redis-disconnect",
                    tr(cx, "断开"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.disconnect_active(window, cx));
                    }
                }),
            );
        }

        bar.into_any_element()
    }
}

/// 类型 → 图标（列表里按类型过滤时用）。
fn type_icon(key_type: KeyType) -> DuckIcon {
    match key_type {
        KeyType::String => DuckIcon::FileText,
        KeyType::List => DuckIcon::ListFilter,
        KeyType::Set => DuckIcon::Folders,
        KeyType::ZSet => DuckIcon::Layers,
        KeyType::Hash => DuckIcon::Columns2,
        KeyType::Stream => DuckIcon::Film,
        KeyType::Module | KeyType::Missing => DuckIcon::File,
    }
}

/// 行内的小动作按钮（删除 / 修改共用）。
///
/// `label` 走 i18n 的 `tr`，同时兼作无障碍名——图标按钮**必须**有名字，
/// 否则读屏只能念出一个「按钮」（见 `AGENTS.md` §3.6 的 a11y 条文）。
fn row_icon_button(
    id: String,
    label: &'static str,
    icon: DuckIcon,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let extended = theme.extended();
    icon_button(
        SharedString::from(id),
        label,
        Icon::new(icon)
            .size(extended.icon.sm)
            .text_color(theme.tokens().colors.muted_foreground),
        ButtonVariant::Ghost,
        cx,
    )
    .on_click(on_click)
    .into_any_element()
}

/// 行尾删除回调（哈希 / 有序集合用：入参是被删项的**主键**）。
type PairDelete = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// 两列 + 行尾删除的表格（哈希 / 有序集合共用）。
///
/// `on_delete` 的入参是**左列的值**——哈希的字段名、有序集合的成员名，
/// 两者都是该项在服务端的主键，按它删不会误删同值的其他项。
fn pair_rows(
    id_prefix: &str,
    key: &str,
    left_head: &'static str,
    right_head: &'static str,
    pairs: &[(String, String)],
    on_delete: PairDelete,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let head = |text: &'static str| {
        div()
            .flex_1()
            .min_w_0()
            .text_size(tokens.typography.xs.size)
            .text_color(tokens.colors.muted_foreground)
            .child(SharedString::from(tr(cx, text)))
    };
    let mut table = v_flex().w_full().gap(tokens.spacing.xxs).child(
        h_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .child(head(left_head))
            .child(head(right_head)),
    );
    for (index, (left_value, right_value)) in pairs.iter().enumerate() {
        let cell = |value: &str| {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.sm.size)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(value.to_owned()))
        };
        let target = left_value.clone();
        table = table.child(
            h_flex()
                .id(SharedString::from(format!("{id_prefix}-{key}-{index}")))
                .w_full()
                .gap(tokens.spacing.xs)
                .child(cell(left_value))
                .child(cell(right_value))
                .child(row_icon_button(
                    format!("{id_prefix}-del-{index}"),
                    tr(cx, "删除此项"),
                    DuckIcon::Trash2,
                    {
                        let on_delete = on_delete.clone();
                        move |_, window, cx| on_delete(target.clone(), window, cx)
                    },
                    cx,
                )),
        );
    }
    table.into_any_element()
}

/// 哈希：字段 / 值 / 删除。
fn hash_rows(
    key: &str,
    pairs: &[(String, String)],
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> AnyElement {
    let on_delete: PairDelete = {
        let view = view.clone();
        Rc::new(move |field, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.delete_hash_field(field.clone(), window, cx);
            });
        })
    };
    pair_rows("redis-hash", key, "字段", "值", pairs, on_delete, cx)
}

/// 有序集合：成员 / 分数 / 删除。
fn zset_rows(
    key: &str,
    pairs: &[(String, String)],
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> AnyElement {
    let on_delete: PairDelete = {
        let view = view.clone();
        Rc::new(move |member, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.delete_zset_member(member.clone(), window, cx);
            });
        })
    };
    pair_rows("redis-zset", key, "成员", "分数", pairs, on_delete, cx)
}

/// 行内额外动作的构造器（列表放「改」，集合没有）。
type RowExtra = Rc<dyn Fn(usize, &str, &App) -> Option<AnyElement>>;
/// 行尾删除的回调：收到 `(行下标, 行内容)`。
type RowDelete = Rc<dyn Fn(usize, String, &mut Window, &mut App)>;

/// 单列 + 行尾动作的表格（列表 / 集合共用）。
///
/// `on_delete` 收到 `(行下标, 行内容)`：**集合按值删**（值即集合的成员标识），
/// **列表按位置删**（值可能重复，按值删会删错行）。
fn single_rows(
    id_prefix: &str,
    key: &str,
    items: &[String],
    extra: &RowExtra,
    on_delete: &RowDelete,
    delete_label: &'static str,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let mut table = v_flex().w_full().gap(tokens.spacing.xxs);
    for (index, item) in items.iter().enumerate() {
        let target = item.clone();
        table = table.child(
            h_flex()
                .id(SharedString::from(format!("{id_prefix}-{key}-{index}")))
                .w_full()
                .gap(tokens.spacing.xs)
                .child(
                    div()
                        .flex_none()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.muted_foreground)
                        .child(SharedString::from(index.to_string())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.sm.size)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(item.clone())),
                )
                .children(extra(index, item.as_str(), cx))
                .child(row_icon_button(
                    format!("{id_prefix}-del-{index}"),
                    delete_label,
                    DuckIcon::Trash2,
                    {
                        let on_delete = on_delete.clone();
                        move |_, window, cx| on_delete(index, target.clone(), window, cx)
                    },
                    cx,
                )),
        );
    }
    table.into_any_element()
}

/// 集合：值 / 删除（按值删，值即成员标识）。
fn set_rows(key: &str, items: &[String], view: &WeakEntity<RedisPage>, cx: &App) -> AnyElement {
    let on_delete: RowDelete = {
        let view = view.clone();
        Rc::new(move |_index, member, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.delete_set_member(member.clone(), window, cx);
            });
        })
    };
    let extra: RowExtra = Rc::new(|_, _, _| None);
    single_rows(
        "redis-set",
        key,
        items,
        &extra,
        &on_delete,
        tr(cx, "删除此成员"),
        cx,
    )
}

/// 列表：序号 / 值 / 改 / 删（按位置删，值可能重复）。
fn list_rows(
    key: &str,
    items: &[String],
    editing: Option<i64>,
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> AnyElement {
    let extra: RowExtra = {
        let view = view.clone();
        Rc::new(move |index, value: &str, app: &App| {
            // 正在编辑的那一行不再给入口：再点一次只是把同一个值重填一遍。
            if editing == Some(index as i64) {
                return None;
            }
            let value = value.to_owned();
            let view = view.clone();
            Some(row_icon_button(
                format!("redis-list-edit-{index}"),
                tr(app, "修改此行"),
                DuckIcon::Code,
                move |_, window, cx| {
                    let value = value.clone();
                    let _ = view.update(cx, |this, cx| {
                        this.edit_list_item(index as i64, value.clone(), window, cx);
                    });
                },
                app,
            ))
        })
    };
    let on_delete: RowDelete = {
        let view = view.clone();
        Rc::new(move |index, _value, window, cx| {
            let _ = view.update(cx, |this, cx| {
                this.delete_list_item(index as i64, window, cx);
            });
        })
    };
    single_rows(
        "redis-list",
        key,
        items,
        &extra,
        &on_delete,
        tr(cx, "删除此元素"),
        cx,
    )
}

// ── 右栏的值编辑器与键管理（M1.2）───────────────────────────────────────

/// 「标签 + 输入框」的表单单元。
fn editor_field(label: &'static str, input: &Entity<InputState>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    v_flex()
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.xxs)
        .child(field_label(tr(cx, label), cx))
        .child(input_shell(input, false, false, cx))
}

/// 解码档位切换（只有字符串值有这九档；M1.5 起从三档扩到九档）。
///
/// 九档在右栏一行放不下，所以允许换行（`flex_wrap`）：档位再多也不会把右栏撑破。
fn decode_chips(
    key_type: KeyType,
    current: DecodeKind,
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> Option<AnyElement> {
    if key_type != KeyType::String {
        return None;
    }
    let tokens = active_theme(cx).tokens();
    let mut row = h_flex().flex_wrap().gap(tokens.spacing.xxs);
    for mode in DecodeKind::ALL {
        row = row.child(
            button(
                SharedString::from(format!("redis-decode-{}", mode.wire_name())),
                decode_label(mode, cx),
                if mode == current {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.switch_decode(mode, cx));
                }
            }),
        );
    }
    Some(row.into_any_element())
}

/// 类型相关的值编辑表单（每个类型只给该类型有意义的字段）。
///
/// `enabled` = 写闸门放行；为假时全部按钮禁用，原因由调用方在下文统一说明
/// （不给每个按钮配一句提示，那会把右栏塞满重复文案）。
fn value_editor(
    page: &RedisPage,
    key_type: KeyType,
    view: &WeakEntity<RedisPage>,
    enabled: bool,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let mut form = v_flex().w_full().gap(tokens.spacing.xs);

    match key_type {
        KeyType::String => {
            form = form
                .child(
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.xxs)
                        .child(field_label(tr(cx, "编辑字符串值"), cx))
                        .child(
                            div()
                                .w_full()
                                // 字符串值编辑框的固定高度（约 4 行文本），布局约束、非间距。
                                .h(px(64.))
                                .px(tokens.spacing.xs)
                                .py(tokens.spacing.xs)
                                .rounded(theme.components().input_radius)
                                .border(theme.extended().stroke.thin)
                                .border_color(theme.extended().colors.hairline)
                                .bg(tokens.colors.background)
                                .child(
                                    Input::new(&page.edit_buffer)
                                        .appearance(false)
                                        .bordered(false)
                                        .w_full(),
                                ),
                        ),
                )
                .child(
                    button(
                        "redis-save-value",
                        tr(cx, "保存值"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.save_string_value(window, cx));
                        }
                    }),
                );
        }
        KeyType::Hash => {
            form = form
                .child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(editor_field("字段名", &page.hash_field, cx))
                        .child(editor_field("字段值", &page.hash_value, cx)),
                )
                .child(
                    button(
                        "redis-hash-write",
                        tr(cx, "写入字段"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.save_hash_field(window, cx));
                        }
                    }),
                );
        }
        KeyType::List => {
            form = form.child(editor_field("元素值", &page.list_value, cx));
            // 底部表单有两种态：正在改某一行（LSET）或追加（LPUSH / RPUSH）。
            // 两种态的按钮长得不一样，用户才不会点错。
            form = match page.list_editing {
                Some(_) => form.child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(
                            button(
                                "redis-list-apply",
                                tr(cx, "保存这一行"),
                                ButtonVariant::Primary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.save_list_value(false, window, cx);
                                    });
                                }
                            }),
                        )
                        .child(
                            button(
                                "redis-list-cancel",
                                tr(cx, "取消修改"),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.cancel_list_edit(window, cx);
                                    });
                                }
                            }),
                        ),
                ),
                None => form.child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(
                            button(
                                "redis-list-push-front",
                                tr(cx, "左推入"),
                                ButtonVariant::Primary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.save_list_value(true, window, cx);
                                    });
                                }
                            }),
                        )
                        .child(
                            button(
                                "redis-list-push-back",
                                tr(cx, "右推入"),
                                ButtonVariant::Primary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.save_list_value(false, window, cx);
                                    });
                                }
                            }),
                        ),
                ),
            };
            if let Some(index) = page.list_editing {
                form = form.child(meta_text(
                    trf(cx, "正在修改第 {} 行", &[&index.to_string()]),
                    cx,
                ));
            }
        }
        KeyType::Set => {
            form = form
                .child(editor_field("成员", &page.set_member, cx))
                .child(
                    button(
                        "redis-set-add",
                        tr(cx, "加入成员"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.add_set_member(window, cx));
                        }
                    }),
                );
        }
        KeyType::ZSet => {
            form = form
                .child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(editor_field("成员", &page.zset_member, cx))
                        .child(editor_field("分数", &page.zset_score, cx)),
                )
                .child(
                    button(
                        "redis-zset-write",
                        tr(cx, "写入成员"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                // 底部表单只做「新增」；改分数/改名走每一行自己的入口。
                                this.save_zset_member(None, window, cx);
                            });
                        }
                    }),
                );
        }
        // 流、模块类型与缺失键都没有通用写入口：给一句实话，而不是一堆禁用按钮。
        KeyType::Stream | KeyType::Module | KeyType::Missing => {
            form = form.child(meta_text(tr(cx, "该类型暂无可用的结构化编辑入口。"), cx));
        }
    }
    form.into_any_element()
}

/// 键管理：设置 / 移除过期时间、重命名。
fn key_ops(page: &RedisPage, view: &WeakEntity<RedisPage>, enabled: bool, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(caption_heading(tr(cx, "键管理"), cx))
        .child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(h_flex().w_full().gap(tokens.spacing.xs).child(editor_field(
                    "过期时间",
                    &page.ttl_input,
                    cx,
                )))
                .child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(
                            button(
                                "redis-ttl-set",
                                tr(cx, "设置过期时间"),
                                ButtonVariant::Primary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| this.set_ttl(window, cx));
                                }
                            }),
                        )
                        .child(
                            button(
                                "redis-ttl-clear",
                                tr(cx, "移除过期时间"),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| this.clear_ttl(window, cx));
                                }
                            }),
                        ),
                ),
        )
        .child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(editor_field("新的键名", &page.rename_input, cx))
                .child(
                    button(
                        "redis-rename",
                        tr(cx, "重命名"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.rename_key(window, cx));
                        }
                    }),
                ),
        )
        .into_any_element()
}

// ── 连接弹窗（对齐 zedis 的「添加新服务器」表单）─────────────────────────

/// 弹窗里输入框的统一外壳：`Input` 关掉自带外观，套本仓的 hairline 圆角框。
///
/// `mask` 打开时带密码小眼睛（zedis 同款）；`disabled` 用于「档位存在但尚未接线」
/// 的灰显占位——此时不要造输入实体（会进焦点链），用 [`disabled_shell`]。
fn input_shell(input: &Entity<InputState>, mask: bool, disabled: bool, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .h(theme.density().control)
        .w_full()
        .min_w_0()
        .px(tokens.spacing.sm)
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .when(disabled, |shell| shell.opacity(extended.opacity.disabled))
        .child({
            let input = Input::new(input)
                .appearance(false)
                .bordered(false)
                .disabled(disabled)
                .w_full();
            if mask { input.mask_toggle() } else { input }
        })
}

/// 未接线档位的灰显占位壳：空盒子 + 说明文字，不造输入实体（不进焦点链）。
fn disabled_shell(placeholder: &'static str, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .h(theme.density().control)
        .w_full()
        .min_w_0()
        .px(tokens.spacing.sm)
        .flex()
        .items_center()
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .opacity(extended.opacity.disabled)
        .child(meta_text(tr(cx, placeholder), cx))
}

/// 单选档位行：一排薄底按钮（与状态栏 / 工具条的按钮族同一套外观）。
///
/// `on_pick` 收到 wire 值；调用方负责回写表单并 `window.refresh()`。
fn choice_row(
    id_prefix: &'static str,
    options: &[(&'static str, &'static str)],
    current_wire: &str,
    on_pick: impl Fn(&str, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let tokens = active_theme(cx).tokens();
    let on_pick = Rc::new(on_pick);
    h_flex()
        .flex_wrap()
        .items_center()
        .gap(tokens.spacing.xxs)
        .children(options.iter().map(|(wire, label)| {
            let selected = *wire == current_wire;
            let wire = (*wire).to_owned();
            let on_pick = Rc::clone(&on_pick);
            button(
                SharedString::from(format!("{id_prefix}-{wire}")),
                tr(cx, label),
                if selected {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(move |_, window, cx| on_pick(&wire, window, cx))
        }))
}

/// 常规页签。
fn render_tab_general(form: &Entity<EditForm>, cx: &App) -> Div {
    let name = form.read(cx).name.clone();
    let host = form.read(cx).host.clone();
    let port = form.read(cx).port.clone();
    let acl_user = form.read(cx).acl_user.clone();
    let password = form.read(cx).password.clone();
    let description = form.read(cx).description.clone();
    let group = form.read(cx).group.clone();

    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        // 名称单独一行并自带必填标记（与 zedis 的 `名称 *` 对齐）。
        .child(form_field(
            tr(cx, "名称 *"),
            input_shell(&name, false, false, cx),
            None,
            None,
            cx,
        ))
        // 主机 + 端口是同一个地址：一行，端口占 1/3（zedis 的 span 9/3）。
        .child(form_row(
            [
                form_field(
                    tr(cx, "主机 *"),
                    input_shell(&host, false, false, cx),
                    None,
                    None,
                    cx,
                )
                .flex_1()
                .min_w_0()
                .into_any_element(),
                form_field(
                    tr(cx, "端口"),
                    input_shell(&port, false, false, cx),
                    None,
                    None,
                    cx,
                )
                .flex_1()
                .min_w_0()
                .into_any_element(),
            ],
            cx,
        ))
        .child(form_field(
            tr(cx, "用户名"),
            input_shell(&acl_user, false, false, cx),
            None,
            Some(tr(cx, "仅用于 Redis 6.0+（ACL）；留空表示只用口令认证").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "密码"),
            div()
                .w_full()
                .child(
                    Input::new(&password)
                        .appearance(false)
                        .bordered(false)
                        .mask_toggle()
                        .w_full(),
                )
                .h(active_theme(cx).density().control)
                .px(active_theme(cx).tokens().spacing.sm)
                .rounded(active_theme(cx).components().input_radius)
                .border(active_theme(cx).extended().stroke.thin)
                .border_color(active_theme(cx).extended().colors.hairline)
                .bg(active_theme(cx).tokens().colors.background),
            None,
            Some(tr(cx, "加密存本机，不入连接档案").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "描述"),
            input_shell(&description, false, false, cx),
            None,
            None,
            cx,
        ))
        .child(form_field(
            tr(cx, "分组"),
            input_shell(&group, false, false, cx),
            None,
            None,
            cx,
        ))
}

/// SSL/TLS 页签：档位真实可存（内核会诚实报错），证书族等 M1.5。
fn render_tab_tls(form: &Entity<EditForm>, cx: &App) -> Div {
    let form_for_pick = form.clone();
    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(form_field(
            tr(cx, "TLS"),
            choice_row(
                "redis-form-tls",
                &[
                    ("disable", "不启用"),
                    ("trust", "校验证书"),
                    ("danger-accept-invalid", "接受无效证书"),
                ],
                form.read(cx).tls.wire_name(),
                move |wire, window, cx| {
                    form_for_pick.update(cx, |f, _| f.tls = TlsMode::from_wire(wire));
                    window.refresh();
                },
                cx,
            ),
            None,
            Some(
                tr(
                    cx,
                    "本版本未启用 TLS：选非「不启用」档会在连接时明确报错，不会静默降级成明文。",
                )
                .into(),
            ),
            cx,
        ))
        .child(form_field(
            tr(cx, "服务器名称指示（SNI）"),
            disabled_shell("M1.5 接入", cx),
            None,
            Some(tr(cx, "M1.5 接入：内核尚未启用 TLS 特性。").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "客户端证书 / 根证书"),
            disabled_shell("M1.5 接入", cx),
            None,
            None,
            cx,
        ))
}

/// SSH 隧道页签：档案可先填（M1.5 前建连会明确报错，绝不静默直连）。
fn render_tab_ssh(form: &Entity<EditForm>, cx: &App) -> Div {
    let (ssh_host, ssh_port, ssh_user, ssh_password, ssh_remote_host, enabled) = {
        let state = form.read(cx);
        (
            state.ssh_host.clone(),
            state.ssh_port.clone(),
            state.ssh_user.clone(),
            state.ssh_password.clone(),
            state.ssh_remote_host.clone(),
            state.ssh_enabled,
        )
    };

    let form_for_toggle = form.clone();
    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(field_label(tr(cx, "启用 SSH 隧道"), cx))
                .child(switch_control(
                    "redis-form-ssh-enabled",
                    enabled,
                    move |value: &bool, window, cx| {
                        form_for_toggle.update(cx, |f, _| f.ssh_enabled = *value);
                        window.refresh();
                    },
                )),
        )
        .when(enabled, |this| {
            this.child(form_row(
                [
                    form_field(
                        tr(cx, "SSH 地址"),
                        input_shell(&ssh_host, false, false, cx),
                        None,
                        None,
                        cx,
                    )
                    .flex_1()
                    .min_w_0()
                    .into_any_element(),
                    form_field(
                        tr(cx, "端口"),
                        input_shell(&ssh_port, false, false, cx),
                        None,
                        None,
                        cx,
                    )
                    .flex_1()
                    .min_w_0()
                    .into_any_element(),
                ],
                cx,
            ))
            .child(form_field(
                tr(cx, "SSH 用户名"),
                input_shell(&ssh_user, false, false, cx),
                None,
                None,
                cx,
            ))
            .child(form_field(
                tr(cx, "SSH 密码"),
                input_shell(&ssh_password, true, false, cx),
                None,
                Some(tr(cx, "加密存本机，不入连接档案").into()),
                cx,
            ))
            .child(form_field(
                tr(cx, "转发目标"),
                input_shell(&ssh_remote_host, false, false, cx),
                None,
                Some(tr(cx, "相对跳板机解析；留空则连跳板机本机").into()),
                cx,
            ))
        })
        .child(field_hint(
            tr(cx, "SSH 密钥与跳板机（ProxyJump）在 M1.5 接入；当前版本开隧道会在连接时明确报错，绝不静默直连。"),
            cx,
        ))
}

/// 高级页签。
fn render_tab_advanced(form: &Entity<EditForm>, cx: &App) -> Div {
    let db = form.read(cx).db.clone();
    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(form_field(
            trf(cx, "默认数据库（0-{}）", &[&MAX_DB.to_string()]),
            input_shell(&db, false, false, cx),
            None,
            Some(tr(cx, "建连后立即 SELECT 该库").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "数据库数量"),
            disabled_shell("M1.4 接入", cx),
            None,
            Some(tr(cx, "M1.4 接入：留空自动探测（CONFIG GET databases）").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "连接 / 响应超时"),
            disabled_shell("M1.4 接入", cx),
            None,
            Some(tr(cx, "M1.4 接入；当前用系统级缺省").into()),
            cx,
        ))
        .child(field_hint(
            tr(
                cx,
                "运行模式（单机 / 哨兵 / 集群）在 M1.6 接入，当前仅支持单机直连。",
            ),
            cx,
        ))
}

/// 安全页签：环境标签 + 写入策略 + 写确认（分级锁写的全部输入）。
fn render_tab_safety(form: &Entity<EditForm>, cx: &App) -> Div {
    let env_options: Vec<(&'static str, &'static str)> = EnvTag::ALL
        .iter()
        .map(|tag| match tag {
            EnvTag::None => ("none", "无标签"),
            EnvTag::Dev => ("dev", "开发"),
            EnvTag::Test => ("test", "测试"),
            EnvTag::Staging => ("staging", "预发布"),
            EnvTag::Prod => ("prod", "生产"),
        })
        .collect();
    let form_for_tag = form.clone();
    let form_for_policy = form.clone();
    let form_for_confirm = form.clone();
    let require_confirm = form.read(cx).require_confirm;

    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(form_field(
            tr(cx, "环境标签"),
            choice_row(
                "redis-form-env",
                &env_options,
                form.read(cx).env_tag.wire_name(),
                move |wire, window, cx| {
                    form_for_tag.update(cx, |f, _| f.env_tag = EnvTag::from_wire(wire));
                    window.refresh();
                },
                cx,
            ),
            None,
            Some(tr(cx, "给连接列表着色分组，防「把生产库当测试库」。").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "写入"),
            choice_row(
                "redis-form-policy",
                &[
                    ("follow-tag", "跟随标签"),
                    ("allow", "允许"),
                    ("lock", "锁定，每次解锁 15 分钟"),
                    ("read-only", "只读"),
                ],
                form.read(cx).write_policy.wire_name(),
                move |wire, window, cx| {
                    form_for_policy
                        .update(cx, |f, _| f.write_policy = WritePolicy::from_wire(wire));
                    window.refresh();
                },
                cx,
            ),
            None,
            Some(
                tr(
                    cx,
                    "「跟随标签」= 生产档默认锁写；「允许」压过生产锁；「锁定」在任何档位都锁。",
                )
                .into(),
            ),
            cx,
        ))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(field_label(tr(cx, "写入确认"), cx))
                .child(switch_control(
                    "redis-form-confirm-writes",
                    require_confirm,
                    move |value: &bool, window, cx| {
                        form_for_confirm.update(cx, |f, _| f.require_confirm = *value);
                        window.refresh();
                    },
                )),
        )
        .child(field_hint(
            tr(
                cx,
                "打开后，该连接上每条写命令在执行前都要二次确认（M1.3 收尾接线）。",
            ),
            cx,
        ))
}

/// 键页签：键树 / SCAN 行为的按连接覆盖。
fn render_tab_keys(form: &Entity<EditForm>, cx: &App) -> Div {
    let separator = form.read(cx).separator.clone();
    let scan_count = form.read(cx).scan_count.clone();
    let tree_depth = form.read(cx).tree_depth.clone();
    let auto_expand = form.read(cx).auto_expand.clone();
    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(form_field(
            tr(cx, "键分隔符"),
            input_shell(&separator, false, false, cx),
            None,
            Some(tr(cx, "键树按它切命名空间；留空则使用 :").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "扫描键数量"),
            input_shell(&scan_count, false, false, cx),
            None,
            Some(tr(cx, "每次 SCAN 的 COUNT；留空则使用界面默认值").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "键树最大深度"),
            input_shell(&tree_depth, false, false, cx),
            None,
            Some(tr(cx, "超过这个层数就不再切分；留空则使用 8 层").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "自动展开阈值"),
            input_shell(&auto_expand, false, false, cx),
            None,
            Some(tr(cx, "子树键数不超过它就默认展开；留空或 0 表示不自动展开").into()),
            cx,
        ))
}

/// 弹窗正文：页签条 + 当前页签的表单。
fn render_form_tabs(form: &Entity<EditForm>, cx: &App) -> Stateful<Div> {
    let tab = form.read(cx).tab;
    let tokens = active_theme(cx).tokens();
    let form_for_select = form.clone();

    let body = match tab {
        1 => render_tab_tls(form, cx),
        2 => render_tab_ssh(form, cx),
        3 => render_tab_advanced(form, cx),
        4 => render_tab_safety(form, cx),
        5 => render_tab_keys(form, cx),
        _ => render_tab_general(form, cx),
    }
    .into_any_element();

    let tabs = segmented_tabs(
        "redis-form-tabs",
        FORM_TABS
            .iter()
            .map(|label| SharedString::from(tr(cx, label))),
        tab,
        move |index, window, cx| {
            form_for_select.update(cx, |f, _| f.tab = index);
            window.refresh();
        },
        cx,
    );

    div()
        .id("redis-form-body")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.md)
                .px(tokens.spacing.lg)
                .py(tokens.spacing.md)
                .child(tabs)
                .child(body),
        )
}

/// 弹窗底部：左侧「测试 / 删除」，右侧「取消 / 添加」。
fn redis_form_footer(
    form: &Entity<EditForm>,
    view: &WeakEntity<RedisPage>,
    is_new: bool,
    cx: &App,
) -> DialogFooter {
    let test_view = view.clone();
    let test_form = form.clone();
    let ok_view = view.clone();
    let ok_form = form.clone();
    let delete_view = view.clone();
    let delete_id = form.read(cx).id.clone();
    let theme = active_theme(cx);
    let tokens = theme.tokens();

    let left = h_flex().gap(tokens.spacing.sm).children(vec![
        button(
            "redis-form-test",
            tr(cx, "测试"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(move |_, window, cx| {
            let _ = test_view.update(cx, |this, cx| this.test_connection(&test_form, window, cx));
        })
        .into_any_element(),
    ]);

    let right = h_flex().gap(tokens.spacing.sm).children({
        let mut items: Vec<AnyElement> = Vec::new();
        if let Some(id) = delete_id {
            items.push(
                button(
                    "redis-form-delete",
                    tr(cx, "删除此连接"),
                    ButtonVariant::Destructive,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = delete_view.update(cx, |this, cx| {
                        this.delete_connection(&id, window, cx);
                    });
                    window.close_dialog(cx);
                })
                .into_any_element(),
            );
        }
        items.push(
            button(
                "redis-form-cancel",
                tr(cx, "取消"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(|_, window, cx| window.close_dialog(cx))
            .into_any_element(),
        );
        items.push(
            button(
                "redis-form-ok",
                tr(cx, if is_new { "添加" } else { "保存" }),
                ButtonVariant::Primary,
                cx,
            )
            .on_click(move |_, window, cx| {
                let saved = ok_view
                    .update(cx, |this, cx| this.save_form(&ok_form, window, cx))
                    .unwrap_or(true);
                if saved {
                    window.close_dialog(cx);
                }
            })
            .into_any_element(),
        );
        items
    });

    DialogFooter::new()
        .justify_between()
        .child(left)
        .child(right)
}
