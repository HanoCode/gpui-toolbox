//! SQL 智能层（纯函数，零 I/O）：语句拆分、分类、风险分级与只读证明。
//!
//! 取向来自 dbx 的 `dbx-sql-core`：这层**只碰 SQL 文本，不碰数据库**，
//! 因此可以被单测钉死，也能被未来的 AI 闸门直接复用——模型产出的每条语句
//! 都必须过 [`prove_read_only`]，写操作必须走确认流程。

use sqlparser::ast::{
    Expr, GroupByExpr, Query, Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::ops::Range;

use crate::config::DatabaseKind;

/// 拆分后的单条语句的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtKind {
    /// 返回行的读语句（SELECT / SHOW / EXPLAIN / PRAGMA 视引擎而定）。
    Read,
    /// 改数据的写语句（INSERT / UPDATE / DELETE）。
    Write,
    /// 结构与权限类（CREATE / ALTER / DROP / GRANT / …）。
    Ddl,
    /// 无法判定（解析失败、非 SQL 等）。执行器按「可能返回行」的保守路径处理。
    Other,
}

/// 风险分级（从低到高）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqlRisk {
    /// 已证明只读：可以不经确认直接执行（AI 闸门的放行条件）。
    ReadOnly,
    /// 有写入，但不删结构。
    Write,
    /// 危险：删库删表或全表清改（无 WHERE 的 DELETE / UPDATE）。
    Dangerous,
}

/// 按引号 / 注释感知的方式把一段 SQL 拆成单条语句（不含尾分号）。
///
/// 自己写拆分而不是用解析器重建文本：拆分的输入常常**不是合法 SQL**
/// （用户写一半就来执行），解析器这条路会整段失败。
///
/// 认识的语法见 [`Splitter`]：三种引号、两类注释、PostgreSQL dollar-quote、
/// MySQL 的 `DELIMITER` 行指令与例程体、SQL Server 的 `GO` 批分隔。
#[must_use]
pub fn split_statements(sql: &str) -> Vec<String> {
    split_statement_spans(sql)
        .into_iter()
        .map(|span| span.text)
        .collect()
}

/// 一条完整语句及其在原文中的起始字节偏移。
///
/// `text` 与 [`split_statements`] 的产出同源（trim 过的语句文本）；`start` 指向
/// 语句的**第一个非空白字符**——界面据此把「逐条执行」的图标钉在语句首行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpan {
    /// 语句文本（与 [`split_statements`] 同口径：trim 过）。
    pub text: String,
    /// 语句首字符在原文里的字节偏移。
    pub start: usize,
}

/// [`split_statements`] 的带位置版本：除了语句文本，还回每条语句的起始偏移。
///
/// 扫描逻辑与 [`split_statements`] **同源**（同一个 [`Splitter`]）：界面上「第几行
/// 有 ▶」与「执行的是哪段文本」必须来自同一次切分，漂了就是图标指到别的语句上。
#[must_use]
pub fn split_statement_spans(sql: &str) -> Vec<StatementSpan> {
    Splitter::new(sql).run().0
}

/// 这段文本除空白与注释外**没有任何内容**（`-- x` / `/* x */` / 空串）。
///
/// 给执行文件那条路用的：`GO` 断批时切分器会把待flush的缓冲交出来，而那个缓冲
/// 可能只装着一条注释（`-- 头注` 独占一行后紧跟 `GO`）——它不是语句，不该送去执行。
/// 判定直接复用词法器（[`tokenize`] 本来就跳过注释与字面量），不另写一套扫描。
#[must_use]
pub(crate) fn is_blank_or_comment(text: &str) -> bool {
    tokenize(text).is_empty()
}

/// **流式**语句扫描器：分块喂入，跨块保住切分状态。
///
/// 为什么需要它：`Splitter` 的 `DELIMITER` 档位是**跨语句**的状态，而它一次只吃
/// 一整段文本（`split_statement_spans` 是纯函数）。执行 100MB 的 dump 时不能把
/// 整份文本端在内存里等着，于是这里做一件事——**每次重扫只从「上一条已交出的
/// 语句之后」开始**：
///
/// 1. 缓冲区里最后一条 span **永远留着不交**（它可能被这一块截断，是半句话），
///    于是尾巴最多只装「一条语句」；
/// 2. 下一块接在它后面，重扫从它的首字符开始——引号 / 注释 / 例程体的状态都在
///    这一条语句内部自洽，重扫结果与「整段一次扫」逐字相同；
/// 3. 两样**跨语句**的东西显式端过去：`DELIMITER` 档位（[`Splitter::run`] 的第二个
///    返回值），以及尾巴偏移 0 处算不算行首（[`Splitter::at_offset`]）。后者不靠
///    「往尾巴前面补一个空格 / 换行」去凑——`SELECT 1; GO` 这种写法里 `GO` 不在
///    行首（整段扫时它是普通词），补字符只会把这件事弄反。
///
/// 等价性由 `streaming_scanner_matches_whole_text_split` 钉死（同一份 dump 按
/// 1 / 2 / 3 / 5 / 7 / 16 / 64 字节切块喂进来，产出必须与整段切分逐条相同）。
#[derive(Debug, Clone)]
pub struct StatementScanner {
    /// 还没交出去的尾巴（含最后一条未竟语句）。
    tail: String,
    /// 尾巴当前生效的语句分隔符（`DELIMITER` 改过就是它，默认 `;`）。
    delimiter: String,
    /// 尾巴 offset 0 处算不算行首（只有文件开头那一块算）。
    starts_at_line_start: bool,
}

impl Default for StatementScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl StatementScanner {
    /// 新建（空尾巴、默认分隔符 `;`、开头算行首）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            tail: String::new(),
            delimiter: ";".to_owned(),
            starts_at_line_start: true,
        }
    }

    /// 尾巴（还没交出去的那部分）的字节数——调用方据此知道单条超大语句的驻留量。
    #[must_use]
    pub fn tail_len(&self) -> usize {
        self.tail.len()
    }

    /// 喂一块文本，返回这一块里**能确定已经写完**的语句。
    ///
    /// 块尾那条语句会被留着（不知道后面还有没有内容），下一块或 [`Self::finish`]
    /// 才交出来——所以**语句顺序不变、总数不变**，只是每条晚一步交付。
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.tail.push_str(chunk);
        self.drain(false)
    }

    /// 文本到头：把尾巴里剩下的那条语句交出来（尾巴里没有语句就返回空）。
    ///
    /// 收口用，之后不该再 [`Self::push`]。
    pub fn finish(&mut self) -> Vec<String> {
        self.drain(true)
    }

    /// 扫描尾巴，交出前 N-1 条（`eof` 时全交）。
    fn drain(&mut self, eof: bool) -> Vec<String> {
        // ⚠️ 两样状态都要传：上一块结束时的分隔符档位、尾巴开头是不是行首。
        let (mut spans, delimiter) =
            Splitter::with_delimiter(&self.tail, &self.delimiter, self.starts_at_line_start).run();
        // 行指令只在语句缓冲为空时认，所以「留着的这条语句首字符」之后不可能再改
        // 档位——扫描结束时的档位就是下次该用的档位。
        self.delimiter = delimiter;
        if eof {
            let out = spans.into_iter().map(|span| span.text).collect();
            self.tail.clear();
            return out;
        }
        // 最后一条留着 —— 它可能是被这一块截断的半句话。
        let Some(last) = spans.pop() else {
            // 整块都在「不成句」的区域（空白 / 注释 / 行指令）：尾巴原样留着。
            // 行指令必须留着，否则 `DELIMITER $$` 会随着「这块没语句」被丢掉。
            return Vec::new();
        };
        // 尾巴 = 从这条语句首字符起的原文（它后面只可能是空白 / 注释 / 分隔符）。
        let keep = self.tail[last.start..].to_owned();
        // 新尾巴的 offset 0 就是原文里的 `last.start`：它是不是行首按原文如实定。
        self.starts_at_line_start = if last.start == 0 {
            self.starts_at_line_start
        } else {
            at_line_start(&self.tail, last.start)
        };
        self.tail = keep;
        spans.into_iter().map(|span| span.text).collect()
    }
}

/// 头部词只看前几个：`CREATE [DEFINER = …] PROCEDURE` 最多四五个词就到关键字。
const HEAD_WORD_CAP: usize = 6;

/// `END` 后面跟这些词时是**块**尾（`END IF;` / `END WHILE;`），不是例程尾。
///
/// 少了这一条，`BEGIN IF … THEN …; END IF; … END;` 会在 `END IF;` 上把层数清零，
/// 后面那句 `;` 就把例程从中间切开——MySQL 的例程体里 `IF` / 循环块是常态，不是特例。
const BLOCK_END_WORDS: [&str; 5] = ["IF", "WHILE", "LOOP", "CASE", "REPEAT"];

/// 扫描状态。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    None,
    Single,
    Double,
    Backtick,
    Block,
    /// dollar-quote 体内（收尾定界符在 [`Splitter::run`] 的 `dollar_close` 里）。
    Dollar,
}

/// 单趟字符扫描的语句切分器：文本与起始偏移一起产出。
///
/// 认的语法分三类，每类都只影响「用户真的这么写」的输入——普通 SQL 的切分结果与
/// 「只认引号 + 注释 + `;`」的老实现逐字相同：
///
/// 1. **遮蔽**：`'…'`（`\` 与 `''` 两种转义）、`"…"`、`` `…` ``、`-- …` 行注释、
///    `/* … */` 块注释（**不嵌套**：MySQL 不嵌套，PG 里嵌套注释少见到不值得为它
///    冒「注释吃不掉、后面全被吞」的险）；
/// 2. **PostgreSQL dollar-quote**：`$tag$ … $tag$`（`$$` 也是合法 tag）。`$1` 这类
///    参数占位符不会被误认——见 [`dollar_tag`] 的「tag 首字符必须是字母 / 下划线」门；
/// 3. **批 / 客户端分隔**：`DELIMITER $$` 行指令（MySQL dump 的例程体全靠它，指令行
///    本身不是语句，丢弃）、`CREATE … PROCEDURE|FUNCTION|TRIGGER|EVENT … BEGIN … END`
///    整体不切（体内 `;` 不切）、独占一行的 `GO`（SQL Server 批分隔，丢弃——它结束
///    上一批，正是切分点）。
struct Splitter<'a> {
    /// 原文。
    sql: &'a str,
    /// 已切出的语句。
    out: Vec<StatementSpan>,
    /// 当前语句缓冲。
    current: String,
    /// 当前语句**第一个实义字符**的字节偏移（`None` = 还没吃到）。
    current_start: Option<usize>,
    /// 当前生效的语句分隔符（`DELIMITER` 行指令可改，默认 `;`）。
    delimiter: String,
    /// 正在成形的词（`[A-Za-z0-9_]` 串，已大写），用来认 `BEGIN` / `END` 与头部词。
    word: String,
    /// 当前语句的头部词（大写，最多 [`HEAD_WORD_CAP`] 个）。
    head_words: Vec<String>,
    /// 当前语句是不是例程定义（头部 `CREATE … PROCEDURE|FUNCTION|TRIGGER|EVENT`）。
    routine_head: bool,
    /// 例程体里的 `BEGIN` 层数。
    routine_depth: usize,
    /// 刚吃到一个 `END`：等下一个词决定它是块尾（`END IF`）还是例程尾。
    routine_end_pending: bool,
    /// 偏移 0 处算不算**行首**（默认算；流式扫描器续扫尾巴时按原文如实传）。
    ///
    /// 只有行指令（`GO` / `DELIMITER`）看这个——续扫的尾巴里，偏移 0 未必真是行首
    /// （`SELECT 1; GO` 里的 `GO` 就不在行首，整段扫时它是普通词，续扫也得是）。
    starts_at_line_start: bool,
}

impl<'a> Splitter<'a> {
    fn new(sql: &'a str) -> Self {
        Self::with_delimiter(sql, ";", true)
    }

    /// 带**初始分隔符**与**行首属性**的构造：流式扫描器续扫上一块的尾巴时，
    /// 要把 `DELIMITER` 档位端过来（见 [`StatementScanner`]）。
    fn with_delimiter(sql: &'a str, delimiter: &str, starts_at_line_start: bool) -> Self {
        Self {
            sql,
            out: Vec::new(),
            current: String::new(),
            current_start: None,
            delimiter: delimiter.to_owned(),
            word: String::new(),
            head_words: Vec::new(),
            routine_head: false,
            routine_depth: 0,
            routine_end_pending: false,
            starts_at_line_start,
        }
    }

    /// [`at_line_start`] 的成员版：偏移 0 的答案不由文本决定（见字段注释）。
    fn at_offset(&self, offset: usize) -> bool {
        if offset == 0 {
            self.starts_at_line_start
        } else {
            at_line_start(self.sql, offset)
        }
    }

    /// 扫描全文，产出语句清单与**结束时的分隔符档位**。
    fn run(mut self) -> (Vec<StatementSpan>, String) {
        let mut chars = self.sql.char_indices().peekable();
        let mut state = Scan::None;
        let mut dollar_close = String::new();
        while let Some((offset, c)) = chars.next() {
            match state {
                Scan::None => {
                    // 行级指令只在**行首**认（`GO` / `DELIMITER` 都是整行的东西）。
                    if self.at_offset(offset) && self.line_directive(offset, &mut chars) {
                        continue;
                    }
                    // 分隔符命中（默认 `;`）：例程体内部不切。
                    if self.sql[offset..].starts_with(self.delimiter.as_str()) {
                        self.word_boundary();
                        self.resolve_pending_end();
                        let delimiter = self.delimiter.clone();
                        let width = delimiter.chars().count();
                        if self.routine_depth == 0 {
                            for _ in 1..width {
                                chars.next();
                            }
                            self.flush();
                        } else {
                            // 体内的分隔符是**正文**（`SELECT 1; SELECT 2;` 都在体里）。
                            self.current.push_str(&delimiter);
                            for _ in 1..width {
                                chars.next();
                            }
                        }
                        continue;
                    }
                    match c {
                        '\'' => {
                            self.word_boundary();
                            self.start(offset);
                            self.current.push(c);
                            state = Scan::Single;
                        }
                        '"' => {
                            self.word_boundary();
                            self.start(offset);
                            self.current.push(c);
                            state = Scan::Double;
                        }
                        '`' => {
                            self.word_boundary();
                            self.start(offset);
                            self.current.push(c);
                            state = Scan::Backtick;
                        }
                        '$' => {
                            self.word_boundary();
                            self.start(offset);
                            match dollar_tag(self.sql, offset) {
                                Some(tag) => {
                                    self.current.push_str(&tag);
                                    for _ in 1..tag.chars().count() {
                                        chars.next();
                                    }
                                    dollar_close = tag;
                                    state = Scan::Dollar;
                                }
                                None => self.current.push(c),
                            }
                        }
                        '-' if self.sql.as_bytes().get(offset + 1) == Some(&b'-') => {
                            // 行注释：吃掉直到换行（换行本身保留，让错误定位的行号不漂移）。
                            self.word_boundary();
                            self.start(offset);
                            self.current.push(c);
                            for (_, n) in chars.by_ref() {
                                self.current.push(n);
                                if n == '\n' {
                                    break;
                                }
                            }
                        }
                        '/' if self.sql.as_bytes().get(offset + 1) == Some(&b'*') => {
                            self.word_boundary();
                            self.start(offset);
                            self.current.push(c);
                            state = Scan::Block;
                        }
                        c if is_word_char(c) => {
                            self.start(offset);
                            self.word.push(c.to_ascii_uppercase());
                            self.current.push(c);
                        }
                        c if c.is_whitespace() => {
                            self.word_boundary();
                            // 语句**前导**空白不进缓冲（偏移要落在第一个实义字符上）。
                            if self.current_start.is_some() {
                                self.current.push(c);
                            }
                        }
                        c => {
                            self.word_boundary();
                            self.start(offset);
                            self.current.push(c);
                        }
                    }
                }
                Scan::Single | Scan::Double | Scan::Backtick => {
                    self.current.push(c);
                    if c == '\\' && state == Scan::Single {
                        // MySQL 风格的反斜杠转义：原样带过下一个字符。
                        if let Some((_, n)) = chars.next() {
                            self.current.push(n);
                        }
                        continue;
                    }
                    let closing = match state {
                        Scan::Double => '"',
                        Scan::Backtick => '`',
                        _ => '\'',
                    };
                    if c == closing {
                        // 双写引号（`''` / `""`）是转义，继续留在引号里。
                        if chars.peek().is_some_and(|(_, n)| *n == closing) {
                            let (_, n) = chars.next().expect("peeked char must exist");
                            self.current.push(n);
                        } else {
                            state = Scan::None;
                        }
                    }
                }
                Scan::Block => {
                    self.current.push(c);
                    if c == '*' && chars.peek().is_some_and(|(_, n)| *n == '/') {
                        let (_, n) = chars.next().expect("peeked char must exist");
                        self.current.push(n);
                        state = Scan::None;
                    }
                }
                Scan::Dollar => {
                    // 收尾定界符与开定界符逐字相同（PG 的规则）。
                    if !dollar_close.is_empty()
                        && self.sql[offset..].starts_with(dollar_close.as_str())
                    {
                        self.current.push_str(&dollar_close);
                        for _ in 1..dollar_close.chars().count() {
                            chars.next();
                        }
                        dollar_close.clear();
                        state = Scan::None;
                    } else {
                        self.current.push(c);
                    }
                }
            }
        }
        self.word_boundary();
        self.flush();
        (self.out, self.delimiter)
    }

