# Duck Toolbox 架构缺口分析与补全方案（P0–P3）

> 范围：对当前仓库（Cargo workspace，`crates/*`，12 个 crate / 约 30,249 行 Rust）做架构级体检，
> 输出 **P0–P3 四档共 23 项** 缺口的完整改造方案。
> 方法：全部结论基于**当前源码事实**（引用 `文件:行`），不含推测性 API 调用；
> 凡涉及外部/上游能力，一律标注「需先核实」（依据 AGENTS.md 3.2「绝不发明 API」）。
> 状态：**分析结论 + 实施记录**。第 3–4 章中标注「（**已实施**）」的条目已落地，
> 单元 / 集成 / 跨进程契约测试全绿（本轮按用户要求不启动应用，实机观感由用户自行验收）。
> 已完成项：P0-1 / P0-2 / P0-3 / P0-4 / P1-1 / P1-2 / P1-3 / P1-4 / P1-5 /
> P2-1（复核：由 P1-4 交付）/ P2-2 / P2-3 / P2-4 / P2-5。
> 估工口径：人日（1 人日 = 8 小时），含实现 + 自测，不含跨版本兼容返工。

---

## 1. 现状基线

### 1.1 素材（已具备的底座）

| crate | 已实现能力 | 质量评价 |
| --- | --- | --- |
| `duck-theme` | 双层 token（基础 + 扩展）、明暗预设、强调色 / 缩放 / 字号 / 字体族、i18n（zh-Hans + En + 跟随系统） | 扎实，设计闭环完整 |
| `duck-components` | 按钮 / 导航 / 卡片 / 表单原语 / 侧栏面板 / 通知、`DuckIcon` 图标枚举、`LOGO_ASSET_PATH` | 扎实 |
| `duck-shell` | 自绘标题栏、活动栏、6 条路由与内容槽位、右侧栏 shell 级统一区域 | 扎实 |
| `duck-command` | 动作注册表派生**四输出面**（快捷键 / 菜单 / 命令面板 / 活动栏）、两档 handler（窗口级 + App 级） | 设计先进，覆盖面偏窄 |
| `duck-store` | SQLite + `user_version` 只追加迁移 + 单写线程 400ms 去抖；表 kv / settings / windows / recent / tool_state | 底座好，**有死表** |
| `duck-platform` | 路径、`RollingWriter` 日志（按天 + `prune_logs`）、rfd 模态框、剪贴板、flock 单实例锁、开机自启、crash 捕获、**加密凭据**（`vault`：AES-256-GCM；`secret`：`credentials.json`）、数据归档 | 已解决：外部唤起请求归一 + 单实例转发通道 + Windows 注册表注册；凭据已与系统钥匙串解绑（2026-10-12） |
| `duck-tools` | 工具注册表、`invoke`（记使用 + 调 run）、`recent()`、dev 工具 `debug.registry` | 内置工具已下线，仅留骨架 |
| `duck-plugin` | `plugin.toml` 契约、CapabilityGranter（默认拒绝 + glob）、PluginIndex 多目录合并 | **安装时子集授权未实现** |
| `duck-plugin-js` | rquickjs 沙箱、每调用独立 Runtime、三重限制（内存 32 MiB / 栈 256 KiB / 墙钟 10 s） | **无网络 host 函数** |
| `duck-plugin-registry` | 市场读端、原子解包、能力上限校验、可复现打包、多源回退 | 质量高 |
| `duck-update` | sha256 校验、多源回退、三平台换位安装、`DEFAULT_SOURCE` 版本无关别名 | 已解决：minisign 验签 + 上一版留存 + 一键回退 + `min_app_version` 门槛 |
| `duck-app` | 装配、8 个设置页、工具箱 / 插件中心 / 概览页、托盘、顶栏搜索 | **有假数据与占位页** |

### 1.2 结论一句话

**底座质量很高、产品化纵深不足。**
"现代化桌面外壳的骨架"已经做扎实（自绘 chrome、双层主题 token、四输出面命令层、插件能力网关、可复现市场发布链），
但**面向终端用户的"最后一公里"能力成片缺失**：安全凭据、可观测性、数据生命周期三块**完全空白**；
窗口会话、搜索、更新回滚这些"用起来该理所当然存在"的东西**只做了一半**。

---

## 2. 缺口总览

| ID | 缺口项 | 档位 | 估工 | 依赖 |
| --- | --- | --- | --- | --- |
| P0-1 | 崩溃捕获与诊断信息 | P0 | 1.5 | 无 |
| P0-2 | 安全凭据存储（**已与系统钥匙串解绑**） | P0 | 2.0 | 无 |
| P0-3 | 数据导出 / 导入 / 重置 | P0 | 3.0 | P0-1（诊断归档） |
| P0-4 | 首启引导 + 开源许可合规 | P0 | 2.5 | 无 |
| P1-1 | ✅ 全局唤起快捷键（唤起时弹**独立命令面板浮窗**，可切回主窗） | P1 | 3.0 + 0.5 | 上游能力核实 + 引依赖 |
| P1-2 | ✅ 窗口会话恢复 | P1 | 2.5 | 无 |
| P1-3 | ✅ 更新链路安全（签名 + 保留 + 回退） | P1 | 5.0 | `manifest.py` / `release.yml` |
| P1-4 | ✅ 命令面板升级（工具 / 最近 / 模糊） | P1 | 3.0 | P2-1 共用索引 |
| P1-5 | ✅ 拖拽 + 深链接 + 文件关联 | P1 | 5.0 | 上游能力核实 + IPC |
| P2-1 | ✅ 顶栏全局搜索（复核：由 P1-4 交付） | P2 | 2.0 | P1-4 |
| P2-2 | ✅ 日志增强（大小上限 / 过滤 / 导出） | P2 | 2.0 | 无 |
| P2-3 | ✅ 死代码 / 死状态清理 | P2 | 1.5 | 无 |
| P2-4 | ✅ 占位路由处置（**已回退**：入口按用户要求恢复，见 P2-4 章节） | P2 | 0.5+0.5 | 无 |
| P2-5 | ✅ 概览页真实化 | P2 | 1.5 | 自建耗时埋点（原以为依赖 P0-1） |
| P2-6 | ✅ 通知体系完善（两条通道 + 三档 + 历史概览） | P2 | 2.0 | 无 |
| P2-7 | ✅ 可访问性（role / 名字 / 选中态 + Tab 可达 + 对比度守卫） | P2 | 3.0 | 无 |
| P2-8 | ✅ 插件能力授权细化（子集授权） | P2 | 2.5 | 无 |
| P2-9 | ✅ 插件 JS 网络能力落地 | P2 | 4.0 | P2-8（授权前提） |
| P3-1 | 插件签名与市场治理 | P3 | 6.0 | P1-3（签名基建复用） |
| P3-2 | i18n 深化（复数 / 日期 / 数字） | P3 | 3.0 | 无 |
| P3-3 | 大文件拆分与架构治理 | P3 | 4.0 | 无 |
| P3-4 | 多平台补齐（Linux / Windows 对齐） | P3 | 3.0 | 无 |
| P3-5 | 主题深化 + 可选遥测 | P3 | 3.0 | 无 |

**小计**：P0 = 9.0 · P1 = 18.5 · P2 = 17.0 · P3 = 19.0 · **合计 ≈ 63.5 人日**。

**已完成**（截至 2026-10-11，估工口径 44.5 人日）：P0-1 · P0-2 · P0-3 · P0-4 · P1-1 · P1-2 ·
P1-3 · P1-4 · P1-5 · P2-1 · P2-2 · P2-3 · P2-4 · P2-5 · P2-6 · **P2-7** · P2-8 · P2-9。
即**第一步全部收口**（9.0 / 9.0）、**第二步全部收口**（18.5 / 18.5）、
**第三步全部收口（17.0 / 17.0）**（先止损三件 + P2-2 + P2-6 + P2-7 + P2-8 + P2-9；
P2-1 的 2.0 是复核出来的虚账，实际由 P1-4 交付）。
> P2-9 的第 3 条（凭据注入）**未随本项交付**，它要求先把 `secret.rs` 接上界面，
> 单列为后续项，不计入这里的 44.5。
> P2-7 的焦点环对比度守卫实测不成立（80/420 组合 < 3:1），已 `#[ignore]` 存档；
> 修 `ring` 的派生方式属产品取舍，单列为后续项，同样不计入 44.5。
> P1-1 交付后按实测反馈补的「唤起语义」（独立命令面板浮窗，0.5 人日）也**不计入 44.5**：
> 它是对已完成项的形态调整，不是路线图里的独立需求。

---

## 3. P0 — 缺失即硬伤

> 判据：一旦有真实用户，缺这三项会让"出了问题无法定位、凭据无法安全存放、数据无法找回"变成硬伤。

### P0-1 崩溃捕获与诊断信息

**现状（证据）**
- 全仓 grep `set_panic_hook|panic::set_hook|catch_unwind` → **无任何匹配**。
- `duck-platform` 只有 `RollingWriter` 日志（按天滚动），无崩溃专用通道。
- release `.app` 双击启动无终端附着 → panic 只写 stderr，**崩溃零痕迹**。

**缺口影响**
用户报"闪退"时，开发侧拿不到任何现场（栈、线程、触发点、版本、插件清单）。

**改造方案（已实施，含实施中的设计修正）**
1. `duck-platform/src/crash.rs`（新增，**纯 std**）：
   - `install_panic_hook(log_dir)`：`OnceLock` 幂等 + `std::panic::set_hook`，
     把「时间 / 线程 / 位置 / payload / `Backtrace::force_capture()`」追加到
     `crash-YYYY-MM-DD.log`；**先转交原先的钩子**再落盘（否则 release 下连 stderr 都看不到）。
   - `crash_count(log_dir)` / `crash_log_path(log_dir)`。
   - 累计计数落 `crash-count`：**刻意不用 `.log` 后缀** —— 日志保留期清理只认 `.log`，
     而计数必须跨清理长期保留。
   - 与运行日志同目录：日志查看器认得目录里任何 `.log`，崩溃现场天然可见。
2. `duck-app/src/diagnostics.rs`（新增，应用层，负责**解读**平台层的记录）：
   `install_hook` / `crash_total` / `crash_unseen` / `take_unseen_crash` / `platform` /
   `plugin_count` / `report` / `copy_report` / `notify_unseen_crash`。
3. **设计修正（实施中发现，原方案在此处不成立）**：原计划用「`kv: session_alive` 标记 +
   正常退出时清除」判断上次是否异常退出，但那需要一个可靠的退出钩子，而标记只能放在
   app 层（平台层不依赖持久层，层边界不允许它写 `kv`）。改为
   **「累计崩溃计数 + `kv: crash_seen` 水位线」**：不依赖任何退出钩子，且语义更有用 ——
   回答的是「有没有还没看过的崩溃」，而不只是「上次是不是崩的」。
4. `duck-app/src/main.rs`：`init_logging()` 之后立刻装钩子；`Store::install` 之后读水位线，
   主窗就绪**之后**才推提示（否则会投给一个还不存在的窗口）。
5. 「关于」页新增**诊断信息**区块：运行平台 / 插件数量 / 累计崩溃 / 数据文件 / 日志目录，
   外加「复制诊断信息」「打开日志目录」两个按钮。
6. 未查看崩溃 > 0 时首屏推一条 `notify::sticky`（同键替换）。

**涉及文件（实际）**
- 新增：`crates/duck-platform/src/crash.rs`、`crates/duck-app/src/diagnostics.rs`
- 修改：`crates/duck-platform/src/{lib,logging}.rs`（模块登记 + 日期/时刻戳提为 `pub(crate)` 复用）、
  `crates/duck-app/src/{main,update}.rs`（接线 + `toast` 提升为 `pub(crate)`）、
  `crates/duck-app/src/pages/settings.rs`（关于页区块）、
  `crates/duck-theme/src/i18n.rs`（`EN` 表 +12 条，417 → 429）

**验收标准（已验证）**
- `duck-platform` 单测 +6：计数从零 / 追加与计数 / 计数文件不落 `.log` / 按当天命名 /
  装钩子幂等 / 坏计数文件读作 0。
- `cargo test -p duck-theme`：`EN` 表有序、源码中文文案全覆盖两条守卫通过。
- 手工：触发一次 `panic!()` → 日志目录出现 `crash-YYYY-MM-DD.log`；
  重启后「关于」页计数 +1，且首屏出现诊断提示。

**估工**：1.5 人日（**已完成**）

---

### P0-2 安全凭据存储（keyring → 应用自有加密存储）

> **2026-10-12 迁移（本节结论已被取代，勿按 keyring 口径实施）**
> 用户实测「AI API key / SSH 口令写进 macOS 钥匙串」后定调：**凭据不许与系统绑定**，
> 必须落在**程序自己的数据库或文件**里。当前口径：
> - `duck-platform::vault`（新）—— AES-256-GCM（`ring`），密钥落 `app_data_dir()/vault.key`（0600），
>   `seal` / `open` 两入口，密文格式 `hex(nonce(12B) ‖ ct+tag)`；
> - `duck-platform::secret` —— 后端由「系统钥匙串」换成**应用自有文件** `app_data_dir()/credentials.json`
>   （值 = vault 密文，键名仍明文），API 形态（`set` / `get` / `delete` / `exists` / `available` / `probe`）
>   **刻意保持不变**，故 SSH / 数据库 / Redis 的既有调用点零改动即换落点；
> - AI API key 不再走 `secret`，改为**密文进厂商档案**（`ProviderProfile::api_key`，随 `kv: ai_providers` 落 SQLite）；
> - `keyring` 依赖已从根 `Cargo.toml` 与 `duck-platform/Cargo.toml` 删除，`Cargo.lock` 计数 **0**；
>   下面第 2~6 条（依赖评审 / 后端特性 / 平台回落 / Linux 不编译后端）**全部随之失效**，仅作历史留档。
>
> 为什么当初就走错过：钥匙串条目 ACL 绑定**签名身份**，本仓 dev 是 ad-hoc 签名、每次重编译都变，
> 读一次弹一次授权框；且「编辑回显」要求应用能无提示地读，与 ACL 模型根本相抵。详见 **P3-9**。

**现状（证据，keyring 阶段）**
- `crates/duck-platform/src/lib.rs` 当时只有一行注释 `//! - 凭据（keyring → 系统钥匙串）`，**无实现**
  （该行「尚未纳入」注释已在实施中删除）。
- 全仓 grep `keyring|SecretService|CredentialStore` → 仅 `Cargo.lock` 中传递依赖 `security-framework`（非本仓直接使用）。
- 现有持久层只有 SQLite（`kv` 表），**明文**。

**缺口影响**
代理认证、云盘账号、第三方 API key 这类数据**没有安全落点**。
一旦 P2-9（插件网络能力）开放，插件将被迫把 token 明文塞进 `kv` 或 `tool_state` —— 这是不可逆的安全债。

**改造方案（已实施，含实施中的设计修正）**
1. `duck-platform/src/secret.rs`（新增，**模块公开** `pub mod secret`）。API：
   `set` / `get` / `delete` / `exists` / `available` / `probe`，
   `SecretStore` trait + `MemoryStore` 内存实现，`SecretError` 三档错误。
   ~~service 名 `com.duck.toolbox`~~（2026-10-12 随 keyring 一并删除；文件后端不需要服务名）。
   - 为什么 `pub mod` 而不是像 `crash` / `autostart` 那样扁平 re-export：那些模块的
     函数名（`read_text` / `set_launch_at_login`）本身表意完整，而 `set` / `get` / `delete`
     一旦提到 crate 根就必须加别名（`set_secret` / `get_secret`），反而把语义拆散了。
     先例是同样是多入口的 `archive`，它也是 `pub mod`。
2. ~~**依赖评审结论（AGENTS 3.5，2026-10-11）**：钉 `keyring = "3.6.3"`——~~
   **【已作废，2026-10-12】** `keyring` 已整块移除（根 `Cargo.toml` + `duck-platform/Cargo.toml`），
   `Cargo.lock` 里 `keyring` 计数为 0，当初净增的 2 个包（`keyring 3.6.3` /
   `security-framework 2.11.1`）一并退回。以下为历史记录：
   MIT OR Apache-2.0、MSRV 1.75、edition 2021。**4.x 全系 MSRV 1.88 不可用**（顶破本仓 1.85）。
   实测依赖代价：`Cargo.lock` **净增 2 个包**——`keyring 3.6.3` 与
   `security-framework 2.11.1`。后者是 `keyring` 的 **iOS target** 声明，Cargo.lock 与 target
   无关所以被收进来，但 `cargo tree --target aarch64-apple-darwin -i security-framework@2.11.1`
   **反查为空**——它永不参与本仓的 mac/win 构建。macOS 侧命中的是**已在依赖图里**的
   `security-framework 3.7.0`（`keyring` 在 macOS 要 `"3"`、在 iOS 要 `"2"`，各取所需）。
   因此原方案预期的「与 `security-framework 2.x` 并行存在 3.x」在**构建图内不成立**、
   只出现在 lock 里。
3. ~~**设计修正（实施中发现，原方案在此处会静默丢数据）**：`keyring` **默认无特性**~~
   **【已作废，2026-10-12】** 换文件后端后不存在「后端特性漏开静默回落内存 mock」这个问题
   （文件后端只有一种实现，漏配即报错）。以下是 keyring 阶段踩过的坑，留作教训：
   （`default-features = false`），而不开后端特性时它**不报错**——`src/lib.rs` 里有
   `pub use mock as default;`，等于把默认后端静默换成**进程内的内存 mock**，
   表现是「写入成功、重启全丢」，且没有任何日志。所以：
   - 后端特性**只能**在 `duck-platform/Cargo.toml` 的 target 依赖表里开
     （macOS `apple-native` / Windows `windows-native`）；
   - `secret.rs` 末尾放**两条编译期断言**，引用被同一特性门控的
     `keyring::macos` / `keyring::windows` 模块：漏开特性时**编译失败**
     （错误信息直接指出 "gated behind the `apple-native` feature"），
     而不是跑起来悄悄丢数据。已实测：去掉 `apple-native` → 编译报 `E0433`。
   - 「编译不过比悄悄丢数据好」是这条的取舍原则（这条原则本身仍然成立，只是现在
     由「文件只有一种后端」天然满足）。
4. **错误归一**：`Error::NoEntry` 在 `get` 里归成 `Ok(None)`（读一个还没设过的键是
   正常路径）、在 `delete` 里归成 `Ok(())`（让 delete 幂等）；而
   `NoStorageAccess` / `PlatformFailure` **绝不**折叠成「当作没有值」——
   那会把「这台机器读不出来」伪装成「用户还没配」，是这类代码最难查的 bug。
   （`Error::NoEntry` 已随 keyring 消失，`secret.rs` 现在同义处理「键不存在 → `Ok(None)`」
   与「密文解不开 → `Err`」，后者**绝不**折叠成空值。）
5. **平台回落策略**：~~`Backend::{Keychain, CredentialManager, None}`~~。
   **【已作废，2026-10-12】** `Backend` 枚举与 `backend()` 已删除。文件后端没有
   「本平台没后端」这个状态，只有「目录建不出来 / 文件读不出来」这类**环境故障**，
   报 `SecretError::Failed` 由调用方禁用相关功能。
   ~~拿不到后端时**不降级为明文或内存**，`available()` 返回 `false`、操作返回
   `SecretError::UnsupportedPlatform`~~ ——「绝不降级为明文」这条口径**保持**。
6. **Linux 不编译后端**（~~实施决策~~ **【已作废，2026-10-12】**）：
   文件后端与操作系统无关，三平台同一套路径，本条不再适用。以下是当初否掉
   Linux Secret Service 的理由，留档：
   两条路都不成立——`linux-native` 用内核钥匙串，**会话级、重启即丢**（换个方式静默丢）；
   `sync-secret-service` 实测拖进 **44 个包**，其中 `libdbus-sys` 要求系统装
   `libdbus-1-dev`（pkg-config 探测），与本仓「终端用户零系统依赖」的取向
   （rusqlite `bundled`、rustls 替 OpenSSL）直接冲突；而本仓**没有 Linux 发布产物与 CI**
   （README / AGENTS 均写「macOS 桌面工具箱外壳」，release 矩阵只到 macos-arm64 /
   macos-x64 / windows-x64）。将来若支持，开 `sync-secret-service` + `crypto-rust`
   并给 `Backend` 加分支。
