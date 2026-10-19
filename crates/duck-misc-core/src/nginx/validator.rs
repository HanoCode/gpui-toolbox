//! 校验与「改前先补默认」（源仓 `nginx/validator.rs`，规则逐条照抄）。
//!
//! 与源仓的唯一差别：错误的正文不再是一句中文，而是 [`NginxError`] 稳定码 +
//! 细节（哪个 upstream、哪个 location）。整句在插件层拼，中文 / 英文各一套。

use super::config::NginxConfig;

/// 一条校验失败。`code()` 是稳定 ASCII 串，`detail()` 是**数据**（不翻译）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NginxError {
    /// 监听端口为空。
    PortEmpty,
    /// 端口不在 1-65535。
    PortRange,
    /// 端口不是数字。
    PortNotNumber,
    /// Server Name 为空。
    ServerNameEmpty,
    /// 网站根目录为空。
    RootEmpty,
    /// 第 n 个 Upstream 没有名字（1 起）。
    UpstreamNameEmpty(usize),
    /// 某个 Upstream 一个服务器都没有。
    UpstreamNoServer(String),
    /// 第 n 个 Location 没有路径（1 起）。
    LocationPathEmpty(usize),
    /// 某个 Location 没有目标。
    LocationTargetEmpty(String),
    /// 反向代理的目标不是 http(s):// 开头。
    ProxyTargetScheme(String),
    /// 开了 SSL 但没给证书路径。
    CertificateEmpty,
    /// 开了 SSL 但没给私钥路径。
    CertificateKeyEmpty,
    /// worker_connections 不是数字。
    WorkersNotNumber,
    /// worker_connections 不在 1-65535。
    WorkersRange,
}

impl NginxError {
    /// 稳定码：日志与测试用，不随语言变。
    pub fn code(&self) -> &'static str {
        match self {
            Self::PortEmpty => "port-empty",
            Self::PortRange => "port-range",
            Self::PortNotNumber => "port-not-number",
            Self::ServerNameEmpty => "server-name-empty",
            Self::RootEmpty => "root-empty",
            Self::UpstreamNameEmpty(_) => "upstream-name-empty",
            Self::UpstreamNoServer(_) => "upstream-no-server",
            Self::LocationPathEmpty(_) => "location-path-empty",
            Self::LocationTargetEmpty(_) => "location-target-empty",
            Self::ProxyTargetScheme(_) => "proxy-target-scheme",
            Self::CertificateEmpty => "certificate-empty",
            Self::CertificateKeyEmpty => "certificate-key-empty",
            Self::WorkersNotNumber => "workers-not-number",
            Self::WorkersRange => "workers-range",
        }
    }

    /// 细节：序号或名字，直接拼进译文。
    ///
    /// 序号在这里就 +1 转成「人看的第几个」，插件层不用再算。
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::UpstreamNameEmpty(index) | Self::LocationPathEmpty(index) => {
                Some(index.to_string())
            }
            Self::UpstreamNoServer(name)
            | Self::LocationTargetEmpty(name)
            | Self::ProxyTargetScheme(name) => Some(name.clone()),
            _ => None,
        }
    }
}

