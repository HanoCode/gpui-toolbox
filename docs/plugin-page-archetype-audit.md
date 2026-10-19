# 插件页「形态」逐页审查（2026-10-18）

> **前情**：`docs/ifm-plugin-gap-audit.md` 核的是**功能入口**（参数面），结论是 34 个工具里
> 26 个参数已对齐，缺口集中在 Hosts 管理 + HTTP / SQL 合并 / Class 比对。
>
> **本轮**核的是**页面形态**（容器面）。触发：SP 包合并页与原版 `SpMerge.vue` 布局
> 完全不同（用户截图）。`pkg-merge` 已按原版复刻成 job 布局，其余 42 个页面这轮逐个过。
>
> 参考仓：`/Users/bws/code/code/github/ifm_ai_tools`（97 个工具页，本仓迁了 43 个）。

---

## 0. 一页速览

| 事实 | 结论 |
| --- | --- |
| 原版 97 页只有 **5 类原型**（A 单列 / B 左右双栏 / C 多 Tab+表格 / D 长任务 / E 设置页），另有本轮新增的 **F 工作台**（原 `ifm-ui-layout-analysis.md` 未覆盖） | 页面不是 97 种设计，是 6 种骨架 |
| 本仓只有 **2 个渲染分支**：默认页（参数横排条 + 左输入右结果）、job 页（分组配置 + 进度/统计 + 右日志） | 6 种骨架里只实现了 2 种 |
| 43 个插件分级：✅ 同形 5、◐ 同功能不同形 18、● 形态缺失 20 | 差异是**容器级**，不是逐页手写 |
| 20 个 ● 页面集中在 **6 项宿主能力**：模式页签、并排双栏、结果表、工作台、预览面板、页面级特例 | 补 6 项能力 ≈ 覆盖 20 页 |

一句话：**参数大多齐了（上一轮核过），缺的是「把这些参数装进原版那种容器」的能力。**

---

## 1. 判据与方法

1. 从原版每个 `.vue` 的 `<template>` 抽骨架（章节标题、控件、按钮、面板、grid 列数），
   **不看行数看容器**——`RandomTextGenerator.vue` 1417 行只是 6 个同构卡片。
2. 三条判据：
   - **输入区**：几个输入、并排还是堆叠、有没有「A/B」语义；
   - **模式**：页面内有没有页签/分段（多模式）——本仓一律压成 `select`；
   - **结果区**：纯文本 / 表 / 树 / 分组列表 / 日志 / 预览。
3. 分级：
   - ✅ **同形**：容器一致，只差细节；
   - ◐ **同功能不同形**：能力在、摆法不同（改渲染即可）；
   - ● **形态缺失**：原版是另一个容器（表 / 工作台 / 预览），现页面表达不了。

> 「现形态」说明：**默认页** = 参数横排条（`flex_wrap`）+ 运行/重置 + 左输入右结果；
> **job 页** = 垂直分组配置 + 进度/统计卡 + 右固定日志。目标列用第 0 节的 A–F 原型标记。

---

## 2. 逐页对照（43 个插件）

### 2.1 文本族（10）

