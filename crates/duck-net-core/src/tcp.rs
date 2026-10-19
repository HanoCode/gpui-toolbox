//! 报文收发：连接 → 编码 → 发 → 按编解码器收 → 关。
//!
//! 合并参考仓三个页面（`TcpRequest.vue` 651 + `XmlSend.vue` 584 + `FixSend.vue` 612）
//! 的**通信骨架**。三个页面各写了一遍「连上去、发出去、读回来、写日志」，
//! 差异只有「报文怎么编帧」；这里把那部分交给 [`crate::codec`]，骨架只留一份。
//!
//! ## 与源页面的两处差异
//!
//! 1. **没有长连接**：源页面为了连续调同一台机器保留了 `connect_tcp` / `disconnect_tcp`
//!    这条长连接状态。插件进程是「跑一次就退」的，长连接既存不住也没意义——
//!    每次运行都是「连 → 发 → 收 → 断」。想发第二条就再点一次执行。
//! 2. **编码显式**：源页面把 `charset` 传给后端让后端 `encode_text`；这里同样支持
//!    UTF-8 与 GBK（GBK 走 `encoding_rs` 的 WHATWG 解码器，兼容 GB18030），
//!    但**报告里一定写明用了哪种**——GBK 发成 UTF-8 是最常见的「对面说不响应」。
//!
//! 读应答的策略：设 `read_timeout`，**读到对端安静下来**（一次读超时且已收到数据）
//! 就收工；`max_replies` 限制最多收几个帧，避免对端持续推送时无限读。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::codec::{Codec, CodecConfig, DecodeIssue};
use crate::error::{ErrorKind, NetError, NetResult};

/// 报文编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charset {
    #[default]
    Utf8,
    /// GBK（= WHATWG gbk，可解 GB18030）。
    Gbk,
}

impl Charset {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "gbk" | "gb2312" | "gb18030" => Self::Gbk,
            _ => Self::Utf8,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Gbk => "gbk",
        }
    }

    /// 文本 → 线上字节。
    pub fn encode(self, text: &str) -> Vec<u8> {
        match self {
            Self::Utf8 => text.as_bytes().to_vec(),
            // GBK 编不了的字符替换成 `?`（`encoding_rs` 的 `_without_bom` 同口径），
            // 与浏览器 `TextEncoder` 之外的实现一致：宁可发问号，也不要静默丢字节。
            Self::Gbk => {
                let (bytes, _, _) = encoding_rs::GBK.encode(text);
                bytes.into_owned()
            }
        }
    }

    /// 线上字节 → 文本（解不了的字节替换成 U+FFFD，绝不 panic）。
    pub fn decode(self, bytes: &[u8]) -> String {
        match self {
            Self::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
            Self::Gbk => {
                let (text, _, _) = encoding_rs::GBK.decode(bytes);
                text.into_owned()
            }
        }
    }
}

/// 报文来源：文本（按 charset 编码）或十六进制（二进制报文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// 文本 + 编码。
    Text { text: String, charset: Charset },
    /// `3C 3F 78 6D` / `3C3F786D` 都认（空白与 `0x` 前缀会被忽略）。
    Hex(String),
}

impl Payload {
    /// 编码成线上字节。
    pub fn bytes(&self) -> NetResult<Vec<u8>> {
        match self {
            Self::Text { text, charset } => Ok(charset.encode(text)),
            Self::Hex(text) => parse_hex(text),
        }
    }
}

/// 十六进制字符串 → 字节。
///
/// 允许空格 / 换行 / `,` 分隔与 `0x` 前缀（抓包工具粘出来的形态都能直接贴）。
/// 奇数字节、非法字符都报 [`ErrorKind::Decode`] 并带出问题片段。
pub fn parse_hex(raw: &str) -> NetResult<Vec<u8>> {
    let cleaned: String = raw
        .replace("0x", " ")
        .replace("0X", " ")
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != ',' && *ch != ':')
        .collect();
    if cleaned.is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    if cleaned.len() % 2 != 0 {
        return Err(NetError::with_detail(ErrorKind::Decode, cleaned.len()));
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    let bytes = cleaned.as_bytes();
    for index in (0..bytes.len()).step_by(2) {
        let pair = std::str::from_utf8(&bytes[index..index + 2])
            .map_err(|_| NetError::with_detail(ErrorKind::Decode, index / 2))?;
        let byte = u8::from_str_radix(pair, 16)
            .map_err(|_| NetError::with_detail(ErrorKind::Decode, pair))?;
        out.push(byte);
    }
    Ok(out)
}

