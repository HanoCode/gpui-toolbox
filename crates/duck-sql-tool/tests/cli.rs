//! process 轨端到端：真起一个 `duck-sql-tool` 进程，走 stdin/stdout JSONL。
//!
//! 这一层补的是「单元测试测不到、链路最可能断」的一截：宿主用
//! [`duck_plugin_process::run`] 送请求、插件进程用 `serve` 收请求，
//! 两边对协议的理解必须一致（字段名、事件形状、错误路径）。

use std::path::Path;

use duck_plugin_process::{CancelHandle, NullSink, RunRequest, run};

const DDL: &str = "CREATE TABLE `fund_trans` (\n  `id` bigint NOT NULL,\n  `ta_code` varchar(10),\n  PRIMARY KEY (`id`)\n);";

fn request(input: &str, params: serde_json::Value) -> RunRequest {
    RunRequest {
        tool: "duck.sql.ddl-generator".into(),
        input: input.into(),
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
/// 取词自然回落到源串（本地化由 `plugin_locales.py` 与执行页负责）。
fn run_once(request: &RunRequest) -> Result<String, String> {
    let bin = env!("CARGO_BIN_EXE_duck-sql-tool");
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

#[test]
fn shard_mode_returns_one_ddl_per_shard() {
    let output = run_once(&request(
        DDL,
        serde_json::json!({"mode": "shard", "shard_count": "3", "ta_code": ""}),
    ))
    .expect("shard mode");
    assert!(output.contains("CREATE TABLE `fund_trans1`"), "{output}");
    assert!(output.contains("CREATE TABLE `fund_trans3`"), "{output}");
    assert_eq!(output.matches("CREATE TABLE").count(), 3, "{output}");
}

#[test]
fn ta_code_is_applied_when_generating() {
    let output = run_once(&request(
        DDL,
        serde_json::json!({"mode": "shard", "shard_count": "2", "ta_code": "Y3"}),
    ))
    .expect("ta shard mode");
    assert!(output.contains("fund_transY31"), "{output}");
    assert!(output.contains("fund_transY32"), "{output}");
}

/// 含 `${num}` 时按数量循环替换（源仓 `generateSharded` 的第一条分支）。
#[test]
fn num_placeholder_expands_over_the_shard_count() {
    let template = "CREATE TABLE t_${NUM} (id int);";
    let output = run_once(&request(
        template,
        serde_json::json!({"mode": "shard", "shard_count": "2", "ta_code": "Y3"}),
    ))
    .expect("placeholder mode");
    assert!(output.contains("CREATE TABLE t_1"), "{output}");
    assert!(output.contains("CREATE TABLE t_2"), "{output}");
}

#[test]
fn view_mode_emits_a_union_view() {
    let output = run_once(&request(
        DDL,
        serde_json::json!({"mode": "view", "shard_count": "2", "with_seq": "true"}),
    ))
    .expect("view mode");
    assert!(
        output.contains("CREATE VIEW view_fund_trans AS"),
        "{output}"
    );
    assert!(output.contains("1 AS table_seq"), "{output}");
}

/// 没有 CREATE TABLE 时 VIEW 无列可生成：必须走 `error` 事件，而不是返回空串。
#[test]
fn view_mode_without_a_create_table_fails_loudly() {
    let error = run_once(&request(
        "SELECT 1;",
        serde_json::json!({"mode": "view", "shard_count": "2"}),
    ))
    .expect_err("must fail");
    assert!(error.contains("CREATE TABLE"), "{error}");
}

#[test]
fn unknown_tool_is_reported_not_silently_ignored() {
    let mut bad = request("x", serde_json::json!({}));
    bad.tool = "duck.sql.nope".into();
    let error = run_once(&bad).expect_err("must fail");
    assert!(error.contains("unknown tool"), "{error}");
}

/// 检查器的请求：工具 id 与 DDL 生成不同，其余字段同一套协议。
fn check_request(input: &str, db_type: &str, lang: &str) -> RunRequest {
    RunRequest {
        tool: "duck.sql.script-check".into(),
        input: input.into(),
        params: serde_json::json!({ "db_type": db_type }),
        files: vec![],
        data: String::new(),
        lang: lang.into(),
        kv: Default::default(),
        // CLI 直跑一次正常执行，不走页内扫描。
        action: None,
    }
}

/// 无 WHERE 的 DELETE 必须报出来：这是 28 条规则里最不能漏的一条
/// （参考仓把它放在 `allow_unconditional_delete = false` 的默认档）。
#[test]
fn script_check_flags_an_unconditional_delete() {
    let output =
        run_once(&check_request("DELETE FROM users;", "mysql", "zh-Hans")).expect("check runs");
    assert!(output.contains("无条件DELETE"), "{output}");
    assert!(output.contains("D01"), "{output}");
    assert!(output.contains("错误"), "{output}");
}

/// 干净脚本给「通过」而不是空串——执行页要靠这句话区分「没问题」和「没跑」。
///
/// 用 MySQL 而不是 Oracle：Oracle 档下**任何**脚本都会带一条 E01「未使用
/// WHENEVER SQLERROR」的信息级提示（参考仓的规则就长这样），拿不到空报告。
#[test]
fn script_check_reports_a_clean_script() {
    let output = run_once(&check_request(
        "SELECT id FROM users WHERE id = 1;",
        "mysql",
        "zh-Hans",
    ))
    .expect("check runs");
    assert!(output.contains("SQL 检查通过"), "{output}");
}

/// 空输入走 `error` 事件（与 DDL 生成同一口径），不返回一张空报告。
#[test]
fn script_check_rejects_empty_input() {
    let error = run_once(&check_request("   \n", "mysql", "zh-Hans")).expect_err("must fail");
    assert!(error.contains("请粘贴"), "{error}");
}

/// 英文界面这一条覆盖的是**模板反查**：`sql_checker` 产出的是插值后的中文
/// （`DELETE没有LIMIT限制，可能超过10000条`），键却是模板
/// （`…可能超过{}条`），只有真读一遍插件译文表才能证明参数被搬对了。
#[test]
fn script_check_translates_interpolated_messages_for_the_english_locale() {
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sql-script-check");
    let bin = env!("CARGO_BIN_EXE_duck-sql-tool");
    let output = run(
        &plugin,
        Path::new(bin),
        &check_request("DELETE FROM users;", "mysql", "en"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .map(|outcome| outcome.output)
    .expect("check runs");

    // 静态串：精确查表。
    assert!(output.contains("Unconditional DELETE"), "{output}");
    // 插值串：模板反查后参数（10000）必须还在句子里。
    assert!(
        output.contains("may affect more than 10000 rows"),
        "{output}"
    );
    // 级别标签与汇总行也走插件表，不该漏成中文。
    assert!(!output.contains("错误"), "{output}");
    assert!(!output.contains("无条件"), "{output}");
}

// ── SQL 合并（sql-merge）────────────────────────────────────────────────

fn merge_request(src_dir: &str, lang: &str) -> RunRequest {
    RunRequest {
        tool: "duck.sql.sql-merge".into(),
        input: String::new(),
        params: serde_json::json!({ "src_dir": src_dir }),
        // `[io] allow_files = "read_write"` ⇒ 执行页会把路径参数一并放进 files。
        files: vec![src_dir.to_owned()],
        data: String::new(),
        lang: lang.into(),
        kv: Default::default(),
        // CLI 直跑一次正常执行，不走页内扫描。
        action: None,
    }
}

/// 造一个「两个版本包」的目录树（标准模式：每个包下必须有 `database/`）。
fn merge_fixture(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("duck-sql-merge-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    for (pkg, version, table) in [
        ("pkg_a", "IFMS6.0V202607.02.001", "t1"),
        ("pkg_b", "IFMS6.0V202607.02.002", "t2"),
    ] {
        let dir = root.join(pkg).join("database/spsql/pub").join(version);
        std::fs::create_dir_all(&dir).expect("build version dir");
        std::fs::write(
            dir.join(format!("{table}.sql")),
            format!("create table {table} (id int);\ncommit;\n"),
        )
        .expect("write sql");
    }
    root
}

/// 合并产物必须真落盘：报告说「输出到 X」，X 下的 `.sql` 就得同时含两个包的语句。
#[test]
fn sql_merge_writes_one_merged_file_per_group() {
    let root = merge_fixture("group");
    let output = run_once(&merge_request(&root.to_string_lossy(), "zh-Hans")).expect("merge runs");
    assert!(output.contains("合并完成"), "{output}");
    assert!(output.contains("2 个源文件"), "{output}");

    let output_dir = std::fs::read_dir(&root)
        .expect("read root")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("merge_sql_"))
        })
        .expect("output directory");
    assert!(
        output.contains(&output_dir.to_string_lossy().to_string()),
        "{output}"
    );
    // 源仓的产物约定：分组成员目录 + 合并文件 + merge.log。
    assert!(output_dir.join("merge.log").is_file(), "merge.log missing");
    let merged = output_dir.join("spsql/pub");
    let file = std::fs::read_dir(&merged)
        .expect("read group dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .expect("merged sql");
    let body = std::fs::read_to_string(&file).expect("read merged sql");
    assert!(body.contains("create table t1"), "{body}");
    assert!(body.contains("create table t2"), "{body}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 源目录不存在：走 `error` 事件，不返回一张空报告。
#[test]
fn sql_merge_rejects_a_missing_source_directory() {
    let error = run_once(&merge_request(
        "/tmp/duck-sql-merge-does-not-exist",
        "zh-Hans",
    ))
    .expect_err("must fail");
    assert!(error.contains("源目录不存在"), "{error}");
}

/// 没选目录（参数为空）：由插件自己拦，报的是「请选择」而不是 core 的「不能为空」。
#[test]
fn sql_merge_rejects_an_empty_source_directory() {
    let error = run_once(&merge_request("", "zh-Hans")).expect_err("must fail");
    assert!(error.contains("请选择要合并的源目录"), "{error}");
}

/// 英文界面：结果拼版 + 阶段标签 + 日志（含 `{:?}` 与 `{:.1}` 两种占位符）都要翻。
#[test]
fn sql_merge_translates_the_whole_report_for_the_english_locale() {
    let root = merge_fixture("english");
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sql-merge");
    let bin = env!("CARGO_BIN_EXE_duck-sql-tool");
    let output = run(
        &plugin,
        Path::new(bin),
        &merge_request(&root.to_string_lossy(), "en"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .map(|outcome| outcome.output)
    .expect("merge runs");

    assert!(output.contains("Merge finished: 1 groups"), "{output}");
    assert!(output.contains("Process log"), "{output}");
    // `{:?}`（包列表）与 `{:.1}`（耗时）两条模板都得反查成功。
    assert!(
        output.contains("Scanned 2 packages: [\"pkg_a\", \"pkg_b\"]"),
        "{output}"
    );
    assert!(output.contains("group done: spsql/pub"), "{output}");
    // 明细表与校验行也走插件表：漏一条，英文界面就会冒中文。
    assert!(
        output.contains("| Source package | Source file | Merged into | Statements | Size |"),
        "{output}"
    );
    assert!(
        output.contains("Statement check: passed (5 statements after merging)"),
        "{output}"
    );
    assert!(output.contains("| pkg_a | `spsql/pub/"), "{output}");
    // 报告正文不该残留中文（组名与路径都是 ASCII）。
    assert!(!output.contains("合并完成"), "{output}");
    assert!(!output.contains("处理日志"), "{output}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 报告要落到**逐源文件**那一层：只看每组总数，核对时还得回目录里翻
/// （参考页右栏那张五列表）。
#[test]
fn sql_merge_report_lists_every_source_file() {
    let root = merge_fixture("detail");
    let output = run_once(&merge_request(&root.to_string_lossy(), "zh-Hans")).expect("merge runs");

    // 两档校验都明说：源仓只在失败时给黄标，读起来像「根本没核对」。
    assert!(output.contains("条数校验：通过（合并后 5 条）"), "{output}");
    assert!(
        output.contains("| 来源包 | 源文件 | 合并到 | SQL 条数 | 大小 |"),
        "{output}"
    );
    // 两行明细：包名 / 源文件名 / 目标文件 / 条数 / 大小。
    assert!(output.contains("| pkg_a | `spsql/pub/"), "{output}");
    assert!(output.contains("| pkg_b | `spsql/pub/"), "{output}");
    assert!(output.contains("t1.sql"), "{output}");
    assert!(output.contains("t2.sql"), "{output}");
    assert!(output.contains("| 2 |"), "{output}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 「只看使用说明」：参考页那个帮助弹窗的等价出口，**不需要**源目录
/// （所以 `src_dir` 不能再声明 `required`，否则宿主先把请求挡在页面里）。
#[test]
fn sql_merge_guide_runs_without_a_source_directory() {
    let mut request = merge_request("", "zh-Hans");
    // 空 src_dir 会被拼成 `files: [""]`，这里干脆给空授权表。
    request.files.clear();
    request.params = serde_json::json!({ "guide": "true" });
    let output = run_once(&request).expect("guide runs");

    assert!(output.contains("SQL 文件合并 · 使用说明"), "{output}");
    assert!(output.contains("三种目录结构"), "{output}");
    assert!(output.contains("db-*.zip"), "{output}");
    assert!(output.contains("各源文件条数之和"), "{output}");
    // 说明模式不读目录、不写盘，也就不该出现执行结果。
    assert!(!output.contains("合并完成"), "{output}");
}

// ── SQL 分类抽取（duck.sql.sql-classifier）────────────────────────────────

fn classify_request(root_dir: &str, output_dir: &str, lang: &str) -> RunRequest {
    RunRequest {
        tool: "duck.sql.sql-classifier".into(),
        input: String::new(),
        params: serde_json::json!({ "root_dir": root_dir, "output_dir": output_dir }),
        files: vec![root_dir.to_owned()],
        data: String::new(),
        lang: lang.into(),
        kv: Default::default(),
        // CLI 直跑一次正常执行，不走页内扫描。
        action: None,
    }
}

/// 造一个含 `pub` / `trans` 的版本包目录：`pub` 放建表与改表，`trans` 放数据。
fn classify_fixture(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("duck-sql-classify-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    let pub_dir = root.join("pub");
    let trans_dir = root.join("trans");
    std::fs::create_dir_all(&pub_dir).expect("build pub");
    std::fs::create_dir_all(&trans_dir).expect("build trans");
    std::fs::write(
        pub_dir.join("a01_ddl.sql"),
        "create table t_user (id int);\n\
         create unique index idx_user on t_user (id);\n\
         alter table t_user add column age int;\n\
         commit;\n",
    )
    .expect("write ddl");
    std::fs::write(
        trans_dir.join("b02_dml.sql"),
        "insert into t_user (id) values (1);\n\
         -- add by lisi 2026-10-18 beg\n\
         update t_user set age = 20;\n\
         -- add by lisi 2026-10-18 end\n\
         select 1 from dual;\n",
    )
    .expect("write dml");
    root
}

/// 六个输出文件按源页面的名字落盘，且每类只收自己该收的语句。
#[test]
fn sql_classifier_writes_the_six_output_files() {
    let root = classify_fixture("files");
    let output = run_once(&classify_request(&root.to_string_lossy(), "", "zh-Hans"))
        .expect("classifier runs");
    assert!(output.contains("分类完成：2 个目录 · 2 个文件"), "{output}");
    assert!(output.contains("未分类语句 1 条"), "{output}");

    let exec = root.join("exec");
    let create = std::fs::read_to_string(exec.join("table_create_ddl.sql")).expect("create file");
    assert!(create.contains("create table t_user"), "{create}");

    let column = std::fs::read_to_string(exec.join("table_column_ddl.sql")).expect("column file");
    assert!(column.contains("create unique index idx_user"), "{column}");
    assert!(
        column.contains("alter table t_user add column age"),
        "{column}"
    );

    let dml = std::fs::read_to_string(exec.join("data_init_dml.sql")).expect("dml file");
    assert!(dml.contains("insert into t_user"), "{dml}");
    // 带标记的块要原样带上 `-- add by … beg/end`，否则回贴发布脚本时丢了归属。
    assert!(dml.contains("-- add by lisi 2026-10-18 beg"), "{dml}");
    assert!(dml.contains("update t_user set age = 20;"), "{dml}");
    assert!(
        !dml.contains("select 1 from dual"),
        "未分类语句不该进任何文件：{dml}"
    );

    let backup = std::fs::read_to_string(exec.join("backup_table.sql")).expect("backup file");
    assert!(backup.contains("as select * from t_user;"), "{backup}");

    let rollback_ddl =
        std::fs::read_to_string(exec.join("rollback_ddl.sql")).expect("rollback ddl");
    assert!(
        rollback_ddl.contains("drop table t_user;"),
        "{rollback_ddl}"
    );
    // 加列的回滚必须是「删列」而不是「删表」（见 `sql_classifier` 模块头的分歧说明）。
    assert!(
        rollback_ddl.contains("alter table t_user drop column age;"),
        "{rollback_ddl}"
    );

    let rollback_data =
        std::fs::read_to_string(exec.join("rollback_data.sql")).expect("rollback data");
    assert!(
        rollback_data.contains("delete from t_user;"),
        "{rollback_data}"
    );
    assert!(
        rollback_data.contains("insert into t_user select * from t_user_bak_"),
        "{rollback_data}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `output_dir` 给了就写那儿（执行页的「输出目录」参数），不给才默认 `<源>/exec`。
#[test]
fn sql_classifier_honours_an_explicit_output_directory() {
    let root = classify_fixture("outdir");
    let target = root.join("dist");
    let output = run_once(&classify_request(
        &root.to_string_lossy(),
        &target.to_string_lossy(),
        "zh-Hans",
    ))
    .expect("classifier runs");
    assert!(target.join("table_create_ddl.sql").is_file(), "{output}");
    assert!(!root.join("exec").exists(), "默认目录不该被创建");
    let _ = std::fs::remove_dir_all(&root);
}

/// 英文界面：统计行与「未分类」提示都要翻，输出**文件名**保持 ASCII 不变。
#[test]
fn sql_classifier_translates_the_report_for_the_english_locale() {
    let root = classify_fixture("english");
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sql-classifier");
    let bin = env!("CARGO_BIN_EXE_duck-sql-tool");
    let output = run(
        &plugin,
        Path::new(bin),
        &classify_request(&root.to_string_lossy(), "", "en"),
        &mut NullSink,
        &CancelHandle::new(),
    )
    .map(|outcome| outcome.output)
    .expect("classifier runs");

    assert!(
        output.contains("Classified 2 directories, 2 files"),
        "{output}"
    );
    assert!(output.contains("Unclassified statements: 1"), "{output}");
    assert!(
        output.contains("table_create_ddl.sql (2 lines)"),
        "{output}"
    );
    assert!(!output.contains("分类完成"), "{output}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 目录里一个 SQL 都没有：报错，而不是给一份「0 个文件」的假成功。
#[test]
fn sql_classifier_rejects_a_directory_without_sql_files() {
    let root = std::env::temp_dir().join("duck-sql-classify-empty");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("make empty dir");
    let error =
        run_once(&classify_request(&root.to_string_lossy(), "", "zh-Hans")).expect_err("must fail");
    assert!(error.contains("没有找到可读取的 SQL 文件"), "{error}");
    let _ = std::fs::remove_dir_all(&root);
}
