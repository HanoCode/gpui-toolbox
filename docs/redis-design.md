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
duck-redis（内核 crate，不依赖 gpui）
  ├─ config       RedisConfig / EnvTag / 连接档案 wire（serde）
  ├─ capability   AccessMode × 命令可用性 → Capability（纯函数，可单测）
  ├─ writegate    破坏性 / 写命令判定与确认要求（纯函数，可单测）
  ├─ conn         阻塞客户端：连接 / PING 计时 / INFO / TYPE / TTL / SELECT / OBJECT / 各类读写
  ├─ scan         SCAN 游标分页 + 分隔符命名空间分组 + 挂 TTL 的排序构树
  ├─ query        关键字的三种匹配档（子串 / 前缀 / 精确）→ MATCH 串（纯函数）
  ├─ sort         同层排序四档（名称 / TTL × 升降，文件夹恒在前）（纯函数）
  ├─ ttl          过期时间过滤七档 + TTL 芯片文案（纯函数）
  ├─ value        统一值模型 RedisValue（类型 + 分页容器）
  ├─ decode       九档解码管线（文本 / JSON / Hex / Base64 / GZIP / JWT / 时间戳 / URL / PHP）
  ├─ json_tree    JSON 可折叠树模型（纯逻辑，两道节点上限）
  ├─ error        RedisError
  └─ server       服务端观测：慢日志 / 客户端 / 配置 / 内存统计 / 状态栏概览的解析（纯函数）
  └─ monitor      MONITOR 增量流（专用连接）
        ▼
duck-platform（paths / secret / dialog）   duck-store（kv 持久化）
```

> 上面这份是**落地后**的模块清单（2026-10-15 校订）。规划期的草稿里还画过
> `secret` / `keyops` / `panels` 三个模块：凭据最终走了 `config` 的薄封装直连
> `duck-platform::secret`，键操作全在 `conn` 上（本来就是「拼命令 → 取应答」），
> 面板解析落在 `server`、UI 落在 `duck-app/pages/redis/panels.rs`。

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
- **分组**：按 `key_separator` 切分构造树；同一层节点按名称排序；叶子显示**类型徽标 + TTL 芯片**。
  分隔符在 `:` / `.` / `/` 间可配置（zedis 同款）。
- **树行形态（2026-10-15 对齐 zedis，`temp/zedis/src/views/key_tree/delegate.rs`）**：
  一行 = `[缩进][折叠三角][文件夹图标 / 空白槽][类型徽标（定宽列）][键名（占满+截断）][右列]`。

  | 元素 | 文件夹行 | 键行 |
  | --- | --- | --- |
  | 第 3 列 | `Folders` / `FolderOpen` 两态图标 | 与图标同宽的**空白槽**（不画文件图标，保持列对齐） |
  | 第 4 列 | 空 | 类型徽标：`badge_code()`（`STR`/`LIST`/`HASH`/`SET`/`ZSET`/`STRM`，模块类型 `MOD`），**去饱和**类型色，定宽 `36px` |
  | 右列 | 子树键数 | TTL 芯片（定宽 `34px`、右对齐）；**不画**时留等宽空白槽 |

  - 类型徽标是**纯色文字不是填充胶囊**：它会在键树里重复上千次，填充会堆成噪音色带。
    明暗各一套调色板（浅色底 `#e2e2e2` / 深色底 `#222222` 上都 ≥ 4.5:1 对比度）。
  - TTL 芯片三层：`< 1m` 警示红、`< 1h` 琥珀、其余弱化色。
    **`-1`（永久）与 `-2`（键已消失）都不画**——永久键若都挂「∞」，会把少数真会过期的键埋掉。
  - 右列的分支判据是**「这行是不是文件夹」而不是「有没有 TTL」**：键行的元数据可能尚未取回
    （`ttl_secs == None`），按 TTL 有无分支会让它瞬间漏出叶子计数 `1`。
- **键类型与 TTL 靠一次 pipeline 批量取**（`RedisConnection::key_meta`）：每 `META_CHUNK_KEYS`
  （256）个键发一个 `TYPE`+`TTL` 交替的管道，**一趟往返拿回一批**——逐键 `TYPE` 会在 500 键的页面上
  产生 1000 次往返。结果缓存在 `RedisPage::key_meta`，重新扫描时整体清空。
  批量应答与请求**必须等长**：不等长（键在 `SCAN` 与 `TTL` 之间消失等）整批丢弃，绝不按下标错配——
  否则会把 A 键的类型/ TTL 挂到 B 键名下。
- **自动展开阈值缺省 100**（zedis `states/app.rs::auto_expand_threshold` 的 `unwrap_or(100)`）：
  连接档案里「留空」＝缺省 100（小命名空间直接铺开），填 `0` ＝显式关掉自动展开。
  ⚠️ 「留空」与「填 0」**不再是同一个可观察结果**（旧口径两者都等于不展开）。
- **分页与虚拟滚动**：`uniform_list` + `cx.processor(|_, range, _, _| …)` 只渲染窗口内行
  （gpui-component 有同款 API，**以当前源码为准，不发明 API**）。
- **计数**：显示「已加载 / 总数」；总数来自 `DBSIZE`（便宜），加载数来自本地。
  TTL 档挡掉一部分时补一句「匹配 N 个」——只报可见数像扫描漏键，只报加载数又看不出过滤生效。
- **规模目标**：百万键下滚动不掉帧——这是 zedis 的核心卖点，也是本模块必须守住的性能判据。

#### 4.5.1 顶栏四档（2026-10-15 对齐 zedis 的键树工具条）

zedis 的键树工具条按「**这个问题要问谁**」分成两类，本模块照此划线：

| 档位 | 档数 | 生效位置 | 换档后做什么 |
| --- | --- | --- | --- |
| 查询模式 | 3（`*` 子串 / `^` 前缀 / `=` 精确） | **服务端** `SCAN MATCH` | **重扫**（换了问题就得重问） |
| 键类型 | 1 + 6 | **服务端** `SCAN TYPE` | **重扫** |
| 过期时间过滤 | 7 | 本地（读 `key_meta` 缓存） | 只重算视图，**零往返** |
| 排序 | 4（名称 / TTL × 升 / 降） | 本地（读 `key_meta` 缓存） | 只重算视图，**零往返** |

- **查询模式的口径是 `duck-redis/src/query.rs::scan_plan`**，纯函数、可单测：

  | 档位 | 关键字 `user` 的 `MATCH` 串 | 说明 |
  | --- | --- | --- |
  | `All`（缺省） | `*user*` | **子串**匹配。⚠️ 不是「用户自己写 glob」 |
  | `Prefix` | `user*` | 按命名空间下钻最省服务端算力 |
  | `Exact` | —（**不扫**） | 按键名直接取（`key_type` 探一下，`Missing` 就不塞空行） |
  | 任意档 + 空关键字 | —（不带 `MATCH`） | 扫全库；`MATCH **` 结果一样，白让服务端比一次 |

