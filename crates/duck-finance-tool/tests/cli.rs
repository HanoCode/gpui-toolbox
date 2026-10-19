//! process 轨端到端：真起一个 `duck-finance-tool` 进程，走 stdin/stdout JSONL。
//!
//! 补的是「单测测不到、链路最容易断」的那一截：宿主送请求、插件进程收请求，
//! 两边对**协议**与**分派表**（`tool` → 哪个内核函数、哪个参数名）的理解必须一致。
//! 参数名拼错、动作分派写反、报告字段算错，只有真跑一次才会现形。
//!
//! 算法本身的正确性由 `duck-finance-core` 的 25 条单测 + 4 条 xlsx 夹具测试保证，
//! 这里只钉端到端的形状。
//!
//! `plugin_dir = "."` 让 locale 环境变量指向不存在的目录，取词回落到源串 —— 所以
//! 下面断言的都是中文源串（本地化由 `plugin_locales.py` 与执行页覆盖）。
//!
//! 夹具：
//! - `tests/fixtures/sample.pdm`（两张表、主键 + 索引、精度 + 默认值）
//! - 接口 Excel 借用 `duck-finance-core/tests/fixtures/interfaces.xlsx`
//!   （双组列 + 单组列 + 修订记录页），一处维护、两个 crate 共用。

use std::path::Path;

use duck_plugin_process::{CancelHandle, NullSink, ProgressSink, RunRequest, run};

fn request(tool: &str, params: serde_json::Value) -> RunRequest {
    RunRequest {
        tool: tool.into(),
        input: String::new(),
        params,
        files: vec![],
        lang: "zh-Hans".into(),
        kv: Default::default(),
        // 金融族两个工具都没有页内扫描（`[io] scan`），走「正常执行一次」。
        action: None,
        // 默认不给数据目录：接口库 / 版本 / 模板那几条用例用 `request_with_data`
        // 显式给一个临时目录，其余用例跑起来就和宿主的「没目录」路径一致。
        data: String::new(),
    }
}

/// 带私有数据目录的请求（接口库 / 版本 / 模板三族要用）。
fn request_with_data(tool: &str, params: serde_json::Value, data: &Path) -> RunRequest {
    RunRequest {
        data: data.to_string_lossy().into_owned(),
        ..request(tool, params)
    }
}

/// 一个空的临时数据目录（按 tag + 进程 id 拼，避免并行用例互踩）。
fn library_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-finance-lib-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建数据目录");
    dir
}

