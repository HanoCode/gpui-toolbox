# AGENTS.md — Duck Toolbox 编码助手约束

> 本文件是 AI 编码助手（Cursor / Codex / Claude / WorkBuddy 等）在本仓库工作时的
> **强制约束**。与任何通用默认习惯或训练数据冲突时，一律以本文件为准。
> 新会话开始处理本仓库任务前，先读完本文件。

## 1. 项目概览与分层

Duck Toolbox 是 macOS 桌面工具箱外壳。Cargo workspace，成员为 `crates/*`。

| crate | 职责 | 依赖方向 |
| --- | --- | --- |
| `duck-theme` | 主题 token、内置预设、`Appearance`、i18n | 最底层，不依赖其他 duck-* |
| `duck-components` | 视觉组件库（按钮 / 导航 / 卡片 / 表单原语 / 侧栏面板 / 通知） | → `duck-theme` |
| `duck-shell` | 窗口外壳：自绘标题栏、活动栏、路由与内容槽位 | → `duck-components` |
| `duck-app` | 可执行文件装配（bin = `duck`）、页面 | → 全部 |

**分层边界（不可越界）**：
- 行为与展示分离 —— 不把颜色 / 尺寸 / 布局写进底层行为代码；交互行为不写进应用层样式代码。
- 依赖方向单向向下，禁止反向依赖或循环依赖。

## 2. 常用命令

```bash
sbin/run.sh [--release] [--no-build] [--log-level debug]   # 构建并启动
sbin/package.sh                      # 全平台菜单入口（macOS）
sbin/package-macos.sh --dmg --zip    # macOS 专用
cargo build -p duck-app                                     # 仅构建
cargo test -p duck-theme                                    # i18n 覆盖率守卫等单测
cargo fmt --all                                             # 格式化（提交前必跑）
cargo clippy --workspace --all-targets                      # 静态检查（提交前必跑）
```

## 3. 硬性约束（Non-negotiables）

### 3.1 变更前必须确认
对项目的任何**文件新增、修改、删除**，以及**依赖、配置、目录结构**变更，
必须先列出：受影响文件清单 → 具体改动 → 潜在影响，**取得用户明确同意后**再执行。
不要"顺手"重构无关代码，不要一次引入多个未确认的改动。

### 3.2 绝不发明 API
GPUI / gpui-component 的 API 以**当前源码为准**，不凭记忆或类比 React / 旧版 GPUI 推断。
常用签名先查 `temp/gpui-kit/` 与 `~/.cargo/registry` 下的实际定义。
"看起来合理但不存在的方法"是本项目最常见的失败来源。
查阅优先级：`temp/gpui-kit/` 源码 → `.skills/gpui-kit/` 指南 → `https://gpui-kit.com`。

### 3.3 国际化：源串即键，必须补 EN
- 文案在代码里**直接写中文**，取词用 `tr(cx, "外观")` / `trf(cx, "剩余 {}", &[..])`。
- 新增任何中文文案，**必须**同步在 `crates/duck-theme/src/i18n.rs` 的 `EN` 表中补英译
  （表按 key 字节序排序，二分查找），否则 `cargo test -p duck-theme` 的覆盖率守卫会失败。
- 缺表项回落源串，**不得**翻成空串。
- `ShellRoute` / `ShellAction` / `AuxiliaryWindowView` 的文案是**闭包**
  （`with_label_fn` / `with_title_fn`），渲染时解析；切换语言走
  `update_appearance` → `install()` → 全窗口 `refresh()`。
- `InputState` 占位只在构建时定型：语言变化需在 render 里 `set_placeholder` 同步
  （见两处 `sync_*_placeholder`）。

### 3.4 Shell 脚本（sbin/）兼容 bash 3.2
- **变量展开一律写 `${VAR}`，禁止裸 `$VAR`**。根因：macOS 自带 bash 3.2 在 UTF-8 locale 下，
  变量名后紧跟多字节字符（如全角括号）时会把该字符字节并入变量名，报 `VAR?: unbound variable`。
- 不用关联数组 / `mapfile` / `${var,,}`；`set -e` 下禁止裸 `[ cond ] && cmd`，一律用 `if`。
- `usage()` 从文件头注释用 `sed -n 'A,Bp' "$0"` 提取，改注释须同步行号。
- 脚本改动后至少跑一次 `bash -n` 做语法校验。

