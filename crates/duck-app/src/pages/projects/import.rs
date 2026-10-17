//! 从文件（CSV / JSON / Markdown）导入任务。
//!
//! # 三种来源，一种结果
//!
//! 三种格式的差异全在这一层被抹平：出口一律是 [`Row`]（一条「将要被建出来的任务」），
//! 于是预览、去重、落库三件事都只认一种东西。上游（`dialogs::open_import`）只负责
//! 把文件读成 `&str` 并告诉这里「它是什么格式」。
//!
//! | 格式 | 认法 | 备注 |
//! |---|---|---|
//! | Markdown | `- [ ] 标题 #标签 @项目 !P1 明天` | 与快速新建**同一套**记号；裸标题行也算一条 |
//! | CSV | 表头列名 → 字段（中英别名都认） | 手写 RFC 4180 读入（引号包裹、字段内逗号/换行/`""`） |
//! | JSON | `[{…}]` / `{"tasks":[…]}` / 单个对象 | 键名走**同一张**别名表，所以中文键也认 |
//!
//! # 为什么是「按列名自动映射」而不是让用户逐列选
//!
//! 参考实现（`temp/devAssistant/src/components/TaskImport.vue:226-238`）就是自动的：
//! `row['任务标题'] || row['title']` —— 中英双认，认不出就空着。逐列下拉选一遍看起来
//! 更「可控」，但它把「这张表是什么」变成用户每次导入都要回答的问题，而 90% 的
//! 情况（我们自己的导出、我们发的模板）答案是一样的。这里的折中是：
//! **自动映射 + 把映射结果画在预览上方**（`Parsed::columns`），认错的、没用的列
//! 都摆在明面上；真要改，改文件表头比改下拉更省事。
//!
//! ⚠️ 中文别名表在 [`super::import_aliases`]（那个文件豁免了 i18n 守卫，理由写在
//! 它自己的模块头）。**本文件里的中文字面量只许出现在注释与测试里**。
//!
//! # 导出 / 导入是一对
//!
//! `export.rs` 写出的 CSV 表头（`id,title,project,status,priority,quadrant,tags,
//! start,due,progress`）与 JSON 字段名，本文件**全部认得**（`id` 除外：跨库搬主键
//! 只会撞上本地已有的 id）。所以「导出 → 改两笔 → 导回来」是一条真实可走的路，
//! 单测 `our_own_csv_export_round_trips` 钉住了它。

use std::fmt;

use duck_project::{CalendarDate, Priority, Quadrant, TaskStatus};

use super::import_aliases;
use super::quickparse;

// ---------------------------------------------------------------------------
// 格式与字段
// ---------------------------------------------------------------------------

/// 导入来源的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SourceFormat {
    /// Markdown 清单（默认：粘贴文本时最常用）。
    #[default]
    Markdown,
    /// CSV（一行一条，首行可为表头）。
    Csv,
    /// JSON（数组 / `{"tasks":[…]}` / 单个对象）。
    Json,
}

impl SourceFormat {
    /// 全部取值，按弹窗里 segmented tabs 的顺序。
    pub(crate) const ALL: [Self; 3] = [Self::Markdown, Self::Csv, Self::Json];

    /// 按钮上的名字。
    ///
    /// 三个都是**格式的专有名词 / 缩写**，中文界面下也照写不译（与
    /// `export::ExportFormat::label` 同一口径，`import.rs` 里也不能出现中文）。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Csv => "CSV",
            Self::Json => "JSON",
        }
    }

    /// 落盘时的扩展名（不带点）。
    pub(crate) const fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }

    /// 按文件扩展名判格式。
    ///
    /// `.txt` / `.text` 按 Markdown 处理（纯文本里的清单行照样能读）；`.markdown`
    /// 与 `.md` 同义。**认不出就是认不出** —— 不按内容猜。
    ///
    /// ⚠️ `Err` 里是**裸扩展名**（原样），不包成 [`ImportError`]：那个类型说的是
    /// 「**解析**这段文本时失败了」，而「扩展名认不出」发生在**读文件之前**，两件事
    /// 不该共用一条通道 —— 调用方拿到的 `Err` 要填进「不支持 .{} 文件」那句话里。
    pub(crate) fn from_extension(extension: &str) -> Result<Self, String> {
        let given = extension.trim();
        // 先 `trim` 再丢前导 `.`：文件面板给的是裸后缀（`csv`），但从文件名里抄一段
        // （`.CSV` / ` .csv `）也是常事 —— 多这一下不会把 `xlsx` 认成别的。
        match given.trim_start_matches('.').to_ascii_lowercase().as_str() {
            "md" | "markdown" | "txt" | "text" => Ok(Self::Markdown),
            "csv" => Ok(Self::Csv),
            "json" => Ok(Self::Json),
            _ => Err(given.to_owned()),
        }
    }

    /// 面板里预填的文件名（模板下载用）。
    pub(crate) fn template_file_name(self) -> String {
        format!("task-import-template.{}", self.extension())
    }
}

/// 一个可导入的字段。
///
/// 是**列（或 JSON 键）的语义**，不是任务的字段镜像：`Row` 里没有对应位置的字段
/// （比如 `Field::Start` 之外还有 `Field::Time`）不在这里列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Field {
    /// 标题（必填）。
    Title,
    /// 项目名（按名字匹配，认不出落收集箱）。
    Project,
    /// 状态。
    Status,
    /// 优先级。
    Priority,
    /// 四象限。
    Quadrant,
    /// 标签（多个）。
    Tags,
    /// 开始日期。
    Start,
    /// 截止日期。
    Due,
    /// 截止时刻（`HH:MM`）。
    Time,
    /// 进度（0–100）。
    Progress,
    /// 预估工时。
    Estimate,
    /// 实际工时。
    Actual,
    /// 置顶。
    Starred,
    /// 描述。
    Description,
    /// 备注。
    Note,
}

impl Field {
    /// 模板里的列序（也用于给「列映射」那行排一个稳定顺序）。
    pub(crate) const TEMPLATE: [Self; 10] = [
        Self::Title,
        Self::Project,
        Self::Status,
        Self::Priority,
        Self::Quadrant,
        Self::Tags,
        Self::Start,
        Self::Due,
        Self::Time,
        Self::Progress,
    ];
}

