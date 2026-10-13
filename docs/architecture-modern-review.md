# Duck Toolbox 架构评审（现代化桌面程序视角）

> 评审日期：2026-10-13
> 评审范围：`crates/*` 19 个 crate + 工作流与构建设施
> 评审方法：以"现代化桌面程序"应有的 10 个设计维度对照本项目实现打分，
> 输出**总评 + 亮点 + 风险 + 行动项**。本文件与 `architecture-gap-analysis.md`
> 互补：那份按 P0–P3 缺口清单排；这份按维度 × 取舍 × 行动项排。

---

## 1. 一句话总评

**底座质量高、产品化纵深不足。**

骨架层（chrome、主题、命令面板、凭据、更新、可观测性）已经做到
Tauri / Electron 级别以上的工程水准；但"最后一公里"——多进程边界、
可插拔扩展点、面向终端用户的发布治理——还停留在工程基线水平。
整体形态介于**"可长期维护的工程底座"**与**"面向终端用户的产品"**之间，
差最后一公里的纵深。

**综合评分：4.35 / 5.0**（10 维度 5 分制加权平均）。

---

## 2. 现代化桌面程序的 10 个设计维度 × 本项目映射

| # | 维度 | 业界水位 | 本项目状态 | 评分 | 关键证据 |
| --- | --- | --- | --- | --- | --- |
| 2.1 | 进程模型与生命周期 | 单实例锁、外部唤起归一、深链接、系统级 AppUserModelID / Info.plist 身份 | 全部到位 | 4.5 | `crates/duck-app/src/main.rs` 1–212、`IpcListener` / `acquire_instance_lock` / `set_app_identity` / `register_open_handlers` |
| 2.2 | UI 渲染与可访问性 | 自绘 chrome、保留模式、滚动层、双层 token、a11y 命名 / 焦点 / Tab | 保留模式 + 滚动层（gpui-fast fork）已默认开；a11y 树收 role，但 **Tab 路径断言难** | 4.0 | `AGENTS.md §3.6`、`gpui-fast` patch、`P2-7` 部分落地 |
| 2.3 | 主题与国际化 | 双层 token + 派生、明暗槽位 + 强调色 / 缩放 / 密度 / 字号 / 字体族四轴、源串即键 | 42 套预设、4 轴、`tr` / `trf` / `trn` / `tr_relative_time` 都齐 | 4.5 | `crates/duck-theme/`、`builtin::base_tokens`、`tr_relative_time` |
| 2.4 | 状态 / 存储层 | 结构化版本化迁移、去抖写入、结构化配置走单键 JSON、append-only 审计 | kv 单写线程 400 ms 去抖、`user_version` 只追加、audit 表触发器拒改 | 4.5 | `crates/duck-store/`、`Op::ClearTables` 同事务、`audit` 触发器 |
| 2.5 | 插件与扩展模型 | 沙箱、能力网关、子集授权、网络 host 函数、市场 + 签名 | QuickJS 沙箱 + 能力网关 + 子集授权 + 网络能力都已落地；**插件签名未做**（P3-1） | 4.0 | `crates/duck-plugin/`、`duck-plugin-js`、`duck-plugin-registry` |
| 2.6 | 命令 / 快捷键 / 菜单 | 四输出面（快捷键 / 菜单 / 命令面板 / 活动栏）、两档 handler、模糊搜索 | 四输出面派生、`install_app_handler` App 级、模糊 `fuzzy.rs` | 4.5 | `crates/duck-command/`、`ActionSlot`、`palette_pages` |
| 2.7 | 可观测性与诊断 | 崩溃捕获 + 水位线 + 诊断信息块 + 日志轮转 + 大小上限 + 导出 | 全到位 | 4.5 | `crash.rs`、`diagnostics.rs`、`RollingWriter`、`enforce_size_cap` |
| 2.8 | 安全与凭据 | 不绑系统钥匙串、AES-256-GCM 单仓保管箱、0600 权限、原子写 | dev / Bundle 解耦（2026-10-12）、vault + ring pair、first-use 双重检查 | 4.5 | `vault.rs`、`secret.rs`、`FIRST_USE: Mutex<()>` |
| 2.9 | 平台能力层抽象 | 平台层零 gpui、可单独测试、target-gated、版本钉死 | `duck-platform` 零依赖 gpui，模块齐全 | 4.5 | `crates/duck-platform/src/lib.rs` |
| 2.10 | 多进程 / 后台任务边界 | 主线程 vs 后台 executor 明确、阻塞驱动只走 background_executor | 全同步内核（db / redis / mail）+ `background_executor` 跑 JS；当前唯一 current-thread tokio 给 `tiberius` | 4.0 | `duck-plugin-js` 10 s = 主线程最长冻结、tokio 不外泄（mail） |