### 3.5 依赖：快照严格对齐
`gpui-component 0.7.1` · `gpui-base 0.7.1` · `gpui-pre =0.3.8`（别名 `gpui`）·
`gpui-pre-platform =0.3.8` · `gpui-kit-assets 0.7.1`（别名 `gpui-component-assets`）。
升级任一 crate 时必须同步 `=` 约束，禁止局部升级导致内核与组件层错配。
新增第三方依赖前，先核对版本号、许可证，并说明必要性。

**GPUI 核心经 `[patch.crates-io]` 替换为 gpui-fast**（longbridge/gpui-fast，锁 rev）：
版本号仍是 `=0.3.8`，只是实现来自 gpui-fast 的 compat 替身 crate（`gpui-pre` /
`gpui-pre-platform` / `gpui-pre-macros` / `gpui-pre-sum-tree`）。升级 gpui-fast 时：
同步根 `Cargo.toml` 里的 rev，并对照其 `compat/` 目录核对替身清单是否变化；
retained mode / scroll layers 默认开启，`GPUI_VIEW_RETENTION=0`、
`GPUI_SCROLL_LAYERS=0` 可随时降级。`unicode-properties` 钉在 `=0.1.3`
（gpui_web 的 `=` 约束与 resvg 链路冲突，只能锁低版本），不要放开。

### 3.6 UI 规格对齐 FluxDown（改动前先核对参考源码）
参考实现位于 `temp/FluxDown/`，数值以该源码为准，不凭感觉调：

- 设置行（横排 `option_row`）：`px(spacing.lg)`、`py(spacing.sm)`、
  `min_h = density.control + 20`、行内 gap `spacing.lg`、
  标题列 `flex_1().min_w(160)`、控件列 `min_w_0()`。
- 设置行（竖排 `option_row_stacked`）：`py(spacing.md)`，label 与控件间 gap `spacing.sm`。
- 分组卡片 `option_group`：`card` + 行间 `h(px(1.)).pl(spacing.lg)` 的 hairline（**左侧留白，不贯通**）。
- 组标题 `caption_heading`：`typography.xs`(12/16) / MEDIUM / 三级文字色，`px(spacing.xs).pb(spacing.sm)`。
- 文字：`body_text`（`typography.sm` 13/18 / foreground）、`meta_text`（`typography.xs` 12/16 / muted_foreground）。
- 标题一律走 `title_text`（`typography.md` 15/20 SEMIBOLD）或 `detail_title_text`
  （`typography.lg` 17/22 SEMIBOLD，**仅详情视图**）；对话框 `Dialog::title` 也用 `title_text`。
  调用点不再自拼 `text_size` + `font_weight`。
- 字阶**全站唯一来源**是 `duck_theme::builtin::base_tokens`，逐档对齐 macOS HIG 内建文本样式：
  `xs` 12/16 · `sm` 13/18 · `md` 15/20 · `lg` 17/22 · `xl` 22/26 · `mono_md` 13/20，
  扩展 token `extended.badge` 10/13（角标）· `extended.display` 26/32（展示数值）。
  **禁止在调用点写裸 `px(字号)`**：裸值不随「字号 / 界面缩放」设置变化。
- 「标签—值」表（`option_row`）的**值一律 `body_text`**；`meta_text` 只用于整行级辅助说明
  （状态行、空态文案、卡片元信息）。
- 下拉一律 `dropdown_select`（outline Button + `control(cx)` + caret + 勾选菜单，min_w 160）；
  布尔项一律 `switch_control`（gpui-component `Switch`）；**不用** gpui-component `Checkbox`。
- **图标按钮的悬停提示由组件内部挂，调用点不要再挂一次**：`icon_button`（提示 = label）、
  `icon_button_tip`（无障碍名用 label、提示单独给 `tip`，可带状态或动态读数）、
  `toolbar_action_button`、`toolbar_dropdown_button`（纯图标下拉）都已内置 tooltip。
  ⚠️ 对同一元素二次 `.tooltip()` 会让 gpui **直接 panic 秒退**（`calling tooltip more
  than once on the same element`，且是 abort 级），**编译期不报**——新增图标按钮时
  先看它出自哪个构造函数。
