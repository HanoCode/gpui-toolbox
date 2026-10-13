//! SQLite 驱动（rusqlite，bundled 内嵌）。
//!
//! `ConnectionConfig::database` 即数据库文件路径（`:memory:` 允许）。

use rusqlite::Connection;
use rusqlite::types::ValueRef;

use crate::cancel::CancelHandle;
use crate::config::ConnectionConfig;
use crate::drivers::{
    CatalogTable, DatabaseObject, InstanceFact, InstanceOverview, InstanceTable, InstanceView,
    OrderBy, PlaceholderStyle, RowCount, TableFilter, TableForeignKey, TableIndex, TableKind,
    TableRef, TableTrigger, human_bytes, order_by_clause, plain_filter_clause, quote_identifier,
    quote_qualified,
};
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value};
use crate::sqlintel::split_statements;

/// 一条 SQLite 连接。
pub struct SqliteConnection {
    conn: Connection,
}

/// SQLite 里算「读语句」的关键字头：SELECT 之外还有返回行的 PRAGMA。
fn sqlite_reads(stmt: &str) -> bool {
    let head = stmt
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(
        head.as_str(),
        "SELECT" | "PRAGMA" | "WITH" | "EXPLAIN" | "VALUES"
    )
}

/// SQLite 里没给 schema 时的缺省数据库（`main` 是所有连接都有的那个）。
const MAIN_SCHEMA: &str = "main";

/// 布尔型 PRAGMA（`foreign_keys` / `query_only`）的展示值（英文，语言无关）。
fn on_off(value: u64) -> String {
    if value == 0 {
        "OFF".to_owned()
    } else {
        "ON".to_owned()
    }
}

/// 指标表的一行（`名字 — 值`）。
fn kv_row(name: &str, value: &str) -> Vec<String> {
    vec![name.to_owned(), value.to_owned()]
}

