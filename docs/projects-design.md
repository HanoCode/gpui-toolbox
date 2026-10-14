# 项目管理模块设计（`projects`）

> 状态：**S1 + S2 + S3-I + S3-II + S3-III 已落地**（内核 crate `crates/duck-project` 103 单测；`duck-app` 侧 `pages/projects` 三件套（state / view / mod）+ 归档渲染 `archive.rs` + 偏好 `prefs.rs` + **7 个**页面私有组件，`duck-app` 项目管理页 55 单测全绿，`duck-app` 全量 289 单测通过）
> 六项拍板决议见 §8 · 对应路由 `PROJECTS_ROUTE = "projects"`（**已替换占位页**，见 §9 S2 记录）
> S3 拆三批：**S3-I（归档中心 + 撤销条 + 快捷键 + 偏好落盘）已落地**（见 §9 S3-I 记录）；**S3-II（列表视图 + 筛选 chips + 主区搜索 + 排序切换器）已落地**（见 §9 S3-II 记录）；**S3-III（以 `docs/projects-mock.html` 为真源对齐左栏 / 工具条 / 状态栏 / 卡片菜单 / 看板列）已落地**（见 §9 S3-III 记录）
> 参考实现：`temp/devAssistant/`（Vue 3 + Tauri 的任务管理）
> 配套 UI 原型：`docs/projects-mock.html`（完整可交互，全部弹窗可点通）；设计注记与取舍见 `docs/projects-ui-mockup.html`

---

## 0. 结论摘要

1. **形态定为「项目 → 任务」两级**，而非参考项目的「扁平任务」单级。理由：路由名为「项目管理」，且本项目壳层的心智模型本就是 *左栏列表 + 主区内容 + 右栏详情*（数据库 / Redis / SSH / AI 助手四页同构），单级任务列表会浪费已有信息架构。
2. **看板是「按状态分列」，四象限只是任务属性**。这一点直接照抄参考项目，不要自研"四象限网格"布局——参考项目 `TaskBoard.vue:1820-1827` 的网格是 `repeat(3,1fr)` 的三列状态看板，四象限仅用于筛选与卡片角标。
3. **新增内核 crate `duck-project`（零 gpui、全同步、SQLite）**，范式对齐既有的 `duck-mail` / `duck-ai`（两者都自带独立存储内核）。项目管理是结构化关系数据（任务 / 标签 / 里程碑多对多），塞进 `duck-store` 的 kv 单键 JSON 会立刻失控（`duck-store` 唯一的非 kv 表是 append-only 的 `audit`，没有通用关系表能力）。
4. **UI 组件优先复用 `duck-components` 现有构件**（`segmented_tabs` / `status_button` / `dropdown_select` / `dialog` / `notify` / `option_group`），真正需要新增的只有 6 个领域组件（看板列、任务卡、详情面板、进度条、标签片、优先级点）——**这 6 个按拍板结果落「页面私有」模块 `pages/projects/widgets/`，不进公共组件库**（详见 §4.2、§8 Q4）。⚠️ S3-III 后为 **7 个**（+ `task_menu`，看板卡片与列表行共用的右键菜单，详见 §9 S3-III）。
5. **AI 能力全部走既有 `duck-ai`**，不新建 AI 通路；无 AI 配置时功能降级为手动。
6. **归档（Archive）是一等能力**：任务与项目**均可归档**，归档后从所有常规视图与计数中消失，只在「归档中心」可见，可随时恢复（详见 §2.3）。
7. **悬浮窗（"随手看"）进首发**：独立无边框置顶小窗，随时瞥一眼进行中 / 待办（§4.3 ⑤），不再排到 M2。

---

## 1. 参考实现对照：抄什么、不抄什么

逐条核对 `temp/devAssistant/src/` 后，取舍如下。

| 参考项目做法 | 出处 | 本项目决策 | 理由 |
| --- | --- | --- | --- |
| 三列状态看板（待办/进行中/已完成） | `TaskBoard.vue:1820-1827` | **照抄** | 成熟布局；`grid: repeat(N,1fr) gap 24px` |
| 四象限作为属性 + 筛选 + 卡片角标 | `TaskBoard.vue:56-65`、`TaskCard.vue:9-24` | **照抄** | 不做四象限网格布局 |
| 卡片 hover 上浮 2px + 扫光 | `TaskCard.vue:326-345` | **照抄**（改用本项目 token） | 本项目约定：悬停一律强调色薄底 `hover_tint`，扫光层改用 `hover_tint` 渐变 |
| 里程碑带 `progressSnapshot`，创建时可推进任务进度 | `TaskDetailModal.vue:508-539` | **照抄** | 进度与里程碑耦合是有效设计 |
| 进度滑块 `min` 绑当前值，禁止回退 | `TaskDetailModal.vue:52-68` | **照抄** | 防误操作 |
| 四类日期职责分离（截止/登记/显示/计划开始） | `types/task.ts:15-18` | **简化采纳** | 只保留 `start_date` / `due_date` / `completed_at` 三个；"日历显示日期"改为由 `due_date` 派生（见 §5.3） |
| "未完成任务默认显示在今天"的日历归属规则 | `TaskCalendarApp.vue:669-698` | **采纳** | 让日历永远能看到待办，避免"任务丢了"的错觉 |
| 快速新建 `#标签 !高优` 本地正则解析 | `QuickTaskModal.vue:299-337` | **照抄并扩展** | 扩展为 `#标签 @项目 !P0 今天`；AI 仅作兜底 |
| 悬浮窗 300×550 无边框置顶 | `tauri.conf.json:73-90` | **采纳为 M1（首发）** | 与本项目多窗口能力匹配；拍板 Q5 定为"随手看"入口 |
| 工作日志三态（日报/周报/周计划） | `WorkLog.vue:10-14` | **采纳为 M2** | 与 AI 助手联动价值高 |
| 报表六 tab（任务/统计/时间线/浏览/截图/番茄钟） | `ReportCenter.vue:6-55` | **裁剪为 M2** | 只保留任务报表与统计；截图/番茄钟不在本模块范围 |
| AI 预测任务（后台定时 + 屏幕上下文） | `main.rs:565-658` | **不采纳** | 依赖截屏采集与 VLM，属独立能力域；本项目不引入屏幕采集 |
| 分类 = 固定枚举（后端/数据库/文档…） | `migrations.rs:1059-1065` | **改为用户可定义** | 本项目已有"项目"维度承担此职责，标签自由定义 |
| 拖拽排序 | 文档声称支持，实现没有 | **自研（M1 需要）** | 看板拖拽是核心交互，必须实现；参考项目这块是与其文档不符的缺口 |
| 看板无搜索框 | `TaskBoard.vue:48-102` | **补上** | 复用顶栏搜索 + 工具条筛选 chips |
| 优先级色点与文字映射相反 | `TaskCard.vue:288-295` vs `:527-540` | **修正** | 统一为 P0 红 / P1 橙 / P2 蓝 / P3 灰 |

---

## 2. 领域模型

### 2.1 实体

```
Group（分组）1 ── n Project（项目）1 ── n Task（任务）1 ── n Task（子任务，自引用）
                                        │                └─ n Milestone（里程碑）
                                        └─ n TaskTag ── Tag
```

| 实体 | 字段 | 说明 |
| --- | --- | --- |
| **Group** | `id` `name` `icon` `sort_order` `collapsed` `created_at` | 左栏一级节点（如"工作""个人"） |
| **Project** | `id` `group_id` `name` `description` `color` `icon` `status` `start_date` `due_date` `sort_order` `created_at` `updated_at` `archived_at` | `status ∈ {active, paused}`（**无 `archived`**，归档是与状态正交的 `archived_at`）；颜色用于左栏圆点与卡片左侧色条 |
| **Task** | `id` `project_id` `parent_id` `title` `description_md` `status` `priority` `quadrant` `tags` `start_date` `due_date` `due_time` `completed_at` `progress` `estimate_hours` `actual_hours` `sort_order` `created_at` `updated_at` `archived_at` `archived_root` | `status ∈ {todo, doing, blocked, done, deferred, cancelled}`；`priority ∈ {P0..P3}`；`quadrant ∈ {Q1..Q4}`；`archived_at` 非空即已归档；`archived_root` 是归档级联键（见 §2.3） |
| **Milestone** | `id` `task_id` `project_id` `title` `description` `due_date` `progress_snapshot` `reached_at` `created_at` | `task_id` 为空表示挂在项目上的项目里程碑 |
| **Tag** | `id` `name` `color` | 全局共享，跨项目复用；**使用次数现算**（不缓存，见 §9） |
| **BoardColumn** | `id` `name` `status_key` `sort_order` `wip_limit` | 表为空 = 用内置默认四列；`name` 为 NULL = 用内置默认文案；`status_key` 映射到内置状态之一 |
| **WorkLog** | `id` `date` `kind` `content_md` `ai_generated` `linked_task_ids` | `kind ∈ {daily, weekly, plan}`；M2 |

**字段设计说明**

- **不使用**参考项目的 `registeredAt` / `displayDate` / `scheduledStartTime` 四日期制：这四者的差异源于其日历与提醒的耦合实现。本项目改为「`start_date`（含时刻，用于排期与提醒）+ `due_date`（日期）」两值，日历归属由规则派生（§5.3）。
- `progress` 与里程碑解耦但联动：创建里程碑时若其 `progress_snapshot` 高于任务当前进度，则同步推进（照抄参考项目 `TaskDetailModal.vue:508-539`）。
- `parent_id` 支持一层子任务（不递归展开为无限层级，避免树型复杂度）。

### 2.2 状态机

```
          ┌──────────── reopen ────────────┐
          ▼                                │
  ┌───┐ start ┌───────┐ complete ┌──────┐ │
  │todo│─────▶│ doing │─────────▶│ done │─┘
  └───┘      └───────┘          └──────┘
    │  ▲          │  ▲
    │  └─ unblock ─┘  │
    │      ┌─────────┐│
    └─────▶│blocked  ││
  defer    └─────────┘
    │
    ▼
 ┌──────────┐  cancel（软删）  ┌───────────┐
 │ deferred │────────────────▶│ cancelled │
 └──────────┘                 └───────────┘
                                    │ delete（物理删，二次确认）
                                    ▼
                                  (移除)
```

约束：**只有一条硬规则** —— `cancelled` 是终态，要复活必须先回到 `todo`（防止「取消 → 直接完成」
把 `completed_at` 写成一个语义混乱的值）。`todo → done` 直跳是**允许**的：已批准的原型里
就是「把卡片从待办直接拖到已完成」，加禁区只会让拖拽看起来失灵。

> **与初稿的差异**：初稿写的是「`done` 只能由 `doing` / `blocked` 进入」。实现时放宽了 ——
> 该约束的真实意图是「`completed_at` 要有意义」，而这一点由下面这条不变量保证，与「从哪来」无关。
> 见 §9 实现记录第 4 条。

**`completed_at` 不变量**（实现层保证，`store::tasks`）：进入 `done` 时写一次
（**已经有了就不重写** —— 反复把卡片拖进「已完成」不会刷新完成时刻），离开 `done` 时清空；
进入 `done` 同时把 `progress` 补到 100，离开时不动进度（留给用户自己调）。

### 2.3 归档模型（Archiving）

**归档 ≠ 删除，也 ≠ 状态**。归档是一个与状态正交的维度：任何状态的任务都能被归档，任何状态的项目都能被归档。

| 维度 | 归档（archive） | 软删（cancelled / status） | 物理删除（delete） |
| --- | --- | --- | --- |
| 是否从常规视图消失 | ✅ 是 | ✅ 是（`cancelled` 默认不显示） | ✅ 是 |
| 是否计入统计 | ❌ 不计 | 计入"取消"口径 | ❌ 不计 |
| 是否可在原地恢复 | ✅ 归档中心里一键恢复 | ✅ 可恢复为 `todo` | ❌ 不可恢复（仅靠提示条撤销） |
| 数据是否保留 | ✅ 全量保留 | ✅ 保留 | ❌ 连同子任务 / 里程碑级联删除 |
| 语义 | "做完了/搁置了，但不想再看见，也不想删" | "这条不做了" | "这条不该存在" |

**规则**

1. **归档即隐藏**：`archived_at IS NOT NULL` 的任务 / 项目，从看板、列表、日历、时间线、报表、侧栏计数、全局搜索**全部默认排除**。唯一入口是左栏「归档中心」。
2. **级联语义**：归档项目 = 该项目下所有未归档任务**一并归档**；恢复项目 = 只恢复**随项目一同归档**的任务（独立归档过的任务不连带恢复，避免误复活）。
   - 实现上用专门的级联键 **`archived_root`**（`'task:<id>'` / `'project:<id>'`）：归档项目时
     `UPDATE tasks SET archived_at = ?, archived_root = 'project:<id>' WHERE project_id = ? AND archived_at IS NULL`，
     恢复时按 `archived_root = 'project:<id>'` 反查。
   - ⚠️ **不用 `archived_at` 时间戳当级联键**（初稿的写法）：时间戳会撞 —— 同一秒里先归档项目 A、
     再单独归档任务 X，恢复 A 时会把 X 一起复活；而且 `task:12` 与 `project:12` 分不出来。
     见 §9 实现记录第 2 条。
   - 归档**顶层任务**同样级联它的子任务（父卡片都看不见了，子任务单独留着只会让人困惑），
     级联键是 `'task:<父 id>'`。
3. **归档不打断引用**：日志（`work_logs.linked_task_ids`）与统计历史仍引用归档任务，只是不再出现在实时列表里。
4. **不可双重归档**：已归档对象再次归档为幂等空操作；「归档中心」里只显示"恢复"与"彻底删除"。
5. **彻底删除**只在归档中心提供，且二次确认（走 `notify::sticky` 需手动关闭的那种强提示）。
   实现层另外加了一道闸：`delete_archived_task` / `delete_archived_project` 拒绝删除**未归档**对象 ——
   让「删」必须经过「归档」这一步，界面上就永远不会有「手滑点了某处、东西直接没了」的入口。
6. **筛选与归档的关系**：`cancelled` 与 `archived` 互不蕴含；看板的"已完成"列可配置是否自动折叠旧任务，但**不会**自动归档（归档必须是显式动作，避免用户"东西不见了"）。

---

## 3. 功能模块

按三个里程碑划分，**M1 为可交付的最小闭环**。

### 3.1 M1 — 核心闭环

