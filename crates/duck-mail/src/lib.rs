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
//! duck-platform::{secret,paths}   async-imap / mail-parser / mail-builder / rusqlite
//! ```
//!
//! # 硬约束
//!
//! 1. **零 gpui**：本 crate 不依赖 `gpui` / `duck-components` / `duck-store` / `duck-db`。
//!    与 `duck-db` / `duck-redis` / `duck-ai` 同纪律——内核可被 CLI、未来独立窗口或
//!    插件宿主复用。
//! 2. **tokio 不外泄**：直接用 `tokio::` 的只有两处——[`runtime`]（自管 runtime）与
//!    [`proto::imap`]（连接层用 `tokio::net::TcpStream` 与 `tokio::time::timeout`，
//!    `async-imap` 的会话就跑在这条连接上）。**公共 API 上没有 tokio 派生类型**：
//!    协议层那两个流类型别名是 `pub(crate)`，`pub fn` 的签名里也不出现 `tokio::`。
//! 3. **口令不进 SQLite**：凭据一律走 [`accounts`] → `duck-platform::secret`。
//! 4. **写路径单事务**：多语句写一律 `MailDb::write_tx`。
//!
//! # 这一版到什么程度
//!
//! **已落地**
//!
//! - M1：账户 CRUD + 凭据加密、本地库与迁移、文件夹 / 线程列表 / 正文的存储 API、
//!   IMAP 连接与拉取、MIME 解码（含 CID 内联与远程图片闸门）、增量同步（UID 水位 +
//!   窗口内旗标对账 + UID 集合删除对账）、FTS5 检索（含条件查询语言）。
//! - M1.5（无新依赖）：IMAP **写方向**（`UID STORE` / `UID COPY` / `UID MOVE` 与无 MOVE
//!   时的三步回退 / `UID EXPUNGE`，含 CAPABILITY 能力位）、动作队列（`pending_actions`
//!   5 态状态机 + 乐观更新 + 撤销宽限期）、标签仓储。[`MailCore`] 的旗标 / 移动 / 删除 /
//!   标签门面与 [`MailCore::flush_pending`] 补发。
//! - M2 第一批：**发信**（手写 SMTP 与 `compose` 头部装配，刻意未引 `lettre`）、
//!   **附件另存**（整封取回 + MIME 解码 + 落盘，见 [`sync::download_attachment`]）。
//! - M2 第二批：**发信带附件**（[`OutgoingAttachment`]）。`multipart/mixed` 的**正文**
//!   改由 `mail-builder` 装配（`MimePart`），**头部与单部件路径仍手写**——取舍写在
//!   `compose.rs` 模块头。`default-features = false`，故仍是零新增传递依赖。
//! - M2 第三批：**常驻同步 actor**（[`MailCore::start_sync_actor`]）：per-account
//!   `SELECT` → `IDLE` 等待 → 变更触发增量同步；无 `IDLE` 能力时降级 `NOOP` 轮询；
//!   断线退避重连；应用退出 / 账户移除时**干净停止**。**附件按 IMAP 节下载**
//!   （`BODY.PEEK[<section>]`）落应用管理目录（0600），落点校验 + 文件名清洗。
//!
//! **未落地（M2+）**：草稿自动保存与推送服务器 `\Drafts`、签名 / 发件身份、写回
//! 「已发送」、会话合并、OAuth2 的**浏览器授权流程**（协议件已落地，见 [`oauth`]）、
//! JMAP / Gmail 专用通道、AI 摘要与起草、联系人 / 日历。设计文档 §6 有分期表。

pub mod accounts;
pub mod compose;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod mime;
pub mod models;
pub mod oauth;
pub mod presets;
pub mod proto;
pub mod queue;
pub mod runtime;
pub mod secrets;
pub mod sync;
pub mod threading;

use std::sync::{Mutex, OnceLock, mpsc};

