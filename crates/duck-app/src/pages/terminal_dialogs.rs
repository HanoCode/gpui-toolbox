//! 终端页的两个弹窗：**新建连接**与**打开目录**。
//!
//! 沿用本仓既有弹窗范式（`pages/plugin_center.rs` / `pages/status_support.rs`），
//! 但公共部分交给 `duck_components` 的弹窗原语，调用点只留业务：
//!
//! 1. 表单状态放**实体**里 —— `Dialog::content` 的闭包是 `Fn`，会被反复调用
//!    （重排 / 重绘），局部变量活不过一轮。
//! 2. 确认回调用 `Rc<dyn Fn>` 传递 —— 外层 dialog 闭包同样是 `Fn`，
//!    泛型闭包参数无法被它「克隆进内层 `move` 闭包」，只能先装进 `Rc`。
//! 3. 可见按钮必须显式 `.footer(...)` —— `button_props` 只接键盘 ⏎ / Esc，
//!    不挂 footer 的弹窗只有一个光秃秃的标题栏 ✕。这一条已收进
//!    [`duck_components::dialog_footer`]，调用点不再重复踩。
//! 4. 几何走 [`duck_components::modal_drag`] 四件套：标题行自管（不用
//!    `Dialog::title`），内容包一层 [`duck_components::modal_drag_area`] 由尾部量尺
//!    在 prepaint 阶段实测高度，[`duck_components::modal_offset`] 每帧据此把
//!    「垂直居中」的补差写回 `Dialog` 的 `.ml/.margin_top`（上游 `Positioner`
//!    不给位置 API，margin 是唯一入口）。**弹窗恒居中、标题行不可拖**——
//!    详见 `duck-components/src/dialog.rs` 模块头。
//!
//! 尺寸一律走 token：控件高度走 `ControlExt::control`（= `density.control`，
//! 随「界面密度 / 界面缩放」两轴走），文字走 `field_label` / `field_hint` /
//! `field_error` / `body_text`，间距走 `spacing.*`。**不要**在这里写裸 `px(字号)`，
//! 也不要用 gpui-component 的 `Size::Small`：它的输入框高度是 `h_6()`（≈19.5px），
//! 比本仓控件基线（`density.control`，默认档 28px）矮一档，既不随密度走，
//! 也和同一行里的按钮对不齐。
//!
//! 两个弹窗都只负责「收集 + 校验」：确认后把结构化结果交回调用方，
//! 弹窗自身不碰连接表、不碰目录状态。

use std::path::{Path, PathBuf};
use std::rc::Rc;

use duck_components::{
    ButtonVariant, ConfirmHandler, ControlExt as _, DuckIcon, FooterAction, ModalDrag, body_text,
    button, dialog_footer, dialog_footer_with_action, dialog_top_offset, dropdown_select,
    field_error, field_hint, field_label, form_field, form_gap, form_row, icon_button,
    input_with_action, modal_drag, modal_drag_area, modal_offset, modal_title_row, switch_control,
};
use duck_theme::{active_theme, tf, tr};
use gpui::{
    AnyElement, App, AppContext as _, Context, Div, Entity, Hsla, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon, WindowExt as _,
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};

use crate::pages::terminal_data::{self, AuthKind, Side};

/// 新建连接弹窗宽度：字段里有「主机 + 端口」这类并排行，560 是能放下两列的最小档。
/// 弹窗宽度由**决策规模**决定（短确认 < 表单 < 独立页面），不为了留白而放大。
const CONNECTION_DIALOG_W: f32 = 560.;
/// 打开目录弹窗宽度（与新建连接同宽，避免同类弹窗宽窄不一）。
const DIRECTORY_DIALOG_W: f32 = 560.;
/// 条目命名（新建目录 / 重命名）弹窗宽度：只有一个输入框，比表单窄一档。
const NAME_DIALOG_W: f32 = 420.;

/// 分组下拉里「自定义分组…」这一项的哨兵值。
///
/// 用 `\0` 开头：**用户不可能通过文本输入敲出来**（输入框不接受 NUL），
/// 所以它永远不会和真实分组名（`StoredProfile.group`）撞车 ——
/// 换成 `"__custom__"` 这类可打印字符串就会有「用户真把分组叫这个名字」的歧义。
const CUSTOM_GROUP_ID: &str = "\u{0}custom";

/// 新建连接的确认回调：装进 `Rc` 是必须的——外层 dialog 闭包是 `Fn`，
/// 泛型闭包参数没法被它「克隆进内层 `move` 闭包」，只能先变成可克隆的句柄。
type CreateHandler = Rc<dyn Fn(NewConnection, &mut Window, &mut App)>;

/// 打开目录的确认回调（同上）。
type OpenHandler = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// 条目命名（新建目录 / 重命名）的确认回调（同上）。
type NameHandler = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// 新建 / 编辑连接的回填结果。
///
/// 同一个结构承载两种意图：`id` 由调用方掌握（编辑时**绝不改 id** ——
/// 它是会话标签、SFTP 面板、凭据键名共同的锚点，换 id 等于把连接拆散重建）。
#[derive(Clone)]
pub(crate) struct NewConnection {
    /// 连接名称。
    pub(crate) label: String,
    /// 主机地址。
    pub(crate) host: String,
    /// 端口。
    pub(crate) port: String,
    /// 用户名。
    pub(crate) user: String,
    /// 认证方式。
    pub(crate) auth: AuthKind,
    /// 公钥文件路径（认证方式为公钥时有意义）。
    pub(crate) key_file: String,
    /// 口令（认证方式为密码时有意义）。
    ///
    /// **只在这一跳存在**：弹窗 → 确认回调 → 加密进连接档案（`duck_platform::vault`）
    /// 或丢弃，绝不以明文落盘。编辑时**解密回显**已存口令（掩码显示）——
    /// 回显要求「应用能无提示地读回」，这正是口令走本机加密保管箱的原因；
    /// 空值 = 用户清掉了口令（保存后不再自动登录）。
    pub(crate) password: String,
    /// 落点分组名（`StoredProfile.group` 的原文）。
    ///
    /// 内置三档是「生产环境 / 测试环境 / 本地」的 id（`prod` / `staging` / `local`）；
    /// 自定义分组**名字即 id** —— 分组名是用户数据、不翻译，也不做 id↔名字两层映射。
    pub(crate) group: String,
    /// 口令是否加密写入档案（认证方式为密码时有意义）。
    pub(crate) remember: bool,
    /// 保存后是否立即打开会话。
    pub(crate) open_now: bool,
    /// 端口转发（`-L` / `-R` / `-D`），按用户添加的顺序。
    pub(crate) forwards: Vec<ForwardDraft>,
    /// 是否把本机 ssh-agent 转发给远端（`ForwardAgent yes`）。
    pub(crate) agent_forwarding: bool,
}

