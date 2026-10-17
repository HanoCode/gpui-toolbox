//! 比较弹窗：源库 → 目标库，逐表对比**结构**或**数据**（dbx「比较架构」弹窗对位）。
//!
//! # 为什么是弹窗而不是标签
//!
//! 比较是一次性的对账动作：选两侧 → 跑 → 看差异 → 补数 / 传输，然后就没有然后了。
//! 占一张标签意味着每次都要先关掉上次那份报告，而标签本身还占着标签条。
//! dbx 把这件事放在一个弹窗里（配置 ↔ 结果两页），本模块照此实现。
//!
//! # 两套后端
//!
//! - **结构比较**：两侧逐表取列清单（名字 / 类型 / 注释）与主键，产出列级 diff
//!   （目标缺列 / 源缺列 / 类型不一致 / 注释不一致）与主键差异。不跑 `COUNT(*)`，
//!   也不拉主键值 —— 结构差异与数据量无关。
//! - **数据比较**：两侧 `COUNT(*)` + 列名 + **主键级行差**（两侧各拉一份主键值做差），
//!   报「目标缺失 N 行 / 目标多出 N 行」与主键样例；差异行可一键生成补数 INSERT。
//!   单元格级的逐列对比不在本期口径（那要对齐两侧类型系统），界面上如实注明。
//!
//! 两套都跑在 `background_executor` 上（要过 [`db_op_gate`]），弹窗只负责画状态。
//! **弹窗正文闭包只跑一次**，所以实时状态全部放在 [`CompareDialogView`] 这个独立实体里
//! —— 状态变化走 `notify()` 重绘，不依赖外层闭包重新执行。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use super::*;
use duck_db::TableColumnMeta;

/// 参与比较的表数上限：两侧逐表跑查询，几百张表已经是分钟级。
const COMPARE_TABLE_CAP: usize = 200;

/// 主键值清单的拉取上限：主键 diff 只在两侧各 5 万键以内做，超出就明说「未比对」。
const COMPARE_PK_CAP: usize = 50_000;

/// 补数 SQL 一次生成的行数上限：缺失行再多也先补前 500 行，剩下交给传输。
const COMPARE_BACKFILL_CAP: usize = 500;

/// 弹窗宽度（比默认 448 宽得多：两侧并排 + 结果明细）。
const COMPARE_DIALOG_WIDTH: f32 = 920.;

/// 结果列表的最大高度（弹窗内滚动区的上限；布局约束，不在 spacing 阶梯上）。
const COMPARE_LIST_MAX_HEIGHT: f32 = 360.;

/// 比较配置的 kv 键（单键 JSON：两侧连接 / 库 + 选项）。
const KEY_COMPARE_CONFIG: &str = "db_compare_config";

/// 参与比较的例程数上限（过程 / 函数；每条两侧各取一次定义，量级同表数）。
const COMPARE_ROUTINE_CAP: usize = 200;

/// 比结构还是比数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CompareScope {
    /// 结构比较（列 / 类型 / 注释 / 主键）。
    #[default]
    Schema,
    /// 数据比较（行数 + 主键级行差）。
    Data,
}

impl CompareScope {
    /// 分段切换上的文案。
    fn label(self, cx: &App) -> SharedString {
        SharedString::from(match self {
            Self::Schema => tr(cx, "结构"),
            Self::Data => tr(cx, "数据"),
        })
    }

    /// 分段索引 ↔ 档位。
    fn index(self) -> usize {
        match self {
            Self::Schema => 0,
            Self::Data => 1,
        }
    }

    fn from_index(index: usize) -> Self {
        if index == 0 { Self::Schema } else { Self::Data }
    }
}

/// 比较规格：两侧「连接 + 命名空间」+ 内容档位 + 选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompareSpec {
    /// 比结构还是比数据。
    pub scope: CompareScope,
    /// 源连接。
    pub source_conn: String,
    /// 源命名空间。
    pub source_ns: String,
    /// 目标连接（默认另一条已连接会话）。
    pub target_conn: String,
    /// 目标命名空间。
    pub target_ns: String,
    /// 只比较这些表（表节点右键入口带单张；空 = 全库）。
    pub tables: Vec<TableRef>,
    /// 比较库内全部基础表（开关「表」）。
    pub all_tables: bool,
    /// 忽略注释差异（只影响结构比较）。
    pub ignore_comments: bool,
    /// 连存储过程 / 函数一起比（只影响结构比较）。
    pub routines: bool,
}

/// 保存下来的比较配置（kv 单键 JSON）。
///
/// 字段一律 `Option` + `serde(default)`：旧版本写下的配置少了任何一栏都能读回来，
/// 缺的那栏按界面默认值走。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CompareConfig {
    #[serde(default)]
    scope: Option<CompareScope>,
    #[serde(default)]
    source_conn: Option<String>,
    #[serde(default)]
    source_ns: Option<String>,
    #[serde(default)]
    target_conn: Option<String>,
    #[serde(default)]
    target_ns: Option<String>,
    #[serde(default)]
    ignore_comments: Option<bool>,
    #[serde(default)]
    routines: Option<bool>,
}

/// 列类型不一致的一条。
#[derive(Debug, Clone)]
pub(super) struct TypeMismatch {
    /// 列名。
    pub column: String,
    /// 源侧类型名。
    pub source: String,
    /// 目标侧类型名。
    pub target: String,
}

/// 一张表的比较结论。
pub(super) enum CompareRowState {
    /// 只在源有（目标缺整张表）。
    OnlySource {
        /// 源侧行数（结构比较为 `None`：不跑 `COUNT(*)`）。
        rows: Option<u64>,
    },
    /// 只在目标有（源缺整张表）。
    OnlyTarget {
        /// 目标侧行数（结构比较为 `None`）。
        rows: Option<u64>,
    },
    /// 数据比较：两侧都有。
    Data {
        /// 源侧行数。
        source_rows: u64,
        /// 目标侧行数。
        target_rows: u64,
        /// 只在源有的列名（目标缺列）。
        columns_missing_in_target: Vec<String>,
        /// 只在目标有的列名。
        columns_missing_in_source: Vec<String>,
        /// 目标缺失的主键值（源有目标无；主键可比对时才有）。
        missing_in_target: Vec<String>,
        /// 目标多出的主键值。
        missing_in_source: Vec<String>,
        /// 主键列名（空 = 两侧都取不到主键）。
        pk: Vec<String>,
        /// 主键级 diff 是否真的跑过（键数超上限或主键不一致时为 false）。
        pk_compared: bool,
    },
    /// 结构比较：两侧都有。
    Schema {
        /// 源侧列数（「结构一致（N 列）」用）。
        column_count: usize,
        /// 目标缺的列。
        columns_missing_in_target: Vec<String>,
        /// 源缺的列。
        columns_missing_in_source: Vec<String>,
        /// 类型不一致的列。
        type_mismatches: Vec<TypeMismatch>,
        /// 注释不一致的列（勾了「忽略注释差异」时恒空）。
        comment_mismatches: Vec<String>,
        /// 主键不一致时的两侧主键（`None` = 一致）。
        pk_mismatch: Option<(Vec<String>, Vec<String>)>,
    },
    /// 例程（存储过程 / 函数）：比存在性与定义原文。
    Routine {
        /// 过程还是函数（决定名字前缀）。
        kind: TableKind,
        /// 源侧有没有这个例程。
        in_source: bool,
        /// 目标侧有没有。
        in_target: bool,
        /// 源侧定义原文（取不到为 `None`：引擎不支持 / 权限不足）。
        source_definition: Option<String>,
        /// 目标侧定义原文。
        target_definition: Option<String>,
    },
}

/// 比较报告的一行（一张表）。
pub(super) struct CompareRow {
    /// 源侧表引用（补数 / 传输 / 看结构都从源拉）。
    pub table: TableRef,
    /// 结论。
    pub state: CompareRowState,
}

impl CompareRow {
    /// 这一行的归类（汇总统计与筛选都用它）。
    fn bucket(&self, ignore_comments: bool) -> CompareBucket {
        match &self.state {
            CompareRowState::OnlySource { .. } => CompareBucket::OnlySource,
            CompareRowState::OnlyTarget { .. } => CompareBucket::OnlyTarget,
            CompareRowState::Data {
                source_rows,
                target_rows,
                columns_missing_in_target,
                columns_missing_in_source,
                missing_in_target,
                missing_in_source,
                ..
            } => {
                let same = source_rows == target_rows
                    && columns_missing_in_target.is_empty()
                    && columns_missing_in_source.is_empty()
                    && missing_in_target.is_empty()
                    && missing_in_source.is_empty();
                if same {
                    CompareBucket::Same
                } else {
                    CompareBucket::Changed
                }
            }
            CompareRowState::Schema {
                columns_missing_in_target,
                columns_missing_in_source,
                type_mismatches,
                comment_mismatches,
                pk_mismatch,
                ..
            } => {
                let same = columns_missing_in_target.is_empty()
                    && columns_missing_in_source.is_empty()
                    && type_mismatches.is_empty()
                    && (ignore_comments || comment_mismatches.is_empty())
                    && pk_mismatch.is_none();
                if same {
                    CompareBucket::Same
                } else {
                    CompareBucket::Changed
                }
            }
            CompareRowState::Routine {
                in_source,
                in_target,
                source_definition,
                target_definition,
                ..
            } => {
                if !*in_target {
                    CompareBucket::OnlySource
                } else if !*in_source {
                    CompareBucket::OnlyTarget
                } else if definitions_differ(
                    source_definition.as_deref(),
                    target_definition.as_deref(),
                ) {
                    CompareBucket::Changed
                } else {
                    CompareBucket::Same
                }
            }
        }
    }