    /// 行首指令：`GO`（断批）与 `DELIMITER x`（改分隔符）。
    ///
    /// 返回 `true` 表示这一行已被吃掉（调用方进下一轮）。两条判定都要求**行首**；
    /// `DELIMITER` 还要求当前语句缓冲是空的——它本身就是语句级指令，夹在语句中间
    /// 只可能是数据（那种情况按普通文本处理，不动分隔符）。
    fn line_directive(
        &mut self,
        offset: usize,
        chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    ) -> bool {
        let line = self.sql[offset..].split('\n').next().unwrap_or_default();
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("GO") {
            // `GO` 是断批点：它**之前**的语句到此收尾（T-SQL 里没有分号也照样断）。
            self.flush();
            eat_line(chars);
            return true;
        }
        if self.current.trim().is_empty() {
            let upper = trimmed.to_ascii_uppercase();
            if upper.starts_with("DELIMITER")
                && upper
                    .as_bytes()
                    .get("DELIMITER".len())
                    .is_some_and(u8::is_ascii_whitespace)
            {
                // 分隔符原样取（不能拿大写副本的切片：`$$` 无所谓，但 `//` 之类也不行）。
                let token = trimmed["DELIMITER".len()..].trim();
                if !token.is_empty() {
                    self.delimiter = token.to_owned();
                    eat_line(chars);
                    return true;
                }
            }
        }
        false
    }

    /// 记下当前语句的第一个实义字符位置（只在首次生效）。
    fn start(&mut self, offset: usize) {
        if self.current_start.is_none() {
            self.current_start = Some(offset);
        }
    }

    /// 词收尾：记账头部词、例程判据与 `BEGIN` / `END` 层数。
    fn word_boundary(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word);
        if self.head_words.len() < HEAD_WORD_CAP {
            self.head_words.push(word.clone());
            self.refresh_routine_head();
        }
        if self.routine_end_pending {
            self.routine_end_pending = false;
            // `END` 后面跟的是块名（`IF` / `WHILE` …）= 块尾，层数不动。
            if !BLOCK_END_WORDS.contains(&word.as_str()) {
                self.routine_depth = self.routine_depth.saturating_sub(1);
            }
        }
        if self.routine_head {
            match word.as_str() {
                "BEGIN" => self.routine_depth += 1,
                "END" => self.routine_end_pending = true,
                _ => {}
            }
        }
    }

    /// 语句在 `END` 上收尾（后面没有词了）：把待决的 `END` 兑现成层数减一。
    fn resolve_pending_end(&mut self) {
        if self.routine_end_pending {
            self.routine_end_pending = false;
            self.routine_depth = self.routine_depth.saturating_sub(1);
        }
    }

    /// 头部词够不够认例程：`CREATE|ALTER … PROCEDURE|FUNCTION|TRIGGER|EVENT`。
    fn refresh_routine_head(&mut self) {
        if self.routine_head {
            return;
        }
        match self.head_words.first().map(String::as_str) {
            Some("CREATE" | "ALTER") => {}
            _ => return,
        }
        if self.head_words[1..].iter().any(|word| {
            matches!(
                word.as_str(),
                "PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT"
            )
        }) {
            self.routine_head = true;
        }
    }

    /// 收尾当前语句：文本为空（或只有空白 / 注释外的空白）就丢弃。
    ///
    /// 每条语句的**语法记账**（头部词 / 层数）一并归零——它们是「这条语句」的属性。
    fn flush(&mut self) {
        let text = self.current.trim();
        if !text.is_empty() {
            self.out.push(StatementSpan {
                text: text.to_owned(),
                start: self.current_start.unwrap_or(0),
            });
        }
        self.current.clear();
        self.current_start = None;
        self.word.clear();
        self.head_words.clear();
        self.routine_head = false;
        self.routine_depth = 0;
        self.routine_end_pending = false;
    }
}

/// 当前字符是否处于行首（`offset == 0` 或前一个字节是换行）。
///
/// 直接看字节而不是维护一个「上一轮是不是换行」的标志：引号里跨行时标志会失准，
/// 而 `\n` 是 ASCII，与 UTF-8 的续字节（≥ 0x80）永不混淆。
fn at_line_start(sql: &str, offset: usize) -> bool {
    offset == 0 || sql.as_bytes().get(offset - 1) == Some(&b'\n')
}

/// 吃掉当前行剩下部分（含换行）。行指令（`GO` / `DELIMITER`）整行丢弃。
fn eat_line(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) {
    for (_, c) in chars.by_ref() {
        if c == '\n' {
            break;
        }
    }
}

/// 词字符：ASCII 字母 / 数字 / 下划线。用来认 `BEGIN` / `END` 与头部词。
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 试图把 `offset` 处的 `$` 认成 dollar-quote 的开定界符，成功返回完整定界符
/// （`$$` / `$tag$`）。
///
/// tag 的规则照抄 PostgreSQL：空串合法；非空时首字符必须是字母或下划线，其余是
/// 字母 / 数字 / 下划线。**这道门把 `$1` 这类参数占位符挡在外面**——它后面没有收尾
/// 的 `$`，认成定界符会把后面整段文本吞掉。
fn dollar_tag(sql: &str, offset: usize) -> Option<String> {
    let bytes = sql.as_bytes();
    if bytes.get(offset) != Some(&b'$') {
        return None;
    }
    let mut index = offset + 1;
    let mut first = true;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'$' {
            return Some(sql[offset..=index].to_owned());
        }
        let allowed = if first {
            byte.is_ascii_alphabetic() || byte == b'_'
        } else {
            byte.is_ascii_alphanumeric() || byte == b'_'
        };
        if !allowed {
            return None;
        }
        first = false;
        index += 1;
    }
    None
}

/// 解析一条语句；解析失败按 `Other` 处理（调用方走保守路径）。
fn parse_one(stmt: &str) -> Option<Statement> {
    Parser::parse_sql(&GenericDialect {}, stmt)
        .ok()
        .and_then(|mut ast| if ast.len() == 1 { ast.pop() } else { None })
}

/// 解析失败路径兜底用的词法标记：只区分「词」与「左括号」，标点折叠成 `Other`。
///
/// 目的是把注释与**字符串字面量**排除在外（见 [`tokenize`]）——`SELECT 'DELETE' FROM t`
/// 里的 `DELETE` 是数据，不是关键字。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// 一段 ASCII 字母 / 数字 / 下划线（已大写）。
    Word(String),
    /// `(`（用来判「名字后面紧跟左括号」= 函数调用）。
    LParen,
    /// 其它标点（会打断「名字 + 左括号」的相邻关系）。
    Other,
}

