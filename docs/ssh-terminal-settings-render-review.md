# SSH / 终端：设置项与渲染层专项评审

> 只读分析，未改任何代码。基线：本仓 `duck-app`（terminal 页）+ `duck-term`（内核）；
> 对照 `temp/oxideterm`、`temp/navop`（同为 GPUI 的 SSH/SFTP 工作台）与主流终端
> （iTerm2 / Windows Terminal / Kitty / WezTerm / SecureCRT·Xshell）的通行做法。
>
> 与另三份文档的分工：
> - `docs/ssh-terminal-review.md`：架构与能力矩阵（协议栈、连接运行时、SFTP）
> - `docs/ssh-terminal-ui-review.md`：布局与鼠标/快捷键的操作习惯
> - `docs/tabby-deep-dive.md`：分层、组件与 Tabby 对照
> - **本文**：设置项的**完备性与作用域**，以及渲染管线的**像素级缺陷**
>
> **终版收敛（裁决 + 唯一执行序列）见 `docs/ssh-terminal-final-plan.md`。**

---

## 0. 结论先说

1. **终端是模块里设置项最少的，但它需要的设置最多。** `TerminalSettings` 8 个字段，
   数据库 11 个、邮箱 10 个。缺的不是"几个开关"，是**六个整组**：字体面、字符与宽度、
   光标与鼠标、配色方案、终端行为、作用域（全局 vs 每连接）。

2. **有一个必然可见的渲染缺陷：中文与任何宽字符会和网格错位。** 行文本是"逐格拼字符串 +
   整行自然排版"（`terminal_canvas.rs:2738`），宽字符按字形自然推进（PingFang = 1em），
   而网格按 `'m'` 量出的格宽算（Menlo = 0.6em）→ **每出现一个汉字，该行后续字符相对
   网格右移约 0.4em（13px 字号 ≈ 5.2px）**。选区、光标、底色块全按列号摆放，于是
   "字跑了、格子没跑"。这不是审美问题：中文 `ls`、`htop`、vim 中文 buffer 全都错位，
   IME 候选窗也跟着偏（它用 `ime_cursor` 的列号定位）。

3. **字号有三份真源，且没有一份是完整的。** 设置里的 `font_size`（持久化，但只在建页面时
   读一次，`helpers.rs:9` 的注释已经明说）、页面字段 `terminal_font_size`（快捷键改它，
   但不落盘）、以及两处各自硬编码的 `10..=20` 区间（`module_settings.rs:36` 与
   `terminal/mod.rs:133`）。结果是：设置页改字号要重启才生效；⌘+ 调完重启就丢；
   上限 20px（iTerm2 可到 128）。

4. **SSH 会话没有 TERM / COLORTERM / LANG。** `terminal_canvas.rs:533` 用
   `env: target.env(...)` **整份替换**了 `SpawnOptions::default()` 里那两条
   （`session.rs:82`，本该是 `TERM=xterm-256color`、`COLORTERM=truecolor`），而
   `target.env()`（`target.rs:405`）只补 agent 与 askpass。从 Finder 启动时进程环境里
   也没有 `TERM`，OpenSSH 只能退回 `xterm`（16 色档）→ **远端看到的是 16 色终端，
   真彩色能力被浪费**；`LANG` 缺失则远端落到 C locale，中文输出本身就会乱。

---

## 1. 现状：终端设置只有 8 项

`module_settings.rs:79` 起的 `TerminalSettings`，落在单键 `kv: terminal.settings`：

| 字段 | 控件 | 默认 | 生效时机 | 问题 |
|---|---|---|---|---|
| `font_size` | 下拉 10–20 | 跟随主题 (13) | **仅新建页面** | 有值域上限 20；改完不实时；区间与页面常量重复 |
| `font_family` | **文本弹窗** | 跟随主题 (Menlo) | 每帧重算 | 无字体列表、无"未安装"提示、无等宽校验 |
| `line_height_percent` | **数字弹窗** | 主题比例 (20/13≈1.54) | 每帧重算 | 无上下限（填 400 会裂）、无常用档位 |
| `padding` | **数字弹窗** | 主题 spacing | 每帧重算 | 只有单值（不能 x/y 分开）、无上限 |
| `scrollback` | 下拉 4 档 | 10 000 | **仅新会话** | 不能自定义；无"无限"档与内存提示 |
| `auto_reconnect` | 开关 | 开 | 下次断线 | — |
| `reconnect_max_attempts` | 数字弹窗 | 常量 | 下次断线 | — |
| `reconnect_backoff_secs` | 数字弹窗 | 常量 | 下次断线 | — |