7. **本模块不写任何日志**：运行日志会落盘、且会被 P0-3 的数据导出整包打走（`logs/` 可选），
   所以这里经手的值一行都不能进日志。失败原因只经 `SecretError` 交给调用方——
   调用方可以记录「存储不可用」这个**事实**，不能记录值本身。（**仍然有效**）
8. **键名校验收在写之前**：非空、≤ 128 字节、只含 `[A-Za-z0-9._:-]`。
   ~~收得紧的理由是钥匙串条目用户删不干净~~（2026-10-12：理由改为「键名写歪就是
   `credentials.json` 里的永久垃圾，且顺手挡掉『拼错键名读不到自己刚写的值』这个最常见的自伤」）。

**涉及文件（实际）**
- 新增：`crates/duck-platform/src/secret.rs`（**2026-10-12 重写为文件后端后为 25 条单测**；
  keyring 阶段是 17 条单测 + 1 条 `#[ignore]` 实机往返，后者已随 keyring 删除）
- 修改：`crates/duck-platform/src/lib.rs`（`pub mod secret`、模块文档去掉「尚未纳入」）、
  ~~`crates/duck-platform/Cargo.toml`（target 依赖表开后端特性）~~、
  ~~根 `Cargo.toml`（`[workspace.dependencies]` 新增 `keyring`）~~
  —— **两条已于 2026-10-12 反向回退**，根 `Cargo.toml` 与 `duck-platform/Cargo.toml` 中
  `keyring` 相关行整块删除，改为记录新落点（`credentials.json` + `vault.key`）。
- 新增（2026-10-12）：`crates/duck-platform/src/vault.rs`（AES-256-GCM 加解密原语）
- 连带：`Cargo.lock`（+2 包 → **已回退为 0**）、`assets/licenses/THIRD_PARTY_LICENSES.md` 重生成
  （keyring 阶段 849→851 组件、409→411 全文；2026-10-12 移除 keyring 后需再重生成一次）

**验收标准（实测结果）**
- 单测覆盖 set / get / delete / NotFound 四态 → 见 `secret.rs::tests`：
  缺失读为 `None`、写入往返、覆盖写、删除后消失、**删不存在的键仍成功**、
  双键不串门、空串 / 非 ASCII 值往返、键名五类非法输入被拒、长度按字节压线、
  **被拒的键名不落存储**、store 存在性一致。
  **2026-10-12 追加**（文件后端 + 密文）：`the_file_does_not_contain_the_plaintext`、
  `a_broken_ciphertext_is_reported_rather_than_read_as_missing`、
  `a_real_round_trip_lands_in_the_credential_file`、`writing_one_key_keeps_the_others`、
  `the_file_is_stable_sorted_json`、`writing_the_same_value_does_not_touch_the_file`（mtime）、
  `deleting_a_missing_key_does_not_create_the_file`、
  `a_corrupt_file_is_an_error_rather_than_an_empty_store`、`an_empty_file_reads_as_no_credentials`、
  `a_file_where_the_directory_belongs_is_reported`、`the_file_is_owner_only`（0600/0700）、
  `the_default_file_lives_in_the_app_data_directory`、`a_rejected_key_never_touches_the_store`。
- ~~macOS 实机：跑 `cargo test -p duck-platform -- --ignored` 真实往返通过；互操作取证……
  `security add-generic-password` / `security delete-generic-password`~~
  **【已作废，2026-10-12】** 不再与系统钥匙串互操作，该取证路径连同 `#[ignore]` 测试一并删除。
- 依赖评审记录：见上文第 2 条与根 `Cargo.toml` 的注释（**结论已改为：不使用 `keyring`**）。

**尚未接线（有意）**：~~本层目前**没有调用方**——`duck-app` 侧一行未改。~~
**2026-10-12 更新**：`secret.rs` 已有三个真实调用方（`duck-db` / `duck-redis` 的
口令包装、`db` 页的 `ai:api_key`），终端 SSH 与 AI 已改走「密文进档案」不经本层。
给用户看的「凭据存储可用 / 不可用」展示位由 `probe()` 支撑（措辞只剩
「凭据存储不可用」一档——文件后端没有「本平台没后端」这个永久事实）。

**估工**：2.0 人日（**已完成**）

---

### P0-3 数据导出 / 导入 / 重置

**现状（证据）**
- 无任何导出入口；`pages/overview.rs:153-159` 的「导入配置」是假按钮，点击仅弹 toast「导入配置尚未接入业务逻辑」。
- 数据分散：SQLite（`kv`/`recent`/`tool_state`/`windows`）、`<config>/keybindings.toml`、插件目录、日志目录。

**缺口影响**
用户无法备份 / 迁移 / 求援；开发无法复现用户环境；卸载不彻底。

**改造方案（已实施，含实施中的设计修正）**
1. `duck-platform/src/archive.rs`（新增，**纯字节搬运**）：`create` / `list` / `read_member` / `extract`。
   两条对称的硬规则：**打包侧跳过链接**、**解包侧拒一切链接 + 拒绝逸 + 512 MiB 上限**。
   打包侧为什么也要跳过链接——解包侧既然一律拒链接，若打包侧把链接原样存进去，
   就会出现「自己导出的包自己导不回来」。
2. `duck-app/src/backup.rs`（新增，**编排层**）：`collect` / `write_archive` / `load_archive` /
   `export` / `import` / `reset`。目录集合做成显式 `DataDirs` 注入，测试能直接跑在临时目录上。
3. `duck-app/src/pages/data.rs`（新增，UI）：三段「备份 / 恢复 / 重置」，纯自由函数，
   不占 `SettingsWindowView` 的状态位（两个开关落 kv）。
4. `duck-store` 新增：`kv_all()`（全表快照）/ `kv_replace_all()`（覆盖式导入）/
   `clear(&[Table])`，对应的 `Op::ReplaceKv` / `Op::ClearTables` 在**同一事务**里
   「先清空、后写入」，否则同批的写会被自己这批的清空抹掉。
5. **设计修正（实施中发现，原方案在此处不可靠）**：原计划是「导入失败 → 自动回滚」。
   改成 **「先验证 → 再备份 → 才动手」**：归档先完整解包到 `.staging-*`（逃逸 / 链接 /
   上限全在这一步被拒），确认无误再自动整包导出到 `backups/pre-import-*.tar.gz`，
   最后才改现状。比自动回滚更可靠——回滚本身失败时，用户手上至少还有一个完整的包。
   重置同流程，落 `pre-reset-*.tar.gz`。
6. **归档布局**：`manifest.json`（`format_version` / `app_version` / `created_at` /
   `includes` / `excludes`）+ `settings/kv.json` + `settings/keybindings.toml`（有才写）+
   `plugins/` + 可选 `logs/`。`excludes` 明确写 `credentials`（2026-10-12 由
   `keyring-credentials` 改名，同时 `credentials.json` 与 `vault.key` 一并排除）——
   让用户看到「包里明确没有什么」。导入侧拒绝 `format_version` 比本程序新的包。
7. 顺手修掉假入口：`pages/overview.rs` 的「导入配置」不再弹「尚未接入业务逻辑」，
   改为跳转「设置 → 数据」。
8. 快照前一律 `duck_store::flush`：`kv` 写是 400ms 去抖的，不先落盘就快照会导出旧值
   （「刚改完设置立刻导出」是最容易踩到的场景）。
9. 删除前的守卫 `assert_safe_to_remove`：路径必须绝对、位于数据目录之下、且不等于数据目录
   本身——防的不是恶意输入（路径全部来自平台目录），而是**改错的常量**。

**涉及文件（实际）**
- 新增：`crates/duck-platform/src/archive.rs`、`crates/duck-app/src/backup.rs`、
  `crates/duck-app/src/pages/data.rs`
- 修改：`crates/duck-platform/{Cargo.toml,src/lib.rs,src/logging.rs}`（`tar`/`flate2` 接上 +
  新增 `file_stamp()`）、`crates/duck-store/src/{lib,writer}.rs`（快照 / 覆盖导入 / 清表）、
  `crates/duck-app/src/pages/{mod,settings,overview}.rs`（PAGES 增页 + 渲染分支 + 假按钮接线）、
  `crates/duck-theme/src/i18n.rs`（`EN` 表 +36 条，429 → 465）

**验收标准（已验证）**
- `duck-platform` 新增 4 测：文件与目录往返、**链接被跳过所以自产包必然可解**、
  恶意 `../` 逃逸被拒且目标目录外不留文件、垃圾 / 缺失输入不 panic。
- `duck-store` 新增 3 测：覆盖导入会清掉包内没有的键、清空先于同批写入、`kv_all` 顺序稳定。
- `duck-app` 新增 7 测：导出 → 解包往返、缺清单 / 未来版本 / 缺 kv 快照三种坏包都被拒、
  目录合并且同名覆盖、重置分级只对「全清」动窗口几何、删除守卫拒绝浅路径。
- `cargo test -p duck-theme`：`EN` 表有序 + 源码中文全覆盖两条守卫通过。

**估工**：3.0 人日（**已完成**）

---

### P0-4 首启引导 + 开源许可合规

**状态：已实施**（2026-10-11）

**现状（证据，改造前）**
- 全仓 grep `license|LICENSE|第三方|许可` 于 `pages/` → 无匹配，**无第三方许可汇总**（根 `LICENSE` 是项目自身）。
- 无引导流程：首次运行直接落主窗口（`duck-app/src/app.rs` 建主窗无首启分支）。

**缺口影响**
1. 合规：发行物内含数百个 MIT / Apache-2.0 / BSD 依赖，**无 NOTICE 汇总属合规缺失**。
2. 体验：新用户第一屏是空工具箱（内置工具已全下线），**无任何引导**。

**改造方案（已实施，含实施中的设计修正）**

1. **许可汇总生成器 `sbin/licenses.py`**（原计划是 `licenses.sh` + `cargo about`，实施时改了）
   - 为什么不用 `cargo-about` / `cargo-license`：两者都要额外 `cargo install` 的第三方二进制，
     而本仓的发布作业**不编 Rust**（`release.yml` 的 publish 跑在 `ubuntu-latest`）；反过来
     `Cargo.lock` 给了完整依赖图、cargo 缓存里有每个包随源码的 `LICENSE*` 全文，拼起来就够，
     且全程离线。改成一个纯标准库 Python 脚本（只依赖 3.11+ 的 `tomllib`）。
   - 覆盖三类来源：`registry+`（crates.io，`registry/src/*/<name>-<version>/`）、
     `git+`（本仓经 `[patch.crates-io]` 换进来的 gpui-fast / zed 系列，
     在 `git/checkouts/<repo>-<hash>/<短 rev>/` 里**按 `package.name` 找**——workspace 仓库的
     目录名不等于包名）、无 `source` 的本地 crate（本仓 12 个，不入表）。
   - 输出 `assets/licenses/THIRD_PARTY_LICENSES.md`：首行是**机器可读统计**（HTML 注释，
     给客户端读，避免正则匹配中文 Markdown），然后是许可分布、组件清单（849 行）、
     按内容去重的许可全文（409 份）。
   - **可复现**：无时间戳、全部按名排序 → 同一份 `Cargo.lock` 在任何机器上输出逐字节相同。
   - `--check` 是三态退出码：`0` 一致 / `1` 已过期 / `2` 环境不支持（缺 `tomllib` 或缓存里没有源码，
     先跑 `cargo fetch`）——调用方要能区分「内容陈旧」与「这台机器查不了」。

2. **打包门禁**：`sbin/package.sh` 在 `cargo build` 之前跑 `licenses.py --check`，
   退出码 `1` 直接 `die`（阻断打包），`2` 只 `warn`。改脚本后按 AGENTS 3.4 跑了 `bash -n`。

3. **首启引导 `crates/duck-app/src/pages/onboarding.rs`**：三步向导
   （欢迎 → 语言 / 主题 → 插件市场），`kv: onboarding_done` 控制只走一次；
   「跳过」与「完成」都写标记，中途关窗**不算**见过；「设置 → 关于」可重新打开。
   - **它盖在主窗口之上，而不是替代主窗口**：先开主窗口，再开向导。这样首启时
     「上次异常退出」的通知与更新检查都照常走，不需要为引导特判
     （若向导替代主窗口，那些提示会投到向导窗口上）。
   - 第二步的语言 / 明暗直接写 `Appearance`（`update_appearance` 重建主题并刷新全部窗口），
     不设中间状态——所以在这一步改完，主窗口当场就变。

4. **开源许可窗口 `crates/duck-app/src/pages/licenses.rs`**：`include_str!` 内嵌整份汇总
   （离线、不依赖安装目录），窗口里是一段可滚动的等宽正文 + 底部统计。
   - **量过再定**：整份 1.2 MiB / 25 553 行放进单个文本块，无头渲染首帧 0.54 ms、
     次帧 0.23 ms，**自动换行与不换行同量级** → 用换行排版。
     回归守卫 `crates/duck-app/tests/licenses_view.rs`（内容比视口高 + 竖滚真的推动）。
   - 只去掉 Markdown 骨架（锚点、代码围栏、标题 `#` 前缀），**不引 Markdown 渲染器**。

5. **「关于」页**新增「法律信息」区（本程序许可 / 第三方组件统计 / 查看开源许可）
   与「信息」区的「重新查看引导」入口。

**涉及文件**
- 新增：`sbin/licenses.py`、`assets/licenses/THIRD_PARTY_LICENSES.md`、
  `crates/duck-app/src/pages/licenses.rs`、`crates/duck-app/src/pages/onboarding.rs`、
  `crates/duck-app/tests/licenses_view.rs`
- 修改：`crates/duck-app/src/main.rs`（首启分支）、`app.rs`（两个窗口句柄）、
  `pages/mod.rs`、`pages/settings.rs`（关于页两处入口）、`sbin/package.sh`（打包门禁）、
  `duck-theme/src/i18n.rs`

**验收标准（均已实测）**
- ✅ 许可汇总随依赖变化可被 `licenses.py --check` 判定过期（人为改动后退出码 1，还原后 0）。
- ✅ 内嵌全文在无头渲染里排得进布局且可竖向滚动；统计行与正文段落数一致性有单测钉住。
- ⏳ **需人工确认**：首启向导的实际观感与交互（清空 `onboarding_done` 后启动），
  以及「关于 → 查看开源许可」窗口的实际渲染——启动过程不渲染这两个窗口。

**估工**：2.5 人日（实际约 1 人日：脚本 + 两个窗口 + 门禁 + 测试）


---

## 4. P1 — 决定产品形态

> 判据：这三项决定产品是"一个窗口工具"还是"常驻的效率层"。

### P1-1 全局唤起快捷键（**已实施**）

**原现状（证据）**：全仓 grep `global_hotkey|GlobalHotkey|RegisterHotKey|hotkey` → **无匹配**。
`duck-command/src/keymap.rs` 只处理应用内键位（`<config>/keybindings.toml`）。

**缺口影响**：无法"随手唤起"。对一个常驻效率工具，这是形态级缺失。

**上游能力核实（先做这一步，结论与预期一致：上游没有）**

| 能力 | 结论 | 证据 |
| --- | --- | --- |
| 系统级热键 | **完全没有** | `grep -i 'hotkey\|hot_key\|RegisterHotKey\|global_shortcut'` 在 `gpui/src/`、`gpui_macos/src/`、`gpui_windows/src/` **零命中** |
| 隐藏应用 | **有（仅 macOS）** | `App::hide()` → macOS `-[NSApplication hide:]`；**Windows 是空实现**（`gpui_windows/src/platform.rs:715 fn hide(&self) {}`）；Linux 只记日志 |
| 还原隐藏 | **没有** | gpui 无 `unhide`；`unhide_other_apps` 操作的是**别的**应用 |
| 还原最小化 | **没有** | 只有 `Window::minimize_window()`，没有反向；且 `activate_window()` 走的 `makeKeyAndOrderFront:` 在 AppKit 里**不解除最小化**（Windows 那边 `PlatformWindow::activate` 自己会 `IsIconic → SW_RESTORE`，所以只有 macOS 需要补） |

**实施结果**
1. **依赖**：`global-hotkey 0.8.0`（Tauri 生态）。`edition 2021` / `rust-version = "1.77"` /
   `Apache-2.0 OR MIT`，MSRV 安全。macOS 走 Carbon `RegisterEventHotKey`、Windows 走
   `RegisterHotKey`；**锁文件里唯一新增下载是 Windows 上的 `windows-sys 0.59`**（本仓已有
   0.52/0.60/0.61 三条）——mac 构建零新增。api 面对外收敛在 `duck-platform/src/hotkey.rs`：
   解析（纯数据，可单测）/ 登记（碰系统）/ 投递（常驻线程）三层。
2. **键名要在两套命名之间翻译**。gpui 用键帽字符（`"a"`、`"space"`、`"up"`——读
   `gpui_macos/src/events.rs::key_to_native` 与 `gpui_windows/src/events.rs::parse_immutable`
   逐字核实），`global-hotkey` 用 `KeyA` / `Space` / `ArrowUp`，**两者都不接受对方的格式**。
   解法是把 `(gpui 名, 注册库名, 显示名)` 做成**唯一一张表** `KEYS`（约 96 条），并用三条
   测试把它钉死：不重名（三列都查）、`alt+<键>` 拼得出合法规范串、显示名对得上。
3. **默认键位**：macOS `⌥Space`，Windows / Linux `control+alt+Space`（`DEFAULT_SPEC` 按
   `cfg!(target_os)` 分派）。按下的语义是 **toggle**：不在前台 → 唤起并**打开命令面板**
   （不是"恢复上次的面板状态"，见取舍）；已在前台 → 收起。**唤起时面板挂在哪 —— 主窗内
   浮层还是独立浮窗 —— 见下面的「唤起语义（补充）」：缺省只弹独立浮窗。**
4. **设置页**（设置 → 行为 → 「唤起」）：启用开关 + 捕获式录入控件。录入用**裸 `div` +
   `track_focus`**、`on_key_down` 里先 `stop_propagation` 再解析——否则 `⌘1`（切路由）会在
   录入的同一瞬间也生效。Esc 取消（不带修饰键时）；无修饰键继续等；解析成功即 `rebind`，
   失败按错误类型分流成界面文案。
5. **落盘时机**：`rebind` **登记成功才写 `kv`**（否则重启后读档会得到一个"看起来设过了、
   其实登记不上"的键位）；换绑是**先登记新的、成功才注销旧的**，中间不留真空。
   缺省**开**——这个开关存在的意义就是"随手把 Duck 叫出来"，默认关等于让能力永远没人发现。

**实施中的取舍（都写进了代码注释）**
- **规范串选注册库的 `into_string()` 格式**，因为它自带往返保证（`"opt+space"` 会被归一成
  `"alt+Space"`），所以"存档里写的是什么"与"下次读到的是什么"不会有第二个真相。
  `the_canonical_spec_is_idempotent` 守着这条；`DEFAULT_SPEC` 也用同一个形状（首版写成
  `"alt+space"` 即被 `the_default_spec_round_trips` 拦下）。
- **至少要一个真修饰键**（`has_real_modifier`：ALT / CONTROL / SUPER / META）。只管 Shift 的
  全局热键会吃掉日常打字（`Shift+A` 在任何程序里都是打大字），这类组合实践中总会被当 bug 报回来。
- **媒体键与音量键一律拒绝**（`MEDIA_KEYS` 单独列表拦下）：macOS 上它们走 `CGEventTap`，
  要辅助功能授权，还会把系统音量 / 播放控制抢走。宁可拒绝，也不做"装完发现音量键失灵"。
- **`fn` 修饰键拒绝而非静默丢弃**（`HotkeyError::UnsupportedKey`）：丢掉它等于把一个用户
  明确表达过的组合变成另一个键位。
- **"收起"两端走不同的路**：macOS 用 `cx.hide()`（隐藏整个应用，与 `⌘H` 同款），
  Windows / Linux 用 `minimize_window()`。理由是 gpui 的 `hide` 在 Windows 上是**空实现**
  ——照搬会变成"按了没反应"；反过来 macOS 上最小化更糟，窗口会永远留在 Dock 里回不来。
  这处取舍写在 `put_away_main_window` 的注释里。
