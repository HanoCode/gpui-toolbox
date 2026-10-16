//! 执行 **SQL 文件**：流式读盘 → 流式切分 → 分批执行 → 字节级进度。
//!
//! ## 与 [`crate::batch`] 的分工
//!
//! [`crate::batch`] 回答「**一批**语句怎么跑得比逐条排队快」；本模块回答
//! 「**一个文件**怎么喂成批」。三层叠起来是这样：
//!
//! ```text
//! execute_sql_file（本模块：读盘 / 解码 / 切分 / 刷批 / 进度）
//!   └─ StatementScanner（sqlintel：跨块保住 DELIMITER 档位的流式切分）
//!   └─ execute_batch（batch：一次加锁 + 一个事务 + 逐条回执）
//!        └─ Connection::execute（drivers：单条语句）
//! ```
//!
//! ## 对照 dbflux / dbx 的控制点
//!
//! | 控制点 | 本模块的做法 |
//! |---|---|
//! | 读盘 | **256KB 块**流式读，整份文本永不驻留（尾巴最多一条语句） |
//! | 刷批 | **条数 / 字节双阈值**（默认 256 条 / 8MB），到一条就交一批 |
//! | 事务 | [`BatchTransaction::Single`] = **整份文件一个事务**（收尾才提交） |
//! | 遇错 | [`SqlFileOptions::continue_on_error`]，`false` 时**连文件都不再读** |
//! | 取消 | 谓词逐块 + 逐语句问；「正在跑的那条」由调用方的 `CancelHandle` 打断 |
//! | 进度 | **字节级**（读了多少 / 共多少），100ms 合流一次 |
//! | 编码 | BOM 嗅探（UTF-8 BOM / UTF-16 LE·BE）→ UTF-8 严格校验 → 兜底 GBK |
//!
//! ## 为什么取消要靠谓词
//!
//! [`crate::cancel::CancelHandle`] 只有「送一枪」没有「查状态」（七引擎的带外取消
//! 原语都是单向信号）。所以「还要不要往下读 / 下一条要不要跑」由调用方的谓词回答
//! （界面在「停止」按钮上同时置自己的原子标志），两者一对就是完整语义。
//!
//! ## ⚠️ 单事务档下「遇错继续」没有意义
//!
//! 整份文件一个事务时，一条失败就要**整份回滚**，再往下跑只是白跑（dbx 的手动事务
//! 档同样禁「继续执行」）。所以这里**强制**：`Single` 档忽略 `continue_on_error`。
//!
//! ⚠️ 本层不做危险闸门也不碰界面：是否要问用户一句，由调用方在进来之前决定
//! （界面走 `DatabasePage::guard_dangerous`）。

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use encoding_rs::{CoderResult, Decoder, GBK, UTF_8, UTF_16BE, UTF_16LE};

use crate::batch::{
    BatchReport, BatchTransaction, SqlBatchOptions, StatementOutcome, StatementStatus,
    execute_batch,
};
use crate::drivers::Connection;
use crate::error::DbError;
use crate::sqlintel::{StatementScanner, TxControl, is_blank_or_comment};

/// 每次读盘的字节数（与 dbx 同档）。
pub const CHUNK_BYTES: usize = 256 * 1024;

/// 编码嗅探窗口：够长到能撞见非 ASCII 正文，又不至于白读大文件。
const SNIFF_BYTES: usize = 64 * 1024;

/// 进度回调的最小间隔（合流；比这更密的回调对界面毫无意义）。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// 默认的刷批条数阈值（与 dbx 同档）。
pub const DEFAULT_FLUSH_STATEMENTS: usize = 256;

/// 默认的刷批字节阈值（与 dbx 同档）。
pub const DEFAULT_FLUSH_BYTES: usize = 8 * 1024 * 1024;

/// 回执里最多留几条错误明细（大文件一次错几千条时不能把内存吃光）。
const MAX_ERRORS: usize = 32;

/// 错误明细里语句原文的截断长度。
const ERROR_SQL_CAP: usize = 200;

/// 文件的**字符编码**。
///
/// 只列四种真实会遇到的：UTF-8（含 BOM）、UTF-16 两种字节序、GBK（GB2312 /
/// GB18030 都能解）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqlFileEncoding {
    /// UTF-8（无 BOM）。
    #[default]
    Utf8,
    /// UTF-8 带 BOM（`EF BB BF`）——Windows 记事本「UTF-8」另存的老习惯。
    Utf8Bom,
    /// UTF-16 小端（`FF FE`）。
    Utf16Le,
    /// UTF-16 大端（`FE FF`）。
    Utf16Be,
    /// GBK / GB2312 / GB18030。
    Gbk,
}

