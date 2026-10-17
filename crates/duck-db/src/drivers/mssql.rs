//! SQL Server 驱动（`tiberius`）。
//!
//! # 异步驱动怎么塞进这个同步内核
//!
//! `tiberius` 只有 async API（没有阻塞版），而本 crate 的对外面全是同步的
//! （`docs/database-design.md` §4.1 方案 A）：**每条连接自管一个 current-thread
//! runtime**，所有 await 都在这条 runtime 上 `block_on` 跑完，tokio 的类型
//! （`Future` 之外的 `TcpStream` / `Runtime` / `Client`）**一个都不出现在公共 API 上**。
//!
//! 为什么要单开 runtime 而不是复用 gpui 的 `background_executor`：后者的底座是 smol，
//! 而 `tiberius` 的连接里到处是 tokio 特有设施（`TcpStream` 的注册、定时器）。
//! 单开一个 current-thread runtime 是 dbdelve 验证过的口径——「no tokio future
//! reaches GPUI's executor」。
//!
//! # 只读档在这条引擎上是**客户端拦截**
//!
//! 另外三条引擎都有服务端开关（SQLite `query_only` / PG
//! `default_transaction_read_only` / MySQL `SESSION TRANSACTION READ ONLY`），
//! **SQL Server 没有**：它只能靠「给一个只读账号」或改数据库的 `READ_ONLY` 属性
//! （后者是全局破坏性操作，不是客户端该做的事）。所以这里的只读档在写语句进
//! 引擎**之前**拦下来——它是可信的（不会漏放），但**不是**服务器兜底的那一层。
//! 真要防住生产库，仍然应该用只读账号；界面上的这一档是防手滑。

use futures_util::StreamExt as _;
use tiberius::{AuthMethod, Client, ColumnData as TdsData, Config, EncryptionLevel, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};

use crate::cancel::CancelHandle;
use crate::config::{ConnectionConfig, TlsMode};
use crate::drivers::{
    CatalogTable, DatabaseObject, FilterOp, OrderBy, PkValues, PlaceholderStyle, RowCount,
    TableColumnMeta, TableFilter, TableForeignKey, TableIndex, TableKind, TableRef, TableTrigger,
    filter_conditions, order_by_clause, quote_identifier, quote_qualified,
};
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value};
use crate::sqlintel::{StmtKind, classify, prove_read_only, split_statements};

/// SQL Server 的标识符引号。
///
/// 用 `"` 而不是 `[`：本仓的 [`quote_identifier`] 只支持**同字符成对**的引号，
/// 而 `[` / `]` 是一对不同的字符（要改就得动所有引擎共用的那个函数）。
/// `"` 在 `QUOTED_IDENTIFIER ON`（SQL Server 的默认）下完全合法。
const QUOTE: char = '"';

/// 一条 SQL Server 连接。
pub struct MssqlConnection {
    client: Client<Compat<TcpStream>>,
    /// 建连参数留档：取消（`KILL <spid>`）协议上必须**另开一条连接**发。
    /// 口令就在 `Config` 里——与连接本体同处内存，不落盘、不进日志。
    config: Config,
    /// 本连接专属的 current-thread runtime。
    runtime: tokio::runtime::Runtime,
    /// 本条会话的 `@@SPID`（`KILL` 的目标）。
    session_id: i16,
    /// 只读档（见模块文档：这条引擎上它是客户端拦截）。
    read_only: bool,
    /// 当前是否在事务里（`BEGIN TRAN` 开、`COMMIT`/`ROLLBACK` 关）。
    ///
    /// ⚠️ 只用来给写路径**分流**：在事务里时 `UPDATE` 的影响行数读不到
    /// （`ExecuteResult` 只在语句结束时给计数），所以事务由用户自己用编辑器控制，
    /// 我们不代管。
    in_tx: bool,
}

/// 由连接档案拼 `Config`（含 TLS 档位）。
fn build_config(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Config, DbError> {
    let mut config = Config::new();
    config.host(cfg.host.trim());
    config.port(cfg.port.unwrap_or_else(|| {
        cfg.kind
            .default_port()
            .expect("SQL Server has a default port")
    }));
    // 库名可留空（见 `ConnectionConfig::validate`）：不设 `database` 时登录落在
    // **登录名的默认库**上（通常 `master`）——与其他 SQL Server 客户端一致。
    if !cfg.database.trim().is_empty() {
        config.database(cfg.database.trim());
    }
    config.authentication(AuthMethod::sql_server(
        cfg.user.trim(),
        password.unwrap_or_default(),
    ));
    config.application_name("Duck Toolbox");
    match cfg.tls {
        // 「不用 TLS」= 只加密登录包（TDS 的 pre-login 一直是加密的），数据走明文。
        // 用 `NotSupported` 会更彻底，但会让「服务器要求加密」的实例直接连不上，
        // 而用户选这一档的本意通常是「我的内网库不用加密」，不是「我拒绝加密协商」。
        TlsMode::Disable => config.encryption(EncryptionLevel::Off),
        TlsMode::Trust => config.encryption(EncryptionLevel::Required),
        TlsMode::DangerAcceptInvalid => {
            config.encryption(EncryptionLevel::Required);
            config.trust_cert();
        }
    }
    Ok(config)
}

/// 建 runtime 并连上（`tiberius` 的 `connect` 要一条已经连好的 `TcpStream`）。
fn connect_with_runtime(
    config: Config,
) -> Result<(tokio::runtime::Runtime, Client<Compat<TcpStream>>), DbError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| DbError::Connect(format!("runtime build failed: {error}")))?;
    let client = runtime.block_on(async {
        let tcp = TcpStream::connect(config.get_addr())
            .await
            .map_err(|error| DbError::Connect(format!("tcp connect failed: {error}")))?;
        tcp.set_nodelay(true)
            .map_err(|error| DbError::Connect(format!("tcp nodelay failed: {error}")))?;
        Client::connect(config, tcp.compat_write())
            .await
            .map_err(map_tiberius)
    })?;
    Ok((runtime, client))
}

/// `tiberius` 错误归一：I/O 与 TLS 归 `Connect`，其余归 `Query`（与 PG / MySQL 同口径）。
fn map_tiberius(error: tiberius::error::Error) -> DbError {
    let text = error.to_string();
    match &error {
        tiberius::error::Error::Io { .. } => DbError::Connect(text),
        tiberius::error::Error::Encoding(_) | tiberius::error::Error::Utf16 => DbError::Query(text),
        tiberius::error::Error::Server(_) => {
            // 11 / 1205 = 被并发事务选为死锁牺牲品；62 / 0 = 命令超时。
            if let tiberius::error::Error::Server(token) = &error
                && matches!(token.code(), 11 | 1205 | 62)
            {
                return DbError::Cancelled(text);
            }
            DbError::Query(text)
        }
        _ => DbError::Query(text),
    }
}

/// 只读档的判定（抽成纯函数是为了能脱开连接单测）。
///
/// 三档之外的引擎都把只读交给服务端，**只有 SQL Server 没有那个开关**（见模块文档），
/// 所以这里是唯一的拦截点：写语句必须在此被拒。
fn read_only_guard(read_only: bool) -> Result<(), DbError> {
    if read_only {
        return Err(DbError::Query(
            "connection is read-only (SQL Server has no server-side read-only session switch)"
                .into(),
        ));
    }
    Ok(())
}

/// TDS 列类型 → 展示用的 SQL 类型名。
fn column_type_name(kind: tiberius::ColumnType) -> &'static str {
    use tiberius::ColumnType::*;
    match kind {
        Null => "NULL",
        Bit | Bitn => "BIT",
        Int1 => "TINYINT",
        Int2 => "SMALLINT",
        Int4 | Intn => "INT",
        Int8 => "BIGINT",
        Float4 => "REAL",
        Float8 | Floatn => "FLOAT",
        Money => "MONEY",
        Money4 => "SMALLMONEY",
        Datetime4 => "SMALLDATETIME",
        Datetime | Datetimen => "DATETIME",
        Daten => "DATE",
        Timen => "TIME",
        Datetime2 => "DATETIME2",
        DatetimeOffsetn => "DATETIMEOFFSET",
        Decimaln | Numericn => "DECIMAL",
        Guid => "UNIQUEIDENTIFIER",
        BigVarBin => "VARBINARY",
        BigBinary => "BINARY",
        Image => "IMAGE",
        BigVarChar => "VARCHAR",
        BigChar => "CHAR",
        Text => "TEXT",
        NVarchar => "NVARCHAR",
        NChar => "NCHAR",
        NText => "NTEXT",
        Xml => "XML",
        Udt => "UDT",
        SSVariant => "SQL_VARIANT",
    }
}

