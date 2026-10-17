//! 深链接与外部唤起：把 `duck://...` 和「被打开的文件」解释成应用里的动作。
//!
//! 三种外部输入最终汇到同一条路上（见 [`dispatch`]）：
//!
//! | 输入 | 落点 |
//! | --- | --- |
//! | `duck://tool/<id>` | 切到工具箱并打开该工具 |
//! | `file://` URL、命令行路径、拖入窗口的文件 | 工具箱的智能粘贴横幅（用户确认后才动手） |
//! | `.ducktool` 文件 | 插件中心（安装要走索引里的摘要校验，没代劳的余地） |
//!
//! # 分层
//!
//! [`interpret`] 是**纯函数**：给它一个 URL，回答「这是什么」。真正的动作在
//! [`dispatch`] 里，只有它需要 `App`。这样 URL 语法（大小写、转义、尾斜杠）的每条
//! 规则都能在普通单测里钉死，不必为了测一句「`Duck://` 也算」去搭一个窗口。

use std::path::{Path, PathBuf};

use duck_platform::OpenRequest;
use duck_theme::{active_theme, t, tr, trf};
use duck_tools::paste::detect;
use duck_tools::{PasteInbox, PendingPaste, ToolRegistry};
use gpui::{App, SharedString, Window};

use crate::app;
use crate::pages::{TOOLBOX_ROUTE, open_plugin_center};
use crate::update;

/// 插件包扩展名（不带点）。
///
/// 与打包脚本 / 文档里的说明必须一致；`.ducktool` 只表示「这是一个插件包」，
/// 安装本身仍要走索引校验（见 [`open_file`]）。
const PLUGIN_PACKAGE_EXTENSION: &str = "ducktool";

/// 一条深链接解释出来的意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deeplink {
    /// `duck://tool/<id>`：跳到工具箱并打开该工具。
    Tool(String),
    /// 指向本机文件：与把该路径拖进窗口同义。
    File(PathBuf),
    /// 方案名认得，内容不认识。
    Unknown,
}

/// 解释一条 URL。
///
/// 认得出的方案只有两个：我们自己注册的 `duck`，以及系统的 `file`。
/// 别的（`http://…` 之类）返回 [`Deeplink::Unknown`]——应用没有「浏览器」这个身份，
/// 把一个网址打开成工具箱只会让人困惑。
#[must_use]
pub fn interpret(url: &str) -> Deeplink {
    let trimmed = url.trim();
    if let Some(rest) = strip_scheme(trimmed, "duck") {
        return interpret_duck(rest);
    }
    if let Some(path) = file_url_path(trimmed) {
        return Deeplink::File(path);
    }
    Deeplink::Unknown
}

/// `duck:` 之后的部分 → 意图。
fn interpret_duck(rest: &str) -> Deeplink {
    // `duck://tool/x` 是标准写法；`duck:tool/x` 也收——手工拼 URL 的人常漏两个斜杠，
    // 而两者想表达的是同一件事，没有理由为这点差别报错。
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, path),
        None => (rest, ""),
    };
    if !host.eq_ignore_ascii_case("tool") {
        return Deeplink::Unknown;
    }
    // 尾斜杠（`duck://tool/x/`）是手滑，不是另一个工具。
    let id = path.trim_end_matches('/');
    // 工具 id 里不含 `/`；`duck://tool/a/b` 更像是打错了，不要拿前半段去猜。
    if id.is_empty() || id.contains('/') {
        return Deeplink::Unknown;
    }
    Deeplink::Tool(id.to_owned())
}

/// `file://` URL → 路径。
///
/// 只处理本机路径的形式（`file:///Users/me/a.txt`）。**不处理** `file://localhost/…`
/// 这类带主机的写法，也不处理 Windows 的 `file:///C:/…`——前者 macOS 不会发，
/// 后者 Windows 的文件关联走的是注册表 `%1` 参数、同样不会发 URL。
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = strip_scheme(url, "file")?;
    // 必须有 `//`：`file:/x` 不是合法 URL，别把它当路径放进来。
    let rest = rest.strip_prefix("//")?;
    if rest.is_empty() {
        return None;
    }
    Some(PathBuf::from(percent_decode(rest)))
}

/// 去掉 `scheme:` 前缀，返回后面的部分（大小写不敏感）。
///
/// 用 `get` 而不是按字节切：URL 里可能有非 ASCII 字符，切在字符中间会 panic。
fn strip_scheme<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let head = url.get(..scheme.len())?;
    if !head.eq_ignore_ascii_case(scheme) {
        return None;
    }
    url.get(scheme.len()..)?.strip_prefix(':')
}