/// 一次性跑完全部规则，按出现顺序返回（源仓同样一次收集全部）。
pub fn validate_config(config: &NginxConfig) -> Result<(), Vec<NginxError>> {
    let mut errors = Vec::new();

    if config.server.listen_port.is_empty() {
        errors.push(NginxError::PortEmpty);
    } else if let Ok(port) = config.server.listen_port.parse::<u16>() {
        // `u16` 解析已经保证 ≤ 65535，源仓那句 `> 65535` 是死代码，这里只留 0。
        if port == 0 {
            errors.push(NginxError::PortRange);
        }
    } else {
        errors.push(NginxError::PortNotNumber);
    }

    if config.server.server_name.is_empty() {
        errors.push(NginxError::ServerNameEmpty);
    }
    if config.server.root.is_empty() {
        errors.push(NginxError::RootEmpty);
    }

    for (index, upstream) in config.upstreams.iter().enumerate() {
        if upstream.name.is_empty() {
            errors.push(NginxError::UpstreamNameEmpty(index + 1));
        }
        if upstream.servers.is_empty() {
            errors.push(NginxError::UpstreamNoServer(upstream.name.clone()));
        }
    }

    for (index, location) in config.locations.iter().enumerate() {
        if location.path.is_empty() {
            errors.push(NginxError::LocationPathEmpty(index + 1));
        }
        if location.target.is_empty() {
            errors.push(NginxError::LocationTargetEmpty(location.path.clone()));
        }
        if location.location_type == "proxy"
            && !location.target.starts_with("http://")
            && !location.target.starts_with("https://")
        {
            errors.push(NginxError::ProxyTargetScheme(location.path.clone()));
        }
    }

    if config.ssl.enabled {
        if config.ssl.certificate_path.is_empty() {
            errors.push(NginxError::CertificateEmpty);
        }
        if config.ssl.certificate_key_path.is_empty() {
            errors.push(NginxError::CertificateKeyEmpty);
        }
    }

    match config.advanced.worker_connections.parse::<u32>() {
        Ok(connections) if (1..=65535).contains(&connections) => {}
        Ok(_) => errors.push(NginxError::WorkersRange),
        Err(_) => errors.push(NginxError::WorkersNotNumber),
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// 渲染前的兜底补全（源仓 `apply_rules`，规则逐条照抄）。
pub fn apply_rules(config: &mut NginxConfig) {
    // 源仓的口径：开了 SSL 就自动开 HSTS —— 不强制等于把安全开关做成默认关。
    if config.ssl.enabled && !config.security.hsts {
        config.security.hsts = true;
    }

    for location in &mut config.locations {
        if location.proxy_buffer_size.is_empty() {
            location.proxy_buffer_size = "4096k".to_owned();
        }
        if location.proxy_buffers.is_empty() {
            location.proxy_buffers = "16 4096k".to_owned();
        }
        if location.proxy_read_timeout.is_empty() {
            location.proxy_read_timeout = "600".to_owned();
        }
        if location.proxy_connect_timeout.is_empty() {
            location.proxy_connect_timeout = "60".to_owned();
        }
    }

    if config.performance.gzip_types.is_empty() {
        config.performance.gzip_types = vec![
            "text/plain".to_owned(),
            "text/css".to_owned(),
            "text/xml".to_owned(),
            "text/javascript".to_owned(),
            "application/json".to_owned(),
            "application/javascript".to_owned(),
        ];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(config: &NginxConfig) -> Vec<&'static str> {
        validate_config(config)
            .unwrap_err()
            .iter()
            .map(NginxError::code)
            .collect()
    }

    #[test]
    fn default_config_is_valid() {
        assert!(validate_config(&NginxConfig::default()).is_ok());
    }

    #[test]
    fn port_rules_match_the_reference() {
        let mut config = NginxConfig::default();
        config.server.listen_port = String::new();
        assert_eq!(codes(&config), vec!["port-empty"]);

        config.server.listen_port = "0".to_owned();
        assert_eq!(codes(&config), vec!["port-range"]);

        config.server.listen_port = "http".to_owned();
        assert_eq!(codes(&config), vec!["port-not-number"]);

        // 65535 是合法的；65536 连 `u16` 都装不下，走「不是数字」那条。
        config.server.listen_port = "65535".to_owned();
        assert!(validate_config(&config).is_ok());
        config.server.listen_port = "65536".to_owned();
        assert_eq!(codes(&config), vec!["port-not-number"]);
    }

    #[test]
    fn upstream_and_location_details_carry_numbers_and_names() {
        let mut config = NginxConfig::default();
        config.upstreams[0].name = String::new();
        config.upstreams[0].servers.clear();
        config.locations[0].path = String::new();
        let errors = validate_config(&config).unwrap_err();
        assert_eq!(errors[0], NginxError::UpstreamNameEmpty(1));
        assert_eq!(errors[0].detail().as_deref(), Some("1"));
        assert_eq!(errors[1], NginxError::UpstreamNoServer(String::new()));
        assert_eq!(errors[2], NginxError::LocationPathEmpty(1));
    }

    #[test]
    fn proxy_target_must_be_http() {
        let mut config = NginxConfig::default();
        config.locations[0].location_type = "proxy".to_owned();
        config.locations[0].target = "backend".to_owned();
        assert_eq!(codes(&config), vec!["proxy-target-scheme"]);
        config.locations[0].target = "http://backend".to_owned();
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn ssl_requires_both_files_and_forces_hsts() {
        let mut config = NginxConfig::default();
        config.ssl.enabled = true;
        config.ssl.certificate_path = String::new();
        config.ssl.certificate_key_path = String::new();
        assert_eq!(
            codes(&config),
            vec!["certificate-empty", "certificate-key-empty"]
        );

        config.ssl.certificate_path = "/c.pem".to_owned();
        config.ssl.certificate_key_path = "/k.pem".to_owned();
        apply_rules(&mut config);
        assert!(config.security.hsts, "开了 SSL 就该自动开 HSTS");
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn worker_connections_bounds() {
        let mut config = NginxConfig::default();
        config.advanced.worker_connections = "0".to_owned();
        assert_eq!(codes(&config), vec!["workers-range"]);
        config.advanced.worker_connections = "65536".to_owned();
        assert_eq!(codes(&config), vec!["workers-range"]);
        config.advanced.worker_connections = "auto".to_owned();
        assert_eq!(codes(&config), vec!["workers-not-number"]);
    }

    #[test]
    fn apply_rules_fills_empty_proxy_buffers_only() {
        let mut config = NginxConfig::default();
        config.locations[0].proxy_buffer_size = String::new();
        config.locations[0].proxy_buffers = "8 8k".to_owned();
        apply_rules(&mut config);
        assert_eq!(config.locations[0].proxy_buffer_size, "4096k");
        assert_eq!(
            config.locations[0].proxy_buffers, "8 8k",
            "非空值不许被覆盖"
        );
    }
}