三个"用户天天要调"的东西（字号、行高、字体）里，**两个走弹窗**：改一次行高要
点开弹窗 → 输入 → 确定，看不到即时效果。而同一份应用里，**界面字体已经做成了可搜索
下拉**（`pages_appearance.rs:710` + `state.rs:38` 枚举本机字体族），终端字体却是裸文本框。
同一件事两套交互，且弱的那套用在了更常调的地方。

> 对比：外观页有「界面缩放 80–150%」「密度」「字号」「字体」四组，
> 终端页只有「显示」「会话」两组，且没有一组是"外观"意义上的。

---

## 2. 渲染管线：五段，以及每段的设置耦合点

```
① 度量  sync_cell_metrics   字体族→resolve_font→量 'm' → cell{w,h}     ← 字体族/字号/行高
② 取格  try_with_term       网格 → Vec<CellPaint>（每格一个 char）      ← 调色板 / 主题明暗
③ 排版  build_row           逐格拼 String + 分色 TextRun               ← 粗体/斜体/下划线
④ 绘制  StyledText          **整行自然排版**，按 cell.width*col 摆放     ← 无（这就是 R1 的根）
⑤ 叠加  绝对定位层          选区/命中/命令标记/光标/预编辑/断线横幅      ← 主题色 / 光标形状
```

关键事实：**②③④ 三段之间没有"逐格定位"这一步**。① 只量了一个字符的宽度，④ 却把
整行交给排版引擎按字形自然推进。只要**某个字符的实际推进 ≠ 它占的格数 × cell.width**，
第 ④ 段之后文字就与第 ⑤ 段（按列号算的色块、光标）分家了。下面 R1/R2/R4 全是这条的实例。

---

## 3. 渲染缺陷（按严重度）

### R1 · 宽字符 / 中文必然错位（P0）

- **机制**：`paint_cell`（`terminal_canvas.rs:2693`）把 `WIDE_CHAR_SPACER` 变成 `' '` 一格，
  `build_row`（`:2738`）逐格 push，于是"中"在文本里是 `"中 "` 两个字符。整行交给
  `StyledText` 自然排版：中文字形推进 1em，空格推进 0.6em ⇒ 这一对占 **1.6em**，
  而网格认为它占 2 格 = **1.2em**。每个汉字向右多推 0.4em。
- **实测（CoreText，13px，本机）**：`Menlo 'm' = 7.8267`、`PingFang '中' = 13.0000`，
  比值 **1.661**（不是 2）。两格应为 15.653，实际推进 `13 + 7.827 = 20.827` ⇒
  **每出现一个汉字，该行后续字符右移 5.17px**。
- **放大**：一行 30 个汉字 → 偏移 **155px（约 20 列）**。`ls -l` 的中文文件名、`htop`
  的中文界面、`vim` 的中文 buffer、`git log` 的中文提交信息，全部中招。
- **连带**：光标（按 `col*width`）、选区色块、搜索命中、`OSC 133` 命令标记、IME 候选窗
  （`ime_cursor` 用列号）—— 五样按格算，文字不按格走，于是"点字选不中、点空格选到字"。
- **验证**：`python3 -c "print('中文中文中文'*8)"`，然后对比选区起点与首个汉字左边。
- **修法（两档，都不依赖用户装字体）**：
  - **S0 段级绝对定位（1 天，先止血）**：行不再靠流式排版累积，而是**按起始列绝对定位**——
    `build_row` 已经在切 run（`:2738`），把每个 run 从"流式子元素"改成
    `absolute().left(col * cell.width).top(row * cell.height)`。汉字仍宽 13px（比 2 格窄
    2.4px），会出现单字轻微右溢，但**起始列永远精确、漂移不再累积**；光标/选区/IME 立刻
    对齐。这正是 90% 用户抱怨的那一半。
  - **S1 逐簇定位（1–2 周，根治）**：每行切成"ASCII 段 / 宽字符 / 零宽簇"，宽字符单独成段
    且按 `2 * cell.width` 画（必要时横向缩放或换 CJK 双宽 fallback），段内只留同推进的字符。
    这一步同时解决 R2（组合字符 / emoji）与 R7（fallback 链）的落点。

### R2 · 组合字符与 emoji 序列被丢弃（P1）

`CellPaint.ch` 是单个 `char`（`:2672`），而 alacritty 的 `Cell` 带
`extra.zerowidth: Vec<char>`（`vendor/alacritty-terminal/src/term/cell.rs:125-139`）——
`e` + U+0301、`👨‍👩‍👧`（ZWJ 序列）、带变体选择符的符号，**除首字符外全部丢**。
中文用户遇到的典型：emoji 只显示半个、部分生僻字/兼容区汉字显示成方框或错字。
修法：`CellPaint` 改成 `String`（或 `SmallVec<char>`），`build_row` 逐簇 push；
这会顺带把 R1 的 cluster 切分逻辑做出来。