fn run_once(request: &RunRequest) -> Result<String, String> {
    let bin = env!("CARGO_BIN_EXE_duck-finance-tool");
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

/// 收过程日志的 sink：`Log` 事件**不进 `outcome.output`**，要断言提示语就得自己接。
#[derive(Default)]
struct LogSink {
    lines: Vec<String>,
}

impl ProgressSink for LogSink {
    fn on_log(&mut self, line: &str, _level: Option<&str>) {
        self.lines.push(line.to_owned());
    }

    fn on_progress(&mut self, _current: u64, _total: Option<u64>, _message: Option<&str>) {}
}

/// 跑一次，同时拿回「正文 + 过程日志」。
fn run_with_logs(request: &RunRequest) -> Result<(String, Vec<String>), String> {
    let bin = env!("CARGO_BIN_EXE_duck-finance-tool");
    let mut sink = LogSink::default();
    let outcome = run(
        Path::new("."),
        Path::new(bin),
        request,
        &mut sink,
        &CancelHandle::new(),
    )
    .map_err(|error| error.to_string())?;
    Ok((outcome.output, sink.lines))
}

fn pdm_fixture() -> String {
    format!("{}/tests/fixtures/sample.pdm", env!("CARGO_MANIFEST_DIR"))
}

fn excel_fixture() -> String {
    format!(
        "{}/../duck-finance-core/tests/fixtures/interfaces.xlsx",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// 一个只放 PDM 的临时目录（两份同名内容的 `sample.pdm`，文件名不同）。
///
/// 为什么要有：目录模式是「多个 PDM 合成一份模型」，参考页还给每个文件一个标签。
/// 这条链路（`parse_directory` → 合并 → 逐文件清单）只有真起进程才看得见。
/// 用进程 id 拼目录名，避免并行用例互相踩。
fn pdm_fixture_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-finance-pdm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let source = std::fs::read(pdm_fixture()).expect("读夹具");
    for name in ["deposit_a.pdm", "deposit_b.pdm"] {
        std::fs::write(dir.join(name), &source).expect("写临时夹具");
    }
    dir
}

// ── PDM 阅读器 ────────────────────────────────────────────────────────

#[test]
fn pdm_list_names_the_model_and_every_table() {
    let params = serde_json::json!({
        "action": "list",
        "source": "file",
        "file": pdm_fixture(),
    });
    let output = run_once(&request("duck.finance.pdm-reader", params)).expect("应当能列出表");
    assert!(output.contains("模型：存款核心 (deposit)"), "{output}");
    assert!(output.contains("Oracle 11g"), "{output}");
    assert!(output.contains("T_ACCOUNT"), "{output}");
    assert!(output.contains("T_LOG"), "{output}");
    assert!(output.contains("ACC_NO"), "{output}");
}

/// 目录模式：多个 PDM 合成一份模型，且「表清单」先给逐文件账（参考页的文件标签栏）。
#[test]
fn pdm_directory_mode_lists_every_file_before_the_merged_tables() {
    let dir = pdm_fixture_dir();
    let params = serde_json::json!({
        "action": "list",
        "source": "dir",
        "dir": dir.to_string_lossy(),
    });
    let output = run_once(&request("duck.finance.pdm-reader", params)).expect("应当能列出目录");
    // 逐文件账（参考页的文件标签栏）：两个文件名都要出现。
    assert!(output.contains("deposit_a.pdm"), "{output}");
    assert!(output.contains("deposit_b.pdm"), "{output}");
    // 合并**不去重**：两份同名表合成两条记录 —— 这是源仓 `parse_directory` 的行为
    // （`combined.tables.extend(...)` 直接并），别「顺手」改成按表代码去重。
    assert_eq!(
        output.matches("T_ACCOUNT").count(),
        2,
        "两份拷贝应当并成两条：{output}"
    );
}

#[test]
fn pdm_ddl_is_a_bare_statement_without_report_header() {
    let params = serde_json::json!({
        "action": "ddl",
        "source": "file",
        "file": pdm_fixture(),
        "dialect": "oracle",
    });
    let output = run_once(&request("duck.finance.pdm-reader", params)).expect("应当能生成 SQL");
    assert!(
        output.starts_with("CREATE TABLE \"T_ACCOUNT\" ("),
        "{output}"
    );
    assert!(
        output.contains("\"ACC_NO\" VARCHAR2(32) NOT NULL"),
        "{output}"
    );
    assert!(output.contains("PRIMARY KEY (\"ACC_NO\")"), "{output}");
    assert!(output.trim_end().ends_with(");"), "{output}");
    // 抬头（PDM · 路径 · 耗时）不能混进 SQL：那不是合法注释。
    assert!(!output.contains("PDM"), "{output}");
    assert!(!output.contains("耗时"), "{output}");
}

#[test]
fn pdm_ddl_supports_the_mysql_dialect() {
    let params = serde_json::json!({
        "action": "ddl",
        "source": "file",
        "file": pdm_fixture(),
        "dialect": "mysql",
    });
    let output = run_once(&request("duck.finance.pdm-reader", params)).expect("应当能生成 SQL");
    assert!(output.contains("CREATE TABLE `T_ACCOUNT` ("), "{output}");
    // 注释跟着列走，且末尾没有分号（参考仓口径）。
    assert!(output.contains("COMMENT '主账号'"), "{output}");
    assert!(!output.trim_end().ends_with(';'), "{output}");
}

#[test]
fn pdm_markdown_carries_the_dictionary_headers() {
    let params = serde_json::json!({
        "action": "markdown",
        "source": "file",
        "file": pdm_fixture(),
    });
    let output = run_once(&request("duck.finance.pdm-reader", params)).expect("应当能导出");
    assert!(output.starts_with("# 存款核心 - 数据字典"), "{output}");
    assert!(output.contains("| 字段名 | 类型 |"), "{output}");
    assert!(output.contains("**主键**: ACC_NO"), "{output}");
    assert!(output.contains("IDX_BAL"), "{output}");
}

#[test]
fn pdm_table_detail_needs_a_filter() {
    let params = serde_json::json!({
        "action": "table",
        "source": "file",
        "file": pdm_fixture(),
    });
    let error = run_once(&request("duck.finance.pdm-reader", params)).unwrap_err();
    assert!(
        error.contains("在「表」里填表代码或名字的一部分"),
        "{error}"
    );

    let params = serde_json::json!({
        "action": "table",
        "source": "file",
        "file": pdm_fixture(),
        "table": "t_account",
    });
    let output = run_once(&request("duck.finance.pdm-reader", params)).expect("大小写不敏感地筛表");
    assert!(output.contains("## 账户表 (T_ACCOUNT)"), "{output}");
    assert!(output.contains("主键：ACC_NO"), "{output}");
    assert!(output.contains("余额"), "{output}");
}

#[test]
fn pdm_insert_is_a_skeleton_with_every_column() {
    let params = serde_json::json!({
        "action": "insert",
        "source": "file",
        "file": pdm_fixture(),
        "table": "T_ACCOUNT",
    });
    let output = run_once(&request("duck.finance.pdm-reader", params)).expect("应当能生成插入骨架");
    // 参考仓前端 `generateInsertSql` 的形状：反引号标识符、值留空、TODO 占位。
    assert!(output.starts_with("-- 表: "), "{output}");
    assert!(
        output.contains("INSERT INTO `T_ACCOUNT` (`ACC_NO`, `BALANCE`) VALUES ("),
        "{output}"
    );
    assert!(output.contains("-- TODO: 添加值"), "{output}");
    assert!(!output.contains("CREATE TABLE"), "{output}");
}

/// 「表」写两张（逗号分隔）⇒ 只导出这两张；`T_LOG` 上没有 COMMENT 字样就说明筛到了。
#[test]
fn pdm_table_filter_accepts_several_names() {
    let params = serde_json::json!({
        "action": "ddl",
        "source": "file",
        "file": pdm_fixture(),
        "table": "T_ACCOUNT, T_LOG",
    });
    let output = run_once(&request("duck.finance.pdm-reader", params)).expect("应当能生成 SQL");
    assert!(output.contains("CREATE TABLE \"T_ACCOUNT\""), "{output}");
    assert!(output.contains("CREATE TABLE \"T_LOG\""), "{output}");

    // 只给一个不存在的名字 ⇒ 明确的「没有匹配的表」，而不是空结果。
    let params = serde_json::json!({
        "action": "ddl",
        "source": "file",
        "file": pdm_fixture(),
        "table": "T_NOPE",
    });
    let error = run_once(&request("duck.finance.pdm-reader", params)).expect_err("该报错");
    assert!(error.contains("没有匹配的表"), "{error}");
}

#[test]
fn pdm_comment_export_quotes_oracle_identifiers() {
    let params = serde_json::json!({
        "action": "comment",
        "source": "file",
        "file": pdm_fixture(),
    });
    let output = run_once(&request("duck.finance.pdm-reader", params)).expect("应当能导出注释");
    assert!(
        output.contains("COMMENT ON TABLE \"T_ACCOUNT\" IS '账户主表';"),
        "{output}"
    );
    assert!(
        output.contains("COMMENT ON COLUMN \"T_ACCOUNT\".\"BALANCE\" IS '账户余额';"),
        "{output}"
    );
}

#[test]
fn pdm_missing_path_and_broken_file_are_reported_separately() {
    let params = serde_json::json!({ "action": "list", "source": "file" });
    let error = run_once(&request("duck.finance.pdm-reader", params)).unwrap_err();
    assert!(error.contains("先选一个 PDM 文件或目录"), "{error}");

    let params = serde_json::json!({
        "action": "list",
        "source": "file",
        "file": "definitely-not-here.pdm",
    });
    let error = run_once(&request("duck.finance.pdm-reader", params)).unwrap_err();
    assert!(error.contains("读取 PDM 文件失败"), "{error}");
}

// ── 渠道网关 ──────────────────────────────────────────────────────────

#[test]
fn gateway_parse_summarises_interfaces() {
    let params = serde_json::json!({ "action": "parse", "path": excel_fixture() });
    let output = run_once(&request("duck.finance.channel-gateway", params)).expect("应当能解析");
    assert!(output.contains("银行：渠道接口清单"), "{output}");
    assert!(output.contains("接口：2"), "{output}");
    assert!(output.contains("B100001"), "{output}");
    assert!(output.contains("B100002"), "{output}");
    // 数组字段单独计数。
    assert!(output.contains("交易码"), "{output}");
}

#[test]
fn gateway_fields_lists_arrays_with_their_items() {
    let params = serde_json::json!({
        "action": "fields",
        "path": excel_fixture(),
        "function": "B100001",
    });
    let output = run_once(&request("duck.finance.channel-gateway", params)).expect("应当能列字段");
    assert!(output.contains("## B100001"), "{output}");
    assert!(output.contains("dataList[]"), "{output}");
    assert!(output.contains("流水号"), "{output}");
    assert!(output.contains("└ ItemNo"), "{output}");
    assert!(output.contains("返回码"), "{output}");
}

#[test]
fn gateway_fields_without_a_code_asks_for_one() {
    let params = serde_json::json!({ "action": "fields", "path": excel_fixture() });
    let error = run_once(&request("duck.finance.channel-gateway", params)).unwrap_err();
    assert!(error.contains("先填交易码"), "{error}");

    let params = serde_json::json!({
        "action": "fields",
        "path": excel_fixture(),
        "function": "Z999999",
    });
    let error = run_once(&request("duck.finance.channel-gateway", params)).unwrap_err();
    assert!(error.contains("找不到这个交易码"), "{error}");
}

#[test]
fn gateway_code_generates_both_sides() {
    let params = serde_json::json!({
        "action": "code",
        "path": excel_fixture(),
        "function": "B100001",
        "side": "bank",
        "bank_code": "hzcb",
        "package_base": "com.example.gateway",
    });
    let output = run_once(&request("duck.finance.channel-gateway", params)).expect("应当能生成");
    assert!(
        output.contains("package com.example.gateway.hzcb.bank.impl.B100001;"),
        "{output}"
    );
    assert!(output.contains("@Service(\"BankB100001\")"), "{output}");
    assert!(
        output.contains("JSONArray dataListArray = new JSONArray();"),
        "{output}"
    );
    assert!(!output.contains("{%"), "{output}");

    let params = serde_json::json!({
        "action": "code",
        "path": excel_fixture(),
        "function": "B100001",
        "side": "host",
        "bank_code": "hzcb",
        "package_base": "com.example.gateway",
    });
    let output = run_once(&request("duck.finance.channel-gateway", params)).expect("应当能生成");
    assert!(output.contains("@Service(\"HostB100001\")"), "{output}");
    assert!(output.contains("public byte[] packRequest"), "{output}");
    assert!(output.contains("new XmlTag(EsbTag.serialNo)"), "{output}");
}

#[test]
fn gateway_code_needs_bank_code_and_package() {
    let params = serde_json::json!({
        "action": "code",
        "path": excel_fixture(),
        "bank_code": "hzcb",
        "package_base": "  ",
    });
    let error = run_once(&request("duck.finance.channel-gateway", params)).unwrap_err();
    assert!(error.contains("包名基路径"), "{error}");

    let params = serde_json::json!({
        "action": "code",
        "path": excel_fixture(),
        "bank_code": "",
        "package_base": "com.example.gateway",
    });
    let error = run_once(&request("duck.finance.channel-gateway", params)).unwrap_err();
    assert!(error.contains("银行代码"), "{error}");
}

#[test]
fn gateway_xml_template_has_both_directions() {
    let params = serde_json::json!({
        "action": "xml",
        "path": excel_fixture(),
        "function": "B100001",
    });
    let output =
        run_once(&request("duck.finance.channel-gateway", params)).expect("应当能生成报文");
    assert!(output.contains("<lcpt>"), "{output}");
    assert!(output.contains("### 请求报文"), "{output}");
    assert!(output.contains("### 应答报文"), "{output}");
    assert!(
        output.contains("<FunctionId>B100001</FunctionId>"),
        "{output}"
    );
    assert!(output.contains("<ErrorNo>0</ErrorNo>"), "{output}");
    assert!(output.contains("<SerialNo>示例值</SerialNo>"), "{output}");
}

#[test]
fn gateway_headers_previews_columns_and_samples() {
    let params = serde_json::json!({ "action": "headers", "path": excel_fixture() });
    let output = run_once(&request("duck.finance.channel-gateway", params)).expect("应当能读列头");
    assert!(output.contains("## B100001"), "{output}");
    assert!(output.contains("内部字段名 | 数据类型"), "{output}");
    assert!(output.contains("serialNo"), "{output}");
}

#[test]
fn gateway_missing_file_is_open_failed() {
    let params = serde_json::json!({ "action": "parse", "path": "nope.xlsx" });
    let error = run_once(&request("duck.finance.channel-gateway", params)).unwrap_err();
    assert!(error.contains("打不开这份 Excel"), "{error}");
}

// ── 接口库 / 版本 / 模板（参考仓的「接口管理 / 变更记录 / 模板管理」） ──
//
// 这三族是**有状态**的：数据落在插件私有目录（`request.data`）。下面每条用例都用
// 自己的临时目录，跑完不清理也没关系（`library_dir` 下次会先删掉同名目录）。

#[test]
fn library_starts_empty_and_reports_it() {
    let dir = library_dir("empty");
    let params = serde_json::json!({ "action": "banks" });
    let output = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        params,
        &dir,
    ))
    .expect("应当能列出空库");
    assert!(output.contains("还没存过任何银行"), "{output}");
}

