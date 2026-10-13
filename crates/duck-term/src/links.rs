//! 终端里「点得动」的链接识别：`OSC 8` 之外的**裸文本** URL 与路径。
//!
//! # 为什么需要它
//!
//! 点得动的链接有两条来源：
//!
//! 1. 应用用 `OSC 8` **显式**标记的格子（`duck-app` 侧走
//!    [`Session::hyperlink_at`](crate::Session::hyperlink_at)）。`ls --hyperlink`、
//!    `gh`、部分 `gcc` 会这么干，但**是少数**；
//! 2. 屏幕上**只是长得像链接**的一串字符 —— 这一条才是绝大多数：`cargo` 报的文档
//!    地址、`git` 打的远端 URL、编译器给的文件路径，全都不带任何标记。
//!
//! 只做第 1 条时的表现是「明明是一串网址，按 ⌘ 点上去什么都没发生」，而用户的
//! 预期是「屏幕上看着像链接的就该能点」。
//!
//! # 分层
//!
//! 本模块**只做纯字符串识别**：给一段文本，回答「哪几段是链接」。
//! 它不碰网格、不认识坐标 —— 「什么算链接」因此可以被单测逐条钉住。
//! 「字符下标 → 屏幕列号」的换算留在 `session.rs`：宽字符（CJK）占两列，
//! 只有网格那侧知道每个字符落在哪一列。
//!
//! # 识别口径
//!
//! - **URL**：带协议（`http(s)://` / `ftp(s)://` / `file://` / `ssh://` / `git://` /
//!   `mailto:`）或裸 `www.`；
//! - **路径**：`/`、`~/`、`./`、`../` 开头，且必须出现在**词边界**上
//!   （行首、空白之后，或 `([{"'=,;:|` 之后）—— 这一条专门挡掉 `and/or` 这类
//!   正文里的斜杠，否则一篇文章里到处都是「路径」；
//! - **尾部标点**按惯例修掉（`见 https://x.com.` 的句号不属于 URL），
//!   括号则**配对时保留**（`…/wiki/Foo_(bar)` 要能整个点开）。

/// 识别出来的一个链接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSpan {
    /// 起始**字符**下标（含）。不是字节下标 —— 终端按字符定位，且上层要把它换成列号。
    pub start: usize,
    /// 结束字符下标（不含）。
    pub end: usize,
    /// 点开时拿什么去开。
    pub target: LinkTarget,
}

/// 链接的去向。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// 一个 URL。可能是 `https://…`，也可能是没写协议的 `www.…`
    ///（上层补协议时按「有 `://` 或 `mailto:` 就原样、否则补 `https://`」处理）。
    Url(String),
    /// 一个文件路径（绝对路径、`~/`、`./`、`../` 开头）。
    Path(String),
}

impl LinkTarget {
    /// 交给系统打开时用的字符串。
    ///
    /// URL 补协议（`www.x.com` → `https://www.x.com`），路径原样 ——
    /// `open` / `xdg-open` 认识裸路径，替它编一个 `file://` 反而会因为转义
    /// 规则不一致（空格、`#`、非 ASCII）而打不开。
    #[must_use]
    pub fn open_text(&self) -> String {
        match self {
            Self::Path(path) => path.clone(),
            Self::Url(url) => {
                if url.contains("://") || url.starts_with("mailto:") {
                    url.clone()
                } else {
                    format!("https://{url}")
                }
            }
        }
    }
}

/// 带协议的 URL 前缀。`mailto:` 不带 `//`，其余都带。
const URL_SCHEMES: [&str; 8] = [
    "https://", "http://", "ftps://", "ftp://", "file://", "ssh://", "git://", "mailto:",
];

