//! 金融工具族（`duck.finance.*`）的 **process 轨入口**。
//!
//! 与其余各族同一个骨架：一个二进制服务整族工具，按 `request.tool` 分派到
//! [`duck_finance_core`]。本文件只做三件事：
//!
//! 1. 把 `[io]` 参数摊平成内核入参（路径 / 动作 / 方言 / 列映射）；
//! 2. 把内核的产出排成**可复制的报告或代码**（预览类排成等宽表格，产物类原样输出）；
//! 3. 把内核的稳定码（`PdmError` / `GatewayError`）翻成当前语言的一句话。
//!
//! ## 为什么「导出」类动作不带抬头
//!
//! 建表 SQL / 数据字典 Markdown / 四种 HTML 都是**拿去落盘**的东西：抬头里的
//! 「PDM · 路径 · 耗时」落进 `.sql` 就是语法错误（`PDM` 不是注释）。所以这类动作
//! 只把产物本身交给结果区，过程信息走 `log` 事件（进度条上方那行）。
//!
//! ## 两个工具共用一个二进制
//!
//! `allow_files` 都是 `read`：PDM 与接口 Excel 都是只读材料，本族不落盘。
//! 于是「导出」在本仓不是「写文件」，而是「把内容交给你复制」——与 nginx 族同口径。

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use duck_finance_core::channel_gateway::{
    BankMeta, Change, ChangeKind, ColMapping, Direction, FieldAttr, FieldDef, FieldEdit,
    FieldPatch, GatewayError, InterfaceDef, Side, Store, Template, XmlSamples, code,
    diff_interfaces, diff_one_interface, diff_versions, generate_message_template, parse_excel,
    preview_headers,
};
use duck_finance_core::pdm::{self, Labels, PdmError, PdmFileResult, PdmModel, Table};
use duck_plugin::{PluginLocales, TableDoc};
use duck_plugin_process::{PluginOutput, RunRequest, serve};

mod i18n;

/// 参数区（与其余各族同一套：宿主把表单值序列化成字符串键值对）。
struct Params(serde_json::Value);

impl Params {
    fn new(value: &serde_json::Value) -> Self {
        Self(value.clone())
    }

    fn raw(&self, key: &str) -> String {
        match self.0.get(key) {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(serde_json::Value::Number(number)) => number.to_string(),
            Some(serde_json::Value::Bool(flag)) => flag.to_string(),
            _ => String::new(),
        }
    }

    /// 枚举型参数：空串用 `fallback`。
    fn pick(&self, key: &str, fallback: &str) -> String {
        match self.raw(key).trim() {
            "" => fallback.to_owned(),
            other => other.to_owned(),
        }
    }

    /// 数值参数（`number` 控件可能给数字也可能给字符串）。
    fn integer(&self, key: &str) -> Option<i64> {
        match self.0.get(key) {
            Some(serde_json::Value::Number(number)) => number
                .as_i64()
                .or_else(|| number.as_f64().map(|v| v as i64)),
            Some(serde_json::Value::String(text)) => text.trim().parse().ok(),
            _ => None,
        }
    }

    /// 列号是**人读的 1 起**，内核要 0 起：`<= 0` 视为没填。
    fn column(&self, key: &str) -> Option<usize> {
        match self.integer(key) {
            Some(value) if value >= 1 => usize::try_from(value - 1).ok(),
            _ => None,
        }
    }
}

/// 取词器：`locales/` 由宿主通过环境变量指路，缺失时原样返回中文源串。
struct Text {
    locales: PluginLocales,
    lang: String,
}

impl Text {
    fn load() -> Self {
        let lang = std::env::var(duck_plugin::ENV_LOCALE).unwrap_or_else(|_| "zh-Hans".to_owned());
        // `DUCK_LOCALES_DIR` 已经是 `locales/` 这一级：用 `load` 会再拼一次目录。
        let locales = std::env::var(duck_plugin::ENV_LOCALES_DIR)
            .map(|dir| PluginLocales::load_locales_dir(std::path::Path::new(&dir), "zh-Hans"))
            .unwrap_or_default();
        Self { locales, lang }
    }

    fn get(&self, source: &str) -> String {
        self.locales.text(&self.lang, source).to_owned()
    }

    fn fill(&self, template: &str, args: &[&str]) -> String {
        i18n::fill(&self.get(template), args)
    }

    /// 分隔符也走译文表：中文全角冒号，英文 `": "`。
    fn separator(&self) -> String {
        self.get("：")
    }
}

fn main() -> ExitCode {
    serve(handle)
}

fn handle(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
) -> Result<String, duck_plugin::PluginError> {
    let text = Text::load();
    match request.tool.as_str() {
        "duck.finance.pdm-reader" => pdm_run(request, out, &text),
        "duck.finance.channel-gateway" => gateway_run(request, out, &text),
        other => Err(duck_plugin::PluginError::Execution(format!(
            "duck-finance-tool: unknown tool {other}"
        ))),
    }
}

fn execution(message: String) -> duck_plugin::PluginError {
    duck_plugin::PluginError::Execution(message)
}

/// 稳定码 → 一句话，细节跟在分隔符后面（细节是数据，不翻译）。
fn with_detail(text: &Text, base: String, detail: Option<String>) -> duck_plugin::PluginError {
    match detail {
        Some(detail) => execution(format!("{base}{}{detail}", text.separator())),
        None => execution(base),
    }
}

fn fail_pdm(text: &Text, error: PdmError) -> duck_plugin::PluginError {
    let base = text.get(match error.code() {
        "read-failed" => "读取 PDM 文件失败",
        "xml-invalid" => "PDM XML 解析错误",
        _ => "找不到这张表",
    });
    with_detail(text, base, Some(error.detail().to_string()))
}

fn fail_gateway(text: &Text, error: GatewayError) -> duck_plugin::PluginError {
    let base = text.get(match error.code() {
        "open-failed" => "打不开这份 Excel",
        "no-sheets" => "这份 Excel 里没有工作表",
        "sheet-unreadable" => "工作表读不出来",
        "bad-sheet-name" => "工作表名不是交易码，也没有给列映射",
        "no-interfaces" => "没解析到任何接口",
        "no-such-interface" => "找不到这个交易码",
        // 接口库 / 版本 / 模板（第三批）
        "store-read-failed" => "读不了接口库",
        "store-write-failed" => "写不进接口库",
        "store-corrupted" => "接口库文件坏了",
        "bad-bank-code" => "银行代码只能用字母、数字、下划线和连字符",
        "bad-template-name" => "模板名不能为空，也不能带斜杠",
        "no-such-bank" => "接口库里没有这家银行",
        "no-such-version" => "这个版本号不存在",
        "no-such-template" => "没有这个模板",
        "no-such-field" => "接口库里没有这个字段",
        _ => "模板渲染失败",
    });
    with_detail(text, base, error.detail())
}

// ── PDM 阅读器 ────────────────────────────────────────────────────────