    /// 这一行能生成什么 ALTER（`None` = 结构上没有可补的）。
    ///
    /// 三条口径与 [`alter_sql`] 一一对应：
    /// - 目标缺整张表 → 建表（只有结构档有「缺表」这一说：数据档的 `OnlySource`
    ///   是行数而不是表结构，不能拿去建表）；
    /// - 目标缺列 / 类型不一致 → `ADD COLUMN` / 改类型；
    /// - 只剩注释或主键差异 → 不给按钮（这两类不在本期 ALTER 口径里，
    ///   `DROP COLUMN` 同样不生成）。
    fn alter_target(&self, scope: CompareScope) -> Option<AlterTarget> {
        match (&self.state, scope) {
            (
                CompareRowState::Schema {
                    columns_missing_in_target,
                    type_mismatches,
                    ..
                },
                _,
            ) if !columns_missing_in_target.is_empty() || !type_mismatches.is_empty() => {
                Some(AlterTarget {
                    table: self.table.clone(),
                    missing_table: false,
                })
            }
            (CompareRowState::OnlySource { .. }, CompareScope::Schema) => Some(AlterTarget {
                table: self.table.clone(),
                missing_table: true,
            }),
            _ => None,
        }
    }
}

/// 生成 ALTER 的一张表：报告已经判过它是「目标缺表」还是「只差列 / 类型」。
#[derive(Debug, Clone)]
struct AlterTarget {
    /// 源侧表引用（列元信息与建表 DDL 都从源取）。
    table: TableRef,
    /// 目标缺整张表（抄源侧 `table_ddl`，不是 `ADD COLUMN`）。
    missing_table: bool,
}

/// 一行的归类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompareBucket {
    /// 两侧一致。
    Same,
    /// 两侧都有但有差异。
    Changed,
    /// 只在源有。
    OnlySource,
    /// 只在目标有。
    OnlyTarget,
}

/// 比较报告整体。
pub(super) struct CompareReport {
    /// 逐表结论（按表名排序）。
    pub rows: Vec<CompareRow>,
    /// 是否顶到表数上限（顶到了就明说「其余表未比对」）。
    pub capped: bool,
}

impl CompareReport {
    /// 四档归类各有多少张表（顺序：一致 / 有差异 / 仅源有 / 仅目标有）。
    fn counts(&self, ignore_comments: bool) -> [usize; 4] {
        let mut counts = [0usize; 4];
        for row in &self.rows {
            let index = match row.bucket(ignore_comments) {
                CompareBucket::Same => 0,
                CompareBucket::Changed => 1,
                CompareBucket::OnlySource => 2,
                CompareBucket::OnlyTarget => 3,
            };
            counts[index] += 1;
        }
        counts
    }
}

/// 弹窗的两页：配置 / 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComparePane {
    Config,
    Result,
}

/// 结果筛选。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompareFilter {
    All,
    Changed,
    OnlySource,
    OnlyTarget,
}

impl CompareFilter {
    fn accepts(self, bucket: CompareBucket) -> bool {
        match self {
            Self::All => true,
            Self::Changed => bucket == CompareBucket::Changed,
            Self::OnlySource => bucket == CompareBucket::OnlySource,
            Self::OnlyTarget => bucket == CompareBucket::OnlyTarget,
        }
    }
}

/// 比较弹窗的运行状态。
pub(super) enum CompareStatus {
    /// 还没跑。
    Idle,
    /// 跑着。
    Loading,
    /// 出了报告。
    Ready(CompareReport),
    /// 整体失败（连不上 / 目录都取不到）。
    Failed(String),
}

/// 四个下拉的槽位（数组下标）。
const PICK_SOURCE_CONN: usize = 0;
const PICK_SOURCE_NS: usize = 1;
const PICK_TARGET_CONN: usize = 2;
const PICK_TARGET_NS: usize = 3;

/// 比较弹窗的独立视图：配置 / 结果的全部状态都在这里。
///
/// 单独立一个实体而不是把状态摊在弹窗闭包里：`open_dialog` 的正文闭包**只跑一次**，
/// 后台比较回来时的结果必须靠实体自己的 `notify()` 重绘。
pub(super) struct CompareDialogView {
    /// 宿主页面（会话、目录、`stage_sql` / 开标签都在那边）。
    page: WeakEntity<DatabasePage>,
    /// 两侧规格 + 选项。
    spec: CompareSpec,
    /// 运行状态。
    status: CompareStatus,
    /// 当前显示哪一页。
    pane: ComparePane,
    /// 结果筛选。
    filter: CompareFilter,
    /// 四个下拉：源连接 / 源库 / 目标连接 / 目标库。
    picks: [Entity<DbSelectState>; 4],
    /// 四个下拉的选项签名（变了才重灌，用户正在敲的搜索词不被下一帧抹掉）。
    stamps: [Option<String>; 4],
    /// 后台比较在跑（防重复点）。
    busy: bool,
    /// 下拉订阅。
    _subscriptions: Vec<Subscription>,
}

