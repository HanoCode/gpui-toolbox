//! 插件自带翻译：`<plugin_dir>/locales/<tag>.toml`。
//!
//! 设计口径（完整方案见 `docs/plugin-i18n-design.md`）：
//!
//! **翻译是数据，不是代码。** 插件的译文放在**插件目录里的 TOML 文件**，由宿主解析、
//! 投影、按当前语言取词——插件代码（JS / wasm / process）写不写、怎么写都不影响
//! 「宿主渲染的那部分文案」（工具卡名、插件中心卡片）。插件自己产出的运行时文案
//! （`duck.notify`）通过取词 API 读**同一张表**，只是传输层不同。
//!
//! 于是「JS 插件 vs 原生插件」的差异被压到取词 API 的传输层，翻译文件本身零差异。
//!
//! # 格式：与宿主完全同构
//!
//! 根级平铺、quoted key、顺序无关，**键就是中文源串**（`source` 可改，见 [`DEFAULT_SOURCE_TAG`]）：
//!
//! ```toml
//! "文本统计" = "Text stats"
//! "报告已写入 {}" = "Report written to {}"
//! ```
//!
//! 复用宿主写法的三条理由：插件作者不用另学一套 key 命名；「源串即键」让
//! 「没翻 → 回落该条源串」天然成立；清单里的 `name` / `description` 本身就是 key。
//!
//! # 为什么不用 rust-i18n
//!
//! 宿主用 `rust-i18n` 是因为它的 `i18n!` 宏**编译期**读目录，适合编译进二进制的文案。
//! 插件是**运行期**才出现的，装不进宏；而且 rust-i18n 没有语言降级链。本模块只用
//! `toml`（本 crate 既有依赖）把文件读成 `BTreeMap`，**零新增依赖**。
//!
//! # 三类轨道的传输层
//!
//! 同一份 `locales/*.toml`、同一套降级链，差别只在插件**怎么问到当前语言**：
//!
//! | 轨道 | 传输层 | 入口 |
//! | --- | --- | --- |
//! | JS（进程内 QuickJS） | 沙箱全局 | `duck.locale` / `duck.t(source, ...args)` |
//! | process（独立进程） | 环境变量 | [`ENV_LOCALE`] / [`ENV_LOCALES_DIR`]，经 [`locale_env`] 组装 |
//! | wasm（沙箱组件） | host import | 宿主侧解析（复用本模块），插件不重复实现降级链 |
//!
//! 三条轨道的语言**都由宿主在派发时给出**（`active_theme(cx).lang().tag()`）；
//! 插件不持有自己的语言状态，所以切语言只需一次整窗刷新。
//!
//! # 分层
//!
//! 语言入参是 **`&str` tag**（如 `"en"` / `"zh-Hans"`），不是 `duck-theme::Lang` 枚举——
//! `duck-theme` 依赖 `gpui`，而本 crate 的分层纪律是「可独立单测的纯函数」，
//! 不能为了一个枚举跨过去。`Lang → tag` 的转换留在 `duck-app`。

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// 源串默认语言 tag。
///
/// 与宿主「中文源串即键」的约定一致（`duck-theme` 的 `Locale::wire_name()` 也用
/// `zh-Hans` 这个写法）。决定「所有语言都 miss 时回落到哪一份」。
pub const DEFAULT_SOURCE_TAG: &str = "zh-Hans";

/// 翻译文件所在子目录名（相对插件目录）。
pub const LOCALES_DIR: &str = "locales";

/// 单个翻译文件的大小上限（256 KiB）。
///
/// 超限**跳过该语言**而不是截断：截断的 TOML 会解析失败，行为与「跳过」相同，
/// 但错误信息会误导成「语法错」。
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// 单个语言的条目数上限。
///
/// 防「超大表拖慢投影」：投影是进程内一次性构建，但表在渲染路径上被反复查。
const MAX_ENTRIES: usize = 20_000;

/// 交给**独立进程插件**的当前语言 tag（由 [`locale_env`] 导出）。
///
/// ⚠️ 这是**跨进程的对外协议**：插件在别的进程、由别人写，编译器管不到。改这个名字
/// 等于一次性弄坏所有 process 插件，且**没有任何编译错误**——所以单测
/// `env_names_are_pinned` 把字面量钉死，改名前必须先看到它红。
pub const ENV_LOCALE: &str = "DUCK_LOCALE";

/// 交给**独立进程插件**的 `locales/` 绝对路径（只读，由 [`locale_env`] 导出）。
///
/// 同上：跨进程协议，字面量由 `env_names_are_pinned` 钉住。
pub const ENV_LOCALES_DIR: &str = "DUCK_LOCALES_DIR";