| # | 功能 | 要点 |
| --- | --- | --- |
| F1 | 项目管理 | 新建 / 重命名 / 改色改图标 / 归档 / 恢复 / 删除；起止日期；分组归属 |
| F2 | **归档中心** | 任务与项目**均可归档**；归档后从一切常规视图与计数中消失；左栏底部入口，可查看 / 恢复 / 彻底删除；支持"清空归档"（二次确认） |
| F3 | 分组管理 | 左栏新建 / 重命名 / 排序 / 折叠 |
| F4 | 任务 CRUD | 标题、Markdown 描述、状态、优先级、四象限、标签、起止日期、预估/实际工时、进度、备注 |
| F5 | 子任务 | 一层嵌套，父任务卡片显示 `2/5` 完成计数 |
| F6 | 里程碑 | 增删、进度快照、达成时间；项目级里程碑（`task_id` 为空） |
| F7 | 看板视图 | 按状态分列；**拖拽改状态 + 列内排序**；列头显示计数与 WIP 提示 |
| F8 | 列表视图 | 表格；列可显隐；按任意列排序；按项目/状态/优先级分组 |
| F9 | 详情面板 | 右栏常驻（选中即显）；空间不足时降级为弹窗 |
| F10 | 任务编辑器 | 双列表单弹窗（左：核心信息；右：属性），照参考项目 `TaskBoard.vue:1556-1584` |
| F11 | 快速新建 | 快捷键唤起的 Spotlight；`#标签 @项目 !P0 今天/明天/周五` 语法解析 |
| F12 | 筛选 | 快捷 chips（今天到期 / 本周 / 逾期 / 未排期 / P0-P1）+ 组合条件（项目·状态·优先级·四象限·标签·日期范围） |
| F13 | 搜索 | 工具条内联搜索 + 顶栏搜索；匹配标题/描述/标签（默认不含归档项） |
| F14 | 标签管理 | 新建 / 改色 / 合并 / 删除；显示使用次数 |
| F15 | 到期提醒 | 到期前一天 / 当天 / 逾期 → `notify::system_with_toast` |
| F16 | **悬浮窗（"随手看"）** | 独立无边框窗口 300×550 置顶；进行中 + 待办两段（照 `tauri.conf.json:73-90`）——**首发即交付**（拍板 Q5） |
| F17 | 持久化 | 本地 SQLite；启动加载 + 变更增量落盘 |

### 3.2 M2 — 视图与复盘

| # | 功能 | 要点 |
| --- | --- | --- |
| F18 | 日历视图 | 月 / 周双模；沿用 `task-calendar` 的显示归属规则；日期格右键新建 |
| F19 | 时间线视图 | 横轴日期、纵轴任务条；按 `start_date → due_date` 绘制；里程碑菱形标记 |
| F20 | 报表 | 完成趋势（折线）、按项目/标签分布（条形）、逾期清单、平均周期；均可导出 |
| F21 | 工作日志 | 日报 / 周报 / 周计划三态；右侧时间轴回溯（照 `WorkLog.vue:2046-2052`） |
| F22 | 导入导出 | Markdown / CSV / JSON 导入导出；含模板下载（照 `TaskImport.vue`） |
| F23 | 批量操作 | 多选后改状态 / 改优先级 / 加标签 / **归档** / 移动项目 / 删除 |
| F24 | 键盘导航 | `J/K` 上下、`Enter` 打开、`Space` 完成、`N` 新建、`⌘Z` 撤销 |

### 3.3 M3 — AI 增强

| # | 功能 | 依赖 |
| --- | --- | --- |
| F25 | AI 智能分类 | 输入标题即推荐优先级 / 四象限 / 标签 / 归属项目 → `duck-ai` |
| F26 | AI 任务拆解 | 一句话生成子任务清单（参考项目是把子任务拼进描述，本项目改为生成真实子任务实体） |
| F27 | AI 生成周报 / 周计划 | 读取已完成任务 + 里程碑进展，生成 Markdown 草稿供编辑（只读未归档任务） |
| F28 | 自然语言查询 | "上周完成了什么""哪个项目逾期最多"→ 转筛选条件或直接作答 |
| F29 | 智能排期建议 | 依据预估工时与截止日期提示排期冲突 |

**AI 降级策略**：`duck-ai` 未配置厂商时，F24/F25/F26/F27 的入口置灰并给出"前往设置配置 AI"的跳转（复用 `pages/settings` 的跳转），**不留演示文案**（对齐 P3-6 判据）。

---

## 4. UI 模块

### 4.1 壳层布局（沿用 `duck-shell` 既有槽位）

```
┌───────────────────────────────────────────────────────────────────────┐
│ 标题栏 40  ·  搜索  ·  窗口按钮                                        │
├────┬──────────────┬──────────────────────────────────┬────────────────┤
│活动│ 左栏 260     │ 主区                              │ 右栏 300       │
│栏  │              │ ┌──────────────────────────────┐ │                │
│48  │ 智能列表     │ │ 视图工具条 34                │ │ 任务详情       │
│    │ · 今天       │ ├──────────────────────────────┤ │ · 标题/状态    │
│ ▣  │ · 本周       │ │                              │ │ · 属性表       │
│ ▤  │ · 逾期       │ │   视图内容                    │ │ · 进度+里程碑  │
│ ▥  │ · 未排期     │ │   （看板 / 列表 / 日历 /      │ │ · 时间线       │
│ ▦  │──────────────│ │     时间线）                  │ │ · 子任务       │
│    │ 项目树       │ │                              │ │ · 备注         │
│    │ ▾ 工作       │ │                              │ │                │
│    │   ● Alpha    │ │                              │ │                │
│    │   ● Beta     │ │                              │ │                │
│    │▸ 个人        │ └──────────────────────────────┘ │                │
├────┴──────────────┴──────────────────────────────────┴────────────────┤
│ 状态栏 28  ·  共 128 任务 · 已完成 47 · 逾期 3                        │
└───────────────────────────────────────────────────────────────────────┘
```

**左栏**（`SidebarPanel`，宽度随用户拖拽，默认 260，范围 200–360）
- 顶部区：智能列表（今天 / 本周 / 逾期 / 未排期 / 已完成 / 全部），行高 28，选中走 `nav_selected`
- 折叠区：项目树（分组 → 项目），项目行右侧显示未完成计数；行首 8px 圆形色点取项目色；项目右键菜单含「归档项目」
- 底部固定：`+ 新建项目`、`+ 新建分组`、**`归档中心`**（带归档件数徽标，仅在归档数 > 0 时显示）
- 栏底统计条：任务总数 / 完成数 / 逾期数（`caption_number` + tabular-nums）——**口径已排除归档项**

**主区工具条**（高 34）
- 左：`segmented_tabs` 视图切换（看板 / 列表 / 日历 / 时间线）
- 中：筛选 chips（可点选，选中走 `accent_tint` 底 + `accent_text` 字）
- 右：排序下拉（`toolbar_text_dropdown_button`）、分组下拉、密度切换、`+ 新建任务`（`ButtonVariant::Primary`）

**右栏**（`with_right_sidebar`，默认收起，选中任务时自动展开；宽度 300，范围 260–400）
- 打开规则照本项目约定：**页面自己展开的才自动收起**，且须同时判分段匹配

### 4.2 组件清单

**复用 `duck-components`（不新增）**

| 用途 | 构件 |
| --- | --- |
| 视图切换 | `segmented_tabs` |
| 工具栏下拉（排序/分组/筛选） | `toolbar_text_dropdown_button`、`dropdown_select` |
| 左栏项目树 | `SidebarPanel` + `nav_list`（`NavItem`） |
| 状态点按钮 | `status_button` / `status_button_content` |
| 任务编辑器表单 | `form`、`form_row`、`form_field`、`field_label`、`field_hint` |
| 详情面板属性表 | `option_row`、`option_group`、`settings_section` |
| 文案层级 | `title_text`、`detail_title_text`、`body_text`、`meta_text`、`caption_heading`、`caption_number` |
| 布尔项 | `switch_control` |
| 弹窗 | `dialog`（`dialog_footer` / `modal_title_row`） |
| 提醒 | `notify::{info,success,warning,error,sticky,system_with_toast}` |
| 快速新建壳 | `command_palette` |
| 动效 | `StateLayer`、`SlidingHighlight`、`color_transition` |

**新增组件（7 个，**落页面私有** `duck-app/src/pages/projects/widgets/`，纯展示无状态）**

| 组件 | 职责 | 关键规格 |
| --- | --- | --- |
| `kanban_column` | 看板列外壳：列头（状态点 + 名称 + 计数）+ 可滚动卡区 + 底部"添加卡片" | 宽自适应、卡区 gap `spacing.sm`、底部虚线按钮（hover 转实线 + 图标旋转 90°） |
| `task_card` | 任务卡：项目色条 / 标签片 / 四象限角标 / 标题 / 进度条 / 元信息行 / 操作菜单 | 圆角 `components.button_radius + 2`、hover 上浮 2px + `hover_tint` 扫光、`doing` 左侧 3px 主色条、`done` 整体 `opacity .7` |
| `task_detail_panel` | 右栏详情：状态头 + 属性表 + 进度 + 里程碑 + 时间线 + 子任务 + 备注 | 通体 `body_text`；区块间 `hairline` 分隔 |
| `progress_bar` | 进度条：轨道 `progress_track`、填充 `progress_fill`；按档位变色 | ≥80 成功绿 / ≥50 主色 / ≥20 警告橙 / 其余中性 |
| `tag_chip` | 标签片：`content::accent_tag` + 标签自定义色点 | 超出 3 个折叠为 `+N` |
| `priority_dot` | 优先级点 + 文字（P0 红 / P1 橙 / P2 蓝 / P3 灰） | 8px 点 + `xs` 文字 |
| `task_menu`（**S3-III 新增**） | 卡片 / 列表行共用的右键菜单：原型 `cardMenu()` 的逐条对应 + 四个子菜单（优先级 / 状态 / 项目 / 四象限） | 菜单数据**右键那一刻现读**（`menu_data(id, cx)`）；子菜单走 `PopupMenu::build` + `PopupMenuItem::submenu` |

> **落位（拍板 Q4）**：这 7 个组件落**页面私有** `duck-app/src/pages/projects/widgets/`，**不进 `duck-components` 公共库**。理由：它们目前只服务项目管理页，放入公共库会污染通用层；待第二个页面出现同类需求时再上提。代价是要自行遵守「重复元素必须有领域派生 `ElementId`」的约定，命名预先约定为 `projects-task-{id}`、`projects-col-{status}`、`projects-archived-{id}`。
> 私有模块内部按组件分文件（`kanban_column.rs` / `task_card.rs` / `task_detail_panel.rs` / `progress_bar.rs` / `tag_chip.rs` / `priority_dot.rs` / `task_menu.rs`），由 `widgets/mod.rs` 统一 `pub(crate)` 导出。
>
> ⚠️ **S3-III 更正上表「左栏项目树」一行**：左栏**不复用 `nav_list`** —— 那里的滑动高亮块按「行下标 × 固定行高」定位，而 S3-III 的左栏每两段之间还夹着段标题与缩进行，下标算不准会滑到错行。改为自绘行（`section_heading` / `indent()` / `collapse_row` / `tag_row` / `smart_id`）。

### 4.3 关键界面规格

**① 任务卡片（`task_card`）**

```
┌─────────────────────────────────────┐
│ ▌ ●Alpha  【前端】     ┌Q1┐   ⋯     │ ← 项目色条 / 标签片 / 四象限角标 / 更多
│ ▌ 实现连接池复用                     │ ← 标题 sm 13/18 MEDIUM，2 行截断
│ ▌ ▓▓▓▓▓▓▓░░░░░░░░ 45%               │ ← 进度条（仅 doing 且有进度时显示）
│ ▌ 2/5 ☑  ⏱ 3/8h  📅 10-18          │ ← 子任务计数 / 工时 / 截止日
└─────────────────────────────────────┘
```

- 状态差异：`todo` 无左侧色条；`doing` 左侧 3px 主色条；`blocked` 左侧 3px 警告色条 + 卡片右上角阻塞图标；`done` 整体 `opacity .7` 且标题加删除线；`deferred` 左侧 3px 三级文字色条
- 逾期：截止日文字转 `destructive`，并在日期前加警示图标
- hover：上浮 2px + `hover_tint` 薄底扫光；操作菜单从 `opacity .5` 提到 `1`
- 选中：`ring` 描边（1px 主色）+ `nav_selected` 底

**② 详情面板（右栏 `task_detail_panel`）**

分区顺序（自上而下）：状态头（状态点按钮 + 优先级点 + 项目名）→ 标题（`detail_title_text`，可点即改）→ 属性表（`option_row`：项目 / 状态 / 优先级 / 四象限 / 标签 / 起止 / 工时 / 进度）→ 里程碑列表 → 时间线（创建 / 开始 / 里程碑 / 完成）→ 子任务清单 → Markdown 备注 → 底部操作（编辑 / 开始 / 完成 / 删除）。

**③ 任务编辑器弹窗**

宽度 `min(940px, 100vw - 32px)`、最大高 `min(88vh, 920px)`、双列 `grid: minmax(0,1.12fr) minmax(340px,.88fr)`（照抄参考项目 `TaskBoard.vue:1556-1584` 的比例）。
- 左列「核心信息」：标题（文本框）、描述（多行 + AI 三按钮〔增强/拆解/分类〕）、起止日期两列、工时两列
- 右列「任务属性」：项目（下拉）、状态（下拉）、优先级（下拉）、四象限（2×2 选择器）、标签（标签选择器 + 管理入口）、父任务（下拉）
- 底部：左"必填校验提示"，右"取消 / 保存"

**④ 快速新建（Spotlight）**

```
┌──────────────────────────────────────────────┐
│  ✦  修复登录超时 #后端 @Alpha !P0 明天 18:00  │ ← 单行输入 + 解析预览
│  ────────────────────────────────────────────│
│  项目: Alpha        优先级: P0               │ ← 解析结果 chips（可点改）
│  标签: 后端          截止: 10-15 18:00       │
│  ────────────────────────────────────────────│
│  ↵ 创建    ⇥ 展开完整表单    esc 取消         │
└──────────────────────────────────────────────┘
```

语法：`#标签`（多值）、`@项目`、`!P0`–`!P3`、`今天/明天/后天/周X/下周一`、`HH:MM`（截止时刻）。解析结果**实时预览且可点击修正**；解析不出的部分原样留作标题。

**⑤ 悬浮窗（`task_float` 独立窗口，"随手看"入口 —— M1 首发）**

宽 300 × 高 550、无标题栏、置顶、不占 Dock。顶部 28px 拖动区 → 「进行中」段（状态点 + 计数 + 卡片）→ 「待办」段（默认显示 5 条，超出折叠为"展开剩余 N 条"）→ 底部操作条（新建 / 打开主窗口 / 刷新 / 隐藏）。点任务就地展开内联详情。

