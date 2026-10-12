//! 邮箱页的纯逻辑：时间 / 大小 / 地址的展示格式化。
//!
//! 与 `terminal_data.rs` 同一形态：**能单测的部分不放进渲染文件**。渲染函数要
//! `Window` 与 `Context`，测不动；而「这个时间戳该显示成什么」是纯函数，一个断言
//! 就能钉住。混在 `mail.rs` 里只会让两边都变长、还都测不了。
//!
//! # 时间为什么是 UTC
//!
//! [`format_list_time`] / [`format_full_time`] 按 **UTC** 输出，与本仓其余时间显示
//! （`terminal_data::format_modified`、日志文件名）一致。本仓的依赖准入结论是
//! 「不引 `chrono` / `time`」，所以 M1 不做时区换算——这是**已知的展示差异**，
//! 不是遗漏（要改成系统本地时区，得先决定是引入时区依赖还是走平台 API，
//! 记在 `docs/email-design.md` 的未决项里）。

/// 列表行的时间：`MM-DD HH:MM`（UTC）。时间戳为 0（解析不出 `Date` 头）时给空串。
pub fn format_list_time(unix_seconds: i64) -> String {
    if unix_seconds <= 0 {
        return String::new();
    }
    let days = unix_seconds.div_euclid(86_400);
    let within_day = unix_seconds.rem_euclid(86_400);
    let (_, month, day) = civil_from_days(days);
    format!(
        "{month:02}-{day:02} {:02}:{:02}",
        within_day / 3_600,
        (within_day % 3_600) / 60
    )
}

/// 阅读器头部的时间：`YYYY-MM-DD HH:MM`（UTC）。
pub fn format_full_time(unix_seconds: i64) -> String {
    if unix_seconds <= 0 {
        return String::new();
    }
    let days = unix_seconds.div_euclid(86_400);
    let within_day = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        within_day / 3_600,
        (within_day % 3_600) / 60
    )
}

/// 人类可读的字节数（附件大小用）。
///
/// 只有 B 不带小数：`1.0 KB` 读起来比 `1024 B` 快，但 `0.0 B` 是纯噪音。
pub fn format_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// 发件人显示：显示名 → 地址 → `unknown`（由调用方给本地化占位文案）。
pub fn sender_display<'a>(
    name: Option<&'a str>,
    addr: Option<&'a str>,
    unknown: &'a str,
) -> &'a str {
    name.filter(|s| !s.trim().is_empty())
        .or_else(|| addr.filter(|s| !s.trim().is_empty()))
        .unwrap_or(unknown)
}

/// 地址列表拼成一行（收件人 / 抄送用）。
pub fn join_addresses(list: &[String]) -> String {
    list.join("、")
}

/// 主题为空时给占位——空标题的行在列表里看起来像没加载出来。
pub fn subject_or_placeholder<'a>(subject: &'a str, placeholder: &'a str) -> &'a str {
    if subject.trim().is_empty() {
        placeholder
    } else {
        subject
    }
}

/// 把「自 1970-01-01 起的天数」还原成公历年月日（Howard Hinnant 的 `civil_from_days`）。
///
/// 手写而不用库：见文件头的依赖结论。本仓 `logging.rs` / `mssql.rs` /
/// `terminal_data.rs` 各有一份同源实现——每处都是**纯整数、可单测**的十几行，
/// 收成一处公共依赖反而要跨 crate 暴露一个只为格式化服务的工具函数。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as i64;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as i64;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_089), (2025, 1, 1));
        // 闰日：2024-02-29 是 1970-01-01 之后的第 19782 天。
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn list_time_is_utc_mm_dd_hh_mm() {
        // 2026-10-12 02:00:00 UTC（= 北京时间 10:00）。按 UTC 显示是本页的既定口径。
        assert_eq!(format_list_time(1_791_770_400), "10-12 02:00");
        assert_eq!(format_full_time(1_791_770_400), "2026-10-12 02:00");
    }

    #[test]
    fn zero_and_negative_timestamps_render_empty() {
        // 解析不出 `Date` 头时内核写 0——列表上留空，而不是印出 1970 年。
        assert_eq!(format_list_time(0), "");
        assert_eq!(format_full_time(0), "");
        assert_eq!(format_list_time(-5), "");
        assert_eq!(format_full_time(-5), "");
    }

    #[test]
    fn times_round_trip_across_day_boundaries() {
        // 跨日进位：2026-10-11 23:59 UTC 再加 60 秒就是 10-12 00:00。
        let base = 1_791_770_400 - 60 - 3_600 * 2; // 10-11 23:59
        assert_eq!(format_list_time(base + 60), "10-12 00:00");
    }

    #[test]
    fn size_uses_binary_units_and_drops_decimals_only_for_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 * 1024 * 3 / 2), "1.5 GB");
    }

    #[test]
    fn sender_display_prefers_name_then_addr_then_placeholder() {
        assert_eq!(sender_display(Some("张三"), Some("z@x.com"), "?"), "张三");
        assert_eq!(sender_display(Some("  "), Some("z@x.com"), "?"), "z@x.com");
        assert_eq!(sender_display(None, Some("z@x.com"), "?"), "z@x.com");
        assert_eq!(sender_display(None, None, "?"), "?");
        assert_eq!(sender_display(Some(""), Some(""), "?"), "?");
    }

    #[test]
    fn subject_placeholder_only_for_blank_subjects() {
        assert_eq!(subject_or_placeholder("", "(无主题)"), "(无主题)");
        assert_eq!(subject_or_placeholder("   ", "(无主题)"), "(无主题)");
        assert_eq!(subject_or_placeholder("hi", "(无主题)"), "hi");
    }

    #[test]
    fn addresses_join_with_the_cjk_separator() {
        assert_eq!(join_addresses(&[]), "");
        assert_eq!(join_addresses(&["a@x.com".into()]), "a@x.com");
        assert_eq!(
            join_addresses(&["a@x.com".into(), "b@x.com".into()]),
            "a@x.com、b@x.com"
        );
    }
}
