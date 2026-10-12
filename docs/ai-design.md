# Duck Toolbox AI 助手模块架构设计（duck-ai）

> 状态：设计稿待确认（2026-10-12）。参考物：temp/zed、temp/oxideterm-ai、gpui-component 0.7.1。
> 用户已拍板三项：① **先出设计文档**，确认后再动代码；② **多 provider 模板**（非单一端点）；
> ③ 首批工具集 = `read_file` / `list_dir` + `search_kb` + `run_sql_readonly`，
> 且 **AI 厂商设置放进「设置」页，不放在 AI 助手页内**。

---

## 1. 目标与范围

- 把占位路由 `assistant`（`ASSISTANT_ROUTE`，快捷键 `⌘7`）实装为**可用的 AI 助手**：
  多会话对话、流式输出、Markdown 渲染、工具调用、知识库检索、历史持久化。
- **多种模型厂商**：以 provider 模板形式内置主流厂商，统一走 OpenAI 兼容协议（见 §4.3）。
- **工具能力受限且可审批**：只读工具自动执行，任何写操作经审批闸门（见 §4.6）。
- **本地知识库检索**：基于 SQLite FTS5，索引本机文本资产（见 §4.7）。
- 不做的事：不做云同步、不做 MCP 服务端、不做多代理（subagent）编排、
  不做 Anthropic / Gemini 的**原生协议**（M2 再评）、不做语音输入（`speech` 组件已具备，但不在本期）。

### 1.1 为什么是独立 crate，而不是 `duck-db` 的 M3

`docs/database-design.md` §3 的备选方案与 §4.8 原计划把 AI 放在 `duck-db::ai`（M3），
并把 `duck-ai` 记作「等 ai 真正落地且 sqlintel 被外部消费时再拆」。**本次按那个条件正式拆出**，理由三条：

1. **消费者已超过一个**。原规划里 AI 只服务 text-to-SQL；现在它同时要服务终端、Redis、
   插件与知识库——已经是**平台级能力**，不是数据库模块的子功能。
2. **协议层与领域层分离**。`duck-db::ai` 混装了两件事：OpenAI 兼容协议的 HTTP/SSE/工具调用
   （通用），与 text-to-SQL 的提示词与只读证明闸门（SQL 领域）。前者属 `duck-ai`，后者留 `duck-db`。
3. **不扰动在编文件**。`db.rs`（4200+ 行）与 `duck-db` 正处于多轮并发编辑中；
   独立 crate 把改动面收敛为「新 crate + 登记点 + 一处薄适配层」，风险最低。

**收敛路径**（§4.9 详述）：`duck-db/src/ai.rs` 的 `AiProvider` trait 与 `OpenAiCompatible`
上移为 `duck-ai` 的实现，`duck-db` 改为依赖 `duck-ai`，调用点签名保持不变。

## 2. 参考项目评估

### 2.1 三源分工（不是单一项目照搬）

| 参考源 | 形态 | 取 | 弃 |
| --- | --- | --- | --- |
| **temp/zed** | Rust + GPUI，AI 栈最完整 | ① wire types：`language_model_core/src/request.rs:263` 的 `MessageContent`（`Text` / `Thinking` / `ToolUse` / `ToolResult` / `Image`）、`LanguageModelRequest`（`:470`）、`LanguageModelToolUse`（`language_model_core.rs:593`）；② 流式事件统一表示 `LanguageModelCompletionEvent`（`language_model_core.rs:42`，含 `StopReason` / `TokenUsage`）；③ **`ChatCompletionEventMapper` + `ToolCallAccumulator`（`chat_completion.rs:424` / `:312`）**——把分片到达的 `tool_calls` 按 index 累加成完整调用，容忍 `index: -1`、字段缺失、`reasoning` / `reasoning_content` 两种思考字段写法。**这是自研 OpenAI 客户端最容易踩坑之处，直接移植**；④ SSE 解析（`open_ai/src/open_ai.rs:1091-1133`：`BufReader::lines()` + `data:` 前缀 + `[DONE]` 终止）；⑤ `AgentTool` trait（`agent/src/thread.rs:5223`）的类型设计：`type Input: JsonSchema`、`const NAME`、`run(...) -> Task<Result<Output, Output>>`——**错误也用结构化 Output 而非字符串**，让模型能读懂失败原因；⑥ `ThreadStatus { Idle, Generating }` 双态机（`acp_thread.rs:3633`）；⑦ 线程持久化范式（`agent/src/db.rs:453`：SQLite 存元数据列 + BLOB 存 JSON + zstd 压缩） | ACP 协议栈（`agent-client-protocol`、`agent_servers`、`acp_tools`）、`cloud_llm_client` / `client` / `proto` 云耦合、`sandboxing.rs`（49 KB，依赖 OS 级 seatbelt / landlock）、profile 系统、subagent / worktree 归档、`agent_skills`、`agent_ui` 的 workspace / editor / project / lsp 耦合 |
| **temp/oxideterm-ai** | 4.9 万行，**本身即零 GPUI 内核 crate** | ① 分层理念的佐证：Cargo.toml 无 gpui、代码零 gpui 引用——证明「AI 内核与 UI 彻底解耦」在本技术栈可行；② `providers/catalog.rs:3` 的 `AI_PROVIDER_TEMPLATES`（9 个模板 + 自定义 `openai_compatible`，`base_url` 全可配）；③ `providers/discovery*.rs` 的模型列表发现与 URL 版本回退（`/chat/completions` vs `/v1/...`）；④ `tool_result_protocol.rs:17-20` 的工具结果分级截断（12000 / 2000 / 1000 字符）；⑤ 无头测试范式（`tempfile` 建库 + `TcpListener` 假服务器，`agent/tests.rs` 918 行全无 UI 依赖） | **`tokio` + `reqwest` + `eventsource-stream` 栈**——与本仓「零 tokio 纪律」（`Cargo.toml:107`）直接冲突，**不可搬**；多代理委派 / `AgentResourceCoordinator` 资源租约 / SHA-256 进度指纹 / epoch 防 late-callback / `AgentText` 全程零化（约 60–70% 代码，属企业级过度设计）；`acp/`（4800 行）、`orchestrator.rs`、`application_tools.rs`（1310 行）、`runtime_context/`（1457 行）、`target_projection.rs`——全部强绑终端会话 |
| **gpui-component 0.7.1**（**本仓已在依赖图**） | 同栈组件库 | **UI 层几乎零自研**：`bubble`（`Bubble` / `BubbleContent` / `BubbleGroup` / `BubbleReactions` / 7 种 `BubbleVariant` / `BubbleReactionSide`）、`message`（`Message` / `MessageGroup` / `MessageAvatar` / `MessageHeader` / `MessageContent` / `MessageAlignment`）、**`message_scroller`（`MessageScroller` + `MessageScrollerState`：`append` / `splice` / `prepend` / `is_scrolled_up` / `is_following_tail` / `remeasure_items` / `scroll_to_item` / `scroll_to_end` / `with_jump_button` / `with_bottom_fade`）**、`text::markdown()` → `TextView`（含 `stream_fade` 流式渐显）、`virtual_list`、`input` / `EditorState` | 无 |