- 唤起：`⌥⌘T` 全局键（走 `app::toggle_launcher` 既有通道），或从概览页/托盘菜单打开
- 数据源同主窗口（共享 `duck-project`），显示**默认排除归档项**；每张卡右侧有"完成 / 归档"快捷按钮
- 关闭 = 隐藏窗口，不销毁状态；重启后是否自动出现由设置项控制（默认关，遵循"不打扰"）

**⑦ 归档中心（M1）**

从看板 / 列表直接进入，或点左栏底部「归档中心」。两段：`已归档任务` / `已归档项目`，各按归档时间倒序。

- 每行：标题（项目色点）· 原状态 · 归档时间（相对时间，走 `duck-theme` 的 `tr_relative_time`）· 操作（`恢复` / `彻底删除`）
- 顶部：搜索框（只搜归档区）+ `清空归档`（二次确认，走强提示）
- 项目归档行可展开显示随它一同归档的任务数（`3 个子任务随项目归档`）
- 空态：`归档区是空的 · 归档的任务和项目会出现在这里`

**⑥ 报表与工作日志（M2）**

- 报表：顶部时间范围选择 → 指标卡 4 枚（已完成 / 进行中 / 逾期 / 平均周期）→ 完成趋势折线 → 按项目分布条形 → 逾期任务表
- 工作日志：`segmented_tabs` 三态（日报 / 周报 / 周计划）→ 左编辑区（Markdown 编辑 ↔ 预览互切）+ 右 280px 时间轴（历史记录，可点击回填）

---

## 5. 交互细则

### 5.1 拖拽

- 拖卡片跨列 → 改 `status`，落列位置决定 `sort_order`
- 拖拽中：源卡片 `opacity .4` + 虚线占位；目标列 `drop_target`（主色 20%）背景 + `drag_border` 描边
- 落列即写入（乐观更新 + 失败回滚 + `notify::error`）
- 列内排序：仅同列内允许，采用"插入到目标卡片的间隙"语义

### 5.2 筛选与统计口径

**全局前置条件**：所有常规视图、快捷筛选、侧栏计数、报表、搜索一律附加 `archived_at IS NULL`（归档中心除外）。即下表口径都隐含"未归档"。

| 快捷筛选 | 口径 |
| --- | --- |
| 今天 | `due_date == today` 且未完成 |
| 本周 | `due_date` 落在本周（周一至周日）且未完成 |
| 逾期 | `due_date < today` 且 `status ∉ {done, cancelled}` |
| 未排期 | `due_date` 为空且未完成 |
| 已完成 | `status == done` |

**归档相关口径**

| 场景 | 口径 |
| --- | --- |
| 归档任务 | `UPDATE tasks SET archived_at = now WHERE id = ? AND archived_at IS NULL` |
| 归档项目（级联） | 同一事务：先把项目下未归档任务 `archived_at` 置为同一时间戳，再置项目行 |
| 归档中心 · 任务段 | `archived_at IS NOT NULL` 且（`project_id IS NULL` 或对应项目未归档） |
| 归档中心 · 项目段 | `projects.archived_at IS NOT NULL` |
| 恢复任务 | `archived_at = NULL`（若其所属项目仍处归档态，提示"该项目仍在归档中"） |
| 恢复项目 | 项目行恢复 + 把 `archived_at == 项目归档时间戳` 的任务一并恢复 |
| 统计排除 | 概览/报表的"任务总数 / 完成率 / 逾期"均不含归档项 |

### 5.3 日历归属规则（照抄并简化）

对每条任务计算"落在哪一天上"：

1. `status == done` → 取 `completed_at` 当日
2. 未完成且 `due_date` 存在 → 取 `due_date`
3. 未完成且 `due_date` 为空 → **显示在今天**（避免待办从日历上消失）
4. 兜底 → `created_at`

月视图：7 列 × 6 行网格，格内最多 2 条 + `+N`；今日格用强调色描边，选中格 `accent_tint` 底。周视图：7 列看板式，列头为星期 + 日期。

### 5.4 快捷键

| 键 | 动作 |
| --- | --- |
| `N` | 新建任务（快速新建） |
| `⇧N` | 新建项目 |
| `J` / `K` | 列表中上下移动选中 |
| `Enter` | 打开选中任务详情 |
| `Space` | 切换选中任务完成态 |
| `E` | 编辑选中任务 |
| `⌫` | 删除选中任务（二次确认） |
| `1`–`4` | 切视图（看板/列表/日历/时间线） |
| `⌘Z` | 撤销最近一次变更（M2） |

均登记为页面级 key binding，不占用全局快捷键；全局唤起走 `app::toggle_launcher` 既有通道。

### 5.5 空态与错误态

- 无项目：居中图标 + "还没有项目" + `新建项目` 按钮
- 项目内无任务：列内空态文案（"暂无待办"）+ 列底"添加卡片"按钮
- 归档中心为空：`归档区是空的` + "归档的任务和项目会出现在这里"
- 归档中心搜索无结果：`归档区里没有匹配「…」的条目`
- 数据库打开失败：全页错误态 + "重试" + "查看日志"（跳 `pages/logs`）
- **不预置任何演示数据**（对齐 P3-6 判据）；首次进入就是空态

### 5.6 归档交互

| 入口 | 位置 |
| --- | --- |
| 归档任务 | 卡片/列表行右键菜单「归档」、详情面板「…」菜单、批量操作工具栏 |
| 归档项目 | 左栏项目行右键菜单「归档项目」（级联提示：`将同时归档该项目下 12 个任务`） |
| 查看归档 | 左栏底部「归档中心」 |
| 恢复 / 彻底删除 | 归档中心每行右侧 |

- **即时反馈 + 撤销**：归档后 `notify` 提示条带「撤销」动作（7s），撤销即恢复（与删除共用同一套撤销通道，但**归档的撤销不触发二次确认**）。
- **可见性原则**：归档动作**不做二次确认**（它可逆且数据不丢）；只有「彻底删除」和「清空归档」需要二次确认。
- **跨视图一致**：在看板归档卡片 → 该卡立即从当前列消失并计数减一；在列表归档 → 行移除；在日历归档 → 日期格上的条目移除。全部走同一状态变更，避免视图间不一致。
- **键盘**：列表中选中后 `A` 归档（`⇧A` 查看归档中心）；`⌘Z` 可撤销最近一次归档。

---

## 6. 分层与持久化

### 6.1 新增内核 crate `duck-project`

| 项 | 约定 |
| --- | --- |
| 依赖 | **零 gpui**、全同步（同 `duck-mail` / `duck-ai` 内核范式）。直接依赖只有 `duck-platform` / `rusqlite` / `tracing`，全部已在工作区依赖图内 —— **零新增第三方下载**（`cargo tree -p duck-project --depth 1` 可核） |
| 存储 | SQLite（`rusqlite`，`bundled`），库文件 `{app_data_dir}/projects.db`（macOS: `~/Library/Application Support/DuckToolbox/projects.db`，同 `duck-ai` 的 `ai.db`） |
| 写入 | 多语句写入走单事务（`Connection::unchecked_transaction`，故全部 API 收 `&self`）；`flush()` 做 WAL checkpoint，供退出与备份前调用 |
| 时间 | **两种表示，各司其职**：时刻（`created_at` / `updated_at` / `completed_at` / `archived_at`）→ `INTEGER` Unix **秒**；日期（`start_date` / `due_date`）→ `TEXT` `YYYY-MM-DD`。日期是日历事实，存成时刻会在别的时区读成前一天（见下） |
| 迁移 | `user_version` 版本号 + 脚本数组顺序迁移，**只追加、不改已发布条目**（同 `duck-store` / `duck-ai`） |
| 时区 | 内核**不知道用户在哪**：`今天 / 本周 / 逾期` 所需的「今天」由调用方传进 `TaskQuery::today`。内核因此可无环境依赖地单测 |

> **`now_seconds` / 历法换算手写**，不引 `chrono` / `time`（`civil_from_days` / `days_from_civil`，
> 公有领域算法，合计不到 30 行 + 单测）。这是本仓既有做法（`duck-mail` / `duck-redis` / `duck-db` 各有一份）。
> ⚠️ `time` 族在 lock 里被钉在 `0.3.44`（`bson` 带进来的），crates.io 当前的 `0.3.55` 已要求 MSRV 1.88。

**表结构（v1，已落地）**

> 与初稿的三处差异见 §9「实现记录」；下面已是**实际建表语句**（`crates/duck-project/src/db.rs`）。

```sql
CREATE TABLE project_groups (
  id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, icon TEXT,
  sort_order INTEGER NOT NULL DEFAULT 0, collapsed INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL          -- Unix 秒
);

CREATE TABLE projects (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  group_id INTEGER REFERENCES project_groups(id) ON DELETE SET NULL,
  name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  color TEXT NOT NULL DEFAULT '#3B82F6', icon TEXT,
  status TEXT NOT NULL DEFAULT 'active',        -- 只有 active / paused，**没有 archived**
  start_date TEXT, due_date TEXT,               -- 'YYYY-MM-DD'
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  archived_at INTEGER                           -- NULL = 未归档（归档是正交维度）
);
CREATE INDEX idx_projects_group    ON projects(group_id, sort_order);
CREATE INDEX idx_projects_live     ON projects(sort_order, id)          WHERE archived_at IS NULL;
CREATE INDEX idx_projects_archived ON projects(archived_at DESC)        WHERE archived_at IS NOT NULL;

CREATE TABLE tasks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
  parent_id  INTEGER REFERENCES tasks(id)    ON DELETE CASCADE,
  title TEXT NOT NULL, description_md TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'todo', priority INTEGER NOT NULL DEFAULT 2,
  quadrant TEXT NOT NULL DEFAULT 'Q2',
  start_date TEXT, due_date TEXT, due_time TEXT,   -- due_time = 'HH:MM'
  completed_at INTEGER,
  progress INTEGER NOT NULL DEFAULT 0,
  estimate_hours REAL, actual_hours REAL,
  sort_order INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  archived_at  INTEGER,
  archived_root TEXT,                            -- 'task:<id>' / 'project:<id>'，归档级联键
  CHECK (priority BETWEEN 0 AND 3), CHECK (progress BETWEEN 0 AND 100)
);
CREATE INDEX idx_tasks_board    ON tasks(project_id, status, sort_order)
                                WHERE archived_at IS NULL AND parent_id IS NULL;
CREATE INDEX idx_tasks_due      ON tasks(due_date)
                                WHERE archived_at IS NULL AND status NOT IN ('done','cancelled');
CREATE INDEX idx_tasks_parent   ON tasks(parent_id);
CREATE INDEX idx_tasks_archived ON tasks(archived_at DESC, archived_root) WHERE archived_at IS NOT NULL;

CREATE TABLE tags (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE, color TEXT NOT NULL
  -- 没有 usage_count 列：使用次数由 task_tags × tasks 现算（缓存列会随归档/删除失配）
);
CREATE TABLE task_tags (
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  tag_id  INTEGER NOT NULL REFERENCES tags(id)  ON DELETE CASCADE,
  PRIMARY KEY (task_id, tag_id)
);
CREATE INDEX idx_task_tags_tag ON task_tags(tag_id);

CREATE TABLE milestones (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id    INTEGER REFERENCES tasks(id)    ON DELETE CASCADE,   -- NULL = 项目级
  project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
  title TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', due_date TEXT,
  progress_snapshot INTEGER, reached_at INTEGER, created_at INTEGER NOT NULL,
  CHECK (task_id IS NOT NULL OR project_id IS NOT NULL)
);
CREATE INDEX idx_milestones_task    ON milestones(task_id, created_at);
CREATE INDEX idx_milestones_project ON milestones(project_id, created_at);

CREATE TABLE board_columns (                     -- 空表 = 用内置默认列；不预置任何行
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT,                                     -- NULL = 用内置默认文案（界面按语言取词）
  status_key TEXT NOT NULL, sort_order INTEGER NOT NULL DEFAULT 0, wip_limit INTEGER
);

CREATE TABLE work_logs (                         -- M2；v1 先把表建好
  id INTEGER PRIMARY KEY AUTOINCREMENT, date TEXT NOT NULL, kind TEXT NOT NULL,
  content_md TEXT NOT NULL DEFAULT '', ai_generated INTEGER NOT NULL DEFAULT 0,
  linked_task_ids TEXT NOT NULL DEFAULT '[]',
  UNIQUE (date, kind)
);
```

> 几个索引都是**部分索引**（带 `WHERE`）。因为「归档即隐藏」让 `archived_at IS NULL` 成为
> 绝大多数查询的过滤前提，部分索引把索引本身也缩小了一个量级（SQLite 3.8 起支持，`rusqlite` 内置版本满足）。
> `idx_tasks_board` 另外带 `parent_id IS NULL`：看板只画顶层任务，子任务不占列。

### 6.2 `duck-app` 侧装配

按 AGENTS §4.1 的 **三件套**拆：

| 层 | 文件 | 职责 |
| --- | --- | --- |
| State | `pages/projects/state.rs` | `ProjectsState`：项目树、任务集合、当前筛选、当前视图、选中项、归档态。**不实现 `Render`**，可单测（筛选/排序/统计口径、**归档可见性规则**全在这里） |
| View | `pages/projects/view.rs` | 纯函数 `render_*(&ProjectsState, &App) -> Element`，**不持有状态字段** |
| App | `pages/projects/mod.rs` | `ProjectsPage`：建 `Entity<ProjectsState>`、挂左栏与右栏、绑快捷键与动作、接内核 |
| Widgets | `pages/projects/widgets/*.rs` | **页面私有**的 **7** 个领域组件（拍板 Q4；`kanban_column` / `task_card` / `task_detail_panel` / `progress_bar` / `tag_chip` / `priority_dot` + **S3-III 新增 `task_menu`**）；`pub(crate)` 导出，不污染 `duck-components` |

- 路由：替换 `app.rs` 里 `PlaceholderPage::new("项目管理", DuckIcon::Kanban)` 那一行
  （S2 落地后实际在 `app.rs:810`；`projects_sidebar` / `projects_right_view` 在 `app.rs:833-834`，路由块在 `app.rs:906-916`）
