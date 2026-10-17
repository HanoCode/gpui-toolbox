//! 报表口径：`view.rs::render_report` 画出来的**每一个数字**都在这里算。
//!
//! 与 `export.rs` / `quickparse.rs` / `remind.rs` 同一个待遇 —— **纯函数**（任务切片 +
//! 筛选条件 + 今天进，一份算好的 [`Report`] 出），不碰 gpui、不碰存储。于是「换一档时间
//! 范围，趋势桶数对不对」「平均周期怎么算的」「完成明细截断到几条」这些口径都能在单测里
//! 钉死，而不是靠肉眼看界面数柱子。
//!
//! # 一批任务，两段筛选
//!
//! 报表的每一块都在回答「这批任务怎么样」，而「这批」由筛选条决定。筛选分两段，作用
//! 范围**刻意不同**：
//!
//! - **状态 / 标签** → 整份报表的任务集：KPI、按项目分布、按标签分布、逾期清单、完成
//!   趋势、完成明细，看的都是它；
//! - **时间范围** → 只落在时间轴上：完成趋势与完成明细，外加「已完成」卡附注里那句
//!   「范围内完成 N 个」。
//!
//! 为什么不把时间范围也套在 KPI 上：「进行中 3」说的是**此刻**挂着三件，与它们是不是
//! 最近 7 天里挂上的没有关系。硬套一档范围会让 KPI 随下拉框随机跳，而它本该是一个稳定
//! 的当前快照。同理，「平均周期」算的是**全部已完成**任务的周期（不限于范围内）—— 只取
//! 最近 7 天完成的几条来平均，样本少到几次点击就会让这个数翻倍。
//!
//! # 一笔账只算一次
//!
//! 全部分块共用同一份 `scoped`（过滤后的任务引用），没有任何一块自己去 `tasks` 里再筛
//! 一遍：两个地方各写一份条件，迟早有一处忘记跟着改，报表内部就先自相矛盾了。

use duck_project::{CalendarDate, Project, Tag, Task, TaskStatus};

use super::prefs::ReportRange;
use super::state::local_date;

/// 完成明细最多列几行。
///
/// 报表是**概览**不是台账，滚出几百行没人看；截断后由界面在标题右侧报出真实条数
/// （「{} 项 · 仅列最近 50」），所以截断不会让人误以为只有 50 条。
pub(crate) const DETAIL_LIMIT: usize = 50;

/// 报表的状态筛选档。
///
/// **不落盘**：与工具条那个「显示已完成」同一类一次性视角（「我现在想看看未完成的那些」），
/// 而不是长期偏好。落盘的只有时间范围档，见 `prefs::ReportRange`。
///
/// 四档而不是「已完成 / 已取消」两档：报表上看「还欠着多少」和看「这段时间清了哪些」
/// 是同样常见的两种用法，少一档就得靠两处开关拼出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ReportStatus {
    /// 全部（默认）。
    #[default]
    All,
    /// 未完成（待办 / 进行中 / 阻塞 / 已搁置）。
    Open,
    /// 已完成。
    Done,
    /// 已取消。
    Cancelled,
}

impl ReportStatus {
    /// 全部取值，按筛选条上的顺序。
    pub(crate) const ALL: [Self; 4] = [Self::All, Self::Open, Self::Done, Self::Cancelled];

    /// 某个任务状态算不算命中本档。
    pub(crate) const fn matches(self, status: TaskStatus) -> bool {
        match self {
            Self::All => true,
            Self::Open => !status.is_settled(),
            Self::Done => matches!(status, TaskStatus::Done),
            Self::Cancelled => matches!(status, TaskStatus::Cancelled),
        }
    }
}

/// 报表的筛选条件。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReportFilters {
    /// 时间范围档。
    pub(crate) range: ReportRange,
    /// 状态档。
    pub(crate) status: ReportStatus,
    /// 标签筛选的 id 列表；**空 = 不限**。
    ///
    /// **多选、OR 语义**（命中任意一个就算）—— 与左栏那个「一次只看一个标签」是两种
    /// 用法：左栏是在缩小工作面，报表是在「挑几个标签横向比一比」。内核
    /// `TaskQuery::tags` 走的是 AND，那条路留给左栏，这里在报表层自己收口。
    pub(crate) tags: Vec<i64>,
}

impl ReportFilters {
    /// 「状态 + 标签」两道筛都过吗。
    ///
    /// 时间范围**不在**这里 —— 它只作用于时间轴上的两块，见模块头。
    fn accepts(&self, task: &Task) -> bool {
        if !self.status.matches(task.status) {
            return false;
        }
        if self.tags.is_empty() {
            return true;
        }
        task.tags.iter().any(|tag| self.tags.contains(&tag.id))
    }
}

/// 完成趋势里的一根柱。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrendBar {
    /// 横轴文字（`10/2`；按周分桶时是该周第一天的日期）。
    pub(crate) label: String,
    /// 这一柱里的完成数。
    pub(crate) count: usize,
    /// 要不要画这根柱的横轴文字。
    ///
    /// 30 根柱（近 30 天按日分桶）每根都写 `10/2` 会糊成一团，所以只画其中几根、
    /// 外加最后一根。这条规则放在这里而不是渲染层：单测能直接数「画了几个标签」。
    pub(crate) labelled: bool,
}

/// 一条分布行（「按项目分布」「按标签分布」共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DistributionRow {
    /// 名称（项目名 / 标签名）。**收集箱那一行不在这里**，见 [`Report::inbox`]。
    pub(crate) name: String,
    /// 颜色（`#RRGGBB`）；`None` 由界面用主题色兜底。
    pub(crate) color: Option<String>,
    /// 已完成数。
    pub(crate) done: u32,
    /// 总数。
    pub(crate) total: u32,
}