/// 弹窗里的一行端口转发（表单草稿）。
///
/// 端口一律**存字符串**：输入框里的中间态（空、正在输入）没有对应的数字，
/// 存 `u16` 就得在每一帧猜一个默认值，「清空重输」会变成「悄悄回填上一次的值」。
/// 解析只在两处发生 —— [`forward_from_row`] 与 `terminal_data::forward_from_stored`
/// —— 两处同源，不会各写一套判据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForwardDraft {
    /// 方向。
    pub(crate) kind: duck_ssh::ForwardKind,
    /// 绑定地址（留空用 ssh 默认）。
    pub(crate) bind: String,
    /// 绑定 / SOCKS 端口。
    pub(crate) bind_port: String,
    /// 目标地址（动态转发不用）。
    pub(crate) host: String,
    /// 目标端口（动态转发不用）。
    pub(crate) host_port: String,
}

/// 转发方向的显示文案（i18n 源串）。
pub(crate) const fn forward_kind_label(kind: duck_ssh::ForwardKind) -> &'static str {
    match kind {
        duck_ssh::ForwardKind::Local => "本地转发",
        duck_ssh::ForwardKind::Remote => "远程转发",
        duck_ssh::ForwardKind::Dynamic => "动态 SOCKS",
    }
}

impl ForwardDraft {
    /// 由持久化形态回填表单（方向认不出时返回 `None`，这一条就不出现在弹窗里）。
    pub(crate) fn from_stored(stored: &terminal_data::StoredForward) -> Option<Self> {
        let kind = duck_ssh::ForwardKind::from_value(&stored.kind)?;
        Some(Self {
            kind,
            bind: stored.bind.clone(),
            bind_port: stored.bind_port.clone(),
            host: stored.host.clone(),
            host_port: stored.host_port.clone(),
        })
    }

    /// 落成持久化形态（端口存字符串，坏值不会拖垮整份档案）。
    pub(crate) fn to_stored(&self) -> terminal_data::StoredForward {
        terminal_data::StoredForward {
            kind: self.kind.value().to_owned(),
            bind: self.bind.trim().to_owned(),
            bind_port: self.bind_port.trim().to_owned(),
            host: self.host.trim().to_owned(),
            host_port: self.host_port.trim().to_owned(),
        }
    }
}

/// 把一行表单草稿解析成内核的转发配置（与档案还原同一条判据）。
fn forward_from_row(draft: &ForwardDraft) -> Option<duck_ssh::PortForward> {
    terminal_data::forward_from_stored(&terminal_data::StoredForward {
        kind: draft.kind.value().to_owned(),
        bind: draft.bind.clone(),
        bind_port: draft.bind_port.clone(),
        host: draft.host.clone(),
        host_port: draft.host_port.clone(),
    })
}

/// 弹窗里一行转发对应的输入框集合。
///
/// 每行有自己的 `id`：重复元素必须有**领域派生的** `ElementId`，
/// 用列表下标当 id 会在删除中间一行后把后面所有行的焦点 / 状态串位。
struct ForwardRow {
    /// 稳定 id（自增，不复用）。
    id: u64,
    /// 方向。
    kind: duck_ssh::ForwardKind,
    /// 绑定地址。
    bind: Entity<InputState>,
    /// 绑定 / SOCKS 端口。
    bind_port: Entity<InputState>,
    /// 目标地址。
    host: Entity<InputState>,
    /// 目标端口。
    host_port: Entity<InputState>,
}

impl ForwardRow {
    /// 按草稿建一行；没有草稿（新增）时给空值。
    fn new(
        id: u64,
        seed: Option<&ForwardDraft>,
        window: &mut Window,
        cx: &mut Context<ConnectionForm>,
    ) -> Self {
        let text = |pick: fn(&ForwardDraft) -> &String| {
            seed.map_or_else(String::new, |draft| pick(draft).clone())
        };
        let bind = text(|draft| &draft.bind);
        let bind_port = text(|draft| &draft.bind_port);
        let host = text(|draft| &draft.host);
        let host_port = text(|draft| &draft.host_port);
        let make = |window: &mut Window,
                    cx: &mut Context<ConnectionForm>,
                    value: String,
                    placeholder: &'static str| {
            cx.new(|cx| {
                let state = InputState::new(window, cx).placeholder(tr(cx, placeholder));
                if value.is_empty() {
                    state
                } else {
                    state.default_value(value)
                }
            })
        };
        Self {
            id,
            kind: seed.map_or(duck_ssh::ForwardKind::Local, |draft| draft.kind),
            bind: make(window, cx, bind, "127.0.0.1"),
            bind_port: make(window, cx, bind_port, "8080"),
            host: make(window, cx, host, "127.0.0.1"),
            host_port: make(window, cx, host_port, "80"),
        }
    }

    /// 把输入框里的当前值收成草稿（确认时调用）。
    fn draft(&self, cx: &App) -> ForwardDraft {
        ForwardDraft {
            kind: self.kind,
            bind: self.bind.read(cx).value().trim().to_owned(),
            bind_port: self.bind_port.read(cx).value().trim().to_owned(),
            host: self.host.read(cx).value().trim().to_owned(),
            host_port: self.host_port.read(cx).value().trim().to_owned(),
        }
    }
}

/// 「测试连接」的结果。
///
/// 只活在**弹窗这一跳**：不落库、不进日志。失败原因存 ssh 的第一行 stderr 原文
/// （`Permission denied (publickey).` / `Connection refused` /
/// `Host key verification failed.`）——比任何「连接失败」的泛化文案都准确，
/// 这也正是 [`duck_ssh::test_connection`] 不开 PTY 的理由。
#[derive(Clone, PartialEq, Eq)]
enum TestState {
    /// 没测过，或刚改过字段（上一次的结果已作废）。
    Idle,
    /// 正在测：`ssh` 子进程在跑。
    Running,
    /// 通了：远端 `uname -srm` 的一行（远端给不出时是 `ok`）。
    Ok(String),
    /// 没通：ssh 的 stderr 原文，或本地参数校验的提示。
    Failed(String),
}

/// 新建连接表单的字段状态**兼弹窗正文视图**。
///
/// 两件事必须同时落在这里，缺一条功能就是坏的：
///
/// 1. **得是视图**：`Dialog::content` 的闭包是 `Fn`，而**子视图 `notify()` 不会把
///    宿主重画**（`db.rs::DriverPickerView` 记过这条：全仓 5 个页面都靠显式订阅
///    `InputEvent::Change` 才让列表跟着输入框变）。正文挂成独立视图后，
///    「测试连接」的后台结果才会自己刷新出来。
/// 2. **得持有订阅**：字段一改就把上一次的测试结果作废 —— 否则界面上会留着一条
///    已经不成立的「连接成功」，那是谎报。
struct ConnectionForm {
    /// 连接名称。
    name: Entity<InputState>,
    /// 主机地址。
    host: Entity<InputState>,
    /// 端口。
    port: Entity<InputState>,
    /// 用户名。
    user: Entity<InputState>,
    /// 公钥文件。
    key_file: Entity<InputState>,
    /// 口令（掩码显示；编辑时解密回填已存口令）。
    password: Entity<InputState>,
    /// 落点分组下标（指向 [`Self::groups`] 里的内置 / 已有分组）。
    group: usize,
    /// 分组下拉当前选的是「自定义分组…」（此时落点名以 [`Self::group_custom`] 为准）。
    group_custom: bool,
    /// 自定义分组名（选「自定义分组…」时出现，落点时用它的输入值）。
    group_custom_input: Entity<InputState>,
    /// 认证方式。
    auth: AuthKind,
    /// 口令是否记住。
    remember: bool,
    /// 是否把本机 ssh-agent 转发给远端。
    agent_forwarding: bool,
    /// 保存后是否立即打开。
    open_now: bool,
    /// 端口转发行（可增删）。
    forwards: Vec<ForwardRow>,
    /// 下一行的稳定 id（自增，不复用 —— 删掉再加不会撞上前一行的 ElementId）。
    next_forward_id: u64,
    /// 是否提交过：**只有提交失败后**才把空字段标红，一打开就红一片是坏体验。
    submitted: bool,
    /// 「测试连接」的结果。
    test: TestState,
    /// 可选分组（`(分组名, 显示名)`；自定义分组的 id 与显示名同一份原文）。
    /// 存进来是因为 [`Self::render`] 里够不到 `open_connection_dialog` 的闭包捕获。
    groups: Vec<(String, String)>,
    /// 弹窗标题（`新建连接` / `编辑连接`）。
    title: &'static str,
    /// 弹窗几何状态（`duck_components::modal_drag` 四件套之一）。
    ///
    /// 存在这里是因为正文已经是一个**独立视图**，`render` 够不到
    /// `open_connection_dialog` 的局部变量，而 [`modal_drag_area`] 要它。
    drag: Entity<ModalDrag>,
    /// 输入框订阅（`Change → 作废测试结果`）。**必须持有**：`Subscription` 一丢，
    /// 回调就不会再来。
    _subscriptions: Vec<Subscription>,
}