/// 一次收发的参数。
#[derive(Debug, Clone)]
pub struct TcpRequest {
    pub host: String,
    pub port: u16,
    /// 要发出去的报文（已按 [`Payload`] 编码前的形态）。
    pub payload: Payload,
    /// 分帧规则（发送编码 + 应答解码共用同一份，与参考仓 `codecConfig` 同义）。
    pub codec: CodecConfig,
    /// 连不上就放弃的时间。
    pub connect_timeout_ms: u64,
    /// 单次读的最长等待；连续一次读超时即认为「对端说完了」。
    pub read_timeout_ms: u64,
    /// 最多收几个帧（对端持续推送时用来收口）。
    pub max_replies: usize,
    /// 应答总字节上限。
    pub max_bytes: usize,
    /// 应答按哪种编码解释（发给报告用；与发送编码独立）。
    pub charset: Charset,
}

impl Default for TcpRequest {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 0,
            payload: Payload::Text {
                text: String::new(),
                charset: Charset::Utf8,
            },
            codec: CodecConfig::default(),
            connect_timeout_ms: 5_000,
            read_timeout_ms: 5_000,
            max_replies: 16,
            max_bytes: 2 * 1024 * 1024,
            charset: Charset::Utf8,
        }
    }
}

/// 一次收发的产出。
#[derive(Debug, Clone)]
pub struct TcpOutcome {
    /// 实际发出去的字节数（含长度头 / 分隔符）。
    pub sent_bytes: usize,
    /// 发出去的完整帧（十六进制，报告里的 `>>` 行）。
    pub sent_hex: String,
    /// 收到的帧（原始字节）。
    pub replies: Vec<Vec<u8>>,
    /// 收到但没凑成完整帧的尾巴（对端没发完就断开时会有）。
    pub leftover: Vec<u8>,
    pub elapsed_ms: u64,
    /// 解码过程中遇到的问题（长度字段越界之类）。
    pub issue: Option<DecodeIssue>,
}

impl TcpOutcome {
    pub fn received_bytes(&self) -> usize {
        self.replies.iter().map(Vec::len).sum::<usize>() + self.leftover.len()
    }
}

/// 连上去、发出去、把应答读回来。
pub fn exchange(request: &TcpRequest) -> NetResult<TcpOutcome> {
    if request.host.trim().is_empty() || request.port == 0 {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    // 文本报文只有空白 = 没填（用户敲了几个空格不该被当成「一条 2 字节的报文」）。
    if let Payload::Text { text, .. } = &request.payload {
        if text.trim().is_empty() {
            return Err(NetError::new(ErrorKind::EmptyInput));
        }
    }
    let body = request.payload.bytes()?;
    if body.is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }

    let mut codec = crate::codec::create(&request.codec);
    let mut framed = Vec::new();
    codec.encode(&body, &mut framed);

    let address = format!("{}:{}", request.host.trim(), request.port);
    let started = Instant::now();
    let mut stream = connect(&address, request.connect_timeout_ms)?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(
        request.read_timeout_ms.max(200),
    )));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(
        request.read_timeout_ms.max(200),
    )));

    stream
        .write_all(&framed)
        .and_then(|()| stream.flush())
        .map_err(|error| map_io(error, &address))?;

    let read = read_replies(&mut codec, &mut stream, request)?;
    Ok(TcpOutcome {
        sent_bytes: framed.len(),
        sent_hex: hex_dump(&framed),
        replies: read.replies,
        leftover: read.leftover,
        elapsed_ms: started.elapsed().as_millis() as u64,
        issue: read.issue,
    })
}

