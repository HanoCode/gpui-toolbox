//! PostgreSQL 驱动（阻塞版 `postgres`，内部即 tokio-postgres 的同步封装）。
//!
//! M1 用 `simple_query` 文本协议取行：所有值原样按文本回传，**不经类型 OID 映射**——
//! 类型映射的工程量留给列编辑，浏览 + 查询先把「值本身」跑稳。
//!
//! # 写路径为什么每处都带 `::text::<列类型>`
//!
//! 读路径走 `simple_query`（文本协议），写路径只能走**扩展协议**（参数化）。两者之间
//! 有个坑：`INSERT INTO t (id) VALUES ($1)`（`id` 是 `integer`）会让服务端把 `$1`
//! 推断成 `integer`，而 rust-postgres 的 `&str` / `Option<String>` 只接受 TEXT 族
//! （`to_sql_checked` 里 `accepts(ty)` 不通过就报 `WrongType`）——**文本值写不进
//! 非文本列**，`NULL` 更没法表达。
//!
//! 解法是先 `::text` 再显式转目标类型：`$1::text::integer` 让服务端把 `$1` 定成
//! TEXT（参数照旧传字符串），再由 PostgreSQL 自己走目标类型的输入函数转换——
//! 与 `psql` 里手敲字面量的行为一致。列类型由 [`PostgresConnection::column_sql_types`]
//! 从 `format_type()` 取，取不到就退回不带转换的裸参数（宁可报类型错，不可静默写错）。

use std::collections::HashMap;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::cancel::CancelHandle;
use crate::config::{AuthMode, ConnectionConfig, TlsMode};
use crate::drivers::{
    CatalogTable, DatabaseObject, FilterOp, InstanceFact, InstanceOverview, InstanceTable,
    InstanceView, OrderBy, PkValues, PlaceholderStyle, RowCount, TableColumnMeta, TableFilter,
    TableForeignKey, TableIndex, TableKind, TableRef, TableTrigger, filter_conditions,
    human_duration_secs, order_by_clause, quote_identifier, quote_qualified,
};
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value};
use crate::sqlintel::{StmtKind, prove_read_only, split_statements};

/// 实例概览的一项（名字 = 引擎口径原文，值 = 原样字符串）。
fn fact(name: &str, value: String) -> InstanceFact {
    InstanceFact {
        name: name.to_owned(),
        value,
    }
}

/// 一条 PostgreSQL 连接。
pub struct PostgresConnection {
    client: postgres::Client,
}

/// 由连接档案拼连接串（含 5 秒建连超时）。
fn connection_string(cfg: &ConnectionConfig, password: Option<&str>) -> String {
    let port = cfg.port.unwrap_or_else(|| {
        cfg.kind
            .default_port()
            .expect("PostgreSQL has a default port")
    });
    let mut s = format!(
        "host={} port={port} user={}",
        cfg.host.trim(),
        cfg.user.trim(),
    );
    // 库名可留空（见 `ConnectionConfig::validate`）。⚠️ **不能传空串**：那会真的往
    // startup 包里塞一个空库名，服务端回 `database "" does not exist`；省略 `dbname`
    // 时 PostgreSQL 才按 libpq 规则回落到**用户名同名的库**。
    let database = cfg.database.trim();
    if !database.is_empty() {
        s.push_str(" dbname=");
        s.push_str(database);
    }
    if let Some(password) = password {
        s.push_str(" password=");
        s.push_str(password);
    }
    s.push_str(" connect_timeout=5");
    s
}

/// 「接受任意证书」的校验器（`TlsMode::DangerAcceptInvalid` 专用，自签测试库）。
///
/// 握手签名本身仍按 rustls 的默认算法真验（否则连自签库都会因算法判定失败而拒），
/// 只跳过**证书链**这一环——这正是那一档的语义：加密要，证书身份不要。
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// 按 TLS 档位造 rustls 配置（与 MySQL 侧同一取向：rustls + ring，不碰系统 OpenSSL）。
///
/// 信任链默认用 `webpki-roots`（编译进二进制）而不是系统钥匙串：终端用户零系统依赖。
/// 档案里给了 **CA 证书**（PEM 文件）时改用它做根 —— 自签 CA 的库不必再退到
/// 「接受任意证书」那一档。客户端证书 + 私钥（同为 PEM）用于**双向 TLS**；
/// 私钥需为未加密格式（rustls 不在这层解口令）。
fn tls_config(mode: TlsMode, cfg: &ConnectionConfig) -> Result<ClientConfig, DbError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|err| DbError::Connect(format!("rustls: {err}")))?;
    let mut config = match mode {
        TlsMode::DangerAcceptInvalid => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(provider)))
            .with_no_client_auth(),
        _ => {
            let mut roots = RootCertStore::empty();
            match cfg.ca_cert_path.as_deref().map(str::trim) {
                Some(ca) if !ca.is_empty() => {
                    let certs = CertificateDer::pem_file_iter(ca)
                        .map_err(|err| DbError::Connect(format!("read ca cert {ca}: {err}")))?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|err| DbError::Connect(format!("parse ca cert {ca}: {err}")))?;
                    for cert in certs {
                        roots
                            .add(cert)
                            .map_err(|err| DbError::Connect(format!("ca cert {ca}: {err}")))?;
                    }
                }
                _ => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            }
            let builder = builder.with_root_certificates(roots);
            match (
                cfg.client_cert_path.as_deref().map(str::trim),
                cfg.client_key_path.as_deref().map(str::trim),
            ) {
                (Some(cert), Some(key)) if !cert.is_empty() && !key.is_empty() => {
                    let certificate = CertificateDer::from_pem_file(cert).map_err(|err| {
                        DbError::Connect(format!("parse client cert {cert}: {err}"))
                    })?;
                    let private_key = PrivateKeyDer::from_pem_file(key).map_err(|err| {
                        DbError::Connect(format!("parse client key {key}: {err}"))
                    })?;
                    builder
                        .with_client_auth_cert(vec![certificate], private_key)
                        .map_err(|err| DbError::Connect(format!("client cert: {err}")))?
                }
                _ => builder.with_no_client_auth(),
            }
        }
    };
    // 部分服务端（云 PG 的连接池 / 代理）要求 ALPN 协商到 `postgresql`。
    postgres_rustls::set_postgresql_alpn(&mut config);
    Ok(config)
}