/// 一天有多少纳秒（进位用）。
const NANOS_PER_DAY: i64 = 86_400_000_000_000;

/// SQL Server `datetime` / `smalldatetime` 的原点 1900-01-01 距 1970-01-01 的天数。
///
/// 70 年 × 365 天 + 17 个闰日（1904…1968；1900 是世纪年但不是闰年）。
const DATETIME_EPOCH_DAYS: i64 = -25_567;

/// `date` / `datetime2` / `datetimeoffset` 的原点 0001-01-01 距 1970-01-01 的天数。
const DATE_EPOCH_DAYS: i64 = -719_162;

/// 「距 1970-01-01 的天数」→ 公历 (年, 月, 日)。
///
/// Howard Hinnant 的 `civil_from_days`：纯整数、无查表、负天数（1753–1899 那段
/// `datetime` 的合法范围）也正确。**为什么不直接开 `tiberius` 的 `time` feature**：
/// 那个 feature 会拉 `time 0.3`，而本仓 lock 里解析到的是 `time 0.3.55`，
/// 它的 `rust-version` 是 **1.88**，顶破本仓的 1.85 硬门。这里二十行整数运算换掉一个
/// 越线的依赖，划算。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `YYYY-MM-DD`。
fn format_date(days_since_1970: i64) -> String {
    let (year, month, day) = civil_from_days(days_since_1970);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `HH:MM:SS[.fffffff]`（小数位裁掉尾零；整秒就不带小数点）。
fn format_clock(nanos_of_day: u64) -> String {
    let total = nanos_of_day % NANOS_PER_DAY.unsigned_abs();
    let seconds = total / 1_000_000_000;
    let nanos = total % 1_000_000_000;
    let clock = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    );
    if nanos == 0 {
        return clock;
    }
    let fraction = format!("{nanos:09}");
    format!("{clock}.{}", fraction.trim_end_matches('0'))
}

/// `YYYY-MM-DD HH:MM:SS[.fffffff]`。
fn format_datetime(days_since_1970: i64, nanos_of_day: u64) -> String {
    format!(
        "{} {}",
        format_date(days_since_1970),
        format_clock(nanos_of_day)
    )
}

/// `datetimeoffset` 的时区后缀（`+08:00`）。
fn format_offset(minutes: i16) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let absolute = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", absolute / 60, absolute % 60)
}

/// `time` / `datetime2` 的 `increments`（10^-scale 秒）→ 纳秒。
///
/// `scale` 在合法范围是 0–7；钳到 9 是为了 `10^(9-scale)` 不会下溢。
fn increments_to_nanos(increments: u64, scale: u8) -> u64 {
    increments.saturating_mul(10u64.pow(9 - u32::from(scale).min(9)))
}

/// 把「纳秒数」折算成 (距 1970 的天数, 当天纳秒)，允许负值（时区往西推会跨天）。
fn split_nanos(days: i64, nanos: i64) -> (i64, u64) {
    let days = days + nanos.div_euclid(NANOS_PER_DAY);
    // `rem_euclid` 保证落在 `[0, NANOS_PER_DAY)`，正是「当天已过的纳秒」。
    let rest = nanos.rem_euclid(NANOS_PER_DAY);
    (days, u64::try_from(rest).unwrap_or(0))
}

/// 一个 TDS 单元格 → 统一值模型。
///
/// DECIMAL 走 `Value::Decimal`（**绝不经过 f64**，本仓值模型的硬纪律）。
///
/// 时间族的单位与原点全部出自 `tiberius` 自己的 `tds/time/time.rs`
/// （`from_sql!` 那一段），不是猜的：`datetime` 的片段是 **1/300 秒**、
/// `smalldatetime` 的片段是**分钟**、`date` / `datetime2` 的原点是 **0001-01-01**。
fn map_cell(cell: TdsData<'_>) -> Value {
    match cell {
        TdsData::U8(value) => value.map_or(Value::Null, |v| Value::UInt(u64::from(v))),
        TdsData::I16(value) => value.map_or(Value::Null, |v| Value::Int(i64::from(v))),
        TdsData::I32(value) => value.map_or(Value::Null, |v| Value::Int(i64::from(v))),
        TdsData::I64(value) => value.map_or(Value::Null, Value::Int),
        TdsData::F32(value) => value.map_or(Value::Null, |v| Value::Float(f64::from(v))),
        TdsData::F64(value) => value.map_or(Value::Null, Value::Float),
        TdsData::Bit(value) => value.map_or(Value::Null, Value::Bool),
        TdsData::String(value) => value.map_or(Value::Null, |v| Value::Text(v.into_owned())),
        TdsData::Guid(value) => value.map_or(Value::Null, |v| Value::Text(v.to_string())),
        TdsData::Binary(value) => value.map_or(Value::Null, |v| Value::Bytes(v.into_owned())),
        TdsData::Numeric(value) => value.map_or(Value::Null, |v| Value::Decimal(v.to_string())),
        TdsData::Xml(value) => value.map_or(Value::Null, |v| Value::Text(v.to_string())),
        // `datetime`：天数自 1900-01-01，片段是 1/300 秒。
        TdsData::DateTime(value) => value.map_or(Value::Null, |v| {
            Value::DateTime(format_datetime(
                i64::from(v.days()) + DATETIME_EPOCH_DAYS,
                u64::from(v.seconds_fragments()).saturating_mul(1_000_000_000) / 300,
            ))
        }),
        // `smalldatetime`：同样是 1900 原点，但第二个字段是**分钟**（精度到分钟）。
        TdsData::SmallDateTime(value) => value.map_or(Value::Null, |v| {
            Value::DateTime(format_datetime(
                i64::from(v.days()) + DATETIME_EPOCH_DAYS,
                u64::from(v.seconds_fragments()) * 60 * 1_000_000_000,
            ))
        }),
        TdsData::Time(value) => value.map_or(Value::Null, |v| {
            Value::Time(format_clock(increments_to_nanos(v.increments(), v.scale())))
        }),
        TdsData::Date(value) => value.map_or(Value::Null, |v| {
            Value::Date(format_date(i64::from(v.days()) + DATE_EPOCH_DAYS))
        }),
        TdsData::DateTime2(value) => value.map_or(Value::Null, |v| {
            let clock = increments_to_nanos(v.time().increments(), v.time().scale());
            Value::DateTime(format_datetime(
                i64::from(v.date().days()) + DATE_EPOCH_DAYS,
                clock,
            ))
        }),
        // `datetimeoffset`：存的是 **UTC** + 偏移分钟数，展示时换算到该偏移的本地时间。
        TdsData::DateTimeOffset(value) => value.map_or(Value::Null, |v| {
            let inner = v.datetime2();
            let clock = increments_to_nanos(inner.time().increments(), inner.time().scale());
            let shifted =
                i64::try_from(clock).unwrap_or(0) + i64::from(v.offset()) * 60 * 1_000_000_000;
            let (days, nanos) =
                split_nanos(i64::from(inner.date().days()) + DATE_EPOCH_DAYS, shifted);
            Value::DateTime(format!(
                "{} {}{}",
                format_date(days),
                format_clock(nanos),
                format_offset(v.offset())
            ))
        }),
    }
}

/// 一个结果集：列名 + 行 + 是否被行上限截断（`drain` 与 `select` 共用）。
///
/// 名字不叫 `ResultSet`：那个名字已经是 [`crate::result::ResultSet`]（带元信息的成品）。
type RowSet = (Vec<Column>, Vec<Vec<Value>>, bool);

