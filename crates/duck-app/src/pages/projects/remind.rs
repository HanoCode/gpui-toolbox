//! 到期 / 逾期 / 久未更新的提醒（设计文档 §3.1 F15）。
//!
//! 四档：**已逾期 / 今天到期 / 明天到期 / 久未更新**。全部走
//! [`notify::system_with_toast`] —— 应用内横幅与系统通知中心两条通道**各自**看自己的
//! 开关，历史只记一条（这是 `AGENTS.md` §3.8 给「需要系统通知中心」场景定下的语义，
//! 邮件页的新信提醒同款）。
//!
//! # 为什么挂在 app 上，而不是项目页里
//!
//! 提醒的价值正在于**你没打开项目页时也告诉你**。所以它由 [`install`] 挂在
//! `app::install` 上，自带一个 `duck-project` 连接（与「随手看」悬浮窗同口径：连接
//! 便宜、互不牵制，页面关掉不影响它），每 [`SCAN_INTERVAL`] 扫一次。
//!
//! # 去重：同一件事一天只说一次
//!
//! 去重键是 `{任务 id}:{档位}:{今天}`（[`Reminder::dedupe_key`]），落 kv
//! `projects_reminders`；键里带日期，所以**读到别的日期就整批作废**。于是
//! 「明天到期」今天响一次、明天变成「今天到期」再响一次 —— 这是要的行为，不是漏去重。
//!
//! # 一轮最多单独说三件
//!
//! 开机时若有二十条逾期，二十条通知会把界面糊满，而它们**明天仍会被重新提起**。
//! 所以单轮最多投 [`MAX_PER_SCAN`] 条，剩下的折成一条汇总。记账时**整批都记**，
//! 免得溢出那部分在下一轮（5 分钟后）又被当成新的、把界面变成滴滴答答的水龙头。
//!
//! # 没窗口时不记账
//!
//! 一个窗口都没有（只剩托盘）时 [`crate::update::on_notify_window`] 投不出去。此时
//! **不写去重册**，等下一次扫描（那时大概已经有窗口了）再投 —— 否则「刚才那几条」会
//! 被静默吞掉，而用户永远不知道丢的是什么。

use std::time::Duration;

use gpui::{App, Global};

use duck_components::notify;
use duck_project::{
    CalendarDate, Priority, ProjectStore, Task, TaskQuery, TaskStatus, now_seconds,
};
use duck_theme::{tr, trf, trn};

/// 两轮扫描之间的间隔。
///
/// 5 分钟：到期是**天级**事实（日期列只到天，没有时刻档位可比），扫得再密也不会
/// 多发现一件事，只多费几次本地 SELECT。
pub(crate) const SCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// 启动后先等一会儿再扫第一轮：开机那几秒留给窗口与页面自己加载。
const FIRST_SCAN_DELAY: Duration = Duration::from_secs(20);

/// 单轮最多单独说几件事（其余折成一条汇总，见模块头）。
const MAX_PER_SCAN: usize = 3;

/// 提醒去重册的存储键。
const KEY_REMINDERS: &str = "projects_reminders";

/// 逾期超过这么多天的不再提醒。
///
/// 「一开工就被半年前的窟窿刷屏」没有任何行动价值 —— 那些该由用户自己在看板/列表里
/// 处理（逾期档筛选本来就能把它们捞出来），而不是每天拦一次。
const OVERDUE_MAX_DAYS: i64 = 30;

/// 「久未更新」的**出厂判定天数**（设置页可改，见 `module_settings::ProjectsSettings`）。
///
/// 放在这里而不是设置页：判定逻辑在 `is_stale`，天数跟着它走才是同一处真源。
pub(crate) const STALE_DAYS: i64 = 3;

/// 一天有多少秒（算「久未更新」的天数用）。
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// 提醒的档位。声明顺序**就是**提醒顺序（`Ord` 派生），见 [`scan`] 的排序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReminderKind {
    /// 已逾期。
    Overdue,
    /// 今天到期。
    DueToday,
    /// 明天到期。
    DueTomorrow,
    /// 久未更新（高优先级且好几天没动过）。
    Stale,
}

