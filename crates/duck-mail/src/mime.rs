//! MIME 解码：把原始 RFC822 字节变成模型层要的字段。
//!
//! # 为什么统一走 `mail-parser`
//!
//! 列表与详情用**同一个解析器**：列表拉 `BODY.PEEK[HEADER]`（只有头部，几百字节），
//! 详情拉整封。两条路径共用 [`parse_headers`] 的地址 / 日期 / 主题逻辑，不会出现
//! 「列表里的发件人和详情里的不是同一个人」这种由两套解析器造成的偏差。
//!
//! # 必须开 `full_encoding`
//!
//! 见 `Cargo.toml` 注释：没有它，GB2312 / GBK / Big5 / Shift-JIS 会解成 U+FFFD。
//! 中文邮件用户对这个的容忍度是零。
//!
//! # 纯文本与 HTML 互为排他
//!
//! [`parse`] 取正文时会**再按部件自己声明的 MIME 类型判一次**（[`is_html_part`]），
//! 而不是直接信 `text_bodies()` / `html_bodies()` 的归属。原因是 `mail-parser`
//! 对「整个报文连 `Content-Type` 都没有」的邮件（老 MUA、或被网关剥掉头的邮件）
//! 会把同一个部件**同时**列进两个迭代器。照单全收的后果是把纯文本当 HTML 渲染：
//! HTML 折叠空白 → 换行全丢；正文里出现的 `<` 会被当标记解释。回归测试
//! `headerless_content_type_is_plain_text_not_html` 钉住这条。
//!
//! # HTML 正文的兜底
//!
//! M1 没有 HTML 渲染器，所以只有 HTML 版本的邮件靠 [`html_to_text`] 压成纯文本
//! 显示（丢 JS/CSS 内容、块级标签变换行、还原常见实体）。它是**解码**不是渲染：
//! 真正的白名单渲染在 M2。

use mail_parser::{MessageParser, MimeHeaders, PartType};

use crate::error::{MailError, Result};

/// 从头部解析出来的字段（列表行用它）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedHeaders {
    /// 主题（已解码）。
    pub subject: String,
    /// 发件人显示名。
    pub from_name: Option<String>,
    /// 发件人地址。
    pub from_addr: Option<String>,
    /// 收件人（`"名字 <addr>"` 形态）。
    pub to: Vec<String>,
    /// 抄送。
    pub cc: Vec<String>,
    /// 发送时间（Unix 秒；解析不出来时为 0）。
    pub date: i64,
    /// `Message-ID`。
    pub message_id: Option<String>,
}

/// 一封完整邮件的解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedMessage {
    /// 头部字段。
    pub headers: ParsedHeaders,
    /// 纯文本正文（已解码）。
    pub text_body: Option<String>,
    /// HTML 正文（已解码，**未净化**）。
    pub html_body: Option<String>,
    /// 附件（含内联图）。
    pub attachments: Vec<ParsedAttachment>,
}

/// 一个附件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAttachment {
    /// 文件名（没有时按 MIME 类型给个占位）。
    pub filename: String,
    /// MIME 类型。
    pub mime_type: String,
    /// 字节数。
    pub size: i64,
    /// `Content-ID`（已去掉尖括号）。
    pub content_id: Option<String>,
    /// 是否内联（`Content-Disposition: inline`）。
    pub is_inline: bool,
    /// 内容。
    pub bytes: Vec<u8>,
}

/// 只解析头部。喂给它的可以是 `BODY.PEEK[HEADER]` 的结果。
pub fn parse_headers(raw: &[u8]) -> Result<ParsedHeaders> {
    let msg = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| MailError::Mime("cannot parse message headers".into()))?;
    Ok(ParsedHeaders {
        subject: msg.subject().unwrap_or_default().to_string(),
        from_name: first_from_name(&msg),
        from_addr: first_from_addr(&msg),
        to: collect_addresses(msg.to()),
        cc: collect_addresses(msg.cc()),
        date: msg.date().map(|d| d.to_timestamp()).unwrap_or(0),
        message_id: msg.message_id().map(str::to_string),
    })
}