/// CSV 的一列映射结果。
///
/// `field` 为 `None` 表示这一列**没被用上**（认不出列名，或同一个字段已经有主了）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnMap {
    /// 在行里的下标（0 起）。
    pub(crate) index: usize,
    /// 原始列名（已 trim；合成列名时是空串）。
    pub(crate) header: String,
    /// 认成了哪个字段。
    pub(crate) field: Option<Field>,
}

/// 一条「将要被建出来的任务」。
///
/// 每种输入格式的差异都在 [`parse`] 里被抹平到这个形状上。全字段可空 —— 缺什么
/// 就用内核的默认值（`status` 默认 `Todo`、`progress` 默认 0，`Done` 默认 100）。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Row {
    /// 标题（已剥掉记号）。
    pub(crate) title: String,
    /// 项目名。
    pub(crate) project: Option<String>,
    /// 状态。
    pub(crate) status: Option<TaskStatus>,
    /// 优先级。
    pub(crate) priority: Option<Priority>,
    /// 四象限。
    pub(crate) quadrant: Option<Quadrant>,
    /// 标签名（未落库）。
    pub(crate) tags: Vec<String>,
    /// 开始日期。
    pub(crate) start: Option<CalendarDate>,
    /// 截止日期。
    pub(crate) due: Option<CalendarDate>,
    /// 截止时刻（`HH:MM`）。
    pub(crate) due_time: Option<String>,
    /// 进度。
    pub(crate) progress: Option<u8>,
    /// 预估工时。
    pub(crate) estimate_hours: Option<f64>,
    /// 实际工时。
    pub(crate) actual_hours: Option<f64>,
    /// 置顶。
    pub(crate) starred: bool,
    /// 描述。
    pub(crate) description: Option<String>,
    /// 备注。
    pub(crate) note: Option<String>,
}

impl Row {
    /// 落库时用的状态（缺省 `Todo`）。
    pub(crate) fn resolved_status(&self) -> TaskStatus {
        self.status.unwrap_or(TaskStatus::Todo)
    }

    /// 落库时用的进度：写在表里就用表里的，否则「已完成 / 已取消」按 100、其余按 0。
    ///
    /// ⚠️ 不给「已完成但 0%」留口子：卡片上那个自相矛盾的读数正是这么来的。
    pub(crate) fn resolved_progress(&self) -> u8 {
        if let Some(progress) = self.progress {
            return progress.min(100);
        }
        if self.resolved_status().is_settled() {
            100
        } else {
            0
        }
    }
}

/// 解析结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Parsed {
    /// 解析出来的行（标题为空的已丢掉）。
    pub(crate) rows: Vec<Row>,
    /// CSV 的列映射（给预览显示）；Markdown / JSON 为空。
    pub(crate) columns: Vec<ColumnMap>,
    /// 被丢掉的行数（空行、标题为空的行）。
    pub(crate) dropped: usize,
    /// 表头里没有标题列，第一列是**猜**的 —— 界面据此提示一句。
    pub(crate) title_guessed: bool,
}

/// 解析失败。详情一律英文（与内核错误同待遇，界面负责包一层中文）。
///
/// 目前只有一种成因（JSON 不合法）。「扩展名认不出」不在这里 —— 那是**读文件之前**
/// 就拦下的事，由 [`SourceFormat::from_extension`] 的 `Err` 带出去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImportError {
    /// JSON 不合法（内容来自 `serde_json`，已带行列位置）。
    Json(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(detail) => write!(formatter, "invalid JSON: {detail}"),
        }
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 把一段文本按指定格式解析成行。
///
/// 空文件不报错：返回空 [`Parsed`]，由调用方给「没有可导入的行」的回执 —— 报错会
/// 让「选错文件」和「文件是空的」看起来像同一件事。
pub(crate) fn parse(
    text: &str,
    format: SourceFormat,
    today: CalendarDate,
) -> Result<Parsed, ImportError> {
    // BOM 在这里一次性剥掉：Windows 记事本与 Excel 导出的 UTF-8 文件常带它，
    // 留在第一个列名 / 键名上会让「第一列认不出」——而列名认不出是静默的。
    let text = text.trim_start_matches('\u{feff}');
    match format {
        SourceFormat::Markdown => Ok(markdown(text, today)),
        SourceFormat::Csv => Ok(csv(text, today)),
        SourceFormat::Json => json(text, today),
    }
}

/// 标题的比对键：trim + 转小写。去重（库里已有的 / 批内重复的）都用它。
pub(crate) fn title_key(title: &str) -> String {
    title.trim().to_lowercase()
}

/// 按「库里已有的标题」把行分成 `(要导入的, 跳过的条数)`。
///
/// ⚠️ 批内也去重：同一个文件里出现两次的同名任务只导第一条 —— 只比对库里的，
/// 会让「导一份自己刚导过的文件」变成导入两遍。
pub(crate) fn partition(rows: &[Row], existing_keys: &[String]) -> (Vec<Row>, usize) {
    let mut seen: Vec<String> = existing_keys.to_vec();
    let mut kept = Vec::with_capacity(rows.len());
    let mut skipped = 0;
    for row in rows {
        let key = title_key(&row.title);
        if seen.contains(&key) {
            skipped += 1;
            continue;
        }
        seen.push(key);
        kept.push(row.clone());
    }
    (kept, skipped)
}

