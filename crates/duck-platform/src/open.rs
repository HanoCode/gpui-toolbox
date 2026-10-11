//! 外部唤起请求：把「命令行参数 / 深链接 / 文件路径」归一成同一种类型。
//!
//! 三个来源最终汇到同一条消费路径上，所以必须先在**入口**收敛成一种形状：
//!
//! - 命令行：`duck /path/to/file`、`duck duck://tool/http.replay`；
//! - 深链接：macOS 由 `NSApplicationDelegate` 送来，Windows 走注册表的 `duck` 方案唤起；
//! - 单实例转发：第二个实例把自己这轮的启动参数经 [`crate::ipc`] 交给正在运行的那个。
//!
//! 本模块**不依赖 gpui**，解析全是字符串与路径判断：深链接里「`tool/<id>` 是什么意思」
//! 属于应用语义，由上层解释，这里只管「这是一条深链接」。
//!
//! # 为什么 `parse` 不碰文件系统
//!
//! 「这条参数是不是一个真实存在的文件」要靠 `metadata` 回答，一旦写进 `parse`，
//! 它就不再是纯函数（测试得先造临时文件，还得处理平台差异）。所以拆成两步：
//! [`parse`] 只做形状判断，[`from_args`] 在真正的启动路径上补一次存在性检查。

use std::ffi::OsStr;
use std::path::PathBuf;

/// 深链接方案名。
///
/// 与打包脚本写入 `Info.plist` 的 `CFBundleURLSchemes`、Windows 注册表的
/// `HKCU\Software\Classes\duck` 必须是同一个串——改这里就得同步改那两处。
pub const URL_SCHEME: &str = "duck";

/// 深链接前缀（[`URL_SCHEME`] 的冒号形式）。
///
/// 手写第二遍而不是拼接：`concat!` 只吃字面量，认不出常量。两处不一致编译不报错，
/// 但 `is_deeplink` 的单测会立刻挂——这就是它存在的意义。
const SCHEME_PREFIX: &str = "duck:";

/// 一次外部唤起携带的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenRequest {
    /// `duck://...` 深链接；原样保留，解释交给上层。
    Url(String),
    /// 文件 / 目录路径。
    Path(PathBuf),
}

impl OpenRequest {
    /// 解析**一条**参数；不属于任何一类时返回 `None`。
    ///
    /// 规则只有两条：以 `-` 开头的是选项、不是内容；`duck:` 开头的是深链接，
    /// 其余一律按路径候选处理。
    ///
    /// 刻意**不**在这里判断「像不像路径」（是否以 `/` 开头、有没有盘符）：
    /// 那样会漏掉相对路径 `duck 报告.pdf`。真伪由 [`from_args`] 的存在性检查兜住，
    /// 效果等价而规则少一条。本函数也不碰文件系统，因此每条分支都能在普通单测里断言。
    #[must_use]
    pub fn parse(argument: &str) -> Option<Self> {
        let trimmed = argument.trim();
        // 空串与选项都不是「要打开的东西」。`--help` 之类在这里丢掉，
        // 由启动流程自己去识别选项——本模块不拥有命令行语法。
        if trimmed.is_empty() || trimmed.starts_with('-') {
            return None;
        }
        if is_deeplink(trimmed) {
            return Some(Self::Url(trimmed.to_owned()));
        }
        Some(Self::Path(PathBuf::from(trimmed)))
    }

    /// 深链接原文；不是深链接则 `None`。
    #[must_use]
    pub fn as_url(&self) -> Option<&str> {
        match self {
            Self::Url(url) => Some(url),
            Self::Path(_) => None,
        }
    }

    /// 路径；不是路径则 `None`。
    #[must_use]
    pub fn as_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Path(path) => Some(path),
            Self::Url(_) => None,
        }
    }

    /// 从命令行参数解析出全部请求。
    ///
    /// **入参要含程序名**（`std::env::args_os()` 的原样输出）：函数自己跳过第一个，
    /// 调用方不必记这个约定。Windows 上 `argv[0]` 同样是可执行文件路径，规则一致。
    ///
    /// 不存在的路径会被丢掉并记一条 debug 日志——打开一个盘上没有的文件没有意义，
    /// 静默丢弃比让用户面对一个「窗口弹出来了但什么都没发生」的结果好。
    #[must_use]
    pub fn from_args<I, S>(args: I) -> Vec<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut requests = Vec::new();
        for argument in args.into_iter().skip(1) {
            let Some(text) = argument.as_ref().to_str() else {
                tracing::debug!("skipping a launch argument that is not valid UTF-8");
                continue;
            };
            match Self::parse(text) {
                // 深链接不做存在性检查：它不是一个路径。
                Some(request @ Self::Url(_)) => requests.push(request),
                Some(Self::Path(path)) => {
                    if path.exists() {
                        requests.push(Self::Path(path));
                    } else {
                        tracing::debug!(
                            path = %path.display(),
                            "skipping a launch argument that is not on disk"
                        );
                    }
                }
                None => {}
            }
        }
        requests
    }
}