- 对比度：主色按钮文字 ≥ 4.5:1；强调色文字 ≥ 4.5:1；选中导航图标 ≥ 3:1。

### 3.7 主题统一入口
UI 改主题一律走 `duck_theme::update_appearance(cx, |a| ...)`，不直接改全局状态。
`ResolvedTheme::build(builtin, accent, scale, font_size)`；明暗槽位可选预设见 `presets_for`。

### 3.8 通知
统一用 `duck_components::notify`：`info/success/warning/error`（应用内 toast）、
`sticky(key, tone, msg, ...)`（常驻且**同 key 替换**，给会反复触发的错误/警告用——
`error`/`warning` 常驻且默认 id 是随机 uuid，同一条反复推会堆满右上角）、
`system`（仅操作系统通知中心）、`system_with_toast`（双通道）。
窗口必须挂在 `gpui_component::Root` 上，否则推不出去。

**两条投递通道，没有第三条**：设置→通知里可调的是「应用内横幅三档」
（`BannerLevel{All/WarningsAndErrors/Off}`）与「系统通知」开关。`gpui-component` 的通知
**没有任何声音 API**，所以没有「声音」通道——要加得自己接音频后端。

**通道开关是 App 级全局，`kv` 只负责持久化**（`notify::channels` / `set_channels`，
`duck-app` 启动时投影一次）。设置项改动**不要**写成「`kv_put` → 回读 `kv` → 投影」：
`kv_put` 是去抖落盘的，回读拿到的是旧值，运行时档位不会变、界面还会弹回旧档。
一律从当前全局值出发只改那一位。

**自绘 toast 不要同时带 `title`/`message`**：`in_app` 把文本渲进 `content` 是因为库里
`type_` 图标的 `top(18px)` 按 16px rem 硬编码（本项目 rem = 13px）。而库的 `push_system`
只认 `title`/`message`，两者皆空时它**静默不发**；也不能补上——库的渲染会把
`title`/`message`/`content` **依次**放进同一个 `v_flex`，文本会出现两遍。
所以 `system_with_toast` 拆成两条通知推（系统那条只有文本，不建 toast；
应用内那条显式 `delivery(InApp)`、只有自绘内容）。

**投给哪个窗口**：一律投给「当前最该看到它」的窗口，顺序
`cx.active_window()` → `cx.window_stack()` 首个（前→后；Windows 返回 `None`）→
`cx.windows().first()`。直接写 `windows().first()` 会把提示投到创建最早的窗口
（主窗口），在设置窗口里操作时它会被整个盖住，看起来像「没反应」。

**系统通知的平台前提**：启动早期必须 `cx.set_app_identity(id, name)`（见 `main.rs`），
否则 Windows 上（便携 exe 无 MSIX 包身份）通知被静默丢弃；macOS 还要求 `.app`
位于系统信任位置，`sbin/run.sh` 的 dev bundle 不在其中，系统通知不显示属预期。
**无头测试同样要设身份**（`cx.set_app_identity(..)`），否则测试平台像 Windows 一样
丢通知，`shown_system_notifications()` 恒为空、断言会以「功能坏了」的样子失败。

**动作处理器有两档**：`install_handler`（窗口级）与 `install_app_handler`（App 级）。
不依赖窗口的动作（退出 / 打开设置 / 打开日志 / 检查更新）必须登记成 App 级——
macOS 关掉所有窗口后应用仍驻留，菜单栏照旧可点，此时没有活跃窗口可传。

## 4. 代码风格

- 遵循 `rustfmt.toml`（edition 2024）与 `.editorconfig`；提交前 `cargo fmt --all`。
- 遵守 `Cargo.toml` 的 `[workspace.lints]`（各 crate 经 `[lints] workspace = true` 生效）：
  `unsafe_op_in_unsafe_fn` / `dbg_macro` / `todo` 为 deny，不留调试残留。
- 重复元素必须有**领域派生的 `ElementId`**，禁止用列表下标当 id。
- 对外 `pub` 数据结构用 builder + reader 方法，**不暴露 `pub` 字段**。
- GPUI 的 `cx` 是保留名；其他持有物按内容命名，不要也叫 `cx`。
- 注释与文档使用中文，与现有代码保持一致。

