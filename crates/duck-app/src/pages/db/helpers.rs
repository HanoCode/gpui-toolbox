//! 跨领域复用的自由函数：kv 档案读取、模块偏好取值、数值与时长格式化、
//! 命名空间推导、标签重名去重、结果集纯计算（列宽 / 命中行 / 分页排序 / 表格模板）。
//!
//! 本文件的函数**不持有页面状态**：要么纯参数进出，要么只收 `&App` / `&Context<Self>`
//! 读全局状态。页面状态在 `mod.rs`，领域行为在各自的子模块。

use std::collections::BTreeMap;

use super::*;

/// 这个引擎的「命名空间」是不是**就是库本身**？
///
/// 是 → 界面上只有一层（下拉 / 库列表里的每一项都是库，选中即切库）；
/// 否 → PG / MSSQL：库是外层、schema 是库内一层，两者必须分开呈现。
///
/// Elasticsearch 也归「是」：它没有库这一层，`list_databases` / `list_schemas` 都恒空，
/// 归哪边都不影响（界面上不会出现这一组控件）。
pub(super) fn namespace_is_database(kind: DatabaseKind) -> bool {
    !matches!(kind, DatabaseKind::Postgres | DatabaseKind::SqlServer)
}

/// 新建一个**空的**可搜索下拉状态（`searchable` 开着：选项多时下拉顶上有搜索框）。
/// 选项与选中值由各渲染入口按指纹灌入；db 页所有下拉（过滤列 / 批量列 / 命名空间×2 /
/// 比较与传输的目标连接、目标库）同用这一个工厂。
pub(super) fn db_select_state(
    window: &mut Window,
    cx: &mut Context<DatabasePage>,
) -> Entity<DbSelectState> {
    cx.new(|cx| SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true))
}

/// 「库」清单优先取 [`Session::databases`]；它空（老引擎 / 权限不足 / ES）时退回
/// [`Session::schemas`]。
///
/// 退回而不是返回空：`databases` 拿不到不代表没得选——部分服务端（受限账号、
/// 只读副本）只肯暴露当前库，这时 `schemas` 里的那点信息仍是可用的导航。
pub(super) fn collect_namespaces(databases: &[String], schemas: &[String]) -> Vec<String> {
    if databases.is_empty() {
        schemas.to_vec()
    } else {
        databases.to_vec()
    }
}

/// 「同对象复用」判定：在已开标签里找**同连接 + 同身份**的第一个。
///
/// 纯函数（输入全是借用）：复用策略是界面行为里最容易写歪的部分，拆出来才好单测。
pub(super) fn find_reusable_tab(
    tabs: &[String],
    kinds: &HashMap<String, TabKind>,
    owners: &HashMap<String, String>,
    conn_id: &str,
    target: &TabKind,
) -> Option<String> {
    tabs.iter()
        .find(|tab| {
            owners.get(*tab).is_some_and(|owner| owner == conn_id)
                && kinds.get(*tab).is_some_and(|kind| kind == target)
        })
        .cloned()
}

/// 标签重名去重：同显示名的第 n 次出现补 ` n` 后缀（`users` → `users 2` → `users 3`）。
///
/// 数据标签叫 `users@main`、库标签叫 `main`、查询标签叫连接名 —— 同名撞车在意料之中
/// （两张同名表分属不同库 / 同一连接开两个查询），不补后缀就分不清点的是哪一个。
/// 纯函数 + 输入按标签顺序，方便单测；后缀按**出现顺序**给，不是全局计数。
pub(super) fn dedupe_titles(base: &[String]) -> Vec<String> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::with_capacity(base.len());
    for name in base {
        let counter = seen.entry(name.as_str()).or_insert(0);
        *counter += 1;
        if *counter == 1 {
            out.push(name.clone());
        } else {
            out.push(format!("{name} {}", *counter));
        }
    }
    out
}

/// 采样时刻（epoch 秒）。时钟漂移只影响横轴间距，不影响数值。
pub(super) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