/// 是否是我们自己的深链接。
///
/// 大小写不敏感：Windows 的注册表唤起会把方案名按用户输入的大小写原样传来，
/// 而 `Duck://` 与 `duck://` 对用户是同一件事。只认 `duck:` 后面紧跟空串或 `/`，
/// 这样 `duck:foo`（漏了两个斜杠）不会被误收——它不是合法 URL。
#[must_use]
pub fn is_deeplink(text: &str) -> bool {
    // `get` 而不是 `split_at`：参数里可能有多字节字符，按字节切会 panic。
    let Some(head) = text.get(..SCHEME_PREFIX.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(SCHEME_PREFIX) {
        return false;
    }
    let rest = &text[SCHEME_PREFIX.len()..];
    rest.is_empty() || rest.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::{OpenRequest, URL_SCHEME, is_deeplink};
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    #[test]
    fn the_scheme_prefix_matches_the_published_scheme() {
        // 两个常量手写了两遍，这条守的是它们没分叉。
        assert!(is_deeplink(&format!("{URL_SCHEME}://tool/x")));
        assert!(is_deeplink(&format!("{URL_SCHEME}:")));
    }

    #[test]
    fn deeplinks_are_recognised_regardless_of_case() {
        assert!(is_deeplink("duck://tool/http.replay"));
        assert!(is_deeplink("Duck://tool/http.replay"));
        assert!(is_deeplink("DUCK://"));
    }

    #[test]
    fn a_missing_double_slash_is_not_a_deeplink() {
        // `duck:foo` 不是合法 URL；当成路径候选，存在性检查会把它丢掉。
        assert!(!is_deeplink("duck:foo"));
        assert!(!is_deeplink("duckhttp://x"));
        assert!(!is_deeplink(""));
    }

    #[test]
    fn non_ascii_input_does_not_panic_the_prefix_check() {
        // 按字节切前缀时最容易踩的坑：多字节字符被切成两半。
        assert!(!is_deeplink("鸭子://tool/x"));
        assert!(!is_deeplink("du"));
        assert!(!is_deeplink("duck"));
    }

    #[test]
    fn parse_keeps_deeplinks_verbatim() {
        assert_eq!(
            OpenRequest::parse("  duck://tool/http.replay  "),
            Some(OpenRequest::Url("duck://tool/http.replay".to_owned()))
        );
    }

    #[test]
    fn parse_accepts_relative_paths() {
        // 不判断「像不像路径」的收益：相对路径也能进来，存在性检查在后面兜底。
        assert_eq!(
            OpenRequest::parse("报告.pdf"),
            Some(OpenRequest::Path(PathBuf::from("报告.pdf")))
        );
        assert_eq!(
            OpenRequest::parse("/tmp/a b/c.txt"),
            Some(OpenRequest::Path(PathBuf::from("/tmp/a b/c.txt")))
        );
    }

    #[test]
    fn parse_drops_options_and_blanks() {
        assert_eq!(OpenRequest::parse(""), None);
        assert_eq!(OpenRequest::parse("   "), None);
        assert_eq!(OpenRequest::parse("--help"), None);
        assert_eq!(OpenRequest::parse("-v"), None);
    }

    #[test]
    fn from_args_skips_the_program_name() {
        // 头一个参数是程序名，哪怕它看起来像个路径也不能收。
        let requests =
            OpenRequest::from_args([OsStr::new("duck"), OsStr::new("duck://tool/image.convert")]);
        assert_eq!(
            requests,
            vec![OpenRequest::Url("duck://tool/image.convert".to_owned())]
        );
    }

    #[test]
    fn from_args_keeps_only_paths_that_exist() {
        let dir = std::env::temp_dir();
        let real = dir.join(format!("duck-open-{}.txt", std::process::id()));
        std::fs::write(&real, b"x").expect("should be able to write a temp file");
        let missing = dir.join(format!("duck-open-missing-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&missing);

        let requests =
            OpenRequest::from_args([OsStr::new("duck"), real.as_os_str(), missing.as_os_str()]);
        assert_eq!(requests, vec![OpenRequest::Path(real.clone())]);

        let _ = std::fs::remove_file(&real);
    }

    #[test]
    fn accessors_only_answer_for_their_own_variant() {
        let url = OpenRequest::Url("duck://tool/x".to_owned());
        assert_eq!(url.as_url(), Some("duck://tool/x"));
        assert!(url.as_path().is_none());

        let path = OpenRequest::Path(PathBuf::from("/tmp/x"));
        assert_eq!(path.as_path(), Some(Path::new("/tmp/x")));
        assert!(path.as_url().is_none());
    }
}