| # | 插件 | 原版页（原型） | 原版形态要点 | 现形态 | 差距 | 目标 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | text-timestamp | Timestamp（145L，A+） | 秒/毫秒/日期**三框联动** + 格式化展示网格 + 三个「复制」 | 默认页：单输入 + 方向 + 单位 | 三框联动退化成单框；无复制三键 | A+ |
| 2 | text-case-convert | CaseConvert（32L，A） | 一框输入，**六种风格同时**列在结果卡 | 默认页：选风格 → 单结果 | 一次只能看一种风格 | A |
| 3 | text-number-to-chinese | NumberToChinese（208L，A） | 双结果（人民币 / 纯数字）+ 常用示例 | 默认页：选风格 → 单结果 | 缺并列双结果与示例 | A |
| 4 | text-regex-plus | RegexPlus（99L，A+） | 正则条在输入上方、flags 复选、匹配列表（分组 + 耗时） | 默认页：pattern/flags/replacement 进参数条 | 正则条位置错位；匹配列表压成文本 | A+ |
| 5 | text-formatter | Formatter（295L，B） | 类型按钮组 + 输入/输出 + 复制（+ JSONPath 查询） | 默认页 | 少 JSONPath 查询；类型按钮组变下拉 | B ✅ |
| 6 | text-cron-gen | CronGen（162L，B+） | 分/时/日/月/周**五段** + 预设列表 + 自然语言 + 未来执行时间 | 默认页：preset/hour/minute/weekday | 缺「日/月」两段；缺「未来执行时间」列表 | B+ |
| 7 | text-random-gen | RandomTextGenerator（1417L，A×6） | 五个生成器**卡片并列** + 客户信息 CRS + 结果列表 | 默认页：category 下拉 + 13 参数 | 六类折叠成一个下拉；无结果列表 | F（轻） |
| 8 | text-data-format | DataFormat（1449L，C） | 三段 Tab + **可编辑表格** + 输出格式 | 默认页：mode 下拉 + 17 参数 | 无 Tab 分段、无表格预览 | C |
| 9 | text-cheat-sheets | CheatSheets（363L，F） | 搜索 + 左列表 + 右详情 + 变量逐个填充 | 默认页：sheet/keyword/vars/view/dir | 无列表/详情；变量变一行 `k=v` | F |
| 10 | text-diff | TextDiff（416L，B） | A/B 并排 + 统计 `+N -M ~K` + **B→A / A→B 合并** + 复制差异 | 默认页：两输入 + diff 双栏结果 | 并排变上下；缺两个合并按钮 | B ✅（补按钮） |

### 2.2 编码族（2）

| # | 插件 | 原版页（原型） | 原版形态要点 | 现形态 | 差距 | 目标 |
| --- | --- | --- | --- | --- | --- | --- |
| 11 | codec-crypto-bundle | CryptoBundle（680L，C） | **9 个功能 Tab**，每 Tab 一块面板 | 默认页：mode 下拉 + 23 参数（显隐靠 `visible_when`） | Tab 变下拉；参数横排条同屏挤 6–8 个 | C |
| 12 | codec-encoding-converter | EncodingConverter（259L，B） | 双栏对照 + 交换 + 「转换 →」 | 默认页：单输入 + 方向 + 编码 | 无双栏、无交换 | B |

### 2.3 比对族（4）

| # | 插件 | 原版页（原型） | 原版形态要点 | 现形态 | 差距 | 目标 |
| --- | --- | --- | --- | --- | --- | --- |
| 13 | config-compare | ConfigDiff（213L，B） | 两框**并排** + 交换 | 默认页：两输入上下堆叠 | 并排变堆叠；无交换 | B |
| 14 | dir-compare | DirCompare（667L，F） | 会话 Tab + 双目录 + ◀ ⇔ ▶ 导航 + 双树 | 默认页：2 目录 + 4 参数 → 表/HTML 文本 | 无会话、无双树；导航缺失 | F |
| 15 | class-compare | ClassCompare（630L，D） | 配置 + 进度 + **统计筛选条** + 日志/结果 Tab + 结果表 + CSV/Excel 导出 | 默认页：6 参数 → 文本 | 结果表 → 文本；无统计条/日志 Tab/导出 | D + 表 |
| 16 | jar-compare | JarCompare（510L，D） | 同 ClassCompare（同一套模板） | 默认页 | 同上 | D + 表 |

### 2.4 文件族（5）