pub use config::MailPaths;
pub use db::repo::queue::{ActionState, PendingAction};
pub use error::{MailError, Result};
pub use events::MailEvent;
pub use models::{
    Account, AttachmentMeta, AuthKind, ConnectionSecurity, ConnectionTestResult, DraftMode,
    DraftSummary, Folder, FolderRole, Label, MessageDetail, MessageSummary, NewAccount, Provider,
    SyncState,
};
pub use oauth::{Endpoints, Pkce, Redirect, TokenSet};
pub use proto::smtp::{SmtpCaps, SmtpConfig, smtp_data_payload};
pub use queue::ActionKind;
pub use secrets::Secrets;
pub use sync::AttachmentSaved;

/// 一轮后台预取的封数上限（[`MailCore::sync_account`] 收尾时会按它跑一次）。
pub use sync::PREFETCH_BODY_LIMIT;

pub use compose::{Draft, Mailbox, OutgoingAttachment};
pub use db::repo::drafts::{DraftAttachmentRef, DraftRecord};

use db::MailDb;
use proto::smtp::SmtpSession;
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
        // **启动清理悬挂的 `fetching`**：上一次运行在拉正文途中被杀掉（崩溃 / 强退 /
        // 断电）会留下永远停在 `fetching` 的行——没有任何任务在跑，界面却永远显示
        // 「正在下载正文」，而原子抢占（只认 `none`）又让它们再也抢不动，那封邮件就
        // 彻底拉不到正文了。这里统一退回 `none`，让它们重新可被抢占。
        //
        // 放在 `open`（每次启动一次）而不是同步循环里：后者会把**正在进行**的拉取
        // 一起误伤。失败不致命（真的库坏了，后面任何操作都会报出来），只记日志。
        // 参考 Flectar `lib.rs:485-497` 的同一处兜底。
        match db.write_tx(|tx| db::repo::messages::reset_stale_body_fetches(tx)) {
            Ok(0) => {}
            Ok(n) => tracing::info!(recovered = n, "reset orphaned mail body fetches"),
            Err(error) => tracing::debug!(%error, "resetting orphaned body fetches failed"),
        }
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
        // 先把常驻 actor 停掉，再删库——否则 actor 会在账户已经不存在之后
        // 继续按老凭据连服务器（直到下一轮才失败退出）。
        self.stop_sync_actor(account_id);
        accounts::remove(&self.db, &self.secrets, account_id)?;
        self.emit(MailEvent::AccountsChanged);
        Ok(())
    }

    /// 编辑账户（服务器参数 / 显示信息）。**口令留空 = 保持现有口令**。
    ///
    /// 连接参数可能变了，actor 必须按新档案重连：先停、写完档案再拉起。写失败
    /// 也要把 actor 恢复——保存失败不该连累同步停摆。
    pub fn update_account(&self, account_id: i64, new: &NewAccount) -> Result<()> {
        self.stop_sync_actor(account_id);
        let result = accounts::update(&self.db, &self.secrets, account_id, new);
        let stopped = result.is_ok();
        if let Err(error) = self.start_sync_actor(account_id) {
            tracing::warn!(account_id, %error, "failed to restart the sync actor after update");
        }
        if stopped {
            self.emit(MailEvent::AccountsChanged);
        }
        result
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
            // `NewAccount` 只有口令一条路：OAuth2 账户不走「测试连接」这个入口
            // （它要先有令牌，见 `oauth` 模块）。
            auth: AuthKind::Password,
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
            auth: account.auth_kind,
        };
        let report = self
            .runtime()?
            .spawn(async move { proto::imap::probe(cfg).await })
            .wait()?;
        Ok(report)
    }

    /// 测试一次**编辑中的**账户配置（编辑弹窗的「测试连接」）。
    ///
    /// 与 [`Self::test_connection`] 的差别只有一处：口令留空时取**已存凭据**——
    /// 编辑态的语义是「改服务器参数、口令保持不变」，用户不该被逼着把口令再敲一遍
    /// 才能测连接。
    pub fn test_account_update(
        &self,
        account_id: i64,
        new: &NewAccount,
    ) -> Result<ConnectionTestResult> {
        if new.password.is_empty() {
            let mut probe = new.clone();
            probe.password =
                accounts::load_password(&self.secrets, account_id)?.unwrap_or_default();
            self.test_connection(&probe)
        } else {
            self.test_connection(new)
        }
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

    /// 后台正文下载：把某账户**收件箱**里最新的、还没拉过正文的若干封抓下来，返回
    /// 成功拉取的封数（阻塞；调用方负责放进 `background_executor`）。
    ///
    /// **只做最新批**——历史全量回填走 [`Self::prefetch_and_backfill_bodies`]。常规
    /// 路径由内核自己跑（每轮同步收尾、以及每次 IDLE 增量结束后），宿主不必操心；
    /// 这个窄入口留给「切到某个账户、想先补一下最上面几封」之类场景。
    ///
    /// **best-effort**：单封失败即结束本轮，不返回错误（返回的是已成功的封数）。
    pub fn prefetch_bodies(&self, account_id: i64, limit: i64) -> Result<usize> {
        sync::prefetch_bodies(self, account_id, limit)
    }

    /// 后台正文下载（完整版）：**最新批 + 一批历史回填**，返回本批成功拉取的封数。
    ///
    /// 历史回填按 `folders.backfill_cursor` 从新往老推进，每调用一次推一批（默认
    /// [`sync::BACKFILL_BODY_BATCH`] 封），推到底后置 `done`，此后只剩最新批。因此
    /// 反复调用是**幂等且可累积**的：已缓存的封会被 `missing_body_ids*` 直接过滤掉，
    /// 不会重复联网。内核在每轮同步收尾时已自动跑一次；公开出来是为了让宿主能主动补
    /// （例如将来在设置里点「下载全部正文」）。
    pub fn prefetch_and_backfill_bodies(
        &self,
        account_id: i64,
        latest_limit: i64,
        backfill_batch: i64,
    ) -> Result<usize> {
        sync::prefetch_and_backfill_bodies(self, account_id, latest_limit, backfill_batch)
    }

    /// 把一个附件另存到 `dest`（阻塞；调用方负责放进 `background_executor`）。
    ///
    /// `dest` 是**完整的目标文件路径**（不是目录）——由界面调
    /// `duck_platform::save_file` 让用户自己选落点。内部**两步走**：先按 IMAP 节
    /// （`BODY.PEEK[<section>]`）取回部件、写入应用管理目录（0600）并回写
    /// `attachments.file_path`，再把缓存拷到 `dest`。缓存命中（文件在、字节数与清单
    /// 一致）时纯本地拷贝、不连网；拿不到可靠段号时降级整封取回 + MIME 解码。
    /// 细节（段号从哪来、序号如何复核、上限与路径校验）见 [`sync::download_attachment`]。
    pub fn download_attachment(
        &self,
        attachment_id: i64,
        dest: &std::path::Path,
    ) -> Result<sync::AttachmentSaved> {
        sync::download_attachment(self, attachment_id, dest)
    }

    /// 同步一个账户（阻塞；调用方负责放进 `background_executor`）。
    pub fn sync_account(&self, account_id: i64) -> Result<sync::SyncReport> {
        sync::sync_account(self, account_id)
    }

    // ---- 常驻同步 actor（IDLE 推送 + 自动重连）----

    /// 启动一个账户的常驻同步 actor。
    ///
    /// # 语义
    ///
    /// - **非阻塞**：只把 actor 投进 runtime 就返回（`&self`，可在 UI 线程直接调）；
    /// - **幂等**：同一账户已经在跑时直接返回，不会叠出第二条 IDLE 长连接；
    /// - **自动重连**：连接出错按退避重连（5s 起、5min 封顶），不必宿主操心。
    ///
    /// actor 的工作循环（见 [`sync`] 模块的 actor 段）：先跑一轮增量同步（复用既有
    /// 同步函数），再 `SELECT` 关注文件夹进入 IDLE；收到变更 / 到 max-idle 就触发一轮
    /// 增量；服务端不支持 IDLE 时降级为 `NOOP` 保活 + 定时轮询。
    ///
    /// 停止：账户移除时 [`Self::remove_account`] 会自动停；应用退出时调
    /// [`Self::stop_all_sync_actors`]。
    pub fn start_sync_actor(&self, account_id: i64) -> Result<()> {
        if self.runtime()?.has_resident(account_id) {
            return Ok(());
        }
        sync::spawn_account_actor(self, account_id)
    }

    /// 停掉一个账户的常驻同步 actor。幂等（没在跑时是空操作）。
    pub fn stop_sync_actor(&self, account_id: i64) {
        if let Some(rt) = self.runtime.get() {
            rt.stop_resident(account_id);
        }
    }

    /// 停掉**所有**账户的常驻同步 actor（应用退出时调）。
    pub fn stop_all_sync_actors(&self) {
        if let Some(rt) = self.runtime.get() {
            rt.stop_all_residents();
        }
    }

    /// 全文检索，返回命中的邮件主键。
    ///
    /// 只给主键是 FTS5 的形态（无 content 表，读不出列值）；界面要的是列表行，
    /// 用 [`Self::search_messages`] 一步拿全。
    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<i64>> {
        self.db
            .read(|conn| db::repo::search::search(conn, query, limit))
    }

    /// 全文检索，直接返回可渲染的列表行（按相关度排序）。
    ///
    /// 列表行跨文件夹——检索本来就是全账号的，界面不必先选文件夹再搜。
    /// 索引里已经不存在的 rowid（邮件被别的设备删了）会被跳过，不报错。
    pub fn search_messages(&self, query: &str, limit: i64) -> Result<Vec<MessageSummary>> {
        self.db.read(|conn| {
            let ids = db::repo::search::search(conn, query, limit)?;
            db::repo::messages::list_by_ids(conn, &ids)
        })
    }

    /// 标一封邮件已读 / 未读。
    ///
    /// 走动作队列：本地立刻生效，远端在撤销宽限期后补发。签名保留给既有调用方
    /// （`open_message` 的「打开即已读」）；账户由邮件行反查，调用方不必知道它。
    pub fn set_message_read(&self, message_id: i64, is_read: bool) -> Result<()> {
        self.db.write_tx(|tx| {
            let account_id = db::repo::messages::location(tx, message_id)?.account_id;
            let request = if is_read {
                queue::Request::MarkRead
            } else {
                queue::Request::MarkUnread
            };
            queue::enqueue(tx, account_id, message_id, request)?;
            Ok(())
        })
    }

    // ---- 动作队列（M1.5：写方向） ----

    /// 标一封邮件已读 / 未读（入队），返回动作主键（供撤销条用）。
    pub fn set_read(&self, account_id: i64, message_id: i64, read: bool) -> Result<i64> {
        let request = if read {
            queue::Request::MarkRead
        } else {
            queue::Request::MarkUnread
        };
        self.db
            .write_tx(|tx| queue::enqueue(tx, account_id, message_id, request))
    }

    /// 加 / 去旗标（星标），返回动作主键。
    pub fn set_starred(&self, account_id: i64, message_id: i64, starred: bool) -> Result<i64> {
        let request = if starred {
            queue::Request::Star
        } else {
            queue::Request::Unstar
        };
        self.db
            .write_tx(|tx| queue::enqueue(tx, account_id, message_id, request))
    }

    /// 把一封邮件移到另一个文件夹（`folder_id` 是本地文件夹主键），返回动作主键。
    pub fn move_message(&self, account_id: i64, message_id: i64, folder_id: i64) -> Result<i64> {
        self.db.write_tx(|tx| {
            queue::enqueue(
                tx,
                account_id,
                message_id,
                queue::Request::Move { folder_id },
            )
        })
    }

    /// 删除一封邮件（有垃圾箱则移入，否则就地销毁），返回动作主键。
    pub fn delete_message(&self, account_id: i64, message_id: i64) -> Result<i64> {
        self.db
            .write_tx(|tx| queue::enqueue(tx, account_id, message_id, queue::Request::Delete))
    }

    /// 给邮件打标签，返回动作主键。
    pub fn label_add(&self, account_id: i64, message_id: i64, label_id: i64) -> Result<i64> {
        self.db.write_tx(|tx| {
            queue::enqueue(
                tx,
                account_id,
                message_id,
                queue::Request::LabelAdd { label_id },
            )
        })
    }

    /// 摘掉邮件的某个标签，返回动作主键。
    pub fn label_remove(&self, account_id: i64, message_id: i64, label_id: i64) -> Result<i64> {
        self.db.write_tx(|tx| {
            queue::enqueue(
                tx,
                account_id,
                message_id,
                queue::Request::LabelRemove { label_id },
            )
        })
    }

    /// 某账户待补发的动作（按入队顺序）。
    pub fn pending_actions(&self, account_id: i64) -> Result<Vec<PendingAction>> {
        self.db
            .read(|conn| db::repo::queue::list(conn, account_id, Some(ActionState::Pending)))
    }

    /// 撤销一条待补发的动作。只在仍 `pending` 时返回 `true`。
    pub fn undo_pending(&self, action_id: i64) -> Result<bool> {
        self.db.write_tx(|tx| queue::undo(tx, action_id))
    }

    /// 按入队顺序把某账户到点的动作补发到服务器，返回成功条数（阻塞；调用方放后台线程）。
    pub fn flush_pending(&self, account_id: i64) -> Result<usize> {
        queue::flush_pending(self, account_id)
    }

    // ---- 发信（SMTP）----

    /// 发一封信（**同步阻塞**；调用方放后台线程）。
    ///
    /// 走 SMTP 直发，**不进动作队列**：队列的语义是「先在本地生效、稍后补发远端」，
    /// 而发信没有「本地生效」这一半（本模块没有本地发件箱），失败就该当场把原因
    /// 告诉用户，而不是悄悄排队重试。
    ///
    /// 发件身份取自档案（`smtp_host` / `smtp_port` / `security` / 用户名 + 口令）；
    /// 收件人取自 [`compose::envelope_recipients`]（**含密送**）；`draft` 里
    /// `date_unix` / `message_id` 留空时由 [`compose::Draft::stamp`] 补上。
    ///
    /// 发出成功后**另起一条后台任务**把本报文写回服务端「已发送」（见
    /// [`Self::schedule_sent_write_back`]）——那条路的成败与本函数的返回值无关。
    pub fn send(&self, account_id: i64, draft: &compose::Draft) -> Result<()> {
        let runtime = self.runtime()?;
        let core = self.clone();
        let draft = draft.clone();
        runtime
            .spawn(async move { send_inner(&core, account_id, &draft).await })
            .wait()?
    }

    /// 把刚发出的原始报文**写回服务端「已发送」**（`APPEND` + 一轮单夹同步）。
    ///
    /// **fire-and-forget**：`send` 的结论只由 SMTP 决定，这一步既不该拖住弹窗关闭，
    /// 也不该把「已发出」翻成失败——它整个包在后台任务里，失败只落一条 warn 日志
    /// （`MailRuntime::spawn` 拿到 `Task` 后立刻丢掉即等于「别等结果」；任务照常在
    /// runtime 的 worker 上跑完）。
    fn schedule_sent_write_back(&self, account_id: i64, message: String) {
        let Ok(runtime) = self.runtime() else {
            return;
        };
        let core = self.clone();
        let task = runtime.spawn(async move {
            if let Err(error) = write_back_sent(&core, account_id, message).await {
                tracing::warn!(account_id, %error, "sent copy was not written back");
            }
        });
        drop(task);
    }

    // ---- 草稿（本地）----

    /// 新建或改存一条草稿，返回本地行号（`messages.id`）。
    ///
    /// `record.id == None` → 新建；`Some(id)` → 改存那一行——**必须是本账户的草稿**，
    /// 否则 `NotFound`（一个错传的 id 绝不能把真邮件改坏）。
    ///
    /// ⚠️ 附件只记**路径**、不记字节：表结构给的就是 `file_path`。重开草稿时才读盘，
    /// 文件被挪走就少一个附件（要不要提示由界面决定）。见 [`db::repo::drafts`] 模块头。
    pub fn save_draft(&self, record: &db::repo::drafts::DraftRecord) -> Result<i64> {
        let now = compose::now_unix();
        self.db
            .write_tx(|tx| db::repo::drafts::save(tx, record, now))
    }

    /// 草稿箱列表（按最后保存时间倒序）。
    pub fn list_drafts(&self, account_id: i64) -> Result<Vec<DraftSummary>> {
        self.db
            .read(|conn| db::repo::drafts::list(conn, account_id))
    }

    /// 草稿条数。侧栏角标用它——草稿的 `folder_id` 是 NULL，落不进 `folders` 的计数。
    pub fn draft_count(&self, account_id: i64) -> Result<i64> {
        self.db
            .read(|conn| db::repo::drafts::count(conn, account_id))
    }

    /// 读回一条草稿（含附件路径引用与 `In-Reply-To` / `References`）。
    pub fn load_draft(
        &self,
        account_id: i64,
        draft_id: i64,
    ) -> Result<db::repo::drafts::DraftRecord> {
        self.db
            .read(|conn| db::repo::drafts::load(conn, account_id, draft_id))
    }

    /// 删一条草稿。`false` = 这行不存在、或不是这个账户的草稿（不是错误）。
    pub fn delete_draft(&self, account_id: i64, draft_id: i64) -> Result<bool> {
        self.db
            .write(|conn| db::repo::drafts::delete(conn, account_id, draft_id))
    }

    // ---- 标签 ----

    /// 某账户可见的标签（自己的账户级 + 全部全局级）。
    pub fn labels(&self, account_id: i64) -> Result<Vec<Label>> {
        self.db
            .read(|conn| db::repo::labels::list_for_account(conn, account_id))
    }

    /// 建一个账户级标签，返回主键（同名复用）。
    pub fn label_create(&self, account_id: i64, name: &str, color: &str) -> Result<i64> {
        queue::create_label(self, account_id, name, color)
    }

    /// 某邮件已贴的标签。
    pub fn labels_for_message(&self, message_id: i64) -> Result<Vec<Label>> {
        self.db
            .read(|conn| db::repo::labels::labels_for_message(conn, message_id))
    }

    /// 某个标签下的邮件（左栏「标签」选中后的列表）。
    pub fn messages_by_label(&self, label_id: i64, limit: i64) -> Result<Vec<MessageSummary>> {
        self.db
            .read(|conn| db::repo::messages::list_by_label(conn, label_id, limit))
    }
}

