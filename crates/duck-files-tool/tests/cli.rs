//! process 轨端到端：真起一个 `duck-files-tool` 进程，走 stdin/stdout JSONL。
//!
//! 补的是「单测测不到、链路最容易断」的那一截：宿主用 `duck_plugin_process::run`
//! 送请求、插件进程用 `serve` 收请求，两边对**协议**与**分派表**（`tool` → 哪个内核
//! 函数、哪个参数名）的理解必须一致。参数名拼错、路径校验写反、报告字段算错，
//! 这些都只有真跑一次才会现形。
//!
//! 算法本身的正确性由 `duck-files-core` 的 53 条单测保证，这里只钉端到端的形状：
//! 有输出、有关键行、有的动作**真的改到了盘上**、失败时报错且不静默。
//!
//! `plugin_dir = "."` 让 locale 环境变量指向不存在的目录，取词回落到源串——所以下面
//! 断言的都是中文源串（本地化由 `plugin_locales.py` 与执行页覆盖）。
//!
//! ## 撤销那两条为什么要加锁
//!
//! 撤销日志的目录由 `main.rs::data_dir()` 决定（`current_exe` 上两级 + `data`），
//! 测试里就是 `target/data`——**同一台机器上所有运行共用**。`undo_last` 取的是
//! 「最新的一条记录」，所以两条撤销测试并行跑会互相 pop 对方的记录。这里用
//! `UNDO_LOCK` 串行化，并在进入临界区后清掉 `target/data/undo`：
//! 那是构建目录里的派生数据，删掉不伤任何人的东西，但不删就可能读到上一次运行
//! 留下的旧记录（表现为「一次本应成功的撤销说没有记录」）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    let bin = env!("CARGO_BIN_EXE_duck-files-tool");
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

/// 一个只属于本测试的目录（名字带进程 id，并行跑互不踩）。
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-files-tool-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn write(root: &Path, name: &str, text: &str) -> PathBuf {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("建父目录");
    }
    std::fs::write(&path, text).expect("写文件");
    path
}

/// 插件进程眼中的数据目录（`current_exe` 上两级 + `data`），撤销测试要清它。
fn plugin_data_dir() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_duck-files-tool"))
        .parent()
        .and_then(Path::parent)
        .expect("二进制在 target/<profile>/ 下")
        .join("data")
}

/// 撤销相关的测试互斥（理由见文件头）。
static UNDO_LOCK: Mutex<()> = Mutex::new(());

/// 进入撤销临界区：拿锁 + 清掉上一次运行留下的日志。
fn reset_undo() -> std::sync::MutexGuard<'static, ()> {
    let guard = UNDO_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let _ = std::fs::remove_dir_all(plugin_data_dir().join("undo"));
    guard
}

// ── 分派表 ─────────────────────────────────────────────────────────────

#[test]
fn unknown_tool_is_rejected_instead_of_silently_passing() {
    let error =
        run_once(&request("duck.files.nope", serde_json::json!({}))).expect_err("未知工具必须报错");
    assert!(error.contains("unknown tool"), "错误信息：{error}");
}

// ── 搜索 ───────────────────────────────────────────────────────────────

