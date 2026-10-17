//! 平台键：注册表里工件是按平台给的，键的写法是 dbx 的口径
//! （`macos-aarch64` / `windows-x64` / `linux-aarch64` …）。
//!
//! 这层单独成模块的理由：它是一处**跨仓约定**，写错的表现不是编译失败，而是
//! 「注册表里明明有这个驱动，装的时候却说本平台没有工件」——把拼装集中在一处，
//! 并让单测钉住每个目标三元组的映射。

/// 当前构建目标的平台键。
///
/// 与注册表的六个键一一对应（[`ALL`](Self::ALL) 是全集）；不在表里的目标返回
/// `None`，调用方据此报「本平台不支持」而不是装错架构的二进制。
#[must_use]
pub fn current() -> Option<&'static str> {
    platform_key(std::env::consts::OS, std::env::consts::ARCH)
}

/// 全部已知平台键（与注册表一致）。
pub const ALL: [&str; 6] = [
    "macos-aarch64",
    "macos-x64",
    "windows-aarch64",
    "windows-x64",
    "linux-aarch64",
    "linux-x64",
];

/// `(os, arch)` → 平台键。
///
/// `os` / `arch` 用 `std::env::consts` 的写法（`macos` / `linux` / `windows`、
/// `aarch64` / `x86_64`）。**只认这三对组合**：其余（如 FreeBSD、32 位 x86）没有
/// 对应工件，返回 `None` 让调用方说实话。
#[must_use]
pub fn platform_key(os: &str, arch: &str) -> Option<&'static str> {
    let arch = match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x64",
        _ => return None,
    };
    let os = match os {
        "macos" => "macos",
        "windows" => "windows",
        "linux" => "linux",
        _ => return None,
    };
    ALL.into_iter().find(|key| {
        key.starts_with(os) && key.ends_with(arch) && key.len() == os.len() + 1 + arch.len()
    })
}

/// 可执行文件名：native 驱动进程与 `java` 在 Windows 上带 `.exe`。
#[must_use]
pub fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 六个键都要能从 `(os, arch)` 反推出来 —— 表与解析规则不会各写一份。
    #[test]
    fn every_key_round_trips() {
        for key in ALL {
            let (os, arch) = key.split_once('-').expect("键形如 <os>-<arch>");
            let arch = if arch == "x64" { "x86_64" } else { arch };
            assert_eq!(platform_key(os, arch), Some(key), "{key}");
        }
    }

    /// 本机目标在表内（三个发布平台都在），且拼出来的是注册表里真实存在的键。
    #[test]
    fn this_build_has_a_platform_key() {
        let key = current().expect("当前目标平台必须有平台键");
        assert!(ALL.contains(&key), "{key}");
    }

    /// 未知组合给 `None`，不猜。
    #[test]
    fn unknown_targets_are_not_guessed() {
        assert_eq!(platform_key("freebsd", "x86_64"), None);
        assert_eq!(platform_key("macos", "x86"), None);
        assert_eq!(platform_key("windows", "riscv64"), None);
    }
}