impl PostgresConnection {
    /// 建连。连接串模式整条直连；TLS 档位：`Disable` 走明文，其余两档走 rustls。
    pub(crate) fn connect(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Self, DbError> {
        let params = match cfg.auth_mode {
            AuthMode::ConnectionString => cfg
                .connection_string
                .as_deref()
                .map(str::trim)
                .filter(|raw| !raw.is_empty())
                .ok_or_else(|| DbError::Config("connection string is empty".into()))?
                .to_owned(),
            _ => connection_string(cfg, password),
        };
        let client = match cfg.tls {
            TlsMode::Disable => {
                postgres::Client::connect(&params, postgres::NoTls).map_err(DbError::from)?
            }
            mode => {
                let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config(mode, cfg)?));
                postgres::Client::connect(
                    &params,
                    postgres_rustls::MakeTlsConnector::new(connector),
                )
                .map_err(DbError::from)?
            }
        };
        tracing::info!(
            host = %cfg.host,
            database = %cfg.database,
            tls = cfg.tls.wire_name(),
            "postgres connected"
        );
        Ok(Self { client })
    }

    /// 活性探测。
    pub fn ping(&mut self) -> Result<(), DbError> {
        self.client
            .simple_query("SELECT 1")
            .map_err(DbError::from)?
            .into_iter()
            .for_each(|_| {});
        Ok(())
    }

    /// 带外取消句柄（服务器侧 `CancelRequest`；走独立短连接，不占本连接的锁）。
    ///
    /// PostgreSQL 的 `CancelRequest` 是明文协议报文，无论主连接是否加密都照发明文——
    /// 所以这里与 TLS 档位无关。
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::postgres(self.client.cancel_token())
    }

    /// 版本描述。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        let mut text = String::new();
        for message in self
            .client
            .simple_query("SELECT version()")
            .map_err(DbError::from)?
        {
            let postgres::SimpleQueryMessage::Row(row) = message else {
                continue;
            };
            if let Some(value) = row.get(0) {
                text.push_str(value);
            }
        }
        Ok(text)
    }

    /// 连接级只读：`default_transaction_read_only`（服务器自己拒绝写）。
    ///
    /// 用 `SET`（会话级）而非 `SET SESSION`：PG 两者等价，但 `SET` 在显式事务里也能生效。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        let value = if read_only { "on" } else { "off" };
        self.client
            .simple_query(&format!("SET default_transaction_read_only = {value}"))
            .map_err(DbError::from)?
            .into_iter()
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
            // SHOW / SET 等方言词解析器不保证认得，走关键字兜底。
            let read = prove_read_only(&stmt) || crate::sqlintel::classify(&stmt) == StmtKind::Read;
            if read {
                outcome.result = Some(self.query_one(&stmt, max_rows)?);
            } else {
                // `execute` 只接受单条语句：拆分器已保证，逐条跑顺便拿到影响行数。
                outcome.affected += self.client.execute(&stmt, &[]).map_err(DbError::from)?;
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
        let wrapped = super::wrap_paged_sql(&inner, offset, limit, '"')?;
        self.query_one(&wrapped, limit)
    }

    /// 跑一条读语句并取结果集（文本协议，`max_rows + 1` 探测截断）。
    fn query_one(&mut self, stmt: &str, max_rows: usize) -> Result<ResultSet, DbError> {
        let messages = self.client.simple_query(stmt).map_err(DbError::from)?;
        let mut columns: Vec<Column> = Vec::new();
        let mut rows_out = Vec::new();
        let mut truncated = false;
        for message in messages {
            let row = match message {
                postgres::SimpleQueryMessage::Row(row) => row,
                _ => continue,
            };
            if columns.is_empty() {
                columns = row
                    .columns()
                    .iter()
                    .map(|col| Column {
                        name: col.name().to_owned(),
                        type_name: String::new(),
                        comment: String::new(),
                    })
                    .collect();
            }
            if rows_out.len() >= max_rows {
                truncated = true;
                break;
            }
            let mut cells = Vec::with_capacity(columns.len());
            for index in 0..columns.len() {
                cells.push(match row.get(index) {
                    None => Value::Null,
                    Some(text) => Value::Text(text.to_owned()),
                });
            }
            rows_out.push(cells);
        }
        Ok(ResultSet {
            columns,
            rows: rows_out,
            truncated,
        })
    }

    /// 把「当前 schema」解析成一个具体名字（`None` 时问一次服务端）。
    ///
    /// 目录查询要多处用同一个 schema 名，逐条写 `COALESCE($n, current_schema())` 也行，
    /// 但目录的返回要带上 schema 标（左栏要显示「库 → schema → 表」），所以这里
    /// 统一解析一次，后面全用具体名字。
    fn resolve_schema(&mut self, schema: Option<&str>) -> Result<String, DbError> {
        if let Some(schema) = schema {
            return Ok(schema.to_owned());
        }
        let row = self
            .client
            .query_one("SELECT current_schema()", &[])
            .map_err(DbError::from)?;
        Ok(row.get::<_, Option<String>>(0).unwrap_or_default())
    }

    /// **全部**库（可连接、非模板的那些）。
    ///
    /// ⚠️ PostgreSQL **没有** `USE <db>`：一个会话终身绑在建连时那个 `dbname` 上，
    /// 切库只能重连。所以这里只负责**列出**；真要切，由上层拿这份清单重建连接
    ///（`Connection::switch_database` 对 PG 回 `NeedsReconnect`）。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        let rows = self
            .client
            .query(
                "SELECT datname FROM pg_database \
                 WHERE datallowconn AND NOT datistemplate \
                 ORDER BY datname",
                &[],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.get::<_, Option<String>>(0))
            .collect())
    }

    /// 可选 schema 清单：`pg_namespace` 剔除系统 schema 与 `information_schema`。
    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        let rows = self
            .client
            .query(
                "SELECT nspname FROM pg_namespace \
                 WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
                 ORDER BY nspname",
                &[],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .into_iter()
            .map(|row| row.get::<_, String>(0))
            .collect())
    }

    /// 一个 schema 里的表 / 视图 / 函数 / 序列。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        let schema = self.resolve_schema(schema)?;
        let mut tables = Vec::new();

        let relations = self
            .client
            .query(
                "SELECT table_name, table_type FROM information_schema.tables \
                 WHERE table_schema = $1 \
                   AND table_type IN ('BASE TABLE', 'VIEW') \
                 ORDER BY lower(table_name)",
                &[&schema],
            )
            .map_err(DbError::from)?;
        for row in relations {
            let name: String = row.get(0);
            let kind = if row.get::<_, String>(1) == "VIEW" {
                TableKind::View
            } else {
                TableKind::Table
            };
            tables.push(CatalogTable {
                name,
                kind,
                schema: Some(schema.clone()),
            });
        }

        // 存储过程 / 函数：`information_schema.routines` 的 `routine_type` 区分两者
        //（`PROCEDURE` / `FUNCTION`），左栏树按两棵分类节点展示。
        let routines = self
            .client
            .query(
                "SELECT routine_name, routine_type FROM information_schema.routines \
                 WHERE routine_schema = $1 \
                 ORDER BY lower(routine_name)",
                &[&schema],
            )
            .map_err(DbError::from)?;
        for row in routines {
            let name: String = row.get(0);
            let kind = if row.get::<_, String>(1) == "PROCEDURE" {
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

        let sequences = self
            .client
            .query(
                "SELECT sequence_name FROM information_schema.sequences \
                 WHERE sequence_schema = $1 \
                 ORDER BY lower(sequence_name)",
                &[&schema],
            )
            .map_err(DbError::from)?;
        for row in sequences {
            tables.push(CatalogTable {
                name: row.get::<_, String>(0),
                kind: TableKind::Sequence,
                schema: Some(schema.clone()),
            });
        }

        Ok(tables)
    }

    /// 一张表的列（参数化查询，表名不走文本拼接）。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        let schema = table.schema.as_deref();
        // 不走 information_schema.columns：`data_type` 不带长度修饰（`character varying`
        // 丢掉 `(50)`），而 `format_type` 是写路径同款口径（见 [`Self::column_sql_types`]），
        // 类型原文可直接写进 SQL。注释走 `col_description`（`COMMENT ON COLUMN` 写入的
        // 那一份；没写就是 NULL → 空串）。
        let rows = self
            .client
            .query(
                "SELECT a.attname, format_type(a.atttypid, a.atttypmod), \
                        col_description(c.oid, a.attnum) \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = $1 AND n.nspname = COALESCE($2, current_schema()) \
                   AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&table.name, &schema],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .into_iter()
            .map(|row| Column {
                name: row.get::<_, String>(0),
                type_name: row.get::<_, String>(1),
                comment: row.get::<_, Option<String>>(2).unwrap_or_default(),
            })
            .collect())
    }

    /// 一张表的列级元数据（`pg_attribute` / `pg_attrdef` / `pg_description` 口径，
    /// 与 [`Self::table_columns`] 同一套系统表；字符集没有——PG 按库统一）。
    pub fn table_column_meta(&mut self, table: &TableRef) -> Result<Vec<TableColumnMeta>, DbError> {
        let schema = table.schema.as_deref();
        let rows = self
            .client
            .query(
                "SELECT a.attname, format_type(a.atttypid, a.atttypmod), \
                        col_description(c.oid, a.attnum), NOT a.attnotnull, \
                        pg_get_expr(ad.adbin, ad.adrelid), attcollation::regcollation::text \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
                 WHERE c.relname = $1 AND n.nspname = COALESCE($2, current_schema()) \
                   AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&table.name, &schema],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .into_iter()
            .map(|row| TableColumnMeta {
                name: row.get::<_, String>(0),
                type_name: row.get::<_, String>(1),
                comment: row.get::<_, Option<String>>(2).unwrap_or_default(),
                nullable: Some(row.get::<_, bool>(3)),
                default: row.get::<_, Option<String>>(4),
                character_set: None,
                collation: row.get::<_, Option<String>>(5),
            })
            .collect())
    }

    /// 「列名（小写）→ 该列的 SQL 类型原文」，`format_type()` 口径。
    ///
    /// 写路径要用它把参数 `::text::<类型>`：取的是 `integer` / `character varying(20)` /
    /// `timestamp with time zone` 这种**可直接写进 SQL 的类型文本**。
    fn column_sql_types(&mut self, table: &TableRef) -> Result<HashMap<String, String>, DbError> {
        let schema = table.schema.as_deref();
        let rows = self
            .client
            .query(
                "SELECT a.attname, format_type(a.atttypid, a.atttypmod) \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = $1 AND n.nspname = COALESCE($2, current_schema()) \
                   AND a.attnum > 0 AND NOT a.attisdropped",
                &[&table.name, &schema],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get::<_, String>(0).to_lowercase(),
                    row.get::<_, String>(1),
                )
            })
            .collect())
    }

    /// 一张表的分页数据。
    ///
    /// `order_by` 是调用方给的稳定序（通常是主键）；PG 的 `LIMIT/OFFSET` 在**没有
    /// `ORDER BY` 时顺序完全由计划决定**（并行扫描 + 不同计划会给出不同顺序），
    /// 翻页必然出现重复行 / 漏行。
    pub(crate) fn table_page(
        &mut self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
        filters: &[TableFilter],
    ) -> Result<ResultSet, DbError> {
        let quoted = quote_qualified(table, '"')?;
        let order = order_by_clause(order_by, '"')?;
        if filters.is_empty() {
            let sql = format!("SELECT * FROM {quoted}{order} LIMIT {limit} OFFSET {offset}");
            return self.query_one(&sql, limit);
        }
        // 有过滤时**必须走扩展协议**（值要绑定成 `$n`），而 `simple_query` 的文本协议
        // 拿不到任意类型的行——所以这里把投影显式 `::text` 出来（`simple_query` 路径
        // 本来也是「一切都当文本」，两种口径一致），列名用 `AS` 保回来。
        let columns = self.table_columns(table)?;
        let types = self.column_sql_types(table).unwrap_or_default();
        let projection = columns
            .iter()
            .map(|column| {
                let quoted_column = quote_identifier(&column.name, '"')?;
                Ok(format!("{quoted_column}::text AS {quoted_column}"))
            })
            .collect::<Result<Vec<_>, DbError>>()?
            .join(", ");
        let (conditions, values) = filter_conditions(
            filters,
            |name, op| {
                let quoted_column = quote_identifier(name, '"')?;
                let known = types.contains_key(&name.to_lowercase());
                // 拿到列类型才能做有类型的比较（`col = $1::text::integer` 能走索引）；
                // `LIKE` 与「类型未知」两种情形退回文本比较。
                Ok(if op == FilterOp::Like || !known {
                    format!("CAST({quoted_column} AS TEXT)")
                } else {
                    quoted_column
                })
            },
            |index, filter| {
                // 占位符编号走中性的 `$n` 渲染（从 1 起），与其它驱动同一套编号规则。
                let placeholder = PlaceholderStyle::Dollar.render(index);
                let known = types.contains_key(&filter.column.to_lowercase());
                if filter.op == FilterOp::Like || !known {
                    format!("{placeholder}::text")
                } else {
                    format!(
                        "{placeholder}::text::{}",
                        types[&filter.column.to_lowercase()]
                    )
                }
            },
        )?;
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let sql = format!(
            "SELECT {projection} FROM {quoted}{where_clause}{order} LIMIT {limit} OFFSET {offset}"
        );
        let refs = as_sql_params(&values);
        let rows = self
            .client
            .query(sql.as_str(), &refs)
            .map_err(DbError::from)?;
        let mut rows_out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut cells = Vec::with_capacity(columns.len());
            for index in 0..columns.len() {
                cells.push(
                    match row
                        .try_get::<_, Option<String>>(index)
                        .map_err(DbError::from)?
                    {
                        Some(text) => Value::Text(text),
                        None => Value::Null,
                    },
                );
            }
            rows_out.push(cells);
        }
        Ok(ResultSet {
            columns,
            rows: rows_out,
            truncated: false,
        })
    }

    /// 主键列名（指定 schema 的主键约束列，按声明序）。
    ///
    /// JOIN 必须带上 `table_name`：约束名只保证**表内**唯一，同 schema 跨表
    /// 同名约束（如手工命名的 `pk`）会把别表的主键列混进来。
    pub fn table_primary_key(&mut self, table: &TableRef) -> Result<Vec<String>, DbError> {
        let schema = table.schema.as_deref();
        let rows = self
            .client
            .query(
                "SELECT kcu.column_name FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON tc.constraint_name = kcu.constraint_name \
                  AND tc.table_schema = kcu.table_schema \
                  AND tc.table_name = kcu.table_name \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                   AND tc.table_schema = COALESCE($2, current_schema()) AND tc.table_name = $1 \
                 ORDER BY kcu.ordinal_position",
                &[&table.name, &schema],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .into_iter()
            .map(|row| row.get::<_, String>(0))
            .collect())
    }

    /// 改一格：主键值按 `CAST(x AS TEXT)` 文本比较（参数化，无文本拼接注入面）。
    /// `value` 为 `None` 时写 `NULL`。
    pub fn update_cell(
        &mut self,
        table: &TableRef,
        pk: &PkValues,
        column: &str,
        value: Option<&str>,
    ) -> Result<u64, DbError> {
        let types = self.column_sql_types(table).unwrap_or_default();
        let sql = update_cell_sql(
            table,
            pk,
            column,
            value.is_none(),
            types.get(&column.to_lowercase()).map(String::as_str),
        )?;
        let owned = update_cell_params(pk, value);
        let refs = as_sql_params(&owned);
        self.client
            .execute(sql.as_str(), &refs)
            .map_err(DbError::from)
    }

    /// **单事务**批量改格（暂存式编辑提交走这里）：多格改动要么全成、要么全回滚。
    pub fn update_cells(
        &mut self,
        table: &TableRef,
        edits: &[super::CellEdit],
    ) -> Result<u64, DbError> {
        let types = self.column_sql_types(table).unwrap_or_default();
        let mut tx = self.client.transaction().map_err(DbError::from)?;
        let mut affected = 0u64;
        for edit in edits {
            let column_type = types.get(&edit.column.to_lowercase());
            let sql = update_cell_sql(
                table,
                &edit.pk,
                &edit.column,
                edit.value.is_none(),
                column_type.map(String::as_str),
            )?;
            let owned = update_cell_params(&edit.pk, edit.value.as_deref());
            let refs = as_sql_params(&owned);
            affected += tx.execute(sql.as_str(), &refs).map_err(DbError::from)?;
        }
        tx.commit().map_err(DbError::from)?;
        Ok(affected)
    }

    /// 按主键删行（参数化）。
    pub fn delete_rows(&mut self, table: &TableRef, pk: &PkValues) -> Result<u64, DbError> {
        delete_row_with(table, pk, |sql, params| self.client.execute(sql, params))
    }

    /// **单事务**批量删行（结果网格多选删除走这里）：要么全删、要么一行都不动。
    pub fn delete_row_set(&mut self, table: &TableRef, keys: &[PkValues]) -> Result<u64, DbError> {
        let mut tx = self.client.transaction().map_err(DbError::from)?;
        let mut affected = 0u64;
        for pk in keys {
            affected += delete_row_with(table, pk, |sql, params| tx.execute(sql, params))?;
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
        let types = self.column_sql_types(table).unwrap_or_default();
        let mut tx = self.client.transaction().map_err(DbError::from)?;
        let mut updated = 0u64;
        for edit in edits {
            let column_type = types.get(&edit.column.to_lowercase());
            let sql = update_cell_sql(
                table,
                &edit.pk,
                &edit.column,
                edit.value.is_none(),
                column_type.map(String::as_str),
            )?;
            let owned = update_cell_params(&edit.pk, edit.value.as_deref());
            let refs = as_sql_params(&owned);
            updated += tx.execute(sql.as_str(), &refs).map_err(DbError::from)?;
        }
        let mut inserted = 0u64;
        for row in inserts {
            let sql = insert_sql(table, &types, &row.columns)?;
            let refs: Vec<&(dyn postgres::types::ToSql + Sync)> = row
                .values
                .iter()
                .map(|value| value as &(dyn postgres::types::ToSql + Sync))
                .collect();
            inserted += tx.execute(sql.as_str(), &refs).map_err(DbError::from)?;
        }
        tx.commit().map_err(DbError::from)?;
        Ok(super::BatchOutcome { updated, inserted })
    }

    /// 一张表的近似建表 DDL：PG 没有内置的 DDL 反查，这里由
    /// `information_schema` 重构（列 / 类型 / 可空 / 缺省 + 主键），不含索引与约束细节。
    pub fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        let schema = table.schema.as_deref();
        let rows = self.client.query(
            "SELECT column_name, data_type, is_nullable, column_default \
                 FROM information_schema.columns \
                 WHERE table_name = $1 AND table_schema = COALESCE($2, current_schema()) \
                 ORDER BY ordinal_position",
            &[&table.name, &schema],
        )?;
        if rows.is_empty() {
            return Err(DbError::Query(format!("table not found: {table}")));
        }
        let mut lines = Vec::with_capacity(rows.len() + 1);
        for row in &rows {
            let name: String = row.get(0);
            let data_type: String = row.get(1);
            let nullable: String = row.get(2);
            let default: Option<String> = row.get(3);
            let quoted_column = quote_identifier(&name, '"')?;
            let mut line = format!("  {quoted_column} {data_type}");
            if nullable == "NO" {
                line.push_str(" NOT NULL");
            }
            if let Some(default) = default {
                line.push_str(&format!(" DEFAULT {default}"));
            }
            lines.push(line);
        }
        let pk = self.table_primary_key(table)?;
        if !pk.is_empty() {
            let columns = pk
                .iter()
                .map(|column| quote_identifier(column, '"'))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ");
            lines.push(format!("  PRIMARY KEY ({columns})"));
        }
        let quoted = quote_qualified(table, '"')?;
        Ok(format!(
            "CREATE TABLE {quoted} (\n{}\n);",
            lines.join(",\n")
        ))
    }

    /// 一张表上的索引：`pg_index` 联 `pg_attribute` 取列序（`unnest(...) WITH ORDINALITY`
    /// 保住 `indkey` 里的顺序）。
    ///
    /// 表达式索引的 `attnum` 为 0，`LEFT JOIN` 后列名是 NULL —— 这里落成空串，
    /// 展示层会显示成 `(expression)` 而不是假装有个名字。
    pub fn table_indexes(&mut self, table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        let schema = table.schema.as_deref();
        let rows = self
            .client
            .query(
                "SELECT i.relname, ix.indisunique, ix.indisprimary, a.attname \
                 FROM pg_index ix \
                 JOIN pg_class i ON i.oid = ix.indexrelid \
                 JOIN pg_class t ON t.oid = ix.indrelid \
                 JOIN pg_namespace n ON n.oid = t.relnamespace \
                 JOIN unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) ON true \
                 LEFT JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
                 WHERE t.relname = $1 AND n.nspname = COALESCE($2, current_schema()) \
                 ORDER BY i.relname, k.ord",
                &[&table.name, &schema],
            )
            .map_err(DbError::from)?;
        let mut indexes: Vec<TableIndex> = Vec::new();
        for row in &rows {
            let name: String = row.get(0);
            let unique: bool = row.get(1);
            let primary: bool = row.get(2);
            let column: Option<String> = row.get(3);
            match indexes.last_mut() {
                // 同一索引的列是连续行（已按 `i.relname` 排序），就地追加。
                Some(index) if index.name == name => {
                    index.columns.push(column.unwrap_or_default());
                }
                _ => indexes.push(TableIndex {
                    name,
                    columns: vec![column.unwrap_or_default()],
                    unique,
                    primary,
                }),
            }
        }
        Ok(indexes)
    }

    /// 行数：读 `pg_class.reltuples` 统计（**估算值**，`ANALYZE` 之后才准）。
    ///
    /// 从未统计过的表 `reltuples` 是 `-1`，此时返回 `Ok(Estimate(0))` 不合适 ——
    /// 由调用方按 `0` 展示「未知」。
    pub fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        let schema = table.schema.as_deref();
        let row = self.client.query_opt(
            "SELECT c.reltuples::bigint FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = $1 AND n.nspname = COALESCE($2, current_schema())",
            &[&table.name, &schema],
        )?;
        let Some(row) = row else {
            return Err(DbError::Query(format!("table not found: {table}")));
        };
        let estimate: i64 = row.get(0);
        Ok(RowCount::Estimate(
            u64::try_from(estimate).unwrap_or_default(),
        ))
    }

    /// 存储过程 / 函数的定义原文（`pg_get_functiondef`）。
    ///
    /// PG 允许同名重载，所以可能返回多份：按 `oid` 排序后用空行拼成一份，
    /// **顺序稳定**才能拿来做文本对比。取不到返回 `Ok(None)`。
    pub fn routine_definition(
        &mut self,
        kind: TableKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<String>, DbError> {
        let prokind = match kind {
            TableKind::Procedure => "p",
            TableKind::Function => "f",
            _ => return Ok(None),
        };
        let rows = self.client.query(
            "SELECT pg_get_functiondef(p.oid) \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.proname = $1 AND n.nspname = COALESCE($2, current_schema()) \
               AND p.prokind::text = $3 \
             ORDER BY p.oid",
            &[&name, &namespace, &prokind],
        )?;
        if rows.is_empty() {
            return Ok(None);
        }
        let definitions = rows
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>();
        Ok(Some(definitions.join("\n\n")))
    }

    /// 一张表上的外键：`pg_constraint`（`contype = 'f'`）+ 两侧
    /// `unnest(...) WITH ORDINALITY` 对齐列序（复合外键的 `conkey` / `confkey`
    /// 等长、按序号一一对应）；动作码经 [`pg_fk_action`] 翻成标准文案。
    pub fn table_foreign_keys(
        &mut self,
        table: &TableRef,
    ) -> Result<Vec<TableForeignKey>, DbError> {
        let schema = table.schema.as_deref();
        let rows = self
            .client
            .query(
                "SELECT con.conname, a.attname, ref.relname, ra.attname, con.confupdtype, con.confdeltype \
                 FROM pg_constraint con \
                 JOIN pg_class t ON t.oid = con.conrelid \
                 JOIN pg_namespace n ON n.oid = t.relnamespace \
                 JOIN pg_class ref ON ref.oid = con.confrelid \
                 JOIN unnest(con.conkey) WITH ORDINALITY AS ck(attnum, ord) ON true \
                 JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = ck.attnum \
                 JOIN unnest(con.confkey) WITH ORDINALITY AS rf(attnum, ord) ON rf.ord = ck.ord \
                 JOIN pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = rf.attnum \
                 WHERE con.contype = 'f' AND t.relname = $1 \
                   AND n.nspname = COALESCE($2, current_schema()) \
                 ORDER BY con.conname, ck.ord",
                &[&table.name, &schema],
            )
            .map_err(DbError::from)?;
        // 已按 `conname` 排序，同一约束的列必定连续。
        let mut keys: Vec<TableForeignKey> = Vec::new();
        for row in &rows {
            let name: String = row.get(0);
            let column: String = row.get(1);
            let ref_table: String = row.get(2);
            let ref_column: String = row.get(3);
            let on_update: i8 = row.get(4);
            let on_delete: i8 = row.get(5);
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
                    on_update: pg_fk_action(on_update).to_owned(),
                    on_delete: pg_fk_action(on_delete).to_owned(),
                }),
            }
        }
        Ok(keys)
    }

    /// 一张表上的触发器：`pg_trigger` 剔系统内建（`tgisinternal`），时机与事件
    /// 由 `tgtype` 位图经 [`pg_trigger_type`] 解出。
    pub fn table_triggers(&mut self, table: &TableRef) -> Result<Vec<TableTrigger>, DbError> {
        let schema = table.schema.as_deref();
        let rows = self
            .client
            .query(
                "SELECT t.tgname, t.tgtype FROM pg_trigger t \
                 JOIN pg_class c ON c.oid = t.tgrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE NOT t.tgisinternal AND c.relname = $1 \
                   AND n.nspname = COALESCE($2, current_schema()) \
                 ORDER BY t.tgname",
                &[&table.name, &schema],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let tgtype: i16 = row.get(1);
                let (timing, event) = pg_trigger_type(tgtype);
                TableTrigger {
                    name,
                    timing,
                    event,
                }
            })
            .collect())
    }

    /// 一个 schema 里的触发器清单：`pg_trigger` 剔内部触发器（`tgisinternal`，
    /// 那是约束 / 分区自带的车间件，不是用户建的），说明列 = 挂在哪张表上。
    pub fn list_triggers(&mut self, schema: Option<&str>) -> Result<Vec<DatabaseObject>, DbError> {
        let rows = self
            .client
            .query(
                "SELECT t.tgname, c.relname FROM pg_trigger t \
                 JOIN pg_class c ON c.oid = t.tgrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE NOT t.tgisinternal \
                   AND n.nspname = COALESCE($1, current_schema()) \
                 ORDER BY t.tgname",
                &[&schema],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .iter()
            .map(|row| DatabaseObject {
                name: row.get(0),
                detail: Some(row.get(1)),
            })
            .collect())
    }

    /// 一个命名空间里的**索引**清单（名字 + 建在哪张表上）。
    ///
    /// 与 [`Self::table_indexes`] 同一套目录表，只把「一张表」换成「一个 schema」：
    /// 主键 / 唯一约束隐式建的索引也在里面（它们是真实存在的索引对象）。
    pub fn list_indexes(&mut self, schema: Option<&str>) -> Result<Vec<DatabaseObject>, DbError> {
        let rows = self
            .client
            .query(
                "SELECT i.relname, t.relname FROM pg_index ix \
                 JOIN pg_class i ON i.oid = ix.indexrelid \
                 JOIN pg_class t ON t.oid = ix.indrelid \
                 JOIN pg_namespace n ON n.oid = t.relnamespace \
                 WHERE n.nspname = COALESCE($1, current_schema()) \
                 ORDER BY lower(t.relname), lower(i.relname)",
                &[&schema],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .iter()
            .map(|row| DatabaseObject {
                name: row.get(0),
                detail: Some(row.get(1)),
            })
            .collect())
    }

    /// 用户与角色清单：`pg_roles`（比 `pg_user` 多涵角色），说明列 = 登录属性。
    pub fn list_users(&mut self) -> Result<Vec<DatabaseObject>, DbError> {
        let rows = self
            .client
            .query(
                "SELECT rolname, rolsuper, rolcanlogin FROM pg_roles ORDER BY rolname",
                &[],
            )
            .map_err(DbError::from)?;
        Ok(rows
            .iter()
            .map(|row| {
                let superuser: bool = row.get(1);
                let can_login: bool = row.get(2);
                let flags = match (superuser, can_login) {
                    (true, _) => "superuser",
                    (false, true) => "can login",
                    (false, false) => "no login",
                };
                DatabaseObject {
                    name: row.get(0),
                    detail: Some(flags.to_owned()),
                }
            })
            .collect())
    }

    /// 实例概览：版本 / 当前库 / 登录身份 / 在线时长 / 库体量 / 编码等服务器级事实。
    ///
    /// 在线时长在 **SQL 里算成秒**再交给 [`human_duration_secs`] 拼——不把
    /// `to_char` 的格式串摊到 SQL 里：那串引号嵌套在 Rust 字符串里极难读，改起来也容易错。
    pub fn instance_overview(&mut self) -> Result<InstanceOverview, DbError> {
        let version = self.server_version()?;
        let row = self
            .client
            .query_one(
                "SELECT current_database(), current_user, \
                        pg_postmaster_start_time()::timestamp(0)::text, \
                        EXTRACT(EPOCH FROM (now() - pg_postmaster_start_time()))::bigint, \
                        pg_size_pretty(pg_database_size(current_database())), \
                        current_setting('server_encoding'), \
                        current_setting('TimeZone'), \
                        current_setting('max_connections'), \
                        (SELECT count(*) FROM pg_database WHERE datallowconn)::text",
                &[],
            )
            .map_err(DbError::from)?;
        let uptime_secs: i64 = row.get(3);
        Ok(InstanceOverview {
            version,
            database: row.get(0),
            user: row.get(1),
            uptime: Some(human_duration_secs(uptime_secs.max(0).unsigned_abs())),
            facts: vec![
                fact("started_at", row.get::<_, String>(2)),
                fact("database_size", row.get::<_, String>(4)),
                fact("server_encoding", row.get::<_, String>(5)),
                fact("timezone", row.get::<_, String>(6)),
                fact("max_connections", row.get::<_, String>(7)),
                fact("databases", row.get::<_, String>(8)),
            ],
        })
    }

    /// 实例指标 / 检查器两张表。
    ///
    /// - **指标**：`pg_stat_database` 里当前库的那一行（事务量、块读写、命中率、临时文件）；
    /// - **检查器**：`pg_stat_activity`（含本连接自己那一条——它也是真实会话，
    ///   隐掉反而让人以为「连接数对不上」）。
    pub fn instance_table(&mut self, view: InstanceView) -> Result<InstanceTable, DbError> {
        match view {
            InstanceView::Metrics => {
                let row = self
                    .client
                    .query_one(
                        "SELECT numbackends::text, xact_commit::text, xact_rollback::text, \
                                blks_read::text, blks_hit::text, \
                                COALESCE(round(100.0 * blks_hit \
                                    / NULLIF(blks_hit + blks_read, 0), 2)::text, '-'), \
                                tup_returned::text, tup_fetched::text, tup_inserted::text, \
                                tup_updated::text, tup_deleted::text, \
                                conflicts::text, deadlocks::text, \
                                temp_files::text, pg_size_pretty(temp_bytes), \
                                pg_size_pretty(pg_database_size(current_database())) \
                         FROM pg_stat_database WHERE datname = current_database()",
                        &[],
                    )
                    .map_err(DbError::from)?;
                const METRICS: [&str; 16] = [
                    "numbackends",
                    "xact_commit",
                    "xact_rollback",
                    "blks_read",
                    "blks_hit",
                    "blks_hit_ratio_%",
                    "tup_returned",
                    "tup_fetched",
                    "tup_inserted",
                    "tup_updated",
                    "tup_deleted",
                    "conflicts",
                    "deadlocks",
                    "temp_files",
                    "temp_bytes",
                    "database_size",
                ];
                let rows = METRICS
                    .iter()
                    .enumerate()
                    .map(|(index, name)| vec![(*name).to_owned(), row.get::<_, String>(index)])
                    .collect();
                Ok(InstanceTable {
                    columns: vec!["metric".into(), "value".into()],
                    rows,
                })
            }
            InstanceView::Processes => {
                let rows = self
                    .client
                    .query(
                        "SELECT pid::text, COALESCE(usename, ''), \
                                COALESCE(client_addr::text, ''), COALESCE(datname, ''), \
                                COALESCE(state, ''), \
                                COALESCE(EXTRACT(EPOCH FROM (now() - query_start))::bigint::text, ''), \
                                COALESCE(wait_event_type || ':' || wait_event, ''), \
                                COALESCE(left(query, 200), '') \
                         FROM pg_stat_activity \
                         ORDER BY query_start DESC NULLS LAST, pid",
                        &[],
                    )
                    .map_err(DbError::from)?;
                Ok(InstanceTable {
                    columns: [
                        "pid",
                        "user",
                        "client",
                        "database",
                        "state",
                        "elapsed_s",
                        "wait",
                        "query",
                    ]
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
                    rows: rows
                        .iter()
                        .map(|row| (0..8).map(|index| row.get::<_, String>(index)).collect())
                        .collect(),
                })
            }
        }
    }

    /// 批量参数化插入：每行一条 INSERT（占位符数固定），**单事务**提交；`None` 写 `NULL`。
    pub fn insert_rows(
        &mut self,
        table: &TableRef,
        columns: &[String],
        rows: &[Vec<Option<String>>],
    ) -> Result<u64, DbError> {
        let types = self.column_sql_types(table).unwrap_or_default();
        let sql = insert_sql(table, &types, columns)?;
        let mut tx = self.client.transaction().map_err(DbError::from)?;
        let mut inserted = 0u64;
        for row in rows {
            let refs: Vec<&(dyn postgres::types::ToSql + Sync)> = row
                .iter()
                .map(|cell| cell as &(dyn postgres::types::ToSql + Sync))
                .collect();
            inserted += tx.execute(sql.as_str(), &refs).map_err(DbError::from)?;
        }
        tx.commit().map_err(DbError::from)?;
        Ok(inserted)
    }
}