/// 从 `CREATE TRIGGER` 语句头部解析触发时机与事件（右栏「触发器」卡展示用）。
///
/// SQLite 不像 MySQL 那样把 `ACTION_TIMING` / `EVENT_MANIPULATION` 单列给出，
/// 只能从建表 SQL 头部读。按**整词**匹配 `BEFORE` / `AFTER` / `INSTEAD OF` 与
/// 其后的首个事件关键字；这是启发式（名字里恰好含这些词、或语句不是
/// `CREATE TRIGGER` 开头的极端情形落空串），解析失败不影响数据本身。
pub(crate) fn trigger_timing_event(sql: &str) -> (String, String) {
    /// `kw` 在 `text` 里作为独立词出现的位置（两侧不得紧邻字母数字 / 下划线）。
    fn find_keyword(text: &str, kw: &str) -> Option<usize> {
        let mut from = 0;
        while let Some(idx) = text[from..].find(kw) {
            let start = from + idx;
            let end = start + kw.len();
            let before_ok = text[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
            let after_ok = text[end..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
            if before_ok && after_ok {
                return Some(start);
            }
            from = start + 1;
        }
        None
    }

    let lower = sql.to_ascii_lowercase();
    // 语句头止于「ON 表名」；表名之后的正文里这些词没有意义。
    let head = match lower.split_once(" on ") {
        Some((head, _)) => head,
        None => lower.as_str(),
    };
    let Some((pos, timing)) = ["instead of", "before", "after"]
        .into_iter()
        .filter_map(|kw| find_keyword(head, kw).map(|pos| (pos, kw)))
        .min_by_key(|&(pos, _)| pos)
    else {
        return (String::new(), String::new());
    };
    let tail = &head[pos + timing.len()..];
    // `UPDATE OF 列清单` 也归为 UPDATE（事件种类只到语句级）。
    let event = ["insert", "update", "delete", "truncate"]
        .into_iter()
        .filter_map(|kw| find_keyword(tail, kw).map(|pos| (pos, kw)))
        .min_by_key(|&(pos, _)| pos)
        .map_or(String::new(), |(_, kw)| kw.to_ascii_uppercase());
    (timing.to_ascii_uppercase(), event)
}

/// `pragma_xxx` 这类**表值函数**的两个参数：对象名 + schema。
///
/// 两参形式（`pragma_table_info(?1, ?2)`）是 SQLite 3.16 起支持的，比
/// `PRAGMA "schema".table_info(...)` 好在**参数可绑定**——schema 名不必拼进 SQL 文本。
fn pragma_target(table: &TableRef) -> (String, &str) {
    (
        table.name.clone(),
        table.schema.as_deref().unwrap_or(MAIN_SCHEMA),
    )
}

/// 单元格值映射。
fn map_value(raw: ValueRef<'_>) -> Value {
    match raw {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => Value::Int(v),
        ValueRef::Real(v) => Value::Float(v),
        ValueRef::Text(v) => Value::Text(String::from_utf8_lossy(v).into_owned()),
        ValueRef::Blob(v) => Value::Bytes(v.to_vec()),
    }
}

impl SqliteConnection {
    /// 建连（打开数据库文件；父目录不存在时由 SQLite 自身报错）。
    pub(crate) fn connect(cfg: &ConnectionConfig) -> Result<Self, DbError> {
        let path = cfg.database.trim();
        if path.is_empty() {
            return Err(DbError::Config("SQLite file path is empty".into()));
        }
        let conn = Connection::open(path).map_err(|err| DbError::Connect(err.to_string()))?;
        tracing::info!(path, "sqlite connected");
        Ok(Self { conn })
    }

    /// 活性探测。
    pub fn ping(&mut self) -> Result<(), DbError> {
        self.conn
            .query_row("SELECT 1", [], |_| Ok(()))
            .map_err(DbError::from)
    }

    /// 带外取消句柄（`sqlite3_interrupt`；在跑长查询时也拿得到，不占连接锁）。
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::sqlite(self.conn.get_interrupt_handle())
    }

    /// 版本描述。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        let version: String = self
            .conn
            .query_row("SELECT sqlite_version() || ' (SQLite)'", [], |row| {
                row.get(0)
            })
            .map_err(DbError::from)?;
        Ok(version)
    }

    /// 连接级只读：`PRAGMA query_only`（服务器自己拒绝写，不是界面软拦）。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        let value = if read_only { "ON" } else { "OFF" };
        self.conn
            .execute_batch(&format!("PRAGMA query_only = {value}"))
            .map_err(|err| DbError::Query(err.to_string()))
    }

    /// 执行一段 SQL（多条语句逐条跑），读语句在 `max_rows` 处截断。
    pub fn execute(&mut self, sql: &str, max_rows: usize) -> Result<ExecOutcome, DbError> {
        let statements = split_statements(sql);
        if statements.is_empty() {
            return Err(DbError::Query("no statement to execute".into()));
        }
        let mut outcome = ExecOutcome::default();
        for stmt in statements {
            if sqlite_reads(&stmt) {
                // 读路径：结果集（可能零行）就是产出，不进 affected。
                outcome.result = Some(self.query_one(&stmt, &[], max_rows)?);
            } else {
                outcome.affected += u64::try_from(
                    self.conn
                        .execute(&stmt, [])
                        .map_err(|err| DbError::Query(err.to_string()))?,
                )
                .unwrap_or(u64::MAX);
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
        self.query_one(&wrapped, &[], limit)
    }

    /// 跑一条**单条**读语句并取结果集（`max_rows + 1` 探测截断）。
    ///
    /// `params` 是按占位符顺序绑定的值（空 `Vec` = 无参数）；表浏览的过滤值走这里，
    /// 一律以 `Value::Text` 送下去——SQLite 按列的亲和性（affinity）自行转换。
    fn query_one(
        &self,
        stmt: &str,
        params: &[rusqlite::types::Value],
        max_rows: usize,
    ) -> Result<ResultSet, DbError> {
        let mut prepared = self
            .conn
            .prepare(stmt)
            .map_err(|err| DbError::Query(err.to_string()))?;
        // 列名先取一份 owned（query 期间 Statement 被行迭代借用）。
        let column_count = prepared.column_count();
        let mut names = Vec::with_capacity(column_count);
        for index in 0..column_count {
            names.push(
                prepared
                    .column_name(index)
                    .expect("column index is always in range")
                    .to_owned(),
            );
        }
        let columns: Vec<Column> = names
            .into_iter()
            .map(|name| Column {
                name,
                type_name: String::new(),
            })
            .collect();

        let mut rows_out = Vec::new();
        let mut truncated = false;
        let mut rows = prepared
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(|err| DbError::Query(err.to_string()))?;
        while let Some(row) = rows.next().map_err(DbError::from)? {
            if rows_out.len() >= max_rows {
                truncated = true;
                break;
            }
            let mut cells = Vec::with_capacity(columns.len());
            for index in 0..columns.len() {
                let raw = row.get_ref(index).map_err(DbError::from)?;
                cells.push(map_value(raw));
            }
            rows_out.push(cells);
        }
        Ok(ResultSet {
            columns,
            rows: rows_out,
            truncated,
        })
    }

    /// 可选的命名空间清单：`pragma_database_list`（`main` / `temp` / 附加库）。
    ///
    /// SQLite 没有「库 → schema → 表」三层，但 `ATTACH` 进来的库确实是分开的命名空间，
    /// 照实列出来比假装只有一层有用。
    /// SQLite 的「库」就是 `main` / `temp` / 已 `ATTACH` 的那些数据库文件。
    ///
    /// 与 [`Self::list_schemas`] 完全同源（`pragma_database_list`）—— 在 SQLite 里
    /// 这两层本来就是同一层，硬造一个「库清单」只会在界面上多出一份重复的下拉。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        self.list_schemas()
    }

    /// 切换「当前库」：SQLite **没有**这个动作。
    ///
    /// 一个连接对应一个文件，语句里的表名要么落在 `main`、要么由用户自己写
    /// `ATTACH` 进来的名字前缀。本仓不给「隐式 ATTACH」——那等于替用户改查询
    /// 的落点。所以这里刻意是空操作，把 `Applied` 语义留给上层。
    pub fn use_database(&mut self, _name: &str) -> Result<(), DbError> {
        Ok(())
    }

    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        let mut prepared = self
            .conn
            .prepare("SELECT name FROM pragma_database_list ORDER BY name")
            .map_err(|err| DbError::Query(err.to_string()))?;
        let rows = prepared
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut schemas = Vec::new();
        for row in rows {
            schemas.push(row.map_err(DbError::from)?);
        }
        Ok(schemas)
    }

    /// 读一条单值 PRAGMA 成字符串（`page_size` 这类整数与 `journal_mode` 这类文本都能取）。
    ///
    /// 走 `query_row` 而不是 `execute_batch`：这些 PRAGMA **返回值**才是要的东西，
    /// 当设置语句发出去既看不见回执、又会真的改状态。
    fn pragma_scalar(&self, name: &str) -> Result<String, DbError> {
        self.conn
            .query_row(&format!("PRAGMA {name}"), [], |row| {
                Ok(match row.get_ref(0)? {
                    ValueRef::Null => String::new(),
                    ValueRef::Integer(value) => value.to_string(),
                    ValueRef::Real(value) => value.to_string(),
                    ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned(),
                    ValueRef::Blob(value) => format!("<{} bytes>", value.len()),
                })
            })
            .map_err(|err| DbError::Query(err.to_string()))
    }

    /// 同 [`Self::pragma_scalar`]，但按 u64 解析（解析不出按 0，只用于展示）。
    fn pragma_u64(&self, name: &str) -> Result<u64, DbError> {
        Ok(self
            .pragma_scalar(name)?
            .trim()
            .parse::<u64>()
            .unwrap_or_default())
    }

    /// 本连接**已挂载**的数据库：`main` / `temp` / `ATTACH` 进来的（`pragma_database_list`）。
    fn attached_databases(&self) -> Result<Vec<(String, String)>, DbError> {
        let mut prepared = self
            .conn
            .prepare("SELECT name, file FROM pragma_database_list ORDER BY seq")
            .map_err(|err| DbError::Query(err.to_string()))?;
        let rows = prepared
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(DbError::from)?);
        }
        Ok(out)
    }

    /// 实例概览：SQLite 没有服务端，「实例」就是这个数据库文件。
    ///
    /// 取的全是 `PRAGMA` 能给出的真实状态（页大小 / 页数 / 日志模式 / 编码 / 只读档），
    /// **不编造**服务器版本与在线时长——`uptime` 恒 `None`（界面据此不画那一行），
    /// `user` 恒空串（SQLite 没有用户体系，文件权限就是一切）。
    pub fn instance_overview(&mut self) -> Result<InstanceOverview, DbError> {
        let version = self.server_version()?;
        let attached = self.attached_databases()?;
        let file = attached
            .iter()
            .find(|(name, _)| name == MAIN_SCHEMA)
            .map(|(_, file)| file.clone())
            .unwrap_or_default();
        let page_size = self.pragma_u64("page_size")?;
        let page_count = self.pragma_u64("page_count")?;
        Ok(InstanceOverview {
            version,
            database: file,
            user: String::new(),
            uptime: None,
            facts: vec![
                InstanceFact {
                    name: "journal_mode".into(),
                    value: self.pragma_scalar("journal_mode")?,
                },
                InstanceFact {
                    name: "encoding".into(),
                    value: self.pragma_scalar("encoding")?,
                },
                InstanceFact {
                    name: "page_size".into(),
                    value: page_size.to_string(),
                },
                InstanceFact {
                    name: "page_count".into(),
                    value: page_count.to_string(),
                },
                InstanceFact {
                    name: "database_size".into(),
                    value: human_bytes(page_size.saturating_mul(page_count)),
                },
                InstanceFact {
                    name: "foreign_keys".into(),
                    value: on_off(self.pragma_u64("foreign_keys")?),
                },
                InstanceFact {
                    name: "read_only".into(),
                    value: on_off(self.pragma_u64("query_only")?),
                },
                InstanceFact {
                    name: "attached_databases".into(),
                    value: attached.len().to_string(),
                },
            ],
        })
    }

    /// 实例指标 / 检查器两张表。
    ///
    /// - **指标**：页级体量与缓存配置（都是 `PRAGMA` 实测值，不是估算）；
    /// - **检查器**：本连接已挂载的数据库 —— SQLite 是**进程内**数据库，没有服务端
    ///   会话列表，能「检查」的只有这条连接够得着哪些文件。这一条由界面按引擎解释，
    ///   内核只给数据。
    pub fn instance_table(&mut self, view: InstanceView) -> Result<InstanceTable, DbError> {
        match view {
            InstanceView::Metrics => {
                let page_size = self.pragma_u64("page_size")?;
                let page_count = self.pragma_u64("page_count")?;
                let cache = self.pragma_scalar("cache_size")?;
                // `cache_size` 为正 = 页数，为负 = KiB（SQLite 的符号约定）。
                let cache_value = match cache.trim().parse::<i64>() {
                    Ok(pages) if pages < 0 => format!("{} KiB", pages.unsigned_abs()),
                    Ok(pages) => format!("{pages} pages"),
                    Err(_) => cache,
                };
                let rows = vec![
                    kv_row("page_size", &format!("{page_size} B")),
                    kv_row("page_count", &page_count.to_string()),
                    kv_row(
                        "database_size",
                        &human_bytes(page_size.saturating_mul(page_count)),
                    ),
                    kv_row(
                        "freelist_count",
                        &self.pragma_u64("freelist_count")?.to_string(),
                    ),
                    kv_row("cache_size", &cache_value),
                    kv_row(
                        "schema_version",
                        &self.pragma_u64("schema_version")?.to_string(),
                    ),
                    kv_row(
                        "user_version",
                        &self.pragma_u64("user_version")?.to_string(),
                    ),
                ];
                Ok(InstanceTable {
                    columns: vec!["metric".into(), "value".into()],
                    rows,
                })
            }
            InstanceView::Processes => Ok(InstanceTable {
                columns: vec!["attached_database".into(), "file".into()],
                rows: self
                    .attached_databases()?
                    .into_iter()
                    .map(|(name, file)| vec![name, file])
                    .collect(),
            }),
        }
    }

    /// 某个命名空间的表 / 视图（`sqlite_master`，排除 sqlite_ 内部表）。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        let schema = schema.unwrap_or(MAIN_SCHEMA);
        let master = quote_qualified(&TableRef::in_schema(schema, "sqlite_master"), '"')?;
        let sql = format!(
            "SELECT name, type FROM {master} \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
             ORDER BY lower(name)"
        );
        let mut prepared = self
            .conn
            .prepare(&sql)
            .map_err(|err| DbError::Query(err.to_string()))?;
        let rows = prepared
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut tables = Vec::new();
        for row in rows {
            let (name, kind) = row.map_err(DbError::from)?;
            tables.push(CatalogTable {
                name,
                kind: if kind == "view" {
                    TableKind::View
                } else {
                    TableKind::Table
                },
                schema: Some(schema.to_owned()),
            });
        }
        Ok(tables)
    }

    /// 一张表的列（`pragma_table_info` 表值函数，支持绑定参数）。
    ///
    /// ⚠️ 第二个参数是 schema——两参形式才能查附加库里的表；不带它只会查 `main`。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        let (name, schema) = pragma_target(table);
        let mut prepared = self
            .conn
            .prepare("SELECT name, coalesce(type, '') FROM pragma_table_info(?1, ?2)")
            .map_err(|err| DbError::Query(err.to_string()))?;
        let rows = prepared
            .query_map(rusqlite::params![name, schema], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut columns = Vec::new();
        for row in rows {
            let (name, type_name) = row.map_err(DbError::from)?;
            columns.push(Column { name, type_name });
        }
        Ok(columns)
    }

    /// 取一张表的分页数据。
    ///
    /// `order_by` 是调用方给的**稳定序**（通常是主键）：没有它 `LIMIT/OFFSET` 的
    /// 「第 N 页」在 SQLite 里只是「碰巧按 rowid 扫」，视图与 `WITHOUT ROWID` 表上
    /// 更是不保证——翻页会重复或漏行。
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
        let (where_clause, values) = plain_filter_clause(filters, '"', PlaceholderStyle::Question)?;
        let sql =
            format!("SELECT * FROM {quoted}{where_clause}{order} LIMIT {limit} OFFSET {offset}");
        let params: Vec<rusqlite::types::Value> = values
            .into_iter()
            .map(rusqlite::types::Value::Text)
            .collect();
        self.query_one(&sql, &params, limit)
    }

    /// 主键列名（`pragma_table_info` 的 `pk` 列：>0 即在主键里，数值是声明序）。
    pub(crate) fn table_primary_key(&mut self, table: &TableRef) -> Result<Vec<String>, DbError> {
        let (name, schema) = pragma_target(table);
        let mut prepared = self
            .conn
            .prepare("SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk")
            .map_err(|err| DbError::Query(err.to_string()))?;
        let rows = prepared
            .query_map(rusqlite::params![name, schema], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut columns = Vec::new();
        for row in rows {
            columns.push(row.map_err(DbError::from)?);
        }
        Ok(columns)
    }

    /// 改一格：主键值按 `CAST(x AS TEXT)` 文本比较，命中行数即返回值。
    /// `value` 为 `None` 时写 `NULL`（不是空串——两者在库里完全不同）。
    pub(crate) fn update_cell(
        &mut self,
        table: &TableRef,
        pk: &super::PkValues,
        column: &str,
        value: Option<&str>,
    ) -> Result<u64, DbError> {
        update_cell_on(&self.conn, table, pk, column, value)
    }

    /// **单事务**批量改格（暂存式编辑提交走这里）：多格改动要么全成、要么全回滚。
    pub(crate) fn update_cells(
        &mut self,
        table: &TableRef,
        edits: &[super::CellEdit],
    ) -> Result<u64, DbError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut affected = 0u64;
        for edit in edits {
            affected += update_cell_on(&tx, table, &edit.pk, &edit.column, edit.value.as_deref())?;
        }
        tx.commit().map_err(|err| DbError::Query(err.to_string()))?;
        Ok(affected)
    }

    /// 按主键删行。
    pub(crate) fn delete_rows(
        &mut self,
        table: &TableRef,
        pk: &super::PkValues,
    ) -> Result<u64, DbError> {
        delete_rows_on(&self.conn, table, pk)
    }

    /// **单事务**批量删行（结果网格多选删除走这里）：要么全删、要么一行都不动。
    pub(crate) fn delete_row_set(
        &mut self,
        table: &TableRef,
        keys: &[super::PkValues],
    ) -> Result<u64, DbError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut affected = 0u64;
        for pk in keys {
            affected += delete_rows_on(&tx, table, pk)?;
        }
        tx.commit().map_err(|err| DbError::Query(err.to_string()))?;
        Ok(affected)
    }

    /// **单事务**提交一次暂存式编辑（改格 + 新增行混排）。
    pub(crate) fn apply_batch(
        &mut self,
        table: &TableRef,
        edits: &[super::CellEdit],
        inserts: &[super::InsertRow],
    ) -> Result<super::BatchOutcome, DbError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut updated = 0u64;
        for edit in edits {
            updated += update_cell_on(&tx, table, &edit.pk, &edit.column, edit.value.as_deref())?;
        }
        let mut inserted = 0u64;
        for row in inserts {
            inserted += insert_row_on(&tx, table, &row.columns, &row.values)?;
        }
        tx.commit().map_err(|err| DbError::Query(err.to_string()))?;
        Ok(super::BatchOutcome { updated, inserted })
    }

    /// 一张表的建表 DDL（`sqlite_master.sql` 原文；视图同理）。
    pub(crate) fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        let schema = table.schema.as_deref().unwrap_or(MAIN_SCHEMA);
        let master = quote_qualified(&TableRef::in_schema(schema, "sqlite_master"), '"')?;
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT COALESCE(sql, '') FROM {master} WHERE name = ?1"
            ))
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut rows = stmt
            .query(rusqlite::params![table.name])
            .map_err(|err| DbError::Query(err.to_string()))?;
        match rows.next().map_err(|err| DbError::Query(err.to_string()))? {
            Some(row) => {
                let sql: String = row.get(0).map_err(|err| DbError::Query(err.to_string()))?;
                if sql.is_empty() {
                    // 自动索引之类没有 SQL 的条目。
                    Err(DbError::Query(format!("no DDL stored for {table}")))
                } else {
                    Ok(sql)
                }
            }
            None => Err(DbError::Query(format!("table not found: {table}"))),
        }
    }

    /// 一张表上的索引：`pragma_index_list` 取名 / 唯一性 / 来源，再逐个
    /// `pragma_index_info` 取列序。
    ///
    /// 含 `sqlite_autoindex_*`（UNIQUE / PRIMARY KEY 隐式建的索引）——它们没有可读的
    /// DDL，但确实是约束的落地形态，展示出来比隐藏有用。
    pub(crate) fn table_indexes(&mut self, table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        let (name, schema) = pragma_target(table);
        let mut head = self
            .conn
            .prepare("SELECT name, \"unique\", origin FROM pragma_index_list(?1, ?2) ORDER BY seq")
            .map_err(|err| DbError::Query(err.to_string()))?;
        let listed: Vec<(String, bool, String)> = head
            .query_map(rusqlite::params![name, schema], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|err| DbError::Query(err.to_string()))?
            .collect::<Result<_, _>>()
            .map_err(DbError::from)?;
        // 先收完上层结果再取列：避免同一条连接上叠两个活着的 `Statement`。
        drop(head);

        let mut indexes = Vec::with_capacity(listed.len());
        for (name, unique, origin) in listed {
            let mut info = self
                .conn
                .prepare("SELECT name FROM pragma_index_info(?1, ?2) ORDER BY seqno")
                .map_err(|err| DbError::Query(err.to_string()))?;
            let columns: Vec<Option<String>> = info
                .query_map(rusqlite::params![name, schema], |row| {
                    row.get::<_, Option<String>>(0)
                })
                .map_err(|err| DbError::Query(err.to_string()))?
                .collect::<Result<_, _>>()
                .map_err(DbError::from)?;
            drop(info);
            indexes.push(TableIndex {
                name,
                // 表达式索引的列名是 NULL，跳过（SQLite 的表达式索引极少见）。
                columns: columns.into_iter().flatten().collect(),
                unique,
                primary: origin == "pk",
            });
        }
        Ok(indexes)
    }

    /// 一张表上的外键：`pragma_foreign_key_list` 按约束 `id` 分组（复合外键
    /// 是同 `id` 的多行，`seq` 即列序）。
    ///
    /// SQLite 的外键没有约束名，[`TableForeignKey::name`] 落空串；
    /// `to` 列为 NULL 表示隐式引用对方主键，落空串。
    pub(crate) fn table_foreign_keys(
        &mut self,
        table: &TableRef,
    ) -> Result<Vec<TableForeignKey>, DbError> {
        let (name, schema) = pragma_target(table);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete \
                 FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
            )
            .map_err(|err| DbError::Query(err.to_string()))?;
        let rows: Vec<(i64, String, String, Option<String>, String, String)> = stmt
            .query_map(rusqlite::params![name, schema], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .map_err(|err| DbError::Query(err.to_string()))?
            .collect::<Result<_, _>>()
            .map_err(DbError::from)?;
        drop(stmt);

        // 已按 `id` 排序，同一约束的列必定连续。
        let mut keys: Vec<TableForeignKey> = Vec::new();
        for (id, ref_table, from, to, on_update, on_delete) in rows {
            match keys.last_mut() {
                Some(fk) if fk.name == format!("fk:{id}") => {
                    fk.columns.push(from);
                    fk.ref_columns.push(to.unwrap_or_default());
                }
                _ => keys.push(TableForeignKey {
                    name: format!("fk:{id}"),
                    columns: vec![from],
                    ref_table,
                    ref_columns: vec![to.unwrap_or_default()],
                    on_update,
                    on_delete,
                }),
            }
        }
        // 内部分组键 `fk:<id>` 不外泄——SQLite 没有真约束名。
        for fk in &mut keys {
            fk.name = String::new();
        }
        Ok(keys)
    }

    /// 一张表上的触发器：`sqlite_master` 里 `type = 'trigger'` 且 `tbl_name` 命中，
    /// 时机 / 事件由建表 SQL 头部解析（[`trigger_timing_event`]）。
    pub(crate) fn table_triggers(
        &mut self,
        table: &TableRef,
    ) -> Result<Vec<TableTrigger>, DbError> {
        let schema = table
            .schema
            .as_deref()
            .filter(|schema| !schema.is_empty())
            .unwrap_or("main");
        let master = quote_qualified(&TableRef::in_schema(schema, "sqlite_master"), '"')?;
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT name, COALESCE(sql, '') FROM {master} \
                 WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name"
            ))
            .map_err(|err| DbError::Query(err.to_string()))?;
        let rows: Vec<(String, String)> = stmt
            .query_map(rusqlite::params![table.name], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|err| DbError::Query(err.to_string()))?
            .collect::<Result<_, _>>()
            .map_err(DbError::from)?;
        drop(stmt);
        Ok(rows
            .into_iter()
            .map(|(name, sql)| {
                let (timing, event) = trigger_timing_event(&sql);
                TableTrigger {
                    name,
                    timing,
                    event,
                }
            })
            .collect())
    }

    /// 一个数据库（`main` / 附加库）里的触发器清单，说明列 = 挂在哪张表上。
    pub(crate) fn list_triggers(
        &mut self,
        schema: Option<&str>,
    ) -> Result<Vec<DatabaseObject>, DbError> {
        let schema = schema.filter(|schema| !schema.is_empty()).unwrap_or("main");
        let master = quote_qualified(&TableRef::in_schema(schema, "sqlite_master"), '"')?;
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT name, tbl_name FROM {master} \
                 WHERE type = 'trigger' ORDER BY name"
            ))
            .map_err(|err| DbError::Query(err.to_string()))?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|err| DbError::Query(err.to_string()))?
            .collect::<Result<_, _>>()
            .map_err(DbError::from)?;
        drop(stmt);
        Ok(rows
            .into_iter()
            .map(|(name, table)| DatabaseObject {
                name,
                detail: Some(table),
            })
            .collect())
    }

    /// 行数：SQLite 没有系统统计信息，只能 `COUNT(*)`（本地文件，代价可接受）。
    ///
    /// 视图同样可数（会真的把它跑一遍）；不可数的视图报错，由调用方 `.ok()` 掉。
    pub(crate) fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        let quoted = quote_qualified(table, '"')?;
        let count: i64 = self
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {quoted}"), [], |row| {
                row.get(0)
            })
            .map_err(|err| DbError::Query(err.to_string()))?;
        Ok(RowCount::Exact(u64::try_from(count).unwrap_or_default()))
    }

    /// 批量参数化插入：预编译一条 INSERT，**单事务**逐行绑定执行；`None` 写 `NULL`。
    ///
    /// 事务是必需的：CSV 导入几百行时既省掉逐行 fsync，又保证失败整批回滚
    /// （半截导入比不导入更难收拾）。
    pub(crate) fn insert_rows(
        &mut self,
        table: &TableRef,
        columns: &[String],
        rows: &[Vec<Option<String>>],
    ) -> Result<u64, DbError> {
        let quoted = quote_qualified(table, '"')?;
        let quoted_cols: Vec<String> = columns
            .iter()
            .map(|column| quote_identifier(column, '"'))
            .collect::<Result<_, _>>()?;
        let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "INSERT INTO {quoted} ({}) VALUES ({})",
            quoted_cols.join(", "),
            placeholders.join(", ")
        );
        let tx = self
            .conn
            .transaction()
            .map_err(|err| DbError::Query(err.to_string()))?;
        let mut inserted = 0u64;
        {
            let mut stmt = tx
                .prepare(&sql)
                .map_err(|err| DbError::Query(err.to_string()))?;
            for row in rows {
                let affected = stmt
                    .execute(rusqlite::params_from_iter(row.iter()))
                    .map_err(|err| DbError::Query(err.to_string()))?;
                inserted += u64::try_from(affected).unwrap_or_default();
            }
        }
        tx.commit().map_err(|err| DbError::Query(err.to_string()))?;
        Ok(inserted)
    }
}