/// 把语句切成词法标记：跳过 `'…'` / `"…"` / `` `…` `` 字面量与 `--` / `/* */` 注释。
///
/// 只做**词法**，不做语法——解析器在这条路上已经失败，兜底只能用最粗的信号。
/// 逐字节扫描对 UTF-8 安全：多字节字符的任何字节都不会等于 ASCII 的引号 / 注释符号。
fn tokenize(stmt: &str) -> Vec<Tok> {
    let bytes = stmt.as_bytes();
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut i = 0;
    // 词缓冲落的收尾统一走它。
    macro_rules! flush {
        () => {
            if !word.is_empty() {
                tokens.push(Tok::Word(std::mem::take(&mut word)));
            }
        };
    }
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' | b'`' => {
                flush!();
                i = skip_quoted(bytes, i, b);
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                flush!();
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                flush!();
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b if b.is_ascii_alphanumeric() || b == b'_' => {
                word.push((b as char).to_ascii_uppercase());
                i += 1;
            }
            b if b.is_ascii_whitespace() => {
                flush!();
                i += 1;
            }
            b'(' => {
                flush!();
                tokens.push(Tok::LParen);
                i += 1;
            }
            _ => {
                flush!();
                tokens.push(Tok::Other);
                i += 1;
            }
        }
    }
    flush!();
    tokens
}

/// 跳过一段引号字面量，返回**下一个**字节的下标。
///
/// 规则与 [`split_statements`] 同源：单引号里 `\x` 是转义；双写引号（`''` / `""`）
/// 也是转义，字面量不在这里结束。
fn skip_quoted(bytes: &[u8], open: usize, quote: u8) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && quote == b'\'' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// 判定写 / DDL 的关键字（大写比较）。出现在语句**任意位置**即视为写信号——
/// 兜底路径不做语法，只看词法。
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "REPLACE", "MERGE", "UPSERT", "TRUNCATE", "DROP", "ALTER",
    "CREATE", "RENAME", "GRANT", "REVOKE", "VACUUM", "REINDEX", "LOAD", "CALL", "COPY", "ATTACH",
    "DETACH",
];

/// 标记 `FOR [NO KEY | KEY] UPDATE | FOR SHARE` 锁定子句占用的词。
///
/// 这些词里的 `UPDATE` / `SHARE` **不是** DML 写：`SELECT … FOR UPDATE` 只是取行锁。
/// 剔除它们，兜底的写关键字扫描才不会把取锁的 SELECT 误判成写。返回与 `tokens` 等长的位图。
fn locking_mask(tokens: &[Tok]) -> Vec<bool> {
    let mut mask = vec![false; tokens.len()];
    for i in 0..tokens.len() {
        if !matches!(&tokens[i], Tok::Word(w) if w == "FOR") {
            continue;
        }
        let mut k = i + 1;
        if matches!(tokens.get(k), Some(Tok::Word(w)) if w == "NO") {
            k += 1;
        }
        if matches!(tokens.get(k), Some(Tok::Word(w)) if w == "KEY") {
            k += 1;
        }
        if matches!(tokens.get(k), Some(Tok::Word(w)) if w == "UPDATE" || w == "SHARE") {
            mask[i..=k].fill(true);
        }
    }
    mask
}

/// 取语句的词序列（大写），**剔除锁定子句里的 `UPDATE` / `SHARE`**（见 [`locking_mask`]）。
fn scan_words(stmt: &str) -> Vec<String> {
    let tokens = tokenize(stmt);
    let locked = locking_mask(&tokens);
    tokens
        .into_iter()
        .enumerate()
        .filter_map(|(i, tok)| match tok {
            Tok::Word(w) if !locked[i] => Some(w),
            _ => None,
        })
        .collect()
}

/// 语句是否含 `SELECT … FOR UPDATE / SHARE` 之类的**取锁**子句（词法判定，覆盖
/// `FOR NO KEY UPDATE` / `FOR KEY SHARE` —— 这两种 GenericDialect 解析不了）。
fn has_locking_clause(tokens: &[Tok]) -> bool {
    locking_mask(tokens).into_iter().any(|locked| locked)
}

/// 解析失败路径的兜底风险：扫描整条的词法标记（跳过字符串 / 注释、剔除取锁子句）。
///
/// - 扫不到任何写关键字 → `None`（交给上层，`Other` 仍按保守处理）；
/// - 出现 `DROP` / `TRUNCATE` → `Dangerous`；
/// - 出现**没有 `WHERE` 的** `DELETE` / `UPDATE` → `Dangerous`（全表清改）；
/// - 其余出现写关键字的形态 → `Write`。
///
/// ⚠️ 这是**词法**兜底，不做语法：`WHERE 1 = 1` 这类恒真条件识别不了（会被当有 WHERE）。
/// 宁可多报危险（fail-closed），不可漏报。
fn keyword_risk(stmt: &str) -> Option<SqlRisk> {
    let words = scan_words(stmt);
    if !words.iter().any(|w| WRITE_KEYWORDS.contains(&w.as_str())) {
        return None;
    }
    if words.iter().any(|w| w == "DROP" || w == "TRUNCATE") {
        return Some(SqlRisk::Dangerous);
    }
    for dml in ["DELETE", "UPDATE"] {
        if let Some(pos) = words.iter().position(|w| w == dml) {
            // 该 DML 之后没有 WHERE → 全表清改（更靠前的 WHERE 是别处的条件，不算）。
            if !words[pos..].iter().any(|w| w == "WHERE") {
                return Some(SqlRisk::Dangerous);
            }
        }
    }
    Some(SqlRisk::Write)
}

/// 按首关键字猜测类别；给解析器覆盖不到的方言词（MySQL 的 `LOCK TABLES` 之类）兜底。
///
/// ⚠️ **这里只做「解析失败时的保守兜底」**：`SELECT` / `WITH` 头一旦解析失败**绝不**
/// 直接判读——`WITH x AS (...) DELETE …`、`SELECT … INTO OUTFILE` 这类写构造正是解析
/// 不了的形态。改为**扫描整条的词法标记**：扫到写关键字（`DELETE` / `UPDATE` / `INSERT`
/// / `TRUNCATE` / `DROP` …）就至少算写。PRAGMA 另走 [`pragma_fallback_kind`]。
fn keyword_kind(stmt: &str) -> StmtKind {
    let words = scan_words(stmt);
    let head = words.first().map(String::as_str).unwrap_or_default();
    match head {
        // 只保留「形态上就是读」的词：SHOW / DESCRIBE / USE 不出写；PRAGMA 单独判。
        "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "USE" => StmtKind::Read,
        "PRAGMA" => pragma_fallback_kind(stmt),
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "MERGE" | "UPSERT" | "LOAD" | "CALL"
        | "COPY" => StmtKind::Write,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE" | "VACUUM"
        | "ANALYZE" | "REINDEX" | "LOCK" | "UNLOCK" | "SET" | "BEGIN" | "START" | "COMMIT"
        | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => StmtKind::Ddl,
        _ => {
            // 头部不是写词（`WITH` / `SELECT` / 乱码）：扫整条，扫到写关键字即算写。
            if words.iter().any(|w| WRITE_KEYWORDS.contains(&w.as_str())) {
                StmtKind::Write
            } else {
                StmtKind::Other
            }
        }
    }
}

/// 「带参数也只读」的 PRAGMA 白名单（小写比较）。
///
/// 只放**取信息**型：`table_info` / `index_list` 这类；像 `journal_mode` / `synchronous`
/// / `encoding` 这类「有参即写」的**不放**——它们带参时会被判写。
const PRAGMA_READERS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "database_list",
    "collation_list",
    "compile_options",
    "pragma_list",
    "function_list",
    "module_list",
    "integrity_check",
    "quick_check",
    "page_count",
    "freelist_count",
];

/// 「**无值时**只读」的 PRAGMA 白名单（小写比较）。
///
/// 覆盖 SQLite 常见的「无值取读、有值判写」型（`journal_mode` / `synchronous` / `encoding`
/// 等）。⚠️ 显式**不含**维护 / 写入型——`wal_checkpoint`、`optimize`、`shrink_memory`、
/// `incremental_vacuum` 这类**无值也写**，必须留白发证明。宁可少放（fail-closed）。
const PRAGMA_BARE_READERS: &[&str] = &[
    "user_version",
    "schema_version",
    "application_id",
    "journal_mode",
    "synchronous",
    "encoding",
    "page_size",
    "cache_size",
    "mmap_size",
    "auto_vacuum",
    "locking_mode",
    "foreign_keys",
    "recursive_triggers",
    "temp_store",
    "busy_timeout",
    "read_uncommitted",
    "secure_delete",
    "wal_autocheckpoint",
    "journal_size_limit",
    "cache_spill",
    "max_page_count",
    "soft_heap_limit",
    "threads",
    "data_version",
    "legacy_file_format",
    "defer_foreign_keys",
    "count_changes",
    "reverse_unordered_selects",
    "ignore_check_constraints",
    "trusted_schema",
    "query_only",
    "cell_size_check",
    "checkpoint_fullfsync",
    "fullfsync",
];

/// PRAGMA 名（取 `schema.pragma` 里的最后一段，小写）。
fn pragma_last_name(name: &sqlparser::ast::ObjectName) -> String {
    name.0
        .last()
        .map(|ident| ident.value.to_ascii_lowercase())
        .unwrap_or_default()
}

/// PRAGMA 名是否属于「无值只读」白名单（[`PRAGMA_READERS`] ∪ [`PRAGMA_BARE_READERS`]）。
fn pragma_name_reads(name: &str) -> bool {
    PRAGMA_READERS.contains(&name) || PRAGMA_BARE_READERS.contains(&name)
}

/// 解析器认不出的 PRAGMA（参数是标识符的形态，如 `PRAGMA table_info(t)` /
/// `PRAGMA journal_mode = WAL`）的保守分类。
///
/// 规则（fail-closed）：`=` 赋值形态一律写；带参形态只有落在 [`PRAGMA_READERS`] 白名单里
/// 才判读（其余如 `PRAGMA journal_mode(WAL)` 判写）；无参形态放宽到 [`pragma_name_reads`]。
/// 可解析的 `PRAGMA name [= v | (v)]` 走 [`classify`] 里的 `Statement::Pragma` 分支。
fn pragma_fallback_kind(stmt: &str) -> StmtKind {
    let upper = stmt.trim_start().to_ascii_uppercase();
    let rest = upper.strip_prefix("PRAGMA").unwrap_or(&upper).trim_start();
    if rest.contains('=') {
        return StmtKind::Write;
    }
    let name_end = rest
        .find(['(', ' ', '\t', '\n', '\r'])
        .unwrap_or(rest.len());
    let name = rest[..name_end].to_ascii_lowercase();
    let reads = if rest.contains('(') {
        PRAGMA_READERS.contains(&name.as_str())
    } else {
        pragma_name_reads(&name)
    };
    if reads {
        StmtKind::Read
    } else {
        StmtKind::Write
    }
}

/// 一条 `SELECT` / `Query`（含 CTE、集合运算、派生表）是否**可证明只读**。
///
/// 数据修改型 CTE（PG 的 `WITH x AS (INSERT/UPDATE … RETURNING *) …`）、
/// `SELECT … INTO new_table` / `INTO OUTFILE`、以及派生表里嵌的 DML，全部不放行。
fn query_is_read_only(query: &Query) -> bool {
    let ctes_read_only = query.with.as_ref().is_none_or(|with| {
        with.cte_tables
            .iter()
            .all(|cte| query_is_read_only(&cte.query))
    });
    ctes_read_only && set_expr_is_read_only(&query.body)
}

/// `SetExpr`（SELECT / 括号子查询 / 集合运算 / `VALUES` / DML 体）是否只读。
fn set_expr_is_read_only(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(select) => select_is_read_only(select),
        SetExpr::Query(inner) => query_is_read_only(inner),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_is_read_only(left) && set_expr_is_read_only(right)
        }
        SetExpr::Values(_) | SetExpr::Table(_) => true,
        // 数据修改型 CTE 体：`WITH x AS (INSERT/UPDATE … RETURNING *) …`。
        SetExpr::Insert(_) | SetExpr::Update(_) => false,
    }
}

/// 一条 `SELECT` 本体是否只读：`INTO` 目标、以及 `FROM` 里的派生表都不能藏写。
fn select_is_read_only(select: &Select) -> bool {
    // `SELECT … INTO new_table` / `INTO OUTFILE`：结果被搬去别处，不是纯读。
    select.into.is_none() && select.from.iter().all(table_with_joins_is_read_only)
}

/// 一个 `FROM` 项（表 + JOIN）及 JOIN 的表因子是否只读。
fn table_with_joins_is_read_only(item: &TableWithJoins) -> bool {
    table_factor_is_read_only(&item.relation)
        && item
            .joins
            .iter()
            .all(|join| table_factor_is_read_only(&join.relation))
}

/// 一个表因子是否只读：派生表 / 嵌套 JOIN / PIVOT 壳里的子查询要递归检查，
/// 普通表名、表值函数、`UNNEST` 等不带内嵌 DML，直接放行。
fn table_factor_is_read_only(factor: &TableFactor) -> bool {
    match factor {
        TableFactor::Derived { subquery, .. } => query_is_read_only(subquery),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => table_with_joins_is_read_only(table_with_joins),
        TableFactor::Pivot { table, .. }
        | TableFactor::Unpivot { table, .. }
        | TableFactor::MatchRecognize { table, .. } => table_factor_is_read_only(table),
        _ => true,
    }
}

/// 语句是否**可证明只读**（AST 层面的判据，[`classify`] 与 [`prove_read_only`] 共用）。
///
/// `EXPLAIN` 的口径是 fail-closed：只有**内层语句本身可证明只读**才放行（`EXPLAIN
/// SELECT` / `EXPLAIN ANALYZE SELECT` 通过；`EXPLAIN DELETE` / `EXPLAIN ANALYZE
/// DELETE` 不通过）。带 `ANALYZE` 会真的执行内层，必须拦；不带 `ANALYZE` 时是否执行
/// 随引擎/方言漂移，一并从严。
fn statement_is_read_only(stmt: &Statement) -> bool {
    match stmt {
        Statement::Query(query) => query_is_read_only(query),
        // `EXPLAIN ANALYZE <stmt>` 会**真的执行**内层语句；而不带 `ANALYZE` 时是否执行
        // 由引擎决定（Postgres/MySQL/SQLite 不执行，但语法与行为随方言漂移）。这里取
        // fail-closed 口径：**只有内层本身可证明只读**的 `EXPLAIN` 才放行——`EXPLAIN
        // DELETE` 一律不自动执行（交人工），`EXPLAIN SELECT` / `EXPLAIN ANALYZE SELECT`
        // 照常放行。
        Statement::Explain { statement, .. } => statement_is_read_only(statement),
        // 赋值 / 带值形态（`PRAGMA n = v` / `PRAGMA n(v)`）：只有「带参也只读」的白名单
        // 才放行（其余判写）；无值形态也要过白名单——`wal_checkpoint` / `optimize` /
        // `shrink_memory` 这些**无值也写**，不能一律放行。
        Statement::Pragma { name, value, .. } => {
            let name = pragma_last_name(name);
            if value.is_some() {
                PRAGMA_READERS.contains(&name.as_str())
            } else {
                pragma_name_reads(&name)
            }
        }
        Statement::ExplainTable { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowTables { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowViews { .. } => true,
        _ => false,
    }
}

/// 分类一条语句。
#[must_use]
pub fn classify(stmt: &str) -> StmtKind {
    match parse_one(stmt) {
        // 读 / 写由 AST 全量判定：CTE 里的 DML、`SELECT … INTO`、`EXPLAIN ANALYZE <写>`
        // 都会被 [`statement_is_read_only`] 判成非只读，落到 Write。
        Some(
            parsed @ (Statement::Query(_) | Statement::Explain { .. } | Statement::Pragma { .. }),
        ) => {
            if statement_is_read_only(&parsed) {
                StmtKind::Read
            } else {
                StmtKind::Write
            }
        }
        Some(Statement::ExplainTable { .. })
        | Some(Statement::ShowVariables { .. })
        | Some(Statement::ShowStatus { .. })
        | Some(Statement::ShowColumns { .. })
        | Some(Statement::ShowTables { .. })
        | Some(Statement::ShowDatabases { .. })
        | Some(Statement::ShowSchemas { .. })
        | Some(Statement::ShowFunctions { .. })
        | Some(Statement::ShowViews { .. }) => StmtKind::Read,
        Some(Statement::Insert(_))
        | Some(Statement::Update { .. })
        | Some(Statement::Delete(_)) => StmtKind::Write,
        Some(Statement::CreateTable(_))
        | Some(Statement::CreateView { .. })
        | Some(Statement::CreateIndex(_))
        | Some(Statement::AlterTable { .. })
        | Some(Statement::AlterView { .. })
        | Some(Statement::Drop { .. })
        | Some(Statement::DropFunction { .. }) => StmtKind::Ddl,
        Some(_) | None => keyword_kind(stmt),
    }
}

/// 只读证明：语句**必须解析成功**且 AST 属于读语句族才放行。
/// 解析失败一律不给证明（宁可多弹一次确认，不可漏一次写库）。
#[must_use]
pub fn prove_read_only(stmt: &str) -> bool {
    parse_one(stmt).is_some_and(|parsed| statement_is_read_only(&parsed))
}

/// 已知会**产生副作用**的函数（大写比较；与 [`tokenize`] 的输出一致）。
///
/// ⚠️ **这是可扩充的「已知集合」，不是完备证明**：这里挡住的是最常被拿来做「借只读之
/// 名行写库之实」的一批（序列推进、跨库、会话 / 后端控制、大对象、配置写入、取锁）。
/// 未列出的函数若有副作用，`allow_auto_execute` 仍会放行——它的定位是「常规 SELECT 的
/// 自动执行闸门」，不是「无副作用的形式化证明」。要更严就继续往这张表里加。
const SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    // 序列推进 / 回置（`nextval` 一调即持久改序列状态）。
    "NEXTVAL",
    "SETVAL",
    // 跨库 / 远程执行。
    "DBLINK",
    "DBLINK_EXEC",
    "DBLINK_CONNECT",
    "DBLINK_CONNECT_U",
    "DBLINK_DISCONNECT",
    "DBLINK_GET_CONNECTION",
    "DBLINK_GET_RESULT",
    "DBLINK_OPEN",
    "DBLINK_FETCH",
    "DBLINK_CLOSE",
    "DBLINK_CANCEL",
    // 后端 / 会话控制。
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "PG_RELOAD_CONF",
    "PG_ROTATE_LOGFILE",
    "PG_LOG_BACKEND_MEMORY_CONTEXTS",
    "SET_CONFIG",
    // 大对象（写入 / 删除服务器端对象）。
    "LO_UNLINK",
    "LO_IMPORT",
    "LO_EXPORT",
    "LO_PUT",
    "LO_TRUNCATE",
    "LO_CREAT",
    "LO_CREATE",
    // 建议锁（取锁同样阻塞他人）。
    "PG_ADVISORY_LOCK",
    "PG_ADVISORY_LOCK_SHARED",
    "PG_ADVISORY_XACT_LOCK",
    "PG_ADVISORY_XACT_LOCK_SHARED",
    "PG_TRY_ADVISORY_LOCK",
    "PG_TRY_ADVISORY_LOCK_SHARED",
    "PG_TRY_ADVISORY_XACT_LOCK",
    "PG_TRY_ADVISORY_XACT_LOCK_SHARED",
    "GET_LOCK",
    "RELEASE_LOCK",
];

/// 语句是否带**已知副作用**：命中 [`SIDE_EFFECT_FUNCTIONS`]（名字后紧跟 `(`），
/// 或含 `FOR UPDATE / SHARE` 之类的取锁子句。
///
/// 词法判定（不做语法）：`allow_auto_execute` 与内核 `execute_read_only` 共用的
/// 「读形状但别自动跑」判据。解析不了的语句也不会误放行——它们本就过不了只读证明。
#[must_use]
pub fn has_write_side_effects(stmt: &str) -> bool {
    let tokens = tokenize(stmt);
    let calls_side_effect_fn = tokens.windows(2).any(|pair| match &pair[0] {
        Tok::Word(word) => SIDE_EFFECT_FUNCTIONS.contains(&word.as_str()) && pair[1] == Tok::LParen,
        _ => false,
    });
    calls_side_effect_fn || has_locking_clause(&tokens)
}

/// 「这段 SQL 能否**自动执行**」的唯一判据（`db.rs` 的 AI 闸门与内核
/// [`crate::drivers::Connection::execute_read_only`] 同源）：**恰好一条**语句、
/// 能被证明只读、且**不带已知副作用**（[`has_write_side_effects`]）。
///
/// 抽成纯函数是为了可单测：越权构造（数据修改型 CTE、`SELECT … INTO`、
/// `EXPLAIN ANALYZE <写>`、赋值型 PRAGMA / `INTO OUTFILE`）、带副作用的读
/// （`nextval` / `setval` / `dblink*` / `pg_terminate_backend` / `SET_CONFIG` …）、
/// 以及取锁读（`SELECT … FOR UPDATE / SHARE`）都必须为 `false`。
///
/// ⚠️ **边界（如实声明）**：副作用判定只覆盖 [`SIDE_EFFECT_FUNCTIONS`] 这张**已知集合**，
/// 以及取锁子句。**未知函数的副作用不在此列**——所以它是「常规 SELECT 的自动执行闸门」，
/// 不是「这条语句绝对无副作用」的完备证明。要收紧就扩充那张表。
#[must_use]
pub fn allow_auto_execute(sql: &str) -> bool {
    let statements = split_statements(sql);
    statements.len() == 1
        && prove_read_only(&statements[0])
        && !has_write_side_effects(&statements[0])
}

/// 一条语句对**显式事务**的影响。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxEffect {
    /// 开启事务（`BEGIN` / `START TRANSACTION`）。
    Begin,
    /// 结束事务（`COMMIT` / `ROLLBACK` / `END`）。
    End,
}

/// 判定单条语句对显式事务的影响（`None` = 无关）。
///
/// 只看首关键字，**不做语法解析**——用户常写半截 SQL 就执行，解析器在这条路上会整段
/// 失败，而事务状态显示必须稳。三引擎的共同拼法：SQLite / PostgreSQL 的 `BEGIN`、
/// MySQL 的 `START TRANSACTION`；收尾是 `COMMIT` / `ROLLBACK` / `END`（PG 的 `END`
/// 即 `COMMIT`）。
///
/// 首关键字要**跳过注释**：[`split_statements`] 会把语句前的注释留在头部
/// （`-- 说明\nBEGIN` 是常态）。
#[must_use]
pub fn tx_effect(stmt: &str) -> Option<TxEffect> {
    match statement_head(stmt).as_str() {
        "BEGIN" | "START" => Some(TxEffect::Begin),
        "COMMIT" | "ROLLBACK" | "END" => Some(TxEffect::End),
        _ => None,
    }
}

/// 显式事务的**控制动作**：工具条 Tx 开关按下一个时到底发哪条语句。
///
/// 与 [`tx_effect`] 是一对：那个负责「把发出去的语句认出来」（状态显示），这个负责
/// 「按用户意图造出语句」。两者必须闭环 —— 造出来的语句若认不出来，点一下开关后
/// `in_tx` 不会变，界面就卡在旧状态（见 [`TxControl::statement`] 的测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxControl {
    /// 开启事务。
    Begin,
    /// 提交。
    Commit,
    /// 回滚。
    Rollback,
}

