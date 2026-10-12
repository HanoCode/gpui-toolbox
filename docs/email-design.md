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
| 写信 | 富文本/纯文本撰写、附件、草稿自动保存、签名、发件身份 | M2 |
| 整理 | 已读/旗标/移动/删除、标签、文件夹管理、批量操作、撤销 | M2 |
| 智能 | 线程摘要、智能回复、起草、校对、意图识别、RAG 问答、自动分类/打标、AI 用量统计 | M3 |
| 自动化 | 规则路由（发件域/关键词）、snooze、定时发送、退订识别 | M2 / M3 |
| 通讯录 | 联系人学习、CardDAV | M3 / M4 |
| 日历 | CalDAV / Google Calendar、会议邀请解析 | M4 |
| 安全 | 凭据加密落程序自有存储、远程内容拦截、HTML 净化、DKIM 显示、OpenPGP | M1 / M4 |
| 生态 | 暴露给 JS 插件（`duck.fetch` 之外新增邮件能力位） | M4 |

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
    │   ├── imap.rs          # async-imap 封装
    │   ├── smtp.rs          # lettre 封装
    │   ├── jmap.rs          # (feature = "jmap")
    │   └── gmail.rs         # (feature = "gmail"，REST + ureq)
    ├── mime/                # mail-parser 计划、正文/附件解码、CID 内联、mail-builder 构造
    ├── sync/
    │   ├── engine.rs        # per-account actor（spawn_account / SyncCmd / SyncCtx）
    │   ├── folder_map.rs
    │   ├── threading.rs     # 会话合并
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
| `mail-builder` | 0.4.4 | MIME 构造 | **未声明** | 新增 | ✅ 准入 |
| `ammonia` | 4.1.7 | HTML 净化 | **1.80** | 新增 | ⏸ **倾向不引**：实测 `gpui-base` 解析器已丢弃 `<script>`/`<style>`（见 §4.8），首版可不加 |
| `jmap-client` | 0.4.2 | JMAP | **未声明** | 新增 | ⏸ M2，`feature = "jmap"` |
| `quick-xml` | 0.42.0 | CalDAV/CardDAV | **1.86 ❌** | 新增 | ⛔ **超线**；M4 且需降版（≤0.37）或换 XML 方案 |
| `candle-core/nn/transformers` | 0.11 | 本地向量嵌入 | 未声明 | 新增 | ⏸ M3，`feature = "local-embeddings"`（Flectar 默认关） |
| `reqwest` | 0.13.5 | Flectar 用于 CalDAV/JMAP | 1.85 | **不在图**（`cargo tree -i reqwest` 空） | ⛔ **M1/M2 不引**，改用已在图的 `ureq` |

**导入纪律**：`duck-mail` 首版（M1）**只新增 4 个第三方 crate**（`async-imap` / `lettre` / `mail-parser` / `mail-builder`；`ammonia` 经实测可不引，见 §4.8），其余全部复用既有图。升级 `rusqlite` 会带来 `libsqlite3-sys 0.30.x → 0.38.x` 的替换（同一 crate，不是新增）。

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
  → ③ TextView::html(id, cleaned) + .image_source(resolver)
         resolver 拦截远程 http(s) 图片：按「远程内容」开关决定放行或占位
