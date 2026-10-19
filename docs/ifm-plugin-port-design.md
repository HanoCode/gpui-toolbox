# IFM 工具箱 → Duck Toolbox 插件化迁移设计

> 状态：**设计稿（已核对范围，未实施）**
> 配套：**逐工具 UI 设计稿见 `ifm-plugin-ui-design.html`**（47 张卡片全部有判定；核对结果回填后：**50 在册工具 / 37 个界面 / 10 插件包**）
> 实施：**宿主改造与迁移顺序见 `ifm-plugin-port-plan.md`**（W1–W6 工作包、M0–M6 里程碑、114 人日）
> 核对记录：`ifm-ui-review-2026-10-18.json`（34 合理 / 3 收窄 / 10 不迁）
> 源仓：`/Users/bws/code/code/github/ifm_ai_tools`（Tauri 2 + Vue 3，`bank-impl-toolkit` 1.3.0）
> 目标仓：`/Users/bws/code/github/gpui-toolbox`（GPUI，插件化工具目录）
> 结论口径：全部判断基于**两仓当前源码事实**（引用 `文件:行` 或统计数字），未核实的项一律标「待核实」。

---

## 0. 结论

1. **不能按 UI 页面 1:1 复刻。** 源仓 88 个工具背后是 67,090 行 Vue 页面，其中 3 个页面就占 18,000 行（EmailClient 8,612 / DatabaseManagerPro 7,641 / SmartSshTerminal 2,337）。宿主是 GPUI 自绘、无 webview 轨，搬页面等于重写。
2. **必须用「声明式 IO」替代页面。** 插件只声明「要什么输入、产出什么形状」，宿主渲染通用表单与结果视图。这一条把 88 个工具的 UI 成本从「逐页重写」压到「一套通用视图」。
3. **必须先补宿主框架，再迁工具。** 当前插件轨是「一次性执行 + 空输入 + toast 输出」，88 个工具按现状**没有一个能在宿主里可用**（证据见 §2）。
4. **重工具走 `process` 子进程插件**（用户已定），逻辑从源仓 Rust 平移、去掉 Tauri 耦合；宿主新增 `duck-plugin-process` 轨。
5. **38 个工具不迁**：邮件、数据库、Redis、SSH 四域及其衍生的运维编排工具（代码部署 / 自动化部署 / 应用中心 / 集群管理 / 集群日志 / 上线检查）宿主已有更完整实现；另加 AI 助手、应用启动器、待办、系统清理、桌面整理、授权码、12 个 beta/dev 独有工具，以及核对时新删的 7 项（Prompt 优化、Protobuf、存储估算、金融文件解析、敏感扫描、截图、Mermaid 渲染）+ `transform-hub` / `drawio-board` / `device-pool`。仅 SSH 四域就甩掉源仓 405,652 行 workspace 代码中的绝大部分（`email-core` 29,771 + `dbx-core`/`dbx-adapter` 系）。
6. 剩余 **50 个工具聚成 10 个插件包、合并为 37 个界面**，估工 **114 人日**（约 23 周）；两人并行约 12 周。逐工具的最终状态以 UI 设计稿的 47 张卡与核对 JSON 为准，本文 §5 的 88 条溯源表仅作**历史留档**。

---

## 1. 两仓基线

| 维度 | ifm_ai_tools（源） | duck-toolbox（目标） |
| --- | --- | --- |
| 技术栈 | Tauri 2.11 + Vue 3 + Rust | GPUI + 22 个 crate |
| 工具注册 | `src/composables/tools.ts` 88 条 | `ToolRegistry`，**内置全部下线**，只认插件贡献（`crates/duck-app/src/tools.rs:3`） |
| 前端代码 | `src/tools/*.vue` 105 个文件 / 67,090 行 | 无（GPUI 视图） |
| 后端代码 | `src-tauri/src` 104,474 行 + workspace crates 405,652 行 | 按 crate 分域（`duck-db` / `duck-redis` / `duck-ssh` / `duck-mail` / `duck-project` …） |
| 插件样例 | 无插件机制（`src/plugins` 只有 markdown-editor 组件） | `plugins/text-stat`（`plugin.toml` + `main.js` + `locales/en.toml`） |
| MSRV | `rust-version = "1.93"`，edition 2021 | 1.85，edition 2024 |
| 分类 | 15 个中文分类 | `ToolCategory` 13 档（`duck-tools/src/category.rs`）：前 8 档与源仓同名同序（代码开发 / 数据管理 / 网络调试 / 代码比对 / 上线合并 / 效率工具 / 文件工具 / 帮助中心），后 5 档是宿主原有的内置分类 |
| i18n | 中文硬编码 | 源串即键 + `en.toml` + 插件 `locales/<tag>.toml` |

---

## 2. 硬约束（决定方案形态的六条）

| # | 约束 | 证据 | 对迁移的影响 |
| --- | --- | --- | --- |
| C1 | 工具执行时 `input` 恒为空串 | `crates/duck-app/src/plugins.rs:434` 传 `""` | 任何「读输入 → 产结果」的工具都拿不到输入 |
| C2 | 产出只经 toast 呈现 | `plugins.rs:487` `emit_outcome` | 表格 / 树 / diff / 日志面板无从展示 |
| C3 | `wasm` / `process` 轨未接入 | `plugins.rs:402-405` → `run_placeholder` | 需要原生库的工具没有落点（本次要补） |
| C4 | JS 单次 10 秒硬上限、无进度回传 | `duck-plugin-js/src/lib.rs` `TIME_LIMIT` | 包合并/压测动辄分钟级，到点被杀 |
| C5 | 文件能力根目录 = 插件 `dataDir` | `duck-plugin-js` 文档 + `Capability::FilesystemRead(glob)` | 用户无法选任意目录，而合并/比对都是目录级输入 |
| C6 | 清单工具只映射 id/name/description/category/icon/keywords | `plugins.rs:608-624` vs `duck-plugin/src/manifest.rs:181-196` | `group` / `beta` / `dev` / 工具级 `settings` / IO 声明全部无处安放 |