> 上游可用性核验：`gpui-component-0.7.1/src/lib.rs:32/58/59/80` 将 `bubble` / `message` /
> `message_scroller` / `speech` 声明为**无条件 `pub mod`**（不在任何 `#[cfg(feature = …)]` 之后），
> 因此无需改动 `gpui-component` 的 feature 配置即可使用。

### 2.2 本仓既有资产（必须复用，不重复造）

| 资产 | 位置 | 复用方式 |
| --- | --- | --- |
| `AiProvider` trait + OpenAI 兼容实现（ureq，非流式） | `crates/duck-db/src/ai.rs:20` / `:38` | 上移并升级为流式（§4.9） |
| 「阻塞调用 → `background_executor` → 回主线程 notify」范式 | `pages/redis.rs:726`（`spawn_redis`）、`pages/db.rs`（`spawn_db`） | 照抄为 `spawn_ai` |
| 「内核发事件 → `std::sync::mpsc` → 前台轮询任务消费」范式 | `pages/terminal_canvas.rs:262-285`（`start_terminal_poller`） | 流式事件回 UI 的主路径 |
| 设置页分类表 | `pages/settings.rs:280`（`PAGES`） | 新增「AI」分类（§4.4） |
| 单键 JSON 持久化范式 | `duck-store/src/lib.rs:61/70`（`kv_get` / `kv_put`） | 设置与轻量状态 |
| 加密凭据 | `duck-platform::vault`（AES-256-GCM，`seal` / `open`） | API key：密文进厂商档案（**2026-10-12 起不再经 `duck-platform::secret` / 系统钥匙串**） |
| 写闸门与审批范式 | `pages/redis.rs` 的 `PendingAction` + `confirm_or_run` | 工具审批的形态参考（§4.6） |

## 3. 总体架构与 crate 布局

```
duck-app（bin）
  ├─ pages/assistant.rs        AssistantPage 实体（照 DatabasePage / RedisPage 模板）
  │    │  左栏会话历史 / 主区消息流 + 输入区 / 右栏会话参数
  │    ▼
  └─ pages/settings.rs         ★新增「AI」分类页（厂商、凭据、默认模型、工具开关、索引范围）
       ▼
duck-ai（内核 crate，★新建，不依赖 gpui、不引 tokio）
  ├─ wire       消息 / 事件 / 工具调用 wire 类型（serde，可单测）
  ├─ sse        SSE 行解析 + ToolCallAccumulator（纯函数，可单测）
  ├─ provider   厂商模板目录 + OpenAI 兼容流式实现 + 模型列表发现
  ├─ secret     API key 存取（→ duck-platform::secret）
  ├─ agent      Agent 主循环（同步阻塞）+ 取消 + 轮次与预算约束
  ├─ tool       工具协议：trait + 注册表 + 结果信封 + 分级截断
  ├─ kb         本地知识库：FTS5 索引与检索
  └─ store      会话与设置持久化（自管 SQLite → 见 §4.5）
        ▼
duck-platform（secret / paths）  duck-store（kv）  ureq（HTTP）  rusqlite（SQLite / FTS5）
```

依赖方向严格向下。`duck-ai` 内核**零 `gpui::` 引用**（对齐 `duck-redis` / `duck-db` 的分层纪律），
可用普通 `#[test]` 覆盖。

**关键方向约束：`duck-ai` 不依赖 `duck-db` / `duck-redis` / `duck-term`。**
反向依赖由「工具注册」解决——上层把工具实现注册进 `duck-ai` 的注册表（§4.6），
`duck-ai` 只认协议不认具体工具。这与 `oxideterm-ai` 把工具执行留在 UI 侧同构，
也守住了 AGENTS §1 的依赖方向。

与既有 crate 的关系：

- **`duck-db` → `duck-ai`**（单向）：text-to-SQL 复用协议层；SQL 领域逻辑（提示词、只读证明）留 `duck-db`。
- **复用 `duck-platform`**：`secret`（凭据）、`paths`（数据目录）。
- **复用 `duck-store`**：仅用于轻量 kv（如「上次激活的会话 id」「AI 分类是否折叠」）；
  会话正文与知识库走 `duck-ai` 自管数据库（§4.5 说明为何不混进 `duck-store`）。

## 4. 核心设计决策

### 4.1 零 tokio 下的流式输出（本模块最关键的一个决定）

本仓零 tokio；而 `zed` 与 `oxideterm-ai` 的流式都是 tokio + `reqwest` + `eventsource-stream`。
**两者不可搬**，必须换实现路径。绕开的依据是一条实测事实：

> **`ureq 3.4.2` 支持阻塞式流读取**：`Body::as_reader() -> BodyReader`
> （`~/.cargo/registry/src/…/ureq-3.4.2/src/body/mod.rs:230`），`BodyReader` 实现
> `std::io::Read`，可 `BufReader::new(reader).lines()` 逐行读 SSE。
> 另有 `into_reader()`（`:260`）与 `read_to_string()`（`:292`）。

因此数据通路设计为：

```
① UI 线程：组装请求 → 起专用 std::thread（不在 background_executor 池里，见下）
② 后台线程：ureq 发请求 → Body::as_reader() → BufReader::lines() 逐行
              → sse::parse_line() → 事件塞 std::sync::mpsc::Sender<AiEvent>
③ UI 线程：前台轮询任务（cx.spawn + background_executor().timer）每 40ms drain 一次
              → 累积到本地缓冲 → 节流后 cx.notify() 一次
```

