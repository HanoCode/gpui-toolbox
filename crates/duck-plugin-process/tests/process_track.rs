//! `process` 轨的端到端守卫：用 `sh` 假插件覆盖协议的关键行为。
//!
//! fixture 是一个一个独立的小脚本（见 `tests/fixtures/`），每个钉一类行为：
//! 正常 result、progress 序列、取消中途、崩溃退出、超长行被拒、error 事件、
//! 无 result 退出、locale 环境变量透传。跑真进程——协议的成败只认真进程，
//! mock 掉 stdin/stdout 就什么都证明不了。
//!
//! 仅 Unix（fixture 依赖 `sh` 与 exec 权限位）；与插件轨的宿主平台一致。

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use duck_plugin::PluginError;
use duck_plugin_process::{CancelHandle, Event, NullSink, ProgressSink, RunRequest};

/// 收集事件的 sink（progress / log 断言用）。
#[derive(Default)]
struct CollectingSink {
    /// `(行, 级别)`：级别缺省为 `None`（插件没带 = info）。
    logs: Vec<(String, Option<String>)>,
    progress: Vec<(u64, Option<u64>, Option<String>)>,
}

impl ProgressSink for CollectingSink {
    fn on_log(&mut self, line: &str, level: Option<&str>) {
        self.logs.push((line.to_owned(), level.map(str::to_owned)));
    }
    fn on_progress(&mut self, current: u64, total: Option<u64>, message: Option<&str>) {
        self.progress
            .push((current, total, message.map(str::to_owned)));
    }
}

/// 把脚本正文写成可执行文件（源文件不带 exec 位，仓库快照不背权限）。
fn script(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("duck-plugin-process-tests");
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write fixture script");
    std::fs::set_permissions(&path, Permissions::from_mode(0o755)).expect("chmod fixture");
    path
}

fn request(tool: &str) -> RunRequest {
    RunRequest {
        tool: tool.to_owned(),
        input: String::new(),
        params: serde_json::json!({}),
        files: vec![],
        data: String::new(),
        lang: "zh-Hans".into(),
        kv: Default::default(),
        action: None,
    }
}

fn plugin_dir() -> &'static Path {
    Path::new("plugins/devkit.sql") // 只用于组装 locale 环境变量，无需真实存在
}

#[test]
fn normal_run_yields_the_result_event() {
    let bin = script(
        "normal.sh",
        r#"#!/bin/sh
printf '{"type":"result","output":"ok"}\n'
"#,
    );
    let outcome = duck_plugin_process::run(
        plugin_dir(),
        &bin,
        &request("t"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .expect("must run");
    assert_eq!(outcome.output, "ok");
}

#[test]
fn progress_and_log_events_reach_the_sink_in_order() {
    let bin = script(
        "progress.sh",
        r#"#!/bin/sh
printf '{"type":"log","line":"start"}\n'
printf '{"type":"progress","current":1,"total":2}\n'
printf '{"type":"progress","current":2,"total":2,"message":"done"}\n'
printf '{"type":"result","output":"finished"}\n'
"#,
    );
    let mut sink = CollectingSink::default();
    let outcome = duck_plugin_process::run(
        plugin_dir(),
        &bin,
        &request("t"),
        &mut sink,
        &CancelHandle::new(),
    )
    .expect("must run");
    assert_eq!(outcome.output, "finished");
    assert_eq!(sink.logs, [("start".into(), None)]);
    assert_eq!(
        sink.progress,
        [(1, Some(2), None), (2, Some(2), Some("done".into())),]
    );
}

#[test]
fn cancelled_run_is_reported_and_terminates_quickly() {
    let bin = script(
        "slow.sh",
        r#"#!/bin/sh
printf '{"type":"progress","current":0}\n'
sleep 30
printf '{"type":"result","output":"late"}\n'
"#,
    );
    let cancel = CancelHandle::new();
    let cancel_for_thread = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        cancel_for_thread.cancel();
    });
    let started = std::time::Instant::now();
    let error = duck_plugin_process::run(plugin_dir(), &bin, &request("t"), &mut NullSink, &cancel)
        .expect_err("cancelled run must fail");
    assert!(
        error.to_string().contains("cancelled by user"),
        "unexpected error: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancel must not wait out the plugin's sleep: {:?}",
        started.elapsed()
    );
}