fn pdm_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let action = params.pick("action", "list");
    let mode = params.pick("source", "file");
    let path = if mode == "dir" {
        params.raw("dir")
    } else {
        params.raw("file")
    };
    let path = path.trim().to_string();
    if path.is_empty() {
        return Err(execution(text.get("先选一个 PDM 文件或目录")));
    }
    let started = Instant::now();

    let mut notes: Vec<String> = Vec::new();
    // 目录模式下保留逐文件清单：参考页第二行是「文件标签栏」，合并之后只剩一份模型，
    // 「解析了哪几个文件、各有多少表」得靠这张表补回来。
    let mut per_file: Vec<PdmFileResult> = Vec::new();
    let model = if mode == "dir" {
        let scan = pdm::parse_directory(Path::new(&path), &text.get("合并数据字典"));
        if scan.files.is_empty() {
            return Err(execution(
                text.fill("目录里没有可解析的 .pdm / .xml 文件: {}", &[&path]),
            ));
        }
        out.log(text.fill("解析了 {} 个文件", &[&scan.files.len().to_string()]));
        if !scan.skipped.is_empty() {
            out.log(text.fill(
                "{} 个文件读不了，已跳过",
                &[&scan.skipped.len().to_string()],
            ));
            for (file, error) in &scan.skipped {
                notes.push(format!("  - {file}{}{}", text.separator(), error.detail()));
            }
        }
        per_file = scan.files;
        scan.combined
    } else {
        let model = pdm::read_file(Path::new(&path)).map_err(|error| fail_pdm(text, error))?;
        out.log(text.fill("解析了 {} 张表", &[&model.tables.len().to_string()]));
        model
    };

    // 「表」支持逗号/换行分隔多张（参考页搜索框的语义）；单词是筛、多词是各挑一张。
    let filter = params.raw("table").trim().to_string();
    let chosen: Vec<&Table> = pdm::select_tables(&model, &filter);

    match action.as_str() {
        "list" => Ok(pdm_list(text, &model, &path, started, &notes, &per_file)),
        "table" => {
            if filter.is_empty() {
                return Err(execution(text.get("在「表」里填表代码或名字的一部分")));
            }
            if chosen.is_empty() {
                return Err(fail_pdm(text, PdmError::TableNotFound(filter.clone())));
            }
            let mut report = pdm_heading(text, &path, started);
            for table in &chosen {
                report.push('\n');
                report.push_str(&pdm_table_detail(text, table));
            }
            if !notes.is_empty() {
                report.push('\n');
                report.push_str(&text.get("跳过的文件:"));
                report.push('\n');
                report.push_str(&notes.join("\n"));
                report.push('\n');
            }
            Ok(report)
        }
        "ddl" => {
            let dialect = params.pick("dialect", "oracle");
            if chosen.is_empty() {
                return Err(execution(text.get("没有匹配的表可以生成")));
            }
            let mut sql = String::new();
            for table in &chosen {
                sql.push_str(&pdm::sql::generate_create_table_sql(table, &dialect));
                sql.push('\n');
            }
            out.log(text.fill(
                "{} 张表的建表语句（{}）",
                &[&chosen.len().to_string(), &dialect],
            ));
            Ok(sql)
        }
        "comment" => {
            if chosen.is_empty() {
                return Err(execution(text.get("没有匹配的表可以生成")));
            }
            let tables: Vec<Table> = chosen.iter().map(|table| (*table).clone()).collect();
            out.log(text.fill("{} 张表的注释语句", &[&tables.len().to_string()]));
            Ok(pdm::sql::generate_comment_sql(&tables))
        }
        "insert" => {
            if chosen.is_empty() {
                return Err(execution(text.get("没有匹配的表可以生成")));
            }
            let labels = pdm_labels(text);
            let tables: Vec<Table> = chosen.iter().map(|table| (*table).clone()).collect();
            out.log(text.fill("{} 张表的插入语句", &[&tables.len().to_string()]));
            Ok(pdm::sql::generate_insert_sql(&tables, &labels))
        }
        "markdown" | "html" | "word" | "excel" | "pdf" => {
            if chosen.is_empty() {
                return Err(execution(text.get("没有匹配的表可以导出")));
            }
            let labels = pdm_labels(text);
            let subset = PdmModel {
                name: model.name.clone(),
                code: model.code.clone(),
                description: model.description.clone(),
                target: model.target.clone(),
                tables: chosen.iter().map(|table| (*table).clone()).collect(),
            };
            out.log(text.fill(
                "导出 {} 张表（{}）",
                &[&subset.tables.len().to_string(), &action],
            ));
            Ok(match action.as_str() {
                "markdown" => pdm::sql::generate_data_dictionary_md(&subset, &labels),
                "html" => pdm::export::generate_html(&subset, &labels),
                "word" => pdm::export::generate_word_html(&subset, &labels),
                "excel" => pdm::export::generate_excel_html(&subset, &labels),
                _ => pdm::export::generate_pdf_html(&subset, &labels),
            })
        }
        other => Err(execution(format!(
            "duck.finance.pdm-reader: unknown action {other}"
        ))),
    }
}

fn pdm_heading(text: &Text, path: &str, started: Instant) -> String {
    let mut line = text.fill(
        "{} · {} · 耗时 {} ms",
        &["PDM", path, &started.elapsed().as_millis().to_string()],
    );
    line.push('\n');
    line
}

/// 模型标题：`名称 (代码)`，缺哪半边就只显示另一半。
fn model_title(model: &PdmModel) -> String {
    match (model.name.trim(), model.code.trim()) {
        ("", "") => "-".to_string(),
        ("", code) => code.to_string(),
        (name, "") => name.to_string(),
        (name, code) => format!("{name} ({code})"),
    }
}

fn pdm_list(
    text: &Text,
    model: &PdmModel,
    path: &str,
    started: Instant,
    notes: &[String],
    per_file: &[PdmFileResult],
) -> String {
    let mut report = pdm_heading(text, path, started);
    report.push('\n');
    report.push_str(&format!(
        "{}{}{}\n",
        text.get("模型"),
        text.separator(),
        model_title(model)
    ));
    if !model.target.trim().is_empty() {
        report.push_str(&format!(
            "{}{}{}\n",
            text.get("目标"),
            text.separator(),
            model.target
        ));
    }
    let columns: usize = model.tables.iter().map(|table| table.columns.len()).sum();
    report.push_str(&format!(
        "{}{}{}    {}{}{}\n\n",
        text.get("表"),
        text.separator(),
        model.tables.len(),
        text.get("字段"),
        text.separator(),
        columns
    ));

    // 目录模式：先给逐文件清单（参考页的文件标签栏），再给合并后的表清单。
    if per_file.len() > 1 {
        let mut rows = vec![vec![text.get("文件"), text.get("表"), text.get("字段")]];
        for file in per_file {
            let columns: usize = file
                .model
                .tables
                .iter()
                .map(|table| table.columns.len())
                .sum();
            rows.push(vec![
                file.file_name.clone(),
                file.model.tables.len().to_string(),
                columns.to_string(),
            ]);
        }
        report.push_str(&render_table(&rows));
        report.push('\n');
    }

    if model.tables.is_empty() {
        report.push_str(&text.get("（没有表）"));
        report.push('\n');
    } else {
        let mut rows = vec![vec![
            text.get("表代码"),
            text.get("名称"),
            text.get("字段"),
            text.get("主键"),
            text.get("索引"),
        ]];
        for table in &model.tables {
            rows.push(vec![
                table.code.clone(),
                table.name.clone(),
                table.columns.len().to_string(),
                if table.primary_key.is_empty() {
                    "-".to_string()
                } else {
                    table.primary_key.join(",")
                },
                table.indexes.len().to_string(),
            ]);
        }
        report.push_str(&render_table(&rows));
    }

    if !notes.is_empty() {
        report.push('\n');
        report.push_str(&text.get("跳过的文件:"));
        report.push('\n');
        report.push_str(&notes.join("\n"));
        report.push('\n');
    }
    report
}

