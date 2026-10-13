//! 同步引擎。
//!
//! # 两个入口
//!
//! 1. **一次性同步函数** [`sync_account`]：调用方跑一轮，连上、`LIST`、按 UID 水位拉
//!    新信封、落库、断连。适合「用户手动点同步」或宿主没开常驻 actor 的场景。
//! 2. **per-account 常驻 actor** [`spawn_account_actor`]：一轮增量同步之后进入 `IDLE`
//!    长连接等推送，收到变更 / 到 max-idle 再触发增量；连接出错按退避重连；服务端不
//!    支持 `IDLE` 时降级为 `NOOP` 保活 + 定时轮询。两条路径**共用** [`sync_folder`] /
//!    [`sync_account_inner`]，actor 不重写同步逻辑。
//!
//! # 增量算法（与服务器实现无关，在每台服务器上都正确）
//!
//! 1. `SELECT` 读 `UIDVALIDITY` / `UIDNEXT`；
//! 2. `UIDVALIDITY` 与本地记录不一致 → **该文件夹本地清空重建**
//!    （服务端重建过文件夹，旧 UID 全部失效）；
//! 3. 拉 `last_seen_uid + 1 : *` 的信封（只拉头部）；
//! 4. 水位推进到本批最大 UID；
//! 5. **旗标对账**：对**窗口内**（最近 [`FLAG_WINDOW`] 条）逐个比 `\Seen` / `\Flagged`，
//!    只写回不一致的那些（见 [`flag_updates`]）；
//! 6. **删除对账**：`UID SEARCH ALL` 拿服务端的 UID 集合，本地多出来的按 UID 删掉
//!    （见 [`deleted_uids`]）。
//!
//! 5 / 6 两步都**只在窗口 / 集合层面做差**，不做全量 `FETCH FLAGS`——大文件夹上
//! 那是分钟级的重操作（`docs/email-design.md` §4.6）。

use std::collections::HashMap;
use std::time::Duration;

use rusqlite::Connection;

use crate::MailCore;
use crate::db::MailDb;
use crate::db::repo;
use crate::error::{MailError, Result};
use crate::models::{FolderRole, MessageDetail, SyncState};
use crate::proto::ImapConfig;
use crate::proto::imap::{FetchedFlags, IdleOutcome, ImapSession, ListedFolder};

/// 旗标对账的窗口大小（最近多少条参与比对）。
///
/// 取 200：一屏几十行、翻两屏的量级。再大就退化成「整箱拉旗标」，那正是这里要避开的。
pub const FLAG_WINDOW: i64 = 200;

/// 一轮同步的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncReport {
    /// 服务端报告的文件夹总数（含不可选中的）。
    pub folders_seen: usize,
    /// 真正同步了信封的文件夹数（角色已识别的那几个）。
    pub folders_synced: usize,
    /// 新落库的邮件数。
    pub new_messages: usize,
    /// 本地删掉的邮件数（服务端已删除）。
    pub removed_messages: usize,
}

/// 单批拉取的条数上限。**防止第一次同步把几万封信全塞进内存**——
/// 列表本来就分页，先落进去再说。
pub const BATCH_LIMIT: u32 = 500;

/// 同步一个账户（阻塞，调用方负责放进 `background_executor`）。
pub fn sync_account(core: &MailCore, account_id: i64) -> Result<SyncReport> {
    let runtime = core.runtime()?;
    let core = core.clone();
    runtime
        .spawn(async move { sync_account_inner(&core, account_id).await })
        .wait()?
}

async fn sync_account_inner(core: &MailCore, account_id: i64) -> Result<SyncReport> {
    let db = core.db().clone();

    // 先把账户标成「同步中」。
    db.write_tx(|tx| repo::accounts::set_sync_state(tx, account_id, SyncState::Syncing, None))?;
    core.emit(crate::MailEvent::AccountState {
        account_id,
        state: SyncState::Syncing,
        error: None,
    });

    let result = run_sync(core, account_id).await;

    match &result {
        Ok(report) => {
            let db = core.db().clone();
            // 会话聚合：等全部文件夹都落库完再重算一次（线程归属取决于整账号的引用链）。
            // 本轮没有任何增删就不必重算——纯本地读 + 内存归并虽然便宜，但没必要白跑。
            if report.new_messages > 0 || report.removed_messages > 0 {
                rebuild_threads(&db, account_id);
            }
            let _ = db.write_tx(|tx| {
                repo::accounts::set_sync_state(tx, account_id, SyncState::Idle, None)
            });
            core.emit(crate::MailEvent::AccountState {
                account_id,
                state: SyncState::Idle,
                error: None,
            });
            core.emit(crate::MailEvent::SearchIndexUpdated { account_id });
            tracing::info!(
                account_id,
                folders = report.folders_seen,
                synced = report.folders_synced,
                new = report.new_messages,
                removed = report.removed_messages,
                "mail sync finished"
            );
            // 收尾的后台预取：把收件箱里最新几封还没正文的抓下来，用户点开时就不必
            // 再等一次 IMAP 往返。排在**状态已回到 `Idle`、事件已发之后**——它不该
            // 拖长用户感知的「同步完成」。预取失败不影响同步结论，所以只记日志。
            if let Err(error) = prefetch_bodies_inner(core, account_id, PREFETCH_BODY_LIMIT).await {
                tracing::debug!(account_id, %error, "body prefetch skipped");
            }
        }
        Err(e) => {
            let db = core.db().clone();
            let _ = db.write_tx(|tx| {
                repo::accounts::set_sync_state(
                    tx,
                    account_id,
                    SyncState::Error,
                    Some(&e.to_string()),
                )
            });
            core.emit(crate::MailEvent::AccountState {
                account_id,
                state: SyncState::Error,
                error: Some(e.to_string()),
            });
            tracing::warn!(account_id, error = %e, "mail sync failed");
        }
    }
    result
}

async fn run_sync(core: &MailCore, account_id: i64) -> Result<SyncReport> {
    let db = core.db().clone();
    let (account, password) = crate::accounts::load_with_password(&db, core.secrets(), account_id)?;

    let cfg = ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        security: account.security,
        username: account.username.clone(),
        password,
        auth: account.auth_kind,
    };

    let mut session = ImapSession::connect(&cfg).await?;
    let listed = session.list_folders().await?;
    let mut report = SyncReport {
        folders_seen: listed.len(),
        ..Default::default()
    };

    // 落库文件夹清单（并删掉服务端已经不在的）。
    let keep: Vec<String> = listed.iter().map(|f| f.name.clone()).collect();
    let folder_ids = persist_folders(&db, account_id, &listed)?;
    db.write_tx(|tx| repo::folders::delete_missing(tx, account_id, &keep))?;
    core.emit(crate::MailEvent::FoldersChanged {
        account_id,
        folder_count: keep.len(),
    });

    // 只同步「角色已识别」的文件夹。没认出来的（用户自建的大量分类夹）等点开再拉——
    // 首轮就把所有文件夹拉一遍，在有几万个文件夹的企业账号上会跑很久。
    for (folder_id, name, role) in folder_ids {
        if role == FolderRole::Other {
            continue;
        }
        match sync_folder(&db, &mut session, account_id, folder_id, &name).await {
            Ok((new, removed)) => {
                report.folders_synced += 1;
                report.new_messages += new;
                report.removed_messages += removed;
                core.emit(crate::MailEvent::MessagesAdded {
                    account_id,
                    folder_id,
                    count: new as u64,
                });
            }
            Err(e) => {
                // 单个文件夹失败不该让整轮同步失败——记下来，下一个继续。
                tracing::warn!(account_id, folder = %name, error = %e, "folder sync failed");
            }
        }
    }

    session.logout().await;
    Ok(report)
}

// ---------------------------------------------------------------------------
// per-account 常驻 actor（IDLE 推送 + 自动重连）
// ---------------------------------------------------------------------------

/// IDLE 单次等待的上限：到点主动重入 IDLE，并跑一次兜底增量同步。
///
/// 25 分钟：RFC 2177 建议客户端 ≤ 29 分钟重入一次（否则服务端可能因 inactivity
/// 把连接踢掉），留出余量。
const IDLE_MAX_WAIT: Duration = Duration::from_secs(25 * 60);

/// 断开后的重连退避（起步 / 上限）。
const RECONNECT_MIN: Duration = Duration::from_secs(5);
const RECONNECT_MAX: Duration = Duration::from_secs(5 * 60);

/// 服务端不支持 IDLE 时的 `NOOP` 轮询周期（回落路径）。
const NOOP_POLL: Duration = Duration::from_secs(60);