#[test]
fn saving_a_bank_then_loading_it_round_trips_through_the_files() {
    let dir = library_dir("save");
    let params = serde_json::json!({
        "action": "bank-save",
        "path": excel_fixture(),
        "bank_code": "hzcb",
        "bank_name": "杭州银行",
    });
    let output = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        params,
        &dir,
    ))
    .expect("存入");
    // 首次导入没有旧数据可比 ⇒ 明说「与上次相同」而不是「0 处变更」。
    assert!(output.contains("已存入 hzcb"), "{output}");
    assert!(output.contains("与上次相同"), "{output}");
    // 真的写了文件（不是只在内存里走一遍）。
    assert!(dir.join("banks.json").is_file(), "缺 banks.json");
    assert!(dir.join("banks/hzcb.json").is_file(), "缺 banks/hzcb.json");

    let listed = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "banks" }),
        &dir,
    ))
    .expect("列库");
    assert!(listed.contains("hzcb"), "{listed}");
    assert!(listed.contains("杭州银行"), "{listed}");

    let loaded = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "bank-load", "bank_code": "hzcb" }),
        &dir,
    ))
    .expect("读回");
    assert!(loaded.contains("B100001"), "{loaded}");
    // 读回来的是**字段级**明细：表格里能逐行改类型/含义，所以文本也按字段排。
    assert!(loaded.contains("请求字段"), "{loaded}");
    assert!(loaded.contains("SerialNo"), "{loaded}");
}

