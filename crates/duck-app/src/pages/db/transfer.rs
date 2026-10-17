//! 数据传输弹窗：源库的表批量搬到目标连接 + 库（dbx `DataTransferDialog` 对位）。
//!
//! # 为什么是弹窗而不是标签
//!
//! dbx 的数据传输是一张可拖动的大弹窗：左侧「传输任务」栏（保存的规格，可复用），
//! 主区自上而下 = 源⇄目标两列（带交换按钮）→ 传输对象多选 → 传输内容三档 →
//! 目标表处理方式 / 目标表名大小写 / 批量大小，底部「取消 / 保存为任务 / 开始传输」。
//! 本模块照此实现，任务存 kv 单键 JSON（`db_transfer_tasks`）。
//!
//! # 执行内核
//!
//! - **数据**：源表 `SELECT *`（行数有上限）读成展示文本，目标侧 `apply_batch`
//!   按「批量大小」分块落库。列按**同名交集**搬运：目标多出的列落默认值，
//!   目标缺的列被丢弃并在结果里注明。
//! - **结构**：目标缺表（或选了「重建」）时按源侧列元信息现场拼 `CREATE TABLE`
//!   （列 / 类型 / 可空 / 默认值 / 主键 + 注释；**不含索引与外键**，界面上如实注明）。
//!   只支持五家 SQL 引擎当目标；「重建」先 `DROP TABLE`。
//! - **覆盖**：先 `DELETE FROM` 目标表再插（通用写法，各 SQL 引擎都认）。
//!
//! ⚠️ **弹窗正文闭包只跑一次**：传输进度与全部表单状态都放在
//! [`TransferDialogView`] 这个独立实体里，状态变化走 `notify()` 重绘。

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use duck_db::TableColumnMeta;

/// 单表传输的行数上限（与导出同一口径：超出的部分不是不能传，是不该悄悄全传）。
const TRANSFER_ROW_CAP: usize = 100_000;

/// 弹窗宽度（dbx 是 `min(1120px, 100vw-2rem)`：左侧任务栏 + 源⇄目标两列并排）。
const TRANSFER_DIALOG_WIDTH: f32 = 1080.;

/// 传输对象清单的最大高度（弹窗内滚动区的上限；布局约束，不在 spacing 阶梯上）。
const TRANSFER_OBJECTS_MAX_HEIGHT: f32 = 260.;

/// 左侧「传输任务」栏宽（dbx 同款 `w-60`）。
const TRANSFER_TASKS_WIDTH: f32 = 240.;

/// 传输任务的 kv 键（单键 JSON：任务数组，范式同比较配置）。
const KEY_TRANSFER_TASKS: &str = "db_transfer_tasks";

/// 批量大小的合法区间（dbx 同款 `min=100 max=10000`）。
const BATCH_SIZE_MIN: usize = 100;
const BATCH_SIZE_MAX: usize = 10_000;
const BATCH_SIZE_DEFAULT: usize = 1_000;

/// 传输什么：结构 + 数据 / 仅结构 / 仅数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TransferContent {
    /// 建表（目标缺才建）+ 复制数据。
    #[default]
    StructureAndData,
    /// 只创建表和对象结构，不复制数据。
    StructureOnly,
    /// 只复制数据到已存在的目标表。
    DataOnly,
}

impl TransferContent {
    /// 单选行上的文案与说明。
    fn label(self, cx: &App) -> (&'static str, &'static str) {
        match self {
            Self::StructureAndData => {
                (tr(cx, "结构和数据"), tr(cx, "创建表和对象结构，并复制数据"))
            }
            Self::StructureOnly => (tr(cx, "仅结构"), tr(cx, "仅创建表和对象结构，不复制数据")),
            Self::DataOnly => (tr(cx, "仅数据"), tr(cx, "仅复制数据到已存在的目标表")),
        }
    }

    fn index(self) -> usize {
        match self {
            Self::StructureAndData => 0,
            Self::StructureOnly => 1,
            Self::DataOnly => 2,
        }
    }

    fn all() -> [Self; 3] {
        [Self::StructureAndData, Self::StructureOnly, Self::DataOnly]
    }
}

/// 目标表处理方式：追加 / 覆盖 / 重建。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TransferStrategy {
    /// 追加（INSERT）：目标有数据就再插一份。
    #[default]
    Append,
    /// 覆盖：先清空目标表再插。
    Overwrite,
    /// 重建：删掉目标表按源结构重建（**危险**，界面上要如实说明）。
    Rebuild,
}

impl TransferStrategy {
    /// 下拉项文案（仅结构时「追加」显示为「保留现有表」——那一档只建缺失的表，
    /// 没有「往里插」的语义，dbx 同一口径）。
    fn label(self, structure_only: bool, cx: &App) -> &'static str {
        match self {
            Self::Append if structure_only => tr(cx, "保留现有表"),
            Self::Append => tr(cx, "追加 (INSERT)"),
            Self::Overwrite => tr(cx, "覆盖 (清空后插入)"),
            Self::Rebuild if structure_only => tr(cx, "重建 (删掉重建结构)"),
            Self::Rebuild => tr(cx, "重建 (删掉重建)"),
        }
    }

    /// 下拉项的值（`Debug` 名；`on_select` 回来按它精确匹配）。
    fn value(self) -> String {
        format!("{self:?}")
    }

    /// 这一档在当前内容档位下可不可选（不可选就从下拉里消失并回落「追加」）。
    fn available(self, content: TransferContent, target_kind: Option<DatabaseKind>) -> bool {
        match self {
            Self::Rebuild => {
                content != TransferContent::DataOnly
                    && target_kind.is_some_and(can_create_structure)
            }
            Self::Overwrite | Self::Append => true,
        }
    }
}

/// 目标表名大小写：保持原样 / 转小写 / 转大写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TableNameCase {
    #[default]
    Preserve,
    Lower,
    Upper,
}

impl TableNameCase {
    fn label(self, cx: &App) -> &'static str {
        match self {
            Self::Preserve => tr(cx, "保持原样"),
            Self::Lower => tr(cx, "转小写"),
            Self::Upper => tr(cx, "转大写"),
        }
    }

    fn value(self) -> String {
        format!("{self:?}")
    }

    fn apply(self, name: &str) -> String {
        match self {
            Self::Preserve => name.to_owned(),
            Self::Lower => name.to_lowercase(),
            Self::Upper => name.to_uppercase(),
        }
    }
}

/// 目标引擎能不能**执行**建表 DDL（结构搬运的硬前提）。
fn can_create_structure(kind: DatabaseKind) -> bool {
    matches!(
        kind,
        DatabaseKind::Mysql
            | DatabaseKind::Postgres
            | DatabaseKind::SqlServer
            | DatabaseKind::Sqlite
            | DatabaseKind::ClickHouse
    )
}

/// 一份任务规格（保存 / 加载的载体；字段一律 `Option` + `serde(default)`，
/// 旧版本写下的任务少了任何一栏都能读回来）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TransferTaskConfig {
    #[serde(default)]
    source_conn: Option<String>,
    #[serde(default)]
    source_ns: Option<String>,
    #[serde(default)]
    target_conn: Option<String>,
    #[serde(default)]
    target_ns: Option<String>,
    #[serde(default)]
    tables: Vec<String>,
    #[serde(default)]
    content: Option<TransferContent>,
    #[serde(default)]
    strategy: Option<TransferStrategy>,
    #[serde(default)]
    name_case: Option<TableNameCase>,
    #[serde(default)]
    batch_size: Option<usize>,
}

/// 一条保存的传输任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransferTask {
    /// 稳定 id（`task-<毫秒>`；定位 / 删除走它，名字可重复）。
    id: String,
    /// 任务名（侧栏显示；默认「传输任务 N」自增）。
    name: String,
    /// 规格。
    config: TransferTaskConfig,
}

/// 读回全部保存的任务（坏 JSON 当空清单：一份坏档不该把整个弹窗卡死）。
fn load_tasks(cx: &App) -> Vec<TransferTask> {
    duck_store::kv_get(cx, KEY_TRANSFER_TASKS)
        .and_then(|raw| serde_json::from_str::<Vec<TransferTask>>(&raw).ok())
        .unwrap_or_default()
}

/// 写回全部任务并立刻落盘（紧接着的读路径必须看到这次写入）。
fn save_tasks(tasks: &[TransferTask], window: &mut Window, cx: &mut App) {
    match serde_json::to_string(tasks) {
        Ok(json) => {
            duck_store::kv_put(cx, KEY_TRANSFER_TASKS, json);
            duck_store::flush(cx);
        }
        Err(error) => notify::error(
            trf(cx, "保存传输任务失败：{}", &[&error.to_string()]),
            window,
            cx,
        ),
    }
}