---

## 3. 现代化设计值得拿出来讲的 5 件事

### 3.1 进程模型走的是「正主 + 转发」而不是「拉起重启」

`crates/duck-app/src/main.rs` 1–212 这段写得**比绝大多数 Electron / Tauri 应用都严谨**：

- **单实例**：macOS flock + Windows 自研 IPCListener 双通道。
- **三种入口归一**（命令行 / 深链接 / 第二实例转发）走**同一个 channel**
  （`requests_tx`），最后统一由 `cx.spawn(async |cx| ...)` 消费。
- **`on_reopen` / `on_open_urls` / `IpcListener` 三个 callback 都只往通道里投**，
  不碰 `App`——避免 `Send` 灾难。
- **`QuitMode::Explicit`**：macOS 关窗不退出，托盘 + 全局快捷键继续可用。

这条流水线本身就是「现代化桌面应用」的硬指标，很多工程实现拉胯在这一步。

### 3.2 主题做成"双层 token + 四轴投影"是正确分层

`crates/duck-theme/src/lib.rs` 39–45 行这一组常量：

```rust
pub const BASE_FONT_SIZE: f32 = 13.;
pub const BASE_LINE_HEIGHT: f32 = 18.;
pub const FONT_SIZES: [f32; 5] = [11., 12., 13., 15., 17.];
pub const FONT_SIZE_RANGE: (f32, f32) = (11., 17.);
```

配合 `builtin::base_tokens` + `extended_tokens` 的双层结构、
`button_surface(bg, fg)` 派生入口、`active_theme(cx).density()` 注入路径——
这是**真正能跨主题换肤 + 跨缩放还保持排版**的设计。

对比 Tauri 生态里很多应用把"字号"散落在各个 render 函数里——
一旦要做"设置 → 字号"就要全站搜 `px(N)`。
本项目用 `AGENTS.md §3.6` 的字阶硬约束把这件事**编码到代码规范里**，
配合 `cargo test -p duck-theme` 的覆盖率守卫（已常驻），
**是一条可执行的工程纪律**。

### 3.3 命令层做成"四输出面派生"是罕见的好抽象

`crates/duck-command/` 把一个 `Action` 派生出**快捷键 + 菜单 + 命令面板 + 活动栏**
四个面，并且**两档 handler**（窗口级 `install_handler` + App 级 `install_app_handler`）
解决 macOS 关窗后菜单栏照旧可点的问题——这是 Zed / Sublime 那种
**已经吃过亏才整理出来的口径**。

### 3.4 凭据**主动解绑系统钥匙串**是 2026 年该有的取舍

`AGENTS.md §4 + vault.rs / secret.rs` 这块：

- 2026-10-12 移除 `keyring`：与系统钥匙串绑定会把"应用数据"拆成两半
  （备份 / 迁移只能搬走一半），且各平台后端可用性不一致。
- 改用自家 `vault`（AES-256-GCM，密钥 `vault.key` 0600）+ `secret`
  （应用自有 `credentials.json`）。
- 配 `FIRST_USE: Mutex<()>` + **双重检查** 修 first-use 竞态
  （这是真密码学关键一步）。

**这条隐含价值观**是"应用数据是一等公民、要可打包迁移"，
比多数 Tauri / Electron 应用默认走系统钥匙串的隐式选择要稳。

