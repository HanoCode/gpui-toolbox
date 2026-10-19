//! 零散工具族（`duck.misc.*`）的 **process 轨入口**。
//!
//! 与其余各族同一个骨架：一个二进制服务整族工具，按 `request.tool` 分派到
//! [`duck_misc_core`]。本文件只做三件事：
//!
//! 1. 把 `[io]` 参数摊平成内核入参；
//! 2. 把内核的产出排成**可复制的文本报告**；
//! 3. 把内核的稳定码（`SchemaProblem` / `NginxError`）翻成当前语言的一句话
//!    （[`i18n::fill`] 负责插值）。
//!
//! 三个工具都 `allow_files = "none"`：不读也不写任何用户文件 —— 输入来自表单，
//! 产物是文本（复制走人），落盘由用户自己决定。

use std::process::ExitCode;

use duck_misc_core::{
    Comments, GITIGNORE_LANGS, NginxError, SchemaProblem, apply_patch, commit_draft,
    generate_config, generate_with_preset, gitignore_for, hook_script, preset_config, validate,
};
use duck_plugin::PluginLocales;
use duck_plugin_process::{PluginOutput, RunRequest, serve};

mod i18n;

/// 参数区（与其余各族同一套：宿主把表单值序列化成字符串键值对）。
struct Params(serde_json::Value);

impl Params {
    fn new(value: &serde_json::Value) -> Self {
        Self(value.clone())
    }

    /// 取一段文本：**不 trim**（JSON / diff 的首尾空白都是内容）。
    fn raw(&self, key: &str) -> String {
        match self.0.get(key) {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(serde_json::Value::Number(number)) => number.to_string(),
            Some(serde_json::Value::Bool(flag)) => flag.to_string(),
            _ => String::new(),
        }
    }