/// 下一个空闲的默认任务名（「传输任务 N」自增，避免一排同名）。
fn next_task_name(tasks: &[TransferTask], cx: &App) -> String {
    let mut index = tasks.len() + 1;
    loop {
        let candidate = trf(cx, "传输任务 {}", &[&index.to_string()]);
        if !tasks.iter().any(|task| task.name == candidate) {
            return candidate;
        }
        index += 1;
    }
}

/// 一张表的传输结论。
#[derive(Debug, Clone)]
pub(super) enum TransferRowStatus {
    /// 还没轮到。
    Pending,
    /// 正在传。
    Working,
    /// 传完：是否建了表 / 实际插入行数 / 读取行数 / 被丢弃列数。
    Done {
        created: bool,
        inserted: u64,
        read: usize,
        dropped: usize,
    },
    /// 跳过（目标表不存在等：不是错误，是前置条件不满足）。
    Skipped(String),
    /// 失败（引擎报错）。
    Failed(String),
}

/// 传输清单里的一行。
#[derive(Debug, Clone)]
pub(super) struct TransferRow {
    /// 目标侧表名（按大小写档转换后的名字）。
    pub target_name: String,
    /// 结论。
    pub status: TransferRowStatus,
}

/// 一次传输任务的后台参数（逐表串行执行共用的一份快照）。
///
/// 跳过原因在主线程译好带下去：后台线程拿不到 `&App`，取不了词。
#[derive(Clone)]
struct TransferJob {
    source_conn: Arc<Mutex<Connection>>,
    target_conn: Arc<Mutex<Connection>>,
    source_ns: String,
    target_ns: String,
    source_kind: DatabaseKind,
    target_kind: DatabaseKind,
    content: TransferContent,
    strategy: TransferStrategy,
    batch_size: usize,
    /// 「目标表不存在」的译文。
    skipped_missing: String,
    /// 「目标引擎不支持建表」的译文。
    skipped_no_create: String,
}

/// 单表传输的后台产物。
struct TransferOutcome {
    created: bool,
    inserted: u64,
    rows_read: usize,
    dropped_columns: usize,
}

/// 单表传输的失败：跳过（前置条件不满足）或引擎错误。
enum TransferFailure {
    Skipped(String),
    Error(DbError),
}

impl From<DbError> for TransferFailure {
    fn from(error: DbError) -> Self {
        Self::Error(error)
    }
}

/// 一张表的单轮执行：建结构（按需）→ 清空（按需）→ 搬数据（按需）。
fn run_table(
    job: &TransferJob,
    source_name: &str,
    target_name: &str,
) -> Result<TransferOutcome, TransferFailure> {
    let source_ref = TableRef {
        schema: Some(job.source_ns.clone()),
        name: source_name.to_owned(),
    };
    let target_ref = TableRef {
        schema: Some(job.target_ns.clone()),
        name: target_name.to_owned(),
    };

    // 结构阶段：仅结构 / 结构和数据，或显式「重建」。
    let mut created = false;
    if job.content != TransferContent::DataOnly {
        let target_exists = {
            let mut guard = job.target_conn.lock().expect("连接锁不应中毒");
            guard.table_columns(&target_ref).is_ok()
        };
        if job.strategy == TransferStrategy::Rebuild {
            if !can_create_structure(job.target_kind) {
                return Err(TransferFailure::Skipped(job.skipped_no_create.clone()));
            }
            {
                let mut guard = job.target_conn.lock().expect("连接锁不应中毒");
                let drop_sql = drop_table_sql(job.target_kind, &target_ref)?;
                guard.execute(&drop_sql, 0)?;
            }
            create_table(job, &source_ref, &target_ref)?;
            created = true;
        } else if !target_exists {
            if !can_create_structure(job.target_kind) {
                return Err(TransferFailure::Skipped(format!(
                    "{}（{}）",
                    job.skipped_missing, job.skipped_no_create
                )));
            }
            create_table(job, &source_ref, &target_ref)?;
            created = true;
        }
    }
    if job.content == TransferContent::StructureOnly {
        return Ok(TransferOutcome {
            created,
            inserted: 0,
            rows_read: 0,
            dropped_columns: 0,
        });
    }

    // 覆盖：先清空（通用 `DELETE FROM`，各 SQL 引擎都认）。
    if job.strategy == TransferStrategy::Overwrite {
        let mut guard = job.target_conn.lock().expect("连接锁不应中毒");
        let quote = guard.identifier_quote().unwrap_or('"');
        let qualified = quote_qualified(&target_ref, quote)?;
        guard.execute(&format!("DELETE FROM {qualified}"), 0)?;
    }

    // 数据阶段：读源 → 同名交集 → 分块落库。
    let (columns, rows) = {
        let mut guard = job.source_conn.lock().expect("连接锁不应中毒");
        let quote = guard.identifier_quote().unwrap_or('"');
        let qualified = quote_qualified(&source_ref, quote)?;
        let columns = guard.table_columns(&source_ref)?;
        let outcome = guard.execute(&format!("SELECT * FROM {qualified}"), TRANSFER_ROW_CAP)?;
        let rows = outcome.result.unwrap_or_default().rows;
        (columns, rows)
    }; // 源锁在这里释放：写目标时不再持有它，两个方向的传输不会互锁。
    let mut guard = job.target_conn.lock().expect("连接锁不应中毒");
    let target_columns = guard
        .table_columns(&target_ref)
        .map_err(|_| TransferFailure::Skipped(job.skipped_missing.clone()))?;
    let common: Vec<String> = columns
        .iter()
        .map(|column| column.name.clone())
        .filter(|name| target_columns.iter().any(|column| column.name == *name))
        .collect();
    let dropped = columns.len() - common.len();
    let indexes: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| common.contains(&column.name))
        .map(|(index, _)| index)
        .collect();
    let mut inserted = 0u64;
    for chunk_start in (0..rows.len()).step_by(job.batch_size) {
        let chunk = &rows[chunk_start..(chunk_start + job.batch_size).min(rows.len())];
        let inserts: Vec<InsertRow> = chunk
            .iter()
            .map(|row| InsertRow {
                columns: common.clone(),
                values: indexes
                    .iter()
                    .map(|index| match &row[*index] {
                        duck_db::Value::Null => None,
                        value => Some(value.render()),
                    })
                    .collect(),
            })
            .collect();
        if inserts.is_empty() {
            break;
        }
        let outcome = guard
            .apply_batch(&target_ref, &[], &inserts)
            .map_err(TransferFailure::Error)?;
        inserted += outcome.inserted;
    }
    Ok(TransferOutcome {
        created,
        inserted,
        rows_read: rows.len(),
        dropped_columns: dropped,
    })
}

/// 在目标库上按源侧列元信息建一张表。
fn create_table(
    job: &TransferJob,
    source_ref: &TableRef,
    target_ref: &TableRef,
) -> Result<(), TransferFailure> {
    let (columns, pk, quote) = {
        let mut guard = job.source_conn.lock().expect("连接锁不应中毒");
        let columns = guard.table_column_meta(source_ref)?;
        let pk = guard.table_primary_key(source_ref).unwrap_or_default();
        let quote = guard.identifier_quote().unwrap_or('"');
        (columns, pk, quote)
    };
    let sql = create_table_sql(
        job.target_kind,
        job.source_kind,
        target_ref,
        &columns,
        &pk,
        quote,
    )?;
    let mut guard = job.target_conn.lock().expect("连接锁不应中毒");
    guard.execute(&sql, 0)?;
    Ok(())
}

/// 一条目标侧的 `DROP TABLE`（SQL Server 没有全引擎同款的 `IF EXISTS`）。
fn drop_table_sql(kind: DatabaseKind, table: &TableRef) -> Result<String, DbError> {
    // 引号字符取不到连接时退回引擎默认：MySQL / ClickHouse 用反引号，其余双引号。
    let quote = match kind {
        DatabaseKind::Mysql | DatabaseKind::ClickHouse => '`',
        _ => '"',
    };
    let qualified = quote_qualified(table, quote)?;
    Ok(match kind {
        DatabaseKind::SqlServer => format!(
            "IF OBJECT_ID(N'{}', N'U') IS NOT NULL DROP TABLE {qualified};",
            table.display()
        ),
        _ => format!("DROP TABLE IF EXISTS {qualified};"),
    })
}