补充两条易被忽略的：

- `plugin.toml` 的 `[i18n]` 段**必须写在 `[[tools]]` 之前**（TOML 表归属，写错整份清单解析失败）。
- `tool.id` 强制以 `<plugin.id>.` 为前缀（`manifest.rs` `validate`），88 个工具的 id 需全部加前缀。

---

## 3. 目标形态：三层模型

```
┌─ L1 JS 函数插件（QuickJS，10s 内、纯计算）────────────── 25 个工具
├─ L2 process 子进程插件（stdio JSON-RPC，可长任务/可取消）── 47 个工具
└─ L3 不迁（宿主既有模块覆盖 / 高危动作）───────────────── 16 个工具
```

**判定规则（可机械执行）**：

| 判据 | 结论 |
| --- | --- |
| 纯字符串 ↔ 字符串，无外部依赖，单次 < 10s | L1 |
| 需目录扫描 / 解压 / 反编译 / 编解码库 / 外部进程（java、soffice） | L2 |
| 宿主已有同等或更强实现（`duck-db`/`duck-redis`/`duck-ssh`/`duck-mail`/`duck-project`/`duck-ai`） | L3 |
| 高危（删系统文件、写 `/etc/hosts`、桌面整理） | L3 或降级为「只读诊断」 |

---

## 4. 插件包划分（88 → 11 包）

| 插件 id | 工具数 | 执行轨 | 主要来源模块（Rust 行数） |
| --- | --- | --- | --- |
| `duck.devkit.text` | 12 | L1 | 前端 `lib/transforms/`（1,121 行）+ `formatter.rs`（1,349） |
| `duck.devkit.codec` | 3 | L1 + L2 | `hundsun_crypt.rs`（1,222）+ 前端编解码 |
| `duck.devkit.sql` | 8 | L1 + L2 | `sql_merge.rs`（2,101）、`sql_checker.rs`（3,198）、`sql_rewriter.rs`（386）、`shell-linter`（1,215） |
| `duck.ifm.merge` | 7 | L2 | `sp_merge.rs`（1,460）、`ifms6_merge.rs`（2,037）、`non_sp_merge.rs`（1,936）、`see_merge.rs`（1,082）、`hswealth_merge.rs`（582）、`scc_merge.rs`（555）、`extract_by_manifest.rs`（503）、`merge_common.rs`（639）、`upd_merge.rs`（552）、`ifm3_merge.rs`（329）、`ifm6_source_merge.rs`（353） |
| `duck.ifm.compare` | 7 | L2 | `class_compare/`（1,496）、`jar_compare/`（891）、`config_diff/`（1,142）、`decompiler.rs`（1,391）、`file_ops.rs`（387） |
| `duck.ifm.finance` | 4 | L2 | `finance_parser/`（3,738）、`pdm/`（1,344）、`channel_gateway/`（2,360） |
| `duck.ifm.datagen` | 5 | L2 | `datagen.rs`（3,490）、`stress_test.rs`（2,898）、`stub/`（1,074）+ `stub-core`（928）+ `stub-agent`（187） |
| `duck.devkit.net` | 7 | L1 + L2 | `transport/`、`soap/`、`net/`、`s3/`（316）、`hosts_manager.rs`（93） |
| `duck.devkit.files` | 10 | L1 + L2 | `fast_file_search/`（1,582）、`converter/`（860）、`excel_batch_convert/`（377）+ java jars |
| `duck.devkit.config` | 3 | L1 + L2 | `nginx/`（786）、`s3/`（316）+ 前端 Docker/K8s 校验 |
| `duck.devkit.misc` | 6 | L1 + L2 | `screenshot/`（1,407）、`device_pool.rs`（349）+ `device-pool-manager`（3,571） |

**合并原则**：一个插件包 = 一个领域数据模型 + 一份依赖集合。包内工具共享同一子进程二进制、同一份 `locales/`、同一套 fixture，避免 11 个包各带一份 `zip` / `quick-xml` / `calamine` 编译产物。
**已排除**：`deploy/`（4,675）、`cluster/`（1,958）、`log_agent/` 三个模块整块不进迁移范围 —— 它们全部架在 SSH 连接与远端 agent 之上，宿主 `duck-ssh` 已覆盖，重复造只会分叉会话/凭据模型。

---

## 5. 逐工具溯源（88 条 · 历史留档）

> **本表已被核对结果取代。** 最终口径（哪些迁、哪些不迁、界面怎么画）以 `ifm-plugin-ui-design.html` 的 47 张卡 +
> `ifm-ui-review-2026-10-18.json` 为准；下表保留最初的全量溯源，用于查「某个工具的后端模块在哪、多少行」。
> 与最终口径的差异：本表中标 `迁` 的 `prompt-optimizer`、`protobuf-codec`、`storage-estimator`、`finance-parser`、
> `sensitive-scanner`、`screenshot`、`diagram-renderer` 已改为不迁；`device-pool` 已改为不迁。

> 轨道：**L1** JS 函数 / **L2** process / **L3** 不迁。`—` 表示纯前端、无独立后端模块。
> 处置：`迁` / `合`（并入同包另一工具）/ `弃`（不迁）/ `降`（降级为只读或诊断）。
> 「待核实」= 尚未读实现，排期时补。