/// 表单缺 id 时的兜底：名字 FNV 哈希混入时间戳，唯一即可。
pub(super) fn id_suffix(seed: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes().chain(nanos.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 建一个带占位符的输入框状态。
///
/// 泛型收 `Context<T>`：页面自己（[`DatabasePage`]）与两个弹窗正文视图
/// （[`DriverPickerView`] / [`EditFormView`]）都从各自的上下文里建输入框。
pub(super) fn text_input<T>(
    window: &mut Window,
    cx: &mut Context<T>,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

/// 建一个**带初值**的输入框状态（构建器「列 ▾」菜单开行时预填列名用）。
///
/// 初值走 [`InputState::default_value`]（构造期直接落文本，不用再 `set_value` 追一次
/// 窗口刷新）；空初值等于 [`text_input`]，调用方不必分支。
pub(super) fn text_input_with<T>(
    window: &mut Window,
    cx: &mut Context<T>,
    placeholder: impl Into<SharedString>,
    value: &str,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

/// 建一个带占位符的口令输入框状态（显示为掩码）。
pub(super) fn secret_input(
    window: &mut Window,
    cx: &mut Context<DatabasePage>,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .masked(true)
    })
}

/// 数据库模块偏好（设置页「数据库」；未设置时是各常量的出厂默认）。
///
/// 每次调用解析一遍 JSON —— 这几处都在「发起一次查询 / 翻一页」的路径上，不是每帧，
/// 相比之下 `serde_json` 的这点开销可以忽略；换来的是没有第二处真源要同步。
///
/// `super::super` 是 `pages`：本模块是 `pages::db` 的子模块，偏好页在兄弟位置。
pub(super) fn db_prefs(cx: &App) -> super::super::module_settings::DbSettings {
    super::super::module_settings::db_settings(cx)
}

/// 单条查询的结果行数上限（设置页可改）。
pub(super) fn query_row_cap(cx: &App) -> usize {
    db_prefs(cx).row_cap as usize
}

/// 表浏览 / 分页查询每页的行数（设置页可改）。
pub(super) fn browse_page_size(cx: &App) -> usize {
    db_prefs(cx).page_size as usize
}

/// 查询历史保留条数（设置页可改）。
pub(super) fn history_limit(cx: &App) -> usize {
    db_prefs(cx).history_limit as usize
}

/// 紧凑时长（数字 + `s`/`m`/`h`/`d`）：指标图的横轴档位与「覆盖 {}」共用。
///
/// 刻意**不**进 EN 表：`3m` 是量纲缩写而不是句子，各语言一致；这与「相对时间文案走
/// `tr_relative_time`」那条规则不冲突——那里管的是「3 分钟前」这类成句的文案。
pub(super) fn compact_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// 纵轴刻度的紧凑数值（`1.5k` / `2.0M` / 整数原样 / 小数留两位）。
///
/// 服务器计数器动辄上亿（`Bytes_sent`），原样铺在纵轴上会把图挤没——k/M/G 是
/// 数字缩写，同样不进 EN 表。
pub(super) fn compact_number(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1e9 {
        format!("{:.1}G", value / 1e9)
    } else if abs >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1}k", value / 1e3)
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// 从 kv 读连接档案；解析失败回落空表（用户重存即可覆盖）。
pub(super) fn load_connections(cx: &App) -> Vec<ConnectionConfig> {
    duck_store::kv_get(cx, KEY_CONNECTIONS)
        .and_then(|json| serde_json::from_str::<Vec<ConnectionConfig>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读分组档案；解析失败回落空表。
pub(super) fn load_groups(cx: &App) -> Vec<String> {
    duck_store::kv_get(cx, KEY_GROUPS)
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读传输档案；解析失败回落空表（用户重存即可覆盖）。
pub(super) fn load_tunnel_profiles(cx: &App) -> Vec<TunnelProfile> {
    duck_store::kv_get(cx, KEY_TUNNEL_PROFILES)
        .and_then(|json| serde_json::from_str::<Vec<TunnelProfile>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读查询历史（解析失败 / 缺键 → 空）。
pub(super) fn load_history(cx: &App) -> Vec<String> {
    duck_store::kv_get(cx, KEY_HISTORY)
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读已保存查询（解析失败 / 缺键 → 空，用户重存即可覆盖）。
pub(super) fn load_saved_queries(cx: &App) -> Vec<SavedQuery> {
    duck_store::kv_get(cx, KEY_SAVED_QUERIES)
        .and_then(|json| serde_json::from_str::<Vec<SavedQuery>>(&json).ok())
        .unwrap_or_default()
}

/// 「保存到 SQL 库」的默认文件名：**`<表名>.sql`**（对齐 dbx —— 它用标签标题，
/// 而标签标题就是对象名）。
///
/// 没有选中表时退回「SQL 首行前 24 个字符 + `.sql`」（与保存图表同口径）：给一个
/// **能改的占位**，而不是「未命名」这种等于没给的东西。表名是空串（理论上不该有）
/// 时同样走这条回退。
pub(super) fn default_sql_file_name(table: Option<&TableRef>, sql: &str) -> String {
    let stem = table
        .map(|table| table.name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            sql.lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(24)
                .collect()
        });
    ensure_sql_extension(&stem)
}

/// 补上 `.sql` 后缀（已有就不重复加；大小写不敏感，`.SQL` 也算已有）。
///
/// 补在后缀这一层而不是塞进默认值里：用户手打 `orders` 时也该落成 `orders.sql` ——
/// 文件名带不带后缀不该取决于他是不是照抄了默认值（dbx 的 `defaultSavedSqlName`
/// 也是这个口径）。
pub(super) fn ensure_sql_extension(stem: &str) -> String {
    let trimmed = stem.trim();
    if trimmed.to_lowercase().ends_with(".sql") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}.sql")
    }
}

/// 已保存查询里**出现过的**文件夹（去重 + 排序）。
///
/// **从查询本身推导**，不另存一份文件夹清单：文件夹存在的意义就是「里面有查询」，
/// 单独存一份就多出两份状态要对齐（删掉查询忘了删文件夹 = 树上挂一个永远空的节点，
/// 而它还是个可点的空壳）。代价是「建一个空文件夹」不保留 —— 本页也没有那个动作：
/// 保存时填一个新名字即新建。
pub(super) fn saved_query_folders(saved: &[SavedQuery]) -> Vec<String> {
    let mut folders: Vec<String> = saved
        .iter()
        .filter_map(saved_query_folder)
        .map(str::to_owned)
        .collect();
    folders.sort();
    folders.dedup();
    folders
}

/// 一条查询所属的文件夹（已裁空白）；`None` = **根文件夹「SQL 库」**。
///
/// 「根」在数据里有两种写法（字段缺失、字段是空串 / 全空白），归一在这一处：
/// 判据散在各个调用点上，迟早出现「存的时候算根、分组的时候算文件夹」这种两边不一致。
pub(super) fn saved_query_folder(saved: &SavedQuery) -> Option<&str> {
    saved
        .folder
        .as_deref()
        .map(str::trim)
        .filter(|folder| !folder.is_empty())
}

/// 「保存到 SQL 库」弹窗里「已有」的那一枚文件夹标签：点一下把名字填进文件夹输入框。
///
/// 值走**输入框自己的** `update`（`InputState` 是个实体，改值自己会重绘），所以这里
/// 不需要一个能跟着重绘的选中态 —— 这正是本页没为这一栏单开一个下拉控件的理由
/// （弹窗正文的宿主 `notify()` 到不了 `content` 闭包，见 `duck-components/src/dialog.rs`）。
pub(super) fn folder_chip(name: &str, input: &Entity<InputState>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let input = input.clone();
    let value = name.to_owned();
    let label = SharedString::from(name.to_owned());
    button(
        SharedString::from(format!("db-save-sql-folder-{name}")),
        label,
        ButtonVariant::Ghost,
        cx,
    )
    .h(theme.density().status_control)
    .px(tokens.spacing.xs)
    .on_click(move |_, window, cx| {
        let value = value.clone();
        input.update(cx, |state, cx| state.set_value(value, window, cx));
    })
    .into_any_element()
}

/// 从 kv 读已保存图表（解析失败 / 缺键 → 空，用户重存即可覆盖）。
pub(super) fn load_saved_charts(cx: &App) -> Vec<SavedChart> {
    duck_store::kv_get(cx, KEY_SAVED_CHARTS)
        .and_then(|json| serde_json::from_str::<Vec<SavedChart>>(&json).ok())
        .unwrap_or_default()
}

/// 从 kv 读用户仪表板（解析失败 / 缺键 → 空，用户重存即可覆盖）。
pub(super) fn load_dashboards(cx: &App) -> Vec<Dashboard> {
    duck_store::kv_get(cx, KEY_DASHBOARDS)
        .and_then(|json| serde_json::from_str::<Vec<Dashboard>>(&json).ok())
        .unwrap_or_default()
}

/// 把一条 SQL 记进查询历史（去重后置顶，超出 [`HISTORY_LIMIT`] 丢最旧的）。
///
/// 只记**非空且与最近一条不同**的语句：重复执行同一条查询是很常见的动作，
/// 每次都记会把历史刷成同一行。
pub(super) fn push_history(history: &mut Vec<String>, sql: &str, limit: usize) {
    let sql = sql.trim();
    if sql.is_empty() {
        return;
    }
    history.retain(|entry| entry != sql);
    history.insert(0, sql.to_owned());
    history.truncate(limit.max(1));
}

/// 翻页用的 `ORDER BY` 档位：**用户排序列在前，主键随后兜底**。
///
/// 只按用户那一列排是不够的：同值行之间的先后在 SQL 里是**未定义**的，PG / MySQL
/// 每页各排一次就可能给出不同顺序 → 翻页重复或漏行（`docs/database-design.md` §4.5）。
/// 主键（唯一）随后补位，整条序才是全序。
///
/// 用户排的列**本身就是主键的一部分**时不再重复挂一列（重复列不报错但没意义），
/// 改用它指定的方向——否则「按主键降序」会被后面的升序主键抵消。
pub(super) fn page_order(sort: Option<&SortSpec>, pk: &[String]) -> Vec<OrderBy> {
    let mut order: Vec<OrderBy> = Vec::with_capacity(pk.len() + 1);
    if let Some(spec) = sort
        && !pk.contains(&spec.column)
    {
        order.push(if spec.descending {
            OrderBy::desc(spec.column.clone())
        } else {
            OrderBy::asc(spec.column.clone())
        });
    }
    order.extend(pk.iter().map(|column| {
        let descending = sort.is_some_and(|spec| spec.column == *column && spec.descending);
        if descending {
            OrderBy::desc(column.clone())
        } else {
            OrderBy::asc(column.clone())
        }
    }));
    order
}

/// 一行结果的 TSV 文本（不含表头）—— 「复制整行」用。
///
/// 单格转义走内核的 [`tsv_field`]：整表那条路（[`ResultSet::to_tsv`]）用的是同一个
/// 函数，两边各写一份转义规则迟早会漂移——而漂移的那一份会把带换行的备注错开一整行。
pub(super) fn row_as_tsv(row: &[duck_db::Value]) -> String {
    row.iter()
        .map(|value| tsv_field(&value.render()))
        .collect::<Vec<_>>()
        .join("\t")
}

/// 把结果集转成数据表组件的**中性模型**（见 `duck_components::data_table`）。
///
/// ⚠️ 转换只在**装结果时**跑一次，**绝不能**落在渲染路径上：每格的展示文本在这
/// 里算好并共享（`Arc<str>`，渲染只 bump 引用计数），而 [`Value::render`] 内含整串
/// 复制 —— 把它放进每帧的 `uniform_list` 闭包（可见行 × 列数 每帧重算一遍）正是
/// 原先「行一多滚动就掉帧」的主源之一。
///
/// 列宽也一并交给组件（`TableModel::estimated_widths` 与原先
/// `compute_column_widths` 同一套采样口径：前 200 行、按展示文本字符数估宽再夹
/// 上下限），宿主不再自留一份宽度缓存。
pub(super) fn build_table_model(
    result: &ResultSet,
    formats: &BTreeMap<String, ColumnFormat>,
) -> TableModel {
    let columns = result
        .columns
        .iter()
        .map(|column| {
            ColumnSpec::new(column.name.clone(), column.type_name.clone())
                .with_comment(column.comment.clone())
        })
        .collect();
    let rows = result
        .rows
        .iter()
        .map(|row| {
            RowData::new(
                row.iter()
                    .enumerate()
                    // 千分位按**列**生效：先查列名有没有开档，开了且列是数值型
                    // 才对这一格换显示文本（原值不动，复制 / 编辑仍走 `Value`）。
                    .map(|(col, value)| {
                        let name = result.columns[col].name.as_str();
                        let formatted = formats.get(name).is_some_and(|f| {
                            *f == ColumnFormat::Thousands && result_column_is_numeric(name, result)
                        });
                        table_cell_with_format(value, formatted)
                    })
                    .collect(),
            )
        })
        .collect();
    TableModel::new(columns, rows)
}

/// 列是不是**数值型**：优先看该列第一个非空单元格的实际类型（结果集的类型名在
/// 表达式列上常常拿不到），类型名兜底（`int` / `decimal` 这类前缀）。
pub(super) fn result_column_is_numeric(name: &str, result: &ResultSet) -> bool {
    if let Some(col) = result.columns.iter().position(|c| c.name == name) {
        if let Some(value) = result.rows.iter().find_map(|row| row.get(col)) {
            return matches!(
                value,
                Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_)
            );
        }
        if let Some(column) = result.columns.get(col) {
            let t = column.type_name.to_ascii_lowercase();
            return [
                "int", "decimal", "numeric", "float", "double", "real", "num",
            ]
            .iter()
            .any(|prefix| t.starts_with(prefix));
        }
    }
    false
}

/// [`Value::render`] 加一层可选的千分位（整数部分按 3 位插逗号；负号与小数不动）。
fn table_cell_with_format(value: &Value, thousands: bool) -> CellValue {
    let kind = match value {
        Value::Null => CellKind::Null,
        Value::Bool(_) => CellKind::Bool,
        Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_) => CellKind::Number,
        Value::Json(_) => CellKind::Json,
        Value::Bytes(_) => CellKind::Bytes,
        Value::Text(_) | Value::Date(_) | Value::Time(_) | Value::DateTime(_) => CellKind::Text,
    };
    let text = value.render();
    CellValue::new(
        kind,
        if thousands && matches!(kind, CellKind::Number) {
            thousands_separated(&text)
        } else {
            text
        },
    )
}

/// 数值展示文本的千分位：`-1234567.89` → `-1,234,567.89`。
///
/// 只处理**已经渲染好**的文本（内核把 `Decimal` 保成字符串正是为了不动精度），
/// 按小数点切两半、只对整数部分从右往左插逗号；没有小数点就整段处理。
pub(super) fn thousands_separated(text: &str) -> String {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((int_part, frac_part)) => (int_part, Some(frac_part)),
        None => (rest, None),
    };
    let mut grouped = String::with_capacity(int_part.len() + int_part.len() / 3);
    for (index, ch) in int_part.chars().enumerate() {
        if index > 0 && (int_part.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    match frac_part {
        Some(frac_part) => format!("{sign}{grouped}.{frac_part}"),
        None => format!("{sign}{grouped}"),
    }
}

/// 单元格的**粗分类**（与展示文本分开：NULL 压灰、将来数值右对齐 / JSON 单独阅览
/// 都靠类型判，不靠「猜字符串像不像数字」）。
///
/// `Decimal` / `Date` / `Time` / `DateTime` 都归 `Text`：它们渲染出来就是文本
/// （内核刻意不让数值经 `f64` 有损中转，保精度的字符串就是最终展示形态）。
fn table_cell(value: &Value) -> CellValue {
    let kind = match value {
        Value::Null => CellKind::Null,
        Value::Bool(_) => CellKind::Bool,
        Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_) => CellKind::Number,
        Value::Json(_) => CellKind::Json,
        Value::Bytes(_) => CellKind::Bytes,
        Value::Text(_) | Value::Date(_) | Value::Time(_) | Value::DateTime(_) => CellKind::Text,
    };
    CellValue::new(kind, value.render())
}

/// 结果里**可见**的列：`(原始下标, 列)`，被「字段筛选」藏掉的按列名剔除。
///
/// 保留下标是因为行数据（`Vec<Value>`）与列宽都按**完整**列序对齐 —— 渲染只是
/// 「跳过几列」，不是「另成一份结果」。网格 / 列模式 / JSON 三个视图共用这一处口径，
/// 免得出现「藏了一列，换个视图它又冒出来」。
pub(super) fn visible_columns(
    columns: &[Column],
    hidden: &BTreeSet<String>,
) -> Vec<(usize, Column)> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, column)| !hidden.contains(&column.name))
        .map(|(index, column)| (index, column.clone()))
        .collect()
}

