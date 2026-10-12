//! 同步引擎（M1 版）。
//!
//! # 形态
//!
//! M1 是**「一次性全量 + 增量」的同步函数**，不常驻：调用 [`sync_account`] 跑一轮，
//! 连上、`LIST`、按 UID 水位拉新信封、落库、断连。
//!
//! M2 才上 per-account 常驻 actor（`IDLE` 长连接 + 命令通道 + 断线退避），届时本模块
//! 会变成 actor 的主循环体。**现在不预先搭 actor 骨架**：没有 IDLE 与动作队列的
//! actor 只是一个把简单流程包起来的空壳，反而让 M2 改起来要拆两层。
//!
//! # 增量算法（与服务器实现无关，在每台服务器上都正确）
//!
//! 1. `SELECT` 读 `UIDVALIDITY` / `UIDNEXT`；
//! 2. `UIDVALIDITY` 与本地记录不一致 → **该文件夹本地清空重建**
//!    （服务端重建过文件夹，旧 UID 全部失效）；
//! 3. 拉 `last_seen_uid + 1 : *` 的信封（只拉头部）；
//! 4. 水位推进到本批最大 UID。
//!
//! 旗标对账与删除对账见 [`reconcile_folder`]（M1 已实现，供手动「重新同步」用）。

use rusqlite::Connection;

use crate::MailCore;
use crate::db::repo;
use crate::error::{MailError, Result};
use crate::models::{FolderRole, MessageDetail, SyncState};
use crate::proto::ImapConfig;
use crate::proto::imap::{ImapSession, ListedFolder};

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

    Ok((new_count, 0))
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

// 旗标与删除对账放在 M2：它依赖 `UID SEARCH`（大文件夹上是重操作），而 M1 的
// 验收只需要「不重复拉取」，UID 水位已经满足。**不在这里留半成品签名**——
// 一个返回 `Unsupported` 的公开函数只会让调用方以为它可用。

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
    // （`body_state` 是 `fetching` 时也不能短路——那说明另一次拉取正在进行，
    //  这里重新拉一遍是无害的，卡在 fetching 才是坏结果。）
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

    db.write_tx(|tx| repo::messages::set_body_state(tx, message_id, "fetching"))?;

    let cfg = ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        security: account.security,
        username: account.username.clone(),
        password,
    };

    let result = fetch_body_remote(&cfg, &folder.name, uid).await;
    let (raw, parsed) = match result {
        Ok(v) => v,
        Err(e) => {
            // 失败要把状态退回去，否则下一帧界面还显示「正在下载正文」，而其实
            // 已经结束了——一个永远转不完的加载态。
            let _ = db.write_tx(|tx| repo::messages::set_body_state(tx, message_id, "none"));
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
            // 段号要靠 `BODYSTRUCTURE` 解析，M1 没做；按需下载附件是 M2。
            imap_section: None,
        })
        .collect();

    db.write_tx(|tx| {
        repo::messages::save_body(
            tx,
            message_id,
            parsed.text_body.as_deref(),
            parsed.html_body.as_deref(),
        )?;
        repo::messages::replace_attachments(tx, message_id, &attachment_rows)?;
        repo::messages::set_has_attachments(tx, message_id, !attachment_rows.is_empty())?;
        Ok(())
    })?;

    let detail = db.read(|conn| repo::messages::get_detail(conn, message_id))?;
    core.emit(crate::MailEvent::BodyReady { message_id });
    Ok(detail)
}

/// 连上服务器把整封原始报文取回来并解码。
async fn fetch_body_remote(
    cfg: &ImapConfig,
    folder_name: &str,
    uid: i64,
) -> Result<(Vec<u8>, crate::mime::ParsedMessage)> {
    let uid = u32::try_from(uid)
        .map_err(|_| MailError::Protocol(format!("uid {uid} is out of range")))?;

    let mut session = ImapSession::connect(cfg).await?;
    // `UID FETCH` 只在**已选中**的邮箱里才有意义，所以必须先 SELECT。
    session.select(folder_name).await?;
    let raw = session.fetch_raw(uid).await;
    session.logout().await;

    let raw = raw?;
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
}