/// 一个插件的全部翻译（已按语言分表）。
///
/// 由 [`Self::load`] 构建：**永不失败**——读不动 / 坏 TOML / 超限都只记一条
/// `tracing::warn!` 并跳过该语言。一个坏翻译文件不该让插件加载失败，更不该崩宿主。
#[derive(Debug, Default, Clone)]
pub struct PluginLocales {
    /// 源串语言 tag（降级链的最后一档）。
    source: String,
    /// 已装载的语言：tag → （源串 → 译文）。
    tables: BTreeMap<String, BTreeMap<String, String>>,
}

impl PluginLocales {
    /// 装载插件目录下的翻译。
    ///
    /// # Arguments
    /// - `dir`：插件目录（`plugin.toml` 所在目录）；
    /// - `source_tag`：插件源串的语言 tag（清单 `[i18n] source`，缺省走
    ///   [`DEFAULT_SOURCE_TAG`]）；
    /// - `declared`：清单 `[i18n] locales` 显式声明的文件列表。
    ///   `None` = 自动发现 `locales/*.toml`；`Some` = **只**装载声明的那些
    ///   （这样 `locales/` 里混进的无关 toml 不会被当成语言）。
    ///
    /// 声明的路径必须「相对、首段为 `locales`、无 `..`、以 `.toml` 结尾」，
    /// 否则记告警并跳过——插件目录里的文件是外部输入，不能让 `../../etc/passwd`
    /// 这种写法把宿主的任意文件当翻译读。
    #[must_use]
    pub fn load(dir: &Path, source_tag: &str, declared: Option<&[String]>) -> Self {
        let mut tables = BTreeMap::new();
        for (tag, path) in Self::locale_files(dir, declared) {
            match read_table(&path, &tag) {
                Ok(table) if table.is_empty() => {
                    tracing::warn!("plugin locale {} has no usable entry", path.display());
                }
                Ok(table) => {
                    tables.insert(tag, table);
                }
                Err(reason) => tracing::warn!("plugin locale skipped: {reason}"),
            }
        }
        Self {
            source: source_tag.to_owned(),
            tables,
        }
    }

    /// 查译文：按 [`Self::load`] 文档里的降级链取第一个命中。
    ///
    /// 降级链（由细到粗）：精确 tag → 主语言子标签（`en-US` → `en`）→ 源串 tag。
    /// 全部 miss 返回 `None`，**由调用方回落到源串本身**（与宿主 `t()` 的分工一致）。
    ///
    /// 这里**不含「宿主表」那一档**：宿主表在 `duck-app`，本 crate 够不着。
    /// 三档的最后一档由调用方补，见 `duck-app::plugins::plugin_text_of_*`。
    #[must_use]
    pub fn lookup(&self, tag: &str, source: &str) -> Option<&str> {
        if let Some(text) = self.lookup_exact(tag, source) {
            return Some(text);
        }
        // `en-US` → `en`：区域变体没提供时退到主语言。当前只有中英两档用不到，
        // 但这是零成本的，且能避免将来加 `zh-Hant` 时出现「翻了却查不到」。
        if let Some((primary, _)) = tag.split_once('-')
            && primary != tag
            && let Some(text) = self.lookup_exact(primary, source)
        {
            return Some(text);
        }
        self.lookup_exact(&self.source, source)
    }

    /// 精确 tag 查表（不含降级）。
    fn lookup_exact(&self, tag: &str, source: &str) -> Option<&str> {
        self.tables.get(tag)?.get(source).map(String::as_str)
    }

    /// 查译文，miss 直接回落**源串本身**——「拿到 key 就要一个能上屏的串」的入口。
    ///
    /// 这是三类轨道共用的最后一跳：JS 的 `duck.t`、wasm 的 host import、以及将来
    /// process 轨的宿主侧辅助，都该走这里，而不是各写一遍
    /// `lookup(..).unwrap_or(source)`。收敛到一处实现，才不会某个轨道漏词时
    /// 悄悄渲染成空串（空标签比显示中文更难排查）。
    #[must_use]
    pub fn text<'a>(&'a self, tag: &str, source: &'a str) -> &'a str {
        self.lookup(tag, source).unwrap_or(source)
    }

    /// 已装载的语言 tag（升序）。供索引 / 市场展示「本插件提供哪些语言」。
    #[must_use]
    pub fn available(&self) -> Vec<&str> {
        self.tables.keys().map(String::as_str).collect()
    }