/// 模板文本。三种格式共用一份「表头 + 样例行」，表头与样例由调用方**翻译好**传进来
/// （这一层不认识界面语言）。
///
/// - CSV：表头行 + 数据行（走与导出同一套引号规则）
/// - JSON：`{"tasks": [ {列名: 值}, … ]}`（键名就是表头，中文键照样认）
/// - Markdown：逐行一条 `- [ ]` 清单（只取每行的第一格当标题）
pub(crate) fn template(format: SourceFormat, headers: &[String], rows: &[Vec<String>]) -> String {
    match format {
        SourceFormat::Csv => csv_text(headers, rows),
        SourceFormat::Json => json_text(headers, rows),
        SourceFormat::Markdown => {
            let mut out = String::new();
            for row in rows {
                out.push_str("- [ ] ");
                out.push_str(row.first().map(String::as_str).unwrap_or_default());
                out.push('\n');
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

/// Markdown 清单 → 行。
fn markdown(text: &str, today: CalendarDate) -> Parsed {
    let mut rows = Vec::new();
    let mut dropped = 0;
    for line in text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let (done, body) = split_checkbox(line);
        let parsed = quickparse::parse(body, today);
        if parsed.title.is_empty() {
            dropped += 1;
            continue;
        }
        rows.push(Row {
            title: parsed.title,
            project: parsed.project,
            status: done.then_some(TaskStatus::Done),
            priority: parsed.priority,
            tags: parsed.tags,
            due: parsed.due,
            due_time: parsed.due_time,
            ..Row::default()
        });
    }
    Parsed {
        rows,
        columns: Vec::new(),
        dropped,
        title_guessed: false,
    }
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
    match inside.trim() {
        "" => (false, after.trim_start()),
        "x" | "X" => (true, after.trim_start()),
        _ => (false, rest),
    }
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

/// CSV 文本 → 行。
fn csv(text: &str, today: CalendarDate) -> Parsed {
    let table = read_csv(text);
    if table.is_empty() {
        return Parsed::default();
    }
    let header_like = has_header(&table[0]);
    let (headers, data) = if header_like {
        (table[0].clone(), &table[1..])
    } else {
        // 没有表头：合成一批**空名**列头，再靠「第一列当标题」的兜底把它认出来。
        // （空名而不是「列 1 / 列 2」：这一层不能出现界面文案，显示名由界面拼。）
        (vec![String::new(); table[0].len()], &table[..])
    };
    let columns = map_columns(&headers);
    let title_guessed = !header_like
        || !columns
            .iter()
            .any(|column| column.field == Some(Field::Title) && !column.header.is_empty());

    let mut rows = Vec::new();
    let mut dropped = 0;
    for cells in data {
        let row = row_from_cells(cells, &columns, today);
        if row.title.is_empty() {
            dropped += 1;
            continue;
        }
        rows.push(row);
    }
    Parsed {
        rows,
        columns,
        dropped,
        title_guessed,
    }
}

/// 读一张 CSV 表：手写 RFC 4180 状态机。
///
/// 认三件事：引号包裹的字段、字段内的 `""`（转义成 `"`）、字段内的换行。
/// 不做类型推断（一律给字符串）—— 每个字段怎么解释由 [`row_from_cells`] 决定。
fn read_csv(text: &str) -> Vec<Vec<String>> {
    let mut table: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut at_field_start = true;
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();

    while let Some(ch) = chars.next() {
        if quoted {
            match ch {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    cell.push('"');
                }
                '"' => quoted = false,
                // 引号里的换行是**内容**：CRLF 归一成 LF，免得同一个字段在两个
                // 平台上读出来不一样。
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    cell.push('\n');
                }
                ch => cell.push(ch),
            }
            continue;
        }
        match ch {
            '"' if at_field_start => {
                quoted = true;
                at_field_start = false;
            }
            ',' => {
                row.push(std::mem::take(&mut cell));
                at_field_start = true;
            }
            // 裸 `\r`（老 Mac 换行）也当行结束 —— 不然整份文件会读成一行。
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut cell));
                push_row(&mut table, &mut row);
                at_field_start = true;
            }
            '\n' => {
                row.push(std::mem::take(&mut cell));
                push_row(&mut table, &mut row);
                at_field_start = true;
            }
            ch => {
                cell.push(ch);
                at_field_start = false;
            }
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        push_row(&mut table, &mut row);
    }
    table
}

/// 收一行；整行都是空白的不入表（文件末尾的空行、Excel 多导出的空行）。
fn push_row(table: &mut Vec<Vec<String>>, row: &mut Vec<String>) {
    if row.iter().all(|cell| cell.trim().is_empty()) {
        row.clear();
        return;
    }
    table.push(std::mem::take(row));
}

/// 首行是不是表头：**至少一个**单元格认得出来就算。
///
/// 判据是「认得出来」而不是「看第一格是不是 `id`」：用户可能删掉几列，或者把
/// 标题列放在中间。全是数据行时（每一格都是任务标题）一个都认不出，于是走
/// 「第一列当标题」的兜底 —— 这正是无表头 CSV 想要的结果。
fn has_header(row: &[String]) -> bool {
    row.iter()
        .any(|cell| import_aliases::field_of_header(cell).is_some())
}

/// 把表头逐列翻成字段。同一个字段只认第一列（后面的标「未使用」）。
///
/// 一列标题都没认出来时，把**第一个还没主的列**当标题（并在 [`Parsed::title_guessed`]
/// 里标记）—— 只认第一列本身，不抢一个已经明确属于别的字段的列。
fn map_columns(headers: &[String]) -> Vec<ColumnMap> {
    let mut used: Vec<Field> = Vec::new();
    let mut columns: Vec<ColumnMap> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            let field = import_aliases::field_of_header(header)
                .filter(|field| !used.iter().any(|known| known == field));
            if let Some(field) = field {
                used.push(field);
            }
            ColumnMap {
                index,
                header: header.trim().to_owned(),
                field,
            }
        })
        .collect();
    // 一列标题都没认出来时，把**第一个还没主的列**当标题（caller 那侧据此置 `title_guessed`）。
    // ⚠️ 不写成两层 `if`：那会被 clippy 判 `collapsible_if`，而它建议的写法是 `let` 链
    // （`if a && let Some(x) = b`），`let_chains` 到 1.88 才稳定，本仓 MSRV 是 1.85。
    // 把「标题还没主」并进 `Option::filter` 就只有一个 `if let` 了。
    let first_free = columns
        .iter()
        .position(|column| column.field.is_none())
        .filter(|_| !used.contains(&Field::Title));
    if let Some(index) = first_free {
        columns[index].field = Some(Field::Title);
    }
    columns
}

