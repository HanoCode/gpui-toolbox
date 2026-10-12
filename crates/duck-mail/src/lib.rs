//! 邮箱模块内核。
//!
//! 分层（`docs/email-design.md` §3）：
//!
//! ```text
//! duck-app/pages/mail.rs（UI，gpui）
//!   │  background_executor().spawn(阻塞调用 / Task::wait())
//!   ▼
//! duck-mail（本 crate：accounts / db / proto / mime / sync / search / ai）
//!   ▼
//! duck-platform::{secret,paths}   async-imap / mail-parser / rusqlite
//! ```
//!
//! # 硬约束
//!
//! 1. **零 gpui**：本 crate 不依赖 `gpui` / `duck-components` / `duck-store` / `duck-db`。
//!    与 `duck-db` / `duck-redis` / `duck-ai` 同纪律——内核可被 CLI、未来独立窗口或
//!    插件宿主复用。
//! 2. **tokio 不外泄**：`tokio::` 只出现在 [`runtime`]，且不出现在任何 `pub fn` 签名里。
//! 3. **口令不进 SQLite**：凭据一律走 [`accounts`] → `duck-platform::secret`。
//! 4. **写路径单事务**：多语句写一律 `MailDb::write_tx`。
//!
//! # 这一版（M1）到什么程度
//!
//! 已落地：账户 CRUD + 凭据加密、本地库与迁移、文件夹 / 线程列表 / 正文的存储 API、
//! IMAP 连接与拉取、MIME 解码、增量同步（UID 水位）、FTS5 检索。
//!
//! 未落地（M2+）：发信（SMTP）、动作队列与撤销、会话合并、OAuth2、JMAP / Gmail 专用
//! 通道、AI 摘要与起草、联系人 / 日历。设计文档 §6 有分期表。

pub mod accounts;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod mime;
pub mod models;
pub mod proto;
pub mod runtime;
pub mod secrets;
pub mod sync;

use std::sync::{Mutex, OnceLock, mpsc};

pub use config::MailPaths;
pub use error::{MailError, Result};
pub use events::MailEvent;
pub use models::{
    Account, AttachmentMeta, AuthKind, ConnectionSecurity, ConnectionTestResult, Folder,
    FolderRole, MessageDetail, MessageSummary, NewAccount, Provider, SyncState,
};
pub use secrets::Secrets;

use db::MailDb;
use runtime::MailRuntime;

/// 邮箱模块的门面：**宿主（`duck-app`）只需要面对这一个类型**。
///
/// 它持有库连接、路径、凭据后端与（惰性创建的）异步 runtime，并把各子模块的操作
/// 转发成同步方法。`Clone` 廉价——内部的库、runtime 与凭据句柄都是 `Arc` 共享。
#[derive(Clone)]
pub struct MailCore {
    paths: MailPaths,
    db: MailDb,
    secrets: Secrets,
    runtime: std::sync::Arc<OnceLock<MailRuntime>>,
    events: std::sync::Arc<Mutex<Option<mpsc::Sender<MailEvent>>>>,
}

impl MailCore {
    /// 打开指定落点的邮箱模块（凭据走平台默认后端）。
    pub fn open(paths: MailPaths) -> Result<Self> {
        Self::open_with_secrets(paths, secrets::platform())
    }

    /// 打开指定落点 + 指定凭据后端。
    ///
    /// 「凭据存哪」是个可替换项而不是写死的常量：测试要把它关进临时目录 / 内存
    /// （见 [`secrets`]），将来托管到企业密钥服务也只换这一处。
    pub fn open_with_secrets(paths: MailPaths, secrets: Secrets) -> Result<Self> {
        paths.ensure()?;
        let db = MailDb::open(paths.db_path())?;
        Ok(Self {
            paths,
            db,
            secrets,
            runtime: std::sync::Arc::new(OnceLock::new()),
            events: std::sync::Arc::new(Mutex::new(None)),
        })
    }

    /// 打开默认落点（`{app_data_dir}/mail`），凭据走平台默认后端。
    pub fn open_default() -> Result<Self> {
        Self::open(MailPaths::default_root())
    }

    /// 底层库（给需要直接跑 SQL 的内部模块用）。
    pub fn db(&self) -> &MailDb {
        &self.db
    }

    /// 凭据后端句柄。
    pub fn secrets(&self) -> &Secrets {
        &self.secrets
    }

