//! gpui 的 HTTP 客户端（ureq 后端）。
//!
//! `gpui_platform::application()` 在桌面装配路径上**不接 HTTP 客户端**——
//! `Application::with_platform` 写死 `NullHttpClient`（gpui `app.rs:182`），
//! 所有走 `cx.http_client()` 的能力当场失败。最典型的受害者就是邮箱正文里的
//! **远程图片**：`ImageAssetLoader` 用 `cx.http_client().get(uri)` 取图，日志里刷
//! `Failed to load asset: Other(loading image asset from …)`，正文里图片位置全空。
//!
//! 这里用**已在依赖图内**的 `ureq`（`Cargo.toml` §3.2，duck-db / duck-mail 已在用）
//! 补一个最小实现，**不把 reqwest 请回依赖图**——`[patch.crates-io]` 注释写明
//! 「reqwest 随旧依赖图整体退出」是有意为之。
//!
//! # 阻塞与执行器
//!
//! gpui-fast 的 background executor **不是 tokio**（tokio 只出现在它自带的
//! `reqwest_client` 里），`tokio::task::spawn_blocking` 在这里会 panic。阻塞式
//! ureq 与 gpui 异步面的桥接走「线程 + oneshot」：每条请求起一个一次性线程，
//! 结果经 `futures::channel::oneshot` 送回。图片请求量级是「一封几条」，代价可忽略。
//!
//! # 已知取舍
//!
//! - **重定向一律跟随**：`Request` 里的 `RedirectPolicy` 扩展被忽略。调用面
//!   （远程图片 GET）全都要跟随，没有反例。
//! - **流式请求体不支持**：`AsyncBody::AsyncReader` 会报错。现网调用都是空体 /
//!   小体量字节，没有流式来源。

use anyhow::anyhow;
use futures_channel::oneshot;
use futures_util::future::BoxFuture;
use gpui::http_client::{AsyncBody, HttpClient, Inner, http};
use std::io::Read as _;

/// 基于 `ureq` 的 [`HttpClient`] 实现。见模块头。
pub struct UreqHttpClient {
    agent: ureq::Agent,
    user_agent: http::header::HeaderValue,
}

impl UreqHttpClient {
    /// 建一个默认客户端：连接 15s、整请求 60s 超时（图片尺寸的上界）。
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(std::time::Duration::from_secs(15)))
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            user_agent: http::header::HeaderValue::from_static(concat!(
                "Duck/",
                env!("CARGO_PKG_VERSION")
            )),
        }
    }
}

impl Default for UreqHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// 把 gpui 递进来的请求体读成字节。
///
/// `AsyncReader` 分支直接报错（见模块头「已知取舍」）：本仓的调用面没有流式来源，
/// 与其默默读挂不如把话说清楚。两个受支持的分支都是纯同步读，不需要 async。
fn body_bytes(body: AsyncBody) -> anyhow::Result<Vec<u8>> {
    match body.0 {
        Inner::Empty => Ok(Vec::new()),
        Inner::Bytes(cursor) => {
            let bytes = cursor.into_inner();
            Ok(bytes.as_ref().to_vec())
        }
        Inner::AsyncReader(_) => Err(anyhow!(
            "streaming request bodies are not supported by the ureq http client"
        )),
    }
}

impl HttpClient for UreqHttpClient {
    fn user_agent(&self) -> Option<&http::header::HeaderValue> {
        Some(&self.user_agent)
    }

    fn proxy(&self) -> Option<&gpui::http_client::Url> {
        // 系统代理读环境变量的路径由 `HttpClientWithProxy` 包一层实现；这里不做。
        None
    }

    fn send(
        &self,
        mut req: http::Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<http::Response<AsyncBody>>> {
        // 请求体先行读成字节：跨线程后无法再借 `AsyncBody`。同步读，失败直接
        // 折进返回的 future，不必起线程。
        let body = match body_bytes(std::mem::take(req.body_mut())) {
            Ok(bytes) => bytes,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let (parts, _) = req.into_parts();
        let agent = self.agent.clone();

        let (tx, rx) = oneshot::channel();
        let spawn = std::thread::Builder::new()
            .name("duck-http".into())
            .spawn(move || {
                let outcome = (|| -> anyhow::Result<http::Response<AsyncBody>> {
                    let mut request = http::Request::builder()
                        .method(parts.method.clone())
                        .uri(parts.uri.clone());
                    for (name, value) in parts.headers.iter() {
                        request = request.header(name, value);
                    }
                    let request = request.body(body)?;
                    let mut response = agent
                        .run(request)
                        .map_err(|e| anyhow!("http request failed: {e}"))?;
                    let mut bytes = Vec::new();
                    response
                        .body_mut()
                        .as_reader()
                        .read_to_end(&mut bytes)
                        .map_err(|e| anyhow!("reading http response failed: {e}"))?;

                    let mut builder = http::Response::builder().status(response.status());
                    for (name, value) in response.headers().iter() {
                        builder = builder.header(name, value);
                    }
                    builder
                        .body(AsyncBody::from(bytes))
                        .map_err(|e| anyhow!("rebuilding http response failed: {e}"))
                })();
                let _ = tx.send(outcome);
            });
        Box::pin(async move {
            // 发送线程失败（只在进程退出时发生）与对端丢弃等价。
            match spawn {
                Ok(handle) => {
                    let _ = handle;
                }
                Err(error) => return Err(anyhow!("spawning http worker failed: {error}")),
            }
            rx.await
                .map_err(|_| anyhow!("http worker dropped the response"))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `body_bytes` 的两条受支持分支：空体 / 内存字节。流式体按口径报错。
    #[test]
    fn body_bytes_handles_the_supported_variants() {
        assert_eq!(body_bytes(AsyncBody::empty()).unwrap(), Vec::<u8>::new());
        assert_eq!(
            body_bytes(AsyncBody::from(Vec::from(b"hi" as &[u8]))).unwrap(),
            b"hi".to_vec()
        );
    }

    /// 客户端能对 `HttpClient` trait object 化（trait 约束满足的最小证明）。
    #[test]
    fn client_implements_http_client() {
        fn assert_impl<T: HttpClient>(_: &T) {}
        assert_impl(&UreqHttpClient::new());
    }
}