/// 一行单元格 → [`Row`]。
///
/// 标题里的**记号照认**（`#标签 @项目 !P1 明天 18:00`）：一列一列填是 CSV 的写法，
/// 但「导出后手改两笔」时人往往只在标题里加记号。两边冲突时**列赢** —— 列是
/// 明确填进去的，记号是顺手写的。
fn row_from_cells(cells: &[String], columns: &[ColumnMap], today: CalendarDate) -> Row {
    let mut row = Row::default();
    let mut title = String::new();
    for column in columns {
        let Some(field) = column.field else {
            continue;
        };
        let raw = cells
            .get(column.index)
            .map(|cell| cell.trim())
            .unwrap_or_default();
        match field {
            Field::Title => title = raw.to_owned(),
            Field::Project if !raw.is_empty() => row.project = Some(raw.to_owned()),
            Field::Status => row.status = import_aliases::status_of(raw),
            Field::Priority => row.priority = import_aliases::priority_of(raw),
            Field::Quadrant => row.quadrant = import_aliases::quadrant_of(raw),
            Field::Tags => row.tags = split_tags(raw),
            Field::Start => row.start = day_of(raw, today),
            Field::Due => row.due = day_of(raw, today),
            Field::Time => row.due_time = time_of(raw, today),
            Field::Progress => row.progress = number_of(raw).map(|value| value.min(100.0) as u8),
            Field::Estimate => row.estimate_hours = number_of(raw),
            Field::Actual => row.actual_hours = number_of(raw),
            Field::Starred => {
                if let Some(value) = import_aliases::bool_of(raw) {
                    row.starred = value;
                }
            }
            Field::Description if !raw.is_empty() => row.description = Some(raw.to_owned()),
            Field::Note if !raw.is_empty() => row.note = Some(raw.to_owned()),
            _ => {}
        }
    }
    merge_markers(&mut row, &title, today);
    row
}

/// 把标题里的记号并进行里，并返回剥掉记号的标题。
///
/// 只填空位：列里已经写了的不被记号覆盖。
fn merge_markers(row: &mut Row, raw_title: &str, today: CalendarDate) {
    let parsed = quickparse::parse(raw_title, today);
    for tag in parsed.tags {
        if !row.tags.contains(&tag) {
            row.tags.push(tag);
        }
    }
    if row.project.is_none() {
        row.project = parsed.project;
    }
    if row.priority.is_none() {
        row.priority = parsed.priority;
    }
    if row.due.is_none() {
        row.due = parsed.due;
    }
    if row.due_time.is_none() {
        row.due_time = parsed.due_time;
    }
    // `parse` 返回的 `title` 就是「剥掉记号后的剩余文字」；记号占满整格时它是空的，
    // 那时保留原文 —— 一条标题全是 `#标签` 的任务仍然该有标题。
    row.title = if parsed.title.is_empty() {
        raw_title.trim().to_owned()
    } else {
        parsed.title
    };
}

