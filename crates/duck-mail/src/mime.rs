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
//! # HTML 正文：渲染 + 兜底
//!
//! 渲染在界面层（`pages/mail.rs` 用 `gpui-base` 的 `TextView::html`，见
//! `docs/email-design.md` §4.8），内核这一侧负责**把 HTML 准备好**：
//!
//! - [`inline_cid_images`]：把 `cid:` 换成 `data:`（附件字节只有解析时拿得到，
//!   所以这一步必须在 `sync::fetch_body_inner` 里做完再落库）；
//! - [`strip_remote_images`] / [`has_remote_images`]：远程图片闸门（默认拦截）。
//!
//! [`html_to_text`] 保留为**渲染层不可用 / 用户选择纯文本**时的兜底：它是解码
//! （丢 JS/CSS 内容、块级标签变换行、还原常见实体），不是渲染。

use std::collections::HashMap;

use mail_parser::{HeaderValue, MessageParser, MimeHeaders, PartType};

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
    /// `In-Reply-To`（直接父的 `Message-ID`，已归一化去尖括号）。
    ///
    /// ⚠️ `References` 与 `In-Reply-To` **不是一回事**：前者是整条祖先链，
    /// 后者只指直接的上一封。很多客户端只写 `In-Reply-To` 不写 `References`，
    /// 所以它会**追加进** [`ParsedHeaders::references`] 的末尾，一并作为线程合并的线索。
    pub in_reply_to: Option<String>,
    /// 引用链：`References` 的每一项（归一化、去重、按出现顺序），
    /// 其后追加 `In-Reply-To`（若存在且未重复）。
    ///
    /// 这是线程合并（[`crate::threading`]）与 `message_refs` 落库的**唯一来源**——
    /// 合并算法需要的「这封引用了谁」就取自这里。
    pub references: Vec<String>,
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
    /// 是否内联——**渲染层据此决定「这是正文里的图」还是「这是附件」**。
    ///
    /// 判定是三档（见 [`parse`] 里的注释）：显式 `attachment` 恒为 `false`；显式
    /// `inline` 恒为 `true`；两者都没有时，**有 `Content-ID` 就算内联**（`cid:` 引用
    /// 的就是它）。
    pub is_inline: bool,
    /// 内容。
    pub bytes: Vec<u8>,
    /// IMAP 节号（如 `2` / `1.2`）——按需下载附件时用它 `BODY.PEEK[<section>]`。
    ///
    /// 由 [`section_map`] 从 MIME 结构走查推出来；嵌套 `message/rfc822` 等语义特殊的
    /// 结构给不出可靠段号，这里是 `None`（调用方降级为整封取回）。
    pub imap_section: Option<String>,
}

/// 从头部抽线程线索，返回 `(in_reply_to, references)`。
///
/// `References` 是祖先链、`In-Reply-To` 只是直接父，很多客户端只写后者，
/// 所以把它追加进引用链（去重），让 [`crate::threading`] 拿到完整线索。
fn thread_headers(msg: &mail_parser::Message<'_>) -> (Option<String>, Vec<String>) {
    let in_reply_to = header_message_ids(msg.in_reply_to()).into_iter().next();
    let mut references = header_message_ids(msg.references());
    // 用 `filter` 而不是嵌套 `if`：后者会被 clippy 建议合并成 `let`-chain（Rust 1.88），
    // 而本仓 MSRV 是 1.85，用不了那个语法。
    if let Some(parent) = in_reply_to
        .as_deref()
        .filter(|parent| !references.iter().any(|r| r.as_str() == *parent))
    {
        references.push(parent.to_string());
    }
    (in_reply_to, references)
}

/// `In-Reply-To` / `References` 这类「一串 `Message-ID`」的头 → 归一化后的 id 列表。
///
/// `mail-parser` 把这类头解析成 `Text`（整行）或 `TextList`（已切开）**两种都可能**，
/// 所以两条路径都要认。归一化走 [`crate::threading::normalize_message_id`]，
/// 与线程合并同一口径——否则「这里带尖括号、那里不带」会让匹配悄悄失败。
fn header_message_ids(value: &HeaderValue<'_>) -> Vec<String> {
    let tokens: Vec<&str> = match value {
        HeaderValue::Text(text) => text.split_whitespace().collect(),
        HeaderValue::TextList(list) => list.iter().flat_map(|s| s.split_whitespace()).collect(),
        _ => Vec::new(),
    };
    tokens
        .into_iter()
        .filter_map(crate::threading::normalize_message_id)
        .collect()
}

/// 只解析头部。喂给它的可以是 `BODY.PEEK[HEADER]` 的结果。
pub fn parse_headers(raw: &[u8]) -> Result<ParsedHeaders> {
    let msg = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| MailError::Mime("cannot parse message headers".into()))?;
    let (in_reply_to, references) = thread_headers(&msg);
    Ok(ParsedHeaders {
        subject: msg.subject().unwrap_or_default().to_string(),
        from_name: first_from_name(&msg),
        from_addr: first_from_addr(&msg),
        to: collect_addresses(msg.to()),
        cc: collect_addresses(msg.cc()),
        date: msg.date().map(|d| d.to_timestamp()).unwrap_or(0),
        message_id: msg.message_id().map(str::to_string),
        in_reply_to,
        references,
    })
}

/// 解析整封邮件（头部 + 正文 + 附件）。
pub fn parse(raw: &[u8]) -> Result<ParsedMessage> {
    let msg = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| MailError::Mime("cannot parse message body".into()))?;

    let (in_reply_to, references) = thread_headers(&msg);
    let headers = ParsedHeaders {
        subject: msg.subject().unwrap_or_default().to_string(),
        from_name: first_from_name(&msg),
        from_addr: first_from_addr(&msg),
        to: collect_addresses(msg.to()),
        cc: collect_addresses(msg.cc()),
        date: msg.date().map(|d| d.to_timestamp()).unwrap_or(0),
        message_id: msg.message_id().map(str::to_string),
        in_reply_to,
        references,
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

    // 先算一遍「MIME 部件 → IMAP 节号」，附件的段号随元数据一起落库（下载时用）。
    let sections = section_map(&msg);

    let mut attachments = Vec::new();
    for part_id in &msg.attachments {
        // `Message::attachments` 是部件下标列表；按它索引 `parts` 才能同时拿到段号
        // （`msg.attachments()` 迭代器把下标藏起来了）。
        let Some(part) = msg.parts.get(*part_id as usize) else {
            continue;
        };
        let content_id = part.content_id().map(trim_cid);
        // `Content-Disposition` 在 mail-parser 里和 `Content-Type` 共用同一个
        // `ContentType` 结构：`inline` / `attachment` 落在 `ctype()` 上（`filename`
        // 才是 attribute），所以按值判而不是按枚举判。
        //
        // ⚠️ 判定是**三档**，只认 `inline` 是错的：
        //   ① 显式 `attachment` → 一定是附件（**哪怕带 `Content-ID`**——Gmail /
        //      Exchange 会给每个部件都盖一个 `Content-ID`，包括普通文件，信它会把
        //      真附件在列表里藏掉）；
        //   ② 显式 `inline` → 内联图；
        //   ③ 没有 `Content-Disposition` → **有 `Content-ID` 就当内联**。
        //
        // 第三档是「正文图片不显示」最常见的根因之一：相当多 MUA（老 Outlook、部分
        // 网易系客户端）的 `cid:` 图片只写 `Content-ID`、不写 `Content-Disposition`。
        // 按旧口径它们全是 `false`，于是 [`inline_cid_images`] 一张都不换，正文里只剩
        // `cid:…` 死链。
        //
        // 参考实现：Flectar Mail `crates/flectar-mail-core/src/mime/mod.rs:257-261`
        // 的三档 `match`（同一份口径）。
        let is_inline = match part.content_disposition().map(|cd| cd.ctype()) {
            Some(value) if value.eq_ignore_ascii_case("attachment") => false,
            Some(value) if value.eq_ignore_ascii_case("inline") => true,
            _ => content_id.is_some(),
        };
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
            imap_section: sections.get(part_id).cloned(),
        });
    }

    Ok(ParsedMessage {
        headers,
        text_body,
        html_body,
        attachments,
    })
}

