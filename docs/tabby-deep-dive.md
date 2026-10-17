# 参考 Tabby 的深度对照：内核分层 / 前端架构 / 页面与组件

> 这是 SSH 评审的第四份。前三份的分工：
> - `docs/ssh-terminal-review.md` —— 架构与正确性（P0/P1 + M5/M6/M7 路线图）
> - `docs/ssh-terminal-ui-review.md` —— 布局分带账本与操作习惯
> - `docs/ssh-terminal-settings-render-review.md` —— 设置完备性与渲染像素缺陷
>
> 本文只做一件事：**把 Tabby（`temp/tabby`，Electron + Angular）拆到层，看它的层界怎么划、
> 组件怎么分、页面怎么组织**，然后回答两个问题：
> ① 哪些是「我们改十行就能享受」的；② 哪些是「看了也抄不动，但必须知道为什么」的。
>
> **终版收敛（裁决 + 唯一执行序列）见 `docs/ssh-terminal-final-plan.md`；
> 本文「工具条整条删除」在终版改为「hover + pin」。**
>
> 所有引用格式 `路径:行`，Tabby 侧路径相对 `temp/tabby/`。

---

## 0. 六条判断

1. **Tabby 的价值不在功能表，在三条缝**：连接层有「可共享、可引用计数」的 session；字节层有
   `SessionMiddlewareStack`；UI 层有十来个 `Provider` 抽象类。我们三条缝都没有 —— 所以我们
   每加一个功能都在改内核，它加一个功能是写插件。**缝的位置比功能清单重要。**
2. **我们选 `ssh(1)` 没错，但代价被低估了**，代价集中在五件事：认证提示跑到系统弹窗、口令只能在
   「建档时」填、转发只能起进程时加、主机密钥不能「接受并保存」、SFTP 只能靠 exec。Tabby 自研
   russh 换来的就是这五件事全在应用内。这五件里**有四件可以用 OpenSSH 自己的机制补齐**，不必换栈。
3. **Tabby 的页面观是「默认只有终端」**：工具条 hover 才出现且可 pin、查找条 fixed 悬浮不重排、
   SFTP 是覆盖在终端上的滑出面板。我们的每个附属 UI 都占固定分带。这比上一份「删工具条」更通用：
   **不是删，是改成「浮出 + 可 pin + 覆盖不挤压」**。
4. **组件观是「一行可承载多个动作」**：档案树行 hover 出 `play`、标签头 hover 出 `options`、
   SFTP 行右键出六项。我们多数行只有「点击 = 唯一动作」，第二个动作只能靠记快捷键或开弹窗。
5. **配置观是「改完立刻在活会话里生效」**：Tabby 每个设置控件都 `config.save()`，前端在
   `config.changed$` 上重配（字体/字号/配色/行高全部即时）。我们写 `kv` 但不回灌活会话 ——
   字号、字体、主题色全中（见 §3.4）。
6. **入口观是「`+` 打开选择器，不是打开表单」**：选择器里同时有最近、全部档案、分组、管理、以及
   一行「输入地址直接连」。**创建与连接是同一个动作**。我们把它拆成了三步：开表单 → 保存 → 再点列表。

---

## 1. 分层对照

### 1.1 Tabby 的五层

```
Profile(数据)  →  Session(传输)  →  MiddlewareStack(字节)  →  Frontend(渲染)  →  TabComponent(页面)
   ↑ 多态             ↑ 引用计数          ↑ 可增删双向              ↑ 可换实现          ↑ 可组合子组件
 每个终端类型            每个 SSH 连接        6 个消费者             xterm / webgl      split / toolbar / panel
 一个 Provider          可被多个标签共享      登录脚本/编码/…         (同一套接口)        都是它的子组件
```

| 层 | Tabby 的实现 | 我们的实现 | 差的到底是什么 |
| --- | --- | --- | --- |
| 档案 | `Profile` + `ProfileProvider` 多态；`ProfileGroup` 可嵌套、带 parentGroupId；档案带 `icon`/`color`/`weight` | `StoredProfile` + `Group`（**平铺，id = 名字**，`terminal_data.rs:397`） | 多态（不同协议同一个列表）与嵌套分组 |
| 连接 | `SSHSession`（russh），`ref()/unref()` 引用计数，`SSHMultiplexerService` 按身份做 key | `ssh(1)` 子进程 + ControlMaster（**隐式**共享，无计数、无显式关闭） | 谁拥有这条连接、什么时候能关 |
| 字节 | `SessionMiddlewareStack`：会话 ⇄ 终端之间的双向可插拔管道 | **不存在**（vte 解析在 alacritty `EventLoop` 线程内，见 `duck-term/src/session.rs:17,235`） | **没有位置**（不是没实现） |
| 渲染 | `Frontend` 抽象（xterm / webgl 可换） | `Term<EventProxy>` 直接用 | 不需要抽象（只有一种后端），不是问题 |
| 页面 | `BaseTerminalTabComponent` + 子组件组合（toolbar/search/sftp-panel 都是独立 component） | 一个 `TerminalPage` + 一堆 `render_*.rs` **自由函数** | 组件边界不存在，所以「某个区域单独重构/复用」不成立 |

### 1.2 三条缝，决定了两边的开发成本曲线