/// 打开目录弹窗的字段状态。
struct DirectoryForm {
    /// 目录路径。
    path: Entity<InputState>,
    /// 是否提交过（同上：校验提示延后到提交之后）。
    submitted: bool,
}

/// 新建 / 编辑连接弹窗。
///
/// `groups` 是 `(分组名, 分组显示名)`（内置三档在前、档案里已有的自定义分组在后）；
/// `initial` 有值时按**编辑**呈现（表单回填、标题与确认按钮改词）；`socket_dir` 是
/// 「测试连接」用的 `ControlPath` 目录（由调用方给，见 `terminal.rs::ssh_socket_dir`：
/// 与终端标签共用同一份，探活命中的才是同一条物理连接）；`on_submit` 在校验通过后被调用一次。
///
/// 两种意图共用一个弹窗是刻意的：字段、校验、键盘行为完全一样，
/// 分成两个弹窗只会让「新建能填的字段编辑时填不了」这类不一致悄悄长出来。
pub(crate) fn open_connection_dialog<F>(
    window: &mut Window,
    cx: &mut App,
    groups: Vec<(String, String)>,
    socket_dir: PathBuf,
    initial: Option<&NewConnection>,
    on_submit: F,
) where
    F: Fn(NewConnection, &mut Window, &mut App) + 'static,
{
    let editing = initial.is_some();
    // 回填值在**打开弹窗这一刻**取定：弹窗开着的期间用户去改档案也不会串味。
    let seeded = initial.cloned();
    // 标题在表单之前算好：表单自身是正文视图，`render` 里的标题行要用它。
    let title = if editing {
        tr(cx, "编辑连接")
    } else {
        tr(cx, "新建连接")
    };
    // 弹窗几何状态：关闭弹窗时随作用域丢弃，偏移不跨弹窗记忆。要在表单之前建好 ——
    // 正文视图的 `render` 要用它量高度。
    let drag = modal_drag(cx);
    let form = cx.new(|cx| {
        let text = |pick: fn(&NewConnection) -> &String| {
            seeded
                .as_ref()
                .map_or_else(String::new, |seed| pick(seed).clone())
        };
        let label = text(|seed| &seed.label);
        let host = text(|seed| &seed.host);
        let port = text(|seed| &seed.port);
        let user = text(|seed| &seed.user);
        let key_file = text(|seed| &seed.key_file);
        let password = text(|seed| &seed.password);
        let group = seeded.as_ref().map_or(0, |seed| {
            groups
                .iter()
                .position(|(id, _)| *id == seed.group)
                .unwrap_or(0)
        });

        // 先各自建好，**再**订阅：订阅要拿这些实体当源，内联在结构体字面量里就够不到。
        let name_input = cx.new(|cx| {
            let state = InputState::new(window, cx).placeholder(tr(cx, "例如 prod-03"));
            if label.is_empty() {
                state
            } else {
                state.default_value(label)
            }
        });
        let host_input = cx.new(|cx| {
            let state = InputState::new(window, cx).placeholder(tr(cx, "10.0.1.13 或主机名"));
            if host.is_empty() {
                state
            } else {
                state.default_value(host)
            }
        });
        let port_input = cx.new(|cx| {
            let value = if port.is_empty() {
                "22".to_owned()
            } else {
                port
            };
            InputState::new(window, cx).default_value(value)
        });
        let user_input = cx.new(|cx| {
            let value = if user.is_empty() {
                std::env::var("USER").unwrap_or_else(|_| "root".to_owned())
            } else {
                user
            };
            InputState::new(window, cx).default_value(value)
        });
        let key_input = cx.new(|cx| {
            let state = InputState::new(window, cx).placeholder(tr(cx, "~/.ssh/id_ed25519"));
            if key_file.is_empty() {
                state
            } else {
                state.default_value(key_file)
            }
        });
        // 口令**解密回显**：编辑已存口令的连接时这里带着旧值（掩码显示），
        // 改个端口不用把口令重输一遍；清空保存 = 不再保存口令。
        let password_input = cx.new(|cx| {
            let state = InputState::new(window, cx)
                .masked(true)
                .placeholder(tr(cx, "留空则连接时在终端里输入"));
            if password.is_empty() {
                state
            } else {
                state.default_value(password)
            }
        });

        // 自定义分组名：选「自定义分组…」时才显示。新建时留空，编辑已存的自定义
        // 分组连接时把名字回填进去（下拉里没有它才回填，避免与已有项重复输入）。
        let group_custom_value = seeded
            .as_ref()
            .filter(|seed| groups.iter().all(|(id, _)| *id != seed.group))
            .map_or_else(String::new, |seed| seed.group.clone());
        let group_custom_seeded = !group_custom_value.is_empty();
        let group_custom_input = cx.new(|cx| {
            let state = InputState::new(window, cx).placeholder(tr(cx, "输入名称"));
            if group_custom_value.is_empty() {
                state
            } else {
                state.default_value(group_custom_value)
            }
        });

        // 端口转发行：编辑时按档案回填，新建时从空表开始。
        // `id` 从 0 起自增，`next_forward_id_after` 是「下一个可用 id」。
        let seeded_forwards = seeded
            .as_ref()
            .map(|seed| seed.forwards.clone())
            .unwrap_or_default();
        let mut next_forward_id_after = 0u64;
        let forwards: Vec<ForwardRow> = seeded_forwards
            .iter()
            .map(|draft| {
                let row = ForwardRow::new(next_forward_id_after, Some(draft), window, cx);
                next_forward_id_after += 1;
                row
            })
            .collect();

        // 任何一个字段被改动，上一次的测试结果就不再成立 —— 当场作废，
        // 免得界面上留着一条已经骗人的「连接成功」。判定放在订阅里、
        // 结果落在字段上，`render` 只负责画（不在那里算）。
        let subscriptions = [
            &name_input,
            &host_input,
            &port_input,
            &user_input,
            &key_input,
            &password_input,
        ]
        .into_iter()
        .map(|input| {
            // 闭包里写不出 `Self`（闭包没有 `Self`），类型只能显式点名。
            cx.subscribe(
                input,
                |this: &mut ConnectionForm,
                 _,
                 event: &InputEvent,
                 cx: &mut Context<ConnectionForm>| {
                    if matches!(event, InputEvent::Change) && this.test != TestState::Idle {
                        this.test = TestState::Idle;
                        cx.notify();
                    }
                },
            )
        })
        .collect();

        ConnectionForm {
            name: name_input,
            host: host_input,
            port: port_input,
            user: user_input,
            key_file: key_input,
            password: password_input,
            group,
            group_custom: group_custom_seeded,
            group_custom_input,
            forwards,
            next_forward_id: next_forward_id_after,
            auth: seeded.as_ref().map_or(AuthKind::Key, |seed| seed.auth),
            remember: seeded.as_ref().is_none_or(|seed| seed.remember),
            // 缺省**不转发**：转发等于把「以你的身份认证其他机器」的能力借给远端，
            // 新建连接时不该替用户预勾。
            agent_forwarding: seeded.as_ref().is_some_and(|seed| seed.agent_forwarding),
            open_now: seeded.as_ref().is_none_or(|seed| seed.open_now),
            submitted: false,
            test: TestState::Idle,
            groups: groups.clone(),
            title,
            drag: drag.clone(),
            _subscriptions: subscriptions,
        }
    });
    let on_create: CreateHandler = Rc::new(on_submit);
    let ok_text = if editing {
        tr(cx, "保存")
    } else {
        tr(cx, "创建")
    };
    // 确认动作放函数级 `Rc`：键盘 ⏎（`on_ok`）与底部确认按钮共用同一份校验与落库。
    let confirm: ConfirmHandler = {
        let form = form.clone();
        let on_create = Rc::clone(&on_create);
        Rc::new(move |window, cx| {
            let (
                label,
                host,
                port,
                user,
                key_file,
                password,
                auth,
                remember,
                agent_forwarding,
                open_now,
            ) = {
                let state = form.read(cx);
                (
                    state.name.read(cx).value().trim().to_owned(),
                    state.host.read(cx).value().trim().to_owned(),
                    state.port.read(cx).value().trim().to_owned(),
                    state.user.read(cx).value().trim().to_owned(),
                    state.key_file.read(cx).value().trim().to_owned(),
                    state.password.read(cx).value().trim().to_owned(),
                    state.auth,
                    state.remember,
                    state.agent_forwarding,
                    state.open_now,
                )
            };
            // 落点分组：选「自定义分组…」时以输入框的名字为准（**名字即 id**），
            // 否则用下拉选中项（内置组是静态 id、已有自定义组是名字）。
            let (group_id, group_missing) = {
                let state = form.read(cx);
                if state.group_custom {
                    let name = state.group_custom_input.read(cx).value().trim().to_owned();
                    let missing = name.is_empty();
                    (name, missing)
                } else {
                    (
                        state.groups.get(state.group).map_or_else(
                            || terminal_data::GROUP_DEFS[0].0.to_owned(),
                            |(id, _)| id.clone(),
                        ),
                        false,
                    )
                }
            };
            let drafts: Vec<ForwardDraft> = form
                .read(cx)
                .forwards
                .iter()
                .map(|row| row.draft(cx))
                .collect();
            // 转发同样在本地就判：端口是 0 / 目标为空 / 填了非数字，
            // 留到起 `ssh` 只会得到一句含糊的 `Bad local forwarding specification`。
            let forwards_invalid = drafts.iter().any(|draft| {
                forward_from_row(draft).is_none_or(|forward| forward.validate().is_err())
            });
            // 公钥认证必须给路径：空路径会让 ssh 退回去试默认密钥，
            // 而用户在界面上明明选的是「公钥」——那是「设置没生效」，不是「连不上」。
            let key_missing = auth == AuthKind::Key && key_file.is_empty();
            if label.is_empty()
                || host.is_empty()
                || port.is_empty()
                || key_missing
                || group_missing
                || forwards_invalid
            {
                // 校验不通过：标记「已提交」让缺的字段亮红，弹窗保持打开。
                form.update(cx, |state, cx| {
                    state.submitted = true;
                    cx.notify();
                });
                return false;
            }
            on_create(
                NewConnection {
                    label,
                    host,
                    port,
                    user,
                    auth,
                    key_file,
                    password,
                    group: group_id,
                    remember,
                    open_now,
                    forwards: drafts,
                    agent_forwarding,
                },
                window,
                cx,
            );
            true
        })
    };
    // 打开即聚焦首字段：新建表单的第一个动作总是输入名称。
    let first = form.read(cx).name.clone();
    let form_for_content = form.clone();
    // 「测试连接」按钮要用：闭包是 `Fn`，每次重建按钮时都拿这一份。
    let socket_dir_for_footer = socket_dir;
    window.open_dialog(cx, move |dialog, _, cx| {
        let draft_form = form_for_content.clone();
        let confirm = Rc::clone(&confirm);
        let offset = modal_offset(&drag, cx);
        // `tr` 返回 `&'static str`（Copy）——不用 clone：`.clone()` 作用在 `&str` 上
        // 等于什么都没做。
        let ok_text_for_props = ok_text;

        dialog
            .w(px(CONNECTION_DIALOG_W))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(ok_text_for_props)
                    .cancel_text(tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, _cx| content.child(draft_form.clone()))
            .on_ok({
                let confirm = Rc::clone(&confirm);
                move |_, window, cx| confirm(window, cx)
            })
            .footer(dialog_footer_with_action(
                "terminal-conn",
                test_action(form_for_content.clone(), socket_dir_for_footer.clone(), cx),
                ok_text,
                ButtonVariant::Primary,
                confirm,
                cx,
            ))
    });
    first.update(cx, |state, cx| state.focus(window, cx));
}