**四条实现纪律**：

1. **用专用 `std::thread` 而非 `background_executor`**。`background_executor` 是共享池
   （`duck-redis` 的注释明确「本 crate 不开线程、不建 runtime」是相对的），长驻阻塞读会占住
   worker 影响其他任务。参照 `duck-store/src/writer.rs:93`（专用写线程）的先例。
2. **必须节流，不能逐 token `notify`**。本仓已有惨痛先例：`dialog.rs` 的 prepaint 监听器
   无条件 `window.refresh()` **自激成死循环**（见 `duck-components/src/dialog.rs` 模块头）。
   流式同理——40ms 合并一次，且 `TextView` 自带 `stream_fade` 渐显会补足观感。
3. **取消 = `AtomicBool` 置位 + 关闭底层连接**。后台线程每次读完一个事件检查一次标志位；
   `ureq` 的连接随线程栈 drop 而关闭。M1 不做「优雅中断当前 token」。
4. **超时分层**：连接超时（`timeout_connect`）+ 全局超时（`timeout_global`）
   + 两次事件之间的空转超时（自管，防止服务端挂死）。

### 4.2 依赖准入（已全部实测，结论：**零新增下载**）

| 依赖 | 状态 | 依据 |
| --- | --- | --- |
| `markdown 1.0.0` | **已在 `Cargo.lock:4196`**（`gpui-component` 的传递依赖） | 提为直接依赖**零新增下载**（与 `flate2` 同理，见根 `Cargo.toml:60-62` 的既有论证方式）。注意 `gpui-component` 已为其开 `["serde"]`，直接依赖须**对齐同一组 feature**，否则会二次解析 |
| SQLite **FTS5** | **已可用，零 feature 改动** | `libsqlite3-sys-0.30.1/build.rs:129` 在 bundled 编译路径**无条件**加 `-DSQLITE_ENABLE_FTS5`；本仓 `rusqlite = { version = "0.32.1", features = ["bundled"] }`（根 `Cargo.toml:74`）正走该路径 |
| `ureq 3.4.2` | 已在图内 | 根 `Cargo.toml:50`，`rustls` + `gzip` |
| `rusqlite 0.32.1` | 已在图内 | 根 `Cargo.toml:74`，`bundled` |
| `serde` / `serde_json` | 已在图内 | 根 `Cargo.toml` |

**预期本轮 `Cargo.lock` 变化：仅新增 `duck-ai` 自身条目**（workspace 成员），无第三方新条目。
`THIRD_PARTY_LICENSES.md` 预计不变，但仍需跑一次 `sbin/licenses.py --check` 确认。

### 4.3 多 provider 模板（协议只有一套）

**决定性简化：多模板 ≠ 多协议。** 首版所有模板统一走 **OpenAI 兼容 `/chat/completions`**，
差异只在 `base_url` 与默认模型列表——因为国内主流厂商（DeepSeek / Moonshot / 智谱 / 通义）
与本地 Ollama 都提供 OpenAI 兼容端点。

```rust
// duck-ai/src/provider.rs
pub struct ProviderTemplate {
    pub id: &'static str,            // "deepseek"
    pub name: &'static str,          // "DeepSeek"
    pub base_url: &'static str,      // "https://api.deepseek.com/v1"
    pub default_models: &'static [&'static str],
    pub key_url: Option<&'static str>,   // 申请入口，设置页展示
    pub is_local: bool,                  // Ollama 等本地端点（免 key）
}
pub const TEMPLATES: &[ProviderTemplate] = &[ /* 见下表 */ ];
```

| 模板 id | 名称 | base_url | 备注 |
| --- | --- | --- | --- |
| `openai` | OpenAI | `https://api.openai.com/v1` | |
| `deepseek` | DeepSeek | `https://api.deepseek.com/v1` | |
| `moonshot` | Moonshot (Kimi) | `https://api.moonshot.cn/v1` | |
| `zhipu` | 智谱 GLM | `https://open.bigmodel.cn/api/paas/v4` | |
| `qwen` | 通义千问 | `https://dashscope.aliyuncs.com/compatible-mode/v1` | 需用兼容模式端点 |
| `ollama` | Ollama（本地） | `http://127.0.0.1:11434/v1` | `is_local = true`，免 key |
| `openai_compatible` | 自定义 | 用户填写 | 兜底项，覆盖一切 OpenAI 兼容端点 |

- **模型列表**：M1 用模板内置 `default_models` + 用户在设置页手填；
  **拉取 `/models` 自动发现**放 M2（`oxideterm-ai/providers/discovery*.rs` 有完整参考）。
- **URL 回退**：M2 加入「`{base}/chat/completions` 与 `{base}/v1/chat/completions` 互试」逻辑
  （用户常漏填 `/v1`）——`oxideterm-ai/src/streaming/openai.rs` 有该逻辑。
- **Anthropic / Gemini 原生协议**：接口留出（`provider` 模块按 `protocol` 字段分派），
  M1 不实现，UI 上该二模板**灰显并说明「M2 接入」**——不做「静默按 OpenAI 协议发过去然后失败」。

### 4.4 配置与凭据（厂商设置放「设置」页）

**用户已明确：AI 厂商设置属于「设置」，不属于 AI 助手页。** 落点：

- **设置页新增分类**：`pages/settings.rs:280` 的 `PAGES` 数组插入一项
  `{ key: "ai", title: "AI", description: "模型厂商、凭据与工具权限", icon: DuckIcon::Bot }`，
  并在 `render_content`（`:846`）新增渲染分支 `render_ai_settings`。
  ⚠️ `PAGES` 的顺序即导航顺序；`palette_pages()`（`:335`）与 `palette_page_key()`（`:346`）
  自动跟着走，无需另改。分类插在「通知」之前（工具类设置聚在一起）。
- **设置页内容**（竖排 `option_row_stacked` / 横排 `option_row` + `settings_section`）：

