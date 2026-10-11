//! 替换 `duck.exe`。
//!
//! manifest 的 windows 条目指向一个 **zip**（见 `PlatformKey::archive_extension`），
//! 里面是**更新归档**——只有 `duck.exe` 落在压缩包根上。那是与分发用便携目录 zip
//! 不同的另一份产物，由 `sbin/package.ps1` 的 `-UpdateArchive` 生成。
//!
//! # 为什么只换 exe
//!
//! Windows 的分发形态是一个便携目录；更新只替换其中的可执行文件，目录里用户自己放的
//! 东西（配置、插件产物）原地不动。将来若出现必须随版本一起换的旁挂文件，这里就要
//! 扩成「整目录换位」——写下来，免得那时候以为漏了。
//!
//! # 运行中的 exe 删不掉，但改得动名
//!
//! 这正是 [`swap`](crate::swap) 那套「先改名、再放新的」的由来。旧的那份**不被删掉**：
//! 它留在 `.duck-old` 上，是 [`rollback`](PlatformInstaller::rollback) 换回去的落点。
//! 与 macOS 同走一条路径。
//!
//! # 拒绝是正常结局
//!
//! 与 macOS 一样，两种情况走 [`InstallOutcome::Manual`] 而不是错误：归档已下好并校验过，
//! 只是这台机器写不进去那个目录（装在 `Program Files` 下而当前不是管理员）。

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::UpdateError;
use crate::swap;
use crate::target::{staging_dir, stale_path};

use super::{InstallOutcome, ManualReason, PlatformInstaller, sole_entry_ending_with};

/// 更新归档里那个可执行文件的名字。
const EXE_NAME: &str = "duck.exe";

/// Windows 安装器。
#[derive(Default)]
pub struct WindowsInstaller {
    /// 运行中的可执行文件；`None` 表示问系统要。
    ///
    /// 与 macOS 安装器同一个覆盖点，存在的唯一目的是让安装路径能对着一个**伪造的
    /// 便携目录**测试，而不是对着测试二进制自己。生产路径永远是 `None`。
    executable: Option<PathBuf>,
}

impl WindowsInstaller {
    /// 用真实运行路径的安装器。
    ///
    /// 非 Windows 上只有测试会碰到这个类型，而测试一律用 [`Self::at`] 造实例，
    /// 于是这个构造函数在那边没人调用——不是死代码，只是平台错位。
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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

impl PlatformInstaller for WindowsInstaller {
    fn install(&self, archive: &Path) -> Result<InstallOutcome, UpdateError> {
        let executable = self.executable()?;

        // Windows 上没有 bundle 概念：被替换的永远是这一个 exe，所以「就地装」就是换掉它。
        let Some(parent) = executable.parent() else {
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

        let Some(new_exe) = sole_entry_ending_with(&staging, EXE_NAME) else {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(UpdateError::Install(format!(
                "the archive does not contain exactly one {EXE_NAME}"
            )));
        };

        let result = swap::swap(&executable, &new_exe);
        let _ = std::fs::remove_dir_all(&staging);
        result?;

        tracing::info!("installed; a restart will run the new version");
        Ok(InstallOutcome::Installed)
    }

    fn relaunch(&self) -> Result<(), UpdateError> {
        let executable = self.executable()?;
        Command::new(executable)
            .spawn()
            .map(|_| ())
            .map_err(|error| UpdateError::Install(format!("could not relaunch: {error}")))
    }

    fn sweep_stale(&self) {
        let Ok(executable) = self.executable() else {
            return;
        };
        if let Some(parent) = executable.parent() {
            swap::sweep(parent);
        }
    }

    fn rollback(&self) -> Result<(), UpdateError> {
        let executable = self.executable()?;
        swap::rollback(&executable).map(|_| ())
    }

    fn has_previous(&self) -> bool {
        let Ok(executable) = self.executable() else {
            return false;
        };
        stale_path(&executable).exists()
    }
}

/// 把归档解到 `into`。
///
/// 首选系统的 `tar`：Windows 10 1803+ 自带 bsdtar，它**同时认得 zip 与 tar.gz**，
/// 一条命令覆盖两种格式。没有它（更老的系统）才回退到 PowerShell 的 `Expand-Archive`——
/// 那个只认 zip，而我们的更新归档本来就是 zip。
fn extract(archive: &Path, into: &Path) -> Result<(), UpdateError> {
    std::fs::create_dir_all(into)
        .map_err(|error| UpdateError::Io(format!("{}: {error}", into.display())))?;

    let tar = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .output();
    if succeeded(&tar) {
        return Ok(());
    }

    let script = format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        powershell_literal(archive),
        powershell_literal(into)
    );
    let expanded = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
    if succeeded(&expanded) {
        return Ok(());
    }