- ⚠️ **三档都把关键字当字面量**（`escape_glob` 转义 `* ? [ ] \`，zedis 同款）：
  `user[1]:` 在任何档位都只匹配它自己。代价是**不能写通配符**——想按前缀找就用
  「前缀匹配」档。占位文案因此不再举例 `user:*`（那会教出错误用法）。
- ⚠️ **`Exact` 与「空关键字」都会得到「没有 `MATCH` 串」，但要做的事相反**（一个不扫、
  一个扫全库），所以内核用三态枚举 `ScanPlan { Everything, Pattern, Exact }` 表达，
  让 `match` 替调用方挡住「把精确档当全库扫」。
- **续扫（「继续加载」）必须问同一个问题**：`MATCH` 串与首页那次共用 `scan_plan`，
  不各拼各的——否则续扫会把「另一个查询」的结果并进来。
- **排序口径**（`duck-redis/src/sort.rs::KeySort`，键树与平铺列表共用）：文件夹行**恒在键行之前**
  （两个方向都是）；TTL 档下「永不过期 / 未知 / 未取到」按 `ttl_rank` 归入垫底组，
  **降序也垫底**——「永不过期」不是一个很大的数字，把它排到队首会把真正要看的快过期键埋掉。
  ⚠️ 这条**改掉了既有形态**：树原先每层是纯字典序，现在顶层同为「文件夹优先」。
  不需要这条语义的调用点继续用 `build_tree_with_depth`（= `NameAsc` + 无 TTL）。
- **过滤与排序都只作用于已加载的键**：`TtlFilter::matches(None)` 恒假（未知 TTL 不命中任何受限档），
  **绝不为一次筛选把整个键空间跑一遍 `TTL`**。
- **平铺列表的行序**存成 `RedisPage::flat_rows: Vec<usize>`（`keys` 的下标序列）而不是键名副本：
  `selected` 继续指向 `keys`，换排序不会让「谁被选中」跟着跑偏。
  树形态的行序在构树时定死（`build_tree_sorted`），展平只读缓存。

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
- **主区**：视图切换器（数据 / 服务端）→ **两行工具条**（第一行查询模式 + 关键字框，
  第二行类型过滤 + 过期时间过滤 + 排序）→ 左侧命名空间 key 树 + 右侧值编辑器（两栏；
  M1.4 起视图切换器右侧接服务端面板）。
- **底部状态栏**：**左半段**「这一页刚做了什么」+ **右半段**「服务端现在什么样」
  （版本 · 访问模式 · 库选择器 · 键数 · 延迟 · 内存 · 类型构成），见 §4.8.2。
  访问模式与解锁按钮在这里（不在顶栏）。
- **右栏**（shell 级 `with_right_sidebar`）：键详情（类型 / 编码 / 空闲时间 / TTL / 字段数）
  + 危险操作区（TTL / 重命名 / 删除，受 capability 约束）。
- **规格**：全部遵从 `AGENTS.md` §3.6 —— 字阶走 `duck_theme::builtin::base_tokens`、
  **禁止裸 `px(字号)`**、悬停一律 `hover_tint`、对比度过 `button_surface`、
  下拉用 `dropdown_select`、布尔项用 `switch_control`。
- **i18n**：源串直接写中文，英译全部进 `duck-theme/locales/en.toml`（**顺序无关**，
  由 rust-i18n 编译期装载）；改完跑 `scripts/i18n_en_toml.py` 同步 key 清单。
  ⚠️ 2026-10-15 起数据源从 `i18n.rs` 的 `static EN`（UTF-8 字节序 + 二分查找）迁来，
  本文下方「按字节序插入 EN 表」的写法为迁移前口径。
- **图标**：新增 redis 专用图标进 `duck-components/assets/icons/`（Lucide 1.75 线宽），
  在 `icons.rs` 的 `duck_icons!` 宏内**按字母序**登记（该宏顺序即 `ALL` 顺序）。

#### 4.8.1 主区两栏 + 值编辑器（2026-10-15 落地，P3）

上面「主区 = 键树 | 值编辑器」这条**从描述变成了实现**，开工前先记住三件事：

- **右侧值栏是主区内的分栏，不是 shell 右栏**。用的是 gpui-pre 的
  `h_resizable` + `resizable_panel`（和左栏 `SidebarPanel` 内部同一套原语），
  右侧面板 `flex_none` + `size_range`，拖动改它、余量给左栏。
  ⚠️ **宽度不存在页面字段里**：`resizable_panel().size()` 按上游文档只是
  「创建时的初始宽度」，真正的宽度由 `ResizableState` 自己持有（所以它得作为
  `Entity` 留在页面上）。再存一份就是第二个真源。
  ⚠️ `ResizablePanelGroup` **只实现 `RenderOnce`**，没有 `flex_1`，而它的
  `size()` 是「cross axis」（横向组上＝高度、不是弹性分配）→ 外面套一层
  `flex_1` 的壳把余量给它。
  ⚠️ 右栏的树档用 `uniform_list`，所以它**直接挂在右栏 `v_flex` 下**，
  与键列表同一个坑（包一层 block `div()` 就整表不渲染）。

- **两条正交的轴，不是一条**。「值怎么显示」分成交叉的两个开关：

  | 轴 | 档位 | 回答 | 换了之后 |
  | --- | --- | --- | --- |
  | 解码 `DecodeKind` | 9 | 字节**怎么变成**文字 | 重算展示态（原始字节留在 `KeyDetail::raw`，不跑网络） |
  | 呈现 `ValueViewMode` | 2（文本 / 树） | 这段文字**怎么摆** | 本地重算行序，零往返 |

  合成一个开关会在「文本 + 树」这种组合上立刻自相矛盾。**树档只在 JSON 解码档下
  成立**（`build_detail` 一处决定，`json_available()` 不重复判定）：不做「文本档里
  恰好是合法 JSON 也偷偷给树」——那会让档位按钮的含义不可预测。
  呈现档位是**偏好**不是**当前状态**：非 JSON 键上 `effective_value_view()` 回落
  到文本，偏好原样留着，换回 JSON 键自动回到树。

- **折叠状态跟着键走**：`adopt_json_tree(key)` 在换键时按默认档（展开两层）重来，
  同一个键重新加载（写完刷新 / 手动刷新）保留用户的折叠。判据不是「上一个键也是
  它」，而是「上一个键那时**确实有树**」——同一个键从文本档切到 JSON 档时，折叠
  集合是从「没有树」的状态里出来的，必须补一次默认展开。
  展平后的行存 `json_rows`（`Arc<Vec<JsonTreeRow>>`）：**渲染期只读**，
  每帧重算会为两万节点各克隆一次 `path` / `name` / `preview`。

- **JSON 树模型在内核**（`duck-redis/src/json_tree.rs`，纯逻辑零 gpui）：
  `json_tree` 建节点树、`flatten` 摊成行、`default_expanded` / `toggle_path` 管展开
  集合。两道上限都**如实计入 `omitted`**：单容器 `JSON_TREE_MAX_CHILDREN = 500`
  （zedis 同款）、**整棵树 `JSON_TREE_MAX_NODES = 20000`**（本条 zedis 没有——
  它只限每层，500×500 的两层嵌套就是 25 万节点）。
  **成员顺序是文档里的原始顺序**——本 crate 的 `serde_json` **显式打开了
  `preserve_order`**，而 JSON 档的文本形态走的同一条路（`from_str` →
  `to_string_pretty`），所以两个档位看到的顺序一致，切档不会让键换位置。
  ⚠️ **这个特性必须显式打开，不能靠特性统一「碰巧」拿到**：工作区里 `schemars`
  （经 gpui-base / gpui-component）会**传递地**打开它，于是
  `cargo test -p duck-redis` 与 `cargo test -p duck-theme -p duck-redis` 曾经
  得到**两种顺序**（`BTreeMap` 键序 vs `IndexMap` 原文序）——单跑绿、一起跑红。
  `crates/duck-redis/Cargo.toml` 里那条 `features = ["preserve_order"]` 就是把这个
  口子焊死，`object_members_keep_document_order` 是它的守卫。
  参考实现同样是显式开的（`temp/zed/Cargo.toml`：
  `serde_json = { features = ["preserve_order", "raw_value"] }`）。

- **键详情多了「内部编码 / 空闲时间」**：`OBJECT ENCODING` + `OBJECT IDLETIME`
  走 `RedisConnection::object_info`，**只对选中的那一个键**发（两条命令一次往返）。
  ⚠️ **绝不加进批量 `key_meta` 的 pipeline**：那条每键两条命令（`TYPE` + `TTL`），
  再加两条就翻倍，而一页 1000 键多 2000 条命令只为了两列锦上添花的信息。
  该方法**永不失败**（返回 `KeyObjectInfo` 而不是 `Result`）：`OBJECT` 在代理 /
  托管云 / `NOPERM` 上都可能不可用，拿不到就 `None`、**整行不画**——
  画一行「未知」等于把「服务端没说」编成了一个值。

- **值面板顶栏**：键名 + 类型标签 + 原始大小 + TTL + 刷新 + 保存。
  「原始大小」是**服务端上那一份的字节数**（解码 / 解压之前），所以和值本体显示的
  长度可以差很多，这正是要同时给出两者的原因；集合类给元素条数。
  「刷新」**复用写成功后的 `reload_selected`**，不另写一条只取值的路径。
  「保存」只在字符串类型下画（其余类型的写入口是下方的结构化表单），
  而不是画一个灰按钮占位。

- **值本体不再出现在 shell 右栏**。右栏只剩「关于这个键」：键名 / 类型 / 内部编码 /
  空闲时间 / 过期时间 / 元素数 + 只读原因 + 危险操作区（TTL、重命名、删除此键）。

#### 4.8.2 状态栏（2026-10-15 落地，P4）

状态栏分成**左半段 + 右半段**，写入方是分开的——这是本节最要紧的一条：

| 半段 | 内容 | 写入方 |
| --- | --- | --- |
| 左 | 「这一页刚做了什么」（正在扫描 / 已加载 N / 已删除键 X / 连接失败原因） | 各功能自己写 `self.status` |
| 右 | 「这台服务端现在什么样」（版本 · 访问模式 · 库 · 键数 · 延迟 · 内存 · 类型构成） | **只有** `status.rs` 的探测写 |

一个数字只有一个写入方，才不会出现「谁把它覆盖了」的问题。

**右半段五段**（都在「已连接」时才渲染）：

| 段 | 来源 | 形状 |
| --- | --- | --- |
| 库 `db_switcher` | 档案 `db` + `INFO keyspace` | 下拉，按钮只写 `db3`；菜单项写 `db3 · 12 键`（非空库才缀键数） |
| 键数 | `DBSIZE` / 本地 `keys` | `9 / 120 键` |
| 延迟 | 计时 `PING` | `延迟 0.42 ms` |
| 内存 | `INFO memory` | `内存 1.05M`；设了上限才 `1.05M / 512.00M` |
| 类型构成 | 本地 `key_meta`（**不发命令**） | `字符串 6 · 哈希 2 · 列表 1`（>3 类折 `· +N more types`） |

**探测只在事件上跑，不轮询**。一次探测 = 一次计时 `PING` + 一条 `INFO memory` /
`INFO keyspace` 的 pipeline（两条命令一个往返）。触发点三个：连接成功、切换库、
用户点右端「重新探测」。**刻意不做定时轮询**——空闲页面每秒往服务端发三条命令是纯
浪费，而本模块两条硬纪律都是冲着「别给服务端添无谓负载」去的。代价是数字会陈旧，
所以刷新按钮一直在，且**探测失败不清空旧值**（显示一个略微过期的真值比显示空白有用）。

⚠️ **探测走的派发器不动 `busy`**（`spawn_probe` 而不是 `spawn_redis`）。`busy` 的语义
是「有**用户在等**的操作在跑」，它挡着「继续加载」也参与面板按钮灰显。把探测算进去有
两个后果：①「连接成功后先 `refresh_keys` 再 `probe_status`」这类顺序里，探测会因为扫描
刚把 `busy` 立起来而**静默不跑**；②探测先回来时把 `busy` 清成 `false` 而扫描还在飞，
「继续加载」提前可点，会拿同一个游标再发一次。探测本身只是一个往返、又与其它调用在
连接互斥锁上自然串行，不需要 `busy` 兜底。

⚠️ **`switch_db` 的顺序不能反**：先写档案再 `SELECT`，一旦 `SELECT` 失败（库号超出
服务端 `databases` 配置）界面会显示 db9、连接却还停在 db0，两边永久错位。所以**档案
只在 `SELECT` 成功之后才改**，然后才落盘 + 作废探测结果 + 重扫 + 重新探测。落盘是必须
的：状态栏的 DB 段与左栏连接行都读 `active_config().db`，不落盘就会「切到 db2、退出重进
又回到 db0」。

⚠️ **类型统计的口径（诚实优先）**：数的是**已加载的键**，不是整个键空间。逐类型的精确
计数要跑六七遍 `SCAN TYPE`，百万键下就是六七次全库遍历——正是硬纪律要避免的那类操作。
所以它紧跟在「`N / M 键`」后面，读起来是「这 N 个键的构成」而不是「全库的构成」。
`Missing` 不进表（它表示「元数据还没回来 / 键已消失」，不是一种类型），数量为 0 的类型
也不进表。排序用**稳定** `sort_by`：数量相同时保持内核 `KeyType::ALL` 的顺序，于是同一
批键每次都得到同一串文字，不会在两次重绘之间抖。

**内核新增**（`duck-redis`）：

- `RedisConnection::ping_latency()` —— 计时 `PING`，量的是**客户端观测到的往返**
  （含服务端执行 + 网络 + 客户端处理），不是服务端内部延迟；跨网络 / 走代理时天然偏大。
  这是所有客户端状态栏共用的口径，误差方向也是用户能理解的那一侧。
- `RedisConnection::status_overview()` —— 两条 `INFO <section>` 进一条 pipeline，
  **永不失败**（返回 `StatusOverview` 而不是 `Result`）。⚠️ 分成两个 `INFO <section>`
  而不是一次 `INFO`：整份 `INFO` 有几十个字段、几 KB 正文，状态栏只用四五个。
- 纯解析（`server.rs`）：`memory_overview` / `parse_keyspace` / `status_overview_from`。
  ⚠️ `maxmemory` 为 `0` = **未设上限**，此时上限的两个字段**一起**变成 `None`——
  把 `0B` 摆上去会被读成「内存已满」。⚠️ `INFO keyspace` **只列有键的库**，所以
  `StatusOverview::keys_in` 把「查不到」与「键数为 0」钉成同一件事。

**顺手修掉的一个旧账**：删除键后 `dbsize` 不减。状态栏前半段（已加载）减了、后半段
（总数）不减，看起来就像「扫漏了一个键」。现在删除成功时 `dbsize -= deleted`。

#### 4.8.3 键的本地标注（2026-10-15 落地，M1.6 首批）

对标 zedis `zedis-db/src/key_metadata_manager.rs`：给每个键挂**标签色 / 备注 / 收藏**三样
本地元数据。这是 M1.6 里**唯一零网络、零新依赖**的一项——所有数据只在本机 kv 里，
**一个字节也不发往服务端**，所以它也是全模块里唯一**不过写闸门**的写操作。

**内核模型**（`duck-redis/src/tags.rs`，纯逻辑、无 gpui、无 IO）：

| 类型 | 形状 | 说明 |
| --- | --- | --- |
| `TagColor` | 六值枚举 `Red/Orange/Yellow/Green/Blue/Purple` | `#[serde(rename_all = "lowercase")]`；`as_str()`/`from_name()` 为首末双向的 wire 名（trim + 忽略大小写）；`index()` 落进直方图下标 |
| `KeyMetadata` | `{ tag: Option<TagColor>, note: String }` | 空字段 `skip_serializing_if`，落盘时不写 `tag`/`note` 空值 |
| `KeyTags` | `entries: BTreeMap<String, KeyMetadata>` + `favorites: BTreeSet<String>` | **单台服务端**的全部标注 |
| `TagStore` | `{ version: u32, servers: BTreeMap<String, KeyTags> }` | 顶层信封，`servers` 以**连接 id** 为键 |

