# 参考项目（ifm_ai_tools）UI 布局实现完整分析

> 目的：现有工具页与参考项目的视觉/布局差异会给老用户带来学习成本。本文把参考项目的
> **布局基座、设计令牌、组件规范、工具页原型**逐层拆到可复刻的粒度，并给出与
> Duck Toolbox 当前实现的差异表与对齐方案。
>
> 参考仓路径：`/Users/bws/code/code/github/ifm_ai_tools`（Vue3 + Tailwind + Tauri）。
> 所有尺寸/类名均摘自源码，不是目测。

---

## 0. 一页速览（如果只看一段）

| 维度 | 参考项目 | Duck Toolbox 现状 | 结论 |
| --- | --- | --- | --- |
| 内容区留白 | `p-4`（16px 四周） | 左 28 / 右 24 / 上 12 / 下 24 | 我们更「松」，老用户会觉得空 |
| 工具页骨架 | `h-full flex flex-col gap-3/4`，控件**行内**排布，无卡片包裹 | 参数装进 `card()` 分组，运行行做卡片 footer | **结构不同 = 学习成本的主因** |
| 参数行 | `标签(10px muted) + 窄控件(w-20~w-32)`，一行多个，靠 `flex-wrap` 折行 | 两列网格 + 标签 12px + 控件吃满单元格 | 参考更紧凑、更像「表单条」 |
| 主操作按钮 | `px-3/4 py-1.5 rounded-lg text-xs` 放在内容流里（参数下方一行） | 卡片底部 footer，`min_h 40` | 位置与尺寸都不同 |
| 输入/输出 | 左右 50/50 双栏（DdlGenerator）或上下（CaseConvert），**textarea 固定高 h-32/h-48** | 左输入 / 右结果，输入 flex 撑满 | 大方向一致，细节差在「固定高 vs 撑满」 |
| 结果区 | 独立 `rounded-lg border` 面板，复制/清空按钮在面板**下方**一行 | 结果卡 + 头部工具按钮 | 按钮位置不同 |
| 字号 | 正文 13px；控件 12px(`text-xs`)；字段标签 10px(`text-2xs`) | 正文 13；标签 12(`field_label` x s)；无 10px 档用于标签 | 参数标签我们偏大一档 |
| 圆角 | 输入/按钮 8px(`rounded-lg`)；卡片 12px(`rounded-xl`)；标签页 6px | `radius.md`(6) 用于输入、`card_radius` 用于卡 | 输入框我们偏方 |
| 控件高度 | 输入/下拉 `py-1.5` + 12px 字 ≈ **28px** | `density.control`（比参考高一档） | 我们控件更高、更「垫」 |

一句话：**参考项目是「行内工具条 + 扁平面板」的工作台风格，我们现在是「设置页卡片 + 大控件」风格。**
老用户认的是前者。

---

## 1. 布局基座（技术栈与三条硬约束）

- **Vue3 + Tailwind utility-first**，颜色/圆角/阴影全部走 CSS 变量（`src/style.css`），
  暗色用 `.dark` 类覆盖同名变量；没有组件库（自建 62 个组件）。
- 字体：`-apple-system / SF Pro Display`，**根字号 13px**；等宽 `JetBrains Mono`。
- 图标：`lucide-vue`，统一 `stroke-width 1.5`，尺寸 10/12/13/14/16/18/32 档。
- 编辑器：CodeMirror 6（`.cm-*` 类被 safelist 保住）；终端 xterm.js。

三条硬约束（所有页面都遵守，违反就会出现「撑不开 / 双滚动条」）：

1. 根容器 `h-screen flex flex-col overflow-hidden`（`Layout.vue`）。
2. 中间区 `flex flex-1 overflow-hidden`，主区 `flex flex-col flex-1 min-w-0 overflow-hidden`。
3. 工具页根 `h-full` + 内部 `flex flex-col`，滚动只发生在**指定的**容器上
   （`overflow-y-auto` + `min-h-0`），页面自身不滚。

> 我们对应的约束：`flex_1().min_h_0()` + 唯一滚动容器（toolbox 的 workspace body）。
> 机制等价，所以「骨架能对上」，差异出在**页面内部的排布方式**。

---

## 2. 全局框架（五段式）