impl SqlFileEncoding {
    /// 给人看的名字（回执与状态栏用；英文，界面文案由调用方 `tr` 组织）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8 (BOM)",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Gbk => "GBK",
        }
    }

    /// 对应的解码器。
    fn decoder(self) -> Decoder {
        self.encoding().new_decoder()
    }

    fn encoding(self) -> &'static encoding_rs::Encoding {
        match self {
            // BOM 自己剥（不指望解码器嗅）：剥没剥由 [`Self::bom_len`] 说了算，
            // 好被单测钉死。
            Self::Utf8 | Self::Utf8Bom => UTF_8,
            Self::Utf16Le => UTF_16LE,
            Self::Utf16Be => UTF_16BE,
            Self::Gbk => GBK,
        }
    }

    /// 文件开头要跳过的 BOM 字节数。
    const fn bom_len(self) -> usize {
        match self {
            Self::Utf8 => 0,
            Self::Utf8Bom => 3,
            Self::Utf16Le | Self::Utf16Be => 2,
            Self::Gbk => 0,
        }
    }
}

/// 执行 SQL 文件的选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlFileOptions {
    /// 事务口径（`Single` = 整份文件一个事务）。
    pub transaction: BatchTransaction,
    /// 遇错是否继续；`Single` 档下**强制为假**（见模块注释）。
    pub continue_on_error: bool,
    /// 读语句的行上限（与 [`Connection::execute`] 同一个参数）。
    pub max_rows: usize,
    /// 刷批条数阈值。
    pub flush_statements: usize,
    /// 刷批字节阈值。
    pub flush_bytes: usize,
    /// 指定编码；`None` = 按文件内容嗅探。
    pub encoding: Option<SqlFileEncoding>,
}

impl SqlFileOptions {
    /// 逐条提交、遇错即停、默认阈值、自动嗅探编码。
    #[must_use]
    pub const fn new(max_rows: usize) -> Self {
        Self {
            transaction: BatchTransaction::AutoCommit,
            continue_on_error: false,
            max_rows,
            flush_statements: DEFAULT_FLUSH_STATEMENTS,
            flush_bytes: DEFAULT_FLUSH_BYTES,
            encoding: None,
        }
    }

    /// 整份文件一个事务（一条失败全回滚；「继续执行」随之失效）。
    #[must_use]
    pub const fn single_transaction(self) -> Self {
        Self {
            transaction: BatchTransaction::Single,
            ..self
        }
    }

    /// 出错继续跑（`Single` 档下不生效）。
    #[must_use]
    pub const fn continue_on_error(self) -> Self {
        Self {
            continue_on_error: true,
            ..self
        }
    }

    /// 改刷批阈值（`0` 按 1 处理，否则会永不刷批）。
    #[must_use]
    pub const fn flush_at(self, statements: usize, bytes: usize) -> Self {
        Self {
            flush_statements: if statements == 0 { 1 } else { statements },
            flush_bytes: if bytes == 0 { 1 } else { bytes },
            ..self
        }
    }

    /// 显式指定编码（跳过嗅探）。
    #[must_use]
    pub const fn with_encoding(self, encoding: SqlFileEncoding) -> Self {
        Self {
            encoding: Some(encoding),
            ..self
        }
    }
}

/// 一次进度快照（**字节级**：界面据此画进度条）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileProgress {
    /// 已从磁盘读出的字节数。
    pub bytes_read: u64,
    /// 文件总字节数（取不到时为 0，此时 [`Self::fraction`] 返回 0）。
    pub total_bytes: u64,
    /// 已交给引擎的语句条数。
    pub statements: usize,
    /// 其中失败的条数。
    pub failed: usize,
    /// 影响行数合计。
    pub affected: u64,
}

impl FileProgress {
    /// 完成比例（0.0 ~ 1.0）。总大小未知时返回 0.0（界面按「不确定」画）。
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.bytes_read as f64 / self.total_bytes as f64).min(1.0) as f32
    }
}

/// 一条失败语句的定位信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatementError {
    /// 第几条语句（**从 1 数**，给人看）。
    pub index: usize,
    /// 它落定时已读到的字节位置（在大文件里定位那一句）。
    pub bytes_read: u64,
    /// 语句原文（截到 [`ERROR_SQL_CAP`] 字符）。
    pub sql: String,
    /// 引擎错误原文（本 crate 的错误一律英文）。
    pub message: String,
}