/// 起一个账户的常驻同步 actor。
///
/// 幂等由调用方 [`crate::MailCore::start_sync_actor`] 保证（先查
/// `MailRuntime::has_resident`）；这里只管把循环体投进 runtime。
pub(crate) fn spawn_account_actor(core: &MailCore, account_id: i64) -> Result<()> {
    let runtime = core.runtime()?;
    let core = core.clone();
    runtime.spawn_resident(account_id, move |stop, wake| async move {
        account_actor_loop(core, account_id, stop, wake).await;
    });
    Ok(())
}

/// actor 的**监督层**：反复跑一轮会话，出错就退避重连，直到被 stop。
///
/// 一轮长连接会一直跑到出错或 stop；连接断了（IMAP 错误 / 网络抖动 / 服务器重启）
/// 不该让推送永久停摆，所以这里必须重连。退避可被打断——stop 时 `wake` 立刻唤醒，
/// 不必等满整个退避窗口。
async fn account_actor_loop(
    core: MailCore,
    account_id: i64,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    wake: std::sync::Arc<tokio::sync::Notify>,
) {
    use std::sync::atomic::Ordering;

    let mut backoff = RECONNECT_MIN;
    loop {
        if stop.load(Ordering::Relaxed) {
            tracing::debug!(account_id, "mail sync actor stopped");
            return;
        }
        match actor_session(&core, account_id, &stop, &wake).await {
            // `Ok` 只在收到 stop 时返回；退避重置后循环由顶部判断退出。
            Ok(()) => backoff = RECONNECT_MIN,
            Err(e) => {
                tracing::warn!(account_id, error = %e, "mail sync actor session failed; reconnecting");
                let _ = core.db().write_tx(|tx| {
                    repo::accounts::set_sync_state(
                        tx,
                        account_id,
                        SyncState::Offline,
                        Some(&e.to_string()),
                    )
                });
                core.emit(crate::MailEvent::AccountState {
                    account_id,
                    state: SyncState::Offline,
                    error: Some(e.to_string()),
                });
                // 可被打断的退避。
                let sleep = tokio::time::sleep(backoff);
                tokio::select! {
                    _ = sleep => {}
                    _ = wake.notified() => {}
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}

/// 一轮长连接会话：先做一次增量同步，再 `SELECT` 关注文件夹并进入 IDLE 等待循环。
///
/// 只有收到 stop 才返回 `Ok(())`；任何 IMAP 错误都返回 `Err` 交给监督层重连。
async fn actor_session(
    core: &MailCore,
    account_id: i64,
    stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    wake: &std::sync::Arc<tokio::sync::Notify>,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    let db = core.db().clone();

    // 第一轮先把「文件夹清单 + 各夹水位」拉到最新（复用既有同步函数，actor 不重写
    // 同步逻辑）。这一轮自己管理连接，跑完即断——IDLE 连接随后单独建立。
    sync_account_inner(core, account_id).await?;

    let (account, password) = crate::accounts::load_with_password(&db, core.secrets(), account_id)?;
    let cfg = ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        security: account.security,
        username: account.username.clone(),
        password,
        auth: account.auth_kind,
    };

    let mut session = ImapSession::connect(&cfg).await?;
    let supports_idle = session.caps().idle;

    let Some(watch) = watch_folder(&db, account_id)? else {
        // 一个可选文件夹都没有：退化成「NOOP 保活 + 定时全量同步」，直到 stop。
        tracing::debug!(account_id, "no selectable folder to idle on; polling");
        loop {
            session.noop().await?;
            if wait_or_stop(NOOP_POLL, stop, wake).await {
                session.logout().await;
                return Ok(());
            }
            let _ = sync_account_inner(core, account_id).await;
        }
    };

    session.select(&watch).await?;
    if !supports_idle {
        tracing::info!(
            account_id,
            "server has no IDLE capability; falling back to NOOP polling"
        );
    }

    loop {
        if stop.load(Ordering::Relaxed) {
            session.logout().await;
            return Ok(());
        }

        if supports_idle {
            // 收到变更 / 到点，两者都触发一次增量同步（复用 `sync_folder`）。
            let (next, outcome) = session.idle_once(IDLE_MAX_WAIT).await?;
            session = next;
            match outcome {
                IdleOutcome::Changed => {
                    tracing::debug!(account_id, "idle: server reported a change")
                }
                IdleOutcome::Timeout => tracing::debug!(account_id, "idle: max-idle reached"),
                // `supports_idle` 为真时不该出现；真出现就当到点处理。
                IdleOutcome::Unsupported => {}
            }
        } else {
            // 回落：NOOP 保活（让服务端有机会推送），再等一个轮询周期。
            session.noop().await?;
            if wait_or_stop(NOOP_POLL, stop, wake).await {
                session.logout().await;
                return Ok(());
            }
        }

        if let Err(e) = sync_all_with_session(core, &mut session, account_id).await {
            tracing::warn!(account_id, error = %e, "incremental sync after idle failed");
        }

        // IDLE 会让邮箱脱出「选中」状态，下一轮 IDLE 前必须重新 SELECT。
        session.select(&watch).await?;
    }
}

/// 等 `duration` 或被 stop 唤醒；返回是否应当停止。
async fn wait_or_stop(
    duration: Duration,
    stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    wake: &std::sync::Arc<tokio::sync::Notify>,
) -> bool {
    use std::sync::atomic::Ordering;
    let sleep = tokio::time::sleep(duration);
    tokio::select! {
        _ = sleep => {}
        _ = wake.notified() => {}
    }
    stop.load(Ordering::Relaxed)
}

/// 关注哪个文件夹做 IDLE：优先收件箱，否则第一个已识别角色的文件夹。
fn watch_folder(db: &crate::db::MailDb, account_id: i64) -> Result<Option<String>> {
    if let Some(folder) =
        db.read(|conn| repo::folders::find_by_role(conn, account_id, FolderRole::Inbox))?
    {
        return Ok(Some(folder.name));
    }
    let folders = db.read(|conn| repo::folders::list(conn, account_id))?;
    Ok(folders
        .into_iter()
        .find(|f| f.role != FolderRole::Other)
        .map(|f| f.name))
}

/// 在**既有连接**上对每个已识别角色的文件夹跑一轮增量同步（复用 [`sync_folder`]）。
///
/// 单夹失败只记日志，不让整轮失败（同 [`run_sync`] 的口径）。有新增就广播
/// [`crate::MailEvent::MessagesAdded`]。
async fn sync_all_with_session(
    core: &MailCore,
    session: &mut ImapSession,
    account_id: i64,
) -> Result<()> {
    let db = core.db().clone();
    let folders = db.read(|conn| repo::folders::list(conn, account_id))?;

    let mut changed = false;
    for folder in folders {
        if folder.role == FolderRole::Other {
            continue;
        }
        match sync_folder(&db, session, account_id, folder.id, &folder.name).await {
            Ok((new, removed)) => {
                if new > 0 || removed > 0 {
                    changed = true;
                }
                if new > 0 {
                    core.emit(crate::MailEvent::MessagesAdded {
                        account_id,
                        folder_id: folder.id,
                        count: new as u64,
                    });
                }
            }
            Err(e) => {
                tracing::warn!(account_id, folder = %folder.name, error = %e, "incremental folder sync failed")
            }
        }
    }

    // 会话聚合：本轮有增删才重算（见 `sync_account_inner` 同款注释）。
    if changed {
        rebuild_threads(&db, account_id);
    }
    core.emit(crate::MailEvent::SearchIndexUpdated { account_id });
    // 增量轮结束后账户回到「已同步」（actor 常驻不代表账户一直在同步）。
    let _ = db.write_tx(|tx| repo::accounts::set_sync_state(tx, account_id, SyncState::Idle, None));
    core.emit(crate::MailEvent::AccountState {
        account_id,
        state: SyncState::Idle,
        error: None,
    });
    Ok(())
}

/// 重算某账号的会话聚合（`threads` + `messages.thread_id`）。
///
/// **失败只记日志、不上抛**：会话合并是同步之后的「锦上添花」，一次成功的同步不该
/// 因为它出错被判为失败——那会让用户看到「同步失败」，而信其实已经收下来了。
fn rebuild_threads(db: &crate::db::MailDb, account_id: i64) {
    match db.write_tx(|tx| repo::threads::rebuild_for_account(tx, account_id)) {
        Ok(threads) => tracing::debug!(account_id, threads, "mail threads rebuilt"),
        Err(e) => tracing::warn!(account_id, error = %e, "mail thread rebuild failed"),
    }
}

/// 把 `LIST` 的结果写进库，返回 `(folder_id, name, role)`。
fn persist_folders(
    db: &crate::db::MailDb,
    account_id: i64,
    listed: &[ListedFolder],
) -> Result<Vec<(i64, String, FolderRole)>> {
    let mut out = Vec::new();
    for f in listed {
        let role = FolderRole::from_imap(&f.name, &f.attributes);
        let id = db.write_tx(|tx| {
            repo::folders::upsert(
                tx,
                account_id,
                &repo::folders::FolderUpsert {
                    name: f.name.clone(),
                    delimiter: f.delimiter.clone(),
                    role,
                    // uidvalidity / uidnext 在 SELECT 之后才知道，这里先不动
                    // （upsert 用 excluded 覆盖，所以传 None 会清掉——故先读后写）。
                    uidvalidity: None,
                    uidnext: None,
                },
            )
        })?;
        out.push((id, f.name.clone(), role));
    }
    Ok(out)
}

/// 同步一个文件夹，返回 `(新增, 删除)`。
async fn sync_folder(
    db: &crate::db::MailDb,
    session: &mut ImapSession,
    account_id: i64,
    folder_id: i64,
    name: &str,
) -> Result<(usize, usize)> {
    let state = session.select(name).await?;

    // UIDVALIDITY 变了 = 服务端把这个文件夹重建了，本地 UID 全部失效。
    let local_uidvalidity = db.read(|conn| repo::folders::uidvalidity(conn, folder_id))?;
    let invalidated = match (local_uidvalidity, state.uid_validity.map(i64::from)) {
        (Some(old), Some(new)) => old != new,
        _ => false,
    };
    if invalidated {
        tracing::info!(folder = %name, "uidvalidity changed; dropping local cache for this folder");
        db.write_tx(|tx| clear_folder(tx, folder_id))?;
    }

    let last_seen = if invalidated {
        0
    } else {
        db.read(|conn| repo::folders::get(conn, folder_id))?
            .last_seen_uid
    };
    let from_uid = last_seen.saturating_add(1).max(1) as u32;

    let envelopes = session.fetch_envelopes(from_uid).await?;
    let mut new_count = 0usize;
    let mut max_uid = last_seen;

    for fetched in envelopes.iter().take(BATCH_LIMIT as usize) {
        // 单封邮件头部解析不了不该让整个文件夹同步失败：跳过它、继续后面的。
        // （真实世界里会遇到结构损坏、或服务端回了半截头部的信；一封坏信把整个
        // 收件箱卡住，用户看到的是「同步一直失败」。）
        let headers = match crate::mime::parse_headers(&fetched.header) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(uid = fetched.uid, error = %e, "skipping message with unparsable headers");
                // 水位仍要推进过它——否则下次同步还会撞上同一封，永远卡在这里。
                max_uid = max_uid.max(i64::from(fetched.uid));
                continue;
            }
        };
        let snippet = headers.subject.clone();
        let row = repo::messages::EnvelopeRow {
            uid: i64::from(fetched.uid),
            message_id: headers.message_id.clone(),
            subject: headers.subject.clone(),
            from_name: headers.from_name.clone(),
            from_addr: headers.from_addr.clone(),
            to: headers.to.clone(),
            cc: headers.cc.clone(),
            date: headers.date,
            is_read: fetched.seen,
            is_starred: fetched.flagged,
            // 附件要等正文拉下来才知道；列表阶段先按头部猜（有 attachment 字样才置位）。
            has_attachments: false,
            size: fetched.size.map(i64::from),
            snippet,
        };
        let message_id =
            db.write_tx(|tx| repo::messages::upsert_envelope(tx, account_id, folder_id, &row))?;
        // 引用链落 `message_refs`——会话合并（`threading`）靠它把往来邮件串起来。
        // 头部本来就拉下来了（`BODY.PEEK[HEADER]`），这里不额外连网。
        db.write_tx(|tx| repo::messages::replace_references(tx, message_id, &headers.references))?;
        // 同步顺手建索引，检索不必等正文。
        db.write_tx(|tx| {
            repo::search::index_message(
                tx,
                message_id,
                &row.subject,
                &row.from_addr.clone().unwrap_or_default(),
                &row.to.join(", "),
                &row.snippet,
            )
        })?;
        new_count += 1;
        max_uid = max_uid.max(i64::from(fetched.uid));
    }

    // 水位推进 + 记录 UIDVALIDITY / UIDNEXT。
    db.write_tx(|tx| {
        repo::folders::set_uid_state(
            tx,
            folder_id,
            state.uid_validity.map(i64::from),
            state.uid_next.map(i64::from),
            max_uid,
        )
    })?;

    // 旗标与删除对账。两者都建立在「`SELECT` 成功、UIDVALIDITY 已核对」之后，
    // 所以只可能是「服务器上的这一批变了」，不可能是本地缓存整体作废。
    if !invalidated {
        match reconcile_flags(db, session, folder_id).await {
            Ok(changed) if changed > 0 => {
                tracing::debug!(folder = %name, changed, "mail flags reconciled")
            }
            Ok(_) => {}
            // 对账失败不该让整轮同步失败：新邮件已经落库了，旗标下次再对。
            Err(e) => tracing::warn!(folder = %name, error = %e, "flag reconciliation failed"),
        }
    }
    let removed = match reconcile_deletions(db, session, folder_id).await {
        Ok(removed) => removed,
        Err(e) => {
            tracing::warn!(folder = %name, error = %e, "delete reconciliation failed");
            0
        }
    };

    Ok((new_count, removed))
}

/// 窗口内旗标对账：拉服务端旗标，只写回本地不一致的那些。返回改了几条。
async fn reconcile_flags(
    db: &crate::db::MailDb,
    session: &mut ImapSession,
    folder_id: i64,
) -> Result<usize> {
    let local = db.read(|conn| repo::messages::flags_window(conn, folder_id, FLAG_WINDOW))?;
    if local.is_empty() {
        return Ok(0);
    }

    // 用 `min:max` 而不是逐个 UID 列表：命令更短，多拉回来的 UID 在下面按窗口过滤掉。
    let min = local.iter().map(|(uid, _, _)| *uid).min().unwrap_or(1);
    let max = local.iter().map(|(uid, _, _)| *uid).max().unwrap_or(min);
    let remote = session.fetch_flags(&format!("{min}:{max}")).await?;

    let updates = flag_updates(&local, &remote);
    let changed = updates.len();
    if changed > 0 {
        db.write_tx(|tx| {
            for (uid, is_read, is_starred) in &updates {
                repo::messages::set_flags_by_uid(tx, folder_id, *uid, *is_read, *is_starred)?;
            }
            Ok(())
        })?;
    }
    Ok(changed)
}

/// 删除对账：本地有、服务端没有的 UID 就是被别的设备删掉的。返回删了几条。
async fn reconcile_deletions(
    db: &crate::db::MailDb,
    session: &mut ImapSession,
    folder_id: i64,
) -> Result<usize> {
    let local = db.read(|conn| repo::messages::existing_uids(conn, folder_id))?;
    if local.is_empty() {
        return Ok(0);
    }
    // ⚠️ 顺序要紧：**先**取到服务端的完整集合，**再**删本地。搜索失败就直接返回，
    // 绝不能拿一个空集合去当「服务端什么都没有」——那会把整箱缓存清空。
    let remote = session.search_all_uids().await?;

    let gone = deleted_uids(&local, &remote);
    if gone.is_empty() {
        return Ok(0);
    }
    let removed = db.write_tx(|tx| repo::messages::delete_by_uids(tx, folder_id, &gone))?;
    tracing::info!(
        folder_id,
        candidates = gone.len(),
        removed,
        "dropped locally cached messages that the server no longer has"
    );
    Ok(removed)
}

/// 纯函数：本地与服务端旗标不一致的 `(uid, is_read, is_starred)`。
///
/// 服务端没回某个 UID（窗口边界、或它已经不在服务端）时不改本地——删不删由
/// [`deleted_uids`] 决定，这里只管旗标。
fn flag_updates(local: &[(i64, bool, bool)], remote: &[FetchedFlags]) -> Vec<(i64, bool, bool)> {
    let by_uid: HashMap<u32, (bool, bool)> = remote
        .iter()
        .map(|f| (f.uid, (f.seen, f.flagged)))
        .collect();
    let mut out = Vec::new();
    for (uid, is_read, is_starred) in local {
        let Ok(key) = u32::try_from(*uid) else {
            continue;
        };
        let Some((seen, flagged)) = by_uid.get(&key) else {
            continue;
        };
        if *is_read != *seen || *is_starred != *flagged {
            out.push((*uid, *seen, *flagged));
        }
    }
    out
}

/// 纯函数：本地有、服务端没有的 UID。
fn deleted_uids(local: &[i64], remote: &[u32]) -> Vec<i64> {
    let present: std::collections::HashSet<u32> = remote.iter().copied().collect();
    local
        .iter()
        .filter(|uid| match u32::try_from(**uid) {
            Ok(key) => !present.contains(&key),
            // 超出 u32 的 UID 不可能是 IMAP UID，留着不动（删它是我们自己的 bug）。
            Err(_) => false,
        })
        .copied()
        .collect()
}

/// 清空一个文件夹的本地缓存（UIDVALIDITY 失效时调）。
fn clear_folder(conn: &Connection, folder_id: i64) -> Result<()> {
    // 先删索引（FTS 表没有外键级联），再删邮件。
    let uids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM messages WHERE folder_id = ?1")?;
        let rows = stmt.query_map([folder_id], |row| row.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for id in uids {
        repo::search::remove_message(conn, id)?;
    }
    conn.execute("DELETE FROM messages WHERE folder_id = ?1", [folder_id])?;
    Ok(())
}

// 旗标与删除对账**在上面**（`reconcile_flags` / `reconcile_deletions`），随每轮同步跑。
// 两条纪律：① 都建立在「服务端的答复拿到了」之上——搜索失败就直接返回，绝不把
// 「搜不到」当成「服务端空了」；② 对账失败只记日志，不让整轮同步失败（新邮件已经
// 落库了，旗标下次再对）。

// ---------------------------------------------------------------------------
// 正文按需拉取
// ---------------------------------------------------------------------------

/// 按需拉取一封邮件的正文（`BODY.PEEK[]` 全量 → MIME 解码 → 落库）。
///
/// 列表阶段只拉了 `BODY.PEEK[HEADER]`（几百字节），正文要等用户真的点开某一封
/// 才拉——首轮就把整个收件箱的正文全拖下来，在几万封的账号上是分钟级甚至更久，
/// 而用户真正会读的可能只有最上面几封（`docs/email-design.md` §4.6）。
///
/// **幂等**：`body_state == "cached"` 时直接回本地副本，不重复连服务器。
pub fn fetch_body(core: &MailCore, message_id: i64) -> Result<MessageDetail> {
    let runtime = core.runtime()?;
    let core = core.clone();
    runtime
        .spawn(async move { fetch_body_inner(&core, message_id).await })
        .wait()?
}

async fn fetch_body_inner(core: &MailCore, message_id: i64) -> Result<MessageDetail> {
    let db = core.db().clone();
    let detail = db.read(|conn| repo::messages::get_detail(conn, message_id))?;

    // 已经有本地副本：直接给，不再连一次服务器。
    // （`fetching` 不在这里短路——那说明另一次拉取正在进行，交给下面的原子抢占
    //  判定：抢不到就回现有详情，等对方拉完推 `MailEvent::BodyReady`。）
    if detail.body_state == "cached" {
        return Ok(detail);
    }

    let folder_id = detail
        .folder_id
        .ok_or_else(|| MailError::NotFound(format!("folder of message {message_id}")))?;
    let uid = detail
        .uid
        .ok_or_else(|| MailError::NotFound(format!("uid of message {message_id}")))?;

    let folder = db.read(|conn| repo::folders::get(conn, folder_id))?;
    let (account, password) =
        crate::accounts::load_with_password(&db, core.secrets(), detail.account_id)?;

    // **原子抢占**：只有 `none` 才会被置成 `fetching`。抢不到说明已经有人在拉这一封
    // （前台点开撞上后台预取，或同一封被连着点开两次）。这时绝不重复发起网络请求，
    // 直接把现有详情给出去——真正的正文会在对方完成后由 `MailEvent::BodyReady`
    // 送到界面（`pages/mail.rs` 的 `apply_event` 会重读本地并重绘）。
    //
    // 重复拉一次的代价不只是多一次往返：两次 `save_body` 都会跑，`replace_attachments`
    // 也会跑两次，后写的赢——白做功，还可能让界面上刚渲染出来的正文闪一下。
    let mut claimed = db.write_tx(|tx| repo::messages::claim_body_fetch(tx, message_id))?;
    if !claimed {
        // 抢不到 = 预取（或上一次点开）正在拉这一封。用户**正看着它**，不能把「后台
        // 慢慢拉」的节奏甩给前台：盯着状态位等在途那次拉取结束——它落库就立刻回本地
        // 副本；它失败放回 `none` 就再抢一次，自己拉。等不到（拉取方卡死在
        // `fetching`，只有崩溃能造成；启动时会清理）才把现有详情给出去，`BodyReady`
        // 之后仍会补刷。
        const WAIT: std::time::Duration = std::time::Duration::from_secs(15);
        const TICK: std::time::Duration = std::time::Duration::from_millis(250);
        let deadline = tokio::time::Instant::now() + WAIT;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(TICK).await;
            let current = db.read(|conn| repo::messages::get_detail(conn, message_id))?;
            match current.body_state.as_str() {
                "cached" => return Ok(current),
                "none" => {
                    claimed = db.write_tx(|tx| repo::messages::claim_body_fetch(tx, message_id))?;
                    if claimed {
                        break;
                    }
                }
                _ => {}
            }
        }
        if !claimed {
            tracing::debug!(
                message_id,
                "body fetch wait timed out; returning the local copy"
            );
            return Ok(detail);
        }
    }

    let cfg = ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        security: account.security,
        username: account.username.clone(),
        password,
        auth: account.auth_kind,
    };

    let result = fetch_body_remote(&cfg, &folder.name, uid).await;
    finish_body_fetch(core, &db, message_id, result).await
}