| 分区 | 项 |
| --- | --- |
| 厂商 | 厂商列表（`card` + `accent_tag` 标注当前激活）；「添加厂商」走 `dropdown_select` 选模板 → 生成一条可编辑档案；每条档案：名称 / base_url / 模型（下拉 + 手填）/ API key（掩码）/ 删除 |
| 默认 | 默认厂商 / 默认模型（`dropdown_select`，候选取自已配置厂商） |
| 生成 | 温度（`option_row` + 数值输入）/ 最大输出 token / 最大工具轮次（防止失控循环） |
| 工具 | 三个工具各一个 `switch_control`（`read_file` / `list_dir` / `search_kb` / `run_sql_readonly`）；`run_sql_readonly` 旁标注「只读语句自动执行，写语句退回编辑器」 |
| 知识库 | 索引范围（多选目录）/「重建索引」按钮 + 索引进度与块数 / 索引体积 |

- **AI 助手页只做「选择」**：输入区上方的模型控件是 `dropdown_select`（候选来自设置页已配置项），
  右栏显示「本轮用量 / 最大轮次 / 已启用工具」只读信息。**不在助手页里改 base_url 或 key。**

- **凭据**（**2026-10-12 改口径**）：API key 经 `duck_ai::secret::seal`（转发
  `duck-platform::vault`，AES-256-GCM）加密后**存进厂商档案**（见下条 `api_key` 字段），
  随 `kv: ai_providers` 落 SQLite。**不再使用系统钥匙串**（原设计是
  `duck-platform::secret` + 键 `ai:{provider_id}:api_key`，已废弃）。
  纪律保持：**存储不可用时不降级明文 / 内存**，直接报错并禁用保存；零日志。

- **档案持久化**：`kv: ai_providers`（单键 JSON 数组，沿用 `db_connections` / `redis_connections` 范式）；
  元素含 `id / template_id / name / base_url / model / is_local / api_key`。
  `api_key` 是 `Option<String>`：`None` = 这条档案没配密钥，`Some` 是 `seal()` 产出的密文
  （`#[serde(default)]` 兼容旧档）。**明文 key 不入 kv**，SQLite 里 grep 不到。
  「这条档案配过密钥没有」= `api_key.is_some()`，不再另存标记键。

### 4.5 会话与知识库的持久化（`duck-ai` 自管 SQLite）

**为什么不用 `duck-store` 的 kv**：kv 是单键 JSON + 400ms 去抖整值重写
（`duck-store/src/writer.rs:21`）。会话正文含长文本与工具输出，每次追加都全量重写 JSON
是 O(n²) 级别的浪费；且工具输出动辄数千字符，JSON 转义开销不可忽略。

**方案**：`duck-ai` 自管一个 SQLite 文件 `{app_data_dir}/ai.db`，三组表：

```sql
-- 会话元数据（列表与搜索用，不含正文）
CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY, title TEXT NOT NULL,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
    model TEXT, archived INTEGER NOT NULL DEFAULT 0);
-- 消息正文（一条一行，流式结束后落库）
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, seq INTEGER NOT NULL,
    role TEXT NOT NULL, content_json TEXT NOT NULL,
    created_at INTEGER NOT NULL, input_tokens INTEGER, output_tokens INTEGER);
CREATE INDEX IF NOT EXISTS idx_messages_conv ON messages(conversation_id, seq);
-- 知识库（FTS5 虚拟表，见 §4.7）
CREATE VIRTUAL TABLE IF NOT EXISTS kb_chunks USING fts5(
    source, heading, body, tokenize = 'unicode61');
CREATE TABLE IF NOT EXISTS kb_meta (key TEXT PRIMARY KEY, value TEXT);
```

**四条纪律**：

1. **不动 `duck-store` 的表结构**（对齐 `docs/redis-design.md` §7 的既有自查项）。
   独立文件还带来额外好处：知识库索引是**可重建的派生数据**，
   会话是**不可重建的用户数据**——两者混在一个库里会让「重建索引」动作必须小心翼翼。
   独立文件下，重建索引只影响 `kb_*` 两张表。
2. **流式期间不落库**。内存累积完整消息，「一轮结束」时一次性写入。
   进程退出走 `cx.on_app_quit` 的既有钩子（对齐 `app.rs:397` 的 `duck_store::flush`）。
3. **标题自动生成**（M1 取首条用户消息的前 N 字；M2 可做「用小模型摘要」）。
4. **备份边界**：`ai.db` 属用户数据，需纳入 `backup.rs` 的导出范围（M2 项，见 §8 未决项）。

### 4.6 工具系统与审批闸门

**工具协议**（照 `zed` 的 `AgentTool` 设计，去掉 GPUI 与异步）：

```rust
// duck-ai/src/tool.rs
pub struct ToolSpec { pub name: &'static str, pub description: &'static str, pub schema: serde_json::Value }
pub trait Tool: Send {
    fn spec(&self) -> ToolSpec;
    /// 同步执行。错误也走 Ok(ToolOutcome)，让模型读懂失败原因（zed 纪律）。
    fn run(&self, input: &serde_json::Value, cancel: &AtomicBool) -> ToolOutcome;
}
pub struct ToolOutcome {
    pub ok: bool,
    /// 回给模型的内容（已按分级阈值截断）
    pub content: String,
    /// 给 UI 展示的原始产物（如结果网格）——不进对话上下文
    pub detail: Option<ToolDetail>,
}
```

- **分级截断**（照 `oxideterm-ai/tool_result_protocol.rs:17-20`）：
  工具输出按 12000 / 2000 / 1000 字符三档截断，截断处显式标注「已截断，共 N 字符」——
  绝不静默丢内容（模型会以为看全了）。
- **注册**：`duck-ai` 只提供 `ToolRegistry`；**工具实现由上层的 `duck-app` 注册**，
  因此 `duck-ai` 无需依赖任何引擎 crate。注册发生在 `AssistantPage` 构造时。
- **审批闸门**：与 `capability × 确认` 同构（`pages/redis.rs` 的 `PendingAction` + `confirm_or_run`）：

| 工具 | 风险 | 默认行为 |
| --- | --- | --- |
| `read_file` | 只读 | 自动执行 |
| `list_dir` | 只读 | 自动执行 |
| `search_kb` | 只读 | 自动执行 |
| `run_sql_readonly` | **看语句** | 经 `duck_db::sqlintel` 的只读证明：**证明为只读 → 自动执行**；否则**不执行**，把语句填回 SQL 编辑器等人工审批 |