| 插件包 | 工具（id） | 前端 Vue / 行 | 后端 Rust / 行 | 轨 | 处置 |
| --- | --- | --- | --- | --- | --- |
| `duck.devkit.text` | 格式化（`formatter`） | Formatter.vue / 295 | formatter.rs 1,349 | L1 | 迁 |
| 〃 | 数据格式转换（`data-format`） | DataFormat.vue / 1,449 | —（前端 transforms） | L1 | 迁 |
| 〃 | 大小写转换（`case-convert`） | CaseConvert.vue / 32 | — | L1 | 迁 |
| 〃 | UUID 生成（`uuid-gen`） | UuidGen.vue / 54 | — | L1 | 迁 |
| 〃 | 时间戳转换（`timestamp`） | Timestamp.vue / 145 | — | L1 | 迁 |
| 〃 | Cron 生成（`cron-gen`） | CronGen.vue / 162 | — | L1 | 迁 |
| 〃 | 数字转中文大写（`number-to-chinese`） | NumberToChinese.vue / 208 | — | L1 | 迁 |
| 〃 | 随机文本生成（`random-text-generator`） | RandomTextGenerator.vue / 1,417 | — | L1 | 迁 |
| 〃 | 速查手册（`cheat-sheets`） | CheatSheets.vue / 363 | — | L1 | 迁 |
| 〃 | 变换枢纽（`transform-hub`） | TransformHub.vue / 142 | — | L1 | 迁 |
| 〃 | 正则增强（`regex-plus`） | RegexPlus.vue / 99 | — | L1 | 迁 |
| 〃 | Prompt 优化（`prompt-optimizer`） | PromptOptimizer.vue / 235 | — | L1 | 迁 |
| `duck.devkit.codec` | 加解密工具包（`crypto-bundle`） | CryptoBundle.vue / 680 | hundsun_crypt.rs 1,222 | L1 | 迁 |
| 〃 | 编码转换（`encoding-converter`） | EncodingConverter.vue / 259 | — | L1 | 迁 |
| 〃 | Protobuf 编解码（`protobuf-codec`） | ProtobufCodec.vue / 174 | 待核实 | L2 | 迁 |
| `duck.devkit.sql` | SQL 替换（`sql-replace`） | SqlReplace.vue / 364 | — | L1 | 迁 |
| 〃 | SQL 合并（`sql-merge`） | SqlMerge.vue / 619 | sql_merge.rs 2,101 | L2 | 迁 |
| 〃 | SQL 质量检查（`sql-checker`） | SQLChecker.vue / 408 | sql_checker.rs 3,198 | L2 | 迁 |
| 〃 | SQL 脚本优化（`sql-reexec`） | SqlReExec.vue / 1,092 | sql_rewriter.rs 386 | L2 | 迁 |
| 〃 | DDL 生成（`ddl-generator`） | DdlGenerator.vue / 255 | — | L1 | 迁 |
| 〃 | SQL 分类抽取（`sql-classifier`） | SqlClassifier.vue / 434 | file_ops.rs 387 | L2 | 迁 |
| 〃 | 数据库存储估算（`storage-estimator`） | StorageEstimator.vue / 245 | — | L1 | 迁 |
| 〃 | ShellCheck 脚本检查（`shellcheck`） | ShellCheckTool.vue / 373 | shellcheck/ 53 + shell-linter 1,215 | L2 | 迁 |
| `duck.ifm.merge` | SP 包合并（`sp-merge`） | SpMerge.vue / 714 | sp_merge.rs 1,460 + merge_common 639 | L2 | 迁 |
| 〃 | 需求单合并（`non-sp-merge`） | NonSpMerge.vue / 617 | non_sp_merge.rs 1,936 | L2 | 迁 |
| 〃 | FINFSS 包合并（`see-merge`） | SeeMerge.vue / 538 | see_merge.rs 1,082 | L2 | 迁 |
| 〃 | v6-see 合并（`ifms6-merge`） | Ifms6Merge.vue / 354 | ifms6_merge.rs 2,037 | L2 | 迁 |
| 〃 | HSWealth 包合并（`hswealth-merge`） | HsWealthMerge.vue / 140 | hswealth_merge.rs 582 | L2 | 迁 |
| 〃 | SCC 包合并（`scc-merge`） | SccMerge.vue / 132 | scc_merge.rs 555 | L2 | 迁 |
| 〃 | 清单提取（`extract-by-manifest`） | ExtractByManifest.vue / 244 | extract_by_manifest.rs 503 | L2 | 迁 |
| `duck.ifm.compare` | Class 代码比对（`class-compare`） | ClassCompare.vue / 629 | class_compare/ 1,496 | L2 | 迁 |
| 〃 | Jar 包代码比对（`jar-compare`） | JarCompare.vue / 509 | jar_compare/ 891 | L2 | 迁 |
| 〃 | 文件比较（`dir-compare`） | DirCompare.vue / 667 | file_ops.rs 387 | L2 | 迁 |
| 〃 | 文本对比（`text-diff`） | TextDiff.vue / 416 | file_ops.rs 387（compute_diff） | L1 | 迁 |
| 〃 | 配置语义对比（`config-diff`） | ConfigDiff.vue / 213 | config_diff/ 1,142 | L2 | 迁 |
| 〃 | 配置文件环境对比（`config-env-diff`） | ConfigEnvDiff.vue / 469 | config_diff/ 1,142 | L2 | 迁 |
| 〃 | Java 反编译（`java-decompiler`） | JavaDecompiler.vue / 1,132 | decompiler.rs 1,391 | L2 | 迁 |
| `duck.ifm.finance` | 金融文件解析（`finance-parser`） | FinanceParser.vue / 1,667 | finance_parser/ 3,738 | L2 | 迁 |
| 〃 | TA确认文件（`finance-file-generator`） | FinanceFileGenerator.vue / 670 | finance_parser/ 3,738 | L2 | 迁 |
| 〃 | PDM 文件阅读器（`pdm-reader`） | PdmReader.vue / 671 | pdm/ 1,344 | L2 | 迁 |
| 〃 | 报文转换代码生成（`channel-gateway`） | ChannelGateway.vue / 1,088 | channel_gateway/ 2,360 | L2 | 迁 |
| `duck.ifm.datagen` | 造数（`data-generate`） | DataGenerate.vue / 1,079 | datagen.rs 3,490 | L2 | 降 |
| 〃 | 表配置（`table-config`） | TableConfig.vue / 877 | datagen.rs 3,490 | L2 | 降 |
| 〃 | 规则模板（`rule-template`） | RuleTemplate.vue / 263 | datagen.rs 3,490 | L2 | 迁 |
| 〃 | 压测工具（`stress-test`） | StressTest.vue / 693 | stress_test.rs 2,898 | L2 | 迁 |
| 〃 | 远程挡板（`stub-manager`） | StubManager.vue / 311 | stub/ 1,074 + stub-core 928 | L2 | 迁 |
| `duck.devkit.net` | HTTP 请求（`http-request`） | HttpRequest.vue / 1,518 | file_ops.rs 387 | L2 | 迁 |
| 〃 | TCP 请求（`tcp-request`） | TcpRequest.vue / 651 | transport/ | L2 | 迁 |
| 〃 | XML请求（`xml-send`） | XmlSend.vue / 584 | transport/ | L2 | 迁 |
| 〃 | FIX/报文 TCP 发送（`fix-send`） | FixSend.vue / 612 | transport/ | L2 | 迁 |
| 〃 | SOAP 请求（`soap-request`） | SoapRequest.vue / 629 | soap/ | L2 | 迁 |
| 〃 | S3/FTP/SFTP 检测（`s3-ftp-checker`） | S3FtpChecker.vue / 269 | s3/ 316 + net/ | L2 | 迁 |
| 〃 | Hosts 管理（`hosts-manager`） | HostsManager.vue / 621 | hosts_manager.rs 93 | L2 | 降 |
| `duck.devkit.files` | 文件内容搜索（`file-search`） | FileSearch.vue / 138 | — | L1 | 迁 |
| 〃 | 极速文件搜索（`fast-file-search`） | FastFileSearch.vue / 458 | fast_file_search/ 1,582 | L2 | 迁 |
| 〃 | 目录树生成（`directory-tree`） | DirectoryTree.vue / 331 | file_ops.rs 387 | L2 | 迁 |
| 〃 | 批量重命名（`batch-renamer`） | BatchRenamer.vue / 248 | — | L2 | 迁 |
| 〃 | 批量文件查找替换（`batch-replace`） | BatchReplace.vue / 200 | batch_replace.rs | L2 | 迁 |
| 〃 | 敏感信息扫描（`sensitive-scanner`） | SensitiveInfoScanner.vue / 188 | — | L2 | 迁 |
| 〃 | CSV 表格编辑（`csv-table-editor`） | CsvTableEditor.vue / 212 | — | L1 | 降 |
| 〃 | CSV 转图表（`csv-to-chart`） | CsvToChart.vue / 286 | — | L2 | 迁 |
| 〃 | 文档转换（`doc-converter`） | DocConverter.vue / 425 | converter/ 860 | L2 | 迁 |
| 〃 | XLS 批量转换（`xls-converter`） | XlsConverter.vue / 180 | excel_batch_convert/ 377 + java jars | L2 | 迁 |
| `duck.devkit.config` | Nginx 配置生成（`nginx-generator`） | NginxGenerator.vue / 476 | nginx/ 786 | L1 | 迁 |
| 〃 | Docker/K8s 工具（`docker-k8s`） | DockerK8s.vue / 141 | —（前端） | L1 | 迁 |
| 〃 | S3 存储管理（`s3-manager`） | S3Manager.vue / 16 | s3/ 316 | L2 | 迁 |
| `duck.devkit.misc` | 截图工具（`screenshot`） | ScreenshotTool.vue / 485 | screenshot/ 1,407 | L2 | 迁 |
| 〃 | Markdown画图（`diagram-renderer`） | DiagramRenderer.vue / 248 | — | L1 | 迁 |
| 〃 | 画图工具（`drawio-board`） | DrawioBoard.vue / 194 | plugin_manager.rs | L2 | 迁 |
| 〃 | JSON Schema 校验（`json-schema-validator`） | JsonSchemaValidator.vue / 197 | — | L1 | 迁 |
| 〃 | Git 辅助工具（`git-tools`） | GitTools.vue / 128 | — | L1 | 迁 |
| 〃 | 设备池管理（`device-pool`） | DevicePool.vue / 271 | device_pool.rs 349 + device-pool-manager 3,571 | L2 | 迁 |
| 不迁 | 数据库管理 pro（`db-manager-pro`） | DatabaseManagerPro.vue / 7,640 | db_pro.rs 2,400 + dbx-core | L3 | 弃 |
| 〃 | Redis 管理（`redis-manager`） | RedisManager.vue / 1,001 | redis_ops.rs | L3 | 弃 |
| 〃 | SSH 终端（`smart-ssh-terminal`） | SmartSshTerminal.vue / 2,337 | ssh/（弃） | L3 | 弃 |
| 〃 | AI邮件客户端（`email-client`） | EmailClient.vue / 8,611 | email_commands.rs 7,944 + email-core 29,771 | L3 | 弃 |
| 〃 | 待办事项（`todo-center`） | TodoCenter.vue / 1,299 | email_commands.rs（弃） | L3 | 弃 |
| 〃 | AI 助手（`ai-assistant`） | AiAssistant.vue / 633 | ai/（弃） | L3 | 弃 |
| 〃 | 应用启动器（`app-launcher`） | AppLauncher.vue / 578 | app_launcher.rs（弃） | L3 | 弃 |
| 〃 | 系统清理工具（`system-cleaner`） | SystemCleaner.vue / 2,291 | system_cleaner/（弃） | L3 | 弃 |
| 〃 | 桌面整理（`desktop-organizer`） | DesktopOrganizer.vue / 196 | — | L3 | 弃 |
| 〃 | 授权码生成（`license-manager`） | LicenseManager.vue / 148 | license.rs 116 | L3 | 弃 |
| 〃 | 上线检查清单（`deploy-checklist`） | DeployChecklist.vue / 278 | ssh/（弃） | L3 | 弃 |
| 〃 | 代码部署（`code-deploy`） | CodeDeployer.vue / 1,282 | deploy/（弃） | L3 | 弃 |
| 〃 | 自动化部署（`deploy-plugin`） | DeployPlugin.vue / 510 | deploy/（弃） | L3 | 弃 |
| 〃 | 应用中心（`app-center`） | AppCenter.vue / 1,110 | deploy/（弃） | L3 | 弃 |
| 〃 | 集群管理（`cluster-manager`） | ClusterManager.vue / 15 | cluster/（弃） | L3 | 弃 |
| 〃 | 集群日志（`cluster-log-viewer`） | ClusterLogViewer.vue / 1,624 | cluster/（弃） | L3 | 弃 |