/// 纯函数：MIME 部件下标 → IMAP 节号。
///
/// RFC 3501 §6.4.5 的编号规则：顶层是 `multipart/*` 时它的子部件从 `1` 开始编号，
/// 深一层就在父编号后加 `.k`；非多部件的根部件（单部件邮件）正文就是 `1`。
/// 嵌套 `message/rfc822` 的编号语义另有规则（`HEADER` / `TEXT` / 内层子节），本模块
/// **不猜**——遇到就跳过，调用方拿到的段号为空时降级为整封取回。
fn section_map(msg: &mail_parser::Message<'_>) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    // 根部件是 `parts[0]`（`Message::root_part`）。空 `prefix` 表示「还在根上」。
    walk_sections(msg, 0, "", &mut out);
    out
}

fn walk_sections(
    msg: &mail_parser::Message<'_>,
    id: u32,
    prefix: &str,
    out: &mut HashMap<u32, String>,
) {
    let Some(part) = msg.parts.get(id as usize) else {
        return;
    };
    match &part.body {
        // 多部件本身没有可取的单一段号，只给它下面的子部件编号。
        PartType::Multipart(children) => {
            for (index, child) in children.iter().enumerate() {
                let number = index + 1;
                let child_prefix = if prefix.is_empty() {
                    number.to_string()
                } else {
                    format!("{prefix}.{number}")
                };
                walk_sections(msg, *child, &child_prefix, out);
            }
        }
        // 嵌套邮件：段号语义特殊，不提供（降级）。
        PartType::Message(_) => {}
        // 叶部件：有空前缀时是「单部件邮件的正文」，否则用它自己的路径。
        _ => {
            let section = if prefix.is_empty() {
                "1".to_string()
            } else {
                prefix.to_string()
            };
            out.insert(id, section);
        }
    }
}

/// 把「按节取回的部件原始内容」解码成附件字节。
///
/// 输入是 `BODY.PEEK[<section>.MIME]`（MIME 头）与 `BODY.PEEK[<section>]`（仍带传输
/// 编码的正文）。做法是把两段拼成一封**单部件报文**再交给 `mail-parser`——这样
/// base64 / quoted-printable / 7bit 三种编码都不必自己写解码器，也不依赖库里存
/// 传输编码（本仓的 `attachments` 表没有这一列）。
///
/// 解析不出内容（服务端没回 MIME 头、或结构不像 MIME）时返回 `None`——调用方据此
/// **降级**为整封取回，绝不把未解码的字节当成附件写盘。
pub fn decode_attachment_part(mime_headers: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    if mime_headers.is_empty() {
        return None;
    }
    let mut raw = Vec::with_capacity(mime_headers.len() + body.len() + 4);
    raw.extend_from_slice(mime_headers);
    if !mime_headers.ends_with(b"\r\n") && !mime_headers.ends_with(b"\n") {
        raw.extend_from_slice(b"\r\n");
    }
    // 头部与正文之间必须有空行。
    raw.extend_from_slice(b"\r\n");
    raw.extend_from_slice(body);

    let msg = MessageParser::default().parse(&raw)?;
    // 单部件报文的内容可能落在「附件」位，也可能落在纯文本 / HTML 正文位——
    // 三种都收，取第一个解析出来的部件。
    let part = msg
        .attachments()
        .next()
        .or_else(|| msg.text_bodies().next())
        .or_else(|| msg.html_bodies().next())?;
    Some(part_bytes(part))
}

/// 内联图张数上限（`docs/email-design.md` §4.8：内联图 ≤ 32 张）。
///
/// 一封被伪造出几百个 `Content-ID` 的邮件会让替换变成 O(n·len) 的字符串扫描，
/// 这条上限同时也是「哪些图值得内联」的取舍：第 33 张开始保留 `cid:`。
const MAX_INLINE_IMAGES: usize = 32;