impl TxControl {
    /// 按引擎给出控制语句。
    ///
    /// 三家只有「开启」有方言差异：SQLite / PostgreSQL 用 `BEGIN`，MySQL 用
    /// `START TRANSACTION`（`BEGIN` 在 MySQL 里是同义词，但 `START TRANSACTION`
    /// 不与 `BEGIN…END` 复合语句歧义，也允许带 `WITH CONSISTENT SNAPSHOT` 之类的选项）。
    /// 提交 / 回滚三家一致，`END` 虽然 PG 也认，但 `COMMIT` 更直白。
    #[must_use]
    pub fn statement(self, kind: DatabaseKind) -> &'static str {
        match self {
            Self::Begin => match kind {
                DatabaseKind::Mysql => "START TRANSACTION",
                _ => "BEGIN",
            },
            Self::Commit => "COMMIT",
            Self::Rollback => "ROLLBACK",
        }
    }

    /// 这个动作对显式事务的影响类别（与 [`tx_effect`] 同域）。
    #[must_use]
    pub const fn effect(self) -> TxEffect {
        match self {
            Self::Begin => TxEffect::Begin,
            Self::Commit | Self::Rollback => TxEffect::End,
        }
    }
}

/// 取语句的首个关键字（大写；跳过前导空白与 `--` / `/* */` 注释）。
fn statement_head(stmt: &str) -> String {
    let mut rest = stmt.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
        rest = rest.trim_start();
    }
    rest.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// 「恒真谓词」判定：`WHERE true` / `WHERE 1` 这类等价于没有 WHERE。
fn always_true(expr: &sqlparser::ast::Expr) -> bool {
    let text = expr.to_string();
    text.trim().eq_ignore_ascii_case("true")
        || text
            .trim()
            .parse::<f64>()
            .map(|v| v != 0.0)
            .unwrap_or(false)
}

/// 单条语句的风险分级。
#[must_use]
pub fn risk_of(stmt: &str) -> SqlRisk {
    match classify(stmt) {
        StmtKind::Read => SqlRisk::ReadOnly,
        StmtKind::Write => {
            // 无 WHERE 或恒真 WHERE 的 UPDATE / DELETE 是全表清改，升一级。
            match parse_one(stmt) {
                Some(Statement::Update { selection, .. }) => {
                    if selection.as_ref().is_none_or(always_true) {
                        SqlRisk::Dangerous
                    } else {
                        SqlRisk::Write
                    }
                }
                Some(Statement::Delete(del)) => {
                    if del.selection.as_ref().is_none_or(always_true) {
                        SqlRisk::Dangerous
                    } else {
                        SqlRisk::Write
                    }
                }
                // 解析不了（`WITH x AS (…) DELETE …` 之流）：词法兜底——扫到无 WHERE 的
                // DML 就判危险，别让 CTE 包裹的全表删躲过「执行危险 SQL」确认。
                _ => keyword_risk(stmt).unwrap_or(SqlRisk::Write),
            }
        }
        StmtKind::Ddl => {
            let head = scan_words(stmt);
            let head = head.first().map(String::as_str).unwrap_or_default();
            if head == "DROP" || head == "TRUNCATE" {
                SqlRisk::Dangerous
            } else {
                keyword_risk(stmt).unwrap_or(SqlRisk::Write)
            }
        }
        // 解析失败的形态：词法兜底（`WITH … DELETE` 落这里）。
        StmtKind::Other => keyword_risk(stmt).unwrap_or(SqlRisk::Write),
    }
}

/// 一段 SQL（多条语句）的整体风险：取最高档。
#[must_use]
pub fn risk_of_statements(stmts: &[String]) -> SqlRisk {
    stmts
        .iter()
        .map(|stmt| risk_of(stmt))
        .max()
        .unwrap_or(SqlRisk::ReadOnly)
}

/// 一条 SELECT 的「可编辑目标」：结果行与某张表的行是**一对一**时，给出那张表。
///
/// 判定的是「能不能把结果集当表来改」，不是「语句是不是 SELECT」。一条 SELECT 只有在
/// 下列条件全满足时才可编辑：
///
/// - 单表、**没有 JOIN**（多表连出来的行没有唯一归属的表）；
/// - 表因子就是一张**表名**（不是子查询 / 表值函数 / UNNEST / JSON_TABLE / PIVOT …）；
/// - 表**没有别名**（有别名时 `t.col` 里的列名与表列名对不上，改格定位会错表）；
/// - 没有 `DISTINCT` / `GROUP BY` / `HAVING` / `SELECT … INTO` —— 这些会让结果行
///   与表行不再是 1:1（一行结果可能是聚合了多行）；
/// - 投影只允许 `*` / `表.*` / 裸列名：出现函数、表达式、常量时，列名不再等于表列名，
///   按名字定位列会落空或落错。
///
/// 另外还要在**运行时**核一件事（这里给不出答案）：结果集里必须真的带上主键列，
/// 否则定位不了行——由调用方拿投影出来的列名对一遍主键。
///
/// `WITH`（CTE）与集合运算（`UNION` 等）一律不认：它们的行来源已经不可判定。
#[must_use]
pub fn edit_target(stmt: &str) -> Option<EditTarget> {
    let Some(Statement::Query(query)) = parse_one(stmt) else {
        return None;
    };
    if query.with.is_some() {
        return None;
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    if !is_plain_single_table(select) {
        return None;
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_ordinality,
        partitions,
        ..
    } = &select.from[0].relation
    else {
        return None;
    };
    // `args.is_some()` = 表值函数调用（`generate_series(...)` 之类），不是真表。
    if alias.is_some() || args.is_some() || *with_ordinality || !partitions.is_empty() {
        return None;
    }
    match name.0.as_slice() {
        // 裸表名：当前命名空间。
        [table] => Some(EditTarget {
            table: table.value.clone(),
            schema: None,
        }),
        // `schema.table`：带命名空间。
        [schema, table] => Some(EditTarget {
            table: table.value.clone(),
            schema: Some(schema.value.clone()),
        }),
        // `db.schema.table` 及以上：本仓三引擎都不支持（SQLite 是 attached 名 + 表名两段）。
        _ => None,
    }
}

/// [`edit_target`] 的返回值：结果集背后的那张表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTarget {
    /// 表名（未加引号的原名）。
    pub table: String,
    /// 命名空间（`SELECT * FROM schema.t` 里的 `schema`）。
    pub schema: Option<String>,
}

/// `SELECT` 的「结果行 ↔ 表行 一对一」那一半判定（表因子与投影）。
fn is_plain_single_table(select: &Select) -> bool {
    // `SELECT … INTO new_table`：结果被搬去新表，与源表不再对应。
    if select.into.is_some() {
        return false;
    }
    // 单表、无 JOIN：`FROM a, b` 与 `FROM a JOIN b` 都会给出多个 `from` 项 / 非空 joins。
    if select.from.len() != 1 || !select.from[0].joins.is_empty() {
        return false;
    }
    if !is_ungrouped(select) {
        return false;
    }
    select.projection.iter().all(is_plain_projection)
}

/// 有没有会破坏「一行结果 = 一行表数据」的修饰（去重 / 分组 / 聚合）。
fn is_ungrouped(select: &Select) -> bool {
    if select.distinct.is_some() || select.having.is_some() {
        return false;
    }
    // `GROUP BY ALL` 与 `GROUP BY <expr>…` 都算分组；只有空 `Expressions` 是没分组。
    matches!(&select.group_by, GroupByExpr::Expressions(exprs, _) if exprs.is_empty())
}

/// 投影项是否是「列名就是表列名」的形态（`*` / `表.*` / 裸列名）。
fn is_plain_projection(item: &SelectItem) -> bool {
    matches!(
        item,
        SelectItem::Wildcard(_)
            | SelectItem::QualifiedWildcard(..)
            | SelectItem::UnnamedExpr(Expr::Identifier(_))
            | SelectItem::ExprWithAlias {
                expr: Expr::Identifier(_),
                ..
            }
    )
}

/// 投影出来的列名是否**覆盖**了定位所需的主键列。
///
/// 这是 [`edit_target`] 判定不了的最后一环：`SELECT name FROM users` 单表、无别名、
/// 无分组，但结果里没有主键列 —— 改了没法定位回哪一行。`SELECT *` 的列名要等结果集
/// 回来才知道，所以这一步必须在运行时按实际列名做。
#[must_use]
pub fn covers_columns(projected: &[String], needed: &[String]) -> bool {
    needed
        .iter()
        .all(|column| projected.iter().any(|name| name == column))
}

/// 关键字大小写档位（对齐主流格式化工具的 keywordCase）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqlKeywordCase {
    /// 保留原样（dbx / sql-formatter 的默认档）。
    #[default]
    Preserve,
    /// 全部大写（本仓历史默认）。
    Upper,
    /// 全部小写。
    Lower,
}

/// SQL 格式化选项：主流格式化工具的最小可配集合。
///
/// 真源在设置页 `DbSettings::sql_format`（duck-app），这里只定义内核侧的取值域；
/// `Default` 即「历史行为」：关键字大写、空格缩进、宽度 2。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlFormatOptions {
    /// 关键字大小写。
    pub keyword_case: SqlKeywordCase,
    /// `true` 用 Tab 缩进，`false` 用空格。
    pub indent_tabs: bool,
    /// 空格缩进宽度（`indent_tabs == false` 时生效）。
    pub indent_width: u8,
}

impl Default for SqlFormatOptions {
    fn default() -> Self {
        SqlFormatOptions {
            keyword_case: SqlKeywordCase::Upper,
            indent_tabs: false,
            indent_width: 2,
        }
    }
}

/// 重新排版后的 SQL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedSql {
    /// 排版后的文本：多语句之间隔一个空行；原文以 `;` 收尾时结果也以 `;` 收尾
    /// （分号只跟着原文走，格式化不新增也不吞掉）。
    pub text: String,
    /// 解析不了、被**原样保留**的语句（界面要据此提示「有 N 段没动」）。
    pub kept_verbatim: Vec<String>,
}

/// 把一段 SQL 重新排版：按语句拆分 → 解析门 → [`sqlformat`] 主流式排版。
///
/// **解析门仍然保留**：解析得了的语句才交给 `sqlformat`（换行、缩进、关键字
/// 大小写都由 [`SqlFormatOptions`] 控制）；解析不了的**原样保留**并记进
/// [`FormattedSql::kept_verbatim`] —— 宁可不动，也不要把一段用户手上跑得通的
/// SQL 改成跑不了的（半截语句、方言特有语法都会走到这条路）。
#[must_use]
pub fn format_sql(sql: &str) -> FormattedSql {
    format_sql_with(sql, &SqlFormatOptions::default())
}

/// [`format_sql`] 的可配版本：设置页与工具条开关由此接入。
#[must_use]
pub fn format_sql_with(sql: &str, options: &SqlFormatOptions) -> FormattedSql {
    let sqlformat_options = sqlformat::FormatOptions {
        indent: if options.indent_tabs {
            sqlformat::Indent::Tabs
        } else {
            sqlformat::Indent::Spaces(options.indent_width)
        },
        uppercase: match options.keyword_case {
            SqlKeywordCase::Preserve => None,
            SqlKeywordCase::Upper => Some(true),
            SqlKeywordCase::Lower => Some(false),
        },
        ..sqlformat::FormatOptions::default()
    };
    let mut formatted = Vec::new();
    let mut kept_verbatim = Vec::new();
    for statement in split_statements(sql) {
        match parse_one(&statement) {
            // 解析门通过：交给 sqlformat 做主流式换行与缩进。
            Some(_) => {
                formatted.push(sqlformat::format(
                    &statement,
                    &sqlformat::QueryParams::None,
                    &sqlformat_options,
                ));
            }
            None => {
                kept_verbatim.push(statement.clone());
                formatted.push(statement);
            }
        }
    }
    // 段落之间那个 `;` 由 `join` 补：拆分会把每条语句的**尾分号**剥掉（见
    // [`split_statements`]），所以最后一条的分号没人补 —— 用户写了 `;` 就不能被
    // 格式化吃掉（单条语句时更明显：一格式化全文没分号了）。原文本来没写分号的
    // 照样不加，格式化不替用户发明标点。
    let mut text = formatted.join(";\n\n");
    if !text.is_empty() && sql.trim_end().ends_with(';') {
        text.push(';');
    }
    FormattedSql {
        text,
        kept_verbatim,
    }
}

/// 把一段 SQL 压成单行：**保留语句上方的注释块**，语句内部的 `-- 行注释` 与
/// `/* 块注释 */` 压掉，连续空白折叠成一个空格。
///
/// 为什么头部注释要留：那一块通常是「这段 SQL 是干什么的 / 谁写的 / 什么时候改的」，
/// 是作者写给下一个人看的东西。压缩的本意是省版面，不是替用户清理正文——把它一起
/// 吃掉，用户看到的就是「我写的说明被一个按钮删了」，而且撤销栈里翻回来还得靠手。
/// 语句**内部**的注释不同：它们夹在词法记号之间，压掉之后语义一字不变。
///
/// **字符串字面量与带引号的标识符原样保留**——`'a  b'` 里的双空格、`"col"` 的内容
/// 都是数据的一部分，压掉就是改语义。这里不需要理解语句本身：注释与空白是仅有的
/// 两类「怎么动都不改变语义」的成分，其余一律照抄。
#[must_use]
pub fn minify_sql(sql: &str) -> String {
    let (head, body) = split_leading_comments(sql);
    let minified = fold_sql_whitespace(body);
    match (head.is_empty(), minified.is_empty()) {
        // 没有头部注释（或只有前导缩进）：压缩结果就是正文的压缩结果。
        (true, _) => minified,
        // 整段都是注释：一个字都压不掉，原样（裁掉两端空白）返回。
        (false, true) => head,
        (false, false) => format!("{head}\n{minified}"),
    }
}

/// 切出 SQL **最前面**那段「只有空白与注释」的前导块：返回 `(头部, 余下正文)`。
///
/// 头部按**原文**保留（只裁两端空白）——里面的换行与缩进是作者排过的版，折叠掉反而
/// 难读；`-- 一` / `-- 二` 这种多行说明压成一行就失去了它作为「抬头」的样子。
///
/// 判据是「在遇到第一个真正的语句记号之前，至少吃到过一条注释」：只跳过空白**不算**
/// 头部注释块（`   SELECT …` 前面的缩进不值得单独占一行），所以 `head_end` 只在真的
/// 吃完一条注释之后才推进。
fn split_leading_comments(sql: &str) -> (String, &str) {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut head_end = 0;
    while i < len {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // `-- …` 行注释：吃到行尾（换行留给下一轮当空白跳过，头部因此保住换行）。
        if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            head_end = i;
            continue;
        }
        // `/* … */` 块注释：没有闭合就吃到结尾（与正文压缩同口径）。
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < len && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(len);
            head_end = i;
            continue;
        }
        break;
    }
    // `head_end` 永远落在 ASCII 边界上（行尾的 `\n` 之前、或 `*/` 之后、或 0），
    // 切分不会切开多字节字符。
    (sql[..head_end].trim().to_owned(), &sql[head_end..])
}