---

## 6. 宿主必须新增的能力（P0，实施前完成）

### 6.1 声明式 IO（`[io]` 段）

工具不再靠页面，而是声明输入与结果形状：

> 落地口径见 `ifm-plugin-port-plan.md` ADR-7：**一工具一插件**，`tool.id` 直接等于
> 插件 id（`manifest.rs` 放行这一写法）。下面这段是设计期的形状示意，最终字段以
> `crates/duck-plugin/src/io.rs` 为准（`[[tools.io.fields]]` / `[[tools.io.params]]`）。

```toml
[[tools]]
id = "duck.text.case-convert"
name = "大小写转换"
description = "驼峰 / 蛇形 / 常量 / 短横线互转"
icon = "case-sensitive"
keywords = ["case", "camel", "snake", "命名"]

[tools.io]
input = "textarea"                # textarea | text | path | dir | paths | none
output = "text"                   # text | code | table | tree | markdown | file
accept = ["txt", "md", "csv"]     # input = path/paths/dir 时用于过滤
fields = [
  { key = "target", label = "目标命名", kind = "select",
    options = ["camel", "snake", "constant", "kebab"], default = "camel" },
  { key = "trim", label = "去掉首尾空白", kind = "switch", default = true },
]
```

- **字段种类只需 6 种**：`select` / `switch` / `text` / `number` / `path` / `dir`，覆盖 88 个工具的参数面。
- 值经 `args` 传给插件：`handle(toolId, input, args)`（JS）/ `{"args":{...}}`（process）。
- 缺省 `input = "none"`、`output = "text"`，**旧插件行为不变**，`api_version` 保持 1。