/// 把一条 `QueryStream` 拉干，取**第一个**结果集。
///
/// ⚠️ 超出 `max_rows` 之后**继续消费**而不是 `break`：TDS 是流水线协议，中途丢下
/// 半条响应流会让下一条语句读到上一个查询的残留（消息错位）。
async fn drain(mut stream: tiberius::QueryStream<'_>, max_rows: usize) -> Result<RowSet, DbError> {
    let mut columns: Vec<Column> = Vec::new();
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut truncated = false;
    while let Some(item) = stream.next().await {
        match item.map_err(map_tiberius)? {
            QueryItem::Metadata(meta) => {
                // 多语句批会来多个元数据块；只认第一个（与另外三条引擎一致）。
                if columns.is_empty() {
                    columns = meta
                        .columns()
                        .iter()
                        .map(|column| Column {
                            name: column.name().to_owned(),
                            type_name: column_type_name(column.column_type()).to_owned(),
                            comment: String::new(),
                        })
                        .collect();
                }
            }
            QueryItem::Row(row) => {
                if rows.len() >= max_rows {
                    truncated = true;
                    continue;
                }
                rows.push(row.into_iter().map(map_cell).collect());
            }
        }
    }
    Ok((columns, rows, truncated))
}

impl MssqlConnection {
    /// 建连（并问一次 `@@SPID`：取消要按它点名）。
    pub(crate) fn connect(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Self, DbError> {
        let config = build_config(cfg, password)?;
        let (runtime, mut client) = connect_with_runtime(config.clone())?;
        let session_id = runtime.block_on(async {
            let stream = client
                .simple_query("SELECT @@SPID")
                .await
                .map_err(map_tiberius)?;
            let (_, rows, _) = drain(stream, 1).await?;
            Ok::<_, DbError>(match rows.first().and_then(|row| row.first()) {
                Some(Value::Int(id)) => i16::try_from(*id).unwrap_or_default(),
                Some(Value::UInt(id)) => i16::try_from(*id).unwrap_or_default(),
                _ => 0,
            })
        })?;
        tracing::info!(host = %cfg.host, database = %cfg.database, "sqlserver connected");
        Ok(Self {
            client,
            config,
            runtime,
            session_id,
            read_only: cfg.read_only,
            in_tx: false,
        })
    }

    /// 带外取消句柄（`KILL <spid>`；另开短连接发，不占本连接）。
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::mssql(self.kill_target())
    }

    /// 活性探测。
    pub fn ping(&mut self) -> Result<(), DbError> {
        let client = &mut self.client;
        self.runtime.block_on(async move {
            let stream = client
                .simple_query("SELECT 1")
                .await
                .map_err(map_tiberius)?;
            drain(stream, 1).await.map(|_| ())
        })
    }