/// 拉取结果收尾：失败释放抢占、成功落库 + 推 `BodyReady`。
///
/// 从 [`fetch_body_inner`] 拆出来是因为预取走**另一条网络路径**（复用一条会话，
/// 见 [`prefetch_bodies_inner`]），但「失败释放 / 成功落库 / 推事件」的口径必须
/// 只有一份——两条路径各自手写一遍，迟早有一边忘记释放。
async fn finish_body_fetch(
    core: &MailCore,
    db: &MailDb,
    message_id: i64,
    result: Result<(Vec<u8>, crate::mime::ParsedMessage)>,
) -> Result<MessageDetail> {
    let (raw, parsed) = match result {
        Ok(v) => v,
        Err(e) => {
            // 失败要把状态退回去，否则下一帧界面还显示「正在下载正文」，而其实
            // 已经结束了——一个永远转不完的加载态。走 `release_body_fetch`（只在
            // 仍为 `fetching` 时才退）而不是无条件置 `none`：否则一旦 `save_body`
            // 已经写成功，会被这条失败路径打回 `none`。
            let _ = db.write_tx(|tx| repo::messages::release_body_fetch(tx, message_id));
            return Err(e);
        }
    };
    tracing::debug!(message_id, bytes = raw.len(), "mail body fetched");

    let attachment_rows: Vec<repo::messages::NewAttachment> = parsed
        .attachments
        .iter()
        .map(|a| repo::messages::NewAttachment {
            filename: a.filename.clone(),
            mime_type: a.mime_type.clone(),
            size: a.size,
            content_id: a.content_id.clone(),
            is_inline: a.is_inline,
            // 附件字节**不落库**（`docs/email-design.md` §4.5：附件按需下载到磁盘，
            // 库里只存清单）。这里只登记元数据，`file_path` 留空。
            file_path: None,
            // IMAP 节号从 MIME 结构推出来（`mime::section_map`），下载时用它
            // `BODY.PEEK[<section>]` 只取这一个部件。推不出可靠段号时为 `None`，
            // 下载路径会降级为整封取回。
            imap_section: a.imap_section.clone(),
        })
        .collect();

    // CID → `data:` 内联。**必须在这里做**：附件字节按 §4.5 不落库，解析这一次是
    // 唯一拿得到它们的时刻；落库之后再渲染就没有字节可以内联了。
    let html_body = parsed.html_body.as_deref().map(|html| {
        crate::mime::inline_cid_images(html, &parsed.attachments, crate::mime::INLINE_BUDGET_BYTES)
    });

    db.write_tx(|tx| {
        repo::messages::save_body(
            tx,
            message_id,
            parsed.text_body.as_deref(),
            html_body.as_deref(),
        )?;
        repo::messages::replace_attachments(tx, message_id, &attachment_rows)?;
        repo::messages::set_has_attachments(tx, message_id, !attachment_rows.is_empty())?;
        Ok(())
    })?;

    let detail = db.read(|conn| repo::messages::get_detail(conn, message_id))?;
    core.emit(crate::MailEvent::BodyReady { message_id });
    Ok(detail)
}

