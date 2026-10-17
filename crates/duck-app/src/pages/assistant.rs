//! AI 助手页：多会话对话 + 流式输出 + Markdown 渲染。
//!
//! 布局与状态挂法沿用本仓 `RedisPage` / `DatabasePage`：
//! 左栏走页面内 [`SidebarPanel`]，右栏走 shell 级 `with_right_sidebar`。
//! 消息流用上游 `gpui-component` 的 `MessageScroller`（虚拟列表 + 跟随尾部 +
//! 回底按钮 + 底部渐隐），气泡用 `Bubble` / `BubbleContent`，正文用
//! `TextView::markdown`。**UI 层几乎零自研**，只在三处做了本项目特有的收敛：
//!
//! 1. **气泡底色不硬编码**：一律走 `BubbleVariant`，其取值来自 gpui-component 主题，
//!    而本仓 `duck-theme` 已把自有 token 投影过去（`manager::project_onto_kit`），
//!    因此自动跟随明暗 / 强调色 / 缩放。
//! 2. **流式通路是本页唯一的复杂点**（`docs/ai-design.md` §4.1）：本仓零 tokio，
//!    所以走「专用 `std::thread` + `mpsc` + 前台轮询」，且**必须节流**——
//!    本仓已有 dialog prepaint「每帧回调自激」的教训，这里每 40ms 合并一次 notify。
//! 3. **不落 `duck-store` 的 kv**：会话正文走 `duck-ai` 自管的 `ai.db`（见 `duck_ai::store`）。
//!
//! M2 起接工具：内核驱动多轮工具循环（[`duck_ai::run_agent`]），本页负责
//! 装配注册表（[`crate::ai_tools::build_registry`]）、给出审批裁决、把工具调用
//! 渲染成卡片。**只读工具自动放行**——审批回调在这里对
//! [`ToolRisk::ReadOnly`] 直接答 `Run`，需要确认的档位一律 `Deny` 并把原因回灌给模型
//! （本页没有「写操作」类工具，所以 `Deny` 分支目前是防未来用，不是空壳：
//! 它保证一旦有工具把风险升到 `NeedsApproval`，就不会被静默执行）。

use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use duck_ai::{
    AgentEvent, AgentOptions, AgentOutcome, AiEvent, AiStore, Approval, ChatMessage, ChatOptions,
    Conversation, ProviderProfile, Role, RunHandle, StopReason, ToolDetail, ToolRegistry, ToolRisk,
    Usage, store as ai_store,
};
use duck_components::{
    ButtonVariant, DuckIcon, SidebarPanel, SidebarState, button, card,
    content::{accent_tag, accordion, empty_state, neutral_tag, tag},
    detail_title_text, dropdown_select, icon_button, icon_text_button, meta_text, notify,
    notify::Tone, selectable_text,
    sidebar_navigation_button,
};
use duck_theme::{DuckThemeState, active_theme, number, tr, trf};
use gpui::{
    AnyElement, App, AppContext as _, Context, Div, Entity, Focusable as _,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, WeakEntity, Window, div, px,
};
use gpui_component::{
    Icon,
    bubble::{Bubble, BubbleVariant},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    message::MessageAlignment,
    message_scroller::{MessageScroller, MessageScrollerState},
    text::TextView,
    v_flex,
};

use crate::ai::{self, AiSettings};
use crate::ai_tools;

/// 左栏宽度档位（与 `db.rs` / `redis.rs` 保持一致，切页时侧栏不跳宽）。
const SIDEBAR_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 184.0..=340.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 236.;

/// 流式事件的轮询周期。
///
/// **这是硬要求**：本仓 `dialog.rs` 的 prepaint 监听器曾因「每帧无条件 refresh」
/// 自激成死循环。逐 token `notify` 是同一类问题——40ms 合并一次，
/// `TextView` 自带的 `stream_fade` 会补足观感。
const POLL_INTERVAL: Duration = Duration::from_millis(40);

/// 输入框高度档（行数）。
///
/// `LayoutMode::AutoGrow` 的空闲高度就是 `min_rows`（见 gpui-base `mode.rs:94`，
/// `rows` 初值即 `min_rows`，只有内容超行时 `update_auto_grow` 才往上抬）。
/// 所以「默认框太矮」改这里，别去加 `min_h`——`min_h` 会和 auto_grow 的
/// 实测行高打架，出现「空框很高、打两行反而不长」的鬼影。
const COMPOSER_MIN_ROWS: usize = 3;
const COMPOSER_MAX_ROWS: usize = 10;

/// 一次问答的生成参数落到内核前的默认网络超时（秒）。
const REQUEST_TIMEOUT_SECS: u64 = 300;

/// 工具轮次上限的**默认值**（设置页可覆盖，见 `AiSettings::max_tool_rounds`）。
///
/// 兜底而非功能：模型偶发会陷入「调工具 → 看结果 → 再调同一个」的循环，
/// 没有上限就是无限花钱。撞上限时会在消息流末尾明说「已达最大工具轮次」。
const MAX_TOOL_ROUNDS: usize = 8;

/// 工具结果预览用的等宽族名（与 `logs.rs` / `licenses.rs` 同款写法）。
///
/// 工具结果里「列对齐」本身就是信息（`list_dir` 的名称 / 类型 / 大小三列），
/// 用比例字体会把它揉成一坨。
#[cfg(target_os = "macos")]
const MONO_FAMILY: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
const MONO_FAMILY: &str = "Consolas";

/// 后台线程 → UI 的消息。
///
/// 不是直接用 `AgentEvent`：内核跑完还会交回**本次新增的整串消息**（助手消息与
/// 工具结果）与累计用量，那些只有 `run_agent` 的返回值有。合成一个枚举，
/// UI 侧一个循环就能收干净。
enum StreamMessage {
    /// 内核事件（流式增量、工具开始 / 结束 / 被拒、轮次耗尽）。
    Event(AgentEvent),
    /// 正常收尾。
    Done(Box<AgentOutcome>),
    /// 失败（含取消）。
    Failed(String),
}

/// 正在跑的一轮（可能包含多轮工具调用）。
struct Streaming {
    /// 取消句柄（与后台线程共享同一个标志位）。
    handle: RunHandle,
    /// 事件接收端。
    receiver: Receiver<StreamMessage>,
    /// 本轮用的档案 id（收尾时要写回设置与落库）。
    provider_id: String,
    /// 本轮用的模型名。
    model: String,
    /// UI 侧累积的正文（与内核并行累积：内核那份在异常路径上拿不到）。
    text: String,
    /// UI 侧累积的思考。
    thinking: String,
    /// 已收到的用量。
    usage: Usage,
    /// 结束原因。
    stop_reason: StopReason,
    /// 是否已经收到过 `Started`（决定要不要显示「生成中」）。
    started: bool,
    /// 本轮跑过的工具调用（按发生顺序）。**只覆盖还没定稿的那一轮**——
    /// `RoundCompleted` 一到就清空，因为那些卡片已经在 `live` 的消息里了。
    tool_runs: Vec<ToolRun>,
    /// 已经定稿的轮次（内核 `RoundCompleted` 原样转存）。
    ///
    /// **不直接写 `self.messages`**：本轮还在跑，`self.messages` 代表「已落库的历史」，
    /// 混在一起会让收尾时的落库多出一份重复。收尾时两者才合流。
    live: Vec<ChatMessage>,
    /// 撞上轮次上限时的上限值（用于收尾文案）。
    rounds_exhausted: Option<usize>,
}

/// 一次工具调用在界面上的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolState {
    /// 已发起、还没回。
    Running,
    /// 跑完了（不论成败，看 `ok`）。
    Finished,
    /// 用户拒绝执行。
    Denied,
}

/// 一张工具调用卡的数据。
#[derive(Clone)]
struct ToolRun {
    call_id: String,
    name: String,
    /// 入参摘要（`path` / `query`）。
    summary: String,
    state: ToolState,
    /// 成败。`Denied` 时为 `false`。
    ok: bool,
    /// 结果正文（已按预览档截断）。
    preview: String,
    /// 结构化产物（表格）。
    detail: Option<ToolDetail>,
}

/// 消息流里的一行（渲染快照）。
///
/// 快照是刻意的：`MessageScroller` 的行渲染闭包必须是 `'static`，
/// 不能再借 `&self`。每帧重建一次 `Vec<Row>` 并把 `Rc` 挪进闭包，成本可忽略。
struct Row {
    /// 稳定 `ElementId`（同一个会话里同一行永远同一个 id，选中态与滚动位置才不会串）。
    id: SharedString,
    kind: RowKind,
    /// 行内操作（没有就不画按钮）。
    action: Option<RowAction>,
}

/// 消息行上的行内操作。
///
/// **只在「最后一条同类消息」上给**（见 [`AssistantPage::message_actions`]）：
/// 「重新生成」要删掉它后面的一切，「编辑重发」同理——挂到中间某条上等于一个
/// 不显眼的分支操作，误点就会静默丢掉后面几轮对话，代价和收益不成比例。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    /// 重新生成：截断这条助手消息及其之后的消息，再按剩下的历史重发。
    Regenerate(usize),
    /// 编辑重发：把这条用户消息的正文填回输入框，发送时从它重新开始。
    Edit(usize),
    /// 重试：本轮失败后，把最后一条用户消息原样重发。
    Retry,
}

/// 哪些消息下标上要挂行内操作。
#[derive(Debug, Clone, Copy, Default)]
struct MessageActions {
    /// 最后一条助手消息（`messages` 里的下标）。
    last_assistant: Option<usize>,
    /// 最后一条用户消息（`messages` 里的下标）。
    last_user: Option<usize>,
}

enum RowKind {
    User {
        text: String,
    },
    Assistant {
        text: String,
        thinking: Option<String>,
    },
    /// 一次工具调用及其结果。
    Tool(Box<ToolCard>),
    Error {
        text: String,
    },
}

/// 渲染用的一张工具卡（`RowKind::Tool` 的载荷）。
///
/// 与 [`ToolRun`] 分开：`ToolRun` 是**正在跑**的那一轮的可变状态，
/// `ToolCard` 是从落库消息里还原出来的只读快照。两者字段重叠但生命周期完全不同。
struct ToolCard {
    name: String,
    summary: String,
    /// 卡片状态。**从实时状态直接带过来**：靠「有没有结果」反推的话，
    /// 「被拒」与「失败」会显示成同一个样子。
    state: ToolState,
    /// `None` = 没找到配对的结果（还在跑、被取消、或撞上轮次上限）。
    result: Option<ToolResultView>,
    /// 结构化产物（表格），从**实时状态**带过来；落库消息里没有这个字段
    /// （它按设计不进对话上下文，也不落库），所以历史回看时为空。
    detail: Option<ToolDetail>,
}

struct ToolResultView {
    ok: bool,
    content: String,
}

