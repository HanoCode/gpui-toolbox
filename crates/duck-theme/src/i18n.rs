//! 极简 i18n 层：语言枚举 + 以「中文源串」为键的英译表。
//!
//! 设计取舍：**源串即键**（`t(lang, "外观")`），缺表项自动回落源串，所以新增文案
//! 也不会翻成空串；英译集中在 [`EN`]（按 key 字节序排序，供二分查找），并有
//! `cargo test` 里的两条守卫：表必须有序、源码里的中文文案必须全部登记。

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
fn translate_en(source: &str) -> Option<&'static str> {
    EN.binary_search_by_key(&source, |(key, _)| *key)
        .ok()
        .map(|index| EN[index].1)
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

/// 英译表（按中文源串的 UTF-8 字节序升序，二分查找依赖这一不变量）。
static EN: &[(&str, &str)] = &[
    ("(无主题)", "(no subject)"),
    ("(无收件人)", "(no recipient)"),
    ("(未知发件人)", "(unknown sender)"),
    ("(表达式)", "(expression)"),
    (
        "0 ~ 2；留空用服务端默认。",
        "0 – 2; blank uses the server default.",
    ),
    ("10.0.1.13 或主机名", "10.0.1.13 or host name"),
    ("12 栏网格", "12-column grid"),
    ("42 天 6 小时", "42d 6h"),
    ("47 个", "47"),
    ("ACL 用户名", "ACL username"),
    ("AI key 保存失败：{}", "Failed to store AI key: {}"),
    (
        "AI 产出的写 / 多条语句已填入编辑器（{} tokens），请审阅后执行",
        "AI-produced write / multi-statement SQL was filled into the editor ({} tokens); review and run it yourself",
    ),
    (
        "AI 产出的语句需要人工审阅，已填入 SQL 编辑器。",
        "AI-produced statements need manual review and were filled into the SQL editor.",
    ),
    ("AI 修错", "AI fix"),
    ("AI 助手", "AI Assistant"),
    (
        "AI 只出草稿，发送前需你确认。",
        "AI only drafts — you confirm before sending.",
    ),
    ("AI 失败：{}", "AI failed: {}"),
    (
        "AI 已执行只读 SQL · {} tokens",
        "AI executed read-only SQL · {} tokens",
    ),
    (
        "AI 已给出解释 · {} tokens",
        "AI explanation ready · {} tokens",
    ),
    ("AI 执行失败：{}", "AI run failed: {}"),
    ("AI 没有返回解释内容。", "The AI returned no explanation."),
    (
        "AI 生成的 SQL 已执行（{} ms）。",
        "AI-generated SQL executed ({} ms).",
    ),
    (
        "AI 生成的 SQL 执行失败：{}",
        "Failed to run the AI-generated SQL: {}",
    ),
    ("AI 解释", "AI explain"),
    ("AI 设置已保存。", "AI settings saved."),
    ("AI 请求失败：{}", "AI request failed: {}"),
    ("Base64 解出 {} 字节。", "Base64 decoded {} bytes."),
    (
        "CARGO_MANIFEST_DIR 必须存在",
        "CARGO_MANIFEST_DIR must be set",
    ),
    (
        "CDP 控制本地浏览器嗅探媒体地址",
        "Sniffs media URLs through a CDP-controlled browser",
    ),
    (
        "DASH 页面会返回音视频分离的分片流，工具会自动合并；分片走有界 Range 请求，需保留完整请求头。",
        "DASH pages return separate audio and video segment streams which the tool merges; segments use bounded Range requests, so full headers must be kept.",
    ),
    (
        "Duck Toolbox 是一个基于 GPUI 构建的桌面工具箱外壳：把常用小工具收进同一个窗口，能力由插件扩展。",
        "Duck Toolbox is a desktop toolbox shell built on GPUI: it gathers everyday utilities into a single window and grows through plugins.",
    ),
    (
        "Duck Toolbox 的示例外壳：自绘标题栏、活动栏、可拖拽侧栏、工具栏与状态栏。",
        "A sample Duck Toolbox shell: custom title bar, activity bar, resizable sidebar, toolbar and status bar.",
    ),
    ("GZIP 解压出 {} 字节。", "GZIP inflated {} bytes."),
    ("HTML 优先", "HTML first"),
    ("HTTP 请求回放", "HTTP request replay"),
    ("IMAP 服务器", "IMAP server"),
    ("IMAP 端口需是 1–65535。", "The IMAP port must be 1–65535."),
    ("JS 脚本", "JS script"),
    ("JSON 美化失败：{}", "Failed to pretty-print JSON: {}"),
    (
        "JWT 的 {} 段不是合法的 UTF-8 JSON。",
        "The JWT {} segment is not valid UTF-8 JSON.",
    ),
    (
        "JWT 的 {} 段不是合法的 base64url。",
        "The JWT {} segment is not valid base64url.",
    ),
    (
        "KEYS 会遍历整个键空间，可能阻塞服务端；建议改用 SCAN。",
        "KEYS walks the whole keyspace and may block the server; prefer SCAN.",
    ),
    ("M1.2 接入", "Wired up in M1.2"),
    (
        "M1.2 收尾接入命名空间键树时生效",
        "Takes effect when the namespace key tree lands at the end of M1.2",
    ),
    ("M1.4 接入", "Wired up in M1.4"),
    (
        "M1.4 接入：留空自动探测（CONFIG GET databases）",
        "Wired up in M1.4; leave empty to auto-detect (CONFIG GET databases)",
    ),
    (
        "M1.4 接入；当前用系统级缺省",
        "Wired up in M1.4; system-level defaults are used for now",
    ),
    ("M1.5 接入", "Wired up in M1.5"),
    (
        "M1.5 接入：内核尚未启用 TLS 特性。",
        "Wired up in M1.5; the kernel does not have the TLS feature enabled yet.",
    ),
    (
        "M2 发信会用；现在只随档案保存。",
        "Used for sending in M2; for now it is just saved with the profile.",
    ),
    ("M2 接入", "Coming in M2"),
    (
        "MIT 许可，版权归 HanoCode。",
        "MIT license, copyright HanoCode.",
    ),
    (
        "N 快速新建 · ⌘K 命令面板 · 右键卡片有菜单",
        "N quick add · ⌘K command palette · right-click a card for its menu",
    ),
    ("NULL", "NULL"),
    (
        "OpenAI 兼容前缀，不带尾斜杠；请求发到该前缀下的 /chat/completions。",
        "OpenAI-compatible prefix without a trailing slash; requests go to /chat/completions under it.",
    ),
    ("OpenAI 兼容接口地址", "OpenAI-compatible API base URL"),
    (
        "PHP serialize 解析在第 {} 字节后还剩内容，可能不是单值。",
        "PHP serialize data has trailing content after byte {}; it may not be a single value.",
    ),
    (
        "PHP serialize 解析在第 {} 字节处失败。",
        "Failed to parse PHP serialize data at byte {}.",
    ),
    (
        "Q1 重要且紧急 · 立即处理",
        "Q1 Important and urgent · do it now",
    ),
    (
        "Q2 重要不紧急 · 尽快安排或委派",
        "Q2 Important, not urgent · schedule or delegate",
    ),
    (
        "Q3 紧急不重要 · 计划安排",
        "Q3 Urgent, not important · plan it",
    ),
    (
        "Q4 都不紧急 · 有空再处理",
        "Q4 Neither urgent nor important · do it later",
    ),
    ("Redis 连接", "Redis connection"),
    (
        "SCAN 每次返回的键数上限；连接档案里单独设置的以档案为准。",
        "Maximum keys returned per SCAN; a per-connection override wins.",
    ),
    ("SFTP 文件管理", "SFTP files"),
    ("SMTP 服务器", "SMTP server"),
    ("SMTP 端口需是 1–65535。", "The SMTP port must be 1–65535."),
    ("SQL 执行", "SQL execution"),
    ("SQL 执行失败：{}", "SQL execution failed: {}"),
    ("SQL 格式化与编辑器", "SQL formatting and editor"),
    ("SQL 结果", "SQL results"),
    ("SQL 语义诊断", "SQL semantic diagnostics"),
    (
        "SQL 语义诊断：已关闭（点击开启）",
        "SQL semantic diagnostics: off (click to turn on)",
    ),
    (
        "SQL 语义诊断：已开启（点击关闭）",
        "SQL semantic diagnostics: on (click to turn off)",
    ),
    (
        "SQL 过长时在编辑器内折行，而不是横向滚动。",
        "Wrap long SQL inside the editor instead of scrolling sideways.",
    ),
    (
        "SQLite 是进程内数据库，没有服务端会话列表；下面是本连接挂载的数据库文件。",
        "SQLite is an in-process database with no server-side session list; below are the database files attached to this connection.",
    ),
    (
        "SQLite 这里填数据库文件路径（:memory: 也允许），不可留空。",
        "For SQLite this is the database file path (the :memory: keyword works too); it cannot be blank.",
    ),
    ("SSH 会话", "SSH session"),
    (
        "SSH 会话意外断开后按退避重试。主机密钥或认证失败不重试，只留手动入口。",
        "Retry SSH sessions with backoff after an unexpected drop. Host-key or auth failures are never retried — manual reconnect only.",
    ),
    ("SSH 口令", "SSH password"),
    ("SSH 口令保存失败：{}", "Failed to store SSH password: {}"),
    ("SSH 地址", "SSH address"),
    ("SSH 密码", "SSH password"),
    (
        "SSH 密钥与跳板机（ProxyJump）在 M1.5 接入；当前版本开隧道会在连接时明确报错，绝不静默直连。",
        "SSH keys and jump hosts (ProxyJump) land in M1.5; enabling a tunnel in this version fails loudly on connect and never silently falls back to a direct one.",
    ),
    ("SSH 用户", "SSH user"),
    ("SSH 用户名", "SSH username"),
    ("SSH 端口", "SSH port"),
    (
        "SSH 端口必须是 1-65535 的数字",
        "SSH port must be a number between 1 and 65535",
    ),
    ("SSH 终端", "SSH Terminal"),
    ("SSH 隧道", "SSH tunnel"),
    (
        "SSH_ASKPASS 在 Windows 上不受支持",
        "SSH_ASKPASS is not supported on Windows",
    ),
    ("SSL 模式", "SSL mode"),
    ("STARTTLS（143 / 587）", "STARTTLS (143 / 587)"),
    ("TLS（993 / 465）", "TLS (993 / 465)"),
    (
        "URL 解码结果不是合法的 UTF-8。",
        "The URL-decoded result is not valid UTF-8.",
    ),
    ("UTC", "UTC"),
    ("WASM 组件", "WASM component"),
    (
        "WebP / AVIF / PNG 互转与压缩",
        "Converts and compresses WebP / AVIF / PNG",
    ),
    (
        "host、host:port、… 或 /path/to.sock",
        "host, host:port, … or /path/to.sock",
    ),
    ("{hours} 小时 {minutes} 分", "{hours}h {minutes}m"),
    ("{minutes} 分 {rest} 秒", "{minutes}m {rest}s"),
    ("{rest} 秒", "{rest}s"),
    ("{} / {} 键", "{} / {} keys"),
    ("{} / {} 项", "{} / {} matches"),
    ("{} · {}（本地端口 {}）", "{} · {} (local port {})"),
    ("{} · {}：{} 封新邮件", "{} · {}: {} new messages"),
    ("{} · 撤销", "{} · Undo"),
    ("{} 不存在。", "{} does not exist."),
    ("{} 个", "{}"),
    ("{} 个工具", "{} tools"),
    ("{} 个插件 · {} 个工具", "{} plugins · {} tools"),
    ("{} 个文件 · {}", "{} files · {}"),
    (
        "{} 个组件 · {} 份许可全文",
        "{} components · {} license texts",
    ),
    (
        "{} 个组件 · {} 份许可全文 · {} 种许可声明",
        "{} components · {} license texts · {} license expressions",
    ),
    ("{} 个账户", "{} accounts"),
    ("{} 个连接", "{} connections"),
    ("{} 分钟", "{} min"),
    ("{} 分钟前", "{} minutes ago"),
    ("{} 列 · {} 行", "{} columns · {} rows"),
    ("{} 副本", "{} copy"),
    ("{} 占用 {}", "{} uses {}"),
    ("{} 天", "{} days"),
    ("{} 天内用过 {} 个", "{} used in the last {} days"),
    (
        "{} 天内还没用过任何工具",
        "Nothing used in the last {} days",
    ),
    ("{} 天前", "{} days ago"),
    ("{} 字节", "{} bytes"),
    ("{} 封邮件", "{} messages"),
    ("{} 小时", "{} h"),
    ("{} 小时前", "{} hours ago"),
    ("{} 条", "{} messages"),
    ("{} 条 · {} 条未读", "{} total · {} unread"),
    ("{} 条待提交", "{} pending"),
    ("{} 条待查看错误", "{} errors to review"),
    ("{} 条语句", "{} statements"),
    ("{} 步", "{} step(s)"),
    ("{} 秒", "{}s"),
    ("{} 秒前", "{} seconds ago"),
    ("{} 索引失败：{}", "Failed to index {}: {}"),
    ("{} 行", "{} rows"),
    (
        "{} 行 · {} 条语句 · {} ms",
        "{} rows · {} statements · {} ms",
    ),
    ("{} 行/页", "{} rows/page"),
    ("{} 项", "{} items"),
    ("{} 项匹配", "{} matches"),
    ("{}-结果", "{}-Result"),
    ("{}-结果{}", "{}-Result {}"),
    ("{}/{} 个任务", "{}/{} tasks"),
    ("{}失败（{}）：{}", "{} failed ({}): {}"),
    ("{}失败：{}", "{} failed: {}"),
    (
        "{}的钩子未通过，已按配置中断连接。",
        "The {} hook did not pass; the connection was aborted as configured.",
    ),
    ("{}（已取消）", "{} (cancelled)"),
    ("{}（已截断）", "{} (truncated)"),
    ("{}：内容未变，跳过", "{}: unchanged, skipped"),
    ("{}：写入 {} 块（{} 个文件）", "{}: {} chunks from {} files"),
    ("{}：已写入 {} 块", "{}: {} chunks written"),
    (
        "~/.ssh/known_hosts 中已有该主机记录，但本次主机密钥校验失败，OpenSSH 已拒绝连接。请核对后执行 ssh-keygen -R {} 删除旧记录再重连。",
        "~/.ssh/known_hosts already has an entry for this host, but host key verification failed, so OpenSSH refused the connection. Verify the fingerprint, then run ssh-keygen -R {} to remove the stale entry and reconnect.",
    ),
    ("… 共 {} 字节", "… {} bytes total"),
    (
        "「{}」不是可用的组合。",
        "\"{}\" is not a usable combination.",
    ),
    (
        "「{}」不是有效的邮箱地址。",
        "\"{}\" is not a valid email address.",
    ),
    (
        "「{}」不能用作全局快捷键：媒体键与音量键要额外的系统授权，还会抢走系统控制。",
        "\"{}\" cannot be a global shortcut: media and volume keys need extra system permission and would take over the system controls.",
    ),
    (
        "「{}」及该项目下的任务将被永久删除，无法恢复。",
        "“{}” and the tasks inside it will be permanently deleted. This cannot be undone.",
    ),
    ("「{}」后端尚未接入", "\"{}\" backend is not wired up yet"),
    (
        "「{}」将被删除，确定继续吗？",
        "“{}” will be deleted. Continue?",
    ),
    (
        "「{}」将被永久删除，无法恢复。",
        "“{}” will be permanently deleted. This cannot be undone.",
    ),
    ("「{}」执行完成", "\"{}\" finished."),
    (
        "「{}」的插件目录会被删除，它贡献的工具也会一起消失。",
        "The plugin folder for \"{}\" will be deleted, along with any tools it contributes.",
    ),
    (
        "「{}」类型的专用查看器尚未实现（计划在后续增量补上）。",
        "A dedicated viewer for {} keys is not implemented yet (planned for a later increment).",
    ),
    (
        "「更多操作」菜单尚未接入业务逻辑",
        "The “More actions” menu is not wired up yet",
    ),
    (
        "「跟随标签」= 生产档默认锁写；「允许」压过生产锁；「锁定」在任何档位都锁。",
        "\"Follow tag\" keeps the production default write lock; \"Allow\" overrides it; \"Lock\" locks on every tag.",
    ),
    (
        "一条记录 = 一个端点 + 一个模型。API key 加密存在本机，不写进设置文件。",
        "One record = one endpoint + one model. The API key is encrypted on this machine, never in the settings file.",
    ),
    (
        "一次问答内最多允许的连续工具调用轮数；留空用默认值 8。",
        "Max consecutive tool-call rounds within one Q&A; blank uses the default of 8.",
    ),
    (
        "三档都空着，存下来会是一份空档案。",
        "All three transports are empty; saving would store an empty profile.",
    ),
    ("上一个", "Previous"),
    (
        "上一条语句还在跑，先等它结束或点「停止」。",
        "The previous statement is still running — wait for it or press Stop.",
    ),
    (
        "上一次执行失败了；修正 SQL 后重新执行。",
        "The last execution failed; fix the SQL and run it again.",
    ),
    ("上一步", "Back"),
    ("上一级目录", "Parent folder"),
    ("上一页", "Previous page"),
    ("上下文估算", "Context estimate"),
    ("上下文预算", "Context budget"),
    (
        "上下文预算要是不小于 0 的整数",
        "The context budget must be an integer of 0 or more",
    ),
    ("上传", "Upload"),
    ("上传中", "Uploading"),
    ("上传选中项", "Upload selected"),
    ("上传限速", "Upload limit"),
    ("下一个", "Next"),
    ("下一步", "Next"),
    ("下一页", "Next page"),
    ("下载", "download"),
    ("下载中", "Downloading"),
    ("下载并安装", "Download and install"),
    ("下载正文", "Download body"),
    ("下载正文失败：{}", "Could not download the body: {}"),
    ("下载的是直链还是分片", "Direct link or segments?"),
    ("下载选中项", "Download selected"),
    ("下载限速", "Download limit"),
    (
        "下面的三档就是这份档案的内容；保存连接时会一并存回档案。",
        "The three entries below are this profile's contents; saving the connection stores them back into the profile.",
    ),
    ("不为空", "is not null"),
    ("不使用代理", "No proxy"),
    (
        "不勾选则不保存口令，每次连接时在终端里输入。",
        "Unchecked: the password is not saved; enter it in the terminal on each connection.",
    ),
    ("不启用", "Disabled"),
    ("不存在", "Missing"),
    (
        "不早于这一天（newer: 同义；中文「晚于:」「之后:」同义）",
        "On or after this day (newer: is an alias; the Chinese aliases work too)",
    ),
    ("不是合法 JSON：{}", "Not valid JSON: {}"),
    (
        "不是合法的 Base64（标准 / URL 安全、带 / 不带 padding 都不匹配）。",
        "Not valid Base64 (neither standard nor URL-safe, with or without padding).",
    ),
    ("不是合法的 GZIP 数据：{}", "Not valid GZIP data: {}"),
    (
        "不是合法的 JWT（应是 `头.载荷` 或 `头.载荷.签名`）。",
        "Not a valid JWT (expected header.payload or header.payload.signature).",
    ),
    (
        "不是合法的 URL 编码（`%` 后面要跟两位十六进制）。",
        "Not valid URL encoding (% must be followed by two hex digits).",
    ),
    (
        "不是合法的 UTF-8 文本，可切到「Hex」档查看原始字节。",
        "Not valid UTF-8 text; switch to the Hex tab to view the raw bytes.",
    ),
    (
        "不是整数，无法按 Unix 时间戳解析（应为整数秒 / 毫秒 / 微秒 / 纳秒）。",
        "Not an integer, so it cannot be read as a Unix timestamp (expected whole seconds, milliseconds, microseconds or nanoseconds).",
    ),
    ("不用 SSM", "No SSM"),
    ("不用 TLS", "No TLS"),
    ("不经代理", "No proxy"),
    ("丢弃", "Discard"),
    ("丢弃全部暂存改动", "Discard all staged changes"),
    ("丢弃更改", "Discard changes"),
    (
        "两种正文都有的邮件默认显示哪一份；阅读器里仍可随时切换。",
        "Which body to show when a message has both; the reader can still switch any time.",
    ),
    ("两者同时", "Both"),
    ("严格只读（ACL 限制）", "Strict read-only (ACL)"),
    ("个", ""),
    ("个键", "keys"),
    ("中", "Medium"),
    ("中文", "Chinese"),
    ("为空", "is null"),
    ("主机", "Host"),
    ("主机 *", "Host *"),
    ("主机地址", "Host"),
    ("主机地址为空。", "Host is empty."),
    (
        "主机密钥与 ~/.ssh/known_hosts 中记录的不一致，OpenSSH 已拒绝连接。这可能是对方重装或更换了密钥，也可能是中间人攻击——请先核对，确认无误后执行 ssh-keygen -R {} 删除旧记录再重连。",
        "The host key does not match the entry in ~/.ssh/known_hosts, so OpenSSH refused the connection. This may mean the server was reinstalled or rotated its key, or it may be a man-in-the-middle attack. Verify the fingerprint first; once confirmed, run ssh-keygen -R {} to remove the stale entry and reconnect.",
    ),
    ("主目录", "Home folder"),
    ("主窗口", "Main window"),
    ("主键", "Primary key"),
    ("主题", "Theme"),
    ("主题与配色", "Theme & colors"),
    (
        "主题导入尚未接入（外壳无主题文件读写）",
        "Theme import is not wired up (the shell has no theme file IO)",
    ),
    (
        "主题导出（完整）尚未接入",
        "Theme export (full) is not wired up",
    ),
    (
        "主题导出（差异快照）尚未接入",
        "Theme export (diff snapshot) is not wired up",
    ),
    ("主题模式", "Theme mode"),
    ("主题画廊尚未提供", "No theme gallery is provided yet"),
    ("主题色", "Accent color"),
    (
        "主题，子串匹配；中文「主题:」或「标题:」同义",
        "Subject, substring match; the Chinese aliases work too",
    ),
    ("事件", "Events"),
    ("事务中", "In transaction"),
    ("二进制内容，共 {} 字节", "Binary content, {} bytes"),
    ("亮色", "Light"),
    ("亮色主题", "Light theme"),
    (
        "仅 UI 外壳示例，未接入任何业务能力。",
        "A UI shell sample only; no business logic is wired up.",
    ),
    ("仅显示前 400 项。", "Showing the first 400 entries."),
    ("仅本次", "Just this time"),
    ("仅本账户可见。", "Visible only to this account."),
    (
        "仅用于 Redis 6.0+（ACL）；留空表示只用口令认证",
        "Redis 6.0+ (ACL) only; leave empty to authenticate with a password only",
    ),
    ("仅警告与错误", "Warnings and errors"),
    ("今天", "Today"),
    ("今天到期 {}", "{} due today"),
    (
        "从中间列表选一封邮件，正文会在这里显示。",
        "Pick a message from the middle list and its body will show here.",
    ),
    (
        "从中间列表选一封邮件，正文会在这里渲染。",
        "Pick a message from the list; its body renders here.",
    ),
    (
        "从右侧对象树选择一张表",
        "Pick a table from the object tree on the right",
    ),
    ("从备份导入？", "Import from this backup?"),
    ("从备份文件导入", "Import from a backup"),
    (
        "从左侧选择一个连接，或新建连接后回车打开。",
        "Pick a connection on the left, or create one and open it.",
    ),
    ("从左侧选择工具", "Pick a tool from the sidebar"),
    (
        "从左侧选择工具后，这里显示该工具的帮助与设置。",
        "Pick a tool on the left; its help and settings appear here.",
    ),
    (
        "从左栏选择一条连接，或在左栏底部新建一条。",
        "Pick a connection from the sidebar, or create one at the bottom.",
    ),
    (
        "从市场安装，或将插件放入 {} 后重启应用。",
        "Install from the marketplace, or drop plugins into {} and restart.",
    ),
    (
        "从抓包记录重建请求",
        "Rebuilds requests from captured traffic",
    ),
    ("从插件市场开始", "Start from the plugin center"),
    ("从文件导入…", "Import from file…"),
    ("从未索引", "Never indexed"),
    ("代理", "Proxy"),
    ("代理主机", "Proxy host"),
    ("代理口令", "Proxy password"),
    ("代理口令保存失败：{}", "Failed to store proxy password: {}"),
    ("代理用户", "Proxy user"),
    ("代理端口", "Proxy port"),
    (
        "代理端口必须是 1-65535 的数字",
        "Proxy port must be a number between 1 and 65535",
    ),
    ("仪表板", "Dashboards"),
    ("仪表板名称", "Dashboard name"),
    ("仪表盘", "Dashboard"),
    ("任务", "Tasks"),
    (
        "任务列表中的图标风格，与主题相互独立。",
        "Icon style used in task lists; independent of the theme.",
    ),
    (
        "任务列表的行高从 44 收到 30。",
        "Task rows shrink from 44 to 30.",
    ),
    ("任务操作", "Task actions"),
    ("任务标题", "Task title"),
    ("任务详情", "Task details"),
    ("优先级", "Priority"),
    ("优先级…", "Priority…"),
    ("优先级已设为 {}", "Priority set to {}"),
    ("优先级最高", "Priority first"),
    ("会话", "Session"),
    ("会话参数", "Conversation settings"),
    (
        "会话库不可用，无法保存这轮对话",
        "The conversation store is unavailable, so this turn cannot be saved",
    ),
    (
        "会话库不可用，无法新建会话",
        "The conversation store is unavailable, so a new conversation cannot be created",
    ),
    (
        "会话库打开失败：{}",
        "Could not open the conversation store: {}",
    ),
    ("会话标题", "Conversation title"),
    ("会话详情", "Session details"),
    ("传输中", "Transferring"),
    ("传输失败", "Transfer failed"),
    ("传输失败：{}", "Transfer failed: {}"),
    ("传输档案", "Transport profile"),
    ("传输档案已保存。", "Transport profile saved."),
    ("传输档案已删除。", "Transport profile deleted."),
    (
        "传输状态出现未登记的迁移，仍按新状态应用",
        "Transfer state saw an unregistered transition; applying the new state anyway",
    ),
    (
        "传输进程状态锁已中毒",
        "Transfer process state lock is poisoned",
    ),
    ("传输通道", "Transport"),
    ("传输队列", "Transfers"),
    ("低", "Low"),
    ("作者", "Author"),
    (
        "你是 Duck Toolbox 的内置助手。回答用简洁的中文，代码与命令原样给出。信息不足时直接说不确定，不要编造。",
        "You are Duck Toolbox's built-in assistant. Answer concisely in Chinese; give code and commands verbatim. When the information is insufficient, say so plainly instead of making things up.",
    ),
    ("使用", "Usage"),
    (
        "使用 ssh-agent 中已加载的密钥，由 agent 完成签名。",
        "Use a key loaded in ssh-agent; the agent signs for you.",
    ),
    ("使用主题字体", "Use theme font"),
    ("使用引导", "Getting started"),
    ("使用次数", "Times used"),
    ("例如 app-v2.tar.gz", "e.g. app-v2.tar.gz"),
    ("例如 prod-03", "e.g. prod-03"),
    ("例如 releases", "e.g. releases"),
    (
        "例如 团队 A / 个人 / 客户 X",
        "e.g. Team A / personal / Client X",
    ),
    ("例如：30", "e.g. 30"),
    ("例如：JetBrains Mono", "e.g. JetBrains Mono"),
    (
        "例如：openvpn --config prod.ovpn",
        "For example: openvpn --config prod.ovpn",
    ),
    ("例如：公司代理", "e.g. corporate proxy"),
    ("例如：本地开发库", "e.g. local dev database"),
    ("例如：本地缓存", "e.g. local cache"),
    ("侧栏", "Sidebar"),
    ("侧栏项间距", "Sidebar item spacing"),
    ("保存", "Save"),
    ("保存 AI 设置", "Save AI settings"),
    ("保存中…", "Saving…"),
    ("保存值", "Save value"),
    ("保存到已保存查询", "Save to saved queries"),
    ("保存后复制", "Copy after saving"),
    (
        "保存后立刻出现在助手页的模型下拉里。",
        "Appears in the assistant's model dropdown as soon as it is saved.",
    ),
    ("保存后立即打开", "Open right after saving"),
    ("保存图表", "Save chart"),
    ("保存失败：{}", "Save failed: {}"),
    (
        "保存常用命令，一键复制执行",
        "Saves frequent commands for one-click copy",
    ),
    ("保存并刷新", "Save and refresh"),
    ("保存更改", "Save changes"),
    ("保存查询", "Save Query"),
    ("保存生成参数", "Save generation parameters"),
    ("保存草稿失败：{}", "Could not save the draft: {}"),
    ("保存这一行", "Save this row"),
    ("保存连接", "Save connection"),
    ("保存附件失败：{}", "Could not save the attachment: {}"),
    (
        "保留 EXIF 等元数据；关闭可进一步压缩体积。",
        "Keeps EXIF and other metadata; turning it off shrinks the file further.",
    ),
    ("保留临时文件", "Keep temporary files"),
    ("保留元数据", "Keep metadata"),
    ("保留原样", "Keep as-is"),
    ("保留时长", "Keep for"),
    ("保留条数", "Entries kept"),
    ("信息", "Information"),
    (
        "信由本机经 SMTP 发出，并自动写回服务器的「已发送」文件夹（需已同步出该文件夹）。",
        "Sent over SMTP from this machine, with a copy written back to the server's Sent folder (once that folder has been synced).",
    ),
    ("修改", "Change"),
    ("修改 {} · {} 行", "Modified {} · {} rows"),
    ("修改失败：{}", "Could not save: {}"),
    ("修改时间", "Modified"),
    ("修改服务端配置", "Change server configuration"),
    ("修改此行", "Edit this row"),
    ("修改结构", "Alter structure"),
    ("修改行", "Edit row"),
    ("修改配置失败：{}", "Failed to change configuration: {}"),
    ("值", "Value"),
    ("值已保存", "Value saved"),
    (
        "值过大（{} 字节），未自动加载。",
        "Value too large ({} bytes); not loaded automatically.",
    ),
    ("偏好", "Preferences"),
    ("停止", "Stop"),
    ("停止失败：{}", "Failed to stop: {}"),
    ("停止执行（⌘.）", "Stop execution (⌘.)"),
    ("停止生成", "Stop generating"),
    (
        "停用后从工具箱列表隐藏，设置项保留。",
        "Disabled tools are hidden from the toolbox list; their settings are kept.",
    ),
    ("像素", "px"),
    ("允许", "Allow"),
    ("允许助手调用工具", "Let the assistant use tools"),
    ("元素值", "Element value"),
    ("元素已删除", "Element deleted"),
    ("元素已推入", "Element pushed"),
    ("元素已更新", "Element updated"),
    ("元素数", "Elements"),
    (
        "先切到图表视角再保存。",
        "Switch to the chart view before saving.",
    ),
    (
        "先添加一个邮箱账户再写信",
        "Add a mail account before composing",
    ),
    ("先给档案起个名字。", "Give the profile a name first."),
    ("先连上一条连接。", "Connect to a database first."),
    ("克隆连接", "Clone connection"),
    (
        "全局快捷键 {} 没能生效。{}可在「设置 → 行为」里换一个。",
        "The global shortcut {} is not working. {}Pick another one under Settings → Behaviour.",
    ),
    (
        "全局生效，与上面的应用内键位互不影响：应用内键位只在 Duck 有焦点时管用，这一条在任何程序里都能把 Duck 叫出来。",
        "Global, and independent of the in-app keys above: those only apply while Duck has focus, whereas this one summons Duck from any app.",
    ),
    ("全选", "Select All"),
    ("全部", "All"),
    ("全部任务", "All tasks"),
    ("全部任务 {}", "All tasks {}"),
    ("全部关掉", "Off"),
    ("全部分类", "All categories"),
    ("全部工具", "All tools"),
    ("全部提示", "All notifications"),
    ("全部显示", "Show all"),
    ("全部清除", "Clear everything"),
    ("全部状态", "All statuses"),
    ("全部类别", "All categories"),
    ("全部级别", "All levels"),
    ("全部项目", "All projects"),
    ("公钥", "Key"),
    ("公钥 + 口令", "Key + passphrase"),
    ("共", "Total"),
    ("共 {}", "Total {}"),
    ("共 {} 个问题", "{} problems in total"),
    (
        "共 {} 字节，可切到「文本」或「Hex」档查看原文。",
        "{} bytes in total; switch to the Text or Hex tab to see the raw content.",
    ),
    ("共 {} 条", "{} total"),
    (
        "共 {} 条，显示最近 {} 条。",
        "{} entries in total; showing the latest {}.",
    ),
    ("共 {} 行", "{} lines"),
    ("关", "Off"),
    ("关于", "About"),
    ("关于 Duck Toolbox", "About Duck Toolbox"),
    (
        "关掉后点「卸载」会直接开始，不再询问。",
        "When off, clicking Uninstall starts straight away without asking.",
    ),
    ("关系型", "Relational"),
    ("关联 id", "Trace id"),
    ("关键字", "Keyword"),
    ("关键字大小写", "Keyword case"),
    ("关键词", "Keywords"),
    ("关闭", "Close"),
    ("关闭全部", "Close all"),
    ("关闭全部标签页", "Close All Tabs"),
    ("关闭其他", "Close others"),
    ("关闭其他标签页", "Close Other Tabs"),
    ("关闭右侧", "Close to the right"),
    (
        "关闭后仅能解析公开分享，私有链接会失败。",
        "When off, only public shares resolve; private links fail.",
    ),
    ("关闭搜索", "Close search"),
    (
        "关闭时助手只能聊天，不注册任何工具。",
        "Off: the assistant can only chat; no tools are registered.",
    ),
    (
        "关闭时只弹出一个快速面板，盖在当前工作之上，不动主窗口；打开时唤起完整主窗口（经典行为）。",
        "When off, summoning shows only a quick panel over your current work and does not touch the main window; when on, it raises the full main window (the classic behaviour).",
    ),
    (
        "关闭时按合并处理：只覆盖备份里出现的设置，本机独有的保留。",
        "When off, merge instead: only settings present in the backup are overwritten; local-only ones are kept.",
    ),
    ("关闭标签", "Close tab"),
    ("关闭标签页", "Close tab"),
    ("关闭结果", "Close result"),
    ("其他", "Other"),
    ("其他邮箱", "Other email"),
    ("其它", "Other"),
    ("内存", "Memory"),
    (
        "内容区右栏停靠在哪一侧；目前仅工具箱页提供右栏内容，切换即时生效。",
        "Which side the content-area right sidebar docks on. Only the toolbox page has right-sidebar content for now. Applies immediately.",
    ),
    (
        "内嵌 Lucide 子集，线宽统一 1.75",
        "Bundled Lucide subset with a uniform 1.75 stroke",
    ),
    ("内置主题 {}", "Built-in theme {}"),
    (
        "内置工具已下线，能力全部来自插件。装好的插件会出现在工具箱的侧栏分类里。",
        "Built-in tools are retired; every capability now comes from plugins. Installed plugins appear as categories in the toolbox sidebar.",
    ),
    (
        "内置引擎复用本机浏览器会话，无需额外安装。",
        "The built-in engine reuses your local browser session; nothing to install.",
    ),
    ("内置浏览器（CDP）", "Built-in browser (CDP)"),
    ("内边距", "Padding"),
    (
        "再删除窗口位置、自定义快捷键与日志，回到初次启动的样子。",
        "Also removes window positions, custom keybindings and logs — back to a first-launch state.",
    ),
    (
        "再删除窗口位置、自定义快捷键与日志，并清空 AI 会话与知识库索引，回到初次启动的样子。",
        "Also removes window positions, custom keybindings and logs, and clears AI conversations and the knowledge-base index — back to a first launch.",
    ),
    (
        "再刷新几次就会出现趋势（当前只有一次采样）。",
        "Refresh a few more times to see a trend (one sample so far).",
    ),
    (
        "再点一下左栏那一项即可取消筛选。",
        "Click that item in the sidebar again to clear the filter.",
    ),
    (
        "再点一下左栏那个标签即可取消筛选。",
        "Click that tag in the sidebar again to clear the filter.",
    ),
    (
        "再点一下那个筛选即可取消。",
        "Click that filter again to clear it.",
    ),
    ("写 NULL", "Write NULL"),
    ("写入", "Writes"),
    ("写入失败：{}", "Write failed: {}"),
    ("写入字段", "Write field"),
    ("写入成员", "Write member"),
    ("写入文件", "Write files"),
    ("写入确认", "Write confirmation"),
    ("写操作已解锁 {} 分钟", "Writes unlocked for {} min"),
    (
        "写操作已解锁 {} 分钟，到期自动上锁",
        "Writes unlocked for {} min; relocks automatically",
    ),
    ("写权限", "Write access"),
    (
        "写满就轮转到下一段；一天最多留 {} 段，目录总量兜底 {}。",
        "A full file rolls into the next segment; up to {} segments per day, with a {} cap on the whole directory.",
    ),
    ("写邮件", "Compose"),
    ("减小字号", "Smaller font"),
    ("减小终端字号", "Decrease terminal font size"),
    ("凭据", "Credentials"),
    ("凭据存储不可用", "Credential storage is unavailable"),
    ("出错了", "Something went wrong"),
    ("函数", "Function"),
    ("分享页优先", "Share page first"),
    ("分屏", "Split"),
    (
        "分屏 ⌘D（左右各一条独立会话）",
        "Split ⌘D (an independent session on each side)",
    ),
    ("分屏（仅限终端会话）", "Split (terminal sessions only)"),
    ("分布在 {} 个分类", "Across {} categories"),
    ("分数", "Score"),
    ("分数必须是数字", "Score must be a number"),
    ("分数，如 1 或 1.5", "Score, e.g. 1 or 1.5"),
    ("分类", "Categories"),
    ("分组", "Group"),
    ("分组名", "Group name"),
    ("分钟", "minutes"),
    ("分隔标题", "Section title"),
    ("切换侧栏", "Toggle sidebar"),
    ("切换分屏", "Toggle split pane"),
    ("切换命名空间失败：{}", "Failed to switch namespace: {}"),
    ("切换外观", "Toggle appearance"),
    ("切换数据库失败：{}", "Failed to switch database: {}"),
    (
        "切换模板会把端点与模型重填成该模板的默认值。",
        "Switching the template refills the endpoint and model with that template's defaults.",
    ),
    ("列", "Columns"),
    (
        "列出全部已注册工具与其派生索引",
        "Lists every registered tool with its derived index",
    ),
    ("列出目录内容", "List directory contents"),
    ("列名", "Column"),
    ("列数", "Column count"),
    ("列模式", "Column view"),
    ("列统计", "Column stats"),
    ("列表", "List"),
    (
        "列表、确认与开机启动",
        "Lists, confirmations, and launch at login",
    ),
    ("列表与确认交互", "Lists & confirmations"),
    ("列表使用紧凑行高", "Use compact list rows"),
    ("列表首屏条数", "Messages per load"),
    ("刚刚", "Just now"),
    ("创建", "Create"),
    (
        "创建 PTY 失败。关闭该标签后重开一次试试。",
        "Failed to create the PTY. Close this tab and open a new one.",
    ),
    (
        "创建一条会话标签并连接到该主机。",
        "Create a session tab and connect to this host.",
    ),
    ("创建标签失败：{}", "Failed to create label: {}"),
    ("创建连接", "Create connection"),
    (
        "删掉全部流水。清空本身也会记一笔 —— 否则「谁把痕迹擦了」就查不出来了。",
        "Deletes every entry. Clearing is itself recorded, otherwise there would be no way to tell who wiped the trail.",
    ),
    ("删除", "Delete"),
    ("删除 {} · {} 行", "Deleted {} · {} rows"),
    ("删除「{}」？", "Delete “{}”?"),
    ("删除仪表板", "Delete dashboard"),
    ("删除任务", "Delete task"),
    ("删除任务？", "Delete this task?"),
    ("删除会话", "Delete conversation"),
    ("删除传输档案", "Delete transport profile"),
    ("删除列表元素", "Delete list element"),
    (
        "删除后引用它的 {} 条连接会回到「自定义」，确定继续吗？",
        "Deleting it resets {} referencing connection(s) back to Custom. Continue?",
    ),
    ("删除哈希字段", "Delete hash field"),
    ("删除失败：{}", "Delete failed: {}"),
    ("删除工具前弹确认", "Confirm before deleting a tool"),
    ("删除已保存图表", "Delete saved chart"),
    ("删除已保存查询", "Delete Saved Query"),
    ("删除当前行", "Delete current row"),
    ("删除有序集合成员", "Delete sorted set member"),
    ("删除此元素", "Delete this element"),
    ("删除此成员", "Delete this member"),
    ("删除此连接", "Delete this connection"),
    ("删除此键", "Delete this key"),
    ("删除此项", "Delete this item"),
    ("删除草稿", "Delete draft"),
    ("删除草稿失败：{}", "Could not delete the draft: {}"),
    ("删除行", "Delete row"),
    ("删除这一条转发", "Remove this forward"),
    ("删除这份档案", "Delete this profile"),
    ("删除这条通知", "Delete this notification"),
    ("删除连接", "Delete connection"),
    ("删除连接「{}」？", "Delete connection \"{}\"?"),
    ("删除选中行", "Delete selected rows"),
    ("删除钩子", "Remove hook"),
    ("删除键", "Delete key"),
    ("删除集合成员", "Delete set member"),
    ("删除（M2 接入）", "Delete (coming in M2)"),
    (
        "别的文件夹来新信时推一条应用内提示与系统通知。",
        "Show an in-app banner and a system notification when mail arrives in another folder.",
    ),
    (
        "到「设置 → AI」里添加一个厂商并填入 API key，回来就能开始对话。",
        "Add a provider and its API key under Settings → AI, then come back to start chatting.",
    ),
    ("刷新", "Refresh"),
    ("刷新两侧", "Refresh both"),
    ("刷新失败：{}", "Refresh failed: {}"),
    ("刷新当前页", "Refresh page"),
    (
        "刷新收件箱（同步引擎待接入）",
        "Refresh inbox (sync engine not wired up yet)",
    ),
    ("前挪", "Move earlier"),
    (
        "前缀匹配，汉字与字母数字都行",
        "Prefix match; works on CJK, letters, and digits",
    ),
    ("剩余 {}", "{} free"),
    ("剪切", "Cut"),
    (
        "剪贴板内容未识别到可处理的工具",
        "No tool target recognized in the clipboard",
    ),
    ("副本", "copy"),
    ("功能正在建设中", "Coming soon"),
    ("加入成员", "Add member"),
    ("加入暂存", "Stage"),
    (
        "加密存在本机，设置文件里不留明文。",
        "Encrypted on this machine; no plaintext is left in the settings file.",
    ),
    (
        "加密存本机，不入连接档案",
        "Encrypted on this machine, not in the connection archive",
    ),
    ("加密方式", "Encryption"),
    ("加载失败：{}", "Load failed: {}"),
    ("加载更多", "Load more"),
    ("加载更多失败：{}", "Failed to load more: {}"),
    ("动态 SOCKS", "Dynamic SOCKS"),
    ("助手工具", "Assistant tools"),
    ("勾选全部行", "Select all rows"),
    ("勾选本页全部行", "Select all rows on this page"),
    (
        "勾选要放行的能力；取消勾选即拒绝，之后可在插件中心逐项调整。",
        "Tick the capabilities to allow; unticked ones are denied, and you can adjust them item by item in the Plugin Center later.",
    ),
    ("包体积", "Package size"),
    ("包含子目录", "Include subdirectories"),
    ("区域", "Region"),
    ("升序", "ascending"),
    ("升级", "Upgrade"),
    ("单元格", "Cell"),
    ("单元格详情", "Cell details"),
    ("单文件上限", "Max file size"),
    (
        "单条查询最多返回多少行，超出即截断。",
        "Maximum rows a single query returns; anything beyond is truncated.",
    ),
    ("卸载", "Uninstall"),
    (
        "卸载插件「{}」失败：{}",
        "Failed to uninstall plugin \"{}\": {}",
    ),
    ("卸载插件前弹确认", "Ask before uninstalling a plugin"),
    ("卸载插件？", "Uninstall plugin?"),
    ("厂商", "Provider"),
    (
        "厂商、密钥、工具开关与知识库来源都在「设置 → AI」里调整。",
        "Providers, keys, the tool switch and knowledge base sources are all under Settings → AI.",
    ),
    (
        "厂商、密钥与生成参数在「设置 → AI」里调整。",
        "Providers, keys and generation parameters are configured under Settings → AI.",
    ),
    (
        "厂商「{}」还没配置 API key，请到「AI 助手」页配置。",
        "Provider {} has no API key yet. Configure it on the AI Assistant page.",
    ),
    ("厂商已保存", "Provider saved"),
    ("厂商已删除", "Provider deleted"),
    ("历史", "History"),
    ("压缩", "compress"),
    ("压缩 SQL", "Compress SQL"),
    ("压缩包", "archive"),
    ("压缩包解包", "Archive extraction"),
    ("压缩归档", "Archives"),
    (
        "原始字节导出要求结果只有一列且全是二进制值。",
        "Raw byte export needs exactly one column of binary values.",
    ),
    ("原始输出", "Raw output"),
    ("原样重放", "Replay as-is"),
    ("去申请密钥", "Get an API key"),
    ("去设置", "Open settings"),
    ("参数名", "Parameter"),
    (
        "双击目录进入 · 单击选中文件",
        "Double-click a folder to enter · click to select",
    ),
    (
        "双引号里当一个短语，不拆词",
        "A phrase in double quotes, not split into words",
    ),
    ("发件人", "From"),
    (
        "发件人地址或显示名，子串匹配；中文「发件人:」同义，冒号全角半角都认",
        "Sender address or display name, substring match; the Chinese alias works too, with either ASCII or fullwidth colon",
    ),
    ("发现新版本 {}。", "Version {} is available."),
    ("发送", "Send"),
    ("发送中…", "Sending…"),
    ("发送失败：{}", "Send failed: {}"),
    ("取消", "Cancel"),
    ("取消中", "Cancelling"),
    ("取消修改", "Cancel edit"),
    ("取消分屏", "Remove split"),
    ("取消旗标", "Unflag"),
    ("取消编辑", "Cancel editing"),
    (
        "取自 workspace 版本号，与打包产物一致。",
        "Taken from the workspace version, matching the packaged build.",
    ),
    (
        "取自账户档案，不在这里更改。",
        "Taken from the account profile; not editable here.",
    ),
    ("受阻", "Blocked"),
    ("变宽", "Wider"),
    ("变窄", "Narrower"),
    ("口令", "Password"),
    ("口令 / 应用专用密码", "Password / app password"),
    (
        "口令 / 应用专用密码（留空保持不变）",
        "Password / app password (leave empty to keep)",
    ),
    (
        "口令会以明文发给服务器，任何人都能在链路上读到；只用于本机测试服务器。",
        "The password is sent to the server in cleartext and anyone on the path can read it; use this only for a local test server.",
    ),
    ("口令保存失败：{}", "Failed to store password: {}"),
    (
        "口令加密后保存在本机数据库，编辑时回显；留空则连接时在终端里输入。",
        "The password is stored encrypted in the local database and shown back when editing; leave empty to enter it in the terminal when connecting.",
    ),
    ("口令已保存", "Password saved"),
    ("口令每次输入", "Password asked each time"),
    (
        "口令（加密存本机，不入库）",
        "Password (encrypted on this machine, not stored in the archive)",
    ),
    ("另存为…", "Save as…"),
    ("另有 {} 行", "{} more rows"),
    (
        "只作用于读路径（执行 SQL、读表、AI 只读执行）；留空用全局默认 30 秒。",
        "Applies to read paths only (running SQL, reading tables, AI read-only runs); blank uses the global 30s default.",
    ),
    (
        "只允许打开 http、https 与 mailto 链接。",
        "Only http, https and mailto links can be opened.",
    ),
    ("只分析选中的语句", "Explain only the selected statements"),
    ("只执行选中的语句", "Run only the selected statements"),
    (
        "只按下了修饰键，还要再按一个普通键。",
        "Only modifiers were pressed; add a regular key as well.",
    ),
    (
        "只改名字，不移动位置。",
        "Renames in place; the item stays in its folder.",
    ),
    (
        "只显示文件末尾，更早的内容没有载入。",
        "Showing the end of the file only; earlier content was not loaded.",
    ),
    (
        "只有字符串值可直接编辑",
        "Only string values are directly editable",
    ),
    (
        "只有需要认证的代理才要填用户名与口令。",
        "Only proxies that require authentication need a user name and password.",
    ),
    (
        "只看带附件的（中文值 has:附件 同义）",
        "With attachments only (the Chinese value works too)",
    ),
    (
        "只看有旗标的（is:unstarred 看没旗标的；中文值 is:有旗标 同义）",
        "Flagged only (is:unstarred for unflagged; the Chinese value works too)",
    ),
    (
        "只看未读（is:read 看已读；中文值 is:未读 / is:已读 同义）",
        "Unread only (is:read for read; the Chinese values work too)",
    ),
    (
        "只看这一次错误。同一个 id 也会出现在那条失败提示里。",
        "Show only this failure. The same id appears in the error message.",
    ),
    ("只看问题", "Problems only"),
    ("只读", "Read-only"),
    ("只读 SQL", "Read-only SQL"),
    ("只读文件", "Read files"),
    (
        "只读档由服务器兜底拒绝写语句（不是界面软拦）；切到只读不需要重连。",
        "In read-only mode the server rejects writes; it is not a UI-level block, and switching to read-only needs no reconnect.",
    ),
    ("可写", "Writable"),
    ("可更新", "Update available"),
    (
        "可用占位符：{name} 原名、{index} 序号、{ext} 扩展名。",
        "Placeholders: {name} original name, {index} serial number, {ext} extension.",
    ),
    ("可留空", "Optional"),
    (
        "可留空。留空即不选库，连上后在命名空间下拉里选一个。",
        "Optional. When blank no database is selected; pick one in the namespace dropdown after connecting.",
    ),
    (
        "可留空。留空即不选库：连上后在命名空间下拉里选一个，或把表名写成 db.table。",
        "Optional. When blank no database is selected: pick one in the namespace dropdown after connecting, or qualify table names as db.table.",
    ),
    (
        "可留空。留空时 PostgreSQL 连到用户名同名的库。",
        "Optional. When blank, PostgreSQL connects to a database named after the user.",
    ),
    (
        "可留空。留空用惯例库 default。",
        "Optional. When blank the conventional database default is used.",
    ),
    (
        "可留空。留空落入登录名的默认库（通常 master）。",
        "Optional. When blank you land on the login's default database (usually master).",
    ),
    (
        "可留空。这里填默认索引或别名（支持 logs-* 这类通配），留空即全部索引。",
        "May be left empty. Fill in a default index or alias (wildcards such as logs-* work); empty means every index.",
    ),
    ("右侧", "Right"),
    ("右推入", "Push right"),
    ("右栏位置", "Right sidebar side"),
    ("同意并安装", "Agree and install"),
    ("同意并更新", "Agree and update"),
    (
        "同时启用多档时，只有优先级最高的一档生效：SSM → SSH → 代理。",
        "When several transports are enabled only the highest-priority one applies: SSM → SSH → proxy.",
    ),
    (
        "同时派生选中导航图标色与链接文字色。",
        "Also derives the selected nav icon color and link text color.",
    ),
    ("同步中…", "Syncing…"),
    ("同步失败", "Sync failed"),
    ("同步失败：{}", "Sync failed: {}"),
    (
        "同步完成：{} 个文件夹，新增 {} 封",
        "Sync complete: {} folders, {} new messages",
    ),
    ("同步当前账户", "Sync current account"),
    ("同步操作失败：{}", "Syncing actions failed: {}"),
    ("名称", "Name"),
    ("名称 *", "Name *"),
    ("名称不能为空。", "The name cannot be empty."),
    ("名称不能包含 / 。", "The name cannot contain /."),
    ("后挪", "Move later"),
    (
        "向上翻页时保留的历史行数；对新建会话生效。",
        "History kept when scrolling up; applies to new sessions.",
    ),
    ("否", "No"),
    ("含 {} 个任务", "{} tasks"),
    ("含 {} 个子任务", "{} subtasks"),
    ("启动", "Startup"),
    ("启动后自动检查更新", "Check for updates on launch"),
    ("启动时恢复上次窗口", "Reopen last session's windows"),
    ("启用", "Enabled"),
    ("启用 SSH 隧道", "Enable SSH tunnel"),
    ("启用唤起快捷键", "Enable the summon shortcut"),
    ("启用插件", "Enabled"),
    ("命中 {} 行", "{} matching rows"),
    ("命令", "Commands"),
    ("命令名", "Command"),
    ("命令片段", "Command snippets"),
    ("命令面板", "Command palette"),
    (
        "命令面板：用键盘完成导航、切换与常用动作。",
        "Command palette: navigate, switch and run common actions from the keyboard.",
    ),
    ("命名模板", "Naming template"),
    ("命名空间", "Namespace"),
    ("命名空间分隔符", "Namespace separator"),
    ("命名空间分隔符，默认 :", "Namespace separator, default :"),
    ("哈希", "Hash"),
    ("唤起", "Summon"),
    ("唤起时打开主窗口", "Open the main window when summoned"),
    ("唯一", "Unique"),
    ("嗅探引擎", "Sniffing engine"),
    ("四象限…", "Quadrant…"),
    ("四象限定位", "Quadrant position"),
    ("四象限已设为 {}", "Quadrant set to {}"),
    ("回信", "Reply"),
    ("回到最新", "Jump to latest"),
    ("回复", "Reply"),
    ("回复建议", "Draft reply"),
    ("回复（M2 接入）", "Reply (coming in M2)"),
    ("回复：", "Re: "),
    ("回放", "replay"),
    ("回滚", "Rollback"),
    ("回滚 4096 行", "Scrollback 4096"),
    ("回滚 {} 行", "Scrollback {} lines"),
    ("回滚事务", "Rollback transaction"),
    ("回滚事务（ROLLBACK）", "Rollback transaction (ROLLBACK)"),
    ("回滚行数", "Scrollback lines"),
    ("回车打开选中连接", "Enter opens the selected connection"),
    ("回退到上一版本", "Roll back to the previous version"),
    ("回退失败：{}", "Rollback failed: {}"),
    ("回退已就绪", "Rollback ready"),
    ("图标路径必须是合法 UTF-8", "icon path must be valid UTF-8"),
    ("图标集", "Icon set"),
    ("图片", "image"),
    ("图片格式转换", "Image conversion"),
    ("图表", "Chart"),
    ("图表名称", "Chart name"),
    ("图表类型", "Chart type"),
    (
        "图表至少要有一列数值；切回表格看原始结果。",
        "A chart needs at least one numeric column; switch back to the table for the raw result.",
    ),
    ("图表面板", "Chart panel"),
    ("在 {}，{} 写道：", "On {}, {} wrote:"),
    ("在「{}」新建任务", "New task in \"{}\""),
    (
        "在上一档的基础上，再删除已安装的插件与插件数据。",
        "Does the above, then removes installed plugins and their data.",
    ),
    (
        "在下面输入问题，回车发送。回答支持 Markdown 渲染。",
        "Type your question below and press Enter to send. Answers are rendered as Markdown.",
    ),
    ("在分屏中打开", "Open in split pane"),
    (
        "在别的程序里也能用；Duck 已经在前台时再按一次会把它收起来。",
        "Works from other apps too; press it again while Duck is in front to put it away.",
    ),
    (
        "在右上角的输入框里写个标题、按回车，就能建第一条任务。",
        "Type a title in the box at the top right and press Enter to create your first task.",
    ),
    ("在右栏查看详情", "Show details in the right panel"),
    (
        "在左栏选择一条连接，或新建一条。",
        "Select a connection in the sidebar, or create a new one.",
    ),
    (
        "在建连 / 断开的前后跑一条本机命令或脚本；失败默认只提示、不阻断。",
        "Run a local command or script before/after connecting or disconnecting; failures only warn by default.",
    ),
    (
        "在当前远端目录下新建一个目录。",
        "Create a folder inside the current remote directory.",
    ),
    ("在本地知识库里检索", "Search the local knowledge base"),
    ("在结果中搜索…", "Search within results…"),
    (
        "在结果网格里点一格，或点行尾的 ⓘ。",
        "Click a cell in the grid, or the ⓘ at the row end.",
    ),
    ("在缓冲区中查找…", "Search the buffer…"),
    ("在编辑器中修改", "Edit in editor"),
    ("在编辑器中修改 DDL", "Edit DDL in editor"),
    ("在设置中打开", "Open in Settings"),
    ("在访达中显示", "Reveal in Finder"),
    ("垃圾邮件", "Junk"),
    ("增大字号", "Larger font"),
    ("增大终端字号", "Increase terminal font size"),
    ("处理中…", "Working…"),
    ("备份", "Backup"),
    ("备份、恢复与重置", "Backup, restore and reset"),
    ("备份中包含日志", "Include logs"),
    ("复制", "Copy"),
    ("复制 SQL", "Copy SQL"),
    (
        "复制 ⌘C（拖选文字后）· 粘贴 ⌘V · 全选 ⌘A",
        "Copy ⌘C (after selecting) · Paste ⌘V · Select all ⌘A",
    ),
    ("复制为 Base64 转储", "Copy as Base64 dump"),
    ("复制为 CSV", "Copy as CSV"),
    ("复制为 INSERT 语句", "Copy as INSERT statements"),
    ("复制为 JSON", "Copy as JSON"),
    ("复制为 JSON（美化）", "Copy as pretty JSON"),
    ("复制为 Markdown 表格", "Copy as Markdown table"),
    ("复制为 TSV（含表头）", "Copy as TSV (with header)"),
    ("复制为十六进制转储", "Copy as hex dump"),
    ("复制为纯文本（对齐）", "Copy as plain text (aligned)"),
    ("复制值", "Copy value"),
    ("复制全部", "Copy all"),
    ("复制列名", "Copy column name"),
    ("复制单元格", "Copy cell"),
    ("复制名称", "Copy name"),
    ("复制外键定义", "Copy foreign-key definition"),
    ("复制对象名", "Copy object name"),
    ("复制所选 {} 行", "Copy {} selected rows"),
    ("复制整行", "Copy row"),
    ("复制索引列", "Copy index columns"),
    ("复制索引名", "Copy index name"),
    ("复制触发器名", "Copy trigger name"),
    ("复制诊断信息", "Copy diagnostics"),
    ("复制诊断信息失败", "Failed to copy diagnostics"),
    ("复制选中行", "Copy selected row"),
    (
        "复用已登录会话，自动识别分享链接",
        "Reuses signed-in sessions to detect share links",
    ),
    ("复用登录会话", "Reuse signed-in session"),
    ("外观", "Appearance"),
    ("外键", "Foreign keys"),
    (
        "多个地址用逗号或分号分隔。",
        "Separate multiple addresses with commas or semicolons.",
    ),
    (
        "多个条件之间、条件与关键词之间都是「同时满足」。",
        "Conditions — and conditions with keywords — must all match.",
    ),
    ("大写", "Uppercase"),
    ("大小", "Size"),
    ("大小：{}", "Size: {}"),
    ("失败", "Failed"),
    ("失败则中断本次操作", "On failure, abort this operation"),
    ("失败只提示（继续）", "On failure, warn and continue"),
    ("失败时", "On failure"),
    ("失败（被信号终止）", "Failed (killed by a signal)"),
    ("失败（退出码 {}）", "Failed (exit code {})"),
    (
        "如果是第一次，点列表条右侧的同步按钮从服务器拉取。",
        "If this is the first time, use the sync button on the right of the list bar to fetch from the server.",
    ),
    ("媒体", "media"),
    ("媒体处理", "Media"),
    ("子任务", "Subtasks"),
    (
        "子树键数不超过它就默认展开；留空或 0 表示不自动展开",
        "Subtrees with no more keys than this expand by default; leave blank or 0 to never auto-expand",
    ),
    ("字体", "Font"),
    ("字体大小", "Font size"),
    ("字体族", "Font family"),
    ("字号 ⌘+ / ⌘- / ⌘0", "Font size ⌘+ / ⌘- / ⌘0"),
    (
        "字号、回滚与会话行为",
        "Font size, scrollback and session behaviour",
    ),
    ("字段", "Field"),
    ("字段值", "Field value"),
    ("字段名", "Field name"),
    ("字段名不能为空", "Field name must not be empty"),
    ("字段已写入", "Field written"),
    ("字段已删除", "Field deleted"),
    ("字段筛选", "Filter columns"),
    ("字符串", "String"),
    ("字符串值", "String value"),
    ("存为新档案", "Save as new profile"),
    ("存储过程", "Stored procedures"),
    ("存草稿", "Save draft"),
    (
        "存进应用自己的凭据文件（仅本用户可读），不进数据库，也不走系统钥匙串。",
        "Stored in the app's own credential file (readable only by you); never in the database, and never in the system keychain.",
    ),
    ("安全", "Safety"),
    ("安全模式（锁写）", "Safe mode (writes locked)"),
    ("安装", "Install"),
    ("安装中…", "Installing…"),
    (
        "安装后，本插件会请求访问以下资源：",
        "Once installed, this plugin requests access to:",
    ),
    ("安装插件「{}」", "Install plugin \"{}\""),
    (
        "安装插件「{}」失败：{}",
        "Failed to install plugin \"{}\": {}",
    ),
    ("安装目录", "Install location"),
    ("完成", "Done"),
    ("完成 {}", "Done {}"),
    (
        "完成后保留分片文件，便于排查合并问题。",
        "Keeps segment files after completion, which helps when debugging a merge.",
    ),
    ("完成编辑", "Done editing"),
    ("完整主题", "Full theme"),
    (
        "实例 id 或实例名（i-… / tag:Name=…）",
        "Instance id or name (i-… / tag:Name=…)",
    ),
    ("实例指标", "Instance Metrics"),
    ("实例检查器", "Instance Inspector"),
    ("实例概览", "Instance Overview"),
    ("实时监控", "Live monitor"),
    ("实时监控已停止", "Live monitoring stopped"),
    ("实时监控已开启", "Live monitoring started"),
    ("实际工时", "Actual hours"),
    ("审计日志", "Audit log"),
    ("审计日志已清空", "Audit log cleared"),
    ("客户端", "Clients"),
    (
        "客户端证书 / 根证书",
        "Client certificate / root certificate",
    ),
    ("宽松", "Comfortable"),
    ("密码", "Password"),
    ("密钥已加密保存", "Key saved encrypted"),
    ("密钥来源", "Key source"),
    ("对象", "Objects"),
    ("对象构成", "Object Breakdown"),
    ("导入", "Import"),
    ("导入 CSV", "Import CSV"),
    (
        "导入会用包里的值覆盖同名设置，本机独有的设置保留。导入前的数据会自动备份。继续？",
        "Settings present in the backup overwrite the local ones; local-only settings are kept. Your current data is backed up first. Continue?",
    ),
    (
        "导入前会自动把当前数据备份到上面的目录。",
        "Your current data is backed up to the folder above before importing.",
    ),
    ("导入失败：{}", "Import failed: {}"),
    (
        "导入将完全以备份包为准，包里没有的设置会被清掉。导入前的数据会自动备份。继续？",
        "The backup becomes the source of truth; settings missing from it are cleared. Your current data is backed up first. Continue?",
    ),
    ("导入时完全覆盖", "Replace everything on import"),
    ("导入连接", "Import connections"),
    ("导入配置", "Import config"),
    (
        "导入配置尚未接入业务逻辑",
        "Importing a config is not wired up yet",
    ),
    ("导出", "Export"),
    ("导出 Base64 转储", "Export Base64 dump"),
    ("导出 CSV", "Export CSV"),
    ("导出 INSERT 语句", "Export INSERT statements"),
    ("导出 JSON", "Export JSON"),
    ("导出 JSON（美化）", "Export pretty JSON"),
    ("导出 Markdown", "Export Markdown"),
    ("导出 TSV", "Export TSV"),
    ("导出…", "Export…"),
    ("导出十六进制转储", "Export hex dump"),
    ("导出原始字节", "Export raw bytes"),
    ("导出备份", "Export a backup"),
    ("导出失败：{}", "Export failed: {}"),
    ("导出完成", "Export complete"),
    ("导出纯文本", "Export plain text"),
    ("导出结果", "Export results"),
    ("导出连接", "Export connections"),
    ("导航", "Navigation"),
    ("封", "messages"),
    (
        "将从哈希 {} 中删除字段 {}。",
        "Delete field {} from hash {}.",
    ),
    (
        "将从数据库中删除 {} 行，确定继续吗？",
        "{} row(s) will be deleted from the database. Continue?",
    ),
    (
        "将从有序集合 {} 中删除成员 {}。",
        "Delete member {} from sorted set {}.",
    ),
    (
        "将从集合 {} 中删除成员 {}。",
        "Delete member {} from set {}.",
    ),
    (
        "将删除列表 {} 中第 {} 个元素。",
        "Delete element {} at index {} from list {}.",
    ),
    (
        "将向服务端发送命令：{}",
        "About to send this command to the server: {}",
    ),
    (
        "将强制断开客户端 {} 的连接。",
        "About to forcibly disconnect client {}.",
    ),
    (
        "将把 {} 重命名为 {}；若目标键已存在，它会被覆盖。",
        "Rename {} to {}; if the target key already exists it will be overwritten.",
    ),
    (
        "将把服务端配置 {} 改为 {}；多数参数立即生效并影响所有客户端。",
        "About to change server configuration {} to {}; most parameters take effect immediately and affect every client.",
    ),
    (
        "将插件放入 {} 后重启应用。",
        "Drop plugins into {} and restart the app.",
    ),
    (
        "将永久删除键 {}，此操作不可撤销。",
        "This permanently deletes key {}; it cannot be undone.",
    ),
    (
        "将清空服务端的慢日志记录。",
        "About to clear the server slow log.",
    ),
    (
        "将移除 {} 的过期时间，该键会永不过期。",
        "Remove the expiry of {}; the key will never expire.",
    ),
    (
        "将移除 {}（含本地缓存的邮件与凭据）。这个动作无法撤销。",
        "This will remove {} along with its cached messages and credentials. This cannot be undone.",
    ),
    ("小写", "Lowercase"),
    ("小写关键字", "Lowercase keywords"),
    ("尚未加载", "Not loaded yet"),
    (
        "尚未建立 SFTP 会话，无法浏览远端目录。",
        "No SFTP session yet; cannot browse the remote directory.",
    ),
    ("尚未添加邮箱账户", "No mailbox account yet"),
    (
        "尚未获取插件清单，点「刷新」从市场获取。",
        "No plugin list yet; use Refresh to fetch it from the marketplace.",
    ),
    ("尚未连接。", "Not connected yet."),
    ("尚未连接数据库", "No database connected"),
    ("尚未配置", "Not configured"),
    (
        "尚未配置 AI 厂商，请到「AI 助手」页添加厂商档案与 API key。",
        "No AI provider configured. Add a provider profile and API key on the AI Assistant page.",
    ),
    ("尚未配置密钥", "No key configured yet"),
    ("尚未配置更新源。", "No update source is configured yet."),
    ("就绪", "Ready"),
    ("展开", "Expand"),
    ("展开「{}」这一段", "Expand “{}”"),
    ("展开侧栏", "Expand sidebar"),
    ("展开右侧栏", "Expand right sidebar"),
    (
        "展示用，留空回落模板名。",
        "Display only; blank falls back to the template name.",
    ),
    ("工作", "Work"),
    ("工作区", "Workspace"),
    ("工作目录", "Workspace"),
    ("工作目录：{}", "Workspace: {}"),
    ("工作邮箱", "Work"),
    ("工具", "Tool"),
    ("工具不可用", "Tool unavailable"),
    ("工具信息", "Tool info"),
    ("工具启停与工具级设置", "Tool toggles and per-tool settings"),
    ("工具未开启", "Tools are off"),
    ("工具箱", "Toolbox"),
    (
        "工具箱页侧栏条目之间的留白（0 为贴紧）。",
        "Spacing between toolbox sidebar entries; 0 packs them tight.",
    ),
    (
        "工具箱：把常用小工具收进同一个窗口，启停与设置都在「设置 → 工具」。",
        "Toolbox: keep your everyday utilities in one window; enable and configure them in Settings → Tools.",
    ),
    ("工具能力", "Tool capabilities"),
    (
        "工具设置项在设置窗口中统一调整，改动即时生效。",
        "Tool settings live in the Settings window; changes apply immediately.",
    ),
    (
        "工具调用与知识库检索将在下一个增量落地，当前仅对话与流式输出。",
        "Tool calling and knowledge-base search land in the next increment; for now this is chat and streaming output only.",
    ),
    ("工具贡献", "Tool contributions"),
    ("左侧", "Left"),
    ("左推入", "Push left"),
    ("差异快照（仅设置）", "Diff snapshot (settings only)"),
    (
        "已下载并校验，安装包在 {}。",
        "Downloaded and verified; the installer is at {}.",
    ),
    ("已丢弃暂存改动", "Discarded pending changes"),
    (
        "已丢弃暂存改动（⌘Z 可撤销）",
        "Discarded pending changes (⌘Z to undo)",
    ),
    ("已为 {} 设置过期时间", "Expiry set to {}"),
    (
        "已从历史放回编辑器。",
        "Restored from history into the editor.",
    ),
    ("已保存", "Saved"),
    ("已保存 {} 处改动。", "Saved {} change(s)."),
    ("已保存仪表板。", "Dashboard saved."),
    ("已保存分隔标题。", "Section title saved."),
    ("已保存到 {}", "Saved to {}"),
    ("已保存图表", "Saved Charts"),
    ("已保存图表。", "Chart saved."),
    ("已保存查询", "Saved Queries"),
    ("已保存查询。", "Query saved."),
    ("已保存键 {}", "Saved key {}"),
    (
        "已保存（清空则删除密钥）",
        "Saved (clear to delete the key)",
    ),
    ("已保存，留空表示不改动", "Saved; leave blank to keep it"),
    ("已修改 {} 处。", "Modified {} cell(s)."),
    ("已修改配置 {}", "Configuration {} updated"),
    ("已停止", "Stopped"),
    ("已停止。", "Stopped."),
    ("已停止自动重连。", "Automatic reconnect stopped."),
    ("已停用", "Disabled"),
    ("已克隆连接「{}」。", "Connection “{}” cloned."),
    ("已关闭。", "Turned off."),
    ("已切到数据库 {}", "Switched to database {}"),
    ("已切换命名空间。", "Namespace switched."),
    ("已切换模型", "Model switched"),
    ("已创建目录 {}。", "Created folder {}."),
    (
        "已创建示例工具「未命名」",
        "Created the sample tool “Untitled”",
    ),
    ("已创建连接「{}」。", "Connection “{}” created."),
    ("已删除", "Trash"),
    ("已删除 {} 个过期日志。", "Deleted {} expired log files."),
    ("已删除 {} 行", "Deleted {} rows"),
    ("已删除 {} 行。", "Deleted {} row(s)."),
    ("已删除 {}。", "Deleted {}."),
    ("已删除。", "Deleted."),
    ("已删除任务。", "Task deleted."),
    ("已删除的图表", "Deleted chart"),
    ("已删除该仪表板。", "Dashboard deleted."),
    ("已删除该图表。", "Chart deleted."),
    ("已删除该查询。", "Query deleted."),
    ("已删除键 {}", "Deleted key {}"),
    ("已刷新。", "Refreshed."),
    ("已刷新仪表板「{}」。", "Dashboard “{}” refreshed."),
    (
        "已剪切，原格已暂存为 NULL。",
        "Cut; the cell is staged as NULL.",
    ),
    ("已加密保存在本机", "Saved encrypted on this machine"),
    ("已加旗标", "Flagged"),
    ("已加载 {} / {} 个键", "Loaded {} / {} keys"),
    (
        "已加载 {} 个键，暂停追加；请用过滤串收窄范围",
        "Loaded {} keys; stopped appending — narrow it down with a filter pattern",
    ),
    ("已加载 {} 行（已到末页）", "{} rows loaded (last page)"),
    ("已加载 {} 行，可继续加载", "{} rows loaded; more available"),
    (
        "已加载 {} 行；结果被行上限截断",
        "{} rows loaded; truncated by the row limit",
    ),
    ("已压缩。", "Minified."),
    (
        "已压缩较早上下文（省略 {} 条消息 / 裁剪 {} 条工具结果）",
        "Compacted earlier context ({} messages omitted, {} tool results pruned)",
    ),
    ("已压缩（没有变化）。", "Nothing to minify."),
    ("已发送", "Sent"),
    ("已取消", "Cancelled"),
    ("已取消 {} 行的注释。", "Uncommented {} lines."),
    ("已取消旗标", "Unflagged"),
    ("已取消更新。", "Update cancelled."),
    ("已同步", "Synced"),
    ("已同步 {} 个操作", "Synced {} actions"),
    (
        "已向数据库发送取消请求。",
        "Cancellation request sent to the database.",
    ),
    ("已启用", "Enabled"),
    (
        "已回退到上一版本，重启后生效。",
        "Rolled back to the previous version; a restart applies it.",
    ),
    ("已复制。", "Copied."),
    ("已复制仪表板。", "Dashboard duplicated."),
    ("已复制全部结果。", "All results copied."),
    ("已复制列名。", "Column name copied."),
    ("已复制单元格。", "Cell copied."),
    ("已复制名称。", "Name copied."),
    ("已复制外键定义。", "Foreign-key definition copied."),
    ("已复制对象名。", "Object name copied."),
    ("已复制所选 {} 行。", "{} selected rows copied."),
    ("已复制触发器名。", "Trigger name copied."),
    ("已复制诊断信息", "Diagnostics copied"),
    ("已复制该 SQL。", "Copied the SQL."),
    ("已复制该行。", "Row copied."),
    (
        "已存过密钥。留空表示不改动，填入则覆盖。",
        "A key is already stored. Leave blank to keep it, or type a new one to replace it.",
    ),
    ("已安装", "Installed"),
    ("已安装工具", "Installed tools"),
    ("已安装插件", "Installed plugins"),
    ("已完成", "Completed"),
    ("已定位到列「{}」。", "Jumped to column “{}”."),
    (
        "已导入 {} 条连接，跳过 {} 条。",
        "Imported {} connection(s), skipped {}.",
    ),
    ("已导入 {} 行。", "Imported {} rows."),
    (
        "已导入 {} 项设置与 AI 会话，重启应用后完全生效。",
        "Imported {} settings and AI conversations; restart the app to apply everything.",
    ),
    (
        "已导入 {} 项设置，重启应用后完全生效。",
        "Imported {} settings; restart the app to apply everything.",
    ),
    (
        "已导入 {} 项设置，重启应用后完全生效（包里没有 AI 会话）。",
        "Imported {} settings; restart the app to apply everything (the archive had no AI conversations).",
    ),
    ("已导出 Base64 转储。", "Exported Base64 dump."),
    ("已导出 CSV。", "CSV exported."),
    ("已导出 INSERT 语句。", "INSERT statements exported."),
    ("已导出 JSON。", "JSON exported."),
    ("已导出 Markdown。", "Markdown exported."),
    ("已导出 TSV。", "TSV exported."),
    ("已导出到 {}（{} 行）", "Exported to {} ({} lines)"),
    ("已导出到 {}（{}）", "Exported to {} ({})"),
    ("已导出十六进制转储。", "Exported hex dump."),
    ("已导出原始字节。", "Exported raw bytes."),
    ("已导出纯文本。", "Exported plain text."),
    ("已导出美化 JSON。", "Exported pretty JSON."),
    ("已导出连接档案。", "Connection profiles exported."),
    ("已开始传输 {} 个条目。", "Started transferring {} items."),
    ("已开始传输 {}。", "Started transferring {}."),
    ("已归档 {}", "Archived {}"),
    (
        "已归档任务，可在归档中心找回。",
        "Task archived; find it in the archive.",
    ),
    (
        "已归档项目，可在归档中心找回。",
        "Project archived; find it in the archive.",
    ),
    (
        "已归档项目，连带 {} 个任务。",
        "Project archived, along with {} tasks.",
    ),
    ("已彻底删除。", "Permanently deleted."),
    ("已恢复。", "Restored."),
    (
        "已打开；助手可以读取工作目录里的文件、列出目录、检索本地知识库。",
        "On: the assistant may read files in the workspace, list directories and search the local knowledge base.",
    ),
    (
        "已打开；还没选工作目录，所以只有知识库检索可用。",
        "On, but no workspace is chosen yet, so only knowledge base search is available.",
    ),
    ("已批量修改 {} 行", "Batch-updated {} rows"),
    (
        "已把引用骨架放进编辑器。",
        "Inserted a reference skeleton into the editor.",
    ),
    (
        "已把当前 DDL 放进编辑器，改好后执行。",
        "Current DDL placed in the editor. Adjust it, then run it.",
    ),
    ("已拉到 {} 个模型。", "Fetched {} models."),
    ("已拒绝", "Denied"),
    ("已拦下", "Blocked"),
    (
        "已拦截远程图片，打开可能向发件人暴露你的阅读状态。",
        "Remote images are blocked; loading them can tell the sender that you opened this mail.",
    ),
    (
        "已按 UTF-8 做百分号解码（`+` 保持原样）。",
        "Percent-decoded as UTF-8 (a literal + is kept as-is).",
    ),
    ("已授予 {} / {} 项能力", "{} of {} capabilities granted"),
    (
        "已提交：改 {} 行 · 新增 {} 行",
        "Committed: {} row(s) updated · {} row(s) inserted",
    ),
    (
        "已提交：改 {} 行 · 新增 {} 行。",
        "Committed: {} row(s) updated · {} row(s) inserted.",
    ),
    ("已搁置", "Deferred"),
    ("已撤销", "Undone"),
    (
        "已撤销一步（共 {} 条待提交）",
        "Undid one step ({} pending in total)",
    ),
    (
        "已撤销归档，任务已恢复。",
        "Archive undone; the task is back.",
    ),
    (
        "已撤销归档，项目已恢复。",
        "Archive undone; the project is back.",
    ),
    ("已改", "Edited"),
    (
        "已改为每页 {} 行，并回到第 1 页。",
        "Now {} rows per page; back to page 1.",
    ),
    ("已改为永不过期", "Changed to never expire"),
    ("已断开", "Disconnected"),
    ("已断开 {}", "Disconnected {}"),
    ("已断开连接", "Disconnected"),
    ("已断开（请重新连接）", "Down (reconnect)"),
    ("已新建「{}」。", "Created “{}”."),
    ("已是最新版本 {}。", "Version {} is the latest."),
    (
        "已暂存 1 条新增行（共 {} 条待提交）",
        "Staged 1 new row ({} pending in total)",
    ),
    (
        "已暂存 {} 处改动（共 {} 条待提交）",
        "Staged {} change(s) ({} pending in total)",
    ),
    ("已标为已读", "Marked as read"),
    ("已标为未读", "Marked as unread"),
    ("已格式化。", "Formatted."),
    ("已格式化（没有变化）。", "Formatted (nothing changed)."),
    (
        "已格式化；{} 段解析不了，保持原样。",
        "Formatted; {} statement(s) could not be parsed and were left untouched.",
    ),
    ("已注释 {} 行。", "Commented {} lines."),
    ("已添加到「{}」。", "Added to \"{}\"."),
    ("已添加标签「{}」", "Added label \"{}\""),
    (
        "已清空 {} 张表、删除 {} 项内容，并清空 AI 会话与知识库索引，重启后生效。",
        "Cleared {} tables and removed {} items, plus AI conversations and the knowledge-base index; a restart applies it.",
    ),
    (
        "已清空 {} 张表、删除 {} 项内容，重启后生效。",
        "Cleared {} tables and removed {} items; restart to apply.",
    ),
    (
        "已清空归档，共 {} 项。",
        "Archive cleared; {} items removed.",
    ),
    (
        "已生成删表语句；确认无误后再执行。",
        "DROP TABLE statement generated. Double-check it before running.",
    ),
    (
        "已生成建表骨架，改好表名与列后执行。",
        "CREATE TABLE skeleton generated. Adjust the table name and columns, then run it.",
    ),
    (
        "已生成改表骨架，改好后执行。",
        "ALTER TABLE skeleton generated. Adjust it, then run it.",
    ),
    ("已移入「{}」", "Moved into \"{}\""),
    ("已移动", "Moved"),
    ("已移动到「{}」。", "Moved to \"{}\"."),
    ("已移除标签「{}」", "Removed label \"{}\""),
    (
        "已索引 {} 个目录、{} 块",
        "Indexed {} directories, {} chunks",
    ),
    ("已装版本", "Installed version"),
    (
        "已设为默认每页 {} 行。",
        "Default page size set to {} rows.",
    ),
    ("已识别为「{}」", "Recognized as \"{}\""),
    (
        "已识别为「{}」，到工具箱确认处理",
        "Recognized as \"{}\" — confirm in the toolbox",
    ),
    ("已读取{}。", "Loaded {}."),
    (
        "已载入图表「{}」，执行后即可看图。",
        "Loaded chart “{}”; run it to see the chart.",
    ),
    ("已载入查询「{}」。", "Loaded query “{}”."),
    ("已达上限", "Capped"),
    ("已达上限，结果被截断", "Row cap reached, results truncated"),
    (
        "已达最大工具轮次（{}）",
        "Reached the maximum number of tool rounds ({})",
    ),
    ("已达最大输出长度", "Reached the maximum output length"),
    ("已连接", "Connected"),
    ("已连接 {}", "Connected {}"),
    ("已连接 {}。", "Connected {}."),
    ("已连接 {}（{}）", "Connected to {} ({})"),
    ("已选 {} 行", "{} rows selected"),
    ("已逾期。", "Overdue."),
    (
        "已重做一步（共 {} 条待提交）",
        "Redid one step ({} pending in total)",
    ),
    ("已重命名为 {}", "Renamed to {}"),
    (
        "已重新扫描插件目录，当前列出 {} 个工具。",
        "Rescanned the plugin directory; {} tools listed.",
    ),
    ("已隐藏 {} 列", "{} column(s) hidden"),
    ("已隐藏完成", "Completed hidden"),
    (
        "市场共 {} 个插件 · 上次更新：{}",
        "{} plugins in the marketplace · last updated: {}",
    ),
    ("市场源", "Marketplace source"),
    (
        "市场源与已安装插件",
        "Marketplace source and installed plugins",
    ),
    ("市场版本", "Marketplace version"),
    ("布局", "Layout"),
    ("帮助", "Help"),
    ("常规", "General"),
    ("平均启动耗时", "Average start-up time"),
    ("平均耗时", "Average duration"),
    ("平铺", "Flat"),
    ("并发嗅探数", "Concurrent sniffs"),
    ("序列", "Sequence"),
    ("库 {}", "db {}"),
    (
        "库名（网络引擎可留空）",
        "Database name (optional for network engines)",
    ),
    ("应用", "Application"),
    ("应用内 + 系统通知", "In-app + system notification"),
    ("应用内与系统通知通道", "In-app and system channels"),
    ("应用内提示", "In-app toast"),
    ("应用内横幅", "In-app banner"),
    ("应用到选中行", "Apply to selected rows"),
    ("建立传输通道", "Open transport"),
    ("建立连接", "Connect"),
    ("建连前", "Before connect"),
    ("建连后", "After connect"),
    ("建连后立即 SELECT 该库", "SELECTed right after connecting"),
    ("开", "On"),
    ("开发", "Dev"),
    ("开发工具", "Developer"),
    ("开发者", "Developer"),
    ("开发者模式", "Developer mode"),
    ("开启事务（BEGIN）", "Begin transaction (BEGIN)"),
    ("开始", "Start"),
    ("开始新的对话", "Start a new conversation"),
    ("开始日期", "Start date"),
    ("开始索引 {}（{} 个文件）", "Indexing {} ({} files)"),
    ("开机自动启动", "Launch at login"),
    (
        "开机自启设置失败：{}",
        "Failed to update launch at login: {}",
    ),
    ("开源许可", "Open source licenses"),
    ("开源许可窗口", "Licenses window"),
    ("引导", "Getting started"),
    ("引导窗口", "Onboarding window"),
    ("引擎", "Engine"),
    (
        "引用的已保存图表已被删除；在编辑态里换一条。",
        "The referenced saved chart was deleted; pick another in edit mode.",
    ),
    ("归档", "Archive"),
    ("归档中心", "Archive"),
    ("归档任务", "Archive task"),
    ("归档区是空的", "The archive is empty"),
    (
        "归档区里没有匹配「{}」的条目",
        "Nothing in the archive matches “{}”",
    ),
    (
        "归档区里的 {} 项将被永久删除，无法恢复。",
        "The {} items in the archive will be permanently deleted. This cannot be undone.",
    ),
    ("归档失败：{}", "Archive failed: {}"),
    ("归档时间", "Archived at"),
    (
        "归档的任务和项目会出现在这里，随时可以恢复。",
        "Archived tasks and projects show up here, ready to restore.",
    ),
    ("归档项目", "Archive project"),
    ("归档（M2 接入）", "Archive (coming in M2)"),
    ("当前位置是只读的。", "The current location is read-only."),
    (
        "当前位置没有写入权限。",
        "The current location is not writable.",
    ),
    (
        "当前使用单次覆盖，清除后回到连接默认。",
        "A one-off override is active; clear it to go back to the connection default.",
    ),
    ("当前占用", "Current usage"),
    ("当前命名空间", "Current namespace"),
    (
        "当前平台不支持就地更新。",
        "In-place updates are not supported on this platform.",
    ),
    (
        "当前库没有表或视图。若还没选定库，请在上方的命名空间下拉里选一个。",
        "This database has no tables or views. If no database is selected yet, pick one in the namespace dropdown above.",
    ),
    (
        "当前引擎不支持导出 INSERT 语句。",
        "This engine cannot export INSERT statements.",
    ),
    (
        "当前数据会先自动备份到上面那个目录。重置后需要重启应用才会完全生效。",
        "Your current data is backed up to the folder above first. Restart the app after resetting for it to fully apply.",
    ),
    ("当前数据库", "Current database"),
    ("当前没有会话。", "No active sessions."),
    (
        "当前没有可分析的执行错误。",
        "There is no execution error to analyze.",
    ),
    ("当前没有可导出的结果。", "No results to export."),
    ("当前没有可导出的连接。", "No connections to export."),
    (
        "当前没有可画图的结果。",
        "There is no result to chart right now.",
    ),
    (
        "当前没有可配置的工具。",
        "No tools are available to configure.",
    ),
    ("当前版本", "Current version"),
    ("当前生效主题", "Active theme"),
    ("当前目录", "Directory"),
    (
        "当前筛选下的 {} 条记录会全部删除（整个表都会清空，不只是筛出来的这些）。",
        "The entire log will be deleted, not just the {} records currently shown.",
    ),
    (
        "当前结果集不可编辑（需要单表 + 主键齐全）。",
        "This result set is not editable (it needs a single table with a complete primary key).",
    ),
    (
        "当前结果集不可编辑（需要单表 + 主键）。",
        "This result set is not editable (it needs a single table with a primary key).",
    ),
    (
        "当前视图没有内容可导出。",
        "There is nothing in the current view to export.",
    ),
    ("当前跟随连接默认。", "Using the connection default."),
    (
        "当前运行的不是打包后的 .app。",
        "This build is not running from a packaged .app.",
    ),
    ("当前连接为只读。", "The current connection is read-only."),
    ("当前连接：{}", "Current connection: {}"),
    (
        "影响 {} 行 · {} 条语句 · {} ms",
        "{} rows affected · {} statements · {} ms",
    ),
    ("彻底删除", "Delete permanently"),
    ("彻底删除任务？", "Permanently delete this task?"),
    (
        "彻底删除后无法恢复。",
        "Permanent deletion cannot be undone.",
    ),
    ("彻底删除失败：{}", "Permanent delete failed: {}"),
    ("彻底删除项目？", "Permanently delete this project?"),
    ("待办", "To do"),
    ("待提交 {} 条", "{} pending"),
    ("快捷键", "Shortcut"),
    ("快速入口", "Quick actions"),
    ("忽略", "Dismiss"),
    ("怎么用", "How to use"),
    ("思考过程", "Reasoning"),
    ("恢复", "Restore"),
    ("恢复失败：{}", "Restore failed: {}"),
    ("慢日志", "Slow log"),
    ("慢日志已清空", "Slow log cleared"),
    ("成功", "Success"),
    (
        "成功以外的记录（失败、被拦下、已取消）。被拦下不是错误，是策略生效。",
        "Everything except success: failures, blocks, and cancellations. A block is policy working, not an error.",
    ),
    ("成员", "Member"),
    ("成员不能为空", "Member must not be empty"),
    ("成员已写入", "Member written"),
    ("成员已删除", "Member deleted"),
    ("成员已加入", "Member added"),
    ("截止", "Due"),
    ("截止 {}", "Due {}"),
    ("截止 {} {}", "Due {} {}"),
    ("截止日最近", "Due soonest"),
    ("截止日最远", "Due latest"),
    ("截止日期", "Due date"),
    ("所属项目", "Project"),
    ("所有数据库", "All databases"),
    (
        "手动事务 · 点击提交",
        "Manual transaction · click to commit",
    ),
    ("手动填写服务器与端口", "Enter servers and ports manually"),
    ("手动配置", "Manual"),
    ("手动顺序", "Manual order"),
    ("打不开链接：{}", "Could not open the link: {}"),
    ("打开", "Open"),
    ("打开 SFTP 面板", "Open SFTP panel"),
    (
        "打开 SFTP 面板（需先连接远程主机）",
        "Open SFTP panel (connect a remote host first)",
    ),
    (
        "打开一个会话后，这里显示连接详情与传输队列。",
        "Open a session to see its details and transfers.",
    ),
    (
        "打开助手页时选中哪一条。",
        "Which entry is selected when the assistant page opens.",
    ),
    (
        "打开后，该连接上每条写命令在执行前都要二次确认（M1.3 收尾接线）。",
        "When on, every write command on this connection asks for confirmation first (wired up at the end of M1.3).",
    ),
    ("打开工作区", "Open workspace"),
    ("打开工具箱", "Open toolbox"),
    ("打开所属表", "Open owning table"),
    ("打开插件中心", "Open plugin center"),
    ("打开文档失败：{}", "Could not open the docs: {}"),
    ("打开日志", "Open logs"),
    ("打开日志目录", "Open log directory"),
    (
        "打开的工具会以标签页保留在这里。",
        "Open tools stay here as tabs.",
    ),
    ("打开目录", "Open folder"),
    ("打开草稿失败：{}", "Could not open the draft: {}"),
    ("打开邮件失败：{}", "Could not open the message: {}"),
    (
        "打开邮箱数据库失败：{}",
        "Could not open the mailbox database: {}",
    ),
    ("打开链接失败：{}", "Could not open the link: {}"),
    ("打标签", "Label"),
    ("打标签（M2 接入）", "Add tag (coming in M2)"),
    ("执行", "Run"),
    (
        "执行 SQL 后在这里查看结果",
        "Run a query to see results here",
    ),
    (
        "执行 SQL 后这里会记录每次执行的读数与错误。",
        "Each execution's row counts and errors are listed here.",
    ),
    ("执行全部语句（⌘↵）", "Execute all statements (⌘↵)"),
    ("执行前确认", "Confirm before running"),
    ("执行危险 SQL", "Run dangerous SQL"),
    ("执行原始命令", "Run raw command"),
    (
        "执行命令前弹出确认框，避免误触。",
        "Asks for confirmation before running a command, to avoid accidental hits.",
    ),
    ("执行失败：{}", "Execution failed: {}"),
    ("执行模型", "Execution model"),
    ("执行第 {} 条语句", "Run statement at line {}"),
    ("执行计划", "Query plan"),
    (
        "执行计划一次只看一条语句。",
        "An execution plan covers one statement at a time.",
    ),
    ("执行语句", "Run statement"),
    ("执行过程", "Execution"),
    ("扫描失败：{}", "Scan failed: {}"),
    ("扫描键数量", "SCAN count"),
    ("扫描页大小", "SCAN page size"),
    ("批量", "batch"),
    ("批量修改失败：{}", "Batch update failed: {}"),
    (
        "批量删除只在表浏览下可用。",
        "Bulk delete is only available while browsing a table.",
    ),
    ("批量删除失败：{}", "Bulk delete failed: {}"),
    ("批量删除行", "Delete rows in bulk"),
    (
        "批量编辑只在表浏览下可用。",
        "Bulk edit is only available while browsing a table.",
    ),
    ("批量重命名", "Batch rename"),
    ("抄送", "Cc"),
    ("抄送：{}", "Cc: {}"),
    ("把 {} 回填到编辑框", "Fill {} into the editor"),
    ("把「{}」这一格写成 NULL", "Write NULL into the \"{}\" cell"),
    (
        "把上次退出时开着的设置、日志等窗口一并打开，各自回到原来的位置。",
        "Reopen the settings, logs and other windows that were open at quit, each back where it was.",
    ),
    (
        "把第 {} 条历史放回编辑器",
        "Put history entry {} back into the editor",
    ),
    (
        "把视频页面的链接粘贴到顶栏搜索框，或用浏览器打开后点「解析」。",
        "Paste the video page URL into the top bar search box, or open it in a browser and hit Parse.",
    ),
    (
        "把设置、快捷键、AI 会话与已安装的插件打成一个归档文件。",
        "Pack settings, keybindings, AI conversations and installed plugins into a single archive.",
    ),
    (
        "把设置、快捷键与已安装的插件打成一个归档文件。",
        "Packs your settings, keybindings and installed plugins into a single archive.",
    ),
    (
        "把选中列整列写成 NULL",
        "Write NULL into the selected column for every selected row",
    ),
    ("抓包", "capture"),
    (
        "投递到操作系统通知中心。需要已打包成应用并授权；开发构建下通常不显示。",
        "Delivered to the OS notification center. Requires a packaged app with permission granted; usually not shown in a development build.",
    ),
    ("折叠全部", "Collapse all"),
    ("折线图", "Line chart"),
    ("折起「{}」这一段", "Collapse “{}”"),
    ("拉取失败：{}", "Fetch failed: {}"),
    ("拉取模型", "Fetch models"),
    ("拖一张卡片到这里", "Drag a card here"),
    ("拷贝", "Copy"),
    (
        "按 0.8 ~ 1.5 倍缩放全部尺寸 token，并同步重算密度档位。",
        "Scales every size token by 0.8–1.5× and recomputes density tiers.",
    ),
    (
        "按 KB/s 输入，0 视为无限制。",
        "Enter KB/s; 0 means unlimited.",
    ),
    (
        "按 Unix 时间戳解析（微秒）。",
        "Read as a Unix timestamp (microseconds).",
    ),
    (
        "按 Unix 时间戳解析（毫秒）。",
        "Read as a Unix timestamp (milliseconds).",
    ),
    (
        "按 Unix 时间戳解析（秒）。",
        "Read as a Unix timestamp (seconds).",
    ),
    (
        "按 Unix 时间戳解析（纳秒）。",
        "Read as a Unix timestamp (nanoseconds).",
    ),
    ("按 {} {}", "Sort by {} {}"),
    ("按 {} 升序", "Sorted by {} ascending"),
    ("按 {} 排序", "Sort by {}"),
    ("按 {} 降序", "Sorted by {} descending"),
    (
        "按上面的保留时长删掉旧日志，不等下一次自动清理。",
        "Delete old logs using the retention period above, without waiting for the next automatic cleanup.",
    ),
    (
        "按下组合键…（Esc 取消）",
        "Press a combination… (Esc to cancel)",
    ),
    ("按优先级", "By priority"),
    ("按名称、驱动或端口筛选", "Filter by name, driver, or port"),
    ("按截止日", "By due date"),
    ("按标题", "By title"),
    (
        "按模板与正则整理文件名",
        "Organizes file names with templates and regex",
    ),
    ("按进度", "By progress"),
    (
        "换个关键词试试，或到插件市场安装更多工具。",
        "Try another keyword, or install more tools from the plugin marketplace.",
    ),
    (
        "换个关键词，或清掉右上角的搜索框再看看。",
        "Try another keyword, or clear the search box at the top right.",
    ),
    (
        "换个关键词，或清空搜索框回到文件夹列表；也可以用 from: / is:unread 这类条件。",
        "Try another keyword, or clear the search box to return to the folder list; from: / is:unread style conditions also work.",
    ),
    ("排序", "Sort"),
    ("排队中", "Queued"),
    ("接入方式", "Access"),
    ("接受无效证书", "Accept invalid certificates"),
    ("接受自签", "Accept self-signed"),
    ("接口地址", "API base URL"),
    ("接口默认", "Server default"),
    (
        "控件高度与行高的疏密档位，与界面缩放叠加生效；「紧凑」同屏显示更多内容，「宽松」更易点选。",
        "Density of control heights and row heights, combined with UI scale. Compact fits more on screen; comfortable is easier to hit.",
    ),
    ("描述", "Description"),
    ("提交", "Commit"),
    (
        "提交 {} · 改 {} 行 · 新增 {} 行",
        "Committed {} · {} rows updated · {} inserted",
    ),
    ("提交 {} 条暂存改动", "Commit {} staged changes"),
    ("提交事务", "Commit transaction"),
    ("提交事务（COMMIT）", "Commit transaction (COMMIT)"),
    ("提交写入", "Commit writes"),
    (
        "提交失败（已整体回滚）：{}",
        "Commit failed, everything rolled back: {}",
    ),
    ("提交失败：{}", "Commit failed: {}"),
    ("插件", "Plugins"),
    ("插件「{}」已停用", "Plugin \"{}\" disabled"),
    ("插件「{}」已卸载。", "Plugin \"{}\" uninstalled."),
    ("插件「{}」已启用", "Plugin \"{}\" enabled"),
    (
        "插件「{}」已安装（v{}）。",
        "Plugin \"{}\" installed (v{}).",
    ),
    (
        "插件「{}」的权限已更新",
        "Permissions for plugin \"{}\" updated",
    ),
    ("插件中心", "Plugin Center"),
    ("插件中心窗口", "Plugin center window"),
    ("插件信息", "Plugin details"),
    (
        "插件包请从插件中心安装。",
        "Install plugin packages from the Plugin Center.",
    ),
    ("插件市场源", "Plugin marketplace source"),
    ("插件执行失败：{}", "Plugin execution failed: {}"),
    ("插件数量", "Plugins"),
    (
        "插件清单已更新，共 {} 个插件。",
        "Plugin list updated: {} plugins.",
    ),
    (
        "插件清单的获取地址；留空回落默认源，保存后立即重新获取。",
        "URL the plugin list is fetched from; leave it empty to use the default source. Saving refetches immediately.",
    ),
    (
        "插件：在插件市场挑选并安装新工具，装完就在工具箱里。",
        "Plugins: browse the plugin center to install new tools; they show up in the toolbox.",
    ),
    ("搜索", "Search"),
    ("搜索任务…", "Search tasks…"),
    ("搜索会话", "Search conversations"),
    ("搜索会话…", "Search conversations…"),
    ("搜索全部邮件", "Search all mail"),
    ("搜索到 {} 封邮件", "Found {} messages"),
    ("搜索失败：{}", "Search failed: {}"),
    ("搜索工具、命令…", "Search tools, commands…"),
    ("搜索归档…", "Search archive…"),
    ("搜索插件…", "Search plugins…"),
    ("搜索结果", "Search results"),
    ("搜索设置...", "Search settings…"),
    ("搜索语法", "Search syntax"),
    ("搜索连接…", "Search connections…"),
    ("搜索连接与对象…", "Search connections and objects…"),
    (
        "搜索邮件，如 from: 或 is:unread",
        "Search mail, e.g. from: or is:unread",
    ),
    ("搜索：{}", "Search: {}"),
    ("摘要", "Summary"),
    ("撤消", "Undo"),
    ("撤销", "Undo"),
    ("撤销失败：{}", "Undo failed: {}"),
    ("操作失败：{}", "Action failed: {}"),
    (
        "支持多条语句；读语句结果按 10000 行一页展示，可逐页加载更多。",
        "Multiple statements are supported; read results show 10,000 rows per page, with load-more paging.",
    ),
    (
        "支持多条语句；读语句结果最多展示 10000 行。",
        "Multiple statements supported; SELECT results show up to 10000 rows.",
    ),
    ("收件人", "To"),
    (
        "收件人，子串匹配；中文「收件人:」同义",
        "Recipients, substring match; the Chinese alias works too",
    ),
    ("收件人：{}", "To: {}"),
    ("收件箱", "Inbox"),
    ("收起", "Collapse"),
    ("收起侧栏", "Collapse sidebar"),
    ("收起右侧栏", "Collapse right sidebar"),
    ("收起设置", "Hide settings"),
    ("收集箱", "Inbox"),
    (
        "改动先暂存，点结果区上方的「提交」才落库。",
        "Edits are staged first; click \"Commit\" above the result area to write them.",
    ),
    ("改标题", "Rename"),
    (
        "放宽筛选，或清掉「只看问题 / 关联 id」再看看。",
        "Loosen the filter, or turn off Problems only / Trace id and look again.",
    ),
    (
        "放宽级别或换一个关键字，或清空过滤条件看全部。",
        "Loosen the level or try another keyword, or clear the filters to see everything.",
    ),
    ("数值列", "Value columns"),
    ("数据", "Data"),
    ("数据传输", "Data Transfer"),
    ("数据库", "Database"),
    ("数据库数量", "Database count"),
    ("数据库文件", "Database file"),
    ("数据库连接", "Database connections"),
    ("数据文件", "Data file"),
    ("数据浏览每页行数", "Rows per page"),
    ("数据表", "Data"),
    (
        "整段历史超过这个字符数就开始压缩：先裁旧工具结果，再丢最早的消息。留空用默认值 96000；填 0 表示关闭压缩，长会话可能因此超出模型的上下文窗口。",
        "Compaction kicks in once the whole history exceeds this many characters: old tool results are trimmed first, then the earliest messages are dropped. Leave blank for the default of 96000; enter 0 to turn compaction off — long conversations may then exceed the model's context window.",
    ),
    ("整行", "Full row"),
    ("文件内容不是合法的 JSON。", "The file is not valid JSON."),
    ("文件名", "filename"),
    ("文件名编码", "Filename encoding"),
    ("文件图标", "File icons"),
    ("文件夹", "Folders"),
    ("文件夹 {}", "Folder {}"),
    ("文件夹 {}，{} 封未读", "Folder {}, {} unread"),
    ("文件夹 {}，{} 封草稿", "Folder {}, {} drafts"),
    ("文件夹名称", "Folder name"),
    (
        "文件夹名，或 inbox / sent / archive 这类角色；中文「文件夹:」同义",
        "Folder name, or a role like inbox / sent / archive; the Chinese alias works too",
    ),
    (
        "文件类工具只能访问这个目录里的内容；含 .. 的路径一律拒绝。",
        "File tools can only reach inside this directory; paths containing .. are always refused.",
    ),
    ("文件访问", "File access"),
    (
        "文件：单击选中 · 双击进入目录",
        "Files: click to select · double-click to enter",
    ),
    ("文本", "Text"),
    ("文档", "Document"),
    ("文档转换", "Documents"),
    ("断开", "Disconnect"),
    ("断开 {}", "Disconnect {}"),
    ("断开前", "Before disconnect"),
    ("断开后", "After disconnect"),
    ("断开客户端失败：{}", "Failed to disconnect client: {}"),
    ("断开客户端连接", "Disconnect client"),
    ("断开连接", "Disconnect"),
    ("断线自动重连", "Auto-reconnect"),
    ("新会话", "New conversation"),
    ("新值", "New value"),
    ("新名称", "New name"),
    ("新增", "New"),
    ("新增行", "New row"),
    (
        "新增行至少要填一列（空白列会落到数据库默认值）。",
        "A new row needs at least one column filled in (blank columns fall back to the database default).",
    ),
    ("新建", "New"),
    ("新建 Redis 连接", "New Redis connection"),
    ("新建 {} 连接", "New {} connection"),
    ("新建仪表板", "New dashboard"),
    ("新建任务，回车确认", "New task — press Enter"),
    ("新建会话", "New conversation"),
    ("新建分组", "New group"),
    ("新建工具", "New tool"),
    ("新建文件夹", "New folder"),
    ("新建查询", "New Query"),
    ("新建查询标签", "New query tab"),
    ("新建标签", "New label"),
    ("新建标签 ⌘T · 关闭标签 ⌘W", "New tab ⌘T · Close tab ⌘W"),
    ("新建标签…", "New label…"),
    ("新建标签页", "New tab"),
    ("新建终端标签", "New terminal tab"),
    ("新建表", "New table"),
    ("新建连接", "New connection"),
    (
        "新建连接时预置「只读档」。连接档案里仍可单独修改。",
        "Pre-set the read-only level on new connections; each connection can still override it.",
    ),
    (
        "新建连接时预置的环境标签。",
        "Environment tag pre-set on new connections.",
    ),
    ("新建连接的默认值", "New-connection defaults"),
    (
        "新建连接的默认查询超时（秒）；留空 = 跟随引擎。",
        "Default query timeout for new connections (seconds); blank follows the engine.",
    ),
    ("新建项目", "New project"),
    ("新的键名", "New key name"),
    ("新的键名不能为空", "New key name must not be empty"),
    ("新邮件", "New mail"),
    ("新邮件通知", "New-mail notifications"),
    (
        "新键名与原名相同",
        "New key name is the same as the old one",
    ),
    ("新项目", "Untitled project"),
    ("旗标", "Flag"),
    ("旗标（M2 接入）", "Flag (coming in M2)"),
    ("无标签", "No tag"),
    (
        "无法启动索引线程：{}",
        "Could not start the indexing thread: {}",
    ),
    (
        "无法启动请求线程：{}",
        "Could not start the request thread: {}",
    ),
    (
        "无法启动连接进程：{}",
        "Could not start the connection process: {}",
    ),
    (
        "无法打开{}，详见日志。",
        "Could not open {}; see the logs for details.",
    ),
    ("无法读取该目录。", "Cannot read this folder."),
    ("无法起 ssh", "Could not start ssh"),
    ("无法重启应用：{}", "Could not relaunch the app: {}"),
    ("无结果", "No results"),
    ("无通知", "No notifications"),
    ("无限制", "Unlimited"),
    ("日志", "Logs"),
    (
        "日志体积较大，只在需要排查问题时才勾选。",
        "Logs are bulky; enable only when troubleshooting.",
    ),
    ("日志保留", "Log retention"),
    ("日志文件与运行记录", "Log files and run history"),
    ("日志目录", "Log directory"),
    (
        "日志目录里所有 .log 的总大小。",
        "Total size of every .log in the log directory.",
    ),
    ("日志窗口", "Logs window"),
    (
        "日期也认相对词：今天 / 昨天 / 前天 / 本周 / 上周 / 本月 / 上月 / 最近7天",
        "Dates also accept relative words: today, yesterday, this week, last week, this month, last 7 days",
    ),
    (
        "早于这一天（older: 同义；中文「早于:」「之前:」同义）",
        "Before this day (older: is an alias; the Chinese aliases work too)",
    ),
    ("时间序列和日志", "Time series & logs"),
    ("时间戳", "Timestamp"),
    ("明文（仅本地测试）", "Plain (local testing only)"),
    ("明细（{} 行）", "Detail ({} rows)"),
    ("是", "Yes"),
    ("显示", "Display"),
    ("显示 {} / {} 行", "{} of {} lines shown"),
    ("显示全部", "Show All"),
    ("显示名", "Display name"),
    ("显示已完成", "Show completed"),
    ("显示查询结果", "Show query results"),
    (
        "显示标记为开发者的工具，仅在排查问题时使用。",
        "Shows tools marked as developer-only. Use it for troubleshooting only.",
    ),
    ("显示活动栏", "Show activity bar"),
    ("显示远程图片", "Show remote images"),
    ("显示通知铃铛", "Show notification bell"),
    ("显示顶栏附加入口", "Show extra top bar entries"),
    ("智能列表", "Smart lists"),
    ("智能列表 · {}", "Smart list · {}"),
    ("暂停", "Pause"),
    ("暂存改动已丢弃。", "Pending changes discarded."),
    ("暂存更改", "Pending changes"),
    ("暂无打开的会话", "No open session"),
    ("暂无执行消息", "No execution messages yet"),
    ("暗色", "Dark"),
    ("暗色主题", "Dark theme"),
    (
        "暗色预设 Default Dark，强调色取自主题 token",
        "Dark preset Default Dark; the accent comes from theme tokens",
    ),
    ("更多", "More"),
    ("更多主题", "More themes"),
    ("更多操作", "More actions"),
    ("更多操作（M2 接入）", "More actions (coming in M2)"),
    ("更换", "Change"),
    ("更新", "Update"),
    (
        "更新后，本插件会请求访问以下资源：",
        "Once updated, this plugin requests access to:",
    ),
    ("更新失败：{}", "Update failed: {}"),
    (
        "更新已安装，重启后生效。",
        "The update is installed; it takes effect after a restart.",
    ),
    ("更新已就绪", "Update ready"),
    (
        "更新已读状态失败：{}",
        "Could not update the read state: {}",
    ),
    ("更新插件「{}」", "Update plugin \"{}\""),
    ("更新源", "Update source"),
    ("更新源与版本", "Update source and version"),
    ("更新状态", "Update status"),
    (
        "更早的语句可能已生效。",
        "Earlier statements may already have taken effect.",
    ),
    ("最后一页", "Last page"),
    ("最大工具轮次", "Max tool rounds"),
    (
        "最大工具轮次要不小于 1 的整数",
        "Max tool rounds must be an integer of at least 1",
    ),
    ("最大输出", "Max output"),
    (
        "最大输出要是正整数",
        "Max output must be a positive integer",
    ),
    ("最小化", "Minimize"),
    ("最新发布", "Latest release"),
    ("最近", "Recent"),
    (
        "最近 100 条投递记录，只在本次运行内保留。",
        "The last 100 deliveries, kept only for this run.",
    ),
    ("最近 {} 次启动的平均值", "Average of the last {} launches"),
    ("最近 {} 次采样 · 覆盖 {}", "Last {} samples · spanning {}"),
    ("最近一次：{}", "Latest: {}"),
    ("最近使用", "Recently used"),
    ("最近创建", "Recently created"),
    ("最近查询", "Recent Queries"),
    (
        "有 {} 个附件读不出来，已从草稿里略去。",
        "{} attachments could not be read and were left out of the draft.",
    ),
    (
        "有 {} 条钩子失败（按配置继续）。",
        "{} of the hooks failed (continuing as configured).",
    ),
    (
        "有一条启用的钩子还没填完（命令或脚本不能为空）。",
        "An enabled hook is incomplete (the command or script cannot be empty).",
    ),
    (
        "有一档传输配置没填完，不会生效。",
        "One transport entry is incomplete and will not take effect.",
    ),
    ("有序集合", "Sorted set"),
    (
        "有改动未命中目标行（行可能已被他人修改），请刷新后重试。",
        "Some changes did not match their target rows (they may have been changed by someone else); refresh and try again.",
    ),
    (
        "有改动未命中目标行（该行可能已被他人修改），请刷新后重试。",
        "Some changes did not match the target row (it may have been modified by someone else). Refresh and try again.",
    ),
    (
        "有数据库操作正在进行，请稍候。",
        "A database operation is running, please wait.",
    ),
    (
        "有行未命中（可能已被他人删除），请刷新后重试。",
        "Some rows were not found (they may have been deleted by someone else); refresh and try again.",
    ),
    ("有附件读不出来：{}", "Could not read an attachment: {}"),
    ("服务", "Services"),
    ("服务商", "Provider"),
    ("服务器名称指示（SNI）", "Server name indication (SNI)"),
    ("服务端", "Server"),
    (
        "服务端 ACL 未授予写权限，解锁也无法写入",
        "The server ACL grants no write access; unlocking will not help",
    ),
    (
        "服务端没有给出诊断报告。",
        "The server returned no diagnostics report.",
    ),
    ("服务端清单", "Server list"),
    ("未发现插件", "No plugins found"),
    ("未同步", "Not synced"),
    ("未命名档案", "Untitled profile"),
    (
        "未在本机授予的能力一律被拒绝；卸载插件即收回全部权限。",
        "Capabilities not granted on this machine are denied; uninstalling the plugin takes them all back.",
    ),
    ("未填写", "Not set"),
    ("未安装", "Not installed"),
    ("未归入项目", "Unassigned"),
    ("未找到客户端 {}", "Client {} not found"),
    (
        "未找到系统 ssh / scp 命令，无法打开 SFTP 面板。",
        "System ssh / scp commands not found; cannot open the SFTP panel.",
    ),
    (
        "未找到系统 ssh 命令，无法建立 SSH 连接。请先安装 OpenSSH 客户端。",
        "System ssh command not found; cannot start an SSH connection. Install an OpenSSH client first.",
    ),
    (
        "未找到系统命令 {}，请先安装 OpenSSH 客户端。",
        "System command {} not found. Install an OpenSSH client first.",
    ),
    ("未排期", "Unscheduled"),
    ("未添加账户", "No account"),
    ("未生效：{}", "Not working: {}"),
    ("未知", "Unknown"),
    ("未知（未统计）", "Unknown (not analyzed)"),
    ("未设置", "Not set"),
    ("未请求任何能力", "No capabilities requested"),
    (
        "未读邮件：{}，来自 {}，{}",
        "Unread message: {}, from {}, {}",
    ),
    (
        "未读邮件：{}，来自 {}，{}（已加旗标）",
        "Unread message: {}, from {}, {} (flagged)",
    ),
    ("未连接", "Not connected"),
    ("未选择", "Not set"),
    ("未选模型", "No model chosen"),
    ("未配置", "Not configured"),
    ("本周", "This week"),
    ("本周执行", "Runs this week"),
    ("本地", "Local"),
    ("本地 shell", "Local shell"),
    (
        "本地 shell 没有远端文件可浏览，请先连接一台远程主机。",
        "The local shell has no remote files to browse; connect to a remote host first.",
    ),
    ("本地 zsh", "Local zsh"),
    ("本地 zsh {index}", "Local zsh {index}"),
    ("本地开发", "Local development"),
    ("本地目录", "Local folder"),
    (
        "本地目录会立刻读取本机磁盘（只读）。",
        "Local folders are read from disk right away (read-only).",
    ),
    ("本地知识库", "Local knowledge base"),
    ("本地端点，无需密钥", "Local endpoint, no key needed"),
    ("本地终端", "Local terminal"),
    ("本地终端未启动", "Local terminal not started"),
    ("本地路径不存在：{}", "Local path does not exist: {}"),
    ("本地转发", "Local forward"),
    (
        "本地转发把本机端口的流量送到远端；远程转发反过来；动态转发在本机起一个 SOCKS 代理。留空绑定地址即用 ssh 默认。",
        "A local forward sends traffic from a local port to the remote side; a remote forward does the opposite; a dynamic forward starts a SOCKS proxy locally. Leave the bind address empty to use the ssh default.",
    ),
    ("本平台不可用", "Unavailable on this platform"),
    (
        "本服务端不支持 MONITOR。",
        "This server does not support MONITOR.",
    ),
    (
        "本服务端没有 {} 命令，无法显示「{}」面板。",
        "This server has no {} command, so the \"{}\" panel is unavailable.",
    ),
    (
        "本机凭据存储不可用",
        "Credential storage is unavailable on this machine",
    ),
    (
        "本机凭据存储不可用，密钥存不下；请改用本地端点（Ollama）。",
        "Credential storage is unavailable on this machine, so the key cannot be saved; use a local endpoint (Ollama) instead.",
    ),
    (
        "本机正文字号；其余文字与行高按同一比例适配，与界面缩放叠加生效。",
        "Body text size for this machine; all other text and line heights scale by the same ratio, stacked on top of the UI scale.",
    ),
    ("本机端口", "Local port"),
    ("本次启动后开始记录", "Recording starts with this launch"),
    (
        "本次按每页 {} 行展示。",
        "Showing {} rows per page for this session.",
    ),
    (
        "本次更新新增 {} 项权限，请确认。",
        "This update adds {} new permission(s). Please confirm.",
    ),
    (
        "本次查询超时已设为 {} 秒",
        "Query timeout for this session set to {}s",
    ),
    (
        "本版本未启用 TLS，选非 disable 档会在连接时报错；请改用 SSH 隧道。",
        "TLS is not enabled in this build; any mode other than disabled will fail on connect — use an SSH tunnel instead.",
    ),
    (
        "本版本未启用 TLS：选非「不启用」档会在连接时明确报错，不会静默降级成明文。",
        "TLS is not enabled in this build: any mode other than disabled fails loudly on connect and never silently falls back to plaintext.",
    ),
    ("本程序许可", "Application license"),
    ("本视图 {} 条", "{} tasks in this view"),
    ("本轮用量", "This turn's usage"),
    ("条", "entries"),
    ("松开并入分屏", "Release to merge into split"),
    ("查看 HTML 版本", "View HTML version"),
    ("查看开源许可", "View open source licenses"),
    ("查看执行计划", "Explain"),
    ("查看文档", "View docs"),
    ("查看纯文本版本", "View plain-text version"),
    ("查询", "Query"),
    (
        "查询上限与新连接默认值",
        "Query limits and new-connection defaults",
    ),
    ("查询内存占用失败：{}", "Failed to query memory usage: {}"),
    ("查询占用", "Query usage"),
    ("查询历史", "Query history"),
    ("查询历史上限", "History limit"),
    ("查询历史与已保存查询", "Query history and saved queries"),
    ("查询历史已清空。", "Query history cleared."),
    ("查询名称", "Query name"),
    ("查询结果", "Query results"),
    (
        "查询超时已恢复连接默认",
        "Query timeout restored to the connection default",
    ),
    (
        "查询超时必须是 1-{} 的整数秒",
        "Query timeout must be a whole number of seconds from 1 to {}",
    ),
    (
        "查询超时（{} 秒）已被中止。",
        "The query was aborted after the {} second timeout.",
    ),
    ("查询超时（秒）", "Query timeout (s)"),
    ("查询返回了 0 行。", "The query returned 0 rows."),
    ("查询选项", "Query options"),
    ("柱状图", "Bar chart"),
    ("标为已读", "Mark as read"),
    ("标为未读", "Mark as unread"),
    ("标准", "Standard"),
    (
        "标明此连接指向的环境。",
        "Indicates which environment this connection points to.",
    ),
    ("标签", "Tags"),
    ("标签 {}", "Label {}"),
    ("标签 · {}", "Tag · {}"),
    ("标签名", "Label name"),
    ("标签名不能为空", "Label name cannot be empty"),
    ("标签已创建", "Label created"),
    ("标红的字段为必填项。", "Fields marked in red are required."),
    ("标记为阻塞", "Mark as blocked"),
    ("标记完成", "Mark done"),
    ("标题", "Title"),
    ("树形", "Tree"),
    ("校验证书", "Verify certificate"),
    ("格式化", "Format"),
    ("格式化 SQL", "Format SQL"),
    (
        "格式化 SQL 时把关键字统一成的大小写。",
        "The case applied to keywords when formatting SQL.",
    ),
    (
        "格式化 SQL 时的缩进方式与宽度。",
        "Indent style and width used when formatting SQL.",
    ),
    (
        "格式化时关键字使用大写。",
        "Formatting will use uppercase keywords.",
    ),
    (
        "格式化时关键字使用小写",
        "Lowercase keywords when formatting",
    ),
    (
        "格式化时关键字使用小写。",
        "Formatting will use lowercase keywords.",
    ),
    (
        "档案与已保存的口令都会被移除，本机不会留下副本。",
        "The profile and any saved password will be removed; nothing is kept on this machine.",
    ),
    ("档案名称", "Profile name"),
    (
        "档案配了传输通道，但当前会话还没建立。",
        "The profile has a transport configured, but this session is not connected yet.",
    ),
    (
        "档案里没有本应用支持的引擎（目前支持 SQLite / PostgreSQL / MySQL）。",
        "The file has no engine this app supports (currently SQLite / PostgreSQL / MySQL).",
    ),
    ("检查更新", "Check for updates"),
    ("检查更新失败：{}", "Update check failed: {}"),
    (
        "检测到 {} 次未查看的崩溃，可在「设置 → 关于」页查看诊断信息。",
        "Detected {} unviewed crashes. See Settings → About for diagnostics.",
    ),
    ("概览", "Overview"),
    ("模块", "Modules"),
    ("模块类型", "Module type"),
    ("模型", "Model"),
    ("模型厂商", "Model providers"),
    (
        "模型厂商、密钥与生成参数",
        "Model providers, keys and generation parameters",
    ),
    ("模型名", "Model name"),
    ("模型请求调用工具", "The model asked to call a tool"),
    ("模板", "Template"),
    ("横轴", "X axis"),
    ("次", "times"),
    ("欢迎使用 Duck Toolbox", "Welcome to Duck Toolbox"),
    ("正在下载更新… {} / {}", "Downloading update… {} / {}"),
    ("正在下载正文…", "Downloading the body…"),
    ("正在保存行改动…", "Saving row changes…"),
    ("正在修改第 {} 行", "Editing row {}"),
    ("正在停止…", "Stopping…"),
    ("正在切换命名空间…", "Switching namespace…"),
    ("正在切换数据库 {}…", "Switching to database {}…"),
    ("正在删除 {} 行…", "Deleting {} rows…"),
    ("正在删除行…", "Deleting row…"),
    ("正在加载更多…", "Loading more…"),
    ("正在同步…", "Syncing…"),
    ("正在安装更新…", "Installing the update…"),
    ("正在导入 CSV…", "Importing CSV…"),
    ("正在打开邮件…", "Opening the message…"),
    ("正在打开邮箱数据库…", "Opening the mailbox database…"),
    ("正在执行 AI 生成的 SQL…", "Running the AI-generated SQL…"),
    ("正在执行…", "Running…"),
    ("正在扫描…", "Scanning…"),
    ("正在扫描键空间…", "Scanning the keyspace…"),
    ("正在批量修改 {} 行…", "Updating {} rows…"),
    ("正在拉取模型清单…", "Fetching the model list…"),
    ("正在提交 {} 条改动…", "Committing {} change(s)…"),
    ("正在校验更新…", "Verifying the update…"),
    ("正在检查更新…", "Checking for updates…"),
    ("正在测试 {}…", "Testing {}…"),
    ("正在测试…", "Testing…"),
    ("正在测试连接…", "Testing connection…"),
    ("正在生成（{}）…", "Generating ({})…"),
    ("正在索引…", "Indexing…"),
    (
        "正在编辑消息，发送后将从这里重新开始",
        "Editing a message; sending restarts the conversation from here.",
    ),
    ("正在获取插件清单…", "Fetching the plugin list…"),
    ("正在请求 AI…", "Asking AI…"),
    ("正在读取{}…", "Loading {}…"),
    ("正在读取实例信息…", "Loading instance info…"),
    ("正在读取表数据…", "Loading table data…"),
    ("正在连接 {}…", "Connecting to {}…"),
    ("正在重连到数据库 {}…", "Reconnecting to database {}…"),
    ("正文", "Body"),
    ("正文已下载", "Body downloaded"),
    ("正文未能获取：{}", "Could not fetch the body: {}"),
    (
        "此模块暂未提供侧栏内容。",
        "This module has no sidebar content yet.",
    ),
    (
        "每个文件夹一次加载多少封邮件。",
        "How many messages to load per folder.",
    ),
    (
        "每次 SCAN 的 COUNT；留空则使用界面默认值",
        "COUNT per SCAN; leave empty for the UI default",
    ),
    (
        "每次从工具箱运行该工具后累加。",
        "Increases every time the tool is run from the toolbox.",
    ),
    (
        "每次请求都作为 system 消息发出；留空则不加。",
        "Sent as a system message on every request; blank adds none.",
    ),
    ("每页 {} 行", "{} rows per page"),
    ("每页行数", "Rows per page"),
    ("每页行数至少是 1。", "Rows per page must be at least 1."),
    ("永久", "No expiry"),
    ("永久保留", "Keep forever"),
    (
        "没填的列会落到数据库默认值。",
        "Columns left blank fall back to the database default.",
    ),
    ("没有「{}」的任务", "No \"{}\" tasks"),
    ("没有列信息。", "No column information."),
    ("没有匹配「{}」的任务", "No tasks match \"{}\""),
    ("没有匹配的会话", "No matching conversations"),
    ("没有匹配的对象。", "No matching objects."),
    ("没有匹配的工具。", "No matching tools."),
    ("没有匹配的指标。", "No matching metrics."),
    ("没有匹配的插件", "No matching plugins"),
    ("没有匹配的数据库类型。", "No matching database types."),
    ("没有匹配的行", "No matching lines"),
    ("没有匹配的连接。", "No matching connection."),
    ("没有匹配的邮件", "No matching mail"),
    ("没有匹配的配置项。", "No matching configuration entries."),
    ("没有匹配的项", "Nothing matches"),
    ("没有匹配的项目。", "No matching projects."),
    ("没有可展示的结果", "No result to show"),
    ("没有可撤销的归档。", "Nothing to undo."),
    ("没有可撤销的暂存改动", "Nothing left to undo"),
    ("没有可用的文件夹", "No folders available"),
    (
        "没有可画的数值列；切回「数据表」看原始结果。",
        "No numeric column to plot; switch back to Data to see the raw result.",
    ),
    ("没有可重做的暂存改动", "Nothing left to redo"),
    ("没有可重试的提问", "There is no question to retry"),
    ("没有命中「{}」的行。", "No rows match “{}”."),
    ("没有客户端连接。", "No client connections."),
    ("没有待提交的改动", "Nothing to commit"),
    ("没有待提交的改动。", "Nothing to commit."),
    ("没有慢日志记录。", "No slow log entries."),
    (
        "没有找到这个工具，可能是插件尚未安装。",
        "That tool is not available. Its plugin may not be installed.",
    ),
    ("没有改动。", "No changes."),
    ("没有数值列。", "No numeric column."),
    (
        "没有新增连接（id 全部重复或无效）。",
        "No new connections (ids all duplicated or invalid).",
    ),
    ("没有日志文件", "No log files"),
    ("没有权限读取该目录。", "No permission to read this folder."),
    ("没有符合条件的记录", "No matching records"),
    ("没有结果集", "No result set"),
    ("没有采到值。", "No values sampled."),
    ("没有键", "No keys"),
    ("没有需要清理的日志。", "No log files needed cleaning."),
    ("法律信息", "Legal"),
    ("注册表", "registry"),
    ("注册表检查", "Registry inspector"),
    ("注释", "Comment"),
    ("注释 / 取消注释", "Toggle comments"),
    ("流", "Stream"),
    ("流式接收中", "Streaming"),
    ("流水", "Activity"),
    ("浅色", "Light"),
    ("测试", "Test"),
    ("测试失败：{}", "Test failed: {}"),
    ("测试成功：{}", "Test succeeded: {}"),
    ("测试环境", "Staging"),
    ("测试连接", "Test connection"),
    ("测试连接 {}（{}）失败：{}", "Test {} ({}) failed: {}"),
    ("测试连接 {}：{}", "Test {}: {}"),
    ("浏览数据", "Browse data"),
    ("浏览表 {}", "Browse table {}"),
    ("浏览表数据", "Browse table"),
    ("消息", "Messages"),
    ("消息输入框", "Message input"),
    ("深色", "Dark"),
    ("添加", "Add"),
    ("添加任务", "Add task"),
    ("添加厂商", "Add provider"),
    ("添加失败：{}", "Could not add: {}"),
    ("添加新服务器", "Add New Server"),
    ("添加条件", "Add condition"),
    ("添加目录", "Add folder"),
    (
        "添加账户后，这里列出你的邮件标签。",
        "Add an account to list your mail tags here.",
    ),
    (
        "添加账户后，这里列出收件箱、草稿等文件夹。",
        "Add an account to list inbox, drafts and other folders here.",
    ),
    ("添加转发", "Add forward"),
    ("添加邮箱账户", "Add mail account"),
    (
        "添加邮箱账户后，收件箱会同步到这里。",
        "Add a mailbox account and your inbox syncs here.",
    ),
    (
        "添加邮箱账户（账户向导待接入）",
        "Add mailbox account (setup wizard not wired up yet)",
    ),
    ("添加钩子", "Add hook"),
    ("添加附件", "Add attachment"),
    ("添加面板", "Add panel"),
    ("清单无效", "Invalid manifest"),
    ("清屏", "Clear screen"),
    (
        "清屏 ⌘K · 翻页 Shift+PageUp / Shift+PageDown",
        "Clear ⌘K · Page Shift+PageUp / Shift+PageDown",
    ),
    ("清理", "Clean up"),
    ("清空", "Clear all"),
    ("清空审计", "Clear audit log"),
    (
        "清空审计日志（原有 {} 条）",
        "Clear the audit log ({} entries)",
    ),
    ("清空审计日志？", "Clear the audit log?"),
    ("清空归档", "Clear archive"),
    ("清空归档失败：{}", "Failed to clear the archive: {}"),
    ("清空归档？", "Clear the archive?"),
    ("清空慢日志", "Clear slow log"),
    ("清空慢日志失败：{}", "Failed to clear slow log: {}"),
    (
        "清空所有设置与使用记录，保留插件与日志。",
        "Clears all settings and usage history; keeps plugins and logs.",
    ),
    ("清空搜索", "Clear search"),
    ("清空条件", "Clear conditions"),
    ("清空选择", "Clear selection"),
    ("清除", "Clear"),
    ("清除结果搜索", "Clear result search"),
    ("清除覆盖", "Clear override"),
    ("温度", "Temperature"),
    (
        "温度要在 0 到 2 之间",
        "Temperature must be between 0 and 2",
    ),
    ("点「同步」拉取文件夹。", "Press Sync to fetch folders."),
    (
        "点上方工具条的「下载正文」从服务器取一次。",
        "Use Download body in the toolbar above to fetch it from the server.",
    ),
    ("点击连接，铅笔进入编辑", "Click to connect; pencil to edit"),
    (
        "点列表条右侧的「写邮件」，写完点「存草稿」就会留在这里。",
        "Use \"Write\" on the list bar; a saved draft will show up here.",
    ),
    (
        "点右上角「刷新」读取 INFO。",
        "Click Refresh at the top right to read INFO.",
    ),
    (
        "点右上角「开始」，在一条专用连接上开启实时命令流。",
        "Click Start at the top right to stream live commands over a dedicated connection.",
    ),
    (
        "点右上角「编辑」，再点「添加面板」摆上图表、分隔标题或实例检查器。",
        "Click “Edit” at the top right, then “Add panel” to place a chart, a section title, or an instance inspector.",
    ),
    ("片段", "snippet"),
    ("版本", "Version"),
    ("版本、更新与发布记录", "Version, updates and release notes"),
    ("版本与当前生效主题", "Version and active theme"),
    ("版本发布记录", "Release notes"),
    ("状态", "Status"),
    ("状态…", "Status…"),
    ("独立进程", "Standalone process"),
    ("环境", "Environment"),
    ("环境标签", "Environment tag"),
    ("生产", "Prod"),
    (
        "生产档即使选「可写」也会默认锁写，需在状态栏限时解锁。",
        "Production connections stay write-locked even when marked writable; unlock from the status bar for a limited window.",
    ),
    ("生产环境", "Production"),
    (
        "生产连接默认锁写，请先在状态栏解锁（限时 15 分钟）",
        "Production connections are write-locked by default; unlock from the status bar first (15-minute window)",
    ),
    ("生成", "Generate"),
    ("生成 SELECT 语句", "Generate SELECT statement"),
    (
        "生成中断：请求线程提前退出",
        "Generation interrupted: the request thread exited early",
    ),
    ("生成删除语句", "Generate DROP statement"),
    ("生成参数", "Generation parameters"),
    ("生成参数已保存", "Generation parameters saved"),
    (
        "用 AI 分析这封邮件（M2 接入）",
        "Analyze with AI (coming in M2)",
    ),
    ("用「{}」处理", "Handle with \"{}\""),
    (
        "用于在账户列表里区分，也是默认登录名。",
        "Used to tell accounts apart; also the default username.",
    ),
    (
        "用左栏的「+」添加账户，再点「同步」拉取邮件。",
        "Add an account with + in the sidebar, then press Sync to fetch mail.",
    ),
    ("用户与权限", "Users and privileges"),
    ("用户名", "Username"),
    ("用户名为空。", "User name is empty."),
    (
        "用自然语言描述要查什么…",
        "Describe what you want to query…",
    ),
    (
        "由 Cargo.lock 汇总，随程序内置，离线可查。",
        "Summarised from Cargo.lock and embedded in the app; readable offline.",
    ),
    (
        "画布四周的留白。0 = 跟随主题。",
        "Space around the canvas. 0 follows the theme.",
    ),
    (
        "画布文字的字号。跟随主题时随界面字号一起变。",
        "Font size of the canvas text. Following the theme tracks the UI font size.",
    ),
    ("界面", "Interface"),
    ("界面密度", "Interface density"),
    (
        "界面显示语言；切换后立即生效，界面文案与提示都会跟随。",
        "Display language; takes effect immediately across the interface.",
    ),
    ("界面缩放", "UI scale"),
    ("留空则使用 8 层", "Leave blank to use 8 levels"),
    ("留空则使用 :", "Leave empty for :"),
    (
        "留空则使用设置中的默认值",
        "Leave empty for the settings default",
    ),
    ("留空则打开 0 号库", "Leave empty to open database 0"),
    ("留空则用模板名", "Blank uses the template name"),
    ("留空则用邮箱地址。", "Blank uses the email address."),
    (
        "留空则连接时在终端里输入",
        "leave blank to enter it in the terminal when connecting",
    ),
    (
        "留空则连跳板机本机",
        "Leave empty to reach the jump host itself",
    ),
    (
        "留空即用内置默认源（GitHub Releases）。",
        "Leave empty to use the built-in default (GitHub Releases).",
    ),
    (
        "留空并确定即恢复「跟随主题」。",
        "Leave blank and confirm to follow the theme again.",
    ),
    (
        "留空并确定即恢复「跟随引擎」。",
        "Leave blank and confirm to follow the engine again.",
    ),
    (
        "留空或 0 表示不自动展开",
        "Leave blank or 0 to never auto-expand",
    ),
    ("留空用 22", "Leave empty for port 22"),
    ("留空用 6379", "Leave empty for 6379"),
    (
        "留空用 aws 配置里的区域",
        "blank uses the region from your aws config",
    ),
    (
        "留空用 aws 默认 profile",
        "blank uses the default aws profile",
    ),
    (
        "留空用密钥认证 / 保持既有",
        "Leave empty for key auth / keep existing",
    ),
    (
        "留空用引擎当前命名空间",
        "blank uses the engine's current namespace",
    ),
    ("留空用缺省端口", "Leave empty for the default port"),
    (
        "留空用进程默认目录",
        "Leave empty to use the process working directory",
    ),
    (
        "留空用默认 {} 秒",
        "Leave empty to use the default {} seconds",
    ),
    ("留空用默认端口", "blank uses the default port"),
    ("留空表示不需要认证", "blank means no authentication"),
    (
        "留空表示保持既有 / 不需要认证",
        "blank keeps the existing one / no authentication",
    ),
    (
        "留空表示保持既有 key 不变",
        "Leave empty to keep the existing key",
    ),
    (
        "留空表示保持既有口令不变",
        "Leave empty to keep the existing password",
    ),
    (
        "留空表示只用口令认证",
        "Leave empty to authenticate by password only",
    ),
    ("留空表示实例本机", "blank means the instance itself"),
    ("留空表示跳板机本机", "Leave empty for the bastion itself"),
    ("留空跟随连接默认", "Blank follows the connection default"),
    ("留空跟随默认", "Blank follows the default"),
    ("登录名", "Username"),
    ("登录名（多为邮箱地址）", "Username (usually the address)"),
    (
        "登录系统后自动运行 Duck。",
        "Run Duck automatically after signing in.",
    ),
    ("目录不存在。", "Folder not found."),
    ("目录浏览", "Browse directories"),
    ("目标实例", "Target instance"),
    ("目标环境", "Target environment"),
    (
        "直接填模型名；从「预置模型」里挑一个会覆盖当前值。",
        "Type the model name directly; picking from 「Preset models」 overwrites the current value.",
    ),
    (
        "直接搜邮箱地址，自动按「发件人或收件人」匹配，不用写 from: / to:",
        "Search an email address directly — it matches sender or recipients, no from: / to: needed",
    ),
    ("直连", "Direct"),
    (
        "直连：没有配置隧道 / 代理。",
        "Direct: no tunnel or proxy configured.",
    ),
    ("直链", "direct link"),
    ("直链优先", "Direct link first"),
    (
        "相对跳板机解析；留空则连跳板机本机",
        "Resolved from the jump host; leave empty to reach the jump host itself",
    ),
    ("看图表", "View chart"),
    ("看板", "Board"),
    ("看表格", "View table"),
    ("知识库检索", "Search the knowledge base"),
    ("知道了", "Got it"),
    ("确定", "OK"),
    ("确定删除这份传输档案吗？", "Delete this transport profile?"),
    ("确认执行", "Confirm"),
    ("确认重置？", "Confirm reset?"),
    ("离线", "Offline"),
    (
        "私钥只在本机读取、只用于签名，不会被发送，也不写入配置文件。",
        "The key is read locally and used only to sign; it is never sent or written to config.",
    ),
    ("私钥文件", "Private key"),
    ("秒", "s"),
    (
        "秒数必须大于 0；要永不过期请用「移除过期时间」",
        "Seconds must be greater than 0; use Remove expiry for a key that never expires",
    ),
    ("秒数，如 3600", "Seconds, e.g. 3600"),
    ("移入项目…", "Move to project…"),
    ("移动到", "Move to"),
    ("移动到文件夹", "Move to folder"),
    ("移除", "Remove"),
    ("移除当前账户", "Remove current account"),
    ("移除账户失败：{}", "Could not remove the account: {}"),
    ("移除过期时间", "Remove expiry"),
    ("移除这条条件", "Remove this condition"),
    ("移除邮箱账户", "Remove mail account"),
    ("移除附件", "Remove attachment"),
    ("移除面板", "Remove panel"),
    ("稍后提醒（M2 接入）", "Snooze (coming in M2)"),
    ("空值", "Nulls"),
    ("空格 × 2", "Spaces \u{00d7} 2"),
    ("空格 × 4", "Spaces \u{00d7} 4"),
    ("空闲 {} 秒", "idle {}s"),
    ("窗口", "Window"),
    (
        "窗口右上角的提示条。无论哪一档，通知都会记入历史（顶栏铃铛）。",
        "The toast strip at the top-right of the window. At any level, notifications are still recorded in history (the bell in the top bar).",
    ),
    (
        "窗口最左侧的功能轨（概览 / 工具箱 / 设置…）。关闭后内容区占满窗口。",
        "The rail on the far left of the window (Overview / Toolbox / Settings…). When off, content fills the window.",
    ),
    ("立即", "Immediately"),
    ("立即清理", "Clean up now"),
    ("端口", "Port"),
    ("端口不能为 0。", "Port cannot be 0."),
    ("端口不能为空。", "Port is required."),
    (
        "端口必须是 1-65535 的数字",
        "Port must be a number between 1 and 65535",
    ),
    ("端口转发", "Port forwarding"),
    ("端点", "Endpoint"),
    ("第 {} / {} 页", "Page {} / {}"),
    ("第 {} 步，共 {} 步", "Step {} of {}"),
    ("第 {} 行", "Row {}"),
    ("第 {} 行第 {} 列", "Line {}, column {}"),
    ("第 {} 页 · 本页 {} 行", "Page {} · {} rows on this page"),
    (
        "第 {} 页 · 本页 {} 行 · 已过滤",
        "Page {} · {} rows on this page · filtered",
    ),
    ("第 {} 页 · 每页 {} 行", "Page {} · {} rows per page"),
    (
        "第 {} 页 · 每页 {} 行 · 已应用 {} 个条件",
        "Page {} · {} rows per page · {} condition(s) applied",
    ),
    (
        "第一次启动时的三步向导，随时可以再看一遍。",
        "The three-step wizard shown on first launch; you can replay it anytime.",
    ),
    (
        "第一次重连前等待的秒数，之后每次翻倍。",
        "Seconds to wait before the first retry; doubles each time.",
    ),
    ("第一页", "First page"),
    ("第三方组件", "Third-party components"),
    ("第三方组件许可汇总", "Third-party license summary"),
    (
        "等宽字体的族名。留空 = 跟随主题。",
        "Monospace family name. Blank follows the theme.",
    ),
    ("筛选", "Filter"),
    ("筛选「{}」", "Filter “{}”"),
    ("筛选任务与项目…", "Filter tasks and projects…"),
    ("筛选对象…", "Filter objects…"),
    (
        "筛选（筛选器待接入）",
        "Filter (filter bar not wired up yet)",
    ),
    (
        "签名段 {} 字节（未校验签名，这里只解码展示）。",
        "Signature segment is {} bytes (not verified, decoded for display only).",
    ),
    ("简体中文", "简体中文"),
    ("简约", "Minimal"),
    ("类别", "Category"),
    ("类型", "Type"),
    ("粘贴", "Paste"),
    ("系统", "System"),
    ("系统代理", "System proxy"),
    (
        "系统拒绝了这个组合，多半已被其他程序占用。",
        "The system refused this combination, so another program most likely owns it.",
    ),
    ("系统提示词", "System prompt"),
    ("系统通知", "System notification"),
    ("系统默认浏览器", "System default browser"),
    ("索引", "Indexes"),
    ("索引失败：{}", "Indexing failed: {}"),
    ("索引完成", "Indexing finished"),
    ("索引线程意外结束", "The indexing thread ended unexpectedly"),
    ("紧凑", "Compact"),
    ("紧急", "Urgent"),
    ("累计崩溃", "Crashes"),
    ("累计执行", "Total runs"),
    ("累计用量", "Session totals"),
    ("约 {} tokens", "About {} tokens"),
    ("约 {} 行", "~{} rows"),
    ("纯文本优先", "Plain text first"),
    ("终端", "terminal"),
    ("终端字号", "Terminal font size"),
    ("终端（分屏）", "terminal (split)"),
    ("经 AWS SSM", "Via AWS SSM"),
    ("经 SSH 隧道", "Via SSH tunnel"),
    ("经代理", "Via proxy"),
    ("结果 {}", "Result {}"),
    ("结果 · {} 行", "Result · {} rows"),
    (
        "结果已达行数上限，仅展示前一部分；可加 LIMIT 或导出 CSV。",
        "Row cap reached, showing the first part only; add LIMIT or export CSV.",
    ),
    ("结果行数上限", "Result row cap"),
    (
        "给连接列表着色分组，防「把生产库当测试库」。",
        "Colors and groups the connection list so a production database never masquerades as a test one.",
    ),
    ("统计", "Statistics"),
    ("继续加载", "Load more"),
    (
        "编译 Windows 资源（duck.ico）失败",
        "failed to compile Windows resources (duck.ico)",
    ),
    ("编辑", "Edit"),
    ("编辑 {} 连接", "Edit {} connection"),
    ("编辑中", "Editing"),
    (
        "编辑中：面板可以改宽、前挪后挪、换内容。",
        "Editing: panels can be resized, moved, and re-pointed.",
    ),
    (
        "编辑器下拉里保留多少条历史 SQL。",
        "How many past SQL statements the editor dropdown keeps.",
    ),
    (
        "编辑器下方提示解析报错与目录里不存在的表。",
        "Shows parse errors and unknown tables below the editor.",
    ),
    ("编辑器里还没有 SQL。", "The editor has no SQL yet."),
    ("编辑字符串值", "Edit string value"),
    ("编辑当前账户", "Edit current account"),
    ("编辑态必有 id", "editing state must have an id"),
    ("编辑服务器", "Edit Server"),
    ("编辑连接", "Edit connection"),
    ("编辑邮箱账户", "Edit mail account"),
    ("编辑重发", "Edit and resend"),
    ("编辑：{}", "Edit: {}"),
    ("缩放", "Zoom"),
    (
        "缩放尺寸、间距、图标与文字档位。",
        "Scales sizes, spacing, icons and text tiers.",
    ),
    ("缩进", "Indent"),
    ("网盘", "netdisk"),
    ("网盘直链解析", "Cloud drive direct links"),
    ("网络", "Network"),
    ("耗时", "Elapsed"),
    ("能力", "Capability"),
    ("能力声明", "Capabilities"),
    ("能力授权", "Capability grants"),
    ("脚本", "Script"),
    ("脚本文件", "Script file"),
    ("脚本文件的绝对路径", "Absolute path to the script file"),
    ("自动", "Auto"),
    ("自动刷新", "Auto refresh"),
    ("自动备份位置", "Automatic backup folder"),
    ("自动展开阈值", "Auto-expand threshold"),
    ("自动换行", "Word wrap"),
    (
        "自动探测编码，处理多分卷",
        "Detects encoding and handles multi-part archives",
    ),
    ("自动提交", "Auto-commit"),
    (
        "自动提交 · 点击开启事务",
        "Autocommit · click to start a transaction",
    ),
    ("自动检测", "Auto detect"),
    (
        "自动检测失败时，可手动指定 UTF-8 或 GBK。",
        "Specify UTF-8 or GBK manually when auto-detection fails.",
    ),
    ("自动重连中…", "Reconnecting…"),
    (
        "自动重连最多尝试几次；用尽后只保留手动重连。",
        "How many auto-reconnect attempts to make; manual reconnect remains after that.",
    ),
    ("自定义…", "Customize…"),
    ("自定义分组…", "Custom group…"),
    ("自定义强调色", "Custom accent"),
    ("自定义行数", "Custom row count"),
    ("自定义限速", "Custom limit"),
    ("自定义（用下面的配置）", "Custom (use the entries below)"),
    ("至少要保留一列可见。", "Keep at least one column visible."),
    (
        "至少要按一个修饰键：⌥ / ⌃ / ⌘（Windows 上是 Alt / Ctrl / Win）。",
        "At least one modifier is required: ⌥ / ⌃ / ⌘ (Alt / Ctrl / Win on Windows).",
    ),
    ("英文", "English"),
    ("范围：{}", "Scope: {}"),
    ("草稿", "Drafts"),
    ("草稿已保存", "Draft saved"),
    ("草稿已删除", "Draft deleted"),
    ("草稿箱还没有草稿", "No drafts yet"),
    ("草稿：{}，发给 {}，{}", "Draft: {}, to {}, {}"),
    (
        "草稿：{}，发给 {}，{}，带 {} 个附件",
        "Draft: {}, to {}, {}, with {} attachments",
    ),
    ("获取中…", "Fetching…"),
    (
        "获取插件清单失败：{}",
        "Failed to fetch the plugin list: {}",
    ),
    ("行", "rows"),
    ("行为", "Behavior"),
    ("行号", "Row"),
    ("行已删除", "Row deleted"),
    ("行已删除。", "Row deleted."),
    ("行改动已保存", "Row changes saved"),
    ("行数", "Row count"),
    (
        "行数统计仍是整表口径",
        "Row counts still cover the whole table",
    ),
    ("行模式", "Row view"),
    ("行首 / 行尾 ⌘← / ⌘→", "Line start/end ⌘← / ⌘→"),
    ("行高", "Line height"),
    (
        "行高占字号的百分比。0 = 跟随主题。",
        "Line height as a percentage of the font size. 0 follows the theme.",
    ),
    ("表", "Tables"),
    ("表浏览", "Table browser"),
    (
        "表浏览与分页查询一次取多少行。",
        "How many rows table browsing and paged queries fetch at a time.",
    ),
    (
        "表浏览以分页方式读取数据，每页 200 行。",
        "Table browser pages through data, 200 rows per page.",
    ),
    ("要写入该列的新值", "New value for this column"),
    ("视图", "View"),
    ("视频统一解析", "Unified video parsing"),
    ("解压", "extract"),
    ("解压后删除源文件", "Delete source after extracting"),
    (
        "解压后超过 {} 字节，已停止（避免解压炸弹）。",
        "Inflated output exceeds {} bytes; stopped to avoid a decompression bomb.",
    ),
    ("解析", "parse"),
    (
        "解析引擎会复用本机浏览器已登录的会话，因此会员内容也能取到直链。",
        "The parser reuses your local browser session, so member-only content resolves too.",
    ),
    ("解析顺序", "Resolution order"),
    (
        "解码结果不是文本，下面是前 {} 字节的十六进制。",
        "The decoded payload is not text; the first {} bytes are shown in hex.",
    ),
    ("解释器", "Interpreter"),
    (
        "解释器，例如 bash / python3",
        "Interpreter, e.g. bash / python3",
    ),
    ("解锁写操作（{} 分钟）", "Unlock writes ({} min)"),
    ("触发器", "Triggers"),
    (
        "计划未能结构化，按原样文本展示",
        "Plan could not be structured; showing the raw output",
    ),
    ("计划树", "Plan tree"),
    (
        "认不出的写法会当成普通关键词原样搜索，不会报错。",
        "Unrecognised syntax is searched as a plain keyword, with no error.",
    ),
    (
        "认不出这个连接档案；支持本应用导出的档案、DBeaver 的 data-sources.json 与 TablePlus 的 .tableplusconnection。",
        "Unrecognised connection file; this app's own export, DBeaver's data-sources.json, and TablePlus's .tableplusconnection are supported.",
    ),
    ("认证方式", "Authentication"),
    (
        "让远端主机也能使用本机 ssh-agent 中的密钥（从跳板机再连别的机器时用）。开启后远端可以以你的身份访问你能访问的主机，默认关闭。",
        "Let the remote host use the keys in your local ssh-agent (handy when hopping through a bastion to reach other machines). While enabled, the remote can reach whatever hosts you can, as you. Off by default.",
    ),
    ("记住口令", "Remember password"),
    ("设为", "Set to"),
    ("设为默认", "Set as default"),
    ("设置", "Settings"),
    ("设置失败：{}", "Failed to set: {}"),
    ("设置窗口", "Settings window"),
    ("设置过期时间", "Set expiry"),
    ("设置项", "Settings"),
    ("识别粘贴", "Smart paste"),
    ("诊断", "Diagnostics"),
    ("诊断信息", "Diagnostics"),
    ("诊断操作", "Diagnostic actions"),
    ("试一下", "Try it"),
    ("试跑", "Test run"),
    (
        "该主机的密钥已被标记为吊销（~/.ssh/known_hosts 中的 @revoked），OpenSSH 已拒绝连接。请勿直接删除记录，先与服务提供方确认新密钥后再更新 ~/.ssh/known_hosts。",
        "This host's key is marked as revoked (@revoked in ~/.ssh/known_hosts), so OpenSSH refused the connection. Do not simply delete the entry; confirm the new key with the server operator before updating ~/.ssh/known_hosts.",
    ),
    (
        "该工具已被移除，或开发者模式已关闭。",
        "This tool has been removed, or developer mode is off.",
    ),
    (
        "该工具暂无独立设置。",
        "This tool has no settings of its own yet.",
    ),
    (
        "该工具没有提供帮助内容。",
        "This tool provides no help content.",
    ),
    (
        "该工具的操作区域尚未接入，后端就绪后在此展示。",
        "This tool's workspace is not wired up yet; it will appear here once the backend is ready.",
    ),
    ("该插件没有工具贡献", "This plugin contributes no tools"),
    (
        "该操作会直接在远端服务器上执行，不可撤销。",
        "This runs directly on the remote server and cannot be undone.",
    ),
    (
        "该数据库类型的支持即将到来。",
        "Support for this database type is coming soon.",
    ),
    (
        "该类型暂无可用的结构化编辑入口。",
        "No structured editor is available for this type yet.",
    ),
    (
        "该表没有主键，无法定位行。",
        "This table has no primary key; rows cannot be located.",
    ),
    (
        "该表没有主键，暂不支持编辑。",
        "This table has no primary key; editing is unavailable.",
    ),
    ("该表没有外键。", "This table has no foreign keys."),
    ("该表没有索引。", "This table has no indexes."),
    ("该表没有触发器。", "This table has no triggers."),
    (
        "该语句未通过只读闸门，已拒绝自动执行。",
        "This statement did not pass the read-only gate; auto-execution was refused.",
    ),
    (
        "该语句未通过只读闸门，已拒绝自动执行（请人工审阅后再手动执行）。",
        "This statement did not pass the read-only gate; auto-execution was refused (review it and run it manually).",
    ),
    (
        "该连接已设为只读，请在连接设置里关闭只读开关",
        "This connection is marked read-only; turn the read-only switch off in its settings",
    ),
    (
        "该连接未设默认，回落全局 {} 秒。",
        "This connection has no default; falling back to the global {}s.",
    ),
    (
        "该连接的写入策略是「只读」，请在连接设置里改",
        "This connection's write policy is \"Read-only\"; change it in the connection settings",
    ),
    ("该连接默认 {} 秒。", "This connection defaults to {}s."),
    (
        "该键本来就没有过期时间",
        "This key has no expiry to begin with",
    ),
    ("详情", "Details"),
    ("语句", "Statement"),
    ("语言", "Language"),
    (
        "语言与明暗都能随时在「设置 → 外观」里改。",
        "Language and light/dark can be changed anytime in Settings → Appearance.",
    ),
    (
        "请先勾选要处理的行，或该表没有主键。",
        "Select the rows to work on first, or this table has no primary key.",
    ),
    (
        "请先勾选要处理的行；当前结果集不可编辑（需要单表 + 主键）。",
        "Select the rows to work on first; this result set is not editable (it needs a single table with a primary key).",
    ),
    (
        "请先在文件列表里选中一个文件。",
        "Select a file in the list first.",
    ),
    (
        "请先在文件列表里选中一项。",
        "Select an item in the file list first.",
    ),
    (
        "请先填参数名（点列表中一项可回填）",
        "Enter a parameter name first (click a row to fill it in).",
    ),
    ("请先输入 SQL。", "Enter some SQL first."),
    ("请先输入问题。", "Enter a question first."),
    ("请先连接一个数据库。", "Connect to a database first."),
    ("请先选择要修改的列。", "Pick the column to change first."),
    (
        "请先选择要过滤的列。",
        "Select a column to filter on first.",
    ),
    ("请填写 IMAP 服务器地址。", "Enter the IMAP server."),
    ("请填写 SMTP 服务器地址。", "Enter the SMTP server."),
    ("请填写主机地址。", "Enter a host."),
    ("请填写仪表板名称。", "Please enter a dashboard name."),
    ("请填写分组名。", "Enter a group name."),
    ("请填写分隔标题。", "Please enter a section title."),
    ("请填写口令。", "Enter the password."),
    ("请填写图表名称。", "Please enter a chart name."),
    ("请填写整数秒数", "Enter a whole number of seconds"),
    ("请填写查询名称。", "Enter a query name."),
    ("请填写至少一个收件人。", "Enter at least one recipient."),
    (
        "请填写过滤值（要匹配空值请用「为空」。）",
        "Enter a filter value (use the \"is null\" operator to match empty values).",
    ),
    ("请填写连接名称。", "Enter a connection name."),
    (
        "请填写邮箱地址（需含 @）。",
        "Enter an email address (must contain @).",
    ),
    ("请填写键名", "Enter a key name"),
    ("请求", "request"),
    (
        "请输入一个行数（正整数）。",
        "Enter a row count (positive integer).",
    ),
    ("请输入要打开的目录路径。", "Enter a folder path to open."),
    ("请选择私钥文件。", "Choose a private key file."),
    (
        "读不出知识库索引：{}",
        "Cannot read the knowledge base index: {}",
    ),
    ("读写", "Read-write"),
    ("读写档", "Read/write mode"),
    ("读取失败：{}", "Could not read: {}"),
    (
        "读取工作目录里的文本文件",
        "Read text files in the workspace",
    ),
    ("读取文件", "Read files"),
    ("读取文件失败：{}", "Failed to read the file: {}"),
    ("读取文件夹失败：{}", "Could not read folders: {}"),
    (
        "读取服务端信息失败：{}",
        "Failed to read server information: {}",
    ),
    ("读取标签失败：{}", "Could not read labels: {}"),
    (
        "读取标签邮件失败：{}",
        "Could not read the messages for this label: {}",
    ),
    ("读取草稿失败：{}", "Could not read drafts: {}"),
    ("读取表数据失败：{}", "Failed to load table data: {}"),
    ("读取账户失败：{}", "Could not read accounts: {}"),
    (
        "读取邮件列表失败：{}",
        "Could not read the message list: {}",
    ),
    ("读取键失败：{}", "Failed to read key: {}"),
    (
        "调整筛选条件或清空搜索词。",
        "Adjust the filters or clear the search.",
    ),
    (
        "调整过滤串或点「重新扫描」。",
        "Adjust the filter pattern or click \"Rescan\".",
    ),
    ("调用插件", "Invoke plugin"),
    ("调用插件工具：{}", "Run plugin tool: {}"),
    ("账户", "Accounts"),
    ("账户 {}", "Account {}"),
    ("账户已更新", "Account updated"),
    (
        "账户已添加，点「同步」拉取邮件",
        "Account added — press Sync to fetch mail",
    ),
    ("账户已移除", "Account removed"),
    ("质量", "Quality"),
    (
        "起不来（命令或解释器不存在）",
        "Could not start (command or interpreter not found)",
    ),
    ("起始序号", "Start index"),
    ("起始退避", "Initial backoff"),
    ("超出 WIP", "Over WIP"),
    (
        "超出上限的旧记录会被自动删除，调小会立刻生效；要长期留证就把上限调大。",
        "Older entries past the limit are deleted automatically, and lowering the limit takes effect immediately; raise it to keep a longer record.",
    ),
    (
        "超出保留期的日志会被自动删除；日志按天滚动，一天一个文件。",
        "Logs older than the retention period are deleted automatically; logs roll daily, one file per day.",
    ),
    ("超时", "Timeout"),
    ("超时 {} 秒", "Timeout {}s"),
    ("超时（秒）", "Timeout (s)"),
    (
        "超过 {} 字节的部分未显示。",
        "Bytes beyond {} are not shown.",
    ),
    (
        "超过这个层数就不再切分；留空则使用 8 层",
        "Stop splitting below this many levels; leave blank to use 8",
    ),
    ("跟随主题", "Follow theme"),
    ("跟随引擎", "Follow the engine"),
    ("跟随标签", "Follow tag"),
    ("跟随第一条", "Follow the first entry"),
    ("跟随系统", "Follow system"),
    (
        "跟随系统时随 macOS 外观切换，切换即时生效。",
        "Follows the macOS appearance and switches instantly.",
    ),
    (
        "跟随系统时随系统语言切换。",
        "Follows the system language when set to system.",
    ),
    (
        "跟随系统，当前生效的是{}",
        "Following the system; currently {}",
    ),
    ("路径", "Path"),
    ("跳板机", "Bastion"),
    ("跳板机主机", "Bastion host"),
    ("跳板机用户", "Bastion user"),
    ("跳转列", "Jump to column"),
    ("跳过", "Skip"),
    ("转发 ssh-agent", "Forward ssh-agent"),
    (
        "转发参数不完整：请检查端口与目标地址。",
        "Incomplete forwarding settings: check the ports and target host.",
    ),
    (
        "转发监听端口不能为 0。",
        "Forwarding listen port cannot be 0.",
    ),
    ("转发目标", "Forward target"),
    ("转发目标地址为空。", "Forwarding target host is empty."),
    (
        "转发目标端口不能为 0。",
        "Forwarding target port cannot be 0.",
    ),
    ("转发（M2 接入）", "Forward (coming in M2)"),
    ("转换", "convert"),
    ("载入到编辑器", "Load into editor"),
    ("载入图表", "Load chart"),
    ("较上周 +12.4%", "+12.4% vs last week"),
    ("较上周 +3", "+3 vs last week"),
    ("较上周 -6.1%", "-6.1% vs last week"),
    (
        "输入 SQL，多条语句用分号分隔…",
        "Enter SQL; separate statements with semicolons…",
    ),
    ("输入 SSH 地址", "Enter SSH address"),
    ("输入 SSH 密码", "Enter SSH password"),
    ("输入 SSH 用户名", "Enter SSH username"),
    ("输入 {} / 输出 {}", "{} in / {} out"),
    ("输入名称", "Enter a name"),
    (
        "输入命令后回车执行；破坏性命令（FLUSHALL 等）会先弹确认框。",
        "Type a command and press Enter; destructive commands (e.g. FLUSHALL) ask for confirmation first.",
    ),
    (
        "输入命令，如 INFO server",
        "Enter a command, e.g. INFO server",
    ),
    ("输入密码", "Enter a password"),
    ("输入描述信息", "Enter a description"),
    (
        "输入本机目录，或从系统目录选择器里挑一个。",
        "Type a local folder, or pick one from the system picker.",
    ),
    (
        "输入用户名，仅用于 Redis 6.0+",
        "Enter a username, Redis 6.0+ only",
    ),
    (
        "输入远端服务器上的目录路径。",
        "Type a folder path on the remote server.",
    ),
    (
        "输出 token 上限；留空用服务端默认。",
        "Output token cap; blank uses the server default.",
    ),
    ("输出格式", "Output format"),
    ("过期时间", "Expiry"),
    ("过期时间已设置", "Expiry set"),
    ("过滤", "Filter"),
    ("过滤值…", "Filter value…"),
    (
        "过滤参数名，如 maxmemory*",
        "Filter parameters, e.g. maxmemory*",
    ),
    ("过滤日志…", "Filter logs…"),
    (
        "过滤键名，如 user:* （留空表示全部）",
        "Filter key names, e.g. user:* (empty means all)",
    ),
    ("过滤项目…", "Filter projects…"),
    ("运行", "Run"),
    (
        "运行一次应用就会在日志目录里留下 .log。",
        "Running the app leaves a .log in the log directory.",
    ),
    ("运行平台", "Platform"),
    (
        "运行模式（单机 / 哨兵 / 集群）在 M1.6 接入，当前仅支持单机直连。",
        "Run modes (standalone / sentinel / cluster) land in M1.6; only standalone direct connections are supported for now.",
    ),
    ("运行环境", "Environment"),
    ("运行钩子", "Run hook"),
    ("返回", "Back"),
    ("返回列表", "Back to list"),
    ("还没有仪表板。", "No dashboards yet."),
    ("还没有会话", "No conversations yet"),
    ("还没有保存的图表。", "No saved charts yet."),
    ("还没有保存的查询。", "No saved queries yet."),
    (
        "还没有可用的模型厂商，先到「设置 → AI」添加一个",
        "No model provider is available yet — add one under Settings → AI first",
    ),
    ("还没有执行过语句。", "No statements run yet."),
    (
        "还没有数据，点右上角「刷新」。",
        "No data yet; click “Refresh” at the top right.",
    ),
    ("还没有标签", "No labels yet"),
    ("还没有标签。", "No tags yet."),
    (
        "还没有标签，点下面的「新建标签」。",
        "No labels yet, click \"New label\" below.",
    ),
    ("还没有索引任何目录。", "No directory has been indexed yet."),
    ("还没有记录", "No records yet"),
    (
        "还没有读取实例信息，点右上角「刷新」重试。",
        "Instance info has not been loaded; click Refresh at the top right to retry.",
    ),
    ("还没有选中的单元格", "No cell selected yet"),
    ("还没有邮件", "No messages yet"),
    ("还没有邮箱账户", "No mail account yet"),
    ("还没有配置任何厂商", "No provider configured"),
    (
        "还没有配置厂商。要用云端模型，先点上面的按钮选一个模板并填入 API key；只想本地跑就选 Ollama。",
        "No provider configured yet. For a cloud model, use the button above to pick a template and fill in the API key; to run locally, pick Ollama.",
    ),
    ("还没有配置模型厂商", "No model provider configured yet"),
    ("还没有钩子。", "No hooks yet."),
    (
        "还没选数值列；勾一个上面列出的数值列就会出图。",
        "No value column selected yet; tick one listed above to draw the chart.",
    ),
    (
        "这一列没有可画的数值。",
        "This column has no plottable values.",
    ),
    (
        "这一格已经不在当前结果里",
        "This cell is no longer in the current result",
    ),
    (
        "这一步是可选的，之后可以在左边的插件中心随时回来。",
        "This step is optional; you can come back to the plugin center anytime.",
    ),
    (
        "这一行将从数据库中删除，确定继续吗？",
        "This row will be deleted from the database. Continue?",
    ),
    ("这个文件夹还没有邮件", "No messages in this folder yet"),
    (
        "这个目录里没有可显示的条目。",
        "No entries to show in this folder.",
    ),
    (
        "这些文件都已经在附件里了",
        "Those files are already attached.",
    ),
    (
        "这份结果没有可转置的行",
        "This result has no row to transpose",
    ),
    (
        "这份结果没有可预览的行",
        "This result has no row to preview",
    ),
    ("这份结果还没有图表", "No chart for this result yet"),
    ("这份结果里没有数值列", "This result has no numeric column."),
    (
        "这份结果里没有数值列，画不出图。",
        "This result has no numeric column, so there is nothing to chart.",
    ),
    (
        "这台服务器没报出可画的数值指标。",
        "This server reports no plottable numeric metrics.",
    ),
    ("这块仪表板已经不在了。", "This dashboard is gone."),
    ("这块仪表板还是空的", "This dashboard is empty"),
    (
        "这封邮件的正文还没下载到本地。",
        "The body of this message has not been downloaded yet.",
    ),
    ("这是一条应用内提示", "This is an in-app toast"),
    ("这是一条系统通知", "This is a system notification"),
    (
        "这条回答前面没有可重发的提问",
        "There is no question to resend before this answer",
    ),
    (
        "这条记录已经不在归档区里了。",
        "This record is no longer in the archive.",
    ),
    (
        "这条语句没有执行计划。",
        "This statement has no execution plan.",
    ),
    (
        "这条语句没有返回结果集。",
        "This statement returned no result set.",
    ),
    (
        "这条连接没有配置钩子。",
        "This connection has no hooks configured.",
    ),
    (
        "这条连接配了钩子，还没跑过。",
        "This connection has hooks configured, but none have run yet.",
    ),
    (
        "这条钩子还没填完，先补上命令或脚本。",
        "This hook is incomplete; fill in the command or script first.",
    ),
    (
        "这段 JWT 没有签名段。",
        "This JWT has no signature segment.",
    ),
    (
        "这段 SQL 包含删库 / 全表清改类语句，确定继续吗？",
        "This SQL contains drop-database or full-table wipe statements. Continue?",
    ),
    ("这里还没有任务", "No tasks yet"),
    ("进度", "Progress"),
    ("进程列表", "Process list"),
    ("进程执行", "Process execution"),
    ("进行中", "Running"),
    ("进行中 {}", "{} in progress"),
    (
        "远程图片已显示，发件人可能由此得知你已读过这封信。",
        "Remote images are shown; the sender may learn that you read this message.",
    ),
    ("远程转发", "Remote forward"),
    ("远端", "Remote"),
    ("远端主机", "Remote host"),
    (
        "远端命令失败（退出码 {}）。",
        "Remote command failed (exit code {}).",
    ),
    (
        "远端命令失败（退出码 {}）：{}",
        "Remote command failed (exit code {}): {}",
    ),
    (
        "远端命令被信号终止。",
        "Remote command was terminated by a signal.",
    ),
    (
        "远端命令被信号终止：{}",
        "Remote command was terminated by a signal: {}",
    ),
    ("远端目录", "Remote folder"),
    (
        "远端目录不存在或无法进入：{}",
        "The remote directory does not exist or cannot be entered: {}",
    ),
    (
        "远端目录尚未确定，请先浏览一次远端。",
        "The remote directory is not known yet; browse the remote side first.",
    ),
    ("远端系统", "Remote OS"),
    (
        "远端路径区分大小写，需以 / 开头。",
        "Remote paths are case-sensitive and start with /.",
    ),
    (
        "远端输出过长，列表已截断。",
        "Remote output was too long; the listing was truncated.",
    ),
    (
        "远端连接尚未建立，无法传输。",
        "The remote connection is not established; cannot transfer.",
    ),
    (
        "远端连接尚未建立，无法操作。",
        "The remote connection is not established; cannot perform this action.",
    ),
    ("连带任务", "Tasks archived with it"),
    ("连接", "Connections"),
    ("连接 / 响应超时", "Connect / response timeout"),
    (
        "连接 SQLite / PostgreSQL / MySQL，浏览表、执行 SQL、编辑数据",
        "Connect to SQLite / PostgreSQL / MySQL; browse tables, run SQL, edit data",
    ),
    ("连接 {} 失败：{}", "Failed to connect {}: {}"),
    (
        "连接 {} 未能建立。",
        "Connection {} could not be established.",
    ),
    ("连接 {}（{}）", "Connect {} ({})"),
    ("连接 {}（{}）失败：{}", "Connect {} ({}) failed: {}"),
    ("连接 {}：{}", "Connect {}: {}"),
    (
        "连接、执行语句、写入、浏览与插件调用都会记在这里（最多保留 {} 条）。",
        "Connections, statements, writes, table browses, and plugin runs are recorded here (up to {} entries).",
    ),
    (
        "连接、查询、写入这类事件各自成一行流水。",
        "Connections, queries, and writes each get their own line.",
    ),
    (
        "连接、查询与写入的流水",
        "Connection, query, and write activity",
    ),
    ("连接中", "Connecting"),
    ("连接信息", "Connection details"),
    ("连接名称", "Connection name"),
    (
        "连接后默认浏览的 schema / 库；留空用引擎当前命名空间。",
        "The schema/database browsed by default after connecting; blank uses the engine's current namespace.",
    ),
    ("连接失败", "Connection failed"),
    ("连接失败。", "Connection failed."),
    ("连接失败：{}", "Connection failed: {}"),
    ("连接已保存", "Connection saved"),
    ("连接已保存。", "Connection saved."),
    ("连接已删除", "Connection deleted"),
    ("连接已删除。", "Connection deleted."),
    ("连接已断开。", "Connection closed."),
    ("连接已更新。", "Connection updated."),
    (
        "连接当前锁写，请先在状态栏解锁（限时 15 分钟）",
        "Writes are locked on this connection; unlock from the status bar first (15-minute window)",
    ),
    ("连接成功", "Connected"),
    ("连接成功（{}）", "Connected ({})"),
    (
        "连接成功，服务端报告 {} 个文件夹。",
        "Connected — the server reports {} folders.",
    ),
    ("连接成功：{}", "Connected: {}"),
    ("连接时长", "Uptime"),
    (
        "连接档案与其保存的口令都会被删除，确定继续吗？",
        "The connection profile and its stored password will both be deleted. Continue?",
    ),
    ("连接钩子", "Connection hooks"),
    ("连接锁不应中毒", "connection lock must not be poisoned"),
    ("退出", "Quit"),
    ("退出 Duck Toolbox", "Quit Duck Toolbox"),
    (
        "选一个服务商会自动填好服务器与端口；「自动识别」按邮箱地址判断。",
        "Picking a provider fills in the servers and ports; Auto-detect infers them from the email address.",
    ),
    (
        "选一家服务商会自动填好收发服务器与加密方式，下一步仍可修改。",
        "Picking a provider pre-fills the servers and security; everything stays editable in the next step.",
    ),
    (
        "选中一封邮件后，可以就它提问。",
        "Select a message to ask questions about it.",
    ),
    (
        "选中一封邮件后，这里给出回复草稿。",
        "Select a message to get a draft reply here.",
    ),
    (
        "选中一封邮件后，这里给出摘要与要点。",
        "Select a message to see a summary and key points here.",
    ),
    (
        "选中一张卡片查看详情。",
        "Select a card to see its details.",
    ),
    ("选中一条查看详情。", "Select a row to view its details."),
    ("选中执行", "Run Selection"),
    ("选中行", "Selected row"),
    ("选择", "Select"),
    ("选择一个键查看详情。", "Select a key to see its details."),
    ("选择一封邮件开始阅读", "Select a message to read"),
    ("选择列", "Select column"),
    ("选择外观", "Pick a look"),
    ("选择文件…", "Choose file…"),
    (
        "选择此电脑上可用的字体。字体不可用时回退到主题字体；新安装的字体可能需要重启应用后才能读取。",
        "Pick a font installed on this computer. Missing fonts fall back to the theme font; newly installed fonts may need an app restart.",
    ),
    ("选择目录", "Choose folder"),
    ("选择目录…", "Choose folder…"),
    ("选择要连接的目标。", "Choose what to connect to."),
    ("选择邮箱服务商", "Choose an email provider"),
    ("通用", "General"),
    ("通知", "Notifications"),
    (
        "通知、正文形态与列表条数",
        "Notifications, body format and list size",
    ),
    ("通知历史", "Notification history"),
    ("通知通道", "Channels"),
    (
        "通道已断开，请重新连接。",
        "Transport is down; reconnect to restore it.",
    ),
    ("通道正常。", "Transport is healthy."),
    ("通道状态", "Transport status"),
    ("逻辑库（0-{}）", "Database (0-{})"),
    ("逾期", "Overdue"),
    ("逾期 {}", "{} overdue"),
    ("邮件已发出", "Message sent"),
    ("邮件正文", "Message body"),
    ("邮件：{}，来自 {}，{}", "Message: {}, from {}, {}"),
    (
        "邮件：{}，来自 {}，{}（已加旗标）",
        "Message: {}, from {}, {} (flagged)",
    ),
    ("邮箱", "Email"),
    ("邮箱地址", "Email address"),
    ("邮箱数据库尚未打开", "The mailbox database is not open yet"),
    ("配置", "Configuration"),
    (
        "配置更新源并检查更新后，这里会显示源上版本的发布说明。",
        "Set an update source and check for updates to see the release notes published there.",
    ),
    ("配置源", "Configure source"),
    ("配色", "Colors"),
    ("重做", "Redo"),
    (
        "重启后运行上一版本。现在重启？",
        "The previous version runs after a restart. Restart now?",
    ),
    (
        "重启后运行新版本。现在重启？",
        "The new version runs after a restart. Restart now?",
    ),
    ("重启并应用更新", "Restart and apply the update"),
    ("重启并运行上一版本", "Restart into the previous version"),
    ("重命名", "rename"),
    ("重命名会话", "Rename conversation"),
    ("重命名失败：{}", "Rename failed: {}"),
    ("重命名键", "Rename key"),
    ("重建索引", "Rebuild index"),
    ("重新打开", "Reopen"),
    ("重新执行这条语句", "Run this statement again"),
    ("重新扫描", "Rescan"),
    ("重新查看引导", "Replay the wizard"),
    ("重新生成", "Regenerate"),
    ("重新连接", "Reconnect"),
    ("重置", "Reset"),
    ("重置失败：{}", "Reset failed: {}"),
    ("重置并删插件", "Reset and remove plugins"),
    ("重置应用设置", "Reset app settings"),
    ("重置终端字号", "Reset terminal font size"),
    ("重置设置", "Reset settings"),
    ("重置设置与插件", "Reset settings and plugins"),
    ("重试", "Retry"),
    (
        "重跑查询或重新点一格。",
        "Re-run the query or pick another cell.",
    ),
    ("重载", "Reload"),
    ("重连", "Reconnect"),
    ("重连上限", "Max retries"),
    ("钩子", "Hooks"),
    (
        "钩子「{}」失败，已中断本次操作：{}",
        "Hook \"{}\" failed and aborted this operation: {}",
    ),
    (
        "钩子只在「连接 / 断开」时跑，不会在测试连接时跑。",
        "Hooks run only on connect/disconnect, never on test connection.",
    ),
    (
        "锁定，每次解锁 15 分钟",
        "Locked, unlock for 15 minutes at a time",
    ),
    ("键", "Keys"),
    ("键不存在", "Key not found"),
    ("键值", "Key-value"),
    ("键分隔符", "Key separator"),
    ("键名", "Key name"),
    ("键已不存在", "Key no longer exists"),
    (
        "键已不存在（可能在选中之后被其他客户端删掉了）。",
        "The key no longer exists (another client may have deleted it after you selected it).",
    ),
    ("键已删除", "Key deleted"),
    ("键已重命名", "Key renamed"),
    ("键扫描与会话", "Key scanning and session"),
    (
        "键树按它切命名空间；留空则使用 :",
        "The key tree splits namespaces on it; leave empty for :",
    ),
    ("键树最大深度", "Max key tree depth"),
    (
        "键树最大深度 / 自动展开阈值",
        "Max key tree depth / auto-expand threshold",
    ),
    ("键浏览", "Key browsing"),
    ("键管理", "Key management"),
    ("键详情", "Key details"),
    ("长语句自动换行", "Wrap long statements"),
    (
        "问点什么…（Enter 发送，Shift+Enter 换行）",
        "Ask something… (Enter to send, Shift+Enter for a newline)",
    ),
    ("问答", "Ask"),
    ("阅读", "Reading"),
    ("阻塞", "Blocked"),
    ("附件", "Attachments"),
    (
        "附件随信一起编码发出；总体积超过服务器声明的上限时，会在点「发送」时被拒。",
        "Attachments are encoded and sent with the message; if the total size exceeds what the server allows, sending will be rejected.",
    ),
    ("降序", "descending"),
    ("隐藏 Duck Toolbox", "Hide Duck Toolbox"),
    ("隐藏其他", "Hide Others"),
    ("隐藏查询结果", "Hide query results"),
    ("隐藏远程图片", "Hide remote images"),
    ("集合", "Set"),
    ("需确认", "Needs approval"),
    ("需要 user@host 参数", "Requires a user@host argument"),
    ("需要密文参数", "Requires a ciphertext argument"),
    (
        "需要本机已装 aws CLI 并完成过 aws sso login / configure。",
        "Requires the aws CLI on this machine, already authenticated via aws sso login / configure.",
    ),
    ("需要重新登录", "Sign-in required"),
    (
        "静态 JSON manifest 的 https 地址。",
        "https URL of a static JSON manifest.",
    ),
    (
        "非法输入（例如温度 2.5、上限 0）会被拒绝，不落盘。",
        "Invalid input (say a temperature of 2.5 or a cap of 0) is rejected and never written.",
    ),
    ("面板", "Panel"),
    ("面积图", "Area chart"),
    ("页码", "Page"),
    (
        "顶栏的插件中心按钮与语言切换；关闭后二者一并隐藏。",
        "The plugin marketplace button and the language switch in the top bar. Turning this off hides both.",
    ),
    (
        "顶栏的通知中心入口；关闭后通知仍记入历史，只是顶栏不再显示铃铛。",
        "The notification center entry in the top bar. When off, notifications are still recorded in history; only the bell is hidden.",
    ),
    ("项目", "Project"),
    ("项目管理", "Projects"),
    ("预估工时", "Estimated hours"),
    ("预发布", "Staging"),
    ("预置模型", "Preset models"),
    ("饼图", "Pie chart"),
    (
        "饼图只吃正值，这一列全是 0 或负数。",
        "A pie chart only takes positive values; this column is all zeros or negatives.",
    ),
    ("首页", "Home"),
    ("驱动管理", "Driver Manager"),
    ("高", "High"),
    ("高级", "Advanced"),
    ("默认 8", "default 8"),
    ("默认 96000", "Default 96000"),
    ("默认分组", "Default group"),
    ("默认厂商", "Default provider"),
    ("默认只读", "Read-only by default"),
    ("默认命名空间", "Default namespace"),
    ("默认数据库（0-{}）", "Default database (0-{})"),
    ("默认正文形态", "Default body format"),
    ("默认环境标签", "Default environment tag"),
    ("默认超时", "Default timeout"),
    (
        "默认选 TLS；STARTTLS 用于 143 / 587。",
        "TLS is the default; STARTTLS is for 143 / 587.",
    ),
    ("（未设置）", "(not set)"),
];

