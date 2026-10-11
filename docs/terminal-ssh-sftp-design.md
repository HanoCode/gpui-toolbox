# 终端 / SSH / SFTP 功能：架构与 UI 设计方案（v2 · 最大化复用版）

> 状态：**提案，待评审**。本文只做分析与设计，**未改动任何源码**。
> 修订：2026-10-11 · v2 取代 v1。**前提变更：不再以许可证为约束**，主轴从「隔离墙」改为「能搬就搬」。
> 依据：仓库现状 + `temp/` 下 26 个参考项目逐目录调研 + crates.io 版本核验
> 适用约束：`AGENTS.md`（硬条文）· `rust-version = "1.85"` · `gpui-pre =0.3.8`（gpui-fast `b8f0c026`）

---

## 0. 本次修订要点

### 0.1 v1 → v2 的结论翻转

| 议题 | v1 结论 | v2 结论（有证据） |
| --- | --- | --- |
| zed 的终端渲染层 | **必须自研**（GPL + GPUI 不兼容） | **几乎可原样搬**——`gpui-fast` 与 zed checkout 的 `Element`/`Interactivity`/`Styled`/`TextSystem`/`Window` **签名逐字一致**，`styled.rs` 零差异 |
| oxideterm 的内核层 | 禁止拷贝 | **整包搬**——`oxideterm-ssh`(19.6K) / `oxideterm-sftp`(11.7K) / `oxideterm-terminal`(29.2K) **全部不依赖 gpui** |
| 必须自研的量 | 渲染 4~6K 行 + 会话模型 + SFTP 面板 + 胶水 ≈ **1.0~1.5 万行** | **≈ 2.5~4K 行**（主要是 SFTP 面板 UI 与跨来源胶水） |
| 参考项目定位 | 只能当图纸 | 四个 GPL 项目里有两个是**可直接剪裁的源码库**（`oxideterm` 自称"可整包搬的 crate" 35 个 / 17.7 万行） |
| UI 外壳复用率 | ~70% | **~95%**——`gpui-kit 0.7.1`（= 本仓已依赖的 `gpui-base`/`gpui-component`）自带 Dock 拖拽分栏(4226 行)/Tab/VirtualList/Tree/ContextMenu |

### 0.2 三个决定性发现

1. **`gpui` 层面几乎没有迁移成本。** 本仓 `gpui-pre 0.3.8`（实为 longbridge `gpui-fast`，声明版本 0.2.2，上游基点 zed `a1b71072e5`）与 zed checkout（`96984225`，`crates/gpui` 版本 0.2.2）在终端用到的全部公开 API 上**签名逐字一致**。zed 的 `terminal_element.rs`（3042 行）可原样编译，真正的改动只在替换 zed 私有业务类型。
2. **oxideterm 把「无 gpui 内核」这条边界划得非常干净。** 88 个 member 中 71 个不依赖 gpui；「终端 + SSH + SFTP」闭包内的 35 个内核 crate 全部无 gpui，且其中 14 个是零/极少依赖的纯逻辑层。
3. **本仓的 CI 门禁不拦许可证。** `sbin/licenses.py` 的退出码只有三态（一致 / 过期 / 环境不支持），**没有白名单校验**。引入任何许可证的代码都不会让 CI 变红——代价仅是「依赖一变必须重生成并提交 `assets/licenses/THIRD_PARTY_LICENSES.md`」。

---

## 1. 门禁的真实边界（写在前面的诚实说明）

`AGENTS.md` 与项目记忆里写着「许可证白名单：只收 MIT / Apache-2.0，GPL 一律不收」。核实后的事实是：

- **`sbin/licenses.py` 不做任何白名单判断**。它只做两件事：把 `Cargo.lock` 里有 `source` 的依赖逐个找出随源码附带的 `LICENSE*` 全文、拼成 `THIRD_PARTY_LICENSES.md`；以及用 `--check` 比对「文件是否与当前依赖一致」（过期 → 退出码 1）。
- 因此 GPL 依赖进入依赖图，**只会让汇总文件多出一段 GPL 全文，不会导致任何作业失败**。
- 那条白名单是**人工 review 约定**，不是脚本强制。按本次指示放宽它，不改变任何自动化行为。

**需要认领的代价（如实记录，非阻挠）**：
1. `assets/licenses/THIRD_PARTY_LICENSES.md` 每次依赖变动后必须重生成并提交，否则 `static` 腿与 `package.sh` 双重门禁会拦。
2. 「搬运源码」有两种形态，合规含义不同：
   - **vendor 整包**（把 GPL crate 的源码拷进 `crates/`）——本仓将整体受 GPL 约束，**分发即触发传染**。若这个工具箱对外的许可声明仍是 MIT，需要一并修改根 `LICENSE` 或明确标注。
   - **作为依赖引用**（path 依赖指向 `temp/` 下的目录）——不进入本仓，但发布产物仍包含该代码。
3. 记忆里的那条规则建议**同步修订**（写明「已按 2026-10-11 决定放宽，依据：xxx」），避免下个会话按旧规则把方案又砍回去。

> 本文后续不再讨论许可证，只讨论「搬什么、改什么、自研什么」。

---

## 2. 参考资产总账（四档）

### 档 A — 可直接搬（零改动或近零改动）

