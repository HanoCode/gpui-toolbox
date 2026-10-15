//! Redis 页：连接管理（左栏）+「键空间浏览 | 值编辑器」（主区两栏）+ 键详情（右栏）。
//!
//! 布局与交互范式沿用本仓 `DatabasePage`（`db.rs`）与 `TerminalPage`：
//! 左栏走页面内 [`SidebarPanel`]，右栏走 shell 级 `with_right_sidebar`。
//! **主区自己也是两栏**（P3，对齐 zedis 的值编辑区）：左＝键树 / 平铺列表，
//! 右＝值编辑器，中间可拖（[`h_resizable`] + [`resizable_panel`]）。
//! 值不再挤在 shell 右栏里——那里只回答「关于这个键」，值回答「它长什么样」。
//! 新建 / 编辑连接走**模态弹窗**（`duck_components::dialog` 一族），分页签
//! 常规 / SSL/TLS / SSH 隧道 / 高级 / 安全 / 键——信息架构对齐 zedis 的
//! 「添加新服务器」表单（`temp/zedis/src/views/servers/form.rs`）。
//!
//! **本增量范围**（`docs/redis-design.md` §6 的 M1.1 + M1.2 + P3 + P4 + M1.6 键标注与回收站）：
//! 连接档案 CRUD 与分组、连通测试、服务端版本、`SCAN` 键空间浏览（分页 / 三档查询模式 /
//! 类型与过期时间过滤 / 排序 / 虚拟滚动 / 命名空间树）、键详情（类型 / 内部编码 / 空闲时间 /
//! TTL / 元素数）、六种类型的值查看、字符串**九档解码**（文本 / JSON / Hex / Base64 / GZIP /
//! JWT / 时间戳 / URL / PHP）、**值面板两栏 + 文本 / 树两档呈现**（JSON 可折叠树）、
//! **五类结构化值编辑**（字符串 / 哈希字段 / 列表元素 / 集合成员 / 有序集合成员）、
//! 设置与移除 TTL、重命名键、删除键、**状态栏右半段**（库选择器 / 延迟 / 内存 / 类型构成，
//! 见 `status.rs`）、**键的本地标注**（标签色 / 备注 / 收藏，纯本地零网络，见 `tags.rs`）、
//! **本地回收站**（删键前抄一份、24h 可恢复，见 `trash.rs`），
//! 以及**分级锁写闸门**的全链路接入（灰显 + 说明原因 + 限时解锁）。
//! **未做**（完整清单见 `docs/redis-design.md` §6「当前进度」）：破坏性命令的
//! **逐字输入连接名**确认（现在用「正文写清要动什么」的弹窗确认替代）、Metrics 时序采样、
//! Stream 编辑器、TLS / SSH 隧道、Cluster / Sentinel / ACL / Pub-Sub / 模块类型查看器等。
//!
//! 弹窗里**尚未接线**的档位一律以禁用控件 + 「M1.x 接入」说明呈现：
//! TLS 证书族与隧道建立（M1.5）、运行模式 / 哨兵与集群（M1.6）。
//!
//! # 两条实现纪律
//!
//! 1. **线程**：`duck-redis` 全部是阻塞 API，本页每条 Redis 操作都经
//!    [`Self::spawn_redis`] 丢进 `background_executor`，结果回主线程后再 `cx.notify()`；
//!    渲染路径上不做任何网络调用。
//! 2. **类型与 TTL 走批量管道、不走逐键往返**：逐键 `TYPE` + `TTL` 在一个 500 键的页面上
//!    要 1000 次往返。`RedisConnection::key_meta` 把每 `META_CHUNK_KEYS`（256）个键的
//!    `TYPE`/`TTL` 交替压进**一个管道**，一趟拿回一批；结果落 `key_meta` 缓存，
//!    键树的**类型徽标与 TTL 芯片**都读它（渲染期不现取）。平铺列表只在启用了类型过滤时
//!    按 `SCAN TYPE` 的类型画图标，其余用中性键图标；精确类型仍由右栏选中后取。
//!    ⚠️ **唯一的例外是 `OBJECT ENCODING` / `IDLETIME`**：它们**只对选中的那一个键**
//!    单独发（两条命令一次往返），绝不进这条批量管道——加进去会让每键的命令数从 2 变 4。
//!
//! # 顶栏四档：哪些会重扫，哪些只在本地算
//!
//! 键空间浏览工具条有四个档位，按「**这个问题要问谁**」分成两类（设计文档 §4.5.1）：
//!
//! | 档位 | 判据落在哪 | 换档后 |
//! |---|---|---|
//! | 查询模式（`*` 子串 / `^` 前缀 / `=` 精确，[`QueryMode`]） | 服务端 `SCAN MATCH` | [`Self::refresh_keys`] 重扫 |
//! | 键类型（`type_filter`） | 服务端 `SCAN TYPE` | 同上 |
//! | 过期时间过滤（[`TtlFilter`]，7 档） | 本地 `key_meta` 缓存 | [`Self::set_ttl_filter`] 只重算视图 |
//! | 排序（[`KeySort`]，4 档） | 本地 `key_meta` 缓存 | [`Self::set_sort`] 只重算视图 |
//!
//! `MATCH` 串怎么拼由内核的 `duck_redis::query::scan_plan` 一处决定（三档都把关键字
//! 当**字面量**转义），所以「重扫」与「继续加载」问的必然是同一个问题。
//!
//! # 值面板：两条正交的轴（P3，2026-10-15）
//!
//! 「值怎么显示」被拆成两条互不干涉的轴，因为把它们塞进一个开关会立刻自相矛盾：
//!
//! | 轴 | 档位 | 回答的问题 | 换了之后 |
//! |---|---|---|---|
//! | **解码**（[`DecodeKind`]） | 9 档（文本 / JSON / Hex / Base64 / GZIP / JWT / 时间戳 / URL / PHP） | 字节**怎么变成**文字 | 重算展示态，不再跑网络（原始字节留在 `KeyDetail::raw`） |
//! | **呈现**（[`ValueViewMode`]） | 2 档（文本 / 树） | 这段文字**怎么摆** | 本地展开集合决定，零往返 |
//!
//! - **树档只在 JSON 解码档下成立**（建树在 [`build_detail`] 一处决定）：解码出 JSON
//!   才谈得上折叠。不做「文本档里恰好是合法 JSON 也偷偷给树」——同一段文字在文本档下
//!   看不了树，这个档位按钮的含义就不可预测了。
//! - **呈现档位是偏好，不是当前是否生效**：[`RedisPage::value_view`] 留着用户的偏好，
//!   非 JSON 键上 [`RedisPage::effective_value_view`] 自动回落到文本；换回一个 JSON 键
//!   就自动回到树档，不必再点一次。
//! - **展开状态跟着键走**：[`RedisPage::adopt_json_tree`] 在换键时按默认档（展开两层）
//!   重来，同一个键重新加载（写完刷新 / 手动刷新）保留用户的折叠状态。
//! - 折叠只在**本地**算：`flatten` 把节点树摊成行存在 `json_rows`（`Arc`），
//!   渲染期只读——每帧重算一遍会为两万节点各克隆一次 `path` / `name` / `preview`。
//!
//! ⚠️ 值本体的树档用 `uniform_list`，所以它**直接挂在右栏的 `v_flex` 下**，
//! 与键列表同一个坑（见 `browser.rs::render_browser` 的注释与无头回归测试）。
//!
//! # 状态栏：两个半段各有各的写入方（P4，2026-10-15）
//!
//! 左半段是「这一页刚做了什么」（`status`，由各功能自己写），右半段是「这台服务端
//! 现在什么样」（版本 / 访问模式 / 库 / 键数 / 延迟 / 内存 / 类型构成，**只有
//! `status.rs` 的探测写**）。一个数字一个写入方，才不会有「谁把它覆盖了」的问题。
//!
//! - **探测只在事件上跑，不轮询**：连接成功 / 切库 / 点右端「重新探测」。
//!   ⚠️ 探测走 `spawn_probe` 而**不是** `spawn_redis`——它不动 `busy`（`busy` 是
//!   「有用户在等的操作在跑」的语义，见该函数的说明）。
//! - **类型构成只数已加载的键**（`key_meta` 里有元数据的那部分），零命令；
//!   逐类型精确计数要六七次全库遍历，与「不用 `KEYS`」同一条取舍。
//! - ⚠️ **切库改的是连接档案并落盘**，不是会话级覆盖（`switch_db`）；顺序必须是
//!   「先 `SELECT` 成功、再写档案」，反过来会在失败时留下界面与连接永久错位的状态。
//!
//! # 键的本地标注：标签 / 备注 / 收藏（M1.6 首批，2026-10-15）
//!
//! 每个键可以挂**一个标签色 + 一段备注 + 一个收藏标记**，对标 zedis
//! `zedis-db/src/key_metadata_manager.rs`。这是 M1.6 里**唯一零网络、零新依赖**的一项：
//! 所有数据只在本机 kv（`redis_key_tags`），**一个字节也不发往服务端**，所以它也是本模块
//! 唯一**不过写闸门**的写操作。模型与取舍见 `tags.rs`（内核）与本文件下方。
//!
//! - **收藏与标签正交**：`KeyTags` 里 `entries`（标签 + 备注）与 `favorites` 是两个容器，
//!   所以「收藏了但没打标签」是可表达的状态，不是靠「tag = 某个灰」去凑。
//! - **文件夹色 = 后代众数**：内核 `scan::stamp_folder_tags` 一次后序遍历算子树六色直方图，
//!   内节点取众数（平票取下标较小者，稳定不抖），`tag_mixed` 为真时左缘色条画成**虚线**。
//! - **过滤在树构建前**：`TagFilter`（4 聚合档 + 6 颜色档）在 `rebuild_tree` / `rebuild_flat`
//!   构建**之前**筛键集，否则被滤掉的键仍会参与文件夹聚合，出现「文件夹有颜色但底下没键」。
//! - ⚠️ **标注只跟显式删除 / 显式清空走，绝不跟 SCAN 走**：SCAN 是分页的，当前页看不到的键
//!   **不代表已被删**；按「已加载的键」裁剪标注会把其它页的标注整片抹掉。改动前必读。
//! - ⚠️ **备注自动保存（无按钮）**：`note_input` 订阅 `InputEvent::Change`，值变即
//!   `set_key_note`；比对相等就早退，避免回填自身触发死循环。`TreeRow` 只存 `has_note: bool`
//!   （不存正文——百万键 × 每行一个 `String` 是纯浪费）。
//! - **重绘范围按影响面区分**：改颜色**必须**重建视图（影响所有祖先文件夹的聚合），改备注
//!   只在「有无备注」这个布尔翻转时才重建，收藏变化重建（影响星标与 Favorites 档）。
//!
//! # 本地回收站：软删除的键 24 小时可恢复（M1.6 第二批，2026-10-15）
//!
//! 删键那条路**照旧只发 `DEL`**；额外的 `MEMORY USAGE` / `PTTL` / `DUMP` 都在删除**之前**
//! 发一次，把「即将被删的那一份」抄进本地 kv（`redis_trash`）。模型与四个常量见内核
//! `duck_redis::trash`。
//!
//! - **抄不下来绝不影响删除**：任何一步失败都只表示「这一份不入站」（对齐 zedis
//!   `stash_key_to_trash` 的口径）。回收站是安全网，不是一个能让删除失败的依赖。
//! - **两道体积闸**：`MEMORY USAGE`（4 MiB，估算）先挡掉「一看就太大」的键、省掉一次昂贵的
//!   `DUMP`；`DUMP` 回来后按精确长度（1 MiB）再判一次。超了**永久删除、不入站**。
//! - **恢复另开专用连接**（`restore_entries`）：条目跨库列出，`RESTORE` 又落在当前
//!   `SELECT` 的库上——在页面那条活会话上换来换去会与状态栏缓存的 db 脱节。
//! - ⚠️ **`RESTORE` 不带 `REPLACE`**：同名键存在 ⇒ `BUSYKEY` ⇒ 算「跳过」，不覆盖。
//! - **恢复过写闸门**（`Capability::WriteKey`），**本地摘行不过**（改的是本机 kv）。
//! - ⚠️ **弹窗是独立视图**：正文有过滤输入框，要边打边过滤；页面的 `notify()` 到不了
//!   `content` 闭包，所以 `TrashDialog` 自己 `notify()` 并 `observe` 页面。
//! - ⚠️ **列表容器不能用 `max_h`**：`Scrollable` 会 clamp 内容本身、永远滚不动；
//!   超过 7 条换成定高（320pt）视口。
//!
//! # 文件划分（P3-3，2026-10-14 拆自单文件 `redis.rs`，零行为变更）
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `mod.rs` | 模块文档 / use / 常量 / 全部类型 / `RedisPage` / `impl Render` / 页面壳方法 |
//! | `helpers.rs` | 自由函数：锁、拒绝文案、连接档案读写、档位下拉骨架、面板骨架、时间与标签 |
//! | `connect.rs` | 连接生命周期：新建 / 编辑 / 测试 / 激活 / 写锁 / 持久化 |
//! | `keys.rs` | 键浏览与键级操作：分页 / 树 / 选中 / 重命名 / TTL / 删除确认 |
//! | `tags.rs` | 键的本地标注：标签 / 备注 / 收藏的读写、标签过滤档、文案与取色（零网络） |
//! | `trash.rs` | 本地回收站：删键前抄一份（两道体积闸）、24h 保留、跨库 `RESTORE` 恢复、弹窗与动作 |
//! | `writes.rs` | 五类结构化值的写入与删除 |
//! | `panels.rs` | 服务端面板的数据拉取与渲染（慢日志 / 客户端 / 配置 / 内存 / 监视器 / CLI） |
//! | `form.rs` | 连接编辑弹窗：六页签表单、页签外壳与页脚 |
//! | `browser.rs` | 键浏览主区（含主区两栏的装配）、结果行渲染、视图切换 |
//! | `sidebar.rs` | 左栏连接列表与 shell 级右栏**键详情**（值不在这里） |
//! | `value.rs` | 值详情构造、九档解码、状态栏之外的**呈现档位**（文本 / 树）与值编辑器 |
//! | `status.rs` | 状态栏：探测调度（`PING` 计时 + `INFO`）、四段渲染、DB 切换、类型统计 |
//!
//! 两条口径（与 `pages/settings/` 相同）：
//! 1. 子模块靠 `use super::*;` 取父模块的私有 `use` 绑定与私有类型，不必复制 use 块；
//! 2. 子模块里 `pub(super)` == 可见于 `redis` 及其全部后代，与原来「模块私有」同一范围。