/// 解 `%XX` 转义。
///
/// 手写而不引 `url` crate：整条链路上只有这一处需要转义，而 macOS 送来的
/// `absoluteString` 一定会把空格、中文等转成 `%XX`，不解的话路径根本对不上盘上的文件。
///
/// 非 UTF-8 的结果会退化（`from_utf8_lossy` 换成 `U+FFFD`）：这种路径在 macOS 上
/// 打不开也属正常，不值得为它引入一套字节路径处理。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// 把一批外部唤起请求落成应用动作。
///
/// 空表是常态（用户直接双击图标启动），调用方不必先判断。
///
/// 这条路径上没有窗口：`on_open_urls` 的回调拿不到 `App`，而命令行唤起时主窗可能
/// 还没建。所以提示走 [`update::on_notify_window`]——找得到窗口就投，找不到就只少
/// 一句提示，动作本身照做。
pub fn dispatch(requests: &[OpenRequest], cx: &mut App) {
    if requests.is_empty() {
        return;
    }
    tracing::info!(count = requests.len(), "handling external open requests");
    for request in requests {
        match request {
            OpenRequest::Url(url) => match interpret(url) {
                Deeplink::Tool(id) => open_tool(&id, cx),
                Deeplink::File(path) => open_file(None, &path, cx),
                Deeplink::Unknown => {
                    tracing::warn!(url, "ignoring an unrecognised deep link");
                }
            },
            OpenRequest::Path(path) => open_file(None, path, cx),
        }
    }
}

/// 文件被拖进窗口；`ShellView::on_files_dropped` 的落点。
///
/// 只处理第一个文件：智能粘贴的一条负载对应一次确认，横幅的粒度也是单个。
/// 多出来的会记一条日志——静默丢掉其余的会让人以为全都处理过了。
pub fn handle_dropped_files(paths: &[PathBuf], window: &mut Window, cx: &mut App) {
    let Some(first) = paths.first() else {
        return;
    };
    if paths.len() > 1 {
        tracing::info!(
            count = paths.len(),
            "more than one file was dropped; only the first one is handled"
        );
    }
    // 拖放的提示直接投在文件落进来的那个窗口上，不必再找。
    open_file(Some(window), first, cx);
}

/// 跳到一个工具。
///
/// 工具没注册（插件被卸了、链接手写错了）就明说一句——静默什么都不发生
/// 会让人以为应用卡住了。
fn open_tool(id: &str, cx: &mut App) {
    if app::open_tool(cx, id) {
        return;
    }
    tracing::warn!(
        tool = id,
        "the deep link points at a tool that is not available"
    );
    update::on_notify_window(cx, |window, cx| {
        duck_components::notify::warning(
            tr(cx, "没有找到这个工具，可能是插件尚未安装。"),
            window,
            cx,
        );
    });
}

/// 打开一个本机文件。
///
/// 与顶栏的「识别粘贴」按钮、以及把文件拖进窗口是**同一条路**：识别交给
/// `duck_tools::paste::detect`，动手交给用户在横幅上点的那一下。
/// 无论深链接还是拖放，都不该越过用户直接执行工具。
///
/// `window` 为 `None` 时（深链接 / 命令行唤起）提示改走
/// [`update::on_notify_window`]。
fn open_file(window: Option<&mut Window>, path: &Path, cx: &mut App) {
    if is_plugin_package(path) {
        // 安装插件要先比对索引里的字节数与摘要，凭空一个文件装不了。
        // 所以这里只把人送到插件中心，剩下的交给那条已经跑通的流程。
        tracing::info!(
            path = %path.display(),
            "a plugin package was opened; directing to the plugin centre"
        );
        open_plugin_center(cx);
        notice(window, tr(cx, "插件包请从插件中心安装。").into(), cx);
        return;
    }

    let text = path.to_string_lossy();
    let hit = detect(&text);
    if let Some(found) = &hit {
        PasteInbox::put(cx, PendingPaste::new(found.tool_id(), found.detail()));
    }
    // 认不出来也把工具箱打开：用户的心智是「我给了它一个文件」，
    // 至少该看到文件被接住了，而不是窗口毫无反应。
    let opened = app::navigate_to(cx, TOOLBOX_ROUTE);
    tracing::info!(path = %path.display(), recognized = hit.is_some(), opened, "opened a file");

    let Some(found) = hit else {
        return;
    };
    let Some(name) = ToolRegistry::get(cx, found.tool_id()).map(|tool| tool.meta().name()) else {
        return;
    };
    let lang = active_theme(cx).lang();
    notice(
        window,
        trf(cx, "已识别为「{}」，到工具箱确认处理", &[t(lang, name)]).into(),
        cx,
    );
}

