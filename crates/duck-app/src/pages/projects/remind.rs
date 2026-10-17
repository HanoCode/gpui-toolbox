//! 到期 / 逾期 / 久未更新的提醒（设计文档 §3.1 F15）。
//!
//! 五档：**已逾期 / 即将到期（带时刻）/ 今天到期 / 明天到期 / 久未更新**。全部走
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
//! 带时刻的那一档（`DueSoon`）键里再串上 `HH:MM`：同一天把截止时刻从 17:00 改到
//! 18:00，是该重新提醒一次的另一件事。
//!
//! # 带时刻的任务：只在「到期前一小时内」和「过了点还没做」时说
//!
//! 没有时刻的任务按天提醒（今天 / 明天）。有时刻的如果也照样先报一次「今天到期」，
//! 用户一天会收到两条讲同一件事的通知。所以那一档由 [`DUE_SOON_LEAD_SECS`] 接管：
//! 到期前 60 分钟内说一次「18:00 到期」；过了点还没完成，才退回「今天到期」。
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

use gpui::{App, Global, Window};

use duck_components::notify;
use duck_project::{
    CalendarDate, Priority, ProjectStore, Task, TaskQuery, TaskStatus, now_seconds,
};
use duck_theme::{tr, trf, trn};

/// 两轮扫描之间的间隔。
///
/// 5 分钟：天级事实（今天 / 明天 / 逾期）扫得再密也不会多发现一件事；
/// [`DUE_SOON_LEAD_SECS`] 那一档是 60 分钟宽的窗口，5 分钟粒度保证窗口内必有一次
/// 命中（应用睡眠错过窗口时，醒来后的第一轮会按「已过点」退回天级档，不会静默漏掉）。
pub(crate) const SCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// 「即将到期」的提前量：到期前这么多秒内说一次。
///
/// 60 分钟与 `temp/TO-DO-Panel` 的 `TODO_REMINDER_LEAD_MS` 同口径 —— 提前一小时
/// 是「还来得及处理」与「说了也没用」的分界。
const DUE_SOON_LEAD_SECS: i64 = 60 * 60;

/// 启动后先等一会儿再扫第一轮：开机那几秒留给窗口与页面自己加载。
const FIRST_SCAN_DELAY: Duration = Duration::from_secs(20);

/// 单轮最多单独说几件事（其余折成一条汇总，见模块头）。
const MAX_PER_SCAN: usize = 3;

/// 提醒去重册的存储键。
const KEY_REMINDERS: &str = "projects_reminders";

/// 简报去重册的存储键（G-PRJ-13）。
///
/// 与 [`KEY_REMINDERS`] **必须分开**：那一册按天整批作废（逐条提醒的「今天说过了」），
/// 而简报要记的是「上一次发的是哪一天」，周报还得跨周认得 —— 挤在一个册子里，
/// 跨天清空会把周报的记录一起抹掉，于是周一那条每次重启都重发一遍。
const KEY_BRIEF: &str = "projects_brief";

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
    /// 即将到期（带 `due_time` 的任务，进入 [`DUE_SOON_LEAD_SECS`] 窗口）。
    DueSoon,
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
            Self::DueSoon => "soon",
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
    /// 截止时刻 `HH:MM`（只有 [`ReminderKind::DueSoon`] 带）。
    pub(crate) at: Option<String>,
}

impl Reminder {
    /// 去重键：`{任务 id}:{档位}:{今天}`；带时刻的档位再串上 `:{HH:MM}`。
    ///
    /// 带日期是**故意**的：同一件事跨天必须能再说一次（「明天到期」到第二天就是
    /// 「今天到期」），而同一天内不重复。
    fn dedupe_key(&self, today: CalendarDate) -> String {
        let base = format!("{}:{}:{}", self.task_id, self.kind.key(), today.to_iso());
        match &self.at {
            Some(at) => format!("{base}:{at}"),
            None => base,
        }
    }
}

