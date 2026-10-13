//! FTS5 全文检索 + 条件查询语言。
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
//! 因此中文长句的召回很差。缓解办法是接 `nlp` 的中文分词（`docs/email-design.md`
//! §2.4①）把词元化结果写进 `body` 字段——**那要改虚表的分词器，得动 schema
//! 与重建索引，属 M3**。本条注释是给未来改这里的人看的：不要以为现在的中文召回是好的。
//!
//! # 条件查询语言
//!
//! 用户输入先经 [`parse_query`] 分流成「自由文本词」与「结构化条件」，再分别落到
//! FTS5 的 `MATCH` 与 `messages` 上的 `WHERE`：
//!
//! | 写法 | 落到哪 | 说明 |
//! | --- | --- | --- |
//! | 裸词 | FTS5 `MATCH` | 多个词是**隐含 AND** |
//! | `"两个词"` | FTS5 `MATCH` | 短语查询（引号内不再分词） |
//! | `词*` | FTS5 `MATCH` | 前缀查询；只在**纯字母数字 / 汉字**上开（见 [`safe_prefix`]） |
//! | 裸**邮箱地址** | `messages` | 自动按「发件人**或**收件人（含抄送）」匹配（`Filters::contact`）——搜人不必想 from 还是 to |
//! | `from:x` | `messages` | 子串匹配 `from_addr` / `from_name` / `sender_addr` |
//! | `to:x` | `messages` | 子串匹配收件人 JSON 串 |
//! | `subject:x` | `messages` | 子串匹配主题 |
//! | `folder:x` | `messages` | 匹配 `folders.imap_name`，或 `role`（`inbox` / `sent`…） |
//! | `is:unread` / `is:read` | `messages` | 也认 `is:starred` / `is:flagged` / `is:unstarred` |
//! | `has:attachment` | `messages` | |
//! | `before:2026-01-01` / `after:2026-01-01` | `messages` | 也认 `older:` / `newer:`；**UTC 日界**（见 [`parse_day`]） |
//!
//! ## 中文与相对日期（智能化别名）
//!
//! 上面每一行都有「不用记英文也能打出来」的说法，解析层归一（[`canonical_key`]）：
//!
//! - **key 同义**：`发件人:` / `发件:` → `from:`，`收件人:` / `收件者:` → `to:`，
//!   `主题:` / `标题:` → `subject:`，`文件夹:` → `folder:`，`早于:` / `之前:` → `before:`，
//!   `晚于:` / `之后:` / `自:` → `after:`。
//! - **全角冒号**：`key` 与值之间的 `：` 与 `:` 完全同权（中文输入法的现实）。
//! - **取值同义**：`is:未读` / `is:已读` / `is:有旗标` / `is:无旗标`，`has:附件`。
//! - **相对日期**：`after:今天 / 昨天 / 前天 / 本周 / 上周 / 本月 / 上月 / 最近7天`
//!   及英文 `today / yesterday / thisweek / lastweek / thismonth / lastmonth`
//!   （[`parse_relative_day`]，同样 UTC 边界）。
//!
//! 这些别名只改「解析成什么条件」，不改变下面三条细节的任何一条。
//!
//! ## 三条不能省的细节
//!
//! 1. **认不出的 `key:value` 整个退回自由文本**（搜 `raw` 原文，不是 `value`）。
//!    否则 `http://example.com` 会被吃成 `//example.com`、`is:whatever` 会被静默丢掉
//!    ——「搜不到」比「搜别的」好，但「搜用户原样输入的东西」更好。
//! 2. **`LIKE` 模式必须转义** `%` / `_` / `\`：不转义时用户搜 `from:%` 会匹配全部邮件，
//!    看起来像「过滤没生效」。（转义符在 SQL 里靠 `ESCAPE '\'` 声明。）
//! 3. **条件与文本都空就返回空**，绝不退化成「无 WHERE 的全表扫描」——一个只有空格的
//!    查询串不该把几万封邮件全捞出来。
//!
//! ## 没做的（不是漏了）
//!
//! `-词` 排除、`OR`、括号、`larger:` / `filename:` 这些都需要更完整的语法树；在
//! 「每加一条都要能说清落到哪张表哪一列」的前提下，先不做比做一半好。真需要时按上面
//! 这张表逐条加。

use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};

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

/// 一条查询的解析结果。
///
/// 公开出来是为了让调用方（界面 / 测试）能看见「这条输入被理解成了什么」——
/// 检索出问题时第一个要问的就是这个。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Query {
    /// 自由文本词，按用户写的顺序（多个词是隐含 AND）。
    pub text: Vec<String>,
    /// 结构化条件。
    pub filters: Filters,
}

/// 结构化过滤条件。全部字段都是 `Option` / 空 `Vec`：默认 = 不加条件。
///
/// 同一个 key 写多次（`from:a from:b`）是**累加**（两个都要满足），不覆盖——
/// 覆盖会让用户以为筛了两个发件人、实际只筛了后一个。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Filters {
    /// `is:unread` / `is:read`（`true` = 未读）。
    pub unread: Option<bool>,
    /// `is:starred` / `is:unstarred`（`true` = 有旗标）。
    pub starred: Option<bool>,
    /// `has:attachment`。
    pub has_attachment: Option<bool>,
    /// `from:`，多个之间是 AND。
    pub from: Vec<String>,
    /// `to:`，多个之间是 AND。
    pub to: Vec<String>,
    /// `subject:`，多个之间是 AND。
    pub subject: Vec<String>,
    /// **裸邮箱地址**（没有 `key:` 前缀、但长得像地址的词）。
    ///
    /// 语义是「这个人相关的邮件」：发件人 / 收件人（含抄送）任一命中即可——
    /// 与 `from:` / `to:` 的 AND 语义刻意不同，多个地址之间仍是 AND。
    pub contact: Vec<String>,
    /// `folder:`。重复写时**最后一条生效**——一个邮件只属于一个文件夹，
    /// 累加会得到一个必然为空的交集，那不是用户想要的。
    pub folder: Option<String>,
    /// `before:` / `older:`，严格早于（Unix 秒，UTC 日界）。
    pub before: Option<i64>,
    /// `after:` / `newer:`，不早于（含当日）。
    pub after: Option<i64>,
}