/// AI 助手页。
pub struct AssistantPage {
    /// 左栏折叠 / 展开状态（shell 顶栏的侧栏开关也读它）。
    sidebar: Entity<SidebarState>,
    /// 输入区（多行、Enter 发送、Shift+Enter 换行）。
    composer: Entity<TextareaState>,
    /// 左栏会话搜索框。
    search: Entity<InputState>,
    /// 消息流滚动状态。
    scroll: Entity<MessageScrollerState>,

    /// 自引用：左栏 / 右栏 / 输入区的回调要把动作送回来。
    view: WeakEntity<Self>,
    /// 订阅句柄（延长生命周期）。
    _subscriptions: Vec<Subscription>,

    /// 厂商档案（kv 读入；设置页写）。
    providers: Vec<ProviderProfile>,
    /// 生成参数（kv 读入；设置页写）。
    settings: AiSettings,
    /// 助手页内选中的档案 id（首次进入取设置里的默认厂商）。
    selected_provider: Option<String>,

    /// 会话库。打开失败时为 `None`——页面给一条明确提示，而不是静默不落库。
    store: Option<AiStore>,
    /// 会话列表（最近更新在前）。
    conversations: Vec<Conversation>,
    /// 当前会话 id。
    active: Option<String>,
    /// 当前会话的消息（含流式中的那条的**已完成**部分）。
    messages: Vec<ChatMessage>,
    /// 「编辑重发」正在编辑的那条用户消息在 [`Self::messages`] 里的下标。
    ///
    /// **不变量**：`messages` 的下标恒等于落库 `seq`——消息总是追加在尾部，
    /// 截断也只从尾部截（[`AiStore::truncate_after`]），所以 seq 始终是 0..n 的稠密序列。
    /// 「重新生成 / 编辑重发」依赖这条不变量把下标当 seq 用。
    editing: Option<usize>,
    /// 左栏就地重命名用的输入框。
    ///
    /// **常驻**（与 [`Self::search`] 同款）而不是每次改名新建一个：新建就要挂一条
    /// 订阅，改几次就攒几条；而且输入框实体换掉时，用户正在输的内容会跟着丢。
    rename_input: Entity<InputState>,
    /// 正在改名的会话 id；`None` = 没有在改名。
    renaming: Option<String>,

    /// 正在跑的一轮。
    streaming: Option<Streaming>,
    /// 轮询任务句柄（有流式任务时才存在）。
    poll_task: Option<gpui::Task<()>>,

    /// 状态栏文案。
    status: SharedString,
    /// 最近一次错误（显示在输入区上方，成功一轮后自动清掉）。
    error: Option<SharedString>,

    /// 上次读到的配置版本号。
    ///
    /// 设置页改了配置（**包括只加了一个 API key**）之后回到本页，靠它发现并重读。
    /// 只看 `providers` 的 `PartialEq` 会漏掉「档案没变、只补了密钥」这一类改动。
    ai_revision: u64,
    /// 安全存储此刻是否可用。
    ///
    /// **必须缓存**：`secret::available()` 会真读一次系统存储，而右栏每帧都要用它
    /// 决定凭据那行怎么写。刷新点与 [`Self::ai_revision`] 一致。
    secure_available: bool,
    /// 当前可用工具的注册表（右栏展示用）。
    ///
    /// **不在渲染路径上重建**：装配要读 kv 并 `canonicalize` 每个根目录，
    /// 每帧来一次是没必要的磁盘访问（同 [`Self::secure_available`] 的道理）。
    /// 发送时另外装配一份新的挪进后台线程——`ToolRegistry` 里装的是 `Box<dyn Tool>`，
    /// 给线程用必须转移所有权。
    tools: ToolRegistry,
}