/// 精确数值的三段分解：`(负号, 整数位〔去前导零〕, 小数位〔去尾随零〕)`。
type ExactParts = (bool, String, String);

/// 客户端排序的一格**排序键**。
///
/// 比较只读键、**不在比较函数里重算**：一万行的一次排序要做十几万次比较，每次都
/// `Value::render()`（内含整串复制）会把「点一下列头」拖成肉眼可见的卡顿 —— 键在
/// 排序前一次性建好，比较阶段零分配。
///
/// 结构照「先分类、再同族比」的口径：
///
/// | 档 | 谁进来 | 怎么比 |
/// | --- | --- | --- |
/// | [`Self::Exact`] | `Int` / `UInt` / `Decimal` | 十进制字面量精确比较（**绝不经过 `f64`**） |
/// | [`Self::Float`] | `Float`，以及「本列出现过浮点」时的全部数值 | `f64` 比较（`NaN` 最小） |
/// | [`Self::Text`] | 文本 / 日期 / 时间 / JSON / 布尔 / 二进制 | 展示文本的**小写**序 |
/// | [`Self::Null`] | `NULL`、越界取不到的格 | 恒排最后（升序时） |
///
/// 同一列里混了类型（SQL 结果集完全允许）时按 [`Self::rank`] 的组序排：
/// 数值 → 文本 → `NULL`。组内才按各自的口径比。
///
/// ⚠️ 数值族**不走 `f64`** 是本仓的硬纪律（`i64` 大数与 `NUMERIC(38)` 过浮点会
/// 失序，而排错的表比不排更误导）。只有「本列真有浮点」时才统一抬到 `f64` ——
/// 那是唯一可比的口径，且浮点类值本身就是 `f64`，不算有损中转。
enum SortKey {
    Exact(ExactParts),
    Float(f64),
    Text(String),
    Null,
}

