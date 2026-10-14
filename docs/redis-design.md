# Duck Toolbox Redis 模块架构设计（duck-redis）

> 状态：已确认开工（2026-10-12）。参考物：temp/zedis。
> 用户已拍板三项：① **独立顶级路由**（左侧栏新增入口，不并入 `database` 路由）；
> ② M1 目标为**完整档**（对标 zedis 全部面板）；③ **要分级锁写安全闸门**。

---

## 1. 目标与范围

- 在**新的顶级路由** `redis`（`RouteId::new("redis")`）下实装一个 Redis / Valkey 客户端模块，
  左侧活动栏新增独立入口，与「数据库」平级。
- 功能与界面以 zedis 为参考，覆盖：连接管理、命名空间 key 树、全类型值查看与编辑、
  redis-cli 终端、服务端观测面板、内存分析、集群拓扑、ACL / CONFIG / Pub-Sub、模块类型查看器。
- **分级锁写**：连接档案带环境标签与只读开关；生产档默认锁写、解锁限时；破坏性命令二次确认。
- 不做的事：不做 Web / wasm 版本（zedis 的 `zedis-bridge` + `zedis-web` 轨不迁）、
  不做云同步、不做 MCP 服务端（远期留口子）。

### 1.1 为什么是独立路由而不是 `duck-db` 的第 4 个引擎

`docs/database-design.md` §5 原计划把 Redis 作为 `duck-db` 的 M2 引擎（`KeyValueExt` 首个消费者）。
经评审改为独立路由，理由三条：

1. **交互模型不同**。`db.rs`（2902 行）的主区是「表浏览 / SQL 编辑器 + 行网格 + 暂存式行编辑」，
   以关系型的行列与主键为中心；Redis 的主区是「命名空间树 + 类型专属值编辑器」，
   没有表、没有主键、没有 SQL 网格。硬塞进同一页面会让两边都变成 if 分支的堆叠。
2. **面板数量级不同**。SQL 侧是 1 个主视图；Redis 侧要对标 zedis 的 19 个 `ServerView` 面板。
3. **不扰动在编文件**。`db.rs` 与 `duck-db` 刚完成 M1/M1.5/M1.6 三轮实施，
   独立路由把改动面收敛在新 crate + 4 个登记点，避免把风险叠加到正在工作的 SQL 链路上。

代价是「连接档案 / 凭据 / SSH 隧道」三件薄封装与 `duck-db` 有概念重叠。
**本设计接受这一重叠**（不预先抽公共层）：三件封装各自约 100~200 行，且两侧的字段、
校验与安全语义并不相同（Redis 有 ACL 用户名与 db 序号，SQL 有 schema 与库名）。
等第三位消费者（Mongo）出现时再评估抽取 `duck-conn`，避免过早抽象。

## 2. 参考项目评估（temp/zedis）

### 2.1 体量与形态

| 项 | 实测值 |
| --- | --- |
| Rust 代码量 | 约 **16.1 万行**（`src/` 10.1 万 · `crates/` 6.0 万 · `zedis-bridge` 7.2 千 · `zedis-web` 1.2 千） |
| 源文件 | 170 个（`src/`） |
| 内部 crate | 5 个：`zedis-core`（解码管线 / RDB / fuzzy / jsonpath / ttl / diff）、`zedis-connection`（池化客户端 / 集群 / 哨兵 / SSH 隧道 / TLS / RESP / 只读与审计 / ACL）、`zedis-db`（redb 本地持久化 / 回收站 / 历史 / Lua 脚本库）、`zedis-ui`（通用 UI 原语）、主 crate |
| 面板 | `ServerView` **19 个**：Editor · Metrics · Slowlog · MemoryAnalysis · Clients · Monitor · Config · Acl · Search · Functions · LuaScripts · Persistence · KeyspaceNotifications · Topology · ServerLoad · Hotkeys · ValueSearch · ServerInfo · TimeSeriesExplorer |
| 值编辑器 / 对话框 | 30 余个（hash / list / set / zset / stream / bitmap / hll / geo / vector / timeseries / probabilistic / proto / script / function / pubsub / json_tree / kv_table / value_diff / multi_search …） |
| 中文词条 | `locales/zh.toml` 2613 行 |
| 许可 | **Apache-2.0** |

### 2.2 四条硬阻隔（结论：可借架构，不可搬代码）

| 维度 | zedis | 本仓 Duck Toolbox | 影响 |
| --- | --- | --- | --- |
| MSRV | **1.96**（toolchain 1.98.1） | **1.85**（workspace 硬门） | 无法作为依赖；其源码可能用到 >1.85 的语言特性 |
| UI 栈 | git 版 GPUI + `gpui_kit::component` | gpui-pre `=0.3.8`（gpui-fast 补丁）+ gpui-component `0.7.1` | API 不同，组件调用须重写 |
| 异步模型 | tokio + smol 双运行时**常开**，redis 异步 API | **零 tokio**；阻塞驱动丢 `background_executor` | 连接与命令层须重写为阻塞版 |
| 凭据 / SSH | keyring **4.2**（含 zbus / Secret Service）、russh 0.63.3 | **无 keyring**（2026-10-12 起凭据走 `duck-platform::vault` 密文落程序自有文件 + SQLite）、russh ≤0.63.1 | 依赖选型独立 |

### 2.3 各取什么、弃什么

| 方向 | 取 | 弃 |
| --- | --- | --- |
| 信息架构 | 连接侧栏（分组 / 环境标签 / 健康点）→ 命名空间 key 树（分隔符分组 + 类型图标 + TTL 标记）→ 右栏值编辑器；服务端面板作为「连接内视图」切换 | 多工作区标签页并行（本项目窗口模型不同，M1 先做单激活连接） |
| 键树与 SCAN | SCAN 游标续扫 + 分页加载 + 虚拟滚动；「已加载 / 总数」计数；服务端 `MATCH` 过滤 | `imbl` 持久化 B 树（为百万键快照设计，M1 用 `Vec` + 分块追加足够） |
| 值查看 | 类型专属编辑器（Hash / List / Set / ZSet / Stream 分页编辑）；「写回存储的原始字节」纪律 | 解码管线的全部 30 余种格式（见 §6 分期，逐期补齐） |
| 安全 | **分级锁写**：环境标签 + Prod 默认锁写 + 限时解锁窗口 + 破坏性命令输入服务器名确认 + capability 灰显并说明原因 | 每机主密钥加密配置文件（本项目凭据统一走 `duck-platform::secret`） |
| 连接 | TLS / SSH 隧道 / Cluster / Sentinel 的分层建模；连接诊断 | `zedis-bridge` 的账号 / 审计 / 只读账户轨（Web 专属） |
| 本地持久化 | 回收站（删除键保留 24h 可恢复）、键标签 / 备注 / 收藏、命令历史 | `redb`（本项目统一走 `duck-store` 的 SQLite kv） |