- **`restore_visible` 还原"全部窗口"**：gpui 不交出 `NSWindow` 指针（`Window` 内部是
  `Box<dyn PlatformWindow>`），而用户按下唤起键时想要的本来就是"把 Duck 叫回来"。
  为此新增 `duck-platform/src/foreground.rs`，用**已在依赖图里**的 `objc2` / `objc2-app-kit`
  发 `unhide:` 与 `deminiaturize:`。
- **`open_palette` 与 `toggle_palette` 分开**：唤起键按下的意思永远是"现在开始打字"，
  不是"把我上次开着的面板关掉"。`open_palette` 在面板已开时只清空查询、把焦点交回搜索框。
- **冲突检测只覆盖"系统拒绝"这一半**，并在模块文档里明说：Windows 的 `RegisterHotKey` 会
  失败所以能如实报；macOS 的 Carbon 允许两个进程登记同一组合（先登记的得事件），返回值是成功
  ——我们**无法**从 API 看出"被 Raycast 抢了"。设置页因此显示"未生效：…"而不是承诺
  "一定检测得到"。
- **登记失败提示同一键位只提示一次**（`kv` 记着上次提示过的串，`announce_startup_failure`），
  换个键位又失败就再说一次。提示走 sticky toast（不会被 5 s 自动关掉）。
- **`start()` 幂等**：重复调用记 warn 返回 false，避免第二条监听线程。

**唤起语义（补充，2026-10-11，已实施）**

**起因**：交付后按 `⌥Space` 实测，现象是「主界面整个被调出来」，与键位暗示不符。

**原状（证据）**：`hotkey::install` 的落点是 `app::toggle_main_window`；不在前台时走
`summon_main_window` = `restore_visible()` + `open_main_window()`（`activate_window` +
`makeKeyAndOrderFront`）**加上** `open_palette()`。而命令面板**不是窗口**，是主窗里的浮层
（`ShellView.palette_open` + `render_palette`，函数注释写的就是「展开时覆盖整个窗口」）。
所以「整个主界面被调出来」是**必然结果，不是渲染错乱**。

**判断**：行为本身没错，但**键位与语义不匹配**——`⌥Space` 紧挨 `⌘Space`（Spotlight），
自带**启动器**语义，用户按它期待的是「一个浮层盖在当前工作之上」。

**上游能力核实（结论：GPUI 原生就有这个原语）**：`WindowOptions.kind` 是公开字段
（`gpui::WindowKind` 经 `pub use platform::*` 导出），现成的 `WindowKind::PopUp` 就是启动器：

| 平台 | 落地形态 | 证据 |
| --- | --- | --- |
| macOS | `NSPanel` 子类 `GPUIPanel` + `NSWindowStyleMaskNonactivatingPanel` + `NSPopUpWindowLevel`(101) + `CanJoinAllSpaces \| FullScreenAuxiliary`；`GPUIPanel` 把 `canBecomeKeyWindow` **和** `canBecomeMainWindow` 都覆写成 YES → **非激活面板仍收得到键盘** | `gpui_macos/src/window.rs` |
| Windows | `WS_EX_TOOLWINDOW \| WS_EX_TOPMOST` + `dwstyle = 0`：无边框、置顶、不进任务栏 | `gpui_windows/src/window.rs` |

**实施结果**
1. **新增浮窗根视图** `crates/duck-shell/src/palette_window.rs`：`PaletteWindow`（持
   `Entity<CommandState>` + 查询串 + 两个回调）+ `palette_window_options()`（`kind: PopUp`、
   固定尺寸、不可缩放 / 不可最小化 / 不可拖动、`titlebar: None`、透明底）。
2. **一份卡片，两处挂载**：`command_palette.rs` 把原 `palette_frame` 拆成 `palette_card`
   （只要卡片：圆角 + 边框 + surface + 关闭按钮）与 `palette_frame`（遮罩 + 卡片）。
   浮窗里画遮罩没有意义——遮罩只会盖住面板自己那一块。两处共用同一张卡片，不会长歪。
3. **三条关闭出口**：Esc（`Command::on_cancel`）、右上角关闭按钮、**失焦**
   （`observe_window_activation`，只在「由激活转不激活」时收——否则窗口刚建出来还不是
   key window，每一次弹出都会被立刻关掉）。
4. **落点**：`hotkey.rs` 从 `toggle_main_window` 换成 `toggle_launcher`（1 行）。
5. **设置项** `kv: launcher_opens_main_window`（缺省**关**），设置页「行为 → 唤起」多一行。

**macOS 上有一条约 28px 的「隐形标题栏」**：即使 `titlebar` 设 `None`，`gpui_macos` 生成的
style mask 里仍有 `NSTitledWindowMask`（它把 `None` 与 `appears_transparent` 同样处理），
窗口顶部于是留一条**看不见但吃点击**的条带。卡片必须整体让开它（`TITLEBAR_STRIP = 28.`），
窗口也因此比卡片高一个条带高度。非 macOS 为 0。

**设置开时为什么「整条转交旧路径」而不是「主窗 + 浮窗叠加」**：一次按键屏幕上只该有
**一个**面板（主窗里再叠一个浮窗就是两个）；而旧路径是浮窗出现之前一直在跑的，窗口激活
语义与中文输入法都不必重新赌一遍——于是这个开关天然成为**浮窗出问题时的兜底路径**。

**待真机实测（均为环境事实，与实现无关；这部分没有自动化测试可覆盖）**
- **中文输入法在非激活 `NSPanel` 里能否正常上屏**——最关键的一条。不行就得退回
  `WindowKind::Floating`，代价是它会激活 Duck（Dock 亮、菜单栏切成 Duck），观感从
  「浮层」变成「切应用」。
- 应用处于 `⌘H` 隐藏态时面板能否弹出（可能需要保留一次 `restore_visible`）。
- 关闭后焦点是否还给原应用。

**涉及文件**
- `crates/duck-platform/src/hotkey.rs`（新增，约 790 行：解析 / 登记 / 投递 + 16 条测试）
- `crates/duck-platform/src/foreground.rs`（新增，`restore_visible`）
- `crates/duck-platform/src/lib.rs`（导出）、`crates/duck-platform/Cargo.toml`
- 根 `Cargo.toml`（`objc2` / `objc2-app-kit` / `global-hotkey` 三处声明，各带理由注释）
- `crates/duck-app/src/hotkey.rs`（新增，应用侧接线：`install` / `effective` / `rebind` /
  `set_enabled` / `announce_startup_failure` / `report_failure` / `explain`）
- `crates/duck-app/src/settings.rs`（3 个 kv 键 + 6 个读写函数）
- `crates/duck-app/src/app.rs`（`toggle_main_window` / `summon_main_window` / `put_away_main_window`）
- `crates/duck-shell/src/view.rs`（`toggle_palette` 拆出 `open_palette`）
- `crates/duck-app/src/pages/settings.rs`（「唤起」分区 + 录入控件 + `on_hotkey_key_down`）
- `crates/duck-app/src/main.rs`（`hotkey::install` / `announce_startup_failure` 接线）
- `crates/duck-theme/src/i18n.rs`（表 515 → **529**，+14 条）
- `assets/licenses/THIRD_PARTY_LICENSES.md`（重跑：854 个组件 / 413 份全文 / 1224 KiB）

**涉及文件（唤起语义补充）**
- `crates/duck-shell/src/palette_window.rs`（新增，浮窗根视图 + `palette_window_options`）
- `crates/duck-shell/src/lib.rs`（导出模块）、`crates/duck-shell/src/view.rs`
  （`PaletteGroupsSource` / `PaletteEmptySource` 两个别名放到 `pub(crate)`，与浮窗共用同一份数据源）
- `crates/duck-components/src/command_palette.rs`（`palette_frame` 拆出 `palette_card`）
- `crates/duck-app/src/app.rs`（`toggle_launcher` / `open_palette_window` / `close_palette_window`
  / `Desktop::palette_window` 句柄；`toggle_main_window` 改为由 `toggle_launcher` 按设置转交）
- `crates/duck-app/src/hotkey.rs`（落点一行）、`crates/duck-app/src/settings.rs`（1 个 kv 键 + 2 个读写）
- `crates/duck-app/src/pages/settings.rs`（「唤起」分区多一行开关）
- `crates/duck-theme/src/i18n.rs`（+2 条）

**验收标准（单测结果）**
- 键名表自洽：不重名（三列）/ 每条都能拼出被解析器接受的规范串 / 每条都有显示名 / **每条都
  在无修饰键时被拒** ✅
- 解析边界：空串与畸形串被拒、只按修饰键与"坏键名"分别报 `ModifierOnly` / `UnsupportedKey`、
  `opt+Space` **必须失败**（库的别名表里只有 `option`，没有 `opt`——这条断言是防止以后有人
  照 macOS 习惯加进来）、媒体键与音量键被拒、`fn` 被拒 ✅
- 归一化与幂等：别名（`option` / `alt` / `command`）归一、大小写不敏感、规范串往返不动 ✅
- 服务初始态空闲 ✅
- **需人工确认**（本轮按约定不启动应用）：默认 `⌥Space` 在**别的程序里**能否唤起、
  已在前台时再按是否收起、设置页的录入与冲突提示观感。

**已知限制**
- **Linux 只覆盖 X11**（注册库的限制），Wayland 下不生效——注册库在该平台是 no-op 且**全成功**，
  所以我们会以为登记上了。已记入本项限制，未额外探测（YAGNI，等真有 Wayland 用户）。
- **macOS 冲突不可探测**（见取舍）；Windows 可如实报。
- 唤起时命令面板必开，不恢复上次面板的开合状态（有意为之）。
- **唤起语义补充（浮窗）的三条环境待测项**见上：非激活 `NSPanel` 下的中文输入法上屏、
  `⌘H` 隐藏态下能否弹出、关闭后焦点归还。三条都只能真机跑，自动化测不到。

**估工**：3.0 人日（**已完成**；实际相当——注册库行为核对与键名翻译表是主要成本）
- 唤起语义补充（浮窗）：约 0.5 人日（未计入上面的 3.0；属交付后据实测反馈做的形态调整）

---

### P1-2 窗口会话恢复（**已实施**）

**原现状（证据）**
- `windows` 表**只存主窗几何**：`duck-store/src/lib.rs:81` 读单 key，`writer.rs:115-157` 写 `key/bounds/monitor_id`。
- 设置 / 日志 / 插件中心窗口每次**居中重建**（`app.rs` 建窗路径）；无「上次所在路由」记录。

**缺口影响**：重启后回到首页，窗口布局丢失 —— 违背"桌面应用该记住我的状态"的基本预期。

**实施结果**
1. **落地位置**（与原方案的分工不同）：几何继续走 `windows` 表（key 扩到
   `main` / `settings` / `logs` / `plugin-center` / `licenses`），**会话状态走 `kv` 表**
   （`window_route` / `window_right_sidebar_open` / `window_aux_open` /
   `restore_windows_on_launch`）。理由：几何是「每窗口一行的结构化数据」，会话是「上次
   长什么样」的一小段状态，混进同一张表会把它的语义撑散。**未新增表**。
2. **路由恢复**：`ShellView::with_initial_route` 在装配期指定首屏路由；存档里的 id 不在
   `KNOWN_ROUTES`（直接引用 `pages` 的路由常量，不抄字面量）或路由表里时**静默回落首条路由**
   —— 存档可能来自旧版本。变化经 `on_route_changed` 钩子去抖落盘。
3. **右栏展开态**：`on_right_sidebar_changed` 只回报**活跃路由**的右栏（非活跃路由的右栏
   在关窗时即丢弃）；建窗时就位，避免「先弹出、再收起」。
4. **辅助窗口**：`Desktop::aux_window` 收敛了原先四个手写字段映射；开窗即记
   `window_aux_open`，关窗由 `cx.on_window_closed` 摘除。**默认不自动恢复**，
   开关在「设置 → 行为 → 启动」。
5. **几何 + 显示状态**：`windows` 行多写一个 `state`（`windowed` / `maximized` /
   `fullscreen`），旧存档缺这个字段按普通窗口处理。

**实施中修正的三处原设计（都是实测逼出来的）**
- **`window_bounds()` 在 macOS 上永远不报 `Maximized`**（`gpui_macos` 只区分全屏与普通）。
  必须改用 `Window::is_maximized()`，否则最大化着退出会被静默记成普通窗口。已实测：
  改前写回 `state:"windowed"`，改后为 `state:"maximized"`。
- **夹取不能「纠正」用户刻意停的位置**。首版按「塞回可见区」实现，实测把一个用户拖到
  `(553,134)` 的窗口（右、下各探出一点，系统本就允许）改成了 `(312,92)` 并写回 ——
  等于每次启动都替用户挪窗口。改为**只在窗口基本看不见时（可见面积 < 1/4）才救**。
- **退出必须显式 `flush`**：平台退出走 `process::exit`，`Drop for Store` 不保证跑到，
  去抖窗口（400 ms）内的改动会丢。新增 `cx.on_app_quit` 落盘钩子。

**涉及文件**：`duck-shell/src/view.rs`（`RouteId::as_str` / `with_initial_route` /
`active_route_id` / 两个钩子 / `set_active_right_sidebar_collapsed`）、
`duck-shell/src/auxiliary.rs`（几何钩子）、`duck-app/src/settings.rs`（会话读写 + 夹取，纯几何核心
可单测）、`duck-app/src/app.rs`、`main.rs`、四个辅助窗口开窗点、`pages/settings.rs`（开关）、`i18n.rs`。

**验收标准（实测结果）**
- 在 SSH 占位页关闭应用 → 重启落回该路由：✅ 日志 `main window opened route="ssh"`；非法 id
  `nonsense` → `route="overview"`（回落）。
- 主窗位置与尺寸一致：✅ 用户位置 `(553,134)` 逐字节保留；几何往返无漂移。
- 外接屏拔除后窗口可见：✅ `(-5000,-5000,1200,800)` → 救回 `(0,33,…)`；最大化状态保持。
- 辅窗打开集：✅ 种子 `["settings","licenses"]` → `count=2` 且写回精确一致。

**估工**：2.5 人日（实际相当）

---

### P1-3 更新链路安全（签名 + 保留 + 回退）（**已实施**）

**原现状（证据）**
- 无签名校验：`duck-update` 只有 sha256（`sbin/manifest.py` 生成 1 MiB 分块 sha256）。
- **无回退**：`duck-update/src/swap.rs` 把 `existing → existing.duck-old`，旧版被下次启动 `sweep` 清掉 → 无长期保留、无一键回退。
- `duck-app/src/update.rs::DEFAULT_SOURCE` = `…/releases/latest/download/update.json`（版本无关别名），**无 beta 通道**。

**缺口影响**
1. sha256 只能防传输损坏，**不能防源被篡改**（CDN 被投毒 / 仓库被入侵即全线沦陷）。
2. 更新后出问题**无法回退** —— 用户只能手动重装。

**实施结果**
1. **签名**：minisign 格式（Ed25519 + BLAKE2b-512 预哈希），detached `update.json.sig`。
   客户端只**验签**（`minisign-verify`，零传递依赖，MIT），私钥永不进二进制；
   公钥**编译期内建**（`option_env!("DUCK_UPDATE_PUBKEY")`），未配置时整套校验关闭。
   `pipeline::check_with` 在 `parse` **之前**验签——不先花力气读一份未必采信的文档，
   也让「没签名的 manifest」在任何一个字节的归档动起来之前就被拒。
2. **保留**：`.duck-old` 的语义从「下次启动要清扫的残留」改为「上一版留存」——
   `sweep` 不再删它，只清失败的安装留下的暂存目录（`.duck-update-<pid>`）。
   同一时刻至多留一份：`swap` 顶掉当前版之前会先删掉更早的那一份。
3. **回退**：`swap::rollback` 把两条路径互换（三次改名 + 同目录临时名），
   换完之后**当前版成为新的落点**——所以可以来回切。`PlatformInstaller` 新增
   `rollback()` / `has_previous()`；后者让「关于」页只在真有上一版时才显示按钮。
4. **manifest 新字段**：`min_app_version`（可选）。低于门槛的客户端拿到
   `UpdateError::AppTooOld`（带 current / required），而不是半懂地装上新格式。
   门槛只在「确实有更新可装」时拦——已是最新时能不能装是无所谓的。

**实施中的取舍（都写进了代码注释，不是事后补的）**
- **签名不放进 manifest 内部**：那会变成自引用，验签前就得先信任一半内容。所以用
  detached `.sig`，地址约定为 manifest 地址后面加 `.sig`（换通道时两个地址一起换）。
- **不用 Ed25519 的「标准」Rust 实现签名、自己写 Python 签名侧**：`sbin/minisign_sign.py`
  是纯标准库（`hashlib` + 大整数）的 RFC 8032 实现。理由有三：CI 三个 runner 都自带
  python3，发版不必装东西；这条路径上没有看不透的二进制；以及「零第三方依赖」这条
  项目习惯在发布链路上同样成立。代价是那一百来行数学要自己保证对，所以有 `--selftest`
  跑 RFC 8032 的官方向量。
- **`allow_legacy = false`**：只接受新版 Minisign 的预哈希签名。旧格式换不来任何
  兼容性收益（更新源是我们自己的），只会多留一条没打算支持的路径。
- **回退失败不改阶段**：失败只发一条 toast。把「已是最新」这种好状态冲成「更新失败」
  是错的——用户只是没能回退，不是更新坏了。

**涉及文件**
- `duck-update/src/`：新增 `signature.rs`；改 `swap.rs`（`rollback` / `sweep` 语义）、
  `target.rs`（`is_staging` / `STAGING_PREFIX`）、`install/{mod,macos,windows}.rs`
  （`rollback` + `has_previous`）、`manifest.rs`（`min_app_version`）、
  `error.rs`（3 个新变体）、`pipeline.rs`（验签关卡 + 门槛比对）、`lib.rs`
- `sbin/`：新增 `minisign_sign.py`（生成密钥 / 签名 / 自检）；`manifest.py` 加
  `--sign-key` / `--sig-out` / `--min-app-version`
- `.github/workflows/release.yml`：构建期注入公钥、签 manifest、同步与上传 `.sig`
- 根 `Cargo.toml` + `duck-update/Cargo.toml`：`minisign-verify = "0.3"`
- `duck-app/src/update.rs`（`Phase::RolledBack` / `rollback_now` / `can_rollback`）、
  `pages/settings.rs`（回退入口）、`assets/licenses/THIRD_PARTY_LICENSES.md`（重跑）
- ⚠️ `release.yml` 的 `upload-artifact` path 保持 `dist/duck-*-${{ matrix.platform }}*`
  （多一个尾连字符会整条漏掉 Windows）；发布资产用的是 `dist/update.json*`，
  尾随 `*` 让签名在存在时被捎上、不存在时也不触发 `fail_on_unmatched_files`

**验收标准（单测结果）**
- 篡改一个字节 → 验签失败：✅ `a_single_flipped_byte_breaks_the_signature`
- 篡改 trusted comment → 失败（global signature 覆盖到它）：✅
- 换成别的公钥 → 失败且理由写明「different key」：✅
- **跨语言闭环**：`sbin/minisign_sign.py` 生成的签名被客户端接受；反向（改一字节）被拒：✅
  `tests/release_toolchain.rs` 真跑一遍 `--generate-key` → `manifest.py --sign-key` → 验签
- RFC 8032 官方向量：✅ `--selftest`（3 条），并由 `the_signing_helper_passes_its_own_rfc8032_selftest` 守住
- 回退：换回上一版、再回退一次回到原处、没有上一版时明确报错、不留临时名：✅ 4 条
- 启动清扫不再毁掉回退源：✅ macos / windows 各一条
- 门槛：低于门槛被拒（带 current/required）、已是最新不受门槛影响：✅ 含跨进程契约测试

**估工**：5.0 人日（实际相当；签名侧的纯标准库实现是主要成本）

---

### P1-4 命令面板升级（工具 / 最近 / 模糊 / 设置项直达）（**已实施**）

**原现状（证据）**
- `duck-command/src/palette.rs:21-25`：`palette_groups()` 只遍历 `action::metas(cx)`，**不含工具、不含最近**。
- `duck-tools/src/registry.rs:233` `record_use` 在写 `recent` 表，但 `:247` 的 `recent()` **UI 从不调用**（只写不读）。

