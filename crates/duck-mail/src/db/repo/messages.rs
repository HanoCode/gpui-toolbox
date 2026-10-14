//! `messages` / `message_bodies` / `attachments` 的读写。
//!
//! 列表查询直接基于 `messages`（M1 不做会话合并）。写路径全部是 upsert，键为
//! `(account_id, folder_id, uid)`——同一封邮件重复同步（重启后增量重跑）不会产生
//! 副本，这正是 M1 验收项「重启后增量同步不重复拉取」要的行为。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{MailError, Result};
use crate::models::{AttachmentMeta, MessageDetail, MessageSummary};

/// 从 IMAP 信封（或 BODYSTRUCTURE 摘要）落库的一行。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvelopeRow {
    /// IMAP UID（文件夹内唯一）。
    pub uid: i64,
    /// `Message-ID` 头。
    pub message_id: Option<String>,
    /// 主题。
    pub subject: String,
    /// 发件人显示名。
    pub from_name: Option<String>,
    /// 发件人地址。
    pub from_addr: Option<String>,
    /// 收件人（`"名字 <addr>"` 形态）。
    pub to: Vec<String>,
    /// 抄送。
    pub cc: Vec<String>,
    /// 发送时间（Unix 秒）。
    pub date: i64,
    /// 是否已读（`\Seen`）。
    pub is_read: bool,
    /// 是否旗标（`\Flagged`）。
    pub is_starred: bool,
    /// 是否有附件。
    pub has_attachments: bool,
    /// 字节数。
    pub size: Option<i64>,
    /// 摘要（列表第三行）。
    pub snippet: String,
}

/// 写入或更新一封邮件的信封，返回主键。
///
/// 已存在时只更新**信封类字段**（旗标、主题、摘要…），**不碰 `body_state` 与正文**
/// ——正文已经拉过了，重复覆盖会让阅读器闪回「加载中」。
pub fn upsert_envelope(
    conn: &Connection,
    account_id: i64,
    folder_id: i64,
    env: &EnvelopeRow,
) -> Result<i64> {
    let to_json = serde_json::to_string(&env.to).unwrap_or_else(|_| "[]".into());
    let cc_json = serde_json::to_string(&env.cc).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO messages
           (account_id, folder_id, uid, message_id, subject, from_name, from_addr,
            to_json, cc_json, date, is_read, is_starred, has_attachments, size, snippet)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(account_id, folder_id, uid) DO UPDATE SET
             message_id      = excluded.message_id,
             subject         = excluded.subject,
             from_name       = excluded.from_name,
             from_addr       = excluded.from_addr,
             to_json         = excluded.to_json,
             cc_json         = excluded.cc_json,
             date            = excluded.date,
             is_read         = excluded.is_read,
             is_starred      = excluded.is_starred,
             has_attachments = excluded.has_attachments,
             size            = excluded.size,
             snippet         = excluded.snippet",
        params![
            account_id,
            folder_id,
            env.uid,
            env.message_id,
            env.subject,
            env.from_name,
            env.from_addr,
            to_json,
            cc_json,
            env.date,
            env.is_read as i64,
            env.is_starred as i64,
            env.has_attachments as i64,
            env.size,
            env.snippet,
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM messages WHERE account_id = ?1 AND folder_id = ?2 AND uid = ?3",
        params![account_id, folder_id, env.uid],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// 与 [`upsert_envelope`] 同口径，但**额外报告这一封是不是新落库的行**。
///
/// 增量同步必须按「真的插入了新行」计数，不能按「拉回几封信封」计数：
/// [`upsert_envelope`] 是 `ON CONFLICT(account_id, folder_id, uid) DO UPDATE`，
/// 重复拉到的老邮件照样写成功；而服务端确实会重复回吐（见
/// [`crate::proto::imap::uid_in_requested_range`] 说明的范围语义）。按「拉回几封」
/// 计数，就会每轮上报一封并不存在的新邮件——「一直提示有新邮件、其实什么都没有」
/// 的直接成因。
///
/// 第二处差别：**冲突更新路径不写 `has_attachments`**。列表阶段只有头部，附件要从
/// MIME 结构里才看得出来（见 [`set_has_attachments`]），此刻传的 `false` 只是
/// 「还不知道」；覆盖上去会把正文解析阶段已经置位的「有附件」抹回 0，回形针角标
/// 会随着每轮同步反复消失。**新插入**的行仍用传入值。
///
/// 刻意与 [`upsert_envelope`] 并存而不是改它的签名：那个函数另有 10 余处调用点
/// （草稿 / 队列 / 写回已发送 / 测试），它们的语义不该被这次同步修复顺手改动。
pub fn upsert_envelope_tracking(
    conn: &Connection,
    account_id: i64,
    folder_id: i64,
    env: &EnvelopeRow,
) -> Result<(i64, bool)> {
    let existed = conn
        .query_row(
            "SELECT id FROM messages WHERE account_id = ?1 AND folder_id = ?2 AND uid = ?3",
            params![account_id, folder_id, env.uid],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    let to_json = serde_json::to_string(&env.to).unwrap_or_else(|_| "[]".into());
    let cc_json = serde_json::to_string(&env.cc).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO messages
           (account_id, folder_id, uid, message_id, subject, from_name, from_addr,
            to_json, cc_json, date, is_read, is_starred, has_attachments, size, snippet)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(account_id, folder_id, uid) DO UPDATE SET
             message_id  = excluded.message_id,
             subject     = excluded.subject,
             from_name   = excluded.from_name,
             from_addr   = excluded.from_addr,
             to_json     = excluded.to_json,
             cc_json     = excluded.cc_json,
             date        = excluded.date,
             is_read     = excluded.is_read,
             is_starred  = excluded.is_starred,
             size        = excluded.size,
             snippet     = excluded.snippet",
        params![
            account_id,
            folder_id,
            env.uid,
            env.message_id,
            env.subject,
            env.from_name,
            env.from_addr,
            to_json,
            cc_json,
            env.date,
            env.is_read as i64,
            env.is_starred as i64,
            env.has_attachments as i64,
            env.size,
            env.snippet,
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM messages WHERE account_id = ?1 AND folder_id = ?2 AND uid = ?3",
        params![account_id, folder_id, env.uid],
        |row| row.get(0),
    )?;
    Ok((id, !existed))
}

/// 替换一封邮件的引用链（`message_refs`）：先删旧、再插新。
///
/// 走「先删后插」而不是「只插不删」：重同步时引用可能变（服务端修正了头部，
/// 或本地换了归一化口径），只插不删会留下幽灵引用，把不相关的会话并到一起。
/// 空 `refs` 即清空引用——合法且常见（一封没有 `In-Reply-To` / `References` 的普通信）。
///
/// 引用值应已归一化（去尖括号、折叠空白），由 [`crate::mime::ParsedHeaders::references`]
/// 保证；这里只做 `trim` 兜底，避免调用方从别处塞进带空白的值。
pub fn replace_references<S: AsRef<str>>(
    conn: &Connection,
    message_id: i64,
    refs: &[S],
) -> Result<()> {
    conn.execute(
        "DELETE FROM message_refs WHERE message_id = ?1",
        params![message_id],
    )?;
    for reference in refs {
        let reference = reference.as_ref().trim();
        if reference.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO message_refs (message_id, ref_message_id) VALUES (?1, ?2)",
            params![message_id, reference],
        )?;
    }
    Ok(())
}

/// 某文件夹的列表行，按时间倒序，分页。
pub fn list_by_folder(
    conn: &Connection,
    folder_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<MessageSummary>> {
    let mut stmt = conn.prepare(
        "SELECT id, folder_id, uid, subject, from_name, from_addr, snippet, date,
                is_read, is_starred, has_attachments
         FROM messages
         WHERE folder_id = ?1
         ORDER BY date DESC, id DESC
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![folder_id, limit, offset], row_to_summary)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 某文件夹的邮件总数。
pub fn count_in_folder(conn: &Connection, folder_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
        [folder_id],
        |row| row.get(0),
    )?)
}

/// 文件夹内已有的全部 UID（删除对账用）。
pub fn existing_uids(conn: &Connection, folder_id: i64) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT uid FROM messages WHERE folder_id = ?1 AND uid IS NOT NULL")?;
    let rows = stmt.query_map([folder_id], |row| row.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 文件夹内的最大 UID（水位对账用）。
pub fn max_uid(conn: &Connection, folder_id: i64) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT MAX(uid) FROM messages WHERE folder_id = ?1",
        [folder_id],
        |row| row.get(0),
    )?)
}