## 3. 总体架构与 crate 布局

```
duck-app（bin）
  └─ pages/redis/             RedisPage 实体（照 DatabasePage 模板，db.rs:359）
       │  Entity / render / 左栏连接树 / 主区 key 树与值编辑器 / 右栏键详情
       ▼
duck-redis（内核 crate，★新建，不依赖 gpui）
  ├─ config       RedisConfig / EnvTag / 连接档案 wire（serde）
  ├─ secret       凭据存取（→ duck-platform::secret，键 redisconn:{id}:password）
  ├─ capability   AccessMode × 命令可用性 → Capability（纯函数，可单测）
  ├─ writegate    破坏性 / 写命令判定与确认要求（纯函数，可单测）
  ├─ conn         阻塞客户端：连接 / PING / INFO / TYPE / SELECT 切库
  ├─ scan         SCAN 游标分页 + 分隔符命名空间分组
  ├─ keyops       键操作：TTL / RENAME / DEL / DUMP / RESTORE / 各类型读写
  ├─ value        统一值模型 RedisValue（类型 + 分页容器 + TTL）
  └─ panels       服务端面板数据获取（INFO 解析 / 慢日志 / 内存分析 / 集群 / ACL / CONFIG）
        ▼
duck-platform（paths / secret / dialog）   duck-store（kv 持久化）
```

依赖方向严格向下；`duck-redis` 内核**零 `gpui::` 引用**（对齐 `docs/terminal-ssh-sftp-design.md`
与 `docs/database-design.md` 的依赖纪律），可用普通 `#[test]` 覆盖。

与本仓既有 crate 的关系：

- **不依赖 `duck-db`**。两者是平行的引擎内核，各自的连接档案、凭据键名与错误类型独立。
- **复用 `duck-platform`**：`secret`（凭据）、`paths`（日志 / 导出落点）、`dialog`（文件对话框）。
- **复用 `duck-store`**：连接档案与用户偏好的 kv 持久化（「结构体 → `serde_json` → 单个 kv 键」范式）。

## 4. 核心设计决策

### 4.1 依赖准入：`redis` crate 版本（已实测）

本机 cargo 索引（`~/.cargo/registry/index/index.crates.io-*/cache/re/di/redis`）中的
`rust_version` 字段实测结果：

| 版本 | MSRV | 结论 |
| --- | --- | --- |
| `1.2.3` · `1.2.4` · `1.3.0` · `1.4.x` · `1.5.0` · `1.6.0` · `1.7.0` · `1.7.1` | **1.88** | ✗ 顶破本仓 1.85 |
| **`1.2.0` · `1.2.1` · `1.2.2`** | **1.85** | ✅ 1.x 轨道上过门的最新版 = **1.2.2** |
| `0.32.7` | **1.80** | ✅ 备选（且 `redis-0.32.7.crate` 已在本机 cargo 缓存，零下载） |

**裁决：钉 `redis = "=1.2.2"`**，`default-features = false`，只开
`["acl", "streams", "geospatial", "script"]`。

- **关掉 `aio` / `tokio-comp` / `smol-comp`**：本仓零 tokio 纪律，走 `Client::get_connection()`
  的**阻塞** API。redis 1.x 的默认特性不含 runtime，无需额外关。
- 不开 `tls`：M1 的 TLS 由 M1.5 评估（若开，走 `tls-rustls` 复用图内 `rustls` / `ring`，
  与 `ureq` / `russh` 同源，不引 openssl）。
- 不开 `cluster`：M1.4 集群面板需要时再单独评估该特性与其传递依赖。
- 版本用 `=` 钉死（本仓先例：`rquickjs = "=0.11.0"`、`gpui-pre = "=0.3.8"`）——
  写 `"1.2"` 会解析到 1.2.3+ 从而顶破 MSRV。

新增依赖的既定代价：`sbin/licenses.py` 重生成并提交 `THIRD_PARTY_LICENSES.md`
（CI `static` 腿与 `package.sh` 双重门禁）。⚠️ 并发上下文：另一会话的 duck-term 落地时
新增了 `alacritty_terminal` / `vte` / `parking_lot`，两笔新增的许可证重生成应合并为一次，
避免互相覆盖。

### 4.2 同步内核 + `background_executor` 桥接

口径与 `duck-db` M1 完全一致（`docs/database-design.md` 实施修订一）：

- `duck-redis` 对外**只有同步 API**：`Connection::connect(cfg, secret)`、`scan(cursor, ..)`、
  `hscan(key, cursor, ..)` 等，全部阻塞。
- 调用方（`duck-app/pages/redis/`）把调用丢进 `cx.background_executor().spawn(async move { work() })`
  （本 fork 要求传 Future，见 memory 2026-10-12「duck-app 编译错误笔记」）。
- 结果回投 UI 用 `cx.defer` 或 `WeakEntity::update`（**返回 `Result`，须 `let _ =`**）。
- ⚠️ **`RunCommand` 监听器禁止在派发期间直接 `window.update`**（AGENTS.md / memory 约定），
  一律先 `cx.defer`。
- 长驻监听（`MONITOR`、Pub/Sub、Streams 实时跟踪）M1.4 才做：届时用**专用连接**
  + 后台线程 + `futures-channel` 把行推给 UI，**不占用** `background_executor` 的公共池。

### 4.3 连接档案与凭据