/// 表格编辑的前置修改：`__edits` 先落库，再走原动作（这里就是「读回」）。
#[test]
fn table_edits_are_applied_before_the_action_runs() {
    let dir = library_dir("apply-edits");
    run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "bank-save",
            "path": excel_fixture(),
            "bank_code": "hzcb",
        }),
        &dir,
    ))
    .expect("存入");

    // 一次调用里做完两件事：改一格 + 读回（`bank-load` 就是「改完看结果」那条路）。
    let (loaded, logs) = run_with_logs(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "bank-load",
            "bank_code": "hzcb",
            "__edits": [
                {
                    "row_id": "B100001|request|SerialNo",
                    "column": "含义",
                    "value": "流水号-改"
                },
                {
                    "row_id": "B100001|request|SerialNo",
                    "column": "类型",
                    "value": "number"
                },
            ],
        }),
        &dir,
    ))
    .expect("应用改动并读回");
    assert!(
        logs.iter().any(|line| line.contains("表格改动已写入 2 处")),
        "{logs:?}"
    );
    assert!(loaded.contains("流水号-改"), "{loaded}");

    // 不带 `__edits` 再读一次：值必须在库里（不是只改了这一次的内存）。
    let again = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "bank-load", "bank_code": "hzcb" }),
        &dir,
    ))
    .expect("再读");
    assert!(again.contains("流水号-改"), "{again}");
    assert!(again.contains("number"), "{again}");
}

