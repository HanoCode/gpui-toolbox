//! 插件进程的**入口侧**：读一行请求、把产出写成事件行。
//!
//! [`runner`](crate::runner) 是宿主侧（父进程读 stdout），本模块是插件侧
//! （子进程写 stdout）。两边共用 [`RunRequest`] / [`Event`] 同一份结构体，
//! 协议只有一个真源。
//!
//! 插件作者的入口就一句话：
//!
//! ```no_run
//! fn main() -> std::process::ExitCode {
//!     duck_plugin_process::serve(|request, out| {
//!         out.log("开始处理");
//!         Ok(format!("收到 {} 字节", request.input.len()))
//!     })
//! }
//! ```
//!
//! `serve` 负责：读请求（上限 [`MAX_LINE_BYTES`]）、跑 handler、把返回值写成
//! `{"type":"result"}`、把 `Err` 写成 `{"type":"error"}` 并以非零码退出。
//! 插件不必自己碰 stdout —— 手写 JSONL 迟早会漏转义。

use std::io::{BufRead, Write};
use std::process::ExitCode;

use duck_plugin::PluginError;

use crate::protocol::{Event, MAX_LINE_BYTES, RunRequest};

/// 结构化正文分片的默认大小：256 KiB（单行上限 1 MiB，转义后仍有余量）。
pub const BODY_CHUNK_BYTES: usize = 256 * 1024;

/// 插件侧的事件出口（`serve` 传给 handler 的写入器）。
pub struct PluginOutput<W: Write> {
    out: W,
}

impl<W: Write> PluginOutput<W> {
    /// 包一个写入器（一般是 stdout）。
    pub fn new(out: W) -> Self {
        Self { out }
    }

    /// 一行过程日志（宿主转发到日志与 log 渲染器）。
    pub fn log(&mut self, line: impl Into<String>) {
        self.emit(&Event::Log {
            line: line.into(),
            level: None,
        });
    }

    /// 带级别的一行过程日志：`info` / `warn` / `error` / `success`。
    ///
    /// 宿主按级别给日志行着色；除此之外没有别的语义——日志不会因为
    /// 级别是 `error` 就把一次运行判失败（那是 `error` 事件 / 非零退出码的事）。
    pub fn log_with_level(&mut self, level: &str, line: impl Into<String>) {
        self.emit(&Event::Log {
            line: line.into(),
            level: Some(level.to_owned()),
        });
    }

    /// 进度；`total = None` 表示不定总量。
    pub fn progress(&mut self, current: u64, total: Option<u64>, message: Option<&str>) {
        self.emit(&Event::Progress {
            current,
            total,
            message: message.map(str::to_owned),
        });
    }

    /// 结构化正文的一个分片（`kind = "diff"` / `"table"` 这类富结果）。
    ///
    /// 一次运行里可以发多片，宿主按顺序拼成一段 JSON，在 `result` 到达时解析。
    /// 一片建议 ≤ 256 KiB：单行上限是 [`MAX_LINE_BYTES`]（1 MiB），切太满会在
    /// JSON 转义后越界。
    pub fn body_chunk(&mut self, chunk: impl Into<String>) {
        self.emit(&Event::Body {
            chunk: chunk.into(),
        });
    }

    /// 把一整段 JSON 按 [`BODY_CHUNK_BYTES`] 切块发出去。
    ///
    /// 切在**字节**边界上不安全（UTF-8 会被切开），所以按 `char_indices` 找最近的
    /// 字符边界——中文文件名、中文日志都在 diff 里，切坏了宿主那边就是「body 不是
    /// 合法 JSON」。
    pub fn body(&mut self, json: &str) {
        let mut start = 0;
        while start < json.len() {
            let mut end = (start + BODY_CHUNK_BYTES).min(json.len());
            while !json.is_char_boundary(end) {
                end -= 1;
            }
            if end == start {
                // 理论上不可达（单字符 ≤ 4 字节）；留个保险免得死循环。
                end = json.len();
            }
            self.body_chunk(&json[start..end]);
            start = end;
        }
    }

    /// 写一条事件；写失败只记 stderr（宿主已经走了，再报也没人接）。
    fn emit(&mut self, event: &Event) {
        if let Ok(line) = serde_json::to_string(event) {
            if writeln!(self.out, "{line}").is_err() {
                let _ = writeln!(std::io::stderr(), "duck plugin: stdout closed");
            }
            let _ = self.out.flush();
        }
    }