impl ReminderKind {
    /// 去重键里的档位名。**进存档**，改名等于让当天已经说过的那些重新说一遍。
    const fn key(self) -> &'static str {
        match self {
            Self::Overdue => "overdue",
            Self::DueToday => "today",
            Self::DueTomorrow => "tomorrow",
            Self::Stale => "stale",
        }
    }
}

/// 一条待投递的提醒。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reminder {
    /// 任务 id（去重键的一半）。
    pub(crate) task_id: i64,
    /// 档位。
    pub(crate) kind: ReminderKind,
    /// 任务标题（投递时用它拼消息）。
    pub(crate) title: String,
    /// 逾期 / 久未更新的天数；到期两档是 0。
    pub(crate) days: i64,
}

impl Reminder {
    /// 去重键：`{任务 id}:{档位}:{今天}`。
    ///
    /// 带日期是**故意**的：同一件事跨天必须能再说一次（「明天到期」到第二天就是
    /// 「今天到期」），而同一天内不重复。
    fn dedupe_key(&self, today: CalendarDate) -> String {
        format!("{}:{}:{}", self.task_id, self.kind.key(), today.to_iso())
    }
}

/// 扫一遍库，给出「此刻值得说一句」的条目（**不去重**，去重交给 [`ReminderLog`]）。
///
/// `today` 由调用方注入 —— 内核不知道用户在哪（见 `duck_project::time` 的模块注释），
/// 「今天 / 明天 / 逾期几天」全靠上层把本地日期传进来；`now` 用来算「久未更新」。
/// 两者都是入参，本函数才能被单测钉死。
pub(crate) fn scan(
    store: &ProjectStore,
    today: CalendarDate,
    now: i64,
    stale_days: i64,
) -> duck_project::Result<Vec<Reminder>> {
    // 默认查询就是「未归档」（`ArchiveScope::Live`）——归档即隐藏，提醒一视同仁。
    let tasks = store.list_tasks(&TaskQuery::new().today(today))?;
    let mut found: Vec<Reminder> = tasks
        .iter()
        .filter_map(|task| reminder_for(task, today, now, stale_days))
        .collect();
    // 档位升序（逾期 → 今天 → 明天 → 久未更新），同档内**越久越靠前**，最后用 id 兜底
    // （同一档里两条一样老时顺序必须稳定，否则每次扫描的投递顺序会飘）。
    found.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(b.days.cmp(&a.days))
            .then(a.task_id.cmp(&b.task_id))
    });
    Ok(found)
}

/// 单条任务的判定：到期三档优先，都没命中才看「久未更新」。
///
/// 一件事在一轮里只说一次 —— 既有截止日又很久没动的任务，说「今天到期」比说
/// 「5 天没更新」有用。
fn reminder_for(task: &Task, today: CalendarDate, now: i64, stale_days: i64) -> Option<Reminder> {
    // 已完成 / 已取消不提醒（`cancelled` 是终态，`done` 没什么可催的）。
    if task.status.is_settled() {
        return None;
    }
    if let Some(due) = task.due_date {
        let diff = today.days_until(due);
        let kind = match diff {
            // 逾期：`diff < 0`；太久远的按 [`OVERDUE_MAX_DAYS`] 放弃。
            days if days < 0 && -days <= OVERDUE_MAX_DAYS => Some(ReminderKind::Overdue),
            0 => Some(ReminderKind::DueToday),
            1 => Some(ReminderKind::DueTomorrow),
            _ => None,
        };
        if let Some(kind) = kind {
            return Some(Reminder {
                task_id: task.id,
                kind,
                title: task.title.clone(),
                days: -diff,
            });
        }
    }
    if is_stale(task, now, stale_days) {
        return Some(Reminder {
            task_id: task.id,
            kind: ReminderKind::Stale,
            title: task.title.clone(),
            days: (now - task.updated_at) / SECONDS_PER_DAY,
        });
    }
    None
}

/// 是不是「久未更新」：**高优先级 + 还没开始或正在做 + 超过 `stale_days` 天没动过**。
///
/// 三条都是有意的收窄：
/// - 只认 `P0` / `P1`：P2 / P3 放三天本来就正常，天天催只会让人把提醒整个关掉；
/// - 只认 `Todo` / `Doing`：`Blocked` 与 `Deferred` 本来就在等外部条件，催了也动不了；
/// - 用 `updated_at` 而不是 `created_at`：任务被改过就说明它在被推进（内核每次写都
///   刷新 `updated_at`），「没更新」才是「放着没动」。
fn is_stale(task: &Task, now: i64, stale_days: i64) -> bool {
    if !matches!(task.status, TaskStatus::Todo | TaskStatus::Doing) {
        return false;
    }
    if task.priority > Priority::P1 {
        return false;
    }
    if stale_days <= 0 {
        return false;
    }
    now - task.updated_at >= stale_days * SECONDS_PER_DAY
}