### R3 · 只画对了 5 个 SGR 属性，其余退化（P1）

`paint_cell` 只认 INVERSE / HIDDEN / BOLD / ITALIC / UNDERLINE / STRIKEOUT：

| SGR | 期望 | 现状 |
|---|---|---|
| 2 `DIM` | 降低亮度（TUI 的次要文字） | **忽略**，与正常文字同亮度 → 层次全平 |
| 4 / 21 | 单/双下划线 | 都画单线（`UNDERLINE` 合并了 DOUBLE/UNDERCURL/DOTTED/DASHED） |
| 58/59 | 彩色下划线（`git diff`、`grep --color` 常用） | **忽略** `Cell.extra.underline_color` |
| 5/6 `BLINK` | 慢/快闪 | 无闪烁实现（alacritty 也不闪，可归"不做"，但要写进不做清单） |

### R4 · 度量只量 `'m'`，且没有"这字体不能用"的判断（P1）

`sync_cell_metrics`（`:1041-1046`）只取 `layout_width(font_id, size, 'm')`：
- 用户填了**比例字体**（"Helvetica"）：格宽按 H 的 m 算，整行自然排版 ⇒ 全线错位，
  且没有任何提示。
- 用户填了**未安装的族名**：`resolve_font` 静默回退，界面照旧 ⇒ 用户以为设置没生效。
- 用户填了**只有 CJK 的字体**（无拉丁字形）：`'m'` 走 fallback，量到的是别的字体的宽度。
- 字体没有 bold 面：`FontWeight::BOLD` 静默退回常规，粗体语义丢失（配色方案里的
  "bright"也没有替代）。

修法（设置层，改动很小）：字体选择器**只列本机字体**（已有 `font_options` 机制），
选中后**当场量 `'m'` 与 `'中'` 两个宽度并显示"等宽：是 / 否（1:1.67）"**，
不满足 2:1 的给黄条提示"中文可能错位"。这一条能把 R1 的一大半投诉提前拦住。

### R5 · 字号三份真源（P0，改动最小、体验最差）

| 位置 | 作用 | 问题 |
|---|---|---|
| `kv: terminal.settings → font_size` | "下次进来从哪儿起" | 只在 `TerminalPage::new` 读一次（`helpers.rs:9`） |
| `TerminalPage::terminal_font_size` | 实际渲染值 | ⌘+ / ⌘- / ⌘0 改它（`mod.rs:628-647`），**不落盘** |
| 两处 `10..=20` 常量 | 钳位 | `module_settings.rs:36` 与 `terminal/mod.rs:133` 各一份 |

后果：① 设置页把字号从 13 改到 18 → 界面无变化（要重开页面/重启）；② ⌘+ 调到 22 不可能
（上限 20），调到 18 后重启回到 13；③ 两处常量改一处、另一处忘改 = 设置页能选 20、
页面钳到 18 这类"输入被悄悄改写"。

修法：**只留一份**。`font_size` 归 `TerminalPage`，变化即 `kv_put + flush`（走现成的
`module_settings::save` 同款路径）；设置页下拉读写同一键；页面 `render` 入口比对
`kv` 值变化后就地更新 `terminal_font_size`（受控输入框的既有纪律），量一次
`layout_width` 后 `apply_canvas_size` 触发重排。⏎ 区间提到 `9..=32` 并允许直接输入。

### R6 · 界面缩放不带动终端字号（P1）

`Appearance.ui_scale`（0.8–1.5）缩放"尺寸、间距、图标与文字档位"，但终端字号是裸 px
（`terminal_font_size`），不乘 `ui_scale`。用户把界面放大到 150% 后：侧栏、列表、按钮
都变大，**只有终端里的字没变**，读起来像另一个应用。反过来 80% 时终端字最大。
修法：终端格宽高乘 `ui_scale`（或提供"跟随界面缩放"开关，默认跟随）。注意格宽要
`round` 到整像素，否则 0.8 倍下会出现 0.5px 累积误差。

### R7 · 没有字体回退链（P1）

`Font { fallbacks: None }`（`:1849`、`:1035`）⇒ 永远用平台默认 fallback。主流终端都允许
**显式回退链**（iTerm2 "Non-ASCII Font"、WezTerm `font` 数组、Kitty `symbol_map`、
Alacritty `font.normal.family` + 多族）。对中文用户的实际差别：
- 想让"中文用更纱黑、英文用 JetBrains Mono"——做不到；
- Nerd Font 图标（`\ue0b0` 那一批）掉到系统 fallback，宽度不可控，powerline 提示符错位；
- 想用"只有 ASCII 的编程字体 + 专门的 CJK 等宽字体"（最稳的组合）——做不到。

