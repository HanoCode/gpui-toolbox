//! 报文分帧：原始 / 长度字段 / ASCII 长度 / 分隔符 / 定长五种。
//!
//! 逐字移植参考仓 `src-tauri/src/net/codec.rs`（219 行）：五套编解码的**判定规则
//! 一行没改**（长度字段 1/2/4 字节大端、ASCII 头部补零、分隔符按字节序列切、
//! 定长帧不足补 `0x00`），只做两件事：
//!
//! 1. **去掉 async**：参考仓的 `Codec` 是纯函数，`recv_exact` / `send_framed` 是
//!    tokio 的甜味剂；这里连接是阻塞 socket，「读够 N 字节 / 读到超时」在 `tcp.rs`
//!    用 `set_read_timeout` 表达即可。
//! 2. **错误从中文串换成稳定枚举**（[`DecodeIssue`]）：内核不许拼中文，整句由
//!    插件层按码 + 参数翻译。参考仓把 `format!("报文长度 {} 超过上限 {}", …)` 直接
//!    塞进 `last_error` 给前端显示，这里保留**同样的信息量**，只是拆成码 + 数字。

/// 解码失败的原因（码 + 参数，不含任何自然语言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeIssue {
    /// 长度字段宽度不支持（只认 1 / 2 / 4）。
    UnsupportedWidth(usize),
    /// 长度字段是 0（无意义帧）。
    ZeroLength,
    /// ASCII 头部出现非 ASCII 字节，附带那几个字节的十六进制。
    HeaderNotAscii(String),
    /// ASCII 头部全是 `0`（宽度内没有有效数字）。
    HeaderAllZeros,
    /// ASCII 头部不是数字。
    HeaderNotNumber(String),
    /// 声明的报文体超过上限。
    TooLarge { len: usize, max: usize },
    /// 报文体还没读全（缓冲区不够）。
    Incomplete { expected: usize, available: usize },
}

/// 一次解码的结果。
///
/// 比参考仓的 `Vec<Vec<u8>>` 多一个 `consumed`：源仓是一次性把整个响应读进内存
/// 再解码，不需要知道「吃掉多少字节」；本仓是流式读（读到对端安静为止），
/// 必须能精确切掉已消费的前缀、把没凑成帧的尾巴留到下一轮。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decoded {
    /// 切出来的完整帧。
    pub frames: Vec<Vec<u8>>,
    /// 这段缓冲里已被识别为「帧 + 帧间头部/分隔符」的字节数。
    pub consumed: usize,
}

/// 一个编解码器。
pub trait Codec {
    /// 从缓冲区里切出**完整的**帧，并报出消费了多少字节。
    fn decode(&mut self, buf: &[u8]) -> Decoded;
    /// 把一条报文编码成要发出去的字节。
    fn encode(&self, msg: &[u8], out: &mut Vec<u8>);
    /// 稳定名（报告里显示）。
    fn name(&self) -> &'static str;
    /// 上一次 `decode` 遇到的问题（`None` = 干净）。
    fn issue(&self) -> Option<&DecodeIssue>;
}

/// 编解码配置（与参考仓 `CodecConfig` 字段一一对应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    pub protocol: String,
    pub header_len: Option<usize>,
    pub max_body_len: Option<usize>,
    pub delimiter: Option<String>,
    pub fixed_frame_len: Option<usize>,
}

impl Default for CodecConfig {
    fn default() -> Self {
        Self {
            protocol: "raw".to_string(),
            header_len: Some(4),
            max_body_len: Some(10 * 1024 * 1024),
            delimiter: None,
            fixed_frame_len: None,
        }
    }
}

/// 原始：整个缓冲区就是一帧，不做任何切分。
#[derive(Default)]
pub struct RawCodec;

impl Codec for RawCodec {
    fn decode(&mut self, buf: &[u8]) -> Decoded {
        if buf.is_empty() {
            Decoded::default()
        } else {
            Decoded {
                frames: vec![buf.to_vec()],
                consumed: buf.len(),
            }
        }
    }

    fn encode(&self, msg: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(msg);
    }

    fn name(&self) -> &'static str {
        "raw"
    }

    fn issue(&self) -> Option<&DecodeIssue> {
        None
    }
}

/// 长度字段（二进制，大端 1 / 2 / 4 字节）。
pub struct LengthFieldCodec {
    pub header_len: usize,
    pub max_body_len: usize,
    issue: Option<DecodeIssue>,
}

impl LengthFieldCodec {
    pub fn new(header_len: usize, max_body_len: usize) -> Self {
        Self {
            header_len,
            max_body_len,
            issue: None,
        }
    }
}

