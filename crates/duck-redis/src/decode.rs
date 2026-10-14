//! 值解码管线（M1.5 首期：Base64 / GZIP / JWT / Unix 时间戳 / URL / PHP serialize）。
//!
//! 两条纪律（`docs/redis-design.md` §4.6）：
//!
//! 1. **显示的是解码后的内容，写回服务端的永远是存储的原始字节**——本模块只做
//!    「字节 → 可读文本」的纯函数，不参与任何写回路径，也不接受写入回调。
//! 2. **档位显式**（§6 末尾）：不做「按内容自动判断」。旧版按 `{` 开头就试 JSON、
//!    失败再悄悄退回原文，用户说不清自己看的是哪一档；显式档位每档行为都能一句话
//!    说清，也便于单测。
//!
//! 零 GPUI 内核不吐成品中文：错误与提示一律以「源串 + 参数」上交（[`Message`]），
//! 界面层用 `tf(lang, source, args)` 渲染。
//!
//! 本模块**不引入新依赖**：`base64`（0.22）与 `flate2`（1.1）都早已在依赖图里，
//! 这里只是提成直接依赖；URL 百分号解码、PHP serialize 解析、Unix 时间戳的
//! 历法换算（Howard Hinnant 的 `civil_from_days`）都是手写的纯函数，附单测。
//!
//! **未做（M1.6）**：MessagePack、ZSTD、BSON、Java 序列化、pickle、LZ4、图片预览——
//! 它们要新引 `rmp*` / `zstd` / `lz4` 等包，须走依赖准入（MSRV 1.85 + 许可证）。

use std::fmt;
use std::io::Read as _;

use base64::Engine as _;

/// hex 预览的字节上限（超出部分只提示不展示）。
pub const HEX_PREVIEW_BYTES: usize = 512;

/// 解压类档位的输出上限 4 MiB：GZIP 是可控炸弹（几百字节能膨胀到 GB 级），
/// 没有这个护栏就会在后台执行器里吃掉内存。与界面层的自动加载上限同量级。
const INFLATE_LIMIT: usize = 4 * 1024 * 1024;

/// PHP serialize 的嵌套上限（手写递归下降，必须有深度护栏否则栈溢出）。
const MAX_DEPTH: usize = 32;

/// 解码档位。**顺序即界面顺序**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum DecodeKind {
    /// 原样文本（要求合法 UTF-8）。
    #[default]
    Text,
    /// JSON 美化（非法 JSON 明确报错，不退化成原文——否则用户以为美化了）。
    Json,
    /// 十六进制预览。
    Hex,
    /// Base64（标准 / URL 安全、带 / 不带 padding 都试）。
    Base64,
    /// GZIP 解压。
    Gzip,
    /// JWT：拆 header / payload 段并美化。
    Jwt,
    /// Unix 时间戳（按位数判档：秒 / 毫秒 / 微秒 / 纳秒）。
    Timestamp,
    /// URL 百分号解码。
    Url,
    /// PHP `serialize()` 数据。
    Php,
}

impl DecodeKind {
    /// 全部档位（M1.5 首期九档）。
    pub const ALL: [Self; 9] = [
        Self::Text,
        Self::Json,
        Self::Hex,
        Self::Base64,
        Self::Gzip,
        Self::Jwt,
        Self::Timestamp,
        Self::Url,
        Self::Php,
    ];

    /// 档位标识（也用作界面元素的 id 后缀：`redis-decode-<wire>`）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Hex => "hex",
            Self::Base64 => "base64",
            Self::Gzip => "gzip",
            Self::Jwt => "jwt",
            Self::Timestamp => "timestamp",
            Self::Url => "url",
            Self::Php => "php",
        }
    }
}

/// 一句待渲染的文案：**源串（中文源，即 i18n 键）+ 位置参数**。
///
/// 内核不给成品中文，只把这两样交上去；[`Message::render`] 用同一份数据渲染中文，
/// 仅供日志与 `Display`（`{}` 按顺序站位，与界面层的 `tf` 同规矩）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// 中文源串（`{}` 为站位符）。
    pub source: &'static str,
    /// 站位参数（按顺序）。
    pub args: Vec<String>,
}