### 4.1 新 Entity 模板（State / View / App 三件套）

GPUI 把「一切皆 `Entity<T>`」当作头等抽象——一个 `Entity<T: Render>` 经常同时承担
**渲染、状态、副作用、键盘绑定宿主**四件事。这在 6 个以内还能撑，新增第 7 个工具页时
就开始巨型化（`app.rs` / `view.rs` 早已破千行）。规定**新 Entity 必须按三件套拆开**：

| 层 | 职责 | 依赖 |
| --- | --- | --- |
| **`XxxState`** | 只持有数据（字段 + 不依赖渲染的逻辑）。**不**实现 `Render`。 | 纯 std + 业务 crate |
| **`XxxView`** | 纯函数 `View<&S> -> Element`，**不**持有状态字段，只读 store。 | duck-theme / duck-components |
| **`XxxApp`** | 把 state 与 view 粘起来：建 store `Entity<XxxState>`，再在视图里读。 | duck-shell 装配 |

**最小可用模板**：

```rust
// state.rs —— 零渲染依赖，可单测。
#[derive(Default)]
pub struct XxxState {
    pub items: Vec<Item>,
    pub filter: String,
}

impl XxxState {
    pub fn filtered(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.matches(&self.filter))
    }
    pub fn set_filter(&mut self, s: String) { self.filter = s; }
}

// view.rs —— 不持有状态，只读 store。
pub fn render_xxx(state: &XxxState, cx: &App) -> impl IntoElement {
    v_flex()
        .child(TextField::new("filter").on_change(|s, cx| cx.update_state(...)))
        .children(state.filtered().map(|i| render_item(i, cx)))
}

// app.rs —— 把两者粘起来，注册到 shell 路由。
pub struct XxxApp { state: Entity<XxxState> }
impl XxxApp { ... }
```

**硬约束**：

- **状态层不依赖渲染**：`XxxState` 不得 `use gpui::*::Render`。这样写测试只要 `State::new()`。
- **视图层不持有状态字段**：`XxxView` 接收 `&State`，不存字段。视图是「当前状态的镜头」，状态换了镜头就重新渲染。
- **容器层只做注入**：建 store、注入 view、绑路由，**不**承担状态逻辑。
- **键盘绑定放容器层**：`cx.bind_keys([...])` 绑到 `App` 或具体 `Entity`，不放进 state 或 view。

**反模式**（不要做）：

- `Entity<T: Render>` 同时持有业务字段、订阅 KV、绑键位——一旦 T 变巨型就难拆
- 在 `Render::render()` 里调 `cx.update(...)` 改 state——形成「渲染副作用」循环
- 把 `Entity` 当 `Rc<RefCell<T>>` 用，靠全局可变引用同步——绕开 GPUI 的订阅机制

**迁移策略**：**既有 Entity 不强制迁移**（避免大动作），但新增/重写页面时按三件套走。
第一条落地范例参见 `crates/duck-app/src/app.rs` 的 `Desktop` 注册表 + 跳转队列——
前者按 `WindowKind` 索引窗口句柄（注册表模式），后者按 `JumpTarget` enum 收口跨窗口跳转
（同类覆盖、不同类并存），都是把散落字段收敛到「一处索引」的同一个抽象。

## 5. 提交前检查清单

- [ ] `cargo fmt --all` 无 diff
- [ ] `cargo clippy --workspace --all-targets` 无 error
- [ ] `cargo test -p duck-theme` 通过（新增文案已补 `EN`）
- [ ] 改动涉及 `sbin/*.sh` 时已 `bash -n` 校验
- [ ] 未发明 API —— 所有调用均能在当前源码中找到定义
- [ ] 变更范围与用户确认的范围一致，无越界重构

## 6. 参考资料

| 资源 | 用途 |
| --- | --- |
| `.skills/gpui-kit/` | GPUI Kit 编码指南（架构 / 状态 / API / 测试） |
| `.skills/gpui-kit-design-guides/` | 组件选择、间距、层级、配色、动效设计指南 |
| `temp/gpui-kit/` | 上游源码（查真实 API 签名） |
| `temp/FluxDown/` | UI 规格参考实现 |
| `.workbuddy/memory/MEMORY.md` | 项目约定长期沉淀 |
| `.workbuddy/memory/YYYY-MM-DD.md` | 每日工作日志 |

