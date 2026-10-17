//! 快速新建的自然语言解析（原型 `parseQuick`，`docs/projects-mock.html:1969`）。
//!
//! 在一句话里认四类记号，其余文字就是标题：
//!
//! | 记号 | 例 | 语义 |
//! |---|---|---|
//! | `#词` | `#后端` | 标签（可多个） |
//! | `@词` | `@Alpha` | 项目名（取第一个） |
//! | `!P0`–`!P3` | `!P1` | 优先级（大小写不敏感） |
//! | 日期 | `2026-10-20` / `10-20` / `今天` / `明天` / `后天` | 截止日 |
//! | 时刻 | `18:00` | 截止时刻 |
//!
//! 解析是**纯函数**：`&str` 进、[`QuickParse`] 出，不碰库也不碰 gpui，所以可以在
//! 单测里把每类记号钉死。界面侧（Spotlight 弹窗）只负责把结果画成 chips 并落地。
//!
//! ⚠️ **Markdown 清单的逐行解析不在这里**，在 [`super::import`] —— 那是「导入」
//! 这件事的一环（三种来源之一），而这里只管「一句话里的记号」。两边共用的是
//! [`parse`] 本身，所以记号语法永远只有一套。

use duck_project::{CalendarDate, Priority};

/// 一句话的解析结果。`title` 是剥掉全部记号后的剩余文字（已去首尾空白）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QuickParse {
    /// 标题（剩余文字）。
    pub(crate) title: String,
    /// `#` 记号认出的标签名（按出现顺序，去重）。
    pub(crate) tags: Vec<String>,
    /// `@` 记号认出的项目名（取第一个）。
    pub(crate) project: Option<String>,
    /// `!P0`–`!P3` 认出的优先级。
    pub(crate) priority: Option<Priority>,
    /// 日期记号认出的截止日。
    pub(crate) due: Option<CalendarDate>,
    /// `HH:MM` 认出的截止时刻。
    pub(crate) due_time: Option<String>,
}

impl QuickParse {
    /// 有没有任何可落库的记号（纯标题 = `false`）。
    pub(crate) fn has_markers(&self) -> bool {
        self.project.is_some()
            || self.priority.is_some()
            || self.due.is_some()
            || self.due_time.is_some()
            || !self.tags.is_empty()
    }
}

/// 解析一句话。
pub(crate) fn parse(input: &str, today: CalendarDate) -> QuickParse {
    let mut out = QuickParse::default();
    let mut title_parts: Vec<&str> = Vec::new();

    for token in input.split_whitespace() {
        if let Some(name) = token.strip_prefix('#') {
            let name = name.trim();
            if !name.is_empty() && !out.tags.iter().any(|known| known == name) {
                out.tags.push(name.to_owned());
            }
            continue;
        }
        if let Some(name) = token.strip_prefix('@') {
            let name = name.trim();
            if !name.is_empty() && out.project.is_none() {
                out.project = Some(name.to_owned());
            }
            continue;
        }
        if let Some(priority) = token.strip_prefix('!').and_then(Priority::parse) {
            out.priority = Some(priority);
            continue;
        }
        if let Some(date) = parse_date(token, today) {
            out.due = Some(date);
            continue;
        }
        if let Some(time) = parse_time(token) {
            out.due_time = Some(time);
            continue;
        }
        title_parts.push(token);
    }

    out.title = title_parts.join(" ");
    out
}

/// [`QuickParse`] → 记号文本（[`parse`] 的**反向**渲染）。
///
/// 用途只有一个：**AI 兜底解析的结果写回 Spotlight 输入框**（见 `dialogs::run_ai_quick`）。
///
/// # 为什么写回文本，而不是在弹窗旁边另存一份结果
///
/// 输入框已经是这个弹窗的唯一真源 —— 预览 chips 与 `submit_spotlight` 都从它现读。
/// AI 结果若另存一份，立刻就是双真源：chips 显示 AI 那份、落库用输入框那份（或反过来），
/// 而两者在用户改一个字之后就再也对不上。写回文本之后，预览与提交自动跟着走，而且
/// 用户可以顺手改 AI 认错的那一个记号 —— 比只给一个只读摘要可控得多。
///
/// # 已知边界：标题里**恰好**长得像记号的词会被再解析一次
///
/// 记号语法没有转义符（`#x` 永远是标签），所以标题是 `2026-01-01 上线` 这种样子时，
/// 回填出去会被 [`parse`] 认成「截止日 + 标题『上线』」。两条防线：① prompt 里明确要求
/// 模型别把日期 / `#` / `@` / `!` 写进标题；② 回填后用户看到的就是 chips 的真实解析，
/// 一眼能发现并改掉。**刻意不加转义语法** —— 为了一个边角情况把整张记号表变复杂，
/// 换算下来不划算。
pub(crate) fn to_markers(parsed: &QuickParse) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(4 + parsed.tags.len());
    if !parsed.title.trim().is_empty() {
        parts.push(parsed.title.trim().to_owned());
    }
    if let Some(name) = parsed
        .project
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        parts.push(format!("@{name}"));
    }
    for name in &parsed.tags {
        let name = name.trim();
        if !name.is_empty() {
            parts.push(format!("#{name}"));
        }
    }
    if let Some(priority) = parsed.priority {
        parts.push(format!("!P{}", priority.as_i64()));
    }
    if let Some(due) = parsed.due {
        parts.push(due.to_iso());
    }
    if let Some(time) = parsed
        .due_time
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        parts.push(time.to_owned());
    }
    parts.join(" ")
}

