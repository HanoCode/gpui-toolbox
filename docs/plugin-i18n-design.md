# 插件国际化设计（plugin i18n）

> 目标：插件自带翻译文件，**跟随宿主「外观 → 语言」的中英文开关实时切换**；
> JS 插件与原生插件（wasm / process）共用**同一套契约**。
> 前置：宿主自身的 i18n 已迁到 `locales/en.toml`（见本仓 2026-10-15 迁移，`docs/` 各设计文档口径已同步）。

---

## 1. 问题：插件文案现在是怎么翻的

宿主所有文案的约定是「**中文源串即键**」：`t(lang, "外观")` 查 `locales/en.toml`，
miss 回落源串（`crates/duck-theme/src/i18n.rs:424`）。

插件的文案**也走这条路**，但查的是**宿主的表**：

| 位置 | 现状 | 证据 |
|---|---|---|
| 工具箱工具卡 | `t(lang, tool.name)` 查宿主表 | `crates/duck-app/src/pages/toolbox.rs:306`、`:405` |
| 插件工具执行提示 | `tr(cx, name)` 查宿主表 | `crates/duck-app/src/plugins.rs:343` |
| 工具注册 | 清单里的 `String` 被 `leak_str` 提成 `&'static str` 再交给宿主取词 | `crates/duck-app/src/plugins.rs:484`、`:554` |
| 插件中心卡片 | **原文直出，完全没翻译** | `crates/duck-app/src/pages/plugin_center.rs:753` |

> ⚠️ **上表是「改造前」快照，行号亦然。** P1–P5 已落地，今天这四处都改走**插件自己的表**
> （对照 `crates/duck-app/src/pages/toolbox.rs:307`、`:410`、
> `crates/duck-app/src/plugins.rs:390`、`crates/duck-app/src/pages/plugin_center.rs:772`）。
> 保留本节是为了讲清「为什么要新做一层」——**不要照上表的行号去改代码**。

`ToolMeta` 的三项都是 `&'static str` 且被注释明确标注为「中文源串，渲染时经 `tr` 取词」
（`crates/duck-tools/src/meta.rs:27-29`、`:45`）。

**结论**：第三方插件想让自己的文案支持英文，只能把译文塞进**宿主的** `locales/en.toml`。
这不可能成立——插件是独立分发、独立升级的，宿主不该知道、也无法收录任意插件的文案。
而且插件中心那张卡（`:753`）干脆不翻译，说明这条路本来就没走通。

---

## 2. 核心决策：翻译是**数据**，不是代码

一句话：**`<plugin_dir>/locales/<tag>.toml` 是唯一接口；由宿主解析、投影、按当前语言取词。**

为什么这样切能同时覆盖 JS 插件与 Rust 插件：

- **宿主侧渲染的文案**（工具卡名、插件中心卡片、设置页里的插件名）——
  与插件用什么语言写代码**完全无关**，宿主直接读插件的 `locales/` 即可覆盖。
- **插件侧运行时产出的文案**（`duck.notify(...)`）——
  通过取词 API 读**同一张表**，只是传输层不同（JS 全局 / 环境变量 / wasm import）。

于是「JS 插件 vs Rust 插件」的差异被压缩到**取词 API 的传输层**，翻译文件本身零差异。

---

## 3. 目录与文件约定

```
<plugin_dir>/
├── plugin.toml            # 清单（新增可选 [i18n] 段，见 §5）
├── main.js                # JS 入口（wasm / process 各用自己的入口）
└── locales/               # 【新增】插件自带翻译，随包分发、只读
    ├── en.toml            # tag = "en"
    └── zh-Hans.toml       # tag = "zh-Hans"（可选；源串通常本来就是中文）
```

TOML 格式与宿主**完全同构**：根级平铺、quoted key、值里可含 `{}` 占位符、顺序无关。

```toml
"文本统计" = "Text stats"
"统计剪贴板文本的字符、单词与行数" = "Count characters, words, and lines in clipboard text"
"报告已写入 {}" = "Report written to {}"
```

同构的三条理由：

