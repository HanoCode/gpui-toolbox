//! 当前二进制在磁盘上「是什么」，因此安装器该替换「什么」。
//!
//! 这里全是**纯路径算术**，不碰磁盘——连 `exists()` 都不查。判断
//! `/Applications/Duck Toolbox.app/Contents/MacOS/duck` 意味着「换整个 bundle」是一条
//! 带边角的规则，而带边角的规则要的是一张测试表，不是文件系统。

use std::path::{Path, PathBuf};

/// 运行中的二进制是什么，以及「替换它」意味着什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallTarget {
    /// macOS 的 `.app`。`bundle` 是那个 `…/Duck Toolbox.app` 目录——归档里装的就是它，
    /// 所以整包替换。
    MacosBundle {
        /// `.app` 目录。
        bundle: PathBuf,
        /// 目录内部的真实可执行文件。
        executable: PathBuf,
    },
    /// 一个游离的可执行文件：`cargo run` 出来的、从裸归档里解出来的、被人丢进 `$PATH` 的。
    /// Linux / Windows 上可以就地替换；macOS 上**不行**，因为 manifest 只提供 bundle。
    BareExecutable {
        /// 可执行文件路径。
        executable: PathBuf,
    },
}

impl InstallTarget {
    /// 要被替换掉的那个东西。
    #[must_use]
    pub fn bundle(&self) -> Option<&Path> {
        match self {
            Self::MacosBundle { bundle, .. } => Some(bundle),
            Self::BareExecutable { .. } => None,
        }
    }

    /// 运行中的可执行文件。
    #[must_use]
    pub fn executable(&self) -> &Path {
        match self {
            Self::MacosBundle { executable, .. } | Self::BareExecutable { executable } => {
                executable
            }
        }
    }

    /// 就地安装需要可写的位置。
    ///
    /// 要的是被替换物的**父目录**：替换意味着在它旁边建一个新的、再改名盖上去。
    #[must_use]
    pub fn writable_parent(&self) -> Option<&Path> {
        match self {
            Self::MacosBundle { bundle, .. } => bundle.parent(),
            Self::BareExecutable { executable } => executable.parent(),
        }
    }
}

/// 判断运行中的可执行文件属于哪种目标。
///
/// macOS bundle 的内部布局是固定的 `Foo.app/Contents/MacOS/foo`，所以判据就是它：
/// 上一层叫 `Contents`、再上一层以 `.app` 结尾。其余一律算游离可执行文件——
/// 包括「只是恰好住在一个叫 `MacOS` 的目录里」的二进制。
///
/// 不按平台分支：这条规则是纯的，不分支才能在这里把它的边角全测掉；
/// 在 Windows / Linux 上现实中没有任何路径能满足它，结果一样是 `BareExecutable`。
#[must_use]
pub fn classify(executable: &Path) -> InstallTarget {
    let bare = || InstallTarget::BareExecutable {
        executable: executable.to_path_buf(),
    };

    let Some(macos_dir) = executable.parent() else {
        return bare();
    };
    if macos_dir.file_name().and_then(|name| name.to_str()) != Some("MacOS") {
        return bare();
    }
    let Some(contents_dir) = macos_dir.parent() else {
        return bare();
    };
    if contents_dir.file_name().and_then(|name| name.to_str()) != Some("Contents") {
        return bare();
    }
    let Some(bundle) = contents_dir.parent() else {
        return bare();
    };
    let is_bundle = bundle
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".app"));
    if !is_bundle {
        return bare();
    }

    InstallTarget::MacosBundle {
        bundle: bundle.to_path_buf(),
        executable: executable.to_path_buf(),
    }
}

/// 安装时解包用的暂存目录：被替换物的**隐藏同级项**。
///
/// 放在 `/tmp` 会让最后的改名变成跨文件系统拷贝——既不原子，对几十 MB 的 bundle 也不便宜。
/// 带上进程号是为了让两个同时在更新的进程不撞在一起。
#[must_use]
pub fn staging_dir(parent: &Path, pid: u32) -> PathBuf {
    parent.join(format!("{STAGING_PREFIX}{pid}"))
}