impl CompareDialogView {
    /// 造一个下拉实体（与 `db_select_state` 同款，只是上下文类型不同）。
    fn new_pick(window: &mut Window, cx: &mut Context<Self>) -> Entity<DbSelectState> {
        cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true)
        })
    }

    fn new(
        page: WeakEntity<DatabasePage>,
        spec: CompareSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
        Self {
            page,
            spec,
            status: CompareStatus::Idle,
            pane: ComparePane::Config,
            filter: CompareFilter::All,
            picks,
            stamps: [None, None, None, None],
            busy: false,
            _subscriptions: subscriptions,
        }
    }

    /// 某个下拉确认后回写规格；换连接就把该侧的库清空（库清单跟着连接走）。
    fn pick(&mut self, slot: usize, value: String, cx: &mut Context<Self>) {
        match slot {
            PICK_SOURCE_CONN => {
                self.spec.source_conn = value;
                self.spec.source_ns.clear();
                self.stamps[PICK_SOURCE_NS] = None;
            }
            PICK_SOURCE_NS => self.spec.source_ns = value,
            PICK_TARGET_CONN => {
                self.spec.target_conn = value;
                self.spec.target_ns.clear();
                self.stamps[PICK_TARGET_NS] = None;
            }
            _ => self.spec.target_ns = value,
        }
        cx.notify();
    }

    /// 交换两侧（连接 + 库一起换）。
    fn swap_sides(&mut self, cx: &mut Context<Self>) {
        std::mem::swap(&mut self.spec.source_conn, &mut self.spec.target_conn);
        std::mem::swap(&mut self.spec.source_ns, &mut self.spec.target_ns);
        self.stamps = [None, None, None, None];
        cx.notify();
    }

    /// 四个下拉的选项 / 选中值按签名同步（渲染入口调用）。
    ///
    /// 与页面层 `sync_select_pickers` 同一套口径：签名没变就一个字都不动，
    /// 免得把用户正在敲的搜索词抹掉。
    fn sync_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.page.upgrade() else {
            return;
        };
        let (conn_options, options, selected) = {
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
                ns_options(&self.spec.source_conn),
                conn_options.clone(),
                ns_options(&self.spec.target_conn),
            ];
            let selected = [
                self.spec.source_conn.clone(),
                self.spec.source_ns.clone(),
                self.spec.target_conn.clone(),
                self.spec.target_ns.clone(),
            ];
            (conn_options, options, selected)
        };
        let _ = conn_options;
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

    /// 两侧是否选全了（开跑的前置条件）。
    fn ready(&self) -> bool {
        !self.busy
            && !self.spec.source_conn.is_empty()
            && !self.spec.source_ns.is_empty()
            && !self.spec.target_conn.is_empty()
            && !self.spec.target_ns.is_empty()
    }

    /// 开跑：源表清单当场从源会话目录拍下来，其余全部在后台闭包里对两侧连接跑。
    fn run(&mut self, cx: &mut Context<Self>) {
        if !self.ready() {
            return;
        }
        let Some(page) = self.page.upgrade() else {
            return;
        };
        let spec = self.spec.clone();
        let prepared = {
            let page = page.read(cx);
            match (
                page.sessions.get(&spec.source_conn),
                page.sessions.get(&spec.target_conn),
            ) {
                (Some(source), Some(target)) => {
                    // 源表清单：表级入口（spec 里带了表）且没打开「全部表」时直接用 ——
                    // 哪怕它已不在目录快照里（刚建的表还没重新内省也照样能比）。
                    let tables: Vec<TableRef> = if spec.all_tables || spec.tables.is_empty() {
                        source
                            .catalog
                            .iter()
                            .filter(|entry| entry.kind == TableKind::Table)
                            .filter(|entry| {
                                entry.schema.as_deref() == Some(spec.source_ns.as_str())
                            })
                            .map(|entry| entry.table_ref())
                            .collect()
                    } else {
                        spec.tables.clone()
                    };
                    // 快照只覆盖会话当时内省的那**一个**命名空间：目录是按连接当前库
                    // 取的，比较时另选一个库（连接默认库 ≠ 所选库）这里就会筛出 0 张
                    // —— 不能据此报「没有表」，交给后台按所选库**现场内省**一次。
                    let live_source =
                        (spec.all_tables || spec.tables.is_empty()) && tables.is_empty();
                    // 深比较逐表跑查询，才有上限一说；表名清单必须**全量**传下去 ——
                    // 「仅目标有」拿它当判定基准，截断了就会把没轮到的源表误报成
                    // 「仅目标有」（两个一样的库报出一堆假差异就是这么来的）。
                    let capped = tables.len() > COMPARE_TABLE_CAP;
                    Some((
                        source.conn.clone(),
                        target.conn.clone(),
                        tables,
                        capped,
                        live_source,
                    ))
                }
                _ => None,
            }
        };
        let Some((source_conn, target_conn, source_tables, capped, live_source)) = prepared else {
            self.status = CompareStatus::Failed(tr(cx, "连接已断开，请重新选择。").to_owned());
            // 失败必须切到结果页：配置页不渲染 status，留在配置页就是「点了没反应」。
            self.pane = ComparePane::Result;
            cx.notify();
            return;
        };
        if source_tables.is_empty() && !live_source {
            self.status = CompareStatus::Failed(tr(cx, "源库里没有可比对的基础表。").to_owned());
            self.pane = ComparePane::Result;
            cx.notify();
            return;
        }
        self.busy = true;
        self.filter = CompareFilter::All;
        self.pane = ComparePane::Result;
        self.status = CompareStatus::Loading;
        cx.notify();

        let target_ns = spec.target_ns.clone();
        let source_ns = spec.source_ns.clone();
        let ignore_comments = spec.ignore_comments;
        let routines = spec.routines;
        let scope = spec.scope;
        let task = cx.background_executor().spawn(async move {
            // 名额凭证活到闭包返回（`let _permit` 不是 `let _ =`：后者会立刻还回去）。
            let _permit = db_op_gate().acquire().await;
            // 快照没带出所选库的表时，按所选库现场内省一次（目录一次往返，开销与
            // 建连后的首次内省同量级）；目标侧本来就走这条现场内省路径。
            let (source_tables, capped) = if live_source {
                let entries = {
                    let mut guard = source_conn.lock().expect("连接锁不应中毒");
                    match guard.catalog(Some(&source_ns)) {
                        Ok(entries) => entries,
                        Err(error) => return Some(Err(error)),
                    }
                };
                let tables: Vec<TableRef> = entries
                    .into_iter()
                    .filter(|entry| entry.kind == TableKind::Table)
                    .map(|entry| entry.table_ref())
                    .collect();
                let capped = tables.len() > COMPARE_TABLE_CAP;
                (tables, capped)
            } else {
                (source_tables, capped)
            };
            if source_tables.is_empty() {
                return None;
            }
            let report = match scope {
                CompareScope::Schema => compare_schema(
                    &source_conn,
                    &target_conn,
                    &source_tables,
                    &source_ns,
                    &target_ns,
                    ignore_comments,
                    routines,
                ),
                CompareScope::Data => {
                    compare_data(&source_conn, &target_conn, &source_tables, &target_ns)
                }
            };
            Some(report.map(|mut report| {
                report.capped = capped;
                report
            }))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            cx.update(move |cx| {
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    this.status = match result {
                        Some(Ok(report)) => CompareStatus::Ready(report),
                        Some(Err(error)) => CompareStatus::Failed(error.to_string()),
                        None => {
                            CompareStatus::Failed(tr(cx, "源库里没有可比对的基础表。").to_owned())
                        }
                    };
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// 保存配置（单键 JSON），给「下次还是这两台机器」省一次选择。
    fn save_config(&self, window: &mut Window, cx: &mut Context<Self>) {
        let config = CompareConfig {
            scope: Some(self.spec.scope),
            source_conn: Some(self.spec.source_conn.clone()),
            source_ns: Some(self.spec.source_ns.clone()),
            target_conn: Some(self.spec.target_conn.clone()),
            target_ns: Some(self.spec.target_ns.clone()),
            ignore_comments: Some(self.spec.ignore_comments),
            routines: Some(self.spec.routines),
        };
        match serde_json::to_string(&config) {
            Ok(json) => {
                duck_store::kv_put(cx, KEY_COMPARE_CONFIG, json);
                // 不留在去抖窗口里：紧接着点「加载配置」的读路径必须看到这次写入。
                duck_store::flush(cx);
                notify::info(tr(cx, "已保存比较配置。"), window, cx);
            }
            Err(error) => notify::error(
                trf(cx, "保存比较配置失败：{}", &[&error.to_string()]),
                window,
                cx,
            ),
        }
    }

    /// 加载配置：连接已断开的那一侧跳过（不把用户送进一个必然失败的规格）。
    fn load_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(raw) = duck_store::kv_get(cx, KEY_COMPARE_CONFIG) else {
            notify::info(tr(cx, "没有已保存的比较配置。"), window, cx);
            return;
        };
        let Ok(config) = serde_json::from_str::<CompareConfig>(&raw) else {
            notify::error(tr(cx, "已保存的比较配置读不出来。"), window, cx);
            return;
        };
        let Some(page) = self.page.upgrade() else {
            return;
        };
        let (has_source, has_target) = {
            let page = page.read(cx);
            let alive = |conn: &Option<String>| {
                conn.as_deref()
                    .is_some_and(|conn| page.sessions.contains_key(conn))
            };
            (alive(&config.source_conn), alive(&config.target_conn))
        };
        if let Some(scope) = config.scope {
            self.spec.scope = scope;
        }
        if has_source {
            self.spec.source_conn = config.source_conn.unwrap_or_default();
            self.spec.source_ns = config.source_ns.unwrap_or_default();
        }
        if has_target {
            self.spec.target_conn = config.target_conn.unwrap_or_default();
            self.spec.target_ns = config.target_ns.unwrap_or_default();
        }
        if let Some(ignore_comments) = config.ignore_comments {
            self.spec.ignore_comments = ignore_comments;
        }
        if let Some(routines) = config.routines {
            self.spec.routines = routines;
        }
        if !has_source || !has_target {
            notify::info(tr(cx, "配置里的连接已断开，已跳过那一侧。"), window, cx);
        }
        self.stamps = [None, None, None, None];
        self.status = CompareStatus::Idle;
        self.pane = ComparePane::Config;
        cx.notify();
    }

    /// 把目标缺失的主键行从源库拉回来（前 [`COMPARE_BACKFILL_CAP`] 行），
    /// 按源方言拼 INSERT 放进编辑器。
    fn generate_backfill(
        &mut self,
        table: TableRef,
        pk: Vec<String>,
        keys: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.page.upgrade() else {
            return;
        };
        let spec = self.spec.clone();
        let (conn, quote) = {
            let page = page.read(cx);
            let Some(session) = page.sessions.get(&spec.source_conn) else {
                return;
            };
            (session.conn.clone(), session.quote.unwrap_or('"'))
        };
        let Ok(qualified) = quote_qualified(&table, quote) else {
            return;
        };
        let column_list = pk
            .iter()
            .filter_map(|column| duck_db::drivers::quote_identifier(column, quote).ok())
            .collect::<Vec<_>>()
            .join(", ");
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let task = cx
                .background_executor()
                .spawn(async move {
                    let _permit = db_op_gate().acquire().await;
                    backfill_sql(&conn, &qualified, &column_list, &keys, quote)
                })
                .await;
            cx.update(move |cx| {
                crate::update::on_notify_window(cx, move |window, cx| {
                    let _ = view.update(cx, |this, cx| match task {
                        Ok((sql, count)) => {
                            if sql.is_empty() {
                                notify::info(tr(cx, "没有需要补的行。"), window, cx);
                                return;
                            }
                            let note = trf(
                                cx,
                                "已生成 {} 条补数 INSERT，确认无误后再执行。",
                                &[&count.to_string()],
                            );
                            if let Some(page) = this.page.upgrade() {
                                page.update(cx, |page, cx| {
                                    page.stage_sql(sql, note.into(), window, cx);
                                });
                            }
                        }
                        Err(error) => notify::error(
                            trf(cx, "读取缺失行失败：{}", &[&error.to_string()]),
                            window,
                            cx,
                        ),
                    });
                });
            });
        })
        .detach();
    }

    /// 把结构差异拼成 ALTER 脚本放进编辑器（**只放不跑**）。
    ///
    /// 为什么必须手动触发：结构比较是只读的，改目标库是不可逆动作。这里只做
    /// 「把差异翻译成语法正确的起点」，落不落库由用户在编辑器里按「执行」决定 ——
    /// 与项目里其它写路径同一条人工审批纪律。
    ///
    /// 生成完就关弹窗：脚本要落到**弹窗后面的**编辑器里，不关的话用户看不到结果，
    /// 而再点一次会覆盖上一次的脚本（`stage_sql` 是替换语义）。
    fn generate_alter(&mut self, targets: Vec<AlterTarget>, cx: &mut Context<Self>) {
        if targets.is_empty() || self.busy {
            return;
        }
        let Some(page) = self.page.upgrade() else {
            return;
        };
        let spec = self.spec.clone();
        let prepared = {
            let page = page.read(cx);
            match (
                page.sessions.get(&spec.source_conn),
                page.sessions.get(&spec.target_conn),
            ) {
                // 方言与引号都取**目标**侧：脚本是要在目标库上跑的。
                (Some(source), Some(target)) => target
                    .quote
                    .map(|quote| (source.conn.clone(), target.conn.clone(), quote, target.kind)),
                _ => None,
            }
        };
        let Some((source_conn, target_conn, quote, kind)) = prepared else {
            crate::update::on_notify_window(cx, move |window, cx| {
                notify::error(tr(cx, "连接已断开，或目标库不是 SQL 引擎。"), window, cx);
            });
            return;
        };
        let target_ns = spec.target_ns.clone();
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let task = cx
                .background_executor()
                .spawn(async move {
                    let _permit = db_op_gate().acquire().await;
                    alter_sql(
                        &source_conn,
                        &target_conn,
                        &targets,
                        &target_ns,
                        kind,
                        quote,
                    )
                })
                .await;
            cx.update(move |cx| {
                crate::update::on_notify_window(cx, move |window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        let (sql, count) = task;
                        if sql.is_empty() {
                            notify::info(tr(cx, "结构上没有可生成的 ALTER。"), window, cx);
                            return;
                        }
                        let note = trf(
                            cx,
                            "已生成 {} 条 ALTER 语句，确认无误后再执行。",
                            &[&count.to_string()],
                        );
                        let Some(page) = this.page.upgrade() else {
                            return;
                        };
                        window.close_dialog(cx);
                        page.update(cx, |page, cx| {
                            page.stage_sql(sql, note.into(), window, cx);
                        });
                    });
                });
            });
        })
        .detach();
    }
}

