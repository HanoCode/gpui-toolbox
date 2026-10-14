//! 时间与日历：Unix 秒（时刻）与 [`CalendarDate`]（日历日）两种表示，以及两者之间的换算。
//!
//! 历法换算照本仓既有做法**手写**（Howard Hinnant 的 `civil_from_days` / `days_from_civil`，
//! 公有领域算法，`std::chrono` 的实现同源），**不引 `chrono` / `time`**：两套算法合计不到
//! 30 行、纯整数无查表，且有单测钉死。同一份实现在 `duck-mail` / `duck-redis` / `duck-db`
//! 里各自存在过（那几处是刻意的重复，理由见各文件注释）；本模块是第五份。
//!
//! ⚠️ **本模块不含任何本地时区逻辑**。`from_unix_seconds` 给出的是 **UTC** 日历日；
//! 需要本地日历时，调用方先把时区偏移加到秒数上再调用（偏移由 `duck-platform` 提供）。
//! 内核保持环境无关，才能在任意机器上跑出同样的单测结果。

use std::time::{SystemTime, UNIX_EPOCH};

/// 一天 86400 秒（不使用闰秒口径，与全仓一致）。
const SECS_PER_DAY: i64 = 86_400;

/// 当前 Unix 秒。
#[must_use]
pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_secs() as i64)
}

/// 星期几（ISO-8601 口径：周一为一周之始）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weekday {
    /// 周一。
    Monday,
    /// 周二。
    Tuesday,
    /// 周三。
    Wednesday,
    /// 周四。
    Thursday,
    /// 周五。
    Friday,
    /// 周六。
    Saturday,
    /// 周日。
    Sunday,
}

impl Weekday {
    /// 全部取值，按周一到周日。
    pub const ALL: [Self; 7] = [
        Self::Monday,
        Self::Tuesday,
        Self::Wednesday,
        Self::Thursday,
        Self::Friday,
        Self::Saturday,
        Self::Sunday,
    ];

    /// 距周一的天数（`Monday` = 0 … `Sunday` = 6）。
    #[must_use]
    pub const fn index_from_monday(self) -> i64 {
        match self {
            Self::Monday => 0,
            Self::Tuesday => 1,
            Self::Wednesday => 2,
            Self::Thursday => 3,
            Self::Friday => 4,
            Self::Saturday => 5,
            Self::Sunday => 6,
        }
    }

    /// 是否周末。
    #[must_use]
    pub const fn is_weekend(self) -> bool {
        matches!(self, Self::Saturday | Self::Sunday)
    }

    /// 从 `days_from_civil` 的天数取星期几。
    ///
    /// 1970-01-01 是**周四**，所以天数 0 落在下标 3（周一为 0）。
    /// `rem_euclid` 保证负数天数（1970 之前）也落在 0..7。
    #[must_use]
    fn from_days_since_epoch(days: i64) -> Self {
        const INDEX: [Weekday; 7] = [
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
            Weekday::Saturday,
            Weekday::Sunday,
        ];
        INDEX[(days + 3).rem_euclid(7) as usize]
    }
}

/// 一个**无时区的日历日**（年 / 月 / 日），落库为 `YYYY-MM-DD` 文本。
///
/// 为什么不用「本地零点的 Unix 秒」：那个值依赖写入时的时区，用户换时区或跨时区同步后
/// 「10-18 截止」会漂成 10-17 或 10-19。日历日就该是日历日。
///
/// 字段私有，走构造器 + 读取器（`AGENTS.md` §4）。`Ord` 是派生的年 → 月 → 日字典序，
/// 与 ISO 文本序一致，所以 Rust 侧排序与 SQLite 侧 `ORDER BY` 结果相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CalendarDate {
    year: i32,
    month: u32,
    day: u32,
}