/// 扫描一段文本，按出现顺序返回其中的链接。
///
/// 重叠的候选**只保留更早出现的那一条**（同一位置的 URL 优先于路径：
/// `https://x.com/a` 里那个 `/a` 是 URL 的一部分，不该再报成一个路径）。
#[must_use]
pub fn scan(text: &str) -> Vec<LinkSpan> {
    let chars: Vec<char> = text.chars().collect();
    let mut found: Vec<LinkSpan> = Vec::new();
    for (index, _) in chars.iter().enumerate() {
        if let Some(span) = url_at(&chars, index) {
            found.push(span);
        }
    }
    for (index, _) in chars.iter().enumerate() {
        if let Some(span) = path_at(&chars, index) {
            found.push(span);
        }
    }
    // 起始位置靠前的先落位；同一位置取更长的那条（`http://` 胜过 `//`）。
    found.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut kept: Vec<LinkSpan> = Vec::new();
    for span in found {
        if kept.last().is_some_and(|last| span.start < last.end) {
            continue;
        }
        kept.push(span);
    }
    kept
}

/// `index` 处是不是一个 URL 的开头。
fn url_at(chars: &[char], index: usize) -> Option<LinkSpan> {
    if !at_url_boundary(chars, index) {
        return None;
    }
    let rest = &chars[index..];
    let (prefix_len, _) = URL_SCHEMES
        .iter()
        .find(|scheme| starts_with(rest, scheme))
        .map(|scheme| (scheme.chars().count(), ()))
        .or_else(|| starts_with(rest, "www.").then_some((4, ())))?;
    // 协议后面必须真跟着东西：`https://` 单独一行不算链接。
    let end = consume_link_chars(chars, index + prefix_len, index);
    if end <= index + prefix_len {
        return None;
    }
    // 裸 `www.` 必须带一个点结尾的域名段（`www.` 后面直接空白的不算）。
    if prefix_len == 4 && !rest[4..end - index].contains(&'.') {
        return None;
    }
    let text: String = chars[index..end].iter().collect();
    // 只有协议没有主机的（`https://`）不给。
    if text.ends_with("://") {
        return None;
    }
    Some(LinkSpan {
        start: index,
        end,
        target: LinkTarget::Url(text),
    })
}

/// `index` 处是不是一个路径的开头。
fn path_at(chars: &[char], index: usize) -> Option<LinkSpan> {
    if !at_boundary(chars, index) {
        return None;
    }
    let rest = &chars[index..];
    let prefix_len = if starts_with(rest, "~/") || starts_with(rest, "./") {
        2
    } else if starts_with(rest, "../") {
        3
    } else if rest.first() == Some(&'/') {
        // `//` 不是路径的常见写法（正文里出现得更多），不当链接。
        if rest.get(1) == Some(&'/') {
            return None;
        }
        1
    } else {
        return None;
    };
    if !rest.get(prefix_len).copied().is_some_and(is_link_char) {
        return None;
    }
    let end = consume_link_chars(chars, index + prefix_len, index);
    let text: String = chars[index..end].iter().collect();
    Some(LinkSpan {
        start: index,
        end,
        target: LinkTarget::Path(text),
    })
}

/// 从 `from` 起一直吃到链接结束，返回**结束下标（不含）**，并修掉尾部标点。
///
/// `start` 是整条候选的起点，只用于配对括号时限定范围。
fn consume_link_chars(chars: &[char], from: usize, start: usize) -> usize {
    let mut end = from;
    while end < chars.len() && is_link_char(chars[end]) {
        end += 1;
    }
    while end > start {
        let last = chars[end - 1];
        let drop = match last {
            // 句读：出现在末尾几乎总是正文的标点，不是链接的一部分。
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            // 括号：只有**不配对**时才丢。`…/Foo_(bar)` 要能整个点开。
            ')' => count(chars, start, end, '(') < count(chars, start, end, ')'),
            ']' => count(chars, start, end, '[') < count(chars, start, end, ']'),
            '}' => count(chars, start, end, '{') < count(chars, start, end, '}'),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= 1;
    }
    end
}

/// `chars[start..end]` 里某个字符出现了几次。
fn count(chars: &[char], start: usize, end: usize, needle: char) -> usize {
    chars[start..end].iter().filter(|c| **c == needle).count()
}

/// 出现在链接内部时算「还在链接里」的字符。
///
/// 判据是**排除法**：除了空白与几个明确的分隔符，其余都算 —— 终端输出里的路径与
/// URL 会包含 `?` `&` `=` `#` `%` `+` `@` `~` 这些在别处都算标点的字符，
/// 逐个白名单列反而更容易漏。中文标点单独挡掉（`。，、）` 属于正文）。
fn is_link_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '"' | '\''
                | '`'
                | '<'
                | '>'
                | '|'
                | '\\'
                | '^'
                | '（'
                | '）'
                | '「'
                | '」'
                | '【'
                | '】'
                | '《'
                | '》'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '，'
                | '。'
                | '、'
                | '；'
                | '：'
                | '？'
                | '！'
        )
}