impl Render for CompareDialogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_pickers(window, cx);
        let (tokens, extended) = {
            let theme = active_theme(cx);
            (theme.tokens().clone(), theme.extended().clone())
        };
        let body: AnyElement = match self.pane {
            ComparePane::Config => self.render_config(&tokens, &extended, cx),
            ComparePane::Result => self.render_result(&tokens, &extended, cx),
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

impl CompareDialogView {
    /// 配置页：内容档位 + 两侧选择 + 选项卡片 + 底部动作条。
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
        let scope = self.spec.scope;
        let tabs = segmented_tabs(
            "db-compare-scope",
            [CompareScope::Schema.label(cx), CompareScope::Data.label(cx)],
            scope.index(),
            {
                let view = view.clone();
                move |index, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.spec.scope = CompareScope::from_index(index);
                        this.status = CompareStatus::Idle;
                        cx.notify();
                    });
                }
            },
            cx,
        );
        // 两侧对称：源（强调色）⇄ 目标（成功色）。
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
            tr(cx, "源数据库"),
            extended.colors.accent_text,
            cx,
        );
        let target_side = side(
            PICK_TARGET_CONN,
            PICK_TARGET_NS,
            tr(cx, "目标数据库"),
            extended.colors.success,
            cx,
        );
        let swap = icon_button(
            "db-compare-swap",
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
        // 选项卡片。
        let preset_tables = !self.spec.tables.is_empty();
        let tables_caption = if preset_tables && !self.spec.all_tables {
            trf(
                cx,
                "仅比较选中的 {} 张表（从表右键进入）。打开开关 = 比较库内全部基础表。",
                &[&self.spec.tables.len().to_string()],
            )
        } else {
            tr(cx, "当前比较库内全部基础表；从表右键进入可只比较选中的表。").to_owned()
        };
        let options = v_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(tokens.colors.border)
            .bg(tokens.colors.surface)
            .p(tokens.spacing.sm)
            .child(
                v_flex()
                    .gap(tokens.spacing.xxs)
                    .child(
                        h_flex()
                            .items_center()
                            .gap(tokens.spacing.sm)
                            .child(
                                // gpui-base 的 `Switch` 没有 bool 版 `.disabled()`：
                                // 没有预选表时闸门在回调里拦 + 外层淡出。
                                div()
                                    .opacity(if preset_tables {
                                        1.
                                    } else {
                                        extended.opacity.disabled
                                    })
                                    .child(switch_control(
                                        "db-compare-tables",
                                        self.spec.all_tables || !preset_tables,
                                        {
                                            let view = view.clone();
                                            move |checked, _, cx| {
                                                // 没有预选表时这个开关没有语义（「指定表」
                                                // 只能从表右键进来），闸门就在这里拦。
                                                if !preset_tables {
                                                    return;
                                                }
                                                let checked = *checked;
                                                let _ = view.update(cx, |this, cx| {
                                                    this.spec.all_tables = checked;
                                                    cx.notify();
                                                });
                                            }
                                        },
                                    )),
                            )
                            .child(body_text(tr(cx, "表"), cx)),
                    )
                    .child(meta_text(SharedString::from(tables_caption), cx)),
            )
            .child(div().h(px(1.)).w_full().bg(extended.colors.hairline))
            .child(
                v_flex()
                    .gap(tokens.spacing.xxs)
                    .child(
                        h_flex()
                            .items_center()
                            .gap(tokens.spacing.sm)
                            .child(
                                div()
                                    .opacity(if scope == CompareScope::Schema {
                                        1.
                                    } else {
                                        extended.opacity.disabled
                                    })
                                    .child(switch_control(
                                        "db-compare-routines",
                                        self.spec.routines,
                                        {
                                            let view = view.clone();
                                            move |checked, _, cx| {
                                                // 数据比较不拉例程定义：这一档没有语义。
                                                if scope != CompareScope::Schema {
                                                    return;
                                                }
                                                let checked = *checked;
                                                let _ = view.update(cx, |this, cx| {
                                                    this.spec.routines = checked;
                                                    cx.notify();
                                                });
                                            }
                                        },
                                    )),
                            )
                            .child(body_text(tr(cx, "存储过程 / 函数"), cx)),
                    )
                    .child(meta_text(
                        SharedString::from(tr(
                            cx,
                            "只在结构比较时生效；MySQL / PostgreSQL / SQL Server 能取到定义原文，其余引擎只比存在性。",
                        )),
                        cx,
                    )),
            )
            .child(div().h(px(1.)).w_full().bg(extended.colors.hairline))
            .child(
                v_flex()
                    .gap(tokens.spacing.xxs)
                    .child(
                        h_flex()
                            .items_center()
                            .gap(tokens.spacing.sm)
                            .child(
                                div()
                                    .opacity(if scope == CompareScope::Schema {
                                        1.
                                    } else {
                                        extended.opacity.disabled
                                    })
                                    .child(switch_control(
                                        "db-compare-ignore-comments",
                                        self.spec.ignore_comments,
                                        {
                                            let view = view.clone();
                                            move |checked, _, cx| {
                                                // 数据比较不看注释：这一档没有语义。
                                                if scope != CompareScope::Schema {
                                                    return;
                                                }
                                                let checked = *checked;
                                                let _ = view.update(cx, |this, cx| {
                                                    this.spec.ignore_comments = checked;
                                                    cx.notify();
                                                });
                                            }
                                        },
                                    )),
                            )
                            .child(body_text(tr(cx, "忽略注释差异"), cx)),
                    )
                    .child(meta_text(
                        SharedString::from(tr(cx, "只在结构比较时生效。")),
                        cx,
                    )),
            );
        // 开跑前置缺哪项就明说哪项：按钮禁用只告诉用户「点不动」，缺项清单才
        // 告诉用户该去补哪个下拉（数据比较「点了没反应」的教训）。
        let missing: Vec<&str> = [
            (self.spec.source_conn.is_empty(), tr(cx, "源连接")),
            (self.spec.source_ns.is_empty(), tr(cx, "源数据库")),
            (self.spec.target_conn.is_empty(), tr(cx, "目标连接")),
            (self.spec.target_ns.is_empty(), tr(cx, "目标数据库")),
        ]
        .into_iter()
        .filter_map(|(missing, label)| missing.then_some(label))
        .collect();
        let ready_hint = if missing.is_empty() {
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
        // 底部动作条：保存 / 加载配置（左），开始比较（右）。
        let run = button(
            "db-compare-run",
            tr(cx, "开始比较"),
            ButtonVariant::Primary,
            cx,
        )
        .disabled(!self.ready())
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                let _ = view.update(cx, |this, cx| this.run(cx));
            }
        });
        let footer = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .pt(tokens.spacing.sm)
            .child(
                button(
                    "db-compare-save",
                    tr(cx, "保存配置"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.save_config(window, cx));
                    }
                }),
            )
            .child(
                button(
                    "db-compare-load",
                    tr(cx, "加载配置"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.load_config(window, cx));
                    }
                }),
            )
            .child(div().flex_1())
            .child(run);

        v_flex()
            .w_full()
            .min_w_0()
            .gap(tokens.spacing.md)
            .child(h_flex().items_center().gap(tokens.spacing.sm).child(tabs))
            .child(
                h_flex()
                    .w_full()
                    .items_end()
                    .gap(tokens.spacing.sm)
                    .child(source_side)
                    .child(div().flex_none().pb(tokens.spacing.sm).child(swap))
                    .child(target_side),
            )
            .child(options)
            .children(ready_hint)
            .child(footer)
            .into_any_element()
    }

    /// 结果页：汇总统计 + 筛选 + 逐表明细 + 底部动作条。
    fn render_result(
        &self,
        tokens: &duck_theme::SemanticThemeTokens,
        extended: &duck_theme::ExtendedTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity().downgrade();
        let mut header = h_flex().w_full().items_center().gap(tokens.spacing.xxs);
        let mut body: AnyElement = div().into_any_element();
        match &self.status {
            CompareStatus::Loading => {
                body = v_flex()
                    .w_full()
                    .items_center()
                    .justify_center()
                    .py(tokens.spacing.xxl)
                    .child(meta_text(SharedString::from(tr(cx, "正在比较…")), cx))
                    .into_any_element();
            }
            CompareStatus::Failed(reason) => {
                body = v_flex()
                    .w_full()
                    .items_center()
                    .justify_center()
                    .py(tokens.spacing.xxl)
                    .child(meta_text(
                        SharedString::from(trf(cx, "比较失败：{}", &[reason.as_str()])),
                        cx,
                    ))
                    .into_any_element();
            }
            CompareStatus::Ready(report) => {
                let counts = report.counts(self.spec.ignore_comments);
                let filters = [
                    (
                        CompareFilter::All,
                        "all",
                        tr(cx, "全部"),
                        counts.iter().sum::<usize>(),
                    ),
                    (
                        CompareFilter::Changed,
                        "changed",
                        tr(cx, "有差异"),
                        counts[1],
                    ),
                    (
                        CompareFilter::OnlySource,
                        "only-source",
                        tr(cx, "仅源有"),
                        counts[2],
                    ),
                    (
                        CompareFilter::OnlyTarget,
                        "only-target",
                        tr(cx, "仅目标有"),
                        counts[3],
                    ),
                ];
                for (filter, key, label, count) in filters {
                    let active = self.filter == filter;
                    header = header.child(
                        button(
                            SharedString::from(format!("db-compare-filter-{key}")),
                            SharedString::from(trf(cx, "{} {}", &[label, &count.to_string()])),
                            if active {
                                ButtonVariant::Secondary
                            } else {
                                ButtonVariant::Ghost
                            },
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.filter = filter;
                                    cx.notify();
                                });
                            }
                        }),
                    );
                }
                header = header.child(div().flex_1()).child(meta_text(
                    SharedString::from(trf(cx, "一致 {}", &[&counts[0].to_string()])),
                    cx,
                ));
                let mut rows = v_flex().w_full().gap(tokens.spacing.xxs);
                let mut shown = 0usize;
                for row in &report.rows {
                    if !self.filter.accepts(row.bucket(self.spec.ignore_comments)) {
                        continue;
                    }
                    shown += 1;
                    rows = rows.child(self.render_row(row, tokens, extended, cx));
                }
                if shown == 0 {
                    rows = rows.child(div().w_full().py(tokens.spacing.xl).text_center().child(
                        meta_text(
                            // 「表」不够用了：打开例程开关后这份报告里也有存储过程 / 函数。
                            SharedString::from(tr(cx, "这个筛选下没有可比对的对象。")),
                            cx,
                        ),
                    ));
                }
                let mut list = div()
                    .id("db-compare-scroll")
                    .w_full()
                    .max_h(px(COMPARE_LIST_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .rounded(tokens.radius.md)
                    .border_1()
                    .border_color(tokens.colors.border)
                    .bg(tokens.colors.surface)
                    .p(tokens.spacing.xs)
                    .child(rows);
                if report.capped {
                    list = list.child(div().px(tokens.spacing.xs).py(tokens.spacing.xs).child(
                        meta_text(
                            SharedString::from(trf(
                                cx,
                                "比对的表数达上限 {}，其余表未比对。",
                                &[&COMPARE_TABLE_CAP.to_string()],
                            )),
                            cx,
                        ),
                    ));
                }
                body = list.into_any_element();
            }
            CompareStatus::Idle => {}
        }
        // 口径说明单独一行：塞进底部动作条会把按钮挤出去（长句也不该出现在按钮行里）。
        let hint = meta_text(
            SharedString::from(match self.spec.scope {
                CompareScope::Schema => tr(
                    cx,
                    "结构差异只读；需要改目标库就手动生成 ALTER（补列 / 改类型 / 建表），脚本只进编辑器、不自动执行。",
                ),
                CompareScope::Data => {
                    tr(cx, "数据差异只到主键行级；单元格级逐列对比不在本期口径。")
                }
            }),
            cx,
        );
        let can_return = !self.busy;
        // 「生成全部 ALTER」只在结构档出现：数据档没有结构差异可言。
        let alter_all: Vec<AlterTarget> = match (&self.status, self.spec.scope) {
            (CompareStatus::Ready(report), CompareScope::Schema) => report
                .rows
                .iter()
                .filter_map(|row| row.alter_target(self.spec.scope))
                .collect(),
            _ => Vec::new(),
        };
        let mut footer = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .pt(tokens.spacing.sm)
            .child(
                button(
                    "db-compare-back",
                    tr(cx, "返回配置"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(!can_return)
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.pane = ComparePane::Config;
                            cx.notify();
                        });
                    }
                }),
            );
        if self.spec.scope == CompareScope::Schema {
            footer = footer.child(
                button(
                    "db-compare-alter-all",
                    tr(cx, "生成全部 ALTER"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(self.busy || alter_all.is_empty())
                .on_click({
                    let alter_all = alter_all.clone();
                    let view = view.clone();
                    move |_, _, cx| {
                        let targets = alter_all.clone();
                        let _ = view.update(cx, |this, cx| this.generate_alter(targets, cx));
                    }
                }),
            );
        }
        let footer = footer
            .child(div().flex_1())
            .child(
                button(
                    "db-compare-close",
                    tr(cx, "关闭"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                button(
                    "db-compare-rerun",
                    tr(cx, "重新比较"),
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(!self.ready())
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.run(cx));
                    }
                }),
            );
        v_flex()
            .w_full()
            .min_w_0()
            .min_h_0()
            .gap(tokens.spacing.sm)
            .child(header)
            .child(hint)
            .child(body)
            .child(footer)
            .into_any_element()
    }

    /// 报告的一行：表名 + 结论 chip + 差异明细 + 动作。
    fn render_row(
        &self,
        row: &CompareRow,
        tokens: &duck_theme::SemanticThemeTokens,
        extended: &duck_theme::ExtendedTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bucket = row.bucket(self.spec.ignore_comments);
        let (chip, state_label) = match bucket {
            CompareBucket::Same => (neutral_tag(SharedString::from(tr(cx, "一致")), cx), "same"),
            CompareBucket::Changed => (
                accent_tag(SharedString::from(tr(cx, "有差异")), cx),
                "changed",
            ),
            CompareBucket::OnlySource => (
                accent_tag(SharedString::from(tr(cx, "仅源有")), cx),
                "only-source",
            ),
            CompareBucket::OnlyTarget => (
                accent_tag(SharedString::from(tr(cx, "仅目标有")), cx),
                "only-target",
            ),
        };
        let detail: SharedString = match &row.state {
            CompareRowState::OnlySource { rows } => SharedString::from(match rows {
                Some(rows) => trf(cx, "{} 行", &[&rows.to_string()]),
                None => tr(cx, "源表存在，目标缺整张表。").to_owned(),
            }),
            CompareRowState::OnlyTarget { rows } => SharedString::from(match rows {
                Some(rows) => trf(cx, "{} 行", &[&rows.to_string()]),
                None => tr(cx, "目标表存在，源缺整张表。").to_owned(),
            }),
            CompareRowState::Data {
                source_rows,
                target_rows,
                columns_missing_in_target,
                columns_missing_in_source,
                missing_in_target,
                missing_in_source,
                pk,
                pk_compared,
            } => {
                let mut parts: Vec<String> = Vec::new();
                parts.push(trf(
                    cx,
                    "行数 源 {} / 目标 {}",
                    &[&source_rows.to_string(), &target_rows.to_string()],
                ));
                if !columns_missing_in_target.is_empty() {
                    parts.push(trf(
                        cx,
                        "目标缺列 {}",
                        &[&columns_missing_in_target.join(", ")],
                    ));
                }
                if !columns_missing_in_source.is_empty() {
                    parts.push(trf(
                        cx,
                        "源缺列 {}",
                        &[&columns_missing_in_source.join(", ")],
                    ));
                }
                if *pk_compared {
                    if !missing_in_target.is_empty() {
                        let sample: Vec<String> =
                            missing_in_target.iter().take(5).cloned().collect();
                        parts.push(trf(
                            cx,
                            "目标缺失 {} 行（如 {}…）",
                            &[&missing_in_target.len().to_string(), &sample.join(", ")],
                        ));
                    }
                    if !missing_in_source.is_empty() {
                        parts.push(trf(
                            cx,
                            "目标多出 {} 行",
                            &[&missing_in_source.len().to_string()],
                        ));
                    }
                    if missing_in_target.is_empty() && missing_in_source.is_empty() {
                        parts.push(tr(cx, "主键级行集一致").to_owned());
                    }
                } else if !pk.is_empty() {
                    parts.push(tr(cx, "主键级行集未比对（键数超上限）").to_owned());
                } else {
                    parts.push(tr(cx, "无主键，未做行级比对").to_owned());
                }
                SharedString::from(parts.join("；"))
            }
            CompareRowState::Schema {
                column_count,
                columns_missing_in_target,
                columns_missing_in_source,
                type_mismatches,
                comment_mismatches,
                pk_mismatch,
            } => {
                let mut parts: Vec<String> = Vec::new();
                if !columns_missing_in_target.is_empty() {
                    parts.push(trf(
                        cx,
                        "目标缺列 {}",
                        &[&columns_missing_in_target.join(", ")],
                    ));
                }
                if !columns_missing_in_source.is_empty() {
                    parts.push(trf(
                        cx,
                        "源缺列 {}",
                        &[&columns_missing_in_source.join(", ")],
                    ));
                }
                for mismatch in type_mismatches {
                    parts.push(trf(
                        cx,
                        "类型不一致 {}：{} → {}",
                        &[&mismatch.column, &mismatch.source, &mismatch.target],
                    ));
                }
                if !self.spec.ignore_comments && !comment_mismatches.is_empty() {
                    parts.push(trf(cx, "注释不一致 {}", &[&comment_mismatches.join(", ")]));
                }
                if let Some((source, target)) = pk_mismatch {
                    let none_label = tr(cx, "无").to_owned();
                    let source_label = if source.is_empty() {
                        none_label.clone()
                    } else {
                        source.join(", ")
                    };
                    let target_label = if target.is_empty() {
                        none_label
                    } else {
                        target.join(", ")
                    };
                    parts.push(trf(
                        cx,
                        "主键不一致：源 {} / 目标 {}",
                        &[&source_label, &target_label],
                    ));
                }
                if parts.is_empty() {
                    parts.push(trf(cx, "结构一致（{} 列）", &[&column_count.to_string()]));
                }
                SharedString::from(parts.join("；"))
            }
            CompareRowState::Routine {
                kind,
                in_source,
                in_target,
                source_definition,
                target_definition,
            } => {
                let mut parts: Vec<String> = vec![table_kind_label(*kind, cx)];
                if !*in_target {
                    parts.push(tr(cx, "目标缺这个例程").to_owned());
                } else if !*in_source {
                    parts.push(tr(cx, "源库缺这个例程").to_owned());
                } else if definitions_differ(
                    source_definition.as_deref(),
                    target_definition.as_deref(),
                ) {
                    // 定义原文动辄上百行，行里只报规模差异，原文走「查看定义」。
                    parts.push(trf(
                        cx,
                        "定义不一致（源 {} 行 / 目标 {} 行）",
                        &[
                            &definition_lines(source_definition.as_deref()).to_string(),
                            &definition_lines(target_definition.as_deref()).to_string(),
                        ],
                    ));
                } else if source_definition.is_some() && target_definition.is_some() {
                    parts.push(tr(cx, "定义一致").to_owned());
                } else {
                    parts.push(tr(cx, "定义取不到，只比了存在性").to_owned());
                }
                SharedString::from(parts.join("；"))
            }
        };
        // 动作：数据 → 补数 SQL / 传输；结构 → 看结构 / 传输。
        let mut actions = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        if let CompareRowState::Data {
            missing_in_target,
            pk,
            ..
        } = &row.state
            && !missing_in_target.is_empty()
            && !pk.is_empty()
        {
            let pk = pk.clone();
            let keys = missing_in_target.clone();
            let table = row.table.clone();
            let view = cx.entity().downgrade();
            actions = actions.child(
                button(
                    SharedString::from(format!("db-compare-backfill-{}", row.table.name)),
                    tr(cx, "生成补数 SQL"),
                    ButtonVariant::Link,
                    cx,
                )
                .disabled(self.busy)
                .on_click(move |_, _, cx| {
                    let (table, pk, keys) = (table.clone(), pk.clone(), keys.clone());
                    let _ = view.update(cx, |this, cx| {
                        this.generate_backfill(table, pk, keys, cx);
                    });
                }),
            );
        }
        // 结构差异 → 生成 ALTER（**手动触发**：只进编辑器，不落库）。
        if let Some(target) = row.alter_target(self.spec.scope) {
            let missing_table = target.missing_table;
            let view = cx.entity().downgrade();
            actions = actions.child(
                button(
                    SharedString::from(format!(
                        "db-compare-alter-{state_label}-{}",
                        row.table.name
                    )),
                    if missing_table {
                        tr(cx, "生成建表 SQL")
                    } else {
                        tr(cx, "生成 ALTER")
                    },
                    ButtonVariant::Link,
                    cx,
                )
                .disabled(self.busy)
                .on_click(move |_, _, cx| {
                    let targets = vec![target.clone()];
                    let _ = view.update(cx, |this, cx| this.generate_alter(targets, cx));
                }),
            );
        }
        if let CompareRowState::Routine {
            source_definition: Some(definition),
            ..
        } = &row.state
        {
            let definition = definition.clone();
            let view = cx.entity().downgrade();
            actions = actions.child(
                button(
                    SharedString::from(format!("db-compare-routine-{}", row.table.name)),
                    tr(cx, "查看定义"),
                    ButtonVariant::Link,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let definition = definition.clone();
                    let _ = view.update(cx, |this, cx| {
                        let Some(page) = this.page.upgrade() else {
                            return;
                        };
                        window.close_dialog(cx);
                        page.update(cx, |page, cx| {
                            page.stage_sql(
                                definition,
                                tr(cx, "例程定义原文（取自源库），只读预览。").into(),
                                window,
                                cx,
                            );
                        });
                    });
                }),
            );
        }
        if matches!(
            &row.state,
            CompareRowState::Data { .. }
                | CompareRowState::Schema { .. }
                | CompareRowState::OnlySource { .. }
        ) {
            let table = row.table.clone();
            let view = cx.entity().downgrade();
            actions = actions.child(
                button(
                    SharedString::from(format!(
                        "db-compare-transfer-{state_label}-{}",
                        row.table.name
                    )),
                    tr(cx, "传输此表"),
                    ButtonVariant::Link,
                    cx,
                )
                .disabled(self.busy)
                .on_click(move |_, window, cx| {
                    let table = table.clone();
                    let _ = view.update(cx, |this, cx| {
                        let Some(page) = this.page.upgrade() else {
                            return;
                        };
                        let (conn, ns) =
                            (this.spec.source_conn.clone(), this.spec.source_ns.clone());
                        let name = table.name.clone();
                        window.close_dialog(cx);
                        page.update(cx, |page, cx| {
                            page.open_transfer_dialog(&conn, &ns, vec![name], window, cx);
                        });
                    });
                }),
            );
        }
        if matches!(&row.state, CompareRowState::Schema { .. }) {
            let table = row.table.clone();
            let view = cx.entity().downgrade();
            actions = actions.child(
                button(
                    SharedString::from(format!("db-compare-structure-{}", row.table.name)),
                    tr(cx, "查看结构"),
                    ButtonVariant::Link,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let table = table.clone();
                    let _ = view.update(cx, |this, cx| {
                        let Some(page) = this.page.upgrade() else {
                            return;
                        };
                        let conn = this.spec.source_conn.clone();
                        window.close_dialog(cx);
                        page.update(cx, |page, cx| {
                            page.open_structure_tab(&conn, table.clone(), false, window, cx);
                        });
                    });
                }),
            );
        }
        h_flex()
            .id(SharedString::from(format!(
                "db-compare-row-{state_label}-{}",
                row.table.name
            )))
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
                    .max_w(px(240.))
                    .text_size(tokens.typography.sm.size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.foreground)
                    .truncate()
                    .child(selectable_text_with_id(
                        SharedString::from(format!(
                            "db-compare-table-{state_label}-{}",
                            row.table.name
                        )),
                        SharedString::from(row.table.display()),
                    )),
            )
            .child(chip)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(tokens.typography.xs.size)
                    .child(meta_text(detail, cx)),
            )
            .child(actions)
            .into_any_element()
    }
}