impl AssistantPage {
    /// 创建页面：读配置、开会话库、展开左栏。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| {
            SidebarState::new(
                px(SIDEBAR_DEFAULT_WIDTH),
                px(*SIDEBAR_WIDTH_RANGE.start())..px(*SIDEBAR_WIDTH_RANGE.end()),
                cx,
            )
        });
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(tr(cx, "问点什么…（Enter 发送，Shift+Enter 换行）"))
                .auto_grow(COMPOSER_MIN_ROWS, COMPOSER_MAX_ROWS)
                .submit_on_enter(true)
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "搜索会话…")));
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "会话标题")));
        let scroll = cx.new(|cx| MessageScrollerState::new(0, cx));

        let providers = ai::load_providers(cx);
        let settings = ai::load_settings(cx);
        let selected_provider = settings
            .default_provider
            .clone()
            .filter(|id| providers.iter().any(|profile| &profile.id == id))
            .or_else(|| providers.first().map(|profile| profile.id.clone()));

        let store = AiStore::open_default();
        let (store, error) = match store {
            Ok(store) => (Some(store), None),
            Err(failure) => (
                None,
                Some(SharedString::from(trf(
                    cx,
                    "会话库打开失败：{}",
                    &[&failure.to_string()],
                ))),
            ),
        };
        let conversations = store
            .as_ref()
            .and_then(|store| store.list_conversations().ok())
            .unwrap_or_default();

        // 输入框内容变化要重绘：发送按钮的可用态跟着空 / 非空变。
        let subscriptions = vec![
            cx.subscribe(
                &composer,
                |_this: &mut Self, _source, event: &InputEvent, cx| {
                    if matches!(
                        event,
                        InputEvent::Change | InputEvent::PressEnter { .. } | InputEvent::Focus
                    ) {
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(
                &search,
                |_this: &mut Self, _source, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ),
            // 改名输入框：回车提交，失焦也提交（点别处 = 就是它了）。
            // 空标题视为取消——见 `commit_rename`。
            cx.subscribe(
                &rename_input,
                |this: &mut Self, _source, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        this.commit_rename(cx);
                    }
                },
            ),
        ];

        Self {
            sidebar,
            composer,
            search,
            rename_input,
            scroll,
            view: cx.entity().downgrade(),
            _subscriptions: subscriptions,
            providers,
            settings,
            selected_provider,
            store,
            conversations,
            active: None,
            messages: Vec::new(),
            editing: None,
            renaming: None,
            streaming: None,
            poll_task: None,
            status: tr(cx, "就绪").into(),
            error,
            ai_revision: ai::revision(cx),
            secure_available: ai::secure_storage_available(),
            tools: ai_tools::build_registry(cx),
        }
    }

    /// 供 shell 挂左栏折叠态。
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
    }

    // ------------------------------------------------------------------
    // 配置
    // ------------------------------------------------------------------

    /// 当前选中的档案。
    fn active_profile(&self) -> Option<&ProviderProfile> {
        let id = self.selected_provider.as_ref()?;
        self.providers.iter().find(|profile| &profile.id == id)
    }

    /// 重载配置（设置页改完之后回到本页时调用）。
    fn reload_config(&mut self, cx: &mut Context<Self>) {
        self.providers = ai::load_providers(cx);
        self.settings = ai::load_settings(cx);
        self.secure_available = ai::secure_storage_available();
        self.ai_revision = ai::revision(cx);
        // 工具开关与工作目录都在设置里，所以注册表要跟着一起重装。
        self.tools = ai_tools::build_registry(cx);
        let still_valid = self
            .selected_provider
            .as_ref()
            .is_some_and(|id| self.providers.iter().any(|profile| &profile.id == id));
        if !still_valid {
            self.selected_provider = self
                .settings
                .default_provider
                .clone()
                .filter(|id| self.providers.iter().any(|profile| &profile.id == id))
                .or_else(|| self.providers.first().map(|profile| profile.id.clone()));
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 会话
    // ------------------------------------------------------------------

    /// 刷新会话列表。
    fn refresh_conversations(&mut self) {
        self.conversations = self
            .store
            .as_ref()
            .and_then(|store| store.list_conversations().ok())
            .unwrap_or_default();
    }

    /// 新建会话并切过去。
    fn new_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 先把库操作做完再动 `self` 的其它字段：`store` 借着 `self`，
        // 与后面的 `refresh_conversations`（`&mut self`）不能同时活着。
        let created = {
            let Some(store) = self.store.as_ref() else {
                notify::error(tr(cx, "会话库不可用，无法新建会话"), window, cx);
                return;
            };
            let model = self.active_profile().map(|profile| profile.model.clone());
            store.create_conversation("", model.as_deref())
        };
        match created {
            Ok(conversation) => {
                let id = conversation.id.clone();
                self.refresh_conversations();
                self.load_conversation(&id, cx);
                self.focus_composer(window, cx);
            }
            Err(failure) => {
                self.error = Some(SharedString::from(failure.to_string()));
                cx.notify();
            }
        }
    }

    /// 切换到某个会话，把消息读出来。
    fn load_conversation(&mut self, id: &str, cx: &mut Context<Self>) {
        // 切会话时先停掉正在跑的一轮：否则事件会落到新会话上。
        self.stop_streaming(cx);
        self.active = Some(id.to_owned());
        self.error = None;
        // 编辑态是「针对某条消息」的，跨会话就失效了。
        self.editing = None;
        self.messages = self
            .store
            .as_ref()
            .and_then(|store| store.messages(id).ok())
            .map(|stored| stored.iter().filter_map(|item| item.message()).collect())
            .unwrap_or_default();
        // 恢复这个会话最后使用的模型：档案里模型名相同的就选中它。
        // 会话存的是**模型名**而不是档案 id（档案可能被删改），所以按名字找。
        let saved = self
            .conversations
            .iter()
            .find(|conversation| conversation.id == id)
            .and_then(|conversation| conversation.model.clone());
        if let Some(saved) = saved.filter(|model| !model.trim().is_empty())
            && let Some(profile) = self.providers.iter().find(|profile| profile.model == saved)
        {
            self.selected_provider = Some(profile.id.clone());
        }
        self.status = tr(cx, "就绪").into();
        self.reset_scroller(cx);
        cx.notify();
    }

    /// 删除会话。
    fn delete_conversation(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        if let Err(failure) = store.delete_conversation(id) {
            self.error = Some(SharedString::from(failure.to_string()));
            cx.notify();
            return;
        }
        if self.active.as_deref() == Some(id) {
            self.stop_streaming(cx);
            self.active = None;
            self.messages.clear();
            self.editing = None;
        }
        // 正在改名的就是它的话，输入框跟着行一起收掉。
        if self.renaming.as_deref() == Some(id) {
            self.renaming = None;
        }
        self.refresh_conversations();
        self.sync_scroller(cx, true);
        cx.notify();
    }

    /// 把会话标题按首条用户消息补上（只在标题还是空的时候补）。
    fn ensure_title(&mut self, conversation_id: &str, first_user_text: &str) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let current = self
            .conversations
            .iter()
            .find(|item| item.id == conversation_id)
            .map(|item| item.title.clone())
            .unwrap_or_default();
        if !current.trim().is_empty() {
            return;
        }
        let title = ai_store::title_from_message(first_user_text);
        if title.is_empty() {
            return;
        }
        let _ = store.rename_conversation(conversation_id, &title);
        self.refresh_conversations();
    }

    // ------------------------------------------------------------------
    // 消息流
    // ------------------------------------------------------------------

    /// 错误是否落在消息流里（而不是输入区那行状态文案）。
    ///
    /// 两者只留一处：**已经聊起来之后**出错，用户的眼睛在消息流末尾，
    /// 那里给一个破坏色气泡；**还没聊起来**（比如没有可用厂商、会话库打不开）
    /// 就没有「末尾」可落，只能走输入区那行。
    fn error_is_inline(&self) -> bool {
        self.error.is_some() && !self.messages.is_empty()
    }

    /// 构造渲染快照。
    ///
    /// 三段拼接：**已落库的历史** → **本轮已定稿的轮次**（内核 `RoundCompleted` 送来的）
    /// → **当前轮的实时正文与工具卡**。
    ///
    /// 中间那段是关键：工具循环一轮要几十秒，前几轮不该压着不显示。
    /// 而且因为它是「内核定稿后的原样消息」，取消时不需要任何重建——
    /// 屏幕上出现过什么，落库的就是什么。
    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = Vec::new();
        push_message_rows(
            &self.messages,
            "assistant-msg",
            self.message_actions(),
            &mut rows,
        );
        if let Some(streaming) = self.streaming.as_ref() {
            // 正在跑的一轮不给行内操作：那几行还没落库，没有稳定的 seq。
            push_message_rows(
                &streaming.live,
                "assistant-live",
                MessageActions::default(),
                &mut rows,
            );

            let has_text = !(streaming.text.is_empty() && streaming.thinking.is_empty());
            // 空正文的占位气泡只在**没有任何别的可看内容**时出现：
            // 已经跑了工具还挂一个「…」会让人以为卡住了。
            if has_text || streaming.tool_runs.is_empty() {
                rows.push(Row {
                    id: SharedString::from("assistant-streaming"),
                    action: None,
                    kind: RowKind::Assistant {
                        text: streaming.text.clone(),
                        thinking: (!streaming.thinking.is_empty())
                            .then(|| streaming.thinking.clone()),
                    },
                });
            }
            for run in &streaming.tool_runs {
                rows.push(Row {
                    id: SharedString::from(format!("assistant-live-tool-{}", run.call_id)),
                    action: None,
                    kind: RowKind::Tool(Box::new(ToolCard {
                        name: run.name.clone(),
                        summary: run.summary.clone(),
                        state: run.state,
                        result: match run.state {
                            ToolState::Running => None,
                            ToolState::Finished | ToolState::Denied => Some(ToolResultView {
                                ok: run.ok,
                                content: run.preview.clone(),
                            }),
                        },
                        // 运行中的卡片不显示产物表格：表格在结果到达前是空的。
                        detail: run.detail.clone(),
                    })),
                });
            }
        }
        if self.error_is_inline()
            && let Some(error) = self.error.clone()
        {
            rows.push(Row {
                id: SharedString::from("assistant-error"),
                // 失败的那一轮没产出任何助手消息，尾部仍然是那条用户提问，
                // 所以「重试」就是把它原样再发一次。
                action: self.streaming.is_none().then_some(RowAction::Retry),
                kind: RowKind::Error { text: error.into() },
            });
        }
        rows
    }

    /// 哪些消息下标上要挂行内操作。
    ///
    /// 流式期间一律不给：此时尾部有还没落库的行，用户点「重新生成」会删掉正在跑的这一轮。
    fn message_actions(&self) -> MessageActions {
        if self.streaming.is_some() {
            return MessageActions::default();
        }
        MessageActions {
            last_assistant: self
                .messages
                .iter()
                .rposition(|message| message.role == Role::Assistant),
            last_user: self
                .messages
                .iter()
                .rposition(|message| message.role == Role::User),
        }
    }

    /// 让滚动列表的行数与快照一致；`remeasure` 决定要不要重测最后一行。
    ///
    /// **不能在 `render` 里调**：`MessageScrollerState` 的每次变更都会 `notify`，
    /// 而 `notify` 会让窗口重绘 → 又回到这里 → 自激。本仓 `dialog.rs` 已经踩过这个坑。
    fn sync_scroller(&mut self, cx: &mut Context<Self>, remeasure: bool) {
        let target = self.rows().len();
        let state = self.scroll.clone();
        state.update(cx, |state, cx| {
            let current = state.item_count();
            if current < target {
                state.append(target - current, cx);
            } else if current > target {
                // `reset` 会把全部行标成「未测量」，等于全量重测。
                state.reset(target, cx);
            } else if remeasure && target > 0 {
                // **行数没变但行内容变了**：流式增长时最后一行在长高；一轮结束、
                // 或开始新一轮（行内操作按钮出现 / 消失）时末尾两行的高度也会变。
                // `append` 只重测最后一行、`reset` 只在行数变化时走，覆盖不到这两种
                // 情形，所以这里显式重测末尾两行——「编辑重发」挂在倒数第二条上。
                state.remeasure_items(target.saturating_sub(2)..target, cx);
            }
        });
    }

    /// 全量重测：整份列表的内容都换了（切会话），逐行比对高度没有意义。
    fn reset_scroller(&mut self, cx: &mut Context<Self>) {
        let target = self.rows().len();
        let state = self.scroll.clone();
        state.update(cx, |state, cx| state.reset(target, cx));
    }

    /// 清掉输入框。
    fn clear_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
    }

    /// 把焦点交还输入框。
    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.composer.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    // ------------------------------------------------------------------
    // 发送与流式
    // ------------------------------------------------------------------

    /// 发送当前输入。
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.streaming.is_some() {
            return;
        }
        let prompt = self.composer.read(cx).value().trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        let profile = match self.ready_profile(cx) {
            Ok(profile) => profile,
            Err(message) => {
                self.error = Some(SharedString::from(message));
                cx.notify();
                return;
            }
        };

        // 没有会话就顺手建一个。
        if self.active.is_none() {
            self.new_conversation(window, cx);
            if self.active.is_none() {
                return;
            }
        }
        let conversation_id = self.active.clone().expect("just ensured");

        // 「编辑重发」：先从被编辑的那条消息起截断，这次发送的内容就是新的正文。
        if let Some(index) = self.editing.take() {
            self.truncate_from(index, cx);
        }

        // 用户消息：先落库再入内存——落库失败时不该让界面显示一条其实没存下来的消息。
        let user_message = ChatMessage::user(prompt.clone());
        if let Some(store) = self.store.as_ref()
            && let Err(failure) = store.append_message(&conversation_id, &user_message, None)
        {
            self.error = Some(SharedString::from(failure.to_string()));
            cx.notify();
            return;
        }
        self.messages.push(user_message.clone());
        self.ensure_title(&conversation_id, &prompt);
        self.clear_composer(window, cx);
        self.error = None;
        self.begin_turn(profile, cx);
    }

    /// 取本轮要用的档案并校验；失败时返回给用户看的文案。
    ///
    /// 三条前置条件（缺厂商 / 档案不完整 / 会话库不可用）在「发送 / 重新生成 / 重试」
    /// 三条路径上完全一致，所以收敛到一处——复制三遍迟早会漏掉一条。
    fn ready_profile(&self, cx: &App) -> Result<ProviderProfile, String> {
        let Some(profile) = self.active_profile().cloned() else {
            return Err(tr(cx, "还没有可用的模型厂商，先到「设置 → AI」添加一个").to_owned());
        };
        if let Err(failure) = profile.validate() {
            return Err(failure.to_string());
        }
        if self.store.is_none() {
            return Err(tr(cx, "会话库不可用，无法保存这轮对话").to_owned());
        }
        Ok(profile)
    }

    /// 组装历史并把这一轮交给后台线程。
    ///
    /// 进入前 `self.messages` 必须已经以「要发给模型的完整对话」结尾，且 `profile`
    /// 已经过 [`Self::ready_profile`] 校验。
    fn begin_turn(&mut self, profile: ProviderProfile, cx: &mut Context<Self>) {
        let Some(conversation_id) = self.active.clone() else {
            return;
        };
        // 任何一轮开始都意味着「编辑态」结束——否则「重新生成」之后再发送一次，
        // 会把早已被截断的那条消息又截一遍。
        self.editing = None;
        // 记下会话最后使用的模型：切回这个会话时按它恢复输入区上方的模型控件。
        if let Some(store) = self.store.as_ref() {
            let _ = store.set_conversation_model(&conversation_id, &profile.model);
        }

        // 组装发给模型的完整对话：system 在最前。
        let mut history = Vec::with_capacity(self.messages.len() + 1);
        if !self.settings.system_prompt.trim().is_empty() {
            history.push(ChatMessage::system(self.settings.system_prompt.clone()));
        }
        history.extend(self.messages.iter().cloned());

        let options = ChatOptions {
            temperature: self.settings.temperature,
            max_tokens: self.settings.max_tokens,
            timeout_secs: REQUEST_TIMEOUT_SECS,
            // 上下文预算可在「设置 → AI → 生成参数」里调（`0` = 关闭压缩）；未设置时
            // 用内核默认口径。这里读的是**设置快照**，改完设置立即对下一轮生效。
            compaction: ai::compaction_policy(&self.settings),
        };
        // 轮次上限可配置（设置 → AI → 生成参数）；`.max(1)` 兜住手改 kv 写入的 0。
        let max_rounds = self
            .settings
            .max_tool_rounds
            .unwrap_or(MAX_TOOL_ROUNDS)
            .max(1);
        let handle = RunHandle::new();
        let (sender, receiver) = mpsc::channel::<StreamMessage>();
        let thread_handle = handle.clone();
        let thread_profile = profile.clone();
        let model = profile.model.clone();
        // 线程里用副本：`model` 本身还要留着构造 `Streaming` 与状态栏文案。
        let thread_model = model.clone();
        let provider_id = profile.id.clone();
        // 注册表按当前配置**现装一份**挪进线程：`ToolRegistry` 里是 `Box<dyn Tool>`，
        // 不能借用 `self.tools`，而 `build_registry` 只读 kv 内存快照，代价可忽略。
        let registry = ai_tools::build_registry(cx);

        // 专用线程而不是 `background_executor`：这是一次长驻的阻塞读，
        // 放进共享池会占住 worker 影响其他任务（`duck-store` 的写线程是先例）。
        let spawned = std::thread::Builder::new()
            .name("duck-ai-stream".into())
            .spawn(move || {
                // key 直接从档案副本里解出来：密文随 `profile` 一起进了线程，
                // 不再有「后台再去系统存储取一次」那一步。解密是纯内存操作
                // （密钥已缓存），所以在这里做不会卡住 UI 线程。
                let api_key = match thread_profile
                    .api_key
                    .as_deref()
                    .and_then(duck_ai::secret::open)
                {
                    Some(key) => key,
                    // 解不开（换了机器、密钥文件被删）与「本来就还没配」在这里
                    // 是同一件事：都当作没配密钥，报同一句话。
                    None if thread_profile.needs_key() => {
                        let _ = sender.send(StreamMessage::Failed(
                            "no API key stored for this provider".to_owned(),
                        ));
                        return;
                    }
                    None => String::new(),
                };

                let mut sink = |event: AgentEvent| {
                    let _ = sender.send(StreamMessage::Event(event));
                };
                // 审批闸门：只读自动放行，其余一律拒绝并把原因回灌给模型。
                // 本页当前只注册了只读工具，所以 `Deny` 分支是为「将来某个工具把风险
                // 升到 `NeedsApproval`」准备的——它保证那种情况下**不会**被静默执行。
                let mut approve = |_call: &duck_ai::ToolCall, risk: ToolRisk| {
                    if risk.needs_approval() {
                        Approval::Deny(
                            "the user has not approved this operation; \
                             describe what you wanted to do instead of doing it"
                                .to_owned(),
                        )
                    } else {
                        Approval::Run
                    }
                };
                let result = duck_ai::run_agent(
                    &thread_profile,
                    &api_key,
                    &thread_model,
                    &history,
                    &AgentOptions {
                        chat: options,
                        max_rounds,
                    },
                    &registry,
                    &mut approve,
                    &thread_handle,
                    &mut sink,
                );
                let _ = match result {
                    Ok(outcome) => sender.send(StreamMessage::Done(Box::new(outcome))),
                    Err(failure) => sender.send(StreamMessage::Failed(failure.to_string())),
                };
            });

        if let Err(failure) = spawned {
            self.error = Some(SharedString::from(trf(
                cx,
                "无法启动请求线程：{}",
                &[&failure.to_string()],
            )));
            cx.notify();
            return;
        }

        let model_for_status = model.clone();
        self.streaming = Some(Streaming {
            handle,
            receiver,
            provider_id,
            model,
            text: String::new(),
            thinking: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::default(),
            started: false,
            tool_runs: Vec::new(),
            live: Vec::new(),
            rounds_exhausted: None,
        });
        self.status = trf(cx, "正在生成（{}）…", &[&model_for_status]).into();
        self.sync_scroller(cx, true);
        // 上一轮的轮询任务已经**自己返回**了（`drain_stream` 一报 `false` 就 `return`），
        // 但句柄还挂在字段上——`Task` 完成不等于字段变 `None`。不在这里丢掉它，
        // `start_poller` 的幂等判断会把新一轮挡在门外：**只有第一条消息会流式输出**。
        // 丢掉一个已完成的 `Task` 是空操作（不涉及从任务内部取消自己）。
        self.poll_task = None;
        self.start_poller(cx);
        cx.notify();
    }

    /// 起前台轮询任务（幂等）。
    ///
    /// 内核的事件走 `std::sync::mpsc`，GPUI 的事件循环看不见它，所以要有一个前台任务
    /// 定期取走并 `notify`。任务持有视图**弱引用**：窗口一关、视图释放，
    /// `update` 返回 `Err`，循环自己结束。
    fn start_poller(&mut self, cx: &mut Context<Self>) {
        if self.poll_task.is_some() {
            return;
        }
        self.poll_task = Some(cx.spawn(async move |page, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                match page.update(cx, |page, cx| page.drain_stream(cx)) {
                    // 一轮还在跑就继续轮询。
                    Ok(true) => {}
                    // 收尾了：任务结束，下一轮发送时再起。
                    Ok(false) => return,
                    // 视图没了。
                    Err(_) => return,
                }
            }
        }));
    }

    /// 取走待处理事件；返回是否还有一轮在跑。
    fn drain_stream(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(streaming) = self.streaming.as_mut() else {
            return false;
        };
        let mut done: Option<Result<Box<AgentOutcome>, String>> = None;
        loop {
            match streaming.receiver.try_recv() {
                Ok(StreamMessage::Event(event)) => {
                    let stop = absorb_agent_event(streaming, event);
                    if let Some(message) = stop {
                        done = Some(Err(message));
                        break;
                    }
                }
                Ok(StreamMessage::Done(outcome)) => {
                    done = Some(Ok(outcome));
                    break;
                }
                Ok(StreamMessage::Failed(message)) => {
                    done = Some(Err(message));
                    break;
                }
                Err(TryRecvError::Empty) => break,
                // 线程没了又没送收尾消息（panic）。
                Err(TryRecvError::Disconnected) => {
                    if done.is_none() {
                        done = Some(Err(tr(cx, "生成中断：请求线程提前退出").to_owned()));
                    }
                    break;
                }
            }
        }

        if let Some(result) = done {
            self.finish_stream(result, cx);
            return false;
        }
        // 有增量就重绘并把最后一行重测一次。
        self.sync_scroller(cx, true);
        cx.notify();
        true
    }

    /// 收尾一轮：落库、写入内存、复位状态。
    fn finish_stream(&mut self, result: Result<Box<AgentOutcome>, String>, cx: &mut Context<Self>) {
        let Some(streaming) = self.streaming.take() else {
            return;
        };

        match result {
            Ok(outcome) => {
                // 落库的**以 `live` 为准**，不以 `outcome.messages` 为准：二者本应完全
                // 相同，但 `live` 是「屏幕上真出现过的东西」。万一某条 `RoundCompleted`
                // 因为线程出意外没送达，用 `live` 至少不会凭空多出用户没见过的内容。
                let mut messages = streaming.live.clone();
                if outcome.cancelled {
                    // 被掐断的那一轮正文只到一半，内核不会把它算成消息；
                    // 但用户已经在屏幕上看到了，丢掉会让界面「跳回去」。
                    if let Some(partial) = partial_from_buffers(&streaming) {
                        messages.push(partial);
                    }
                }
                let usage = outcome.usage;
                let stop_reason = outcome.stop_reason;
                let cancelled = outcome.cancelled;
                let exhausted = streaming.rounds_exhausted;
                let compaction = outcome.compaction;

                self.persist_messages(&messages, Some(usage));
                self.status = if cancelled {
                    tr(cx, "已停止").into()
                } else if exhausted.is_some() {
                    trf(
                        cx,
                        "已达最大工具轮次（{}）",
                        &[&exhausted.unwrap_or_default().to_string()],
                    )
                    .into()
                } else if compaction.changed() {
                    // 长会话里内核会在发请求前压缩上下文。说清楚「省了什么」，
                    // 否则用户只会发现「模型忽然忘了前面的事」而不知道发生了什么。
                    trf(
                        cx,
                        "已压缩较早上下文（省略 {} 条消息 / 裁剪 {} 条工具结果）",
                        &[
                            &compaction.dropped_messages.to_string(),
                            &compaction.pruned_tool_results.to_string(),
                        ],
                    )
                    .into()
                } else {
                    match stop_reason {
                        StopReason::MaxTokens => tr(cx, "已达最大输出长度").into(),
                        StopReason::ToolUse => tr(cx, "模型请求调用工具").into(),
                        _ => tr(cx, "就绪").into(),
                    }
                };
                self.error = None;
            }
            Err(message) => {
                // 取消是用户意图，不是错误。判据用**句柄**而不是匹配错误文案：
                // 后者是给人看的字符串，改一次就会让这里静默失灵。
                if streaming.handle.is_cancelled() {
                    // 半截回答也留下——用户已经看到它了，丢掉会让界面「跳回去」。
                    // 已经定稿的那几轮（含工具调用）照常落库，它们是真发生过的。
                    let mut messages = streaming.live.clone();
                    if let Some(partial) = partial_from_buffers(&streaming) {
                        messages.push(partial);
                    }
                    self.persist_messages(&messages, None);
                    self.status = tr(cx, "已停止").into();
                } else {
                    // 请求本身失败了：已经定稿的轮次仍然落库，只有当前这轮作废。
                    let messages = streaming.live.clone();
                    self.persist_messages(&messages, None);
                    self.error = Some(SharedString::from(message));
                    self.status = tr(cx, "出错了").into();
                }
            }
        }
        self.sync_scroller(cx, true);
        cx.notify();
    }

    /// 把一串消息写进内存与库（按顺序逐条落）。
    ///
    /// `usage` 只挂在**最后一条助手消息**上：用量是一轮问答的总和，
    /// 摊到每条消息上会显示成「每条都花了这么多」。
    fn persist_messages(&mut self, messages: &[ChatMessage], usage: Option<Usage>) {
        let Some(conversation_id) = self.active.clone() else {
            return;
        };
        let last_assistant = messages
            .iter()
            .rposition(|message| message.role == Role::Assistant);
        let mut failure: Option<String> = None;
        for (index, message) in messages.iter().enumerate() {
            if message.is_empty() {
                continue;
            }
            let message_usage = (Some(index) == last_assistant).then_some(usage).flatten();
            let error = self.store.as_ref().and_then(|store| {
                store
                    .append_message(&conversation_id, message, message_usage)
                    .err()
            });
            if let Some(error) = error {
                failure = Some(error.to_string());
            }
            self.messages.push(message.clone());
        }
        if let Some(failure) = failure {
            self.error = Some(SharedString::from(failure));
        }
        self.refresh_conversations();
    }

    /// 停掉正在跑的一轮。
    fn stop_streaming(&mut self, cx: &mut Context<Self>) {
        if let Some(streaming) = self.streaming.as_ref() {
            streaming.handle.cancel();
        }
        // 真正的收尾由下一轮轮询完成（线程会送回 `Failed(cancelled)`）。
        // 这里只保证「停止」按钮立刻变成不可再点。
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 重新生成 / 编辑重发 / 重试
    // ------------------------------------------------------------------

    /// 从 `messages[index]` 起把内存与库里的消息一起删掉。
    ///
    /// **下标即 `seq`**：见 [`Self::messages`] 字段上的不变量说明。
    /// 落库失败时不动内存——宁可什么都不删，也不要让界面和库里对不上。
    fn truncate_from(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.messages.len() {
            return;
        }
        let Some(conversation_id) = self.active.clone() else {
            return;
        };
        if let Some(store) = self.store.as_ref()
            && let Err(failure) = store.truncate_after(&conversation_id, index as i64)
        {
            self.error = Some(SharedString::from(failure.to_string()));
            cx.notify();
            return;
        }
        self.messages.truncate(index);
    }

    /// 「重新生成」：删掉这条助手消息及其之后的一切，按剩下的历史重发。
    fn regenerate(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.streaming.is_some() {
            return;
        }
        // 只对助手消息开放：截断点必须落在「提问之后、回答之前」，否则重发的输入不对。
        if self.messages.get(index).map(|message| message.role) != Some(Role::Assistant) {
            return;
        }
        let profile = match self.ready_profile(cx) {
            Ok(profile) => profile,
            Err(message) => {
                self.error = Some(SharedString::from(message));
                cx.notify();
                return;
            }
        };
        self.truncate_from(index, cx);
        // 截断后尾部必须是一条用户消息，否则这一轮没有可重发的输入。
        if self.messages.last().map(|message| message.role) != Some(Role::User) {
            self.error = Some(SharedString::from(tr(cx, "这条回答前面没有可重发的提问")));
            cx.notify();
            return;
        }
        self.error = None;
        self.begin_turn(profile, cx);
    }

    /// 「编辑重发」：把这条用户消息的正文填回输入框，发送时从它重新开始。
    fn begin_edit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.streaming.is_some() {
            return;
        }
        if self.messages.get(index).map(|message| message.role) != Some(Role::User) {
            return;
        }
        let text = self.messages[index].text();
        self.composer
            .update(cx, |state, cx| state.set_value(text, window, cx));
        self.editing = Some(index);
        self.error = None;
        self.status = tr(cx, "正在编辑消息，发送后将从这里重新开始").into();
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// 退出编辑态（不清库：用户只是不想改了）。
    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_none() {
            return;
        }
        self.clear_composer(window, cx);
        self.status = tr(cx, "就绪").into();
        cx.notify();
    }

    /// 「重试」：把最后一条用户消息原样重发。
    ///
    /// 失败时 `finish_stream` 只会落库**已经定稿的轮次**，所以出错后尾部通常仍是
    /// 那条用户消息；万一不是（多轮工具跑到一半断的），就明确报错而不是乱发。
    fn retry(&mut self, cx: &mut Context<Self>) {
        if self.streaming.is_some() {
            return;
        }
        let profile = match self.ready_profile(cx) {
            Ok(profile) => profile,
            Err(message) => {
                self.error = Some(SharedString::from(message));
                cx.notify();
                return;
            }
        };
        if self.messages.last().map(|message| message.role) != Some(Role::User) {
            self.error = Some(SharedString::from(tr(cx, "没有可重试的提问")));
            cx.notify();
            return;
        }
        self.error = None;
        self.begin_turn(profile, cx);
    }

    // ------------------------------------------------------------------
    // 会话重命名
    // ------------------------------------------------------------------

    /// 进入某个会话的就地重命名。
    fn start_rename(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .conversations
            .iter()
            .find(|conversation| conversation.id == id)
            .map(|conversation| conversation.title.clone())
            .unwrap_or_default();
        self.rename_input
            .update(cx, |state, cx| state.set_value(title, window, cx));
        self.renaming = Some(id);
        let handle = self.rename_input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        cx.notify();
    }

    /// 提交改名：空标题视为取消（不写库、保持原标题）。
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.renaming.take() else {
            return;
        };
        let title = self.rename_input.read(cx).value().trim().to_owned();
        if title.is_empty() {
            cx.notify();
            return;
        }
        if let Some(store) = self.store.as_ref()
            && let Err(failure) = store.rename_conversation(&id, &title)
        {
            self.error = Some(SharedString::from(failure.to_string()));
        }
        self.refresh_conversations();
        cx.notify();
    }

    /// 取消就地改名：不写库、保持原标题。
    ///
    /// **先把 `renaming` 清空是这里的关键**：输入框随渲染树消失时会派发一次 `Blur`，
    /// 而 `commit_rename` 的第一行就是 `renaming.take()`——取到 `None` 便直接返回。
    /// 所以这次取消不会被随后的失焦提交覆盖，不需要额外的「正在取消」标志位。
    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.renaming.take().is_none() {
            return;
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 渲染
    // ------------------------------------------------------------------

    /// 消息流区域。
    fn render_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        // 整份主题快照 clone 进行渲染闭包：闭包必须 `'static`，不能再借 `cx` 的全局。
        let theme = active_theme(cx).clone();
        let hairline = theme.extended().colors.hairline;
        let rows = Rc::new(self.rows());

        if rows.is_empty() {
            let (title, description, action) = if self.providers.is_empty() {
                (
                    tr(cx, "还没有配置模型厂商"),
                    Some(
                        tr(
                            cx,
                            "到「设置 → AI」里添加一个厂商并填入 API key，回来就能开始对话。",
                        )
                        .to_owned(),
                    ),
                    Some(
                        button(
                            "assistant-open-ai-settings",
                            tr(cx, "去设置"),
                            ButtonVariant::Primary,
                            cx,
                        )
                        .on_click(|_, _window, cx| {
                            crate::pages::open_settings_at("ai", None, cx);
                        })
                        .into_any_element(),
                    ),
                )
            } else {
                (
                    tr(cx, "开始新的对话"),
                    Some(tr(cx, "在下面输入问题，回车发送。回答支持 Markdown 渲染。").to_owned()),
                    None,
                )
            };
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px(theme.tokens().spacing.xl)
                .child(empty_state(
                    DuckIcon::Bot,
                    title,
                    description.map(SharedString::from),
                    action,
                    cx,
                ))
                .into_any_element();
        }

        let renderer_rows = Rc::clone(&rows);
        let render_theme = theme;
        // 行内操作要能把动作送回页面，而 `MessageScroller` 的行闭包是 `'static` 的：
        // 弱引用可以挪进去，`&self` 不行。
        let renderer_view = self.view.clone();
        MessageScroller::new(
            "assistant-transcript",
            self.scroll.clone(),
            move |index, _window, cx| {
                let Some(row) = renderer_rows.get(index) else {
                    return div().into_any_element();
                };
                render_row(row, &render_theme, &renderer_view, cx)
            },
        )
        .scrollbar(true)
        .jump_button(true)
        .with_jump_button_label(tr(cx, "回到最新"))
        .with_bottom_fade(hairline)
        .into_any_element()
    }

    /// 输入区（模型控件 + 文本框 + 发送 / 停止）。
    fn render_composer(&self, cx: &mut Context<Self>) -> Div {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let view = self.view.clone();
        let busy = self.streaming.is_some();

        // 模型控件：候选来自设置页已配置的厂商档案。选项拼法与设置页共用
        // `ai::model_options`——两处各拼一遍就会出现「档案名改了、这里没跟着改」。
        let options: Vec<(SharedString, SharedString)> = ai::model_options(&self.providers)
            .into_iter()
            .map(|(value, label)| (SharedString::from(value), SharedString::from(label)))
            .collect();
        let current = self
            .selected_provider
            .clone()
            .unwrap_or_else(|| ai::PLACEHOLDER_MODEL.to_owned());
        let model_picker = dropdown_select(
            "assistant-model",
            options,
            SharedString::from(current),
            {
                let view = view.clone();
                move |value, _window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.selected_provider = Some(value.to_string());
                        this.status = tr(cx, "已切换模型").into();
                        // 顺手记到当前会话上：切走再切回来时选中项要跟着它走。
                        if let Some(store) = this.store.as_ref()
                            && let Some(conversation_id) = this.active.clone()
                            && let Some(model) =
                                this.active_profile().map(|profile| profile.model.clone())
                        {
                            let _ = store.set_conversation_model(&conversation_id, &model);
                        }
                        cx.notify();
                    });
                }
            },
            cx,
        );

        // 错误已经落进消息流时就别再重复一遍（见 `error_is_inline`）。
        let inline_error = self.error_is_inline();
        let status_text = if inline_error {
            self.status.clone()
        } else {
            self.error.clone().unwrap_or_else(|| self.status.clone())
        };

        let mut header = h_flex().w_full().items_center().gap(tokens.spacing.sm);
        if self.editing.is_some() {
            header = header
                .child(accent_tag(SharedString::from(tr(cx, "编辑中")), cx))
                .child(
                    icon_button(
                        "assistant-cancel-edit",
                        tr(cx, "取消编辑"),
                        Icon::new(DuckIcon::X)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.cancel_edit(window, cx));
                        }
                    }),
                );
        }
        let header = header
            .child(model_picker)
            .child(div().flex_1().min_w_0())
            .child(
                div()
                    .flex_none()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .text_color(if inline_error || self.error.is_none() {
                        extended.colors.text_tertiary
                    } else {
                        tokens.colors.destructive
                    })
                    .child(status_text),
            );

        let can_send = !busy && !self.composer.read(cx).value().trim().is_empty();

        let action = if busy {
            icon_button(
                "assistant-stop",
                tr(cx, "停止生成"),
                Icon::new(DuckIcon::Square)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.destructive),
                ButtonVariant::Destructive,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.stop_streaming(cx));
                }
            })
            .into_any_element()
        } else {
            icon_button(
                "assistant-send",
                tr(cx, "发送"),
                Icon::new(DuckIcon::Send)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.primary_foreground),
                ButtonVariant::Primary,
                cx,
            )
            .disabled(!can_send)
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.send(window, cx));
                }
            })
            .into_any_element()
        };

        v_flex()
            .w_full()
            .flex_none()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.lg)
            .pt(tokens.spacing.md)
            .pb(tokens.spacing.lg)
            .border_t_1()
            .border_color(extended.colors.hairline)
            .child(header)
            .child(
                h_flex()
                    .w_full()
                    .items_end()
                    .gap(tokens.spacing.sm)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .rounded(theme.components().input_radius)
                            .border(extended.stroke.thin)
                            .border_color(tokens.colors.border)
                            .bg(tokens.colors.surface)
                            .px(tokens.spacing.sm)
                            .py(tokens.spacing.xs)
                            .child(
                                Textarea::new(&self.composer)
                                    .appearance(false)
                                    // 输入区没有可见的行标题，名字只能显式写：
                                    // 读屏下「一个没有名字的多行输入框」等于没有。
                                    .aria_label(tr(cx, "消息输入框"))
                                    .w_full()
                                    .text_size(tokens.typography.sm.size)
                                    .line_height(tokens.typography.sm.line_height),
                            ),
                    )
                    .child(action),
            )
    }

    /// 左栏：会话历史。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let view = self.view.clone();
        let query = self.search.read(cx).value().trim().to_lowercase();

        let new_button = button(
            "assistant-new-conversation",
            tr(cx, "新建会话"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click({
            let view = view.clone();
            move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.new_conversation(window, cx));
            }
        });

        let mut nav = v_flex().w_full().gap(tokens.spacing.xxs);
        let visible: Vec<&Conversation> = self
            .conversations
            .iter()
            .filter(|conversation| {
                query.is_empty() || conversation.title.to_lowercase().contains(&query)
            })
            .collect();

        if visible.is_empty() {
            nav = nav.child(
                div()
                    .w_full()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.md)
                    .child(meta_text(
                        if self.conversations.is_empty() {
                            tr(cx, "还没有会话")
                        } else {
                            tr(cx, "没有匹配的会话")
                        },
                        cx,
                    )),
            );
        }

        for conversation in visible {
            let id = conversation.id.clone();
            let selected = self.active.as_deref() == Some(id.as_str());

            // 正在就地改名的会话：整行换成输入框。提交走「回车 / 失焦」，
            // 空标题视为取消（见 `commit_rename`）——所以这里不再放取消按钮：
            // 点按钮会先触发输入框的 `Blur`，提交先跑，取消按钮永远来不及生效。
            if self.renaming.as_deref() == Some(id.as_str()) {
                nav = nav.child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.xxs)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .rounded(theme.components().input_radius)
                                .border(extended.stroke.thin)
                                .border_color(tokens.colors.border)
                                .bg(tokens.colors.surface)
                                .px(tokens.spacing.sm)
                                // `Esc` 取消改名。`InputEvent` 只有 `Change` /
                                // `PressEnter` / `Focus` / `Blur` 四种，接不到 `Esc`，
                                // 所以只能挂在输入框的**祖先**上收冒泡事件
                                //（`Input` 仅在 `disabled` 时才 `stop_propagation`）。
                                .on_key_down(cx.listener(
                                    |this, event: &KeyDownEvent, _window, cx| {
                                        if event.keystroke.key.eq_ignore_ascii_case("escape")
                                            && !event.keystroke.modifiers.modified()
                                        {
                                            this.cancel_rename(cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                                .child(
                                    Input::new(&self.rename_input)
                                        .appearance(false)
                                        .aria_label(tr(cx, "会话标题"))
                                        .w_full()
                                        .text_size(tokens.typography.sm.size)
                                        .line_height(tokens.typography.sm.line_height),
                                ),
                        ),
                );
                continue;
            }

            let count = self
                .store
                .as_ref()
                .and_then(|store| store.message_count(&id).ok())
                .unwrap_or(0);
            let trailing = h_flex()
                .flex_none()
                .gap(tokens.spacing.xxs)
                .child(neutral_tag(
                    SharedString::from(trf(cx, "{} 条", &[&count.to_string()])),
                    cx,
                ))
                .child(
                    icon_button(
                        SharedString::from(format!("assistant-rename-{id}")),
                        tr(cx, "重命名会话"),
                        Icon::new(DuckIcon::Pencil)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        let id = id.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.start_rename(id.clone(), window, cx);
                            });
                        }
                    }),
                )
                .child(
                    icon_button(
                        SharedString::from(format!("assistant-delete-{id}")),
                        tr(cx, "删除会话"),
                        Icon::new(DuckIcon::Trash2)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        let id = id.clone();
                        move |_, _window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.delete_conversation(&id, cx);
                            });
                        }
                    }),
                );

            let label = if conversation.title.trim().is_empty() {
                SharedString::from(tr(cx, "新会话"))
            } else {
                SharedString::from(conversation.title.clone())
            };

            nav = nav.child(
                sidebar_navigation_button(
                    SharedString::from(format!("assistant-conv-{id}")),
                    label,
                    Icon::new(DuckIcon::Bot)
                        .size(extended.icon.md)
                        .text_color(if selected {
                            extended.colors.nav_selected_icon
                        } else {
                            tokens.colors.muted_foreground
                        }),
                    trailing,
                    selected,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    let id = id.clone();
                    move |_, _window, cx| {
                        let _ = view.update(cx, |this, cx| this.load_conversation(&id, cx));
                    }
                }),
            );
        }

        v_flex()
            .w_full()
            .h_full()
            .min_h_0()
            .gap(tokens.spacing.sm)
            .p(tokens.spacing.sm)
            .child(new_button)
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .rounded(theme.components().input_radius)
                    .border(extended.stroke.thin)
                    .border_color(tokens.colors.border)
                    .bg(tokens.colors.surface)
                    .px(tokens.spacing.sm)
                    .child(
                        Input::new(&self.search)
                            .appearance(false)
                            .aria_label(tr(cx, "搜索会话"))
                            .w_full()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height),
                    ),
            )
            .child(
                div()
                    .id("assistant-conversation-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(nav),
            )
            .into_any_element()
    }

    /// 右栏：会话参数（只读）。
    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();

        let profile = self.active_profile();
        let mut rows: Vec<AnyElement> = Vec::new();

        let push_pair = |label: &'static str, value: String, rows: &mut Vec<AnyElement>| {
            rows.push(
                h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .gap(tokens.spacing.sm)
                    .child(meta_text(tr(cx, label), cx))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(selectable_text(SharedString::from(value))),
                    )
                    .into_any_element(),
            );
        };

        match profile {
            Some(profile) => {
                // 流式途中以**本轮实际发出的**厂商 / 模型为准：选择器可以在中途切走，
                // 但已经在跑的这一轮仍然钉在发出去时那个档案上，显示成新档案会骗人。
                let (vendor, model) = match self.streaming.as_ref() {
                    Some(streaming) => (
                        self.providers
                            .iter()
                            .find(|item| item.id == streaming.provider_id)
                            .map(ai::profile_display_name)
                            .unwrap_or_else(|| streaming.provider_id.clone()),
                        streaming.model.clone(),
                    ),
                    None => (ai::profile_display_name(profile), profile.model.clone()),
                };
                push_pair("厂商", vendor, &mut rows);
                push_pair(
                    "模型",
                    if model.is_empty() {
                        tr(cx, "未填写").to_owned()
                    } else {
                        model
                    },
                    &mut rows,
                );
                push_pair("端点", profile.base_url.clone(), &mut rows);
                // 这里是渲染路径（每帧），所以只读 kv 里那份档案标记，**不解密**：
                // 真正的解密在 `send` 起的后台线程与「打开编辑弹窗回显」两处。
                push_pair(
                    "凭据",
                    if profile.is_local {
                        tr(cx, "本地端点，无需密钥").to_owned()
                    } else if !self.secure_available {
                        tr(cx, "凭据存储不可用").to_owned()
                    } else if ai::load_configured(cx).iter().any(|id| id == &profile.id) {
                        tr(cx, "已加密保存在本机").to_owned()
                    } else {
                        tr(cx, "尚未配置").to_owned()
                    },
                    &mut rows,
                );
            }
            None => rows.push(meta_text(tr(cx, "还没有配置任何厂商"), cx).into_any_element()),
        }

        // 会话级累计用量。**不是本轮的**——本轮那份在流式途中另有一行（见下）。
        if let Some(conversation) = self
            .active
            .as_ref()
            .and_then(|id| self.conversations.iter().find(|item| &item.id == id))
        {
            push_pair(
                "累计用量",
                trf(
                    cx,
                    "输入 {} / 输出 {}",
                    &[
                        &number(conversation.input_tokens as i64),
                        &number(conversation.output_tokens as i64),
                    ],
                ),
                &mut rows,
            );
        }

        // 当前上下文的规模**估算**：发出去之前给用户一个量级。
        // 措辞一律是「估算」——`duck_ai::tokens` 是启发式，偏差 20%~40% 是常态，
        // 说成精确值会误导人（真值只能以服务端回的 usage 为准）。
        push_pair(
            "上下文估算",
            trf(
                cx,
                "约 {} tokens",
                &[&number(duck_ai::estimate_conversation(
                    Some(self.settings.system_prompt.as_str()),
                    &self.messages,
                ) as i64)],
            ),
            &mut rows,
        );

        if let Some(streaming) = self.streaming.as_ref() {
            push_pair(
                "本轮用量",
                trf(
                    cx,
                    "输入 {} / 输出 {}",
                    &[
                        &number(i64::from(streaming.usage.input_tokens)),
                        &number(i64::from(streaming.usage.output_tokens)),
                    ],
                ),
                &mut rows,
            );
            push_pair(
                "状态",
                if streaming.started {
                    tr(cx, "流式接收中").to_owned()
                } else {
                    tr(cx, "连接中").to_owned()
                },
                &mut rows,
            );
        }

        // 工具清单**从注册表实际内容渲染**，不写死一份愿望清单：
        // 写了「只读 SQL」但注册表里没有，就是在界面里摆演示项（P3-6 明令禁止）。
        let tool_rows: Vec<AnyElement> = self
            .tools
            .specs()
            .into_iter()
            .map(|spec| {
                let needs_approval = spec.risk.needs_approval();
                h_flex()
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(accent_tag(SharedString::from(spec.name.clone()), cx))
                    .child(tag(
                        SharedString::from(if needs_approval {
                            tr(cx, "需确认")
                        } else {
                            tr(cx, "只读")
                        }),
                        if needs_approval {
                            Tone::Warning
                        } else {
                            Tone::Success
                        },
                        cx,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(extended.colors.text_tertiary)
                            .child(selectable_text(SharedString::from(tool_label(&spec.name, cx)))),
                    )
                    .into_any_element()
            })
            .collect();

        let mut tool_card = v_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.md);
        if self.tools.is_empty() {
            tool_card = tool_card.child(meta_text(tr(cx, "工具未开启"), cx)).child(
                button(
                    "assistant-open-tool-settings",
                    tr(cx, "去设置"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(|_, _window, cx| {
                    crate::pages::open_settings_at("ai", None, cx);
                }),
            );
        } else {
            tool_card = tool_card.children(tool_rows);
        }
        if let Some(workspace) = self.settings.workspace_dir.as_ref() {
            tool_card = tool_card.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .text_color(extended.colors.text_tertiary)
                    .child(selectable_text(SharedString::from(trf(
                        cx,
                        "工作目录：{}",
                        &[workspace.as_str()],
                    )))),
            );
        }

        // 工具一多右栏就会超过窗口高度，所以从一开始就带 `.id()`：
        // gpui 的 `overflow_y_scroll` 挂在 `StatefulInteractiveElement` 上，
        // 先建成裸 `Div` 再补 `.id()` 会把类型换成 `Stateful<Div>`，赋不回原变量。
        v_flex()
            .id("assistant-right-sidebar")
            .w_full()
            .h_full()
            .min_h_0()
            .overflow_y_scroll()
            .gap(tokens.spacing.md)
            .p(tokens.spacing.md)
            .child(detail_title_text(tr(cx, "会话参数"), cx))
            .child(
                card(cx).child(
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .p(tokens.spacing.md)
                        .children(rows),
                ),
            )
            .child(detail_title_text(tr(cx, "工具能力"), cx))
            .child(card(cx).child(tool_card))
            .child(
                meta_text(
                    tr(
                        cx,
                        "厂商、密钥、工具开关与知识库来源都在「设置 → AI」里调整。",
                    ),
                    cx,
                )
                .text_color(extended.colors.text_tertiary),
            )
            .into_any_element()
    }
}