| 缝 | Tabby | 我们 | 后果 |
| --- | --- | --- | --- |
| 连接 | 一个 `SSHSession` 可被 N 个标签共享，`reuseSession` 是**每档案可关**的开关；复用失败自动回退到「新建一条」（`tabby-ssh/src/components/sshTab.component.ts:178-192`） | 靠 `control_path` 哈希复用，哈希只含 destination/port/identity（`duck-ssh/src/target.rs:523`） | 连接级参数（转发、ForwardAgent）改了不重连＝静默不生效（前一份 P0 已列） |
| 字节 | 中间件栈，见 §2.2 | 无 | 登录脚本 / 编码 / hex / ZMODEM / 换行翻译**全部无处可插** |
| UI | 10 个 Provider 抽象类，见 §3.1 | 插件是「JS 工具」，不能往 UI 塞元素 | 第三方能扩「能力」，不能扩「界面」 |

---

## 2. 后端内核：六个可迁移的设计

### 2.1 连接复用：显式引用计数（可抄，成本最低）

Tabby 的模型只有三行代码，但语义完整：

```
session = multiplexer.getSession(profile)        // 按「身份」查
if (!session || !profile.reuseSession) session = new SSHSession(...)
session.ref()                                     // 用一次 +1
session.willDestroy$.subscribe(() => session.unref())
unref(): if (--refCount === 0) destroy()          // 没人用了才真关
```

- 引用计数：`tabby-ssh/src/session/ssh.ts:1004-1012`
- 身份 key（**包含跳板机递归 hash、ProxyCommand、SOCKS/HTTP 代理**）：
  `tabby-ssh/src/services/sshMultiplexer.service.ts:36-45`
- 每档案开关 `reuseSession`（默认 `true`）：`tabby-ssh/src/profiles.ts:55`

**我们的等价物**：`ssh -M -S <path>` 的 control socket。差别在「谁负责关」——现在没有任何人关，
只能等 `ControlPersist` 超时。补齐的最小改动：

1. `ConnKey` 的 hash 输入补上**所有影响连接的参数**（`forwards`、`agent_forwarding`、`options`），
   这类「改了不重连就没生效」的 bug 会一次性消失；
2. 维护 `HashMap<ConnKey, (sock_path, refcount)>` 在页面状态里；最后一个标签关闭时
   `ssh -O exit -S <sock>`；
3. 档案级 `reuse` 开关（默认开），对应 Tabby 的 `reuseSession` —— 这条能直接消掉「改配置不生效」
   的那类投诉（用户自己关掉复用即可）。

> 注意：不要引入「连接池」概念。三条就够：key 全、有人关、可关。

### 2.2 字节缝：我们缺的不是功能，是位置

Tabby 在「会话」和「终端」之间插了一条**双向**管道，每个中间件只关心字节进出：

`tabby-terminal/src/api/middleware.ts:46-95` —— `SessionMiddlewareStack`，支持 `push` / `unshift` /
`replace` / `remove`，`relink()` 重新串联所有订阅。装上去的消费者有六个：

| 中间件 | 文件 | 干什么 | 我们 |
| --- | --- | --- | --- |
| 登录脚本 | `middleware/loginScriptProcessing.ts:5-10` | `expect`/`send` 对（sudo 口令、`tmux attach`、`cd /app`、跳板设备菜单） | 无 |
| 流处理 | `middleware/streamProcessing.ts` + `components/streamProcessingSettings.component.ts:10-45` | 本地回显 / 逐行发送 / 十六进制收发 / **换行翻译六档**（strip / CR / LF / CRLF / implicit） | 无 |
| OSC 处理 | `middleware/oscProcessing.ts`（在 `session.ts:35` 挂载） | 窗口标题、**上报 shell 当前目录** | 半个：标题有（`duck-term` `OSC 0/2`），cwd 无 |
| 输入处理 | `middleware/inputProcessing.ts` | 退格键编码（`backspace` / `^H` / `DEL` 三档，SSH 档案可单独配） | 有，但写死在 `duck-term/src/keymap.rs` |
| ZMODEM | `features/zmodem.ts:345` | `rz`/`sz` 走终端里传文件（串口/老设备唯一通路） | 无 |
| （编码转换） | **不存在** | GBK/Big5 解码 | 无；第三份文档已论证它必须插在 vte 之前 |

最后一行是关键：**Tabby 没有编码支持，但它有位置；我们连位置都没有。**

我们的数据通路是：`alacritty EventLoop` 自己 `read()` → 直接喂 vte → 只回调 `EventProxy::notify`
（那是终端**回话**用的，不是输出）。`SessionEvent` 里只有 `Wakeup`，**没有 `Output(bytes)`**
（`duck-term/src/session.rs:176-198`）。所以应用层拿不到一个字节的原始输出。

**落点（唯一正确的一处）**：包一层实现 `alacritty_terminal::event::EventedReadWrite` 的 PTY 适配器，
在 `read()` 返回后、交回事件循环前串联一条 `Vec<Box<dyn Middleware>>`。这一层同时解决：

- 编码解码（第三份文档的 S2）
- 换行翻译（老设备/串口/某些网络设备）
- 登录脚本 expect/send（需要同时看输出、写输入 → 管道本来就是双向的）
- ZMODEM（需要「暂停把字节送终端」）
- 十六进制视图（调试）
- 输出录制 / 脱敏（合规场景）