修法：`TerminalSettings` 加 `font_fallbacks: Vec<String>`（设置页一串可增删的族名），
`Font.fallbacks = Some(vec)`。同时把 R1 的 S0 兜底做成"默认回退到 CJK 双宽字体"。

### R8 · 每帧 `resolve_font` + 全行重建（观察项，非结论）

`sync_cell_metrics` 每帧 `resolve_font` + `layout_width`（`:1041`），`render_terminal_session`
每帧重建 `screen_lines` 个 `div + StyledText`（`:1906-1918`）。注释说走布局缓存，但
200×50 的网格每帧仍要产 50 个元素 + 50 次 `with_runs`。**不要凭感觉优化**：先用
`sample` 抓一次持续输出的火焰图（`yes | head -n 200000`），看 `shape_line` 占比。
如果占比高，方向是"行级 memo：行内容 + 样式键不变则复用上一帧的 `AnyElement`"。

### R9 · 配色只有两套，且与"主题"半绑定（P1）

`palette_for`（`:2931`）取 `Palette::dark()/light()` 的 16 个 ANSI 色，只把
fg/bg/cursor 换成主题色。于是：
- 不能选配色方案（Solarized / Dracula / One Dark / GitHub Light），也不能导入 `.itermcolors`；
- 同一个主题下，`ls` 的蓝/绿永远是内核写死的那一组，和界面强调色无关；
- 浅色主题下 ANSI 亮色（8–15）是从深色版调暗的，对比度是否够没有校验（无 minimum contrast）。

主流对照：iTerm2 / Windows Terminal / Tabby 都有"scheme 列表 + 导入 + 每 profile 覆盖"。
修法：`TerminalSettings` 加 `scheme: Option<String>`（内置 6–8 套 + 导入 `.itermcolors`），
`Palette` 从 scheme 构造；`fg/bg` 仍允许"跟随主题"。

---

## 4. 设置缺口矩阵

图例：● 已有 ／ ○ 缺 ／ ◐ 有但不完整。优先级 P0 = 本迭代该做，P1 = 下一迭代，P2 = 可排期。

### 4.1 字体与度量

| 能力 | 主流做法 | 本仓 | 优先级 |
|---|---|---|---|
| 字体族选择 | 只列本机字体、标记未安装、可选等宽过滤（iTerm2 / WT / navop） | ○ 裸文本框 + 弹窗 | **P0** |
| 字号 | 任意 8–32（iTerm2 到 128），滑杆/输入 + ⌘± 即时 | ◐ 下拉 10–20，⌘± 不落盘 | **P0** |
| 行高 | 1.0–2.0 常用档位 + 自定义（WT `cellHeight`、Kitty `adjust_line_height`） | ◐ 弹窗输百分比，无上下限 | P1 |
| 字距 / 列宽 | Kitty `adjust_column_width`、WT `cellWidth` | ○ | P2 |
| 粗体/斜体/粗斜体 字面 | 四个面分别指定（Alacritty / Kitty） | ○ 只给 weight | P2 |
| 字体回退链 | 多族列表（WezTerm / Kitty `symbol_map`） | ○ `fallbacks: None` | **P0**（R7） |
| 连字 | 开关 + 特性串（iTerm2 "Use ligatures"、Kitty `font_features`） | ○ 只开 `tnum` | P1 |
| 制表符/制表线字形 | 内置 box-drawing（Alacritty `builtin_box_drawing`） | ○ 依赖字体 | P2 |
| 内边距 | x/y 分开（WT `padding: "8,16"`） | ◐ 单值 | P2 |
| 字重/粗细 | WT `font.weight`、iTerm2 "thin strokes" | ○ | P2 |

### 4.2 字符与宽度（详见 §5）

| 能力 | 主流做法 | 本仓 | 优先级 |
|---|---|---|---|
| 字符编码 | **逐 profile**：UTF-8 / GBK / GB18030 / Big5 / Shift-JIS / EUC（iTerm2 / SecureCRT / Xshell / navop） | ○ 只有 UTF-8 隐式 | **P0** |
| 入站解码 | 有状态解码器置于 vte 之前（navop `TerminalOutputDecoder`） | ○ 原始字节直进 vte | P0 |
| 出站编码 | 文本输入按目标编码转码（navop `encode_terminal_input`） | ○ | P1 |
| 远端 locale | `-o SetEnv LANG=…` / 会话内 `export LANG` | ○ | **P0**（R-§5.3） |
| 歧义宽度字符 | "按双宽处理"开关（iTerm2 / WT `experimental`） | ○ 固定窄 | P1 |
| Unicode 归一化 | NFC / NFD / 不做（iTerm2） | ○ 不做 | P2 |
| 宽字符对齐 | 逐簇定位或强制双宽 | ○ **自然推进 → 错位** | **P0**（R1） |
| 组合字符/emoji | 逐簇渲染 | ○ 只取首 char | P1（R2） |

