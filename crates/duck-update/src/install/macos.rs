//! 替换 `Duck Toolbox.app`。
//!
//! manifest 的 macOS 条目指向发行版的 `-app.tar.gz`，里面是**一个** `.app`。
//! 所以步骤是：在已安装的 bundle 旁边解包、清掉隔离属性、换位、重启。
//!
//! # 隔离属性（quarantine）
//!
//! 正式发行的包会用 Developer ID 证书签名并公证；没有那些密钥的构建（fork、本地
//! `sbin/package.sh`）只有 ad-hoc 签名，而经浏览器下载来的东西会带上
//! `com.apple.quarantine`，Gatekeeper 就会拒绝打开。我们自己的下载**不会**打这个
//! 属性——那是由下载它的应用决定的——但归档里可能已经在扩展属性里带着，所以在换位前
//! 用 `xattr -dr` 过一遍解出来的 bundle。**签名存在时这条依然成立**：删一个扩展属性
//! 不会让签名或公证票据失效，因为二者都不住在扩展属性里。失败只记日志、不阻断安装：
//! 在不需要它的机器上这就是个空操作，为一次「多此一举」的清理拒绝更新是荒谬的。
//!
//! # 拒绝是正常结局
//!
//! 三种情况会走到 [`InstallOutcome::Manual`] 而不是错误，且都很平常：跑的是裸二进制
//! （没有 bundle 可换）；`/Applications` 这份用户写不进去；bundle 在只读卷上
//! （直接从挂载的 dmg 里跑就是）。这三种都照样下载并校验，然后告诉用户归档在哪。

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::UpdateError;
use crate::install::{InstallOutcome, ManualReason, PlatformInstaller, sole_entry_ending_with};
use crate::swap;
use crate::target::{InstallTarget, classify, staging_dir, stale_path};

/// 归档里那个 bundle 的扩展名。
const BUNDLE_EXTENSION: &str = ".app";

/// macOS 安装器。
#[derive(Default)]
pub struct MacosInstaller {
    /// 运行中的可执行文件；`None` 表示问系统要。
    ///
    /// 这个覆盖点存在的唯一目的是让安装路径能对着一个**伪造的 bundle** 测试，
    /// 而不是对着测试二进制自己。生产路径永远是 `None`。
    executable: Option<PathBuf>,
}

impl MacosInstaller {
    /// 用真实运行路径的安装器。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 对着指定路径的安装器（测试用）。
    #[cfg(test)]
    fn at(executable: PathBuf) -> Self {
        Self {
            executable: Some(executable),
        }
    }

    fn executable(&self) -> Result<PathBuf, UpdateError> {
        match &self.executable {
            Some(path) => Ok(path.clone()),
            None => std::env::current_exe()
                .map_err(|error| UpdateError::Install(format!("current_exe: {error}"))),
        }
    }
}

impl PlatformInstaller for MacosInstaller {
    fn install(&self, archive: &Path) -> Result<InstallOutcome, UpdateError> {
        let executable = self.executable()?;
        let InstallTarget::MacosBundle { bundle, .. } = classify(&executable) else {
            tracing::warn!(
                "not running from an app bundle; leaving the archive for a manual install"
            );
            return Ok(InstallOutcome::Manual {
                reason: ManualReason::NotABundle,
                archive: archive.to_path_buf(),
            });
        };

        let Some(parent) = bundle.parent() else {
            return Ok(InstallOutcome::Manual {
                reason: ManualReason::ReadOnlyLocation,
                archive: archive.to_path_buf(),
            });
        };

        if let Err(reason) = swap::probe_writable(parent) {
            tracing::warn!(%reason, "the install directory is not writable");
            return Ok(InstallOutcome::Manual {
                reason,
                archive: archive.to_path_buf(),
            });
        }

        let staging = staging_dir(parent, std::process::id());
        // 上一次失败的安装可能留下暂存目录；不清掉会让解包结果混着旧文件。
        let _ = std::fs::remove_dir_all(&staging);
        extract(archive, &staging)?;

        let Some(new_bundle) = sole_entry_ending_with(&staging, BUNDLE_EXTENSION) else {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(UpdateError::Install(format!(
                "the archive does not contain exactly one {BUNDLE_EXTENSION} bundle"
            )));
        };

        strip_quarantine(&new_bundle);

        let result = swap::swap(&bundle, &new_bundle);
        let _ = std::fs::remove_dir_all(&staging);
        result?;

        tracing::info!("installed; a restart will run the new version");
        Ok(InstallOutcome::Installed)
    }