/// 单信内联图的总字节预算。
///
/// 取设计文档 §4.8 的量级（单图 8 MB / 单信 16 MB / ≤ 32 张）。**与设计口径的
/// 差异**：设计把 CID 重写放在「渲染前、内存里」，而本模块把它放在
/// `sync::fetch_body_inner`——那是唯一还能拿到附件字节的地方（附件字节按 §4.5
/// 不落库）。因此这条预算**同时是单条 `message_bodies` 行的增长上界**（base64
/// 后约 4/3）。要收紧只改这一个数。
pub const INLINE_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// 把 HTML 正文里的 `cid:xxx` 换成 `data:` 内联 URL。
///
/// 只替换**确实是内联图**（`is_inline` 且有 `content_id`）的附件，且受
/// `max_total_bytes` 与 [`MAX_INLINE_IMAGES`] 约束——超过就不再内联，改为保留
/// `cid:`（渲染层会显示占位）。
///
/// 做法分两步：先把「归一化后的 cid → `data:` URL」算成一张表（预算与张数闸门只在
/// 这一步消耗），再交给 [`rewrite_cid_src`] **一次走完** HTML。旧实现是逐个 cid 做
/// `str::replace`（每个都是 O(len) 全文扫描）＋全文替换，既慢又会把 `href` 里的
/// `cid:` 一起改坏（详见 [`rewrite_cid_src`] 的注释）。
pub fn inline_cid_images(
    html: &str,
    attachments: &[ParsedAttachment],
    max_total_bytes: usize,
) -> String {
    let mut budget = max_total_bytes;
    let mut inlined = 0usize;
    let mut data_urls: HashMap<String, String> = HashMap::new();

    for a in attachments {
        let Some(cid) = a.content_id.as_deref() else {
            continue;
        };
        if !a.is_inline {
            continue;
        }
        if inlined >= MAX_INLINE_IMAGES {
            tracing::debug!(
                filename = %a.filename,
                "skipping cid inlining: image count cap reached"
            );
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
        inlined += 1;
        // MIME 必须归一化：渲染侧拿 `data:` URL 的类型串去查 GPUI 的 `ImageFormat`
        // 值域，**查不到就整张图放弃**（空白，不是占位也不是报错）。见
        // [`normalize_image_mime`]。
        let mime = normalize_image_mime(&a.mime_type, &a.bytes);
        data_urls.insert(
            trim_cid(cid),
            format!("data:{mime};base64,{}", base64_encode(&a.bytes)),
        );
    }

    if data_urls.is_empty() {
        return html.to_string();
    }
    rewrite_cid_src(html, |cid| data_urls.get(cid).cloned())
}

/// GPUI 的 `ImageFormat` 能解码的 MIME 值域（内核 ↔ 渲染层的**硬契约**）。
///
/// 出处：`gpui-fast` 的 `crates/gpui/src/platform.rs`——`ImageFormat::mime_type()`
/// 给出九个规范名（`:3134-3145`），`from_mime_type_alias()` 另收 `image/jpg` /
/// `image/tif` 两个别名（`:3174-3179`）。渲染侧 `gpui-base` 的
/// `text/utils.rs::data_url_image` 把 `data:` URL 的 `<type>/<subtype>` **原样**
/// 喂给 `ImageFormat::from_mime_type`，认不出就 `None`——图片位置永远是空白。
///
/// ⚠️ 这是 `duck-mail` 向 gpui 的**唯一**约定。本 crate 不依赖 gpui（依赖纪律），
/// 编译器不会替我们核这张表，所以升级 gpui 时必须回来对一遍。测试
/// `every_emitted_mime_is_in_the_gpui_table` 钉住「我们生成的每个值都在这张表里」。
const GPUI_DECODABLE_IMAGE_MIMES: [&str; 11] = [
    "image/png",
    "image/jpeg",
    "image/jpg", // 别名（`from_mime_type_alias`）
    "image/webp",
    "image/gif",
    "image/svg+xml",
    "image/bmp",
    "image/tiff",
    "image/tif", // 别名（`from_mime_type_alias`）
    "image/ico",
    "image/x-portable-anymap",
];

/// 把内联图声明的 MIME 归一化到 [`GPUI_DECODABLE_IMAGE_MIMES`] 的值域内。
///
/// 真实邮件的声明五花八门，而渲染层只认那张表：
///
/// - 部件根本没写 `Content-Type` → 上层（[`parse`]）回落成
///   `application/octet-stream`；
/// - 写的是别名：`image/x-png` / `image/x-icon` / `image/vnd.microsoft.icon` /
///   `image/svg` / `image/pjpeg`；
/// - 类型串与内容对不上（`application/octet-stream` 里其实是 PNG 字节）。
///
/// 所以三级回落：① 已在值域内 → 直接用；② 命中别名表 → 映射到规范名；
/// ③ **嗅探魔数**（[`sniff_image_mime`]）。三级都不中则原样返回——渲染层会和改这个
/// 之前一样给空白，但至少不会比现状更差，也不会谎报类型。
fn normalize_image_mime(mime_type: &str, bytes: &[u8]) -> String {
    let lower = mime_type.trim().to_ascii_lowercase();
    if GPUI_DECODABLE_IMAGE_MIMES.contains(&lower.as_str()) {
        return lower;
    }
    if let Some(canonical) = canonical_image_mime(&lower) {
        return canonical.to_string();
    }
    if let Some(sniffed) = sniff_image_mime(bytes) {
        return sniffed.to_string();
    }
    lower
}

/// 把邮件里常见的 `image/*` 别名映射到 GPUI 的规范名。
///
/// 只收**真正需要改写**的：值域表里已有的（含 `image/jpg` / `image/tif`）不在这里
/// 重复一遍，免得同一个事实有两个真源。
fn canonical_image_mime(lower: &str) -> Option<&'static str> {
    Some(match lower {
        "image/pjpeg" | "image/x-citrix-jpeg" => "image/jpeg",
        "image/x-png" => "image/png",
        // 标准名是 `image/x-icon` / `image/vnd.microsoft.icon`，但 gpui 用的是
        // `image/ico`——不映射就永远空白。
        "image/x-icon" | "image/vnd.microsoft.icon" => "image/ico",
        "image/svg" => "image/svg+xml",
        _ => return None,
    })
}

/// 按**魔数**判断字节流是什么图。返回值一律落在 [`GPUI_DECODABLE_IMAGE_MIMES`] 内。
///
/// 只覆盖 GPUI 能解码的那几种——嗅出 AVI 之类也没用，渲染层照样放弃。
/// 顺序有意为之：JPEG 的 `FF D8 FF` 最短也最易撞，放前面先判掉。
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // RIFF 容器：`RIFF????WEBP`（4 字节长度占位）。
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }
    // TIFF 两种字节序。
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some("image/tiff");
    }
    if bytes.starts_with(b"\0\0\x01\0") {
        return Some("image/ico");
    }
    // Netpbm：`P1`..`P6`（PBM / PGM / PPM / PAM 家族）。
    if bytes.len() >= 2 && bytes[0] == b'P' && (b'1'..=b'6').contains(&bytes[1]) {
        return Some("image/x-portable-anymap");
    }
    if looks_like_svg(bytes) {
        return Some("image/svg+xml");
    }
    None
}

/// SVG 是文本，没有定长魔数：在前 1 KB 里找 `<svg`。
///
/// 前导可能是 BOM、`<?xml …?>`、注释、`<!DOCTYPE svg …>`，所以不能只比开头。
fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    String::from_utf8_lossy(head)
        .to_ascii_lowercase()
        .contains("<svg")
}

/// 把正文里的 `src="cid:…"` 换成 `src="data:…"`。
///
/// `resolve` 收**归一化后的 cid**（见 [`trim_cid`]），返回要写入的 `data:` URL；
/// 返回 `None` 表示不认这个 cid，原样保留（渲染层给空白，但绝不改坏文本）。
///
/// # 与旧实现的三点差异（都是「图片显示不对」的直接原因）
///
/// 1. **只改 `src` 属性**。旧实现是 `str::replace("cid:<id>")` 全文替换——`cid:`
///    同样会出现在 `href`（引用别的邮件里的附件）与纯文本里，全文替换会把一个
///    「链接」变成一整张 base64 内联图。
/// 2. **属性名与 `cid:` 前缀都大小写不敏感**。`SRC=` / `CID:` 是合法 HTML，旧实现
///    全漏；这也是「有的图显示、有的不显示」的来源——取决于发信客户端的写法。
/// 3. **两侧都做 cid 归一化**（去首尾空白与尖括号）。引用侧常写
///    `src="cid:<logo@x>"`，附件侧存的是 `logo@x`；不比归一化值就永不匹配。
///
/// 已内联的 `data:` 与不认识的 `cid:` 都原样留下——本函数**只做替换**，不做闸门。
fn rewrite_cid_src(html: &str, resolve: impl Fn(&str) -> Option<String>) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    // 已抄进 `out` 的**原文**字节数。命中替换时先补 `copied..start`，再从 `stop` 续抄。
    let mut copied = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let Some(end) = find_tag_end(html, i) else {
            // 没有闭合的 `>`：从这里到末尾都不是标签，整段原样留下。
            break;
        };
        let tag = &html[i..=end];
        // 用 `and_then` 链而不是三层嵌套 `if let`：嵌套写法会被 clippy 建议合并成
        // `let`-chain（Rust 1.88），而本仓 MSRV 是 1.85，用不了那个语法。
        let rewrite = attr_value_span(tag, "src").and_then(|(span, value)| {
            let cid_ref = strip_cid_prefix(&value)?;
            let data_url = resolve(&trim_cid(cid_ref))?;
            Some((span, data_url))
        });
        if let Some((span, data_url)) = rewrite {
            // 标签内偏移 → 全文偏移（`tag` 从 `i` 开始）。
            let start = i + span.start;
            let stop = i + span.end;
            out.push_str(&html[copied..start]);
            // 无条件用双引号重写：原写法可能是单引号或无引号，统一成一种更省心，
            // 且双引号是三种写法里唯一处处合法的。
            out.push('"');
            out.push_str(&data_url);
            out.push('"');
            copied = stop;
        }
        i = end + 1;
    }
    out.push_str(&html[copied..]);
    out
}

