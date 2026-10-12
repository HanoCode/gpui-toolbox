//! `MONITOR` 实时流：一条**专用连接**上的增量日志（`docs/redis-design.md` §4.7，M1.4）。
//!
//! 为什么单独一条连接：`MONITOR` 一旦开启，**该连接上再也不能发别的命令**——
//! 所有后续应答都会被监控行顶掉。所以它绝不能在主连接上跑
//! （设计文档 §4.7 明确要求「`MONITOR` 用专用连接且可停止」）。
//!
//! 为什么是「拉」而不是「推」：本 crate 的纪律是**不开线程、不建 runtime**
//! （见 `lib.rs` 的分层说明）。所以这里不建后台线程，而是把
//! [`MonitorSession::poll`] 设计成**带读超时的批读**：调用方（`duck-app` 的
//! `background_executor`）按节拍调它，没有新行就很快返回空 Vec。
//!
//! ⚠️ `recv_response` 依赖 redis-rs 内部的有缓冲读，读超时发生在「一行还没收全」
//! 时，已收到的半行会留在它自己的缓冲里，下一次调用继续读——所以超时是**安全的**。
//! 但若连接真的断了（非超时错误），本类型会标记 [`Self::is_closed`] 并停止再读，
//! 不再对一条坏连接反复报错。

use std::time::Duration;

use redis::{Connection, Value};

use crate::config::RedisConfig;
use crate::error::RedisError;
use crate::value::as_text;

/// 单次 [`MonitorSession::poll`] 的最长等待。
///
/// 200 毫秒：比界面刷新节拍略短，既不会让「有没有新行」的判定显得迟钝，
/// 也不会在空闲时把后台线程占死。
pub const POLL_TIMEOUT: Duration = Duration::from_millis(200);

/// `MONITOR` 的请求帧（`*1\r\n$7\r\nMONITOR\r\n`）。
///
/// 手写 RESP 而不是走 `redis::cmd`：`redis::cmd(...).query()` 会**等一个完整应答**，
/// 而 `MONITOR` 只回一个 `+OK` 就转成流，用 `send_packed_command` + 自己读首帧
/// 才能把连接留在手里继续收监控行。
const MONITOR_REQUEST: &[u8] = b"*1\r\n$7\r\nMONITOR\r\n";

/// 一条已在 `MONITOR` 模式的专用连接。
pub struct MonitorSession {
    conn: Connection,
    target: String,
    closed: bool,
}

impl MonitorSession {
    /// 建一条专用连接并开启 `MONITOR`。
    ///
    /// 建连走与主连接同一套守卫（[`crate::conn::open_connection`]）：
    /// 档案里启用了 TLS / SSH 隧道时**明确报错**，不静默直连。
    pub fn start(cfg: &RedisConfig, password: Option<&str>) -> Result<Self, RedisError> {
        let mut conn = crate::conn::open_connection(cfg, password)?;
        conn.set_read_timeout(Some(POLL_TIMEOUT))
            .map_err(RedisError::from)?;
        conn.send_packed_command(MONITOR_REQUEST)
            .map_err(RedisError::from)?;
        // 首帧是 `+OK`；把它读掉，之后收到的才是监控行。
        match conn.recv_response().map_err(RedisError::from)? {
            Value::Okay | Value::SimpleString(_) => {}
            other => {
                return Err(RedisError::Command(format!(
                    "unexpected MONITOR acknowledgement: {other:?}"
                )));
            }
        }
        Ok(Self {
            conn,
            target: format!("{}:{}/{}", cfg.host.trim(), cfg.effective_port(), cfg.db),
            closed: false,
        })
    }

    /// 监控目标的展示串（`host:port/db`）。
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// 连接是否已经断掉（读到非超时错误后为真，此后不再尝试读取）。
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// 取一批新到的监控行（最多 `max` 行）。
    ///
    /// - 有行：返回它们（此时不等待，直接把已在缓冲里的行读干）；
    /// - 没有新行：等 [`POLL_TIMEOUT`] 后返回空 `Vec`（**不是错误**）；
    /// - 连接断了：返回 `Err`，并把 [`Self::is_closed`] 置真。
    pub fn poll(&mut self, max: usize) -> Result<Vec<String>, RedisError> {
        if self.closed || max == 0 {
            return Ok(Vec::new());
        }
        let mut lines = Vec::new();
        while lines.len() < max {
            match self.conn.recv_response() {
                Ok(value) => {
                    let text = as_text(&value).unwrap_or_default();
                    if !text.is_empty() {
                        lines.push(text);
                    }
                }
                // 读超时 = 「这一批读完了」，不是失败。
                Err(err) if err.is_timeout() => break,
                Err(err) => {
                    self.closed = true;
                    return Err(RedisError::from(err));
                }
            }
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 请求帧必须是一个合法的 RESP 数组，且命令名长度与 `$7` 一致。
    ///
    /// 手写 RESP 最容易错的就是长度前缀 —— 长度写错服务端会静默把它当成
    /// 一条错误命令，界面上表现为「`MONITOR` 打开了但一条都收不到」。
    #[test]
    fn monitor_request_frame_is_well_formed() {
        let text = std::str::from_utf8(MONITOR_REQUEST).expect("请求帧是 ASCII");
        assert_eq!(text, "*1\r\n$7\r\nMONITOR\r\n");
        let name = "MONITOR";
        assert_eq!(name.len(), 7, "$7 必须等于命令名字节数");
    }
}
