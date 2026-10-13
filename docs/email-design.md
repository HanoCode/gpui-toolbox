# Duck Toolbox 邮箱模块架构设计（duck-mail）

> 状态：**设计稿（待评审）** · 撰写日期 2026-10-12 · 前置阅读 `AGENTS.md` §1/§3
> 相关文档：`docs/redis-design.md`（同类模块的接入范式）· `docs/database-design.md` §4.1（异步桥接裁决）· `docs/ai-design.md` §4.1（流式输出纪律）
>
> **一句话结论**：以 `temp/mail`（Flectar Mail）的 **`flectar-mail-core`（72,834 行、零 UI 的独立内核）为主体移植为 `duck-mail`**，协议层换成同名现成 crate（`async-imap` / `lettre` / `mail-parser` / `mail-builder`），**HTML 正文用本仓已在依赖图内的 `gpui-base::text::html` 原生渲染**（无需 WebView / Blitz），AI 能力**复用本仓 `duck-ai`**，前置做一次 **rusqlite 全仓升级到 0.40**。

---

## 1. 目标与范围

### 1.1 为什么是独立路由 + 独立内核 crate

左侧活动栏已挂 `email` 入口（`EMAIL_ROUTE` / `⌘8`，当前是 `PlaceholderPage`，见 `crates/duck-app/src/app.rs:783`）。本设计把它替换为真实模块。

不把邮箱塞进任何现有 crate，理由与 `duck-redis`（`docs/redis-design.md` §1.1）同构：

1. **协议栈完全不同**：IMAP/SMTP/JMAP 与数据库驱动、终端 PTY、AI HTTP 无任何共享代码。
2. **存储是独立库**：邮箱要一个几十张表的 SQLite 库（见 §4.5），与 `duck-store` 的 kv、`duck-ai` 的会话库、`duck-db` 的用户库都不是一回事。
3. **生命周期不同**：邮箱有常驻后台同步（IDLE 长连接）与定时任务（snooze / send-later），不能挂在页面生命周期上。
4. **内核零 GPUI**（AGENTS.md §1 分层边界）：内核可被 CLI / 未来的独立窗口 / 插件宿主复用。

### 1.2 能力矩阵

| 域 | 能力 | 里程碑 |
| --- | --- | --- |
| 账户 | IMAP/SMTP 账号（自动发现、连通测试）、OAuth2（Gmail / Outlook）、多账号 | M1 / M2 |
| 同步 | 首次全量、增量（UID 水位 + flag 对账）、IDLE 推送、断线重连、失败重试 | M1 |
| 阅读 | 线程列表、阅读器、纯文/HTML 正文、附件、内联图（CID）、引用折叠 | M1 |
| 检索 | 条件查询语言、FTS5 全文、字段过滤、跨账号 | M1 |
| 写信 | 纯文本撰写 + **附件** + **本地草稿**（均已落地）；富文本、草稿*自动*保存、签名、发件身份 | M2 |
| 整理 | 已读/旗标/移动/删除、标签、文件夹管理、批量操作、撤销 | M2 |
| 智能 | 线程摘要、智能回复、起草、校对、意图识别、RAG 问答、自动分类/打标、AI 用量统计 | M3 |
| 自动化 | 规则路由（发件域/关键词）、snooze、定时发送、退订识别 | M2 / M3 |
| 通讯录 | 联系人学习、CardDAV | M3 / M4 |
| 日历 | CalDAV / Google Calendar、会议邀请解析 | M4 |
| 安全 | 凭据加密落程序自有存储、远程内容拦截、HTML 净化、DKIM 显示、OpenPGP | M1 / M4 |
| 生态 | 暴露给 JS 插件（`duck.fetch` 之外新增邮件能力位） | M4 |

> ⚠️ **落地状态订正（2026-10-12 复核，逐条核到源码）**：下表把 M1 声称已落地、但代码里**没有**的项如实降级到 M2/M3。判据是「有没有可执行的代码路径」，不是「设计里写了」。
>
> | 声称 | 实际 | 证据 |
> | --- | --- | --- |
> | 同步：IDLE 推送 / 断线重连 / 失败重试（M1） | **降 M2**。同步是**一次性全量 + 增量函数，不常驻**；`proto/imap.rs` 无 `idle()` | `sync.rs:5-11` 自述；`proto/imap.rs` 只有 `connect`/`list_folders`/`select`/`fetch_envelopes`/`fetch_raw`/`fetch_flags`/`search_all_uids`/`noop`/`logout` |
> | 阅读：附件下载（M1） | ~~**降 M2**。只有附件**清单**，落库时 `file_path: None` / `imap_section: None`，无下载函数~~ **已落地（2026-10-13）**：`MailCore::download_attachment(id, dest)` + 阅读器每行的「另存为…」（`duck_platform::save_file`）。⚠️ 走的是**整封取回 + MIME 解码**而不是按段 `FETCH`（理由与代价见 §4.13）；`imap_section` 仍留空 | `sync.rs::download_attachment`；`db/repo/messages.rs::attachment_target` |
> | 阅读：引用折叠（M1） | **降 M2**。渲染分支只有 Plain / Html | `mail.rs:2439-2513` |
> | 检索：`fold()` 去音标 + stalwart `nlp` 中文分词（M1） | **降 M3**。中文按整段切、召回很差；代码注释自己承认 | `db/repo/search.rs:10-15`；`fold` 只在虚表 tokenizer 里 `remove_diacritics 2`（`001_init.sql:401`） |
> | 检索：条件查询语言 / 字段过滤（M1） | ~~**未做**。`to_match_expression` 只做「每 token 加引号 + 隐式 AND」~~ **已落地（2026-10-13）**：`search.rs` 重写为「词法 → `Query { text, filters }` → `MATCH` + `WHERE`」两层，认 `from:` / `to:` / `subject:` / `folder:` / `is:` / `has:` / `before:`(`older:`) / `after:`(`newer:`)、引号短语与安全前缀；`LIKE` 全量转义（`ESCAPE '\'`）；空查询不退化成全表扫描。界面侧加了「搜索语法」速查弹窗。**细节见 §4.12**（含「为什么不做 `-词` / `OR`」） | `db/repo/search.rs`；`mail.rs::SEARCH_SYNTAX_ROWS` |
> | 账户：自动发现（M1） | **未做**。`validate` 只做格式校验，无 autoconfig / MX 探测 | `accounts.rs:63` |
> | 列表行「旗标」标记（§4.10） | ~~**未做**。列表行只画未读圆点 + 回形针；`MailCore` 无 `set_starred`，无切换入口~~ **已解决（2026-10-13）**：`MailCore::set_starred` 与工具条「旗标」按钮早已落地（走动作队列、可撤销），列表行现在也画旗标星（`message_row`，琥珀色 `warning`）。可访问名一并覆盖「未读 × 已加旗标」四种组合 | `mail.rs::message_row`；`messages.is_starred` |
>
> **另有两处文档失准**：① `db/migrations/files_001.sql` / `files_002.sql` 是**游离文件**——`migrations.rs:15-28` 的 `MIGRATIONS` 只含 001–012，二者未被 `include_str!` 引用，且内容是 `connections/spaces/nodes`（与邮箱附件无关）。⚠️ **不得纳入 `MIGRATIONS`**（会改 `user_version` 语义，`migrations.rs:7-8` 明令只可追加）。② §3.2 与 §4.5 写的「24 个 repo」实际只有 **4 个**（`accounts` / `folders` / `messages` / `search`）。
>
> **好消息**：M2 需要的库表**一张不缺且列够用**（`001_init.sql` + 007/008/010/012）——`pending_actions`（:190，5 态含 `cancelled`）、`snoozes`（:208）、`split_rules`（:225）、`route_cache`（:233）、`drafts_meta`（:238）、`draft_attachments`（:245）、`labels`（007:44 重写后含 `scope`/`owner_account_id`/`origin`/`is_auto`）、`message_labels`（007:154）、`sender_identities`（008:1 / 012:6）、`threads`（:43）、`attachments`（:136）、`app_settings`（:254）。**缺口 100% 在 repo 层与内核业务层**。
>
> ⚠️ **`proto/imap.rs` 整个写方向一条都没有**（无 `STORE` / `COPY` / `MOVE` / `EXPUNGE` / `APPEND` / `IDLE`）——这是 M2 的第一块必补。
>
> ✅ **发信已落地（2026-10-13 复核）**：「写信」这一行从「M2 计划」变成「**纯文本发信 + 附件可用**」——详见 §6 M2 的落地表。**富文本、签名、发件身份选择、写回「已发送」文件夹仍未落地**，逐条列在那里。「整理」一行的动作队列已落地（另修掉一处并发竞态，见 §4.11 末）。
>
> ✅ **本地草稿随后落地（2026-10-13，§4.15）**：`drafts_meta` / `draft_attachments` 从「表在、repo 无」变成可用——撰写弹窗「存草稿」、草稿箱置顶本地草稿 + 点行回填（含附件）、行内删除、发信成功顺手删草稿。⚠️「草稿*自动*保存」（关弹窗 / 停输入时去抖落库）**仍未做**，只有显式按钮；把草稿推到服务器 `\Drafts`（`IMAP APPEND`）也没做。第 54 行那份「缺口 100% 在 repo 层」的清单里，「草稿」这一项可以划掉了。

### 1.3 非目标（明确不做）

- **不做邮件服务端**（不实现 SMTP/IMAP 服务端、不做投递代理）。`temp/stalwart` 是完整服务端（见 §2.4），本次复核后它的角色是「**抠部件 + 当测试服务器**」，不改变这条边界。
- **不做 POP3**：三个参考实现都没有（Flectar 无、melib 无）；POP3 与"多设备同步"语义相悖，只列 M4 的可选项。
- **不做 Exchange/EWS**：走 Microsoft Graph（M4），与 Flectar 的 `graph.rs` 同路线。
- **不引入 WebView**：见 §4.8——本仓已有原生 HTML 渲染，无必要承担 `wry`/WebKit 的体积与进程模型成本。

---

## 2. 参考项目评估

`temp/` 下与本模块相关的共有 5 个邮件项目 + 1 个 UI 参照，另有若干现成 crate 本身就是"可搬的积木"。

### 2.1 六源分工（不是单一项目照搬）

| 来源 | 形态 | 定位 | 结论 |
| --- | --- | --- | --- |
| **`temp/mail`**（Flectar Mail） | 完整原生邮件客户端：Rust + Slint + Blitz，`flectar-mail-core` **72,834 行**零 UI 内核 | **主体蓝本** | **整块移植**为 `duck-mail`（见 §2.2 的三条阻隔与替代） |
| **`temp/meli`**（meli/melib） | 终端邮件客户端 + 邮件库（melib 0.8.13） | 架构参照 | **只借架构，不搬代码**（MSRV 1.88 超线 + alpha 版协议 crate） |
| **`temp/lettre`** | SMTP 发信库 0.11.23 | 直接依赖 | **直接使用**（发信）；同时是 Flectar 的发信实现 |
| **`temp/rustmailer`** | poem + gRPC 的自托管邮件中间件（服务端二进制） | 同步引擎思路 | **只借思路**（与 poem、全局单例强耦合，不可剥离） |
| **`temp/stalwart`** | **完整邮件服务端** v0.16.26（IMAP/JMAP/SMTP/POP3/ManageSieve/DAV/SCIM，28 个 crate 约 33.9 万行，edition 2024） | 部件库 + 测试服务器 | **只抠 3 样**：`nlp`（CJK 分词 + 命中摘要，§2.4）、LLM 结构化输出解析法、Docker 测试服务器；协议层证实 `async-imap` 选型（§2.4） |
| **`temp/tusk`**（旁证） | GPUI 数据库客户端（三栏 + AI 面板），`gpui-kit 0.6` | UI 参照 | 三栏布局与 AI 面板的 GPUI 参照（与本仓 `gpui-component 0.7.1` 同族） |

### 2.2 逐项取舍总表

| 能力 | Flectar（主蓝本） | melib（参照） | 本设计取谁 | 理由 |
| --- | --- | --- | --- | --- |
| **IMAP** | `async-imap 0.11.3`（`src/imap/mod.rs`，1,597 行） | 自研 `imap-codec 2.0.0-alpha.6` | **Flectar 路线** | 自研 IMAP 是最大风险源；melib 用的是 alpha 依赖 |
| **SMTP** | `lettre 0.11.23`（`src/smtp/`，371 行） | 自研 `SmtpConnection` | **lettre** | 两侧都指向 lettre 或等价物；lettre 是事实标准 |
| **JMAP** | `jmap-client 0.4.2`（`src/jmap/`，4,048 行） | 自研 | **Flectar 路线**（M2，feature 门控） | — |
| **Gmail API** | 自研 REST（`sync/gmail.rs`，156KB） | — | **Flectar 路线**（M2） | — |
| **MIME 解析** | `mail-parser 0.11.8` + `full_encoding`（`src/mime/`，2,987 行） | 自研 nom 解析器（`email/parser.rs`） | **mail-parser** | `full_encoding` 才有 `encoding_rs`，能正确解码 GB2312/GBK/Big5/Shift-JIS；自研解析器不可取 |
| **MIME 构造** | `mail-builder 0.4.4` | `email/compose/mime.rs` | **mail-builder** | — |
| **存储** | `rusqlite 0.40.2`（bundled）+ FTS5 + 向量表（`src/db/`，15,283 行，12 个迁移） | `rusqlite ^0.37`（可选 feature） | **Flectar schema 原样复用**，rusqlite 版本见 §4.3 | 12 个迁移是**纯 SQL**，与 rusqlite 版本无关 |
| **同步引擎** | per-account actor（`sync/engine.rs`，155KB） | `SyncPolicy::{Basic, Condstore, CondstoreQresync}` | **Flectar 路线**，分层采纳 melib 的"策略分级" | 见 §4.6 |
| **凭据** | `keyring 3`（`accounts/credentials.rs:57` `CredentialStore` trait） | 无 keyring（`Secret` 走 shell 命令） | **本仓 `duck-platform::{vault,secret}`**（AES-256-GCM 密文，落程序自有文件 / SQLite） | **2026-10-12 起本仓不再使用 keyring**；OAuth token 同源，密文进账号档案 |
| **OAuth2** | PKCE + loopback（`oauth/`，2,577 行） | — | **Flectar 路线**，把"打开浏览器"抽成宿主回调 | 保持内核零 GPUI |
| **HTML 渲染** | Blitz（自研渲染引擎） | 无（外挂 mailcap） | **本仓 `gpui-base` 原生 HTML 渲染** | **重大简化**，见 §4.8 |
| **AI** | 自研 `ai/mod.rs`（68KB）：摘要/回复/起草/校对/RAG/工具调用/嵌入 | — | **prompt 层移植 + 调用层换 `duck-ai`** | 本仓已有 AI 内核，不重复造 |
| **异步运行时** | tokio 全栈 | smol + futures | **内嵌 tokio runtime**（封在 crate 内） | 见 §4.1 |

### 2.3 三条硬阻隔与替代路径

移植 `flectar-mail-core` 会遇到三条真实阻隔，逐条给替代路径（这正是"可搬架构、需改边角"的部分）：