impl DatabasePage {
    /// 打开比较弹窗：源 = 打开菜单所在的连接 + 命名空间，目标默认挑另一条已连接会话。
    pub(super) fn open_compare_dialog(
        &mut self,
        scope: CompareScope,
        conn_id: &str,
        namespace: &str,
        tables: Vec<TableRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        // 会话的默认命名空间（与配置页「数据库」下拉同一份来源）。
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
        // 目标连接默认 = 第一条「命名空间能用」的**别的**已连接会话；一条都没有
        // 才退回随便挑（再不行就源自己，同库对账也有意义）。不能不加挑拣地拿
        // 第一条别的会话：HashMap 顺序随机，挑中一条命名空间为空的会话（agent
        // 轨没报出库清单很常见）目标库就无从选起，「开始比较」永远点不动 ——
        // 那正是数据比较「点了没反应」的根因。
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
        let spec = CompareSpec {
            scope,
            source_conn: conn_id.to_owned(),
            source_ns: namespace.to_owned(),
            target_conn,
            target_ns,
            all_tables: tables.is_empty(),
            tables,
            ignore_comments: false,
            routines: true,
        };
        let page = cx.entity().downgrade();
        let view = cx.new(|cx| CompareDialogView::new(page, spec, window, cx));
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let view = view.clone();
            let drag_for_content = drag.clone();
            dialog
                .w(px(COMPARE_DIALOG_WIDTH))
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
                                "db-compare-title",
                                &drag_for_content,
                                tr(cx, "比较"),
                                cx,
                            ))
                            .child(view.clone()),
                    ))
                })
        });
    }
}