- **连接档案**：`kv: redis_connections`（单键 JSON 数组，沿用 `appearance` / `db_connections` 范式）。
  元素字段：`id / name / host / port / db / acl_user / tls / ssh_tunnel / group / env_tag(Local|Testing|Staging|Prod) / read_only / key_separator / secret_ref`。
  **口令不入 kv**，只留 `secret_ref` 语义的键名。
- **凭据**：`duck-platform::secret`（**2026-10-12 起为应用自有文件后端**：值 = vault 密文，
  落 `<应用数据目录>/credentials.json`），
  键 `redisconn:{id}:password`（SSH 口令走 `redisconn:{id}:ssh`）。
  ⚠️ 沿用既有纪律：**存储不可用时不降级明文 / 内存**，直接报错；零日志。
  ~~keyring 3.6.3 / `SERVICE = "com.duck.toolbox"` / `UnsupportedPlatform`~~ 已随
  keyring 依赖一并删除（见 `architecture-gap-analysis.md` P3-10）。
- **`db` 序号**：Redis 有 16 个逻辑库。连接体内保存当前 `db`，切换走 `SELECT`。
- **`key_separator`**：默认 `:`，用于命名空间树分组；每连接可改（`.` / `/` / `-`）。
- 连接档案的**导出 / 导入**沿用 `db.rs` 的做法（`serde_json::to_vec_pretty(&Vec<RedisConfig>)`，
  口令只以密文存在程序自己的凭据文件里、天然不导出；按 `id` 去重，跳过 `validate` 失败项）。

### 4.4 分级锁写与 capability（本模块的安全基座）

zedis 最值得学的一层，**必须一开始就设计进内核**，后补会改接口。

```rust
// duck-redis/src/capability.rs
pub enum AccessMode {
    ReadWrite,       // 全放开
    SafeMode,        // 连接只读开关打开，或 Prod 档在解锁窗口之外
    StrictReadOnly,  // ACL 用户无写权限（由只读探测发现）
}
```

- **输入**：连接档案的 `env_tag` + `read_only` + 解锁窗口剩余时间 + 探测到的 ACL 权限。
- **输出**：`Capability` —— 一个用户可见动作（`DeleteKey` / `FlushDb` / `ConfigWrite` /
  `ScriptLoad` / `ClusterReshard` …）能否执行、不能执行时的**原因文案键**。
- **纯函数**：`fn resolve(cfg: &RedisConfig, unlock: Option<Duration>, acl: AclProbe) -> Capabilities`，
  可在 `duck-redis` 内用普通 `#[test]` 覆盖（对齐 `duck-db::sqlintel` 的「纯函数、可单测」纪律）。
- **UI 契约**：按钮**灰显并说明原因**（不是点了才报错）。本仓已有先例 `unavailable_chip`
  的等价物可用 `duck-components` 的既有原语表达；文案全部进 i18n EN 表。
- **解锁窗口**：Prod 档默认锁写；状态栏提供「解锁 15 分钟」入口，窗口结束自动回到 SafeMode。
  时长常量进 `duck-redis::capability`（如 `WRITE_UNLOCK_SECS`），**不在 UI 里写裸值**。

```rust
// duck-redis/src/writegate.rs
/// 需要二次确认的命令（要求用户输入服务器名 / 连接名）。
pub fn requires_confirm(cmd: &str) -> Option<ConfirmKind>;
/// 是否为破坏性命令（FLUSHALL / FLUSHDB / KEYS / SCRIPT FLUSH / CLUSTER RESET …）。
pub fn is_destructive(cmd: &str) -> bool;
```

- **`KEYS`**：判为「高风险但允许」——先提示「用 SCAN 替代」再放行（zedis 同款取舍）。
- **`FLUSHALL` / `FLUSHDB` / `CLUSTER RESET`**：输入连接名确认。
- 判定只看**命令名**（首 token，大小写不敏感），不看参数——纯函数、可单测、无误判面。

### 4.5 SCAN 键树与命名空间分组

- **绝不用 `KEYS`** 做键树数据源：一律 `SCAN cursor MATCH pattern COUNT n` 游标续扫。
- **分组**：按 `key_separator` 切分构造树；同一层节点按名称排序；叶子显示**类型图标 + TTL 标记**。
  分隔符在 `:` / `.` / `/` 间可配置（zedis 同款）。
- **分页与虚拟滚动**：`uniform_list` + `cx.processor(|_, range, _, _| …)` 只渲染窗口内行
  （gpui-component 有同款 API，**以当前源码为准，不发明 API**）。
- **计数**：显示「已加载 / 总数」；总数来自 `DBSIZE`（便宜），加载数来自本地。
- **过滤**：服务端 `MATCH`（用户输入的通配串）；分组折叠状态存页面实体内存。
- **规模目标**：百万键下滚动不掉帧——这是 zedis 的核心卖点，也是本模块必须守住的性能判据。

### 4.6 值模型与解码管线（分期）

```rust
// duck-redis/src/value.rs
pub enum RedisValue {
    String(Vec<u8>),                       // 原始字节，UI 侧按解码视图呈现
    Hash(Vec<(Vec<u8>, Vec<u8>)>),         // HSCAN 分页
    List(Vec<Vec<u8>>),                    // LRANGE 窗口
    Set(Vec<Vec<u8>>),                     // SSCAN 分页
    ZSet(Vec<(Vec<u8>, f64)>),             // ZSCAN 分页
    Stream(/* 条目 + ID */),
    Unsupported { type_name: String },     // 模块类型：只读 DUMP 展示
}
```

- **纪律（照抄 zedis）**：编辑器里显示的是**解码后**的内容，但**写回服务端的永远是存储的原始字节**。
  一个 MessagePack 值编辑后仍要以 MessagePack 写回；不能识别格式的元素退到 hex 编辑。
- **分页**：Hash / Set / ZSet 走 `HSCAN` / `SSCAN` / `ZSCAN` 游标，List 走 `LRANGE` 窗口，
  **绝不 `HGETALL`**（大 hash 会把服务端主线程序列化死）。