/// `cid:` 前缀（ASCII 大小写不敏感）之后的内容；不是 `cid:` 引用时返回 `None`。
///
/// 用字节切片判前缀而不是 `str::strip_prefix`：后者区分大小写；而直接切
/// `value[..4]` 在值以非 ASCII 开头时（中文文件名）会切在字符中间 panic。
fn strip_cid_prefix(value: &str) -> Option<&str> {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() >= 4 && bytes[..4].eq_ignore_ascii_case(b"cid:") {
        Some(value[4..].trim())
    } else {
        None
    }
}

/// 这段 HTML 里有没有指向远程资源的图片。
///
/// 「远程」= `http://` / `https://` / 协议相对 `//host/…`。`data:`（已内联的 CID）与
/// 相对路径都不算。
pub fn has_remote_images(html: &str) -> bool {
    scan_images(html, false).1
}

/// 去掉所有指向远程资源的 `<img>` 标签（远程图片闸门）。
///
/// 为什么**整段删掉**而不是把 `src` 清空：`TextView` 是文档模型，一个 `src` 为空的
/// `<img>` 不占版面也不报错，但如果仓库里的加载器去取一个 `cid:` / 远程 URL 会多一次
/// 必然失败的资源请求。删掉是唯一不需要额外机制的确定性结果。
///
/// 代价是 `alt` 文案一起没了——调用方要在正文上方给出「已拦截远程图片」的提示条
/// （`docs/email-design.md` §4.8 的安全口径），否则用户会以为邮件本来就是空的。
pub fn strip_remote_images(html: &str) -> String {
    scan_images(html, true).0
}

/// 把「表格布局」的 HTML 拍平成块级流。
///
/// 营销邮件 / Newsletter 几乎全靠嵌套 `<table>` 排版（Marketo、Mailchimp 的产物一封
/// 能塞四五十个 `<table>`），而 `gpui-base` 的文档模型把表格单元格按顺序平铺——列
/// 布局、`colspan`、单元格留白全都不在，整页挤成一团分不出段。把
/// `table / tbody / thead / tr / td / th` 整族标签改写成 `<div>`（`center` 一并处理
/// ，它不在解析器的块级清单里）：每个单元格自成一个块，整封读起来是自上而下的堆叠
/// 布局。真「数据表」也会被拍平——损失列对齐，换来整体可读，这笔交换在邮件阅读器
/// 里是划算的，也是简版客户端的通行做法。
///
/// 属性不再是「整段丢弃」：改写出的 `<div>` 会带上仍然有意义的样式——`style` 原样
/// 保留，传统 `bgcolor` / `align` 翻成等价声明，`<center>` 补 `text-align:center`。
/// 块级**背景 / 内边距 / 对齐**由文档模型真正渲染出来（`BlockNode::Root` 的块级
/// 样式，见 `vendor/gpui-base/GPUI_BASE_PATCHES.md`）；拍平后确实没有意义的属性
/// （`width` / `height` / `cellspacing` / `role="presentation"`）仍然丢。
///
/// 条件注释（`<!--[if mso]><table>…<![endif]-->`）整段跳过：那是给 Outlook 的降级
/// 布局，解析器本来就把注释丢掉，改写进去只会留下孤儿闭合标签。
pub fn flatten_email_tables(html: &str) -> String {
    if !html.contains('<') {
        return html.to_string();
    }
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if html[i..].starts_with("<!--") {
                // 注释整段原样拷贝（解析器会丢掉它，改写里面没意义）。
                match html[i + 4..].find("-->") {
                    Some(end) => {
                        let stop = i + 4 + end + 3;
                        out.push_str(&html[i..stop]);
                        i = stop;
                    }
                    None => {
                        out.push_str(&html[i..]);
                        break;
                    }
                }
                continue;
            }
            if let Some(end) = find_tag_end(html, i) {
                let tag = &html[i..=end];
                match table_tag_replacement(tag) {
                    Some(replacement) => out.push_str(&replacement),
                    None => out.push_str(tag),
                }
                i = end + 1;
                continue;
            }
        }
        // 不是标签开头：按字符拷（按字节会劈开 UTF-8）。
        let ch = html[i..].chars().next().unwrap_or('<');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 表格族标签 → `<div>` / `</div>`；其它标签返回 `None`（原样保留）。
///
/// 属性不再整段丢弃：`style` 现在承载块级背景 / 内边距 / 对齐，文档模型**已经能
/// 渲染它**（`BlockNode::Root` 的块级样式），所以原样带过去；传统属性
/// `bgcolor` / `align` 也翻成等价声明。仍然丢弃的只有 `width` `height`
/// `cellspacing` `role="presentation"` 之类——在拍平后的堆叠布局里本就没有意义。
///
/// `<center>` 额外带上 `text-align:center`，否则拍成 `<div>` 就把居中丢了。
fn table_tag_replacement(tag: &str) -> Option<String> {
    let (body, closing) = match tag.strip_prefix("</") {
        Some(rest) => (rest, true),
        None => (tag.strip_prefix('<')?, false),
    };
    let body = body.trim_end_matches('>');
    let name: String = body
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    match name.as_str() {
        "table" | "tbody" | "thead" | "tfoot" | "tr" | "td" | "th" | "center" => Some(if closing {
            "</div>".to_string()
        } else {
            format!("<div{}>", div_style_attr(body, name == "center"))
        }),
        _ => None,
    }
}

