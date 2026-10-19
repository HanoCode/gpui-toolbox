//! stdio JSONL 协议（宿主 ⇆ 插件进程的唯一通道）。
//!
//! 一行请求进、多行事件出，全部 UTF-8 JSON、一行一个值：
//!
//! ```text
//! →  {"type":"run","tool":"dev.duck.sql.script-check","input":"…","params":{…},
//!     "files":["/abs/path"],"lang":"zh-Hans","kv":{…}}
//! ←  {"type":"log","line":"scanning …"}
//! ←  {"type":"progress","current":3,"total":10,"message":"scanning"}
//! ←  {"type":"body","chunk":"{\"files\":["}          # 结构化正文，可多行
//! ←  {"type":"result","output":"…"}
//! ←  {"type":"error","message":"…"}          # 也可以只失败退出（非零退出码）
//! ```
//!
//! `body` 是给富结果（`diff` / `table` 这类要画成表格或双栏的东西）用的：
//! 分片拼起来是一段 JSON，语义由插件的 `[io.output] kind` 决定。没有它也能跑——
//! `output` 永远是一份纯文本镜像（复制按钮复制的是它）。
//!
//! 硬限制（见 `ifm-plugin-port-plan.md` W3）：单行 ≤ [`MAX_LINE_BYTES`]、
//! 一次运行的**事件总量** ≤ [`MAX_TOTAL_BYTES`]、结构化正文 ≤ [`MAX_BODY_BYTES`]、
//! [`IDLE_WARN`] 内无事件先告警（不杀进程——慢任务合法，失控由用户取消）。
//!
//! 取消：宿主往 stdin 补一行 `{"type":"cancel"}` 让插件体面收尾，2 秒不退
//! 先 `SIGTERM`、再 2 秒 `SIGKILL`（见 [`runner::CANCEL_GRACE`]）。

use std::collections::BTreeMap;
use std::time::Duration;

use duck_plugin::PluginError;
use serde::{Deserialize, Serialize};

/// 单行（请求或事件）的字节上限：1 MiB。
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// 一次运行全部事件行累计的字节上限：8 MiB。
pub const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
/// 结构化正文（`body` 分片拼起来）的上限：32 MiB。
///
/// 比 [`MAX_TOTAL_BYTES`] 大是有意的：差异/表格结果是**按行**长的，几千行就上
/// 几兆，卡在 8 MiB 会让「比两个大版本包」这类正常任务在九成处失败。行数再多的
/// 任务本就该由插件自己截断（并在 `output` 里说明截了多少）。
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// 无事件的告警阈值：30 秒（只告警，不杀）。
pub const IDLE_WARN: Duration = Duration::from_secs(30);

/// 一次独立进程运行请求（宿主 → 插件，stdin 单行 JSON）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    /// 被调用的工具 id（与 JS 轨的 `handle(toolId, input)` 同一口径）。
    pub tool: String,
    /// 主输入文本（执行页输入区的值）。
    #[serde(default)]
    pub input: String,
    /// 参数区（声明式 `[io]` 收集的键值，JSON 对象）。
    #[serde(default)]
    pub params: serde_json::Value,
    /// 本次运行授权的用户文件**绝对路径**（ADR-3：per-run，不落盘不记忆）。
    #[serde(default)]
    pub files: Vec<String>,
    /// 当前语言 tag（[`duck_plugin::ENV_LOCALE`] 同值，环境变量里也带一份）。
    #[serde(default)]
    pub lang: String,
    /// 插件 KV 快照（ADR-4：宿主读好传入，写回走回执，进程不直连存储）。
    #[serde(default)]
    pub kv: BTreeMap<String, String>,
    /// 该插件**私有数据目录**的绝对路径（`<app_data>/plugin-data/<plugin_id>`）。
    ///
    /// 给「有状态的工具」用：接口库、版本快照、自定义模板这类要跨运行留住的东西。
    /// 与 `files` 是两回事——`files` 是用户这次授权的路径，这个是插件自己的家，
    /// 目录由宿主按插件 id 生成，插件改不了位置。
    ///
    /// 空串 = 没有（老宿主 / 宿主没建目录）。插件当它不存在处理，不要自己猜路径。
    #[serde(default)]
    pub data: String,
    /// 本次调用的动作（缺省 = 正常执行一次工具）。
    ///
    /// 目前只定义 `"scan"`：页内扫描（`[io] scan` 声明触发）。宿主以同样的
    /// 请求再拉一次插件二进制，插件只做扫描、以 `{"versions": […]}` 形状的
    /// `body` 返回结果，不发 `progress` / `log`。老插件不认识这个字段也没关系
    /// ——`serde` 缺省丢弃未知字段，但老插件会把它当一次正常运行跑完，
    /// 因此宿主只对**声明了 `scan` 的插件**发这个动作。
    #[serde(default)]
    pub action: Option<String>,
}

impl RunRequest {
    /// 序列化成一行（保证不含换行——`serde_json` 会转义字符串里的 `\n`，
    /// 这里再防御一次结构层面，防止未来有人把请求拆成多行）。
    ///
    /// # Errors
    /// 序列化失败（几乎不可能：字段全是可序列化类型）。
    pub fn to_line(&self) -> Result<String, PluginError> {
        let line = serde_json::to_string(self)
            .map_err(|error| PluginError::Execution(format!("serialize run request: {error}")))?;
        debug_assert!(!line.contains('\n'), "request must be a single line");
        Ok(line)
    }
}