/// `index` 处的字符是否落在**词边界**上。
///
/// 这条规则是「别把 `and/or` 当成路径」的全部依据：斜杠前面跟着字母数字时，
/// 它就是正文里的分隔符，不是路径开头。
fn at_boundary(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = chars[index - 1];
    previous.is_whitespace()
        || matches!(
            previous,
            '(' | '[' | '{' | '<' | '"' | '\'' | '`' | '=' | ',' | ';' | '|' | ':' | '「' | '（'
        )
}

/// URL 的边界判定：比 [`at_boundary`] **松一档**。
///
/// `https://` 本身就是极强的信号 —— 正文里不会有「一个字母紧挨着 `https://`」
/// 这种情况。而中文排版的习惯是**不空格**（`文档见https://例子.com`），
/// 若照搬路径那套边界规则，中文提示里的网址会整条漏掉。
///
/// 因此这里只挡「前一个字符是 ASCII 字母/数字」：那才是 `foohttps://` 这类
/// 拼接串（`and/or` 的同类）该被排除的情形。
fn at_url_boundary(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    at_boundary(chars, index) || !chars[index - 1].is_ascii_alphanumeric()
}

/// `chars` 是否以 `pattern` 开头。
///
/// 用 `enumerate` + `get` 而不是 `zip`：`zip` 在 `chars` 比 `pattern` 短时会**提前结束**
/// 并让 `all` 返回 `true`，把「不够长」误判成「前缀相同」。
fn starts_with(chars: &[char], pattern: &str) -> bool {
    pattern
        .chars()
        .enumerate()
        .all(|(offset, expected)| chars.get(offset) == Some(&expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只取命中的文本，断言更直观。
    fn hits(text: &str) -> Vec<(String, bool)> {
        scan(text)
            .into_iter()
            .map(|span| {
                let raw: String = text
                    .chars()
                    .skip(span.start)
                    .take(span.end - span.start)
                    .collect();
                let is_url = matches!(span.target, LinkTarget::Url(_));
                (raw, is_url)
            })
            .collect()
    }

    #[test]
    fn a_scheme_url_is_found_anywhere_in_the_line() {
        assert_eq!(
            hits("see https://example.com/a?b=1&c=2 for details"),
            vec![("https://example.com/a?b=1&c=2".to_owned(), true)]
        );
    }

    #[test]
    fn sentence_punctuation_is_not_part_of_the_link() {
        // 尾随句号 / 逗号 / 冒号都是正文的标点。
        assert_eq!(
            hits("见 https://example.com/doc。"),
            vec![("https://example.com/doc".to_owned(), true)]
        );
        assert_eq!(
            hits("open http://a.io/x, then"),
            vec![("http://a.io/x".to_owned(), true)]
        );
    }

    #[test]
    fn balanced_parentheses_stay_inside_the_link() {
        // 维基式的 `Foo_(bar)`：括号配平，必须整段可点。
        assert_eq!(
            hits("https://en.wikipedia.org/wiki/Foo_(bar) is the page"),
            vec![("https://en.wikipedia.org/wiki/Foo_(bar)".to_owned(), true)]
        );
        // 不配平时那是正文的括号，丢掉。
        assert_eq!(
            hits("(see https://example.com/a)"),
            vec![("https://example.com/a".to_owned(), true)]
        );
    }

    #[test]
    fn a_bare_www_host_is_a_url() {
        assert_eq!(
            hits("docs at www.example.com/index"),
            vec![("www.example.com/index".to_owned(), true)]
        );
        // 光一个 `www.` 不成链接。
        assert!(hits("www. ").is_empty());
    }

    #[test]
    fn mailto_and_other_schemes_are_recognised() {
        assert_eq!(
            hits("contact mailto:me@example.com"),
            vec![("mailto:me@example.com".to_owned(), true)]
        );
        assert_eq!(
            hits("file:///Users/foo/a.txt"),
            vec![("file:///Users/foo/a.txt".to_owned(), true)]
        );
        assert_eq!(
            hits("clone ssh://git@host/x.git now"),
            vec![("ssh://git@host/x.git".to_owned(), true)]
        );
    }

    #[test]
    fn absolute_and_home_paths_are_found() {
        assert_eq!(
            hits("error at /Users/foo/code/main.rs:12:5"),
            vec![("/Users/foo/code/main.rs:12:5".to_owned(), false)]
        );
        assert_eq!(
            hits("run ~/bin/deploy.sh now"),
            vec![("~/bin/deploy.sh".to_owned(), false)]
        );
        assert_eq!(
            hits("./target/debug/duck --help"),
            vec![("./target/debug/duck".to_owned(), false)]
        );
        assert_eq!(
            hits("../sibling/file.rs"),
            vec![("../sibling/file.rs".to_owned(), false)]
        );
    }

    #[test]
    fn a_slash_in_prose_is_not_a_path() {
        // 这条规则的价值全在这里：正文里的斜杠（and/or、TCP/IP、3/4）不该变成链接。
        assert!(hits("and/or TCP/IP 3/4").is_empty());
        assert!(hits("a / b").is_empty());
        // 前面是字母数字 = 不是词边界。
        assert!(hits("see also foo/bar").is_empty());
        // 词边界之后才认。
        assert_eq!(
            hits("see also /usr/local"),
            vec![("/usr/local".to_owned(), false)]
        );
    }

    #[test]
    fn a_path_inside_a_url_is_not_reported_twice() {
        // `/a/b` 落在 URL 的区间里，不该再报一条路径。
        assert_eq!(
            hits("https://example.com/a/b"),
            vec![("https://example.com/a/b".to_owned(), true)]
        );
    }

    #[test]
    fn cjk_and_full_width_punctuation_terminate_the_link() {
        assert_eq!(
            hits("地址是https://例子.com/路径（测试）"),
            vec![("https://例子.com/路径".to_owned(), true)]
        );
    }

    #[test]
    fn quote_and_backtick_delimit_the_link() {
        assert_eq!(
            hits("echo 'https://a.io/x' | pbcopy"),
            vec![("https://a.io/x".to_owned(), true)]
        );
        assert_eq!(hits("`/etc/hosts`"), vec![("/etc/hosts".to_owned(), false)]);
    }

    #[test]
    fn an_empty_or_scheme_only_text_yields_nothing() {
        assert!(hits("").is_empty());
        assert!(hits("https://").is_empty());
        assert!(hits("plain text without any link").is_empty());
        assert!(hits("///").is_empty());
    }

    #[test]
    fn several_links_on_one_line_keep_their_order() {
        let found = hits("a /tmp/x then https://b.io/y and ~/z");
        assert_eq!(
            found,
            vec![
                ("/tmp/x".to_owned(), false),
                ("https://b.io/y".to_owned(), true),
                ("~/z".to_owned(), false),
            ]
        );
    }

    #[test]
    fn open_text_adds_a_scheme_only_when_missing() {
        assert_eq!(
            LinkTarget::Url("www.x.com".to_owned()).open_text(),
            "https://www.x.com"
        );
        assert_eq!(
            LinkTarget::Url("https://x.com".to_owned()).open_text(),
            "https://x.com"
        );
        assert_eq!(
            LinkTarget::Url("mailto:a@b.c".to_owned()).open_text(),
            "mailto:a@b.c"
        );
        assert_eq!(
            LinkTarget::Path("/tmp/a b".to_owned()).open_text(),
            "/tmp/a b",
            "路径原样交给系统：自己编 file:// 会因为转义不一致而打不开"
        );
    }

    #[test]
    fn character_offsets_are_characters_not_bytes() {
        // 前面有中文时，start 必须是**字符**下标（上层要拿它查列号表）。
        let text = "中文 https://a.io/x";
        let span = scan(text).into_iter().next().expect("应命中一条");
        assert_eq!(span.start, 3, "「中文 」是 3 个字符");
        let raw: String = text
            .chars()
            .skip(span.start)
            .take(span.end - span.start)
            .collect();
        assert_eq!(raw, "https://a.io/x");
    }
}