**缺口影响**
命令面板是效率层的核心入口，却搜不到工具箱里的工具 —— 四输出面里的"命令面板"这一面覆盖率最低。

**实施结果**
1. **数据源合并**（`duck-app/src/palette_tools.rs`）：面板分组顺序固定为
   **最近使用（≤5，置顶）→ 动作 → 设置 → 工具（按分类）**。「最近使用」**不从分类组里
   摘人**（同一工具两处都出现）——按分类浏览的列表不因最近项被抽走而残缺（用户选定）。
   路由跳转原本就由动作面覆盖（`route.*`），未重复建设。
2. **过滤权移交宿主**：上游 `CommandItem::matches` 是 `pub(crate)` 的 `contains`，**没有
   matcher 注入缝**；正门是 `Command::filterable(false)`（上游文档原文 *"Turn it off when
   an external source already answers the query"*）。`render_palette` 挂上它后，上游把
   供给的条目**原样全留**、查询串照旧从 `on_query` 回报——命中与排序全部由
   `duck-command` 新增的 `fuzzy` 打分器决定。**零新依赖、零上游改动**。
3. **模糊匹配**（`duck-command/src/fuzzy.rs`，纯字符串零依赖）：档位阶梯
   标签全等(1000) > 标签前缀(900) > 附加词全等(860) > 标签包含(800) > 附加词前缀(760)
   > 附加词包含(700) > 子序列(≤600)，阶梯顺序用 `const _` **编译期断言**钉死
   （与 `secret.rs` 同一手法）。子序列（跳跃命中）只认**标签**、不认附加词——
   拼音串动辄十几个字母，两字母查询几乎必然构成子序列，会淹没有效结果。
4. **设置项直达**：`PAGES` 经 `palette_pages()` 快照进面板，条目动作
   `OpenSettingsPage` 走**既有**的 `open_settings_at(page, None, cx)` 投递槽。
   匹配词铺中英双语 + 分类 key + 描述。**分区级（卡片内某组）滚动定位未做**——
   设置内容区目前是裸 `overflow_y_scrollbar()`，没有 `ScrollHandle`，做定位要先
   引入滚动句柄并核实 gpui-fast 的定位 API，留给 P2-1 一起。
5. **空结果引导**：`Command::empty` 接管（上游只在 `rows_count == 0 && !loading` 时渲染），
   引导按钮「打开插件中心」点击后顺手把面板关掉（再派发一次 `palette.toggle`）。
6. **与 P2-1 共用索引**：打分器放在 `duck-command::fuzzy`（duck-shell / duck-app 都依赖它），
   顶栏搜索届时直接复用同一份规则，不会漂移。

**实施中确认的两处上游事实（都是读源码 + 无头实测钉住的）**
- `CommandState::set_query`（程序化写入）**也会**触发 `on_query`（`on_query_changed`
  里 `applied_query` 去重后照常回报）；`toggle_palette` 每次打开都会 `set_query("")`，
  所以 shell 侧的 `palette_query` 也要显式归零，不能指望回调一定来。
- 上游只在 `rows_count == 0 && !loading` 时渲染空态——我们供给零分组时正好落进去。

**验证**
- 无头：`duck-command` 19 + 4（打分器 12 + 面板排序 7 + 既有派发 4，含组内排序、
  空组消失、子序列兜底）；`duck-app` 新增 6（4 条真实数据源分组顺序 / 命中归组 /
  「最近使用」置顶——用**真临时库 + `flush`** 跑通，不碰用户数据目录；2 条
  `palette_filtering` 契约守卫，**对照组**证明「条目全留着」确由 `filterable(false)`
  带来而非上游本来不过滤）。全仓测试 55 + 1 + 4 + 3 + 2 + 19 + 4 + 8 + 13 全绿。
- 门禁：`fmt --check` 干净；clippy 我方文件零新增（阶梯断言改 `const _` 后顺带消掉
  6 条 `assertions_on_constants`）；`sbin/licenses.py --check` exit 0（**零新依赖**，
  Cargo.lock 未动）；i18n +3（`最近使用` / `没有匹配的项` / `换个关键词试试…`），
  表 504 → 507，两条守卫绿。
- 实机：dev bundle 重启冒烟通过，日志 `main window opened route="overview"`、
  **零 ERROR**（唯一 WARN 是既有的 `plugin index 404`）。
- **未人工验证**：⌘K / 顶栏胶囊打开面板后的实际观感（键盘事件无 Accessibility 授权
  送不进去，与 P1-2 时的结论一致）——需要人工点一遍：模糊命中（如输入「统解」
  应命中「视频统一解析」）、最近使用置顶、设置项直达、空结果引导。

**涉及文件**：`duck-command/src/{fuzzy.rs(新增), palette.rs, lib.rs}`、
`duck-shell/src/view.rs`、`duck-app/src/{palette_tools.rs, app.rs, pages/mod.rs, pages/settings.rs}`、
`duck-app/tests/palette_filtering.rs(新增)`、`duck-theme/src/i18n.rs`。

**验收标准**：面板输入工具名可直接执行 ✅（含拼音与跳跃命中）；最近使用项置顶 ✅；
无结果时展示引导态 ✅。设置项直达按分类粒度达成（分区级滚动定位移交 P2-1）。

**估工**：3.0 人日（**已完成**）

---

### P1-5 拖拽 + 深链接 + 文件关联（**已实施**）

**现状（证据）**
- 全仓 grep `on_drop|FileDrop|drag_drop` → **无匹配**：**拖拽完全未实现**。
- 全仓 grep `on_open_urls|CFBundleURLTypes|CFBundleDocumentTypes` → **无匹配**：**深链接与文件关联为零**。
- `sbin/package.sh` 只做 codesign，**无 notarize 步骤**（记忆已确认）。

**缺口影响**
1. 工具箱类应用最常见的输入方式（拖文件进窗口）不可用。
2. 无法从浏览器 / 终端唤起（`duck://`），无法注册文件类型打开方式。

**上游能力核实（先做这一步，结论推翻了原判断）**

| 能力 | 结论 | 证据 |
| --- | --- | --- |
| 文件拖放 | **有** | `FileDropEvent`（`gpui/src/interactive.rs:737`）+ `.drag_over<T>` / `.on_drop<T>` / `.can_drop`（`elements/div.rs:1191/1229/1239`） |
| 深链接 | **有** | `Application::on_open_urls`（`gpui/src/app.rs:287`）；macOS 走 `NSApplicationDelegate` 的 `openURLs:`（`gpui_macos/src/platform.rs:153/1568`），Windows 也有（`gpui_windows/src/platform.rs:789`） |
| 全局热键 | **完全没有** | `grep -i 'hotkey\|hot_key\|RegisterHotKey\|global_shortcut'` 在 `gpui/src/`、`gpui_macos/src/`、`gpui_windows/src/` **零命中** → 曾属 P1-1，已引 `global-hotkey` 补上（见 P1-1 章节） |

关键机制（决定了实现形状，全部读源码确认过）：
`Window::dispatch_event`（`window.rs:5607`）把平台文件拖放**翻成内部拖拽**——
`Entered` 建一个负载为 `ExternalPaths` 的 `active_drag` 并转成 `MouseMove{左键}`，`Submit`
转成 `MouseUp{左键}`。于是「接住文件」= 挂一个 `on_drop::<ExternalPaths>`，
「拖拽悬停态」= `drag_over::<ExternalPaths>`（**声明式样式，不需要自己维护状态**）。
`on_drop` 的派发守卫是 `hitbox.is_hovered(window)`，而 `HitboxBehavior::Normal` 的元素
**全部**进 `hit_test.ids`（`window.rs:1174-1192`），所以挂在根容器上时子元素不会挡住它。

**实施结果**
1. **拖拽**：`ShellView` 新增 `on_files_dropped` 回调与整窗落点；悬停时铺 `hover_tint`
   薄强调底（与悬停同一族语义）。文件落下后走**工具箱既有的智能粘贴链路**
   （`duck_tools::paste::detect` → `PasteInbox` → 横幅上由用户点确认）——不新造一套
   「拖入后干什么」的语义，也就不需要第二个待处理状态。
2. **深链接** `duck://tool/<id>`：`duck-app/src/deeplink.rs` 解析（纯函数，可单测），
   落点是「切工具箱路由 + 打开该工具」。macOS 在 `package.sh` 的 `Info.plist` 里注入
   `CFBundleURLTypes`；Windows 在运行期写 `HKCU\Software\Classes\duck`。
3. **单实例转发**：`duck-platform/src/ipc.rs`。Unix 用 `app_data_dir()/duck.ipc` 上的
   `UnixListener`（socket 文件权限收紧到 `0600`）；Windows 绑 `127.0.0.1` **随机端口**、
   把端口号写进同名文件（固定端口会被别的程序占，那时会把「端口被占」误判成「已有实例」）。
   第二实例在让位**之前**把本轮 argv / URL 送过去，正主侧由一条 `cx.spawn` 的主线程任务消费。
4. **文件关联**：`.ducktool` 注册为可打开类型；打开后落到**插件中心**并给一句提示
   （安装要走索引里的摘要校验，凭空一个文件装不了，所以这里不假装能代劳）。
5. **命令行唤起**：`duck <路径>` / `duck duck://…` 与深链接走同一套解析——`OpenRequest`
   把「URL 还是路径」归一之后，下游只有一条路。

**实施中的取舍**
- **不做「拖入即执行」**：拖入与深链接都只把内容放进智能粘贴横幅，动手与否交给用户。
  这既与既有「识别粘贴」按钮一致，也避开了「拖错文件就跑了工具」这类惊吓。
- **`on_files_dropped` 未挂时完全不接拖放**（连悬停底色也没有）：给一个「能拖但拖了没反应」
  的视觉承诺比不支持更糟。
- **IPC 协议用手写文本行**而不是 JSON：内容只有「一批 URL / 路径」，为它把 `serde` 拉进
  `duck-platform` 不划算；首行魔数认不出就整条丢弃，宁可什么都不做也不把别家程序发来的
  字节当成用户意图。
- **注册表项的形状单独抽成纯数据**（`handlers::entries`）：写注册表的 API 只有 Windows 有，
  但「写哪几个键」没有平台属性，抽出来之后这部分就能在 macOS 上单测——而它恰恰是最容易
  写错、错了又只在 Windows 上才看得见的地方。
- **`.ducktool` 没做成「双击即安装」**：那需要绕过索引里的字节数与摘要校验，或者先伪造一条
  `RegistryEntry`。两条路都在拿安装链的可信度换一点点便利，不值。
- **Windows 单实例**：那边 `flock` 是空实现（`lock.rs` 的 `cfg(not(unix))`），所以
  「端口能不能连上」顺带成了该平台上的单实例判据——这是通道设计顺带补上的一个既有缺口。

**涉及文件**
`crates/duck-platform/src/{open,ipc,handlers}.rs`（新增）、`crates/duck-platform/src/lib.rs`、
`crates/duck-platform/tests/packaging_matches_the_code.rs`（新增，跨文件契约）、
`crates/duck-shell/src/view.rs`（拖放回调 + 悬停态）、
`crates/duck-app/src/deeplink.rs`（新增）、`crates/duck-app/src/app.rs`（开放 `navigate_to` /
`open_tool`）、`crates/duck-app/src/main.rs`（入口接线）、`sbin/package.sh`（plist）、
`crates/duck-theme/src/i18n.rs`。

**验收标准**
- 浏览器 / 终端打开 `duck://tool/xxx` → 已运行实例跳到对应工具；未运行时先启动再跳 ✅
  （解析与派发各由单测覆盖；「真的跳了」需实机验收）
- 已运行实例存在时，命令行再次 `duck` **不产生第二个窗口**，且带参数的 `duck <文件>`
  能把文件送到正主 ✅（`ipc` 的真实 socket 往返测试覆盖转发；窗口数需实机验收）
- 拖入文件时出现 drag-over 态 ✅（`drag_over` 的样式生效路径已核实；观感需实机验收）
- 打包脚本里的方案名 / 扩展名与 Rust 常量一致 ✅
  （`packaging_matches_the_code.rs` 三条契约测试盯着，防「改了一处忘了另一处」）
- **已知限制**：`.ducktool` 打开只到插件中心，不直接安装（理由见「取舍」）；
  Windows 的 `file://` URL 形式未处理（那边文件关联走注册表 `%1`，不会发 URL）。

**新增测试**：`duck-platform` +14（`open` 9 · `ipc` 9 里含 5 条真实 socket 往返 / 陈旧落点
回收 / 二次监听被拒 · `handlers` 5）、`duck-platform/tests` +3（跨文件契约）、
`duck-app` +13（`deeplink` 解析与转义）；i18n 表 513 → **515**。

**估工**：5.0 人日（**已完成**）

---

## 5. P2 — 体验与一致性

### P2-1 顶栏全局搜索（**已收口；实质由 P1-4 交付**）

**原现状**：`top_bar.rs` 占位写「搜索工具、命令…」，实际只写 `ToolQuery` **过滤工具箱**，与副标题承诺不符。

**实况（复核后修正了原判断）**：P1-4 落地时把顶栏入口从「输入框」改成了**打开命令面板的触发器**
（`on_click` 派发 `palette.toggle`，与 `⌘K` 同一条链），角标从键位注册表派生。于是承诺里那件事
——「搜工具、搜命令」——现在由面板本身兑现，而面板的分组已经是
**最近使用 → 动作（含路由）→ 设置 → 工具（按分类）**，命中与排序走 `duck_command::fuzzy`。
也就是说 P2-1 原方案的四条要求（跨四类、分组、键盘上下选择 + Enter、与 P1-4 共用索引）
**全部已达成**，不需要再写第二套搜索。

**本次只做了「收口」与如实记录**，没有改动顶栏文案：那行占位文字依然成立（面板里排在前面的
就是工具与命令），而改文案要动 i18n 与一处用户可见的界面，收益不抵成本。

**仍未做的部分（原 P1-4 移交项，现改判）**：命令面板的「设置」分组只到**分类**粒度
（「设置 → 行为」），点进去落在分类顶部，**不会滚到具体那一行**。要做分区级直达需要：
① 把设置内容区（现在是「滚动容器 → 一个 body 容器 → 各分区」三层）**拍平**成
「滚动容器 → 各分区」两层 —— 这是 `ScrollHandle` 能寻址的前提
（`child_bounds` 只记**直接子元素**的布局位置，见 `gpui/src/elements/div.rs:1990-2003`）；
② 再补一张分区登记表（key / 标题 / 关键词）。
把它移出 P2-1 的理由：这是一次**设置页结构重构**，而 P3-3 本来就要拆 `pages/settings.rs`
（2300+ 行），届时顺手拍平最自然，不必现在为它单独改一遍再改一遍。

**涉及**：无代码改动（复核确认；原方案预计的 `top_bar.rs` / `i18n.rs` 改动均不需要）。

**估工**：2.0 人日（**实际 ≈0.2：复核后确认 P1-4 已交付，只补了文档与遗留说明**）

---

### P2-2 日志增强（**已实施**）

**原现状（证据）**：`duck-platform/src/logging.rs` 按天滚动 + `prune_logs(dir, keep_days)`
（保留天数已在设置页日志页顶部配置）；**无单文件大小上限**（长跑 / debug 级别下单日文件可无限膨胀）。

**实施结果**

1. **单文件上限 —— `LOG_FILE_MAX_BYTES = 8 MiB` + 序号轮转**（`logging.rs`）。
   `RollingWriter` 从「换日才重开文件」改成「换日、或当天写满就重开」：写满时
   `duck-YYYY-MM-DD.log` → `duck-YYYY-MM-DD.1.log`，已有的段各推一格。
   **段窗口有界**：`LOG_SEGMENTS_PER_DAY = 5`，最老的一段被挤掉，所以「一天最多占多少」
   有确定答案（5 × 8 MiB = 40 MiB），而不是「按天滚动」听起来无害、实际能吃到整块磁盘。
   已写字节数是**自己数的**（`OpenFile::written`），不在写入路径上 `stat`。
2. **目录占用兜底 —— `LOG_DIR_MAX_BYTES = 256 MiB`**（`enforce_size_cap`）。
   超出就按「最老优先」删到线下；排序键 `(日期戳, 段序号)`，同一天里段序号大的更老。
   **永不删最新那一个文件**：它多半正是当前在写的那个（Windows 上还删不掉），而「把日志
   删到一条不剩」比「稍微超一点」更像一次故障。
   它**与保留期无关**——`LOG_RETENTION_FOREVER` 拦得住按时间删，拦不住占用兜底：两者回答的
   是不同的问题（「多久之前的不要了」vs「这台机器上最多占多少」）。这条语义有专门的测试盯着。
3. **查看器加级别过滤 / 关键字搜索 / 导出**（`pages/logs.rs`）。
   过滤在**事件**粒度上做，不是行粒度：边界是「下一个有级别列的行」。这样留下的条目永远
   带着自己的头（不会出现看不出是谁在报的裸续行），关键字命中续行时也能把整条捞出来
   （否则那么长的 `{:#?}` 展开永远搜不到），「留下 N 行」跟「N 条日志」也对得上。
   过滤结果**算一次就存起来**（`refilter`），不在 `render` 里算——`render` 会因为鼠标悬停
   之类的原因随时被调到，而过滤要遍历整段尾巴的两千行。工具条因此扩成两行（文件 / 过滤）。
   「导出」导的是**过滤后**的文本：刚把范围收窄到想看的那几行，再导一份几万行的完整文件
   等于白过滤。按钮在无可导出内容时置灰，不靠弹一句「没内容」。
   「过滤到一行不剩」有独立空态，与「目录里没有日志」分开说。
4. **设置页日志页补两个只读项**：「单文件上限」（8 MB + 一天 5 段 + 目录兜底 256 MB）、
   「当前占用」（N 个文件 · X）。占用数字**跟着查看器的每秒扫描走**（订阅它的 `notify`），
   不在渲染里 `read_dir`。

**顺带修掉的两个既有缺陷**

- **查看器的文件列表排序**：原来直接比文件名倒序。段序号是十进制数字，字典序会把 `.10`
  排到 `.9` 前面，于是同一份日志的旧段跑到新段前头。改成按 `(日期, 段序号)` 排：日期照旧
  比字符串（`YYYY-MM-DD` 字典序即时间序），段序号按数值升序。
- **命名规则两处实现**：写入方知道拼法、读出方得再猜一遍，必然会漂。把
  `log_file_stamp` / `log_file_segment` 从 `duck-platform` **公开**出去给查看器用，
  并加一条**往返契约测试**（`file_name` / `segment_name` 造出来的名字必须被两个解析函数
  解释回原值）——谁改了拼法立刻红。

**取舍（4 条）**

1. **上限做成常量，不做用户可调项**。可调的东西要配界面、要落盘、要文档，而这只是一个
   兜底；真正的旋钮是保留天数（已有）。
2. **轮转失败一律忽略**（改名 / 删段 / 建目录）。最坏结果是继续往旧文件里写，比在写日志的
   路径上抛错好得多——日志系统不该成为故障源。
3. **单条日志比上限还大时不轮转**（`written > 0` 这个条件）。否则每次写入都会新开一个文件，
   变成死循环。
4. **不做日志内容直接 `` `tail -f` `` 式流式跟随的改造**。现状是每秒 `stat` + 长度守卫，
   已经把「看起来实时」和「白跑 3153 万次」之间的账算清了（见 `pages/logs.rs` 的模块文档）。

**涉及**：`crates/duck-platform/src/logging.rs`（+9 测试）、`crates/duck-app/src/pages/logs.rs`
（+10 测试）、`crates/duck-app/src/pages/settings.rs`、`crates/duck-theme/src/i18n.rs`（+12 条）。

**验证**：`cargo test --offline --workspace` 35 个测试目标全绿（`duck-platform` 87 条、
`duck-app` 75 条）；`cargo fmt --all -- --check` 干净；`clippy --all-targets` 零新增告警
（新代码 0 条，全仓 16 条均为既有）。

**估工**：2.0 人日（**已完成**；实际相当——段窗口 / 占用兜底 / 事件粒度过滤三处的边界
条件各占了一部分，且顺带修掉了排序缺陷）

---

### P2-3 死代码 / 死状态清理（**已实施**）