⚠️ **收藏与标签是两套存储**（照抄 zedis：收藏在 `history_manager` 的 `FAVORITES_TABLE`，
标签在 `KeyMetadataManager`，二者不相干）。这里把它们放进同一个 `KeyTags` 但**用不同的
容器**——`favorites` 是 `BTreeSet`，`entries` 是 `BTreeMap`。这样「收藏了但没打标签」
是一个**可表达**的状态，而不是靠「tag = 某个灰」去凑。三样东西各自的语义：

- **标签色**：一个键**至多一个**颜色（`Option<TagColor>`，不是集合）；
- **备注**：一段自由文本，可以只有备注没有颜色；
- **收藏**：独立的布尔标记，与颜色、备注都无关。

⚠️ **「既无标签又无备注」不是一个记录**。`set(key, None, "")` 走的是**删除**分支而不是
写一条空行——否则清空一个键的标注会在 store 里留一条 `{}`，`len()` 也对不上用户看到的
「已标 N 个」。

**按连接隔离**：`TagStore.servers` 以连接 id 为键，因此**两个不同的服务端上同名的
`user:1` 不会串味**（`two_servers_annotating_the_same_key_name_do_not_collide` 钉这条）。
连接被删除时 `forget_connection` 整块摘掉该连接的分区，`drop_if_empty` 保证摘空后不留空壳。

