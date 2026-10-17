//! 任务导出（原型 `exportText` / `openExport`，`docs/projects-mock.html:2453`）。
//!
//! 三种格式都是**纯文本**：Markdown 清单 / CSV / JSON。这一层只产生一段文本
//! （数据进、`String` 出），所以是纯函数，单测里可以把每一列 / 每一行钉死。
//! **落点不在这里**：出口（剪贴板 / 另存为）由弹窗决定，见
//! `dialogs::open_export` 与 `ProjectsPage::{copy_export, save_export}`。
//!
//! # 出口是两个，不是两个实现
//!
//! 「复制到剪贴板」与「另存为…」共用**同一份** `render` 输出，弹窗里也共用同一个
//! 预览框 —— 于是「我看到的」与「我存下来的」永远是同一段文字。早先只有复制：
//! 落盘要自己开个文本编辑器粘一遍，而「导出一份备份」正是最常见的诉求。
//!
//! ⚠️ 保存面板是 gpui 的**异步**面板（`crate::file_dialogs`），所以文本必须在
//! **打开面板之前**就拼好交出去；面板回调里不许再回读页面（用户可能在面板上停很久，
//! 回来时页面早翻页了）。
//!
//! # 一个刻意的重复
//!
//! [`csv_field`] 与 `import::csv_escape` 是同一套规则的两份实现（各 6 行）。不合成
//! 公共函数：一份管「写出去」、一份管「写进模板」，两边都短到一眼能核完，而跨模块
//! 暴露一个 CSV 转义工具会让「谁在用它」变得不清楚。要改规则时**两处一起改** ——
//! `import.rs` 的单测 `csv_template_quotes_every_cell_and_round_trips` 会盯着。

use duck_project::{CalendarDate, Task};

/// 导出范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ExportScope {
    /// 当前视图（已应用筛选与排序）—— 默认，最常见的就是「把我正在看的这一屏导出去」。
    #[default]
    View,
    /// 全部任务（绕过筛选与分档）。
    All,
}

impl ExportScope {
    /// 该范围对应的任务清单。
    ///
    /// 收成一个方法而不是在调用处 `match`：弹窗里「画读数」和「生成文本」都要这一份，
    /// 两处各写一遍 `match` 迟早会有一处漏改。
    pub(crate) fn tasks<'a>(self, view: &'a [Task], all: &'a [Task]) -> &'a [Task] {
        match self {
            Self::View => view,
            Self::All => all,
        }
    }
}

/// 导出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ExportFormat {
    /// Markdown 清单（默认）。
    #[default]
    Markdown,
    /// CSV（一行一条，含表头）。
    Csv,
    /// JSON（带导出时间与条数的对象）。
    Json,
}

impl ExportFormat {
    /// 全部取值，按弹窗里 segmented tabs 的顺序。
    pub(crate) const ALL: [Self; 3] = [Self::Markdown, Self::Csv, Self::Json];

    /// 按钮上的名字。
    ///
    /// 三个都是**格式的专有名词 / 缩写**（`Markdown` / `CSV` / `JSON`），中文界面下
    /// 也照写不译，所以不走 i18n —— 给它们安中文名反而没人认得。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Csv => "CSV",
            Self::Json => "JSON",
        }
    }
}

/// 生成导出文本。
///
/// `heading` 只被 Markdown 用（文件第一行的标题）；翻译是**调用方**的事，这一层
/// 不认识界面语言，也就不需要 `&App`。
///
/// `project_name` 把任务的 `project_id` 翻成项目名：`Some(id)` 查不到（项目被删了）
/// 与 `None`（收集箱）都当「没有项目」处理，导出里留空 —— 导出不该因为一个悬空
/// 外键就整份失败。
pub(crate) fn render(
    format: ExportFormat,
    today: CalendarDate,
    heading: &str,
    tasks: &[Task],
    project_name: impl Fn(Option<i64>) -> Option<String>,
) -> String {
    match format {
        ExportFormat::Markdown => markdown(today, heading, tasks, &project_name),
        ExportFormat::Csv => csv(tasks, &project_name),
        ExportFormat::Json => json(today, tasks, &project_name),
    }
}