- 左栏：`projects.read(cx).sidebar_state()`，与 db/redis 页同款挂法
- 右栏：`.with_right_sidebar(move |cx| projects_right_view.read(cx).render_right_sidebar(cx))`
- UI 偏好（当前视图 / 排序 / 分组 / 列宽 / 折叠态）：单键 JSON 落 `kv: projects_ui`，全字段 `Option` + `serde(default)`，默认值回落常量。
  ✅ **S3-I 已落这条**（`pages/projects/prefs.rs`，`ProjectsPrefs` 四个方法：`load` / `save` / `view_mode` / `task_sort` + 两个 `with_*` 构建器）。
  实现口径与初稿的两点差异：① **只落「视图模式 + 排序」两途**（分组 / 视图折叠是 S3-II 及以后的事，S2 的列宽仍走 `SidebarState`，与 db/redis 同款由 shell 统一持有）；
  ✅ **S3-II 复核**：视图模式的 wire 键表已含 `"list"`（`ViewMode::{Board, List, Archive}` 同表），排序切换器也已接上界面 —— 但**快捷筛选与搜索词刻意不落盘**（属「一次性视角」而非长期偏好，理由见 §9 S3-II 的「与初稿差异」表 #4）。
  ② 内核枚举（`ViewMode` / `TaskSort`）**不进 JSON**，wire 形式钉成 `&'static str`（`"board"` / `"archive"`、`"manual"` / `"due"` …），
  认不出的键**回落默认值**而**不 panic、不清档**（前向兼容：将来加了枚举变体，旧档不会让新版本起不来）。
  `load` / `save` 是 `ProjectsPrefs` 的**关联函数**（不是模块自由函数），调用点写 `prefs::ProjectsPrefs::load(cx)` / `prefs::ProjectsPrefs::save(cx, &prefs)`。
- ⚠️ **读偏好破例**：`ProjectsState::new(cx: &App)` 要读 kv，而 `state.rs` 是「不碰 gpui」的状态层 → 模块头已注明**唯一例外**是 `use gpui::App`，
  且单测走 `memory_state()` 绕开 `new(cx)`（无 `App` 可传）。这是本页为读偏好开的一道口，写偏好在 `mod.rs`（`App` 层）里做。

### 6.3 与既有模块的联动

| 目标 | 联动方式 |
| --- | --- |
| 命令面板 | 新增条目：新建任务 / 新建项目 / 跳到项目 / **打开归档中心** / **归档选中任务**（走 `palette_tools` 既有派生链，路由登记后自动出现） |
| 概览页 | 概览页"今日任务"卡片显示 `今天/逾期` 计数，点击跳 `projects` |
| 通知 | 到期/逾期提醒走 `notify`（**同 key 用 `sticky`**，避免重复堆叠） |
| AI 助手 | 助手可创建任务（`duck-ai` 的 tool 调用 → `duck-project` 写入）；任务详情可作为助手上下文 |
| SSH 终端 | 任务可关联工作目录与 Git 分支（参考项目 `Task.task.gitBranch` 的价值点），从终端页"为当前任务开工" |
| 工具箱 | 不联动（保持职责单一） |

---

## 7. 落地路径

| 阶段 | 内容 | 交付判据 |
| --- | --- | --- |
| S0 | 设计评审 + 可交互原型确认 | ✅ **已通过**（六项拍板见 §8） |
| S1 | `duck-project` 内核：表结构（**含 `archived_at` + 两个部分索引**）+ 迁移 + 项目/任务/标签/里程碑 CRUD + 归档查询口径 + 单测 | ✅ **已落地**（`crates/duck-project`，102 个单测全绿；零 gpui 已核；详见 §9） |
| S2 | `pages/projects` 三件套 + 6 个私有组件：左栏 + 工具条 + 看板 + 卡片 + 详情右栏 + 右键归档 | ✅ **已落地**（可增删改查、看板拖拽生效、归档即时隐藏、i18n EN 表已补 41 条并按字节序插入；详见 §9 S2 记录） |
| S3-I | **归档中心**（两段列表 + 搜索 + 每行恢复/彻底删除 + 清空归档）+ 撤销条（7s）+ 页面级快捷键（`N`/`⇧N`/`J`/`K`/`Space`/`A`/`⇧A`/`⌫`/`Esc`/`⌘Z`）+ 偏好落盘（`kv: projects_ui`） | ✅ **已落地**（快捷键全通；归档/恢复/彻底删除/清空全通且不可逆动作全走二次确认；空态与错误态齐备；`duck-app` 项目管理页 37 单测全绿；详见 §9 S3-I 记录） |
| S3-II | 列表视图（`ViewMode::List`）+ 筛选 chips + 主区搜索 + 排序切换器 | ✅ **已落地**（四项全通；另加视图切换 tab + `1` / `2` 两个快捷键 + 三分支空态；`duck-app` 项目管理页 50 单测全绿；详见 §9 S3-II 记录） |
| S3-III | **按 `docs/projects-mock.html` 对齐呈现与操作路径**：左栏四段重写 + 搜索框移入左栏 + 智能列表（与范围互斥）+ 「显示已完成」独立开关 + 工具条范围 chips + 状态栏重写 + 卡片右键菜单 11 项 + 「更多」显式入口 + 看板列底「+ 添加任务」+ 卡片四象限 / 阻塞标记 + 排序补「按进度」档 + 子菜单标题 | ✅ **已落地**（差异清单 **13 项**全收口；`duck-app` bin **289 单测全绿**、`pages/projects` 55 个 `#[test]`；i18n EN 表本批 **+53 条**；详见 §9 S3-III 记录） |
| S4 | **悬浮窗（"随手看"，首发）** | 全局键唤起；显示未归档的进行中/待办；与主窗口数据一致 |
| S5 | M2：日历 / 时间线 / 报表 / 工作日志 / 导入导出 | 逐项验收 |
| S6 | M3：AI 分类 / 拆解 / 周报生成 / 自然语言查询 | AI 未配置时正确降级 |

**每阶段的门禁**（照仓库既有纪律）：`cargo test --locked`、`cargo clippy --all-targets`、单文件 `rustfmt --check`、下游 `cargo check`、licenses check、i18n guard；新增依赖须审计 `rust-version ≤ 1.85` 并重生成 `THIRD_PARTY_LICENSES.md`。

---

## 8. 未纳入范围与决议

**明确不做**
- 屏幕上下文采集 / VLM 分析 / AI 预测任务（参考项目的 `screen_contexts` 体系）——涉及权限与隐私，且属独立能力域
- 番茄钟（可作独立模块，不塞进项目管理）
- 多人协作 / 云端同步

**已拍板决议（2026-10-14 评审）**

| # | 问题 | 决策 | 对文档的连带改动 |
| --- | --- | --- | --- |
| Q1 | 是否引入"项目"这一级 | **维持「项目 → 任务」两级**（现状，不再纠结）。理由：现有壳层与信息架构已适配，改动成本高于收益。 | 无 |
| Q2 | 看板列可否自定义 | **可自定义**：可改名、可新增（新增列映射到内置状态之一）、可调 WIP 上限、可排序。 | §2.1 `BoardColumn` 已支持；§4.2「列设置」；状态枚举仍是 6 种内置状态，列只是它的视图映射 |
| Q3 | 存储方案 | **新增内核 crate `duck-project` + SQLite**。 | §6.1 定稿；S1 起即按内核实现，不走 kv 过渡 |
| Q4 | 6 个新组件落位 | **页面私有**（`pages/projects/widgets/`），**不进** `duck-components` 公共库。 | §0-4、§4.2 落位说明、§6.2 增 Widgets 行 |
| Q5 | 悬浮窗是否进首发 | **进首发**（"随手看"入口）。 | F16 移入 M1；§4.3 ⑤ 标为 M1；落地路径新增 S4 |
| Q6 | 工作日志是否独立存储 | **独立 `work_logs` 表**（"都需要"：既要独立存储，也要与 AI 助手联动）。 | §6.1 表结构保留；§6.3 联动行保留 |
| Q7 | **归档**（评审新增需求） | **任务与项目均可归档，归档后不再显示**；可恢复；只有归档中心可见。 | 新增 §2.3、F2、§4.1 左栏入口、§4.3 ⑦、§5.2 口径、§5.6、DDL 字段与索引 |

> Q4 的回复原文是「私有 creator 吧」——按上下文理解为「组件页面私有」，落实为 `pages/projects/widgets/` 私有模块。若原意是别的（例如某个具体的 `creator` 命名或角色），告知即可调整落位，不影响其余决策。

---

## 9. 实现记录（S1，2026-10-14）

内核已落地：**`crates/duck-project`**（14 个源文件、103 个单测）。

| 文件 | 职责 |
| --- | --- |
| `lib.rs` | 模块地图与硬约束（crate 级文档） |
| `error.rs` | `ProjectError` / `Result`（错误详情**一律英文**） |
| `time.rs` | Unix 秒 + `CalendarDate` + 手写历法换算（无日期库依赖） |
| `models.rs` | 领域枚举与记录、写入口的构建器（`NewTask` / `TaskPatch` / `TaskQuery`…） |
| `query.rs` | `TaskQuery` → SQL 的构建（筛选口径 + 排序 + 归档可见性，纯函数） |
| `db.rs` | 打开连接、PRAGMA、`user_version` 迁移 |
| `store/mod.rs` | `ProjectStore` 门面、行映射、统计口径、`SetBuilder` |
| `store/{groups,projects,tasks,tags,milestones,columns,archive}.rs` | 按实体分的读写 |

**与设计初稿的 9 处差异**（都是实现时才看清的，逐条给出理由）：

| # | 初稿 | 实现 | 理由 |
| --- | --- | --- | --- |
| 1 | 库文件 `~/.duck_toolbox/projects.db` | `{app_data_dir}/projects.db` | 全仓没有 `~/.duck_toolbox` 这个约定，`duck-platform::app_data_dir()` 才是（`duck-ai` 的 `ai.db` 同款）。初稿那行是凭印象写的 |
| 2 | 归档级联按「同一 `archived_at` 时间戳」判别 | 专用列 `archived_root`（`'task:<id>'` / `'project:<id>'`） | 时间戳会撞：同一秒里先归档项目 A、再单独归档任务 X，恢复 A 会把 X 一起复活。而且 `task:12` 与 `project:12` 分不出来。显式键没有这两个洞，还能建索引 |
| 3 | `projects.status ∈ {active, paused, archived}` | `{active, paused}` | §2.3 自己写着「归档 ≠ 状态」。两处都能表达「已归档」迟早会不一致 |
| 4 | `done` 只能由 `doing` / `blocked` 进入 | 只保留「`cancelled` 是终态」；`todo → done` 允许 | 已批准的原型就是「拖动卡片到任意列」，加禁区会让拖拽看起来失灵。初稿那条的真实意图是「`completed_at` 要有意义」，而那由 `completed_at` 不变量保证（见 §2.2） |
| 5 | `tags.usage_count` 缓存列 | 不建该列，使用次数由 `task_tags × tasks` 现算 | 缓存列要跟着归档 / 删除 / 级联改，一处漏改就长期偏。这张表本来就很小，现算更省心 |
| 6 | `board_columns` 预置默认四列 | 表为空 = 用内置默认列；`name` 可为 NULL | 内核里不能出现界面文案（i18n 守卫会拦下中文字面量），预置中文列名等于把 UI 文案写进内核层；且英文用户会看到四个中文列名 |
| 7 | 时间统一存 UTC ISO-8601 字符串 | 时刻 → `INTEGER` Unix 秒；日期 → `TEXT` `YYYY-MM-DD` | 日期是**日历事实**：把「10-18 截止」存成时刻，在 UTC-5 的机器上读出来是 10-17。文本日期不漂移，且字典序即时间序 |
| 8 | （未提） | 全部方法收 `&self`（`Connection::unchecked_transaction`） | 事务通常要 `&mut Connection`，那样每个写方法都变 `&mut self`，GPUI 侧到处要可变借用。单连接单线程的用法下，运行时借用检查等价且接口轻得多 |
| 9 | （未提） | 彻底删除**拒绝**未归档对象 | 让「删」必须经过「归档」这一步，界面上就永远没有「手滑点了某处、东西直接没了」的入口 |

**另外补齐的**：`work_logs` 表在 v1 就建好（读写属 M2，避免以后多一次迁移）；
`milestones` 支持项目级（`task_id IS NULL`）；`TaskQuery` 的「今天 / 本周 / 逾期 / 未排期」
四个口径全部落在内核并有单测（含「本周含逾期项」这个容易写错的地方）。

**门禁状态**（S1 本批）

| 项 | 结果 |
| --- | --- |
| `cargo test -p duck-project` | ✅ 103 passed / 0 failed（S1 时为 102；S2 前置件补的 `local_offset_shifts_the_calendar_day` 是第 103 条） |
| `cargo test -p duck-platform`（前置件） | ✅ 106 lib + 全部集成通过。⚠️ 首次全量跑出过 1 个失败，随后两次复跑均未再现，且失败不在新增的 `time::tests` 三条之内 → 判定为与并发编译抢构建锁时的偶发（集成测试含 IPC 套接字，耗时 7.2s） |
| `cargo clippy -p duck-platform`（前置件） | ⚠️ 1 warning，位于 `tests/ipc_protocol.rs:188`（`redundant_pattern_matching`；该文件 mtime 00:59，是**另一位会话**的新增文件，未动）。本次改动的 `time.rs` / `lib.rs` 零 warning |
| `cargo clippy -p duck-project --all-targets` | ✅ 零 warning |
| `rustfmt --edition 2024 --check`（本 crate 全部文件） | ✅ 无 diff |
| 零 gpui | ✅ `cargo tree -p duck-project --depth 1` = `duck-platform` / `rusqlite` / `tracing` |
| 零新增第三方下载 | ✅ 生成本仓许可汇总时组件数只多了 `sqlformat` / `unicode_categories` —— 两个都来自工作区**既有**的未提交改动（duck-db 的 `sqlformat`），与本 crate 无关 |
| `sbin/licenses.py --check` | ✅ 一致（已按纪律重生成汇总） |
| i18n 中文覆盖率守卫 | ✅ **本 crate 零命中**。原先的阻塞已消除：`vendor/gpui-base` 里那段未登记的重复补丁（`soft_wrap` / `set_soft_wrap`，E0592）于 18:31 被移除，`cargo test -p duck-theme` 恢复可跑。实测命中项**全部**来自并发在改的 `pages/db.rs` / `pages/module_settings.rs` / `pages/terminal/tests.rs`，`duck-project` 的 14 个文件一条未中 |
| 下游 `cargo check` | 未跑（本批只新增 crate + 一行 `[workspace.dependencies]`，未接入 `duck-app`；接入是 S2 的事） |

### S2 前置件（已落地，2026-10-14）

内核故意不知道时区（§9 差异表第 7 条），所以「此刻 → 用户眼中的今天」必须在上层完成。
这一层现已补齐：

