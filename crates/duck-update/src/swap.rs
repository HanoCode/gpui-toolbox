//! 把新的放回旧的所在之处，出错就放回去；并且留着旧的那份，随时能换回来。
//!
//! # 「原子」到什么程度
//!
//! 并不完全原子，而诚实的描述比这个词本身更重要。换位是**两次**改名：
//!
//! 1. `existing` → `existing.duck-old`
//! 2. `replacement` → `existing`
//!
//! 每次改名各自是原子的（两条路径在同一卷上——这正是
//! [`staging_dir`](crate::target::staging_dir) 存在的理由），但**这一对**不是：
//! 两次之间崩掉，`existing` 就什么都不剩。所以第 2 步失败时会把第 1 步退回去，
//! 「两边都不存在」的窗口只有一个 `rename(2)` 那么长。
//!
//! macOS 其实有真正原子的交换——`renamex_np` 配 `RENAME_SWAP`——这里没用，因为够到它
//! 就要为一个平台的一次调用引入直接依赖。这是个取舍，写下来而不是用「原子」二字盖过去。
//!
//! # 为什么旧的是被改名而不是被删除
//!
//! 两个理由，后者是主要的：
//!
//! 1. Windows 删不掉正在运行的可执行文件，但**能改名**——运行中的进程继续按新名字
//!    握着同一个文件，新二进制占住老路径。
//! 2. **它就是回退的落点**。[`rollback`] 把这两条路径再换一次，用户就回到了上一版。
//!    所以 [`sweep`] 启动时**不动** `.duck-old`；它只清理失败安装留下的暂存目录。
//!    同一时刻至多有一份 `.duck-old`：[`swap`] 顶掉当前版之前会先删掉更早的那一份。
//!
//! 代价是磁盘上常驻多一份安装（几十 MB 量级）。这是「更新后还能一键回去」的价钱，
//! 写在这里而不是让用户自己去猜为什么空间少了。

use std::path::{Path, PathBuf};

use crate::error::UpdateError;
use crate::install::ManualReason;
use crate::target::{is_staging, stale_path};