1. **复用写法心智**：插件作者已经在写 `plugin.toml`，再学一套 key 命名是额外负担；
2. **「源串即键」让降级天然成立**：某条没翻 → 直接回落该条源串，不会翻成空串；
3. **key 就是清单里的原文**：`name` / `description` / `[[tools]].name` 本身就是 key，
   插件作者不用维护「清单字段 ↔ key」的映射表。

> 为什么不让插件也走 `rust-i18n`：`i18n!` 宏是**编译期**读目录（`crates/duck-theme/src/i18n_backend.rs:30`），
> 而插件是**运行期**才出现的，装不进宏；且 rust-i18n 无复数 API、无语言降级链。
> 插件侧用 `toml` + `BTreeMap` 更贴合，且 `duck-plugin` 已有 `toml` 依赖（零新增）。

---

## 4. 语言 tag 与降级链

宿主侧已有 wire 值（`crates/duck-theme/src/i18n.rs:33-39`）：`"system" | "zh-Hans" | "en"`；
`Lang` 解析后是 `Zh | En`（`:74-79`）。映射：

| `Lang` | tag | 文件名 |
|---|---|---|
| `Zh` | `zh-Hans` | `locales/zh-Hans.toml` |
| `En` | `en` | `locales/en.toml` |

**降级链（由细到粗，只向下降级，任一命中即返回）**：

1. 精确 tag —— `en` → `en.toml`
2. 主语言子标签 —— `en-US` → `en.toml`（当前两档用不到，为将来 `zh-Hant` / 区域变体预留，成本≈0）
3. 插件声明的 `source` tag（§5）——「源串语言」对应的文件；通常不提供，因为源串本身就是它
4. 宿主表（可选档，见 §12）
5. 全部 miss → **原样返回源串**

---

## 5. 清单扩展（`plugin.toml`）

```toml
[i18n]
# 插件源串使用的语言 tag。默认 "zh-Hans"（与宿主源串约定一致）。
# 决定「所有语言都 miss 时回落到哪一份」，也让「英文优先」的插件能自然写作。
source = "zh-Hans"

# 可选：显式声明随包分发的语言文件（相对插件目录，必须落在 locales/ 下）。
# 缺省 = 自动发现 locales/*.toml。显式声明用于：
#   ① 打包白名单校验  ② 让「本插件提供哪些语言」可被索引 / 市场展示
#   ③ 防止 locales/ 里混进无关 toml
locales = ["locales/en.toml"]
```

字段全部 `Option` + `#[serde(default)]`，`deny_unknown_fields` 保持不变
（`crates/duck-plugin/src/manifest.rs:177-198`）——**老清单零改动即可解析**。

**「英文优先」的插件**：作者用英文写 `plugin.toml` 的 `name` 等字段，声明
`source = "en"`，再给一份 `locales/zh-Hans.toml`（中文译文）。
降级链第 3 档保证中文用户看中文、英文用户看源串。

**API 版本**：纯字段新增（老插件不受影响），**保持 `api_version = 1`**，不走 2.0 双轨。
备选：若希望插件能显式「要求宿主支持 locales」，可抬到 2 并保留 1 的加载轨道——
代价是多一条加载分支与一套测试。

---

## 6. 分层落点（关键：内核零 gpui）

```
duck-plugin（内核 crate，零 gpui，复用既有 toml 依赖 ⇒ **零新增依赖**）
  └── mod i18n（crates/duck-plugin/src/i18n.rs）
        PluginLocales::load(dir, source_tag, declared) -> PluginLocales  // **永不失败**，只 warn+skip
        PluginLocales::lookup(tag: &str, source: &str) -> Option<&str>   // 含 §4 降级链
        PluginLocales::text(tag, source) -> &str                         // miss 折进源串（三轨共用）
        PluginLocales::available() -> Vec<&str>                          // 索引/市场展示用
        locales_dir(plugin_dir) / locale_env(plugin_dir, lang)           // 原生轨契约，见 §9
        —— 纯函数、可独立单测、零新增依赖
        —— 语言入参是 &str（tag），不是 Lang 枚举 ← 这是它不依赖 duck-theme 的关键

duck-app（宿主接线，已有插件索引与 leak_str 去重缓存）
  └── PluginLocaleState 全局：plugin_id -> PluginLocales
        （扫描时构建；init / rescan 都走 install_index，**两份状态同步重建**）
      plugin_text_of_plugin(cx, lang, plugin_id, source) -> &'static str   // :146
      plugin_text_of_tool(cx, lang, tool_id, source)     -> &'static str   // :162，内部走 plugin_for_tool
      plugin_text_owned(cx, lang, plugin_id, source)     -> &'static str   // :179，市场卡片用
      —— 「Lang → tag」由 Lang::tag()（duck-theme/src/i18n.rs）直接给，**不需要 resolve_locale**

duck-plugin-js（JS 轨）
  └── run_tool(..., lang: &str)   // 新增第 7 个参数（crates/duck-plugin-js/src/lib.rs:109）
      install_host_functions：新增 duck.locale / duck.t(source, ...args)   // :220 起

duck-theme（基本不变）—— 只管宿主自己的文案；本方案只给它加了 Lang::tag()
```