/// 连一个 `host:port`，把「解析失败」与「连不上」分开报。
///
/// `pub(crate)`：连通检测（[`crate::conn`]）复用它，「先解析再连」这套判定
/// 只该有一份 —— 两处各写一遍，错误码迟早会漂。
pub(crate) fn connect(address: &str, timeout_ms: u64) -> NetResult<TcpStream> {
    let mut last = None;
    let timeout = Duration::from_millis(timeout_ms.max(200));
    // 先解析再连：解析失败与「连不上」在报告里是两件事（DNS 错了改主机名，
    // 拒绝连接看防火墙/端口）。
    let addresses = address
        .to_socket_addrs()
        .map_err(|error| NetError::with_detail(ErrorKind::Dns, format!("{address} {error}")))?;
    for candidate in addresses {
        match TcpStream::connect_timeout(&candidate, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(match last {
        Some(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            NetError::with_detail(ErrorKind::Timeout, address)
        }
        Some(error) => NetError::with_detail(ErrorKind::Connect, format!("{address} {error}")),
        None => NetError::with_detail(ErrorKind::Dns, address),
    })
}

/// 读应答：一直读到「对端安静」「收满帧数」「触到字节上限」三者之一。
///
/// 裁剪靠 [`Decoded::consumed`]：解码器自己知道「这一轮吃掉多少字节」，
/// 调用方 `drain(..consumed)` 即可，剩下的半帧尾巴原地留着等下一轮。
/// 这一点是相对参考仓的唯一结构性改动——源仓一次性把整个响应读进内存再解码，
/// 不需要偏移；这里是流式读（读到对端安静为止），没有偏移就没法边读边切。
fn read_replies(
    codec: &mut Box<dyn Codec>,
    stream: &mut TcpStream,
    request: &TcpRequest,
) -> NetResult<Replies> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut replies: Vec<Vec<u8>> = Vec::new();
    let mut chunk = vec![0u8; 8192];
    // `raw` 的语义是「整个响应就是一帧」：分块解码会把它切成好几帧，
    // 所以这一档只在读完之后解一次；其余协议边读边切，好让 `max_replies`
    // 能在对端持续推送时尽早收口。
    let incremental = codec.name() != "raw";

    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if incremental {
                    let decoded = codec.decode(&buffer);
                    let consumed = decoded.consumed.min(buffer.len());
                    replies.extend(decoded.frames);
                    buffer.drain(..consumed);
                }
                let received = buffer.len() + replies.iter().map(Vec::len).sum::<usize>();
                if replies.len() >= request.max_replies || received > request.max_bytes {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // 读超时：已经收到东西就认为对端说完了；一个字节都没有才是真的没应答。
                break;
            }
            Err(error) => return Err(NetError::io(error)),
        }
    }

    if !incremental && !buffer.is_empty() {
        let decoded = codec.decode(&buffer);
        let consumed = decoded.consumed.min(buffer.len());
        replies.extend(decoded.frames);
        buffer.drain(..consumed);
    }

    Ok(Replies {
        issue: codec.issue().cloned(),
        replies,
        leftover: buffer,
    })
}

/// 一轮读应答的结果。
///
/// 单独立结构体而不是三元组：`(Vec<Vec<u8>>, Vec<u8>, Option<DecodeIssue>)` 在
/// 调用点要按位置取值，`leftover` 与 `replies` 顺序写反了编译器也不会拦
///（两者都是 `Vec<u8>` 家族）。字段名才是这里的可读性来源。
struct Replies {
    replies: Vec<Vec<u8>>,
    leftover: Vec<u8>,
    issue: Option<DecodeIssue>,
}

fn map_io(error: std::io::Error, address: &str) -> NetError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            NetError::with_detail(ErrorKind::Timeout, address)
        }
        std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted => {
            NetError::with_detail(ErrorKind::Connect, format!("{address} {error}"))
        }
        _ => NetError::io(error),
    }
}