// ── 弹窗正文 ────────────────────────────────────────────────────────────────

/// 正文视图。
///
/// 单独成视图（而不是直接写在 `Dialog::content` 的闭包里）的原因见
/// [`ConnectionForm`] 的文档：闭包是 `Fn`、且**子视图 `notify()` 到不了宿主**，
/// 「测试连接」的结果必须在视图自己身上刷出来才看得见。
///
/// 订阅（[`ConnectionForm::_subscriptions`]）落在视图上还有第二个好处：认证方式
/// 下拉、分组下拉、开关这些交互的 `cx.notify()` 从此有了明确的落点 —— 此前它们
/// 通知的是一个不在元素树里的数据实体，能不能重画全看别的控件是否恰好也刷新了。
impl Render for ConnectionForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let submitted = self.submitted;
        let auth = self.auth;
        let name_missing = self.name.read(cx).value().trim().is_empty();
        let host_missing = self.host.read(cx).value().trim().is_empty();
        let port_missing = self.port.read(cx).value().trim().is_empty();
        let key_missing = auth == AuthKind::Key && self.key_file.read(cx).value().trim().is_empty();

        // 分组下拉：值是「分组名」（= `StoredProfile.group` 原文），显示名对内置组
        // 取译文、对自定义组取原文；末尾追加「自定义分组…」哨兵项。
        //
        // 回调里的 `cx.entity()` 就是本视图 —— 通知落在元素树里的实体上，重画才生效。
        let mut group_options: Vec<(SharedString, SharedString)> = self
            .groups
            .iter()
            .map(|(id, label)| {
                (
                    SharedString::from(id.clone()),
                    SharedString::from(label.clone()),
                )
            })
            .collect();
        group_options.push((
            SharedString::from(CUSTOM_GROUP_ID),
            SharedString::from(tr(cx, "自定义分组…")),
        ));
        let current_group = if self.group_custom {
            SharedString::from(CUSTOM_GROUP_ID)
        } else {
            self.groups.get(self.group).map_or_else(
                || SharedString::from(""),
                |(id, _)| SharedString::from(id.clone()),
            )
        };

        let groups_for_group = self.groups.clone();
        let form_for_group = cx.entity();
        let group_control = dropdown_select(
            "terminal-conn-group",
            group_options,
            current_group,
            move |value, _, cx| {
                let custom = value.as_ref() == CUSTOM_GROUP_ID;
                let index = groups_for_group.iter().position(|(id, _)| value == *id);
                form_for_group.update(cx, |state, cx| {
                    state.group_custom = custom;
                    if let Some(index) = index {
                        state.group = index;
                    }
                    cx.notify();
                });
            },
            cx,
        );
        // 选「自定义分组…」时在下面补一个名字输入框：分组名就是它的身份
        //（没有 id↔名字两层），所以这里输什么，左栏就出什么分组名。
        let group_column: AnyElement = if self.group_custom {
            v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(group_control)
                .child(Input::new(&self.group_custom_input).control(cx).w_full())
                .into_any_element()
        } else {
            group_control
        };
        let group_invalid = submitted
            && self.group_custom
            && self.group_custom_input.read(cx).value().trim().is_empty();
        let group_field = form_field(
            tr(cx, "分组"),
            group_column,
            group_invalid.then(|| SharedString::from(tr(cx, "请填写分组名。"))),
            None,
            cx,
        );

        let name_field = form_field(
            tr(cx, "连接名称"),
            Input::new(&self.name).control(cx).w_full(),
            (submitted && name_missing).then(|| SharedString::from(tr(cx, "请填写连接名称。"))),
            None,
            cx,
        );
        let host_field = form_field(
            tr(cx, "主机地址"),
            Input::new(&self.host).control(cx).w_full(),
            (submitted && host_missing).then(|| SharedString::from(tr(cx, "请填写主机地址。"))),
            None,
            cx,
        );
        let port_field = form_field(
            tr(cx, "端口"),
            Input::new(&self.port).control(cx).w_full(),
            (submitted && port_missing).then(|| SharedString::from(tr(cx, "端口不能为空。"))),
            None,
            cx,
        );
        let user_field = form_field(
            tr(cx, "用户名"),
            Input::new(&self.user).control(cx).w_full(),
            None,
            None,
            cx,
        );

        // 分组：下拉项的「值」是分组名，显示名对内置组是译文、对自定义组是原文。
        let auth_options: Vec<(SharedString, SharedString)> = AuthKind::ALL
            .iter()
            .map(|(_, value, label)| {
                (
                    SharedString::from(*value),
                    SharedString::from(tr(cx, label)),
                )
            })
            .collect();
        let form_for_auth = cx.entity();
        let auth_control = dropdown_select(
            "terminal-conn-auth",
            auth_options,
            SharedString::from(auth.value()),
            move |value, _, cx| {
                form_for_auth.update(cx, |state, cx| {
                    state.auth = AuthKind::from_value(&value);
                    // 换认证方式 = 换一条认证路径，上一次的结论（多半出自另一条路）
                    // 当场作废：「公钥超时、改密码就通」这类串台最误导人。
                    state.test = TestState::Idle;
                    cx.notify();
                });
            },
            cx,
        );

        // 认证方式的展开区：公钥要选文件，密码要决定是否记住，agent 只需说明。
        let auth_detail: AnyElement = match auth {
            AuthKind::Key => {
                let key_file = self.key_file.clone();
                let browse = button(
                    "terminal-conn-browse-key",
                    tr(cx, "选择文件…"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let Some(path) = duck_platform::open_file() else {
                        return;
                    };
                    let text = path.to_string_lossy().into_owned();
                    key_file.update(cx, |state, cx| state.set_value(text, window, cx));
                });
                form_field(
                    tr(cx, "私钥文件"),
                    input_with_action(Input::new(&self.key_file).control(cx), browse, cx),
                    (submitted && key_missing)
                        .then(|| SharedString::from(tr(cx, "请选择私钥文件。"))),
                    Some(SharedString::from(tr(
                        cx,
                        "私钥只在本机读取、只用于签名，不会被发送，也不写入配置文件。",
                    ))),
                    cx,
                )
                .into_any_element()
            }
            AuthKind::Agent => {
                let tokens = active_theme(cx).tokens().clone();
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs + tokens.spacing.xxs)
                    .child(field_label(tr(cx, "密钥来源"), cx))
                    .child(field_hint(
                        SharedString::from(tr(
                            cx,
                            "使用 ssh-agent 中已加载的密钥，由 agent 完成签名。",
                        )),
                        cx,
                    ))
                    .into_any_element()
            }
            AuthKind::Password => {
                let form_for_remember = cx.entity();
                v_flex()
                    .w_full()
                    .gap(form_gap(cx))
                    .child(form_field(
                        tr(cx, "口令"),
                        // `mask_toggle` 自带「显示 / 隐藏」小眼睛：输入时看不清
                        // 自己敲了什么，是密码框最高频的补救需求。
                        Input::new(&self.password)
                            .control(cx)
                            .w_full()
                            .mask_toggle(),
                        None,
                        Some(SharedString::from(tr(
                            cx,
                            "口令加密后保存在本机数据库，编辑时回显；留空则连接时在终端里输入。",
                        ))),
                        cx,
                    ))
                    .child(switch_row(
                        "terminal-conn-remember",
                        tr(cx, "记住口令"),
                        Some(SharedString::from(tr(
                            cx,
                            "不勾选则不保存口令，每次连接时在终端里输入。",
                        ))),
                        self.remember,
                        move |value, _, cx| {
                            form_for_remember.update(cx, |state, cx| {
                                state.remember = *value;
                                cx.notify();
                            });
                        },
                        cx,
                    ))
                    .into_any_element()
            }
        };

        // agent 转发与「认证方式」**正交**（它管的是远端能不能用本机 agent，
        // 不是本机怎么向远端证明身份），所以放在认证展开区**之外** ——
        // 任何一种认证方式下这一行都在，不会「换了认证方式开关就找不到了」。
        let form_for_forward = cx.entity();
        let agent_forwarding_row = switch_row(
            "terminal-conn-agent-forward",
            tr(cx, "转发 ssh-agent"),
            Some(SharedString::from(tr(
                cx,
                "让远端主机也能使用本机 ssh-agent 中的密钥（从跳板机再连别的机器时用）。开启后远端可以以你的身份访问你能访问的主机，默认关闭。",
            ))),
            self.agent_forwarding,
            move |value, _, cx| {
                form_for_forward.update(cx, |state, cx| {
                    state.agent_forwarding = *value;
                    cx.notify();
                });
            },
            cx,
        );

        let form_for_open = cx.entity();
        let open_now_row = switch_row(
            "terminal-conn-open-now",
            tr(cx, "保存后立即打开"),
            Some(SharedString::from(tr(
                cx,
                "创建一条会话标签并连接到该主机。",
            ))),
            self.open_now,
            move |value, _, cx| {
                form_for_open.update(cx, |state, cx| {
                    state.open_now = *value;
                    cx.notify();
                });
            },
            cx,
        );

        modal_drag_area(
            &self.drag,
            v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(modal_title_row(
                    "terminal-conn-title",
                    &self.drag,
                    self.title,
                    cx,
                ))
                .child(name_field)
                .child(form_row(
                    vec![host_field.into_any_element(), port_field.into_any_element()],
                    cx,
                ))
                .child(form_row(
                    vec![
                        user_field.into_any_element(),
                        group_field.into_any_element(),
                    ],
                    cx,
                ))
                .child(form_field(tr(cx, "认证方式"), auth_control, None, None, cx))
                .child(auth_detail)
                .child(self.forwards_section(cx))
                .child(agent_forwarding_row)
                .child(open_now_row)
                .children(test_result_row(&self.test, cx)),
        )
    }
}