impl std::fmt::Debug for MailCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailCore")
            .field("db", &self.paths.db_path())
            .finish_non_exhaustive()
    }
}

/// [`MailCore::send`] 的异步体：拿凭据 → 建 SMTP 会话 → 发信 → 道别。
async fn send_inner(core: &MailCore, account_id: i64, draft: &compose::Draft) -> Result<()> {
    let (account, password) = accounts::load_with_password(core.db(), core.secrets(), account_id)?;

    let recipients = compose::envelope_recipients(draft);
    if recipients.is_empty() {
        return Err(MailError::Config("no recipients to send to".into()));
    }

    // 补信头元数据（`Date` / `Message-ID`）——必须在装配**之前**，因为装配
    // 会把它们写进头部。
    let mut draft = draft.clone();
    draft.stamp(compose::now_unix());
    let message = compose::build_message(&draft)?;

    let cfg = SmtpConfig {
        host: account.smtp_host.clone(),
        port: account.smtp_port,
        security: account.security,
        username: account.username.clone(),
        password,
        auth: account.auth_kind,
        client_name: ehlo_domain(&draft.from.address),
    };

    let mut session = SmtpSession::connect(&cfg).await?;
    let outcome = session
        .send(&draft.from.address, &recipients, &message)
        .await;
    // 无论成败都发 `QUIT`：失败时直接丢连接会让服务端留一条半开会话。
    // `quit` 自己吞掉错误，所以这里不会把真正的失败原因盖掉。
    session.quit().await;
    outcome?;
    // 真发出去了才写回副本（`write_back_sent`）。它不是「发送」的一部分：那条后台
    // 任务在本函数返回后才跑，界面上「邮件已发出」的结论已经定下来了。
    core.schedule_sent_write_back(account_id, message);
    Ok(())
}