fn pdm_table_detail(text: &Text, table: &Table) -> String {
    let mut block = format!("## {} ({})\n", table.name, table.code);
    let pk = if table.primary_key.is_empty() {
        "-".to_string()
    } else {
        table.primary_key.join(", ")
    };
    block.push_str(&format!("{}{}{}\n", text.get("主键"), text.separator(), pk));
    if !table.comment.trim().is_empty() {
        block.push_str(&format!(
            "{}{}{}\n",
            text.get("注释"),
            text.separator(),
            table.comment
        ));
    }
    block.push('\n');

    let mut rows = vec![vec![
        text.get("名称"),
        text.get("代码"),
        text.get("类型"),
        text.get("长度"),
        text.get("精度"),
        text.get("非空"),
        text.get("默认值"),
        text.get("注释"),
    ]];
    for column in &table.columns {
        rows.push(vec![
            column.name.clone(),
            column.code.clone(),
            column.data_type.clone(),
            column
                .length
                .map(|value| value.to_string())
                .unwrap_or_default(),
            column
                .precision
                .map(|value| value.to_string())
                .unwrap_or_default(),
            if column.mandatory {
                text.get("是")
            } else {
                String::new()
            },
            column.default_value.clone(),
            column.comment.clone(),
        ]);
    }
    block.push_str(&render_table(&rows));

    if !table.indexes.is_empty() {
        block.push('\n');
        block.push_str(&format!("{}{}\n", text.get("索引"), text.separator()));
        for index in &table.indexes {
            block.push_str(&format!(
                "  - {} ({})\n",
                index.name,
                index.columns.join(", ")
            ));
        }
    }
    block
}

fn pdm_labels(text: &Text) -> Labels {
    Labels {
        title: text.get("数据字典"),
        generated_at: text.get("生成时间"),
        tables: text.get("表数量"),
        target: text.get("目标"),
        target_database: text.get("目标数据库"),
        field: text.get("字段名"),
        code: text.get("代码"),
        data_type: text.get("类型"),
        length: text.get("长度"),
        precision: text.get("精度"),
        mandatory: text.get("非空"),
        default_value: text.get("默认值"),
        comment: text.get("注释"),
        primary_key: text.get("主键"),
        index: text.get("索引"),
        columns: text.get("字段数"),
        footer: text.get("由 PDM 阅读器生成"),
        table: text.get("表"),
        todo_values: text.get("TODO: 添加值"),
    }
}

// ── 渠道网关 ──────────────────────────────────────────────────────────

// ── 接口库 / 版本 / 模板（有状态的那一半） ──────────────────────────────
//
// 参考仓这三块各占一个 Tab、各自弹窗表单；本仓把每个动作摊成一次运行。
// 数据落插件的**私有数据目录**（宿主经 `DUCK_DATA_DIR` 给），布局见
// `duck_finance_core::channel_gateway::store` 的文档头。

/// 取插件私有数据目录。
///
/// 拿不到就直接失败，**不退回到当前目录**：那会把接口库写进宿主的工作目录，
/// 换个启动位置就「银行全没了」，而用户完全看不出发生了什么。
fn open_store(text: &Text) -> Result<Store, duck_plugin::PluginError> {
    let root = std::env::var(duck_plugin::ENV_DATA_DIR).unwrap_or_default();
    if root.trim().is_empty() {
        return Err(execution(text.get("这个插件没有数据目录，存不了东西")));
    }
    Ok(Store::new(root))
}

/// 库类动作的抬头（没有 Excel 路径可写，只有耗时）。
fn library_heading(text: &Text, started: Instant) -> String {
    let mut line = text.fill(
        "{} · 耗时 {} ms",
        &[
            &text.get("渠道网关"),
            &started.elapsed().as_millis().to_string(),
        ],
    );
    line.push('\n');
    line
}

fn require_bank_code(params: &Params, text: &Text) -> Result<String, duck_plugin::PluginError> {
    let code = params.raw("bank_code").trim().to_string();
    if code.is_empty() {
        return Err(execution(text.get("先填银行代码")));
    }
    Ok(code)
}

