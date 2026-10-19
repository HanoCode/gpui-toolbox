//! 生成流程与 5 份预设（源仓 `nginx/generator.rs`，逐条照抄）。
//!
//! 流程：`apply_rules`（补默认）→ `validate_config`（校验）→ `render`（渲染）。
//! 校验不过就返回错误列表、不渲染 —— 与源仓一致。

use super::config::{
    AdvancedConfig, GenerateResult, LocationConfig, LoggingConfig, NginxConfig, PerformanceConfig,
    PresetTemplate, SecurityConfig, ServerConfig, SslConfig, UpstreamConfig, UpstreamServer,
};
use super::render::{Comments, render};
use super::validator::{apply_rules, validate_config};

/// 五份预设的 id（名字与说明在插件译文表里，见 [`PresetTemplate`]）。
pub const PRESET_IDS: [&str; 5] = [
    "basic",
    "reverse-proxy",
    "static-site",
    "java-app",
    "split-view",
];

/// 预设清单（界面列表用）。
pub fn presets() -> Vec<PresetTemplate> {
    PRESET_IDS
        .iter()
        .map(|id| PresetTemplate {
            id: (*id).to_owned(),
        })
        .collect()
}

/// 按用户配置生成。
pub fn generate_config(config: &mut NginxConfig, comments: &Comments<'_>) -> GenerateResult {
    apply_rules(config);

    if let Err(errors) = validate_config(config) {
        return GenerateResult {
            success: false,
            config: String::new(),
            errors,
            warnings: Vec::new(),
        };
    }

    GenerateResult {
        success: true,
        config: render(config, comments),
        errors: Vec::new(),
        warnings: Vec::new(),
    }
}

/// 用默认配置生成（源仓 `generate_default_config`）。
pub fn generate_default(comments: &Comments<'_>) -> GenerateResult {
    generate_config(&mut NginxConfig::default(), comments)
}

/// 预设 id → 配置本体；未知 id 回落默认（源仓 `generate_with_preset` 同语义）。
///
/// 单独暴露出来是给「自定义覆盖」用的：它需要**先拿到预设的配置**再打补丁。
pub fn preset_config(preset: &str) -> NginxConfig {
    match preset {
        "basic" => basic(),
        "reverse-proxy" => reverse_proxy(),
        "static-site" => static_site(),
        "java-app" => java_app(),
        "split-view" => split_view(),
        _ => NginxConfig::default(),
    }
}

/// 用预设生成。
pub fn generate_with_preset(preset: &str, comments: &Comments<'_>) -> GenerateResult {
    let mut config = preset_config(preset);
    generate_config(&mut config, comments)
}

/// 各组件的「与默认相同」部分靠 `..Default::default()` 收口，只写出预设真正改掉的字段。
fn base(port: &str, name: &str) -> ServerConfig {
    ServerConfig {
        listen_port: port.to_owned(),
        server_name: name.to_owned(),
        ..ServerConfig::default()
    }
}

fn upstream(name: &str, address: &str) -> UpstreamConfig {
    UpstreamConfig {
        name: name.to_owned(),
        servers: vec![UpstreamServer {
            address: address.to_owned(),
            ..UpstreamServer::default()
        }],
        ..UpstreamConfig::default()
    }
}

fn location(path: &str, kind: &str, target: &str, extra: &str) -> LocationConfig {
    LocationConfig {
        path: path.to_owned(),
        location_type: kind.to_owned(),
        target: target.to_owned(),
        extra_directives: extra.to_owned(),
        ..LocationConfig::default()
    }
}

/// 静态托管 + 一条基本反向代理。
fn basic() -> NginxConfig {
    NginxConfig {
        server: base("80", "_"),
        upstreams: vec![upstream("backend", "127.0.0.1:8080")],
        locations: vec![
            location("/api/", "proxy", "http://backend", ""),
            location("/", "root", "/var/www/html", ""),
        ],
        ssl: SslConfig::default(),
        security: SecurityConfig::default(),
        performance: PerformanceConfig::default(),
        logging: LoggingConfig::default(),
        error_pages: super::config::default_error_pages(),
        advanced: AdvancedConfig::default(),
    }
}

/// 前后端分离：API 反代 + 前端静态（history 模式回退）。
fn reverse_proxy() -> NginxConfig {
    NginxConfig {
        server: base("80", "_"),
        upstreams: vec![upstream("api_backend", "127.0.0.1:8080")],
        locations: vec![
            location("/api/", "proxy", "http://api_backend", ""),
            location(
                "/",
                "alias",
                "/var/www/static/",
                "try_files $uri $uri/ /index.html;",
            ),
        ],
        ssl: SslConfig::default(),
        security: SecurityConfig::default(),
        performance: PerformanceConfig::default(),
        logging: LoggingConfig::default(),
        error_pages: super::config::default_error_pages(),
        advanced: AdvancedConfig::default(),
    }
}