impl Message {
    /// 无参数文案。
    #[must_use]
    pub fn new(source: &'static str) -> Self {
        Self {
            source,
            args: Vec::new(),
        }
    }

    /// 带参数文案。
    #[must_use]
    pub fn with(source: &'static str, args: Vec<String>) -> Self {
        Self { source, args }
    }

    /// 渲染成中文（日志 / `Display` 用；界面层应改用 `tf`）。
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.source.len());
        let mut args = self.args.iter();
        let mut rest = self.source;
        while let Some(at) = rest.find("{}") {
            out.push_str(&rest[..at]);
            match args.next() {
                Some(arg) => out.push_str(arg),
                // 参数不够就保留站位符：宁可露出 `{}` 也不要悄悄吞掉信息。
                None => out.push_str("{}"),
            }
            rest = &rest[at + 2..];
        }
        out.push_str(rest);
        out
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// 一次成功解码的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// 解码后的可读文本。`binary` 为真时它是**十六进制预览**，而不是解码出的内容。
    pub text: String,
    /// `text` 是十六进制预览（解出的内容不是文本）：界面据此选等宽展示。
    pub binary: bool,
    /// 附带说明（如「GZIP 解压出 1234 字节」）；无话可说时为 `None`。
    pub notice: Option<Message>,
}

impl Decoded {
    /// 纯文本结果，无附带说明。
    fn plain(text: String) -> Self {
        Self {
            text,
            binary: false,
            notice: None,
        }
    }

    /// 带说明的文本结果。
    fn noted(text: String, notice: Message) -> Self {
        Self {
            text,
            binary: false,
            notice: Some(notice),
        }
    }

    /// 十六进制预览（解出的内容不是文本），必带说明。
    fn preview(text: String, notice: Message) -> Self {
        Self {
            text,
            binary: true,
            notice: Some(notice),
        }
    }
}

/// 按**显式档位**解码。
///
/// # Errors
///
/// 该档位下数据不合法时返回 [`Message`]（中文源串 + 参数），界面层渲染后展示。
pub fn decode(kind: DecodeKind, bytes: &[u8]) -> Result<Decoded, Message> {
    match kind {
        DecodeKind::Text => decode_text(bytes),
        DecodeKind::Json => decode_json(bytes),
        DecodeKind::Hex => Ok(decode_hex(bytes)),
        DecodeKind::Base64 => decode_base64(bytes),
        DecodeKind::Gzip => decode_gzip(bytes),
        DecodeKind::Jwt => decode_jwt(bytes),
        DecodeKind::Timestamp => decode_timestamp(bytes),
        DecodeKind::Url => decode_url(bytes),
        DecodeKind::Php => decode_php(bytes),
    }
}

/// 「要文本却拿到非 UTF-8」的共用文案。
const NOT_UTF8: &str = "不是合法的 UTF-8 文本，可切到「Hex」档查看原始字节。";

fn utf8(bytes: &[u8]) -> Result<&str, Message> {
    std::str::from_utf8(bytes).map_err(|_| Message::new(NOT_UTF8))
}

fn decode_text(bytes: &[u8]) -> Result<Decoded, Message> {
    Ok(Decoded::plain(utf8(bytes)?.to_owned()))
}

fn decode_json(bytes: &[u8]) -> Result<Decoded, Message> {
    let text = utf8(bytes)?;
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| Message::with("不是合法 JSON：{}", vec![error.to_string()]))?;
    let pretty = serde_json::to_string_pretty(&value)
        .map_err(|error| Message::with("JSON 美化失败：{}", vec![error.to_string()]))?;
    Ok(Decoded::plain(pretty))
}