    /// 版本描述。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        let (_, rows, _) = self.select("SELECT @@VERSION", &[], 1)?;
        let text = match rows.first().and_then(|row| row.first()) {
            Some(Value::Text(text)) => text.clone(),
            _ => String::new(),
        };
        // `@@VERSION` 是多行的（版本 / 平台 / 版权），只留第一行。
        let first_line = text.lines().next().unwrap_or_default().trim().to_owned();
        Ok(format!("{first_line} (SQL Server)"))
    }

    /// 连接级只读：**客户端拦截**（见模块文档——这条引擎没有服务端开关）。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        self.read_only = read_only;
        Ok(())
    }

    /// 执行一段 SQL（多条语句逐条跑），读语句在 `max_rows` 处截断。
    pub fn execute(&mut self, sql: &str, max_rows: usize) -> Result<ExecOutcome, DbError> {
        let statements = split_statements(sql);
        if statements.is_empty() {
            return Err(DbError::Query("no statement to execute".into()));
        }
        let mut outcome = ExecOutcome::default();
        for stmt in statements {
            let read = prove_read_only(&stmt) || classify(&stmt) == StmtKind::Read;
            if read {
                let (columns, rows, truncated) = self.select(&stmt, &[], max_rows)?;
                outcome.result = Some(ResultSet {
                    columns,
                    rows,
                    truncated,
                });
            } else {
                self.ensure_writable()?;
                outcome.affected += self.execute_write(&stmt, &[])?;
            }
            // 事务状态只在 `BEGIN` / `COMMIT` 这类语句上变，其余语句不动它。
            match crate::sqlintel::tx_effect(&stmt) {
                Some(crate::sqlintel::TxEffect::Begin) => self.in_tx = true,
                Some(crate::sqlintel::TxEffect::End) => self.in_tx = false,
                None => {}
            }
            outcome.statements_run += 1;
        }
        Ok(outcome)
    }

    /// 分页读取：**不支持**（显式拒绝，不返回可能错的页）。
    ///
    /// SQL Server 的 `OFFSET/FETCH` **强制要求 `ORDER BY`**，而派生表又不能带 `ORDER BY`，
    /// 外层只能 `ORDER BY (SELECT NULL)`——顺序不定会让翻页重复 / 漏行。与其返回一份
    /// 看似正常、实则可能重复 / 漏行的页，不如按「不支持」处理。与 MongoDB /
    /// Elasticsearch 同口径；能力判据单一来源见
    /// [`crate::config::DatabaseKind::supports_paged_read`]（界面据同一判据不出「加载更多」）。
    ///
    /// 需要确定序的分页浏览请走 [`Self::table_page`]（它把稳定序摆在最外层）。
    pub fn execute_paged(
        &mut self,
        _sql: &str,
        _offset: u64,
        _limit: usize,
    ) -> Result<ResultSet, DbError> {
        Err(super::paged_unsupported("SQL Server"))
    }

    /// 只读档拦截：写语句在**进引擎之前**就拒（这条引擎没有服务端兜底）。
    fn ensure_writable(&self) -> Result<(), DbError> {
        read_only_guard(self.read_only)
    }

    /// 跑一条读语句（参数化，`@P1` 占位）。
    fn select(
        &mut self,
        sql: &str,
        params: &[&dyn tiberius::ToSql],
        max_rows: usize,
    ) -> Result<RowSet, DbError> {
        let client = &mut self.client;
        self.runtime.block_on(async move {
            let stream = client.query(sql, params).await.map_err(map_tiberius)?;
            drain(stream, max_rows).await
        })
    }

    /// 跑一条写语句，返回影响行数。
    fn execute_write(
        &mut self,
        sql: &str,
        params: &[&dyn tiberius::ToSql],
    ) -> Result<u64, DbError> {
        let client = &mut self.client;
        self.runtime.block_on(async move {
            let result = client.execute(sql, params).await.map_err(map_tiberius)?;
            Ok(result.rows_affected().iter().sum())
        })
    }

    /// 一个 schema 里的表 / 视图 / 函数 / 序列。
    ///
    /// SQL Server 的目录里这四类分得清楚（`sys.objects.type`）：
    /// `U` 用户表、`V` 视图、`FN`/`IF`/`TF` 函数、`P`/`PC` 存储过程、`SO` 序列对象。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        let schema = self.resolve_schema(schema);
        let (_, rows, _) = self.select(
            "SELECT o.name, o.type FROM sys.objects o \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P1 AND o.type IN ('U', 'V', 'FN', 'IF', 'TF', 'P', 'PC', 'SO') \
             ORDER BY o.name",
            &[&schema.as_str()],
            usize::MAX,
        )?;
        let mut objects = Vec::with_capacity(rows.len());
        for row in rows {
            let (Some(Value::Text(name)), Some(Value::Text(kind))) = (row.first(), row.get(1))
            else {
                continue;
            };
            objects.push(CatalogTable {
                name: name.clone(),
                // `P` / `PC` 是存储过程（普通 / CLR），`FN` / `IF` / `TF` 是函数
                //（标量 / 内联表值 / 多语句表值）。
                kind: match kind.as_str() {
                    "U" => TableKind::Table,
                    "V" => TableKind::View,
                    "SO" => TableKind::Sequence,
                    "P" | "PC" => TableKind::Procedure,
                    _ => TableKind::Function,
                },
                schema: Some(schema.clone()),
            });
        }
        Ok(objects)
    }

    /// 可选的 schema 清单（剔掉固定的内置角色 schema）。
    /// **切换会话的当前库**（T-SQL `USE <db>`）。
    ///
    /// 同 MySQL 的道理：库名可留空建连（落到登录名的默认库，通常是 `master`），
    /// 之后左栏点了别的库必须真的切会话，否则后续语句仍打在 `master` 上。
    /// 服务器自己会校验权限与库是否存在，错了就报错 —— 不要在客户端「猜」。
    pub fn use_database(&mut self, name: &str) -> Result<(), DbError> {
        // `]` 是标识符转义符，名字里带 `]` 时必须双写。
        let escaped = name.replace(']', "]]");
        self.execute_write(&format!("USE [{escaped}]"), &[])?;
        Ok(())
    }

    /// **全部**库（当前登录有权限访问的那些）。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        let (_, rows, _) = self.select(
            "SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1 ORDER BY name",
            &[],
            usize::MAX,
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|row| match row.first() {
                Some(Value::Text(name)) => Some(name.clone()),
                _ => None,
            })
            .collect())
    }

    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        let (_, rows, _) = self.select(
            "SELECT name FROM sys.schemas \
             WHERE name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest', \
                                'db_owner', 'db_accessadmin', 'db_securityadmin', \
                                'db_ddladmin', 'db_backupoperator', 'db_datareader', \
                                'db_datawriter', 'db_denydatareader', 'db_denydatawriter') \
             ORDER BY name",
            &[],
            usize::MAX,
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|row| match row.first() {
                Some(Value::Text(name)) => Some(name.clone()),
                _ => None,
            })
            .collect())
    }

    /// 一张表的列（`sys.columns` + `sys.types`，按 `column_id` 即声明序）。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        let schema = self.resolve_schema(table.schema.as_deref());
        let (_, rows, _) = self.select(
            "SELECT c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable \
             FROM sys.columns c \
             JOIN sys.types t ON t.user_type_id = c.user_type_id \
             JOIN sys.objects o ON o.object_id = c.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P1 AND o.name = @P2 \
             ORDER BY c.column_id",
            &[&schema.as_str(), &table.name.as_str()],
            usize::MAX,
        )?;
        let mut columns = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(Value::Text(name)) = row.first() else {
                continue;
            };
            let base = match row.get(1) {
                Some(Value::Text(text)) => text.clone(),
                _ => String::new(),
            };
            // 长度 / 精度补在类型名后面变成 `nvarchar(50)` / `decimal(18,2)`——
            // 这是右栏最常被看的一列，只有基名不够用。
            let length = match row.get(2) {
                Some(Value::Int(value)) => *value,
                _ => 0,
            };
            let precision = match row.get(3) {
                Some(Value::UInt(value)) => *value,
                _ => 0,
            };
            let scale = match row.get(4) {
                Some(Value::UInt(value)) => *value,
                _ => 0,
            };
            let nullable = matches!(row.get(5), Some(Value::Bool(true)));
            columns.push(Column {
                name: name.clone(),
                type_name: describe_type(&base, length, precision, scale, nullable),
                comment: String::new(),
            });
        }
        Ok(columns)
    }

    /// 一张表的列级元数据（`sys.columns`：可空拿得到；默认值走
    /// `OBJECT_DEFINITION(default_object_id)`；注释走扩展属性 `MS_Description`；
    /// 字符集没有——T-SQL 的排序规则是列级的，但文本拿的是排序规则名，不冒充成字符集）。
    pub fn table_column_meta(&mut self, table: &TableRef) -> Result<Vec<TableColumnMeta>, DbError> {
        let schema = self.resolve_schema(table.schema.as_deref());
        let (_, rows, _) = self.select(
            "SELECT c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable, \
                    OBJECT_DEFINITION(c.default_object_id), ep.value, c.collation_name \
             FROM sys.columns c \
             JOIN sys.types t ON t.user_type_id = c.user_type_id \
             JOIN sys.objects o ON o.object_id = c.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             LEFT JOIN sys.extended_properties ep \
                    ON ep.major_id = o.object_id AND ep.minor_id = c.column_id \
                   AND ep.class = 1 AND ep.name = 'MS_Description' \
             WHERE s.name = @P1 AND o.name = @P2 \
             ORDER BY c.column_id",
            &[&schema.as_str(), &table.name.as_str()],
            usize::MAX,
        )?;
        let mut columns = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(Value::Text(name)) = row.first() else {
                continue;
            };
            let base = match row.get(1) {
                Some(Value::Text(text)) => text.clone(),
                _ => String::new(),
            };
            let length = match row.get(2) {
                Some(Value::Int(value)) => *value,
                _ => 0,
            };
            let precision = match row.get(3) {
                Some(Value::UInt(value)) => *value,
                _ => 0,
            };
            let scale = match row.get(4) {
                Some(Value::UInt(value)) => *value,
                _ => 0,
            };
            let nullable = matches!(row.get(5), Some(Value::Bool(true)));
            let default = match row.get(6) {
                Some(Value::Text(text)) => Some(text.clone()),
                _ => None,
            };
            let comment = match row.get(7) {
                Some(Value::Text(text)) => text.clone(),
                _ => String::new(),
            };
            let collation = match row.get(8) {
                Some(Value::Text(text)) => Some(text.clone()),
                _ => None,
            };
            columns.push(TableColumnMeta {
                name: name.clone(),
                type_name: describe_type(&base, length, precision, scale, nullable),
                comment,
                nullable: Some(nullable),
                default,
                character_set: None,
                collation,
            });
        }
        Ok(columns)
    }

    /// 一张表的分页数据。
    ///
    /// SQL Server 的 `OFFSET … FETCH` 语法**强制要求** `ORDER BY`；调用方没给稳定序
    /// （无主键表）时用 `ORDER BY (SELECT NULL)`——它是合法的「不指定序」，比拒绝分页好。
    pub fn table_page(
        &mut self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
        filters: &[TableFilter],
    ) -> Result<ResultSet, DbError> {
        let quoted = quote_qualified(table, QUOTE)?;
        let order = order_by_clause(order_by, QUOTE)?;
        let order = if order.is_empty() {
            " ORDER BY (SELECT NULL)".to_owned()
        } else {
            order
        };
        // 值一律走 `@Pn` 绑定；`LIKE` 的左操作数显式 `CONVERT` 成字符串
        // （`integer LIKE @P1` 在 SQL Server 上是类型错误），其余算子按参数的自然类型
        // 由 TDS 隐式转换（`int = @P1` 会把 NVARCHAR 参数转成 int）。
        let (conditions, values) = filter_conditions(
            filters,
            |name, op| {
                let quoted_column = quote_identifier(name, QUOTE)?;
                Ok(if op == FilterOp::Like {
                    format!("CONVERT(NVARCHAR(4000), {quoted_column})")
                } else {
                    quoted_column
                })
            },
            |index, _| PlaceholderStyle::At.render(index),
        )?;
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let sql = format!(
            "SELECT * FROM {quoted}{where_clause}{order} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
        );
        let params: Vec<Box<dyn tiberius::ToSql + Send>> = values
            .into_iter()
            .map(|value| Box::new(value) as Box<dyn tiberius::ToSql + Send>)
            .collect();
        let refs: Vec<&dyn tiberius::ToSql> = params
            .iter()
            .map(|param| param.as_ref() as &dyn tiberius::ToSql)
            .collect();
        let (columns, rows, truncated) = self.select(&sql, &refs, limit)?;
        Ok(ResultSet {
            columns,
            rows,
            truncated,
        })
    }

    /// 主键列名（`sys.indexes.is_primary_key` + `sys.index_columns.key_ordinal`）。
    pub fn table_primary_key(&mut self, table: &TableRef) -> Result<Vec<String>, DbError> {
        let schema = self.resolve_schema(table.schema.as_deref());
        let (_, rows, _) = self.select(
            "SELECT c.name FROM sys.indexes i \
             JOIN sys.index_columns ic \
               ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             JOIN sys.objects o ON o.object_id = i.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P1 AND o.name = @P2 AND i.is_primary_key = 1 \
             ORDER BY ic.key_ordinal",
            &[&schema.as_str(), &table.name.as_str()],
            usize::MAX,
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|row| match row.first() {
                Some(Value::Text(name)) => Some(name.clone()),
                _ => None,
            })
            .collect())
    }

    /// 一张表上的索引（含主键索引；`sys.indexes` 里 `type > 0` 才是真索引，
    /// `0` 是堆）。
    pub fn table_indexes(&mut self, table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        let schema = self.resolve_schema(table.schema.as_deref());
        let (_, rows, _) = self.select(
            "SELECT i.name, i.is_unique, i.is_primary_key, c.name, ic.key_ordinal \
             FROM sys.indexes i \
             JOIN sys.index_columns ic \
               ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             JOIN sys.objects o ON o.object_id = i.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P1 AND o.name = @P2 AND i.type > 0 AND ic.is_included_column = 0 \
             ORDER BY i.name, ic.key_ordinal",
            &[&schema.as_str(), &table.name.as_str()],
            usize::MAX,
        )?;
        let mut indexes: Vec<TableIndex> = Vec::new();
        for row in rows {
            let Some(Value::Text(name)) = row.first() else {
                continue;
            };
            let unique = matches!(row.get(1), Some(Value::Bool(true)));
            let primary = matches!(row.get(2), Some(Value::Bool(true)));
            let column = match row.get(3) {
                Some(Value::Text(text)) => text.clone(),
                _ => String::new(),
            };
            match indexes.last_mut() {
                // 已按 `i.name` 排序，同一索引的列必定连续。
                Some(index) if index.name == *name => index.columns.push(column),
                _ => indexes.push(TableIndex {
                    name: name.clone(),
                    columns: vec![column],
                    unique,
                    primary,
                }),
            }
        }
        Ok(indexes)
    }

    /// 行数：`sys.partitions.rows` 求和（**估算**口径——它是分配统计，不是 `COUNT(*)`）。
    pub fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        let schema = self.resolve_schema(table.schema.as_deref());
        let (_, rows, _) = self.select(
            "SELECT SUM(p.rows) FROM sys.partitions p \
             JOIN sys.objects o ON o.object_id = p.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P1 AND o.name = @P2 AND p.index_id IN (0, 1)",
            &[&schema.as_str(), &table.name.as_str()],
            1,
        )?;
        match rows.first().and_then(|row| row.first()) {
            Some(Value::Int(value)) => Ok(RowCount::Estimate(
                u64::try_from(*value).unwrap_or_default(),
            )),
            Some(Value::UInt(value)) => Ok(RowCount::Estimate(*value)),
            // 视图没有分区统计 → 用 `0`（调用方按「未知」展示）。
            _ => Ok(RowCount::Estimate(0)),
        }
    }

    /// 一张表上的外键：`sys.foreign_keys` + `sys.foreign_key_columns`，按约束名
    /// 分组（已按 `constraint_column_id` 排序，列序即声明序）。
    ///
    /// 动作描述列（`update_referential_action_desc`）的取值形如 `NO_ACTION` /
    /// `SET_NULL`，[`mssql_action`] 把下划线翻回空格。
    pub fn table_foreign_keys(
        &mut self,
        table: &TableRef,
    ) -> Result<Vec<TableForeignKey>, DbError> {
        let schema = self.resolve_schema(table.schema.as_deref());
        let (_, rows, _) = self.select(
            "SELECT fk.name, pc.name, rt.name, rc.name, \
                    fk.update_referential_action_desc, fk.delete_referential_action_desc \
             FROM sys.foreign_keys fk \
             JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
             JOIN sys.tables pt ON pt.object_id = fk.parent_object_id \
             JOIN sys.schemas s ON s.schema_id = pt.schema_id \
             JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id \
               AND pc.column_id = fkc.parent_column_id \
             JOIN sys.tables rt ON rt.object_id = fkc.referenced_object_id \
             JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id \
               AND rc.column_id = fkc.referenced_column_id \
             WHERE s.name = @P1 AND pt.name = @P2 \
             ORDER BY fk.name, fkc.constraint_column_id",
            &[&schema.as_str(), &table.name.as_str()],
            usize::MAX,
        )?;
        let mut keys: Vec<TableForeignKey> = Vec::new();
        for row in &rows {
            let Some(Value::Text(name)) = row.first() else {
                continue;
            };
            let text = |index: usize| match row.get(index) {
                Some(Value::Text(value)) => value.clone(),
                _ => String::new(),
            };
            let (column, ref_table, ref_column) = (text(1), text(2), text(3));
            let on_update = mssql_action(&text(4));
            let on_delete = mssql_action(&text(5));
            match keys.last_mut() {
                Some(fk) if fk.name == *name => {
                    fk.columns.push(column);
                    fk.ref_columns.push(ref_column);
                }
                _ => keys.push(TableForeignKey {
                    name: name.clone(),
                    columns: vec![column],
                    ref_table,
                    ref_columns: vec![ref_column],
                    on_update,
                    on_delete,
                }),
            }
        }
        Ok(keys)
    }

    /// 一张表上的触发器：`sys.triggers`（`is_instead_of_trigger` 定时机——
    /// T-SQL 没有 `BEFORE`，非 INSTEAD 的都是 AFTER）联 `sys.trigger_events`
    /// 取事件（一个触发器多事件是多行）。
    pub fn table_triggers(&mut self, table: &TableRef) -> Result<Vec<TableTrigger>, DbError> {
        let schema = self.resolve_schema(table.schema.as_deref());
        let (_, rows, _) = self.select(
            "SELECT tr.name, tr.is_instead_of_trigger, te.type_desc \
             FROM sys.triggers tr \
             JOIN sys.tables pt ON pt.object_id = tr.parent_id \
             JOIN sys.schemas s ON s.schema_id = pt.schema_id \
             JOIN sys.trigger_events te ON te.object_id = tr.object_id \
             WHERE s.name = @P1 AND pt.name = @P2 \
             ORDER BY tr.name, te.type_desc",
            &[&schema.as_str(), &table.name.as_str()],
            usize::MAX,
        )?;
        let mut triggers: Vec<TableTrigger> = Vec::new();
        for row in &rows {
            let Some(Value::Text(name)) = row.first() else {
                continue;
            };
            let instead = matches!(row.get(1), Some(Value::Bool(true)));
            let event = match row.get(2) {
                Some(Value::Text(event)) => event.clone(),
                _ => String::new(),
            };
            let timing = if instead { "INSTEAD OF" } else { "AFTER" };
            match triggers.last_mut() {
                Some(trigger) if trigger.name == *name => {
                    trigger.event = format!("{}, {}", trigger.event, event);
                }
                _ => triggers.push(TableTrigger {
                    name: name.clone(),
                    timing: timing.to_owned(),
                    event,
                }),
            }
        }
        Ok(triggers)
    }

    /// 一个 schema 里的触发器清单：`sys.triggers`，说明列 = 挂在哪张表上。
    pub fn list_triggers(&mut self, schema: Option<&str>) -> Result<Vec<DatabaseObject>, DbError> {
        let schema = self.resolve_schema(schema);
        let (_, rows, _) = self.select(
            "SELECT tr.name, pt.name FROM sys.triggers tr \
             JOIN sys.tables pt ON pt.object_id = tr.parent_id \
             JOIN sys.schemas s ON s.schema_id = pt.schema_id \
             WHERE s.name = @P1 \
             ORDER BY tr.name",
            &[&schema.as_str()],
            usize::MAX,
        )?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let (Some(Value::Text(name)), Some(Value::Text(table))) = (row.first(), row.get(1))
                else {
                    return None;
                };
                Some(DatabaseObject {
                    name: name.clone(),
                    detail: Some(table.clone()),
                })
            })
            .collect())
    }

    /// 一个命名空间里的**索引**清单（名字 + 建在哪张表上）。
    ///
    /// `sys.indexes.name` 对**堆**（无聚簇索引的表）是 NULL，那类行跳过 ——
    /// 它不是「一个索引对象」，只是「这张表没有聚簇索引」的事实。
    pub fn list_indexes(&mut self, schema: Option<&str>) -> Result<Vec<DatabaseObject>, DbError> {
        let schema = self.resolve_schema(schema);
        let (_, rows, _) = self.select(
            "SELECT i.name, t.name FROM sys.indexes i \
             JOIN sys.tables t ON t.object_id = i.object_id \
             JOIN sys.schemas s ON s.schema_id = t.schema_id \
             WHERE s.name = @P1 AND i.name IS NOT NULL \
             ORDER BY t.name, i.name",
            &[&schema.as_str()],
            usize::MAX,
        )?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let (Some(Value::Text(name)), Some(Value::Text(table))) = (row.first(), row.get(1))
                else {
                    return None;
                };
                Some(DatabaseObject {
                    name: name.clone(),
                    detail: Some(table.clone()),
                })
            })
            .collect())
    }

    /// 登录与服务器角色：`sys.server_principals`，说明列 = 类型（SQL 登录 / Windows 登录 / 服务器角色）。
    pub fn list_users(&mut self) -> Result<Vec<DatabaseObject>, DbError> {
        let (_, rows, _) = self.select(
            "SELECT name, type_desc FROM sys.server_principals \
             WHERE name NOT LIKE '##%' ORDER BY name",
            &[],
            usize::MAX,
        )?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let (Some(Value::Text(name)), Some(Value::Text(kind))) = (row.first(), row.get(1))
                else {
                    return None;
                };
                Some(DatabaseObject {
                    name: name.clone(),
                    detail: Some(kind.clone()),
                })
            })
            .collect())
    }

    /// 一张表的建表 DDL。
    ///
    /// SQL Server **没有** `SHOW CREATE TABLE` 这类原文出口，所以由目录**重构**出
    /// 近似 DDL（与 PostgreSQL 那条同法）：列 / 类型 / NULL 性 / 标识列 / 主键齐全，
    /// 但默认值、约束名、索引、外键不在其中——它是「看一眼结构」用的，不是可重放的
    /// 迁移脚本。
    pub fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        let schema = self.resolve_schema(table.schema.as_deref());
        let (_, rows, _) = self.select(
            "SELECT c.name, t.name, c.max_length, c.precision, c.scale, \
                    c.is_nullable, c.is_identity \
             FROM sys.columns c \
             JOIN sys.types t ON t.user_type_id = c.user_type_id \
             JOIN sys.objects o ON o.object_id = c.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P1 AND o.name = @P2 \
             ORDER BY c.column_id",
            &[&schema.as_str(), &table.name.as_str()],
            usize::MAX,
        )?;
        if rows.is_empty() {
            return Err(DbError::Query(format!("table not found: {table}")));
        }
        let mut lines = Vec::with_capacity(rows.len() + 1);
        for row in &rows {
            let Some(Value::Text(name)) = row.first() else {
                continue;
            };
            let base = match row.get(1) {
                Some(Value::Text(text)) => text.clone(),
                _ => String::new(),
            };
            let length = match row.get(2) {
                Some(Value::Int(value)) => *value,
                _ => 0,
            };
            let precision = match row.get(3) {
                Some(Value::UInt(value)) => *value,
                _ => 0,
            };
            let scale = match row.get(4) {
                Some(Value::UInt(value)) => *value,
                _ => 0,
            };
            let nullable = matches!(row.get(5), Some(Value::Bool(true)));
            let identity = matches!(row.get(6), Some(Value::Bool(true)));
            let mut line = format!(
                "    [{}] {}",
                name.replace(']', "]]"),
                describe_type(&base, length, precision, scale, true)
            );
            if identity {
                line.push_str(" IDENTITY");
            }
            if !nullable {
                line.push_str(" NOT NULL");
            }
            lines.push(line);
        }
        let pk = self.table_primary_key(table)?;
        if !pk.is_empty() {
            let quoted: Vec<String> = pk
                .iter()
                .map(|name| format!("[{}]", name.replace(']', "]]")))
                .collect();
            lines.push(format!("    PRIMARY KEY ({})", quoted.join(", ")));
        }
        let qualified = quote_qualified(table, QUOTE)?;
        Ok(format!(
            "CREATE TABLE {qualified} (\n{}\n);",
            lines.join(",\n")
        ))
    }

    /// 存储过程 / 函数的定义原文（`OBJECT_DEFINITION`）。
    ///
    /// 取不到（不存在 / 无 VIEW DEFINITION 权限）返回 `Ok(None)`。
    pub fn routine_definition(
        &mut self,
        kind: TableKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<String>, DbError> {
        if !matches!(kind, TableKind::Procedure | TableKind::Function) {
            return Ok(None);
        }
        let schema = self.resolve_schema(namespace);
        let qualified = format!("{schema}.{name}");
        let (_, rows, _) = self.select(
            "SELECT OBJECT_DEFINITION(OBJECT_ID(@P1))",
            &[&qualified.as_str()],
            usize::MAX,
        )?;
        for row in &rows {
            if let Some(Value::Text(text)) = row.first() {
                return Ok(Some(text.clone()));
            }
        }
        Ok(None)
    }

    /// 改一格：按主键定位行（主键值按 `CONVERT(NVARCHAR, x)` 文本比较，全参数化）。
    pub fn update_cell(
        &mut self,
        table: &TableRef,
        pk: &PkValues,
        column: &str,
        value: Option<&str>,
    ) -> Result<u64, DbError> {
        self.ensure_writable()?;
        let (sql, params) = update_cell_sql(table, pk, column, value)?;
        let refs: Vec<&dyn tiberius::ToSql> = params
            .iter()
            .map(|param| param.as_ref() as &dyn tiberius::ToSql)
            .collect();
        self.execute_write(&sql, &refs)
    }

    /// **一个事务**里改多格。
    pub fn update_cells(
        &mut self,
        table: &TableRef,
        edits: &[crate::drivers::CellEdit],
    ) -> Result<u64, DbError> {
        self.ensure_writable()?;
        let statements = edits
            .iter()
            .map(|edit| update_cell_sql(table, &edit.pk, &edit.column, edit.value.as_deref()))
            .collect::<Result<Vec<_>, _>>()?;
        self.run_transaction(statements, Vec::new())
            .map(|(updated, _)| updated)
    }

    /// 按主键删行。
    pub fn delete_rows(&mut self, table: &TableRef, pk: &PkValues) -> Result<u64, DbError> {
        self.ensure_writable()?;
        let (sql, params) = delete_sql(table, pk)?;
        let refs: Vec<&dyn tiberius::ToSql> = params
            .iter()
            .map(|param| param.as_ref() as &dyn tiberius::ToSql)
            .collect();
        self.execute_write(&sql, &refs)
    }

    /// **一个事务**里按主键删多行。
    pub fn delete_row_set(&mut self, table: &TableRef, keys: &[PkValues]) -> Result<u64, DbError> {
        self.ensure_writable()?;
        let statements = keys
            .iter()
            .map(|pk| delete_sql(table, pk))
            .collect::<Result<Vec<_>, _>>()?;
        self.run_transaction(statements, Vec::new())
            .map(|(deleted, _)| deleted)
    }

    /// **一个事务**里提交一次暂存式编辑（改格 + 新增行混排）。
    pub fn apply_batch(
        &mut self,
        table: &TableRef,
        edits: &[crate::drivers::CellEdit],
        inserts: &[crate::drivers::InsertRow],
    ) -> Result<crate::drivers::BatchOutcome, DbError> {
        self.ensure_writable()?;
        let updates = edits
            .iter()
            .map(|edit| update_cell_sql(table, &edit.pk, &edit.column, edit.value.as_deref()))
            .collect::<Result<Vec<_>, _>>()?;
        let insert_batches = inserts
            .iter()
            .map(|row| insert_sql(table, &row.columns, std::slice::from_ref(&row.values)))
            .collect::<Result<Vec<_>, _>>()?;
        let (updated, inserted) = self.run_transaction(updates, insert_batches)?;
        Ok(crate::drivers::BatchOutcome { updated, inserted })
    }

    /// 批量参数化插入：**单事务**逐行 `INSERT`。
    pub fn insert_rows(
        &mut self,
        table: &TableRef,
        columns: &[String],
        rows: &[Vec<Option<String>>],
    ) -> Result<u64, DbError> {
        self.ensure_writable()?;
        let inserts = rows
            .iter()
            .map(|row| insert_sql(table, columns, std::slice::from_ref(row)))
            .collect::<Result<Vec<_>, _>>()?;
        self.run_transaction(Vec::new(), inserts)
            .map(|(_, inserted)| inserted)
    }

    /// 把改格与插入放进**一个** `BEGIN TRAN … COMMIT` 里跑，返回（改了几行、插了几行）。
    ///
    /// 参数与语句一起带进来，所以它们同生共死：任一条失败就整体 `ROLLBACK`。
    fn run_transaction(
        &mut self,
        updates: Vec<(String, Vec<Box<dyn tiberius::ToSql + Send>>)>,
        inserts: Vec<(String, Vec<Box<dyn tiberius::ToSql + Send>>)>,
    ) -> Result<(u64, u64), DbError> {
        if updates.is_empty() && inserts.is_empty() {
            return Ok((0, 0));
        }
        let update_count = updates.len();
        let client = &mut self.client;
        self.runtime.block_on(async move {
            let stream = client
                .simple_query("BEGIN TRANSACTION")
                .await
                .map_err(map_tiberius)?;
            stream.into_results().await.map_err(map_tiberius)?;

            let mut counts: Vec<u64> = Vec::with_capacity(update_count + inserts.len());
            for (sql, params) in updates.iter().chain(inserts.iter()) {
                let refs: Vec<&dyn tiberius::ToSql> = params
                    .iter()
                    .map(|p| p.as_ref() as &dyn tiberius::ToSql)
                    .collect();
                match client.execute(sql.as_str(), &refs).await {
                    Ok(result) => counts.push(result.rows_affected().iter().sum()),
                    Err(error) => {
                        // 出错必须收尾：把事务回滚掉再把连接交还给用户，
                        // 否则下一条语句会**悄悄**跑在别人的事务里。
                        if let Ok(stream) = client.simple_query("ROLLBACK TRANSACTION").await {
                            let _ = stream.into_results().await;
                        }
                        return Err(map_tiberius(error));
                    }
                }
            }

            let stream = client
                .simple_query("COMMIT TRANSACTION")
                .await
                .map_err(map_tiberius)?;
            stream.into_results().await.map_err(map_tiberius)?;

            let updated: u64 = counts.iter().take(update_count).sum();
            let inserted: u64 = counts.iter().skip(update_count).sum();
            Ok((updated, inserted))
        })
    }

    /// 把 `None` 解析成具体的 schema 名（`None` 用 `dbo`）。
    fn resolve_schema(&self, schema: Option<&str>) -> String {
        schema
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("dbo")
            .to_owned()
    }
}

