//! 发信方向的消息装配（RFC 5322 头部 + RFC 2047 编码 + `multipart/mixed` 正文）。
//!
//! # 头部仍自己拼，正文的 `multipart` 交给 `mail-builder`
//!
//! **头部**（`Date` … `MIME-Version`）整段是本模块手写的，因为有三件最容易出错的
//! 事在这里被做成了**可断言的纯函数**：
//!
//! 1. **头部注入**：主题 / 显示名里的 `\r\n` 能凭空插出一个 `Bcc:` 头。本模块
//!    一律**拒绝**（[`MailError::Config`]），而不是悄悄替换——用户看不到的静默
//!    改写比报错危险。
//! 2. **非 ASCII 头部**：中文主题必须走 RFC 2047 的 `=?UTF-8?B?…?=`，而且单个
//!    encoded-word 不得超长（否则被服务器拒），所以按 UTF-8 字符边界切块。
//! 3. **折行**：头部超长要在空白处折，且 `encoded-word` 之间是合法折点——
//!    解码方会把折行空白吃掉，正好拼回原文。
//!
//! 还有两条只有在这一层才能保证的：**`Bcc` 绝不进头部**（只进信封，见
//! [`envelope_recipients`]）；**无附件时输出逐字节固定**（同一封信两次装配必须一样，
//! 否则同步逻辑一出问题就会让人怀疑是装配在抖）。
//!
//! **正文**只在**有附件**时才交给 `mail-builder` 的 `mime::MimePart`。`multipart`
//! 的难处从来不是「拼 boundary」，而是**每个部件怎么编码**：base64 的 76 列折行、
//! 非 ASCII 文件名在 `Content-Disposition` 里的编码、按内容自动选编码。这些写错
//! 不会报错，只会让**对方**收到一个坏附件——正是该交给库的部分。无附件时**仍走本
//! 模块自己的单部件路径**（`text/plain` + `8bit`）：那条路径的输出已被测试逐字节
//! 钉住，没有理由换掉它，也就没有理由让「不带附件的邮件」多担一层依赖风险。
//!
//! 正文体的点填充（SMTP `DATA` 要求）不在这里，那是传输层的事，
//! 见 [`crate::proto::smtp`]。

use mail_builder::mime::{BodyPart, MimePart};

use crate::error::{MailError, Result};

/// 单个 encoded-word 里允许的最大**原始字节数**。
///
/// RFC 2047 规定一个 encoded-word 连包装在内不超过 75 字符。
/// 包装是 `=?UTF-8?B?`（10）+ `?=`（2）= 12，base64 每 3 字节出 4 字符，
/// 所以原始字节上限 = `(75 - 12) / 4 * 3` = 47。取 45 留余量，
/// 也保证 `len % 3` 时的尾部 padding 不会顶破。
const ENCODED_WORD_MAX_RAW: usize = 45;

/// 头部单行的软上限（RFC 5322 建议 78，硬上限 998）。
const HEADER_FOLD_WIDTH: usize = 78;

/// 一个邮箱地址（含可选显示名）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// 显示名。非 ASCII 会在头部里走 RFC 2047 编码。
    pub name: Option<String>,
    /// 地址（`local@domain`）。
    pub address: String,
}

impl Mailbox {
    /// 只有地址。
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            name: None,
            address: address.into(),
        }
    }

    /// 带显示名。
    pub fn with_name(address: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            address: address.into(),
        }
    }

    /// 头部里的写法：`显示名 <地址>`；没有显示名时只剩地址。
    ///
    /// 不含折行——折行由 [`fold_header`] 统一做（这里再折一次会折两遍）。
    pub fn header_value(&self) -> Result<String> {
        let address = check_header_safe("address", &self.address)?;
        if address.trim().is_empty() {
            return Err(MailError::Config("recipient address is empty".into()));
        }
        // 地址里出现尖括号会把 `Name <a@b>` 的结构弄坏（`<a@b> <c@d>` 之类）。
        if address.contains(['<', '>']) {
            return Err(MailError::Config(format!(
                "address contains angle brackets: {address}"
            )));
        }
        match &self.name {
            None => Ok(address),
            Some(name) if name.trim().is_empty() => Ok(address),
            Some(name) => {
                let name = check_header_safe("display name", name)?;
                Ok(format!(
                    "{} <{}>",
                    encode_header_text(name.trim_matches(' ')),
                    address
                ))
            }
        }
    }
}

/// 一个待发附件。
///
/// 存的是**原始字节**（不是编码后的）：编码是装配层的事，`build_message` 把它交给
/// `mail-builder` 做 base64。这样同一个附件既能进 MIME，将来也能原样写进本地草稿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    /// 收件人看到的文件名（`Content-Disposition` 的 `filename`）。非 ASCII 交给
    /// `mail-builder` 编码——实测它走 RFC 2047 的 encoded-word（`filename="=?utf-8?B?…?="`），
    /// 这是各家客户端事实上的通行做法（RFC 2231 的续行参数反而是少数派）。
    pub filename: String,
    /// `Content-Type` 的值。由扩展名猜，认不出来是 `application/octet-stream`。
    pub content_type: String,
    /// 文件原始字节（**未编码**）。
    pub bytes: Vec<u8>,
}