### 4.3 光标 / 鼠标 / 选区

| 能力 | 主流做法 | 本仓 | 优先级 |
|---|---|---|---|
| 光标形状 | 块 / 竖线 / 下划线 / 空心（DECSCUSR 已支持 + 用户默认档） | ◐ 只跟应用给的 `CursorShape`，无用户默认 | P1 |
| 光标闪烁 | 开/关 + 频率（iTerm2 / WT `blink`） | ○ 完全不闪 | P1 |
| 光标颜色 | 可用强调色 / 自定义 | ◐ 取 `colors.primary` | P2 |
| 选区颜色 | 可配 + 不透明度 | ◐ 固定 `drop_target` 20% | P2 |
| 选中即复制 | 开关（X11 习惯；iTerm2 "Copy to pasteboard on selection"） | ○ | P2 |
| 右键行为 | 菜单 / 粘贴 / 扩展选区（可配） | ◐ 菜单固定三项 | P2 |
| 滚轮速度 | 行/页 倍率（WT `rowsToScroll`） | ○ 固定 | P2 |
| 备用屏滚轮转方向键 | 开关（`alternateScroll`） | ○ | P2 |
| 三击选行 / ⌥ 块选 | 有 | ● `SelectionKind` 四档齐全 | — |

### 4.4 配色

| 能力 | 主流做法 | 本仓 | 优先级 |
|---|---|---|---|
| 配色方案 | 内置多套 + 导入（`.itermcolors` / JSON） | ○ 两套写死 | P1 |
| 每连接覆盖 | profile 级 scheme | ○ | P2 |
| 前景/背景跟随主题 | 有 | ● `palette_for` | — |
| 最低对比度 | 自动提亮（iTerm2 "minimum contrast"、WT） | ○ | P2 |
| 背景不透明度/模糊 | 有（iTerm2 / WT `opacity`） | ○ | P2 |
| bold 转亮色 | 开关（WT `intenseTextStyle`） | ○ | P2 |

### 4.5 终端行为

| 能力 | 主流做法 | 本仓 | 优先级 |
|---|---|---|---|
| `TERM` | 可选 `xterm-256color` / `xterm` / `screen-256color` / `linux` | ○ SSH 路径丢失 | **P0** |
| `COLORTERM` | `truecolor` | ○ SSH 路径丢失 | **P0** |
| 响铃 | 无 / 视觉 / 声音（iTerm2 三档 + 每 profile） | ○ BEL 丢弃，无任何反馈 | P1 |
| Backspace 序列 | `0x7f` / `0x08` 可切（Xshell / SecureCRT / oxideterm） | ○ 固定 `0x7f` | P1 |
| Delete 序列 | `CSI 3~` / `CSI 3;5~` | ◐ 固定 | P2 |
| ⌥ 作为 Meta | 开关（iTerm2 "Option as Meta"；关掉才能打 `é`） | ○ **恒发 ESC 前缀**，打不出组合字符 | P1 |
| 粘贴前确认（多行） | 有 | ○（见 UI 报告 P0） | P0 |
| 关闭标签时确认运行中进程 | 有 | ○ | P1 |
| KeepAlive | `ServerAliveInterval=30`（已有） | ● | — |
| 会话日志 / 录制 | 有（SecureCRT / oxideterm） | ○ | P2 |

### 4.6 作用域与生效时机（最容易被忽略的一组）

| 能力 | 主流做法 | 本仓 | 优先级 |
|---|---|---|---|
| 全局 vs 每连接 | 字体/配色/编码都可 per-profile 覆盖（iTerm2 profile、SecureCRT session） | ○ 全应用一套 | P1 |
| 改完即时生效 | 字体/字号/行高/配色改完立刻重排 | ○ 字号要重启（R5） | **P0** |
| 恢复默认 | 每组一个"恢复默认"（WT / iTerm2） | ○ | P1 |
| 设置项可被搜索到 | 搜到**具体条目**并跳转高亮（WT / iTerm2） | ◐ 只搜页标题+描述（`settings/mod.rs:838`） | P1 |
| 设置项单位与范围 | 输入框带单位、越界即时红字 | ◐ 弹窗里一句 hint，无校验 | P1 |
| 预览 | 改字体/字号有实时样例 | ○ | P2 |

> 设置搜索那条特别值得修：现在搜「行高」「连字」「编码」「字符集」**一个都搜不到**
> （`终端` 页的描述只有"字号、回滚与会话行为"）。加关键词数组 + 命中后跳到该行并高亮，
> 成本很低，感知很强。

