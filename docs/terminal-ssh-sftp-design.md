# 终端 / SSH / SFTP 功能：架构与 UI 设计方案（v2 · 最大化复用版）

> 状态：**M1 已落地、M2/M3 按路线 (b) 落地，本文兼作实施记录与后续设计依据**。
> 修订：2026-10-11 · v2 取代 v1。**前提变更：不再以许可证为约束**，主轴从「隔离墙」改为「能搬就搬」。
> 修订：2026-10-12 · 补 §4.3 的实测裁决（路线 (a) 不成立 → 采用 (b)）与实施现状表。
> 依据：仓库现状 + `temp/` 下 26 个参考项目逐目录调研 + crates.io 版本核验
> 适用约束：`AGENTS.md`（硬条文）· `rust-version = "1.85"` · `gpui-pre =0.3.8`（gpui-fast `b8f0c026`）

### 实施现状（2026-10-12，与本文计划的差异）

计划里的 `crates/duck-terminal`（GPUI 视图层）**没有单独建 crate**：终端画布直接落在 `crates/duck-app/src/pages/terminal_canvas.rs`，因为视图层只有这一个页面用，多一层 crate 只增加改动面。

| 计划 | 实际 |
| --- | --- |
| `crates/duck-term`（内核 + shim） | ✅ 已落地：本地真实 PTY，无 gpui 依赖 |
| `crates/duck-ssh`（走 oxideterm-ssh / russh） | ✅ 已落地，但**零依赖**（只用 `std::process`）：驱动系统 `ssh` / `scp`，见 §4.3 |
| `crates/duck-terminal`（渲染层） | ❌ 未单独建 crate → 并入 `duck-app` 的 `pages/terminal_canvas.rs` |
| `gpui-kit` 的 `DockArea` 做分屏 | ❌ 未用 Dock：**分屏已按「一条标签两条会话」自实现**（工具条图标 + `⌘D` + 左栏右键「在分屏中打开」+ **拖标签过中线并入**，支持不同连接，见 §8.6）。死图标那一版 2026-10-12 已撤 |
| 连接档案 `kv: ssh_profiles` | ✅ 已落地；**首次启动为空**，界面给空态引导（不给编造的预置连接） |
| 主机密钥二次确认（TOFU） | ❌ 未做：交给系统 `~/.ssh/known_hosts` 与用户自己的 `StrictHostKeyChecking`，**不替用户决定** |
| 端口转发 / 多跳跳板机 | ❌ 未做（M4） |

> 页面里**不存在任何演示数据**：连接列表、标签、远端目录、传输队列全部来自真实来源，取不到就如实说明（空态 / 「尚未建立 SFTP 会话」/ 失败原因），**不编造一行**。

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
| 连接/凭据存储精简版 | 1,000~2,000 行 | 不搬 `oxideterm-connections`(29,898)——只需「JSON 档案 + 加密口令 + ssh_config 导入」三件事 |
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

#### 实测结论（2026-10-12）：**路线 (a) 不成立，已采用 (b)，S1 结案**

**(a) 为什么不通。** 前提是「本仓 vendored 的那份 alacritty 已经为了让 `SshPty` 工作打过补丁」。核对源码后不成立：本仓 vendored 的 alacritty **只补了 ASCII 快路径**，没有 `SshPty` 后端 —— 也就是说「把 SSH channel 喂给 `EventLoop`」这件事**没有现成实现可借**，路线 (a) 的整个杠杆点（复用别人的补丁）落空。若坚持走 (a)，实际要做的是 (c)（自研 800 行 IO 胶水），而不是「改动量最小」。

**(b) 的落地形态。** 由系统 `ssh` 起真实 PTY（`ssh -t host`）。关键事实：`duck-term` 的 `SpawnOptions.shell: Option<(String, Vec<String>)>` **本来就能接任意 argv**，所以远程终端这一层**内核一行都没改**。

**表里对 (b) 的三条顾虑，实际结果如下**：

| 顾虑 | 实际结果 |
| --- | --- |
| 失去连接池共享（SFTP 无法复用同一条连接） | **不成立**：OpenSSH 自己的 `ControlMaster` / `ControlPath` / `ControlPersist=300` 就是连接池。终端标签与 SFTP 列举/传输共用同一条物理连接，握手不重来。**不需要自研连接池** |
| 跳板机要交给系统 ssh | **是本路线的收益而非代价**：`~/.ssh/config`、agent、公钥、密码、`ProxyJump`、`ProxyCommand` 全部原样继承 |
| 凭据交给系统 agent | 密码走 `SSH_ASKPASS` 助手脚本 + 环境变量注入，**口令绝不进 argv**（`ps` 对同机所有用户可见）；不设口令时**一个 askpass 变量都不设**，否则本来能走公钥的连接会卡在等助手 |

**因此 §6 的 M2 清单大幅缩水**：认证四件套、`~/.ssh/config` 解析、跳板机、主机密钥确认，**全部改为「继承系统 OpenSSH」**；主机密钥走系统 `~/.ssh/known_hosts`，fail-closed 且**不替用户决定**。自研部分只剩 `crates/duck-ssh` 一层薄封装（构造 argv + 跑进程 + 解析结果），**零依赖、不认识 gpui 与 duck-term**。

**代价（如实记下）**：需要机器上有 `ssh` / `scp`（macOS / Linux 自带；Windows 10+ 需「可选功能 → OpenSSH 客户端」）。缺失时 `duck_ssh::probe()` 如实报告、界面禁用入口，**不静默失败**。

