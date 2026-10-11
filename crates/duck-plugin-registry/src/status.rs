//! 本地与远端两份快照合并后的插件状态。

use semver::Version;

/// 一个插件相对「本机 + 远端」的位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginStatus {
    /// 远端有、本机没有 —— 插件中心的「未安装」。
    NotInstalled,
    /// 本机有、远端没有 —— 本地投放过、或已下架。
    Installed,
    /// 两边都有且远端不更新。
    UpToDate,
    /// 两边都有且远端版本更高 —— 插件中心的「可更新」。
    Updatable,
}

impl PluginStatus {
    /// 是否算「已安装」（`NotInstalled` 之外都是）。
    #[must_use]
    pub const fn is_installed(self) -> bool {
        !matches!(self, Self::NotInstalled)
    }

    /// 是否有可执行的安装 / 更新动作。
    #[must_use]
    pub const fn needs_action(self) -> bool {
        matches!(self, Self::NotInstalled | Self::Updatable)
    }
}

/// 由本地与远端版本号派生状态。
///
/// 比较用语义化版本；**任一侧版本不可解析时按「没有更新」处理**——宁可少提示一次
/// 更新，也不要在版本号写错时给用户一个点了没用的按钮。
#[must_use]
pub fn classify(local: Option<&str>, remote: Option<&str>) -> PluginStatus {
    match (local, remote) {
        (None, Some(_)) => PluginStatus::NotInstalled,
        (Some(_), None) => PluginStatus::Installed,
        // 两侧都没有的条目不该出现；按「未安装」处理，界面上会退化成不可操作项。
        (None, None) => PluginStatus::NotInstalled,
        (Some(local), Some(remote)) => match (Version::parse(local), Version::parse(remote)) {
            (Ok(local), Ok(remote)) if remote > local => PluginStatus::Updatable,
            _ => PluginStatus::UpToDate,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{PluginStatus, classify};

    #[test]
    fn remote_only_is_not_installed() {
        assert_eq!(classify(None, Some("1.0.0")), PluginStatus::NotInstalled);
        assert!(!classify(None, Some("1.0.0")).is_installed());
    }

    #[test]
    fn local_only_is_installed() {
        assert_eq!(classify(Some("1.0.0"), None), PluginStatus::Installed);
        assert!(classify(Some("1.0.0"), None).is_installed());
    }

    #[test]
    fn a_newer_remote_means_updatable() {
        assert_eq!(
            classify(Some("1.0.0"), Some("1.1.0")),
            PluginStatus::Updatable
        );
        assert!(classify(Some("1.0.0"), Some("1.1.0")).needs_action());
    }

    #[test]
    fn an_equal_or_older_remote_is_up_to_date() {
        assert_eq!(
            classify(Some("1.0.0"), Some("1.0.0")),
            PluginStatus::UpToDate
        );
        assert_eq!(
            classify(Some("2.0.0"), Some("1.0.0")),
            PluginStatus::UpToDate
        );
        assert!(!classify(Some("2.0.0"), Some("1.0.0")).needs_action());
    }

    /// 版本号写错时不许谎报「可更新」——那会让用户点到一个必然失败的按钮。
    #[test]
    fn an_unreadable_version_never_claims_an_update() {
        assert_eq!(
            classify(Some("nightly"), Some("1.0.0")),
            PluginStatus::UpToDate
        );
        assert_eq!(
            classify(Some("1.0.0"), Some("latest")),
            PluginStatus::UpToDate
        );
    }
}
