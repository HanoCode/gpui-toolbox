//! `drafts_meta` / `draft_attachments` 的读写（**本地草稿**）。
//!
//! 本地草稿落**四张既有表**——不建新表、不加迁移（表早在 `migrations/001_init.sql`）：
//!
//! | 表 | 存什么 |
//! | --- | --- |
//! | `messages` | 一行草稿：`is_draft = 1`，`folder_id` 与 `uid` **留 NULL**；`subject` / `from_name` / `from_addr` / `to_json` / `cc_json` / `bcc_json` / `date` / `snippet` 都在这里 |
//! | `message_bodies.text_body` | 纯文本正文（与收信同一个落点） |
//! | `drafts_meta` | `mode` / `in_reply_to_message_id` / `remote_uid`（后者本批不写） |
//! | `draft_attachments` | 附件**路径**（`file_path` / `filename` / `mime_type`） |
//!
//! # 两条硬口径
//!
//! 1. **草稿的 `folder_id` 必须是 NULL。** 草稿不属于任何 IMAP 文件夹，于是
//!    [`super::messages::list_by_folder`] 的 `WHERE folder_id = ?1` 天然带不出它，
//!    同步侧「UIDVALIDITY 变了就清空该文件夹」（`sync.rs::clear_folder`）也不会
//!    误删草稿。⚠️ 代价是**检索必须显式排除草稿**——`search` 里那条
//!    `m.is_draft = 0` 就是干这个的：`folder_id` 为 NULL 的行走到
//!    `messages::row_to_summary` 会直接报 `InvalidColumnType`。
//! 2. **附件只存路径，不存字节。** 表结构给的就是 `file_path`（没有字节列）。所以
//!    草稿里的附件与界面上「已选附件」不是一回事：后者是「已读进内存的字节」，
//!    前者是「用户当初选的那个文件」。打开草稿要**重新读盘**——文件被挪走/删掉时，
//!    内核照旧把路径还给你，读不读得出来、要不要提示，是调用方的事。
//!
//! # 已知取舍
//!
//! `References:` 链落 `message_refs`（`WITHOUT ROWID` + 联合主键 `(message_id,
//! ref_message_id)`）→ **读回来是按引用串排序的，不是原始顺序**。收信路径本来就在
//! 同一张表上受这个限制，这里保持一致；真要在意顺序得加一列 `position`（新迁移），
//! 本批不做。
//!
//! # 错误详情一律英文
//!
//! `duck-theme` 的 `every_chinese_literal_is_translated` 会扫到本文件，而本 crate
//! **没有 EN 表可登记**，中文详情会直接把守卫打红——见 `error.rs` 模块头。

use rusqlite::{Connection, OptionalExtension, params};

use crate::compose::{self, Draft, Mailbox};
use crate::error::{MailError, Result};
use crate::models::{DraftMode, DraftSummary};

/// 草稿附件的一行（**只记路径**，不记字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftAttachmentRef {
    /// 用户当初在「另存为/选文件」里挑的那个文件的绝对路径。
    pub file_path: String,
    /// 收件人看到的文件名（可能与 `file_path` 的末段不同：用户可以只改显示名）。
    pub filename: String,
    /// 落库时声明的类型；`None` = 重开草稿时按扩展名再猜一次。
    pub mime_type: Option<String>,
}

impl DraftAttachmentRef {
    /// 直接构造。
    pub fn new(
        file_path: impl Into<String>,
        filename: impl Into<String>,
        mime_type: Option<String>,
    ) -> Self {
        Self {
            file_path: file_path.into(),
            filename: filename.into(),
            mime_type,
        }
    }
}

/// 一条草稿——**存与取共用同一个载荷**，这样往返测试可以整体 `assert_eq!`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRecord {
    /// `messages.id`。`None` = 还没存过（[`save`] 会新建）。
    pub id: Option<i64>,
    /// 所属账户。
    pub account_id: i64,
    /// 头部字段与正文。
    ///
    /// ⚠️ `date_unix` **不落库**（落库的是「最后保存时间」，见 [`save`]），
    /// [`load`] 回填的一律是 `0`——发信那一刻由 [`Draft::stamp`] 补上。
    pub draft: Draft,
    /// 草稿的由来（新写 / 回复 / 转发）。
    pub mode: DraftMode,
    /// 附件路径引用。
    pub attachments: Vec<DraftAttachmentRef>,
}