```
┌──────────────────────────────────────────────────────────────┐
│ Header  h-10 (40px)  logo + 名称 + 副标题 | 搜索 / 主题 / …   │  backdrop-blur, border-b subtle
├──────────┬───────────────────────────────────────────────────┤
│ Sidebar  │ TabBar  h-9 (36px)  [折叠] [工具标签…] [+菜单]     │
│ w-60     ├───────────────────────────────────────────────────┤
│ (240px)  │ Content  flex-1 overflow-auto p-4 bg-app          │
│ 收藏夹   │   ┌─────────────────────────────────────────────┐ │
│ 最近使用 │   │  工具页（ToolView → 具体工具组件，h-full）   │ │
│ 分类…    │   └─────────────────────────────────────────────┘ │
├──────────┴───────────────────────────────────────────────────┤
│ StatusBar h-7 (28px)  ● 就绪 | 88 个工具 | ⌘K 快捷启动        │
└──────────────────────────────────────────────────────────────┘
```

| 区域 | 尺寸 | 关键样式 | 我们的对应 |
| --- | --- | --- | --- |
| Header | h-10 / 40px | `justify-between`、`backdrop-blur-sm`、`border-b --border-subtle`、`bg rgba(255,255,255,.85)` | `duck-shell` 自绘标题栏 40px（**已对齐**） |
| Sidebar | w-60 / 240px | `bg-sidebar #f0f0eb`、`px-2 py-3`、分组折叠 | 左侧导航（路由/工具列表） |
| TabBar | h-9 / 36px | 标签 `px-3 py-1 rounded-md text-sm`，激活 `bg-overlay` + 图标转 accent | `toolbox` 路由标签（**高度接近**） |
| Content | `p-4` | `bg-app #f5f5f5`，唯一滚动区 | 左 28 右 24（**偏松**） |
| StatusBar | h-7 / 28px | `text-xs text-disabled`、`border-t subtle` | 有状态栏（高度接近） |

### 2.1 工具页容器（`pages/ToolView.vue`）

```vue
<div class="flex flex-col h-full">
  <div class="flex-1 min-h-0" v-if="tool">
    <component :is="toolComponent" />   <!-- 具体工具，自己 h-full -->
  </div>
</div>
```

**重要**：工具组件**不带页头**——名称/描述在 TabBar 标签与侧栏里；只有少数复杂工具
（如 SqlMerge）自己在页内再画一条 40px 的「工具抬头」。

### 2.2 设置页（`pages/Settings.vue`）

左侧二级导航 `w-52 (208px)` + 右侧内容：`<h2 class="text-lg font-semibold mb-1">` +
`<p class="text-xs mb-6 muted">` + `space-y-5 max-w-xl (576px)` 的 `SettingRow` 列表。
**没有卡片包裹**，靠留白分组。

---

## 3. 设计令牌（`src/style.css`，浅色 / 暗色双表）

| 类别 | 令牌 | 浅色值 | 暗色值 |
| --- | --- | --- | --- |
| 背景 | `--bg-app` | `#f5f5f5` | `#09090b` |
| | `--bg-surface` | `#ffffff` | `#111114` |
| | `--bg-sidebar` | `#f0f0eb`（暖灰） | `#0d0d10` |
| | `--bg-input` | `#ffffff` | `#1c1c20` |
| | `--bg-hover` | `#f1f5f9` | `rgba(255,255,255,.06)` |
| | `--bg-selected` | `#e0f2fe` | `#1e3a8a` |
| | `--bg-overlay` | `rgba(0,0,0,.04)` | `rgba(255,255,255,.06)` |
| 文字 | `--text-primary` | `#1e293b` | `#e8eaed` |
| | `--text-secondary` | `#64748b` | `#9aa0a6` |
| | `--text-muted` | `#71717a` | `#80868b` |
| | `--text-disabled` | `#9ca3af` | `#5f6368` |
| 边框 | `--border-subtle` | `rgba(0,0,0,.08)` | `rgba(255,255,255,.10)` |
| | `--border-default` | `#e2e8f0` | `#2d2d32` |
| | `--border-strong` | `rgba(0,0,0,.14)` | `rgba(255,255,255,.16)` |
| 强调 | `--accent` | `#3b82f6` | `#8ab4f8` |
| | `--accent-soft-bg` | `rgba(59,130,246,.08)` | `rgba(138,180,248,.15)` |
| 语义 | success / warning / error | `#15803d` / `#b45309` / `#dc2626` | `#4ade80` / `#fbbf24` / `#f87171` |
| 圆角 | sm/md/lg/xl/2xl | `4 / 6 / 8 / 12 / 16px` | 同 |
| 阴影 | sm/md/lg | `0 1px 2px .04` / `0 4px 12px .08` / `0 8px 30px .12` | 对应加深 |