/// 按源侧列元信息拼一条跨引擎的 `CREATE TABLE`。
///
/// 类型名**照搬源侧**：同族引擎（MySQL→MySQL、SQLite→SQLite）无损，跨引擎时
/// 常用类型（整数 / 定长变长文本 / 时间戳）大体兼容，不兼容的会在执行时报错并
/// 落到该表的结果行里 —— 比界面层猜一套类型映射更诚实。
fn create_table_sql(
    target_kind: DatabaseKind,
    source_kind: DatabaseKind,
    table: &TableRef,
    columns: &[TableColumnMeta],
    pk: &[String],
    quote: char,
) -> Result<String, DbError> {
    let qualified = quote_qualified(table, quote)?;
    let mut lines: Vec<String> = Vec::new();
    for column in columns {
        let Ok(quoted) = duck_db::drivers::quote_identifier(&column.name, quote) else {
            continue;
        };
        let type_name = column.type_name.trim();
        if type_name.is_empty() {
            continue;
        }
        let mut definition = format!("{quoted} {type_name}");
        // ClickHouse 的可空是 `Nullable(T)` 包在类型里，没有 `NOT NULL` 子句。
        if column.nullable == Some(false) && target_kind != DatabaseKind::ClickHouse {
            definition.push_str(" NOT NULL");
        }
        if let Some(default) = column
            .default
            .as_deref()
            .and_then(|raw| default_literal(source_kind, raw))
        {
            definition.push_str(&format!(" DEFAULT {default}"));
        }
        let comment = column.comment.trim();
        if !comment.is_empty()
            && matches!(target_kind, DatabaseKind::Mysql | DatabaseKind::ClickHouse)
        {
            definition.push_str(&format!(" COMMENT '{}'", comment.replace('\'', "''")));
        }
        lines.push(definition);
    }
    // 主键：源侧取得到、且引用的列都在清单里才带上。
    let pk_columns: Vec<String> = pk
        .iter()
        .filter(|name| columns.iter().any(|column| &column.name == *name))
        .filter_map(|name| duck_db::drivers::quote_identifier(name, quote).ok())
        .collect();
    if !pk_columns.is_empty() {
        lines.push(format!("PRIMARY KEY ({})", pk_columns.join(", ")));
    }
    if lines.is_empty() {
        return Err(DbError::Query("source table has no columns".into()));
    }
    let mut sql = format!("CREATE TABLE {qualified} (\n  {}\n)", lines.join(",\n  "));
    // PG 的列注释是独立语句，追加在建表之后。
    if target_kind == DatabaseKind::Postgres {
        for column in columns {
            let comment = column.comment.trim();
            if comment.is_empty() {
                continue;
            }
            if let Ok(quoted) = duck_db::drivers::quote_identifier(&column.name, quote) {
                sql.push_str(&format!(
                    "\nCOMMENT ON COLUMN {qualified}.{quoted} IS '{}';",
                    comment.replace('\'', "''")
                ));
            }
        }
    }
    Ok(sql)
}

/// 默认值原文 → DDL 里的 `DEFAULT …` 值。
///
/// 口径：MySQL 的默认值报的是**裸值**，字符串要补引号；其余引擎报的已是
/// 表达式原文。这里按**源**引擎解释（原文是源侧目录报出来的），再把表达式
/// 原样搬进目标 DDL。
fn default_literal(kind: DatabaseKind, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("NULL") {
        return None;
    }
    if kind != DatabaseKind::Mysql {
        return Some(raw.to_owned());
    }
    let numeric = raw
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == '+');
    let quoted = raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'');
    let expression = raw.starts_with('(');
    if numeric
        || quoted
        || expression
        || raw.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
        || raw.eq_ignore_ascii_case("CURRENT_TIMESTAMP()")
        || raw.eq_ignore_ascii_case("CURRENT_DATE")
    {
        Some(raw.to_owned())
    } else {
        Some(format!("'{}'", raw.replace('\'', "''")))
    }
}

/// 弹窗的两页：配置 / 进度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferPane {
    Config,
    Progress,
}

/// 四个下拉的槽位（数组下标）。
const PICK_SOURCE_CONN: usize = 0;
const PICK_SOURCE_NS: usize = 1;
const PICK_TARGET_CONN: usize = 2;
const PICK_TARGET_NS: usize = 3;

/// 数据传输弹窗的独立视图：表单 / 任务侧栏 / 进度的全部状态都在这里。
///
/// 单独立一个实体而不是把状态摊在弹窗闭包里：`open_dialog` 的正文闭包**只跑一次**，
/// 后台传输每传完一张表都要靠实体自己的 `notify()` 重绘。
pub(super) struct TransferDialogView {
    /// 宿主页面（会话、目录、`spawn_db` 都在那边）。
    page: WeakEntity<DatabasePage>,
    // ── 表单 ──
    source_conn: String,
    source_ns: String,
    target_conn: String,
    target_ns: String,
    /// 选中的源表名。
    selected: BTreeSet<String>,
    content: TransferContent,
    strategy: TransferStrategy,
    name_case: TableNameCase,
    /// 批量大小输入框（run 时解析；非法 / 越界夹回合法区间）。
    batch_input: Entity<InputState>,
    // ── 任务侧栏 ──
    tasks: Vec<TransferTask>,
    active_task_id: Option<String>,
    /// 加载 / 保存任务那一刻的规格快照（JSON）——「脏了没」的判断基准。
    saved_snapshot: String,
    task_search: Entity<InputState>,
    /// 批量录入的粘贴框（`None` = 没开）。
    batch_paste: Option<Entity<InputState>>,
    // ── 下拉 ──
    picks: [Entity<DbSelectState>; 4],
    stamps: [Option<String>; 4],
    // ── 传输对象 ──
    /// 对象搜索框。
    object_search: Entity<InputState>,
    /// 源库的基础表清单（按 `source_conn + source_ns` 签名缓存）。
    objects: Vec<String>,
    objects_stamp: Option<String>,
    // ── 进度 ──
    pane: TransferPane,
    rows: Vec<TransferRow>,
    job: Option<TransferJob>,
    running: bool,
    /// 下拉订阅。
    _subscriptions: Vec<Subscription>,
}