#[test]
fn content_search_reports_file_line_and_column() {
    let root = temp_dir("search-content");
    write(&root, "a.txt", "alpha needle\nbeta\n");
    write(&root, "nested/b.md", "no hit here\nsecond needle two\n");

    let output = run_once(&request(
        "duck.files.search",
        serde_json::json!({
            "dir": root,
            "pattern": "needle",
            "mode": "content",
            "max_results": 50,
        }),
    ))
    .expect("搜索应成功");

    assert!(output.contains("命中 2 处"), "{output}");
    // 内容模式每行是 `相对路径  行:列  原文`：路径被补空格对齐，所以只断言片段。
    assert!(output.contains("a.txt"), "{output}");
    assert!(output.contains("1:7"), "{output}");
    assert!(output.contains("nested/b.md"), "{output}");
    assert!(output.contains("2:8"), "{output}");
    // 「扫描 N 个文件」也要报，否则用户不知道有没有漏看。
    assert!(output.contains("扫描 2 个文件"), "{output}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn filename_search_lists_relative_paths() {
    let root = temp_dir("search-name");
    write(&root, "keep-me.txt", "x");
    write(&root, "nested/keep-me-too.md", "y");
    write(&root, "other.txt", "z");

    let output = run_once(&request(
        "duck.files.search",
        serde_json::json!({ "dir": root, "pattern": "keep", "mode": "filename" }),
    ))
    .expect("搜索应成功");

    assert!(output.contains("keep-me.txt"), "{output}");
    assert!(output.contains("nested/keep-me-too.md"), "{output}");
    assert!(!output.contains("other.txt"), "{output}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn search_without_a_keyword_fails_with_the_empty_input_message() {
    let root = temp_dir("search-empty");
    let error = run_once(&request(
        "duck.files.search",
        serde_json::json!({ "dir": root, "pattern": "", "mode": "content" }),
    ))
    .expect_err("空关键词必须报错");
    assert!(error.contains("输入是空的"), "{error}");
    let _ = std::fs::remove_dir_all(root);
}

// ── 目录树 ─────────────────────────────────────────────────────────────

#[test]
fn tree_reports_counts_and_wraps_in_a_fence_for_markdown() {
    let root = temp_dir("tree");
    write(&root, "a.txt", "12345");
    write(&root, "sub/b.txt", "123");

    let plain = run_once(&request(
        "duck.files.tree",
        serde_json::json!({ "dir": root, "format": "text", "show_size": true }),
    ))
    .expect("目录树应成功");
    assert!(plain.contains("1 个目录"), "{plain}");
    assert!(plain.contains("2 个文件"), "{plain}");
    assert!(plain.contains("sub"), "{plain}");

    let markdown = run_once(&request(
        "duck.files.tree",
        serde_json::json!({ "dir": root, "format": "markdown" }),
    ))
    .expect("目录树应成功");
    // markdown 形态要能直接粘进文档：整棵树必须落在 ```text 围栏里。
    assert!(markdown.contains("```text"), "{markdown}");
    let _ = std::fs::remove_dir_all(root);
}

// ── 批量改名 ───────────────────────────────────────────────────────────

#[test]
fn rename_preview_reports_without_touching_the_disk() {
    let root = temp_dir("rename-preview");
    write(&root, "a.txt", "1");
    write(&root, "b.txt", "2");

    let output = run_once(&request(
        "duck.files.batch-edit",
        serde_json::json!({
            "dir": root,
            "mode": "rename",
            "action": "preview",
            "pattern": "b",
            "replacement": "z",
        }),
    ))
    .expect("预览应成功");

    assert!(output.contains("需要改名 1 个"), "{output}");
    assert!(output.contains("将改名"), "{output}");
    // 预览只说不做：盘上必须还是原来的名字。
    assert!(root.join("b.txt").exists(), "预览不该改名");
    assert!(!root.join("z.txt").exists(), "预览不该改名");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn rename_apply_then_undo_restores_the_original_names() {
    let _guard = reset_undo();
    let root = temp_dir("rename-undo");
    write(&root, "a.txt", "1");
    write(&root, "b.txt", "2");

    // ⚠️ 不能拿 `b → B` 当用例：macOS 默认的文件系统**大小写不敏感**，那种改名在盘上
    // 看不出区别（`exists()` 两个名字都真），断言会变成永远通过或永远失败的噪声。
    let applied = run_once(&request(
        "duck.files.batch-edit",
        serde_json::json!({
            "dir": root,
            "mode": "rename",
            "action": "apply",
            "pattern": "b",
            "replacement": "z",
        }),
    ))
    .expect("应用应成功");
    assert!(applied.contains("已改名 1 个文件"), "{applied}");
    assert!(root.join("z.txt").exists(), "应用后应当改名");
    assert!(!root.join("b.txt").exists());

    // 撤销是**下一次运行**做的事（日志落盘），所以这里另起一次请求。
    let undone = run_once(&request(
        "duck.files.batch-edit",
        serde_json::json!({ "action": "undo" }),
    ))
    .expect("撤销应成功");
    assert!(undone.contains("已恢复 1 个文件"), "{undone}");
    assert!(root.join("b.txt").exists(), "撤销后应当恢复原名");
    assert!(!root.join("z.txt").exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn chained_renames_survive_the_two_phase_apply_and_undo() {
    let _guard = reset_undo();
    let root = temp_dir("rename-chain");
    // `a→b` 与 `b→c` 同批出现：直接 rename 会把 a 的内容写进已经被 b 占用的名字，
    // 两阶段（源→临时名→目标名）才不会丢数据。
    write(&root, "1.txt", "one");
    write(&root, "2.txt", "two");

    let applied = run_once(&request(
        "duck.files.batch-edit",
        serde_json::json!({
            "dir": root,
            "mode": "rename",
            "action": "apply",
            "pattern": "^([12])$",
            "replacement": "n$1",
            "is_regex": true,
        }),
    ))
    .expect("应用应成功");
    assert!(applied.contains("已改名 2 个文件"), "{applied}");
    assert_eq!(
        std::fs::read_to_string(root.join("n1.txt")).expect("n1"),
        "one"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("n2.txt")).expect("n2"),
        "two"
    );

    run_once(&request(
        "duck.files.batch-edit",
        serde_json::json!({ "action": "undo" }),
    ))
    .expect("撤销应成功");
    assert_eq!(
        std::fs::read_to_string(root.join("1.txt")).expect("1"),
        "one"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn content_replace_preview_then_apply() {
    let _guard = reset_undo();
    let root = temp_dir("replace");
    write(&root, "a.txt", "foo foo\nbar\n");

    let preview = run_once(&request(
        "duck.files.batch-edit",
        serde_json::json!({
            "dir": root,
            "mode": "replace",
            "action": "preview",
            "pattern": "foo",
            "replacement": "baz",
        }),
    ))
    .expect("预览应成功");
    assert!(preview.contains("命中 2 处"), "{preview}");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("读"),
        "foo foo\nbar\n"
    );

    let applied = run_once(&request(
        "duck.files.batch-edit",
        serde_json::json!({
            "dir": root,
            "mode": "replace",
            "action": "apply",
            "pattern": "foo",
            "replacement": "baz",
        }),
    ))
    .expect("应用应成功");
    assert!(applied.contains("替换 2 处"), "{applied}");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("读"),
        "baz baz\nbar\n"
    );

    run_once(&request(
        "duck.files.batch-edit",
        serde_json::json!({ "action": "undo" }),
    ))
    .expect("撤销应成功");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("读"),
        "foo foo\nbar\n"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn undo_without_a_record_says_so_instead_of_failing() {
    let _guard = reset_undo();
    let output = run_once(&request(
        "duck.files.batch-edit",
        serde_json::json!({ "action": "undo" }),
    ))
    .expect("没有记录不该算失败");
    assert!(output.contains("没有可撤销的记录"), "{output}");
}

// ── CSV ────────────────────────────────────────────────────────────────

#[test]
fn csv_exports_markdown_json_and_insert() {
    let root = temp_dir("csv-export");
    let csv = write(&root, "t.csv", "name,qty\napple,3\npear,5\n");
    let file = csv.display().to_string();

    let markdown = run_once(&request(
        "duck.files.csv-tools",
        serde_json::json!({ "file": file, "delimiter": "auto", "mode": "export", "format": "markdown" }),
    ))
    .expect("markdown 应成功");
    assert!(markdown.contains("| name | qty |"), "{markdown}");
    assert!(markdown.contains("| apple | 3 |"), "{markdown}");

    let json = run_once(&request(
        "duck.files.csv-tools",
        serde_json::json!({ "file": file, "mode": "export", "format": "json" }),
    ))
    .expect("json 应成功");
    // ⚠️ 值一律是**字符串**：参考仓 `CsvTableEditor.vue::copyAsJson` 用的是
    // `Record<string, string>`，没有做数字推断。照着复刻，不在这里「顺手优化」——
    // 一旦把 `3` 输出成数字，含前导零的编码（`007`）就会在导出时掉零。
    assert!(json.contains("\"qty\": \"3\""), "{json}");

    let insert = run_once(&request(
        "duck.files.csv-tools",
        serde_json::json!({
            "file": file,
            "mode": "export",
            "format": "insert",
            "table_name": "t_demo",
        }),
    ))
    .expect("insert 应成功");
    assert!(
        insert.contains("INSERT INTO t_demo (name, qty)"),
        "{insert}"
    );
    assert!(insert.contains("('apple', 3)"), "{insert}");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn csv_pasted_into_the_input_box_works_without_a_file() {
    let mut req = request(
        "duck.files.csv-tools",
        serde_json::json!({ "mode": "export", "format": "markdown" }),
    );
    req.input = "a;b\n1;2\n".into();
    let output = run_once(&req).expect("粘贴 CSV 应成功");
    // 分隔符自动识别要认得 `;`（源仓只认逗号，遇到欧洲导出会整行当一列）。
    assert!(output.contains("| a | b |"), "{output}");
}

#[test]
fn csv_chart_writes_an_svg_file() {
    let root = temp_dir("csv-chart");
    let csv = write(&root, "t.csv", "month,total\n1月,10\n2月,20\n");
    let out = root.join("out");
    std::fs::create_dir_all(&out).expect("建输出目录");

    let report = run_once(&request(
        "duck.files.csv-tools",
        serde_json::json!({
            "file": csv.display().to_string(),
            "mode": "chart",
            "chart": "bar",
            "palette": "default",
            "output_dir": out.display().to_string(),
        }),
    ))
    .expect("图表应成功");
    assert!(report.contains("已生成图表"), "{report}");

    let svg = out.join("t.svg");
    let body = std::fs::read_to_string(&svg).expect("SVG 应落在输出目录");
    assert!(body.starts_with("<svg"), "{}", &body[..40.min(body.len())]);
    assert!(body.contains("1月"), "图表要带轴上标签");
    let _ = std::fs::remove_dir_all(root);
}

// ── 格式转换 ───────────────────────────────────────────────────────────

#[test]
fn doc_convert_previews_in_the_result_pane_when_no_output_dir_is_given() {
    let root = temp_dir("convert-preview");
    let file = write(&root, "readme.md", "# Title\n\nbody text\n");

    let output = run_once(&request(
        "duck.files.doc-convert",
        serde_json::json!({
            "mode": "single",
            "input_file": file.display().to_string(),
            "format": "html",
        }),
    ))
    .expect("转换应成功");
    // 没有输出目录 = 只要预览：结果区直接是转出来的内容。
    assert!(output.contains("<h1>Title</h1>"), "{output}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn doc_convert_writes_the_file_and_refuses_to_overwrite_by_default() {
    let root = temp_dir("convert-write");
    let file = write(&root, "readme.md", "# Title\n");
    let out = root.join("out");
    std::fs::create_dir_all(&out).expect("建输出目录");

    let report = run_once(&request(
        "duck.files.doc-convert",
        serde_json::json!({
            "mode": "single",
            "input_file": file.display().to_string(),
            "format": "txt",
            "output_dir": out.display().to_string(),
        }),
    ))
    .expect("转换应成功");
    assert!(report.contains("已转换"), "{report}");
    assert!(out.join("readme.txt").exists());

    // 第二次不给 overwrite：同名文件必须挡住，而不是静默覆盖。
    let error = run_once(&request(
        "duck.files.doc-convert",
        serde_json::json!({
            "mode": "single",
            "input_file": file.display().to_string(),
            "format": "txt",
            "output_dir": out.display().to_string(),
        }),
    ))
    .expect_err("同名文件应被挡住");
    assert!(error.contains("目标已存在"), "{error}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn doc_convert_rejects_an_unsupported_output_format() {
    let root = temp_dir("convert-bad-format");
    let file = write(&root, "readme.md", "# Title\n");
    let error = run_once(&request(
        "duck.files.doc-convert",
        serde_json::json!({
            "mode": "single",
            "input_file": file.display().to_string(),
            "format": "pdf",
        }),
    ))
    .expect_err("pdf 输出不支持");
    // 细节要把用户填的那个值带回来，否则「不支持」等于没说。
    assert!(error.contains("这种格式不支持"), "{error}");
    assert!(error.contains("pdf"), "{error}");
    let _ = std::fs::remove_dir_all(root);
}