/// 按 UID 批量删除（服务端已删掉的邮件），返回真正删掉的行数。
///
/// **同时清掉 FTS 索引**：`messages_fts` 是虚表，没有外键级联，漏了它会让
/// `search` 返回一批 `messages` 里已经不存在的 rowid。
pub fn delete_by_uids(conn: &Connection, folder_id: i64, uids: &[i64]) -> Result<usize> {
    let mut removed = 0usize;
    for uid in uids {
        // 先取主键：索引按 `messages.id` 建，行删掉之后就拿不到它了。
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2",
                params![folder_id, uid],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            continue;
        };
        crate::db::repo::search::remove_message(conn, id)?;
        removed += conn.execute("DELETE FROM messages WHERE id = ?1", [id])?;
    }
    Ok(removed)
}

/// 一段 UID 窗口内本地的 `(uid, is_read, is_starred)`（旗标对账用）。
///
/// 窗口按 `date DESC` 取——最近这批才是用户会看到、也最可能被别的设备改动的。
pub fn flags_window(
    conn: &Connection,
    folder_id: i64,
    limit: i64,
) -> Result<Vec<(i64, bool, bool)>> {
    let mut stmt = conn.prepare(
        "SELECT uid, is_read, is_starred FROM messages
         WHERE folder_id = ?1 AND uid IS NOT NULL
         ORDER BY date DESC, id DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![folder_id, limit], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)? != 0,
            row.get::<_, i64>(2)? != 0,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 按一组主键取列表行，**保持传入的顺序**（FTS5 的相关度序）。
///
/// `ids` 里已经不存在的（索引脏了 / 邮件被别的设备删了）直接跳过，不报错——
/// 检索结果是「能显示多少显示多少」，不该被一条脏 rowid 整个打断。
pub fn list_by_ids(conn: &Connection, ids: &[i64]) -> Result<Vec<MessageSummary>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT id, folder_id, uid, subject, from_name, from_addr, snippet, date,
                is_read, is_starred, has_attachments
         FROM messages WHERE id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), row_to_summary)?;
    let mut found: Vec<MessageSummary> = Vec::new();
    for row in rows {
        found.push(row?);
    }
    // 相关度序在 `ids` 里，SQL 的 `IN` 不保证顺序，所以在 Rust 侧排回去。
    let rank = |id: i64| {
        ids.iter()
            .position(|probe| *probe == id)
            .unwrap_or(usize::MAX)
    };
    found.sort_by_key(|m| rank(m.id));
    Ok(found)
}

