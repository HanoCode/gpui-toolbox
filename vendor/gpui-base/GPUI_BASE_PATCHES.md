# GPUI_BASE_PATCHES — vendored gpui-base 0.7.1 的补丁记录

上游：`gpui-base` **0.7.1**（crates.io，Apache-2.0，edition 2024）。
来源：`~/.cargo/registry/src/…/gpui-base-0.7.1` 原样拷入（未含 `Cargo.lock`、`.cargo-ok`）。

## 为什么 vendor

`TextView::html` 的文档模型**不解析任何内联 CSS**：`text/format/html.rs` 的
`parse_paragraph` 兜底分支丢弃所有 `style` 属性，`TextMark` 连 color / font-size
字段都没有（上游 main 分支 0.7.3 逐字段一致，升级无解）。营销邮件的彩色标题、
灰色脚注、按钮文字因此全部渲染成同一字号同一颜色——邮件模块「正文排版混乱」的
根因（2026-10-13 定性，Flectar Mail 用的是 Blitz CSS 引擎，MSRV 1.92 不可移植）。

本补丁分两层把内联 `style` 提出来：

1. **行内层** → `TextMark`：`color`、`font-size`、`font-weight`（≥600 → bold）、
   `background-color`（→ 高亮）、遗留的 `<font color>` 属性。
2. **块层** → `BlockStyle`（挂在 `BlockNode::Root` 上）：`background-color` /
   `background` / `bgcolor`、`padding`（简写与四条长手）、`text-align` / `align`。

仍然被忽略的是**需要布局引擎**的部分：`%` / `em` 的 padding（要知道容器宽度或字号）、
`width` / `height` / `margin` / `border-radius` / `flex` / 定位，以及外部样式表与
`<style>` 块。

## 补丁清单（全部以 `DUCK PATCH` 注释标记）

| 文件 | 改动 |
| --- | --- |
| `src/text/node.rs` | `TextMark` 新增 `color: Option<Hsla>`、`font_size: Option<f32>`（CSS px）；`merge` 按 CSS 级联（内层声明覆盖继承值）；`mark_highlight` 把两者写进 `HighlightStyle` / `InlineHighlight`；4 处链接着色点改为「`<a>` 自带 color 时优先于主题链接色」；`text_size_ranges` 调用传基准字号 |
| `src/text/inline.rs` | `InlineHighlight` 新增 `font_size: Option<Pixels>`（绝对字号随 highlight 层叠）；`text_size_ranges` 增加 `base_font_size` 参数，绝对字号换算成段内 scale |
| `src/text/inline_flow.rs` | 两处 `text_size_ranges` 调用传 `font_size` |
| `src/text/format/html.rs` | `css_color`（`#hex` / `rgb()` / `rgba()` / 常用命名色）、`css_length`（`px` / `pt` / `em` / `%`）、`style_mark`（继承 + 声明级联）；`parse_paragraph` / `parse_node` / `parse_table_row` / `parse_table_cell` / `consume_children_nodes` 增加 `inherited: Option<&TextMark>` 贯穿——**块元素是继承边界**（`div`/`p`/`td`/`li`/`h1-6` 的 `style` 下传给全部子内容）；未知内联元素（`span`/`font`/…）的 `style` 不再被丢弃（含内联 `background-color` → 高亮、遗留 `<font color>`） |
| `src/text/format/html.rs`（块层） | `block_style()` 读容器的 `background-color` / `background`（取第一个颜色 token）/ 遗留 `bgcolor`、`padding` 简写 + 四条长手（长手覆盖简写）、`text-align` / 遗留 `align`；`with_block_style()` 把结果挂到容器上；`<center>` 新增独立分支（块级 + `text-align:center`——它不在上游的块级清单里，原先会掉进行内路径丢掉换行）；带样式无文字的容器**不再被丢**（空色带仍然画得出来） |
| `src/text/node.rs`（块层） | 新增 `BlockStyle`（`background` / `text_align` / 四边 `padding_*`，`is_empty()` 判空）；`BlockNode::Root` 增加 `style: BlockStyle`；`compact()` **不折叠带样式的单子节点容器**（折叠会把背景 / 内边距吃掉）；`render_block` 的 `Root` 分支把三者落到 gpui：`.bg()` / `.pt().pr().pb().pl()` / `.text_center()|.text_right()` |
| `src/text/format/markdown.rs` | `Node::Html` 分支构造 `Root` 时补 `style: BlockStyle::default()`（`HtmlFile` 内层块自带样式） |
| `src/text/node.rs`（元素 id，**崩溃修复**） | `leaf_element_id` 增加 `ix`：span 缺失（HTML）时的回落从裸 `"p"` 改成 `("p", ix)`。`Paragraph::render` 增加 `ix` 参数，4 个调用点分别传 `options.ix`（段落 / 标题）与 `cell_ix`（表格单元格）；blockquote 与 list-item 的子块改为传**各自的兄弟下标**（原先用 `..options` 继承外层 `ix`，兄弟之间照样撞） |
| `src/text/inline_flow.rs`（兜底） | 新增 `PositionedFragment::item_ix()`；`prepaint` 开头校验「layout 里每个 fragment 的 `item_ix` 都落在 `items` 范围内」，不满足则 `return Vec::new()`（当帧不画，而不是越界 abort） |

