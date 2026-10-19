//! 配置结构体与默认值（源仓 `nginx/config.rs`，逐字段照抄）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NginxConfig {
    pub server: ServerConfig,
    pub upstreams: Vec<UpstreamConfig>,
    pub locations: Vec<LocationConfig>,
    pub ssl: SslConfig,
    pub security: SecurityConfig,
    pub performance: PerformanceConfig,
    pub logging: LoggingConfig,
    pub error_pages: Vec<ErrorPage>,
    pub advanced: AdvancedConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen_port: String,
    pub server_name: String,
    pub root: String,
    pub index_files: String,
    pub charset: String,
    pub extra_server_directives: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UpstreamConfig {
    pub name: String,
    pub servers: Vec<UpstreamServer>,
    pub method: String,
    pub keepalive: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UpstreamServer {
    pub address: String,
    pub weight: String,
    pub backup: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LocationConfig {
    pub path: String,
    pub location_type: String,
    pub target: String,
    pub proxy_buffer_size: String,
    pub proxy_buffers: String,
    pub proxy_read_timeout: String,
    pub proxy_connect_timeout: String,
    pub autoindex: bool,
    pub extra_directives: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SslConfig {
    pub enabled: bool,
    pub certificate_path: String,
    pub certificate_key_path: String,
    pub trusted_certificate_path: String,
    pub protocols: Vec<String>,
    pub ciphers: String,
    pub prefer_server_ciphers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SecurityConfig {
    pub server_tokens: bool,
    pub hsts: bool,
    pub hsts_max_age: String,
    pub hsts_include_subdomains: bool,
    pub x_frame_options: String,
    pub x_content_type_options: bool,
    pub x_xss_protection: String,
    pub referrer_policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PerformanceConfig {
    pub gzip: bool,
    pub gzip_types: Vec<String>,
    pub gzip_min_length: String,
    pub gzip_comp_level: String,
    pub keepalive_timeout: String,
    pub client_max_body_size: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LoggingConfig {
    pub access_log: bool,
    pub access_log_path: String,
    pub error_log: bool,
    pub error_log_path: String,
    pub error_log_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ErrorPage {
    pub code: String,
    pub uri: String,
}

impl Default for ErrorPage {
    fn default() -> Self {
        Self {
            code: "500".to_owned(),
            uri: "/50x.html".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AdvancedConfig {
    pub worker_processes: String,
    pub worker_connections: String,
    pub extra_http_directives: String,
    pub extra_events_directives: String,
}

impl Default for NginxConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            upstreams: vec![UpstreamConfig::default()],
            locations: vec![LocationConfig::default()],
            ssl: SslConfig::default(),
            security: SecurityConfig::default(),
            performance: PerformanceConfig::default(),
            logging: LoggingConfig::default(),
            error_pages: default_error_pages(),
            advanced: AdvancedConfig::default(),
        }
    }
}

/// 四个 50x 错误页（源仓在 `Default` 与 5 份预设里各抄了一遍，这里收成一处）。
pub(crate) fn default_error_pages() -> Vec<ErrorPage> {
    ["500", "502", "503", "504"]
        .into_iter()
        .map(|code| ErrorPage {
            code: code.to_owned(),
            uri: "/50x.html".to_owned(),
        })
        .collect()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_port: "80".to_owned(),
            server_name: "_".to_owned(),
            root: "/var/www/html".to_owned(),
            index_files: "index.html index.htm".to_owned(),
            charset: "utf-8".to_owned(),
            extra_server_directives: String::new(),
        }
    }
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            name: "backend".to_owned(),
            servers: vec![UpstreamServer::default()],
            method: "round-robin".to_owned(),
            keepalive: "32".to_owned(),
        }
    }
}

impl Default for UpstreamServer {
    fn default() -> Self {
        Self {
            address: "127.0.0.1:8080".to_owned(),
            weight: "1".to_owned(),
            backup: false,
        }
    }
}

impl Default for LocationConfig {
    fn default() -> Self {
        Self {
            path: "/".to_owned(),
            location_type: "root".to_owned(),
            target: "/var/www/html".to_owned(),
            proxy_buffer_size: "4096k".to_owned(),
            proxy_buffers: "16 4096k".to_owned(),
            proxy_read_timeout: "600".to_owned(),
            proxy_connect_timeout: "60".to_owned(),
            autoindex: false,
            extra_directives: String::new(),
        }
    }
}

impl Default for SslConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            certificate_path: "/etc/nginx/ssl/cert.pem".to_owned(),
            certificate_key_path: "/etc/nginx/ssl/key.pem".to_owned(),
            trusted_certificate_path: "/etc/nginx/ssl/chain.pem".to_owned(),
            protocols: vec!["TLSv1.2".to_owned(), "TLSv1.3".to_owned()],
            ciphers: "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384".to_owned(),
            prefer_server_ciphers: true,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            server_tokens: false,
            hsts: false,
            hsts_max_age: "31536000".to_owned(),
            hsts_include_subdomains: true,
            x_frame_options: "SAMEORIGIN".to_owned(),
            x_content_type_options: true,
            x_xss_protection: "1; mode=block".to_owned(),
            referrer_policy: "strict-origin-when-cross-origin".to_owned(),
        }
    }
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            gzip: true,
            gzip_types: vec![
                "text/plain".to_owned(),
                "text/css".to_owned(),
                "text/xml".to_owned(),
                "text/javascript".to_owned(),
                "application/json".to_owned(),
                "application/javascript".to_owned(),
                "application/xml".to_owned(),
                "application/rss+xml".to_owned(),
                "application/atom+xml".to_owned(),
                "image/svg+xml".to_owned(),
            ],
            gzip_min_length: "256".to_owned(),
            gzip_comp_level: "5".to_owned(),
            keepalive_timeout: "65".to_owned(),
            client_max_body_size: "64M".to_owned(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            access_log: true,
            access_log_path: "/var/log/nginx/access.log".to_owned(),
            error_log: true,
            error_log_path: "/var/log/nginx/error.log".to_owned(),
            error_log_level: "warn".to_owned(),
        }
    }
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            worker_processes: "auto".to_owned(),
            worker_connections: "1024".to_owned(),
            extra_http_directives: String::new(),
            extra_events_directives: String::new(),
        }
    }
}

/// 生成结果（源仓同名字段一一对应）。
///
/// 不带 serde：`errors` 是稳定码枚举（[`crate::nginx::validator::NginxError`]），
/// 序列化成 JSON 反而会把它压成不可读的元组，插件层直接读字段更直接。
#[derive(Debug, Clone)]
pub struct GenerateResult {
    pub success: bool,
    pub config: String,
    /// 校验失败项。源仓是 `Vec<String>`（中文句子），这里换成稳定码，
    /// 整句由插件层翻译（见 [`crate::nginx::validator::NginxError`]）。
    pub errors: Vec<crate::nginx::validator::NginxError>,
    pub warnings: Vec<String>,
}

/// 预设模板：**只有 id**。
///
/// 源仓这里带 `name` / `description`（「基础配置」/「反向代理」…）。内核不产中文，
/// 所以名字与说明留在插件译文表里（`locales/en.toml` 的中文源串就是键），
/// 渲染期按 id 取词 —— 顺便让英文界面能显示英文模板名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetTemplate {
    pub id: String,
}