/// 执行 SQL 文件的总回执。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileReport {
    /// 实际用的编码（嗅探结果或显式指定）。
    pub encoding: SqlFileEncoding,
    /// 从磁盘读出的字节数。
    pub bytes_read: u64,
    /// 文件总字节数（取不到时为 0）。
    pub total_bytes: u64,
    /// 交给引擎执行的语句条数（**不含**空白 / 纯注释——那些不是语句）。
    pub statements: usize,
    /// 真跑了的条数。
    pub executed: usize,
    /// 报错的条数。
    pub failed: usize,
    /// 没跑的条数（遇错即停后剩下的，或被叫停的）。
    pub skipped: usize,
    /// 空白 / 纯注释的块数（切出来后直接丢掉，不是语句）。
    pub ignored: usize,
    /// 影响行数合计。
    pub affected: u64,
    /// 上游叫停（谓词命中）。
    pub stopped: bool,
    /// 遇错即停：文件**没读完**就停了（剩下没读的内容不进账）。
    pub aborted_on_error: bool,
    /// 引擎报了 [`DbError::Cancelled`]。
    pub cancelled: bool,
    /// 整份文件的耗时。
    pub elapsed_ms: u128,
    /// 事务收尾（`COMMIT` / `ROLLBACK`）报错的原文；只有 `Single` 档会有。
    pub transaction_error: Option<String>,
    /// 失败明细（最多 [`MAX_ERRORS`] 条）。
    pub errors: Vec<FileStatementError>,
    /// 失败明细是否被截断（真失败数比 `errors.len()` 多）。
    pub errors_truncated: bool,
}

impl FileReport {
    /// 全部成功（没有任何失败 / 跳过）。
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.failed == 0 && self.skipped == 0 && self.transaction_error.is_none()
    }

    /// 第一条错误原文（界面报错优先用它）。
    #[must_use]
    pub fn first_error(&self) -> Option<&str> {
        self.errors.first().map(|entry| entry.message.as_str())
    }
}

/// 执行 SQL 文件失败的原因。
///
/// 为什么不并进 [`DbError`]：文件层的失败（打不开 / 读一半 IO 错）与语句层的失败
/// 是两回事，而且 [`DbError`] 是被全仓穷举匹配的枚举，不该为一条 IO 错误加变体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlFileError {
    /// 文件读不出来（不存在 / 无权限 / 读到一半出错）。
    Io(String),
    /// 语句执行层自己报的错——只有「整份文件一个事务」的 `BEGIN` 失败会走到这里
    ///（那时一条语句都没跑过）。
    Db(DbError),
}

impl fmt::Display for SqlFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(detail) => write!(f, "cannot read SQL file: {detail}"),
            Self::Db(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SqlFileError {}

impl From<DbError> for SqlFileError {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}

/// 流式读取并执行一个 `.sql` 文件。
///
/// - `should_stop`：每读一块、每条语句**发起前**都问一次；命中即停（剩下的不跑，
///   文件也不再往下读）。正在跑的那条由调用方的 [`crate::cancel::CancelHandle`]
///   负责打断。
/// - `on_statement`：逐条回执（与 [`execute_batch`] 同一个回调，界面据此记流水）。
/// - `on_progress`：字节级进度，**100ms 合流**一次；结束时必定补一次。
///
/// ⚠️ 调用方要保证**同一个文件只跑一次**：本函数不记任何跨调用的状态（同一条语句
/// 跑两遍就是跑两遍，脚本里的 `INSERT` 通常会变成两行数据）。
pub fn execute_sql_file(
    conn: &mut Connection,
    path: &Path,
    options: &SqlFileOptions,
    should_stop: &dyn Fn() -> bool,
    on_statement: &mut dyn FnMut(&StatementOutcome),
    on_progress: &mut dyn FnMut(&FileProgress),
) -> Result<FileReport, SqlFileError> {
    let started = Instant::now();
    let total_bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let mut file = File::open(path).map_err(|error| SqlFileError::Io(error.to_string()))?;

    // ① 嗅探：先读一小段定编码（显式指定就跳过）。这段读完还得接着当正文用。
    let mut head = vec![0u8; SNIFF_BYTES];
    let filled = fill(&mut file, &mut head)?;
    head.truncate(filled);
    let encoding = options.encoding.unwrap_or_else(|| sniff(&head));

    let mut session = Session {
        conn,
        options,
        should_stop,
        on_statement,
        scanner: StatementScanner::new(),
        pending: Vec::new(),
        pending_bytes: 0,
        report: FileReport {
            encoding,
            total_bytes,
            ..FileReport::default()
        },
        last_progress: Instant::now() - PROGRESS_INTERVAL,
        halted: false,
    };

    // ② 整份文件一个事务：开在第一条语句之前，收尾才提交 / 回滚。
    let whole_file_transaction = options.transaction == BatchTransaction::Single;
    if whole_file_transaction {
        let begin = TxControl::Begin.statement(session.conn.kind());
        session.conn.execute(begin, 1).map_err(SqlFileError::Db)?;
    }

    // ③ 逐块读 → 解码 → 切分 → 刷批。
    let mut bom = encoding.bom_len();
    let mut decoder = encoding.decoder();
    let mut raw = vec![0u8; CHUNK_BYTES];
    let mut text = String::new();
    // 头一段先处理（它已经读进来了）。BOM 只在文件最开头，只可能落在这一段。
    let first = strip_bom(&head, &mut bom);
    decode(&mut decoder, first, false, &mut text);
    session.feed(&text, head.len() as u64, on_progress)?;
    text.clear();

    while !session.halted {
        if should_stop() {
            session.report.stopped = true;
            break;
        }
        let read = file
            .read(&mut raw)
            .map_err(|error| SqlFileError::Io(error.to_string()))?;
        if read == 0 {
            break;
        }
        decode(&mut decoder, &raw[..read], false, &mut text);
        let bytes_read = session.report.bytes_read + read as u64;
        session.feed(&text, bytes_read, on_progress)?;
        text.clear();
    }

    // ④ 尾巴：切分器里可能还压着最后一条没有分隔符的语句。
    if !session.halted {
        decode(&mut decoder, &[], true, &mut text);
        let tail = session.scanner.finish();
        session.queue(tail);
        session.flush_statement_batch()?;
    }

    // ⑤ 收尾事务。
    let mut report = session.report;
    if whole_file_transaction {
        let rollback =
            report.failed > 0 || report.cancelled || report.stopped || report.aborted_on_error;
        let control = if rollback {
            TxControl::Rollback
        } else {
            TxControl::Commit
        };
        let statement = control.statement(conn.kind());
        if let Err(error) = conn.execute(statement, 1) {
            report.transaction_error = Some(error.to_string());
        }
    }
    report.elapsed_ms = started.elapsed().as_millis();
    on_progress(&report.progress());
    Ok(report)
}

/// 执行过程中会变的那一堆状态（避免把十几个局部量在函数间传来传去）。
struct Session<'a> {
    conn: &'a mut Connection,
    options: &'a SqlFileOptions,
    should_stop: &'a dyn Fn() -> bool,
    on_statement: &'a mut dyn FnMut(&StatementOutcome),
    scanner: StatementScanner,
    /// 攒着等刷批的语句。
    pending: Vec<String>,
    /// [`Self::pending`] 的字节数（估算：文本长度，够触发阈值即可）。
    pending_bytes: usize,
    report: FileReport,
    last_progress: Instant,
    /// 已经停了（遇错即停 / 被叫停）：不再往下读。
    halted: bool,
}

