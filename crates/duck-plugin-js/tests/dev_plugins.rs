//! 仓库根 `plugins/` 插件的端到端守卫 + **能力授予**的端到端契约。
//!
//! dev 构建（`sbin/run.sh`）直接加载仓库 `plugins/` 目录（见
//! `duck-app/src/plugins.rs`），本测试确保里面的插件始终满足宿主契约：
//! 可发现、清单有效、JS 轨可执行——fixture 腐化会在 `cargo test` 即刻暴露。
//!
//! **目录粒度**：一工具一插件（`plugins/text-*` / `plugins/codec-*`，工具 id = 插件 id）。
//! 所以这里的「仓库插件」是复数，且每个插件只声明一个工具、不申请任何能力——文本与
//! 编解码类全是纯函数。这条也顺带把「插件边界 = 安装 / 授权 / 译文 / 数据目录边界」钉住。
//!
//! 后半段（`granted` 那几条）钉的是 P2-8 的语义：**声明 ∩ 授予**才是生效授权。
//! 只断言「允了能跑」不够——那只证明「能跑」，不证明闸门接上了；所以每条都配一个
//! 「不给就真的跑不动」的反向断言，并且区分到**具体哪一项能力**。
//! 闸门测试用 `tests/fixtures/sample`（先写文件、再发通知，两步各挂一项能力）。

use std::path::{Path, PathBuf};

use duck_plugin::{Capabilities, ExecutionModel, PluginIndex, PluginManifest};

fn repo_plugins_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins")
}

/// 仓库自带的全部插件，按族记账（一工具一插件，所以「几个插件」=「几个工具」）：
///
/// | 族 | 数 | 目录 |
/// |---|---|---|
/// | 文本 | 10 | `text-case-convert` / `text-cheat-sheets` / `text-cron-gen` /
///   `text-data-format` / `text-diff` / `text-formatter` / `text-number-to-chinese` /
///   `text-random-gen` / `text-regex-plus` / `text-timestamp` |
/// | 编解码 | 2 | `codec-crypto-bundle` / `codec-encoding-converter` |
/// | SQL | 5 | `sql-classifier` / `sql-ddl-generator` / `sql-merge` / `sql-rewrite` /
///   `sql-script-check` |
/// | 上线包合并 | 7 | `merge-manifest-extract` / `pkg-merge` / `non-sp-merge` /
///   `ifms6-merge` / `non-see-merge` / `scc-merge` / `see-merge` |
/// | 比对 | 4 | `text-diff` 之外的 `class-compare` / `dir-compare` /
///   `config-compare` / `jar-compare`（`text-diff` 已计入文本族，别再算一遍） |
/// | 文件 | 5 | `files-batch-edit` / `files-csv-tools` / `files-doc-convert` /
///   `files-search` / `files-tree` |
/// | 网络 | 5 | `net-hosts-manager` / `net-http-request` / `net-packet-send` /
///   `net-s3-ftp-checker` / `net-s3-manager` |
/// | 杂项 | 3 | `misc-git-tools` / `misc-json-schema` / `misc-nginx` |
/// | 金融 | 2 | `finance-pdm-reader` / `finance-channel-gateway` |
///
/// 合计 **43**。⚠️ 加插件必改这里的数字：它挡的是「清单写坏了 ⇒ 某个工具**静默消失**」，
/// 而静默消失正是这条测试唯一能抓到的东西。
fn repo_plugins() -> PluginIndex {
    let index = PluginIndex::discover(&repo_plugins_dir());
    assert_eq!(
        index.loaded_count(),
        43,
        "repo plugins/ must contain one plugin per tool"
    );
    index
}

/// 取某个工具所属插件的目录与清单（一工具一插件 ⇒ 工具 id 就是插件 id）。
fn repo_plugin_for(tool_id: &str) -> (PathBuf, PluginManifest) {
    let index = repo_plugins();
    let plugin = index
        .plugin_for_tool(tool_id)
        .unwrap_or_else(|| panic!("{tool_id} must be discoverable under plugins/"));
    let manifest = plugin
        .manifest
        .as_ref()
        .expect("repo manifest must be valid")
        .clone();
    (plugin.dir.clone(), manifest)
}

/// 能力闸门测试用的夹具：`fixtures/sample`（声明 写 report.txt + 通知）。
fn gate_plugin() -> (PathBuf, PluginManifest) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample");
    let source = std::fs::read_to_string(dir.join("plugin.toml")).expect("read fixture manifest");
    let manifest = PluginManifest::parse(&source).expect("fixture manifest must be valid");
    (dir, manifest)
}

