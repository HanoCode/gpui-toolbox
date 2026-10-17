//! 进度事件：安装 / 更新 / 卸载过程中把「现在到哪一步了」送回界面。
//!
//! # 为什么是 `mpsc` + 宿主侧节流
//!
//! 这套口径与 `duck-mail` 一致：内核把事件推进标准库 `mpsc`，宿主在
//! `background_executor` 的定时器里 `try_recv` **批量** drain，再统一 `notify`。
//! 直接在每个事件里 `cx.notify()` 会让一个 20 MB 的下载触发几百次重绘 —— 下载
//! 传输本身就是几百个块。
//!
//! `Sender` 断开（界面关了、页面切走了）不是错误：`send` 失败就当事件没人要，
//! 安装照常跑完。这是有意的——**下载不该因为没人看进度而中断**。
//!
//! # 步骤是枚举，不是字符串
//!
//! dbx 那边 `step` 是字符串，界面自己映射成文案；我们这次把它做成枚举，让界面
//! 的 `match` 由编译器盯着（少一个分支就编译不过），文案照旧由界面 `trf`。

use std::sync::mpsc::Sender;

/// 安装流程里的步骤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressStep {
    /// 正在拉注册表。
    Registry,
    /// 正在下 JRE（jar 轨驱动首次安装时可能需要）。
    JreDownload,
    /// 正在下工件。
    Download,
    /// 正在校验摘要。
    Verify,
    /// 正在解包。
    Extract,
    /// 正在落位（替换旧版本）。
    Install,
    /// 正在删除。
    Remove,
    /// 该驱动的流程结束（成功或失败都发一条，界面据此清掉「进行中」）。
    Finished,
}

impl ProgressStep {
    /// 落盘 / 日志用的稳定名。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::JreDownload => "jre",
            Self::Download => "download",
            Self::Verify => "verify",
            Self::Extract => "extract",
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Finished => "finished",
        }
    }
}

/// 一条进度事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverEvent {
    /// 哪个驱动（注册表键；JRE 步骤时是该 JRE 键）。
    pub key: String,
    /// 现在到哪一步。
    pub step: ProgressStep,
    /// 已传字节（只有下载类步骤有）。
    pub downloaded: Option<u64>,
    /// 总字节（只有下载类步骤有）。
    pub total: Option<u64>,
    /// 批量操作里的第几个（1 起）。
    pub index: Option<u32>,
    /// 批量操作的总数。
    pub count: Option<u32>,
    /// 失败原因（内核英文；只有 `Finished` 可能带）。
    pub error: Option<String>,
}

impl DriverEvent {
    /// 一条不带字节数的步骤事件。
    #[must_use]
    pub fn step(key: impl Into<String>, step: ProgressStep) -> Self {
        Self {
            key: key.into(),
            step,
            downloaded: None,
            total: None,
            index: None,
            count: None,
            error: None,
        }
    }

    /// 一条带字节数的下载事件。
    #[must_use]
    pub fn transfer(
        key: impl Into<String>,
        step: ProgressStep,
        downloaded: u64,
        total: u64,
    ) -> Self {
        Self {
            key: key.into(),
            step,
            downloaded: Some(downloaded),
            total: Some(total),
            index: None,
            count: None,
            error: None,
        }
    }

    /// 给事件标注它在批量操作里的位置。
    #[must_use]
    pub fn at(mut self, index: u32, count: u32) -> Self {
        self.index = Some(index);
        self.count = Some(count);
        self
    }

    /// 给事件附上失败原因。
    #[must_use]
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }
}

/// 进度接收口。
///
/// 内核拿它推事件；界面断了（`send` 失败）也不影响流程继续。可选带一层**改写**
/// （[`EventSink::decorate`]），批量操作靠它给内部产生的事件统一打上「第几个 / 共几个」。
#[derive(Clone)]
pub struct EventSink {
    sender: Option<Sender<DriverEvent>>,
    decorate: Option<std::sync::Arc<dyn Fn(DriverEvent) -> DriverEvent + Send + Sync>>,
}

impl Default for EventSink {
    fn default() -> Self {
        Self::silent()
    }
}

impl EventSink {
    /// 不关心进度的调用方（测试、脚本）用这个。
    #[must_use]
    pub fn silent() -> Self {
        Self {
            sender: None,
            decorate: None,
        }
    }

    /// 从通道建一个。
    #[must_use]
    pub fn new(sender: Sender<DriverEvent>) -> Self {
        Self {
            sender: Some(sender),
            decorate: None,
        }
    }

    /// 推一条；没人收就当没发生。
    pub fn send(&self, event: DriverEvent) {
        let event = match &self.decorate {
            Some(decorate) => decorate(event),
            None => event,
        };
        if let Some(sender) = &self.sender
            && sender.send(event).is_err()
        {
            tracing::debug!("driver event receiver is gone; dropping the event");
        }
    }

    /// 加一层改写（批量操作打位置用）。重复调用时**后者覆盖前者**。
    ///
    /// 为什么需要它：批量路径里绝大多数事件（下载进度、解包……）是在**被调用方内部**
    /// 生成的，调用方拿不到，也就没法一条条 `.at(..)`；包装一层之后，位置信息对
    /// 内部产生的每一条都成立。
    #[must_use]
    pub fn decorate<F>(&self, decorate: F) -> Self
    where
        F: Fn(DriverEvent) -> DriverEvent + Send + Sync + 'static,
    {
        Self {
            sender: self.sender.clone(),
            decorate: Some(std::sync::Arc::new(decorate)),
        }
    }
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSink")
            .field("connected", &self.sender.is_some())
            .field("decorated", &self.decorate.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没人接收时 `send` 静默丢弃 —— 下载不该因为界面关了而失败。
    #[test]
    fn a_dropped_receiver_does_not_panic_or_fail() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let sink = EventSink::new(sender);
        drop(receiver);
        sink.send(DriverEvent::step("dameng", ProgressStep::Download));
        EventSink::silent().send(DriverEvent::step("dameng", ProgressStep::Install));
    }

    /// 事件原样送到接收端（带批量位置与字节数）。
    #[test]
    fn events_arrive_with_their_metadata() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let sink = EventSink::new(sender);
        sink.send(DriverEvent::transfer("dameng", ProgressStep::Download, 10, 100).at(1, 3));
        let event = receiver.try_recv().expect("收到一条");
        assert_eq!(event.step, ProgressStep::Download);
        assert_eq!((event.downloaded, event.total), (Some(10), Some(100)));
        assert_eq!((event.index, event.count), (Some(1), Some(3)));
    }

    /// 步骤名唯一（落盘 / 日志里当键用）。
    #[test]
    fn step_names_are_unique() {
        let names = [
            ProgressStep::Registry,
            ProgressStep::JreDownload,
            ProgressStep::Download,
            ProgressStep::Verify,
            ProgressStep::Extract,
            ProgressStep::Install,
            ProgressStep::Remove,
            ProgressStep::Finished,
        ]
        .map(ProgressStep::wire_name);
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
    }
}