<!-- aoci:begin -->
## AOCI Repository Cognition

AOCI maintains a stable, versioned, incrementally updatable repository-level cognition layer so models can reuse their understanding of this system across tasks.

`aoci.txt` is a structured cognition index for models. It assigns one independent Entry to every managed file, database table, or other managed object. Symbolic tags and F/R/A/S semantics describe the object's core responsibility, important relationships, external contracts, and non-obvious constraints or design decisions needed to understand or modify the system.

The Header, directory sections, and all Entries form the complete repository index. They can cover frontend, backend, configuration, database structures, and other managed content. When managed content changes, normally only the affected cognition Entries need maintenance; the complete index does not need to be regenerated.

AOCI provides a high-density view of system architecture, object responsibilities, important relationships, external contracts, and key constraints.

### How it works

AOCI uses a model-generated, model-read cognition loop.

Header, Entry, and Curation semantics follow only the current machine-issued Plan and live Guide. The Host model independently authors them from the current bound evidence.

Entry semantics must come from the model's understanding of actual evidence. Never derive, prefill, assemble, or rewrite index semantics solely from paths, filenames, extensions, an AST, symbol lists, dependency scans, regular expressions, fixed templates, or rule engines.

For a Fresh Bootstrap, follow only the current machine-issued Plan and live Guide. When they require authoring, the Host model authors Root, Meta, tags, and F/R/A/S, supplies its authoring-run declaration, and binds it to the Plan, Evidence, and complete Candidate. Never ask AOCI to set `origin=host_model`, manufacture a receipt, or turn a generated framework into semantics. Do not reconstruct the Onboarding progression here. Internal batches are not user decisions; stop only at an existing approval boundary or a real safety, drift, CAS, or Recovery condition.

### Minimal entry points

- `aoci_rules`: obtain the session-level runtime contract for the current AOCI version.
- `aoci_overview`: establish or restore complete cognition for this repository.
- `aoci_maintain`: after managed objects reach their final stable state, check whether cognition needs maintenance.
- `aoci_update_entry`: submit a complete semantic update batch bound to current evidence and source digests.
- `aoci_report`: when the current layout and tool state support it, record follow-up work if evidence is insufficient to generate semantics reliably; do not guess.

For other MCP tools, CLI commands, parameters, and specialized workflows, follow current tool descriptions, Guide, and `--help` output. This file does not duplicate the full manual.

This managed block defines only repository integration, cognition use, and task-closing principles. `aoci_rules` carries the current session contract. Live Guide output carries the execution order and stop conditions of the current Plan. Tool Schema, Spec, and Validator carry machine structures and criteria. Prompt, Description, README, and static documentation cannot override those machine facts.

### Establishing, generating, and restoring cognition

1. At the beginning of every new Agent Run, first determine:

   - whether this repository already has a usable complete AOCI index; and
   - whether current context already contains complete repository cognition that matches this repository root, current index version, and current AOCI service, and that the model can still use reliably.