**原现状（证据，逐条）**
| 位置 | 问题 |
| --- | --- |
| `duck-store/src/db.rs:49` | `settings` 表**建了但无任何读写方**（死表） |
| `duck-app/src/pages/toolbox.rs:147/149/151` | `download_limit_bps` / `upload_limit_bps` / `proxy_mode` 三个本地字段，不落盘、无消费方 |
| `duck-app/src/pages/settings.rs:399-400` | `compact_rows` / `confirm_delete` 两个开关，仅改内存（`:1304-1319`），不落盘、无消费方 |
| `duck-shell/src/view.rs:122/204` | `ShellRoute::visible` / `ShellAction::visible` **永远是 `true`**，三处过滤是空转（本项清理时新发现） |

**判据**：逐项二选一 —— **接线**（落 `kv` + 真实消费）或**删除**。**能接线的前提是存在真实消费方**，
否则「落盘」只是把假控件变得更像真的（重启后数值还在，用户更相信它生效了）。

**实施结果（4 项逐条处置）**
1. **`settings` 表 → 删除**（新增迁移 v2：`DROP TABLE IF EXISTS settings`）。`MIGRATIONS` 只追加、
   不改已发布条目，所以这条以 v2 落地；`user_version` 由 1 推到 2。**从未写入过 → 没有数据会丢**。
   两条测试盯着：新建库与「从 v1 升级上来」都必须看不到这张表（反向断言，将来谁想再加一张
   「存放设置的表」会先撞上 v2 的说明，而不是默默加回来）。
2. **工具箱三字段（限速 / 代理）→ 删除**。三个字段本身有界面（限速预设下拉 + 自定义 KB/s 弹窗、
   代理模式下拉），**是「长得完全能用」的那一类假控件** —— 改了当下有效、重启就没了，而下载引擎
   不存在，所以「真实消费」无法达成。删掉 `render_speed_limit_control` / `render_proxy_control` /
   `apply_speed_limit` / `SPEED_PRESETS_MB` / `PROXY_MODES` 与状态栏里的两个调用点。
   状态栏保留的「全部暂停 / 全部开始 / 完成后关机」**都以「演示：…」自我标注**（点击即弹），
   属于诚实的演示项，与本次删除的沉默假控件不是一类，故保留。
   ⚠️ **10-12 口径反转（见 P3-6）**：这三个控件**已整体删除** —— 用户明确「界面不应再有演示
   文字、每个图标都要有真实功能」，「自我标注」不再构成保留理由。
3. **`compact_rows`（列表紧凑行高）→ 删除**。它的说明写的是「任务列表的行高从 44 收到 30」，
   而本应用**没有任务列表**（工具箱是工具页签，不是下载任务队列），没有任何可消费方。
4. **`confirm_uninstall`（原 `confirm_delete`）→ 接线**（本项唯一接线项）。文案是「删除工具前弹确认」，
   而真存在这条链路：插件中心 → `market::uninstall_plugin` **会真删插件目录**并摘掉它贡献的工具，
   原先**没有任何确认**。现在卸载前读 [设置项] 决定是否弹 `duck_platform::confirm`，
   设置也落 `kv`（缺省**开**：误点一次要去市场重装，多一次确认只多一次点击）。
5. **`visible` 死标志 → 删除**。两处字段永远为 `true`，三处 `.filter(|x| x.visible)` 与
   `has_activity_items` 的 `any(visible)` 都是空转。改成按实际列表判空。

**验证**
- `duck-store` +2：`the_dead_settings_table_is_dropped`、`upgrading_a_v1_database_drops_the_settings_table`
  （前者覆盖新建库，后者把库退到 v1 并重建表后重跑迁移）。
- i18n：删掉 5 条随之作废的文案（`SSH 终端` / `数据库` / `项目管理` / `AI 助手` / `功能正在建设中`
  与「紧凑行高」两条），新增 4 条（卸载确认弹窗与开关行）；`cargo test -p duck-theme` 的表序校验通过。
- `cargo check` / `clippy`：涉及文件零新增告警；`fmt --check` 干净。

**涉及文件**：`duck-store/src/db.rs`、`duck-tools/src/registry.rs`（P2-5 共用）、
`duck-app/src/{market.rs,settings.rs,pages/toolbox.rs,pages/settings.rs}`、
`duck-shell/src/view.rs`、`duck-theme/src/i18n.rs`。

**估工**：1.5 人日（**已完成**）

---

### P2-4 占位路由处置（**已实施 → 2026-10-11 晚按用户要求回退恢复**）

**原现状（证据）**：6 条路由中 4 条（`ssh` / `database` / `projects` / `assistant`）是
`pages/placeholder.rs` 占位页（`PlaceholderPage::new(title, icon)`），**长期空挂**，给了入口预期却无功能。

**第一轮实施（选了「A：收起」，但收得更彻底）**
把 4 条占位路由**从注册表里整体移除**，而不是「登记了但隐藏」：
- `app.rs::build_main_window` 只登记**已实现的两条**路由（概览 / 工具箱）；
- 4 条 `route.<id>` 动作（含 `⌘3`–`⌘6` 键位、原生菜单项、命令面板条目）一并删除 ——
  动作清单一处登记四处输出面同时出现，留着就是继续承诺一个点开是空页的功能；
- `pages/placeholder.rs` 整个文件删除；`SSH_ROUTE` / `DATABASE_ROUTE` / `PROJECTS_ROUTE` /
  `ASSISTANT_ROUTE` 四个常量删除；
- `settings::KNOWN_ROUTES` 从 6 条收缩到 **2 条** → 旧存档里的 `ssh` 等 id 自动变成「不认识」，
  静默回落概览（复用 P1-2 已有的回落机制，没有新增分支）。

**为什么不选「隐藏」（`with_visible(false)`）**：登记了却没有任何路径能到达的页面，
与 P2-3 要清的死代码是同一类东西 —— 只是死得更安静。路由是**一行 `ShellRoute::new`** 的成本，
等哪个模块真落地了再登记即可，不必现在就留四个不可达的桩。相应地，
`ShellRoute` / `ShellAction` 上那个从没被置过 `false` 的 `visible` 字段也一并删除（见 P2-3）。

**第一轮验证**：全仓 `cargo test --workspace` 通过；`cargo check -p duck-app` 零告警
（顺带消掉 `SharedString` / `tr` 等三处因删路由而失效的引用）；`⌘3`–`⌘6` 不再是键位。

**回退（2026-10-11 晚，用户要求恢复入口）**
用户核查截图发现活动栏只剩两个图标后明确要求恢复。四条占位路由**原样恢复**：
`pages/placeholder.rs` 从 git 游离对象中找回原文；4 条 `ShellRoute` / `route.*` 动作 /
`⌘3`–`⌘6` 键位 / 菜单与面板条目 / 路由常量 / `KNOWN_ROUTES`（回到 6 条）与 5 条 i18n 译条
（`SSH 终端` / `数据库` / `项目管理` / `AI 助手` / `功能正在建设中`）全部复原。
此决策与本章原「收起」结论相反：**入口预期优先于「空页伤信任」**，属产品取舍由用户定。
另修复第一轮遗留：删除时把 `build_main_window` 的文档注释误挂到了
`observe_system_appearance` 头上，本轮已归位并改写。

**回退验证**：`duck-theme` 15 测全绿（`table_is_sorted` + `every_chinese_literal_is_translated`）；
`duck-app` 86 单测 + 5 组集成全绿；clippy 本轮文件零新增；6 文件 `rustfmt --check` 干净；
隔离数据目录实机取证：活动栏 6 个图标齐全，会话恢复 `route="ssh"` 直接落到 SSH 占位页，
页面正常渲染（标题 + 建设中说明），无 panic。

**涉及文件**：`duck-app/src/{app.rs,actions.rs,settings.rs,pages/mod.rs}`、
`pages/placeholder.rs`（恢复）、`duck-theme/src/i18n.rs`。

**估工**：0.5 人日（第一轮）+ 0.5 人日（回退恢复）

---

### P2-5 概览页真实化（**已实施**）

**原现状（证据）**：`pages/overview.rs:110/116/122` 三张指标卡是**硬编码** `"24"` / `"1,284"` / `"820ms"`，
连变化说明（`较上周 +3` / `+12.4%` / `-6.1%`）都是编的；`:153-159`「新建工具」只弹一句
「已创建示例工具「未命名」」；`:167`「查看文档」**没有处理器**；「运行环境」三行的值
（`100%` / `跟随系统` / `47 个`）同样是写死的。

**实施结果**
1. **三张卡接真实数据，且读数口径写进注释**
   - **已安装工具** = `ToolRegistry::visible(cx).len()`（即用户真能看见的那些），
     说明行 = `分布在 {} 个分类`（`ToolRegistry::present_categories`）。
   - **累计执行** = 各工具 `tool_settings::runs` 之和。
     说明行 = `近 7 天用过 N 个` / `7 天内还没用过任何工具`。
   - **平均启动耗时** = 新增的启动耗时样本（见下）的平均值；
     **没有样本时显示 `—` + 「本次启动后开始记录」**，绝不摆一个编出来的毫秒数。
2. **新增「启动耗时」实测埋点**（P2-5 原方案里那句「依赖 P0-1 的会话计时埋点」实际并不存在，
   本项自己补上）：`main()` 入口起一个 `Instant`，`app::open_main_window` 之后取值，
   写进 `kv: startup_durations_ms`（JSON 数组，保留最近 10 次，**只写不读别的用途所以不去抖**），
   同时落一条 `millis=… main window ready` 的 info 日志。
3. **运行环境三行改读当前状态**：界面缩放取 `appearance.ui_scale`（`{}%`）、
   主题取 `appearance.preference`（跟随系统 / 浅色 / 深色），
   「跟随系统」时把**当前实际落到的模式**也说出来；图标集取 `DuckIcon::ALL.len()`。
4. **「新建工具」删除**：没有任何创建流程（工具来自插件），那个按钮点了只弹一句假装成功的提示。
   留下「导入配置」（真入口，P0-3 起就接到设置 → 数据）与「查看文档」（新接线）。
5. **「查看文档」接线**：`duck_platform::open_url(DOCS_URL)`，并把 `open_url` 的签名改成
   `-> std::io::Result<()>` —— **它原来是 `()` 且把 `spawn` 的错误丢掉了**，静默失败和假按钮是一回事。
   现在打不开就在窗口上弹一条错误 toast。

**实施中的取舍**
- **`open_url` 只报「拉不起来」那一半**：`open` / `xdg-open` 对「没有程序认这个链接」本身就是静默的，
  我们不假装比它们知道得更多。
- **子进程在后台线程 `wait` 回收**：原来 `spawn` 完不管，在 Unix 上点一次链接留一个僵尸进程；
  改成原地 `wait` 又可能在某些 `xdg-open` 实现上阻塞 UI 线程，所以两者都避开。
- **「近 7 天用过」数的是工具数、不是次数**：`recent` 表主键是 `(kind, id)`，每工具只留最后一次
  时间戳（`recent_since` 的文档注释里写明了）。要真正的次数得另建明细表，为一个概览指标不值得 ——
  所以卡片文案就写「用过 N 个」，不冒充「执行 N 次」。
- **启动耗时的口径是「进程启动 → 主窗就绪」**，不是「工具执行耗时」：卡片标签相应改成
  「平均启动耗时」，避免用编得出的口径硬撑原来的「平均耗时」。

**验证**：`duck-store` 新增 `recent_since`（按时间窗读最近项）；`duck-tools` 新增
`recent_within_days`；`duck-platform` 的 `open_url` 改签名（**原先零调用方**，改动无波及）。
i18n +11 条。`cargo check` / `clippy` 零新增告警。
**需人工验收**：概览页三张卡的数字与设置 → 数据页的记录是否对得上；点「查看文档」是否真打开 README。

**已知限制**：启动耗时样本从本版起采集，**首次启动前没有历史数据**（这正是显示 `—` 的原因）。

**涉及文件**：`duck-app/src/pages/overview.rs`、`duck-app/src/settings.rs`（启动耗时读写）、
`duck-app/src/main.rs`（埋点）、`duck-platform/src/shell.rs`（`open_url`）、
`duck-store/src/lib.rs`、`duck-tools/src/registry.rs`、`duck-theme/src/i18n.rs`。

**估工**：1.5 人日（**已完成**）

---

### P2-6 通知体系完善（**已实施**）

**原现状（复核后与原方案有出入）**：`duck_components::notify` 当时已有
`info/success/warning/error` + `sticky`（按 key 替换）+ `system`（`NotificationDelivery::System`），
**通知历史也已经有了**——App 级环形 100 条 + 顶栏铃铛面板 + 未读角标 + 清空
（`duck-shell/src/notifications.rs`）。原方案里写的「无通知历史」是错的。

真正缺的是**消费方**：设置页那个「通知」分组只有一个 `option_row_stacked`，里面是一句写死的
说明（`「应用内」是窗口右上角提示；「系统通知」投递到 macOS 通知中心（需已授权）。`）加三个
演示按钮，**一个开关都没有**——用户关不掉任何东西，也就谈不上「设置项消费」。

**实施结果**

1. **两条通道、三个档位**（`duck_components::notify`）
   - 应用内横幅：`BannerLevel{ All / WarningsAndErrors / Off }`，`kv: notify_banners`；
   - 系统通知：一个开关，`kv: notify_system`；
   - 五处投递入口（`info` / `success` / `warning` / `error` / `sticky`）**共用一处判据**
     `push_banner` —— 五处各写一遍 `if` 迟早会漏一处，而漏掉的那处就是「明明关了还在弹」。
2. **通道从三条收成两条：没有「声音」这一条。** 原方案写的是「声音 / 横幅 / 系统通知三通道」，
   读 `gpui-component-0.7.1/src/notification.rs` 确认**它没有任何声音 API**：
   `Notification` 上只有 `message/info/success/warning/error/id/title/icon/with_type/placement/
   delivery/system/in_app_and_system/autohide/on_click/on_close/action/dismiss/content`。
   要么为此自己接一个音频后端，要么如实把通道收成两条——选了后者，并把理由写进模块文档，
   免得下次又有人来「补上这条」。
3. **设置页通知页重写**：两条通道各占一行（说明就在各自控件旁边）、
   「试一下」三个按钮（应用内 / 系统 / 两者同时）、通知历史一行
   （`{} 条 · {} 条未读` + 清空）。历史**列表**仍只留在顶栏铃铛：
   两处各渲一份就得维护两条时间格式 / 空态 / 滚动路径，而它们没有任何理由不一样。
4. **启动接线**（`main.rs`）：`settings::apply_notify_channels(cx)` 排在
   `load_appearance` 之后、`install` / `tray` / `hotkey` / `open_main_window` **之前**——
   后面几步会产生通知（保留清理、快捷键登记失败、崩溃提示、更新检查），
   不先投影一遍就会按默认档全弹，用户明明关过。

**实施中发现并修掉的两个真缺陷**（本次最值钱的产出，都不是「加个开关」能覆盖的）

1. **改档位不生效**：初版 setter 是「`kv_put` → `apply_notify_channels`」，而后者从 `kv` 回读。
   `kv_put` 是**去抖落盘**的（400ms），回读拿到的是旧值——于是运行时档位压根没变，
   设置页下一次重绘又弹回旧档，用户看到的就是「点了没反应」。
   改判：运行时那份从 `notify::channels`（App 级全局，当前生效值）出发只改这一位，
   `kv` 只负责让它活过重启；设置页也改读运行时值，两个「快照」字段随之删除。
   回归测试 `changing_the_banner_level_takes_effect_without_the_store`（**连 `Store` 都不装**）。
2. **「两者同时」根本没投系统那条**：`in_app()` 把标题与正文都渲进 `content` 闭包
   （因为库的 `type_` 图标定位按 16px rem 硬编码，而本项目 rem = 13px），
   而库的 `push_system` 取的是 `notification.title` / `notification.message`，
   两者都是 `None` → 撞上它内部那句 `(None, None) => return`，**静默不发**。
   也不能简单补 `.title()/.message()`：库的渲染会把 `title`、`message`、`content`
   **依次**塞进同一个 `v_flex`，标题与正文会在 toast 里出现两遍。
   改判：`system_with_toast` **拆成两条通知**推——系统那条只有文本（因此不建 toast），
   应用内那条显式 `delivery(InApp)` 且只有自绘内容（因此系统通道取不到文本、直接不发）。
   回归测试 `both_channels_on_deliver_to_both`。

**实施中的取舍**

- **三档而不是一个开关**：「全部静音」与「只想少看点成功提示」是两种需求——前者是明确的取舍
  （连错误也不弹），后者才是大多数人的偏好。
- **无论哪一档，历史照记**：这是关掉横幅之后仍然看得到结论的**唯一**途径，
  `a_suppressed_toast_is_still_recorded_in_history` 把它钉死。
- **两条通道各看各的开关**：只关系统就只弹横幅，只关横幅就只投系统，都关则只剩历史；
  任意组合下都不会「因为另一条关着而把这条也吞了」。
- **历史不落盘**：正文是用户数据（可能含路径），漏进 JSON 文件不是好交换；
  而「回看五秒前那条 toast」是**会话内**需求，重启后看不看得到不在预期里。
- **「试一下」那一行特意不写说明**：上面两行的说明已经说清两条通道各是什么、去哪看，
  再重复一遍就是噪音。

**顺带清掉的两处**

- 一条孤儿译条（`「应用内」是窗口右上角提示；「系统通知」投递到 macOS 通知中心（需已授权）。`）
  ——它的唯一使用方就是被重写的那个分组。同时顺手修掉「macOS 通知中心」这个**跨平台说法**：
  本项目有 Windows 构建，`main.rs` 里那句 `set_app_identity` 就是为系统通知准备的。
- 演示按钮 id 从 `duck-settings-notify-system` 改成 `duck-settings-notify-system-demo`：
  原 id 现在归「系统通知」那个开关，同窗口两个同 id 的控件会串。

**验证**

- `duck-components` 单测 +3：三档 `wire` 往返（含大小写敏感与未知值回落）、
  中间档只留警告与错误、缺省全开。
- `duck-app` 新增 `tests/notify_channels.rs` **7 条无头行为测试**（真的建一个挂着 `Root` 的窗口、
  真的推、再数 `WindowExt::notifications()` 与 `TestAppContext::shown_system_notifications()`）：
  关掉后四条全拦 / 放开后四条全回（对照组）、中间档只放警告与错误、被拦下的仍进历史、
  四种通道组合（两者都开 / 只开横幅 / 只开系统 / 系统单通道关闭）。
  > 测试里那句 `cx.set_app_identity(..)` **不能省**：测试平台会像 Windows 一样，
  > 在拿到 AppUserModelID 之前直接丢掉系统通知——不设身份，`shown_system_notifications()`
  > 永远是空的，断言会以「功能坏了」的样子失败。
- `duck-app` `src/settings.rs` +2 条：不装 `Store` 也立刻生效、两条通道互不牵连。
- i18n +9 条（另删 1 条孤儿）→ 581 条。
- `cargo fmt --check` 干净；`clippy` **零新增告警**（全仓 11 条均为既有）；
  全仓 36 个测试目标全绿。

**需人工验收**：三档下拉改动是否立即生效（选「全部关掉」后点「应用内提示」应什么都不弹，
但顶栏铃铛里多一条）；系统通知开关在打包后的应用里是否真投；
三个演示按钮的通知落在**哪个窗口**（通知投当前活跃窗口，这是跨窗口投递的既有语义）。

**涉及文件**：`duck-components/src/notify.rs`、`duck-app/src/settings.rs`（kv 读写 + 启动投影）、
`duck-app/src/main.rs`（接线）、`duck-app/src/pages/settings.rs`（通知页）、
`duck-app/tests/notify_channels.rs`（新增）、`duck-theme/src/i18n.rs`。

**估工**：2.0 人日（**已完成**）

---

### P2-7 可访问性 — ✅ **已完成**（1 条守卫挂起，见「剩余项」）

**原状**：仅零星 `accessibility_label`（`duck-components/src/lib.rs` 附近），
**无纯键盘可达性测试、无对比度守卫**。更根本的是：**GPUI 的 a11y 树只收录带 `role`
的节点**，所以仓库里每个自绘 `on_click` div 对读屏**等于不存在** —— 成本大头在这里，
不在缺 label。

**落地**