### 3.5 GPUI 经 patch 换 gpui-fast 是性能分叉的工程化样本

`Cargo.toml` `[patch.crates-io]`：

```toml
gpui-pre            = { git "longbridge/gpui-fast", rev "b8f0c026…" }
gpui-pre-platform   = { git "longbridge/gpui-fast", rev "b8f0c026…" }
gpui-pre-macros     = { git "longbridge/gpui-fast", rev "b8f0c026…" }
gpui-pre-sum-tree   = { git "longbridge/gpui-fast", rev "b8f0c026…" }
```

- **零版本号改动**，仍写 `=0.3.8`——这是上游 ck 的兼容替身。
- **retained mode + scroll layers** 默认开，可用 `GPUI_VIEW_RETENTION=0 /
  GPUI_SCROLL_LAYERS=0` 降级。
- `alacritty_terminal` + `vte` 也走 vendored（带 ASCII 快路径 +
  视口滚动单独上报 damage 两处有文档的补丁）。

这条路径**不是简单的"fork"，而是有 runbook 的可控实验**。
`vendor/README.md` 把"为什么必须 vendored、差异在哪"都列了——
这是"工程化分叉"该有的样子。

---

## 4. 没达到水位线的 5 件事（按风险大小排）

### 4.1 ❗ 单一窗口根 + 强耦合的"全局窗口句柄集合"

`crates/duck-app/src/app.rs` 36–67 行：

```rust
pub struct Desktop {
    main_window: Option<WindowHandle<Root>>,
    settings_window: Option<WindowHandle<Root>>,
    logs_window: Option<WindowHandle<Root>>,
    plugin_center_window: Option<WindowHandle<Root>>,
    licenses_window: Option<WindowHandle<Root>>,
    onboarding_window: Option<WindowHandle<Root>>,
    palette_window: Option<WindowHandle<Root>>,
    settings_pending: Option<(&'static str, Option<String>)>,
    main_views: Option<MainViews>,
    pending_tool: Option<String>,
}
```

这是**典型的"窗口级 Global"**——每开一个窗口就要加字段、
改 `empty()`、改 `install()`、改 `open_*`。

**现代化口径**应当是：

- **窗口管理器**（`WindowRegistry<WindowKind>`，`HashMap<WindowKind, WindowHandle<Root>>`），
  开窗是注册表项，**不是字段**。
- **跨窗口跳转**走 `pending_jumps: VecDeque<JumpTarget>`，**消费即弹**，
  不用新字段。
- **跨窗口通信**走 topic / channel（`window.broadcast::<T>()` 之类），
  不是直接传 `WeakEntity`。

这个项目目前只有 6 类窗口还撑得住，加到 15+ 类就会变形。
**优先**：

| 重构 | 价值 | 工作量 |
| --- | --- | --- |
| `WindowRegistry<WindowKind>` 替换散落字段 | 解决字段膨胀 | 1.5 人日 |
| `pending_jumps` 替换 `settings_pending` / `pending_tool` 两个并列字段 | 统一跳转协议 | 0.5 人日 |
| 提取 `MainViews` 为 trait object + 入口协议 | 解开 `ShellView` / `ToolboxPage` 强耦合 | 2 人日 |

### 4.2 ❗ 渲染态和数据态混在 `Entity<T: Render>` 里（无强隔离）

`crates/duck-app/src/app.rs` 70–74 行：

```rust
struct MainViews {
    shell: WeakEntity<ShellView>,
    toolbox: WeakEntity<ToolboxPage>,
}
```

`ShellView` 和 `ToolboxPage` **同时承担**：

1. **渲染**（`Render` 实现）
2. **状态**（业务字段：当前路由、活动栏选中、右栏状态）
3. **副作用**（订阅主题变化、订阅通知、订阅 KV 变更）
4. **键盘绑定的宿主**（`view.context("Main")` 之类）

这是 GPUI 鼓励的模式（一切皆 `Entity`），但**长期演化**会出现：