一个缝，六个能力。这是我在这份对照里认为**最值得先做**的结构性改动，比自研 SFTP 便宜得多。

### 2.3 认证：输入时机决定一切

| 维度 | Tabby | 我们 |
| --- | --- | --- |
| 尝试顺序 | 自己维护 `allAuthMethods` 列表 + `selectNextAuthMethod` 失败后降级（`session/authMethodSelection.ts`） | 交给 `ssh(1)`，它对 agent/密钥/KI 的处理比我们稳 |
| 交互输入 | **终端内联面板**：多步、可「上一步」、可勾「保存密码」、提示里的 URL 可点（`components/keyboardInteractiveAuthPanel.component.pug`） | askpass 助手 → 系统弹窗；且**只有「已经知道口令」时才挂助手**（`target.rs:410-417`） |
| 口令记忆 | 首次连接时问，问完可存；key 是 `{user, host, port}`（`services/passwordStorage.service.ts:97-103`）——**跟着身份走，不跟档案走** | 存在档案里（`remember` 字段），要建档时就填 |
| 跳过 banner | `skipBanner`（每档案） | 无 |
| 算法白名单 | 五类可配（kex/cipher/hmac/hostkey/compression） | 交给 `~/.ssh/config` |

**判定**：不抄认证计划（自研成本高、收益低），但抄「输入时机」。今天的路径是「先编辑档案、
把口令填进去、保存、再点连接」，主流客户端的路径是「点连接 → 问一次 → 勾保存」。后者需要一条
**交互式 askpass 回调**：现有的 askpass 只是「把已知口令交给 ssh」，没有「ssh 问 → 我们弹窗 →
把答案写回管道」这条回路。

这条回路的价值不止口令：`sudo` 提示、KI 多步挑战、私钥 passphrase 全都走它。**它是 2.2 那条缝的
一个特例**（在 `write()` 方向拦下提问），所以做缝的时候顺手就有了。

### 2.4 主机密钥：TOFU 闭环

Tabby 的弹窗有三种状态、两个按钮（`components/hostKeyPromptModal.component.ts:23-45`）：

- `isUnknown`（首次）→ 接受 / 接受并保存
- `isMismatched`（变了）→ 同样两键，但文案是警告

我们的实现（`duck-app/src/pages/terminal_canvas.rs:3047-3100`）是**读系统 `~/.ssh/known_hosts` 判状态，
然后显示一段文案教用户去终端里敲 `ssh-keygen -R host`**。这个「只读不写」的取舍是对的（不碰用户的
安全文件），但它把用户扔进了一个三步手工流程，而且**没有任何一键出口**。

Tabby 的取舍相反：自己存一份（`services/sshKnownHosts.service.ts:24`），于是「接受并保存」一秒钟完成，
代价是与系统 `ssh` 不互通 —— 你在终端里确认过的主机，Tabby 还要再问一次；反之亦然。**两套 TOFU 真相，
这是它的设计债**。

我们的折中路径（两边都占）：

1. 保持「读系统库」不变；
2. 弹窗里**显示指纹**（`ssh-keyscan -t ed25519 -p <port> <host> | ssh-keygen -lf -`），让 TOFU 不只是 FYI；
3. 「接受并保存」＝执行 `ssh-keygen -R <host>` 然后把新键写进系统 `known_hosts`（用一个明确的按钮，
   不静默）；
4. 「只此一次」＝`-o StrictHostKeyChecking=accept-new` 单独一条会话（不进配置、下次还问）。

代价：需要一次 `ssh-keyscan`（几百毫秒）+ 一次写文件（要备份原文件）。比 Tabby 的私有库更正确。

### 2.5 转发：三型 + **运行时增删**

Tabby 的转发是会话级的对象，可以随时加减（`session/ssh.ts:845-871`），所以它的弹窗能列出
「当前活着的隧道」（`components/sshPortForwardingModal.component.ts:35-37` 读 `session.forwardedPorts`）。

我们其实**有**完整的三型模型（`duck-ssh/src/target.rs:13-20` 的 `Local/Remote/Dynamic`）、也有编辑器
（`terminal_dialogs.rs:309` 的 `ForwardRow`），缺的只是「连上以后」。代码里自己写着：

> 改了转发不重连，旧会话上新增的隧道一条都不会生效。（`terminal/conn.rs:289-290`）

不必自研协议栈也能补：OpenSSH 的 control socket 支持**运行时改转发**。

```
ssh -O forward   -S <sock> -L 8080:127.0.0.1:80  <target>   # 加一条
ssh -O cancel    -S <sock> -L 8080:127.0.0.1:80  <target>   # 撤一条
```

前提：主连接必须是 `-M`（我们已经在用），且加转发时要带**完整的目标串**（`-O` 的语法要求重新说明
目标）。这条让「右栏一列隧道，带开关」从 M6 降到 M5 —— 纯 UI + 三次 fork。

### 2.6 恢复令牌：标签、cwd、甚至整个布局都能存成档案

Tabby 的恢复是「令牌」模式：每个标签能把「重建自己所需的输入」吐出来，重启后按令牌重建。