/// 扫一遍库，给出「此刻值得说一句」的条目（**不去重**，去重交给 [`ReminderLog`]）。
///
/// `today` 由调用方注入 —— 内核不知道用户在哪（见 `duck_project::time` 的模块注释），
/// 「今天 / 明天 / 逾期几天」全靠上层把本地日期传进来；`now` 用来算「久未更新」与
/// 「离到期还有多久」；`offset_seconds` 是把 `(日期, HH:MM)` 换算成 Unix 秒所需的
/// 本地时区偏移（同样由上层给，跨夏令时每轮重取）。三者都是入参，本函数才能被单测钉死。
pub(crate) fn scan(
    store: &ProjectStore,
    today: CalendarDate,
    now: i64,
    offset_seconds: i32,
    stale_days: i64,
) -> duck_project::Result<Vec<Reminder>> {
    // 默认查询就是「未归档」（`ArchiveScope::Live`）——归档即隐藏，提醒一视同仁。
    let tasks = store.list_tasks(&TaskQuery::new().today(today))?;
    let mut found: Vec<Reminder> = tasks
        .iter()
        .filter_map(|task| reminder_for(task, today, now, offset_seconds, stale_days))
        .collect();
    // 档位升序（逾期 → 即将到期 → 今天 → 明天 → 久未更新），同档内**越久越靠前**，
    // 最后用 id 兜底（同一档里两条一样老时顺序必须稳定，否则每次扫描的投递顺序会飘）。
    found.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(b.days.cmp(&a.days))
            .then(a.task_id.cmp(&b.task_id))
    });
    Ok(found)
}

/// 单条任务的判定：到期档位优先，都没命中才看「久未更新」。
///
/// 一件事在一轮里只说一次 —— 既有截止日又很久没动的任务，说「今天到期」比说
/// 「5 天没更新」有用。
fn reminder_for(
    task: &Task,
    today: CalendarDate,
    now: i64,
    offset_seconds: i32,
    stale_days: i64,
) -> Option<Reminder> {
    // 已完成 / 已取消不提醒（`cancelled` 是终态，`done` 没什么可催的）。
    if task.status.is_settled() {
        return None;
    }
    if let Some(due) = task.due_date {
        let diff = today.days_until(due);
        if diff == 0 {
            // 带时刻的那一批由「即将到期」接管，见模块头：不提前报「今天到期」，
            // 否则同一件事一天会响两遍。
            if let Some(minutes) = task.due_time.as_deref().and_then(minute_of_day) {
                let remaining = due_unix(due, minutes, offset_seconds) - now;
                if remaining > 0 {
                    return (remaining <= DUE_SOON_LEAD_SECS).then(|| Reminder {
                        task_id: task.id,
                        kind: ReminderKind::DueSoon,
                        title: task.title.clone(),
                        days: 0,
                        at: task.due_time.clone(),
                    });
                }
                // 过了点还没完成：退回天级档（一天最多一次）。
                return Some(Reminder {
                    task_id: task.id,
                    kind: ReminderKind::DueToday,
                    title: task.title.clone(),
                    days: 0,
                    at: None,
                });
            }
            return Some(Reminder {
                task_id: task.id,
                kind: ReminderKind::DueToday,
                title: task.title.clone(),
                days: 0,
                at: None,
            });
        }
        let kind = match diff {
            // 逾期：`diff < 0`；太久远的按 [`OVERDUE_MAX_DAYS`] 放弃。
            days if days < 0 && -days <= OVERDUE_MAX_DAYS => Some(ReminderKind::Overdue),
            1 => Some(ReminderKind::DueTomorrow),
            _ => None,
        };
        if let Some(kind) = kind {
            return Some(Reminder {
                task_id: task.id,
                kind,
                title: task.title.clone(),
                days: -diff,
                at: None,
            });
        }
    }
    if is_stale(task, now, stale_days) {
        return Some(Reminder {
            task_id: task.id,
            kind: ReminderKind::Stale,
            title: task.title.clone(),
            days: (now - task.updated_at) / SECONDS_PER_DAY,
            at: None,
        });
    }
    None
}

