//! 项目管理页的 AI 编排层（S6 第二批）—— **纯函数**，不碰 gpui、不碰存储。
//!
//! 这里只做两件在单测里能钉死的事：
//!
//! 1. **把模型回复抠成结构化结果**（[`quick_from_json`]）—— 模型爱在 JSON 前后加客套话
//!    或 ```json 围栏，容错放在这里，调用点拿到的就是干净的值；
//! 2. **把工作现状拼成 prompt**（[`summarize`] / [`log_prompt`] / [`plan_prompt`]）——
//!    「日报里写了哪几件事」取决于摘要怎么取，这属于口径不属于渲染，所以和
//!    `report.rs` / `export.rs` / `quickparse.rs` 一样留在页面层之外。
//!
//! 真正的网络调用与回填在 `dialogs.rs`（`run_ai` 那一层）—— 那一层需要有窗口、
//! 有实体，测不了；这一层必须测得动。
//!
//! # 为什么 prompt 全是英文
//!
//! 与编辑器三按钮的 `ai_prompt` 同一条口径：字段名和指令用英文，**要生成的内容的
//! 语言由用户输入的语言决定**（日志正文那几个 prompt 明确要求「与下面清单的语言一致」）。
//! 好处是这一层不出现任何界面中文 —— 也就不会被 `every_chinese_literal_is_translated`
//! 要求登记，不需要进 `NON_UI_SOURCES` 豁免名单。

use duck_project::{CalendarDate, LogKind, Priority, Project, Tag, Task, TaskStatus};

use super::quickparse::{self, QuickParse};
use super::state::local_date;

/// 所有 AI 调用共用的系统提示。
///
/// 与编辑器三按钮那份**刻意同文**：同一个应用里的助手不该在不同弹窗里换一副人格。
pub(crate) const SYSTEM_PROMPT: &str = "You are a concise task-management assistant inside a desktop app. \
Answer with exactly what the user asks for and nothing else; never add commentary.";

/// 一次 prompt 里每个桶最多列几条（超出的折成一句「and N more」）。
///
/// 这是**成本闸**不是界面闸：一个几百条任务的库里，「未完成」可能有 300 行，全塞进去
/// 只会把上下文烧光、还让模型抓不住重点。20 行足够让模型看出「这周在忙什么」。
pub(crate) const BUCKET_LIMIT: usize = 20;

// ---------------------------------------------------------------------------
// 模型回复 → 结构化结果
// ---------------------------------------------------------------------------