/// 拼 INSERT 语句。每个占位符都先 `::text` 再显式转目标类型（见模块头的说明）。
fn insert_sql(
    table: &TableRef,
    types: &HashMap<String, String>,
    columns: &[String],
) -> Result<String, DbError> {
    let quoted = quote_qualified(table, '"')?;
    let quoted_cols: Vec<String> = columns
        .iter()
        .map(|column| quote_identifier(column, '"'))
        .collect::<Result<_, _>>()?;
    let placeholders: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| match types.get(&column.to_lowercase()) {
            Some(sql_type) => format!("${}::text::{sql_type}", index + 1),
            None => format!("${}", index + 1),
        })
        .collect();
    Ok(format!(
        "INSERT INTO {quoted} ({}) VALUES ({})",
        quoted_cols.join(", "),
        placeholders.join(", ")
    ))
}

/// 拼改格 SQL。参数：`$1` 是新值（`value` 为 `None` 时不占参数，直接写 `NULL` 字面量），
/// 其后依次是主键条件。
///
/// `column_type` 是目标列的 SQL 类型文本（`format_type()` 口径）：给了就走
/// `$1::text::<类型>`，让服务端把参数当 TEXT 收下再由目标类型自己转（见模块头）。
fn update_cell_sql(
    table: &TableRef,
    pk: &PkValues,
    column: &str,
    is_null: bool,
    column_type: Option<&str>,
) -> Result<String, DbError> {
    let quoted = quote_qualified(table, '"')?;
    let quoted_column = quote_identifier(column, '"')?;
    let mut assignments = Vec::with_capacity(pk.len() + 1);
    let mut next = 1usize;
    if is_null {
        // `NULL` 是字面量，不是用户数据 —— 既没有注入面，也避开了「参数类型推断不出」。
        assignments.push(format!("{quoted_column} = NULL"));
    } else {
        let placeholder = match column_type {
            Some(sql_type) => format!("${next}::text::{sql_type}"),
            None => format!("${next}"),
        };
        assignments.push(format!("{quoted_column} = {placeholder}"));
        next += 1;
    }
    for (name, _) in pk {
        let quoted_pk = quote_identifier(name, '"')?;
        assignments.push(format!("CAST({quoted_pk} AS TEXT) = ${next}"));
        next += 1;
    }
    Ok(format!(
        "UPDATE {quoted} SET {} WHERE {}",
        assignments[0],
        assignments[1..].join(" AND ")
    ))
}