- **路径约束**：`read_file` / `list_dir` 限定在白名单目录（默认为工作区根 + 用户配置项），
  并拒绝 `..` 逃逸——对齐 `duck-plugin-js` 网络层的「逐跳重新过闸门」纪律
  （`duck-plugin-js/src/net.rs:257`）。
- **轮次上限**：`max_rounds`（默认 8，可配）防止工具循环失控；达到上限时明确告知用户
  「已达最大工具轮次」而非静默停止。

### 4.7 本地知识库检索（FTS5）

**背景**：本仓当前**没有任何知识库 / 全文检索基座**（`duck-store` 仅 4 张表；`duck-tools`
只有拼音索引用于工具搜索）。`search_kb` 需从零建，但不需要自研索引算法。

**技术选型**：SQLite **FTS5**——已在依赖图内可用（§4.2 实测）。排序用 FTS5 内建 BM25
（`bm25()` 辅助函数），片段用 `snippet()`，无需自研倒排或引入 `tantivy`。

**索引来源（M1 范围，排序即优先级）**：

| 来源 | 说明 |
| --- | --- |
| 本仓 `docs/*.md` | 设计文档；开发者模式下默认索引 |
| 插件目录 | `plugins/*/plugin.toml` 的描述 + 同名 `README*`（插件开发者写给自己用户的说明） |
| 用户指定目录 | 设置页「索引范围」里添加的目录（`md` / `txt` / `rs` / `js` / `toml` / `json` 等文本扩展名） |
| 应用日志 | M2 再加（日志量大，需按时间窗与级别过滤，否则索引会被噪声淹没） |

**分块策略**：按 Markdown 标题层级切块（无标题则按固定行数），块保留 `source`（文件路径）
与 `heading`（标题路径），便于 UI 展示「出处」并支持「打开原文件」。

**索引纪律**：

- **索引可重建、不参与备份**：`kb_meta` 记索引版本与来源清单，
  设置页的「重建索引」= 清空 `kb_chunks` 后重扫。
- **增量**：按文件 `mtime` + 大小判定是否需要重index（`kb_meta` 记每个来源的 `mtime`）。
- **索引在专用线程**：与 §4.1 同款，不占 `background_executor`；进度经 `mpsc` 回报 UI。
- **只读**：`search_kb` 绝不写索引。

### 4.8 UI 形态（三栏，全部走上游组件）

照 `DatabasePage`（`pages/db.rs`）/ `RedisPage`（`pages/redis.rs`）模板：

- **实体**：`pub struct AssistantPage { sidebar: Entity<SidebarState>, …, view: WeakEntity<Self>, _subscriptions: Vec<Subscription> }`，
  配 `sidebar_state()` / `render_sidebar()` / `render_right_sidebar()`，
  根渲染 `SidebarPanel::new("assistant-content", &self.sidebar, content, window, cx)`。

| 区域 | 组件（**全部已有**） | 主题 token |
| --- | --- | --- |
| 左栏会话历史 | `SidebarPanel` + 本仓 `nav_list` / `sidebar_navigation_button` | `density.nav_row`、`nav_selected`、`hover_tint` |
| 会话项元信息 | 本仓 `meta_text` | `typography.xs` 12/16、`muted_foreground` |
| 消息列表 | **`MessageScroller`**（`with_jump_button(true)` + `with_bottom_fade(hairline)` + `scrollbar(true)`） | `hairline` |
| 用户消息 | **`Bubble::new().with_variant(Tinted).alignment(End)`** | 主题 `primary`（经 `project_onto_kit` 自动跟随） |
| 助手消息 | **`Bubble::Ghost`** + **`text::markdown(..)` → `TextView`** | `foreground`、`typography.sm` 13/18 |
| 流式渐显 | `TextView` 内建 `stream_fade` | — |
| 思考块 | 本仓 `content::accordion` | `hairline`、`text_tertiary` |
| 工具调用卡 | 本仓 `card` + `accent_tag` / `tag` + `StateLayer` | `card_radius`、`success` / `warning` / `destructive` |
| 输入区 | `Input`（多行）+ `ControlExt::control` | `density.control` 28、`input` |
| 发送 / 停止 | `button(.., Primary)` / `button(.., Destructive)` | `button_surface` 对比度收敛 |
| 右栏 | shell 级 `with_right_sidebar` | — |
| 长会话虚拟化 | `VirtualList` / `v_virtual_list` | — |

**三条 UI 纪律**：

1. **气泡底色不硬编码**，一律走 `BubbleVariant`（`Tinted` / `Ghost` / `Muted` / `Outline`）。
   其取值来自 `gpui-component` 主题，而本仓 `duck-theme/src/manager.rs:418` 的
   `project_onto_kit` 已把本项目 token 投影过去 → **自动跟随明暗、强调色与缩放**。
   ⚠️ 落地前需核对投影是否覆盖 `Bubble` 用到的全部槽位（`primary` / `muted` / `accent` / `border`）。
2. **字阶一律走 token**，禁止裸 `px(字号)`（AGENTS §3.6）。Markdown 内部的标题字号由
   `TextViewStyle` 决定，需经 `base_text_view_style`（`gpui-component/src/text/mod.rs:35`）
   对齐本项目 token，不额外写死。
3. **a11y**：上游自绘元素默认不入 a11y 树（本项目 P2-7 结论：两条无头断言通路都不可用）。
   自绘的可点元素必须补 `role` + 显式 `aria_label`；裸 `div` 需另加 `.tab_stop(true)`
   （`track_focus` 只置 `focusable` 不置 `tab_stop`）。**只能靠代码审查，不能靠测试兜底。**

### 4.9 与 `duck-db::ai` 的收敛路径

现状：`crates/duck-db/src/ai.rs`（134 行）已有 `AiProvider` trait 与 `OpenAiCompatible`
（非流式）。**避免两套实现并存**，收敛步骤：

1. `duck-ai` 提供流式 `ChatProvider`（§4.3）与 `wire` 类型。
2. `duck-ai` 提供**非流式便捷方法** `complete(prompt) -> Result<String>`——即
   `duck-db` 当前所需能力的超集。