// ── 「测试连接」 ───────────────────────────────────────────────────────────

/// 「测试连接」的结果行。`Idle` 返回 `None`（不占位）—— 没测过就空着，
/// 比挂一句「尚未测试」安静。
fn test_result_row(state: &TestState, cx: &App) -> Option<AnyElement> {
    let theme = active_theme(cx);
    let xs = theme.tokens().typography.xs;
    let (color, text): (Hsla, SharedString) = match state {
        TestState::Idle => return None,
        TestState::Running => (
            theme.extended().colors.text_tertiary,
            SharedString::from(tr(cx, "正在测试连接…")),
        ),
        TestState::Ok(banner) => (
            theme.extended().colors.success,
            SharedString::from(format!("{} · {}", tr(cx, "连接成功"), banner)),
        ),
        TestState::Failed(reason) => (
            // 失败走 `destructive`（与 `field_error` 同一个来源），
            // 成功走 `success` —— 两色都只落在这一行小字上，不做大面积填充。
            theme.tokens().colors.destructive,
            SharedString::from(reason.clone()),
        ),
    };
    Some(
        div()
            .w_full()
            .text_size(xs.size)
            .line_height(xs.line_height)
            .text_color(color)
            .child(text)
            .into_any_element(),
    )
}

/// 把结果写回表单并重画。校准失败与探活返回两处共用这一个落点。
fn set_test(form: &Entity<ConnectionForm>, state: TestState, cx: &mut App) {
    // `update` 会失败的唯一情形是表单已经没了（弹窗已关）—— 那就什么都不做。
    let _ = form.update(cx, |this, cx| {
        this.test = state;
        cx.notify();
    });
}