fn decode_hex(bytes: &[u8]) -> Decoded {
    let notice = if bytes.len() > HEX_PREVIEW_BYTES {
        Some(Message::with(
            "超过 {} 字节的部分未显示。",
            vec![HEX_PREVIEW_BYTES.to_string()],
        ))
    } else {
        None
    };
    Decoded {
        text: hex_dump(bytes),
        binary: true,
        notice,
    }
}

/// 前 [`HEX_PREVIEW_BYTES`] 字节的 hex（每 16 字节一行）。
#[must_use]
pub fn hex_dump(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(HEX_PREVIEW_BYTES)];
    let mut out = String::with_capacity(head.len() * 3 + bytes.len() % 16);
    for (index, byte) in head.iter().enumerate() {
        if index > 0 {
            if index % 16 == 0 {
                out.push('\n');
            } else {
                out.push(' ');
            }
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Base64 四种形态依次试：标准带 padding / 标准无 padding / URL 安全带 padding /
/// URL 安全无 padding。Redis 里的 base64 多来自各家 SDK，形态并不统一，
/// 逐个试比让用户自己猜「是哪种」友好得多。
///
/// 说明里**不报**命中的是哪一种：那是实现细节，用户要的是解出来的内容。
fn decode_base64(bytes: &[u8]) -> Result<Decoded, Message> {
    use base64::engine::general_purpose as gp;
    let text = utf8(bytes)?.trim();
    let engines = [
        gp::STANDARD,
        gp::STANDARD_NO_PAD,
        gp::URL_SAFE,
        gp::URL_SAFE_NO_PAD,
    ];
    let payload = engines
        .iter()
        .find_map(|engine| engine.decode(text).ok())
        .ok_or_else(|| {
            Message::new("不是合法的 Base64（标准 / URL 安全、带 / 不带 padding 都不匹配）。")
        })?;
    Ok(payload_result(
        &payload,
        Message::with("Base64 解出 {} 字节。", vec![payload.len().to_string()]),
    ))
}

fn decode_gzip(bytes: &[u8]) -> Result<Decoded, Message> {
    let mut payload = Vec::new();
    let mut reader = flate2::read::GzDecoder::new(bytes).take(INFLATE_LIMIT as u64 + 1);
    reader.read_to_end(&mut payload).map_err(|error| {
        // GZIP 头不合法、校验和不对都会走到这里。
        Message::with("不是合法的 GZIP 数据：{}", vec![error.to_string()])
    })?;
    if payload.len() > INFLATE_LIMIT {
        return Err(Message::with(
            "解压后超过 {} 字节，已停止（避免解压炸弹）。",
            vec![INFLATE_LIMIT.to_string()],
        ));
    }
    Ok(payload_result(
        &payload,
        Message::with("GZIP 解压出 {} 字节。", vec![payload.len().to_string()]),
    ))
}

/// 解码后的载荷：能读成文本就给文本，否则退 hex 预览（不显示乱码，也不给空面板）。
fn payload_result(payload: &[u8], text_notice: Message) -> Decoded {
    match std::str::from_utf8(payload) {
        Ok(text) => Decoded::noted(text.to_owned(), text_notice),
        Err(_) => Decoded::preview(
            hex_dump(payload),
            Message::with(
                "解码结果不是文本，下面是前 {} 字节的十六进制。",
                vec![payload.len().min(HEX_PREVIEW_BYTES).to_string()],
            ),
        ),
    }
}

/// JWT：`header.payload[.signature]`，两段都按 base64url 解码后美化。
///
/// 签名段是二进制，只报长度不展示（展示成 hex 对用户没有意义）。
fn decode_jwt(bytes: &[u8]) -> Result<Decoded, Message> {
    let text = utf8(bytes)?.trim();
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(Message::new(
            "不是合法的 JWT（应是 `头.载荷` 或 `头.载荷.签名`）。",
        ));
    }
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut out = String::new();
    for (index, name) in ["header", "payload"].iter().enumerate() {
        let section = parts[index];
        let raw = engine.decode(section).map_err(|_| {
            Message::with(
                "JWT 的 {} 段不是合法的 base64url。",
                vec![(*name).to_owned()],
            )
        })?;
        let json = std::str::from_utf8(&raw).map_err(|_| {
            Message::with(
                "JWT 的 {} 段不是合法的 UTF-8 JSON。",
                vec![(*name).to_owned()],
            )
        })?;
        let pretty = match serde_json::from_str::<serde_json::Value>(json) {
            Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| json.to_owned()),
            Err(_) => json.to_owned(),
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(name);
        out.push('\n');
        out.push_str(&pretty);
    }
    let notice = match parts.get(2) {
        Some(signature) => {
            let raw = engine.decode(signature).unwrap_or_else(|_| Vec::new());
            Message::with(
                "签名段 {} 字节（未校验签名，这里只解码展示）。",
                vec![raw.len().to_string()],
            )
        }
        None => Message::new("这段 JWT 没有签名段。"),
    };
    Ok(Decoded::noted(out, notice))
}

/// Unix 时间戳：按整数位数判单位。位数判定比猜数量级稳（`1700000000` 是秒、
/// `1700000000000` 是毫秒，两者差 1000 倍但对人眼都是「一串数字」）。
fn decode_timestamp(bytes: &[u8]) -> Result<Decoded, Message> {
    let text = utf8(bytes)?.trim();
    let value: i64 = text.parse().map_err(|_| {
        Message::new("不是整数，无法按 Unix 时间戳解析（应为整数秒 / 毫秒 / 微秒 / 纳秒）。")
    })?;
    let digits = text.trim_start_matches('-').len();
    // 单位各自一条模板：界面层 `tf` 只翻译源串、**不翻译参数**，
    // 把「毫秒」当中文参数传过去，英文界面就会漏出中文。
    let (secs, sub, unit, template) = match digits {
        0..=11 => (value, 0_i64, 0_u8, "按 Unix 时间戳解析（秒）。"),
        12..=14 => (
            value.div_euclid(1_000),
            value.rem_euclid(1_000),
            1,
            "按 Unix 时间戳解析（毫秒）。",
        ),
        15..=17 => (
            value.div_euclid(1_000_000),
            value.rem_euclid(1_000_000),
            2,
            "按 Unix 时间戳解析（微秒）。",
        ),
        _ => (
            value.div_euclid(1_000_000_000),
            value.rem_euclid(1_000_000_000),
            3,
            "按 Unix 时间戳解析（纳秒）。",
        ),
    };
    let rendered = format_utc(secs, sub, unit);
    Ok(Decoded::noted(rendered, Message::new(template)))
}

/// `YYYY-MM-DD HH:MM:SS[.fraction] UTC`。手写历法换算，不引时间库：
/// `iana-time-zone` 之类的特性会带来新的传递依赖，而这里只需要 UTC 一个时区。
fn format_utc(secs: i64, sub: i64, unit: u8) -> String {
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = rest / 3600;
    let minute = (rest % 3600) / 60;
    let second = rest % 60;
    let fraction = match (sub, unit) {
        (0, _) => String::new(),
        (value, 1) => format!(".{value:03}"),
        (value, 2) => format!(".{value:06}"),
        (value, _) => format!(".{value:09}"),
    };
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}{fraction} UTC")
}