/// 管理类动作的统一入口。
fn gateway_library(
    action: &str,
    params: &Params,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
    started: Instant,
) -> Result<String, duck_plugin::PluginError> {
    let store = open_store(text)?;
    match action {
        "banks" => {
            let banks = store.banks().map_err(|error| fail_gateway(text, error))?;
            out.log(text.fill("{} 家银行", &[&banks.len().to_string()]));
            let mut report = library_heading(text, started);
            report.push_str(&render_banks(text, &banks));
            Ok(report)
        }
        "bank-load" => {
            let code = require_bank_code(params, text)?;
            let bank = store
                .bank(&code)
                .map_err(|error| fail_gateway(text, error))?;
            let Some(bank) = bank else {
                return Err(fail_gateway(text, GatewayError::NoSuchBank(code)));
            };
            out.log(text.fill("{} 个接口", &[&bank.interfaces.len().to_string()]));
            // 读回是「字段级」：参考页的字段表就在这一屏逐行改类型/含义，所以
            // 表格与文本都按字段排——屏幕上是几行，复制出来就是几行。
            push_table(out, &interfaces_table(bank.interfaces.iter(), text, true));
            let mut report = library_heading(text, started);
            for iface in &bank.interfaces {
                report.push('\n');
                report.push_str(&gateway_fields(text, iface));
            }
            Ok(report)
        }
        "bank-delete" => {
            let code = require_bank_code(params, text)?;
            // 先读一遍只为把「删掉了什么」说清楚：删完就再也查不到了。
            let before = store
                .bank(&code)
                .map_err(|error| fail_gateway(text, error))?;
            let versions = store
                .versions(&code)
                .map_err(|error| fail_gateway(text, error))?;
            if !store
                .delete_bank(&code)
                .map_err(|error| fail_gateway(text, error))?
            {
                return Err(fail_gateway(text, GatewayError::NoSuchBank(code)));
            }
            let interfaces = before.map(|bank| bank.interfaces.len()).unwrap_or(0);
            out.log(text.fill("已删除银行 {}", &[&code]));
            let mut report = library_heading(text, started);
            report.push_str(&text.fill(
                "已删除 {}（{} 个接口，{} 个版本）",
                &[&code, &interfaces.to_string(), &versions.len().to_string()],
            ));
            report.push('\n');
            Ok(report)
        }
        "bank-save" => {
            let path = params.raw("path").trim().to_string();
            if path.is_empty() {
                return Err(execution(text.get("先选一份接口 Excel")));
            }
            let code = require_bank_code(params, text)?;
            let mapping = mapping_from(params);
            let esb_side = params.pick("esb_side", "right");
            let parsed = parse_excel(&path, &esb_side, mapping.as_ref())
                .map_err(|error| fail_gateway(text, error))?;
            if parsed.interfaces.is_empty() {
                return Err(fail_gateway(text, GatewayError::NoInterfaces));
            }
            // 银行名留空时用解析器从 sheet 名猜出来的那个（参考仓同款行为，
            // 猜不出来是空串，那就空着——比瞎填一个好）。
            let bank_name = {
                let raw = params.raw("bank_name").trim().to_string();
                if raw.is_empty() {
                    parsed.bank_name.clone()
                } else {
                    raw
                }
            };
            let package_base = {
                let raw = params.raw("package_base").trim().to_string();
                if raw.is_empty() {
                    "com.hundsun.lcpt.gateway".to_owned()
                } else {
                    raw
                }
            };
            let changes = store
                .save_bank(
                    BankMeta {
                        bank_code: code.clone(),
                        bank_name,
                        package_base,
                        ..Default::default()
                    },
                    &parsed.interfaces,
                    &text.get("变更前备份"),
                )
                .map_err(|error| fail_gateway(text, error))?;
            out.log(text.fill(
                "{} 个接口，{} 处变更",
                &[
                    &parsed.interfaces.len().to_string(),
                    &changes.len().to_string(),
                ],
            ));
            let mut report = library_heading(text, started);
            if changes.is_empty() {
                report.push_str(&text.fill(
                    "已存入 {}（{} 个接口，与上次相同）",
                    &[&code, &parsed.interfaces.len().to_string()],
                ));
                report.push('\n');
            } else {
                report.push_str(&text.fill(
                    "已存入 {}（{} 个接口，{} 处变更，变更前的旧数据已备份）",
                    &[
                        &code,
                        &parsed.interfaces.len().to_string(),
                        &changes.len().to_string(),
                    ],
                ));
                report.push_str("\n\n");
                report.push_str(&render_changes(text, &changes));
            }
            Ok(report)
        }
        "version-create" => {
            let code = require_bank_code(params, text)?;
            // 标签留空由内核填时间戳（时间依赖只在内核里）。
            let label = params.raw("version_label").trim().to_string();
            let description = params.raw("version_description").trim().to_string();
            let version = store
                .snapshot(&code, &label, &description)
                .map_err(|error| fail_gateway(text, error))?;
            out.log(text.fill(
                "已存版本 {}（{} 个接口）",
                &[
                    &version.id.to_string(),
                    &version.interfaces.len().to_string(),
                ],
            ));
            let mut report = library_heading(text, started);
            report.push_str(&text.fill(
                "已存版本 {}（{} 个接口）",
                &[
                    &version.id.to_string(),
                    &version.interfaces.len().to_string(),
                ],
            ));
            report.push('\n');
            Ok(report)
        }
        "versions" => {
            let code = require_bank_code(params, text)?;
            let versions = store
                .versions(&code)
                .map_err(|error| fail_gateway(text, error))?;
            out.log(text.fill("{} 个版本", &[&versions.len().to_string()]));
            let mut report = library_heading(text, started);
            report.push_str(&render_versions(text, &versions));
            Ok(report)
        }
        "version-compare" => {
            let code = require_bank_code(params, text)?;
            let versions = store
                .versions(&code)
                .map_err(|error| fail_gateway(text, error))?;
            if versions.len() < 2 {
                return Err(execution(text.get("至少要存过两个版本才能对比")));
            }
            // 留空 = 最近两版（新 vs 次新）——这是「重新导了一份 Excel，看看变了什么」
            // 的默认问法，也是参考仓 `get_interface_diff` 取的同一对。
            let (older_id, newer_id) =
                match (params.integer("version_a"), params.integer("version_b")) {
                    (Some(a), Some(b)) => (a, b),
                    _ => (versions[1].id as i64, versions[0].id as i64),
                };
            let fetch = |id: i64| -> Result<
                duck_finance_core::channel_gateway::Version,
                duck_plugin::PluginError,
            > {
                let id = id.max(0) as u64;
                let found = store
                    .version(&code, id)
                    .map_err(|error| fail_gateway(text, error))?
                    .ok_or_else(|| fail_gateway(text, GatewayError::NoSuchVersion(id)))?;
                Ok(found)
            };
            let older = fetch(older_id)?;
            let newer = fetch(newer_id)?;
            let changes = diff_versions(&older, &newer);
            out.log(text.fill("{} 处变更", &[&changes.len().to_string()]));
            let mut report = library_heading(text, started);
            report.push_str(&text.fill(
                "版本 {} → {}（{} 处变更）",
                &[
                    &older.id.to_string(),
                    &newer.id.to_string(),
                    &changes.len().to_string(),
                ],
            ));
            report.push_str("\n\n");
            report.push_str(&render_changes(text, &changes));
            Ok(report)
        }
        "interface-diff" => {
            let code = require_bank_code(params, text)?;
            let function_id = params.raw("function").trim().to_string();
            if function_id.is_empty() {
                return Err(execution(text.get("先填交易码")));
            }
            let versions = store
                .versions(&code)
                .map_err(|error| fail_gateway(text, error))?;
            if versions.len() < 2 {
                return Err(execution(text.get("至少要存过两个版本才能对比")));
            }
            let find = |version: &duck_finance_core::channel_gateway::Version| {
                version
                    .interfaces
                    .iter()
                    .find(|iface| iface.function_id == function_id)
                    .cloned()
            };
            let older = find(&versions[1]);
            let newer = find(&versions[0]);
            if older.is_none() && newer.is_none() {
                return Err(fail_gateway(
                    text,
                    GatewayError::NoSuchInterface(function_id),
                ));
            }
            let changes = diff_one_interface(&function_id, older.as_ref(), newer.as_ref());
            out.log(text.fill("{} 处变更", &[&changes.len().to_string()]));
            let mut report = library_heading(text, started);
            report.push_str(&text.fill(
                "接口 {} · 版本 {} → {}（{} 处变更）",
                &[
                    &function_id,
                    &versions[1].id.to_string(),
                    &versions[0].id.to_string(),
                    &changes.len().to_string(),
                ],
            ));
            report.push_str("\n\n");
            report.push_str(&render_changes(text, &changes));
            Ok(report)
        }
        "field-edit" => {
            let code = require_bank_code(params, text)?;
            let function_id = params.raw("function").trim().to_string();
            let field_name = params.raw("field").trim().to_string();
            if field_name.is_empty() {
                return Err(execution(text.get("先填字段名")));
            }
            let data_type = params.raw("field_type").trim().to_string();
            let meaning = params.raw("field_meaning").trim().to_string();
            if data_type.is_empty() && meaning.is_empty() {
                return Err(execution(text.get("新类型和新含义至少填一个")));
            }
            // 改前 / 改后各读一次，用同一套 `diff_interfaces` 出清单：
            // 手写「改了哪几处」等于把 diff 逻辑复制一份，两份迟早对不上。
            let before = store
                .bank(&code)
                .map_err(|error| fail_gateway(text, error))?
                .ok_or_else(|| fail_gateway(text, GatewayError::NoSuchBank(code.clone())))?;
            let hits = store
                .edit_field(
                    &code,
                    &FieldEdit {
                        function_id: &function_id,
                        field_name: &field_name,
                        // 不区分方向：同名同改是常态（`serialNo` 两侧都有），
                        // 要只改一侧时把交易码填准即可——那一侧通常本来也不同名。
                        direction: None,
                        // 空串 ≠ 「改成空」：`Option` 才是「不改」，所以这里把
                        // 用户没填的项转成 `None`，而不是传一个空串进去。
                        data_type: (!data_type.is_empty()).then_some(data_type.as_str()),
                        esb_field_name: None,
                        meaning: (!meaning.is_empty()).then_some(meaning.as_str()),
                        required: None,
                    },
                )
                .map_err(|error| fail_gateway(text, error))?;
            if hits == 0 {
                return Err(fail_gateway(
                    text,
                    GatewayError::NoSuchField {
                        function_id,
                        field_name,
                    },
                ));
            }
            let after = store
                .bank(&code)
                .map_err(|error| fail_gateway(text, error))?
                .ok_or_else(|| fail_gateway(text, GatewayError::NoSuchBank(code.clone())))?;
            let changes = diff_interfaces(&before.interfaces, &after.interfaces);
            out.log(text.fill("改了 {} 处", &[&hits.to_string()]));
            let mut report = library_heading(text, started);
            report.push_str(&text.fill(
                "{} 里改了 {} 处（{}）",
                &[&code, &hits.to_string(), &field_name],
            ));
            report.push_str("\n\n");
            report.push_str(&render_changes(text, &changes));
            Ok(report)
        }
        "templates" => {
            let templates = store
                .templates()
                .map_err(|error| fail_gateway(text, error))?;
            out.log(text.fill("{} 个模板", &[&templates.len().to_string()]));
            let mut report = library_heading(text, started);
            report.push_str(&render_templates(text, &templates));
            Ok(report)
        }
        "template-show" => {
            let name = params.raw("template").trim().to_string();
            if name.is_empty() {
                return Err(execution(text.get("先填模板名")));
            }
            let template = store
                .template(&name)
                .map_err(|error| fail_gateway(text, error))?;
            let Some(template) = template else {
                return Err(fail_gateway(text, GatewayError::NoSuchTemplate(name)));
            };
            out.log(text.fill(
                "{}（{}）",
                &[
                    &template.name,
                    &if template.builtin {
                        text.get("内置")
                    } else {
                        text.get("自定义")
                    },
                ],
            ));
            // 只给内容本身：这是要整段复制出去改的东西，抬头落在 .java 里就是语法错。
            Ok(template.content)
        }
        "template-import" => {
            let path = params.raw("path").trim().to_string();
            if path.is_empty() {
                return Err(execution(text.get("保存模板要先选一个模板文件")));
            }
            let content = std::fs::read_to_string(&path)
                .map_err(|error| execution(format!("{path}: {error}")))?;
            let name = {
                let raw = params.raw("template").trim().to_string();
                if !raw.is_empty() {
                    raw
                } else {
                    // 没给名字就用文件名，并把 `.tera` 剥掉——参考仓的模板名
                    // 叫 `BankImpl.java`（不含 `.tera`），剥掉才和内置的摆在一起。
                    Path::new(&path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .map(|name| {
                            name.strip_suffix(".tera")
                                .map(str::to_owned)
                                .unwrap_or(name)
                        })
                        .unwrap_or_default()
                }
            };
            if name.is_empty() {
                return Err(execution(text.get("先填模板名")));
            }
            store
                .save_template(Template {
                    name: name.clone(),
                    content,
                    ..Default::default()
                })
                .map_err(|error| fail_gateway(text, error))?;
            out.log(text.fill("已存入模板 {}", &[&name]));
            let mut report = library_heading(text, started);
            report.push_str(&text.fill("已存入模板 {}", &[&name]));
            report.push('\n');
            Ok(report)
        }
        "template-delete" => {
            let name = params.raw("template").trim().to_string();
            if name.is_empty() {
                return Err(execution(text.get("先填模板名")));
            }
            let removed = store
                .delete_template(&name)
                .map_err(|error| fail_gateway(text, error))?;
            if !removed {
                // 内置的删不掉（参考仓那边内置根本不在表里，删了也是静默无动作）。
                // 分开报「内置不能删」与「没这个模板」：前者是用户点错了对象，
                // 后者是名字打错了，两种情况的下一步完全不同。
                let known = store
                    .template(&name)
                    .map_err(|error| fail_gateway(text, error))?
                    .is_some();
                if known {
                    return Err(execution(text.get("内置模板不能删")));
                }
                return Err(fail_gateway(text, GatewayError::NoSuchTemplate(name)));
            }
            out.log(text.fill("已删除模板 {}", &[&name]));
            let mut report = library_heading(text, started);
            report.push_str(&text.fill("已删除模板 {}", &[&name]));
            report.push('\n');
            Ok(report)
        }
        other => Err(execution(format!(
            "duck.finance.channel-gateway: unknown action {other}"
        ))),
    }
}

fn render_banks(text: &Text, banks: &[BankMeta]) -> String {
    if banks.is_empty() {
        let mut line = text.get("还没存过任何银行");
        line.push('\n');
        return line;
    }
    let mut rows = vec![vec![
        text.get("代码"),
        text.get("名称"),
        text.get("包名"),
        text.get("接口"),
        text.get("更新"),
    ]];
    for bank in banks {
        rows.push(vec![
            bank.bank_code.clone(),
            bank.bank_name.clone(),
            bank.package_base.clone(),
            String::new(),
            bank.updated_at.clone(),
        ]);
    }
    render_table(&rows)
}

/// 字段级表格：每个接口的请求 / 应答字段各占一行。
///
/// `row_id` 用 `交易码|方向|字段名`：宿主把改动送回来时（`params.__edits`），
/// 这三段正好能定位到唯一一行。**不能只靠行下标**——重跑后接口顺序一变，
/// 「第 3 行」就换了人，改动会落到别的字段上。
fn interfaces_table<'a, I>(interfaces: I, text: &Text, editable: bool) -> TableDoc
where
    I: IntoIterator<Item = &'a InterfaceDef>,
{
    let columns = vec![
        text.get("交易码"),
        text.get("方向"),
        text.get("字段"),
        text.get("类型"),
        text.get("必填"),
        text.get("含义"),
    ];
    let mut rows = Vec::new();
    let mut row_ids = Vec::new();
    for iface in interfaces {
        let sides: [(&str, &str, &[FieldDef]); 2] = [
            ("request", "请求", iface.request_fields.as_slice()),
            ("response", "应答", iface.response_fields.as_slice()),
        ];
        for (key, label, fields) in sides {
            for field in fields {
                rows.push(vec![
                    iface.function_id.clone(),
                    text.get(label),
                    field.field_name.clone(),
                    field.data_type.clone(),
                    if field.required {
                        text.get("是")
                    } else {
                        text.get("否")
                    },
                    field.meaning.clone(),
                ]);
                if editable {
                    row_ids.push(format!(
                        "{}|{}|{}",
                        iface.function_id, key, field.field_name
                    ));
                }
            }
        }
    }
    // 没有 `row_ids` 宿主就不给输入框（见 `docs/ifm-plugin-port-design.md` §13）：
    // 「可编辑」意味着「改动会留下」，而 Excel 解析结果没有可落库的去处——
    // 想改字段就先「存入接口库」，再在读回的表里改。
    if editable {
        TableDoc::with_row_ids(columns, rows, row_ids)
    } else {
        TableDoc::new(columns, rows)
    }
}

/// 把表格发给宿主（`body` 通道）。序列化失败就当没发：宿主会退回纯文本，
/// 而纯文本镜像本来就在返回值里。
fn push_table(out: &mut PluginOutput<std::io::Stdout>, table: &TableDoc) {
    if let Ok(json) = serde_json::to_string(table) {
        out.body(&json);
    }
}

/// 解析 `交易码|方向|字段名`（[`interfaces_table`] 造的 `row_id`）。
fn split_row_id(row_id: &str) -> Option<(String, Option<Direction>, String)> {
    let mut parts = row_id.split('|');
    let function_id = parts.next()?.to_owned();
    let direction = match parts.next()? {
        "request" => Some(Direction::Request),
        "response" => Some(Direction::Response),
        _ => return None,
    };
    let field_name = parts.next()?.to_owned();
    Some((function_id, direction, field_name))
}

/// 把宿主送来的表格改动（`params.__edits`）先落进接口库。
///
/// 这是**前置修改**：调用方（`gateway_run`）在动作分派之前调它，于是
/// 「改几格 → 应用」复用的仍是原来的动作（`bank-load` 就是「改完读回来看」），
/// 不需要为编辑另开一条宿主 → 插件的双向通道。
fn apply_table_edits(
    params: &Params,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<(), duck_plugin::PluginError> {
    let edits = duck_plugin::CellEdit::decode_list(&params.0);
    if edits.is_empty() {
        return Ok(());
    }
    let code = params.raw("bank_code").trim().to_string();
    if code.is_empty() {
        return Err(execution(text.get("改了表格就要填银行代码")));
    }
    let mut patches = Vec::new();
    let mut skipped = 0usize;
    for edit in &edits {
        let Some((function_id, direction, field_name)) = split_row_id(&edit.row_id) else {
            skipped += 1;
            continue;
        };
        // 列名与清单 `[io.output.cells]` 一致；不认识的列只跳过，不整批失败。
        let (data_type, meaning) = match edit.column.as_str() {
            "类型" => (Some(edit.value.clone()), None),
            "含义" => (None, Some(edit.value.clone())),
            _ => {
                skipped += 1;
                continue;
            }
        };
        patches.push(FieldPatch {
            function_id,
            field_name,
            direction,
            data_type,
            meaning,
        });
    }
    let store = open_store(text)?;
    let hits = store
        .apply_patches(&code, &patches)
        .map_err(|error| fail_gateway(text, error))?;
    out.log(text.fill("表格改动已写入 {} 处", &[&hits.to_string()]));
    if skipped > 0 {
        out.log(text.fill("{} 处定位不到，已跳过", &[&skipped.to_string()]));
    }
    Ok(())
}

fn render_versions(
    text: &Text,
    versions: &[duck_finance_core::channel_gateway::Version],
) -> String {
    if versions.is_empty() {
        let mut line = text.get("还没有版本");
        line.push('\n');
        return line;
    }
    let mut rows = vec![vec![
        text.get("编号"),
        text.get("标签"),
        text.get("说明"),
        text.get("接口"),
        text.get("时间"),
    ]];
    for version in versions {
        rows.push(vec![
            version.id.to_string(),
            version.label.clone(),
            version.description.clone(),
            version.interfaces.len().to_string(),
            version.created_at.clone(),
        ]);
    }
    render_table(&rows)
}

fn render_templates(text: &Text, templates: &[Template]) -> String {
    let mut rows = vec![vec![
        text.get("模板"),
        text.get("来源"),
        text.get("语言"),
        text.get("行数"),
    ]];
    for template in templates {
        rows.push(vec![
            template.name.clone(),
            if template.builtin {
                text.get("内置")
            } else {
                text.get("自定义")
            },
            template.language.clone(),
            template.content.lines().count().to_string(),
        ]);
    }
    render_table(&rows)
}

/// 变更清单的表格。
fn render_changes(text: &Text, changes: &[Change]) -> String {
    if changes.is_empty() {
        let mut line = text.get("没有变更");
        line.push('\n');
        return line;
    }
    let mut rows = vec![vec![
        text.get("交易码"),
        text.get("变更"),
        text.get("字段"),
        text.get("旧值"),
        text.get("新值"),
    ]];
    for change in changes {
        let (old_value, new_value) = change_values(text, change);
        rows.push(vec![
            change.function_id.clone(),
            change_label(text, change),
            change_field(text, change),
            old_value,
            new_value,
        ]);
    }
    render_table(&rows)
}

fn change_label(text: &Text, change: &Change) -> String {
    match (change.kind, change.attr) {
        (ChangeKind::InterfaceAdded, _) => text.get("新增接口"),
        (ChangeKind::InterfaceRemoved, _) => text.get("删除接口"),
        (ChangeKind::FieldAdded, _) => text.get("新增字段"),
        (ChangeKind::FieldRemoved, _) => text.get("删除字段"),
        // ESB 名变了单列一条：它直接决定报文里的标签名，比类型变更更容易出事。
        (ChangeKind::FieldChanged, Some(FieldAttr::EsbName)) => text.get("ESB 名变更"),
        (ChangeKind::FieldChanged, Some(FieldAttr::Meaning)) => text.get("含义变更"),
        (ChangeKind::FieldChanged, _) => text.get("类型变更"),
    }
}

fn change_field(text: &Text, change: &Change) -> String {
    match (&change.direction, &change.field_name) {
        (Some(direction), Some(name)) => {
            let side = match direction {
                Direction::Request => text.get("请求"),
                Direction::Response => text.get("应答"),
            };
            format!("{side}{}{name}", text.separator())
        }
        _ => String::new(),
    }
}

fn change_values(text: &Text, change: &Change) -> (String, String) {
    let Some((request, response)) = change.field_counts else {
        return (change.old_value.clone(), change.new_value.clone());
    };
    let counts = text.fill(
        "请求 {} / 应答 {}",
        &[&request.to_string(), &response.to_string()],
    );
    match change.kind {
        ChangeKind::InterfaceAdded => (String::new(), counts),
        _ => (counts, String::new()),
    }
}

fn gateway_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let action = params.pick("action", "parse");
    let started = Instant::now();

    // 表格编辑是**前置修改**：宿主点「应用改动」时把 `params.__edits` 一起送来，
    // 先落库再走原动作——于是「改几格 → 重跑」用的还是同一个动作。
    apply_table_edits(&params, out, text)?;

    // 接口库 / 版本 / 模板这几族**不读 Excel**，先分派掉再谈路径。
    // 顺序重要：下面那条「先选一份接口 Excel」的闸门是为解析类动作设的，
    // 放在前面会让「列一下已存的银行」也先要一份 Excel。
    if matches!(
        action.as_str(),
        "banks"
            | "bank-load"
            | "bank-delete"
            | "bank-save"
            | "field-edit"
            | "version-create"
            | "versions"
            | "version-compare"
            | "interface-diff"
            | "templates"
            | "template-show"
            | "template-import"
            | "template-delete"
    ) {
        return gateway_library(&action, &params, out, text, started);
    }

    // 来源两条路：Excel 优先，其次接口库。
    //
    // 第二条不是锦上添花——参考仓的「代码生成 / 报文生成」两个 Tab 都是从**库里**
    // 选接口的（先导入一次，之后反复生成）。只留 Excel 的话，「存入接口库」就退化成
    // 纯存档，用户每次生成还得回去翻那个文件。
    let path = params.raw("path").trim().to_string();
    let bank_code = params.raw("bank_code").trim().to_string();

    if action == "headers" {
        // 列头预览只能看文件：库里存的是解析结果，没有原始列头。
        if path.is_empty() {
            return Err(execution(text.get("先选一份接口 Excel")));
        }
        let sheets = preview_headers(&path).map_err(|error| fail_gateway(text, error))?;
        if sheets.is_empty() {
            return Err(execution(text.get("这份 Excel 里没有工作表")));
        }
        out.log(text.fill("{} 个工作表", &[&sheets.len().to_string()]));
        let mut report = gateway_heading(text, &path, started);
        for sheet in &sheets {
            report.push('\n');
            report.push_str(&format!("## {}\n", sheet.sheet_name));
            report.push_str(&format!("{}{} ", text.get("列头"), text.separator()));
            report.push_str(&sheet.headers.join(" | "));
            report.push('\n');
            if !sheet.sample_rows.is_empty() {
                report.push_str(&format!("{}{}", text.get("样例行"), text.separator()));
                report.push('\n');
                for row in &sheet.sample_rows {
                    report.push_str(&format!("  {}\n", row.join(" | ")));
                }
            }
        }
        return Ok(report);
    }

    // 绑定成 `path`：后面所有 `&path` 的抬头都自动变成「Excel 路径」或「接口库：hzcb」。
    let (result, path) = if !path.is_empty() {
        let mapping = mapping_from(&params);
        let esb_side = params.pick("esb_side", "right");
        let parsed = parse_excel(&path, &esb_side, mapping.as_ref())
            .map_err(|error| fail_gateway(text, error))?;
        out.log(text.fill("{} 个接口", &[&parsed.interfaces.len().to_string()]));
        (parsed, path.clone())
    } else if !bank_code.is_empty() {
        let store = open_store(text)?;
        let bank = store
            .bank(&bank_code)
            .map_err(|error| fail_gateway(text, error))?
            .ok_or_else(|| fail_gateway(text, GatewayError::NoSuchBank(bank_code.clone())))?;
        out.log(text.fill(
            "从接口库读 {} 个接口",
            &[&bank.interfaces.len().to_string()],
        ));
        let label = format!("{}{}{}", text.get("接口库"), text.separator(), bank_code);
        (
            duck_finance_core::channel_gateway::ExcelParseResult {
                bank_name: bank.meta.bank_name.clone(),
                interfaces: bank.interfaces,
            },
            label,
        )
    } else {
        return Err(execution(
            text.get("先选一份接口 Excel，或者填一个已存进库的银行代码"),
        ));
    };

    let filter = params.raw("function").trim().to_lowercase();
    let chosen: Vec<&duck_finance_core::channel_gateway::InterfaceDef> = result
        .interfaces
        .iter()
        .filter(|iface| {
            filter.is_empty()
                || iface.function_id.to_lowercase().contains(&filter)
                || iface.description.to_lowercase().contains(&filter)
        })
        .collect();

    match action.as_str() {
        "parse" => {
            // 表格与文本同源：表里的行就是下面报告里的行（复制得到的是同一份）。
            push_table(out, &interfaces_table(chosen.iter().copied(), text, false));
            Ok(gateway_overview(text, &result, &path, started))
        }
        "fields" => {
            if filter.is_empty() {
                return Err(execution(text.get("先填交易码（或它的一部分）")));
            }
            if chosen.is_empty() {
                let missing = params.raw("function").trim().to_string();
                return Err(fail_gateway(text, GatewayError::NoSuchInterface(missing)));
            }
            let mut report = gateway_heading(text, &path, started);
            for iface in &chosen {
                report.push('\n');
                report.push_str(&gateway_fields(text, iface));
            }
            Ok(report)
        }
        "code" => {
            let side = Side::parse(&params.pick("side", "bank"));
            let bank_code = params.raw("bank_code").trim().to_string();
            let package_base = params.raw("package_base").trim().to_string();
            if bank_code.is_empty() {
                return Err(execution(text.get("生成代码要先填银行代码")));
            }
            if package_base.is_empty() {
                return Err(execution(text.get("生成代码要先填包名基路径")));
            }
            if chosen.is_empty() {
                return Err(execution(text.get("没有匹配的接口可以生成")));
            }
            // 模板名留空 = 用内置那两份；填了就去库里取（内置优先，见 `Store::template`）。
            let custom = {
                let name = params.raw("template").trim().to_string();
                if name.is_empty() {
                    None
                } else {
                    let store = open_store(text)?;
                    let found = store
                        .template(&name)
                        .map_err(|error| fail_gateway(text, error))?;
                    let Some(found) = found else {
                        return Err(fail_gateway(text, GatewayError::NoSuchTemplate(name)));
                    };
                    Some(found.content)
                }
            };
            let mut files = Vec::new();
            for iface in &chosen {
                let generated = code::generate(&code::CodeRequest {
                    bank_code: &bank_code,
                    package_base: &package_base,
                    side,
                    iface,
                    template: custom.as_deref(),
                })
                .map_err(|error| fail_gateway(text, error))?;
                files.push(generated);
            }
            out.log(text.fill("已生成 {} 个文件", &[&files.len().to_string()]));
            let mut report = String::new();
            for (index, generated) in files.iter().enumerate() {
                if files.len() > 1 {
                    if index > 0 {
                        report.push('\n');
                    }
                    report.push_str(&format!("// --- {} ---\n", generated.file_name));
                }
                report.push_str(&generated.content);
            }
            Ok(report)
        }
        "xml" => {
            if chosen.is_empty() {
                return Err(execution(text.get("没有匹配的接口可以生成")));
            }
            let serial = text.get("示例流水号");
            let success = text.get("成功");
            let sample = text.get("示例值");
            let samples = XmlSamples {
                serial: &serial,
                success: &success,
                sample: &sample,
            };
            let mut report = gateway_heading(text, &path, started);
            for iface in &chosen {
                let template = generate_message_template(iface, &samples);
                report.push('\n');
                report.push_str(&format!(
                    "## {} {}\n\n",
                    template.function_id, iface.description
                ));
                report.push_str(&format!("### {}\n", text.get("请求报文")));
                report.push_str(&template.request_xml);
                report.push_str("\n\n");
                report.push_str(&format!("### {}\n", text.get("应答报文")));
                report.push_str(&template.response_xml);
                report.push('\n');
            }
            Ok(report)
        }
        other => Err(execution(format!(
            "duck.finance.channel-gateway: unknown action {other}"
        ))),
    }
}

fn gateway_heading(text: &Text, path: &str, started: Instant) -> String {
    let mut line = text.fill(
        "{} · {} · 耗时 {} ms",
        &[
            &text.get("渠道网关"),
            path,
            &started.elapsed().as_millis().to_string(),
        ],
    );
    line.push('\n');
    line
}

fn gateway_overview(
    text: &Text,
    result: &duck_finance_core::channel_gateway::ExcelParseResult,
    path: &str,
    started: Instant,
) -> String {
    let mut report = gateway_heading(text, path, started);
    report.push('\n');
    let bank = if result.bank_name.trim().is_empty() {
        "-".to_string()
    } else {
        result.bank_name.clone()
    };
    report.push_str(&format!(
        "{}{}{}\n",
        text.get("银行"),
        text.separator(),
        bank
    ));
    report.push_str(&format!(
        "{}{}{}\n\n",
        text.get("接口"),
        text.separator(),
        result.interfaces.len()
    ));

    let mut rows = vec![vec![
        text.get("交易码"),
        text.get("描述"),
        text.get("请求字段"),
        text.get("应答字段"),
        text.get("数组"),
    ]];
    for iface in &result.interfaces {
        let arrays = iface
            .request_fields
            .iter()
            .chain(iface.response_fields.iter())
            .filter(|field| field.is_array)
            .count();
        rows.push(vec![
            iface.function_id.clone(),
            iface.description.clone(),
            iface.request_fields.len().to_string(),
            iface.response_fields.len().to_string(),
            arrays.to_string(),
        ]);
    }
    report.push_str(&render_table(&rows));
    report
}

fn gateway_fields(text: &Text, iface: &duck_finance_core::channel_gateway::InterfaceDef) -> String {
    let mut block = format!("## {} {}\n", iface.function_id, iface.description);
    block.push_str(&format!(
        "{}{}{}\n\n",
        text.get("请求字段"),
        text.separator(),
        iface.request_fields.len()
    ));
    block.push_str(&render_table(&field_rows(text, &iface.request_fields)));
    block.push('\n');
    block.push_str(&format!(
        "{}{}{}\n\n",
        text.get("应答字段"),
        text.separator(),
        iface.response_fields.len()
    ));
    block.push_str(&render_table(&field_rows(text, &iface.response_fields)));
    block
}

fn field_rows<'a>(
    text: &Text,
    fields: impl IntoIterator<Item = &'a duck_finance_core::channel_gateway::FieldDef>,
) -> Vec<Vec<String>> {
    let mut rows = vec![vec![
        text.get("名称"),
        text.get("ESB 字段"),
        text.get("类型"),
        text.get("必填"),
        text.get("含义"),
    ]];
    for field in fields {
        let name = if field.is_array {
            format!("{}[]", field.field_name)
        } else {
            field.field_name.clone()
        };
        rows.push(vec![
            name,
            field.esb_field_name.clone(),
            field.data_type.clone(),
            if field.required {
                text.get("是")
            } else {
                String::new()
            },
            field.meaning.clone(),
        ]);
        for item in &field.array_items {
            rows.push(vec![
                format!("  └ {}", item.field_name),
                item.esb_field_name.clone(),
                item.data_type.clone(),
                if item.required {
                    text.get("是")
                } else {
                    String::new()
                },
                item.meaning.clone(),
            ]);
        }
    }
    rows
}