### 6.2 工具执行页

新增 `crates/duck-app/src/pages/tool_run.rs`：左表单 + 右结果（进度 / 日志 / 结构化结果）。

| 子视图 | 数据来源 | 复用 |
| --- | --- | --- |
| 输入区 | `[io].input` + `fields` | `duck-components` 表单原语 |
| 进度 | `{"type":"progress"}` 事件 | 现有 progress 组件 |
| 日志 | `{"type":"log"}` 事件 | `pages/logs.rs` 的只读列表 |
| 结果 | `output` 声明 | `code` → 可选文本；`table` → 现成数据网格；`tree` → 目录树组件 |

### 6.3 文件授权（替代 C5）

用户在选择器里选定路径后，宿主把该**具体路径**加入本次执行的临时授权集，子进程/JS 侧凭 token 读写。能力语义从「插件预声明的 glob」扩为「声明 glob ∪ 本次用户指定的路径」，且后者只活一次执行。

### 6.4 插件 KV

`duck.kv.get(key)` / `duck.kv.set(key, value)`，命名空间 `<plugin_id>`，落现有 `kv` 表。用于 hosts 方案、连接列表、规则模板、最近输入。

### 6.5 清单扩展（保持向后兼容）

| 字段 | 位置 | 作用 |
| --- | --- | --- |
| `group` | `[[tools]]` | 承载源仓 15 个中文分类（宿主 `ToolCategory` 只有 7 档，**不动枚举**） |
| `beta` / `dev` | `[[tools]]` | 源仓 10 个 `dev: true` 工具需原样保留 |
| `io` | `[[tools]]` | §6.1 |
| `[process]` | 清单顶层 | `entry` / `args` / `timeout_secs` |

### 6.6 `duck-plugin-process` 轨（新 crate）

```
plugins/<plugin_id>/
  plugin.toml
  bin/<binary>            # 与宿主同架构，随包分发
  locales/en.toml
```

协议：**行分隔 JSON（JSONL）over stdin/stdout**，stderr 直接进宿主日志。