**阻隔 1 — rusqlite 版本冲突（硬性 cargo 错误）**
`flectar-mail-core` 用 `rusqlite 0.40.2` → `libsqlite3-sys ^0.38.2`；本仓用 `rusqlite 0.32.1` → `libsqlite3-sys ^0.30.1`。
两者都声明 `links = "sqlite3"`，**cargo 禁止同一依赖图中存在两个 `links` 相同的原生库**（会报 `multiple packages link to native library sqlite3`），因此**必须统一版本**。裁决见 §4.3。

**阻隔 2 — Slint / Blitz 前端（仅影响 UI 层，不影响内核）**
`flectar-mail-core` 本身**零 UI**，不依赖 Slint/Blitz；Slint 只在根 `src/`（控制器 + 视图模型）与 `ui/`。故内核可整块搬，UI 需按 GPUI 重写（§4.10）。注意 `Cargo.toml` 的 `[patch.crates-io]` 里 `blitz-dom`/`blitz-paint`/`parley` 是根包的，**core 不涉及**。

**阻隔 3 — 前端宿主改动面（Slint 特有的东西内核里没有）**
Flectar 的 HTML 渲染、`mailcap` 外挂、桌面通知、主题都在宿主侧。本仓各自有对应物：`gpui-base::text::html`（HTML）、`duck_components::notify`（通知）、`duck_theme`（主题）、`TextView`（正文）。**这些是替换点，不是移植障碍。**

> **许可证事实**（按你要求不作为约束，仅记录）：`flectar-mail-core` 为 `AGPL-3.0-only`，根包 `AGPL-3.0-only`；`melib` 为 `EUPL-1.2 OR GPL-3.0-or-later`；`lettre` 为 `MIT`；`jmap-client` / `mail-builder` / `mail-parser` 为 `Apache-2.0 OR MIT`；`async-imap` 为 `MIT OR Apache-2.0`。

### 2.4 stalwart 复核（v0.16.26，2026-10-12 二次拉取后）

首次评估时 `temp/stalwart` 是空仓库；现已拉全（tag `v0.16.26`，69 MB，28 个 workspace 成员约 **33.9 万行**，edition 2024）。它是**服务端**：IMAP4rev1/rev2、JMAP（RFC 8620/8621 + Sieve/日历/联系人/文件草案）、SMTP、POP3、ManageSieve、CalDAV/CardDAV/WebDAV、SCIM，外加自带的 spam-filter 与 NLP。逐 crate 行数（`wc -l` 实测前八）：`registry` 87,411 · `common` 37,871 · `jmap` 33,597 · `store` 24,822 · `smtp` 19,954 · `jmap-proto` 16,720 · `dav` 14,949 · `trc` 13,766；`imap-proto` 11,665 · **`nlp` 10,379** · `imap` 9,468 · `spam-filter` 8,833。

**结论先行：服务端主体（jmap/imap/smtp/store/common/registry）一行都不搬**——它们是「守连接、跑投递、管存储」的引擎，与客户端需求镜像相反；但里面有四样东西值得拿走。

**① `crates/nlp`（10,379 行）——真正可整块搬走的资产**

可移植性实测（这条最重要）：
- `crates/nlp/Cargo.toml` **零 `path` 依赖**——不依赖 `types`/`trc`/`utils`/`registry` 中任何一个；
- 全部非测试代码**不引用**任何 stalwart 内部 crate：`rg "trc::|types::|utils::|registry::"` 在 `crates/nlp/src/` 只命中一处 `tokenizers/stream.rs:80` 的 `types::{TokenType, TypesTokenizer}`——它在 `#[cfg(test)]` 测试模块里，且指向的是 crate 自己的 `tokenizers/types.rs`（`tokenizers/types.rs:1`），不是服务端的 `types` crate；
- **因此：整目录拷进 `duck-mail/src/nlp/`（或独立 `duck-mail-nlp`），只改 `Cargo.toml` 即可编译。**

内容与我们的对应关系：

| 部件 | 证据 | 对 duck-mail 的价值 |
| --- | --- | --- |
| 中文分词（jieba-rs 0.10，全局 `LazyLock<Jieba>`） | `tokenizers/chinese.rs:9,13` | **FTS5 对中文按整句分词的痛点**：SQLite 默认 unicode61 不分中文词。M1 检索要用它产 token 喂 FTS5 |
| 日文分词 + 空格/词元/流式分词 | `tokenizers/{japanese,space,word,stream}.rs` | 同上，覆盖 CJK |
| 语言检测 + 词干 + 停用词 | `language/{detect,stemmer,stopwords}.rs`（whatlang 0.18 + rust-stemmers 1.2） | 查询时按语言选词干器 |
| **命中摘要** `generate_snippet(text, needles, language, is_exact) -> Option<String>`（`Term{offset,len}` 按 token 偏移回填） | `language/search_snippet.rs:31-36` | 搜索结果里「…命中词高亮片段…」，正是 FTS5 `snippet()` 对中文做不好的那部分 |
| 在线学习分类器（FTRL/Adam/SGD + reservoir 采样） | `classifier/{ftrl,adam,sgd,reservoir}.rs` | M3 自动分类的**本地零 token 备选**（先启发式 → 再本地分类器 → 最后才上调 LLM） |

依赖与 MSRV（crates.io API 实查，2026-10-12）：`rand 0.10.2` 声明 MSRV **恰为 1.85**（卡线过）· `rkyv 0.8.18` 1.81 · `whatlang/jieba-rs/rust-stemmers/hashify/xxhash-rust` 未声明。新增外部依赖约 9 个，**分两步引**：M1 只搬 `language/` + `tokenizers/`（jieba-rs、whatlang、rust-stemmers、xxhash-rust 等，无 rkyv 硬需求），M3 再搬 `classifier/`（引入 rkyv）。许可证为 `AGPL-3.0-only OR LicenseRef-SEL` 双许可（按你的要求不作为约束，记录备查）。

**② 协议层「反向印证」——不搬，但验证了选型**

- `imap-proto` 是**纯服务端视角**：`parser/` 解析客户端发来的 `Command`（`parser/mod.rs` 显式 `use crate::{Command, …, receiver::CommandParser}`），`protocol/` 里的 `ResponseCode` 是给服务端**序列化响应**用的（`protocol/mod.rs:481` 起）。客户端需要的是镜像方向（序列化命令、解析响应），所以**不能搬**——同时证实了 §4.4 选 `async-imap`（自带客户端方向的 `imap-proto`）是对的。
- `jmap-proto`（16,720 行）**硬耦合**内部 crate（`Cargo.toml` path 依赖 `utils/types/trc/registry`，其中 `registry` 87k 行是配置注册表巨物）→ 不可剥离；JMAP 客户端继续用 Flectar 同款 `jmap-client`（§4.4），M2 落地时可用 stalwart 的 `jmap/`（33.6k 行，RFC 8621 参考实现）**当对照答案**查协议细节。
- `dav-proto` 的 CalDAV/CardDAV 类型层用的是 **crates.io 公开包 `calcard 0.3`**（`dav-proto/Cargo.toml`）→ M4 做通讯录/日历时直接依赖 `calcard`，不必碰 dav-proto。

**③ LLM 结构化输出解析法（抄思路，20 行）**

`spam-filter/src/analysis/llm.rs`（112 行，LicenseRef-SEL）的做法：让模型按**固定分隔符**输出 `类别<sep>置信度<sep>解释`，再用 `config.index_category / index_confidence / index_explanation` 三个**索引**定位字段（而非 JSON）。好处是抗模型输出漂移——模型多打一个引号不会崩，只是某字段为空。`duck-mail::ai` 的自动分类/意图识别输出解析照这个模式写（`ai_usage_events` 记录 + 分类落 `labels`）。另见 `common/src/scripts/plugins/llm_prompt.rs`（86 行，AGPL OR SEL）的脚本化 prompt 插件形态。`spam-filter` 的 23 个分析模块（`analysis/{headers,url,ip,dmarc,from,…}.rs`）整体依赖 `Server`/`store`（`lib.rs:24`），**只借「头部分析特征清单」的设计，不搬代码**。

**④ 本地测试服务器（开发基建，不进依赖图）**

`Dockerfile`（cargo-chef 多阶段）+ 官方镜像可一键起一个**真 JMAP + IMAP + SMTP + ManageSieve** 服务器（HTTP/JMAP 8080、IMAP/POP3/Sieve 443、SMTP 25；`STALWART_RECOVERY_ADMIN` 注入初始管理员）。这解决了「手头没有 JMAP 服务器可测」的问题：M2 的 `feature = "jmap"` 与 Sieve 过滤器编辑都在它上面联调。`tests/resources/` 里有现成的 `imap/`、`jmap/`、`smtp/` 夹具与 `tls_cert.pem` 自签证书，可直接给客户端的 TLS/协议测试复用。

**可复用性总表**

| 部件 | 行数 | 复用方式 | 价值 | 理由 |
| --- | --- | --- | --- | --- |
| `crates/nlp` 的 language + tokenizers | ~6k | 整目录拷贝 + 改 Cargo.toml（实测零内部耦合） | **高** | 中文分词 + 命中摘要，补 FTS5 的中文短板 |
| `crates/nlp` 的 classifier | ~2k | 同上（M3，引入 rkyv） | 中 | 本地零 token 自动分类备选 |
| LLM 分隔符解析法 | 20 行 | 抄思路 | 中 | 比 JSON 稳，直接用于自动分类 |
| Docker 测试服务器 | — | 开发基建 | **高** | M2 JMAP/Sieve 的联调环境 |
| `imap-proto` | 11.7k | 不搬 | — | 服务端视角，反向证实 async-imap |
| `jmap-proto` / `jmap` | 50k | 不搬（jmap 参考实现可查） | 低 | registry 硬耦合 |
| spam-filter | 8.8k | 只借特征清单设计 | 低 | 依赖 Server/store，31 文件 SEL 头 |

---

## 3. 总体架构与 crate 布局

### 3.1 crate 图

```
                      ┌──────────────────────────────────────────┐
                      │  duck-app（可执行装配 + 页面）            │
                      │   pages/mail.rs（三栏）                   │
                      │   pages/mail_dialogs.rs（向导/撰写/设置） │
                      └───────┬───────────────────────┬──────────┘
                              │                       │
              ┌───────────────▼──────────┐   ┌────────▼─────────────┐
              │ duck-mail（内核，零 GPUI）│   │ duck-ai（已有）       │
              │  协议 / 同步 / 存储 / 检索 │──▶│ provider/sse/tool/kb │
              │  / 规则 / 邮件 AI prompt  │   └──────────────────────┘
              └───┬──────────┬───────────┘
                  │          │
      ┌───────────▼──┐  ┌────▼────────────────────────────────┐
      │duck-platform │  │ 第三方：async-imap / lettre /        │
      │vault/secret  │  │ mail-parser / mail-builder /         │
      │ (AES-256-GCM)│  │ jmap-client(opt) / rusqlite / tokio  │
      │paths         │  └─────────────────────────────────────┘
      └──────────────┘
```

依赖方向单向向下，无环。**`duck-mail` 不依赖 `gpui`、不依赖 `duck-components`**（与 `duck-db`/`duck-redis`/`duck-ai` 同纪律）。

### 3.2 `duck-mail` 模块树

以 `flectar-mail-core` 的模块划分为蓝本，去掉日历/通讯录到 M3+，重排为：

```
crates/duck-mail/
├── Cargo.toml               # 零 gpui；tokio 封在 crate 内
└── src/
    ├── lib.rs               # MailCore：生命周期、账户注册表、事件广播
    ├── models.rs            # 数据模型（Account/ThreadSummary/MessageDetail/Attachment/...）
    ├── error.rs
    ├── config.rs            # Paths（数据目录、附件目录、库文件路径）
    ├── runtime.rs           # ★ 内嵌 tokio runtime（本模块唯一允许出现 tokio 的地方）
    ├── accounts/            # 账户配置 + 凭据（CredentialStore trait → duck-platform::secret）
    ├── db/
    │   ├── mod.rs           # 连接管理（WAL、pragma、迁移驱动）
    │   ├── migrations/      # ★ 12 个 .sql（纯 SQL，原样复用）
    │   ├── repo/            # 24 个 repo（accounts/threads/messages/folders/labels/search/…）
    │   └── snapshot.rs
    ├── proto/
    │   ├── imap.rs          # async-imap 封装（★ 只有读方向）
    │   ├── smtp.rs          # ★ 已落地：手写 RFC 5321 发信方向（**不用 lettre**，见 §6 M2）
    │   ├── jmap.rs          # (feature = "jmap")
    │   └── gmail.rs         # (feature = "gmail"，REST + ureq)
    ├── compose.rs           # ★ 已落地：RFC 5322 头部装配 + RFC 2047 编码（**不用 mail-builder**）
    ├── mime/                # mail-parser 计划、正文/附件解码、CID 内联（~~mail-builder 构造~~ → compose.rs）
    ├── sync/
    │   ├── engine.rs        # per-account actor（spawn_account / SyncCmd / SyncCtx）
    │   ├── folder_map.rs
    │   ├── threading.rs     # ★ 已落地（实际在 `src/threading.rs`）：JWZ 会话合并（纯函数）
    │   └── gmail.rs         # (feature = "gmail")
    ├── oauth/               # PKCE + loopback + token 刷新（浏览器打开走宿主回调）
    ├── search/              # 查询语言解析 + FTS5 绑定 + fold() 去音标 + nlp 中文分词（§2.4）
    ├── route/               # 规则路由（发件域/关键词 → 标签/箱）
    ├── autolabel.rs         # AI/启发式自动分类
    ├── queue.rs             # pending_actions（动作队列，支持离线排队与撤销）
    ├── scheduler.rs         # snooze / 定时发送
    ├── unsubscribe.rs
    ├── mail_security.rs     # 远程内容策略、追踪像素
    ├── events.rs            # MailEvent（内核 → 宿主）
    └── ai.rs                # ★ 邮件专用 prompt 层，调用 duck-ai
```

### 3.3 与既有 crate 的关系

| 既有资产 | 关系 | 说明 |
| --- | --- | --- |
| `duck-platform::{vault,secret}` | **复用** | 凭据与 OAuth token 走 `vault`（AES-256-GCM）加密；需要「键名 → 密文」独立落点时用 `secret`（`credentials.json`）。**不使用系统钥匙串** |
| `duck-ai` | **复用** | `provider` / `sse` / `tool` / `kb` / `wire` 全套；邮箱只写 prompt 层 |
| `duck-store` | **不复用** | kv 是「单键小对象」模型，不适合邮件；邮箱自带 SQLite |
| `duck-db` | **不复用连接** | 见 §4.5：只共享 rusqlite 版本，不共享连接对象（生命周期与事务语义冲突） |
| `duck-components` | **UI 层复用** | `dialog` / `sidebar` / `notify` / `content::accent_tag` / `dropdown_select` / `switch_control` |
| `gpui-base::text` | **关键复用** | `TextView::html()` 渲染邮件正文（§4.8） |
| `duck-command` | **登记** | 路由、撰写、刷新等动作 → 四输出面自动派生 |

---

## 4. 核心设计决策