impl CalendarDate {
    /// 构造；越界（月 / 日不合法）返回 `None`。
    #[must_use]
    pub fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return None;
        }
        Some(Self { year, month, day })
    }

    /// 年。
    #[must_use]
    pub const fn year(self) -> i32 {
        self.year
    }

    /// 月（1–12）。
    #[must_use]
    pub const fn month(self) -> u32 {
        self.month
    }

    /// 日（1–31，按当月实际天数）。
    #[must_use]
    pub const fn day(self) -> u32 {
        self.day
    }

    /// 解析 `YYYY-MM-DD`（严格：位数固定、不认前导空白、不认 `YYYY-M-D`）。
    ///
    /// 严格是**有意**的：宽松解析会把 `2026-1-8` 与 `2026-01-08` 存成两种文本，
    /// 而筛选与排序依赖「同一日期只有一种文本」这个不变量。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        let year: i32 = text.get(0..4)?.parse().ok()?;
        let month: u32 = text.get(5..7)?.parse().ok()?;
        let day: u32 = text.get(8..10)?.parse().ok()?;
        Self::new(year, month, day)
    }

    /// 与 [`Self::parse`] 互逆。
    #[must_use]
    pub fn to_iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// 自 1970-01-01 起的天数（可负）。
    #[must_use]
    pub fn days_since_epoch(self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    /// [`Self::days_since_epoch`] 的反函数。
    #[must_use]
    pub fn from_days_since_epoch(days: i64) -> Self {
        let (year, month, day) = civil_from_days(days);
        Self { year, month, day }
    }

    /// UTC 时刻 → UTC 日历日。
    ///
    /// 要本地日历日，先把时区偏移（秒）加到 `secs` 上：`from_unix_seconds(secs + offset)`。
    /// 也可以直接用 [`Self::from_local_unix_seconds`]（同一件事，名字更直白）。
    #[must_use]
    pub fn from_unix_seconds(secs: i64) -> Self {
        Self::from_days_since_epoch(secs.div_euclid(SECS_PER_DAY))
    }

    /// UTC 时刻 + 本地时区偏移（秒）→ **本地**日历日。
    ///
    /// 偏移由上层提供（`duck_platform::local_utc_offset_seconds`）——内核故意不读
    /// 时区，好让全部历法测试在任意机器上给出一致结果。偏移跨夏令时时会是另一个
    /// 值，因此**每次都要用该时刻的偏移**，不要把它当常量缓存。
    #[must_use]
    pub fn from_local_unix_seconds(secs: i64, offset_seconds: i32) -> Self {
        Self::from_unix_seconds(secs + i64::from(offset_seconds))
    }

    /// 该日 UTC 零点对应的 Unix 秒（`from_unix_seconds` 的左逆）。
    #[must_use]
    pub fn start_of_day_unix(self) -> i64 {
        self.days_since_epoch() * SECS_PER_DAY
    }

    /// 加减天数（跨月 / 跨年由历法换算处理）。
    #[must_use]
    pub fn add_days(self, days: i64) -> Self {
        Self::from_days_since_epoch(self.days_since_epoch() + days)
    }

    /// 星期几。
    #[must_use]
    pub fn weekday(self) -> Weekday {
        Weekday::from_days_since_epoch(self.days_since_epoch())
    }

    /// 所在周的周一（「本周」筛选的左闭边界）。
    #[must_use]
    pub fn start_of_week(self) -> Self {
        self.add_days(-self.weekday().index_from_monday())
    }

    /// 所在周的周日（「本周」筛选的右闭边界）。
    #[must_use]
    pub fn end_of_week(self) -> Self {
        self.start_of_week().add_days(6)
    }

    /// 与另一日期相差的天数（`self - other`）。
    #[must_use]
    pub fn days_until(self, other: Self) -> i64 {
        other.days_since_epoch() - self.days_since_epoch()
    }

    /// 是否早于另一日期（严格）。
    #[must_use]
    pub fn is_before(self, other: Self) -> bool {
        self < other
    }
}