/// 取一封邮件的详情（含正文与附件）。
pub fn get_detail(conn: &Connection, id: i64) -> Result<MessageDetail> {
    let mut detail = conn
        .query_row(
            "SELECT m.id, m.account_id, m.thread_id, m.folder_id, m.uid, m.message_id,
                    m.subject, m.from_name, m.from_addr, m.to_json, m.cc_json, m.date,
                    m.is_read, m.is_starred, m.has_attachments, m.size, m.body_state,
                    b.text_body, b.html_body
             FROM messages m
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE m.id = ?1",
            [id],
            |row| {
                let to_json: String = row.get(9)?;
                let cc_json: String = row.get(10)?;
                Ok(MessageDetail {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    thread_id: row.get(2)?,
                    folder_id: row.get(3)?,
                    uid: row.get(4)?,
                    message_id: row.get(5)?,
                    subject: row.get(6)?,
                    from_name: row.get(7)?,
                    from_addr: row.get(8)?,
                    to: parse_address_list(&to_json),
                    cc: parse_address_list(&cc_json),
                    date: row.get(11)?,
                    is_read: row.get::<_, i64>(12)? != 0,
                    is_starred: row.get::<_, i64>(13)? != 0,
                    has_attachments: row.get::<_, i64>(14)? != 0,
                    size: row.get(15)?,
                    body_state: row.get(16)?,
                    text_body: row.get(17)?,
                    html_body: row.get(18)?,
                    attachments: Vec::new(),
                })
            },
        )
        .optional()?
        .ok_or_else(|| MailError::NotFound(format!("message {id}")))?;

    detail.attachments = attachments_for(conn, id)?;
    Ok(detail)
}

/// 写正文（幂等 upsert）。`body_state` 置 `cached`。
pub fn save_body(conn: &Connection, id: i64, text: Option<&str>, html: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO message_bodies (message_id, text_body, html_body)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(message_id) DO UPDATE SET
             text_body = excluded.text_body,
             html_body = excluded.html_body",
        params![id, text, html],
    )?;
    conn.execute(
        "UPDATE messages SET body_state = 'cached' WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// 置正文状态（`none` / `fetching` / `cached`）。
pub fn set_body_state(conn: &Connection, id: i64, state: &str) -> Result<()> {
    conn.execute(
        "UPDATE messages SET body_state = ?2 WHERE id = ?1",
        params![id, state],
    )?;
    Ok(())
}

/// **原子抢占**一次正文拉取：只有当前是 `none` 的行会被置成 `fetching`。
///
/// 返回 `true` = 抢到了（调用方必须真的去拉，并在拿到后写正文 / 失败时释放）；
/// 返回 `false` = **不要**发起网络请求，有两种成因，处理方式相同：
///
/// - 已经是 `fetching`：另一条路径（用户点开、或后台预取）正在拉这一封；
/// - 已经是 `cached`：刚被别处拉完。
///
/// 为什么必须是「带条件的 UPDATE」而不是「先 SELECT 再 UPDATE」：读写之间会被别的
/// 连接插进来，两条路径同时读到 `none`，于是同一封信被拉两遍。SQLite 的单语句
/// UPDATE 自带这个原子性，不需要额外加锁（对照 Flectar `begin_body_fetch`）。
pub fn claim_body_fetch(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE messages SET body_state = 'fetching'
         WHERE id = ?1 AND body_state = 'none'",
        [id],
    )? != 0)
}

/// 放弃一次正文拉取：只在**还没完成**时把 `fetching` 退回 `none`。
///
/// `AND body_state = 'fetching'` 是这条语句的全部意义：如果 [`save_body`] 已经把它
/// 推到 `cached`，这里就什么都不做（`cached` 绝不能被失败路径打回 `none`，否则会
/// 出现「正文其实已经拿到了、状态却说没拉」，下次又要连一次服务器）。
pub fn release_body_fetch(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE messages SET body_state = 'none'
         WHERE id = ?1 AND body_state = 'fetching'",
        [id],
    )?;
    Ok(())
}

/// 启动时清理**悬挂**的 `fetching`，返回被清理的行数。
///
/// 上一次运行在拉正文途中被杀掉（崩溃、强退、断电）会留下永远停在 `fetching` 的行：
/// 没有任何任务在跑，界面却永远显示「正在下载正文」，而抢占闸门（[`claim_body_fetch`]
/// 只认 `none`）又让它们再也抢不动——一封邮件就这样彻底拉不到正文了。启动时统一
/// 退回 `none`，让它们重新可抢占。
///
/// 只在**打开库**时调用一次：放进同步循环里会把正在进行的拉取一起误伤。
/// 索引 `idx_messages_body_fetching`（005 迁移）让「没有悬挂行」这个常见情形不需要
/// 扫全表。参考 Flectar `lib.rs:485-497` 的同一处兜底。
pub fn reset_stale_body_fetches(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE messages SET body_state = 'none' WHERE body_state = 'fetching'",
        [],
    )?)
}

/// 还需要拉正文的邮件 id（**新→旧**），一次最多 `limit` 条。
///
/// `uid IS NOT NULL` 是硬前提：没有 UID 的行还没和服务端对上（或来自导入路径），
/// 按 UID 拉正文必然失败。排序与 `idx_messages_pending_body_page`（011 迁移的部分
/// 索引）**完全对齐**——那条索引就是为这个查询建的，写成别的顺序会退化成全表扫。
pub fn missing_body_ids(conn: &Connection, folder_id: i64, limit: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM messages
         WHERE folder_id = ?1 AND uid IS NOT NULL AND body_state = 'none'
         ORDER BY date DESC, id DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![folder_id, limit], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(rows)
}