impl SortKey {
    /// 跨类型的组序（数值 → 文本 → NULL）。
    const fn rank(&self) -> u8 {
        match self {
            Self::Exact(_) | Self::Float(_) => 0,
            Self::Text(_) => 1,
            Self::Null => 2,
        }
    }
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        use SortKey::{Exact, Float, Null, Text};
        match (self, other) {
            (Exact(left), Exact(right)) => exact_cmp(left, right),
            // `NaN` 在这里被当成「与谁都不等」会让排序失去全序（`sort_by` 要求
            // 全序），统一压成 `Equal` 参与比较、由下一档（行号）分先后。
            (Float(left), Float(right)) => left.partial_cmp(right).unwrap_or(Ordering::Equal),
            (Text(left), Text(right)) => left.cmp(right),
            (Null, Null) => Ordering::Equal,
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SortKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for SortKey {}

/// 两格的精确十进制比较：先看符号，再比整数位（长度 → 逐位），最后补零比小数位。
fn exact_cmp(left: &ExactParts, right: &ExactParts) -> Ordering {
    match (left.0, right.0) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let magnitude = left
        .1
        .len()
        .cmp(&right.1.len())
        .then_with(|| left.1.cmp(&right.1))
        .then_with(|| cmp_fraction(&left.2, &right.2));
    // 同号负数：绝对值大的更小。
    if left.0 {
        magnitude.reverse()
    } else {
        magnitude
    }
}

/// 小数位比较：短的按 `0` 补到等长再逐位比（`0.5` 与 `0.50` 相等）。
fn cmp_fraction(left: &str, right: &str) -> Ordering {
    for index in 0..left.len().max(right.len()) {
        let x = left.as_bytes().get(index).copied().unwrap_or(b'0');
        let y = right.as_bytes().get(index).copied().unwrap_or(b'0');
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// 拆一个十进制字面量；不是十进制（科学计数法、`NaN` / `Infinity`）时给 `None`。
fn exact_parts(text: &str) -> Option<ExactParts> {
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty()
        || digits.matches('.').count() > 1
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '.')
    {
        return None;
    }
    let (integer, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let integer = integer.trim_start_matches('0');
    Some((
        negative,
        if integer.is_empty() {
            "0".to_owned()
        } else {
            integer.to_owned()
        },
        fraction.trim_end_matches('0').to_owned(),
    ))
}

/// 一格的排序键；`floats` = 本列出现过浮点（见 [`SortKey`] 的口径）。
fn sort_key(value: Option<&Value>, floats: bool) -> SortKey {
    let Some(value) = value else {
        return SortKey::Null;
    };
    let text = match value {
        Value::Null => return SortKey::Null,
        Value::Float(number) => return SortKey::Float(*number),
        Value::Int(number) => number.to_string(),
        Value::UInt(number) => number.to_string(),
        Value::Decimal(text) => text.clone(),
        other => return SortKey::Text(other.render().to_lowercase()),
    };
    if floats {
        return text
            .trim()
            .parse::<f64>()
            .map_or(SortKey::Null, SortKey::Float);
    }
    exact_parts(&text).map_or_else(|| SortKey::Text(text.to_lowercase()), SortKey::Exact)
}

/// 按某一列**重排行下标**（客户端排序，SQL 结果集的列头）。
///
/// 只动下标、不动行数据：结果集仍是那**一份**（导出 / 编辑 / 多选都按原始行集走），
/// 排序改变的是「网格按什么顺序看它」——与「结果内搜索」「客户端分页」同一层语义。
///
/// 口径三条：
///
/// 1. `NULL` **恒在最后**（升序时）——`NULL` 不是「最小的值」，是「没有值」，
///    混在数字堆里会让人以为库里真有这个数；
/// 2. 降序 = 升序结果**整体取反**：`NULL` 因此排到最前，与 PostgreSQL 的默认
///    `NULLS FIRST` 行为一致（同一个直觉，不是两套规则）；
/// 3. 同键行按**原行号**定序（`then_with`）：全序、结果可复现 —— 只按键排的话
///    同值行的先后由排序算法决定，翻页 / 重排时看着像在跳。
pub(super) fn sort_row_indexes(
    rows: &[Vec<Value>],
    indexes: &mut [usize],
    column: usize,
    descending: bool,
) {
    // 「本列有没有浮点」先扫一遍（O(行数)）：有就把**全部**数值抬到 `f64`，
    // 免得在比较函数里反复解析一遍（那是 O(比较次数)，多得多）。
    let floats = rows
        .iter()
        .any(|row| matches!(row.get(column), Some(Value::Float(_))));
    let mut keyed: Vec<(SortKey, usize)> = indexes
        .iter()
        .map(|index| {
            let key = sort_key(rows.get(*index).and_then(|row| row.get(column)), floats);
            (key, *index)
        })
        .collect();
    keyed.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    for (slot, (_, index)) in indexes.iter_mut().zip(keyed) {
        *slot = index;
    }
    if descending {
        indexes.reverse();
    }
}

/// 结果内搜索的命中行：升序的**原始行下标**（空词 / 纯空白 = 不过滤，全部命中）。
///
/// 只搜**可见列**（搜到了却看不见命中来源的行比搜不到更让人糊涂）；匹配文本与
/// 网格单元格同源（[`Value::render`]，NULL 也算一行文本），大小写不敏感。
///
/// 口径与「字段筛选」一致：搜索**只影响网格显示**——多选 / 行编辑 / 导出仍按完整
/// 行集走，搜索是找行用的，不是删行用的。
pub(super) fn matched_row_indexes(
    rows: &[Vec<Value>],
    visible: &[(usize, Column)],
    needle: &str,
) -> Vec<usize> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return (0..rows.len()).collect();
    }
    rows.iter()
        .enumerate()
        .filter(|(_index, row)| {
            visible.iter().any(|(cell_index, _column)| {
                row.get(*cell_index)
                    .is_some_and(|value| value.render().to_lowercase().contains(&needle))
            })
        })
        .map(|(index, _row)| index)
        .collect()
}

/// 把一段 SQL 压成**单行**（换行与连续空白都并成一个空格，首尾不留）。
///
/// 只给**显示**用（读数条里的来源语句 chip）—— 复制与执行都走原文。按「空白折叠」
/// 而不是「换行换空格」做，是因为 SQL 的换行后面通常还跟着缩进，逐字符替换会留下
/// 一长串空格。
pub(super) fn one_line_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 结果网格行首的多选复选框（表头全选 / 每行勾选共用）。
///
/// 纯图标没有可访问名，所以名字必须**显式**给（`aria_label`）：AccessKit 的
/// `CheckBox` 只认 `aria_toggled` 与显式名字，读不出 `check_mark` 里那个图标。
pub(super) fn grid_checkbox(
    id: SharedString,
    state: CheckState,
    label: SharedString,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let hover = theme.extended().colors.hover_tint;
    let press = theme.extended().colors.hover_tint_press;
    let ring = theme.tokens().colors.ring;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(GRID_ROW_HEIGHT))
        .rounded(theme.tokens().radius.sm)
        .cursor_pointer()
        .role(Role::CheckBox)
        .aria_label(label)
        .aria_toggled(match state {
            CheckState::Checked => Toggled::True,
            CheckState::Unchecked => Toggled::False,
            CheckState::Indeterminate => Toggled::Mixed,
        })
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(press))
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.border_1().border_color(ring))
        .on_click(on_click)
        .child(check_mark(state, cx))
        .into_any_element()
}