    /// 成功产出（`serve` 在 handler 返回 `Ok` 时自动调用；单独用可多次输出）。
    pub fn result(&mut self, output: impl Into<String>) {
        self.emit(&Event::Result {
            output: output.into(),
        });
    }

    /// 自报失败。
    pub fn error(&mut self, message: impl Into<String>) {
        self.emit(&Event::Error {
            message: message.into(),
        });
    }
}

/// 读一行请求（上限 [`MAX_LINE_BYTES`]，超限按协议违约报错）。
///
/// # Errors
/// 读失败、超限、非 JSON、字段缺失都返回 [`PluginError`]。
pub fn read_request<R: BufRead>(reader: &mut R) -> Result<RunRequest, PluginError> {
    let mut line = String::new();
    // `Read::take` 按值收 `self`，对 `&mut R` 要显式 UFCS 才不会把底层 reader 移走。
    let limit = MAX_LINE_BYTES as u64 + 1;
    let mut limited = std::io::Read::take(&mut *reader, limit);
    let read = limited
        .read_line(&mut line)
        .map_err(|error| PluginError::Execution(format!("read run request: {error}")))?;
    if read == 0 {
        return Err(PluginError::Execution(
            "no run request on stdin (expected one JSON line)".into(),
        ));
    }
    if line.len() > MAX_LINE_BYTES {
        return Err(PluginError::Execution(format!(
            "run request exceeds {MAX_LINE_BYTES} bytes"
        )));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|error| PluginError::Execution(format!("parse run request: {error}")))
}

/// 插件进程的主循环：读一行请求 → 跑 handler → 写结果事件。
///
/// handler 拿到请求和一个 [`PluginOutput`]（可中途 `log` / `progress`），
/// 返回正文即成功，返回 `Err` 即失败（会额外写一条 `error` 事件并以非零码退出）。
pub fn serve<F>(handler: F) -> ExitCode
where
    F: FnOnce(&RunRequest, &mut PluginOutput<std::io::Stdout>) -> Result<String, PluginError>,
{
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut out = PluginOutput::new(std::io::stdout());
    match read_request(&mut reader) {
        Ok(request) => match handler(&request, &mut out) {
            Ok(output) => {
                out.result(output);
                ExitCode::SUCCESS
            }
            Err(error) => {
                let message = error.to_string();
                let _ = writeln!(std::io::stderr(), "{message}");
                out.error(message);
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            let message = error.to_string();
            let _ = writeln!(std::io::stderr(), "{message}");
            out.error(message);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_read_from_one_line() {
        let mut input = std::io::Cursor::new(
            b"{\"type\":\"run\",\"tool\":\"duck.sql.ddl-generator\",\"input\":\"select 1\"}\n"
                .to_vec(),
        );
        let request = read_request(&mut input).expect("parse");
        assert_eq!(request.tool, "duck.sql.ddl-generator");
        assert_eq!(request.input, "select 1");
    }

    #[test]
    fn empty_stdin_is_an_error_not_a_panic() {
        let mut input = std::io::Cursor::new(Vec::new());
        let error = read_request(&mut input).unwrap_err();
        assert!(error.to_string().contains("no run request"), "{error}");
    }

    #[test]
    fn overlong_request_is_rejected() {
        let mut payload = b"{\"tool\":\"x\",\"input\":\"".to_vec();
        payload.extend(std::iter::repeat_n(b'a', MAX_LINE_BYTES));
        let mut input = std::io::Cursor::new(payload);
        let error = read_request(&mut input).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[test]
    fn output_writes_one_json_line_per_event() {
        let mut buffer = Vec::new();
        {
            let mut out = PluginOutput::new(&mut buffer);
            out.log("hello");
            out.progress(1, Some(2), Some("half"));
            out.result("done");
        }
        let text = String::from_utf8(buffer).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            Event::parse_line(lines[0]).expect("log"),
            Event::Log {
                line: "hello".into(),
                level: None,
            }
        );
        assert_eq!(
            Event::parse_line(lines[2]).expect("result"),
            Event::Result {
                output: "done".into()
            }
        );
    }
}