/// 工具名 → 中文副标题。
///
/// **工具名本身不翻译**（它是模型认识的标识符，翻了对不上号），
/// 这一行只是给用户一个能一眼看懂「这个工具是干什么的」的注解。
fn tool_label(name: &str, cx: &App) -> String {
    match name {
        "read_file" => tr(cx, "读取工作目录里的文本文件").to_owned(),
        "list_dir" => tr(cx, "列出目录内容").to_owned(),
        "search_kb" => tr(cx, "在本地知识库里检索").to_owned(),
        other => other.to_owned(),
    }
}

impl Render for AssistantPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 设置页改完配置（含「只补了一个 API key」）之后回到本页，靠版本号发现并重读。
        // 只比一个整数：每帧解析一遍档案 JSON 只是为了做相等判断，不划算。
        if ai::revision(cx) != self.ai_revision {
            self.reload_config(cx);
        }

        let content = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.render_transcript(cx)),
            )
            .child(self.render_composer(cx));

        let mut panel = SidebarPanel::new("assistant-content", &self.sidebar, content, window, cx);
        if panel.is_sidebar_visible() {
            panel = panel.sidebar(self.render_sidebar(cx));
        }
        div().size_full().min_h_0().min_w_0().child(panel)
    }
}

/// 吃一条内核事件，更新流式状态；返回 `Some(错误文案)` 表示这一轮就此失败。
///
/// 抽成自由函数是为了让 [`AssistantPage::drain_stream`] 里那个循环保持扁平——
/// 它已经在处理「收尾 / 空 / 断连」三种接收结果，再塞进十几个事件分支会很难读。
fn absorb_agent_event(streaming: &mut Streaming, event: AgentEvent) -> Option<String> {
    match event {
        AgentEvent::Stream(inner) => match inner {
            AiEvent::Started => streaming.started = true,
            AiEvent::TextDelta(chunk) => streaming.text.push_str(&chunk),
            AiEvent::ThinkingDelta(chunk) => streaming.thinking.push_str(&chunk),
            // 完整工具调用列表在这里用不上：逐个的 `ToolStarted` 才是渲染要的粒度。
            AiEvent::ToolCalls(_) => {}
            AiEvent::Finished { stop_reason, usage } => {
                streaming.stop_reason = stop_reason;
                // 累加而不是覆盖：多轮的话只留最后一轮，界面上就成了「第二轮花了多少」。
                streaming.usage = Usage {
                    input_tokens: streaming
                        .usage
                        .input_tokens
                        .saturating_add(usage.input_tokens),
                    output_tokens: streaming
                        .usage
                        .output_tokens
                        .saturating_add(usage.output_tokens),
                    total_tokens: streaming
                        .usage
                        .total_tokens
                        .saturating_add(usage.total_tokens),
                };
            }
            AiEvent::Failed(message) => return Some(message),
        },
        AgentEvent::ToolStarted { call, .. } => {
            streaming.tool_runs.push(ToolRun {
                call_id: call.id.clone(),
                name: call.name.clone(),
                summary: ai_tools::call_summary(&call),
                state: ToolState::Running,
                ok: false,
                preview: String::new(),
                detail: None,
            });
        }
        AgentEvent::ToolFinished {
            call,
            ok,
            preview,
            detail,
            ..
        } => {
            if let Some(run) = find_tool_run(streaming, &call.id) {
                run.state = ToolState::Finished;
                run.ok = ok;
                run.preview = preview;
                run.detail = detail;
            }
        }
        AgentEvent::ToolDenied { call, reason, .. } => {
            if let Some(run) = find_tool_run(streaming, &call.id) {
                run.state = ToolState::Denied;
                run.ok = false;
                run.preview = reason;
            }
        }
        AgentEvent::RoundCompleted { messages, .. } => {
            // 本轮定稿：搬进 `live`，并把当前轮的流式缓冲与工具卡**清掉**——
            // 它们已经在 `messages` 里了，留着就会渲染两份。
            streaming.live.extend(messages);
            streaming.text.clear();
            streaming.thinking.clear();
            streaming.tool_runs.clear();
        }
        AgentEvent::RoundsExhausted { max_rounds } => {
            streaming.rounds_exhausted = Some(max_rounds);
        }
    }
    None
}