/// 1970-01-01 起的天数 → `(年, 月, 日)`。
///
/// 来自 Howard Hinnant 的 `civil_from_days`（公有领域算法），对 1970 之前
/// 的负数天数同样成立。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { y + 1 } else { y }, month, day)
}

/// URL 百分号解码。
///
/// **只处理 `%XX`，不把 `+` 当空格**：Redis 里的值多是正文或签名，`+` 若被
/// 悄悄吃掉会毁掉 base64 一类的原样内容；表单语义（`+` = 空格）由调用方的
/// 业务语境决定，不该由解码档位替用户拍板。
fn decode_url(bytes: &[u8]) -> Result<Decoded, Message> {
    let text = utf8(bytes)?;
    let source = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        if source[index] == b'%' {
            if index + 2 >= source.len() {
                return Err(Message::new(
                    "不是合法的 URL 编码（`%` 后面要跟两位十六进制）。",
                ));
            }
            let hi = hex_digit(source[index + 1]);
            let lo = hex_digit(source[index + 2]);
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(Message::new(
                    "不是合法的 URL 编码（`%` 后面要跟两位十六进制）。",
                ));
            };
            out.push(hi * 16 + lo);
            index += 3;
        } else {
            out.push(source[index]);
            index += 1;
        }
    }
    let decoded =
        String::from_utf8(out).map_err(|_| Message::new("URL 解码结果不是合法的 UTF-8。"))?;
    Ok(Decoded::noted(
        decoded,
        Message::new("已按 UTF-8 做百分号解码（`+` 保持原样）。"),
    ))
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_php(bytes: &[u8]) -> Result<Decoded, Message> {
    let text = utf8(bytes)?;
    let mut parser = Php::new(text);
    let value = parser.value(0).map_err(|error| {
        Message::with(
            "PHP serialize 解析在第 {} 字节处失败。",
            vec![error.at.to_string()],
        )
    })?;
    parser.finish()?;
    Ok(Decoded::plain(value))
}