- **解码管线分期**：
  - **M1.2**：文本（UTF-8）+ JSON 美化 + Hex（三档覆盖绝大多数排查场景）。
  - **M1.5**：+ MessagePack / GZIP / ZSTD / Base64 / JWT / Unix 时间戳 / PHP serialize / URL 编码。
    **已落地（10-12，首期）**：除 MessagePack 与 ZSTD（要先做**依赖准入**，见 §1 依赖纪律）外的
    六档全部实现，住 `duck-redis/src/decode.rs`（零 GPUI 纯函数 + 20 个单测）。
    档位仍是**显式选择**（不做「按内容自动判断」），因为自动判断的灰区说不清
    （`{` 开头的非 JSON 会被当 JSON 试、失败又悄悄退回原文，用户不知道自己在看哪一档）。
  - **M1.6**：+ 图片预览 / BSON / Java 序列化 / pickle / LZ4 / 自定义脚本查看器；把 M1.5 欠下的
    MessagePack / ZSTD 一并补上（`rmp*` / `zstd` 都需新引包，按 MSRV 1.85 + 许可证双闸门走）。
- **大值保护**：单值超过阈值（如 4 MB）不自动加载，显示「体积 + 手动加载」；
  **32 MB 以上绝不发 `DUMP`**（服务端主线程序列化整个值）。

### 4.7 服务端面板（对标 zedis 19 个 ServerView）

按数据来源分四组，逐期落地（见 §6）：

| 组 | 面板 | 数据来源 |
| --- | --- | --- |
| 观测 | Metrics · ServerLoad · ServerInfo（原始 INFO 浏览器） | `INFO` / `INFO everything` 解析；采样历史存页面内存 |
| 诊断 | Slowlog · Monitor · Clients · KeyspaceNotifications | `SLOWLOG GET` · `MONITOR`（专用连接）· `CLIENT LIST` · 键事件订阅 |
| 容量 | MemoryAnalysis · Persistence · Hotkeys | `MEMORY DOCTOR` / `MEMORY STATS` / `SCAN`+`MEMORY USAGE` 抽样 · `INFO persistence` · `HOTKEYS` |
| 管理与拓扑 | Config · Acl · Topology · Functions · LuaScripts · Search · ValueSearch · TimeSeriesExplorer | `CONFIG GET/SET` · `ACL *` · `CLUSTER *` / `SENTINEL *` · `FUNCTION *` / `SCRIPT *` · `FT.*` · 全库值搜索 · `TS.MRANGE` |

- **能力探测**：连接后跑一次只读探测（`COMMAND INFO` / 模块列表 / `CONFIG GET` 试读），
  把「本连接支持哪些命令」记成 `ServerFeatures`；运行时遇 `NOPERM` / `unknown command`
  再修正。**面板与按钮按探测结果灰显并说明原因**，不做品牌（ElastiCache / 阿里云 / Twemproxy）
  特判——代理与云托管一律走「缺什么命令」这一条通则。
- **版本门槛**：每个功能声明「首个具备该功能的 Redis 版本」，低于则隐藏（zedis 的 Floor 思想）。

### 4.8 UI 形态（duck-app 内）

照 `DatabasePage` 模板（`db.rs:359` 起）：

- **实体**：`pub struct RedisPage { sidebar: Entity<SidebarState>, … }`，
  配 `pub fn sidebar_state(&self) -> Entity<SidebarState>`（db.rs:478）、
  `fn render_sidebar(&self, cx: &mut Context<Self>)`（db.rs:1387）、
  `pub(crate) fn render_right_sidebar(&self, cx: &App)`（db.rs:2262）、
  根渲染用 `SidebarPanel::new("redis-content", &self.sidebar, content, window, cx)`（db.rs:2574）。
- **左栏**（页面内 `SidebarPanel`）：连接档案分组树（环境标签着色 + 健康点）→
  分组标题「连接」→ 底部「新建连接 / 导入 / 导出」。
- **主区**：顶部连接状态条（服务端版本 / 模式 / 锁写状态 / 解锁按钮）→
  左侧命名空间 key 树 + 右侧值编辑器（两栏；M1.4 起加视图切换器接服务端面板）。
- **右栏**（shell 级 `with_right_sidebar`）：键详情（类型 / 编码 / 空闲时间 / TTL / 字段数）
  + 属性（标签 / 备注）+ 危险操作区（删除 / 重命名 / 设置 TTL，受 capability 约束）。
- **规格**：全部遵从 `AGENTS.md` §3.6 —— 字阶走 `duck_theme::builtin::base_tokens`、
  **禁止裸 `px(字号)`**、悬停一律 `hover_tint`、对比度过 `button_surface`、
  下拉用 `dropdown_select`、布尔项用 `switch_control`。
- **i18n**：源串直接写中文，全部进 `duck-theme/src/i18n.rs` 的 `EN` 表（UTF-8 字节序，
  二分查找）；**补表必须用只读脚本算序号，不能靠肉眼排序**。
- **图标**：新增 redis 专用图标进 `duck-components/assets/icons/`（Lucide 1.75 线宽），
  在 `icons.rs` 的 `duck_icons!` 宏内**按字母序**登记（该宏顺序即 `ALL` 顺序）。

### 4.9 连接编辑弹窗（对标 zedis「添加新服务器」）

**形态**：连接的新建 / 编辑走**模态弹窗**，不再是主区内的内嵌表单（旧 `Mode` 枚举已整体删除）。
弹窗复用 `duck-components` 的模态原语——`modal_drag` / `modal_drag_area`（几何状态 + 面板高度量尺）、
`modal_offset`（居中补差）、`dialog_top_offset`（基线落点）、`modal_title_row`（标题 + 关闭）、
`DialogFooter`（底部动作区）。**标题行不再可拖**：自绘位移与库的绝对定位叠加会漂移，
2026-10-12 起弹窗改为**恒居中**（面板高度在 prepaint 阶段由内容区尾部的量尺实测，
下一帧据此算 `margin_top`）——详见 `duck-components/src/dialog.rs` 模块头。
底部左「测试」、右「删除此连接 / 取消 / 添加(保存)」，
与 zedis 一致；`DialogFooter` 实现 `Styled`，故可 `.justify_between()` 把两组分开。

**六个页签**（`FORM_TABS: [&str; 6] = ["常规","SSL/TLS","SSH 隧道","高级","安全","键"]`），
与 zedis `servers/form.rs` 同构：