impl Session<'_> {
    /// 吃一块**已解码**的文本：切语句 → 攒批 → 够了就刷。
    fn feed(
        &mut self,
        text: &str,
        bytes_read: u64,
        on_progress: &mut dyn FnMut(&FileProgress),
    ) -> Result<(), SqlFileError> {
        self.report.bytes_read = bytes_read;
        let statements = self.scanner.push(text);
        self.queue(statements);
        if self.pending.len() >= self.options.flush_statements
            || self.pending_bytes >= self.options.flush_bytes
        {
            self.flush_statement_batch()?;
        }
        // 进度按时间合流：磁盘读得快时不必每块都回调一次。
        if self.last_progress.elapsed() >= PROGRESS_INTERVAL {
            self.last_progress = Instant::now();
            let progress = self.report.progress();
            on_progress(&progress);
        }
        Ok(())
    }

    /// 收下切出来的语句（空白 / 纯注释不是语句，直接丢）。
    fn queue(&mut self, statements: Vec<String>) {
        for sql in statements {
            if is_blank_or_comment(&sql) {
                // `GO` 断批会把「只装着一条注释」的缓冲交出来——那不是语句。
                self.report.ignored += 1;
                continue;
            }
            self.pending_bytes += sql.len();
            self.pending.push(sql);
        }
    }

    /// 把攒着的一批交给 [`execute_batch`]。
    fn flush_statement_batch(&mut self) -> Result<(), SqlFileError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let statements = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        // 整份文件一个事务时，批内**不能再开事务**（嵌套 `BEGIN`），事务交收尾管；
        // 同时「遇错继续」在单事务档下失效（一条失败就要整份回滚）。
        let whole_file_transaction = self.options.transaction == BatchTransaction::Single;
        let batch_options = SqlBatchOptions {
            transaction: BatchTransaction::AutoCommit,
            continue_on_error: self.options.continue_on_error && !whole_file_transaction,
            max_rows: self.options.max_rows,
        };
        let report = execute_batch(
            self.conn,
            &statements,
            &batch_options,
            self.should_stop,
            self.on_statement,
        )
        .map_err(SqlFileError::Db)?;
        self.fold(&report);
        // 遇错即停：文件不再往下读（剩下的内容不进账）。
        if !batch_options.continue_on_error && report.failed > 0 {
            self.halted = true;
            self.report.aborted_on_error = true;
        }
        if report.stopped {
            self.halted = true;
        }
        Ok(())
    }

    /// 把一批回执并进总账。
    fn fold(&mut self, batch: &BatchReport) {
        let base = self.report.statements;
        self.report.statements += batch.statements.len();
        self.report.executed += batch.executed;
        self.report.failed += batch.failed;
        self.report.skipped += batch.skipped;
        self.report.affected += batch.affected;
        self.report.stopped |= batch.stopped;
        self.report.cancelled |= batch.cancelled;
        if let Some(error) = &batch.transaction_error {
            self.report.transaction_error = Some(error.clone());
        }
        for entry in &batch.statements {
            let StatementStatus::Failed(message) = &entry.status else {
                continue;
            };
            if self.report.errors.len() >= MAX_ERRORS {
                self.report.errors_truncated = true;
                continue;
            }
            self.report.errors.push(FileStatementError {
                index: base + entry.index + 1,
                bytes_read: self.report.bytes_read,
                sql: head_of(&entry.sql, ERROR_SQL_CAP),
                message: message.clone(),
            });
        }
    }
}