| 资产 | 来源 | 行数 | 为什么能直接搬 |
| --- | --- | --- | --- |
| `ssh_config` 纯函数解析器 | `temp/zedis/crates/zedis-core/src/ssh_config.rs` | 289 | 无 I/O、无 gpui；支持 wildcard/`!` 取反/`ProxyJump`/`IdentityFile`/`~`/`%d`/`%h` 展开，自带完整测试 |
| 行/单元格数据模型 | `temp/oxideterm/crates/oxideterm-terminal-model` | 342 | **零依赖**；`TerminalCell`(24B)/`TerminalRow`(`Arc<Vec<Cell>>` + `signature: u64`) |
| russh 隧道最小实现 | `temp/tusk/src/ssh.rs` | 175 | 单文件；jump host + 随机本地端口 + `direct_tcpip`；host key 走 OpenSSH 语义 |
| 本地端口转发 | `temp/verve/src/ssh/port_forward.rs` | 118 | `LocalForward` + `direct_tcpip`，自洽 |
| TOFU known_hosts | `temp/verve/src/ssh/known_hosts.rs` | 182 | 自管文件 + `stored/presented_fingerprint` |
| SOCKS5 连接器 | `temp/Oxidal/src/proxy.rs` | 222 | `tokio-socks`；`Oxidal/src/host_keys.rs`(432) 另有「多 key/端口 + 弹窗」校验 |
| vendored `russh-sftp` | `temp/oxideterm/crates/russh-sftp` | 8,371 | 客户端 + 服务端，源码自足 |
| vendored `alacritty_terminal` 0.26.0 | `temp/oxideterm/crates/alacritty-terminal` | 12,500 | 含 `OXIDETERM_PATCHES.md`（补丁有据可查）；**源码在本地，不必联网 fetch** |
| vendored `vte` 0.15.0 | `temp/oxideterm/crates/vte` | 4,742 | 同上 |
| 主题 token | `temp/oxideterm/crates/oxideterm-theme` | 2,489 | 仅依赖 serde，可作调色板移植的参照 |

### 档 B — 搬 + 适配（列出改动点）

| 资产 | 来源 | 行数 | 改动点 |
| --- | --- | --- | --- |
| 终端内核 | `temp/zed/crates/terminal` | 生产 **8,326**（总 11,111） | 8 个 zed 内部依赖：`settings`/`theme`/`theme_settings`/`task` **需写 shim**；`collections`/`util`/`release_channel` 易剥离。**gpui 侧零改动** |
| 终端渲染 Element | `temp/zed/crates/terminal_view/src/terminal_element.rs` | 3,042（生产 ~2,014） | 替换 `editor::{CursorLayout, HighlightedRange, EditorSettings}`（自研 ~200-400 行）、`ui::Tooltip`、`language::CursorShape`、`theme_settings`；**gpui 侧零改动** |
| SSH 域模型 | `temp/oxideterm/crates/oxideterm-ssh` | 19,620 | 无 gpui；跟随 `russh` 版本决策（见 §5.2）；按需裁掉 GSSAPI/X11 |
| SFTP 协议层 | `temp/oxideterm/crates/oxideterm-sftp` | 11,662 | 无 gpui；含 `transfer_manager`/`conflict`/`retry`/`scp`/`tar`/`text_diff` |
| 终端内核（备选） | `temp/oxideterm/crates/oxideterm-terminal` | 29,162 | 无 gpui；含**现成的 `SshPty` 后端**（见 F5）——若走「全 oxideterm」路线则靠它省掉胶水 |
| 终端渲染（备选） | `temp/oxideterm/crates/oxideterm-gpui-terminal` | 36,743 | 依赖它自己 patch 的 `gpui-pre 0.3.7`（`LinePaintCache`/`paint_cached*`，见 `crates/gpui-ce/gpui/OXIDETERM_PATCHES.md:323-339`）→ 需换回 `shaped.paint` 或给我方 gpui 打同款补丁 |
| SFTP 会话 + 队列 UI | `temp/verve/src/ssh/sftp.rs` + `src/ui/sftp_browser.rs` | 799 + 1,339 | actor 模式（`SftpCmd` → 后台线程 → `cx.spawn` 回灌）；`QueuedUpload` 批量、冲突预检、`TransferState` 进度 |
| SFTP 双栏面板 | `temp/Oxidal/src/sftp/{panel,client,mod}.rs` | 1,924 + 811 + 566 | **双栏（Local/Remote）+ 拖放上传 + `uniform_list` + Progress**，且用上游 `gpui-component` |
| 终端 widget（备选） | `temp/waku/src/terminal.rs` | 1,997 | alacritty 0.26 全家桶 + 自绘 canvas |
| 终端 widget（备选） | `temp/verve/src/ssh/terminal.rs` | 3,005 | alacritty 0.24 + `portable-pty` + zmodem，功能最全 |
| 隧道生命周期框架 | `temp/dbflux/crates/dbflux_tunnel_core/src/lib.rs` | 540 | `TunnelConnector` trait + `ForwardingConnection` + `Tunnel` RAII |
| SSH 连接池 | `temp/Fulgur/src/fulgur/sync/ssh/pool.rs` | 326 | `checkout_or_connect` 复用模式 |

### 档 C — 只能参考（设计借鉴）

| 资产 | 来源 | 行数 | 为什么搬不动 |
| --- | --- | --- | --- |
| SFTP 完整 UI | `temp/oxideterm/crates/oxideterm-gpui-app/src/workspace/sftp/` | 16,566 | 深绑 35 万行的 workspace 架构 / 模态所有权 / 11 语言 i18n / settings-model |
| oxideterm 自有组件库 | `temp/oxideterm/crates/oxideterm-gpui-ui` | 21,604 | 与本仓 `gpui-base`/`gpui-component` 0.7.1 职责重叠，混用会破坏组件体系 |
| herdr 的「远端 surface 协议」 | `temp/herdr-gpui/src/{terminal_painter.rs, terminal/selection.rs}` | 974 + 723 | 把 VT 模拟放在远端、客户端只画单元格帧——架构可借鉴，代码不通用 |
| zed 的 remote/remote_server | `temp/zed/crates/{remote,remote_server,fs}` | 26,982 | 自研 RPC + ControlMaster 体系，为编辑器协作设计，与工具箱目标不符 |
| 系统 `ssh` 子进程方案 | `temp/herdr-gpui/crates/herdr-client/src/{ssh,forward,upload}.rs` | 851+426+607 | 把连接生命周期外包给外部进程，与「可观测/可恢复」冲突（可作降级兜底） |
| ssh2/libssh2 路线 | `temp/dbflux`(1,393) · `temp/Fulgur`(4,824) | — | C 版 libssh2 + 必须 vendored OpenSSL |