- SSH 标签的令牌额外带 **cwd**（`components/sshTab.component.ts:190-205`，配合 `rememberCwd`）
- 「保存布局」直接是一个档案类型：`SplitLayoutProfilesService`，把整棵分屏树存进
  `profile.options.recoveryToken`（`tabby-core/src/profiles.ts:14-60`）

我们**完全没有**（终端页不落 `kv`，重启即全丢；已核对 `terminal/mod.rs` 无任何持久化）。

这条对「每天开同样的五个标签」的用户是硬需求，而且成本不高：`TabItem` 已经是可序列化的结构，
存一张 `kv: terminal.tabs`（档案 id + split 关系 + 活动标签）就够，**不需要**恢复终端内容（那不是
我们的战场）。至于「保存布局」可以后置。

---

## 3. 前端架构

### 3.1 十个扩展点，对应我们的什么

| 扩展点 | Tabby | 我们的对应物 | 可抄度 |
| --- | --- | --- | --- |
| `ProfileProvider` | 一种协议＝一个 provider，自动进选择器/设置/命令面板 | 无（工具是另一套模型） | 借概念：**连接类型**将来要分 SSH/串口/Telnet 时必须有 |
| `QuickConnectProfileProvider` | 解析 `user@host:port` 并反序列化回字符串 | 无 | ★★★ 见 §4.1 |
| `TabContextMenuItemProvider` | 任意插件往标签右键菜单加项 | 无 | ★★ 我们有插件表，可加「菜单项」类型 |
| `SFTPContextMenuItemProvider` | 同上，作用于文件行 | 无 | ★★ |
| `HotkeyProvider` | 快捷键表由数据生成（含每档案一条） | `HotkeySlot::ALL: [Self; N]`（**编译期常量**） | ★ 见 §3.2 |
| `CommandProvider` | 命令面板条目（含最近档案） | `palette_pages()` 只出设置页（`settings/mod.rs:428`） | ★★★ |
| `SessionMiddleware` | 字节缝 | 无 | ★★ 见 §2.2 |
| `TabRecoveryProvider` | 标签恢复 | 无 | ★★★ 见 §2.6 |
| `ProfileImporter` | 导入 `~/.ssh/config` | 无 | ★★★ 成本极低（OpenSSH 自己有 `ssh -G`） |
| `AutoPrivateKeyLocator` | 自动找 `~/.ssh/*.pub` | 无（要手填路径） | ★★★ 扫目录即可 |

一句话：**Tabby 的扩展点是给第三方的，我们要的只是「把这十个点里的五个，做成我们自己的数据驱动」**，
不需要插件系统。

### 3.2 数据驱动：三个实例，一个模式

Tabby 有三处「列表不是写死的，是从数据长出来的」：

1. **每档案一条快捷键**：`core/hotkeys.ts:284-290` 遍历 `profiles` 生成 `profile.<name>`，
   于是设置页里自动出现「New tab: 生产库」并可以绑 `⌘⌥1`。
2. **每 provider 一条选择器快捷键**：`hotkeys.ts:291-294`。
3. **命令面板带最近档案**：`core/commands.ts:33-48`。

我们的障碍是硬的：`HotkeySlot::ALL: [Self; N]` 是常量数组，槽位数编译期固定。两条路：

| 方案 | 做法 | 代价 | 评价 |
| --- | --- | --- | --- |
| A. 槽位投影 | 固定 9 个槽位 `connection-1..9`，绑定目标是「收藏列表第 N 项」；收藏是档案表的一个布尔 | 一个字段 + 9 条静态绑定 | **推荐**。用户心智：「我把三台机器放收藏，⌘⌥1/2/3」 |
| B. 运行时注册表 | 把 `HotkeySlot` 改成可增长的 `Vec<Slot>` | 动 `hotkey.rs` 的 `ALL/name/keys/default_spec/index/label` 六处 + 设置页 + 冲突校验 | 不值得，且违反仓里「加槽位要同步五处」的既有约定 |

方案 A 还顺手解决另一个问题：`⌘1..8` 已给路由、`⌘9` 空着 —— 终端页内用 `⌘⌥数字` 不冲突。

### 3.3 三态继承：`true | false | null`

`SSHProfile.warnOnClose: boolean | null`，`null` 表示「跟全局」：

```
if (!(this.profile.options.warnOnClose ?? this.config.store.ssh.warnOnClose)) return true
```
（`components/sshTab.component.ts:206-222`）

这是**唯一的正确做法**：全局默认 + 每档覆盖 + 「没表态就跟随」。我们所有档案级开关都应该长这样
（现在多数是 `bool`，改全局对历史档案不生效）。落地成本极低：`Option<bool>` + `unwrap_or(global)`，
序列化天然兼容（缺字段 = `None`）。

### 3.4 广播：改完设置，活会话要跟着变

Tabby：每个控件 `(ngModelChange)='config.save()'`，前端订阅 `config.changed$` 重新 `configure()`
（`frontends/xtermFrontend.ts:744` 处一次性读 `copyOnSelect` 等全部外观项）。所以「调字号」是
**当场看到字变大**，不是「保存后重开」。

我们：设置写 `kv` + `flush` + `refresh_windows`（只刷窗口几何/主题），终端页的字号/字体/行高/配色
是**建页面时读一次**（`terminal/helpers.rs:9` 的注释自己写了这件事）。这就是第三份文档说的「三份真源」
的用户可见后果。