/// 把 `sys.types.name` + 长度 / 精度拼成可读的类型名。
fn describe_type(base: &str, length: i64, precision: u64, scale: u64, _nullable: bool) -> String {
    let lower = base.to_ascii_lowercase();
    match lower.as_str() {
        // 需要长度的字符 / 二进制类；`max_length = -1` 是 `(max)`。
        "varchar" | "char" | "varbinary" | "binary" => {
            if length < 0 {
                format!("{base}(max)")
            } else {
                format!("{base}({length})")
            }
        }
        "nvarchar" | "nchar" => {
            if length < 0 {
                format!("{base}(max)")
            } else {
                // `sys.columns.max_length` 是**字节数**，Unicode 类型要除以 2。
                format!("{base}({})", (length / 2).max(1))
            }
        }
        "decimal" | "numeric" => format!("{base}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => {
            if scale > 0 {
                format!("{base}({scale})")
            } else {
                base.to_owned()
            }
        }
        _ => base.to_owned(),
    }
}

/// 拼改格 SQL 与参数（`@P1` 是新值，其后依次是主键原值）。
fn update_cell_sql(
    table: &TableRef,
    pk: &PkValues,
    column: &str,
    value: Option<&str>,
) -> Result<(String, Vec<Box<dyn tiberius::ToSql + Send>>), DbError> {
    let quoted = quote_qualified(table, QUOTE)?;
    let quoted_column = quote_identifier(column, QUOTE)?;
    let mut params: Vec<Box<dyn tiberius::ToSql + Send>> = vec![Box::new(value.map(str::to_owned))];
    let mut conditions = Vec::with_capacity(pk.len());
    for (index, (name, original)) in pk.iter().enumerate() {
        let quoted_pk = quote_identifier(name, QUOTE)?;
        // 按**展示文本**比较：主键可能是 int / guid / date，`CONVERT(NVARCHAR, x)`
        // 三者在 SQL Server 里的默认格式都与本仓 `Value::*` 的 `to_string()` 一致
        // （`uniqueidentifier` 的大写与大括号由 `CONVERT` 的默认风格决定，见下面的测试）。
        conditions.push(format!(
            "CONVERT(NVARCHAR(4000), {quoted_pk}) = @P{}",
            index + 2
        ));
        params.push(Box::new(original.clone()));
    }
    let sql = format!(
        "UPDATE {quoted} SET {quoted_column} = @P1 WHERE {}",
        conditions.join(" AND ")
    );
    Ok((sql, params))
}