impl Filters {
    fn is_empty(&self) -> bool {
        self.unread.is_none()
            && self.starred.is_none()
            && self.has_attachment.is_none()
            && self.from.is_empty()
            && self.to.is_empty()
            && self.subject.is_empty()
            && self.contact.is_empty()
            && self.folder.is_none()
            && self.before.is_none()
            && self.after.is_none()
    }

    /// 追加 `WHERE` 片段与**同序**的绑定值。
    ///
    /// ⚠️ 两个 `Vec` 必须严格同序同步 push：SQL 里用的是无名 `?`，靠位置绑定。
    /// 这是本文件唯一一处「顺序即契约」的地方，改任何一条 push 都要连着看另一条。
    fn append_to(&self, parts: &mut Vec<String>, values: &mut Vec<Value>) {
        if let Some(unread) = self.unread {
            // 布尔先转 0 / 1 再 format：`is_read = false` 在 SQLite 里是合法的，
            // 但绑成整数列更直白，也和 `messages.is_read` 的类型一致。
            parts.push(format!("m.is_read = {}", i64::from(!unread)));
        }
        if let Some(starred) = self.starred {
            parts.push(format!("m.is_starred = {}", i64::from(starred)));
        }
        if let Some(has) = self.has_attachment {
            parts.push(format!("m.has_attachments = {}", i64::from(has)));
        }
        for value in &self.from {
            parts.push(
                "(m.from_addr LIKE ? ESCAPE '\\' OR m.from_name LIKE ? ESCAPE '\\' \
                 OR m.sender_addr LIKE ? ESCAPE '\\')"
                    .to_owned(),
            );
            let pattern = Value::Text(like_pattern(value));
            // 同一个模式绑三次：三个列各一次。`sender_addr` 是信封发件人，
            // 与 `from_addr` 在转发 / 代发时不同，漏了它会搜不到一些邮件。
            values.push(pattern.clone());
            values.push(pattern.clone());
            values.push(pattern);
        }
        for value in &self.to {
            parts.push("m.to_json LIKE ? ESCAPE '\\'".to_owned());
            values.push(Value::Text(like_pattern(value)));
        }
        for value in &self.contact {
            // 五列任一命中即可：地址三个列 + 收件人 / 抄送 JSON。**同一模式绑五次**，
            // 与 `from:` 的三连绑同一条「顺序即契约」纪律。
            parts.push(
                "(m.from_addr LIKE ? ESCAPE '\\' OR m.from_name LIKE ? ESCAPE '\\' \
                 OR m.sender_addr LIKE ? ESCAPE '\\' OR m.to_json LIKE ? ESCAPE '\\' \
                 OR m.cc_json LIKE ? ESCAPE '\\')"
                    .to_owned(),
            );
            let pattern = Value::Text(like_pattern(value));
            values.push(pattern.clone());
            values.push(pattern.clone());
            values.push(pattern.clone());
            values.push(pattern.clone());
            values.push(pattern);
        }
        for value in &self.subject {
            parts.push("m.subject LIKE ? ESCAPE '\\'".to_owned());
            values.push(Value::Text(like_pattern(value)));
        }
        if let Some(folder) = &self.folder {
            // `imap_name` 是服务端原名（`Archive/2024`），`role` 是识别出的角色
            // （`inbox` / `sent` / …）。两个都试：`folder:Archive` 与 `folder:inbox`
            // 都该能用。`role` 加 `COLLATE NOCASE` 是因为它是我们写进去的 ASCII 常量，
            // 大小写不该敏感；`imap_name` 走 LIKE（ASCII 下 LIKE 本就不敏感），
            // 但不加 NOCASE——服务端文件夹名的大小写是它的语义，不该被我们折叠。
            parts.push(
                "m.folder_id IN (SELECT id FROM folders \
                 WHERE imap_name LIKE ? ESCAPE '\\' OR role = ? COLLATE NOCASE)"
                    .to_owned(),
            );
            values.push(Value::Text(like_pattern(folder)));
            values.push(Value::Text(folder.clone()));
        }
        if let Some(before) = self.before {
            parts.push("m.date < ?".to_owned());
            values.push(Value::Integer(before));
        }
        if let Some(after) = self.after {
            parts.push("m.date >= ?".to_owned());
            values.push(Value::Integer(after));
        }
    }
}

/// 解析用户的原始输入。
///
/// **不报错**：任何不认识的写法都会退回成自由文本（见模块头第 1 条细节）。
pub fn parse_query(raw: &str) -> Query {
    let mut query = Query::default();
    for token in scan(raw) {
        classify(token, &mut query);
    }
    query
}

