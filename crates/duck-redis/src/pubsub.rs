//! `SUBSCRIBE` 实时订阅：一条**专用连接**上的消息流。
//!
//! 为什么单独一条连接：与 `MONITOR` 同一条纪律（见 `monitor.rs`）——
//! 订阅一旦开启，该连接上再也不能发别的命令（所有后续读写都被消息流顶掉），
//! 所以绝不能复用主连接。
//!
//! 为什么是「拉」而不是「推」：本 crate 不开线程、不建 runtime（`lib.rs` 分层说明），
//! 调用方按节拍调 [`PubSubSession::poll`]，没有新消息就很快返回空 `Vec`。
//! 读超时的安全性论证同 `monitor.rs`（半行留在 redis-rs 自己的缓冲里，下次继续）。

use std::time::Duration;

use redis::{Connection, Value};

use crate::config::RedisConfig;
use crate::error::RedisError;
use crate::value::as_text;

/// 单次 [`PubSubSession::poll`] 的最长等待（与 `monitor::POLL_TIMEOUT` 同档）。
pub const POLL_TIMEOUT: Duration = Duration::from_millis(200);

/// `SUBSCRIBE` 的命令名长度（RESP 手写帧的 `$9` 校验用）。
const SUBSCRIBE_NAME: &str = "SUBSCRIBE";

/// 一条已在订阅模式的专用连接。
pub struct PubSubSession {
    conn: Connection,
    target: String,
    channels: Vec<String>,
    closed: bool,
}

/// 一条推送消息（`message` 帧）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubSubMessage {
    /// 频道名。
    pub channel: String,
    /// 载荷（按 UTF-8 lossy 转写；二进制载荷会带替换符）。
    pub payload: String,
}

/// 手拼 `SUBSCRIBE` 的 RESP 请求帧。
///
/// 与 `MONITOR` 同理：`redis::cmd(...).query()` 会等「一个完整应答」，而订阅连接
/// 之后的应答全是推送帧，必须把连接留在自己手里继续读。
fn subscribe_request(channels: &[String]) -> Vec<u8> {
    debug_assert!(!channels.is_empty());
    let mut frame = format!(
        "*{}\r\n${}\r\n{SUBSCRIBE_NAME}\r\n",
        channels.len() + 1,
        SUBSCRIBE_NAME.len()
    )
    .into_bytes();
    for channel in channels {
        frame.extend_from_slice(format!("${}\r\n", channel.len()).as_bytes());
        frame.extend_from_slice(channel.as_bytes());
        frame.extend_from_slice(b"\r\n");
    }
    frame
}

/// 从一个推送帧里取出文本（RESP2 的 `SimpleString` / `Bulk` 都当字符串看）。
fn frame_text(value: &Value) -> Option<String> {
    as_text(value).or_else(|| match value {
        Value::Int(number) => Some(number.to_string()),
        _ => None,
    })
}

impl PubSubSession {
    /// 建一条专用连接并订阅给定频道。
    ///
    /// 建连与握手都走与主连接同一套守卫（[`crate::conn::open_connection`]）；
    /// 档案启用了 TLS / SSH 隧道时**明确报错**，不静默直连。
    pub fn start(
        cfg: &RedisConfig,
        password: Option<&str>,
        channels: &[String],
    ) -> Result<Self, RedisError> {
        if channels.is_empty() {
            return Err(RedisError::Config("no channels to subscribe".into()));
        }
        let mut conn = crate::conn::open_connection(cfg, password)?;
        conn.set_read_timeout(Some(POLL_TIMEOUT))
            .map_err(RedisError::from)?;
        conn.send_packed_command(&subscribe_request(channels))
            .map_err(RedisError::from)?;
        // 每个频道一条确认帧（`[subscribe, 频道, 当前订阅数]`），全部读掉；
        // 之后到来的才是 `message` 推送帧。
        for channel in channels {
            match conn.recv_response().map_err(RedisError::from)? {
                Value::Array(items) if frame_kind(&items).as_deref() == Some("subscribe") => {}
                other => {
                    return Err(RedisError::Command(format!(
                        "unexpected SUBSCRIBE acknowledgement for `{channel}`: {other:?}"
                    )));
                }
            }
        }
        Ok(Self {
            conn,
            target: format!("{}:{}/{}", cfg.host.trim(), cfg.effective_port(), cfg.db),
            channels: channels.to_vec(),
            closed: false,
        })
    }