| 位置 | 内容 |
| --- | --- |
| `duck-platform::time`（新增） | `local_utc_offset_seconds(at: i64) -> i32`：走 `localtime_r` 取**该时刻实际生效**的偏移（夏令时两侧得到不同值，调用方不必自己判断 DST），取不到时回落 `0`（按 UTC）——含非 Unix 平台。3 条单测：偏移落在 ±14 小时内且为整分钟、夏冬两侧都可信、同输入两次一致。**零新增依赖**（`libc` 本就是该 crate 的直接依赖，`lock` 的 `flock` 在用） |
| `duck-project::time`（补一个构造器） | `CalendarDate::from_local_unix_seconds(secs, offset_seconds)`：内核里仍只是「加偏移再取整日」的纯算术，偏移由上层注入，因此历法测试依旧不依赖运行机器的时区。1 条单测覆盖「东八区把 23:30Z 推到次日」「西五区把 02:00Z 退回前一日」「偏移 0 时与纯 UTC 解释逐字一致」 |

调用侧（S2 起统一走这一行）：

```rust
let now = now_seconds();
let today = CalendarDate::from_local_unix_seconds(
    now,
    duck_platform::local_utc_offset_seconds(now),
);
```

**动手前的冲突预判（事后已核对）**：S2 要改的三处共享文件恰好是并发会话正在写的——`crates/duck-app/src/app.rs`
（路由替换）、`crates/duck-app/src/pages/mod.rs`（登记新页）、`crates/duck-theme/src/i18n.rs`（EN 表按字节序插条目），
且当时 i18n 守卫在仓库级是红的。实际落地时这三处都动了；`i18n.rs` 期间被并发写入 1 次，
已复跑守卫确认本批插入未被覆盖。**结论：预判成立，代价可控** —— 详见下方 S2 记录。

---

### S2（已落地，2026-10-14）

`duck-app` 侧已交付：**`crates/duck-app/src/pages/projects/`（三件套 + 6 个页面私有组件）**，
并把 `PROJECTS_ROUTE` 从占位页换成真实页面。`pages/placeholder.rs` 由此失去全部引用，**已删除**
（`PlaceholderPage` 全仓零残留，`grep -rn` 无命中）。

| 文件 | 职责 | 行数 |
| --- | --- | --- |
| `projects/mod.rs` | 装配：建实体、挂左右栏、动作族（选范围 / 选中 / 移动 / 完成 / 归档任务 / 归档项目 / 建项目 / 快速新建）、两条输入框订阅 | 274 |
| `projects/state.rs` | 状态层：**不实现 `Render`、不碰 gpui**，内存库可单测。持内核 + 项目树 + 看板列 + 任务集 + 范围 + 选中项 + 统计 + 「今天」+ 排序口 | 486 |
| `projects/view.rs` | 全部 `render_*` 是纯函数（`&State` + `&App` 进、`AnyElement` 出）：左栏 / 工具条 / 看板 / 状态栏 / 右栏 | 493 |
| `widgets/kanban_column.rs` | 看板列：列头（名称 / 计数 / WIP 提示）+ 卡片栈；**整列是投放目标** | 135 |
| `widgets/task_card.rs` | 卡片：优先级点 + 标题 + 逾期 / 子任务角标 + 标签片 + 进度 + 截止日；**拖拽源 + 右键菜单** | 207 |
| `widgets/task_detail_panel.rs` | 右栏详情：状态 / 优先级 / 日期 / 工时 / 描述 / 进度 / 标签 / 子任务 / 逾期提示 + 「标记完成」「归档任务」 | 211 |
| `widgets/priority_dot.rs` | 优先级色点（P0 红 / P1 橙 / P2 蓝 / P3 灰） | 55 |
| `widgets/tag_chip.rs` | 标签片：中性底 + 彩色圆点 + 名称 | 43 |
| `widgets/progress_bar.rs` | 任务进度条（复用 `duck_components::content::progress_bar`，右侧 `n%`） | 36 |
| `widgets/mod.rs` | 模块声明 + `TaskDrag` 载荷 + `status_label` | 41 |

**新增的公共件**（S2 过程中补的，都不在 `pages/projects/` 内）：

| 文件 | 内容 |
| --- | --- |
| `duck-theme/src/color.rs`（新增） | `color_from_hex` / `color_from_rgb`：把用户数据里的 `#RRGGBB` 转成 `Hsla`。守「不许裸色字面量」纪律的**边界转换**——自己写 RGB→HSL（不依赖上游 `From<Rgba> for Hsla`），5 条单测 |
| `duck-theme/src/lib.rs` | 导出上面两个函数 |

**三处共享文件的改动**（`git diff --numstat`，相对 HEAD，含并发会话的既有改动）：

| 文件 | 改动 |
| --- | --- |
| `duck-app/src/app.rs` | `+15 / -8`：导入换 `PlaceholderPage → ProjectsPage`；`app.rs:810` 的占位页换成 `cx.new(\|cx\| ProjectsPage::new(window, cx))`；取 `projects_sidebar` / `projects_right_view`；`PROJECTS_ROUTE` 加 `.with_sidebar(..).with_right_sidebar(..)` |
| `duck-app/src/pages/mod.rs` | `+2 / -2`：`mod placeholder;` → `mod projects;`，`pub use` 同步换 |
| `duck-theme/src/i18n.rs` | `+114 / -0`：EN 表新增 **41 条**英译（含并发会话同期的条目）。按 key 的 **UTF-8 字节序**插入，由脚本完成，插入后 `rustfmt` 归一 |

**EN 表插入的实测口径**：脚本先取表体**全部**字符串字面量、只保留含 CJK 的作 key（第一版只认单行起头的 key，
把多行条目漏掉，误报 47 条缺失）；修正后得「表内中文键 2333 条、真缺英译 41 条」，41 条全部补齐。
建表规模 2337 → 2378 条。

**S2 侧与设计初稿的差异**（S1 那 9 条之外）：

| # | 初稿 | 实现 | 理由 |
| --- | --- | --- | --- |
| 1 | 左栏元素 id 用可见文案 | 用稳定 ASCII 键（`projects-scope-all` / `projects-project-{id}`） | gpui 按 id 认元素；用译文的话**切语言时 id 变化**，会被当成另一个元素，选中态与焦点都丢 |
| 2 | 左栏「未分组」需要一个分组节点 | `group_id IS NULL` 的项目归到单独一段，段标题**仅在存在兄弟段时**才画 | 不为显示而造一条假分组记录；只有一个段时标题没有区分作用，只是噪音 |
| 3 | 卡片右键菜单挂在 `Button` 上 | 挂在外层 `div`（有 `id` 的才是 `Stateful`） | `duck_components::button()` 返回自家 `Button`，**没有** `context_menu` |
| 4 | 拖拽落点位置可指定 | 一律落列首（`sort_order = 0`），列内次序由内核重排 | 跨列时页面不知道也不该猜插入位置；拖拽载荷只带 `task_id` |
| 5 | 归档项目后停在原范围 | 若当前范围正是该项目，回落「全部」 | 停在已归档的范围会让看板永远空着，看起来像坏了 |
| 6 | 新建项目弹窗（名称 / 颜色 / 日期） | S2 只给「点一下就有一个新项目」，名字自动取且**保证不重名**（`新项目` / `新项目 2`…） | 遵守 P3-6「界面不许有演示控件」：要么能做完整，要么先不做。完整弹窗与重命名排 S3 |
| 7 | 空态直接显示四列空看板 | 当前范围零任务时显示 `empty_state`（含「怎么建第一条」的指引） | 全空时四列没有可拖的东西，指引比空列更有用；有任务时才画列 |
| 8 | 详情面板底部按 §4.3 ② 是「编辑 / 开始 / 完成 / 删除」（删除＝彻底删除，走危险色） | S2 放「标记完成（`Secondary`）+ 归档任务（`Secondary`）」，**归档不涂危险色** | 归档可逆（§5.6：可逆且数据不丢、不做二次确认），涂红会让人以为点下去就没了；仓库里 `Destructive` 只用于丢弃型动作（删除连接 / 全部清除 / 卸载 / 清空慢日志），且原型 `projects-mock.html:1606-1608` 的详情页脚只有「开始 / 完成（primary）/ 编辑」、没有任何红色按钮。**红色的位置留给 S3 的「彻底删除」** |

**门禁状态**（S2 本批）

| 项 | 结果 |
| --- | --- |
| `cargo check -p duck-app --locked` | ✅ `Finished`，**0 error / 0 warning**（本批初版留过 2 条 `dead_code`：`ProjectsState::counts` 真冗余已删；`sort` / `set_sort` 与 `Priority::ALL` 是 S3 预留，按仓库惯例标 `#[allow(dead_code)]` + 理由） |
| `cargo test -p duck-app --locked` | ✅ **243 passed / 0 failed**（含本批新增 10 条：`pages::projects::state::tests::*` 8 条 + `pages::projects::tests::*` 2 条）；**10 个**集成测试文件全绿：`db_grid_horizontal_scroll` / `layout_settings_take_effect` / `licenses_view` / `logs_scroll_repro` / `notify_channels` / `notify_history` / `notify_sticky` / `palette_filtering` / `terminal_tab_drag_split` / `terminal_tab_key`。本批复跑 3 次，243 这个数稳定 |
| `cargo clippy -p duck-app --all-targets --locked` | ✅ **本批文件零警告**。修掉 2 条：`mod.rs` 的 `let _ = entity.update(..)`（`update` 返回闭包返回值，这里是 `()`）+ `unique_name` 的 `iter().any()` → `contains()`。⚠️ 其余 warning 全在并发文件（`pages/db.rs`、`pages/mail.rs`、`pages/plugin_center.rs`、`pages/redis/panels.rs`、`duck-components/src/kit.rs`、`duck-shell/src/view.rs`），非本批 |
| `rustfmt --edition 2024 --check`（本批 13 个文件） | ✅ 无 diff（首轮报 7 个文件待归一，已跑写模式） |
| 交付前自查出的**一处错配**（已在差异表第 8 条记录） | ⚠️→✅ 详情面板「归档任务」初版用 `ButtonVariant::Destructive`（红），与 §5.6「归档可逆、不做二次确认」+ 仓库 `Destructive` 惯例 + 已审原型三者矛盾 → 改 `Secondary`。**改后 `check` / `clippy` / `test`（243 ✓）/ 单文件 `rustfmt --check` 全部复跑通过** |
| `cargo test -p duck-project --locked` | ✅ 103 passed / 0 failed（回归：本批只加了一个构造器 + 1 条单测） |
| `cargo test -p duck-platform --locked` | ✅ 106 lib + 7 个集成测试文件全通过 |
| `cargo test -p duck-theme --locked` | ✅ **33 passed / 0 failed**，三条同族守卫 `every_chinese_literal_is_translated` / `table_is_sorted` / `plural_table_is_sorted` **全绿**。见下方「守卫归属」 |
| `sbin/licenses.py --check` | ✅ `assets/licenses/THIRD_PARTY_LICENSES.md 是最新的` |
| 零 gpui（内核） | ✅ 本批未触碰内核分层（`duck-project` / `duck-platform` 仍零 gpui） |
| 下游 `cargo check` | ✅ 即 `cargo check -p duck-app`（本页就是下游） |

**i18n 守卫的归属（如实记录）**

动手前判定这一批只能拿到「本批绿 + 仓库级红」，依据是当时命中项全部落在并发在改的
`pages/db.rs` / `pages/module_settings.rs` / `pages/terminal/tests.rs`。实际跑下来**仓库级也绿了**，
但**原因不是补齐译文** —— 并发会话在同一时段给守卫**修了一个盲点**（源码已核）：

`every_chinese_literal_is_translated` 的 `collect()` 原先只跳过 `tests/` **目录**，不跳 `tests.rs` **文件**。
现在 `i18n.rs:4954-4961` 补上了「`Path::file_stem() == "tests"` 即跳过」。为什么原先必红：
逐行扫描靠「见到 `#[cfg(test)]` 就停」收手，而 `xxx/tests.rs` 的属性写在**父文件的 `mod tests;`** 上，
落到 `tests.rs` 里就没有了 —— 于是整份测试夹具与断言（测试名、失败消息、`prod-01 副本` 这类夹具串）
都被当成界面文案要求登记。修完这一条，`pages/terminal/tests.rs` 那 11 条命中**整体消失**，仓库级随之转绿。

因此这一批的准确结论是：

- **本批贡献**：41 条新译条，字节序正确（`table_is_sorted` 钉住），且在**守卫仍扫本批文件**的前提下零命中
  —— `pages/projects/` 下没有任何 `tests.rs`（单测都在 `state.rs` / `mod.rs` 的 `#[cfg(test)] mod tests` 里），
  所以本批的中文字面量**全部**在守卫的检查范围内，`every_chinese_literal_is_translated` 通过即证明它们都已登记；
- **仓库级绿**：来自并发会话对守卫盲点的修复（以及 `db` / 设置页串的登记），**不是本批的功劳**；并发仍在写，状态随时可能再变；
- ⚠️ **副产物**（值得记一笔）：守卫现在**跳过一切 `tests.rs` 文件**。本仓的 `tests.rs` 里只有断言与夹具，
  所以这个豁免是合理的；但**今后若把界面文案写进 `tests.rs`，守卫不会再拦**；
- 本批期间观测到 `i18n.rs` 被并发写入 1 次（mtime `19:15:31`，晚于本批最后一次触碰 `19:05:52`）。
  已复跑守卫确认本批插入内容未被覆盖：抽查 12 条 key 全在（含 `今天到期 {}`），且表中相邻条目的字节序仍严格递增。

**S2 的已知边界**（不是缺陷，是范围划分）

- 不做：列表视图、筛选 chips、排序切换器、搜索、快捷键、归档中心（→ S3）；悬浮窗（→ S4）。
  为此 `TaskSort` 的读写口已备好但 `#[allow(dead_code)]` 标注，**内核侧的 `list_tasks(sort)` 已支持**。
- **§5.6 的「即时反馈 + 撤销」未做**：归档后目前只有「条目立刻消失」这一个反馈，
  `notify` 提示条 + 7s「撤销」动作排 S3（与归档中心、`⌘Z` 撤销通道一起做，它们共用同一套撤销机制）。
  设计上归档**不做二次确认**（§5.6），所以 S2 没有确认弹窗是正确的，不是漏做。
- 卡片的 `Role::Button` 只给了读屏名字（`aria_label` = 标题 + 优先级），**没有键盘焦点**（无 `FocusHandle`）。
  这条与 P2-7 的既有状况一致（无头 a11y 断言在本仓库不可用，只能靠代码审查），排 S3 与快捷键一起做。
- `render_board` 在当前范围零任务时整块换成空态，因此此时**没有列投放目标**；这是刻意的（空板上无可拖之物）。

---

### S3-I（已落地，2026-10-14）