补法（不新建机制）：`TerminalSettings` 落盘后 `cx.emit` 一个 `PrefsChanged`，`TerminalPage` 订阅后
把 `terminal_font_size` / 字体 / 配色 重新灌进所有活会话（`Session::apply_palette` 已存在，
`duck-term/src/session.rs:904`；字号需要重算 `cell metrics` 并发 `Resize`）。

---

## 4. 页面设计

### 4.1 逐页对照

| 页面 | Tabby | 我们 | 判断 |
| --- | --- | --- | --- |
| 入口 | `+`/`⌘T` → **选择器**：最近 3 条 → 全部档案（按分组）→ 管理档案 → `Quick connect: 输入地址…`（`services/profiles.service.ts:262-357`） | `+` → 「新建连接」**表单弹窗**（14 字段） | ❌ 我们的入口是「管理」，他们的是「连接」 |
| 首屏 | 起始页 = 命令按钮列表（`startPage.component.pug`） | 左栏 = 三个全折叠组 + 搜索框 | ⚠️ 我们的首屏是空的 |
| 连接列表 | 树：递归分组、可折叠、每行 hover 出 `play`、**双击连、单击选**、右键菜单 | 平铺分组、单击即连、右键 6 项（`render_sidebar.rs:262`） | ✅ 右键菜单我们不差；差「最近/收藏」与「双击」语义 |
| 终端页 | 标签（7 槽位）→ 画布；工具条 **hover 浮出、可 pin**；查找条 **fixed 悬浮** | 标签（3 槽位）→ 工具条（常驻 24px）→ 画布；查找条占一行 | ❌ 两条固定分带可以消失 |
| SFTP | **覆盖在终端上的滑出面板**（单栏远端），打开时自动跳到 shell 当前目录 | 独立标签 + 本地/远端双栏 + 队列 + 状态栏 | 双栏是我们的优势；缺 cwd 跟随、路径编辑、过滤 |
| 转发 | 弹窗列出**当前活着的隧道**，可随时开关 | 只在档案编辑里，改了要重连 | ❌ 见 §2.5 |
| 设置 | 左侧竖排分类 + 即时生效 + 实时预览（`color-scheme-preview`） | 分类页 + 即时写盘 + **无预览** | ⚠️ 见 §4.4 |
| 快捷键设置 | 可搜索、每项可多绑定、录制控件 | 只读说明（右栏硬编码） | ❌ |

### 4.2 四个页面级原则

**原则一：终端页默认状态只有终端。** Tabby 的默认态是「标签 + 画布」两块，一切附属 UI 都是
「需要时出现」（工具条 hover、查找 fixed、SFTP 滑出）。我们默认态是四块（标签 + 工具条 + 画布 +
状态栏）。这条原则比「删掉工具条」更耐用：将来还要加的东西（会话录制、协作光标、AI 侧栏）都该走
「浮出」，而不是再叠一条分带。

**原则二：入口即选择器。** 一个输入框同时承担「搜已有」和「建新的」（`freeInputPattern`
`services/profiles.service.ts:337-345`）。Tabby 更进一步：列表里每条档案都带
`freeInputEquivalent`（`selectorOptionForProfile`），所以用户输入 `root@10.0.0.5` 时能**命中已有档案**
而不是盲目新建一条重复记录。我们补这个只需要两件事：档案能序列化成一个地址串 + 搜索框多一个
「直接连接」行。

**原则三：一行承载多个动作。** `profileTree.component.pug:37-49`：整行单击=选中、双击=启动、
hover 右侧出 `play`、右键出菜单。四个动作挤在一行里，谁也不用记。我们的行只有一个 `on_click`
（即连），第二个动作（分屏 / SFTP / 编辑）全藏进右键 —— 右键是**发现性最差**的入口。

**原则四：状态只出现一次。** Tabby **没有底部状态栏**：连接状态是标签头的一个圆点（`session.open`
→ 工具栏绿点，`sshTab.component.pug:2-3`），活动是标签头的 `activity-indicator`。我们把同一件事
说了三遍（工具条、状态栏、标签点）。状态栏要保留，但只放「标签里看不到的信息」：**尺寸、编码、
当前目录、延迟**。

### 4.3 SFTP：它是「终端的临时视图」，不是「第二个文件管理器」

这是两边最大的观念分歧，也是我上一份报告没说到位的：

```
Tabby:  sshTab.openSFTP() → this.sftpPath = await session.getWorkingDirectory() ?? this.sftpPath
        （components/sshTab.component.ts:229-234）
        面板 ngOnInit → 在当前 SSH 连接上 openSFTP() → navigate(path)
        （components/sftpPanel.component.ts:48-58）
        navigate 失败 → 通知 + 回退到 "/"（:71-80）
```

三个含义：
1. **不新增连接**（走同一条 SSH 的 sftp channel），所以打开是毫秒级；
2. **默认落在你正在看的那个目录**（shell 报了 cwd 的话），而不是 `/` 或上次的路径；
3. 关掉面板就回到终端，**不占第二个标签**。