**版本化信封**：顶层写 `version: 1`，读到**未来版本**（`> TAG_STORE_VERSION`）时
`from_json` **整体回落成空 store**，而不是尝试按旧结构反序列化——宁可丢一次本地标注，
也不让一个结构不匹配的文档把整个页面炸在启动路径上（`a_future_envelope_version_falls_back_to_empty`）。

**文件夹标签色 = 后代众数**。这是本节最容易被忽略的一块：键树里的**文件夹行**要显示
它**下面所有键**的标签色聚合。做法在 `scan.rs::stamp_folder_tags`：一次后序遍历，
每个节点返回**子树六色直方图**，叶子把自己的颜色计 1；内节点把子节点的直方图求和后
**众数**即该文件夹的颜色（`resolve_folder_tag`），平票时取**下标较小**者（调色板顺序，
稳定不抖）。返回值是 `Option<(TagColor, bool)>`——第二个 bool 是 `tag_mixed`，表示子树里
**不止一种**颜色，UI 据此把左侧色条画成**虚线**。全零直方图返回 `None`（不打色）。
⚠️ 只对 `!children.is_empty()` 的节点盖章：叶子键行显示的是**自己的**颜色，不参与聚合。

**过滤是树构建前的预筛**。`TagFilter`（`All` / `Tagged` / `Untagged` / `Favorites` /
`Color(_)` 共 **10 档**：4 个聚合档 + 6 个颜色档）的 `matches(tag, favorite)` 在
`rebuild_tree` / `rebuild_flat` **构建之前**就作用在键集上，而不是构建完再滤行——
否则被滤掉的键仍会参与文件夹聚合，出现「文件夹有颜色但底下没键」的假象。同理，
`stamp_folder_tags` 的闭包取的是**已过滤键集**的 `tag_of`。

**标注在哪里丢、在哪里绝不丢**（这是本批最容易踩的取舍）：

| 时机 | 行为 | 原因 |
| --- | --- | --- |
| 显式删除某个键（`delete_key_at` 成功） | `forget_key_tags` 删该键的标注 | 键真没了，留着是垃圾 |
| 显式清空某键的标注 | `set(key, None, "")` 删除记录 | 见上「不是记录」 |
| 删除整条连接 | `forget_connection_tags` 摘该连接分区 | 连接没了 |
| **重新扫描 / 翻页 / 换 MATCH** | **不动** | ⚠️ SCAN 是分页的，当前页看不到的键**不代表已被删**；按「已加载的键」裁剪标注会把其它页的标注全抹掉 |

⚠️ 最后一行是本节的**红线**：标注**永不**按「当前加载的键集」裁剪。它只跟键的**显式删除**
与**显式清空**走。这一条直接决定了「备注输入框」的行为——见下。

**右栏编辑区**（`sidebar.rs::tag_editor`）：七个色块（「无」+ 六色，`swatch` 复用同一
个按钮形态）+ 一个收藏 `icon_button`（带 tooltip）+ 一个备注 `Input`。它挂在键详情描述表
**之后**、只读原因**之前**。⚠️ 它**刻意不套写闸门**——写的是本地 kv，不是服务端；把它
灰显成「只读模式下不可用」会让人误以为标注也要联网。

**备注自动保存，没有按钮**。`note_input` 是一个 `Entity<InputState>`，订阅 `InputEvent::Change`：
读出 `value()` 与 `active_tags().note_of(&key)` 比对，**相等就直接返回**（避免回填自身触发
的死循环），不等才 `set_key_note`。切换键时 `select_key` 把新键的备注**回填**进输入框。

**重绘范围按「是否影响文件夹色」区分**（`after_tags_changed` 的取舍）：

- **改颜色**（`set_key_tag`）→ **必须**重建视图：颜色会影响所有祖先文件夹的聚合众数；
- **改备注**（`set_key_note`）→ **只在**「有无备注」这个**布尔**翻转时才重建（`has_note`
  变了才影响行上的铅笔标记），文本内容变化本身不重建；