- **巨型 Entity**（`view.rs` 已 1167 行、`app.rs` 1041 行）。
- **跨页通信全靠 `WeakEntity`**，类型耦合难拆。
- **测试粒度粗**：要测渲染就得 `cx.open_window`，要测状态就得 `cx.new()`。

**现代化口径**应当是：

- **状态层**：`Store<T>`（不可变快照 + `update`）+ `Subscription`，**不依赖渲染**。
- **视图层**：纯函数 `View<S> -> Element`，**不持有状态**。
- **容器层**：`App<S>` 把 store 注入 view。
- **这是 React / Redux 已经趟过的路**，Rust + GPUI 完全可以做。

短期（项目当前阶段）**不建议大改**，但**新增 Entity 必须区分**：

- `XxxState` 只持有数据
- `XxxView` 只做渲染
- `XxxApp` 把两者粘起来

否则 7.9 万行还会继续膨胀到 15 万行。

### 4.3 ⚠ `gpui-fast` fork 锁 rev 没有 release 频道

`Cargo.toml` `[patch.crates-io]`：

```toml
gpui-pre = { git = "https://github.com/longbridge/gpui-fast", rev = "b8f0c026553711c1e049fee501932d3801f23567" }
```

**风险**：

- `longbridge/gpui-fast` 是公司仓库，**没有 release 标签**。
- 项目升级只能 `git fetch` + 重新锁 rev，**没有 changelog、没有 semver**。
- 升一次就要重跑整套 CI + 真机测试（macOS / Windows / Linux）。
- 哪天 longbridge 弃坑，**要 fork 到自己组织下**或者回退到 crates.io 上游
  （性能会回退）。

**现代化口径**应当是：

- **要么内化到自己组织**（`yourorg/gpui-fast`），定期 rebase + release tag。
- **要么抽象一层** `gpuix` = 自己的薄壳，把 fork 的差量**收敛到一个 crate**，
  未来 1 行依赖就能回退。
- **必须有 changelog.md + 性能基准测试**（retained mode 启停的帧时间）。

### 4.4 ⚠ 模块依赖关系有"循环嫌疑"的零号位

虽然 `Cargo.toml` 写了**单向分层**（theme ← components ← shell ← app），
但下面这些是**事实依赖**，文档里没明说：

- `crates/duck-platform/src/lib.rs` 注释明确"不依赖 gpui"——这是**正确的隔离边界**。
- `crates/duck-store/` 是否依赖 gpui？（需逐行核）。
- `crates/duck-plugin/` `Capability::set_of` 与 `Capabilities::only` **互逆对**——
  这是规则硬约束，但**没有自动化测试**守护。
- `crates/duck-theme/` `tr / trf / trn / tr_relative_time` 在哪些 crate 调用？
  是否到处都用、还是被绕过？

**现代化口径**应当是：

- **`cargo metadata` + `cargo-deps` 定期出依赖图快照**（已经是 P3-3 候选）。
- **`#[deny(unreachable_crates)]` 在 CI 里跑**。
- **`set_of` / `only` 互逆对有 property-based test**。

### 4.5 ⚠ 测试基线偏弱（影响长期演化）

`crates/duck-theme/src/i18n.rs` **4543 行**——但里面主要是 EN 表 + 守卫测试。

实际**业务逻辑测试**覆盖：

- `duck-db` 7 引擎 × `sqlintel` 1759 行（应有大量）
- `duck-mail` `mime.rs` 1745 + `sync.rs` 1704 + `compose.rs` 1190（应有大量）
- `duck-redis` 7 行 ~1000+（应有）

**没看到的**：

- **GPUI 视图层**几乎没渲染快照测试（`RenderSnapshot`）。
- **跨进程协议**（IPC 转发、deeplink、托盘 click）没有 headless 集成测试。
- **崩溃钩子**没看到"panic 后能落盘 + 计数 +1 + 重启能看到水位线"的端到端测试。

**现代化口径**应当是：