/// `HH:MM` → 当天第几分钟（0..=1439）。
///
/// 复用 [`super::quickparse::parse_time`]：记号语法只有一套，时刻的合法形状（两位
/// 小时、两位分钟、`23:59` 上限）不该在这里再抄一遍。
fn minute_of_day(text: &str) -> Option<i64> {
    let normalized = super::quickparse::parse_time(text)?;
    let (hour, minute) = normalized.split_once(':')?;
    Some(hour.parse::<i64>().ok()? * 60 + minute.parse::<i64>().ok()?)
}

/// 把用户输入的时刻规范成 `HH:MM`（设置页保存前用）；不合法返回 `None`。
///
/// 比记号解析（`quickparse::parse_time`）**宽一档**：那边要认一句话里的 `18:00`，
/// 位数不够就不是记号；这里是人在设置页里手打，`9:05` 不该被拒。但**输出**必须是
/// 规范的两位形状 —— 存档里只有一种形状，[`minute_of_day`] 与 [`brief_due`] 才不会有
/// 「填了看着像对的时刻，简报却永远不发」这种静默失败。
pub(crate) fn normalize_time(text: &str) -> Option<String> {
    let (hour, minute) = text.trim().split_once(':')?;
    let hour: i64 = hour.trim().parse().ok()?;
    let minute: i64 = minute.trim().parse().ok()?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }
    Some(format!("{hour:02}:{minute:02}"))
}

/// `(日期, 当天第几分钟)` → Unix 秒。
///
/// `start_of_day_unix` 给的是**UTC** 零点，所以要减掉本地偏移才是「用户眼中的
/// 那天零点」（与 [`CalendarDate::from_local_unix_seconds`] 互逆）。
fn due_unix(due: CalendarDate, minutes: i64, offset_seconds: i32) -> i64 {
    due.start_of_day_unix() - i64::from(offset_seconds) + minutes * 60
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

/// 简报类型：日报 / 周报。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BriefKind {
    /// 每天一次。
    Daily,
    /// 每周一一次。
    Weekly,
}

/// 简报去重册：记「上一次发的是哪一天」。
///
/// 只记日期、不记内容：判断只需要「今天发过没有」，而内容每次都现算 —— 存正文会让
/// 「库变了但简报还是旧的」这种偏差长期留在盘上。
#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct BriefLog {
    /// 日报上次发出的日期（`YYYY-MM-DD`）。
    daily: Option<String>,
    /// 周报上次发出的日期。
    weekly: Option<String>,
}

impl BriefLog {
    /// 读册；没写过或解析失败时是空册（不报错、不挡简报）。
    fn load(cx: &App) -> Self {
        let Some(json) = duck_store::kv_get(cx, KEY_BRIEF) else {
            return Self::default();
        };
        serde_json::from_str(&json).unwrap_or_else(|error| {
            tracing::warn!(%error, "failed to parse the brief log, starting a new one");
            Self::default()
        })
    }

    /// 写册并立刻落盘（与 [`ReminderLog::save`] 同一个理由：等去抖窗口时退出就会重发）。
    fn save(&self, cx: &App) {
        let Ok(json) = serde_json::to_string(self) else {
            tracing::warn!("failed to serialize the brief log");
            return;
        };
        duck_store::kv_put(cx, KEY_BRIEF, json);
        duck_store::flush(cx);
    }

    /// 这一档上次发在哪天。
    fn last(&self, kind: BriefKind) -> Option<&str> {
        match kind {
            BriefKind::Daily => self.daily.as_deref(),
            BriefKind::Weekly => self.weekly.as_deref(),
        }
    }