/// 把刚发出的原始报文写进服务端「已发送」文件夹，并让本地副本跟上。
///
/// # 为什么必须有这一步
///
/// 惯例是 **SMTP 只投递，副本由发信方自己塞**（RFC 3501 §6.3.11 的 `APPEND`）：
/// 服务端不会把从 SMTP 发出去的信自动托管进 `\Sent`。缺了它，用户发完信在「已发送」
/// 里找不着自己，换台设备打开更是完全没有——界面早先只能常驻一行说明告知用户。
///
/// # 三条口径
///
/// 1. **尽力而为**：调用点只记日志（`schedule_sent_write_back`）。信已经发出去了，
///    写回失败不能让「发送」判失败。
/// 2. **本地副本靠同步补齐，不手工插行**：手工往 `messages` 里插的行没有服务端 UID，
///    下一轮删除对账会把它当「服务端已删」清掉（`sync.rs::reconcile_deletions`）。
/// 3. **目标按角色找**（[`FolderRole::Sent`]）而不是按名字：`Sent` / `已发送` /
///    `&XfJT0ZAB-` 都可能是它。账户还没同步过（文件夹表空）就跳过——那说明连
///    「已发送叫什么」都还不知道。
///
/// 写完就地跑一轮**单夹**增量同步（`sync::sync_folder`）而不是整账户同步：只有这一个
/// 文件夹变了，整账户跑一遍在有几万个文件夹的企业账号上纯属浪费。新增的行由
/// `MessagesAdded` 事件推给界面——「正看着已发送」的列表因此自己刷新（事件通道口径，
/// 见 `duck-app/src/pages/mail.rs` 的 `apply_event`）。
async fn write_back_sent(core: &MailCore, account_id: i64, message: String) -> Result<()> {
    let sent = core
        .db()
        .read(|conn| db::repo::folders::find_by_role(conn, account_id, FolderRole::Sent))?;
    // 名字是服务端 `LIST` 回的原始串（可能是 Modified UTF-7），`APPEND` 要的就是它。
    let Some(folder) = sent else {
        return Err(MailError::NotFound(
            "no \\Sent folder known for this account; sync it first".into(),
        ));
    };

    let (account, password) = accounts::load_with_password(core.db(), core.secrets(), account_id)?;
    let cfg = proto::ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        security: account.security,
        username: account.username.clone(),
        password,
        auth: account.auth_kind,
    };

    let mut session = proto::imap::ImapSession::connect(&cfg).await?;
    session
        .append_message(&folder.name, message.as_bytes())
        .await?;
    // 副本已经在服务端了，所以这一步失败也不回滚（下一轮同步照样会把它收下来）——
    // 只记日志，让本轮结论停在「已写回」而不是「写回失败」。
    match crate::sync::sync_folder(core.db(), &mut session, account_id, folder.id, &folder.name)
        .await
    {
        Ok((new, _removed)) => {
            if new > 0 {
                core.emit(MailEvent::MessagesAdded {
                    account_id,
                    folder_id: folder.id,
                    count: new as u64,
                });
            }
        }
        Err(error) => {
            tracing::warn!(account_id, %error, "sent folder sync after append failed");
        }
    }
    session.logout().await;
    Ok(())
}

