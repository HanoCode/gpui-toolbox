//! 本地时区偏移：把「Unix 秒」换算成用户眼中的日期。
//!
//! `duck-project` 的 `CalendarDate` 只认 **UTC 秒**——日期是日历事实，故意不含
//! 时区（理由见 `docs/projects-design.md` §9 差异表第 7 条），因此内核在单测里
//! 不需要任何环境依赖。但要把「此刻」翻成用户眼中的**今天**，就必须知道本机比
//! UTC 快 / 慢多少；那是纯粹的宿主能力，落回这一层，由上层把偏移注进内核。
//!
//! 为什么用 `localtime_r` 而不是读 `/etc/localtime` 或 `TZ`：前者直接给出**该
//! 时刻实际生效**的偏移，夏令时切换两侧的两次调用会得到不同的值，调用方不必自己
//! 判断 DST、也不需要区号表。`libc` 本来就是这个 crate 的直接依赖（`lock` 的
//! `flock` 在用），不新增下载。

/// `at`（Unix 秒，UTC）这一刻，本机时区相对 UTC 的偏移，**单位秒**。
///
/// 东八区（UTC+8）= `28_800`；西五区（UTC-5）= `-18_000`。
///
/// 返回的是「**那一刻实际生效**」的偏移，而不是某个固定区号：跨夏令时的地区，
/// 一年里不同时刻调用会得到不同的值。
///
/// 取不到时返回 `0`（按 UTC 处理）：
///
/// - 非 Unix 平台（Windows 需要 `GetTimeZoneInformation`，本 crate 尚未实现）；
/// - `localtime_r` 失败（理论上只在 `time_t` 越界时才发生）。
#[must_use]
pub fn local_utc_offset_seconds(at: i64) -> i32 {
    #[cfg(unix)]
    {
        // SAFETY：`tm` 在这里全零初始化，`localtime_r` 成功时会把它整体写入；
        // `time_t` 是本机 ABI 的整数类型，`at` 由调用方提供。用可重入的
        // `localtime_r`（而非 `localtime`）是因为后者返回指向进程级静态缓冲区的
        // 指针，多线程下会互相覆盖。
        unsafe {
            let time: libc::time_t = at as libc::time_t;
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&time, &mut tm).is_null() {
                return 0;
            }
            tm.tm_gmtoff as i32
        }
    }
    #[cfg(not(unix))]
    {
        let _ = at;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 现实中存在的最大 / 最小偏移是 UTC+14（基里巴斯莱恩群岛）与 UTC-12，
    /// 所以任何时刻的偏移都不该超出这个范围。
    const MAX_ABS_SECONDS: i32 = 14 * 3600;

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    }

    #[test]
    fn offset_for_now_is_plausible() {
        let offset = local_utc_offset_seconds(now());
        assert!(
            offset.abs() <= MAX_ABS_SECONDS,
            "偏移 {offset} 超出 ±14 小时"
        );
        // 现代时区偏移一律是整分钟；这条同时挡掉「把 tm_gmtoff 当别的字段读」。
        assert_eq!(offset % 60, 0, "偏移 {offset} 不是整分钟");
    }

    #[test]
    fn offset_is_plausible_on_both_sides_of_a_dst_switch() {
        // 2024-01-15T12:00:00Z 与 2024-07-15T12:00:00Z：对实行夏令时的地区，
        // 这两次调用的结果**可以**不同（正是这条路径要处理的），但都必须可信。
        for at in [1_705_320_000_i64, 1_721_044_800_i64] {
            let offset = local_utc_offset_seconds(at);
            assert!(
                offset.abs() <= MAX_ABS_SECONDS,
                "{at} 处的偏移 {offset} 超出 ±14 小时"
            );
        }
    }

    #[test]
    fn offset_is_a_pure_function_of_its_input() {
        // 同一个输入连续两次必须一致：这里不能有「第一次调用初始化、之后变样」的
        // 全局状态（`localtime_r` 会读进程级 TZ 环境，但不该改写它）。
        let epoch_first = local_utc_offset_seconds(0);
        let epoch_second = local_utc_offset_seconds(0);
        assert_eq!(epoch_first, epoch_second);

        let now = now();
        let now_first = local_utc_offset_seconds(now);
        let now_second = local_utc_offset_seconds(now);
        assert_eq!(now_first, now_second);
    }
}