/// 参数序列：新值（`None` 不占位）+ 各主键原值（顺序与 [`update_cell_sql`] 的 `$n` 一致）。
fn update_cell_params(pk: &PkValues, value: Option<&str>) -> Vec<String> {
    let mut owned = Vec::with_capacity(pk.len() + 1);
    if let Some(value) = value {
        owned.push(value.to_owned());
    }
    owned.extend(pk.iter().map(|(_, original)| original.clone()));
    owned
}

/// 借用一串 `String` 作 postgres 的参数切片。
fn as_sql_params(owned: &[String]) -> Vec<&(dyn postgres::types::ToSql + Sync)> {
    owned
        .iter()
        .map(|text| text as &(dyn postgres::types::ToSql + Sync))
        .collect()
}

/// 在给定执行体（client / 事务）上按主键删一行（参数化）。
///
/// postgres 的 `Client` 与 `Transaction` **没有共同 trait**（`Transaction` 不 Deref 到
/// `Client`），所以这里把「怎么跑」收成一个闭包参数，两边各自传自己那句 `execute`。
fn delete_row_with(
    table: &TableRef,
    pk: &PkValues,
    mut run: impl FnMut(&str, &[&(dyn postgres::types::ToSql + Sync)]) -> Result<u64, postgres::Error>,
) -> Result<u64, DbError> {
    let quoted = quote_qualified(table, '"')?;
    let mut conditions = Vec::with_capacity(pk.len());
    let mut owned = Vec::with_capacity(pk.len());
    for (index, (name, original)) in pk.iter().enumerate() {
        let quoted_pk = quote_identifier(name, '"')?;
        conditions.push(format!("CAST({quoted_pk} AS TEXT) = ${}", index + 1));
        owned.push(original.clone());
    }
    let sql = format!("DELETE FROM {quoted} WHERE {}", conditions.join(" AND "));
    let refs = as_sql_params(&owned);
    run(&sql, &refs).map_err(DbError::from)
}