| # | 插件 | 原版页（原型） | 原版形态要点 | 现形态 | 差距 | 目标 |
| --- | --- | --- | --- | --- | --- | --- |
| 17 | files-search | FileSearch（139L，A+） | 目录/模式/过滤 + 结果**按文件分组** + 全词匹配 | 默认页：9 参数 → 文本 | 结果应为分组列表；**缺「全词匹配」** | A + 列表 |
| 18 | files-batch-edit | BatchReplace（201L）/ BatchRenamer（248L），F | 输入即搜索 + 按文件分组预览 + 「替换全部 (N)」 | 默认页：10 参数 + `action=preview/apply/undo` | 无即时预览列表，靠先跑 preview | F（预览） |
| 19 | files-tree | DirectoryTree（331L，A+） | canvas 图形树 + 图标开关 + 导出 PNG | 默认页：6 参数 → 等宽文本树 | 图形树 → 文本；无 PNG 导出 | A+（预览） |
| 20 | files-csv-tools | CsvTableEditor（212L）/ CsvToChart（286L），C | 可编辑表格（复制 MD/JSON/INSERT）+ 画布图表 + 图表配置 | 默认页：mode 下拉 + 参数 → 文本 | 不可编辑；图表只落 SVG 文件、页面看不到 | C（grid+预览） |
| 21 | files-doc-convert | DocConverter（425L，F） | 左配置 + **右预览** + 拖拽 + 保存文件 | 默认页：6 参数 → 文本 | 无预览、无拖拽、无「保存」动作 | F（预览） |

### 2.5 SQL 族（5）

| # | 插件 | 原版页（原型） | 原版形态要点 | 现形态 | 差距 | 目标 |
| --- | --- | --- | --- | --- | --- | --- |
| 22 | sql-script-check | SQLChecker（408L，C/D） | 工具栏（打开/导出/导出 MD）+ 编辑器拖放 + **问题列表**（L 行号 + 级别色） | 默认页：textarea + db_type → 文本 | 问题列表 → 文本；无导出 | C + 表 |
| 23 | sql-rewrite | SqlReExec（1092L，B）/ SqlReplace（364L） | 三种**输入模式**（粘贴/文件/目录）+ 选项 + 输出 + 保存 | 默认页：textarea + 4 处理方式 | 只有粘贴；无文件/目录输入、无保存 | B |
| 24 | sql-merge | SqlMerge（619L，D） | 左配置 + 进度 + 合并统计 + **分组详情表** + 右日志 | 默认页：3 参数 → 文本 | **未接 job**；统计/分组表没画 | D + 表 |
| 25 | sql-classifier | SqlClassifier（434L，F） | 目录列表（增删 + 文件数）+ 开始抽取 + 结果统计 | 默认页：2 目录 → 文本 | 多目录管理缺失 | F（轻） |
| 26 | sql-ddl-generator | DdlGenerator（255L，B） | 输入 + 参数行 + 两个生成按钮 + 高级折叠 + 输出 | 默认页（mode 覆盖两按钮） | 基本对齐，只差高级设置折叠 | B ✅ |

### 2.6 网络 / 系统族（5）

| # | 插件 | 原版页（原型） | 原版形态要点 | 现形态 | 差距 | 目标 |
| --- | --- | --- | --- | --- | --- | --- |
| 27 | net-http-request | HttpRequest（1518L，F） | Postman 式：环境栏 + 方法/URL + **Params/Auth/Headers/Body/Response 五 Tab** + 历史 + 集合 + 导入 cURL | 默认页：12 参数 + body → 文本 | Tab / 历史 / 集合全无；响应头与 Cookie 不可分栏 | F（大） |
| 28 | net-packet-send | SoapRequest（629L）/ XmlSend（584L）/ TcpRequest（651L）/ FixSend（612L），B | **4 个独立工具**，每个双栏（报文 / 应答）+ 通信日志侧栏 + 格式化/校验和/插入报文头 | 默认页：protocol 下拉 + 13 参数 | 四工具挤成一个；缺应答/日志分栏与格式化按钮 | C + F |
| 29 | net-hosts-manager | HostsManager（621L，F） | 三栏工作台：方案列表 / 编辑器 / 聚合预览 + 校验 + 同步 + 留痕 + 回收站 | 默认页：5 参数 → 文本 | 编辑器/同步/审计全无 | F |
| 30 | net-s3-manager | S3Manager → `S3View.vue`（1583L，F） | 完整对象浏览器（桶/前缀/上传下载/右键菜单） | 默认页：action + 14 参数 | 浏览器 → 命令行式动作 | F |
| 31 | net-s3-ftp-checker | S3FtpChecker（269L，C） | 协议 Tab + 各协议独立表单 + 检测结果卡 | 默认页：protocol 下拉 + 12 参数 | Tab → 下拉；结果卡 → 文本 | C |

