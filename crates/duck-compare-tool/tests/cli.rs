//! process 轨端到端：真起一个 `duck-compare-tool` 进程，走 stdin/stdout JSONL。
//!
//! 补的是「单测测不到、链路最容易断」的那一截：宿主用 `duck_plugin_process::run`
//! 送请求、插件进程用 `serve` 收请求，两边对**协议**与**分派表**（`tool` → 哪个内核
//! 函数、哪个参数名）的理解必须一致。参数名拼错、目录校验写反、body 与 output 送错
//! 通道，这些都只有真跑一次才会现形。
//!
//! 比对算法本身的正确性由 `duck-compare-core` 的复刻件单测保证（22 条），这里只钉
//! 端到端的形状：有输出、有关键行、失败时报错且不静默。
//!
//! `plugin_dir = "."` 让 locale 环境变量指向不存在的目录，取词回落到源串——所以下面
//! 断言的都是中文源串（本地化由 `plugin_locales.py` 与执行页覆盖）。

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
        // 比对族没有页内扫描，正常执行即可。
        action: None,
    }
}

fn run_once(request: &RunRequest) -> Result<String, String> {
    let bin = env!("CARGO_BIN_EXE_duck-compare-tool");
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
fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-compare-tool-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 一个能过 `parse_class_file` 的最小 `.class`（版本号塞在常量池的 Utf8 里）。
fn fake_class(version: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&52u16.to_be_bytes());
    out.extend_from_slice(&2u16.to_be_bytes());
    out.push(1);
    out.extend_from_slice(&(version.len() as u16).to_be_bytes());
    out.extend_from_slice(version.as_bytes());
    out.extend_from_slice(&0x0021u16.to_be_bytes());
    for _ in 0..6 {
        out.extend_from_slice(&0u16.to_be_bytes());
    }
    out
}

fn write_class(root: &Path, service: &str, class: &str, version: &str) {
    let mut path = root.join(service).join("WEB-INF/classes");
    for part in class.split('/') {
        path.push(part);
    }
    path.set_extension("class");
    std::fs::create_dir_all(path.parent().expect("有父目录")).expect("建包目录");
    std::fs::write(path, fake_class(version)).expect("写 class");
}

fn write_jar(path: &Path, entries: &[(&str, &str)]) {
    use std::io::Write as _;

    std::fs::create_dir_all(path.parent().expect("有父目录")).expect("建包目录");
    let file = std::fs::File::create(path).expect("建 jar");
    let mut writer = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, version) in entries {
        writer.start_file(*name, options).expect("写条目");
        writer.write_all(&fake_class(version)).expect("写字节");
    }
    writer.finish().expect("收尾");
}

/// 分派表里没有的 id 必须明确报错，不能静默返回空结果。
#[test]
fn unknown_tool_is_rejected() {
    let error = run_once(&request("duck.compare.nope", serde_json::json!({})))
        .expect_err("未知工具应当失败");
    assert!(error.contains("unknown tool"), "{error}");
}

/// 文本差异：`output` 是可复制的 unified diff，`body` 里是画双栏用的结构化行。
#[test]
fn text_diff_returns_unified_diff_text() {
    let output = run_once(&request(
        "duck.compare.text-diff",
        serde_json::json!({ "text_a": "alpha\nbeta", "text_b": "alpha\nbeta2" }),
    ))
    .expect("文本差异应当成功");
    assert!(output.contains("--- A"), "{output}");
    assert!(output.contains("-beta"), "{output}");
    assert!(output.contains("+beta2"), "{output}");
}

/// 两侧都空：不是崩溃，是一句能看懂的话。
#[test]
fn text_diff_rejects_two_empty_sides() {
    let error = run_once(&request(
        "duck.compare.text-diff",
        serde_json::json!({ "text_a": "", "text_b": "" }),
    ))
    .expect_err("两侧都空应当失败");
    assert!(error.contains("两侧都是空的"), "{error}");
}

