//! 剪贴板并发访问的回归守卫。
//!
//! 这条测试**没有断言体**是刻意的：出错的形状不是「断言失败」而是**进程 abort**。
//! 去掉 `duck-platform/src/clipboard.rs` 里那把进程级访问锁，这个测试会以
//! `Rust cannot catch foreign exceptions, aborting` 死掉——macOS 的 `NSPasteboard`
//! 不允许并发访问，抛出的 Objective-C 异常跨不过 Rust FFI 边界。
//!
//! 之所以值得一条测试守着：本应用**天然会并发**——插件工具在后台线程跑、可以
//! 通过 `duck.readClipboard()` / `duck.writeClipboard()` 碰剪贴板，而同一时刻
//! 用户可能正在主线程上走智能粘贴。两者撞上是一次没有提示的闪退。

use std::thread;

/// 多个线程同时读。
#[test]
fn concurrent_reads_do_not_abort_the_process() {
    let handles: Vec<_> = (0..8)
        .map(|_| thread::spawn(duck_platform::read_text))
        .collect();
    for handle in handles {
        // `join()` 返回 `Err` 只说明那条线程 panic 了；进程 abort 则根本到不了这里。
        assert!(handle.join().is_ok(), "读取线程不该 panic");
    }
}

/// 读写交错：插件在后台写、界面在主线程读，是同一把锁要挡住的真实形状。
#[test]
fn interleaved_reads_and_writes_stay_serialized() {
    let handles: Vec<_> = (0..6)
        .map(|index| {
            thread::spawn(move || {
                for round in 0..5 {
                    let _ = duck_platform::write_text(&format!("duck-test-{index}-{round}"));
                    let _ = duck_platform::read_text();
                }
            })
        })
        .collect();
    for handle in handles {
        assert!(handle.join().is_ok(), "读写线程不该 panic");
    }
}
