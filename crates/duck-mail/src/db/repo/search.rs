//! FTS5 全文检索。
//!
//! 虚表 `messages_fts` 是 **contentless**（`content=''` + `contentless_delete=1`），
//! 所以：
//!
//! - 写入必须**显式给 `rowid`**（= `messages.id`），FTS5 不会自己去读原文；
//! - 读不出列值，只能拿到 `rowid` —— 结果要 join 回 `messages` 取标题发件人；
//! - 删除走 `DELETE ... WHERE rowid = ?`（`contentless_delete=1` 才允许）。
//!
//! # 中文
//!
//! 虚表用的是 `unicode61 remove_diacritics 2` 分词器，**中文按整段切**（没有词边界），
//! 因此中文长句的召回很差。M1 的缓解有两条：①把正文按标点/空白切成短片段分别索引；
//! ②后续接 `nlp` 的中文分词（`docs/email-design.md` §2.4①）把词元化结果写进
//! `body` 字段。**本条注释是给未来改这里的人看的**：不要以为现在的中文召回是好的。

use rusqlite::{Connection, params};

use crate::error::Result;

/// 把一封邮件写进索引（覆盖旧值）。
///
/// `rowid` = `messages.id`，调用方保证已落库拿到主键。
pub fn index_message(
    conn: &Connection,
    message_id: i64,
    subject: &str,
    from_text: &str,
    to_text: &str,
    body: &str,
) -> Result<()> {
    // contentless 表没有「UPSERT」语义，先删后插是标准做法。
    remove_message(conn, message_id)?;
    conn.execute(
        "INSERT INTO messages_fts (rowid, subject, from_text, to_text, body)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![message_id, subject, from_text, to_text, body],
    )?;
    Ok(())
}

/// 从索引里删掉一封邮件。
pub fn remove_message(conn: &Connection, message_id: i64) -> Result<()> {
    conn.execute("DELETE FROM messages_fts WHERE rowid = ?1", [message_id])?;
    Ok(())
}

/// 检索，返回命中的 `messages.id`（按 FTS5 相关度排序）。
///
/// `raw` 是用户输入的原始查询串；内部会转成安全的 FTS5 表达式（见
/// [`to_match_expression`]）——**绝不把用户输入直接拼进 `MATCH`**。
pub fn search(conn: &Connection, raw: &str, limit: i64) -> Result<Vec<i64>> {
    let expr = to_match_expression(raw);
    if expr.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT rowid FROM messages_fts
         WHERE messages_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![expr, limit], |row| row.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 把用户输入转成安全的 FTS5 查询表达式。
///
/// 每个空白分隔的 token 用双引号括起来（内部引号翻倍），token 之间是**隐含 AND**。
/// 这样 `"`、`*`、`(`、`:`、`-` 等 FTS5 元字符都变成字面量，用户输错语法也只会
/// 「搜不到」，不会让 `MATCH` 报错。
pub fn to_match_expression(raw: &str) -> String {
    raw.split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn mem() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations::run(&mut conn).unwrap();
        conn
    }

    #[test]
    fn expression_quotes_every_token() {
        assert_eq!(to_match_expression("hello world"), "\"hello\" \"world\"");
        // 元字符被引号吃掉，不会变成语法。
        assert_eq!(to_match_expression("a*b"), "\"a*b\"");
        assert_eq!(to_match_expression("x:y"), "\"x:y\"");
        // 内部引号翻倍转义。
        assert_eq!(to_match_expression("say \"hi\""), "\"say\" \"\"\"hi\"\"\"");
        assert_eq!(to_match_expression("   "), "");
    }

    #[test]
    fn index_then_search_roundtrip() {
        let conn = mem();
        index_message(
            &conn,
            1,
            "Quarterly report",
            "alice@example.com",
            "me@x.com",
            "budget numbers",
        )
        .unwrap();
        index_message(
            &conn,
            2,
            "Lunch",
            "bob@example.com",
            "me@x.com",
            "are you free",
        )
        .unwrap();

        let hits = search(&conn, "report", 10).unwrap();
        assert_eq!(hits, vec![1]);
        let hits = search(&conn, "free", 10).unwrap();
        assert_eq!(hits, vec![2]);
        // 跨列命中（发件人）。
        let hits = search(&conn, "alice@example.com", 10).unwrap();
        assert_eq!(hits, vec![1]);
    }

    #[test]
    fn reindex_replaces_old_content() {
        let conn = mem();
        index_message(&conn, 1, "old subject", "", "", "old body").unwrap();
        index_message(&conn, 1, "new subject", "", "", "new body").unwrap();
        assert!(search(&conn, "old", 10).unwrap().is_empty());
        assert_eq!(search(&conn, "new", 10).unwrap(), vec![1]);
    }

    #[test]
    fn delete_removes_from_index() {
        let conn = mem();
        index_message(&conn, 1, "target", "", "", "body").unwrap();
        remove_message(&conn, 1).unwrap();
        assert!(search(&conn, "target", 10).unwrap().is_empty());
    }

    #[test]
    fn malformed_user_input_never_errors() {
        let conn = mem();
        index_message(&conn, 1, "hello", "", "", "world").unwrap();
        // 各种会让 FTS5 原生语法炸掉的输入，经转义后都该安静返回空。
        for raw in ["\"unclosed", "AND OR NOT", "col:", "(((", "a - b", "*"] {
            assert!(
                search(&conn, raw, 10).is_ok(),
                "query {raw:?} should not error"
            );
        }
    }

    #[test]
    fn limit_is_respected() {
        let conn = mem();
        for id in 1..=10 {
            index_message(&conn, id, "common", "", "", "common body").unwrap();
        }
        assert_eq!(search(&conn, "common", 3).unwrap().len(), 3);
    }
}
