use super::*;

// 日期部分的解析**复用** `duck-project` 的 `CalendarDate`（本仓既有的严格 `YYYY-MM-DD`
// 解析器，projects 页也在用）：自己再写一份「从 civil 日期算 epoch 天数」的算法，
// 就是第二个真相源。时刻部分（`HH:MM:SS`）那边没有，所以在下面自己解析。
use duck_project::CalendarDate;

impl ChartKind {
    /// 六档全量（菜单顺序）。
    ///
    /// 顺序是「一列一张」那四档（折线 / 柱状 / 面积 / 饼）在前，后加的两档在后 ——
    /// 下拉里既有的四项位置不动，习惯了的用户不必重新找。
    pub(super) const ALL: [Self; 6] = [
        Self::Line,
        Self::Bar,
        Self::Area,
        Self::Pie,
        Self::Scatter,
        Self::StackedBar,
    ];

    /// 展示文案。
    pub(super) fn heading(self, cx: &App) -> &'static str {
        match self {
            Self::Line => tr(cx, "折线图"),
            Self::Bar => tr(cx, "柱状图"),
            Self::Area => tr(cx, "面积图"),
            Self::Pie => tr(cx, "饼图"),
            Self::Scatter => tr(cx, "散点图"),
            Self::StackedBar => tr(cx, "堆叠柱状图"),
        }
    }

    /// 下拉选项 / 持久化用的稳定英文键（**不翻译**，同 [`InstanceTab::key`] 理由）。
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Bar => "bar",
            Self::Area => "area",
            Self::Pie => "pie",
            Self::Scatter => "scatter",
            Self::StackedBar => "stacked_bar",
        }
    }

    /// 从稳定键还原（下拉回投用；认不出时给 `None`，不猜）。
    pub(super) fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// 同时能画几条数值序列。
    ///
    /// 饼图只吃**一列**：多列拼进一张饼是「苹果 + 橘子」的和，读不出任何东西，所以
    /// 饼图档下选另一列 = 换掉这一条。
    ///
    /// 其余五档封顶 [`CHART_SERIES_LIMIT`]，但**用法不同**：折线 / 柱状 / 面积 / 散点
    /// 是一列一张小图（一屏碎图，剩下的列留在表里不删）；堆叠柱把这几列**叠进同一根柱**，
    /// 所以这个上限对它是「一根柱最多堆几段」。
    pub(super) const fn series_limit(self) -> usize {
        match self {
            Self::Pie => 1,
            Self::Line | Self::Bar | Self::Area | Self::Scatter | Self::StackedBar => {
                CHART_SERIES_LIMIT
            }
        }
    }
}

impl ChartConfig {
    /// 按结果集给一份能直接画的默认配置（`x` 是第一个非数值列，`y` 是数值列）。
    pub(super) fn from_result(result: &ResultSet) -> Option<Self> {
        let (x, y) = default_chart_columns(result)?;
        Some(Self {
            kind: ChartKind::Line,
            x,
            y,
        })
    }

    /// 这份配置在当前结果集上还能不能用（列名都还在）。
    pub(super) fn is_valid_for(&self, result: &ResultSet) -> bool {
        let has = |name: &str| result.columns.iter().any(|column| column.name == name);
        has(&self.x) && !self.y.is_empty() && self.y.iter().all(|name| has(name))
    }

    /// 按类型截断到能画的序列数（饼图只留一条）。
    pub(super) fn effective_y(&self) -> &[String] {
        let limit = self.kind.series_limit();
        let end = self.y.len().min(limit);
        &self.y[..end]
    }

    /// 当前横轴在给定结果集上的轴类型。
    ///
    /// 列已经不在结果里时给分类档（读数行照样写得出来，不必为「配置失效」再造一句文案）。
    pub(super) fn axis_kind(&self, result: &ResultSet) -> ChartAxisKind {
        result
            .columns
            .iter()
            .position(|column| column.name == self.x)
            .map_or(ChartAxisKind::Categorical, |index| {
                chart_axis_kind(result, index)
            })
    }
}

impl SavedChart {
    /// 生成一个新 id（范式同 [`SavedQuery::next_id`]）。
    pub(super) fn next_id(existing: &[Self]) -> String {
        let mut seq = existing.len() + 1;
        loop {
            let candidate = format!("chart-{seq}");
            if !existing.iter().any(|saved| saved.id == candidate) {
                return candidate;
            }
            seq += 1;
        }
    }

    /// 取出视图配置（已保存图表与实时配置共用一份）。
    pub(super) fn config(&self) -> ChartConfig {
        ChartConfig {
            kind: self.kind,
            x: self.x.clone(),
            y: self.y.clone(),
        }
    }
}

impl Dashboard {
    /// 生成一个新 id：现有最大 +1（删除后**不复用**，免得旧面板 id 撞上）。
    pub(super) fn next_id(existing: &[Self]) -> u32 {
        existing.iter().map(|saved| saved.id).max().unwrap_or(0) + 1
    }

    /// 复制一份（id / 名字另起，面板原样搬过去）。
    ///
    /// 名字由调用方给定：新名字里的「副本」二字是要翻译的文案，而**内核侧不产面向用户的
    /// 散文**这条纪律在这里同样适用 —— 本结构不做 `tr`，由界面层拼好再传进来。
    pub(super) fn duplicate(&self, id: u32, name: String) -> Self {
        Self {
            id,
            name,
            panels: self.panels.clone(),
        }
    }

    /// 把第 `index` 块面板与相邻的 `delta` 位换位（`±1`；越界不动）。
    ///
    /// 12 栏**流式**网格里「左 / 右 / 上 / 下」是同一件事（都只是顺序上前挪或后挪），
    /// 所以只给这一个动作，界面上也就只画两个按钮。
    pub(super) fn move_panel(&mut self, index: usize, delta: i32) {
        let Some(target) = index.checked_add_signed(delta as isize) else {
            return;
        };
        if target < self.panels.len() {
            self.panels.swap(index, target);
        }
    }

    /// 下一个可用的面板 id（同样不回收已删除的号）。
    pub(super) fn next_panel_id(&self) -> u32 {
        self.panels.iter().map(|panel| panel.id).max().unwrap_or(0) + 1
    }
}

/// `span` 字段的 serde 默认值（写不出 `DASHBOARD_DEFAULT_SPAN` 作为属性参数，包一层）。
pub(super) fn default_panel_span() -> u32 {
    DASHBOARD_DEFAULT_SPAN
}

/// 12 栏网格的**流式打包**：按面板顺序从左往右铺，铺不下就换行。
///
/// 返回「每行有哪些面板下标」。用流式而不是自由 `(x, y)` 定位：自由定位要么额外做一套
/// 碰撞 / 吸附规则，要么允许重叠；而本页的编辑动作只有「宽窄 / 前移后移」两样，
/// 流式网格下这两样都是确定行为（不会出现两块面板叠在一起这种"看上去排好了"的坏状态）。
pub(super) fn pack_dashboard_rows(spans: &[u32]) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut row: Vec<usize> = Vec::new();
    let mut used = 0_u32;
    for (index, raw) in spans.iter().copied().enumerate() {
        let span = clamp_span(raw);
        if !row.is_empty() && used + span > DASHBOARD_COLUMNS {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(index);
        used += span;
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// 跨度收口到 1..=12 栏（kv 是外部可改的，越界值在这里拦掉而不是让布局算出负数宽度）。
pub(super) fn clamp_span(span: u32) -> u32 {
    span.clamp(1, DASHBOARD_COLUMNS)
}

/// 面板在网格里**实际**占的栏数。
///
/// 分隔标题一律整行：它是给面板分组用的横条，半行宽就失去「这一段属于它」的意思。
/// 布局与宽度都走这一个函数，不会出现「存的是 6 栏、画的是 12 栏」这种两处不一致。
pub(super) fn dashboard_panel_span(panel: &DashboardPanel) -> u32 {
    match panel.kind {
        DashboardPanelKind::Divider { .. } => DASHBOARD_COLUMNS,
        _ => clamp_span(panel.span),
    }
}

/// 一栏宽度占整行的比例（`span / 12`），给相对宽度用。
pub(super) fn column_fraction(span: u32) -> f32 {
    clamp_span(span) as f32 / DASHBOARD_COLUMNS as f32
}

/// 哪些面板当前可见：被折叠的分隔条把它**下面直到下一个分隔条**的面板挡掉。
///
/// 与 DBFlux 同口径。分隔条自己始终可见 —— 否则折起来之后就没东西可以点开了。
pub(super) fn visible_dashboard_panels(
    panels: &[DashboardPanel],
    collapsed: &BTreeSet<u32>,
) -> Vec<usize> {
    let mut visible = Vec::new();
    let mut hidden = false;
    for (index, panel) in panels.iter().enumerate() {
        if matches!(panel.kind, DashboardPanelKind::Divider { .. }) {
            // 每个分隔条只掌自己那一段：见到它就按它自己的折叠态重设。
            hidden = collapsed.contains(&panel.id);
            visible.push(index);
            continue;
        }
        if !hidden {
            visible.push(index);
        }
    }
    visible
}

/// 从结果集挑默认的图表列：`x` 取**最左的时间列**，没有时间列就取第一个非数值列
/// （全数值就退回第一列）；`y` 取前 [`CHART_SERIES_LIMIT`] 个数值列。
///
/// 「时间列优先」与 DBFlux 的 `detect_chart_columns` 同口径（它取最左的 `Timestamp` 列）：
/// 结果里既有日期又有别的文本列时（`SELECT created_at, status, amount …`），
/// 想看的走势几乎总是随**日期**变化的，拿 `status` 当轴画出来的图没人要。
///
/// 没有任何数值列时返回 `None`——那说明这张表画不出图，界面该说清楚而不是画一张空图。
pub(super) fn default_chart_columns(result: &ResultSet) -> Option<(String, Vec<String>)> {
    let numeric: Vec<bool> = (0..result.columns.len())
        .map(|index| column_is_numeric(result, index))
        .collect();
    let x_index = result
        .columns
        .iter()
        .enumerate()
        .find(|(index, _)| chart_axis_kind(result, *index) == ChartAxisKind::Time)
        // 退一步：第一个非数值列（分类轴）。
        .or_else(|| {
            result
                .columns
                .iter()
                .enumerate()
                .find(|(index, _)| !numeric[*index])
        })
        // 全是数值时用第一列当轴。
        .or_else(|| result.columns.iter().enumerate().next())
        .map(|(index, _)| index)?;
    let x = result.columns[x_index].name.clone();
    let y: Vec<String> = result
        .columns
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != x_index && numeric[*index])
        .map(|(_, column)| column.name.clone())
        .take(CHART_SERIES_LIMIT)
        .collect();
    if y.is_empty() {
        return None;
    }
    Some((x, y))
}

/// 一列是不是数值列。
///
/// 不看引擎报的 `type_name`：各引擎口径不一（SQLite 是动态类型、ClickHouse 一堆
/// `Nullable(...)` 包装），拿它当判据会漏判；**值**才是真凭据。空值跳过（NULL 不代表
/// 非数值），见到第一个非数值非空的值就判否。
pub(super) fn column_is_numeric(result: &ResultSet, index: usize) -> bool {
    let mut saw_number = false;
    for row in result.rows.iter().take(NUMERIC_PROBE_ROWS) {
        match row.get(index) {
            None | Some(Value::Null) => continue,
            Some(Value::Int(_) | Value::UInt(_) | Value::Float(_)) => saw_number = true,
            Some(Value::Decimal(text)) => {
                if text.trim().parse::<f64>().is_ok() {
                    saw_number = true;
                } else {
                    return false;
                }
            }
            // 布尔画成数是误导（`true` 出 1 还是 0 都有理由），一律不当数值。
            Some(_) => return false,
        }
    }
    saw_number
}

/// 图表横轴的**类型** —— 决定「点怎么排」与「标签怎么写」两件事。
///
/// 判据只看**值**，不看引擎报的 `type_name`（与 [`column_is_numeric`] 同一条纪律：
/// 各引擎口径不一，`Text` 里装的完全可能就是日期）。DBFlux 的同位概念是 `AxisKind::Time`
/// / `AxisKind::Numeric`，区别是它读驱动填的 `ColumnKind`，而本仓驱动一律把日期**渲染成
/// 字符串**（`duck-db/src/result.rs` 的模块注释写明这是刻意设计，且没有任何驱动构造
/// `Value::Date/Time/DateTime`），所以这里按值的形状认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChartAxisKind {
    /// 时间轴：按时间升序排点，标签紧凑成 `MM-DD HH:MM`。
    Time,
    /// 数值轴：按数值升序排点。
    Numeric,
    /// 分类轴：**保持结果顺序**（分类的先后是 SQL 给的，排一下就是换了意思）。
    Categorical,
}