    /// 数据落点。
    pub fn paths(&self) -> &MailPaths {
        &self.paths
    }

    /// 注册事件接收端。宿主在装配时调一次。
    pub fn set_event_sink(&self, sink: mpsc::Sender<MailEvent>) {
        if let Ok(mut guard) = self.events.lock() {
            *guard = Some(sink);
        }
    }

    /// 广播一个事件。**没有接收端时静默丢弃**——不是错误（页面还没打开、
    /// 或宿主没装事件通道，都不该让同步失败）。
    pub fn emit(&self, event: MailEvent) {
        if let Ok(guard) = self.events.lock()
            && let Some(tx) = guard.as_ref()
        {
            let _ = tx.send(event);
        }
    }

    /// 取异步 runtime（首次调用时创建）。
    ///
    /// **惰性**是有意的：只打开账户列表、不连服务器的场景不该付出起线程池的代价。
    pub fn runtime(&self) -> Result<&MailRuntime> {
        if let Some(rt) = self.runtime.get() {
            return Ok(rt);
        }
        let rt = MailRuntime::new()?;
        // 并发首次调用时 `set` 可能失败——那说明别人已经放进去了，用它的即可。
        let _ = self.runtime.set(rt);
        self.runtime
            .get()
            .ok_or_else(|| MailError::Other("mail runtime initialization failed".into()))
    }

    // ---- 账户 ----

    /// 全部账户。
    pub fn accounts(&self) -> Result<Vec<Account>> {
        accounts::list(&self.db)
    }

    /// 新增账户。
    pub fn add_account(&self, new: &NewAccount) -> Result<i64> {
        let id = accounts::add(&self.db, &self.secrets, new)?;
        self.emit(MailEvent::AccountsChanged);
        Ok(id)
    }

    /// 删除账户。
    pub fn remove_account(&self, account_id: i64) -> Result<()> {
        accounts::remove(&self.db, &self.secrets, account_id)?;
        self.emit(MailEvent::AccountsChanged);
        Ok(())
    }

    /// 测试一个尚未保存的账户配置能否连上（添加向导的「测试连接」按钮）。
    pub fn test_connection(&self, new: &NewAccount) -> Result<ConnectionTestResult> {
        accounts::validate(new)?;
        let cfg = proto::ImapConfig {
            host: new.imap_host.clone(),
            port: new.imap_port,
            security: new.security,
            username: new.username.clone(),
            password: new.password.clone(),
        };
        let report = self
            .runtime()?
            .spawn(async move { proto::imap::probe(cfg).await })
            .wait()?;
        Ok(report)
    }

    /// 测试一个**已保存**账户的连通性。
    pub fn test_saved_account(&self, account_id: i64) -> Result<ConnectionTestResult> {
        let (account, password) =
            accounts::load_with_password(&self.db, &self.secrets, account_id)?;
        let cfg = proto::ImapConfig {
            host: account.imap_host.clone(),
            port: account.imap_port,
            security: account.security,
            username: account.username.clone(),
            password,
        };
        let report = self
            .runtime()?
            .spawn(async move { proto::imap::probe(cfg).await })
            .wait()?;
        Ok(report)
    }

    // ---- 文件夹 / 邮件（读） ----

    /// 某账户的文件夹列表。
    pub fn folders(&self, account_id: i64) -> Result<Vec<Folder>> {
        self.db
            .read(|conn| db::repo::folders::list(conn, account_id))
    }

    /// 某文件夹的列表行（分页）。
    pub fn messages(&self, folder_id: i64, limit: i64, offset: i64) -> Result<Vec<MessageSummary>> {
        self.db
            .read(|conn| db::repo::messages::list_by_folder(conn, folder_id, limit, offset))
    }

    /// 邮件详情（含正文与附件）。
    ///
    /// **只读本地**：正文没拉过时 `text_body` / `html_body` 为 `None`、
    /// `body_state` 为 `none`。要真的把正文取回来用 [`Self::fetch_message_body`]。
    pub fn message_detail(&self, message_id: i64) -> Result<MessageDetail> {
        self.db
            .read(|conn| db::repo::messages::get_detail(conn, message_id))
    }