impl TransferDialogView {
    /// 造一个下拉实体（与 `db_select_state` 同款，只是上下文类型不同）。
    fn new_pick(window: &mut Window, cx: &mut Context<Self>) -> Entity<DbSelectState> {
        cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true)
        })
    }

    fn new_input(window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        cx.new(|cx| InputState::new(window, cx))
    }

    /// 带占位符的输入框（`placeholder` 定型在构造时，语言切换重建弹窗即可）。
    fn new_input_placeholder(
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let text = tr(cx, placeholder);
        cx.new(|cx| InputState::new(window, cx).placeholder(text))
    }

    fn new(page: WeakEntity<DatabasePage>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let picks = [
            Self::new_pick(window, cx),
            Self::new_pick(window, cx),
            Self::new_pick(window, cx),
            Self::new_pick(window, cx),
        ];
        let mut subscriptions = Vec::new();
        for (slot, pick) in picks.iter().enumerate() {
            subscriptions.push(cx.subscribe(
                pick,
                move |this, _, event: &SelectEvent<SearchableVec<DbSelectOption>>, cx| {
                    let SelectEvent::Confirm(value) = event;
                    let value = value
                        .as_ref()
                        .map(std::string::ToString::to_string)
                        .unwrap_or_default();
                    this.pick(slot, value, cx);
                },
            ));
        }
        let mut this = Self {
            page,
            source_conn: String::new(),
            source_ns: String::new(),
            target_conn: String::new(),
            target_ns: String::new(),
            selected: BTreeSet::new(),
            content: TransferContent::StructureAndData,
            strategy: TransferStrategy::Append,
            name_case: TableNameCase::Preserve,
            batch_input: Self::new_input(window, cx),
            tasks: Vec::new(),
            active_task_id: None,
            saved_snapshot: String::new(),
            task_search: Self::new_input_placeholder("搜索任务...", window, cx),
            batch_paste: None,
            picks,
            stamps: [None, None, None, None],
            object_search: Self::new_input_placeholder("搜索对象...", window, cx),
            objects: Vec::new(),
            objects_stamp: None,
            pane: TransferPane::Config,
            rows: Vec::new(),
            job: None,
            running: false,
            _subscriptions: subscriptions,
        };
        this.tasks = load_tasks(cx);
        this.saved_snapshot = this.snapshot(cx);
        this
    }

    // ── 表单状态 ──

    /// 规格快照（脏检查的基准）。
    fn snapshot(&self, cx: &App) -> String {
        let config = self.task_config(cx);
        serde_json::to_string(&config).unwrap_or_default()
    }

    /// 当前表单 → 任务规格。
    fn task_config(&self, cx: &App) -> TransferTaskConfig {
        TransferTaskConfig {
            source_conn: Some(self.source_conn.clone()),
            source_ns: Some(self.source_ns.clone()),
            target_conn: Some(self.target_conn.clone()),
            target_ns: Some(self.target_ns.clone()),
            tables: self.selected.iter().cloned().collect(),
            content: Some(self.content),
            strategy: Some(self.strategy),
            name_case: Some(self.name_case),
            batch_size: Some(self.batch_size_value(cx)),
        }
    }

    /// 批量大小的当前值：输入框里能解析就夹到合法区间，否则回落默认。
    fn batch_size_value(&self, cx: &App) -> usize {
        self.batch_input
            .read(cx)
            .value()
            .trim()
            .parse::<usize>()
            .ok()
            .map(|size| size.clamp(BATCH_SIZE_MIN, BATCH_SIZE_MAX))
            .unwrap_or(BATCH_SIZE_DEFAULT)
    }

    /// 某个下拉确认后回写表单；换连接就把该侧的库清空（库清单跟着连接走）。
    fn pick(&mut self, slot: usize, value: String, cx: &mut Context<Self>) {
        match slot {
            PICK_SOURCE_CONN => {
                self.source_conn = value;
                self.source_ns.clear();
                self.stamps[PICK_SOURCE_NS] = None;
                self.reset_objects();
            }
            PICK_SOURCE_NS => {
                self.source_ns = value;
                self.reset_objects();
            }
            PICK_TARGET_CONN => {
                self.target_conn = value;
                self.target_ns.clear();
                self.stamps[PICK_TARGET_NS] = None;
            }
            _ => self.target_ns = value,
        }
        cx.notify();
    }

    /// 换了源连接 / 源库：对象清单重灌，选中集合清空（新源里的名字才算数）。
    fn reset_objects(&mut self) {
        self.objects_stamp = None;
        self.selected.clear();
        self.objects.clear();
    }

    /// 交换两侧（连接 + 库一起换；对象清单与选中集合跟着新源重算）。
    fn swap_sides(&mut self, cx: &mut Context<Self>) {
        std::mem::swap(&mut self.source_conn, &mut self.target_conn);
        std::mem::swap(&mut self.source_ns, &mut self.target_ns);
        self.stamps = [None, None, None, None];
        self.reset_objects();
        cx.notify();
    }

    // ── 传输对象 ──

    /// 对象清单的签名变了就从源会话目录重灌（渲染入口调用）。
    fn sync_objects(&mut self, cx: &mut Context<Self>) {
        let stamp = format!("{}\u{1}{}", self.source_conn, self.source_ns);
        if self.objects_stamp.as_deref() == Some(stamp.as_str()) {
            return;
        }
        self.objects_stamp = Some(stamp);
        let source_conn = self.source_conn.clone();
        let source_ns = self.source_ns.clone();
        let names: Vec<String> = self
            .page
            .upgrade()
            .and_then(|page| {
                let page = page.read(cx);
                page.sessions.get(&source_conn).map(|session| {
                    session
                        .catalog
                        .iter()
                        .filter(|entry| entry.kind == TableKind::Table)
                        .filter(|entry| entry.schema.as_deref() == Some(source_ns.as_str()))
                        .map(|entry| entry.name.clone())
                        .collect()
                })
            })
            .unwrap_or_default();
        let mut names = names;
        names.sort();
        names.dedup();
        // 选中集合收进新清单：源换了之后留下的名字传不了，不如当场收干净。
        let valid: BTreeSet<String> = names.iter().cloned().collect();
        self.selected.retain(|name| valid.contains(name));
        self.objects = names;
    }

    fn toggle_object(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.selected.remove(name) {
            self.selected.insert(name.to_owned());
        }
        cx.notify();
    }

    /// 全选当前清单（搜索过滤后的可见项）。
    fn select_visible(&mut self, visible: &[String], cx: &mut Context<Self>) {
        for name in visible {
            self.selected.insert(name.clone());
        }
        cx.notify();
    }

    /// 打开（或复用）批量录入输入区。
    fn open_batch_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.batch_paste.is_none() {
            self.batch_paste = Some(Self::new_input_placeholder(
                "粘贴对象名，空格 / 逗号分隔",
                window,
                cx,
            ));
        }
        cx.notify();
    }

    /// 批量录入：把输入框里的名字收进选中集合。
    ///
    /// 分隔符 = 空白 / 逗号 / 分号；容忍 `库.表` / `schema.表` 前缀（限定词与源库
    /// 一致、或源库为空时剥掉），从别的工具整段粘贴过来也能用。
    fn import_pasted_names(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.batch_paste.clone() else {
            return;
        };
        let raw = input.read(cx).value().to_string();
        let source_ns = self.source_ns.clone();
        for token in raw.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
            let mut name = token.trim();
            if name.is_empty() {
                continue;
            }
            // 剥掉 `a.b` 形式的前缀：限定词与当前源库一致（或源库为空）时才剥。
            if let Some((qualifier, rest)) = name.split_once('.') {
                let rest = rest.trim();
                if !rest.is_empty() && (qualifier == source_ns || source_ns.is_empty()) {
                    name = rest;
                }
            }
            if self.objects.iter().any(|known| known == name) {
                self.selected.insert(name.to_owned());
            }
        }
        self.batch_paste = None;
        cx.notify();
    }

    // ── 任务侧栏 ──

    fn filtered_tasks(&self, cx: &App) -> Vec<TransferTask> {
        let query = self.task_search.read(cx).value().trim().to_lowercase();
        if query.is_empty() {
            self.tasks.clone()
        } else {
            self.tasks
                .iter()
                .filter(|task| task.name.to_lowercase().contains(&query))
                .cloned()
                .collect()
        }
    }

    /// 新建空白任务：表单清空，活动任务清空。
    fn new_blank_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.source_conn.clear();
        self.source_ns.clear();
        self.target_conn.clear();
        self.target_ns.clear();
        self.selected.clear();
        self.content = TransferContent::StructureAndData;
        self.strategy = TransferStrategy::Append;
        self.name_case = TableNameCase::Preserve;
        self.batch_input.update(cx, |input, cx| {
            input.set_value(BATCH_SIZE_DEFAULT.to_string(), window, cx)
        });
        self.active_task_id = None;
        self.stamps = [None, None, None, None];
        self.reset_objects();
        self.pane = TransferPane::Config;
        self.saved_snapshot = self.snapshot(cx);
        cx.notify();
    }

    /// 加载一条任务：连接已断开的那一侧跳过（不把用户送进必然失败的规格）。
    fn load_task(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.tasks.iter().find(|task| task.id == id).cloned() else {
            return;
        };
        let config = task.config;
        let source_conn_alive = config.source_conn.as_deref().is_some_and(|conn| {
            self.page
                .upgrade()
                .is_some_and(|page| page.read(cx).sessions.contains_key(conn))
        });
        let target_conn_alive = config.target_conn.as_deref().is_some_and(|conn| {
            self.page
                .upgrade()
                .is_some_and(|page| page.read(cx).sessions.contains_key(conn))
        });
        if source_conn_alive {
            self.source_conn = config.source_conn.clone().unwrap_or_default();
            self.source_ns = config.source_ns.clone().unwrap_or_default();
        }
        if target_conn_alive {
            self.target_conn = config.target_conn.clone().unwrap_or_default();
            self.target_ns = config.target_ns.clone().unwrap_or_default();
        }
        self.selected = config.tables.iter().cloned().collect();
        if let Some(content) = config.content {
            self.content = content;
        }
        if let Some(strategy) = config.strategy {
            self.strategy = strategy;
        }
        if let Some(name_case) = config.name_case {
            self.name_case = name_case;
        }
        if let Some(batch_size) = config.batch_size {
            self.batch_input.update(cx, |input, cx| {
                input.set_value(batch_size.to_string(), window, cx)
            });
        }
        self.stamps = [None, None, None, None];
        self.objects_stamp = None;
        self.objects.clear();
        self.active_task_id = Some(task.id.clone());
        self.saved_snapshot = self.snapshot(cx);
        self.pane = TransferPane::Config;
        if !source_conn_alive || !target_conn_alive {
            notify::info(tr(cx, "任务里的连接已断开，已跳过那一侧。"), window, cx);
        }
        cx.notify();
    }

    /// 保存：有活动任务就覆盖它，没有就新建一条（默认名自增）。
    fn save_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.task_config(cx);
        match &self.active_task_id {
            Some(id) => {
                if let Some(task) = self.tasks.iter_mut().find(|task| &task.id == id) {
                    task.config = config;
                }
            }
            None => {
                let id = format!(
                    "task-{}",
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_millis())
                        .unwrap_or_default()
                );
                let name = next_task_name(&self.tasks, cx);
                self.tasks.push(TransferTask {
                    id: id.clone(),
                    name,
                    config,
                });
                self.active_task_id = Some(id);
            }
        }
        save_tasks(&self.tasks, window, cx);
        self.saved_snapshot = self.snapshot(cx);
        notify::info(tr(cx, "已保存传输任务。"), window, cx);
        cx.notify();
    }

    fn delete_task(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.tasks.retain(|task| task.id != id);
        if self.active_task_id.as_deref() == Some(id) {
            self.active_task_id = None;
        }
        save_tasks(&self.tasks, window, cx);
        cx.notify();
    }

    // ── 下拉同步 ──

    /// 四个下拉的选项 / 选中值按签名同步（渲染入口调用；口径同比较弹窗）。
    fn sync_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (options, selected) = {
            let Some(page) = self.page.upgrade() else {
                return;
            };
            let page = page.read(cx);
            let conn_options: Vec<DbSelectOption> = page
                .sessions
                .keys()
                .filter_map(|conn_id| {
                    page.connections
                        .iter()
                        .find(|cfg| &cfg.id == conn_id)
                        .map(|cfg| DbSelectOption {
                            value: SharedString::from(conn_id.clone()),
                            label: SharedString::from(cfg.display_name()),
                        })
                })
                .collect();
            let ns_options = |conn_id: &str| -> Vec<DbSelectOption> {
                page.sessions
                    .get(conn_id)
                    .map(|session| {
                        let names = if namespace_is_database(session.kind) {
                            collect_namespaces(&session.databases, &session.schemas)
                        } else {
                            session.schemas.clone()
                        };
                        names
                            .into_iter()
                            .map(|name| DbSelectOption {
                                value: SharedString::from(name.clone()),
                                label: SharedString::from(name),
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let options = [
                conn_options.clone(),
                ns_options(&self.source_conn),
                conn_options.clone(),
                ns_options(&self.target_conn),
            ];
            let selected = [
                self.source_conn.clone(),
                self.source_ns.clone(),
                self.target_conn.clone(),
                self.target_ns.clone(),
            ];
            (options, selected)
        };
        for slot in 0..4 {
            let signature = options[slot]
                .iter()
                .map(|option| option.value.to_string())
                .collect::<Vec<_>>()
                .join("\u{1}");
            if self.stamps[slot].as_deref() != Some(signature.as_str()) {
                self.stamps[slot] = Some(signature);
                let items = options[slot].clone();
                self.picks[slot].update(cx, |select, cx| {
                    select.set_items(SearchableVec::new(items), window, cx);
                });
            }
            let current = self.picks[slot].read(cx).selected_value().cloned();
            if current.as_deref() != Some(selected[slot].as_str()) {
                let value = SharedString::from(selected[slot].clone());
                self.picks[slot].update(cx, |select, cx| {
                    select.set_selected_value(&value, window, cx);
                });
            }
        }
    }

    // ── 开跑 ──

    /// 两侧是否选全 + 有对象 + 源 ≠ 目标（同库对传没有意义）。
    fn ready(&self) -> bool {
        !self.running
            && !self.source_conn.is_empty()
            && !self.source_ns.is_empty()
            && !self.target_conn.is_empty()
            && !self.target_ns.is_empty()
            && !self.selected.is_empty()
            && !(self.source_conn == self.target_conn && self.source_ns == self.target_ns)
    }

    /// 开跑前置缺哪项就明说哪项（口径同比较弹窗的缺项清单）。
    fn missing_prerequisites(&self, cx: &App) -> Vec<String> {
        let mut missing: Vec<String> = [
            (self.source_conn.is_empty(), tr(cx, "源连接")),
            (self.source_ns.is_empty(), tr(cx, "源数据库")),
            (self.target_conn.is_empty(), tr(cx, "目标连接")),
            (self.target_ns.is_empty(), tr(cx, "目标数据库")),
        ]
        .into_iter()
        .filter(|&(missing, _)| missing)
        .map(|(_, label)| label.to_owned())
        .collect();
        if missing.is_empty() && self.selected.is_empty() {
            missing.push(tr(cx, "传输对象").to_owned());
        }
        if missing.is_empty()
            && self.source_conn == self.target_conn
            && self.source_ns == self.target_ns
        {
            missing.push(tr(cx, "不同的源与目标").to_owned());
        }
        missing
    }

    /// 开跑：把规格拍成快照逐表串行执行（一张传完再传下一张）。
    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let missing = self.missing_prerequisites(cx);
        if !missing.is_empty() {
            notify::warning(
                SharedString::from(trf(cx, "还未选择：{}", &[missing.join("、").as_str()])),
                window,
                cx,
            );
            return;
        }
        let Some(page) = self.page.upgrade() else {
            return;
        };
        // 批量大小先读出来：`page.read(cx)` 占着 cx，输入框不能再读。
        let batch_size = self.batch_size_value(cx);
        let prepared = {
            let page = page.read(cx);
            match (
                page.sessions.get(&self.source_conn),
                page.sessions.get(&self.target_conn),
            ) {
                (Some(source), Some(target)) => Some(TransferJob {
                    source_conn: source.conn.clone(),
                    target_conn: target.conn.clone(),
                    source_ns: self.source_ns.clone(),
                    target_ns: self.target_ns.clone(),
                    source_kind: source.kind,
                    target_kind: target.kind,
                    content: self.content,
                    strategy: self.strategy,
                    batch_size,
                    skipped_missing: tr(cx, "目标表不存在").to_owned(),
                    skipped_no_create: tr(cx, "目标引擎不支持建表").to_owned(),
                }),
                _ => None,
            }
        };
        let Some(job) = prepared else {
            notify::error(tr(cx, "连接已断开，请重新选择。"), window, cx);
            return;
        };
        let rows: Vec<TransferRow> = self
            .selected
            .iter()
            .map(|name| TransferRow {
                target_name: self.name_case.apply(name),
                status: TransferRowStatus::Pending,
            })
            .collect();
        self.job = Some(job);
        self.rows = rows;
        self.running = true;
        self.pane = TransferPane::Progress;
        cx.notify();
        self.transfer_step(0, cx);
    }

    /// 传第 `index` 张表（递归入口：回调里排下一张）。
    fn transfer_step(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.rows.len() {
            self.running = false;
            self.job = None;
            cx.notify();
            return;
        }
        let Some(job) = self.job.clone() else {
            self.running = false;
            return;
        };
        let Some(source_name) = self.selected.iter().nth(index).cloned() else {
            self.running = false;
            return;
        };
        let target_name = self.rows[index].target_name.clone();
        if let Some(row) = self.rows.get_mut(index) {
            row.status = TransferRowStatus::Working;
        }
        cx.notify();
        let page = self.page.clone();
        let _ = page.update(cx, |page, cx| {
            page.spawn_db(
                cx,
                move || -> (usize, Result<TransferOutcome, TransferFailure>) {
                    (index, run_table(&job, &source_name, &target_name))
                },
                move |page,
                      (index, result): (usize, Result<TransferOutcome, TransferFailure>),
                      _window,
                      cx| {
                    if let Some(dialog) = page.transfer_dialog.clone() {
                        dialog.update(cx, |view, cx| {
                            view.on_table_done(index, result, cx);
                        });
                    }
                },
            );
        });
    }

    /// 一张表传完：落结论，排下一张。
    fn on_table_done(
        &mut self,
        index: usize,
        result: Result<TransferOutcome, TransferFailure>,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.rows.get_mut(index) {
            row.status = match result {
                Ok(outcome) => TransferRowStatus::Done {
                    created: outcome.created,
                    inserted: outcome.inserted,
                    read: outcome.rows_read,
                    dropped: outcome.dropped_columns,
                },
                Err(TransferFailure::Skipped(reason)) => TransferRowStatus::Skipped(reason),
                Err(TransferFailure::Error(error)) => TransferRowStatus::Failed(error.to_string()),
            };
        }
        self.transfer_step(index + 1, cx);
    }
}

impl Render for TransferDialogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_pickers(window, cx);
        self.sync_objects(cx);
        let (tokens, extended) = {
            let theme = active_theme(cx);
            (theme.tokens().clone(), theme.extended().clone())
        };
        let body: AnyElement = match self.pane {
            TransferPane::Config => self.render_config(&tokens, &extended, cx),
            TransferPane::Progress => self.render_progress(&tokens, &extended, cx),
        };
        v_flex()
            .w_full()
            .min_w_0()
            .min_h_0()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.lg)
            .pb(tokens.spacing.md)
            .child(body)
    }
}

