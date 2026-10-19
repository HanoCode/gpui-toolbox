//! Nginx 配置生成（`duck.misc.nginx`）。
//!
//! 逐字移植参考仓 `src-tauri/src/nginx/`（786 行）：
//!
//! | 源文件 | 这里 | 处理 |
//! | --- | --- | --- |
//! | `config.rs` | [`config`] | 结构体与默认值**逐字段照抄**（含 5 份预设） |
//! | `validator.rs` | [`validator`] | 校验规则照抄；中文错误改 [`NginxError`] 稳定码 |
//! | `generator.rs` | [`generator`] | 流程照抄（`apply_rules` → 校验 → 渲染） |
//! | `templates.rs` + `nginx.conf.tera` | [`render`] | **手写渲染器**，见下 |
//!
//! ## 为什么踢掉 `tera`
//!
//! 源仓为了渲染 116 行模板引了 `tera`（连带 `pest` / `globwalk` / `unic-segment`…）。
//! 插件进程是「跑一次就退」的短命程序，为一段固定文本背一套模板引擎不划算，
//! 所以 [`render`] 逐行对齐模板的**结构**：
//!
//! - 输出**内容**一致：指令顺序、缩进、`;`、条件分支都与模板相同；
//! - 唯一的差别是**空行不再带空白**。`tera` 会把 `{% if %}` 那一行前面的缩进
//!   原样留在输出里（于是出现「只有四个空格的空行」），手写渲染器直接给空行。
//!
//! ## 模板里的三句中文注释
//!
//! 模板里 `# 客户端最大请求体大小`、`# SSL 配置`、`# 错误页面` 是**产物的一部分**，
//! 内核不产中文 ⇒ 由调用方经 [`Comments`] 传进来（插件层按当前语言取词）。

pub mod config;
pub mod generator;
pub mod merge;
pub mod render;
pub mod validator;

pub use config::{
    AdvancedConfig, ErrorPage, GenerateResult, LocationConfig, LoggingConfig, NginxConfig,
    PerformanceConfig, PresetTemplate, SecurityConfig, ServerConfig, SslConfig, UpstreamConfig,
    UpstreamServer,
};
pub use generator::{
    PRESET_IDS, generate_config, generate_default, generate_with_preset, preset_config, presets,
};
pub use merge::apply_patch;
pub use render::Comments;
pub use validator::{NginxError, apply_rules, validate_config};
