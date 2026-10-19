//! 零散工具族内核（`duck.misc.*`，计划 §6.8/§6.9 合并后的一包）。
//!
//! 三个工具、一份内核：
//!
//! | 工具 | 模块 | 参考仓来源 |
//! | --- | --- | --- |
//! | JSON Schema 校验 | [`json_schema`] | `JsonSchemaValidator.vue`（源仓用前端 `ajv`，**无后端**） |
//! | Git 文本生成 | [`git`] | `GitTools.vue`（三个页签全是纯前端字符串处理，**无后端调用**） |
//! | Nginx 配置生成 | [`nginx`] | `nginx/{config,generator,validator,templates}.rs`（786 行，逐字） |
//!
//! 纪律与前几族一致：**零 gpui、零 tauri、全同步、内核不出中文**。错误走枚举 /
//! 稳定码，整句与细节由插件层（`crates/duck-misc-tool`）按当前语言拼。
//!
//! ## 三个工具各自的一处主动偏离
//!
//! 1. **JSON Schema 是重写不是复制**：源仓前端挂 `ajv` + `ajv-formats`，那是 JS 库
//!    不是业务逻辑，Rust 侧没有对应实现可抄（`jsonschema` crate 不在本仓依赖图、
//!    也不在离线缓存里）。这里按 Draft-07 规范实现**够用子集**，
//!    报错文案**逐句照抄 ajv**（`must have required property 'x'` 那套），
//!    这样同一份 Schema 在两个工具里给出的错误长得一样。
//! 2. **Nginx 不引 `tera`**：源仓用 `tera` 渲染 116 行模板。为了一个文本模板把
//!    `tera`（连带 `pest` / `globwalk` / `chrono` / `unic-segment`）拉进进程是
//!    纯负担，这里改成手写渲染器，逐行对齐模板的**输出**，并用快照测试钉住。
//! 3. **Git 的「变更文件:」抬头下沉**：源仓把中文抬头直接拼在提交信息里；
//!    内核只返回结构化草案（[`git::CommitDraft`]），抬头由插件层翻译后拼。

pub mod cheat;
pub mod git;
pub mod json_schema;
pub mod nginx;

pub use cheat::{
    CHEAT_SHEETS, Command, Hit, Sheet, Variable, builtin as builtin_cheat_sheets,
    extract_variables, fuzzy_score, has_placeholder, parse as parse_cheat, parse_assignments,
    parse_commands, render_template, search as search_cheat_sheets,
};
pub use git::{
    CommitDraft, GITIGNORE_LANGS, changed_files, commit_draft, gitignore_for, hook_script,
    summarize_additions,
};
pub use json_schema::{SchemaError, SchemaProblem, validate};
pub use nginx::{
    Comments, GenerateResult, LocationConfig, NginxConfig, NginxError, PRESET_IDS, PresetTemplate,
    apply_patch, generate_config, generate_default, generate_with_preset, preset_config, presets,
};