/// 标签格 → 标签名。
///
/// 先按分隔符切；切出来只有一项时再按空白切。原因：**我们自己的导出**把标签用
/// 空格连起来（`export.rs::csv` 的 `join(" ")`），标签名里带空格的情况远少于
/// 「导出再导回」的需求。逗号类分隔符优先，于是带空格的标签写成
/// `线上 事故, 复盘` 仍然读得出第一个。
fn split_tags(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<&str> = raw
        .split([',', '，', '、', ';', '；', '|'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() <= 1 {
        parts = raw.split_whitespace().collect();
    }
    let mut out: Vec<String> = Vec::new();
    for part in parts {
        let part = part.trim_start_matches('#').trim();
        if part.is_empty() || out.iter().any(|known| known == part) {
            continue;
        }
        out.push(part.to_owned());
    }
    out
}

/// 日期格 → 日期。复用快速新建的日期记号（`2026-10-20` / `10-20` / `今天`…）。
fn day_of(raw: &str, today: CalendarDate) -> Option<CalendarDate> {
    if raw.is_empty() {
        return None;
    }
    // 斜杠分隔的日期在 CSV 里很常见（Excel 的中文默认格式就是 `2026/10/20`）。
    let normalized = raw.trim().replace('/', "-");
    quickparse::parse(&normalized, today).due
}

/// 时刻格 → `HH:MM`。
fn time_of(raw: &str, today: CalendarDate) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    quickparse::parse(raw, today).due_time
}

/// 数字格 → `f64`。空 / 认不出返回 `None`（不静默当 0）。
fn number_of(raw: &str) -> Option<f64> {
    let raw = raw.trim().trim_end_matches('%').trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// 按 CSV 规则拼一段文本（表头 + 数据行）。
fn csv_text(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut out = headers
        .iter()
        .map(|header| csv_escape(header))
        .collect::<Vec<_>>()
        .join(",");
    out.push('\n');
    for row in rows {
        out.push_str(
            &row.iter()
                .map(|cell| csv_escape(cell))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    out
}

/// CSV 的一个字段：一律加引号并把内部的 `"` 翻倍（RFC 4180）。
///
/// 与 `export.rs::csv_field` 同一套规则、同一条理由（一律加引号而不是「必要时才加」）：
/// 模板与导出必须是同一种 CSV，否则「导出改两笔再导回」会在引号上栽跟头。
/// 这一份写成独立的（而不是把 `export.rs` 那个改成 `pub`）：两处各 6 行，比起
/// 跨模块暴露一个纯工具函数，各自完整更好读。
fn csv_escape(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// 按 JSON 规则拼一份模板：`{"tasks": [ {列名: 值}, … ]}`。
fn json_text(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::from("{\n  \"tasks\": [");
    for (index, row) in rows.iter().enumerate() {
        out.push('\n');
        out.push_str("    {\n");
        let fields: Vec<String> = headers
            .iter()
            .enumerate()
            .map(|(column, header)| {
                let value = row.get(column).map(String::as_str).unwrap_or_default();
                format!("      {}: {}", json_string(header), json_string(value))
            })
            .collect();
        out.push_str(&fields.join(",\n"));
        out.push_str("\n    }");
        if index + 1 < rows.len() {
            out.push(',');
        }
    }
    if rows.is_empty() {
        out.push_str("]\n}\n");
        return out;
    }
    out.push_str("\n  ]\n}\n");
    out
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// JSON 文本 → 行。认三种外形：数组、`{"tasks": […]}`、单个对象。
fn json(text: &str, today: CalendarDate) -> Result<Parsed, ImportError> {
    let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|err| ImportError::Json(err.to_string()))?;
    let items: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(items) => items.iter().collect(),
        serde_json::Value::Object(map) => match map.get("tasks") {
            Some(serde_json::Value::Array(items)) => items.iter().collect(),
            // 单个对象（`{"title": "…"}`）也当一条 —— 用户把一条任务单独存成文件是
            // 常见做法，为此报「不是数组」纯属刁难。
            _ => vec![&value],
        },
        _ => {
            return Err(ImportError::Json(
                "expected an array or an object".to_owned(),
            ));
        }
    };

    let mut rows = Vec::new();
    let mut dropped = 0;
    for item in items {
        let Some(object) = item.as_object() else {
            dropped += 1;
            continue;
        };
        let row = row_from_object(object, today);
        if row.title.is_empty() {
            dropped += 1;
            continue;
        }
        rows.push(row);
    }
    Ok(Parsed {
        rows,
        columns: Vec::new(),
        dropped,
        title_guessed: false,
    })
}

/// 一个 JSON 对象 → [`Row`]。
///
/// 键名走**同一张**别名表，所以 `title` / `任务标题` / `Title` 等价；取值也走**同一套**
/// 解析（`status_of` / `priority_of` / `day_of` …），于是同一个值在 CSV 与 JSON 里
/// 读出来必然一致 —— 两套解析迟早会分叉，分叉了还没人发现。
fn row_from_object(
    object: &serde_json::Map<String, serde_json::Value>,
    today: CalendarDate,
) -> Row {
    let mut row = Row::default();
    let mut title = String::new();
    for (key, value) in object {
        let Some(field) = import_aliases::field_of_header(key) else {
            continue;
        };
        let Some(raw) = text_of(value) else {
            continue;
        };
        let raw = raw.trim();
        match field {
            Field::Title if title.is_empty() => title = raw.to_owned(),
            Field::Project if row.project.is_none() && !raw.is_empty() => {
                row.project = Some(raw.to_owned());
            }
            Field::Status if row.status.is_none() => row.status = import_aliases::status_of(raw),
            Field::Priority if row.priority.is_none() => {
                row.priority = import_aliases::priority_of(raw);
            }
            Field::Quadrant if row.quadrant.is_none() => {
                row.quadrant = import_aliases::quadrant_of(raw);
            }
            Field::Tags if row.tags.is_empty() => row.tags = split_tags(raw),
            Field::Start if row.start.is_none() => row.start = day_of(raw, today),
            Field::Due if row.due.is_none() => row.due = day_of(raw, today),
            Field::Time if row.due_time.is_none() => row.due_time = time_of(raw, today),
            Field::Progress if row.progress.is_none() => {
                row.progress = number_of(raw).map(|value| value.min(100.0) as u8);
            }
            Field::Estimate if row.estimate_hours.is_none() => row.estimate_hours = number_of(raw),
            Field::Actual if row.actual_hours.is_none() => row.actual_hours = number_of(raw),
            Field::Starred if !row.starred => {
                row.starred = import_aliases::bool_of(raw).unwrap_or(false);
            }
            Field::Description if row.description.is_none() && !raw.is_empty() => {
                row.description = Some(raw.to_owned());
            }
            Field::Note if row.note.is_none() && !raw.is_empty() => {
                row.note = Some(raw.to_owned());
            }
            _ => {}
        }
    }
    merge_markers(&mut row, &title, today);
    row
}

/// JSON 取值 → 文本。
///
/// 数字与布尔也接：`{"progress": 35}` 和 `{"starred": true}` 是最自然的写法，
/// 要求用户写成字符串纯属刁难。数组拼成逗号串，交给 `split_tags` 那一套切。
fn text_of(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        serde_json::Value::Array(items) => Some(
            items
                .iter()
                .filter_map(text_of)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        serde_json::Value::Null | serde_json::Value::Object(_) => None,
    }
}

/// JSON 字符串字面量（与 `export.rs` 同一套转义，理由见那里的 `json_string`）。
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
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Field, ImportError, Parsed, Row, SourceFormat, csv, json, markdown, parse, partition,
        template, title_key,
    };
    use duck_project::{CalendarDate, Priority, Quadrant, TaskStatus};

    fn today() -> CalendarDate {
        CalendarDate::new(2026, 10, 14).expect("合法日期")
    }

    fn day(iso: &str) -> CalendarDate {
        CalendarDate::parse(iso).expect("合法日期")
    }

    // -- 格式判定 ----------------------------------------------------------

    #[test]
    fn extensions_map_to_formats_and_junk_is_rejected() {
        for markdown in ["md", "MARKDOWN", "txt", "text", ".md"] {
            assert_eq!(
                SourceFormat::from_extension(markdown),
                Ok(SourceFormat::Markdown),
                "{markdown:?}"
            );
        }
        assert_eq!(SourceFormat::from_extension("csv"), Ok(SourceFormat::Csv));
        assert_eq!(
            SourceFormat::from_extension(" .CSV "),
            Ok(SourceFormat::Csv),
            "带点、带空格的写法也认"
        );
        assert_eq!(SourceFormat::from_extension("json"), Ok(SourceFormat::Json));
        // 认不出时把**裸扩展名**带回去 —— 界面要把它填进「不支持 .{} 文件」。
        assert_eq!(
            SourceFormat::from_extension("xlsx"),
            Err("xlsx".to_owned()),
            "xlsx 不支持，不猜"
        );
        assert_eq!(
            SourceFormat::from_extension(" xlsx "),
            Err("xlsx".to_owned()),
            "回给界面的那一段要 trim 过"
        );
        assert_eq!(SourceFormat::from_extension(""), Err(String::new()));
    }

    #[test]
    fn format_labels_are_stable_ascii() {
        assert_eq!(SourceFormat::Markdown.label(), "Markdown");
        assert_eq!(SourceFormat::Csv.label(), "CSV");
        assert_eq!(SourceFormat::Json.label(), "JSON");
        assert_eq!(SourceFormat::Csv.extension(), "csv");
        assert_eq!(
            SourceFormat::Json.template_file_name(),
            "task-import-template.json"
        );
    }

    // -- Markdown ----------------------------------------------------------

    #[test]
    fn markdown_reads_checklists_plain_lines_and_markers() {
        let text = "\u{feff}# 备忘\n\n- [ ] 修登录 #后端 @Alpha !P1 明天 18:00\n- [x] 写周报\n- 没勾选框的一行\n* 星号项\n+ 加号项\n裸标题\n";
        let parsed = markdown(text, today());
        assert_eq!(parsed.rows.len(), 7, "空行丢掉：{:?}", parsed.rows);
        assert_eq!(parsed.dropped, 0);
        let first = &parsed.rows[0];
        assert_eq!(first.title, "备忘");
        let second = &parsed.rows[1];
        assert_eq!(second.title, "修登录");
        assert_eq!(second.tags, ["后端"]);
        assert_eq!(second.project.as_deref(), Some("Alpha"));
        assert_eq!(second.priority, Some(Priority::P1));
        assert_eq!(second.due, Some(day("2026-10-15")));
        assert_eq!(second.due_time.as_deref(), Some("18:00"));
        assert_eq!(second.status, None, "没勾选框 = 不写状态，落库时取默认");
        assert_eq!(parsed.rows[2].status, Some(TaskStatus::Done));
        assert_eq!(parsed.rows[2].title, "写周报");
        assert_eq!(parsed.rows[3].title, "没勾选框的一行");
        assert_eq!(parsed.rows[4].title, "星号项");
        assert_eq!(parsed.rows[5].title, "加号项");
        assert_eq!(parsed.rows[6].title, "裸标题");
    }

    #[test]
    fn markdown_drops_lines_without_a_title() {
        // 只有记号的整行剥完标题就空了 —— 丢掉并计数，不建一条没标题的任务。
        let parsed = markdown("- [ ] #后端\n- [ ] 有标题的\n- [!] 重要的事\n", today());
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.dropped, 1);
        assert_eq!(parsed.rows[0].title, "有标题的");
        assert_eq!(
            parsed.rows[1].title, "[!] 重要的事",
            "认不出的括号留在标题里"
        );
    }

    #[test]
    fn unknown_brackets_are_not_checkboxes() {
        let parsed = markdown("- [!] 重要的事\n- [x 半截\n", today());
        assert!(!parsed.rows[0].title.is_empty());
        assert_eq!(parsed.rows[0].title, "[!] 重要的事");
        assert_eq!(parsed.rows[0].status, None);
        assert_eq!(parsed.rows[1].title, "[x 半截");
    }

    #[test]
    fn blank_input_imports_nothing() {
        for text in ["", "  \n\n\t\n"] {
            let parsed = markdown(text, today());
            assert!(parsed.rows.is_empty());
            assert_eq!(parsed.dropped, 0, "空行不算「被丢掉的行」");
        }
    }

    // -- CSV 读入 ----------------------------------------------------------

    /// 引号包裹、字段内逗号/换行、`""` 转义 —— RFC 4180 的三件套。
    #[test]
    fn csv_reader_handles_quotes_commas_and_embedded_newlines() {
        let table = super::read_csv("a,\"b,还是b\",c\n\"第一行\n第二行\",\"含\"\"引号\"\"\",,\n");
        assert_eq!(table.len(), 2, "{table:?}");
        assert_eq!(table[0], ["a", "b,还是b", "c"]);
        assert_eq!(table[1][0], "第一行\n第二行");
        assert_eq!(table[1][1], "含\"引号\"");
        assert_eq!(table[1].len(), 4, "末尾两个空字段也保留");
    }

    #[test]
    fn csv_reader_normalizes_bom_crlf_and_drops_blank_rows() {
        let table = super::read_csv("\u{feff}标题,状态\r\n甲,todo\r\n\r\n乙,done\r\n");
        assert_eq!(table.len(), 3, "空行不入表：{table:?}");
        assert_eq!(table[0], ["标题", "状态"], "BOM 不能留进第一个列名");
        assert_eq!(table[2], ["乙", "done"]);
    }

    /// 无表头的 CSV：第一列当标题，其余列「未使用」。
    #[test]
    fn a_headerless_csv_falls_back_to_the_first_column() {
        let parsed = csv("修登录\n写周报\n\n,,\n", today());
        assert_eq!(parsed.rows.len(), 2, "{:?}", parsed.rows);
        assert!(parsed.title_guessed);
        assert_eq!(parsed.rows[0].title, "修登录");
        assert_eq!(parsed.rows[1].title, "写周报");
        assert_eq!(parsed.columns[0].field, Some(Field::Title));
        assert_eq!(
            parsed.columns[0].header, "",
            "合成列名是空的，显示名由界面拼"
        );
    }

    /// 我们自己导出的 CSV 必须能原样导回来（列名 → 字段 → 值，逐项核对）。
    #[test]
    fn our_own_csv_export_round_trips() {
        let text = "id,title,project,status,priority,quadrant,tags,start,due,progress\n\
                    7,\"修\"\"登录\"\"超时\",Alpha,done,P1,Q2,后端 网关,2026-10-13,2026-10-15,40\n";
        let parsed = csv(text, today());
        assert!(!parsed.title_guessed, "标题列明明白白，不该是猜的");
        assert_eq!(parsed.rows.len(), 1);
        let row = &parsed.rows[0];
        assert_eq!(row.title, "修\"登录\"超时", "引号转义要还原");
        assert_eq!(row.project.as_deref(), Some("Alpha"));
        assert_eq!(row.status, Some(TaskStatus::Done));
        assert_eq!(row.priority, Some(Priority::P1));
        assert_eq!(row.quadrant, Some(Quadrant::Q2));
        assert_eq!(row.tags, ["后端", "网关"], "导出用空格连标签，导回要切开");
        assert_eq!(row.start, Some(day("2026-10-13")));
        assert_eq!(row.due, Some(day("2026-10-15")));
        assert_eq!(row.progress, Some(40));
        // `id` 列不导入：认不出，标「未使用」。
        assert_eq!(parsed.columns[0].field, None);
        assert_eq!(parsed.columns[0].header, "id");
    }

    /// 中文表头 + 中文取值。
    #[test]
    fn chinese_headers_and_values_are_understood() {
        let text = "任务标题,项目,状态,优先级,四象限,标签,开始日期,截止日期,进度,备注\n\
                    灰度上线,Alpha,进行中,高,重要不紧急,发布,2026/10/13,2026/10/20,60%,先发 1%\n";
        let parsed = csv(text, today());
        let row = &parsed.rows[0];
        assert_eq!(row.title, "灰度上线");
        assert_eq!(row.project.as_deref(), Some("Alpha"));
        assert_eq!(row.status, Some(TaskStatus::Doing));
        assert_eq!(row.priority, Some(Priority::P1));
        assert_eq!(row.quadrant, Some(Quadrant::Q2));
        assert_eq!(row.tags, ["发布"]);
        assert_eq!(row.start, Some(day("2026-10-13")), "斜杠日期也要认");
        assert_eq!(row.due, Some(day("2026-10-20")));
        assert_eq!(row.progress, Some(60), "带百分号的进度");
        assert_eq!(row.note.as_deref(), Some("先发 1%"));
    }

    /// 同一字段有两列时只认第一列，第二列标「未使用」。
    #[test]
    fn a_duplicate_column_is_left_unused() {
        let parsed = csv("title,due,duedate\n甲,2026-10-15,2026-11-01\n", today());
        let due_columns: Vec<&super::ColumnMap> = parsed
            .columns
            .iter()
            .filter(|column| column.header.starts_with("due"))
            .collect();
        assert_eq!(due_columns[0].field, Some(Field::Due));
        assert_eq!(due_columns[1].field, None);
        assert_eq!(parsed.rows[0].due, Some(day("2026-10-15")));
    }

    /// 列里写的优先于标题里的记号；记号仍然会填空位。
    #[test]
    fn columns_win_over_title_markers() {
        let parsed = csv(
            "title,due,priority\n修登录 #后端 !P3 @Beta 明天,,P0\n只写记号的 #网关 @Gamma !P1 后天,,\n",
            today(),
        );
        let first = &parsed.rows[0];
        assert_eq!(first.title, "修登录", "记号从标题里剥掉");
        assert_eq!(first.tags, ["后端"]);
        assert_eq!(first.project.as_deref(), Some("Beta"));
        assert_eq!(first.due, Some(day("2026-10-15")), "空的截止列让记号说了算");
        assert_eq!(first.priority, Some(Priority::P0), "优先级列有值，压过 !P3");

        let second = &parsed.rows[1];
        assert_eq!(second.title, "只写记号的");
        assert_eq!(second.tags, ["网关"]);
        assert_eq!(second.project.as_deref(), Some("Gamma"));
        assert_eq!(second.priority, Some(Priority::P1));
        assert_eq!(second.due, Some(day("2026-10-16")));
    }

    /// 只有标题列时也照样能用（最小可用的 CSV）。
    #[test]
    fn a_title_only_csv_still_works() {
        let parsed = csv("title\n甲\n乙\n", today());
        assert!(!parsed.title_guessed);
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.columns.len(), 1);
    }

    #[test]
    fn empty_csv_parses_to_nothing() {
        for text in ["", "\n\n", "   \n"] {
            let parsed = csv(text, today());
            assert!(parsed.rows.is_empty());
            assert!(parsed.columns.is_empty());
        }
        // 只有表头、没有数据行：不算猜、也没有行。
        let parsed = csv("title,due\n", today());
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.columns.len(), 2);
        assert!(!parsed.title_guessed);
    }

    /// 全是空白的数据行被读入层就丢掉了（Excel 常导出这种尾巴）。
    #[test]
    fn trailing_blank_rows_are_dropped_before_parsing() {
        let parsed = csv("title\n甲\n,\n,,\n", today());
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.dropped, 0, "空行在读入层就没了，不算被丢掉的行");
    }

    /// 状态为已完成时进度默认 100（不给「已完成但 0%」留口子）。
    #[test]
    fn a_done_row_defaults_to_full_progress() {
        let parsed = csv("title,status,progress\n甲,done,\n乙,done,40\n", today());
        assert_eq!(parsed.rows[0].resolved_progress(), 100);
        assert_eq!(parsed.rows[1].resolved_progress(), 40, "写明了就听写明的");
        assert_eq!(parsed.rows[0].resolved_status(), TaskStatus::Done);
        let parsed = csv("title\n甲\n", today());
        assert_eq!(parsed.rows[0].resolved_status(), TaskStatus::Todo);
        assert_eq!(parsed.rows[0].resolved_progress(), 0);
    }

    // -- JSON --------------------------------------------------------------

    #[test]
    fn json_accepts_array_object_wrapper_and_single_object() {
        let array = json(r#"[{"title":"甲"},{"title":"乙"}]"#, today()).expect("数组");
        assert_eq!(array.rows.len(), 2);

        let wrapped = json(
            r#"{"exportedAt":"2026-10-14","tasks":[{"title":"甲"}]}"#,
            today(),
        )
        .expect("带 tasks 的对象");
        assert_eq!(wrapped.rows.len(), 1);

        let single = json(r#"{"title":"只有一条"}"#, today()).expect("单个对象");
        assert_eq!(single.rows.len(), 1);
        assert_eq!(single.rows[0].title, "只有一条");
    }

    /// 我们自己导出的 JSON 能原样导回来（键名与取值逐项核对）。
    #[test]
    fn our_own_json_export_round_trips() {
        let text = r#"{
          "exportedAt": "2026-10-14",
          "count": 1,
          "tasks": [{
            "id": 6,
            "title": "含\"引号\"的标题",
            "project": "Alpha",
            "status": "done",
            "priority": "P2",
            "quadrant": "Q2",
            "tags": ["后端", "网关"],
            "starred": true,
            "start": null,
            "due": "2026-10-15",
            "progress": 35,
            "descriptionMd": "第一行\n第二行",
            "note": "随手记"
          }]
        }"#;
        let parsed = json(text, today()).expect("合法 JSON");
        assert_eq!(parsed.rows.len(), 1);
        let row = &parsed.rows[0];
        assert_eq!(row.title, "含\"引号\"的标题");
        assert_eq!(row.project.as_deref(), Some("Alpha"));
        assert_eq!(row.status, Some(TaskStatus::Done));
        assert_eq!(row.priority, Some(Priority::P2));
        assert_eq!(row.quadrant, Some(Quadrant::Q2));
        assert_eq!(row.tags, ["后端", "网关"], "JSON 数组标签");
        assert!(row.starred, "布尔取值也接");
        assert_eq!(row.start, None, "null 就是没有");
        assert_eq!(row.due, Some(day("2026-10-15")));
        assert_eq!(row.progress, Some(35), "数字进度也接");
        assert_eq!(row.description.as_deref(), Some("第一行\n第二行"));
        assert_eq!(row.note.as_deref(), Some("随手记"));
    }

    #[test]
    fn json_accepts_chinese_keys_and_values() {
        let text = r#"[{"任务标题":"灰度上线","状态":"进行中","优先级":"高","截止日期":"2026/10/20","标签":"发布, 复盘"}]"#;
        let parsed = json(text, today()).expect("合法 JSON");
        let row = &parsed.rows[0];
        assert_eq!(row.title, "灰度上线");
        assert_eq!(row.status, Some(TaskStatus::Doing));
        assert_eq!(row.priority, Some(Priority::P1));
        assert_eq!(row.due, Some(day("2026-10-20")));
        assert_eq!(row.tags, ["发布", "复盘"]);
    }

    #[test]
    fn json_reports_malformed_input_and_skips_rows_without_a_title() {
        let err = json("{不是 JSON", today()).expect_err("要报错");
        assert!(matches!(err, ImportError::Json(_)));
        assert!(
            err.to_string().starts_with("invalid JSON: "),
            "详情是英文：{err}"
        );

        let parsed = json(r#"[{"note":"没有标题"},{"title":"有标题"}]"#, today()).expect("合法");
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.dropped, 1);
        // 顶层是标量：不是「没有行」，是「这不是一份任务表」。
        assert!(matches!(json("42", today()), Err(ImportError::Json(_))));
    }

    #[test]
    fn the_parse_error_detail_is_english() {
        let err = ImportError::Json("expected value at line 1 column 1".to_owned());
        assert_eq!(
            err.to_string(),
            "invalid JSON: expected value at line 1 column 1"
        );
    }

    // -- 去重 --------------------------------------------------------------

    #[test]
    fn partition_skips_titles_already_in_the_database() {
        let rows = vec![row("甲"), row("乙"), row("甲")];
        let (kept, skipped) = partition(&rows, &[title_key("乙")]);
        assert_eq!(kept.len(), 1, "库里有的 + 批内重复的都要跳过");
        assert_eq!(kept[0].title, "甲");
        assert_eq!(skipped, 2);
    }

    #[test]
    fn partition_ignores_case_and_surrounding_spaces() {
        let rows = vec![row("  Fix Login  ")];
        let (kept, skipped) = partition(&rows, &[title_key("fix login")]);
        assert!(kept.is_empty());
        assert_eq!(skipped, 1);
    }

    #[test]
    fn partition_without_existing_keys_keeps_the_first_of_each_title() {
        let rows = vec![row("甲"), row("甲"), row("乙")];
        let (kept, skipped) = partition(&rows, &[]);
        assert_eq!(kept.len(), 2);
        assert_eq!(skipped, 1);
    }

    fn row(title: &str) -> Row {
        Row {
            title: title.to_owned(),
            ..Row::default()
        }
    }

    // -- 模板 --------------------------------------------------------------

    #[test]
    fn csv_template_quotes_every_cell_and_round_trips() {
        let headers = vec!["title".to_owned(), "due".to_owned()];
        let rows = vec![
            vec!["带,逗号".to_owned(), "2026-10-15".to_owned()],
            vec!["带\"引号\"".to_owned(), String::new()],
        ];
        let text = template(SourceFormat::Csv, &headers, &rows);
        assert_eq!(
            text,
            "\"title\",\"due\"\n\"带,逗号\",\"2026-10-15\"\n\"带\"\"引号\"\"\",\"\"\n"
        );
        // 模板必须能被自己的读入侧读回来 —— 否则「下模板 → 填 → 导入」这条路是假的。
        let parsed = csv(&text, today());
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0].title, "带,逗号");
        assert_eq!(parsed.rows[1].title, "带\"引号\"");
    }

    #[test]
    fn json_template_parses_back_into_the_same_rows() {
        let headers = vec!["title".to_owned(), "status".to_owned()];
        let rows = vec![vec!["灰度上线".to_owned(), "done".to_owned()]];
        let text = template(SourceFormat::Json, &headers, &rows);
        let value: serde_json::Value = serde_json::from_str(&text).expect("模板必须是合法 JSON");
        assert_eq!(value["tasks"][0]["title"], "灰度上线");
        let parsed = json(&text, today()).expect("能被自己的读入侧读回");
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.rows[0].status, Some(TaskStatus::Done));
        // 空行时也要是合法 JSON（不能截断）。
        let empty = template(SourceFormat::Json, &headers, &[]);
        serde_json::from_str::<serde_json::Value>(&empty).expect("空模板也要合法");
        // 中文键照样认。
        let chinese = template(
            SourceFormat::Json,
            &["任务标题".to_owned()],
            &[vec!["甲".to_owned()]],
        );
        assert_eq!(json(&chinese, today()).expect("合法").rows[0].title, "甲");
    }

    #[test]
    fn markdown_template_is_a_checklist_of_the_titles() {
        let headers = vec!["title".to_owned(), "due".to_owned()];
        let rows = vec![
            vec!["甲".to_owned(), "2026-10-15".to_owned()],
            vec!["乙".to_owned(), String::new()],
        ];
        let text = template(SourceFormat::Markdown, &headers, &rows);
        assert_eq!(text, "- [ ] 甲\n- [ ] 乙\n");
        let parsed = markdown(&text, today());
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[1].title, "乙", "模板只带标题，其余列不在清单里");
    }

    // -- 入口 --------------------------------------------------------------

    #[test]
    fn parse_dispatches_on_the_format() {
        let markdown_text = "- [x] 甲\n";
        let csv_text = "title,status\n乙,doing\n";
        let json_text = r#"[{"title":"丙"}]"#;

        let from_markdown =
            parse(markdown_text, SourceFormat::Markdown, today()).expect("Markdown");
        assert_eq!(from_markdown.rows[0].status, Some(TaskStatus::Done));

        let from_csv = parse(csv_text, SourceFormat::Csv, today()).expect("CSV");
        assert_eq!(from_csv.rows[0].status, Some(TaskStatus::Doing));
        assert!(!from_csv.columns.is_empty(), "CSV 才有列映射");

        let from_json = parse(json_text, SourceFormat::Json, today()).expect("JSON");
        assert_eq!(from_json.rows[0].title, "丙");
        assert!(from_json.columns.is_empty(), "JSON 没有列映射");

        assert_eq!(parse("", SourceFormat::Csv, today()), Ok(Parsed::default()));
    }
}