/// 在给定连接（或事务）上按给定列集插一行（参数化；`None` 绑 `Value::Null`）。
///
/// 与 [`SqliteConnection::insert_rows`] 的区别是**列集逐行给**：暂存式编辑里每条新行
/// 用户填的列可能不同，没填的列要落到 `DEFAULT` 而不是被写成 `NULL`。
fn insert_row_on(
    conn: &Connection,
    table: &TableRef,
    columns: &[String],
    values: &[Option<String>],
) -> Result<u64, DbError> {
    let quoted = quote_qualified(table, '"')?;
    let quoted_cols: Vec<String> = columns
        .iter()
        .map(|column| quote_identifier(column, '"'))
        .collect::<Result<_, _>>()?;
    let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "INSERT INTO {quoted} ({}) VALUES ({})",
        quoted_cols.join(", "),
        placeholders.join(", ")
    );
    let params: Vec<rusqlite::types::Value> = values
        .iter()
        .map(|value| match value {
            Some(text) => rusqlite::types::Value::Text(text.clone()),
            None => rusqlite::types::Value::Null,
        })
        .collect();
    let affected = conn
        .execute(&sql, rusqlite::params_from_iter(params))
        .map_err(|err| DbError::Query(err.to_string()))?;
    Ok(u64::try_from(affected).unwrap_or_default())
}