- **收藏**（`toggle_key_favorite`）→ 重建（影响星标与 Favorites 档的过滤结果）。

⚠️ `TreeRow` 只存 `has_note: bool`，**不存备注正文**。百万键 × 每行一个 `String` 是纯浪费；
行上只需知道「有没有铅笔」这一个 bit。

**UI 落点**（duck-app 里共 5 个文件）：

| 文件 | 改动 |
| --- | --- |
| `pages/redis/tags.rs`（新） | `static NO_TAGS: KeyTags`；`active_tags` / `load_tags` / `persist_tags` / `set_key_tag` / `set_key_note` / `toggle_key_favorite` / `set_tag_filter` / `forget_key_tags` / `forget_connection_tags` + 三个文案/取色自由函数 |
| `pages/redis/mod.rs` | `tags` / `tag_filter` / `note_input` 三个字段；`TreeRow` 增 `tag`/`tag_mixed`/`favorite`/`has_note`；`TreeShape` 增 `tags: &KeyTags`；`flatten_tree` 填这些字段；`new()` 建 `note_input` 并订阅 |
| `pages/redis/keys.rs` | `rebuild_flat` / `rebuild_tree` 应用 `tag_filter`；树构建后调 `stamp_folder_tags`；`flat_view` 加 `tag_filter`/`tags` 两参（6 参）；`select_key` 回填备注；`delete_key_at` 删标注 |
| `pages/redis/browser.rs` | 工具条插 `tag_switcher`（在弹性空隙与 TTL 档之间）；树行左缘色条 + 混色虚线 + 星标/铅笔标记；平表行同样装饰 |
| `pages/redis/sidebar.rs` | 右栏插 `tag_editor` |
| `pages/redis/connect.rs` | `delete_connection` 里 `forget_connection_tags` |

**取色**：`tag_color(color, dark) -> Hsla` 给六色各配一对明/暗 hex（`duck_theme::color_from_rgb`
取色），星标同理（暗 `0xd8b45c` / 明 `0xa8801a`）。左缘色条宽 `TREE_TAG_BAR_WIDTH = 3.`
（`px`），色块 `TAG_SWATCH = 16.`。

#### 4.8.4 本地回收站（2026-10-15 落地，M1.6 第二批）

对标 zedis `zedis-db/src/trash.rs` + `src/views/trash_dialog.rs`：删掉的键在本机留
**24 小时**，可以恢复。和 §4.8.3 同属「零服务端占用」那一类——**删键那条路照旧只发 `DEL`**，
额外的三条命令（`MEMORY USAGE` / `PTTL` / `DUMP`）都在删除**之前**发一次，用来把
「即将被删的那一份」抄下来。

**内核模型**（`duck-redis/src/trash.rs`，纯逻辑、无 gpui、无 IO）：

| 项 | 形状 | 说明 |
| --- | --- | --- |
| `TrashEntry` | `{ key, db, pttl_ms, deleted_at_ms, payload: Vec<u8> }` | `payload` 是 `DUMP` 原样字节；`db` 记删除时所在的库 |
| `TrashEntry::id()` | `{毫秒:020}:{库}:{键}` | 零填充 ⇒ **按 id 字典序 = 按删除时间序**（zedis 存 redb 的复合键同构） |
| `TrashMeta` | `id / key / db / pttl_ms / deleted_at_ms / bytes` | 列表行；**只带载荷长度、不带载荷** |
| `TrashStore` | `{ version, servers: BTreeMap<连接 id, Vec<TrashEntry>> }` | 版本化信封；每台连接的桶按删除时间**升序** |

**四个常量**（前两个照抄 zedis 的值，后两个是本仓加的）：

| 常量 | 值 | 作用 |
| --- | --- | --- |
| `TRASH_RETENTION_MS` | 24h | 保留窗口，**不可配** |
| `TRASH_MAX_VALUE_MEMORY` | 4 MiB | `DUMP` **之前**的闸（`MEMORY USAGE` 估算） |
| `TRASH_MAX_PAYLOAD` | 1 MiB | `DUMP` **之后**的闸（精确长度），超了**永久删除、不入站** |
| `TRASH_MAX_ENTRIES` / `TRASH_MAX_TOTAL_BYTES` | 200 / 16 MiB | 见下「三处偏差」 |

**两道体积闸的顺序不能反**：`MEMORY USAGE` 是**估算**，先用它挡掉「一看就太大」的键，
省掉一次昂贵的 `DUMP`；`DUMP` 回来后按**精确长度**再判一次。两者之间留 4 倍余量，
因为内存占用普遍高估（序列化、压缩后的）`DUMP` 体积好几倍——卡在同一水平会误伤一批
本来装得下的中等值。⚠️ `MEMORY USAGE` **探测失败（`None`）一律放行**（代理 / 托管云 /
无权限都会拒它），届时的兜底是第二道闸；这与 `object_info` 那类「拿不到就不显示」的
取舍方向一致——这里是「拿不到就不拦」。

**入站是尽力而为，绝不影响删除**。`capture_stash` 任何一步失败都只返回 `None`，
调用方照旧发 `DEL`（对齐 zedis `stash_key_to_trash` 的口径：回收站是安全网，
不是一个能让删除失败的依赖）。`PTTL` 单独读不到时退成 **`-1`（无过期）**而不是整份丢弃
——副本本身还是好的。

**恢复走 `RESTORE`，刻意不带 `REPLACE`**：同名键在删除之后又被建出来时，服务端以
`BUSYKEY` 拒绝，界面把它算作「**跳过**」而不是「失败」——从回收站恢复一个键不该顺手
覆盖别人刚写进去的数据。删不掉就留着，下次再说。

⚠️ **恢复另开一条专用连接**（`restore_entries`）。理由有两层：① `RESTORE` 落在**当前
`SELECT` 的库**上，而回收站**跨库**列条目（每条记着自己被删时所在的 `db`），必须按条目
`SELECT` 回它的库；② 在页面那条活会话上 `SELECT` 来 `SELECT` 去会与状态栏缓存的
`active_config().db` 脱节（切一次库就永久错位，见 §4.8.2 的 `switch_db` 顺序）。
专用连接「建连 → 按条目 `SELECT` → `RESTORE` → 用完即弃」把这两件事一起解决，代价只是
一次建连（手动动作，可以接受）。zedis 的 `ServerDb::new(server_id, entry.db)` 是同一件事
（它那边是按 db 从连接池取一条）。

**恢复是写操作**，所以过写闸门（`Capability::WriteKey`）：只读会话 / 未解锁的生产档下
不给恢复，按钮灰显。**本地摘行**（永久移除、恢复成功后的清理）纯本地、不联网，
锁写闸门管不着——它改的是本机 kv，不是服务端数据。

**过期清理只在事件上跑，不设定时器**，两个触发点：**新删一个键时**顺手扫一遍
（`stash_deleted_key`）、**打开回收站时先清后列**（所以列表里永远不会出现
「已经恢复不了」的行）。另外 `purge_all_expired` 留给启动兜底（某台连接可能从此
再没被删过键、也没被打开过回收站）。回收站不需要「准时」，只需要「别无限长」。

**与本仓 kv 底座的三处偏差**（都是刻意的，写在内核模块头里）：