/// 解析失败的位置。
///
/// 只带位置不带「原因」：原因是给开发者看的英文标记，露在界面上就成了没头没脑的
/// 英文；用户真正需要知道的是**停在第几个字节**，拿这个就能回去对数据。
struct PhpError {
    at: usize,
}

struct Php<'a> {
    source: &'a [u8],
    pos: usize,
}

impl<'a> Php<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            source: text.as_bytes(),
            pos: 0,
        }
    }

    fn rest(&self) -> &'a [u8] {
        &self.source[self.pos.min(self.source.len())..]
    }

    fn fail(&self) -> PhpError {
        PhpError { at: self.pos }
    }

    fn expect(&mut self, byte: u8) -> Result<(), PhpError> {
        if self.rest().first() == Some(&byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.fail())
        }
    }

    /// 读到分隔符为止的 ASCII 标记（数字 / 长度）。
    fn digits(&mut self) -> Result<i64, PhpError> {
        let start = self.pos;
        while matches!(self.rest().first(), Some(b'0'..=b'9' | b'-')) {
            self.pos += 1;
        }
        let raw = std::str::from_utf8(&self.source[start..self.pos]).map_err(|_| self.fail())?;
        raw.parse().map_err(|_| self.fail())
    }

    /// 一个 PHP 值。`depth` 是嵌套深度护栏。
    fn value(&mut self, depth: usize) -> Result<String, PhpError> {
        if depth > MAX_DEPTH {
            return Err(self.fail());
        }
        let tag = *self.rest().first().ok_or_else(|| self.fail())?;
        self.pos += 1;
        // `N;`（null）是唯一**没有**冒号的形式，先单独收掉再要求 `:`。
        if tag == b'N' {
            self.expect(b';')?;
            return Ok("null".to_owned());
        }
        self.expect(b':')?;
        match tag {
            b'b' => {
                let raw = self.digits()?;
                self.expect(b';')?;
                Ok(format!("bool({})", raw != 0))
            }
            b'i' => {
                let raw = self.digits()?;
                self.expect(b';')?;
                Ok(format!("int({raw})"))
            }
            b'd' => {
                let start = self.pos;
                while matches!(
                    self.rest().first(),
                    Some(
                        b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E' | b'N' | b'A' | b'I' | b'F'
                    )
                ) {
                    self.pos += 1;
                }
                let raw =
                    std::str::from_utf8(&self.source[start..self.pos]).map_err(|_| self.fail())?;
                self.expect(b';')?;
                Ok(format!("float({raw})"))
            }
            b's' => {
                let len = self.digits()?;
                self.expect(b':')?;
                self.expect(b'"')?;
                // PHP 的长度是**字节数**，不是字符数：中文串必须按字节切。
                let len = usize::try_from(len).map_err(|_| self.fail())?;
                let end = self.pos + len;
                if end > self.source.len() {
                    return Err(self.fail());
                }
                let raw = String::from_utf8_lossy(&self.source[self.pos..end]).into_owned();
                self.pos = end;
                self.expect(b'"')?;
                self.expect(b';')?;
                Ok(format!("string({len}) {}", quote(&raw)))
            }
            b'a' => {
                let count = self.digits()?;
                self.expect(b':')?;
                self.expect(b'{')?;
                let items = self.pairs(count, depth)?;
                Ok(format!("array({count}) {{{items}}}"))
            }
            b'O' => {
                let name_len = self.digits()?;
                self.expect(b':')?;
                self.expect(b'"')?;
                let name_len = usize::try_from(name_len).map_err(|_| self.fail())?;
                let end = self.pos + name_len;
                if end > self.source.len() {
                    return Err(self.fail());
                }
                let name = String::from_utf8_lossy(&self.source[self.pos..end]).into_owned();
                self.pos = end;
                self.expect(b'"')?;
                self.expect(b':')?;
                let count = self.digits()?;
                self.expect(b':')?;
                self.expect(b'{')?;
                let items = self.pairs(count, depth)?;
                Ok(format!("object({name}) {{{items}}}"))
            }
            _ => Err(self.fail()),
        }
    }

    /// `键 值` 对列表，每对占一行、缩进一级。
    fn pairs(&mut self, count: i64, depth: usize) -> Result<String, PhpError> {
        let mut out = String::new();
        for _ in 0..count.max(0) {
            let key = self.value(depth + 1)?;
            let value = self.value(depth + 1)?;
            out.push('\n');
            out.push_str(&"  ".repeat(depth + 1));
            out.push_str(&key);
            out.push_str(" => ");
            out.push_str(&value);
        }
        if !out.is_empty() {
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
        }
        self.expect(b'}')?;
        Ok(out)
    }

    /// 收尾校验：值自身已经吃掉自己的结尾（标量是 `;`，数组与对象是 `}`），
    /// 所以这里只容忍一个**可选**的尾随 `;`，再有多余内容就说明不是单值。
    fn finish(&mut self) -> Result<(), Message> {
        if self.rest().first() == Some(&b';') {
            self.pos += 1;
        }
        if self.rest().trim_ascii().is_empty() {
            Ok(())
        } else {
            Err(Message::with(
                "PHP serialize 解析在第 {} 字节后还剩内容，可能不是单值。",
                vec![self.pos.to_string()],
            ))
        }
    }
}

