//! 版本与发布通道。

use serde::Deserialize;

pub use semver::Version;

/// 发布通道。
///
/// 目前只发 `stable`，`beta` 先留着：manifest 里已经有这个字段，少一个变体会让
/// 某天一份带 `"beta"` 的文档**整份**解析失败，而那正是我们最想避免的失败形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// 正式版。
    Stable,
    /// 预发布版。
    Beta,
}

/// 本程序编译时的版本。
///
/// 全 workspace 共用 `version.workspace = true`，所以这里读到的就是 `duck-app`
/// 那个版本号；不必为「把二进制版本传进来」再开一个参数。
#[must_use]
pub fn compiled_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is a semantic version")
}

/// `offered` 是否比 `current` 新。
///
/// 相等与更旧一律算「已是最新」：降级不是本更新器的事，误判成「有新版」只会让用户
/// 反复被提示。
#[must_use]
pub fn is_newer(offered: &Version, current: &Version) -> bool {
    offered > current
}

#[cfg(test)]
mod tests {
    use super::{Version, compiled_version, is_newer};

    fn version(text: &str) -> Version {
        Version::parse(text).expect("valid semantic version")
    }

    #[test]
    fn a_higher_version_is_newer() {
        assert!(is_newer(&version("0.2.0"), &version("0.1.9")));
    }

    #[test]
    fn an_equal_or_older_version_is_not_newer() {
        assert!(!is_newer(&version("0.1.0"), &version("0.1.0")));
        assert!(!is_newer(&version("0.0.9"), &version("0.1.0")));
    }

    /// 预发布版低于同号正式版：`1.0.0-beta.1` 不该把 `1.0.0` 顶掉。
    #[test]
    fn a_prerelease_is_below_its_release() {
        assert!(!is_newer(&version("1.0.0-beta.1"), &version("1.0.0")));
    }

    #[test]
    fn this_build_has_a_semantic_version() {
        assert_eq!(compiled_version().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