impl ChartAxisKind {
    /// 读数行里那三个字（稳定英文键见 [`Self::key`]）。
    pub(super) fn heading(self, cx: &App) -> &'static str {
        match self {
            Self::Time => tr(cx, "时间"),
            Self::Numeric => tr(cx, "数值"),
            Self::Categorical => tr(cx, "分类"),
        }
    }
}

/// 一个可画的点。
///
/// 三件事合成一个载体、而不是三个并行 `Vec`：标签（给人看）、Y 值（画高度）、
/// 横轴位置（排序与抽稀用）。排序与抽稀都必须按**同一个**横轴口径算，拆开就迟早错位。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChartPoint {
    /// 展示用的标签（时间轴已按紧凑格式重写）。
    pub(super) label: String,
    /// Y 值。
    pub(super) value: f64,
    /// 横轴上的**可比较位置**：时间 = 自 epoch 秒，数值 = 数值本身，分类 = 行序。
    pub(super) axis: f64,
}

/// 判一列适合当哪种横轴。
///
/// 只探开头 [`NUMERIC_PROBE_ROWS`] 行（与 [`column_is_numeric`] 同一个采样口径）：
/// 表可能几十万行，定性用不着扫完。判定要求**这一列是齐的** —— 见到一个非时间的值就退回
/// 分类轴，宁可少认一类，也不要把「大部分是日期、夹几条脏数据」的列当成时间轴排序
/// （那会把脏行甩到图的两端，看起来像真的极值）。
///
/// 布尔 / 二进制 / JSON 一律分类：它们当横轴时读者看的是「哪一类」，不是「谁大谁小」。
pub(super) fn chart_axis_kind(result: &ResultSet, index: usize) -> ChartAxisKind {
    let mut seen = 0_usize;
    let mut times = 0_usize;
    for row in result.rows.iter().take(NUMERIC_PROBE_ROWS) {
        match row.get(index) {
            None | Some(Value::Null) => continue,
            Some(Value::Int(_) | Value::UInt(_) | Value::Float(_)) => {
                return ChartAxisKind::Numeric;
            }
            Some(Value::Bool(_) | Value::Bytes(_) | Value::Json(_)) => {
                return ChartAxisKind::Categorical;
            }
            Some(Value::Date(_) | Value::Time(_) | Value::DateTime(_)) => {
                seen += 1;
                times += 1;
            }
            Some(Value::Decimal(text)) => {
                if text.trim().parse::<f64>().is_ok() {
                    return ChartAxisKind::Numeric;
                }
                seen += 1;
                if time_seconds(text).is_some() {
                    times += 1;
                }
            }
            Some(Value::Text(text)) => {
                seen += 1;
                if time_seconds(text).is_some() {
                    times += 1;
                }
            }
        }
    }
    if seen > 0 && seen == times {
        ChartAxisKind::Time
    } else {
        ChartAxisKind::Categorical
    }
}

/// 把 `YYYY-MM-DD[ HH:MM[:SS]]` / `YYYY-MM-DDTHH:MM[:SS]` / `HH:MM[:SS]` 拆成（日期, 时刻）。
///
/// 「有没有日期」只按 `T` / 空格切一刀、再看剩下那段是不是 10 位带两个 `-` —— 不认的
/// 就整串当日期，交给 [`CalendarDate::parse`] 去否掉。刻意**不**做宽松识别：宽宽松松地
/// 把 `2026-1-8` 也认下来，等于让两种写法在同一个轴上表示两个不同时刻。
fn split_datetime(text: &str) -> (&str, &str) {
    if let Some(index) = text.find([' ', 'T']) {
        return (&text[..index], &text[index + 1..]);
    }
    if text.len() == 10 && text.as_bytes().get(4) == Some(&b'-') {
        (text, "")
    } else {
        ("", text)
    }
}

/// 恰好两位数字（`HH` / `MM` / `SS` 共用）。位数不固定就不认（见 [`split_datetime`]）。
fn two_digits(text: &str) -> Option<i64> {
    if text.len() != 2 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `HH:MM[:SS[.fff]]` → 当日秒数。
fn time_of_day_seconds(text: &str) -> Option<i64> {
    let mut parts = text.split(':');
    let hour = two_digits(parts.next()?)?;
    let minute = two_digits(parts.next()?)?;
    let second = match parts.next() {
        Some(part) => {
            let (whole, fraction) = match part.split_once('.') {
                Some((whole, fraction)) => (whole, Some(fraction)),
                None => (part, None),
            };
            // 小数位要么没有，要么全是数字；空的小数点（`07.`）也不认。
            if let Some(fraction) = fraction
                && (fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()))
            {
                return None;
            }
            two_digits(whole)?
        }
        None => 0,
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(hour * 3600 + minute * 60 + second)
}

/// 时间文本 → 自 epoch 秒（**UTC 口径**）。
///
/// 只用来**排序与抽稀**，不做任何时区换算 —— 轴的先后顺序在哪个时区都一样，而一旦
/// 掺进本地偏移，「同一列里两行谁在前」就会随机器设置变，那种图没法比对。
///
/// 纯时刻（`HH:MM:SS`）以 epoch 当天为基准：同一列里只可能是同一种形状，差一个常数
/// 不影响先后。
///
/// `pub(super)`：纯函数层的**可测面**（`db/tests.rs` 直接压它的严格性），与同文件其余
/// 纯函数同档；拆出来的 [`split_datetime`] / [`time_of_day_seconds`] 是实现细节，保持私有。
pub(super) fn time_seconds(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date_part, time_part) = split_datetime(text);
    // ⚠️ 空串必须在这里挡掉：SQLite 常拿 `''` 代替 NULL，而下面两段都为空时
    // `days * 86400 + seconds` 会算出**合法的 0**（= epoch 那一刻），于是每个空值
    // 都变成一个落在轴最左端的点 —— 正是「把脏行甩到一端」那种错。
    if date_part.is_empty() && time_part.is_empty() {
        return None;
    }
    let days = if date_part.is_empty() {
        0
    } else {
        CalendarDate::parse(date_part)?.days_since_epoch()
    };
    let seconds = if time_part.is_empty() {
        0
    } else {
        time_of_day_seconds(time_part)?
    };
    Some(days * 86_400 + seconds)
}

/// 时间标签紧凑化：`2026-10-15 22:39:07` → `10-15 22:39`。
///
/// 只在**已判为时间轴**时用（见 [`chart_axis_kind`]）：轴标签的格子就几十像素宽，
/// 带年份的完整时间戳必然被截成半截，反而读不出是几号。年份不进标签的理由是
/// 「同一条轴上的点几乎总在同一年」——真跨年了，读数行里的横轴列名还在，去表格看得到。
///
/// 纯时刻列原样返回：那时候时间本身就是全部信息，砍掉时分就什么都不剩了。
///
/// `pub(super)`：同 [`time_seconds`]，是纯函数层的可测面。
pub(super) fn compact_time_label(text: &str) -> String {
    let text = text.trim();
    let (date_part, time_part) = split_datetime(text);
    if date_part.len() != 10 {
        return text.to_owned();
    }
    let short_date = &date_part[5..];
    let hour_minute: Vec<&str> = time_part.split(':').take(2).collect();
    let hour_minute = hour_minute.join(":");
    if hour_minute.is_empty() {
        short_date.to_owned()
    } else {
        format!("{short_date} {hour_minute}")
    }
}