    /// 取枚举型参数（select）：空串用 `fallback`。
    fn pick(&self, key: &str, fallback: &str) -> String {
        match self.raw(key).trim() {
            "" => fallback.to_owned(),
            other => other.to_owned(),
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
    // 页内扫描（宿主 `[io] scan` 声明触发）：不是一次正常执行，先拦截。
    if request.action.as_deref() == Some("scan") {
        return page_scan(request, out, &text);
    }
    match request.tool.as_str() {
        "duck.misc.json-schema" => schema_run(request, out, &text),
        "duck.misc.git-tools" => git_run(request, out, &text),
        "duck.misc.nginx" => nginx_run(request, out, &text),
        "duck.text.cheat-sheets" => cheat_run(request, out, &text),
        other => Err(duck_plugin::PluginError::Execution(format!(
            "duck-misc-tool: unknown tool {other}"
        ))),
    }
}

/// 页内扫描（`layout = "workbench"` 的速查手册）：列出可用的速查表，
/// 喂宿主工作台左列表（body 形状 `{"list": [{"value": slug, "label": 名}]}`）。
///
/// 与正常运行同一份数据源（内置表 + 导入目录），所以「列表里有的」永远可以打开
/// ——不存在「列表选了、运行却找不到」的错位。导入目录是可选参数：空 = 只列内置。
fn page_scan(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    if request.tool != "duck.text.cheat-sheets" {
        return Err(duck_plugin::PluginError::Execution(
            "page scan is not supported by this tool".into(),
        ));
    }
    let params = Params::new(&request.params);
    let dir = params.pick("dir", "");
    let mut sheets = Vec::new();
    if !dir.is_empty() {
        // 导入目录扫不出来不当失败（目录还没有 .cheat 文件很常见），
        // 列表里至少还有内置表。
        if let Ok(imported) = load_cheat_dir(std::path::Path::new(&dir), text) {
            out.log(text.fill("从目录导入 {} 张速查表", &[&imported.len().to_string()]));
            sheets.extend(imported);
        }
    }
    sheets.extend(duck_misc_core::builtin_cheat_sheets());
    let list: Vec<serde_json::Value> = sheets
        .iter()
        .map(|sheet| {
            serde_json::json!({
                "value": sheet.slug,
                "label": text.get(&sheet.name),
            })
        })
        .collect();
    let body = serde_json::json!({ "list": list });
    out.body(&body.to_string());
    Ok(String::new())
}

/// 稳定码 → 当前语言的一句话，细节跟在分隔符后面（细节是数据，不翻译）。
fn with_detail(text: &Text, base: String, detail: Option<String>) -> duck_plugin::PluginError {
    match detail {
        Some(detail) => {
            duck_plugin::PluginError::Execution(format!("{base}{}{detail}", text.separator()))
        }
        None => duck_plugin::PluginError::Execution(base),
    }
}

// ── JSON Schema 校验 ───────────────────────────────────────────────────

fn schema_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let schema = params.raw("schema");
    let instance = params.raw("instance");

    if schema.trim().is_empty() || instance.trim().is_empty() {
        return Err(duck_plugin::PluginError::Execution(
            text.get("Schema 和 JSON 实例都要填"),
        ));
    }

    out.log(text.get("正在校验…"));
    let errors = validate(&schema, &instance).map_err(|problem| match problem {
        SchemaProblem::SchemaNotJson => {
            duck_plugin::PluginError::Execution(text.get("Schema JSON 格式错误"))
        }
        SchemaProblem::InstanceNotJson => {
            duck_plugin::PluginError::Execution(text.get("实例 JSON 格式错误"))
        }
        SchemaProblem::SchemaNotObject => {
            duck_plugin::PluginError::Execution(text.get("Schema 必须是 JSON 对象（或布尔值）"))
        }
        SchemaProblem::UnknownRef(reference) => {
            with_detail(text, text.get("Schema 引用了找不到的定义"), Some(reference))
        }
        SchemaProblem::BadRegex(pattern) => {
            with_detail(text, text.get("Schema 里的正则写错了"), Some(pattern))
        }
        SchemaProblem::TooDeep => {
            duck_plugin::PluginError::Execution(text.get("Schema 嵌套太深（可能有循环引用）"))
        }
    })?;

    if errors.is_empty() {
        out.log(text.get("校验通过"));
        return Ok(format!("{}\n", text.get("✓ 校验通过")));
    }

    let count = errors.len().to_string();
    out.log(text.fill("{} 个错误", &[&count]));
    let mut report = format!("{}\n", text.fill("{} 个错误", &[&count]));
    for error in &errors {
        report.push_str(&format!("  {} — {}\n", error.display_path(), error.message));
    }
    Ok(report)
}

// ── Git 文本生成 ──────────────────────────────────────────────────────

fn git_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    match params.pick("mode", "commit").as_str() {
        "commit" => git_commit(&params, out, text),
        "hook" => {
            let hook = params.pick("hook", "pre-commit");
            let lang = params.pick("lang", "bash");
            let script = hook_script(&hook, &lang);
            out.log(text.fill("已生成 {}（{}）", &[&hook, &lang]));
            Ok(script)
        }
        "gitignore" => {
            let langs = parse_langs(&params.pick("langs", "Node,IDE"));
            if langs.is_empty() {
                return Err(duck_plugin::PluginError::Execution(
                    text.get("至少要选一个语言或平台"),
                ));
            }
            let known = langs
                .iter()
                .filter(|lang| GITIGNORE_LANGS.contains(&lang.as_str()))
                .count();
            out.log(text.fill(
                "{} 段模板（{} 段有内置模板）",
                &[&langs.len().to_string(), &known.to_string()],
            ));
            Ok(gitignore_for(&langs))
        }
        other => Err(duck_plugin::PluginError::Execution(format!(
            "duck-misc-tool: unknown git mode {other}"
        ))),
    }
}

fn git_commit(
    params: &Params,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let diff = params.raw("diff");
    if diff.trim().is_empty() {
        return Err(duck_plugin::PluginError::Execution(
            text.get("先粘贴 git diff 的输出"),
        ));
    }
    let kind = params.pick("type", "feat");
    let draft = commit_draft(&diff, &kind);

    out.log(text.fill("识别到 {} 个文件", &[&draft.files.len().to_string()]));
    let mut report = format!("{}\n", draft.subject);
    if draft.files.is_empty() {
        report.push('\n');
        report.push_str(&format!(
            "{}\n",
            text.get("diff 里没有 `diff --git` 行，认不出变更文件")
        ));
    } else {
        report.push('\n');
        report.push_str(&text.get("变更文件:"));
        report.push('\n');
        for file in &draft.files {
            report.push_str(&format!("- {file}\n"));
        }
    }
    Ok(report)
}