    fn relaunch(&self) -> Result<(), UpdateError> {
        let executable = self.executable()?;

        // `open -n` 起的是 bundle 的**新实例**：直接跑里面的可执行文件也行，
        // 但那样得到的进程在 Dock 和窗口服务器眼里不属于任何 bundle。
        let started = match classify(&executable) {
            InstallTarget::MacosBundle { bundle, .. } => {
                Command::new("open").arg("-n").arg(bundle).spawn()
            }
            // 裸二进制没有 bundle 可以交给 `open`，直接跑它。
            InstallTarget::BareExecutable { executable } => Command::new(executable).spawn(),
        };

        started
            .map(|_| ())
            .map_err(|error| UpdateError::Install(format!("could not relaunch: {error}")))
    }

    fn sweep_stale(&self) {
        let Ok(executable) = self.executable() else {
            return;
        };
        if let Some(parent) = classify(&executable).writable_parent() {
            swap::sweep(parent);
        }
    }

    fn rollback(&self) -> Result<(), UpdateError> {
        let executable = self.executable()?;
        let InstallTarget::MacosBundle { bundle, .. } = classify(&executable) else {
            return Err(UpdateError::Install(ManualReason::NotABundle.to_string()));
        };
        swap::rollback(&bundle).map(|_| ())
    }

    fn has_previous(&self) -> bool {
        let Ok(executable) = self.executable() else {
            return false;
        };
        let InstallTarget::MacosBundle { bundle, .. } = classify(&executable) else {
            return false;
        };
        stale_path(&bundle).exists()
    }
}