/// 找最近一个同 id 的工具运行记录。
///
/// 倒序找而不是正序：同一个 `call_id` 不可能重复，但**倒序天然拿到最新的那条**，
/// 万一某个网关发了重复 id，界面也只会更新最近那张卡。
fn find_tool_run<'a>(streaming: &'a mut Streaming, call_id: &str) -> Option<&'a mut ToolRun> {
    streaming
        .tool_runs
        .iter_mut()
        .rev()
        .find(|run| run.call_id == call_id)
}

/// 从累积缓冲拼一条助手消息（取消 / 断流时用）。
fn partial_from_buffers(streaming: &Streaming) -> Option<ChatMessage> {
    if streaming.text.is_empty() && streaming.thinking.is_empty() {
        return None;
    }
    let mut message = ChatMessage::assistant(streaming.text.clone());
    if !streaming.thinking.is_empty() {
        message.blocks.insert(
            0,
            duck_ai::ContentBlock::Thinking {
                text: streaming.thinking.clone(),
            },
        );
    }
    Some(message)
}

/// 把一串消息摊成渲染行。
///
/// 工具结果**不单独成行**——它被折进对应 `ToolUse` 的卡片里，所以先扫一遍结果表，
/// 再按顺序出行。这样「助手说了一句 → 调了两个工具 → 工具结果是并排两张卡」
/// 而不是「一张卡 + 后面孤零零两条结果」。
fn push_message_rows(
    messages: &[ChatMessage],
    prefix: &str,
    actions: MessageActions,
    rows: &mut Vec<Row>,
) {
    let mut results: Vec<(&str, &str, bool)> = Vec::new();
    for message in messages {
        for block in &message.blocks {
            if let duck_ai::ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } = block
            {
                results.push((call_id.as_str(), content.as_str(), *is_error));
            }
        }
    }

    // 「重新生成」要落在**最后一个真有气泡的**助手行上：末条助手消息可能只有工具调用、
    // 没有正文（被取消 / 撞上轮次上限），那一行压根不出行，按钮就没了落点。
    let mut last_assistant_row: Option<(usize, usize)> = None;

    for (index, message) in messages.iter().enumerate() {
        match message.role {
            Role::User => rows.push(Row {
                id: SharedString::from(format!("{prefix}-{index}")),
                action: (actions.last_user == Some(index)).then_some(RowAction::Edit(index)),
                kind: RowKind::User {
                    text: message.text(),
                },
            }),
            Role::Assistant => {
                let text = message.text();
                let thinking = message.thinking();
                if !(text.is_empty() && thinking.is_none()) {
                    last_assistant_row = Some((rows.len(), index));
                    rows.push(Row {
                        id: SharedString::from(format!("{prefix}-{index}")),
                        action: None,
                        kind: RowKind::Assistant { text, thinking },
                    });
                }
                for call in message.tool_calls() {
                    let result = results
                        .iter()
                        .find(|(call_id, ..)| *call_id == call.id)
                        .map(|(_, content, is_error)| ToolResultView {
                            ok: !*is_error,
                            content: (*content).to_owned(),
                        });
                    rows.push(Row {
                        id: SharedString::from(format!("{prefix}-tool-{}", call.id)),
                        action: None,
                        kind: RowKind::Tool(Box::new(ToolCard {
                            name: call.name.clone(),
                            summary: ai_tools::call_summary(call),
                            // 历史回看时状态只能从结果反推：结果没了就是没跑成。
                            state: if result.is_some() {
                                ToolState::Finished
                            } else {
                                ToolState::Running
                            },
                            result,
                            detail: None,
                        })),
                    });
                }
            }
            // 工具结果已经被折进上面的卡片；系统消息是配置项，不该出现在对话里。
            Role::Tool | Role::System => {}
        }
    }

    // `actions.last_assistant` 有值 = 这一趟是「可操作的历史」（正在跑的那一轮传的是
    // `MessageActions::default()`），此时才补按钮。
    if actions.last_assistant.is_some()
        && let Some((row_index, message_index)) = last_assistant_row
    {
        rows[row_index].action = Some(RowAction::Regenerate(message_index));
    }
}

