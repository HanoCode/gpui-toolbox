//! 极简 i18n 层：语言枚举 + 以「中文源串」为键的英译表。
//!
//! 设计取舍：**源串即键**（`t(lang, "外观")`），缺表项自动回落源串，所以新增文案
//! 也不会翻成空串。英译数据在 `locales/en.toml`（可直接编辑、**顺序无关**），由
//! [`crate::i18n_backend`] 编译期装载、首次取词时投影成静态表；`cargo test` 的守卫
//! 保证「源码里的中文文案必须全部登记」。

use std::sync::OnceLock;

use gpui::App;
use serde::{Deserialize, Serialize};

use crate::manager::active_theme;

/// 用户在「外观 → 语言」里的选择。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Locale {
    /// 跟随系统语言。
    #[default]
    System,
    /// 简体中文。
    ZhHans,
    /// English。
    En,
}

impl Locale {
    /// 下拉里的全部档位（顺序即展示顺序）。
    pub const ALL: [Self; 3] = [Self::System, Self::ZhHans, Self::En];

    /// 偏好 wire 值（落盘 / 比较用）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::ZhHans => "zh-Hans",
            Self::En => "en",
        }
    }

    /// 解析 wire 值；未知值回落到「跟随系统」。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "zh-Hans" => Self::ZhHans,
            "en" => Self::En,
            _ => Self::System,
        }
    }

    /// 下拉项名称：语言名按本族写法展示，不随界面语言变化。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::ZhHans => "简体中文",
            Self::En => "English",
        }
    }

    /// 解析成具体语言；`System` 读系统语言。
    #[must_use]
    pub fn resolve(self) -> Lang {
        match self {
            Self::System => system_lang(),
            Self::ZhHans => Lang::Zh,
            Self::En => Lang::En,
        }
    }
}

/// 具体生效语言（没有「跟随系统」这一档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// 简体中文（源串本身）。
    Zh,
    /// English。
    En,
}

impl Lang {
    /// 中文源串 → 当前语言文案；[`Lang::Zh`] 直接返回源串。
    #[must_use]
    pub fn text(self, source: &'static str) -> &'static str {
        t(self, source)
    }

    /// 语言 tag（BCP 47 风格）：`zh-Hans` / `en`。
    ///
    /// 这是**跨进程 / 跨插件的语言标识**——插件翻译文件名（`locales/en.toml`）、
    /// JS 的 `duck.locale`、process 插件的 `DUCK_LOCALE` 环境变量都用它。
    ///
    /// 取值与 [`Locale::wire_name`] 里两个具体语言**逐一相同**（`System` 档
    /// [`resolve`](Locale::resolve) 之后也只会落在这两者之一），由
    /// `lang_tag_matches_wire_name` 钉住——两处各写一份字面量迟早会漂。
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Zh => "zh-Hans",
            Self::En => "en",
        }
    }
}

/// 系统语言探测结果：进程内只算一次（GUI 进程从 Finder 启动时环境变量常为空，
/// 需要读一次 macOS 全局偏好，代价不值得每帧重复）。
#[must_use]
pub fn system_lang() -> Lang {
    static CACHE: OnceLock<Lang> = OnceLock::new();
    *CACHE.get_or_init(detect_system_lang)
}