S3 拆两批的**第一批**：归档中心（列表 + 搜索 + 恢复 / 彻底删除 / 清空）+ 撤销条（7s）+ 页面级快捷键 + 偏好落盘。
选先做这一批的理由：归档中心与快捷键共用同一套「选中行 + 展平导航 + 二次确认」骨架，
而 S3-II 的列表视图要开 `ViewMode::List` 的第二个分支，插进来会与本批的 `ViewMode` 分派互相干扰。

`pages/projects/` 新增 **2 个文件**（`archive.rs` / `prefs.rs`），三个既有文件均扩充：

| 文件 | 职责 | 行数（S2 → S3-I） |
| --- | --- | --- |
| `projects/mod.rs` | 装配：动作薄封装、快捷键绑定、确认弹窗族、撤销/回执提示条 | 274 → **1007** |
| `projects/state.rs` | 状态层：**仍不实现 `Render`**（模块头注明唯一例外是 `use gpui::App` 读偏好），新增视图模式 / 归档三态 / 选中行 / 撤销槽 | 486 → **1301** |
| `projects/view.rs` | 纯函数渲染：左栏底部「归档中心」入口 + 主区/右栏的归档分派 | 493 → **541** |
| `projects/archive.rs`（新增） | 归档中心：工具条 + 两段列表 + 行内恢复/彻底删除 + 右栏详情 + 年龄文案 | — → **576** |
| `projects/prefs.rs`（新增） | `kv: projects_ui` 的读写（`ProjectsPrefs`：`load`/`save`/`view_mode`/`task_sort`/`with_view`/`with_sort`） | — → **181** |

**三处共享文件的改动**（本批增量，`numstat` 含并发会话不算数，故按内容记）：

| 文件 | 本批改动 |
| --- | --- |
| `duck-app/src/pages/mod.rs` | **+2 行**：注释一行 + `pub(crate) use projects::bind_keys as bind_projects_keys;`（S2 的 `mod projects;` / `pub use projects::ProjectsPage;` 是上一批的） |
| `duck-app/src/app.rs` | **+1 处**：在 `bind_mail_keys(cx)` 与 `refresh_menus(cx)` 之间调一次 `bind_projects_keys(cx)`（带一段解释 `!Input` 为何必需的注释） |
| `duck-theme/src/i18n.rs` | EN 表 **+39 条**、`EN_PLURAL` **+5 条**；表规模 2378 → 2417、复数表 5 → 10（按 key 的 **UTF-8 字节序**逐条插入并重新解析表，插入后 `rustfmt` 归一） |

**关键技术点（`!Input` 谓词）** —— 这是本批唯一需要读 gpui 源码才能定下来的地方：

- `input_preference` 只有在 `prefer_character_input` 为真时才可能返回 `CharacterInput`，而该字段在**四个平台实现里全是硬编码 `false`**
  → 按键**永远先走 `KeyBindings`**，裸字母键会抢掉输入框的字符输入。故本页所有裸字母键的谓词都写成 `"Projects && !Input"`。
- 上下文栈是**由浅到深**（`Input` 比 `Projects` 更深一层），而 `Not(pred)` 会对**整个前缀栈**求值：
  写 `"Input && !Projects"` 时 `!Projects` 落到最深一层（`Input`）上求值而恒真，**谓词失效**；必须是 `"Projects && !Input"`。
- `⌘Z` **故意不带 `!Input`**：焦点在输入框里时，更深的 `Input` 上下文先认领 `⌘Z`，撤销的是**文本**而不是归档。这与「输入框里 `⌘Z` 退文字」的通用预期一致。
- 输入框的上下文常量是 `"Input"`（`vendor/gpui-base/src/input/base/state.rs` 的 `CONTEXT`）。
- `bind_keys` 里 `KEY_CONTEXT_BARE` 与页面根 `div` 的 `.key_context(KEY_CONTEXT)`（`"Projects"`）**必须同改**；
  单测 `bare_key_context_tracks_the_page_context` 钉住 `KEY_CONTEXT_BARE.starts_with(KEY_CONTEXT)` 且 `contains("!Input")`。

**S3-I 侧与设计初稿的差异**：

| # | 初稿 | 实现 | 理由 |
| --- | --- | --- | --- |
| 1 | §5.6「归档后 `notify` 提示条带『撤销』动作（7s）」 | **页面自绘撤销条**（`mod.rs::render_hint`，带「撤销」按钮，7s 自动消失） | 本仓 `duck_components::notify` **没有暴露** `action(F)`（底层 `gpui_component::notification::Notification` 有，但 `notify.rs` 没转出来）。自绘后还能避免与全局通知中心堆叠冲突。改 `notify.rs` 属于公共组件变更，超出本页范围 |
| 2 | §5.4「`⌫` 删除选中任务（二次确认）」 | 看板的 `⌫` 调内核 `delete_task`（未归档任务的**物理删除**）；归档中心的 `⌫` 调彻底删除 | 内核 `delete_task` 的注释写「界面上只在归档中心的彻底删除里调」。让 `⌫` 真正可用是**本页的口径扩展**（同样走二次确认），已在代码与 `state.rs` 单测里写明，不是偷偷改内核 |
| 3 | §5.4 键位表含 `Enter`（详情）/`E`（编辑）/`1`–`4`（切视图） | S3-I 只落 `N`/`⇧N`/`J`/`K`/`Space`/`A`/`⇧A`/`⌫`/`Esc`/`⌘Z` | `Enter` 需要右栏详情由「打开」驱动（当前靠选中驱动）、`E` 编辑弹窗按批分给 S3-II、`1`–`4` 的日历/时间线视图**尚未实现**（S5） |
| 4 | §5.4「`⌘Z` 撤销最近一次变更（M2）」 | 只撤销**最近一次归档**（`UndoAction::{RestoreTask, RestoreProject}`） | 归档撤销是「撤销变更」的子集；S3-I 先落归档这一路，其余变更的撤销面很宽，留待与 M2 一起 |
| 5 | 归档中心的「选中行」用 `i64` | 新增 `enum ArchiveRow { Task(i64), Project(i64) }` | 只存 `i64` 时「任务 12」与「项目 12」会撞，`⌫` 会删错东西 |
| 6 | （未提） | `reload_archive` **只无条件读计数**（一条 `COUNT`），两份清单仅在归档中心可见时才读 | `list_archived_tasks` 每行要 `hydrate`（标签 + 子任务进度），每次 `reload` 都读会拖慢看板。单测 `the_archive_lists_are_only_read_while_the_center_is_visible` 钉住 |
| 7 | （未提） | 归档中心五个写口（恢复 / 彻底删除 / 清空）返回 `Result`，**不**塞 `self.error` | `self.error` 是「库打不开」的**常驻横幅**；恢复失败应该是一条**即时回执**，不是常驻错误 |
| 8 | （未提） | 撤销槽失效清理：`forget_undo(action)` 只认同**一个**动作；`clear_archive` 直接清空槽；`undo()` 失败时也放下槽 | 在归档中心手动恢复 / 彻底删除 / 清空 / 删掉未归档任务之后，原来那条「撤销归档」按下去**必然失败** —— 必须提前失效，否则用户点了没反应像坏了。7 条单测覆盖 |
| 9 | （未提） | 行内按钮一律先 `cx.stop_propagation()` | 归档中心的「恢复 / 彻底删除」按钮在可点行内，不掐断冒泡就会**顺手选中那一行** |

**门禁状态**（S3-I 本批）

| 项 | 结果 |
| --- | --- |
| `cargo check -p duck-app --locked` | ✅ `Finished`，**0 error**；本批文件 0 warning |
| `cargo test -p duck-app --locked` | ✅ **271 passed / 0 failed**（bin）；10 个集成测试文件全绿。项目管理页 **37/37**，按模块：`state::tests` **30** + `prefs::tests` **4** + `tests`（`mod.rs`）**3** |
| `cargo clippy -p duck-app --all-targets --locked` | ✅ **本批文件零警告**。修掉 1 条：`mod.rs:690` 的 `redundant_closure`（`.map(\|row\| ConfirmScope::Purge(row))` → `.map(ConfirmScope::Purge)`）。⚠️ 其余 22 条 warning **全在并发文件**（`pages/mail.rs` 6、`pages/plugin_center.rs` 4、`pages/db.rs` 3、`duck-shell/src/view.rs`、`duck-components/src/kit.rs`、`plugins.rs`、`terminal_dialogs.rs`、`terminal/helpers.rs`、`status_support.rs`、`settings/*` 2、`redis/*` 2），非本批 |
| `rustfmt --edition 2024 --check`（本批 7 个文件） | ✅ 无 diff。⚠️ **不能用 `rustfmt <file>` 直写 `pages/mod.rs`** —— 它会跟随 `mod db;` 递归把并发会话的 `db.rs` 一起格式化；改用 `--config skip_children=true` 逐文件处理，并实测确认 `pages/mod.rs` **自身零差异**（全部 diff 落在 `db.rs` / 子模块上） |
| `cargo test -p duck-project --locked` | ✅ 103 passed / 0 failed（回归：本批未动内核） |
| `cargo test -p duck-platform --locked` | ✅ 2 passed + 1 doctest ignored |
| `cargo test -p duck-theme --locked --lib i18n` | ✅ **11 passed / 0 failed**，四条守卫 `every_chinese_literal_is_translated` / `table_is_sorted` / `plural_table_is_sorted` / `plural_entries_are_well_formed` **全绿** |
| `sbin/licenses.py --check` | ✅ `assets/licenses/THIRD_PARTY_LICENSES.md 是最新的`（本批**零新增依赖**，`duck-store` 的 kv 与 `serde` 都是 `duck-app` 既有依赖） |
| 下游 `cargo check` | ✅ 即 `cargo check -p duck-app`（本页就是下游） |

**i18n 归属**：本批 39 条单数 + 5 条复数**全部**在守卫检查范围内
（`pages/projects/` 下没有 `tests.rs`，单测都在 `state.rs` / `mod.rs` 的 `#[cfg(test)] mod tests` 里，
所以本批的中文字面量一条不落都被 `every_chinese_literal_is_translated` 覆盖）。守卫全绿即证明它们都已登记。
复数表 5 条专为带数量的文案而设（`trn` 查 `EN_PLURAL` 的单数档，避免英文写出 `1 subtasks`）：
`含 {} 个任务` / `含 {} 个子任务` / `已归档项目，连带 {} 个任务。` / `已清空归档，共 {} 项。` / `归档区里的 {} 项将被永久删除，无法恢复。`。

**S3-I 的已知边界**（不是缺陷，是范围划分）

- **不做**（→ S3-II）：列表视图（`ViewMode::List`）、筛选 chips、主区搜索、排序切换器。
  `state.rs::set_sort` 已留 `#[allow(dead_code)]` 接这一批；`ProjectsPrefs` 已经能存排序，只差界面。
- **不做**（→ 后续）：`Enter`/`E`/`1`–`4` 三个键、归档中心的键盘焦点（与 S2 卡片一样只有 `aria_label` 无数 `FocusHandle`，排 P2-7）。
- **不做**（→ S4/S5）：悬浮窗、日历 / 时间线 / 报表 / 工作日志 / 导入导出。
- ⚠️ **并发快照提示**：本批跑 clippy 时，`duck-components/src/kit.rs:855` 曾被并发会话改成 `Button::tooltip(move |window, cx| ..)`（而 `gpui_component::Button::tooltip` 只接受 `impl Into<SharedString>`），
  一度把 `duck-components` 编译封住、连带 `duck-app` 的 clippy 跑不起来。**该文件非本批改动，未触碰**；其 mtime 20:13:54、md5 一度 `672e9a3e…`，
  随后并发会话修好，clippy 复跑通过。记此一笔以免后人误判成本批引入。

---

### S3-II（已落地，2026-10-14）

S3 拆两批的**第二批**：列表视图（`ViewMode::List`）+ 筛选 chips（5 档）+ 主区搜索 + 排序切换器。
四项落点全部收在**同一份 `ProjectsState`** 上 —— 看板与列表读的都是 `state.tasks()`，
口径（左栏范围 + 快捷筛选 + 搜索 + 排序）**逐条一致**，切视图不会改变看到的东西。

本批**零内核改动**：`TaskQuery` 的 `.due(DueFilter)` / `.search(term)` / `.statuses([..])` / `.sort(TaskSort)`、
`DueFilter::{Today, ThisWeek, Overdue, Unscheduled}` 与 `TaskSort::ALL`（6 档）在 S1 就已备齐且有单测，
本批是第一次把它们**接出界面**。

`pages/projects/` 四个既有文件扩充，**未新增文件**：

| 文件 | 本批职责 | 行数（S3-I → S3-II） |
| --- | --- | --- |
| `projects/prefs.rs` | `ViewMode` 加 `List` 变体 + `TABS`（常规视图序列）+ `is_archive()` | 181 → **228** |
| `projects/state.rs` | `QuickFilter` 五档枚举 + `query()`（范围 / 筛选 / 搜索三组条件收在一处）+ `quick` / `search` 两对读写口 + `select_next` 的 `List` 分支 | 1301 → **1589** |
| `projects/view.rs` | `render_toolbar` / `render_filter_bar` / `render_list` + `main_empty_state` 三分支 | 541 → **1084** |
| `projects/mod.rs` | `search: Entity<InputState>` 订阅推状态、`set_sort` / `toggle_quick` 薄封装、`ViewBoard` / `ViewList` 两动作与 `1` / `2` 绑定 | 1007 → **1069** |

**共享文件的改动**（本批增量）：

| 文件 | 本批改动 |
| --- | --- |
| `duck-theme/src/i18n.rs` | EN 表 **+16 条**（2422 → 2438）、`EN_PLURAL` **+1 条**（10 → 11，`本视图 {} 条`）；按 key 的 **UTF-8 字节序**逐条插入并**每条重新解析表**，插入后 `rustfmt` 归一（净增 27 行、0 删除） |
| `duck-platform/src/vault.rs` | **不在本批范围**；顺带修了一条既有 flaky 单测（根因与实测见本节末「本批之外」） |

**关键口径**

- **`QuickFilter` 是单选，不是多选**：内核 `TaskQuery::due(DueFilter)` 收的是**单值**；且四档 `DueFilter` 的 `WHERE`
  **全部**带 `t.status NOT IN ('done', 'cancelled')`（`SETTLED_STATUSES_SQL`）—— 「今天 + 已完成」是**必然空集**。
  与其给一个按下去没反应的组合，不如不给。
- **「已完成」这一档不走 `DueFilter`**：内核 `DueFilter` 只有 4 档（没有「已完成」），而 §5.2 的快捷筛选表列了它。
  落法是 `QuickFilter::due(self) -> Option<DueFilter>`，`None` 那一档改走 `query.statuses([TaskStatus::Done])`。
  **不改内核枚举**（那是按日期口径设计的）。