> ⚠️ **订正（2026-10-13）**：原文此处写「**刻意不做自动重连**」，与代码不符 —— 断线重连**已落地**（`docs/module-gap-analysis.md` §7 的 G-CONN-1）。形态＝**手动重连**（`RespawnPlan` 重放 argv + 网格尺寸）**＋带护栏的自动重连**：退避 2s 起、×2 封顶 60s、最多 5 次、**认证 / 主机密钥类不重试**、连接存活 ≥10s 才把尝试计数清零（`crates/duck-app/src/pages/terminal_canvas.rs` 的 `AutoReconnect` / `reconnect_after_disconnect`，纯数据、可单测）。
>
> 原设计里那条顾虑（「重试一百次也不会好，只会把真实错误刷出屏幕」）**没有被丢掉，而是内化成了护栏**：不可重试的错误类直接落 `Stopped`，手动「重新连接」横条照旧保留。

**仍未做的（M4 之后）**：多跳 `ProxyJump` 链的**可视化配置**（运行时已由系统 `ssh` 继承 `~/.ssh/config`，缺的是链式编辑器）、会话录制。
> ⚠️ **订正（2026-10-13）**：原列的**端口转发已做** —— 内核 `crates/duck-ssh/src/target.rs`（`ForwardKind::Local/Remote/Dynamic → -L/-R/-D`，独立 argv 不走 `-o`，单测 `port_forward_specs_follow_openssh_syntax`）＋界面 `crates/duck-app/src/pages/terminal_dialogs.rs` 的转发行与档案持久化。

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
| 凭据 | ~~`keyring`~~ | ~~`3.6.3`~~ | — | ❌ **不引入**：2026-10-12 起凭据走 `duck-platform::{vault,secret}`（AES-256-GCM），`keyring` 已从图内删除，见 §15.4 |
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
| 连接档案 | `kv: ssh_profiles` | JSON 数组 | 含**口令密文**字段 `password`（见下）；含策略位 `remember` 与私钥路径 |
| 口令（密文） | 档案行内 `password` 字段 | hex(`nonce` ‖ AES-256-GCM) | 加密走 `duck-platform::vault`，密钥在 `app_data_dir()/vault.key`（0600）；SQLite 里 grep 不到明文 |
| 主机密钥（自有） | `kv: ssh_known_hosts` | JSON / 文本行 | 与系统 `~/.ssh/known_hosts` **只读叠加**，不写系统文件 |
| 标签布局 / 滚动位置 | `kv: terminal_session` | JSON | 会话恢复 |
| 终端调色板偏好 | 并入 `kv: appearance` | `AppearanceWire` | **新增字段一律 `Option` + `#[serde(default)]`**（旧存档逐字段一致有单测钉） |

**存储范式沿用 `appearance`**（结构体 → `serde_json` → 单个 kv 键），M1~M3 **不开新表**——`duck-store` 的迁移是只追加的 `MIGRATIONS` 数组，新表是「一旦加上撤不回」的结构性承诺。

**口令加密落库，不走系统钥匙串**（2026-10-12 下午改判，见 §15 追记（四））：SSH 口令要「读回来交给 askpass 自动登录 + 编辑弹窗解密回显」，而本仓 dev 构建是 ad-hoc 签名，每次重编译钥匙串 ACL 都失配、读一次弹一次系统授权框 —— 用户实测「点连接弹系统密码器」即此。改走 `duck_platform::vault`（AES-256-GCM，ring 已在依赖图，零新增传递依赖），密文存进档案行随档案一起增删改。「用时才读」的口径保留（连接时才解密），只是读取不再有任何系统交互。

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

### 8.4 快捷键与冲突（M1 已落地，2026-10-12 复核）

#### 机制：gpui 的派发顺序决定的「铁律」

- **派发顺序**：`gpui` 的 `window.rs::dispatch_key_event` 沿焦点链**先匹配 action 绑定**并逐个派发；
  **全部未命中**才 `finish_dispatch_key_event` 把 `KeyDownEvent` 交给 `on_key_down`。
  → **`on_key_down` 不是第一优先**，这与多数人（包括我第一版设计）的直觉相反。
- **抢占者**：`gpui-base-0.7.1/src/root.rs:14-23` 在 `"Root"` 上下文全局绑了
  `"tab" → Tab`、`"shift-tab" → TabPrev`，以及 macOS 的 `"cmd-c" → Copy` /
  非 macOS 的 `"ctrl-c" → Copy`。窗口的**任何**按键派发都经过 `Root`。
- **修法（= zed / Oxidal 同款）**：画布挂 `.key_context("Terminal")`；终端专属动作用
  `actions!(terminal, [...])` 声明，在 `app::install` 的 `cx.bind_keys` 里注册到
  **更深的 `"Terminal"` 上下文** —— 更深上下文胜出，`Root` 的同键动作不再触发。
  这正是 zed 键位表里那句注释的所指：
  *"There are conflicting bindings for these keys in the global context. these bindings
  override them, remove at your own risk"*（`temp/zed/assets/keymaps/default-macos.json`）。
- **无头回归**：`crates/duck-app/tests/terminal_tab_key.rs`（2 例）——
  ① 画布聚焦时 `Terminal` 动作被派发、`Root` 的动作**不**触发；
  ② 焦点在画布外时 Tab 仍走 `Root` 的焦点遍历（证明绑定收窄到了 `"Terminal"`）。
- **通用纪律**：`duck_term::keymap::encode` 第 0 步**拒收 `platform` 修饰键**（macOS ⌘ / Windows ⊞）。
  走到编码层就说明没有任何绑定命中，此时若把字母写进 PTY，用户按 `⌘A` / `⌘S` 会在 shell 里
  凭空多出一个字母。规则放在内核而不是 UI 层：可单测，且新增调用点漏填 `platform` 会编译报错。

#### 已实现键位（`app::install` 的 `"Terminal"` 上下文）