/// 正文压缩：去掉 `--` / `/* */` 注释、折叠连续空白（[`minify_sql`] 的实际干活那半）。
fn fold_sql_whitespace(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    // 引号态：`'…'` / `"…"` / `` `…` `` 三种成对出现，内部不折叠空白、不理会注释记号。
    let mut quote: Option<char> = None;
    let mut pending_space = false;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => {
                if pending_space && !matches!(c, ',' | ';' | ')' | ']') {
                    out.push(' ');
                }
                pending_space = false;
                quote = Some(c);
                out.push(c);
            }
            // 行注释：吃掉到行尾（不含换行——换行交给空白折叠统一处理）。
            '-' if chars.peek() == Some(&'-') => {
                chars.next();
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                pending_space = !out.is_empty();
            }
            // 块注释：吃掉到 `*/`；没有闭合就吃到结尾（半截注释同样不参与语义）。
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
                pending_space = !out.is_empty();
            }
            c if c.is_whitespace() => {
                pending_space = !out.is_empty();
            }
            c => {
                // 逗号 / 右括号 / 分号**前**不留空格、左括号**后**不留空格
                // （`f( a , b ) ;` → `f(a,b);`），其余保一个空格。
                let snug_before = matches!(c, ',' | ';' | ')' | ']');
                let snug_after = matches!(out.as_bytes().last(), Some(b'(' | b'['));
                if pending_space && !snug_before && !snug_after {
                    out.push(' ');
                }
                pending_space = false;
                out.push(c);
            }
        }
    }
    out
}

/// 「注释切换」做完之后的样子（编辑器「注释」按钮的回执素材）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineCommentToggle {
    /// 切换后的完整文本（行数不变，只有覆盖到的行被加 / 剥 `-- `）。
    pub text: String,
    /// `true` = 这次是**去掉**注释（首行已有 `--`）；`false` = 这次是**加上**。
    pub removed: bool,
    /// 被切换的行数（界面据此报「已注释 N 行」）。
    pub lines: usize,
}

/// 切换选区 `[start..end]` 覆盖到的**整行**的 `-- ` 行注释。
///
/// 方向以**首行**为准：首行已有注释就整段剥掉，没有就整段补上——混合状态时
/// 按一次只做一件事，行为可预期。字节越界按文本长度截住（编辑器给的选区
/// 理论上不会越界，但内核不赌调用方）。
#[must_use]
pub fn toggle_line_comments(sql: &str, range: Range<usize>) -> LineCommentToggle {
    let clamp = |at: usize| at.min(sql.len());
    // 选区边界落在多字节字符中间时退到字符边界，别把 UTF-8 切出 panic。
    let snap = |mut at: usize| {
        while at > 0 && !sql.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let start = snap(clamp(range.start));
    let end = snap(clamp(range.end)).max(start);
    let first_line = sql[..start].matches('\n').count();
    let last_line = sql[..end].matches('\n').count();
    let removed = sql
        .lines()
        .nth(first_line)
        .is_some_and(|line| line.trim_start().starts_with("--"));
    let mut text = sql
        .lines()
        .enumerate()
        .map(|(index, line)| {
            if index < first_line || index > last_line {
                return line.to_owned();
            }
            if removed {
                strip_line_comment(line)
            } else {
                format!("-- {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    // `lines()` 会把结尾的换行丢掉：编辑器文本以换行收尾时原样补回，别悄悄改行数。
    if sql.ends_with('\n') {
        text.push('\n');
    }
    LineCommentToggle {
        text,
        removed,
        lines: last_line - first_line + 1,
    }
}

/// 剥掉一行的 `-- ` 注释前缀（连同其后**一个**空格；缩进原样保留）。
fn strip_line_comment(line: &str) -> String {
    let indent = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent);
    match rest.strip_prefix("--") {
        Some(body) => format!("{indent}{}", body.strip_prefix(' ').unwrap_or(body)),
        None => line.to_owned(),
    }
}

/// 「区域注释切换」做完之后的样子（[`toggle_block_comment`] 的回执素材）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockCommentToggle {
    /// 切换后的完整文本。
    pub text: String,
    /// `true` = 这次是**剥掉**最外层 `/* */`；`false` = 这次是**包上**。
    pub removed: bool,
}

/// 切换选区的 `/* */` 区域注释：选区（裁掉两端空白后）已经是完整的块注释就剥掉
/// 最外层一对，否则包上。
///
/// 边界先收掉两端空白——换行、缩进不该被卷进注释记号里（选区拖到行首行尾是
/// 常态）。与 [`toggle_line_comments`] 一样按字节越界与多字节字符边界收口。
#[must_use]
pub fn toggle_block_comment(sql: &str, range: Range<usize>) -> BlockCommentToggle {
    let clamp = |at: usize| at.min(sql.len());
    let snap = |mut at: usize| {
        while at > 0 && !sql.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let start = snap(clamp(range.start));
    let end = snap(clamp(range.end)).max(start);
    let view = &sql[start..end];
    let body_start = start + (view.len() - view.trim_start().len());
    let body_end = start + view.trim_end().len();
    let body = &sql[body_start..body_end];
    let unwrapped = body.starts_with("/*") && body.ends_with("*/") && body.len() >= 4;
    if unwrapped {
        let mut inner = &body[2..body.len() - 2];
        inner = inner.strip_prefix(' ').unwrap_or(inner);
        inner = inner.strip_suffix(' ').unwrap_or(inner);
        BlockCommentToggle {
            text: format!("{}{inner}{}", &sql[..body_start], &sql[body_end..]),
            removed: true,
        }
    } else {
        BlockCommentToggle {
            text: format!("{}/*{body}*/{}", &sql[..body_start], &sql[body_end..]),
            removed: false,
        }
    }
}

/// 「注释」按钮这一下干了什么（界面据此给 toast 文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentAction {
    /// 给覆盖的行加上了 `-- `。
    LineAdded,
    /// 剥掉了覆盖行的 `-- `。
    LineRemoved,
    /// 用 `/* */` 包住了选区。
    BlockAdded,
    /// 剥掉了选区最外层的 `/* */`。
    BlockRemoved,
}

/// 统一入口 [`toggle_comments`] 的回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentToggle {
    /// 切换后的完整文本。
    pub text: String,
    /// 这一下实际做的动作。
    pub action: CommentAction,
    /// 行注释路径被切换的行数；块注释没有行数概念，恒为 0。
    pub lines: usize,
}

/// 编辑器「注释」按钮的统一入口：按选区现状决定这一下做什么。
///
/// **去掉优先**——再按一次总是往「还原」走：
/// 1. 首行已有 `-- ` → 剥掉覆盖行的行注释（[`toggle_line_comments`] 的语义）；
/// 2. 选区已是完整 `/* */` → 剥掉最外层（[`toggle_block_comment`] 的语义）；
/// 3. 选区跨多行（一条完整 SQL 的典型选法）→ 用 `/* */` 整段包住，比逐行打
///    `--` 更贴近「把这条语句整个停用」的意图；
/// 4. 选区落在单行内（含空选区 = 光标行）→ 给那一行加 `-- `。
#[must_use]
pub fn toggle_comments(sql: &str, range: Range<usize>) -> CommentToggle {
    let clamp = |at: usize| at.min(sql.len());
    let snap = |mut at: usize| {
        while at > 0 && !sql.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let start = snap(clamp(range.start));
    let end = snap(clamp(range.end)).max(start);
    let first_line = sql[..start].matches('\n').count();
    let last_line = sql[..end].matches('\n').count();
    let first_line_commented = sql
        .lines()
        .nth(first_line)
        .is_some_and(|line| line.trim_start().starts_with("--"));
    if first_line_commented {
        let toggled = toggle_line_comments(sql, range);
        return CommentToggle {
            text: toggled.text,
            action: CommentAction::LineRemoved,
            lines: toggled.lines,
        };
    }
    let body = &sql[start..end];
    let wrapped = body.trim().starts_with("/*") && body.trim().ends_with("*/");
    if wrapped || last_line > first_line {
        let toggled = toggle_block_comment(sql, range);
        return CommentToggle {
            text: toggled.text,
            action: if toggled.removed {
                CommentAction::BlockRemoved
            } else {
                CommentAction::BlockAdded
            },
            lines: 0,
        };
    }
    let toggled = toggle_line_comments(sql, range);
    CommentToggle {
        text: toggled.text,
        action: CommentAction::LineAdded,
        lines: toggled.lines,
    }
}

/// 一条语句「出不了执行计划」的原因。文案由界面层决定，内核只给分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainSkip {
    /// 编辑器里不是单条语句（空、多条，或拆出来是空的）。
    NotASingleStatement,
    /// 解析不了，或这个种类本来就没有执行计划（事务控制、`SET`、DDL 等）。
    NoPlanForThisStatement,
}

/// 把一段 SQL 包成一条 `EXPLAIN <stmt>`（SQLite / PostgreSQL / MySQL 的语法一致）。
///
/// 只接受**单条**出得了计划的语句：`EXPLAIN BEGIN` / `EXPLAIN DROP TABLE` 都不是合法
/// 用法。与其把它丢给数据库报一句方言各异的语法错，不如在闸门这一层先分类清楚。
pub fn explain_sql(sql: &str) -> Result<String, ExplainSkip> {
    explain_target(sql).map(|parsed| format!("EXPLAIN {parsed}"))
}

/// 按**引擎**包一条能拿到结构化输出的执行计划语句。
///
/// 与 [`explain_sql`] 的分工：那条给的是"各引擎都认的通用形态"（`EXPLAIN <stmt>`，
/// 输出是一坨文本）；这条按引擎挑出**能结构化**的形态，交给 [`crate::plan::parse_plan`]
/// 去解析：
///
/// - SQLite：`EXPLAIN QUERY PLAN`（四列 `id/parent/detail`，层级是真的）
/// - PostgreSQL：`EXPLAIN (FORMAT JSON)`（`Plan` / `Plans` 嵌套）
/// - MySQL：`EXPLAIN FORMAT=JSON`（`query_block` / `table` 嵌套）
/// - ClickHouse：`EXPLAIN`（缩进文本；`json = 1` 只有较新版本认，不拿它赌）
///
/// 其余四条引擎（SQL Server / MongoDB / Elasticsearch / agent 档）没有这些形态，退回
/// 通用写法——由数据库自己报错，胜过在这里编一个不存在的语法。
///
/// ⚠️ 语法正确性由**语句本身**保证（校验与 [`explain_sql`] 完全同一条路：单条 + 属于
/// 读 / 写族），这里只改前缀。
pub fn explain_sql_for(kind: DatabaseKind, sql: &str) -> Result<String, ExplainSkip> {
    let parsed = explain_target(sql)?;
    Ok(match kind {
        DatabaseKind::Sqlite => format!("EXPLAIN QUERY PLAN {parsed}"),
        DatabaseKind::Postgres => format!("EXPLAIN (FORMAT JSON) {parsed}"),
        DatabaseKind::Mysql => format!("EXPLAIN FORMAT=JSON {parsed}"),
        // agent 档退回通用 `EXPLAIN`：结构化形态各家方言差异太大（Oracle 的
        // `DBMS_XPLAN`、Hive 的 `EXPLAIN FORMATTED`、DB2 的 `EXPLAIN PLAN FOR`），
        // 按 kind 挑不出来——而 `EXPLAIN <stmt>` 是它们**多数**都认的那一种。
        // 解析器（`crate::plan::parse_plan`）认不出这种一坨文本的输出，退回文本展示。
        DatabaseKind::ClickHouse
        | DatabaseKind::SqlServer
        | DatabaseKind::Mongo
        | DatabaseKind::Elasticsearch
        | DatabaseKind::Agent => format!("EXPLAIN {parsed}"),
    })
}

/// 归一化输出缓冲：负责「连续空白只留一个」与「引号外一律折叠为小写」两条规则。
///
/// 抽成一个小结构体而不是在扫描循环里内联：吐字符的地方有六七处，每处都手写一遍
/// 「先补空格再收字符」迟早会漏一处，而漏了只表现为「指纹偶尔对不上」——最难查的那种。
struct FingerprintBuilder {
    out: String,
    /// 上一个被扫描到的字符是空白 / 注释结束：下一个字符前要先补一个空格。
    pending_space: bool,
}

impl FingerprintBuilder {
    fn new(capacity: usize) -> Self {
        Self {
            out: String::with_capacity(capacity),
            pending_space: false,
        }
    }

    /// 原样收（引号**内**的标识符：`"MyCol"` 与 `"mycol"` 是两列，不能合并）。
    fn verbatim(&mut self, c: char) {
        self.flush_space();
        self.out.push(c);
    }

    /// 折叠大小写后收（引号外的标识符与运算符：关键字大小写是风格，不是语义）。
    fn folded(&mut self, c: char) {
        self.flush_space();
        for lower in c.to_lowercase() {
            self.out.push(lower);
        }
    }

    /// 记一个「这里是空白」的信号（不立刻吐空格，等真有字符要吐时再补）。
    fn note_space(&mut self) {
        self.pending_space = true;
    }

    fn flush_space(&mut self) {
        if self.pending_space && !self.out.is_empty() {
            self.out.push(' ');
        }
        self.pending_space = false;
    }

    /// 收工：去掉尾部空白（`trim_end` 不能直接用在 `String` 的 `self` 上，这里显式做）。
    fn finish(mut self) -> String {
        self.flush_space();
        self.out.trim_end().to_owned()
    }
}

/// 语句的**归一化文本**：把「每次跑都不一样的部分」抹平，留下语句的形状。
///
/// 抹掉的东西：字面量（`'…'`）、数字、注释、多余空白、以及引号外的大小写。
/// 留下的东西：**双引号 / 反引号里的标识符原样保留**（`"MyCol"` 与 `"mycol"` 是两列，
/// 不能合并），以及运算符与括号结构。
///
/// 为什么值得做：审计里「同一类语句跑了多少次」比「每条 SQL 的原文」有用得多，
/// 而把字面量换成 `?` 之后，`WHERE id = 1` 与 `WHERE id = 2` 才算同一类。
///
/// ⚠️ **刻意不做的三件事**：① 不做语法解析（审计要能记下写坏的 SQL，解析器会整段失败）；
/// ② 不展开 `IN (1,2,3)` 与 `IN (1,2)` 的差别（元素个数留在形状里 —— 它们确实是两种计划）；
/// ③ 双引号按**标识符**而不是字符串处理（PG 里 `"x"` 是列名；MySQL 的字符串一律走单引号）。
#[must_use]
pub fn normalize_for_fingerprint(sql: &str) -> String {
    /// 扫描状态。`Single`/`Double`/`Backtick` 是三种引号，`Line`/`Block` 是两类注释，
    /// `Number` 是数字字面量（连续吃掉，整体只记一个 `?`）。
    #[derive(PartialEq)]
    enum In {
        None,
        Single,
        Double,
        Backtick,
        Line,
        Block,
        Number,
    }

    let mut built = FingerprintBuilder::new(sql.len());
    let mut chars = sql.chars().peekable();
    let mut state = In::None;

    while let Some(c) = chars.next() {
        match state {
            In::None => match c {
                '\'' => {
                    built.folded('?');
                    state = In::Single;
                }
                // 双引号 / 反引号里的内容**原样收**（标识符），所以连引号一起吐。
                '"' | '`' => {
                    built.verbatim(c);
                    state = if c == '"' { In::Double } else { In::Backtick };
                }
                '-' if chars.peek() == Some(&'-') => {
                    chars.next();
                    state = In::Line;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    state = In::Block;
                }
                c if c.is_ascii_digit() => {
                    built.folded('?');
                    state = In::Number;
                }
                c if c.is_whitespace() => built.note_space(),
                c => built.folded(c),
            },
            In::Single => {
                // `''` 是转义的单引号，不是字符串结束。
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        state = In::None;
                    }
                }
            }
            In::Double | In::Backtick => {
                built.verbatim(c);
                let closer = if state == In::Double { '"' } else { '`' };
                if c == closer {
                    // 连着两个同样的引号 = 转义，仍在引号内。
                    if chars.peek() == Some(&closer) {
                        if let Some(next) = chars.next() {
                            built.verbatim(next);
                        }
                    } else {
                        state = In::None;
                    }
                }
            }
            In::Line => {
                if c == '\n' {
                    // 换行结束注释，也是**词与词之间的分隔**：`a--x\nb` 是 `a b`。
                    built.note_space();
                    state = In::None;
                }
            }
            In::Block => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    // 注释替换成一个空白，免得把两边粘成一个词（`a/*x*/b` 是 `a b`）。
                    built.note_space();
                    state = In::None;
                }
            }
            In::Number => {
                if c.is_ascii_alphanumeric() || c == '.' {
                    // `1e5` / `1.5` / `0x1f` 都还是一个数字，继续吃。
                } else if c.is_whitespace() {
                    built.note_space();
                    state = In::None;
                } else {
                    // 数字后面紧跟运算符（`1+1` → `?+?`）：运算符照常收。
                    state = In::None;
                    built.folded(c);
                }
            }
        }
    }
    built.finish()
}