- **`is_archive()` 统一判据**：加 `List` 变体后，散落的 `state.view() != ViewMode::Board` 会**逐个漏改**。
  本批把 6 处「非归档」判据（`mod.rs` 4 处动作 `focus_quick` / `toggle_selected` / `archive_selected` / `leave_archive`
  + `view.rs` 2 处渲染分派 `render_main` / 右栏）全部改成 `!view().is_archive()`；
  并把 `delete_selected` 的 `match` 从 `_ =>` 改成显式 `ViewMode::Board | ViewMode::List =>` ——
  **这就是那个真实陷阱**：不改的话列表视图的 `⌫` 会完全不响应。
- **`query()` 是唯一口径出口**：左栏 `BoardScope` 与工具条 `QuickFilter` 是 **AND**，搜索词再 AND 一次；
  三组条件都收在 `ProjectsState::query()` 一处，看板 / 列表共用。
- **列表表头与数据行共用列宽常量**：`LIST_PROJECT_WIDTH` / `LIST_PRIORITY_WIDTH` / `LIST_STATUS_WIDTH` /
  `LIST_DUE_WIDTH` / `LIST_PROGRESS_WIDTH` / `LIST_TAGS_WIDTH`（两处各写一份早晚会错位）。
- **空态要说清「是哪一种空」**：`main_empty_state` 按「有没有在筛」分三种说法 ——
  有搜索词 → `没有匹配「{}」的任务`；有快捷筛选 → `没有「{}」的任务`；两者都没有 → 原有的「这里还没有任务」+ 建第一条的指引。
  一律说「这里还没有任务」会让人以为数据丢了。
- **`本视图 {} 条` 走 `trn` 不 `trf`**：英文 `1 task` / `2 tasks` 不同形 → 登记 `EN_PLURAL`；
  `EN` 里那条（`{} tasks in this view`）是**不带数量的兜底**，两者都必须在（`plural_entries_are_well_formed` 会钉）。

**与设计初稿的差异**：

| # | 初稿 | 实现 | 理由 |
| --- | --- | --- | --- |
| 1 | §5.4 键位表 `1`–`4` 切视图 | 只绑 `1`（看板）/ `2`（列表） | 日历 / 时间线还没做（S5）；摆一个点了没反应的 tab 就是演示控件（P3-6 判据） |
| 2 | 视图切换控件未指定 | `segmented_tabs`，只放**已实现**的看板 / 列表（`ViewMode::TABS`） | 同上 |
| 3 | §5.2 快捷筛选表列了 5 档 | 5 档全落，但做成**单选** | 见上「`QuickFilter` 是单选」 |
| 4 | （未提） | **快捷筛选与搜索词刻意不落盘** | `kv: projects_ui` 只存「视图模式 / 排序」这类**长期偏好**；「今天 / 逾期 / 某个搜索词」是**一次性视角**，跟着上次关闭的样子回来会让人困惑（下次打开看到的是子集，却不知道在筛什么）。重启即回落「全部」 |
| 5 | §5.2 的「统计」 | 工具条第 2 行右端只放「本视图 {} 条」 | 计数与当前筛选口径一致（`task_total()` = 筛选**之后**的条数），不是全局计数 |
| 6 | （未提） | 主区 `search` 与归档 `archive_search` 是**两个独立 `InputState`** | 归档中心的搜索只筛归档区、主区搜索只筛常规视图；两者可能同屏（归档中心打开时工具条仍在） |

**门禁状态**（S3-II 本批）

| 项 | 结果 |
| --- | --- |
| `cargo check -p duck-app --locked` | ✅ `Finished`，**0 error / 0 warning** |
| `cargo test -p duck-app --locked` | ✅ bin **284 passed / 0 failed**；10 个集成测试文件全绿（合计 **316 passed / 0 failed / 1 ignored**）。项目管理页 **50/50**（S3-I 末是 37，本批 **+13**：`prefs::tests` +3、`state::tests` +10） |
| `cargo test -p duck-theme --locked --lib i18n` | ✅ **11 passed / 0 failed**，四条守卫全绿（EN 表 +16、复数表 +1 之后仍有序、格式合规） |
| `cargo clippy -p duck-app --all-targets --locked` | ✅ exit 0；`grep -c "pages/projects/"` = **0**（本批文件零告警）。其余 warning 全在非本批文件（mail / plugin_center / redis / settings / terminal* / db 等） |
| `rustfmt --edition 2024 --config skip_children=true --check`（6 文件） | ✅ 全 OK。`view.rs` 归一了 34 行（S3-II 首轮遗留的换行，非本批新增语义） |
| `cargo test -p duck-project --locked` | ✅ 103 passed / 0 failed（回归：本批未动内核） |
| `cargo test -p duck-platform --locked` | ✅ 106 passed + 6 个集成文件全绿（含修复后的 vault 那条） |
| `sbin/licenses.py --check` | ✅ `assets/licenses/THIRD_PARTY_LICENSES.md 是最新的`（本批**零依赖变更**） |

**i18n 归属**：本批新增 **16 条**中文文案入 `EN`，其中 `本视图 {} 条` **另登记** `EN_PLURAL`。
补表前守卫 `every_chinese_literal_is_translated` 连**文件:行**报出 16 条缺失
（`看板` / `今天` / `本周` / `未排期` / `截止` / `手动顺序` / `截止日最近` / `截止日最远` / `优先级最高` / `最近创建` /
`搜索任务…` / `本视图 {} 条` / `没有匹配「{}」的任务` / `没有「{}」的任务` /
`换个关键词，或清掉右上角的搜索框再看看。` / `再点一下那个筛选即可取消。`），补完全绿 ——
即证明**一条不落**都已登记（本批中文字面量全在 `#[cfg(test)]` **之外**的界面代码里）。
既有词 `逾期` / `已完成` / `列表` / `任务` / `项目` / `优先级` / `状态` / `进度` / `标签` 逐条比对后确认**早已在表内**，未重复登记。

**S3-II 的已知边界**（不是缺陷，是范围划分）

- **不做**（→ 后续）：编辑弹窗（完整任务表单）、`Enter`（开详情）/ `E`（编辑）两键、
  列表视图的**列宽可拖**与**点表头排序**（当前排序走工具条下拉，`TaskSort` 六档已能覆盖全部口径）。
- **不做**（→ S5）：日历 / 时间线 / 报表 / 工作日志 / 导入导出。
- **不做**（→ S6）：AI 拆解与周报。
- 主区搜索是**边打边筛**（`InputEvent::Change` → `set_search` → `reload_tasks()`，与归档搜索同款），
  **未做输入去抖**；本地 SQLite 查询代价可接受，大库待观察。

**本批之外（顺带修复，非 S3-II 范围）**

`crates/duck-platform/src/vault.rs::tests::tampered_ciphertext_is_rejected` 是一条**既有 flaky 测试**，
与项目管理模块无关（`vault.rs` 停在 10-12 18:36:56，且 `duck-platform` 不依赖 `duck-theme`）。
根因：`seal()` 返回的是 **hex 字符串**，而该测试直接在密文**末位字符**上做 `wrapping_add(1)`；
末位字符域是 `0-9a-f`，取到 `9`（→`:`）或 `f`（→`g`）时会变成**非法 hex**，
于是命中的是 `VaultError::Invalid`（格式错）而不是 `VaultError::Crypto`（认证失败）—— **2/16 = 12.5%** 的概率间歇报红。
实测：修复前连跑 32 次 **fail=3**（≈9.4%，与理论值吻合）；改成「hex 解码 → 翻转一个**字节** → 重新 hex 编码」后连跑 32 次 **fail=0**。
改动仅限 `#[cfg(test)]` 模块内（2 行），零生产语义变更。

---

### S3-III（已落地，2026-10-14）

**主题：以 `docs/projects-mock.html` 为唯一真源，把「布局 + 交互」逐处对齐原型。**

S3 的**第三批**，也是唯一一批**不改功能范围、只改呈现与操作路径**的批次。触发点是一句直白的评审意见：
「很多操作都不一致」—— 于是把原型（3214 行、87 个 `id`、约 70 个 JS 函数）当规格书重读一遍，
逐项核对左栏 / 工具条 / 状态栏 / 卡片菜单 / 看板列五块，把差异清单一次性收口。

本批**内核只动一处**（`TaskSort` 补一档），其余全部落在 `duck-app` 侧。

| 文件 | 本批职责 | 终态行数 |
| --- | --- | --- |
| `projects/prefs.rs` | `sort_key` 补 `ProgressDesc` 档（穷尽 match，漏一档编译就红） | 229 |
| `projects/state.rs` | `QuickFilter` → `SmartFilter`；新增标签筛选 / `show_done` / 项目组折叠 / 左栏计数；`query()` 四条件重写；`set_task_status` 写口（错误原样上抛） | 1952 |
| `projects/view.rs` | `render_sidebar` 四段重写 + `render_toolbar` 重排 + `render_status_bar` 重写 + 列表行挂同一份右键菜单 | 1471 |
| `projects/mod.rs` | 卡片菜单的落点（`set_task_status` / `menu_data` / `confirm_delete_task`）+ 看板列底「+ 添加任务」弹窗 | 1336 |
| `projects/widgets/mod.rs` | 组件数 **6 → 7**；`status_label(Blocked)` 由「受阻」改「阻塞」；新增 `column_title` / `quadrant_label` | 69 |
| `projects/widgets/task_menu.rs` | **新建**：原型 `cardMenu()` 的逐条对应（见下表），四个子菜单均带标题行 | 319 |
| `projects/widgets/task_card.rs` | 四象限徽标（`.qd`）+ 「阻塞」标记 + 「更多」显式菜单按钮 + 接上共享菜单 | 257 |
| `projects/widgets/kanban_column.rs` | 列底「+ 添加任务」虚线框（原型 `.addcard`） | 171 |

**共享文件的改动**（本批增量）：

| 文件 | 本批改动 |
| --- | --- |
| `duck-project/src/query.rs` | `TaskSort` **6 → 7 档**：补 `ProgressDesc`（`ALL: [Self; 7]` 与 `order_by()` 同步），`ProgressDesc => "t.progress DESC, t.sort_order ASC, t.id ASC"`。守卫单测 `every_sort_has_a_stable_tiebreaker` 遍历 `ALL`，自动覆盖新档 |
| `duck-theme/src/i18n.rs` | EN 表 **本批 +53 条**（分三段：界面主体 44 + 列底新建弹窗 5 + 子菜单标题 4）。⚠️ 该文件是**跨会话共享**：期间并发会话另增 **16 条**（db 页的 `SQL 语义诊断…` / `手动事务…` / `自动提交…` 等），表总量 **2441 → 2510**。本批每次插入都以**当前磁盘状态**为基线增量插入（脚本带「解析无损」自检），插入后 `rustfmt` 归一 |

**差异清单（原型口径 → 本批实现）**

| # | 位置 | 原型口径 | 本批之前 | 本批落法 |
| --- | --- | --- | --- | --- |
| 1 | 左栏 | 四段：智能列表（今天 / 本周 / 逾期 / 未排期）/ 工作 / 标签 / 底部计数 | 只有「全部任务 + 收集箱 + 项目组」 | `render_sidebar` 整块重写为四段；段标题走 `section_heading`，项走 `indent()`（`pl(px(18.))`） |
| 2 | 快捷筛选 | 左栏「智能列表」段，**与范围互斥** | 主区第二行 chips，与范围 AND | `QuickFilter` → `SmartFilter`；`set_scope` 里清 `smart`、`set_smart` 里清范围 |
| 3 | 左栏搜索 | 一个框，**同时筛任务与项目名** | 主区工具条只有任务搜索 | 搜索框移到左栏，`query()` 里同一词同时喂任务与项目树 |
| 4 | 「已完成」 | 工具条上一个**独立开关** | 被当成筛选第五档 | 独立 `show_done: bool`（默认 `true`） |
| 5 | 工具条 | 视图 seg + 范围 chips + 排序 + 分组 + 显示已完成 + 导入导出 + 快速新建 + 新建任务 | 视图 tab + 排序 + 搜索 + 快速新建 | 加范围 chips + 显示已完成开关 + 分隔线（分组 / 导入导出仍缺 → S5） |
| 6 | 排序 | 按截止日 / 按优先级 / 按进度 / 按标题（4 档） | 内核 6 档 | 内核补 `ProgressDesc` → **7 档**，菜单文案对齐原型（`按截止日` / `按优先级` / `按进度` / `按标题`） |
| 7 | 状态栏 | `范围：X · 本视图 N 条 · 共 N · 完成 N · 逾期 N · 搜索：词` + 右侧快捷键提示 | 共 / 进行中 / 逾期 / 今天到期 | `render_status_bar` 重写；`scope_text` 按「智能列表 / 标签 / 范围」优先级取其一 |
| 8 | 卡片菜单 | `cardMenu()` 13 项 | 2 项（标记完成 / 归档） | `widgets/task_menu.rs` 落 **11 项**（见下表），看板卡片与列表行**共用同一份** |
| 9 | 看板列底 | `+ 添加任务`（虚线框，悬停转实线） | 无 | 列底虚线框按钮（`border_dashed()` + `hover` 转实色底） |
| 10 | 卡片徽标 | `.qd` 显示 `Q1`..`Q4` | 无 | 四象限徽标 `neutral_tag(task.quadrant.as_str())` |
| 11 | 卡片「更多」 | `.ctop` 末尾的 `.more` 按钮 | 只有右键菜单 | `Button::dropdown_menu_with_anchor(Anchor::BottomRight, …)` 弹**同一份** `task_menu` —— 右键是熟手路径，「更多」是发现路径，原型两个都给 |
| 12 | 卡片阻塞标记 | `status === 'blocked'` → `阻塞` 标签 | 无 | `task.status == Blocked` 时挂 `neutral_tag("阻塞")`（内核无 `block` 原因字段，只标状态） |
| 13 | 子菜单标题 | 各子菜单首行是标题（`设为` / `移动到` / `所属项目` / `四象限定位`） | 直接列选项 | `PopupMenu::label(heading)` |

**`task_menu` 的逐条对应**（原型 `projects-mock.html:2682` 的 `cardMenu()`）