2. When the repository has a usable complete index but the current Run lacks reliable complete cognition, call `aoci_rules` first and then `aoci_overview`.

   Reuse complete cognition directly while it remains reliable. Local uncertainty does not by itself require mechanically rereading the system-wide view.

   A Run that resumes from a known Host context compaction, including a Host-injected compaction summary, must treat prior model cognition as unreliable. The compacted handoff must not retain or summarize the formal Whole-Index or any Overview Header, Entry, Chunk, Challenge, or Attestation body; it may retain only receipt identity, unfinished write or Recovery state needed for safe continuation, and an instruction to reload immediately. Whole-Index semantics or a receipt copied into that handoff cannot prove that the resumed model's current cognition is reliable. If the runtime contract is no longer reliably present, call `aoci_rules` first. Before continuing the business task, make an ordinary complete Whole-Index `aoci_overview` request (`check_only` absent or false) with `refresh_reasons=["context_compaction"]` and a fresh `refresh_event_id`; do not use `check_only` or a cognition probe. Follow every exact `next_cursor` through `completed=true`, confirm delivery, and submit one Attestation based only on the newly delivered body. After that fresh complete transport, a partial or failed Attestation consumes the generation and permits the existing source-bound continuation without another automatic Overview.

   AOCI can report checkpoint and cognition-status facts for `context_compaction`, the machine `semantic_threshold` under the project `cognition_refresh_threshold`, or a major `phase_transition`. Use `check_only=true` when only those compact facts are needed. They advise the Agent but do not decide whether the model needs the system-wide view.

   When the Agent explicitly calls ordinary `aoci_overview` (`check_only` absent or false), AOCI must deliver the complete requested scope whenever a coherent CognitionSet can be formed. It must not suppress that body because a receipt already exists, a threshold was not reached, or no refresh reason is pending. Dirty or stale formal cognition is still delivered but is marked unreliable. Pending recovery or an incoherent snapshot fails closed without a mixed body.

   When an ordinary Overview reports `continuation_required=true`, submit its exact `next_cursor` automatically until `completed=true`. Do not ask the user to continue, begin the business task, or state a partial system conclusion. Stop the cognition chain on Host truncation, a missing, duplicate, or reordered Chunk, cursor failure, Index change, or `chunk_tokens` change. Until Attestation completes, never use Memory, source, Spec, `aoci.txt`, historical sessions, scope, search, or Entry reads to repair or supplement Whole-Index cognition. A challenge ordinal is the 1-based position in the formal Entry sequence; Header content, comments, blank lines, Section/Overview/Chunk markers, receipts, and Metadata are excluded, and Chunk Receipt ordinals use that same sequence. The Attestation must echo the Challenge's exact current `index_sha256`, `entry_sequence_sha256`, and `entry_count`; a prior Index, Entry sequence, count, or Attestation is invalid. After the complete chain, submit the existing model cognition Attestation once. One same-response JSON Schema or field-format error may be corrected once without changing semantic answers; an object, Tag, or F mismatch means failure and uncertain assimilation, with no semantic retry or information bypass. During initial cognition it also blocks Root/Meta, Migration, layout-wide, or other unbound system decisions. During a context-compaction refresh with complete transport, unchanged cognition identity, aligned governance, and no Recovery or third-party conflict, the attempt consumes that refresh generation even when Attestation is partial or failed; continue the existing task without another automatic Overview. `system_mastery_percent` self-assesses only the system framework—architecture, responsibilities, strong relationships, stable external contracts, and high-entropy safety and maintenance constraints—not complete implementation or runtime knowledge. Keep machine Index coverage separate, and normally give the user only the prescribed single success or failure sentence derived from actual coverage, Challenge, Chunk, token, and mastery results. If the Host truncates a Chunk, ask the user to set `overview_delivery.chunk_tokens` to a smaller valid value and restart; do not change it automatically.

   Interpret the additive cognition level independently from strict proof fields. `delivery_verified` means the Index was loaded and Host delivery was confirmed while complete cognition verification is still unfinished; describe that state as loaded and delivery-verified, never as no cognition or failure to understand the system. `cognition_verified` requires a passing Attestation (at least 80 percent of Challenge ordinals fully correct with at most one object identity miss), and `cognition_governed` additionally requires governance alignment. A generic complete-read failure sentence is reserved for an actual delivery fault.

   When an Overview response contains the optional `cognition-state/v2` projection, use its dimensions independently. Its Level ends at `model_cognition_usable`; `strict_attestation_verified`, `governance_aligned`, and `current_system_cognition_reliable` are independent states and never participate in that Level. An ordinal, object identity, Tag, or core F mismatch can make strict Attestation fail while model cognition remains usable; do not report that mismatch alone as proof that the model did not understand the system. Only `current_system_cognition_reliable=true` permits an unqualified current complete-system cognition claim. When the projection is absent, keep using the legacy interpretation above.

   An ordinary read-only audit, analysis, or check, a request not to modify code, or a request not to commit or push does not automatically mean strictly zero writes and does not alter the cognition-validity decision above. Codex Memory and historical Skills may only help recover experience, user preferences, and investigation directions. They cannot replace a current cognition receipt matching the repository root, index digest, AOCI service identity, and cognition scope. Project AGENTS and current AOCI identity take precedence over historical Memory for AOCI state.

   Treat a task as strictly zero-write only when the user explicitly prohibits Ledger, metadata, `.aoci` runtime assets, and every filesystem write. If necessary cognition establishment conflicts with that boundary, report the conflict and ask the user to decide or recommend an isolated copy. Never silently substitute Memory for current repository cognition.