/// 「完成明细」里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletedRow {
    /// 任务 id（点一行要能选中它）。
    pub(crate) task_id: i64,
    /// 标题。
    pub(crate) title: String,
    /// 所属项目；`None` = 收集箱。
    pub(crate) project_id: Option<i64>,
    /// 所属项目的颜色（查不到就是 `None`）。
    pub(crate) project_color: Option<String>,
    /// 完成日。
    pub(crate) completed: CalendarDate,
    /// 周期（创建日 → 完成日，天）。
    ///
    /// 创建日晚于完成日时是 `None`：那不是「负数天的周期」，是数据本身拧了（改过系统
    /// 时间、或从外部导入）—— 列表里留一个「—」比编一个数诚实。
    pub(crate) cycle_days: Option<i64>,
}

/// 一份算好的报表。
pub(crate) struct Report<'a> {
    /// 当前筛选下的任务数（面板口径说明用）。
    pub(crate) scoped: usize,
    /// 已完成。
    pub(crate) done: usize,
    /// 进行中。
    pub(crate) doing: usize,
    /// 阻塞中。
    pub(crate) blocked: usize,
    /// 范围内完成的个数（「已完成」卡的附注）。
    pub(crate) done_in_range: usize,
    /// 未完成项的平均进度（%）。
    pub(crate) avg_progress: u32,
    /// 逾期任务（按截止日升序）。
    pub(crate) overdue: Vec<&'a Task>,
    /// 最久逾期天数（没有逾期就是 0）。
    pub(crate) max_overdue_days: i64,
    /// 平均周期（天）；没有样本时 `None`。
    pub(crate) avg_cycle_days: Option<i64>,
    /// 平均周期用了几个样本。
    pub(crate) cycle_samples: usize,
    /// 完成趋势（最早一桶在前，最后一桶含今天）。
    pub(crate) trend: Vec<TrendBar>,
    /// 每根趋势柱跨几天（1 = 按日，7 = 按周）。
    pub(crate) bucket_days: i64,
    /// 按项目分布（与 `projects` 同序；即使一个任务都没有也保留那一行 —— 行会随
    /// 筛选增删比行数忽上忽下更难看）。
    pub(crate) by_project: Vec<DistributionRow>,
    /// 收集箱那一行（名称由界面填译文，这一层不认识界面语言）。
    pub(crate) inbox: DistributionRow,
    /// 按标签分布（只留命中的标签，按总数降序）。
    pub(crate) by_tag: Vec<DistributionRow>,
    /// 完成明细（按完成日降序，最多 [`DETAIL_LIMIT`] 条）。
    pub(crate) completed: Vec<CompletedRow>,
    /// 范围内一共完成了多少条（截断前的真实条数）。
    pub(crate) completed_total: usize,
    /// 明细是不是被截断了。
    pub(crate) completed_truncated: bool,
}

impl Report<'_> {
    /// 明细被截断时标题右侧那句话（「{总数} 项 · 仅列最近 {上限}」）；没截断就是 `None`。
    pub(crate) fn detail_truncated_note(&self) -> Option<(usize, usize)> {
        self.completed_truncated
            .then_some((self.completed_total, DETAIL_LIMIT))
    }
}

/// 算一份报表。
///
/// `projects` / `tags` 只用来把 id 翻成名字与颜色，并决定分布行的**排列基准**：报表是
/// 「这里有哪些项目 / 标签」的一张固定表，缺了谁就少一行，会让两张截图对不上。
pub(crate) fn build<'a>(
    tasks: &'a [Task],
    today: CalendarDate,
    filters: &ReportFilters,
    projects: &[Project],
    tags: &[Tag],
) -> Report<'a> {
    let scoped: Vec<&Task> = tasks.iter().filter(|task| filters.accepts(task)).collect();

    let count = |status: TaskStatus| scoped.iter().filter(|task| task.status == status).count();
    let open: Vec<&&Task> = scoped
        .iter()
        .filter(|task| !task.status.is_settled())
        .collect();
    let avg_progress = if open.is_empty() {
        0
    } else {
        open.iter()
            .map(|task| u32::from(task.progress))
            .sum::<u32>()
            / open.len() as u32
    };

    let from = today.add_days(-(window_days(filters.range) - 1));
    let is_done_within = |task: &Task| {
        task.status == TaskStatus::Done
            && task
                .completed_at
                .map(local_date)
                .is_some_and(|day| day >= from)
    };
    let done_in_range = scoped.iter().filter(|task| is_done_within(task)).count();

    let mut overdue: Vec<&Task> = scoped
        .iter()
        .copied()
        .filter(|task| task.is_overdue(today))
        .collect();
    overdue.sort_by_key(|task| task.due_date.map_or(0, CalendarDate::days_since_epoch));
    let max_overdue_days = overdue
        .iter()
        .filter_map(|task| task.due_date)
        .map(|due| due.days_until(today))
        .max()
        .unwrap_or(0);

    // 平均周期：全部已完成任务的「创建日 → 完成日」，四舍五入到整天。
    // 整数运算（先加半个样本数再整除）而不是过一遍 f64：报表上是一位数级的量，
    // 浮点只会多一层「0.5 到底进不进」的疑问。
    let cycles: Vec<i64> = scoped
        .iter()
        .filter(|task| task.status == TaskStatus::Done)
        .filter_map(|task| cycle_days(task))
        .collect();
    let avg_cycle_days = if cycles.is_empty() {
        None
    } else {
        let samples = cycles.len() as i64;
        Some((cycles.iter().sum::<i64>() + samples / 2) / samples)
    };

    let trend = trend_bars(&scoped, today, filters.range);

    let by_project = projects
        .iter()
        .map(|project| {
            let rows: Vec<&&Task> = scoped
                .iter()
                .filter(|task| task.project_id == Some(project.id))
                .collect();
            distribution(project.name.clone(), Some(project.color.clone()), &rows)
        })
        .collect();
    let inbox_rows: Vec<&&Task> = scoped
        .iter()
        .filter(|task| task.project_id.is_none())
        .collect();
    let inbox = distribution(String::new(), None, &inbox_rows);

    let mut by_tag: Vec<DistributionRow> = tags
        .iter()
        .filter_map(|tag| {
            let rows: Vec<&&Task> = scoped
                .iter()
                .filter(|task| task.tags.iter().any(|bound| bound.id == tag.id))
                .collect();
            (!rows.is_empty())
                .then(|| distribution(tag.name.clone(), Some(tag.color.clone()), &rows))
        })
        .collect();
    // 多的在前：这是一张「哪几个标签最沉」的图，按名称排反而把重点排到最后一行。
    // 名称只做同数量时的稳定次序（`sort_by` 是稳定排序，但显式给出来更清楚）。
    by_tag.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.name.cmp(&b.name)));

    let mut completed: Vec<CompletedRow> = scoped
        .iter()
        .filter_map(|task| {
            if task.status != TaskStatus::Done || !is_done_within(task) {
                return None;
            }
            let completed = local_date(task.completed_at?);
            let project = task
                .project_id
                .and_then(|id| projects.iter().find(|project| project.id == id));
            Some(CompletedRow {
                task_id: task.id,
                title: task.title.clone(),
                project_id: task.project_id,
                project_color: project.map(|project| project.color.clone()),
                completed,
                cycle_days: cycle_days(task),
            })
        })
        .collect();
    // 完成日相同时按 id 倒序：新建的排前面，两次「导出报表」之间的顺序才是确定的
    // （`sort_by` 虽然稳定，但稳定所依赖的输入顺序来自 SQL 的排序口径，不该被依赖）。
    completed.sort_by(|a, b| {
        b.completed
            .cmp(&a.completed)
            .then_with(|| b.task_id.cmp(&a.task_id))
    });
    let completed_total = completed.len();
    let completed_truncated = completed_total > DETAIL_LIMIT;
    completed.truncate(DETAIL_LIMIT);

    Report {
        scoped: scoped.len(),
        done: count(TaskStatus::Done),
        doing: count(TaskStatus::Doing),
        blocked: count(TaskStatus::Blocked),
        done_in_range,
        avg_progress,
        overdue,
        max_overdue_days,
        avg_cycle_days,
        cycle_samples: cycles.len(),
        trend,
        bucket_days: filters.range.bucket_days(),
        by_project,
        inbox,
        by_tag,
        completed,
        completed_total,
        completed_truncated,
    }
}