/// 解析整封邮件（头部 + 正文 + 附件）。
pub fn parse(raw: &[u8]) -> Result<ParsedMessage> {
    let msg = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| MailError::Mime("cannot parse message body".into()))?;

    let headers = ParsedHeaders {
        subject: msg.subject().unwrap_or_default().to_string(),
        from_name: first_from_name(&msg),
        from_addr: first_from_addr(&msg),
        to: collect_addresses(msg.to()),
        cc: collect_addresses(msg.cc()),
        date: msg.date().map(|d| d.to_timestamp()).unwrap_or(0),
        message_id: msg.message_id().map(str::to_string),
    };

    // 正文：`text_bodies()` / `html_bodies()` 只为 `multipart/alternative` 里
    // 真正呈给用户看的那部分产出，已经排除了附件。
    //
    // ⚠️ 但**必须再按部件自己声明的 MIME 类型判一次**：`mail-parser` 遇到
    // 「整个报文连 `Content-Type` 都没有」时（老 MUA、或被网关剥掉头的邮件，
    // 数量并不小），会把**同一个部件同时列进两个迭代器**——它不知道这是什么，
    // 就两边都算。照单全收的后果是把纯文本当 HTML 渲染：换行全丢（HTML 折叠
    // 空白）、正文里出现的 `<` 会被当成标记。所以两侧互为排他（见 `is_html_part`）。
    let text_body = msg
        .text_bodies()
        .next()
        .filter(|p| !is_html_part(p))
        .and_then(part_text)
        .map(str::to_string);
    let html_body = msg
        .html_bodies()
        .next()
        .filter(|p| is_html_part(p))
        .and_then(part_text)
        .map(str::to_string);

    let mut attachments = Vec::new();
    for part in msg.attachments() {
        let content_id = part.content_id().map(trim_cid);
        // `Content-Disposition` 在 mail-parser 里和 `Content-Type` 共用同一个
        // `ContentType` 结构：`inline` / `attachment` 落在 `ctype()` 上（`filename`
        // 才是 attribute），所以按值判而不是按枚举判。
        let is_inline = part
            .content_disposition()
            .is_some_and(|cd| cd.ctype().eq_ignore_ascii_case("inline"));
        let bytes = part_bytes(part);
        let mime_type = part
            .content_type()
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".into());
        let filename = part
            .attachment_name()
            .map(str::to_string)
            .unwrap_or_else(|| default_attachment_name(&mime_type, attachments.len()));

        attachments.push(ParsedAttachment {
            filename,
            mime_type,
            size: bytes.len() as i64,
            content_id,
            is_inline,
            bytes,
        });
    }

    Ok(ParsedMessage {
        headers,
        text_body,
        html_body,
        attachments,
    })
}