use std::collections::{HashMap, HashSet, VecDeque};
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
    sidebar_navigation_button, switch_control, toolbar_action_button, toolbar_text_dropdown_button,
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
    AclProbe, ClientInfo, ConfigEntry, ConfirmKind, DbKeyStats, JsonTreeNode, JsonTreeRow, KeyMeta,
    KeyObjectInfo, KeySort, KeyTags, MemoryOverview, MonitorSession, QueryMode, RedisConnection,
    RedisError, RedisValue, ScanPlan, ServerFeatures, SlowlogEntry, Sortable, StatusOverview,
    TagColor, TagFilter, TagStore, TrashEntry, TrashMeta, TrashStore, TtlFilter, WRITE_UNLOCK_SECS,
    build_tree_sorted, confirm_kind, default_expanded, flatten, format_ttl_chip, is_destructive,
    now_millis, parse_info_sections, payload_fits, scan_plan, should_auto_expand, should_stash,
    stamp_folder_tags, toggle_path, tree_of, ttl_chip_kind,
};
use duck_theme::{UnderMinute, active_theme, tf, tr, tr_relative_time, trf};
use gpui::{
    Anchor, AnyElement, App, AppContext, ClickEvent, Context, Div, Entity, InteractiveElement as _,
    IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::{
    Icon, ResizableState, WindowExt as _,
    dialog::{DialogButtonProps, DialogFooter},
    h_flex, h_resizable,
    input::{Input, InputEvent, InputState},
    menu::PopupMenuItem,
    resizable_panel, v_flex,
};

mod browser;
mod connect;
mod form;
mod helpers;
mod keys;
mod panels;
mod sidebar;
mod status;
mod tags;
mod trash;
mod value;
mod writes;

// 含自由函数 / 类型别名的子模块，名字要转出到父模块供兄弟模块取用。
use self::browser::*;
use self::form::*;
use self::helpers::*;
use self::tags::*;
use self::trash::*;
use self::value::*;

/// kv 单键：连接档案数组（结构体 → serde_json，范式同 `appearance` / `db_connections`）。
const KEY_CONNECTIONS: &str = "redis_connections";
/// kv 单键：**全部连接**的键标注（标签色 / 备注 / 收藏）。
///
/// 一台连接一份（`TagStore` 里按连接 id 分表），所以这里只需要一个键；
/// 拆成「一连接一个 kv 键」反而要在删连接时枚举前缀去清。
const KEY_TAGS: &str = "redis_key_tags";
/// kv 单键：**全部连接**的本地回收站（软删除的键，24 小时内可恢复）。
///
/// 与 [`KEY_TAGS`] 同款：一台连接一份（`TrashStore` 里按连接 id 分表），一个键就够。
/// ⚠️ 里面装的是 `DUMP` 载荷（base64），所以它是本模块唯一一个**可能有十几 MB** 的
/// kv 值——内核用「单条 1 MiB + 条目数 / 总字节两条预算」把它封住（见 `duck_redis::trash`）。
const KEY_TRASH: &str = "redis_trash";
/// 键树行左缘的标签色条宽度。
///
/// 3px：够在 24px 行高里被看见，又不至于把行首的缩进推得歪掉。
/// 未标色的行**照样占这个宽度**（透明色）——否则同一列键名会随「有没有标签」
/// 左右跳一像素。
const TREE_TAG_BAR_WIDTH: f32 = 3.;
/// 右栏标签色块的边长。
///
/// 16px：七个小方块（「无」+ 六档）连间隙正好铺满右栏宽度；再大就换行了。
const TAG_SWATCH: f32 = 16.;
/// 左栏宽度档位（与 `db.rs` 保持一致，两页切换时侧栏不跳宽）。
const SIDEBAR_WIDTH_RANGE: RangeInclusive<f32> = 184.0..=340.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 236.;
/// 键列表行高：与 `typography.sm` 的 18px 行高同源。
const KEY_ROW_HEIGHT: f32 = 24.;
/// JSON 树的行高：比键行略矮（树里一行只有「名字 + 值」，没有徽标）。
const JSON_TREE_ROW_HEIGHT: f32 = 22.;
/// 主区右侧值面板的宽度档位（可拖拽）。
///
/// 下限 320：再窄就连一个 JSON 值的标量预览都放不下（`JSON_TREE_PREVIEW_CHARS`
/// 是 80 个字符）。上限 760：超过这个宽度，键树会被挤成一条竖线，
/// 而值本身早就能在面板里横向滚动了。
const VALUE_PANE_RANGE: RangeInclusive<f32> = 320.0..=760.0;
/// 值面板出厂宽度：JSON 的默认展开两档正好一屏、又给键树留出大半。
const VALUE_PANE_DEFAULT: f32 = 460.;
/// 树里**类型徽标列**的固定宽度（zedis `TYPE_BADGE_COL_WIDTH`）：
/// 定宽才能让不同长度的徽标（`SET` / `LIST` / `ZSET`）后面的键名对齐。
const TREE_TYPE_COL_WIDTH: f32 = 36.;
/// 树里 **TTL 芯片列**的固定宽度（zedis `TTL_CHIP_WIDTH`）：
/// 右对齐的定宽槽，滚动时不会因为标签长短（`59s` / `52w`）而抖动。
const TREE_TTL_COL_WIDTH: f32 = 34.;
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

/// 状态栏一次探测的结果：延迟 / 内存 / 各库键数（`docs/redis-design.md` §4.8.2）。
///
/// 三样东西合成一个结构，是因为它们**只在同一次探测里一起更新**（一次计时 `PING`
/// + 一条 `INFO memory` / `INFO keyspace` 的 pipeline）。拆成三个字段只会让
/// 「换连接 / 换库 / 断开时该清哪几个」出现三种答案。
///
/// ⚠️ **类型统计不在这里**：它按已加载的键元数据在界面侧算（[`RedisPage::type_census`]），
/// 内核不为它发任何命令。
#[derive(Default)]
struct StatusProbe {
    /// 一次 `PING` 往返耗时；还没探测过 / 探测失败时为 `None`。
    latency: Option<Duration>,
    /// `INFO memory` 的概览（`used_memory_human` / 上限 / 淘汰策略）。
    memory: Option<MemoryOverview>,
    /// `INFO keyspace` 的各库键数（只含非空库）。
    keyspace: Vec<DbKeyStats>,
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

/// 值面板的**呈现档位**（同一段文字怎么摆）。
///
/// 与解码档位（[`DecodeKind`]，九档）是**两条正交的轴**：解码回答「字节怎么变成
/// 文字」，呈现回答「这段文字怎么显示」。所以「JSON + 树」和「文本 + 树」
/// 放在一起是说不通的——树档只在解码出 JSON 时才成立。
///
/// ⚠️ 只对**字符串值**有意义：哈希 / 列表 / 集合 / 有序集合 / 流各有自己的
/// 行式渲染，本来就不是「一段文字」，没有树可折。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ValueViewMode {
    /// 原样显示解码结果。
    #[default]
    Text,
    /// 可折叠树（只有 JSON 档解析成功才有）。
    Tree,
}

impl ValueViewMode {
    const ALL: [Self; 2] = [Self::Text, Self::Tree];

    /// 元素 id 后缀（也用于单测断言，所以是稳定的 ASCII）。
    fn wire_name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Tree => "tree",
        }
    }
}

