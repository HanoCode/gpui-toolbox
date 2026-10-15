//! 项目管理页：左栏项目树 + 主区看板 / 归档中心 + 右栏详情。
//!
//! 按 AGENTS §4.1 的三件套拆：`state.rs` 持状态（**不实现 `Render`**，因此可单测），
//! `view.rs` 是纯函数渲染，本文件只做装配（建实体、挂左右栏、接动作、绑键位）。
//! 6 个领域组件在 `widgets/` 下，**页面私有**（拍板 Q4，不进 `duck-components`）。
//!
//! 数据源是内核 `duck-project`（自管 SQLite、零 gpui、全同步），所以这里**没有**
//! 后台线程：一次看板读是几次本地 SELECT，放进 `background_executor` 只会多一次
//! 跨线程往返。写入同理 —— 每条动作写完立刻重读，界面永远反映库里的真实状态。
//!
//! # 已落地增量
//!
//! - **S2**：看板（按状态分列）+ 卡片 + 左栏项目树 + 右栏详情 + 快速新建任务 +
//!   右键归档（任务 / 项目）+ 拖拽改状态。
//! - **S3-I**：归档中心（两段清单 + 搜索 + 恢复 / 彻底删除 / 清空）+
//!   页面级快捷键 + 归档撤销条 + UI 偏好落 `kv: projects_ui`（补上 §6.2 那条
//!   「S2 未落」的欠账）。
//! - **S3-II**：列表视图（`ViewMode::List`）+ 视图切换 tab + 快捷筛选
//!   chips（今天 / 本周 / 逾期 / 未排期 / 已完成）+ 主区搜索 + 排序切换器 +
//!   `1` / `2` 切视图。
//! - **S3-III**：按 `docs/projects-mock.html` 对齐呈现与操作路径（结构差异 13 项全收口）。
//! - **S4**：「随手看」悬浮窗（[`float`]，§4.3 ⑤）+ 两处视图之间的变更广播
//!   （[`bus`]）+ 全局键 `⌥⌘T`（两槽位热键，见 `crate::hotkey`）。悬浮窗是**独立窗口、
//!   独立连接**，所以两处视图靠广播互相同步 —— 本页既订阅它也广播它。
//! - **S5-I（本增量）**：完整任务编辑器（`dialogs::open_task_editor` —— 新建 / 编辑
//!   共用双列表单，日期走 gpui-component 的 `DatePicker`，不再手打 ISO 文本）。
//! - **未做（见设计文档 §7）**：同属 S5 的日历 / 时间线 / 报表 / 工作日志；
//!   AI 拆解与周报属 S6（编辑器里那三个 AI 按钮随它一起落地）。
//!
//! # 三条本页特有的口径
//!
//! 1. **归档不做二次确认，彻底删除 / 删除 / 清空才做**（§5.6 的可见性原则）。
//!    归档可逆、不丢数据，拦一道弹窗只会让「收掉一件事」变得啰嗦；所以它改用
//!    **带「撤销」的提示条**兜底。反过来，`⌫` 删除与归档中心的彻底删除都是
//!    **不可逆**的，一律先弹确认。
//! 2. **动作回执走页面级底部提示条，不走 `duck_components::notify`**：撤销条要挂
//!    「撤销」按钮，而 `notify` 没暴露 `Notification::action`。提示条本身照抄
//!    `pages/db.rs` 的 `Toast` 骨架（绝对定位 + 序号守卫的定时器），
//!    见 [`ProjectsPage::show`]。
//! 3. **落库之后要广播一声**（[`bus::notify`]）。写动作**绝大多数**收口在
//!    [`ProjectsPage::show`]（凡弹回执的都走它），所以广播就发在那一处；剩下几条
//!    **不弹回执**的写路径得各自再叫一声 —— 拖拽改列（`move_task_to`）、
//!    `Space` 标完成（`complete_task` / `toggle_selected`）、列内新建落在 Todo 列
//!    那一支（`submit_new_task`）。漏掉的症状很隐蔽：主窗改了、悬浮窗还显示旧的。
//!    兜底在 `app::open_task_float` —— 每次唤起都广播一次，所以最坏情况也会在
//!    「唤起来看一眼」时对齐。

mod archive;
// `bus` / `float` 对 `app` 可见：跨窗广播（[`bus::notify`]）与「随手看」悬浮窗的
// 唤起入口都挂在 shell 层（`app::open_task_float` / `app.rs` 的订阅），够不着私有模块。
pub(crate) mod bus;
mod dialogs;
mod export;
pub(crate) mod float;
mod prefs;
mod quickparse;
mod state;
mod view;
mod widgets;

use std::ops::RangeInclusive;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement, Render, SharedString, Styled, Subscription, WeakEntity, Window,
    actions, div, px,
};
use gpui_component::{
    Icon, WindowExt as _, h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};

use duck_components::notify::Tone;
use duck_components::{
    ButtonVariant, ConfirmHandler, SidebarPanel, SidebarState, body_text, button, dialog_footer,
    dialog_top_offset, modal_drag, modal_drag_area, modal_offset, modal_title_row,
};
use duck_project::{
    ColumnSpec, LogKind, NewMilestone, Priority, Quadrant, TaskPatch, TaskSort, TaskStatus,
};
use duck_theme::{active_theme, tr, trf, trn};

use prefs::{GroupMode, ViewMode};
use state::{ArchiveRow, BoardScope, ProjectsState, SmartFilter, UndoAction};
use widgets::status_label;

/// 左栏宽度档位（与数据库 / Redis 页取同一档，切页面时侧栏不跳宽）。
const SIDEBAR_WIDTH_RANGE: RangeInclusive<f32> = 184.0..=340.0;
/// 左栏默认宽度。
const SIDEBAR_DEFAULT_WIDTH: f32 = 236.;

/// 二次确认弹窗的宽度（与 Redis 页的确认框同档，全仓一致）。
const CONFIRM_WIDTH: f32 = 440.;

/// 底部提示条的阅读宽度上限（约 30 个中文字符 + 一个按钮，再加长就压到看板了）。
const HINT_MAX_WIDTH: f32 = 480.;

/// 普通回执的存活时长（照 `pages/db.rs` 的 toast：够看清一句话，连点也不堆叠）。
const HINT_LIFETIME: Duration = Duration::from_millis(2400);

/// 可撤销归档的提示条存活时长 = §5.6 规定的 **7 秒撤销窗口**。
///
/// 与 [`HINT_LIFETIME`] 分成两条常量而不是一条：回执「看一眼就走」，撤销窗口
/// 「留给你反应」—— 后者短了等于没给撤销的机会。
const UNDO_WINDOW: Duration = Duration::from_secs(7);

/// 页面的键盘上下文。
///
/// 必须比 `"Root"` 更深：gpui 的按键派发先查绑定、更深的上下文优先命中，
/// 页面级键位另开一层才不会与 shell / 编辑器抢（与 `pages/db.rs` 同一条纪律）。
const KEY_CONTEXT: &str = "Projects";

/// **裸字母键**（`N` / `J` / `K` / `A` / `Space` / `⌫` / `Esc`）的上下文。
///
/// `!Input` 是必需的，不是保险：gpui 的 `input_preference`（`window.rs`）在
/// **所有平台**都把 `prefer_character_input` 视为 false，按键一律先走 `KeyBindings`
/// —— 于是「无修饰的字母键」会直接抢掉输入框的字符输入。加上 `!Input` 之后，
/// 焦点在输入框里时这一条根本不匹配（[`KeyBindingContextPredicate::Not`] 会对整个
/// 上下文栈求值），字符照常落进输入框。
///
/// ⚠️ 与 [`KEY_CONTEXT`] **必须同改**：谓词靠名字匹配，改错不会编译报错，只会
/// 让裸键在输入框里抢字。由 `bare_key_context_tracks_the_page_context` 钉住。
const KEY_CONTEXT_BARE: &str = "Projects && !Input";

actions!(
    projects,
    [
        /// `N`：快速新建（Spotlight）——把一句话解析成任务。
        QuickCreate,
        /// `⌘N`：新建任务（打开任务编辑大表单）。
        NewTask,
        /// `⇧N`：新建项目。
        NewProject,
        /// `J`：在当前视图的视觉顺序里向下移动选中。
        MoveSelectionDown,
        /// `K`：向上移动选中。
        MoveSelectionUp,
        /// `Space`：切换选中任务的完成态。
        ToggleSelected,
        /// `A`：归档选中任务（可逆，不弹确认，改用撤销条）。
        ArchiveSelected,
        /// `⇧A`：跳到归档中心。
        OpenArchive,
        /// `⌫`：删除选中任务（看板）/ 彻底删除选中行（归档中心）——都不可逆，二次确认。
        DeleteSelected,
        /// `Esc`：从归档中心回到常规视图。
        LeaveArchive,
        /// `⌘Z`：撤销最近一次归档。
        UndoLastArchive,
        /// `1`：切到看板。
        ViewBoard,
        /// `2`：切到列表。
        ViewList,
        /// `3`：切到日历。
        ViewCalendar,
        /// `4`：切到时间线。
        ViewTimeline,
        /// `5`：切到报表。
        ViewReport,
        /// `6`：切到工作日志。
        ViewLog,
    ]
);