fn detect_system_lang() -> Lang {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(key)
            && let Some(lang) = lang_from_tag(&value)
        {
            return lang;
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(lang) = macos_language() {
        return lang;
    }
    // 探测不到时按项目主语言（中文）兜底。
    Lang::Zh
}

/// `zh_CN` / `zh-Hans` / `en_US` 形态的语言标签 → 语言；`C` / `POSIX` / 空值返回 `None`。
fn lang_from_tag(tag: &str) -> Option<Lang> {
    let tag = tag.trim().to_ascii_lowercase();
    if tag.is_empty() || tag == "c" || tag == "posix" {
        return None;
    }
    Some(if tag.starts_with("zh") {
        Lang::Zh
    } else {
        Lang::En
    })
}

/// macOS：读 `AppleLocale`（如 `zh_CN`），与「系统设置 → 语言与地区」一致。
#[cfg(target_os = "macos")]
fn macos_language() -> Option<Lang> {
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    lang_from_tag(&String::from_utf8_lossy(&output.stdout))
}

/// 取文案：`lang` 为中文时返回源串本身，英文时查表，缺表项回落源串。
#[must_use]
pub fn t(lang: Lang, source: &'static str) -> &'static str {
    match lang {
        Lang::Zh => source,
        Lang::En => translate_en(source).unwrap_or(source),
    }
}

/// 取文案（从全局主题读当前语言）。
#[must_use]
pub fn tr(cx: &App, source: &'static str) -> &'static str {
    t(active_theme(cx).lang(), source)
}

/// 带占位符的文案：源串里用 `{}` 站位，按 `args` 顺序逐个替换。
#[must_use]
pub fn tf(lang: Lang, source: &'static str, args: &[&str]) -> String {
    fill(t(lang, source), args)
}

/// 带占位符的文案（从全局主题读当前语言）。
#[must_use]
pub fn trf(cx: &App, source: &'static str, args: &[&str]) -> String {
    tf(active_theme(cx).lang(), source, args)
}

/// 带**数量**的文案：源串里 `{}` 站数量（只支持一个占位符）。中文直接替换；英文按
/// `count == 1` 在 [`EN_PLURAL`] 里取单数 / 复数。
///
/// 复数表里没有该源串时回落 [`tf`]（英文用 [`EN`] 的通用形式，可能是错的复数——
/// 比如 `1 tools`），所以**带数量的文案要尽量登记** [`EN_PLURAL`]。
///
/// 数量按十进制原样输出、**不加千分位**：相对时间这类场景「1,024 天前」读着别扭，
/// 千分位留给「统计数字」那种场景单独做。
#[must_use]
pub fn tn(lang: Lang, source: &'static str, count: i64) -> String {
    let template = match lang {
        Lang::Zh => source,
        Lang::En => match plural_en(source) {
            Some((one, other)) => {
                if count == 1 {
                    one
                } else {
                    other
                }
            }
            None => translate_en(source).unwrap_or(source),
        },
    };
    fill(template, &[&count.to_string()])
}

/// 带数量的文案（从全局主题读当前语言）。
#[must_use]
pub fn trn(cx: &App, source: &'static str, count: i64) -> String {
    tn(active_theme(cx).lang(), source, count)
}

/// 「不满一分钟」那一档的显示口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnderMinute {
    /// 一律说成「刚刚」（通知中心 / 插件中心的口径）。
    JustNow,
    /// 说到秒（`45 秒前`）——Redis 慢日志要区分「刚发生」与「几秒前」。
    Seconds,
}

/// 相对时间：`secs` 秒**之前** → 文案。
///
/// 分档：`< 60s` 按 `under_minute` 走「刚刚」或「N 秒前」、`< 60min`「N 分钟前」、
/// `< 24h`「N 小时前」、更早「N 天前」。英文走 [`tn`]，所以**单数正确**
/// （`1 minute ago`——本仓原有三处各写一遍的实现都会写出 `1 minutes ago`）。
///
/// `under_minute` 是参数而不是常量：三处原有调用里两处用「刚刚」、Redis 慢日志用秒，
/// 统一成一种会改掉另一处的显示口径，所以留成显式选择。
///
/// 负数（时间戳比现在还晚，一般是系统时钟漂移）按 `0` 处理，不显示「-3 分钟前」。
#[must_use]
pub fn relative_time(lang: Lang, secs: i64, under_minute: UnderMinute) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        match under_minute {
            UnderMinute::JustNow => t(lang, "刚刚").to_owned(),
            UnderMinute::Seconds => tn(lang, "{} 秒前", secs),
        }
    } else if secs < 3_600 {
        tn(lang, "{} 分钟前", secs / 60)
    } else if secs < 86_400 {
        tn(lang, "{} 小时前", secs / 3_600)
    } else {
        tn(lang, "{} 天前", secs / 86_400)
    }
}

/// 相对时间（从全局主题读当前语言）。
#[must_use]
pub fn tr_relative_time(cx: &App, secs: i64, under_minute: UnderMinute) -> String {
    relative_time(active_theme(cx).lang(), secs, under_minute)
}