3. `duck-db/src/ai.rs` 改为薄适配层：保留 `AiProvider` trait 名与 `build_prompt`，
   内部改为调 `duck_ai`；`OpenAiCompatible` 结构体保留（字段不变），
   `complete()` 转调 `duck_ai`。
   → **`db.rs` 的调用点（`run_ai_question`，`pages/db.rs:2228`）零改动**。
4. 待 `db.rs` 的并发编辑收口后，再评估是否把 `build_prompt` 也上移
   （涉及 SQL 领域提示词，属 `duck-db`，倾向留在原地）。

## 5. 登记清单（精确到文件）

| # | 文件 | 位置 | 改动 |
| --- | --- | --- | --- |
| 1 | `Cargo.toml`（根） | `[workspace.dependencies]` | 新增 `duck-ai = { path = "crates/duck-ai" }`；**无需新增第三方依赖**（§4.2） |
| 2 | `crates/duck-ai/` | — | **新建 crate**：`Cargo.toml` + `src/{lib,wire,sse,provider,secret,agent,tool,kb,store}.rs` |
| 3 | `crates/duck-app/Cargo.toml` | `[dependencies]` | 新增 `duck-ai.workspace = true` |
| 4 | `crates/duck-app/src/pages/assistant.rs` | — | **新建**：`AssistantPage`（三栏 + 流式 + 输入区 + 工具卡片） |
| 5 | `crates/duck-app/src/pages/mod.rs` | 模块与导出区 | 新增 `mod assistant;` + `pub use assistant::AssistantPage;`（**路由常量 `ASSISTANT_ROUTE` 已存在，不改**） |
| 6 | `crates/duck-app/src/app.rs` | `:674` | `PlaceholderPage::new("AI 助手", DuckIcon::Bot)` → `AssistantPage::new(cx)`；`routes` 表（`:764-772`）补 `.with_sidebar(..)` 与 `.with_right_sidebar(..)` |
| 7 | `crates/duck-app/src/pages/settings.rs` | `PAGES`（`:280`） | 插入 AI 分类一项（顺序在「通知」之前） |
| 8 | `crates/duck-app/src/pages/settings.rs` | `render_content`（`:846`） | 新增 `key == "ai"` 渲染分支 → `render_ai_settings` |
| 9 | `crates/duck-theme/src/i18n.rs` | `EN` 表 | 补全部新增中文文案（按 UTF-8 字节序插入，二分查找） |
| 10 | `crates/duck-db/src/ai.rs` | 全文 | 改为 `duck-ai` 的薄适配层（§4.9） |
| 11 | `crates/duck-db/Cargo.toml` | `[dependencies]` | 新增 `duck-ai.workspace = true` |
| 12 | `crates/duck-components/src/icons.rs` + `assets/icons/` | `duck_icons!` | **仅在需要新图标时**新增（Lucide 1.75；`Bot` 已存在，可直接复用为「AI」分类图标） |

> **不改动的既有约定**（重点确认项）：
> - `settings.rs:62` 的 `KNOWN_ROUTES`（7 条）**不动**——AI 助手路由早已登记。
> - `actions.rs` 的 `ROUTE_ASSISTANT` / `⌘7` / `ActionMeta` **不动**。
> - `duck-store` 表结构**不动**（§4.5）。
> - `gpui-component` 的 feature 配置**不动**（§2.1 已核验无需改）。

## 6. 分期实施计划

| 增量 | 内容 | 交付判据 |
| --- | --- | --- |
| **M1 可用** | `duck-ai` crate（`wire` / `sse` / `provider` / `secret` / `agent` / `store`，**不含 `tool` / `kb`**）+ `pages/assistant.rs`（三栏、流式、Markdown、多会话、持久化）+ 设置页 AI 分类（厂商 CRUD、凭据、默认模型）+ 登记 12 处 + i18n | **「新建会话 → 提问 → 逐字出字 → 重启后仍在」全链路通**；`cargo test -p duck-ai` 覆盖 `sse` 解析与模板解析（含脏数据用例）；Ctrl 级取消可用 |
| **M2 工具与检索** | `tool.rs`（协议 + 注册表 + 分级截断 + 审批闸门）+ 四个工具（`read_file` / `list_dir` / `search_kb` / `run_sql_readonly`）+ `kb.rs`（FTS5 索引与检索）+ 思考块折叠 + 工具调用卡片 + 模型列表自动发现 | 提问能触发工具调用并展示卡片；只读 SQL 自动执行进结果网格、写语句退回编辑器；知识库能索引指定目录并返回带出处的片段 |
| **M3 深度集成** | 全局「问 AI」（选中文本 / 报错信息 → 新会话）、停止 / 重试 / 编辑重发、`ai.db` 纳入备份导出、Anthropic / Gemini 原生协议、日志索引、多模型对比 | 从任意页面把选中内容一键送入 AI；备份能带走会话历史 |

**风险提示（诚实口径）**：M1 的难点集中在 §4.1 的流式通路（本仓无先例，`terminal_canvas`
的轮询范式是最近似的参考，但它不是流式文本）；M2 的难点是审批闸门与 `duck-db::sqlintel`
的接线。建议 M1 / M2 分别验收，不等 M3。

## 7. 与项目约定的符合性自查

- [x] 分层：`duck-ai` 不依赖 gpui、不依赖引擎 crate；依赖方向单向向下；工具由上层注入
- [x] 依赖准入：**零新增第三方依赖**（实测 `markdown` 与 FTS5 均已在图内）；MSRV 1.85 不受影响；`--locked` 并发纪律
- [x] 许可证：`THIRD_PARTY_LICENSES.md` 预计不变，仍跑 `sbin/licenses.py --check` 确认
- [x] 异步纪律：零 tokio；专用 `std::thread` + `mpsc` + 前台轮询 + 节流 notify；`RunCommand` 监听器先 `cx.defer`
- [x] i18n：中文源串 + EN 表字节序插入 + 覆盖率守卫绿（`table_is_sorted` / `every_chinese_literal_is_translated`）
- [x] 存储：不碰 `duck-store` 表结构；`duck-ai` 自管 `ai.db`；API key 零明文、无降级
- [x] UI：全部走上游 `Bubble` / `MessageScroller` / `TextView`；气泡底色走 `BubbleVariant` 不硬编码；字阶走 token、无裸 `px(字号)`；悬停 `hover_tint`；下拉 `dropdown_select`；布尔 `switch_control`
- [x] a11y：自绘可点元素补 `role` + `aria_label`；裸 div 加 `.tab_stop(true)`
- [x] 不发明 API：所有 gpui / gpui-component 调用以 `temp/gpui-kit` 与 0.7.1 源码为准（本文档行号均为实测）
- [x] 不改动既有约定：`KNOWN_ROUTES` / `⌘7` / `gpui-component` features 均不动