/// 数据比较：两侧 `COUNT(*)` + 列名 + 主键级行差。
fn compare_data(
    source_conn: &Arc<Mutex<Connection>>,
    target_conn: &Arc<Mutex<Connection>>,
    source_tables: &[TableRef],
    target_ns: &str,
) -> Result<CompareReport, DbError> {
    // 目标侧目录：一次取齐表名（与建连同一条内省路径）。
    let target_names: BTreeSet<String> = {
        let mut guard = target_conn.lock().expect("连接锁不应中毒");
        Introspection::load(&mut guard, Some(target_ns))?
            .catalog
            .iter()
            .filter(|entry| entry.kind == TableKind::Table)
            .map(|entry| entry.name.clone())
            .collect()
    };
    let source_names: BTreeSet<String> = source_tables
        .iter()
        .map(|table| table.name.clone())
        .collect();
    let mut rows: Vec<CompareRow> = Vec::new();
    // 深比较只跑前 [`COMPARE_TABLE_CAP`] 张（逐表查询是分钟级开销）；表名清单是
    // 全量的，「仅目标有」判定不受上限影响 —— 超出部分靠 `capped` 提示交代。
    for table in source_tables.iter().take(COMPARE_TABLE_CAP) {
        if !target_names.contains(&table.name) {
            let count = {
                let mut guard = source_conn.lock().expect("连接锁不应中毒");
                count_rows(&mut guard, table)?
            };
            rows.push(CompareRow {
                table: table.clone(),
                state: CompareRowState::OnlySource { rows: Some(count) },
            });
            continue;
        }
        let target_ref = TableRef {
            schema: Some(target_ns.to_owned()),
            name: table.name.clone(),
        };
        // 两侧各取行数 / 列名 / 主键（哪侧失败都让整次比较失败：半份报告比没有更害人）。
        let (source_rows, source_columns, source_pk) = {
            let mut guard = source_conn.lock().expect("连接锁不应中毒");
            let count = count_rows(&mut guard, table)?;
            let columns = guard
                .table_columns(table)?
                .into_iter()
                .map(|column| column.name)
                .collect::<Vec<_>>();
            let pk = guard.table_primary_key(table).unwrap_or_default();
            (count, columns, pk)
        };
        let (target_rows, target_columns, target_pk) = {
            let mut guard = target_conn.lock().expect("连接锁不应中毒");
            let count = count_rows(&mut guard, &target_ref)?;
            let columns = guard
                .table_columns(&target_ref)?
                .into_iter()
                .map(|column| column.name)
                .collect::<Vec<_>>();
            let pk = guard.table_primary_key(&target_ref).unwrap_or_default();
            (count, columns, pk)
        };
        let columns_missing_in_target: Vec<String> = source_columns
            .iter()
            .filter(|column| !target_columns.contains(column))
            .cloned()
            .collect();
        let columns_missing_in_source: Vec<String> = target_columns
            .iter()
            .filter(|column| !source_columns.contains(column))
            .cloned()
            .collect();
        // 主键级行差：主键同名且非空才跑；键数超上限就明说未比对。
        let pk_usable = !source_pk.is_empty() && source_pk == target_pk;
        let (missing_in_target, missing_in_source, pk_compared) = if pk_usable {
            let source_keys = fetch_pk_values(source_conn, table, &source_pk, COMPARE_PK_CAP)?;
            let target_keys =
                fetch_pk_values(target_conn, &target_ref, &source_pk, COMPARE_PK_CAP)?;
            if source_keys.len() >= COMPARE_PK_CAP || target_keys.len() >= COMPARE_PK_CAP {
                (Vec::new(), Vec::new(), false)
            } else {
                (
                    source_keys.difference(&target_keys).cloned().collect(),
                    target_keys.difference(&source_keys).cloned().collect(),
                    true,
                )
            }
        } else {
            (Vec::new(), Vec::new(), false)
        };
        rows.push(CompareRow {
            table: table.clone(),
            state: CompareRowState::Data {
                source_rows,
                target_rows,
                columns_missing_in_target,
                columns_missing_in_source,
                missing_in_target,
                missing_in_source,
                pk: source_pk,
                pk_compared,
            },
        });
    }
    // 目标独有表。
    for name in &target_names {
        if source_names.contains(name) {
            continue;
        }
        let target_ref = TableRef {
            schema: Some(target_ns.to_owned()),
            name: name.clone(),
        };
        let count = {
            let mut guard = target_conn.lock().expect("连接锁不应中毒");
            count_rows(&mut guard, &target_ref)?
        };
        rows.push(CompareRow {
            table: TableRef {
                schema: None,
                name: name.clone(),
            },
            state: CompareRowState::OnlyTarget { rows: Some(count) },
        });
    }
    rows.sort_by(|left, right| left.table.name.cmp(&right.table.name));
    Ok(CompareReport {
        rows,
        capped: false,
    })
}