- **`RenderSnapshot`**：CPU 端渲染 + diff 像素（gpui-kit 已提供，仓库里没看到用例）。
- **跨进程 fixture**：`fake-app` crate 跑 IPCListener + 转发断言。
- **崩溃注入**：`std::panic::catch_unwind` 在测试里人为 panic，断言 `crash_count` 自增。

---

## 5. 现代化桌面程序该有、本项目还没有的 4 件事

| 缺失项 | 含义 | 紧迫度 |
| --- | --- | --- |
| **5.1 插件签名 + 市场治理**（P3-1） | 复用 `duck-update` 的 minisign 验签基建，给插件维护一个签名链 + 撤列表 | 高（用户拿到插件就敢装） |
| **5.2 自动更新协议 + 灰度** | 当前只有"手动检查 → 下载 → 校验 → 替换"，没有灰度、回滚窗口、用户提示 | 中（产品化前必须有） |
| **5.3 崩溃上报 + 遥测（可选）** | 当前只是本地落盘，没有去重聚合 + 远程上报（要 P0 隐私设置闸门） | 中（产品化前必须有） |
| **5.4 i18n 深化**（P3-2 留尾） | 复数和相对时间已做；日期格式 / 数字千分位 / 序数后缀（1st / 2nd / 3rd）未做 | 低 |

---

## 6. 行动项（按 ROI

### 6.1 立即做（单 PR / 单人日可完成）

| 行动 | 价值 | 工作量 | 涉及 |
| --- | --- | --- | --- |
| `WindowRegistry<WindowKind>` 替换 `Desktop` 散落窗口字段 | 解决字段膨胀、降低新窗口接入成本 | 1.5 人日 | `duck-app/src/app.rs` |
| `pending_jumps: VecDeque<JumpTarget>` 替换 `settings_pending` / `pending_tool` 两个并列字段 | 统一跳转协议 | 0.5 人日 | `duck-app/src/app.rs` |
| 新增 Entity 模板：`XxxState` / `XxxView` / `XxxApp` 三件套 | 阻止 Entity 巨型化继续 | 0.5 人日 | `AGENTS.md §4` 新增章节 |
| `cargo deny` + `cargo metadata` 依赖图检查入工作流 | 防回归 | 0.5 人日 | `.github/workflows/ci.yml` |

### 6.2 短期做（1–2 周一个）

| 行动 | 价值 | 工作量 | 涉及 |
| --- | --- | --- | --- |
| `gpui-fast` fork 内化到自己组织 + changelog.md + release tag + 性能基准 | 解锁第三方依赖风险 | 4 人日 | `Cargo.toml`、`.github/workflows/` |
| 渲染快照测试基线（`RenderSnapshot`） | 锁定 UI 正确性 | 3 人日 | `crates/duck-components/tests/` |
| 跨进程协议集成测试（IPCListener + 转发） | 锁定单实例 + 唤起行为 | 2 人日 | `crates/duck-platform/tests/` |
| 插件签名（复用 minisign 验签基建） | 让插件可发版 | 6 人日 | `crates/duck-plugin-registry/`、`crates/duck-update/` |

### 6.3 中期做（1 个月一个）

| 行动 | 价值 | 工作量 | 涉及 |
| --- | --- | --- | --- |
| 状态 / 视图 / 容器分层治理（按 4.2 思路） | 长期可维护性 | 8 人日 | 全 `duck-app` |
| 多平台补齐（Linux / Windows 对齐） | 跨平台分发 | 3 人日 | `crates/duck-platform/` |
| 自动更新灰度 + 回滚窗口 | 产品化 | 5 人日 | `crates/duck-update/` |
| i18n 深化（日期 / 数字 / 序数） | 国际化质量 | 3 人日 | `crates/duck-theme/src/i18n.rs` |
| 崩溃上报（隐私开关闸门 + 去重 + 远程） | 运维可观测性 | 4 人日 | `crates/duck-platform/src/crash.rs` |

---

## 7. 三句话总结（给团队）