### 4.1 异步模型：内嵌 tokio runtime（本模块最关键的一个决定）

**本仓纪律**：`duck-ai` 明写「不引 tokio」（`crates/duck-ai/Cargo.toml:8`）；`duck-redis` 明写「不开线程、不建 runtime、不引 tokio」（`crates/duck-redis/src/lib.rs:6`）；`duck-db` 则走**方案 B**：「自管 current-thread runtime `block_on`，tokio 的 Future 与类型都**不出本 crate**」（`crates/duck-db/src/lib.rs:7`，`docs/database-design.md` §4.1 裁决）。

**邮件必须异步**：IMAP 常驻连接 + IDLE 推送 + 多账号并发，用同步阻塞驱动会把线程吃光。melib 用 smol，Flectar 用 tokio。

**裁决：沿用 `duck-db` 的方案 B。**

```rust
// crates/duck-mail/src/runtime.rs
//! 本 crate 唯一允许出现 tokio 的地方。对外只暴露 Task<T> 与回调，
//! tokio 类型（Runtime/Handle/mpsc 除外）不得出现在公共 API。
pub(crate) struct MailRuntime { rt: tokio::runtime::Runtime } // worker_threads = 2，惰性创建
pub struct Task<T>(futures_channel::oneshot::Receiver<Result<T, MailError>>);
impl MailRuntime {
    pub fn spawn<F>(&self, f: F) -> Task<F::Output> { /* … */ }
}
```

三条纪律：

1. **`tokio` 已在依赖图中**（`postgres 0.19.14 → tokio-postgres 0.7.18 → tokio 1.53.2`，见 §4.2 实测），把它提为 `duck-mail` 的直接依赖是 **零新增下载**。需要额外开的 feature（`rt-multi-thread`/`macros`/`sync`/`time`/`net`/`io-util`/`fs`/`process`）**不引入任何新 crate**（`mio` / `socket2` / `bytes` / `libc` 均已在图内）。
2. **tokio 不出公共 API**：`duck-mail` 的 `pub` 面只有同步函数、`Task<T>` 与 `MailEvent`。加一条 CI grep 守卫（照 `duck-db` 的架构测试脚本先例）扫 `duck-mail/src/lib.rs` 与各 `pub` 签名的 `tokio::` 出现。
3. **DB 层不进 runtime**：`db/repo/*` 是纯同步 rusqlite（与 Flectar 一致），由 `duck-mail` 自己的**专用写线程**执行（照 `duck-store/src/writer.rs:93` 的先例），不经 tokio。

**UI 侧桥接**：沿用 `duck-ai` §4.1 的四条纪律——

```
① UI 线程：调内核方法 → 内核把 job 投进 tokio runtime
② 内核：tokio 任务跑完 → 把 MailEvent 塞 std::sync::mpsc::Sender
③ UI 线程：cx.spawn + background_executor().timer(40ms) 轮询 drain → 节流后 cx.notify() 一次
④ 取消 = AtomicBool + 关连接；超时分层（连接/全局/空转）
```

> ⚠️ 禁止逐事件 `notify()`（`dialog.rs` 的 prepaint 自激是前车之鉴）；必须节流。

### 4.2 依赖准入（逐个实测）

下列 MSRV 均用 crates.io API 实查（2026-10-12）。本仓硬门 `rust-version = "1.85"`（`Cargo.toml`），且 `resolver = "3"` 会做 MSRV 感知解析——**声明 MSRV 高于 1.85 的依赖会被解析器回避**（本仓曾因此否掉 `zip 8.x`/`keyring 4.x`）：

| 依赖 | 版本 | 用途 | 声明 MSRV | `Cargo.lock` 现状 | 结论 |
| --- | --- | --- | --- | --- | --- |
| `tokio` | 1.53.2 | 内嵌 runtime | 1.71 | **已在图**（postgres→tokio-postgres） | **提为直接依赖**，零新增下载 |
| `ureq` | 3.4.x | OAuth token / Gmail REST | — | **已在图** | 复用 |
| `keyring` | ~~3.6.3~~ | 凭据 | — | **已于 2026-10-12 从图内删除** | **不引入**；凭据改用 `duck-platform::{vault,secret}`（本条原写「已在图 / 复用」，已作废） |
| `rusqlite` | **0.32.1 → 0.40.2** | 本地邮件库 | 未声明 | **已在图（0.32.1）** | **升级**，见 §4.3 |
| `encoding_rs` | 0.8.42 | 非 UTF-8 字符集 | — | **已在图** | 复用（`mail-parser` 的 `full_encoding` 会用到） |
| `base64` | 0.22.1 | 内联图/编码 | — | **已在图** | 复用 |
| `chrono` | 0.4.45 | 时间 | — | **已在图** | 复用 |
| `async-imap` | 0.11.3 | IMAP | **未声明** | 新增 | ✅ 准入 |
| `lettre` | 0.11.23 | SMTP | **1.85** | 新增 | ✅ 准入（正好卡线） |
| `mail-parser` | 0.11.8 | MIME 解析 | **未声明** | 新增 | ✅ 准入（开 `full_encoding`） |
| `mail-builder` | 0.4.4 | MIME 构造 | **未声明** | 新增 | ✅ **已引入（2026-10-13，附件批）**：只用于 `multipart/mixed` 正文；`default-features = false`（关掉 `gethostname`）→ **零新增传递依赖**，`Cargo.lock` 只多它一条。头部仍手写，见 §4.14 |
| `ammonia` | 4.1.7 | HTML 净化 | **1.80** | 新增 | ⏸ **倾向不引**：实测 `gpui-base` 解析器已丢弃 `<script>`/`<style>`（见 §4.8），首版可不加 |
| `jmap-client` | 0.4.2 | JMAP | **未声明** | 新增 | ⏸ M2，`feature = "jmap"` |
| `quick-xml` | 0.42.0 | CalDAV/CardDAV | **1.86 ❌** | 新增 | ⛔ **超线**；M4 且需降版（≤0.37）或换 XML 方案 |
| `candle-core/nn/transformers` | 0.11 | 本地向量嵌入 | 未声明 | 新增 | ⏸ M3，`feature = "local-embeddings"`（Flectar 默认关） |
| `reqwest` | 0.13.5 | Flectar 用于 CalDAV/JMAP | 1.85 | **不在图**（`cargo tree -i reqwest` 空） | ⛔ **M1/M2 不引**，改用已在图的 `ureq` |

**导入纪律**：`duck-mail` 首版（M1）实际只新增 **2 个**第三方 crate（`async-imap` / `mail-parser`；`lettre` 与 `mail-builder` **当时都没引**，`ammonia` 经实测可不引，见 §4.8），其余全部复用既有图——`tokio` 与 `base64` 是由传递依赖提为直接依赖（**零新增下载**），`rusqlite` 直接复用全仓的 0.32.1（见 §4.3 第 11 条）。**附件批（2026-10-13）把 `mail-builder` 加回来**，但配 `default-features = false`，图上仍是零新增传递依赖。

> 迁移 SQL 是纯 SQL，与 rusqlite 版本无关，可原样复用；**只有 Rust 调用点需要适配**。

### 4.3 rusqlite 版本统一（硬约束，M0 前置）

**冲突事实**（实测）：

| | 本仓现状 | Flectar |
| --- | --- | --- |
| `rusqlite` | `0.32.1`（`Cargo.toml` workspace） | `0.40.2` |
| → `libsqlite3-sys` | `^0.30.1` | `^0.38.2` |
| `links = "sqlite3"` | 是 | 是 |

两个版本的 `libsqlite3-sys` **不可能共存**（cargo 硬错误）。因此必须二选一。

**量化对比**（grep 实测）：

| | 调用点规模 | API 特征 |
| --- | --- | --- |
| 本仓 4 个 crate（`duck-store` / `duck-ai` / `duck-db` / `duck-platform`） | **约 60 处** | 全是跨版本稳定 API：`params!`、`Connection`、`Error`、`InterruptHandle`、`types::Value`、`params_from_iter`、`open_in_memory`、`ValueRef` |
| `flectar-mail-core` | **约 1,000 处** | `params!` 594 · `query_row` 311 · `pragma_update` 35 · `types` 26 · `execute_batch` 24 · `prepare_cached` 10 · `ffi` 2 · **`config` 2（0.37+ 新增模块）** |

**裁决：把全仓 `rusqlite` 升到 `0.40.2`（作为 M0 独立前置改造）。**

理由：① 本仓改动面小一个数量级，且使用的都是十年来未变的 API；② 保住"参考项目原样复用"的诉求（迁 Flectar 的 1,000 处远比改本仓的 60 处贵）；③ `bundled` 路径下 FTS5 依旧无条件开启。

**M0 必须实测的风险点**（不能只靠推断）：
1. `crates/duck-db/src/drivers/` 的 SQLite 驱动 + `cancel.rs` 的 `InterruptHandle` 语义（0.32→0.40 是否仍一致）。
2. `duck-ai/src/store.rs`、`duck-store/src/*` 的 `params!`/`query_row` 是否有签名漂移。
3. Flectar 用到的 `rusqlite::config`（0.37+ 新 API）在 0.40 下的实际用法。
4. `THIRD_PARTY_LICENSES.md` 重生成（`sbin/licenses.py --check` 双重门禁，AGENTS/CI 规则）。

**兜底**：若 M0 实测发现 0.32→0.40 破坏面过大，退路是**把 `duck-mail` 的 db 层降到 0.32**（迁移 SQL 不变，只改 Rust 调用点）——但这是次优解，因为它把改动压到了新代码上，反而更难验证。

### 4.4 协议层选型

| 协议 | 选型 | 落点 | 说明 |
| --- | --- | --- | --- |
| **IMAP** | `async-imap 0.11.3`（`runtime-tokio`） | `src/proto/imap.rs` | Flectar 同款；依赖树干净（`imap-proto` / `nom` / `async-channel` / `stop-token`） |
| **SMTP** | `lettre 0.11.23`（`smtp-transport` + `tokio1-rustls-tls` + `builder` + `pool` + `hostname`） | `src/proto/smtp.rs` | 连接池 + rustls，无系统 OpenSSL |
| **JMAP** | `jmap-client 0.4.2`（`async` + `ring`） | `src/proto/jmap.rs` | M2，`feature = "jmap"` |
| **Gmail** | 自研 REST（`ureq`） | `src/proto/gmail.rs` | M2，`feature = "gmail"`；Flectar 用 `reqwest`，本仓换 `ureq` 以免进 `hyper` |
| **POP3** | — | — | 不做（§1.3） |

**统一抽象**：不照抄 melib 的 19 方法 `MailBackend` trait（它是为了同时支持 maildir/mbox/notmuch 等本地后端），本模块只有"远端账号"一种形态，用**枚举分发 + 能力位**（照 `duck-db` §4.2 的裁决）：

```rust
// crates/duck-mail/src/proto/mod.rs
pub enum MailProtocol { Imap, Jmap, Gmail }
pub struct ProtocolCaps { /* idle | condstore | move_ | labels | server_search | … */ }
```

### 4.5 存储与 schema

**独立库文件**：`{app_data_dir}/mail/{account_scope}.db`（WAL 模式）。与 `duck-store` 的 kv 库、`duck-ai` 的会话库、`duck-db` 的用户库**分库**——生命周期、备份策略、损坏爆炸半径都不同。

**不共享 `duck-db` 的连接对象**：`duck-db` 的抽象是"用户任意 SQL + 可编辑网格 + 取消"，而邮箱要的是"固定 schema + 迁移 + 事务写队列"。共享连接会强迫两边互相适配；**只共享 rusqlite 版本**即可。

**schema 原样复用 Flectar 的 12 个迁移**（`db/migrations/*.sql`，纯 SQL）：

| 迁移 | 内容 |
| --- | --- |
| `001_init.sql` | `accounts` / `folders` / `threads` / `messages` / `message_bodies` / `message_refs` / `attachments` / `contacts` / `contact_accounts` / `pending_actions` / `snoozes` / `snippets` / `split_rules` / `route_cache` / `drafts_meta` / `draft_attachments` / `app_settings` / `labels` / `message_labels` |
| `002` / `003` | 附件文件与内容分离 |
| `004` | CardDAV |
| `005` | 邮箱计数索引 |
| `006` | 联系人恢复 |
| `007` | 账号-标签归属 |
| `008` / `012` | 发件身份与来源 |
| `009` | 联系人学习清空重来 |
| `010` | 文件夹层级 |
| `011` | 待拉正文分页 |
| `files_001/002` | 附件文件表 |

后续迁移新增：`message_embeddings`（向量）、`ai_usage_events`（AI 用量）、`sync_failures`、`notification_outbox`、`gmail_sync_state`、`jmap_sync_state`、`message_folders`、`gmail_labels`、`cross_store_operations`，以及两个 FTS5 虚表：`messages_fts`、`attachment_files_fts`。

**写模型**：单写线程 + 事务（照 `duck-store` 的 writer 范式），读走只读连接池。**所有写路径单事务**（沿用 `duck-db` 纪律）。去抖策略与 `duck-store` 不同——邮件数据不允许"丢失最近的写"，故关键写（标记已读、移动）**立即落盘**，仅**批量同步结果**做事务批提交。

### 4.6 同步引擎

**形态**：per-account actor（`spawn_account(ctx, config) -> AccountHandle`），与 Flectar 一致。

```
AccountHandle { cmd_tx: mpsc<SyncCmd> }        // 宿主 → actor
MailEvent::SyncProgress / MailNew / MailUpdated / AccountState / NetworkState
```

**分层**（采纳 melib 的"策略分级"思想，但**不做 CONDSTORE/QRESYNC**——Flectar 与 rustmailer 都没做，且服务器支持率参差）：

| 层 | 职责 | 线程/连接 |
| --- | --- | --- |
| **主 actor** | 命令调度、延迟敏感的轻活（标已读、移动） | 1 条 IMAP 连接 |
| **回填 worker** | 历史正文预取、附件下载 | 独立连接，可并行、可取消 |
| **IDLE** | 收信推送（支持 `IDLE` 的服务器） | 1 条专用连接 |
| **SMTP worker** | 发信（durable，失败重试） | 独立 |

**增量算法**：
1. `SELECT` → 读 `UIDVALIDITY` / `UIDNEXT` / `HIGHESTMODSEQ`；
2. `UIDVALIDITY` 变了 → 该文件夹整体作废重建；
3. 新邮件：`UID FETCH (UIDNEXT 水位 - 已有) 的信封`；
4. 旗标对账：对**窗口内**（最近 N 条 + 本地未同步项）做 flag 全量比对（rustmailer 的"UID 水位 + flags_hash diff"思路），而非全量拉取；
5. 删除：`UID SEARCH` 对账本地 UID 集合。

**断线重连**：指数退避 + 网络状态事件（`NetworkState`）；离线时动作进 `pending_actions` 队列，恢复后按序补发。

**首次全量**：分批（每批 N 条）拉信封，正文按需/后台预取，UI 不等全量完成即可用（先出列表，正文懒加载）。

### 4.7 凭据与 OAuth