/// 目录比对：只列有差异的条目，未变的不列（会埋掉要看的行）。
#[test]
fn dir_compare_lists_only_changed_entries() {
    let root = temp_dir("dir");
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).expect("建 a");
    std::fs::create_dir_all(&b).expect("建 b");
    std::fs::write(a.join("only-a.txt"), "x").expect("写");
    std::fs::write(b.join("only-b.txt"), "y").expect("写");
    std::fs::write(a.join("same.txt"), "same").expect("写");
    std::fs::write(b.join("same.txt"), "same").expect("写");

    let output = run_once(&request(
        "duck.compare.dir-compare",
        serde_json::json!({
            "dir_a": a.to_string_lossy(),
            "dir_b": b.to_string_lossy(),
        }),
    ))
    .expect("目录比对应当成功");

    assert!(output.contains("+ only-b.txt"), "{output}");
    assert!(output.contains("- only-a.txt"), "{output}");
    assert!(!output.contains("same.txt"), "未变的不该列出来：{output}");
    let _ = std::fs::remove_dir_all(root);
}

/// 三个新开关串起来跑一遍：`view = all` 让未变的条目也出、`format = table` 换成
/// Markdown 表、`exclude` 把命中规则的目录整棵剪掉。
#[test]
fn dir_compare_honours_view_format_and_exclude() {
    let root = temp_dir("dir-opts");
    let a = root.join("a");
    let b = root.join("b");
    for side in [&a, &b] {
        std::fs::create_dir_all(side.join("node_modules")).expect("建排除目录");
        std::fs::write(side.join("node_modules/dep.js"), "same").expect("写");
        std::fs::write(side.join("same.txt"), "same").expect("写");
    }

    let table = run_once(&request(
        "duck.compare.dir-compare",
        serde_json::json!({
            "dir_a": a.to_string_lossy(),
            "dir_b": b.to_string_lossy(),
            "view": "all",
            "format": "table",
            "exclude": "node_modules",
        }),
    ))
    .expect("目录比对应当成功");
    assert!(table.contains("| 状态 | 路径 | 大小 |"), "{table}");
    assert!(table.contains("| 未变 | `same.txt` |"), "{table}");
    // 头部那行「已按规则跳过: node_modules」本来就会提到它，所以查表格里的路径列。
    assert!(
        !table.contains("`node_modules`") && !table.contains("node_modules/"),
        "排除的目录不该出现在表格里：{table}"
    );
    assert!(table.contains("已按规则跳过: node_modules"), "{table}");

    // 反向确认：不填 exclude 时它就该在清单里，否则上一条断言等于没测。
    let plain = run_once(&request(
        "duck.compare.dir-compare",
        serde_json::json!({
            "dir_a": a.to_string_lossy(),
            "dir_b": b.to_string_lossy(),
            "view": "all",
        }),
    ))
    .expect("目录比对应当成功");
    assert!(plain.contains("= node_modules/"), "{plain}");

    let _ = std::fs::remove_dir_all(root);
}

/// HTML 形态是完整文档（双击就能看），不是片段。
#[test]
fn dir_compare_can_emit_an_html_report() {
    let root = temp_dir("dir-html");
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).expect("建 a");
    std::fs::create_dir_all(&b).expect("建 b");
    std::fs::write(b.join("new<1>.txt"), "y").expect("写");

    let output = run_once(&request(
        "duck.compare.dir-compare",
        serde_json::json!({
            "dir_a": a.to_string_lossy(),
            "dir_b": b.to_string_lossy(),
            "format": "html",
        }),
    ))
    .expect("目录比对应当成功");
    assert!(output.starts_with("<!DOCTYPE html>"), "{output}");
    assert!(
        output.contains("<td style=\"color:#22c55e\">新增</td>"),
        "{output}"
    );
    // 路径里的尖括号必须转义，否则报告在浏览器里会被吃掉半截。
    assert!(output.contains("new&lt;1&gt;.txt"), "{output}");
    let _ = std::fs::remove_dir_all(root);
}