impl Codec for LengthFieldCodec {
    fn decode(&mut self, buf: &[u8]) -> Decoded {
        let mut frames = Vec::new();
        let mut pos = 0;
        while pos + self.header_len <= buf.len() {
            let body_len = match self.header_len {
                1 => buf[pos] as usize,
                2 => u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize,
                4 => u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
                    as usize,
                other => {
                    self.issue = Some(DecodeIssue::UnsupportedWidth(other));
                    break;
                }
            };
            if body_len == 0 {
                self.issue = Some(DecodeIssue::ZeroLength);
                break;
            }
            if body_len > self.max_body_len {
                self.issue = Some(DecodeIssue::TooLarge {
                    len: body_len,
                    max: self.max_body_len,
                });
                break;
            }
            let frame_end = pos + self.header_len + body_len;
            if frame_end > buf.len() {
                self.issue = Some(DecodeIssue::Incomplete {
                    expected: body_len,
                    available: buf.len() - pos - self.header_len,
                });
                break;
            }
            frames.push(buf[pos + self.header_len..frame_end].to_vec());
            pos = frame_end;
            self.issue = None;
        }
        Decoded {
            frames,
            consumed: pos,
        }
    }

    fn encode(&self, msg: &[u8], out: &mut Vec<u8>) {
        match self.header_len {
            1 => out.push(msg.len() as u8),
            2 => out.extend_from_slice(&(msg.len() as u16).to_be_bytes()),
            4 => out.extend_from_slice(&(msg.len() as u32).to_be_bytes()),
            _ => {}
        }
        out.extend_from_slice(msg);
    }

    fn name(&self) -> &'static str {
        "length_field"
    }

    fn issue(&self) -> Option<&DecodeIssue> {
        self.issue.as_ref()
    }
}

/// 分隔符（默认 `\n`）。
pub struct DelimiterCodec {
    pub delimiter: Vec<u8>,
}

impl Codec for DelimiterCodec {
    fn decode(&mut self, buf: &[u8]) -> Decoded {
        let mut frames = Vec::new();
        if self.delimiter.is_empty() {
            return Decoded::default();
        }
        let mut start = 0;
        while let Some(position) = buf[start..]
            .windows(self.delimiter.len())
            .position(|window| window == self.delimiter)
        {
            let end = start + position;
            if end > start {
                frames.push(buf[start..end].to_vec());
            }
            start = end + self.delimiter.len();
        }
        Decoded {
            frames,
            consumed: start,
        }
    }

    fn encode(&self, msg: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(msg);
        out.extend_from_slice(&self.delimiter);
    }

    fn name(&self) -> &'static str {
        "delimiter"
    }

    fn issue(&self) -> Option<&DecodeIssue> {
        None
    }
}

/// 定长帧（不足补 `0x00`）。
pub struct FixedLengthCodec {
    pub frame_len: usize,
}

impl Codec for FixedLengthCodec {
    fn decode(&mut self, buf: &[u8]) -> Decoded {
        let mut frames = Vec::new();
        if self.frame_len == 0 {
            return Decoded::default();
        }
        let mut pos = 0;
        while pos + self.frame_len <= buf.len() {
            frames.push(buf[pos..pos + self.frame_len].to_vec());
            pos += self.frame_len;
        }
        Decoded {
            frames,
            consumed: pos,
        }
    }

    fn encode(&self, msg: &[u8], out: &mut Vec<u8>) {
        let copy_len = msg.len().min(self.frame_len);
        out.extend_from_slice(&msg[..copy_len]);
        out.resize(out.len() + self.frame_len - copy_len, 0);
    }

    fn name(&self) -> &'static str {
        "fixed_length"
    }

    fn issue(&self) -> Option<&DecodeIssue> {
        None
    }
}

/// ASCII 长度头部（十进制、左侧补零）。
pub struct AsciiLengthCodec {
    pub header_len: usize,
    pub max_body_len: usize,
    issue: Option<DecodeIssue>,
}

impl AsciiLengthCodec {
    pub fn new(header_len: usize, max_body_len: usize) -> Self {
        Self {
            header_len,
            max_body_len,
            issue: None,
        }
    }
}