| 键（macOS） | 动作 | 去向 |
| --- | --- | --- |
| `Tab` / `Shift-Tab` | `SendTab` / `SendTabPrev` | PTY `\t` / `CSI Z` |
| `Ctrl-C` | `SendCtrlC` | PTY `0x03`（SIGINT）——**非 macOS 上必须重绑**（`Root` 占着它） |
| `⌘=` `⌘+` `⌘-` `⌘0` | `ZoomIn` / `ZoomOut` / `ZoomReset` | 宿主：字号（钳 10~20px，复位到 `mono_md`） |
| `⌘K` | `Clear` | PTY `CSI 2J 3J H` + 视口回底（作用于**当前窗格**） |
| `⌘D` | `SplitPane` | 宿主：分屏 / 取消分屏（右侧再开一条会话，见 §8.6） |
| `Shift-↑` / `Shift-↓` | `ScrollLineUp` / `ScrollLineDown` | 宿主：视口 ±1 行 |
| `Shift-PgUp/PgDn`、`⌘↑/⌘↓` | `ScrollPageUp` / `ScrollPageDown` | 宿主：视口 ±(行数−1)，留一行重叠 |
| `Shift-Home/End`、`⌘Home/End` | `ScrollToTop` / `ScrollToBottom` | 宿主：视口顶 / 底 |
| `⌘←` / `⌘→` | `MoveToLineStart` / `MoveToLineEnd` | PTY `Ctrl-A` / `Ctrl-E` |
| `⌘⌫` / `⌘⌦` | `DeleteToLineStart` / `DeleteToLineEnd` | PTY `Ctrl-U` / `Ctrl-K` |
| `⌥⌦` | `DeleteWordForward` | PTY `ESC d` |
| `⌘C` | `CopySelection` | 宿主：把**选区**文字写进剪贴板（没有选区就什么都不做，不覆盖剪贴板） |
| `⌘V` | `PasteFromClipboard` | PTY：走**括号粘贴模式**（`\x1b[200~…201~`，见 §8.3） |
| `⌘A` | `SelectAll` | 宿主：全选（回滚区 + 当前屏幕） |
| `⌘T` | `NewTab` | 宿主：新建本地标签（与工具条 `+` 同一入口 `new_local_tab`） |
| `⌘W` | `CloseTab` | 宿主：关闭当前标签（与标签行 `✕` 同一入口 `close_tab_at`） |

> `⌘C` / `⌘A` / `⌘W` 三个键在 `gpui-base` 的 `"Root"` 上下文里已有全局绑定
> （`Copy` / `SelectAll` / `Close`，见 §8.4 开头）。它们**必须**在更深的 `"Terminal"`
> 上下文重绑，否则按下去触发的是 gpui 的通用动作而不是终端自己的 —— 这与 `Tab`、非 macOS 的
> `Ctrl-C` 是同一类问题（`tests/terminal_tab_key.rs` 钉的就是这条）。
> `⌘X`（剪切）**故意不绑**：终端里的文字在远端，本地没有「剪掉」的语义 ——
> 剪贴板里放什么、远端删不删，两个问题都没有正确答案，不如留给用户自己用 `⌘C`。

**未占用的键无需绑定**：方向键、`Home/End`、`PgUp/PgDn`、`Ins/Del`、`F1~F12`、`Enter`、`Esc`、
`Ctrl+字母`、`Alt+字母`、可打印文本与 IME 上屏，都直接经 `on_key_down` → `encode`。
它们是安全的：`gpui-base` / `gpui-component` 里 `up`/`down`/`escape`/`enter`/`pageup` 等绑定
**都带自己的 context**（文本输入、下拉、表格、命令面板），而画布的焦点路径里没有这些 context。
（`ctrl-g` 那条 `None` 上下文的绑定在 `#[gpui::test]` 里，不是生产代码。）

#### 与 zed 的差距（未做项，附原因）

| 缺什么 | 为什么没做 |
| --- | --- |
| ~~`⌘C` 复制 / `⌘A` 全选~~ | ✅ **2026-10-12 已实现**：内核加了**选区模型**（`alacritty_terminal::Term::selection` 当唯一权威 + `SelectionRange` 转网格坐标），画布拖选高亮、`context_menu`、`⌘C`/`⌘V`/`⌘A` 一起补齐 —— 见 §8.4 键位表。剪切 `⌘X` **故意不做**（终端里的文字在远端，本地没有「剪掉」的语义） |
| `⌘F` 搜索缓冲区 | 需要缓冲区搜索实现 |
| `Ctrl+Shift+C/V`（Linux/Windows 惯例） | 等非 macOS 平台打磨时补（`⌘C`/`⌘V` 的 Action 与绑定位置已经就位，补一对键位即可） |
| 鼠标上报（点击 → 应用）、vi mode | M4 后置 |
| 光标 / 选区配色独立于 UI 语义槽 | 见 §8.5（ADR-7）。**选区底色当前借 `extended.colors.drop_target`**，属同一笔待办 |
| ~~`⌘T` 新建标签 / `⌘W` 关闭标签~~ | ✅ **2026-10-12 已实现**：两个 Action 已加到 `"Terminal"` 上下文；右侧栏的快捷键列表同日改成只列真实键位，与 `app::install` 逐条对应 |
| ~~`⌘D` 分屏~~ | ✅ **2026-10-12 已实现**：真正要动的确实是会话表（一条标签两条会话）、焦点归属与尺寸上报 —— 都做了，见 §8.6 |
| 分屏的剩余边界：分隔线拖拽调宽 / 上下分屏 / 三格及以上 | 等宽 + 左右两格够用，见 §8.6「不做」清单；按需另评（**拖标签并入**不在此列，同日已实现） |

#### 一条**故意不跟**的 zed 做法

zed 把 `alt-b` / `alt-f` 绑成 `ESC b` / `ESC f`（注释写 "Terminal.app compatibility"）；
`alt-←/→` 绑成同义。**本仓只做后者**（`⌥←/→` 走 xterm 标准的 `CSI 1;3D/C`，已实现）。