/// 优先级 / 象限的短名。
///
/// 内核的 `Priority` 没有 `Display`（它的 `as_i64` 是**落库**用的），这里补一个
/// 面向文本的写法。`P{0..3}` 与 `as_i64` 一一对应，不给第二份真源。
fn priority_label(task: &Task) -> String {
    format!("P{}", task.priority.as_i64())
}

fn markdown(
    today: CalendarDate,
    heading: &str,
    tasks: &[Task],
    project_name: &impl Fn(Option<i64>) -> Option<String>,
) -> String {
    let mut out = format!("# {heading} · {}\n\n", today.to_iso());
    for task in tasks {
        let done = task.status.is_settled();
        out.push_str(if done { "- [x] " } else { "- [ ] " });
        out.push_str(&task.title);
        out.push_str(&format!(" `{}`", priority_label(task)));
        if let Some(name) = project_name(task.project_id) {
            out.push_str(&format!(" @{name}"));
        }
        for tag in &task.tags {
            out.push_str(&format!(" #{}", tag.name));
        }
        if let Some(due) = task.due_date {
            out.push_str(&format!(" {}", due.to_iso()));
        }
        out.push_str(&format!(" ({}%)", task.progress));
        out.push('\n');
    }
    out
}

fn csv(tasks: &[Task], project_name: &impl Fn(Option<i64>) -> Option<String>) -> String {
    const HEAD: &str = "id,title,project,status,priority,quadrant,tags,start,due,progress";
    let mut out = String::from(HEAD);
    out.push('\n');
    for task in tasks {
        let tags = task
            .tags
            .iter()
            .map(|tag| tag.name.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{}\n",
            task.id,
            csv_field(&task.title),
            csv_field(&project_name(task.project_id).unwrap_or_default()),
            task.status.as_str(),
            priority_label(task),
            task.quadrant.as_str(),
            csv_field(&tags),
            task.start_date.map_or(String::new(), |date| date.to_iso()),
            task.due_date.map_or(String::new(), |date| date.to_iso()),
            task.progress,
        ));
    }
    out
}