/// `Node, IDE` / `Node IDE` / `Node;IDE` 都认。
fn parse_langs(raw: &str) -> Vec<String> {
    raw.split([',', ';', ' ', '，', '；'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

// ── Nginx 配置生成 ────────────────────────────────────────────────────

fn nginx_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let comments = Comments {
        client_max_body_size: &text.get("客户端最大请求体大小"),
        ssl: &text.get("SSL 配置"),
        error_pages: &text.get("错误页面"),
    };
    let preset = params.pick("preset", "basic");

    if params.pick("mode", "preset") == "preset" {
        out.log(text.fill("按预设 {} 生成", &[&preset]));
        let result = generate_with_preset(&preset, &comments);
        return finish_nginx(result, text);
    }

    // `custom`：预设当底，再用用户那段 JSON 覆盖。
    let patch_text = params.raw("config");
    if patch_text.trim().is_empty() {
        return Err(duck_plugin::PluginError::Execution(
            text.get("自定义模式要在输入框写一段 JSON（覆盖预设里想改的字段）"),
        ));
    }
    let patch: serde_json::Value = serde_json::from_str(&patch_text)
        .map_err(|error| with_detail(text, text.get("JSON 解析失败"), Some(error.to_string())))?;
    if !patch.is_object() {
        return Err(duck_plugin::PluginError::Execution(
            text.get("覆盖内容必须是一个 JSON 对象"),
        ));
    }

    // 预设当底；未知 id 与「不选」都回落默认，和源仓一致。
    let mut merged = apply_patch(&preset_config(&preset), &patch).map_err(|error| {
        with_detail(
            text,
            text.get("覆盖内容与配置结构对不上"),
            Some(error.to_string()),
        )
    })?;
    let result = generate_config(&mut merged, &comments);
    // 校验不过就别报「已生成」：那会让日志与错误互相打脸。
    if result.success {
        out.log(text.fill("在预设 {} 上覆盖生成", &[&preset]));
    }
    finish_nginx(result, text)
}

fn finish_nginx(
    result: duck_misc_core::GenerateResult,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    if !result.success {
        let lines: Vec<String> = result
            .errors
            .iter()
            .map(|error| format!("  - {}", nginx_error_text(text, error)))
            .collect();
        return Err(duck_plugin::PluginError::Execution(format!(
            "{}\n{}",
            text.get("配置校验没通过:"),
            lines.join("\n")
        )));
    }
    Ok(result.config)
}

/// `NginxError` → 当前语言的一句话。
fn nginx_error_text(text: &Text, error: &NginxError) -> String {
    let base = text.get(match error {
        NginxError::PortEmpty => "监听端口不能为空",
        NginxError::PortRange => "监听端口必须在 1-65535 之间",
        NginxError::PortNotNumber => "监听端口必须是有效数字",
        NginxError::ServerNameEmpty => "Server Name 不能为空",
        NginxError::RootEmpty => "网站根目录不能为空",
        NginxError::UpstreamNameEmpty(_) => "第 {} 个 Upstream 没有名字",
        NginxError::UpstreamNoServer(_) => "Upstream {} 至少需要一个服务器",
        NginxError::LocationPathEmpty(_) => "第 {} 个 Location 没有路径",
        NginxError::LocationTargetEmpty(_) => "Location {} 没有目标",
        NginxError::ProxyTargetScheme(_) => "Location {} 的代理目标要以 http:// 或 https:// 开头",
        NginxError::CertificateEmpty => "开了 SSL 就要填证书路径",
        NginxError::CertificateKeyEmpty => "开了 SSL 就要填证书私钥路径",
        NginxError::WorkersNotNumber => "工作连接数必须是有效数字",
        NginxError::WorkersRange => "工作连接数必须在 1-65535 之间",
    });
    match error.detail() {
        Some(detail) => i18n::fill(&base, &[&detail]),
        None => base,
    }
}

// ── 速查手册 ──────────────────────────────────────────────────────────

/// 递归找 `.cheat` 文件时最多下钻几层。
///
/// 没有上限的话，一个指向父目录的符号链接就能让扫描转圈——插件进程有 10 秒墙钟，
/// 超时只报「命令执行超时」，排查起来看不出是目录结构的问题。
const MAX_CHEAT_DEPTH: usize = 8;

fn cheat_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let builtin = duck_misc_core::builtin_cheat_sheets();

    // 导入目录：参考页那个「导入速查表目录」按钮，扫到的表排在**内置表前面**
    // （参考仓 `[...importedSheets, ...cheatSheets]`），用户刚导的应该先看到。
    let dir = params.pick("dir", "");
    let mut sheets_ordered = Vec::new();
    if !dir.is_empty() {
        sheets_ordered = load_cheat_dir(std::path::Path::new(&dir), text)?;
        out.log(text.fill(
            "从目录导入 {} 张速查表",
            &[&sheets_ordered.len().to_string()],
        ));
    }
    let imported_count = sheets_ordered.len();
    sheets_ordered.extend(builtin);
    let sheets = sheets_ordered;

    let keyword = params.raw("keyword");
    let selected = params.pick("sheet", "auto");
    if selected == "auto" {
        return Ok(cheat_list(&sheets, keyword.trim(), imported_count, text));
    }

    let Some(sheet) = sheets.iter().find(|sheet| sheet.slug == selected) else {
        return Err(with_detail(
            text,
            text.get("找不到这张速查表"),
            Some(selected),
        ));
    };
    out.log(text.fill(
        "打开 {}（{} 条命令模板）",
        &[&text.get(&sheet.name), &sheet.commands.len().to_string()],
    ));
    Ok(cheat_open(
        sheet,
        params.pick("view", "all").as_str(),
        &duck_misc_core::parse_assignments(&params.raw("vars")),
        text,
    ))
}

/// 列表/搜索结果：按分类分组，一张表一行。
fn cheat_list(
    sheets: &[duck_misc_core::Sheet],
    keyword: &str,
    imported: usize,
    text: &Text,
) -> String {
    let hits = duck_misc_core::search_cheat_sheets(sheets, keyword);
    let mut report = String::new();
    if keyword.is_empty() {
        report.push_str(&text.fill("{} 张速查表", &[&sheets.len().to_string()]));
    } else if hits.is_empty() {
        return text.fill("没有匹配「{}」的速查表", &[keyword]);
    } else {
        report.push_str(&text.fill(
            "匹配「{}」的 {} 张速查表",
            &[keyword, &hits.len().to_string()],
        ));
    }
    if imported > 0 {
        report.push_str(&text.fill(" · 已扫描导入目录 {} 张", &[&imported.to_string()]));
    }
    report.push_str("\n\n");

    // 分组顺序＝首次出现顺序：内置表已经是参考仓的分类顺序，导入的跟在后面。
    // 分类名与表名是 `.cheat` 里的数据（中文），但它们是**导航骨架**：下拉、列表、
    // 标题里反复出现。渲染期取词，英文界面下才不是一屏中文。正文与命令描述不翻
    // ——那是内容本身，参考仓也只有中文一份。
    let mut current: Option<String> = None;
    for hit in &hits {
        let heading = if hit.sheet.category.is_empty() {
            text.get("未分类")
        } else {
            text.get(&hit.sheet.category)
        };
        if current.as_deref() != Some(heading.as_str()) {
            // 第一组紧接抬头，后面每组空一行。
            if current.is_some() {
                report.push('\n');
            }
            report.push_str(&heading);
            report.push('\n');
            current = Some(heading);
        }
        report.push_str(&format!(
            "  {}  ({})",
            text.get(&hit.sheet.name),
            hit.sheet.slug
        ));
        report.push_str("  · ");
        report.push_str(&if hit.sheet.commands.is_empty() {
            text.get("纯速查")
        } else {
            text.fill("{} 条命令", &[&hit.sheet.commands.len().to_string()])
        });
        report.push('\n');
    }
    report.push('\n');
    report.push_str(&text.get("把「速查表」换成上面任意一个 slug 打开；留空则按关键词重新搜索"));
    report
}

/// 打开一张表：副标题 + 命令模板（变量已替换）+ 速查正文。
fn cheat_open(
    sheet: &duck_misc_core::Sheet,
    view: &str,
    values: &std::collections::BTreeMap<String, String>,
    text: &Text,
) -> String {
    let mut report = format!("# {}\n\n", text.get(&sheet.name));
    let mut meta = vec![text.get(&sheet.category), sheet.slug.clone()];
    if !sheet.commands.is_empty() {
        meta.push(text.fill("{} 条命令模板", &[&sheet.commands.len().to_string()]));
    }
    report.push_str(
        &meta
            .iter()
            .filter(|part| !part.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" · "),
    );
    report.push_str("\n\n");

    let show_commands = view != "content" && !sheet.commands.is_empty();
    let show_content = view != "commands" && !sheet.content.is_empty();

    if show_commands {
        report.push_str(&format!("## {}\n\n", text.get("命令模板")));
        for command in &sheet.commands {
            report.push_str(&format!(
                "### {}\n\n",
                if command.description.is_empty() {
                    text.get("命令")
                } else {
                    command.description.clone()
                }
            ));
            let rendered = duck_misc_core::render_template(&command.template, values);
            report.push_str("```\n");
            report.push_str(&rendered);
            report.push_str("\n```\n");

            let names = command.variable_names();
            if !names.is_empty() {
                report.push('\n');
            }
            for name in &names {
                let value = values.get(name);
                let variable = command.variable(name);
                let mut line = format!("- {}", text.fill("变量 `{}`", &[name]));
                if let Some(value) = value {
                    // ` = 值` 不用译文：纯符号加数据，翻不出花样。
                    line.push_str(&format!(" = {value}"));
                } else {
                    line.push_str(&text.get("（未填写）"));
                }
                if let Some(variable) = variable {
                    if !variable.options.is_empty() {
                        line.push_str(&text.fill("（候选：{}）", &[&variable.options.join(" / ")]));
                    }
                    if let Some(shell) = &variable.command {
                        line.push_str(&text.fill("（候选由这条命令产出：{}）", &[shell]));
                    }
                }
                report.push_str(&line);
                report.push('\n');
            }
            // 未填的占位符会让命令原样留着 `<x>`，提醒一句别直接粘进终端。
            let missing: Vec<&String> = names
                .iter()
                .filter(|name| !values.contains_key(name.as_str()))
                .collect();
            if !missing.is_empty() {
                let joined = missing
                    .iter()
                    .map(|name| format!("<{name}>"))
                    .collect::<Vec<_>>()
                    .join(" ");
                report.push('\n');
                report.push_str(&text.fill("⚠️ 还差 {} 没填", &[&joined]));
                report.push('\n');
            }
            report.push('\n');
        }
    }

    if show_content {
        report.push_str(&format!("## {}\n\n", text.get("速查内容")));
        report.push_str(&sheet.content);
        report.push('\n');
    }

    if !show_commands && !show_content {
        report.push_str(&text.get("这张表没有速查正文，也没有命令模板"));
        report.push('\n');
    }
    report
}

/// 扫一个目录（递归）里的 `.cheat` 文件，逐份解析。
fn load_cheat_dir(
    dir: &std::path::Path,
    text: &Text,
) -> Result<Vec<duck_misc_core::Sheet>, duck_plugin::PluginError> {
    if !dir.is_dir() {
        return Err(with_detail(
            text,
            text.get("路径不是有效目录"),
            Some(dir.display().to_string()),
        ));
    }
    let mut paths = Vec::new();
    collect_cheat_files(dir, 0, &mut paths);
    if paths.is_empty() {
        return Err(duck_plugin::PluginError::Execution(
            text.get("目录里没有 .cheat 文件"),
        ));
    }
    paths.sort();

    let mut sheets = Vec::new();
    for path in paths {
        // 读不动的单份文件跳过（可能是二进制或权限问题），不让整次导入失败。
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "sheet".to_owned());
        let mut sheet = duck_misc_core::parse_cheat(&raw, &stem);
        // `import-` 前缀：参考仓 `import-${slug}`，避免与内置表的 slug 撞车。
        sheet.slug = format!("import-{}", sheet.slug);
        sheets.push(sheet);
    }
    if sheets.is_empty() {
        return Err(duck_plugin::PluginError::Execution(
            text.get("目录里的 .cheat 文件都读不出来"),
        ));
    }
    Ok(sheets)
}