/// 插件进程产出的一行事件（stdout，宿主逐行解析）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// 过程日志（转发给宿主日志与执行页的 log 渲染器）。
    ///
    /// `level` 是插件自报的级别（`info` / `warn` / `error` / `success`）；
    /// 缺省按 `info` 处理。宿主用它给日志行着色，不做别的语义。
    Log {
        line: String,
        #[serde(default)]
        level: Option<String>,
    },
    /// 进度。`total` 缺省 = 不定总量（转圈而非百分比）。
    Progress {
        current: u64,
        #[serde(default)]
        total: Option<u64>,
        #[serde(default)]
        message: Option<String>,
    },
    /// 结构化正文的一个分片。
    ///
    /// 多行按发送顺序**原样拼接**成一段 JSON（宿主不解析分片边界，只拼），
    /// 必须在 `result` 之前发完；`result` 之后到达的分片会被丢弃。
    /// 单行仍受 [`MAX_LINE_BYTES`] 约束，大结果请自己切块
    /// （建议 ≤ 256 KiB 一片，给 JSON 转义留足余量）。
    Body {
        /// JSON 文本的一段（不要自己加换行）。
        chunk: String,
    },
    /// 工具产出（正文）。
    Result {
        #[serde(default)]
        output: String,
    },
    /// 插件自报失败（附带人类可读原因；非零退出码同样按失败计）。
    Error { message: String },
}

impl Event {
    /// 解析一行事件。
    ///
    /// # Errors
    /// 非 JSON、`type` 不认识、字段形状不符都算协议违约，直接按执行错误处理
    /// （宽进严出：坏行不静默丢弃，否则「插件写错了键名」会表现成「结果缺一块」）。
    pub fn parse_line(line: &str) -> Result<Self, PluginError> {
        serde_json::from_str(line)
            .map_err(|error| PluginError::Execution(format!("bad event line: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RunRequest {
        RunRequest {
            tool: "dev.duck.sql.script-check".into(),
            input: "select 1;\n".into(),
            params: serde_json::json!({"dialect": "mysql"}),
            files: vec!["/tmp/a.sql".into()],
            lang: "zh-Hans".into(),
            kv: BTreeMap::from([("last_dir".into(), "/tmp".into())]),
            data: "/tmp/plugin-data/dev.duck.demo".into(),
            action: None,
        }
    }

    #[test]
    fn request_roundtrips_as_a_single_line() {
        let line = request().to_line().expect("serialize");
        assert!(!line.contains('\n'));
        let parsed: RunRequest = serde_json::from_str(&line).expect("parse back");
        assert_eq!(parsed.tool, "dev.duck.sql.script-check");
        assert_eq!(parsed.params["dialect"], "mysql");
        assert_eq!(parsed.files, ["/tmp/a.sql"]);
        assert_eq!(parsed.kv["last_dir"], "/tmp");
        assert_eq!(parsed.data, "/tmp/plugin-data/dev.duck.demo");
        assert_eq!(parsed.action, None);
    }

    /// `action` / `data` 都是后加的字段：不带它们的一行请求（老宿主写法）照样能解析。
    ///
    /// `data` 缺省成空串而不是 `Option`：插件侧只需判一次 `is_empty()`，
    /// 不必区分「老宿主没给」与「给了空」——两种情况下它都拿不到目录。
    #[test]
    fn request_without_action_still_parses() {
        let parsed: RunRequest = serde_json::from_str(
            r#"{"tool":"dev.duck.sql.script-check","input":"","params":{},"files":[],"lang":"zh-Hans","kv":{}}"#,
        )
        .expect("parse back");
        assert_eq!(parsed.action, None);
        assert!(parsed.data.is_empty());

        let scanned: RunRequest = serde_json::from_str(
            r#"{"tool":"duck.merge.pkg-merge","action":"scan","input":"","params":{},"files":[],"lang":"zh-Hans","kv":{}}"#,
        )
        .expect("parse back");
        assert_eq!(scanned.action.as_deref(), Some("scan"));
    }

    #[test]
    fn events_roundtrip_with_snake_case_tags() {
        for (line, expected) in [
            (
                r#"{"type":"log","line":"hi"}"#,
                Event::Log {
                    line: "hi".into(),
                    level: None,
                },
            ),
            (
                r#"{"type":"log","line":"boom","level":"error"}"#,
                Event::Log {
                    line: "boom".into(),
                    level: Some("error".into()),
                },
            ),
            (
                r#"{"type":"progress","current":3}"#,
                Event::Progress {
                    current: 3,
                    total: None,
                    message: None,
                },
            ),
            (
                r#"{"type":"progress","current":3,"total":10,"message":"…"}"#,
                Event::Progress {
                    current: 3,
                    total: Some(10),
                    message: Some("…".into()),
                },
            ),
            (
                r#"{"type":"result","output":"out\nline"}"#,
                Event::Result {
                    output: "out\nline".into(),
                },
            ),
            (
                r#"{"type":"error","message":"boom"}"#,
                Event::Error {
                    message: "boom".into(),
                },
            ),
        ] {
            assert_eq!(Event::parse_line(line).expect("parse"), expected);
        }
    }

    #[test]
    fn unknown_event_type_is_an_error_not_a_silent_drop() {
        let error = Event::parse_line(r#"{"type":"toast","line":"hi"}"#).unwrap_err();
        assert!(error.to_string().contains("bad event line"), "{error}");
        assert!(Event::parse_line("not json").is_err());
    }

    #[test]
    fn result_output_may_span_lines_via_json_escapes() {
        let event = Event::Result {
            output: "a\nb".into(),
        };
        let line = serde_json::to_string(&event).expect("serialize");
        assert!(!line.contains('\n'), "newline must be escaped: {line}");
    }
}