1. **载荷 base64 进 JSON**：`duck-store` 的 kv 是 SQLite 的 `kv(key TEXT, value TEXT)`，
   没有二进制列。代价 33% 膨胀 + 「一次删键重写整份文档」，所以有下面两条预算把它压住，
   而不是靠「一般不会删很多键」这种侥幸。`TrashEntry.payload` 的内存表示始终是 `Vec<u8>`，
   base64 只出现在落盘那一层（`#[serde(with = "payload_b64")]`）。
2. **另加两条预算**（条目数 200 / 总字节 16 MiB），超出时按**删除时间从旧到新**驱逐
   ——保「刚删的」比保「很久以前删的」有用。zedis 用 redb（一行一条）没有
   「一次重写多少钱」的问题，我们的底座有，所以必须显式封顶。
3. **过期判定用毫秒比较**，不是 id 前缀的字符串比较——因为 id 不再是存储主键，
   它只是回传给界面的一个稳定句柄。

**UI 落点**（duck-app 里 3 个文件 + 1 个新文件）：

| 文件 | 改动 |
| --- | --- |
| `pages/redis/trash.rs`（新） | `KeyStash` / `capture_stash` / `RestoreSummary` / `restore_entries` + 6 个页面动作（`stash_deleted_key` / `drop_trash_row` / `restore_trash` / `forget_connection_trash` / `open_trash_dialog` / `load_`·`persist_trash`）+ 弹窗视图 `TrashDialog` |
| `pages/redis/mod.rs` | `trash: TrashStore` 字段 + `KEY_TRASH` 单键 + `load_trash` 初始化 |
| `pages/redis/keys.rs` | `delete_key_at` 在 `DEL` **之前** `capture_stash`，成功后 `stash_deleted_key`；状态栏文案区分「可在回收站恢复」/「未进回收站」 |
| `pages/redis/browser.rs` | 工具条在「重新扫描」旁加「回收站」入口（有条目时标题缀条数） |
| `pages/redis/connect.rs` | `delete_connection` 里 `forget_connection_trash` |

**弹窗为什么是独立视图**：正文里有过滤输入框（要「边打边过滤」），所以 `TrashDialog`
必须是 `Entity` 并自己 `notify()`；宿主页面的 `cx.notify()` **到不了** `content` 闭包
（同 `duck-components/src/dialog.rs` 模块头那条坑）。它另外 `cx.observe(&page)` 订阅页面，
于是「页面里的回收站变了」也能带动弹窗重绘。

⚠️ 列表容器**不能用 `max_h`**（同 zedis 那条坑）：`Scrollable` 会把调用方的尺寸样式抄到
内层内容上，强制 `h_auto` 不会重置 `max_h`，于是被夹住的是内容本身、永远滚不动。
所以超过 7 条时换成一个**定高**（320pt）的滚动视口，否则直接铺开。

⚠️ 定高视口还有第二个必须满足的条件：**`.id()` 不能省**。本仓的 GPUI 分叉把
`overflow_y_scroll` 挂在 `StatefulInteractiveElement` 上（**只对 `Stateful<E>` 实现**，
因为滚动位置要按 ElementId 保留），裸 `div()` / `v_flex()` 根本没有这个方法——漏了会得到
`E0599: no method named overflow_y_scroll found for struct gpui::Div`。同款先例见
`pages/mail.rs` 的 `mail-sidebar-scroll` 与 `pages/redis/sidebar.rs` 的
`redis-connections-scroll`（这里用 `redis-trash-scroll`）。

「全部恢复」在**有过滤时作用于当前看到的那一批**（对齐 zedis），所以它的禁用条件看的是
过滤后的可见条数，不是总条数。

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

**当前进度（2026-10-15，M1.2 完成 + M1.3 破坏性确认接线 + M1.4 观测面板落地 + M1.5 解码管线首期 + P3 值编辑器 + P4 状态栏 + M1.6 键标注与回收站 + P5 界面复刻 zedis）**：