impl Codec for AsciiLengthCodec {
    fn decode(&mut self, buf: &[u8]) -> Decoded {
        let mut frames = Vec::new();
        let mut pos = 0;
        while pos + self.header_len <= buf.len() {
            let header = &buf[pos..pos + self.header_len];
            let header_str = match std::str::from_utf8(header) {
                Ok(text) => text,
                Err(_) => {
                    self.issue = Some(DecodeIssue::HeaderNotAscii(hex_bytes(header)));
                    break;
                }
            };
            let trimmed = header_str.trim_start_matches('0');
            if trimmed.is_empty() {
                self.issue = Some(DecodeIssue::HeaderAllZeros);
                break;
            }
            let body_len: usize = match trimmed.parse() {
                Ok(len) => len,
                Err(_) => {
                    self.issue = Some(DecodeIssue::HeaderNotNumber(header_str.to_string()));
                    break;
                }
            };
            if body_len > self.max_body_len {
                self.issue = Some(DecodeIssue::TooLarge {
                    len: body_len,
                    max: self.max_body_len,
                });
                break;
            }
            let frame_end = pos + self.header_len + body_len;
            if frame_end > buf.len() {
                self.issue = Some(DecodeIssue::Incomplete {
                    expected: body_len,
                    available: buf.len() - pos - self.header_len,
                });
                break;
            }
            frames.push(buf[pos + self.header_len..frame_end].to_vec());
            pos = frame_end;
            self.issue = None;
        }
        Decoded {
            frames,
            consumed: pos,
        }
    }

    fn encode(&self, msg: &[u8], out: &mut Vec<u8>) {
        // 与参考仓一致：超宽时 `{:0width$}` 不会被截断，长度会撑破头部宽度 ——
        // 那种报文本来就不该按这个协议发（解码端会把它读成别的东西）。
        let header = format!("{:0width$}", msg.len(), width = self.header_len);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(msg);
    }

    fn name(&self) -> &'static str {
        "ascii_length"
    }

    fn issue(&self) -> Option<&DecodeIssue> {
        self.issue.as_ref()
    }
}

/// 按配置产出编解码器（未知协议名 → 原始，与参考仓 `_ => RawCodec` 同）。
pub fn create(config: &CodecConfig) -> Box<dyn Codec> {
    match config.protocol.as_str() {
        "length_field" => Box::new(LengthFieldCodec::new(
            config.header_len.unwrap_or(4),
            config.max_body_len.unwrap_or(10 * 1024 * 1024),
        )),
        "ascii_length" => Box::new(AsciiLengthCodec::new(
            config.header_len.unwrap_or(8),
            config.max_body_len.unwrap_or(10 * 1024 * 1024),
        )),
        "delimiter" => Box::new(DelimiterCodec {
            delimiter: config
                .delimiter
                .as_ref()
                .map(|text| text.as_bytes().to_vec())
                .unwrap_or_else(|| b"\n".to_vec()),
        }),
        "fixed_length" => Box::new(FixedLengthCodec {
            frame_len: config.fixed_frame_len.unwrap_or(1024),
        }),
        _ => Box::new(RawCodec),
    }
}

