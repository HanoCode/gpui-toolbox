//! 智能粘贴：识别剪贴板内容并路由到对应工具。
//!
//! **纯函数、不碰文件系统也不碰剪贴板**——调用方负责取剪贴板文本（
//! `duck_platform::read_text`）与执行命中的工具；这里只做「这段文本属于哪个工具」的判定，
//! 因此全部规则都能在普通单测里覆盖。
//!
//! 各规则命中的工具 id 以 `TOOL_*` 常量给出，注册端（`duck-app` 的 catalog）引用同一组常量，
//! 避免两边各写一遍字符串。

/// 网盘直链解析。
pub const TOOL_NETDISK: &str = "netdisk.parse";
/// 视频统一解析。
pub const TOOL_VIDEO: &str = "video.parse";
/// HTTP 请求回放。
pub const TOOL_HTTP: &str = "http.replay";
/// 图片格式转换。
pub const TOOL_IMAGE: &str = "image.convert";
/// 压缩包解包。
pub const TOOL_ARCHIVE: &str = "archive.unpack";
/// 命令片段。
pub const TOOL_SNIPPET: &str = "snippet.save";
/// 批量重命名。
pub const TOOL_RENAME: &str = "file.rename";

/// 网盘分享站点。
const NETDISK_HOSTS: &[&str] = &[
    "pan.baidu.com",
    "yun.baidu.com",
    "aliyundrive.com",
    "alipan.com",
    "123pan.com",
    "pan.quark.cn",
    "cloud.189.cn",
    "share.weiyun.com",
    "115.com",
    "caiyun.139.com",
    "mypikpak.com",
    "terabox.com",
];

/// 视频站点。
const VIDEO_HOSTS: &[&str] = &[
    "bilibili.com",
    "b23.tv",
    "youtube.com",
    "youtu.be",
    "v.qq.com",
    "iqiyi.com",
    "youku.com",
    "douyin.com",
    "kuaishou.com",
    "ixigua.com",
    "mgtv.com",
    "acfun.cn",
    "vimeo.com",
    "weibo.com",
];

/// 图片扩展名。
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "avif", "gif", "bmp", "tiff", "heic",
];
/// 压缩包扩展名。
const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "7z", "rar", "tar", "gz", "bz2", "xz", "zst"];

/// 命令行的起始特征。
const COMMAND_PREFIXES: &[&str] = &[
    "$", "sudo ", "npm ", "pnpm ", "yarn ", "git ", "cargo ", "docker ", "kubectl ", "brew ",
    "python ", "pip ", "curl ", "ssh ",
];

/// 一次智能粘贴命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteMatch {
    tool_id: &'static str,
    detail: String,
}

impl PasteMatch {
    /// 命中的工具 id。
    #[must_use]
    pub fn tool_id(&self) -> &'static str {
        self.tool_id
    }

    /// 提取出的关键内容（链接 / 路径 / 命令），用于提示文案。
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// 识别一段文本应交给哪个工具；`None` 表示不识别。
///
/// 规则按优先级：网盘链接 → 视频链接 → 其他链接 → 图片 → 压缩包 → 命令行 → 文件路径。
#[must_use]
pub fn detect(text: &str) -> Option<PasteMatch> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lowered = trimmed.to_lowercase();

    let hit = |tool_id| {
        Some(PasteMatch {
            tool_id,
            detail: trimmed.to_owned(),
        })
    };

    if has_http_url(&lowered) {
        if NETDISK_HOSTS.iter().any(|host| lowered.contains(host)) {
            return hit(TOOL_NETDISK);
        }
        if VIDEO_HOSTS.iter().any(|host| lowered.contains(host)) {
            return hit(TOOL_VIDEO);
        }
        return hit(TOOL_HTTP);
    }

    match extension_of(trimmed).as_deref() {
        Some(extension) if IMAGE_EXTENSIONS.contains(&extension) => return hit(TOOL_IMAGE),
        Some(extension) if ARCHIVE_EXTENSIONS.contains(&extension) => return hit(TOOL_ARCHIVE),
        _ => {}
    }

    if is_command_like(trimmed) {
        return hit(TOOL_SNIPPET);
    }
    if is_path_like(trimmed) {
        return hit(TOOL_RENAME);
    }
    None
}