⚠️ 与初稿蓝图的两处差异（照实记录，别照蓝图找代码）：`load` 是**三参**
（多了 `source_tag`，因为源串语言由清单 `[i18n] source` 决定）；`resolve_locale` **没有实现**
——`Lang::tag()` 是 `const fn`，拿 `cx` 绕一圈没有收益。

**为什么 `duck-plugin` 不能依赖 `duck-theme`**：`duck-theme` 依赖 `gpui`
（`crates/duck-theme/Cargo.toml`），而 `duck-plugin` 的分层纪律是「清单校验与能力仲裁
必须是可独立单测的纯函数」（`crates/duck-plugin/src/lib.rs` 文件头）。
跨过去会污染内核层。所以插件 i18n 层**只认 `&str` tag**，`Lang → tag` 的转换留在 `duck-app`。

---

## 7. 宿主侧取词改造（宿主渲染的文案）

**必须在渲染期解析，不能烘进 `ToolMeta`**：语言切换靠全窗口 `refresh()`
（见 `AGENTS.md §3.3`），烘进 `ToolMeta` 就得重建整个注册表才能切语言。

改造点只有几处，且都是**把 `t(lang, source)` 换成 `plugin_text_*`**：

| 文件:行 | 现状 | 改为 |
|---|---|---|
| `crates/duck-app/src/pages/toolbox.rs:306` | `t(lang, row.name)` | `plugin_text_of_tool(cx, lang, row.id, row.name)` |
| `crates/duck-app/src/pages/toolbox.rs:405` | `t(lang, tool.name)` | 同上 |
| `crates/duck-app/src/plugins.rs:343` | `tr(cx, name)` | `plugin_text_of_tool(cx, …)` |
| `crates/duck-app/src/pages/plugin_center.rs:753`、`:902` | 原文直出 | 同插件表解析（顺带修掉「不翻译」的不一致） |

**插件归属判据**：`PluginIndex::plugin_for_tool(tool_id)` 已有反查
（`crates/duck-plugin/src/registry.rs:168`），工具名因此可定位到插件。
插件中心的卡片/详情按 `plugin_id` 定位。两者都 miss（= 内置工具）→ 回落宿主 `t(lang, source)`，行为不变。

**返回类型**：`ToolMeta` 的字段是 `&'static str`（`crates/duck-tools/src/meta.rs:27-29`），
所以 `plugin_text_*` 也返 `&'static str` —— 用宿主**已有**的 `leak_str` 内容去重缓存收起
（`crates/duck-app/src/plugins.rs:682`）。上界 = 插件数 × 语言数 × 条目数，
且按内容去重后重复渲染不再增长。

---

## 8. JS 轨：`duck.locale` 与 `duck.t()`

在 `install_host_functions`（`crates/duck-plugin-js/src/lib.rs:220`）新增两个全局：

```js
duck.locale          // string：当前语言 tag，如 "en" / "zh-Hans"
duck.t(source, ...args)
//   用**插件自己的** locales/ 查源串；miss 返回 source 原样。
//   给了 args 就按顺序替换源串里的 {}（语义对齐宿主 trf）。
```

用法（插件作者视角）：

```js
export function handle(toolId, input) {
  const text = duck.readClipboard();
  const report = `chars=${[...text].length}`;
  duck.writeFile("last-report.txt", report);
  duck.notify(duck.t("报告已写入 {}", "last-report.txt"));
  return report;
}
```

三条设计取舍：