/// 「测试连接」按钮（排在最左，与「取消 / 确认」隔开）。
fn test_action(form: Entity<ConnectionForm>, socket_dir: PathBuf, cx: &App) -> FooterAction {
    FooterAction {
        id: SharedString::from("terminal-conn-test"),
        text: SharedString::from(tr(cx, "测试连接")),
        on_click: Rc::new(move |_window: &mut Window, cx: &mut App| {
            run_test(&form, &socket_dir, cx)
        }),
    }
}

/// 跑一次探活，把结论写回表单。
///
/// 用的是**表单里当前的值**，包括还没保存的口令：这正是「测试连接」的意义 ——
/// 在把参数写进档案之前先确认它是对的。口令只在进程内存里过一手交给
/// [`duck_ssh::Target`]（经环境变量给 askpass 助手），不入库、不进 argv、不落日志。
///
/// 测的就是**真正连接时会走的那条路**：同一份 `duck-ssh` 参数骨架、同一个
/// socket 目录（连接复用命中同一条物理连接）、同一套 askpass 口令注入。
fn run_test(form: &Entity<ConnectionForm>, socket_dir: &Path, cx: &mut App) {
    let (host, port, user, auth, key_file, password, forwards, agent_forwarding) = {
        let state = form.read(cx);
        (
            state.host.read(cx).value().trim().to_owned(),
            state.port.read(cx).value().trim().to_owned(),
            state.user.read(cx).value().trim().to_owned(),
            state.auth,
            state.key_file.read(cx).value().trim().to_owned(),
            state.password.read(cx).value().to_string(),
            // 测试走的是 base_argv（`scp` 那一组不带转发），但**带上转发配置**
            // 才能让 `validate` 把坏掉的转发在本地拦下 —— 否则用户要等到
            // 真正连接时才发现转发行填错了。
            state
                .forwards
                .iter()
                .map(|row| row.draft(cx))
                .filter_map(|draft| forward_from_row(&draft))
                .collect::<Vec<_>>(),
            // 转发开关也一并带上：真正连接时会注入 `ForwardAgent=yes` 与 agent
            // socket 环境变量，实测路径必须一致，否则「测试通过、真连失败」。
            state.agent_forwarding,
        )
    };
    let target = duck_ssh::Target {
        host,
        // 端口填错不该花一次网络往返才知道：解析不了就记 0，`validate` 当场拦下。
        port: port.parse().unwrap_or(0),
        user,
        identity: (auth == AuthKind::Key && !key_file.is_empty()).then(|| PathBuf::from(key_file)),
        password: (auth == AuthKind::Password && !password.is_empty()).then_some(password),
        // 测试连接与实际连接**同一份认证口径**：这里把用户选的认证方式一并传下去，
        // 否则「测试通过、真连失败」（或反过来）会成为一种长期存在的假象。
        auth: match auth {
            AuthKind::Key => duck_ssh::AuthMethod::Key,
            AuthKind::Agent => duck_ssh::AuthMethod::Agent,
            AuthKind::Password => duck_ssh::AuthMethod::Password,
        },
        options: Vec::new(),
        forwards,
        agent_forwarding,
    };
    // 本地就能判的错别去起 `ssh`：一次往返几秒，而这里立刻能说清楚。
    if let Err(reason) = target.validate() {
        set_test(form, TestState::Failed(tr(cx, reason).to_owned()), cx);
        return;
    }
    set_test(form, TestState::Running, cx);
    let socket_dir = socket_dir.to_path_buf();
    // `duck-ssh` 全是**同步** API（`Command::output`），直接在渲染线程上调用会把
    // UI 冻住一次网络往返 —— 与 `db` / `redis` / `mail` 三个页面同一条纪律。
    let task = cx
        .background_executor()
        .spawn(async move { duck_ssh::test_connection(&target, Some(&socket_dir)) });
    let form = form.clone();
    // `App::spawn` 的回调只收一个 `AsyncApp`（两个形参那是 `Context::spawn`）。
    cx.spawn(async move |cx| {
        let result = task.await;
        // 后台线程只负责跑 ssh；回主线程才动状态。
        cx.update(|cx| {
            let state = match result {
                Ok(banner) => TestState::Ok(banner),
                Err(error) => {
                    // 内核零依赖、不认识 `duck-theme`，只交出「源串 + 参数」，
                    // 语言在这里定 —— 否则英文界面会冒出中文。
                    let (source, args) = error.template();
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    TestState::Failed(tf(active_theme(cx).lang(), source, &args))
                }
            };
            set_test(&form, state, cx);
        });
    })
    .detach();
}
/// 打开目录弹窗：手输路径；本地侧另有「选择目录…」调用系统目录选择器。
pub(crate) fn open_directory_dialog<F>(
    window: &mut Window,
    cx: &mut App,
    side: Side,
    current: String,
    on_open: F,
) where
    F: Fn(String, &mut Window, &mut App) + 'static,
{
    let form = cx.new(|cx| DirectoryForm {
        path: cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(current)
                .placeholder(tr(cx, "/srv/app/releases"))
        }),
        submitted: false,
    });
    let on_open: OpenHandler = Rc::new(on_open);
    // 与连接弹窗同范式：确认动作放函数级 `Rc`，键盘 ⏎ 与底部「打开」按钮共用。
    let confirm: ConfirmHandler = {
        let form = form.clone();
        let on_open = Rc::clone(&on_open);
        Rc::new(move |window, cx| {
            let value = form.read(cx).path.read(cx).value().trim().to_owned();
            if value.is_empty() {
                form.update(cx, |state, cx| {
                    state.submitted = true;
                    cx.notify();
                });
                return false;
            }
            on_open(value, window, cx);
            true
        })
    };
    let path_for_content = form.read(cx).path.clone();
    let path_for_focus = path_for_content.clone();
    let form_for_content = form.clone();
    let drag = modal_drag(cx);

    window.open_dialog(cx, move |dialog, _, cx| {
        let draft_form = form_for_content.clone();
        let confirm = Rc::clone(&confirm);
        let path = path_for_content.clone();
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        // 侧别只在说明行里说一次：标题已表明是「打开目录」，正文再重复「本地目录」
        // 是上下文冗余；这行真正要传达的是「读哪里、会不会动磁盘」。
        let lead = if side.is_local() {
            SharedString::from(tr(cx, "输入本机目录，或从系统目录选择器里挑一个。"))
        } else {
            SharedString::from(tr(cx, "输入远端服务器上的目录路径。"))
        };
        let hint = if side.is_local() {
            SharedString::from(tr(cx, "本地目录会立刻读取本机磁盘（只读）。"))
        } else {
            SharedString::from(tr(cx, "远端路径区分大小写，需以 / 开头。"))
        };

        dialog
            .w(px(DIRECTORY_DIALOG_W))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(tr(cx, "打开"))
                    .cancel_text(tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                let submitted = draft_form.read(cx).submitted;
                let empty = path.read(cx).value().trim().is_empty();

                let mut control = Input::new(&path).control(cx).w_full();
                if side.is_local() {
                    let target = path.clone();
                    let pick = button(
                        "terminal-pick-folder",
                        tr(cx, "选择目录…"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let Some(dir) = duck_platform::pick_folder() else {
                            return;
                        };
                        let text = dir.to_string_lossy().into_owned();
                        target.update(cx, |state, cx| state.set_value(text, window, cx));
                    });
                    control = control.suffix(pick);
                }

                let path_field = form_field(
                    tr(cx, "路径"),
                    control,
                    (submitted && empty)
                        .then(|| SharedString::from(tr(cx, "请输入要打开的目录路径。"))),
                    // 说明行分两段：第一段说「怎么填」，第二段说「填完会发生什么」。
                    None,
                    cx,
                );
                let tokens = active_theme(cx).tokens().clone();
                content.child(modal_drag_area(
                    &drag_for_content,
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .child(modal_title_row(
                            "terminal-dir-title",
                            &drag_for_content,
                            tr(cx, "打开目录"),
                            cx,
                        ))
                        .child(path_field)
                        .child(field_hint(lead.clone(), cx))
                        .child(field_hint(hint.clone(), cx)),
                ))
            })
            .on_ok({
                let confirm = Rc::clone(&confirm);
                move |_, window, cx| confirm(window, cx)
            })
            .footer(dialog_footer(
                "terminal-dir",
                tr(cx, "打开"),
                ButtonVariant::Primary,
                confirm,
                cx,
            ))
    });
    path_for_focus.update(cx, |state, cx| state.focus(window, cx));
}