/// 把结果集的两列抽成「横轴 → 数值」序列。
///
/// 跳过两类行：横轴为 `NULL` 的（分类轴上一个叫「NULL」的桶是噪声，真要看那几行去表格
/// 视图）与数值非数的（画不进坐标系）。**不补 0**：补 0 会在图上画出一段实际不存在的
/// 「跌到底」，比断开更难解释。
///
/// ⚠️ **时间 / 数值轴会按轴值升序重排**。理由是「折线图看走势」的前提就是横轴单调：
/// `SELECT *` 的行序并不保证按时间给（不写 `ORDER BY` 时引擎可以任意给），照原序连点
/// 会画出一条来回折的锯齿，看上去像数据在剧烈震荡。分类轴**不排** —— 分类的先后是
/// SQL 给的，动一下就是换了意思。
///
/// 选中列在当前结果集里不存在时返回空表——调用方据此显示「配置失效」而不是画空图。
pub(super) fn chart_series(result: &ResultSet, x: &str, y: &str) -> Vec<ChartPoint> {
    let column_index = |name: &str| result.columns.iter().position(|column| column.name == name);
    let (Some(xi), Some(yi)) = (column_index(x), column_index(y)) else {
        return Vec::new();
    };
    let kind = chart_axis_kind(result, xi);
    let mut points: Vec<ChartPoint> = result
        .rows
        .iter()
        .enumerate()
        .filter_map(|(row_index, row)| {
            let axis_cell = row.get(xi)?;
            if axis_cell.is_null() {
                return None;
            }
            let raw = axis_cell.render();
            let value = chart_number(row.get(yi)?)?;
            let axis = match kind {
                // 已经判为时间轴，可单行仍解析不出（超过采样窗口的那几行）就**丢弃**：
                // 给它编一个「第 n 行」的位置会把它插到时间轴的中间，比不画更误导。
                ChartAxisKind::Time => time_seconds(&raw)? as f64,
                ChartAxisKind::Numeric => chart_number(axis_cell)?,
                ChartAxisKind::Categorical => row_index as f64,
            };
            let label = match kind {
                ChartAxisKind::Time => compact_time_label(&raw),
                _ => raw,
            };
            Some(ChartPoint { label, value, axis })
        })
        .collect();
    if kind != ChartAxisKind::Categorical {
        points.sort_by(|left, right| {
            left.axis
                .partial_cmp(&right.axis)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    points
}

/// LTTB（Largest-Triangle-Three-Buckets）抽稀：把 `points` 降到至多 `limit` 个点。
///
/// 首尾**必留**（横轴两端是边界，抽掉就不是这张图了），中间按「与已选的上一块、和下一块
/// 的均值构成的三角形面积最大」来选 —— 面积大 = 离直线远 = 是转折点。等距抽样做不到这
/// 一点：它会把峰谷一起抹平，而峰谷恰恰是看图要找的东西。
///
/// 为什么必须抽：结果区一屏 ≈ 500 像素宽（见 [`CHART_MAX_POINTS`]），十万个点铺进去会
/// 叠成一条实心带，还画得慢。DBFlux 的同位实现是 `chart/decimate.rs`。
///
/// 数据不足或 `limit < 3` 时退化：前者原样返回，后者只留首尾（少于三个点构不成三角形，
/// 硬按算法走会除零）。
pub(super) fn decimate_lttb(points: Vec<ChartPoint>, limit: usize) -> Vec<ChartPoint> {
    if limit >= points.len() {
        return points;
    }
    if limit < 3 {
        // 少于三个点构不成三角形，硬走算法会除零。要一个给第一个，要两个给首尾。
        return match limit {
            0 => Vec::new(),
            1 => vec![points[0].clone()],
            _ => vec![points[0].clone(), points[points.len() - 1].clone()],
        };
    }
    let last = points.len() - 1;
    let every = (points.len() - 2) as f64 / (limit - 2) as f64;
    let mut sampled: Vec<ChartPoint> = Vec::with_capacity(limit);
    sampled.push(points[0].clone());
    let mut anchor = 0_usize;
    for bucket in 0..limit - 2 {
        // 下一块的均值点：三角形面积里的第三个顶点。
        let avg_start = ((bucket + 1) as f64 * every).floor() as usize + 1;
        let avg_end = (((bucket + 2) as f64 * every).floor() as usize + 1).min(points.len());
        let (mut avg_axis, mut avg_value, mut count) = (0.0_f64, 0.0_f64, 0_usize);
        for point in points.iter().take(avg_end).skip(avg_start) {
            avg_axis += point.axis;
            avg_value += point.value;
            count += 1;
        }
        if count == 0 {
            avg_axis = points[last].axis;
            avg_value = points[last].value;
        } else {
            avg_axis /= count as f64;
            avg_value /= count as f64;
        }
        // 本块的候选范围；右端不越过 `last`，最后一个点要留给收尾。
        let from = (bucket as f64 * every).floor() as usize + 1;
        let to = (((bucket + 1) as f64 * every).floor() as usize + 1).min(last);
        let previous = &points[anchor];
        let mut best = from;
        let mut best_area = -1.0_f64;
        for index in from..to.max(from) {
            let candidate = &points[index];
            let area = ((previous.axis - avg_axis) * (candidate.value - previous.value)
                - (previous.axis - candidate.axis) * (avg_value - previous.value))
                .abs();
            if area > best_area {
                best_area = area;
                best = index;
            }
        }
        anchor = best;
        sampled.push(points[best].clone());
    }
    sampled.push(points[last].clone());
    sampled
}

/// X 轴最多标几个刻度：点越多标得越稀（`x_tick_count` / `band_tick_count` 都是
/// 「均匀标 N 个」），否则几百个分类标签会糊成一条灰带。
///
/// 下限 2 是因为 1 个刻度等于没有坐标感（只有一条光轴）；上限 8 是窄栏能容下的
/// 刻度标签数（结果区一张图约半屏宽，再密就互相盖住）。
pub(super) fn axis_tick_count(points: usize) -> usize {
    points.clamp(2, 8)
}

/// 一格的数值（能画进坐标系的才算）；非数返回 `None`。
pub(super) fn chart_number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(v) => Some(*v as f64),
        Value::UInt(v) => Some(*v as f64),
        Value::Float(v) if v.is_finite() => Some(*v),
        Value::Decimal(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 饼图配色：以强调色为起点按**黄金角**旋转色相，切片多了也不容易撞色。
///
/// 不用 gpui-component 自带的 `chart.1..5`：那是它自己主题里的蓝系五档，与 duck 的
/// 主题色不一路；按色相旋转则由本主题的强调色派生，明暗模式都跟着走。
pub(super) fn chart_palette(base: Hsla, count: usize) -> Vec<Hsla> {
    /// 黄金角（度）：连续切片之间的色相间隔，任意前 n 个都分布均匀。
    const GOLDEN_ANGLE: f32 = 137.508;
    (0..count)
        .map(|index| {
            let hue = (base.h * 360.0 + GOLDEN_ANGLE * index as f32).rem_euclid(360.0);
            Hsla {
                h: hue / 360.0,
                ..base
            }
        })
        .collect()
}

/// 按类型造一张图（六档的构造差异只集中在这一处）。
///
/// 数据一律 `Vec<ChartPoint>`；饼图另外带上下标（配色要按切片顺序取）。
/// 返回 `None` = 这份数据画不出来（饼图全是非正值），调用方据此给一句说明。
///
/// 两条实现路：折线 / 柱状 / 面积 / 饼走 `gpui-component` 的现成组件（悬停提示、动画、
/// 坐标轴都是白拿的）；**散点 / 堆叠柱走自绘**（见 `charts_plot`，那两档组件给不了）。
/// 两路的入参口径完全一样，所以这里只是多两个 `match` 分支，调用方不必知道差别。
///
/// 刻度密度由点数算（[`axis_tick_count`]）：`x_tick_count` / `band_tick_count` 是
/// 「均匀标 N 个」的杠杆，不管的话几百个分类标签会自己糊成一排。
pub(super) fn chart_element(
    kind: ChartKind,
    points: Vec<ChartPoint>,
    key: &str,
    style: &PlotStyle,
) -> Option<AnyElement> {
    let accent = style.accent;
    let id = SharedString::from(format!("db-chart-{key}"));
    let ticks = axis_tick_count(points.len());
    let element = match kind {
        ChartKind::Line => LineChart::new(points)
            .id(id)
            .x(|point| point.label.clone())
            .y(|point| point.value)
            .stroke(accent)
            .grid(true)
            .x_tick_count(ticks)
            .y_tick_format(compact_number)
            .tooltip_title(|point| SharedString::from(point.label.clone()))
            .tooltip_value(|_, value| SharedString::from(compact_number(value)))
            .into_any_element(),
        ChartKind::Bar => BarChart::new(points)
            .id(id)
            .band(|point| point.label.clone())
            .value(|point| point.value)
            .fill(move |_, _, _, _| accent)
            .grid(true)
            .band_tick_count(ticks)
            // 柱状图的「带」就是分类，值轴要自己画刻度（否则只有柱高没有刻度）。
            .value_axis(true)
            .value_tick_format(compact_number)
            .tooltip_title(|point| SharedString::from(point.label.clone()))
            .tooltip_value(|_, value| SharedString::from(compact_number(value)))
            .into_any_element(),
        ChartKind::Area => AreaChart::new(points)
            .id(id)
            .x(|point| point.label.clone())
            .y(|point| point.value)
            .stroke(accent)
            .fill(accent.opacity(0.22))
            .grid(true)
            .x_tick_count(ticks)
            .y_tick_format(compact_number)
            .tooltip_title(|point| SharedString::from(point.label.clone()))
            .tooltip_value(|_, _, value| SharedString::from(compact_number(value)))
            .into_any_element(),
        ChartKind::Pie => {
            // 饼图画的是占比：非正值（负数 / 零）在饼上没有位置，先滤掉。
            let slices: Vec<(String, f64, usize)> = points
                .into_iter()
                .filter(|point| point.value > 0.0)
                .enumerate()
                .map(|(index, point)| (point.label, point.value, index))
                .collect();
            if slices.is_empty() {
                return None;
            }
            let palette = chart_palette(accent, slices.len());
            PieChart::new(slices)
                .id(id)
                .value(|(_, value, _)| *value as f32)
                .color(move |(_, _, index)| palette.get(*index).copied().unwrap_or(accent))
                .label(|(label, _, _)| SharedString::from(label.clone()))
                .label_color(accent)
                .inner_radius(0.5)
                .tooltip_name(|(label, _, _)| SharedString::from(label.clone()))
                .tooltip_value(|_, _, value| SharedString::from(compact_number(f64::from(value))))
                .into_any_element()
        }
        // ── 下面两档是自绘的（`charts_plot`）──
        ChartKind::Scatter => scatter_element(points, style),
        // 单列走到这儿时「堆叠」没有可叠的对象，结果就是一根占满格的柱 —— 这正是对的，
        // 所以不必另给一句说明。多列的堆叠由 `stacked_panel` 把几列一起送给
        // `stacked_element`，那条路才真的在叠。
        ChartKind::StackedBar => stacked_element(vec![(String::new(), points)], style),
    };
    Some(element)
}

/// 把一段 SQL 压成**单行**并截断（仪表盘「最近查询」的行标签用）。
///
/// 不直接把多行 SQL 塞进导航行：行高是固定的，多行文本会被裁成半截，看起来像乱码。
pub(super) fn one_line(sql: &str) -> String {
    let mut out = String::new();
    for word in sql.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= 60 {
            out.push('…');
            break;
        }
    }
    out
}

impl DatabasePage {
    /// 把已保存图表写回 kv（单键结构化数组）。
    pub(super) fn persist_saved_charts(&self, cx: &App) {
        match serde_json::to_string(&self.saved_charts) {
            Ok(json) => duck_store::kv_put(cx, KEY_SAVED_CHARTS, json),
            Err(error) => tracing::warn!(%error, "serialize db saved charts failed"),
        }
    }

    /// 把用户仪表板写回 kv（单键结构化数组）。
    pub(super) fn persist_dashboards(&self, cx: &App) {
        match serde_json::to_string(&self.dashboards) {
            Ok(json) => duck_store::kv_put(cx, KEY_DASHBOARDS, json),
            Err(error) => tracing::warn!(%error, "serialize db dashboards failed"),
        }
    }

    /// 在表格与图表之间切换结果区。
    ///
    /// 第一次切进图表时按结果集**自动选列**（见 [`ChartConfig::from_result`]）；没有
    /// 可画的数值列就如实说不画，而不是切过去给一张空图。切回表格保留配置（下次切回
    /// 来还是同一张图），配置随 [`SessionView`] 按标签走。
    pub(super) fn toggle_chart_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chart_view.is_some() {
            self.chart_view = None;
            cx.notify();
            return;
        }
        let Some(result) = self.current_result() else {
            notify::info(tr(cx, "当前没有可画图的结果。"), window, cx);
            return;
        };
        let Some(config) = ChartConfig::from_result(result) else {
            notify::info(tr(cx, "这份结果里没有数值列，画不出图。"), window, cx);
            return;
        };
        self.chart_view = Some(config);
        cx.notify();
    }

    /// 改图表类型（保列映射；饼图只留第一条序列）。
    pub(super) fn set_chart_kind(&mut self, kind: ChartKind, cx: &mut Context<Self>) {
        if let Some(config) = self.chart_view.as_mut() {
            config.kind = kind;
            let limit = kind.series_limit();
            config.y.truncate(config.y.len().min(limit));
            cx.notify();
        }
    }

    /// 改横轴列。
    ///
    /// 同一列若已经在序列里就把它摘掉（「轴 = 值」画出来是一条对角线，没有意义），
    /// 但**至少留一条序列**：摘光了这份配置就失效，`render_chart` 会整份回落到默认映射，
    /// 用户刚选的轴会被一起抹掉——那比多画一条对角线更难理解。
    pub(super) fn set_chart_axis(&mut self, column: String, cx: &mut Context<Self>) {
        if let Some(config) = self.chart_view.as_mut() {
            if config.y.len() > 1 {
                config.y.retain(|name| name != &column);
            }
            config.x = column;
            cx.notify();
        }
    }

    /// 切换某个数值列的勾选（序列选择）。
    ///
    /// 三种情形分开：已选 → 摘掉；单选类（饼图）没选 → **换成这一列**；多选类到顶 →
    /// 不动（界面把到顶的未选项画成停用态，用户看得见为什么点不动）。
    pub(super) fn toggle_chart_series(&mut self, column: &str, cx: &mut Context<Self>) {
        let Some(config) = self.chart_view.as_mut() else {
            return;
        };
        let capacity = config.kind.series_limit();
        if let Some(index) = config.y.iter().position(|name| name == column) {
            config.y.remove(index);
        } else if capacity == 1 {
            config.y = vec![column.to_owned()];
        } else if config.y.len() < capacity {
            config.y.push(column.to_owned());
        } else {
            return;
        }
        cx.notify();
    }

    /// 把当前图表（SQL + 列映射 + 类型）存成一条已保存图表（打开命名弹窗）。
    pub(super) fn open_save_chart_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.chart_view.clone() else {
            notify::info(tr(cx, "先切到图表视角再保存。"), window, cx);
            return;
        };
        let sql = self.sql.read(cx).value().trim().to_owned();
        if sql.is_empty() {
            notify::info(tr(cx, "编辑器里还没有 SQL。"), window, cx);
            return;
        }
        // 默认名取首行前若干字符（与保存查询同口径：给一个能改的占位，不是「未命名」）。
        let suggestion: String = sql
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(24)
            .collect();
        self.open_chart_name_dialog(None, suggestion, sql, config, window, cx);
    }

    /// 给一条已保存图表改名（左栏右键「重命名」）。
    pub(super) fn open_rename_chart_dialog(
        &mut self,
        id: &str,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.saved_charts.iter().find(|saved| saved.id == id) else {
            return;
        };
        let sql = saved.sql.clone();
        let config = saved.config();
        self.open_chart_name_dialog(
            Some(id.to_owned()),
            current.to_owned(),
            sql,
            config,
            window,
            cx,
        );
    }

    /// 「命名图表」弹窗（新建与重命名共用；`target` 为 `None` 即新建）。
    pub(super) fn open_chart_name_dialog(
        &mut self,
        target: Option<String>,
        initial: String,
        sql: String,
        config: ChartConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, "图表名称"));
        if !initial.is_empty() {
            let seed = initial.clone();
            input.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    notify::warning(tr(cx, "请填写图表名称。"), window, cx);
                    return false;
                }
                let sql = sql.clone();
                let config = config.clone();
                let target = target.clone();
                let _ = view.update(cx, |this, cx| {
                    match target {
                        // 改名：只动名字（与「重命名查询」同口径，不覆盖列映射）。
                        Some(id) => {
                            if let Some(saved) =
                                this.saved_charts.iter_mut().find(|saved| saved.id == id)
                            {
                                saved.name = name;
                            }
                        }
                        None => {
                            let id = SavedChart::next_id(&this.saved_charts);
                            this.saved_charts.push(SavedChart {
                                id,
                                name,
                                sql,
                                kind: config.kind,
                                x: config.x,
                                y: config.y,
                            });
                        }
                    }
                    this.persist_saved_charts(cx);
                    this.status = tr(cx, "已保存图表。").into();
                    cx.notify();
                });
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let confirm = Rc::clone(&confirm);
            let input_for_content = input.clone();
            dialog
                .w(px(GROUP_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .gap(form_gap(cx))
                            .child(modal_title_row(
                                "db-saved-chart-title",
                                &drag_for_content,
                                tr(cx, "保存图表"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "名称"), &input_for_content)),
                    ))
                })
                .footer(dialog_footer(
                    "db-saved-chart",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 载入一条已保存图表：SQL 放回编辑器（**不执行**）+ 切到图表视角用同一份配置。
    ///
    /// 与已保存查询的差别只有「顺手把图表视角打开」。SQL 放回但不跑，所以图表要等到
    /// 用户执行之后才有数据——那时配置已经就位，直接出图。
    pub(super) fn load_saved_chart(
        &mut self,
        saved_id: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.saved_charts.iter().find(|saved| saved.id == saved_id) else {
            return;
        };
        let sql = saved.sql.clone();
        let config = saved.config();
        self.chart_view = Some(config);
        self.fill_editor(sql, window, cx);
        self.status = trf(cx, "已载入图表「{}」，执行后即可看图。", &[name]).into();
        cx.notify();
    }

    /// 删除一条已保存图表（带二次确认：删了取不回来）。
    pub(super) fn delete_saved_chart(
        &mut self,
        id: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmed = duck_platform::confirm(
            tr(cx, "删除已保存图表"),
            &trf(cx, "「{}」将被删除，确定继续吗？", &[name]),
        );
        if !confirmed {
            return;
        }
        self.saved_charts.retain(|saved| saved.id != id);
        self.persist_saved_charts(cx);
        self.status = tr(cx, "已删除。").into();
        notify::success(tr(cx, "已删除该图表。"), window, cx);
        cx.notify();
    }

    /// 打开一块仪表板：落在**仪表板标签**里、拉一遍面板数据。
    ///
    /// 复用策略与库 / 实例标签同款：同连接同板已开就只激活，没有才新开 ——
    /// 点仪表板不再把当前标签正看着的内容整个盖掉。连接 id 由调用方显式传入
    /// （入口行长在某条连接下面）。
    ///
    /// 进来一律是**查看态**（对齐 DBFlux）：仪表板多半是回来看的，先看；想改再按编辑。
    pub(super) fn open_dashboard(
        &mut self,
        conn_id: &str,
        id: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.dashboards.iter().any(|board| board.id == id) {
            return;
        }
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        let kind = TabKind::Dashboard(id);
        if let Some(existing) =
            find_reusable_tab(&self.tabs, &self.tab_kind, &self.tab_conn, conn_id, &kind)
        {
            self.activate(&existing, window, cx);
            return;
        }
        self.spawn_tab_for(conn_id, kind, window, cx);
        self.mode = Mode::Dashboard(id);
        // 新标签是干净快照：选中行 / 行编辑器 / 图表视角 / 编辑态都不带过来。
        self.dashboard_editing = false;
        self.refresh_dashboard(cx);
    }

    /// 重取当前仪表板上所有要服务器数据的面板。
    ///
    /// 一次 `spawn_db` 跑完所有面板（不是一板一枪）：连接后面是一把锁，并发发枪只会
    /// 排在锁上；攒成一批还能让「引用的图表已被删除」这种**当场可判**的结果和
    /// 真取数的结果一起落位，界面只会闪一次。
    pub(super) fn refresh_dashboard(&mut self, cx: &mut Context<Self>) {
        let Mode::Dashboard(board_id) = self.mode else {
            return;
        };
        let Some(conn_id) = self.active.clone() else {
            return;
        };
        let Some(board) = self
            .dashboards
            .iter()
            .find(|board| board.id == board_id)
            .cloned()
        else {
            return;
        };
        // 这块板在这个连接上的旧结果先清掉：面板删了 / 换了内容之后不该留着上一份。
        self.dashboard_results
            .retain(|(conn, dash, _), _| conn != &conn_id || *dash != board_id);

        let Some(session) = self.sessions.get(&conn_id) else {
            return;
        };
        let kind = session.kind;
        let conn = session.conn.clone();
        // 折线 / 柱状这些面板的 SQL 是**替用户跑**的，该过只读闸门。但
        // [`duck_db::sqlintel::prove_read_only`] 靠解析 SQL，MongoDB / Elasticsearch 的
        // shell / DSL 语句永远证明不了——一律拒绝等于让这两条引擎的图表面板彻底不能用。
        // 判据复用 `identifier_quote()`（它就是「这条引擎接不接 SQL」的同源口径），
        // 非 SQL 引擎退回与编辑器手动执行**同一条** `execute` 路径：面板的语句本就是
        // 用户自己存下来的，不存在"替用户写库"。
        let sql_engine = kind.identifier_quote().is_some();

        // 先把「要干什么」算成清单：后台闭包不该再读界面状态。
        let mut jobs: Vec<(u32, DashboardJob)> = Vec::new();
        for panel in &board.panels {
            match &panel.kind {
                DashboardPanelKind::Chart { saved_chart_id } => {
                    match self
                        .saved_charts
                        .iter()
                        .find(|chart| &chart.id == saved_chart_id)
                    {
                        Some(chart) => jobs.push((panel.id, DashboardJob::Sql(chart.sql.clone()))),
                        None => {
                            self.dashboard_results.insert(
                                (conn_id.clone(), board_id, panel.id),
                                PanelFetch::MissingChart,
                            );
                        }
                    }
                }
                DashboardPanelKind::Inspector { view } => {
                    jobs.push((panel.id, DashboardJob::Inspector(view.view())));
                }
                DashboardPanelKind::Divider { .. } => {}
            }
        }
        if jobs.is_empty() {
            cx.notify();
            return;
        }
        let board_name = board.name.clone();
        self.spawn_db(
            cx,
            move || -> Vec<(u32, PanelFetch)> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                jobs.into_iter()
                    .map(|(panel_id, job)| {
                        let fetch = match job {
                            DashboardJob::Sql(sql) => {
                                let outcome = if sql_engine {
                                    guard.execute_read_only(&sql, DASHBOARD_ROW_LIMIT)
                                } else {
                                    guard.execute(&sql, DASHBOARD_ROW_LIMIT)
                                };
                                match outcome {
                                    Ok(outcome) => match outcome.result {
                                        Some(result) => PanelFetch::Chart(result),
                                        None => PanelFetch::NoRows,
                                    },
                                    Err(error) => PanelFetch::Failed(error.to_string()),
                                }
                            }
                            DashboardJob::Inspector(view) => match guard.instance_table(view) {
                                Ok(table) => PanelFetch::Table(table),
                                Err(error) => PanelFetch::Failed(error.to_string()),
                            },
                        };
                        (panel_id, fetch)
                    })
                    .collect()
            },
            move |this, fetches, _window, cx| {
                for (panel_id, fetch) in fetches {
                    this.dashboard_results
                        .insert((conn_id.clone(), board_id, panel_id), fetch);
                }
                this.status = trf(cx, "已刷新仪表板「{}」。", &[&board_name]).into();
                cx.notify();
            },
        );
    }

    /// 把一块面板的改动落盘（所有面板级编辑动作共用的收尾）。
    pub(super) fn commit_dashboard(&self, cx: &App) {
        self.persist_dashboards(cx);
    }

    /// 找一块面板。
    pub(super) fn dashboard_panel_mut(
        &mut self,
        board_id: u32,
        panel_id: u32,
    ) -> Option<&mut DashboardPanel> {
        self.dashboards
            .iter_mut()
            .find(|board| board.id == board_id)?
            .panels
            .iter_mut()
            .find(|panel| panel.id == panel_id)
    }

    /// 往当前仪表板加一块面板（返回新面板 id，调用方接着做「改标题」这类一步动作）。
    ///
    /// 分隔条一律整行（见 [`dashboard_panel_span`]），所以它的 `span` 直接存满栏 ——
    /// 存成别的值也不会生效，不如存成真实生效的那个，kv 读出来不会自相矛盾。
    pub(super) fn add_dashboard_panel(
        &mut self,
        board_id: u32,
        kind: DashboardPanelKind,
        cx: &mut Context<Self>,
    ) -> Option<u32> {
        let span = match kind {
            DashboardPanelKind::Divider { .. } => DASHBOARD_COLUMNS,
            _ => DASHBOARD_DEFAULT_SPAN,
        };
        let board = self
            .dashboards
            .iter_mut()
            .find(|board| board.id == board_id)?;
        let id = board.next_panel_id();
        board.panels.push(DashboardPanel { id, span, kind });
        self.commit_dashboard(cx);
        // 新面板的数据得先取到手（图表面板 / 检查器面板），否则摆上去是一块空壳。
        self.refresh_dashboard(cx);
        cx.notify();
        Some(id)
    }

    /// 移除一块面板。
    pub(super) fn remove_dashboard_panel(
        &mut self,
        board_id: u32,
        panel_id: u32,
        cx: &mut Context<Self>,
    ) {
        if let Some(board) = self
            .dashboards
            .iter_mut()
            .find(|board| board.id == board_id)
        {
            board.panels.retain(|panel| panel.id != panel_id);
        }
        // 折叠状态与取回的数据跟着走：面板没了，这两份也不该留。
        self.dashboard_collapsed.remove(&panel_id);
        self.dashboard_results
            .retain(|(_, dash, panel), _| *dash != board_id || *panel != panel_id);
        self.commit_dashboard(cx);
        cx.notify();
    }

    /// 改一块面板的宽度（`delta` 栏；收口在 1..=12）。
    pub(super) fn set_dashboard_span(
        &mut self,
        board_id: u32,
        panel_id: u32,
        span: u32,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.dashboard_panel_mut(board_id, panel_id) {
            panel.span = clamp_span(span);
        }
        self.commit_dashboard(cx);
        cx.notify();
    }

    /// 前挪 / 后挪一块面板（12 栏流式网格里「左右上下」是同一件事）。
    pub(super) fn move_dashboard_panel(
        &mut self,
        board_id: u32,
        panel_id: u32,
        delta: i32,
        cx: &mut Context<Self>,
    ) {
        let Some(board) = self
            .dashboards
            .iter_mut()
            .find(|board| board.id == board_id)
        else {
            return;
        };
        let Some(index) = board.panels.iter().position(|panel| panel.id == panel_id) else {
            return;
        };
        board.move_panel(index, delta);
        self.commit_dashboard(cx);
        cx.notify();
    }

    /// 换掉图表面板引用的已保存图表。
    pub(super) fn set_dashboard_chart(
        &mut self,
        board_id: u32,
        panel_id: u32,
        chart_id: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.dashboard_panel_mut(board_id, panel_id) {
            panel.kind = DashboardPanelKind::Chart {
                saved_chart_id: chart_id,
            };
        }
        self.commit_dashboard(cx);
        // 整板重取（而不是只重取这一块）：连接后面是一把锁，多跑一次往返换不来什么，
        // 而「哪块数据是哪次取的」只留一种情况，界面不会出现新旧混排。
        self.refresh_dashboard(cx);
        cx.notify();
    }

    /// 换掉检查器面板看的那张表。
    pub(super) fn set_dashboard_inspector(
        &mut self,
        board_id: u32,
        panel_id: u32,
        view: InspectorView,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.dashboard_panel_mut(board_id, panel_id) {
            panel.kind = DashboardPanelKind::Inspector { view };
        }
        self.commit_dashboard(cx);
        self.refresh_dashboard(cx);
        cx.notify();
    }

    /// 折叠 / 展开一条分隔标题（它统领它下面那一段）。
    pub(super) fn toggle_dashboard_divider(&mut self, panel_id: u32, cx: &mut Context<Self>) {
        if !self.dashboard_collapsed.insert(panel_id) {
            self.dashboard_collapsed.remove(&panel_id);
        }
        cx.notify();
    }

    /// 新建 / 重命名仪表板（`None` = 新建，`Some` = 改名）。
    pub(super) fn open_dashboard_name_dialog(
        &mut self,
        target: Option<u32>,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, "仪表板名称"));
        if !initial.is_empty() {
            let seed = initial.clone();
            input.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    notify::warning(tr(cx, "请填写仪表板名称。"), window, cx);
                    return false;
                }
                // `target` 是 `Option<u32>`（`Copy`），闭包是 `Fn` 也能直接读。
                let mut created = None;
                let _ = view.update(cx, |this, cx| {
                    match target {
                        // 改名：只动名字（面板与取回的数据都不动）。
                        Some(id) => {
                            if let Some(board) =
                                this.dashboards.iter_mut().find(|board| board.id == id)
                            {
                                board.name = name;
                            }
                        }
                        None => {
                            let id = Dashboard::next_id(&this.dashboards);
                            this.dashboards.push(Dashboard {
                                id,
                                name,
                                panels: Vec::new(),
                            });
                            created = Some(id);
                        }
                    }
                    this.commit_dashboard(cx);
                    this.status = tr(cx, "已保存仪表板。").into();
                    cx.notify();
                });
                // 新建的当场打开：用户按「新建」的意思是「现在就开始摆」。
                // 连接取当前活跃的那条（新建入口长在连接节点下，活跃连接即语境）。
                if let Some(id) = created {
                    let _ = view.update(cx, |this, cx| {
                        if let Some(conn) = this.active.clone() {
                            this.open_dashboard(&conn, id, window, cx);
                        }
                    });
                }
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let confirm = Rc::clone(&confirm);
            let input_for_content = input.clone();
            dialog
                .w(px(GROUP_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .gap(form_gap(cx))
                            .child(modal_title_row(
                                "db-dashboard-title",
                                &drag_for_content,
                                tr(cx, "仪表板名称"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "名称"), &input_for_content)),
                    ))
                })
                .footer(dialog_footer(
                    "db-dashboard-name",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 改分隔标题的正文（分隔条的内容就一行字，也走弹窗以免和面板里的按钮抢焦点）。
    pub(super) fn open_divider_title_dialog(
        &mut self,
        board_id: u32,
        panel_id: u32,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, "分隔标题"));
        if !initial.is_empty() {
            let seed = initial.clone();
            input.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let title = input.read(cx).value().trim().to_owned();
                if title.is_empty() {
                    notify::warning(tr(cx, "请填写分隔标题。"), window, cx);
                    return false;
                }
                let _ = view.update(cx, |this, cx| {
                    if let Some(panel) = this.dashboard_panel_mut(board_id, panel_id) {
                        panel.kind = DashboardPanelKind::Divider { title };
                    }
                    this.commit_dashboard(cx);
                    this.status = tr(cx, "已保存分隔标题。").into();
                    cx.notify();
                });
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let confirm = Rc::clone(&confirm);
            let input_for_content = input.clone();
            dialog
                .w(px(GROUP_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .gap(form_gap(cx))
                            .child(modal_title_row(
                                "db-divider-title",
                                &drag_for_content,
                                tr(cx, "分隔标题"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "标题"), &input_for_content)),
                    ))
                })
                .footer(dialog_footer(
                    "db-divider-name",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 复制一块仪表板（面板照搬，名字加「副本」）。
    pub(super) fn duplicate_dashboard(&mut self, id: u32, cx: &mut Context<Self>) {
        let Some(source) = self.dashboards.iter().find(|board| board.id == id).cloned() else {
            return;
        };
        let new_id = Dashboard::next_id(&self.dashboards);
        let name = trf(cx, "{} 副本", &[&source.name]);
        self.dashboards.push(source.duplicate(new_id, name));
        self.persist_dashboards(cx);
        self.status = tr(cx, "已复制仪表板。").into();
        cx.notify();
    }

    /// 删除一块仪表板（正在看这块的话退回 SQL 结果区）。
    pub(super) fn delete_dashboard(
        &mut self,
        id: u32,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmed = duck_platform::confirm(
            tr(cx, "删除仪表板"),
            &trf(cx, "「{}」将被删除，确定继续吗？", &[name]),
        );
        if !confirmed {
            return;
        }
        let panels: Vec<u32> = self
            .dashboards
            .iter()
            .find(|board| board.id == id)
            .map(|board| board.panels.iter().map(|panel| panel.id).collect())
            .unwrap_or_default();
        self.dashboards.retain(|board| board.id != id);
        for panel in panels {
            self.dashboard_collapsed.remove(&panel);
        }
        self.dashboard_results.retain(|(_, dash, _), _| *dash != id);
        // 这块板的标签一起收掉（可能不止一张？不会 —— 同连接同板只留一张，但
        // 收集后再关的顺序不能省：`close_tab` 会改 `self.tabs`，边遍历边关会漏）。
        let stale: Vec<String> = self
            .tabs
            .iter()
            .filter(|tab| self.tab_kind.get(*tab) == Some(&TabKind::Dashboard(id)))
            .cloned()
            .collect();
        for tab in stale {
            self.close_tab(&tab, window, cx);
        }
        self.persist_dashboards(cx);
        self.status = tr(cx, "已删除。").into();
        notify::success(tr(cx, "已删除该仪表板。"), window, cx);
        cx.notify();
    }

    /// 仪表板页：页头（查看 / 编辑、刷新、加面板）+ 12 栏网格。
    ///
    /// 网格按**流式**铺：每行的栏数加起来不超过 12（[`pack_dashboard_rows`]），面板宽窄
    /// 用相对宽度表示（`span / 12`），所以换个窗口宽度不会错位。面板之间的空隙由每块
    /// 面板自己的内边距让出来 —— 而不是给行加 `gap`，那样一行的总宽会超过 100%。
    pub(super) fn render_dashboard_page(
        &self,
        board_id: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(board) = self.dashboards.iter().find(|board| board.id == board_id) else {
            return instance_hint(tr(cx, "这块仪表板已经不在了。"), cx);
        };
        let conn = self.active.clone().unwrap_or_default();
        let editing = self.dashboard_editing;

        let mut header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                Icon::new(DuckIcon::Columns2)
                    .size(extended.icon.lg)
                    .text_color(extended.colors.accent_text),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(tokens.typography.md.size)
                            .line_height(tokens.typography.md.line_height)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(board.name.clone())),
                    )
                    .child(meta_text(
                        if editing {
                            tr(cx, "编辑中：面板可以改宽、前挪后挪、换内容。")
                        } else {
                            tr(cx, "12 栏网格")
                        },
                        cx,
                    )),
            );

        // 「添加面板」只在编辑态出现：查看态不该长出一排编辑按钮。
        if editing {
            let chart_ids: Vec<(String, String)> = self
                .saved_charts
                .iter()
                .map(|chart| (chart.id.clone(), chart.name.clone()))
                .collect();
            let supports_instance = self
                .active_session()
                .is_some_and(|session| session.kind.supports_instance_insight());
            header = header.child(toolbar_dropdown_button(
                "db-dashboard-add",
                tr(cx, "添加面板"),
                Icon::new(DuckIcon::Plus)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.muted_foreground),
                Anchor::BottomRight,
                {
                    let view = view.clone();
                    move |menu, _, cx| {
                        // 图表面板默认落在**第一条**已保存图表上：加完面板在面板自己的
                        // 下拉里换（那条下拉列全量），比在这里再套一层选择器少一次点击。
                        // 一条都没有时这一项禁用 —— 「点了才知道不行」不如直接不给点。
                        let add_chart = chart_ids.first().map(|(id, _)| id.clone());
                        let chart_view = view.clone();
                        let divider_view = view.clone();
                        let inspector_view = view.clone();
                        menu.item(
                            PopupMenuItem::new(tr(cx, "图表面板"))
                                .disabled(add_chart.is_none())
                                .on_click(move |_, _, cx| {
                                    let Some(chart_id) = add_chart.clone() else {
                                        return;
                                    };
                                    let _ = chart_view.update(cx, |this, cx| {
                                        this.add_dashboard_panel(
                                            board_id,
                                            DashboardPanelKind::Chart {
                                                saved_chart_id: chart_id,
                                            },
                                            cx,
                                        );
                                    });
                                }),
                        )
                        .item(PopupMenuItem::new(tr(cx, "分隔标题")).on_click(
                            move |_, window, cx| {
                                let _ = divider_view.update(cx, |this, cx| {
                                    let panel_id = this.add_dashboard_panel(
                                        board_id,
                                        DashboardPanelKind::Divider {
                                            title: tr(cx, "分组").to_owned(),
                                        },
                                        cx,
                                    );
                                    // 加完直接让用户命名：占位名「分组」只是为了让面板
                                    // 先立住（不命名的话框里是空的，像坏了）。
                                    if let Some(panel_id) = panel_id {
                                        this.open_divider_title_dialog(
                                            board_id,
                                            panel_id,
                                            String::new(),
                                            window,
                                            cx,
                                        );
                                    }
                                });
                            },
                        ))
                        // 引擎没这个能力就禁用（与左栏那三个入口同一判据）：画一个点开
                        // 只会报「不支持」的面板，和画一个点不动的按钮一样糟。
                        .item(
                            PopupMenuItem::new(tr(cx, "实例检查器"))
                                .disabled(!supports_instance)
                                .on_click(move |_, _, cx| {
                                    let _ = inspector_view.update(cx, |this, cx| {
                                        this.add_dashboard_panel(
                                            board_id,
                                            DashboardPanelKind::Inspector {
                                                view: InspectorView::Metrics,
                                            },
                                            cx,
                                        );
                                    });
                                }),
                        )
                    }
                },
                cx,
            ));
        }

        header = header.child(
            icon_text_button(
                "db-dashboard-refresh",
                tr(cx, "刷新"),
                Icon::new(DuckIcon::RotateCw)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Ghost,
                cx,
            )
            .disabled(self.busy)
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.refresh_dashboard(cx));
                }
            }),
        );
        header = header.child(if editing {
            button(
                "db-dashboard-view",
                tr(cx, "完成编辑"),
                ButtonVariant::Primary,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.dashboard_editing = false;
                        cx.notify();
                    });
                }
            })
        } else {
            button(
                "db-dashboard-edit",
                tr(cx, "编辑"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.dashboard_editing = true;
                        cx.notify();
                    });
                }
            })
        });

        // ── 网格 ──
        let visible = visible_dashboard_panels(&board.panels, &self.dashboard_collapsed);
        let spans: Vec<u32> = visible
            .iter()
            .map(|index| dashboard_panel_span(&board.panels[*index]))
            .collect();
        let mut grid = v_flex().w_full().gap(tokens.spacing.xs);
        if visible.is_empty() {
            grid = grid.child(
                empty_state(
                    DuckIcon::Columns2,
                    tr(cx, "这块仪表板还是空的"),
                    Some(
                        tr(
                            cx,
                            "点右上角「编辑」，再点「添加面板」摆上图表、分隔标题或实例检查器。",
                        )
                        .into(),
                    ),
                    None,
                    cx,
                )
                .w_full()
                .into_any_element(),
            );
        }
        for row in pack_dashboard_rows(&spans) {
            let mut line = h_flex().w_full().items_stretch();
            for slot in row {
                let panel = &board.panels[visible[slot]];
                line = line.child(
                    div()
                        .w(relative(column_fraction(dashboard_panel_span(panel))))
                        .p(tokens.spacing.xs)
                        .child(self.render_dashboard_panel(&conn, board, panel, cx)),
                );
            }
            grid = grid.child(line);
        }

        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .id("db-dashboard-page")
            .overflow_y_scroll()
            .children(vec![
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.md)
                    .p(tokens.spacing.md)
                    .child(header)
                    .child(grid)
                    .into_any_element(),
            ])
            .into_any_element()
    }

    /// 一块面板：标题条（编辑态带控件）+ 内容。
    pub(super) fn render_dashboard_panel(
        &self,
        conn: &str,
        board: &Dashboard,
        panel: &DashboardPanel,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let editing = self.dashboard_editing;
        let fetch = self
            .dashboard_results
            .get(&(conn.to_owned(), board.id, panel.id));

        // ── 分隔标题：整行横条，点一下折起它统领的那一段 ──
        if let DashboardPanelKind::Divider { title } = &panel.kind {
            let panel_id = panel.id;
            let collapsed = self.dashboard_collapsed.contains(&panel_id);
            let mut strip = h_flex()
                .id(("db-dash-divider", panel_id))
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .px(tokens.spacing.xs)
                .py(tokens.spacing.xxs)
                .rounded(tokens.radius.sm)
                .cursor_pointer()
                .role(Role::Button)
                .aria_label(SharedString::from(if collapsed {
                    trf(cx, "展开「{}」这一段", &[title])
                } else {
                    trf(cx, "折起「{}」这一段", &[title])
                }))
                .hover(move |style| style.bg(extended.colors.hover_tint))
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.toggle_dashboard_divider(panel_id, cx);
                        });
                    }
                })
                .child(
                    Icon::new(if collapsed {
                        DuckIcon::ChevronRight
                    } else {
                        DuckIcon::ChevronDown
                    })
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.colors.foreground)
                        .truncate()
                        .child(SharedString::from(title.clone())),
                )
                .child(div().flex_1());
            if editing {
                strip = strip.child(self.dashboard_divider_controls(board, panel, cx));
            }
            return strip.into_any_element();
        }

        // ── 其余两类：卡片 = 标题条 + 内容 ──
        let (icon, title) = match &panel.kind {
            DashboardPanelKind::Chart { saved_chart_id } => (
                DuckIcon::ChartLine,
                self.saved_charts
                    .iter()
                    .find(|chart| &chart.id == saved_chart_id)
                    .map(|chart| chart.name.clone())
                    .unwrap_or_else(|| tr(cx, "已删除的图表").to_owned()),
            ),
            DashboardPanelKind::Inspector { view: tab } => (
                // 与左栏那两个入口共用同一套图标：面板上的「实例指标」和左栏的
                // 「实例指标」是同一份数据，不该画成两个样子。
                instance_tab_icon(match tab {
                    InspectorView::Metrics => InstanceTab::Metrics,
                    InspectorView::Processes => InstanceTab::Inspector,
                }),
                tab.heading(cx).to_owned(),
            ),
            // 分隔标题已在上面 return。
            DashboardPanelKind::Divider { .. } => (DuckIcon::Columns2, String::new()),
        };
        let mut head = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(
                Icon::new(icon)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(tokens.colors.muted_foreground)
                    .child(SharedString::from(title)),
            );
        if editing {
            head = head.child(self.dashboard_panel_controls(board, panel, cx));
        }

        let content = match &panel.kind {
            DashboardPanelKind::Chart { .. } => self.render_dashboard_chart_body(panel, fetch, cx),
            DashboardPanelKind::Inspector { .. } => match fetch {
                Some(PanelFetch::Table(table)) => self.instance_table_card(table, cx),
                Some(PanelFetch::Failed(message)) => meta_text(
                    SharedString::from(trf(cx, "读取失败：{}", &[message.as_str()])),
                    cx,
                )
                .into_any_element(),
                _ => meta_text(tr(cx, "还没有数据，点右上角「刷新」。"), cx).into_any_element(),
            },
            DashboardPanelKind::Divider { .. } => div().into_any_element(),
        };

        v_flex()
            .size_full()
            .min_w_0()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .rounded(tokens.radius.md)
            .bg(tokens.colors.surface)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(head)
            .child(content)
            .into_any_element()
    }

    /// 图表面板的内容：按面板引用的那条已保存图表的列映射出图。
    pub(super) fn render_dashboard_chart_body(
        &self,
        panel: &DashboardPanel,
        fetch: Option<&PanelFetch>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let DashboardPanelKind::Chart { saved_chart_id } = &panel.kind else {
            return div().into_any_element();
        };
        let Some(PanelFetch::Chart(result)) = fetch else {
            return match fetch {
                Some(PanelFetch::MissingChart) => {
                    meta_text(tr(cx, "引用的已保存图表已被删除；在编辑态里换一条。"), cx)
                        .into_any_element()
                }
                Some(PanelFetch::NoRows) => {
                    meta_text(tr(cx, "这条语句没有返回结果集。"), cx).into_any_element()
                }
                Some(PanelFetch::Failed(message)) => meta_text(
                    SharedString::from(trf(cx, "读取失败：{}", &[message.as_str()])),
                    cx,
                )
                .into_any_element(),
                _ => meta_text(tr(cx, "还没有数据，点右上角「刷新」。"), cx).into_any_element(),
            };
        };
        // 面板引用的图表**可能已被删**（那时面板还能立着，但配置没了），列映射对不上
        // 就回落默认映射 —— 与结果区的图表视角同一条口径。
        let stored = self
            .saved_charts
            .iter()
            .find(|chart| &chart.id == saved_chart_id)
            .map(|chart| chart.config());
        let config = match stored {
            Some(config) if config.is_valid_for(result) => config,
            _ => match ChartConfig::from_result(result) {
                Some(fallback) => fallback,
                None => {
                    return meta_text(tr(cx, "这份结果里没有数值列，画不出图。"), cx)
                        .into_any_element();
                }
            },
        };
        let series = config.effective_y();
        let mut column = v_flex().w_full().min_w_0().gap(tokens.spacing.xs);
        if config.kind == ChartKind::StackedBar {
            // 仪表板的面板本来就窄，几列同框才读得出「这一柱由谁构成」。
            column = column.child(self.stacked_panel(
                result,
                &config,
                series,
                DASHBOARD_CHART_HEIGHT,
                cx,
            ));
        } else {
            for name in series {
                column = column.child(self.chart_panel(
                    result,
                    &config,
                    name,
                    DASHBOARD_CHART_HEIGHT,
                    cx,
                ));
            }
        }
        column.into_any_element()
    }

    /// 编辑态下**普通面板**标题条右侧那排控件：换内容 / 前挪后挪 / 改宽 / 移除。
    pub(super) fn dashboard_panel_controls(
        &self,
        board: &Dashboard,
        panel: &DashboardPanel,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let board_id = board.id;
        let panel_id = panel.id;
        let index = board
            .panels
            .iter()
            .position(|candidate| candidate.id == panel_id)
            .unwrap_or_default();
        let last = board.panels.len().saturating_sub(1);

        let mut row = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);

        // 换内容：图表面板列出全部已保存图表，检查器面板列出两档表。
        match &panel.kind {
            DashboardPanelKind::Chart { saved_chart_id } => {
                if !self.saved_charts.is_empty() {
                    let options: Vec<(SharedString, SharedString)> = self
                        .saved_charts
                        .iter()
                        .map(|chart| {
                            (
                                SharedString::from(chart.id.clone()),
                                SharedString::from(chart.name.clone()),
                            )
                        })
                        .collect();
                    row = row.child(dropdown_select(
                        SharedString::from(format!("db-dash-chart-{panel_id}")),
                        options,
                        SharedString::from(saved_chart_id.clone()),
                        {
                            let view = view.clone();
                            move |value: SharedString, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.set_dashboard_chart(
                                        board_id,
                                        panel_id,
                                        value.to_string(),
                                        cx,
                                    );
                                });
                            }
                        },
                        cx,
                    ));
                }
            }
            DashboardPanelKind::Inspector { view: current } => {
                let options: Vec<(SharedString, SharedString)> = InspectorView::ALL
                    .into_iter()
                    .map(|tab| {
                        (
                            SharedString::from(tab.key()),
                            SharedString::from(tab.heading(cx)),
                        )
                    })
                    .collect();
                row = row.child(dropdown_select(
                    SharedString::from(format!("db-dash-inspector-{panel_id}")),
                    options,
                    SharedString::from(current.key()),
                    {
                        let view = view.clone();
                        move |value: SharedString, _, cx| {
                            let Some(tab) = InspectorView::from_key(&value) else {
                                return;
                            };
                            let _ = view.update(cx, |this, cx| {
                                this.set_dashboard_inspector(board_id, panel_id, tab, cx);
                            });
                        }
                    },
                    cx,
                ));
            }
            DashboardPanelKind::Divider { .. } => {}
        }

        // 前挪 / 后挪。
        for (tag, icon, label, delta, disabled) in [
            (
                "move-prev",
                DuckIcon::ChevronLeft,
                tr(cx, "前挪"),
                -1_i32,
                index == 0,
            ),
            (
                "move-next",
                DuckIcon::ChevronRight,
                tr(cx, "后挪"),
                1_i32,
                index >= last,
            ),
        ] {
            row = row.child(
                icon_button(
                    SharedString::from(format!("db-dash-{tag}-{panel_id}")),
                    label,
                    Icon::new(icon)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(disabled)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.move_dashboard_panel(board_id, panel_id, delta, cx);
                        });
                    }
                }),
            );
        }

        // 改宽：−1 / +1 栏 + 整行。
        for (tag, icon, label, span, disabled) in [
            (
                "narrow",
                DuckIcon::Minus,
                tr(cx, "变窄"),
                panel.span.saturating_sub(1),
                panel.span <= 1,
            ),
            (
                "widen",
                DuckIcon::Plus,
                tr(cx, "变宽"),
                panel.span + 1,
                panel.span >= DASHBOARD_COLUMNS,
            ),
            (
                "full",
                DuckIcon::Columns2,
                tr(cx, "整行"),
                DASHBOARD_COLUMNS,
                panel.span >= DASHBOARD_COLUMNS,
            ),
        ] {
            row = row.child(
                icon_button(
                    SharedString::from(format!("db-dash-{tag}-{panel_id}")),
                    label,
                    Icon::new(icon)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(disabled)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.set_dashboard_span(board_id, panel_id, span, cx);
                        });
                    }
                }),
            );
        }

        row = row.child(
            icon_button(
                SharedString::from(format!("db-dash-remove-{panel_id}")),
                tr(cx, "移除面板"),
                Icon::new(DuckIcon::X)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.remove_dashboard_panel(board_id, panel_id, cx);
                    });
                }
            }),
        );
        row.into_any_element()
    }

    /// 编辑态下**分隔标题**右侧的控件：只有「改标题」与「移除」。
    ///
    /// 分隔条不做改宽 / 挪位以外的排布动作：它本来就是整行横条，宽度没有意义。
    pub(super) fn dashboard_divider_controls(
        &self,
        board: &Dashboard,
        panel: &DashboardPanel,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let board_id = board.id;
        let panel_id = panel.id;
        let title = match &panel.kind {
            DashboardPanelKind::Divider { title } => title.clone(),
            _ => String::new(),
        };
        h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .child(
                icon_button(
                    SharedString::from(format!("db-dash-rename-{panel_id}")),
                    tr(cx, "改标题"),
                    Icon::new(DuckIcon::Pencil)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.open_divider_title_dialog(
                                board_id,
                                panel_id,
                                title.clone(),
                                window,
                                cx,
                            );
                        });
                    }
                }),
            )
            .child(
                icon_button(
                    SharedString::from(format!("db-dash-remove-panel-{panel_id}")),
                    tr(cx, "移除面板"),
                    Icon::new(DuckIcon::X)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.remove_dashboard_panel(board_id, panel_id, cx);
                        });
                    }
                }),
            )
            .into_any_element()
    }

    /// 结果区的**图表视角**：配置条 + 图。
    ///
    /// 为什么是「小倍数」而不是叠线：`gpui-component` 的样式化图表一次只画**一条序列**
    /// （折线 / 面积内部就是一块单线 `Plot`，柱状是一带一值）。要叠几条线得自绘 `Plot`，
    /// 而把量纲不同的列（`count` 与 `amount`）塞进同一条 Y 轴本身就是错的。所以多选几列
    /// 时**每列一张小图**并排铺开——这是可视化里的标准做法（small multiples），也正好
    /// 是这块原语擅长的事。
    pub(super) fn render_chart(&self, result: &ResultSet, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let Some(stored) = self.chart_view.clone() else {
            return div().into_any_element();
        };
        // 列名对不上（重跑后列变了）→ 回落默认映射；连数值列都没有就如实说。
        let config = if stored.is_valid_for(result) {
            stored
        } else if let Some(fallback) = ChartConfig::from_result(result) {
            fallback
        } else {
            return empty_state(
                DuckIcon::ChartLine,
                tr(cx, "这份结果里没有数值列"),
                Some(tr(cx, "图表至少要有一列数值；切回表格看原始结果。").into()),
                None,
                cx,
            )
            .flex_1()
            .into_any_element();
        };

        // ── 配置条：类型 / 横轴 / 数值列 / 保存 ──
        let kind_options: Vec<(SharedString, SharedString)> = ChartKind::ALL
            .into_iter()
            .map(|kind| {
                (
                    SharedString::from(kind.key()),
                    SharedString::from(kind.heading(cx)),
                )
            })
            .collect();
        let axis_options: Vec<(SharedString, SharedString)> = result
            .columns
            .iter()
            .map(|column| {
                (
                    SharedString::from(column.name.clone()),
                    SharedString::from(column.name.clone()),
                )
            })
            .collect();
        // 数值列勾选：到顶的未选项画成停用态（点不动看得见原因，不静默吞掉点击）。
        let capacity = config.kind.series_limit();
        let mut series_row = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(field_label(tr(cx, "数值列"), cx));
        let mut numeric_columns = 0_usize;
        for (index, column) in result.columns.iter().enumerate() {
            if !column_is_numeric(result, index) {
                continue;
            }
            numeric_columns += 1;
            let name = column.name.clone();
            let selected = config.y.iter().any(|picked| picked == &name);
            let at_capacity = !selected && config.y.len() >= capacity;
            let row = check_row(
                SharedString::from(format!("db-chart-series-{name}")),
                selected,
                div().child(SharedString::from(name.clone())),
                {
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.toggle_chart_series(&name, cx));
                    }
                },
                cx,
            );
            series_row = series_row.child(if at_capacity {
                row.opacity(extended.opacity.disabled)
            } else {
                row
            });
        }
        if numeric_columns == 0 {
            series_row = series_row.child(meta_text(tr(cx, "没有数值列。"), cx));
        }

        let controls = v_flex()
            .w_full()
            .flex_none()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .rounded(tokens.radius.md)
            .bg(tokens.colors.surface)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(field_label(tr(cx, "图表类型"), cx))
                    .child(dropdown_select(
                        "db-chart-kind",
                        kind_options,
                        SharedString::from(config.kind.key()),
                        {
                            let view = view.clone();
                            move |value: SharedString, _window, cx| {
                                let Some(kind) = ChartKind::from_key(&value) else {
                                    return;
                                };
                                let _ = view.update(cx, |this, cx| this.set_chart_kind(kind, cx));
                            }
                        },
                        cx,
                    ))
                    .child(field_label(tr(cx, "横轴"), cx))
                    .child(dropdown_select(
                        "db-chart-axis",
                        axis_options,
                        SharedString::from(config.x.clone()),
                        {
                            let view = view.clone();
                            move |value: SharedString, _window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.set_chart_axis(value.to_string(), cx);
                                });
                            }
                        },
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(
                        button(
                            "db-save-chart",
                            tr(cx, "保存图表"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(config.y.is_empty())
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.open_save_chart_dialog(window, cx);
                                });
                            }
                        }),
                    ),
            )
            .child(series_row)
            // 读数行：这份图**画的是哪一列、按什么排、一共多少行**。
            //
            // 此前这块区域一句事实都没有 —— 用户看不出横轴是按时间还是按分类排的，
            // 也看不出图里是不是只有前几百行。轴类型尤其要说：时间 / 数值轴会**重排**
            // 行序，不讲明白，「和我 SQL 里的顺序不一样」就会像 bug。
            .child(meta_text(
                SharedString::from(trf(
                    cx,
                    "共 {} 行 · 横轴「{}」（{}轴）· {} 个数值列",
                    &[
                        &result.rows.len().to_string(),
                        &config.x,
                        config.axis_kind(result).heading(cx),
                        &numeric_columns.to_string(),
                    ],
                )),
                cx,
            ));

        // ── 图：堆叠柱**一张**（几条列同框），其余每列一张、两列一行 ──
        let series: Vec<String> = config.effective_y().to_vec();
        let mut body = v_flex().w_full().gap(tokens.spacing.md);
        if series.is_empty() {
            body = body.child(meta_text(
                tr(cx, "还没选数值列；勾一个上面列出的数值列就会出图。"),
                cx,
            ));
        } else if config.kind == ChartKind::StackedBar {
            // 堆叠柱只有一张：多列的意义是「叠进同一根柱」，拆成两列一行就不是堆叠了。
            // 宽度与「两列一行」的一张等宽，纵向节奏与其余各档一致。
            let mut row = h_flex().w_full().items_start().gap(tokens.spacing.md);
            row = row.child(self.stacked_panel(result, &config, &series, CHART_PANEL_HEIGHT, cx));
            row = row.child(div().flex_1());
            body = body.child(row);
        } else {
            for chunk in series.chunks(2) {
                let mut row = h_flex().w_full().items_start().gap(tokens.spacing.md);
                for column in chunk {
                    row = row.child(self.chart_panel(
                        result,
                        &config,
                        column,
                        CHART_PANEL_HEIGHT,
                        cx,
                    ));
                }
                // 奇数张时补一个空位，免得最后一张被拉成整行宽（两张一行的节奏）。
                if chunk.len() == 1 {
                    row = row.child(div().flex_1());
                }
                body = body.child(row);
            }
        }

        v_flex()
            .id("db-chart-body")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .gap(tokens.spacing.md)
            .p(tokens.spacing.md)
            .overflow_y_scroll()
            .child(controls)
            .child(body)
            .into_any_element()
    }

    /// 一张小图：标题（`数值列 · 横轴`）+ 图。
    ///
    /// `height` 由调用方给：结果区的图表视角一屏只放两张，图给得高（[`CHART_PANEL_HEIGHT`]）；
    /// 仪表板一块面板里可能叠好几张，得矮一些（[`DASHBOARD_CHART_HEIGHT`]）。除了高度，
    /// 两条路画的是**同一张图**（同一套 [`chart_element`] 与列映射口径）。
    pub(super) fn chart_panel(
        &self,
        result: &ResultSet,
        config: &ChartConfig,
        column: &str,
        height: f32,
        cx: &App,
    ) -> AnyElement {
        let points = chart_series(result, &config.x, column);
        let total = points.len();
        // 饼图画的是**汇总占比**，抽稀会让每一片都变大变小、百分比失真 —— 不抽。
        // 其余五档画的是「随横轴怎么变」，抽掉中间的点不改变形状，只是把像素分不开的
        // 那部分点合并掉。堆叠柱的抽稀在 `stacked_panel` 里逐列做。
        let points = match config.kind {
            ChartKind::Pie => points,
            _ => decimate_lttb(points, CHART_MAX_POINTS),
        };
        let drawn = points.len();
        // 标题里的「已抽稀 M/N 点」是**事实读数**，不是装饰：用户看到一条平滑的折线时
        // 有权知道它由多少原始行化简而来，否则会把抽稀后的形状当成逐行真相。
        let mut title = format!("{column} · {}", config.x);
        if drawn < total {
            title.push_str(" · ");
            title.push_str(&trf(
                cx,
                "已抽稀 {}/{} 点",
                &[&drawn.to_string(), &total.to_string()],
            ));
        }
        let panel = chart_frame(title, cx);
        if points.is_empty() {
            return panel
                .child(meta_text(tr(cx, "这一列没有可画的数值。"), cx))
                .into_any_element();
        }
        let Some(chart) = chart_element(config.kind, points, column, &plot_style(cx)) else {
            return panel
                .child(meta_text(tr(cx, "饼图只吃正值，这一列全是 0 或负数。"), cx))
                .into_any_element();
        };
        panel
            .child(div().w_full().h(px(height)).child(chart))
            .into_any_element()
    }

    /// **多列同框**的堆叠柱：一屏一张，几条数值列叠进同一根柱。
    ///
    /// 与 [`Self::chart_panel`]（一列一张的小倍数）分成两条路，是因为堆叠的语义就在
    /// 「同一根柱由谁构成」：拆成一列一张，要读的东西就被拆没了。`columns` 传
    /// [`ChartConfig::effective_y`]，所以「一根柱最多几段」仍由 [`ChartKind::series_limit`]
    /// 把关，不是在另一处再写一遍上限。
    pub(super) fn stacked_panel(
        &self,
        result: &ResultSet,
        config: &ChartConfig,
        columns: &[String],
        height: f32,
        cx: &App,
    ) -> AnyElement {
        let mut raw = 0_usize;
        let series: Vec<(String, Vec<ChartPoint>)> = columns
            .iter()
            .map(|column| {
                let points = chart_series(result, &config.x, column);
                raw += points.len();
                (column.clone(), decimate_lttb(points, CHART_MAX_POINTS))
            })
            .collect();
        let drawn: usize = series.iter().map(|(_, points)| points.len()).sum();
        // 标题要写清**哪几列叠在一起**：堆叠图离开这一句，每一段是谁就读不出来。
        let mut title = format!("{} · {}", columns.join(" + "), config.x);
        if drawn < raw {
            title.push_str(" · ");
            title.push_str(&trf(
                cx,
                "已抽稀 {}/{} 点",
                &[&drawn.to_string(), &raw.to_string()],
            ));
        }
        let panel = chart_frame(title, cx);
        if series.iter().all(|(_, points)| points.is_empty()) {
            return panel
                .child(meta_text(tr(cx, "这几列都没有可画的数值。"), cx))
                .into_any_element();
        }
        panel
            .child(
                div()
                    .w_full()
                    .h(px(height))
                    .child(stacked_element(series, &plot_style(cx))),
            )
            .into_any_element()
    }
}

/// 图的外壳：标题 + 边框底色。结果区与仪表板两条路的图都套这一层，所以只有这一份定义。
fn chart_frame(title: String, cx: &App) -> gpui::Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    v_flex()
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.xs)
        .p(tokens.spacing.sm)
        .rounded(tokens.radius.md)
        .bg(tokens.colors.surface)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(
            div()
                .text_size(tokens.typography.xs.size)
                .text_color(tokens.colors.muted_foreground)
                .truncate()
                .child(SharedString::from(title)),
        )
}

/// 自绘图要的主题色与字号。
///
/// 集中在这一处取：`charts_plot` 的绘制闭包活在 `'static` 里、拿不到 `cx`，颜色字号只能
/// 在渲染期带进去；两处各取一遍迟早走岔。
fn plot_style(cx: &App) -> PlotStyle {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    PlotStyle {
        accent: extended.colors.accent_text,
        grid: extended.colors.hairline,
        tick: tokens.colors.muted_foreground,
        text_size: tokens.typography.xs.size,
    }
}