1. **role / 名字 / 选中态**（全部由生产代码直接表达）
   - `activity_button` 改为**角色由调用方注入**：活动栏上半是路由（`Role::Tab`，配容器
     `TabList`），下半是命令（`Role::Button`，配容器 `Toolbar`）；补 `aria_selected` + 显式名字。
   - `navigation_row`（侧栏 / 活动栏 / 设置分类 / `nav_list` 共用）补 `Role::Tab` +
     `aria_selected`；`segmented_tabs` 与 `nav_list` 的容器补 `Role::TabList`。
   - 设置页与主题选择器的三组「多选一」：`Role::RadioButton` + `aria_toggled` + 容器 `RadioGroup`。
   - 设置页工具列行、主题卡、图标包卡、预设 / 自定义色点、快捷键录入框、日志过滤框逐项补 role / 名字。
   - 名字一律**复用既有可见 / i18n 文案**，未新增待翻译串。
2. **键盘可达性**：补 `focusable` + `tab_stop(true)` + **焦点描边**（先占 1px 透明边，
   聚焦只换色、不引起位移）。焦点色统一取 `ring`；**仅**当自身边框的选中态已等于 `ring`
   时改用 `colors.foreground`（否则焦点环与选中态同色 = 看起来没反应）。
3. **无头测试**
   - `crates/duck-components/tests/accessibility.rs`：3 条 —— 控件可达、
     Tab / Shift-Tab **真键位**驱动焦点、禁用项不进 Tab 序。
   - `crates/duck-theme/tests/accessibility_contrast.rs`：3 条对比度守卫 ——
     `accent_text` ≥ 4.5:1、`nav_selected_icon` ≥ 3:1（都绿，42 预设 × 2 强调色），
     焦点环 ≥ 3:1（**实测不成立，`#[ignore]` 存档**，见剩余项）。

**关键结论（其中两条改变了实施方式）**

- ⚠️ **`gpui_base::test_support` 在本工作区不可用**。开 `gpui-base` 的 `test-support`
  会把 `.test_support()` 的返回类型换成 `Observed<E>`；而 `gpui-component 0.7.1` 在
  `menu/popup_menu.rs:1469` 调用过它之后，仍继续链式调用 `ScrollableElement::vertical_scrollbar`
  （同文件 `:1508`）→ **整条依赖链 `E0599` 编译失败**。`gpui-component` 自己没开这个
  feature，所以上游从未走过这条路径。
- ⚠️ **`Window::debug_a11y_tree_json()` 在无头环境同样不可用**。它要求 a11y 适配器**已激活**
  （`window/a11y.rs:220`：`is_active() == is_enabled() && active_flag`），而该标志只由平台
  适配器的 activation 回调置位；`TestPlatform`（`platform/test/platform.rs`）与
  `VisualTestPlatform`（`platform/visual_test.rs`）都**没实现 `a11y_init`**，落到
  `platform.rs:1351` 的空实现 → 树永不采集，永远返回 `None`。
  → **role / 名字 / 选中态在本仓库无法自动化断言**，只能由生产代码表达 + 代码审查覆盖。
- ✅ **Tab 焦点遍历确实是通的**：`gpui_component::Root` 就是 `gpui_base::Root`
  （`gpui-component/src/lib.rs:97`），其 `#root` 挂 `key_context("Root")`；而
  `gpui_component::init` 会调 `gpui_base::init`，把 `tab` / `shift-tab` 绑到
  `window.focus_next` / `focus_prev`（`gpui-base/src/root.rs:14-22, 194-227`）。
  这条为前提不成立时（换窗口根 / 不调 init），新测试会立刻变红。
- ✅ `gpui_base::Button` 默认 `tab_stop: true` + `focusable: true`（`button.rs:41, 53-54`），
  构造时即 `.test_support()`；且 `Button::accessibility_label()` 内部就是 `.aria_label()`
  （`button.rs:232`），所以「名字」与「读屏真正拿到的东西」是同一个字段。

**剩余项**

- **焦点环对比度**：实测 **80 / 420 组合低于 3:1**（深色主题占 75：`surface` 25 ·
  `nav_selected` 20 · `muted` 19 · `chrome` 8 · `background` 3；浅色 5；最差 ≈ 2.10:1）。
  根因：`base_tokens` 里 `ring` 继承 `primary`，而 `primary` 的收敛对象是
  `primary_foreground`（按钮上的文字），**不是焦点环真正落上去的底**。修法（未采用，属产品取舍）
  见 `duck-theme/tests/accessibility_contrast.rs` 的 `#[ignore]` 说明。
- `content/nav.rs` 的 `tab_bar`：上游 `TabBar` 的标签只报 `Role::Button`、**永不报
  `aria_selected`**，且无注入缝隙，需自绘标签行才能补；该包装器目前**零调用方**，
  已在其文档注释里记录。

**涉及**：`duck-components/src/{lib,kit}.rs`、`duck-shell/src/view.rs`、
`duck-app/src/theme_picker.rs`、`duck-app/src/pages/{settings,logs}.rs`、
`duck-components/tests/`（新）、`duck-theme/tests/`（新）。**估工 3.0 人日。**

---

### P2-8 插件能力授权细化（子集授权）— ✅ **已完成**

**原现状（证据）**：`duck-plugin/src/capability.rs` 的 `CapabilityGranter::grant_manifest` 自带 **TODO**：
**安装时勾选的子集授权尚未实现**，安装确认框目前只是"展示"（声明即授予）。

**方案**
1. 安装确认框支持**勾选能力子集** → 用户授予结果落 `kv: plugin_caps::<id>`；
2. 运行时仲裁改为「manifest 声明 ∩ 用户授予」，未授予的能力调用被拒并给出可读原因；
3. 设置页「插件」支持**事后调整授权**；
4. 与插件市场索引的 `capabilities` 关系保持：索引是**授权上限**，用户授予只能 ≤ 上限。

**涉及**：`duck-plugin/src/capability.rs`、`duck-app/src/pages/plugin_center.rs`、`plugins.rs`、`i18n.rs`。**估工 2.5 人日。**

**实施结果（2026-10-11）**

四项全部落地，另附一处用户可见缺陷修复。落点：

| 环节 | 落点 | 关键点 |
| --- | --- | --- |
| 授予记录 | `duck-app/src/plugins.rs`：`kv: plugin_caps::<id>` | 存 `Capabilities` 本体（与清单 `[capabilities]` 同一 serde 形状），非自定义结构 |
| 运行时镜像 | `struct PluginGrants(HashMap<String, Capabilities>)`（`impl Global`） | **不读 kv**，见下 |
| 仲裁 | `resolve_grant(declared, record)` → `declared.intersect(&record)` | 一个纯函数，7 条单测钉死 |
| 安装确认框 | `plugin_center.rs`：`GrantSelection` + 逐条 `switch_control` | 每条能力一个开关，`unit_key` 区分 `read:`/`write:`/`net:`/`exec:` |
| 事后调整 | 详情页逐条开关（标题随状态在「能力授权」/「能力声明」间切换） | 每次切换**重读运行时全局**，不缓存快照 |
| 卸载回收 | `market.rs::finish_uninstall` → `forget_granted` | 与 `set_enabled(false)` 同处调用 |

**三处判断（都写下来，因为都不是显然的）**

1. **授予记录必须落 `Global`、不能读 `kv`。** `kv_put` 是 400ms 去抖写，紧接着 `kv_get` 拿到的是**旧值**
   —— 与 P2-6 通知档位踩的是同一个坑。所以运行时状态放 `PluginGrants` 全局，`kv` 只负责**跨重启持久化**；
   setter 与设置页一律「从全局出发改这一位」。`load_grants(cx)` 只在 `init()` 里调一次（且必须在
   `scan_dirs()` **之前**，否则首帧渲染时授予表还是空的）。
2. **「无记录」的缺省是「按声明全给」，不是「什么都不给」。** 老安装没有 `plugin_caps::<id>` 键，
   若按「无记录＝拒绝」处理，所有已装插件会在升级后一夜之间全部哑火，而界面上只显示「插件执行失败」。
   故 `resolve_grant(Some(declared), None) == declared`。反过来，更新已有插件时确认框的初始勾选
   取**现有授予**（不是全勾），否则用户此前收回过的权限会被静默重新授予。
3. **清单读不到时按记录走。** `resolve_grant(None, Some(record)) == record`——目录抖动一次
   （插件刚被删、索引还没刷新）不等于「权限全没」。

**顺带修掉的用户可见缺陷**：`duck-plugin-js` 的四处 `map_err` 直接用了 `Error::Exception` 的 `Display`，
而它对 rquickjs 只产出 `"Exception generated by QuickJS"` 这一句——**插件抛的真实原因全丢了**。
授权被拒时用户看到的只有「插件执行失败」，看不到「capability not granted: Notifications」，
也就无从知道该去授权。改为 `js_failure(&ctx, error)`：先 `ctx.catch()` 取 `Value`，
再 `.as_exception()` → `Exception::message()`，拿不到才回落 `Display`。
三个 `dev_plugins.rs` 断言（`granting_only_the_write_leaves_the_notification_denied` 等）
从失败转绿——它们是**先把消息丢了才失败的**，不是权限逻辑错。

**这一项是被一个编译错误逼出来的**：`duck_plugin_js::run_tool` 的签名已经加了第 4 个参数
`granted: &Capabilities`（由 `duck-plugin-js` 侧先落地），而 `duck-app` 的调用点没跟——
`error[E0061]: this function takes 6 arguments but 5 arguments were supplied`，
整个应用**构建不过**，也就是用户报的「启动报错」。即：**这项迁移做了一半**，
补上 app 侧那一半（运行时授予表 + 确认框勾选 + 事后调整 + 卸载回收）才闭合。

**验证**：新增 `plugins::tests` **7 条**（6 条权限语义 + 1 条「外来单元只砍自己不连坐」）；
`cargo test --workspace --locked` **37 个测试组 / 累计 469 条用例零失败**；六个改动文件
`rustfmt --check` 干净；clippy `duck-app` 9 条告警**全部为存量**（`plugins.rs` 的 `map_or` 属
`tool_enabled`，逐字核对非本次改动）；启动冒烟 `./target/debug/duck` 存活 8s、
日志 `main window opened route="overview"` + `main window ready millis=314`（无 panic、无权限相关报错）。

---

### P2-9 插件 JS 网络能力落地 — ✅ **已完成**（第 3 条凭据注入见「移交」）

**原现状（证据）**：`duck-plugin-js/src/lib.rs:153-211` —— `NetworkHost` 能声明、能仲裁，
但**无任何调用点**（宿主 API 只有 `duck.{id,dataDir,notify,readClipboard,writeClipboard,readFile,writeFile}` + `console.*`），
**没有网络 host 函数**，属空转。

**方案**（安全敏感，须在 P2-8 之后做）
1. 新增 `duck.fetch`：受能力网关（`network` 能力）+ 域名白名单（manifest 声明 + 用户授予）双重约束；
2. 硬限制：超时、响应大小上限、**默认不跟随跨域重定向**、禁止访问非 http(s)；
3. 敏感值（token）从 P0-2 的加密凭据注入，**不允许脚本自行持久化密钥**；
4. 请求日志可审计（写入插件专属日志文件）。

**涉及**：`duck-plugin-js/src/**`、`duck-plugin/src/capability.rs`、`duck-platform/src/secret.rs`（P0-2）、文档。**估工 4.0 人日。**

**实施结果（2026-10-11）**

**零新依赖**：`ureq 3.4.2` 本就在依赖图里（`duck-update` / `duck-plugin-registry` 同款配置），
lock 里只出现一次，本次只是把它提成 `duck-plugin-js` 的直接依赖。

| 环节 | 落点 | 关键点 |
| --- | --- | --- |
| 主机白名单仲裁 | `duck-plugin/src/capability.rs`：`require_host(host, port)` | 与 `require_path_read/write` 对称；条目语义 `host`（任意端口）/ `host:port` |
| 出网原语 | `duck-plugin-js/src/net.rs`（新） | `parse_target`（协议策略）+ `NetBudget`（跨请求预算）+ 逐跳重定向 |
| JS 宿主 | `duck-plugin-js/src/lib.rs`：`duck.fetch(url, options?)` | 返回 `{status, ok, headers, body}`；method 白名单；保留头禁设 |
| 审计 | `tracing` target `duck::plugin::net` | **只记** method / host / port / status / 字节数 |
| 端到端守卫 | `duck-plugin-js/tests/network.rs`（新，8 条） | 起真实回环服务，不是 mock |

**三处「必须这样写」的地方**（都不是显然的，写下来免得后人踩回去）

1. **超时不能依赖 QuickJS 的 10 秒上限。** `run_tool` 用 `set_interrupt_handler` 实现执行上限，
   而 interrupt **只在字节码之间**触发——Rust 侧一旦进入阻塞的 socket 调用，引擎拿不回控制权，
   10 秒上限形同不存在。所以出网自带 `NetBudget`（总时长 + 请求条数），**且跨请求共享**：
   不共享的话，插件连发 6 个请求就能把 10 秒撑成 48 秒，卡住的是宿主的后台执行线程。
   预算与中断处理器**同源**（同一个 `deadline`），`duck-plugin-js/src/lib.rs` 里先算它再传给两边。
2. **重定向必须自己逐跳走。** `ureq` 跟随重定向是「跟完才把结果给你」，那时字节已经从任意主机
   出去了，白名单只在第一跳有效。故配置 `max_redirects(0)` + `max_redirects_will_error(false)`
   把 3xx 原样交回来，由 `net::fetch` 逐跳校验。**副作用是好的：「跨域重定向」不需要单独写规则**
   ——每一跳都重过一遍 `require_host`，新 host 本来就不在授予里。重定向断链上限 5 跳。
3. **明文 http 只对回环开口子。** 原方案写「禁止非 http(s)」，实际做得更紧：https 全放行，
   http 仅回环（`127.0.0.0/8` / `::1` / 字面量 `localhost`）。理由有两条：插件对接本机服务是
   真实需求；以及 **CI 只能靠回环起真服务**——不放这个口子，重定向与超限就只能测纯函数，测不到
   「闸门是否真的接在发请求之前」。判定刻意**不用** `ureq` 的 `https_only`（它是全局开关，没有例外档）。

**另外两条判定**：响应体上限 1 MiB，**先看 `Content-Length` 提前拒**（等读满 1 MiB 再报错等于白下
1 MiB；对 100 MB 的文件那 1 MiB 也算钱），再以 `limit()` 兜没有声明长度的分块响应；4xx/5xx
通过 `http_status_as_error(false)` 当正常响应交给插件（401/404 是它的业务结果，不是宿主的错误）。

**审计只记方法与目的地**，绝不记 header 与 body —— 下一项的凭据就挂在 header 上，
记了等于把 token 抄进日志文件，而日志是要被导出、被贴进 issue 的。

**验证**：新增 **21 条**用例（`capability` 6 条 host 匹配 / `net` 7 条策略与预算 /
`tests/network.rs` 8 条真实 socket 端到端）。`cargo test --workspace --locked`
**39 个测试组、累计 499 条用例，连跑三轮全绿**；四个改动文件 `rustfmt --check` 干净；
clippy 对 `duck-plugin` / `duck-plugin-js` **零告警**。

**移交（不在本项内）**：原方案第 3 条「凭据注入」**未做**，因为它要求先把
`duck-platform/src/secret.rs` 接上界面（该层至今零调用方，展示位未接）。本项交付的是**网络通路
本身**；`duck.secret(name)` 与插件详情页的凭据输入，连同 `secret.rs` 的首次接线，留给独立一项。
在那之前，`duck.fetch` 可用于无需鉴权的公开接口。

---

## 6. P3 — 长期与生态

### P3-1 插件签名与市场治理
**现状（记忆确认，P3 未做）**：无 Ed25519 签名、无评分 / 下载量、无插件依赖声明。
**方案**：复用 P1-3 的签名基建给插件包签名（公钥内建于客户端）；
索引增 `rating` / `downloads`（需服务端侧计数，静态索引可先只做展示位）；
`plugin.toml` 增可选 `dependencies` 并做安装前解析 + 循环依赖检测。
**涉及**：`duck-plugin-registry`、`sbin/plugin_index.py`、`release` 侧、`plugin_center.rs`。**估工 6.0 人日。**

### P3-2 i18n 深化
**现状**：只有 zh-Hans / En / 跟随系统；无复数、日期、数字、货币、相对时间本地化（`i18n.rs:14-46`）。
**方案**：引入轻量格式化层（复选 / 序数 / 相对时间 / 数字分组）；`trf` 扩展为支持 `{count}` 复数变体；
日期时间统一走格式化层，禁止手拼字符串。
**涉及**：`duck-theme/src/i18n.rs`、全仓 `tr/trf` 调用点。**估工 3.0 人日。**

### P3-3 大文件拆分与架构治理
**现状（证据）**：`pages/settings.rs` **2321 行**、`pages/plugin_center.rs` **1632 行**、`pages/toolbox.rs` **1441 行**。
**方案**：按「**页面壳 / 数据快照 / 控件构造**」三块拆分；先拆 `settings.rs`（含 8 个页），
再拆 `plugin_center.rs`、`toolbox.rs`。拆分期间**禁止混入功能改动**（AGENTS 3.1 不顺手重构的例外需先确认）。
**涉及**：`duck-app/src/pages/**`。**估工 4.0 人日。**

### P3-4 多平台补齐
**现状**：`release.yml` build 矩阵为 **macos-arm64 / macos-x64 / windows-x64**，**无 Linux**。
**方案**：补 Linux 构建（deb / AppImage）+ Windows 体验对齐（通知 / 单实例 / 自启已具备）；
`package.sh` 无 notarize 步骤，若走签名分发需补 `notarytool`。
**涉及**：`.github/workflows/release.yml`、`sbin/package.sh`、平台层条件编译。**估工 3.0 人日。**

### P3-5 主题深化 + 可选遥测
**方案**：自定义主题的**导入 / 导出**（JSON，与 `duck-theme` 的 `ResolvedTheme::build(builtin, accent, scale, font_size)` 对齐）；
可选匿名统计（**默认关闭**、明确告知、本地可查看已发送内容）。
**涉及**：`duck-theme`、`pages/settings.rs`（外观页）、`i18n.rs`。**估工 3.0 人日。**

### P3-6 消灭「演示项」：图标不许是死按钮（**2026-10-12 已实施**）

**触发**：用户明确「界面不应再有演示数据 / 演示文字，**每个图标的功能都需要实现完整**」。
这一条**推翻了 P2-3 的保留口径** —— 当时认为「自我标注为演示」属诚实项，可以留；用户不认这个理由。

**处置判据（二选一）**：
1. **前提在后端存在 → 接线**；
2. **前提在后端不存在 → 删除**（一个点下去不会有结果的按钮，比没有它更糟）。

**逐条处置**
| 位置 | 原状 | 判据 | 处置 |
| --- | --- | --- | --- |
| `actions.rs` `TOOLBOX_REFRESH`（`⌘R` / 菜单「工具」/ 命令面板 / 活动栏，`RotateCw` 图标） | 只弹「演示：刷新（未接入工具后端）」 | 前提**存在**：`plugins::rescan` 早已就绪（安装/卸载插件后调用的就是它） | **接线**：调 `plugins::rescan`，提示真实结果（「已重新扫描插件目录，当前列出 {} 个工具。」） |
| `toolbox.rs` 状态栏左：`↓ 0 B/s` / `↑ 0 B/s` | 恒为 0 的假读数 | 无下载引擎 | **删除** |
| `toolbox.rs` 状态栏：全部暂停 / 全部开始 | 弹「演示：…（未接入任务后端）」 | 本应用**没有全局任务队列**（工具箱是工具页签，不是下载任务队列 —— 与 P2-3 删 `compact_rows` 同一理由） | **删除**（含 `render_status_action`） |
| `toolbox.rs` 状态栏：完成后关机（`Power` 图标 + 预设下拉） | 弹「演示：…（未接入宿主电源能力）」 | 「完成后」的前提（任务队列）不存在；独立定时关机是另一个功能，不在本次范围 | **删除**（含 `render_shutdown_control` / `SHUTDOWN_PRESETS_MIN`） |
| `toolbox.rs` 状态栏：剩余空间 | 真实（`statfs`） | — | **保留**，并补上插件 / 工具**真实计数** |
| `pages/terminal.rs` 模块头 | 仍写着「TabKind::Ssh / Sftp 仍是演示数据」 | 10-12 已接真实 `ssh` / `scp` | **改写注释**（代码早已真实，注释过期） |