/// 新建或改存一条草稿，返回 `messages.id`。
///
/// - `record.id == None` → 插一行；`Some(id)` → 改存那一行（**必须**是同一账户的草稿，
///   否则 `NotFound` ——一个错传的 id 绝不能把别人的邮件改坏）。
/// - `messages.date` 存的是**最后保存时间** `now_unix`（草稿列表按它倒序），
///   与 `draft.date_unix` 无关。
/// - `Message-ID` 一旦定下就**不再变**：本层只在「新草稿且调用方没给」时才生成一个，
///   改存时沿用它。否则每次存草稿都换一个 id，别人引用这一封的 `In-Reply-To` 会悬空。
/// - 附件与引用链一律**整体替换**（先删后写）：只做增量的话，「移除一个附件」留不下痕迹。
pub fn save(conn: &Connection, record: &DraftRecord, now_unix: i64) -> Result<i64> {
    let draft = &record.draft;
    let to_json = addresses_json(&draft.to);
    let cc_json = addresses_json(&draft.cc);
    let bcc_json = addresses_json(&draft.bcc);
    let snippet = snippet_of(&draft.body);
    let has_attachments = i64::from(!record.attachments.is_empty());
    let from_name = draft
        .from
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let from_addr = draft.from.address.trim();

    let (id, existing_message_id) = match record.id {
        None => (None, None),
        Some(id) => {
            let owned: Option<Option<String>> = conn
                .query_row(
                    "SELECT message_id FROM messages
                     WHERE id = ?1 AND account_id = ?2 AND is_draft = 1",
                    params![id, record.account_id],
                    |row| row.get(0),
                )
                .optional()?;
            let existing = owned.ok_or_else(|| MailError::NotFound(format!("draft {id}")))?;
            (Some(id), existing)
        }
    };

    let message_id = if !draft.message_id.trim().is_empty() {
        draft.message_id.trim().to_owned()
    } else {
        existing_message_id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| compose::generate_message_id(from_addr, now_unix))
    };

    let id = match id {
        Some(id) => {
            conn.execute(
                "UPDATE messages SET
                     message_id      = ?2,
                     subject         = ?3,
                     from_name       = ?4,
                     from_addr       = ?5,
                     to_json         = ?6,
                     cc_json         = ?7,
                     bcc_json        = ?8,
                     date            = ?9,
                     has_attachments = ?10,
                     snippet         = ?11
                 WHERE id = ?1",
                params![
                    id,
                    message_id,
                    draft.subject,
                    from_name,
                    from_addr,
                    to_json,
                    cc_json,
                    bcc_json,
                    now_unix,
                    has_attachments,
                    snippet,
                ],
            )?;
            id
        }
        None => {
            conn.execute(
                "INSERT INTO messages
                     (account_id, folder_id, uid, message_id, subject, from_name, from_addr,
                      to_json, cc_json, bcc_json, date, is_draft, has_attachments, snippet)
                 VALUES (?1, NULL, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?11)",
                params![
                    record.account_id,
                    message_id,
                    draft.subject,
                    from_name,
                    from_addr,
                    to_json,
                    cc_json,
                    bcc_json,
                    now_unix,
                    has_attachments,
                    snippet,
                ],
            )?;
            conn.last_insert_rowid()
        }
    };

    conn.execute(
        "INSERT INTO message_bodies (message_id, text_body) VALUES (?1, ?2)
         ON CONFLICT(message_id) DO UPDATE SET text_body = excluded.text_body",
        params![id, draft.body],
    )?;

    let in_reply_to_id = reply_target_id(conn, &record.draft)?;
    conn.execute(
        "INSERT INTO drafts_meta (message_id, mode, in_reply_to_message_id, remote_uid)
         VALUES (?1, ?2, ?3, NULL)
         ON CONFLICT(message_id) DO UPDATE SET
             mode                   = excluded.mode,
             in_reply_to_message_id = excluded.in_reply_to_message_id",
        params![id, record.mode.as_str(), in_reply_to_id],
    )?;

    conn.execute("DELETE FROM draft_attachments WHERE draft_id = ?1", [id])?;
    for attachment in &record.attachments {
        conn.execute(
            "INSERT INTO draft_attachments (draft_id, file_path, filename, mime_type)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                id,
                attachment.file_path,
                attachment.filename,
                attachment.mime_type,
            ],
        )?;
    }

    conn.execute("DELETE FROM message_refs WHERE message_id = ?1", [id])?;
    for reference in &draft.references {
        if reference.trim().is_empty() {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO message_refs (message_id, ref_message_id) VALUES (?1, ?2)",
            params![id, reference.trim()],
        )?;
    }

    Ok(id)
}