| 方向 | 消息 | 说明 |
| --- | --- | --- |
| host → child | `{"id":"01","tool":"<tool_id>","input":"…","args":{…},"locale":"en","dataDir":"…"}` | 一次执行一条 |
| host → child | `{"type":"cancel"}` | 取消；3 秒后 `SIGTERM`，再 2 秒 `SIGKILL` |
| child → host | `{"type":"log","level":"info","message":"…"}` | 进日志面板 |
| child → host | `{"type":"progress","value":0.42,"message":"合并 42/100"}` | 进度条 |
| child → host | `{"type":"result","output":"…","artifacts":["…"]}` | 终态 |
| child → host | `{"type":"error","code":"E_ZIP","message":"…"}` | 终态（错误码进 en.toml 取词） |

关键约束：

- **i18n 用既有常量**：`duck_plugin::ENV_LOCALE` / `ENV_LOCALES_DIR` + `locale_env()` 已在 `duck-plugin` 里定义并单测钉住（`env_names_are_pinned`），本次是它第一个真实消费者。
- **取消走带外**（对齐 `duck-db` 的既有取舍）：读线程不能被阻塞调用占死。
- **超时**：默认无墙钟上限、仅靠用户取消；`timeout_secs` 可选，硬上限 30 分钟。
- **无 shell**：`Command::new(entry)` 直接执行，不经 `sh -c`；entry 必须是插件目录内的普通文件（拒符号链接逃逸）。
- **环境变量白名单**：只透传 `PATH` / `HOME` / `LANG` + `DUCK_LOCALE` / `DUCK_LOCALES_DIR`，不带 `GITHUB_TOKEN` 之类。
- **签名**：插件目录不在 `.app` 内 ⇒ 不经 Gatekeeper 隔离；市场下载的包须 `codesign -s -` 并清 `com.apple.quarantine`（`sbin/package-macos.sh` 的 `--deep` 不覆盖外部目录）。

---

## 7. 宿主改动清单（文件级）

| # | 文件 | 改动 | 影响面 | 门禁 |
| --- | --- | --- | --- | --- |
| 1 | `crates/duck-plugin/src/manifest.rs` | 新增 `group` / `beta` / `dev` / `ToolIo` / `ProcessSection` | 清单解析，`deny_unknown_fields` 下无破坏 | 现有 manifest 单测全绿 |
| 2 | `crates/duck-plugin-process/`（新） | 子进程轨：spawn / JSONL / 进度 / 取消 / 超时 | 新 crate，零 gpui | 单测：协议、取消、超时、坏 JSON |
| 3 | `crates/duck-app/src/plugins.rs` | 分轨派发；`input`/`args` 从执行页取；`emit_outcome` 分支 | 插件执行主路径 | `dev_plugins.rs` 集成测试扩展 |
| 4 | `crates/duck-app/src/pages/tool_run.rs`（新） | 执行页（表单 + 进度 + 日志 + 结果） | 工具箱点击行为 | 手动 + 快照 |
| 5 | `crates/duck-app/src/pages/toolbox.rs` | 有 `io` 的工具改为进执行页；无 `io` 的保持 toast | 交互分流 | — |
| 6 | `crates/duck-plugin/src/capability.rs` | 单次执行临时路径授权 | 能力仲裁 | 现有 glob 单测 + 新增临时授权用例 |
| 7 | `crates/duck-plugin-js/src/lib.rs` | `handle(toolId, input, args)` + `duck.kv` | JS 契约（向后兼容：缺 `args` 不报错） | `dev_plugins.rs` |
| 8 | `crates/duck-theme/locales/en.toml` | 新界面文案 | i18n | `cargo test -p duck-theme` |
| 9 | `crates/duck-tools/src/meta.rs` | `group` / `beta` 透传到 `ToolMetaBuilder` | 侧栏分组 | 注册表单测 |
| 10 | `THIRD_PARTY_LICENSES.md` | 新依赖（`zip`/`quick-xml`/`calamine`… 在子进程 crate 内） | 合规双门禁 | 重新生成并提交 |

⚠️ 已在工作区未提交的改动较多：本清单落地时逐文件反向 `Edit`，不 `git checkout --`；`cargo fmt`/`clippy` 按单文件跑。

---

## 8. 迁移方法论：fixture 先行

源仓几乎无单测（`#[cfg(test)]` 命中极少），**没有行为基线就无法判断「复刻对不对」**。每个工具迁前必须做三步：

1. **固化输入**：从真实交付场景取 3–5 组脱敏样例（SP 包、PDM、OFD、SQL 脚本…），放 `plugins/<pkg>/fixtures/<tool>/case-XX/`。
2. **生成期望输出**：在源仓跑一遍（临时给 `main.rs` 加 CLI 子命令，或直接 `cargo test` 调内部函数），把结果固化为 `expected.json`。
3. **差分测试**：迁移实现读同一 fixture，逐字段比对；不一致即失败，不做「差不多」。

对 L1（纯函数）可用属性测试替代部分 fixture（幂等性、往返性：`parse(format(x)) == parse(x)`）。

---

## 9. 里程碑与估工

> 权威版本在 **`ifm-plugin-port-plan.md` §5**（含出口标准、并行关系与排期）。本表只留结论：

| 里程碑 | 内容 | 估工 |
| --- | --- | --- |
| **M0 框架** | W1–W6（`[io]` schema、执行页、process 轨、文件授权、KV、审计/打包/签名） | **30 人日** |
| **M1 纯 JS 首包** | `devkit.text`（10 工具 / 9 界面） | 6 人日 |
| **M2 process 落地** | `devkit.codec`（2，纯 JS）+ `devkit.sql`（7 工具 / 5 界面，首个 process 包） | 14 人日 |
| **M3 高价值包** | `ifm.merge`（4 工具 / 2 界面）+ `ifm.compare`（6 工具 / 4 界面） | 26 人日 |
| **M4 文件与网络** | `devkit.files`（9 / 5）+ `devkit.net`（6 / 4） | 21 人日 |
| **M5 收尾** | `devkit.misc`（2）+ `devkit.config`（2）+ `ifm.finance`（2） | 17 人日 |
| 合计 | 10 包 / 50 工具 / 37 界面 | **114 人日** |