/// 从模型回复里抠出 JSON 主体。
///
/// 容忍三种常见包装：纯 JSON、` ```json ` 围栏、前后带一句客套话。取**第一对**花括号
/// 之间的内容 —— 与 `TaskEditorForm::apply_ai` 的抓法同一口径（那里的注释写了理由：
/// 模型偶尔带一句「好的，这是结果：」）。
pub(crate) fn json_body(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

/// 模型给的快速新建 JSON → [`QuickParse`]。
///
/// # 逐项容错，不整段失败
///
/// 优先级不认 / 日期不合法 / 标题为空 —— 每一项单独丢掉即可，没有理由因为模型把
/// `due` 写成 `"next week"` 就把整次解析判死。只有**一项都没留下**时才返回 `None`
/// （调用点据此报「没解析出内容」而不是默默回填一个空串）。
///
/// 日期与时刻的识别**复用 [`quickparse`] 的那两个解析器**（`今天` / `明天` / `MM-DD`
/// / `HH:MM` 都认），所以「哪些写法算数」全仓仍然只有一份真源。
pub(crate) fn quick_from_json(text: &str, today: CalendarDate) -> Option<QuickParse> {
    let value: serde_json::Value = serde_json::from_str(json_body(text)?).ok()?;
    let mut out = QuickParse::default();

    if let Some(title) = value.get("title").and_then(|v| v.as_str()) {
        out.title = title.trim().to_owned();
    }
    if let Some(project) = value.get("project").and_then(|v| v.as_str()) {
        let project = project.trim().trim_start_matches('@').trim();
        if !project.is_empty() {
            out.project = Some(project.to_owned());
        }
    }
    if let Some(tags) = value.get("tags").and_then(|v| v.as_array()) {
        for tag in tags {
            let Some(name) = tag.as_str() else { continue };
            let name = name.trim().trim_start_matches('#').trim();
            if name.is_empty() {
                continue;
            }
            if !out.tags.iter().any(|known| known == name) {
                out.tags.push(name.to_owned());
            }
        }
    }
    // 优先级两种写法都收：`"P1"` 与 `1`（模型常把枚举写成数字）。
    let priority = value.get("priority").and_then(|v| {
        v.as_str()
            .map(|text| text.trim().trim_start_matches('!').to_owned())
            .or_else(|| v.as_i64().map(|n| format!("P{n}")))
    });
    if let Some(priority) = priority.and_then(|text| Priority::parse(&text)) {
        out.priority = Some(priority);
    }
    if let Some(due) = value
        .get("due")
        .and_then(|v| v.as_str())
        .and_then(|text| quickparse::parse_date(text.trim(), today))
    {
        out.due = Some(due);
    }
    if let Some(time) = value
        .get("time")
        .and_then(|v| v.as_str())
        .and_then(|text| quickparse::parse_time(text.trim()))
    {
        out.due_time = Some(time);
    }

    (!out.title.is_empty() || out.has_markers()).then_some(out)
}

/// 快速新建的 AI 兜底 prompt。
///
/// 「兜底」的位置写在这里：本地 [`quickparse`] 认不出的自然语言（「下周三把登录超时
/// 那个事收一下，挺急的」）才需要模型。所以 prompt 里把记号表原样交代一遍，让它的
/// 输出结构和本地解析器**能对上**。
pub(crate) fn quick_prompt(
    raw: &str,
    projects: &[Project],
    tags: &[Tag],
    today: CalendarDate,
) -> String {
    let projects = projects
        .iter()
        .map(|project| project.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let tags = tags
        .iter()
        .map(|tag| tag.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Today is {today}. Parse this quick-capture line into a task.\n\
         Raw input: {raw}\n\n\
         Known projects: [{projects}]\nKnown tags: [{tags}]\n\n\
         Split it into a short imperative title plus optional markers. \
         Prefer an existing project / tag when the input clearly refers to one.\n\
         The title must be plain text: it must NOT start with '#', '@' or '!', \
         and must NOT contain a bare date (like 2026-01-01) or a bare time (like 18:00).\n\
         \"due\" accepts YYYY-MM-DD or a relative word (today / tomorrow / the day after tomorrow).\n\n\
         Reply with ONLY a JSON object, no prose, no code fence:\n\
         {{\"title\": \"...\", \"project\": \"...\" or null, \"tags\": [\"...\"], \
         \"priority\": \"P0\"|\"P1\"|\"P2\"|\"P3\" or null, \
         \"due\": \"YYYY-MM-DD\" or null, \"time\": \"HH:MM\" or null}}",
        today = today.to_iso(),
    )
}

// ---------------------------------------------------------------------------
// 工作现状 → prompt（日报 / 周报 / 周计划）
// ---------------------------------------------------------------------------

/// 一条里程碑在摘要里的样子。
///
/// 里程碑不在 `Task` 上（内核里是一张独立表、按 task_id 外键关联），所以由调用点
/// 查好交进来 —— 这一层只负责排版，不碰库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MilestoneLine {
    /// 所属任务标题（里程碑可能属于项目，那时这里是空串，见内核的 `task_id IS NULL`）。
    pub(crate) task: String,
    /// 里程碑标题。
    pub(crate) title: String,
    /// 达成时刻是否落在统计区间内（区间外达成的不算这一段进展）。
    pub(crate) reached: bool,
}

/// 生成日报 / 周报的区间与今天。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SummaryScope {
    /// 区间起（含）。
    pub(crate) from: CalendarDate,
    /// 区间止（含）。
    pub(crate) to: CalendarDate,
    /// 今天（判定逾期的基准）。
    pub(crate) today: CalendarDate,
}