impl std::fmt::Display for CalendarDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// 当月天数（含闰年判定）。
#[must_use]
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// 闰年：能被 4 整除但不能被 100 整除，或能被 400 整除。
#[must_use]
pub const fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Howard Hinnant 的 `days_from_civil`（公有领域）：公历日期 → 自 1970-01-01 的天数。
///
/// 把三月当作一年之始（`y -= m <= 2`），于是闰日落在年末，不必逐月查表；
/// 纪元整体平移到 `0000-03-01`，使修正项只与年有关。对 1970 之前的日期同样有效。
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// [`days_from_civil`] 的反函数（Howard Hinnant，公有领域）。
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
    let year = y + i64::from(m <= 2);
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::{CalendarDate, Weekday, days_from_civil, is_leap_year, now_seconds};

    fn date(year: i32, month: u32, day: u32) -> CalendarDate {
        CalendarDate::new(year, month, day).expect("合法日期")
    }

    #[test]
    fn days_from_civil_matches_known_values() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1900, 1, 1), -25_567);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(days_from_civil(2026, 10, 14), 20_740);
    }

    #[test]
    fn leap_years() {
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2023));
        assert!(!is_leap_year(1900));
        assert!(is_leap_year(2000));
        assert_eq!(super::days_in_month(2024, 2), 29);
        assert_eq!(super::days_in_month(1900, 2), 28);
    }

    #[test]
    fn construct_rejects_out_of_range() {
        assert!(CalendarDate::new(2026, 0, 1).is_none());
        assert!(CalendarDate::new(2026, 13, 1).is_none());
        assert!(CalendarDate::new(2026, 1, 0).is_none());
        assert!(CalendarDate::new(2026, 2, 30).is_none());
        assert!(CalendarDate::new(2024, 2, 29).is_some());
        assert!(CalendarDate::new(2023, 2, 29).is_none());
    }

    #[test]
    fn parse_is_strict_and_round_trips() {
        assert_eq!(
            CalendarDate::parse("2026-10-14"),
            Some(CalendarDate::new(2026, 10, 14).unwrap())
        );
        // 宽松写法一律拒绝：同一日期必须只有一种文本，筛选与排序都依赖它。
        assert!(CalendarDate::parse("2026-1-8").is_none());
        assert!(CalendarDate::parse("2026/10/14").is_none());
        assert!(CalendarDate::parse("2026-10-14 ").is_none());
        assert!(CalendarDate::parse("2026-13-01").is_none());
        for days in [-30_000_i64, -1, 0, 1, 20_740, 40_000] {
            let date = CalendarDate::from_days_since_epoch(days);
            assert_eq!(CalendarDate::parse(&date.to_iso()), Some(date));
            assert_eq!(date.days_since_epoch(), days);
        }
    }

    #[test]
    fn week_boundaries_are_monday_to_sunday() {
        // 2026-10-14 是周三。
        let wed = date(2026, 10, 14);
        assert_eq!(wed.weekday(), Weekday::Wednesday);
        assert_eq!(wed.start_of_week(), date(2026, 10, 12));
        assert_eq!(wed.end_of_week(), date(2026, 10, 18));
        // 周日属于**当周**（不是下一周），这是「本周」筛选最常写错的地方。
        let sun = date(2026, 10, 18);
        assert_eq!(sun.weekday(), Weekday::Sunday);
        assert_eq!(sun.start_of_week(), date(2026, 10, 12));
        // 周一自己就是周首。
        let mon = date(2026, 10, 12);
        assert_eq!(mon.start_of_week(), mon);
        // 跨月/跨年：2026-01-01 是周四，所在周从 2025-12-29（周一）起。
        assert_eq!(date(2026, 1, 1).start_of_week(), date(2025, 12, 29));
    }

    #[test]
    fn unix_seconds_round_trip_on_day_boundaries() {
        let day = date(2026, 10, 14);
        let start = day.start_of_day_unix();
        assert_eq!(CalendarDate::from_unix_seconds(start), day);
        assert_eq!(CalendarDate::from_unix_seconds(start + 86_399), day);
        assert_eq!(
            CalendarDate::from_unix_seconds(start + 86_400),
            day.add_days(1)
        );
        // 1970 之前的负秒也必须落在正确的日子上（`div_euclid`，不是截断除法）。
        assert_eq!(
            CalendarDate::from_unix_seconds(-1),
            CalendarDate::new(1969, 12, 31).unwrap()
        );
    }

    #[test]
    fn local_offset_shifts_the_calendar_day() {
        // 2026-10-14T23:30:00Z：UTC 是 10-14，东八区已经是 10-15。
        let utc = date(2026, 10, 14).start_of_day_unix() + 23 * 3600 + 30 * 60;
        assert_eq!(CalendarDate::from_unix_seconds(utc), date(2026, 10, 14));
        assert_eq!(
            CalendarDate::from_local_unix_seconds(utc, 8 * 3600),
            date(2026, 10, 15)
        );

        // 反向：2026-10-14T02:00:00Z，西五区还停在 10-13。
        let utc = date(2026, 10, 14).start_of_day_unix() + 2 * 3600;
        assert_eq!(
            CalendarDate::from_local_unix_seconds(utc, -5 * 3600),
            date(2026, 10, 13)
        );

        // 偏移为 0 时必须与纯 UTC 解释逐字一致。
        assert_eq!(
            CalendarDate::from_local_unix_seconds(utc, 0),
            CalendarDate::from_unix_seconds(utc)
        );
    }

    #[test]
    fn days_until_and_ordering() {
        let a = date(2026, 10, 14);
        let b = date(2026, 10, 18);
        assert_eq!(a.days_until(b), 4);
        assert_eq!(b.days_until(a), -4);
        assert!(a.is_before(b));
        assert!(a < b, "派生 Ord 应与 ISO 文本序一致");
    }

    #[test]
    fn now_seconds_is_plausible() {
        // 2020-01-01 之后、2100 年之前 —— 只要能挡住「全 0」和单位写错就够了。
        let now = now_seconds();
        assert!(now > 1_577_836_800, "now_seconds() 太早：{now}");
        assert!(now < 4_102_444_800, "now_seconds() 太晚：{now}");
    }
}