**状态栏现在的内容**：左 = 「{} 个插件 · {} 个工具」（读注册表 —— 与侧栏同一份真源），右 = 家目录所在卷剩余空间。
**两样都取不到时整条不渲染**（`render_status_bar` 返回 `Option<AnyElement>`），不留一条空栏占位。

**验证**：`cargo check -p duck-app` 中 `toolbox.rs` / `actions.rs` **零错误零告警**；
`cargo fmt -p duck-theme -- --check` 下 `i18n.rs` 干净；`duck-theme` 的 `table_is_sorted` 通过。
i18n：新增 2 条（`{} 个插件 · {} 个工具` / `已重新扫描插件目录，当前列出 {} 个工具。`），
删除 13 条随本次改动失效的条目（5 条「演示：…」+ 3 条上一轮终端页遗留 + 5 条状态栏控件文案）。

⚠️ **顺带发现（当天稍后已由并发会话补齐）**：并发会话新建的 `crates/duck-app/src/pages/assistant.rs` 与
`src/ai.rs` 当时有 **50 条中文尚无英译**，`duck-theme` 的 `every_chinese_literal_is_translated` 因此是红的。
2026-10-12 14:30 复核：该守卫**已全绿**（P3-7 一轮的 `cargo test -p duck-theme` 通过）。

**涉及文件**：`duck-app/src/actions.rs`、`duck-app/src/pages/toolbox.rs`、`duck-app/src/pages/terminal.rs`、
`duck-theme/src/i18n.rs`、本文档。**估工 0.5 人日（已完成）**

---

### P3-7 终端分屏落地 + 两处「图标看着在、点下去没用」（**2026-10-12 已实施**）

**触发**：用户实测三条 ——「分屏图标和功能没有实现」/「SFTP 窗口关掉后再点图标就无法显示」/「页面的图标都没有 tooltips」。

| # | 现象 | 根因 | 处置 |
| --- | --- | --- | --- |
| 1 | 工具条没有分屏图标（**上一轮**按「不摆点不动的图标」把它撤了） | 真分屏要动会话表 / 焦点 / 尺寸 | **实现**：`SessionTab.split` = 右窗格会话键（`<id>#2`），`ensure_sessions` 为它独立起会话（Local 新 shell / SSH 复用 `ControlMaster` 再开一条 `ssh -t`），左右各一个画布 + 1px 发丝线；退出只收那一半；`⌘D`（`SplitPane` 动作）与工具条图标同一条路径。设计见 `terminal-ssh-sftp-design.md` §8.6 |
| 2 | SFTP 面板关掉后再点图标没反应 | 目标**只从当前标签推导**：面板关掉后活动标签回落到本地 shell（于是 `connection_entry("local-shell")` 给出内置条目 → 开出一个永远列举不出东西的假面板）或空态（`active_tab()` 为 `None` → 按钮置灰） | **修**：`last_sftp_owner` + 纯函数 `resolve_sftp_owner`（当前标签所属连接 → 上次开过 SFTP 的连接 → 都没有才置灰，并把原因写进按钮文案）；`open_sftp_tab` 加守卫；标签右键菜单的「打开 SFTP 面板」只对 SSH 标签出现 |
| 3 | 页面图标没有悬停提示 | 图标按钮只设了 `accessibility_label`（读屏能吃、鼠标看不见） | **修**：`duck-components` 的 `icon_button` / `toolbar_action_button` 两个构造器统一挂 `Tooltip`（文案与读屏标签同源），一处收口覆盖全仓约 50 个图标按钮；disabled 分支同样挂（gpui-base 的 `Button::disabled` 保留命中区，只拦点击） |

**验证**：`cargo test -p duck-app` **113 条单测全绿**（含本项新增 3 条：`sftp_target_survives_closing_the_panel` /
`sftp_target_refuses_local_shell_and_missing_profiles` / `split_key_never_collides_with_sftp_or_tab_ids`）；
`duck-theme` 21 + 3 全绿（i18n 守卫含新增 6 条）；改动文件 clippy **零告警**、`rustfmt --check` 干净。
i18n +6（分屏 / 取消分屏 / 分屏（仅限终端会话）/ 打开 SFTP 面板（需先连接远程主机）/ 本地 shell 没有远端文件可浏览… / 终端（分屏）/ 分屏 ⌘D…）。

**已知不能无头断言的**：tooltip（`window.rendered_frame` 是 `pub(crate)`，测试拿不到），与全仓既有 5 处 tooltip 调用点同构、靠代码审查；
分屏的真机表现（PTY 起两条会话、`⌘D` 手感、分隔线视觉）需 `sbin/run.sh` 后人工确认。

**涉及文件**：`duck-app/src/pages/{terminal,terminal_canvas,terminal_data}.rs`、`duck-app/src/{app,pages/mod}.rs`、
`duck-components/src/lib.rs`、`duck-theme/src/i18n.rs`、两份设计/差距文档。**估工 1 人日（已完成）**

---

### P3-8 SSH 连接报错 · 分屏支持不同连接 · 拖标签并入（**2026-10-12 已实施**）

**触发**：用户实测「无法连接」（附报错截图）+ 对分屏提两点要求 ——「不止同一个连接分屏，还支持不同连接分屏」「拖动 tab 超过中线时提示是否并入分屏，参考主流软件设计」。

| # | 现象 | 根因 | 处置 |
| --- | --- | --- | --- |
| 1 | 任何 SSH 连接都报 `command-line line 0: keyword controlpath extra arguments at end of line` | `ControlPath` 的值是 `~/Library/Application Support/DuckToolbox/ssh/<socket>` —— **路径含空格**；`ssh -o` 的值不再重新加引号解析，空格被当成「行尾多余参数」（真机 `ssh -G` 已复现：不带引号报同款错、带引号正常通过） | **修**：① `duck-ssh::ConnectionTarget::argv` 给 `ControlPath` 值加引号（通用防线，任何含空格路径都安全）；② 终端页 socket 目录从 `app_data_dir()` 改到 `cache_dir()`（`~/Library/Caches/DuckToolbox`，天然无空格）；③ 单测钉住「带空格目录 → 带引号 `ControlPath`」 |
| 2 | 分屏只能连同一个连接 | `split` 只存会话键，没有「连谁」的信息，`ensure_sessions` 一律按标签自己的连接起会话 | **实现**：`SessionTab.split_owner` 记右窗格连接归属，`ensure_sessions` 升级为**按归属起会话**；入口三处共用同一份数据 —— ① 工具条 / `⌘D`（缺省 = 标签自己的连接，`split_owner_for`）② 左栏连接右键菜单「在分屏中打开」（连点的那条）③ 拖标签并入。右窗格**类型**跟着归属走（`split_kind()`），左 SSH 右 Local 合法 |
| 3 | 分屏只能靠图标 / 快捷键，没有主流终端的拖拽手感 | 标签栏此前只挂 `on_click` / `context_menu`，没有拖拽载荷 | **实现**：`TabDrag` 载荷 + `TabDragGhost` 拖拽预览 + 画布 `.on_drag_move` / `.on_drop`；指针**过中线**点亮右半屏投放区（主题 `drag_border` 描边 + `drop_target` 底色，文案「松开并入分屏」），松手把该标签**整条会话搬过去**当右窗格；投放区渲染带 `cx.has_active_drag()` 守卫做拖放结束自愈 |

**验证**：`cargo test -p duck-app` **132 条单测全绿**（较上轮 +19，含新增分屏归属 / 拖拽并入相关单测）+ 集成测试全绿；
`duck-ssh` **18 条全绿**（含新增 `control_path_is_quoted_when_dir_has_spaces`）；`duck-theme` 20 绿 +
1 红（红的 `every_chinese_literal_is_translated` 缺译清单 **100% 来自并发会话新写的 `db.rs`**，非本项）；
改动文件 `rustfmt --check` 干净、clippy **零告警**（其余告警均在并发会话的文件上）。

**已知不能无头断言的**：拖拽（`on_drag` / `on_drop` / `on_drag_move` 无头驱动不易）、真机 SSH 连接、分屏与拖拽的观感，
均需 `sbin/run.sh` 后人工确认。

**涉及文件**：`duck-app/src/pages/{terminal,terminal_data,terminal_canvas}`、`duck-ssh/src/target.rs`、
`duck-theme/src/i18n.rs`、`docs/terminal-ssh-sftp-design.md`（§8.6 + §15）、本文档。**估工 1 人日（已完成）**

---

### P3-9 连接口令走加密落库 + 编辑回显（钥匙串退场）（**2026-10-12 已实施**）

**触发**：用户实测「保存口令后点连接**弹出 macOS 钥匙串授权框**（『系统密码器』）仍无法连接」+「编辑连接时已保存的口令需要**反显**，连接信息要保存在数据库」。

| # | 现象 | 根因 | 处置 |
| --- | --- | --- | --- |
| 1 | 点连接弹系统钥匙串授权框 | 口令存 keyring → macOS 钥匙串；dev 构建 ad-hoc 签名**每次重编译都变**，条目 ACL 失配 → 读一次弹一次，「始终允许」随签名失效 | **修**：口令改走新 `duck-platform::vault`（AES-256-GCM；ring 已在依赖图，零新增传递依赖），密钥落 `app_data_dir()/vault.key`（0600）；连接时本地解密，**全程零系统弹框** |
| 2 | 编辑连接看不到已保存的口令 | 旧口径「不回读、不回显，空 = 不修改」—— 是为绕开钥匙串弹框做的妥协 | **修**：编辑弹窗**解密回显**（掩码显示），改端口不用重输口令；空值语义改为「清除口令」 |
| 3 | 「连接信息要保存到数据库」 | 档案本就落 `kv: ssh_profiles`（`duck.sqlite3`）；口令却在钥匙串 / credentials.json | **修**：`StoredProfile.password` 存密文（hex(nonce‖ct)，`#[serde(default)]` 兼容旧档），SQLite 里 grep 不到明文（M2 验收线保持）；增删改随档案行原子生效 |

**验证**：`duck-platform` 101+2+1+3+2 全绿（含 vault 往返 / 篡改拒绝 / 密钥文件 0600 单测）；
`duck-theme` 21+3 全绿（i18n 表换 2 键、字节序守卫过）。`duck-app` 因并发会话的 `mssql.rs` 中间态暂阻，收尾后补跑。
**不做钥匙串迁移**（读旧条目必弹框）；旧条目（服务 `com.duck.toolbox`）留用户在「钥匙串访问」自删。

**涉及文件**：`duck-platform/src/vault.rs`（新）、`duck-platform/{Cargo.toml,lib.rs}`、根 `Cargo.toml`、
`duck-app/src/pages/{terminal,terminal_data,terminal_dialogs}.rs`、`duck-theme/src/i18n.rs`、`docs/terminal-ssh-sftp-design.md`（§7 + §15.4）、本文档。

---

### P3-10 全仓凭据与系统钥匙串解绑（**2026-10-12 已实施**）

**触发**：用户复查后定调 ——「AI API key 写进钥匙串这不对，不要和系统绑定关系，
要和程序的数据库和文件绑定；我发现还有其他的连接信息使用的钥匙串，请都修改」。

**处置**（全仓一次性清干净，不留残口径）：

| 凭据 | 旧落点 | 新落点 |
| --- | --- | --- |
| AI API key | 系统钥匙串 | **vault 密文进厂商档案**（`ProviderProfile::api_key`，随 `kv: ai_providers` 落 `duck.sqlite3`） |
| SSH 口令 | 系统钥匙串 | vault 密文进档案行（`StoredProfile.password`，见 P3-9） |
| 数据库 / Redis 口令 | 系统钥匙串 | vault 密文进 `<应用数据目录>/credentials.json`（`secret.rs` 换文件后端，API 形态不变） |
| 加密密钥本身 | — | `app_data_dir()/vault.key`（0600，32 字节 hex） |

- `keyring` 依赖**整块删除**（根 `Cargo.toml`、`duck-platform/Cargo.toml`），`Cargo.lock` 计数 **0**；
  `secret.rs` 里的 `SERVICE` / `Backend` / 两条编译期断言 / `#[ignore]` 实机往返测试一并删除。
- `duck-ai::secret` 从「转发 `duck_platform::secret`」改为「转发 `duck_platform::vault`」，
  `storage_key()` 删除；`ai_configured_keys` 标记表删除（一个事实不再记两处，
  `load_configured` 从档案推导：`api_key.is_some()`）。
- 备份归档的 `excludes` 由 `keyring-credentials` 改为 `credentials`（`vault.key` 同排除）；
  导出包里**没有**任何凭据。
- i18n：EN 表 7 替换 / 1 删除 / 3 新增，整表按 UTF-8 字节序重排（1424 → 1425 条）。

**顺带修掉的真 bug**：`vault.rs` 的**首用竞态** —— `OnceLock<Result<..>>` + `create_new`
仍不够：先创建的线程还没把 hex 写进去，另一个线程能读到空文件，`unhex("")` 得到长度 0 的
`Vec`，转换失败报 `Invalid`。修法是进程内 `static FIRST_USE: Mutex<()>` + **双重检查**
（拿到锁后再查一次 `KEY.get()`）。进程间不用考虑（本仓有单实例锁）。
触发条件是本轮新增的 25 条 `secret` 测试都用 vault、把首用并发概率放大。

**未迁移**：旧钥匙串条目（服务名 `com.duck.toolbox`）不改读（读一次必弹授权框），
留给用户在「钥匙串访问」里自行删除；首次重新保存后新链路即接管。

**涉及文件**：`duck-platform/src/{secret.rs,vault.rs,Cargo.toml,lib.rs}`、根 `Cargo.toml`、
`duck-ai/src/{secret.rs,provider.rs,agent.rs,lib.rs}`、`duck-app/src/ai.rs`、
`duck-app/src/pages/{settings,assistant}.rs`、`duck-app/src/backup.rs`、`duck-store/src/lib.rs`、
`duck-theme/src/i18n.rs`、本文档 + `docs/{ai,database,redis,email,terminal-ssh-sftp}-design.md`。

---

## 7. 现有功能待完善清单（速查）

| 模块 | 具体问题 | 证据 | 归口 |
| --- | --- | --- | --- |
| 概览页 | ~~指标卡硬编码 `24 / 1,284 / 820ms`~~ **已解决**：三张卡分别读工具注册表 / 使用记录 / 启动耗时实测值，无样本时显示 `—` | `pages/overview.rs` | **P2-5 ✅** |
| 概览页 | ~~「导入配置」已接「设置 → 数据」；「查看文档」仍无处理器~~ **已解决**：「查看文档」接 `open_url`（并给 `open_url` 补上失败反馈）；「新建工具」因无创建流程而删除 | `pages/overview.rs`、`duck-platform/src/shell.rs` | **P2-5 ✅** |
| 活动栏 | ~~4 条占位路由空挂（ssh / database / projects / assistant）~~ 第一轮整体移除后，**2026-10-11 晚按用户要求恢复**：4 条路由连动作 / 键位 / 占位页原样复原，活动栏 6 个入口（见 P2-4 章节回退段） | `app.rs`、`actions.rs`、`settings.rs::KNOWN_ROUTES` | **P2-4**（回退） |
| 顶栏搜索 | ~~承诺「搜索工具、命令…」，实际只过滤工具箱~~ **复核后不需要改**：顶栏入口已由 P1-4 改成「打开命令面板」，承诺由面板本身兑现，分组 / 键盘选择 / 模糊命中全部到位 | `top_bar.rs`、`pages/palette_tools.rs` | **P2-1 ✅**（由 P1-4 交付） |
| 命令面板 | ~~只投影动作，不含工具与最近；`recent` 只写不读~~ **已解决**：最近使用置顶 + 工具按分类 + 设置项直达 + 自研模糊打分接管过滤 | `palette_tools.rs`、`duck-command/src/fuzzy.rs` | **P1-4 ✅** |
| 工具箱 | ~~限速 / 代理三个死状态~~ **已解决**：三个字段与两组控件删除（下载引擎不存在，「真实消费」无从谈起） | `toolbox.rs` | **P2-3 ✅** |
| 工具箱·状态栏 | ~~假速率 `0 B/s` + 全部暂停 / 全部开始 / 完成后关机，点击只弹「演示：…」~~ **已解决**：全部删除（前提——下载引擎 / 全局任务队列 / 宿主电源——都不存在），改挂真实读数（插件·工具计数 + 磁盘剩余空间），无内容时整条不渲染 | `pages/toolbox.rs` | **P3-6 ✅** |
| 工具箱·刷新 | ~~`⌘R` / 菜单「工具→刷新」/ 命令面板的刷新只弹「演示：刷新（未接入工具后端）」~~ **已解决**：改调 `plugins::rescan` 真重扫插件目录 | `actions.rs`、`app/plugins.rs` | **P3-6 ✅** |
| 设置·行为 | ~~「紧凑行高」「删除前确认」死开关~~ **已解决**：紧凑行高删除（无任务列表可作用）；「删除前确认」接线成**卸载插件前确认**并落 `kv` | `pages/settings.rs`、`market.rs` | **P2-3 ✅** |
| 持久层 | ~~`settings` 表无读写方（死表）~~ **已解决**：迁移 v2 `DROP TABLE IF EXISTS settings`，两条测试反向钉住 | `store/db.rs` | **P2-3 ✅** |
| 平台层 | ~~日志无单文件大小上限~~ **已解决**：`LOG_FILE_MAX_BYTES`（8 MiB，序号轮转）+ `LOG_DIR_MAX_BYTES`（256 MiB 占用兜底）；`dialog` 全阻塞；~~`open_url` 无失败反馈~~ **已解决**：`open_url` 改为返回 `Result` 并在后台回收子进程（原来点一次链接留一个僵尸） | `logging.rs`、`dialog.rs`、`shell.rs` | 日志上限已在 **P2-2 ✅**；`open_url` 已在 **P2-5 ✅** |
| 日志查看器 | ~~文件列表按文件名倒序~~ **已解决**：段序号是十进制数字，字典序会把 `.10` 排到 `.9` 前面；改成按 `(日期, 段序号)` 排，并把命名解析从平台层公开出去 + 加往返契约测试 | `pages/logs.rs`、`logging.rs` | **P2-2 ✅**（实施中新发现） |
| 通知 | ~~设置页「通知」分组只有一句说明 + 三个演示按钮，**一个开关都没有**~~ **已解决**：应用内横幅三档（全部 / 仅警告与错误 / 全部关掉）+ 系统通知开关，落 `kv` 且即时生效；历史照记且原方案里的「声音通道」因上游无 API 而如实取消。顺带修掉两处静默失败：去抖回读把档位改动吞掉、「两者同时」因自绘 toast 无文本而系统那条被丢弃 | `duck-components/src/notify.rs`、`pages/settings.rs`、`settings.rs` | **P2-6 ✅** |
| 插件 JS 轨 | ~~无网络 host 函数，`NetworkHost` 空转~~ **已解决**：`duck.fetch` 上线，主机白名单逐跳校验 + 跨请求时长预算 + 逐跳重定向 + 响应体上限 + 审计日志（零新依赖，复用已在图内的 `ureq`） | `plugin-js/src/net.rs`（新）、`lib.rs`、`plugin/capability.rs` | **P2-9 ✅** |
| 插件授权 | ~~安装确认框只展示，子集授权未实现~~ **已解决**：确认框逐条开关 + 详情页事后调整 + 卸载回收，运行时按「声明 ∩ 授予」仲裁；顺带修掉 JS 侧 `Display` 丢异常消息导致「只报插件执行失败」的缺陷 | `plugin/capability.rs`、`app/plugins.rs`、`pages/plugin_center.rs` | **P2-8 ✅** |
| i18n | 无复数 / 日期 / 数字本地化 | `i18n.rs:14-46` | P3-2 |
| 窗口 | ~~仅主窗几何落盘；辅窗每次居中重建~~ **已解决**：主/辅窗几何与显示状态均落盘，会话（路由 / 右栏 / 辅窗集合）经 `kv` 恢复 | `app.rs`、`settings.rs` | **P1-2 ✅** |
| 唤起快捷键 | ~~`⌥Space` 会把主窗整个切到前台（与键位的启动器语义不符）~~ **已解决**：落点改为**独立命令面板浮窗**（`WindowKind::PopUp` → macOS `NSPanel` + 非激活面板 + 层级 101；Windows `WS_EX_TOOLWINDOW \| WS_EX_TOPMOST`），失焦 / Esc / 关闭按钮三条出口；设置里加「唤起时打开主窗口」开关可回到完整旧行为 | `duck-shell/src/palette_window.rs`（新）、`command_palette.rs`、`app.rs`、`hotkey.rs`、`settings.rs` | **P1-1 ✅**（补充） |
| 可访问性 | ~~零星 label，无全流程验证~~ **已解决**：自绘可点元素补 role / 名字 / 选中态（a11y 树只收带 role 的节点，此前**等于不存在**）+ `tab_stop` + 焦点描边；`gpui_base::Button` 默认已可 Tab | `duck-components/src/{lib,kit}.rs`、`duck-shell/src/view.rs`、`duck-app/src/theme_picker.rs`、`pages/{settings,logs}.rs`、两处新 `tests/` | **P2-7 ✅**（焦点环对比度挂起） |
| 图标按钮 | ~~只有 `accessibility_label`，鼠标悬停看不到任何提示~~ **已解决**：`icon_button` / `toolbar_action_button` 两个构造器统一挂 `Tooltip`（文案与读屏标签同源），一处收口覆盖全仓约 50 个图标按钮 | `duck-components/src/lib.rs` | **P3-7 ✅** |
| 终端·分屏 | ~~工具条上没有分屏入口（上一轮把死图标撤了）~~ **已解决**：`SessionTab.split`（右窗格会话键 `<id>#2`）+ 独立第二条会话 + 左右双画布；`⌘D` 与工具条图标同一路径。**增强（P3-8）**：`split_owner` 支持**不同连接分屏**（左栏右键菜单「在分屏中打开」）+ **拖标签过中线并入**（主流终端手感） | `pages/{terminal,terminal_canvas,terminal_data}.rs`、`app.rs` | **P3-7 ✅ / P3-8 ✅** |
| 终端·SSH 连接 | ~~任何连接都报 `keyword controlpath extra arguments at end of line`~~ **已解决**：`ControlPath` 路径含空格（`Application Support`）被 ssh 拆成多词；给值加引号 + socket 目录改到无空格的 `cache_dir()` | `duck-ssh/src/target.rs`、`pages/terminal.rs` | **P3-8 ✅** |
| 终端·SFTP 面板 | ~~面板关掉后再点图标没反应（目标只从当前标签推导 → 本地 shell 时开出假面板、无标签时按钮置灰）~~ **已解决**：记住上次开过 SFTP 的连接（`resolve_sftp_owner`），本地 shell / 非档案 id 一律拒绝 | `pages/terminal.rs` | **P3-7 ✅** |
| 终端·连接口令 | ~~保存口令后点连接弹 macOS 钥匙串授权框 + 编辑时口令不反显~~ **已解决**：口令走 `duck_platform::vault`（AES-256-GCM）加密进档案行（`duck.sqlite3`），连接 / 回显全程本地解密零弹框 | `duck-platform/src/vault.rs`（新）、`pages/{terminal,terminal_data,terminal_dialogs}.rs` | **P3-9 ✅** |
| AI·API key | ~~写进系统钥匙串（与系统绑定，且 dev 重编译后读一次弹一次授权框）~~ **已解决**：改密文进厂商档案（`ProviderProfile::api_key` → `kv: ai_providers`），编辑弹窗解密回显、清空即删除 | `pages/settings.rs`、`duck-ai/src/{secret,provider}.rs` | **P3-10 ✅** |
| 全仓凭据 | ~~`keyring` 依赖 + `SERVICE = com.duck.toolbox`（系统钥匙串 / 凭据管理器）~~ **已解决**：`keyring` 依赖整块删除，`Cargo.lock` 计数 0；`secret.rs` 换「`credentials.json`（vault 密文）」文件后端 + `vault.key`（0600），全仓再无任何系统凭据存储调用 | `duck-platform/src/{secret,vault}.rs`、根 `Cargo.toml` | **P3-10 ✅** |