/// 提醒去重册（kv 里的一份 JSON）。
#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct ReminderLog {
    /// 记的是哪一天（ISO 日期）；与当前日期不同即整批作废。
    day: Option<String>,
    /// 当天已经说过的去重键。
    seen: Vec<String>,
}

impl ReminderLog {
    /// 读册子；没写过或解析失败时是空册（不报错、不挡提醒）。
    fn load(cx: &App) -> Self {
        let Some(json) = duck_store::kv_get(cx, KEY_REMINDERS) else {
            return Self::default();
        };
        serde_json::from_str(&json).unwrap_or_else(|error| {
            tracing::warn!(%error, "failed to parse the reminder log, starting a new one");
            Self::default()
        })
    }

    /// 写册子并**立刻落盘**：去重册的价值全在「下次还认得出」，
    /// 等 400ms 去抖窗口时进程正好退出就会把同一件事再说一遍。
    fn save(&self, cx: &App) {
        let Ok(json) = serde_json::to_string(self) else {
            tracing::warn!("failed to serialize the reminder log");
            return;
        };
        duck_store::kv_put(cx, KEY_REMINDERS, json);
        duck_store::flush(cx);
    }

    /// 挑出当天还**没说过的**条目，并把这批全部记下（含调用方不单独投递的那些，
    /// 见模块头「一轮最多单独说三件」）。
    fn take_new(&mut self, today: CalendarDate, found: &[Reminder]) -> Vec<Reminder> {
        let day = today.to_iso();
        if self.day.as_deref() != Some(day.as_str()) {
            self.day = Some(day);
            self.seen.clear();
        }
        let mut fresh = Vec::new();
        for reminder in found {
            let key = reminder.dedupe_key(today);
            if self.seen.contains(&key) {
                continue;
            }
            self.seen.push(key);
            fresh.push(reminder.clone());
        }
        fresh
    }
}

/// 「周期扫描已挂上」的哨兵。`install` 可能被调多次（设置变更触发重建 / 单测），
/// 靠它保证只挂一条链。
struct ReminderLoop;

impl Global for ReminderLoop {}

/// 挂上周期扫描（幂等）。由 `app::install` 调用。
pub(crate) fn install(cx: &mut App) {
    if cx.has_global::<ReminderLoop>() {
        return;
    }
    cx.set_global(ReminderLoop);
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_SCAN_DELAY).await;
        // 连接只开一次，但**每轮都重试**：库文件第一次打不开（目录还没建好、
        // 权限被改）时不该永久放弃 —— 下一次扫描再试一次就是了。
        let mut store: Option<ProjectStore> = None;
        loop {
            if store.is_none() {
                match ProjectStore::open_default() {
                    Ok(opened) => store = Some(opened),
                    Err(error) => {
                        tracing::warn!(%error, "failed to open the projects database for reminders");
                    }
                }
            }
            if let Some(store) = store.as_ref() {
                cx.update(|cx| run_once(store, cx));
            }
            cx.background_executor().timer(SCAN_INTERVAL).await;
        }
    })
    .detach();
}

/// 扫一轮并按需投递。返回投出去的通知条数（0 = 没什么可说 / 通道没开 / 没窗口）。
fn run_once(store: &ProjectStore, cx: &mut App) -> usize {
    let settings = crate::pages::module_settings::projects_settings(cx);
    if !settings.reminders {
        return 0;
    }
    let now = now_seconds();
    let today =
        CalendarDate::from_local_unix_seconds(now, duck_platform::local_utc_offset_seconds(now));
    let found = match scan(store, today, now, settings.stale_days) {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, "failed to scan task reminders");
            return 0;
        }
    };
    if found.is_empty() {
        return 0;
    }
    let mut log = ReminderLog::load(cx);
    let fresh = log.take_new(today, &found);
    if fresh.is_empty() {
        return 0;
    }

    let mut lines: Vec<(String, String)> = fresh
        .iter()
        .take(MAX_PER_SCAN)
        .map(|reminder| text_for(reminder, cx))
        .collect();
    let rest = fresh.len() - lines.len();
    if rest > 0 {
        lines.push((
            tr(cx, "任务提醒").to_owned(),
            trn(cx, "还有 {} 条任务需要注意", rest as i64),
        ));
    }

    let count = lines.len();
    let delivered = crate::update::on_notify_window(cx, move |window, cx| {
        for (title, message) in lines {
            notify::system_with_toast(title.clone(), message.clone(), window, cx);
        }
    });
    // 没窗口可投就**不记账**：这几条要留给下一轮，不能静默吞掉（见模块头）。
    if delivered {
        log.save(cx);
    }
    count
}