// ── SQL 补全（CompletionProvider）───────────────────────────────────────────
//
// 词表**不在这里**：「这个引擎该认识哪些词」是 SQL 知识，落在
// `duck_db::sqlwords`（通用核心 + 方言增量，外加 MongoDB / Elasticsearch 各自的
// 词表）。这一层只管三件事：拿当前引擎的词表 → 按光标前缀过滤 → 贴菜单分类。

/// 候选是否命中查询词（大小写不敏感：前缀命中排前由调用方排序，这里只判命中）。
pub(super) fn matches_query(candidate: &str, query: &str) -> bool {
    query.is_empty() || candidate.to_lowercase().contains(&query.to_lowercase())
}

/// 审计摘要里放的那一行 SQL：折掉换行、按字符截断。
///
/// 不放全文：审计是**明文落盘**的一张表（会被读、会被复制出去），一条几百行的脚本就能把
/// 它撑成日志文件；而「一眼认出是哪条语句」只需要开头这一截 —— 要精确比对有**指纹**
/// （同一形状的语句指纹相同，正文里的字面量本来就该被抹掉）。
pub(super) fn audit_sql_summary(sql: &str) -> String {
    /// 摘要的字符上限（中文按字符算，不是字节）。
    const SUMMARY_LIMIT: usize = 80;
    let flat: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SUMMARY_LIMIT {
        return flat;
    }
    let head: String = flat.chars().take(SUMMARY_LIMIT).collect();
    format!("{head}…")
}