### 档 D — 必须自研

| 项 | 预估 | 说明 |
| --- | --- | --- |
| 跨来源胶水 | 400~800 行 | 若「zed 内核 + oxideterm SSH」：把 SSH channel 包成终端内核可消费的 IO（见 §4.3） |
| 连接/凭据存储精简版 | 1,000~2,000 行 | 不搬 `oxideterm-connections`(29,898)——只需「JSON 档案 + keyring 引用 + ssh_config 导入」三件事 |
| SFTP 面板 UI | 1,500~2,500 行 | 参照 Oxidal 双栏 + verve 队列 |
| settings / theme shim | 300~600 行 | zed `TerminalSettings` → 本仓 `Appearance` + `duck-theme` token |
| 路由/命令/i18n 对接 | 300~500 行 | `duck-command` Action、`EN` 表、`ShellRoute` 登记 |

**合计自研 ≈ 3,500~6,400 行**（v1 估算是 1.0~1.5 万行）。

---

## 3. 关键发现（证据）

### F1 · gpui 层面：zed 的终端代码可原样编译

逐文件对比 zed checkout 与 `gpui-fast`：

| 文件 | 差异 |
| --- | --- |
| `crates/gpui/src/element.rs` | `Element` trait 的 `request_layout`/`prepaint`/`paint` **行号与方法签名完全相同** |
| `crates/gpui/src/styled.rs` | **0 行差异**（`gap`/`flex`/`size_full` 默认值一致） |
| `crates/gpui/src/elements/div.rs` | `Interactivity`/`InteractiveElement`/`StatefulInteractiveElement` 签名零差异（gpui-fast 仅插入内部 `fast::` 钩子） |
| `crates/gpui/src/text_system.rs` · `text_system/line.rs` | `shape_line`/`ShapedLine::paint` 签名不变（内部换成 retained-mode `LineGlyphPainter`） |
| `crates/gpui/src/window.rs` | `paint_quad`/`with_content_mask`/`set_cursor_style`/`handle_input`/`on_key_event` 签名不变 |

**推论**：`terminal_element.rs` 的 `impl Element for TerminalElement`（`@1102`）与三个方法（`request_layout@1114` → `(LayoutId, ())`、`prepaint@1162` → `LayoutState`、`paint@1573`）可直接落地。

### F2 · zed 的 `terminal` crate 不是「纯内核」

- 它**依赖 gpui**（`crates/terminal/Cargo.toml:29`），`Terminal` 本身是 `Entity`，用 `Context<Self>`/`cx.spawn`/`EventEmitter`。
- zed 内部依赖 8 个：`collections`(418) · `gpui` · `release_channel`(306) · `settings`(9,293) · `task`(3,204) · `theme`(5,957) · `theme_settings`(2,448) · `util`(10,643)。
  - 易剥离：`collections`→`std`、`util`（只摘 3 个函数）、`release_channel`（1 处 `AppVersion::global`）。
  - 需 shim：`settings`（`Settings` trait + `TerminalSettings` 派生体系）、`theme`（色板 + `players()`）、`theme_settings`（字体度量）、`task`（`Shell`/`ShellKind`/`SpawnInTerminal`——构造 `tty::Options` 的关键，需实搬）。
- **不存在 `trait TerminalBackend`**，后端是具体类型 `Term<ZedListener>`；若要多后端需自己引入 trait。

### F3 · 两家都 fork 了 alacritty，但形态不同

| | zed | oxideterm |
| --- | --- | --- |
| 来源 | `git = zed-industries/alacritty, rev = 4c129667` | **vendored 源码** `crates/alacritty-terminal`（0.26.0，带 `OXIDETERM_PATCHES.md`） |
| 本机是否已有源码 | ❌ 需要 fetch | ✅ 就在 `temp/` 下 |
| `vte` | `0.15.0` + `features=["ansi"]` | vendored `0.15.0` |

**建议**：统一采用 **oxideterm 的 vendored `alacritty_terminal 0.26.0`**——源码自足、补丁有文档、不必依赖网络 fetch。代价是上游更新需手动合并。

### F4 · oxideterm 的「无 gpui 内核」边界

「终端 + SSH + SFTP」闭包并集 = **38 个 crate / 242,513 行 src**，其中 gpui 视图层仅 3 个（65,275 行），**内核 35 个（177,238 行）全部无 gpui**。

### F5 · oxideterm 已解决「远程终端」——`SshPty` 后端

`crates/oxideterm-terminal/src/session/types.rs:1-8` 的 `TerminalSessionKind` 含 **`LocalPty / SshPty / Telnet / Mosh / Serial`**；后端抽象是 `TerminalSessionBackend` trait（`session/types.rs:111-299`），统一暴露 `snapshot` / `snapshot_incremental(previous)` / `write_input` / `resize_with_cell_size`。

**这是选择路线的关键**：把 SSH channel 变成终端 IO 这件事，oxideterm 已经做完了（`local/pty.rs` + `session/`），而 zed 是**在本地 PTY 里跑 `ssh -t host`**（`crates/project/src/terminals.rs:644` 的 `create_remote_shell`）。