**字号阶梯**（Tailwind 默认 + 两个自定义档）：

| 类 | 值 | 用途 |
| --- | --- | --- |
| `text-2xs` | **10px** | 参数标签、状态角标、统计说明、日志时间 |
| `text-xs` | 12px | 控件文字、按钮、正文小字、表格 |
| `text-sm` | 14px | 导航项、设置项标题、标签页 |
| `text-base` | 16px | 少用 |
| `text-lg` | 18px | 设置页标题 / 空态标题 |
| `text-2xl` | 24px | 首页大标题 |

**过渡**：全局 `* { transition: background-color .2s, border-color .2s }`；
悬停只用底色变化，**不做位移/缩放**（唯一例外：页面帮助浮动按钮 hover 放大）。

---

## 4. 组件规范（自建 62 个，以下是与工具页最相关的）

### 4.1 SelectBox（`components/SelectBox.vue`）

原生 `<select>` + `appearance-none` + 右侧自绘 chevron（12px）。

```html
<select class="appearance-none pr-8 rounded-lg w-full text-xs px-2.5 py-1.5">
```

| size | 类 | 高度 |
| --- | --- | --- |
| `tiny`（**默认**） | `text-xs px-2.5 py-1.5` | ≈28px |
| `small` | `text-xs px-3 py-1.5` | ≈28px |
| `medium` | `text-sm px-3 py-2` | ≈32px |
| `large` | `text-base px-4 py-2.5` | ≈38px |

宽度由调用方给：`w-24 / w-28 / w-32 / w-36`（**都很窄**，靠内容自适应不会超 160px）。

### 4.2 Toggle（开关）

`w-9 h-5`（36×20）圆角全圆，开 `#3b82f6`、关 `--border-strong`。**无文字**，靠行标签说明。

### 4.3 SettingRow（`components/SettingRow.vue`，设置页唯一行规范）

```html
<div class="flex items-start justify-between py-2 gap-6">
  <div class="flex-1 min-w-0">
    <div class="text-sm font-medium">标签</div>
    <div class="text-xs mt-0.5 muted">说明</div>
  </div>
  <div class="flex-shrink-0"><slot /></div>
</div>
```

要点：**标题左、控件右、间距 gap-6(24px)**；行高由 `py-2` 决定（≈48px）；行间无分隔线。

### 4.4 IconButton（`components/IconButton.vue`，11 行）

`p-1.5 rounded-md` + `hover:bg-overlay` + `text-text-secondary`；图标 14–16px。
**提示由 `.btn-wrap + .tip` 手写**（10px 黑底白字），不是组件内置。

### 4.5 按钮三档（无组件，全部内联类）

| 档 | 类 | 背景 |
| --- | --- | --- |
| 主 | `px-3/4/5 py-1.5 rounded-lg text-xs font-medium` | `#3b82f6`，字白 |
| 次 | 同上 | `var(--bg-overlay)`，字 `--text-muted`（部分用 `--bg-input`+`--text-primary`） |
| 危险 | 同上 | `rgba(239,68,68,.1)`，字 `#ef4444` |

禁用：`disabled:opacity-50`（主）/ `opacity-30`（次）。图标与文字 `gap-1.5`，图标 12–13px。

### 4.6 输入框

`px-3 py-1.5 rounded-lg text-xs font-mono outline-none` +
`bg --bg-input` + `border 1px --border-subtle`（少数用 `--border-strong`）。
**聚焦不加 ring**（全局 `:focus-visible { outline:none; box-shadow:none }`）——这是参考项目的
取舍，我们**不照搬**（见 §9）。

### 4.7 表格 / 数据网格

`text-xs`；表头 `bg-overlay` + `border-b subtle` + `font-medium`；行 `px-2 py-1`；
单元格 `max-w-48 truncate`；可编辑单元格双击就地变 input。

### 4.8 导航（Sidebar）

- `NavSection`：`px-3 py-1.5 rounded-lg text-sm font-medium`，左侧 12px chevron + 14px 图标，
  右侧计数角标（`text-2xs` + `bg-overlay`）；折叠状态存本地。
- `NavItem`：`pl-10 pr-3 py-1.5 rounded-lg text-xs`（**10px 缩进表示层级**）；
  激活 `bg-accent/15 text-accent`；悬停 `bg-overlay`；收藏星标 hover 才显形。

### 4.9 标签页（TabBar/TabButton）

