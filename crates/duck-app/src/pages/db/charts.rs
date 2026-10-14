use super::*;

impl ChartKind {
    /// 四档全量（菜单顺序）。
    pub(super) const ALL: [Self; 4] = [Self::Line, Self::Bar, Self::Area, Self::Pie];

    /// 展示文案。
    pub(super) fn heading(self, cx: &App) -> &'static str {
        match self {
            Self::Line => tr(cx, "折线图"),
            Self::Bar => tr(cx, "柱状图"),
            Self::Area => tr(cx, "面积图"),
            Self::Pie => tr(cx, "饼图"),
        }
    }

    /// 下拉选项 / 持久化用的稳定英文键（**不翻译**，同 [`InstanceTab::key`] 理由）。
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Bar => "bar",
            Self::Area => "area",
            Self::Pie => "pie",
        }
    }

    /// 从稳定键还原（下拉回投用；认不出时给 `None`，不猜）。
    pub(super) fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// 同时能画几条数值序列。
    ///
    /// 饼图只吃**一列**：多列拼进一张饼是「苹果 + 橘子」的和，读不出任何东西，所以
    /// 饼图档下选另一列 = 换掉这一条。其余三档封顶 [`CHART_SERIES_LIMIT`]：再多就
    /// 是一屏碎图，剩下的列留在表里（不删）。
    pub(super) const fn series_limit(self) -> usize {
        match self {
            Self::Pie => 1,
            Self::Line | Self::Bar | Self::Area => CHART_SERIES_LIMIT,
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

/// 从结果集挑默认的图表列：`x` 取**第一个非数值列**（全数值就退回第一列），
/// `y` 取前 [`CHART_SERIES_LIMIT`] 个数值列。
///
/// 没有任何数值列时返回 `None`——那说明这张表画不出图，界面该说清楚而不是画一张空图。
pub(super) fn default_chart_columns(result: &ResultSet) -> Option<(String, Vec<String>)> {
    let numeric: Vec<bool> = (0..result.columns.len())
        .map(|index| column_is_numeric(result, index))
        .collect();
    // X 优先文本列（分类轴）；全是数值时用第一列当轴。
    let x_index = result
        .columns
        .iter()
        .enumerate()
        .find(|(index, _)| !numeric[*index])
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

/// 把结果集的两列抽成「分类 → 数值」序列。
///
/// 跳过两类行：分类为 `NULL` 的（分类轴上一个叫「NULL」的桶是噪声，真要看那几行去表格
/// 视图）与数值非数的（画不进坐标系）。**不补 0**：补 0 会在图上画出一段实际不存在的
/// 「跌到底」，比断开更难解释。
///
/// 选中列在当前结果集里不存在时返回空表——调用方据此显示「配置失效」而不是画空图。
pub(super) fn chart_series(result: &ResultSet, x: &str, y: &str) -> Vec<(String, f64)> {
    let column_index = |name: &str| result.columns.iter().position(|column| column.name == name);
    let (Some(xi), Some(yi)) = (column_index(x), column_index(y)) else {
        return Vec::new();
    };
    result
        .rows
        .iter()
        .filter_map(|row| {
            if matches!(row.get(xi), None | Some(Value::Null)) {
                return None;
            }
            let label = row.get(xi)?.render();
            let value = chart_number(row.get(yi)?)?;
            Some((label, value))
        })
        .collect()
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

/// 按类型造一张图（四种图的构造差异只集中在这一处）。
///
/// 数据一律 `Vec<(分类, 数值)>`；饼图另外带上下标（配色要按切片顺序取）。
/// 返回 `None` = 这份数据画不出来（饼图全是非正值），调用方据此给一句说明。
pub(super) fn chart_element(
    kind: ChartKind,
    points: Vec<(String, f64)>,
    key: &str,
    accent: Hsla,
) -> Option<AnyElement> {
    let id = SharedString::from(format!("db-chart-{key}"));
    let element = match kind {
        ChartKind::Line => LineChart::new(points)
            .id(id)
            .x(|(label, _)| label.clone())
            .y(|(_, value)| *value)
            .stroke(accent)
            .grid(true)
            .y_tick_format(compact_number)
            .tooltip_title(|(label, _)| SharedString::from(label.clone()))
            .tooltip_value(|_, value| SharedString::from(compact_number(value)))
            .into_any_element(),
        ChartKind::Bar => BarChart::new(points)
            .id(id)
            .band(|(label, _)| label.clone())
            .value(|(_, value)| *value)
            .fill(move |_, _, _, _| accent)
            .grid(true)
            // 柱状图的「带」就是分类，值轴要自己画刻度（否则只有柱高没有刻度）。
            .value_axis(true)
            .value_tick_format(compact_number)
            .tooltip_title(|(label, _)| SharedString::from(label.clone()))
            .tooltip_value(|_, value| SharedString::from(compact_number(value)))
            .into_any_element(),
        ChartKind::Area => AreaChart::new(points)
            .id(id)
            .x(|(label, _)| label.clone())
            .y(|(_, value)| *value)
            .stroke(accent)
            .fill(accent.opacity(0.22))
            .grid(true)
            .y_tick_format(compact_number)
            .tooltip_title(|(label, _)| SharedString::from(label.clone()))
            .tooltip_value(|_, _, value| SharedString::from(compact_number(value)))
            .into_any_element(),
        ChartKind::Pie => {
            // 饼图画的是占比：非正值（负数 / 零）在饼上没有位置，先滤掉。
            let slices: Vec<(String, f64, usize)> = points
                .into_iter()
                .filter(|(_, value)| *value > 0.0)
                .enumerate()
                .map(|(index, (label, value))| (label, value, index))
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
        let mut column = v_flex().w_full().min_w_0().gap(tokens.spacing.xs);
        for series in config.effective_y() {
            column =
                column.child(self.chart_panel(result, &config, series, DASHBOARD_CHART_HEIGHT, cx));
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
            .child(series_row);

        // ── 图：每列一张，两列一行 ──
        let series: Vec<String> = config.effective_y().to_vec();
        let mut body = v_flex().w_full().gap(tokens.spacing.md);
        if series.is_empty() {
            body = body.child(meta_text(
                tr(cx, "还没选数值列；勾一个上面列出的数值列就会出图。"),
                cx,
            ));
        }
        for chunk in series.chunks(2) {
            let mut row = h_flex().w_full().items_start().gap(tokens.spacing.md);
            for column in chunk {
                row = row.child(self.chart_panel(result, &config, column, CHART_PANEL_HEIGHT, cx));
            }
            // 奇数张时补一个空位，免得最后一张被拉成整行宽（两张一行的节奏）。
            if chunk.len() == 1 {
                row = row.child(div().flex_1());
            }
            body = body.child(row);
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
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let points = chart_series(result, &config.x, column);
        let panel = v_flex()
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
                    .child(SharedString::from(format!("{column} · {}", config.x))),
            );
        if points.is_empty() {
            return panel
                .child(meta_text(tr(cx, "这一列没有可画的数值。"), cx))
                .into_any_element();
        }
        let Some(chart) = chart_element(config.kind, points, column, extended.colors.accent_text)
        else {
            return panel
                .child(meta_text(tr(cx, "饼图只吃正值，这一列全是 0 或负数。"), cx))
                .into_any_element();
        };
        panel
            .child(div().w_full().h(px(height)).child(chart))
            .into_any_element()
    }
}