## 8. 未决项（需用户拍板或后续验证）

1. **知识库索引什么**（§4.7）。本仓当前无知识库基座，M1 表格里列的是建议范围。
   若用户所指的「资料库」是**外部某个具体目录**，请指明，索引策略随之调整。
2. **`ai.db` 是否纳入备份**。会话是用户数据，理应可备份；但需与 `backup.rs`
   既有的导出/导入语义对齐（是否包含知识库索引；索引可重建，倾向不导出）。
3. **首版模板清单**（§4.3）是否需增删。已覆盖 OpenAI / DeepSeek / Moonshot / 智谱 /
   通义 / Ollama / 自定义；如需加本地 vLLM、LM Studio、Azure OpenAI，按同结构补一行。
4. **是否保留 `duck-db::ai::build_prompt`**（§4.9 第 4 条）。倾向保留在 `duck-db`（SQL 领域提示词）。
5. **`MessageScroller` 的 pin-to-bottom 需实机验证**（§4.8）。流式增长时
   `remeasure_items` + `is_following_tail` 的行为只能真机确认。
6. **工具审批的默认档位**。`run_sql_readonly` 的「只读自动执行」是否需用户显式开启
   （对齐本项目「确认是用户开的开关」的既有口径，`redis.rs` §4.9 第 1 条）。

## 9. 落地状态（M1，本次实施）

> 与 §6 的 M1 判据逐条对应。**未完成项写在最后一节，不粉饰。**

### 9.1 已落地

| 落点 | 内容 |
| --- | --- |
| `crates/duck-ai/`（新 crate） | `lib` / `wire` / `sse` / `provider` / `secret` / `agent` / `store` 七个模块；**零 gpui、零 tokio**；31 个单测全绿；clippy 无警告 |
| `crates/duck-app/src/ai.rs`（新） | 配置读写层：`ai_providers` / `ai_settings` / `ai_config_rev` 三个 kv 键的唯一入口（`ai_configured_keys` 于 2026-10-12 删除：密钥挂在档案上，一个事实不记两处） |
| `crates/duck-app/src/pages/assistant.rs`（新） | 三栏 UI（左：会话 + 搜索；中：`MessageScroller` + `Bubble` + `TextView::markdown`；右：会话参数与工具能力）；专用 `std::thread` + `mpsc` + 40ms 前台轮询 + 取消 |
| `settings.rs` | 新增「AI」分类（顺序在「通知」之前，`DuckIcon::Bot`）：厂商清单 + 就地展开的编辑器 + 生成参数；命令面板自动收录（`palette_pages()`） |
| 图标 | `send.svg` / `square.svg`（Lucide，`stroke-width="1.75"`），登记进 `duck_icons!` |
| i18n | 新增 94 条英译，按 UTF-8 字节序并入 EN 表（1130 → 1224），`table_is_sorted` 绿 |

### 9.2 相对设计文档的**偏离**（都是有理由的）

1. **厂商编辑器是「展开式单列表单」，不是「左列表 + 右表单」**。设置窗口自身已是两栏，
   再嵌一层同形态两栏会出现两条纵向导航。
2. **凭据状态不问存储**（2026-10-12 更新）。原设计是「`secret::get()` 会真的读一次系统
   存储，所以『配过密钥没有』另落 kv（`ai_configured_keys`）」。密钥改挂档案后，
   渲染路径只读内存里的档案标记（`api_key.is_some()`），**不落盘、不解密**；
   真正的解密只有两处：**发送前**（取明文进请求）与**编辑弹窗回显**。
3. **多了一个 kv 键 `ai_config_rev`**。原设计用 `providers` 的 `PartialEq` 判断
   「设置页改过没有」，但**只补一个 API key 不会让档案集合变化**；版本号让助手页
   每帧只比一个整数。
4. **模板下拉只列 `is_available()` 的模板**。Anthropic / Gemini 若可选中、可保存，
   要到第一次发请求才失败——正是 §4.3 点名不许的静默失败。
5. **`MessageScroller` 的行渲染改用 `TextView::markdown(id, …)`**，不用便利函数
   `markdown(…)`：后者以 `ElementId::CodeLocation` 为 id，同一处代码渲染的每一行会撞 id。

### 9.3 未完成（下一轮）

- **`duck-db::ai` 收敛为 `duck-ai` 的薄适配层**（§4.9 第 1–4 条，登记清单第 10、11 项）。
  **本轮暂缓的原因**：`crates/duck-app/src/pages/db.rs` 正被另一会话持续改写
  （mtime 以秒为单位变化，且已在用 `render_ai_bar`），两者共面，此时改 `duck-db/src/ai.rs`
  会与其在编成果互冲。**这是一处明确的技术债，不是遗漏。**
- **M2 / M3 全部内容**（工具系统、FTS5 知识库、Anthropic / Gemini 原生协议）。
- **真机验证清单**（无头测不到，只能手跑）：
  1. 流式逐字输出时 `MessageScroller` 是否稳在底部（§8 第 5 条）；
  2. 中文输入法在输入框里的组合串是否会被流式 notify 打断；
  3. ~~钥匙串首次写入时的系统授权弹窗~~ **（2026-10-12 已删除该验证项）**：
     不再使用系统凭据存储，不存在授权弹窗；改为验证「保存密钥后重启应用，编辑弹窗仍能
     解密回显」与「清空密钥即删除」。
  4. 左栏会话列表在窄窗口下的截断与删除按钮的悬停区。

## 10. 落地状态（M2：工具系统 + 知识库 + 多轮循环）

> 与 §6 的 M2 判据对应。**未完成项照旧写在最后一节。**