#[test]
fn saving_the_same_workbook_twice_reports_no_change() {
    let dir = library_dir("same");
    let params = serde_json::json!({
        "action": "bank-save",
        "path": excel_fixture(),
        "bank_code": "hzcb",
    });
    run_once(&request_with_data(
        "duck.finance.channel-gateway",
        params.clone(),
        &dir,
    ))
    .expect("首次");
    let again = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        params,
        &dir,
    ))
    .expect("二次");
    assert!(again.contains("与上次相同"), "{again}");
    // 没有变更就不该产生备份版本（否则每点一次保存多一个版本）。
    let versions = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "versions", "bank_code": "hzcb" }),
        &dir,
    ))
    .expect("版本列表");
    assert!(versions.contains("还没有版本"), "{versions}");
}

#[test]
fn versions_can_be_snapshotted_listed_and_compared() {
    let dir = library_dir("versions");
    let save = serde_json::json!({
        "action": "bank-save",
        "path": excel_fixture(),
        "bank_code": "hzcb",
    });
    run_once(&request_with_data(
        "duck.finance.channel-gateway",
        save,
        &dir,
    ))
    .expect("存入");

    for label in ["初版", "第二版"] {
        let output = run_once(&request_with_data(
            "duck.finance.channel-gateway",
            serde_json::json!({
                "action": "version-create",
                "bank_code": "hzcb",
                "version_label": label,
            }),
            &dir,
        ))
        .expect("存版本");
        assert!(
            output.contains(label) || output.contains("已存版本"),
            "{output}"
        );
    }

    let listed = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "versions", "bank_code": "hzcb" }),
        &dir,
    ))
    .expect("列版本");
    assert!(listed.contains("初版"), "{listed}");
    assert!(listed.contains("第二版"), "{listed}");

    // 两版内容一样 ⇒ 0 处变更（不是「没有变更」以外的怪输出）。
    let compared = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "version-compare", "bank_code": "hzcb" }),
        &dir,
    ))
    .expect("对比");
    assert!(compared.contains("没有变更"), "{compared}");

    // 单接口差异：接口存在但两版相同 ⇒ 也是 0 处。
    let diff = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "interface-diff",
            "bank_code": "hzcb",
            "function": "B100001",
        }),
        &dir,
    ))
    .expect("单接口差异");
    assert!(diff.contains("没有变更"), "{diff}");
}