/// 「给模型看的工作现状」—— 五个桶，每桶已是排好序的文本行。
///
/// 刻意做成**已经排好版的字符串**而不是结构体列表：这些行的唯一去处就是 prompt，
/// 而「一行怎么写」正是要在单测里钉住的口径（`[P1] 标题 (项目, 截止 10-20, 30%)`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkSummary {
    /// 区间内完成的（按完成日倒序）。
    pub(crate) done: Vec<String>,
    /// 正在推进的（进行中 / 阻塞）。
    pub(crate) doing: Vec<String>,
    /// 逾期未完成的（按截止日升序）。
    pub(crate) overdue: Vec<String>,
    /// 区间内到期且未完成的（= 这段时间要交的）。
    pub(crate) due_in_range: Vec<String>,
    /// 区间内达成的里程碑。
    pub(crate) reached: Vec<String>,
    /// 「还有多少条没列进来」的四个计数，与上面四个桶一一对应。
    pub(crate) done_more: usize,
    pub(crate) doing_more: usize,
    pub(crate) overdue_more: usize,
    pub(crate) due_more: usize,
    /// 当前筛选下总共有多少条未完成任务（给模型一个量级感）。
    pub(crate) open_total: usize,
}

impl WorkSummary {
    /// 一个桶都没有内容吗（调用点据此决定「没什么可写」的回执，而不是给模型一段空上下文）。
    pub(crate) fn is_empty(&self) -> bool {
        self.done.is_empty()
            && self.doing.is_empty()
            && self.overdue.is_empty()
            && self.due_in_range.is_empty()
            && self.reached.is_empty()
    }

    /// 周计划那条路（[`plan_prompt`]）有没有事实可写。
    ///
    /// 与 [`Self::is_empty`] 分开：周计划**不引用**「已完成」那一桶，所以只有「这段时间
    /// 刚清完几件、手上没别的活」时，`is_empty()` 是 `false` 而计划其实无从写起。
    pub(crate) fn plan_facts_empty(&self) -> bool {
        self.due_in_range.is_empty() && self.doing.is_empty() && self.overdue.is_empty()
    }
}

/// 把现状算成 [`WorkSummary`]。
///
/// 口径与 `report.rs` 刻意区分：报表是**给人看**的（KPI / 趋势 / 分布），这里是
/// **给模型看**的（谁完成了、谁还挂着、谁逾期了）。两者都从同一份 `tasks` 切片来，
/// 所以不存在「报表说完成 3、周报只写了 2」这种对不上的情况 —— 差异只可能是筛选条件
/// 不同，而那是调用点传进来的。
pub(crate) fn summarize(
    tasks: &[Task],
    projects: &[Project],
    milestones: &[MilestoneLine],
    scope: SummaryScope,
) -> WorkSummary {
    let project_name = |id: Option<i64>| {
        id.and_then(|id| {
            projects
                .iter()
                .find(|project| project.id == id)
                .map(|project| project.name.clone())
        })
        .unwrap_or_default()
    };

    let mut done_rows: Vec<(CalendarDate, String)> = Vec::new();
    let mut doing_rows: Vec<(i64, Priority, String)> = Vec::new();
    let mut overdue_rows: Vec<(i64, String)> = Vec::new();
    let mut due_rows: Vec<(i64, String)> = Vec::new();
    let mut open_total = 0usize;

    for task in tasks {
        if !task.status.is_settled() {
            open_total += 1;
        }
        let name = project_name(task.project_id);
        let line = task_line(task, &name);
        if task.status == TaskStatus::Done {
            if let Some(day) = task.completed_at.map(local_date)
                && day >= scope.from
                && day <= scope.to
            {
                done_rows.push((day, line));
            }
            continue;
        }
        if task.is_overdue(scope.today) {
            overdue_rows.push((
                task.due_date.map_or(0, CalendarDate::days_since_epoch),
                line,
            ));
            continue;
        }
        let due_in_range = task
            .due_date
            .is_some_and(|due| due >= scope.from && due <= scope.to);
        if due_in_range {
            due_rows.push((
                task.due_date.map_or(0, CalendarDate::days_since_epoch),
                line,
            ));
            continue;
        }
        if matches!(task.status, TaskStatus::Doing | TaskStatus::Blocked) {
            doing_rows.push((task.priority.as_i64(), task.priority, line));
        }
    }

    // 完成日倒序（最近的排前面，与报表的「完成明细」同序）；同日按行文本倒序，
    // 让「同一天完成几条」之间的先后**稳定**，两次调用不会给出不同的 prompt。
    done_rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    doing_rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)));
    overdue_rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    due_rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let (done, done_more) = cut(done_rows.into_iter().map(|(_, line)| line).collect());
    let (doing, doing_more) = cut(doing_rows.into_iter().map(|(_, _, line)| line).collect());
    let (overdue, overdue_more) = cut(overdue_rows.into_iter().map(|(_, line)| line).collect());
    let (due_in_range, due_more) = cut(due_rows.into_iter().map(|(_, line)| line).collect());
    let reached = milestones
        .iter()
        .filter(|line| line.reached)
        .map(|line| {
            if line.task.is_empty() {
                line.title.clone()
            } else {
                format!("{} ({})", line.title, line.task)
            }
        })
        .collect::<Vec<_>>();

    WorkSummary {
        done,
        doing,
        overdue,
        due_in_range,
        reached,
        done_more,
        doing_more,
        overdue_more,
        due_more,
        open_total,
    }
}