---

## 5. 字符集：为什么不是一个下拉框

这一节单独写，因为"加个编码下拉"是最容易被低估的一项。

### 5.1 解码必须插在 vte 之前，且必须有状态

现在的字节流是 `PTY read → alacritty EventLoop → vte(UTF-8) → 网格`，中间没有钩子。
GBK 的双字节可能被 `read()` 切成两半：`0xB8 0xFA` 第一次只到 `0xB8`。所以解码器必须是
**有状态的流式解码器**（navop 用 `encoding_rs::Decoder` + `decode_to_utf8(..., last)`）。
落点只有一个：包一层实现 `EventedReadWrite` 的 Pty 适配器，在 `read()` 里转码后再喂给
EventLoop。改动集中在 `duck-term`，约 1 个新文件 + `spawn` 传一个 `encoding` 参数；
**但要注意**：转码后长度变化，`EventLoop` 的读缓冲不能假设"一字节进一字节出"。

### 5.2 出站也要转

用户输入的 UTF-8 要按目标编码转回去（`encode_terminal_input`）。只转文本来源
（键盘 / 粘贴 / 脚本），**不能转**功能键序列与控制字节——它们是 ASCII，转了也无害，
但混合输入（`echo 中文`）必须整段转。UTF-8 档直接 `Cow::Borrowed` 零开销。

### 5.3 比编码更常见的是"远端 locale 没设"

即使编码设成 GBK，如果远端 `LANG` 是空的，`ls` 仍可能输出 `?`。
主流 SSH 客户端都接受这个事实：**编码是客户端的事，locale 是服务端的事**，
但客户端至少要能把 `LANG` 送过去：

- 首选 `ssh -o SetEnv LANG=zh_CN.UTF-8`（OpenSSH 7.8+，需服务端 `AcceptEnv`）；
- 退路：连接建立后自动发一行 `export LANG=…`（可选，默认关，避免污染用户 shell）。
- 同时 **`TERM` 必须显式给**（现在的 SSH 路径没有，见结论 4）。

### 5.4 歧义宽度与归一化

- **歧义宽度**（`○●★※±`、希腊字母、制表符）：Unicode 定义为"ambiguous"，中日韩环境
  通常按 2 格显示。iTerm2 有 "Treat ambiguous-width characters as double width"。
  我们固定按 alacritty 的 `unicode-width`（窄），中文用户会觉得"框线/符号挤在一起"。
- **NFC/NFD**：macOS 文件名是 NFD（`é` = `e` + U+0301）。远端是 NFC。不做归一化时，
  复制过去的路径"看起来一样但匹配不上"。iTerm2 提供三档，默认 None。
  与 R2（丢弃 zerowidth）叠加时，NFD 文件名会**直接显示成缺字**。

---

## 6. 目标设置信息架构（建议）

保持"单键 JSON + 全 `Option` + `serde(default)`"的既有约定，只扩字段与分组。
下面 7 组 ≈ 26 项，其中 P0 的 9 项是本次要做的。

**① 字体**（`font`）
`family`（可搜索下拉，仅本机）· `fallbacks[]`（可增删）· `size`（9–32，行内 stepper + ⌘±）·
`line_height`（1.0–2.0，档位 1.0/1.2/1.4/1.6 + 自定义）· `ligatures`（开/关）·
`bold_as_bright`（开/关）

**② 字符**（`text`）
`encoding`（UTF-8 默认 / GBK / GB18030 / Big5 / Shift-JIS / EUC-JP / EUC-KR / Windows-1252，
**每连接可覆盖**）· `ambiguous_width`（窄/双宽）· `normalization`（无/NFC/NFD）·
`remote_lang`（不设置 / 跟随系统 / 自定义，写到 `SetEnv LANG=`）

**③ 光标与选区**（`cursor`）
`shape`（跟随应用/块/竖线/下划线/空心）· `blink`（关/慢/快）· `color`（强调色/自定义）·
`selection_color` · `copy_on_select`（关）

**④ 配色**（`color`）
`scheme`（跟随主题前景背景 + 8 套内置 ANSI）· `minimum_contrast`（0–1，0=关）·
`background_opacity`（1.0，1.0=不透明）

**⑤ 终端行为**（`behavior`）
`term`（xterm-256color 默认）· `colorterm`（truecolor 默认）· `bell`（无/视觉/声音）·
`backspace`（DEL/Ctrl-H）· `option_as_meta`（开）· `alternate_scroll`（开）·
`scroll_speed`（3 行/格）· `confirm_close_running`（开）