容器 `h-9 px-2 border-b subtle`；标签 `px-3 py-1 rounded-md text-sm`；
激活 `bg-overlay text-primary` + 图标 accent；关闭 ✕ 仅 hover 显示；右键菜单
`min-w-[150px] rounded-lg shadow-dropdown border-strong py-1 text-sm`。

---

## 5. 工具页五类原型（核心章节）

> 所有原型共用：根 `h-full`、**无页头**（除 D 类）、间距 `gap-3`/`gap-4`、
> 控件 28px 高、参数标签 10px、按钮行在内容流里。

### 原型 A｜单列文本工具（`CaseConvert.vue`，最简）

```
┌────────────────────────────────────────────┐
│ textarea  w-full h-32 rounded-lg p-3        │  ← 固定高，不是撑满
│           text-xs font-mono leading-relaxed │
├────────────────────────────────────────────┤
│ 结果卡 rounded-xl border p-4 text-xs        │
│   标签(muted medium)        值(font-mono)   │  ← 多行「标签—值」右对齐
│   camelCase                helloWorld       │
│   snake_case               hello_world      │
└────────────────────────────────────────────┘
```

要点：**没有参数、没有运行按钮**（输入即算）；textarea 固定 h-32，结果卡跟在其后，
整体 `flex flex-col h-full gap-4`（内容不满高就留白在下方——参考项目**接受这种留白**）。

### 原型 B｜左右双栏（`DdlGenerator.vue`，最常用）

```
┌──────────────────────────────┬──────────────────────────────┐
│ flex-1 flex flex-col gap-3   │ flex-1 flex flex-col          │
│ h2 text-sm font-medium       │ label text-xs muted          │
│ ┌──────────────────────────┐ │ ┌──────────────────────────┐ │
│ │ 输入编辑器  flex-1 min-h-0│ │ │ 输出编辑器 flex-1 min-h-0│ │
│ └──────────────────────────┘ │ └──────────────────────────┘ │
│ 分表数量 [w-20] TA编码[w-28] │ [复制] [清空]     说明文字   │  ← 按钮在面板下方
│ [生成分表 DDL][生成 VIEW] ☐  │                              │
└──────────────────────────────┴──────────────────────────────┘
```

关键代码（原样）：

```html
<div class="flex h-full gap-4 min-w-0">
  <div class="flex-1 flex flex-col gap-3 min-w-0 min-h-0">
    <h2 class="text-sm font-medium">DDL 生成与分表工具</h2>
    <div class="flex-1 flex flex-col min-h-0">   <!-- 编辑器吃满 -->
    <div class="flex items-center gap-3 flex-wrap">  <!-- 参数行：label + 窄控件 -->
      <label class="text-xs mr-2 muted">分表数量</label>
      <input class="w-20 px-2 py-1 rounded text-xs font-mono text-center">
    <div class="flex items-center gap-2 flex-wrap">  <!-- 按钮行 -->
```

要点：
- **参数是「行内 label + 固定窄宽控件」**，`flex-wrap` 自动折行 —— 这就是用户说的
  「一行多个、输入框别那么长」。
- 主按钮与次按钮**同一行**，主左次右；勾选项跟在同一行尾部。
- 输出区的按钮（复制/清空）在**输出框下面一行**，靠右留说明。

### 原型 C｜多 Tab + 表格（`DataFormat.vue`）

```
[CSV] [定长] [固定分隔符] [JSON ↔ …]        ← pill tabs: px-3 py-1.5 rounded-lg text-xs
分隔符 [SelectBox w-28]  ☑ 首行为表头        ← toolbar 行：text-2xs 标签 + tiny 控件
┌ textarea h-48 (输入) ───────────────────┐
└─────────────────────────────────────────┘
[解析] [导入文件]        12 行 × 4 列       ← 动作行：主按钮 + 次按钮 + 计数
共 128 条            [搜索…w-36]            ← 表格工具条
┌ table  max-h-40 ────────────────────────┐
└─────────────────────────────────────────┘
输出格式 [SelectBox w-32] [转换] [复制]      ← 转换行
┌ textarea flex-1 min-h-0 (输出) ──────────┐
```

要点：**一条竖线到底的分段流**，每段一种「行」；表格限高 `max-h-40` 内部滚；
输出 textarea 用 `flex-1 min-h-0` 吃掉剩余高度（全页唯一弹性元素）。

### 原型 D｜长任务 / 日志（`SqlMerge.vue`）