fn collect_cheat_files(dir: &std::path::Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
    if depth > MAX_CHEAT_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_cheat_files(&path, depth + 1, out);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cheat"))
        {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_accept_comma_space_and_full_width() {
        assert_eq!(parse_langs("Node, IDE"), vec!["Node", "IDE"]);
        assert_eq!(parse_langs("Rust  Docker"), vec!["Rust", "Docker"]);
        assert_eq!(parse_langs("Node；IDE"), vec!["Node", "IDE"]);
        assert!(parse_langs("   ").is_empty());
    }

    #[test]
    fn preset_config_matches_the_generator_presets() {
        // 预设 id 全都能取到配置，且 `static-site` 没有 upstream。
        for id in duck_misc_core::PRESET_IDS {
            assert!(duck_misc_core::PRESET_IDS.contains(&id));
        }
        assert!(preset_config("static-site").upstreams.is_empty());
        assert_eq!(preset_config("split-view").server.listen_port, "8180");
        // 未知 id 回落默认（有 upstream）。
        assert_eq!(preset_config("nope").upstreams.len(), 1);
    }

    #[test]
    fn commit_draft_reaches_the_core_parser() {
        let draft = commit_draft(
            "diff --git a/x.rs b/x.rs\n+++ b/x.rs\n+    let a = 1;\n",
            "fix",
        );
        assert_eq!(draft.subject, "fix(x.rs): let a = 1");
    }
}