/// 绑定项目管理页的键位（由 `app::install` 调一次）。
///
/// 单独成函数（而不是内联在 `install` 里）是为了让测试能只做「绑一次 + 读回键位表」，
/// 不必跑整套 `install`（那会建托盘、开主窗口、读用户配置）。
pub(crate) fn bind_keys(cx: &mut App) {
    let modifier = duck_command::primary_modifier();
    cx.bind_keys([
        KeyBinding::new("n", QuickCreate, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("shift-n", NewProject, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("j", MoveSelectionDown, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("k", MoveSelectionUp, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("space", ToggleSelected, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("a", ArchiveSelected, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("shift-a", OpenArchive, Some(KEY_CONTEXT_BARE)),
        // `⌫` 在 macOS 上就是 backspace；`gpui_base::Root` 只绑了 `tab` / `⇧tab` /
        // `⌘C` / `Ctrl-C`（已逐条核对），所以这一条不会与别处抢。
        KeyBinding::new("backspace", DeleteSelected, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("escape", LeaveArchive, Some(KEY_CONTEXT_BARE)),
        // `1`–`6` 切常规视图（原型状态栏那条提示的排法）。裸数字键同样要 `!Input`。
        KeyBinding::new("1", ViewBoard, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("2", ViewList, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("3", ViewCalendar, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("4", ViewTimeline, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("5", ViewReport, Some(KEY_CONTEXT_BARE)),
        KeyBinding::new("6", ViewLog, Some(KEY_CONTEXT_BARE)),
        // `⌘N` 走 [`KEY_CONTEXT`] 而**不带** `!Input`：它不与文本编辑抢（`⌘N` 在输入
        // 上下文里没有含义），而「正打着字想直接新建一条」是常态。`⌘Z` 同理，见下。
        KeyBinding::new(&format!("{modifier}-n"), NewTask, Some(KEY_CONTEXT)),
        // `⌘Z` 在文本输入上下文里是上游的「撤销文字」，所以这一条也**不带** `!Input`：
        // 焦点在输入框里时更深的 `Input` 上下文先认领，退的是文本（与 db.rs 的
        // `⌘Z` 是同一套分层，无歧义——同一时刻只有一个上下文认领）。
        KeyBinding::new(&format!("{modifier}-z"), UndoLastArchive, Some(KEY_CONTEXT)),
    ]);
}

/// 归档中心与看板共用的「需要二次确认的不可逆动作」。
///
/// 收成一个枚举而不是三个各自的弹窗：确认框的骨架（几何、按钮档位、`on_ok` 分发）
/// 完全相同，只有标题 / 正文 / 确认按钮文案有一处不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmScope {
    /// 物理删除一条**未归档**任务（看板 `⌫`）。
    DeleteTask(i64),
    /// 物理删除列表里**当前勾选的那一批**（批量条的「删除」）。
    ///
    /// 故意不带 `Vec<i64>`：带了这一档就不再是 `Copy`，而 [`ProjectsPage::confirm`]
    /// 要把 `scope` move 进 `open_dialog` 的内容闭包（那是 `Fn`、每次重排都重跑），
    /// 一改就得给沿路每个匹配点补一遍 `clone`。反正确认框是**模态**的 —— 弹着的时候
    /// 勾选动不了，执行时现读 [`ProjectsState::checked_ids`] 与弹框那一刻完全一致。
    DeleteChecked,
    /// 删除一个**项目**（项目设置弹窗的「删除项目」）——连带它的任务，不可逆。
    DeleteProject(i64),
    /// 彻底删除一条归档记录（归档中心的行内按钮 / 右栏按钮 / `⌫`）。
    Purge(ArchiveRow),
    /// 清空整个归档区。
    ClearArchive,
}

/// 「在某一列新建任务」弹窗的状态。
///
/// 挂成 [`Entity`] 而不是页面字段：`open_dialog` 的内容闭包是 `Fn`（重排会重跑），
/// 拿不到 `&mut ProjectsPage`，但可以克隆 `Entity` 自由读写（与 `redis/mod.rs` 的
/// `EditForm` 同一条理由）。
struct NewTaskForm {
    /// 新任务落在哪一列。
    status: TaskStatus,
    /// 标题输入框。
    title: Entity<InputState>,
}

/// 底部提示条（动作回执，或带「撤销」的归档反馈）。
struct Hint {
    /// 正文。
    message: SharedString,
    /// 语义级别：决定图标与图标色。
    tone: Tone,
    /// 可撤销时是状态里那条槽的序号 —— 超时用它清状态、点击走 `state.undo()`。
    undo: Option<u64>,
    /// 本条提示的序号（定时器守卫，见 [`ProjectsPage::show`]）。
    seq: u64,
}

/// 项目管理页。
pub struct ProjectsPage {
    /// 全部页面状态（项目树 / 看板 / 选中项 / 归档中心 / 统计）。
    state: Entity<ProjectsState>,
    /// 左栏折叠态，由 shell 顶栏的侧栏开关共享。
    sidebar: Entity<SidebarState>,
    /// 左栏顶部的搜索框。
    ///
    /// 一份输入管两处（原型同款，占位文案写的就是「筛选任务与项目」）：左栏的项目 /
    /// 标签名按它筛（页面自己读，只重绘），主区任务走内核的 `search`。
    /// 与 `archive_search` 仍然是**两个**框：归档区可能和它同屏，共用一个词会打架。
    filter: Entity<InputState>,
    /// 归档中心的搜索框。
    ///
    /// 与 `filter` 分开是**必须**的：两个框可能同屏（归档中心 + 左栏），共用一个词
    /// 会互相打架（一个筛项目、一个筛档案）。
    archive_search: Entity<InputState>,
    /// 底部提示条；`None` = 不显示。
    hint: Option<Hint>,
    /// 提示条的序号源。
    hint_seq: u64,
    /// 自己的弱引用：动作处理器要多处使用，`routes` 会把强引用 move 走。
    view: WeakEntity<Self>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectsPage {
    /// 建页：打开内核、读第一屏数据。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 偏好（`kv: projects_ui`）由状态层读一次：视图 / 排序在这里就位，
        // 之后每次改动由本页落盘（见 [`Self::save_prefs`]）。
        let state = cx.new(|cx| ProjectsState::new(cx));
        let sidebar = cx.new(|cx| {
            SidebarState::new(
                px(SIDEBAR_DEFAULT_WIDTH),
                px(*SIDEBAR_WIDTH_RANGE.start())..px(*SIDEBAR_WIDTH_RANGE.end()),
                cx,
            )
        });
        let filter = text_input(window, cx, tr(cx, "筛选任务与项目…"));
        let archive_search = text_input(window, cx, tr(cx, "搜索归档…"));

        let mut subscriptions = Vec::new();
        // 左栏这个框一份输入管两处（原型同款）：树上项目 / 标签名按它筛（本页直接读，
        // 只重绘），主区任务走内核的 `search`（口径在 `ProjectsState::set_search`）。
        subscriptions.push(cx.subscribe(
            &filter,
            |this: &mut Self,
             _src: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    let term = this.filter.read(cx).value().to_owned();
                    this.state.update(cx, |state, _| state.set_search(&term));
                    cx.notify();
                }
            },
        ));
        // 归档搜索框：边打边筛（口径在 `ProjectsState::set_archive_search`）。
        subscriptions.push(cx.subscribe(
            &archive_search,
            |this: &mut Self,
             _src: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    let term = this.archive_search.read(cx).value().to_owned();
                    this.state
                        .update(cx, |state, _| state.set_archive_search(&term));
                    cx.notify();
                }
            },
        ));
        // 页面自己订阅状态实体：`ProjectsPage` 读 `state` 渲染，不订阅的话
        // 状态变了页面不会重绘。
        subscriptions.push(cx.observe(&state, |_this, _state, cx| cx.notify()));
        // 「随手看」悬浮窗（§4.3 ⑤）改了库要能传过来 —— 它开的是**另一个**连接，
        // 两边不会自动互知。订阅的是「变了」这条广播而不是某个具体动作：谁改的都一样，
        // 重读一遍就是最新状态（口径与 `float.rs` 的订阅完全对称）。
        // 先取实体再订阅：`cx.subscribe(&bus::handle(cx), …)` 会在同一次调用里
        // 两次可变借 `*cx`（`handle` 要 `&mut App`），得拆成两句。
        let bus = bus::handle(cx);
        subscriptions.push(cx.subscribe(
            &bus,
            |this: &mut Self, _bus, _event: &bus::ProjectChanged, cx| {
                this.state.update(cx, |state, _| state.reload());
                cx.notify();
            },
        ));

        Self {
            state,
            sidebar,
            filter,
            archive_search,
            hint: None,
            hint_seq: 0,
            view: cx.entity().downgrade(),
            _subscriptions: subscriptions,
        }
    }

    /// 供 shell 挂左栏折叠态。
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
    }

    /// 右栏内容（shell 级 `with_right_sidebar` 调它，签名只能是 `&App`）。
    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        view::render_right_sidebar(self.state.read(cx), &self.view, cx)
    }

    // ------------------------------------------------------------------
    // 动作：薄封装。真正的口径在 `ProjectsState`，这里只负责转调 + 回执 + `notify`。
    // ------------------------------------------------------------------

    /// 切换左栏范围。
    pub(crate) fn set_scope(&mut self, scope: BoardScope, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.set_scope(scope));
        cx.notify();
    }

    /// 切主区视图（左栏「归档中心」入口与 `⇧A` 都走这里）。
    ///
    /// 视图记在偏好里（§6.2），所以改完顺手落盘：下次进页保持上次看的那一块。
    pub(crate) fn set_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.set_view(view));
        self.save_prefs(cx);
        cx.notify();
    }

    /// 换排序口径（工具条的下拉）。排序也是偏好的一部分，改完落盘。
    pub(crate) fn set_sort(&mut self, sort: TaskSort, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.set_sort(sort));
        self.save_prefs(cx);
        cx.notify();
    }

    /// 换列表分组轴（工具条的下拉）。与排序同属偏好，改完落盘。
    pub(crate) fn set_group(&mut self, group: GroupMode, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.set_group(group));
        self.save_prefs(cx);
        cx.notify();
    }

    /// 点左栏「智能列表」的一档：已选中就取消，否则换成它。
    ///
    /// 「再点一次取消」是这一列行的通用直觉（项目行也是这么回去的），少了它就得再给
    /// 一个「清除筛选」按钮 —— 多一个控件只为了回到起点。
    ///
    /// 状态层会顺带把范围收回「全部任务」：两者互斥（见 `ProjectsState::set_scope`）。
    pub(crate) fn toggle_smart(&mut self, filter: SmartFilter, cx: &mut Context<Self>) {
        let current = self.state.read(cx).smart();
        let next = if current == Some(filter) {
            None
        } else {
            Some(filter)
        };
        self.state.update(cx, |state, _| state.set_smart(next));
        cx.notify();
    }

    /// 点左栏一行标签：已选中就取消，否则只看它。
    pub(crate) fn toggle_tag(&mut self, id: i64, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.toggle_tag(id));
        cx.notify();
    }

    /// 工具条「显示已完成」开关。
    ///
    /// **不落盘**：与智能列表 / 搜索词同一类「一次性视角」，而不是长期偏好 ——
    /// 偏好表里只留视图模式与排序两项（§6.2）。
    pub(crate) fn toggle_show_done(&mut self, cx: &mut Context<Self>) {
        let next = !self.state.read(cx).show_done();
        self.state.update(cx, |state, _| state.set_show_done(next));
        cx.notify();
    }

    /// 折叠 / 展开左栏的「全部项目」（纯界面态，不动任何筛选口径）。
    pub(crate) fn toggle_projects_collapsed(&mut self, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, _| state.toggle_projects_collapsed());
        cx.notify();
    }

    /// 选中 / 取消选中一张卡片。
    pub(crate) fn select_task(&mut self, id: Option<i64>, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.select(id));
        cx.notify();
    }

    /// 选中 / 取消选中归档中心的一行。
    pub(crate) fn select_archive(&mut self, row: ArchiveRow, cx: &mut Context<Self>) {
        let current = self.state.read(cx).archive_selected();
        let next = if current == Some(row) {
            None
        } else {
            Some(row)
        };
        self.state.update(cx, |state, _| state.select_archive(next));
        cx.notify();
    }

    /// 拖拽落点：把任务挪到目标列。
    pub(crate) fn move_task_to(&mut self, id: i64, status: TaskStatus, cx: &mut Context<Self>) {
        // 落在列首（`sort_order` 0）—— 跨列时列内次序由内核重排，这里不猜位置。
        self.state
            .update(cx, |state, _| state.move_task(id, status, 0));
        // 这几条写动作**不弹回执**，所以拿不到 [`Self::show`] 里那次广播，得自己叫一声
        // （口径见模块头第 3 条）。
        bus::notify(cx);
        cx.notify();
    }

    /// 标记完成。
    pub(crate) fn complete_task(&mut self, id: i64, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, _| state.move_task(id, TaskStatus::Done, 0));
        bus::notify(cx);
        cx.notify();
    }

    /// 卡片菜单：「开始 / 暂停 / 重新打开 / 标记完成 / 标记为阻塞」与「状态…」子菜单
    /// 共走这一条。
    ///
    /// 不直连 [`Self::complete_task`] 那条：那些动作要拿到内核的 `Err` 才能说清
    /// 「为什么没改成」（比如从「已取消」出发的非法迁移），而 `move_task` 只把错误
    /// 记在状态里。成功也只回一条「已移动到「X」」—— 与原型 `状态…` 的回执一致。
    pub(crate) fn set_task_status(&mut self, id: i64, status: TaskStatus, cx: &mut Context<Self>) {
        let moved = trf(cx, "已移动到「{}」。", &[status_label(status, cx)]);
        let before = self.undo_seq(cx);
        let outcome = self
            .state
            .update(cx, |state, _| state.set_task_status(id, status));
        match outcome {
            Ok(()) => self.show(Tone::Success, moved, self.fresh_undo_seq(before, cx), cx),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "修改失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    /// 卡片菜单要的现成数据：这条任务 + 全部项目 + 看板列。
    ///
    /// 菜单是**右键那一刻**才构建的，所以在这里现读 —— 若让每张卡片在渲染时就
    /// 捕获一份，一屏几十张卡每帧都要克隆一遍全项目表。
    pub(crate) fn menu_data(
        &self,
        id: i64,
        cx: &App,
    ) -> Option<(
        duck_project::Task,
        Vec<duck_project::Project>,
        Vec<duck_project::BoardColumn>,
    )> {
        let state = self.state.read(cx);
        let task = state.tasks().iter().find(|task| task.id == id)?.clone();
        Some((task, state.projects().to_vec(), state.columns().to_vec()))
    }

    /// 卡片菜单 / 右栏：改优先级。
    pub(crate) fn set_priority(&mut self, id: i64, priority: Priority, cx: &mut Context<Self>) {
        let message = trf(cx, "优先级已设为 {}", &[&format!("{priority:?}")]);
        self.patch_task(id, TaskPatch::new().priority(priority), message, cx);
    }

    /// 卡片菜单 / 右栏：改四象限。
    pub(crate) fn set_quadrant(&mut self, id: i64, quadrant: Quadrant, cx: &mut Context<Self>) {
        let message = trf(cx, "四象限已设为 {}", &[&format!("{quadrant:?}")]);
        self.patch_task(id, TaskPatch::new().quadrant(quadrant), message, cx);
    }

    /// 右栏「进度」：**只可前进**地推进到某一档。
    ///
    /// 不回退、也不给「不能后退」的回执 —— 原型那句「只可前进」挂在分区标题上，用户
    /// 已经看得见规则；为一次点击再念一遍只会刷屏。回退走「完整编辑器」（那里能直接
    /// 改数值），所以这条只拦界面上的快捷档。
    pub(crate) fn advance_progress(&mut self, id: i64, progress: u8, cx: &mut Context<Self>) {
        let current = self
            .state
            .read(cx)
            .tasks()
            .iter()
            .find(|task| task.id == id)
            .map(|task| task.progress);
        let Some(current) = current else {
            return;
        };
        if progress <= current {
            return;
        }
        let message = trf(cx, "进度已推进到 {}%。", &[&progress.to_string()]);
        self.patch_task(id, TaskPatch::new().progress(progress), message, cx);
    }

    /// 卡片菜单：置顶 / 取消置顶（星标）。
    ///
    /// 走 [`patch_task`](Self::patch_task) 而不是状态层的同名方法：两者完全同形
    /// （改一个布尔字段 + 成功回执 + 失败提示），再维护一条平行路径只会让「失败时要不要
    /// 也弹回执」这种细节在两处各飘一次。
    pub(crate) fn set_starred(&mut self, id: i64, starred: bool, cx: &mut Context<Self>) {
        let message = if starred {
            tr(cx, "已置顶。")
        } else {
            tr(cx, "已取消置顶。")
        };
        self.patch_task(
            id,
            TaskPatch::new().starred(starred),
            message.to_owned(),
            cx,
        );
    }

    /// 卡片菜单：复制任务（新建一条字段相同的，标题加「（副本）」）。
    ///
    /// 回执带「撤销」（原型 `已复制为「X」` 同款）：刚复制出来的那条除了这次复制之外
    /// 没有别的来路，撤销就是把它删掉。
    pub(crate) fn duplicate_task(&mut self, id: i64, cx: &mut Context<Self>) {
        let before = self.undo_seq(cx);
        let outcome = self.state.update(cx, |state, _| state.duplicate_task(id));
        match outcome {
            Ok(_) => self.show(
                Tone::Success,
                tr(cx, "已复制任务。"),
                self.fresh_undo_seq(before, cx),
                cx,
            ),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "复制失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 里程碑（右栏「里程碑」分区，原型 openMilestoneEditor）
    // ------------------------------------------------------------------

    /// 给任务加一个里程碑。
    ///
    /// 快照由弹窗给（默认「当前进度 + 20」封顶 100，与原型一致）。内核在创建时若发现
    /// 快照高于任务当前进度，会把任务进度一并推到快照上 —— 所以这里不用另外改任务本身。
    pub(crate) fn submit_milestone(
        &mut self,
        task_id: i64,
        title: &str,
        snapshot: u8,
        cx: &mut Context<Self>,
    ) {
        let outcome = self.state.update(cx, |state, _| {
            state.add_milestone(NewMilestone::for_task(task_id, title).progress_snapshot(snapshot))
        });
        match outcome {
            Ok(()) => self.show(Tone::Success, tr(cx, "里程碑已添加。"), None, cx),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "添加失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    /// 右栏「里程碑」：达成 / 取消达成。
    pub(crate) fn toggle_milestone(&mut self, id: i64, reached: bool, cx: &mut Context<Self>) {
        let message = if reached {
            tr(cx, "已标记达成。")
        } else {
            tr(cx, "已取消达成。")
        };
        let outcome = self
            .state
            .update(cx, |state, _| state.toggle_milestone_reached(id, reached));
        match outcome {
            Ok(()) => self.show(Tone::Success, message.to_owned(), None, cx),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "修改失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    /// 右栏「里程碑」：删除一个。里程碑是**节点标记**（不参与进度计算），删掉不牵连任务，
    /// 所以不走二次确认 —— 与「归档」同一档可逆性判断（§5.6）。
    pub(crate) fn remove_milestone(&mut self, id: i64, cx: &mut Context<Self>) {
        let outcome = self.state.update(cx, |state, _| state.remove_milestone(id));
        match outcome {
            Ok(()) => self.show(Tone::Success, tr(cx, "里程碑已删除。"), None, cx),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "删除失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 列表批量操作（原型 `.bulkbar`）
    // ------------------------------------------------------------------

    /// 列表行最左边那个勾。
    pub(crate) fn toggle_checked(&mut self, id: i64, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.toggle_checked(id));
        cx.notify();
    }

    /// 表头的全选勾。
    pub(crate) fn toggle_check_all(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.toggle_check_all());
        cx.notify();
    }

    /// 批量条上的「取消选择」。
    pub(crate) fn clear_checked(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.clear_checked());
        cx.notify();
    }

    /// 批量改字段的公共路径：读勾选 → 写库 → 清勾选 → 回执。
    ///
    /// 失败时**不清勾选**：用户还得在那几条上重试，清掉等于逼他再勾一遍。
    fn bulk_patch_then(
        &mut self,
        patch: TaskPatch,
        message: impl Fn(&App, usize) -> String,
        cx: &mut Context<Self>,
    ) {
        let ids = self.state.read(cx).checked_ids();
        if ids.is_empty() {
            return;
        }
        let outcome = self
            .state
            .update(cx, |state, _| state.bulk_patch(&ids, &patch));
        match outcome {
            Ok(count) => {
                self.state.update(cx, |state, _| state.clear_checked());
                let text = message(cx, count);
                self.show(Tone::Success, text, None, cx);
            }
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "批量修改失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    /// 批量条：改优先级。
    pub(crate) fn bulk_priority(&mut self, priority: Priority, cx: &mut Context<Self>) {
        self.bulk_patch_then(
            TaskPatch::new().priority(priority),
            |cx, count| trf(cx, "已更新 {} 条任务的优先级。", &[&count.to_string()]),
            cx,
        );
    }

    /// 批量条：移入项目；`None` = 移出到「未归入项目」。
    pub(crate) fn bulk_project(&mut self, project_id: Option<i64>, cx: &mut Context<Self>) {
        let patch = match project_id {
            Some(id) => TaskPatch::new().project(id),
            None => TaskPatch::new().clear_project(),
        };
        self.bulk_patch_then(
            patch,
            |cx, count| trf(cx, "已移动 {} 条任务。", &[&count.to_string()]),
            cx,
        );
    }

    /// 批量条：改状态（「标记完成」就是 `Done` 这一档的特例）。
    ///
    /// 单开一条而不是并进 [`Self::bulk_patch_then`]：状态迁移要走内核的
    /// `move_task`（那里才有 `completed_at` 与进度的联动），不是普通字段 patch。
    pub(crate) fn bulk_status(&mut self, status: TaskStatus, cx: &mut Context<Self>) {
        let ids = self.state.read(cx).checked_ids();
        if ids.is_empty() {
            return;
        }
        let outcome = self
            .state
            .update(cx, |state, _| state.bulk_set_status(&ids, status));
        match outcome {
            Ok(count) => {
                self.state.update(cx, |state, _| state.clear_checked());
                self.show(
                    Tone::Success,
                    trf(cx, "已更新 {} 条任务的状态。", &[&count.to_string()]),
                    None,
                    cx,
                );
            }
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "批量修改失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    /// 批量条：删除（**不可逆** → 与单条删除同一条纪律，先弹二次确认）。
    pub(crate) fn confirm_delete_checked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.read(cx).checked_count() == 0 {
            return;
        }
        self.confirm(ConfirmScope::DeleteChecked, window, cx);
    }

    /// 卡片菜单 / 右栏：移入项目；`None` = 移出到「未归入项目」。
    pub(crate) fn set_task_project(
        &mut self,
        id: i64,
        project_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        let name = project_id.map_or_else(
            || SharedString::from(tr(cx, "未归入项目")),
            |project_id| {
                self.state
                    .read(cx)
                    .projects()
                    .iter()
                    .find(|project| project.id == project_id)
                    .map_or_else(
                        || SharedString::from(tr(cx, "项目")),
                        |project| SharedString::from(project.name.clone()),
                    )
            },
        );
        let message = trf(cx, "已移入「{}」", &[name.as_str()]);
        let outcome = self
            .state
            .update(cx, |state, _| state.move_task_to_project(id, project_id));
        match outcome {
            Ok(()) => self.show(Tone::Success, message, None, cx),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "修改失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    /// 卡片菜单里「改一个字段」的公共路径：走状态层 patch、成功给回执、失败报错。
    fn patch_task(&mut self, id: i64, patch: TaskPatch, done: String, cx: &mut Context<Self>) {
        let outcome = self
            .state
            .update(cx, |state, _| state.patch_task(id, patch));
        match outcome {
            Ok(()) => self.show(Tone::Success, done, None, cx),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "修改失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    /// 归档任务。
    ///
    /// 归档**不弹二次确认**（§5.6：可逆且不丢数据），代价是必须给撤销通道 ——
    /// 所以这里归档成功后一定亮起带「撤销」的提示条。
    pub(crate) fn archive_task(&mut self, id: i64, cx: &mut Context<Self>) {
        let outcome = self.state.update(cx, |state, _| state.archive_task(id));
        match outcome {
            Ok(()) => {
                let slot = self.undo_seq(cx);
                self.show(
                    Tone::Success,
                    tr(cx, "已归档任务，可在归档中心找回。"),
                    slot,
                    cx,
                );
            }
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "归档失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
    }

    /// 归档项目（连带它的任务）。
    pub(crate) fn archive_project(&mut self, id: i64, cx: &mut Context<Self>) {
        let outcome = self.state.update(cx, |state, _| state.archive_project(id));
        match outcome {
            Ok(cascaded) => {
                let slot = self.undo_seq(cx);
                let message = if cascaded == 0 {
                    tr(cx, "已归档项目，可在归档中心找回。").to_owned()
                } else {
                    // 数量文案走 `trn`（英文单复数不同形，见 `archive.rs` 的同款说明）。
                    trn(cx, "已归档项目，连带 {} 个任务。", i64::from(cascaded))
                };
                self.show(Tone::Success, message, slot, cx);
            }
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "归档失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
    }

    /// 新建项目。
    ///
    /// 现在只给一条「点一下就有一个新项目」的路径（名字自动取且保证不重名）；
    /// 带名称 / 颜色 / 日期的完整弹窗与重命名**仍未做** —— 不做「点了没反应」
    /// 或者「只有演示文案」的控件（P3-6），所以宁可先不摆那个入口。
    pub(crate) fn create_project(&mut self, cx: &mut Context<Self>) {
        let base = tr(cx, "新项目").to_owned();
        let taken: Vec<String> = self
            .state
            .read(cx)
            .projects()
            .iter()
            .map(|project| project.name.clone())
            .collect();
        let name = unique_name(&base, &taken);
        let created = self
            .state
            .update(cx, |state, _| state.create_project(&name));
        match created {
            Some(_) => self.show(Tone::Success, trf(cx, "已新建「{}」。", &[&name]), None, cx),
            None => cx.notify(),
        }
    }

    // ------------------------------------------------------------------
    // 归档中心：恢复 / 彻底删除 / 清空
    // ------------------------------------------------------------------

    /// 恢复一条归档记录（行内按钮与右栏按钮共用）。
    ///
    /// 恢复**不做二次确认**：它只是把东西放回去，可逆且不丢数据。
    pub(crate) fn restore(&mut self, row: ArchiveRow, window: &mut Window, cx: &mut Context<Self>) {
        let _ = window;
        let outcome = self.state.update(cx, |state, _| match row {
            ArchiveRow::Task(id) => state.restore_task(id),
            ArchiveRow::Project(id) => state.restore_project(id),
        });
        match outcome {
            Ok(_) => self.show(Tone::Success, tr(cx, "已恢复。"), None, cx),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "恢复失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
    }

    /// 彻底删除一条归档记录 —— **不可逆**，先弹二次确认。
    pub(crate) fn confirm_purge(
        &mut self,
        row: ArchiveRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm(ConfirmScope::Purge(row), window, cx);
    }

    /// 清空归档 —— **不可逆**，先弹二次确认。
    pub(crate) fn confirm_clear_archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm(ConfirmScope::ClearArchive, window, cx);
    }

    /// 卡片菜单的「删除任务」—— **不可逆**，与看板上的 `⌫` 走**同一个**确认框。
    ///
    /// 单独开一个口是为了让卡片菜单不必知道 [`ConfirmScope`] 的存在；口径只有一处
    /// （`ConfirmScope::DeleteTask` 的标题 / 正文 / 按钮文案全在 [`Self::prompt`] 里），
    /// 两条入口不可能说不一样的话。
    pub(crate) fn confirm_delete_task(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm(ConfirmScope::DeleteTask(id), window, cx);
    }

    /// 弹二次确认框（三处不可逆动作共用）。
    ///
    /// 照 `pages/redis/keys.rs` 的 `confirm_or_run`：恒居中的量尺在 prepaint 阶段量，
    /// 所以 `.opacity(offset.opacity)` 与 `.close_button(false)` 两条**都不能省**
    /// （前者防「先冒头再跳居中」，后者防两个 ✕）。
    fn confirm(&mut self, scope: ConfirmScope, window: &mut Window, cx: &mut Context<Self>) {
        let (title, message, ok_text) = self.prompt(scope, cx);
        let drag = modal_drag(cx);
        let view = self.view.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let offset = modal_offset(&drag, cx);
            // 外层闭包是 `Fn`（每次重排都重跑）→ 每一轮都得重新克隆一份交给内层。
            let drag_for_content = drag.clone();
            let title_for_content = title.clone();
            let message_for_content = message.clone();
            let view_for_ok = view.clone();

            // 这一窗的每一档都是不可逆删除，确认键统一用警示红 —— 普通蓝会让人
            // 把「永久删除」当成一次普通保存。
            let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
                // 确认后才真正动手；视图已销毁（`update` 返回 `Err`）也照样关弹窗，
                // 不让一个点不掉的确认框挂在屏幕上。
                let _ = view_for_ok.update(cx, |this, cx| this.run_confirm(scope, cx));
                true
            });
            let footer = dialog_footer(
                "projects-confirm",
                ok_text.clone(),
                ButtonVariant::Destructive,
                confirm,
                cx,
            );

            dialog
                .w(px(CONFIRM_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "projects-confirm-title",
                                &drag_for_content,
                                title_for_content.clone(),
                                cx,
                            ))
                            .child(body_text(message_for_content.clone(), cx)),
                    ))
                })
                .footer(footer)
        });
    }

    /// 确认框的标题 / 正文 / 确认按钮文案。
    fn prompt(&self, scope: ConfirmScope, cx: &App) -> (SharedString, SharedString, SharedString) {
        let state = self.state.read(cx);
        match scope {
            ConfirmScope::DeleteTask(id) => {
                let title = task_title(state, id);
                (
                    SharedString::from(tr(cx, "删除任务？")),
                    SharedString::from(trf(cx, "「{}」将被永久删除，无法恢复。", &[&title])),
                    SharedString::from(tr(cx, "删除")),
                )
            }
            ConfirmScope::DeleteChecked => {
                let count = state.checked_count();
                (
                    SharedString::from(tr(cx, "删除选中的任务？")),
                    SharedString::from(trf(
                        cx,
                        "选中的 {} 条任务将被永久删除，无法恢复。",
                        &[&count.to_string()],
                    )),
                    SharedString::from(tr(cx, "删除")),
                )
            }
            ConfirmScope::DeleteProject(id) => {
                let name = state
                    .projects()
                    .iter()
                    .find(|project| project.id == id)
                    .map_or_else(String::new, |project| project.name.clone());
                (
                    SharedString::from(tr(cx, "删除项目？")),
                    SharedString::from(trf(
                        cx,
                        "「{}」及该项目下的任务将被永久删除，无法恢复。",
                        &[&name],
                    )),
                    SharedString::from(tr(cx, "删除项目")),
                )
            }
            ConfirmScope::Purge(ArchiveRow::Task(id)) => {
                let title = state
                    .archive_tasks()
                    .iter()
                    .find(|entry| entry.task.id == id)
                    .map_or_else(|| task_title(state, id), |entry| entry.task.title.clone());
                (
                    SharedString::from(tr(cx, "彻底删除任务？")),
                    SharedString::from(trf(cx, "「{}」将被永久删除，无法恢复。", &[&title])),
                    SharedString::from(tr(cx, "彻底删除")),
                )
            }
            ConfirmScope::Purge(ArchiveRow::Project(id)) => {
                let entry = state
                    .archive_projects()
                    .iter()
                    .find(|entry| entry.project.id == id);
                let name = entry.map_or_else(String::new, |entry| entry.project.name.clone());
                // 连带任务数写在正文里会变成第二个占位符，而带数量的文案只支持一个
                // 占位符（`trn` 的契约）—— 那个数字在归档中心那一行已经显示过了，这里
                // 只说「连同它的任务」，免得为了一句文案把复数表撬开。
                (
                    SharedString::from(tr(cx, "彻底删除项目？")),
                    SharedString::from(trf(
                        cx,
                        "「{}」及该项目下的任务将被永久删除，无法恢复。",
                        &[&name],
                    )),
                    SharedString::from(tr(cx, "彻底删除")),
                )
            }
            ConfirmScope::ClearArchive => {
                let total = state.archive_counts().total();
                (
                    SharedString::from(tr(cx, "清空归档？")),
                    SharedString::from(trn(
                        cx,
                        "归档区里的 {} 项将被永久删除，无法恢复。",
                        i64::from(total),
                    )),
                    SharedString::from(tr(cx, "清空归档")),
                )
            }
        }
    }

    /// 确认之后真正执行。
    fn run_confirm(&mut self, scope: ConfirmScope, cx: &mut Context<Self>) {
        match scope {
            ConfirmScope::DeleteTask(id) => {
                let outcome = self.state.update(cx, |state, _| state.delete_task(id));
                match outcome {
                    Ok(()) => self.show(Tone::Success, tr(cx, "已删除任务。"), None, cx),
                    Err(err) => self.show(
                        Tone::Error,
                        trf(cx, "删除失败：{}", &[&err.to_string()]),
                        None,
                        cx,
                    ),
                }
            }
            ConfirmScope::DeleteChecked => {
                let ids = self.state.read(cx).checked_ids();
                let outcome = self.state.update(cx, |state, _| state.bulk_delete(&ids));
                match outcome {
                    Ok(deleted) => {
                        self.state.update(cx, |state, _| state.clear_checked());
                        self.show(
                            Tone::Success,
                            trf(cx, "已删除 {} 条任务。", &[&deleted.to_string()]),
                            None,
                            cx,
                        );
                    }
                    Err(err) => self.show(
                        Tone::Error,
                        trf(cx, "删除失败：{}", &[&err.to_string()]),
                        None,
                        cx,
                    ),
                }
            }
            ConfirmScope::DeleteProject(id) => {
                let outcome = self.state.update(cx, |state, _| state.delete_project(id));
                match outcome {
                    Ok(()) => self.show(Tone::Success, tr(cx, "已删除项目。"), None, cx),
                    Err(err) => self.show(
                        Tone::Error,
                        trf(cx, "删除失败：{}", &[&err.to_string()]),
                        None,
                        cx,
                    ),
                }
            }
            ConfirmScope::Purge(row) => {
                let outcome = self.state.update(cx, |state, _| match row {
                    ArchiveRow::Task(id) => state.purge_task(id),
                    ArchiveRow::Project(id) => state.purge_project(id),
                });
                match outcome {
                    Ok(()) => self.show(Tone::Success, tr(cx, "已彻底删除。"), None, cx),
                    Err(err) => self.show(
                        Tone::Error,
                        trf(cx, "彻底删除失败：{}", &[&err.to_string()]),
                        None,
                        cx,
                    ),
                }
            }
            ConfirmScope::ClearArchive => {
                let outcome = self.state.update(cx, |state, _| {
                    state.clear_archive(duck_project::ClearScope::All)
                });
                match outcome {
                    Ok(cleared) => self.show(
                        Tone::Success,
                        trn(cx, "已清空归档，共 {} 项。", i64::from(cleared)),
                        None,
                        cx,
                    ),
                    Err(err) => self.show(
                        Tone::Error,
                        trf(cx, "清空归档失败：{}", &[&err.to_string()]),
                        None,
                        cx,
                    ),
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // 快捷键
    // ------------------------------------------------------------------

    /// `J` / `K`。
    fn move_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.select_next(delta));
        cx.notify();
    }

    /// `Space`：切换选中任务的完成态。
    ///
    /// 归档中心里没有「完成态」这一说（那是一条已归档的记录），所以不做事 ——
    /// 比按一下弹个「不支持」体面。看板与列表共用这一条（两个视图选中的都是任务）。
    fn toggle_selected(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        if state.view().is_archive() {
            return;
        }
        let Some(id) = state.selected() else {
            return;
        };
        self.state.update(cx, |state, _| state.toggle_done(id));
        // 不弹回执 ⇒ 自己广播（同上）。
        bus::notify(cx);
        cx.notify();
    }

    /// `A`：归档选中任务。
    fn archive_selected(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        if state.view().is_archive() {
            return;
        }
        let Some(id) = state.selected() else {
            return;
        };
        self.archive_task(id, cx);
    }

    /// `⇧A`：跳到归档中心。
    fn open_archive(&mut self, cx: &mut Context<Self>) {
        self.set_view(ViewMode::Archive, cx);
    }

    /// `Esc`：从归档中心回到看板。
    ///
    /// 设计文档的快捷键表里没有这一条，但 `⇧A` 是单向门 —— 没有它，键盘用户只能靠
    /// 鼠标点左栏才能回去。补一条「原路返回」，不改任何既有语义。
    fn leave_archive(&mut self, cx: &mut Context<Self>) {
        if self.state.read(cx).view().is_archive() {
            self.set_view(ViewMode::Board, cx);
        }
    }

    /// `⌫`：看板 / 列表上删任务、归档中心里彻底删除 —— 两者都不可逆，都要二次确认。
    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scope = {
            let state = self.state.read(cx);
            match state.view() {
                ViewMode::Board | ViewMode::List => state.selected().map(ConfirmScope::DeleteTask),
                ViewMode::Archive => state.archive_selected().map(ConfirmScope::Purge),
                // 其余视图没有「选中的可删对象」。
                ViewMode::Calendar | ViewMode::Timeline | ViewMode::Report | ViewMode::Log => None,
            }
        };
        if let Some(scope) = scope {
            self.confirm(scope, window, cx);
        }
    }

    /// `⌘Z`（以及提示条上的「撤销」）：把上一步可撤销的变更退回去。
    ///
    /// 撤销槽记的是**动作**（归档 / 改状态 / 复制），不是「上一次归档」——所以这里按动作
    /// 分派回执文案，而不是一律说「已撤销归档」。
    fn undo_last(&mut self, cx: &mut Context<Self>) {
        let outcome = self.state.update(cx, |state, _| state.undo());
        match outcome {
            Ok(Some(action)) => {
                let message: SharedString = match action {
                    UndoAction::RestoreTask(_) => {
                        SharedString::from(tr(cx, "已撤销归档，任务已恢复。"))
                    }
                    UndoAction::RestoreProject(_) => {
                        SharedString::from(tr(cx, "已撤销归档，项目已恢复。"))
                    }
                    UndoAction::RevertStatus { status, .. } => SharedString::from(trf(
                        cx,
                        "已把状态改回「{}」。",
                        &[status_label(status, cx)],
                    )),
                    UndoAction::RemoveCreated(_) => {
                        SharedString::from(tr(cx, "已删掉刚复制出来的那条。"))
                    }
                };
                self.show(Tone::Success, message, None, cx);
            }
            Ok(None) => self.show(Tone::Warning, tr(cx, "没有可撤销的操作。"), None, cx),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "撤销失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
    }

    /// 看板列底的「+ 添加任务」（原型 `.addcard`，`projects-mock.html:1208`）。
    ///
    /// 原型是**列内联输入框**（`inlineAdd`，`projects-mock.html:2938`）；这里换成一个小
    /// 弹窗：内联输入要往列里插一行临时元素（会改动布局、还要和拖拽落点抢命中区），
    /// 而弹窗只有一处输入、一处焦点，标题里还写明建到哪一列。
    pub(crate) fn add_task_to_column(
        &mut self,
        status: TaskStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = cx.new(|cx| NewTaskForm {
            status,
            title: cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "任务标题"))),
        });
        let heading = SharedString::from(trf(cx, "在「{}」新建任务", &[status_label(status, cx)]));
        let drag = modal_drag(cx);
        let view = self.view.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let offset = modal_offset(&drag, cx);
            // 外层闭包是 `Fn`（每次重排都重跑）→ 每一轮都得重新克隆一份交给内层。
            let drag_for_content = drag.clone();
            let form_for_content = form.clone();
            let form_for_ok = form.clone();
            let view_for_ok = view.clone();
            let heading_for_content = heading.clone();

            let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
                // 只有「弹窗那一刻建的输入框」这一处来源，没有别的可回读对象。
                let (title, status) = {
                    let form = form_for_ok.read(cx);
                    (form.title.read(cx).value().to_owned(), form.status)
                };
                let _ = view_for_ok.update(cx, |page, cx| page.submit_new_task(&title, status, cx));
                true
            });
            let footer = dialog_footer(
                "projects-new-task",
                tr(cx, "新建"),
                ButtonVariant::Primary,
                confirm,
                cx,
            );

            dialog
                .w(px(CONFIRM_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .gap(active_theme(cx).tokens().spacing.sm)
                            .child(modal_title_row(
                                "projects-new-task-title",
                                &drag_for_content,
                                heading_for_content.clone(),
                                cx,
                            ))
                            .child(input_box(&form_for_content.read(cx).title, cx)),
                    ))
                })
                .footer(footer)
        });
    }

    /// 弹窗确认：把标题建成任务，再挪到目标列。
    ///
    /// 分两步而不是一步到位：`create_task` 建出来的是 `todo`，落到别的列要再过一次
    /// 内核的状态机（`set_task_status`）—— 那里是唯一校验迁移合法性的地方，绕过去
    /// 就等于在界面层另写一份状态机。
    fn submit_new_task(&mut self, title: &str, status: TaskStatus, cx: &mut Context<Self>) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        // 与快速新建同一口径：「全部」不是容器，落收集箱。
        let project = match self.state.read(cx).scope() {
            BoardScope::Project(id) => Some(id),
            BoardScope::All | BoardScope::Inbox => None,
        };
        let Some(id) = self
            .state
            .update(cx, |state, _| state.create_task(title, project))
        else {
            return;
        };
        // 建成功了就先广播：下面「落 Todo 列」这一支不弹回执，走不到 `show` 里那次广播。
        bus::notify(cx);
        if status != TaskStatus::Todo {
            let added = trf(cx, "已添加到「{}」。", &[status_label(status, cx)]);
            let outcome = self
                .state
                .update(cx, |state, _| state.set_task_status(id, status));
            match outcome {
                Ok(()) => self.show(Tone::Success, added, None, cx),
                Err(err) => self.show(
                    Tone::Error,
                    trf(cx, "修改失败：{}", &[&err.to_string()]),
                    None,
                    cx,
                ),
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 弹窗接线（`dialogs.rs` 只画，动作与落库全在页面层）
    // ------------------------------------------------------------------
    //
    // 每个 `open_*` 都是一层薄壳：弹窗的几何 / 表单状态在 `dialogs.rs`，而「弹出来」
    // 这件事会用到页面的私有字段（`state` / `view`），所以由页面转一手。

    /// 打开任务编辑弹窗（`task_id` 为 `None` = 新建）。
    pub(crate) fn open_task_editor(
        &mut self,
        task_id: Option<i64>,
        preset_project: Option<i64>,
        preset_status: TaskStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        dialogs::open_task_editor(self, task_id, preset_project, preset_status, window, cx);
    }

    /// 「新建任务」按钮 / `⌘N`：在当前范围里新建。
    pub(crate) fn new_task_in_scope(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (project, status) = {
            let state = self.state.read(cx);
            let project = match state.scope() {
                BoardScope::Project(id) => Some(id),
                BoardScope::All | BoardScope::Inbox => None,
            };
            // 落在看板的**第一列**（不是硬编码 `Todo`）：列是可自定义的，第一列才是
            // 用户心里的「入口列」。
            let status = state
                .columns()
                .first()
                .map_or(TaskStatus::Todo, |column| column.status_key);
            (project, status)
        };
        self.open_task_editor(None, project, status, window, cx);
    }

    /// 任务编辑弹窗的「保存」。
    ///
    /// 状态**不**走 `TaskPatch`（[`dialogs::CollectedTask::into_patch`] 刻意不带
    /// status）：状态迁移有内核状态机把关（比如「已取消」只许回 `todo`），绕过去就
    /// 等于在界面层另写一份。所以先提交其余字段，再按需过一次 `set_task_status`。
    pub(crate) fn submit_task_editor(
        &mut self,
        task_id: Option<i64>,
        collected: dialogs::CollectedTask,
        cx: &mut Context<Self>,
    ) {
        let status = collected.status;
        match task_id {
            Some(id) => {
                let patch = collected.into_patch();
                let unchanged = self
                    .state
                    .read(cx)
                    .tasks()
                    .iter()
                    .find(|task| task.id == id)
                    .is_some_and(|task| task.status == status);
                let outcome = self.state.update(cx, |state, _| {
                    state.patch_task(id, patch)?;
                    // 状态没变就不碰：`move_task` 对「原地不动」不是空操作，会写
                    // `completed_at` / `progress` 的联动（等于把用户的进度改掉）。
                    if !unchanged {
                        state.set_task_status(id, status)?;
                    }
                    Ok(())
                });
                self.report(outcome, tr(cx, "已保存任务。").to_owned(), cx);
            }
            None => {
                let created = self.state.update(cx, |state, _| {
                    state.create_task_full(collected.into_new_task())
                });
                match created {
                    Some(_) => self.show(Tone::Success, tr(cx, "已新建任务。"), None, cx),
                    None => cx.notify(),
                }
            }
        }
    }

    /// 打开项目编辑弹窗（`project_id` 为 `None` = 新建）。
    pub(crate) fn open_project_editor(
        &mut self,
        project_id: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        dialogs::open_project_editor(self, project_id, window, cx);
    }

    /// 项目编辑弹窗的「保存」。
    pub(crate) fn submit_project_editor(
        &mut self,
        project_id: Option<i64>,
        collected: dialogs::CollectedProject,
        cx: &mut Context<Self>,
    ) {
        match project_id {
            Some(id) => {
                let outcome = self.state.update(cx, |state, _| {
                    state.update_project(id, collected.into_patch())
                });
                self.report(outcome, tr(cx, "已保存项目。").to_owned(), cx);
            }
            None => {
                let created = self.state.update(cx, |state, _| {
                    state.create_project_full(collected.into_new_project())
                });
                match created {
                    Some(_) => self.show(Tone::Success, tr(cx, "已新建项目。"), None, cx),
                    None => cx.notify(),
                }
            }
        }
    }

    /// 项目设置弹窗里的「删除项目」——**不可逆**，先弹二次确认。
    pub(crate) fn confirm_delete_project(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm(ConfirmScope::DeleteProject(id), window, cx);
    }

    /// 打开标签管理弹窗。
    pub(crate) fn open_tag_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        dialogs::open_tag_manager(self, window, cx);
    }

    /// 新建标签（标签管理弹窗的「添加」）。颜色按现有标签数在色板上轮转 ——
    /// 让新建的标签一眼能和已有的区分开，同时**不引入**随机数（同一次操作可重现）。
    pub(crate) fn create_tag(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let color = {
            let state = self.state.read(cx);
            if state.tags().iter().any(|tag| tag.name == name) {
                // 同名的已经在了：不重复建（内核会拒，但错误文案对用户没意义），
                // 静默重绘就够 —— 清单里那个标签本来就在。
                cx.notify();
                return;
            }
            dialogs::PROJECT_COLORS[state.tags().len() % dialogs::PROJECT_COLORS.len()]
        };
        let outcome = self
            .state
            .update(cx, |state, _| state.create_tag(name, color));
        match outcome {
            Ok(_) => self.show(
                Tone::Success,
                trf(cx, "已新建标签「{}」。", &[name]),
                None,
                cx,
            ),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "新建失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
    }

    /// 换一个标签颜色（色板上往后挪一格）。
    pub(crate) fn cycle_tag_color(&mut self, id: i64, cx: &mut Context<Self>) {
        let next = {
            let state = self.state.read(cx);
            let Some(tag) = state.tags().iter().find(|tag| tag.id == id) else {
                return;
            };
            let current = dialogs::PROJECT_COLORS
                .iter()
                .position(|color| color.eq_ignore_ascii_case(&tag.color))
                // 认不出的自定义色：从色板头开始（用户按一下就落到一个已知颜色上）。
                .unwrap_or(dialogs::PROJECT_COLORS.len() - 1);
            dialogs::PROJECT_COLORS[(current + 1) % dialogs::PROJECT_COLORS.len()]
        };
        let outcome = self
            .state
            .update(cx, |state, _| state.update_tag(id, None, Some(next)));
        self.report(outcome, tr(cx, "已换标签颜色。").to_owned(), cx);
    }

    /// 删除标签（**不可逆**，但只解除关联、不删任务 —— 所以不弹二次确认，
    /// 弹窗里那行「删除」本身就是那一次确认）。
    pub(crate) fn delete_tag(&mut self, id: i64, cx: &mut Context<Self>) {
        let outcome = self.state.update(cx, |state, _| state.delete_tag(id));
        self.report(outcome, tr(cx, "已删除标签。").to_owned(), cx);
    }

    /// 打开看板列设置弹窗。
    pub(crate) fn open_column_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        dialogs::open_column_settings(self, window, cx);
    }

    /// 看板列设置的「保存」：整份列定义替换。
    pub(crate) fn submit_columns(&mut self, specs: Vec<ColumnSpec>, cx: &mut Context<Self>) {
        let outcome = self
            .state
            .update(cx, |state, _| state.replace_columns(&specs));
        self.report(outcome, tr(cx, "已保存看板列。").to_owned(), cx);
    }

    /// 打开快速新建（Spotlight）。
    pub(crate) fn open_spotlight(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_spotlight_in(None, window, cx);
    }

    /// 打开快速新建，并**预填**一个项目。
    ///
    /// 项目右键菜单的「新建任务并归入」用 `Some(id)`；工具条与快捷键走 [`open_spotlight`]
    /// （即 `None`），落点回落到当前范围。两条路只是预填值的来源不同，解析与落库全在
    /// `submit_spotlight` 一份，所以这里只做到「把 fallback 定下来」为止。
    pub(crate) fn open_spotlight_in(
        &mut self,
        project: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = project.or(match self.state.read(cx).scope() {
            BoardScope::Project(id) => Some(id),
            BoardScope::All | BoardScope::Inbox => None,
        });
        dialogs::open_spotlight(self, project, window, cx);
    }

    /// Spotlight 的「创建」：把一句话解析成任务。
    pub(crate) fn submit_spotlight(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        // 落点优先级：`@项目` > 当前范围。与原型一致 —— 在某个项目下快速新建，
        // 不写 `@项目` 就该落在这个项目里；写了就以写的为准。
        let fallback = match self.state.read(cx).scope() {
            BoardScope::Project(id) => Some(id),
            BoardScope::All | BoardScope::Inbox => None,
        };
        let parsed = quickparse::parse(text, self.state.read(cx).today());
        let created = self.state.update(cx, |state, _| {
            state.create_task_from_quick(&parsed, fallback)
        });
        match created {
            Some(_) => self.show(Tone::Success, tr(cx, "已新建任务。"), None, cx),
            None => cx.notify(),
        }
    }

    /// 打开「导出任务」弹窗（工具条的导出按钮）。
    pub(crate) fn open_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        dialogs::open_export(self, window, cx);
    }

    /// 导出的「复制到剪贴板」：把生成的文本交出去，并弹一条回执。
    ///
    /// 回执与剪贴板写在同一个动作里，不拆开 —— 拆开迟早会出现「复制了但没提示」
    /// 或者「提示了但没复制」这两种都让人怀疑的情况。
    pub(crate) fn copy_export(&mut self, text: String, cx: &mut Context<Self>) {
        let chars = text.chars().count();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.show(
            Tone::Success,
            trf(cx, "已复制到剪贴板（{} 字符）。", &[&chars.to_string()]),
            None,
            cx,
        );
    }

    /// 打开「从文本导入任务」弹窗（工具条的导入按钮）。
    pub(crate) fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        dialogs::open_import(self, window, cx);
    }

    /// 导入弹窗的「导入」：把粘贴的文本按行解析并落库。
    ///
    /// 落点优先级与快速新建一致：`@项目` > 当前范围（见 [`Self::submit_spotlight`]）。
    pub(crate) fn submit_import(&mut self, text: &str, cx: &mut Context<Self>) {
        let today = self.state.read(cx).today();
        let lines = quickparse::parse_lines(text, today);
        if lines.is_empty() {
            self.show(Tone::Warning, tr(cx, "没有可导入的行。"), None, cx);
            return;
        }
        let fallback = match self.state.read(cx).scope() {
            BoardScope::Project(id) => Some(id),
            BoardScope::All | BoardScope::Inbox => None,
        };
        let outcome = self
            .state
            .update(cx, |state, _| state.import_lines(&lines, fallback));
        match outcome {
            Ok(created) => {
                // 导入是**批量**写入，自己广播一声（与快速新建同理：它不回执时不该静默）。
                bus::notify(cx);
                self.show(
                    Tone::Success,
                    trf(cx, "已导入 {} 个任务。", &[&created.to_string()]),
                    None,
                    cx,
                );
            }
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "导入失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
    }

    /// 打开某天（某档）的工作日志编辑弹窗。
    pub(crate) fn open_log_editor(
        &mut self,
        date: duck_project::CalendarDate,
        kind: LogKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        dialogs::open_log_editor(self, date, kind, window, cx);
    }

    /// 工作日志弹窗的「保存」。
    pub(crate) fn submit_log(
        &mut self,
        date: duck_project::CalendarDate,
        kind: LogKind,
        content: &str,
        cx: &mut Context<Self>,
    ) {
        let outcome = self
            .state
            .update(cx, |state, _| state.save_log(date, kind, content));
        self.report(outcome, tr(cx, "已保存日志。").to_owned(), cx);
    }

    /// 换工作日志的档位（日报 / 周报 / 周计划）。
    pub(crate) fn set_log_kind(&mut self, kind: LogKind, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.set_log_kind(kind));
        cx.notify();
    }

    /// 选中工作日志清单里的一篇。
    pub(crate) fn select_log(&mut self, id: i64, cx: &mut Context<Self>) {
        self.state.update(cx, |state, _| state.select_log(id));
        cx.notify();
    }

    /// 换日历视图的锚月（`delta` = 前后翻几个月；`None` = 回到今天所在月）。
    pub(crate) fn step_calendar_month(&mut self, delta: Option<i32>, cx: &mut Context<Self>) {
        let next = match delta {
            None => None,
            Some(delta) => {
                let state = self.state.read(cx);
                let anchor = state.calendar_month().unwrap_or_else(|| state.today());
                // 月份加减走整数运算再夹回合法日：`CalendarDate::new` 对越界的
                // 「1 月 31 日 + 1 月」是 `None`，先取当月 1 号就不会踩到。
                let total = i64::from(anchor.year()) * 12 + i64::from(anchor.month()) - 1
                    + i64::from(delta);
                let year = i32::try_from(total.div_euclid(12)).unwrap_or(anchor.year());
                let month = u32::try_from(total.rem_euclid(12) + 1).unwrap_or(1);
                duck_project::CalendarDate::new(year, month, 1)
            }
        };
        self.state
            .update(cx, |state, _| state.set_calendar_month(next));
        cx.notify();
    }

    /// 打开某天日志的编辑弹窗（当前档位）。
    pub(crate) fn open_today_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (date, kind) = {
            let state = self.state.read(cx);
            let date = state
                .selected_log()
                .map_or_else(|| state.today(), |log| log.date);
            (date, state.log_kind())
        };
        self.open_log_editor(date, kind, window, cx);
    }

    /// 「改一个字段」类动作的公共收尾：成功给回执、失败报错，最后重绘。
    ///
    /// 与私有的 [`Self::patch_task`] 并存：那个只收一条 `TaskPatch`，这里用在**多步**
    /// 写入（比如「先 patch 再迁状态」）上 —— 两步只在同一个 `state.update` 里合成一次
    /// 重绘，中途失败也不会留下半截状态。
    fn report(&mut self, outcome: duck_project::Result<()>, done: String, cx: &mut Context<Self>) {
        match outcome {
            Ok(()) => self.show(Tone::Success, done, None, cx),
            Err(err) => self.show(
                Tone::Error,
                trf(cx, "修改失败：{}", &[&err.to_string()]),
                None,
                cx,
            ),
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 偏好与提示条
    // ------------------------------------------------------------------

    /// 把当前视图 / 排序 / 分组写进 `kv: projects_ui`（§6.2 的单键 JSON）。
    ///
    /// 整份偏好是一个 JSON 对象，**所有字段永远一起写**：只写一半会把另一半写成
    /// 默认值 —— 比如「切个视图」不该把用户刚选的排序悄悄复位。
    fn save_prefs(&self, cx: &mut Context<Self>) {
        let (view, sort, group) = {
            let state = self.state.read(cx);
            (state.view(), state.sort(), state.group())
        };
        let prefs = prefs::ProjectsPrefs::default()
            .with_view(view)
            .with_sort(sort)
            .with_group(group);
        prefs::ProjectsPrefs::save(cx, &prefs);
    }

    /// 状态里那条撤销槽的序号（没有就是 `None`）。
    fn undo_seq(&self, cx: &Context<Self>) -> Option<u64> {
        self.state.read(cx).undo_slot().map(|slot| slot.seq)
    }

    /// 「这次动作有没有压出一条**新的**撤销槽」；有才把序号交给提示条。
    ///
    /// 直接用 [`Self::undo_seq`] 是不够的：改回同一档状态是空操作、不压槽，此时它返回的
    /// 还是上一条动作的槽 —— 把那个序号挂到这次的提示条上，按「撤销」撤掉的是上一步
    /// 而不是这一步。
    fn fresh_undo_seq(&self, before: Option<u64>, cx: &Context<Self>) -> Option<u64> {
        self.undo_seq(cx).filter(|seq| Some(*seq) != before)
    }

    /// 弹一条底部提示条。
    ///
    /// `undo` 有值时右侧出现「撤销」按钮，存活时间延长到 [`UNDO_WINDOW`]；否则约
    /// [`HINT_LIFETIME`] 后自消。序号守卫照 `pages/db.rs` 的 `toast_seq`：定时器到点时
    /// 只有「还是自己那一条」才清 —— 少了它，7 秒内连做两次归档时第一条的定时器会把
    /// 第二条一起抹掉（第二条等于没给撤销的机会）。
    ///
    /// 提示条超时也顺手作废状态里的撤销槽（[`ProjectsState::expire_undo`]，同样带序号
    /// 守卫）：§5.6 的撤销窗口就是 7 秒，过了窗口 `⌘Z` 不该还能翻出一条十秒前的归档。
    pub(crate) fn show(
        &mut self,
        tone: Tone,
        message: impl Into<SharedString>,
        undo: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.hint_seq += 1;
        let seq = self.hint_seq;
        let lifetime = if undo.is_some() {
            UNDO_WINDOW
        } else {
            HINT_LIFETIME
        };
        self.hint = Some(Hint {
            message: message.into(),
            tone,
            undo,
            seq,
        });
        // 提示条的时机就是「写入路径的收口」——本页所有成功 / 失败回执都走这里，
        // 所以在这广播一声，就覆盖了全部写动作，不必在几十个动作函数里各插一次。
        // （广播在**落库之后**：写动作都是「先改库、再 show」，见各 `apply_*`。）
        bus::notify(cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(lifetime).await;
            let _ = this.update(cx, |this, cx| {
                if this.hint.as_ref().is_none_or(|hint| hint.seq != seq) {
                    return;
                }
                this.hint = None;
                if let Some(slot) = undo {
                    this.state.update(cx, |state, _| state.expire_undo(slot));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 底部提示条浮层；没有可弹的就返回 `None`。
    ///
    /// 锚在页面底部居中：外层是整幅宽的居中容器、本身不接鼠标事件（没有 hitbox），
    /// 所以它不会挡住看板底部的点击 —— 挡住交互的浮层比没有浮层更烦人。
    fn render_hint(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let hint = self.hint.as_ref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let body = tokens.typography.sm;
        let view = self.view.clone();
        Some(
            div()
                .absolute()
                .left(px(0.))
                .right(px(0.))
                .bottom(tokens.spacing.lg)
                .flex()
                .justify_center()
                .child(
                    h_flex()
                        .id("projects-hint")
                        .max_w(px(HINT_MAX_WIDTH))
                        .items_center()
                        .gap(tokens.spacing.sm)
                        .px(tokens.spacing.md)
                        .py(tokens.spacing.xs)
                        .rounded(tokens.radius.lg)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(extended.colors.chrome)
                        // 浮层要压过看板，给一点投影把「浮起来」画出来。
                        .shadow_md()
                        .child(
                            Icon::new(hint.tone.icon_name())
                                .size(extended.icon.md)
                                .text_color(hint.tone.color(cx)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(body.size)
                                .line_height(body.line_height)
                                .text_color(tokens.colors.foreground)
                                .child(hint.message.clone()),
                        )
                        .children(hint.undo.map(|_| {
                            button(
                                "projects-hint-undo",
                                tr(cx, "撤销"),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .on_click({
                                let view = view.clone();
                                move |_, _window, cx| {
                                    let _ = view.update(cx, |page, cx| page.undo_last(cx));
                                }
                            })
                        })),
                )
                .into_any_element(),
        )
    }
}

impl Render for ProjectsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = view::render_main(self.state.read(cx), &self.view, &self.archive_search, cx);

        let mut panel = SidebarPanel::new("projects-content", &self.sidebar, content, window, cx);
        if panel.is_sidebar_visible() {
            panel = panel.sidebar(view::render_sidebar(
                self.state.read(cx),
                &self.view,
                &self.filter,
                cx,
            ));
        }

        // 键位落在页面根上：焦点在输入框里时上下文栈更深的是 `Input`，
        // 裸字母键由 [`KEY_CONTEXT_BARE`] 的 `!Input` 谓词让开（见那里的说明）。
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &QuickCreate, window, cx| {
                this.open_spotlight(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTask, window, cx| {
                this.new_task_in_scope(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewProject, _window, cx| {
                this.create_project(cx);
            }))
            .on_action(cx.listener(|this, _: &MoveSelectionDown, _window, cx| {
                this.move_selection(1, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveSelectionUp, _window, cx| {
                this.move_selection(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSelected, _window, cx| {
                this.toggle_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &ArchiveSelected, _window, cx| {
                this.archive_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &OpenArchive, _window, cx| {
                this.open_archive(cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteSelected, window, cx| {
                this.delete_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &LeaveArchive, _window, cx| {
                this.leave_archive(cx);
            }))
            .on_action(cx.listener(|this, _: &UndoLastArchive, _window, cx| {
                this.undo_last(cx);
            }))
            .on_action(cx.listener(|this, _: &ViewBoard, _window, cx| {
                this.set_view(ViewMode::Board, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewList, _window, cx| {
                this.set_view(ViewMode::List, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewCalendar, _window, cx| {
                this.set_view(ViewMode::Calendar, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewTimeline, _window, cx| {
                this.set_view(ViewMode::Timeline, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewReport, _window, cx| {
                this.set_view(ViewMode::Report, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewLog, _window, cx| {
                this.set_view(ViewMode::Log, cx);
            }))
            .child(div().flex_1().min_h_0().min_w_0().child(panel))
            // 浮层放最后：绝对定位、贴在页面底沿。
            .children(self.render_hint(cx))
    }
}

/// 建一个输入框。
fn text_input(
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

/// 输入框壳（与左栏搜索框 / `redis/helpers.rs` 的 `panel_input_box` 同源，
/// 三处的圆角、描边、底色都取自同一组 token）。
fn input_box(input: &Entity<InputState>, cx: &App) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .w_full()
        .h(theme.density().control)
        .px(tokens.spacing.sm)
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .child(Input::new(input).appearance(false).bordered(false).w_full())
}

/// 看板上某条任务的标题；查不到（刚被删）给空串。
fn task_title(state: &ProjectsState, id: i64) -> String {
    state
        .tasks()
        .iter()
        .find(|task| task.id == id)
        .map_or_else(String::new, |task| task.title.clone())
}

/// 在 `taken` 里挑一个不重名的名字：`基名`、`基名 2`、`基名 3`……
///
/// 重名会让左栏出现两个一模一样的项目，用户没法分辨；内核没有唯一约束，
/// 唯一性由这一层保证。
fn unique_name(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|name| name == base) {
        return base.to_owned();
    }
    let mut index = 2_u32;
    loop {
        let candidate = format!("{base} {index}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{KEY_CONTEXT, KEY_CONTEXT_BARE, dialogs, unique_name};

    /// 里程碑快照的默认值：当前进度 + 20，封顶 100（原型 `Math.min(100, t.prog + 20)`）。
    ///
    /// 封顶那一条是真会踩到的：进度 90 时 +20 会溢出 `u8` 之外的语义（155%），
    /// 内核把它钳到 100 之前这里就得对。
    #[test]
    fn the_default_milestone_snapshot_advances_twenty_and_caps_at_hundred() {
        assert_eq!(dialogs::default_milestone_snapshot(0), 20);
        assert_eq!(dialogs::default_milestone_snapshot(35), 55);
        assert_eq!(dialogs::default_milestone_snapshot(80), 100);
        assert_eq!(dialogs::default_milestone_snapshot(90), 100, "封顶");
        assert_eq!(dialogs::default_milestone_snapshot(100), 100);
        assert_eq!(
            dialogs::default_milestone_snapshot(255),
            100,
            "极端值也不能溢出（`saturating_add`）"
        );
    }

    /// 裸键上下文必须仍然落在页面上下文里、并且仍然排除输入框。
    ///
    /// 谓词是**字符串**，改名字不会有编译错误 —— 只会让裸字母键悄悄开始抢输入框的
    /// 字符（或整页快捷键失灵）。这条把它拉回编译期附近。
    #[test]
    fn bare_key_context_tracks_the_page_context() {
        assert!(
            KEY_CONTEXT_BARE.starts_with(KEY_CONTEXT),
            "裸键上下文 {KEY_CONTEXT_BARE} 必须以页面上下文 {KEY_CONTEXT} 开头"
        );
        assert!(
            KEY_CONTEXT_BARE.contains("!Input"),
            "裸键上下文必须用 `!Input` 把输入框让开（见 KEY_CONTEXT_BARE 的说明）"
        );
    }

    #[test]
    fn unique_name_avoids_collisions() {
        let taken = vec!["新项目".to_owned(), "新项目 2".to_owned()];
        assert_eq!(unique_name("新项目", &taken), "新项目 3");
    }

    #[test]
    fn unique_name_keeps_the_first_one_when_free() {
        assert_eq!(unique_name("新项目", &[]), "新项目");
    }
}