/// 渲染消息流的一行。
///
/// 自由函数而不是方法：它要能挪进 `MessageScroller` 的 `'static` 闭包，
/// 因此只依赖传进来的 token 快照与 `cx`，不碰 `self`。
fn render_row(
    row: &Row,
    theme: &DuckThemeState,
    view: &WeakEntity<AssistantPage>,
    cx: &App,
) -> AnyElement {
    let body = render_row_body(row, theme, cx);
    // 一条消息可能只有行内动作、只有「复制」、或者两者都有 ——
    // 都没有时不画那条操作栏，免得每行底下都空留一段高度。
    if row.action.is_none() && copyable_text(row).is_none() {
        return body;
    }
    v_flex()
        .w_full()
        .min_w_0()
        .gap(theme.tokens().spacing.xxs)
        .child(body)
        .child(render_row_actions(row, theme, view, cx))
        .into_any_element()
}

/// 一行的正文里**能复制走**的那部分；没有就不给复制按钮。
///
/// 只认用户气泡与助手气泡 —— 这两类才是「消息」。工具卡是结构化产物（有状态标签、
/// 有表格），复制成一坨等宽文本只会把结构丢掉；错误行是诊断提示，复制它没有去处。
/// 流式刚开始、正文还没到时的空占位同理不给（复制到一个空串是坏行为）。
fn copyable_text(row: &Row) -> Option<String> {
    match &row.kind {
        RowKind::User { text } | RowKind::Assistant { text, .. } => {
            (!text.is_empty()).then(|| text.clone())
        }
        RowKind::Tool(_) | RowKind::Error { .. } => None,
    }
}