/// 一行分布。
fn distribution(name: String, color: Option<String>, rows: &[&&Task]) -> DistributionRow {
    DistributionRow {
        name,
        color,
        done: rows
            .iter()
            .filter(|task| task.status == TaskStatus::Done)
            .count() as u32,
        total: rows.len() as u32,
    }
}

/// 创建日 → 完成日的天数；创建日晚于完成日（数据拧了）时 `None`。
fn cycle_days(task: &Task) -> Option<i64> {
    let completed = local_date(task.completed_at?);
    let created = local_date(task.created_at);
    let days = created.days_until(completed);
    (days >= 0).then_some(days)
}

/// 报表往回看的天数。
///
/// 「近 90 天」按周分桶时是 13 桶，正好 91 天 —— 取整到整桶而不是把最后一桶切一半：
/// 切一半会让「这一周完成了几个」变成一个只统计了三四天的数。所以窗口 = 桶数 × 每桶
/// 天数，`from` 与趋势图用的是**同一个窗口**（否则明细里会冒出一条柱子里看不见的完成，
/// 或者反过来）。
pub(crate) fn window_days(range: ReportRange) -> i64 {
    let bucket = range.bucket_days();
    // 向上取整到整桶：`div_ceil` 在 i64 上还不是稳定功能，所以自己算。
    (range.days() + bucket - 1) / bucket * bucket
}

/// 完成趋势的分桶。
///
/// 桶数 = 窗口天数 / 每桶天数；`range.bucket_days()` 让近 7 / 30 天按日、近 90 天按周
/// —— 90 根柱子在几百像素宽的面板里既画不清也读不出，按周合并后是 13 根。
fn trend_bars(scoped: &[&Task], today: CalendarDate, range: ReportRange) -> Vec<TrendBar> {
    let bucket = range.bucket_days();
    let buckets = (window_days(range) / bucket).max(1);
    let stride = label_stride(buckets as usize);
    // 「离上一根写了字的柱子隔了几根」：用一个自己归零的计数器，而不是 `index % stride`。
    // 后者会踩 clippy 的 `manual_is_multiple_of`（而 `is_multiple_of` 又把 MSRV 顶到
    // 1.87，本仓钉的是 1.85），计数器的写法两边都不欠。
    let mut since_label = 0usize;
    let mut bars = Vec::with_capacity(buckets as usize);
    for index in 0..buckets {
        // 第 0 桶是最早的一桶：它离今天 `buckets - 1 - index` 个桶。
        let back = (buckets - 1 - index) * bucket;
        let start = today.add_days(-(back + bucket - 1));
        let end = today.add_days(-back);
        let count = scoped
            .iter()
            .filter(|task| {
                task.status == TaskStatus::Done
                    && task
                        .completed_at
                        .map(local_date)
                        .is_some_and(|day| day >= start && day <= end)
            })
            .count();
        bars.push(TrendBar {
            // 横轴写桶的**起点**日期：按周分桶时就是那一周的周一，与日历视图里
            // 「这一周」的起点是同一个日子。
            label: format!("{}/{}", start.month(), start.day()),
            count,
            labelled: since_label == 0 || index + 1 == buckets,
        });
        since_label += 1;
        if since_label == stride {
            since_label = 0;
        }
    }
    bars
}