| 原型项 | 本批 | 说明 |
| --- | --- | --- |
| 在右栏查看详情 | ✅ | 选中该任务 |
| 打开完整编辑器 `⌘E` | ⛔ | 编辑器弹窗属 S5；**摆一个点了没反应的入口比不摆更糟** |
| 置顶 | ⛔ | 内核 `Task` 无 `starred` 字段 |
| `重新打开` / `暂停` / `开始` | ✅ | 三态开关，按当前状态只显示该显示的那一项 |
| 标记完成 | ✅ | `status != Done && can_transition_to(Done)` |
| 标记为阻塞 | ✅ | `status != Blocked && can_transition_to(Blocked)` |
| 优先级… | ✅ | 子菜单 `P0..P3`，`checked` 标当前档 |
| 状态… | ✅ | 子菜单走**看板列**（`column.status_key`，列名用户可改）；非法迁移 `.disabled()` |
| 移入项目… | ✅ | 子菜单首项「未归入项目」（`Inbox` 图标），其余 `FolderOpen` |
| 四象限… | ✅ | 子菜单四项，文案取 `quadrant_label`（含处置建议） |
| 复制任务 | ⛔ | 要「新建一条 + 复制字段」，内核侧还没这条路径 |
| 删除任务 | ✅ | 走二次确认（复用看板 `⌫` 的 `ConfirmScope::DeleteTask`） |
| （原型无）归档任务 | ➕ | 本仓自加能力，保留 |

**关键口径**（与代码注释逐字一致）

- **不复用 `duck_components` 的 `nav_list`**：那里的滑动高亮块按「行下标 × 固定行高」定位，而这里每两段之间还夹着段标题与缩进行，下标算不准会滑到错行。
- **范围与智能列表互斥**（同原型）：两者回答的都是「我现在在看哪一堆」，同时亮两处高亮只会让人说不清在看什么。
- **「已完成」不是智能列表的第五档**，而是工具条上一个**独立开关**（原型同款）。落点上 `show_done = false` 走 `statuses([Todo, Doing, Blocked, Deferred, Cancelled])` —— **不能用 `.done(false)`、也不能写成 `statuses([..])` 把 `Cancelled` 漏掉**，那会把归档与子任务的过滤口径一起弄丢。
- **左栏计数是全局口径**：`SidebarCounts` 由 `reload_sidebar_counts()` 单独查（「本周」单开一条 `DueFilter::ThisWeek`），**不随主区筛选收窄** —— 否则一筛数字就变，就失去了「左栏告诉你总共有多少」的意义。
- **子菜单的正确建法**：`ContextMenuExt::context_menu` 的闭包只给「菜单值 + `Window` + `Context<PopupMenu>`」，没有 `Entity`、也没有「把菜单锚回调用元素」的入口。正确路径是 `PopupMenu::build` + `PopupMenuItem::submenu(label, menu)` —— `PopupMenu::render`（`popup_menu.rs:1429-1445`）专门为这条路径补 `parent_menu` 与 `priority = parent + 1`，`native_menu/fallback.rs:99-106` 也是这么用的。**不需要 `PopupMenu::submenu(..)`（那要 `&mut Context<Self>`）**。
- **菜单数据「右键那一刻现读」**：卡片渲染时捕获数据会让一屏几十张卡每帧克隆一份全项目表；`context_menu` 存的是闭包而非菜单本体，故 `menu_data(id, cx)` 放在闭包里读一次。
- **看板列底不做内联输入**：原型是列内插一行临时元素（`inlineAdd`，`projects-mock.html:2938`）；这里换成一个小弹窗 —— 内联输入要往列里插一行临时元素（会改动布局、还要和拖拽落点抢命中区），而弹窗只有一处输入、一处焦点，标题里还写明建到哪一列。
- **「删除任务」走二次确认**：内核 `delete_task` 是**物理删除**（子任务随 `ON DELETE CASCADE` 一起没），和原型那个「从数组里挪走 + 底部提示条 7 秒撤销」的软删除不是一回事。
- **「受阻」→「阻塞」的词表统一**：内核 `store/columns.rs` 模块头写的是默认四列「待办 / 进行中 / **阻塞** / 已完成」，而界面层却叫「受阻」——同一档状态两个名字。按原型的 `cols[2].name = '阻塞'` 统一，并让列头与菜单子菜单共用 `column_title`。
- **列表行也挂同一份菜单**：原型里看板卡片与列表行都带 `data-card`，全局 `contextmenu` 处理器一视同仁（`projects-mock.html:3045-3052`），故抽成共享 `task_menu` 而不是各写一份。

**与初稿的差异**：

| # | 初稿 | 实现 | 理由 |
| --- | --- | --- | --- |
| 1 | §5.2 快捷筛选表列 5 档（含「已完成」） | 智能列表 4 档 + 工具条独立「显示已完成」开关 | 对齐原型；且「今天 + 已完成」在内核 `DueFilter` 里是必然空集（`SETTLED_STATUSES_SQL`） |
| 2 | §5.2 未指定筛选与范围的组合关系 | **互斥** | 见上「关键口径」 |
| 3 | §4.3 工具条未指定搜索框位置 | 移入左栏，一份输入同时筛任务与项目名 | 对齐原型 |
| 4 | （未提） | 智能列表 / 标签筛选 / `show_done` / 项目组折叠**刻意不落盘** | 与 S3-II 的「快捷筛选不落盘」同一理由：一次性视角，重启回落 |
| 5 | §4.2「6 个领域组件」 | **7 个**（+ `task_menu`） | 卡片菜单与列表行共用，落成 Widget 比在 `view.rs` 里塞一个长闭包更可读 |
| 6 | §2.1 状态枚举 6 种 | 不变；列头显示名统一为「阻塞」 | 只是界面词表与内核文档对齐，不动枚举 |

**门禁状态**（S3-III 本批）

| 项 | 结果 |
| --- | --- |
| `cargo check -p duck-app --locked` | ✅ 本批改动**全部落盘后**跑过 `Finished`，**0 error / 0 warning**（此时是 22:10，`pages/db.rs` 那两条 dead_code 告警也已随并发会话收口）。⏳ 此后并发会话开始在 `pages/db.rs` 迭代「SQL 语义诊断」，`duck-app` 整体编译随之间歇性红（`no method named refresh_sql_diagnostics` → `borrow of moved value: text`）—— **报错文件 100% 不在 `pages/projects/**`**（`grep -cE "pages/projects/"` = **0**） |
| `cargo test -p duck-app --locked` | ✅ bin **289 passed / 0 failed**（S3-II 末 287 → 本批 **+2**：`the_card_menu_status_action_hands_the_refusal_back_to_the_page` / `a_task_created_for_a_column_is_moved_into_that_column`）；10 个集成测试文件全绿。⏳ 补做「阻塞标签 / `.more` 按钮 / 子菜单标题」三项之后的**终局复跑被上述 `pages/db.rs` 阻断**（三项改动单独经 `cargo check` 验证通过） |
| `cargo test -p duck-theme --locked --lib i18n` | ✅ **10 passed / 1 failed**，失败项**只来自并发会话**：`crates/duck-app/src/pages/module_settings.rs:734` 的 `编辑器下方提示解析报错与目录里不存在的表。` 尚未登记（该文件是并发会话的落点，本批不碰）。✅ **本批新增/改动的 53 条字面量全部已登记**（补表前守卫连**文件:行**报出 6 处 projects 侧缺失，补完后 projects 侧 0 命中）；`table_is_sorted` / `plural_table_is_sorted` / `plural_entries_are_well_formed` / `lookups_and_placeholders` 四条**全绿**（表 +53 之后仍有序、格式合规） |
| `cargo clippy -p duck-app --all-targets --locked` | ✅ 本批文件 **0 命中**（`grep -cE "pages/projects/"` = **0**）。两次采样：第一次 exit 0（18 条 warning 全在非本批文件）；第二次 exit 101，但失败点是并发会话 `pages/db.rs` 的 `refresh_sql_diagnostics` 未编译过，告警（2 条 `very complex type` + 1 条未用导入 `duck_db::sqldiag::Severity`）也都在非本批文件 |
| `rustfmt --edition 2024 --check`（本批 10 文件） | ✅ 全 OK。`mod.rs` / `view.rs` / `kanban_column.rs` 归一了 **23 行**格式（先 `cp` 到 `/tmp`、跑 rustfmt、再比「去空白与逗号后的 token 序列」—— 三文件**逐字节相同**，确证零语义变化） |
| `sbin/licenses.py --check` | ✅ `assets/licenses/THIRD_PARTY_LICENSES.md 是最新的`（本批**零依赖变更**） |
| `cargo test -p duck-project --locked` | ⏳ **被并发改动阻塞（非本批）**：`crates/duck-db/Cargo.toml` 于 21:57:53 被并发会话改动，而 `Cargo.lock` 停在 18:51:46，故 `--locked` 报 `cannot update the lock file`（22:00:06 lock 被并发更新后，又轮到 `duck-db/src/sqldiag.rs` 改到一半报 `unresolved import sqlparser::ast::ControlFlow`）。按纪律**本批不碰 `Cargo.lock`、不碰 `duck-db`**，两条都是并发中间态。⚠️ 本批对内核的唯一改动是 `query.rs` 加一档 `order_by()` 分支，`TaskSort::ALL` 的守卫单测会覆盖；且 `duck-project` 的 103 单测此前已跑绿、本批未触碰其余内核文件 |

**i18n 归属**：本批新增 **53 条**中文文案入 `EN`（三段：界面主体 44 条 + 列底新建弹窗 5 条 + 子菜单标题 4 条）。
补表前守卫 `every_chinese_literal_is_translated` 连**文件:行**报出缺失清单，补完后 **projects 侧 0 命中** —— 即证明本批字面量**一条不落**都已登记。
三段插入都用同一份脚本 `/tmp/i18n_insert.py`（只改 `NEW` 列表复用），脚本带**「解析无损」自检**（拼回去必须与原表格区间逐字节一致）。
- ⚠️ 第三段插入时 `diff` 是 **删 4 / 增 20** 而非纯新增：多出来的 4 行删除是**并发会话刚加的超 100 列条目**（`SQL 语义诊断…` ×2 / `手动事务…` / `自动提交…`）被 `rustfmt` 从一行式展开成三行式 —— 字节等价、方向是「更符合仓库规范」，非本批语义改动。
- ⚠️ i18n 表是**跨会话共享文件**：本批三次插入之间，表总量从 2485 涨到 2506（并发会话的 +16）。每次插入都以**当前磁盘状态**为基线增量插入，而不是拿本会话开始时的快照拼接。

**S3-III 的已知边界**（不是缺陷，是范围划分）

- **不做**（→ S5）：工具条的「分组方式」下拉（按优先级 / 项目 / 状态 / 不分组）、导入导出、完整任务编辑弹窗、标签管理、列设置、里程碑。
- **做不了**（内核无字段）：卡片上的 `kind` 徽标（Ticket/Doc/Issue）、置顶（`starred`）、阻塞**原因**（`block`）—— 都要先加内核字段。本批只做到「状态为 `Blocked` 时挂一个 `阻塞` 标签」：状态本身在内核里，原因是另一回事。
- **图标顶替**：`DuckIcon` 无日历族（`CalendarToday` / `CalendarWeek` 不存在）→ 智能列表用 `Clock` / `Layers`；四象限原型用 `target` → 这里用 `Columns2`。

---

## 附：原型索引

### ① `docs/projects-mock.html` —— 完整可交互原型（评审主入口）

单窗口、真实状态驱动的活原型：**六视图 + 全部弹窗都可点通**，数据在内存中实时变化，刷新即重置（真实实现由 `duck-project` 内核 crate 落库，首次进入为空）。

已实现的可交互能力：

| 类别 | 内容 |
| --- | --- |
| 视图 | 看板 / 列表 / 日历 / 时间线 / 报表 / 工作日志，工具条六段切换，左栏智能列表 / 项目 / 标签筛选 |
| 看板 | 卡片拖拽改状态（带撤销）、列内快加、WIP 超限提示、列设置、置顶 |
| 详情 | 右栏详情面板：状态 / 优先级 / 四象限 / 项目下拉、只可前进的进度滑块、子任务勾选、里程碑达成判定、归档开关 |
| **归档** | 卡片/列表行/详情面板右键「归档」；项目行右键「归档项目（级联）」；左栏底部「归档中心」（任务/项目两段，可恢复 / 彻底删除 / 清空归档）；归档项从所有视图与计数中即时消失 |
| 弹窗 | 任务编辑器（双列，⌘↵ 保存）、快速新建 Spotlight、命令面板 ⌘K、确认框、项目设置、标签管理、看板列设置、里程碑编辑、导入、导出、设置（四页签）、通知中心、交互说明 |
| 批量 | 列表勾选 → 改状态 / 优先级 / 移入项目 / **归档** / 删除，全部可撤销 |
| 其他 | 右键上下文菜单、Toast 撤销（7s）、明暗主题、标准 / 紧凑密度、导入解析预览、导出 Markdown/CSV/JSON |

快捷键：`⌘K` 命令面板 · `N` 快速新建 · `⌘N` 新建任务 · `A` 归档选中 · `⇧A` 归档中心 · `⌘,` 设置 · `?` 交互说明 · `Esc` 逐层关闭 · `⌘↵` 编辑器内保存。

支持深链参数，评审可直接跳到指定状态：
`?view=list|calendar|timeline|report|log` · `?theme=dark` · `?density=compact` · `?select=t01` · `?demo=editor|spotlight|palette|settings|notify|import|export|tags|columns|guide|project|day`

### ② `docs/projects-ui-mockup.html` —— 静态评审稿（七画面 + 设计注记）

保留作为「设计决策」阅读材料：每个画面带注记说明来源（对齐 `temp/devAssistant` 的哪一行）与取舍理由，以及文末的六项待拍板问题。交互行为以 ① 为准。

| # | 画面 | 覆盖 |
| --- | --- | --- |
| 1 | 主看板 | 左栏项目树 + 智能列表 / 视图工具条 / 三列看板 + 任务卡片 / 右栏详情 |
| 2 | 列表视图 | 表格 + 分组 + 排序 + 批量选择 |
| 3 | 日历视图 | 月视图网格 + 任务归属 + 日期格右键 |
| 4 | 任务详情与编辑器 | 右栏详情全貌 + 双列编辑器弹窗 |
| 5 | 快速新建 | Spotlight 单行输入 + 解析预览 chips |
| 6 | 报表与工作日志 | 指标卡 + 图表 + 日报三态 |
| 7 | 悬浮窗 | 独立窗口形态（进行中 / 待办两段） |

> 悬浮窗（300×550 独立窗口）**属 M1 首发**（拍板 Q5），因是独立窗口形态，未画进单窗口的 `projects-mock.html`；其形态与窗口参数见静态稿画面 7。
> 归档能力（拍板 Q7）已在 `projects-mock.html` 中实现：卡片/项目右键可归档与恢复，左栏底部有「归档中心」，归档项从全部常规视图与计数中消失。