/// 一轮后台预取的规模上限（封）。
///
/// 20 是「一屏多几行、够覆盖用户打开后头几封」的量级。每封正文一次 IMAP 往返，
/// 再大就会让弱网下的一轮同步明显变长（预取排在同步收尾之后，但仍在这个任务里）。
pub const PREFETCH_BODY_LIMIT: i64 = 20;

/// 后台预取：把收件箱里**最新**的、还没拉过正文的若干封正文抓下来（阻塞版）。
///
/// # 为什么要有这一步
///
/// 列表阶段只拉 `BODY.PEEK[HEADER]`（§4.6 的取舍：首轮不把几万封信的正文全拖下来），
/// 正文原先只在用户点开时才拉。弱网下这意味着**每次点开都要等一次往返**——用户的
/// 第一反应是「邮件怎么打不开」。预取把这份等待挪到后台：同步收尾后顺手抓最近
/// [`PREFETCH_BODY_LIMIT`] 封，用户点开时大概率已经命中本地副本（[`fetch_body`] 的
/// `cached` 短路直接回本地，不连网）。
///
/// 「分批」的含义是**本轮 20 封、下轮再 20 封**：每轮同步各推进一批，不会一次性把
/// 整个收件箱拖空（对照 Flectar 的 `missing_bodies_at` + `pending_body_page` 分页）。
///
/// # 只做收件箱
///
/// 用户真正会读的就是收件箱。把所有文件夹都预取会让首轮流量翻好几倍，而「已发送 /
/// 垃圾箱」里的正文基本没人看。
///
/// **best-effort**：任何一封失败都只记日志并**结束本轮**——连续失败通常说明网络或
/// 认证出了问题，硬撑完 20 次只会白等 20 个超时。
pub fn prefetch_bodies(core: &MailCore, account_id: i64, limit: i64) -> Result<usize> {
    let runtime = core.runtime()?;
    let core = core.clone();
    runtime
        .spawn(async move { prefetch_bodies_inner(&core, account_id, limit).await })
        .wait()?
}