增量机制也现成：`incremental_snapshot_from_term()`（`local/pty.rs:1013-1088`）用 `term.damage()` 的 `TermFullDamage/ScrollUp/Partial` 只重建脏行；行复用靠 `TerminalRow::reuse_cells_from_if_equal()`（`terminal-model/src/lib.rs:236-253`）+ `TerminalSnapshot::reuse_unchanged_rows_from()`（`terminal/src/data.rs:32-78`）。

### F6 · UI 外壳几乎零自研

`gpui-kit 0.7.1`（本仓直接依赖其 `gpui-base`/`gpui-component`/`gpui-kit-assets`）源码里已有：

| 需求 | 组件 | 位置 | 行数 |
| --- | --- | --- | --- |
| 可拖拽分栏（Dock 系统） | `DockArea` / `TabGroup` / `Panel` / `TabPanel` | `crates/base/src/dock/dock_area.rs` 等 | 4,226 / 1,815 / 310 / 1,899 |
| Tab 标签页 | `TabBar` / `Tab` / `Tabs` | `crates/component/src/tab/*`、`crates/base/src/tabs.rs` | 1,457 / 1,242 / 428 |
| 虚拟滚动列表 | `VirtualList` | `crates/base/src/virtual_list.rs` | 905 |
| 树 | `Tree` | `crates/base/src/tree.rs` | 715 |
| 右键菜单 | `ContextMenu` / `PopupMenu` | `crates/component/src/menu/*` | 870 / 1,556 |
| 可拖拽分隔条 | `ResizablePanelGroup` | `crates/component/src/resizable.rs` | 148 |
| 深色画布 | `Theme`/`ThemeTokens` + `themes/*.json`（21 套，含 catppuccin/gruvbox/tokyonight） | `crates/base/src/theme*.rs` | — |

⚠️ **本仓当前一处都没用过**（`DockArea`/`VirtualList`/`TabBar` 在 `crates/*/src` 里 grep 无命中）——首次使用需按当前源码核对 API（AGENTS §3.2 红线）。

### F7 · `zedis` 与本仓 GPUI 栈完全同版

`temp/zedis` 用 `gpui-pre = 0.3.8` + `gpui-kit = 0.7.1`——**与本仓逐字一致**（唯一差别：本仓把 `gpui-pre` patch 到 gpui-fast）。它的 `ssh_tunnel.rs`(1,611) 是「系统 known_hosts 只读 + 自管可写 + agent 认证 + ProxyJump 单跳 + 会话复用清理」最完整的一份实现，**其 UI 代码也可按 0.7.1 直接取用**。

### F8 · 都不需要 `portable-pty`

zed 与 oxideterm 都直接用 `alacritty_terminal` 自带的 `tty` 模块（底层 `rustix-openpty`；Windows 走 ConPTY）。**引入 `alacritty_terminal` 后 `portable-pty` 不必引入**，少一条 `nix 0.28 + winapi 0.3` 依赖链。

---

## 4. 方案对比与推荐

### 4.1 三条候选路线

| | 路线 ① zed 主导 | 路线 ② oxideterm 主导 | 路线 ③ 纯自研 |
| --- | --- | --- | --- |
| 终端内核 | zed `terminal`（8.3K） | oxideterm `terminal` + `terminal-model`（29.5K） | `alacritty_terminal` 自封装 |
| 终端渲染 | zed `terminal_element.rs`（2K，**gpui 零改造**） | oxideterm `gpui-terminal`（36.7K，**需处理 0.3.7 补丁**） | 自写 4~6K 行 Element |
| 远程终端 | **自研胶水**（SSH channel → 内核 IO，400~800 行） | **现成**（`SshPty` 后端） | 自研 |
| SSH / SFTP | oxideterm `ssh`+`sftp`（31.3K，无 gpui） | 同左 | 基于 `russh` 自写 |
| 数据模型一致性 | 需在两端各留一层 | **统一**（`TerminalRow`/`TerminalCell`） | — |
| 自研量估算 | **4~6K** | **6~9K**（渲染适配是大头） | 15K+ |
| 主要风险 | 远程终端胶水的可行性 | `LinePaintCache` 补丁移植 | 工期与正确性 |

### 4.2 推荐：**路线 ①**（zed 终端 + oxideterm 远端）

**理由（按权重排序）**：

1. **gpui 零改造是最大杠杆**。渲染 Element 是整件事里唯一「必须自写、写错了很难看出来」的部分，而 zed 的 `terminal_element.rs` 是生产验证过的实现，且与本仓 gpui **签名逐字一致**（F1）。这一条就省下 4~6K 行。
2. **zed 内核比 oxideterm 内核小 3.5 倍**（8.3K vs 29.5K，均指生产行）。oxideterm 的内核把 Telnet/Serial/Mosh/triggers/graphics/tmux/trzsz/zmodem 全塞了进来，对一个工具箱是过度配置。
3. **远端层无论哪条路都来自 oxideterm**（它无 gpui、功能最全），所以选 zed 主导不影响远端能力上限。
4. **唯一不确定项集中且可早期验证**——「把 SSH channel 变成 zed 内核可消费的 IO」是一个可独立 spike 的问题（§9 的 S1）。

**明确不选路线 ②的理由**：它的最大优势是 `SshPty` 现成，但代价是渲染层要处理 `paint_cached*` 补丁与 36.7K 行适配，且要先接受 29.5K 行的内核（含大量本仓用不到的协议后端）。**若 S1 spike 失败，再回退到路线 ②**——两条路线的前置调研已在本文档内，切换成本可控。

### 4.3 路线 ① 的关键胶水（唯一的技术赌注）

zed 的 `Terminal` 内核通过 `alacritty_terminal::tty::Pty` 读写（`crates/terminal/src/alacritty.rs:183` 的 `tty::new`、`:214` 的 `EventLoop::new(...).spawn()`）。

**三条候选实现**（Spike 需要选定其一）：