### 元素 id 必须在同一父节点下唯一（2026-10-14，崩溃修复）

gpui 的元素状态按 **id 路径**（`GlobalElementId`）索引，**对兄弟不做去重**：

```rust
window.element_id_stack.push(element_id);   // element.rs::prepare_element_id
```

两个**兄弟**元素只要本地 id 相同，全局 id 就相同，状态也就是同一份（这里正是
`InlineFlow` 的 `Rc<RefCell<InlineFlowFrameState>>`：layout 缓存 + 逐 fragment 的选择
状态）。埋在里面的雷：

- HTML 解析器**不记录 span** → 每个 `<p>` 的 `fade_key` 都是 `None`
  → `leaf_element_id` 原先一律返回 `"p"` → **所有 HTML 段落共用一个 id**；
- 于是 A 段可能拿到 B 段（item 更多）的 layout，`prepaint` 用 B 的 `item_ix` 索引 A 的
  `items` → 越界 panic（`inline_flow.rs` 的 `Image` 分支）→ **abort 整个进程**。

修法是把这条不变量补回来：`("p", ix)` 里的 `ix` 是**兄弟下标**——`"p"` 不是任何块用的
`kind`，因此也不会与 `block_element_id` 撞。`inline_flow.rs` 的 up-front 校验只是**兜底**：
万一还有漏网的同 id 共享，当帧不画这个 flow（下一帧按当时的 items 重新布局）。

⚠️ 以后再给「每个段落 / 每个块」派 id，**先确认它在同一父节点下唯一**：位置型 id
（兄弟下标）安全，常量 id 不安全。

### 块级样式的语义口径

- **背景、内边距不继承**（CSS 如此）：内层容器只画自己声明的，未声明就透出外层——
  嵌套卡片天然正确。
- **`text-align` 继承**：`Root` 上用 gpui 的 text-style refinement，子树自动继承，
  内层声明自己的就覆盖（CSS 语义）。
- **`text-align: left` 不额外写 refinement**（gpui 默认就是左对齐），但**会**作为
  一次 refinement 覆盖外层继承下来的居中——这正是内层 `align="left"` 该有的效果。
- `padding` 只认绝对长度（`px` / `pt` / 无单位）；`%` 与 `em` 需要容器宽度 / 字号，
  文档模型没有这个上下文，保持 gpui 默认。

## 测试

`src/text/format/html.rs` 测试模块新增 15 条。内联层 7 条（`test_inline_style_*` /
`test_block_style_inherits_into_child_paragraph` / `test_nested_style_overrides_inherited` /
`test_table_cell_style_inherits` / `test_css_*`）；块层 8 条（`test_block_style_*` /
`test_heading_and_table_keep_their_block_style` / `test_inline_elements_keep_their_declared_style`）。