| 增量 | 状态 | 缺口 |
| --- | --- | --- |
| M1.1 底座 | **完成** | — |
| M1.2 浏览与编辑 | **完成**（性能未实测） | 全部落地：SCAN 分页 + MATCH 过滤 + 类型过滤 + 虚拟滚动、键详情、六类值查看、**五类结构化值编辑**（字符串 / 哈希字段 / 列表元素 / 集合成员 / 有序集合成员）、**三档解码切换**（文本 / JSON / Hex）、TTL 设置与移除、重命名、删除、**命名空间树**（可折叠 + 自动展开阈值 + 树深可配，内核 `build_tree_with_depth` / `should_auto_expand`）。**2026-10-15 对齐 zedis 工具条**：键树行形态（类型徽标 + TTL 芯片 + 文件夹两态图标）、**三档查询模式**（`*` 子串 / `^` 前缀 / `=` 精确）、**过期时间过滤 7 档**、**排序 4 档**（均见 §4.5.1）。**未做**：百万键的滚动性能压测（结构上已按「展平行 / 可见行缓存、渲染不现算」设计，但没有真实数据佐证）；Stream 编辑器与完整解码管线属 M1.5。**2026-10-15 P3 值编辑器**：主区改**两栏**（键树 | 值编辑器，可拖），值面板带**文本 / 树两档呈现**（JSON 可折叠树，内核 `json_tree`）、原始大小 / TTL / 刷新 / 保存，shell 右栏瘦身为「键详情 + 危险操作区」并新增 `OBJECT ENCODING` / `IDLETIME`（详见 §4.8.1）。**2026-10-15 P4 状态栏**：右半段四段——**DB 选择器**（`INFO keyspace` 标出非空库，切换走 `SELECT` + 落盘档案 + 重扫）、**计时 `PING` 延迟**、**`INFO memory` 内存**（含上限与占比）、**类型构成**（按已加载 `key_meta` 统计，零命令）；探测只在「连接 / 切库 / 点刷新」三个事件上跑，**不轮询**；顺手修掉「删键后 `dbsize` 不减」的旧账（详见 §4.8.2） |
| M1.3 安全闸门 | **完成（除逐字输入连接名）** | 已通：`Gate`/`AccessMode` 全链路、只读开关、Prod 默认锁写 + 状态栏限时解锁（15 分钟）、灰显 + `DenyReason` 原因文案、`KEYS` 未被使用、「写入确认」开关已进连接表单（落盘）、**值编辑 / TTL / 重命名全部过闸门**（`WriteKey` / `SetTtl` / `RenameKey` 三个 capability 分别判定）、**十一个破坏性动作的二次确认已接线**（`PendingAction` + `confirm_or_run`，含 M1.4 新增的 `RawCommand` / `ConfigSet` / `SlowlogReset` / `ClientKill`）。**未做**：`writegate::ConfirmKind::TypeServerName`（逐字输入连接名）仍未接线——本模块用**弹窗确认**这一更强可读的形式替代（正文写清「要动什么」），逐字校验留给后续 |
| M1.4 观测面板 | **完成**（Monitor 待真机实测） | 内核新增 `server.rs`（慢日志 / 客户端 / 配置 / 内存统计的纯函数解析 + `ServerFeatures` 能力探测）与 `monitor.rs`（专用连接的 `MONITOR` 增量流）；UI 新增「数据 / 服务端」视图切换器 + **七个面板**（概览 `INFO` / 慢日志 / 客户端 / 配置 / 内存 / 实时监控 / 终端）。面板按探测结果**缺命令即灰显并说明原因**。新增 `Capability::ServerAdmin` 档（`CLIENT KILL` / `SLOWLOG RESET`）。**未做**：Metrics 时序采样与落盘（当前概览是 `INFO` 快照浏览器）、`CLIENT PAUSE`、`Latency` 页签、慢日志的时间戳用「多久以前」而非本地时区绝对时间（不引时区库的取舍）。⚠️ `MONITOR` 的「带读超时批读」在真机上的稳定性未实测 |
| M1.5 进阶值与容量 | **部分**（解码管线首期） | **已通：解码管线九档**——`duck-redis/src/decode.rs` 是纯函数内核（`DecodeKind` + `decode()` + `Message{源串,参数}`），九档＝文本 / JSON / Hex / Base64 / GZIP / JWT / Unix 时间戳 / URL 百分号 / PHP serialize；右栏档位按钮由三档扩到九档并允许换行。**零新增包**（`base64`/`flate2` 早已在图内，从传递依赖提成直接依赖；URL 百分号、PHP serialize、时间戳历法换算都是手写纯函数 + 单测）。安全护栏：GZIP 输出上限 4 MiB（防解压炸弹）、PHP 递归深度上限 32。**未做**：Stream 编辑器、MemoryAnalysis 抽样（`MEMORY USAGE` 单键查询已通，缺批量抽样）、TLS / SSH 隧道，以及需要新引包的解码档（MessagePack / ZSTD / LZ4 / BSON / Java / pickle / 图片预览） |
| M1.6 高级与模型 | **部分**（终端 + 键标注 + 回收站已提前接线） | 已通：redis-cli 终端（多行按行串行、历史 chips、`KEYS` 引导、破坏性命令无条件二次确认、输出贴底滚动、`Capability::RawCommand` 全链路）；**键的本地标注**（标签色 / 备注 / 收藏，纯本地零网络、按连接隔离、文件夹色=后代众数 + 混色虚线、10 档标签过滤、右栏编辑区、备注自动保存，详见 §4.8.3）；**本地回收站**（删键前 `DUMP` 抄一份、24h 保留、两道体积闸、跨库 `RESTORE` 恢复且跳过已存在的键、工具条入口 + 过滤 / 恢复 / 移除 / 全部恢复弹窗，详见 §4.8.4）。未做：Cluster / Sentinel 拓扑、ACL 编辑器、CONFIG 类型化编辑器 + `CONFIG REWRITE` 按钮、Pub/Sub、Functions / LuaScripts / Search、模块类型查看器、上/下键召回历史 |
| **P5 界面复刻 zedis（2026-10-15，三批）** | **完成** | 对照 zedis 截图逐区复刻，**第一批（工具条与值区）**：**工具条单行 40px**（查询模式下拉收拢类型 / 标签 / TTL / 排序全部档位；「更多(…)」收拢平铺/树、回收站、重新扫描、数据/服务端切换）；**键树行**加斑马纹与虚线缩进辅助线；**值区键头部 40px**（收藏星 + 类型胶囊 + 大小 `raw` + 编码·空闲 + TTL 芯片 + 刷新 + 字符串编辑铅笔钮 + 解码九档下拉）；**JSONPath 栏**（输入 + 运行 + 「文本 | 树」切换，内核新增 `jsonpath.rs` 求值器：点/括号取子、通配、递归下降、下标（负数）、并集、切片，结果封顶 `JSON_TREE_MAX_NODES`，15 条单测；命中路径与树行身份同源）。**第二批（布局骨架对齐）**：键列表**默认命名空间树**；工具条从「横贯主区」改为**收进左侧键列顶部**（zedis 同位）；值面板默认宽 **640px**（`VALUE_PANE_DEFAULT`，范围 320–1000）；字符串值改**按需编辑**（默认只读，点铅笔才出现输入框 + 保存/取消，替代常驻表单）；JSON 文本档升级为**代码视图**（44px 行号 gutter + 每行词法着色：字符串绿 / 数字蓝 / 字面量紫，深浅两套，最多 4000 行超出截断并如实说明）；**状态栏去状态消息与类型构成**（zedis 无此两项；类型构成整块删除），DB 切换器 + 工具菜单（重新探测/解锁/断开）移到状态栏左端；**左栏去搜索框与底部「新建连接」按钮**（zedis 左栏只有标题 + 加号）。**命中定位（同日第三批）**：JSONPath 命中行**可点**——切到树档、沿树走一遍祖先链（`ancestor_chain`，按 `JsonTreeNode::path` 身份比对，不做字符串反解）全部展开、`UniformListScrollHandle::scroll_to_item(Center)` 滚动到该行（每帧 `.track_scroll` 挂回）、被定位行与命中行**双向高亮**（`nav_selected`）；编辑态先退出、树档不可用直接返回、命中节点因树预算未建出则不跳（树预算与命中封顶是两套口径，静默放过）；换键 / 换解码档 / 重新求值即清命中与高亮。**第四批（2026-10-15 晚，六项反馈）**：① 工具条前置按钮改**固定分类图标**（`SlidersHorizontal`，不再随档位跳图标），菜单 32 条平铺改 **5 个二级子菜单**（查询模式 / 类型 / 标签 / TTL / 排序，`PopupMenu::submenu_with_icon`，勾选态保留）；② 键树**默认全折叠**（内核 `AUTO_EXPAND_DEFAULT` 100→0，「未填=不展开」，连接表单可配正数），键行删**空图标槽**（间隔缩短 20px，zedis 叶子行同款无前导槽）；③ 键列表栏**默认收在最窄档**（`KEY_LIST_MIN_WIDTH` 330 / max 800，初始 = min，zedis 同款）；④ **值内容区整体换成 gpui-component `Editor`**（`EditorState`：行号 / 软换行 / tree-sitter 语法高亮；JSON 高亮零新增依赖——本仓 `tree-sitter-sql` feature 已隐含 `tree-sitter` + `tree-sitter-json`），**默认自动检测**：详情到位即按内容重置解码档（合法 JSON → JSON 档美化+高亮，`auto_decode`，zedis 的 Auto 语义），用户选档活到下次加载；⑤ 内容区**可选中可编辑**：`readonly(!can_write)`（外观不变、可选中复制、拒绝改动），用户直接在编辑器里改，改动经 `Change` 订阅与详情比对记 `value_modified`，头部按需出现「保存值 / 放弃修改」（替代铅笔 + 常驻 Input 编辑态，`string_editing` / `edit_buffer` 删除）；自绘 code_view（StyledText 行渲染）整体退役；⑥ **底栏三件套**（状态栏左段，仅数据视图 + 字符串值时显示）：「✓软换行」开关（超 `SOFT_WRAP_MAX_BYTES` 512KB 灰显）、格式标签（Code 图标 + `json`/`text`/`hex` 小写读数，非交互）、「视图：Auto/纯文本/Hex」下拉（映射解码档，与九档下拉同一真源）。刻意偏差：mono 字体未接 `font_family`（本仓无字体族基础设施，沿用 `typography.mono_md` 字号口径） |

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
   百万键下每次重绘都跑一遍会把滚动拖垮；`rebuild_views()`（键集 / 展开状态 / 树参数 /
   TTL 档 / 排序档变化时调用）**任何渲染代码都不许调它**。平铺列表同理：
   可见行存 `flat_rows`（`keys` 的下标序列），渲染只读。