不跟的原因：`⌥b` 在 macOS 上本该能打出 `∫`（`⌥2` = `™`、`⌥5` = `∞` 同理）。
把 `⌥b` 抢成 `ESC b` 会**拿走**用户输入特殊字符的能力，而词移动能力 `⌥←/→` 已经具备 ——
净效果是功能变少。要同时满足两者，需要「Option 作 Meta」开关（真正的终端都提供这个设置项），
那属于 M4 的设置面，M1 不引入。

⚠️ 相关已知细节：macOS 上按 `⌥+字母` 时 `keystroke.key_char` 是**组合后的字符**（`⌥f` → `ƒ`），
当前会发出 `ESC + ƒ`。严格 xterm 语义应发 `ESC + f`。同属上面那个开关的取舍，暂不改。

### 8.5 主题（ADR-7 保留）

ANSI 16 色 + 光标/选区/背景**不复用** UI 语义槽位，而是在 `duck-theme` 新增一组**终端调色板 token**（内置 2~3 套 + 深浅跟随），纳入 `Appearance` 的 `ui_scale` / `Density` 两条轴联动。

**理由**：终端背景常是「深色画布即便在浅色 UI 下」，而 UI 语义色的对比度守卫是按 UI 底算的——混用会同时破坏两边的对比度保证。

### 8.6 分屏（2026-10-12 落地）

**一句话**：分屏不是「同一块画布画两遍」，而是**一条标签挂两条会话**。

| 决定 | 做法 | 为什么 |
| --- | --- | --- |
| 数据模型 | `SessionTab.split: Option<String>` = 右窗格的**会话键**（`<标签 id>#2`，`terminal_data::split_session_key`） | 存会话键而不是 `bool`：会话表里有没有这一条，直接决定窗格里有没有东西可画（与「点连接只建标签、`ensure_sessions` 起会话」同一条规矩） |
| 第二条会话 | `Local` → 再开一个 shell；`Ssh` → 再开一条 `ssh -t`（同一个 `ControlMaster` socket，只多一个通道） | zed / tmux 的语义：每一个窗格就是一个独立会话。**不复制**左边的会话（两条流会互相打架） |
| 目标连接 | `SessionTab.split_owner: Option<String>` 记右窗格连**哪条**连接（资产 id / `local-shell`） | **不同连接分屏**：右窗格不必连左边那台机器。「缺省 = 标签自己的连接」由 `split_owner_for(kind, id)` 给出（本地标签映射到内置 `local-shell`，因为它 id 是标签序号、不是连接 id） |
| 拖拽并入 | 拖标签划过画布、**过中线** → 右半屏亮投放区，松手该标签变本画布右窗格（`merge_tab_into_split`，会话整条搬过去） | 主流终端（iTerm2 / Windows Terminal / Tabby）的通用手感。落点判定与视觉共用同一条中线：`on_drag_move` 按「是否过中线」翻转 `split_drop_hover`，`on_drop` 只在它点亮时执行（左半屏松手 = 取消） |
| 布局 | 左右各 `flex_1` + 中间 1px 发丝线（与 SFTP 双栏同一条 `hairline`） | 不做拖拽调宽：等宽够用，且 `h_resizable` 把手在本仓有已知问题 |
| 尺寸 | 各窗格各自把自己的像素宽度换算成列行上报（`apply_canvas_size(会话键, …)`） | 尺寸回传本来就带会话键，两半天然互不干涉 |
| 焦点 | 点哪一半就把键盘交给哪一半；`focus_pane` 记下会话键，供工具条「清屏」定位 | 「当前会话」不再等于「当前标签」 |
| `⌘K` 清屏 | 画布内的动作清**自己那一格**（动作就在该窗格里派发）；工具条按钮清 `focus_pane` | 由页面猜焦点需要窗口信息，而画布手里就有答案 |
| 退出 | 本地 shell 退出 → 只收那一半（标签留着）；SSH 退出 → 留着，错误就打在那一半上 | 与单标签时同一条规矩（`ChildExit` 的分支），`kinds` 表把分屏键也算进去 |
| 收尾 | 关标签 / 取消分屏都会 `sessions.remove` 掉第二条会话 | `Session::drop` 发 `Shutdown`，不留孤儿 shell |
| 入口 | ① 工具条图标（`Columns2`）+ `⌘D`（`SplitPane` 动作）→ 连**标签自己的连接**；② 左栏连接右键菜单「在分屏中打开」→ 连**点的那条**；③ 拖标签过中线并入（见上） | SFTP 标签禁用并把原因写进标签文案（没有 PTY 可分）。三条入口共用同一份数据（`split` + `split_owner`） |

**不做**（写在这里免得下次又被当成遗漏）：分隔线拖拽调宽、上下分屏、三格及以上、每标签独立字号、分屏布局持久化。（注意「拖拽」在本节有两个含义：**拖标签并入**已做，**拖分隔线调宽**不做。）

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
| **S1** | **远程终端胶水**：SSH channel ↔ zed 内核 IO（§4.3 三条路线） | **决定路线 ① 是否成立** | ✅ **已结案（2026-10-12）**：(a) 前提不成立（vendored alacritty 无 `SshPty` 后端），采用 **(b) 驱动系统 `ssh -t`**，`duck-term` 零改动。详见 §4.3 |
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
> §4.3 的路线裁决已落地：S1 结案、S2/S3 随「不引 russh / 不用 vendored alacritty」一起消失（`duck-ssh` 零依赖，只用 `std::process`）。

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

---

## 13. 2026-10-12 追记：S1 的答案，以及它推翻了什么

S1 问的是「SSH channel 能否驱动终端内核」。**答案是可以不驱动**：终端内核要的东西只是一个「可读可写的 IO」，而系统 `ssh` 在 PTY 下**本来就是一个 IO**——`ssh -t host` 起在真 PTY 里，内核看到的就是一条本地连接，一行都不用改（§4.3）。

这条答案连带推翻了本文的两处前提：