### 10.1 已落地

| 落点 | 内容 |
| --- | --- |
| `crates/duck-ai/src/tool.rs`（新） | `Tool` trait / `ToolSpec` / `ToolRegistry` / `ToolOutcome` / `ToolDetail` / `ToolRisk` / `TruncationTier`；14 个单测 |
| `crates/duck-ai/src/kb.rs`（新） | SQLite **FTS5** 知识库：`KbSource` / `Chunk` / `KbHit` / `KbIndex` / `IndexProgress`；增量索引（指纹命中原样跳过）；16 个单测 |
| `crates/duck-ai/src/agent.rs`（扩） | `run_agent` 多轮循环 + `Approval` 审批回调 + `AgentEvent` 事件流 + `AgentOutcome`；`run_turn` 改为薄包装。测试里起**本机 stub SSE 服务**走真实往返（含分片拼装的 `tool_calls`） |
| `crates/duck-app/src/ai_tools.rs`（新） | 三个只读工具：`read_file` / `list_dir` / `search_kb`；`Sandbox` 目录白名单；14 个单测 |
| `crates/duck-app/src/pages/assistant.rs`（扩） | 工具卡片（工具名 + 风险标签 + 结果预览 + 结构化产物表）；分轮落库；状态栏四分支；输入框默认高度 2 → 3 行 |
| `crates/duck-app/src/pages/settings.rs`（扩） | 「AI」页新增「助手工具」（总开关 + 工作目录）与「本地知识库」（来源清单 + 添加 / 移除 / 重建 / 停止 + 进度日志）两个分区 |
| i18n | 新增 39 条英译（+ M1 的 94 条），按 UTF-8 字节序并入 EN 表 |

### 10.2 关键实测结论（写下来免得下次重踩）

1. **⚠️ SQLite FTS5 默认分词器下中文检索完全失效**。`unicode61` 把一整段连续汉字当作
   **单个 token**：`INSERT '数据库连接失败怎么办'` 后 `fts5vocab` 只出一个词，
   `MATCH '数据库'` 与 `MATCH '连接失败'` 都是 0 行。**`tokenize='trigram'` 也救不了**
   （两字查询仍 0 行）。采用的方案：**索引前与查询前都做同一套 CJK 预分词**
   （相邻 CJK 字符之间插空格，汉字各自成 token），展示时 `despace_cjk` 还原。
   代价是中英交界处还原后可能多一个空格（`abc数据库` → `abc 数据库`）。
2. **FTS5 的 `snippet()` 在预分词下不可用**：它会给**每个 token** 单独加高亮括号
   （`数 据 库 [连] [接]`）。所以片段在 Rust 侧按原文字符窗口自切（`make_snippet`）。
3. **FTS5 的表不要起别名**：`MATCH` 与 `bm25()` 都按表名解析，写成
   `FROM kb_chunks c` + `WHERE c MATCH …` 多数版本会报 `no such column`。
   要联表就用「表名 + 别名」两套并存的写法。
4. **`LayoutMode::AutoGrow` 的空闲高度就是 `min_rows`**（`gpui-base/src/input/base/mode.rs`）。
   所以「输入框太矮」改 `min_rows`，**不能加 `min_h`**——会和 auto_grow 实测行高打架。
5. **`ToolRegistry` 装箱 `Box<dyn Tool>` ⇒ 不是 `Clone`**：给线程用必须现场
   `build_registry(cx)` 重装一份并转移所有权；视图里另存一份供右栏展示。
6. **clippy 的 `manual_is_multiple_of` 不能照改**：`usize::is_multiple_of` 稳定于 **1.87**，
   而本仓 MSRV 硬门是 **1.85**（`duck-ai` 的 `Cargo.toml` 没写 `rust-version.workspace`，
   只有 `duck-ssh` / `duck-term` 写了，别拿「本机 1.98 能编」当理由）。所以
   `kb.rs` 里 `written % 200 == 0` 改写成了 `since_report` 计数器。同类坑见
   `agent.rs::run_turn_with` 的 `too_many_arguments`（加 `#[allow]` + 理由，同仓先例
   `plugin_center.rs:1311`）。

### 10.3 相对设计文档的**偏离**（都是有理由的）

1. **审批闸门当前对所有工具都直接答 `Run`（只读）/ `Deny`（其余）**，没有做
   §4.6 表格里 `run_sql_readonly` 的「按语句证明只读 → 自动放行」。原因见 10.4 第 1 条：
   本批工具全是只读的，`Deny` 分支是为「将来某个工具把风险升到 `NeedsApproval`」预备的
   兜底——**它保证那种情况下不会被静默执行**，而不是现在就有东西要走它。
2. **未知工具名一律判 `NeedsApproval`**（`ToolRegistry::risk_of`）。模型幻觉出来的工具名
   不该因为「查不到风险」被当成只读放行。
3. **设置页的分区叫「助手工具」而不是「工具」**：设置页已有一个「工具」分类，管的是
   `duck-tools` 那批工具。两处都叫「工具」会让人以为是同一个东西。
4. **「添加目录」= 添加即索引**，不留「登记了但没索引」的中间态。`kb_sources` 表的语义
   就是「已索引的来源」；空目录（扫不到可索引文本）会让 `index_source` 返回 `Err`，
   来源因此不会登记，不需要额外回滚。

### 10.4 未完成（下一轮）

- **`run_sql_readonly`（§4.6 的第四个工具）暂缓**。它需要**活着的数据库会话**，
  而会话句柄在 `DatabasePage` 内部，`duck-db/src/config.rs` 的 `ConnectionConfig` 只有元数据，
  没有可查的会话注册表。等 `db.rs` 收口后与下一项一起接。
- **`duck-db::ai` 收敛为 `duck-ai` 的薄适配层**（§4.9，登记清单第 10、11 项）。
  仍与并发会话共面（`db.rs`、`duck-db/src/drivers/*` 正被另一会话改写），继续暂缓。
- **M3**：全局「问 AI」、停止 / 重试 / 编辑重发、`ai.db` 纳入备份、
  Anthropic / Gemini 原生协议、日志索引、多模型对比。
- **真机验证**：§9.3 的四条仍然有效（无头测不到）。
