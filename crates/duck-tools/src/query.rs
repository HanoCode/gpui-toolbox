//! 全局搜索词：顶栏搜索框写入，工具箱列表读取。
//!
//! 搜索框在 chrome 层、列表在页面层，两者互不持有对方句柄，靠这一个全局串起来。
//! 存文本而非句柄，是因为列表只需要一个字符串，没必要为一个查询词把两个视图耦合成
//! 「页面持有 chrome 实体」。

use std::cell::RefCell;

use gpui::{App, Global};

/// 当前搜索词（一条容量）。
#[derive(Default)]
pub struct ToolQuery {
    query: RefCell<String>,
}

impl Global for ToolQuery {}

impl ToolQuery {
    /// 建立空查询（在创建任何视图之前调用一次）。
    pub fn install(cx: &mut App) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
    }

    /// 写入搜索词。
    pub fn set(cx: &App, query: impl Into<String>) {
        if let Some(state) = cx.try_global::<Self>() {
            *state.query.borrow_mut() = query.into();
        }
    }

    /// 读取搜索词。
    ///
    /// 返回 `String` 而非 `&str`：渲染期会反复读，借用一旦跨过 `cx` 的可变调用就会
    /// 撞上借用检查；查询词很短，克隆成本可以忽略。
    #[must_use]
    pub fn get(cx: &App) -> String {
        cx.try_global::<Self>()
            .map_or_else(String::new, |state| state.query.borrow().clone())
    }

    /// 是否为空（含纯空白）。
    #[must_use]
    pub fn is_empty(cx: &App) -> bool {
        Self::get(cx).trim().is_empty()
    }
}