/// 把自由文本词编译成 FTS5 的 `MATCH` 表达式；没有词时返回空串。
///
/// 每个词用双引号括起来（内部引号翻倍），词之间是**隐含 AND**。这样 `"`、`*`、`(`、
/// `:`、`-` 等 FTS5 元字符都变成字面量，用户输错语法也只会「搜不到」，不会让 `MATCH`
/// 报错。唯一的例外是**纯字母数字 / 汉字**上的尾随 `*`（前缀查询，见
/// [`safe_prefix`]）。
pub fn quote_tokens(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|token| match safe_prefix(token) {
            Some(prefix) => format!("\"{}\"*", prefix.replace('"', "\"\"")),
            None => format!("\"{}\"", token.replace('"', "\"\"")),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 检索，返回命中的 `messages.id`。
///
/// 有自由文本词时按 FTS5 相关度（`rank`）排序，只有条件时按时间倒序——否则
/// 「筛出所有未读」会按相关度（全是并列）排出一个看起来随机的顺序。
///
/// `raw` 是用户输入的原始查询串；内部转成安全的表达式与参数绑定——**绝不把用户输入
/// 直接拼进 SQL**。
pub fn search(conn: &Connection, raw: &str, limit: i64) -> Result<Vec<i64>> {
    let query = parse_query(raw);
    let expr = quote_tokens(&query.text);
    let joined = !expr.is_empty();
    if !joined && query.filters.is_empty() {
        // 空查询：不查。见模块头第 3 条细节。
        return Ok(Vec::new());
    }

    // ⚠️ 基谓词 `m.is_draft = 0` 是**必需**的，不是优化：本地草稿的 `folder_id` 是 NULL，
    // 若被捞出来，`search_messages` 回填时走到 `row_to_summary` 的 `folder_id: i64`
    // 会直接 `InvalidColumnType`（不是「显示成 0」，是报错）。
    // 它没有 `?` 占位符，所以不会打乱 `parts` 与 `values` 的位置对应。
    let mut parts: Vec<String> = vec!["m.is_draft = 0".to_owned()];
    let mut values: Vec<Value> = Vec::new();
    query.filters.append_to(&mut parts, &mut values);
    if joined {
        // MATCH 放在**最后**一条：`?` 是位置绑定的，这样它就是 `values` 的最后一个
        // 过滤值，不必在中间插队。
        parts.push("messages_fts MATCH ?".to_owned());
        values.push(Value::Text(expr));
    }

    let sql = format!(
        "SELECT m.id FROM messages m{join}\n WHERE {where_clause}\n ORDER BY {order}\n LIMIT ?",
        join = if joined {
            " JOIN messages_fts ON messages_fts.rowid = m.id"
        } else {
            ""
        },
        where_clause = parts.join(" AND "),
        order = if joined {
            "messages_fts.rank"
        } else {
            "m.date DESC, m.id DESC"
        },
    );
    // 负数 `LIMIT` 在 SQLite 里是「不限制」，会把整库捞出来 —— 夹到 0。
    values.push(Value::Integer(limit.max(0)));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| row.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 词法
// ---------------------------------------------------------------------------

/// 扫描出的一个原始词。
struct RawToken {
    /// `key:value` 里的 key。没有冒号（或冒号紧跟在引号前）时为 `None`。
    key: Option<String>,
    /// 值。引号已剥掉，所以 `"两个词"` 的值是 `两个词`。
    value: String,
    /// **原始文本**（含 key 与引号）。认不出 key 时按它退回自由文本 ——
    /// 拿 `value` 拼会把 `http://a` 变成 `//a`。
    raw: String,
}

/// 按空白切词，带**最小**的引号语义。
///
/// 引号只在「值的开头」有特殊含义：`subject:"a b"` 是一个词，`a"b` 是字面量 `a"b`。
/// 这样处理是为了让**没写完整的引号**（`from:"abc`）也能得到一个可用的值，而不是
/// 让整条查询变成语法错误。
fn scan(raw: &str) -> Vec<RawToken> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;

        // 先看有没有 `key:` —— 冒号必须出现在任何空白或引号**之前**，且 key 非空。
        // `subject:"a b"` 的 key 是 `subject`；`a"b:` 不是（引号挡住了），key 为 `None`。
        // **全角冒号 `：` 同样算分隔**：中文输入法下冒号几乎必然打成全角，`from：张三`
        // 若不认，表现就是「过滤没生效」，而用户根本看不出差别在哪。
        let mut colon = i;
        while colon < chars.len()
            && !chars[colon].is_whitespace()
            && chars[colon] != ':'
            && chars[colon] != '：'
            && chars[colon] != '"'
        {
            colon += 1;
        }
        let key = if colon > i && colon < chars.len() && matches!(chars[colon], ':' | '：') {
            // key 统一小写后交给 `classify`，这样 `FROM:` 与 `from:` 一样能被认出来。
            // **取值不折叠大小写** —— `from:Alice` 的 `A` 是地址的一部分。
            i = colon + 1; // 跳过冒号
            let typed = chars[start..colon]
                .iter()
                .collect::<String>()
                .to_lowercase();
            Some(canonical_key(&typed).to_owned())
        } else {
            None
        };

        // 值。引号只在**值的开头**有含义：
        // - 开引号后一直吃到闭引号（含空白），所以 `subject:"a b"` 是一个词；
        // - 没闭合（`from:"abc`）就吃到词尾 —— 给一个能用的值，而不是让整条查询报语法错；
        // - 闭引号后粘着的字符继续算进值里（`subject:"a"b` → `ab`）。
        let mut value = String::new();
        if i < chars.len() && chars[i] == '"' {
            i += 1; // 吃掉开引号
            while i < chars.len() && chars[i] != '"' {
                value.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1; // 吃掉闭引号（没闭合时这里已经是词尾）
            }
        }
        // 没有引号、或引号之后剩下的部分：按空白断词。**不把 `"` 当断点** ——
        // 值中间的引号是字面量（`a"b` 是一个词）。
        while i < chars.len() && !chars[i].is_whitespace() {
            value.push(chars[i]);
            i += 1;
        }

        out.push(RawToken {
            key,
            value,
            raw: chars[start..i].iter().collect(),
        });
    }
    out
}

/// 把用户敲的 key 归一成词法里的标准写法（小写之后跑）。
///
/// 中文写法是**输入侧**的字面量（匹配用户的自然输入），不是输出文案——内核照旧
/// 不吐中文。归一只做同义词收敛：`发件人:张三` 与 `from:张三` 走同一条 `match` 臂，
/// 后面就不必两处维护。
fn canonical_key(key: &str) -> &str {
    match key {
        "发件人" | "发件" => "from",
        "收件人" | "收件者" | "收件" => "to",
        "主题" | "标题" => "subject",
        "文件夹" => "folder",
        "早于" | "之前" => "before",
        "晚于" | "之后" | "自" => "after",
        other => other,
    }
}

/// 宽松判定「像邮箱地址」：恰好一个 `@`、两端非空、不含空白。
///
/// 只在**裸词**（没有 `key:`）上用：搜一个地址时用户想的是「这个人的往来邮件」，
/// 不该要求他想清楚该写 `from:` 还是 `to:`。误判（比如把长得像地址的词认进来）的
/// 代价只是从全文换成按联系人列匹配，不会丢结果。
fn looks_like_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && !value.contains(char::is_whitespace)
}