/// `pg_constraint.confupdtype` / `confdeltype` 的单字符动作码 → 标准文案
/// （文档口径见 `pg_constraint` 的列说明；未知码保守落 `NO ACTION`）。
fn pg_fk_action(code: i8) -> &'static str {
    match u8::try_from(code) {
        Ok(b'c') => "CASCADE",
        Ok(b'n') => "SET NULL",
        Ok(b'd') => "SET DEFAULT",
        Ok(b'r') => "RESTRICT",
        _ => "NO ACTION",
    }
}

/// `pg_trigger.tgtype` 位图 → (时机, 事件清单)。
///
/// 位定义（`pg_trigger.h`）：ROW=1、BEFORE=2、INSERT=4、DELETE=8、UPDATE=16、
/// TRUNCATE=32、INSTEAD=64。既非 `INSTEAD` 也非 `BEFORE` 的都落 `AFTER`
/// （服务端就是这么解释的，不是「未知」）。
fn pg_trigger_type(tgtype: i16) -> (String, String) {
    let timing = if tgtype & 64 != 0 {
        "INSTEAD OF"
    } else if tgtype & 2 != 0 {
        "BEFORE"
    } else {
        "AFTER"
    };
    let mut events: Vec<&'static str> = Vec::new();
    if tgtype & 4 != 0 {
        events.push("INSERT");
    }
    if tgtype & 8 != 0 {
        events.push("DELETE");
    }
    if tgtype & 16 != 0 {
        events.push("UPDATE");
    }
    if tgtype & 32 != 0 {
        events.push("TRUNCATE");
    }
    (timing.to_owned(), events.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(values: &[(&str, &str)]) -> PkValues {
        values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// 外键动作码：五种码 + 未知码的落点。
    #[test]
    fn fk_action_codes_map_to_standard_words() {
        assert_eq!(pg_fk_action(b'c' as i8), "CASCADE");
        assert_eq!(pg_fk_action(b'n' as i8), "SET NULL");
        assert_eq!(pg_fk_action(b'd' as i8), "SET DEFAULT");
        assert_eq!(pg_fk_action(b'r' as i8), "RESTRICT");
        assert_eq!(pg_fk_action(b'a' as i8), "NO ACTION");
        assert_eq!(pg_fk_action(0), "NO ACTION");
    }

    /// `tgtype` 位图：单事件、多事件、INSTEAD OF、缺省 AFTER。
    #[test]
    fn trigger_type_bits_decode() {
        // BEFORE INSERT = 2 | 4
        assert_eq!(
            pg_trigger_type(2 | 4),
            ("BEFORE".to_owned(), "INSERT".to_owned())
        );
        // AFTER DELETE（AFTER 位不存在，缺省）= 8
        assert_eq!(
            pg_trigger_type(8),
            ("AFTER".to_owned(), "DELETE".to_owned())
        );
        // INSTEAD OF UPDATE = 64 | 16
        assert_eq!(
            pg_trigger_type(64 | 16),
            ("INSTEAD OF".to_owned(), "UPDATE".to_owned())
        );
        // 多事件按固定顺序拼接 = INSERT | DELETE | UPDATE | TRUNCATE（含 ROW 位）
        assert_eq!(
            pg_trigger_type(1 | 4 | 8 | 16 | 32),
            (
                "AFTER".to_owned(),
                "INSERT, DELETE, UPDATE, TRUNCATE".to_owned()
            )
        );
    }

    /// 改格 SQL：有类型走 `::text::<类型>`（让参数当 TEXT 收），无类型退裸参数。
    #[test]
    fn update_sql_casts_parameters_to_the_column_type() {
        let table = TableRef::bare("t");
        let key = pk(&[("id", "7")]);
        let sql =
            update_cell_sql(&table, &key, "score", false, Some("numeric(10,2)")).expect("合法");
        assert_eq!(
            sql,
            "UPDATE \"t\" SET \"score\" = $1::text::numeric(10,2) WHERE CAST(\"id\" AS TEXT) = $2"
        );
        // 无类型信息：不硬塞转换（宁可让服务端按目标列推断）。
        let sql = update_cell_sql(&table, &key, "score", false, None).expect("合法");
        assert_eq!(
            sql,
            "UPDATE \"t\" SET \"score\" = $1 WHERE CAST(\"id\" AS TEXT) = $2"
        );
    }

    /// 写 NULL 走字面量：不占参数位，主键仍从 `$1` 起（参数个数与 [`update_cell_params`] 对齐）。
    #[test]
    fn update_sql_writes_null_as_a_literal() {
        let table = TableRef::bare("t");
        let key = pk(&[("a", "1"), ("b", "2")]);
        let sql = update_cell_sql(&table, &key, "note", true, Some("text")).expect("合法");
        assert_eq!(
            sql,
            "UPDATE \"t\" SET \"note\" = NULL WHERE CAST(\"a\" AS TEXT) = $1 AND CAST(\"b\" AS TEXT) = $2"
        );
        assert_eq!(update_cell_params(&key, None).len(), 2);
        assert_eq!(update_cell_params(&key, Some("x")).len(), 3);
    }

    /// 限定名要进 SQL：schema 段与名字段**各自**加引号。
    #[test]
    fn update_sql_qualifies_the_schema() {
        let table = TableRef::in_schema("analytics", "events");
        let key = pk(&[("id", "1")]);
        let sql = update_cell_sql(&table, &key, "kind", true, None).expect("合法");
        assert!(
            sql.starts_with("UPDATE \"analytics\".\"events\" SET"),
            "{sql}"
        );
    }

    /// 库名可留空：**省略 `dbname`**，而不是传空串。
    ///
    /// 传空串会真的把空库名塞进 startup 包（服务端回 `database "" does not exist`）；
    /// 省略时 PostgreSQL 才按 libpq 规则回落到**用户名同名的库**。
    #[test]
    fn connection_string_omits_dbname_when_blank() {
        let cfg = ConnectionConfig {
            id: "conn-1".into(),
            kind: crate::config::DatabaseKind::Postgres,
            host: "db.local".into(),
            user: "duck".into(),
            database: String::new(),
            ..ConnectionConfig::default()
        };
        let text = connection_string(&cfg, None);
        assert!(!text.contains("dbname"), "留空不该出现 dbname：{text}");
        assert!(text.contains("user=duck"), "{text}");
        assert!(text.contains("connect_timeout=5"), "{text}");

        let named = ConnectionConfig {
            database: "shop".into(),
            ..cfg
        };
        assert!(connection_string(&named, None).contains("dbname=shop"));
    }
}