/// 一次性的临时数据目录：**每次从空目录开始**，否则上一条测试留下的
/// `report.txt` 会让「文件不该存在」的断言假通过。
fn fresh_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-plugin-js-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp data dir");
    dir
}

/// 目录粒度契约：一工具一插件。
///
/// 每个插件恰好声明一个工具、工具 id 复用插件 id、必须带 `[tools.io]`
/// （否则通用执行页无从渲染）、且**不申请任何能力**——文本类插件是纯函数，
/// 不读文件、不出网、不发通知。任何一条破掉，说明有人把多个工具塞回了一个目录。
#[test]
fn every_repo_plugin_is_a_single_pure_tool() {
    let index = repo_plugins();
    for plugin in index.plugins() {
        let manifest = plugin
            .manifest
            .as_ref()
            .expect("repo manifest must be valid");
        assert_eq!(
            manifest.tools.len(),
            1,
            "{} must declare exactly one tool (one plugin per tool)",
            manifest.id
        );
        assert_eq!(
            manifest.tools[0].id, manifest.id,
            "single-tool plugin reuses the plugin id as the tool id"
        );
        assert!(
            manifest.tools[0].io.is_some(),
            "{} must declare [tools.io] for the generic run page",
            manifest.id
        );
        assert_eq!(
            manifest.capabilities,
            Capabilities::default(),
            "{} is a pure text tool: it must not request capabilities",
            manifest.id
        );
    }
}