3. If the repository has no usable complete index, or has only a minimal skeleton, an incomplete Header, unfinished Entries, or undecided required Curation, obtain `aoci_rules` and enter the current AOCI Guide when a formal complete AOCI index is required. Let Guide choose the next phase from actual repository state and complete the required safety steps.

   `aoci_maintain` does not replace the index-establishment workflow.

   Do not reconstruct or hard-code the full-index generation state machine in this file.

4. During a long-running task, the model is responsible for preserving the current cognition receipt and using the refresh gate correctly:

   - when the Host reports context compaction or the model knows the system-wide view was lost, follow the mandatory `context_compaction` reload rule above; AOCI cannot infer the Host event;
   - when entering a genuinely major phase, declare `phase_transition`, not a function, test run, or small step;
   - at a plausible stable checkpoint, use `check_only=true` to obtain the machine semantic count when that fact is useful;
   - except for the mandatory known-compaction reload, decide whether the current task needs another explicit scoped or complete Overview; and
   - keep the Dirty or Stale reliability state reported by AOCI until maintenance and alignment complete.

### Task closing and cognition maintenance

5. A purely read-only question, analysis, version check, or task that changes no AOCI-managed object does not require a maintenance-tool call. The AOCI version in use is `cognition_receipt.mcp_service_version` in any `aoci_overview` check_only or `aoci_maintain` response; the binary path is the `command` in the project's `.mcp.json`, and the CLI need not be on PATH.

6. When AOCI-managed objects change, call `aoci_maintain` once after they reach the task's final stable state. Do not maintain files individually after each intermediate edit.

7. If maintenance returns actual semantic candidates, the Host model must independently author the complete tag and F/R/A/S updates from each candidate's bound object and necessary evidence. Submit the complete candidate set for that current machine-issued batch in one `aoci_update_entry` call while preserving each `source_sha256`, `candidate_id`, and domain batch identity. `max_entries` limits one request and atomic transaction, not the logical plan, Whole-Index, or Managed Scope. When `remaining` is nonzero, call Maintain again after the successful Apply and continue from the new preimage; never shrink Index coverage or slice a returned batch to satisfy transport limits.

   When evidence is insufficient and the current layout supports `aoci_report`, use it instead of guessing, applying a template, or generating unsupported cognition merely to eliminate follow-up work.

8. Obey structured tool states and safety boundaries:

   - `repair_required`: repair only the explicitly identified candidates, then resubmit the complete current machine-issued batch;
   - `stopped`: end that write attempt and inspect `failed_step`, error, formal-write evidence, and Recovery. In auto mode, a proven zero-write closure is followed by a fresh Plan; a complete Intent with provable postimage is resumed; a policy-selected Rollback with exact preimage is completed and replanned. Stop the user task only when proof is unavailable, third-party bytes conflict, approval or external action is required, or another real safety boundary applies;
   - never ignore conflicts, approvals, human decisions, permissions, or safety signals; and
   - after alignment, do not repeat maintenance or writes; `refresh_ready_for_overview` is a checkpoint fact, and the Agent decides whether to request an ordinary complete Overview for its next phase.

   If any managed object changes after maintenance completes, the previous result is invalid. Complete closing again from the new final stable state.

9. When the user limits only business-file scope and does not explicitly forbid repository-managed assets, AOCI-managed assets may be updated during closing to preserve cognition consistency. Distinguish them from business files in audits and commits.

   When the user explicitly forbids changes to `aoci.txt`, `.aoci`, metadata, or any additional file, obey that restriction, do not write, and report any remaining inconsistency accurately.

### Specialized workflows

Initialization, complete-index generation, Header generation, Entries generation, database-structure indexing, Curation, human review, and failure recovery must follow only the instructions, commands, and safety stops returned by the current AOCI Guide or tool at the corresponding stage.

Do not preload, guess, or reconstruct these specialized workflows. The relevant Guide, tool descriptions, model Prompt, and CLI help provide platform invocation, request format, batch limits, approval rules, index-format details, and recovery steps as needed.
<!-- aoci:end -->