impl OutgoingAttachment {
    /// 直接构造（内容已在内存里时用，测试也走这条）。
    pub fn new(
        filename: impl Into<String>,
        content_type: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            filename: filename.into(),
            content_type: content_type.into(),
            bytes,
        }
    }

    /// 从磁盘读一个附件；文件名取路径的最后一段。
    ///
    /// 读盘放在**内核**而不是界面：这样「文件没了 / 读不出来」与「装配失败」是同一类
    /// 错误（都带 `code()`），界面只需要一条 `trf` 文案，不必自己去拼 `io::Error`。
    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                MailError::Config(format!(
                    "attachment path has no file name: {}",
                    path.display()
                ))
            })?;
        let bytes = std::fs::read(path).map_err(|error| {
            MailError::Io(format!(
                "cannot read attachment {}: {error}",
                path.display()
            ))
        })?;
        Ok(Self {
            content_type: guess_content_type(&filename).to_owned(),
            filename,
            bytes,
        })
    }

    /// 原始体积（字节）。界面用 [`crate::compose::OutgoingAttachment::bytes`] 的长度
    /// 自己格式化即可，这个方法只是给测试与日志一个稳定的口径。
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// 空附件（0 字节）。**不算错误**：空文件是合法的，只是要显式区分。
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// 扩展名 → MIME 类型。**只覆盖常见档**，认不出来一律
/// `application/octet-stream`（RFC 2046 规定的「未知二进制」）。
///
/// 用途是「让对方的客户端知道该拿什么打开」，不是安全边界——**扩展名永远不可信**。
/// 刻意不查内容（那要靠 magic bytes，本模块没有），因为收件方真正依赖的是自己那边的
/// 类型识别，而不是我们声明的那一行。
fn guess_content_type(filename: &str) -> &'static str {
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "ics" => "text/calendar",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "eml" => "message/rfc822",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "heic" => "image/heic",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// 一封待发邮件（纯文本正文 + 可选附件）。
///
/// `date_unix` 与 `message_id` **由调用方给**，不在本层生成——这样装配过程是
/// 纯函数，测试可以钉死输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// 发件人（同时用于 `From:` 头）。
    pub from: Mailbox,
    /// 收件人。
    pub to: Vec<Mailbox>,
    /// 抄送。
    pub cc: Vec<Mailbox>,
    /// 密送。**进信封、不进头部**（这就是「密」的含义）。
    pub bcc: Vec<Mailbox>,
    /// 主题（可空）。非 ASCII 走 RFC 2047。
    pub subject: String,
    /// 纯文本正文（LF 或 CRLF 都行，装配时统一成 CRLF）。
    pub body: String,
    /// 回复哪一封（`In-Reply-To:`）。
    pub in_reply_to: Option<String>,
    /// 引用链（`References:`）。
    pub references: Vec<String>,
    /// 发信时刻（Unix 秒），格式化成 `Date:` 头。
    pub date_unix: i64,
    /// 本封的 `Message-ID:`（**要含尖括号还是不含？** 本层收不含的裸值，输出时补上）。
    pub message_id: String,
    /// 随信发出的附件（**空的就发纯文本单部件**，见 [`build_message`]）。
    pub attachments: Vec<OutgoingAttachment>,
}

impl Draft {
    /// 最小可用的一封：发件人 + 一个收件人 + 主题 + 正文。
    pub fn new(
        from: Mailbox,
        to: Mailbox,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            from,
            to: vec![to],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.into(),
            body: body.into(),
            in_reply_to: None,
            references: Vec::new(),
            date_unix: 0,
            message_id: String::new(),
            attachments: Vec::new(),
        }
    }

    /// 挂一个附件（链式，便于构造 + 装配的测试）。
    #[must_use]
    pub fn with_attachment(mut self, attachment: OutgoingAttachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// 补上 `Date` 与 `Message-ID`（只在调用方没填时）。
    ///
    /// **发信前应调用一次**：没有 `Message-ID` 的信会被不少反垃圾系统降权，
    /// 而没有 `Date` 的信在各客户端的排序会乱。
    pub fn stamp(&mut self, now_unix: i64) {
        if self.date_unix == 0 {
            self.date_unix = now_unix;
        }
        if self.message_id.trim().is_empty() {
            self.message_id = generate_message_id(&self.from.address, now_unix);
        }
    }
}

/// 当前 Unix 秒。
///
/// 与 `db::repo::queue::now_secs` 是同一段代码，但那一处在存储层（它服务的是入队
/// 时间），这里服务的是信头时间——挪成公共依赖会把「协议层不依赖存储层」这条分层
/// 破掉，所以宁可重复五行。
#[must_use]
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 造一个 `Message-ID` 的裸值（不含尖括号）。
///
/// 形态：`<秒>.<纳秒>.<进程内序号>.<pid>@<发件域>`。**不引 uuid 依赖**：这里要的是
/// 「实际不重复」而不是密码学唯一——纳秒 + 单调序号 + pid 已经足够，而且零新增依赖。
/// 带上发件域是因为不少反垃圾策略要求 `Message-ID` 的右半部分与发件域一致。
#[must_use]
pub fn generate_message_id(from_address: &str, now_unix: i64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let domain = match from_address.split_once('@') {
        Some((_, domain)) if !domain.trim().is_empty() => domain.trim(),
        _ => "localhost",
    };
    format!(
        "{now_unix}.{nanos}.{sequence}.{}@{domain}",
        std::process::id()
    )
}

/// 信封收件人 = `To` + `Cc` + `Bcc` 的地址（去重、保序）。
///
/// **`Bcc` 只出现在这里**：`RCPT TO` 要发给它，但头部里绝不能提它——否则密送
/// 就白密了。这个函数与 [`build_message`] 的分工就是「信封 vs 头部」。
#[must_use]
pub fn envelope_recipients(draft: &Draft) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for mailbox in draft.to.iter().chain(&draft.cc).chain(&draft.bcc) {
        let address = mailbox.address.trim().to_owned();
        if address.is_empty() {
            continue;
        }
        if seen.insert(address.clone()) {
            out.push(address);
        }
    }
    out
}