| 方式 | 做法 | 评价 |
| --- | --- | --- |
| a. 借道 oxideterm 的 vendored alacritty | oxideterm 已经为了让 `SshPty` 工作而 patch 了 alacritty（见 `crates/alacritty-terminal/OXIDETERM_PATCHES.md`）——复用同一份 vendored 源码，直接获得「非本地 tty 也能驱动 ] EventLoop」的能力 | **首选**：改动量最小，且补丁有文档 |
| b. 本地 PTY 里跑 `ssh -t`（zed 的做法） | 不做 channel ↔ IO 对接，直接 `tty::new` 起一个 `ssh -t host` 子进程 | **最省**，但失去连接池共享（SFTP 无法复用同一条连接）、跳板机要交给系统 ssh、凭据交给系统 agent |
| c. 自研 `Pty` 抽象层 | 把 SSH channel 包成 `AsyncRead+AsyncWrite`，自己驱动 alacritty 的 `vte::Parser` | 最灵活、最贵（~800 行），但完全掌控 |

**建议**：先验证 (a)；(a) 不通则退 (b) 作为 M2 起点、(c) 作为 M3 的补齐。

---

## 5. 依赖与版本决策

### 5.1 依赖准入清单（已核）

| 用途 | crate | 版本 | MSRV | 门禁 |
| --- | --- | --- | --- | --- |
| 终端仿真 | `alacritty_terminal` | `0.26.0`（**vendored，从 oxideterm 拷**） | **1.85.0** | ⚠️ 恰好卡在门槛上，可入 |
| VTE 解析 | `vte` | `0.15.0`（vendored 或 crates.io，+`ansi` 特性） | — | ✅ |
| PTY | — | 由 `alacritty_terminal::tty` 提供 | — | ✅ **不引 `portable-pty`** |
| SSH | `russh` | 见 §5.2 | **1.85**（0.63.1） | ⚠️ **必须锁 `=0.63.1`**，或抬 MSRV |
| SFTP | `russh-sftp` | `3.0`（或 vendored `2.4`） | 未声明 | ⚠️ 携带**非可选** `tokio ^1` |
| 凭据 | `keyring` | `3.6.3` | 1.75 | ✅ **已在图内**，零新增 |
| 异步 I/O | `async-io` / `async-net` | 已在 lockfile（`2.6` / `2.0`） | — | ✅ 提为直接依赖即可 |

**不引入**：`portable-pty`（F8）、`ssh2`（C 依赖链）、`zip`(MSRV 1.88)。

### 5.2 `russh` 的版本裁决（**唯一需要拍板的前置项**）

实测事实：

| 版本 | MSRV | 其他方用法 |
| --- | --- | --- |
| `0.63.0` | ~1.85 | oxideterm 的 workspace 名义版本（实际 patch 到 `AnalyseDeCircuit/russh@df41ee42`） |
| **`0.63.1`** | **1.85** ✅ | **Oxidal 正在用** |
| `0.63.2` / `0.63.3` | **1.89** ❌ | zedis 用 0.63.3（它没有 MSRV 1.85 的约束） |
| `0.64.x` | **1.89** ❌ | — |

**结论**：`0.63.1` 是本仓 MSRV 下可用的最高版本。建议直接钉 `russh = "=0.63.1"`，**不跟 oxideterm 的 git fork**（多一条 git 依赖、MSRV 未声明，收益不明）。

**对特性开关的建议**：`russh` 默认特性含 `aws-lc-rs`（需 CMake/汇编工具链）。建议改为：

```toml
russh = { version = "=0.63.1", default-features = false, features = ["ring", "flate2", "rsa"] }
```

（Oxidal 与 zedis 都是这么关的；具体可用性需在 S2 验证。）

### 5.3 `tokio` 问题（ADR-4 保留，但不再是阻塞项）

`russh` 与 `russh-sftp` 都把 `tokio ^1` 声明为**非可选**依赖，所以 `tokio` 一定进图。但 `tokio` 默认特性为空——**「引入 tokio crate」≠「必须起 tokio 运行时」**。

参考项目的实际做法很有说服力：**oxideterm 直接引了完整 tokio**（`rt-multi-thread` + `net` + `fs` + `process` + `macros`，见其 `Cargo.toml:235`），并跑得通。

| 路径 | 做法 | 取舍 |
| --- | --- | --- |
| A（保守） | 自实现 `russh::client::Config` 的 `Runtime`，挂在本仓 gpui 执行器 + `async-io`/`async-net` 上 | 不新增第二运行时；与 `duck-plugin-js` 的「后台执行器」模式一致。代价是自写适配层 |
| B（省事） | 直接引 `tokio`（`rt-multi-thread`+`net`+`time`），SSH 侧自建 runtime | 上游默认路径、样本最多（oxideterm 即此路）。代价是多一套线程池 |

**建议**：M2 起步用 B（照抄 oxideterm 的配置，风险最低），把 A 作为后续优化项——因为**这正是「能参考就参考」的最直接体现**。

---

## 6. 功能设计（分期）

原则不变：**先把终端跑起来，再叠远端，最后补文件**——因为 SFTP 复用 SSH 的连接与认证结果。

### M1 — 本地终端（地基）

- **内核**：搬 zed `crates/terminal` → `crates/duck-term`；写 `settings`/`theme`/`theme_settings`/`task` 四个 shim（300~600 行）。
- **渲染**：搬 `terminal_element.rs` → `crates/duck-terminal/src/element.rs`；自研 `CursorLayout`/`HighlightedRange` 替代（200~400 行，用 `paint_quad`/`shape_line`）。
- **交互**：标签页（`gpui-kit` 的 `TabBar` 或本仓工具箱页已有的自绘标签）、分屏（`DockArea` 或 `h_resizable`）、字体缩放、滚动条、链接悬停。
- **主题**：终端调色板独立成 token（见 §7）。
- **验收**：本地 shell 可交互；中文输入法上屏正确；回滚 ≥10k 行；`yes` 压测单帧 ≤16ms；内核层无头单测。