/// 列映射：填了「字段名列」才进映射模式（其余列留空 = 只用这一列认字段）。
fn mapping_from(params: &Params) -> Option<ColMapping> {
    Some(ColMapping {
        field_name_col: params.column("field_col")?,
        esb_name_col: params.column("esb_col"),
        data_type_col: params.column("type_col"),
        required_col: params.column("required_col"),
        meaning_col: params.column("meaning_col"),
    })
}

// ── 报告排版 ──────────────────────────────────────────────────────────

/// 等宽表格：按**显示宽度**对齐（中文算两列）。第一行当表头，末列不补空格。
fn render_table(rows: &[Vec<String>]) -> String {
    let Some(first) = rows.first() else {
        return String::new();
    };
    let columns = first.len();
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(columns) {
            widths[index] = widths[index].max(display_width(cell));
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (index, cell) in row.iter().enumerate().take(columns) {
            line.push_str(cell);
            if index + 1 < columns {
                let pad = widths[index].saturating_sub(display_width(cell)) + 2;
                line.push_str(&" ".repeat(pad));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|ch| if is_wide(ch) { 2 } else { 1 })
        .sum()
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_numbers_are_one_based() {
        let params = Params::new(&serde_json::json!({
            "field_col": 1,
            "esb_col": "3",
            "type_col": 0,
            "required_col": serde_json::Value::Null,
        }));
        assert_eq!(params.column("field_col"), Some(0));
        assert_eq!(params.column("esb_col"), Some(2));
        // 0 / 空 / 缺失都算「没填」。
        assert_eq!(params.column("type_col"), None);
        assert_eq!(params.column("required_col"), None);
        assert_eq!(params.column("meaning_col"), None);
    }

    #[test]
    fn mapping_requires_the_field_column() {
        let empty = Params::new(&serde_json::json!({}));
        assert!(mapping_from(&empty).is_none());
        let mapped = Params::new(&serde_json::json!({ "field_col": 2 }));
        let mapping = mapping_from(&mapped).expect("给了字段列就是映射模式");
        assert_eq!(mapping.field_name_col, 1);
        assert!(mapping.esb_name_col.is_none());
    }

    #[test]
    fn table_alignment_counts_wide_characters() {
        let rows = vec![
            vec!["名称".to_string(), "code".to_string()],
            vec!["账号".to_string(), "accNo".to_string()],
        ];
        let text = render_table(&rows);
        let lines: Vec<&str> = text.lines().collect();
        // 两行的第二列都从同一列开始（中文按两列宽算）。
        assert_eq!(lines[0].find("code"), lines[1].find("accNo"), "{text}");
    }

    #[test]
    fn model_title_falls_back_on_missing_halves() {
        let mut model = PdmModel::default();
        assert_eq!(model_title(&model), "-");
        model.code = "deposit".into();
        assert_eq!(model_title(&model), "deposit");
        model.name = "存款核心".into();
        assert_eq!(model_title(&model), "存款核心 (deposit)");
    }
}