/// class 比对：回退排在升级前面（要人立刻处理的那组在最上）。
#[test]
fn class_compare_puts_rollbacks_first() {
    let root = temp_dir("class");
    let a = root.join("a");
    let b = root.join("b");
    write_class(&a, "svc-a", "com/demo/Up", "1.2.4");
    write_class(&b, "svc-a", "com/demo/Up", "1.2.3");
    write_class(&a, "svc-a", "com/demo/Down", "1.2.3");
    write_class(&b, "svc-a", "com/demo/Down", "1.2.4");

    let output = run_once(&request(
        "duck.compare.class-compare",
        serde_json::json!({
            "dir_a": a.to_string_lossy(),
            "dir_b": b.to_string_lossy(),
            "env_a": "pro",
            "env_b": "uat",
        }),
    ))
    .expect("class 比对应当成功");

    assert!(output.contains("基准 2 个类"), "{output}");
    assert!(output.contains("回退 1 条"), "{output}");
    // 统计行按参考页的六项给（异常与回退同数：异常只由版本回退产生）。
    assert!(
        output.contains("新增 0 · 升级 1 · 回退 1 · 异常 1 · 相同 0 · 总计 2"),
        "{output}"
    );
    let rollback_at = output.find("回退 1 条").expect("有回退组");
    let upgrade_at = output.find("升级 1 条").expect("有升级组");
    assert!(rollback_at < upgrade_at, "回退必须排在升级前面：{output}");

    // 表头与列：参考页那张表的字段在这里一列不少。
    assert!(output.contains("服务"), "{output}");
    assert!(output.contains("类名"), "{output}");
    assert!(output.contains("包名"), "{output}");
    assert!(output.contains("基准版本"), "{output}");
    assert!(output.contains("对比版本"), "{output}");

    // 回退行：基准版本（新）在左、对比版本（现场）在右，方向不能写反。
    let rollback_rows = &output[rollback_at..upgrade_at];
    let base_at = rollback_rows.find("1.2.0.3").expect("回退行有基准版本");
    let target_at = rollback_rows.find("1.2.0.4").expect("回退行有对比版本");
    assert!(
        base_at < target_at,
        "基准版本应在对比版本左侧：{rollback_rows}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// jar 比对：只在基准侧的判「新增」，只在对比侧的不进结果。
#[test]
fn jar_compare_reports_new_packages() {
    let root = temp_dir("jar");
    let a = root.join("a");
    let b = root.join("b");
    write_jar(&a.join("lib/core.jar"), &[("com/demo/A.class", "2.0.0")]);
    write_jar(&b.join("lib/core.jar"), &[("com/demo/A.class", "1.0.0")]);
    write_jar(&a.join("lib/new.jar"), &[("com/demo/N.class", "1.0.0")]);
    write_jar(&b.join("lib/gone.jar"), &[("com/demo/G.class", "1.0.0")]);

    let output = run_once(&request(
        "duck.compare.jar-compare",
        serde_json::json!({
            "dir_a": a.to_string_lossy(),
            "dir_b": b.to_string_lossy(),
            "compare_lib": "true",
        }),
    ))
    .expect("jar 比对应当成功");

    assert!(output.contains("基准 2 个包"), "{output}");
    assert!(output.contains("lib/new.jar"), "{output}");
    assert!(
        !output.contains("lib/gone.jar"),
        "只在对比侧的不该出现：{output}"
    );
    // jar 的表头是五列：服务 / 路径 / 两边版本 / 两边类数（参考页那张表减掉状态列，
    // 状态已经由分组标题说明）。
    assert!(output.contains("JAR 路径"), "{output}");
    assert!(output.contains("类数（基准/对比）"), "{output}");
    let _ = std::fs::remove_dir_all(root);
}

/// 一组「一升级一回退」的 class 夹具，给筛选与导出两条测试共用。
fn class_fixture(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let root = temp_dir(name);
    let a = root.join("a");
    let b = root.join("b");
    write_class(&a, "svc-a", "com/demo/Up", "1.2.4");
    write_class(&b, "svc-a", "com/demo/Up", "1.2.3");
    write_class(&a, "svc-a", "com/demo/Down", "1.2.3");
    write_class(&b, "svc-a", "com/demo/Down", "1.2.4");
    (root, a, b)
}

/// 目录里唯一那个 CSV（文件名带环境名，测试不猜它）。
fn only_csv(dir: &Path) -> std::path::PathBuf {
    let files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .expect("读导出目录")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "csv"))
        .collect();
    assert_eq!(files.len(), 1, "导出目录应只有一个 CSV：{files:?}");
    files.into_iter().next().expect("有文件")
}