/// 键盘横轴文字每隔几根画一根（约合每屏 7 根）。
///
/// 取「向上取整到至少 1」，于是 7 根柱时每根都有字（stride 1）、30 根时 5 根一标
/// （stride 5）、13 根时 2 根一标（stride 2）。
fn label_stride(buckets: usize) -> usize {
    buckets.div_ceil(7).max(1)
}

// ---------------------------------------------------------------------------
// 导出报表（Markdown）
// ---------------------------------------------------------------------------

/// 导出那份 Markdown 要用到的**全部文案**。
///
/// 一个字段一句话地摆出来，而不是在 [`to_markdown`] 里写中文字面量：报表这一层
/// （`report.rs`）与 `export.rs` 同一个待遇 —— 它不知道界面语言。文案由界面层用
/// `tr` 取好传进来，于是同一个函数既能出中文报表也能出英文报表，而单测里可以塞
/// 一组假标签把结构钉死。
pub(crate) struct ReportLabels<'a> {
    /// 标题（「任务报表」）。
    pub(crate) title: &'a str,
    /// 口径那一行的前缀（「口径」）。
    pub(crate) scope: &'a str,
    /// 「概览」。
    pub(crate) overview: &'a str,
    /// 「指标」。
    pub(crate) metric: &'a str,
    /// 「数值」。
    pub(crate) value: &'a str,
    /// 「完成趋势」。
    pub(crate) trend: &'a str,
    /// 「日期」。
    pub(crate) date: &'a str,
    /// 「完成」（趋势表的数据列）。
    pub(crate) count: &'a str,
    /// 「按项目分布」。
    pub(crate) by_project: &'a str,
    /// 「按标签分布」。
    pub(crate) by_tag: &'a str,
    /// 「名称」（项目表与标签表共用的首列）。
    pub(crate) name: &'a str,
    /// 「完成」（分布表的数据列）。
    pub(crate) done: &'a str,
    /// 「总数」。
    pub(crate) total: &'a str,
    /// 「逾期清单」。
    pub(crate) overdue: &'a str,
    /// 「完成明细」。
    pub(crate) completed: &'a str,
    /// 「任务」。
    pub(crate) task: &'a str,
    /// 「项目」。
    pub(crate) project: &'a str,
    /// 「逾期天数」。
    pub(crate) overdue_days: &'a str,
    /// 「完成日」。
    pub(crate) completed_on: &'a str,
    /// 「周期（天）」。
    pub(crate) cycle_days: &'a str,
    /// 表格没有数据时那一格（「（无）」）。
    pub(crate) empty: &'a str,
}

/// 导出报表的上下文：文案之外那几样由界面层拼好的东西。
pub(crate) struct ReportExport<'a> {
    /// 文案。
    pub(crate) labels: ReportLabels<'a>,
    /// 「状态「全部」· 标签「不限」· 时间范围「近 7 天」· 共 18 项」那一行。
    pub(crate) scope_line: String,
    /// 概览表的（指标, 数值）两列；数值里**已经含附注**（「12（范围内完成 3 个）」），
    /// 于是表只要两列、也少一套要翻译的表头。
    pub(crate) kpi_rows: Vec<(String, String)>,
    /// 分布面板底下那行（「平均进度（未完成项）40%」）；空串就不写。
    pub(crate) distribution_note: String,
    /// 完成明细被截断时那句（「共 120 项 · 仅列最近 50」）；`None` 不写。
    pub(crate) completed_note: Option<String>,
    /// 收集箱那一行的名字（「未归入项目」）。
    pub(crate) inbox_name: String,
    /// 用来把 `project_id` 翻成项目名。
    pub(crate) projects: &'a [Project],
}