/// 把拍平后仍有意义的样式属性拼成 `<div>` 的 `style` 属性文本（含前导空格）。
///
/// 顺序即优先级：`<center>` 的居中最先，传统属性居中，标签自带的 `style` 最后
/// ——同一份 `style` 里后者胜出，与 CSS 一致。
fn div_style_attr(body: &str, force_center: bool) -> String {
    let attrs = tag_attrs(body);
    let value_of = |name: &str| {
        attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.trim())
    };

    let mut declarations: Vec<String> = Vec::new();
    if force_center {
        declarations.push("text-align:center".to_string());
    }
    // 传统居中属性（`<td align="center">` 是营销邮件里最常见的居中手段）。
    if let Some(align) = value_of("align")
        && matches!(
            align.to_ascii_lowercase().as_str(),
            "left" | "center" | "right"
        )
    {
        declarations.push(format!("text-align:{}", align.to_ascii_lowercase()));
    }
    // 传统背景属性。
    if let Some(bgcolor) = value_of("bgcolor")
        && !bgcolor.is_empty()
    {
        declarations.push(format!("background-color:{}", bgcolor));
    }
    // 标签自带的 style 原样保留（去掉末尾分号，拼起来才不出现 `;;`）。
    if let Some(style) = value_of("style") {
        let style = style.trim_end_matches(';');
        if !style.is_empty() {
            declarations.push(style.to_string());
        }
    }

    if declarations.is_empty() {
        String::new()
    } else {
        format!(" style=\"{}\"", declarations.join(";"))
    }
}

/// 按原始字节扫一遍标签体，取出 `name="value"` / `name='value'` / 裸 `name`。
///
/// 只用 ASCII 做分隔符（空白、`=`、引号），多字节字符必然整段落在名字或值里，
/// 切片不会落到字符边界外。
fn tag_attrs(body: &str) -> Vec<(String, String)> {
    let bytes = body.as_bytes();
    let mut attrs: Vec<(String, String)> = Vec::new();
    let mut i = 0usize;
    let mut first_token = true;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        if name_start == i {
            i += 1;
            continue;
        }
        // 首个 token 是标签名本身，不是属性。
        let is_tag_name = first_token;
        first_token = false;
        let name = body[name_start..i].to_ascii_lowercase();

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            // 裸属性（`disabled`）没有值。
            if !is_tag_name {
                attrs.push((name, String::new()));
            }
            continue;
        }
        i += 1; // `=`
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
            let quote = bytes[i];
            i += 1;
            let value_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let value = body[value_start..i].to_string();
            if i < bytes.len() {
                i += 1;
            }
            value
        } else {
            let value_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            body[value_start..i].to_string()
        };
        if !is_tag_name {
            attrs.push((name, value));
        }
    }

    attrs
}

/// 扫 `<img>`：返回 `(可能被改写后的 HTML, 是否出现过远程地址)`。
///
/// 按**字节**遍历而不是先摊成 `Vec<char>`：HTML 的标签结构符（`<` `>` `"` `'`）全是
/// ASCII，字节下标与字符下标在它们身上一致，省掉一次全量分配。要注意的只有一处——
/// 复制「非标签」的普通文本时必须整段 `push_str`，不能逐字节 `push`，否则多字节字符
/// 会被切断（切片落在非字符边界上会 panic）。
fn scan_images(html: &str, block_remote: bool) -> (String, bool) {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut remote = false;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            // 一次抄到下一个 `<` 为止。
            let next = html[i..].find('<').map_or(bytes.len(), |offset| i + offset);
            out.push_str(&html[i..next]);
            i = next;
            continue;
        }
        let Some(end) = find_tag_end(html, i) else {
            // 没有闭合的 `>`：从这里到末尾都不是标签，整段原样留下。
            out.push_str(&html[i..]);
            break;
        };
        let raw = &html[i + 1..end];
        let body = raw.trim_start();
        // 闭合标签（`</img>`）与注释不在这里判。
        if !body.starts_with('/') && tag_name(body) == "img" && img_is_remote(raw) {
            remote = true;
            if block_remote {
                i = end + 1;
                continue;
            }
        }
        out.push_str(&html[i..=end]);
        i = end + 1;
    }
    (out, remote)
}

/// 标签名（小写）。`body` 是 `<` 之后的内容。
fn tag_name(body: &str) -> String {
    body.chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// 找标签的结束 `>`；引号里的 `>` 不算。找不到说明这不是标签（返回 `None`）。
///
/// `start` 是 `<` 的字节下标，返回值是 `>` 的字节下标——[`scan_images`] 与
/// [`rewrite_cid_src`] 共用同一个标签边界口径，避免两处对「标签在哪结束」有分歧。
fn find_tag_end(html: &str, start: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut quote = 0u8;
    for (offset, &byte) in bytes[start + 1..].iter().enumerate() {
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = byte,
            b'>' => return Some(start + 1 + offset),
            _ => {}
        }
    }
    None
}

/// 这个 `<img>` 的 `src` / `srcset` 指向远程资源吗。
fn img_is_remote(tag: &str) -> bool {
    if let Some(src) = attr_value(tag, "src")
        && is_remote_source(&src)
    {
        return true;
    }
    // `srcset` 是「url [descriptor], url [descriptor]」列表。
    attr_value(tag, "srcset").is_some_and(|srcset| {
        srcset.split(',').any(|candidate| {
            let url = candidate.split_whitespace().next().unwrap_or("");
            is_remote_source(url)
        })
    })
}