/// 详情页分区标题：图标 + 大写字距标题 + 通栏 hairline（dbx 的 SERVER / AUTHENTICATION 同形）。
pub(super) fn db_section_header(icon: DuckIcon, label: &'static str, cx: &App) -> gpui::Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    h_flex()
        .w_full()
        .flex_none()
        .items_center()
        .gap(tokens.spacing.xs)
        .pb(tokens.spacing.xs)
        .border_b_1()
        .border_color(tokens.colors.border)
        .child(
            Icon::new(icon)
                .size(theme.extended().icon.sm)
                .text_color(tokens.colors.muted_foreground),
        )
        .child(
            // 英文分区名走 ASCII，无需进 i18n 表（与「BETA」角标同一口径）。
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.colors.muted_foreground)
                .child(label),
        )
}

/// 校验「自定义行数」输入并取出页大小；非法时弹警告并给 `None`。
///
/// 弹层内联输入框与旧弹窗共用同一套判据（正整数、至少 1），文案同源。
pub(super) fn parse_page_size_input(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
) -> Option<u32> {
    let Ok(size) = input.read(cx).value().trim().parse::<u32>() else {
        notify::warning(tr(cx, "请输入一个行数（正整数）。"), window, cx);
        return None;
    };
    if size == 0 {
        notify::warning(tr(cx, "每页行数至少是 1。"), window, cx);
        return None;
    }
    Some(size)
}