/// 本次进程能不能在 `dir` 里创建文件；不能的话原因是什么。
///
/// 用**真的建一个文件**来探测，而不是读权限位：权限位说明不了 ACL、只读挂载、
/// 系统完整性保护或沙箱，而唯一要紧的问题是「这次写会不会成功」。
///
/// # Errors
/// 探针写失败时返回用户该看到的那个原因。
pub fn probe_writable(dir: &Path) -> Result<(), ManualReason> {
    let probe = dir.join(format!(".duck-write-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(error) => Err(classify_probe_failure(error.kind())),
    }
}

/// 把探针失败翻译成给用户看的原因。
///
/// 拆成纯函数并单独测试，是因为两种情况给出的建议完全不同——「用管理员身份运行」对
/// 「把应用挪出这个卷」——而且只读文件系统在各地报的 `ErrorKind` 并不一致。
#[must_use]
pub fn classify_probe_failure(kind: std::io::ErrorKind) -> ManualReason {
    match kind {
        std::io::ErrorKind::PermissionDenied => ManualReason::NotWritable,
        // EROFS、目录不存在、沙箱拒绝：不是用户认证一下就能解决的权限问题，
        // 所以按「位置不对」报。
        _ => ManualReason::ReadOnlyLocation,
    }
}

/// 用 `replacement` 顶掉 `existing`，返回旧的那份被挪去了哪里。
///
/// 两条路径必须在同一卷上。失败时旧的会被放回去，所以一次失败的安装不会留下
/// 一个半成品安装。
///
/// 顶掉之前会先删掉更早的那一份 `.duck-old`：磁盘上只留**一份**上一版，
/// 否则每更新一次就多堆一份安装。
///
/// # Errors
/// 两次改名任一失败都返回 [`UpdateError::Install`]；回滚也失败时，错误里会带上
/// 旧版本现在的实际位置，让用户能手动恢复。
pub fn swap(existing: &Path, replacement: &Path) -> Result<PathBuf, UpdateError> {
    let old = stale_path(existing);
    // 上一次保留下来的那份会让第一次改名失败——在有些平台上「改名到一个已存在的路径」是错误；
    // 而且留两份也没有意义，回退只需要最近的一版。
    remove_any(&old);

    let existed = existing.exists();
    if existed {
        std::fs::rename(existing, &old).map_err(|error| {
            UpdateError::Install(format!(
                "could not move the current version aside ({}): {error}",
                existing.display()
            ))
        })?;
    }

    if let Err(error) = std::fs::rename(replacement, existing) {
        // 放回去。如果这一步也失败，就没有别的可试了，错误里带上被挪开的那份的位置。
        if existed && std::fs::rename(&old, existing).is_err() {
            return Err(UpdateError::Install(format!(
                "the update failed and the previous version could not be restored; it is at {}",
                old.display()
            )));
        }
        return Err(UpdateError::Install(format!(
            "could not move the new version into place ({}): {error}",
            existing.display()
        )));
    }

    Ok(old)
}

/// 把 `existing` 换回上一版，返回上一版原来的位置（也就是当前版被挪去的地方）。
///
/// 这是 [`swap`] 的反向操作，但**不做删除**：换完之后当前版就成了新的「上一版」，
/// 于是两个版本可以在磁盘上交替来回——用户点第二次回退就回到了更新后的版本。
/// 想把其中一个彻底丢掉，得在它成为「更早的那一份」时由下一次 [`swap`] 顺手清掉。
///
/// 与 [`swap`] 一样，真正的互换是三次改名（借助一个临时名），中途失败会把已经挪开的
/// 那份放回去。诚实的说法：`existing` 短暂消失的窗口只有一个 `rename(2)` 那么长。
///
/// # Errors
/// 没有上一版可退时返回 [`UpdateError::NoPreviousVersion`]（这是**正常状态**，
/// 不是故障——从来没更新过，或者上一次更新被清过）；改名失败返回
/// [`UpdateError::Install`]。
pub fn rollback(existing: &Path) -> Result<PathBuf, UpdateError> {
    let previous = stale_path(existing);
    if !previous.exists() {
        return Err(UpdateError::NoPreviousVersion);
    }

    // 当前版先挪到一个临时名——不能直接挪到 `previous`，那个名字正被要换回去的那份占着。
    let temporary = scratch_path(existing);
    remove_any(&temporary);

    // 当前版可能已经不在了（用户手工删过、或者上一次 swap 崩在最窄的那个窗口里）。
    // 那种情况下没有东西需要留作新的「上一版」，直接换上去就好。
    let moved_aside = existing.exists();
    if moved_aside {
        std::fs::rename(existing, &temporary).map_err(|error| {
            UpdateError::Install(format!(
                "could not move the current version aside ({}): {error}",
                existing.display()
            ))
        })?;
    }

    if let Err(error) = std::fs::rename(&previous, existing) {
        if moved_aside {
            let _ = std::fs::rename(&temporary, existing);
        }
        return Err(UpdateError::Install(format!(
            "could not restore the previous version ({}): {error}",
            previous.display()
        )));
    }

    if moved_aside {
        std::fs::rename(&temporary, &previous).map_err(|error| {
            // 上一版已经就位了，用户重启能跑；只是「再回退一次」的落点丢了。
            // 这仍然是错误——但它不该把已经成功的回退说成失败。
            UpdateError::Install(format!(
                "the previous version is in place, but the current one could not be kept as the new fallback; it is at {}: {error}",
                temporary.display()
            ))
        })?;
    }

    Ok(previous)
}

/// 换位途中用的临时名：与目标同目录（保证同卷），带进程号（两个进程不撞车）。
fn scratch_path(existing: &Path) -> PathBuf {
    let mut name = existing
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "duck".to_owned());
    name.push_str(&format!(".duck-swap-{}", std::process::id()));
    existing.with_file_name(name)
}

/// 清掉失败安装留下的暂存目录（同一个目录下）。
///
/// **不动 `.duck-old`**：那是上一版，[`rollback`] 的落点。这一条是本模块最容易
/// 被「顺手清理」改坏的地方，所以它有专门的测试盯着。
///
/// 全程 best effort：删不掉的残留不值得上报，下次启动还会再试。
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_staging(&path) {
            remove_any(&path);
        }
    }
}

