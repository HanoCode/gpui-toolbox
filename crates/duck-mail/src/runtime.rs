//! 内嵌 tokio runtime —— **本 crate 唯一允许出现 tokio 的地方**。
//!
//! 异步模型沿用 `duck-db` 的「方案 B」（`docs/database-design.md` §4.1、
//! `docs/email-design.md` §4.1）：邮件必须异步（IMAP 常驻连接 + IDLE 推送 + 多账号
//! 并发，同步阻塞会把线程吃光），但**本仓的调用方是 GPUI 的
//! `background_executor`，不是 async 运行时**。
//!
//! 所以这里起一个自己的 runtime，把 async 世界关在 crate 内，对外只暴露：
//!
//! - [`MailRuntime::spawn`] → [`Task<T>`]：投一个 future，拿到一个可 `wait()` 的句柄；
//! - [`Task`]：`wait()` 阻塞到结果（**只能在非 UI 线程调用**）、`try_wait()` 轮询。
//!
//! **纪律**：`tokio::` 类型不得出现在任何 `pub fn` 的签名里。`Cargo.toml` 的注释与
//! 这条纪律是配套的——CI 的架构守卫会扫 `pub` 面上的 `tokio::`（照 `duck-db` 先例）。
//!
//! ```text
//! duck-app/pages/mail.rs（UI，gpui）
//!   │  background_executor().spawn(async move { core.sync_once(id)?.wait() })
//!   ▼
//! duck-mail::runtime（本文件：MailRuntime + Task）
//!   ▼
//! async-imap / rustls（真正的异步世界）
//! ```

use std::future::Future;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use crate::error::{MailError, Result};

/// 一个已经投出去的异步任务。
///
/// 通道里跑的是 future 的**原始输出** `T`——调用方想要 `Result` 就自己把 `T` 选成
/// `Result<U>`。这样 `Task<U>` 与 `Task<Result<U>>` 都能表达，不必为「带错误的任务」
/// 和「不带错误的任务」各造一个类型。
///
/// 内部是 `std::sync::mpsc`——**刻意不用 `tokio::sync::oneshot`**：后者的 `Receiver`
/// 类型会跟着 `Task` 一起出现在公共 API 上，而 `Task` 是要给 GPUI 页面用的。
/// 用标准库通道，`Task` 的具体形状与 tokio 无关。
pub struct Task<T> {
    rx: Receiver<T>,
}

impl<T> Task<T> {
    /// 阻塞等待结果。
    ///
    /// ⚠️ **绝不能在 UI 线程调用**——会把界面卡到任务结束。调用方是
    /// `background_executor` 里的阻塞上下文（`duck-db` / `duck-redis` 同一口径）。
    ///
    /// 通道断开（任务 panic 或发送端被丢弃）映射成错误——Runtime 会吞掉 panic 的
    /// unwinding，只留下一个关闭的通道，这是唯一能察觉到的痕迹。
    pub fn wait(self) -> Result<T> {
        self.rx
            .recv()
            .map_err(|_| MailError::Other("mail task was dropped without a result".into()))
    }

    /// 限时等待。超时返回 `None`，任务本身不受影响（仍在跑）。
    pub fn wait_timeout(self, timeout: Duration) -> Option<Result<T>> {
        match self.rx.recv_timeout(timeout) {
            Ok(v) => Some(Ok(v)),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Err(MailError::Other(
                "mail task was dropped without a result".into(),
            ))),
        }
    }

    /// 非阻塞轮询。任务没结束返回 `None`。
    pub fn try_wait(&self) -> Option<Result<T>> {
        match self.rx.try_recv() {
            Ok(v) => Some(Ok(v)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(MailError::Other(
                "mail task was dropped without a result".into(),
            ))),
        }
    }
}

/// 邮件内核的异步执行器。惰性创建、跨线程共享。
#[derive(Clone)]
pub struct MailRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    rt: tokio::runtime::Runtime,
}

impl MailRuntime {
    /// 建一个 runtime。
    ///
    /// worker 数取 **2**：一条给主同步 actor、一条给回填 / IDLE 之外的工作，
    /// 再多也没有活可干（`duck-mail` 的并发瓶颈在 IMAP 连接数，不在 CPU）。
    /// 多留 worker 只会多占几个线程栈。
    ///
    /// 第一次用到才建（`MailCore::new` 里建的是 `Arc<OnceLock>` 语义的惰性对象，
    /// 见 `MailCore`）。单纯的「打开账户列表」不该付出建 runtime 的代价。
    pub fn new() -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("duck-mail")
            .enable_all()
            .build()
            .map_err(|e| MailError::Other(format!("cannot start mail runtime: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner { rt }),
        })
    }

    /// 投一个 future，拿到 [`Task`]。
    pub fn spawn<F>(&self, future: F) -> Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.inner.rt.spawn(async move {
            // 接收端可能已经走了（UI 关掉了页面），发送失败不算错误。
            let _ = tx.send(future.await);
        });
        Task { rx }
    }

    /// 直接在 runtime 上跑一个 future 并等结果（便捷包装，等价于 `spawn(..).wait()`）。
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        self.inner.rt.block_on(future)
    }
}

impl std::fmt::Debug for MailRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Runtime` 没实现 Debug；只报告「有一个存活的 runtime」。
        f.debug_struct("MailRuntime").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn spawn_and_wait_returns_value() {
        let rt = MailRuntime::new().unwrap();
        let task = rt.spawn(async { 6 * 7 });
        assert_eq!(task.wait().unwrap(), 42);
    }

    #[test]
    fn spawn_propagates_errors() {
        let rt = MailRuntime::new().unwrap();
        let task = rt.spawn(async { Err::<i32, _>(MailError::Timeout("idle".into())) });
        // `T` 是 `Result<_, MailError>`，所以 outer `Result` 成功、inner 是那个错误。
        let inner = task.wait().unwrap();
        assert!(matches!(inner, Err(MailError::Timeout(_))));
    }

    #[test]
    fn concurrent_tasks_run_on_the_pool() {
        let rt = MailRuntime::new().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let c = counter.clone();
            tasks.push(rt.spawn(async move {
                c.fetch_add(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.wait().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 16);
    }

    #[test]
    fn wait_timeout_gives_up_without_killing_the_task() {
        let rt = MailRuntime::new().unwrap();
        let task = rt.spawn(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            "done"
        });
        // 立刻问：还没好。
        assert!(task.try_wait().is_none());
        // 限时等到——任务照常跑完。
        assert_eq!(
            task.wait_timeout(Duration::from_secs(2)).unwrap().unwrap(),
            "done"
        );
    }

    #[test]
    fn runtime_is_cloneable_and_shared() {
        let rt = MailRuntime::new().unwrap();
        let rt2 = rt.clone();
        let task = rt2.spawn(async { "shared" });
        assert_eq!(task.wait().unwrap(), "shared");
    }
}