/// 呈现档位的显示名。
fn value_view_label(mode: ValueViewMode, cx: &App) -> &'static str {
    match mode {
        ValueViewMode::Text => tr(cx, "文本"),
        ValueViewMode::Tree => tr(cx, "树"),
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
    /// 「树」档的节点树。
    ///
    /// **只在 JSON 解码档且解析成功时**才建（其余档没有树可言，`None`）。
    /// 建树是一次纯内存操作、随详情一起算好，渲染期只做展平查表。
    tree: Option<JsonTreeNode>,
    /// `OBJECT ENCODING`（内部编码）。拿不到就是 `None`（见 `KeyObjectInfo`）。
    encoding: Option<String>,
    /// `OBJECT IDLETIME` 的秒数（空闲时长）。同上，拿不到就不显示。
    idletime_secs: Option<u64>,
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
            auto_expand: text_input(window, cx, tr(cx, "留空用默认 100；0 表示不自动展开")),
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
    /// 值面板的呈现档位（文本 / 树）。
    ///
    /// 这是**用户的偏好**，不保证当前键上能生效：非 JSON 值会自动按文本显示
    /// （见 [`Self::effective_value_view`]），偏好原样留着——换回一个 JSON 键
    /// 就自动回到树档，不必再点一次。
    value_view: ValueViewMode,
    /// JSON 树档的展开集合属于**哪个键**（`None` = 还没有树）。
    ///
    /// 与 `json_expanded` 配对使用：换键时按默认档（展开两层）重来，
    /// 同一个键重新加载则保留用户的折叠状态。
    json_key: Option<String>,
    /// JSON 树档里展开的容器路径集合。
    json_expanded: HashSet<String>,
    /// JSON 树档展平后的行（含折叠、含「还有 N 项」占位行）。
    ///
    /// 与键树的 `tree_rows` 同理：**只在展开状态变化时重建，渲染只读**。
    /// 包在 `Arc` 里是因为行渲染闭包要 `'static`——每帧克隆一个 `Arc`
    /// 只是加一次引用计数，克隆整个 `Vec<JsonTreeRow>`（每条路径都是 `String`）
    /// 会随节点数线性烧内存。
    json_rows: Arc<Vec<JsonTreeRow>>,
    /// 主区右侧值面板的分栏状态（宽度可拖）。
    ///
    /// ⚠️ **宽度不另存一份**：`ResizableState` 自己持有 `sizes`，`resizable_panel()`
    /// 的 `.size()` 只是「创建时的初始宽度」。再存一个字段就是第二个真源，
    /// 拖动之后两边迟早对不上。
    value_pane: Entity<ResizableState>,

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
    /// 键元数据缓存（类型 + TTL）：键树的**类型徽标与 TTL 芯片**都读它。
    ///
    /// 值由 [`RedisConnection::key_meta`] 的 pipeline **按页批量**取回
    /// （256 键一次往返，见 `META_CHUNK_KEYS`），**渲染路径只读不拉**；
    /// 键被重新扫描时整表作废旧数据（见 `refresh_keys`）。
    key_meta: HashMap<String, KeyMeta>,
    /// `SCAN` 游标（`0` = 遍历结束）。
    scan_cursor: u64,
    /// 是否已遍历完整个键空间。
    scan_done: bool,
    /// 当前库的键总数（`DBSIZE`）。
    dbsize: u64,
    /// 类型过滤（`None` = 不过滤）。
    type_filter: Option<KeyType>,
    /// 键名查询模式（`*` 子串 / `^` 前缀 / `=` 精确）。
    ///
    /// **改变它等于换了一个问题**（服务端 `MATCH` 串随之变化），所以切换后要重扫，
    /// 而不是像 TTL / 排序那样只重算本地视图。
    query_mode: QueryMode,
    /// TTL 范围过滤：**纯本地**，只筛 `key_meta` 里已经取到 TTL 的键，不发任何往返。
    ///
    /// 元数据还没回来的键不命中任何受限档（见 `TtlFilter::matches`）——
    /// 界面只按已知的样本过滤，绝不为了一次筛选把整个键空间跑一遍 `TTL`。
    ttl_filter: TtlFilter,
    /// 同层排序档（键树兄弟节点与平铺列表共用）。
    sort: KeySort,
    /// 平铺列表的**可见行**：`keys` 的下标序列（已按 TTL 档过滤 + 按排序档排）。
    ///
    /// 存下标而不是键名副本，是为了让 `selected: Option<usize>` 继续指向 `keys`
    /// ——排序不改变「谁是选中项」，只改变它出现在第几行。
    flat_rows: Vec<usize>,
    /// 选中的键下标。
    selected: Option<usize>,
    /// 右栏键详情。
    detail: Option<KeyDetail>,
    /// 状态栏文案。
    status: SharedString,
    /// 状态栏右半段的探测结果（延迟 / 内存 / 各库键数）。
    ///
    /// 探测**只在事件上跑**（连接后、切库、写操作后、点刷新），不做定时轮询：
    /// 一个空闲页面每秒往服务端发 `PING` + 两条 `INFO` 是纯浪费，而本模块的
    /// 两条硬纪律都是冲着「别给服务端添无谓负载」去的。
    probe: StatusProbe,
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
    /// 本地键标注（标签色 / 备注 / 收藏）：**全部连接**一份，落盘进 `KEY_TAGS`。
    ///
    /// 一台连接一张子表（[`TagStore`] 按连接 id 分），当前连接的那张走
    /// [`Self::active_tags`]。**只有一个真源**：页面不再另存一份「当前连接的标注」，
    /// 否则切连接时两份迟早对不上。
    ///
    /// ⚠️ 标注是**用户手写**的数据：换连接、重扫、翻页都不会动它；
    /// 只有「用户显式删键」会顺手丢掉那个键的标注（见 [`Self::delete_selected_key`]）。
    tags: TagStore,
    /// 标签档过滤：与过期时间过滤同一层（**纯本地**，零往返）。
    tag_filter: TagFilter,
    /// 本地回收站：软删除的键（`DUMP` 载荷，24 小时可恢复）。
    ///
    /// 与 [`Self::tags`] 一样按连接 id 分表（[`TrashStore`]）。**唯一**会装二进制
    /// 载荷的本地数据，体积闸与预算都在内核里（见 [`KEY_TRASH`] 与 `duck_redis::trash`）。
    trash: TrashStore,
    /// 右栏「备注」编辑框。
    ///
    /// ⚠️ 它**跟着选中的键走**（换键要回填），所以是唯一一个「内容由状态驱动」
    /// 的输入框；写回时先跟本地标注比一次，避免回填动作被当成用户输入。
    note_input: Entity<InputState>,

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
    /// 实体键的类型（纯中间节点为 `None`）——类型徽标用。
    key_type: Option<KeyType>,
    /// 实体键的剩余秒数（`-1` 永久 / `-2` 未知，见 `duck_redis::ttl`）。
    ///
    /// 纯中间节点（文件夹行）为 `None`：TTL 是**键**的属性，命名空间没有 TTL。
    ttl_secs: Option<i64>,
    /// 这一行左缘色条的颜色。
    ///
    /// 键行 = 该键自己的标签色；文件夹行 = **后代标签的众数色**
    /// （由 [`stamp_folder_tags`] 在构树时算好）。`None` = 不画色条
    /// （未标色的键 / 后代一个标色都没有的文件夹）。
    tag: Option<TagColor>,
    /// 文件夹行：后代的标签色**不止一种**（渲染成虚线色条）。
    ///
    /// 键行恒为 `false`——一个键只有一个颜色，谈不上「混」。
    tag_mixed: bool,
    /// 键行是否已收藏（文件夹行恒为 `false`）。
    favorite: bool,
    /// 键行有没有备注（文件夹行恒为 `false`）。
    ///
    /// 只存**有没有**，不存正文：正文可能上百字，而一百万个键里各存一份 `String`
    /// 是白烧内存——真要读全文，选中这个键，右栏里就有。
    has_note: bool,
}