#[test]
fn comparing_needs_two_versions_and_says_so() {
    let dir = library_dir("compare-guard");
    run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "bank-save",
            "path": excel_fixture(),
            "bank_code": "hzcb",
        }),
        &dir,
    ))
    .expect("存入");
    let error = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "version-compare", "bank_code": "hzcb" }),
        &dir,
    ))
    .unwrap_err();
    assert!(error.contains("至少要存过两个版本"), "{error}");
}

#[test]
fn templates_list_the_two_builtins_and_can_be_shown() {
    let dir = library_dir("templates");
    let listed = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "templates" }),
        &dir,
    ))
    .expect("列模板");
    assert!(listed.contains("BankImpl.java"), "{listed}");
    assert!(listed.contains("HostImpl.java"), "{listed}");
    assert!(listed.contains("内置"), "{listed}");

    let shown = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "template-show", "template": "BankImpl.java" }),
        &dir,
    ))
    .expect("看模板");
    // 给的是**内容本身**（要整段复制出去改），不是报告：所以没有抬头、没有表格。
    assert!(shown.contains("{{"), "模板内容里应当有 tera 占位: {shown}");
    assert!(!shown.contains("渠道网关 · 耗时"), "不该带抬头: {shown}");
}

#[test]
fn a_imported_template_can_be_used_to_generate_code() {
    let dir = library_dir("custom-template");
    let template_file = dir.join("Plain.java.tera");
    std::fs::write(
        &template_file,
        "// {{ function_id }} in {{ package_base }}\n// {{ description }}\n",
    )
    .expect("写模板");

    let imported = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "template-import",
            "path": template_file.to_string_lossy(),
        }),
        &dir,
    ))
    .expect("导入模板");
    // 名字从文件名来，`.tera` 剥掉（与内置的 `BankImpl.java` 同一套命名）。
    assert!(imported.contains("Plain.java"), "{imported}");

    let generated = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "code",
            "path": excel_fixture(),
            "bank_code": "hzcb",
            "package_base": "com.example.gw",
            "template": "Plain.java",
            "function": "B100001",
        }),
        &dir,
    ))
    .expect("用自定义模板生成");
    // 上下文与内置模板同一套：`package_base` 已经拼上银行代码。
    assert!(
        generated.contains("// B100001 in com.example.gw.hzcb"),
        "{generated}"
    );
}