    /// 记下「这一档今天发过了」。
    fn mark(&mut self, kind: BriefKind, today: CalendarDate) {
        let day = Some(today.to_iso());
        match kind {
            BriefKind::Daily => self.daily = day,
            BriefKind::Weekly => self.weekly = day,
        }
    }
}

/// 现在是不是该发这一档简报。
///
/// 判据是「**已经过了**设定时刻，且这一档今天还没发过」，不是「正好等于」：
/// 扫描是 5 分钟一跳、应用还可能睡着，要求精确相等等于让「21:00 关着应用」的用户
/// 永远收不到日报。过了点补发一条，比漏一期有用。
///
/// 周报额外钉在**周一**（`index_from_monday() == 0`）：设成「周一 09:00」就是为了
/// 早上规划本周，落在周二读上周的账没有意义。
fn brief_due(
    kind: BriefKind,
    now: i64,
    offset_seconds: i32,
    today: CalendarDate,
    at: &str,
    last: Option<&str>,
) -> bool {
    let day = today.to_iso();
    if last == Some(day.as_str()) {
        return false;
    }
    if kind == BriefKind::Weekly && today.weekday().index_from_monday() != 0 {
        return false;
    }
    let Some(target) = minute_of_day(at) else {
        // 存档里的时刻坏了（手改过 / 老版本写歪）：这期不发，也不 panic。
        return false;
    };
    let local_minutes = (now + i64::from(offset_seconds)).rem_euclid(SECONDS_PER_DAY) / 60;
    local_minutes >= target
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
    let offset = duck_platform::local_utc_offset_seconds(now);
    let today = CalendarDate::from_local_unix_seconds(now, offset);
    // 简报先发（一天一次 / 一周一次），与逐条提醒是两本去重册；
    // 它**不占**「每天最多 N 条」的额度 —— 那份额度是给逐条提醒的。
    let mut count = deliver_briefs(store, cx, &settings, now, offset, today);
    let found = match scan(store, today, now, offset, settings.stale_days) {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, "failed to scan task reminders");
            return count;
        }
    };
    if found.is_empty() {
        return count;
    }
    let mut log = ReminderLog::load(cx);
    let already = log.seen.len();
    let mut fresh = log.take_new(today, &found);
    if fresh.is_empty() {
        return count;
    }
    // 每天上限：到量之后**不再单独说**，但这一批照样记账（`take_new` 已经写进去了）——
    // 不记账的话下一轮它们又是「新的」，会在上限上反复撞同一个墙。
    if settings.daily_limit > 0 {
        let room = usize::try_from(settings.daily_limit.saturating_sub(already as i64))
            .unwrap_or_default();
        fresh.truncate(room);
        if fresh.is_empty() {
            log.save(cx);
            return count;
        }
    }

    let mut lines: Vec<Delivery> = fresh
        .iter()
        .take(MAX_PER_SCAN)
        .map(|reminder| {
            let (title, message) = text_for(reminder, cx);
            Delivery {
                title,
                message,
                task_id: Some(reminder.task_id),
            }
        })
        .collect();
    let rest = fresh.len() - lines.len();
    if rest > 0 {
        lines.push(Delivery {
            title: tr(cx, "任务提醒").to_owned(),
            message: trn(cx, "还有 {} 条任务需要注意", rest as i64),
            // 汇总行不对应某一条任务，自然也没有「完成」可按。
            task_id: None,
        });
    }

    let sent = lines.len();
    let delivered = crate::update::on_notify_window(cx, move |window, cx| {
        for line in lines {
            match line.task_id {
                Some(id) => notify::system_with_toast_action(
                    line.title,
                    line.message,
                    notify::BannerAction::new(tr(cx, "完成"), move |window, cx| {
                        complete_from_reminder(id, window, cx);
                    }),
                    window,
                    cx,
                ),
                None => notify::system_with_toast(line.title, line.message, window, cx),
            }
        }
    });
    // 没窗口可投就**不记账**：这几条要留给下一轮，不能静默吞掉（见模块头）。
    if delivered {
        log.save(cx);
        count += sent;
    }
    count
}