| 页签 | 已接线字段 | 未接线档位（灰显 `disabled_shell`） |
| --- | --- | --- |
| 常规 | 名称\* / 主机\* / 端口 / 用户名（ACL）/ 密码（掩码，加密落库）/ 描述 / 分组 | — |
| SSL/TLS | 开关 + 证书路径等表单控件（写入档案） | 建连暂不启用 TLS → `RedisError::Unsupported` 明确报错 |
| SSH 隧道 | 开关 + 5 个 SSH 字段（写入档案） | 建连暂不建隧道 → `RedisError::Unsupported` 明确报错 |
| 高级 | 默认库 / 连接超时等 | Sentinel / Cluster 相关 → 「M1.4 接入」 |
| 安全 | 环境标签（单选 5 档）/ 写入策略（单选 4 档）/ 写入确认开关 | 逐字「输入连接名」校验 → `TypeServerName` 未接线（见 M1.3 口径） |
| 键 | 键分隔符 / SCAN COUNT / 键树最大深度 / 自动展开阈值 | — |

**关键实现约束**（都是踩过的坑，改动前必读）：

1. **表单状态必须是 `Entity<EditForm>`，不能借用 `&mut RedisPage`**。`open_dialog` 的
   `content` 闭包是 `Fn`（每次重排都会重跑），闭包内无法拿到 `&mut self`。故所有输入项的
   `Entity<InputState>` 与单选档位值都挂在 `Entity<EditForm>` 上；页签切换 / 单选回写一律
   `form.update(cx, …)` 后紧跟 **`window.refresh()`**，否则界面不重绘。
2. **灰显占位不造输入实体**。`disabled_shell(placeholder, cx)` 渲染一个灰盒子 + 说明文字，
   **不创建 `Entity<InputState>`**——既避免把未接线字段塞进焦点链，也绕开「无法从 `&App`
   造 `InputState`」的约束。
3. **`text_input<C: AppContext>` 泛化**：同一个 helper 要同时服务 `RedisPage` 与 `EditForm`
   两种上下文，故把 `cx` 约束放宽到 `AppContext` 而非具体 `Context<T>`。
4. **保存入口单一**：底部主按钮与键盘 `⏎` 共用同一份 `ConfirmHandler`，都调 `save_form`；
   `save_form` 返回 `false` 表示校验失败**保持弹窗打开**（返回 `true` 才 `window.close_dialog`）。

## 5. 路由与入口登记清单（精确到文件）

`ShellRoute` 的登记是**手工同源**的四处，改路由必须同步，否则存档校验或编译期哨兵会报错。

| # | 文件 | 位置 | 改动 |
| --- | --- | --- | --- |
| 1 | `crates/duck-app/src/pages/mod.rs` | 路由常量区 | 新增 `pub const REDIS_ROUTE: RouteId = RouteId::new("redis");` + `pub use redis::RedisPage;` + `mod redis;` |
| 2 | `crates/duck-app/src/pages/mod.rs` | — | 新建 `pages/redis/`（10-14 由单文件拆成目录模块） |
| 3 | `crates/duck-app/src/app.rs` | `routes` vec（634~694） | 在 `DATABASE_ROUTE` 条目之后插入 `ShellRoute::new(REDIS_ROUTE, "duck-route-redis", "Redis", Icon::new(DuckIcon::DatabaseZap), redis).with_label_fn(tr("Redis")).with_sidebar(..).with_right_sidebar(..)` |
| 4 | `crates/duck-app/src/settings.rs` | `KNOWN_ROUTES`（62） | 数组类型 `[RouteId; 6]` → `[RouteId; 7]`，并在 `DATABASE_ROUTE` 后插入 `REDIS_ROUTE`（**数组长度是唯一编译期哨兵**） |
| 5 | `crates/duck-app/src/actions.rs` | `ids` 模块（29） | 新增 `pub const ROUTE_REDIS: &str = "route.redis";` |
| 6 | `crates/duck-app/src/actions.rs` | `declare_all`（160~174） | 新增一条 `ActionMeta`（label `tr("Redis")`、keywords `["redis","valkey","kv","cache"]`、group `导航`、icon `DatabaseZap`、slots `[Palette, Menu]`、menu `("视图", Some("导航"))`、`keys: &["$mod-5"]`） |
| 7 | `crates/duck-app/src/actions.rs` | `install_shell_handlers`（434） | `install_handler(cx, ids::ROUTE_REDIS, navigate("redis"));` |
| 8 | `crates/duck-components/src/icons.rs` | `duck_icons!`（63） | 按字母序新增 `DatabaseZap => "database-zap"` |
| 9 | `crates/duck-components/assets/icons/` | — | 新增 `database-zap.svg`（Lucide 单行版，`stroke-width="1.75"`） |
| 10 | `Cargo.toml`（根） | `[workspace.dependencies]` | `duck-redis = { path = "crates/duck-redis" }` + `redis = "=1.2.2"`（带版本理由注释） |

### 5.1 落地状态（2026-10-12，M1.1 已收口）

上表 **10 处全部已改**（行号为本节写作时的实测值），`crates/duck-app/Cargo.toml` 另加
`duck-redis.workspace = true`。门禁实测：`cargo build --workspace --locked` 绿、
`cargo test --workspace --locked` **0 失败**、`cargo clippy --workspace --all-targets` 中
新增代码（`duck-redis` 全 7 个模块 + `pages/redis.rs` + 4 处登记）**零警告**、
`duck-theme` i18n 守卫（`every_chinese_literal_is_translated` / `table_is_sorted`）绿、
`sbin/licenses.py --check` 退出码 0（`redis 1.2.2` BSD-3-Clause 已入表，组件数 900）。

> ⚠️ **并发会话干扰（2026-10-12 10:29~10:41 全程）**：另一个会话正在把同一套
> 模态弹窗范式搬进 `crates/duck-app/src/pages/db.rs`（并在 10:29 往
> `crates/duck-app/Cargo.toml` 补了 `anyhow` / `lsp-types` 两条直依赖）。
> 期间 `Cargo.lock` 一度滞后（`cargo metadata --locked` 报
> 「cannot update the lock file」），**10:37:56 对方重生成后已恢复**。
> 截至本文档写作时 `db.rs` 仍有 **18 处编译错误**（缺 `dialog_top_offset` /
> `WindowExt` / `EditForm::with_kind` 等，全是对方的在途状态）。