1. **`duck.t` 不加能力闸门**：它只在插件自己的包内数据上做纯查表，和 `duck.id` 同级，
   不触达任何宿主资源。
2. **不自动翻译 `duck.notify` 的入参**：隐式翻译会让人分不清「插件自己拼的字符串」与
   「可翻译文案」，也给不出占位符语义。显式 `duck.t` 更可读，且忘写时行为无害
   （显示中文源串）。
3. **需要给 `run_tool` 加第 7 个参数 `lang: &str`**
   （现签名见 `crates/duck-plugin-js/src/lib.rs:109` 起，**7 个参数**）。
   调用点 `crates/duck-app/src/plugins.rs:428` 传 `active_theme(cx).lang().tag()`。
   **这是本方案唯一需要改签名的既有 API。**

---

## 9. 原生轨：wasm / process

翻译文件**不进插件代码**，所以「宿主渲染的文案」天然被 §7 覆盖。插件**自己产出**的
运行时文案按同一契约，只是传输层不同：

**process（独立进程）**：宿主在执行前注入环境变量

```text
DUCK_LOCALE=<tag>              # 当前语言，如 en
DUCK_LOCALES_DIR=<abs path>    # 插件 locales 目录（只读）
```

插件自行读 `<DUCK_LOCALES_DIR>/<DUCK_LOCALE>.toml`，解析顺序**就是**
`PluginLocales::lookup` 那条链（**不需要**再导出一个「源串 tag」变量——key 就是源串，
第 3 档天然成立）：

1. `<DUCK_LOCALES_DIR>/<DUCK_LOCALE>.toml`
2. 主语言子标签（`en-US` → `en.toml`）
3. 该条 key 本身

**wasm**：运行时尚未接入（`crates/duck-app/src/plugins.rs:402-403` 落「后端尚未接入」占位）。
接入时**推荐由宿主侧解析**——提供 host import `duck_t(key, args) -> string` 与
`duck_locale() -> tag`，逻辑直接复用 `duck_plugin::i18n`，插件侧不重复实现降级链。

统一表述：**三类插件共享「`locales/*.toml` + 宿主广告当前 tag」的契约；
差别只在取词 API 的传输层（JS 全局 / 环境变量 / wasm import）。**

### 契约冻结状态（P6 前置，2026-10-15 落地）

原生运行时还不存在，但**协议部分已经固化在代码里**，接入时不必再重新设计：

| 已入库 | 位置 | 作用 |
| --- | --- | --- |
| `ENV_LOCALE` / `ENV_LOCALES_DIR` | `duck-plugin/src/i18n.rs` | 环境变量名常量，取代散落的字面量 |
| `locale_env(plugin_dir, lang)` | 同上 | 组装 `[(name, value); 2]`，可直接喂 `Command::envs`；`lang` 为空退回 `DEFAULT_SOURCE_TAG` |
| `locales_dir(plugin_dir)` | 同上 | 「译文放哪」的唯一定义处（`discover` 也走它） |
| `PluginLocales::text(tag, source)` | 同上 | 把 miss 折进源串——三条轨道共用的最后一跳 |
| `env_names_are_pinned` | `i18n.rs` 单测 | 钉死两个变量名的字面量 |

⚠️ **环境变量名是跨进程对外协议**：插件在别的进程、由别人写，改名**没有编译错误**，
只会静默弄坏所有 process 插件——所以字面量由单测钉住，改名前必须先看到它红。

**接入时要做的**（只剩这步）：在 `run_plugin_tool` 的 `Process` 分支里
`Command::new(..).envs(duck_plugin::locale_env(&dir, lang.tag()))`，再把
`Process` 从 `run_placeholder` 摘出去。宿主侧**零新增逻辑**。

---

## 10. 安全

读插件 `locales/` 是**宿主主动读自己的插件树**（和读 `plugin.toml` 同类），
不是插件能力调用 → **不需要 capability**，也不进授权确认框。

但必须防住四条：