/// 页大小变更回调：`(新页大小, 是否落盘为默认, window, cx)`。
///
/// 抽成别名只为让 [`page_size_custom_section`] 的签名读得动 —— 裸写
/// `Rc<dyn Fn(u32, bool, &mut Window, &mut App)>` 会被 clippy 的
/// `type_complexity` 点出来（两个调用点也照此收窄）。
pub(super) type PageSizeApply = Rc<dyn Fn(u32, bool, &mut Window, &mut App)>;

/// 「N 行/页」弹层里的**自定义行数**区（dbx grid-foot 同款）：标题 +
/// 读数行（可选）+ 输入框与 ✓，外加一行动作按钮。
///
/// `session_label` = `Some("仅本次")` 时出「仅本次 / 设为默认」两枚按钮
/// （查询模式：预设与 ✓ 都只改本次会话）；表浏览是服务器侧取数，没有
/// 「本次」这一层，按钮只出「设为默认」，✓ 直接写默认。
/// `readout` 是「当前 / 默认」那行读数 —— 表浏览的当前值就是默认值，两栏
/// 相同没有信息量，调用方给 `None` 时整行不出。
///
/// ⚠️ 整块必须拦住点击冒泡：菜单行（`PopupMenuItem::ElementItem`）的默认
/// 行为是「点行即关弹层」，不拦的话点进输入框弹层就没了（与顶栏语言切换
/// 拦鼠标按下同一条约定）。`close` 在动作成功后关弹层（发 `DismissEvent`，
/// 由 `dropdown_menu` 的监听收尾）。
pub(super) fn page_size_custom_section(
    input: &Entity<InputState>,
    readout: Option<SharedString>,
    session_label: Option<SharedString>,
    on_apply: PageSizeApply,
    close: Rc<dyn Fn(&mut App)>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let check_persists = session_label.is_none();
    v_flex()
        .id("db-page-size-custom")
        .gap(tokens.spacing.xs)
        .py(tokens.spacing.xxs)
        // 输入框 / 按钮的点击不能冒泡到菜单行（点行即关），见上方 ⚠️。
        .on_click(|_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .text_size(tokens.typography.sm.size)
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.colors.foreground)
                .child(tr(cx, "自定义行数")),
        )
        .children(readout.map(|readout| meta_text(readout, cx)))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(input)
                            .appearance(true)
                            .with_size(Size::Small)
                            .text_size(tokens.typography.sm.size)
                            .w_full(),
                    ),
                )
                .child(
                    // ✓ = 快捷确认：查询模式按「仅本次」，表浏览直接写默认
                    //（`check_persists` 随 `session_label` 推导，见上方）。
                    icon_button(
                        "db-page-size-check",
                        tr(cx, "应用"),
                        Icon::new(DuckIcon::CheckboxCheck).size(theme.extended().icon.sm),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click({
                        let input = input.clone();
                        let on_apply = on_apply.clone();
                        let close = close.clone();
                        move |_, window, cx| {
                            let Some(size) = parse_page_size_input(&input, window, cx) else {
                                return;
                            };
                            on_apply(size, check_persists, window, cx);
                            close(cx);
                        }
                    }),
                ),
        )
        .children(session_label.map(|label| {
            button("db-page-size-session", label, ButtonVariant::Secondary, cx)
                .flex_1()
                .on_click({
                    let input = input.clone();
                    let on_apply = on_apply.clone();
                    let close = close.clone();
                    move |_, window, cx| {
                        let Some(size) = parse_page_size_input(&input, window, cx) else {
                            return;
                        };
                        on_apply(size, false, window, cx);
                        close(cx);
                    }
                })
        }))
        .child(
            button(
                "db-page-size-default",
                tr(cx, "设为默认"),
                ButtonVariant::Primary,
                cx,
            )
            .flex_1()
            .on_click({
                let input = input.clone();
                let on_apply = on_apply.clone();
                let close = close.clone();
                move |_, window, cx| {
                    let Some(size) = parse_page_size_input(&input, window, cx) else {
                        return;
                    };
                    on_apply(size, true, window, cx);
                    close(cx);
                }
            }),
        )
        .into_any_element()
}