impl TransferDialogView {
    // ── 配置页 ──

    fn render_config(
        &self,
        tokens: &duck_theme::SemanticThemeTokens,
        extended: &duck_theme::ExtendedTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        let control = {
            let theme = active_theme(cx);
            theme.density().control
        };
        // 左侧「传输任务」栏（dbx 同款：任务清单 + 搜索 + 新建）。
        let tasks_bar = self.render_tasks_bar(tokens, extended, cx);
        // 源⇄目标两列 + 中间交换按钮。
        let side = |slot_conn: usize,
                    slot_ns: usize,
                    title: &'static str,
                    color: Hsla,
                    cx: &mut Context<Self>| {
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(tokens.spacing.xxs)
                .child(
                    div()
                        .text_size(tokens.typography.sm.size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(color)
                        .child(SharedString::from(title)),
                )
                .child(field_label(tr(cx, "连接"), cx))
                .child(
                    Select::new(&self.picks[slot_conn])
                        .placeholder(tr(cx, "选择连接"))
                        .h(control)
                        .w_full(),
                )
                .child(field_label(tr(cx, "数据库"), cx))
                .child(
                    Select::new(&self.picks[slot_ns])
                        .placeholder(tr(cx, "选择数据库"))
                        .h(control)
                        .w_full(),
                )
        };
        let source_side = side(
            PICK_SOURCE_CONN,
            PICK_SOURCE_NS,
            tr(cx, "源"),
            extended.colors.accent_text,
            cx,
        );
        let target_side = side(
            PICK_TARGET_CONN,
            PICK_TARGET_NS,
            tr(cx, "目标"),
            extended.colors.success,
            cx,
        );
        let swap = icon_button(
            "db-transfer-swap",
            tr(cx, "交换两侧"),
            Icon::new(DuckIcon::ArrowRightLeft).size(extended.icon.sm),
            ButtonVariant::Ghost,
            cx,
        )
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                let _ = view.update(cx, |this, cx| this.swap_sides(cx));
            }
        });

        // 传输对象（分组多选；本期口径 = 基础表）。
        let objects = self.render_objects(tokens, extended, cx);

        // 选项区（传输内容 / 目标表处理方式 / 表名大小写 / 批量大小）。
        let options = self.render_options(tokens, extended, cx);

        // 底部动作条：取消 / 保存为任务 / 开始传输。
        let missing = self.missing_prerequisites(cx);
        let missing_hint = if missing.is_empty() {
            None
        } else {
            Some(
                meta_text(
                    SharedString::from(trf(cx, "还未选择：{}", &[missing.join("、").as_str()])),
                    cx,
                )
                .into_any_element(),
            )
        };
        let save_label = if self.active_task_id.is_some() {
            tr(cx, "保存任务")
        } else {
            tr(cx, "保存为任务")
        };
        let footer = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .pt(tokens.spacing.sm)
            .child(
                button(
                    "db-transfer-cancel",
                    tr(cx, "取消"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(|_, window, cx| {
                    window.close_dialog(cx);
                }),
            )
            .child(
                button("db-transfer-save", save_label, ButtonVariant::Secondary, cx)
                    .disabled(self.running)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.save_task(window, cx));
                        }
                    }),
            )
            .child(div().flex_1())
            .children(missing_hint)
            .child(
                button(
                    "db-transfer-run",
                    tr(cx, "开始传输"),
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(!self.ready())
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.run(window, cx));
                    }
                }),
            );

        h_flex()
            .w_full()
            .min_w_0()
            .gap(tokens.spacing.md)
            .child(tasks_bar)
            .child(
                v_flex()
                    .id("db-transfer-config-scroll")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap(tokens.spacing.md)
                    .child(
                        h_flex()
                            .w_full()
                            .items_end()
                            .gap(tokens.spacing.sm)
                            .child(source_side)
                            .child(div().flex_none().pb(tokens.spacing.sm).child(swap))
                            .child(target_side),
                    )
                    .child(objects)
                    .child(options)
                    .child(footer),
            )
            .into_any_element()
    }

    /// 左侧「传输任务」栏：清单 + 搜索 + 新建。
    fn render_tasks_bar(
        &self,
        tokens: &duck_theme::SemanticThemeTokens,
        extended: &duck_theme::ExtendedTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        let filtered = self.filtered_tasks(cx);
        let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
        if filtered.is_empty() {
            list = list.child(
                div()
                    .py(tokens.spacing.xl)
                    .w_full()
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(tr(cx, "暂无已保存的传输任务")),
            );
        }
        for task in &filtered {
            let selected = self.active_task_id.as_deref() == Some(task.id.as_str());
            let task_id = task.id.clone();
            let task_id_delete = task.id.clone();
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("db-transfer-task-{}", task.id)))
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .px(tokens.spacing.xs)
                    .py(tokens.spacing.xxs)
                    .rounded(tokens.radius.sm)
                    .bg(if selected {
                        extended.colors.nav_selected
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|style| style.bg(extended.colors.hover_tint))
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let id = task_id.clone();
                            let _ = view.update(cx, |this, cx| {
                                this.load_task(&id, window, cx);
                            });
                        }
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(tokens.typography.sm.size)
                            .text_color(tokens.colors.foreground)
                            .truncate()
                            .child(selectable_text_with_id(
                                SharedString::from(format!("db-transfer-task-label-{}", task.id)),
                                SharedString::from(task.name.clone()),
                            )),
                    )
                    .child(
                        icon_button(
                            SharedString::from(format!("db-transfer-task-del-{}", task.id)),
                            tr(cx, "删除任务"),
                            Icon::new(DuckIcon::Eraser).size(extended.icon.sm),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            let id = task_id_delete;
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.delete_task(&id, window, cx);
                                });
                            }
                        }),
                    ),
            );
        }
        v_flex()
            .flex_none()
            .w(px(TRANSFER_TASKS_WIDTH))
            .min_h_0()
            .gap(tokens.spacing.xs)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        div()
                            .flex_1()
                            .text_size(tokens.typography.sm.size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(tokens.colors.foreground)
                            .child(tr(cx, "传输任务")),
                    )
                    .child(
                        icon_button(
                            "db-transfer-task-new",
                            tr(cx, "新建空白任务"),
                            Icon::new(DuckIcon::Plus).size(extended.icon.sm),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.new_blank_task(window, cx);
                                });
                            }
                        }),
                    ),
            )
            .child(
                Input::new(&self.task_search)
                    .appearance(true)
                    .with_size(Size::Small)
                    .text_size(tokens.typography.sm.size)
                    .w_full(),
            )
            .child(
                div()
                    .id("db-transfer-task-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .border_r_1()
            .border_color(extended.colors.hairline)
            .into_any_element()
    }

    /// 传输对象区：标题 +（搜索 / 批量录入 / 全选）+ 可勾选清单。
    fn render_objects(
        &self,
        tokens: &duck_theme::SemanticThemeTokens,
        extended: &duck_theme::ExtendedTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        let query = self.object_search.read(cx).value().trim().to_lowercase();
        let visible: Vec<String> = if query.is_empty() {
            self.objects.clone()
        } else {
            self.objects
                .iter()
                .filter(|name| name.to_lowercase().contains(&query))
                .cloned()
                .collect()
        };
        let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
        if self.source_conn.is_empty() || self.source_ns.is_empty() {
            list = list.child(
                div()
                    .py(tokens.spacing.lg)
                    .w_full()
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(tr(cx, "请先选择源连接和数据库")),
            );
        } else if visible.is_empty() {
            list = list.child(
                div()
                    .py(tokens.spacing.lg)
                    .w_full()
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(tr(cx, "暂无对象")),
            );
        }
        for name in &visible {
            let checked = self.selected.contains(name);
            let name_clone = name.clone();
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("db-transfer-obj-{name}")))
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .px(tokens.spacing.xs)
                    .py(tokens.spacing.xxs)
                    .rounded(tokens.radius.sm)
                    .hover(|style| style.bg(extended.colors.hover_tint))
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let name = name_clone.clone();
                            let _ = view.update(cx, |this, cx| this.toggle_object(&name, cx));
                        }
                    })
                    .child(
                        Icon::new(if checked {
                            DuckIcon::CheckboxCheck
                        } else {
                            DuckIcon::Square
                        })
                        .size(extended.icon.sm)
                        .text_color(if checked {
                            tokens.colors.primary
                        } else {
                            tokens.colors.muted_foreground
                        }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(tokens.typography.sm.size)
                            .text_color(tokens.colors.foreground)
                            .truncate()
                            .child(selectable_text_with_id(
                                SharedString::from(format!("db-transfer-obj-label-{name}")),
                                SharedString::from(name.clone()),
                            )),
                    ),
            );
        }
        // 批量录入的粘贴区（开着就多一块输入 + 导入 / 收起）。
        let paste_area = self.batch_paste.clone().map(|input| {
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&input)
                            .appearance(true)
                            .with_size(Size::Small)
                            .text_size(tokens.typography.sm.size)
                            .w_full(),
                    ),
                )
                .child(
                    button(
                        "db-transfer-paste-import",
                        tr(cx, "导入"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.import_pasted_names(cx));
                        }
                    }),
                )
                .child(
                    button(
                        "db-transfer-paste-cancel",
                        tr(cx, "收起"),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.batch_paste = None;
                                cx.notify();
                            });
                        }
                    }),
                )
        });
        v_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(
                        div()
                            .flex_1()
                            .text_size(tokens.typography.sm.size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(tokens.colors.foreground)
                            .child(tr(cx, "传输对象")),
                    )
                    .child(meta_text(
                        SharedString::from(trf(
                            cx,
                            "{}/{}",
                            &[
                                &self.selected.len().to_string(),
                                &self.objects.len().to_string(),
                            ],
                        )),
                        cx,
                    ))
                    .child(
                        div().w(px(160.)).child(
                            Input::new(&self.object_search)
                                .appearance(true)
                                .with_size(Size::Small)
                                .text_size(tokens.typography.sm.size)
                                .w_full(),
                        ),
                    )
                    .child(
                        button(
                            "db-transfer-paste",
                            tr(cx, "批量录入"),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .disabled(self.source_conn.is_empty())
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.open_batch_paste(window, cx);
                                });
                            }
                        }),
                    )
                    .child(
                        button(
                            "db-transfer-select-all",
                            tr(cx, "全选"),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .disabled(visible.is_empty())
                        .on_click({
                            let view = view.clone();
                            let visible = visible.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.select_visible(&visible, cx);
                                });
                            }
                        }),
                    ),
            )
            .child(
                div()
                    .id("db-transfer-objects")
                    .w_full()
                    .max_h(px(TRANSFER_OBJECTS_MAX_HEIGHT))
                    .min_h_0()
                    .overflow_y_scroll()
                    .rounded(tokens.radius.md)
                    .border_1()
                    .border_color(tokens.colors.border)
                    .bg(tokens.colors.surface)
                    .p(tokens.spacing.xs)
                    .child(list),
            )
            .children(paste_area)
            .into_any_element()
    }

    // ── 选项区 ──

    fn render_options(
        &self,
        tokens: &duck_theme::SemanticThemeTokens,
        extended: &duck_theme::ExtendedTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        let target_kind = self
            .page
            .upgrade()
            .and_then(|page| page.read(cx).sessions.get(&self.target_conn))
            .map(|session| session.kind);
        // 传输内容三档单选（dbx 同款：标签 + 一句说明）。
        let mut radios = v_flex().gap(tokens.spacing.xxs);
        for content in TransferContent::all() {
            let (label, hint) = content.label(cx);
            let selected = self.content == content;
            radios = radios.child(
                h_flex()
                    .id(SharedString::from(format!(
                        "db-transfer-content-{}",
                        content.index()
                    )))
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .py(tokens.spacing.xxs)
                    .pr(tokens.spacing.sm)
                    .rounded(tokens.radius.sm)
                    .cursor_pointer()
                    .hover(|style| style.bg(extended.colors.hover_tint))
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.set_content(content, cx);
                            });
                        }
                    })
                    .child(radio_dot(tokens, extended, selected))
                    .child(body_text(SharedString::from(label), cx))
                    .child(meta_text(SharedString::from(hint), cx)),
            );
        }
        // 目标表处理方式：当前内容档位下可用的档位才进下拉。
        let strategies: Vec<TransferStrategy> = [
            TransferStrategy::Append,
            TransferStrategy::Overwrite,
            TransferStrategy::Rebuild,
        ]
        .into_iter()
        .filter(|strategy| strategy.available(self.content, target_kind))
        .collect();
        // 当前档被档位切换挤掉时回落「追加」（界面上不出现悬空的选中值）。
        let effective_strategy = if strategies.contains(&self.strategy) {
            self.strategy
        } else {
            TransferStrategy::Append
        };
        let structure_only = self.content == TransferContent::StructureOnly;
        let strategy_row = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(field_label(tr(cx, "目标表处理方式"), cx))
            .child(dropdown_select(
                "db-transfer-strategy",
                strategies
                    .iter()
                    .map(|strategy| {
                        (
                            SharedString::from(strategy.value()),
                            SharedString::from(strategy.label(structure_only, cx)),
                        )
                    })
                    .collect::<Vec<_>>(),
                SharedString::from(effective_strategy.value()),
                {
                    let view = view.clone();
                    move |value: SharedString, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if value.as_ref() == TransferStrategy::Overwrite.value() {
                                this.strategy = TransferStrategy::Overwrite;
                            } else if value.as_ref() == TransferStrategy::Rebuild.value() {
                                this.strategy = TransferStrategy::Rebuild;
                            } else {
                                this.strategy = TransferStrategy::Append;
                            }
                            cx.notify();
                        });
                    }
                },
                cx,
            ));
        // 重建的危险提示（选了重建才出现；界面上每一档都要对得上口径）。
        let rebuild_hint = (self.strategy == TransferStrategy::Rebuild
            && self.content != TransferContent::StructureOnly)
            .then(|| {
                meta_text(
                    SharedString::from(tr(cx, "重建会先删掉目标表再建，目标表现有数据将丢失。")),
                    cx,
                )
                .into_any_element()
            });
        let case_row = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(field_label(tr(cx, "目标表名大小写"), cx))
            .child(dropdown_select(
                "db-transfer-name-case",
                [
                    TableNameCase::Preserve,
                    TableNameCase::Lower,
                    TableNameCase::Upper,
                ]
                .into_iter()
                .map(|case| {
                    (
                        SharedString::from(case.value()),
                        SharedString::from(case.label(cx)),
                    )
                })
                .collect::<Vec<_>>(),
                SharedString::from(self.name_case.value()),
                {
                    let view = view.clone();
                    move |value: SharedString, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if value.as_ref() == TableNameCase::Lower.value() {
                                this.name_case = TableNameCase::Lower;
                            } else if value.as_ref() == TableNameCase::Upper.value() {
                                this.name_case = TableNameCase::Upper;
                            } else {
                                this.name_case = TableNameCase::Preserve;
                            }
                            cx.notify();
                        });
                    }
                },
                cx,
            ));
        let batch_row = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(field_label(tr(cx, "批量大小"), cx))
            .child(
                div().w(px(96.)).child(
                    Input::new(&self.batch_input)
                        .appearance(true)
                        .with_size(Size::Small)
                        .text_size(tokens.typography.sm.size)
                        .w_full(),
                ),
            )
            .child(meta_text(
                SharedString::from(trf(
                    cx,
                    "每次落库的行数（{}–{}）",
                    &[&BATCH_SIZE_MIN.to_string(), &BATCH_SIZE_MAX.to_string()],
                )),
                cx,
            ));
        v_flex()
            .w_full()
            .gap(tokens.spacing.sm)
            .child(
                v_flex()
                    .gap(tokens.spacing.xxs)
                    .child(field_label(tr(cx, "传输内容"), cx))
                    .child(radios),
            )
            .child(strategy_row)
            .children(rebuild_hint)
            .child(case_row)
            .child(batch_row)
            .into_any_element()
    }

    /// 切内容档位：把被新档位挤掉的处理方式回落到「追加」。
    fn set_content(&mut self, content: TransferContent, cx: &mut Context<Self>) {
        self.content = content;
        let target_kind = self
            .page
            .upgrade()
            .and_then(|page| page.read(cx).sessions.get(&self.target_conn))
            .map(|session| session.kind);
        if !self.strategy.available(content, target_kind) {
            self.strategy = TransferStrategy::Append;
        }
        cx.notify();
    }

    // ── 进度页 ──

    fn render_progress(
        &self,
        tokens: &duck_theme::SemanticThemeTokens,
        extended: &duck_theme::ExtendedTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        let done = self
            .rows
            .iter()
            .filter(|row| {
                !matches!(
                    row.status,
                    TransferRowStatus::Pending | TransferRowStatus::Working
                )
            })
            .count();
        let transferred: u64 = self
            .rows
            .iter()
            .filter_map(|row| match &row.status {
                TransferRowStatus::Done { inserted, .. } => Some(*inserted),
                _ => None,
            })
            .sum();
        let failed = self
            .rows
            .iter()
            .filter(|row| matches!(row.status, TransferRowStatus::Failed(_)))
            .count();
        let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
        for (index, row) in self.rows.iter().enumerate() {
            list = list.child(self.render_progress_row(index, row, extended, cx));
        }
        let footer = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .pt(tokens.spacing.sm)
            .child(
                button(
                    "db-transfer-back",
                    tr(cx, "返回配置"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(self.running)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.pane = TransferPane::Config;
                            this.rows.clear();
                            cx.notify();
                        });
                    }
                }),
            )
            .child(div().flex_1())
            .child(
                button(
                    "db-transfer-close",
                    tr(cx, "关闭"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(|_, window, cx| {
                    window.close_dialog(cx);
                }),
            );
        v_flex()
            .w_full()
            .min_w_0()
            .min_h_0()
            .gap(tokens.spacing.sm)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(neutral_tag(
                        SharedString::from(trf(
                            cx,
                            "{}/{} 张",
                            &[&done.to_string(), &self.rows.len().to_string()],
                        )),
                        cx,
                    ))
                    .child(if self.running {
                        accent_tag(SharedString::from(tr(cx, "传输中…")), cx).into_any_element()
                    } else {
                        neutral_tag(
                            SharedString::from(trf(
                                cx,
                                "已传输 {} 行",
                                &[&transferred.to_string()],
                            )),
                            cx,
                        )
                        .into_any_element()
                    })
                    .children((failed > 0).then(|| {
                        accent_tag(
                            SharedString::from(trf(cx, "失败 {}", &[&failed.to_string()])),
                            cx,
                        )
                        .into_any_element()
                    }))
                    .child(div().flex_1())
                    .children(self.running.then(|| {
                        meta_text(SharedString::from(tr(cx, "正在传输数据…")), cx)
                            .into_any_element()
                    })),
            )
            .child(
                div()
                    .id("db-transfer-progress")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .rounded(tokens.radius.md)
                            .border_1()
                            .border_color(tokens.colors.border)
                            .bg(tokens.colors.surface)
                            .p(tokens.spacing.sm)
                            .child(list),
                    ),
            )
            .child(footer)
            .into_any_element()
    }

    /// 进度清单的一行：表名 + 状态 chip + 说明。
    fn render_progress_row(
        &self,
        index: usize,
        row: &TransferRow,
        extended: &duck_theme::ExtendedTokens,
        cx: &App,
    ) -> AnyElement {
        let tokens = active_theme(cx).tokens();
        let (chip, detail) = match &row.status {
            TransferRowStatus::Pending => (
                neutral_tag(SharedString::from(tr(cx, "待传输")), cx),
                div().into_any_element(),
            ),
            TransferRowStatus::Working => (
                accent_tag(SharedString::from(tr(cx, "传输中")), cx),
                div().into_any_element(),
            ),
            TransferRowStatus::Done {
                created,
                inserted,
                read,
                dropped,
            } => {
                let mut parts: Vec<String> = Vec::new();
                if *created {
                    parts.push(tr(cx, "已建表").to_owned());
                }
                parts.push(trf(cx, "读 {} 行", &[&read.to_string()]).to_owned());
                parts.push(trf(cx, "已插入 {} 行", &[&inserted.to_string()]).to_owned());
                if *dropped > 0 {
                    parts.push(trf(cx, "丢弃 {} 列", &[&dropped.to_string()]).to_owned());
                }
                (
                    neutral_tag(SharedString::from(tr(cx, "完成")), cx),
                    meta_text(SharedString::from(parts.join(" · ")), cx).into_any_element(),
                )
            }
            TransferRowStatus::Skipped(reason) => (
                neutral_tag(SharedString::from(tr(cx, "跳过")), cx),
                meta_text(SharedString::from(reason.clone()), cx).into_any_element(),
            ),
            TransferRowStatus::Failed(reason) => (
                accent_tag(SharedString::from(tr(cx, "失败")), cx),
                meta_text(SharedString::from(reason.clone()), cx).into_any_element(),
            ),
        };
        h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.xs)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.sm)
            .hover(|style| style.bg(extended.colors.hover_tint))
            .child(
                div()
                    .min_w_0()
                    .flex_none()
                    .max_w(px(280.))
                    .text_size(tokens.typography.sm.size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.foreground)
                    .truncate()
                    .child(selectable_text_with_id(
                        SharedString::from(format!("db-transfer-row-{index}")),
                        SharedString::from(row.target_name.clone()),
                    )),
            )
            .child(chip)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(tokens.typography.xs.size)
                    .child(detail),
            )
            .into_any_element()
    }
}

