//! 崩溃捕获：把 panic 现场落成文件，供用户求助、供开发者定位。
//!
//! 与 [`crate::logging`] 的分工：那里写**运行日志**（按天滚动、受保留期清理），
//! 这里写**崩溃现场**——同样按天滚动（因此也在日志目录里、日志查看器中看得到），
//! 另外维护一个**不受清理影响**的累计计数。
//!
//! 只依赖 `std`：本 crate 不依赖 gpui，也不依赖持久层。所以「有没有用户还没看过的
//! 崩溃」由应用层拿 [`crash_count`] 与自己的水位线比较得出；这一层只负责如实记录。

use std::any::Any;
use std::fs::OpenOptions;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::logging::{today_stamp, utc_timestamp};

/// 累计崩溃次数。刻意**不用** `.log` 后缀：日志保留期清理只认 `.log`，
/// 而这个计数要跨清理长期保留，否则「以前崩过几次」会在几天后被抹平。
const COUNT_FILE: &str = "crash-count";

/// 钩子只装一次。
static INSTALLED: OnceLock<()> = OnceLock::new();

/// 安装 panic 钩子：现场追加到 `crash-YYYY-MM-DD.log`，并把累计计数加一。
///
/// 重复调用是空操作——多窗口、重复初始化都不该挂上第二条钩子。
/// 钩子内部**先转交原先的钩子**再落盘：默认钩子负责把 panic 打到 stderr，
/// 「捕获崩溃」这件事不该把崩溃信息本身吞掉。
pub fn install_panic_hook(log_dir: &Path) {
    if INSTALLED.set(()).is_err() {
        return;
    }
    let dir = log_dir.to_path_buf();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record(&dir, &render(info));
        previous(info);
    }));
}

/// 当天崩溃日志的路径（与运行日志同目录，按天滚动）。
#[must_use]
pub fn crash_log_path(log_dir: &Path) -> PathBuf {
    log_dir.join(format!("crash-{}.log", today_stamp()))
}

/// 累计崩溃次数；从未崩过、或目录 / 文件不可读时均为 0。
#[must_use]
pub fn crash_count(log_dir: &Path) -> u64 {
    std::fs::read_to_string(log_dir.join(COUNT_FILE))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// 把一份现场文本落盘：追加进当天崩溃日志，并推进累计计数。
///
/// 任何一步失败都静默跳过——崩溃路径上的写失败没有第二个人能接手，
/// 而在这里再 panic 一次只会把「一次崩溃」变成「递归崩溃」。
fn record(log_dir: &Path, text: &str) {
    if std::fs::create_dir_all(log_dir).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(crash_log_path(log_dir))
    {
        let _ = file.write_all(text.as_bytes());
        let _ = file.flush();
    }
    let next = crash_count(log_dir).saturating_add(1);
    let _ = std::fs::write(log_dir.join(COUNT_FILE), next.to_string());
}

/// 把一次 panic 渲染成可读的现场文本。
fn render(info: &PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    let location = info
        .location()
        .map_or_else(|| "<unknown>".to_owned(), ToString::to_string);
    format!(
        "\n===== panic @ {} =====\nthread: {name}\nlocation: {location}\npayload: {}\nbacktrace:\n{}\n",
        utc_timestamp(),
        payload_text(info.payload()),
        std::backtrace::Backtrace::force_capture(),
    )
}

/// panic 的 payload 通常是 `&str`（`panic!("…")`）或 `String`（`panic!("{}", ..)`）。
fn payload_text(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "<non-string payload>".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{crash_count, crash_log_path, install_panic_hook, record};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 临时目录：带进程号 + 自增序号，避免并行跑的测试撞同一目录。
    fn scratch(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("duck-crash-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn nothing_recorded_means_zero_crashes() {
        let dir = scratch("zero");
        assert_eq!(crash_count(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_appends_reports_and_bumps_the_count() {
        let dir = scratch("record");
        record(&dir, "first\n");
        record(&dir, "second\n");
        assert_eq!(crash_count(&dir), 2);
        let text = std::fs::read_to_string(crash_log_path(&dir)).expect("读崩溃日志");
        assert!(text.contains("first"));
        assert!(text.contains("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 计数文件不是 `.log`，所以日志保留期清理不会碰它。
    #[test]
    fn the_count_file_is_not_a_log_file() {
        let dir = scratch("notlog");
        record(&dir, "x\n");
        let name = std::fs::read_dir(&dir)
            .expect("读目录")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with("crash-count"))
            .expect("存在计数文件");
        assert!(!name.ends_with(".log"), "计数文件不能落在清理范围内");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_is_named_after_today() {
        let dir = scratch("name");
        let path = crash_log_path(&dir);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, format!("crash-{}.log", super::today_stamp()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 装钩子会改进程级状态，这里只断言幂等（重复调用不炸）。
    #[test]
    fn installing_twice_is_a_no_op() {
        let dir = scratch("install");
        install_panic_hook(&dir);
        install_panic_hook(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unparsable_count_file_reads_as_zero() {
        let dir = scratch("garbage");
        std::fs::write(dir.join("crash-count"), b"not a number").expect("写坏文件");
        assert_eq!(crash_count(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