**凭据**（`duck-platform::secret`，**2026-10-12 起为应用自有文件后端**：值 = vault 密文，
落 `<应用数据目录>/credentials.json`；~~keyring 3.6.3 / SERVICE = `com.duck.toolbox`~~ 已废弃）：

| 键 | 内容 |
| --- | --- |
| `email:{account_id}:password` | IMAP/SMTP 口令 |
| `email:{account_id}:refresh_token` | OAuth 长期令牌 |
| `email:{account_id}:client_secret` | 用户自带的 OAuth client secret（可选） |

**口令/令牌绝不进 kv**（沿用 `duck-db` §4.3 与 `duck-redis` 的约定）；账号档案本身落 SQLite 的 `accounts` 表（脱敏，只留"引用"）。**access token 只在内存**，进程退出即失效。

**OAuth2 流程**（移植 `oauth/`，2,577 行）：
- `authorization_code` + **PKCE**（`oauth/flow.rs` / `providers.rs`）；
- 重定向：**loopback HTTP 服务器**（`oauth/loopback.rs`，本地随机端口）；
- **浏览器打开抽成宿主回调**：内核不依赖 GPUI/rfd，由 `duck-app` 注入 `open_url` 实现（照 `duck-platform` 的 rfd 模态禁忌——不在 render 里调）；
- token 刷新：`TokenProvider`，刷新失败 → `AccountState { sync_error }` 事件 + UI 提示。

> **协议层已落地（2026-10-13，G-MAIL-6）**：PKCE / 授权 URL / 回调解析 / 令牌交换与刷新 /
> XOAUTH2 SASL 全部在 `src/oauth.rs`，且 `ImapConfig`/`SmtpConfig` 已按 `AuthKind::OAuth2` 走
> XOAUTH2。**尚未落地的是宿主侧**——上面「loopback 服务器」「浏览器打开」「刷新令牌持久化」
> 三条要碰 `duck-app` 热点，属独立一批。完整口径见 **§4.17**。

**凭据读写落文件/OAuth 刷新是阻塞调用**，必须在 `background_executor` 或专用线程执行
（`duck-ai` 已有同类处理）。~~「keyring 是阻塞调用」~~（keyring 已移除，但纪律不变）。

### 4.8 MIME 与正文渲染（本模块第二关键决定）

**解析**：`mail-parser 0.11.8`（**必开 `full_encoding`**）——否则 GB2312/GBK/Big5/Shift-JIS 会解码成 U+FFFD（中文邮件用户不可接受）。正文计划沿用 Flectar 的 `MimePlan` 思路：从 BODYSTRUCTURE / 解析树得出「正文文本段（含 charset/CTE）+ 附件清单」，解码走 `mime/decode_text_section`；Flectar 还带两个启发式修复（`looks_like_undecoded_quoted_printable`、`looks_like_base64_encoded_html`），值得一并移植。

**HTML 渲染 —— 本仓原生方案（重大简化）**：

实测确认：**本仓已在用的 `gpui-base 0.7.1` 自带完整 HTML 渲染**，且经 `gpui_component::text` 转出（本仓 `crates/duck-app/src/pages/assistant.rs:51` 就是这么引 `TextView` 的）——**无需任何新增依赖**：

| API | 位置 |
| --- | --- |
| `TextView::html(id, html)` → 邮箱正文首选 | `gpui-base-0.7.1/src/text/text_view.rs:237` |
| `TextView::image_source(resolver)` → 远程图片闸门 | `src/text/text_view.rs:265` |
| `TextViewState::html(text, cx)` | `src/text/state.rs:167` |
| `gpui_base::text::html(source)`（便利函数） | `src/text/mod.rs:43` |
| 内部解析器（html5ever + RcDom → `ParsedDocument`） | `src/text/format/html.rs`（35 种块级元素、表格、图片、链接、代码块、引用） |
| 模块公开性 | `gpui-base/src/lib.rs:68`（`pub mod text`）、`:179`（`pub use text::{…}`）；`gpui-component/src/lib.rs:88`（`pub mod text`） |

含义：**不需要 Blitz（Flectar 路线）、不需要 WebView（`wry`）、不需要 `html2text`、不需要新增任何 HTML crate。** `html5ever 0.27` 已通过 `gpui-base` 在依赖图内。

正文渲染管线：

```
原始 HTML
  → ① 净化（大部分已由 gpui-base 兜底，见下）
  → ② CID 重写：cid:xxx → data:image/...;base64,...（带容量上限）
  → ③ 表格布局拍平：table/tbody/tr/td/th → div（duck_mail::mime::flatten_email_tables）
  → ④ TextView::html(id, cleaned) + .image_source(resolver)
         resolver 按远程图片开关（默认放行，提示条可收回）决定放行或剥离
```

**表格布局拍平**（2026-10-13 实测结论）：营销邮件 / Newsletter 全靠嵌套 `<table>` 排版
（NVIDIA 的 Marketo 邮件一封 40 个 `<table>`），而 gpui-base 的文档模型把单元格按
顺序平铺，整页挤成一团分不出段。把表格族标签整体改写成 `<div>`（每个单元格自成一
块），整封变成自上而下的堆叠布局；条件注释 `<!--[if mso]>…` 整段跳过。真数据表也
会被拍平——损失列对齐换整体可读，是简版客户端的通行做法。

**净化层的实测结论**（决定要不要引 `ammonia`）：

| 事实 | 位置 | 含义 |
| --- | --- | --- |
| `<script>` / `<style>` **在解析时被显式丢弃**（匹配分支直接返回 `None`） | `src/text/format/html.rs:594` | 脚本注入面已被消除 |
| `cleanup_html()` 只做**压缩**（`html5minify::Minifier`），**不是**安全净化 | `src/text/format/html.rs:83` | 不要把它当 sanitizer |
| 渲染目标是**文档模型**（`ParsedDocument`），不是浏览器 | `src/text/format/html.rs` | 无 JS 执行、无 CSS 布局引擎、无 `iframe`/外部资源加载 |

即：**XSS 面几乎不存在**（不执行脚本、不跑 CSS），首版**可以不引 `ammonia`**。仍需自建的部分是**属性级白名单**（`on*` 事件属性、`javascript:` 协议链接）——`TextView` 的链接点击是宿主回调（`link_click_handler`），在回调里做协议校验（只放行 `http`/`https`/`mailto`）比全量净化更省。**M1 建议：不引 `ammonia`，改用"解析器自带丢弃 + 链接协议白名单 + 远程图片开关"三层。**

容量上限沿用 Flectar 的实测值：单图 8 MB / 单信 16 MB / 单线程 32 MB，内联图 ≤ 32 张；`.image_source` 返回的 `data:` URL 由 `ImageNode.embedded: OnceLock` 解码一次后复用（不会每帧重解码）。

**纯文本邮件**：`TextView` 直接渲染；HTML 邮件若有 `multipart/alternative` 的 text/plain，**默认展示 HTML**（纯文本备胎多为机器压出的兜底版：丢链接、丢表格、丢行内图），用户在阅读器里可一键切纯文本；只有纯文本的邮件照常按纯文本显示。形态由「设置 → 邮箱 → 默认正文形态」决定（出厂 `html`），见 `module_settings::MailSettings::prefer_html` 与 `pages::mail::render_body`。

**正文获取**：打开邮件即**自动**拉取正文（`fetch_message_body`），失败**不静默**——状态栏 + 通知都会给出原因；若拉取权被后台预取占着（原子抢占生效），前台**等在途拉取收尾**（最多 15s，落库立即返回；对方失败则自己再抢一次），不把「后台慢慢拉」的节奏甩给正看着邮件的用户。预取**一条连接拉完整轮**（connect/SELECT 一次，逐封 `UID FETCH`——每封重连一次时 163 上 20 封要几十秒），另保留「启动时清理悬挂 `fetching`」兜底，口径对齐 Flectar Mail `crates/flectar-mail-core`（`missing_bodies_at` / 原子抢占 / 启动清理）。

**附件**：`mail-builder` 构造、`attachments` 表 + 文件目录分离存储（迁移 002/003 的模型）；预览沿用 `duck-app` 现有策略（图片/PDF 由 `duck-platform` 的 opener 交给系统）。

### 4.9 AI 能力（复用 `duck-ai`，只写 prompt 层）

**分层**：

```
duck-mail/src/ai.rs          ← 邮件语义 prompt（移植 Flectar ai/mod.rs 的 prompt 构造）
        │  上下文：thread_context()
        ▼
duck-ai::{provider, sse, tool, kb, wire}   ← 本仓既有：协议 / 流式 / 工具 / 知识库
```

**可移植的 Flectar prompt 资产**（`flectar-mail-core/src/ai/mod.rs`，68 KB，全部是"数据 → 消息序列"的纯函数，与网络层解耦）：

| 能力 | Flectar 函数 |
| --- | --- |
| 线程摘要 | `summarize_prompt` / `summarize_thread` / `summarize_thread_stream` |
| 智能回复 | `quick_replies_prompt` / `quick_replies` / `quick_replies_prompt_voiced` |
| 起草 | `draft_prompt` / `draft_prompt_voiced` / `voice_profile_prompt` |
| 校对 | `proofread_prompt` |
| 签名生成 | `signature_prompt` |
| 意图识别 | `intent`（`AiIntent`） |
| RAG 问答 | `rag_context` / `ask_prompt` / `AGENTIC_ASK_SYSTEM` / `search_inbox_tool` |
| 上下文构造 | `thread_context` / `format_excerpt` / `reply_target_line` |
| 注入防护 | `clean_untrusted`（把邮件正文当不可信输入） |
| 风格/语言 | `language_directive` / `apply_language` / `ui_language_name` |

**AI 专用设计的三个要点**：

1. **提示注入防护**：邮件正文天然不可信（攻击者可构造"忽略以上指令"）。Flectar 的 `clean_untrusted()` 是必须移植的一环；`duck-ai` 的工具审批闸门（`docs/ai-design.md` §4.6）在邮件场景要更严——**AI 不得自主执行发信/删除**，只出建议。
2. **用量与成本可见**：移植 `ai_usage_events` 表与 `AiUsage`/`AiUsageStats`，设置页可见（邮件 AI 调用频次远高于聊天）。
3. **流式**：`duck-ai::stream_chat` 是**同步阻塞**的（本仓零 tokio 的派生结果），所以 AI 流式走 §4.1 的"专用线程 + 40ms 轮询 drain"，与 `duck-ai` 页面同一套范式，不额外引入异步。

**自动分类**：`autolabel::classify` 是**启发式**（先判已知发件人 → 再判关键词/列表头），不烧 token；只有启发式不确定时才上调 AI（`route/category_prompt`）。这个"先规则后模型"的两级结构应当保留。**结构化输出解析**照 stalwart 的分隔符 + 字段索引法（§2.4-③）：`类别<sep>置信度<sep>解释` 按索引取字段，不依赖模型输出合法 JSON。

**本地向量（M3，可选）**：Flectar 的 `local-embeddings` feature 用 `candle-*`（CPU-only）+ `tokenizers` + `hf-hub`，默认关闭；本仓若上，必须做成**非默认 feature**，并把"模型下载"明确为可选能力（离线优先原则）。

### 4.10 UI 形态

> **效果图**：`docs/mail-ui-mockup.html`（自包含，离线可看；含明暗主题切换与缩放）——主界面三栏 / AI 面板 / 撰写辅助窗口 / 账户向导 / 空态与同步 / 命令面板与右键菜单 / 字阶与补图标清单，颜色与尺寸全部取自 `duck-theme` 真实 token。以下文字版与之一一对应。

**三栏主视图**（`crates/duck-app/src/pages/mail.rs`）：

```
┌──────────────┬───────────────────────────┬──────────────────────────┐
│ 左栏（树）    │ 中栏（线程列表）           │ 右栏（阅读器）            │
│ 账户         │ 虚拟滚动 + 无限加载        │ 发件人/收件人/时间        │
│ └ 智能箱     │ 行：发件人·主题·摘要·时间  │ 正文（TextView::html）   │
│   ├ 收件箱   │ 未读圆点 / 旗标 / 附件标记 │ 附件条 / 内联图          │
│   ├ 旗标     │ 多选 + 批量操作            │ 引用折叠                 │
│   └ 已发送   │ 右键菜单（移动/标签/删除） │ AI 面板（摘要/回复建议）  │
│ └ 标签       │                           │                          │
└──────────────┴───────────────────────────┴──────────────────────────┘
```

- **左栏/右栏**用 shell 级 `with_sidebar` / `with_right_sidebar`（AGENTS 记录：右栏是 shell 级统一区域，别嵌进页面——`flex_1` 在 gpui-fast Block 容器里会塌高）。
- **列表虚拟化**：只渲染可视行（参照 `tusk` 的网格虚拟化思路与 `duck-app` 现有列表）。
- **三栏骨架**：可直接抄 `crates/duck-app/src/pages/redis.rs`（左键树 `flatten_tree` + 中内容 + 右详情）与 `db.rs` 的 `EditFormView` 范式。

**弹窗**（`crates/duck-app/src/pages/mail_dialogs.rs`）：

| 弹窗 | 形态 | 参照 |
| --- | --- | --- |
| 添加账户向导 | 多页签模态（邮箱/密码·OAuth → 服务器 → 安全 → 高级） | `redis.rs` 的 6 页签连接编辑器（`FORM_TABS`） |
| 撰写 | **独立辅助窗口**（`AuxiliaryWindowView`），非模态 | `settings.rs` 的 `AuxWindow` 机制 |
| 邮件设置 | 设置页新增「邮箱」分类 | `settings.rs` |

**硬约束提醒**（AGENTS/记忆中的既有教训）：
- 每个弹窗调用点必须 `.close_button(false)`（否则与 `modal_title_row` 叠成两个 ✕）；
- "边打边过滤"的输入框必须挂**独立视图**（`Entity<V: Render>`），靠 `cx.subscribe(&input, Change → notify())`；
- 图标进 `duck-components/assets/icons/`（Lucide，stroke 1.75），标签用 `content::accent_tag`；
- 中文文案必须同步补 `crates/duck-theme/src/i18n.rs` 的 EN 表（字节序）。

**布局落地状态**（2026-10-12，M1 已接数据）：`duck-mail` 内核与 `crates/duck-app/src/pages/mail.rs` 一起落地，`app.rs` 的 `duck-route-email` 从占位页换成真实页面（活动栏图标 / `⌘8` / 原生菜单 / 命令面板四个输出面早在占位阶段就已派生，无需再动）：

| 区域 | 现状 |
| --- | --- |
| 左栏 | 账户头（首字母头像 + 账户名 + 移除 / 添加账户）+ 账户切换（多账户时）+ 文件夹列表（角色图标 + 未读数）；无账户 → 空态 + 引导 |
| 中栏 | 列表条（文件夹名 + 计数 + 写邮件 + 清空搜索 / 同步）+ 搜索行（检索框 + 「搜索语法」速查入口）+ 邮件列表（未读圆点 / 旗标星 / 发件人 / 附件夹 / 时间 / 主题 / 摘要） |
| 阅读器 | 工具条（标为已读 · 未读 ┃ 回复 ┃ 下载正文〔正文打开即自动获取，此键是失败后的重试入口〕）+ 头部（主题 / 发件人 / 时间 / 收件人 / 抄送 / 大小）+ 正文 + 附件清单（每行「另存为…」） |
| 右栏 | **无**（见下第 1 条） |