### 2.7 金融 / 杂项（5）

| # | 插件 | 原版页（原型） | 原版形态要点 | 现形态 | 差距 | 目标 |
| --- | --- | --- | --- | --- | --- | --- |
| 32 | misc-git-tools | GitTools（128L，A） | diff 输入 → commit 信息 / .gitignore | 默认页（mode 覆盖） | 形态可接受 | A ✅ |
| 33 | misc-json-schema | JsonSchemaValidator（197L，B） | 双栏编辑器 + 实时校验 + 错误列表 + 格式化 | 默认页：两 textarea → 文本 | 并排变堆叠；无实时校验；错误列表压平 | B + 表 |
| 34 | misc-nginx | NginxGenerator（476L，F） | 左**表单构建器**（server/upstream/location）+ 右配置 + 复制/下载 | 默认页：JSON 覆盖 + 预设 → 配置代码 | 表单构建器退化为手写 JSON；无下载 | F |
| 35 | finance-channel-gateway | ChannelGateway（1088L，F） | 6 Tab + **列映射表**（逐列下拉）+ 接口树 + 入库 | 默认页：action + 13 参数 | 映射表/接口树缺失；映射靠「第几列」数字 | F |
| 36 | finance-pdm-reader | PdmReader（671L，F） | 文件 Tab + 表列表 + 字段表 + 5 种格式导出 | 默认页：action + 5 参数 → 代码 | 浏览态缺失，逐字段联查要跑两次 | F |

### 2.8 合并族（7）

| # | 插件 | 原版页（原型） | 原版形态要点 | 现形态 | 差距 | 目标 |
| --- | --- | --- | --- | --- | --- | --- |
| 37 | pkg-merge | SpMerge（714L，D） | 组配置 + pills + 版本范围/待合并列表（只读）+ 统计卡 + 右日志 | job 页 | **已对齐（上轮）** | ✅ |
| 38 | non-sp-merge | NonSpMerge（617L，D） | 需求单范围 / 待合并包列表（只读）+ 2 开关 + 4 格统计 + 右日志 | job 页（骨架） | 缺 3 个 display + 扫描；统计不全 | D+ |
| 39 | non-see-merge | （无独立页；Ifms6Merge 的非 SEE 分支） | 同 ifms6（少 SQL 合并） | job 页 | 同 ifms6 裁剪版 | D |
| 40 | see-merge | SeeMerge（538L，D） | 高级选项（Slim / 个性化）+ 客户编号 + 版本范围 + 3 格统计 + 输出 | job 页 | 参数比原版多（db_type/版本）却缺范围过滤；统计 3 格缺 | D+ |
| 41 | ifms6-merge | Ifms6Merge（354L，D） | 客户名称 + 版本范围 + 待合并列表 + SQL 合并 + 6 格统计 | job 页 | 缺客户名称/待合并列表/6 格统计 | D+ |
| 42 | scc-merge | SccMerge（132L，D） | 11 参数 + 单行统计（包数｜服务｜跨基线｜耗时） | job 页 | 参数齐；统计只 1 行 | D+ |
| 43 | merge-manifest-extract | ExtractByManifest（244L，D） | 3 路径 + 进度 + 总计/耗时 + **未找到文件清单** + 日志 | job 页 | 缺「未找到」清单与统计卡 | D+ |