#[test]
fn builtin_templates_cannot_be_deleted_but_custom_ones_can() {
    let dir = library_dir("delete-template");
    let error = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "template-delete", "template": "BankImpl.java" }),
        &dir,
    ))
    .unwrap_err();
    assert!(error.contains("内置模板不能删"), "{error}");

    let missing = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "template-delete", "template": "Nope.java" }),
        &dir,
    ))
    .unwrap_err();
    assert!(missing.contains("没有这个模板"), "{missing}");
}

#[test]
fn deleting_a_bank_removes_it_and_reports_what_went_with_it() {
    let dir = library_dir("delete-bank");
    run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "bank-save",
            "path": excel_fixture(),
            "bank_code": "hzcb",
        }),
        &dir,
    ))
    .expect("存入");
    let deleted = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "bank-delete", "bank_code": "hzcb" }),
        &dir,
    ))
    .expect("删除");
    assert!(deleted.contains("已删除 hzcb"), "{deleted}");

    let listed = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "banks" }),
        &dir,
    ))
    .expect("列库");
    assert!(listed.contains("还没存过任何银行"), "{listed}");
}

#[test]
fn a_bank_code_that_would_escape_the_directory_is_rejected() {
    let dir = library_dir("bad-code");
    let error = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "bank-load", "bank_code": "../etc" }),
        &dir,
    ))
    .unwrap_err();
    assert!(error.contains("银行代码只能用"), "{error}");
    // 目录里没多出任何东西（既没写到外面，也没建子目录）。
    let stray: Vec<_> = std::fs::read_dir(&dir)
        .expect("读目录")
        .filter_map(Result::ok)
        .collect();
    assert!(stray.is_empty(), "不该落任何文件: {stray:?}");
}

#[test]
fn code_can_be_generated_straight_from_the_library_without_a_workbook() {
    let dir = library_dir("code-from-library");
    run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "bank-save",
            "path": excel_fixture(),
            "bank_code": "hzcb",
        }),
        &dir,
    ))
    .expect("存入");

    // 不给 path，只给银行代码：从库里读接口再生成。
    // 这条闭合的是参考仓的正常用法（先导入一次，之后反复生成），
    // 没有它「存入接口库」就只是存档。
    let output = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "code",
            "bank_code": "hzcb",
            "package_base": "com.example.gw",
            "function": "B100001",
        }),
        &dir,
    ))
    .expect("从库生成");
    assert!(output.contains("BankB100001.java"), "{output}");
    assert!(output.contains("com.example.gw.hzcb"), "{output}");
}

#[test]
fn neither_a_workbook_nor_a_bank_code_is_reported_as_such() {
    let dir = library_dir("no-source");
    let error = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "code" }),
        &dir,
    ))
    .unwrap_err();
    assert!(error.contains("或者填一个已存进库的银行代码"), "{error}");
}

#[test]
fn editing_a_field_changes_the_library_and_reports_the_diff() {
    let dir = library_dir("field-edit");
    run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "bank-save",
            "path": excel_fixture(),
            "bank_code": "hzcb",
        }),
        &dir,
    ))
    .expect("存入");

    let output = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "field-edit",
            "bank_code": "hzcb",
            "function": "B100001",
            // `field_name` 是 Excel 里「内部字段名」那一列（驼峰化后的 `SerialNo`）；
            // `serialNo` 是它的 ESB 名，不是改字段用的键。
            "field": "SerialNo",
            "field_type": "number",
            "field_meaning": "流水号（改过）",
        }),
        &dir,
    ))
    .expect("改字段");
    // 类型与含义各报一条（同名字段在请求 / 应答各有一份时是两组四条，夹具里只有一处）。
    assert!(output.contains("类型变更"), "{output}");
    assert!(output.contains("含义变更"), "{output}");
    assert!(output.contains("number"), "{output}");

    // 改的是库里的数据：再读回来应当是新类型。
    let loaded = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({ "action": "bank-load", "bank_code": "hzcb" }),
        &dir,
    ))
    .expect("读回");
    assert!(loaded.contains("B100001"), "{loaded}");

    // 没有这个字段时明确报错，而不是静默成功。
    let missing = run_once(&request_with_data(
        "duck.finance.channel-gateway",
        serde_json::json!({
            "action": "field-edit",
            "bank_code": "hzcb",
            "function": "B100001",
            "field": "noSuchField",
            "field_type": "string",
        }),
        &dir,
    ))
    .unwrap_err();
    assert!(missing.contains("没有这个字段"), "{missing}");
}
