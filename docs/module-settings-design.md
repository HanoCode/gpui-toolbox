# Duck Toolbox 设置层「模块」分组与四模块偏好设计

> 状态：**P1 已落地**（2026-10-13）。承接 `docs/module-gap-analysis.md`。
> 用户诉求：设置里缺少**终端 / 数据库 / 邮箱 / 项目管理**四类模块的设置项。
> 本批范围：**框架（侧栏分组 + 模块页）+ 4 个模块共 18 个偏好字段**，其中后端早已存在、
> 只差入口的「接线项」是主力；**项目管理页不建**（后端为零，见 §7）。

---

## 1. 现状缺口

设置窗原本只有「通用 / 能力」两类页，四大模块**零设置项**。但偏好其实散落在代码里，
只是没有落盘通道，或写死在常量上：

| 模块 | 缺口证据（改前） |
| --- | --- |
| 终端 | 字号只存在内存（`terminal/` 的 `terminal_font_size` + `nudge_font_size`，**不落盘**，重启即回默认）；回滚行数写死 `10_000`（`duck-term` 的 `SpawnOptions::default()`），界面只有只读展示 |
| 终端·会话 | 断线自动重连的上限 / 起始退避写死在 `terminal_canvas.rs` 的 `RECONNECT_MAX_ATTEMPTS` / `RECONNECT_BACKOFF_START` |
| 数据库 | `QUERY_ROW_CAP = 10_000` / `PAGE_SIZE = 200` / `HISTORY_LIMIT = 50` 三个常量写死（`db.rs`）；新建连接的表单每次都要手填只读档 / 超时 / 环境标签 |
| 邮箱 | `PREVIEW_LIMIT = 300` 写死（`mail.rs`）；新邮件是否出声、默认正文形态（HTML 还是纯文本）都无入口 |
| Redis | `SCAN_PAGE = 500` 写死（`redis.rs`），只有连接档案里能按连接覆盖 |

**已经够好的部分不做重复入口**：连接级配置（主机、端口、认证、库名、TLS CA、SSH 隧道、
按连接的超时与环境标签）今天都在**连接档案行**里，继续留在各自的连接编辑器。

## 2. 三条硬口径：**不给开关**

设置页只放「偏好」，**不放安全闸门**。下列三条**不出现在设置里**，就是不给关：

| 闸门 | 位置 | 为什么不给开关 |
| --- | --- | --- |
| 危险 SQL 二次确认（删库 / 全表清改） | `db.rs:3348`「危险语句先确认」→ `db.rs:3352`「执行危险 SQL」 | 无条件介入；关掉它 = 把不可逆操作变成一次误点 |
| Redis 破坏性命令确认（`FLUSHALL` / `CONFIG SET` 等） | `redis.rs:569` `enum PendingAction`，由 `redis.rs:338/417` 的 `require_confirm_writes` 驱动 | 该开关**按连接**给（生产档案默认开、开发档可关），属于连接档案语义，不是全局偏好 |
| SSH 主机密钥校验 | `duck-ssh/src/known_hosts.rs`（fail-closed，按用户自己的 `known_hosts` 与 `StrictHostKeyChecking` 执行） | 应用**既不写 `known_hosts`、也不改用户策略**（见 `terminal_canvas.rs:915`）——多一个「忽略校验」的开关就是多一条被钓鱼的路 |

## 3. 侧栏分组

侧栏由「平铺 7 条」改为**四组**（`settings.rs` 的 `enum PageGroup`）：

| 组 | 成员 |
| --- | --- |
| 通用 | 外观、行为、启动 |
| 能力 | 插件、AI、通知 |
| **模块**（本批新增） | **终端、数据库、邮箱、Redis** |
| 系统 | 更新、隐私、关于 |

**实现要点**：组标题**不塞进列表**，而是用 `meta_text` 画在列表之上，每组一个
`nav_list`（`id = settings-nav-{group.key()}`、`selected` 用组内下标）。
理由：`duck-components/src/kit.rs` 明确「导航列表不适合承载非导航行」，所以**不动组件层**，
在页面这一层拼装。空组自动跳过（某组的页面被隐藏时不留孤零零的标题）。

> ⚠️ `palette_pages()` / `palette_page_key()` 仍按 `PAGES` 全量导出，命令面板**不会**因为分组而漏页。