**小结**：✅ 5（pkg-merge / text-formatter / text-diff / sql-ddl-generator / misc-git-tools）、
◐ 18、● 20。

---

## 3. 按「缺失能力」聚类（这才是施工单）

### 3.1 模式页签（Tab）→ 9 个页面，收益最高

| 插件 | 原版 Tab | 现渲染 |
| --- | --- | --- |
| codec-crypto-bundle | 9 | `mode` 下拉 + 23 参数（显隐已靠 `visible_when`） |
| net-packet-send | 4（SOAP/TCP/XML/FIX，本是 4 个工具） | `protocol` 下拉 |
| text-data-format | 3（CSV/定长/分隔符） | `mode` 下拉 |
| net-s3-ftp-checker | 3（S3/FTP/SFTP） | `protocol` 下拉 |
| finance-pdm-reader | 2–3（动作/方言） | `action` 下拉 |
| finance-channel-gateway | 3（已迁的部分） | `action` 下拉 |
| files-csv-tools | 2（表格/图表） | `mode` 下拉 |
| files-doc-convert | 2（文件/目录） | `mode` 下拉 |
| sql-rewrite | 4（替换/优化/清理/备份） | `mode` 下拉 |

**做法**：不需要新参数——`select` 已经是「当前模式」，只是改成**顶部 pill 页签**渲染，
并把同一模式下可见的参数聚到一块面板。改渲染层 + 清单标 `tabs = true` 即可。

### 3.2 并排双栏 + 交换 → 4 个页面

`config-compare`、`codec-encoding-converter`、`misc-json-schema`、`text-diff`
（外加 `files-doc-convert` 左配置/右预览、`misc-nginx` 左表单/右输出）。

**做法**：`fields` 支持 `layout = "row"`（并排，各 `flex_1`）+ 可选 `swap`。

### 3.3 结果表（`OutputKind::Table` 落地）→ 11 个页面

| 现状 | 数据是否现成 |
| --- | --- |
| `dir-compare` / `config-compare` 已在工具里拼 **Markdown 表**字符串 | 现成（`DirDiffEntry` / `Entry`） |
| `class-compare` / `jar-compare` 输出文本报告 | 现成（比对结果结构） |
| `files-search` 文本命中列表 | 现成 |
| `sql-script-check` 文本问题列表 | 现成（`CheckReport`） |
| `text-case-convert` / `text-number-to-chinese` / `text-regex-plus` 单串结果 | 现成（多值） |
| `sql-merge` 分组详情、`merge-manifest-extract` 未找到清单 | 现成 |

✅ **2026-10-18 已接线**：`TableDoc::decode(body)` → Markdown 回退 → `pages::tool_table::render_table`，
并按 `[io.output.cells]` 支持**就地编辑**（`row_ids` + `params.__edits`，见
`docs/ifm-plugin-port-design.md` §13）。`OutputKind::Tree` 仍未接（本表没有工具要树）。
渲染没有沿用 `duck_components::data_table`：插件结果通常几十行，虚拟滚动 / 列宽拖拽的收益
抵不过「表头与行几何两处各写一遍」的成本，所以 `tool_table` 自己画表壳。

### 3.4 工作台（左列表/导航 + 右详情）→ 8 个页面

`net-hosts-manager`、`net-s3-manager`、`net-http-request`、`finance-pdm-reader`、
`finance-channel-gateway`、`text-cheat-sheets`、`files-csv-tools`、`dir-compare`。

这一档是「原版本身是一个应用」，逐个复刻成本最高。建议**只做骨架**（左列表 + 右详情 + 顶部工具条），
不追历史 / 回收站 / 审计这些应用级功能。

### 3.5 预览面板 → 3 个页面