fn is_remote_source(value: &str) -> bool {
    let value = value.trim();
    if value.starts_with("//") {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// 取标签内某个属性的值（带引号与不带引号两种写法都认）。
fn attr_value(tag: &str, wanted: &str) -> Option<String> {
    attr_value_span(tag, wanted).map(|(_, value)| value)
}

/// 取标签内某个属性的**值 token 的字节区间**（区间含两侧引号）与解引号后的值。
///
/// 区间是给「原地替换属性值」用的：[`rewrite_cid_src`] 用它把 `src="cid:…"` 换成
/// `src="data:…"` 而完全不动标签里的其它属性。
///
/// 刻意不做完整 HTML 解析：输入是**已经过 `mail-parser` 解码**的正文，这里只需要
/// 从 `<img …>` 的字节里认出 `src`。`to_ascii_lowercase` 不改字节长度，所以
/// 小写副本上的下标可以直接用来切原文。
fn attr_value_span(tag: &str, wanted: &str) -> Option<(std::ops::Range<usize>, String)> {
    let lower = tag.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0usize;

    while let Some(rel) = lower[from..].find(wanted) {
        let at = from + rel;
        from = at + wanted.len();

        // 属性名的左边界：不能是另一个名字的一部分（`data-src` 里的 `src` 不算）。
        if at > 0 {
            let prev = bytes[at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'-' || prev == b'_' {
                continue;
            }
        }
        // 名字后面要跟 `=`（中间允许空白）。
        let mut j = at + wanted.len();
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if bytes.get(j) != Some(&b'=') {
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() {
            return None;
        }

        let quote = bytes[j];
        if quote == b'"' || quote == b'\'' {
            let start = j + 1;
            let mut k = start;
            while k < bytes.len() && bytes[k] != quote {
                k += 1;
            }
            // 引号没闭合：这不是一个能安全改写的值，当作没找到（否则区间会指到标签
            // 外面，替换时切坏原文）。
            if k >= bytes.len() {
                return None;
            }
            return Some((j..k + 1, tag[start..k].to_string()));
        }
        let start = j;
        let mut k = start;
        while k < bytes.len() && !bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        return Some((start..k, tag[start..k].to_string()));
    }
    None
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 把 HTML 正文压成纯文本，供**不用 HTML 渲染**的场合显示。
///
/// # 这为什么是内核的活，不是界面的活
///
/// 它是**解码**而不是渲染：输入是解码后的 HTML（字符集/传输编码已经由
/// `mail-parser` 处理过），输出是给人读的文本。放内核才能单测——而这里恰恰全是
/// 坑：`<script>` 里的 JS 必须整段丢掉（否则会像正文一样印出来）、`<br>` 与
/// `</p>` 必须变成换行（否则整封信挤成一行）、`&nbsp;` 之类实体必须还原。
///
/// **不做的事**：不保留任何格式、不解析表格布局、不还原链接地址。
///
/// 两处调用口径（`pages/mail.rs`）：`multipart/alternative` 邮件默认给纯文本
/// （§4.8 的「更安全」），用户可一键切 HTML；只有 HTML 版本的邮件直接走
/// `TextView::html`。所以本函数是**用户偏好 / 渲染降级**时的路径，不是唯一路径。
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

/// 归一化 `Content-ID`：去首尾空白与包裹的尖括号。
///
/// ⚠️ **两侧共用同一个函数**：解析附件时用它把 `Content-ID: <logo@x>` 存成
/// `logo@x`；替换 `cid:` 引用时用它把 `cid:<logo@x>` 归一化成同一个键。少了一侧
/// 就会出现「附件明明在、图就是不显示」——两边各自 trim 的口径一不一致，只有共用
/// 才能保证匹配（对照 Flectar Mail 的 `normalize_cid`，同一口径）。
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

    /// 回复信：`References` 是祖先链、`In-Reply-To` 是直接父，两者都要抽出来。
    const REPLY: &[u8] = b"From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: Re: topic\r\n\
Message-ID: <c@example.com>\r\n\
In-Reply-To: <b@example.com>\r\n\
References: <a@example.com> <b@example.com>\r\n\
Date: Tue, 12 Oct 2026 10:00:00 +0800\r\n\
\r\n\
body\r\n";

    #[test]
    fn thread_headers_extract_references_and_in_reply_to() {
        let h = parse_headers(REPLY).unwrap();
        assert_eq!(h.message_id.as_deref(), Some("c@example.com"));
        assert_eq!(h.in_reply_to.as_deref(), Some("b@example.com"));
        // `References` 原样保留，`In-Reply-To` 已在链里 → 不重复追加。
        assert_eq!(
            h.references,
            vec!["a@example.com".to_string(), "b@example.com".to_string()]
        );
    }

    #[test]
    fn in_reply_to_is_appended_when_references_absent() {
        // 有些客户端只写 `In-Reply-To`、不写 `References`——不能因此丢掉线索。
        const ONLY_IN_REPLY_TO: &[u8] = b"From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: Re: topic\r\n\
Message-ID: <c@example.com>\r\n\
In-Reply-To: <b@example.com>\r\n\
\r\n\
body\r\n";
        let h = parse_headers(ONLY_IN_REPLY_TO).unwrap();
        assert_eq!(h.in_reply_to.as_deref(), Some("b@example.com"));
        assert_eq!(h.references, vec!["b@example.com".to_string()]);
    }

    #[test]
    fn missing_thread_headers_yield_empty() {
        let h = parse_headers(SIMPLE).unwrap();
        assert_eq!(h.in_reply_to, None);
        assert!(h.references.is_empty());
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
    fn attachments_carry_a_usable_imap_section() {
        // 顶层 multipart/mixed：正文是 1、附件是 2。
        let m = parse(WITH_ATTACHMENT).unwrap();
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].imap_section.as_deref(), Some("2"));
    }

    #[test]
    fn nested_multipart_sections_are_dotted() {
        // multipart/mixed（顶层）→ 1 = multipart/alternative（1.1 纯文本 / 1.2 HTML）、
        // 2 = 附件。
        const NESTED: &[u8] = b"From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: nested\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"MIX\"\r\n\
\r\n\
--MIX\r\n\
Content-Type: multipart/alternative; boundary=\"ALT\"\r\n\
\r\n\
--ALT\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
plain\r\n\
--ALT\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>html</p>\r\n\
--ALT--\r\n\
--MIX\r\n\
Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--MIX--\r\n";
        let m = parse(NESTED).unwrap();
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].imap_section.as_deref(), Some("2"));
    }

    #[test]
    fn section_of_a_single_part_message_is_one() {
        let m = parse(SIMPLE).unwrap();
        // 没有附件；直接验走查器的核心结论：单部件正文是节 1。
        let sections = section_map(&MessageParser::default().parse(SIMPLE).unwrap());
        assert_eq!(sections.get(&0).map(String::as_str), Some("1"));
        assert!(m.attachments.is_empty());
    }

    #[test]
    fn decode_attachment_part_undoes_base64_using_the_mime_headers() {
        // 服务端按 `BODY.PEEK[2.MIME]` + `BODY.PEEK[2]` 回的原始字节：
        // MIME 头声明 base64，正文是 base64 文本。
        let headers = b"Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n";
        let body = b"JVBERi0xLjQK";
        let decoded = decode_attachment_part(headers, body).expect("decode");
        assert_eq!(decoded, b"%PDF-1.4\n");
        // 没有 MIME 头就拿不到传输编码 → 拒绝解码（调用方降级为整封取回），
        // 绝不把 base64 文本当附件写盘。
        assert_eq!(decode_attachment_part(b"", body), None);
    }

    #[test]
    fn decode_attachment_part_passes_through_seven_bit_text() {
        let headers =
            b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n";
        let decoded = decode_attachment_part(headers, b"hello\r\n").expect("decode");
        assert_eq!(decoded, b"hello\r\n");
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
            imap_section: None,
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
            imap_section: None,
        };
        let html = "<a href=\"cid:doc@x\">x</a>";
        assert_eq!(inline_cid_images(html, &[att], 1024), html);
    }

    #[test]
    fn inlining_stops_at_the_image_count_cap() {
        // 第 33 张（下标 32）起不再内联——不能被一封伪造出几百个 CID 的邮件拖住。
        let attachments: Vec<ParsedAttachment> = (0..MAX_INLINE_IMAGES + 1)
            .map(|i| ParsedAttachment {
                filename: format!("{i}.png"),
                mime_type: "image/png".into(),
                size: 1,
                content_id: Some(format!("img{i}@x")),
                is_inline: true,
                bytes: vec![0u8],
                imap_section: None,
            })
            .collect();
        let html: String = (0..MAX_INLINE_IMAGES + 1)
            .map(|i| format!("<img src=\"cid:img{i}@x\">"))
            .collect();
        let out = inline_cid_images(&html, &attachments, usize::MAX);
        // 前 32 张变成 data:，第 33 张原样留着 `cid:`（渲染层给占位）。
        assert_eq!(
            out.matches("data:image/png;base64,").count(),
            MAX_INLINE_IMAGES
        );
        assert!(out.contains(&format!("cid:img{MAX_INLINE_IMAGES}@x")));
    }

    #[test]
    fn remote_images_are_detected_and_stripped() {
        let html = "<p>a</p><img src=\"https://tracker.example/px.gif\"><p>b</p>";
        assert!(has_remote_images(html));
        let stripped = strip_remote_images(html);
        assert_eq!(stripped, "<p>a</p><p>b</p>");
        // 协议相对地址同样算远程。
        assert!(has_remote_images("<img src=//cdn.example/logo.png>"));
        // 已经内联的 data: 与相对路径都不算远程，且必须原样留下。
        for local in [
            "<img src=\"data:image/png;base64,AAAA\">",
            "<img src=\"logo.png\">",
            "<img src=\"/static/logo.png\">",
            "<img src=\"cid:logo@x\">",
        ] {
            assert!(
                !has_remote_images(local),
                "{local} should not count as remote"
            );
            assert_eq!(strip_remote_images(local), local);
        }
    }

    #[test]
    fn remote_images_only_looking_at_img_tags() {
        // 只有 `<img>` 会被拦；链接与表单里的远程地址不动（它们不是图片闸门的活）。
        let html = "<a href=\"https://example.com\">x</a>";
        assert!(!has_remote_images(html));
        assert_eq!(strip_remote_images(html), html);
        // `data-src` 不是 `src`，不该被误判成属性名的一部分。
        assert!(!has_remote_images("<img data-src=\"logo.png\">"));
    }

    #[test]
    fn stripping_tolerates_awkward_attribute_syntax() {
        // 属性名与值之间带空白、单引号、无引号三种写法都要认。
        assert!(has_remote_images("<img src = 'http://a/b.png'>"));
        assert!(has_remote_images("<img src=http://a/b.png>"));
        // 引号里的 `>` 不能把标签提前切断。
        let html = "<img alt=\"a > b\" src=\"https://a/b.png\">";
        assert!(has_remote_images(html));
        assert_eq!(strip_remote_images(html), "");
        // `srcset` 也拦。
        assert!(has_remote_images(
            "<img srcset=\"https://a/s.png 1x, https://a/l.png 2x\">"
        ));
    }

    #[test]
    fn stripping_leaves_broken_markup_alone() {
        // 未闭合的 `<`、没结束的标签都不该被吞掉。
        assert_eq!(strip_remote_images("3 < 5"), "3 < 5");
        assert_eq!(
            strip_remote_images("<img src=\"http://a/b.png\""),
            "<img src=\"http://a/b.png\""
        );
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

    #[test]
    fn flatten_tables_turns_the_whole_family_into_divs() {
        let html = "<table width=\"600\"><tbody><tr><td style=\"padding:8px\">A</td>\
                    <td>B</td></tr></tbody></table>";
        assert_eq!(
            flatten_email_tables(html),
            "<div><div><div><div style=\"padding:8px\">A</div><div>B</div></div></div></div>"
        );
    }

    #[test]
    fn flatten_tables_handles_uppercase_tags_and_center() {
        let html = "<CENTER><TABLE><TR><TD>x</TD></TR></TABLE></CENTER>";
        assert_eq!(
            flatten_email_tables(html),
            "<div style=\"text-align:center\"><div><div><div>x</div></div></div></div>"
        );
    }

    #[test]
    fn flatten_tables_keeps_block_style_and_legacy_attributes() {
        // 拍平后块级背景 / 内边距 / 对齐还要靠这些属性，绝不能丢：
        // `style` 原样带过去，传统 `bgcolor` / `align` 翻成等价声明，
        // 且自带的 `style` 排在后面（同一份声明里后者胜出）。
        let html = "<table bgcolor=\"#ffffff\" align=\"center\"><tr>\
                    <td style=\"padding:0 16px;text-align:right\" align=\"left\">A</td>\
                    </tr></table>";
        assert_eq!(
            flatten_email_tables(html),
            "<div style=\"text-align:center;background-color:#ffffff\"><div>\
             <div style=\"text-align:left;padding:0 16px;text-align:right\">A</div>\
             </div></div>"
        );
    }

    #[test]
    fn flatten_tables_drops_meaningless_attributes_only() {
        // `width` / `cellspacing` 在堆叠布局里没有意义，继续丢；没有样式就
        // 不生成空的 `style=""`。
        let html =
            "<table width=\"600\" cellspacing=\"0\"><tr><td valign=\"top\">x</td></tr></table>";
        assert_eq!(
            flatten_email_tables(html),
            "<div><div><div>x</div></div></div>"
        );
    }

    #[test]
    fn flatten_tables_leaves_non_table_markup_and_comments_alone() {
        // 条件注释里的表格是给 Outlook 的降级布局，整段原样保留。
        let html = "<!--[if mso]><table><tr><td>w</td></tr></table><![endif]-->\
                    <p>hello</p><span>tail</span><img src=\"https://x/y.png\">";
        assert_eq!(flatten_email_tables(html), html);
    }

    #[test]
    fn flatten_tables_keeps_closing_tags_balanced() {
        // 没有表格族标签的 HTML 必须原样（含引号里的 `>`）。
        let html = "<a href=\"https://x?a>b\">link</a> 正文";
        assert_eq!(flatten_email_tables(html), html);
    }

    /// `multipart/related` 内联图邮件：HTML 正文用 `cid:` 引用，图片部件带
    /// `Content-ID: <logo@x>`。`disposition` 参数用来变更那个部件的
    /// `Content-Disposition` 行（含行尾 CRLF；空串表示压根不写这一行）。
    fn related_with_disposition(disposition: &str) -> Vec<u8> {
        format!(
            "From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: img\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/related; boundary=\"REL\"\r\n\
\r\n\
--REL\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<img src=\"cid:logo@x\">\r\n\
--REL\r\n\
Content-Type: image/png\r\n\
Content-ID: <logo@x>\r\n\
{disposition}Content-Transfer-Encoding: base64\r\n\
\r\n\
iVBORw0KGgo=\r\n\
--REL--\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn explicit_attachment_wins_over_content_id() {
        // Gmail / Exchange 给**每个**部件都盖 `Content-ID`，包括普通文件。显式
        // `attachment` 必须压过它——否则真附件会被当成内联图，列表里既没有回形针
        // 也没有附件芯片。
        let m = parse(&related_with_disposition(
            "Content-Disposition: attachment\r\n",
        ))
        .unwrap();
        assert_eq!(m.attachments.len(), 1);
        assert!(!m.attachments[0].is_inline);
        // `Content-ID` 照样要抽出来（下载 / 匹配仍可能用到）。
        assert_eq!(m.attachments[0].content_id.as_deref(), Some("logo@x"));
    }

    #[test]
    fn explicit_inline_is_inline() {
        let m = parse(&related_with_disposition("Content-Disposition: inline\r\n")).unwrap();
        assert_eq!(m.attachments.len(), 1);
        assert!(m.attachments[0].is_inline);
    }

    #[test]
    fn bare_content_id_counts_as_inline() {
        // 回归：不少 MUA（老 Outlook、部分网易系客户端）的 `cid:` 图片**只写
        // `Content-ID`、不写 `Content-Disposition`**。旧口径它们全是 `false` →
        // `inline_cid_images` 一张都不换 → 正文里只剩 `cid:…` 死链。
        let m = parse(&related_with_disposition("")).unwrap();
        assert_eq!(m.attachments.len(), 1);
        assert!(
            m.attachments[0].is_inline,
            "a part with a Content-ID and no disposition is an inline image"
        );
    }

    #[test]
    fn part_without_disposition_or_content_id_is_not_inline() {
        // 反向边界：既没有 `Content-Disposition` 也没有 `Content-ID` 的部件是普通
        // 附件，不能被算成内联图。
        const BARE_PART: &[u8] = b"From: a@example.com\r\n\
To: b@example.com\r\n\
Subject: bare\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"BARE\"\r\n\
\r\n\
--BARE\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
body\r\n\
--BARE\r\n\
Content-Type: application/pdf; name=\"x.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--BARE--\r\n";
        let m = parse(BARE_PART).unwrap();
        assert_eq!(m.attachments.len(), 1);
        assert!(!m.attachments[0].is_inline);
        assert_eq!(m.attachments[0].content_id, None);
    }

    /// 一张内联 PNG 附件（字节是真的 PNG 魔数，好让 MIME 归一化不必改类型）。
    fn inline_png(cid: &str) -> ParsedAttachment {
        ParsedAttachment {
            filename: "logo.png".into(),
            mime_type: "image/png".into(),
            size: 8,
            content_id: Some(cid.into()),
            is_inline: true,
            bytes: b"\x89PNG\r\n\x1a\n".to_vec(),
            imap_section: None,
        }
    }

    #[test]
    fn cid_replacement_is_case_insensitive_and_touches_only_src() {
        let att = inline_png("logo@x");
        // ① 属性名写 `SRC`、值里写 `CID:` 都是合法 HTML，必须认。
        let out = inline_cid_images("<IMG SRC=\"CID:logo@x\">", std::slice::from_ref(&att), 4096);
        assert!(out.contains("data:image/png;base64,"), "{out}");

        // ② `href` 里的 `cid:` 不动——那是「引用另一封邮件里的附件」的链接，改成
        //    内联图会把链接文本变成一整张 base64 图。
        let html = "<a href=\"cid:logo@x\">file</a>";
        assert_eq!(
            inline_cid_images(html, std::slice::from_ref(&att), 4096),
            html
        );

        // ③ 引用侧带尖括号（`cid:<logo@x>`）也要匹配——两侧都过 `trim_cid`。
        let out = inline_cid_images(
            "<img src=\"cid:<logo@x>\">",
            std::slice::from_ref(&att),
            4096,
        );
        assert!(out.contains("data:image/png;base64,"), "{out}");
    }

    #[test]
    fn cid_replacement_leaves_unknown_cids_alone() {
        let att = inline_png("logo@x");
        // 不认识的 cid 原样保留（渲染层给空白，但不能改坏文本）。
        let html = "<img src=\"cid:other@y\">";
        assert_eq!(inline_cid_images(html, &[att], 4096), html);
    }

    #[test]
    fn image_mime_is_normalized_into_the_gpui_table() {
        // 已在 gpui 值域内的：原样（含它的两个别名）。
        assert_eq!(normalize_image_mime("image/png", b""), "image/png");
        assert_eq!(normalize_image_mime(" IMAGE/JPEG ", b""), "image/jpeg");
        assert_eq!(normalize_image_mime("image/jpg", b""), "image/jpg");
        assert_eq!(normalize_image_mime("image/tif", b""), "image/tif");
        // 不在值域里的别名 → 映射到 gpui 认得的规范名。
        assert_eq!(normalize_image_mime("image/x-png", b""), "image/png");
        assert_eq!(normalize_image_mime("image/x-icon", b""), "image/ico");
        assert_eq!(
            normalize_image_mime("image/vnd.microsoft.icon", b""),
            "image/ico"
        );
        assert_eq!(normalize_image_mime("image/svg", b""), "image/svg+xml");
        assert_eq!(normalize_image_mime("image/pjpeg", b""), "image/jpeg");
    }

    #[test]
    fn unknown_image_mime_falls_back_to_magic_bytes() {
        // `application/octet-stream` 是「部件没写 `Content-Type`」时的回落值——真实
        // 邮件里这种内联图（尤其网易 / 老客户端的转存件）并不少见，只能靠魔数认。
        let cases: [(&[u8], &str); 9] = [
            (b"\x89PNG\r\n\x1a\n", "image/png"),
            (b"\xFF\xD8\xFF\xE0", "image/jpeg"),
            (b"GIF89a", "image/gif"),
            (b"RIFF\x00\x00\x00\x00WEBP", "image/webp"),
            (b"BM\x00\x00", "image/bmp"),
            (b"II*\x00", "image/tiff"),
            (b"\x00\x00\x01\x00", "image/ico"),
            (b"P6\n1 1\n255\n", "image/x-portable-anymap"),
            (b"<?xml version=\"1.0\"?><svg/>", "image/svg+xml"),
        ];
        for (magic, expected) in cases {
            assert_eq!(
                normalize_image_mime("application/octet-stream", magic),
                expected,
                "{magic:?}"
            );
            // 空的声明串（真出现时上层会给 octet-stream，但这一路也要稳）。
            assert_eq!(normalize_image_mime("", magic), expected, "{magic:?}");
        }
    }

    #[test]
    fn unpredictable_bytes_keep_the_declared_mime() {
        // 嗅不出来就原样返回——不谎报类型（谎报会让渲染层报一个更离谱的错）。
        assert_eq!(
            normalize_image_mime("application/pdf", b"%PDF-1.4"),
            "application/pdf"
        );
    }

    #[test]
    fn every_emitted_mime_is_in_the_gpui_table() {
        // 契约测试：`normalize_image_mime` 的输出只要落在 `GPUI_DECODABLE_IMAGE_MIMES`
        // 之外，渲染层就会「认不出 → 整张图放弃」（空白）。gpui 升级/换 rev 后若那张
        // 表变了，这里必须一起改——本 crate 不依赖 gpui，编译器不会替我们查。
        let declared = [
            "image/png",
            "image/jpeg",
            "image/jpg",
            "image/webp",
            "image/gif",
            "image/bmp",
            "image/tiff",
            "image/tif",
            "image/ico",
            "image/x-portable-anymap",
            "image/svg+xml",
            "image/x-png",
            "image/x-icon",
            "image/vnd.microsoft.icon",
            "image/svg",
            "image/pjpeg",
            "image/x-citrix-jpeg",
            "application/octet-stream",
            "",
            "IMAGE/PNG",
        ];
        for name in declared {
            let out = normalize_image_mime(name, b"\x89PNG\r\n\x1a\n");
            assert!(
                GPUI_DECODABLE_IMAGE_MIMES.contains(&out.as_str()),
                "{name:?} → {out:?} is outside the GPUI-decodable table"
            );
        }
    }
}