我们的实现是「独立标签 + 双栏」，因此每次都要重新选路径、重新连（每次操作 fork 一个 `ssh`）。
双栏本身是优点（本地↔远端拖拽是 Tabby 没有的），不该丢。建议的形态是**混合**：

- 保留双栏（左本地 / 右远端）；
- 叠一条「跟随终端」开关：打开时，远端栏跟随活动终端的 cwd（需要 shell 集成上报，见第三份文档的
  `OSC 7` / `133` 方案）；
- 窗口宽度 < 1280 时，远端栏改为**覆盖式滑出**（宽度 60%，从右贴边滑入），把宽度还给终端。

### 4.4 设置页：三个可抄的细节

1. **实时预览**：`colorSchemePreview` 组件（`components/colorSchemePreview.component.ts:16-24`）——
   改字体/配色时旁边有一块活体样例。我们改字号要「弹窗 → 输数字 → 确定 → 看效果 → 不对再改」。
2. **字体是枚举出来的**：`this.fonts = await this.platform.listFonts()`（`appearanceSettingsTab.component.ts:23`）
   + `ngbTypeahead` 前缀匹配。我们是裸文本框 —— 打错一个字就静默回退到默认字体。
3. **行为设置项我们少了整整一组**（`terminalSettingsTab.component.pug`）：滚动行为（`scroll on input`）、
   右键行为（菜单/粘贴/选词）、中键粘贴、词分隔符、**「点击链接需要按键」**（防钓鱼）、选中即复制、
   带格式复制、括号粘贴、**多行粘贴警告**、**把换行替换成空格**、**裁剪首尾空白**、响铃三态、闪任务栏。

   其中「多行粘贴」的三件套（警告 + 替换换行 + 裁剪）是上游主流做法，比我们现在的「直写 PTY」
   安全得多，也比弹窗打断更省事。

---

## 5. 核心组件设计

### 5.1 组件粒度对照

我们的 `render_*.rs` 基本都是「一个函数返回一整块」。Tabby 对应位置是独立组件，可以被别处复用、
可以单独有输入输出。看差距最明显的是标签头：

| Tabby 标签头的 7 个槽位（`components/tabHeader.component.pug:1-30`） | 我们 |
| --- | --- |
| `.colorbar`（档案颜色条） | ✗ |
| `.progressbar`（标签级进度，为传输预留） | ✗ |
| `.activity-indicator`（未聚焦时有输出 → 亮点） | ◐（有状态点，不区分「活动」） |
| `.index`（`⌘1..9` 的可见提示，可关） | ✗ |
| `profile-icon`（图标 + 颜色） | ✗（只有文字标题） |
| `.pin-indicator` | ✗ |
| `.name`（drag handle，可改自定义标题） | ✗（不可拖、不可改名） |

七个槽位里，**索引号**和**活动点**是两个「零成本、高收益」的：索引号让 `⌘1..9` 从「隐藏功能」变成
「看得见的提示」；活动点解决了「后台标签里有输出但我不知道」（替代我们现在的「标题被 OSC 改掉」）。

### 5.2 我们缺的六个基础组件

| 组件 | Tabby 原型 | 我们的替代 | 说明 |
| --- | --- | --- | --- |
| 统一选择器 | `selectorModal.component` + `SelectorOption`（图标/分组/描述/权重/freeInput） | 无 | 一个 `searchable_menu` 变体：分组标题 + 右对齐快捷键提示 + 首行「输入即连接」 |
| 通用输入弹窗 | `promptModal.component`（标题/说明/多字段/校验/回填） | 每个弹窗手写 | 我们现在「打开目录 / 新建目录 / 重命名 / 改权限 / 关标签确认」各自一份 |
| 多绑定录制器 | `multiHotkeyInput.component` | 无 | 快捷键设置页的前置件 |
| 档案图标 | `profileIcon.component`（图标 + 底色 + 首字母回退） | 只有 `DuckIcon` | 档案的颜色/图标是「一眼认出生产库」的主要手段 |
| 分屏三件套 | `splitTabDropZone` / `splitTabSpanner` / `splitTabPaneLabel` | 一个 `divider` + 手写拖拽 | 我们分屏只有两格、只能竖分；拖拽是有的（`render_sftp.rs` 的 `PaneDrag`），但模型是 `Option<String>` |
| 配色预览 | `colorSchemePreview.component` | 无 | §4.4 |

### 5.3 三处行级设计差异

**SFTP 行**（Tabby `sftpPanel.component.pug:48-64` / 我们 `render_sftp.rs:455-490`）

| 列 | Tabby | 我们 | 判定 |
| --- | --- | --- | --- |
| 图标 | 按扩展名分 9 类 + 目录/软链 | 目录/文件两态 | ⚠️ 可后置（不是痛点） |
| 名称 | 弹性 | 弹性 + truncate | ✅ |
| 大小 | 右对齐 + `filesize` | 固定宽 + tabular | ✅ |
| 时间 | `tabbyDate` | 固定宽 + tabular | ✅ |
| **权限** | `getModeString()` 十位 `-rwxr-xr-x` | ✗ | ❌ **缺**（改权限的前提是先看见） |
| 右键 | 6 项（含「下载目录」「改权限」） | 4 项（仅远端行） | ⚠️ 本地面板无菜单（`render_sftp.rs:491-492` 注释自陈） |