/// 把 HTML 正文里的 `cid:xxx` 换成 `data:` 内联 URL。
///
/// 只替换**确实是内联图**（`is_inline` 且有 `content_id`）的附件，且受
/// `max_total_bytes` 约束——超过就不再内联，改为保留 `cid:`（渲染层会显示占位）。
/// 这是 Flectar 实测出来的容量纪律：单信 16 MB 上限
/// （`docs/email-design.md` §4.8）。
pub fn inline_cid_images(
    html: &str,
    attachments: &[ParsedAttachment],
    max_total_bytes: usize,
) -> String {
    let mut out = html.to_string();
    let mut budget = max_total_bytes;
    for a in attachments {
        let Some(cid) = a.content_id.as_deref() else {
            continue;
        };
        if !a.is_inline {
            continue;
        }
        let cost = a.bytes.len();
        if cost > budget {
            tracing::debug!(
                filename = %a.filename,
                size = cost,
                "skipping cid inlining: over the per-message budget"
            );
            continue;
        }
        budget -= cost;
        let data_url = format!("data:{};base64,{}", a.mime_type, base64_encode(&a.bytes));
        // `cid:` 在正文里可能带尖括号也可能不带，两种都替换。
        out = out.replace(&format!("cid:{cid}"), &data_url);
        out = out.replace(&format!("cid:<{cid}>"), &data_url);
    }
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 把 HTML 正文压成纯文本，供**没有 HTML 渲染器**的阅读器显示。
///
/// # 这为什么是内核的活，不是界面的活
///
/// 它是**解码**而不是渲染：输入是解码后的 HTML（字符集/传输编码已经由
/// `mail-parser` 处理过），输出是给人读的文本。放内核才能单测——而这里恰恰全是
/// 坑：`<script>` 里的 JS 必须整段丢掉（否则会像正文一样印出来）、`<br>` 与
/// `</p>` 必须变成换行（否则整封信挤成一行）、`&nbsp;` 之类实体必须还原。
///
/// **不做的事**：不保留任何格式、不解析表格布局、不还原链接地址。M2 上真正的
/// HTML 渲染器（含 `docs/email-design.md` §4.8 的标签白名单）后，这个函数退化成
/// 「无渲染器时的兜底」。
///
/// 安全口径：输出是**纯文本**，调用方把它塞进 `Text` 元素，不经过任何标记解释，
/// 所以这里不做转义（转义反而是错的——用户会看到 `&lt;`）。
pub fn html_to_text(html: &str) -> String {
    let bytes: Vec<char> = html.chars().collect();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0usize;
    // 当前是否在「丢弃整段内容」的标签里（`script` / `style`）——这两种标签的
    // 内容不是给人看的。
    let mut dropping: Option<&str> = None;

    while i < bytes.len() {
        match bytes[i] {
            '<' => {
                // 找标签结束。到不了 `>` 就说明这不是标签，按普通字符处理。
                let Some(rel_end) = bytes[i..].iter().position(|c| *c == '>') else {
                    out.push('<');
                    i += 1;
                    continue;
                };
                let raw: String = bytes[i + 1..i + rel_end].iter().collect();
                i += rel_end + 1;

                // 注释 `<!-- … -->` 整段丢。
                if raw.starts_with("!--") {
                    continue;
                }
                let closing = raw.starts_with('/');
                let body = raw.trim_start_matches('/').trim();
                let name: String = body
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_ascii_lowercase();
                let name = name.as_str();

                match dropping {
                    Some(tag) if closing && name == tag => dropping = None,
                    // 丢弃段里的一切标签都忽略（嵌套同名标签极少见，不必处理）。
                    Some(_) => {}
                    None => {
                        if !closing && (name == "script" || name == "style") {
                            dropping = Some(if name == "script" { "script" } else { "style" });
                        } else if is_single_line_tag(name) {
                            // 列表项 / 表格单元自己只占一行，且前后**本来就**有块级
                            // 标签的换行。这里再补一次会挤出「甲空行乙空行」——
                            // 比挤在一起更难读。所以只在还没换行时补一个。
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                        } else if is_block_tag(name) {
                            // 段落级标签前后各补一个换行 → 段落之间是**空行**，
                            // 这是纯文本阅读里区分段落最省事的形式。
                            out.push('\n');
                        }
                    }
                }
            }
            '&' => {
                // 实体：找到 `;` 就尝试还原，找不到就当普通 `&`。
                let Some(rel) = bytes[i..].iter().position(|c| *c == ';') else {
                    out.push('&');
                    i += 1;
                    continue;
                };
                // 实体名有长度上限，避免把一长串文本当成实体扫。
                if rel > 12 {
                    out.push('&');
                    i += 1;
                    continue;
                }
                let name: String = bytes[i + 1..i + rel].iter().collect();
                if let Some(ch) = decode_entity(&name) {
                    out.push(ch);
                    i += rel + 1;
                } else {
                    out.push('&');
                    i += 1;
                }
            }
            c => {
                if dropping.is_none() {
                    out.push(c);
                }
                i += 1;
            }
        }
    }

    collapse_blank_lines(&out)
}

/// 这些标签代表**段落间隔**（前后各一次换行）。
fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "br" | "p"
            | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "blockquote"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "hr"
            | "table"
            | "ul"
            | "ol"
            | "pre"
    )
}

/// 这些标签只占一行：换行只在「还没换行时」补一次，避免列表项之间被挤出空行。
/// 与 [`is_block_tag`] **互斥**（调用点先判这个）。
fn is_single_line_tag(name: &str) -> bool {
    matches!(name, "li" | "tr" | "td" | "th" | "option" | "dt" | "dd")
}