#[test]
fn crash_exit_carries_the_stderr_tail() {
    let bin = script(
        "crash.sh",
        r#"#!/bin/sh
printf 'boom reason\n' >&2
printf 'second line\n' >&2
exit 3
"#,
    );
    let error = duck_plugin_process::run(
        plugin_dir(),
        &bin,
        &request("t"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .expect_err("crash must fail");
    let text = error.to_string();
    assert!(
        text.contains("exit code: 3") || text.contains("exited with"),
        "{text}"
    );
    assert!(text.contains("boom reason"), "stderr tail missing: {text}");
}

#[test]
fn error_event_surfaces_the_plugin_message() {
    let bin = script(
        "error-event.sh",
        r#"#!/bin/sh
printf '{"type":"error","message":"bad input"}\n'
"#,
    );
    let error = duck_plugin_process::run(
        plugin_dir(),
        &bin,
        &request("t"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .expect_err("error event must fail");
    assert!(error.to_string().contains("bad input"), "{error}");
}

#[test]
fn exit_without_result_is_an_error() {
    let bin = script(
        "no-result.sh",
        r#"#!/bin/sh
printf '{"type":"log","line":"hi"}\n'
"#,
    );
    let error = duck_plugin_process::run(
        plugin_dir(),
        &bin,
        &request("t"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .expect_err("exit without result must fail");
    assert!(
        error.to_string().contains("without a result event"),
        "{error}"
    );
}

#[test]
fn overlong_event_line_is_rejected() {
    // 1 MiB + 1 个 'a'：单行上限只认字节，超一个就拒。
    let bin = script(
        "overlong.sh",
        r#"#!/bin/sh
awk 'BEGIN{for(i=0;i<1048577;i++) printf "a"; printf "\n"}'
"#,
    );
    let error = duck_plugin_process::run(
        plugin_dir(),
        &bin,
        &request("t"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .expect_err("overlong line must be rejected");
    assert!(error.to_string().contains("byte cap"), "{error}");
}

#[test]
fn locale_env_reaches_the_plugin_process() {
    let bin = script(
        "locale.sh",
        r#"#!/bin/sh
printf '{"type":"result","output":"%s"}\n' "$DUCK_LOCALE"
"#,
    );
    let outcome = duck_plugin_process::run(
        plugin_dir(),
        &bin,
        &request("t"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .expect("must run");
    assert_eq!(outcome.output, "zh-Hans");
}

#[test]
fn events_parse_contract_still_holds() {
    // 协议层单测的看门狗：假插件全走 `Event::parse_line`，这里守住解析契约
    // 不被 fixture 悄悄绕过。
    assert_eq!(
        Event::parse_line(r#"{"type":"result","output":"x"}"#).expect("parse"),
        Event::Result { output: "x".into() }
    );
    let io_error = PluginError::Execution("probe".into());
    assert!(io_error.to_string().contains("probe"));
}

#[test]
fn body_chunks_are_joined_and_parsed_as_json() {
    // 分片故意切在 JSON 结构和多字节字符中间：宿主只做拼接，不认边界。
    let bin = script(
        "body.sh",
        r#"#!/bin/sh
printf '%s\n' '{"type":"body","chunk":"{\"files\":[{\"rows\":["}'
printf '%s\n' '{"type":"body","chunk":"{\"kind\":\"insert\",\"new_text\":\"差异\"}]}]}"}'
printf '%s\n' '{"type":"result","output":"+1 -0"}'
"#,
    );
    let outcome = duck_plugin_process::run(
        plugin_dir(),
        &bin,
        &request("t"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .expect("must run");
    // `output` 仍是纯文本镜像（复制用），`body` 是渲染用的结构。
    assert_eq!(outcome.output, "+1 -0");
    let body = outcome.body.expect("body must be present");
    assert_eq!(body["files"][0]["rows"][0]["new_text"], "差异");
}

#[test]
fn malformed_body_json_is_a_protocol_error() {
    let bin = script(
        "bad-body.sh",
        r#"#!/bin/sh
printf '{"type":"body","chunk":"{not json"}\n'
printf '{"type":"result","output":"done"}\n'
"#,
    );
    let error = duck_plugin_process::run(
        plugin_dir(),
        &bin,
        &request("t"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .expect_err("bad body must be rejected");
    assert!(error.to_string().contains("not valid JSON"), "{error}");
}