**档案树行**：Tabby 有 hover `play`；我们没有（`render_sidebar.rs` 只有一个整行 `on_click`）。

**工具条**：Tabby 的每标签工具条是 hover 浮层 + `pin` 按钮（`terminalToolbar.component.pug:17-25`），
不占高度；我们是常驻 24px。**这是上一份「建议整条删除」的更优解**：不是删功能，是改成默认隐藏。

---

## 6. 落地清单

三档，按「改多少代码 / 拿多少收益」排。每条都给验收标准。

### T1　纯前端（不动内核、不动数据模型）

| # | 事 | 涉及文件 | 验收 |
| --- | --- | --- | --- |
| 1 | 工具条改 hover 浮出 + 可 pin，默认不占高度 | `terminal/render_*.rs`（工具条那一段） | 默认画布高度 +24px；鼠标移入出现，pin 后常驻 |
| 2 | 查找条改 `absolute()` 悬浮（右上，36px 级） | 查找渲染处 | 打开查找 **不触发** `Resize`（PTY 行列不变） |
| 3 | 标签头补「索引号」+「活动点」 | `terminal/render_tabs.rs` | 有 2 个以上标签时显示 `1..9`；后台标签有输出时点闪烁/常亮 |
| 4 | 左栏行：hover 出「连接」图标 + `⌘` 点击 = 新会话 | `terminal/render_sidebar.rs` | 已连接的档案，行尾图标变「再开一条」 |
| 5 | `⌘T` / `+` 改为「同主机新会话」（当前是本地 shell） | `terminal_canvas.rs:2279`、标签栏 `+` | 在 SSH 标签里 `⌘T` = 同一 `Target` 再开一条；本地 shell 移 `⌘⇧T` |
| 6 | 设置页补「实时预览」（一行样例文本 + 当前字体/字号/配色） | 模块设置页 | 改任一外观项，样例立刻变（不需要保存） |

### T2　内核小改（几十行，收益大）

| # | 事 | 涉及文件 | 验收 |
| --- | --- | --- | --- |
| 7 | `ConnKey` hash 纳入 `forwards` / `agent_forwarding` / `options` | `duck-ssh/src/target.rs:523` | 改转发后重连，`ssh -O check` 走新 socket（不再是旧 master） |
| 8 | control socket 引用计数 + 最后一个标签关闭时 `ssh -O exit` | 页面状态 + `duck-ssh` | 关掉所有该主机标签后，`ps` 里没有残留 `ssh -M` |
| 9 | 转发运行时增删：`ssh -O forward` / `-O cancel` | 新增 `duck-ssh` 命令 + 右栏一列 | 连上后加一条 `-L`，`nc -z 127.0.0.1 8080` 通；撤掉后不通 |
| 10 | 主机密钥：显示指纹 + 「接受并保存」（写系统 `known_hosts`，带备份） | `terminal_canvas.rs:3047`、`duck-ssh/src/known_hosts.rs` | 首次连接能看见 40 位指纹；保存后重连不再提示 |
| 11 | 标签/会话恢复：`kv: terminal.tabs`（档案 id + split + 活动） | `terminal/mod.rs` | 关掉应用再开，五个标签按原顺序回来（不恢复内容） |
| 12 | 粘贴三件套：多行警告 / 换行替换空格 / 裁剪首尾 | `terminal_canvas.rs:1561` | 粘贴 3 行文本弹一次确认；开「替换」后单行发送 |

### T3　结构性（需要新层，但一次解决多个问题）

| # | 事 | 前置 | 收益 |
| --- | --- | --- | --- |
| 13 | **字节缝**：包一层 `EventedReadWrite` + `Vec<Middleware>`（`read()` 后串联） | 无 | 编码、换行翻译、登录脚本、ZMODEM、hex、录制 —— 六件一起解锁（§2.2） |
| 14 | 交互式 askpass（ssh 问 → 弹窗 → 写回），并「记住口令」 | 13（或独立做） | 口令从「建档时填」变成「连接时问一次」；KI/`sudo`/passphrase 同路 |
| 15 | 统一选择器（最近 / 全部 / 分组 / 管理 / 输入即连）+ `ssh_config` 导入 | 无 | 消掉「临时连一台要填 14 字段」这个最贵的缺口 |
| 16 | SFTP：路径可编辑 + 名称过滤 + 权限列 + 「跟随终端 cwd」 | 13（cwd 需要 OSC 7 或 `133;P`） | 见 §4.3 |

> 顺序建议：**T1 全做（一天级）→ 7/8/10（半天级）→ 13 → 14 → 15**。
> 13 是分水岭：它之前每个能力都要单独设计落点，它之后都是「往栈里 push 一个」。

---

## 7. 明确不抄