1. **`russh` / `tokio` 的整个版本裁决（§5.2 / §5.3）不再阻塞 M2**。它们只在「自研 SSH 客户端」那条路上才是前置项。当前实现**不引 `russh`、不引 `tokio`**，`duck-ssh` 零依赖。
2. **「搬 oxideterm-ssh / oxideterm-sftp 的 31.3K 行」不再是必要动作**。认证、`~/.ssh/config`、跳板机、主机密钥、连接复用（`ControlMaster`）都是 OpenSSH 已有的能力，**重新实现一遍只会更差**。真正自研的是 `duck-ssh` 那层薄封装。

**没有推翻的是 M1**：终端内核与画布仍然是自家实现（`duck-term` + `duck-app` 的自绘 Element），这是「中文双宽 / 输入法 / 主题联动 / 单帧预算」这些要求真正落点的地方。

**代价再强调一次**（写在文档里比写在提交信息里更不容易被忘掉）：这条路要求目标机器上有 `ssh` / `scp`。Windows 10+ 默认没有，需要用户在「可选功能」里装 OpenSSH 客户端。缺失时**如实报错并禁用入口**，不静默失败。

---

## 14. 2026-10-12 追记（二）：分屏落地 · SFTP 图标失灵 · 图标悬停提示

用户实测报告三条，逐条核实后修完：

1. **「分屏图标和功能没有实现」** → 真做了（§8.6）。此前那版是**死图标**（没有 `on_click`），按「不摆点不动的图标」原则撤掉；这次连会话表、焦点、尺寸、退出裁决一起补上，图标与 `⌘D` 同时可用。
2. **「SFTP 面板关掉后再点图标没反应」** → 根因：工具条那个图标的目标**只从当前标签推导**。面板一关，活动标签回落到本地 shell（或空态），于是两种坏结果二选一 ——
   - 活动标签是本地 shell：`connection_entry("local-shell")` 会返回**内置条目**，于是开出一个「SFTP · 本地 shell」的假面板，远端永远列举不出东西（看起来就是「打不开」）；
   - 连标签都没了：`active_tab()` 为 `None`，按钮直接置灰，点了当然没反应。
   
   修法：新增 `last_sftp_owner`（记住上次开过 SFTP 的连接）+ 纯函数 `resolve_sftp_owner`（**当前标签所属连接 → 上次的连接 → 都没有才置灰**），`open_sftp_tab` 再补一道「不认本地 shell / 不认真实档案之外的东西」的守卫，标签右键菜单里的「打开 SFTP 面板」也不再对本地标签出现。三条单测钉住「关了还能再开」。
3. **「页面的图标都没有 toptips 提示」** → 图标按钮此前只有 `accessibility_label`（读屏能吃、鼠标看不见）。提示语与读屏标签合并到一处：`duck-components` 的 `icon_button` / `toolbar_action_button` 两个构造器里统一挂 `Tooltip`，**一处收口覆盖全仓约 50 个图标按钮**。注意 disabled 分支也要挂：gpui-base 的 `Button::disabled` 仍保留命中区（只在 `on_mouse_down` 里拦传播），所以灰掉的图标同样能显示提示。

**依然没做**（承前）：`⌘F` 搜索、鼠标上报、TOFU 主机密钥确认、端口转发 / 多跳、分隔线拖拽调宽。
（`⌘T` / `⌘W` 键位与选区复制粘贴已于同日补齐，见 **§16**；`⌘X` 剪切是**有意不做**，理由见 §8.4 键位表下的注。）

---

## 15. 2026-10-12 追记（三）：ControlPath 报错 · 不同连接分屏 · 拖标签并入

用户实测报「无法连接」，并对分屏提出两点明确要求（不同连接也能分屏、拖标签过中线并入），逐条落实：

### 15.1 SSH 无法连接：`keyword controlpath extra arguments at end of line`

**现象**：任何 SSH 连接都报 `command-line line 0: keyword controlpath extra arguments at end of line`。

**根因**：`ControlPath` 的 socket 目录是 `~/Library/Application Support/DuckToolbox/ssh` —— **路径里有空格**。`ssh -o "ControlPath=<路径>"` 的值**不再重新加引号解析**，`Application Support` 之间的空格被 ssh 当成「行尾多余参数」。用真机 `ssh -G` 复现：

```
$ ssh -G -o 'ControlPath=/tmp/a b/x.sock' localhost
command-line line 0: keyword controlpath extra arguments at end of line   # 不带引号 → 复现
$ ssh -G -o 'ControlPath="/tmp/a b/x.sock"' localhost
controlpath "/tmp/a b/x.sock"                                            # 带引号 → 接受
```

**修法（双管齐下）**：
1. `duck-ssh::ConnectionTarget::argv` 里给 `ControlPath` 的**值加引号**（`ControlPath="…"`）—— 这层是通用防线，任何调 `duck-ssh` 的人都受保护，不依赖调用方给的目录「恰好无空格」。
2. `TerminalPage::ssh_socket_dir` 从 `app_data_dir()/ssh` 改到 **`cache_dir()/ssh`**（`~/Library/Caches/DuckToolbox`，天然无空格）。
3. 单测钉住「带空格的目录必须产出带引号的 `ControlPath`」。

> 为什么 socket 不放 `/tmp`（承前 §8 的决定）：`/tmp` 是共享的，别人预占同名 socket 就能劫持连接复用。

### 15.2 分屏支持不同连接

**用户要求**：分屏不止「同一个连接分两半」，还要能「左连接 A、右连接 B」。

**做法**：`SessionTab` 加 `split_owner: Option<String>` 记右窗格**连哪条**连接。`ensure_sessions` 的补会话循环从「(会话键, 类型, 连接 id) 三元组」升级为**按归属起会话** —— 分屏窗格连 `split_owner` 而不是标签自己。于是：