/// 复数英译表：`(中文源串, 英文单数, 英文复数)`，按中文源串的 UTF-8 字节序升序
/// （与 [`EN`] 同一不变量，二分查找依赖它）。
///
/// 源串里用 `{}` 站**一个数量**。中文不区分单复数（源串本身即中文文案），英文按
/// `count == 1` 取单数档。**每条源串必须同时登记在 [`EN`]**——那里是不带数量的
/// 兜底形式，也是「中文文案必须全部登记」守卫的落点（由
/// `plural_entries_are_well_formed` 钉住）。
///
/// 只收「英文单数真的和复数不同形」的条目：`{} tokens` 这类（单复数同形）留在 [`EN`]
/// 就够，登记进来只会多一份要维护的重复。
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

    /// 表必须有序，否则二分查找会漏词。
    #[test]
    fn table_is_sorted() {
        for pair in EN.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "英译表顺序错误：{} 应在 {} 之后",
                pair[0].0,
                pair[1].0
            );
        }
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

    /// 复数表必须有序，否则二分查找会漏词（与 [`EN`] 同一不变量）。
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

    /// 复数表每条都要：源串登记在 [`EN`]、三列各含恰好一个 `{}`、单复数不同形。
    ///
    /// 三条都是「登记进来时必须成立」的不变量：源串不在 `EN` 会让「中文文案必须
    /// 全部登记」守卫报红；占位符数目不一致会让 `fill` 少替换或多留 `{}`；
    /// 单复数同形的条目没有存在意义（`EN` 那条已经够了）。
    #[test]
    fn plural_entries_are_well_formed() {
        for &(source, one, other) in EN_PLURAL {
            assert!(
                translate_en(source).is_some(),
                "复数表的源串 {source} 未登记在英译表"
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