### M2 — SSH 远端

- **连接池**：借 `oxideterm-ssh` 的引用计数模型——`ConnectionEntry{ ref_count: AtomicU64, consumers: RwLock<Vec<ConnectionConsumer>>, sftp: Mutex<SharedSftpState> }`（`connection_registry.rs:249-253`），`acquire(config, consumer)` / `release(id, consumer)`（`:845-932`）。**一条物理连接被终端 / SFTP / 转发共用，最后一个消费者退出才断开。**
- **认证**：密码、公钥（含口令）、ssh-agent、keyboard-interactive（OTP/MFA）——oxideterm 的 `transport/auth.rs` 四种全有。
- **主机密钥**：fail-closed。顺序 = 系统 `~/.ssh/known_hosts`（只读）→ 自有 store → 未知主机弹指纹确认（TOFU）→ **变更必须显式确认**。参考 `zedis/ssh_tunnel.rs`(1,611) 与 `verve/known_hosts.rs`(182)。
- **配置导入**：`~/.ssh/config` 只读解析——**直接搬 `zedis-core/src/ssh_config.rs`(289 行，纯函数)**。
- **跳板机**：`ProxyJump` 单跳 → 多跳（oxideterm 的 `proxy_chain: Option<Vec<ProxyHopConfig>>`）。
- **端口转发**：`-L` 优先（`verve/port_forward.rs` 118 行可直接参考），`-R`/`-D` 后置。
- **断线**：Grace Period 重连状态机（`oxideterm-ssh/src/reconnect.rs`）。
- **远程终端**：见 §4.3 的三条胶水路线。

### M3 — SFTP 文件管理

- **协议层**：搬 `oxideterm-sftp`（11,662 行，含 `transfer_manager`/`conflict`/`retry`/`scp`/`tar`）。
  - 关键 API：`download_file` / `upload_file` / `download_with_resume` / `upload_with_resume` / `download_dir` / `upload_dir`（`session/transfers.rs:106/139/174/284/406/506`）。
- **UI**：双栏（本地 ↔ 远端）——**参照 `Oxidal/src/sftp/panel.rs`(1,924)**：双栏 + `uniform_list` + 拖放上传（`DragMoveEvent`/`ExternalPaths`/`FileDrag`）+ 右键菜单 + `Progress`。
- **传输队列**：**参照 `verve/src/ui/sftp_browser.rs`(1,339)** 的 actor 模式（UI 发 `SftpCmd` → 后台线程 → 结果回灌 `cx.spawn` 循环），含 `QueuedUpload` 批量、冲突预检、`TransferState` 进度。
- **零成本复用点**：本仓已支持 `ExternalPaths` 拖放（P1-5 已落地），**「从访达拖文件上传」可直接复用**。
- **安全**：远端路径规范化，拒绝 `..` 逃逸。

### M4 — 后置

- 连接快速搜索 / 命令片段 / 登录自动执行；终端触发器（`oxideterm-terminal-triggers` 1,895 行可参考）；会话录制（`oxideterm-terminal-recording`）；zmodem（`verve/zmodem.rs` 988 行）。
- **以插件形态暴露**：建议**不做**——`duck-plugin-js` 的授权体系里没有「出站到任意主机」这一档，`duck.fetch` 只放行 https。

---

## 7. 数据与存储设计

| 数据 | 落点 | 格式 | 备注 |
| --- | --- | --- | --- |
| 连接档案 | `kv: ssh_profiles` | JSON 数组 | **不含任何口令**；含 `secret_ref` 键名指针 |
| 口令 / 私钥口令 | `duck-platform::secret`（keyring 3.6.3） | 键值 | 键名如 `ssh/<profile-id>/password`（≤128B，仅 `[A-Za-z0-9._:-]`） |
| 主机密钥（自有） | `kv: ssh_known_hosts` | JSON / 文本行 | 与系统 `~/.ssh/known_hosts` **只读叠加**，不写系统文件 |
| 标签布局 / 滚动位置 | `kv: terminal_session` | JSON | 会话恢复 |
| 终端调色板偏好 | 并入 `kv: appearance` | `AppearanceWire` | **新增字段一律 `Option` + `#[serde(default)]`**（旧存档逐字段一致有单测钉） |

**存储范式沿用 `appearance`**（结构体 → `serde_json` → 单个 kv 键），M1~M3 **不开新表**——`duck-store` 的迁移是只追加的 `MIGRATIONS` 数组，新表是「一旦加上撤不回」的结构性承诺。

**凭据只走钥匙串，且「用时才读」**（学 `tusk/src/db.rs:431/463`）——不在启动时读，避免 macOS 首次弹授权框阻塞首帧。`duck-platform::secret` 目前**零调用方**，SSH 凭据是它天然的落点。

---

## 8. UI 设计

### 8.1 整体布局

沿用本仓「活动栏 + 侧栏 + 内容区 + shell 级右栏」骨架：

```
┌──────────────────────────────────────────────────────────────────────┐
│ 自绘标题栏                                                            │
├────┬──────────────────┬───────────────────────────────────────────────┤
│活动│ 连接列表（左栏）  │  标签页  [ 本地 zsh ] [ prod-01 ] [ sftp ] +  │
│栏  │ ┌──────────────┐ ├───────────────────────────────────────────────┤
│    │ │ 🔍 搜索      │ │                                               │
│ ▣  │ ├──────────────┤ │        终端字符网格画布                        │
│ ▤  │ │ 分组：生产    │ │      （深色底，独立调色板 token）              │
│ ◫  │ │  ● prod-01   │ │                                               │
│ ...│ │  ○ prod-02   │ │                                               │
│    │ │ 分组：本地    │ │                                               │
│    │ │  ○ 本地 zsh  │ │                                               │
│    │ └──────────────┘ │                                               │
│    ├──────────────────┴───────────────────────────────────────────────┤
│    │ 状态栏：● 已连接 · 12ms · UTF-8 · 80×24 · 回滚 4096 行             │
└────┴──────────────────────────────────────────────────────────────────┘
```