/// 按**本地行 id 游标**取一批还需要拉正文的邮件（新→旧），最多 `limit` 条。
///
/// 与 [`missing_body_ids`] 的分工：那个固定取「最新若干封」（覆盖刚到的邮件，每轮
/// 同步都跑一遍），这个往老推进，是收件箱全量回填的推进器。游标语义就是
/// `messages.id < before_id`——本地主键单调递增，拿它当游标比拿 `date` 稳：同一秒
/// 到达的两封不会互相越位，也不会因为服务端时间戳乱序而漏拉。`before_id` 传
/// `i64::MAX` 就等于「从头开始」。
///
/// 排序与 `idx_messages_body_backfill`（013 迁移）**完全对齐**——那是专为这条查询
/// 建的部分索引，写成别的顺序会退化成扫整表。
pub fn missing_body_ids_before(
    conn: &Connection,
    folder_id: i64,
    before_id: i64,
    limit: i64,
) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM messages
         WHERE folder_id = ?1 AND uid IS NOT NULL AND body_state = 'none' AND id < ?2
         ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![folder_id, before_id, limit], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(rows)
}

/// 改已读 / 旗标。
pub fn set_flags(conn: &Connection, id: i64, is_read: bool, is_starred: bool) -> Result<()> {
    conn.execute(
        "UPDATE messages SET is_read = ?2, is_starred = ?3 WHERE id = ?1",
        params![id, is_read as i64, is_starred as i64],
    )?;
    Ok(())
}

/// 置「含附件」标记。
///
/// 列表阶段只有头部，猜不出有没有附件（附件可能没有 `filename`、也可能纯靠
/// `Content-Disposition` 区分），所以这一位只能等正文拉下来、MIME 结构解析完再回填。
pub fn set_has_attachments(conn: &Connection, id: i64, has: bool) -> Result<()> {
    conn.execute(
        "UPDATE messages SET has_attachments = ?2 WHERE id = ?1",
        params![id, has as i64],
    )?;
    Ok(())
}

/// 按 UID 置旗标（同步时按服务端结果批量覆盖本地）。
pub fn set_flags_by_uid(
    conn: &Connection,
    folder_id: i64,
    uid: i64,
    is_read: bool,
    is_starred: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE messages SET is_read = ?3, is_starred = ?4
         WHERE folder_id = ?1 AND uid = ?2",
        params![folder_id, uid, is_read as i64, is_starred as i64],
    )?;
    Ok(())
}

/// 替换一封邮件的附件清单（重新解析正文后调用）。
pub fn replace_attachments(
    conn: &Connection,
    message_id: i64,
    attachments: &[NewAttachment],
) -> Result<()> {
    conn.execute(
        "DELETE FROM attachments WHERE message_id = ?1",
        [message_id],
    )?;
    for a in attachments {
        conn.execute(
            "INSERT INTO attachments
               (message_id, filename, mime_type, size, content_id, is_inline,
                file_path, imap_section)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                message_id,
                a.filename,
                a.mime_type,
                a.size,
                a.content_id,
                a.is_inline as i64,
                a.file_path,
                a.imap_section,
            ],
        )?;
    }
    Ok(())
}

/// 新附件（入库用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttachment {
    /// 文件名。
    pub filename: String,
    /// MIME 类型。
    pub mime_type: String,
    /// 字节数。
    pub size: i64,
    /// `Content-ID`（去尖括号）。
    pub content_id: Option<String>,
    /// 是否内联。
    pub is_inline: bool,
    /// 本地实体文件路径（未下载为 `None`）。
    pub file_path: Option<String>,
    /// IMAP 段号。
    pub imap_section: Option<String>,
}

/// 只改「已读」一位（动作队列的乐观更新 / 撤销用，不碰旗标位）。
pub fn set_read(conn: &Connection, id: i64, is_read: bool) -> Result<()> {
    conn.execute(
        "UPDATE messages SET is_read = ?2 WHERE id = ?1",
        params![id, is_read as i64],
    )?;
    Ok(())
}

/// 只改「旗标（星标）」一位（不碰已读位）。
pub fn set_starred(conn: &Connection, id: i64, is_starred: bool) -> Result<()> {
    conn.execute(
        "UPDATE messages SET is_starred = ?2 WHERE id = ?1",
        params![id, is_starred as i64],
    )?;
    Ok(())
}

/// 把一封邮件挪到另一个文件夹（**乐观本地移动 / 撤销**用）。
///
/// 用 `UPDATE OR IGNORE` 是**有意的**：`messages` 上有 `UNIQUE(account_id, folder_id, uid)`，
/// 目标文件夹里已经存在同一 UID 的行（另一台设备移动后同步下来、或垃圾箱里恰好有同号）
/// 时，这个更新会冲突。冲突时**跳过本地挪动**（行留在原处），而不是报错中断整条动作——
/// 这只是「乐观 UI 没跟上」，远端动作照常排队；下一次同步会把状态对齐。
///
/// ⚠️ `ON CONFLICT` **不覆盖外键**：`folder_id` 指向不存在的文件夹时仍会报
/// `MailError::Db`。所以调用方必须先确认目标文件夹存在（见 `queue::apply_local`）。
pub fn set_folder(conn: &Connection, id: i64, folder_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE OR IGNORE messages SET folder_id = ?2 WHERE id = ?1",
        params![id, folder_id],
    )?;
    Ok(())
}

/// 一封邮件的「位置 + 旗标」快照（动作队列用，避免为取几个标量拉整个详情）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageLocation {
    /// 所属账户。
    pub account_id: i64,
    /// 所属文件夹（可能为 `NULL`）。
    pub folder_id: Option<i64>,
    /// IMAP UID。
    pub uid: Option<i64>,
    /// 是否已读。
    pub is_read: bool,
    /// 是否旗标。
    pub is_starred: bool,
}

/// 取一封邮件的位置与旗标快照。
pub fn location(conn: &Connection, id: i64) -> Result<MessageLocation> {
    conn.query_row(
        "SELECT account_id, folder_id, uid, is_read, is_starred FROM messages WHERE id = ?1",
        [id],
        |row| {
            Ok(MessageLocation {
                account_id: row.get(0)?,
                folder_id: row.get(1)?,
                uid: row.get(2)?,
                is_read: row.get::<_, i64>(3)? != 0,
                is_starred: row.get::<_, i64>(4)? != 0,
            })
        },
    )
    .optional()?
    .ok_or_else(|| MailError::NotFound(format!("message {id}")))
}