/// 一条要投出去的通知。
///
/// `task_id` 为 `None` 的是「还有 N 条」那条汇总 —— 它不对应某一条任务，
/// 所以没有「完成」可按。
struct Delivery {
    title: String,
    message: String,
    task_id: Option<i64>,
}

/// 发简报（日报 / 周报），返回投出去的条数（0 / 1 / 2）。
///
/// 与逐条提醒共用同一条纪律：**没窗口可投就不记账**，下一轮再发（否则一期简报
/// 会在「只剩托盘」的那次扫描里被静默吞掉）。
fn deliver_briefs(
    store: &ProjectStore,
    cx: &mut App,
    settings: &crate::pages::module_settings::ProjectsSettings,
    now: i64,
    offset_seconds: i32,
    today: CalendarDate,
) -> usize {
    let mut log = BriefLog::load(cx);
    let mut sent = 0;
    for (kind, enabled, at) in [
        (
            BriefKind::Daily,
            settings.daily_brief,
            settings.daily_at.as_str(),
        ),
        (
            BriefKind::Weekly,
            settings.weekly_brief,
            settings.weekly_at.as_str(),
        ),
    ] {
        if !enabled || !brief_due(kind, now, offset_seconds, today, at, log.last(kind)) {
            continue;
        }
        let Some(body) = brief_body(store, kind, today, cx) else {
            continue;
        };
        let title = match kind {
            BriefKind::Daily => tr(cx, "今日简报").to_owned(),
            BriefKind::Weekly => tr(cx, "本周简报").to_owned(),
        };
        let delivered = crate::update::on_notify_window(cx, move |window, cx| {
            notify::system_with_toast(title, body, window, cx);
        });
        if delivered {
            log.mark(kind, today);
            sent += 1;
        }
    }
    if sent > 0 {
        log.save(cx);
    }
    sent
}

/// 简报正文：数字汇总 + 前几条待办。
///
/// 口径全走内核（`board_stats` / `completed_on`）—— 界面这一层不自己数。一旦这里
/// 手算一遍，报表与简报就会各说一套，而两边看起来都「有道理」。
fn brief_body(
    store: &ProjectStore,
    kind: BriefKind,
    today: CalendarDate,
    cx: &App,
) -> Option<String> {
    let stats = match store.board_stats(today) {
        Ok(stats) => stats,
        Err(error) => {
            tracing::warn!(%error, "failed to read the board stats for a brief");
            return None;
        }
    };
    let head = match kind {
        BriefKind::Daily => trf(
            cx,
            "今天到期 {} 条 · 逾期 {} 条 · 进行中 {} 条",
            &[
                &stats.due_today.to_string(),
                &stats.overdue.to_string(),
                &stats.doing.to_string(),
            ],
        ),
        BriefKind::Weekly => trf(
            cx,
            "本周完成 {} 条 · 逾期 {} 条 · 进行中 {} 条",
            &[
                &week_completed(store, today).to_string(),
                &stats.overdue.to_string(),
                &stats.doing.to_string(),
            ],
        ),
    };
    match next_titles(store, today, cx) {
        Some(list) => Some(format!("{head}\n{list}")),
        None => Some(head),
    }
}

/// 本周（周一起）完成的任务数。
///
/// 逐日问内核（7 次 `COUNT`）：一周发一次，不值得为它开一个新的聚合口径；
/// 而「本周」的边界（周一起）已经由 [`CalendarDate::start_of_week`] 钉住。
fn week_completed(store: &ProjectStore, today: CalendarDate) -> u32 {
    let start = today.start_of_week();
    (0..7)
        .filter_map(|offset| store.completed_on(start.add_days(offset)).ok())
        .sum()
}