/// 单选圆点（选中 = 主色圈 + 白芯，未选 = hairline 描边空圈）。
fn radio_dot(
    tokens: &duck_theme::SemanticThemeTokens,
    extended: &duck_theme::ExtendedTokens,
    selected: bool,
) -> AnyElement {
    let size = px(14.);
    if selected {
        div()
            .flex_none()
            .size(size)
            .rounded_full()
            .border_1()
            .border_color(tokens.colors.primary)
            .p(px(3.))
            .child(div().size_full().rounded_full().bg(tokens.colors.primary))
            .into_any_element()
    } else {
        div()
            .flex_none()
            .size(size)
            .rounded_full()
            .border_1()
            .border_color(extended.colors.hairline)
            .into_any_element()
    }
}

impl DatabasePage {
    /// 打开数据传输弹窗：源 = 打开菜单所在的连接 + 命名空间，目标默认挑
    /// 「命名空间能用」的另一条已连接会话（口径同比较弹窗：HashMap 顺序随机，
    /// 挑中命名空间为空的会话会让目标库无从选起）。
    pub(super) fn open_transfer_dialog(
        &mut self,
        conn_id: &str,
        namespace: &str,
        tables: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        let namespace_of = |session: &Session| -> Option<String> {
            if namespace_is_database(session.kind) {
                session
                    .databases
                    .first()
                    .cloned()
                    .or_else(|| session.schema.clone())
            } else {
                session.schema.clone()
            }
        };
        let (target_conn, target_ns) = self
            .sessions
            .iter()
            .filter(|(id, _)| id.as_str() != conn_id)
            .find_map(|(id, session)| namespace_of(session).map(|ns| (id.clone(), ns)))
            .unwrap_or_else(|| {
                let fallback = self
                    .sessions
                    .keys()
                    .find(|other| other.as_str() != conn_id)
                    .cloned()
                    .unwrap_or_else(|| conn_id.to_owned());
                let ns = self
                    .sessions
                    .get(&fallback)
                    .and_then(namespace_of)
                    .unwrap_or_default();
                (fallback, ns)
            });
        let page = cx.entity().downgrade();
        let view = cx.new(|cx| {
            let mut this = TransferDialogView::new(page, window, cx);
            this.source_conn = conn_id.to_owned();
            this.source_ns = namespace.to_owned();
            this.target_conn = target_conn;
            this.target_ns = target_ns;
            this.selected = tables.iter().cloned().collect();
            this
        });
        // 进度回调要能把逐表结论送回弹窗视图：挂在页面层（关掉弹窗即被下一次
        // 打开覆盖；弹窗已关时回调配到一个已脱离的实体上是空操作）。
        self.transfer_dialog = Some(view.clone());
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let view = view.clone();
            let drag_for_content = drag.clone();
            dialog
                .w(px(TRANSFER_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(DialogButtonProps::default())
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .min_h_0()
                            .child(modal_title_row(
                                "db-transfer-title",
                                &drag_for_content,
                                tr(cx, "数据传输"),
                                cx,
                            ))
                            .child(view.clone()),
                    ))
                })
        });
    }
}