**⑥ 会话**（`session`，保留现有 4 项）
`scrollback`（10k / 50k / 100k / 自定义 / 无限+内存提示）· `auto_reconnect` ·
`reconnect_max_attempts` · `reconnect_backoff_secs`

**⑦ 显示**（`view`）
`padding_x` / `padding_y`（分开）· `follow_ui_scale`（开）· `cursor_trail`（关，P2）

作用域规则写死一句话：**这七组都是"会话默认值"；连接档案里只覆盖 `encoding` / `term` /
`scheme` / `font` 四项**——其他项 per-connection 覆盖的收益远小于"设置项爆炸"的成本。

---

## 7. 与主流工具的操作习惯对照

| 习惯 | iTerm2 | Windows Terminal | Kitty / WezTerm | SecureCRT / Xshell | navop | oxideterm | 本仓 |
|---|---|---|---|---|---|---|---|
| 字体选择器（列本机、标未安装） | ● | ● | 配置文件 | ● | ● | ● | ○ 裸文本框 |
| 字号即时 + 持久 | ● | ● | ● | ● | ● | ● | ◐ 即时但不持久 |
| 行高可调 | ● | ● `cellHeight` | ● | ◐ | ○ | ● | ◐ 弹窗无范围 |
| 连字开关 | ● | ○ | ● | ○ | ○ | ● | ○ |
| 字体回退链 | ● 非 ASCII 字体 | ◐ | ● `symbol_map` | ● | ○ | ● | ○ |
| 逐 profile 编码 | ● | ○ | ○ | ● | ● | ● | ○ |
| 歧义宽度 | ● | ○ | ● | ○ | ○ | ◐ | ○ |
| 光标形状 + 闪烁 | ● | ● | ● | ● | ◐ | ● | ◐ 仅形状 |
| 配色方案 + 导入 | ● | ● | ● | ● | ● | ● | ○ 两套写死 |
| 响铃三档 | ● | ● | ● | ● | ○ | ◐ | ○ 无 |
| `TERM` 可选 | ● | n/a | ● | ● | ● | ● | ○ |
| 设置项搜索 | ● | ● | 配置文件 | ◐ | ○ | ● | ◐ 仅页级 |
| 每连接覆盖 | ● | ● profile | ● | ● | ● | ● | ○ |

读法：**● 有 / ◐ 部分 / ○ 无**。一眼能看出两件事——(a) 我们在"字体选择、编码、
配色、TERM"四格上是○，而这四格恰好是 SSH 客户端用户最先调的；(b) 在"行高、光标、
设置搜索"三格上我们有但残（◐ 比 ○ 更糟：用户以为能用）。

---

## 8. 落地顺序

### S0 · 对齐与真源（P0，1 周内，不动数据模型以外的东西）

1. **SSH 环境变量补齐**：`target.env()` 里加 `TERM` / `COLORTERM` / `LANG`（或
   `terminal_argv` 加 `-o SetEnv`）。验收：远端 `echo $TERM; echo $COLORTERM; locale`
   三行正确，`ls --color` 出 24 位色。
2. **字号单一真源**：⌘± 写回 `kv`；设置页下拉与页面字段同源；区间提到 `9..=32` 并
   只留一处常量。验收：改字号→重启→仍是该值；两处常量只剩一处。
3. **段级绝对定位止血**（R1 的 S0）：行内 run 改为按起始列绝对定位。
   验收：`python3 -c "print('中文'*40)"` 后，末字起始列与第 80 列格线误差 < 1px
   （字形可右溢 2.4px，但**列起点不再漂移**）。
4. **字体选择器**：复用外观页的 `font_options`；选中后显示"等宽 是/否"。
   验收：输入未安装族名不可能（只能从列表选）；选比例字体有黄条警告。

### S1 · 渲染正确性（P0/P1，2–3 周）

5. **逐簇定位重排**（R1 正解 + R2）：`CellPaint.ch → String`；行按 cluster 切段，
   段级绝对定位。验收：中文 200 列不错位；emoji ZWJ 完整；组合重音正确；
   IME 候选窗贴住光标。
6. **SGR 补全**（R3）：DIM 降亮、双/波浪/点线下划线、`underline_color`。
   验收：`printf '\e[2m dim \e[22m \e[4:3m curl \e[24m \e[58;5;1m red-ul \e[59m\n'`。
7. **界面缩放联动**（R6）。验收：150% 界面下终端字号同步变大，格宽取整无累积误差。

### S2 · 编码与配色（P1，2 周）

8. **流式解码 + 出站转码 + per-connection encoding**（§5）。
   验收：连一台 GBK 机器，`ls` 中文、`cat` 中文、粘贴中文命令三步全对；
   UTF-8 会话零性能回归（`Cow::Borrowed` 路径）。