/// 安装时解包用的暂存目录的前缀。
///
/// 与 [`staging_dir`] 是同一个事实的两面：一个负责造名字，一个负责认名字。
/// **只有它是启动时该清扫的东西**——失败的安装会把它留下。
pub const STAGING_PREFIX: &str = ".duck-update-";

/// 这个路径是不是一次（可能失败的）安装留下的暂存目录。
#[must_use]
pub fn is_staging(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(STAGING_PREFIX))
}

/// 上一版被挪到哪去。
///
/// Windows 删不掉正在运行的可执行文件，但**能改名**；同一招在别的平台上就是回滚路径。
/// 这个后缀**不再**是「下次启动要清扫的目标」——它就是用户点「回退」时换回去的那一份，
/// 所以启动时不清它（见 [`sweep`](crate::swap::sweep)）。同一时刻至多留一份：
/// 顶掉当前版之前，更早的那一份会被删掉。
pub const STALE_SUFFIX: &str = ".duck-old";

/// 某个路径被挪到一边后的名字（也就是「上一版」将要去的位置）。
#[must_use]
pub fn stale_path(original: &Path) -> PathBuf {
    let mut name = original
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "duck".to_owned());
    name.push_str(STALE_SUFFIX);
    original.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::{InstallTarget, classify, is_staging, staging_dir, stale_path};
    use std::path::{Path, PathBuf};

    fn bundle_of(root: &str) -> PathBuf {
        Path::new(root).join("Duck Toolbox.app")
    }

    #[test]
    fn a_bundled_executable_is_recognised() {
        let executable = bundle_of("/Applications").join("Contents/MacOS/duck");
        assert_eq!(
            classify(&executable),
            InstallTarget::MacosBundle {
                bundle: bundle_of("/Applications"),
                executable,
            }
        );
    }

    /// 目录名对但层级不对的那些，都不是 bundle。
    #[test]
    fn a_path_that_only_looks_like_a_bundle_is_bare() {
        for candidate in [
            "/Applications/duck",
            "/Applications/duck.app/duck",
            "/Applications/duck.app/Contents/duck",
            "/Applications/Duck/Contents/MacOS/duck",
            "/Applications/MacOS/Contents/MacOS/duck",
        ] {
            assert_eq!(
                classify(Path::new(candidate)),
                InstallTarget::BareExecutable {
                    executable: PathBuf::from(candidate)
                },
                "{candidate}"
            );
        }
    }

    #[test]
    fn a_loose_binary_has_no_bundle_to_replace() {
        let target = classify(Path::new("/usr/local/bin/duck"));
        assert_eq!(target.bundle(), None);
        assert_eq!(target.writable_parent(), Some(Path::new("/usr/local/bin")));
    }

    #[test]
    fn staging_is_a_hidden_sibling_of_the_target() {
        assert_eq!(
            staging_dir(Path::new("/Applications"), 42),
            Path::new("/Applications/.duck-update-42")
        );
    }

    #[test]
    fn the_stale_path_keeps_the_extension_for_a_bundle() {
        assert_eq!(
            stale_path(&bundle_of("/Applications")),
            Path::new("/Applications/Duck Toolbox.app.duck-old")
        );
        assert_eq!(
            stale_path(Path::new("/usr/local/bin/duck")),
            Path::new("/usr/local/bin/duck.duck-old")
        );
    }

    #[test]
    fn only_staging_directories_are_recognised_as_leftovers() {
        assert!(is_staging(Path::new("/Applications/.duck-update-4321")));
        assert!(
            !is_staging(Path::new("/Applications/Duck Toolbox.app.duck-old")),
            "the kept previous version is not a leftover"
        );
        assert!(!is_staging(Path::new("/Applications/Duck Toolbox.app")));
        assert!(!is_staging(Path::new("/Applications/duck-update-4321")));
    }
}