- `files-doc-convert`：转换结果预览（原版右栏直接渲染）
- `files-csv-tools`：图表内联（现在只写 SVG 文件）
- `files-tree`：图形树 / 导出 PNG（现在是等宽文本）

宿主没有「图片 / SVG 预览」输出形状，需要新增 `kind = "preview"`（文件路径 → 渲染）。

### 3.6 页面级特例 → 2 个页面

- `text-timestamp`：三框联动（秒/毫秒/日期），需要「联动输入组」控件；
- `text-data-format` / `files-csv-tools`：**可编辑表格**（原版每格一个 input）——
  这是唯一真正要新造控件的地方（`data_table` 目前是只读展示 + 选区）。

---

## 4. 顺带发现的「功能」缺口（不是形态问题）

| 插件 | 缺什么 | 原版位置 |
| --- | --- | --- |
| ~~files-search~~ | ✅ **已补（2026-10-18）** 全词匹配：`whole_word` 开关 + 后端套 `\b(?:…)\b` | FileSearch.vue 复选框 |
| ~~text-cron-gen~~ | ✅ **已补（2026-10-18）** 日 / 月 两段 + 每月/每年模板 + 未来 10 次执行时间 | CronGen.vue `grid-cols-5` |
| ~~text-case-convert~~ | ✅ 六种风格并列（`OutputKind::Table`） | CaseConvert.vue 结果卡 |
| ~~text-number-to-chinese~~ | ✅ 双结果 + 常用示例 | NumberToChinese.vue |
| sql-rewrite | 文件 / 目录输入（现只有粘贴）+ 保存 | SqlReExec.vue 三种输入模式 |
| net-packet-send | 格式化 / 插入报文头 / 校验和 按钮 | Xml / Tcp / FixSend.vue |
| ~~全部结果区~~ | ✅ 复制 / 导出（表格→CSV，其余→Markdown）/ 保存 三键 | ClassCompare / SQLChecker / NginxGenerator / DocConverter |

---

## 5. 建议落地顺序

| 阶段 | 内容 | 覆盖 | 风险 |
| --- | --- | --- | --- |
| **P0** | 模式页签渲染（`select` → pill tabs + 面板聚合） | 9 页 | 纯渲染层，不碰协议 |
| **P1** | ~~`OutputKind::Table` 接线~~ ✅ 已完成（含可编辑）；剩「导出 CSV/MD」按钮与各工具补 body 输出 | 11 页 | 各工具要补一次 body 输出 |
| **P2** | `fields` 并排 + `swap`；结果区「保存 / 下载」动作 | 5 页 | 小 |
| **P3** | merge 族补 display / scan / 统计（对齐 pkg-merge 口径） | 5 页 | 已有 scan 范式，照抄 |
| **P4** | 工作台骨架（左列表 + 右详情） | 8 页 | 工作量最大，需逐页定交互 |
| **P5** | 预览面板（doc / SVG / 树 PNG）、~~可编辑表格~~（✅ 框架已支持，剩各插件声明）、时间戳三联动 | 4 页 | 新控件 |

---

## 6. 需要你定的三件事

1. **P0 / P1 一起做，还是先只做 P0 看效果？**（P0 无协议改动；P1 要给 11 个工具各补一次 body。）
2. **工作台型 8 个页面**：接受「精简版骨架」（左列表 + 右详情，不做历史 / 回收站），还是要逐个全量复刻？
3. **结果区动作**：先统一补「导出 CSV / 导出 Markdown / 保存到文件」三个动作，够不够？

---

## 7. 落地进度（全量复刻，按能力推进）

