//! 取索引（以及归档）：走 HTTPS，带大小上限与超时。
//!
//! # 为什么是阻塞式客户端
//!
//! 与 `duck-update` 同一取舍：调用点只会在后台执行器里跑它，永远不在 UI 线程上。
//! 为一个索引请求拖进一个异步运行时是错的取舍。
//!
//! # 超时按「一次没人盯着的请求」定
//!
//! 索引只有几 KB，给它 30 秒；归档可能几 MB，给 10 分钟。两者都设了**体积上限**——
//! 真正的上限是大小而不是耐心。

use std::time::Duration;

use crate::error::RegistryError;

/// 索引大小上限：再大也不该超过这个数（挡住错误的重定向把巨大 body 读进来）。
pub(crate) const INDEX_LIMIT: u64 = 4 * 1024 * 1024;

/// 索引请求的时间预算。
const INDEX_TIMEOUT: Duration = Duration::from_secs(30);

/// 建连超时。
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 归档传输的时间预算。
pub(crate) const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

const USER_AGENT: &str = concat!(
    "DuckToolbox/",
    env!("CARGO_PKG_VERSION"),
    " (plugin-registry)"
);

/// 取一份索引的原始字节。
///
/// # Errors
/// 非 https 地址、网络失败、非 2xx 状态码、或超出 [`INDEX_LIMIT`] 都返回
/// [`RegistryError::Network`]。
pub fn fetch_index(url: &str) -> Result<Vec<u8>, RegistryError> {
    if !url.starts_with("https://") {
        return Err(RegistryError::Network(format!("not an https url: {url:?}")));
    }

    let agent = agent(INDEX_TIMEOUT);
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| RegistryError::Network(error.to_string()))?;

    response
        .body_mut()
        .with_config()
        .limit(INDEX_LIMIT)
        .read_to_vec()
        .map_err(|error| RegistryError::Network(error.to_string()))
}

/// 建一个只走 HTTPS 的 agent。
///
/// 每次调用建一个：索引与插件包都是稀罕操作，为此养一个长连接池不划算，也就没有
/// 共享状态需要管。`https_only` 让重定向也逃不掉。
pub(crate) fn agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .user_agent(USER_AGENT)
        .build();
    ureq::Agent::new_with_config(config)
}