1. **不要急着加新 feature**——先把 `WindowRegistry` + `pending_jumps` 抽出来，
   再做一次"状态 / 视图 / 容器"分层治理。
   这一步走完，**整个项目加新工具页的成本会从"写 800 行 Entity + 100 行 action"
   降到"写 200 行 View + 30 行 Action + 30 行 Store"**。

2. **把 `gpui-fast` fork 内化到自己组织**——加 changelog.md + 性能基准 + release tag。
   这是这个项目最深的供应商依赖，必须能自己控制节奏。

3. **`docs/architecture-gap-analysis.md` 已经把 P0–P3 缺口逐条盘点过**——
   下一个冲刺建议**优先级 P3-1（插件签名）+ P3-3（架构治理）+ 5.2（更新灰度）**。
   这三件事做完，"工程底座"→"可发布产品"的最后一道门就过了。

---

## 附 A：核心架构事实摘录

**`Cargo.toml` workspace 拓扑**：

```
duck-theme (底层)
  ↑
duck-components
  ↑
duck-shell
  ↑
duck-app (二进制装配)
  + 旁路：duck-platform / duck-store / duck-plugin* / duck-update
    （刻意不依赖 gpui，可单独测试）
```

**进程模型**：

- macOS flock 单实例 + IPCListener（Unix 域套接字风格）。
- Windows：IPCListener 是**唯一判据**（无 flock）。
- 三入口归一：CLI 参数 / 深链接 / 第二实例转发 → 同一 channel → 主线程消费。

**主题系统**：

- 双层 token：基础 token（`base_tokens`）+ 扩展 token（`extended_tokens`）。
- 四轴：明暗预设 / 字号 / 缩放 / 密度（"缩放"与"密度"两条独立轴）。
- `Appearance` 单结构体 → 单 kv 键 `kv: appearance`。
- 新字段一律 `Option` + `#[serde(default)]`，保证向后兼容。

**凭据存储**（2026-10-12 后）：

- `duck_platform::vault`（AES-256-GCM，密钥 `vault.key` 0600）→ 密文存档案行。
- `duck_platform::secret`（应用自有 `credentials.json`，0600 / 0700 原子写）
  → 值也是 vault 密文。
- AI key 不走 `secret`（进 `kv: ai_providers`）。

**已完成的 P0–P2 缺口**（截至 2026-10-11）：

P0-1（崩溃）/ P0-2（凭据）/ P0-3（数据生命周期）/ P0-4（首启引导）/
P1-1（唤起键）/ P1-2（窗口会话）/ P1-3（更新链路）/ P1-4（命令面板）/
P1-5（拖拽 + 深链接）/ P2-2（日志）/ P2-3（死代码）/ P2-4（占位路由）/
P2-5（概览真实化）/ P2-6（通知）/ P2-7（a11y）/ P2-8（插件子集授权）/
P2-9（JS 网络能力）。

**P3 待做**：P3-1（插件签名 6 人日）/ P3-2（i18n 深化 3 人日）/
P3-3（大文件拆分 4 人日）/ P3-4（多平台补齐 3 人日）/
P3-5（主题深化 + 遥测 3 人日） = **合计 ≈ 19 人日**。

---

## 附 B：与其他文档的关系

| 文档 | 角度 | 互补 |
| --- | --- | --- |
| `architecture-gap-analysis.md` | 按 P0–P3 缺口清单排，**已经逐条盘点** | 与本文互补：本文件给维度评分 + 行动项；那份给逐项改造方案 |
| `module-gap-analysis.md` | 单个模块的缺口分析 | 属于"模块级"，本文件属于"全局架构级" |
| `database-design.md` / `redis-design.md` / `email-design.md` / `terminal-ssh-sftp-design.md` | 单模块的详细设计 | 与本文件无重叠，本文件是横向评审 |
| `ai-design.md` | AI 子模块设计 | 同上 |
| `module-settings-design.md` | 设置层架构 | 同上 |

---

> 本文档将随重大架构决策同步修订。下一次评审建议在 **P3-1（插件签名）落地后**进行，
> 届时"产品化纵深"评分会从当前 4.35 上调到 4.5+。