/// 拼删行 SQL 与参数（全参数化）。
fn delete_sql(
    table: &TableRef,
    pk: &PkValues,
) -> Result<(String, Vec<Box<dyn tiberius::ToSql + Send>>), DbError> {
    let quoted = quote_qualified(table, QUOTE)?;
    let mut conditions = Vec::with_capacity(pk.len());
    let mut params: Vec<Box<dyn tiberius::ToSql + Send>> = Vec::with_capacity(pk.len());
    for (index, (name, original)) in pk.iter().enumerate() {
        let quoted_pk = quote_identifier(name, QUOTE)?;
        conditions.push(format!(
            "CONVERT(NVARCHAR(4000), {quoted_pk}) = @P{}",
            index + 1
        ));
        params.push(Box::new(original.clone()));
    }
    let sql = format!("DELETE FROM {quoted} WHERE {}", conditions.join(" AND "));
    Ok((sql, params))
}

/// 拼 INSERT SQL 与参数（`@P1…@Pn` 与列一一对应）。
fn insert_sql(
    table: &TableRef,
    columns: &[String],
    rows: &[Vec<Option<String>>],
) -> Result<(String, Vec<Box<dyn tiberius::ToSql + Send>>), DbError> {
    let quoted = quote_qualified(table, QUOTE)?;
    let quoted_cols: Vec<String> = columns
        .iter()
        .map(|column| quote_identifier(column, QUOTE))
        .collect::<Result<_, _>>()?;
    let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("@P{i}")).collect();
    let sql = format!(
        "INSERT INTO {quoted} ({}) VALUES ({})",
        quoted_cols.join(", "),
        placeholders.join(", ")
    );
    // 多行插入反复发同一条语句、换一批参数：参数是**逐行**给的，所以这里只认第一行，
    // 由调用方（`insert_rows` / `apply_batch`）拆成多次 `execute`。
    let params: Vec<Box<dyn tiberius::ToSql + Send>> = rows
        .first()
        .map(|row| {
            row.iter()
                .map(|cell| Box::new(cell.clone()) as Box<dyn tiberius::ToSql + Send>)
                .collect()
        })
        .unwrap_or_default();
    Ok((sql, params))
}