## 4. 四个模块页

每页都是「一个单键 JSON」，键名与结构体一一对应。**所有字段都是 `Option` 或带
`#[serde(default)]`**——旧版本写的 JSON 少了字段也能读，读不出来就整段回默认。

### 4.1 终端 —— `terminal.settings` → `TerminalSettings`

| 字段 | 类型 | 默认 | 界面 |
| --- | --- | --- | --- |
| `font_size` | `Option<u32>` | `None` = 跟随主题 | 下拉：跟随主题 / 11~20 |
| `font_family` | `Option<String>` | `None` = 主题等宽字体 | 文本输入 |
| `line_height_percent` | `Option<u32>` | `None` = 主题行高比 | 数字输入 |
| `padding` | `Option<u32>` | `None` = 主题常规留白 | 数字输入 |
| `scrollback` | `u32` | `SpawnOptions::default().scrollback`（10 000） | 下拉：1 000 / 10 000 / 50 000 / 100 000 |
| `auto_reconnect` | `bool` | `true` | 开关 |
| `reconnect_max_attempts` | `u32` | `RECONNECT_MAX_ATTEMPTS` | 数字输入 |
| `reconnect_backoff_secs` | `u32` | `RECONNECT_BACKOFF_START`（秒） | 数字输入 |

**接线点**：`terminal/helpers.rs::initial_font_size`（夹在 `FONT_SIZE_RANGE` 10~20 内）、
`terminal_canvas.rs` 的三处 `spawn_local` / `respawn` 与 `ssh_options(..)`（新增 `scrollback` 参数）、
`sync_cell_metrics` + `render_terminal_session`（字体族 / 行高 / 留白），
以及状态栏的回滚读数。

**重连策略**收进 `ReconnectPolicy { max_attempts, backoff_start }`（`terminal_canvas.rs`），
`poll_sessions` 每轮读一次设置：`AutoReconnect::new(policy)`、`on_disconnected(.., policy)`。
关掉「自动重连」时把它折进 `blocked`，与「主机密钥 / 认证失败不重试」走**同一个落点**
（`ReconnectState::Stopped`），不新增状态。
`RECONNECT_BACKOFF_MAX`（60 s）**保持常量**——它是护栏（防止用户把退避调成小时级），不是偏好。

### 4.2 数据库 —— `db.settings` → `DbSettings`

| 字段 | 类型 | 默认 | 界面 |
| --- | --- | --- | --- |
| `row_cap` | `u32` | `QUERY_ROW_CAP`（10 000） | 「结果行数上限」 |
| `page_size` | `u32` | `PAGE_SIZE`（200） | 「分页每页行数」 |
| `history_limit` | `u32` | `HISTORY_LIMIT`（50） | 「历史条数」 |
| `new_read_only` | `bool` | `false` | 「新连接默认只读」开关 |
| `new_timeout_secs` | `Option<u32>` | `None` = 跟随引擎 | 「新连接默认超时」 |
| `new_env_tag` | `Option<String>` | `None` = 无标签 | 下拉（`EnvTag::ALL` 的 wire 值） |

**接线点**：三个 helper `query_row_cap(cx)` / `browse_page_size(cx)` / `history_limit(cx)`
（`db.rs`），**全部常量使用点已收敛到它们**——首屏 `execute`、`load_more_query`、
表浏览 `load_table_page`、分页器 `render_pager`、`push_history`。
`EditFormView::new` 用 `db_prefs(cx)` 给新连接表单播种；`from_config` 改为**无条件**写 timeout
（`None` 时写空串），否则「编辑老连接」会莫名继承播种值。

### 4.3 邮箱 —— `mail.settings` → `MailSettings`

| 字段 | 类型 | 默认 | 界面 |
| --- | --- | --- | --- |
| `notify_new_mail` | `bool` | `true` | 「新邮件通知」开关 |
| `prefer_html` | `bool` | `false`（纯文本优先，与既有 `render_body` 口径一致） | 「默认正文形态」 |
| `preview_limit` | `u32` | `PREVIEW_LIMIT`（300） | 「列表首屏条数」 |

**接线点**：`preview_limit(cx)` 落到三处 `core.search_messages` / `messages` /
`messages_by_label`；`notify_new_mail` 作为 `notify_new_mail()` 的**第一道闸门**（关掉直接 return，
连历史都不记）；`set_detail` 用 `prefer_html` 决定初始正文形态。