    /// 订阅目标（`host:port/db`）。
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// 已订阅的频道（快照；订阅后无法再改，改频道要重开会话）。
    #[must_use]
    pub fn channels(&self) -> &[String] {
        &self.channels
    }

    /// 连接是否已经断掉（读到非超时错误后为真，此后不再尝试读取）。
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// 取一批新到的消息（最多 `max` 条）。
    ///
    /// - 有消息：返回它们（不等待，把缓冲里的读干）；
    /// - 没有：等 [`POLL_TIMEOUT`] 后返回空 `Vec`（**不是错误**）；
    /// - 连接断了：返回 `Err`，并把 [`Self::is_closed`] 置真。
    ///
    /// 订阅确认 / 取消订阅等**非 message 帧**在这里被静默跳过：它们是协议
    /// 自己的心跳，不是用户关心的内容。
    pub fn poll(&mut self, max: usize) -> Result<Vec<PubSubMessage>, RedisError> {
        let mut messages = Vec::new();
        if self.closed || max == 0 {
            return Ok(messages);
        }
        while messages.len() < max {
            match self.conn.recv_response() {
                Ok(Value::Array(items)) => {
                    if let Some(message) = parse_message(&items) {
                        messages.push(message);
                    }
                }
                Ok(_) => {}
                // 读超时 = 「这一批读完了」，不是失败（同 `monitor.rs`）。
                Err(err) if err.is_timeout() => break,
                Err(err) => {
                    self.closed = true;
                    return Err(RedisError::from(err));
                }
            }
        }
        Ok(messages)
    }
}

/// 确认帧的种类（首元素文本）；不是数组或首元素不是文本时为 `None`。
fn frame_kind(items: &[Value]) -> Option<String> {
    items.first().and_then(frame_text)
}

/// 从一个推送帧解析 `message`；其余（确认帧）返回 `None`。
fn parse_message(items: &[Value]) -> Option<PubSubMessage> {
    if frame_kind(items).as_deref() != Some("message") {
        return None;
    }
    let channel = items.get(1).and_then(frame_text)?;
    let payload = items.get(2).and_then(frame_text).unwrap_or_default();
    Some(PubSubMessage { channel, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 请求帧必须是合法 RESP 数组：`SUBSCRIBE` + 每频道一项，长度前缀与字节长度一致。
    #[test]
    fn subscribe_request_frame_is_well_formed() {
        let frame = subscribe_request(&["chat.1".to_owned(), "chat.2".to_owned()]);
        let text = std::str::from_utf8(&frame).expect("请求帧是 ASCII");
        assert!(text.starts_with("*3\r\n$9\r\nSUBSCRIBE\r\n"));
        assert!(text.contains("$6\r\nchat.1\r\n"));
        assert!(text.contains("$6\r\nchat.2\r\n"));
    }

    /// 空频道列表不该被拼进请求帧（调用方在 `start` 已经拦，这里是最后防线）。
    #[test]
    #[should_panic(expected = "channels.is_empty()")]
    fn subscribe_request_rejects_empty() {
        subscribe_request(&[]);
    }

    /// `message` 帧解析：`[message, 频道, 载荷]`；确认帧一律忽略。
    #[test]
    fn parse_message_and_skip_confirmations() {
        let message = parse_message(&[
            Value::BulkString(b"message".to_vec()),
            Value::BulkString(b"chat.1".to_vec()),
            Value::BulkString(b"hello".to_vec()),
        ])
        .expect("message 帧应被解析");
        assert_eq!(message.channel, "chat.1");
        assert_eq!(message.payload, "hello");

        let confirmation = Value::Array(vec![
            Value::BulkString(b"subscribe".to_vec()),
            Value::BulkString(b"chat.1".to_vec()),
            Value::Int(1),
        ]);
        if let Value::Array(items) = confirmation {
            assert!(parse_message(&items).is_none(), "确认帧不该被当成消息");
        }
    }
}