/// 前三条还没做完的事，接成一行；一条都没有时返回 `None`。
///
/// 用「 · 」而不是顿号：顿号是全角标点，埋在 `format!` 里既不好看也不好翻。
fn next_titles(store: &ProjectStore, today: CalendarDate, cx: &App) -> Option<String> {
    let tasks = store.list_tasks(&TaskQuery::new().today(today)).ok()?;
    let titles: Vec<&str> = tasks
        .iter()
        .filter(|task| !task.status.is_settled())
        .map(|task| task.title.as_str())
        .take(3)
        .collect();
    if titles.is_empty() {
        return None;
    }
    Some(trf(cx, "接下来：{}", &[&titles.join(" · ")]))
}

/// 一条提醒的（标题, 正文）。文案在这里组织，判定层不碰 i18n。
fn text_for(reminder: &Reminder, cx: &App) -> (String, String) {
    let title = tr(cx, "任务提醒").to_owned();
    let days = reminder.days.to_string();
    let message = match reminder.kind {
        ReminderKind::Overdue => trf(cx, "已逾期 {} 天：{}", &[&days, &reminder.title]),
        ReminderKind::DueSoon => {
            let at = reminder.at.as_deref().unwrap_or_default();
            trf(cx, "{} 到期：{}", &[at, &reminder.title])
        }
        ReminderKind::DueToday => trf(cx, "今天到期：{}", &[&reminder.title]),
        ReminderKind::DueTomorrow => trf(cx, "明天到期：{}", &[&reminder.title]),
        ReminderKind::Stale => trf(cx, "已 {} 天没有更新：{}", &[&days, &reminder.title]),
    };
    (title, message)
}