/// 仓库里每个工具逐个真机跑一遍：**随仓库分发的每个插件都必须能出结果**。
///
/// 一工具一插件之后，一个 `main.js` 写坏只影响它自己——也就更容易没人发现。
/// 这条用「每个工具的默认参数 + 一份最小输入」扫一遍，输出非空才算过。
///
/// 断言用**集合相等**而不是「表长 = 插件数」：少写一条会点名哪个工具没覆盖，
/// 多写一条不存在的 id 会点名哪个用例已过期，两者都不需要再维护一个数字。
#[test]
fn every_repo_tool_produces_output_with_its_defaults() {
    // (工具 id, 输入, 参数 JSON)——参数留空即用清单里的默认值。
    //
    // ⚠️ 只能放 **JS 轨** 工具。`duck.text.data-format` / `duck.text.random-gen` /
    // `duck.codec.crypto-bundle` / `duck.text.cheat-sheets` 已迁到 process 轨（前端算法
    // 落进了 `duck-files-core` / `duck-codec-core` / `duck-misc-core`），真机验证归各自
    // 的 CLI 测试，
    // 混进这张表会让 `covered - discovered` 三条一起报「工具不存在」。
    let cases: [(&str, &str, &str); 8] = [
        ("duck.text.formatter", r#"{"a":1}"#, ""),
        ("duck.text.case-convert", "MyVar", ""),
        ("duck.text.timestamp", "", r#"{"mode":"now"}"#),
        ("duck.text.cron-gen", "", ""),
        ("duck.text.number-to-chinese", "123", ""),
        ("duck.text.regex-plus", "a1b2", r#"{"pattern":"\\d"}"#),
        (
            "duck.codec.encoding-converter",
            "中文",
            r#"{"direction":"encode","encoding":"unicode"}"#,
        ),
        (
            "duck.sql.sql-rewrite",
            "SQL执行: select * from t where id = ? 参数: 1",
            "",
        ),
    ];

    let index = repo_plugins();
    let covered: std::collections::HashSet<&str> = cases.iter().map(|(id, ..)| *id).collect();
    // 只有 JS 轨能在本 crate 里跑（process 轨要真起进程，归 `duck-sql-tool/tests/cli.rs`）。
    let discovered: std::collections::HashSet<&str> = index
        .plugins()
        .iter()
        .filter_map(|plugin| plugin.manifest.as_ref())
        .filter(|manifest| manifest.execution_model == ExecutionModel::Js)
        .flat_map(|manifest| manifest.tools.iter().map(|tool| tool.id.as_str()))
        .collect();
    assert_eq!(
        covered.difference(&discovered).collect::<Vec<_>>(),
        Vec::<&&str>::new(),
        "用例表里有插件目录中不存在的工具"
    );
    assert_eq!(
        discovered.difference(&covered).collect::<Vec<_>>(),
        Vec::<&&str>::new(),
        "新增插件必须在上面的用例表里补一条（否则它永远没人跑）"
    );

    for (tool_id, input, params) in cases {
        let (dir, manifest) = repo_plugin_for(tool_id);
        let data_dir = fresh_data_dir(tool_id);
        let outcome = duck_plugin_js::run_tool(
            &dir,
            &data_dir,
            &manifest,
            &manifest.capabilities.clone(),
            tool_id,
            input,
            params,
            "zh-Hans",
        )
        .unwrap_or_else(|error| panic!("{tool_id} failed: {error}"));
        assert!(
            !outcome.output.trim().is_empty(),
            "{tool_id} produced empty output"
        );
    }
}

/// process 轨插件不能混进上面那张表：它的可执行文件由 `sbin/plugins.sh` 投放，
/// 真机验证在 `crates/duck-sql-tool/tests/cli.rs`（起真进程走 stdio）。
/// 这里只钉一件事——声明了 process 就必须有形参齐全的 `[process] bin`。
#[test]
fn process_plugins_declare_their_entry_point() {
    let index = repo_plugins();
    let process: Vec<_> = index
        .plugins()
        .iter()
        .filter_map(|plugin| plugin.manifest.as_ref())
        .filter(|manifest| manifest.execution_model == ExecutionModel::Process)
        .collect();
    assert!(
        process.len() >= 4,
        "process 轨真机工具（ddl-generator / script-check / sql-merge / sql-classifier）都要在"
    );
    for manifest in process {
        let config = manifest
            .process
            .as_ref()
            .unwrap_or_else(|| panic!("{} must declare [process]", manifest.id));
        assert!(
            !config.bin.trim().is_empty() && !config.bin.starts_with('/'),
            "{} 的 [process] bin 必须是插件目录内的相对路径",
            manifest.id
        );
    }
}

// 加密算法（SHA-1/256/384/512、AES-CBC、恒生 LCPT）已随 `codec-crypto-bundle` 一起
// 迁到 process 轨，算法本体在 `duck-codec-core`，这里不再有 JS 沙箱可跑。
//
// ⚠️ **已知向量（`SHA-1("abc")` = `a9993e36…`、`SHA-256("abc")` = `ba7816bf…`、
// AES-CBC 加解密往返）的覆盖随那次迁移一起丢了**：当年这条测试存在的原因是
// 「Node 侧对了不代表 QuickJS 侧也对」（BigInt 语义、位运算截断），而那个理由
// 随沙箱一起消失；但「换实现后向量仍然对」这个理由不消失。补法是在
// `crates/duck-codec-tool/tests/cli.rs` 起真进程重钉同样的四条摘要 + 一次往返，
// 归 `codec-*` 那一族做（本 crate 只测 JS 轨，硬塞进来还要复制一份进程协议）。

/// `duck.sql.sql-rewrite` 是**纯前端算法的移植**（参考仓没有 Rust 后端），
/// 所以行为锁只能靠这里的用例：四套模式各跑一遍，断言的是源页面里那些
/// 「不写出来就会悄悄坏掉」的细节。
#[test]
fn sql_rewrite_plugin_covers_all_four_modes() {
    let (dir, manifest) = repo_plugin_for("duck.sql.sql-rewrite");
    let data_dir = fresh_data_dir("sql-rewrite");
    let run = |input: &str, params: &str| {
        duck_plugin_js::run_tool(
            &dir,
            &data_dir,
            &manifest,
            &manifest.capabilities.clone(),
            "duck.sql.sql-rewrite",
            input,
            params,
            "zh-Hans",
        )
        .map(|outcome| outcome.output)
    };

    // ① 参数替换：日志行里的 `?` 按位置换成字面量，数字不加引号、字符串加；
    //    行首的 `SQL:` / `当前SQL:` 前缀要去掉。注意源页面的前缀正则只认
    //    `sql|SQL|当前SQL` 三种写法（`SQL执行:` 不在其中），这里照抄不扩大。
    let filled = run(
        "SQL: select * from t_user where id = ? and name = ? 参数: 1, 张伟",
        r#"{"mode":"param"}"#,
    )
    .expect("param mode");
    assert_eq!(
        filled,
        "select * from t_user where id = 1 and name = '张伟';"
    );

    // ② Schema 前缀：只给 FROM / INTO 后面那个表名加，已经带前缀的不重复加。
    let prefixed = run(
        "SQL: select * from t_user where id = ? 参数: 7",
        r#"{"mode":"param","schema":"app"}"#,
    )
    .expect("param mode with schema");
    assert_eq!(prefixed, "select * from app.t_user where id = 7;");

    // ③ 可重复执行：给 INSERT 补一条 `select *` 注释（已知表走业务主键），
    //    语句本身原样保留；`comment_delete` 打开时 DELETE 变注释。
    let reexec = run(
        "insert into TBPARAM (ta_code, param_id, param_value) values ('Y3', 'P001', 'x');",
        r#"{"mode":"reexec"}"#,
    )
    .expect("reexec mode");
    assert!(
        reexec.contains("/* select * from TBPARAM where ta_code='Y3' and param_id='P001'; */"),
        "已知表要用业务主键生成查询：{reexec}"
    );
    assert!(reexec.contains("insert into TBPARAM"), "{reexec}");

    // ④ 纯净版：行注释、块注释、行尾注释全去，语句之间留一个空行。
    let clean = run(
        "-- 头部说明\n/* 块注释\n   还没结束 */\nselect 1 from dual; -- 行尾\n\nselect 2 from dual;\n",
        r#"{"mode":"clean"}"#,
    )
    .expect("clean mode");
    assert_eq!(clean, "select 1 from dual;\n\nselect 2 from dual;");

    // ⑤ 备份：DDL 里的表也要进备份清单（`ALTER TABLE` 命中），且 DB2 用
    //    `AS (...) WITH DATA` 写法——这条差异只看代码是容易漏的。
    let backup = run(
        "update t_user set name = 'x' where id = 1;\nalter table t_role add column c1 int;",
        r#"{"mode":"backup","db_type":"db2"}"#,
    )
    .expect("backup mode");
    assert!(backup.contains("CREATE TABLE t_user_"), "{backup}");
    assert!(
        backup.contains("AS (SELECT * FROM t_user) WITH DATA"),
        "DB2 的表复制要带 WITH DATA：{backup}"
    );
    assert!(
        backup.contains("DELETE FROM t_user;")
            && backup.contains("INSERT INTO t_user SELECT * FROM t_user_"),
        "非 DROP 的表回滚走 DELETE + INSERT：{backup}"
    );

    // ⑥ 没有任何可改数据的语句时，备份模式要给明确错误，而不是一段空注释头。
    let empty = run("select 1 from dual;", r#"{"mode":"backup"}"#);
    assert!(empty.is_err(), "空备份必须报错");
}

#[test]
fn repo_dev_plugin_discovers_and_executes() {
    let (dir, manifest) = repo_plugin_for("duck.text.case-convert");
    let data_dir = fresh_data_dir("dev-plugin-full");

    // 全给＝按声明全授（宿主在没有授予记录时的缺省）。
    let outcome = duck_plugin_js::run_tool(
        &dir,
        &data_dir,
        &manifest,
        &manifest.capabilities.clone(),
        "duck.text.case-convert",
        "MyVar",
        "",
        "zh-Hans",
    )
    .expect("repo tool must execute when every declared capability is granted");
    // 结果是**六种风格并列的 Markdown 表**（宿主 `kind = "table"` 会解析成真表格）。
    assert!(
        outcome.output.contains("| camelCase | myVar |"),
        "{}",
        outcome.output
    );
    assert!(
        outcome.notifications.is_empty(),
        "纯函数插件不该发通知：结果由执行页的结果区呈现"
    );
}

/// 清单 `[tools.io]` 参数区收集到的值经宿主序列化成 JSON 注入 `duck.params`。
///
/// 走真实仓库插件 `cron-gen`：显式给 `{preset: daily, hour: 7, minute: 15}`，
/// 说明里必须出现「第 15 分钟」「7 点」—— 证明参数**真的**从宿主流进了沙箱并参与
/// 运算，而不是插件侧的默认值在碰巧兜底（`case-convert` 现在没有参数了：六种风格
/// 一次全给，正好拿它做不成这件事）。
#[test]
fn declared_params_are_injected_into_the_sandbox() {
    let (dir, manifest) = repo_plugin_for("duck.text.cron-gen");
    let data_dir = fresh_data_dir("dev-plugin-params");

    let outcome = duck_plugin_js::run_tool(
        &dir,
        &data_dir,
        &manifest,
        &manifest.capabilities.clone(),
        "duck.text.cron-gen",
        "",
        r#"{"preset":"daily","hour":"7","minute":"15"}"#,
        "zh-Hans",
    )
    .expect("repo tool must execute with declared params");
    assert!(outcome.output.contains("第 15 分钟"), "{}", outcome.output);
    assert!(outcome.output.contains("7 点"), "{}", outcome.output);
    // 默认值是 9 点 / 0 分，参数没生效时这两句会自动「兜」出一个 9 点，钉住它。
    assert!(!outcome.output.contains("第 0 分钟"), "{}", outcome.output);
}

/// 一个都不给：夹具必须**跑不起来**，而且写不出任何文件。
///
/// 后半句才是重点——「返回 Err」也可能是别的原因（比如沙箱路径判断），
/// 文件不存在才能证明它连第一句 `duck.writeFile` 都没过去。
#[test]
fn nothing_granted_means_the_plugin_cannot_even_write_its_report() {
    let (dir, manifest) = gate_plugin();
    let data_dir = fresh_data_dir("dev-plugin-none");

    let result = duck_plugin_js::run_tool(
        &dir,
        &data_dir,
        &manifest,
        &Capabilities::default(),
        "dev.duck.sample.hash",
        "",
        "",
        "zh-Hans",
    );

    let error = result.expect_err("no capability granted: the tool must fail");
    let text = error.to_string();
    assert!(
        text.contains("not granted"),
        "失败原因必须是「能力未授予」，而不是别的意外：{text}"
    );
    assert!(
        !data_dir.join("report.txt").exists(),
        "被拒之后不该留下任何产出"
    );
}

/// 只给「写」不给「通知」：文件写得出来，但后面 `duck.notify` 那步被拒。
///
/// 这条专门盯 [`duck.notify`] 的闸门——它原先**完全没走仲裁**（只往一个
/// `Vec` 里 push），于是确认框上取消勾选「通知」是个摆设。
#[test]
fn granting_only_the_write_leaves_the_notification_denied() {
    let (dir, manifest) = gate_plugin();
    let data_dir = fresh_data_dir("dev-plugin-write-only");
    let granted = Capabilities {
        filesystem_write: manifest.capabilities.filesystem_write.clone(),
        ..Capabilities::default()
    };

    let result = duck_plugin_js::run_tool(
        &dir,
        &data_dir,
        &manifest,
        &granted,
        "dev.duck.sample.hash",
        "",
        "",
        "zh-Hans",
    );

    let error = result.expect_err("notifications were not granted: the tool must fail");
    assert!(
        error.to_string().contains("Notifications"),
        "这一半必须卡在通知上：{error}"
    );
    assert!(
        data_dir.join("report.txt").is_file(),
        "写权限给了就该写得出来——否则证明的不是闸门而是整条链路坏了"
    );
}

/// 只给「通知」不给「写」：连文件都写不出来（与上一条互为镜像）。
#[test]
fn granting_only_notifications_leaves_the_write_denied() {
    let (dir, manifest) = gate_plugin();
    let data_dir = fresh_data_dir("dev-plugin-notify-only");
    let granted = Capabilities {
        notifications: true,
        ..Capabilities::default()
    };

    let result = duck_plugin_js::run_tool(
        &dir,
        &data_dir,
        &manifest,
        &granted,
        "dev.duck.sample.hash",
        "",
        "",
        "zh-Hans",
    );

    let error = result.expect_err("filesystem write was not granted: the tool must fail");
    assert!(
        error.to_string().contains("not granted"),
        "这一半必须卡在写权限上：{error}"
    );
    assert!(
        !data_dir.join("report.txt").exists(),
        "写权限没给，文件就不该出现"
    );
}

/// 授予里多出来的能力不会被执行到：`hash` 一下声明与授予后**再**求交，
/// 结果里只能有声明过的那些（仓库插件根本没声明网络与进程）。
#[test]
fn a_grant_cannot_smuggle_in_undeclared_capabilities() {
    let (_, manifest) = repo_plugin_for("duck.text.case-convert");
    let greedy = Capabilities {
        network_hosts: vec!["exfil.example.com".into()],
        process_exec: vec!["curl".into()],
        ..manifest.capabilities.clone()
    };
    let effective = manifest.capabilities.intersect(&greedy);
    assert_eq!(effective, manifest.capabilities);
    assert!(
        effective.network_hosts.is_empty() && effective.process_exec.is_empty(),
        "声明里没有的网络与进程能力不能被授予出来"
    );
}