`src/text/node.rs` 测试模块新增 1 条：`span_less_paragraphs_get_distinct_element_ids`
（钉住上面那条 id 不变量：两个 span 缺失的段落 id 必须不同，且不与带 span 的段落、
不与其它 `kind` 的块 id 相同）。

⚠️ gpui-base 是 patch crate（非 workspace 成员），**不能从工作区跑** `cargo test -p gpui-base`。
验证方法：临时在 `vendor/gpui-base/Cargo.toml` 末尾追加空 `[workspace]` 表，
`CARGO_TARGET_DIR=/tmp/gpui-base-target cargo test --lib`（或加 `text::format::html`
只跑解析器），跑完**立即删掉**该表并 `rm -f Cargo.lock`（会拉 crates.io 的
gpui-pre 0.3.8，与工作区的 gpui-fast 补丁不同源，仅供测试，不进锁文件）。
最近一次：**1323 passed / 0 failed**（全量 lib）。

## 与邮件管线的耦合（改这里之前先看）

邮件正文进 `TextView::html` 之前会先过 `duck_mail::mime::flatten_email_tables`
（把 `table / tbody / tr / td / th / center` 整族改写成 `<div>`）。于是：

- **`<td style="background:#eee;padding:20px">` 是走块层 `Root` 生效的**，
  不是表格单元格路径——所以拍平时**必须保留 `style`**（`duck-mail/src/mime.rs`
  的 `div_style_attr()` 同时把遗留 `bgcolor` / `align` 翻成等价声明）。
  改 `flatten_email_tables` 的属性策略会直接改掉邮件的观感。
- 因此**表格单元格自身的块级样式没做**：拍平后邮件根本不走 `TableCell` 渲染，
  而给 `TableCell` 加 padding 还得同步改列宽测算（`CELL_PAD_PX` 那条缓存假设），
  为一个不可达路径不值得动。真数据表（Markdown 表格、未拍平的 HTML）保留上游行为。

## 文本选中：光标与拖选性能（2026-10-16，`DUCK PATCH`）

用户报「可选中的文字悬停时鼠标不变形」「拖选/复制十分卡顿」，两处都在
上游的 `SelectableText::paint`：

1. **I 型光标**（`src/selectable_text.rs`）：上游**从不**调
   `Window::set_cursor_style`，可选文字悬停时仍是箭头。补上
   `window.set_cursor_style(CursorStyle::IBeam, hitbox)` —— 用本 run 自己的
   hitbox 申请，所以只有真有文字的地方才变形（导入 `CursorStyle`）。

2. **拖选卡顿**（`src/selectable_text.rs` + `src/text_selection.rs`）：上游在
   paint 里前后各调一次 `TextSelection::selected_text(window, cx)` 比对，据此
   决定是否 `window.refresh()`。该函数是**全窗口聚合**（遍历每个参与者 →
   clone 各自选中子串 → 排序 → join），而 paint 是**每个 run 每帧**都跑 ⇒
   代价 O(runs × 选中长度)：全站上千个可选 run 时，拖选一格每帧要拷贝几十 KB
   到几 MB 字符串，帧率直接塌。两次读之间只有**本 run 自己**的投影会变
   （`update_runs` 只改自己那份），故新增 `TextSelectionHandle::projected_copy_text`
   只读自己那份做比对 —— 语义等价（触发 `refresh` 的条件不变），代价降到
   O(本文长度)。**不要**改回全窗口比对。

## 升级约定

从上游重新拷入 0.7.x 后，按上表重放（检索 `DUCK PATCH`），并同步 `vendor/README.md` 的基线行。
依赖集合未变（patch 不增删 crate），`THIRD_PARTY_LICENSES.md` 无需变化；
但 `[patch.crates-io]` 新增了一行，见根 `Cargo.toml`。