/// [`prefetch_bodies`] 的异步本体。
///
/// **调用方若已经在 runtime 任务里（例如 [`sync_account_inner`] 的收尾），必须直接
/// `.await` 它**，不要走上面的同步包装——那会 `spawn` 一个新任务再 `wait`，在
/// worker 线程数少时叠出「等自己」的嵌套等待。
///
/// # 一条连接拉完整轮
///
/// 早先每封都走一遍 `connect → SELECT → fetch → logout`（[`fetch_body_inner`] 的
/// 路径），20 封就是 20 次登录——163 这类服务端单次登录要 1~3s，一轮预取拖到几十
/// 秒，用户在这中间点开邮件就只能看「正文还没下载」的兜底文案。预取的 id 全部来自
/// 同一个收件箱，`connect` 一次、`SELECT` 一次、逐封 `UID FETCH` 就够了；单封的
/// 抢占 / 落库 / 释放口径不变（[`finish_body_fetch`]）。
async fn prefetch_bodies_inner(core: &MailCore, account_id: i64, limit: i64) -> Result<usize> {
    let db = core.db().clone();
    let folder =
        db.read(|conn| repo::folders::find_by_role(conn, account_id, FolderRole::Inbox))?;
    let Some(folder) = folder else {
        // 还没有识别出收件箱（首轮 LIST 之前、或服务端没给 `\Inbox`）：不预取。
        return Ok(0);
    };
    let ids = db.read(|conn| repo::messages::missing_body_ids(conn, folder.id, limit))?;
    if ids.is_empty() {
        return Ok(0);
    }

    let (account, password) = crate::accounts::load_with_password(&db, core.secrets(), account_id)?;
    let cfg = ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        security: account.security,
        username: account.username.clone(),
        password,
        auth: account.auth_kind,
    };
    let mut session = match ImapSession::connect(&cfg).await {
        Ok(session) => session,
        Err(error) => {
            tracing::debug!(account_id, %error, "body prefetch could not connect");
            return Ok(0);
        }
    };
    if let Err(error) = session.select(&folder.name).await {
        tracing::debug!(account_id, %error, "body prefetch could not select the inbox");
        let _ = session.logout().await;
        return Ok(0);
    }

    let mut done = 0usize;
    for message_id in ids {
        // 单封仍走完整的「抢占 → 拉取 → 落库 → `BodyReady` → 失败释放」口径，只是
        // 网络部分复用这条会话。已缓存 / 前台正在拉的（抢不到）直接跳过。
        let claimed = db.write_tx(|tx| repo::messages::claim_body_fetch(tx, message_id))?;
        if !claimed {
            continue;
        }
        let uid = match db.read(|conn| repo::messages::get_detail(conn, message_id)) {
            Ok(detail) => detail.uid,
            Err(error) => {
                tracing::debug!(message_id, %error, "body prefetch could not read the uid");
                break;
            }
        };
        let Some(uid) = uid else {
            // 没有 UID 的行（本地草稿之类混进来）不该占预取名额，放回 `none`。
            let _ = db.write_tx(|tx| repo::messages::release_body_fetch(tx, message_id));
            continue;
        };
        let result = fetch_from_session(&mut session, uid).await;
        match finish_body_fetch(core, &db, message_id, result).await {
            Ok(_) => done += 1,
            Err(error) => {
                tracing::debug!(message_id, %error, "body prefetch stopped early");
                break;
            }
        }
    }
    let _ = session.logout().await;
    if done > 0 {
        tracing::debug!(account_id, fetched = done, "mail body prefetch finished");
    }
    Ok(done)
}

// ---------------------------------------------------------------------------
// 附件按需下载
// ---------------------------------------------------------------------------

/// 一次「另存为附件」的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSaved {
    /// 落盘路径（= 调用方给的目标路径）。
    pub path: std::path::PathBuf,
    /// 附件文件名（提示文案用）。
    pub filename: String,
    /// 落盘的字节数。
    pub bytes: usize,
    /// 命中的是磁盘上已有的文件——**没有连网**。
    pub from_cache: bool,
}

/// 把一个附件落到 `dest`（阻塞；调用方负责放进后台执行器）。
///
/// # 两步走：先入应用缓存，再拷到用户位置
///
/// 1. **应用管理目录**（`{app_data_dir}/mail/attachments/<message_id>/<attachment_id>-<名>`，
///    见 [`crate::MailPaths::attachment_cache_path`]）：第一次按 IMAP 节取回部件、
///    解码、以 0600 权限落盘，并把路径回写 `attachments.file_path`。
/// 2. **用户选的 `dest`**：把缓存拷过去（`duck_platform::save_file` 选的位置）。
///
/// 这样「同一封邮件的第二个附件」不会各拉一份；用户再点一次「另存为」是纯本地拷贝。
///
/// # 按节取，而不是整封取
///
/// `BODY.PEEK[<section>]` 只取目标部件；`section` 是索引阶段从 MIME 结构推出的
/// （`mime::section_map`）。拿不到可靠段号（或服务端没回 MIME 头导致解不出编码）时
/// **降级**为整封取回 + MIME 解码——正确性优先，代价是多传正文那几十 KB。
///
/// # 幂等与防护
///
/// - 缓存命中（文件在且字节数与清单一致）就直接用，**不连网**；
/// - 缓存路径由 [`crate::MailPaths::attachment_cache_path`] 拼装并校验，文件名经
///   `sanitize_filename` 清洗，`../` 之类穿越不出去；
/// - 单附件字节数有 [`MAX_ATTACHMENT_BYTES`] 上限。
///
/// # 与 `mime::parse` 的顺序耦合
///
/// 降级路径用 `attachments.ordinal`（见 [`repo::messages::attachment_target`]）索引
/// 解析结果，但**必须复核** `filename` + `size`：序号只是「同一批写入」的巧合，不是
/// 外键。对不上时退回「按文件名 + 字节数再找一遍」，找不到就报错——绝不把另一份内容
/// 写进用户以为的那个文件里。
pub fn download_attachment(
    core: &MailCore,
    attachment_id: i64,
    dest: &std::path::Path,
) -> Result<AttachmentSaved> {
    let runtime = core.runtime()?;
    let core = core.clone();
    let dest = dest.to_path_buf();
    runtime
        .spawn(async move { download_attachment_inner(&core, attachment_id, &dest).await })
        .wait()?
}

