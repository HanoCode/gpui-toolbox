//! 单实例锁：同一时刻只有一个进程能持有同名锁。
//!
//! 实现是 `flock(LOCK_EX | LOCK_NB)` 锁住 `<应用数据目录>/<name>.lock`：
//!
//! - **不会留下僵尸锁**：锁属于「打开的文件描述符」，进程结束（正常退出、崩溃、
//!   `kill -9`）时内核一律自动释放。所以不需要「读 PID → 探测进程是否还活着 →
//!   删锁」那套自带竞态的清理逻辑。
//! - **进程内也守得住**：同一进程两次独立打开同一文件会得到两个文件描述符，
//!   第二次 `flock` 同样失败。（POSIX `fcntl` 锁按进程去重，就守不住这一点。）
//! - 不用 `std::fs::File::try_lock()`（Rust 1.89 起稳定）是**为守住 MSRV 1.85**：
//!   它在 Unix 上做的就是同一件事，本工程只能用手写 FFI 或 `libc` 替代。
//!
//! 非 Unix 平台没有 `flock`，此时退化为「无锁」（取锁恒成功）。

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::paths;

/// 取锁结果。
///
/// 「被占」拆成独立变体而不是塞进 `io::Error`：它对调用方是**预期分支**
/// （另一个实例在跑），不是需要上报的故障。
#[derive(Debug)]
pub enum LockOutcome {
    /// 拿到锁；值存活期间锁一直有效，丢弃即释放。
    Acquired(InstanceLock),
    /// 另一个实例正持有同一把锁；`path` 是锁文件位置，供日志 / 提示使用。
    Busy {
        /// 锁文件位置。
        path: PathBuf,
    },
}

/// 单实例锁的持有凭证。
///
/// 刻意不实现 `Clone`：克隆会让「谁负责释放」变得含糊。需要共享时用 `Rc`。
#[derive(Debug)]
pub struct InstanceLock {
    /// 锁挂在这个句柄上 —— 它一关闭（含 `drop`）锁就没了，所以必须一直持有。
    /// 该字段不用读、只为持有生命周期。
    file: File,
    /// 锁文件位置。
    path: PathBuf,
}

impl InstanceLock {
    /// 锁文件位置。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // `File` 关闭时内核本来就会释放锁，这里显式解一次，让「释放点」在代码里可见。
        unlock(&self.file);
    }
}

/// 取名为 `name` 的单实例锁。
///
/// 锁文件落在 [`paths::app_data_dir`] 下（`<name>.lock`），目录不存在会自动创建；
/// 拿到锁后把当前进程号写进文件，方便事后排查「是不是还有别的实例在跑」。
///
/// # Errors
/// 只有锁文件**打不开 / 建不出来**时才返回 `Err`（例如目录只读、磁盘满）。
/// 「已被别的实例持有」走的是 `Ok(LockOutcome::Busy { .. })`。
pub fn acquire(name: &str) -> io::Result<LockOutcome> {
    acquire_at(paths::lock_path(name))
}

/// [`acquire`] 的实际实现：把「路径怎么算」与「怎么加锁」拆开，便于用临时目录做测试。
fn acquire_at(path: PathBuf) -> io::Result<LockOutcome> {
    if let Some(parent) = path.parent() {
        paths::ensure_dir(parent)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;

    if !try_lock_exclusive(&file) {
        return Ok(LockOutcome::Busy { path });
    }

    // 写入进程号只是给人看的：判断「有没有实例在跑」靠锁本身，不靠这个数字。
    file.set_len(0)?;
    file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
    file.flush()?;

    Ok(LockOutcome::Acquired(InstanceLock { file, path }))
}

/// 尝试对 `file` 加**排他 + 非阻塞**锁；已在别处持有时返回 `false`。
#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> bool {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `flock` 只读取 `fd` 与整数标志，不触碰内存；`fd` 来自调用期间存活的
    // `File`，必然有效。失败只通过返回值表达。
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// 释放锁；未持有时也是无害的成功。
#[cfg(unix)]
fn unlock(file: &File) {
    use std::os::fd::AsRawFd as _;

    // SAFETY: 同 `try_lock_exclusive`；`LOCK_UN` 不依赖任何前置状态。
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

/// 非 Unix 平台没有 `flock`：退化为无锁。
#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> bool {
    true
}

/// 非 Unix 平台无需解锁。
#[cfg(not(unix))]
fn unlock(_file: &File) {}

#[cfg(test)]
mod tests {
    use super::{LockOutcome, acquire_at};
    use std::path::PathBuf;

    /// 临时锁路径：带进程号，避免与并发跑的其它测试撞名。
    fn temp_lock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("duck-lock-{tag}-{}.lock", std::process::id()))
    }

    #[test]
    fn second_acquire_reports_busy() {
        let path = temp_lock("busy");
        let first = acquire_at(path.clone()).expect("首次取锁不该报错");
        assert!(matches!(first, LockOutcome::Acquired(_)));

        let second = acquire_at(path.clone()).expect("被占是正常分支，不该是 Err");
        match second {
            LockOutcome::Busy { path: reported } => assert_eq!(reported, path),
            LockOutcome::Acquired(_) => panic!("同一进程的第二次取锁应当失败"),
        }

        drop(first);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lock_is_reusable_after_release() {
        let path = temp_lock("reuse");
        drop(acquire_at(path.clone()).expect("首次取锁不该报错"));
        // 释放后同一进程仍可重新拿到（锁跟着句柄走，不是一次性状态）。
        assert!(matches!(
            acquire_at(path.clone()).expect("释放后重取不该报错"),
            LockOutcome::Acquired(_)
        ));
        let _ = std::fs::remove_file(&path);
    }
}