三处与效果图的**有意**差异（同已落进代码注释）：

1. **前一版「禁用 + 说明」的控件全部删除，而不是接线**。前一版按 `redis.rs` 弹窗「M1.x 接入」的既有口径保留了 8 个阅读器动作 + 右栏 AI 面板（禁用 + 悬停写明等哪一期）。2026-10-12 的 **P3-6**（`docs/architecture-gap-analysis.md`）把这种「自我标注为待接入」判为不可保留——「界面不应再有演示数据 / 演示文字，每个图标的功能都需要实现完整」，处置判据是**后端存在 → 接线；后端不存在 → 删除**。于是：左栏添加 / 移除账户、列表条同步、阅读器已读切换与下载正文**接线**；标签分组、筛选、回复 / 转发 / 归档 / 稍后 / 删除 / 打标签 / 旗标 / AI 分析 / 更多、以及右栏 AI 面板**删除**（前提都在 M2）。M2 把发信、动作队列与 AI 接通后，这些入口按同一份判据接回来。
2. **左栏宽度取 236，不是效果图的 216**：与 `db.rs` / `redis.rs` / `terminal.rs` 走同一条档位（184–340，默认 236），四页互相切换时侧栏不跳宽。效果图的 216 只是示意。
3. **纵向尺寸一律走 `DensityTokens`，本页只留横向分配比例**。原先「邮件列表行高」是本页常量 64 并注明待定——现在收口为**新增的 `density.mail_row`（56 / 64 / 76）**：`task_row` 最宽只有 52，且语义是「双行任务」，压不住三行邮件。同理左栏行取 `density.nav_row`、分组标题取 `density.section_header`，否则调密度设置时邮箱页不动。

配套改动：`duck-mail` 新 crate（账户 CRUD + 凭据加密 + SQLite + IMAP + MIME + 增量同步 + FTS5，91 个测试）；`duck-components/assets/icons/` 新增 10 个 Lucide 1.48.0 图标（`inbox` `star` `reply` `forward` `paperclip` `tag` `sparkles` `pencil` `at-sign` `flag`，stroke 1.75）；`duck-theme` 新增 `mail_row` 密度档。

> **补记（2026-10-13）：撰写弹窗的实际形态，与它的「附件」区。** 上面那张弹窗表把「撰写」设计成**独立辅助窗口**（非模态），而落地的这一版是**模态弹窗**（`open_compose_dialog` → `window.open_dialog`）——先按「最小可用发信」做出来；改成辅助窗口属独立设计（要处理多窗口与草稿归属），届时再动。弹窗正文的字段顺序固定为：发件人（只读）→ 收件人 → 抄送 → 主题 → 正文 → **附件** → 错误 → 「不会写回已发送」的常驻说明。「附件」区是这一批新加的（「添加附件」按钮 + 已选清单 + 逐条移除），取舍与「为什么这里才引 `mail-builder`」见 §4.14。

### 4.11 离线优先、撤销与定时

- **动作队列** `pending_actions`：所有远端动作（标已读/移动/删除/发信）先写本地库并乐观更新 UI，再由 worker 补发；失败进 `sync_failures` 并可重试。
- **撤销**：删除/移动走"软删除 + 宽限期"（本地标记，宽限内可撤销，超时才真正提交远端）。
- **snooze / 定时发送**：`snoozes` 表 + `scheduler::spawn`（整点扫描 + `notification_outbox`）。
- **外观**：`mail` 的设置（列表密度、是否加载远程图片、HTML/纯文本偏好）一律进新增的 `Appearance` 子键或 `app_settings` 表——注意本项目约定「布局/密度类设置进 `Appearance`」，但**邮箱专属偏好**（远程内容开关等）留在 `app_settings` 更合适，需在评审时定。

> ⚠️ **补记（2026-10-13）：动作队列的并发竞态已修**。原文「由 worker 补发」把 `flush_pending` 当成了唯一执行者，代码里却**不成立**：`MailPage` 的 40ms 轮询（`check_pending_flush`）与手动同步都能在「上一条还没跑完」时再进来一次，两条 `flush_pending` 于是真并发（异步 runtime 上的阻塞任务不在同一线程，`mark_inflight` 的无条件 `UPDATE` 拦不住）。后果不轻：`move` / `delete` **不幂等**，同一行会被执行两次。
>
> 修法是两道闸门叠在一起，缺一不可：
>
> | 闸门 | 位置 | 拦的是 |
> | --- | --- | --- |
> | **进程内** per-account RAII 守卫（`try_begin_flush` + `flush_gate()`） | `queue.rs` | 同账户并发（抢不到就返回 `Ok(0)`，不是错误） |
> | **条件认领** `mark_inflight`（`WHERE state = 'pending'`，返回 `bool`） | `db/repo/queue.rs` | 跨进程 / 跨 runtime 的并发 |
>
> 守卫在**阻塞栈帧**上持有（不是跨 `.await`），Drop 里对中毒的锁**必须忽略而不是 `unwrap()`**——否则一次 panic 会把闸门永久锁死。这条与「同账户重入返回 `Ok(0)`」一起，也让 `requeue_inflight` 的旧注释（「补发是本进程唯一的执行者」）作废：那条前提本来就是错的，安全性现在由上面两处共同保证。

### 4.12 检索：条件查询语言与它的界面

`db/repo/search.rs` 从「每 token 加引号 + 隐式 AND」（原来的 `to_match_expression`）重写为两层：

1. **词法** `scan()` → `RawToken { key, value, raw }`：按空白切词，`key:` 的冒号必须出现在任何空白或引号**之前**；引号**只在值的开头**有含义——`subject:"quarterly report"` 是一个词、`a"b` 是字面量 `a"b`、没写完的 `from:"abc` 也给出可用值（而不是让整条查询报语法错）。
2. **归位** `classify()` → `Query { text, filters }`，再分别落到 FTS5 的 `MATCH` 与 `messages` 的 `WHERE`。

| 写法 | 落到哪 | 说明 |
| --- | --- | --- |
| 裸词 / `"两个词"` / `词*` | FTS5 `MATCH` | 多个词隐含 AND；前缀只在**纯字母数字 / 汉字**上开（`a-b*` 一律当字面量，放行了用户敲个 `*` 就能让 `MATCH` 报错） |
| `from:` / `to:` / `subject:` / `folder:` | `messages`（`LIKE`） | 子串匹配；`from:` 同时试 `from_addr` / `from_name` / `sender_addr`；`folder:` 同时试 `imap_name` 与 `role` |
| `is:unread` / `is:starred` / `has:attachment` | `messages` | 布尔档，含反档 `is:read` / `is:unstarred` 与别名 `is:flagged` |
| `before:`(`older:`) / `after:`(`newer:`) | `messages` | `YYYY-M-D`（月/日允许一位数）→ **UTC 日界**；本地时区换算会让「同一句话在不同机器上筛出不同结果」 |

三条不能省的实现口径：

- **认不出的 `key:value` 整个退回自由文本**——搜 `raw` **原文**，不是 `value`。否则 `http://example.com` 会被吃成 `//example.com`、`is:whatever` 会被静默丢掉。取舍是「宁可搜不到用户想要的，也不去搜用户没写的东西」。
- **`LIKE` 模式必须转义** `%` / `_` / `\`（`ESCAPE '\'`）：不转义时 `from:%` 会匹配全部邮件，看起来像「过滤没生效」。
- **条件与文本都空就返回空**，绝不退化成无 `WHERE` 的全表扫描（一个只有空格的查询串不该把几万封邮件全捞出来）。

排序分两种：有自由文本时按 `messages_fts.rank`，**只有条件时按 `m.date DESC, m.id DESC`**——相关度在「筛出所有未读」这类查询里全是并列，会给出一个看起来随机的顺序。

同一个 key 写多次是**累加**（`from:a from:b` 两个都要满足），只有 `folder:` **只留最后一条**——一封邮件只属于一个文件夹，累加得到的是必然为空的交集。

**没做的（不是漏了）**：`-词` 排除、`OR`、括号、`larger:` / `filename:` 都需要更完整的语法树；在「每加一条都要能说清落到哪张表哪一列」的前提下，先不做比做一半好。

**界面侧**：这套语法没有别的地方能让人知道，所以搜索行右侧挂了一个 `info` 图标 → 「搜索语法」速查弹窗（`SEARCH_SYNTAX_ROWS` + `render_search_syntax`，只读、单一「知道了」，用的是缺省的 `ok_handler`）。两条 UI 约束钉在 `search_syntax_rows_keep_the_ui_translatable` 里：

- **语法列一个字都不许带中文**。i18n 守卫是从源码行里逐字取引号之间的**原文**（`\"两个词\"` 取出来就是带反斜杠的 `\"两个词\"`）再去 EN 表里查，而表里的键是**已解析**的——两者永远对不上，所以左列一旦出现中文，`every_chinese_literal_is_translated` 必红，**且没法靠登记条目消掉**。顺带这也是把这张表做成 `const`（而不是 `fn(cx)`）的原因：它与语言无关，`tr()` 属于渲染那一步，于是这条守卫不需要 `App`（`duck-app` 的单元测试里没有 `DuckThemeState` 全局）。
- **关键词类语法必须齐**：少一条就是「内核支持、界面不说」，用户不可能猜出来。

> ⚠️ 仍未做：`fold()` 去音标与 stalwart `nlp` 的中文分词——虚表分词器是 `unicode61 remove_diacritics 2`，**中文按整段切**，长句召回很差。修它要换分词器 + 重建索引，属 M3（`search.rs` 模块头有同样的说明）。

### 4.13 附件按需下载

列表 / 阅读器早就有附件清单（文件名 + 大小），但**一直没有任何取回路径**——界面许诺了一份拿不到的文件，这正是 P3-6 要治的形态。现在补齐：阅读器里每个附件行右侧一个「另存为…」按钮。

**入口为什么是「另存为」而不是「下载到某个固定目录」**：桌面客户端的惯例是让用户自己选落点（`duck_platform::save_file`，rfd 原生对话框，预填文件名）。固定目录要么写进用户的「下载」文件夹（未经许可动别人的目录），要么写进 `app_data_dir`（用户之后找不到）——两个都不如让用户点一下。

**内核侧的关键取舍：整封取回，而不是只 `FETCH` 那一段。**

| | 按段 `BODY.PEEK[<section>]` | 整封 `BODY.PEEK[]`（**采用**） |
| --- | --- | --- |
| 传输量 | 只那一段 | 多了正文的几十 KB |
| 前置能力 | 需要 `BODYSTRUCTURE` 解析器（`proto/imap.rs` 没有） | 已有（`fetch_body_remote`） |
| 段号来源 | `attachments.imap_section`（**现在是空的**，落库时留空） | 不需要 |

真要省流量，正确做法是**索引时就把段号写进库**（那要动 MIME 走查器，让 `ParsedAttachment` 带上 part 号），不是现在手写一个 `BODYSTRUCTURE` 解析器。这一批选了「整封取回」——在「能拿到文件」这个目标下这笔交易划算。⚠️ 这条要在 M3 做中文分词 / 重索引时一并处理（重索引本来就要过一遍 MIME）。

**把库里的行对回 `mime::parse` 的结果**靠 `attachments.ordinal`（0 起的清单位置，`ORDER BY id`）：`replace_attachments` 按 `ParsedMessage::attachments` 的先后逐条 `INSERT`，所以「第 n 行」就是「解析出来的第 n 个附件」。⚠️ **这条耦合没有数据库层面的保证**（表上就没有「序号」列），所以下载端**必须复核** `filename` + `size`：

- 序号对不上 → 退回「按文件名 + 字节数再找一遍」；
- 还是找不到 → 报错（`MailError::Mime`，英文详情），**绝不把另一份内容写进用户以为的那个文件里**；
- 库里 `size` 为 0 表示「未知」，此时只看文件名（「未知」不是「必须是 0 字节」）。

**幂等**：库里 `file_path` 记的就是这次选的路径、且那个文件还在 → 直接回它，**不连网**。反过来说，文件被用户自己删了就必须重新下载——`is_file()` 那一判不能省，否则界面会去打开一个不存在的文件。

**失败不留半个文件**：字节先在内存里备齐（`ParsedAttachment::bytes` 本来就解码好了），再 `fs::write` 一次落盘。失败时目标路径上是空的（有测试钉住）。

**错误原样上抛**：连接失败 / 认证失败 / 超时各有自己的 `code()`，界面靠它选文案。这里**不套** `MailError::Other`——套了 `code()` 就变成 `other`，「连不上服务器」这类判断全丢（有测试钉住）。

> ⚠️ 已知缺口：**错误详情是英文**（内核约定，见 §7 与 `error.rs` 模块头），所以中文界面下会看到「保存附件失败：connection failed: …」这样的中英混排。要修得做一张 `code() → 本地化文案` 的表，并把 7 处 `trf(cx, "…失败：{}", error)` 改成查表——属独立一批（§8 第 12 条）。
>
> 未做：下载进度（大附件时界面只有一个 `busy` 态）、「在访达中显示」（要平台 API）、附件内容进 FTS（`attachment_files_fts` 虚表在设计里有、未建）。

### 4.14 发信附件（`multipart/mixed`）

收件方向的附件在 §4.13 落地后，发件方向还欠着——撰写弹窗能写信，但**带不上文件**。这一批补齐：撰写弹窗的「附件」区 = 一个「添加附件」按钮（原生多选）+ 已选清单（每行一个移除按钮）。

**最后一个刻意的依赖让步：这里引了 `mail-builder`。**

§6 M2 那一批之所以能手写 `compose.rs` 而**不引** `mail-builder`，是因为那段只发纯文本——真正要自己写的只有「头部装配」与「SMTP 状态机」这两件**本来也引不到合适库**的事。附件改变了这份账：

| | 手写 `multipart/mixed` | `mail-builder`（**采用**） |
| --- | --- | --- |
| 要自己保证的 | boundary 唯一性、每段 base64 的 76 列折行、`Content-Disposition` 里非 ASCII 文件名的编码、按内容选 `7bit`/`base64`/`quoted-printable`、段与段之间那个 CRLF 的归属 | 库已实现，自带回归测试 |
| 写错的后果 | **不报错**，只是对方收到一个打不开的附件 | —— |
| 代价 | 0 | 多一个依赖（**但零新增传递依赖**，见下） |

**头部仍然手写。** 引库不等于把整封信交给库：`Date` … `MIME-Version` 那一整段（含头部注入拒绝、RFC 2047 编码、折行、`Bcc` 绝不进头）都留在 `compose.rs`，只把**正文**交给 `mail-builder::mime::MimePart`。无附件时干脆**完全不走**这条路径——输出与引库之前逐字节相同（原有 16 条 `compose` 测试一条没动）。理由：那条路径的输出已经被钉死，没有理由让「不带附件的邮件」也白担一层依赖风险。