/// 文件目录通删：`.app` 是目录，`duck` 是文件，换位代码对它们一视同仁。
fn remove_any(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_probe_failure, probe_writable, rollback, swap, sweep};
    use crate::error::UpdateError;
    use crate::install::ManualReason;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("duck-swap-test-{}-{index}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        dir
    }

    #[test]
    fn a_swap_installs_the_new_copy_and_keeps_the_old_one_aside() {
        let dir = scratch();
        let installed = dir.join("duck");
        let staged = dir.join("staging-duck");
        std::fs::write(&installed, b"old").expect("writes");
        std::fs::write(&staged, b"new").expect("writes");

        let old = swap(&installed, &staged).expect("swaps");

        assert_eq!(std::fs::read(&installed).expect("installed"), b"new");
        assert_eq!(
            std::fs::read(&old).expect("kept aside"),
            b"old",
            "the previous version has to survive until a later launch sweeps it"
        );
        assert!(!staged.exists(), "the staged copy was moved, not copied");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `.app` 是目录，换位逻辑对目录和文件要表现一致。
    #[test]
    fn a_directory_swaps_the_same_way_a_file_does() {
        let dir = scratch();
        let installed = dir.join("Duck Toolbox.app");
        let staged = dir.join(".duck-update-staging/Duck Toolbox.app");
        std::fs::create_dir_all(installed.join("Contents/MacOS")).expect("creates");
        std::fs::create_dir_all(staged.join("Contents/MacOS")).expect("creates");
        std::fs::write(installed.join("Contents/MacOS/duck"), b"old").expect("writes");
        std::fs::write(staged.join("Contents/MacOS/duck"), b"new").expect("writes");

        swap(&installed, &staged).expect("swaps");

        assert_eq!(
            std::fs::read(installed.join("Contents/MacOS/duck")).expect("installed"),
            b"new"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清扫**只**清暂存目录：`.duck-old` 是回退的落点，动它就是毁掉回退。
    #[test]
    fn sweeping_clears_staging_and_keeps_the_previous_version() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join(".duck-update-1234")).expect("creates");
        std::fs::write(dir.join(".duck-update-1234/leftover"), b"half unpacked").expect("writes");
        std::fs::write(dir.join("Duck Toolbox.app.duck-old"), b"previous").expect("writes");
        std::fs::write(dir.join("duck"), b"live").expect("writes");
        std::fs::write(dir.join("untouched"), b"live").expect("writes");

        sweep(&dir);

        assert!(
            !dir.join(".duck-update-1234").exists(),
            "a half-finished install must not pile up"
        );
        assert!(
            dir.join("Duck Toolbox.app.duck-old").exists(),
            "the kept previous version is the whole point of rollback"
        );
        assert!(dir.join("duck").exists(), "the live copy stays");
        assert!(dir.join("untouched").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rollback_swaps_the_previous_version_back_in() {
        let dir = scratch();
        let installed = dir.join("duck");
        std::fs::write(&installed, b"new").expect("writes");
        std::fs::write(dir.join("duck.duck-old"), b"old").expect("writes");

        let previous = rollback(&installed).expect("rolls back");

        assert_eq!(std::fs::read(&installed).expect("installed"), b"old");
        assert_eq!(
            std::fs::read(&previous).expect("kept aside"),
            b"new",
            "the version we rolled back from becomes the new fallback"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 来回切换：回退两次应当回到原处，谁都不丢。
    #[test]
    fn rolling_back_twice_returns_to_where_it_started() {
        let dir = scratch();
        let installed = dir.join("duck");
        std::fs::write(&installed, b"v2").expect("writes");
        std::fs::write(dir.join("duck.duck-old"), b"v1").expect("writes");

        rollback(&installed).expect("first rollback");
        assert_eq!(std::fs::read(&installed).expect("reads"), b"v1");

        rollback(&installed).expect("second rollback");
        assert_eq!(
            std::fs::read(&installed).expect("reads"),
            b"v2",
            "rolling back twice must land where we started, not on a half state"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 从来没有上一版：这是正常状态，不是故障。
    #[test]
    fn a_rollback_with_nothing_to_fall_back_to_says_so() {
        let dir = scratch();
        let installed = dir.join("duck");
        std::fs::write(&installed, b"only copy").expect("writes");

        assert_eq!(rollback(&installed), Err(UpdateError::NoPreviousVersion));
        assert_eq!(
            std::fs::read(&installed).expect("reads"),
            b"only copy",
            "a refused rollback must leave the installation alone"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没有任何临时名残留：互换用的是同目录下的两个固定位置，跑完就该干干净净。
    #[test]
    fn a_rollback_leaves_no_temporary_names_behind() {
        let dir = scratch();
        let installed = dir.join("duck");
        std::fs::write(&installed, b"new").expect("writes");
        std::fs::write(dir.join("duck.duck-old"), b"old").expect("writes");

        rollback(&installed).expect("rolls back");

        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("reads")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            2,
            "expected exactly the live copy and the fallback, got {names:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_that_can_be_written_to_passes_the_probe() {
        let dir = scratch();
        assert_eq!(probe_writable(&dir), Ok(()));
        assert!(
            std::fs::read_dir(&dir).expect("reads").next().is_none(),
            "the probe file is cleaned up"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_is_reported_as_a_location_problem() {
        let missing = std::env::temp_dir().join("duck-probe-does-not-exist");
        let _ = std::fs::remove_dir_all(&missing);
        assert_eq!(
            probe_writable(&missing),
            Err(ManualReason::ReadOnlyLocation)
        );
    }

    #[test]
    fn only_permission_denied_reads_as_not_writable() {
        assert_eq!(
            classify_probe_failure(ErrorKind::PermissionDenied),
            ManualReason::NotWritable
        );
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::ReadOnlyFilesystem,
            ErrorKind::Other,
        ] {
            assert_eq!(
                classify_probe_failure(kind),
                ManualReason::ReadOnlyLocation,
                "{kind:?}"
            );
        }
    }
}