/// `true` = 两个类型名等价（大小写与首尾空白不计）。
fn same_type(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

/// 定义文本归一化：统一换行、去掉行尾空白与首尾空行，只留语义相关的部分。
fn normalized_definition(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// 两侧定义是否不同。**拿不到定义时不算差异** —— 那种情况只能比存在性，
/// 硬判「有差异」是假阳性。
fn definitions_differ(source: Option<&str>, target: Option<&str>) -> bool {
    match (source, target) {
        (Some(source), Some(target)) => {
            normalized_definition(source) != normalized_definition(target)
        }
        _ => false,
    }
}

/// 结构比较：两侧逐表取列（名字 / 类型 / 注释）与主键，产出列级 diff；
/// 打开「存储过程 / 函数」时追加例程的存在性与定义差异。
fn compare_schema(
    source_conn: &Arc<Mutex<Connection>>,
    target_conn: &Arc<Mutex<Connection>>,
    source_tables: &[TableRef],
    source_ns: &str,
    target_ns: &str,
    ignore_comments: bool,
    routines: bool,
) -> Result<CompareReport, DbError> {
    let target_names: BTreeSet<String> = {
        let mut guard = target_conn.lock().expect("连接锁不应中毒");
        Introspection::load(&mut guard, Some(target_ns))?
            .catalog
            .iter()
            .filter(|entry| entry.kind == TableKind::Table)
            .map(|entry| entry.name.clone())
            .collect()
    };
    let source_names: BTreeSet<String> = source_tables
        .iter()
        .map(|table| table.name.clone())
        .collect();
    let mut rows: Vec<CompareRow> = Vec::new();
    // 深比较只跑前 [`COMPARE_TABLE_CAP`] 张（同 [`compare_data`] 的口径）；
    // 「仅目标有」判定用的 `source_names` 是全量表名，不受上限影响。
    for table in source_tables.iter().take(COMPARE_TABLE_CAP) {
        if !target_names.contains(&table.name) {
            rows.push(CompareRow {
                table: table.clone(),
                state: CompareRowState::OnlySource { rows: None },
            });
            continue;
        }
        let target_ref = TableRef {
            schema: Some(target_ns.to_owned()),
            name: table.name.clone(),
        };
        let (source_columns, source_pk) = {
            let mut guard = source_conn.lock().expect("连接锁不应中毒");
            let columns = guard.table_columns(table)?;
            let pk = guard.table_primary_key(table).unwrap_or_default();
            (columns, pk)
        };
        let (target_columns, target_pk) = {
            let mut guard = target_conn.lock().expect("连接锁不应中毒");
            let columns = guard.table_columns(&target_ref)?;
            let pk = guard.table_primary_key(&target_ref).unwrap_or_default();
            (columns, pk)
        };
        let target_by_name: BTreeMap<&str, &Column> = target_columns
            .iter()
            .map(|column| (column.name.as_str(), column))
            .collect();
        let source_by_name: BTreeMap<&str, &Column> = source_columns
            .iter()
            .map(|column| (column.name.as_str(), column))
            .collect();
        let mut columns_missing_in_target: Vec<String> = Vec::new();
        let mut type_mismatches: Vec<TypeMismatch> = Vec::new();
        let mut comment_mismatches: Vec<String> = Vec::new();
        for column in &source_columns {
            let Some(target) = target_by_name.get(column.name.as_str()) else {
                columns_missing_in_target.push(column.name.clone());
                continue;
            };
            if !same_type(&column.type_name, &target.type_name) {
                type_mismatches.push(TypeMismatch {
                    column: column.name.clone(),
                    source: column.type_name.clone(),
                    target: target.type_name.clone(),
                });
            } else if !ignore_comments && column.comment.trim() != target.comment.trim() {
                comment_mismatches.push(column.name.clone());
            }
        }
        let columns_missing_in_source: Vec<String> = target_columns
            .iter()
            .filter(|column| !source_by_name.contains_key(column.name.as_str()))
            .map(|column| column.name.clone())
            .collect();
        let pk_mismatch = if source_pk == target_pk {
            None
        } else {
            Some((source_pk, target_pk))
        };
        rows.push(CompareRow {
            table: table.clone(),
            state: CompareRowState::Schema {
                column_count: source_columns.len(),
                columns_missing_in_target,
                columns_missing_in_source,
                type_mismatches,
                comment_mismatches,
                pk_mismatch,
            },
        });
    }
    for name in &target_names {
        if source_names.contains(name) {
            continue;
        }
        rows.push(CompareRow {
            table: TableRef {
                schema: None,
                name: name.clone(),
            },
            state: CompareRowState::OnlyTarget { rows: None },
        });
    }
    if routines {
        rows.extend(compare_routines(
            source_conn,
            target_conn,
            source_ns,
            target_ns,
        )?);
    }
    rows.sort_by(|left, right| {
        left.table
            .name
            .cmp(&right.table.name)
            .then_with(|| routine_order(&left.state).cmp(&routine_order(&right.state)))
    });
    Ok(CompareReport {
        rows,
        capped: false,
    })
}

/// 排序时的档位：表在前、过程居中、函数在后（同名时才有区别）。
fn routine_order(state: &CompareRowState) -> u8 {
    match state {
        CompareRowState::Routine {
            kind: TableKind::Procedure,
            ..
        } => 1,
        CompareRowState::Routine {
            kind: TableKind::Function,
            ..
        } => 2,
        _ => 0,
    }
}

/// 例程对比：两侧目录里的过程 / 函数按 (种类, 名字) 对齐，比存在性与定义原文。
///
/// 定义取不到（引擎不支持 / 权限不足）不当作错误：那一行退化成「只比存在性」，
/// 由行详情如实说明。
fn compare_routines(
    source_conn: &Arc<Mutex<Connection>>,
    target_conn: &Arc<Mutex<Connection>>,
    source_ns: &str,
    target_ns: &str,
) -> Result<Vec<CompareRow>, DbError> {
    let source_items = routine_items(source_conn, source_ns)?;
    let target_items = routine_items(target_conn, target_ns)?;
    // 对齐键 = (种类档位, 名字)：`TableKind` 没实现 `Ord`（它是驱动的公开枚举，不该为了
    // 这里的排序给它加约束），换成 `routine_order_of` 的 u8 就够了 —— 过程与函数本来
    // 就分开比，两种例子不会混在同一个键上。
    let key_of = |kind: TableKind, name: &str| (routine_order_of(kind), name.to_owned());
    let target_keys: BTreeSet<(u8, String)> = target_items
        .iter()
        .map(|(kind, name)| key_of(*kind, name))
        .collect();
    let source_keys: BTreeSet<(u8, String)> = source_items
        .iter()
        .map(|(kind, name)| key_of(*kind, name))
        .collect();
    let mut rows: Vec<CompareRow> = Vec::new();
    for (kind, name) in source_items.iter().take(COMPARE_ROUTINE_CAP) {
        let in_target = target_keys.contains(&key_of(*kind, name));
        let source_definition = routine_definition(source_conn, *kind, source_ns, name);
        let target_definition = if in_target {
            routine_definition(target_conn, *kind, target_ns, name)
        } else {
            None
        };
        rows.push(CompareRow {
            table: TableRef {
                schema: Some(source_ns.to_owned()),
                name: name.clone(),
            },
            state: CompareRowState::Routine {
                kind: *kind,
                in_source: true,
                in_target,
                source_definition,
                target_definition,
            },
        });
    }
    for (kind, name) in &target_items {
        if source_keys.contains(&key_of(*kind, name)) {
            continue;
        }
        rows.push(CompareRow {
            table: TableRef {
                schema: Some(target_ns.to_owned()),
                name: name.clone(),
            },
            state: CompareRowState::Routine {
                kind: *kind,
                in_source: false,
                in_target: true,
                source_definition: None,
                target_definition: routine_definition(target_conn, *kind, target_ns, name),
            },
        });
    }
    Ok(rows)
}

/// 一个命名空间里的过程 / 函数清单（顺序稳定：种类 → 名字）。
fn routine_items(
    conn: &Arc<Mutex<Connection>>,
    namespace: &str,
) -> Result<Vec<(TableKind, String)>, DbError> {
    let mut guard = conn.lock().expect("连接锁不应中毒");
    let mut items: Vec<(TableKind, String)> = guard
        .catalog(Some(namespace))?
        .into_iter()
        .filter(|entry| matches!(entry.kind, TableKind::Procedure | TableKind::Function))
        .map(|entry| (entry.kind, entry.name))
        .collect();
    items.sort_by(|left, right| {
        routine_order_of(left.0)
            .cmp(&routine_order_of(right.0))
            .then_with(|| left.1.cmp(&right.1))
    });
    items.truncate(COMPARE_ROUTINE_CAP);
    Ok(items)
}

/// 例程种类在排序里的档位。
fn routine_order_of(kind: TableKind) -> u8 {
    match kind {
        TableKind::Procedure => 1,
        TableKind::Function => 2,
        _ => 0,
    }
}

/// 取一条例程的定义；失败一律折成 `None`（比较不该因为一条权限不足就整份失败）。
fn routine_definition(
    conn: &Arc<Mutex<Connection>>,
    kind: TableKind,
    namespace: &str,
    name: &str,
) -> Option<String> {
    let mut guard = conn.lock().expect("连接锁不应中毒");
    guard
        .routine_definition(kind, Some(namespace), name)
        .ok()
        .flatten()
}

/// `SELECT COUNT(*)` 一问一答。
fn count_rows(conn: &mut Connection, table: &TableRef) -> Result<u64, DbError> {
    let quote = conn.identifier_quote().unwrap_or('"');
    let qualified = quote_qualified(table, quote)?;
    let outcome = conn.execute(&format!("SELECT COUNT(*) FROM {qualified}"), 1)?;
    let count = outcome
        .result
        .and_then(|set| set.rows.into_iter().next())
        .and_then(|row| row.into_iter().next())
        .map(|value| match value {
            duck_db::Value::Int(value) => value.max(0) as u64,
            duck_db::Value::UInt(value) => value,
            value => value.render().parse::<u64>().unwrap_or(0),
        })
        .unwrap_or(0);
    Ok(count)
}

/// 拉一张表的主键值清单（多列主键按序拼接成一个键串），上限 `cap` 行。
fn fetch_pk_values(
    conn: &Arc<Mutex<Connection>>,
    table: &TableRef,
    pk: &[String],
    cap: usize,
) -> Result<BTreeSet<String>, DbError> {
    let mut guard = conn.lock().expect("连接锁不应中毒");
    let quote = guard.identifier_quote().unwrap_or('"');
    let qualified = quote_qualified(table, quote)?;
    let columns = pk
        .iter()
        .filter_map(|column| duck_db::drivers::quote_identifier(column, quote).ok())
        .collect::<Vec<_>>()
        .join(", ");
    let outcome = guard.execute(&format!("SELECT {columns} FROM {qualified}"), cap)?;
    let keys = outcome
        .result
        .map(|set| {
            set.rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| match value {
                            duck_db::Value::Null => String::from("\u{0}NULL"),
                            value => value.render(),
                        })
                        .collect::<Vec<_>>()
                        .join("\u{1}")
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(keys)
}

/// 按缺失的主键值把整行拉回来，拼成 INSERT（前 [`COMPARE_BACKFILL_CAP`] 行）。
fn backfill_sql(
    conn: &Arc<Mutex<Connection>>,
    qualified: &str,
    column_list: &str,
    keys: &[String],
    quote: char,
) -> Result<(String, usize), DbError> {
    let mut guard = conn.lock().expect("连接锁不应中毒");
    let selected = keys
        .iter()
        .take(COMPARE_BACKFILL_CAP)
        .cloned()
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok((String::new(), 0));
    }
    // 主键值清单转成 IN 列表：值是展示文本（字符串带引号），逐个转义。
    let in_list = selected
        .iter()
        .map(|key| format!("'{}'", key.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let outcome = guard.execute(
        &format!("SELECT * FROM {qualified} WHERE ({column_list}) IN ({in_list});"),
        COMPARE_BACKFILL_CAP,
    )?;
    let result = outcome.result.unwrap_or_default();
    let columns: Vec<String> = result
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    let columns_sql = columns
        .iter()
        .filter_map(|column| duck_db::drivers::quote_identifier(column, quote).ok())
        .collect::<Vec<_>>()
        .join(", ");
    let mut statements = String::new();
    let mut count = 0;
    for row in &result.rows {
        let values = row
            .iter()
            .map(|value| match value {
                duck_db::Value::Null => "NULL".to_owned(),
                value => format!("'{}'", value.render().replace('\'', "''")),
            })
            .collect::<Vec<_>>()
            .join(", ");
        statements.push_str(&format!(
            "INSERT INTO {qualified} ({columns_sql}) VALUES ({values});\n"
        ));
        count += 1;
    }
    Ok((statements, count))
}

/// 一段定义原文有多少行（`None` 记 0：那一侧取不到定义）。
fn definition_lines(definition: Option<&str>) -> usize {
    definition
        .map(|text| normalized_definition(text).lines().count())
        .unwrap_or(0)
}

/// 结构差异 → ALTER 脚本（**只生成，永远不执行**）。
///
/// 口径（本期，界面上的每一档都对得上）：
/// - 目标缺整张表 → 抄源侧 `table_ddl`（只建表，不搬数据；搬数据走「传输此表」）；
/// - 目标缺列 → `ADD COLUMN`（类型 / 可空 / 默认值 / 注释，源侧有的才带）；
/// - 类型不一致 → MySQL / ClickHouse `MODIFY COLUMN`，其余 `ALTER COLUMN …`。
///
/// **不生成** `DROP COLUMN`、不动主键与索引：删列不可逆，主键 / 索引牵一发动全身，
/// 这两类留给结构编辑器人工处理（界面上不给按钮，就没有「点了一下库就变了」的路径）。
///
/// 单张表取不到元数据不打断整批：那个位置留一行注释说明原因 —— 一次「生成全部」
/// 里有一张权限不足的表，不该把另外二十张的脚本一起吞掉。
///
/// `kind` / `quote` 都是**目标**侧的：脚本要在目标库上跑。
fn alter_sql(
    source_conn: &Arc<Mutex<Connection>>,
    target_conn: &Arc<Mutex<Connection>>,
    targets: &[AlterTarget],
    target_ns: &str,
    kind: DatabaseKind,
    quote: char,
) -> (String, usize) {
    let mut lines: Vec<String> = Vec::new();
    for target in targets {
        let target_ref = TableRef {
            schema: Some(target_ns.to_owned()),
            name: target.table.name.clone(),
        };
        let Ok(target_table) = quote_qualified(&target_ref, quote) else {
            continue;
        };
        if target.missing_table {
            let ddl = {
                let mut guard = source_conn.lock().expect("连接锁不应中毒");
                guard.table_ddl(&target.table)
            };
            match ddl {
                Ok(ddl) => {
                    let ddl = ddl.trim().trim_end_matches(';').trim_end();
                    if ddl.is_empty() {
                        continue;
                    }
                    lines.push(format!(
                        "-- 目标缺表：{}（源侧建表语句）",
                        target.table.display()
                    ));
                    // 两侧库名不同时提醒一句：引擎原文可能带库 / schema 前缀。
                    if target
                        .table
                        .schema
                        .as_deref()
                        .is_some_and(|ns| ns != target_ns)
                    {
                        lines.push(
                            "-- 语句取自源库，若其中带库名 / schema 前缀，请改成目标库。"
                                .to_owned(),
                        );
                    }
                    lines.push(format!("{ddl};"));
                    lines.push(String::new());
                }
                Err(error) => lines.push(format!(
                    "-- {}：取建表语句失败（{error}），请改走「传输此表」。",
                    target.table.display()
                )),
            }
            continue;
        }
        let source_columns = {
            let mut guard = source_conn.lock().expect("连接锁不应中毒");
            guard.table_column_meta(&target.table)
        };
        let source_columns = match source_columns {
            Ok(columns) => columns,
            Err(error) => {
                lines.push(format!(
                    "-- {}：取源侧列信息失败（{error}），未生成。",
                    target.table.display()
                ));
                continue;
            }
        };
        let target_columns = {
            let mut guard = target_conn.lock().expect("连接锁不应中毒");
            guard.table_column_meta(&target_ref)
        }
        .unwrap_or_default();
        let target_by_name: BTreeMap<&str, &TableColumnMeta> = target_columns
            .iter()
            .map(|column| (column.name.as_str(), column))
            .collect();
        let mut statements: Vec<String> = Vec::new();
        for column in &source_columns {
            match target_by_name.get(column.name.as_str()) {
                None => push_add_column(&mut statements, kind, &target_table, column, quote),
                Some(existing) => {
                    if !same_type(&column.type_name, &existing.type_name) {
                        push_modify_type(
                            &mut statements,
                            kind,
                            &target_table,
                            &column.name,
                            &column.type_name,
                            quote,
                        );
                    }
                }
            }
        }
        if !statements.is_empty() {
            lines.push(format!("-- {}", target_ref.display()));
            lines.extend(statements);
            lines.push(String::new());
        }
    }
    // 语句数按「非注释、非空行」数：建表 DDL 可能一段里带多条，按行数是保守估计。
    let count = lines
        .iter()
        .filter(|line| !line.is_empty() && !line.starts_with("--"))
        .count();
    (lines.join("\n").trim_end().to_owned(), count)
}

/// 一条 `ADD COLUMN`（目标缺列）。
///
/// 列注释三种写法：MySQL / ClickHouse 内联 `COMMENT`，PG 是独立的
/// `COMMENT ON COLUMN`，其余引擎没有列注释的 DDL 写法 —— 少了这一句只是注释没跟过去，
/// 不该因此不生成补列语句。
fn push_add_column(
    lines: &mut Vec<String>,
    kind: DatabaseKind,
    table_sql: &str,
    column: &TableColumnMeta,
    quote: char,
) {
    let Ok(quoted) = quote_identifier(&column.name, quote) else {
        return;
    };
    let type_name = column.type_name.trim();
    // 没有类型名的列没法 `ADD COLUMN`（Mongo / ES 这类「列」只有名字）。
    if type_name.is_empty() {
        return;
    }
    let mut definition = format!("{quoted} {type_name}");
    // ClickHouse 的可空是 `Nullable(T)` 包在类型里，没有 `NOT NULL` 子句。
    if column.nullable == Some(false) && kind != DatabaseKind::ClickHouse {
        definition.push_str(" NOT NULL");
    }
    if let Some(default) = column
        .default
        .as_deref()
        .and_then(|raw| default_literal(kind, raw))
    {
        definition.push_str(&format!(" DEFAULT {default}"));
    }
    let comment = column.comment.trim();
    let inline_comment = matches!(kind, DatabaseKind::Mysql | DatabaseKind::ClickHouse);
    if !comment.is_empty() && inline_comment {
        definition.push_str(&format!(" COMMENT '{}'", comment.replace('\'', "''")));
    }
    lines.push(format!("ALTER TABLE {table_sql} ADD COLUMN {definition};"));
    if !comment.is_empty() && kind == DatabaseKind::Postgres {
        lines.push(format!(
            "COMMENT ON COLUMN {table_sql}.{quoted} IS '{}';",
            comment.replace('\'', "''")
        ));
    }
}

/// 一条改类型的语句（两侧类型名不一致）。
///
/// SQLite 改列类型要重建表（`ALTER TABLE` 只认加列 / 改名 / 删列），agent 轨背后
/// 几十个引擎写法各不相同 —— 这两档如实出注释，不猜语法。
fn push_modify_type(
    lines: &mut Vec<String>,
    kind: DatabaseKind,
    table_sql: &str,
    column: &str,
    type_name: &str,
    quote: char,
) {
    let type_name = type_name.trim();
    if type_name.is_empty() {
        return;
    }
    match kind {
        DatabaseKind::Mysql | DatabaseKind::ClickHouse => {
            let Ok(quoted) = quote_identifier(column, quote) else {
                return;
            };
            lines.push(format!(
                "ALTER TABLE {table_sql} MODIFY COLUMN {quoted} {type_name};"
            ));
        }
        DatabaseKind::Postgres => {
            let Ok(quoted) = quote_identifier(column, quote) else {
                return;
            };
            lines.push(format!(
                "ALTER TABLE {table_sql} ALTER COLUMN {quoted} TYPE {type_name};"
            ));
        }
        DatabaseKind::SqlServer => {
            let Ok(quoted) = quote_identifier(column, quote) else {
                return;
            };
            lines.push(format!(
                "ALTER TABLE {table_sql} ALTER COLUMN {quoted} {type_name};"
            ));
        }
        DatabaseKind::Sqlite => lines.push(format!(
            "-- SQLite 不能直接改「{column}」的类型，需要重建表；此改动未生成语句。"
        )),
        DatabaseKind::Mongo | DatabaseKind::Elasticsearch | DatabaseKind::Agent => {
            lines.push(format!(
                "-- 这类引擎改列类型的写法不统一，请手工处理「{column}」（目标类型 {type_name}）。"
            ));
        }
    }
}

/// 默认值原文 → DDL 里的 `DEFAULT …` 值。
///
/// 两个口径必须分开：MySQL 的 `information_schema.column_default` 报的是**裸值**
/// （`0` / `abc` / `CURRENT_TIMESTAMP`），字符串要补引号；其余引擎报的已经是
/// **表达式原文**（PG 的 `'x'::text`、SQL Server 的 `((0))`、SQLite 的 `'x'`），
/// 再加一层引号就变成一句字符串字面量了。
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
