//! 用一份**已校验**的归档替换当前安装。
//!
//! 安装器不负责下载也不负责校验：它拿到的是 [`verify`](crate::verify::verify) 已经放行的
//! 路径。这样「什么会被装上」只有一个关口。

use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::UpdateError;

#[cfg(target_os = "macos")]
mod macos;

// Windows 安装器在**开发机的测试构建**里也在场：平台判断留给 `platform_installer`，
// 逻辑本身就能在非 Windows 上被测到（与 `target` 模块拒绝 `cfg` 分支同一个理由）。
#[cfg(any(target_os = "windows", test))]
mod windows;

/// 就地安装做不到时，用户该知道的原因。
///
/// 这些**都不是错误**：归档已经下好并校验过了，只是这台机器不适合原地换。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManualReason {
    /// 跑的是裸二进制（`cargo run` 出来的、从裸归档里解出来的），没有 bundle 可换。
    NotABundle,
    /// 安装目录当前用户写不进去。
    NotWritable,
    /// 安装位置在只读卷上——比如直接从挂载的 dmg 里运行。
    ReadOnlyLocation,
    /// 这个平台还没有就地安装的实现。
    Unsupported,
}

impl ManualReason {
    /// 英文描述；界面层把它套进中文句式里。
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotABundle => "this build is not running from an app bundle",
            Self::NotWritable => "the install location is not writable",
            Self::ReadOnlyLocation => "the install location is read-only",
            Self::Unsupported => "in-place updates are not supported on this platform",
        }
    }
}

impl fmt::Display for ManualReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// 一次安装的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    /// 已就地替换，下次启动就是新版本。
    Installed,
    /// 归档已下好并校验过，但这台机器无法就地更新；`archive` 是它的位置，交给用户手动处理。
    Manual {
        /// 为什么没能就地装。
        reason: ManualReason,
        /// 已验证的归档留在哪。
        archive: PathBuf,
    },
}

/// 用一份归档替换当前安装。
///
/// 三个方法都是阻塞的，调用点只会在后台任务里跑它们。
pub trait PlatformInstaller: Send + Sync + 'static {
    /// 把 `archive` 装到当前安装上。
    ///
    /// 返回 [`InstallOutcome::Manual`] 是**成功**的一种：这台机器就是没法原地更新。
    /// 只有真的失败——归档里没有 bundle、改名改到一半——才是 `Err`。
    ///
    /// # Errors
    /// 解包失败、归档里不是恰好一个 `.app`、换位失败且回滚也失败。
    fn install(&self, archive: &Path) -> Result<InstallOutcome, UpdateError>;

    /// 拉起刚装好的程序并让它继续跑。调用方在这之后立刻退出，本方法不代劳。
    ///
    /// # Errors
    /// 起不来时返回 [`UpdateError::Install`]。
    fn relaunch(&self) -> Result<(), UpdateError>;

    /// 删掉上次安装挪到一边的残留。
    ///
    /// 启动时调一次：失败的安装会留下暂存目录，删除发生在**后一个**进程里最省事。
    /// **不动 `.duck-old`**——那是上一版，回退的落点（见 [`rollback`](Self::rollback)）。
    fn sweep_stale(&self);

    /// 把上一版换回主位。成功后需要重启才生效（与安装同一个约定）。
    ///
    /// 这是 [`install`](Self::install) 的反向操作，但**不会**因为「没有上一版」而
    /// 静默成功——那种情况会明确报 [`UpdateError::NoPreviousVersion`]，界面才能说清
    /// 「没有可回退的版本」而不是含糊地失败。
    ///
    /// # Errors
    /// 没有上一版、位置不可换、改名失败。
    fn rollback(&self) -> Result<(), UpdateError>;

    /// 磁盘上还留着上一版吗。
    ///
    /// 界面拿它决定「回退」这个操作该不该出现——一个点了只会报错的按钮比没有更糟。
    /// 它必须**便宜**：只查一条路径，不解包、不算摘要。
    fn has_previous(&self) -> bool;
}

/// 当前平台的安装器。
#[must_use]
pub fn platform_installer() -> Box<dyn PlatformInstaller> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosInstaller::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsInstaller::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Box::new(UnsupportedInstaller)
    }
}

/// 解包后的暂存目录里**唯一**一个以 `suffix` 结尾的条目。
///
/// 恰好一个才算数：零个说明归档不对；多个说明我们不知道该换哪个——两种都不能猜。
/// 两个平台的安装器共用它：macOS 找 `.app`，Windows 找 `duck.exe`。
///
/// # Errors / None
/// 目录读不开、一个都没有、或不止一个时返回 `None`。
#[cfg(any(target_os = "macos", target_os = "windows", test))]
#[must_use]
pub(crate) fn sole_entry_ending_with(dir: &Path, suffix: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut matches = entries.flatten().map(|entry| entry.path()).filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(suffix))
    });
    let only = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(only)
}

/// 没有就地安装实现的平台的兜底：永远如实地说「这里做不到」，把归档留给用户。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct UnsupportedInstaller;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl PlatformInstaller for UnsupportedInstaller {
    fn install(&self, archive: &Path) -> Result<InstallOutcome, UpdateError> {
        Ok(InstallOutcome::Manual {
            reason: ManualReason::Unsupported,
            archive: archive.to_path_buf(),
        })
    }

    fn relaunch(&self) -> Result<(), UpdateError> {
        Err(UpdateError::Install(ManualReason::Unsupported.to_string()))
    }

    fn sweep_stale(&self) {}

    fn rollback(&self) -> Result<(), UpdateError> {
        Err(UpdateError::Install(ManualReason::Unsupported.to_string()))
    }

    fn has_previous(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::sole_entry_ending_with;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("duck-install-test-{}-{index}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        dir
    }

    #[test]
    fn a_single_match_in_the_staging_directory_is_found() {
        let root = scratch();
        std::fs::create_dir_all(root.join("Duck Toolbox.app")).expect("creates");
        std::fs::create_dir_all(root.join("README")).expect("creates");

        assert_eq!(
            sole_entry_ending_with(&root, ".app"),
            Some(root.join("Duck Toolbox.app"))
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_candidates_are_refused_rather_than_guessed() {
        let root = scratch();
        std::fs::create_dir_all(root.join("One.app")).expect("creates");
        std::fs::create_dir_all(root.join("Two.app")).expect("creates");

        assert_eq!(
            sole_entry_ending_with(&root, ".app"),
            None,
            "two candidates means we cannot know which one is ours"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 后缀不匹配的一律不算数：Windows 的归档里可能还躺着 LICENSE。
    #[test]
    fn entries_that_do_not_match_the_suffix_are_ignored() {
        let root = scratch();
        std::fs::write(root.join("duck.exe"), b"binary").expect("writes");
        std::fs::write(root.join("LICENSE"), b"text").expect("writes");

        assert_eq!(
            sole_entry_ending_with(&root, "duck.exe"),
            Some(root.join("duck.exe"))
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_matching_is_none_rather_than_a_guess() {
        let root = scratch();
        std::fs::write(root.join("notes.txt"), b"x").expect("writes");

        assert_eq!(sole_entry_ending_with(&root, ".app"), None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
