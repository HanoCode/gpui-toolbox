//! process 轨端到端：真起一个 `duck-misc-tool` 进程，走 stdin/stdout JSONL。
//!
//! 补的是「单测测不到、链路最容易断」的那一截：宿主送请求、插件进程收请求，
//! 两边对**协议**与**分派表**（`tool` → 哪个内核函数、哪个参数名）的理解必须一致。
//! 参数名拼错、模式分派写反、报告字段算错，只有真跑一次才会现形。
//!
//! 算法本身的正确性由 `duck-misc-core` 的 67 条单测保证（含 Nginx 渲染快照与
//! Draft-07 各种关健字），这里只钉端到端的形状。
//!
//! `plugin_dir = "."` 让 locale 环境变量指向不存在的目录，取词回落到源串 —— 所以
//! 下面断言的都是中文源串（本地化由 `plugin_locales.py` 与执行页覆盖）。
//!
//! 三个工具都 `allow_files = "none"`，所以这里不写任何临时文件。速查手册多一个
//! 只读的导入目录（`allow_files = "read"`），它的用例自己建临时目录再删。

use std::path::Path;

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

fn run_once(request: &RunRequest) -> Result<String, String> {
    let bin = env!("CARGO_BIN_EXE_duck-misc-tool");
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

// ── JSON Schema ───────────────────────────────────────────────────────

#[test]
fn json_schema_passes_the_reference_sample() {
    let schema = r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#;
    let params = serde_json::json!({ "schema": schema, "instance": r#"{"name":"张三"}"# });
    let output = run_once(&request("duck.misc.json-schema", params)).expect("应当通过");
    assert!(output.contains("✓ 校验通过"), "{output}");
}

#[test]
fn json_schema_lists_every_error_with_a_pointer() {
    let schema = r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}},"additionalProperties":false}"#;
    let params = serde_json::json!({ "schema": schema, "instance": r#"{"name":1,"extra":true}"# });
    let output = run_once(&request("duck.misc.json-schema", params)).expect("校验能跑完");
    assert!(output.contains("2 个错误"), "{output}");
    assert!(output.contains("/name — must be string"), "{output}");
    assert!(
        output.contains("/ — must NOT have additional properties"),
        "{output}"
    );
}

#[test]
fn json_schema_separates_broken_input_from_failed_validation() {
    // Schema 本身不是 JSON：这是「没法校验」，不是「校验没过」。
    let params = serde_json::json!({ "schema": "{", "instance": "{}" });
    let error = run_once(&request("duck.misc.json-schema", params)).unwrap_err();
    assert!(error.contains("Schema JSON 格式错误"), "{error}");

    let params = serde_json::json!({ "schema": "{}", "instance": "{" });
    let error = run_once(&request("duck.misc.json-schema", params)).unwrap_err();
    assert!(error.contains("实例 JSON 格式错误"), "{error}");

    // 两个都空：先拦「没填」。
    let params = serde_json::json!({ "schema": "  ", "instance": "" });
    let error = run_once(&request("duck.misc.json-schema", params)).unwrap_err();
    assert!(error.contains("Schema 和 JSON 实例都要填"), "{error}");
}

// ── Git 文本生成 ──────────────────────────────────────────────────────

#[test]
fn git_commit_mode_builds_subject_and_file_list() {
    let diff = "diff --git a/src/parser.rs b/src/parser.rs
+++ b/src/parser.rs
@@ -1,1 +1,2 @@
+    let value = read().unwrap_or_default(); // fallback
";
    let params = serde_json::json!({ "mode": "commit", "type": "fix", "diff": diff });
    let output = run_once(&request("duck.misc.git-tools", params)).expect("应当生成");
    assert!(
        output.starts_with(
            "fix(parser.rs): let value = read().unwrap_or_default(); // fallback to empty"
        ) || output.starts_with("fix(parser.rs): let value = read().unwrap_or_default()"),
        "{output}"
    );
    assert!(output.contains("变更文件:"), "{output}");
    assert!(output.contains("- src/parser.rs"), "{output}");
}

#[test]
fn git_commit_mode_wants_a_diff() {
    let params = serde_json::json!({ "mode": "commit", "type": "feat", "diff": "" });
    let error = run_once(&request("duck.misc.git-tools", params)).unwrap_err();
    assert!(error.contains("先粘贴 git diff 的输出"), "{error}");
}

#[test]
fn git_hook_mode_returns_a_runnable_script() {
    let params = serde_json::json!({ "mode": "hook", "hook": "commit-msg", "lang": "bash" });
    let output = run_once(&request("duck.misc.git-tools", params)).expect("应当生成");
    assert!(output.starts_with("#!/bin/bash"), "{output}");
    assert!(output.contains("Invalid commit message format"), "{output}");
}

#[test]
fn git_gitignore_mode_splits_languages_and_keeps_sections() {
    let params = serde_json::json!({ "mode": "gitignore", "langs": "Rust; IDE" });
    let output = run_once(&request("duck.misc.git-tools", params)).expect("应当生成");
    assert!(output.contains("# Rust\ntarget/"), "{output}");
    assert!(output.contains("# IDE\n.idea/"), "{output}");

    // 留空 = 用源仓的默认勾选（Node + IDE），不是报错。
    let params = serde_json::json!({ "mode": "gitignore", "langs": "   " });
    let output = run_once(&request("duck.misc.git-tools", params)).expect("留空走默认");
    assert_eq!(output.matches("# ").count(), 2, "{output}");
    assert!(output.contains("# Node"), "{output}");
}

// ── Nginx 配置生成 ────────────────────────────────────────────────────

#[test]
fn nginx_preset_mode_renders_a_full_config() {
    let params = serde_json::json!({ "mode": "preset", "preset": "reverse-proxy" });
    let output = run_once(&request("duck.misc.nginx", params)).expect("应当生成");
    assert!(output.contains("worker_processes auto;"), "{output}");
    assert!(output.contains("upstream api_backend {"), "{output}");
    assert!(
        output.contains("try_files $uri $uri/ /index.html;"),
        "{output}"
    );
    // 三句注释来自译文表（这里是中文源串）。
    assert!(output.contains("# 客户端最大请求体大小"), "{output}");
    assert!(output.contains("# 错误页面"), "{output}");
}

#[test]
fn nginx_custom_mode_overrides_the_preset() {
    let params = serde_json::json!({
        "mode": "custom",
        "preset": "static-site",
        "config": r#"{"server":{"listen_port":"8180","server_name":"demo.local"}}"#
    });
    let output = run_once(&request("duck.misc.nginx", params)).expect("应当生成");
    assert!(output.contains("listen 8180;"), "{output}");
    assert!(output.contains("server_name demo.local;"), "{output}");
}

#[test]
fn nginx_custom_mode_rejects_mistyped_keys() {
    let params = serde_json::json!({
        "mode": "custom",
        "preset": "basic",
        "config": r#"{"server":{"listne_port":"8080"}}"#
    });
    let error = run_once(&request("duck.misc.nginx", params)).unwrap_err();
    assert!(error.contains("覆盖内容与配置结构对不上"), "{error}");
    assert!(error.contains("listne_port"), "{error}");
}

#[test]
fn nginx_validation_errors_are_translated_sentences() {
    let params = serde_json::json!({
        "mode": "custom",
        "preset": "basic",
        "config": r#"{"locations":[{"path":"/api/","location_type":"proxy","target":"backend"}]}"#
    });
    let error = run_once(&request("duck.misc.nginx", params)).unwrap_err();
    assert!(error.contains("配置校验没通过"), "{error}");
    assert!(
        error.contains("/api/ 的代理目标要以 http:// 或 https:// 开头"),
        "{error}"
    );
}

#[test]
fn nginx_custom_mode_wants_a_json_patch() {
    let params = serde_json::json!({ "mode": "custom", "preset": "basic", "config": "" });
    let error = run_once(&request("duck.misc.nginx", params)).unwrap_err();
    assert!(error.contains("自定义模式要在输入框写一段 JSON"), "{error}");
}

#[test]
fn unknown_tool_is_reported_instead_of_silently_passing() {
    let error = run_once(&request("duck.misc.nope", serde_json::json!({}))).unwrap_err();
    assert!(error.contains("unknown tool"), "{error}");
}

// ── 速查手册 ─────────────────────────────────────────────────────────

#[test]
fn cheat_sheets_lists_every_builtin_sheet_by_category() {
    let output =
        run_once(&request("duck.text.cheat-sheets", serde_json::json!({}))).expect("列表应当成功");
    assert!(output.starts_with("20 张速查表"), "{output}");
    // 分类标题来自 `.cheat` 数据；英文界面下才走译文。
    assert!(output.contains("版本控制"), "{output}");
    assert!(output.contains("Git  (git)  · 4 条命令"), "{output}");
    // 纯正文表标注「纯速查」，不是空一行。
    assert!(output.contains("Python  (python)  · 纯速查"), "{output}");
}

#[test]
fn cheat_sheets_search_accepts_initialisms() {
    // 参考页那把打分器：`dcom` 该命中 Docker Compose。
    let output = run_once(&request(
        "duck.text.cheat-sheets",
        serde_json::json!({ "keyword": "dcom" }),
    ))
    .expect("搜索应当成功");
    assert!(output.contains("匹配「dcom」的 1 张速查表"), "{output}");
    assert!(
        output.contains("Docker Compose  (docker-compose)"),
        "{output}"
    );
}

#[test]
fn cheat_sheets_render_fills_declared_variables() {
    let output = run_once(&request(
        "duck.text.cheat-sheets",
        serde_json::json!({
            "sheet": "git",
            "view": "commands",
            "vars": "branch=feature/login message=修复 登录页",
        }),
    ))
    .expect("打开 git 应当成功");

    // 值里有空格的 `message` 没被空格切碎。
    assert!(output.contains("git checkout -b feature/login"), "{output}");
    assert!(
        output.contains(r#"git commit -m "修复 登录页""#),
        "{output}"
    );
    // 没给的变量留着占位符并点名，别让人直接粘进终端。
    assert!(output.contains("⚠️ 还差 <file> 没填"), "{output}");
    // 静态候选与「候选由命令产出」两种变量都要标出来。
    assert!(
        output.contains("（候选：修复bug / 新增功能 / 重构代码）"),
        "{output}"
    );
    assert!(
        output.contains("（候选由这条命令产出：git ls-files）"),
        "{output}"
    );
    // 只命令模式不该带正文。
    assert!(!output.contains("## 速查内容"), "{output}");
}

#[test]
fn cheat_sheets_imports_a_folder_of_navi_files() {
    let root = std::env::temp_dir().join(format!("duck-cheat-{}", std::process::id()));
    let nested = root.join("sub");
    std::fs::create_dir_all(&nested).expect("建临时目录");
    // 纯 navi（导入的主力形态）：没有 `%%` 行，整份都是命令区。
    std::fs::write(
        nested.join("k8s.cheat"),
        "% 运维\n# 看 Pod 日志\nkubectl logs -f <pod>\n$ pod: kubectl get pods -o name\n",
    )
    .expect("写 k8s.cheat");
    std::fs::write(root.join("not-a-sheet.txt"), "ignored").expect("写干扰文件");

    let output = run_once(&request(
        "duck.text.cheat-sheets",
        serde_json::json!({ "dir": root.to_string_lossy() }),
    ))
    .expect("导入应当成功");
    assert!(output.starts_with("21 张速查表"), "{output}");
    assert!(output.contains("已扫描导入目录 1 张"), "{output}");
    // 名称取文件名（去扩展名），slug 加 `import-` 前缀。
    assert!(output.contains("k8s  (import-k8s)"), "{output}");
    assert!(output.contains("运维"), "分类来自 % 标签行：{output}");

    let error = run_once(&request(
        "duck.text.cheat-sheets",
        serde_json::json!({ "dir": root.join("missing").to_string_lossy() }),
    ))
    .expect_err("不存在的目录应当报错");
    assert!(error.contains("路径不是有效目录"), "{error}");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cheat_sheets_unknown_slug_names_itself() {
    let error = run_once(&request(
        "duck.text.cheat-sheets",
        serde_json::json!({ "sheet": "nope" }),
    ))
    .expect_err("不存在的表应当报错");
    assert!(error.contains("找不到这张速查表：nope"), "{error}");
}
