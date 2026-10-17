//! 排期负载与冲突提示（设计文档 F29）—— 纯函数，不碰 gpui、不碰存储。
//!
//! # 为什么这一档不做 AI
//!
//! F29 的原文是「依据预估工时与截止日期**提示排期冲突**」—— 这是一道**算术题**：
//! 把未完成任务的剩余工时按截止日累加到每一天，再跟「一天干几小时」比大小。
//! 同一份数据问两次必须得到同一个答案，所以它该是一个能写单测的纯函数，而不是一次
//! 网络往返（模型还会给出「大概还行吧」这种没法落地的结论）。模型该干的是**替人写字**
//! （日报 / 周报 / 周计划，见 [`super::ai_assist`]），不是替人算加法。
//!
//! # 三条口径
//!
//! 1. **剩余工时** = 预估工时 ×（100 − 进度）/ 100。写了一半的任务只欠一半的时间 ——
//!    用满额去算会把每个进行中的任务都算成一座山。
//! 2. **欠账**（[`ScheduleAdvice::overdue_hours`]）算的是**这一周开始之前**就已经逾期
//!    的那些任务：它们的截止日已经过去，落到哪一天都不诚实，所以单列一笔，
//!    让界面直接说「另有 N 项已逾期，欠 X 小时」。
//! 3. **容量不分工作日与周末**：一周七天用同一个 [`analyze`] 的 `capacity`。
//!    替用户假设「周末不干活」是工具替人做了决定，而排期本来就是人的决定。

use duck_project::{CalendarDate, Task};

/// 一次排期分析的结果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScheduleAdvice {
    /// 一周七天，**周一起、周日止**（下标 0 = 周一）。
    pub(crate) days: Vec<DayLoad>,
    /// 每天可用的工时（与 [`DayLoad::hours`] 同一单位）。
    pub(crate) capacity: f64,
    /// 本周落下来的总工时（七天之和）。
    pub(crate) scheduled_hours: f64,
    /// 超出容量的天数。
    pub(crate) overloaded_days: usize,
    /// 这一周开始前就已逾期的剩余工时（欠账）。
    pub(crate) overdue_hours: f64,
    /// 欠账涉及几条任务。
    pub(crate) overdue_tasks: usize,
    /// 有剩余工时、却没有截止日的未完成任务数（排不了期的那一批）。
    pub(crate) unplanned_tasks: usize,
    /// 有截止日、却没填预估工时的未完成任务数（算不进负载、但会占掉一天的时间）。
    pub(crate) unsized_tasks: usize,
}

/// 某一天压了多少小时。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DayLoad {
    /// 这一天。
    pub(crate) date: CalendarDate,
    /// 落在这一天的剩余工时。
    pub(crate) hours: f64,
    /// 落在这一天的任务 id（界面用来跳转 / 悬停说明）。
    pub(crate) tasks: Vec<i64>,
}

impl DayLoad {
    /// 这一天超载了吗。
    #[must_use]
    pub(crate) fn is_overloaded(&self, capacity: f64) -> bool {
        self.hours > capacity
    }
}

impl ScheduleAdvice {
    /// 七天里最重的那一天的小时数（画柱子时当分母用）。
    ///
    /// 与 `capacity` 取**较大者**：否则「全周都没超」时柱高会被放大，看上去像天天爆满。
    #[must_use]
    pub(crate) fn peak(&self) -> f64 {
        let heaviest = self
            .days
            .iter()
            .map(|day| day.hours)
            .fold(0.0_f64, f64::max);
        heaviest.max(self.capacity)
    }

    /// 这一周有没有任何值得说的（超载 / 欠账 / 排不了期）。
    ///
    /// 界面据此决定要不要画那块面板：一周排得整整齐齐时多一块「一切正常」的卡片，
    /// 只会占掉本来给任务列表的地方。
    #[must_use]
    pub(crate) fn has_findings(&self) -> bool {
        self.overloaded_days > 0
            || self.overdue_tasks > 0
            || self.unplanned_tasks > 0
            || self.sized_days() > 0
    }