9. **配色方案 + 导入**（R9）。验收：切换 scheme 立即生效、重启保留、`.itermcolors` 导入可用。

### S3 · 手感（P2）

10. 光标闪烁 / 形状默认档 / 选中即复制 / 滚轮速度 / 响铃三档 / 关闭前确认。
11. 设置项级搜索（关键词数组 + 跳转高亮）、每组"恢复默认"、字体预览行。

---

## 9. 可测验收清单

| # | 操作 | 期望 |
|---|---|---|
| A1 | `python3 -c "print('中文中文中文'*10)"` | 末字仍在网格内；光标贴在字上；选区起止与字形对齐 |
| A2 | 同 A1 后按 End / 拖选到最后 | 选区右边界 = 最后一列，不多不少 |
| A3 | `printf 'e\xcc\x81 \xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa9\n'` | 显示 `é` 与两人 ZWJ emoji，不是 `e` / 单人 |
| A4 | 字号 20 → 重启 | 仍是 20；设置页下拉显示 20 |
| A5 | 界面缩放 150% | 终端字号随之变大；格宽为整像素（截图量） |
| A6 | 选"Helvetica"作字体 | 出现"非等宽"警告；不静默错位 |
| A7 | 连 GBK 服务器 `ls` | 中文正常；粘贴 `echo 你好` 远端收到 GBK 字节 |
| A8 | 远端 `echo $TERM $COLORTERM` | `xterm-256color truecolor` |
| A9 | 切换配色 scheme | 立即生效、重启保留 |
| A10 | 设置搜索"行高"/"编码"/"连字" | 命中终端页对应行并高亮 |
| A11 | `printf '\e[2mX\e[0m'` | X 明显比正常字暗 |

---

## 10. 明确不做

- **sixel / kitty graphics / iTerm2 inline images**：内核要加图像协议与图层，收益集中在
  `imgcat` 小众场景；先不做，写进能力矩阵的"不做"。
- **字距/列宽任意调节**、**逐字形自定义 advance**：与 R1 的 cluster 定位冲突，做了会互相打架。
- **GPU shader / 字体抗锯齿模式**：GPUI 不暴露，硬做等于 fork 渲染栈。
- **每连接覆盖全部设置项**：只覆盖 encoding / term / scheme / font 四项，其余保持全局。
- **SGR 5/6 闪烁**：主流（Alacritty、WT 部分）也不做，归"不做"而不是"没做"。

---

## 11. 证据索引

| 结论 | 位置 |
|---|---|
| 终端设置仅 8 字段 | `crates/duck-app/src/pages/module_settings.rs:79-125` |
| 字号区间两处硬编码 | `module_settings.rs:36`、`crates/duck-app/src/pages/terminal/mod.rs:133` |
| 字号只读一次 | `crates/duck-app/src/pages/terminal/helpers.rs:9-15`（注释自认） |
| ⌘± 不落盘 | `terminal/mod.rs:628-647`（无 `save` 调用） |
| 字体族走弹窗 | `module_settings.rs:1090`（`text_setting_row`） |
| 外观页已有字体下拉 | `pages_appearance.rs:710`、`settings/state.rs:38` |
| 设置搜索只到页级 | `settings/mod.rs:838-839` |
| 格宽只量 `'m'` | `terminal_canvas.rs:1041-1046` |
| `fallbacks: None` | `terminal_canvas.rs:1035`、`:1849` |
| 整行自然排版 | `terminal_canvas.rs:1906-1918`、`build_row` `:2738` |
| 宽字符占位变空格 | `terminal_canvas.rs:2695-2706` |
| 单 `char` 丢 zerowidth | `terminal_canvas.rs:2672`、`vendor/alacritty-terminal/src/term/cell.rs:125-139` |
| DIM/彩色下划线未处理 | `terminal_canvas.rs:2693-2730` vs `cell.rs:15-36` |
| 光标/选区按列号 | `terminal_canvas.rs:2805-2831`、`:2027-2040` |
| IME 用列号 | `terminal_canvas.rs:1955-1959` |
| 配色两套写死 | `terminal_canvas.rs:2931-2944`、`duck-term/src/palette.rs:31-120` |
| SSH 丢失 TERM/COLORTERM | `terminal_canvas.rs:533-537`、`duck-ssh/src/target.rs:405-433`、`duck-term/src/session.rs:82-86` |
| 原始字节进 vte | `duck-term/src/session.rs:487`（`EventLoop::new`） |
| 参考实现：流式解码 | `temp/navop/crates/terminal/src/encoding.rs:51-111` |
| 参考实现：设置项集合 | `temp/oxideterm/docs/user-guide/zh-Hans/desktop.md:96-108` |
