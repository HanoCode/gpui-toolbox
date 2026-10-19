//! process 轨端到端：真起一个 `duck-merge-tool` 进程，走 stdin/stdout JSONL。
//!
//! 这一层补的是「单测测不到、链路最可能断」的一截：宿主用
//! [`duck_plugin_process::run`] 送请求、插件进程用 `serve` 收请求，两边对协议与
//! 分派表（`tool` → 哪个内核函数）的理解必须一致。
//!
//! 四个工具都需要真实的包目录才能跑出结果，这里只钉「进不去的那几条路」——
//! 它们恰好是最容易在分派层写错的：参数名拼错、目录不存在、扫描不到版本包。
//! 合并本身的正确性由 `duck-merge-core` 的复刻件单测（57 条）与参考仓的现场
//! 验证共同保证。

use std::path::{Path, PathBuf};

use duck_plugin_process::{CancelHandle, NullSink, RunRequest, run};

fn request(tool: &str, params: serde_json::Value) -> RunRequest {
    RunRequest {
        tool: tool.into(),
        input: String::new(),
        params,
        files: vec![],
        data: String::new(),
        lang: "zh-Hans".into(),
        kv: Default::default(),
        // CLI 直跑一次正常执行，不走页内扫描。
        action: None,
    }
}

/// 跑一次插件进程；`plugin_dir = "."` 让 locale 环境变量指向不存在的目录，
/// 取词自然回落到源串（本地化由 `plugin_locales.py` 与执行页覆盖）。
fn run_once(request: &RunRequest) -> Result<String, String> {
    let bin = env!("CARGO_BIN_EXE_duck-merge-tool");
    run(
        Path::new("."),
        Path::new(bin),
        request,
        &mut NullSink,
        &CancelHandle::new(),
    )
    .map(|outcome| outcome.output)
    .map_err(|error| error.to_string())
}

/// 一个只属于本测试的空目录（不同测试各用各的，避免并行互踩）。
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-merge-tool-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 分派表里没有的 id 必须**明确报错**，不能静默返回空结果。
#[test]
fn unknown_tool_is_rejected() {
    let error =
        run_once(&request("duck.merge.nope", serde_json::json!({}))).expect_err("未知工具应当失败");
    assert!(error.contains("unknown tool"), "{error}");
}

/// 源目录不存在：四个工具都该在动手前拦下（不写盘、不建半个目录）。
///
/// 报错文案分两种：`ifms6` / `non-see` 先跑扫描（「目录不存在」），`scc` / `see`
/// 走内核的入口校验（「源目录不存在: …」）。两种都算「进不去」，这里钉的是
/// **行为**：报错 + 结果目录一个都没被创建。
#[test]
fn missing_source_directory_is_reported_before_any_write() {
    let out = temp_dir("missing-src");
    let src = out.join("does-not-exist");
    for tool in [
        "duck.merge.ifms6-merge",
        "duck.merge.non-see-merge",
        "duck.merge.scc-merge",
        "duck.merge.see-merge",
    ] {
        let dst = out.join(format!("dst-{tool}"));
        let error = run_once(&request(
            tool,
            serde_json::json!({
                "src_dir": src.to_string_lossy(),
                "dst_dir": dst.to_string_lossy(),
                "customer_no": "C001",
            }),
        ))
        .expect_err("源目录不存在应当失败");
        assert!(error.contains("目录不存在"), "{tool}: {error}");
        assert!(!dst.exists(), "{tool}: 不该创建结果目录");
    }
}

/// 空源目录 + 版本清单留空 ⇒ 「扫描不到版本包」，而不是拿空列表硬跑。
#[test]
fn ifms6_reports_no_versions_when_the_directory_is_empty() {
    let src = temp_dir("ifms6-empty");
    let error = run_once(&request(
        "duck.merge.ifms6-merge",
        serde_json::json!({
            "src_dir": src.to_string_lossy(),
            "dst_dir": src.join("dst").to_string_lossy(),
        }),
    ))
    .expect_err("空目录应当失败");
    assert!(error.contains("没有找到版本包"), "{error}");
}

/// 非 SEE 合包与 IFMS6 合包共用同一份扫描：留空同样应当在扫描阶段就停下。
#[test]
fn non_see_reports_no_versions_when_the_directory_is_empty() {
    let src = temp_dir("non-see-empty");
    let error = run_once(&request(
        "duck.merge.non-see-merge",
        serde_json::json!({
            "src_dir": src.to_string_lossy(),
            "dst_dir": src.join("dst").to_string_lossy(),
        }),
    ))
    .expect_err("空目录应当失败");
    assert!(error.contains("没有找到版本包"), "{error}");
}

/// SEE 合包的入口检查：没有匹配命名规范的包就直接说清楚要什么名字。
#[test]
fn see_merge_names_the_expected_package_pattern() {
    let src = temp_dir("see-empty");
    let error = run_once(&request(
        "duck.merge.see-merge",
        serde_json::json!({
            "src_dir": src.to_string_lossy(),
            "dst_dir": src.join("dst").to_string_lossy(),
        }),
    ))
    .expect_err("空目录应当失败");
    assert!(error.contains("FS5.0-FINFSS-FSS-"), "{error}");
}