```

**净化层的实测结论**（决定要不要引 `ammonia`）：

| 事实 | 位置 | 含义 |
| --- | --- | --- |
| `<script>` / `<style>` **在解析时被显式丢弃**（匹配分支直接返回 `None`） | `src/text/format/html.rs:594` | 脚本注入面已被消除 |
| `cleanup_html()` 只做**压缩**（`html5minify::Minifier`），**不是**安全净化 | `src/text/format/html.rs:83` | 不要把它当 sanitizer |
| 渲染目标是**文档模型**（`ParsedDocument`），不是浏览器 | `src/text/format/html.rs` | 无 JS 执行、无 CSS 布局引擎、无 `iframe`/外部资源加载 |

即：**XSS 面几乎不存在**（不执行脚本、不跑 CSS），首版**可以不引 `ammonia`**。仍需自建的部分是**属性级白名单**（`on*` 事件属性、`javascript:` 协议链接）——`TextView` 的链接点击是宿主回调（`link_click_handler`），在回调里做协议校验（只放行 `http`/`https`/`mailto`）比全量净化更省。**M1 建议：不引 `ammonia`，改用"解析器自带丢弃 + 链接协议白名单 + 远程图片开关"三层。**

容量上限沿用 Flectar 的实测值：单图 8 MB / 单信 16 MB / 单线程 32 MB，内联图 ≤ 32 张；`.image_source` 返回的 `data:` URL 由 `ImageNode.embedded: OnceLock` 解码一次后复用（不会每帧重解码）。

**纯文本邮件**：`TextView` 直接渲染；HTML 邮件若有 `multipart/alternative` 的 text/plain，默认优先展示纯文本（更安全），用户可一键切 HTML。

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
| 中栏 | 列表条（文件夹名 + 计数 + 同步按钮）+ 邮件列表（未读圆点 / 发件人 / 附件夹 / 时间 / 主题 / 摘要） |
| 阅读器 | 工具条（标为已读 · 未读 ┃ 下载正文）+ 头部（主题 / 发件人 / 时间 / 收件人 / 抄送 / 大小）+ 正文 + 附件清单 |
| 右栏 | **无**（见下第 1 条） |

三处与效果图的**有意**差异（同已落进代码注释）：

1. **前一版「禁用 + 说明」的控件全部删除，而不是接线**。前一版按 `redis.rs` 弹窗「M1.x 接入」的既有口径保留了 8 个阅读器动作 + 右栏 AI 面板（禁用 + 悬停写明等哪一期）。2026-10-12 的 **P3-6**（`docs/architecture-gap-analysis.md`）把这种「自我标注为待接入」判为不可保留——「界面不应再有演示数据 / 演示文字，每个图标的功能都需要实现完整」，处置判据是**后端存在 → 接线；后端不存在 → 删除**。于是：左栏添加 / 移除账户、列表条同步、阅读器已读切换与下载正文**接线**；标签分组、筛选、回复 / 转发 / 归档 / 稍后 / 删除 / 打标签 / 旗标 / AI 分析 / 更多、以及右栏 AI 面板**删除**（前提都在 M2）。M2 把发信、动作队列与 AI 接通后，这些入口按同一份判据接回来。
2. **左栏宽度取 236，不是效果图的 216**：与 `db.rs` / `redis.rs` / `terminal.rs` 走同一条档位（184–340，默认 236），四页互相切换时侧栏不跳宽。效果图的 216 只是示意。
3. **纵向尺寸一律走 `DensityTokens`，本页只留横向分配比例**。原先「邮件列表行高」是本页常量 64 并注明待定——现在收口为**新增的 `density.mail_row`（56 / 64 / 76）**：`task_row` 最宽只有 52，且语义是「双行任务」，压不住三行邮件。同理左栏行取 `density.nav_row`、分组标题取 `density.section_header`，否则调密度设置时邮箱页不动。

配套改动：`duck-mail` 新 crate（账户 CRUD + 凭据加密 + SQLite + IMAP + MIME + 增量同步 + FTS5，91 个测试）；`duck-components/assets/icons/` 新增 10 个 Lucide 1.48.0 图标（`inbox` `star` `reply` `forward` `paperclip` `tag` `sparkles` `pencil` `at-sign` `flag`，stroke 1.75）；`duck-theme` 新增 `mail_row` 密度档。

### 4.11 离线优先、撤销与定时

- **动作队列** `pending_actions`：所有远端动作（标已读/移动/删除/发信）先写本地库并乐观更新 UI，再由 worker 补发；失败进 `sync_failures` 并可重试。
- **撤销**：删除/移动走"软删除 + 宽限期"（本地标记，宽限内可撤销，超时才真正提交远端）。
- **snooze / 定时发送**：`snoozes` 表 + `scheduler::spawn`（整点扫描 + `notification_outbox`）。
- **外观**：`mail` 的设置（列表密度、是否加载远程图片、HTML/纯文本偏好）一律进新增的 `Appearance` 子键或 `app_settings` 表——注意本项目约定「布局/密度类设置进 `Appearance`」，但**邮箱专属偏好**（远程内容开关等）留在 `app_settings` 更合适，需在评审时定。

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
| 6 | `crates/duck-app/src/actions.rs` | 新增 `ActionMeta`：撰写（`⌘N`）、刷新收件箱、标记已读、搜索邮件…（`ids::ROUTE_EMAIL` 已在位） |
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

### M2 —— 收发闭环
- `proto::smtp`（lettre）+ 草稿 + 附件 + 签名 + 发件身份。
- 动作：已读/旗标/移动/删除/标签 + 批量 + 撤销（`queue`）。
- 会话合并（`threading`）+ 规则路由（`route`）+ snooze/定时发送（`scheduler`）。
- OAuth2（Gmail / Outlook）+ `feature = "gmail"`。
- `feature = "jmap"`（Fastmail 等）。

**验收**：能发信（含附件）、能整理邮箱、能用 Gmail 账号 OAuth 登录。

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
| MSRV 1.85 硬门 | `Cargo.toml` | ✅ 5 个新依赖全部实测；`quick-xml 0.42`（1.86）已明确排除 |
| 凭据加密落程序自有存储、明文不落 kv | `duck-db` §4.3 / `duck-redis`（**2026-10-12 改口径**） | ✅ §4.7 |
| 写路径单事务 | `duck-db` 纪律 | ✅ §4.5 |
| 取消必须带外 | `duck-db` 纪律 | ✅ 同步 worker 走 `AtomicBool` + 关连接；IMAP `stop-token` |
| 依赖尽量零新增 | 根 `Cargo.toml` 注释 | ✅ M1 仅新增 5 个；`tokio`/`ureq`/`encoding_rs`/`base64`/`chrono` 全复用（~~`keyring`~~ 已于 2026-10-12 从图内删除） |
| 中文文案补 EN 表 | AGENTS §3.3 | ✅ 登记清单 #8 |
| 图标 Lucide 1.75 | AGENTS §3.6 | ✅ 登记清单 #9 |
| 主题走 `duck_theme` | AGENTS §3.7 | ✅ 页面只用 token |
| 弹窗 `.close_button(false)` | 项目记忆 | ✅ §4.10 已列 |
| 提示文案只列真绑定的键 | 项目记忆 | ✅ 撰写/刷新键位落地后核对 |

---

## 8. 未决项（需你拍板或后续验证）

1. **M0 的 rusqlite 升级实测结论**——是全仓升 0.40（本设计推荐）还是把 `duck-mail` 降到 0.32？建议先做 30 分钟 spike：只改 `Cargo.toml` 后 `cargo check` 全 workspace，看真实报错量。
2. **HTML 渲染的"够用"边界**：`gpui-base` 的 HTML 渲染是**文档模型**（无语义 CSS 布局）。营销邮件大量用 `<table>` + 内联 CSS，需实测复杂营销邮件样式还原度；若不够，退路是"纯文本优先 + 一键在浏览器打开"。
3. **远程图片默认策略**：默认拦截 + 顶部提示条（Flectar 同款）还是默认放行？涉及隐私（追踪像素）与体验的取舍。
4. **是否接受 `ammonia` 新增**：实测 `gpui-base` 解析器已丢弃 `<script>`/`<style>` 且不执行 JS/CSS（§4.8），**建议 M1 不引**，改用"解析器丢弃 + 链接协议白名单 + 远程图片开关"。若后续发现属性级注入风险再补。
5. **邮箱偏好放哪**：`Appearance` 子键 vs `app_settings` 表（§4.11）。
6. **多账号并发上限**：每个账号 1 主连接 + IDLE + 回填，N 个账号的连接数需要设上限（建议默认 3 账号活跃同步）。
7. **AI 是否允许"一键发送"**：建议**禁止**（只出草稿），需你确认。
8. **本地向量是否上**（`candle-*` 体积与模型下载）——建议 M3 再定。
9. **stalwart `nlp` 的搬运范围**（§2.4①）：M1 只搬 `language/` + `tokenizers/`（新增 jieba-rs / whatlang / rust-stemmers / xxhash-rust 等 ~5 个依赖，`rand 0.10.2` 的 MSRV 恰卡 1.85 需在 M0 的 spike 里实测解析）是否接受？还是 M1 先用「逐字 bigram」凑合、M3 再整搬？影响 M1 的依赖准入数量。
10. ~~UI 效果图定稿~~ **已按效果图落地**（2026-10-12，见 §4.10「布局落地状态」）。~~唯一悬空的邮箱列表行高~~ **已定案**：新增 `DensityTokens.mail_row`（56 / 64 / 76，标准档 64 即效果图值），`mail.rs` 改取该 token，页面内不再留行高常量。理由与三档取值见 §4.10 第 3 条。
11. ~~M0 的 rusqlite 是否升级~~ **不升级**（2026-10-12）：`duck-mail` 直接复用全仓既有的 `rusqlite 0.32.1`，M0 从「全仓升 0.40」缩到零改动——省掉了全 workspace 的破坏面评估，代价是暂时用不上 0.40 的新 API（邮箱用不到）。

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