/// 某个标签下的邮件（按时间倒序，跨文件夹）——左栏「标签」选中后的列表。
pub fn list_by_label(conn: &Connection, label_id: i64, limit: i64) -> Result<Vec<MessageSummary>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.folder_id, m.uid, m.subject, m.from_name, m.from_addr, m.snippet,
                m.date, m.is_read, m.is_starred, m.has_attachments
         FROM messages m
         JOIN message_labels ml ON ml.message_id = m.id
         WHERE ml.label_id = ?1
         ORDER BY m.date DESC, m.id DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![label_id, limit], row_to_summary)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 某邮件的附件清单。
pub fn attachments_for(conn: &Connection, message_id: i64) -> Result<Vec<AttachmentMeta>> {
    let mut stmt = conn.prepare(
        "SELECT id, message_id, COALESCE(filename, ''), COALESCE(mime_type, ''), COALESCE(size, 0),
                content_id, is_inline, file_path, imap_section
         FROM attachments WHERE message_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([message_id], |row| {
        Ok(AttachmentMeta {
            id: row.get(0)?,
            message_id: row.get(1)?,
            filename: row.get(2)?,
            mime_type: row.get(3)?,
            size: row.get(4)?,
            content_id: row.get(5)?,
            is_inline: row.get::<_, i64>(6)? != 0,
            file_path: row.get(7)?,
            imap_section: row.get(8)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 一个附件的**下载目标**：连库读出的一行，加上它所属邮件的 IMAP 定位。
///
/// `ordinal` 是把库里的行对回 `mime::ParsedMessage::attachments` 的凭据，说明见
/// [`attachment_target`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentTarget {
    /// `attachments.id`。
    pub attachment_id: i64,
    /// 所属邮件。
    pub message_id: i64,
    /// 所属账户（取口令 / 建连要用）。
    pub account_id: i64,
    /// 文件名（库里为空时给空串，调用方用邮件的兜底名）。
    pub filename: String,
    /// MIME 类型。
    pub mime_type: String,
    /// 上一次解析出的字节数（`0` = 未知）。
    pub size: i64,
    /// 上一次保存到的路径（没保存过为 `None`）。
    pub file_path: Option<String>,
    /// 该附件在这封邮件的清单里的序号（0 起，按 `id` 升序）。
    pub ordinal: i64,
    /// IMAP 节号（如 `2` / `1.2`）——按需下载时 `UID FETCH BODY.PEEK[<section>]` 用它。
    /// `None` = 推不出可靠段号（嵌套 `message/rfc822` 等），调用方降级整封取回。
    pub imap_section: Option<String>,
    /// 所属邮件的文件夹**服务端名**（`SELECT` 用）。邮件没有文件夹时为 `None`。
    pub folder_name: Option<String>,
    /// 所属邮件的 UID。没有 UID（本地草稿之类）时为 `None`。
    pub uid: Option<i64>,
}

/// 读出一个附件的下载目标；附件不存在时 [`MailError::NotFound`]。
///
/// # ⚠️ `ordinal` 与 `mime::parse` 的顺序是同一个序
///
/// `replace_attachments` 按 `ParsedMessage::attachments` 的先后逐条 `INSERT`
/// （`sync.rs` 里解析完就地写库），而本函数与 `attachments_for` 都按 `id` 升序读。
/// 于是「清单里的第 n 行」就是「解析出来的第 n 个附件」，可以拿序号直接索引。
///
/// 这条耦合**没有数据库层面的保证**（`attachments` 表上没有「序号」列），所以调用
/// 端**必须**用 `filename` + `size` 复核——对不上要报错，而不是把另一个附件的内容
/// 写进用户以为的那个文件里（见 `sync::download_attachment`）。
pub fn attachment_target(conn: &Connection, attachment_id: i64) -> Result<AttachmentTarget> {
    conn.query_row(
        "SELECT a.message_id, m.account_id, COALESCE(a.filename, ''), COALESCE(a.mime_type, ''),
                COALESCE(a.size, 0), a.file_path,
                (SELECT COUNT(*) FROM attachments b
                  WHERE b.message_id = a.message_id AND b.id < a.id),
                a.imap_section, f.imap_name, m.uid
           FROM attachments a
           JOIN messages m ON m.id = a.message_id
           LEFT JOIN folders f ON f.id = m.folder_id
          WHERE a.id = ?1",
        [attachment_id],
        |row| {
            Ok(AttachmentTarget {
                attachment_id,
                message_id: row.get(0)?,
                account_id: row.get(1)?,
                filename: row.get(2)?,
                mime_type: row.get(3)?,
                size: row.get(4)?,
                file_path: row.get(5)?,
                ordinal: row.get(6)?,
                imap_section: row.get(7)?,
                folder_name: row.get(8)?,
                uid: row.get(9)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| MailError::NotFound(format!("attachment {attachment_id}")))
}

/// 记下一个附件保存到本地后的路径。
///
/// 只写 `file_path` 一列：文件名 / 大小 / 类型都是解析时的事实，保存动作不改它们。
pub fn set_attachment_file_path(conn: &Connection, attachment_id: i64, path: &str) -> Result<()> {
    conn.execute(
        "UPDATE attachments SET file_path = ?2 WHERE id = ?1",
        params![attachment_id, path],
    )?;
    Ok(())
}

fn parse_address_list(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(json).unwrap_or_default()
}

fn row_to_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageSummary> {
    Ok(MessageSummary {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        uid: row.get(2)?,
        subject: row.get(3)?,
        from_name: row.get(4)?,
        from_addr: row.get(5)?,
        snippet: row.get(6)?,
        date: row.get(7)?,
        is_read: row.get::<_, i64>(8)? != 0,
        is_starred: row.get::<_, i64>(9)? != 0,
        has_attachments: row.get::<_, i64>(10)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::repo::{accounts, folders};
    use crate::models::{ConnectionSecurity, FolderRole, NewAccount};

    fn mem() -> (Connection, i64, i64) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations::run(&mut conn).unwrap();
        let acct = accounts::insert(
            &conn,
            &NewAccount {
                email: "a@example.com".into(),
                display_name: None,
                username: "a@example.com".into(),
                imap_host: "h".into(),
                imap_port: 993,
                smtp_host: "s".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "p".into(),
            },
        )
        .unwrap();
        let folder = folders::upsert(
            &conn,
            acct,
            &folders::FolderUpsert {
                name: "INBOX".into(),
                delimiter: Some("/".into()),
                role: FolderRole::Inbox,
                uidvalidity: Some(1),
                uidnext: Some(10),
            },
        )
        .unwrap();
        (conn, acct, folder)
    }

    fn env(uid: i64, subject: &str, date: i64) -> EnvelopeRow {
        EnvelopeRow {
            uid,
            message_id: Some(format!("<{uid}@example.com>")),
            subject: subject.into(),
            from_name: Some("张三".into()),
            from_addr: Some("z@example.com".into()),
            to: vec!["me@example.com".into()],
            cc: vec![],
            date,
            is_read: false,
            is_starred: false,
            has_attachments: false,
            size: Some(1234),
            snippet: "你好".into(),
        }
    }

    #[test]
    fn upsert_is_idempotent_on_uid() {
        let (conn, acct, folder) = mem();
        let a = upsert_envelope(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        let b = upsert_envelope(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        assert_eq!(a, b);
        assert_eq!(count_in_folder(&conn, folder).unwrap(), 1);
    }

    #[test]
    fn re_sync_does_not_reset_cached_body() {
        let (conn, acct, folder) = mem();
        let id = upsert_envelope(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        save_body(&conn, id, Some("正文"), None).unwrap();
        // 重启后增量重跑：信封会重写，但正文必须留着。
        upsert_envelope(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        let d = get_detail(&conn, id).unwrap();
        assert_eq!(d.body_state, "cached");
        assert_eq!(d.text_body.as_deref(), Some("正文"));
    }

    /// 「新邮件」必须按**真的插入了新行**判定，而不是按「拉回了几封信封」。
    ///
    /// 服务端会用 `n:*` 的范围语义回吐区间外的那一封（见
    /// `proto::imap::uid_in_requested_range`），而重复 upsert 同一封照样成功——
    /// 同步层若照「拉回几封」计数，用户每轮都会收到一条并不存在的新邮件通知。
    #[test]
    fn tracking_upsert_reports_only_real_inserts() {
        let (conn, acct, folder) = mem();
        let (first_id, first_is_new) =
            upsert_envelope_tracking(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        assert!(first_is_new, "首次落库必须报「新」");
        // 重复同步同一封（越界回吐、或重启后增量重跑）：同一个主键，不算新增。
        let (again_id, again_is_new) =
            upsert_envelope_tracking(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        assert_eq!(first_id, again_id);
        assert!(!again_is_new, "重复 upsert 不是新邮件");
        assert_eq!(count_in_folder(&conn, folder).unwrap(), 1);
    }

    /// 更新路径**不得**碰 `has_attachments`：列表阶段只有头部、猜不出附件，
    /// 覆盖上去会把正文解析出来的「有附件」抹掉，回形针角标随每轮同步反复消失。
    #[test]
    fn tracking_upsert_keeps_the_attachment_flag_from_the_body_pass() {
        let (conn, acct, folder) = mem();
        let (id, _) = upsert_envelope_tracking(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        // 正文拉下来后解析出附件。
        set_has_attachments(&conn, id, true).unwrap();
        // 下一轮同步又拉到同一封信封（此刻还不知道有没有附件，传 `false`）。
        upsert_envelope_tracking(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        assert!(
            get_detail(&conn, id).unwrap().has_attachments,
            "重同步不该抹掉正文阶段判定出的附件标记"
        );
        // 反过来：**新插入**时传进来的值必须生效（新行没有历史可保留）。
        let (fresh, _) = upsert_envelope_tracking(
            &conn,
            acct,
            folder,
            &EnvelopeRow {
                has_attachments: true,
                ..env(2, "With file", 200)
            },
        )
        .unwrap();
        assert!(get_detail(&conn, fresh).unwrap().has_attachments);
    }

    #[test]
    fn body_fetch_claim_is_atomic_and_one_shot() {
        let (conn, acct, folder) = mem();
        let id = upsert_envelope(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        // 第一次抢到。
        assert!(claim_body_fetch(&conn, id).unwrap());
        // 第二次抢不到——已经在 `fetching`。前台点开撞上后台预取时，这条闸门就是
        // 「同一封不重复发网络请求」的全部依据。
        assert!(!claim_body_fetch(&conn, id).unwrap());
        // 拉取失败 → 释放 → 重新可抢。
        release_body_fetch(&conn, id).unwrap();
        assert_eq!(get_detail(&conn, id).unwrap().body_state, "none");
        assert!(claim_body_fetch(&conn, id).unwrap());
        // 正文写成功即 `cached`：抢占永远失败，不该再连一次服务器。
        save_body(&conn, id, Some("正文"), None).unwrap();
        assert!(!claim_body_fetch(&conn, id).unwrap());
        // ⚠️ 失败路径的释放**不能**把 `cached` 打回 `none`——否则会出现「正文其实
        // 已经拿到了、状态却说没拉」，下次又白连一次。
        release_body_fetch(&conn, id).unwrap();
        assert_eq!(get_detail(&conn, id).unwrap().body_state, "cached");
    }

    #[test]
    fn reset_stale_body_fetches_recovers_orphans_only() {
        let (conn, acct, folder) = mem();
        let a = upsert_envelope(&conn, acct, folder, &env(1, "a", 100)).unwrap();
        let b = upsert_envelope(&conn, acct, folder, &env(2, "b", 200)).unwrap();
        let c = upsert_envelope(&conn, acct, folder, &env(3, "c", 300)).unwrap();
        // a 卡在 `fetching`（上次运行时被杀掉），c 已缓存，b 从未拉过。
        claim_body_fetch(&conn, a).unwrap();
        save_body(&conn, c, Some("x"), None).unwrap();
        assert_eq!(reset_stale_body_fetches(&conn).unwrap(), 1);
        assert_eq!(get_detail(&conn, a).unwrap().body_state, "none");
        assert_eq!(get_detail(&conn, b).unwrap().body_state, "none");
        // 已缓存的正文不受影响。
        assert_eq!(get_detail(&conn, c).unwrap().body_state, "cached");
        // 幂等：没有悬挂行时是 0，调用方据此不打日志。
        assert_eq!(reset_stale_body_fetches(&conn).unwrap(), 0);
    }

    #[test]
    fn missing_body_ids_are_newest_first_and_skip_cached() {
        let (conn, acct, folder) = mem();
        let old = upsert_envelope(&conn, acct, folder, &env(1, "old", 100)).unwrap();
        let mid = upsert_envelope(&conn, acct, folder, &env(2, "mid", 200)).unwrap();
        let new = upsert_envelope(&conn, acct, folder, &env(3, "new", 300)).unwrap();
        // 中间那封已经有正文：不该出现在待拉清单里。
        save_body(&conn, mid, Some("x"), None).unwrap();
        assert_eq!(missing_body_ids(&conn, folder, 10).unwrap(), vec![new, old]);
        // `limit` 取的是**最新**的 n 封（排序方向不能反）。
        assert_eq!(missing_body_ids(&conn, folder, 1).unwrap(), vec![new]);
    }

    /// 历史回填的游标语义：`id < before_id` 且**从新往老**取，同样跳过已有正文的封。
    ///
    /// 这三条就是「收件箱全量回填」不会重复联网、也不会漏拉的全部依据。
    #[test]
    fn backfill_cursor_walks_newest_first_and_stops_at_the_cursor() {
        let (conn, acct, folder) = mem();
        let a = upsert_envelope(&conn, acct, folder, &env(1, "a", 100)).unwrap();
        let b = upsert_envelope(&conn, acct, folder, &env(2, "b", 200)).unwrap();
        let c = upsert_envelope(&conn, acct, folder, &env(3, "c", 300)).unwrap();

        // 从头开始（`i64::MAX`）：新→旧全都要。
        assert_eq!(
            missing_body_ids_before(&conn, folder, i64::MAX, 10).unwrap(),
            vec![c, b, a]
        );
        // 一批只取 `limit` 封，且取的是**最新**的那几封（排序方向不能反）。
        assert_eq!(
            missing_body_ids_before(&conn, folder, i64::MAX, 2).unwrap(),
            vec![c, b]
        );
        // 游标推过 `c`（`id < c`）：下一批从 `b` 起，`c` 不再出现。
        assert_eq!(
            missing_body_ids_before(&conn, folder, c, 10).unwrap(),
            vec![b, a]
        );
        // 已有正文的封不进清单——游标推进不会把同一封下载两遍。
        save_body(&conn, b, Some("已有正文"), None).unwrap();
        assert_eq!(
            missing_body_ids_before(&conn, folder, c, 10).unwrap(),
            vec![a]
        );
    }

    #[test]
    fn list_is_newest_first() {
        let (conn, acct, folder) = mem();
        upsert_envelope(&conn, acct, folder, &env(1, "old", 100)).unwrap();
        upsert_envelope(&conn, acct, folder, &env(2, "new", 300)).unwrap();
        upsert_envelope(&conn, acct, folder, &env(3, "mid", 200)).unwrap();
        let subs: Vec<String> = list_by_folder(&conn, folder, 10, 0)
            .unwrap()
            .into_iter()
            .map(|m| m.subject)
            .collect();
        assert_eq!(subs, vec!["new", "mid", "old"]);
    }

    #[test]
    fn list_paginates() {
        let (conn, acct, folder) = mem();
        for uid in 1..=5 {
            upsert_envelope(&conn, acct, folder, &env(uid, &format!("m{uid}"), uid)).unwrap();
        }
        let page1 = list_by_folder(&conn, folder, 2, 0).unwrap();
        let page2 = list_by_folder(&conn, folder, 2, 2).unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].subject, "m5");
        assert_eq!(page2[0].subject, "m3");
    }

    #[test]
    fn sender_falls_back_to_address() {
        let (conn, acct, folder) = mem();
        let mut e = env(1, "x", 1);
        e.from_name = None;
        let id = upsert_envelope(&conn, acct, folder, &e).unwrap();
        let row = list_by_folder(&conn, folder, 1, 0).unwrap().remove(0);
        assert_eq!(row.id, id);
        assert_eq!(row.sender_display(), "z@example.com");
    }

    #[test]
    fn uid_reconciliation_helpers() {
        let (conn, acct, folder) = mem();
        upsert_envelope(&conn, acct, folder, &env(1, "a", 1)).unwrap();
        upsert_envelope(&conn, acct, folder, &env(5, "b", 2)).unwrap();
        assert_eq!(max_uid(&conn, folder).unwrap(), Some(5));
        assert_eq!(existing_uids(&conn, folder).unwrap(), vec![1, 5]);
        // 服务端只认 1：5 该被删。
        let removed = delete_by_uids(&conn, folder, &[5]).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(existing_uids(&conn, folder).unwrap(), vec![1]);
    }

    #[test]
    fn deleting_by_uid_also_clears_the_index() {
        // 漏了索引会让 search 返回一个已经不存在的 rowid。
        let (conn, acct, folder) = mem();
        let id = upsert_envelope(&conn, acct, folder, &env(1, "findme", 1)).unwrap();
        crate::db::repo::search::index_message(&conn, id, "findme", "", "", "body").unwrap();
        assert_eq!(
            crate::db::repo::search::search(&conn, "findme", 10).unwrap(),
            vec![id]
        );
        delete_by_uids(&conn, folder, &[1]).unwrap();
        assert!(
            crate::db::repo::search::search(&conn, "findme", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn flags_window_is_newest_first_and_bounded() {
        let (conn, acct, folder) = mem();
        upsert_envelope(&conn, acct, folder, &env(1, "old", 100)).unwrap();
        let newest = upsert_envelope(&conn, acct, folder, &env(2, "new", 300)).unwrap();
        set_flags(&conn, newest, true, true).unwrap();
        let window = flags_window(&conn, folder, 1).unwrap();
        assert_eq!(window, vec![(2, true, true)]);
    }

    #[test]
    fn list_by_ids_keeps_the_requested_order_and_skips_missing() {
        let (conn, acct, folder) = mem();
        let a = upsert_envelope(&conn, acct, folder, &env(1, "a", 100)).unwrap();
        let b = upsert_envelope(&conn, acct, folder, &env(2, "b", 200)).unwrap();
        // 相关度序是 b → a；多给一个不存在的 id 也不该报错。
        let rows = list_by_ids(&conn, &[b, a, 9999]).unwrap();
        assert_eq!(rows.iter().map(|m| m.id).collect::<Vec<_>>(), vec![b, a]);
        assert!(list_by_ids(&conn, &[]).unwrap().is_empty());
    }

    #[test]
    fn flags_roundtrip_by_uid_and_id() {
        let (conn, acct, folder) = mem();
        let id = upsert_envelope(&conn, acct, folder, &env(7, "x", 1)).unwrap();
        set_flags(&conn, id, true, true).unwrap();
        let d = get_detail(&conn, id).unwrap();
        assert!(d.is_read && d.is_starred);
        set_flags_by_uid(&conn, folder, 7, false, true).unwrap();
        let d = get_detail(&conn, id).unwrap();
        assert!(!d.is_read && d.is_starred);
    }

    #[test]
    fn attachments_replace_and_read_back() {
        let (conn, acct, folder) = mem();
        let id = upsert_envelope(&conn, acct, folder, &env(1, "x", 1)).unwrap();
        replace_attachments(
            &conn,
            id,
            &[NewAttachment {
                filename: "报告.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 2048,
                content_id: None,
                is_inline: false,
                file_path: None,
                imap_section: Some("2".into()),
            }],
        )
        .unwrap();
        let list = attachments_for(&conn, id).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].filename, "报告.pdf");
        // 重解析时替换而非追加。
        replace_attachments(&conn, id, &[]).unwrap();
        assert!(attachments_for(&conn, id).unwrap().is_empty());
    }

    /// 附件的「下载目标」：序号必须就是清单位置，邮件的 IMAP 定位必须跟着带出来。
    #[test]
    fn attachment_target_carries_the_ordinal_and_the_message_locator() {
        let (conn, acct, folder) = mem();
        let id = upsert_envelope(&conn, acct, folder, &env(1, "x", 1)).unwrap();
        let new = |filename: &str, size: i64| NewAttachment {
            filename: filename.into(),
            mime_type: "application/octet-stream".into(),
            size,
            content_id: None,
            is_inline: false,
            file_path: None,
            imap_section: None,
        };
        replace_attachments(
            &conn,
            id,
            &[new("a.bin", 10), new("b.bin", 20), new("c.bin", 30)],
        )
        .unwrap();
        let list = attachments_for(&conn, id).unwrap();
        assert_eq!(list.len(), 3);

        // 序号 = 清单里的位置（`ORDER BY id`，也就是 `mime::parse` 的 push 顺序）。
        for (index, meta) in list.iter().enumerate() {
            let target = attachment_target(&conn, meta.id).unwrap();
            assert_eq!(target.ordinal, index as i64, "序号必须是清单位置");
            assert_eq!(target.filename, meta.filename);
            assert_eq!(target.message_id, id);
            assert_eq!(target.account_id, acct);
        }

        // 邮件的 IMAP 定位（下载要 `SELECT` 到文件夹 + 按 UID 取）跟着一起出来。
        let first = attachment_target(&conn, list[0].id).unwrap();
        assert_eq!(first.folder_name.as_deref(), Some("INBOX"));
        assert_eq!(first.uid, Some(1));
        assert_eq!(first.file_path, None, "还没下载过");

        // 落点回写只改 `file_path` 一列。
        set_attachment_file_path(&conn, list[1].id, "/tmp/b.bin").unwrap();
        assert_eq!(
            attachment_target(&conn, list[1].id)
                .unwrap()
                .file_path
                .as_deref(),
            Some("/tmp/b.bin")
        );
        assert_eq!(
            attachment_target(&conn, list[0].id).unwrap().file_path,
            None
        );

        // 不存在的附件：`NotFound`，而不是「随便给一行」。
        assert!(matches!(
            attachment_target(&conn, 9999),
            Err(MailError::NotFound(_))
        ));
    }

    #[test]
    fn detail_of_missing_message_is_not_found() {
        let (conn, _, _) = mem();
        assert!(matches!(
            get_detail(&conn, 999),
            Err(MailError::NotFound(_))
        ));
    }
}