/// 条目命名弹窗的字段状态。
///
/// 「新建目录」与「重命名」共用这一张表单：两件事的输入、校验、「确认前不改磁盘」
/// 完全一样，差别只在标题、初始值与落点 —— 分成两个弹窗只会让校验规则各长一份。
struct NameForm {
    /// 名称 / 新名称。
    name: Entity<InputState>,
    /// 是否提交过（校验提示延后到提交之后）。
    submitted: bool,
}

/// 命名弹窗的两种意图。
///
/// 文案随意图走，而不是由调用点逐个传 —— 一个「新建」一个「重命名」，各自该说什么
/// 是弹窗自己的知识，散到调用点只会让同一个字段在两处各写一份译文。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameDialogKind {
    /// 新建远端目录。
    NewFolder,
    /// 重命名远端条目。
    Rename,
}

impl NameDialogKind {
    /// 标题（i18n 源串）。
    const fn title(self) -> &'static str {
        match self {
            Self::NewFolder => "新建文件夹",
            Self::Rename => "重命名",
        }
    }

    /// 输入框标签。
    const fn field(self) -> &'static str {
        match self {
            Self::NewFolder => "文件夹名称",
            Self::Rename => "新名称",
        }
    }

    /// 输入框占位。
    const fn placeholder(self) -> &'static str {
        match self {
            Self::NewFolder => "例如 releases",
            Self::Rename => "例如 app-v2.tar.gz",
        }
    }

    /// 确认按钮文字。
    const fn ok_text(self) -> &'static str {
        match self {
            Self::NewFolder => "创建",
            Self::Rename => "重命名",
        }
    }

    /// 一行说明。
    const fn hint(self) -> &'static str {
        match self {
            Self::NewFolder => "在当前远端目录下新建一个目录。",
            Self::Rename => "只改名字，不移动位置。",
        }
    }
}