/// 确认框「同意」回调。
///
/// 用 `Rc<dyn Fn>` 而不是泛型 `FnOnce`：弹层的构建闭包是 `Fn`（每一轮重排都会
/// 重跑），要在里面再克隆一份给按钮回调，`FnOnce` 拿不出来。抽成别名也让签名
/// 不必内联那个复杂类型。
type ConfirmHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// 破坏性动作的**主题确认框**（`duck_components` 的 `Dialog`，不是 rfd 系统模态）。
///
/// 为什么不用 `duck_platform::confirm`：那是**同步**的系统模态（原生窗），与本应用
/// 的主题 / 字号 / 深浅色完全两套外观，而且它把 UI 线程钉在原地等用户点 —— 弹窗
/// 期间整页不重绘。主题确认框是**异步**的：函数立刻返回，真正要做的事整段搬进
/// `on_ok`。
///
/// 标题 / 正文 / 按钮文案在调用时就取成 `SharedString`：`tr` 要 `&App`，而弹层重排
/// 的闭包里只有 `&mut App` 的弹层上下文，取不到「当前语言」那一刻。
pub(super) fn open_confirm(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    ok_text: impl Into<SharedString>,
    on_ok: ConfirmHandler,
    window: &mut Window,
    cx: &mut App,
) {
    let title: SharedString = title.into();
    let message: SharedString = message.into();
    let ok_text: SharedString = ok_text.into();
    let drag = modal_drag(cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        // 外层闭包是 `Fn`（每次重排都重跑）→ 每一轮都得重新克隆一份交给内层。
        let drag_for_content = drag.clone();
        let title_for_content = title.clone();
        let message_for_content = message.clone();
        let on_ok_for_ok = on_ok.clone();
        dialog
            .w(px(CONFIRM_DIALOG_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    // `ok_text` 按值收：弹层闭包是 `Fn`，这里必须克隆一份出去。
                    .ok_text(ok_text.clone())
                    .cancel_text(tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    v_flex()
                        .w_full()
                        .child(modal_title_row(
                            "db-confirm-title",
                            &drag_for_content,
                            title_for_content.clone(),
                            cx,
                        ))
                        .child(body_text(message_for_content.clone(), cx)),
                ))
            })
            .on_ok(move |_, window, cx| {
                // 确认之后才真正动手；视图已销毁（`update` 返回 `Err`）也照样关弹窗，
                // 不让一个点不掉的确认框挂在屏幕上。
                on_ok_for_ok(window, cx);
                true
            })
    });
}

/// 编辑表单的**指纹**：列名 + 展示原值 + NULL 位。
///
/// 回答的问题是「这份表单服务的那一行还是不是原来那一行」。结果重取 / 翻页 / 换表
/// 之后，同一个下标已经指向另一行数据 —— 表单里的草稿不能跨行存活，所以对不上就
/// 整份重建（见 [`DatabasePage::sync_json_editor`]）。
///
/// 比「行号相同」严：行号相同但内容换了（翻页回第一页、重跑同一句 SQL）同样要重建。
pub(super) fn specs_stamp(specs: &[(String, String, bool)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    specs.hash(&mut hasher);
    hasher.finish()
}