/// 截到 [`BUCKET_LIMIT`]，返回（留下的, 被砍掉的条数）。
fn cut(lines: Vec<String>) -> (Vec<String>, usize) {
    if lines.len() <= BUCKET_LIMIT {
        return (lines, 0);
    }
    let extra = lines.len() - BUCKET_LIMIT;
    (lines.into_iter().take(BUCKET_LIMIT).collect(), extra)
}

/// 摘要里的一行任务。
///
/// 与 `report.rs` / `export.rs` 的表格行不是一个东西：那两处要**人**读（所以列宽、
/// 对齐、本地化都在意），这里只要模型读得懂 —— 一行里把「标题 / 项目 / 截止 / 进度」
/// 一次说清，且**只用 ASCII 标点**，避免模型把中西标点混起来。
fn task_line(task: &Task, project: &str) -> String {
    let mut parts = vec![format!("[P{}] {}", task.priority.as_i64(), task.title)];
    let mut detail: Vec<String> = Vec::new();
    if !project.is_empty() {
        detail.push(project.to_owned());
    }
    if let Some(due) = task.due_date {
        detail.push(format!("due {}", due.to_iso()));
    }
    detail.push(format!("{}%", task.progress));
    parts.push(format!("({})", detail.join(", ")));
    parts.join(" ")
}

/// 一个桶在 prompt 里的段落（含「and N more」）。
fn bucket(heading: &str, lines: &[String], more: usize) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = format!("{heading} ({}):\n", lines.len() + more);
    for line in lines {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    if more > 0 {
        out.push_str(&format!("- ... and {more} more\n"));
    }
    out
}

/// 日报 / 周报的 prompt。
///
/// `kind` 只认 [`LogKind::Daily`] 与 [`LogKind::Weekly`]；[`LogKind::Plan`] 走
/// [`plan_prompt`]（周计划是**往前看**的，与这两档看的东西不一样）。
pub(crate) fn log_prompt(kind: LogKind, summary: &WorkSummary, scope: SummaryScope) -> String {
    let what = match kind {
        LogKind::Weekly => "weekly report",
        _ => "daily work log",
    };
    let span = if scope.from == scope.to {
        scope.from.to_iso()
    } else {
        format!("{} .. {}", scope.from.to_iso(), scope.to.to_iso())
    };
    let mut facts = bucket("Completed in this period", &summary.done, summary.done_more);
    facts.push_str(&bucket("Reached milestones", &summary.reached, 0));
    facts.push_str(&bucket(
        "Still in progress",
        &summary.doing,
        summary.doing_more,
    ));
    facts.push_str(&bucket("Overdue", &summary.overdue, summary.overdue_more));
    format!(
        "Period: {span} (today {today}). Open tasks under the current filter: {open}.\n\n{facts}\n\
         Write a {what} in Markdown as the first person, past tense for what was finished and \
         present tense for what is still moving.\n\
         Rules: use ONLY the facts above, never invent a task, a person or a number; group related \
         items instead of restating the list one by one; if a bucket is empty just leave it out; \
         write the prose in the same language as the task titles below; 6-12 lines total; \
         output the Markdown body only.",
        open = summary.open_total,
        today = scope.today.to_iso(),
    )
}