/// `EHLO` 里自报的域名：取发件地址的域名。
///
/// 自报一个**不存在**的域名会让一部分严格的反垃圾服务器直接拒收，所以用发件域
/// （这也是绝大多数客户端的做法）；地址里没有 `@` 时才回落到 `localhost`。
fn ehlo_domain(from_address: &str) -> String {
    match from_address.split_once('@') {
        Some((_, domain)) if !domain.trim().is_empty() => domain.trim().to_owned(),
        _ => "localhost".to_owned(),
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

    /// 一份最小可用的账户档案。写回相关测试只关心「账户存在」，主机名指哪都行
    /// （真正的连接分支在无服务器环境里跑不到）。
    fn account_seed(email: &str) -> NewAccount {
        NewAccount {
            email: email.into(),
            display_name: None,
            username: email.into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            security: ConnectionSecurity::Tls,
            password: "pw".into(),
        }
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
    fn search_messages_hydrates_hits_into_list_rows() {
        let (core, dir) = core("search");
        let account = core
            .add_account(&NewAccount {
                email: "s@example.com".into(),
                display_name: None,
                username: "s@example.com".into(),
                imap_host: "h".into(),
                imap_port: 993,
                smtp_host: "smtp".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "p".into(),
            })
            .unwrap();
        let folder = core
            .db()
            .write_tx(|tx| {
                db::repo::folders::upsert(
                    tx,
                    account,
                    &db::repo::folders::FolderUpsert {
                        name: "INBOX".into(),
                        delimiter: None,
                        role: FolderRole::Inbox,
                        uidvalidity: Some(1),
                        uidnext: Some(2),
                    },
                )
            })
            .unwrap();
        let message = core
            .db()
            .write_tx(|tx| {
                db::repo::messages::upsert_envelope(
                    tx,
                    account,
                    folder,
                    &db::repo::messages::EnvelopeRow {
                        uid: 1,
                        subject: "Quarterly report".into(),
                        date: 1,
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        core.db()
            .write_tx(|tx| {
                db::repo::search::index_message(
                    tx,
                    message,
                    "Quarterly report",
                    "a@example.com",
                    "",
                    "budget",
                )
            })
            .unwrap();

        // 命中的主键要**变成能直接渲染的列表行**（跨文件夹、按相关度序）。
        let hits = core.search_messages("report", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, message);
        assert_eq!(hits[0].subject, "Quarterly report");
        assert!(core.search_messages("nothing", 10).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
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

    /// 写回目标按**角色**找，不按名字：`Sent` / `已发送` / `&XfJT0ZAB-` 都可能是它，
    /// 名字是各服务商自定的。这里故意用 Modified UTF-7 形态的「已发送」落库。
    #[test]
    fn sent_folder_is_found_by_role_not_by_name() {
        let (core, dir) = core("sent-role");
        let account = core
            .add_account(&account_seed("sent-role@example.com"))
            .unwrap();
        for (name, role) in [
            ("Archive", FolderRole::Archive),
            // `&XfJT0ZAB-` = Modified UTF-7 的「已发送」（各服务商叫法不同，角色才稳）。
            ("&XfJT0ZAB-", FolderRole::Sent),
        ] {
            core.db()
                .write_tx(|tx| {
                    db::repo::folders::upsert(
                        tx,
                        account,
                        &db::repo::folders::FolderUpsert {
                            name: name.into(),
                            delimiter: Some("/".into()),
                            role,
                            uidvalidity: None,
                            uidnext: None,
                        },
                    )
                })
                .unwrap();
        }

        let found = core
            .db()
            .read(|conn| db::repo::folders::find_by_role(conn, account, FolderRole::Sent))
            .unwrap()
            .expect("role lookup must find the \\Sent folder");
        assert_eq!(found.name, "&XfJT0ZAB-");
        assert_eq!(found.role, FolderRole::Sent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 账户还没同步过（文件夹表里没有 `\Sent`）时，写回**不连网**就带错误返回。
    ///
    /// 这是无真实 IMAP 服务器环境里唯一能验的写回分支，也钉住了一条口径：
    /// 找不到目标文件夹是「跳过并记日志」，不是「尝试 `APPEND` 到一个猜出来的名字」。
    #[test]
    fn sent_write_back_stops_before_connecting_without_a_sent_folder() {
        let (core, dir) = core("sent-back");
        let account = core
            .add_account(&account_seed("sent-back@example.com"))
            .unwrap();

        let error = core
            .runtime()
            .unwrap()
            .block_on(async {
                write_back_sent(&core, account, "Subject: x\r\n\r\nbody".into()).await
            })
            .unwrap_err();
        // 是 `NotFound`（查不到文件夹）而不是 `Connect` / `Timeout`——说明没去碰网络。
        assert!(matches!(&error, MailError::NotFound(_)), "got {error:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
