//! MySQL / MariaDB 驱动（阻塞版 `mysql`，rustls TLS）。
//!
//! `caching_sha2_password` 的完整握手需要 TLS 通道：TLS 档位为 `Disable` 且服务器
//! 只开了默认认证插件时，首次连接可能被服务器拒绝——换 `Trust` 档或把账号改成
//! `mysql_native_password` 都能绕开。

use mysql::prelude::Queryable as _;

use crate::cancel::CancelHandle;
use crate::config::{ConnectionConfig, TlsMode};
use crate::drivers::{
    CatalogTable, DatabaseObject, InstanceFact, InstanceOverview, InstanceTable, InstanceView,
    OrderBy, PlaceholderStyle, RowCount, TableFilter, TableForeignKey, TableIndex, TableKind,
    TableRef, TableTrigger, human_duration_secs, order_by_clause, plain_filter_clause,
    quote_identifier, quote_qualified,
};
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value};
use crate::sqlintel::{StmtKind, prove_read_only, split_statements};

/// 实例指标要展示的 `SHOW GLOBAL STATUS` 项——**顺序即展示顺序**。
///
/// 只挑「看一眼能判断服务器忙不忙、稳不稳」的那些：连接与线程、请求量、慢查询与
/// 临时表、流量、InnoDB 缓冲池命中与锁等待。全量三千多项是给监控系统抓的，不是给人看的。
const METRIC_WHITELIST: [&str; 16] = [
    "Uptime",
    "Threads_connected",
    "Threads_running",
    "Connections",
    "Aborted_connects",
    "Queries",
    "Questions",
    "Slow_queries",
    "Created_tmp_tables",
    "Created_tmp_disk_tables",
    "Bytes_received",
    "Bytes_sent",
    "Innodb_buffer_pool_read_requests",
    "Innodb_buffer_pool_reads",
    "Innodb_row_lock_waits",
    "Table_locks_waited",
];

/// 实例概览的一项（名字 = 引擎口径原文，值 = 原样字符串）。
fn fact(name: &str, value: String) -> InstanceFact {
    InstanceFact {
        name: name.to_owned(),
        value,
    }
}

/// 一条 MySQL 连接。
pub struct MysqlConnection {
    conn: mysql::Conn,
    /// 建连参数留档：`KILL QUERY` 协议上必须**另开一条连接**发，取消时靠它重连。
    /// 口令就在 `Opts` 里——与连接本体同处内存，不落盘、不进日志。
    opts: mysql::Opts,
}

/// 由连接档案拼 `OptsBuilder`（含 TLS 档位）。
fn build_opts(cfg: &ConnectionConfig, password: Option<&str>) -> Result<mysql::Opts, DbError> {
    let port = cfg
        .port
        .unwrap_or_else(|| cfg.kind.default_port().expect("MySQL has a default port"));
    // 库名可留空（见 `ConnectionConfig::validate`）：给了 `None` 就不发 COM_INIT_DB，
    // 连上后 `SELECT DATABASE()` 回 NULL——这正是其他客户端「不选库连上」的行为。
    // （传 `Some("")` 驱动也能兜住，但语义上「没给」比「给了个空名」准确。）
    let database = cfg.database.trim();
    let mut builder = mysql::OptsBuilder::new()
        .ip_or_hostname(Some(cfg.host.trim()))
        .tcp_port(port)
        .prefer_socket(false)
        .tcp_nodelay(true)
        .user(Some(cfg.user.trim()))
        .db_name((!database.is_empty()).then(|| database.to_owned()));
    if let Some(password) = password {
        builder = builder.pass(Some(password));
    }
    match cfg.tls {
        TlsMode::Disable => {}
        // 默认信任链（webpki-roots）；自签库请选下一档。
        TlsMode::Trust => builder = builder.ssl_opts(Some(mysql::SslOpts::default())),
        TlsMode::DangerAcceptInvalid => {
            builder = builder.ssl_opts(Some(
                mysql::SslOpts::default().with_danger_accept_invalid_certs(true),
            ));
        }
    }
    Ok(builder.into())
}

/// `mysql::ColumnType` → 展示类型名（取常用族；未知走 OTHER）。
fn column_type_name(t: mysql::consts::ColumnType) -> &'static str {
    use mysql::consts::ColumnType::*;
    match t {
        MYSQL_TYPE_TINY => "TINYINT",
        MYSQL_TYPE_SHORT => "SMALLINT",
        MYSQL_TYPE_LONG => "INT",
        MYSQL_TYPE_LONGLONG => "BIGINT",
        MYSQL_TYPE_INT24 => "MEDIUMINT",
        MYSQL_TYPE_FLOAT => "FLOAT",
        MYSQL_TYPE_DOUBLE => "DOUBLE",
        MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_DECIMAL => "DECIMAL",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "DATE",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "TIME",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "DATETIME",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "TIMESTAMP",
        MYSQL_TYPE_YEAR => "YEAR",
        MYSQL_TYPE_BIT => "BIT",
        MYSQL_TYPE_JSON => "JSON",
        MYSQL_TYPE_BLOB | MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB => {
            "BLOB"
        }
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => "VARCHAR",
        MYSQL_TYPE_STRING => "CHAR",
        MYSQL_TYPE_ENUM => "ENUM",
        MYSQL_TYPE_SET => "SET",
        MYSQL_TYPE_GEOMETRY => "GEOMETRY",
        MYSQL_TYPE_NULL => "NULL",
        _ => "OTHER",
    }
}