impl FileReport {
    /// 当前进度快照。
    fn progress(&self) -> FileProgress {
        FileProgress {
            bytes_read: self.bytes_read,
            total_bytes: self.total_bytes,
            statements: self.report_statements_for_progress(),
            failed: self.failed,
            affected: self.affected,
        }
    }

    /// 进度里的条数 = 已落定 + 已忽略（空白 / 注释）——两者都不是「待办」。
    fn report_statements_for_progress(&self) -> usize {
        self.statements + self.ignored
    }
}

/// 把缓冲尽量填满（普通文件一次 `read` 通常就够，但别指望它）。
fn fill(file: &mut File, buffer: &mut [u8]) -> Result<usize, SqlFileError> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file
            .read(&mut buffer[filled..])
            .map_err(|error| SqlFileError::Io(error.to_string()))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

/// 跳掉还没跳完的 BOM（BOM 可能被第一块读切断，虽然 64KB 下不可能）。
fn strip_bom<'a>(bytes: &'a [u8], bom: &mut usize) -> &'a [u8] {
    let skip = (*bom).min(bytes.len());
    *bom -= skip;
    &bytes[skip..]
}

/// 按文件的**真实编码**嗅探：BOM → UTF-8 严格校验 → 兜底 GBK。
///
/// ⚠️ 这是启发式，不是判定：纯 ASCII 的文件永远报 UTF-8（那没关系——两种解法的
/// 结果一模一样），而 GBK 正文只要出现一个「按 UTF-8 说不过去」的字节就会被认出来。
/// 唯一会猜错的情形是「整份正文都是 ASCII，只有窗尾一个孤立高位字节」——真遇到
/// 就由调用方用 [`SqlFileOptions::with_encoding`] 显式指定。
fn sniff(head: &[u8]) -> SqlFileEncoding {
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return SqlFileEncoding::Utf8Bom;
    }
    if head.starts_with(&[0xFF, 0xFE]) {
        return SqlFileEncoding::Utf16Le;
    }
    if head.starts_with(&[0xFE, 0xFF]) {
        return SqlFileEncoding::Utf16Be;
    }
    match std::str::from_utf8(head) {
        Ok(_) => SqlFileEncoding::Utf8,
        // 窗口正好切在一个多字节序列中间（`error_len()` 为 `None`，表示「输入就
        // 这么断了」）。这时要看尾部那几个字节**像不像**没写完的 UTF-8 序列：
        // 像就当 UTF-8（真被切了），不像说明前面那个字节本来就不该出现 → GBK。
        Err(error) if error.error_len().is_none() => {
            let rest = &head[error.valid_up_to()..];
            if looks_like_truncated_utf8(rest) {
                SqlFileEncoding::Utf8
            } else {
                SqlFileEncoding::Gbk
            }
        }
        Err(_) => SqlFileEncoding::Gbk,
    }
}

/// 这几个字节像不像「一个被切断的 UTF-8 序列」（引导字节 + 若干续字节）。
fn looks_like_truncated_utf8(rest: &[u8]) -> bool {
    let Some(&lead) = rest.first() else {
        return false;
    };
    let needed = match lead {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return false,
    };
    let have = rest.len();
    have < needed && rest[1..].iter().all(|byte| (byte & 0xC0) == 0x80)
}

