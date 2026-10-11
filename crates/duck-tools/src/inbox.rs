//! 智能粘贴收件箱：暂存最近一次识别到的剪贴板内容。
//!
//! 识别（[`crate::paste::detect`]）是纯函数，但「识别出来的东西给谁用」需要跨视图传递：
//! 顶栏搜索框粘贴 → 工具箱页顶部横幅提示 → 用户确认后用对应工具处理。收件箱就是这一段的载体。
//!
//! 只保留**最近一条**：粘贴是新意图，旧负载再留着只会让人困惑。
//! 用 `RefCell` 而非锁——gpui 全局只活在主线程，无并发访问。

use std::cell::RefCell;

use gpui::{App, Global};

/// 一次待处理的粘贴负载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPaste {
    /// 命中的工具 id。
    tool_id: &'static str,
    /// 原始文本（已 trim）。
    text: String,
}

impl PendingPaste {
    /// 构造一条负载。
    #[must_use]
    pub fn new(tool_id: &'static str, text: impl Into<String>) -> Self {
        Self {
            tool_id,
            text: text.into(),
        }
    }

    /// 命中的工具 id。
    #[must_use]
    pub fn tool_id(&self) -> &'static str {
        self.tool_id
    }

    /// 原始文本。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// 全局收件箱（一条容量）。
#[derive(Default)]
pub struct PasteInbox {
    pending: RefCell<Option<PendingPaste>>,
}

impl Global for PasteInbox {}

impl PasteInbox {
    /// 建立空收件箱（在创建任何视图之前调用一次）。
    pub fn install(cx: &mut App) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
    }

    /// 存入一条负载（覆盖旧的）。
    pub fn put(cx: &App, pending: PendingPaste) {
        if cx.has_global::<Self>() {
            *cx.global::<Self>().pending.borrow_mut() = Some(pending);
        }
    }

    /// 取出当前负载（不移除）。
    #[must_use]
    pub fn peek(cx: &App) -> Option<PendingPaste> {
        cx.try_global::<Self>()
            .and_then(|inbox| inbox.pending.borrow().clone())
    }

    /// 取走并清除当前负载。
    pub fn take(cx: &App) -> Option<PendingPaste> {
        cx.try_global::<Self>()?.pending.borrow_mut().take()
    }

    /// 清空。
    pub fn clear(cx: &App) {
        if let Some(inbox) = cx.try_global::<Self>() {
            *inbox.pending.borrow_mut() = None;
        }
    }

    /// 是否有针对某个工具的待处理内容（行内提示用）。
    #[must_use]
    pub fn has_for(cx: &App, tool_id: &str) -> bool {
        Self::peek(cx).is_some_and(|pending| pending.tool_id() == tool_id)
    }
}