依赖顺序：M0 是一切前提；`duck-plugin-process` 必须在第一个 L2 包（`devkit.sql`）之前完成，否则会退化成「把逻辑塞回宿主」。
`protobuf-codec` 已按核对结果移出，process 协议的首个真机验证改在 `devkit.sql` 的 `script-check`（只读目录 → 问题表）。

---

## 10. 风险与缓解

| # | 风险 | 触发条件 | 缓解 |
| --- | --- | --- | --- |
| R1 | 无测试基线导致「迁完看起来一样、实际不同」 | 直接拷逻辑 | §8 fixture 差分；先固基线再写代码 |
| R2 | MSRV 落差（源 1.93 / 目标 1.85） | 依赖或语法用到 1.86+ 新特性 | 迁入前 `cargo +1.85 check`；依赖逐个过 `deny.toml` 许可门禁 |
| R3 | Tauri 类型渗透 | 拷贝带 `AppHandle`/`Emitter` 的函数 | 已核：`sp_merge.rs`/`see_merge.rs`/`jar_compare/`/`sql_merge.rs` 的 `tauri::command` 计数为 0，改造点集中在 `AppHandle + Emitter` → `trait ProgressSink` |
| R4 | UI 重写跑偏成「像素级复刻」 | 参照 Vue 页面做 GPUI | 断言：一个工具的执行页由 `[io]` 声明生成，**不允许**为单个工具写专属 GPUI 视图（除非 IO 表达不了，且需评审） |
| R5 | i18n 债务 | 几千条中文串 | 插件文案进 `<pkg>/locales/en.toml`；宿主只加界面文案；`python3 scripts/plugin_locales.py --strict` |
| R6 | 依赖膨胀 / 许可 | `calamine`、`pdf-extract`、`tera`、`ristretto_classfile`、`sm4` | 全部关在 process 子进程 crate 内，**不进宿主依赖图**；`THIRD_PARTY_LICENSES.md` 重生成 |
| R7 | 合规 | 恒生 / IFM 产品线协议、私有包结构、国密与自研 DES | 仅内部使用，**不进公开插件市场**；包仓库内部分发 |
| R8 | 高危动作 | 写 `/etc/hosts`、系统清理、桌面整理 | 不迁（清理/整理）或降级为「生成内容 + 用户手动应用」（hosts） |
| R9 | 子进程僵尸 / 残留 | 用户强退宿主 | 宿主退出时 `SIGTERM` 全部子进程；启动时按 pid 文件清扫 |

---

## 11. 不迁清单（38 个）

> 88 = 50 在册 + 38 不迁。不迁构成：**四域及运维衍生 16 + beta/dev 独有 12 + 核对时新删 7 + 另判 3**（`transform-hub` / `drawio-board` / `device-pool`）。
> 另有 3 项收窄（`data-format`、`random-gen`、`crypto-bundle`），仍在册，只是首版砍掉部分能力。
>
> **铁律：邮件 / 数据库 / Redis / SSH 四域不迁。** 这四域宿主已有 `duck-mail`、`duck-db`+`duck-drivers`+`duck-db-agent`、`duck-redis`、`duck-ssh`+`duck-term`，且已经把会话模型、凭据（`vault`）、取消、只读连接这些硬骨头啃完了。四域下的任何一层（含部署 / 集群 / 应用中心 / 日志 / 上线检查）都不重造。

| 工具 | 域 | 原因 |
| --- | --- | --- |
| 数据库管理 pro（7,641 行 Vue + `db_pro.rs` + `dbx-core`/`dbx-adapter`） | 数据库 | 宿主 `duck-db` + `duck-drivers` + `duck-db-agent` 已覆盖，含方言元信息、取消、只读连接 |
| Redis 管理（1,002 行） | Redis | 宿主 `duck-redis`（`PendingAction` 破坏性确认） |
| SSH 终端（2,337 行 Vue + `ssh/`） | SSH | 宿主 `duck-term` / `duck-ssh`（分屏、SFTP、ControlMaster） |
| 邮件客户端（8,612 行 Vue + 29,771 行 `email-core` + 8,547 行 `email-ai`） | 邮件 | 宿主 `duck-mail` |
| 代码部署 `code-deploy`（1,283 行 Vue + `deploy/`） | SSH | 架在 SSH + 远端备份/回滚之上，与宿主机密会话模型重复 |
| 自动化部署 `deploy-plugin`（511 行 + `deploy/`） | SSH | 同上（中间件安装脚本） |
| 应用中心 `app-center`（1,111 行 + `deploy/`） | SSH | 同上（微服务启停） |
| 集群管理 `cluster-manager`（16 行 + `cluster/` 1,958） | SSH | 同上 |
| 集群日志 `cluster-log-viewer`（1,625 行 + `log_agent/`） | SSH | 同上（远端 agent 推送） |
| 上线检查清单 `deploy-checklist`（279 行） | SSH | 逻辑就是 `ssh_exec` 校验命令，宿主已有 SSH 执行能力 |
| 待办事项 `todo-center`（1,300 行） | 其他 | 宿主 `duck-project` |
| AI 助手 `ai-assistant`（634 行 + `ai/`） | 其他 | 宿主 `duck-ai` + `RightDock::Ai` |
| 应用启动器 `app-launcher`（579 行） | 其他 | 宿主 launcher / `toggle_launcher` |
| 授权码生成 `license-manager` | 其他 | 宿主有 `duck-update` 签名链与自身授权模型 |
| 系统清理 `system-cleaner`（2,290 行） | 高危 | 与「界面禁演示数据」冲突，误删不可逆 |
| 桌面整理 `desktop-organizer`（197 行） | 高危 | 高危文件移动 |

