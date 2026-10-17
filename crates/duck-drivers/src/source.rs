//! 下载源：**同一个工件，从哪条路取**。
//!
//! 两个源，与 dbx 一致：
//!
//! | 源 | 优先 | 兜底 |
//! | --- | --- | --- |
//! | 官方 | GitHub Releases | CNB 镜像 |
//! | 镜像（国内） | CNB | GitHub Releases |
//!
//! 注册表与全部工件都放在 dbx 的 release 上（`agents-latest` / `agents-v0.2.142`），
//! 所以「换源」在实现上就是**同一个路径前缀的替换**：
//! `https://github.com/t8y2/dbx/releases/download/` ⇄
//! `https://cnb.cool/dbxio.com/dbx/-/releases/download/`。
//!
//! 除了替换得到的地址，官方源还会多试一条 **CDN**（`dl.dbxio.com`）—— 它当前只镜像
//! 注册表本身（工件在 CDN 上按未知路径摆放，实测 404），因此只给注册表加这一条候选；
//! 工件列表照旧是「GitHub 主 + CNB 备」。
//!
//! ⚠️ 不要试图从工件 URL 反推 CDN 路径：反推出来的路径**不存在**，白白多一次 404
//! 往返（每装一个驱动一次）。要加 CDN 工件镜像，得先确认真实路径。

use serde::{Deserialize, Serialize};

use crate::registry::{REGISTRY_CDN_PATH, REGISTRY_URL};

/// GitHub Releases 的下载前缀（注册表里工件 URL 的形态）。
pub const GITHUB_PREFIX: &str = "https://github.com/t8y2/dbx/releases/download/";

/// CNB（国内镜像）的下载前缀 —— 与 GitHub 前缀逐段对应。
pub const CNB_PREFIX: &str = "https://cnb.cool/dbxio.com/dbx/-/releases/download/";

/// CDN 基址。
pub const CDN_BASE: &str = "https://dl.dbxio.com/";

/// 注册表的镜像地址（同一份 JSON 的副本）。
pub const REGISTRY_MIRROR_URL: &str =
    "https://cnb.cool/dbxio.com/dbx/-/releases/download/agents-latest/agent-registry.json";

/// 更新下载源。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadSource {
    /// 官方源：GitHub 优先。
    #[default]
    Official,
    /// 国内镜像：CNB 优先。
    Mirror,
}

impl DownloadSource {
    /// 全部档位（界面上的下拉顺序）。
    pub const ALL: [Self; 2] = [Self::Official, Self::Mirror];

    /// 落盘值。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::Mirror => "mirror",
        }
    }

    /// 解析落盘值；未知值回落官方源。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "mirror" | "cnb" => Self::Mirror,
            _ => Self::Official,
        }
    }

    /// 把注册表里的 GitHub 地址换成**本源的优先地址**。
    ///
    /// 认不出的前缀原样返回（注册表将来换了托管方时，我们要照它给的地址走，
    /// 而不是拼一个自以为是的前缀出来）。
    #[must_use]
    pub fn prefer(self, github_url: &str) -> String {
        match self.candidates(github_url).as_slice() {
            [first, ..] => first.clone(),
            [] => github_url.to_owned(),
        }
    }

    /// 某个工件在**本源**下的地址列表（第一个优先，其余兜底）。
    ///
    /// 两个源**都包含对方**：主地址挂了照样要能下下来，区别只在谁先。
    /// 输入已经是镜像地址时也照同样规则给两条（不再重复计入同一个地址）。
    #[must_use]
    pub fn artifact_candidate_urls(self, github_url: &str) -> Vec<String> {
        self.candidates(github_url)
    }

    /// 两个前缀之间来回换，得到「GitHub 形态」与「CNB 形态」两个地址，再按源排序。
    ///
    /// 认不出的宿主返回单条（照原样走）—— **不猜**：拼一个不存在的镜像地址只会
    /// 多一次必然失败的往返。
    fn candidates(self, url: &str) -> Vec<String> {
        let github = if url.starts_with(GITHUB_PREFIX) {
            url.to_owned()
        } else if url.starts_with(CNB_PREFIX) {
            match rewrite_prefix(url, CNB_PREFIX, GITHUB_PREFIX) {
                Some(github) => github,
                None => return vec![url.to_owned()],
            }
        } else {
            return vec![url.to_owned()];
        };
        let Some(cnb) = rewrite_prefix(&github, GITHUB_PREFIX, CNB_PREFIX) else {
            return vec![github];
        };
        match self {
            Self::Official => vec![github, cnb],
            Self::Mirror => vec![cnb, github],
        }
    }
}