/// 单个附件允许下载的字节上限。
///
/// 50 MB：既挡得住服务端谎报 / 清单过期导致的超大部件，又比「真实大附件」留足余量。
/// 超限**在下网络之前**就先判一次（清单里的 `size`），下载后再判一次（真实字节数）。
pub const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

async fn download_attachment_inner(
    core: &MailCore,
    attachment_id: i64,
    dest: &std::path::Path,
) -> Result<AttachmentSaved> {
    let db = core.db().clone();
    let target = db.read(|conn| repo::messages::attachment_target(conn, attachment_id))?;

    // 上限先于网络动作：清单里的 size 已知（> 0）且超限时直接拒绝。
    if target.size > MAX_ATTACHMENT_BYTES as i64 {
        return Err(too_large(&target, target.size as usize));
    }

    // 应用管理目录内的缓存落点（拼装 + 路径校验在 `MailPaths` 里）。
    let cache =
        core.paths()
            .attachment_cache_path(target.message_id, attachment_id, &target.filename)?;

    let (bytes, from_cache) = match read_cache(&cache, &target) {
        // 缓存命中（文件在、且与清单字节数一致）：零网络。
        Some(bytes) => (bytes, true),
        None => {
            let bytes = fetch_attachment_bytes(core, &db, &target).await?;
            if bytes.len() > MAX_ATTACHMENT_BYTES {
                return Err(too_large(&target, bytes.len()));
            }
            // 先写应用缓存（0600），再把缓存路径回写 `file_path`。
            write_cache(&cache, &bytes)?;
            let path = cache.to_string_lossy().into_owned();
            db.write_tx(|tx| repo::messages::set_attachment_file_path(tx, attachment_id, &path))?;
            (bytes, false)
        }
    };

    // 拷到用户选的位置（「另存为」的落点）。
    write_out(dest, &bytes)?;
    Ok(AttachmentSaved {
        path: dest.to_path_buf(),
        filename: target.filename,
        bytes: bytes.len(),
        from_cache,
    })
}

fn too_large(target: &repo::messages::AttachmentTarget, bytes: usize) -> MailError {
    MailError::Other(format!(
        "attachment {:?} is {bytes} bytes, over the {MAX_ATTACHMENT_BYTES} byte limit",
        target.filename
    ))
}

/// 读应用缓存；文件不在、或与清单字节数不一致（过期）时返回 `None` → 重新下载。
fn read_cache(
    cache: &std::path::Path,
    target: &repo::messages::AttachmentTarget,
) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(cache).ok()?;
    if !meta.is_file() {
        return None;
    }
    // 清单大小已知时必须一致；未知（0）时接受任意大小。
    if target.size > 0 && meta.len() != target.size as u64 {
        return None;
    }
    std::fs::read(cache).ok()
}

/// 把字节写进应用缓存目录（0600）。
fn write_cache(cache: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MailError::Io(format!("{}: {e}", parent.display())))?;
    }
    std::fs::write(cache, bytes).map_err(|e| MailError::Io(format!("{}: {e}", cache.display())))?;
    restrict_permissions(cache)?;
    Ok(())
}

/// 附件缓存文件只给本用户读写（0600）。非 Unix 平台是空操作。
fn restrict_permissions(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| MailError::Io(format!("{}: {e}", path.display())))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 拷到用户选的位置：先建父目录（`save_file` 允许直接输入还不存在的目录），截断写。
fn write_out(dest: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| MailError::Io(format!("{}: {e}", parent.display())))?;
    }
    std::fs::write(dest, bytes).map_err(|e| MailError::Io(format!("{}: {e}", dest.display())))?;
    Ok(())
}

/// 取回一个附件的字节：优先按 IMAP 节取，拿不到段号 / 解不出编码时降级整封取回。
async fn fetch_attachment_bytes(
    core: &MailCore,
    db: &crate::db::MailDb,
    target: &repo::messages::AttachmentTarget,
) -> Result<Vec<u8>> {
    let folder_name = target
        .folder_name
        .clone()
        .ok_or_else(|| MailError::NotFound(format!("folder of message {}", target.message_id)))?;
    let uid = target
        .uid
        .ok_or_else(|| MailError::NotFound(format!("uid of message {}", target.message_id)))?;

    let (account, password) =
        crate::accounts::load_with_password(db, core.secrets(), target.account_id)?;
    let cfg = ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        security: account.security,
        username: account.username.clone(),
        password,
        auth: account.auth_kind,
    };

    // 首选：只取这一个部件（`BODY.PEEK[<section>]`）。
    if let Some(section) = target.imap_section.as_deref() {
        match fetch_by_section(&cfg, &folder_name, uid, section).await {
            Ok(bytes) => return Ok(bytes),
            // 单次段取回失败（服务端不支持该节的 MIME 查询、连接抖动…）不该让整个
            // 下载失败——降级整封取回，正确性优先。
            Err(e) => tracing::warn!(
                attachment_id = target.attachment_id,
                section,
                error = %e,
                "imap section fetch failed; falling back to fetching the whole message"
            ),
        }
    } else {
        tracing::debug!(
            attachment_id = target.attachment_id,
            "no reliable imap section; fetching the whole message"
        );
    }

    // 降级路径。错误**原样上抛**：连接失败 / 认证失败 / 超时各有自己的 `code()`，
    // 界面的文案就是按 `code()` 选的（同 `fetch_body_inner`）；套一层 `Other` 会把
    // 它们全压成「未知错误」。
    let (_raw, parsed) = fetch_body_remote(&cfg, &folder_name, uid).await?;
    pick_attachment_bytes(&parsed, target)
}

/// 按 IMAP 节取回并解码一个部件。
async fn fetch_by_section(
    cfg: &ImapConfig,
    folder_name: &str,
    uid: i64,
    section: &str,
) -> Result<Vec<u8>> {
    let uid = u32::try_from(uid)
        .map_err(|_| MailError::Protocol(format!("uid {uid} is out of range")))?;

    let mut session = ImapSession::connect(cfg).await?;
    session.select(folder_name).await?;
    let fetched = session.fetch_section(uid, section).await;
    session.logout().await;
    let fetched = fetched?;

    crate::mime::decode_attachment_part(&fetched.mime_headers, &fetched.body).ok_or_else(|| {
        MailError::Mime(format!(
            "cannot decode imap section {section} of uid {uid}; the server returned no usable MIME headers"
        ))
    })
}

/// 从整封解析结果里挑出目标附件（降级路径用）。
///
/// 用 `ordinal` 索引，但**必须**用 `filename` + `size` 复核（见 [`matches_target`]）；
/// 序号对不上就按文件名 + 字节数再找一遍，找不到报错。
fn pick_attachment_bytes(
    parsed: &crate::mime::ParsedMessage,
    target: &repo::messages::AttachmentTarget,
) -> Result<Vec<u8>> {
    let ordinal = usize::try_from(target.ordinal).unwrap_or(usize::MAX);
    let pick = parsed
        .attachments
        .get(ordinal)
        .filter(|part| matches_target(part, target))
        .or_else(|| {
            parsed
                .attachments
                .iter()
                .find(|part| matches_target(part, target))
        })
        .ok_or_else(|| {
            // ⚠️ 内核错误详情**一律英文**（`error.rs` 模块头）：`Display` 已经加了
            // 「mime parse error: 」这样的英文前缀，界面再用 `tr` 包一层中文外壳。
            // 这里写中文会被 `duck-theme` 的 `every_chinese_literal_is_translated`
            // 直接拦下（内核里没有 EN 表可登记）——那是**对的**，别把它加进
            // `NON_UI_SOURCES` 绕过。
            MailError::Mime(format!(
                "attachment {:?} on the server no longer matches the local list; \
                 refetch the body of this message",
                target.filename
            ))
        })?;
    Ok(pick.bytes.clone())
}

/// 解析出来的这一份，是不是库里那一条。
///
/// 文件名必须一致；字节数在**库里已知**（> 0）时也必须一致。库里是 0 时只看文件名
/// ——那是「大小未知」，不是「大小为 0」。
fn matches_target(
    part: &crate::mime::ParsedAttachment,
    target: &repo::messages::AttachmentTarget,
) -> bool {
    part.filename == target.filename && (target.size <= 0 || part.size == target.size)
}

