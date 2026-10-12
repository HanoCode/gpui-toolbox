//! MySQL / MariaDB 驱动（阻塞版 `mysql`，rustls TLS）。
//!
//! `caching_sha2_password` 的完整握手需要 TLS 通道：TLS 档位为 `Disable` 且服务器
//! 只开了默认认证插件时，首次连接可能被服务器拒绝——换 `Trust` 档或把账号改成
//! `mysql_native_password` 都能绕开。

use mysql::prelude::Queryable as _;

use crate::cancel::CancelHandle;
use crate::config::{ConnectionConfig, TlsMode};
use crate::drivers::{
    CatalogTable, OrderBy, RowCount, TableIndex, TableKind, TableRef, order_by_clause,
    quote_identifier, quote_qualified,
};
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value};
use crate::sqlintel::{StmtKind, prove_read_only, split_statements};

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
                outcome.result = Some(self.query_one(&stmt, max_rows)?);
            } else {
                let result = self.conn.query_iter(&stmt)?;
                outcome.affected += result.affected_rows();
            }
            outcome.statements_run += 1;
        }
        Ok(outcome)
    }

    /// 跑一条读语句并取结果集（`max_rows + 1` 探测截断）。
    fn query_one(&mut self, stmt: &str, max_rows: usize) -> Result<ResultSet, DbError> {
        let result = self.conn.query_iter(stmt)?;
        let columns: Vec<Column> = result
            .columns()
            .as_ref()
            .iter()
            .map(|col| Column {
                name: col.name_str().into_owned(),
                type_name: column_type_name(col.column_type()).to_owned(),
            })
            .collect();
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

        let routines = self.conn.exec_iter(
            "SELECT routine_name FROM information_schema.routines \
             WHERE routine_schema = ? ORDER BY lower(routine_name)",
            (&schema,),
        )?;
        for row in routines {
            let row = row.map_err(DbError::from)?;
            if let Some(mysql::Value::Bytes(bytes)) = row.as_ref(0) {
                tables.push(CatalogTable {
                    name: String::from_utf8_lossy(bytes.as_slice()).into_owned(),
                    kind: TableKind::Routine,
                    schema: Some(schema.clone()),
                });
            }
        }
        Ok(tables)
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
    ) -> Result<ResultSet, DbError> {
        let quoted = quote_qualified(table, '`')?;
        let order = order_by_clause(order_by, '`')?;
        let sql = format!("SELECT * FROM {quoted}{order} LIMIT {limit} OFFSET {offset}");
        self.query_one(&sql, limit)
    }

    /// 主键列名（指定库的主键约束列，按声明序）。
    pub fn table_primary_key(&mut self, table: &TableRef) -> Result<Vec<String>, DbError> {
        let schema = table.schema.as_deref();
        let result = self
            .conn
            .exec_iter(
                "SELECT kcu.COLUMN_NAME FROM information_schema.TABLE_CONSTRAINTS tc \
                 JOIN information_schema.KEY_COLUMN_USAGE kcu \
                   ON tc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME \
                  AND tc.TABLE_SCHEMA = kcu.TABLE_SCHEMA \
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