/// 提醒横幅上那个「完成」：把这条任务直接标成已完成。
///
/// 三条纪律：
///
/// 1. **自己开一个连接**，不借用扫描循环里的那个 —— 那个活在异步任务里，而按钮的
///    回调要 `'static`，借用根本传不进去；单次 `open_default` 是一次文件打开，
///    点按钮的频率下可以忽略。
/// 2. **写完广播**：打开着的页面 / 随手看浮窗各自重读一遍。不广播的话，用户点了
///    「完成」，主窗看板上那条还挂在 Todo 列里，只能以为按钮没生效。
/// 3. 失败只报一句错，不重试也不静默 —— 提醒是「顺手的出口」，不是数据入口。
fn complete_from_reminder(id: i64, window: &mut Window, cx: &mut App) {
    let outcome =
        ProjectStore::open_default().and_then(|store| store.set_task_status(id, TaskStatus::Done));
    match outcome {
        Ok(()) => {
            super::bus::notify(cx);
            notify::success(tr(cx, "已完成。"), window, cx);
        }
        Err(error) => notify::error(
            trf(cx, "标记完成失败：{}", &[&error.to_string()]),
            window,
            cx,
        ),
    }
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

        let found = scan(&store, today(), 0, 0, 3).expect("扫描");
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

        let found = scan(&store, today(), 0, 0, 3).expect("扫描");
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

        assert!(scan(&store, today(), 0, 0, 3).expect("扫描").is_empty());
    }

    #[test]
    fn archived_tasks_are_never_reminded() {
        let store = store();
        let archived = add(&store, "已归档", Some(today().add_days(-3)));
        store.archive_task(archived).expect("归档");

        assert!(scan(&store, today(), 0, 0, 3).expect("扫描").is_empty());
    }

    #[test]
    fn the_oldest_overdue_comes_first() {
        let store = store();
        add(&store, "逾期两天", Some(today().add_days(-2)));
        add(&store, "逾期五天", Some(today().add_days(-5)));
        add(&store, "逾期一天", Some(today().add_days(-1)));

        let found = scan(&store, today(), 0, 0, 3).expect("扫描");
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

        let reminder = reminder_for(&task, today(), now, 0, 3).expect("该有一条");
        assert_eq!(reminder.kind, ReminderKind::DueToday);
    }

    #[test]
    fn a_stale_task_is_reported_with_its_age() {
        let store = store();
        let now = 1_800_000_000;
        let mut task = task_with(&store, "五天没动", now - 5 * SECONDS_PER_DAY);
        task.priority = Priority::P0;

        let reminder = reminder_for(&task, today(), now, 0, 3).expect("该有一条");
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
            at: None,
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
            at: None,
        }];
        log.take_new(today(), &found);
        // 第二天：档位变成「今天到期」，而且日期换了，两处都该让它重新可说。
        let tomorrow = today().add_days(1);
        let next = vec![Reminder {
            task_id: 7,
            kind: ReminderKind::DueToday,
            title: "写周报".to_owned(),
            days: 0,
            at: None,
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

    /// 简报：过了点就发（补发），当天不重复。
    #[test]
    fn a_brief_fires_after_its_time_only_once_a_day() {
        let today = today();
        // 21:00 = 当天第 1260 分钟。偏移 0，`now` 直接按「当天第几分钟 × 60」造。
        let at_20_59 = 20 * 3600 + 59 * 60;
        let at_21_00 = 21 * 3600;
        assert!(
            !brief_due(BriefKind::Daily, at_20_59, 0, today, "21:00", None),
            "还没到点不该发"
        );
        assert!(
            brief_due(BriefKind::Daily, at_21_00, 0, today, "21:00", None),
            "到点该发"
        );
        // 22:30 才启动应用：补发一条，而不是等到明天。
        assert!(
            brief_due(
                BriefKind::Daily,
                22 * 3600 + 30 * 60,
                0,
                today,
                "21:00",
                None
            ),
            "过了点要补发"
        );
        // 今天已经发过：再扫多少轮都不发。
        let day = today.to_iso();
        assert!(
            !brief_due(
                BriefKind::Daily,
                23 * 3600,
                0,
                today,
                "21:00",
                Some(day.as_str())
            ),
            "一天只发一次"
        );
        // 存档里的时刻坏了：不发，也不 panic。
        assert!(!brief_due(
            BriefKind::Daily,
            23 * 3600,
            0,
            today,
            "nope",
            None
        ));
    }

    /// 周报固定周一：其余六天即使过了点也不发。
    #[test]
    fn the_weekly_brief_only_fires_on_monday() {
        // 2026-10-19 是周一。
        let monday = CalendarDate::new(2026, 10, 19).expect("合法日期");
        assert_eq!(monday.weekday().index_from_monday(), 0);
        assert!(
            brief_due(BriefKind::Weekly, 9 * 3600, 0, monday, "09:00", None),
            "周一到点该发"
        );
        let tuesday = monday.add_days(1);
        assert!(
            !brief_due(BriefKind::Weekly, 23 * 3600, 0, tuesday, "09:00", None),
            "周二不补发周报"
        );
        // 日报不受星期影响。
        assert!(brief_due(
            BriefKind::Daily,
            23 * 3600,
            0,
            tuesday,
            "21:00",
            None
        ));
    }

    /// 时区偏移参与判定：`now` 是 UTC 秒，必须按本地时间比。
    #[test]
    fn the_brief_time_is_compared_in_local_time() {
        let today = today();
        // UTC 13:00 + 8 小时 = 本地 21:00。
        let utc_13 = 13 * 3600;
        assert!(
            brief_due(BriefKind::Daily, utc_13, 8 * 3600, today, "21:00", None),
            "本地 21:00 该发"
        );
        assert!(
            !brief_due(BriefKind::Daily, utc_13, 0, today, "21:00", None),
            "UTC 13:00 还没到本地的 21:00"
        );
    }

    /// 设置页存的时刻与判定认的时刻是同一个形状。
    #[test]
    fn normalize_time_round_trips_through_the_parser() {
        assert_eq!(normalize_time("9:05").as_deref(), Some("09:05"));
        assert_eq!(normalize_time("21:00").as_deref(), Some("21:00"));
        assert_eq!(normalize_time("nope"), None);
        // 规范出来的串必须还能被判定的那一跳认出来。
        let normalized = normalize_time("7:30").expect("可规范化");
        assert!(minute_of_day(&normalized).is_some(), "{normalized}");
    }

    /// 一条「今天 18:00 到期」的任务，以及它那一刻对应的 Unix 秒（偏移 0）。
    fn timed_task(store: &ProjectStore, title: &str, at: &str) -> (Task, i64) {
        let id = store
            .create_task(NewTask::new(title).dates(None, Some(today())).due_time(at))
            .expect("建任务");
        let task = store.get_task(id).expect("读任务").expect("任务在");
        let due = due_unix(today(), minute_of_day(at).expect("合法时刻"), 0);
        (task, due)
    }

    #[test]
    fn a_timed_task_is_announced_within_the_lead_hour_only() {
        let store = store();
        let (task, due) = timed_task(&store, "开评审会", "18:00");

        // 窗口外（差 61 分钟）：不说话，免得一天报两遍。
        assert!(
            reminder_for(&task, today(), due - DUE_SOON_LEAD_SECS - 60, 0, 3).is_none(),
            "还没进窗口"
        );
        // 刚进窗口（差 60 分钟）：说一次，并带上时刻。
        let reminder =
            reminder_for(&task, today(), due - DUE_SOON_LEAD_SECS, 0, 3).expect("该说一句");
        assert_eq!(reminder.kind, ReminderKind::DueSoon);
        assert_eq!(reminder.at.as_deref(), Some("18:00"));
        // 窗口内最后一分钟仍然算「即将到期」。
        let reminder = reminder_for(&task, today(), due - 60, 0, 3).expect("该说一句");
        assert_eq!(reminder.kind, ReminderKind::DueSoon);
        // 过了点：退回天级档，而不是继续喊「即将到期」。
        let reminder = reminder_for(&task, today(), due + 1, 0, 3).expect("该说一句");
        assert_eq!(reminder.kind, ReminderKind::DueToday);
        assert_eq!(reminder.at, None);
    }

    #[test]
    fn a_timed_task_is_not_reported_as_due_today_in_the_morning() {
        let store = store();
        let (task, due) = timed_task(&store, "晚上的会", "18:00");
        // 早上 09:00：带时刻的任务由「即将到期」接管，不该先报一次「今天到期」。
        let morning = due - 9 * 3600;
        assert!(reminder_for(&task, today(), morning, 0, 3).is_none());
    }

    #[test]
    fn a_changed_due_time_is_a_new_thing_to_say() {
        let mut log = ReminderLog::default();
        let mut reminder = Reminder {
            task_id: 7,
            kind: ReminderKind::DueSoon,
            title: "开评审会".to_owned(),
            days: 0,
            at: Some("17:00".to_owned()),
        };
        assert_eq!(log.take_new(today(), &[reminder.clone()]).len(), 1);
        assert!(log.take_new(today(), &[reminder.clone()]).is_empty());
        // 同一天改到 18:00：这是另一件事，该重新提醒。
        reminder.at = Some("18:00".to_owned());
        assert_eq!(log.take_new(today(), &[reminder]).len(), 1);
    }

    #[test]
    fn an_unparsable_due_time_falls_back_to_the_day_tier() {
        let store = store();
        let id = store
            .create_task(
                NewTask::new("时刻写歪了")
                    .dates(None, Some(today()))
                    .due_time("傍晚"),
            )
            .expect("建任务");
        let task = store.get_task(id).expect("读任务").expect("任务在");
        let reminder =
            reminder_for(&task, today(), today().start_of_day_unix(), 0, 3).expect("该说一句");
        assert_eq!(reminder.kind, ReminderKind::DueToday);
    }
}