    /// 按需把一封邮件的正文拉回来（阻塞；调用方负责放进 `background_executor`）。
    ///
    /// 返回的是**落库后**的详情，所以调用方拿到就能直接渲染。已缓存时不会连网。
    pub fn fetch_message_body(&self, message_id: i64) -> Result<MessageDetail> {
        sync::fetch_body(self, message_id)
    }

    /// 同步一个账户（阻塞；调用方负责放进 `background_executor`）。
    pub fn sync_account(&self, account_id: i64) -> Result<sync::SyncReport> {
        sync::sync_account(self, account_id)
    }

    /// 全文检索，返回命中的邮件主键。
    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<i64>> {
        self.db
            .read(|conn| db::repo::search::search(conn, query, limit))
    }

    /// 标一封邮件已读 / 未读。
    pub fn set_message_read(&self, message_id: i64, is_read: bool) -> Result<()> {
        self.db.write_tx(|tx| {
            let detail = db::repo::messages::get_detail(tx, message_id)?;
            db::repo::messages::set_flags(tx, message_id, is_read, detail.is_starred)
        })
    }
}

impl std::fmt::Debug for MailCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailCore")
            .field("db", &self.paths.db_path())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;

    fn core(tag: &str) -> (MailCore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("duck-mail-core-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 凭据走后端注入（内存），**不碰**用户真实的 `<应用数据目录>/credentials.json`。
        let core =
            MailCore::open_with_secrets(MailPaths::with_root(&dir), secrets::memory()).unwrap();
        (core, dir)
    }

    #[test]
    fn open_creates_dirs_and_migrates() {
        let (core, dir) = core("open");
        assert!(core.paths().db_path().exists());
        assert!(core.paths().attachments_dir().is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_lifecycle_through_the_facade() {
        let (core, dir) = core("acct");
        let id = core
            .add_account(&NewAccount {
                email: "a@example.com".into(),
                display_name: Some("工作".into()),
                username: "a@example.com".into(),
                imap_host: "imap.example.com".into(),
                imap_port: 993,
                smtp_host: "smtp.example.com".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "pw".into(),
            })
            .unwrap();
        assert_eq!(core.accounts().unwrap().len(), 1);
        core.remove_account(id).unwrap();
        assert!(core.accounts().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn events_are_delivered_to_the_sink() {
        let (core, dir) = core("events");
        let (tx, rx): (mpsc::Sender<MailEvent>, Receiver<MailEvent>) = mpsc::channel();
        core.set_event_sink(tx);
        core.add_account(&NewAccount {
            email: "e@example.com".into(),
            display_name: None,
            username: "e@example.com".into(),
            imap_host: "h".into(),
            imap_port: 993,
            smtp_host: "s".into(),
            smtp_port: 465,
            security: ConnectionSecurity::Tls,
            password: "p".into(),
        })
        .unwrap();
        assert_eq!(rx.try_recv().unwrap(), MailEvent::AccountsChanged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emit_without_sink_is_a_no_op() {
        let (core, dir) = core("nosink");
        // 没有接收端也不该 panic / 报错。
        core.emit(MailEvent::AccountsChanged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn passwords_land_in_the_injected_backend_only() {
        let (core, dir) = core("secrets");
        core.add_account(&NewAccount {
            email: "s@example.com".into(),
            display_name: None,
            username: "s@example.com".into(),
            imap_host: "h".into(),
            imap_port: 993,
            smtp_host: "smtp".into(),
            smtp_port: 465,
            security: ConnectionSecurity::Tls,
            password: "hunter2".into(),
        })
        .unwrap();

        // ① 口令确实进了注入的后端（不是平台的 default_store）。
        let key = accounts::password_key(1);
        assert_eq!(
            core.secrets().get(&key).unwrap().as_deref(),
            Some("hunter2")
        );

        // ② 库里没有明文——把整份 mail.db 的字节扫一遍，口令不该出现。
        //    （凭据走的是另一个文件，数据库整体拷走也读不出口令。）
        let bytes = std::fs::read(core.paths().db_path()).unwrap();
        assert!(
            !contains_subslice(&bytes, b"hunter2"),
            "password leaked into mail.db"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn runtime_is_lazy_and_shared() {
        let (core, dir) = core("rt");
        let a = core.runtime().unwrap();
        let b = core.runtime().unwrap();
        // 两次拿到的是同一个 runtime（`OnceLock` 只建一次）。
        assert!(std::ptr::eq(a, b));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