**`default-features = false`，因此仍是零新增传递依赖。** `mail-builder 0.4.4` 的默认特性是 `gethostname`，而它**只被 `MessageBuilder::write_to` 的「调用方没给 `Message-ID` 时自动生成一个」用到**；本仓自己生成 `Message-ID`（`compose::generate_message_id`）、并且只调 `MimePart::write_part`，所以关掉它**不会少任何能力**，却少拉一个 crate。`Cargo.lock` 的净变更因此只有 `mail-builder` 一条（`gethostname 1.1.0` 本来就在图里，由别的依赖带进来）。

**顶层 `Content-Type` 由库里写，不是我们写。** 它必须和自己生成的 boundary 一致；分两处写迟早会对不上。位置不变——仍紧接 `MIME-Version`，排在头部块的最后。

**正文段的编码由内容决定（库的行为，已用测试钉住）**：纯 ASCII 正文走 `Content-Transfer-Encoding: 7bit` + 原文；含非 ASCII 走 `base64`。所以「带附件的邮件里，正文段在源码里可能不是肉眼可读的」是**正常**的，不是编码错误。

**读盘放在内核（`OutgoingAttachment::from_path`），且在选择文件那一刻就读。** 只存路径、等点「发送」再读的话，「文件没了 / 读不出来」会推迟到发送那一刻才暴露；而且 `Draft` 就不再是纯数据（装配会变成带 IO 的操作，`build_message` 的纯函数性质与那组测试都保不住）。界面侧把读盘放进后台执行器（几十 MB 的附件在渲染线程上读会把界面冻住），并遵循「一次挑了多个、其中一个读不出来时**不退回整批**」——只记第一条失败原因，其余照常挂上。

**同名附件只留一个。** 清单上两行一模一样的文件名分不清谁是谁，而「同一封邮件挂两份同名文件」在实践里只能是误操作。

**「附件」区的状态怎么刷新**：`ComposeForm` 不是 `Render` 视图，但它被 `render_compose_form` 在弹窗 `content` 闭包里读——而弹窗层（`gpui-component` 的 `WindowStateLayers`）每帧重跑一次 `content`，所以改完 `ComposeForm::attachments` 再 `notify` 就会立刻反映出来（与 `sending` / `error` 同一个机制）。

**「在访达中显示」只在落过盘之后出现**（2026-10-13 接线）。附件行原本只有「另存为…」；现在存成功后会多出定位按钮，指向**用户选的那个落点** —— 不是 `AttachmentMeta::file_path`，后者指的是 `app_data_dir()/mail/attachments` 里的应用缓存副本，拿它去「显示」会把人带到应用目录。落点记在页面状态 `MailPage::saved_attachments`（`(附件主键, 目标路径)`，同一附件重复另存以最后一次为准），**只活在本次会话**：重启后按钮消失，重新另存一次即可回来。还没落过盘就不给按钮 —— 给个永远弹不出东西的按钮不如不给。

> 未做：单个附件的体积预检（只在服务端声明 `SIZE` 且整封超限时报错，报的是内核英文详情，见 §8 第 12 条）、拖拽入窗、**附件下载进度**（`下载中` 是个不动的状态字，没有百分比）。

### 4.15 草稿（本地）

`drafts_meta` / `draft_attachments` 两张表在 M0 就建好了，但 repo 层一直空着——撰写框一关，写了一半的信就没了。这一批补齐：撰写弹窗左半终于有「存草稿」，草稿箱列表置顶本地草稿，点一条草稿整条回填（含附件）。

**表早就够了，一行 schema 都没动。** 一条草稿 = 四张既有表里的四行：

| 表 | 存什么 |
| --- | --- |
| `messages` | 一行草稿：`is_draft = 1`，**`folder_id` 与 `uid` 留 NULL**；主题 / 收发地址 JSON / `has_attachments` / `snippet` 都在这里 |
| `message_bodies` | 纯文本正文（与收信同一个落点） |
| `drafts_meta` | `mode`（new / reply / forward）、`in_reply_to_message_id`、`remote_uid`（后者留空） |
| `draft_attachments` | 附件**路径**（`file_path` / `filename` / `mime_type`） |

**`folder_id` 留 NULL 是这一批最关键的一处口径。** 草稿不属于任何 IMAP 文件夹，于是 `messages::list_by_folder` 的 `WHERE folder_id = ?1` 天然带不出它，「UIDVALIDITY 变了就清空该文件夹」（`sync::clear_folder`）也不会误删草稿。**代价是检索必须显式排除草稿**：`search.rs` 原来没有 `is_draft = 0` 这个基谓词，而草稿一旦被捞出来，回填 `MessageSummary` 时那个**非空 `i64`** 的 `folder_id` 会直接 `InvalidColumnType` 报错。所以 `search()` 补了这条基谓词，并用 `has:attachment`（纯过滤器、不依赖 FTS 索引）双向钉住「草稿既不在文件夹列表里、也不在检索结果里」。

**`Message-ID` 只生成一次。** 新草稿由内核生成一个，改存时沿用旧的（`drafts::save` 里那段 `existing_message_id`）。否则每存一次草稿就换一个 id，别人引这一封的 `In-Reply-To` 会悬空。

**附件在库里只有路径，没有字节。** 表结构给的就是 `file_path`，所以打开草稿要**重新读盘**——文件被挪走 / 删掉时草稿照样打得开，只是少一个附件，读不出来的条数会提示出来（**不阻断**打开）。界面侧为此把「已读好的字节」与「源路径」配对存着（`ComposeAttachment`）：字节是**选文件那一刻**读的，路径是同一时刻记的——等存草稿时再回头问系统，文件可能已经不在了。

**「存草稿」不校验收件人。** 草稿的全部意义就是「还没写完」；要求先填一个像样的收件人才能存，等于把最该被存下来的那半途而废挡在门外。地址语法同理不校验。这一条与「发送」正好相反——`to_draft` 那套校验只服务发信。

**存完不关弹窗。** 存草稿是幂等的、可反复做的动作；关掉反而逼着用户再打开一次才能接着写。`ComposeForm::draft_id` 记下行号，所以再点一次是**改存同一行**（不是又冒出一条）。发送成功后顺手删掉这条草稿（同所有邮件客户端），走的是已经写好的 `delete_draft`——那条路已经把「删掉 + 刷新列表 + 失败不连坐」写完了。

**「删除草稿」做成行内的兄弟节点，不在可点击区域里面。** 草稿行主体整块是一个按钮（点开撰写弹窗），往按钮里再塞一个按钮，两次点击谁先拿到就交给事件冒泡去决定了——「点了删除却打开了弹窗」这种错在这一层不值得赌。

**草稿箱的列表 = 本地草稿置顶 + 该文件夹里已同步的邮件。** 两者来源不同（前者 `folder_id` 为 NULL、走 `list_drafts`；后者按 `folder_id` 查），所以在 `render_list` 里合并渲染；空态单独写了一条——「点同步拉取」这句在草稿箱里会把人带偏，草稿全在本机、与同步无关。

**服务端没有 `\Drafts` 角色时，界面自己补一行「草稿」。** 本地草稿只挂在「草稿箱」那一屏下（`is_drafts_folder` 判的是**角色**不是名字），而那个入口本来取决于服务端有没有 `\Drafts` 角色的文件夹——多数服务商有，没有的话用户**根本走不到**自己的本地草稿（存了却没处看，等于没有）。所以 `reload_folders` 在文件夹清单**非空**（空 = 还没同步过，此时保持「点「同步」拉取文件夹」那条提示原样）且**没有** `Drafts` 角色时补一行合成行：`const LOCAL_DRAFTS_FOLDER_ID: i64 = -1`——取**负数**，`folders.id` 是 SQLite 的 `INTEGER PRIMARY KEY`（自增、恒正），负值不可能撞上；`name` 只是占位（`folder_row` 对非 `Other` 角色一律用 `role_label` 的本地化名渲染，那个串永远不显示）。这一行**只活在界面内存里**：不落库、不参与同步；点开它只会去查 `folder_id = -1`（恒空）再叠加本地草稿，走的是与真实草稿箱**完全相同**的代码路径。两处护栏：① 补在**算 `still_there` 之前**——不然「用户已经停在草稿箱」会被判成「这个文件夹没了」，一点同步就把人踢回收件箱；② 「移动到文件夹」下拉把它**过滤掉**（它不对应任何 IMAP 文件夹，选它只会让动作队列去 COPY 一个不存在的 uid）。另外草稿行的角标改用**本地草稿数**（`duck-mail` 的 `draft_count`，纯本地 `COUNT(*)`，走 `spawn_local` 不点亮同步按钮）而不是 `folder.unread`——本地草稿的 `folder_id` 是 NULL，压根不在那个 IMAP 文件夹里，照 `unread` 显示的话用户存了一堆草稿角标还是 0。⚠️ 这条对**服务端真有 `\Drafts`** 的那一行同样生效（规则统一为「『草稿』这一行的角标 = 本机草稿数」），代价是那个 IMAP 文件夹自身的未读数不再显示——本仓的草稿就是本机的，对「草稿」这个入口来说未读数没有意义；这是**刻意的取舍**，不是漏改。

> 未做：**把草稿推到服务器的 `\Drafts` 文件夹**（要 `IMAP APPEND`，`drafts_meta.remote_uid` 先留空）、**关弹窗 / 停输入时自动存草稿**（没有去抖自动保存，只有显式「存草稿」按钮）、草稿多选批量删除、草稿 `mode` 的界面呈现（回信的主题本来就带「回复：」前缀，`mode` 目前落库但不显示，留给以后的线程视图）。

### 4.16 会话合并（`threading`，内核已落地 2026-10-13）

**为什么是「整账号重算」而不是增量维护**：线程归属不是一封邮件自己能决定的——它取决于
同账号其它邮件的引用链（祖先可能晚于子信到达，或反过来）。增量维护的每条分支都要处理
「祖先还没到」，越写越难验证。这里选整账号重算：读全部消息 + 引用链 → 跑一遍纯函数
`group_threads` → 删旧线程、写新线程、回填 `messages.thread_id`。纯本地 SQLite 读 +
内存归并，且**幂等**（重算任意次结果一致）。

**引用链从哪来**：`messages` 表**没有**存 `references`（那要改 schema），引用落在既有的
`message_refs` 表。链路是：`mime::parse_headers` 抽 `References` + `In-Reply-To`（归一化去
尖括号、去重、把 `In-Reply-To` 追加进链）→ `sync_folder` 用
`repo::messages::replace_references` 落库 → `repo::threads::rebuild_for_account` 读回来喂给
`group_threads`。**零 schema 迁移**（`threads` 与 `message_refs` 两张表 001 就建好了）。

**算法（JWZ 简化，纯函数）**：并查集 + 幻影节点。
1. `Message-ID` 建索引（重复的合并）；
2. 引用里命中的合并、**没命中的分配「幻影节点」**——让「A、B 都引用不在库里的 X」
   仍能并到一起；
3. `X-GM-THRID` 相同的强制合并（Gmail 服务端算好的，优先于本地推断）；
4. **主题兜底**：只对「无引用线索」且「同主题里至少一条带回复前缀」的消息按规范化主题合并。

**主题兜底为什么比 JWZ 更保守**：JWZ 直接合并同主题的「根」，但国内邮箱里
「月账单已生成」「系统维护通知」这类**周期性、同主题、彼此无关**的邮件会被并成一条
永远增长的超长会话。加两道闸（只并无引用线索的、且要有回复前缀）后，代价是极端的
「回复方丢了 `References` 且前缀也被客户端吃掉」会漏合并——漏合并只是少一层折叠，
**过度合并是错误信息**。

**未接界面**：`duck-app/pages/mail.rs` 的列表仍一封一行（`MessageSummary`）；
`ThreadSummary` 与 `repo::threads::list` 已就绪，接界面是独立一批（碰 `duck-app` 热点）。

### 4.17 OAuth2 登录（G-MAIL-6，内核协议层已落地 2026-10-13）

**落地的是「协议层」，不是「登录界面」**。`src/oauth.rs`（纯协议、全单元测试）覆盖了整条
`authorization_code` + **PKCE(S256)** 链路里所有不需要 GPUI 的部分：

| 项 | 位置 | 说明 |
| --- | --- | --- |
| 服务商端点 | `Endpoints` + `GOOGLE` / `MICROSOFT` / `endpoints(Provider)` | auth/token URL + scope |
| PKCE | `Pkce::generate` / `from_verifier` | `S256` 挑战；对 RFC 7636 附录 B 向量 |
| 授权 URL | `authorization_url(...)` | Gmail 要 `access_type=offline`；Microsoft 强制账户选择器 |
| 回调解析 | `parse_redirect(query)` → `Redirect { code, state }` | 服务商回传的 `error=` 也在此浮出 |
| 令牌响应 | `parse_token_response(body)` → `TokenSet` | 含 `expires_in` 上限校验（≤ 1 年） |
| 换码 / 刷新 | `exchange_code` / `refresh_access_token`（= `post_token_form`） | **阻塞 HTTP（`ureq`）**，同 `duck-db` / `duck-update`；只在非 UI 线程调 |
| SASL | `sasl_xoauth2` / `sasl_xoauth2_base64` | XOAUTH2 初始响应串 |

**凭证怎么接到协议层**：账号档案的 `AuthKind::OAuth2` 让 `ImapConfig` / `SmtpConfig` 走
XOAUTH2 而不是口令——`password` 字段此时承载 **access token**：
- IMAP：`connect()` 走 `client.authenticate("XOAUTH2", XOAuth2 { .. })`（实现
  `async_imap::Authenticator`，base64 由库做），**不是** `login_with_capabilities`；
- SMTP：`authenticate()` 在 `auth == OAuth2` 时发 `AUTH XOAUTH2 <base64>`。服务端不声明
  `XOAUTH2` → **明确 `Unsupported`**，**绝不回落到 `AUTH PLAIN` / `LOGIN`**（把令牌当口令发出去
  只会得到看不懂的失败）。

**Microsoft 的坑**：它的刷新令牌是**单资源**的——刷新时**必须显式带上 scope**，否则换回的 token
受众不是 IMAP/SMTP，认证照样失败。所以 `refresh_form` 的 scope 是可选参数，Microsoft 走「带」、
Google 走「不带」。

**随机数的均匀性**：`random_string` 用**拒绝采样**（接受区间 `66 * 3 = 198`）而不是 `% 66`——
后者会让前 58 个字符多出现一次，对 `state`（防 CSRF）是实打实的强度损失。覆盖性与等权由
`rejection_sampling_covers_every_character_uniformly`（遍历 256 个字节值）**确定性**断言，
不靠随机采样（那约 3% 会假红）。

**零新增依赖**：`ring` / `ureq` 从传递依赖提升为直接依赖，`Cargo.lock` 净变 **+2 行**。

**仍未落地（宿主侧，独立一批，要碰 `duck-app` 热点）**：
- **loopback HTTP 服务器**（收回调）+ **打开浏览器**——按 §4.7 的口径，内核不依赖 GPUI/rfd，
  由 `duck-app` 注入 `open_url` 实现；
- **刷新令牌持久化**——`secret` 里的 `email:{account_id}:refresh_token`（§4.7）；当前只到
  「换到 `TokenSet`」为止，落盘与定时刷新属下一批；
- `feature = "gmail"`（Gmail REST）/ `feature = "jmap"` 仍未做。