/// 可读的字符串字面量（控制字符转义，避免破坏多行展示）。
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(kind: DecodeKind, text: &str) -> Decoded {
        decode(kind, text.as_bytes()).expect("应当解码成功")
    }

    #[test]
    fn text_requires_valid_utf8() {
        assert_eq!(ok(DecodeKind::Text, "你好").text, "你好");
        assert!(decode(DecodeKind::Text, &[0xff, 0xfe]).is_err());
    }

    #[test]
    fn json_is_prettified_and_invalid_json_reports() {
        let decoded = ok(DecodeKind::Json, r#"{"a":1}"#);
        assert!(decoded.text.contains("\n"), "应当是美化后的多行 JSON");
        assert!(decoded.notice.is_none());
        let error = decode(DecodeKind::Json, b"{oops").unwrap_err();
        assert!(error.render().starts_with("不是合法 JSON："));
    }

    #[test]
    fn hex_dump_wraps_every_sixteen_bytes() {
        let bytes: Vec<u8> = (0..=17).collect();
        let text = hex_dump(&bytes);
        assert_eq!(text.lines().count(), 2);
        assert!(text.starts_with("00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f"));
        assert!(text.ends_with("10 11"));
    }

    #[test]
    fn long_hex_preview_notes_the_cut() {
        let bytes = vec![0u8; HEX_PREVIEW_BYTES + 3];
        let decoded = decode(DecodeKind::Hex, &bytes).expect("hex 档不会失败");
        assert!(decoded.text.lines().count() == HEX_PREVIEW_BYTES / 16);
        assert!(decoded.notice.is_some());
    }

    #[test]
    fn base64_accepts_standard_and_url_safe_forms() {
        // 标准形态（带 / 不带 padding）都要认。
        assert_eq!(ok(DecodeKind::Base64, "aGVsbG8=").text, "hello");
        assert_eq!(ok(DecodeKind::Base64, "aGVsbG8").text, "hello");
        // URL 安全形态用 `-` / `_` 代替 `+` / `/`：`____` 解出三个 0xff。
        assert_eq!(ok(DecodeKind::Base64, "____").text, "ff ff ff");
        let error = decode(DecodeKind::Base64, b"!!!not base64!!!").unwrap_err();
        assert!(error.render().contains("Base64"));
    }

    #[test]
    fn base64_payload_that_is_not_text_falls_back_to_hex() {
        // 0xff 0xfe 不是 UTF-8。
        let decoded = ok(DecodeKind::Base64, "//4=");
        assert_eq!(decoded.text, "ff fe");
        assert!(
            decoded
                .notice
                .expect("应有说明")
                .render()
                .contains("十六进制")
        );
    }

    #[test]
    fn gzip_round_trip() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write as _;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all("压缩过的中文".as_bytes())
            .expect("写压缩流");
        let packed = encoder.finish().expect("收压缩流");
        let decoded = decode(DecodeKind::Gzip, &packed).expect("应当解压成功");
        assert_eq!(decoded.text, "压缩过的中文");
        assert!(
            decoded
                .notice
                .expect("应有说明")
                .render()
                .contains("GZIP 解压出")
        );
    }

    #[test]
    fn gzip_rejects_plain_text() {
        let error = decode(DecodeKind::Gzip, b"not gzip at all").unwrap_err();
        assert!(error.render().contains("GZIP"));
    }

    #[test]
    fn jwt_splits_header_and_payload() {
        // `{"alg":"HS256"}` / `{"sub":"1"}` / 两字节签名。
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.AQ";
        let decoded = ok(DecodeKind::Jwt, token);
        assert!(decoded.text.starts_with("header\n"));
        assert!(decoded.text.contains("\"alg\": \"HS256\""));
        assert!(decoded.text.contains("payload\n"));
        assert!(decoded.text.contains("\"sub\": \"1\""));
        let notice = decoded.notice.expect("应有签名说明");
        assert!(notice.render().contains("签名段"));
    }

    #[test]
    fn jwt_without_signature_is_accepted() {
        let decoded = ok(DecodeKind::Jwt, "eyJzdWIiOiIxIn0.e30");
        assert!(
            decoded
                .notice
                .expect("应有说明")
                .render()
                .contains("没有签名段")
        );
        assert!(decode(DecodeKind::Jwt, b"just.one.too.many.parts").is_err());
    }

    #[test]
    fn timestamp_units_are_chosen_by_digit_count() {
        assert_eq!(
            ok(DecodeKind::Timestamp, "0").text,
            "1970-01-01 00:00:00 UTC"
        );
        let seconds = ok(DecodeKind::Timestamp, "1700000000");
        assert_eq!(seconds.text, "2023-11-14 22:13:20 UTC");
        assert!(seconds.notice.expect("应有说明").render().contains("秒"));
        // 同一时刻的毫秒 / 微秒 / 纳秒形态必须落到同一秒。
        assert_eq!(
            ok(DecodeKind::Timestamp, "1700000000000").text,
            seconds.text
        );
        assert_eq!(
            ok(DecodeKind::Timestamp, "1700000000000000").text,
            seconds.text
        );
        assert_eq!(
            ok(DecodeKind::Timestamp, "1700000000000000000").text,
            seconds.text
        );
        // 带亚秒精度时按单位补位。
        assert_eq!(
            ok(DecodeKind::Timestamp, "1700000000123").text,
            "2023-11-14 22:13:20.123 UTC"
        );
    }

    #[test]
    fn timestamp_rejects_non_integers() {
        assert!(decode(DecodeKind::Timestamp, b"not a ts").is_err());
    }

    #[test]
    fn civil_from_days_handles_leap_years_and_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2024-02-29（闰日）。
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        // 1969-12-31（1970 之前）。
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn url_decodes_percent_but_keeps_plus() {
        assert_eq!(ok(DecodeKind::Url, "a%20b").text, "a b");
        assert_eq!(ok(DecodeKind::Url, "%E4%B8%AD%E6%96%87").text, "中文");
        assert_eq!(ok(DecodeKind::Url, "a+b").text, "a+b");
        assert!(decode(DecodeKind::Url, b"%zz").is_err());
        assert!(decode(DecodeKind::Url, b"%2").is_err());
    }

    #[test]
    fn php_scalars_and_arrays() {
        assert_eq!(ok(DecodeKind::Php, "i:42;").text, "int(42)");
        assert_eq!(ok(DecodeKind::Php, "b:1;").text, "bool(true)");
        assert_eq!(ok(DecodeKind::Php, "N;").text, "null");
        assert_eq!(ok(DecodeKind::Php, "d:1.5;").text, "float(1.5)");
        // 「中文」是 6 个字节（UTF-8），长度按字节算。
        assert_eq!(
            ok(DecodeKind::Php, "s:6:\"中文\";").text,
            "string(6) \"中文\""
        );
    }

    #[test]
    fn php_array_and_object_layout() {
        let array = ok(DecodeKind::Php, "a:2:{i:0;s:2:\"ab\";s:1:\"k\";i:1;}").text;
        assert!(array.starts_with("array(2) {"), "{array}");
        assert!(array.contains("int(0) => string(2) \"ab\""), "{array}");
        assert!(array.contains("string(1) \"k\" => int(1)"), "{array}");
        let object = ok(DecodeKind::Php, "O:3:\"Foo\":1:{s:1:\"a\";i:7;}").text;
        assert!(object.starts_with("object(Foo) {"), "{object}");
        assert!(object.contains("int(7)"), "{object}");
    }

    #[test]
    fn php_reports_position_and_rejects_trailing_junk() {
        let error = decode(DecodeKind::Php, b"i:42").unwrap_err();
        assert!(
            error
                .render()
                .contains("PHP serialize 解析在第 4 字节处失败")
        );

        let error = decode(DecodeKind::Php, b"i:42;tail").unwrap_err();
        assert!(error.render().contains("还剩内容"));
    }

    #[test]
    fn php_depth_is_capped() {
        // 构造 40 层嵌套数组：应当报错而不是把栈打穿。
        let mut payload = String::new();
        for _ in 0..40 {
            payload.push_str("a:1:{i:0;");
        }
        payload.push_str("i:1;");
        for _ in 0..40 {
            payload.push('}');
        }
        payload.push(';');
        assert!(decode(DecodeKind::Php, payload.as_bytes()).is_err());
    }

    #[test]
    fn notice_rendering_keeps_placeholders_when_args_are_missing() {
        let message = Message::with("解出 {} 字节（{}）。", vec!["12".to_owned()]);
        assert_eq!(message.render(), "解出 12 字节（{}）。");
        assert_eq!(Message::new("无参数").render(), "无参数");
    }

    #[test]
    fn every_kind_has_a_distinct_wire_name() {
        let mut names: Vec<&str> = DecodeKind::ALL
            .iter()
            .map(|kind| kind.wire_name())
            .collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "wire_name 不能重复（要当元素 id）");
        assert_eq!(DecodeKind::default(), DecodeKind::Text);
    }
}