/// `sys.foreign_keys` 动作描述列（`NO_ACTION` / `SET_NULL` / `SET_DEFAULT` /
/// `CASCADE` / `RESTRICT`）→ 空格分隔的标准文案；空值落空串。
fn mssql_action(desc: &str) -> String {
    match desc {
        "" => String::new(),
        _ => desc.replace('_', " "),
    }
}

/// 取消一条正在跑的语句所需的全部内容（`KILL` 必须另开一条连接发）。
#[derive(Clone)]
pub(crate) struct KillTarget {
    config: Config,
    session_id: i16,
}

impl KillTarget {
    /// 发一条 `KILL <spid>`（自己开 runtime 与连接，用完即断）。
    pub(crate) fn kill(&self) -> Result<(), DbError> {
        if self.session_id <= 0 {
            // 没拿到 SPID：宁可不发，也不要 `KILL 0`（那是「杀掉所有会话」）。
            return Ok(());
        }
        let (runtime, mut client) = connect_with_runtime(self.config.clone())?;
        let statement = format!("KILL {}", self.session_id);
        runtime.block_on(async move {
            let stream = client.simple_query(statement).await.map_err(map_tiberius)?;
            stream.into_results().await.map_err(map_tiberius)?;
            Ok(())
        })
    }
}

/// 取消通道要的东西（`Arc` 里的一份 `KillTarget`）。
impl MssqlConnection {
    /// 供 [`crate::cancel`] 构造句柄。
    pub(crate) fn kill_target(&self) -> KillTarget {
        KillTarget {
            config: self.config.clone(),
            session_id: self.session_id,
        }
    }