/// 草稿箱列表，**按最后保存时间倒序**。
pub fn list(conn: &Connection, account_id: i64) -> Result<Vec<DraftSummary>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.account_id, m.subject, m.to_json, m.date, d.mode,
                (SELECT COUNT(*) FROM draft_attachments a WHERE a.draft_id = m.id)
         FROM messages m
         JOIN drafts_meta d ON d.message_id = m.id
         WHERE m.account_id = ?1 AND m.is_draft = 1
         ORDER BY m.date DESC, m.id DESC",
    )?;
    let rows = stmt.query_map([account_id], |row| {
        let to_json: String = row.get(3)?;
        let mode: String = row.get(5)?;
        Ok(DraftSummary {
            id: row.get(0)?,
            account_id: row.get(1)?,
            subject: row.get(2)?,
            recipients: parse_address_list(&to_json),
            date: row.get(4)?,
            mode: DraftMode::from_db(&mode),
            attachment_count: row.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 草稿条数（给侧栏「草稿」角标用；`folder_id` 为 NULL 的行不在 `folders` 的计数里）。
pub fn count(conn: &Connection, account_id: i64) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages m
         JOIN drafts_meta d ON d.message_id = m.id
         WHERE m.account_id = ?1 AND m.is_draft = 1",
        [account_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// 读回一条草稿（含附件路径引用）。
///
/// `from` 优先取落库时记下的 `from_name` / `from_addr`；老数据或空值时回落到账户档案
/// 的邮箱 + 显示名。
pub fn load(conn: &Connection, account_id: i64, draft_id: i64) -> Result<DraftRecord> {
    let row = conn
        .query_row(
            "SELECT m.subject, m.from_name, m.from_addr, m.to_json, m.cc_json, m.bcc_json,
                    m.message_id, d.mode, d.in_reply_to_message_id, COALESCE(b.text_body, '')
             FROM messages m
             JOIN drafts_meta d ON d.message_id = m.id
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE m.id = ?1 AND m.account_id = ?2 AND m.is_draft = 1",
            params![draft_id, account_id],
            |row| {
                Ok(StoredDraft {
                    subject: row.get(0)?,
                    from_name: row.get(1)?,
                    from_addr: row.get(2)?,
                    to_json: row.get(3)?,
                    cc_json: row.get(4)?,
                    bcc_json: row.get(5)?,
                    message_id: row.get(6)?,
                    mode: row.get(7)?,
                    in_reply_to_id: row.get(8)?,
                    body: row.get(9)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| MailError::NotFound(format!("draft {draft_id}")))?;

    let from = match row.from_addr.as_deref().map(str::trim) {
        Some(address) if !address.is_empty() => Mailbox {
            name: row
                .from_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            address: address.to_owned(),
        },
        _ => {
            let account = super::accounts::get(conn, account_id)?;
            Mailbox {
                name: account
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
                address: account.email,
            }
        }
    };

    let mut draft = Draft::new(from, Mailbox::new(""), row.subject, row.body);
    draft.to = parse_mailboxes(&row.to_json);
    draft.cc = parse_mailboxes(&row.cc_json);
    draft.bcc = parse_mailboxes(&row.bcc_json);
    draft.message_id = row.message_id.unwrap_or_default();
    draft.in_reply_to = match row.in_reply_to_id {
        Some(parent) => message_id_of(conn, parent)?,
        None => None,
    };
    draft.references = references_of(conn, draft_id)?;

    Ok(DraftRecord {
        id: Some(draft_id),
        account_id,
        draft,
        mode: DraftMode::from_db(&row.mode),
        attachments: attachments_of(conn, draft_id)?,
    })
}

/// 删一条草稿。返回「真的删掉了」——`false` 表示这行不存在、或不属于这个账户的草稿。
///
/// 子表（`message_bodies` / `drafts_meta` / `draft_attachments` / `message_refs`）都是
/// `ON DELETE CASCADE`，一条 `DELETE` 就够。
pub fn delete(conn: &Connection, account_id: i64, draft_id: i64) -> Result<bool> {
    let removed = conn.execute(
        "DELETE FROM messages WHERE id = ?1 AND account_id = ?2 AND is_draft = 1",
        params![draft_id, account_id],
    )?;
    Ok(removed > 0)
}

// ---------------------------------------------------------------------------
// 内部
// ---------------------------------------------------------------------------

/// `load` 的一行原始值——单独拎出来是为了让 `query_row` 的闭包短到能读。
struct StoredDraft {
    subject: String,
    from_name: Option<String>,
    from_addr: Option<String>,
    to_json: String,
    cc_json: String,
    bcc_json: String,
    message_id: Option<String>,
    mode: String,
    in_reply_to_id: Option<i64>,
    body: String,
}

/// 回复目标：把 `draft.in_reply_to`（`Message-ID` 裸值）翻回本地行号。
///
/// 翻不回来就存 NULL——草稿本身照样能存，只是少一条「回的是哪一封」的线索。
fn reply_target_id(conn: &Connection, draft: &Draft) -> Result<Option<i64>> {
    let Some(target) = draft.in_reply_to.as_deref().map(str::trim) else {
        return Ok(None);
    };
    if target.is_empty() {
        return Ok(None);
    }
    let bare = target.trim_start_matches('<').trim_end_matches('>');
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM messages WHERE message_id = ?1 ORDER BY date DESC LIMIT 1",
            [bare],
            |row| row.get(0),
        )
        .optional()?;
    Ok(id)
}

/// 某一行的 `Message-ID`（给 `In-Reply-To:` 用）。
fn message_id_of(conn: &Connection, message_id: i64) -> Result<Option<String>> {
    let value: Option<Option<String>> = conn
        .query_row(
            "SELECT message_id FROM messages WHERE id = ?1",
            [message_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value
        .flatten()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty()))
}

fn references_of(conn: &Connection, draft_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT ref_message_id FROM message_refs WHERE message_id = ?1 ORDER BY ref_message_id",
    )?;
    let rows = stmt.query_map([draft_id], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn attachments_of(conn: &Connection, draft_id: i64) -> Result<Vec<DraftAttachmentRef>> {
    let mut stmt = conn.prepare(
        "SELECT file_path, filename, mime_type FROM draft_attachments
         WHERE draft_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([draft_id], |row| {
        Ok(DraftAttachmentRef {
            file_path: row.get(0)?,
            filename: row.get(1)?,
            mime_type: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 地址列表 → `messages.to_json` 那套 JSON 数组（元素是**展示串**）。
///
/// 刻意用展示串而不是结构化对象：`messages.to_json` 这一列收信路径写进去的就是
/// 「名字 <地址>」（来自 IMAP 信封），草稿跟它同构，界面/检索那一侧不必分两套解析。
fn addresses_json(mailboxes: &[Mailbox]) -> String {
    let displays: Vec<String> = mailboxes.iter().map(address_display).collect();
    serde_json::to_string(&displays).unwrap_or_else(|_| "[]".into())
}

/// 单个地址的展示串：`名字 <地址>`；没有名字（或名字全是空白）时只剩地址。
fn address_display(mailbox: &Mailbox) -> String {
    let address = mailbox.address.trim();
    let name = mailbox
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    match name {
        None => address.to_owned(),
        // 名字里出现 `<>`/`,`/`"` 会把 `名字 <地址>` 的结构弄坏（也弄坏 JSON 的可读性），
        // 用双引号包起来——与 RFC 5322 的 quoted-string 同形，读回来去掉引号即可。
        Some(name) => {
            if name.contains(['<', '>', ',', '"']) {
                format!("\"{}\" <{}>", name.replace('"', "'"), address)
            } else {
                format!("{name} <{address}>")
            }
        }
    }
}

/// 展示串 → [`Mailbox`]（[`address_display`] 的逆）。
///
/// 认不出来的一律当「整串就是地址」——宁可发不出去（发信那一刻 `header_value` 会拦），
/// 也不要在读草稿这一步报错把整条草稿卡住。
fn parse_address(display: &str) -> Mailbox {
    let text = display.trim();
    // 这里写成「先算 `open`、再 `if let`」，而不是 let-chain（`if a && let Some(..) = b`）：
    // let-chain 要 Rust 1.88，本仓声明的却是 `rust-version = "1.85"`。⚠️ 但那条 1.85
    // 只对**依赖准入**实检——语言特性没有任何作业在查（`ci.yml` 跑 stable、`static`
    // 作业根本不编译，且 HEAD 里早就有 let-chain）。所以这是本处的**选择**，不是禁令。
    let open = if text.ends_with('>') {
        text.rfind('<')
    } else {
        None
    };
    if let Some(open) = open {
        let address = text[open + 1..text.len() - 1].trim();
        let name = text[..open].trim().trim_matches('"').trim();
        if !address.is_empty() {
            return if name.is_empty() {
                Mailbox::new(address)
            } else {
                Mailbox::with_name(address, name)
            };
        }
    }
    Mailbox::new(text)
}

fn parse_address_list(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

fn parse_mailboxes(json: &str) -> Vec<Mailbox> {
    parse_address_list(json)
        .iter()
        .map(|display| parse_address(display))
        .collect()
}

/// 列表第三行的摘要：正文第一个非空行，最多 120 个字符。
fn snippet_of(body: &str) -> String {
    const LIMIT: usize = 120;
    let line = body
        .split(['\n', '\r'])
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if line.chars().count() <= LIMIT {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(LIMIT).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::models::{ConnectionSecurity, NewAccount};

    /// 内存库 + 一个账户。
    fn mem() -> (Connection, i64) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations::run(&mut conn).unwrap();
        let id = super::super::accounts::insert(
            &conn,
            &NewAccount {
                email: "me@example.com".into(),
                display_name: Some("Me".into()),
                username: "me@example.com".into(),
                imap_host: "h".into(),
                imap_port: 993,
                smtp_host: "s".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "p".into(),
            },
        )
        .unwrap();
        (conn, id)
    }

    fn record(account_id: i64) -> DraftRecord {
        let mut draft = Draft::new(
            Mailbox::with_name("me@example.com", "Me"),
            Mailbox::new("bob@example.com"),
            "报表",
            "第一行\n\n第二行",
        );
        draft.cc = vec![Mailbox::with_name("carol@example.com", "Carol")];
        DraftRecord {
            id: None,
            account_id,
            draft,
            mode: DraftMode::New,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn a_new_draft_round_trips_every_field() {
        let (conn, acct) = mem();
        let mut saved = record(acct);
        saved.mode = DraftMode::Reply;
        saved.draft.bcc = vec![Mailbox::new("dave@example.com")];
        saved.attachments = vec![DraftAttachmentRef::new(
            "/tmp/report.pdf",
            "报表.pdf",
            Some("application/pdf".into()),
        )];

        let id = save(&conn, &saved, 1_700_000_000).unwrap();
        let back = load(&conn, acct, id).unwrap();

        assert_eq!(back.id, Some(id));
        assert_eq!(back.account_id, acct);
        assert_eq!(back.mode, DraftMode::Reply);
        assert_eq!(back.draft.subject, "报表");
        assert_eq!(back.draft.body, "第一行\n\n第二行");
        assert_eq!(back.draft.to.len(), 1);
        assert_eq!(back.draft.to[0].address, "bob@example.com");
        assert_eq!(back.draft.cc[0].name.as_deref(), Some("Carol"));
        assert_eq!(back.draft.bcc[0].address, "dave@example.com");
        assert_eq!(back.draft.from.address, "me@example.com");
        assert_eq!(back.draft.from.name.as_deref(), Some("Me"));
        assert_eq!(back.attachments, saved.attachments);
        // 落库的是「最后保存时间」，不是 `date_unix`。
        assert_eq!(list(&conn, acct).unwrap()[0].date, 1_700_000_000);
    }

    #[test]
    fn saving_again_reuses_the_same_row() {
        let (conn, acct) = mem();
        let first = record(acct);
        let id = save(&conn, &first, 1_000).unwrap();

        let mut second = record(acct);
        second.id = Some(id);
        second.draft.subject = "改过的主题".into();
        let again = save(&conn, &second, 2_000).unwrap();

        assert_eq!(again, id, "改存必须是同一行");
        assert_eq!(count(&conn, acct).unwrap(), 1);
        let back = load(&conn, acct, id).unwrap();
        assert_eq!(back.draft.subject, "改过的主题");
        assert_eq!(
            list(&conn, acct).unwrap()[0].date,
            2_000,
            "最后保存时间要跟着走"
        );
    }

    #[test]
    fn the_message_id_is_generated_once_and_kept() {
        let (conn, acct) = mem();
        let first = record(acct);
        let id = save(&conn, &first, 1_000).unwrap();
        let generated = load(&conn, acct, id).unwrap().draft.message_id;
        assert!(!generated.is_empty(), "新草稿该有一个 Message-ID");

        // 后续每次存草稿都**不能**换 id，否则别人引这一封的 In-Reply-To 会悬空。
        let mut second = record(acct);
        second.id = Some(id);
        save(&conn, &second, 2_000).unwrap();
        assert_eq!(load(&conn, acct, id).unwrap().draft.message_id, generated);
    }

    #[test]
    fn a_reply_draft_keeps_in_reply_to_and_references() {
        let (conn, acct) = mem();
        // 先造一封「收到的信」，让 Message-ID 能在本地查到。
        let folder = super::super::folders::upsert(
            &conn,
            acct,
            &super::super::folders::FolderUpsert {
                name: "INBOX".into(),
                delimiter: Some("/".into()),
                role: crate::models::FolderRole::Inbox,
                uidvalidity: Some(1),
                uidnext: Some(10),
            },
        )
        .unwrap();
        super::super::messages::upsert_envelope(
            &conn,
            acct,
            folder,
            &super::super::messages::EnvelopeRow {
                uid: 7,
                message_id: Some("parent@example.com".into()),
                subject: "原信".into(),
                date: 900,
                ..Default::default()
            },
        )
        .unwrap();

        let mut reply = record(acct);
        reply.mode = DraftMode::Reply;
        reply.draft.in_reply_to = Some("parent@example.com".into());
        reply.draft.references = vec!["root@example.com".into(), "parent@example.com".into()];
        let id = save(&conn, &reply, 1_000).unwrap();

        let back = load(&conn, acct, id).unwrap();
        assert_eq!(
            back.draft.in_reply_to.as_deref(),
            Some("parent@example.com")
        );
        assert_eq!(back.mode, DraftMode::Reply);
        // ⚠️ 顺序**不保**：`message_refs` 是 `(message_id, ref_message_id)` 的联合主键，
        // 读回来按引用串排序（模块头的「已知取舍」）。所以这里比集合，不比 Vec。
        let refs: std::collections::BTreeSet<&str> =
            back.draft.references.iter().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> = ["parent@example.com", "root@example.com"]
            .into_iter()
            .collect();
        assert_eq!(refs, expected);

        let message = compose::build_message(&{
            let mut draft = back.draft.clone();
            draft.stamp(1_000);
            draft
        })
        .unwrap();
        assert!(message.contains("In-Reply-To: <parent@example.com>"));
        assert!(message.contains("<root@example.com>"));
    }

    #[test]
    fn attachments_are_replaced_not_appended() {
        let (conn, acct) = mem();
        let mut draft = record(acct);
        draft.attachments = vec![
            DraftAttachmentRef::new("/tmp/a.txt", "a.txt", None),
            DraftAttachmentRef::new("/tmp/b.txt", "b.txt", None),
        ];
        let id = save(&conn, &draft, 1_000).unwrap();
        assert_eq!(list(&conn, acct).unwrap()[0].attachment_count, 2);

        // 移除一个再存：旧行必须没了，不能留成 3 行。
        draft.id = Some(id);
        draft.attachments.truncate(1);
        save(&conn, &draft, 2_000).unwrap();
        let back = load(&conn, acct, id).unwrap();
        assert_eq!(back.attachments.len(), 1);
        assert_eq!(back.attachments[0].filename, "a.txt");
    }

    #[test]
    fn list_is_newest_first_and_scoped_to_the_account() {
        let (conn, acct) = mem();
        let mut older = record(acct);
        older.draft.subject = "旧".into();
        let older_id = save(&conn, &older, 1_000).unwrap();
        let mut newer = record(acct);
        newer.draft.subject = "新".into();
        let newer_id = save(&conn, &newer, 2_000).unwrap();

        let rows = list(&conn, acct).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![newer_id, older_id]
        );
        // 另一个账户看不到。
        let other = super::super::accounts::insert(
            &conn,
            &NewAccount {
                email: "other@example.com".into(),
                display_name: None,
                username: "other@example.com".into(),
                imap_host: "h".into(),
                imap_port: 993,
                smtp_host: "s".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "p".into(),
            },
        )
        .unwrap();
        assert!(list(&conn, other).unwrap().is_empty());
        assert_eq!(count(&conn, acct).unwrap(), 2);
    }

    #[test]
    fn deleting_a_draft_takes_its_children_with_it() {
        let (conn, acct) = mem();
        let mut draft = record(acct);
        draft.attachments = vec![DraftAttachmentRef::new("/tmp/a.txt", "a.txt", None)];
        let id = save(&conn, &draft, 1_000).unwrap();

        assert!(delete(&conn, acct, id).unwrap());
        assert!(!delete(&conn, acct, id).unwrap(), "删过了再删就是 false");
        assert!(matches!(
            load(&conn, acct, id).unwrap_err(),
            MailError::NotFound(_)
        ));
        assert_eq!(count(&conn, acct).unwrap(), 0);
        for table in [
            "message_bodies",
            "drafts_meta",
            "draft_attachments",
            "message_refs",
        ] {
            let left: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(left, 0, "{table} 该被级联清空");
        }
    }

    #[test]
    fn saving_onto_a_foreign_id_is_not_found() {
        let (conn, acct) = mem();
        let mut draft = record(acct);
        draft.id = Some(9_999);
        assert!(matches!(
            save(&conn, &draft, 1_000).unwrap_err(),
            MailError::NotFound(_)
        ));
        assert_eq!(count(&conn, acct).unwrap(), 0, "不许顺手新建一行");
    }

    #[test]
    fn loading_another_accounts_draft_is_not_found() {
        let (conn, acct) = mem();
        let id = save(&conn, &record(acct), 1_000).unwrap();
        let other = super::super::accounts::insert(
            &conn,
            &NewAccount {
                email: "other@example.com".into(),
                display_name: None,
                username: "other@example.com".into(),
                imap_host: "h".into(),
                imap_port: 993,
                smtp_host: "s".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "p".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            load(&conn, other, id).unwrap_err(),
            MailError::NotFound(_)
        ));
    }

    /// 草稿的 `folder_id` 是 NULL —— 它绝不能从「按文件夹列邮件」那条路冒出来，
    /// 也绝不能被检索捞出来（后者走到 `row_to_summary` 会直接报类型错误）。
    #[test]
    fn drafts_never_leak_into_folder_listing_or_search() {
        let (conn, acct) = mem();
        let folder = super::super::folders::upsert(
            &conn,
            acct,
            &super::super::folders::FolderUpsert {
                name: "INBOX".into(),
                delimiter: Some("/".into()),
                role: crate::models::FolderRole::Inbox,
                uidvalidity: Some(1),
                uidnext: Some(10),
            },
        )
        .unwrap();
        let mut draft = record(acct);
        draft.attachments = vec![DraftAttachmentRef::new("/tmp/a.txt", "a.txt", None)];
        let id = save(&conn, &draft, 1_000).unwrap();

        let listed = super::super::messages::list_by_folder(&conn, folder, 50, 0).unwrap();
        assert!(listed.is_empty(), "草稿不该出现在文件夹列表里");
        // `has:attachment` 是纯过滤器（不依赖 FTS 索引），草稿若漏进来就会命中。
        let hits = super::super::search::search(&conn, "has:attachment", 50).unwrap();
        assert!(!hits.contains(&id), "草稿不该被检索捞出来：{hits:?}");
    }

    #[test]
    fn address_display_and_parse_are_inverse() {
        for mailbox in [
            Mailbox::new("a@example.com"),
            Mailbox::with_name("b@example.com", "Bob"),
            Mailbox::with_name("c@example.com", "张三"),
            Mailbox::with_name("d@example.com", "Weird, <Name>"),
        ] {
            assert_eq!(parse_address(&address_display(&mailbox)), mailbox);
        }
        // 认不出来的一律当「整串就是地址」，不报错。
        assert_eq!(
            parse_address("  nobody@example.com  ").address,
            "nobody@example.com"
        );
    }

    #[test]
    fn snippet_is_the_first_non_empty_line_capped_at_120_chars() {
        assert_eq!(snippet_of("\n\n  你好  \n世界"), "你好");
        assert_eq!(snippet_of(""), "");
        let long = "字".repeat(200);
        let snippet = snippet_of(&long);
        assert_eq!(snippet.chars().count(), 121, "120 个字符 + 省略号");
        assert!(snippet.ends_with('…'));
    }

    #[test]
    fn unknown_modes_fall_back_to_new() {
        assert_eq!(DraftMode::from_db("reply"), DraftMode::Reply);
        assert_eq!(DraftMode::from_db("forward"), DraftMode::Forward);
        assert_eq!(DraftMode::from_db("weird"), DraftMode::New);
        assert_eq!(DraftMode::New.as_str(), "new");
    }
}