/// 注册表的候选地址（按优先级）。
#[must_use]
pub fn registry_candidate_urls(source: DownloadSource) -> Vec<String> {
    match source {
        // CDN 优先（离得近），GitHub 兜底，镜像最后 —— 官方源的用户通常在墙外。
        DownloadSource::Official => vec![
            format!("{CDN_BASE}{REGISTRY_CDN_PATH}"),
            REGISTRY_URL.to_owned(),
            REGISTRY_MIRROR_URL.to_owned(),
        ],
        // 镜像源的用户通常在墙内：CNB 优先，GitHub 次之，CDN 最后。
        DownloadSource::Mirror => vec![
            REGISTRY_MIRROR_URL.to_owned(),
            REGISTRY_URL.to_owned(),
            format!("{CDN_BASE}{REGISTRY_CDN_PATH}"),
        ],
    }
}

/// 前缀替换：`url` 必须以 `from` 开头，否则 `None`。
fn rewrite_prefix(url: &str, from: &str, to: &str) -> Option<String> {
    url.strip_prefix(from).map(|rest| format!("{to}{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAMENG: &str = "https://github.com/t8y2/dbx/releases/download/agents-v0.2.142/dbx-agent-dameng-0.1.92.tar.zst";
    const DAMENG_CNB: &str = "https://cnb.cool/dbxio.com/dbx/-/releases/download/agents-v0.2.142/dbx-agent-dameng-0.1.92.tar.zst";

    /// 官方源：GitHub 在前、CNB 兜底。
    ///
    /// 输入已经是镜像地址时，**仍按源排序**（官方源就是先试 GitHub）—— 注册表是从
    /// 镜像拉的、地址自然是镜像形态，这不改变用户选的源该先走哪条路。
    #[test]
    fn official_prefers_github_and_keeps_the_mirror_as_backup() {
        assert_eq!(
            DownloadSource::Official.artifact_candidate_urls(DAMENG),
            [DAMENG, DAMENG_CNB]
        );
        assert_eq!(
            DownloadSource::Official.artifact_candidate_urls(DAMENG_CNB),
            [DAMENG, DAMENG_CNB]
        );
    }

    /// 镜像源：CNB 在前、GitHub 兜底。
    #[test]
    fn mirror_prefers_cnb_and_keeps_github_as_backup() {
        assert_eq!(
            DownloadSource::Mirror.artifact_candidate_urls(DAMENG),
            [DAMENG_CNB, DAMENG]
        );
    }

    /// 认不出的前缀不动它 —— 将来换托管方时不会拼出坏地址。
    #[test]
    fn unknown_prefixes_are_left_alone() {
        let other = "https://example.com/whatever.tar.zst";
        assert_eq!(
            DownloadSource::Mirror.artifact_candidate_urls(other),
            [other]
        );
    }

    /// 注册表候选：两个源都覆盖 GitHub 与 CNB，且首条跟着源走。
    #[test]
    fn registry_candidates_cover_both_hosts() {
        for source in DownloadSource::ALL {
            let urls = registry_candidate_urls(source);
            assert!(urls.len() >= 2, "{source:?} 只有 {} 条候选", urls.len());
            assert!(urls.iter().any(|url| url.starts_with(GITHUB_PREFIX)));
            assert!(urls.iter().any(|url| url.starts_with(CNB_PREFIX)));
            assert!(
                urls.iter().all(|url| url.starts_with("https://")),
                "{urls:?}"
            );
        }

        // 官方源先走 CDN，镜像源先走 CNB —— 首条是「谁先被尝试」的唯一判据。
        assert_eq!(
            registry_candidate_urls(DownloadSource::Official)[0],
            format!("{CDN_BASE}{REGISTRY_CDN_PATH}")
        );
        assert_eq!(
            registry_candidate_urls(DownloadSource::Mirror)[0],
            REGISTRY_MIRROR_URL
        );
    }

    /// 落盘值往返。
    #[test]
    fn wire_names_round_trip() {
        for source in DownloadSource::ALL {
            assert_eq!(DownloadSource::from_wire(source.wire_name()), source);
        }
        assert_eq!(DownloadSource::from_wire("nope"), DownloadSource::Official);
    }
}