/// 十六进制日志（`3C 3F 78 6D` 形态，每行最多 32 字节，带行内 ASCII）。
///
/// 设计稿要求「日志支持 Hex/文本切换」，Hex 这一侧就是这里：一行 32 字节，
/// 右边跟可打印 ASCII（不可打印显示 `.`），贴进工单能直接对照抓包。
pub fn hex_dump(bytes: &[u8]) -> String {
    const PER_LINE: usize = 32;
    let mut out = String::new();
    for (line, chunk) in bytes.chunks(PER_LINE).enumerate() {
        let mut hex = String::new();
        for byte in chunk {
            hex.push_str(&format!("{byte:02X} "));
        }
        for _ in chunk.len()..PER_LINE {
            hex.push_str("   ");
        }
        let ascii: String = chunk
            .iter()
            .map(|byte| {
                if (0x20..0x7F).contains(byte) {
                    *byte as char
                } else {
                    '.'
                }
            })
            .collect();
        out.push_str(&format!("{:04X}  {hex} {ascii}\n", line * PER_LINE));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charset_round_trips_utf8_and_gbk() {
        assert_eq!(Charset::parse("GBK"), Charset::Gbk);
        assert_eq!(Charset::parse("utf-8"), Charset::Utf8);
        // GBK 里「中」是 0xD6 0xD0 —— 与 UTF-8 的三字节完全不同，这正是
        // 「对面收不到响应」最常见的原因。
        assert_eq!(Charset::Gbk.encode("中"), vec![0xD6, 0xD0]);
        assert_eq!(Charset::Gbk.decode(&[0xD6, 0xD0]), "中");
        assert_eq!(Charset::Utf8.encode("中"), "中".as_bytes().to_vec());
        // 解不了的字节不 panic，替换成 U+FFFD。
        assert!(Charset::Gbk.decode(&[0xFF]).contains('\u{FFFD}'));
    }

    #[test]
    fn hex_payload_accepts_common_pasting_styles() {
        assert_eq!(parse_hex("3C 3F").expect("空格"), vec![0x3C, 0x3F]);
        assert_eq!(parse_hex("3C3F").expect("连写"), vec![0x3C, 0x3F]);
        assert_eq!(parse_hex("0x3C,0x3F").expect("0x+逗号"), vec![0x3C, 0x3F]);
        assert_eq!(
            parse_hex("3C:3F\n78").expect("冒号+换行"),
            vec![0x3C, 0x3F, 0x78]
        );
        // 奇数字节 / 非法字符：报 Decode，不静默补零。
        assert_eq!(parse_hex("3C3").unwrap_err().kind(), ErrorKind::Decode);
        assert_eq!(parse_hex("ZZ").unwrap_err().kind(), ErrorKind::Decode);
        assert_eq!(parse_hex("  ").unwrap_err().kind(), ErrorKind::EmptyInput);
    }

    #[test]
    fn hex_dump_shows_offsets_and_ascii() {
        let dump = hex_dump(b"<xml>\x01\x02");
        assert!(dump.starts_with("0000  3C 78 6D 6C 3E 01 02"));
        assert!(dump.contains("<xml>."), "{dump}");
    }

    #[test]
    fn raw_and_ascii_length_exchanges_work_over_loopback() {
        // 起一个只应答一次的本地服务：收什么、回什么前面加 8 位长度头。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定");
        let port = listener.local_addr().expect("地址").port();
        let handle = std::thread::spawn(move || {
            use std::io::Read as _;
            let (mut socket, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 64];
            let count = socket.read(&mut buf).expect("read");
            // 回一个 ascii_length 帧：8 位长度 + `PONG`
            let _ = socket.write_all(format!("{:08}PONG", 4).as_bytes());
            let _ = socket.write_all(&buf[..count.min(1)]);
        });

        let request = TcpRequest {
            host: "127.0.0.1".into(),
            port,
            payload: Payload::Text {
                text: "hello".into(),
                charset: Charset::Utf8,
            },
            codec: CodecConfig {
                protocol: "raw".into(),
                ..CodecConfig::default()
            },
            read_timeout_ms: 400,
            ..TcpRequest::default()
        };
        let outcome = exchange(&request).expect("收发应成功");
        assert_eq!(outcome.sent_bytes, 5);
        // 十六进制日志每行带 `0000  ` 偏移，所以断言偏移之后的那一段。
        assert!(
            outcome.sent_hex.starts_with("0000  68 65 6C 6C 6F"),
            "{}",
            outcome.sent_hex
        );
        assert!(outcome.received_bytes() > 0);
        handle.join().expect("server thread");
    }

    #[test]
    fn connecting_to_a_closed_port_reports_connect_not_io() {
        // 端口 1 基本不可能有人监听（需要 root 才能绑）。
        let request = TcpRequest {
            host: "127.0.0.1".into(),
            port: 1,
            payload: Payload::Text {
                text: "x".into(),
                charset: Charset::Utf8,
            },
            connect_timeout_ms: 300,
            ..TcpRequest::default()
        };
        let error = exchange(&request).unwrap_err();
        assert!(
            matches!(error.kind(), ErrorKind::Connect | ErrorKind::Timeout),
            "{error:?}"
        );
    }

    #[test]
    fn empty_host_or_payload_is_empty_input() {
        let request = TcpRequest::default();
        assert_eq!(
            exchange(&request).unwrap_err().kind(),
            ErrorKind::EmptyInput
        );

        let request = TcpRequest {
            host: "127.0.0.1".into(),
            port: 80,
            payload: Payload::Text {
                text: "  ".into(),
                charset: Charset::Utf8,
            },
            ..TcpRequest::default()
        };
        assert_eq!(
            exchange(&request).unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
    }
}
