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

/// 日期记号：`YYYY-MM-DD` / `MM-DD`（年份取今天所在年，跨年不回绕）/ `今天` /
/// `明天` / `后天`。
fn parse_date(token: &str, today: CalendarDate) -> Option<CalendarDate> {
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
fn parse_time(token: &str) -> Option<String> {
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

/// 「从文本导入」里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportLine {
    /// 勾选态：`- [x]` / `- [X]` 为 `true`，`- [ ]` 与没有勾选框为 `false`。
    pub(crate) done: bool,
    /// 逐行解析的结果（标题已剥掉全部记号）。
    pub(crate) parsed: QuickParse,
}

/// 把一段粘贴文本按行解析成任务（原型 `openImport` 的 `rows()`，
/// `docs/projects-mock.html:2470` 一带）。
///
/// 语法与快速新建**同一套**（[`parse`]），额外认 Markdown 清单前缀：
///
/// | 写法 | 结果 |
/// |---|---|
/// | `- [ ] 修登录 #后端` | 待办 |
/// | `- [x] 写周报` / `- [X] 写周报` | 已完成 |
/// | `- 修登录` / `* 修登录` / `+ 修登录` | 待办（裸清单项） |
/// | `修登录` | 待办（光秃秃一行也算一条） |
///
/// 空行与纯空白行丢掉。**不含任何前缀的行照样成一条**：直接粘一串标题是最常见的
/// 用法，要求用户先加 `- ` 只会把人挡在门外。
pub(crate) fn parse_lines(input: &str, today: CalendarDate) -> Vec<ImportLine> {
    input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (done, body) = split_checkbox(line);
            ImportLine {
                done,
                parsed: parse(body, today),
            }
        })
        .collect()
}

/// 剥掉清单前缀，返回「勾选态 + 剩余文字」。
///
/// 认不出的括号（`[!]`）**不当勾选框**：整行原样留在标题里，免得把一个正常标题
/// 的方括号吃掉。
fn split_checkbox(line: &str) -> (bool, &str) {
    let Some(mark) = line.chars().next() else {
        return (false, line);
    };
    if !matches!(mark, '-' | '*' | '+') {
        return (false, line);
    }
    // 标记本身是 ASCII，切一个字节安全；后面的空白（`-   x` 也认）一并吃掉。
    let rest = line[mark.len_utf8()..].trim_start();
    let Some(after_bracket) = rest.strip_prefix('[') else {
        return (false, rest);
    };
    let Some((inside, after)) = after_bracket.split_once(']') else {
        return (false, rest);
    };
    let done = match inside.trim() {
        "" => false,
        "x" | "X" => true,
        _ => return (false, rest),
    };
    (done, after.trim_start())
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
    fn import_reads_markdown_checklists_and_plain_lines() {
        let text = "# 备忘\n\n- [ ] 修登录 #后端\n- [x] 写周报\n- 没勾选框的一行\n* 星号项\n+ 加号项\n裸标题\n";
        let rows = super::parse_lines(text, today());
        assert_eq!(rows.len(), 7, "空行丢掉，其余逐行成条：{rows:?}");
        // 光秃秃的 `#` 是「标签记号为空的记号」，按既有口径丢掉，剩下的是标题。
        assert_eq!(rows[0].parsed.title, "备忘");
        assert!(!rows[1].done);
        assert_eq!(rows[1].parsed.title, "修登录");
        assert_eq!(rows[1].parsed.tags, ["后端"]);
        assert!(rows[2].done);
        assert_eq!(rows[2].parsed.title, "写周报");
        assert!(!rows[3].done);
        assert_eq!(rows[3].parsed.title, "没勾选框的一行");
        assert_eq!(rows[4].parsed.title, "星号项");
        assert_eq!(rows[5].parsed.title, "加号项");
        assert_eq!(rows[6].parsed.title, "裸标题");
    }

    #[test]
    fn unknown_brackets_are_not_checkboxes() {
        // `[!]` 不是勾选框 —— 整行（含方括号）留在标题里，不把正常标题啃掉一块。
        let rows = super::parse_lines("- [!] 重要的事", today());
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].done);
        assert_eq!(rows[0].parsed.title, "[!] 重要的事");
        // 没闭合的方括号同理。
        let rows = super::parse_lines("- [x 半截", today());
        assert_eq!(rows[0].parsed.title, "[x 半截");
    }

    #[test]
    fn blank_input_imports_nothing() {
        assert!(super::parse_lines("  \n\n\t\n", today()).is_empty());
        assert!(super::parse_lines("", today()).is_empty());
    }
}