- **左栏挂 shell 级 `with_sidebar`**（`duck-components/src/sidebar.rs:191`，自带可拖拽分栏），**不要嵌进页面**（`flex_1` 在 gpui-fast 的 Block 容器里会塌高，本仓已踩过）。
- **右栏留给「会话详情 / 传输队列」**，用 shell 级 `with_right_sidebar`。
- 状态栏复用 `content/data.rs:250` 的 `status_bar` + `status_cell`。

### 8.2 组件复用清单（**全部现成，零自研**）

| 需求 | 复用 | 备注 |
| --- | --- | --- |
| 标签页 | `gpui-kit` `TabBar`/`Tab`，或本仓工具箱页自绘标签（`toolbox.rs:393`，带右键菜单与中键关闭，**更接近终端用户预期**） | 建议沿用后者保持一致性 |
| 分屏 / 可拖拽分栏 | `DockArea`（4,226 行）/ `h_resizable` | ⚠️ `h_resizable` 拖拽把手上**挂不了双击复位**（上游硬编码，已定案不做） |
| 连接列表 | `Tree`(715) 或本仓 `toolbox.rs` 的分类折叠范式（`collapsed: HashSet<ToolCategory>`） | |
| 右键菜单 | `gpui_component::menu::ContextMenuExt`（现成用法 `toolbox.rs:512`） | |
| 虚拟滚动 | `VirtualList`(905) | 回滚缓冲、SFTP 大目录、传输队列三处都需要 |
| 空态 | `content/feedback.rs:30` 的 `empty_state` | |
| 深色画布 | `ThemeTokens` + 内置 21 套主题（catppuccin/gruvbox/tokyonight…） | |

### 8.3 终端画布（自绘 Element）

**搬 zed `terminal_element.rs`，改造点集中**：

1. **布局**：`layout_grid()`（`@438`）把 cell 流合并成三个 Vec——`BatchedTextRun`（文本 run）、`LayoutRect`（背景矩形）、`BlockElementLayoutRect`（块字符子格）。
   - 文本 run 合并：`BatchedTextRun::can_append`（`@123`）/ `append_char`（`@131`）；绘制走 `shape_line(text, font_size, &[style], Some(cell_width)).paint(...)`（`@161-177`）。
   - 背景合并：`merge_background_regions`（`@360`）→ `window.paint_quad(fill(...))`（`@265`）。
2. **字体度量**：`text_system.advance(font_id, font_pixels, 'm').unwrap().width` → `cell_width`（`@1261`）；`line_height = font_pixels * line_height`（`@1258`）。**度量结果必须缓存**（键含字号/字体/缩放）。
3. **视口裁剪**：全可见走无裁剪快路径（`@1417`）；部分可见用 `chunk_by(line).skip(rows_above).take(visible_row_count)`（`@1443-1449`）。
4. **像素对齐**：Standalone 模式做整行吸附（`@1281-1326`）+ origin 设备像素吸附（`@1316-1321`）。
5. **背压**：高吞吐输出（`yes`）时**不得逐次 `cx.notify()`**——按帧节流，缓冲积压超阈值时丢中间帧。
6. **纪律**：所有布局/着色结果在 `prepaint` 算好存进 `LayoutState`，`paint` 只做绘制（本仓日志查看器已确立「计算绝不放 render」）。
7. **需要替换的 zed 私有类型**：`editor::{CursorLayout, HighlightedRange, HighlightedRangeLine, EditorSettings}`（自研 ~200-400 行）、`ui::{Tooltip, ensure_minimum_contrast}`、`language::CursorShape`、`theme_settings::ThemeSettings`、`workspace::Workspace`（仅用于打开链接）。

### 8.4 快捷键与冲突

- **冲突现状**：`$mod-1`~`$mod-6` 已被 6 条路由占用（`duck-app/src/actions.rs:130-143`）。
- **处理**：引入 **key context 隔离**——终端画布聚焦时挂 `key_context("Terminal")`，终端专属绑定注册在该 context 下。
- **待验证**：`duck-command` 的默认键位来自 `ActionMeta.keys`（`keymap.rs:35-60`），**需确认它是否支持 key context 维度**；不支持则需扩展（AGENTS 已标注为 API 缺口）。
- 字体缩放 ⌘+ / ⌘- / ⌘0；**终端字号应作为独立设置项**（终端要等宽字体，UI 是比例字体）。

### 8.5 主题（ADR-7 保留）

ANSI 16 色 + 光标/选区/背景**不复用** UI 语义槽位，而是在 `duck-theme` 新增一组**终端调色板 token**（内置 2~3 套 + 深浅跟随），纳入 `Appearance` 的 `ui_scale` / `Density` 两条轴联动。

**理由**：终端背景常是「深色画布即便在浅色 UI 下」，而 UI 语义色的对比度守卫是按 UI 底算的——混用会同时破坏两边的对比度保证。

---

## 9. 集成点清单