/// 把归档解到 `into`。
///
/// 交给系统的工具而不是链一个解包库：`tar` 每个 macOS 都有，`.zip` 用 `ditto`
/// （它认得 macOS 的扩展属性，`unzip` 不认得）。
fn extract(archive: &Path, into: &Path) -> Result<(), UpdateError> {
    std::fs::create_dir_all(into)
        .map_err(|error| UpdateError::Io(format!("{}: {error}", into.display())))?;

    let name = archive
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut command = if name.ends_with(".zip") {
        let mut command = Command::new("ditto");
        command.arg("-x").arg("-k").arg(archive).arg(into);
        command
    } else {
        let mut command = Command::new("tar");
        command.arg("-xzf").arg(archive).arg("-C").arg(into);
        command
    };

    let output = command
        .output()
        .map_err(|error| UpdateError::Install(format!("could not run the unpacker: {error}")))?;
    if !output.status.success() {
        return Err(UpdateError::Install(format!(
            "unpacking {} failed: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// 清掉解出来的 bundle 上的 `com.apple.quarantine`。
///
/// 设计上就是 best effort——见模块说明。只记日志、不返回错误，这样真被 Gatekeeper
/// 拦下时还能从日志里找回原因。
fn strip_quarantine(bundle: &Path) {
    match Command::new("xattr")
        .arg("-dr")
        .arg("com.apple.quarantine")
        .arg(bundle)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(_) => tracing::debug!("xattr reported nothing to clear"),
        Err(error) => tracing::warn!(%error, "could not run xattr"),
    }
}

#[cfg(test)]
mod tests {
    use super::MacosInstaller;
    use crate::error::UpdateError;
    use crate::install::{InstallOutcome, ManualReason, PlatformInstaller};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("duck-macos-test-{}-{index}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        dir
    }

    /// 在 `dir` 下造一个 `Duck Toolbox.app`，里面放上标记内容，返回「运行中的可执行文件」
    /// 本该在哪。
    fn fake_bundle(dir: &Path, marker: &[u8]) -> PathBuf {
        let macos = dir.join("Duck Toolbox.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).expect("creates");
        std::fs::write(macos.join("duck"), marker).expect("writes");
        macos.join("duck")
    }

    /// 把 `<dir>/Duck Toolbox.app` 打成发行版那种 `-app.tar.gz`。
    fn pack(dir: &Path, into: &Path) {
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(into)
            .arg("-C")
            .arg(dir)
            .arg("Duck Toolbox.app")
            .status()
            .expect("tar");
        assert!(status.success(), "packing the fixture failed");
    }

    /// 整个 macOS 安装流程跑一遍：打一个新的 `Duck Toolbox.app`，盖掉旧的，
    /// 看到新字节就位、旧的那份被挪到一边**留着**——它就是回退的落点。
    #[test]
    fn installs_a_new_bundle_over_the_running_one() {
        let root = scratch();

        let source = root.join("source");
        std::fs::create_dir_all(&source).expect("creates");
        fake_bundle(&source, b"new binary");
        let archive = root.join("duck-app.tar.gz");
        pack(&source, &archive);

        let installed = root.join("installed");
        std::fs::create_dir_all(&installed).expect("creates");
        let running = fake_bundle(&installed, b"old binary");

        let outcome = MacosInstaller::at(running)
            .install(&archive)
            .expect("installs");
        assert_eq!(outcome, InstallOutcome::Installed);
        assert_eq!(
            std::fs::read(installed.join("Duck Toolbox.app/Contents/MacOS/duck"))
                .expect("installed"),
            b"new binary"
        );
        assert!(
            installed.join("Duck Toolbox.app.duck-old").exists(),
            "the previous bundle is kept so the user can roll back to it"
        );
        assert!(
            !installed
                .join(format!(".duck-update-{}", std::process::id()))
                .exists(),
            "the staging directory is cleaned up"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 更新之后能回退：装好的新版被换回上一版。
    #[test]
    fn a_rollback_restores_the_previous_bundle() {
        let root = scratch();

        let source = root.join("source");
        std::fs::create_dir_all(&source).expect("creates");
        fake_bundle(&source, b"new binary");
        let archive = root.join("duck-app.tar.gz");
        pack(&source, &archive);

        let installed = root.join("installed");
        std::fs::create_dir_all(&installed).expect("creates");
        let running = fake_bundle(&installed, b"old binary");

        MacosInstaller::at(running.clone())
            .install(&archive)
            .expect("installs");
        assert_eq!(std::fs::read(&running).expect("installed"), b"new binary");

        MacosInstaller::at(running.clone())
            .rollback()
            .expect("rolls back");
        assert_eq!(
            std::fs::read(&running).expect("rolled back"),
            b"old binary",
            "rollback must bring the previous bundle back"
        );
        assert!(
            installed.join("Duck Toolbox.app.duck-old").exists(),
            "the version we rolled back from is kept as the new fallback"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 没有上一版时回退要说清楚，而不是装作成功。
    #[test]
    fn a_rollback_with_no_previous_bundle_reports_it() {
        let root = scratch();
        let installed = root.join("installed");
        std::fs::create_dir_all(&installed).expect("creates");
        let running = fake_bundle(&installed, b"only copy");

        let error = MacosInstaller::at(running.clone())
            .rollback()
            .expect_err("nothing to fall back to");
        assert!(matches!(error, UpdateError::NoPreviousVersion), "{error:?}");
        assert_eq!(
            std::fs::read(&running).expect("untouched"),
            b"only copy",
            "a refused rollback must leave the installation alone"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 界面拿 `has_previous` 决定「回退」按钮该不该出现，所以它必须在**装过之后**
    /// 才为真，而不是看有没有磁盘空间或者别的什么。
    #[test]
    fn has_previous_tracks_whether_a_fallback_is_on_disk() {
        let root = scratch();
        let installed = root.join("installed");
        std::fs::create_dir_all(&installed).expect("creates");
        let running = fake_bundle(&installed, b"current");

        assert!(
            !MacosInstaller::at(running.clone()).has_previous(),
            "nothing has been replaced yet"
        );

        std::fs::create_dir_all(installed.join("Duck Toolbox.app.duck-old")).expect("creates");
        assert!(MacosInstaller::at(running).has_previous());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 启动时清扫**只**清暂存目录：`.duck-old` 是回退的落点，留着。
    #[test]
    fn a_later_launch_clears_staging_and_keeps_the_fallback() {
        let root = scratch();
        let installed = root.join("installed");
        std::fs::create_dir_all(&installed).expect("creates");
        let running = fake_bundle(&installed, b"current");
        std::fs::create_dir_all(installed.join("Duck Toolbox.app.duck-old")).expect("creates");
        std::fs::create_dir_all(installed.join(".duck-update-9999")).expect("creates");

        MacosInstaller::at(running).sweep_stale();

        assert!(
            installed.join("Duck Toolbox.app.duck-old").exists(),
            "the previous version is what rollback falls back to"
        );
        assert!(
            !installed.join(".duck-update-9999").exists(),
            "a half-finished install must not pile up"
        );
        assert!(
            installed.join("Duck Toolbox.app").exists(),
            "the live bundle stays"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 拒绝是一次带归档位置的**成功**，不是 `Err`。
    #[test]
    fn a_bare_binary_is_refused_cleanly_rather_than_failing() {
        let root = scratch();
        let archive = root.join("duck-app.tar.gz");
        std::fs::write(&archive, b"unused: nothing gets extracted").expect("writes");
        let running = root.join("duck");
        std::fs::write(&running, b"a loose binary").expect("writes");

        assert_eq!(
            MacosInstaller::at(running)
                .install(&archive)
                .expect("refuses cleanly"),
            InstallOutcome::Manual {
                reason: ManualReason::NotABundle,
                archive: archive.clone(),
            }
        );
        assert!(
            archive.exists(),
            "the verified archive is kept for the user"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_archive_with_no_bundle_in_it_is_a_real_failure() {
        let root = scratch();

        let source = root.join("source");
        std::fs::create_dir_all(source.join("not-an-app")).expect("creates");
        std::fs::write(source.join("not-an-app/x"), b"x").expect("writes");
        let archive = root.join("wrong.tar.gz");
        assert!(
            std::process::Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&source)
                .arg("not-an-app")
                .status()
                .expect("tar")
                .success()
        );

        let installed = root.join("installed");
        std::fs::create_dir_all(&installed).expect("creates");
        let running = fake_bundle(&installed, b"old binary");

        let error = MacosInstaller::at(running)
            .install(&archive)
            .expect_err("an archive with no .app cannot be installed");
        assert!(error.to_string().contains(".app"), "{error}");
        assert_eq!(
            std::fs::read(installed.join("Duck Toolbox.app/Contents/MacOS/duck"))
                .expect("untouched"),
            b"old binary",
            "a failed install must leave the installation alone"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 归档里有两个 `.app` 时不能猜：安装器该把它当成「解包结果不合规」报错，
    /// 而不是随便挑一个换上去。
    #[test]
    fn an_archive_with_two_bundles_is_refused_rather_than_guessed() {
        let root = scratch();

        let source = root.join("source");
        for name in ["One.app", "Two.app"] {
            std::fs::create_dir_all(source.join(name)).expect("creates");
        }
        let archive = root.join("two-apps.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .arg("One.app")
            .arg("Two.app")
            .status()
            .expect("tar");
        assert!(status.success(), "packing the fixture failed");

        let installed = root.join("installed");
        std::fs::create_dir_all(&installed).expect("creates");
        let running = fake_bundle(&installed, b"old binary");

        let error = MacosInstaller::at(running)
            .install(&archive)
            .expect_err("two bundles cannot be resolved to one");
        assert!(error.to_string().contains("exactly one"), "{error}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