/// CSV 的一个字段：一律加引号并把内部的 `"` 翻倍（RFC 4180 的做法）。
///
/// **一律加引号**而不是「必要时才加」：标题里带逗号、引号、换行都可能，逐个判断
/// 多一处「漏判就整份表格错列」的地方。
fn csv_field(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

fn json(
    today: CalendarDate,
    tasks: &[Task],
    project_name: &impl Fn(Option<i64>) -> Option<String>,
) -> String {
    let mut out = String::from("{\n");
    out.push_str(&format!("  \"exportedAt\": \"{}\",\n", today.to_iso()));
    out.push_str(&format!("  \"count\": {},\n", tasks.len()));
    out.push_str("  \"tasks\": [");
    for (index, task) in tasks.iter().enumerate() {
        out.push('\n');
        out.push_str(&json_task(task, project_name));
        if index + 1 < tasks.len() {
            out.push(',');
        }
    }
    if tasks.is_empty() {
        out.push_str("]\n}\n");
        return out;
    }
    out.push_str("\n  ]\n}\n");
    out
}

fn json_task(task: &Task, project_name: &impl Fn(Option<i64>) -> Option<String>) -> String {
    let tags = task
        .tags
        .iter()
        .map(|tag| json_string(&tag.name))
        .collect::<Vec<_>>()
        .join(", ");
    // `name` 收的是**裸名**（`id` / `title`…），外层这一对引号由 `field` 自己补。
    // 早先这里写成 `field("\"id\"", …)`，于是每行都出成 `""id"": 4` —— 引号转了两道。
    let field = |name: &str, value: String| format!("      \"{name}\": {value}");
    let fields = [
        field("id", task.id.to_string()),
        field("title", json_string(&task.title)),
        field(
            "project",
            json_string(&project_name(task.project_id).unwrap_or_default()),
        ),
        field("status", json_string(task.status.as_str())),
        field("priority", json_string(&priority_label(task))),
        field("quadrant", json_string(task.quadrant.as_str())),
        field("tags", format!("[{tags}]")),
        field("starred", task.starred.to_string()),
        field("start", json_date(task.start_date)),
        field("due", json_date(task.due_date)),
        field("progress", task.progress.to_string()),
        field("descriptionMd", json_string(&task.description_md)),
        field("note", json_string(&task.note)),
    ];
    format!("    {{\n{}\n    }}", fields.join(",\n"))
}

fn json_date(date: Option<CalendarDate>) -> String {
    match date {
        Some(date) => json_string(&date.to_iso()),
        None => "null".to_owned(),
    }
}

/// JSON 字符串字面量。
///
/// 手写而不是引 `serde_json`：内核（`duck-project`）零序列化依赖，而 `duck-app`
/// 里拿 `Task` 拼一个 `serde_json::Value` 要先给内核模型加派生 —— 一个导出功能
/// 不值得把存档格式焊进内核模型。
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // 其余控制字符按 `\uXXXX` 转义（JSON 不允许裸控制字符）。
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::{ExportFormat, render};
    use duck_project::{CalendarDate, Priority, Quadrant, SubtaskProgress, Tag, Task, TaskStatus};

    fn today() -> CalendarDate {
        CalendarDate::new(2026, 10, 14).expect("合法日期")
    }

    fn task(id: i64, title: &str) -> Task {
        Task {
            id,
            project_id: None,
            parent_id: None,
            title: title.to_owned(),
            description_md: String::new(),
            status: TaskStatus::Todo,
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

    /// 项目名的假查询口：只认 id `1` → `Alpha`。
    fn names(id: Option<i64>) -> Option<String> {
        match id {
            Some(1) => Some("Alpha".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn markdown_lists_markers_and_checks_done_items() {
        let mut open = task(1, "修复登录超时");
        open.priority = Priority::P1;
        open.project_id = Some(1);
        open.due_date = Some(CalendarDate::new(2026, 10, 15).expect("合法日期"));
        open.progress = 40;
        open.tags = vec![Tag {
            id: 7,
            name: "后端".to_owned(),
            color: "#112233".to_owned(),
            is_favorite: false,
            usage_count: 1,
        }];
        let mut done = task(2, "写周报");
        done.status = TaskStatus::Done;

        let text = render(
            ExportFormat::Markdown,
            today(),
            "任务导出",
            &[open, done],
            names,
        );
        assert!(text.starts_with("# 任务导出 · 2026-10-14\n"), "{text}");
        assert!(
            text.contains("- [ ] 修复登录超时 `P1` @Alpha #后端 2026-10-15 (40%)"),
            "{text}"
        );
        assert!(text.contains("- [x] 写周报 `P2` (0%)"), "{text}");
    }

    #[test]
    fn csv_quotes_every_field_and_doubles_inner_quotes() {
        let mut row = task(3, "带,逗号与\"引号\"的标题");
        row.tags = vec![Tag {
            id: 1,
            name: "a".to_owned(),
            color: "#000000".to_owned(),
            is_favorite: false,
            usage_count: 0,
        }];

        let text = render(ExportFormat::Csv, today(), "任务导出", &[row], names);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "id,title,project,status,priority,quadrant,tags,start,due,progress"
        );
        assert!(
            lines[1].starts_with("3,\"带,逗号与\"\"引号\"\"的标题\",\"\""),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn an_unknown_project_id_is_exported_as_empty_not_as_a_failure() {
        let mut orphan = task(4, "悬空外键");
        orphan.project_id = Some(999);

        let csv = render(ExportFormat::Csv, today(), "任务导出", &[orphan], names);
        assert!(
            csv.lines()
                .nth(1)
                .expect("有数据行")
                .contains(",\"\",todo,")
        );

        let orphan = task(4, "悬空外键");
        let json = render(ExportFormat::Json, today(), "任务导出", &[orphan], names);
        assert!(json.contains("\"project\": \"\""), "{json}");
    }

    #[test]
    fn json_escapes_quotes_and_keeps_null_dates() {
        let mut row = task(5, "含\"引号\"与\\反斜杠");
        row.description_md = "第一行\n第二行".to_owned();

        let text = render(ExportFormat::Json, today(), "任务导出", &[row], names);
        assert!(text.contains("\"exportedAt\": \"2026-10-14\""), "{text}");
        assert!(text.contains("\"count\": 1"), "{text}");
        assert!(
            text.contains("\"title\": \"含\\\"引号\\\"与\\\\反斜杠\""),
            "{text}"
        );
        assert!(
            text.contains("\"descriptionMd\": \"第一行\\n第二行\""),
            "{text}"
        );
        assert!(text.contains("\"start\": null"), "{text}");
        assert!(text.contains("\"due\": null"), "{text}");
    }

    /// JSON 那一支**真的能被解析**，而不是「看起来像 JSON」。
    ///
    /// 上面两条只 `contains` 了片段，写错引号层级时正好漏过去（`""id"": 4` 也含
    /// `"count"` 之外的一切）；拉 `serde_json` 解一遍是唯一能钉住整份结构的办法。
    /// 断言逐字钉的是**字段名**与几个值 —— 上一个版本就是字段名多转义了一道。
    #[test]
    fn the_json_export_parses_back_into_the_same_values() {
        let mut row = task(6, "含\"引号\"的标题");
        row.project_id = Some(1);
        row.progress = 35;
        row.due_date = Some(CalendarDate::new(2026, 10, 15).expect("合法日期"));

        let text = render(ExportFormat::Json, today(), "任务导出", &[row], names);
        let value: serde_json::Value = serde_json::from_str(&text).expect("导出的 JSON 必须合法");

        assert_eq!(value["exportedAt"], "2026-10-14");
        assert_eq!(value["count"], 1);
        let task = &value["tasks"][0];
        assert_eq!(task["id"], 6);
        assert_eq!(task["title"], "含\"引号\"的标题");
        assert_eq!(task["project"], "Alpha");
        assert_eq!(task["priority"], "P2");
        assert_eq!(task["quadrant"], "Q2");
        assert_eq!(task["progress"], 35);
        assert_eq!(task["due"], "2026-10-15");
        assert!(task["start"].is_null(), "没排期就是 null，不是空串");
        assert_eq!(task["starred"], false);
        assert_eq!(task["tags"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn an_empty_selection_still_produces_valid_text() {
        let markdown = render(ExportFormat::Markdown, today(), "任务导出", &[], names);
        assert_eq!(markdown, "# 任务导出 · 2026-10-14\n\n");

        let csv = render(ExportFormat::Csv, today(), "任务导出", &[], names);
        assert_eq!(csv.lines().count(), 1, "只有表头：{csv}");

        let json = render(ExportFormat::Json, today(), "任务导出", &[], names);
        assert!(json.contains("\"count\": 0"), "{json}");
        assert!(
            json.trim_end().ends_with('}'),
            "截断的 JSON 会解析不了：{json}"
        );
        let value: serde_json::Value = serde_json::from_str(&json).expect("空选的 JSON 也要合法");
        assert_eq!(value["tasks"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn format_labels_are_stable_ascii() {
        // 这三个是格式名，不随界面语言变 —— 顺手钉住，免得有人「顺手」把它们翻掉。
        assert_eq!(ExportFormat::Markdown.label(), "Markdown");
        assert_eq!(ExportFormat::Csv.label(), "CSV");
        assert_eq!(ExportFormat::Json.label(), "JSON");
    }
}