/// 展平一棵树时**逐层不变**的那几个入参。
///
/// 打包成一处，是为了让 [`flatten_tree`] 的递归调用点收得住参数个数
/// （单拆开是 8 个，clippy 的 `too_many_arguments` 会拦）。
struct TreeShape<'a> {
    /// 命名空间分隔符（组 path 用）。
    separator: &'a str,
    /// 用户显式的展开 / 收起覆盖（键 = 节点 path）。
    overrides: &'a HashMap<String, bool>,
    /// 覆盖表里没有的节点，用它推导是否展开（[`should_auto_expand`]）。
    auto_expand: usize,
    /// 键元数据缓存（类型 + TTL）。
    meta: &'a HashMap<String, KeyMeta>,
    /// 当前连接的本地标注（标签色 / 收藏）——键行按它上色、加星。
    ///
    /// 文件夹行的颜色**不从这里查**：它在构树时就被 [`stamp_folder_tags`]
    /// 聚合进了 [`TreeNode::tag`]（那是 O(节点数) 的活，不能挪到渲染期）。
    tags: &'a KeyTags,
}

/// 把命名空间树展平成行列表（深度优先，只展开处于展开态的节点）。
///
/// 纯函数：`shape.overrides` 是用户显式的展开 / 收起记录，未记录时按
/// `shape.auto_expand` 阈值推导（[`should_auto_expand`]）；
/// `shape.meta` 是键元数据缓存（类型 + TTL），**只查不补**——没取到就渲染成无徽标无芯片。
fn flatten_tree(
    nodes: &[TreeNode],
    depth: usize,
    prefix: &str,
    shape: &TreeShape<'_>,
    out: &mut Vec<TreeRow>,
) {
    for node in nodes {
        let path = if prefix.is_empty() {
            node.label.clone()
        } else {
            format!("{prefix}{}{}", shape.separator, node.label)
        };
        let has_children = !node.children.is_empty();
        let expanded = shape
            .overrides
            .get(&path)
            .copied()
            .unwrap_or_else(|| should_auto_expand(node, shape.auto_expand));
        // 有键：每个键成一行（限深时同一节点下可能挤着多个键）。
        // 折叠三角只挂在第一行上，否则同一节点会出现两个三角。
        let multi = node.keys.len() > 1;
        for (index, key) in node.keys.iter().enumerate() {
            let first = index == 0;
            let key_meta = shape.meta.get(key).copied();
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
                // 「展开态」只对**能展开**的行成立：叶子行若也标成展开，
                // 渲染层的 `!expanded` 分支（例如文件夹的弱化字色）就会被误触发。
                expanded: has_children && expanded && first,
                leaf_key: Some(key.clone()),
                key_type: key_meta.map(|meta| meta.key_type),
                ttl_secs: key_meta.map(|meta| meta.ttl_secs),
                // 键行的色与星**直接查本地标注**（不做聚合）：一个键就一个色。
                tag: shape.tags.tag_of(key),
                tag_mixed: false,
                favorite: shape.tags.is_favorite(key),
                has_note: !shape.tags.note_of(key).is_empty(),
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
                // 同上：`expanded` 只在真有子节点时才有意义。
                expanded: has_children && expanded,
                leaf_key: None,
                key_type: None,
                ttl_secs: None,
                // 文件夹行的颜色是**构树时聚合好**的（`stamp_folder_tags`），
                // 这里只搬过来；没有后代标色时是 `None`。
                tag: node.tag,
                tag_mixed: node.tag_mixed,
                favorite: false,
                has_note: false,
            });
        }
        if has_children && expanded {
            flatten_tree(&node.children, depth + 1, &path, shape, out);
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
        // ⚠️ 关键字在**三档里都当字面量**（`*` `?` `[` 会被转义），所以占位文案不再
        // 举例写 `user:*`——那会让用户以为可以写通配符。想按前缀找就用「前缀匹配」档
        // （对齐 zedis：`escape_glob` 在前，通配符后加）。
        let pattern = text_input(
            window,
            cx,
            tr(cx, "输入键名关键字，回车搜索（留空表示全部）"),
        );
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
        // 备注框与上面那一排**不同**：它的内容跟着选中的键走（换键回填）。
        let note_input = text_input(window, cx, tr(cx, "给这个键写点备注，自动保存"));

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
        // 键名过滤框里回车即重扫（否则改了过滤串还得去点「重新扫描」才知道生效）。
        // 不需要窗口——重扫的提示与错误都在后台回调里发。
        subscriptions.push(cx.subscribe(
            &pattern,
            |_this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    cx.entity().update(cx, |this, cx| this.refresh_keys(cx));
                }
            },
        ));
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
                        entity.update(cx, |this, cx| this.submit_cli(window, cx));
                    });
                }
            },
        ));

        // 备注框：内容变化即写回本地标注。
        //
        // ⚠️ **换键时的「回填」也会触发 `Change`**，所以必须先跟已存的备注比一次：
        // 相同就说明这次变化是程序写进去的，不是用户敲的。少了这一步，换键时
        // 回填新键的备注会把上一个键的备注写串门。
        subscriptions.push(cx.subscribe(
            &note_input,
            |this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let value = this.note_input.read(cx).value().to_owned();
                let Some(index) = this.selected else {
                    return;
                };
                let Some(key) = this.keys.get(index).cloned() else {
                    return;
                };
                // `value()` 给的是 `SharedString`，与 `&str` 的相等比较要按值来。
                if value == this.active_tags().note_of(&key) {
                    return;
                }
                this.set_key_note(&key, &value, cx);
            },
        ));

        Self {
            sidebar,
            search,
            pattern,
            edit_buffer,
            decode: DecodeKind::default(),
            value_view: ValueViewMode::default(),
            json_key: None,
            json_expanded: HashSet::new(),
            json_rows: Arc::new(Vec::new()),
            value_pane: cx.new(|_| ResizableState::default()),
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
            query_mode: QueryMode::default(),
            ttl_filter: TtlFilter::default(),
            sort: KeySort::default(),
            flat_rows: Vec::new(),
            selected: None,
            detail: None,
            status: tr(cx, "就绪").into(),
            probe: StatusProbe::default(),
            busy: false,
            limit_announced: false,
            key_meta: HashMap::new(),
            tree_mode: false,
            tree_overrides: HashMap::new(),
            tree_rows: Vec::new(),
            tags: Self::load_tags(cx),
            tag_filter: TagFilter::default(),
            trash: Self::load_trash(cx),
            note_input,
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

#[cfg(test)]
mod tests {
    use super::*;
    // 只在测试里用：放模块内引，免得非测试构建多一条 unused import。
    use duck_redis::scan::AUTO_EXPAND_DEFAULT;

    // `keys` 是私有子模块，`use super::*` 带不进来（孩子模块的私有项要靠路径显式点名）。
    use super::keys::flat_view;

    /// 纯中间节点（只有子节点、自己不挂键）。
    ///
    /// 标签聚合字段走 `..Default::default()`：夹具只关心树的形状，
    /// 颜色由 `stamp_folder_tags` 自己盖（要测颜色就显式调它）。
    fn branch(label: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            label: label.to_owned(),
            keys: Vec::new(),
            children,
            ..TreeNode::default()
        }
    }

    /// 叶子节点（挂实体键、无子节点）。
    fn leaf(label: &str, keys: &[&str]) -> TreeNode {
        TreeNode {
            label: label.to_owned(),
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            children: Vec::new(),
            ..TreeNode::default()
        }
    }

    /// 两层夹具：顶层键 `config` + 命名空间 `user` 下的 `user:1` / `user:2`。
    fn fixture() -> Vec<TreeNode> {
        vec![
            leaf("config", &["config"]),
            branch("user", vec![leaf("1", &["user:1"]), leaf("2", &["user:2"])]),
        ]
    }

    fn meta_of(entries: &[(&str, KeyType, i64)]) -> HashMap<String, KeyMeta> {
        entries
            .iter()
            .map(|(key, key_type, ttl)| {
                (
                    (*key).to_owned(),
                    KeyMeta {
                        key_type: *key_type,
                        ttl_secs: *ttl,
                    },
                )
            })
            .collect()
    }

    /// 展平（无覆盖表、无标注）——渲染层拿到的就是这份行数据。
    fn flatten(
        nodes: &[TreeNode],
        meta: &HashMap<String, KeyMeta>,
        auto_expand: usize,
    ) -> Vec<TreeRow> {
        flatten_with_tags(nodes, meta, auto_expand, &KeyTags::empty())
    }

    /// 展平，并指定本地标注（测「颜色 / 星 / 备注有没有透传进行」）。
    fn flatten_with_tags(
        nodes: &[TreeNode],
        meta: &HashMap<String, KeyMeta>,
        auto_expand: usize,
        tags: &KeyTags,
    ) -> Vec<TreeRow> {
        let overrides = HashMap::new();
        let mut rows = Vec::new();
        flatten_tree(
            nodes,
            0,
            "",
            &TreeShape {
                separator: ":",
                overrides: &overrides,
                auto_expand,
                meta,
                tags,
            },
            &mut rows,
        );
        rows
    }

    /// 行数据契约：**类型徽标读 `key_type`、TTL 芯片读 `ttl_secs`**。
    ///
    /// 元数据未取回的键两个字段都是 `None` ⇒ 渲染成「无徽标 + 空白右列」，
    /// **不能退化成文件夹的键数**（叶子只有一个键，会先闪出一个 `1` 再变芯片）。
    /// 所以本用例同时钉「有元数据要透传」和「没元数据不许冒充文件夹」。
    #[test]
    fn rows_carry_badge_and_ttl_from_metadata() {
        let meta = meta_of(&[
            ("config", KeyType::Hash, 30),
            ("user:1", KeyType::String, -1),
        ]);
        let rows = flatten(&fixture(), &meta, 100);

        let config = rows
            .iter()
            .find(|row| row.leaf_key.as_deref() == Some("config"))
            .expect("config 行");
        assert_eq!(config.key_type, Some(KeyType::Hash), "类型徽标读 key_type");
        assert_eq!(config.ttl_secs, Some(30), "TTL 芯片读 ttl_secs");
        assert!(!config.has_children, "顶层键没有子节点");
        assert!(!config.expanded, "没有子节点的行不该是展开态");

        let first = rows
            .iter()
            .find(|row| row.leaf_key.as_deref() == Some("user:1"))
            .expect("user:1 行");
        assert_eq!(first.key_type, Some(KeyType::String));
        assert_eq!(
            first.ttl_secs,
            Some(-1),
            "-1 = 永久；芯片侧由 ttl_chip_kind 收成 None（不画）"
        );

        // 没进元数据缓存的键：字段留空，而不是 0 / 1 这类会画出东西的值。
        let second = rows
            .iter()
            .find(|row| row.leaf_key.as_deref() == Some("user:2"))
            .expect("user:2 行");
        assert_eq!(second.key_type, None, "元数据未到时不该猜类型");
        assert_eq!(second.ttl_secs, None, "元数据未到时不该猜 TTL");

        // 纯中间节点：`leaf_key` 为空——渲染层就是靠它判定「这是文件夹行」。
        let folder = rows
            .iter()
            .find(|row| row.leaf_key.is_none())
            .expect("user 文件夹行");
        assert_eq!(folder.label, "user");
        assert!(folder.has_children, "中间节点必须能展开");
        assert_eq!(folder.key_count, 2, "文件夹行右列显示子树键数");
        assert_eq!(folder.ttl_secs, None, "TTL 是键的属性，文件夹没有");
        assert_eq!(folder.key_type, None, "文件夹没有类型徽标");
    }

    /// 默认展开：阈值 0 = 只看顶层；阈值 100（zedis 缺省）铺开小命名空间；
    /// 用户显式切过的节点**优先于阈值**。
    #[test]
    fn expansion_follows_threshold_then_overrides() {
        let meta = HashMap::new();

        let folded = flatten(&fixture(), &meta, 0);
        assert_eq!(folded.len(), 2, "阈值 0 只出顶层两行");
        assert!(folded.iter().all(|row| row.depth == 0));

        let opened = flatten(&fixture(), &meta, AUTO_EXPAND_DEFAULT);
        assert!(
            opened.iter().any(|row| row.label == "1"),
            "缺省 100 时 user 的子树应展开"
        );

        // 覆盖表优先：显式收起后，阈值再大也不展开。
        let mut overrides = HashMap::new();
        overrides.insert("user".to_owned(), false);
        let mut pinned = Vec::new();
        flatten_tree(
            &fixture(),
            0,
            "",
            &TreeShape {
                separator: ":",
                overrides: &overrides,
                auto_expand: AUTO_EXPAND_DEFAULT,
                meta: &meta,
                tags: &KeyTags::empty(),
            },
            &mut pinned,
        );
        assert!(
            !pinned.iter().any(|row| row.label == "1"),
            "显式收起后子树不应出现"
        );
    }

    /// 平铺视图（标签档 = 全部、无标注）：多数用例只关心 TTL 档与排序档，
    /// 让他们每次多写两个「不参与」的入参只会把断言的意图埋掉。
    fn flat_of(
        keys: &[String],
        meta: &HashMap<String, KeyMeta>,
        ttl_filter: TtlFilter,
        sort: KeySort,
    ) -> Vec<usize> {
        flat_view(
            keys,
            meta,
            ttl_filter,
            TagFilter::All,
            sort,
            &KeyTags::empty(),
        )
    }

    /// 平铺视图：TTL 档过滤 + 排序档排，返回的是 `keys` 的**下标**。
    ///
    /// 必须是下标而不是键名副本——`selected` 用的就是 `keys` 的坐标系，
    /// 换排序不该让「谁被选中」跟着跑偏。
    #[test]
    fn flat_view_filters_by_ttl_and_sorts() {
        let keys = vec![
            "a:soon".to_owned(),
            "b:never".to_owned(),
            "c:later".to_owned(),
            "d:unknown".to_owned(),
        ];
        let meta = meta_of(&[
            ("a:soon", KeyType::String, 30),
            ("b:never", KeyType::String, -1),
            ("c:later", KeyType::String, 600),
            // d:unknown 故意不进缓存：元数据未到时 TTL 是「未知」。
        ]);

        // 不设限 + 名称升序：就是入参原序。
        assert_eq!(
            flat_of(&keys, &meta, TtlFilter::All, KeySort::NameAsc),
            vec![0, 1, 2, 3]
        );
        // 名称降序：倒序。
        assert_eq!(
            flat_of(&keys, &meta, TtlFilter::All, KeySort::NameDesc),
            vec![3, 2, 1, 0]
        );
        // TTL 升序：先过期的（30s → 600s）在前，永不过期与未知**垫底**且按键名。
        assert_eq!(
            flat_of(&keys, &meta, TtlFilter::All, KeySort::TtlAsc),
            vec![0, 2, 1, 3]
        );

        // 「即将过期」只剩 30s 那个：未知 TTL 的键不命中任何受限档。
        assert_eq!(
            flat_of(&keys, &meta, TtlFilter::Expiring, KeySort::TtlAsc),
            vec![0]
        );
        // 「永不过期」只剩 -1 那个。
        assert_eq!(
            flat_of(&keys, &meta, TtlFilter::NoTtl, KeySort::NameAsc),
            vec![1]
        );
        // 「1 天内」含 30s 与 600s，不含永久 / 未知。
        assert_eq!(
            flat_of(&keys, &meta, TtlFilter::Lt1d, KeySort::NameAsc),
            vec![0, 2]
        );
        // 「7 天以上」一个都排不到（两个存活 TTL 都不到一周，永久 / 未知不算）。
        assert!(
            flat_of(&keys, &meta, TtlFilter::Gte7d, KeySort::NameAsc).is_empty(),
            "Gte7d 只认真实存活期 ≥7d 的键"
        );
    }

    /// 平铺列表的标签档过滤：颜色档只看那一色，收藏档只看收藏，
    /// 「已标」与「未标」按有没有颜色分（收藏**不算**标色）。
    #[test]
    fn flat_view_filters_by_tag_and_favorites() {
        let keys = vec![
            "a:red".to_owned(),
            "b:blue".to_owned(),
            "c:none".to_owned(),
            "d:fav".to_owned(),
        ];
        let mut tags = KeyTags::empty();
        tags.set("a:red", Some(TagColor::Red), "热键");
        tags.set("b:blue", Some(TagColor::Blue), "");
        // c:none 故意不打标注。
        tags.toggle_favorite("d:fav");
        let meta = HashMap::new();

        let view = |filter: TagFilter| {
            flat_view(
                &keys,
                &meta,
                TtlFilter::All,
                filter,
                KeySort::NameAsc,
                &tags,
            )
        };

        assert_eq!(view(TagFilter::All), vec![0, 1, 2, 3]);
        // 「已标」= 有颜色。收藏的 d 不算标色。
        assert_eq!(view(TagFilter::Tagged), vec![0, 1]);
        // 「未标」= 没颜色（收藏与否无关）。
        assert_eq!(view(TagFilter::Untagged), vec![2, 3]);
        assert_eq!(view(TagFilter::Favorites), vec![3]);
        assert_eq!(view(TagFilter::Color(TagColor::Red)), vec![0]);
        assert_eq!(
            view(TagFilter::Color(TagColor::Purple)),
            Vec::<usize>::new()
        );
    }

    /// 树行的三个标注字段：键行查自己的标注，文件夹行用**聚合色**。
    #[test]
    fn tree_rows_carry_tag_favorite_and_note() {
        let meta = meta_of(&[
            ("config", KeyType::Hash, -1),
            ("user:1", KeyType::String, -1),
        ]);
        let mut tags = KeyTags::empty();
        tags.set("config", Some(TagColor::Green), "常量表");
        tags.toggle_favorite("config");
        tags.set("user:1", Some(TagColor::Red), "");
        tags.set("user:2", Some(TagColor::Blue), "");

        let mut nodes = fixture();
        stamp_folder_tags(&mut nodes, |key| tags.tag_of(key));
        let rows = flatten_with_tags(&nodes, &meta, AUTO_EXPAND_DEFAULT, &tags);

        let config = rows.iter().find(|row| row.label == "config").expect("键行");
        assert_eq!(config.tag, Some(TagColor::Green));
        assert!(!config.tag_mixed, "键行只有一个颜色，谈不上混");
        assert!(config.favorite);
        assert!(config.has_note);

        // 文件夹行：红蓝各一 ⇒ 众数取下标靠前的红，并标成混色；
        // 收藏与备注是**键**的属性，文件夹行恒为 false。
        let user = rows
            .iter()
            .find(|row| row.label == "user" && row.leaf_key.is_none())
            .expect("文件夹行");
        assert_eq!(user.tag, Some(TagColor::Red));
        assert!(user.tag_mixed);
        assert!(!user.favorite);
        assert!(!user.has_note);

        // 子键行的颜色来自自己的标注，不是父文件夹的聚合色。
        let first = rows
            .iter()
            .find(|row| row.leaf_key.as_deref() == Some("user:1"))
            .expect("user:1 行");
        assert_eq!(first.tag, Some(TagColor::Red));
        assert!(!first.tag_mixed);
        assert!(!first.favorite);
    }

    /// 值面板顶栏的「原始大小」写法：字节档不留小数，往上按 1024 进制换档。
    ///
    /// 这条是**纯函数**，正好卡住两个容易写歪的地方：`0` 不能显示成「0.0 KB」、
    /// `1023` 不能提前进位成「1.0 KB」。
    #[test]
    fn byte_sizes_switch_unit_at_1024() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 + 512 * 1024), "3.5 MB");
        // 到顶不越界：GB 之后不再换档（再大也只是数字变大）。
        assert_eq!(format_bytes(2048 * 1024 * 1024), "2.0 GB");
    }

    /// 呈现档位的元素 id 后缀是稳定的 ASCII（改它 = 改测试与 UI 自动化）。
    #[test]
    fn value_view_wire_names_are_stable() {
        assert_eq!(ValueViewMode::Text.wire_name(), "text");
        assert_eq!(ValueViewMode::Tree.wire_name(), "tree");
        assert_eq!(ValueViewMode::default(), ValueViewMode::Text);
    }
}