---

## 12. 待确认 → 已定（2026-10-18 核对）

| 议题 | 结论 |
| --- | --- |
| M0 是否先行 | **先做**。M0 不落地，后面所有工具只能读剪贴板 |
| `[io]` 字段种类 | 6 类参数 + 6 类输出够用；~~「多列可编辑」不做，改为「选文件 + 只读预览 + 导出」~~ → **2026-10-18 已补上**：`[io.output.cells]` 声明可编辑列（`text` / `select`），配合 `TableDoc.row_ids` 与 `params.__edits`（前置修改语义）实现「就地改 → 重跑」。见 §13 |
| `devkit.ops`（部署/集群/应用中心） | **不迁**（归入 SSH 域），只留 `nginx-generator` / `s3-manager` 组成 `devkit.config` |
| `device-pool` | **不迁**：源工具含 Cursor 账号自动注册、临时邮箱池、无限重置，属绕过厂商授权，合规上不做 |
| 核对时的 7 项裁撤 | Prompt 优化、Protobuf、存储估算、金融文件解析、敏感扫描、截图、Mermaid 渲染 |

剩余开放项（实施中再定，不阻塞）：`s3-manager` 源页面仅 17 行，开工前先核实是否空壳。

## 13. 可编辑结果表（2026-10-18 落地，补 §12 那条「不做」）

参考项目里 `ChannelGatewayFieldTable.vue`（22 个 select 逐行改）/ `DataFormat.vue` /
`CsvTableEditor.vue` 三处都是「表格里就地改」，而 `[io]` 原本只声明只读表格。补齐方式：

### 声明（插件侧）
```toml
[tools.io.output]
kind = "table"
fallback = "code"          # 表格解不出来时按代码渲染（保住 Java 源码的等宽）
columns = ["交易码", "方向", "字段", "类型", "必填", "含义"]

[[tools.io.output.cells]]  # 可编辑列；不写 = 整表只读
column = "类型"
kind = "select"
options = [{ value = "string", label = "string" }, { value = "number", label = "number" }]

[[tools.io.output.cells]]
column = "含义"
kind = "text"
```

### 数据（插件 → 宿主）
`body` 里除 `columns` / `rows` 再多一个 `row_ids`（与 `rows` 一一对应）。

**`row_ids` 不是可选的锦上添花**：没有它，宿主只认「第 3 行第 2 列」，而重跑后行序一变，
改动就落到别的行上。网关用的是 `交易码|方向|字段名`。

### 回传（宿主 → 插件）
点「应用 N 处改动」→ 宿主把脏格子序列化成 `params.__edits`：

```json
{"__edits": [{"row_id": "B100001|request|SerialNo", "column": "含义", "value": "流水号"}]}
```

**不开双向协议**：语义定成「前置修改」——插件收到后先落库，再照常执行原本的动作。
于是「改几格 → 重跑」用的还是同一个动作（网关的 `bank-load` 就是「改完读回来看」），
宿主不需要 v3 才有的 dispatch 能力，插件也不必保存会话状态。

### 宿主侧实现（三处）
| 位置 | 改动 |
|---|---|
| `duck-plugin/src/io.rs` | `OutputSpec.cells` / `fallback`、`CellSpec`、`CellKind`；装载期校验「声明了就得能兑现」（非 table 不许有 cells、列名必须在 columns 里、select 必须有 options） |
| `duck-plugin/src/table.rs` | `TableDoc.row_ids` + `row_id(i)`、`CellEdit`、`EDIT_PARAM` |
| `duck-app/src/pages/tool_table.rs` | 只读 / 可编辑两种形态的渲染 + 编辑状态（从 `tool_run.rs` 搬出，避免把 v3 要压到 400 行的文件再撑大） |

⚠️ 编辑控件要 `window` 才建得出来，而结果是从后台回调里落的（那里没有窗口）
⇒ 在 `Render::render` 入口懒建，不在 `run` 的完成回调里建。

### 一条硬规则：没有 `row_ids` 就不给输入框

宿主侧（`ToolRunView::ensure_table_editor`）看到「声明了 cells 但表里一个 `row_id` 都没有」
时**不建编辑器**。理由不是保守，而是：

- 编辑的价值是「改完之后它还在」。行序一变，「第 3 行」就换了人，改动落到别的行上比不给改更糟。
- 插件不给 `row_ids` ＝ 说「这张表不打算被编辑」。典型例子是 Excel 解析结果：它没有可落库的
  去处，改了下次重跑就没了。

所以网关的两张表分工是：**`parse`（Excel 来源）只读**、**`bank-load`（库来源）可编辑**。
想改字段就先「存入接口库」，再在读回的表里改——这也正是参考仓「先导入、再反复生成」的路径。

### 另一个静默陷阱：宿主塞的默认值不算改动

`select` 格遇到**空值**时会显示成第一个候选（空下拉框等于告诉用户「没得选」），但这只是
显示用的默认值。`Slot::Select` 因此带一个 `injected` 标志：它仍为真时，`edits()` **跳过这一格**。
少了这个标志，用户什么都没点，「应用」就会把一个本来为空的字段悄悄写成 `string`。