5. **树的展开状态记「覆盖」而非全量**（`tree_overrides: HashMap<path, bool>`）。没被用户
   切过的节点按连接档案的「自动展开阈值」实时推导，这样改阈值能立刻反映到未碰过的节点上。
6. **折叠三角是独立可点元素**。若让整行既负责折叠又负责选中，「既有实体键又有子节点」的
   行（`user` 与 `user:1` 同时存在）就没法两者兼顾。
7. **零值语义**：TTL 输入 0 在服务端是「立即删除」，比「永不过期」危险得多——界面直接拒绝
   0 并指引用「移除过期时间」。连接档案里这两个数字档的口径是**「空 ⇒ `None`、填数 ⇒ `Some(n)`」**
   （`optional_num` 只把**空串**收成 `None`，`0` 是**有意义的数**）：
   `max_key_tree_depth` 未填用内核缺省 8；`auto_expand_threshold` 未填用**缺省 100**、
   填 `0` 表示**关掉自动展开**（见 §4.5）。⚠️ 早先本文写的「为 0 则归一成 `None`」是错的，
   且**留空与填 0 曾等价**——该等价在 2026-10-15 对齐 zedis 时已取消。

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
5. **值面板宽度不跨会话保留**（P3）：拖动后的宽度由 `ResizableState` 记在本次会话里，
   关闭应用回到出厂 460pt。要不要落 `windows` 表（几何）由使用体验决定，现在不落——
   少一个可写点，也就少一处「拖坏了不知道怎么回去」的口子。
6. **树档只在 JSON 解码档下开放**（P3）：文本档里恰好是合法 JSON 时**不给**树。
   换档结构清晰，代价是要多点一次「JSON」。若实测觉得绕，再评估是否让文本档也按
   内容自动识别（届时判据要写进 `json_available()` 一处，不许散在渲染里）。
7. **JSON 树长标量靠截断而非换行**（`JSON_TREE_PREVIEW_CHARS = 80`，行高固定 22pt）：
   保持行高统一才能用 `uniform_list` 虚拟化；要看全文切回文本档。
8. **状态栏探测不轮询**（P4）：刻意不做定时器，只在「连接 / 切库 / 点刷新」三个事件上
   跑（理由见 §4.8.2）。代价是内存与延迟会陈旧。若实测觉得数字不够「活」，再加一个
   **低频**（≥5s）定时器，并且只在窗口可见时跑。
9. **类型统计只覆盖已加载的键**（P4）：精确的全库类型计数要六七次 `SCAN TYPE` 全库遍历，
   与「不用 `KEYS`、不整读大集合」同一条取舍。若哪天要做，应该走**一次采样**
   （`SCAN COUNT=1000` 若干轮后按比例外推）并在界面上明确标「估算」。
10. **延迟是客户端观测到的往返**（P4）：含服务端执行 + 网络 + 客户端处理，走代理 / 跨地域
    时天然偏大，与服务端 `INFO latency` 记的内部延迟不是一回事。不打算改口径，但界面上
    写的是「延迟」而不是「服务端延迟」就是为了不让人误读。
11. **库切换是落盘的**（P4）：切库会改连接档案的 `db` 并写回 kv（理由见 §4.8.2）。也就是
    说它是「把这个连接的默认库改了」，不是「临时看一眼另一个库」。若用户希望后者，
    需要把 `db` 拆成「档案默认值」+「会话覆盖值」两处。
12. **键标注是「本机 + 本连接」的**（M1.6）：标签 / 备注 / 收藏存在本地 kv（`redis_key_tags`），
    **按连接 id 隔离**，不上服务端、也不跨机器同步（理由见 §4.8.3）。也就是说换一台电脑打开
    同一个库，标注是空的。这与 zedis 同构——它也是本地 `sled` 表。要跨机同步得另起一套存储
    方案，不做。
13. **标注只跟「显式删除 / 显式清空」走，不跟 SCAN 走**（M1.6）：翻页 / 重扫 / 改 MATCH 都不会
    动标注（否则会把未加载页的标注整片抹掉）。代价是「服务端上被别的客户端删掉的键，本地
    标注会留下孤儿」，直到用户显式删除该键或被过滤掉。**刻意不做 GC**——要清理就得全库遍历，
    正是硬纪律要避免的。若实测孤儿多到碍事，再评估「仅当用户点了『清理失效标注』时，
    对已加载键集做一次交集」（仍不遍历全库）。
14. **收藏不是标签的一个特例**（M1.6）：`favorites` 与 `entries` 是 **`KeyTags` 里的两个
    容器**，因此一个键可以「只收藏、无颜色、无备注」（理由见 §4.8.3）。`TagFilter::Favorites`
    与 `TagFilter::Tagged` 是**正交**的两档，不是「收藏 = 某个特殊色」。保持这条，别为了省一个
    `BTreeSet` 把收藏折进颜色枚举。
15. **备注自动保存，没有「保存」按钮**（M1.6）：`note_input` 订阅 `InputEvent::Change`，
    值变即落 `set_key_note`（理由见 §4.8.3）。好处是不丢草稿；代价是**没有「放弃修改」这一档**，
    且每次击键都会 `persist_tags`（经 `kv` 的写线程去抖，落盘频率有上限）。若实测觉得去抖不够，
    再给备注加一个**显式失焦时才落盘**的开关，但**不要**退回手动保存按钮。
16. **回收站的两道体积闸要按实测标定**（M1.6）：`TRASH_MAX_PAYLOAD = 1 MiB` 与
    `TRASH_MAX_VALUE_MEMORY = 4 MiB` 是照抄 zedis 的值，本仓没有实测数据支撑。
    1 MiB 那档偏保守（一条 900 KB 的 JSON 会话能进、1.1 MB 的进不去，体感上很随机）；
    真嫌窄就**只抬这一条**，别抬总预算（16 MiB）——后者封的是「一次删键重写多大一份 kv」。
17. **回收站没有「清空」按钮**（M1.6）：只能逐条「移除」，或等 24 小时 / 被预算驱逐。
    zedis 也没有。真要做，加一个 `TrashStore::clear(connection)`（6 行）+ 一个二次确认即可；
    现在不做是为了**少一个不可撤销的本地动作**——回收站里的东西已经是「删过一次」的了，
    再提供一个一键抹掉全部的口子，代价大于收益。
18. **恢复没有「覆盖」选项**（M1.6）：`RESTORE` 不带 `REPLACE`，同名键存在就跳过
    （理由见 §4.8.4）。若实测觉得「我知道它存在，我就是要覆盖」是常见诉求，再加一个显式
    的「覆盖恢复」按钮 + 逐条二次确认；**不要**把 `REPLACE` 变成默认——那会让恢复变成一个
    静默毁数据的动作。
