//! 各表的读写。
//!
//! 每个函数都接收 `&Connection`——`rusqlite::Transaction` 通过 `Deref` 也能直接传，
//! 所以同一份 repo 代码既能用在只读连接上，也能用在事务里。
//!
//! **不在这里做事务控制**：起事务是 [`crate::db::MailDb::write_tx`] 的职责。
//! repo 只管「一条 SQL 怎么写」。

pub mod accounts;
pub mod drafts;
pub mod folders;
pub mod labels;
pub mod messages;
pub mod queue;
pub mod search;
pub mod threads;