```
┌ 页头 px-4 pt-2.5 pb-2 border-b ──────────────────────────┐
│ [icon14] SQL 文件合并 (text-xs medium) [?] [成功 chip]    │
├──────────────────────────────────────────────────────────┤
│ 左栏 flex-1 overflow-y-auto p-4 gap-3                     │
│  ▸ 字段组：text-2xs muted 标签 + (输入 flex-1 + [浏览])    │
│  ▸ 按钮行：[开始合并 primary][重置]        [取消 danger]  │
│  ▸ 进度：stage 名 + 百分比 + h-2 进度条 + 当前文件          │
│  ▸ 统计卡 rounded-lg border p-3 → grid-cols-3             │
│  ▸ 日志区：等宽 10–12px，按级别着色，自动滚底              │
└──────────────────────────────────────────────────────────┘
```

要点：
- **页内抬头**只在复杂工具出现（icon + 名称 + 帮助 + 状态 chip），高度约 40px，`border-b subtle`。
- 参数是**竖排字段组**（每组：10px 标签 + 输入 + 右侧「浏览」按钮），不是两列网格。
- 主/次/取消三个按钮一行，取消靠右（`flex-1` 占位撑开）。
- 进度条 `h-2 rounded-full`，底色 `--border-subtle`，填充主色。
- 统计用 `grid-cols-3` 的「图标+数字」小格。

### 原型 E｜设置页

`h2 text-lg` + `p text-xs muted mb-6` + `space-y-5 max-w-xl` + 每行一个 `SettingRow`；
无卡片、无分隔线、无分组标题（分组靠左导航切页）。控件多为 `Toggle` 与按钮组
（`px-3 py-1.5 rounded-md border text-sm`，选中：accent 边框 + `accent-soft-bg` + accent 字）。

---

## 6. 交互与视觉约定（跨页面一致，老用户的肌肉记忆）

1. **按钮永远在内容流里**（参数下方 / 面板下方），不做「卡片 footer」。
2. **主操作只有一个**，主色实底；其余一律 `bg-overlay` 灰底。
3. **参数标签 10px muted**，控件 12px，二者间距 6–8px，控件窄（w-20~w-36）。
4. **悬停只变底色**（`bg-overlay`），不位移、不加阴影、不改字号。
5. **空态**：居中 `icon 32px (20% 透明度)` + `text-xs muted` 主文案 + `text-2xs` 提示。
6. **快捷键**用 `<kbd class="px-1 py-0.5 rounded text-2xs font-mono bg-elevated">⌘K</kbd>`。
7. **进度/长任务**：进度条 + stage 文案 + 当前文件，可取消（danger 软底按钮）。
8. **破坏性操作**用 danger 软底（`rgba(239,68,68,.1)` + `#ef4444`），不用系统弹窗做确认按钮。
9. 设置页行距靠 `py-2`/`space-y-5`，**不用卡片**。

---

## 7. 与 Duck Toolbox 现状的差异表

| # | 项目 | 参考项目 | 我们（`tool_run.rs` / `toolbox.rs` / 设置） | 影响 |
| --- | --- | --- | --- | --- |
| 1 | 工具页骨架 | 参数、按钮、输入、结果全在**一条内容流**里 | 操作区独立卡片 + 底部运行 footer | **最大差异**：老用户找不到按钮 |
| 2 | 参数排布 | 行内 label + 窄控件，`flex-wrap` 折行 | 两列网格，控件吃满单元格 | 控件过宽、行过高 |
| 3 | 参数标签字号 | 10px（`text-2xs`）muted | 12px（`field_label`）foreground | 视觉密度差一档 |
| 4 | 控件高度 | ≈28px | `density.control`（≈30–32）+ 行 min_h 40 | 整页偏「胖」 |
| 5 | 按钮位置 | 参数下方一行（主/次/取消同排） | 卡片 footer，单独一行 | 位置 + 语义都不同 |
| 6 | 输入框 | 固定高（h-32/h-48），结果区 flex-1 | 输入 flex-1 撑满 | 大方向反了（老用户习惯固定高输入） |
| 7 | 结果操作 | 复制/清空在面板**下方**一行 | 复制在结果标题行（icon 按钮） | 位置不同 |
| 8 | 结果面板 | `rounded-lg border`，无标题栏 | 标题行 + hairline + 卡片 | 多了一层「标题栏」 |
| 9 | 圆角 | 输入/按钮 8px，卡片 12px | 输入 6px，卡片 8px | 我们更方 |
| 10 | 内容留白 | `p-4`（16px） | 左 28 / 右 24 | 我们更松 |
| 11 | 页头 | 多数无页头；复杂工具有 40px 工具抬头 | 无抬头（标题在标签页） | 接近 ✅ |
| 12 | 空态 | icon + 主文案 + 提示，居中 | 仅一行虚线文案 | 层次不足 |
| 13 | 标签页 | `text-sm`、激活 `bg-overlay` | 接近，但高度/内边距略大 | 小 |
| 14 | 焦点环 | 全局关闭 `focus-visible` | 保留 ring（可访问性更好） | **不照搬** |