/// 常见命名实体 + 数字实体（`&#123;` / `&#x7B;`）。
fn decode_entity(name: &str) -> Option<char> {
    if let Some(digits) = name.strip_prefix("#") {
        let code = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            digits.parse::<u32>().ok()?
        };
        return char::from_u32(code);
    }
    // 只收**常见**的几个：全表（2000+ 条）会带来一张无意义的常量表，而正文里
    // 真正高频的就这些。未收录的实体保留 `&`（见调用点），用户至少能看到原文。
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ensp" | "emsp" | "thinsp" => ' ',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "laquo" => '«',
        "raquo" => '»',
        "ldquo" => '“',
        "rdquo" => '”',
        "lsquo" => '‘',
        "rsquo" => '’',
        "bull" => '•',
        "middot" => '·',
        "deg" => '°',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "sect" => '§',
        "times" => '×',
        "divide" => '÷',
        _ => return None,
    })
}

/// 收掉多余空行：连续 3 个以上换行压成 2 个，并去掉首尾空白。
///
/// 不做「把行内多余空格压成一个」——中文邮件里常有靠空格对齐的表格化正文，
/// 压掉就把排版毁了。
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            run += 1;
            if run > 1 {
                continue;
            }
        } else {
            run = 0;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    out.trim().to_string()
}