---

## 5. 登记清单（精确到文件）

新增一条路由会**同时**产出四个输出面（活动栏图标 / `⌘n` 键位 / 原生菜单 / 命令面板），已由 `⌘8` 的占位登记完成。本次是把占位替换为真实页面，并补齐新动作：

| # | 文件 | 改动 |
| --- | --- | --- |
| 1 | `Cargo.toml`（根） | workspace 成员加 `duck-mail`；`[workspace.dependencies]` 加 `duck-mail` 与 5 个新协议 crate；`rusqlite` 升 `0.40.2`（M0） |
| 2 | `crates/duck-mail/Cargo.toml` + `src/**` | **新增整个 crate** |
| 3 | `crates/duck-app/src/app.rs` | `PlaceholderPage::new("邮箱", …)` → `MailPage::new(...)`；`ShellRoute` 的 `duck-route-email` 接真实视图 |
| 4 | `crates/duck-app/src/pages/mod.rs` | 加 `pub mod mail;`（`EMAIL_ROUTE` 已就位） |
| 5 | `crates/duck-app/src/pages/mail.rs` / `mail_dialogs.rs` | **新增页面与弹窗** |
| 6 | `crates/duck-app/src/actions.rs` | ~~新增 `ActionMeta`：撰写（`⌘N`）、刷新收件箱、标记已读、搜索邮件…~~ **订正（2026-10-13）**：撰写键位改走**页面上下文**（`mail::bind_keys` + `KEY_CONTEXT = "Mail"`），不进 `ActionMeta`——`ActionMeta::keys` 会被 `duck_command::bind_all` 绑到全局，而 `⌘N` 是页内语义。**没有**加动作面板条目：面板里的「撰写」需要能跨页面唤到邮箱页的弹窗，属独立设计（同 `db.rs` 的页内动作也没进面板）。`ids::ROUTE_EMAIL` 已在位 |
| 7 | `crates/duck-app/src/settings.rs` | 设置页新增「邮箱」分类；账号档案读写 |
| 8 | `crates/duck-theme/src/i18n.rs` | EN 表补全部新中文（字节序） |
| 9 | `crates/duck-components/assets/icons/` | 补齐列表/文件夹/标签/回复/转发等图标（`mail.svg` 已在位） |
| 10 | `THIRD_PARTY_LICENSES.md` | 依赖变更后必须重生成（双重门禁） |
| 11 | `docs/database-design.md` / `architecture-gap-analysis.md` | 若有路由/模块表述过时，同步订正 |

**内核边界自查**：`duck-mail` 的 `Cargo.toml` 不得出现 `gpui` / `gpui-component` / `duck-components` / `duck-store`。

---

## 6. 分期实施计划

### M0（前置，独立可回归）——依赖地基
1. **全仓 `rusqlite` 0.32.1 → 0.40.2**，跑通全部现有测试（`duck-store` / `duck-ai` / `duck-db` / `duck-platform`）。
2. 重生成 `THIRD_PARTY_LICENSES.md`（`sbin/licenses.py --check`）。
3. 落地 5 个协议 crate 的准入（写进根 `Cargo.toml` 注释，含 MSRV 依据）。

**验收**：现有全量测试绿 + 许可门禁绿 + `cargo tree` 无 `libsqlite3-sys` 双版本。

### M1 —— 只读邮件（可用的最小闭环）
- `duck-mail` 骨架：`runtime` / `models` / `db`（12 迁移 + 核心 repo）/ `accounts` / `proto::imap` / `mime` / `sync`（主 actor + 首次全量 + 增量 + IDLE）/ `search`（FTS5 + stalwart `nlp` 的中文分词与命中摘要，§2.4①）。
- 账号 CRUD + 连通测试（口令加密落库，**不走系统钥匙串**）。
- `MailPage` 三栏：文件夹树 / 线程列表 / 阅读器（`TextView::html`）/ 附件下载 / 内联图。
- 全文检索（FTS5 + `fold`）。

**验收**：能用真实 IMAP 账号收信、翻列表、读正文（含中文 GBK 邮件与 HTML 邮件）、搜到邮件；重启后增量同步不重复拉取。

> **M1 实际落地 vs 计划**（2026-10-12 复核，2026-10-13 补记）：`runtime` / `models` / `db`（12 迁移 + **4 个** repo）/ `accounts` / `proto::imap`（**只读方向**）/ `mime`（含 CID 内联与远程图片闸门）/ `sync`（**一次性全量 + 增量，不常驻**）/ 检索（FTS5，**中英混排可用、中文召回差**）已落地；**IDLE / 断线重连 / 失败重试 / 附件下载 / 引用折叠 / 自动发现 / 中文分词与 `fold()` 未落地**——逐条证据见 §1.2 的订正表，按「有没有可执行的代码路径」判据降级到 M2/M3，**不改代码去迁就文档**。
>
> ✅ **条件查询语言 / 字段过滤已补（2026-10-13）**：从上一版的「未做」变为已落地（§4.12）。它曾在这一行的未落地清单里，故在此注记离开清单的原因。

### M1.5 —— 整理能力（M2 拆出的第一批，无新依赖）

> 顺序依据：M2 里**只有这一批不需要新增第三方依赖**（`lettre` / `mail-builder` 属发信批），且 `pending_actions` 的 5 态状态机本就为「撤销」留了 `cancelled` 位。

- `proto/imap.rs` 补**写方向**：`UID STORE`（`+FLAGS` / `-FLAGS` / `FLAGS`）、`UID COPY`、`UID MOVE`（无扩展则回退 `COPY` + `+FLAGS \Deleted` + `UID EXPUNGE`）、`UID EXPUNGE`。⚠️ `uid_store` / `uid_expunge` 返回 `impl Stream`，**必须消费到底**否则协议脱同步。
- 新建 `db/repo/labels.rs`（`labels` + `message_labels`）与 `db/repo/queue.rs`（`pending_actions`）。
- 新建 `src/queue.rs`：动作模型 + **乐观更新**（先写本地库让 UI 立即反映）+ **撤销**（宽限期内 `cancel` + 反向复原）。
- `MailCore` 补 `set_starred` / `move_message` / `delete_message` / `label_add` / `label_remove` / `undo_pending` / `flush_pending`。
- UI：阅读器工具条接回旗标 / 移动 / 删除 / 打标签；左栏补「标签」分组；状态栏 8 秒撤销条；列表多选批量。

### M2 —— 收发闭环
- `proto::smtp`（lettre）+ 草稿 + 附件 + 签名 + 发件身份。
- 动作：已读/旗标/移动/删除/标签 + 批量 + 撤销（`queue`）。
- ~~会话合并（`threading`）~~ → **内核已落地（2026-10-13，见 §4.16）**；
  规则路由（`route`）+ snooze/定时发送（`scheduler`）仍未做。
- OAuth2（Gmail / Outlook）+ `feature = "gmail"`。
- `feature = "jmap"`（Fastmail 等）。

**验收**：能发信（含附件）、能整理邮箱、能用 Gmail 账号 OAuth 登录。

> **M2 实际落地 vs 计划**（2026-10-13 复核）。判据同 §1.2：**有没有可执行的代码路径**。
>
> | 计划项 | 实际 | 证据 |
> | --- | --- | --- |
> | `proto::smtp` | ✅ **已落地，但没用 `lettre`** | `proto/smtp.rs`（981 行，含 9 条脚本化服务端测试） |
> | 邮件构造 | ✅ **已落地**。头部手写；**附件（2026-10-13）后** `multipart/mixed` 正文改用 `mail-builder`（头部仍手写，`default-features = false`） | `src/compose.rs`（24 条测试：原有 16 + 附件 8） |
> | 发信门面 | ✅ `MailCore::send` | `lib.rs:382` + `send_inner`（`lib.rs:426`） |
> | 撰写 UI | ✅ 撰写弹窗 + 列表条「写邮件」+ 阅读器「回信」+ **草稿箱点行回填**（三种入口共用一个弹窗，差别只有初值与「存草稿」改存哪一行） | `mail.rs`：`ComposeForm` / `open_compose_dialog_loaded` / `send_compose` / `render_compose_form` |
> | 页内键位 | ✅ `⌘N` → 撰写（`"Mail"` 上下文） | `mail.rs::bind_keys` + `the_compose_key_is_bound_in_the_mail_context` |
> | 动作队列 | ✅ 已落地（含并发竞态修复，见 §4.11 补记） | `src/queue.rs` 161 条测试全绿 |
> | 草稿（`drafts_meta`） | ✅ **已落地（2026-10-13）**：本地草稿 CRUD（`db/repo/drafts.rs`，落 `messages(is_draft=1)` + `message_bodies` + `drafts_meta` + `draft_attachments` **四张既有表**，零 schema 变更）；撰写弹窗左半「存草稿」（不校验收件人、存完不关弹窗、再存改存同一行）；草稿箱列表**置顶本地草稿** + 点行整条回填（含附件重读盘）+ 行内删除；发信成功后顺手删掉该草稿；**侧栏入口**——服务端没有 `\Drafts` 角色时界面补一行合成「草稿」（id `-1`，只在内存里），角标用本地 `draft_count` 而非 `folder.unread`。§4.15 | `db/repo/drafts.rs`（13 条测试）；`mail.rs::save_draft` / `open_draft` / `delete_draft` / `draft_row` / `reload_drafts` / `reload_draft_count` / `synthetic_drafts_folder` |
> | 附件（收） | ✅ **已落地（2026-10-13）**：阅读器每行「另存为…」→ `MailCore::download_attachment` → 整封取回 + MIME 解码 + 落盘 + 记 `file_path`；同一位置重复保存不连网。§4.13 |
> | 附件（发） | ✅ **已落地（2026-10-13）**：`Draft::attachments` + `OutgoingAttachment`（`from_path` 读盘 + 扩展名猜类型）；撰写弹窗「附件」区（原生多选 + 逐条移除 + 同名去重）；`multipart/mixed` **正文**交给 `mail-builder`（`default-features = false` → 零新增传递依赖），**头部仍手写**。§4.14 | `compose.rs::build_multipart_body`；`mail.rs::render_compose_attachments` / `pick_attachments` |
> | 签名 / 发件身份选择 | ❌ **未落地**。`sender_identities` 表在、repo 无；发件身份只有「当前账户」一个 | — |
> | 写回「已发送」文件夹 | ❌ **未落地**（要 `IMAP APPEND`，`proto/imap.rs` 没有） | 界面已常驻一行说明告知用户 |
> | `threading` | 🔶 **内核已落地（2026-10-13，G-MAIL-7，见 §4.16）**：`src/threading.rs` JWZ 合并 + `db/repo/threads.rs` 整账号重建 + `mime.rs` 提引用链 → `message_refs` + `sync.rs` 收尾重算；**零 schema 迁移**。未接界面 | `src/threading.rs`、`db/repo/threads.rs`、`mime.rs`、`sync.rs` |
> | `route` / `scheduler` | ❌ **未落地**。`split_rules` / `snoozes` 表在、repo 无 | — |
> | OAuth2 | 🔶 **内核协议层已落地（2026-10-13，G-MAIL-6，见 §4.17）**：PKCE(S256) + 授权 URL + 回调解析 + 令牌交换/刷新 + XOAUTH2 SASL；`ImapConfig`/`SmtpConfig` 按 `AuthKind::OAuth2` 走 XOAUTH2（**绝不回落到口令机制**）。**宿主侧未接**：loopback 回调服务器 / 打开浏览器 / 刷新令牌持久化 | `src/oauth.rs`、`proto/imap.rs`（`XOAuth2`）、`proto/smtp.rs` |
> | `feature = "jmap"` / `feature = "gmail"` | ❌ **未落地** | — |
>
> ### 为什么发信没有引入 `lettre`（以及 `mail-builder` 最终只用在正文上）
>
> §4.2 的准入结论是「依赖尽量零新增」，而**纯文本**发信真正需要的只有三件事：**头部装配**（含 RFC 2047 编码与非 ASCII 地址）、**SMTP 状态机**（EHLO / STARTTLS / AUTH / MAIL / RCPT / DATA / QUIT + 点填充）、**分块编码**。前两件在 `compose.rs` 与 `proto/smtp.rs` 里做完，第三件本批不需要（纯文本正文走 `8bit`）。于是那一批 `Cargo.toml` 与 `THIRD_PARTY_LICENSES.md` **一行未动**。
>
> 原文在这里写着「一旦要附件 / 富文本……那时应当**重新评估 `mail-builder`**」。**附件那一批照这句话做了**：`multipart/*` 的构造确实不是「再写两百行」的量级（边界生成、按内容选 `Content-Transfer-Encoding`、base64 折行、非 ASCII 文件名的参数编码），写错的后果还不是报错，而是**对方收到一个打不开的附件**。于是引入 `mail-builder`——但**只让它装配正文**，头部与「无附件」的单部件路径仍是手写的（完整取舍见 §4.14）。`lettre` 仍然不引：SMTP 状态机与「`StartTls` 档下服务端不声明 `STARTTLS` 就拒绝降级」那条口径，本仓自己那份更直接。
>
> ### 几个刻意的实现口径
>
> - **`Plain`（明文）档直接 `MailError::Unsupported`**：不是「不支持这个功能」，而是**不肯把口令明文发出去**。`StartTls` 档下服务端不声明 `STARTTLS` 时**拒绝降级**，同上。
> - **本地拒发**：收件人为空、或正文超过服务端 `SIZE` 声明值时，在发 `MAIL FROM` **之前**就返回错误——不必等服务端 552。
> - **`QUIT` 无论成败都发**：失败时直接丢连接会让服务端留一条半开会话。`quit` 自己吞错误，所以不会盖掉真因。
> - ~~**`In-Reply-To` / `References` 只有一环**~~ → **已修（2026-10-13，G-MAIL-7）**：`messages`
>   表确实没存 `references`，但**无需改 schema**——引用链落既有的 `message_refs` 表
>   （`mime.rs` 解析 `References` + `In-Reply-To` → `sync` 落库 → `threading` 合并 →
>   `threads` + `messages.thread_id`）。发信侧 `compose` 早已写这两个头，收信侧现在能读回来了。
> - **发信不进动作队列**：队列语义是「先本地生效、稍后补发」，而本模块没有本地发件箱——发信没有「本地生效」那一半（§4.11）。
> - **附件另存走「整封取回」**：不做 `BODYSTRUCTURE` 解析、不按段 `FETCH`，理由与代价见 §4.13。
> - **附件走 `multipart/mixed`，正文段的编码随内容变**：纯 ASCII 正文段是 `7bit` + 原文，含非 ASCII 是 `base64`（库的判定，已用测试钉住）；顶层 `Content-Type`（含 boundary）也由库写，但**位置仍在头部块最后**（§4.14）。
> - **内核错误详情一律英文**：`error.rs` 的 `Display` 加英文前缀，界面用 `trf` 包中文外壳。**这条由 `duck-theme` 的 i18n 守卫强制**（内核无 EN 表可登记，写中文即红）。