---

## 8. 对齐方案（建议按 P0 → P2 落地）

### P0｜工具执行页回到「原型 B/C」的形态（`tool_run.rs`）

1. **去掉参数卡片**：参数区 = 一条内容流里的若干「行内行」
   （`label(10px muted) + 控件(w 128~176px)`，`flex-wrap`，行高 28）。
2. **运行按钮进内容流**：参数行下方一行「[运行] [重置] …」（主/次/危险同排），
   删除卡片 footer。
3. **输入/输出改回固定高 + 弹性结果**：
   - 输入 `min_h 128`（≈h-32），**不再吃满页面**；结果区 `flex_1 min_h_0` 内部滚。
   - 左右双栏保持 50/50（这一条我们已经对齐）。
4. **结果面板**：去掉标题栏 hairline，改「面板 + 下方按钮行（复制/清空）」；
   面板 `radius 8`、`border --border-subtle`。
5. **字号/高度收一档**：参数标签 10px、控件 28px、按钮 `px-3 py-1.5 text-xs`。

### P1｜通用组件补规范（`duck-components`）

- 新增 `control_tiny` 令牌（28px 高）与 `text-2xs` 字阶用法约定（参数标签）。
- `dropdown_select` 增加 `min_w` 参数（参考 128 / 176 档），支持「窄下拉」。
- 新增 `tool_page_header(title, icon, help, status)`（原型 D 的 40px 抬头）。
- 空态组件 `empty_state(icon, title, hint)`。

### P2｜外围一致化

- 内容区留白 28/24 → **20/20**（折中：比参考 16 宽、比现状窄），或按 `spacing.lg`。
- 标签页高度/内边距按参考收窄（36px 高、`text-sm`）。
- 快捷键提示统一 `kbd` 样式。

> 不做：全局关闭 focus ring（可访问性倒退）；暗色跟随系统（已有主题系统）。

---

## 9. 明确不照搬的部分（避免为了「像」而退步）

| 参考做法 | 问题 | 我们的做法 |
| --- | --- | --- |
| `:focus-visible { outline:none; box-shadow:none }` | 键盘用户无法定位焦点 | 保留 `ring`（AGENTS §3.6 已要求） |
| 大量内联 `style="color:#a1a1aa"` 硬编码 | 暗色/主题漂移 | 全部走 `tokens` / `extended` |
| 参数标签 10px + muted 到 `#71717a` | 大屏可读性偏弱 | 10px 但用 `--text-secondary`（对比度提升） |
| 每页自绘按钮/输入样式 | 无统一 disabled/hover 语义 | 走 `duck-components` 的 `button` / `input` |
| 页面无 aria、图标按钮无 label | 读屏不可用 | `role` + `aria_label` 强制（AGENTS §3.6） |

---

## 10. 附：Tailwind → Duck token 速查

| 参考 | 值 | 我们 |
| --- | --- | --- |
| `p-4` | 16px | `spacing.lg`(16) |
| `gap-3` / `gap-4` | 12 / 16px | `spacing.md` / `spacing.lg` |
| `text-2xs` | 10px | `extended.badge`(10/13) 或新增 10px 标签档 |
| `text-xs` | 12px | `typography.xs`(12/16) |
| `text-sm` | 14px | `typography.sm`(13/18) / `md`(15/20) |
| `rounded-md/lg/xl` | 6 / 8 / 12px | `radius.md`(6) / 新增 `radius.lg`(8) / `card_radius` |
| `bg-overlay` | `rgba(0,0,0,.04)` | `extended.colors.row_hover` |
| `border-subtle` | `rgba(0,0,0,.08)` | `extended.colors.hairline` |
| `border-strong` | `rgba(0,0,0,.14)` | `tokens.colors.border` |
| `--accent` | `#3b82f6` | `tokens.colors.primary` |
| 主按钮 `py-1.5` + 12px | ≈28px | `density.control` |