/// 把一个原始词归到「条件」或「自由文本」。
///
/// 认不出的 key、认不出的取值、解析不出的日期，**一律退回 `raw` 原文**（模块头第 1 条）：
/// 用户能在结果里看见「这条没被当成条件」，比被静默丢掉（`is:whatever`）或只留半截
/// （`http://example.com/a` → `//example.com/a`）都好。
fn classify(token: RawToken, query: &mut Query) {
    let RawToken { key, value, raw } = token;

    // 没有 key：自由文本。空值（比如输入了一个 `""`）丢掉 —— 它进 `MATCH` 也只是个空短语。
    // 例外：长得像邮箱地址的裸词按「联系人」匹配（发件**或**收件任一命中）。
    let Some(key) = key else {
        if value.is_empty() {
            return;
        }
        if looks_like_email(&value) {
            query.filters.contact.push(value);
        } else {
            query.text.push(value);
        }
        return;
    };
    // `from:` 这种只有 key 没有值的写法**当没写**：不是条件，也不该变成文本
    // —— 变成文本就成了一个必然搜不到的词，会把整条查询拖成 0 命中。
    if value.is_empty() {
        return;
    }

    match key.as_str() {
        "from" => query.filters.from.push(value),
        "to" => query.filters.to.push(value),
        "subject" => query.filters.subject.push(value),
        // 只留最后一条：一封邮件只属于一个文件夹，累加得到的是必然为空的交集。
        "folder" => query.filters.folder = Some(value),
        "is" => match value.to_lowercase().as_str() {
            "unread" | "未读" => query.filters.unread = Some(true),
            "read" | "已读" => query.filters.unread = Some(false),
            "starred" | "flagged" | "有旗标" | "已加旗标" | "已加星" | "有星标" => {
                query.filters.starred = Some(true);
            }
            "unstarred" | "无旗标" | "没旗标" | "未加旗标" | "无星标" => {
                query.filters.starred = Some(false);
            }
            _ => query.text.push(raw),
        },
        "has" => match value.to_lowercase().as_str() {
            "attachment" | "attachments" | "附件" | "有附件" => {
                query.filters.has_attachment = Some(true);
            }
            _ => query.text.push(raw),
        },
        "before" | "older" => match parse_day_rel(&value, now_secs()) {
            Some(day) => query.filters.before = Some(day),
            None => query.text.push(raw),
        },
        "after" | "newer" => match parse_day_rel(&value, now_secs()) {
            Some(day) => query.filters.after = Some(day),
            None => query.text.push(raw),
        },
        _ => query.text.push(raw),
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 词尾 `*` 的前缀查询；不是安全前缀时返回 `None`（退回字面量查询）。
///
/// 只放行**纯字母数字 / 汉字**：`a-b*`、`x:*`、`(a)*` 这类里带 FTS5 元字符的写法
/// 一律当字面量，否则用户敲一个 `*` 就把 `MATCH` 变成语法错误。
fn safe_prefix(token: &str) -> Option<&str> {
    let prefix = token.strip_suffix('*')?;
    (!prefix.is_empty() && prefix.chars().all(char::is_alphanumeric)).then_some(prefix)
}

/// 把用户输入变成 `LIKE` 模式：`%` / `_` / `\` 转义，两端补 `%`。
fn like_pattern(value: &str) -> String {
    let mut pattern = String::with_capacity(value.len() + 2);
    pattern.push('%');
    for ch in value.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// `YYYY-M-D`（月 / 日允许一位数）→ 当天 00:00:00 **UTC** 的 Unix 秒。
///
/// 用 UTC 而不是本地时区：库里 `messages.date` 是服务端给的 UTC 秒，
/// 用本地时区换算会让「同一句话在不同机器上筛出不同结果」。
fn parse_day(text: &str) -> Option<i64> {
    let mut parts = text.split('-');
    let year: i64 = parts.next()?.trim().parse().ok()?;
    let month: i64 = parts.next()?.trim().parse().ok()?;
    let day: i64 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400)
}

/// 「现在」的 Unix 秒。
///
/// 与 `compose.rs::now_secs` 刻意重复一份而不是共用（同 [`days_from_civil`] 的理由）。
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 日期值解析：先试绝对日期（`YYYY-M-D`），再试相对词。
///
/// 相对词与绝对日期一样落到**某天 00:00:00 UTC** 的边界上，`before:` / `after:`
/// 的「严格早于 / 不早于」语义原样适用：`after:昨天` 含昨天全天，`before:昨天`
/// 是昨天零点之前。
fn parse_day_rel(text: &str, now: i64) -> Option<i64> {
    parse_day(text).or_else(|| parse_relative_day(text, now))
}

/// 相对日期词 → 某天 00:00:00 UTC。
///
/// 认：`今天 / 昨天 / 前天 / 本周 / 上周 / 本月 / 上月（上个月） / 最近N天 / 近N天 / N天`
/// 及英文紧凑别名（`today / yesterday / thisweek / lastweek / thismonth / lastmonth`）。
/// 「本周」按**周一为一周之始**；「最近N天」含今天、往回数 N 天。
fn parse_relative_day(text: &str, now: i64) -> Option<i64> {
    let days = now.div_euclid(86_400);
    // 1970-01-01 是周四：`(days + 3) % 7` 把周一映射到 0。
    let weekday = (days + 3).rem_euclid(7);
    let today = days * 86_400;
    match text.trim().to_lowercase().as_str() {
        "今天" | "today" => Some(today),
        "昨天" | "yesterday" => Some(today - 86_400),
        "前天" => Some(today - 2 * 86_400),
        "本周" | "thisweek" | "this_week" => Some(today - weekday * 86_400),
        "上周" | "lastweek" | "last_week" => Some(today - (weekday + 7) * 86_400),
        "本月" | "thismonth" | "this_month" => Some(month_start(days, 0)),
        "上月" | "上个月" | "lastmonth" | "last_month" => Some(month_start(days, 1)),
        other => {
            // 最近N天 / 近N天 / N天：N 天窗口的**起点**（含今天往回数 N 天）。
            let stripped = other
                .strip_prefix("最近")
                .or_else(|| other.strip_prefix("近"))
                .unwrap_or(other);
            let digits = stripped
                .strip_suffix("天内")
                .or_else(|| stripped.strip_suffix("天"))
                .or_else(|| stripped.strip_suffix("日"))
                .unwrap_or(stripped);
            let n: i64 = digits.parse().ok()?;
            (n > 0).then_some(today - (n - 1) * 86_400)
        }
    }
}

/// `days`（自纪元起的天数）所在月份往前 `months_back` 个月的 1 号 00:00:00 UTC。
fn month_start(days: i64, months_back: i64) -> i64 {
    let (year, month, _) = civil_from_days(days);
    let total = year * 12 + (month - 1) - months_back;
    days_from_civil(total.div_euclid(12), total.rem_euclid(12) + 1, 1) * 86_400
}

/// Howard Hinnant 的 `civil_from_days`（公有领域）。
///
/// [`days_from_civil`] 的反函数，给「本月 / 上月」找月初用。同样刻意重复一份：
/// 另一份在 `compose.rs`（协议层），不为十几行算术把存储检索拽去依赖 compose。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Howard Hinnant 的 `days_from_civil`（公有领域；`std::chrono` 的实现同源）。
///
/// 是 `compose.rs::civil_from_days` 的反函数。**刻意重复一份**而不是共用：那一份在
/// `compose`（协议 / 头部装配层），这一份在存储层的检索里，把两者拽到一起就得为一个
/// 十几行的纯算术函数新开模块，还会让检索依赖 compose 的公开面（同 `compose.rs` 里
/// 重复五行 `now_secs` 的处理）。
///
/// 调用方保证 `month` ∈ 1–12、`day` ∈ 1–31；本函数**不校验当月天数**（与原文一致，
/// `2 月 31 日` 会算成 3 月 2 日 —— 对这个用途无害）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    // 负数年份按「400 年一轮」向下取整（Rust 的 `/` 与 C++ 一样朝零截断，
    // 所以这里必须手动减 399，见原文的 `(y >= 0 ? y : y-399) / 400`）。
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (month + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::repo::{accounts, folders, messages};
    use crate::models::{ConnectionSecurity, FolderRole, NewAccount};

    /// 建一个内存库：一个账户 + `INBOX`（收件箱，含 j 的邮件）+ `Archive/2024`。
    ///
    /// 返回 `(conn, 账户, 收件箱, 归档)` 四个句柄 —— 过滤器测试要往不同文件夹塞邮件。
    fn mem() -> (Connection, i64, i64, i64) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations::run(&mut conn).unwrap();
        let account = accounts::insert(
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
        let folder = |name: &str, role: FolderRole| {
            folders::upsert(
                &conn,
                account,
                &folders::FolderUpsert {
                    name: name.into(),
                    delimiter: Some("/".into()),
                    role,
                    uidvalidity: Some(1),
                    uidnext: Some(100),
                },
            )
            .unwrap()
        };
        let inbox = folder("INBOX", FolderRole::Inbox);
        let archive = folder("Archive/2024", FolderRole::Other);
        (conn, account, inbox, archive)
    }

    /// 落一封邮件并同时建索引（`search` 只读索引，不建就没有命中）。
    fn seed(
        conn: &Connection,
        account: i64,
        folder: i64,
        uid: i64,
        subject: &str,
        from_addr: &str,
        date: i64,
    ) -> i64 {
        let id = messages::upsert_envelope(
            conn,
            account,
            folder,
            &messages::EnvelopeRow {
                uid,
                message_id: Some(format!("<{uid}@example.com>")),
                subject: subject.into(),
                from_name: Some("张三".into()),
                from_addr: Some(from_addr.into()),
                to: vec!["me@example.com".into()],
                cc: vec![],
                date,
                is_read: false,
                is_starred: false,
                has_attachments: false,
                size: Some(1024),
                snippet: "摘要".into(),
            },
        )
        .unwrap();
        index_message(conn, id, subject, from_addr, "me@example.com", "正文").unwrap();
        id
    }

    // ---- 词法 ----

    #[test]
    fn bare_words_are_quoted_and_anded() {
        assert_eq!(
            quote_tokens(&["hello".into(), "world".into()]),
            "\"hello\" \"world\""
        );
        // 元字符被引号吃掉，不会变成语法。
        assert_eq!(quote_tokens(&["a*b".into()]), "\"a*b\"");
        assert_eq!(quote_tokens(&["x:y".into()]), "\"x:y\"");
        assert_eq!(quote_tokens(&["(".into()]), "\"(\"");
        // 内部引号翻倍。
        assert_eq!(quote_tokens(&["say \"hi\"".into()]), "\"say \"\"hi\"\"\"");
        assert_eq!(quote_tokens(&[]), "");
    }

    #[test]
    fn trailing_star_is_a_prefix_query_only_on_safe_words() {
        assert_eq!(quote_tokens(&["report*".into()]), "\"report\"*");
        // 汉字也算「字母数字」（`char::is_alphanumeric`），前缀查询对中文同样开。
        assert_eq!(quote_tokens(&["报告*".into()]), "\"报告\"*");
        // 只有一个 `*`：剥掉后是空串，退回字面量。
        assert_eq!(quote_tokens(&["*".into()]), "\"*\"");
        // 带元字符的前缀不放行 —— 放行了用户敲 `a-b*` 就能让 `MATCH` 报语法错。
        assert_eq!(quote_tokens(&["a-b*".into()]), "\"a-b*\"");
        assert_eq!(quote_tokens(&["a b*".into()]), "\"a b*\"");
        assert_eq!(quote_tokens(&["x:*".into()]), "\"x:*\"");
    }

    #[test]
    fn quotes_make_a_phrase_and_keys_are_extracted() {
        let query = parse_query("subject:\"quarterly report\" from:alice report");
        assert_eq!(query.text, ["report"]);
        assert_eq!(query.filters.subject, ["quarterly report"]);
        assert_eq!(query.filters.from, ["alice"]);
    }

    #[test]
    fn quoted_phrase_without_key_stays_one_token() {
        // 旧实现按空白切，`"两个词"` 会变成 `"` 与 `词"` 两个碎片；现在是一个短语。
        let query = parse_query("\"两个 词\" 别的");
        assert_eq!(query.text, ["两个 词", "别的"]);
        assert!(query.filters.is_empty());
    }

    #[test]
    fn unknown_key_falls_back_to_the_original_text() {
        // URL：冒号后面不是已知 key，必须整条当文本，不能吃成 `//example.com`。
        let query = parse_query("http://example.com/a");
        assert_eq!(query.text, ["http://example.com/a"]);
        assert!(query.filters.is_empty());

        // 认得的 key、认不得的取值：同样退回原文。
        let query = parse_query("is:whatever");
        assert_eq!(query.text, ["is:whatever"]);

        // 日期写错：退回原文（用户能看见自己写错了，而不是「筛了但筛不到」）。
        let query = parse_query("before:2026-13-40");
        assert_eq!(query.text, ["before:2026-13-40"]);
        assert_eq!(query.filters.before, None);
    }

    #[test]
    fn boolean_and_date_keys_map_to_filters() {
        let query = parse_query("is:unread is:starred has:attachment before:2026-01-01");
        assert_eq!(query.filters.unread, Some(true));
        assert_eq!(query.filters.starred, Some(true));
        assert_eq!(query.filters.has_attachment, Some(true));
        assert_eq!(query.filters.before, parse_day("2026-01-01"));
        assert!(query.text.is_empty());

        // 反档与别名。
        let query = parse_query("is:read is:flagged older:2026-01-01");
        assert_eq!(query.filters.unread, Some(false));
        assert_eq!(query.filters.starred, Some(true));
        assert_eq!(query.filters.before, parse_day("2026-01-01"));

        let query = parse_query("is:unstarred newer:2026-01-01");
        assert_eq!(query.filters.starred, Some(false));
        assert_eq!(query.filters.after, parse_day("2026-01-01"));

        // key 大小写不敏感，取值也是。
        let query = parse_query("FROM:Alice IS:UNREAD");
        assert_eq!(query.filters.from, ["Alice"]);
        assert_eq!(query.filters.unread, Some(true));
    }

    // ---- 智能化别名（中文 / 全角冒号 / 相对日期 / 裸邮箱）----

    #[test]
    fn chinese_keys_are_canonicalised() {
        let query = parse_query("发件人:张三 收件者:li@example.com 主题:报告 文件夹:INBOX");
        assert_eq!(query.filters.from, ["张三"]);
        assert_eq!(query.filters.to, ["li@example.com"]);
        assert_eq!(query.filters.subject, ["报告"]);
        assert_eq!(query.filters.folder.as_deref(), Some("INBOX"));
        assert!(query.text.is_empty());

        // 简写与日期别名的两个方向。
        let query = parse_query("发件:张三 早于:2026-01-01 晚于:2026-01-02");
        assert_eq!(query.filters.from, ["张三"]);
        assert_eq!(query.filters.before, parse_day("2026-01-01"));
        assert_eq!(query.filters.after, parse_day("2026-01-02"));
    }

    #[test]
    fn fullwidth_colon_is_a_separator_too() {
        // 中文输入法下冒号几乎必然打成全角：认不出就是「过滤没生效」。
        let query = parse_query("from：张三 发件人：李四 is：未读");
        assert_eq!(query.filters.from, ["张三", "李四"]);
        assert_eq!(query.filters.unread, Some(true));
    }

    #[test]
    fn chinese_values_map_to_booleans() {
        let query = parse_query("is:未读 is:已读");
        assert_eq!(query.filters.unread, Some(false)); // 后写覆盖（与英文同语义）

        let query = parse_query("is:有旗标 has:附件");
        assert_eq!(query.filters.starred, Some(true));
        assert_eq!(query.filters.has_attachment, Some(true));

        let query = parse_query("is:无旗标");
        assert_eq!(query.filters.starred, Some(false));

        // 认不得的中文值照旧退回原文，不静默丢。
        let query = parse_query("is:红标的");
        assert_eq!(query.text, ["is:红标的"]);
    }

    #[test]
    fn relative_days_resolve_to_utc_day_bounds() {
        // 锚定一个已知时刻：2026-10-13 12:00:00 UTC（周二）。
        let now = parse_day("2026-10-13").unwrap() + 12 * 3600;
        let day = |offset: i64| Some(parse_day("2026-10-13").unwrap() + offset * 86_400);

        assert_eq!(parse_relative_day("今天", now), day(0));
        assert_eq!(parse_relative_day("today", now), day(0));
        assert_eq!(parse_relative_day("昨天", now), day(-1));
        assert_eq!(parse_relative_day("前天", now), day(-2));
        // 周二：本周一 = -1 天，上周一 = -8 天。
        assert_eq!(parse_relative_day("本周", now), day(-1));
        assert_eq!(parse_relative_day("上周", now), day(-8));
        // 本月 1 号与上月 1 号。
        assert_eq!(parse_relative_day("本月", now), parse_day("2026-10-01"));
        assert_eq!(parse_relative_day("上个月", now), parse_day("2026-09-01"));
        // 最近N天：起点含今天往回数 N 天（最近 1 天 = 今天）。
        assert_eq!(parse_relative_day("最近7天", now), day(-6));
        assert_eq!(parse_relative_day("近30天", now), day(-29));
        assert_eq!(parse_relative_day("7天", now), day(-6));

        // 边界口径与绝对日期一致：都是 00:00:00 UTC（绝对日期由 parse_day_rel 先接住）。
        assert_eq!(parse_day_rel("2026-10-13", now), parse_day("2026-10-13"));
        // 认不出 → None（调用方退回原文）。
        assert_eq!(parse_relative_day("上周的某天", now), None);
        assert_eq!(parse_relative_day("最近零天", now), None);
    }

    #[test]
    fn relative_days_reach_the_filters() {
        let query = parse_query("after:上周 is:未读");
        assert!(query.filters.after.is_some());
        assert_eq!(query.filters.unread, Some(true));
        assert!(query.text.is_empty(), "认得出的相对日期不该退回文本");

        // 写不出解析的照旧退回原文。
        let query = parse_query("after:大概上周");
        assert_eq!(query.text, ["after:大概上周"]);
    }

    #[test]
    fn bare_email_becomes_a_contact_filter() {
        let query = parse_query("zhang@corp.com 报告");
        assert_eq!(query.filters.contact, ["zhang@corp.com"]);
        assert_eq!(query.text, ["报告"]);

        // 带 key 的照旧走各自的列，不进 contact。
        let query = parse_query("from:zhang@corp.com");
        assert!(query.filters.contact.is_empty());
        assert_eq!(query.filters.from, ["zhang@corp.com"]);

        // 不是邮箱的裸词（多个 @、空段、URL）仍是普通文本。
        let query = parse_query("a@b@c http://example.com");
        assert!(query.filters.contact.is_empty());
        assert_eq!(query.text, ["a@b@c", "http://example.com"]);
    }

    #[test]
    fn contact_filter_hits_sender_and_recipients() {
        let (conn, account, inbox, _archive) = mem();
        seed(
            &conn,
            account,
            inbox,
            1,
            "周报",
            "zhang@corp.com",
            1_800_000_000,
        );
        // 第二封：zhang 是**收件人**（seed 的 to 固定是 me，这里要自定义插入）。
        // 第三封：别人发的，但 zhang 在抄送里。发件 / 收件 / 抄送三种命中各一。
        let rows = [
            (
                2,
                "other@corp.com",
                vec!["zhang@corp.com".into()],
                vec![],
                1_800_000_100,
            ),
            (
                3,
                "other@corp.com",
                vec!["me@example.com".into()],
                vec!["zhang@corp.com".into()],
                1_800_000_200,
            ),
        ];
        for (uid, from_addr, to, cc, date) in rows {
            let id = messages::upsert_envelope(
                &conn,
                account,
                inbox,
                &messages::EnvelopeRow {
                    uid,
                    message_id: Some(format!("<{uid}@example.com>")),
                    subject: "周报".into(),
                    from_name: Some("张三".into()),
                    from_addr: Some(from_addr.into()),
                    to,
                    cc,
                    date,
                    is_read: false,
                    is_starred: false,
                    has_attachments: false,
                    size: Some(1024),
                    snippet: "摘要".into(),
                },
            )
            .unwrap();
            index_message(&conn, id, "周报", from_addr, "me@example.com", "正文").unwrap();
        }

        // 发件命中、收件命中、抄送命中，三封都该出来；无关地址不出。
        let hits = search(&conn, "zhang@corp.com", 10).unwrap();
        assert_eq!(hits.len(), 3, "裸邮箱应命中发件 / 收件 / 抄送三种命中");
        assert!(search(&conn, "nobody@corp.com", 10).unwrap().is_empty());
    }

    #[test]
    fn repeated_filters_accumulate_except_folder() {
        let query = parse_query("from:a from:b folder:x folder:y subject:s subject:t");
        assert_eq!(query.filters.from, ["a", "b"]);
        assert_eq!(query.filters.subject, ["s", "t"]);
        // 一封邮件只在一个文件夹里，累加会得到必然为空的交集 —— 只留最后一条。
        assert_eq!(query.filters.folder.as_deref(), Some("y"));
    }

    #[test]
    fn empty_and_degenerate_input_is_dropped() {
        assert_eq!(parse_query("   "), Query::default());
        // 只有 key 没有值：当没写（不是条件，也不是文本）。
        assert_eq!(parse_query("from:"), Query::default());
        assert_eq!(parse_query("from:   to:  "), Query::default());
        // 没写完的引号：值就是剩下的内容，不报错也不吞掉。
        let query = parse_query("from:\"abc");
        assert_eq!(query.filters.from, ["abc"]);
        // 值后面粘了东西：一并算进值里，不产生第二个词。
        let query = parse_query("subject:\"a\"b");
        assert_eq!(query.filters.subject, ["ab"]);
    }

    // ---- 日期 ----

    #[test]
    fn days_from_civil_inverts_known_days() {
        // 期望值全部由 Python 的 `datetime` 算出（外部权威实现），不是手推的。
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1900, 1, 1), -25_567);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2024, 1, 1), 19_723);
        // 闰日：2024 是闰年，2 月 29 日存在。
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(days_from_civil(2025, 1, 1), 20_089);
        // 2100 不是闰年（百年不闰、四百年再闰），3 月 1 日因此比平年晚一天。
        assert_eq!(days_from_civil(2100, 3, 1), 47_541);
        assert_eq!(days_from_civil(2026, 10, 13), 20_739);
    }

    #[test]
    fn parse_day_accepts_one_and_two_digit_parts() {
        // 1 月 1 日 = 0 秒；UTC 日界，不随时区漂。
        assert_eq!(parse_day("1970-01-01"), Some(0));
        assert_eq!(parse_day("1970-1-1"), Some(0));
        assert_eq!(parse_day("2024-02-29"), Some(19_782 * 86_400));
        assert_eq!(parse_day("2026-10-13"), Some(20_739 * 86_400));
        // 前后空白容忍（用户从别处粘过来的）。
        assert_eq!(parse_day(" 2026-10-13 "), Some(20_739 * 86_400));

        assert_eq!(parse_day(""), None);
        assert_eq!(parse_day("2026"), None);
        assert_eq!(parse_day("2026-10"), None);
        assert_eq!(parse_day("2026-10-13-01"), None);
        assert_eq!(parse_day("2026-13-01"), None);
        assert_eq!(parse_day("2026-00-01"), None);
        assert_eq!(parse_day("2026-10-00"), None);
        assert_eq!(parse_day("2026-10-32"), None);
        assert_eq!(parse_day("abc-de-fg"), None);
        assert_eq!(parse_day("0-01-01"), None);
    }

    // ---- 转义 ----

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("alice"), "%alice%");
        // 不转义的话 `from:%` 会匹配全部邮件 —— 看起来像「条件没生效」。
        assert_eq!(like_pattern("%"), "%\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        // 反斜杠自身也要转义，否则用户输入的反斜杠会吃掉下一个字符。
        assert_eq!(like_pattern("a\\b"), "%a\\\\b%");
        assert_eq!(like_pattern(""), "%%");
    }

    // ---- 端到端 ----

    #[test]
    fn index_then_search_roundtrip() {
        let (conn, acct, inbox, _) = mem();
        seed(
            &conn,
            acct,
            inbox,
            1,
            "Quarterly report",
            "alice@example.com",
            100,
        );
        seed(&conn, acct, inbox, 2, "Lunch", "bob@example.com", 200);

        assert_eq!(search(&conn, "report", 10).unwrap(), vec![1]);
        // 写 `is_empty()` 而不是 `vec![]`：`serde_json` 也给 `i64` 实现了 `PartialEq`，
        // 空 `Vec` 字面量会让类型推断歧义（E0283）。
        assert!(search(&conn, "free", 10).unwrap().is_empty());
        // 跨列命中（发件人）。
        assert_eq!(search(&conn, "alice@example.com", 10).unwrap(), vec![1]);
    }

    #[test]
    fn filters_narrow_the_hits() {
        let (conn, acct, inbox, archive) = mem();
        let read = seed(&conn, acct, inbox, 1, "报告", "alice@example.com", 1_000);
        messages::set_read(&conn, read, true).unwrap();
        let starred = seed(&conn, acct, inbox, 2, "报告", "bob@example.com", 2_000);
        messages::set_starred(&conn, starred, true).unwrap();
        let attachment = seed(&conn, acct, inbox, 3, "报告", "carol@example.com", 3_000);
        messages::set_has_attachments(&conn, attachment, true).unwrap();
        let archived = seed(&conn, acct, archive, 1, "报告", "dave@example.com", 4_000);

        // 只有条件、没有文本：不能退化成「全表」（那就是 4 条都出来了）。
        assert_eq!(search(&conn, "is:unread", 10).unwrap().len(), 3);
        assert_eq!(search(&conn, "is:read", 10).unwrap(), vec![read]);
        assert_eq!(search(&conn, "is:starred", 10).unwrap(), vec![starred]);
        assert_eq!(
            search(&conn, "has:attachment", 10).unwrap(),
            vec![attachment]
        );
        assert_eq!(search(&conn, "from:bob", 10).unwrap(), vec![starred]);
        // 文件夹：`imap_name` 全名与 `role` 都能认。
        assert_eq!(
            search(&conn, "folder:Archive/2024", 10).unwrap(),
            vec![archived]
        );
        assert_eq!(search(&conn, "folder:inbox", 10).unwrap().len(), 3);
        assert_eq!(search(&conn, "folder:收件箱", 10).unwrap().len(), 0);

        // 日期：`before` 严格早于、`after` 含当日。
        assert_eq!(search(&conn, "after:1970-01-01", 10).unwrap().len(), 4);
        assert_eq!(search(&conn, "before:1970-01-01", 10).unwrap().len(), 0);

        // 文本 + 条件：两个都要满足。
        assert_eq!(search(&conn, "报告 is:starred", 10).unwrap(), vec![starred]);
        assert!(search(&conn, "午餐 is:starred", 10).unwrap().is_empty());
    }

    #[test]
    fn a_wildcard_in_a_filter_is_a_literal_not_a_wildcard() {
        let (conn, acct, inbox, _) = mem();
        seed(&conn, acct, inbox, 1, "a", "alice@example.com", 1);
        seed(&conn, acct, inbox, 2, "b", "bob@example.com", 2);
        // 不转义 `%` 的话这里会命中两封 —— 用户会以为过滤没生效。
        assert!(search(&conn, "from:%", 10).unwrap().is_empty());
        // `_` 也是字面量：`a_ice` 不该匹配 `alice`。
        assert!(search(&conn, "from:a_ice", 10).unwrap().is_empty());
    }

    #[test]
    fn reindex_replaces_old_content() {
        let (conn, acct, inbox, _) = mem();
        // 必须走 `seed` 落一行 `messages` —— `search` 是 `JOIN messages` 的，
        // 只往虚表塞 rowid 的话两边都查不到，测试会「通过」但什么都没验证。
        let id = seed(&conn, acct, inbox, 1, "old subject", "x@example.com", 1_000);
        assert_eq!(search(&conn, "old", 10).unwrap(), vec![id]);

        // 同一封邮件重新索引（改标题 / 正文更新）后，旧词不该还搜得到。
        index_message(
            &conn,
            id,
            "new subject",
            "x@example.com",
            "me@example.com",
            "new body",
        )
        .unwrap();
        assert!(search(&conn, "old", 10).unwrap().is_empty());
        assert_eq!(search(&conn, "new", 10).unwrap(), vec![id]);
    }

    #[test]
    fn delete_removes_from_index() {
        let (conn, acct, inbox, _) = mem();
        let id = seed(&conn, acct, inbox, 1, "target", "x@example.com", 1_000);
        assert_eq!(search(&conn, "target", 10).unwrap(), vec![id]);

        remove_message(&conn, id).unwrap();
        assert!(search(&conn, "target", 10).unwrap().is_empty());
    }

    #[test]
    fn malformed_user_input_never_errors() {
        let conn = mem().0;
        index_message(&conn, 1, "hello", "", "", "world").unwrap();
        // 各种会让 FTS5 原生语法炸掉的输入，经转义后都该安静返回，不 panic 不报错。
        for raw in [
            "\"unclosed",
            "AND OR NOT",
            "col:",
            "(((",
            "a - b",
            "*",
            "* *",
            "NEAR(a b)",
            "a:b:c",
            "from:",
            "is:",
            "before:",
            "\"\"",
            "^^^",
            "-a",
        ] {
            assert!(
                search(&conn, raw, 10).is_ok(),
                "query {raw:?} should not error"
            );
        }
    }

    #[test]
    fn limit_is_respected_and_negative_limits_are_clamped() {
        let (conn, acct, inbox, _) = mem();
        for uid in 1..=10 {
            seed(
                &conn,
                acct,
                inbox,
                uid,
                "common",
                "x@example.com",
                1_000 + uid,
            );
        }
        assert_eq!(search(&conn, "common", 3).unwrap().len(), 3);
        // 负数 `LIMIT` 在 SQLite 里是「不限制」，会被夹到 0。
        assert!(search(&conn, "common", -1).unwrap().is_empty());
        assert!(search(&conn, "common", 0).unwrap().is_empty());
    }

    #[test]
    fn filter_only_results_come_back_newest_first() {
        let (conn, acct, inbox, _) = mem();
        let older = seed(&conn, acct, inbox, 1, "旧", "x@example.com", 1_000);
        let newer = seed(&conn, acct, inbox, 2, "新", "x@example.com", 2_000);
        // 只有条件时按时间倒序：按相关度排（成员全是并列）会给出看起来随机的顺序。
        assert_eq!(search(&conn, "is:unread", 10).unwrap(), vec![newer, older]);
    }

    #[test]
    fn text_and_filters_apply_together() {
        let (conn, acct, inbox, _) = mem();
        seed(&conn, acct, inbox, 1, "alpha", "x@example.com", 1_000);
        let newer = seed(&conn, acct, inbox, 2, "alpha", "y@example.com", 2_000);
        messages::set_starred(&conn, newer, true).unwrap();

        // 文本命中两封，加上条件后只剩一封。
        assert_eq!(search(&conn, "alpha", 10).unwrap().len(), 2);
        assert_eq!(search(&conn, "alpha is:starred", 10).unwrap(), vec![newer]);

        // 日期条件按 UTC 日界，与 `m.date`（服务端给的 UTC 秒）同尺度。
        assert_eq!(
            search(&conn, "alpha after:1970-01-01", 10).unwrap().len(),
            2
        );
        assert!(
            search(&conn, "alpha before:1970-01-01", 10)
                .unwrap()
                .is_empty()
        );

        // 写坏的日期**整条退回文本**（与 `before:2026-13-40` 同一行为），所以它只会让
        // 查询更窄 —— 0 命中，而不是静默变成一个「筛了但筛不到」的条件。这条断言是
        // 有意钉住「宁可搜不到，也不搜别的」这个取舍的。
        assert!(
            search(&conn, "alpha after:1970-01-01T01", 10)
                .unwrap()
                .is_empty()
        );

        // 条件满足、文本不满足：仍然是空。
        assert!(search(&conn, "beta is:starred", 10).unwrap().is_empty());
    }
}