/// 单元格值映射（`column_type` 用于把 DECIMAL 的字节承载改挂成 `Value::Decimal`）。
fn map_value(raw: &mysql::Value, is_decimal: bool) -> Value {
    match raw {
        mysql::Value::NULL => Value::Null,
        mysql::Value::Bytes(bytes) => {
            let text = String::from_utf8_lossy(bytes.as_slice()).into_owned();
            if is_decimal {
                Value::Decimal(text)
            } else {
                Value::Text(text)
            }
        }
        mysql::Value::Int(v) => Value::Int(*v),
        mysql::Value::UInt(v) => Value::UInt(*v),
        mysql::Value::Float(v) => Value::Float(f64::from(*v)),
        mysql::Value::Double(v) => Value::Float(*v),
        mysql::Value::Date(y, m, d, hh, mm, ss, us) => {
            let mut text = format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}");
            if *us > 0 {
                text.push_str(&format!(".{us:06}"));
            }
            Value::DateTime(text)
        }
        mysql::Value::Time(neg, d, hh, mm, ss, us) => {
            let sign = if *neg { "-" } else { "" };
            let hours = u64::from(*d) * 24 + u64::from(*hh);
            let mut text = format!("{sign}{hours:02}:{mm:02}:{ss:02}");
            if *us > 0 {
                text.push_str(&format!(".{us:06}"));
            }
            Value::Time(text)
        }
    }
}

/// 把一条查询结果收成 [`ResultSet`]（文本协议与二进制协议共用一份映射）。
///
/// 截断判定在**取到下一行之后**：`max_rows` 行已满且还有行，才算「被截断」——
/// 这正是「多取一条」的探测口径。
fn map_query_result<T: mysql::prelude::Protocol>(
    result: mysql::QueryResult<'_, '_, '_, T>,
    max_rows: usize,
) -> Result<ResultSet, DbError> {
    let columns: Vec<Column> = result
        .columns()
        .as_ref()
        .iter()
        .map(|col| Column {
            name: col.name_str().into_owned(),
            type_name: column_type_name(col.column_type()).to_owned(),
        })
        .collect();
    // 定点数绝不进 `f64`：这两类列的值按字符串承载（`Value::Decimal`）。
    let decimal_flags: Vec<bool> = result
        .columns()
        .as_ref()
        .iter()
        .map(|col| {
            matches!(
                col.column_type(),
                mysql::consts::ColumnType::MYSQL_TYPE_NEWDECIMAL
                    | mysql::consts::ColumnType::MYSQL_TYPE_DECIMAL
            )
        })
        .collect();

    let mut rows_out = Vec::new();
    let mut truncated = false;
    for row in result {
        let row = row.map_err(DbError::from)?;
        if rows_out.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(columns.len());
        for (index, is_decimal) in decimal_flags.iter().enumerate() {
            let raw = row.as_ref(index).cloned().unwrap_or(mysql::Value::NULL);
            cells.push(map_value(&raw, *is_decimal));
        }
        rows_out.push(cells);
    }
    Ok(ResultSet {
        columns,
        rows: rows_out,
        truncated,
    })
}

impl MysqlConnection {
    /// 建连。
    pub(crate) fn connect(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Self, DbError> {
        let opts = build_opts(cfg, password)?;
        let conn = mysql::Conn::new(opts.clone())?;
        tracing::info!(host = %cfg.host, database = %cfg.database, "mysql connected");
        Ok(Self { conn, opts })
    }

    /// 带外取消句柄（`KILL QUERY <connection_id>`；另开短连接发，不占本连接的锁）。
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::mysql(self.opts.clone(), self.conn.connection_id())
    }

    /// 活性探测。
    pub fn ping(&mut self) -> Result<(), DbError> {
        self.conn.query_iter("SELECT 1")?.for_each(|_| {});
        Ok(())
    }