- 工具条图标 / `⌘D`：缺省连标签自己的连接（`split_owner_for`）。
- 左栏连接右键菜单「在分屏中打开」：连用户**点的那条**（本地 shell 与远端主机都行）。
- 已分屏时再点同一条连接 = 取消分屏；另一条 = 换目标（旧窗格会话就地收掉，新会话按新归属补起）。

**关键取舍**：右窗格的**类型**（`Local` / `Ssh`）跟着**归属**走，不跟标签 —— 左标签是 SSH、右窗格连本地 shell 是合法的，`split_kind()` 由 `split_owner` 反查。

### 15.3 拖标签过中线并入分屏

**用户要求**：参考主流软件，拖动 tab 越过中线时提示「是否并入分屏」。

**做法**（§8.6 决策表「拖拽并入」行的实现）：
- 标签行挂 `.on_drag(TabDrag { tab_id }, …)`（`TabDrag` 是载荷类型，另配一个圆角小卡片的 `TabDragGhost` 做拖拽预览）。
- 未分屏的画布包一层 `.on_drag_move::<TabDrag>` + `.on_drop::<TabDrag>` 的投放目标；`on_drag_move` 按指针是否越过容器中线翻转 `split_drop_hover`，`on_drop` 只在点亮时执行并入。
- 投放区视觉复用主题的专用语义色（`drag_border` 描边 + `drop_target` 底色），不另造第二套拖放语言；文案「松开并入分屏」。
- 并入 = 被拖标签**从标签栏消失、会话整条搬过去**当右窗格（对齐主流终端手感）。会话键从 `<被拖 id>` 迁到 `split_session_key(目标 id)`，`close_tab_at` 删的是已被搬空的键，不会误杀。
- 拖放结束的自愈：投放区渲染带 `cx.has_active_drag()` 守卫 —— 拖放一旦结束（无论落没落进），投放区下一帧自动消失，不靠额外的「拖放取消」事件。

**已知边界**：只有「未分屏 + 非 SFTP」的两个标签能并（本应用分屏是两条会话，没有第三格）；自己拖进自己被拒。


### 15.4 口令加密落库 + 编辑回显（钥匙串退场）

**用户实测**（2026-10-12 16:00）：① 保存口令后点连接仍失败，且**弹出 macOS 钥匙串授权框**（用户称为「系统密码器」）；② 编辑连接时已保存的口令不回显，要求反显、且连接信息要落数据库。

**根因**：口令此前走 `duck-platform::secret`（当时 = keyring → macOS 钥匙串）。本仓 dev 构建是 ad-hoc 签名，**每次重编译签名都变**，钥匙串条目 ACL 里的「创建者」随即失配 —— 读一次弹一次授权框，「始终允许」也救不了（授权跟着签名走）。且「编辑回显」本身要求应用能随时无提示地读，与钥匙串 ACL 模型根本相抵：两条用户诉求在钥匙串方案下**互斥**。

**做法**（对齐 PuTTY / Remmina / WindTerm 一类客户端的通用思路 —— 口令加密存自己的库）：

1. 新增 `duck-platform::vault`：AES-256-GCM（ring 提成直依赖，**零新增传递依赖**）。密钥 32 字节首次用时生成，落 `app_data_dir()/vault.key`（0600）；密文 `hex(nonce ‖ ct+tag)` 存进档案行。SQLite 里 grep 不到明文，M2 验收线「SQLite 内 grep 不到口令」继续成立。
2. `StoredProfile.password: Option<String>`（密文，`#[serde(default)]`，旧存档兼容有单测）。
3. 编辑弹窗**解密回显**：`open_edit_connection` 用 `open_password` 回填口令框（掩码显示）；「空 = 不修改」的旧口径废除 —— 空值就是「清除口令」。
4. 连接时 `ssh_target` 解密口令 → askpass 助手；解不开（换机、密钥被删）退回 ssh 终端交互。**全程零系统弹框**。
5. 删除连接时口令密文随档案行一起消失，无第二副本。

**不做钥匙串迁移**：读旧条目必弹框（正是要消灭的交互）。旧条目（服务名 `com.duck.toolbox`）留给用户在「钥匙串访问」里自行删除；首次重新保存后新链路即接管。

**与 `duck-platform::secret` 的分工**（该模块同日已被并发会话重写为 `credentials.json` 文件存储）：连接口令类走 `vault` 加密落库；其余凭据走 `credentials.json`。

---

## 16. 2026-10-12 追记（四）：终端选区 / 复制粘贴 · 连接后焦点 · 测试连接 · 拖标签并入的回归

用户一次提了 7 条，本节按「毛病 → 根因 → 做法」记已落地的几条。

### 16.1 终端里选不中文字、复制不了、也没有右键菜单

**根因**：选区此前**只做了一半** —— `duck-term` 内核里 `Term::selection` 那套 API 早就有了（`select_start` / `select_update` / `selection_text`），但画布上**没有任何鼠标入口**去调用它们，也没有任何东西把 `SelectionRange` 画出来。用户看到的就是「点了没反应」。这与 §13 里那条教训同源：**内核做完了不等于功能做完了**，界面那一跳没人接。

**做法**（三段，缺一段都不成立）：

1. **鼠标入口**：`on_mouse_down(Left)` → `begin_selection`、`on_mouse_move`（`pressed_button == Left` 才认）→ `drag_selection`、`on_mouse_up(Left)` **与** `on_mouse_up_out(Left)` → `end_selection`。⚠️ `on_mouse_up_out` 不能省：指针拖到画布外松手时只有 `*_out` 会到，少了它 `selecting` 永远为 `true`，下一次移动会把选区**一路拖下去**。
   - 用一个显式的 `selecting: bool`（而不是「左键此刻是否按下」）：把标签往下拖去分屏时左键也是按下的，用状态位判会把那次拖动解释成「选了一大片字」。
   - `begin_selection` 直接 `Selection::new` —— 它本身就是**覆写**语义，不需要先 `clear_selection`（早先草稿里的那一步是多余的）。
