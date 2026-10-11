//! manifest 的 `files` 表按平台键索引；这里是**读取端**那张表。
//!
//! 表写成数据而不是 `#[cfg]` 分支，是为了**每一行都能测**：四个发布目标里有三个无法
//! 在开发机上编译，拿 `cfg` 分成两条路，就等于两条永远没人跑过的分支。

/// manifest `files` 的键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlatformKey {
    /// macOS / Apple Silicon。
    MacosArm64,
    /// macOS / Intel。
    MacosX64,
    /// Linux / x86-64。
    LinuxX64,
    /// Windows / x86-64。
    WindowsX64,
}

impl PlatformKey {
    /// 本项目发版的全部平台。
    pub const ALL: [Self; 4] = [
        Self::MacosArm64,
        Self::MacosX64,
        Self::LinuxX64,
        Self::WindowsX64,
    ];

    /// 写进 manifest 的键名。
    ///
    /// 这是**线格式契约**：`sbin/package.sh` 生成的资产名、已发布的 manifest 都依赖它，
    /// 改一个就会让已经装好的用户再也找不到自己的条目。
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::MacosArm64 => "macos-arm64",
            Self::MacosX64 => "macos-x64",
            Self::LinuxX64 => "linux-x64",
            Self::WindowsX64 => "windows-x64",
        }
    }

    /// 认出一个 manifest 键；不认识的返回 `None`。
    ///
    /// **未知键不是解析错误**——见 [`Manifest::file_for`](crate::Manifest) 的说明：
    /// 将来多一个平台，不该让还在跑旧版的用户整份 manifest 都读不了。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|platform| platform.key() == value)
    }

    /// `(系统, 架构)` 对应的键；本项目不给该平台发版时为 `None`。
    #[must_use]
    pub fn from_host(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("macos", "aarch64") => Some(Self::MacosArm64),
            ("macos", "x86_64") => Some(Self::MacosX64),
            ("linux", "x86_64") => Some(Self::LinuxX64),
            ("windows", "x86_64") => Some(Self::WindowsX64),
            _ => None,
        }
    }

    /// 当前进程所在平台的键。
    ///
    /// `None` 是一个**真答案**而不是要糊弄过去的失败：自己编了个不常见目标的用户
    /// 确实没有对应归档，该如实说，而不是把别的平台的包塞给他。
    #[must_use]
    pub fn current() -> Option<Self> {
        Self::from_host(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// 该平台发行包的后缀。
    ///
    /// Windows 用 zip：`.zip` 保留不了可执行位，而在 Windows 上本来也不靠它；
    /// 其余平台用 `tar.gz`。
    #[must_use]
    pub const fn archive_extension(self) -> &'static str {
        match self {
            Self::WindowsX64 => ".zip",
            _ => ".tar.gz",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PlatformKey;

    #[test]
    fn every_release_target_maps_to_its_key() {
        for (os, arch, expected) in [
            ("macos", "aarch64", PlatformKey::MacosArm64),
            ("macos", "x86_64", PlatformKey::MacosX64),
            ("linux", "x86_64", PlatformKey::LinuxX64),
            ("windows", "x86_64", PlatformKey::WindowsX64),
        ] {
            assert_eq!(
                PlatformKey::from_host(os, arch),
                Some(expected),
                "{os}/{arch}"
            );
        }
    }

    /// 键名是线格式契约，和已发布的 manifest 绑死。
    #[test]
    fn the_keys_are_the_wire_contract() {
        assert_eq!(
            PlatformKey::ALL.map(PlatformKey::key),
            ["macos-arm64", "macos-x64", "linux-x64", "windows-x64"]
        );
    }

    #[test]
    fn a_platform_we_do_not_publish_for_has_no_key() {
        for (os, arch) in [
            ("linux", "aarch64"),
            ("windows", "aarch64"),
            ("freebsd", "x86_64"),
        ] {
            assert_eq!(PlatformKey::from_host(os, arch), None, "{os}/{arch}");
        }
    }

    #[test]
    fn an_unknown_manifest_key_parses_to_nothing() {
        assert_eq!(
            PlatformKey::parse("macos-arm64"),
            Some(PlatformKey::MacosArm64)
        );
        assert_eq!(PlatformKey::parse("freebsd-x64"), None);
        assert_eq!(PlatformKey::parse(""), None);
        assert_eq!(PlatformKey::parse("MACOS-ARM64"), None);
    }

    /// 这个二进制必须能在 manifest 里找到自己，否则更新器在它编译出来的平台上就是死代码。
    #[test]
    fn this_build_knows_which_platform_it_is() {
        assert!(
            PlatformKey::current().is_some(),
            "no manifest key for the target this test was built for"
        );
    }
}