/// 投一条提示：有窗口就投给它，没有就找「最该收到提示的那一个」。
///
/// 提示投错窗口（投到被盖住的那个）看起来就像「没反应」，所以没窗口时不能随便挑一个
/// ——`on_notify_window` 里那三档顺序是有讲究的，见它的注释。
fn notice(window: Option<&mut Window>, message: SharedString, cx: &mut App) {
    match window {
        Some(window) => duck_components::notify::success(message, window, cx),
        None => {
            update::on_notify_window(cx, move |window, cx| {
                duck_components::notify::success(message, window, cx);
            });
        }
    }
}

/// 文件是不是一个插件包。
fn is_plugin_package(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(PLUGIN_PACKAGE_EXTENSION))
}

#[cfg(test)]
mod tests {
    use super::{Deeplink, interpret, is_plugin_package, percent_decode};
    use std::path::{Path, PathBuf};

    #[test]
    fn a_tool_link_carries_the_tool_id() {
        assert_eq!(
            interpret("duck://tool/http.replay"),
            Deeplink::Tool("http.replay".to_owned())
        );
    }

    #[test]
    fn scheme_and_host_are_case_insensitive() {
        // Windows 的注册表唤起会把方案名按用户输入原样传来。
        assert_eq!(
            interpret("Duck://TOOL/image.convert"),
            Deeplink::Tool("image.convert".to_owned())
        );
    }

    #[test]
    fn a_missing_double_slash_is_tolerated() {
        // 手写 URL 时常见的漏写；语义没歧义，就收下。
        assert_eq!(
            interpret("duck:tool/archive.unpack"),
            Deeplink::Tool("archive.unpack".to_owned())
        );
    }

    #[test]
    fn a_trailing_slash_is_not_part_of_the_id() {
        assert_eq!(
            interpret("duck://tool/video.parse/"),
            Deeplink::Tool("video.parse".to_owned())
        );
    }

    #[test]
    fn a_nested_path_is_refused_rather_than_guessed() {
        // `duck://tool/a/b` 更像打错了；拿前半段去猜会打开一个用户没要的工具。
        assert_eq!(interpret("duck://tool/a/b"), Deeplink::Unknown);
        assert_eq!(interpret("duck://tool/"), Deeplink::Unknown);
        assert_eq!(interpret("duck://tool"), Deeplink::Unknown);
    }

    #[test]
    fn other_hosts_and_schemes_are_unknown() {
        assert_eq!(interpret("duck://settings/appearance"), Deeplink::Unknown);
        // 应用没有「浏览器」这个身份：把网址当工具打开只会让人困惑。
        assert_eq!(interpret("https://example.com/"), Deeplink::Unknown);
        assert_eq!(interpret("notduck://tool/x"), Deeplink::Unknown);
        assert_eq!(interpret(""), Deeplink::Unknown);
    }

    #[test]
    fn file_urls_become_paths() {
        assert_eq!(
            interpret("file:///Users/me/Pictures/a.png"),
            Deeplink::File(PathBuf::from("/Users/me/Pictures/a.png"))
        );
    }

    #[test]
    fn percent_escapes_in_file_urls_are_decoded() {
        // macOS 送来的 `absoluteString` 一定会转义，不解就对不上盘上的文件。
        assert_eq!(
            interpret("file:///Users/me/My%20Pictures/%E6%8A%A5%E5%91%8A.pdf"),
            Deeplink::File(PathBuf::from("/Users/me/My Pictures/报告.pdf"))
        );
    }

    #[test]
    fn a_file_url_without_a_path_is_unknown() {
        assert_eq!(interpret("file://"), Deeplink::Unknown);
        // 少了 `//` 不是合法 URL，别当作路径收下。
        assert_eq!(interpret("file:/tmp/a.txt"), Deeplink::Unknown);
    }

    #[test]
    fn percent_decoding_keeps_what_it_cannot_decode() {
        assert_eq!(percent_decode("no-escapes"), "no-escapes");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        // 不完整的转义原样保留，而不是吃掉后面两个字符。
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn percent_decoding_handles_non_ascii_input_without_panicking() {
        assert_eq!(percent_decode("鸭子"), "鸭子");
        assert_eq!(percent_decode("%E9%B8%AD"), "鸭");
    }

    #[test]
    fn plugin_packages_are_recognised_case_insensitively() {
        assert!(is_plugin_package(Path::new("/tmp/a.ducktool")));
        assert!(is_plugin_package(Path::new("/tmp/A.DUCKTOOL")));
        assert!(!is_plugin_package(Path::new("/tmp/a.png")));
        assert!(!is_plugin_package(Path::new("/tmp/ducktool")));
    }
}