    /// 本周真有任务落下来的天数（与 [`Self::has_findings`] 一起决定画不画）。
    #[must_use]
    fn sized_days(&self) -> usize {
        self.days.iter().filter(|day| !day.tasks.is_empty()).count()
    }
}

/// 一条未完成任务的剩余工时。
///
/// 已结束（完成 / 取消）的任务返回 `None` —— 它们不再占排期。没填预估工时的也返回
/// `None`：**不替用户猜**（猜一个 4 小时出来，排期面板立刻就在编数）。
#[must_use]
pub(crate) fn remaining_hours(task: &Task) -> Option<f64> {
    let estimate = task.estimate_hours?;
    if task.status.is_settled() || estimate <= 0.0 {
        return None;
    }
    let left = f64::from(100_u32.saturating_sub(u32::from(task.progress))) / 100.0;
    Some(estimate * left)
}

/// 分析锚定周的排期负载。
///
/// `anchor` 落在哪一周就以哪一周为准（周一起算，与日历视图的周视图同一个起点）。
pub(crate) fn analyze(tasks: &[Task], anchor: CalendarDate, capacity: f64) -> ScheduleAdvice {
    let monday = anchor.start_of_week();
    let mut days: Vec<DayLoad> = (0..7)
        .map(|offset| DayLoad {
            date: monday.add_days(offset),
            hours: 0.0,
            tasks: Vec::new(),
        })
        .collect();
    let mut overdue_hours = 0.0;
    let mut overdue_tasks = 0usize;
    let mut unplanned_tasks = 0usize;
    let mut unsized_tasks = 0usize;

    for task in tasks {
        if task.status.is_settled() {
            continue;
        }
        let due = task.due_date;
        let Some(hours) = remaining_hours(task) else {
            if due.is_some() {
                unsized_tasks += 1;
            }
            continue;
        };
        let Some(due) = due else {
            unplanned_tasks += 1;
            continue;
        };
        if due < monday {
            overdue_hours += hours;
            overdue_tasks += 1;
            continue;
        }
        let offset = monday.days_until(due);
        if let Some(day) = usize::try_from(offset)
            .ok()
            .and_then(|index| days.get_mut(index))
        {
            day.hours += hours;
            day.tasks.push(task.id);
        }
        // `offset >= 7`：落在本周之后的那些周三，不属于这一屏，静默跳过（不是丢弃
        // 数据 —— 翻到那一周就会看到它）。
    }

    let mut scheduled_hours = 0.0;
    for day in &mut days {
        // 浮点累加会留下 2.9999999 这种尾巴，界面上一比 `capacity` 就可能误报超载。
        // 统一**四舍五入到一位小数**再比 —— 排期看的是「半天」，一位小数足够。
        day.hours = (day.hours * 10.0).round() / 10.0;
        scheduled_hours += day.hours;
        day.tasks.sort_unstable();
    }
    scheduled_hours = (scheduled_hours * 10.0).round() / 10.0;
    overdue_hours = (overdue_hours * 10.0).round() / 10.0;
    let overloaded_days = days
        .iter()
        .filter(|day| day.is_overloaded(capacity))
        .count();

    ScheduleAdvice {
        days,
        capacity,
        scheduled_hours,
        overloaded_days,
        overdue_hours,
        overdue_tasks,
        unplanned_tasks,
        unsized_tasks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_project::{Priority, Quadrant, SubtaskProgress, TaskStatus};

    fn monday() -> CalendarDate {
        // 2026-10-14 是周三 ⇒ 它那一周的周一是 10-12。
        CalendarDate::new(2026, 10, 14)
            .expect("legal date")
            .start_of_week()
    }

    fn day(text: &str) -> CalendarDate {
        CalendarDate::parse(text).expect("legal ISO date")
    }

    fn task(id: i64) -> Task {
        Task {
            id,
            project_id: None,
            parent_id: None,
            title: format!("Task {id}"),
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

    /// 一条「截止某天、预估若干小时、进度若干」的任务。
    fn scheduled(id: i64, due: &str, estimate: f64, progress: u8) -> Task {
        let mut item = task(id);
        item.due_date = Some(day(due));
        item.estimate_hours = Some(estimate);
        item.progress = progress;
        item
    }

    #[test]
    fn the_week_starts_on_monday_and_has_seven_days() {
        let advice = analyze(&[], day("2026-10-14"), 8.0);
        assert_eq!(advice.days.len(), 7);
        assert_eq!(advice.days[0].date.to_iso(), "2026-10-12");
        assert_eq!(advice.days[6].date.to_iso(), "2026-10-18");
        assert!(!advice.has_findings(), "空库里没有什么可说的");
        assert_eq!(advice.peak(), 8.0, "没活时柱高按容量算");
    }

    #[test]
    fn remaining_hours_shrinks_with_progress_and_skips_unestimatable() {
        assert_eq!(
            remaining_hours(&scheduled(1, "2026-10-13", 8.0, 0)),
            Some(8.0)
        );
        assert_eq!(
            remaining_hours(&scheduled(2, "2026-10-13", 8.0, 50)),
            Some(4.0),
            "写了一半就只欠一半"
        );
        assert_eq!(remaining_hours(&task(3)), None, "没填预估工时不算数");

        let mut done = scheduled(4, "2026-10-13", 8.0, 0);
        done.status = TaskStatus::Done;
        assert_eq!(remaining_hours(&done), None, "结束了的任务不再占排期");
    }

    #[test]
    fn hours_land_on_their_due_day_and_overload_is_counted() {
        let tasks = [
            scheduled(1, "2026-10-12", 6.0, 0),
            scheduled(2, "2026-10-12", 4.0, 0),
            scheduled(3, "2026-10-16", 8.0, 0),
        ];
        let advice = analyze(&tasks, day("2026-10-14"), 8.0);
        assert_eq!(advice.days[0].hours, 10.0, "周一 6 + 4");
        assert_eq!(advice.days[0].tasks, vec![1, 2]);
        assert_eq!(advice.days[4].hours, 8.0, "周五 8 小时正好打平");
        assert!(!advice.days[4].is_overloaded(8.0), "等于容量不算超");
        assert_eq!(advice.overloaded_days, 1);
        assert_eq!(advice.scheduled_hours, 18.0);
        assert_eq!(advice.peak(), 10.0);
        assert!(advice.has_findings());
    }

    #[test]
    fn tasks_before_the_week_become_backlog_and_later_weeks_are_ignored() {
        let tasks = [
            scheduled(1, "2026-10-05", 5.0, 0),
            scheduled(2, "2026-10-25", 9.0, 0),
        ];
        let advice = analyze(&tasks, day("2026-10-14"), 8.0);
        assert_eq!(advice.overdue_hours, 5.0);
        assert_eq!(advice.overdue_tasks, 1);
        assert_eq!(advice.scheduled_hours, 0.0, "下周的任务不属于这一屏");
        assert!(advice.has_findings(), "有欠账就得说");
    }

    #[test]
    fn tasks_without_a_date_or_without_an_estimate_are_reported_separately() {
        let mut no_date = scheduled(1, "2026-10-14", 3.0, 0);
        no_date.due_date = None;
        let mut no_estimate = task(2);
        no_estimate.due_date = Some(day("2026-10-14"));
        let tasks = [no_date, no_estimate];
        let advice = analyze(&tasks, day("2026-10-14"), 8.0);
        assert_eq!(advice.unplanned_tasks, 1, "有工时没截止日");
        assert_eq!(advice.unsized_tasks, 1, "有截止日没工时");
        assert_eq!(advice.scheduled_hours, 0.0);
    }

    #[test]
    fn fractional_accumulation_is_rounded_before_the_overload_test() {
        // 0.1 加七次在 f64 里是 0.7000000000000001 —— 不收敛到一位小数的话，
        // 「0.7 + 0.7 + …」这类组合迟早会撞出一个假的「超载」。
        let tasks: Vec<Task> = (0..7)
            .map(|index| scheduled(index + 1, "2026-10-14", 0.1, 0))
            .collect();
        let advice = analyze(&tasks, day("2026-10-14"), 0.7);
        assert_eq!(advice.days[2].hours, 0.7);
        assert_eq!(advice.overloaded_days, 0, "0.7 打平 0.7，不是超载");
    }
}