fn trim_cid(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

/// 这个部件自己声明的是不是 HTML。
///
/// **只认显式的 `subtype == "html"`**：`text/html` 里的 `html` 是在 subtype 位上，
/// `ctype()` 拿到的是 `text`。没有 `Content-Type`（`subtype()` 为 `None`）的部件
/// 一律不算 HTML——那正是纯文本邮件最常见的形态。
fn is_html_part(part: &mail_parser::MessagePart<'_>) -> bool {
    part.content_type()
        .and_then(|ct| ct.subtype())
        .is_some_and(|sub| sub.eq_ignore_ascii_case("html"))
}

fn default_attachment_name(mime_type: &str, index: usize) -> String {
    let ext = match mime_type {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "text/plain" => "txt",
        "application/zip" => "zip",
        _ => "bin",
    };
    format!("attachment-{}.{ext}", index + 1)
}

fn first_from_name(msg: &mail_parser::Message<'_>) -> Option<String> {
    msg.from()?
        .first()
        .and_then(|a| a.name())
        .map(str::to_string)
}

fn first_from_addr(msg: &mail_parser::Message<'_>) -> Option<String> {
    msg.from()?
        .first()
        .and_then(|a| a.address())
        .map(str::to_string)
}

fn collect_addresses(addr: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(addr) = addr else {
        return out;
    };
    // `Addr` 的字段是 `Option<Cow<str>>`，而且这里拿到的只有 `&Addr`，不能 move
    // 字段出来 —— 一律走 `name()` / `address()` 访问器拿 `&str`。分组地址里的
    // 成员已经被 `Address::iter()` 展平了。
    for item in addr.iter() {
        match (item.name(), item.address()) {
            (Some(n), Some(e)) => out.push(format!("{n} <{e}>")),
            (None, Some(e)) => out.push(e.to_string()),
            (Some(n), None) => out.push(n.to_string()),
            (None, None) => {}
        }
    }
    out
}

// 显式写两个生命周期：`MessagePart<'x>` 里 `Cow<'x, _>` 的 `'x` 比借用它的 `&`
// 活得更久，光靠省略规则推不出 `'x: 'a`，会报「returns a value referencing data
// owned by the current function」。
//
// ⚠️ `Text` 与 `Html` 是 `PartType` 的**两个不同变体**（不是同一个变体带个标记），
// 取正文时两个都要收——只匹配 `Text` 的话，所有 HTML 正文会静默变成 `None`。
fn part_text<'a>(part: &'a mail_parser::MessagePart<'a>) -> Option<&'a str> {
    match &part.body {
        PartType::Text(text) | PartType::Html(text) => Some(text.as_ref()),
        _ => None,
    }
}

fn part_bytes(part: &mail_parser::MessagePart<'_>) -> Vec<u8> {
    match &part.body {
        PartType::Text(text) | PartType::Html(text) => text.as_bytes().to_vec(),
        PartType::Binary(bytes) | PartType::InlineBinary(bytes) => bytes.to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &[u8] = b"From: =?UTF-8?B?5byg5LiJ?= <zhang@example.com>\r\n\
To: me@example.com\r\n\
Cc: cc@example.com\r\n\
Subject: =?UTF-8?B?5L2g5aW977yM5LiW55WM?=\r\n\
Message-ID: <abc@example.com>\r\n\
Date: Tue, 12 Oct 2026 10:00:00 +0800\r\n\
\r\n\
hello world\r\n";

    const WITH_ATTACHMENT: &[u8] = b"From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: files\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n\
\r\n\
--BOUND\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
see attached\r\n\
--BOUND\r\n\
Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--BOUND--\r\n";

    /// 只有 HTML 正文、没有 text/plain 备胎的邮件。
    const HTML_ONLY: &[u8] = b"From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: rich\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>hello<br>world</p>\r\n";

    /// `multipart/alternative`：同一封邮件同时有纯文本与 HTML 两个版本。
    const ALTERNATIVE: &[u8] = b"From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: alt\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=\"ALT\"\r\n\
\r\n\
--ALT\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
plain version\r\n\
--ALT\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>html version</p>\r\n\
--ALT--\r\n";

    #[test]
    fn headers_decode_mime_encoded_words() {
        let h = parse_headers(SIMPLE).unwrap();
        // RFC2047 编码的主题与显示名都要解出来。
        assert_eq!(h.subject, "你好，世界");
        assert_eq!(h.from_name.as_deref(), Some("张三"));
        assert_eq!(h.from_addr.as_deref(), Some("zhang@example.com"));
        // mail-parser 已经剥掉了 `Message-ID` 的尖括号（`<abc@x>` → `abc@x`），
        // 所以库里存的是裸值——将来比对 `References` / `In-Reply-To` 时两边同归一化。
        assert_eq!(h.message_id.as_deref(), Some("abc@example.com"));
        assert_eq!(h.to, vec!["me@example.com"]);
        assert_eq!(h.cc, vec!["cc@example.com"]);
        // 2026-10-12 10:00:00 +0800 == 2026-10-12 02:00:00 UTC
        assert_eq!(h.date, 1_791_770_400);
    }

    #[test]
    fn full_parse_extracts_body() {
        let m = parse(SIMPLE).unwrap();
        // 单部件纯文本的正文按原样保留（含报文里那个行尾 CRLF）——做展示层的
        // 再决定要不要 trim，内核不替它去掉可能有意义的空白。
        assert_eq!(m.text_body.as_deref(), Some("hello world\r\n"));
        assert_eq!(m.html_body, None);
        assert!(m.attachments.is_empty());
    }

    #[test]
    fn headerless_content_type_is_plain_text_not_html() {
        // 回归：`mail-parser` 对没有 `Content-Type` 的报文会把同一个部件同时列进
        // `text_bodies()` 与 `html_bodies()`。若照单全收，SIMPLE 这封信会被当成
        // HTML —— 换行丢失，正文里的 `<` 还会被当标记解释。必须只认纯文本。
        let m = parse(SIMPLE).unwrap();
        assert!(
            m.html_body.is_none(),
            "plain-text mail must not be read as HTML"
        );
    }

    #[test]
    fn html_body_is_picked_up_when_declared() {
        let m = parse(HTML_ONLY).unwrap();
        assert_eq!(m.html_body.as_deref(), Some("<p>hello<br>world</p>\r\n"));
        // 反过来也不能把一个真 HTML 部件塞进纯文本位。
        assert!(m.text_body.is_none());
    }

    #[test]
    fn alternative_keeps_both_versions_apart() {
        let m = parse(ALTERNATIVE).unwrap();
        // 两个版本各归各位，不互相串门。边界行吃掉了正文末尾的 CRLF，所以这里
        // 没有行尾符（与单部件报文的 `SIMPLE` 不同）。
        assert_eq!(m.text_body.as_deref(), Some("plain version"));
        assert_eq!(m.html_body.as_deref(), Some("<p>html version</p>"));
    }

    #[test]
    fn attachments_are_decoded() {
        let m = parse(WITH_ATTACHMENT).unwrap();
        assert_eq!(m.text_body.as_deref(), Some("see attached"));
        assert_eq!(m.attachments.len(), 1);
        let a = &m.attachments[0];
        assert_eq!(a.filename, "report.pdf");
        assert_eq!(a.mime_type, "application/pdf");
        assert!(!a.is_inline);
        // base64 "JVBERi0xLjQK" == "%PDF-1.4\n"
        assert_eq!(a.bytes, b"%PDF-1.4\n");
    }

    #[test]
    fn headerless_input_is_an_error_not_a_panic() {
        // 一行既没有冒号也没有结构的文本，mail-parser 认为「找不到头部」→ 返回
        // None。内核把它当错（不是一封信），但**绝不能 panic**。
        // 调用方（`sync.rs`）对这种情况是「跳过这一封、继续同步」。
        assert!(parse(b"just some text\r\n").is_err());
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        // 空输入解析不出东西。
        assert!(parse(b"").is_err());
    }

    #[test]
    fn cid_inlining_respects_the_budget() {
        let att = ParsedAttachment {
            filename: "logo.png".into(),
            mime_type: "image/png".into(),
            size: 4,
            content_id: Some("logo@x".into()),
            is_inline: true,
            bytes: vec![1, 2, 3, 4],
        };
        let html = "<img src=\"cid:logo@x\">";
        // 预算够：替换成 data:。
        let out = inline_cid_images(html, std::slice::from_ref(&att), 1024);
        assert!(out.starts_with("<img src=\"data:image/png;base64,"));
        // 预算不够：保留原样（渲染层显示占位），不硬塞。
        let out = inline_cid_images(html, std::slice::from_ref(&att), 1);
        assert_eq!(out, html);
    }

    #[test]
    fn non_inline_attachment_is_not_inlined() {
        let att = ParsedAttachment {
            filename: "doc.pdf".into(),
            mime_type: "application/pdf".into(),
            size: 4,
            content_id: Some("doc@x".into()),
            is_inline: false,
            bytes: vec![1, 2, 3, 4],
        };
        let html = "<a href=\"cid:doc@x\">x</a>";
        assert_eq!(inline_cid_images(html, &[att], 1024), html);
    }

    #[test]
    fn html_to_text_keeps_the_reading_order() {
        let text = html_to_text(
            "<html><body><p>第一段</p><p>第二段<br>换行</p><ul><li>甲</li><li>乙</li></ul></body></html>",
        );
        // 段落之间是空行（`</p><p>` 各补一次换行），列表项各自一行。
        assert_eq!(text, "第一段\n\n第二段\n换行\n\n甲\n乙");
    }

    #[test]
    fn html_to_text_drops_script_and_style_bodies() {
        // `script` / `style` 的**内容**不是给人看的：留一句 JS 在正文里是缺陷。
        let text =
            html_to_text("开始<style>p{color:red}</style>中间<script>alert('x')</script>结束");
        assert_eq!(text, "开始中间结束");
        assert!(!text.contains("alert"));
        assert!(!text.contains("color"));
    }

    #[test]
    fn html_to_text_decodes_entities() {
        assert_eq!(
            html_to_text("a&amp;b &lt;tag&gt; &quot;q&quot;"),
            "a&b <tag> \"q\""
        );
        assert_eq!(html_to_text("&#20013;&#x6587;"), "中文");
        assert_eq!(html_to_text("x&nbsp;y&hellip;"), "x y…");
        // 不认识的实体原样保留 `&`（至少用户能看出这里有个实体）。
        assert_eq!(html_to_text("a&notarealentity;b"), "a&notarealentity;b");
    }

    #[test]
    fn html_to_text_tolerates_broken_markup() {
        // 未闭合的 `<`、未闭合的标签、悬空的 `&` 都不该 panic 或吞掉正文。
        assert_eq!(html_to_text("3 < 5"), "3 < 5");
        assert_eq!(html_to_text("<p>没闭合"), "没闭合");
        assert_eq!(html_to_text("AT&T"), "AT&T");
    }

    #[test]
    fn html_to_text_collapses_only_blank_runs() {
        // 行内空格保留（中文邮件常靠空格对齐），只收多余空行。
        let text = html_to_text("<p>a</p><br><br><br><p>b</p>");
        assert_eq!(text, "a\n\nb");
    }
}