/// 渲染一行消息的正文（气泡 / 卡片）。
fn render_row_body(row: &Row, theme: &DuckThemeState, cx: &App) -> AnyElement {
    let tokens = theme.tokens();
    let extended = theme.extended();
    match &row.kind {
        RowKind::User { text } => Bubble::new()
            .with_variant(BubbleVariant::Tinted)
            .alignment(MessageAlignment::End)
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        RowKind::Assistant { text, thinking } => {
            let mut column = v_flex()
                .w_full()
                .min_w_0()
                .gap(tokens.spacing.xxs + tokens.spacing.xxs);
            if let Some(thinking) = thinking {
                // `ElementId` 没有 `(SharedString, &str)` 的 `From`（只有
                // `(&'static str, EntityId/usize/u64/u32)`），所以 id 自己拼。
                column = column.child(accordion(
                    SharedString::from(format!("{}-think", row.id)),
                    [(
                        SharedString::from(tr(cx, "思考过程")),
                        div()
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(extended.colors.text_tertiary)
                            .child(SharedString::from(thinking.clone()))
                            .into_any_element(),
                        false,
                    )],
                    cx,
                ));
            }
            let body = if text.is_empty() {
                // 流式刚开始、正文还没到：给一个占位，否则用户看到的是一个空气泡。
                div()
                    .text_size(tokens.typography.sm.size)
                    .text_color(extended.colors.text_tertiary)
                    .child(SharedString::from(tr(cx, "…")))
                    .into_any_element()
            } else {
                // 用 `TextView::markdown(id, …)` 而不是便利函数 `markdown(…)`：
                // 后者把 `ElementId::CodeLocation` 当 id，**同一处代码渲染的每一行
                // 都会撞到同一个 id**，滚动位置与选中态会互相串。这里按行给唯一 id。
                TextView::markdown(
                    SharedString::from(format!("{}-md", row.id)),
                    SharedString::from(text.clone()),
                )
                .selectable(true)
                .into_any_element()
            };
            column
                .child(
                    Bubble::new()
                        .with_variant(BubbleVariant::Ghost)
                        .alignment(MessageAlignment::Start)
                        .child(body),
                )
                .into_any_element()
        }
        RowKind::Error { text } => Bubble::new()
            .with_variant(BubbleVariant::Destructive)
            .alignment(MessageAlignment::Start)
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        RowKind::Tool(card_data) => render_tool_card(card_data, theme, cx),
    }
}