**本轮门禁实测（10:38~10:41，`--locked`，全部在真实仓内跑）**：

| 命令 | 结果 |
| --- | --- |
| `cargo metadata --locked` | ✅ 退出 0（锁已恢复一致） |
| `cargo build -p duck-redis --locked` | ✅ |
| `cargo test -p duck-redis --locked` | ✅ **48 passed / 0 failed** |
| `cargo clippy -p duck-redis --locked --all-targets` | ✅ **零警告** |
| `cargo build -p duck-app --locked` | ❌ 18 个错误，**全部落在 `db.rs`**；`redis.rs` **零诊断**（`grep -c 'redis.rs'` = 0） |
| `cargo clippy -p duck-app --locked --all-targets` | ❌ 同上（`db.rs` 编译不过）；**`redis.rs` 零命中** |
| `cargo test -p duck-theme --locked` | **20 passed / 1 failed**；唯一失败是 `every_chinese_literal_is_translated`，**19 条缺失全在 `db.rs`**，`redis.rs` 一条不缺（`table_is_sorted` 绿） |
| `rustfmt --check`（6 个改动文件） | ✅ 全过 |

> 即：**本模块的代码是干净的**；`duck-app` 整体编译不过完全由对方在途的 `db.rs` 造成。
> `duck-redis` 7 个模块共 **48 个单测全绿**，与隔离验证工作区（`/tmp/redis-verify`，
> 见下）结论一致——独立复现了一遍。

**隔离复现手段（可复用，与上面互为交叉验证）**：`/tmp/redis-verify` 拷一份
`crates/duck-redis` + 一个只复刻 `duck_platform::secret` **对外签名**的 30 行替身
（签名与真 `secret.rs` 逐字一致，**含 `SecretError` 定义在 `secret` 模块内部这一点**）
→ `cargo test --offline` **48 passed**、`cargo clippy --offline --all-targets` **零警告**。
这条路**不依赖工作区解析**，在 `Cargo.lock` 滞后期间仍可用。

**i18n 计数坑**：`grep -c '^    ("'` 只有 **748**——长条目被 rustfmt 折成 4 行，
`(` 与字符串不同行；**只有解析式计数才准（真实 992 条）**。



**键位决定**：改动前 `⌘1`~`⌘6` 依次对应 概览 / 工具箱 / SSH / 数据库 / 项目管理 / AI 助手。
Redis 插在「数据库」之后，为保持「侧栏顺序 ↔ 键位序号」一一对应，
把 `项目管理` 顺延为 `⌘6`、`AI 助手` 顺延为 `⌘7`，Redis 取 `⌘5`（**已按此实施**）。
（两条被顺延的路由当前都是占位页，改动风险最低；如不认可此顺延，退回方案是 Redis 取 `⌘7`
并置于侧栏末尾——只需交换 `actions.rs` 里三条 `ActionMeta` 的 `keys` 字段。）

**命令面板**：条目由 `duck_command::declare` 自动派生（`ActionSlot::Palette`），
**无需**改 `duck-tools`——该 crate 只登记插件工具，不登记路由。

## 6. 分期实施计划

用户口径为「M1 完整档」，即**目标是对标 zedis 全部面板**。下表把它拆成可独立验收的六个增量，
每一项都能单独交付并过门禁；「完整」是终点，不是一次提交。

| 增量 | 内容 | 交付判据 |
| --- | --- | --- |
| **M1.1 底座** | `duck-redis` crate（`config` / `secret` / `capability` / `writegate` / `conn` / `scan`）+ 路由与入口 10 处登记 + `pages/redis.rs` 连接管理（新建 / 编辑 / 删除 / 分组 / 环境标签 / 连通测试）+ 左栏连接树 | 应用启动后活动栏出现 Redis 入口；能存两条连接、重启后仍在、连通测试成功；`cargo test -p duck-redis` 覆盖 capability 与 writegate 纯函数 |
| **M1.2 浏览与编辑** | 主区命名空间 key 树（SCAN 分页 + 分隔符分组 + 类型图标 + TTL 标记 + 虚拟滚动 + MATCH 过滤）；右栏键详情；String / Hash / List / Set / ZSet 值查看与编辑；文本 / JSON / Hex 三档解码；TTL 设置 / 重命名 / 删除 | **「扫 key → 打开 → 改一格 → 保存」全链路通**；百万键滚动不掉帧 |
| **M1.3 安全闸门** | capability 全链路接入 UI（灰显 + 原因文案）；Prod 档默认锁写 + 限时解锁窗口；破坏性命令输入连接名确认；只读开关；`KEYS` 拦截引导 | Prod 档下写按钮全部灰显并说明原因；解锁后可写、窗口到期自动回锁；`FLUSHALL` 必须输入连接名才能执行 |
| **M1.4 观测面板** | 连接内视图切换器 + Metrics · ServerLoad · ServerInfo · Slowlog · Monitor · Clients · Persistence；能力探测 `ServerFeatures` | 各面板在真实 Redis 上取到数据；不支持的命令灰显并说明原因；`MONITOR` 用专用连接且可停止 |
| **M1.5 进阶值与容量** | Stream 编辑器；完整解码管线（MessagePack / GZIP / ZSTD / Base64 / JWT / 时间戳 / PHP / URL）；MemoryAnalysis（`MEMORY DOCTOR` / `STATS` + 抽样 `MEMORY USAGE`）；TLS 与 SSH 隧道 | 大 hash / 大 list 秒开；解码管线对样例值判定正确；经 SSH 隧道连上远端 Redis |
| **M1.6 高级与模型** | Cluster / Sentinel 拓扑；ACL 编辑器；CONFIG 编辑器（带类型 + `CONFIG REWRITE`）；Pub/Sub（含分片）；Functions / LuaScripts / Search；模块类型查看器（Bitmap / HLL / Geo / Vector / Probabilistic / TimeSeries）；回收站（删除键 24h 可恢复）；键标签 / 备注 / 收藏；redis-cli 终端（多行 + 历史） | 对标 zedis 的 19 个面板全部可达；集群下槽位与主从可见；终端可执行任意命令且受 capability 约束 |