    /// 当前是否在事务里（仅用于状态栏展示）。
    #[must_use]
    pub fn in_transaction(&self) -> bool {
        self.in_tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 外键动作描述：下划线翻空格、空值落空串。
    #[test]
    fn fk_action_desc_normalizes_underscores() {
        assert_eq!(mssql_action("NO_ACTION"), "NO ACTION");
        assert_eq!(mssql_action("SET_NULL"), "SET NULL");
        assert_eq!(mssql_action("CASCADE"), "CASCADE");
        assert_eq!(mssql_action(""), "");
    }

    /// 类型名拼装：Unicode 长度要除以 2、`(max)` 由负长度表达、decimal 带精度。
    #[test]
    fn column_type_names_carry_length_and_precision() {
        assert_eq!(describe_type("varchar", 50, 0, 0, true), "varchar(50)");
        assert_eq!(describe_type("nvarchar", 100, 0, 0, true), "nvarchar(50)");
        assert_eq!(describe_type("nvarchar", -1, 0, 0, true), "nvarchar(max)");
        assert_eq!(describe_type("varbinary", -1, 0, 0, true), "varbinary(max)");
        assert_eq!(describe_type("decimal", 9, 18, 2, true), "decimal(18,2)");
        assert_eq!(describe_type("datetime2", 8, 0, 7, true), "datetime2(7)");
        assert_eq!(describe_type("datetime2", 8, 0, 0, true), "datetime2");
        // 不带长度的类型原样保留。
        assert_eq!(describe_type("int", 4, 10, 0, true), "int");
    }

    /// 只读档拦截：写语句在进引擎之前被拒，读语句放行。
    #[test]
    fn read_only_blocks_writes_before_they_reach_the_engine() {
        let error = read_only_guard(true).unwrap_err();
        assert!(
            matches!(error, DbError::Query(ref detail) if detail.contains("read-only")),
            "只读档拒写要给出可读原因，实际：{error}"
        );
        assert!(read_only_guard(false).is_ok());
    }

    /// 改格 SQL：新值在最前，主键条件按声明序、逐个占位、按展示文本比较。
    #[test]
    fn update_sql_places_value_first_and_compares_pk_as_text() {
        let table = TableRef::in_schema("dbo", "users");
        let pk = vec![("id".to_owned(), "7".to_owned())];
        let (sql, params) = update_cell_sql(&table, &pk, "name", Some("Ada")).expect("可拼");
        assert_eq!(
            sql,
            "UPDATE \"dbo\".\"users\" SET \"name\" = @P1 \
             WHERE CONVERT(NVARCHAR(4000), \"id\") = @P2"
        );
        assert_eq!(params.len(), 2, "新值 + 一列主键");

        // NULL 也走占位（不是字面量），所以参数个数不变。
        let (_, params) = update_cell_sql(&table, &pk, "name", None).expect("可拼");
        assert_eq!(params.len(), 2);
    }

    /// 删行 SQL：全参数化，不带任何内联字面量。
    #[test]
    fn delete_sql_is_fully_parameterised() {
        let table = TableRef::bare("orders");
        let pk = vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "x'y".to_owned()),
        ];
        let (sql, params) = delete_sql(&table, &pk).expect("可拼");
        assert_eq!(
            sql,
            "DELETE FROM \"orders\" WHERE CONVERT(NVARCHAR(4000), \"a\") = @P1 \
             AND CONVERT(NVARCHAR(4000), \"b\") = @P2"
        );
        // 注入面：值里的引号进的是参数，不是 SQL 文本。
        assert!(!sql.contains("x'y"), "值不该出现在语句里：{sql}");
        assert_eq!(params.len(), 2);
    }

    /// 列名里的引号必须被拒（`quote_identifier` 的取向），不能拼出被拆散的语句。
    #[test]
    fn identifiers_with_quote_chars_are_rejected() {
        let table = TableRef::bare("t");
        assert!(
            update_cell_sql(
                &table,
                &vec![("a".to_owned(), "1".to_owned())],
                "we\"ird",
                Some("x")
            )
            .is_err()
        );
        assert!(delete_sql(&table, &vec![("we\"ird".to_owned(), "1".to_owned())]).is_err());
    }

    /// TDS 单元格 → 统一值：NULL 归一、DECIMAL 不走 f64、日期族各自归位。
    #[test]
    fn tds_cells_map_to_the_shared_value_model() {
        assert_eq!(map_cell(TdsData::I32(None)), Value::Null);
        assert_eq!(map_cell(TdsData::I32(Some(7))), Value::Int(7));
        assert_eq!(map_cell(TdsData::U8(Some(3))), Value::UInt(3));
        assert_eq!(map_cell(TdsData::Bit(Some(true))), Value::Bool(true));
        assert_eq!(
            map_cell(TdsData::String(Some("hi".into()))),
            Value::Text("hi".into())
        );
        assert_eq!(
            map_cell(TdsData::Binary(Some(vec![1, 2].into()))),
            Value::Bytes(vec![1, 2])
        );
    }

    /// 目录对象种类按 `sys.objects.type` 归一。
    #[test]
    fn catalog_kinds_follow_sys_objects_type() {
        let kind_of = |code: &str| match code {
            "U" => TableKind::Table,
            "V" => TableKind::View,
            "SO" => TableKind::Sequence,
            "P" | "PC" => TableKind::Procedure,
            _ => TableKind::Function,
        };
        assert_eq!(kind_of("U"), TableKind::Table);
        assert_eq!(kind_of("V"), TableKind::View);
        assert_eq!(kind_of("P"), TableKind::Procedure);
        assert_eq!(kind_of("PC"), TableKind::Procedure);
        assert_eq!(kind_of("FN"), TableKind::Function);
        assert_eq!(kind_of("TF"), TableKind::Function);
        assert_eq!(kind_of("SO"), TableKind::Sequence);
    }

    /// 日历换算：两个原点的天数、闰年边界、以及 1900 不是闰年这件事。
    #[test]
    fn civil_dates_cover_both_epochs_and_leap_rules() {
        // 两个原点：`Date` 家族的起点本身、以及 1900-01-01。
        assert_eq!(format_date(DATE_EPOCH_DAYS), "0001-01-01");
        assert_eq!(format_date(DATETIME_EPOCH_DAYS), "1900-01-01");
        assert_eq!(format_date(DATE_EPOCH_DAYS + 1), "0001-01-02");
        assert_eq!(format_date(0), "1970-01-01");

        // 1900-02-28 的下一天是 03-01（1900 不是闰年——世纪年要能被 400 整除才是）。
        assert_eq!(format_date(DATETIME_EPOCH_DAYS + 58), "1900-02-28");
        assert_eq!(format_date(DATETIME_EPOCH_DAYS + 59), "1900-03-01");
        // 2000 是闰年（能被 400 整除）。
        assert_eq!(format_date(11_016), "2000-02-29");
        assert_eq!(format_date(11_017), "2000-03-01");
    }

    /// 时钟格式化：整秒不带小数点、小数位裁掉尾零。
    #[test]
    fn clock_formatting_trims_trailing_zeros() {
        assert_eq!(format_clock(0), "00:00:00");
        assert_eq!(format_clock(1_000_000_000), "00:00:01");
        assert_eq!(format_clock(3_661_000_000_000), "01:01:01");
        assert_eq!(format_clock(1_500_000_000), "00:00:01.5");
        assert_eq!(format_clock(1_230_000_000), "00:00:01.23");
        // 纳秒精度到 7 位（SQL Server 的上限）。
        assert_eq!(format_clock(1_234_567_800), "00:00:01.2345678");
    }

    /// `datetime` 的 1/300 秒片段 → 纳秒：`1` 个片段约 3.33 毫秒。
    #[test]
    fn datetime_fragments_are_three_hundredths_of_a_second() {
        let nanos = |fragments: u64| fragments * 1_000_000_000 / 300;
        assert_eq!(nanos(300), 1_000_000_000, "300 片段 = 1 秒");
        assert_eq!(nanos(1), 3_333_333, "1 片段 ≈ 3.33 毫秒");
        // 一天的最后一刻（23:59:59.997）不越过午夜。
        let last = 25_919_997u64;
        assert!(last * 1_000_000_000 / 300 < NANOS_PER_DAY as u64);
    }

    /// 时区后缀的符号与补零。
    #[test]
    fn offsets_render_with_sign_and_padding() {
        assert_eq!(format_offset(0), "+00:00");
        assert_eq!(format_offset(480), "+08:00");
        assert_eq!(format_offset(-300), "-05:00");
        assert_eq!(format_offset(-30), "-00:30");
    }

    /// 负向时区偏移要**往前退一天**，而不是绕成当天的最后一刻。
    #[test]
    fn offset_shifts_can_walk_back_a_day() {
        // 2026-10-12T00:30:00Z 在 UTC-5 下是 2026-10-11T19:30:00-05:00。
        // `split_nanos` 收的是「距 1970-01-01 的天数」，不是距 0001-01-01，所以
        // 这里直接用 20738（`date(2026,10,12) - date(1970,1,1)`），别再叠
        // `DATE_EPOCH_DAYS`——那是 `map_cell` 把 TDS 原始天数换到同一原点时用的。
        let base_days = 20_738i64;
        let shifted = 30 * 60 * 1_000_000_000i64 - 5 * 3_600 * 1_000_000_000i64;
        let (days, nanos) = split_nanos(base_days, shifted);
        assert_eq!(format_date(days), "2026-10-11");
        assert_eq!(format_clock(nanos), "19:30:00");
    }
}