/// 装配整封消息（CRLF 行尾，头部已折行）。
///
/// 头部顺序刻意固定：`Date` / `From` / `To` / `Cc` / `Subject` / `Message-ID` /
/// `In-Reply-To` / `References` / `MIME-Version` / `Content-Type`。固定顺序让测试
/// 能整段比对，也避免「同一封信两次装配字节不同」这种让人怀疑同步逻辑的现象。
///
/// 有附件时 `Content-Type: multipart/mixed; boundary=…` 由 `mail-builder` 写
/// （boundary 是它生成的，必须同源），但**位置不变**——仍紧接 `MIME-Version`。
pub fn build_message(draft: &Draft) -> Result<String> {
    let body = normalize_crlf(&draft.body);

    let mut headers = String::new();
    headers.push_str(&fold_header("Date", &format_date(draft.date_unix)));
    headers.push_str(&fold_header("From", &draft.from.header_value()?));
    headers.push_str(&fold_header("To", &mailbox_list(&draft.to)?));
    if !draft.cc.is_empty() {
        headers.push_str(&fold_header("Cc", &mailbox_list(&draft.cc)?));
    }
    // 主题为空也写出来：留空比整条缺席更好（有些客户端把缺席当「无主题」，
    // 有些当「头部损坏」）。
    headers.push_str(&fold_header(
        "Subject",
        &encode_header_text(&check_header_safe("subject", &draft.subject)?),
    ));
    if !draft.message_id.trim().is_empty() {
        let id = check_header_safe("message id", &draft.message_id)?;
        headers.push_str(&fold_header("Message-ID", &angle_addr(&id)));
    }
    if let Some(in_reply_to) = &draft.in_reply_to {
        let value = check_header_safe("in-reply-to", in_reply_to)?;
        if !value.trim().is_empty() {
            headers.push_str(&fold_header("In-Reply-To", &angle_addr(&value)));
        }
    }
    if !draft.references.is_empty() {
        let mut value = String::new();
        for reference in &draft.references {
            let reference = check_header_safe("references", reference)?;
            if reference.trim().is_empty() {
                continue;
            }
            if !value.is_empty() {
                value.push(' ');
            }
            value.push_str(&angle_addr(&reference));
        }
        if !value.is_empty() {
            headers.push_str(&fold_header("References", &value));
        }
    }
    headers.push_str(&fold_header("MIME-Version", "1.0"));

    // 无附件：单部件 `text/plain` + `8bit`，**输出与引入 `mail-builder` 之前逐字节
    // 相同**（测试钉住）。有附件：顶层 `Content-Type` 一并交给 `mail-builder` 写——
    // 它必须和自己生成的 boundary 一致，分两处写迟早会对不上。
    if draft.attachments.is_empty() {
        headers.push_str(&fold_header("Content-Type", "text/plain; charset=utf-8"));
        headers.push_str(&fold_header("Content-Transfer-Encoding", "8bit"));
        return Ok(format!("{headers}\r\n{body}"));
    }
    Ok(format!("{headers}{}", build_multipart_body(draft, body)?))
}

/// 装配 `multipart/mixed` 正文（**含顶层 `Content-Type`**）。
///
/// 返回的片段从 `Content-Type: multipart/mixed; …` 起、到收尾的 `--<boundary>--`
/// 止，直接接在 [`build_message`] 的头部块之后即可。
fn build_multipart_body(draft: &Draft, body: String) -> Result<String> {
    let mut parts: Vec<MimePart<'static>> = Vec::with_capacity(draft.attachments.len() + 1);
    // 第 0 段是正文。`MimePart::new` 见到 `BodyPart::Text` 且无显式属性时会自动补
    // `charset=utf-8`；至于用 `8bit` 还是 `base64` 由库按内容定（实测含非 ASCII 的
    // 正文会走 base64 —— 这正是「自己写 base64 折行」不划算的地方）。
    parts.push(MimePart::new("text/plain", body));
    for attachment in &draft.attachments {
        parts.push(
            MimePart::new(
                attachment.content_type.clone(),
                BodyPart::Binary(attachment.bytes.clone().into()),
            )
            // `.attachment()` 补 `Content-Disposition: attachment; filename="…"`，
            // 非 ASCII 文件名由库编码。
            .attachment(attachment.filename.clone()),
        );
    }

    let mut out: Vec<u8> = Vec::new();
    MimePart::new("multipart/mixed", BodyPart::Multipart(parts))
        .write_part(&mut out)
        .map_err(|error| {
            MailError::Mime(format!("failed to assemble the multipart body: {error}"))
        })?;
    String::from_utf8(out)
        .map_err(|error| MailError::Mime(format!("multipart body is not valid utf-8: {error}")))
}

/// `To:` / `Cc:` 的值：逗号分隔的地址列表。
fn mailbox_list(mailboxes: &[Mailbox]) -> Result<String> {
    if mailboxes.is_empty() {
        return Err(MailError::Config("recipient list is empty".into()));
    }
    let mut parts = Vec::with_capacity(mailboxes.len());
    for mailbox in mailboxes {
        parts.push(mailbox.header_value()?);
    }
    Ok(parts.join(", "))
}