---

## 8. 建议路线图

### 第一步（先补 P0，约 9 人日）— 进度 9.0 / 9.0（**P0 全部完成**）
P0-1 崩溃捕获（**已完成**）→ P0-2 安全凭据存储（**已完成**，2026-10-12 已与系统钥匙串解绑）→ P0-3 数据导出/重置
（**已完成**）→ P0-4 引导 + 许可（**已完成**）。
理由：它们不是锦上添花 —— 一旦有真实用户，"无法定位、凭据无处安放、数据找不回"会变成硬伤。
> P0-2 的依赖评审**已作废重做**（2026-10-12）：原结论钉 `keyring = "3.6.3"`（MIT OR Apache-2.0、
> MSRV 1.75；4.x 全系 MSRV 1.88 顶破本仓 1.85），现改为**不使用 `keyring`**——
> 凭据落 `<应用数据目录>/credentials.json`（vault 密文）+ `vault.key`，`Cargo.lock` 净减 2 包。
> 当初踩到的**真陷阱**（`keyring` 默认无特性、漏开时静默回落内存 mock → 写入成功、重启全丢）
> 已随依赖移除而不复存在，详见 P0-2 章节的历史留档。
> P0-1 / P0-3 / P0-4 零新依赖（P0-3 复用已在依赖图里的 `tar` / `flate2`），因此先行落地。

### 第二步（再做 P1，约 18.5 人日）— 进度 18.5 / 18.5（**P1 全部完成**）
P1-2 会话恢复（**已完成**）→ P1-4 命令面板（**已完成**）→ P1-3 更新链路安全（**已完成**）→
P1-5 拖拽/深链接（**已完成**）→ P1-1 全局唤起快捷键（**已完成**）。
理由：决定产品是"一个窗口工具"还是"常驻的效率层"；其中 P1-3 有安全属性，建议不晚于公开分发。
> P1-2 与 P1-4 都**零新依赖**，因此排在 P1 前列；P1-4 实施时把「模糊」做成了
> `duck-command::fuzzy` 共享打分器（P2-1 顶栏搜索直接复用），并把过滤权从上游
> `Command` 移交给宿主（`filterable(false)` + `on_query`）——这是上游文档写明的
> 「外部数据源答题」用法。P1-4 顺带确认：设置项直达的**分区级**滚动定位需要
> 先给设置内容区补 `ScrollHandle`，已移交 P2-1。详见 P1-4 章节。
> P1-3 原计划排在 P1-1 之后，实际提前到这里：它有**安全属性**，且上游能力无需核实
> （minisign 是成熟格式）。新依赖只有一个零传递依赖的 `minisign-verify`。详见 P1-3 章节。
> P1-5 同样**零新依赖**（复用已在依赖图里的 `futures-channel` / `libc` / `windows-registry`），
> 且它原先那句「需先核实上游能力」在动手前十分钟就查清了：拖放与深链接上游**都有**，
> 全局热键上游**完全没有**——所以 P1-5 排到了 P1-1 前面，把「引新依赖」留到最后一项。
> 详见 P1-5 章节。
> **P1-1 是 P1 里唯一引新依赖的一项**（`global-hotkey 0.8.0`，`Apache-2.0 OR MIT`、MSRV 1.77）。
> 真正的成本不在引依赖，在两处没人预料到的地方：**键名要在 gpui 与注册库的两套命名之间翻译**
> （做成唯一一张表 + 三条测试钉死），以及 **macOS 上「把窗口叫回来」不是一句话**——gpui 只暴露
> `hide` 与 `minimize_window`，反向两条都没有，且 `activate_window` 走的 `makeKeyAndOrderFront:`
> 不解除最小化；于是新增 `duck-platform/src/foreground.rs`，用已在依赖图里的 `objc2` 补齐。
> **交付后补做的「唤起语义」**（独立命令面板浮窗，约 0.5 人日，不计入路线图估工）：实测发现
> `⌥Space` 把主窗整个切到前台，与这个键位的启动器语义不符。上游**原生支持**（`WindowKind::PopUp`
> 在 macOS 就是 `NSPanel` + 非激活面板），平台层零改动；见 P1-1 章节的补充小节。
> 详见 P1-1 章节。

### 第三步（P2 体验收口，约 17 人日）— ✅ **进度 17.0 / 17**
**先止损**：P2-4 占位路由（**已完成**）+ P2-3 死代码（**已完成**）+ P2-5 概览真实化
（**已完成**）→ P2-1 搜索（**已完成**，复核后确认由 P1-4 交付）→ P2-2 日志（**已完成**）→
P2-6 通知（**已完成**）→ P2-8 子集授权（**已完成**）→ P2-9 网络能力（**已完成**）→
**P2-7 可访问性（已完成）**。
> 第三步**全部收口**。P2-7 遗留两项不计入估工：① 焦点环对比度（`ring` 的收敛对象
> 是按钮文字而不是它落上去的底，实测 80/420 组合 < 3:1）；② `content/nav.rs` 的 `tab_bar`
> 受上游 `TabBar` 限制（不复报 `aria_selected`、无注入缝隙，且该包装器零调用方）。
> 「先止损」三件是有意排在最前的：它们共同的问题是**承诺与实现不符**（假数字、假按钮、空占位、
> 不可达的死状态），而这类问题对信任的伤害比缺功能大得多，修起来又最便宜。
> 三件按「从外到内」的顺序做：P2-4 先让界面上不再有点开是空页的入口 → P2-5 把概览页的假数字
> 换成实测值（顺带自建启动耗时埋点，原方案以为 P0-1 已经提供了它，实际没有）→ P2-3 清掉剩下的
> 死表 / 死字段 / 死开关（其中「卸载前确认」是唯一接线项，因为它真有一个消费方：
> 会真删插件目录的卸载链路）。详见各自章节。
> **P2-2 是三件之后唯一还能自证「越用越坏」的一项**：日志按天滚动看着无害，实际单日文件没有
> 上限、目录总量也没有上限，长跑 + debug 级别就是缓慢吃满磁盘。所以它排在 P2-6 之前：修的是
> 一个**已经在发生**的问题，而不是一个将来会遇到的问题。详见 P2-2 章节。
> **P2-6 的价钱同样不在「加开关」上**：原方案要的三条通道里有一条（声音）**库里根本没有 API**，
> 而剩下一半工作量耗在两个静默失败上——去抖落盘让「改档位」压根不生效、
> 自绘 toast 让「系统通道」拿不到文本而按设计静默丢弃。两个都是**只有跑起来数得出来**的问题，
> 所以先补了 9 条无头行为测试再去调界面。详见 P2-6 章节。

### 第四步（P3 生态与治理，约 19 人日）— 进度 0 / 19
P3-3 大文件拆分（**加功能前先拆，否则技术债复利**）→ P3-1 插件签名 → P3-2 i18n → P3-4 多平台 → P3-5 主题/遥测。

### 贯穿性规则（来自 AGENTS.md，每项都必须满足）
- 任何文件新增 / 修改 / 删除**先列清单取得确认**（3.1）；
- **绝不发明 API**，GPUI 签名以 `temp/gpui-kit/` 与源码为准（3.2）；
- 新增中文文案**必须补 `EN` 表**，否则 `cargo test -p duck-theme` 失败（3.3）；
- 改 `sbin/*.sh` 后 `bash -n`（3.4）；新增依赖先核对版本与许可证（3.5）；
- 提交前 `cargo fmt --all` + `cargo clippy --workspace --all-targets`（第 5 节清单）。

---

## 附录 A：优先级判据

| 档位 | 判据 | 典型后果 |
| --- | --- | --- |
| **P0** | 有真实用户即暴露的硬伤，且**无替代手段** | 崩溃无痕 / 凭据无处安全存放 / 数据无法找回 / 合规风险 |
| **P1** | 决定产品形态与安全底线 | 只是"一个窗口"而非"效率层"；更新链路可被篡改 |
| **P2** | 影响体验一致性与信任 | 承诺与实现不符（假数据、假按钮、空占位） |
| **P3** | 生态、治理与长期可维护性 | 技术债复利；生态无法扩展 |

## 附录 B：当初判断风险最高的三项（现已扩到七项）

> 前三项是原方案里的「风险最高三项」；P2-9 是补进去的攻击面项，P2-2 / P2-6 / P2-8 是实施后**追认**的
> ——它们当初根本没被列进风险清单（P2-2 甚至连"风险"都没被当成风险），而实际价钱正落在
> 没人写进方案的地方。
> 已实施的几项都把结论留在这里：**风险的实际形状和当初的估计常常不一样**，留着比删掉有用。
> 本附录这七项**现已全部落地**；最后一项 P2-9 的结论见下面第 4 条。

1. **P1-5（拖拽 / 深链接）** —— ✅ **已实施，风险估计偏保守。**
   当初担心的是「GPUI 上游可能不暴露 `on_drop` / `on_open_urls`」。实际核实（读源码，见 P1-5
   章节的上游能力表）两者**都有**，且拖放被上游翻成了内部拖拽，接起来只需挂两个声明式 API。
   真正花时间的是**没人预料到的那部分**：单实例**转发通道**（让位者怎么把手上的参数交给正主）。
   它要选通道、定协议、处理陈旧落点、保证退出路径不卡住，还要在没有 `flock` 的 Windows 上
   顺带把单实例判据补上。
2. **P1-3（更新签名）** —— ✅ **已实施。** 密钥管理（CI Secrets）、公钥轮换、
   跨进程契约测试三处联动确实都有活；其中「客户端只认预哈希签名」与
   「公钥是编译期内建」两条是最容易配错又最难自查的地方。
3. **P1-1（全局唤起快捷键）** —— ✅ **已实施，风险实际落在「没人担心过」的两处。**
   当初估计的风险是「引一个系统级 API 的 crate」——那部分很顺（`global-hotkey` 的 API 面很窄）。
   真正花时间的是：① **键名要在 gpui 与注册库的两套命名之间翻译**，两者都不接受对方的格式，
   只能靠一张唯一表 + 三条测试钉死；② **macOS 上把窗口叫回来**——上游只有 `hide` /
   `minimize_window`，反向两条都没有，且 `activate_window` 不解除最小化，得自己用
   `objc2-app-kit` 补 `unhide:` / `deminiaturize:`。另一处**能力边界**必须记住：macOS 的
   Carbon 允许两个进程登记同一组合，所以「被 Raycast 抢了」我们**探测不到**，只能如实报
   「系统拒绝了」那一半。
   > **交付后的补充（0.5 人日）**：按 `⌥Space` 实测发现「主界面整个被调出来」——查证后是
   > **必然结果而非渲染错乱**（面板本是主窗里的浮层）。判断落在「键位与语义不匹配」上：
   > `⌥Space` 自带 Spotlight 式的**启动器**语义。补的活是把面板**换挂到独立浮窗**
   > （`WindowKind::PopUp`，GPUI 原生就有，平台层一行没动）。真正要自己想的只有两处：
   > macOS 上**非激活 `NSPanel` 仍收键盘**（`GPUIPanel` 覆写了 `canBecomeKeyWindow`）这条
   > 前提，以及「失焦即收」只能靠**状态迁移**（激活→不激活）而不能看当前值——否则窗口
   > 刚建出来还不是 key window，每次弹出都会被立刻关掉。另外 `titlebar: None` 在 macOS 上
   > **仍留一条 28pt 隐形标题栏**（吃点击），卡片必须让开它。
4. **P2-9（插件网络能力）** —— ✅ **已实施，风险不在「加一个 `fetch`」，在「上游的超时管不住阻塞调用」。**
   当初的判断（一旦开放就是攻击面扩大、必须排在 P0-2 与 P2-8 之后、默认拒绝）**都对**，
   但**没估计到真正的技术坑**：QuickJS 的 10 秒中断只会在**字节码之间**触发，`ureq` 一旦阻塞在
   socket 上，那个上限**完全无效**——所以出网预算只能自己做，而且必须与中断处理**共享同一个
   `deadline`**，否则「脚本总时长 10 秒」这句话对含网络的脚本就是假的（这也就解释了为什么
   预算要按「一次运行」而不是「一次请求」来记）。
   同一约束还推出了另外两处写法：① 一次运行内的**请求数与总时长合并成一个预算**（`NetBudget`），
   逐请求递减而不是每请求重置；② 重定向**手写逐跳循环**（`max_redirects(0)` 配
   `max_redirects_will_error(false)`，让 3xx 原样返回而不是报错），于是**跨域自动被拒**——
   每一跳都要重跑 `require_host`，「阻止跨域跳转」不是一个另外要写对的规则，而是白送的。
   另有两处只有动手才会碰到的判断：响应体**先看声明长度**（声明超标就根本不下载）、
   4xx/5xx **不当传输错误**（交给脚本按 `status` 判断）；审计日志**只记**
   `plugin / method / host / port / status / bytes`——刻意不记 header 与 body，因为紧接着的
   凭据注入项会把凭据放在那里，而日志是要被导出进 issue 的。
5. **P2-2（日志增强）** —— ✅ **已实施，风险不在「加个上限」，在语义边界。**
   原方案三条看起来都是「配置项 + 界面」，动手后价钱全在没人会写进方案的地方：① 一级按时间删、
   二级按占用删，两者**回答的不是同一个问题**，所以「永久保留」拦不住占用兜底——这条不写清楚
   就是个能吃掉整块磁盘的坑，得用测试把语义钉死；② 过滤按**行**做还是按**事件**做，决定
   `{:#?}` 展开的字段能不能被搜到、留下的条目会不会缺头，这不是实现细节而是功能定义；
   ③ 顺带暴露了一个**已经存在**的缺陷（段序号按字典序排，`.10` 跑到 `.9` 前面），
   而它恰好说明「命名规则一处写、另一处猜」这种结构迟早要还账——于是把解析函数公开出去，
   补一条往返契约测试。**估工本身没超**，超的是「方案里没写、但必须想清楚」的部分。
6. **P2-6（通知体系）** —— ✅ **已实施，风险不在「加两个开关」，在两处静默失败。**
   原方案的「三条通道」里有一条（声音）**上游根本没有 API**——这是查一下源码就能否掉的，
   不算风险。真正的价钱是：① 去抖落盘（400ms）让「读完再写回」的写法把档位改动**吞掉**，
   运行时压根没变、界面还弹回旧档，表现成「点了没反应」；② 自绘 toast 为了绕开图标定位的
   硬编码 rem，把标题正文都塞进了 `content`，于是库的系统投递分支取不到文本、
   撞上它的 `(None, None) => return` **按设计静默丢弃**——「两者同时」实际上是「只有应用内」。
   两个都只有**跑起来数得出来**（前者数全局值、后者数 `shown_system_notifications()`），
   所以本轮先把 9 条无头行为测试补齐再去调界面。**老实说：原方案对这项的工作量估计是对的
   （2.0 人日），但对「干什么」的估计偏了**——真正的工作是修这两个静默失败，而不是加开关。
   > 这也是本附录反复在说的事：**风险的实际形状和当初的估计常常不一样**。
7. **P2-8（插件子集授权）** —— ✅ **已实施，风险不在「加勾选框」，在「缺省往哪边倒」。**
   `Capability::set_of`（声明 → 单元）与 `Capabilities::only`（单元 → 声明）是一对互逆映射，
   分开写就迟早对不上——`only` 里漏掉 `process_exec`，就是「确认框勾了也跑不起来」，
   而界面上完全看不出来（所以配了 `the_effective_grant_is_the_intersection_of_both_sides` 钉死）。
   但真正决定成败的是两个**缺省**：① 「没有记录」必须倒向**全给**，倒错了会让所有已装插件在
   升级后一夜哑火；② 「有记录但不是全给」在**更新**插件时必须作为初始勾选沿用，不能重置成全勾，
   否则用户收回过的权限会被静默还回去。两处都写成了 `resolve_grant` 的分支 + 单测。
   另外这项的**入口是一个构建错误**——上游半截迁移导致 `duck-app` 编译不过，
   于是「顺手把剩下半截补完」和「让应用能启动」是同一件事。

---

*本文档既回答"缺什么 / 怎么补 / 多大代价"，也记录已完成项的实施结果、取舍与踩坑。*
*实施任一 P 项前，按 AGENTS.md 3.1 提交「受影响文件清单 → 具体改动 → 潜在影响」并取得确认。*