    // 两条路都没走通。**优先报 tar 的详情**：它认得 zip，失败基本就是归档真的坏了，
    // 而此刻 PowerShell 常常是「压根不存在」（开发机、精简系统），拿它的
    // "No such file or directory" 顶上去只会把人引偏。
    let detail = failure_detail(&tar)
        .or_else(|| failure_detail(&expanded))
        .unwrap_or_else(|| "no usable unpacker (tar or powershell)".to_owned());
    Err(UpdateError::Install(format!(
        "unpacking {} failed: {detail}",
        archive.display()
    )))
}

/// 把路径塞进 PowerShell 的**单引号字面量**里。
///
/// PowerShell 的单引号串里只有单引号需要转义，写法是写两遍（`''`）。不转义的话，
/// 路径里出现一个 `'` 就能让 `-Command` 的脚本提前收尾，**剩下的部分变成新语句执行**。
/// 而这两个路径（下载目录、归档名）都不是我们能控制的——归档名来自更新清单，
/// 目录名来自用户环境，于是「解压一个路径里带单引号的包」就等于执行任意 PowerShell。
/// 开发机上不常见，但这是白送的提权面，没有理由留着。
fn powershell_literal(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}

/// 命令跑起来了**并且**退出码是 0。
///
/// 拆成一个谓词而不是层层 `if let` + `if`：既读了得，也不必用 `let` 链——
/// 那要求 Rust 1.88，会顶破本仓声明的 1.85。
fn succeeded(output: &std::io::Result<std::process::Output>) -> bool {
    matches!(output, Ok(output) if output.status.success())
}

/// 一条解包命令的失败详情：跑起来了取 stderr，压根没跑起来就取 spawn 错误。
///
/// 返回 `None`（跑起来了、但 stderr 是空的）让调用方能继续往下找一个更有内容的来源。
fn failure_detail(output: &std::io::Result<std::process::Output>) -> Option<String> {
    match output {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            (!text.is_empty()).then_some(text)
        }
        Err(error) => Some(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{WindowsInstaller, powershell_literal};
    use crate::install::{InstallOutcome, ManualReason, PlatformInstaller};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 路径里的单引号必须被写成两个 —— 否则脚本会被提前收尾，后面那段成了新语句。
    #[test]
    fn powershell_literal_doubles_quotes() {
        assert_eq!(
            powershell_literal(Path::new(r"C:\Users\duck\Updates")),
            r"C:\Users\duck\Updates"
        );
        assert_eq!(
            powershell_literal(Path::new(r"C:\it's here\duck's.zip")),
            r"C:\it''s here\duck''s.zip"
        );
        // 反引号 / 分号在单引号串里是普通字符，不需要动。
        assert_eq!(powershell_literal(Path::new(r"C:\a;`b")), r"C:\a;`b");
    }

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("duck-windows-test-{}-{index}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        dir
    }

    /// 在 `dir` 下造一个便携目录，返回「运行中的 duck.exe」本该在哪。
    fn fake_install(dir: &Path, marker: &[u8]) -> PathBuf {
        std::fs::create_dir_all(dir).expect("creates");
        let exe = dir.join("duck.exe");
        std::fs::write(&exe, marker).expect("writes");
        exe
    }

    /// 把 `dir` 里点名的文件打成归档。
    ///
    /// 用 `.tar.gz` 而不是 `.zip` 只是为了让同一段 fixture 代码在开发机和 Windows 上都能跑；
    /// 生产走的是同一条 `tar -xf`，它两种格式都认（没装 tar 的机器才落到 `Expand-Archive`，
    /// 而那条分支只对 zip 有意义——所以这里刻意不为它造第二次 fixture）。
    fn pack(dir: &Path, into: &Path, names: &[&str]) {
        let mut command = std::process::Command::new("tar");
        command.arg("-czf").arg(into).arg("-C").arg(dir);
        for name in names {
            command.arg(name);
        }
        assert!(command.status().expect("tar").success(), "packing failed");
    }

    /// 整个安装流程跑一遍：打一份新的 `duck.exe`，盖掉旧的，看到新字节就位、
    /// 旧的那份被改名留着给下次启动清扫。
    #[test]
    fn installs_a_new_executable_over_the_running_one() {
        let root = scratch();

        let source = root.join("source");
        std::fs::create_dir_all(&source).expect("creates");
        std::fs::write(source.join("duck.exe"), b"new binary").expect("writes");
        let archive = root.join("duck-update.zip");
        pack(&source, &archive, &["duck.exe"]);

        let installed = root.join("installed");
        let running = fake_install(&installed, b"old binary");

        let outcome = WindowsInstaller::at(running)
            .install(&archive)
            .expect("installs");
        assert_eq!(outcome, InstallOutcome::Installed);
        assert_eq!(
            std::fs::read(installed.join("duck.exe")).expect("installed"),
            b"new binary"
        );
        assert!(
            installed.join("duck.exe.duck-old").exists(),
            "the previous executable stays until a later launch sweeps it"
        );
        assert!(
            !installed
                .join(format!(".duck-update-{}", std::process::id()))
                .exists(),
            "the staging directory is cleaned up"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 用户自己放在便携目录里的东西不该被动到。
    #[test]
    fn files_beside_the_executable_are_left_alone() {
        let root = scratch();

        let source = root.join("source");
        std::fs::create_dir_all(&source).expect("creates");
        std::fs::write(source.join("duck.exe"), b"new binary").expect("writes");
        let archive = root.join("duck-update.zip");
        pack(&source, &archive, &["duck.exe"]);

        let installed = root.join("installed");
        let running = fake_install(&installed, b"old binary");
        std::fs::write(installed.join("settings.toml"), b"mine").expect("writes");

        WindowsInstaller::at(running)
            .install(&archive)
            .expect("installs");

        assert_eq!(
            std::fs::read(installed.join("settings.toml")).expect("kept"),
            b"mine",
            "only the executable is replaced"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 启动时清扫**只**清暂存目录：`.duck-old` 是回退的落点，留着。
    #[test]
    fn a_later_launch_clears_staging_and_keeps_the_fallback() {
        let root = scratch();
        let installed = root.join("installed");
        let running = fake_install(&installed, b"current");
        std::fs::write(installed.join("duck.exe.duck-old"), b"previous").expect("writes");
        std::fs::create_dir_all(installed.join(".duck-update-9999")).expect("creates");

        WindowsInstaller::at(running).sweep_stale();

        assert!(
            installed.join("duck.exe.duck-old").exists(),
            "the previous version is what rollback falls back to"
        );
        assert!(
            !installed.join(".duck-update-9999").exists(),
            "a half-finished install must not pile up"
        );
        assert!(
            installed.join("duck.exe").exists(),
            "the live executable stays"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 归档里没有 `duck.exe` 是真失败，而且不能动到现有安装。
    #[test]
    fn an_archive_without_the_executable_is_a_real_failure() {
        let root = scratch();

        let source = root.join("source");
        std::fs::create_dir_all(source.join("nested")).expect("creates");
        std::fs::write(source.join("nested/duck.exe"), b"wrong place").expect("writes");
        let archive = root.join("nested.tar.gz");
        pack(&source, &archive, &["nested"]);

        let installed = root.join("installed");
        let running = fake_install(&installed, b"old binary");

        let error = WindowsInstaller::at(running)
            .install(&archive)
            .expect_err("an archive with the exe one level down cannot be installed");
        assert!(error.to_string().contains("exactly one"), "{error}");
        assert_eq!(
            std::fs::read(installed.join("duck.exe")).expect("untouched"),
            b"old binary",
            "a failed install must leave the installation alone"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 归档本身是坏的（不是压缩包）：报错，而不是把现有安装弄成半成品。
    #[test]
    fn a_corrupt_archive_fails_without_touching_the_installation() {
        let root = scratch();
        let archive = root.join("duck-update.zip");
        std::fs::write(&archive, b"this is not an archive").expect("writes");

        let installed = root.join("installed");
        let running = fake_install(&installed, b"old binary");

        let error = WindowsInstaller::at(running)
            .install(&archive)
            .expect_err("a corrupt archive cannot be unpacked");
        assert!(
            error.to_string().contains("unpacking"),
            "the error must say what failed: {error}"
        );
        assert_eq!(
            std::fs::read(installed.join("duck.exe")).expect("untouched"),
            b"old binary"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 拒绝是一次带归档位置的**成功**，不是 `Err`。
    #[test]
    fn a_missing_install_directory_is_refused_cleanly() {
        let root = scratch();
        let archive = root.join("duck-update.zip");
        std::fs::write(&archive, b"unused: nothing gets extracted").expect("writes");

        // 一个不存在的目录：`probe_writable` 会如实说「这里写不进去」。
        let running = root.join("gone").join("duck.exe");

        assert_eq!(
            WindowsInstaller::at(running)
                .install(&archive)
                .expect("refuses cleanly"),
            InstallOutcome::Manual {
                reason: ManualReason::ReadOnlyLocation,
                archive: archive.clone(),
            }
        );
        assert!(
            archive.exists(),
            "the verified archive is kept for the user"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