    /// 一条译文都没有（老插件，或全部装载失败）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// 解析出「要装载哪些文件」：`(tag, 绝对路径)`，按 tag 升序（装载顺序确定）。
    ///
    /// 显式声明的非法路径在这里被过滤掉（记告警），不进入后续读取。
    fn locale_files(dir: &Path, declared: Option<&[String]>) -> Vec<(String, PathBuf)> {
        let mut files: Vec<(String, PathBuf)> = match declared {
            Some(list) => list
                .iter()
                .filter_map(|raw| match resolve_declared(dir, raw) {
                    Ok(pair) => Some(pair),
                    Err(reason) => {
                        tracing::warn!("plugin locale declaration rejected ({raw}): {reason}");
                        None
                    }
                })
                .collect(),
            None => discover(dir),
        };
        files.sort();
        files.dedup_by(|a, b| a.0 == b.0);
        files
    }
}

/// 插件的翻译目录：`<plugin_dir>/locales`。
///
/// 「译文放在哪」只有这一处定义——[`discover`] 的扫描、[`locale_env`] 的导出、
/// 文档里写给插件作者的路径规则，指的都是它。
#[must_use]
pub fn locales_dir(plugin_dir: &Path) -> PathBuf {
    plugin_dir.join(LOCALES_DIR)
}

/// 组装**独立进程插件**要注入的环境变量对，形如 `[(name, value); 2]`。
///
/// process 轨的取词传输层：宿主把「当前语言」与「翻译目录」当环境变量交给子进程，
/// 插件自己按下面的顺序解析（**与 [`PluginLocales::lookup`] 是同一条链**）：
///
/// 1. `<DUCK_LOCALES_DIR>/<DUCK_LOCALE>.toml`；
/// 2. 主语言子标签（`en-US` → `en.toml`）；
/// 3. 该条 key 本身——key 就是源串，所以「没翻」天然回落，**不需要**再导出一个
///    「源串 tag」变量。加那个变量等于凭空多一条必须维护的协议。
///
/// 返回成对的 `(&'static str, String)` 而不是 `HashMap`：调用方直接喂
/// `Command::envs`。`lang` 传空串时退回 [`DEFAULT_SOURCE_TAG`]，免得子进程
/// 拿着 `""` 去拼 `.toml` 文件名。
///
/// 插件的 `locales/` **不存在也照常返回路径**（老插件的情形）：插件那边读不到
/// 文件就回落源串，与宿主侧行为一致；为此屏蔽掉整个变量反而让插件分不清
/// 「宿主没给」和「我没翻」。
#[must_use]
pub fn locale_env(plugin_dir: &Path, lang: &str) -> [(&'static str, String); 2] {
    let tag = if lang.is_empty() {
        DEFAULT_SOURCE_TAG
    } else {
        lang
    };
    [
        (ENV_LOCALE, tag.to_owned()),
        (
            ENV_LOCALES_DIR,
            locales_dir(plugin_dir).to_string_lossy().into_owned(),
        ),
    ]
}

/// 校验一条清单声明的相对路径，返回 `(tag, 绝对路径)`。
///
/// 三道闸门：**相对**（拒绝绝对路径与 Windows 盘符）、**首段必须是 [`LOCALES_DIR`]**、
/// **不含 `..` / `.` 之类的非普通分量**（否则 `locales/../../x.toml` 能逃出插件目录）。
/// 末了要求扩展名是 `.toml`——声明一个 `.js` 当翻译表是写错了，早报早改。
fn resolve_declared(dir: &Path, raw: &str) -> Result<(String, PathBuf), String> {
    let relative = Path::new(raw);
    if relative.is_absolute() {
        return Err("absolute path is not allowed".into());
    }
    let mut components = relative.components();
    match components.next() {
        Some(Component::Normal(first)) if first == OsStr::new(LOCALES_DIR) => {}
        _ => return Err(format!("must live under {LOCALES_DIR}/")),
    }
    if components.any(|component| !matches!(component, Component::Normal(_))) {
        return Err("path must not contain `..` or other non-plain segments".into());
    }
    if relative.extension().and_then(OsStr::to_str) != Some("toml") {
        return Err("translation file must be a .toml".into());
    }
    let tag = tag_of(relative).ok_or_else(|| "missing locale tag in filename".to_string())?;
    Ok((tag, dir.join(relative)))
}

/// 自动发现：`<dir>/locales/*.toml`（**不递归**）。
///
/// 目录不存在（老插件只留 `plugin.toml` 与入口）是正常分支，返回空。
fn discover(dir: &Path) -> Vec<(String, PathBuf)> {
    let path = locales_dir(dir);
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            tracing::warn!("cannot read {}: {err}", path.display());
            return Vec::new();
        }
    };

    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(OsStr::to_str) != Some("toml") {
            continue;
        }
        match tag_of(&path) {
            Some(tag) => files.push((tag, path)),
            None => tracing::warn!(
                "plugin locale {} has no tag in its filename",
                path.display()
            ),
        }
    }
    files
}