/// 新建目录 / 重命名弹窗。
///
/// 校验只有两条：**非空**、**不含 `/`**。后者是因为名字会被拼进远端路径交给
/// `mkdir` / `mv`，带斜杠就等于允许用户输入一个任意路径 —— 那是一个远超
/// 「改个名字」的权限，得由一个明确的「移动」入口承担，而不是藏在重命名框里。
pub(crate) fn open_entry_name_dialog<F>(
    window: &mut Window,
    cx: &mut App,
    kind: NameDialogKind,
    initial: String,
    on_submit: F,
) where
    F: Fn(String, &mut Window, &mut App) + 'static,
{
    let (title, field, ok_text, hint) = (kind.title(), kind.field(), kind.ok_text(), kind.hint());
    let form = cx.new(|cx| NameForm {
        name: cx.new(|cx| {
            let state = InputState::new(window, cx).placeholder(tr(cx, kind.placeholder()));
            if initial.is_empty() {
                state
            } else {
                state.default_value(initial)
            }
        }),
        submitted: false,
    });
    let on_submit: NameHandler = Rc::new(on_submit);
    // 与连接 / 目录弹窗同范式：确认动作放函数级 `Rc`，键盘 ⏎ 与底部确认按钮共用。
    let confirm: ConfirmHandler = {
        let form = form.clone();
        let on_submit = Rc::clone(&on_submit);
        Rc::new(move |window, cx| {
            let value = form.read(cx).name.read(cx).value().trim().to_owned();
            if value.is_empty() || value.contains('/') {
                form.update(cx, |state, cx| {
                    state.submitted = true;
                    cx.notify();
                });
                return false;
            }
            on_submit(value, window, cx);
            true
        })
    };
    let name_for_content = form.read(cx).name.clone();
    let name_for_focus = name_for_content.clone();
    let form_for_content = form.clone();
    let drag = modal_drag(cx);

    window.open_dialog(cx, move |dialog, _, cx| {
        let draft_form = form_for_content.clone();
        let confirm = Rc::clone(&confirm);
        let name = name_for_content.clone();
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let submitted = draft_form.read(cx).submitted;
        let empty = name.read(cx).value().trim().is_empty();
        let slashed = name.read(cx).value().contains('/');
        // 两条规则各有各的说法：让用户知道**这一条**为什么不行，而不是一句笼统的「名称无效」。
        let error = if submitted && empty {
            Some(SharedString::from(tr(cx, "名称不能为空。")))
        } else if submitted && slashed {
            Some(SharedString::from(tr(cx, "名称不能包含 / 。")))
        } else {
            None
        };

        dialog
            .w(px(NAME_DIALOG_W))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(ok_text)
                    .cancel_text(tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                let name_field = form_field(
                    field,
                    Input::new(&name).control(cx).w_full(),
                    error.clone(),
                    None,
                    cx,
                );
                content.child(modal_drag_area(
                    &drag_for_content,
                    v_flex()
                        .w_full()
                        .gap(form_gap(cx))
                        .child(modal_title_row(
                            "terminal-name-title",
                            &drag_for_content,
                            title,
                            cx,
                        ))
                        .child(name_field)
                        .child(field_hint(SharedString::from(tr(cx, hint)), cx)),
                ))
            })
            .on_ok({
                let confirm = Rc::clone(&confirm);
                move |_, window, cx| confirm(window, cx)
            })
            .footer(dialog_footer(
                "terminal-name",
                ok_text,
                ButtonVariant::Primary,
                confirm,
                cx,
            ))
    });
    name_for_focus.update(cx, |state, cx| state.focus(window, cx));
}

impl ConnectionForm {
    /// 端口转发区：一行一条（方向 + 绑定 + 目标），可增可删。
    ///
    /// 每行的 `ElementId` 由行自己的自增 `id` 派生 —— 删掉中间一行不会让后面几行的
    /// 控件状态串位（用下标当 id 就会，见 AGENTS「重复元素必须有领域派生的 ElementId」）。
    fn forwards_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = active_theme(cx).tokens();
        let icon_size = active_theme(cx).extended().icon.md;
        let form = cx.entity();
        let submitted = self.submitted;

        let rows: Vec<AnyElement> = self
            .forwards
            .iter()
            .map(|row| {
                let row_id = row.id;
                let kind_options: Vec<(SharedString, SharedString)> = duck_ssh::ForwardKind::ALL
                    .iter()
                    .map(|kind| {
                        (
                            SharedString::from(kind.value()),
                            SharedString::from(tr(cx, forward_kind_label(*kind))),
                        )
                    })
                    .collect();
                let form_for_kind = form.clone();
                let kind_control = dropdown_select(
                    SharedString::from(format!("terminal-fwd-kind-{row_id}")),
                    kind_options,
                    SharedString::from(row.kind.value()),
                    move |value, _, cx| {
                        let Some(kind) = duck_ssh::ForwardKind::from_value(&value) else {
                            return;
                        };
                        form_for_kind.update(cx, |state, cx| {
                            if let Some(target) =
                                state.forwards.iter_mut().find(|item| item.id == row_id)
                            {
                                target.kind = kind;
                            }
                            cx.notify();
                        });
                    },
                    cx,
                );

                let remove_id = SharedString::from(format!("terminal-fwd-remove-{row_id}"));
                let form_for_remove = form.clone();
                let remove = icon_button(
                    remove_id,
                    tr(cx, "删除这一条转发"),
                    Icon::new(DuckIcon::X).size(icon_size),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    form_for_remove.update(cx, |state, cx| {
                        state.forwards.retain(|item| item.id != row_id);
                        cx.notify();
                    });
                });

                // 提交过才亮红：一打开就红一片是坏体验。
                let error = (submitted
                    && forward_from_row(&row.draft(cx))
                        .is_none_or(|forward| forward.validate().is_err()))
                .then(|| SharedString::from(tr(cx, "转发参数不完整：请检查端口与目标地址。")));

                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xxs)
                    .child(form_row(
                        vec![
                            kind_control,
                            Input::new(&row.bind)
                                .control(cx)
                                .w_full()
                                .into_any_element(),
                            Input::new(&row.bind_port)
                                .control(cx)
                                .w_full()
                                .into_any_element(),
                        ],
                        cx,
                    ))
                    .child(form_row(
                        vec![
                            Input::new(&row.host)
                                .control(cx)
                                .w_full()
                                .into_any_element(),
                            Input::new(&row.host_port)
                                .control(cx)
                                .w_full()
                                .into_any_element(),
                            div().flex_none().child(remove).into_any_element(),
                        ],
                        cx,
                    ))
                    .when_some(error, |this, error| this.child(field_error(error, cx)))
                    .into_any_element()
            })
            .collect();

        let form_for_add = form.clone();
        let add = button(
            "terminal-fwd-add",
            tr(cx, "添加转发"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(move |_, window, cx| {
            form_for_add.update(cx, |state, cx| {
                let id = state.next_forward_id;
                state.next_forward_id += 1;
                let row = ForwardRow::new(id, None, window, cx);
                state.forwards.push(row);
                cx.notify();
            });
        });

        v_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .child(field_label(tr(cx, "端口转发"), cx))
            .child(field_hint(
                SharedString::from(tr(
                    cx,
                    "本地转发把本机端口的流量送到远端；远程转发反过来；动态转发在本机起一个 SOCKS 代理。留空绑定地址即用 ssh 默认。",
                )),
                cx,
            ))
            .children(rows)
            .child(div().flex().child(add))
            .into_any_element()
    }
}

/// 开关行：左侧标题 + 说明，右侧开关。
///
/// 说明与标题间距用 `xxs`（同一控件内部），整行与相邻字段的间距由外层 `form_gap` 给。
fn switch_row(
    id: &'static str,
    label: impl Into<SharedString>,
    hint: Option<SharedString>,
    checked: bool,
    on_toggle: impl Fn(&bool, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let tokens = active_theme(cx).tokens();
    h_flex()
        .w_full()
        .items_center()
        .gap(tokens.spacing.lg)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(tokens.spacing.xxs)
                .child(body_text(label, cx))
                .when_some(hint, |this, hint| this.child(field_hint(hint.clone(), cx))),
        )
        .child(switch_control(id, checked, on_toggle))
}