/// 周计划的 prompt（往前看）。
///
/// 与 [`log_prompt`] 的区别在事实来源：日志读「这段时间做完了什么」，计划读
/// 「这段时间要交什么、还挂着什么」。所以「已完成」那一桶在这里**不出现在 prompt 里**
/// —— 往前的计划不需要罗列过去的完成项。
pub(crate) fn plan_prompt(summary: &WorkSummary, scope: SummaryScope) -> String {
    let span = format!("{} .. {}", scope.from.to_iso(), scope.to.to_iso());
    let mut facts = bucket(
        "Due in this period",
        &summary.due_in_range,
        summary.due_more,
    );
    facts.push_str(&bucket(
        "Still in progress",
        &summary.doing,
        summary.doing_more,
    ));
    facts.push_str(&bucket(
        "Overdue already",
        &summary.overdue,
        summary.overdue_more,
    ));
    format!(
        "Period: {span} (today {today}). Open tasks under the current filter: {open}.\n\n{facts}\n\
         Write a weekly plan in Markdown with three short parts: the goal of the week, the concrete \
         tasks to move, and how to verify them.\n\
         Rules: pick tasks ONLY from the list above, never invent one; put the highest priority and \
         the most overdue ones first; keep it under 12 lines; write the prose in the same language \
         as the task titles below; output the Markdown body only.",
        open = summary.open_total,
        today = scope.today.to_iso(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_project::{Quadrant, SubtaskProgress};

    fn today() -> CalendarDate {
        CalendarDate::new(2026, 10, 14).expect("legal date")
    }

    fn day(text: &str) -> CalendarDate {
        CalendarDate::parse(text).expect("legal ISO date")
    }

    /// Local noon on a calendar day — mirrors `report.rs`'s helper so a task's
    /// `completed_at` lands on the intended day in every timezone.
    fn noon_on(date: CalendarDate) -> i64 {
        let base = date.days_since_epoch() * 86_400;
        base + 43_200 - i64::from(duck_platform::local_utc_offset_seconds(base))
    }

    fn task(id: i64, status: TaskStatus) -> Task {
        Task {
            id,
            project_id: None,
            parent_id: None,
            title: format!("Task {id}"),
            description_md: String::new(),
            status,
            priority: Priority::P2,
            quadrant: Quadrant::Q2,
            start_date: None,
            due_date: None,
            due_time: None,
            completed_at: None,
            progress: 0,
            estimate_hours: None,
            actual_hours: None,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            archived_at: None,
            archived_root: None,
            starred: false,
            block_reason: String::new(),
            note: String::new(),
            git_branch: String::new(),
            context: duck_project::TaskContext::default(),
            tags: Vec::new(),
            subtasks: SubtaskProgress { total: 0, done: 0 },
        }
    }

    fn project(id: i64, name: &str) -> Project {
        Project {
            id,
            group_id: None,
            name: name.to_owned(),
            description: String::new(),
            color: "#445566".to_owned(),
            icon: None,
            status: duck_project::ProjectStatus::Active,
            start_date: None,
            due_date: None,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            archived_at: None,
        }
    }

    fn scope(from: &str, to: &str) -> SummaryScope {
        SummaryScope {
            from: day(from),
            to: day(to),
            today: today(),
        }
    }

    #[test]
    fn json_body_survives_fences_and_small_talk() {
        assert_eq!(json_body("{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(
            json_body("Sure! Here it is:\n```json\n{\"a\":1}\n```\nHope that helps."),
            Some("{\"a\":1}")
        );
        assert_eq!(json_body("no json at all"), None);
        assert_eq!(json_body("}{"), None, "花括号顺序拧了就是没有");
    }

    #[test]
    fn quick_from_json_reads_every_field() {
        // `r##"…"##`：正文里有 `"#后端"`，用单 `#` 定界会在那儿提前收尾。
        let text = r##"```json
{"title":"修复登录超时","project":"Alpha","tags":["#后端","网关","后端"],
 "priority":"P1","due":"明天","time":"18:00"}
```"##;
        let parsed = quick_from_json(text, today()).expect("parsed");
        assert_eq!(parsed.title, "修复登录超时");
        assert_eq!(parsed.project.as_deref(), Some("Alpha"));
        assert_eq!(parsed.tags, ["后端", "网关"], "`#` 前缀削掉且去重");
        assert_eq!(parsed.priority, Some(Priority::P1));
        assert_eq!(
            parsed.due.map(|due| due.to_iso()).as_deref(),
            Some("2026-10-15"),
            "相对词交给 quickparse 的日期解析器"
        );
        assert_eq!(parsed.due_time.as_deref(), Some("18:00"));
    }

    #[test]
    fn quick_from_json_drops_only_the_bad_pieces() {
        let text = r#"{"title":"上线灰度","priority":"urgent","due":"next week","time":"99:99"}"#;
        let parsed = quick_from_json(text, today()).expect("title 还在就不算失败");
        assert_eq!(parsed.title, "上线灰度");
        assert_eq!(parsed.priority, None, "`urgent` 不是优先级档");
        assert_eq!(parsed.due, None, "认不出的日期宁可不要");
        assert_eq!(parsed.due_time, None);

        // 数字档的优先级也收。
        let numeric = quick_from_json(r#"{"title":"x","priority":0}"#, today()).expect("parsed");
        assert_eq!(numeric.priority, Some(Priority::P0));
    }

    #[test]
    fn quick_from_json_is_none_when_nothing_survives() {
        assert_eq!(quick_from_json("{}", today()), None);
        assert_eq!(quick_from_json(r#"{"title":"  "}"#, today()), None);
        // 只有记号、没有标题 —— 仍然算解析出了东西。
        let markers_only = quick_from_json(r#"{"tags":["后端"]}"#, today()).expect("parsed");
        assert!(markers_only.title.is_empty());
        assert!(markers_only.has_markers());
    }

    #[test]
    fn quick_prompt_lists_the_known_catalogs() {
        let text = quick_prompt(
            "下周三把登录超时收一下",
            &[project(1, "Alpha")],
            &[duck_project::Tag {
                id: 2,
                name: "后端".to_owned(),
                color: "#112233".to_owned(),
                is_favorite: false,
                usage_count: 0,
            }],
            today(),
        );
        assert!(text.contains("Today is 2026-10-14"), "{text}");
        assert!(text.contains("Raw input: 下周三把登录超时收一下"), "{text}");
        assert!(text.contains("Known projects: [Alpha]"), "{text}");
        assert!(text.contains("Known tags: [后端]"), "{text}");
        assert!(text.contains("\"project\": \"...\" or null"), "{text}");
    }

    #[test]
    fn summarize_splits_every_bucket_by_its_own_rule() {
        let mut done_in_range = task(1, TaskStatus::Doing);
        done_in_range.status = TaskStatus::Done;
        done_in_range.completed_at = Some(noon_on(day("2026-10-13")));
        done_in_range.progress = 100;

        let mut done_before = task(2, TaskStatus::Done);
        done_before.completed_at = Some(noon_on(day("2026-10-01")));

        let mut doing = task(3, TaskStatus::Doing);
        doing.priority = Priority::P0;
        doing.progress = 40;

        let mut overdue = task(4, TaskStatus::Todo);
        overdue.due_date = Some(day("2026-10-10"));

        let mut due_this_week = task(5, TaskStatus::Todo);
        due_this_week.due_date = Some(day("2026-10-16"));

        let tasks = [done_in_range, done_before, doing, overdue, due_this_week];
        let summary = summarize(
            &tasks,
            &[project(9, "Alpha")],
            &[
                MilestoneLine {
                    task: "Task 3".to_owned(),
                    title: "灰度上线".to_owned(),
                    reached: true,
                },
                MilestoneLine {
                    task: "Task 3".to_owned(),
                    title: "还没到".to_owned(),
                    reached: false,
                },
            ],
            scope("2026-10-12", "2026-10-18"),
        );

        assert_eq!(summary.done.len(), 1, "10-01 完成的那条在区间外");
        assert!(
            summary.done[0].contains("[P2] Task 1"),
            "{:?}",
            summary.done
        );
        assert_eq!(summary.doing.len(), 1);
        assert!(summary.doing[0].contains("[P0] Task 3"));
        assert_eq!(summary.overdue.len(), 1, "逾期的不再算进「本周到期」");
        assert_eq!(summary.due_in_range.len(), 1);
        assert_eq!(summary.reached, ["灰度上线 (Task 3)"]);
        assert_eq!(summary.open_total, 3, "未完成的三条：doing / overdue / due");
        assert!(!summary.is_empty());
    }

    #[test]
    fn summarize_sorts_and_caps_each_bucket() {
        let tasks: Vec<Task> = (0..BUCKET_LIMIT as i64 + 4)
            .map(|index| {
                let mut item = task(index + 1, TaskStatus::Doing);
                item.due_date = Some(day("2026-11-01"));
                item.progress = 10;
                item
            })
            .collect();
        let summary = summarize(&tasks, &[], &[], scope("2026-10-12", "2026-10-18"));
        assert_eq!(summary.doing.len(), BUCKET_LIMIT);
        assert_eq!(summary.doing_more, 4);
        assert!(
            summary.due_in_range.is_empty(),
            "截止日 11-01 落在区间外，不该进「本周到期」"
        );
        assert_eq!(summary.overdue.len(), 0, "11-01 是将来，不算逾期");
        // 同优先级时按行文本排 —— 两次调用必须给出同一份 prompt。
        let again = summarize(&tasks, &[], &[], scope("2026-10-12", "2026-10-18"));
        assert_eq!(summary, again);
    }

    #[test]
    fn an_empty_summary_says_so() {
        let summary = summarize(&[], &[], &[], scope("2026-10-14", "2026-10-14"));
        assert!(summary.is_empty());
        assert_eq!(summary.open_total, 0);
    }

    #[test]
    fn bucket_lines_carry_heading_count_and_overflow() {
        let text = bucket("Done", &["a".to_owned(), "b".to_owned()], 3);
        assert_eq!(text, "Done (5):\n- a\n- b\n- ... and 3 more\n");
        assert_eq!(bucket("Done", &[], 0), "", "空桶整段不出现");
    }

    #[test]
    fn log_prompt_carries_facts_and_no_completed_bucket_in_plans() {
        let summary = WorkSummary {
            done: vec!["[P1] 修网关".to_owned()],
            overdue: vec!["[P0] 写文档".to_owned()],
            due_in_range: vec!["[P2] 评审".to_owned()],
            open_total: 4,
            ..WorkSummary::default()
        };
        let daily = log_prompt(LogKind::Daily, &summary, scope("2026-10-14", "2026-10-14"));
        assert!(
            daily.contains("Period: 2026-10-14 (today 2026-10-14)"),
            "{daily}"
        );
        assert!(
            daily.contains("Open tasks under the current filter: 4"),
            "{daily}"
        );
        assert!(daily.contains("Completed in this period (1):"), "{daily}");
        assert!(daily.contains("Overdue (1):"), "{daily}");
        assert!(daily.contains("daily work log"), "{daily}");

        let weekly = log_prompt(LogKind::Weekly, &summary, scope("2026-10-12", "2026-10-18"));
        assert!(
            weekly.contains("Period: 2026-10-12 .. 2026-10-18"),
            "{weekly}"
        );
        assert!(weekly.contains("weekly report"), "{weekly}");

        let plan = plan_prompt(&summary, scope("2026-10-12", "2026-10-18"));
        assert!(plan.contains("Due in this period (1):"), "{plan}");
        assert!(plan.contains("[P2] 评审"), "{plan}");
        assert!(
            !plan.contains("[P1] 修网关"),
            "往前的计划不罗列已完成项：{plan}"
        );
        assert!(plan.contains("weekly plan in Markdown"), "{plan}");
    }
}