/// 纯静态：没有 upstream。
fn static_site() -> NginxConfig {
    NginxConfig {
        server: base("80", "_"),
        upstreams: vec![],
        locations: vec![location("/", "root", "/var/www/html", "")],
        ssl: SslConfig::default(),
        security: SecurityConfig::default(),
        performance: PerformanceConfig::default(),
        logging: LoggingConfig::default(),
        error_pages: super::config::default_error_pages(),
        advanced: AdvancedConfig::default(),
    }
}

/// Spring Boot / Tomcat：整站反代 + 四条转发头。
fn java_app() -> NginxConfig {
    // 第二行起的 8 空格缩进是**内容的一部分**（源仓原样如此）：
    // 渲染器只给第一行补缩进，其余行照搬用户/预设写的缩进。
    let headers = "proxy_set_header Host $host;\n        \
proxy_set_header X-Real-IP $remote_addr;\n        \
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n        \
proxy_set_header X-Forwarded-Proto $scheme;";
    NginxConfig {
        server: base("80", "_"),
        upstreams: vec![upstream("java_app", "127.0.0.1:8080")],
        locations: vec![location("/", "proxy", "http://java_app", headers)],
        ssl: SslConfig::default(),
        security: SecurityConfig::default(),
        performance: PerformanceConfig::default(),
        logging: LoggingConfig::default(),
        error_pages: super::config::default_error_pages(),
        advanced: AdvancedConfig::default(),
    }
}

/// 混合：一个 API 反代 + 一个目录浏览 + 一个下载根（源仓现场案例原样保留）。
fn split_view() -> NginxConfig {
    NginxConfig {
        server: base("8180", "localhost"),
        upstreams: vec![upstream("metal", "192.16.73.202:8281")],
        locations: vec![
            location("/metal/", "proxy", "http://metal", ""),
            LocationConfig {
                autoindex: true,
                ..location("/console", "alias", "/home/lcpt60/app/console/", "")
            },
            location("/server/", "root", "/home/lcpt60/upload/", ""),
        ],
        ssl: SslConfig::default(),
        security: SecurityConfig::default(),
        performance: PerformanceConfig::default(),
        logging: LoggingConfig::default(),
        error_pages: super::config::default_error_pages(),
        advanced: AdvancedConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_generates_valid_config() {
        for id in PRESET_IDS {
            let result = generate_with_preset(id, Comments::empty());
            assert!(result.success, "{id} 生成失败：{:?}", result.errors);
            assert!(result.config.contains("server {"), "{id} 没有 server 块");
        }
    }

    #[test]
    fn unknown_preset_falls_back_to_default() {
        let unknown = generate_with_preset("nope", Comments::empty());
        let default = generate_default(Comments::empty());
        assert_eq!(unknown.config, default.config);
    }

    #[test]
    fn static_site_has_no_upstream_block() {
        let text = generate_with_preset("static-site", Comments::empty()).config;
        assert!(!text.contains("upstream "));
    }

    #[test]
    fn java_app_carries_the_forwarding_headers() {
        let text = generate_with_preset("java-app", Comments::empty()).config;
        assert!(text.contains("proxy_set_header X-Forwarded-Proto $scheme;"));
        assert!(text.contains("proxy_pass http://java_app/;"));
    }

    #[test]
    fn static_site_preset_differs_from_basic_only_by_upstream() {
        let basic = generate_with_preset("basic", Comments::empty()).config;
        assert!(basic.contains("upstream backend"));
        assert!(basic.contains("location /api/"));
    }

    #[test]
    fn invalid_config_reports_errors_and_no_text() {
        let mut config = NginxConfig::default();
        config.server.listen_port = "abc".to_owned();
        let result = generate_config(&mut config, Comments::empty());
        assert!(!result.success);
        assert_eq!(result.errors, vec![super::super::NginxError::PortNotNumber]);
        assert!(result.config.is_empty());
    }

    #[test]
    fn presets_list_follows_the_reference_order() {
        let ids: Vec<String> = presets().into_iter().map(|preset| preset.id).collect();
        assert_eq!(ids, PRESET_IDS.map(str::to_owned).to_vec());
        assert_eq!(PRESET_IDS[0], "basic");
    }
}