/// 给裸 `Message-ID` 补尖括号；已经有就原样返回。
fn angle_addr(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        trimmed.to_owned()
    } else {
        format!("<{trimmed}>")
    }
}

/// 头部值不得含 CR / LF（**头部注入**的唯一入口）。
///
/// 返回去掉首尾空白后的值；含控制字符一律 [`MailError::Config`]。
fn check_header_safe(field: &str, value: &str) -> Result<String> {
    if value.contains(['\r', '\n']) {
        return Err(MailError::Config(format!(
            "{field} contains a line break (header injection)"
        )));
    }
    // NUL 与其它 C0 控制字符在头部里没有合法含义。
    if value.chars().any(|c| c.is_control() && c != '\t') {
        return Err(MailError::Config(format!(
            "{field} contains a control character"
        )));
    }
    Ok(value.trim().to_owned())
}

/// 需要时把文本编成 RFC 2047 encoded-word；纯 ASCII 原样返回。
///
/// 超长时按 UTF-8 **字符边界**切成多个 encoded-word，用空格连接——RFC 2047 规定
/// 相邻 encoded-word 之间的空白在解码时被丢弃，所以空格连接能拼回原文。
#[must_use]
pub fn encode_header_text(text: &str) -> String {
    if text.is_ascii() {
        return text.to_owned();
    }
    let mut words: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for ch in text.chars() {
        // `ch.len_utf8()` 保证不会把一个多字节字符切两半。
        if !chunk.is_empty() && chunk.len() + ch.len_utf8() > ENCODED_WORD_MAX_RAW {
            words.push(encode_word(&chunk));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(encode_word(&chunk));
    }
    words.join(" ")
}

/// 一个 `=?UTF-8?B?<base64>?=`。
fn encode_word(text: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("=?UTF-8?B?{encoded}?=")
}

/// `Name: value` + 必要的续行；续行以单个空格开头。
///
/// 折点只在空白处（`encoded-word` 之间、地址之间都正好是空白），所以**不会把
/// 一个词切成两半**。单个 token 超宽时独占一行——折不开比折错好。
fn fold_header(name: &str, value: &str) -> String {
    let mut out = String::new();
    // `"{name}:"` 的长度就是「还没放任何 token」的基线。
    let baseline = name.len() + 1;
    let mut line = format!("{name}:");
    for token in value.split(' ') {
        if token.is_empty() {
            continue;
        }
        // +1 是词间那个空格。
        let projected = line.len() + 1 + token.len();
        if projected > HEADER_FOLD_WIDTH && line.len() > baseline {
            out.push_str(&line);
            out.push_str("\r\n");
            line = format!(" {token}");
        } else {
            line.push(' ');
            line.push_str(token);
        }
    }
    out.push_str(&line);
    out.push_str("\r\n");
    out
}

/// RFC 5322 的 `Date:` 值：`Thu, 01 Jan 1970 00:00:00 +0000`。
///
/// **一律 UTC（`+0000`）**：本 crate 不引时区库，而伪造一个本地偏移比诚实地说
/// 「这是 UTC」更糟——收件方看到的时刻始终是对的。
#[must_use]
pub fn format_date(unix_secs: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let days = unix_secs.div_euclid(86_400);
    let seconds_of_day = unix_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 是星期四：`days % 7 == 0` → "Thu"。
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    let month_name = MONTHS[(month - 1) as usize];
    let (hour, minute, second) = (
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    );
    format!("{weekday}, {day:02} {month_name} {year:04} {hour:02}:{minute:02}:{second:02} +0000")
}

/// 从「1970-01-01 起的天数」求公历年月日。
///
/// Howard Hinnant 的 `civil_from_days`（公有领域算法，被 `std::chrono` 的实现
/// 广泛采用）：先落到 400 年一轮的 `era` 上，再在年内解出月日。用整数运算，
/// 无查表、无闰年特判。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153; // [0, 11]，3 月为 0
    let day = (day_of_year - (153 * march_month + 2) / 5 + 1) as u32;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    } as u32;
    // 1/2 月落在「次年」的 3 月起算里，要减回去。
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// 统一成 CRLF 行尾（邮件协议要求的行尾只有 `\r\n`）。
///
/// 三种输入都要处理：`\r\n`（已对）、`\n`（Unix 编辑器）、`\r`（老 Mac）。
pub(crate) fn normalize_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\r\n");
            }
            '\n' => out.push_str("\r\n"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        let mut draft = Draft::new(
            Mailbox::with_name("alice@example.com", "Alice"),
            Mailbox::with_name("bob@example.com", "Bob"),
            "hello",
            "line one\nline two",
        );
        draft.message_id = "abc@example.com".into();
        draft
    }

    /// `Date` 头的日历换算：含闰年、月末、负时间戳（1970 之前）。
    ///
    /// 期望值全部由外部权威实现（Python `datetime`）算出后钉进来，不是手推的。
    #[test]
    fn date_header_matches_known_instants() {
        assert_eq!(format_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        // 2000-02-29（闰年，且落在「3 月起算」的 1/2 月回退分支上）。
        assert_eq!(format_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 +0000");
        // 1972-02-29（另一个闰日）。
        assert_eq!(format_date(68_169_600), "Tue, 29 Feb 1972 00:00:00 +0000");
        // 1900-03-01：1900 **不是**闰年（能被 100 整除但不能被 400 整除）。
        assert_eq!(
            format_date(-2_203_891_200),
            "Thu, 01 Mar 1900 00:00:00 +0000"
        );
        // 1970-01-01 之前：`div_euclid` / `rem_euclid` 必须给出正确的前一天。
        assert_eq!(format_date(-1), "Wed, 31 Dec 1969 23:59:59 +0000");
        assert_eq!(format_date(-86_400), "Wed, 31 Dec 1969 00:00:00 +0000");
        // 带时分秒的一刻。
        assert_eq!(
            format_date(1_760_272_496),
            "Sun, 12 Oct 2025 12:34:56 +0000"
        );
    }

    /// 头部注入：CR / LF / 控制字符一律拒绝，不静默改写。
    #[test]
    fn header_injection_is_rejected_not_silently_rewritten() {
        let mut poisoned = draft();
        poisoned.subject = "hi\r\nBcc: evil@example.com".into();
        let err = build_message(&poisoned).expect_err("换行必须被拒");
        assert_eq!(err.code(), "config");
        assert!(err.to_string().contains("header injection"), "{err}");

        // 收件人地址里换行同理。
        let mut bad_address = draft();
        bad_address.to = vec![Mailbox::new(
            "bob@example.com\r\nRCPT TO:<evil@example.com>",
        )];
        assert_eq!(build_message(&bad_address).unwrap_err().code(), "config");

        // 显示名里换行。
        let mut bad_name = draft();
        bad_name.from = Mailbox::with_name("alice@example.com", "Alice\nBcc: x@y");
        assert_eq!(build_message(&bad_name).unwrap_err().code(), "config");

        // 地址里的尖括号会破坏 `Name <addr>` 结构，也要拒。
        assert_eq!(
            Mailbox::new("a@b> <c@d").header_value().unwrap_err().code(),
            "config"
        );
    }

    /// RFC 2047：ASCII 不动，非 ASCII 编码，长串按字符边界切块且不超 75 字符。
    #[test]
    fn non_ascii_headers_use_rfc2047_encoded_words() {
        assert_eq!(encode_header_text("plain ascii"), "plain ascii");

        let encoded = encode_header_text("会议纪要");
        assert!(encoded.starts_with("=?UTF-8?B?"), "{encoded}");
        assert!(encoded.ends_with("?="), "{encoded}");
        // 4 个汉字 = 12 字节，base64 后 16 字符，一个 word 装得下。
        assert!(!encoded.contains(' '), "不该切块：{encoded}");

        // 长中文串必须切块，且每块 ≤ 75 字符（RFC 2047 的硬上限）。
        let long = "这是一段相当长的中文主题用于验证编码分词是否会在字符边界处切开而不是把多字节字符劈成两半".repeat(3);
        let encoded = encode_header_text(&long);
        for word in encoded.split(' ') {
            assert!(word.len() <= 75, "encoded-word 超长：{} 字符", word.len());
            assert!(word.starts_with("=?UTF-8?B?"), "{word}");
            assert!(word.ends_with("?="), "{word}");
        }
        // 逐块解回来必须与原文完全一致（证明确实切在字符边界上）。
        let decoded: String = encoded
            .split(' ')
            .map(|word| {
                let payload = word
                    .strip_prefix("=?UTF-8?B?")
                    .and_then(|rest| rest.strip_suffix("?="))
                    .expect("encoded-word 形状");
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .expect("合法 base64");
                String::from_utf8(bytes).expect("切块没劈坏 UTF-8")
            })
            .collect();
        assert_eq!(decoded, long);
    }

    /// 折行：只在空白处折，且 encoded-word 之间正好是合法折点。
    #[test]
    fn folding_only_breaks_at_whitespace() {
        let recipients: Vec<String> = (0..12)
            .map(|index| format!("recipient-{index}@a-fairly-long-domain.example"))
            .collect();
        let value = recipients.join(", ");
        let folded = fold_header("To", &value);
        for line in folded.split("\r\n").filter(|line| !line.is_empty()) {
            assert!(
                line.len() <= HEADER_FOLD_WIDTH,
                "超宽行（{}）：{line}",
                line.len()
            );
        }
        // 续行以空格开头；折掉续行空白后应拼回原值。
        let lines: Vec<&str> = folded
            .split("\r\n")
            .filter(|line| !line.is_empty())
            .collect();
        assert!(lines.len() > 1, "这么长的值应该折行");
        let rejoined = lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    (*line).strip_prefix("To: ").unwrap_or(line)
                } else {
                    line.strip_prefix(' ').expect("续行以空格开头")
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(rejoined, value);
    }

    /// `Bcc` 进信封不进头部——这就是「密送」。
    #[test]
    fn bcc_goes_to_the_envelope_but_never_to_the_headers() {
        let mut draft = draft();
        draft.cc = vec![Mailbox::new("carol@example.com")];
        draft.bcc = vec![Mailbox::new("dave@example.com")];

        let recipients = envelope_recipients(&draft);
        assert_eq!(
            recipients,
            vec![
                "bob@example.com".to_owned(),
                "carol@example.com".to_owned(),
                "dave@example.com".to_owned(),
            ]
        );

        let message = build_message(&draft).unwrap();
        let headers = message.split("\r\n\r\n").next().unwrap();
        assert!(headers.contains("carol@example.com"), "Cc 要在头部");
        assert!(
            !headers.contains("dave@example.com"),
            "Bcc 绝不能出现在头部：{headers}"
        );
        assert!(!headers.contains("Bcc:"), "连 Bcc 这个头都不该有");
    }

    /// 信封收件人去重保序（同一人同时在 To 与 Cc 时只发一次）。
    #[test]
    fn envelope_recipients_dedupe_and_keep_order() {
        let mut draft = draft();
        draft.cc = vec![
            Mailbox::new("bob@example.com"),
            Mailbox::new("carol@example.com"),
        ];
        draft.bcc = vec![Mailbox::new("bob@example.com")];
        assert_eq!(
            envelope_recipients(&draft),
            vec!["bob@example.com".to_owned(), "carol@example.com".to_owned()]
        );
    }

    /// 整封装配：头部顺序固定、CRLF 行尾、正文与头部之间恰有一个空行。
    #[test]
    fn build_message_has_a_stable_header_shape() {
        let message = build_message(&draft()).unwrap();
        let expected = "Date: Thu, 01 Jan 1970 00:00:00 +0000\r\n\
                        From: Alice <alice@example.com>\r\n\
                        To: Bob <bob@example.com>\r\n\
                        Subject: hello\r\n\
                        Message-ID: <abc@example.com>\r\n\
                        MIME-Version: 1.0\r\n\
                        Content-Type: text/plain; charset=utf-8\r\n\
                        Content-Transfer-Encoding: 8bit\r\n\
                        \r\n\
                        line one\r\nline two";
        assert_eq!(message, expected);
        // 正文里的 LF 必须已被规整成 CRLF，且没有裸 LF 残留。
        let body = message.split("\r\n\r\n").nth(1).unwrap();
        assert!(!body.contains('\n') || body.contains("\r\n"));
        assert_eq!(body.matches('\n').count(), body.matches("\r\n").count());
    }

    /// 没有 Cc 就不写 `Cc:` 头；主题为空也要写出 `Subject:`（缺席歧义比空值大）。
    #[test]
    fn optional_headers_are_omitted_only_when_the_field_is_absent() {
        let mut draft = draft();
        draft.subject = String::new();
        let message = build_message(&draft).unwrap();
        assert!(message.contains("\r\nSubject:\r\n"), "{message}");
        assert!(!message.contains("\r\nCc:"), "没抄送就不该有 Cc 头");
        assert!(
            !message.contains("References:"),
            "没有引用链就不该有 References"
        );
        assert!(!message.contains("In-Reply-To:"));
    }

    /// 回复一封：`In-Reply-To` / `References` 自动补尖括号，重复写也不重复补。
    #[test]
    fn reply_headers_get_angle_brackets_exactly_once() {
        let mut draft = draft();
        draft.in_reply_to = Some("parent@example.com".into());
        draft.references = vec!["<root@example.com>".into(), "parent@example.com".into()];
        let message = build_message(&draft).unwrap();
        assert!(
            message.contains("\r\nIn-Reply-To: <parent@example.com>\r\n"),
            "{message}"
        );
        assert!(
            message.contains("\r\nReferences: <root@example.com> <parent@example.com>\r\n"),
            "{message}"
        );
        assert!(!message.contains("<<"), "不该出现双层尖括号");
    }

    /// 没有收件人直接拒绝（否则会发出一个没有任何 `RCPT TO` 的会话）。
    #[test]
    fn empty_recipient_list_is_rejected() {
        let mut draft = draft();
        draft.to.clear();
        assert_eq!(build_message(&draft).unwrap_err().code(), "config");
        assert!(envelope_recipients(&draft).is_empty());
    }

    /// CRLF 规整：三种行尾都收敛到 `\r\n`，且不产生重复的 `\r`。
    #[test]
    fn crlf_normalisation_handles_all_three_line_endings() {
        assert_eq!(normalize_crlf("a\nb"), "a\r\nb");
        assert_eq!(normalize_crlf("a\r\nb"), "a\r\nb");
        assert_eq!(normalize_crlf("a\rb"), "a\r\nb");
        assert_eq!(normalize_crlf("a\r\r\nb"), "a\r\n\r\nb");
        assert_eq!(normalize_crlf(""), "");
    }

    /// 纯 ASCII 显示名不加引号也不编码（最小惊讶）。
    #[test]
    fn ascii_display_names_stay_verbatim() {
        let mailbox = Mailbox::with_name("bob@example.com", "Bob Smith");
        assert_eq!(
            mailbox.header_value().unwrap(),
            "Bob Smith <bob@example.com>"
        );
        assert_eq!(
            Mailbox::new("bob@example.com").header_value().unwrap(),
            "bob@example.com"
        );
    }

    /// 中文显示名走 encoded-word，尖括号结构不受影响。
    #[test]
    fn non_ascii_display_names_are_encoded_within_the_angle_brackets() {
        let mailbox = Mailbox::with_name("bob@example.com", "张三");
        let value = mailbox.header_value().unwrap();
        assert!(value.starts_with("=?UTF-8?B?"), "{value}");
        assert!(value.ends_with(" <bob@example.com>"), "{value}");
    }

    /// `stamp`：只补空缺，已经填好的不动。
    #[test]
    fn stamp_only_fills_what_is_missing() {
        let mut draft = draft();
        draft.stamp(1_700_000_000);
        assert_eq!(draft.date_unix, 1_700_000_000);
        assert_eq!(draft.message_id, "abc@example.com", "已填的不该被覆盖");

        let mut fresh = Draft::new(
            Mailbox::new("alice@example.com"),
            Mailbox::new("bob@example.com"),
            "s",
            "b",
        );
        fresh.stamp(1_700_000_000);
        assert_eq!(fresh.date_unix, 1_700_000_000);
        assert!(fresh.message_id.ends_with("@example.com"), "{fresh:?}");
        // 补出来的值要能直接用（装配时不报错，且头部带尖括号）。
        let message = build_message(&fresh).expect("装配");
        assert!(message.contains("\r\nMessage-ID: <"), "{message}");
        assert!(!message.contains("<<"), "不该出现双层尖括号");
    }

    /// `Message-ID` 的形态与「两次调用不相同」。
    #[test]
    fn generated_message_ids_are_well_formed_and_distinct() {
        let first = generate_message_id("alice@example.com", 1_700_000_000);
        let second = generate_message_id("alice@example.com", 1_700_000_000);
        assert_ne!(first, second, "同一秒内连续两封也必须不同");
        assert!(first.ends_with("@example.com"), "{first}");
        assert!(first.starts_with("1700000000."), "{first}");
        assert_eq!(first.matches('@').count(), 1, "{first}");

        // 地址里没有 `@` 时回落到 localhost（而不是造出一个空域名）。
        assert!(generate_message_id("nobody", 1).ends_with("@localhost"));
        // 带显示名的地址一般不会传进来，但传了也不该把域名解析错。
        assert!(generate_message_id("a@b", 1).ends_with("@b"));
    }

    /// `now_unix` 必须在合理区间内（不要因为 `SystemTime` 出错而回落到 0）。
    #[test]
    fn now_unix_is_plausible() {
        let now = now_unix();
        assert!(now > 1_600_000_000, "时间戳不该回落到 0：{now}");
    }

    // ---- 附件（发）--------------------------------------------------------

    /// 顶层 `Content-Type` 里的 boundary（`mail-builder` 写的，可能带折行）。
    fn boundary_of(message: &str) -> String {
        let marker = "boundary=\"";
        let start = message
            .find(marker)
            .expect("顶层 Content-Type 该带 boundary")
            + marker.len();
        let rest = &message[start..];
        let end = rest.find('"').expect("boundary 该有收尾引号");
        rest[..end].to_owned()
    }

    /// 含 `marker` 的那一段（部件头 + 正文，不含分隔符本身）。
    fn part_segment<'a>(message: &'a str, boundary: &str, marker: &str) -> &'a str {
        message
            .split(&format!("--{boundary}"))
            .find(|segment| segment.contains(marker))
            .unwrap_or_else(|| panic!("没有含 {marker} 的部件：\n{message}"))
    }

    /// 某一段的正文（部件头与正文之间的空行之后）。
    fn part_body<'a>(message: &'a str, boundary: &str, marker: &str) -> &'a str {
        let segment = part_segment(message, boundary, marker);
        segment
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("部件头与正文之间该有空行：{segment}"))
            .1
    }

    /// 挂上附件 → 正文切成 `multipart/mixed`；boundary 头尾一致、两段齐全。
    #[test]
    fn attachments_switch_the_body_to_multipart_mixed() {
        let draft = draft().with_attachment(OutgoingAttachment::new(
            "notes.txt",
            "text/plain",
            b"hello attachment".to_vec(),
        ));
        let message = build_message(&draft).unwrap();

        // 头部仍是本模块写的，顺序不变：`Content-Type` 紧接 `MIME-Version`。
        assert!(
            message.contains("MIME-Version: 1.0\r\nContent-Type: multipart/mixed"),
            "{message}"
        );
        // 单部件那两行都该退场：顶层不能再有 `8bit`，正文段也不能再叫无引号的 charset。
        assert!(
            !message.contains("Content-Transfer-Encoding: 8bit"),
            "{message}"
        );
        assert!(
            !message.contains("Content-Type: text/plain; charset=utf-8\r\n"),
            "{message}"
        );

        let boundary = boundary_of(&message);
        assert!(boundary.len() > 8, "boundary 太短：{boundary}");
        // 收尾分隔符用**同一个** boundary，且是整封信的最后一行。
        assert!(
            message.ends_with(&format!("--{boundary}--\r\n")),
            "{message}"
        );
        // 恰好两个"开分隔符"（正文 + 1 附件）；收尾的 `--b--` 不算。
        assert_eq!(
            message.matches(&format!("--{boundary}\r\n")).count(),
            2,
            "{message}"
        );
        // 正文段：库自动补了带引号的 charset。
        assert!(
            message.contains("Content-Type: text/plain; charset=\"utf-8\""),
            "{message}"
        );
        // 附件段：带 `Content-Disposition`，文件名原样（ASCII 不编码）。
        assert!(
            message.contains("Content-Disposition: attachment; filename=\"notes.txt\""),
            "{message}"
        );
    }

    /// 无附件时**完全不碰** multipart（与引入 `mail-builder` 之前逐字节相同）。
    #[test]
    fn a_plain_draft_never_touches_multipart() {
        let message = build_message(&draft()).unwrap();
        assert!(!message.contains("multipart"), "{message}");
        assert!(!message.contains("boundary"), "{message}");
        assert!(
            message.contains("Content-Type: text/plain; charset=utf-8\r\n"),
            "{message}"
        );
        assert!(
            message.contains("Content-Transfer-Encoding: 8bit\r\n"),
            "{message}"
        );
        assert!(message.ends_with("line one\r\nline two"), "{message}");
    }

    /// 正文段的编码**由内容决定**（库的行为，钉住它，免得日后读代码时惊讶）：
    /// 纯 ASCII 走 `7bit` 原样；含非 ASCII 走 `base64`。
    #[test]
    fn the_text_part_encoding_follows_the_body() {
        use base64::Engine as _;

        // 纯 ASCII：7bit，正文原样在内（LF 已规整成 CRLF）。
        let ascii = draft().with_attachment(OutgoingAttachment::new(
            "blob.bin",
            "application/octet-stream",
            vec![1],
        ));
        let message = build_message(&ascii).unwrap();
        let boundary = boundary_of(&message);
        let segment = part_segment(&message, &boundary, "text/plain");
        assert!(
            segment.contains("Content-Transfer-Encoding: 7bit"),
            "{segment}"
        );
        assert!(segment.contains("line one\r\nline two"), "{segment}");

        // 非 ASCII：base64，解回来必须与原文（CRLF 规整后）一致。
        let mut chinese = draft();
        chinese.body = "你好".into();
        let chinese = chinese.with_attachment(OutgoingAttachment::new(
            "blob.bin",
            "application/octet-stream",
            vec![1],
        ));
        let message = build_message(&chinese).unwrap();
        let boundary = boundary_of(&message);
        let segment = part_segment(&message, &boundary, "text/plain");
        assert!(
            segment.contains("Content-Transfer-Encoding: base64"),
            "{segment}"
        );
        let compact: String = part_body(&message, &boundary, "text/plain")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(compact)
            .expect("正文段该是合法 base64");
        assert_eq!(String::from_utf8(decoded).unwrap(), "你好");
    }

    /// 附件字节经 base64 往返后必须**逐字节**相同（含 `0x00` / `0xFF` / 形似 CRLF 的序列）。
    #[test]
    fn attachment_bytes_survive_the_base64_round_trip() {
        use base64::Engine as _;
        let payload: Vec<u8> = vec![0x00, 0x0d, 0x0a, 0xff, 0xfe, b'm', b'a', b'i', b'l'];
        let draft = draft().with_attachment(OutgoingAttachment::new(
            "blob.bin",
            "application/octet-stream",
            payload.clone(),
        ));
        let message = build_message(&draft).unwrap();
        let boundary = boundary_of(&message);
        // 二进制附件一律 base64（库的 `Binary` + 非 text 类型分支）。
        let compact: String = part_body(&message, &boundary, "application/octet-stream")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(compact)
            .expect("附件段该是合法 base64");
        assert_eq!(decoded, payload);
    }

    /// 非 ASCII 文件名要被**编码**（不是原样塞进头部，也不能丢）。
    #[test]
    fn non_ascii_attachment_names_are_encoded() {
        let draft = draft().with_attachment(OutgoingAttachment::new(
            "报告 2026.pdf",
            "application/pdf",
            vec![1, 2, 3],
        ));
        let message = build_message(&draft).unwrap();
        assert!(message.contains("filename=\"=?utf-8?B?"), "{message}");
        // 头部区（第一个空行之前）里不该出现裸中文。
        let headers = message.split("\r\n\r\n").next().unwrap();
        assert!(
            !headers
                .chars()
                .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "部件头里不该有裸中文：{headers}"
        );
    }

    /// `guess_content_type`：常见档 + 大小写不敏感 + 认不出回落 `octet-stream`。
    #[test]
    fn content_type_is_guessed_from_the_extension() {
        assert_eq!(guess_content_type("a.PDF"), "application/pdf");
        assert_eq!(
            guess_content_type("报表.xlsx"),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
        assert_eq!(guess_content_type("photo.jpeg"), "image/jpeg");
        // 取**最后**一个后缀。
        assert_eq!(guess_content_type("archive.tar.gz"), "application/gzip");
        // 没有后缀 / 认不出 → 「未知二进制」。
        assert_eq!(guess_content_type("README"), "application/octet-stream");
        assert_eq!(guess_content_type("a.qqq"), "application/octet-stream");
        assert_eq!(guess_content_type("说明"), "application/octet-stream");
    }

    /// `from_path`：文件名取末段、字节原样读出、类型按后缀猜；失败要给**可分辨**的 `code()`。
    #[test]
    fn from_path_reads_bytes_and_names_the_file() {
        let dir = std::env::temp_dir().join(format!("duck-compose-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("报表.pdf");
        std::fs::write(&path, [0u8, 1, 255]).unwrap();

        let attachment = OutgoingAttachment::from_path(&path).unwrap();
        assert_eq!(attachment.filename, "报表.pdf");
        assert_eq!(attachment.content_type, "application/pdf");
        assert_eq!(attachment.bytes, vec![0u8, 1, 255]);
        assert_eq!(attachment.len(), 3);
        assert!(!attachment.is_empty());

        // 文件不在 → `io`（界面靠 `code()` 选文案，别退化成 `other`）。
        let missing = OutgoingAttachment::from_path(&dir.join("nope.bin")).unwrap_err();
        assert_eq!(missing.code(), "io", "{missing}");

        // 路径没有末段（`/` 在 macOS 上就是个合法目录）→ `config`，不要去读它。
        let no_name = OutgoingAttachment::from_path(std::path::Path::new("/")).unwrap_err();
        assert_eq!(no_name.code(), "config", "{no_name}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 0 字节附件**合法**（空文件是真实存在的），只是要能分辨。
    #[test]
    fn an_empty_attachment_is_valid_but_marked_empty() {
        let empty = OutgoingAttachment::new("empty.txt", "text/plain", Vec::new());
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        let draft = draft().with_attachment(empty);
        let message = build_message(&draft).unwrap();
        // 仍然是 multipart；空的 base64 正文段只是没有内容，不是错误。
        assert!(
            message.contains("Content-Type: multipart/mixed"),
            "{message}"
        );
        assert!(message.contains("filename=\"empty.txt\""), "{message}");
    }
}