/// 把一份报表拼成 Markdown。
///
/// 结构照抄界面上的分块顺序（概览 → 趋势 → 项目分布 → 标签分布 → 逾期 → 完成明细），
/// 于是「导出的那份」和「屏幕上那份」是同一份东西的两种排法，不会各说各话。
pub(crate) fn to_markdown(
    report: &Report<'_>,
    today: CalendarDate,
    export: &ReportExport<'_>,
) -> String {
    let labels = &export.labels;
    let mut out = format!("# {} · {}\n\n", labels.title, today.to_iso());
    out.push_str(&format!("> {}：{}\n\n", labels.scope, export.scope_line));

    out.push_str(&format!("## {}\n\n", labels.overview));
    out.push_str(&two_column_header(labels.metric, labels.value));
    for (name, value) in &export.kpi_rows {
        out.push_str(&format!(
            "| {} | {} |\n",
            markdown_cell(name),
            markdown_cell(value)
        ));
    }
    out.push('\n');

    out.push_str(&format!("## {}\n\n", labels.trend));
    out.push_str(&two_column_header(labels.date, labels.count));
    for bar in &report.trend {
        out.push_str(&format!("| {} | {} |\n", bar.label, bar.count));
    }
    out.push('\n');

    out.push_str(&format!("## {}\n\n", labels.by_project));
    out.push_str(&distribution_header(labels));
    for row in report
        .by_project
        .iter()
        .chain(std::iter::once(&report.inbox))
    {
        out.push_str(&distribution_row(row, &export.inbox_name));
    }
    out.push('\n');

    out.push_str(&format!("## {}\n\n", labels.by_tag));
    if report.by_tag.is_empty() {
        out.push_str(&format!("{}\n\n", labels.empty));
    } else {
        out.push_str(&distribution_header(labels));
        for row in &report.by_tag {
            out.push_str(&distribution_row(row, &export.inbox_name));
        }
        out.push('\n');
    }

    if !export.distribution_note.is_empty() {
        out.push_str(&format!("{}\n\n", export.distribution_note));
    }

    out.push_str(&format!("## {}\n\n", labels.overdue));
    if report.overdue.is_empty() {
        out.push_str(&format!("{}\n\n", labels.empty));
    } else {
        out.push_str(&format!(
            "| {} | {} | {} |\n| --- | --- | --- |\n",
            labels.task, labels.project, labels.overdue_days
        ));
        for task in &report.overdue {
            let project = project_name(export.projects, task.project_id);
            let days = task.due_date.map_or(0, |due| due.days_until(today));
            out.push_str(&format!(
                "| {} | {} | {days} |\n",
                markdown_cell(&task.title),
                markdown_cell(&project)
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!("## {}\n\n", labels.completed));
    if report.completed.is_empty() {
        out.push_str(&format!("{}\n\n", labels.empty));
    } else {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n| --- | --- | --- | --- |\n",
            labels.task, labels.project, labels.completed_on, labels.cycle_days
        ));
        for row in &report.completed {
            let project = project_name(export.projects, row.project_id);
            let cycle = row
                .cycle_days
                .map_or_else(|| "—".to_owned(), |days| days.to_string());
            out.push_str(&format!(
                "| {} | {} | {} | {cycle} |\n",
                markdown_cell(&row.title),
                markdown_cell(&project),
                row.completed.to_iso()
            ));
        }
        out.push('\n');
    }

    if let Some(note) = &export.completed_note {
        out.push_str(&format!("{note}\n\n"));
    }

    out
}

fn two_column_header(first: &str, second: &str) -> String {
    format!("| {first} | {second} |\n| --- | --- |\n")
}

fn distribution_header(labels: &ReportLabels<'_>) -> String {
    format!(
        "| {} | {} | {} |\n| --- | --- | --- |\n",
        labels.name, labels.done, labels.total
    )
}

fn distribution_row(row: &DistributionRow, inbox_name: &str) -> String {
    let name = if row.name.is_empty() {
        inbox_name
    } else {
        row.name.as_str()
    };
    format!(
        "| {} | {} | {} |\n",
        markdown_cell(name),
        row.done,
        row.total
    )
}

fn project_name(projects: &[Project], id: Option<i64>) -> String {
    id.and_then(|id| {
        projects
            .iter()
            .find(|project| project.id == id)
            .map(|project| project.name.clone())
    })
    .unwrap_or_default()
}

/// Markdown 表格里的一格。
///
/// 竖线是列分隔符、换行会断行 —— 任务标题里两个都可能出现（描述是 Markdown、标题
/// 是自由文本），不转义就会让一行数据变成两行、后面所有列全错位。
fn markdown_cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_project::{Priority, Quadrant, SubtaskProgress};

    /// 2026-10-14 是**周三**（与 `state.rs` 的单测固定日一致）。
    fn today() -> CalendarDate {
        CalendarDate::new(2026, 10, 14).expect("合法日期")
    }

    fn day(text: &str) -> CalendarDate {
        CalendarDate::parse(text).expect("合法 ISO 日期")
    }

    /// 某个日历日的**本地正午**对应的 Unix 秒。
    ///
    /// 不能直接写 `days_since_epoch * 86400`（那是 UTC 午夜）：`local_date` 会按本地时区
    /// 换算回来，在 UTC 以西的机器上会退到前一天。先按该日的起点取一次时区偏移、把时间
    /// 推到当地正午，无论机器在哪个时区（偏移 −12 到 +14 之内）都落在同一天。
    fn noon_on(date: CalendarDate) -> i64 {
        let base = date.days_since_epoch() * 86_400;
        base + 43_200 - i64::from(duck_platform::local_utc_offset_seconds(base))
    }

    fn task(id: i64, status: TaskStatus) -> Task {
        Task {
            id,
            project_id: None,
            parent_id: None,
            title: format!("任务 {id}"),
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

    fn tag(id: i64, name: &str) -> Tag {
        Tag {
            id,
            name: name.to_owned(),
            color: "#112233".to_owned(),
            is_favorite: false,
            usage_count: 0,
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

    /// 把任务标成「在某天完成」；`cycle` 是创建日到完成日的天数（负数 = 数据拧了）。
    fn completed_on(mut task: Task, text: &str, cycle: i64) -> Task {
        let finished = day(text);
        task.status = TaskStatus::Done;
        task.completed_at = Some(noon_on(finished));
        task.created_at = noon_on(finished.add_days(-cycle));
        task
    }

    fn filters() -> ReportFilters {
        ReportFilters::default()
    }

    #[test]
    fn status_filters_split_open_from_settled() {
        assert!(ReportStatus::Open.matches(TaskStatus::Todo));
        assert!(ReportStatus::Open.matches(TaskStatus::Doing));
        assert!(ReportStatus::Open.matches(TaskStatus::Blocked));
        assert!(ReportStatus::Open.matches(TaskStatus::Deferred));
        assert!(!ReportStatus::Open.matches(TaskStatus::Done));
        assert!(!ReportStatus::Open.matches(TaskStatus::Cancelled));
        assert!(ReportStatus::Done.matches(TaskStatus::Done));
        assert!(!ReportStatus::Done.matches(TaskStatus::Cancelled));
        assert!(ReportStatus::Cancelled.matches(TaskStatus::Cancelled));
        for status in [
            TaskStatus::Todo,
            TaskStatus::Done,
            TaskStatus::Cancelled,
            TaskStatus::Deferred,
        ] {
            assert!(ReportStatus::All.matches(status), "{status:?}");
        }
    }

    #[test]
    fn the_status_filter_narrows_every_block() {
        let tasks = vec![
            task(1, TaskStatus::Todo),
            task(2, TaskStatus::Doing),
            completed_on(task(3, TaskStatus::Todo), "2026-10-13", 4),
        ];
        let report = build(
            &tasks,
            today(),
            &ReportFilters {
                status: ReportStatus::Open,
                ..filters()
            },
            &[],
            &[],
        );
        assert_eq!(report.scoped, 2);
        assert_eq!(report.done, 0);
        assert_eq!(report.doing, 1);
        assert_eq!(report.completed_total, 0, "已完成的不在「未完成」这一档里");
        assert_eq!(report.done_in_range, 0);
    }

    #[test]
    fn the_tag_filter_is_an_or_over_the_selected_tags() {
        let mut first = task(1, TaskStatus::Todo);
        first.tags = vec![tag(7, "后端")];
        let mut second = task(2, TaskStatus::Todo);
        second.tags = vec![tag(8, "前端")];
        let mut third = task(3, TaskStatus::Todo);
        third.tags = vec![tag(7, "后端"), tag(8, "前端")];
        let tasks = vec![first, second, third];

        let report = build(
            &tasks,
            today(),
            &ReportFilters {
                tags: vec![7],
                ..filters()
            },
            &[],
            &[],
        );
        assert_eq!(report.scoped, 2, "勾了「后端」，命中 1 与 3");

        let report = build(
            &tasks,
            today(),
            &ReportFilters {
                tags: vec![7, 8],
                ..filters()
            },
            &[],
            &[],
        );
        assert_eq!(report.scoped, 3, "两个都勾是 OR，不是 AND");
    }

    #[test]
    fn the_trend_bucket_count_follows_the_range() {
        let tasks = vec![task(1, TaskStatus::Todo)];
        for (range, buckets, bucket_days) in [
            (ReportRange::Days7, 7usize, 1i64),
            (ReportRange::Days30, 30, 1),
            (ReportRange::Days90, 13, 7),
        ] {
            let report = build(
                &tasks,
                today(),
                &ReportFilters { range, ..filters() },
                &[],
                &[],
            );
            assert_eq!(report.trend.len(), buckets, "{range:?}");
            assert_eq!(report.bucket_days, bucket_days, "{range:?}");
        }
    }

    #[test]
    fn the_last_trend_bucket_ends_today() {
        // 今天完成的那一条必须落在最后一根柱子里（近 7 天时是第 7 根）。
        let tasks = vec![completed_on(task(1, TaskStatus::Todo), "2026-10-14", 1)];
        let report = build(&tasks, today(), &filters(), &[], &[]);
        assert_eq!(report.trend.len(), 7);
        assert_eq!(report.trend[6].count, 1);
        assert_eq!(report.trend[6].label, "10/14", "最后一根就是今天");
        assert!(report.trend[6].labelled, "最后一根总要写字");
        assert_eq!(report.trend[0].label, "10/8", "近 7 天从 10-8 起");
        assert_eq!(report.trend.iter().map(|bar| bar.count).sum::<usize>(), 1);
    }

    #[test]
    fn a_completion_older_than_the_range_falls_out_of_the_trend() {
        let tasks = vec![completed_on(task(1, TaskStatus::Todo), "2026-10-01", 3)];
        let recent = build(&tasks, today(), &filters(), &[], &[]);
        assert_eq!(recent.done_in_range, 0);
        assert_eq!(recent.trend.iter().map(|bar| bar.count).sum::<usize>(), 0);
        assert_eq!(recent.completed_total, 0);

        let wide = build(
            &tasks,
            today(),
            &ReportFilters {
                range: ReportRange::Days30,
                ..filters()
            },
            &[],
            &[],
        );
        assert_eq!(wide.done_in_range, 1, "10-01 在近 30 天里");
        assert_eq!(wide.completed_total, 1);
    }

    #[test]
    fn the_week_buckets_add_up_to_the_range() {
        // 近 90 天按周分桶：13 桶 × 7 天 = 91 天（见 `window_days`）。窗口里 14 条
        // 全部命中；100 天前那条落在窗口外，一根柱子都不该有它。
        let mut tasks: Vec<Task> = (0..14)
            .map(|back| {
                let date = today().add_days(-back);
                completed_on(task(back + 1, TaskStatus::Todo), &date.to_iso(), 1)
            })
            .collect();
        let stale = today().add_days(-100);
        tasks.push(completed_on(
            task(900, TaskStatus::Todo),
            &stale.to_iso(),
            1,
        ));

        let report = build(
            &tasks,
            today(),
            &ReportFilters {
                range: ReportRange::Days90,
                ..filters()
            },
            &[],
            &[],
        );
        assert_eq!(report.trend.len(), 13);
        assert_eq!(
            report.trend.iter().map(|bar| bar.count).sum::<usize>(),
            14,
            "100 天前那条在 91 天窗口之外"
        );
        assert_eq!(report.completed_total, 14, "明细与趋势用同一个窗口");
        let labels = report.trend.iter().filter(|bar| bar.labelled).count();
        assert_eq!(labels, 7, "13 根柱按 stride 2 标 7 个字");
    }

    #[test]
    fn the_window_rounds_up_to_whole_buckets() {
        assert_eq!(window_days(ReportRange::Days7), 7);
        assert_eq!(window_days(ReportRange::Days30), 30);
        assert_eq!(window_days(ReportRange::Days90), 91, "13 周，不切半周");
    }

    #[test]
    fn the_average_cycle_ignores_tasks_without_a_sane_span() {
        let tasks = vec![
            // 3 天与 5 天 → 平均 4。
            completed_on(task(1, TaskStatus::Todo), "2026-10-13", 3),
            completed_on(task(2, TaskStatus::Todo), "2026-10-10", 5),
            // 未完成的没资格算周期。
            task(3, TaskStatus::Doing),
        ];
        let report = build(&tasks, today(), &filters(), &[], &[]);
        assert_eq!(report.avg_cycle_days, Some(4));
        assert_eq!(report.cycle_samples, 2);
    }

    #[test]
    fn the_average_cycle_rounds_half_up_and_reports_no_sample_as_none() {
        let tasks = vec![
            completed_on(task(1, TaskStatus::Todo), "2026-10-13", 3),
            completed_on(task(2, TaskStatus::Todo), "2026-10-12", 4),
        ];
        let report = build(&tasks, today(), &filters(), &[], &[]);
        assert_eq!(report.avg_cycle_days, Some(4), "3.5 进位到 4");

        let empty = build(&[], today(), &filters(), &[], &[]);
        assert_eq!(empty.avg_cycle_days, None);
        assert_eq!(empty.cycle_samples, 0);
    }

    #[test]
    fn a_creation_date_after_the_completion_is_not_a_negative_cycle() {
        let task = completed_on(task(1, TaskStatus::Todo), "2026-10-13", -2);
        let tasks = [task];
        let report = build(&tasks, today(), &filters(), &[], &[]);
        assert_eq!(report.avg_cycle_days, None, "数据拧了就不算，不编一个负数");
        assert_eq!(report.cycle_samples, 0);
        assert_eq!(report.completed[0].cycle_days, None);
    }

    #[test]
    fn overdue_rows_are_open_tasks_sorted_by_due_date() {
        let mut later = task(1, TaskStatus::Todo);
        later.due_date = Some(day("2026-10-13"));
        let mut earlier = task(2, TaskStatus::Doing);
        earlier.due_date = Some(day("2026-10-09"));
        let mut finished = completed_on(task(3, TaskStatus::Todo), "2026-10-12", 1);
        finished.due_date = Some(day("2026-10-01"));
        let mut future = task(4, TaskStatus::Todo);
        future.due_date = Some(day("2026-10-20"));

        let tasks = [later, earlier, finished, future];
        let report = build(&tasks, today(), &filters(), &[], &[]);
        let ids: Vec<i64> = report.overdue.iter().map(|task| task.id).collect();
        assert_eq!(ids, vec![2, 1], "只算未完成的，且按截止日升序");
        assert_eq!(report.max_overdue_days, 5);
    }

    #[test]
    fn the_distribution_keeps_every_project_and_hides_untouched_tags() {
        let mut alpha = task(1, TaskStatus::Done);
        alpha.project_id = Some(10);
        alpha.tags = vec![tag(7, "后端")];
        let mut beta = task(2, TaskStatus::Todo);
        beta.project_id = Some(10);
        let inbox = task(3, TaskStatus::Todo);
        let mut tagged = task(4, TaskStatus::Todo);
        tagged.project_id = Some(20);
        tagged.tags = vec![tag(7, "后端"), tag(8, "前端")];

        let projects = vec![
            project(10, "Alpha"),
            project(20, "Beta"),
            project(30, "Gamma"),
        ];
        let tags = vec![tag(7, "后端"), tag(8, "前端"), tag(9, "没用到")];
        let tasks = [alpha, beta, inbox, tagged];
        let report = build(&tasks, today(), &filters(), &projects, &tags);

        assert_eq!(report.by_project.len(), 3, "零任务的项目也留一行");
        assert_eq!(report.by_project[0].name, "Alpha");
        assert_eq!(
            (report.by_project[0].done, report.by_project[0].total),
            (1, 2)
        );
        assert_eq!(
            (report.by_project[1].done, report.by_project[1].total),
            (0, 1)
        );
        assert_eq!(
            (report.by_project[2].done, report.by_project[2].total),
            (0, 0)
        );
        assert_eq!((report.inbox.done, report.inbox.total), (0, 1));
        assert!(report.inbox.name.is_empty(), "收集箱那一行的名字由界面填");

        assert_eq!(report.by_tag.len(), 2, "一个任务都没有的标签不占行");
        assert_eq!(report.by_tag[0].name, "后端");
        assert_eq!(report.by_tag[0].total, 2);
        assert_eq!(report.by_tag[1].name, "前端");
    }

    #[test]
    fn the_completion_detail_is_sorted_newest_first_and_capped() {
        let mut tasks: Vec<Task> = (0..DETAIL_LIMIT as i64 + 3)
            .map(|index| {
                // 全部落在同一天：靠 id 倒序把次序钉死。
                completed_on(task(index + 1, TaskStatus::Todo), "2026-10-13", 2)
            })
            .collect();
        tasks.push(completed_on(task(999, TaskStatus::Todo), "2026-10-14", 2));

        let report = build(&tasks, today(), &filters(), &[], &[]);
        assert_eq!(report.completed_total, DETAIL_LIMIT + 4);
        assert!(report.completed_truncated);
        assert_eq!(report.completed.len(), DETAIL_LIMIT);
        assert_eq!(report.completed[0].task_id, 999, "今天完成的最靠前");
        assert_eq!(report.completed[1].task_id, DETAIL_LIMIT as i64 + 3);
        assert_eq!(
            report.detail_truncated_note(),
            Some((DETAIL_LIMIT + 4, DETAIL_LIMIT))
        );

        let short = build(&tasks[..2], today(), &filters(), &[], &[]);
        assert!(!short.completed_truncated);
        assert_eq!(short.detail_truncated_note(), None);
    }

    #[test]
    fn the_average_progress_only_counts_open_tasks() {
        let mut doing = task(1, TaskStatus::Doing);
        doing.progress = 30;
        let mut blocked = task(2, TaskStatus::Blocked);
        blocked.progress = 50;
        let mut done = task(3, TaskStatus::Todo);
        done.progress = 100;
        done.status = TaskStatus::Done;

        let tasks = [doing, blocked, done];
        let report = build(&tasks, today(), &filters(), &[], &[]);
        assert_eq!(report.avg_progress, 40, "(30 + 50) / 2，已完成的不算");
    }

    #[test]
    fn label_stride_keeps_about_seven_labels() {
        assert_eq!(label_stride(7), 1);
        assert_eq!(label_stride(8), 2);
        assert_eq!(label_stride(13), 2);
        assert_eq!(label_stride(30), 5);
    }

    /// 一组**假标签**：导出测试只关心结构，文案是不是中文无关紧要，所以这里刻意用
    /// 一眼能认出的记号 —— 断言里出现 `[[A1]]` 时，读到的人不会以为它是真文案。
    fn labels() -> ReportLabels<'static> {
        ReportLabels {
            title: "[[T]]",
            scope: "[[S]]",
            overview: "[[A1]]",
            metric: "[[A2]]",
            value: "[[A3]]",
            trend: "[[B1]]",
            date: "[[B2]]",
            count: "[[B3]]",
            by_project: "[[C1]]",
            by_tag: "[[C2]]",
            name: "[[C3]]",
            done: "[[C4]]",
            total: "[[C5]]",
            overdue: "[[D1]]",
            completed: "[[D2]]",
            task: "[[D3]]",
            project: "[[D4]]",
            overdue_days: "[[D5]]",
            completed_on: "[[D6]]",
            cycle_days: "[[D7]]",
            empty: "[[E]]",
        }
    }

    #[test]
    fn the_markdown_report_carries_every_block() {
        let mut alpha = task(1, TaskStatus::Done);
        alpha.title = "写完 | 带竖线的标题".to_owned();
        alpha.project_id = Some(10);
        alpha.tags = vec![tag(7, "后端")];
        let finished = day("2026-10-13");
        let alpha = completed_on(alpha, &finished.to_iso(), 3);

        let mut late = task(2, TaskStatus::Todo);
        late.due_date = Some(day("2026-10-10"));
        late.project_id = Some(10);

        let inbox = task(3, TaskStatus::Todo);
        let projects = vec![project(10, "Alpha")];
        let tags = vec![tag(7, "后端")];

        let tasks = [alpha, late, inbox];
        let report = build(&tasks, today(), &filters(), &projects, &tags);
        let export = ReportExport {
            labels: labels(),
            scope_line: "[[scoped]]".to_owned(),
            kpi_rows: vec![("[[k]]".to_owned(), "12".to_owned())],
            distribution_note: "[[note]]".to_owned(),
            completed_note: Some("[[truncated]]".to_owned()),
            inbox_name: "[[inbox]]".to_owned(),
            projects: &projects,
        };
        let text = to_markdown(&report, today(), &export);

        assert!(text.starts_with("# [[T]] · 2026-10-14\n\n"), "{text}");
        assert!(text.contains("> [[S]]：[[scoped]]\n"), "{text}");
        for heading in ["[[A1]]", "[[B1]]", "[[C1]]", "[[C2]]", "[[D1]]", "[[D2]]"] {
            assert!(
                text.contains(&format!("## {heading}\n")),
                "缺 {heading}：{text}"
            );
        }
        assert!(text.contains("| [[k]] | 12 |"), "{text}");
        assert!(text.contains("| [[A2]] | [[A3]] |"), "{text}");
        assert!(text.contains("| [[B2]] | [[B3]] |"), "{text}");
        assert!(text.contains("| [[C3]] | [[C4]] | [[C5]] |"), "{text}");
        // 7 根趋势柱各一行（近 7 天每根都有标签）。
        assert_eq!(report.trend.len(), 7);
        assert!(text.contains("| 10/8 | 0 |"), "{text}");
        assert!(text.contains("| Alpha | 1 | 2 |"), "{text}");
        assert!(text.contains("| [[inbox]] | 0 | 1 |"), "{text}");
        assert!(text.contains("| 后端 | 1 | 1 |"), "{text}");
        assert!(
            text.contains("| 任务 2 | Alpha | 4 |"),
            "逾期天数：10-10 距 10-14 四天"
        );
        assert!(
            text.contains("| 写完 \\| 带竖线的标题 | Alpha | 2026-10-13 | 3 |"),
            "标题里的竖线要转义、周期是创建日到完成日：{text}"
        );
        assert!(text.contains("| [[D3]] | [[D4]] | [[D5]] |"), "{text}");
        assert!(
            text.contains("| [[D3]] | [[D4]] | [[D6]] | [[D7]] |"),
            "{text}"
        );
        assert!(text.contains("[[note]]"), "{text}");
        assert!(text.contains("[[truncated]]"), "{text}");
    }

    #[test]
    fn the_markdown_report_says_so_when_a_table_is_empty() {
        let report = build(&[], today(), &filters(), &[], &[]);
        let export = ReportExport {
            labels: labels(),
            scope_line: String::new(),
            kpi_rows: Vec::new(),
            distribution_note: String::new(),
            completed_note: None,
            inbox_name: "[[inbox]]".to_owned(),
            projects: &[],
        };
        let text = to_markdown(&report, today(), &export);
        // 标签 / 逾期 / 明细三块各一句「（无）」。
        assert_eq!(text.matches("[[E]]\n").count(), 3, "{text}");
        assert!(!text.contains("[[truncated]]"), "{text}");
        assert!(!text.contains("[[note]]"), "{text}");
    }

    #[test]
    fn an_unknown_project_becomes_an_empty_cell_not_a_broken_row() {
        let mut orphan = task(1, TaskStatus::Done);
        orphan.title = "悬空外键".to_owned();
        orphan.project_id = Some(999);
        let orphan = completed_on(orphan, "2026-10-13", 1);

        let tasks = [orphan];
        let report = build(&tasks, today(), &filters(), &[], &[]);
        let export = ReportExport {
            labels: labels(),
            scope_line: String::new(),
            kpi_rows: Vec::new(),
            distribution_note: String::new(),
            completed_note: None,
            inbox_name: "inbox".to_owned(),
            projects: &[],
        };
        let text = to_markdown(&report, today(), &export);
        assert!(text.contains("| 悬空外键 |  | 2026-10-13 | 1 |"), "{text}");
    }
}