/// 「仅异常」只列回退组，但**统计行仍然是全量**——筛选是看的维度，不是算的维度。
#[test]
fn class_compare_view_abnormal_lists_only_rollbacks() {
    let (root, a, b) = class_fixture("class-view");
    let output = run_once(&request(
        "duck.compare.class-compare",
        serde_json::json!({
            "dir_a": a.to_string_lossy(),
            "dir_b": b.to_string_lossy(),
            "view": "abnormal",
        }),
    ))
    .expect("class 比对应当成功");

    assert!(output.contains("筛选：仅异常"), "{output}");
    assert!(output.contains("回退 1 条"), "{output}");
    assert!(
        !output.contains("升级 1 条"),
        "仅异常不该列升级组：{output}"
    );
    assert!(
        output.contains("新增 0 · 升级 1 · 回退 1 · 异常 1 · 相同 0 · 总计 2"),
        "统计行仍是全量：{output}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 导出 CSV：BOM 开头（Excel 不乱码）、表头九列、状态列已本地化且异常优先。
#[test]
fn class_compare_exports_a_utf8_bom_csv() {
    let (root, a, b) = class_fixture("class-csv");
    let out_dir = root.join("out");
    std::fs::create_dir_all(&out_dir).expect("建导出目录");

    let output = run_once(&request(
        "duck.compare.class-compare",
        serde_json::json!({
            "dir_a": a.to_string_lossy(),
            "dir_b": b.to_string_lossy(),
            "env_a": "上线包",
            "env_b": "生产包",
            "csv_dir": out_dir.to_string_lossy(),
        }),
    ))
    .expect("class 比对应当成功");

    let csv = only_csv(&out_dir);
    assert!(
        csv.file_name().unwrap_or_default().to_string_lossy()
            == "class-compare-上线包-vs-生产包.csv",
        "{csv:?}"
    );
    let raw = std::fs::read_to_string(&csv).expect("读 CSV");
    assert!(raw.starts_with('\u{feff}'), "CSV 必须以 BOM 开头");
    let mut lines = raw.trim_start_matches('\u{feff}').lines();
    assert_eq!(
        lines.next().expect("表头"),
        "状态,服务,类名,包名,基准版本,对比版本,级差,基准文件,对比文件"
    );
    // 回退行在 CSV 里叫「异常」（参考页的状态列先判 is_abnormal）。
    assert!(raw.contains("异常,svc-a,Down,com.demo"), "{raw}");
    assert!(raw.contains("升级,svc-a,Up,com.demo"), "{raw}");
    // 报告末尾要给出落盘位置与行数——不留一句「已导出」就完事。
    assert!(output.contains("CSV："), "{output}");
    assert!(output.contains("（2 行）"), "{output}");
    let _ = std::fs::remove_dir_all(root);
}

/// jar 导出：类数是两列、对比类型给原始字面量（脚本可以直接判）。
#[test]
fn jar_compare_exports_counts_and_raw_verdicts() {
    let root = temp_dir("jar-csv");
    let a = root.join("a");
    let b = root.join("b");
    let out_dir = root.join("out");
    std::fs::create_dir_all(&out_dir).expect("建导出目录");
    write_jar(&a.join("lib/core.jar"), &[("com/demo/A.class", "2.0.0")]);
    write_jar(&b.join("lib/core.jar"), &[("com/demo/A.class", "1.0.0")]);
    write_jar(&a.join("lib/new.jar"), &[("com/demo/N.class", "1.0.0")]);

    run_once(&request(
        "duck.compare.jar-compare",
        serde_json::json!({
            "dir_a": a.to_string_lossy(),
            "dir_b": b.to_string_lossy(),
            "csv_dir": out_dir.to_string_lossy(),
        }),
    ))
    .expect("jar 比对应当成功");

    let raw = std::fs::read_to_string(only_csv(&out_dir)).expect("读 CSV");
    assert!(
        raw.contains("状态,JAR 包名,服务,基准版本,对比版本,基准类数,对比类数,对比类型,是否异常,基准文件,对比文件"),
        "{raw}"
    );
    // 升级：两个版本与两边的类数都要在。
    assert!(raw.contains("升级,lib/core.jar"), "{raw}");
    assert!(raw.contains(",1,1,update,否,"), "{raw}");
    // 新增：空壳条目（没解压过），两边类数都是 0、版本也留空。
    assert!(raw.contains("新增,lib/new.jar"), "{raw}");
    assert!(raw.contains(",0,0,new,否,"), "{raw}");
    let _ = std::fs::remove_dir_all(root);
}

/// 目录不存在：三个吃目录的工具都要在动手前拦下，报同一句话。
#[test]
fn missing_directory_is_rejected_by_every_tool() {
    let root = temp_dir("missing");
    let missing = root.join("does-not-exist");
    for tool in [
        "duck.compare.dir-compare",
        "duck.compare.class-compare",
        "duck.compare.jar-compare",
    ] {
        let error = run_once(&request(
            tool,
            serde_json::json!({
                "dir_a": missing.to_string_lossy(),
                "dir_b": root.to_string_lossy(),
            }),
        ))
        .expect_err("目录不存在应当失败");
        assert!(error.contains("路径不是有效目录"), "{tool}: {error}");
    }
    let _ = std::fs::remove_dir_all(root);
}

/// 配置比对：两侧写法不同（properties vs YAML）但内容一致的部分不该报差异——
/// 这正是「先归一、再拍平」要保证的事。
#[test]
fn config_compare_normalises_properties_and_yaml() {
    let output = run_once(&request(
        "duck.compare.config-compare",
        serde_json::json!({
            "config_a": "server.port=8080\ndb.url=jdbc:mysql://a\nmq.url=x",
            "config_b": "server:\n  port: 8080\ndb:\n  url: jdbc:mysql://b\n  pool: 10",
        }),
    ))
    .expect("配置比对应当成功");

    assert!(output.contains("PROPERTIES vs YAML"), "{output}");
    assert!(
        output.contains("~ db.url  (jdbc:mysql://a → jdbc:mysql://b)"),
        "{output}"
    );
    assert!(output.contains("- mq.url  (x)"), "{output}");
    assert!(output.contains("+ db.pool  (10)"), "{output}");
    // `server.port` 两侧一致：默认「仅差异」下不该出现。
    assert!(!output.contains("server.port"), "{output}");
}

/// 表格形态 + 全部视图：一致项也要列出来，且状态词是「仅左 / 仅右」而不是
/// 目录比对那套「新增 / 删除」。
#[test]
fn config_compare_table_lists_same_rows_too() {
    let output = run_once(&request(
        "duck.compare.config-compare",
        serde_json::json!({
            "config_a": "a=1",
            "config_b": "a=1\nb=2",
            "view": "all",
            "format": "table",
        }),
    ))
    .expect("配置比对应当成功");

    assert!(
        output.contains("| 状态 | Key | 左侧值 | 右侧值 |"),
        "{output}"
    );
    assert!(output.contains("| 相同 | `a` | 1 | 1 |"), "{output}");
    assert!(output.contains("| 仅右 | `b` | — | 2 |"), "{output}");
}

/// 坏 JSON 要明确报错：静默当成 properties 会「一个键都没读到 ⇒ 报完全一致」。
#[test]
fn config_compare_rejects_broken_json() {
    let error = run_once(&request(
        "duck.compare.config-compare",
        serde_json::json!({ "config_a": "{oops", "config_b": "a=1" }),
    ))
    .expect_err("坏 JSON 应当失败");
    assert!(error.contains("JSON 解析失败"), "{error}");
}