| 能力 | 状态 | 落地位置 | 已接入页面 |
| --- | --- | --- | --- |
| **模式页签**（原型 C 的容器） | ✅ 宿主能力 + 清单接线 | `ParamSpec.style = "tabs"` → `duck_components::segmented_tabs`（通用页 / job 页顶部） | codec-crypto-bundle、text-data-format、net-s3-ftp-checker、files-csv-tools、files-doc-convert、sql-rewrite、files-search、files-batch-edit、misc-git-tools、net-packet-send、text-random-gen（11 个） |
| **并排双栏 + 交换**（原型 B） | ✅ 宿主能力 + 清单接线 | `[io] fields_layout = "row"` / `swap_fields = true` | config-compare、misc-json-schema、text-diff（3 个） |
| **结果表**（`OutputKind::Table`） | ✅ 宿主能力 + 五处数据源 | `duck_plugin::TableDoc`（body `{columns,rows}` / Markdown 表回退）+ 宿主表壳（表头 / 斑马纹 / 滚动）；`duck-app/src/pages/tool_table.rs` 另有可编辑形态 | text-case-convert（六风格）、text-number-to-chinese（双读法）、sql-script-check（问题清单）、dir-compare（差异清单，Markdown 回退）、config-compare（配置差异，Markdown 回退） |
| **结果动作**（复制 / 导出 / 保存） | ✅ 通用页 | 结果栏头三键：复制、导出（表格 → CSV，其余 → Markdown）、保存（`file_dialogs` 异步面板） | 全部通用页 |
| **预览面板**（原型 F 的右栏） | ✅ 宿主能力 + 一处接线 | `OutputKind::Preview`：body `{"preview": "<已落盘文件路径>"}`，宿主按扩展名走 GPUI 图片管线内联（PNG/JPEG/GIF/WebP/BMP 光栅 + SVG 走内置 resvg 光栅化，全彩）；解不出按 `fallback`（缺省 `text`）退回 | files-csv-tools（chart 模式 SVG 内联）；files-tree PNG / doc-convert 待后端产出图片后零改动接入 |
| 长任务页（原型 D） | ✅（上一轮） | `layout = "job"` | 7 个合并族插件 |
| **P3 merge 族补齐** | ✅（2026-10-19） | `page_scan` 扩展 + 统计卡补格 | ifms6-merge / non-see-merge：scan + 「待合并版本列表」display + 分块统计（前端/服务端/数据库[/ifmcounter/spconfig]）；scc-merge：跨大版本统计；manifest-extract 统计卡已有（未找到清单在结果正文） |
| 工作台骨架（原型 F） | ✅ 骨架（2026-10-19） | `layout = "workbench"` + `[io.workbench] target`：scan（body `list`）喂左列表，点中回填 target 参数，右侧 = 通用执行区；首帧自动扫描 | text-cheat-sheets（速查表列表）；hosts / S3 / HTTP / PDM / 渠道网关 / CSV / 目录比较待各插件补 scan 后照抄接线 |

**已知取舍**：

- 页签只取**一个** `select`（装载期校验「至多一个 `style = "tabs"`」）：页面顶部只有一行，
  两个页签组会让人不知道哪个在控制哪块面板。
- 表格有**两条供给**：process 轨发 `body`（首选），JS 轨没有 body 通道 → 输出 Markdown 表，
  宿主用 `TableDoc::from_markdown` 解析（`text-case-convert` 走的就是这条）。
  两条都不成立时退回纯文本，不当失败。
- 预览是**读盘不是传字节**：`{"preview": "<路径>"}` 必须指向插件已写盘的文件
  （几 MB 图片塞 NDJSON 又慢又占内存）；扩展名不认识 / 文件缺失退回 `fallback`，
  绝不给用户一个裂图。
- 工作台只做**骨架**（左列表 + 右完整执行区），历史 / 回收站 / 审计不在契约里；
  列表来自同一条 scan 通道（body `list`），点列表 = 回填 target 参数——「列表里有的
  永远可以打开」，不存在列表与运行两套数据源。
- 导出/保存的内容在**打开面板之前**固化（面板是异步的，用户可能停很久）。
- non-sp-merge 的扫描做不了：内核 `non_sp_merge` 没有 `scan_versions`（清单头注释有记），
  维持「留空 = 全部」的执行语义。