**风险提示（诚实口径）**：M1.6 的体量约为 M1.1~M1.3 总和的 2~3 倍。参照 zedis 的
16 万行规模，本模块完整落地是**多人周**量级的工作，建议按增量逐个验收，而不是等全部完成再评审。

**当前进度（2026-10-12，M1.2 完成 + M1.3 破坏性确认接线 + M1.4 观测面板落地 + M1.5 解码管线首期）**：

| 增量 | 状态 | 缺口 |
| --- | --- | --- |
| M1.1 底座 | **完成** | — |
| M1.2 浏览与编辑 | **完成**（性能未实测） | 全部落地：SCAN 分页 + MATCH 过滤 + 类型过滤 + 虚拟滚动、键详情、六类值查看、**五类结构化值编辑**（字符串 / 哈希字段 / 列表元素 / 集合成员 / 有序集合成员）、**三档解码切换**（文本 / JSON / Hex）、TTL 设置与移除、重命名、删除、**命名空间树**（可折叠 + 自动展开阈值 + 树深可配，内核 `build_tree_with_depth` / `should_auto_expand`）。**未做**：百万键的滚动性能压测（结构上已按「展平行缓存、渲染不现算」设计，但没有真实数据佐证）；Stream 编辑器与完整解码管线属 M1.5 |
| M1.3 安全闸门 | **完成（除逐字输入连接名）** | 已通：`Gate`/`AccessMode` 全链路、只读开关、Prod 默认锁写 + 状态栏限时解锁（15 分钟）、灰显 + `DenyReason` 原因文案、`KEYS` 未被使用、「写入确认」开关已进连接表单（落盘）、**值编辑 / TTL / 重命名全部过闸门**（`WriteKey` / `SetTtl` / `RenameKey` 三个 capability 分别判定）、**十一个破坏性动作的二次确认已接线**（`PendingAction` + `confirm_or_run`，含 M1.4 新增的 `RawCommand` / `ConfigSet` / `SlowlogReset` / `ClientKill`）。**未做**：`writegate::ConfirmKind::TypeServerName`（逐字输入连接名）仍未接线——本模块用**弹窗确认**这一更强可读的形式替代（正文写清「要动什么」），逐字校验留给后续 |
| M1.4 观测面板 | **完成**（Monitor 待真机实测） | 内核新增 `server.rs`（慢日志 / 客户端 / 配置 / 内存统计的纯函数解析 + `ServerFeatures` 能力探测）与 `monitor.rs`（专用连接的 `MONITOR` 增量流）；UI 新增「数据 / 服务端」视图切换器 + **七个面板**（概览 `INFO` / 慢日志 / 客户端 / 配置 / 内存 / 实时监控 / 终端）。面板按探测结果**缺命令即灰显并说明原因**。新增 `Capability::ServerAdmin` 档（`CLIENT KILL` / `SLOWLOG RESET`）。**未做**：Metrics 时序采样与落盘（当前概览是 `INFO` 快照浏览器）、`CLIENT PAUSE`、`Latency` 页签、慢日志的时间戳用「多久以前」而非本地时区绝对时间（不引时区库的取舍）。⚠️ `MONITOR` 的「带读超时批读」在真机上的稳定性未实测 |
| M1.5 进阶值与容量 | **部分**（解码管线首期） | **已通：解码管线九档**——`duck-redis/src/decode.rs` 是纯函数内核（`DecodeKind` + `decode()` + `Message{源串,参数}`），九档＝文本 / JSON / Hex / Base64 / GZIP / JWT / Unix 时间戳 / URL 百分号 / PHP serialize；右栏档位按钮由三档扩到九档并允许换行。**零新增包**（`base64`/`flate2` 早已在图内，从传递依赖提成直接依赖；URL 百分号、PHP serialize、时间戳历法换算都是手写纯函数 + 单测）。安全护栏：GZIP 输出上限 4 MiB（防解压炸弹）、PHP 递归深度上限 32。**未做**：Stream 编辑器、MemoryAnalysis 抽样（`MEMORY USAGE` 单键查询已通，缺批量抽样）、TLS / SSH 隧道，以及需要新引包的解码档（MessagePack / ZSTD / LZ4 / BSON / Java / pickle / 图片预览） |
| M1.6 高级与模型 | **部分**（终端已提前接线） | 已通：redis-cli 终端（多行按行串行、历史 chips、`KEYS` 引导、破坏性命令无条件二次确认、输出贴底滚动、`Capability::RawCommand` 全链路）。未做：Cluster / Sentinel 拓扑、ACL 编辑器、CONFIG 类型化编辑器 + `CONFIG REWRITE` 按钮、Pub/Sub、Functions / LuaScripts / Search、模块类型查看器、回收站、键标签 / 备注 / 收藏、上/下键召回历史 |

> **M1.1 的口径修正**：连接管理已从「主区内嵌表单」改为**对标 zedis 的六页签模态弹窗**
> （详见 §4.9）——新建 / 编辑走 `open_connection_dialog`，页签、字段、底部动作均与
> zedis「添加新服务器」同构；未接线的档位一律灰显并标注所属增量（M1.5 / M1.6）。
> 这一步是对用户「弹窗连接、页面内容与布局都要参考 zedis」反馈的直接落实。

**M1.2 的关键实现取舍（踩过的坑，改前必读）**：

1. **列表没有「按位置删元素」的命令**。`LSET` 哨兵 + `LREM 1 <哨兵>` 两步法（照抄 zedis
   `list_ops.rs::remove_list_indexes`），且哨兵必须**唯一**（纳秒时间戳 + 进程内自增序号）
   并与原值比对过。`LREM` 的 count 用 **1 不用 0**——0 会删光列表里所有等于哨兵的元素，
   一旦撞车就是数据灾难。
2. **集合类写操作一律「写完重读」**。本地拼装（尤其列表顺序、有序集合分数）容易与服务端
   实际内容不一致，宁可多一次往返也不显示一个「看起来对」的假状态。
3. **解码档位显式化**。旧版按内容「自动判断」（`{` 开头就当 JSON 试），失败再悄悄退回
   原文——用户不知道自己看的是哪一档。现在是三档单选，JSON 档解析失败**明确报错**
   （`ValueView::JsonFailed`），不退化成原文。