/// 日期记号：`YYYY-MM-DD` / `MM-DD`（年份取今天所在年，跨年不回绕）/ `今天` /
/// `明天` / `后天`。
///
/// `pub(super)`：`ai_assist` 把模型给的 `due` 字段交给它，于是「哪些日期写法算数」
/// 全仓只有这一份 —— 不在 AI 那条路上再抄一遍 `CalendarDate::parse` + 相对词的匹配。
pub(super) fn parse_date(token: &str, today: CalendarDate) -> Option<CalendarDate> {
    match token {
        "今天" => return Some(today),
        "明天" => return Some(today.add_days(1)),
        "后天" => return Some(today.add_days(2)),
        _ => {}
    }
    if let Some(date) = CalendarDate::parse(token) {
        return Some(date);
    }
    // `MM-DD`：补上今天的年份；2-29 一类只在闰年合法的日期解析失败按没写处理。
    // （`CalendarDate` 没有暴露年读取器，年份从 ISO 文本取 —— 日历日本来就是文本。）
    let (month, day) = token.split_once('-')?;
    if token.matches('-').count() != 1 {
        return None;
    }
    let year: i32 = today.to_iso().get(..4).and_then(|year| year.parse().ok())?;
    CalendarDate::new(year, month.parse().ok()?, day.parse().ok()?)
}

/// 时刻记号：`HH:MM`，`00`–`23` 与 `00`–`59` 之外不认（`18:00` 可以，`99:99` 不行）。
///
/// `pub(super)` 的理由同 [`parse_date`]。
pub(super) fn parse_time(token: &str) -> Option<String> {
    let (hour, minute) = token.split_once(':')?;
    if hour.len() != 2 || minute.len() != 2 {
        return None;
    }
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(format!("{hour:02}:{minute:02}"))
}

#[cfg(test)]
mod tests {
    use super::{QuickParse, parse};
    use duck_project::{CalendarDate, Priority};

    fn today() -> CalendarDate {
        CalendarDate::new(2026, 10, 14).expect("合法日期")
    }

    #[test]
    fn plain_text_has_no_markers() {
        let parsed = parse("修复登录超时", today());
        assert_eq!(
            parsed,
            QuickParse {
                title: "修复登录超时".to_owned(),
                ..QuickParse::default()
            }
        );
        assert!(!parsed.has_markers());
    }

    #[test]
    fn every_marker_is_recognized_and_stripped() {
        let parsed = parse("修复登录超时 #后端 #网关 @Alpha !P1 明天 18:00", today());
        assert_eq!(parsed.title, "修复登录超时");
        assert_eq!(parsed.tags, ["后端", "网关"]);
        assert_eq!(parsed.project.as_deref(), Some("Alpha"));
        assert_eq!(parsed.priority, Some(Priority::P1));
        assert_eq!(
            parsed.due.map(|date| date.to_iso()),
            Some("2026-10-15".to_owned())
        );
        assert_eq!(parsed.due_time.as_deref(), Some("18:00"));
        assert!(parsed.has_markers());
    }

    #[test]
    fn iso_and_short_dates_both_work() {
        assert_eq!(
            parse("x 2026-11-02", today()).due.map(|date| date.to_iso()),
            Some("2026-11-02".to_owned())
        );
        assert_eq!(
            parse("x 11-02", today()).due.map(|date| date.to_iso()),
            Some("2026-11-02".to_owned()),
            "MM-DD 补今天所在年"
        );
        assert_eq!(parse("x 2-30", today()).due, None, "不合法的日期不硬造");
    }

    #[test]
    fn relative_words_map_to_today_plus_offset() {
        assert_eq!(parse("x 今天", today()).due, Some(today()));
        assert_eq!(parse("x 明天", today()).due, Some(today().add_days(1)));
        assert_eq!(parse("x 后天", today()).due, Some(today().add_days(2)));
    }

    #[test]
    fn duplicate_tags_and_projects_collapse() {
        let parsed = parse("x #a #a @p @q", today());
        assert_eq!(parsed.tags, ["a"]);
        assert_eq!(parsed.project.as_deref(), Some("p"));
    }

    #[test]
    fn bogus_markers_stay_in_the_title() {
        // `!P9` 不是优先级、`18:99` 不是时刻：留在标题里，不静默吞掉。
        let parsed = parse("修 !P9 的 18:99 东西", today());
        assert_eq!(parsed.title, "修 !P9 的 18:99 东西");
        assert_eq!(parsed.priority, None);
        assert_eq!(parsed.due_time, None);
    }

    #[test]
    fn to_markers_round_trips_through_parse() {
        let parsed = parse("修复登录超时 #后端 #网关 @Alpha !P1 明天 18:00", today());
        let text = super::to_markers(&parsed);
        assert_eq!(text, "修复登录超时 @Alpha #后端 #网关 !P1 2026-10-15 18:00");
        // 回填出去再解析一遍，必须与原结果逐字段相同 —— 这是「写回输入框」这条路
        // 能成立的全部依据（预览与落库都只看输入框）。
        assert_eq!(parse(&text, today()), parsed);
    }

    #[test]
    fn to_markers_drops_empty_pieces_and_keeps_a_bare_title() {
        let only_title = parse("只是标题", today());
        assert_eq!(super::to_markers(&only_title), "只是标题");

        // 空标题 + 只有记号时不留前导空格。
        let mut markers_only = QuickParse::default();
        markers_only.priority = Some(Priority::P0);
        markers_only.due = Some(today());
        assert_eq!(super::to_markers(&markers_only), "!P0 2026-10-14");

        // 全空的解析结果回填成空串（调用方据此判断「没解析出东西」）。
        assert_eq!(super::to_markers(&QuickParse::default()), "");
    }
}