/// 语句指纹：归一化文本的 64 位 FNV-1a，输出 16 位十六进制。
///
/// 空串（或全是注释 / 空白）得空串 —— 「没有语句形状」不该与「某个形状」撞成一个值。
#[must_use]
pub fn fingerprint(sql: &str) -> String {
    let normalized = normalize_for_fingerprint(sql);
    if normalized.is_empty() {
        return String::new();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// [`explain_sql`] / [`explain_sql_for`] 共用的闸门：单条 + 出得了计划的语句。
fn explain_target(sql: &str) -> Result<Statement, ExplainSkip> {
    let statements = split_statements(sql);
    if statements.len() != 1 {
        return Err(ExplainSkip::NotASingleStatement);
    }
    let statement = &statements[0];
    let Some(parsed) = parse_one(statement) else {
        return Err(ExplainSkip::NoPlanForThisStatement);
    };
    match classify(statement) {
        StmtKind::Read | StmtKind::Write => Ok(parsed),
        StmtKind::Ddl | StmtKind::Other => Err(ExplainSkip::NoPlanForThisStatement),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 归一化：抹掉字面量 / 数字 / 注释 / 空白 / 引号外大小写，**保留**标识符原文。
    #[test]
    fn fingerprint_normalization_keeps_the_shape_only() {
        assert_eq!(
            normalize_for_fingerprint("SELECT * FROM t WHERE id = 1"),
            "select * from t where id = ?"
        );
        // 字符串字面量（含转义的单引号）整体变一个 `?`。
        assert_eq!(
            normalize_for_fingerprint("UPDATE t SET a = 'it''s 1' WHERE id = 42"),
            "update t set a = ? where id = ?"
        );
        // 缩进 / 换行 / 块注释不影响形状。
        assert_eq!(
            normalize_for_fingerprint("SELECT\n    a,\n    b\n  FROM t /* 说明 */ WHERE a = 3"),
            "select a, b from t where a = ?"
        );
        // 行注释到行尾为止，且后面的词还在。
        assert_eq!(
            normalize_for_fingerprint("SELECT 1 -- 随手记\nFROM t"),
            "select ? from t"
        );
        // 注释把两边隔开（`a/*x*/b` 是 `a b`，不是 `ab`）。
        assert_eq!(normalize_for_fingerprint("a/*x*/b"), "a b");
        assert_eq!(normalize_for_fingerprint("a--x\nb"), "a b");
        // 数字字面量的各种写法都吃成一个 `?`。
        assert_eq!(
            normalize_for_fingerprint("SELECT 1e5, 1.5, 0x1f"),
            "select ?, ?, ?"
        );
        // 数字紧跟运算符：运算符留在形状里。
        assert_eq!(normalize_for_fingerprint("SELECT 1+2"), "select ?+?");
        // 标识符原样保留大小写（两列就是两列），关键字则折叠。
        assert_eq!(
            normalize_for_fingerprint("SELECT \"MyCol\" FROM \"MyTable\""),
            "select \"MyCol\" from \"MyTable\""
        );
        assert_eq!(
            normalize_for_fingerprint("SELECT `MyCol`"),
            "select `MyCol`"
        );
        // 转义的双引号不结束标识符。
        assert_eq!(
            normalize_for_fingerprint("SELECT \"a\"\"b\" FROM t"),
            "select \"a\"\"b\" from t"
        );
        // 引号里的内容不会被当成分隔符 / 注释。
        assert_eq!(
            normalize_for_fingerprint("SELECT 'a -- b', 'c /* d */' FROM t"),
            "select ?, ? from t"
        );
        assert_eq!(normalize_for_fingerprint("   \n  "), "");
        assert_eq!(normalize_for_fingerprint("-- 只有注释"), "");
    }

    /// 指纹：同一类语句同值、结构不同则不同、空输入给空串。
    #[test]
    fn fingerprints_group_the_same_statement_shape() {
        let select_one = fingerprint("SELECT * FROM t WHERE id = 1");
        assert_eq!(select_one, fingerprint("select * from t where id = 2"));
        assert_eq!(
            select_one,
            fingerprint("\n  SELECT *\n  FROM t\n  WHERE id = 999999  ")
        );
        assert_eq!(select_one.len(), 16, "指纹是 16 位十六进制");
        assert!(select_one.chars().all(|c| c.is_ascii_hexdigit()));

        // 结构不同 → 不同（表格不同、列不同、语句种类不同）。
        assert_ne!(select_one, fingerprint("SELECT * FROM other WHERE id = 1"));
        assert_ne!(select_one, fingerprint("SELECT * FROM t WHERE name = 'x'"));
        assert_ne!(select_one, fingerprint("DELETE FROM t WHERE id = 1"));
        // 标识符大小写是**两个不同的对象**，不该被并成一条。
        assert_ne!(
            fingerprint("SELECT \"A\" FROM t"),
            fingerprint("SELECT \"a\" FROM t")
        );
        // `IN` 的元素个数留在形状里（确实是两种计划）。
        assert_ne!(
            fingerprint("SELECT * FROM t WHERE id IN (1,2,3)"),
            fingerprint("SELECT * FROM t WHERE id IN (1,2)")
        );

        // 空 / 只有注释 → 空串（「没有形状」不是一个形状）。
        assert_eq!(fingerprint(""), "");
        assert_eq!(fingerprint("/* nothing */"), "");
    }

    /// 事务状态判定：三引擎的开关拼法、前置注释、大小写、以及「无关语句」。
    #[test]
    fn tx_effect_detection() {
        assert_eq!(tx_effect("BEGIN"), Some(TxEffect::Begin));
        assert_eq!(tx_effect("begin transaction"), Some(TxEffect::Begin));
        assert_eq!(
            tx_effect("START TRANSACTION WITH CONSISTENT SNAPSHOT"),
            Some(TxEffect::Begin)
        );
        assert_eq!(tx_effect("COMMIT"), Some(TxEffect::End));
        assert_eq!(tx_effect("  rollback ;"), Some(TxEffect::End));
        assert_eq!(tx_effect("END"), Some(TxEffect::End));
        // 前置注释（`split_statements` 会把它留在语句头部）。
        assert_eq!(tx_effect("-- 收尾\nCOMMIT"), Some(TxEffect::End));
        assert_eq!(tx_effect("/* 块注释 */ BEGIN"), Some(TxEffect::Begin));
        assert_eq!(tx_effect("-- 只有注释"), None);
        // 无关语句：含「以事务词开头」的干扰项。
        assert_eq!(tx_effect("SELECT 1"), None);
        assert_eq!(tx_effect("SAVEPOINT s1"), None);
        assert_eq!(tx_effect("UPDATE t SET a = 'COMMIT'"), None);
    }

    /// 控制语句与状态判定必须**闭环**：造出来的语句一定要被 [`tx_effect`] 认出来。
    /// 认不出来 = 点一下开关后状态纹丝不动（界面卡在旧态），这是这条路径上唯一的
    /// 致命失败，值得单独钉死。
    #[test]
    fn tx_control_statements_round_trip_through_tx_effect() {
        for kind in [
            DatabaseKind::Sqlite,
            DatabaseKind::Postgres,
            DatabaseKind::Mysql,
            DatabaseKind::ClickHouse,
            DatabaseKind::SqlServer,
            DatabaseKind::Mongo,
            DatabaseKind::Elasticsearch,
        ] {
            for control in [TxControl::Begin, TxControl::Commit, TxControl::Rollback] {
                let statement = control.statement(kind);
                assert_eq!(
                    tx_effect(statement),
                    Some(control.effect()),
                    "{kind:?} 的 {control:?} 造出 `{statement}` 却认不出来"
                );
            }
        }
    }

    /// 方言：只有「开启」按引擎分叉（MySQL 用 `START TRANSACTION`），提交 / 回滚三家一致。
    #[test]
    fn only_begin_is_dialect_specific() {
        assert_eq!(
            TxControl::Begin.statement(DatabaseKind::Mysql),
            "START TRANSACTION"
        );
        assert_eq!(TxControl::Begin.statement(DatabaseKind::Sqlite), "BEGIN");
        assert_eq!(TxControl::Begin.statement(DatabaseKind::Postgres), "BEGIN");
        for kind in [
            DatabaseKind::Sqlite,
            DatabaseKind::Postgres,
            DatabaseKind::Mysql,
        ] {
            assert_eq!(TxControl::Commit.statement(kind), "COMMIT");
            assert_eq!(TxControl::Rollback.statement(kind), "ROLLBACK");
        }
    }

    /// 拆分：普通分号、字符串里的分号、注释、结尾无分号。
    #[test]
    fn split_handles_quotes_and_comments() {
        let sql = "SELECT 1; -- 说明; 还有分号\nUPDATE t SET a = 'x;y' WHERE id = 1;\n/* 块;注释 */ INSERT INTO t(a) VALUES ('it''s'); SELECT 2";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "SELECT 1");
        assert!(parts[1].contains("'x;y'"));
        // 块注释保留在语句头部（注释跟着语句走）。
        assert!(parts[2].contains("INSERT INTO t"));
        assert_eq!(parts[3], "SELECT 2");
    }

    /// 带位置切分：文本与 [`split_statements`] 同口径，偏移落在语句首字符。
    #[test]
    fn split_spans_match_texts_and_offsets() {
        let sql =
            "\n  SELECT 1;\n\n  update t set a = 'x;y' where id = 1;\n\n-- 注释; 分号\nSELECT 2";
        let spans = split_statement_spans(sql);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "SELECT 1");
        assert_eq!(&sql[spans[0].start..spans[0].start + 8], "SELECT 1");
        assert_eq!(spans[1].text, "update t set a = 'x;y' where id = 1");
        assert_eq!(spans[2].text, "-- 注释; 分号\nSELECT 2");
        // 偏移必须能对回原文（截出去的片段以语句文本开头）。
        for span in &spans {
            let tail = &sql[span.start..];
            assert!(tail.starts_with(&span.text) || tail.starts_with("--"));
        }
        // 与无位置版本互证：文本清单必须完全一致。
        let plain = split_statements(sql);
        let from_spans: Vec<String> = spans.into_iter().map(|span| span.text).collect();
        assert_eq!(plain, from_spans);
    }

    /// 切分器升级（A1）：dollar-quote / `DELIMITER` / 例程体 / `GO` —— 四类
    /// 「dump 与真实脚本里的常态」，老实现全都在这些地方切错。
    #[test]
    fn split_understands_dumps() {
        // PG dollar-quote：体内的 `;` 不是分隔符（`$$` 与 `$tag$` 两种写法）。
        assert_eq!(
            split_statements("SELECT $$a;b$$; SELECT 2"),
            vec!["SELECT $$a;b$$", "SELECT 2"]
        );
        let parts = split_statements(
            "CREATE FUNCTION f() RETURNS int AS $body$ BEGIN SELECT 1; END $body$ LANGUAGE plpgsql; SELECT 3",
        );
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].contains("SELECT 1;"), "{}", parts[0]);
        assert_eq!(parts[1], "SELECT 3");
        // 参数占位符不是 dollar-quote（`$1` 没有收尾的 `$`，认了就吞掉后面整段）。
        assert_eq!(
            split_statements("SELECT $1; SELECT $2"),
            vec!["SELECT $1", "SELECT $2"]
        );

        // MySQL dump：`DELIMITER` 改分隔符后例程体内的 `;` 不切；指令行本身不是语句。
        let dump = "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND$$\nDELIMITER ;\nSELECT 3;";
        let parts = split_statements(dump);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(
            parts[0].contains("BEGIN\n  SELECT 1;\n  SELECT 2;\nEND"),
            "{}",
            parts[0]
        );
        assert_eq!(parts[1], "SELECT 3");

        // 没写 `DELIMITER` 的例程体照样整体不切（靠 `BEGIN…END` 层数）。
        let parts = split_statements(
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN INSERT INTO y VALUES (1); INSERT INTO y VALUES (2); END; SELECT 9",
        );
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].ends_with("END"), "{}", parts[0]);
        assert_eq!(parts[1], "SELECT 9");

        // `END IF` / `END WHILE` 是**块**尾：不能把例程层数提前清零（否则从中间切开）。
        let parts = split_statements(
            "CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; SELECT 2; END; SELECT 9",
        );
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].contains("SELECT 2;"), "{}", parts[0]);
        assert_eq!(parts[1], "SELECT 9");

        // SQL Server 的 `GO`：断批，本身不是语句；行首空白也认。
        assert_eq!(
            split_statements("SELECT 1\nGO\nSELECT 2\nGO\n"),
            vec!["SELECT 1", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT 1\n  GO  \nSELECT 2"),
            vec!["SELECT 1", "SELECT 2"]
        );
        // 不在行首 / 只是名字里的 `go` 不是分隔符。
        assert_eq!(
            split_statements("SELECT go FROM t"),
            vec!["SELECT go FROM t"]
        );
    }

    /// 流式扫描器与整段切分**逐条相同**——按各种块宽喂进去，产出必须一致。
    ///
    /// 这是「执行 SQL 文件」那条路的正确性底座：文件按 256KB 读进来、块边界落在
    /// 任何位置（哪怕落在一个多字节字符或一句话中间），都不许把语句切歪。
    #[test]
    fn streaming_scanner_matches_whole_text_split() {
        let samples = [
            "SELECT 1; SELECT 2;",
            "SELECT 1",
            "",
            "   \n  ",
            "-- 只有注释\n/* 块注释 */\n",
            "SELECT 'a;b'; SELECT $$c;d$$;",
            "SELECT 'it''s'; SELECT \"a;b\"; SELECT `c;d`;",
            "SELECT $1; SELECT $2;",
            "SELECT 1;\nGO\nSELECT 2\nGO\n",
            "SELECT 1; GO",
            "SELECT 1\n  GO  \nSELECT 2",
            "SELECT go FROM t",
            "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND$$\nDELIMITER ;\nSELECT 3;",
            "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 'a;b';\nEND$$\nSELECT 3;",
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN INSERT INTO y VALUES (1); INSERT INTO y VALUES (2); END; SELECT 9",
            "CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; SELECT 2; END; SELECT 9",
            "INSERT INTO t VALUES (1);\n-- 抬头\nGO\nSELECT 2;",
            "SELECT 1 -- 行内注释\nFROM t;\nSELECT 2;",
            "SELECT '未收尾; SELECT 2;",
        ];
        for sql in samples {
            let expected = split_statements(sql);
            for width in [1usize, 2, 3, 5, 7, 16, 64, 4096] {
                let got = scan_in_chunks(sql, width);
                assert_eq!(got, expected, "块宽 {width}，原文 {sql:?}");
            }
        }
    }

    /// 按**字符**切块（不能按字节切：那会切碎多字节字符，反而不是「文件读进来」的情形）。
    fn scan_in_chunks(sql: &str, width: usize) -> Vec<String> {
        let mut scanner = StatementScanner::new();
        let mut out = Vec::new();
        let mut buffer = String::new();
        for c in sql.chars() {
            buffer.push(c);
            if buffer.chars().count() >= width {
                out.extend(scanner.push(&buffer));
                buffer.clear();
            }
        }
        if !buffer.is_empty() {
            out.extend(scanner.push(&buffer));
        }
        out.extend(scanner.finish());
        out
    }

    /// 只有注释 / 空白的文本不是语句（执行文件时会被丢掉的正是这类）。
    #[test]
    fn blank_and_comment_only_text_is_not_a_statement() {
        assert!(is_blank_or_comment(""));
        assert!(is_blank_or_comment("   \n\t "));
        assert!(is_blank_or_comment("-- 抬头"));
        assert!(is_blank_or_comment("/* 块注释 */"));
        assert!(!is_blank_or_comment("SELECT 1"));
        assert!(!is_blank_or_comment("-- 注释\nSELECT 1"));
    }

    /// 分类：读 / 写 / DDL / 无法判定。
    #[test]
    fn classify_kinds() {
        assert_eq!(classify("SELECT * FROM t"), StmtKind::Read);
        assert_eq!(classify("INSERT INTO t VALUES (1)"), StmtKind::Write);
        assert_eq!(classify("UPDATE t SET a = 1 WHERE id = 1"), StmtKind::Write);
        assert_eq!(classify("CREATE TABLE t(id INT)"), StmtKind::Ddl);
        assert_eq!(classify("PRAGMA table_info(t)"), StmtKind::Read);
        assert_eq!(classify("SHOW TABLES"), StmtKind::Read);
    }

    /// 只读证明：SELECT 放行；写与解析失败都不放行。
    #[test]
    fn read_only_proof() {
        assert!(prove_read_only("SELECT * FROM t WHERE id = 1"));
        assert!(prove_read_only("EXPLAIN SELECT 1"));
        assert!(prove_read_only("SHOW TABLES"));
        assert!(!prove_read_only("UPDATE t SET a = 1"));
        assert!(!prove_read_only("DROP TABLE t"));
        // 解析失败（写一半的语句）必须不给证明。
        assert!(!prove_read_only("SELEC * FORM t"));
    }

    /// 四类「写形状的查询」绝不能判读：数据修改型 CTE、`SELECT … INTO`、
    /// `EXPLAIN ANALYZE <写>`、赋值型 PRAGMA / `INTO OUTFILE`。
    #[test]
    fn write_shaped_queries_are_never_read() {
        // 第 1 类：数据修改型 CTE / 解析失败的 WITH / 派生表里嵌 DML。
        for stmt in [
            "WITH x AS (SELECT 1) DELETE FROM users",
            "WITH x AS (DELETE FROM users RETURNING *) SELECT * FROM x",
            "WITH x AS (INSERT INTO users VALUES (1) RETURNING *) SELECT * FROM x",
            "WITH x AS (UPDATE users SET a = 1 RETURNING *) SELECT * FROM x",
            "WITH x AS (SELECT 1) INSERT INTO users VALUES (1)",
            "WITH x AS (SELECT 1) UPDATE users SET a = 1",
            "WITH x AS (SELEC bad) DELETE FROM users",
            "SELECT * FROM (INSERT INTO users VALUES (1) RETURNING *) x",
            "SELECT * FROM (DELETE FROM users RETURNING *) x",
        ] {
            assert_ne!(classify(stmt), StmtKind::Read, "不得判 Read：{stmt}");
            assert!(!prove_read_only(stmt), "不得给只读证明：{stmt}");
        }
        // 第 2 类：`SELECT … INTO`。
        assert_ne!(classify("SELECT * INTO newtbl FROM users"), StmtKind::Read);
        assert!(!prove_read_only("SELECT * INTO newtbl FROM users"));
        // 第 3 类：内层为写的 EXPLAIN 一律不放行（`ANALYZE` 会真的执行内层）。
        assert_ne!(
            classify("EXPLAIN ANALYZE DELETE FROM users"),
            StmtKind::Read
        );
        assert!(!prove_read_only("EXPLAIN ANALYZE DELETE FROM users"));
        assert!(!prove_read_only("EXPLAIN DELETE FROM users"));
        // 第 4 类：赋值型 PRAGMA / `INTO OUTFILE`。
        assert_ne!(classify("PRAGMA user_version = 5"), StmtKind::Read);
        assert_ne!(classify("PRAGMA journal_mode = WAL"), StmtKind::Read);
        assert_ne!(classify("PRAGMA journal_mode(WAL)"), StmtKind::Read);
        assert_ne!(
            classify("SELECT * FROM users INTO OUTFILE '/tmp/x'"),
            StmtKind::Read
        );
        assert!(!prove_read_only(
            "SELECT * FROM users INTO OUTFILE '/tmp/x'"
        ));
    }

    /// 安全形态不回归：普通只读查询仍判 Read（可证明只读的仍是子集）。
    #[test]
    fn plain_reads_stay_read() {
        for stmt in [
            "SELECT * FROM users",
            "select id, name from users where id = 1 order by id limit 10",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "EXPLAIN SELECT 1",
            "EXPLAIN ANALYZE SELECT 1",
            "SHOW TABLES",
            "PRAGMA table_info(t)",
            "PRAGMA user_version",
            "-- 前缀注释\nSELECT * FROM users",
        ] {
            assert_eq!(classify(stmt), StmtKind::Read, "应判 Read：{stmt}");
        }
        // `PRAGMA table_info(t)` 参数是标识符、解析器不认，故不给证明，但分类仍是 Read。
        for stmt in [
            "SELECT * FROM users",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "EXPLAIN SELECT 1",
            "EXPLAIN ANALYZE SELECT 1",
            "SHOW TABLES",
        ] {
            assert!(prove_read_only(stmt), "应给只读证明：{stmt}");
        }
    }

    /// 自动执行闸门（纯函数）：恰好一条 + 可证明只读才为真。
    #[test]
    fn allow_auto_execute_gate() {
        // 放行：单条只读（含大小写 / 前置注释）。
        assert!(allow_auto_execute("SELECT * FROM users"));
        assert!(allow_auto_execute("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(allow_auto_execute("  SELECT 1  "));
        assert!(allow_auto_execute("-- 说明\nselect * from t"));
        // 不放行：四类越权构造 + 普通写。
        for stmt in [
            "WITH x AS (SELECT 1) DELETE FROM users",
            "WITH x AS (INSERT INTO users VALUES (1) RETURNING *) SELECT * FROM x",
            "SELECT * INTO newtbl FROM users",
            "EXPLAIN ANALYZE DELETE FROM users",
            "PRAGMA user_version = 5",
            "SELECT * FROM users INTO OUTFILE '/tmp/x'",
            "INSERT INTO users VALUES (1)",
            "UPDATE users SET a = 1 WHERE id = 1",
            "DROP TABLE users",
            "-- 说明\ninsert into t values (1)",
        ] {
            assert!(!allow_auto_execute(stmt), "不得自动执行：{stmt}");
        }
        // 不放行：多条（哪怕每条都只读）/ 空 / 只有注释。
        assert!(!allow_auto_execute("SELECT 1; SELECT 2"));
        assert!(!allow_auto_execute(""));
        assert!(!allow_auto_execute("   "));
        assert!(!allow_auto_execute("-- 只有注释"));
    }

    /// RES-1：解析失败时的兜底要**扫描整条**的词法标记——CTE 包裹的全表删 / 全表改
    /// 必须判 `Dangerous`（触发「执行危险 SQL」确认），不能只落 `Write`。
    #[test]
    fn fallback_scan_flags_cte_wrapped_writes() {
        // 解析失败的 WITH + DML：词法兜底判危险（无 WHERE）。
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) DELETE FROM users"),
            SqlRisk::Dangerous
        );
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) UPDATE users SET a = 1"),
            SqlRisk::Dangerous
        );
        assert_eq!(
            risk_of("WITH x AS (DELETE FROM users RETURNING *) SELECT * FROM x"),
            SqlRisk::Dangerous
        );
        // 带 WHERE 的 CTE 包裹 DML：写，但不升危险。
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) DELETE FROM users WHERE id = 1"),
            SqlRisk::Write
        );
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) UPDATE users SET a = 1 WHERE id = 2"),
            SqlRisk::Write
        );
        // 字符串里的 `delete` 不算关键字（不因它多出一条 DML）。
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) DELETE FROM users WHERE msg = 'delete'"),
            SqlRisk::Write
        );
        // 对照：裸 DML 本来就是 Dangerous（不能被改动破坏）。
        assert_eq!(risk_of("DELETE FROM users"), SqlRisk::Dangerous);
        assert_eq!(risk_of("UPDATE users SET a = 1"), SqlRisk::Dangerous);
        // 不误判：字面量 / 注释里的写关键字不触发。
        assert_eq!(risk_of("SELECT 'delete' FROM t"), SqlRisk::ReadOnly);
        assert_eq!(risk_of("SELECT 'DROP TABLE t' FROM t"), SqlRisk::ReadOnly);
        assert_eq!(risk_of("-- DELETE FROM users\nSELECT 1"), SqlRisk::ReadOnly);
    }

    /// RES-2：命中已知副作用函数（`nextval` / `setval` / `dblink*` / 后端控制 …）的读
    /// **不自动执行**；名字不带括号（列名 / 表名）、或只在字面量里出现，不算命中。
    #[test]
    fn side_effect_functions_are_not_auto_executed() {
        for stmt in [
            "SELECT nextval('s')",
            "SELECT setval('s', 1)",
            "SELECT dblink('host=h', 'SELECT 1')",
            "SELECT pg_terminate_backend(123)",
            "SELECT pg_cancel_backend(123)",
            "SELECT set_config('a', 'b', false)",
            "SELECT lo_unlink(1)",
            "SELECT pg_advisory_lock(1)",
        ] {
            assert!(has_write_side_effects(stmt), "应识别副作用函数：{stmt}");
            assert!(!allow_auto_execute(stmt), "不得自动执行：{stmt}");
        }
        // 名字不带括号（当列名 / 表名用）不算函数调用。
        assert!(!has_write_side_effects("SELECT nextval FROM t"));
        // 只在字面量里出现，不算调用。
        assert!(!has_write_side_effects("SELECT 'nextval(1)' FROM t"));
        // 分类仍判读（副作用不改变语句种类），只是闸门不放行。
        assert_eq!(classify("SELECT nextval('s')"), StmtKind::Read);
        assert!(prove_read_only("SELECT nextval('s')"));
        // 普通聚合读照常放行。
        assert!(allow_auto_execute("SELECT count(*) FROM t"));
    }

    /// RES-3：可解析的**无值** PRAGMA 也要过白名单——维护型（`wal_checkpoint` /
    /// `optimize` / `shrink_memory`）无值也写，不给只读证明。
    #[test]
    fn bare_pragmas_need_the_whitelist() {
        assert!(prove_read_only("PRAGMA user_version"));
        assert!(prove_read_only("PRAGMA journal_mode"));
        assert_eq!(classify("PRAGMA user_version"), StmtKind::Read);
        assert_eq!(classify("PRAGMA journal_mode"), StmtKind::Read);
        for stmt in [
            "PRAGMA wal_checkpoint",
            "PRAGMA optimize",
            "PRAGMA shrink_memory",
        ] {
            assert!(
                !prove_read_only(stmt),
                "无值维护型 PRAGMA 不得判只读：{stmt}"
            );
            assert_eq!(classify(stmt), StmtKind::Write, "应判写：{stmt}");
            assert!(!allow_auto_execute(stmt), "不得自动执行：{stmt}");
        }
        assert!(allow_auto_execute("PRAGMA user_version"));
        // 带参的取信息型（解析器不认、走兜底白名单）仍判读。
        assert_eq!(classify("PRAGMA table_info(t)"), StmtKind::Read);
    }

    /// RES-4：`SELECT … FOR UPDATE / SHARE`（含 `FOR NO KEY UPDATE` / `FOR KEY SHARE`）
    /// 取行锁、会阻塞他人——判读可以，但**不自动执行**。
    #[test]
    fn locking_selects_are_not_auto_executed() {
        // 可解析的两种：分类仍是 Read、仍给只读证明，只是闸门不放行。
        assert_eq!(classify("SELECT * FROM t FOR UPDATE"), StmtKind::Read);
        assert!(prove_read_only("SELECT * FROM t FOR UPDATE"));
        for stmt in [
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t FOR SHARE",
            "SELECT * FROM t FOR NO KEY UPDATE",
            "SELECT * FROM t FOR KEY SHARE",
        ] {
            assert!(has_write_side_effects(stmt), "应识别取锁子句：{stmt}");
            assert!(
                !allow_auto_execute(stmt),
                "取锁 SELECT 不得自动执行：{stmt}"
            );
        }
        // 对照：同一条 SELECT 去掉取锁子句就放行。
        assert!(allow_auto_execute("SELECT * FROM t"));
    }

    /// 风险分级：无 WHERE 升危险档，整体取最高。
    #[test]
    fn risk_grades() {
        assert_eq!(risk_of("SELECT 1"), SqlRisk::ReadOnly);
        assert_eq!(risk_of("DELETE FROM t WHERE id = 1"), SqlRisk::Write);
        assert_eq!(risk_of("DELETE FROM t"), SqlRisk::Dangerous);
        assert_eq!(risk_of("UPDATE t SET a = 1"), SqlRisk::Dangerous);
        assert_eq!(risk_of("UPDATE t SET a = 1 WHERE 1"), SqlRisk::Dangerous);
        assert_eq!(risk_of("UPDATE t SET a = 1 WHERE id = 2"), SqlRisk::Write);
        assert_eq!(risk_of("DROP TABLE t"), SqlRisk::Dangerous);
        assert_eq!(risk_of("ALTER TABLE t ADD c INT"), SqlRisk::Write);

        let mixed = vec!["SELECT 1".to_owned(), "DROP TABLE t".to_owned()];
        assert_eq!(risk_of_statements(&mixed), SqlRisk::Dangerous);
    }

    /// 可编辑目标：单表裸 SELECT 认得出来；连表 / 聚合 / 别名 / 子查询一律不认。
    #[test]
    fn edit_target_only_for_plain_single_table_selects() {
        let target = |stmt: &str, table: &str, schema: Option<&str>| {
            assert_eq!(
                edit_target(stmt),
                Some(EditTarget {
                    table: table.to_owned(),
                    schema: schema.map(str::to_owned),
                }),
                "应认出可编辑目标：{stmt}"
            );
        };
        let none = |stmt: &str| {
            assert_eq!(edit_target(stmt), None, "不该认出可编辑目标：{stmt}");
        };

        // 认得出来的：通配、裸列名、带别名列（别名只影响展示）、WHERE / ORDER / LIMIT。
        target("SELECT * FROM users", "users", None);
        target("SELECT id, name FROM users WHERE id > 1", "users", None);
        target(
            "SELECT id AS 编号, name FROM users ORDER BY id LIMIT 10",
            "users",
            None,
        );
        target(
            "SELECT public.users.* FROM public.users",
            "users",
            Some("public"),
        );
        target("SELECT id FROM public.users", "users", Some("public"));

        // 连表：行没有唯一归属。
        none("SELECT * FROM a JOIN b ON a.id = b.id");
        none("SELECT * FROM a, b");
        none("SELECT * FROM a LEFT JOIN b ON a.id = b.id");
        // 表因子不是真表。
        none("SELECT * FROM (SELECT * FROM users) x");
        none("SELECT * FROM generate_series(1, 10)");
        none("SELECT * FROM users u");
        // 行数与表行不是一对一。
        none("SELECT DISTINCT name FROM users");
        none("SELECT count(*) FROM users");
        none("SELECT id, count(*) FROM users GROUP BY id");
        none("SELECT id FROM users HAVING id > 1");
        // 投影里有表达式 / 常量：列名不再等于表列名。
        none("SELECT id + 1 FROM users");
        none("SELECT upper(name) FROM users");
        none("SELECT 1 FROM users");
        // CTE / 集合运算 / 非 SELECT。
        none("WITH x AS (SELECT 1) SELECT * FROM x");
        none("SELECT * FROM a UNION SELECT * FROM b");
        none("UPDATE users SET name = 'x'");
        none("SELEC * FORM users");
    }

    /// 主键覆盖：结果集里必须真的带上定位所需的列（`SELECT name FROM t` 改不了）。
    #[test]
    fn edit_requires_the_primary_key_in_the_projection() {
        let projected = vec!["id".to_owned(), "name".to_owned()];
        assert!(covers_columns(&projected, &["id".to_owned()]));
        assert!(covers_columns(&projected, &[]));
        assert!(!covers_columns(&["name".to_owned()], &["id".to_owned()]));
        // 复合主键缺一不可（缺一个就定位不到唯一一行）。
        assert!(!covers_columns(
            &["a".to_owned(), "c".to_owned()],
            &["a".to_owned(), "b".to_owned()]
        ));
    }

    /// 排版只做规范化，绝不改语义：解析得了的换上规范形式，解析不了的**原样留着**。
    #[test]
    fn formatting_normalises_the_parsable_and_keeps_the_rest_verbatim() {
        let tidy = format_sql("select id,name from users where id=1");
        assert_eq!(
            tidy.text,
            "SELECT\n  id,\n  name\nFROM\n  users\nWHERE\n  id = 1"
        );
        assert!(tidy.kept_verbatim.is_empty(), "这一句应该能解析");

        // 多语句用空行分隔。分号**跟着原文走**：原文最后一条没写 `;` 就不加，
        // 写了就必须留住（拆分会把每条语句的尾分号剥掉，段间那个靠 `join` 补，
        // 末条那个只能显式补回）。
        let two = format_sql("select 1;select 2");
        assert_eq!(two.text, "SELECT\n  1;\n\nSELECT\n  2");
        let two_semi = format_sql("select 1;select 2;");
        assert_eq!(two_semi.text, "SELECT\n  1;\n\nSELECT\n  2;");

        // 单条带分号（最常用的一档）：格式化不能把分号吃掉。
        let one = format_sql("select 1;");
        assert_eq!(one.text, "SELECT\n  1;");
        // 补回的分号不破坏幂等：再格式化一次还是同一份文本。
        assert_eq!(format_sql(&one.text).text, one.text);

        // 半截语句 / 方言语法：原样保留，并**列出来**让界面能提示。
        let broken = format_sql("select from where; this is not sql");
        assert_eq!(broken.kept_verbatim.len(), 2);
        assert!(broken.text.contains("this is not sql"));
        assert!(broken.text.contains("select from where"));

        // 空输入不 panic，也不产出内容。
        let empty = format_sql("   ");
        assert_eq!(empty.text, "");
        assert!(empty.kept_verbatim.is_empty());
    }

    /// 关键字大小写与缩进档位要真正生效（对齐主流格式化工具的选项面）。
    #[test]
    fn formatting_honors_case_and_indent_options() {
        let lower = SqlFormatOptions {
            keyword_case: SqlKeywordCase::Lower,
            ..SqlFormatOptions::default()
        };
        assert_eq!(
            format_sql_with("SELECT id FROM users", &lower).text,
            "select\n  id\nfrom\n  users"
        );

        let preserve = SqlFormatOptions {
            keyword_case: SqlKeywordCase::Preserve,
            ..SqlFormatOptions::default()
        };
        assert_eq!(
            format_sql_with("SELECT id FROM users", &preserve).text,
            "SELECT\n  id\nFROM\n  users"
        );

        let tabs = SqlFormatOptions {
            indent_tabs: true,
            ..SqlFormatOptions::default()
        };
        assert!(
            format_sql_with("select id from users", &tabs)
                .text
                .contains("\n\tid")
        );
    }

    /// 排版是幂等的：对已经规范化过的文本再跑一次不变（否则「点两次格式化」会一直抖）。
    #[test]
    fn formatting_is_idempotent() {
        let once = format_sql("select a,b from t where x=1 and y=2").text;
        let twice = format_sql(&once).text;
        assert_eq!(once, twice);
    }

    /// 执行计划只对「单条 + 有计划的语句」成立。
    #[test]
    fn explaining_needs_exactly_one_statement_that_has_a_plan() {
        assert_eq!(
            explain_sql("select 1").as_deref(),
            Ok("EXPLAIN SELECT 1"),
            "读语句应当能被包成 EXPLAIN"
        );
        assert_eq!(
            explain_sql("update t set a = 1").as_deref(),
            Ok("EXPLAIN UPDATE t SET a = 1"),
            "改数据的语句也有计划"
        );
        for (sql, expected) in [
            ("", ExplainSkip::NotASingleStatement),
            ("   ", ExplainSkip::NotASingleStatement),
            ("select 1; select 2", ExplainSkip::NotASingleStatement),
            ("drop table t", ExplainSkip::NoPlanForThisStatement),
            ("begin", ExplainSkip::NoPlanForThisStatement),
            ("this is not sql", ExplainSkip::NoPlanForThisStatement),
        ] {
            assert_eq!(explain_sql(sql), Err(expected), "`{sql}` 的判定不对");
        }
    }

    /// 按引擎挑「能结构化」的形态：四条 SQL 引擎各有各的写法，其余三条退回通用。
    #[test]
    fn per_engine_explain_shapes() {
        assert_eq!(
            explain_sql_for(DatabaseKind::Sqlite, "select 1").as_deref(),
            Ok("EXPLAIN QUERY PLAN SELECT 1")
        );
        assert_eq!(
            explain_sql_for(DatabaseKind::Postgres, "select 1").as_deref(),
            Ok("EXPLAIN (FORMAT JSON) SELECT 1")
        );
        assert_eq!(
            explain_sql_for(DatabaseKind::Mysql, "select 1").as_deref(),
            Ok("EXPLAIN FORMAT=JSON SELECT 1")
        );
        assert_eq!(
            explain_sql_for(DatabaseKind::ClickHouse, "select 1").as_deref(),
            Ok("EXPLAIN SELECT 1")
        );
        // 没有结构化形态的三条：与 `explain_sql` 同形（由数据库自己报错，不编语法）。
        for kind in [
            DatabaseKind::SqlServer,
            DatabaseKind::Mongo,
            DatabaseKind::Elasticsearch,
        ] {
            assert_eq!(
                explain_sql_for(kind, "select 1"),
                explain_sql("select 1"),
                "{kind:?} 应当退回通用形态"
            );
        }
        // 闸门是同一条：不合格的语句在任何引擎下都拿不到计划语句。
        for kind in [
            DatabaseKind::Sqlite,
            DatabaseKind::Postgres,
            DatabaseKind::Mysql,
            DatabaseKind::ClickHouse,
        ] {
            assert_eq!(
                explain_sql_for(kind, "drop table t"),
                Err(ExplainSkip::NoPlanForThisStatement)
            );
            assert_eq!(
                explain_sql_for(kind, "select 1; select 2"),
                Err(ExplainSkip::NotASingleStatement)
            );
        }
    }

    /// 生成的计划语句必须**本身**被认成读语句：否则驱动会把它当写语句走 `execute`，
    /// 结果集就没了（各驱动的读 / 写分流靠 `prove_read_only` + `classify`）。
    #[test]
    fn generated_plan_statements_count_as_reads() {
        for kind in [
            DatabaseKind::Sqlite,
            DatabaseKind::Postgres,
            DatabaseKind::Mysql,
            DatabaseKind::ClickHouse,
        ] {
            let sql = explain_sql_for(kind, "select 1").expect("包装成功");
            assert!(
                prove_read_only(&sql) || classify(&sql) == StmtKind::Read,
                "{kind:?} 的计划语句被当成写语句了：{sql}"
            );
        }
    }

    #[test]
    fn minify_sql_folds_whitespace_and_strips_inner_comments() {
        assert_eq!(
            minify_sql("SELECT a , b\n-- 备注\nFROM t ;"),
            "SELECT a, b FROM t;"
        );
        // 语句内部的块注释整段吃掉（含未闭合的）。
        assert_eq!(minify_sql("SELECT 1 /* 尾"), "SELECT 1");
        // 字符串字面量内部原样：双空格、注释记号、引号都不动；逗号后照常保一个空格。
        assert_eq!(
            minify_sql("SELECT 'a  -- b' , \"c /* d\" FROM t"),
            "SELECT 'a  -- b', \"c /* d\" FROM t"
        );
        // 括号内侧不留空格（`f(` 直接连住）；逗号后照常保一个空格。
        assert_eq!(minify_sql("f( a , b ) ;"), "f(a, b);");
        assert_eq!(minify_sql("   "), "");
    }

    /// 压缩**保留语句上方的注释块**：那一块是作者写给下一个人看的说明，
    /// 压掉它等于替用户删内容（压缩的目的是省版面，不是清理正文）。
    #[test]
    fn minify_sql_keeps_leading_comments() {
        // 行注释抬头：整块按原文留下（换行与相对缩进不折叠），尾随空行裁掉。
        assert_eq!(
            minify_sql("-- 日活统计\n-- 2026-10\n\nSELECT  a ,\n  b FROM t ;\n"),
            "-- 日活统计\n-- 2026-10\nSELECT a, b FROM t;"
        );
        // 块注释抬头同样保留（未闭合的也保留 —— 用户可能正打着字）。
        assert_eq!(minify_sql("/* 头部 */ SELECT 1"), "/* 头部 */\nSELECT 1");
        // **只跳过空白不算抬头**：前面只是缩进时不额外占一行。
        assert_eq!(minify_sql("   \n  SELECT 1"), "SELECT 1");
        // 整段都是注释：一个字都压不掉，原样（裁两端空白）返回。
        assert_eq!(minify_sql("\n-- 只有注释\n"), "-- 只有注释");
        // 抬头与语句内部各按各的规则：前者留、后者压。
        assert_eq!(
            minify_sql("-- 抬头\nSELECT 1 -- 行内\nFROM t"),
            "-- 抬头\nSELECT 1 FROM t"
        );
    }

    #[test]
    fn toggle_line_comments_follows_first_line_direction() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;\n";
        // 覆盖第 2 行（字节 10..=18 是 "SELECT 2;"）：加注释。
        let range = Range { start: 10, end: 19 };
        let toggled = toggle_line_comments(sql, range.clone());
        assert_eq!(toggled.text, "SELECT 1;\n-- SELECT 2;\nSELECT 3;\n");
        assert!(!toggled.removed);
        assert_eq!(toggled.lines, 1);
        // 再按一次（首行已有注释）→ 剥掉，缩进保留。
        let back = toggle_line_comments(&toggled.text, range);
        assert_eq!(back.text, sql);
        assert!(back.removed);
        // 选区跨多行：整段一起切。
        let wide = toggle_line_comments(sql, Range { start: 0, end: 29 });
        assert_eq!(wide.lines, 3);
        assert!(
            wide.text
                .starts_with("-- SELECT 1;\n-- SELECT 2;\n-- SELECT 3;")
        );
        // 结尾换行不被 `lines()` 吃掉。
        assert!(wide.text.ends_with('\n'));
    }

    #[test]
    fn toggle_block_comment_wraps_and_unwraps() {
        let sql = "SELECT 1;\nSELECT 2;\n";
        // 选区盖住第 2 行（字节 10..19 是 "SELECT 2;"）→ 包上 `/* */`。
        let range = Range { start: 10, end: 19 };
        let wrapped = toggle_block_comment(sql, range.clone());
        assert_eq!(wrapped.text, "SELECT 1;\n/*SELECT 2;*/\n");
        assert!(!wrapped.removed);
        // 再按一次（选区盖住整段注释，包上后 `/*` 在字节 10、`*/` 收在 23）→ 剥掉，原文还原。
        let unwrapped = toggle_block_comment(&wrapped.text, Range { start: 10, end: 23 });
        assert_eq!(unwrapped.text, sql);
        assert!(unwrapped.removed);
        // 内侧带空格的注释同样剥得掉（剥记号时各宽容量一个空格；注释占 10..25）。
        assert_eq!(
            toggle_block_comment("SELECT 1;\n/* SELECT 2; */\n", Range { start: 10, end: 25 }).text,
            sql
        );
        // 选区带尾随换行：换行不卷进注释记号里。
        let with_newline = toggle_block_comment(sql, Range { start: 10, end: 20 });
        assert_eq!(with_newline.text, "SELECT 1;\n/*SELECT 2;*/\n");
    }

    #[test]
    fn toggle_comments_picks_action_by_selection_state() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;\n";
        // 多行选区（盖住第 2~3 行）→ 用 `/* */` 整段包住。
        let wide = toggle_comments(sql, Range { start: 10, end: 30 });
        assert_eq!(wide.action, CommentAction::BlockAdded);
        assert_eq!(wide.text, "SELECT 1;\n/*SELECT 2;\nSELECT 3;*/\n");
        // 选区已是完整块注释 → 剥掉（`/*` 在字节 10，`*/` 收在字节 33）。
        let unwrapped = toggle_comments(&wide.text, Range { start: 10, end: 33 });
        assert_eq!(unwrapped.action, CommentAction::BlockRemoved);
        assert_eq!(unwrapped.text, sql);
        // 首行已有 `-- ` → 行注释剥掉（去掉优先级最高）。
        let lined = toggle_comments("-- SELECT 1;\nSELECT 2;\n", Range { start: 0, end: 12 });
        assert_eq!(lined.action, CommentAction::LineRemoved);
        assert_eq!(lined.text, "SELECT 1;\nSELECT 2;\n");
        // 单行选区 → 加 `-- `（行注释路径）。
        let single = toggle_comments(sql, Range { start: 10, end: 19 });
        assert_eq!(single.action, CommentAction::LineAdded);
        assert_eq!(single.text, "SELECT 1;\n-- SELECT 2;\nSELECT 3;\n");
        // 空选区（光标停在第 2 行）→ 同样给那一行加 `-- `。
        let cursor = toggle_comments(sql, Range { start: 12, end: 12 });
        assert_eq!(cursor.action, CommentAction::LineAdded);
        assert_eq!(cursor.text, "SELECT 1;\n-- SELECT 2;\nSELECT 3;\n");
        // 光标停在已注释的行上 → 剥掉（空选区也走「去掉优先」）。
        let back = toggle_comments(&single.text, Range { start: 12, end: 12 });
        assert_eq!(back.action, CommentAction::LineRemoved);
        assert_eq!(back.text, sql);
    }
}