    /// 版本描述。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        let mut text = String::new();
        for row in self.conn.query_iter("SELECT VERSION()")? {
            if let mysql::Value::Bytes(bytes) =
                row?.as_ref(0).cloned().unwrap_or(mysql::Value::NULL)
            {
                text = String::from_utf8_lossy(&bytes).into_owned();
            }
        }
        Ok(format!("{text} (MySQL/MariaDB)"))
    }

    /// 连接级只读：`SET SESSION TRANSACTION READ ONLY`（服务器自己拒绝写）。
    ///
    /// ⚠️ MySQL 只对**事务内**的写入生效：`READ ONLY` 限制的是当前会话后续事务的写，
    /// 而 MySQL 默认 autocommit，每条语句就是一个事务 —— 所以对写语句同样会拒。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        let value = if read_only { "READ ONLY" } else { "READ WRITE" };
        self.conn
            .query_iter(format!("SET SESSION TRANSACTION {value}"))?
            .for_each(|_| {});
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
            let read = prove_read_only(&stmt) || crate::sqlintel::classify(&stmt) == StmtKind::Read;
            if read {
                outcome.result = Some(self.query_one(&stmt, mysql::Params::Empty, max_rows)?);
            } else {
                let result = self.conn.query_iter(&stmt)?;
                outcome.affected += result.affected_rows();
            }
            outcome.statements_run += 1;
        }
        Ok(outcome)
    }

    /// 分页读取（结果区「加载更多」走这里）：把语句包一层 `LIMIT/OFFSET`，
    /// 从第 `offset` 行起取 `limit` 行；多取一行探测「还有下一页」。
    ///
    /// 语义与边界见 [`crate::drivers::Connection::execute_paged`]。
    pub fn execute_paged(
        &mut self,
        sql: &str,
        offset: u64,
        limit: usize,
    ) -> Result<ResultSet, DbError> {
        let inner = super::paged_inner_sql(sql)?;
        let wrapped = super::wrap_paged_sql(&inner, offset, limit, '`')?;
        self.query_one(&wrapped, mysql::Params::Empty, limit)
    }

    /// 跑一条读语句并取结果集（`max_rows + 1` 探测截断）。
    ///
    /// `params` 为空时**照旧走文本协议**（`query_iter`，与加过滤之前逐字节一致）；
    /// 带过滤值时只能走参数化路径（`exec_iter`，二进制协议）——值一律以 `Bytes` 绑定。
    fn query_one(
        &mut self,
        stmt: &str,
        params: mysql::Params,
        max_rows: usize,
    ) -> Result<ResultSet, DbError> {
        if matches!(params, mysql::Params::Empty) {
            let result = self.conn.query_iter(stmt)?;
            map_query_result(result, max_rows)
        } else {
            let result = self.conn.exec_iter(stmt, params)?;
            map_query_result(result, max_rows)
        }
    }

    /// 可选 schema 清单（MySQL 里 schema 与库同义）。
    ///
    /// 剔除四个系统库：它们必然存在、且永远不会被当作业务对象浏览。
    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        let mut schemas = Vec::new();
        let result = self.conn.query_iter(
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name NOT IN ('information_schema', 'performance_schema', 'mysql', 'sys') \
             ORDER BY schema_name",
        )?;
        for row in result {
            let row = row.map_err(DbError::from)?;
            if let Some(mysql::Value::Bytes(bytes)) = row.as_ref(0) {
                schemas.push(String::from_utf8_lossy(bytes.as_slice()).into_owned());
            }
        }
        Ok(schemas)
    }

    /// **切换会话的当前库**（`COM_INIT_DB`）。
    ///
    /// 为什么必须真发这条：连接时库名可以留空（`build_opts` 不发 `COM_INIT_DB`，
    /// 服务器侧 `DATABASE()` 回 `NULL`）。此后用户就算在左栏点了某个库，若只把
    /// 名字记在客户端，编辑器里跑一句 `SELECT * FROM t` 仍会服务端报
    /// `No database selected` —— 界面「切了」，查询没切。dbx / DBeaver 的
    /// 库节点点击都真的改服务端会话，这里对齐。
    pub fn use_database(&mut self, name: &str) -> Result<(), DbError> {
        // 反引号是标识符引用符，名字里带反引号时必须双写，否则整条语句被拆成两半。
        let escaped = name.replace('`', "``");
        self.conn
            .query_iter(format!("USE `{escaped}`"))?
            .for_each(|_| {});
        Ok(())
    }

    /// **全部**库（含四个系统库）。
    ///
    /// 与 [`Self::list_schemas`] 的分工：那个给的是「业务对象浏览」用的清单
    ///（剔掉 `mysql` / `sys` / `information_schema` / `performance_schema`），
    /// 这里是「连接上有哪些库」的全量读数 —— 用户连上还没选库时最先想问的
    /// 就是这道题，dbx 的库树也是全量。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        let mut names = Vec::new();
        let result = self.conn.query_iter(
            "SELECT schema_name FROM information_schema.schemata ORDER BY schema_name",
        )?;
        for row in result {
            let row = row.map_err(DbError::from)?;
            if let Some(mysql::Value::Bytes(bytes)) = row.as_ref(0) {
                names.push(String::from_utf8_lossy(bytes.as_slice()).into_owned());
            }
        }
        Ok(names)
    }

    /// 把「当前库」解析成一个具体名字（`None` 时问一次服务端）。
    fn resolve_schema(&mut self, schema: Option<&str>) -> Result<String, DbError> {
        if let Some(schema) = schema {
            return Ok(schema.to_owned());
        }
        let mut name = String::new();
        for row in self.conn.query_iter("SELECT DATABASE()")? {
            if let Some(mysql::Value::Bytes(bytes)) = row?.as_ref(0) {
                name = String::from_utf8_lossy(bytes.as_slice()).into_owned();
            }
        }
        Ok(name)
    }

    /// 一个库里的表 / 视图 / 函数（MySQL 没有序列）。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        let schema = self.resolve_schema(schema)?;
        let mut tables = Vec::new();
        let result = self.conn.exec_iter(
            "SELECT table_name, table_type FROM information_schema.tables \
             WHERE table_schema = ? \
               AND table_type IN ('BASE TABLE', 'VIEW') \
             ORDER BY lower(table_name)",
            (&schema,),
        )?;
        for row in result {
            let row = row.map_err(DbError::from)?;
            let name = match row.as_ref(0) {
                Some(mysql::Value::Bytes(bytes)) => {
                    String::from_utf8_lossy(bytes.as_slice()).into_owned()
                }
                _ => continue,
            };
            let kind = match row.as_ref(1) {
                Some(mysql::Value::Bytes(bytes)) if bytes.as_slice() == b"VIEW" => TableKind::View,
                _ => TableKind::Table,
            };
            tables.push(CatalogTable {
                name,
                kind,
                schema: Some(schema.clone()),
            });
        }

        // 存储过程 / 函数：`routine_type` 分得清（`PROCEDURE` / `FUNCTION`），
        // 左栏树要按两棵分类节点展示，不能混在一起。
        let routines = self.conn.exec_iter(
            "SELECT routine_name, routine_type FROM information_schema.routines \
             WHERE routine_schema = ? ORDER BY lower(routine_name)",
            (&schema,),
        )?;
        for row in routines {
            let row = row.map_err(DbError::from)?;
            let name = Self::text_at(&row, 0);
            if name.is_empty() {
                continue;
            }
            let kind = if Self::text_at(&row, 1) == "PROCEDURE" {
                TableKind::Procedure
            } else {
                TableKind::Function
            };
            tables.push(CatalogTable {
                name,
                kind,
                schema: Some(schema.clone()),
            });
        }
        Ok(tables)
    }

    /// 触发器清单：`information_schema.TRIGGERS`，说明列 = 挂在哪张表上。
    pub fn list_triggers(&mut self, schema: Option<&str>) -> Result<Vec<DatabaseObject>, DbError> {
        let schema = self.resolve_schema(schema)?;
        let mut objects = Vec::new();
        let result = self.conn.exec_iter(
            "SELECT trigger_name, event_object_table FROM information_schema.triggers \
             WHERE trigger_schema = ? ORDER BY lower(trigger_name)",
            (&schema,),
        )?;
        for row in result {
            let row = row.map_err(DbError::from)?;
            let name = Self::text_at(&row, 0);
            if name.is_empty() {
                continue;
            }
            objects.push(DatabaseObject {
                name,
                detail: Some(Self::text_at(&row, 1)),
            });
        }
        Ok(objects)
    }

    /// 事件清单：`information_schema.EVENTS`，说明列 = 调度状态（ENABLED / DISABLED…）。
    pub fn list_events(&mut self, schema: Option<&str>) -> Result<Vec<DatabaseObject>, DbError> {
        let schema = self.resolve_schema(schema)?;
        let mut objects = Vec::new();
        let result = self.conn.exec_iter(
            "SELECT event_name, status FROM information_schema.events \
             WHERE event_schema = ? ORDER BY lower(event_name)",
            (&schema,),
        )?;
        for row in result {
            let row = row.map_err(DbError::from)?;
            let name = Self::text_at(&row, 0);
            if name.is_empty() {
                continue;
            }
            objects.push(DatabaseObject {
                name,
                detail: Some(Self::text_at(&row, 1)),
            });
        }
        Ok(objects)
    }

    /// 用户清单：`mysql.user`（`user` + `host` 共同决定一个账号）。
    ///
    /// 查系统表需要读权限；取不到（权限不足）就让上层拿空清单——树少一个
    /// 分类节点，不该把整条连接判死。
    pub fn list_users(&mut self) -> Result<Vec<DatabaseObject>, DbError> {
        let mut objects = Vec::new();
        let result = self.conn.exec_iter(
            "SELECT user, host FROM mysql.user ORDER BY lower(user), host",
            (),
        )?;
        for row in result {
            let row = row.map_err(DbError::from)?;
            let name = Self::text_at(&row, 0);
            if name.is_empty() {
                continue;
            }
            objects.push(DatabaseObject {
                name,
                detail: Some(Self::text_at(&row, 1)),
            });
        }
        Ok(objects)
    }

    /// 实例概览：版本 / 当前库 / 登录身份 / 在线时长 / 服务器标识与字符集。
    pub fn instance_overview(&mut self) -> Result<InstanceOverview, DbError> {
        let version = self.server_version()?;
        let head = self.query_text_row(
            "SELECT COALESCE(DATABASE(), ''), CURRENT_USER(), @@hostname, @@port, \
                    @@character_set_server, @@collation_server, @@max_connections",
            7,
        )?;
        let get = |index: usize| head.get(index).cloned().unwrap_or_default();
        let uptime = self
            .status_value("Uptime")
            .parse::<u64>()
            .ok()
            .map(human_duration_secs);
        let databases = self
            .query_text_row("SELECT COUNT(*) FROM information_schema.SCHEMATA", 1)?
            .first()
            .cloned()
            .unwrap_or_default();
        Ok(InstanceOverview {
            version,
            database: get(0),
            user: get(1),
            uptime: uptime.clone(),
            facts: vec![
                fact("uptime", uptime.unwrap_or_default()),
                fact("hostname", get(2)),
                fact("port", get(3)),
                fact("character_set_server", get(4)),
                fact("collation_server", get(5)),
                fact("max_connections", get(6)),
                fact("databases", databases),
            ],
        })
    }

    /// 实例指标 / 检查器两张表。
    ///
    /// - **指标**：`SHOW GLOBAL STATUS` 里**白名单**的那些项（不是全量几千行——全量里
    ///   九成是 InnoDB 的细粒度计数器，摊在界面上没法看）；
    /// - **检查器**：`SHOW FULL PROCESSLIST`（含本连接自己那一条）。
    pub fn instance_table(&mut self, view: InstanceView) -> Result<InstanceTable, DbError> {
        match view {
            InstanceView::Metrics => {
                let mut values: Vec<(String, String)> = Vec::new();
                for row in self.conn.query_iter("SHOW GLOBAL STATUS")? {
                    let row = row.map_err(DbError::from)?;
                    let name = Self::text_at(&row, 0);
                    if METRIC_WHITELIST.contains(&name.as_str()) {
                        values.push((name, Self::text_at(&row, 1)));
                    }
                }
                // 按白名单顺序输出：`SHOW GLOBAL STATUS` 的返回序不保证稳定，
                // 照抄会让同一台服务器每次打开的指标顺序都不一样。
                let rows = METRIC_WHITELIST
                    .iter()
                    .filter_map(|name| {
                        values
                            .iter()
                            .find(|(key, _)| key == name)
                            .map(|(key, value)| vec![key.clone(), value.clone()])
                    })
                    .collect();
                Ok(InstanceTable {
                    columns: vec!["metric".into(), "value".into()],
                    rows,
                })
            }
            InstanceView::Processes => {
                let mut rows = Vec::new();
                for row in self.conn.query_iter("SHOW FULL PROCESSLIST")? {
                    let row = row.map_err(DbError::from)?;
                    rows.push(
                        (0..8)
                            .map(|index| Self::text_at(&row, index))
                            .collect::<Vec<_>>(),
                    );
                }
                Ok(InstanceTable {
                    columns: [
                        "id", "user", "host", "database", "command", "time", "state", "query",
                    ]
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
                    rows,
                })
            }
        }
    }

    /// 跑一条查询并把**第一行**的前 `columns` 列取成字符串（没有行则回空 `Vec`）。
    ///
    /// 用它而不是「先拿 `Row` 再逐列取」：`mysql::Row` 借自 `QueryResult` 那个临时值，
    /// 出了语句就被释放（E0716）。当场取成 owned 字符串才留得住。
    fn query_text_row(&mut self, sql: &str, columns: usize) -> Result<Vec<String>, DbError> {
        // `next()` 而不是 `for`：这些内省语句都只回一行，循环体里必然 `return`，
        // 写成循环会被 clippy 判成 `never_loop`（而且确实没有第二趟）。
        let row = self
            .conn
            .query_iter(sql)?
            .next()
            .transpose()
            .map_err(DbError::from)?;
        let Some(row) = row else {
            return Ok(Vec::new());
        };
        Ok((0..columns)
            .map(|index| Self::text_at(&row, index))
            .collect())
    }

    /// 一条 `SHOW GLOBAL STATUS LIKE '…'` 的值（取不到回落空串）。
    ///
    /// `name` 只传本文件里的常量，不接用户输入——`LIKE` 的引号是字符串拼接，不参数化。
    fn status_value(&mut self, name: &str) -> String {
        self.query_text_row(&format!("SHOW GLOBAL STATUS LIKE '{name}'"), 2)
            .ok()
            .and_then(|cells| cells.get(1).cloned())
            .unwrap_or_default()
    }

    /// 一张表的列（参数化查询）。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        let schema = table.schema.as_deref();
        let mut columns = Vec::new();
        let result = self.conn.exec_iter(
            "SELECT column_name, column_type FROM information_schema.columns \
             WHERE table_schema = COALESCE(?, DATABASE()) AND table_name = ? \
             ORDER BY ordinal_position",
            (schema, table.name.as_str()),
        )?;
        for row in result {
            let row = row.map_err(DbError::from)?;
            let name = match row.as_ref(0) {
                Some(mysql::Value::Bytes(bytes)) => {
                    String::from_utf8_lossy(bytes.as_slice()).into_owned()
                }
                _ => continue,
            };
            let type_name = match row.as_ref(1) {
                Some(mysql::Value::Bytes(bytes)) => {
                    String::from_utf8_lossy(bytes.as_slice()).into_owned()
                }
                _ => String::new(),
            };
            columns.push(Column { name, type_name });
        }
        Ok(columns)
    }

    /// 一张表的分页数据。
    ///
    /// `order_by` 是调用方给的稳定序（通常是主键）：MySQL 的 `LIMIT/OFFSET` 在无
    /// `ORDER BY` 时顺序取决于执行计划（走不同索引顺序就不同），翻页会重复 / 漏行。
    pub(crate) fn table_page(
        &mut self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
        filters: &[TableFilter],
    ) -> Result<ResultSet, DbError> {
        let quoted = quote_qualified(table, '`')?;
        let order = order_by_clause(order_by, '`')?;
        let (where_clause, values) = plain_filter_clause(filters, '`', PlaceholderStyle::Question)?;
        let sql =
            format!("SELECT * FROM {quoted}{where_clause}{order} LIMIT {limit} OFFSET {offset}");
        // 值一律以 `Bytes`（即文本）绑定：MySQL 按列的隐式转换规则自己转。
        let params = if values.is_empty() {
            mysql::Params::Empty
        } else {
            mysql::Params::Positional(
                values
                    .into_iter()
                    .map(|value| mysql::Value::Bytes(value.into_bytes()))
                    .collect(),
            )
        };
        self.query_one(&sql, params, limit)
    }

    /// 主键列名（指定库的主键约束列，按声明序）。
    ///
    /// ⚠️ JOIN 必须带上 `TABLE_NAME`：MySQL 的主键约束名**一律叫 `PRIMARY`**，
    /// 只按名字 + schema 关联会把同库**所有**有主键的表都连进来（混入别表的
    /// 主键列 → 翻页 `ORDER BY` 一个不存在的列，服务端报 1054）。
    pub fn table_primary_key(&mut self, table: &TableRef) -> Result<Vec<String>, DbError> {
        let schema = table.schema.as_deref();
        let result = self
            .conn
            .exec_iter(
                "SELECT kcu.COLUMN_NAME FROM information_schema.TABLE_CONSTRAINTS tc \
                 JOIN information_schema.KEY_COLUMN_USAGE kcu \
                   ON tc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME \
                  AND tc.TABLE_SCHEMA = kcu.TABLE_SCHEMA \
                  AND tc.TABLE_NAME = kcu.TABLE_NAME \
                 WHERE tc.CONSTRAINT_TYPE = 'PRIMARY KEY' \
                   AND tc.TABLE_SCHEMA = COALESCE(?, DATABASE()) AND tc.TABLE_NAME = ? \
                 ORDER BY kcu.ORDINAL_POSITION",
                (schema, table.name.as_str()),
            )
            .map_err(DbError::from)?;
        let mut columns = Vec::new();
        for row in result {
            if let mysql::Value::Bytes(bytes) = row
                .map_err(DbError::from)?
                .as_ref(0)
                .cloned()
                .unwrap_or(mysql::Value::NULL)
            {
                columns.push(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        Ok(columns)
    }

    /// 改一格：主键值按 `CAST(x AS CHAR)` 文本比较（参数化）。`None` 写 `NULL`。
    pub fn update_cell(
        &mut self,
        table: &TableRef,
        pk: &super::PkValues,
        column: &str,
        value: Option<&str>,
    ) -> Result<u64, DbError> {
        let (sql, params) = update_cell_sql(table, pk, column, value)?;
        self.conn
            .exec_drop(sql.as_str(), params)
            .map_err(DbError::from)?;
        Ok(self.conn.affected_rows())
    }

    /// **单事务**批量改格（暂存式编辑提交走这里）：多格改动要么全成、要么全回滚。
    pub fn update_cells(
        &mut self,
        table: &TableRef,
        edits: &[super::CellEdit],
    ) -> Result<u64, DbError> {
        let mut tx = self
            .conn
            .start_transaction(mysql::TxOpts::default())
            .map_err(DbError::from)?;
        let mut affected = 0u64;
        for edit in edits {
            let (sql, params) =
                update_cell_sql(table, &edit.pk, &edit.column, edit.value.as_deref())?;
            tx.exec_drop(sql.as_str(), params).map_err(DbError::from)?;
            affected += tx.affected_rows();
        }
        tx.commit().map_err(DbError::from)?;
        Ok(affected)
    }

    /// 按主键删行（参数化）。
    pub fn delete_rows(&mut self, table: &TableRef, pk: &super::PkValues) -> Result<u64, DbError> {
        let (sql, params) = delete_sql(table, pk)?;
        self.conn
            .exec_drop(sql.as_str(), params)
            .map_err(DbError::from)?;
        Ok(self.conn.affected_rows())
    }

    /// **单事务**批量删行（结果网格多选删除走这里）：要么全删、要么一行都不动。
    pub fn delete_row_set(
        &mut self,
        table: &TableRef,
        keys: &[super::PkValues],
    ) -> Result<u64, DbError> {
        let mut tx = self
            .conn
            .start_transaction(mysql::TxOpts::default())
            .map_err(DbError::from)?;
        let mut affected = 0u64;
        for pk in keys {
            let (sql, params) = delete_sql(table, pk)?;
            tx.exec_drop(sql.as_str(), params).map_err(DbError::from)?;
            affected += tx.affected_rows();
        }
        tx.commit().map_err(DbError::from)?;
        Ok(affected)
    }

    /// **单事务**提交一次暂存式编辑（改格 + 新增行混排）。
    pub fn apply_batch(
        &mut self,
        table: &TableRef,
        edits: &[super::CellEdit],
        inserts: &[super::InsertRow],
    ) -> Result<super::BatchOutcome, DbError> {
        let mut tx = self
            .conn
            .start_transaction(mysql::TxOpts::default())
            .map_err(DbError::from)?;
        let mut updated = 0u64;
        for edit in edits {
            let (sql, params) =
                update_cell_sql(table, &edit.pk, &edit.column, edit.value.as_deref())?;
            tx.exec_drop(sql.as_str(), params).map_err(DbError::from)?;
            updated += tx.affected_rows();
        }
        let mut inserted = 0u64;
        for row in inserts {
            let sql = insert_sql(table, &row.columns)?;
            tx.exec_drop(sql.as_str(), values_of(&row.values))
                .map_err(DbError::from)?;
            inserted += tx.affected_rows();
        }
        tx.commit().map_err(DbError::from)?;
        Ok(super::BatchOutcome { updated, inserted })
    }

    /// 一张表上的索引：`information_schema.STATISTICS` 按 `INDEX_NAME` 分组，
    /// 列是 `COLUMN_NAME`（同一索引里 `SEQ_IN_INDEX` 升序即列序）。
    pub fn table_indexes(&mut self, table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        let schema = table.schema.as_deref();
        let result = self.conn.exec_iter(
            "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ? \
             ORDER BY INDEX_NAME, SEQ_IN_INDEX",
            (schema, table.name.as_str()),
        )?;
        let mut indexes: Vec<TableIndex> = Vec::new();
        for row in result {
            let row = row.map_err(DbError::from)?;
            let name = match row.as_ref(0) {
                Some(mysql::Value::Bytes(bytes)) => {
                    String::from_utf8_lossy(bytes.as_slice()).into_owned()
                }
                _ => continue,
            };
            // `NON_UNIQUE`：0 = 唯一。
            let unique = match row.as_ref(1) {
                Some(mysql::Value::Int(value)) => *value == 0,
                Some(mysql::Value::UInt(value)) => *value == 0,
                Some(mysql::Value::Bytes(bytes)) => bytes.as_slice() != b"0",
                _ => false,
            };
            let column = match row.as_ref(2) {
                Some(mysql::Value::Bytes(bytes)) => {
                    String::from_utf8_lossy(bytes.as_slice()).into_owned()
                }
                _ => String::new(),
            };
            match indexes.last_mut() {
                // 已按 `INDEX_NAME` 排序，同一索引的列必定连续。
                Some(index) if index.name == name => index.columns.push(column),
                _ => indexes.push(TableIndex {
                    primary: name == "PRIMARY",
                    name,
                    columns: vec![column],
                    unique,
                }),
            }
        }
        Ok(indexes)
    }

    /// 行数：读 `information_schema.TABLES.TABLE_ROWS`（**估算值**，InnoDB 的统计）。
    ///
    /// 视图该列是 NULL，直接落 `Estimate(0)`；调用方按 `0` 展示「未知」。
    pub fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        let schema = table.schema.as_deref();
        let mut result = self.conn.exec_iter(
            "SELECT TABLE_ROWS FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ?",
            (schema, table.name.as_str()),
        )?;
        // 这条查询最多一行（表名唯一），所以取第一行即可，不从迭代器里循环。
        if let Some(row) = result.next() {
            let row = row.map_err(DbError::from)?;
            let estimate = match row.as_ref(0) {
                Some(mysql::Value::Int(value)) => u64::try_from(*value).unwrap_or_default(),
                Some(mysql::Value::UInt(value)) => *value,
                Some(mysql::Value::Bytes(bytes)) => {
                    String::from_utf8_lossy(bytes).parse().unwrap_or_default()
                }
                _ => 0,
            };
            return Ok(RowCount::Estimate(estimate));
        }
        Err(DbError::Query(format!("table not found: {table}")))
    }

    /// `information_schema` 的文本列取值（NULL / 非文本统一落空串）。
    fn text_at(row: &mysql::Row, index: usize) -> String {
        match row.as_ref(index) {
            Some(mysql::Value::Bytes(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
            _ => String::new(),
        }
    }

    /// 一张表上的外键：`KEY_COLUMN_USAGE`（带被引用表 / 列）联
    /// `REFERENTIAL_CONSTRAINTS`（更新 / 删除动作），按约束名分组。
    pub fn table_foreign_keys(
        &mut self,
        table: &TableRef,
    ) -> Result<Vec<TableForeignKey>, DbError> {
        let schema = table.schema.as_deref();
        let result = self.conn.exec_iter(
            "SELECT kcu.CONSTRAINT_NAME, kcu.COLUMN_NAME, kcu.REFERENCED_TABLE_NAME, \
                    kcu.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
             FROM information_schema.KEY_COLUMN_USAGE kcu \
             JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
               ON rc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA \
              AND rc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME \
             WHERE kcu.TABLE_SCHEMA = COALESCE(?, DATABASE()) AND kcu.TABLE_NAME = ? \
               AND kcu.REFERENCED_TABLE_NAME IS NOT NULL \
             ORDER BY kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
            (schema, table.name.as_str()),
        )?;
        let mut rows: Vec<mysql::Row> = Vec::new();
        for row in result {
            rows.push(row.map_err(DbError::from)?);
        }
        // 已按 `CONSTRAINT_NAME` 排序，同一约束的列必定连续。
        let mut keys: Vec<TableForeignKey> = Vec::new();
        for row in &rows {
            let name = Self::text_at(row, 0);
            let column = Self::text_at(row, 1);
            let ref_table = Self::text_at(row, 2);
            let ref_column = Self::text_at(row, 3);
            let on_update = Self::text_at(row, 4);
            let on_delete = Self::text_at(row, 5);
            match keys.last_mut() {
                Some(fk) if fk.name == name => {
                    fk.columns.push(column);
                    fk.ref_columns.push(ref_column);
                }
                _ => keys.push(TableForeignKey {
                    name,
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

    /// 一张表上的触发器：`information_schema.TRIGGERS` 的
    /// `ACTION_TIMING` / `EVENT_MANIPULATION` 是现成的两列（SQLite 才要解析）。
    pub fn table_triggers(&mut self, table: &TableRef) -> Result<Vec<TableTrigger>, DbError> {
        let schema = table.schema.as_deref();
        let result = self.conn.exec_iter(
            "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION \
             FROM information_schema.TRIGGERS \
             WHERE EVENT_OBJECT_SCHEMA = COALESCE(?, DATABASE()) AND EVENT_OBJECT_TABLE = ? \
             ORDER BY TRIGGER_NAME",
            (schema, table.name.as_str()),
        )?;
        let mut triggers: Vec<TableTrigger> = Vec::new();
        for row in result {
            let row = row.map_err(DbError::from)?;
            triggers.push(TableTrigger {
                name: Self::text_at(&row, 0),
                timing: Self::text_at(&row, 1),
                event: Self::text_at(&row, 2),
            });
        }
        Ok(triggers)
    }

    /// 一张表的建表 DDL（`SHOW CREATE TABLE` 原文；限定名逐段加反引号）。
    pub fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        let quoted = quote_qualified(table, '`')?;
        let result = self
            .conn
            .query_iter(format!("SHOW CREATE TABLE {quoted}"))
            .map_err(DbError::from)?;
        for row in result {
            let row = row.map_err(DbError::from)?;
            if let Some(mysql::Value::Bytes(bytes)) = row.as_ref(1) {
                return Ok(String::from_utf8_lossy(bytes).into_owned());
            }
        }
        Err(DbError::Query(format!("table not found: {table}")))
    }

    /// 批量参数化插入：每行一条 INSERT，**单事务**提交；`None` 写 `NULL`。
    pub fn insert_rows(
        &mut self,
        table: &TableRef,
        columns: &[String],
        rows: &[Vec<Option<String>>],
    ) -> Result<u64, DbError> {
        let sql = insert_sql(table, columns)?;
        let mut tx = self
            .conn
            .start_transaction(mysql::TxOpts::default())
            .map_err(DbError::from)?;
        let mut inserted = 0u64;
        for row in rows {
            tx.exec_drop(sql.as_str(), values_of(row))
                .map_err(DbError::from)?;
            inserted += tx.affected_rows();
        }
        tx.commit().map_err(DbError::from)?;
        Ok(inserted)
    }
}

/// 拼 INSERT 语句（`?` 占位，列名逐个加反引号）。
fn insert_sql(table: &TableRef, columns: &[String]) -> Result<String, DbError> {
    let quoted = quote_qualified(table, '`')?;
    let quoted_cols: Vec<String> = columns
        .iter()
        .map(|column| quote_identifier(column, '`'))
        .collect::<Result<_, _>>()?;
    let placeholders = vec!["?"; columns.len()].join(", ");
    Ok(format!(
        "INSERT INTO {quoted} ({}) VALUES ({placeholders})",
        quoted_cols.join(", ")
    ))
}

/// 把一行的文本值转成 mysql 参数（`None` → `Value::NULL`）。
fn values_of(values: &[Option<String>]) -> Vec<mysql::Value> {
    values
        .iter()
        .map(|value| match value {
            Some(text) => mysql::Value::from(text.as_str()),
            None => mysql::Value::NULL,
        })
        .collect()
}

/// 拼删行 SQL 与参数（主键值按 `CAST(x AS CHAR)` 文本比较，全参数化）。
fn delete_sql(
    table: &TableRef,
    pk: &super::PkValues,
) -> Result<(String, Vec<mysql::Value>), DbError> {
    let quoted = quote_qualified(table, '`')?;
    let mut conditions = Vec::with_capacity(pk.len());
    let mut params: Vec<mysql::Value> = Vec::with_capacity(pk.len());
    for (name, original) in pk {
        let quoted_pk = quote_identifier(name, '`')?;
        conditions.push(format!("CAST({quoted_pk} AS CHAR) = ?"));
        params.push(mysql::Value::from(original.as_str()));
    }
    let sql = format!("DELETE FROM {quoted} WHERE {}", conditions.join(" AND "));
    Ok((sql, params))
}

/// 拼改格 SQL 与参数（`?` 占位：第一个是新值，其后依次是主键原值）。
///
/// `value` 为 `None` 时绑 `mysql::Value::NULL` —— MySQL 协议里 NULL 是**带类型标记的
/// 值**（不是「无类型字面量」），所以不需要 PostgreSQL 那套 `::text::<类型>` 的迂回。
fn update_cell_sql(
    table: &TableRef,
    pk: &super::PkValues,
    column: &str,
    value: Option<&str>,
) -> Result<(String, Vec<mysql::Value>), DbError> {
    let quoted = quote_qualified(table, '`')?;
    let quoted_column = quote_identifier(column, '`')?;
    let mut conditions = Vec::with_capacity(pk.len());
    let mut params: Vec<mysql::Value> = vec![match value {
        Some(text) => mysql::Value::from(text),
        None => mysql::Value::NULL,
    }];
    for (name, original) in pk {
        let quoted_pk = quote_identifier(name, '`')?;
        conditions.push(format!("CAST({quoted_pk} AS CHAR) = ?"));
        params.push(mysql::Value::from(original.as_str()));
    }
    let sql = format!(
        "UPDATE {quoted} SET {quoted_column} = ? WHERE {}",
        conditions.join(" AND ")
    );
    Ok((sql, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 库名可留空：`db_name` 收的是 `None`（连上后 `SELECT DATABASE()` 回 NULL），
    /// 而不是空串——「没给库」与「给了个空库名」在语义上不是一回事。
    #[test]
    fn blank_database_leaves_the_session_without_a_schema() {
        let cfg = ConnectionConfig {
            id: "conn-1".into(),
            kind: crate::config::DatabaseKind::Mysql,
            host: "db.local".into(),
            user: "root".into(),
            database: String::new(),
            ..ConnectionConfig::default()
        };
        assert_eq!(build_opts(&cfg, None).expect("拼选项").get_db_name(), None);

        let named = ConnectionConfig {
            database: "shop".into(),
            ..cfg
        };
        assert_eq!(
            build_opts(&named, None).expect("拼选项").get_db_name(),
            Some("shop")
        );
    }
}
