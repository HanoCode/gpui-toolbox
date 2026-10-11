//! 端到端：真的 panic 一次，钩子把现场落进指定目录并推进计数。
//!
//! 放在集成测试里而不是单测里，是因为集成测试跑在**独立进程**中：
//! panic 钩子是进程级全局状态，只有独占进程才测得准（并行的单测会互相抢它）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// 临时目录：进程号 + 自增序号，避免同名测试互撞。
fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("duck-crash-e2e-{tag}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

#[test]
fn a_real_panic_lands_in_the_crash_log() {
    let dir = scratch("panic");

    // 先把默认钩子换成一个静默的：安装器会把「当前的钩子」当作上一个钩子接过去转交，
    // 这样捕获过程就干净了 —— 测试输出里不该出现一段看着像失败的 panic 回显。
    std::panic::set_hook(Box::new(|_| {}));
    duck_platform::install_panic_hook(&dir);

    let _ = std::panic::catch_unwind(|| panic!("boom for the crash log"));

    assert_eq!(duck_platform::crash_count(&dir), 1, "应记下一次崩溃");
    let text = std::fs::read_to_string(duck_platform::crash_log_path(&dir)).expect("读崩溃日志");
    assert!(
        text.contains("boom for the crash log"),
        "现场应含 panic 消息"
    );
    assert!(text.contains("backtrace"), "现场应含回溯段");

    let _ = std::fs::remove_dir_all(&dir);
}