/// 从文件名取语言 tag（`en.toml` → `en`）。
///
/// 非 UTF-8 文件名返回 `None`：语言 tag 只能是 ASCII（BCP 47 的写法），
/// 拼不出可比较的字符串就当它不存在。
fn tag_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_owned())
}

/// 读一个翻译文件成 `源串 → 译文`。任何问题都返回人话错误串（由调用方记日志）。
///
/// **空译文条目会被丢掉**（当作「这条没翻」→ 回落源串）：渲染一个空标签比显示中文
/// 更糟，而作者手滑留个 `""` 是很常见的。丢掉时记一条告警，作者的疏漏仍可见。
fn read_table(path: &Path, tag: &str) -> Result<BTreeMap<String, String>, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is {} bytes (limit {MAX_FILE_BYTES})",
            path.display(),
            metadata.len()
        ));
    }

    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let table: BTreeMap<String, String> = toml::from_str(&source).map_err(|error| {
        format!(
            "{} ({tag}) is not a flat string table: {error}",
            path.display()
        )
    })?;

    if table.len() > MAX_ENTRIES {
        return Err(format!(
            "{} ({tag}) has {} entries (limit {MAX_ENTRIES})",
            path.display(),
            table.len()
        ));
    }

    let mut usable = BTreeMap::new();
    for (key, value) in table {
        if value.is_empty() {
            tracing::warn!(
                "plugin locale {} ({tag}) has an empty translation for {key:?}",
                path.display()
            );
            continue;
        }
        usable.insert(key, value);
    }
    Ok(usable)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 专门的 fixture 目录：**不能**放进 `tests/fixtures/`——那里是
    /// `registry.rs` 的扫描目标，多一个含 `plugin.toml` 的子目录会撞坏它的计数断言。
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures-i18n")
            .join(name)
    }

    #[test]
    fn auto_discovers_and_translates() {
        let locales = PluginLocales::load(&fixture("full"), DEFAULT_SOURCE_TAG, None);
        assert!(!locales.is_empty());
        assert_eq!(locales.available(), ["en", "zh-Hans"]);
        assert_eq!(locales.lookup("en", "文本统计"), Some("Text stats"));
        assert_eq!(locales.lookup("zh-Hans", "文本统计"), Some("文本统计"));
    }

    /// 没翻的条目必须 `None`（而不是误命中别的条目）——调用方据此回落源串。
    #[test]
    fn missing_entry_returns_none() {
        let locales = PluginLocales::load(&fixture("full"), DEFAULT_SOURCE_TAG, None);
        assert_eq!(locales.lookup("en", "这条肯定不在表里 zzz"), None);
    }

    /// 只提供 `en` 的老式插件：中文请求回落到源串 tag（也没有）→ `None` → 显示源串。
    #[test]
    fn absent_language_falls_back_to_none() {
        let locales = PluginLocales::load(&fixture("en-only"), DEFAULT_SOURCE_TAG, None);
        assert_eq!(locales.available(), ["en"]);
        assert_eq!(locales.lookup("en", "文本统计"), Some("Text stats"));
        assert_eq!(locales.lookup("zh-Hans", "文本统计"), None);
    }

    /// 区域变体退到主语言：`en-US` → `en.toml`。
    #[test]
    fn region_variant_falls_back_to_primary_subtag() {
        let locales = PluginLocales::load(&fixture("en-only"), DEFAULT_SOURCE_TAG, None);
        assert_eq!(locales.lookup("en-US", "文本统计"), Some("Text stats"));
    }

    /// `source` 档：英文优先的插件（源串是英文），中文译文放 `zh-Hans.toml`。
    #[test]
    fn source_tag_is_the_last_resort() {
        let locales = PluginLocales::load(&fixture("en-source"), "en", None);
        assert_eq!(locales.lookup("zh-Hans", "Text stats"), Some("文本统计"));
        // 英文请求：`en.toml` 不存在 → 落到 source 档（也没有）→ 调用方显示源串。
        assert_eq!(locales.lookup("en", "Text stats"), None);
    }

    /// 坏 TOML 只跳过该语言，不 panic、不影响其他语言。
    #[test]
    fn broken_file_is_skipped_without_affecting_others() {
        let locales = PluginLocales::load(&fixture("broken"), DEFAULT_SOURCE_TAG, None);
        assert_eq!(locales.available(), ["zh-Hans"]);
        assert_eq!(locales.lookup("zh-Hans", "文本统计"), Some("文本统计"));
    }

    /// 目录不存在（老插件）→ 空表，不是错误。
    #[test]
    fn missing_locales_dir_yields_empty() {
        let locales = PluginLocales::load(&fixture("no-locales"), DEFAULT_SOURCE_TAG, None);
        assert!(locales.is_empty());
        assert_eq!(locales.lookup("en", "文本统计"), None);
    }

    /// 声明列表存在时**只**装载声明的那些：`locales/` 里混进的无关 toml 被忽略。
    #[test]
    fn declared_list_limits_what_is_loaded() {
        let declared = vec!["locales/en.toml".to_string()];
        let locales = PluginLocales::load(&fixture("full"), DEFAULT_SOURCE_TAG, Some(&declared));
        assert_eq!(locales.available(), ["en"]);
    }

    /// 逃逸路径（绝对 / `..` / 非 `locales` 前缀 / 非 toml）一律拒收。
    ///
    /// `locales/./en.toml` **不**在拒绝之列：`Path::components()` 会把 `.` 归一化掉，
    /// 它等价于 `locales/en.toml`、逃不出插件目录，没有拒绝的理由。
    #[test]
    fn declares_reject_escaping_paths() {
        let dir = fixture("full");
        for raw in [
            "/etc/passwd",
            "../../etc/passwd",
            "locales/../../secret.toml",
            "en.toml",
            "locales/en.txt",
        ] {
            assert!(
                resolve_declared(&dir, raw).is_err(),
                "声明 {raw:?} 必须被拒绝"
            );
        }
        for raw in ["locales/en.toml", "locales/./en.toml"] {
            assert!(
                resolve_declared(&dir, raw).is_ok(),
                "声明 {raw:?} 应当被接受"
            );
        }
    }

    /// 空译文条目被丢掉（回落源串），不让界面出现空标签。
    #[test]
    fn empty_translation_is_dropped() {
        let locales = PluginLocales::load(&fixture("empty-values"), DEFAULT_SOURCE_TAG, None);
        assert_eq!(locales.lookup("en", "文本统计"), Some("Text stats"));
        assert_eq!(locales.lookup("en", "报告已写入 {}"), None);
    }

    /// 环境变量名是**跨进程的对外协议**：插件在别的进程、由别人写，编译器管不到。
    /// 改名必须先把这条测试改红，否则会静默弄坏所有 process 插件。
    #[test]
    fn env_names_are_pinned() {
        assert_eq!(ENV_LOCALE, "DUCK_LOCALE");
        assert_eq!(ENV_LOCALES_DIR, "DUCK_LOCALES_DIR");
    }

    /// `locale_env` 指向插件的 `locales/`；空 tag 退回源串 tag；缺目录也照常给路径。
    #[test]
    fn locale_env_points_at_the_locales_dir() {
        let dir = fixture("full");
        let env: BTreeMap<_, _> = locale_env(&dir, "en").into_iter().collect();
        assert_eq!(env[ENV_LOCALE].as_str(), "en");
        let exported = PathBuf::from(&env[ENV_LOCALES_DIR]);
        assert_eq!(exported, locales_dir(&dir));
        assert!(exported.is_dir(), "{} 应当是目录", exported.display());

        // 空 tag：绝不能给子进程一个空串，让它去拼 `.toml` 文件名。
        let fallback: BTreeMap<_, _> = locale_env(&dir, "").into_iter().collect();
        assert_eq!(fallback[ENV_LOCALE].as_str(), DEFAULT_SOURCE_TAG);

        // 老插件没有 `locales/`：路径照给（插件读不到就回落源串），不屏蔽整个变量。
        let legacy: BTreeMap<_, _> = locale_env(&fixture("no-locales"), "en")
            .into_iter()
            .collect();
        assert!(!PathBuf::from(&legacy[ENV_LOCALES_DIR]).is_dir());
    }

    /// `text`：命中给译文，miss 给**源串本身**（不是空串、也不是别的语言的串）。
    #[test]
    fn text_folds_a_miss_into_the_source_string() {
        let locales = PluginLocales::load(&fixture("full"), DEFAULT_SOURCE_TAG, None);
        assert_eq!(locales.text("en", "文本统计"), "Text stats");
        assert_eq!(locales.text("en", "这条没翻 zzz"), "这条没翻 zzz");

        // 一条译文都没有的老插件：原样返回。
        let legacy = PluginLocales::load(&fixture("no-locales"), DEFAULT_SOURCE_TAG, None);
        assert_eq!(legacy.text("en", "文本统计"), "文本统计");
    }
}
