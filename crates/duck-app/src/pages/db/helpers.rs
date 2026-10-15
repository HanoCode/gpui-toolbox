//! 跨领域复用的自由函数：kv 档案读取、模块偏好取值、数值与时长格式化、
//! 命名空间推导、标签重名去重、结果集纯计算（列宽 / 命中行 / 分页排序 / 表格模板）。
//!
//! 本文件的函数**不持有页面状态**：要么纯参数进出，要么只收 `&App` / `&Context<Self>`
//! 读全局状态。页面状态在 `mod.rs`，领域行为在各自的子模块。

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
pub(super) fn build_table_model(result: &ResultSet) -> TableModel {
    let columns = result
        .columns
        .iter()
        .map(|column| ColumnSpec::new(column.name.clone(), column.type_name.clone()))
        .collect();
    let rows = result
        .rows
        .iter()
        .map(|row| RowData::new(row.iter().map(table_cell).collect()))
        .collect();
    TableModel::new(columns, rows)
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