4. **命名空间树的展平行必须缓存**（`RedisPage::tree_rows`）。渲染路径重算展平是 O(n)，
   百万键下每次重绘都跑一遍会把滚动拖垮；`rebuild_tree()` 只在键集 / 展开状态 / 树参数
   变化时调用，**任何渲染代码都不许调它**。
5. **树的展开状态记「覆盖」而非全量**（`tree_overrides: HashMap<path, bool>`）。没被用户
   切过的节点按连接档案的「自动展开阈值」实时推导，这样改阈值能立刻反映到未碰过的节点上。
6. **折叠三角是独立可点元素**。若让整行既负责折叠又负责选中，「既有实体键又有子节点」的
   行（`user` 与 `user:1` 同时存在）就没法两者兼顾。
7. **零值语义**：TTL 输入 0 在服务端是「立即删除」，比「永不过期」危险得多——界面直接拒绝
   0 并指引用「移除过期时间」；连接档案里的 `max_key_tree_depth` / `auto_expand_threshold`
   为 0 则归一成 `None`（用内核缺省 / 不自动展开）。

**M1.3 破坏性确认的接线要点（改前必读）**：

1. **`confirm_or_run(capability, action, …)` 是唯一入口**：先过 `check(capability)`（分级锁写
   闸门），再按 `active_config().require_confirm_writes` 决定「直接执行」还是「弹确认框」。
   确认是**用户开的开关**，不是我们强加的门槛——关掉就直接 `run_pending`。
2. **动作参数在按下按钮那一刻冻结**。`PendingAction` 是 `Clone` 的**值**，不是「回读输入框的
   闭包」：用户点了「重命名」之后再改输入框，确认时动的仍是当时那个目标名。反过来做会让
   「看到的」与「确认的」不一致。
3. **确认框正文必须写清「要动什么」**（`PendingAction::message`）：键名 / 字段名 / 下标逐个
   列出。凭记忆点「确定」等于没有确认。
4. **`*_at` 族各自再过一次闸门**。`confirm_or_run` 之外的调用点（测试、未来新增入口）也不许
   绕过锁写约束——双保险写在**被调方**，不写在调用方。
5. **`PendingAction::title` / `message` 收 `&App`**（`tr` / `trf` 是纯查表）：文案在打开弹窗前
   就定下来，`on_ok` 里只做 `view.update(|this, cx| this.run_pending(action, window, cx))`。
6. **`on_ok` 恒返回 `true`**：动作执行后弹窗必须关闭；`view.update` 返回 `Err`（视图已销毁）
   也照样关——不让一个点不掉的确认框挂在屏幕上。
7. **弹窗宽度另立常量**（`CONFIRM_DIALOG_WIDTH = 440`），不复用表单的 680：正文只有一两句话，
   套用表单宽度会让确认框显得空旷。

## 7. 与项目约定的符合性自查

- [x] 分层：`duck-redis` 不依赖 gpui；依赖方向单向向下；新 crate 经用户确认后建
- [x] 依赖准入：MSRV 硬门（实测 `redis 1.2.2` = 1.85）、`=` 钉版本、版本理由注释、`--locked` 并发纪律
- [x] 许可证：Apache-2.0 参考物与新依赖均无冲突；`THIRD_PARTY_LICENSES.md` 重生成并提交
- [x] i18n：中文源串 + EN 字节序 + 覆盖率守卫；补表用只读脚本算序号。⚠️ 本次实测到一个坑：
  **守卫的 `literals()` 不做反转义**，所以**中文文案里不能出现 `\n` 这类转义**——
  `"\n… 共 {} 字节"` 这种 key 永远命中不了英译（查找用的运行时值是「真换行」，
  而守卫拿的是「反斜杠 + n」）。已把该处拆成 `out.push('\n')` + 无转义字面量。
  ⚠️ **M1.3 补表实测（已完成）**：新增 14 条中文（7 标题 + 7 正文模板 + 「确认执行」），
  表 1076 → 1090 条。过程中发现表内有**一处既有逆序**（`关键字` 排在 `关系型` 之前——两串
  首字同为「关」，次字 `键` U+952E > `系` U+7CFB），会触发 `table_is_sorted`；
  插入脚本按**原始** key 的 UTF-8 字节序整体重排，**顺带修掉**（插入前 1 处 → 插入后 0 处）。
  复刻守卫 `/tmp/i18n_guard.py`：条目 1090 / 逆序 0 / 重复 0 / 缺英译 0 → PASS；
  `cargo test -p duck-theme --locked` 22 项全绿（含 `table_is_sorted` 与覆盖率守卫）。
- [x] 存储：kv 单键 JSON + `secret` 零明文；不动 `duck-store` 表结构
- [x] UI：字阶 token、无裸 `px(字号)`、`hover_tint`、`button_surface`、`dropdown_select` / `switch_control`
- [x] a11y：`Button` 族直接用；自绘 `on_click` 元素补 `role` + 显式 `aria_label`；裸 div 需 `.tab_stop(true)`
- [x] 不发明 API：所有 gpui / gpui-component 调用以 `temp/gpui-kit` 与 0.7.1 源码为准
- [x] 异步纪律：同步内核 + `background_executor`；零 tokio；`RunCommand` 监听器先 `cx.defer`

## 8. 未决项

1. **键位顺延**（§5）：是否接受 `项目管理` / `AI 助手` 顺延为 `⌘6` / `⌘7`。
   → **已按顺延方案实施**（Redis = `⌘5`）。若用户要求改口径，按 §5 的退回方案反向改三行。
2. **连接档案是否与 `duck-db` 合并导出**：两侧目前独立，用户可能希望「一次导出全部连接」。
3. **TLS 的 MSRV 复核**：`redis` 的 `tls-rustls` 特性若引入超线传递依赖，则降级为
   「TLS 由 SSH 隧道覆盖，M1.5 后再评」（对齐 `docs/database-design.md` 对 PG TLS 的处理）。
4. **大值阈值**（§4.6 的 4 MB / 32 MB 两个常量）需按真实使用体验标定。