/// 一条提醒的（标题, 正文）。文案在这里组织，判定层不碰 i18n。
fn text_for(reminder: &Reminder, cx: &App) -> (String, String) {
    let title = tr(cx, "任务提醒").to_owned();
    let days = reminder.days.to_string();
    let message = match reminder.kind {
        ReminderKind::Overdue => trf(cx, "已逾期 {} 天：{}", &[&days, &reminder.title]),
        ReminderKind::DueToday => trf(cx, "今天到期：{}", &[&reminder.title]),
        ReminderKind::DueTomorrow => trf(cx, "明天到期：{}", &[&reminder.title]),
        ReminderKind::Stale => trf(cx, "已 {} 天没有更新：{}", &[&days, &reminder.title]),
    };
    (title, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_project::{NewTask, ProjectStore};

    fn date(year: i32, month: u32, day: u32) -> CalendarDate {
        CalendarDate::new(year, month, day).expect("合法日期")
    }

    /// 固定「今天」：所有断言都不依赖跑测试那天是什么日子。
    fn today() -> CalendarDate {
        date(2026, 10, 16)
    }

    fn store() -> ProjectStore {
        ProjectStore::open_in_memory().expect("内存库")
    }

    fn add(store: &ProjectStore, title: &str, due: Option<CalendarDate>) -> i64 {
        store
            .create_task(NewTask::new(title).dates(None, due))
            .expect("建任务")
    }

    #[test]
    fn the_three_due_tiers_are_told_apart() {
        let store = store();
        add(&store, "昨天到期", Some(date(2026, 10, 15)));
        add(&store, "今天到期", Some(today()));
        add(&store, "明天到期", Some(date(2026, 10, 17)));
        add(&store, "下周到期", Some(date(2026, 10, 23)));
        add(&store, "没有日期", None);

        let found = scan(&store, today(), 0, 3).expect("扫描");
        let kinds: Vec<ReminderKind> = found.iter().map(|item| item.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ReminderKind::Overdue,
                ReminderKind::DueToday,
                ReminderKind::DueTomorrow
            ],
            "只有这三条该被提醒：{found:?}"
        );
    }

    #[test]
    fn a_very_old_overdue_task_stops_being_reminded() {
        let store = store();
        add(&store, "刚逾期", Some(today().add_days(-1)));
        add(
            &store,
            "远古逾期",
            Some(today().add_days(-OVERDUE_MAX_DAYS - 1)),
        );

        let found = scan(&store, today(), 0, 3).expect("扫描");
        assert_eq!(found.len(), 1, "只有刚逾期的那条：{found:?}");
        assert_eq!(found[0].days, 1);
    }

    #[test]
    fn settled_tasks_are_never_reminded() {
        let store = store();
        let done = add(&store, "已完成", Some(today()));
        let cancelled = add(&store, "已取消", Some(today().add_days(-2)));
        store
            .set_task_status(done, TaskStatus::Done)
            .expect("标完成");
        store
            .set_task_status(cancelled, TaskStatus::Cancelled)
            .expect("标取消");

        assert!(scan(&store, today(), 0, 3).expect("扫描").is_empty());
    }

    #[test]
    fn archived_tasks_are_never_reminded() {
        let store = store();
        let archived = add(&store, "已归档", Some(today().add_days(-3)));
        store.archive_task(archived).expect("归档");

        assert!(scan(&store, today(), 0, 3).expect("扫描").is_empty());
    }

    #[test]
    fn the_oldest_overdue_comes_first() {
        let store = store();
        add(&store, "逾期两天", Some(today().add_days(-2)));
        add(&store, "逾期五天", Some(today().add_days(-5)));
        add(&store, "逾期一天", Some(today().add_days(-1)));

        let found = scan(&store, today(), 0, 3).expect("扫描");
        let days: Vec<i64> = found.iter().map(|item| item.days).collect();
        assert_eq!(days, vec![5, 2, 1], "越久的越靠前：{found:?}");
    }

    /// 从库里取一条任务再改字段：`Task` 有二十多个字段且没有 `Default`，
    /// 逐个拼字面量既长又会在加字段时全线报错。
    fn task_with(store: &ProjectStore, title: &str, updated_at: i64) -> Task {
        let id = add(store, title, None);
        let mut task = store.get_task(id).expect("读任务").expect("任务在");
        task.updated_at = updated_at;
        task
    }

    #[test]
    fn the_stale_tier_is_narrow_by_design() {
        let store = store();
        let now = 1_800_000_000;
        let old = now - 4 * SECONDS_PER_DAY;

        // 高优先级 + 待办 + 四天没动 = 该催。
        let mut task = task_with(&store, "放着没动", old);
        task.priority = Priority::P0;
        assert!(is_stale(&task, now, 3));

        // 优先级不够（P2 放三天本来就正常）。
        task.priority = Priority::P2;
        assert!(!is_stale(&task, now, 3));

        // 状态不对（受阻 / 搁置在等外部条件）。
        task.priority = Priority::P0;
        task.status = TaskStatus::Blocked;
        assert!(!is_stale(&task, now, 3));

        // 刚动过。
        task.status = TaskStatus::Doing;
        task.updated_at = now - SECONDS_PER_DAY;
        assert!(!is_stale(&task, now, 3));
    }

    #[test]
    fn a_due_reminder_wins_over_a_stale_one() {
        let store = store();
        let now = 1_800_000_000;
        let mut task = task_with(&store, "既要到期也很久没动", now - 10 * SECONDS_PER_DAY);
        task.due_date = Some(today());

        let reminder = reminder_for(&task, today(), now, 3).expect("该有一条");
        assert_eq!(reminder.kind, ReminderKind::DueToday);
    }

    #[test]
    fn a_stale_task_is_reported_with_its_age() {
        let store = store();
        let now = 1_800_000_000;
        let mut task = task_with(&store, "五天没动", now - 5 * SECONDS_PER_DAY);
        task.priority = Priority::P0;

        let reminder = reminder_for(&task, today(), now, 3).expect("该有一条");
        assert_eq!(reminder.kind, ReminderKind::Stale);
        assert_eq!(reminder.days, 5);
    }

    #[test]
    fn a_reminder_is_only_delivered_once_a_day() {
        let mut log = ReminderLog::default();
        let found = vec![Reminder {
            task_id: 7,
            kind: ReminderKind::DueToday,
            title: "写周报".to_owned(),
            days: 0,
        }];
        assert_eq!(log.take_new(today(), &found).len(), 1, "第一次要说");
        assert!(log.take_new(today(), &found).is_empty(), "同一天不再说");
    }

    #[test]
    fn the_very_same_task_can_be_reminded_again_on_the_next_day() {
        let mut log = ReminderLog::default();
        let found = vec![Reminder {
            task_id: 7,
            kind: ReminderKind::DueTomorrow,
            title: "写周报".to_owned(),
            days: 0,
        }];
        log.take_new(today(), &found);
        // 第二天：档位变成「今天到期」，而且日期换了，两处都该让它重新可说。
        let tomorrow = today().add_days(1);
        let next = vec![Reminder {
            task_id: 7,
            kind: ReminderKind::DueToday,
            title: "写周报".to_owned(),
            days: 0,
        }];
        assert_eq!(log.take_new(tomorrow, &next).len(), 1);
        assert_eq!(log.day.as_deref(), Some(tomorrow.to_iso().as_str()));
    }

    #[test]
    fn the_log_is_dropped_when_the_day_changes() {
        let mut log = ReminderLog::default();
        log.take_new(today(), &[]);
        log.seen.push("7:today:2026-10-16".to_owned());
        log.take_new(today().add_days(1), &[]);
        assert!(log.seen.is_empty(), "跨天整批作废：{log:?}");
    }
}