/// 在给定连接（或事务）上按主键删一行：参数化 + `CAST(x AS TEXT)` 主键文本比较。
fn delete_rows_on(
    conn: &Connection,
    table: &TableRef,
    pk: &super::PkValues,
) -> Result<u64, DbError> {
    let quoted = quote_qualified(table, '"')?;
    let mut params = Vec::with_capacity(pk.len());
    let mut conditions = Vec::with_capacity(pk.len());
    for (index, (name, original)) in pk.iter().enumerate() {
        let quoted_pk = quote_identifier(name, '"')?;
        conditions.push(format!("CAST({quoted_pk} AS TEXT) = ?{}", index + 1));
        params.push(rusqlite::types::Value::Text(original.clone()));
    }
    let sql = format!("DELETE FROM {quoted} WHERE {}", conditions.join(" AND "));
    let affected = conn
        .execute(&sql, rusqlite::params_from_iter(params))
        .map_err(|err| DbError::Query(err.to_string()))?;
    Ok(u64::try_from(affected).unwrap_or_default())
}

/// 在给定连接（或事务）上改一格：参数化 + `CAST(x AS TEXT)` 主键文本比较。
///
/// `value` 为 `None` 时绑 `Value::Null` —— 「写 NULL」与「写空串」是两件事，
/// 用 `Option` 在类型上分开，避免调用方用 `""` 表达 NULL。
fn update_cell_on(
    conn: &Connection,
    table: &TableRef,
    pk: &super::PkValues,
    column: &str,
    value: Option<&str>,
) -> Result<u64, DbError> {
    let quoted = quote_qualified(table, '"')?;
    let quoted_column = quote_identifier(column, '"')?;
    let mut params = vec![match value {
        Some(text) => rusqlite::types::Value::Text(text.to_owned()),
        None => rusqlite::types::Value::Null,
    }];
    let mut conditions = Vec::with_capacity(pk.len());
    for (index, (name, original)) in pk.iter().enumerate() {
        let quoted_pk = quote_identifier(name, '"')?;
        conditions.push(format!("CAST({quoted_pk} AS TEXT) = ?{}", index + 2));
        params.push(rusqlite::types::Value::Text(original.clone()));
    }
    let sql = format!(
        "UPDATE {quoted} SET {quoted_column} = ?1 WHERE {}",
        conditions.join(" AND ")
    );
    let affected = conn
        .execute(&sql, rusqlite::params_from_iter(params))
        .map_err(|err| DbError::Query(err.to_string()))?;
    Ok(u64::try_from(affected).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::{CellEdit, PkValues};

    /// 编辑全链路：主键内省 → 改一格 → 删行（内存库，覆盖 CAST 文本比较）。
    #[test]
    fn edit_roundtrip() {
        let conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        let mut conn = conn;
        conn.conn
            .execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, note TEXT);
                 INSERT INTO t VALUES (1, 'a', 'x'), (2, 'b', 'y'), (3, 'c', NULL);",
            )
            .expect("建表");

        let table = TableRef::bare("t");
        let pk = conn.table_primary_key(&table).expect("主键");
        assert_eq!(pk, vec!["id".to_owned()]);
        assert!(
            conn.table_primary_key(&TableRef::bare("nope"))
                .expect("不存在的表")
                .is_empty()
        );

        let pk_values: PkValues = vec![("id".to_owned(), "2".to_owned())];
        let affected = conn
            .update_cell(&table, &pk_values, "name", Some("改过的 b"))
            .expect("改一格");
        assert_eq!(affected, 1);

        let result = conn
            .execute("SELECT name FROM t WHERE id = 2", 10)
            .expect("回查")
            .result
            .expect("结果集");
        assert_eq!(result.rows[0][0].render(), "改过的 b");

        // 单引号值不破坏 SQL。
        let affected = conn
            .update_cell(&table, &pk_values, "note", Some("it's ok"))
            .expect("带引号改值");
        assert_eq!(affected, 1);

        // `None` 写的是 NULL（不是空串）：`IS NULL` 命中，`= ''` 不命中。
        let affected = conn
            .update_cell(&table, &pk_values, "note", None)
            .expect("写 NULL");
        assert_eq!(affected, 1);
        let result = conn
            .execute("SELECT count(*) FROM t WHERE id = 2 AND note IS NULL", 10)
            .expect("回查 NULL")
            .result
            .expect("结果集");
        assert_eq!(result.rows[0][0].render(), "1", "None 必须落成 NULL");

        // 无主键表拒绝写。
        conn.conn
            .execute_batch("CREATE TABLE nokey(a TEXT)")
            .expect("建无主键表");
        assert!(
            conn.update_cell(&TableRef::bare("nokey"), &PkValues::new(), "a", Some("x"))
                .is_err()
        );

        let affected = conn.delete_rows(&table, &pk_values).expect("删行");
        assert_eq!(affected, 1);
        let result = conn
            .execute("SELECT count(*) FROM t", 10)
            .expect("计数")
            .result
            .expect("结果集");
        assert_eq!(result.rows[0][0].render(), "2");
    }
    use crate::config::DatabaseKind;

    /// 批量改格是**单事务**：一格失败时先前那格必须回滚（不留下半截改动）。
    #[test]
    fn update_cells_is_transactional() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        // `name` 唯一：第二格故意撞唯一约束。
        conn.conn
            .execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT UNIQUE, note TEXT);
                 INSERT INTO t VALUES (1, 'a', 'x'), (2, 'b', 'y');",
            )
            .expect("建表");

        let pk1: PkValues = vec![("id".to_owned(), "1".to_owned())];
        let pk2: PkValues = vec![("id".to_owned(), "2".to_owned())];

        // 成功路径：两格都改（第一格改 note，第二格把 name 换成不冲突的值）。
        let affected = conn
            .update_cells(
                &TableRef::bare("t"),
                &[
                    CellEdit {
                        pk: pk1.clone(),
                        column: "note".to_owned(),
                        value: Some("first".to_owned()),
                    },
                    CellEdit {
                        pk: pk2.clone(),
                        column: "name".to_owned(),
                        value: Some("b2".to_owned()),
                    },
                ],
            )
            .expect("两格都合法");
        assert_eq!(affected, 2);

        // 失败路径：第一格合法（改 note），第二格把 id=2 的 name 改成 'a' 撞唯一约束。
        let result = conn.update_cells(
            &TableRef::bare("t"),
            &[
                CellEdit {
                    pk: pk1.clone(),
                    column: "note".to_owned(),
                    value: Some("should-roll-back".to_owned()),
                },
                CellEdit {
                    pk: pk2.clone(),
                    column: "name".to_owned(),
                    value: Some("a".to_owned()),
                },
            ],
        );
        assert!(result.is_err(), "撞唯一约束应报错");

        let rows = conn
            .execute("SELECT note FROM t WHERE id = 1", 10)
            .expect("回查")
            .result
            .expect("结果集");
        assert_eq!(
            rows.rows[0][0].render(),
            "first",
            "整批回滚：第一格不该被写进去"
        );
    }

    /// 批量删行是**单事务**：中途一行被外键挡住时整批不落地。
    #[test]
    fn delete_row_set_is_transactional() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        // 外键约束只在显式打开时生效（默认是关的）。
        conn.conn
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE p(id INTEGER PRIMARY KEY);
                 CREATE TABLE c(pid INTEGER REFERENCES p(id));
                 INSERT INTO p VALUES (1), (2);
                 INSERT INTO c VALUES (1);",
            )
            .expect("建表");

        let pk1: PkValues = vec![("id".to_owned(), "1".to_owned())];
        let pk2: PkValues = vec![("id".to_owned(), "2".to_owned())];

        // 第 1 行被 `c` 引用 → 整批失败，第 2 行（本来能删）也必须留下。
        let result = conn.delete_row_set(&TableRef::bare("p"), &[pk1.clone(), pk2.clone()]);
        assert!(result.is_err(), "被外键引用应报错");
        let left = conn
            .execute("SELECT count(*) FROM p", 10)
            .expect("计数")
            .result
            .expect("结果集");
        assert_eq!(left.rows[0][0].render(), "2", "整批回滚：一行都不该被删");

        // 解除引用后同一批就能删干净。
        conn.execute("DELETE FROM c", 10).expect("清引用");
        let affected = conn
            .delete_row_set(&TableRef::bare("p"), &[pk1, pk2])
            .expect("批量删");
        assert_eq!(affected, 2);
    }

    /// CSV 批量插入是单事务：中途一行失败时整批不落地。
    #[test]
    fn insert_rows_is_transactional() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        conn.conn
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT UNIQUE)")
            .expect("建表");
        let columns = vec!["id".to_owned(), "name".to_owned()];
        let rows = vec![
            vec![Some("1".to_owned()), Some("a".to_owned())],
            // 第二行与前一行同名 → 撞唯一约束。
            vec![Some("2".to_owned()), Some("a".to_owned())],
        ];
        assert!(
            conn.insert_rows(&TableRef::bare("t"), &columns, &rows)
                .is_err(),
            "应报错"
        );

        let count = conn
            .execute("SELECT count(*) FROM t", 10)
            .expect("计数")
            .result
            .expect("结果集");
        assert_eq!(count.rows[0][0].render(), "0", "整批回滚：一行都不该留下");
    }

    /// 端到端：建表 → 插入 → 查询 → 值映射 → 截断。
    #[test]
    fn end_to_end_query_and_write() {
        let cfg = ConnectionConfig {
            id: "t1".into(),
            kind: DatabaseKind::Sqlite,
            database: ":memory:".into(),
            ..ConnectionConfig::default()
        };
        let mut conn = SqliteConnection::connect(&cfg).expect("建连");
        conn.ping().expect("ping");

        let outcome = conn
            .execute(
                "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, score REAL); \
                 INSERT INTO users(name, score) VALUES ('ada', 9.5), ('bob', 8.0), ('cat', 7.5);",
                10,
            )
            .expect("建表与插入");
        assert_eq!(outcome.affected, 3);

        let outcome = conn
            .execute("SELECT id, name, score FROM users ORDER BY id", 2)
            .expect("查询");
        let result = outcome.result.expect("读语句必有结果集");
        assert_eq!(result.column_count(), 3);
        assert_eq!(result.columns[1].name, "name");
        assert_eq!(result.row_count(), 2);
        assert!(result.truncated, "3 行取 2 应报截断");
        assert_eq!(result.rows[0][0], Value::Int(1));
        assert_eq!(result.rows[0][1], Value::Text("ada".into()));
        assert_eq!(result.rows[0][2], Value::Float(9.5));

        // 写语句与读语句混排：affected 只累计写语句。
        let outcome = conn
            .execute(
                "UPDATE users SET score = 10 WHERE id = 1; SELECT count(*) FROM users;",
                10,
            )
            .expect("混排");
        assert_eq!(outcome.affected, 1);
        assert!(outcome.result.is_some());
    }

    /// 索引内省与行数口径：主键 / 唯一约束 / 普通索引都要认出来，列序不能乱。
    #[test]
    fn indexes_and_row_count() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        conn.conn
            .execute_batch(
                "CREATE TABLE t(a INTEGER, b TEXT, c TEXT);
                 CREATE UNIQUE INDEX idx_bc ON t(b, c);
                 CREATE INDEX idx_a ON t(a);
                 INSERT INTO t VALUES (1, 'x', 'y'), (2, 'z', 'w'), (3, 'p', 'q');",
            )
            .expect("建表与索引");

        let indexes = conn.table_indexes(&TableRef::bare("t")).expect("索引");
        let by_name = |name: &str| {
            indexes
                .iter()
                .find(|index| index.name == name)
                .unwrap_or_else(|| panic!("缺索引 {name}"))
        };
        // 多列索引的列序按 `seqno`（不是字母序）。
        assert_eq!(by_name("idx_bc").columns, vec!["b", "c"]);
        assert!(by_name("idx_bc").unique);
        assert!(!by_name("idx_bc").primary);
        assert_eq!(by_name("idx_a").columns, vec!["a"]);
        assert!(!by_name("idx_a").unique);
        assert_eq!(
            conn.table_indexes(&TableRef::bare("nope"))
                .expect("没有索引")
                .len(),
            0
        );

        // 非 rowid 主键（`TEXT PRIMARY KEY`）会建 `sqlite_autoindex_*`，
        // `origin = 'pk'` 才能认出来。⚠️ `INTEGER PRIMARY KEY` 就是 rowid 本身，
        // SQLite **不为它建索引**，列表里根本不会有这一行。
        conn.conn
            .execute_batch("CREATE TABLE k(id TEXT PRIMARY KEY, v TEXT UNIQUE)")
            .expect("带主键的表");
        let keyed = conn.table_indexes(&TableRef::bare("k")).expect("索引");
        assert!(
            keyed.iter().any(|index| index.primary),
            "主键索引应被标为 primary: {keyed:?}"
        );
        assert!(
            keyed.iter().any(|index| index.unique && !index.primary),
            "UNIQUE 约束的隐式索引应被标为 unique: {keyed:?}"
        );

        // 行数：SQLite 只能精确数（`COUNT(*)`）。
        assert_eq!(
            conn.table_row_count(&TableRef::bare("t")).expect("行数"),
            RowCount::Exact(3)
        );
    }

    /// 目录与列内省 + CSV。
    #[test]
    fn catalog_columns_and_csv() {
        let cfg = ConnectionConfig {
            id: "t2".into(),
            kind: DatabaseKind::Sqlite,
            database: ":memory:".into(),
            ..ConnectionConfig::default()
        };
        let mut conn = SqliteConnection::connect(&cfg).expect("建连");
        conn.execute(
            "CREATE TABLE items(id INTEGER, 名字 TEXT); CREATE VIEW v AS SELECT 1;",
            10,
        )
        .expect("建对象");
        let catalog = conn.catalog(None).expect("目录");
        assert_eq!(catalog.len(), 2);
        assert!(
            catalog
                .iter()
                .any(|t| t.name == "items" && t.kind == TableKind::Table)
        );
        assert!(
            catalog
                .iter()
                .any(|t| t.name == "v" && t.kind == TableKind::View)
        );
        assert_eq!(catalog[0].schema.as_deref(), Some("main"));

        let columns = conn.table_columns(&TableRef::bare("items")).expect("列");
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[1].name, "名字");

        let result = conn
            .execute("SELECT id, 名字 FROM items", 10)
            .expect("查询")
            .result
            .expect("结果集");
        let csv = result.to_csv();
        assert!(csv.starts_with("id,名字\n"));
    }

    /// 暂存式提交：改格 + 新增行**一个事务**；任一步失败则两边都不落地。
    #[test]
    fn apply_batch_is_one_transaction() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        conn.conn
            .execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT UNIQUE, note TEXT);
                 INSERT INTO t VALUES (1, 'a', 'x');",
            )
            .expect("建表");
        let table = TableRef::bare("t");
        let pk: PkValues = vec![("id".to_owned(), "1".to_owned())];
        let edit = CellEdit {
            pk: pk.clone(),
            column: "note".to_owned(),
            value: Some("改过".to_owned()),
        };
        let insert = |name: &str| super::super::InsertRow {
            columns: vec!["id".to_owned(), "name".to_owned()],
            values: vec![Some("2".to_owned()), Some(name.to_owned())],
        };

        // 成功路径：改动 + 新增都落地。
        let outcome = conn
            .apply_batch(&table, std::slice::from_ref(&edit), &[insert("b")])
            .expect("提交");
        assert_eq!(outcome.updated, 1);
        assert_eq!(outcome.inserted, 1);
        assert_eq!(outcome.total(), 2);

        // 失败路径：新增行撞唯一约束 → 连那一格改动也回滚。
        let result = conn.apply_batch(
            &table,
            &[CellEdit {
                pk: pk.clone(),
                column: "note".to_owned(),
                value: Some("不该留下".to_owned()),
            }],
            &[insert("b")],
        );
        assert!(result.is_err(), "撞唯一约束应报错");
        let rows = conn
            .execute("SELECT note FROM t WHERE id = 1", 10)
            .expect("回查")
            .result
            .expect("结果集");
        assert_eq!(rows.rows[0][0].render(), "改过", "改动应随插入失败一起回滚");

        // 空批次：不白开事务，也不报错。
        let outcome = conn.apply_batch(&table, &[], &[]).expect("空批次");
        assert_eq!(outcome.total(), 0);
    }

    /// 连接级只读档：`PRAGMA query_only` 由**服务器**拒绝写（不是界面软拦）。
    #[test]
    fn read_only_profile_blocks_writes() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        conn.conn
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT)")
            .expect("建表");
        conn.execute("INSERT INTO t VALUES (1, 'x')", 10)
            .expect("写前可写");

        conn.set_read_only(true).expect("切只读");
        assert!(
            conn.execute("INSERT INTO t VALUES (2, 'y')", 10).is_err(),
            "只读档下写入必须被引擎拒"
        );
        // 读照常。
        assert!(conn.execute("SELECT * FROM t", 10).is_ok());

        conn.set_read_only(false).expect("切回可写");
        conn.execute("INSERT INTO t VALUES (3, 'z')", 10)
            .expect("切回后可写");
    }

    /// 分页稳定序：不给 `ORDER BY` 时 SQLite 按**物理顺序**（rowid）扫，
    /// 与主键序不一致时翻页就会「同一行出现两次、另一行永远看不到」。
    ///
    /// 这里故意让插入顺序与主键序相反，用来分辨有没有真的加上稳定序。
    #[test]
    fn paging_is_stable_only_with_an_order_by() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        // 插入序 c / a / b —— 与主键序 a / b / c 刻意不同。
        conn.conn
            .execute_batch(
                "CREATE TABLE t(id TEXT PRIMARY KEY, v TEXT);
                 INSERT INTO t VALUES ('c', '3'), ('a', '1'), ('b', '2');",
            )
            .expect("建表");

        let table = TableRef::bare("t");
        let pk = conn.table_primary_key(&table).expect("主键");
        assert_eq!(pk, vec!["id".to_owned()]);
        let order: Vec<OrderBy> = pk.iter().map(OrderBy::asc).collect();

        let page0 = conn.table_page(&table, 2, 0, &order, &[]).expect("第 1 页");
        let page1 = conn.table_page(&table, 2, 2, &order, &[]).expect("第 2 页");
        let ids = |result: &ResultSet| -> Vec<String> {
            result
                .rows
                .iter()
                .map(|row| row[0].render().to_string())
                .collect()
        };
        assert_eq!(ids(&page0), vec!["a", "b"], "第 1 页必须按主键序");
        assert_eq!(ids(&page1), vec!["c"], "第 2 页不能重复第 1 页的行");

        // 反证：不给稳定序时物理顺序是插入序（c / a / b），与上面不同。
        let raw = conn.table_page(&table, 2, 0, &[], &[]).expect("无稳定序");
        assert_eq!(
            ids(&raw),
            vec!["c", "a"],
            "无 ORDER BY 时按物理顺序——这正是旧实现的翻页乱序"
        );
    }

    /// 表浏览过滤：算符、`IS NULL`、`LIKE` 都生效；值走绑定参数
    /// （含引号的注入串只当普通文本，绝不进 SQL 文本）。
    #[test]
    fn table_page_filters_are_parameterised() {
        use crate::drivers::{FilterOp, TableFilter};
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        conn.conn
            .execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, score INTEGER, note TEXT);
                 INSERT INTO t VALUES (1, 'ada', 9, 'x'), (2, 'bob', 5, NULL), (3, 'cat', 7, 'z');",
            )
            .expect("建表");
        let table = TableRef::bare("t");
        let order = [OrderBy::asc("id")];
        let mut ids = |filters: &[TableFilter]| -> Vec<String> {
            conn.table_page(&table, 10, 0, &order, filters)
                .expect("过滤分页")
                .rows
                .iter()
                .map(|row| row[0].render().to_string())
                .collect()
        };

        // 等值 / 比较 / 范围。
        assert_eq!(ids(&[TableFilter::new("score", FilterOp::Eq, "5")]), ["2"]);
        assert_eq!(
            ids(&[TableFilter::new("score", FilterOp::Gte, "7")]),
            ["1", "3"]
        );
        assert_eq!(ids(&[TableFilter::new("score", FilterOp::Lt, "6")]), ["2"]);
        assert_eq!(
            ids(&[TableFilter::new("name", FilterOp::NotEq, "ada")]),
            ["2", "3"]
        );
        // 多条件是 AND。
        assert_eq!(
            ids(&[
                TableFilter::new("score", FilterOp::Gte, "7"),
                TableFilter::new("name", FilterOp::Like, "c%"),
            ]),
            ["3"]
        );
        // NULL 判定不占位：`IS NULL` 命中 NULL 行，`IS NOT NULL` 命中其余行。
        assert_eq!(ids(&[TableFilter::null("note", FilterOp::IsNull)]), ["2"]);
        assert_eq!(
            ids(&[TableFilter::null("note", FilterOp::IsNotNull)]),
            ["1", "3"]
        );
        // 值里的引号 / 分号只是文本，不改变语义（绑定参数）。
        assert!(
            ids(&[TableFilter::new(
                "name",
                FilterOp::Eq,
                "x'; DROP TABLE t; --"
            )])
            .is_empty()
        );
        assert_eq!(ids(&[]).len(), 3, "空过滤 = 全表这一页");
        // 过滤后翻页仍按稳定序：第 2 页从第 2 条命中开始。
        let second = conn
            .table_page(
                &table,
                1,
                1,
                &order,
                &[TableFilter::new("score", FilterOp::Gte, "7")],
            )
            .expect("过滤翻页");
        assert_eq!(second.rows[0][0].render(), "3");
    }

    /// 查询结果分页（[`SqliteConnection::execute_paged`]）：外层 `LIMIT/OFFSET` 包装、
    /// 多取一行探测「还有下一页」、多语句一律拒绝。
    #[test]
    fn execute_paged_walks_the_result_in_windows() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        conn.conn
            .execute_batch(
                "CREATE TABLE t(id INTEGER);
                 INSERT INTO t VALUES (1), (2), (3), (4), (5);",
            )
            .expect("建表");
        let sql = "SELECT id FROM t ORDER BY id";
        let mut page = |offset: u64, limit: usize| -> (Vec<String>, bool) {
            let result = conn.execute_paged(sql, offset, limit).expect("分页");
            let ids = result
                .rows
                .iter()
                .map(|row| row[0].render().to_string())
                .collect();
            (ids, result.truncated)
        };

        assert_eq!(page(0, 2), (vec!["1".to_owned(), "2".to_owned()], true));
        assert_eq!(page(2, 2), (vec!["3".to_owned(), "4".to_owned()], true));
        // 最后一页：取到末行且后面没有了 → 不能再续拉。
        assert_eq!(page(4, 2), (vec!["5".to_owned()], false));
        // 越过末页：空结果、无更多。
        assert_eq!(page(10, 2), (Vec::new(), false));

        // 只接受单条语句：多语句批次没有「第 N 行起」的语义。
        assert!(
            conn.execute_paged("SELECT 1; SELECT 2", 0, 10).is_err(),
            "多语句必须拒绝"
        );
    }

    /// 只读执行（[`crate::drivers::Connection::execute_read_only`]）：写语句一条都不跑，
    /// 只读语句走的是与手动执行**同一条**路径。
    #[test]
    fn read_only_execution_refuses_writes_outright() {
        let mut wrapper = crate::drivers::Connection::Sqlite(SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        });
        // 建表是 DDL：证明不了只读 → 拒绝（连"试试看"都不给）。
        assert!(
            wrapper
                .execute_read_only("CREATE TABLE t(a INTEGER)", 10)
                .is_err()
        );
        // 走普通执行建表并塞两行（这条路径不受只读闸门约束）。
        wrapper
            .execute(
                "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2);",
                10,
            )
            .expect("建表");

        let read = wrapper
            .execute_read_only("SELECT a FROM t ORDER BY a", 10)
            .expect("只读语句放行");
        assert_eq!(read.result.expect("结果集").row_count(), 2);

        // 写语句被拒，且**真的没执行**（再看一眼行数）。
        assert!(wrapper.execute_read_only("DELETE FROM t", 10).is_err());
        let after = wrapper
            .execute_read_only("SELECT count(*) FROM t", 10)
            .expect("只读语句放行");
        assert_eq!(after.result.expect("结果集").rows[0][0].render(), "2");

        // 混合批次整体拒绝：不做「能跑的跑、不能跑的跳过」。
        assert!(
            wrapper
                .execute_read_only("SELECT 1; DROP TABLE t", 10)
                .is_err()
        );
        // 空串 / 只有注释：一律拒绝，不静默返回空结果。
        assert!(wrapper.execute_read_only("   ", 10).is_err());
        assert!(wrapper.execute_read_only("-- 只有注释", 10).is_err());
    }

    /// 附加库（`ATTACH`）里的对象要能通过限定名访问：目录 / 列 / 主键 / 分页 / 行数
    /// 都得认 schema 那一段。
    #[test]
    fn attached_schema_is_addressable() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        conn.conn
            .execute_batch(
                "CREATE TABLE main_t(a INTEGER PRIMARY KEY, b TEXT);
                 ATTACH ':memory:' AS aux;
                 CREATE TABLE aux.q(x INTEGER PRIMARY KEY, y TEXT);
                 INSERT INTO aux.q VALUES (1, 'one'), (2, 'two');",
            )
            .expect("建表并附加");

        let schemas = conn.list_schemas().expect("命名空间");
        assert!(schemas.contains(&"main".to_owned()), "{schemas:?}");
        assert!(schemas.contains(&"aux".to_owned()), "{schemas:?}");

        // 默认（`None`）只列 `main`。
        let main_only = conn.catalog(None).expect("main 目录");
        assert!(main_only.iter().all(|t| t.name == "main_t"));
        // 指定 aux 才看得到附加库里的表，且带 schema 标。
        let aux = conn.catalog(Some("aux")).expect("aux 目录");
        assert_eq!(aux.len(), 1);
        assert_eq!(aux[0].name, "q");
        assert_eq!(aux[0].schema.as_deref(), Some("aux"));

        let qualified = TableRef::in_schema("aux", "q");
        let columns = conn.table_columns(&qualified).expect("列");
        assert_eq!(
            columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["x", "y"]
        );
        assert_eq!(
            conn.table_primary_key(&qualified).expect("主键"),
            vec!["x".to_owned()]
        );
        let page = conn
            .table_page(&qualified, 10, 0, &[OrderBy::asc("x")], &[])
            .expect("分页");
        assert_eq!(page.row_count(), 2);
        assert_eq!(
            conn.table_row_count(&qualified).expect("行数"),
            RowCount::Exact(2)
        );

        // 限定名写进 SQL 时要逐段加引号（`"aux"."q"`，不是 `"aux.q"`）。
        assert_eq!(
            quote_qualified(&qualified, '"').expect("合法"),
            "\"aux\".\"q\""
        );
    }

    /// 外键内省：单列 + 复合外键各一条，验证分组与动作列。
    #[test]
    fn foreign_keys_roundtrip() {
        let conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        let mut conn = conn;
        conn.conn
            .execute_batch(
                "CREATE TABLE parent(id INTEGER PRIMARY KEY, a TEXT, b TEXT, UNIQUE(a, b));
                 CREATE TABLE child(
                   id INTEGER PRIMARY KEY,
                   p_id INTEGER REFERENCES parent(id) ON DELETE SET NULL,
                   pa TEXT, pb TEXT,
                   FOREIGN KEY(pa, pb) REFERENCES parent(a, b) ON UPDATE CASCADE
                 );",
            )
            .expect("建表");
        let mut keys = conn
            .table_foreign_keys(&TableRef::bare("child"))
            .expect("外键");
        assert_eq!(keys.len(), 2);
        // pragma 的约束顺序未指定，按内容找两条断言。
        keys.sort_by_key(|fk| fk.columns.len());
        let single = &keys[0];
        let composite = &keys[1];
        assert_eq!(single.name, ""); // SQLite 没有约束名
        assert_eq!(single.ref_table, "parent");
        assert_eq!(single.columns, vec!["p_id"]);
        assert_eq!(single.ref_columns, vec!["id"]);
        assert_eq!(single.on_delete, "SET NULL");
        assert_eq!(composite.columns, vec!["pa", "pb"]);
        assert_eq!(composite.ref_columns, vec!["a", "b"]);
        assert_eq!(composite.on_update, "CASCADE");
    }

    /// 触发器内省：时机 / 事件从建表 SQL 头部解析。
    #[test]
    fn triggers_roundtrip() {
        let conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        let mut conn = conn;
        conn.conn
            .execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT);
                 CREATE VIEW v AS SELECT id, n FROM t;
                 CREATE TRIGGER t_after_ins AFTER INSERT ON t
                 BEGIN SELECT 1; END;
                 CREATE TRIGGER t_before_upd BEFORE UPDATE OF n ON t
                 BEGIN SELECT 2; END;
                 CREATE TRIGGER v_instead_del INSTEAD OF DELETE ON v
                 BEGIN SELECT 3; END;",
            )
            .expect("建表");
        let triggers = conn.table_triggers(&TableRef::bare("t")).expect("触发器");
        // `INSTEAD OF` 只能挂在视图上，不在 t 的清单里。
        assert_eq!(triggers.len(), 2);
        assert_eq!(triggers[0].name, "t_after_ins");
        assert_eq!(triggers[0].timing, "AFTER");
        assert_eq!(triggers[0].event, "INSERT");
        assert_eq!(triggers[1].timing, "BEFORE");
        assert_eq!(triggers[1].event, "UPDATE");
        // 视图的触发器同样按表名取得到。
        let view_triggers = conn.table_triggers(&TableRef::bare("v")).expect("触发器");
        assert_eq!(view_triggers[0].timing, "INSTEAD OF");
        assert_eq!(view_triggers[0].event, "DELETE");
    }

    /// 库级触发器清单：不筛表名，说明列 = 挂在哪张表上（含视图上的）。
    #[test]
    fn database_trigger_list() {
        let conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        let mut conn = conn;
        conn.conn
            .execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY);
                 CREATE VIEW v AS SELECT id FROM t;
                 CREATE TRIGGER t_after_ins AFTER INSERT ON t BEGIN SELECT 1; END;
                 CREATE TRIGGER v_instead_del INSTEAD OF DELETE ON v BEGIN SELECT 2; END;",
            )
            .expect("建表");
        let objects = conn.list_triggers(None).expect("触发器清单");
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].name, "t_after_ins");
        assert_eq!(objects[0].detail.as_deref(), Some("t"));
        assert_eq!(objects[1].name, "v_instead_del");
        assert_eq!(objects[1].detail.as_deref(), Some("v"));
    }

    /// 头部解析纯函数：整词匹配、正文里的关键字不干扰。
    #[test]
    fn trigger_head_parse() {
        assert_eq!(
            trigger_timing_event("CREATE TRIGGER x BEFORE INSERT ON t BEGIN SELECT 'on x'; END;"),
            ("BEFORE".into(), "INSERT".into())
        );
        // 时机在名字之后也要找到（`before_x` 里 `before` 后跟 `_` 不算整词）。
        assert_eq!(
            trigger_timing_event("CREATE TRIGGER before_x AFTER DELETE ON t BEGIN END;"),
            ("AFTER".into(), "DELETE".into())
        );
        // `UPDATE OF 列` 归为 UPDATE。
        assert_eq!(
            trigger_timing_event("create trigger a after update of n on t begin end;"),
            ("AFTER".into(), "UPDATE".into())
        );
        // 不是建触发器语句 → 落空串。
        assert_eq!(
            trigger_timing_event("SELECT 1"),
            (String::new(), String::new())
        );
    }

    /// 实例级内省：概览取到版本与页级体量，指标/检查器两张表的口径各就各位。
    #[test]
    fn instance_insight() {
        let mut conn = SqliteConnection {
            conn: Connection::open_in_memory().expect("内存库"),
        };
        conn.conn
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);")
            .expect("建表");

        let overview = conn.instance_overview().expect("实例概览");
        assert!(overview.version.contains("SQLite"), "{}", overview.version);
        // SQLite 没有服务端：这两项必须是「空」而不是编造的值。
        assert_eq!(overview.uptime, None);
        assert!(overview.user.is_empty());
        assert!(
            overview.facts.iter().any(|fact| fact.name == "page_size"),
            "缺 page_size：{:?}",
            overview.facts
        );
        // 内存库的 `main` 没有文件路径 → `database` 为空串（不是伪路径）。
        assert!(overview.database.is_empty());
        assert_eq!(
            overview
                .facts
                .iter()
                .find(|fact| fact.name == "attached_databases")
                .map(|fact| fact.value.as_str()),
            Some("1") // 内存库只有 `main`
        );

        let metrics = conn.instance_table(InstanceView::Metrics).expect("指标");
        assert_eq!(
            metrics.columns,
            vec!["metric".to_owned(), "value".to_owned()]
        );
        assert!(
            metrics
                .rows
                .iter()
                .any(|row| row[0] == "database_size" && !row[1].is_empty())
        );

        let processes = conn
            .instance_table(InstanceView::Processes)
            .expect("检查器");
        assert_eq!(processes.columns.len(), 2);
        assert_eq!(processes.rows.len(), 1);
        assert_eq!(processes.rows[0][0], "main");
    }
}