| 文件 | 改动 |
| --- | --- |
| `crates/duck-app/src/app.rs:608` | `PlaceholderPage::new("SSH 终端", DuckIcon::SquareTerminal)` → 真实页实体。**路由常量、id、`ShellRoute::new` 那一行都不用改** |
| `crates/duck-app/src/app.rs` 路由登记表 | 加 `.with_sidebar(...)`（左栏连接列表） |
| `crates/duck-app/src/actions.rs` | 新增 ActionMeta（新建标签/连接、打开 SFTP、分屏、字体缩放…）→ 快捷键/菜单/命令面板/活动栏**四处输出面同时生效** |
| `crates/duck-theme/src/i18n.rs` | 新中文文案补 `EN` 表（**按 UTF-8 字节序插入**，否则覆盖率守卫失败） |
| `crates/duck-theme`（内置 token） | 终端调色板（§8.5） |
| `crates/duck-store` | **不改**（M1~M3 走 kv） |
| 根 `Cargo.toml` | 新增 workspace 依赖 + 版本理由注释；`[patch.crates-io]` 加 vendored `alacritty_terminal` / `vte` |
| `assets/licenses/THIRD_PARTY_LICENSES.md` | **必须重生成并提交**（双重门禁） |
| 新增 crate | `crates/duck-term`（内核 + shim）、`crates/duck-ssh`（SSH/SFTP 传输，无 gpui）、`crates/duck-terminal`（GPUI 视图层）。**依赖方向严格向下，内核层不得出现 `gpui::` 类型** |

---

## 10. 风险与待验证（Spike 清单）

| # | 风险 | 影响 | 处置 |
| --- | --- | --- | --- |
| **S1** | **远程终端胶水**：SSH channel ↔ zed 内核 IO（§4.3 三条路线） | **决定路线 ① 是否成立** | **第一优先**：最小 demo——一条真 SSH 连接打通 PTY shell |
| S2 | `russh =0.63.1` + 关默认特性（去 `aws-lc-rs`、改 `ring`）是否完整可用 | 构建工具链复杂度 | 与 S1 同一 demo 内验证 |
| S3 | vendored `alacritty_terminal 0.26.0`（oxideterm 那份）能否直接支撑 zed 的 `terminal` crate | 决定 F3 的统一方案 | 编译即知；失败则改用 zed 的 git fork |
| S4 | `tokio` 实际体积 / 编译开销（oxideterm 已引完整 tokio） | 二进制体积、编译时间 | `cargo tree -e features` + 构建耗时实测 |
| S5 | `VirtualList` / `DockArea` / `TabBar` **本仓首次使用** | 回滚缓冲、SFTP 目录、传输队列、分屏 | 按 `gpui-fast` 当前源码核对签名（AGENTS §3.2），勿凭记忆 |
| S6 | 自绘 Element 高吞吐输出的帧率与 CPU | 体验底线 | `yes` / 大文件 `cat` 压测，目标单帧 ≤16ms |
| S7 | CJK 双宽 / 连字 / emoji | 中文用户体感 | M1 **必须**支持 CJK 双宽；连字后置 |
| S8 | `duck-command` 是否支持 key context 维度 | 终端内快捷键能否与全局隔离 | 读源码确认；不支持则扩展 |
| S9 | Windows ConPTY / Linux 行为 | 跨平台（CI 有 `windows-check` 腿） | M1 起保持 `check --all-targets` 绿 |
| S10 | a11y：自绘画布对读屏等于不存在 | 无障碍 | 挂 `role` + 显式 `aria_label`；**本仓无法自动断言，需人工清单** |
| S11 | 搬运代码的后续上游同步成本 | 长期维护 | vendored 目录加 `README` 记录基线 rev 与补丁清单 |

> v1 里的 S1（russh MSRV）已升级为 §5.2 的**决策项**；S10（GPL 传递依赖）随本次前提变更已消解。

---

## 11. 里程碑与验收

| 里程碑 | 交付 | 验收判据（可测） |
| --- | --- | --- |
| **S0 Spike** | S1~S3 结论 + 一个 demo 分支 | 真机 SSH 连接成功 + 本地 PTY shell 可交互；给出远程终端胶水路线的裁决 |
| **M1 本地终端** | `duck-term` + `duck-terminal` + 替换 `ssh` 占位页 | ① 本地 shell 可交互，中文输入法上屏正确；② 回滚 ≥10k 行；③ `yes` 压测单帧 ≤16ms；④ 标签页增删切换；⑤ 深浅主题与两条轴联动；⑥ 内核层无头单测覆盖输入编码/快照/选择；⑦ 新增文案全部有英译 |
| **M2 SSH** | `duck-ssh` + 连接列表 + 远程终端 | ① 密码/公钥/agent 三种认证各打通一次；② 未知主机必须确认、密钥变更被拒；③ `~/.ssh/config` 导入生效；④ 单跳跳板机可用；⑤ 断网重连可恢复会话；⑥ SQLite 内 grep 不到口令 |
| **M3 SFTP** | 双栏 + 传输队列 | ① 双向传输递归目录；② 断点/重试/冲突策略正确；③ 从访达拖放上传可用；④ 进度与系统通知；⑤ 路径越界被拒 |
| **M4 后置** | 触发器 / 录制 / zmodem | 按需另评 |

---

## 12. 一句话总结

**终端内核与渲染层直接搬 zed（`gpui` 签名逐字一致，几乎零改造）；SSH/SFTP 协议层整包搬 oxideterm（35 个内核 crate 全无 gpui）；UI 外壳用本仓已依赖的 `gpui-kit 0.7.1`（Dock/Tab/VirtualList/Tree/ContextMenu 全现成）。真正要自研的只剩三件：远程终端的 IO 胶水、连接/凭据存储的精简版、SFTP 面板 UI——合计约 3.5~6.4K 行，比 v1 估算少一个数量级。**

**唯一需要先拍板的是 `russh` 版本**：本仓 MSRV 1.85 下最高只能用 `=0.63.1`（0.63.2+ 起要 1.89）。
**唯一需要先验证的是 S1**：SSH channel 能否驱动 zed 的终端内核——这一个 spike 的结果决定「路线 ①（zed 主导）」是否成立。