| 风险 | 处置 |
|---|---|
| **路径逃逸** —— `locales = ["../../etc/passwd"]` | 声明路径必须「相对、无 `..`、解析后仍在 `<plugin_dir>/locales/` 下」；规则同 `duck-plugin-js` 的 `resolve_sandbox` |
| **体积** —— 恶意插件塞超大 toml | 单文件上限（建议 256 KiB）+ 单插件 locales 目录总量上限 |
| **条目数** —— 超大表拖慢投影 | 每语言条目上限（建议 20 000 条） |
| **解析失败** —— 坏 TOML 崩宿主 | **不 panic**：`tracing::warn!` 并当「该语言不可用」跳过。坏翻译文件不该让插件加载失败 |

**市场包**：`extract` 已经挡掉 `..` 逃逸与符号链接（`crates/duck-plugin-registry/src/install.rs:264-295`），
`locales/` 子目录会被正常解包。可在 `prepare`（`:215`）加一条**软校验**：
「声明的 locales 文件能被 `PluginLocales::load` 读通」——失败只告警、不阻断安装，
与现有「清单无效仍保留在索引里展示」的宽容度一致。

> 这条软校验**已经有了离线版本**：`scripts/plugin_locales.py` 会逐条校验
> `[i18n] locales` 声明（非法路径 / 文件缺失直接 fail），可直接被 `prepare` 复用或调用。

---

## 11. 校验与门禁

**脚本** `scripts/plugin_locales.py [<dir>…] [--strict] [--all]`（默认扫仓库 `plugins/`）：

- TOML 可解析、key 无重复、值非空、tag 与文件名一致
- `--strict`：以 `source` 为基准，列出各语言缺失 / 多余的 key
- **清单 `[i18n] locales` 声明列表**：逐条按 Rust 侧规则校验（相对、位于 `locales/` 下、无 `..`、
  `.toml`、文件存在），并报出「在盘上但没被声明」的文件。
  ⚠️ 这条是必需的——loader 对非法声明**只记 warn 就跳过**，写错一个路径会让那个语言整份消失。
- ⚠️ 本脚本**只读**（不像 `i18n_en_toml.py` 默认会写文件），所以不需要 `--check`：
  该参数作为**等价别名**收下，但**未知 flag 一律报错**——`--stict` 这种笔误若被静默
  忽略，门禁会悄悄降级成非 strict，而输出看起来一切正常。

**单测**（`duck-plugin`，零 gpui，配 `tests/fixtures-i18n/`）：

- `lookup("en", "中文")` 命中；miss 返回 `None`（回落源串）
- 降级链：`en-US` → `en.toml`；`source` 档；全 miss → 源串
- 路径逃逸被拒（`locales = ["../evil.toml"]`）
- 体积 / 条目上限被拒
- 坏 TOML 只告警不 panic
- `env_names_are_pinned`：钉住两个环境变量名的**字面量**（跨进程协议，编译器看不见）
- `text_folds_a_miss_into_the_source_string`：三轨共用的最后一跳

**`every_chinese_literal_is_translated` 守卫**：只管**宿主源码**。插件文案是**运行期数据**，
不在其覆盖范围——这点要写进 `AGENTS.md`，免得有人以为插件文案也被守卫兜住。

---

## 12. 兼容与迁移

| 场景 | 行为 |
|---|---|
| 老插件（无 `locales/`、无 `[i18n]`） | 与今天**逐字节相同**（§7 的兜底走宿主表） |
| 插件提供 `locales/en.toml` | 英文界面显示插件自己的译文 |
| 插件 `source = "en"` + `locales/zh-Hans.toml` | 中文看译文、英文看源串 |
| 插件译文 miss 某条 | 回落该条源串 |
| 用户切语言 | 渲染期解析 → 全窗口 `refresh()` 自动生效；正在执行的插件按**执行时**的 tag |

⚠️ **一处需要拍板的行为差异**：若某插件的源串**恰好**与宿主某条中文文案同字
（例如插件 `name = "设置"`），今天是命中**宿主**译文；改造后若只查插件表，英文会显示中文源串。

- **建议保留宿主兜底档**（三档：插件表 → 宿主表 → 源串）→ 纯增量、零回归；
- 代价是插件文案的渲染结果依赖宿主表内容（一层弱耦合），作为**兜底**无害。

---

## 13. 验收清单