2. **高亮**：`selection_bands(range, display_offset, screen_lines, columns)` 把 `SelectionRange` 的**网格坐标**映射到视口坐标并逐行裁剪，产出 `(行, 首列, 末列)`，再画成一叠 `absolute` 的方块。⚠️ 必须过 `display_offset`：不换算的话，往上滚一屏高亮就**整体错位**（选区是对的、画出来的是别的行）。`is_block`（矩形选择）走同列区间。
3. **剪贴板与右键**：`⌘C` / `⌘V` / `⌘A` 三个 Action（见 §8.4 键位表）+ `.context_menu(...)` 的「复制 / 粘贴 / 全选」——「复制」在**没有选区时置灰**，不做「点了没反应」的假可用项。

**内核侧**：`duck-term::session` 补 `selection_range()` 供画布取范围；`Line` 的导入收进 `mod tests`（生产代码不自己构造行号，都问 `Term` / `Dimensions` 要）。

### 16.2 SSH 连着但键盘要先点一下才有反应

**根因**：标签建出来以后没人把焦点交给画布，焦点还停在左栏的连接树上。

**做法**：`ensure_sessions` 起会话的那一刻把焦点交给该窗格的画布。这样「连接成功 → 直接打字」成立，不需要用户先点一次。焦点归属本来就已有单一入口（§8.6 的 `focus_pane`），这次只是**在会话诞生时也调它一次**。

### 16.3 「测试连接」按钮（连接弹窗）

**做法**：`duck_components` 新增 `dialog_footer_with_action` —— 在「取消 / 确认」左侧**另起一个旁路按钮**（中间用 `flex_1` 撑开）。放在最左是刻意的：它是这一行里**唯一不关窗**的按钮，贴着确认键会让人以为「点它等于提交」。

按钮的落点是 `duck_ssh::test_connection`（`remote.rs`）：跑 `uname -srm`，**不开 PTY** —— 没有终端，`ssh` 就不会停下来等口令提示；失败时第一行 stderr（`Permission denied (publickey).` / `Connection refused` / `Host key verification failed.`）原样显示，比任何泛化文案都有用。参数全部取自**表单当前值**（含还没保存的口令），所以它测的就是「按创建之后会走的那条路」。

⚠️ **弹窗正文必须挂成独立视图**。`Dialog::content` 的闭包是 `Fn`，而且**子视图 `notify()` 不会把宿主重画**（`db.rs::DriverPickerView` 记过这条）。此前 `ConnectionForm` 只是一个数据实体、不在元素树里，它的 `notify()` 能否生效全看别的控件是否恰好也刷新了。这次把它变成 `Entity<ConnectionForm>` 并实现 `Render`、由 `.content(...)` 直接挂上去，同时让**它自己**订阅 6 个输入框的 `InputEvent::Change`（→ 把上一次的测试结果作废）与认证方式下拉。于是「测试结果后台回来立刻出现」「改了主机那条过期的『连接成功』自动消失」两件事都有了确定的落点。

顺带一处副作用（是修不是坏）：订阅与该视图的 `notify()` 落在同一个实体上之后，认证方式下拉、分组下拉、开关这些交互的重绘不再依赖「恰巧有人也刷新了」。

**口令**：只在这一跳的内存里过一手交给 `Target`（经环境变量给 askpass 助手），不入库、不进 argv、不落日志。`Target` 的 `Debug` 把口令一律打成 `***`。

**数据库 / 邮箱弹窗的同款按钮**：同一套 `dialog_footer_with_action` + 各自内核的探活函数即可，属**待办**（那两个页面当时正被并发会话编辑）。SSH 这一条已可用。

### 16.4 拖标签并入分屏「拖过去没反应」：先钉契约

**结论先给**：`with_split_drop_target` 的**结构是对的**（`on_drag_move::<TabDrag>` 在 Capture 相位按类型命中、越过容器中线点亮；`on_drop::<TabDrag>` 在 MouseUp 的 Bubble 相位且 `hitbox.is_hovered()` 成立时才真并），所以这条更可能是**运行期帧不对**而不是逻辑写错。为了把「怀疑」变成「可证伪」，新增无头回归 `crates/duck-app/tests/terminal_tab_drag_split.rs`（2 例：右半屏并入 / 左半屏取消），复刻「拖拽源标签 + 投放容器」这层骨架。

它顺带钉住了三条容易复发的 gpui 事实（都写在测试模块头里）：

- `on_drag` 的构造器要等指针位移**超过 2px 阈值**（`div.rs:52`）才会被叫起来；不到阈值就不是拖拽。
- `on_drag_move` 的回调**在 Capture 相位**、且只比 `active_drag` 的 `type_id`；`DragMoveEvent.bounds` 给的是**接收方自己**的 hitbox 边界。
- `on_drop` 在 **MouseUp 的 Bubble 相位**，前提是那个 hitbox 此刻真的被 hover —— 少了悬停，回调永远不来。

⚠️ 测试里**必须手动补两帧**：鼠标事件的监听器来自**上一帧**的 `rendered_frame.mouse_listeners`，而 `dispatch_mouse_event` 不像 `dispatch_key_event` 那样顺手补一次 `draw`。真实运行时由平台按帧驱动，测试里没有这个驱动。

### 16.5 快捷键注册（`"Terminal"` 上下文）

补了 `⌘C` / `⌘V` / `⌘A` / `⌘T` / `⌘W` 五个绑定，B 表见 §8.4。⚠️ `⌘C` / `⌘A` / `⌘W` 在 `gpui-base` 的 `"Root"` 上下文里**已有全局绑定**（`Copy` / `SelectAll` / `Close`），所以必须绑在**更深的 `"Terminal"`** 上才拿得回来 —— 与 `Tab`、非 macOS 的 `Ctrl-C` 同一类问题。