/// 渲染一行底部的次要操作条：能复制就有「复制」按钮，另有行内动作时并排。
///
/// 都走 `Ghost` + 小图标 + 文字：它们是**次要操作**，视觉上不该和
/// 发送 / 停止争注意力；但也不做「悬停才出现」——悬停态要额外存一份 hover 状态，
/// 而消息行现在是纯快照，为它引一套交互状态不划算。
fn render_row_actions(
    row: &Row,
    theme: &DuckThemeState,
    view: &WeakEntity<AssistantPage>,
    cx: &App,
) -> AnyElement {
    let tokens = theme.tokens();
    let extended = theme.extended();
    let icon_color = tokens.colors.muted_foreground;

    let mut bar = h_flex().w_full().gap(tokens.spacing.xxs);
    // 用户消息靠右，助手消息靠左：按钮跟着自己那条气泡那一侧对齐。
    if matches!(row.kind, RowKind::User { .. }) {
        bar = bar.justify_end();
    }

    if let Some(action) = row.action {
        // `ElementId` 由行 id 派生：同一时刻最多只有一条消息带操作，但按领域 id 拼
        // 是仓库约定（AGENTS §4），也免得将来放开「每行都给」时撞 id。
        let (id, label, icon) = match action {
            RowAction::Regenerate(_) => ("regenerate", tr(cx, "重新生成"), DuckIcon::RotateCw),
            RowAction::Edit(_) => ("edit", tr(cx, "编辑重发"), DuckIcon::Pencil),
            RowAction::Retry => ("retry", tr(cx, "重试"), DuckIcon::RotateCw),
        };

        let button = icon_text_button(
            SharedString::from(format!("{}-{id}", row.id)),
            label,
            Icon::new(icon)
                .size(extended.icon.sm)
                .text_color(icon_color),
            ButtonVariant::Ghost,
            cx,
        );
        let button = match action {
            RowAction::Regenerate(index) => button.on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.regenerate(index, cx));
                }
            }),
            RowAction::Edit(index) => button.on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.begin_edit(index, window, cx));
                }
            }),
            RowAction::Retry => button.on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.retry(cx));
                }
            }),
        };
        bar = bar.child(button);
    }

    if let Some(text) = copyable_text(row) {
        // 正文整段进系统剪贴板（而不是只复制选中部分）：重发到别处、
        // 贴进工单，要的通常就是这一条消息的全文。
        bar = bar.child(
            icon_text_button(
                SharedString::from(format!("{}-copy", row.id)),
                tr(cx, "复制"),
                Icon::new(DuckIcon::Copy)
                    .size(extended.icon.sm)
                    .text_color(icon_color),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click(move |_, _window, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
            }),
        );
    }

    bar.into_any_element()
}

/// 渲染一张工具调用卡。
///
/// 三行结构：**标题行**（工具名 + 状态标签 + 入参摘要）、**正文**（结果预览）、
/// **产物表格**（如果有）。表格是刻意的：`list_dir` / `search_kb` 回给模型的是
/// 一段文本，而同一份内容做成表格给用户看，才不至于把卡片变成一坨等宽字。
fn render_tool_card(data: &ToolCard, theme: &DuckThemeState, cx: &App) -> AnyElement {
    let tokens = theme.tokens();
    let extended = theme.extended();

    let (state_text, tone) = match data.state {
        ToolState::Running => (tr(cx, "进行中"), Tone::Info),
        ToolState::Denied => (tr(cx, "已拒绝"), Tone::Warning),
        ToolState::Finished => match &data.result {
            Some(result) if result.ok => (tr(cx, "完成"), Tone::Success),
            _ => (tr(cx, "失败"), Tone::Error),
        },
    };

    let mut header = h_flex()
        .w_full()
        .min_w_0()
        .items_center()
        .gap(tokens.spacing.xs)
        // 工具名**不翻译**：它是模型认识的标识符，翻成中文反而对不上号。
        .child(accent_tag(SharedString::from(data.name.clone()), cx))
        .child(tag(SharedString::from(state_text), tone, cx));
    if !data.summary.is_empty() {
        header = header.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(extended.colors.text_tertiary)
                .child(SharedString::from(data.summary.clone())),
        );
    }

    let mut body = v_flex()
        .w_full()
        .min_w_0()
        .gap(tokens.spacing.xs)
        .child(header);
    if let Some(result) = &data.result
        && !result.content.trim().is_empty()
    {
        body = body.child(
            div()
                .w_full()
                .min_w_0()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if result.ok {
                    tokens.colors.foreground
                } else {
                    tokens.colors.destructive
                })
                .font_family(MONO_FAMILY)
                .child(SharedString::from(result.content.clone())),
        );
    }
    match &data.detail {
        Some(ToolDetail::Table { rows, .. }) => {
            body = body.child(render_detail_table(rows, theme, cx));
        }
        Some(ToolDetail::Handoff { label, body: text }) => {
            body = body
                .child(
                    div()
                        .text_size(tokens.typography.xs.size)
                        .text_color(extended.colors.text_tertiary)
                        .child(SharedString::from(label.clone())),
                )
                .child(
                    div()
                        .font_family(MONO_FAMILY)
                        .text_size(tokens.typography.xs.size)
                        .child(SharedString::from(text.clone())),
                );
        }
        None => {}
    }

    card(cx)
        .w_full()
        .min_w_0()
        .p(tokens.spacing.sm)
        .child(body)
        .into_any_element()
}

/// 产物表格。
///
/// **不复用 `content::DataTable`**：那套是为「可排序、可选中、可翻页」的数据工作台
/// 设计的，卡片里塞一个虚拟列表会让每张工具卡都背一份滚动状态。
/// 这里只是一段两列的对齐文本，等宽字体 + 截断就够了。
fn render_detail_table(rows: &[Vec<String>], theme: &DuckThemeState, cx: &App) -> AnyElement {
    let tokens = theme.tokens();
    let extended = theme.extended();
    // 卡片里最多列 6 行：再多就把卡片撑得比对话还长，而正文预览里已经有一份更完整的。
    const MAX_VISIBLE: usize = 6;
    let mut list = v_flex().w_full().min_w_0().gap(tokens.spacing.xxs);
    for row in rows.iter().take(MAX_VISIBLE) {
        let mut line = h_flex().w_full().min_w_0().gap(tokens.spacing.sm);
        for (index, cell) in row.iter().enumerate() {
            let first = index == 0;
            let mut entry = div()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if first {
                    extended.colors.text_tertiary
                } else {
                    tokens.colors.foreground
                })
                .child(SharedString::from(cell.clone()));
            // 第一列是「出处 / 名称」，给个固定上限免得把说明挤没；其余列吃掉剩余宽度。
            entry = if first {
                entry.max_w(px(180.))
            } else {
                entry.flex_1()
            };
            line = line.child(entry);
        }
        list = list.child(line);
    }
    if rows.len() > MAX_VISIBLE {
        list = list.child(
            div()
                .text_size(tokens.typography.xs.size)
                .text_color(extended.colors.text_tertiary)
                .child(SharedString::from(trf(
                    cx,
                    "另有 {} 行",
                    &[&rows.len().saturating_sub(MAX_VISIBLE).to_string()],
                ))),
        );
    }
    list.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行内操作只落在「最后一条用户 / 助手消息」上。
    #[test]
    fn actions_land_on_the_tail_rows() {
        let messages = vec![
            ChatMessage::user("q1"),
            ChatMessage::assistant("a1"),
            ChatMessage::user("q2"),
            ChatMessage::assistant("a2"),
        ];
        let actions = MessageActions {
            last_assistant: Some(3),
            last_user: Some(2),
        };
        let mut rows = Vec::new();
        push_message_rows(&messages, "h", actions, &mut rows);

        assert_eq!(rows.len(), 4);
        assert!(rows[0].action.is_none());
        assert!(rows[1].action.is_none());
        assert!(matches!(rows[2].action, Some(RowAction::Edit(2))));
        assert!(matches!(rows[3].action, Some(RowAction::Regenerate(3))));
    }

    /// 末条助手消息只有工具调用、没有正文时那一行不出气泡：
    /// 「重新生成」要退到最后一个真有气泡的助手行上，而不是凭空消失。
    #[test]
    fn regenerate_falls_back_to_the_last_visible_bubble() {
        let messages = vec![
            ChatMessage::user("q"),
            ChatMessage::assistant("a"),
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![duck_ai::ContentBlock::ToolUse {
                    call: duck_ai::ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        arguments: "{}".into(),
                    },
                }],
            },
        ];
        let actions = MessageActions {
            last_assistant: Some(2),
            last_user: Some(0),
        };
        let mut rows = Vec::new();
        push_message_rows(&messages, "h", actions, &mut rows);

        // user + assistant + tool 卡片 = 3 行。
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].action, Some(RowAction::Edit(0))));
        assert!(matches!(rows[1].action, Some(RowAction::Regenerate(1))));
        assert!(rows[2].action.is_none());
    }

    /// 正在跑的那一轮（`MessageActions::default()`）一律不给行内操作。
    #[test]
    fn live_rows_carry_no_actions() {
        let messages = vec![ChatMessage::user("q"), ChatMessage::assistant("a")];
        let mut rows = Vec::new();
        push_message_rows(&messages, "l", MessageActions::default(), &mut rows);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.action.is_none()));
    }

    /// 「复制」跟着气泡走：用户 / 助手消息都有，工具卡与错误行都没有。
    #[test]
    fn every_message_bubble_is_copyable() {
        // 中间两条**没有**行内动作（不是最后一条），照样要给「复制」。
        let messages = vec![
            ChatMessage::user("q1"),
            ChatMessage::assistant("a1"),
            ChatMessage::user("q2"),
            ChatMessage::assistant("a2"),
        ];
        let actions = MessageActions {
            last_assistant: Some(3),
            last_user: Some(2),
        };
        let mut rows = Vec::new();
        push_message_rows(&messages, "h", actions, &mut rows);

        assert_eq!(rows.len(), 4);
        // 行内动作只有尾部两条有，但四条**全都能复制**。
        assert!(rows[0].action.is_none());
        assert_eq!(copyable_text(&rows[0]).as_deref(), Some("q1"));
        assert_eq!(copyable_text(&rows[1]).as_deref(), Some("a1"));
        assert_eq!(copyable_text(&rows[2]).as_deref(), Some("q2"));
        assert_eq!(copyable_text(&rows[3]).as_deref(), Some("a2"));

        // 流式占位（正文还没到）不给复制：复制到一个空串比没有按钮更糟。
        let empty = Row {
            id: "x".into(),
            kind: RowKind::Assistant {
                text: String::new(),
                thinking: None,
            },
            action: None,
        };
        assert_eq!(copyable_text(&empty), None);

        // 工具卡与错误行不给。
        let tool = Row {
            id: "t".into(),
            kind: RowKind::Tool(Box::new(ToolCard {
                name: "read_file".into(),
                summary: "{}".into(),
                state: ToolState::Finished,
                result: None,
                detail: None,
            })),
            action: None,
        };
        assert_eq!(copyable_text(&tool), None);
        let error = Row {
            id: "e".into(),
            kind: RowKind::Error {
                text: "boom".into(),
            },
            action: None,
        };
        assert_eq!(copyable_text(&error), None);
    }
}