- [x] `duck-plugin` 单测全绿（含降级链 / 逃逸 / 上限 / 坏 TOML）—— **42 passed / 0 failed**
- [x] fixture 插件带 `locales/en.toml`，`lookup` 命中率 100%—— `fixtures-i18n/` 6 夹具
- [x] 老插件（无 `locales/`）**行为不变**：`load` 无目录 → `is_empty()` → `lookup` 返 `None` → 落到宿主表 → 源串（即迁移前行为）
- [x] 示例插件 `plugins/text-stat` 补 `locales/en.toml`，`duck.t` 在英文界面生效—— `duck_t_follows_the_injected_locale` 覆盖
- [ ] 切语言时工具箱卡片 / 插件中心卡片**即时刷新**（不重启）—— ⏳ **端到端未做**（需打包后手动验；机制上成立：取词全在 `render` 内，切语言＝整窗 `refresh()`）
- [x] clippy `--all-targets` 无告警（`duck-plugin` / `duck-plugin-js` **零告警**；`duck-app` 的既有 27 条与本改动无关）；`rustfmt --check` 干净
- [x] `scripts/plugin_locales.py --strict` 对 `plugins/` 下全部插件通过（默认扫描根就是 `plugins/`）
- [x] `AGENTS.md §3.3` 补「插件文案走插件自己的 `locales/`」一档
- [x] `docs/plugin-signing-design.md` 的打包清单补 `locales/`
- [x] **P6 前置**：原生轨契约入库（`ENV_LOCALE` / `ENV_LOCALES_DIR` / `locale_env` / `text`）
      + `env_names_are_pinned` 守卫 —— 详见 §9「契约冻结状态」

---

## 14. 实施顺序

| 阶段 | 内容 | 依赖 |
|---|---|---|
| **P1**（~0.5 人日） | `duck-plugin::i18n`：加载 + 降级链 + 上限/逃逸防护 + 单测 + fixture | 无 |
| **P2**（~0.25） | 清单 `[i18n]` 段（`source` / `locales`）+ 校验 | P1 |
| **P3**（~0.5） | `duck-app`：`plugin_locales` 全局 + `plugin_text_*` + 渲染点改造（`toolbox.rs` ×2、`plugins.rs` ×1、`plugin_center.rs` **×4**：卡片名/描述 + 详情标题/描述） | P1、P2 |
| **P4**（~0.5） | JS 轨：`run_tool` 加 `lang` + `duck.locale` / `duck.t` + 示例插件补译文 | P1 |
| **P5**（~0.25） | `scripts/plugin_locales.py` + 本文档 + `AGENTS.md` + 签名文档打包清单 | P1–P4 |
| **P6**（随原生运行时） | process 注入 `DUCK_LOCALE` / `DUCK_LOCALES_DIR`；wasm host import —— **契约已冻结（常量 + `locale_env` + `text` + 守卫已入库，见 §9）**，运行时接入时宿主侧只剩「在 `Process` 分支调 `locale_env`」这一步 | P3 |

**P1–P5 合计约 2 人日，已于 2026-10-15 全量落地**；P6 的**协议部分**同日先行固化
（原生运行时本身尚不存在，见 §9）。与既有代码的唯一签名变更在 P4（`run_tool` 加 `lang`）。

---

## 15. 与「源串即键」宿主方案的关系（一页速查）

| 维度 | 宿主（duck-theme） | 插件（本方案） |
|---|---|---|
| 数据源 | `crates/duck-theme/locales/en.toml` | `<plugin_dir>/locales/<tag>.toml` |
| 装载时机 | **编译期**（`i18n!` 宏） | **运行期**（扫描插件目录时） |
| key | 中文源串 | 中文源串（`source` 可改） |
| 查表 | rust-i18n 4.2.4 → `HashMap<&'static,&'static>` 投影 | `toml` + `BTreeMap`（零新增依赖） |
| miss | 回落源串 | 回落源串（可插宿主表兜底档） |
| 语言开关 | `active_theme(cx).lang()` | **同一个**（宿主把 tag 传给插件） |
| 守卫 | `every_chinese_literal_is_translated` | `scripts/plugin_locales.py` |
| 受影响 crate | duck-theme | duck-plugin / duck-plugin-js / duck-app |

**关键不变量：语言只有一个真源** —— `active_theme(cx).lang()`。
插件不持有自己的语言状态，只是被宿主告知当前 tag。这样切语言只需一次 `refresh()`。