/// 字节串 → `3C 3F 78 6D` 形态（ASCII 头部不是 UTF-8 时要能看懂收到了什么）。
pub fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_passes_everything_through() {
        let mut codec = RawCodec;
        let decoded = codec.decode(b"abc");
        assert_eq!(decoded.frames, vec![b"abc".to_vec()]);
        assert_eq!(decoded.consumed, 3);
        assert!(codec.decode(b"").frames.is_empty());
    }

    #[test]
    fn length_field_round_trips_one_two_and_four_bytes() {
        for width in [1usize, 2, 4] {
            let codec = LengthFieldCodec::new(width, 1024);
            let mut framed = Vec::new();
            codec.encode(b"hello", &mut framed);
            assert_eq!(framed.len(), width + 5);
            let mut decoder = LengthFieldCodec::new(width, 1024);
            let decoded = decoder.decode(&framed);
            assert_eq!(decoded.frames, vec![b"hello".to_vec()]);
            assert_eq!(decoded.consumed, framed.len());
        }
    }

    #[test]
    fn length_field_splits_multiple_frames_and_keeps_the_tail() {
        let codec = LengthFieldCodec::new(2, 1024);
        let mut buf = Vec::new();
        codec.encode(b"one", &mut buf);
        codec.encode(b"two", &mut buf);
        let complete = buf.len();
        // 尾部再塞半个帧：只能切出两个完整帧，剩下的不算帧、也不该被消费掉。
        buf.extend_from_slice(&[0x00, 0x05, b'x']);

        let mut decoder = LengthFieldCodec::new(2, 1024);
        let decoded = decoder.decode(&buf);
        assert_eq!(decoded.frames, vec![b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(decoded.consumed, complete);
        // 第三个帧声明 5 字节、只给了 1 字节 → 必须报 Incomplete，而不是静默吞掉。
        assert_eq!(
            decoder.issue(),
            Some(&DecodeIssue::Incomplete {
                expected: 5,
                available: 1
            })
        );
    }

    #[test]
    fn length_field_rejects_zero_and_oversized_bodies() {
        let mut decoder = LengthFieldCodec::new(4, 8);
        assert!(decoder.decode(&[0, 0, 0, 0]).frames.is_empty());
        assert_eq!(decoder.issue(), Some(&DecodeIssue::ZeroLength));

        let mut decoder = LengthFieldCodec::new(4, 8);
        assert!(decoder.decode(&[0, 0, 0, 9, 1, 2]).frames.is_empty());
        assert_eq!(
            decoder.issue(),
            Some(&DecodeIssue::TooLarge { len: 9, max: 8 })
        );
    }

    #[test]
    fn length_field_rejects_unsupported_width() {
        let mut decoder = LengthFieldCodec::new(3, 1024);
        assert!(decoder.decode(&[0, 0, 1]).frames.is_empty());
        assert_eq!(decoder.issue(), Some(&DecodeIssue::UnsupportedWidth(3)));
    }

    #[test]
    fn ascii_length_reads_padded_decimal_and_flags_bad_headers() {
        let codec = AsciiLengthCodec::new(8, 1024);
        let mut buf = Vec::new();
        codec.encode(b"payload", &mut buf);
        assert_eq!(&buf[..8], b"00000007");

        let mut decoder = AsciiLengthCodec::new(8, 1024);
        assert_eq!(decoder.decode(&buf).frames, vec![b"payload".to_vec()]);

        // 头部全 0：宽度内没有有效数字。
        let mut decoder = AsciiLengthCodec::new(4, 1024);
        assert!(decoder.decode(b"0000").frames.is_empty());
        assert_eq!(decoder.issue(), Some(&DecodeIssue::HeaderAllZeros));

        // 头部不是数字：要把原文带回报告（用户才知道自己发成了什么）。
        let mut decoder = AsciiLengthCodec::new(4, 1024);
        assert!(decoder.decode(b"00AB").frames.is_empty());
        assert_eq!(
            decoder.issue(),
            Some(&DecodeIssue::HeaderNotNumber("00AB".to_string()))
        );

        // 非 ASCII 头部：给十六进制，别给乱码。
        let mut decoder = AsciiLengthCodec::new(2, 1024);
        assert!(decoder.decode(&[0xFF, 0xFE]).frames.is_empty());
        assert_eq!(
            decoder.issue(),
            Some(&DecodeIssue::HeaderNotAscii("FF FE".to_string()))
        );
    }

    #[test]
    fn delimiter_codec_splits_on_the_sequence() {
        let mut codec = DelimiterCodec {
            delimiter: b"\r\n".to_vec(),
        };
        let decoded = codec.decode(b"A\r\nB\r\nC");
        assert_eq!(decoded.frames, vec![b"A".to_vec(), b"B".to_vec()]);
        // 只消费到最后一个分隔符；`C` 是尾巴，留到下一轮。
        assert_eq!(decoded.consumed, 6);
        // 只有分隔符（空帧）不产出空报文。
        assert!(codec.decode(b"\r\n").frames.is_empty());
        let mut out = Vec::new();
        codec.encode(b"X", &mut out);
        assert_eq!(out, b"X\r\n");
    }

    #[test]
    fn fixed_length_pads_short_bodies_with_zeros() {
        let codec = FixedLengthCodec { frame_len: 4 };
        let mut out = Vec::new();
        codec.encode(b"ab", &mut out);
        assert_eq!(out, vec![b'a', b'b', 0, 0]);
        // 超长报文按参考仓口径截断到帧长。
        let mut out = Vec::new();
        codec.encode(b"abcdef", &mut out);
        assert_eq!(out, b"abcd".to_vec());

        let mut decoder = FixedLengthCodec { frame_len: 4 };
        let decoded = decoder.decode(b"abcdXY");
        assert_eq!(decoded.frames, vec![b"abcd".to_vec()]);
        assert_eq!(decoded.consumed, 4);
    }

    #[test]
    fn factory_maps_names_and_falls_back_to_raw() {
        let config = CodecConfig {
            protocol: "ascii_length".into(),
            header_len: Some(8),
            ..CodecConfig::default()
        };
        assert_eq!(create(&config).name(), "ascii_length");
        let unknown = CodecConfig {
            protocol: "whatever".into(),
            ..CodecConfig::default()
        };
        assert_eq!(create(&unknown).name(), "raw");
    }
}