| 项 | 为什么不抄 |
| --- | --- |
| 自研 SSH 协议栈（russh） | 我们已有 `ssh(1)` 的全部能力（agent/KI/算法/ProxyJump），换栈是半年工程，换不来对等收益。§2.3–2.5 的四件事都能用 OpenSSH 机制补齐 |
| 私有 `known_hosts` | 两套 TOFU 真相是债（Tabby 自己就有这个债）。我们读写系统库更正确 |
| 全量 JSON 一次写盘 | `config.save()` 是全文件覆盖；我们的 kv 400ms 去抖 + 单键更新更稳 |
| Angular/Electron 的组件生态 | 不是价值观问题，是技术栈不同。**借模式不借实现** |
| 单栏 SFTP + 系统对话框 | 我们的双栏 + 拖拽是优势，不倒退 |
| 每档案一条全局快捷键（照搬） | 我们的 `HotkeySlot` 是编译期常量数组，照搬要动六处；用「槽位投影」替代（§3.2） |
| 无虚拟滚动的文件列表 | Tabby 是 DOM 全量，我们是「每帧重建元素树 + `overflow_y_scroll`」，大目录更吃亏。该上 `uniform_list`（注意仓里「行必须 `.w_full()`」「行内不生效」两条约束） |

---

## 8. 与前三份文档的差异（避免重复）

| 主题 | 前三份已说 | 本文新增 |
| --- | --- | --- |
| 连接复用 | 「ControlMaster 吃参数」是 P0 bug | 为什么：**隐式复用没有所有权**；引用计数 + 显式关闭是解法（§2.1） |
| 编码 | 必须插在 vte 之前，落点是 Pty 适配器 | 这个「适配器」就是 Tabby 的 `SessionMiddlewareStack`；一个缝解锁六个能力（§2.2） |
| 工具条 | 「整条删除」 | 改成 hover + pin（Tabby 的做法），比删更好（§5.3） |
| 查找条 | 「改浮层避免 SIGWINCH」 | 给出参照实现：fixed、36px、右上、不重排（§4.2） |
| SFTP | 「面板 6 条分带太多」 | 它是「终端的临时视图」而非「第二个文件管理器」；cwd 跟随 + 窄屏覆盖式（§4.3） |
| 转发 | 「运行时不可见」 | `ssh -O forward/cancel` 是现成通路，不必等协议栈（§2.5） |
| 设置 | 「三份真源 / 改完要重开」 | 补 `PrefsChanged` 广播，回到活会话（§3.4） |
| 快捷键 | 分两层上下文 + 20 行目标表 | 每档案快捷键的**可实现版本**：槽位投影（§3.2） |

---

## 附录：证据索引

**Tabby（`temp/tabby/`）**

- 复用：`tabby-ssh/src/session/ssh.ts:1004-1012`、`tabby-ssh/src/services/sshMultiplexer.service.ts:36-45`、
  `tabby-ssh/src/profiles.ts:55`、`tabby-ssh/src/components/sshTab.component.ts:178-192`
- 跳板机：`sshTab.component.ts:81-125`（递归 `setupOneSession` + `openTCPForwardChannel`）
- 中间件：`tabby-terminal/src/api/middleware.ts:46-95`、`tabby-terminal/src/session.ts:17,35,90`、
  `middleware/loginScriptProcessing.ts:5-10`、`components/streamProcessingSettings.component.ts:10-45`、
  `features/zmodem.ts:345`
- SFTP：`session/sftp.ts:117-140`（临时文件 + rename 原子上传）、`components/sftpPanel.component.ts:48-90,118-160,355-370`
- 目录跟随：`components/sshTab.component.ts:229-234`
- 认证/凭据：`services/passwordStorage.service.ts:97-103`、`components/keyboardInteractiveAuthPanel.component.pug`
- 主机密钥：`components/hostKeyPromptModal.component.ts:23-45`、`services/sshKnownHosts.service.ts:24`
- 转发：`session/ssh.ts:845-871`、`components/sshPortForwardingModal.component.ts:35-37`
- 恢复：`recoveryProvider.ts`、`components/sshTab.component.ts:190-205`、`tabby-core/src/profiles.ts:14-60`
- 数据驱动：`tabby-core/src/hotkeys.ts:284-296`、`tabby-core/src/commands.ts:33-48`、
  `tabby-core/src/services/profiles.service.ts:262-357`
- 页面/组件：`tabby-core/src/components/tabHeader.component.pug:1-30`、`profileTree.component.pug:37-49`、
  `terminalToolbar.component.pug:17-25`、`tabby-terminal/src/components/searchPanel.component.scss:1-12`
- 设置：`components/appearanceSettingsTab.component.ts:23,26-45`、`components/terminalSettingsTab.component.pug`

**本仓**

- 无字节缝：`crates/duck-term/src/session.rs:17,176-198,235`
- 复用哈希：`crates/duck-ssh/src/target.rs:523`；转发模型 `:13-20`；askpass 单向 `:405-433`
- 转发不可运行时改：`crates/duck-app/src/pages/terminal/conn.rs:289-294`
- 主机密钥：`crates/duck-app/src/pages/terminal_canvas.rs:3047-3100`、`crates/duck-ssh/src/known_hosts.rs:43-56`
- 平铺分组：`crates/duck-app/src/pages/terminal_data.rs:397-405`
- 左栏单击即连 / 右键六项：`crates/duck-app/src/pages/terminal/render_sidebar.rs:218,262-330`
- SFTP 行 / 面包屑：`crates/duck-app/src/pages/terminal/render_sftp.rs:152-170,455-492`
- 命令面板只出设置页：`crates/duck-app/src/pages/settings/mod.rs:428-433`
- 字号初值只读一次：`crates/duck-app/src/pages/terminal/helpers.rs:9`