/// 把一块字节解码进 `text`（跨块的半个字符由 [`Decoder`] 自己端着）。
///
/// ⚠️ `decode_to_string` 把 `String` 的**剩余容量**当输出上限（它自己不扩容），
/// 返回的 `read` 是这次真吃掉的输入字节数——**必须自己往前挪**。忽略了它，
/// `OutputFull` 就会从头再解一遍，输出成倍膨胀（只有几 KB 以内的小文件因
/// 「首次 `reserve` 后一遍解完」而看不出问题）。
fn decode(decoder: &mut Decoder, bytes: &[u8], last: bool, text: &mut String) {
    let mut src = bytes;
    loop {
        let (result, read, _had_errors) = decoder.decode_to_string(src, text, last);
        src = &src[read..];
        match result {
            CoderResult::InputEmpty => break,
            // GBK → UTF-8 最长能到 1.5 倍，多要一点（再加个零头）。
            CoderResult::OutputFull => text.reserve(src.len() + src.len() / 2 + 32),
        }
    }
}

/// 截断语句原文（按**字符**截，不切碎多字节字符）。
fn head_of(sql: &str, cap: usize) -> String {
    if sql.len() <= cap {
        return sql.to_owned();
    }
    let mut end = cap;
    while end > 0 && !sql.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &sql[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConnectionConfig, DatabaseKind};
    use crate::result::Value;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 每个测试一个独立临时目录（放脚本文件与库文件）。
    fn temp_dir() -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "duck-sqlfile-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn temp_sqlite(dir: &Path) -> Connection {
        let config = ConnectionConfig {
            id: "sqlfile-test".to_owned(),
            kind: DatabaseKind::Sqlite,
            database: dir.join("t.sqlite").display().to_string(),
            ..ConnectionConfig::default()
        };
        Connection::connect(&config, None).expect("临时 SQLite 建连")
    }

    fn count(conn: &mut Connection, table: &str) -> i64 {
        let outcome = conn
            .execute(&format!("SELECT count(*) FROM {table}"), 10)
            .expect("count 查询");
        match &outcome.result.expect("count 有结果集").rows[0][0] {
            Value::Int(value) => *value,
            other => other.render().to_string().parse().expect("count 是整数"),
        }
    }

    /// 跑一个文件（「不停」的谓词，不关心逐条回执）。
    fn run(
        conn: &mut Connection,
        path: &Path,
        options: &SqlFileOptions,
    ) -> Result<FileReport, SqlFileError> {
        execute_sql_file(conn, path, options, &|| false, &mut |_| {}, &mut |_| {})
    }

    /// 整份文件跑通：条数、影响行数、编码、进度都如实。
    #[test]
    fn executes_a_whole_file_and_reports_progress() {
        let dir = temp_dir();
        let script = dir.join("ok.sql");
        std::fs::write(
            &script,
            "-- 建表\nCREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);\n\
             INSERT INTO t VALUES (1, 'a');\nINSERT INTO t VALUES (2, 'b');\n\
             UPDATE t SET name = 'c' WHERE id = 2;\nSELECT * FROM t;\n",
        )
        .expect("写脚本");

        let mut conn = temp_sqlite(&dir);
        // `FileProgress` 是 `Copy`：留下快照本身，别留引用（引用会带出闭包的生命周期）。
        let mut seen: Vec<FileProgress> = Vec::new();
        let mut remember = |progress: &FileProgress| seen.push(*progress);
        let report = execute_sql_file(
            &mut conn,
            &script,
            &SqlFileOptions::new(100),
            &|| false,
            &mut |_| {},
            &mut remember,
        )
        .expect("执行文件");

        assert_eq!(report.encoding, SqlFileEncoding::Utf8);
        assert_eq!(report.statements, 5, "{:?}", report.errors);
        assert_eq!(report.executed, 5);
        assert_eq!(report.failed, 0);
        assert_eq!(report.affected, 3, "两条 INSERT 各 1 行 + UPDATE 命中 1 行");
        assert!(report.all_ok());
        assert_eq!(report.bytes_read, report.total_bytes);
        assert_eq!(count(&mut conn, "t"), 2);
        // 进度至少送了收尾那一次，且字节数是满的。
        let last = seen.last().expect("有进度回调");
        assert_eq!(last.bytes_read, report.total_bytes);
        assert_eq!(last.fraction(), 1.0);
    }

    /// 遇错即停：后面的语句不跑，文件也不再往下读。
    #[test]
    fn stops_at_the_first_error_and_stops_reading() {
        let dir = temp_dir();
        let script = dir.join("bad.sql");
        std::fs::write(
            &script,
            "CREATE TABLE t(id INTEGER PRIMARY KEY);\n\
             INSERT INTO t VALUES (1);\nINSERT INTO nope VALUES (1);\n\
             INSERT INTO t VALUES (2);\n",
        )
        .expect("写脚本");

        let mut conn = temp_sqlite(&dir);
        // 刷批阈值降到 1：每条一刷，好让「即停」发生在读第二句之前。
        let options = SqlFileOptions::new(100).flush_at(1, 1);
        let report = run(&mut conn, &script, &options).expect("执行文件");

        assert_eq!(report.executed, 2, "{:?}", report.errors);
        assert_eq!(report.failed, 1);
        assert!(report.aborted_on_error);
        assert!(!report.all_ok());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].index, 3, "第 3 条语句失败");
        assert_eq!(count(&mut conn, "t"), 1, "失败之后的语句没跑");
    }

    /// 遇错继续：后面照跑，错误明细如实。
    #[test]
    fn continue_on_error_keeps_going() {
        let dir = temp_dir();
        let script = dir.join("continue.sql");
        std::fs::write(
            &script,
            "CREATE TABLE t(id INTEGER PRIMARY KEY);\n\
             INSERT INTO t VALUES (1);\nINSERT INTO nope VALUES (1);\n\
             INSERT INTO t VALUES (2);\n",
        )
        .expect("写脚本");

        let mut conn = temp_sqlite(&dir);
        let report = run(
            &mut conn,
            &script,
            &SqlFileOptions::new(100).continue_on_error(),
        )
        .expect("执行文件");

        assert_eq!(report.executed, 3);
        assert_eq!(report.failed, 1);
        assert!(!report.aborted_on_error);
        assert_eq!(report.affected, 2, "建表 0 行 + 两条 INSERT 各 1 行");
        assert_eq!(count(&mut conn, "t"), 2, "失败那条跳过后继续跑");
    }

    /// 整份文件一个事务：中途失败 → 前面几条也不留；且「继续执行」被强制关掉。
    #[test]
    fn single_transaction_rolls_back_the_whole_file() {
        let dir = temp_dir();
        let script = dir.join("txn.sql");
        // 表建在**文件之外**：整份文件的事务回滚掉的必须是脚本里那几条 INSERT
        //（建表本身若在脚本里，回滚后连表都没了，就分不清「回滚生效」还是「没跑」）。
        std::fs::write(
            &script,
            "INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\n\
             INSERT INTO nope VALUES (1);\nINSERT INTO t VALUES (3);\n",
        )
        .expect("写脚本");

        let mut conn = temp_sqlite(&dir);
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", 1)
            .expect("建表");
        // 故意再挂一次 `continue_on_error`：单事务档下它必须无效果。
        let report = run(
            &mut conn,
            &script,
            &SqlFileOptions::new(100)
                .single_transaction()
                .continue_on_error(),
        )
        .expect("执行文件");

        // 单事务档下「遇错即停」被强制生效：失败那条之前的两条跑过，后面那条跳过。
        assert_eq!(report.executed, 2, "{:?}", report.errors);
        assert_eq!(report.failed, 1);
        assert_eq!(report.skipped, 1, "失败之后那条被跳过");
        assert!(report.aborted_on_error);
        assert!(report.transaction_error.is_none(), "回滚本身不该报错");
        assert_eq!(count(&mut conn, "t"), 0, "整份文件的事务要整体回滚");
    }

    /// 边界：空文件、只有注释的文件、最后一条没写分号。
    #[test]
    fn handles_empty_comment_only_and_unterminated_tails() {
        let dir = temp_dir();
        let mut conn = temp_sqlite(&dir);

        let empty = dir.join("empty.sql");
        std::fs::write(&empty, "").expect("写空文件");
        let report = run(&mut conn, &empty, &SqlFileOptions::new(10)).expect("空文件");
        assert_eq!(report.statements, 0);
        assert!(report.all_ok());

        let comments = dir.join("comments.sql");
        std::fs::write(&comments, "-- 只有注释\n/* 还是注释 */\n\n").expect("写注释文件");
        let report = run(&mut conn, &comments, &SqlFileOptions::new(10)).expect("注释文件");
        assert_eq!(report.statements, 0);
        assert_eq!(report.ignored, 1, "切出一段纯注释，直接丢掉");

        // 最后一条没写分号：`finish()` 必须把它交出来。
        let tail = dir.join("tail.sql");
        std::fs::write(
            &tail,
            "CREATE TABLE t(id INTEGER PRIMARY KEY);\nINSERT INTO t VALUES (7)",
        )
        .expect("写文件");
        let report = run(&mut conn, &tail, &SqlFileOptions::new(10)).expect("尾巴");
        assert_eq!(report.executed, 2, "{:?}", report.errors);
        assert_eq!(count(&mut conn, "t"), 1);
    }

    /// 编码：UTF-8 BOM / GBK 都认得出来，且正文不是乱码。
    #[test]
    fn detects_bom_and_gbk() {
        let dir = temp_dir();
        let mut conn = temp_sqlite(&dir);
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)", 1)
            .expect("建表");

        // UTF-8 带 BOM（记事本「UTF-8」另存的样子）。
        let bom = dir.join("bom.sql");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("INSERT INTO t VALUES (1, '中文')".as_bytes());
        std::fs::write(&bom, &bytes).expect("写 BOM 文件");
        let report = run(&mut conn, &bom, &SqlFileOptions::new(10)).expect("BOM 文件");
        assert_eq!(
            report.encoding,
            SqlFileEncoding::Utf8Bom,
            "{:?}",
            report.errors
        );
        assert_eq!(report.executed, 1);

        // GBK：`(2, '中文')` 里的中文用 GBK 编码（`encoding_rs` 反向编码来造样本）。
        let gbk = dir.join("gbk.sql");
        let (encoded, _, _) = GBK.encode("INSERT INTO t VALUES (2, '中文')");
        std::fs::write(&gbk, &encoded).expect("写 GBK 文件");
        let report = run(&mut conn, &gbk, &SqlFileOptions::new(10)).expect("GBK 文件");
        assert_eq!(report.encoding, SqlFileEncoding::Gbk, "{:?}", report.errors);
        assert_eq!(report.executed, 1);
        assert_eq!(count(&mut conn, "t"), 2);
        // 中文没变成 U+FFFD（那是「按 UTF-8 硬解」的症状）。
        let outcome = conn
            .execute("SELECT name FROM t WHERE id = 2", 10)
            .expect("查中文");
        let cell = outcome.result.expect("有结果集").rows[0][0].render();
        assert_eq!(cell, "中文");
    }

    /// 分块读取：脚本远大于一块时，语句仍逐条完整（不因块边界被切歪）。
    #[test]
    fn splits_statements_across_chunk_boundaries() {
        let dir = temp_dir();
        let script = dir.join("big.sql");
        let mut text = String::from("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);\n");
        // 造 6000 条 INSERT，让文件明显超过 256KB 的一块。
        for id in 0..6000 {
            text.push_str(&format!(
                "INSERT INTO t VALUES ({id}, 'name-{id:06}-padding-padding-padding');\n"
            ));
        }
        std::fs::write(&script, &text).expect("写大文件");

        let mut conn = temp_sqlite(&dir);
        assert!(
            text.len() > CHUNK_BYTES,
            "样本要跨块才有意义：{}",
            text.len()
        );
        // 刷批阈值故意设小：跨块 + 跨批都要跑到。
        let options = SqlFileOptions::new(10).flush_at(64, 4096);
        let report = run(&mut conn, &script, &options).expect("执行大文件");

        assert_eq!(report.executed, 6001, "{:?}", report.errors);
        assert_eq!(report.affected, 6000);
        assert_eq!(count(&mut conn, "t"), 6000);
        assert!(
            report.bytes_read > CHUNK_BYTES as u64,
            "确实读了多块：{}",
            report.bytes_read
        );
    }

    /// 上游叫停：谓词命中后剩下的不跑，`stopped` 置位。
    #[test]
    fn stop_predicate_ends_the_run() {
        let dir = temp_dir();
        let script = dir.join("stop.sql");
        let mut text = String::from("CREATE TABLE t(id INTEGER PRIMARY KEY);\n");
        for id in 0..20 {
            text.push_str(&format!("INSERT INTO t VALUES ({id});\n"));
        }
        std::fs::write(&script, &text).expect("写文件");

        let mut conn = temp_sqlite(&dir);
        let done = AtomicUsize::new(0);
        let report = execute_sql_file(
            &mut conn,
            &script,
            &SqlFileOptions::new(10).flush_at(1, 1),
            &|| done.load(Ordering::Relaxed) >= 5,
            &mut |_| {
                done.fetch_add(1, Ordering::Relaxed);
            },
            &mut |_| {},
        )
        .expect("执行文件");

        assert!(report.stopped);
        assert!(report.executed <= 6, "叫停后不该再跑：{}", report.executed);
        assert!(report.failed == 0);
    }

    /// 回执里的错误明细带上「第几条 + 读到多少字节 + 语句原文」。
    #[test]
    fn error_entries_carry_position_and_sql() {
        let dir = temp_dir();
        let script = dir.join("detail.sql");
        std::fs::write(
            &script,
            "CREATE TABLE t(id INTEGER PRIMARY KEY);\nSELECT * FROM missing_table;\n",
        )
        .expect("写脚本");

        let mut conn = temp_sqlite(&dir);
        let report = run(&mut conn, &script, &SqlFileOptions::new(10)).expect("执行文件");
        let first = report.errors.first().expect("有一条错误");
        assert_eq!(first.index, 2);
        assert_eq!(first.sql, "SELECT * FROM missing_table");
        assert!(!first.message.is_empty());
        assert!(first.bytes_read > 0);
        assert_eq!(report.first_error(), Some(first.message.as_str()));
    }
}