### M3 —— 智能与自动化
- `ai.rs`：摘要 / 智能回复 / 起草 / 校对 / 意图 / RAG 问答（复用 `duck-ai`），流式进阅读器右栏。
- `autolabel` + AI 自动分类与自动规则（`AiAutomationRule`）；结构化输出走分隔符 + 索引解析（§2.4③）。
- 本地零 token 分类备选：搬 stalwart `nlp` 的 `classifier/`（FTRL 在线学习，引入 rkyv，§2.4①）。
- AI 用量统计页；提示注入防护（`clean_untrusted`）。
- 联系人学习（`contacts` repo + 通讯录视图）。
- 可选本地向量（`feature = "local-embeddings"`）。

**验收**：能对线程一键摘要、生成回复草稿、用自然语言问"上周张三发的发票在哪"并给出来源引用。

### M4 —— 生态与安全
- CalDAV/CardDAV（**先解决 `quick-xml` 的 MSRV 1.86 越线**）、Google Calendar。
- OpenPGP（`sequoia` / `gpgme`）、DKIM 校验显示。
- 暴露给 JS 插件（新增 `Capability` 位与宿主 API）。
- 可能的 POP3 / Exchange（Graph）。

---

## 7. 与项目约定的符合性自查

| 约定 | 出处 | 符合性 |
| --- | --- | --- |
| 内核零 GPUI | AGENTS §1 | ✅ `duck-mail` 不依赖 `gpui`/`duck-components` |
| tokio 不外泄 | `duck-db` §4.1 先例 | ✅ 封在 `runtime.rs`，CI grep 守卫 |
| MSRV 1.85 硬门 | `Cargo.toml` | ✅ 逐包实测；`quick-xml 0.42`（1.86）已明确排除。附件批的 `mail-builder 0.4.4` 声明 `rust_version` 为空（未声明）、edition 2021，`default-features = false` 后无传递依赖 |
| 凭据加密落程序自有存储、明文不落 kv | `duck-db` §4.3 / `duck-redis`（**2026-10-12 改口径**） | ✅ §4.7 |
| 写路径单事务 | `duck-db` 纪律 | ✅ §4.5 |
| 取消必须带外 | `duck-db` 纪律 | ✅ 同步 worker 走 `AtomicBool` + 关连接；IMAP `stop-token` |
| 依赖尽量零新增 | 根 `Cargo.toml` 注释 | ✅ M1 实际只新增 2 个（`async-imap` / `mail-parser`）；`tokio`/`base64` 由传递依赖提升（零下载）。**附件批（2026-10-13）新增 `mail-builder`**，但 `default-features = false` 关掉 `gethostname` → `Cargo.lock` 净变更只有它一条，仍**零新增传递依赖**；`THIRD_PARTY_LICENSES.md` 已重生成并通过 `--check`（958 → 959 个组件）。~~`lettre`~~ 始终未引 |
| 中文文案补 EN 表 | AGENTS §3.3 | ✅ 登记清单 #8；发信批 +22 条（1745 → 1767）、检索语法批 +15 条（1767 → 1782）、附件另存批 +3 条（1782 → 1785）、发信附件批 +5 条（1785 → 1790）、**草稿批 +15 条（1820 → 1835——起点 1820 含并发会话在同一时段加进的条目，所以与上一段的 1790 不连续）、草稿可达性收尾 +1 条（`文件夹 {}，{} 封草稿`）**，`duck-theme` 双守卫绿（`table_is_sorted` 字节序 + `every_chinese_literal_is_translated` 零缺失） |
| ⚠️ 含中文的文案不许写进「带转义引号的字面量」 | `i18n.rs::literals()` 的实现方式 | ✅ 检索语法表左列因此**一个字都不带中文**（示例用 `"..."` / `report*`），并由 `search_syntax_rows_keep_the_ui_translatable` 钉住。守卫取的是源码行里引号之间的**原文**、表里的键是**已解析**的，`\"…\"` 这类永远对不上，**没法靠登记条目消掉** |
| 界面不留演示项（P3-6） | `architecture-gap-analysis.md` | ✅ 新增的「搜索语法」弹出的是**真实语法表**（`SEARCH_SYNTAX_ROWS` 与内核词法逐条对齐），不是占位说明 |
| 图标 Lucide 1.75 | AGENTS §3.6 | ✅ 登记清单 #9 |
| 主题走 `duck_theme` | AGENTS §3.7 | ✅ 页面只用 token |
| 弹窗 `.close_button(false)` | 项目记忆 | ✅ §4.10 已列；撰写弹窗同（`open_compose_dialog`） |
| 提示文案只列真绑定的键 | 项目记忆 | ✅ 撰写键位已落地（`⌘N`，`"Mail"` 上下文）；「刷新收件箱」**刻意没有**页内键位（`⌘R` 归全局 `toolbox.refresh`，要压掉它属产品取舍），所以它的提示里也不写键位 |
| 页面级键位不进 `ActionMeta::keys` | `actions.rs` 的同类说明 | ✅ `mail::bind_keys` 只绑 `"Mail"` 上下文；`ActionMeta::keys` 会被 `bind_all` 绑到全局 |
| 内核错误详情不写中文 | `error.rs` 模块头「不吐成品中文」 | ✅ 由守卫强制：内核没有 EN 表可登记，中文详情会让 `cargo test -p duck-theme` 直接红（本轮真的红过一次，改回英文才对）。附件下载的「内容不一致」走 `MailError::Mime` + 英文详情 |
| 原生模态对话框只在动作处理器里调 | `duck-platform/src/dialog.rs` 模块头 | ✅ 「另存为」挂在附件行的 `on_click`、「添加附件」挂在按钮的 `on_click`——`save_file` / `open_files` 都是原生模态，都不在 render 里（`save_attachment` / `pick_attachments` 的文档各写明了） |
| 内核抛出的失败要给**可分辨**的 `code()` | 本设计 §4.14 | ✅ `OutgoingAttachment::from_path`：读不出来 → `MailError::Io`（`io`）、路径没有末段 → `MailError::Config`（`config`）。**不套 `Other`**——套了 `code()` 就压成 `other`，界面按错误类型选文案的能力全丢 |
| 部分失败不连坐 | 本设计 §4.14 | ✅ 一次多选里有一个读不出来：只记**第一条**原因去提示，其余读成功的照常挂上；同名附件只留一份（均有测试） |
| 幂等 / 不留半个文件 | 本设计 §4.13 | ✅ 落点相同且文件在 → 不连网；字节备齐后一次 `fs::write`；失败时目标路径为空（均有测试） |
| 草稿的 `folder_id` 必须为 NULL | 本设计 §4.15 | ✅ `drafts::save` 插行时写 `NULL`；由此「按文件夹列邮件」与「同步清空该文件夹」都不会碰草稿。⚠️ **代价是检索必须显式排除草稿**：`search()` 补了 `is_draft = 0` 基谓词——草稿若被捞出来，回填 `MessageSummary` 那个**非空 `i64`** 的 `folder_id` 会直接 `InvalidColumnType`；用 `has:attachment`（纯过滤器、不依赖 FTS）双向钉住（有测试） |
| 草稿的 `Message-ID` 只生成一次 | 本设计 §4.15 | ✅ 改存沿用旧值（`drafts::save` 的 `existing_message_id`）；否则每存一次草稿就换 id，别人引这一封的 `In-Reply-To` 会悬空（有测试） |
| 幂等 / 不留孤儿行（草稿） | 本设计 §4.15 | ✅ 附件与引用链**整体替换**（先删后写）——只做增量的话「移除一个附件」留不下痕迹；删草稿靠 `ON DELETE CASCADE` 一条 `DELETE` 带走四张子表（有测试） |
| 草稿存储**不存字节** | 本设计 §4.15 | ✅ `draft_attachments` 只有 `file_path`（表结构如此），打开草稿**重新读盘**；读不出来的**不阻断**打开，只少一个附件并把条数报出来。界面把「已读字节 + 源路径」配对存着（`ComposeAttachment`），路径在**选文件那一刻**记下 |
| 本地草稿必须**有入口**（无 `\Drafts` 时补合成行） | 本设计 §4.15 | ✅ `LOCAL_DRAFTS_FOLDER_ID = -1` 的合成行：`folders` 非空且无 `Drafts` 角色时补，且补在算 `still_there` **之前**（否则「已停在草稿箱」会被判成「文件夹没了」）；「移动到文件夹」下拉过滤掉它；角标改用本地 `draft_count` 而非 `folder.unread`（后者对本地草稿恒为 0）。⚠️ 这一行只在内存里，不落库、不参与同步 |

---

## 8. 未决项（需你拍板或后续验证）

1. **M0 的 rusqlite 升级实测结论**——是全仓升 0.40（本设计推荐）还是把 `duck-mail` 降到 0.32？建议先做 30 分钟 spike：只改 `Cargo.toml` 后 `cargo check` 全 workspace，看真实报错量。
2. **HTML 渲染的"够用"边界**：`gpui-base` 的 HTML 渲染是**文档模型**（无语义 CSS 布局）。营销邮件大量用 `<table>` + 内联 CSS，复杂营销邮件的列布局无法还原——已用「表格布局拍平」（§4.8）保证可读；若仍不够，退路是"纯文本优先 + 一键在浏览器打开"。
3. ~~**远程图片默认策略**~~ **已定（2026-10-13，用户拍板）**：默认放行 + 顶部提示条说明代价（追踪像素），提示条上可一键收回。
4. **是否接受 `ammonia` 新增**：实测 `gpui-base` 解析器已丢弃 `<script>`/`<style>` 且不执行 JS/CSS（§4.8），**建议 M1 不引**，改用"解析器丢弃 + 链接协议白名单 + 远程图片开关"。若后续发现属性级注入风险再补。
5. **邮箱偏好放哪**：`Appearance` 子键 vs `app_settings` 表（§4.11）。
6. **多账号并发上限**：每个账号 1 主连接 + IDLE + 回填，N 个账号的连接数需要设上限（建议默认 3 账号活跃同步）。
7. **AI 是否允许"一键发送"**：建议**禁止**（只出草稿），需你确认。
8. **本地向量是否上**（`candle-*` 体积与模型下载）——建议 M3 再定。
9. **stalwart `nlp` 的搬运范围**（§2.4①）：M1 只搬 `language/` + `tokenizers/`（新增 jieba-rs / whatlang / rust-stemmers / xxhash-rust 等 ~5 个依赖，`rand 0.10.2` 的 MSRV 恰卡 1.85 需在 M0 的 spike 里实测解析）是否接受？还是 M1 先用「逐字 bigram」凑合、M3 再整搬？影响 M1 的依赖准入数量。
10. ~~UI 效果图定稿~~ **已按效果图落地**（2026-10-12，见 §4.10「布局落地状态」）。~~唯一悬空的邮箱列表行高~~ **已定案**：新增 `DensityTokens.mail_row`（56 / 64 / 76，标准档 64 即效果图值），`mail.rs` 改取该 token，页面内不再留行高常量。理由与三档取值见 §4.10 第 3 条。
11. ~~M0 的 rusqlite 是否升级~~ **不升级**（2026-10-12）：`duck-mail` 直接复用全仓既有的 `rusqlite 0.32.1`，M0 从「全仓升 0.40」缩到零改动——省掉了全 workspace 的破坏面评估，代价是暂时用不上 0.40 的新 API（邮箱用不到）。
12. **内核错误详情的本地化**（2026-10-13 记）：内核按约定只吐**英文**详情（`error.rs`），界面用 `trf(cx, "…失败：{}", error)` 包一层中文外壳——于是中文界面下会看到「保存附件失败：connection failed: …」这样的中英混排。`MailError::code()` 早就给出了 14 个语言无关的短标识，但**宿主一次都没用过**。要不要做一张 `code() → 本地化文案` 的表、把 `mail.rs` 里那些 `…失败：{}` 包装点（**现 25 处**；附件批 +1「有附件读不出来：{}」，草稿批 +4「读/打开/保存/删除草稿失败：{}」）改成查表？涉及「同一 code 在不同调用点要有不同措辞」（`not_found` 在取正文与存附件里说的不是一件事），需要一层「调用点 + code」的二维映射，不是一张平表能解决的——**建议在 M2 收尾时统一做，而不是零散地补**。

---

## 9. 风险登记

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| `rusqlite` 0.32→0.40 破坏面超预期 | M0 阻塞 | 先 spike；兜底方案见 §4.3 |
| 复杂 HTML 邮件渲染不还原 | M1 体验 | 纯文本优先 + 浏览器打开逃生门；M3 再评估 Blitz/WebView |
| `async-imap 0.11` 未声明 MSRV，实际可能需 >1.85 | 构建失败 | M0 实测（本机 rustc 1.98 能编，但 CI 走 stable，风险低；若失败则锁版本或换 `imap` 同步栈） |
| 移植面过大（72k 行）导致失控 | 进度 | **按 M1 只搬必需模块**（accounts/db/imap/mime/sync/search），日历/通讯录/CalDAV 全推到 M3+ |
| 服务器实现差异（中文邮箱常见非标准行为） | 同步异常 | 保留 Flectar 的 MIME 启发式修复；`sync_failures` 可观测 |
| 邮件内容作为 AI 输入被注入 | 安全 | `clean_untrusted` + 工具审批闸门 + AI 不自主发信 |

---

## 附：本设计引用的实测证据

- `gpui-base 0.7.1` 的 HTML 渲染：`src/text/mod.rs:43`、`src/text/text_view.rs:237/265`、`src/text/state.rs:167`、`src/text/format/html.rs`；其中 `<script>`/`<style>` 被显式丢弃（`format/html.rs:594`），`cleanup_html` 仅压缩（`format/html.rs:83`）。
- `flectar-mail-core` 体量：`src/` 合计 72,834 行（`db/` 15,283 · `sync/` 9,690 · `jmap/` 4,048 · `mime/` 2,987 · `oauth/` 2,577 · `models.rs` 1,947 · `ai/` 1,731 · `imap/` 1,597 · `route/` 1,390 · `queue/` 1,197 · `accounts/` 862 · `search/` 438 · `smtp/` 371）。
- 依赖实测（crates.io API，2026-10-12）：`lettre 0.11.23` MSRV 1.85；`ammonia 4.1.7` MSRV 1.80；`quick-xml 0.42.0` MSRV **1.86**；`async-imap 0.11.3` / `mail-parser 0.11.8` / `mail-builder 0.4.4` / `jmap-client 0.4.2` / `rusqlite 0.40.2` 未声明 MSRV。
- 依赖图现状（`cargo tree --locked`）：`tokio 1.53.2` 已在图（`postgres → tokio-postgres`）；`html5ever 0.27.0` 已在图（`gpui-base`）；`encoding_rs 0.8.42` / `base64 0.22.1` / `chrono 0.4.45` 已在图；`reqwest` / `hyper` **不在图**。
- rusqlite 冲突：`rusqlite 0.32.1 → libsqlite3-sys ^0.30.1`、`rusqlite 0.40.2 → libsqlite3-sys ^0.38.2`，均 `links = "sqlite3"`。
- `temp/stalwart`：**已拉全**（v0.16.26，28 crate 约 33.9 万行）；复核结论见 §2.4——`nlp` 零内部耦合可整搬、`imap-proto` 为服务端视角、Docker 可当本地测试服务器。