右侧栏的「快捷键」列表同日改成只列**真的绑了**的键，与 `app::install` 的绑定表逐条对应（此前那版列了三个一条都没绑的键）。

## 17. 2026-10-12 追记（五）：鼠标 / 焦点 / 超链接，以及 known_hosts 告警

### 17.1 鼠标上报（`duck-term/src/mouse.rs`，366 行 / 9 单测）

`MouseButton` / `MouseAction` / `MouseModifiers` / `reports_mouse` / `reports_motion` / `encode`。协议档：`MOUSE_MODE`（1000/1002/1003）、SGR 扩展（1006）、UTF-8（1005）、经典 `CSI M`。**Shift 作逃逸键**——按住 Shift 强制本地选择，不再上报给远端。

### 17.2 焦点上报（`duck-term/src/focus.rs`）

`reports_focus(mode)` / `encode(focused, mode) -> Option<Vec<u8>>`（`\x1b[I` / `\x1b[O`）。远端程序（vim / tmux）靠它切换「有焦点」的渲染变体。

### 17.3 超链接只在按 ⌘ 时才认

理由是**否则会毁掉拖选**：裸点击既可能是「点链接」也可能是「选文字」的开端，只有修饰键能区分。

### 17.4 known_hosts：只加告警，不写库、不改 `StrictHostKeyChecking`

**裁决**：交给系统 `~/.ssh/known_hosts` 与用户自己的 `StrictHostKeyChecking`，**不替用户决定**。所以本模块**不写** `~/.ssh/known_hosts`、**不改** `StrictHostKeyChecking`、**不新增依赖**（`duck-ssh` 仍零依赖）。

**取「事后识别」而不是「提前探测」**：边界要的是「**因主机密钥被拒**」这个事件，只有 OpenSSH 打在屏幕上的字能给出它；提前 `ssh-keygen -F` 探测只能得到「库里已有记录」，对每次连一台正常已知主机都会响 = 噪声，而且会引入**一段不受 known_hosts 保护的探测连接**。上一轮的 `duck_ssh::host_key_status` 因此降为**佐证**用（判断此前是否信任过这台主机）。

**判定表**：

| 屏幕信号 | known_hosts 记录 | 结果 |
| --- | --- | --- |
| 无任何告警 | 任意 | **不出声** |
| `REMOTE HOST IDENTIFICATION HAS CHANGED` 横幅 | 任意（含哈希化 = `Unknown`） | Changed 告警（横幅本身即证明此前有记录） |
| 吊销文案 或 记录 `@revoked` | — | Revoked 告警（**反对直接删记录**，先找服务提供方确认新密钥） |
| 仅 `Host key verification failed` | `Known` | Mismatch 告警（提示 `ssh-keygen -R <host>`） |
| 仅 `Host key verification failed` | `Unknown` | **不出声**（首次连接 TOFU，交 OpenSSH 自己决定） |

**落点**：
- `crates/duck-term/src/session.rs:614` `Session::buffer_contains(&str)`——整块网格（含回滚区）正则子串匹配的**阻塞加锁**版本。刻意**不复用** `search_matches`（它走 `try_with_term`）：退出判定是一次性的，**一次抢不到锁就把安全告警丢掉不可接受**。
- `crates/duck-app/src/pages/terminal.rs:867` `TerminalPage::ssh_endpoint(session_id)`（+ 私有 `endpoint_of(kind, owner)`）：会话键 → 所属连接 id 的归属口径**与 `ensure_sessions` 完全一致**（主窗格 = 标签 id；分屏看 `split_owner`），只读档案的 host/port，**不解密口令**。
- `crates/duck-app/src/pages/terminal_canvas.rs:2019-2131`：`HOST_KEY_CHANGED_MARKER` / `HOST_KEY_VERIFY_FAILED_MARKER` / `HOST_KEY_REVOKED_MARKER`、`HostKeySignals`、`HostKeyNotice{Changed,Revoked,Mismatch}`、`scan_host_key_signals()`、纯函数 `host_key_notice(signals, status)`、`ssh_keygen_target(host,port)`、`host_key_notice_text()`。
- `:399-466` `poll_sessions`：SSH 会话 `ChildExit` 时置标记，**在 `drain_events` 之外**扫描屏幕信号（避免与事件队列锁纠缠）。
- `:506-530`：解析主机 → `duck_ssh::host_key_status()` → 判定 → 命中推 `notify::sticky`，固定 key `terminal-host-key-<host>:<port>`（同主机反复触发只占一条）；Changed = `Error`，Revoked / Mismatch = `Warning`。投递走 `crate::update::on_notify_window`（poller 任务里没有 `window`）。

**未覆盖 / 存疑（如实记录，均为「表现为沉默、不误报」的保守退化）**：
1. **SFTP 路径未接**：`duck_ssh::{exec, list_dir, scp}` 的失败（`SshError::Failed` 带 stderr）同样可能出现 `Host key verification failed`，那是另一条集成线。
2. **GUI 端到端无实测**：需要「已在 known_hosts、且密钥已变」的真实服务器才能观察到告警，本环境无此条件。**判定 + 文案 + `ssh-keygen` 写法有单测**；屏幕扫描与通知投递两段无自动化覆盖。
3. `known_hosts::status` **只读 `$HOME/.ssh/known_hosts`**：`UserKnownHostsFile` / 系统级 / `known_hosts2` 都不覆盖 → 会判成 `Unknown` → 沉默（不影响「密钥已变」路径，那条靠横幅，与记录来源无关）。
4. **依赖 OpenSSH 英文原文**：OpenSSH 官方不做 gettext 本地化，一般认为稳定；被包装脚本替换时表现为沉默。
5. **极窄终端**（< 40 列）横幅可能折行导致扫不到。
6. `ControlMaster` 复用已握手成功的 master 时本次不再打印告警 → 沉默。