/// 连上服务器把整封原始报文取回来并解码。
async fn fetch_body_remote(
    cfg: &ImapConfig,
    folder_name: &str,
    uid: i64,
) -> Result<(Vec<u8>, crate::mime::ParsedMessage)> {
    let mut session = ImapSession::connect(cfg).await?;
    // `UID FETCH` 只在**已选中**的邮箱里才有意义，所以必须先 SELECT。
    let out = match session.select(folder_name).await {
        Ok(_) => fetch_from_session(&mut session, uid).await,
        Err(e) => Err(e),
    };
    let _ = session.logout().await;
    out
}

/// 在**已选中**的会话上取一整封并解码。前台路径（临时会话）与预取路径
/// （复用会话）共用这最后一步，保证取回的东西完全一致。
async fn fetch_from_session(
    session: &mut ImapSession,
    uid: i64,
) -> Result<(Vec<u8>, crate::mime::ParsedMessage)> {
    let uid = u32::try_from(uid)
        .map_err(|_| MailError::Protocol(format!("uid {uid} is out of range")))?;
    let raw = session.fetch_raw(uid).await?;
    let parsed = crate::mime::parse(&raw)?;
    Ok((raw, parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ConnectionSecurity, NewAccount};

    /// 一份临时库 + 内存凭据后端，外加一个已入库的账户。
    ///
    /// 账户主机用 `.invalid`（RFC 2606 保留域名，DNS 必然解析失败），所以这个夹具
    /// **不会真的连出去**。
    fn core_with_account(tag: &str) -> (MailCore, i64, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("duck-mail-sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = MailCore::open_with_secrets(
            crate::MailPaths::with_root(&dir),
            crate::secrets::memory(),
        )
        .unwrap();
        let id = core
            .add_account(&NewAccount {
                email: format!("{tag}@example.com"),
                display_name: None,
                username: format!("{tag}@example.com"),
                imap_host: "imap.invalid".into(),
                imap_port: 993,
                smtp_host: "smtp.invalid".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "p".into(),
            })
            .unwrap();
        (core, id, dir)
    }

    #[test]
    fn sync_marks_account_error_when_server_is_unreachable() {
        let (core, id, dir) = core_with_account("unreachable");
        // `.invalid` 是 RFC 2606 保留域名，DNS 必然解析失败。
        let result = sync_account(&core, id);
        assert!(result.is_err(), "sync against an invalid host must fail");
        // 失败要**留在账户状态里**，不能只写日志。
        let account = core.accounts().unwrap().into_iter().next().unwrap();
        assert_eq!(account.sync_state, SyncState::Error);
        assert!(account.sync_error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_of_missing_account_is_not_found() {
        let (core, _, dir) = core_with_account("missing");
        assert!(matches!(
            sync_account(&core, 9999),
            Err(MailError::NotFound(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_body_of_missing_message_is_not_found() {
        let (core, _, dir) = core_with_account("bodymissing");
        assert!(matches!(
            fetch_body(&core, 4242),
            Err(MailError::NotFound(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_body_short_circuits_when_the_body_is_already_cached() {
        let (core, id, dir) = core_with_account("bodycached");
        // 造一封「正文已缓存」的邮件：不连服务器就该拿到内容。
        let message_id = seed_cached_message(&core, id);
        let detail = fetch_body(&core, message_id).unwrap();
        assert_eq!(detail.body_state, "cached");
        assert_eq!(detail.text_body.as_deref(), Some("已经在本地的正文"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_body_failure_returns_the_state_to_none() {
        let (core, id, dir) = core_with_account("bodyfail");
        // 只有头部、没有正文的邮件，且账户指向 `.invalid` → 连不上。
        let message_id = seed_header_only_message(&core, id);
        assert!(fetch_body(&core, message_id).is_err());
        // 关键：失败后不能停在 `fetching`，否则界面永远显示「正在下载正文」。
        let detail = core
            .db()
            .read(|conn| repo::messages::get_detail(conn, message_id))
            .unwrap();
        assert_eq!(detail.body_state, "none");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_body_while_already_fetching_skips_the_network() {
        let (core, id, dir) = core_with_account("bodyinflight");
        let message_id = seed_header_only_message(&core, id);
        // 模拟「另一条路径正在拉这一封」（后台预取撞上前台点开）：置成 `fetching`。
        core.db()
            .write_tx(|tx| repo::messages::claim_body_fetch(tx, message_id))
            .unwrap();
        // 账户指向 `.invalid`——**只要真的连网就必然失败**。这里返回 `Ok` 就证明
        // 原子抢占把这次调用挡在了网络请求之前。
        let detail = fetch_body(&core, message_id).unwrap();
        assert_eq!(detail.body_state, "fetching");
        // 也不能动那行状态：原来的拉取还在进行，它要负责把它推到 `cached` / `none`。
        let state = core
            .db()
            .read(|conn| repo::messages::get_detail(conn, message_id))
            .unwrap()
            .body_state;
        assert_eq!(state, "fetching");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_resets_orphaned_body_fetches() {
        let (core, id, dir) = core_with_account("bodystale");
        let message_id = seed_header_only_message(&core, id);
        // 模拟「上次运行在拉正文途中被杀掉」：留下一行永远的 `fetching`。
        core.db()
            .write_tx(|tx| repo::messages::claim_body_fetch(tx, message_id))
            .unwrap();
        drop(core);
        // 重新打开同一个落点 = 又一次启动。
        let core = MailCore::open_with_secrets(
            crate::MailPaths::with_root(&dir),
            crate::secrets::memory(),
        )
        .unwrap();
        let state = core
            .db()
            .read(|conn| repo::messages::get_detail(conn, message_id))
            .unwrap()
            .body_state;
        // 不清理的话这封信会被抢占闸门永久锁死（闸门只认 `none`），正文再也拉不到。
        assert_eq!(state, "none", "orphaned fetching must be reset on open");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 造一封「正文已缓存」的邮件，返回它的主键。
    fn seed_cached_message(core: &MailCore, account_id: i64) -> i64 {
        let folder_id = seed_folder(core, account_id);
        let db = core.db();
        let id = db
            .write_tx(|tx| {
                repo::messages::upsert_envelope(
                    tx,
                    account_id,
                    folder_id,
                    &repo::messages::EnvelopeRow {
                        uid: 1,
                        message_id: Some("cached@example.com".into()),
                        subject: "cached".into(),
                        from_name: None,
                        from_addr: Some("a@example.com".into()),
                        to: Vec::new(),
                        cc: Vec::new(),
                        date: 1_791_770_400,
                        is_read: true,
                        is_starred: false,
                        has_attachments: false,
                        size: None,
                        snippet: String::new(),
                    },
                )
            })
            .unwrap();
        db.write_tx(|tx| repo::messages::save_body(tx, id, Some("已经在本地的正文"), None))
            .unwrap();
        id
    }

    /// 造一封只有信封、没有正文的邮件。
    fn seed_header_only_message(core: &MailCore, account_id: i64) -> i64 {
        let folder_id = seed_folder(core, account_id);
        core.db()
            .write_tx(|tx| {
                repo::messages::upsert_envelope(
                    tx,
                    account_id,
                    folder_id,
                    &repo::messages::EnvelopeRow {
                        uid: 2,
                        message_id: Some("header-only@example.com".into()),
                        subject: "header only".into(),
                        from_name: None,
                        from_addr: Some("b@example.com".into()),
                        to: Vec::new(),
                        cc: Vec::new(),
                        date: 1_791_770_400,
                        is_read: false,
                        is_starred: false,
                        has_attachments: false,
                        size: None,
                        snippet: String::new(),
                    },
                )
            })
            .unwrap()
    }

    fn seed_folder(core: &MailCore, account_id: i64) -> i64 {
        core.db()
            .write_tx(|tx| {
                repo::folders::upsert(
                    tx,
                    account_id,
                    &repo::folders::FolderUpsert {
                        name: "INBOX".into(),
                        delimiter: Some("/".into()),
                        role: FolderRole::Inbox,
                        uidvalidity: Some(1),
                        uidnext: Some(3),
                    },
                )
            })
            .unwrap()
    }

    #[test]
    fn flag_updates_only_reports_real_differences() {
        let local = vec![(1, false, false), (2, true, false), (3, false, true)];
        let remote = vec![
            // 与本地一致：不产生更新。
            FetchedFlags {
                uid: 1,
                seen: false,
                flagged: false,
            },
            // 服务端标了已读：要写回。
            FetchedFlags {
                uid: 2,
                seen: true,
                flagged: true,
            },
            // 窗口边界外多回来的 UID 不参与。
            FetchedFlags {
                uid: 99,
                seen: true,
                flagged: true,
            },
        ];
        assert_eq!(flag_updates(&local, &remote), vec![(2, true, true)]);
        // 服务端一条都没回：不动本地（删除对账才是删的地方）。
        assert!(flag_updates(&local, &[]).is_empty());
    }

    #[test]
    fn deleted_uids_is_the_set_difference() {
        let local = vec![1, 2, 5, 9];
        let remote = vec![1u32, 2, 9];
        assert_eq!(deleted_uids(&local, &remote), vec![5]);
        // 服务端全都在：没有可删的。
        assert!(deleted_uids(&local, &[1, 2, 5, 9]).is_empty());
        // 本地空集合不会产生删除。
        assert!(deleted_uids(&[], &[]).is_empty());
    }

    #[test]
    fn clear_folder_wipes_messages_and_index() {
        let (core, id, dir) = core_with_account("clear");
        let db = core.db();
        let folder = db
            .write_tx(|tx| {
                repo::folders::upsert(
                    tx,
                    id,
                    &repo::folders::FolderUpsert {
                        name: "INBOX".into(),
                        delimiter: Some("/".into()),
                        role: FolderRole::Inbox,
                        uidvalidity: Some(1),
                        uidnext: Some(10),
                    },
                )
            })
            .unwrap();
        let msg = db
            .write_tx(|tx| {
                repo::messages::upsert_envelope(
                    tx,
                    id,
                    folder,
                    &repo::messages::EnvelopeRow {
                        uid: 1,
                        subject: "x".into(),
                        date: 1,
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        db.write_tx(|tx| repo::search::index_message(tx, msg, "x", "", "", "x"))
            .unwrap();
        // 落库后：1 封邮件、索引里能搜到。
        assert_eq!(
            db.read(|c| repo::messages::count_in_folder(c, folder))
                .unwrap(),
            1
        );
        assert_eq!(
            db.read(|c| repo::search::search(c, "x", 10)).unwrap().len(),
            1
        );

        db.write_tx(|tx| clear_folder(tx, folder)).unwrap();
        assert_eq!(
            db.read(|c| repo::messages::count_in_folder(c, folder))
                .unwrap(),
            0
        );
        assert!(
            db.read(|c| repo::search::search(c, "x", 10))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 附件下载 ----

    /// 夹具：账户 + `INBOX` + 一封带一个附件的邮件（`报告.bin`，5 字节）。
    ///
    /// 返回 `(core, 附件 id, 应用缓存落点, 临时根目录)`。库里的 `file_path` **不预置**
    /// ——新流程里它由下载动作回写成缓存路径（见 [`download_attachment`]）；
    /// 「缓存命中」判的是**缓存文件本身在不在**，不是库里的字符串。夹具**不创建**
    /// 缓存文件——是否预置由各用例自己决定。
    fn core_with_attachment(tag: &str) -> (MailCore, i64, std::path::PathBuf, std::path::PathBuf) {
        let (core, account, dir) = core_with_account(tag);
        let folder = core
            .db()
            .write(|conn| {
                repo::folders::upsert(
                    conn,
                    account,
                    &repo::folders::FolderUpsert {
                        name: "INBOX".into(),
                        delimiter: Some("/".into()),
                        role: FolderRole::Inbox,
                        uidvalidity: Some(1),
                        uidnext: Some(10),
                    },
                )
            })
            .unwrap();
        let message = core
            .db()
            .write(|conn| {
                repo::messages::upsert_envelope(
                    conn,
                    account,
                    folder,
                    &repo::messages::EnvelopeRow {
                        uid: 7,
                        message_id: Some("<7@example.com>".into()),
                        subject: "带附件".into(),
                        from_name: None,
                        from_addr: Some("x@example.com".into()),
                        to: vec!["me@example.com".into()],
                        cc: vec![],
                        date: 1_000,
                        is_read: false,
                        is_starred: false,
                        has_attachments: true,
                        size: Some(9),
                        snippet: String::new(),
                    },
                )
            })
            .unwrap();
        let attachment = core
            .db()
            .write(|conn| {
                repo::messages::replace_attachments(
                    conn,
                    message,
                    &[repo::messages::NewAttachment {
                        filename: "报告.bin".into(),
                        mime_type: "application/octet-stream".into(),
                        size: 5,
                        content_id: None,
                        is_inline: false,
                        file_path: None,
                        imap_section: None,
                    }],
                )?;
                Ok(repo::messages::attachments_for(conn, message)?[0].id)
            })
            .unwrap();
        let cache = core
            .paths()
            .attachment_cache_path(message, attachment, "报告.bin")
            .unwrap();
        (core, attachment, cache, dir)
    }

    #[test]
    fn matches_target_requires_the_filename_and_a_known_size() {
        let part = |filename: &str, size: usize| crate::mime::ParsedAttachment {
            filename: filename.into(),
            mime_type: "application/octet-stream".into(),
            size: size as i64,
            content_id: None,
            is_inline: false,
            bytes: vec![0; size],
            imap_section: None,
        };
        let target = |filename: &str, size: i64| repo::messages::AttachmentTarget {
            attachment_id: 1,
            message_id: 1,
            account_id: 1,
            filename: filename.into(),
            mime_type: "application/octet-stream".into(),
            size,
            file_path: None,
            ordinal: 0,
            imap_section: None,
            folder_name: Some("INBOX".into()),
            uid: Some(7),
        };

        assert!(matches_target(&part("a.bin", 10), &target("a.bin", 10)));
        // 文件名不同 = 不是同一条（哪怕字节数一样）——这正是「不能只按序号取」的理由。
        assert!(!matches_target(&part("b.bin", 10), &target("a.bin", 10)));
        // 库里大小已知时必须一致。
        assert!(!matches_target(&part("a.bin", 11), &target("a.bin", 10)));
        // 库里大小未知（0）时只看文件名——「未知」不是「必须是 0 字节」。
        assert!(matches_target(&part("a.bin", 999), &target("a.bin", 0)));
    }

    #[test]
    fn missing_attachment_is_not_found_and_touches_no_network() {
        let (core, _, dir) = core_with_account("attachment-missing");
        let dest = dir.join("x.bin");
        assert!(matches!(
            download_attachment(&core, 9999, &dest),
            Err(MailError::NotFound(_))
        ));
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 应用缓存已命中：把缓存文件写好，再一次「另存为」应当是**纯本地拷贝，不连网**。
    #[test]
    fn a_previously_saved_attachment_comes_back_from_disk() {
        let (core, attachment, cache, dir) = core_with_attachment("attachment-cache");
        // 模拟「上次已经下到应用缓存」：把 5 字节（与清单一致）写进缓存落点。
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, b"hello").unwrap();

        let dest = dir.join("out.bin");
        let saved = download_attachment(&core, attachment, &dest).unwrap();
        assert!(saved.from_cache, "缓存命中时不该连网");
        assert_eq!(saved.filename, "报告.bin");
        assert_eq!(saved.bytes, 5);
        // 缓存被拷到了用户选的位置。
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存没命中（文件不在）→ 必须**真的连服务器重新下载**，而不是回一个「成功」
    /// 让界面去打开一个不存在的文件。
    #[test]
    fn a_cache_miss_falls_back_to_the_network() {
        let (core, attachment, cache, dir) = core_with_attachment("attachment-gone");
        assert!(!cache.exists(), "夹具不该预置缓存文件");
        let dest = dir.join("out.bin");
        let error = download_attachment(&core, attachment, &dest).unwrap_err();
        assert!(
            matches!(error, MailError::Connect(_) | MailError::Timeout(_)),
            "缓存没命中就该真的去下载（这里是连不上），实际 {error:?}"
        );
        // 失败不能留下半个文件——用户会以为已经存下来了。
        assert!(!dest.exists(), "下载失败不该落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没有本地副本时要真的连服务器；连不上时**错误必须原样上抛**。
    ///
    /// 「原样」是这条断言的要点：套一层 `MailError::Other` 会把 `code()` 压成
    /// `other`，界面再也选不出「连不上服务器」的文案。
    #[test]
    fn a_failed_download_surfaces_the_connection_error_unwrapped() {
        let (core, attachment, _cache, dir) = core_with_attachment("attachment-offline");
        let dest = dir.join("out.bin");
        let error = download_attachment(&core, attachment, &dest).unwrap_err();
        assert!(
            matches!(error, MailError::Connect(_) | MailError::Timeout(_)),
            "期望连接类错误（`imap.invalid` 解析不了），实际 {error:?}"
        );
        // 失败不能留下半个文件——用户会以为已经存下来了。
        assert!(!dest.exists(), "下载失败不该落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