### 4.4 Redis —— `redis.settings` → `RedisSettings`

| 字段 | 类型 | 默认 | 界面 |
| --- | --- | --- | --- |
| `scan_page` | `u32` | `SCAN_PAGE`（500） | 「扫描页大小」 |

**接线点**：`redis.rs::scan_page_size` 回落——连接档案的 `scan_count` **优先**，
没设才看全局偏好。

## 5. 落盘 / 读取约定

- **结构体 → `serde_json` → 单个 kv 键**（沿用项目既有口径，不拆成多键）。
- 泛型 `load::<T>(cx, key)` / `save::<T>(cx, key, &value)` 收口读写；
  `save` 走 `kv_put` → `duck_store::flush` → `refresh_windows`——**三步缺一不可**，
  否则 400 ms 去抖期内窗口看不到新值（模式来自 `data.rs::set_flag`）。
- 默认值**一律回落到既有常量**（`SpawnOptions::default().scrollback`、`QUERY_ROW_CAP`、
  `PREVIEW_LIMIT`…），**不复制字面量**，避免制造第二处真源。
- 页面里每行都是「展示当前值 + 编辑」，编辑走既有的 `open_number_prompt` /
  `open_text_prompt`（`status_support.rs`），不新造弹窗。

## 6. i18n

新增文案按 `AGENTS.md §3.3` 登记进 `duck-theme/src/i18n.rs` 的 `static EN`，
**按 UTF-8 字节序插入**（表是二分查找，乱序会静默查不到）。本批 EN 表增至 **1 944 条**，
`table_is_sorted` + `every_chinese_literal_is_translated` 双守卫通过。

> ⚠️ **守卫盲区（本批发现）**：`contains_cjk` 只认 `\u{4e00}..=\u{9fff}` 基本汉字区，
> **全角标点（U+FF00–FFEF）、扩展 A 区、兼容汉字区都不在内**。
> 因此 `"{}：{}"` 这类「纯标点中文串」能**逃过守卫**却不会被翻译。
> 这类串在英文界面下会露出全角标点，复核时要用更宽的字符范围自查。

## 7. 未做与阻塞

| 项 | 状态 | 原因 |
| --- | --- | --- |
| SSH keepalive（`ServerAliveInterval`） | **阻塞** | 要改 `duck-ssh` 的 `Target::base_argv` / `scp_base_argv`，而它们被 `duck-ssh/src/remote.rs`（SFTP）**直接调用**，没有应用层参数通道。本批已把该字段与「连接」段**整个撤下**，等 `duck-ssh` 不再被别的会话占用时再做 |
| 项目管理设置页 | **不建** | `app.rs:709` 的 `projects` 仍是 `PlaceholderPage`（`app.rs:804`），后端为零。按「演示项判据」：后端没有就不摆控件 |
| 邮箱签名 / 定时同步 | 未做 | `duck-mail` 无签名后端；定时同步没有调度器 |
| 终端渲染个性化（字体族 / 行高 / 留白） | **已做** | 见 §4.1（原计划列在 P2，随本批一起接线） |
| 数据库连接编辑器的 TLS CA 入口 | 未做 | 属连接编辑器范围，与「设置」分层无关 |

## 8. P1 门禁（2026-10-13）

| 门禁 | 结果 |
| --- | --- |
| `rustfmt --check`（9 个改动文件） | clean |
| `cargo test -p duck-theme` | 24 / 0 / 0（含 i18n 双守卫） |
| `cargo check -p duck-app --locked` | 绿（33.79 s；仅第三方 `block v0.1.6` 的 future-incompat 提示） |
| `cargo test -p duck-app --locked` | **192 单测 + 29 集成 = 0 failed**（1 个既有 `#[ignore]`），连续两次一致 |
| `cargo clippy -p duck-app --all-targets` | 0 error；新增文件 `module_settings.rs` **0 告警** |

> 过程中自查出并修掉一个真实缺陷：给 `ssh_options(..)` 加 `scrollback` 参数时**漏改了 3 处测试调用点**。
> `cargo check -p duck-app` 默认**不编测试目标**，所以只有 `test` / `--all-targets` 能抓到——
> 这也是 `AGENTS.md §5` 要求 `clippy --all-targets` 的原因。