/// 文本里是否含 `http(s)://` 链接（允许前后有其他文字，例如「分享给你 <链接>」）。
fn has_http_url(lowered: &str) -> bool {
    lowered.contains("http://") || lowered.contains("https://")
}

/// 取最后一个路径分量的扩展名（小写）；无扩展名或扩展名不像扩展名时为 `None`。
fn extension_of(text: &str) -> Option<String> {
    let trimmed = text.trim().trim_matches(['"', '\'']);
    let without_query = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    let last = without_query
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(without_query);
    let (_, extension) = last.rsplit_once('.')?;
    let plausible = !extension.is_empty()
        && extension.len() <= 5
        && extension.chars().all(|ch| ch.is_ascii_alphanumeric());
    plausible.then(|| extension.to_ascii_lowercase())
}

/// 是否像一条命令行。
fn is_command_like(text: &str) -> bool {
    let lowered = text.to_lowercase();
    COMMAND_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
}

/// 是否像一个文件 / 目录路径。
fn is_path_like(text: &str) -> bool {
    if text.starts_with('/')
        || text.starts_with("~/")
        || text.starts_with("./")
        || text.starts_with("../")
    {
        return true;
    }
    // Windows 盘符：`C:\` 或 `C:/`
    let mut chars = text.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(drive), Some(':'), Some('/' | '\\')) if drive.is_ascii_alphabetic()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        TOOL_ARCHIVE, TOOL_HTTP, TOOL_IMAGE, TOOL_NETDISK, TOOL_RENAME, TOOL_SNIPPET, TOOL_VIDEO,
        detect,
    };

    #[test]
    fn netdisk_links_win_over_generic_url() {
        assert_eq!(
            detect("https://pan.baidu.com/s/1abcDEF").map(|m| m.tool_id()),
            Some(TOOL_NETDISK)
        );
        assert_eq!(
            detect("分享给你 https://pan.quark.cn/s/xyz").map(|m| m.tool_id()),
            Some(TOOL_NETDISK)
        );
    }

    #[test]
    fn video_links_are_recognized() {
        assert_eq!(
            detect("https://www.bilibili.com/video/BV1xx411c7mD").map(|m| m.tool_id()),
            Some(TOOL_VIDEO)
        );
        assert_eq!(
            detect("https://youtu.be/dQw4w9WgXcQ").map(|m| m.tool_id()),
            Some(TOOL_VIDEO)
        );
    }

    #[test]
    fn other_links_go_to_http_replay() {
        assert_eq!(
            detect("https://example.com/api/users?page=1").map(|m| m.tool_id()),
            Some(TOOL_HTTP)
        );
    }

    #[test]
    fn files_route_by_extension() {
        assert_eq!(
            detect("/Users/me/Pictures/photo.HEIC").map(|m| m.tool_id()),
            Some(TOOL_IMAGE)
        );
        assert_eq!(
            detect(r"D:\downloads\bundle.tar.gz").map(|m| m.tool_id()),
            Some(TOOL_ARCHIVE)
        );
        assert_eq!(
            detect("~/Downloads/资料.7z").map(|m| m.tool_id()),
            Some(TOOL_ARCHIVE)
        );
    }

    #[test]
    fn command_lines_route_to_snippet() {
        assert_eq!(
            detect("git rebase -i HEAD~3").map(|m| m.tool_id()),
            Some(TOOL_SNIPPET)
        );
        assert_eq!(
            detect("$ docker compose up -d").map(|m| m.tool_id()),
            Some(TOOL_SNIPPET)
        );
    }

    #[test]
    fn plain_directories_route_to_rename() {
        assert_eq!(
            detect("/Volumes/Data/incoming").map(|m| m.tool_id()),
            Some(TOOL_RENAME)
        );
        assert_eq!(
            detect(r"C:\Users\me\Downloads").map(|m| m.tool_id()),
            Some(TOOL_RENAME)
        );
    }

    #[test]
    fn ignores_plain_text_and_blanks() {
        assert!(detect("").is_none());
        assert!(detect("   ").is_none());
        assert!(detect("今天天气不错").is_none());
        // 版本号不是扩展名
        assert!(detect("v1.2.3").is_none());
    }

    #[test]
    fn detail_keeps_original_text() {
        let hit = detect("  https://example.com/a  ").expect("应命中");
        assert_eq!(hit.detail(), "https://example.com/a");
    }
}