/// 统计数字：按千分位分组（`1234567` → `1,234,567`）。
///
/// **只用于展示**：输入框回填、写回配置、机器可读字段一律走 `to_string()`——
/// 分组符混进可编辑文本会让用户改不了、解析失败（`max_tokens` 的输入框就是一例）。
///
/// 与 [`tn`] 的分工正是 [`tn`] 文档里留下的那条：`tn` 的「数量」按十进制原样输出
/// （「1024 天前」比「1,024 天前」好读），分组留给**统计数字**——token 计数、文件数、
/// 结果行数这类。所以本函数不参与 `tn`/`relative_time` 的取词路径，是独立入口。
///
/// **不收 `lang`**：简繁中文与英文的分组符都是 `,`（小数点是 `.`），本函数没有语言分支
/// 可写；收一个被忽略的参数只会误导调用方。将来真要支持「分组符不同的语言」
/// （法语用窄不换行空格、印地语按 2 位分组）再引入参数。
///
/// `value` 取 `i64`：与 [`tn`] / [`relative_time`] 的计数口径一致（调用方从 `u32` /
/// `u64` / `usize` 转换时按展示量级用 `as i64`）。`i64::MIN` 走 `unsigned_abs`，
/// 不会因取绝对值而溢出。
#[must_use]
pub fn number(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if value < 0 {
        out.push('-');
    }
    // 从右往左每 3 位一组：`head` 是**首组**的位数（`0` 表示首组恰好 3 位）。
    // 之后在每个「下标余数 == head」的位置前面补分隔符，`index > 0` 保证首位之前不补
    // （`1000` 不会写成 `,1000`）。写成余数比较而非 `% 3 == 0`：后者会让 clippy 建议
    // `is_multiple_of`，而那个 API 要 Rust 1.87，本仓 MSRV 是 1.85。
    let head = digits.len() % 3;
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && index % 3 == head {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// 人类可读的字节数：`512 B` / `4.0 KB` / `1.2 MB` / `3.0 GB` / `2.5 TB`。
///
/// **只有 B 不带小数**：`1.0 KB` 读起来比 `1024 B` 快，但 `0.0 B` 是纯噪音。
/// 档位到 TB 封顶——本仓最大的量是磁盘容量与归档体积，PB 级不在展示范围内，
/// 超出的量级以「大数字 + TB」呈现即可，不必再补一档。
///
/// 本函数是仓内**唯一**的字节格式化实现。`duck-app` 原先有 7 份同源拷贝
/// （`status_support` / `data` / `ai_tools` / `mail_data` / `plugin_center` /
/// `update` / `redis`），口径**四不像**（KB 有的取整有的留 1 位，redis 那份留 2 位，
/// `data` 与 `update` 那两份根本没有 GB 档），已全部收拢到这里——如今只剩
/// `status_support::format_bytes` 一处**同名委派**（6 个导入者不动），其余 6 份已删。
/// 内核 crate 里的 `duck-db::drivers::human_bytes` **保留**：内核不许依赖 gpui，
/// 用不上本层（其输出与本函数逐字一致，无正确性缺陷可修）。
///
/// **不收 `lang`**：与 [`number`] 同理——简繁中文与英文的单位符号都是
/// `B` / `KB` / `MB` / `GB` / `TB`，没有语言分支可写（法语 `Mo` / `Ko`、
/// 俄语 `МБ` 那类才需要参数，届时再引入）。
///
/// `value` 取 `u64`：字节数天然非负（体积 / 容量 / 剩余空间），负值无意义；
/// 调用方若持有有符号量（`duck-mail` 的附件大小是 `i64`）须先收敛到非负。
#[must_use]
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if value < 1024 {
        return format!("{value} B");
    }
    let mut scaled = value as f64;
    let mut unit = 0usize;
    while scaled >= 1024. && unit + 1 < UNITS.len() {
        scaled /= 1024.;
        unit += 1;
    }
    format!("{scaled:.1} {}", UNITS[unit])
}

/// 日期 + 时间：`YYYY-MM-DD HH:MM`（**UTC**）。
///
/// 时间戳 ≤ 0 给**空串**：内核解析不出 `Date` 头时会写 0，列表上应当留空，
/// 而不是印出一个 1970 年（那会被当成真日期）。
///
/// **为什么是 UTC**：本仓的依赖准入结论是「不引 `chrono` / `time`」，没有时区库
/// 就做不了本地时区换算。这是**已知的展示差异**（未决项记在
/// `docs/email-design.md`），不是遗漏。
///
/// **不收 `lang`**：本仓三种语言的日期顺序都是 ISO（年-月-日）。美式
/// `MM/DD/YYYY` 配 12 小时制（`2:23 PM`）是一处**可见的界面变更**，
/// 得先有产品决定——不在格式化层的单方面权限内，故此处不留一个被忽略的参数。
#[must_use]
pub fn datetime_full(secs: i64) -> String {
    let Some((year, month, day, hour, minute)) = civil_parts(secs) else {
        return String::new();
    };
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// 日期 + 时间（短）：`MM-DD HH:MM`（**UTC**）——列表行宽度紧，省掉年份。
///
/// 其余口径（UTC / `≤ 0` 给空串 / 不收 `lang`）与 [`datetime_full`] 完全一致。
#[must_use]
pub fn datetime_short(secs: i64) -> String {
    let Some((_, month, day, hour, minute)) = civil_parts(secs) else {
        return String::new();
    };
    format!("{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// 把 Unix 秒拆成 `(年, 月, 日, 时, 分)`；`secs <= 0` 返回 `None`（见 [`datetime_full`]）。
fn civil_parts(secs: i64) -> Option<(i64, i64, i64, i64, i64)> {
    if secs <= 0 {
        return None;
    }
    let days = secs.div_euclid(86_400);
    let within_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Some((
        year,
        month,
        day,
        within_day / 3_600,
        (within_day % 3_600) / 60,
    ))
}

/// 由「自 1970-01-01 起的天数」还原公历年月日（Howard Hinnant 的 `civil_from_days`）。
///
/// 手写而不用库：见 [`datetime_full`] 的依赖结论。**本仓仍有若干份同源实现**
/// （`duck-platform::logging`、`duck-db::drivers::mssql`、`duck-redis::decode`、
/// `duck-mail::{compose, db::repo::search}`）——它们都是**内核 crate，不许依赖 gpui**，
/// 因而用不上本层。`duck-app` 的两份（`mail_data` / `terminal_data`）已并入这里。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as i64;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as i64;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 把**已译好**的模板里的 `{}` 按 `args` 顺序替换（[`tf`] / [`tn`] 共用的收尾步骤）。
///
/// 参数多于占位符时多余的忽略，少于时保留未替换的 `{}`（与旧 [`tf`] 行为一致）。
fn fill(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    for arg in args {
        match rest.find("{}") {
            Some(index) => {
                out.push_str(&rest[..index]);
                out.push_str(arg);
                rest = &rest[index + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// 英译查表：未登记的源串返回 `None`（调用方回落源串）。
///
/// 数据源是 `locales/en.toml`——经 [`crate::i18n_backend`] 投影成
/// `HashMap<&'static str, &'static str>`，所以这里仍能给出 `&'static str`。
fn translate_en(source: &str) -> Option<&'static str> {
    crate::i18n_backend::lookup_en(source)
}

/// 复数查表：未登记的源串返回 `None`（调用方回落 [`translate_en`]）。
fn plural_en(source: &str) -> Option<(&'static str, &'static str)> {
    EN_PLURAL
        .binary_search_by_key(&source, |(key, _, _)| *key)
        .ok()
        .map(|index| {
            let (_, one, other) = EN_PLURAL[index];
            (one, other)
        })
}

/// 复数英译表：`(中文源串, 英文单数, 英文复数)`，按中文源串的 UTF-8 字节序升序
/// （二分查找依赖这一不变量）。
///
/// 源串里用 `{}` 站**一个数量**。中文不区分单复数（源串本身即中文文案），英文按
/// `count == 1` 取单数档。**每条源串必须同时在 `locales/en.toml` 登记**——那里是
/// 不带数量的兜底形式，也是「中文文案必须全部登记」守卫的落点（由
/// `plural_entries_are_well_formed` 钉住）。
///
/// 只收「英文单数真的和复数不同形」的条目：`{} tokens` 这类（单复数同形）留在
/// `en.toml` 就够，登记进来只会多一份要维护的重复。
///
/// 为什么留在这里而不进 TOML：rust-i18n 的 3.x 与 4.2.4 **都没有复数 API**，
/// 复数形态只能自己维护。
static EN_PLURAL: &[(&str, &str, &str)] = &[
    ("{} 分钟前", "{} minute ago", "{} minutes ago"),
    ("{} 天前", "{} day ago", "{} days ago"),
    ("{} 小时前", "{} hour ago", "{} hours ago"),
    (
        "{} 条待查看错误",
        "{} error to review",
        "{} errors to review",
    ),
    ("{} 秒前", "{} second ago", "{} seconds ago"),
    ("含 {} 个任务", "{} task", "{} tasks"),
    ("含 {} 个子任务", "{} subtask", "{} subtasks"),
    (
        "展开剩余 {} 个任务",
        "Show {} more task",
        "Show {} more tasks",
    ),
    (
        "已归档项目，连带 {} 个任务。",
        "Project archived, along with {} task.",
        "Project archived, along with {} tasks.",
    ),
    (
        "已清空归档，共 {} 项。",
        "Archive cleared; {} item removed.",
        "Archive cleared; {} items removed.",
    ),
    (
        "归档区里的 {} 项将被永久删除，无法恢复。",
        "The {} item in the archive will be permanently deleted. This cannot be undone.",
        "The {} items in the archive will be permanently deleted. This cannot be undone.",
    ),
    (
        "本视图 {} 条",
        "{} task in this view",
        "{} tasks in this view",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// 含中文但**不是界面文案**的源文件（相对 workspace `crates/` 的后缀）。
    ///
    /// - `duck-mail/src/models.rs` 的 IMAP 文件夹别名表。那里的中文字符串
    ///   （`"已发送"` / `"垃圾箱"` / `"草稿箱"` …）是 **`FolderRole::from_imap` 拿去和
    ///   服务端返回的文件夹名做比对**的，一个字节都不会渲染；国内邮箱（QQ / 163 /
    ///   Foxmail）经常不给 `\Sent` 这类旗标，只给中文名，所以这张表必须留在内核里。
    /// - `duck-mail/src/db/repo/search.rs` 的**检索解析别名**。`canonical_key` /
    ///   `classify` / `parse_relative_day` 里的中文（`"发件人"`、`"未读"`、`"上周"` …）
    ///   是拿去和**用户敲进的查询串**做比对的输入侧字面量，同样一个字节都不渲染；
    ///   渲染出来的说法在 `duck-app/src/pages/mail.rs` 的搜索帮助表里，那里照常登记。
    ///
    /// 判据是「会不会被渲染」，不是「是不是中文」。加新条目要能说清**为什么不渲染**；
    /// 只是因为「翻译不了」而加进来，等于把守卫关了。
    const NON_UI_SOURCES: &[&str] = &["duck-mail/src/models.rs", "duck-mail/src/db/repo/search.rs"];

    /// 迁移收口断言：`static EN` 表不得回来。
    ///
    /// 英译的唯一数据源是 `locales/en.toml`。若有人「按老办法」重新引入静态表，
    /// `t()` 仍然只读 TOML —— 那些新词会被**静默忽略**（英文界面显示中文），
    /// 而 `cargo check` 至多给一条 dead_code 警告、不会失败。EN 表历史上正是被
    /// 多会话并发追加的热点，所以这里把它钉死。
    #[test]
    fn the_static_en_table_never_comes_back() {
        // 针拆成两段拼出来：否则断言里的字面量会把测试自己判红。
        let needle = ["static ", "EN:"].join("");
        let source = include_str!("i18n.rs");
        assert!(
            !source.contains(&needle),
            "i18n.rs 里出现了 `static EN` —— 英译数据源是 locales/en.toml，\
             请改 TOML（顺序无关），不要恢复静态表"
        );
    }

    /// [`Lang::tag`] 与 [`Locale::wire_name`] 必须逐字相同：前者是插件翻译文件名
    /// 与 `duck.locale` 的依据，后者是落盘偏好值。两处各写一份字面量，改一处忘另一处
    /// 会让插件译文在切语言时**静默查不到**（回落源串，编译与守卫全绿）。
    #[test]
    fn lang_tag_matches_wire_name() {
        assert_eq!(Lang::Zh.tag(), Locale::ZhHans.wire_name());
        assert_eq!(Lang::En.tag(), Locale::En.wire_name());
        // `System` 解析后只会落在两个具体语言之一，tag 因此也在这两者之内。
        assert!(matches!(Locale::System.resolve().tag(), "zh-Hans" | "en"));
    }

    /// 取词与占位符替换的基本行为。
    #[test]
    fn lookups_and_placeholders() {
        assert_eq!(t(Lang::Zh, "外观"), "外观");
        assert_eq!(t(Lang::En, "外观"), "Appearance");
        assert_eq!(t(Lang::En, "未登记的文案"), "未登记的文案");
        assert_eq!(tf(Lang::En, "{} 个工具", &["7"]), "7 tools");
        assert_eq!(tf(Lang::Zh, "剩余 {}", &["12 GB"]), "剩余 12 GB");
        // 参数多于占位符时多余的忽略，少于时保留未替换的占位符。
        assert_eq!(tf(Lang::En, "剩余 {}", &[]), "{} free");
        assert_eq!(
            Locale::ALL.map(Locale::wire_name),
            ["system", "zh-Hans", "en"]
        );
        assert_eq!(Locale::from_wire("en"), Locale::En);
        assert_eq!(Locale::from_wire("zh-Hans"), Locale::ZhHans);
        assert_eq!(Locale::from_wire("nope"), Locale::System);
    }

    /// 复数表必须有序，否则二分查找会漏词。
    #[test]
    fn plural_table_is_sorted() {
        for pair in EN_PLURAL.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "复数表顺序错误：{} 应在 {} 之后",
                pair[0].0,
                pair[1].0
            );
        }
    }

    /// 复数表每条都要：源串登记在 `locales/en.toml`、三列各含恰好一个 `{}`、
    /// 单复数不同形。
    ///
    /// 三条都是「登记进来时必须成立」的不变量：源串不在 `en.toml` 会让「中文文案
    /// 必须全部登记」守卫报红；占位符数目不一致会让 `fill` 少替换或多留 `{}`；
    /// 单复数同形的条目没有存在意义（`en.toml` 那条已经够了）。
    #[test]
    fn plural_entries_are_well_formed() {
        for &(source, one, other) in EN_PLURAL {
            assert!(
                translate_en(source).is_some(),
                "复数表的源串 {source} 未登记在 locales/en.toml"
            );
            for text in [source, one, other] {
                assert_eq!(text.matches("{}").count(), 1, "{text} 必须恰好有一个占位符");
            }
            assert_ne!(one, other, "{source} 的单复数同形，不必登记进复数表");
        }
    }

    /// 复数取词 + 相对时间分档。
    #[test]
    fn plurals_and_relative_time() {
        // 英文按 `count == 1` 选单数档。
        assert_eq!(tn(Lang::En, "{} 分钟前", 1), "1 minute ago");
        assert_eq!(tn(Lang::En, "{} 分钟前", 5), "5 minutes ago");
        assert_eq!(tn(Lang::En, "{} 天前", 1), "1 day ago");
        assert_eq!(tn(Lang::En, "{} 秒前", 1), "1 second ago");
        // 中文不分单复数，只做占位符替换。
        assert_eq!(tn(Lang::Zh, "{} 分钟前", 1), "1 分钟前");
        // 未登记复数表的源串回落 `EN` 的通用形式（可能复数不对，但不空白）。
        assert_eq!(tn(Lang::En, "{} 个工具", 2), "2 tools");

        // 相对时间分档：< 60s 走 `under_minute` 指定的口径。
        let just_now = UnderMinute::JustNow;
        let by_second = UnderMinute::Seconds;
        assert_eq!(relative_time(Lang::Zh, 0, just_now), "刚刚");
        assert_eq!(relative_time(Lang::Zh, 59, just_now), "刚刚");
        assert_eq!(relative_time(Lang::En, 59, just_now), "Just now");
        assert_eq!(relative_time(Lang::Zh, 45, by_second), "45 秒前");
        assert_eq!(relative_time(Lang::En, 45, by_second), "45 seconds ago");
        assert_eq!(relative_time(Lang::En, 1, by_second), "1 second ago");
        // 负数（时钟漂移）归零，不显示「-3 分钟前」。
        assert_eq!(relative_time(Lang::Zh, -5, just_now), "刚刚");
        // 三档边界与单数形态。
        assert_eq!(relative_time(Lang::Zh, 60, just_now), "1 分钟前");
        assert_eq!(relative_time(Lang::En, 60, just_now), "1 minute ago");
        assert_eq!(relative_time(Lang::En, 3_599, just_now), "59 minutes ago");
        assert_eq!(relative_time(Lang::En, 3_600, just_now), "1 hour ago");
        assert_eq!(relative_time(Lang::En, 86_399, just_now), "23 hours ago");
        assert_eq!(relative_time(Lang::En, 86_400, just_now), "1 day ago");
        assert_eq!(relative_time(Lang::En, 86_400 * 3, just_now), "3 days ago");
    }

    /// 统计数字的千分位分组：边界、负数、`i64` 两端。
    #[test]
    fn number_groups_thousands() {
        // 不足四位不分组，且首位之前绝不补分隔符（`1000` 不是 `,1000`）。
        assert_eq!(number(0), "0");
        assert_eq!(number(7), "7");
        assert_eq!(number(999), "999");
        assert_eq!(number(1_000), "1,000");
        // 逐档进位：5 位一处、7 位两处、10 位三处。
        assert_eq!(number(12_345), "12,345");
        assert_eq!(number(1_234_567), "1,234,567");
        assert_eq!(number(1_000_000_000), "1,000,000,000");
        // 负号在分组之前，不参与位数计算。
        assert_eq!(number(-1), "-1");
        assert_eq!(number(-999), "-999");
        assert_eq!(number(-1_000), "-1,000");
        assert_eq!(number(-1_234_567), "-1,234,567");
        // i64 两端：`MIN` 取绝对值会溢出，`unsigned_abs` 必须扛住。
        assert_eq!(number(i64::MIN), "-9,223,372,036,854,775,808");
        assert_eq!(number(i64::MAX), "9,223,372,036,854,775,807");
    }

    /// 字节数的可读化：档位边界、「只有 B 无小数」、TB 封顶。
    #[test]
    fn bytes_use_binary_units_without_decimals_only_for_bytes() {
        // 不足 1 KB 原样印整数：`0.0 B` 是纯噪音。
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1024), "1.0 KB");
        // 收拢前的分歧点：`status_support` 那份会输出 `2 KB`，这里统一保留 1 位小数。
        assert_eq!(bytes(1536), "1.5 KB");
        assert_eq!(bytes(1024 * 1024), "1.0 MB");
        assert_eq!(bytes(1024 * 1024 * 1024 * 3 / 2), "1.5 GB");
        assert_eq!(bytes(1024_u64.pow(4) * 2), "2.0 TB");
        // TB 封顶：再大也只把数字放大，不引入 PB 档。
        assert_eq!(bytes(1024_u64.pow(5)), "1024.0 TB");
        assert_eq!(bytes(u64::MAX), "16777216.0 TB");
    }

    /// 日期时间：UTC + ISO 顺序；非法时间戳（`<= 0`）给空串。
    #[test]
    fn datetime_is_utc_and_iso_ordered() {
        // 2026-10-12 02:00:00 UTC（= 北京时间 10:00）。按 UTC 显示是本仓的既定口径。
        assert_eq!(datetime_full(1_791_770_400), "2026-10-12 02:00");
        assert_eq!(datetime_short(1_791_770_400), "10-12 02:00");
        // 解析不出 `Date` 头时内核写 0 —— 留空，而不是印出 1970 年。
        assert_eq!(datetime_full(0), "");
        assert_eq!(datetime_short(0), "");
        assert_eq!(datetime_full(-5), "");
        assert_eq!(datetime_short(-5), "");
    }

    /// 日历还原：已知日期、闰日、1970 之前、跨日进位。
    #[test]
    fn civil_calendar_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_089), (2025, 1, 1));
        // 闰日：2024-02-29 是 1970-01-01 之后的第 19782 天。
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        // 1970 之前：负数天数也要落在 1969-12-31，而不是差一天。
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // 跨日进位：2026-10-11 23:59 UTC 再加 60 秒就是 10-12 00:00。
        let base = 1_791_770_400 - 60 - 3_600 * 2;
        assert_eq!(datetime_short(base + 60), "10-12 00:00");
    }

    /// 语言标签解析（`LANG` / `LC_*` / macOS `AppleLocale` 同一条规则）。
    #[test]
    fn language_tags() {
        assert_eq!(lang_from_tag("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(lang_from_tag("zh-Hans"), Some(Lang::Zh));
        assert_eq!(lang_from_tag("en_US.UTF-8"), Some(Lang::En));
        assert_eq!(lang_from_tag("ja_JP"), Some(Lang::En));
        assert_eq!(lang_from_tag("C"), None);
        assert_eq!(lang_from_tag("POSIX"), None);
        assert_eq!(lang_from_tag("  "), None);
    }

    /// 源码里的中文文案必须全部登记，避免新增文案漏翻。
    ///
    /// 只扫非测试代码：`#[cfg(test)]` 之后（含断言消息、panic 文案）不属于界面文案。
    #[test]
    fn every_chinese_literal_is_translated() {
        let mut missing: Vec<String> = Vec::new();
        for file in rust_sources() {
            // 含中文但**不是界面文案**的源文件先跳过——判据是「这个字面量会不会被渲染
            // 到屏幕上」，不是「它是不是中文」。名单与理由见 [`NON_UI_SOURCES`]。
            if NON_UI_SOURCES
                .iter()
                .any(|suffix| file.to_string_lossy().ends_with(suffix))
            {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("读取源码");
            for (line_index, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("#[cfg(test)]") {
                    break;
                }
                if trimmed.starts_with("//") {
                    continue;
                }
                for literal in literals(line) {
                    if !contains_cjk(literal) {
                        continue;
                    }
                    if translate_en(literal).is_none() {
                        missing.push(format!("{}:{} → {literal}", file.display(), line_index + 1));
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "以下文案缺少英译：\n{}",
            missing.join("\n")
        );
    }

    fn workspace_crates() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates 目录")
            .to_path_buf()
    }

    fn rust_sources() -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect(&workspace_crates(), &mut files);
        files.sort();
        files
    }

    fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // 集成测试目录（`crates/*/tests/`）不是界面代码：里面的断言与
                // panic 文案不进 EN 表，否则守卫会把测试夹具当文案拦下。
                if path.file_name().is_some_and(|name| name == "tests") {
                    continue;
                }
                collect(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                // 单测模块文件（`xxx/tests.rs`）与上面的 `tests/` 目录同一类东西，
                // 但逐行扫描**拦不住它**：下游靠「见到 `#[cfg(test)]` 就停」收手，
                // 而 `mod tests;` 的那个属性写在**父文件**里，落到 `tests.rs` 里就没
                // 了 —— 于是整份断言与夹具（测试名、失败消息、`prod-01 副本` 这种
                // 夹具串）都会被当成界面文案要求登记。
                if path.file_stem().is_some_and(|stem| stem == "tests") {
                    continue;
                }
                files.push(path);
            }
        }
    }

    /// 极简字面量扫描：逐行取 `"…"` 之间的内容（字符串内的 `\"` 视为转义）。
    fn literals(line: &str) -> Vec<&str> {
        let mut found = Vec::new();
        let mut start: Option<usize> = None;
        let mut escaped = false;
        for (index, ch) in line.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' if start.is_some() => escaped = true,
                '"' => match start.take() {
                    Some(from) => found.push(&line[from..index]),
                    None => start = Some(index + 1),
                },
                _ => {}
            }
        }
        found
    }

    fn contains_cjk(text: &str) -> bool {
        text.chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
    }
}
