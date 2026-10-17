//! 设置窗口：完全对照 FluxDown `SettingsView` 的结构 ——
//! 左侧「搜索 + 分类导航」，右侧「页面标题区 + 可滚动内容区」。
//!
//! 分类页内容沿用外观 / 行为 / 通知 / 关于四组设置；外观页按参考实现的宽视口
//! 双列分组卡片布局（左：主题与配色；右：界面）。所有改动即时生效，走
//! [`duck_theme::update_appearance`] 统一投影到两层主题。
//!
//! 「工具」页是**注册表驱动**的：左侧列出 `ToolRegistry` 里当前列出的工具，
//! 右侧按该工具的 [`ToolSettingSpec`] 逐项渲染，外加通用的启用开关与使用次数。
//! 新增工具设置不需要改 `pages_tools.rs`——加一条 `settings()` 就在界面上出现。
//!
//! 窗口为独立辅助窗口（`duck_shell::AuxiliaryWindowView`），已打开则前置聚焦。
//!
//! # 文件划分（P3-3）
//!
//! 本模块原先是一个 5.5k 行的单文件；按「页面壳 / 数据快照 / 控件构造」拆开后：
//!
//! | 文件 | 职责 |
//! | --- | --- |
//! | `mod.rs`（本文件） | 壳：常量、入口、分类表 `PAGES`、窗口视图与导航、`Render` |
//! | `state.rs` | 数据快照：选择器状态、AI 草稿、知识库任务句柄 |
//! | `shell.rs` | 侧栏与窗口壳辅助构造 |
//! | `widgets.rs` | 跨页复用的控件构造 |
//! | `pages_*.rs` | 各设置页正文（`impl SettingsWindowView` 的按页拆分） |
//!
//! ⚠️ 子模块一律以 `use super::*;` 承接本文件的导入面；**跨模块调用的项必须带
//! `pub(super)`**（子模块里的 `pub(super)` 恰好等于原先「模块私有」的可见范围）。
//! ⚠️ 子模块里写 `super::foo` 指的是**本模块**，要引用 `pages` 下的兄弟页必须写
//! `super::super::foo`。

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use duck_ai::{IndexProgress, KbIndex, KbSource, KbStats};
use duck_components::{
    ButtonVariant, ControlExt, DuckIcon, LOGO_ASSET_PATH, NavItem, beta_badge, body_text, button,
    content, dropdown_select, meta_text, nav_list, notify, option_group, option_row,
    option_row_stacked, settings_section, switch_control, title_text, toolbar_action_button,
};
use duck_shell::AuxiliaryWindowView;
use duck_theme::{
    Appearance, BuiltinThemeId, Density, FONT_SIZE_RANGE, FONT_SIZES, Lang, Locale, NAV_GAP_RANGE,
    SidebarSide, ThemeMode, ThemePreference, UnderMinute, accent_from_rgb, active_theme,
    default_accent, number, t, tr, tr_relative_time, trf, update_appearance,
};
use duck_tools::ToolRegistry;
use duck_tools::settings as tool_settings;
use duck_tools::settings::{SettingValue, ToolSettingKind, ToolSettingSpec};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, Div, Entity, FocusHandle, Focusable as _,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Render, Role, SharedString, StatefulInteractiveElement as _, Styled,
    Subscription, Task, Toggled, WeakEntity, Window, div, img, percentage,
    prelude::FluentBuilder as _, px, relative, size,
};
use gpui_component::{
    Icon, Root,
    button::Button,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    tooltip::Tooltip,
    v_flex,
};

use crate::ai::{self, AiSettings};
use crate::app::{Desktop, WindowKind};
use crate::diagnostics;
use crate::hotkey::HotkeySlot;
use crate::pages::logs::LogView;
use crate::pages::status_support::{
    NumberConfirm, TextConfirm, format_bytes, open_number_prompt, open_text_prompt,
};
use crate::settings::{self, AuxWindow};
use crate::update;

mod pages_about;
mod pages_ai;
mod pages_appearance;
mod pages_audit;
mod pages_hotkey;
mod pages_logs;
mod pages_notify;
mod pages_plugins;
mod pages_tools;
mod shell;
mod state;
mod widgets;

use self::shell::*;
use self::state::*;
use self::widgets::*;

/// 设置窗口初始尺寸：比主窗口（1180×760）小一档，视觉上从属于主窗口。
pub(super) const SETTINGS_WINDOW_SIZE: gpui::Size<gpui::Pixels> = size(px(1000.), px(660.));
/// 设置窗口最小尺寸：仍能容纳 200px 分类导航 + 内容区左右留白。
pub(super) const SETTINGS_WINDOW_MIN: gpui::Size<gpui::Pixels> = size(px(860.), px(560.));

/// 分类导航列宽（对齐参考实现 `SIDEBAR_WIDTH`）。
pub(super) const SIDEBAR_WIDTH: f32 = 200.;
/// 内容区左右留白（对齐参考实现 `CONTENT_PADDING_LEFT/RIGHT`）。
/// 工具箱工作区与设置页共用同一节奏，`pub(crate)` 供 `pages::toolbox` 引用。
pub(crate) const CONTENT_PADDING_LEFT: f32 = 28.;
pub(crate) const CONTENT_PADDING_RIGHT: f32 = 24.;

/// 主题卡片宽度（对齐参考实现 `THEME_CARD_WIDTH`）。
pub(super) const THEME_CARD_WIDTH: f32 = 120.;
/// 卡片内预览块高度。
pub(super) const THEME_PREVIEW_HEIGHT: f32 = 52.;

/// 「关于」页 logo 的边长。
pub(super) const ABOUT_LOGO_SIZE: f32 = 88.;
/// logo 圆角：应用图标本身是圆角方形，按比例取 88 的四分之一略弱。
pub(super) const ABOUT_LOGO_RADIUS: f32 = 20.;
/// 品牌说明的最大宽度：再宽行长就不好读了（约 40 个汉字）。
pub(super) const ABOUT_DESCRIPTION_MAX_WIDTH: f32 = 460.;
/// 预览块左侧栏宽度。
pub(super) const THEME_PREVIEW_SIDEBAR_WIDTH: f32 = 28.;
/// 预览块内的示意条高度。
pub(super) const THEME_PREVIEW_BAR_HEIGHT: f32 = 3.;
/// 强调色色点直径。
pub(super) const COLOR_DOT_SIZE: f32 = 28.;

/// 审计流水每列的固定宽度（px）。
///
/// 固定而不是让内容撑：流水是要**竖着扫**的（「哪几行是连接」「哪几行失败了」），
/// 列宽随内容浮动会让同名字段落在不同的水平位置上，扫视就断了。
pub(super) const AUDIT_TIME_WIDTH: f32 = 84.;
pub(super) const AUDIT_KIND_WIDTH: f32 = 72.;
pub(super) const AUDIT_ACTION_WIDTH: f32 = 88.;
pub(super) const AUDIT_OUTCOME_WIDTH: f32 = 76.;

/// 可选强调色（预设色点）。首项即默认强调色，与 `duck_theme::DEFAULT_ACCENT_RGB` 同源。
pub(super) const ACCENTS: [u32; 6] = [
    duck_theme::DEFAULT_ACCENT_RGB,
    0x8B5CF6,
    0x06B6D4,
    0x22C55E,
    0xF97316,
    0xEC4899,
];

/// 可选界面缩放档位。
pub(super) const SCALES: [f32; 4] = [0.9, 1.0, 1.1, 1.25];

/// 可选侧栏项间距档位（px）。落在 `duck_theme::NAV_GAP_RANGE` 内。
pub(super) const NAV_GAPS: [f32; 6] = [0., 4., 8., 12., 16., 24.];

/// 字号下拉中「跟随主题」项的值。
pub(super) const FONT_SIZE_THEME: &str = "theme";

// 设置窗口句柄由 `Desktop` 持有，打开或聚焦复用。

/// 打开（或聚焦）设置窗口。一律 defer：动作回调处于活动窗口的 update 栈内，
/// 同步打开窗口可能静默失败（对齐参考实现 `reveal` 的处理）。
///
/// 不收 `&mut Window`：设置窗口是独立辅助窗口，跟「从哪个窗口点进来的」无关。
/// 托盘也要开它，而托盘那条路径上没有窗口可给。
pub fn open_settings(cx: &mut App) {
    cx.defer(open_or_focus);
}

/// 打开（或聚焦）设置窗口并定位到指定分类，可按需同时选中「工具」页里的某个工具。
///
/// 工具箱工作区的「设置」页签经此跳转。定位不走「从外部驱动窗口内视图」——窗口句柄
/// 的类型是 `Root`，拿不到里面的内容视图；改成把目标投递到全局待处理槽，
/// 由设置窗口在下一次渲染时取走（见 [`SettingsWindowView::apply_pending_page`]）。
pub fn open_settings_at(page: &'static str, tool: Option<&'static str>, cx: &mut App) {
    Desktop::set_settings_pending(cx, page, tool.map(ToOwned::to_owned));
    cx.defer(open_or_focus);
}

/// 跳转目标：分类 key + （可选）「工具」页要选中的工具 id。
pub struct Target {
    pub(super) page: &'static str,
    pub(super) tool: Option<String>,
}

impl Target {
    /// 取走全局待处理槽里的跳转请求。
    fn take(cx: &mut App) -> Option<Self> {
        Desktop::take_settings_pending(cx).map(|(page, tool)| Self { page, tool })
    }
}

pub(super) fn open_or_focus(cx: &mut App) {
    if let Some(handle) = Desktop::window_handle(cx, WindowKind::Aux(AuxWindow::Settings)) {
        if handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            // 窗口已开着：待处理跳转要等它下一次渲染才被取走，这里催一次重绘。
            cx.refresh_windows();
            cx.activate(true);
            return;
        }
    }
    Desktop::clear_window(cx, WindowKind::Aux(AuxWindow::Settings));

    let mut options = duck_shell::auxiliary_window_options(SharedString::from(tr(cx, "设置")));
    // 上次的位置尺寸（已夹回可见区域）；没有存档则主屏居中（多屏时跟随主显示器）。
    options.window_bounds = Some(settings::aux_window_bounds(
        cx,
        AuxWindow::Settings,
        SETTINGS_WINDOW_SIZE,
    ));
    options.window_min_size = Some(SETTINGS_WINDOW_MIN);

    match cx.open_window(options, |window, cx| {
        let content = cx.new(|cx| SettingsWindowView::new(window, cx));
        let view = content.downgrade();
        let chrome = cx.new(|cx| {
            // 占位标题：真实文案由 with_title_fn 按语言解析，此处不会被渲染。
            AuxiliaryWindowView::new("Settings", content.into(), cx)
                .with_title_fn(|cx| SharedString::from(tr(cx, "设置")))
                // 折叠按钮排在「设置」标题左边（标题栏是唯一能横跨导航列与内容区的地方）。
                .with_title_leading(move |cx| sidebar_toggle(view.clone(), cx))
                // 几何变化 → 去抖持久化（与主窗口同形）。
                .on_window_bounds_changed(|window, cx| {
                    settings::save_aux_bounds(AuxWindow::Settings, window, cx);
                })
        });
        crate::app::observe_system_appearance(window);
        cx.new(|cx| Root::new(chrome, window, cx))
    }) {
        Ok(handle) => {
            Desktop::set_window(cx, WindowKind::Aux(AuxWindow::Settings), handle);
            // 记下「这个窗口开着」，供下次启动恢复（见「设置 → 行为」的开关）。
            Desktop::remember_aux_open(cx);
            cx.activate(true);
        }
        Err(error) => crate::app::report_window_failure("设置窗口", &error, cx),
    }
}

/// 分类页在导航里的分组。
///
/// 分组只影响**侧栏怎么排版**（组标题 + 一段导航列表），不影响搜索：搜索跨全部组，
/// 命中的组才显示出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageGroup {
    /// 应用级：界面与交互本身。
    General,
    /// 能力：工具、插件、AI。
    Capability,
    /// 模块：终端 / 数据库 / 邮箱 / Redis 各自的偏好。
    Module,
    /// 系统：日志、数据、关于。
    System,
}

impl PageGroup {
    /// 导航顺序（同时决定组标题的展示顺序）。
    const ALL: [Self; 4] = [Self::General, Self::Capability, Self::Module, Self::System];

    /// 组标题（中文源串，渲染时按当次语言 `tr`）。
    const fn title(self) -> &'static str {
        match self {
            Self::General => "通用",
            Self::Capability => "能力",
            Self::Module => "模块",
            Self::System => "系统",
        }
    }

    /// 导航列表元素 id 用的稳定短名。
    const fn key(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Capability => "capability",
            Self::Module => "module",
            Self::System => "system",
        }
    }
}

/// 分类页描述（导航与搜索索引共用）。
pub struct SettingsPageSpec {
    pub(super) key: &'static str,
    pub(super) title: &'static str,
    pub(super) description: &'static str,
    pub(super) icon: DuckIcon,
    pub(super) group: PageGroup,
}

/// 日志分类的 key。
///
/// 它是唯一**内容区不套滚动容器**的分类（见 [`SettingsWindowView::render_content`]），
/// 规格表与渲染分支两处引用必须同源，别各写一遍字面量。
pub(super) const LOGS_PAGE: &str = "logs";

/// 数据分类的 key（规格表与渲染分支同源）。
pub(super) const DATA_PAGE: &str = "data";

/// AI 分类的 key（规格表与渲染分支同源）。
pub(super) const AI_PAGE: &str = "ai";

/// 「终端」模块页的 key（规格表与渲染分支同源）。
pub(super) const TERMINAL_PAGE: &str = "terminal";
/// 「数据库」模块页的 key（规格表与渲染分支同源）。
pub(super) const DATABASE_PAGE: &str = "database";
/// 「邮箱」模块页的 key（规格表与渲染分支同源）。
pub(super) const MAIL_PAGE: &str = "email";
/// 「Redis」模块页的 key（规格表与渲染分支同源）。
pub(super) const REDIS_PAGE: &str = "redis";
/// 「项目管理」模块页的 key（规格表与渲染分支同源）。
pub(super) const PROJECTS_PAGE: &str = "projects";

/// 分类清单（顺序即导航顺序）。
pub(super) const PAGES: &[SettingsPageSpec] = &[
    SettingsPageSpec {
        key: "appearance",
        title: "外观",
        description: "主题与配色",
        icon: DuckIcon::SlidersHorizontal,
        group: PageGroup::General,
    },
    SettingsPageSpec {
        key: "behavior",
        title: "行为",
        description: "列表、确认与开机启动",
        icon: DuckIcon::Zap,
        group: PageGroup::General,
    },
    SettingsPageSpec {
        key: "notify",
        title: "通知",
        description: "应用内与系统通知通道",
        icon: DuckIcon::Bell,
        group: PageGroup::General,
    },
    SettingsPageSpec {
        key: "tools",
        title: "工具",
        description: "工具启停与工具级设置",
        icon: DuckIcon::Box,
        group: PageGroup::Capability,
    },
    SettingsPageSpec {
        key: "plugins",
        title: "插件",
        description: "市场源与已安装插件",
        icon: DuckIcon::Library,
        group: PageGroup::Capability,
    },
    SettingsPageSpec {
        key: AI_PAGE,
        title: "AI",
        description: "模型厂商、密钥与生成参数",
        icon: DuckIcon::Bot,
        group: PageGroup::Capability,
    },
    SettingsPageSpec {
        key: TERMINAL_PAGE,
        title: "终端",
        description: "字号、回滚与会话行为",
        icon: DuckIcon::SquareTerminal,
        group: PageGroup::Module,
    },
    SettingsPageSpec {
        key: DATABASE_PAGE,
        title: "数据库",
        description: "查询上限与新连接默认值",
        icon: DuckIcon::Database,
        group: PageGroup::Module,
    },
    SettingsPageSpec {
        key: MAIL_PAGE,
        title: "邮箱",
        description: "通知、正文形态与列表条数",
        icon: DuckIcon::Mail,
        group: PageGroup::Module,
    },
    SettingsPageSpec {
        key: REDIS_PAGE,
        title: "Redis",
        description: "键扫描与会话",
        icon: DuckIcon::DatabaseZap,
        group: PageGroup::Module,
    },
    SettingsPageSpec {
        key: PROJECTS_PAGE,
        title: "项目管理",
        description: "到期提醒与久未更新口径",
        icon: DuckIcon::Kanban,
        group: PageGroup::Module,
    },
    SettingsPageSpec {
        key: LOGS_PAGE,
        title: "日志",
        description: "日志文件与运行记录",
        icon: DuckIcon::FileText,
        group: PageGroup::System,
    },
    SettingsPageSpec {
        key: crate::audit::AUDIT_PAGE,
        title: "审计日志",
        description: "连接、查询与写入的流水",
        icon: DuckIcon::ShieldCheck,
        group: PageGroup::System,
    },
    SettingsPageSpec {
        key: DATA_PAGE,
        title: "数据",
        description: "备份、恢复与重置",
        icon: DuckIcon::Archive,
        group: PageGroup::System,
    },
    SettingsPageSpec {
        key: "about",
        title: "关于",
        description: "版本、更新与发布记录",
        icon: DuckIcon::Info,
        group: PageGroup::System,
    },
];

/// 面板索引用的分类快照：`(key, 标题源串, 描述源串, 图标)`。
///
/// 文案保持**中文源串**形式交给调用方按当次语言 `tr`——这里不预先翻译，
/// 面板条目每次重算，语言切换后自然跟着变。
pub(crate) fn palette_pages()
-> impl Iterator<Item = (&'static str, &'static str, &'static str, DuckIcon)> {
    PAGES
        .iter()
        .map(|page| (page.key, page.title, page.description, page.icon))
}

/// 把分类 key 还原成 `'static`。
///
/// 命令面板的条目携带的是运行时字符串，而 [`open_settings_at`] 要的是静态 key；
/// 认不出来（存档 / 快照来自旧版本）就返回 `None`，由调用方静默忽略。
pub(crate) fn palette_page_key(key: &str) -> Option<&'static str> {
    PAGES
        .iter()
        .find(|page| page.key == key)
        .map(|page| page.key)
}

/// 「工具」页左侧工具列的宽度。
pub(super) const TOOL_PICKER_WIDTH: f32 = 176.;

/// 「AI」页短输入框的宽度（名称 / 温度 / 上限）。
///
/// 端点与模型是长串，走整行不受这个约束；把短字段也拉满宽度只会让表单看着散。
pub(super) const AI_FIELD_WIDTH: f32 = 240.;

/// 知识库索引进度轮询间隔。
///
/// 索引是**一次性长任务**，进度文案不是逐 token 的流式内容，120ms 已经比人眼快；
/// 与助手页的 40ms 拉开距离，是因为那里的 `notify` 还要驱动 `TextView` 的流式淡入。
pub(super) const KB_POLL_INTERVAL: Duration = Duration::from_millis(120);

/// 「知识库」进度日志最多保留的行数（只留尾部）。
///
/// 一次误选整个盘会刷出上万条「已写入 N 块」；不设上限就是把内存当垃圾桶。
pub(super) const KB_LOG_LIMIT: usize = 24;

/// 一个全局热键槽位在设置页里的界面状态。
///
/// 收成一个小结构而不是散成六个字段：两个槽位（将来可能更多）要各有一份焦点句柄、
/// 录入态、开关快照与失焦订阅，散着写迟早漏掉一个（`_blur` 漏了就是「点了别处还在录」）。
pub(super) struct HotkeyEntry {
    /// 键帽框的焦点句柄。没有焦点的话按键根本派发不过来。
    pub(super) focus: FocusHandle,
    /// 是否正在录入。`true` 时下一次按键被当成新键位。
    pub(super) recording: bool,
    /// 开关的快照（构造时从 `kv` 读一次；切换失败时按真实值回摆）。
    pub(super) enabled: bool,
    /// 录入控件失焦时退出录入的订阅。**必须留着** —— drop 掉监听就没了。
    _blur: Subscription,
}

impl HotkeyEntry {
    /// 建一个槽位的界面状态：焦点句柄、开关快照、失焦退出录入。
    ///
    /// 不做这一步的话，界面会一直停在「按下组合键…」，而按键早就跑到别的控件那里去了。
    fn new(
        window: &mut Window,
        cx: &mut Context<SettingsWindowView>,
        slot: crate::hotkey::HotkeySlot,
    ) -> Self {
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, move |this, _, cx| {
            let entry = this.hotkey_entry_mut(slot);
            if entry.recording {
                entry.recording = false;
                cx.notify();
            }
        });
        Self {
            focus,
            recording: false,
            enabled: settings::hotkey_enabled(cx, slot),
            _blur: blur,
        }
    }
}

impl SettingsWindowView {
    /// 某个热键槽位的录入 / 开关状态。
    pub(super) fn hotkey_entry(&self, slot: HotkeySlot) -> &HotkeyEntry {
        &self.hotkeys[slot.index()]
    }

    /// 同上，可变借用。
    pub(super) fn hotkey_entry_mut(&mut self, slot: HotkeySlot) -> &mut HotkeyEntry {
        &mut self.hotkeys[slot.index()]
    }
}

pub struct SettingsWindowView {
    pub(super) search: Entity<InputState>,
    /// 当前选中分类的 key。
    pub(super) selected: SharedString,
    /// 「卸载插件前弹确认」的快照（构造时从 `kv` 读一次，切换时同步落盘）。
    pub(super) confirm_uninstall: bool,
    /// 「开机自动启动」当前是否生效（构造时从系统读一次，切换后按返回结果刷新；
    /// 系统登记是唯一事实来源，本字段只是快照）。
    pub(super) autostart_enabled: bool,
    /// 「启动时恢复上次窗口」是否开启（构造时从 `kv` 读一次，切换时同步更新）。
    ///
    /// 缺省**关**：辅助窗口是「用户主动叫出来的」，开机时自动弹出一堆窗口是打扰。
    pub(super) restore_windows: bool,
    /// 文件图标包（[`ICON_PACKS`] 的 id）。
    pub(super) icon_pack: SharedString,
    /// 「字体」可搜索下拉（本机字体族枚举）。
    pub(super) font_select: Entity<FontSelectState>,
    /// 上次同步进下拉的字体偏好，避免每帧重复 `set_selected_value`。
    pub(super) font_synced: Option<SharedString>,
    /// 自定义强调色取色器是否展开（对齐参考实现：选中「自定义」时展开）。
    pub(super) custom_accent_open: bool,
    /// 「工具」页选中的工具 id；`None` 表示跟随列表首项。
    pub(super) selected_tool: Option<SharedString>,
    /// 搜索框占位文案使用的语言；语言切换后同步刷新（占位只在构建时定型）。
    pub(super) search_placeholder_lang: Option<Lang>,
    /// 分类导航列是否收起（标题栏左侧的按钮切换）。
    ///
    /// 会话内保留即可：窗口关掉即释放，重开是新视图、默认展开。
    /// 不用 `SidebarPanel` 那套动画：设置窗口只有几个分类，收起是瞬时的事，
    /// 为此引入可拖宽分栏（会多一条拖拽热区）不划算。
    pub(super) sidebar_collapsed: bool,
    /// 「日志」页的查看器。**懒建**：查看器自带每秒轮询的任务，没进过日志页就不该有它在跑；
    /// 建过一次就留着，离开再回来时选中的文件还在。
    pub(super) logs_view: Option<Entity<LogView>>,
    /// 日志目录的 `(文件数, 总字节数)` 快照。
    ///
    /// 存在这里而不是每次渲染去 `read_dir`：查看器**本来**就每秒扫一遍目录，这里跟着它的
    /// `notify` 抄一份即可（见 [`Self::logs_view`] 的订阅）。渲染里做系统调用是另一回事。
    pub(super) logs_summary: (usize, u64),
    /// 插件市场源输入框（「插件」页）。改动前从 `market` 读一次初值。
    pub(super) market_source: Entity<InputState>,
    /// 日志保留天数（[`crate::pages::logs::RETENTION_CHOICES`] 里的一档，0 = 永久保留）。
    /// 构造时从 `kv` 读一次快照，改档位时同步更新。
    pub(super) log_retention_days: u32,
    /// 两个全局热键槽位（命令面板 / 随手看）的录入与开关状态，按
    /// [`HotkeySlot::index`] 索引。
    pub(super) hotkeys: [HotkeyEntry; HotkeySlot::ALL.len()],
    /// 「唤起时打开主窗口」开关的快照（构造时从 `kv` 读一次）。
    pub(super) launcher_opens_main_window: bool,
    #[allow(dead_code)]
    pub(super) _font_subscription: Subscription,

    /// 「审计日志」页：按类别筛选（`None` = 全部）。
    pub(super) audit_kind: Option<SharedString>,
    /// 「审计日志」页：只看失败 / 被拦 / 被取消。
    pub(super) audit_problems_only: bool,
    /// 「审计日志」页：只看某一次错误（点主窗状态栏的错误徽标进来时自动带上）。
    ///
    /// 用 `SharedString` 而不是 `String`：它同时是那一行筛选提示的渲染内容。
    pub(super) audit_trace: Option<SharedString>,
    /// 「审计日志」页：保留条数的快照（构造时从 `kv` 读一次，改档位时同步落盘）。
    pub(super) audit_keep: usize,
    /// 「审计日志」页：当前筛选下的记录快照。
    ///
    /// 为什么快照而不是每帧查库：这一页在筛选没变时**每帧渲染的都是同一批行**，
    /// 而查一次要走读连接 + `LIMIT 200` 的排序；快照把开销压到「筛选变了才查」。
    /// 代价是它不会自己变新，所以进页 / 改筛选 / 点刷新都要显式 [`Self::reload_audit`]。
    pub(super) audit_rows: Vec<duck_store::AuditRow>,

    /// 「AI」页的厂商档案快照（`kv` 读入；本页写，写完立刻回读刷新）。
    pub(super) ai_providers: Vec<duck_ai::ProviderProfile>,
    /// 「AI」页的生成参数快照（同上）。
    pub(super) ai_settings: AiSettings,
    /// 安全存储此刻是否可用。
    ///
    /// **必须缓存**：`secret::available()` 会真读一次系统存储，而这一页每帧都要
    /// 拿它决定凭据输入框的可用态。快照在进页与每次保存后刷新（见 [`Self::ai_reload`]）。
    pub(super) ai_secure_available: bool,
    /// 生成参数四个控件（温度 / 最大输出 / 最大工具轮次 / 系统提示词）。
    ///
    /// 常驻而不是按需建：这一页是「改完点保存」的形态，控件一旦被销毁，
    /// 用户没保存的输入就没了。
    pub(super) ai_temperature: Entity<InputState>,
    pub(super) ai_max_tokens: Entity<InputState>,
    pub(super) ai_max_rounds: Entity<InputState>,
    /// 上下文预算（字符数）；`0` = 关闭压缩。
    pub(super) ai_compaction_budget: Entity<InputState>,
    pub(super) ai_system_prompt: Entity<TextareaState>,
    /// 上次读到的配置版本号（`kv: ai_config_rev`）。变了就重读快照。
    pub(super) ai_revision: u64,
    /// 「AI」页正在编辑的厂商；`None` = 编辑器收起。
    pub(super) ai_draft: Option<AiDraft>,
    /// 知识库索引快照；`None` = 还没读过（没进过 AI 页）或上次读失败。
    pub(super) ai_kb: Option<KbSnapshot>,
    /// 读索引失败的原因（打不开 `ai.db` / 表损坏）。空 = 正常。
    pub(super) ai_kb_error: Option<SharedString>,
    /// 正在跑的一次索引任务；`None` = 空闲。
    pub(super) ai_kb_job: Option<KbJob>,
    /// 索引任务的轮询句柄。跑完即 drop，下次再起（幂等见 `start_kb_poller`）。
    pub(super) ai_kb_poll: Option<Task<()>>,
}

impl Render for SettingsWindowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_search_placeholder(window, cx);
        // 外部投递的跳转要先于分类过滤与正文渲染生效，避免闪一下旧页面。
        self.apply_pending_page(window, cx);
        let tokens = active_theme(cx).tokens().clone();
        let visible = self.visible_pages(cx);
        if !visible.iter().any(|page| self.selected == page.key)
            && let Some(first) = visible.first()
        {
            self.selected = SharedString::from(first.key);
        }
        let active = visible
            .iter()
            .find(|page| self.selected == page.key)
            .copied()
            .or_else(|| visible.first().copied());

        // 收起态不构造侧栏（搜索框与导航一起消失），内容区自然占满整宽。
        let sidebar = (!self.sidebar_collapsed).then(|| self.render_sidebar(&visible, cx));

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(tokens.colors.surface)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .items_stretch()
                    .children(sidebar)
                    .children(active.map(|page| self.render_content(page, window, cx))),
            )
    }
}

impl SettingsWindowView {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "搜索设置...")));
        cx.observe(&search, |_, _, cx| cx.notify()).detach();
        // 市场源：占位是 URL（与语言无关，不必随语言同步），初值取当前生效值。
        let market_source = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::market::source(cx))
                .default_value(crate::market::source(cx).to_string())
        });
        let font_select: Entity<FontSelectState> =
            cx.new(|cx| SelectState::new(font_options(cx), None, window, cx).searchable(true));
        let font_subscription = cx.subscribe(
            &font_select,
            |_, _, event: &SelectEvent<SearchableVec<FontOption>>, cx| {
                if let SelectEvent::Confirm(Some(value)) = event {
                    let family = if value.is_empty() {
                        None
                    } else {
                        Some(value.clone())
                    };
                    update_appearance(cx, move |appearance| appearance.font_family = family);
                }
            },
        );
        // 快捷键录入框：两个槽位各一份（焦点句柄 / 录入态 / 开关快照 / 失焦退出录入）。
        let hotkeys = HotkeySlot::ALL.map(|slot| HotkeyEntry::new(window, cx, slot));
        // AI 页的四个生成参数控件：初值取当前快照，之后只在「保存」时回写。
        let ai_settings = ai::load_settings(cx);
        let ai_temperature = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(cx, "接口默认"))
                .default_value(ai::temperature_text(&ai_settings))
        });
        let ai_max_tokens = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(cx, "接口默认"))
                .default_value(ai::max_tokens_text(&ai_settings))
        });
        let ai_max_rounds = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(cx, "默认 8"))
                .default_value(ai::max_rounds_text(&ai_settings))
        });
        let ai_compaction_budget = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(cx, "默认 96000"))
                .default_value(ai::compaction_budget_text(&ai_settings))
        });
        let ai_system_prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 12)
                .default_value(ai_settings.system_prompt.clone())
        });
        Self {
            search,
            selected: SharedString::from(PAGES[0].key),
            confirm_uninstall: settings::confirm_uninstall(cx),
            autostart_enabled: duck_platform::launch_at_login_enabled(),
            restore_windows: settings::restore_windows_on_launch(cx),
            icon_pack: SharedString::from("system"),
            font_select,
            font_synced: None,
            custom_accent_open: false,
            selected_tool: None,
            search_placeholder_lang: Some(active_theme(cx).lang()),
            sidebar_collapsed: false,
            logs_view: None,
            logs_summary: (0, 0),
            market_source,
            log_retention_days: crate::pages::logs::retention_days(cx),
            hotkeys,
            launcher_opens_main_window: settings::launcher_opens_main_window(cx),
            _font_subscription: font_subscription,
            // 审计页的筛选默认全空（= 最近一批记录）；带 trace 的跳转由
            // `apply_pending_page` 在渲染前填上。
            audit_kind: None,
            audit_problems_only: false,
            audit_trace: None,
            audit_keep: crate::audit::keep_limit(cx),
            audit_rows: Vec::new(),
            ai_providers: ai::load_providers(cx),
            ai_settings,
            ai_secure_available: ai::secure_storage_available(),
            ai_temperature,
            ai_max_tokens,
            ai_max_rounds,
            ai_compaction_budget,
            ai_system_prompt,
            ai_revision: ai::revision(cx),
            ai_draft: None,
            ai_kb: None,
            ai_kb_error: None,
            ai_kb_job: None,
            ai_kb_poll: None,
        }
    }

    /// 语言切换后同步搜索框占位文案。
    pub(super) fn sync_search_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = active_theme(cx).lang();
        if self.search_placeholder_lang == Some(lang) {
            return;
        }
        self.search_placeholder_lang = Some(lang);
        let placeholder = tr(cx, "搜索设置...");
        self.search.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx)
        });
    }

    /// 跳到指定分类，并按需选中「工具」页里的某个工具。
    ///
    /// 搜索框可能正把目标分类过滤掉——[`Self::visible_pages`] 会把选中项弹回首个可见分类，
    /// 于是「跳过去了但停在别的页」；所以跳转时一并清空搜索词。
    pub(super) fn focus_page(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if PAGES.iter().any(|page| page.key == target.page) {
            self.select_page(SharedString::from(target.page), cx);
        }
        if let Some(tool) = target.tool {
            self.selected_tool = Some(SharedString::from(tool));
        }
        self.search
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// 切换分类。进「审计日志」时顺带刷一次列表快照。
    ///
    /// 收口成一处是因为快照**不会自己变新**：两条切页路径（点侧栏、外部投递跳转）
    /// 只要有一条忘了刷，那一页就会显示上一次进来时的旧内容 —— 而那种错看起来
    /// 像是「审计没记下来」，比空列表更容易误导。
    pub(super) fn select_page(&mut self, key: SharedString, cx: &mut Context<Self>) {
        self.selected = key;
        // 审计页**每次**切到它都重查：它的内容是流水、快照不会自己变新，
        // 而「再点一下当前这个导航项」在审计页上就是很自然的「刷新」动作 ——
        // 对着选中项早退会把它变成一次空转。
        if self.selected == crate::audit::AUDIT_PAGE {
            self.reload_audit(cx);
        }
    }

    /// 取走外部投递的跳转请求（工具箱工作区「在设置中打开」）。
    ///
    /// 在 `render` 开头调用，且早于 `visible_pages` / 正文渲染：同一帧里就切到目标
    /// 分类，不会先闪一下旧页面。没有请求时零开销。
    pub(super) fn apply_pending_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 主窗的错误徽标投递的「只看这一次错误」：与分类跳转同一条路子，先取先得。
        // ⚠️ 即使跳转还没到（`Target` 为空），也要把 trace 取走 —— 否则它会一直挂在槽里，
        // 下一次用户自己点开审计页时莫名其妙地被筛上。
        if let Some(trace) = crate::audit::take_pending_trace(cx) {
            self.selected = SharedString::from(crate::audit::AUDIT_PAGE);
            self.audit_kind = None;
            self.audit_problems_only = false;
            self.audit_trace = Some(SharedString::from(trace));
            // 走 `select_page` 的话会在 `selected` 已经是审计页时提前返回 ——
            // 而这条路径**每次都要重查**（筛选刚被改成了「就这一次」）。
            self.reload_audit(cx);
        }
        if let Some(target) = Target::take(cx) {
            self.focus_page(target, window, cx);
        }
    }

    /// 搜索过滤后的可见分类；选中项被过滤掉时回落到第一个可见分类。
    ///
    /// 搜索按**当前语言**的标题与描述匹配（切到英文后可用 `theme` 搜到外观页）。
    pub(super) fn visible_pages(&self, cx: &Context<Self>) -> Vec<&'static SettingsPageSpec> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let lang = active_theme(cx).lang();
        let visible: Vec<&'static SettingsPageSpec> = PAGES
            .iter()
            .filter(|page| {
                query.is_empty()
                    || t(lang, page.title).to_lowercase().contains(&query)
                    || t(lang, page.description).to_lowercase().contains(&query)
            })
            .collect();
        visible
    }

    /// 左侧分类导航：搜索框 + **按分组**的导航列表（chrome 底 + hairline 右缘）。
    ///
    /// 分组只决定排版：每组一条组标题 + 一段 [`nav_list`]。列表本身不适合放非导航行
    ///（`duck-components/src/kit.rs:383`），所以组标题留在列表之外自己画。
    ///
    /// 分类切换的选中底是滑动高亮块（与 shell 活动栏同一动效），图标色随之淡变。
    /// 高亮块按**组内下标**定位，所以每组各用自己的 `nav_list` id。
    pub(super) fn render_sidebar(
        &self,
        pages: &[&'static SettingsPageSpec],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();

        let mut groups: Vec<AnyElement> = Vec::new();
        for group in PageGroup::ALL {
            let members: Vec<&'static SettingsPageSpec> = pages
                .iter()
                .copied()
                .filter(|page| page.group == group)
                .collect();
            // 搜索把整组过滤光时连组标题一起收起，不留一个空壳。
            if members.is_empty() {
                continue;
            }
            let items: Vec<NavItem> = members
                .iter()
                .map(|page| {
                    NavItem::new(
                        SharedString::from(format!("settings-nav-{}", page.key)),
                        SharedString::from(tr(cx, page.title)),
                        page.icon,
                    )
                })
                .collect();
            let selected = members.iter().position(|page| self.selected == page.key);
            let targets: Vec<&'static SettingsPageSpec> = members.clone();
            let this = cx.entity().downgrade();
            let nav = nav_list(
                SharedString::from(format!("settings-nav-{}", group.key())),
                items,
                selected,
                move |index, _, cx| {
                    let Some(page) = targets.get(index) else {
                        return;
                    };
                    let key = SharedString::from(page.key);
                    if let Ok(()) = this.update(cx, |this, cx| {
                        // 不判「是不是当前项」：审计页要能靠再点一下刷新，
                        // 而重绘一次本来就是点击该有的事（分类没变时零成本）。
                        this.select_page(key.clone(), cx);
                        cx.notify();
                    }) {}
                },
                cx,
            );
            groups.push(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xxs)
                    .child(
                        div()
                            .px(tokens.spacing.sm)
                            .pb(tokens.spacing.xxs)
                            .child(meta_text(tr(cx, group.title()), cx)),
                    )
                    .child(nav)
                    .into_any_element(),
            );
        }

        v_flex()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .min_h_0()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.md)
            .gap(tokens.spacing.md)
            .bg(extended.colors.chrome)
            .border_r_1()
            .border_color(extended.colors.hairline)
            .child(
                // `.control`：Medium 档字号 + 显式 control 高度（`Size::Size(28)` 会把
                // 文字字号派生为 28×0.875 ≈ 24.5px，占位文字溢出）。
                Input::new(&self.search).control(cx).prefix(
                    Icon::new(DuckIcon::Search)
                        .size(extended.icon.md)
                        .text_color(extended.colors.text_tertiary),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap(tokens.spacing.sm)
                    .overflow_y_scrollbar()
                    .children(groups),
            )
    }

    /// 右侧内容：页面标题区（hairline 底线）+ 内容区。
    ///
    /// 内容区默认套一层 y 滚动容器；**日志页是唯一例外**：查看器自己就要占满剩余高度、
    /// 在内部滚动（工具条固定、正文滚），外面再套一层会把它的正文挤成一个可以滚出视口的
    /// 孩子，自动跟随也就失去意义了。所以那一页直接整个交给查看器。
    pub(super) fn render_content(
        &mut self,
        page: &SettingsPageSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let lang = theme.lang();

        let body: AnyElement = if page.key == LOGS_PAGE {
            // 先把查看器叫出来（懒建发生在这里），它会把目录占用抄进 `logs_summary`；
            // 下面那条设置行读的就是这个快照，所以顺序不能反——反了第一帧会显示 0。
            let view = self.logs_view(window, cx);
            // 保留设置压在查看器上方，**不参与滚动**：它是一条配置，不是日志内容的一部分。
            // 左右留白与查看器工具条同源（`CONTENT_PADDING_*`），两者的左缘才对得上。
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .pl(px(CONTENT_PADDING_LEFT))
                        .pr(px(CONTENT_PADDING_RIGHT))
                        .pt(tokens.spacing.lg)
                        .pb(tokens.spacing.sm)
                        .child(self.logs_retention_section(cx)),
                )
                // 外层给 `flex_1` 的壳：查看器自己渲染成 `size_full`，
                // 直接当 `v_flex` 的第二个孩子会「100% + 标题高度」撑出父容器。
                .child(div().flex_1().min_h_0().w_full().child(view))
                .into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("settings-body-{}", page.key)))
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scrollbar()
                .pl(px(CONTENT_PADDING_LEFT))
                .pr(px(CONTENT_PADDING_RIGHT))
                .pt(tokens.spacing.lg + tokens.spacing.xs)
                .pb(tokens.spacing.xl)
                .child(self.render_page_body(page, window, cx))
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(tokens.colors.surface)
            .child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .px(px(CONTENT_PADDING_LEFT))
                    .pt(tokens.spacing.lg)
                    .pb(tokens.spacing.md)
                    .gap(tokens.spacing.xxs)
                    .border_b_1()
                    .border_color(extended.colors.hairline)
                    .child(page_heading(
                        t(lang, page.title),
                        t(lang, page.description),
                        cx,
                    )),
            )
            .child(body)
    }

    /// 分类内容分发。
    pub(super) fn render_page_body(
        &mut self,
        page: &SettingsPageSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        match page.key {
            "appearance" => self.appearance_body(window, cx),
            "behavior" => self.behavior_body(cx),
            "tools" => self.tools_body(window, cx),
            "plugins" => self.plugins_body(cx),
            AI_PAGE => self.ai_body(cx),
            "notify" => self.notify_body(cx),
            // 日志页的内容由 `render_content` 直接接管（它不套滚动容器，返回的也不是 `Div`）。
            // 留一个空臂而不是落到 `_`，是为了让人一眼看出「这里有意为之」而不是漏了分支。
            LOGS_PAGE => div(),
            // 数据页整块搬去了 `pages::data`：它只读 kv、不需要视图状态，
            // 留在这里只会让本就过长的本文件再胖一圈。
            DATA_PAGE => super::data::body(cx),
            // 模块页同理（`pages::module_settings`）：它们只读写 kv，不需要视图状态，
            // 拼成 `Div` 交给本页渲染即可。
            TERMINAL_PAGE => super::module_settings::terminal_body(cx),
            DATABASE_PAGE => super::module_settings::database_body(cx),
            MAIL_PAGE => super::module_settings::mail_body(cx),
            REDIS_PAGE => super::module_settings::redis_body(cx),
            PROJECTS_PAGE => super::module_settings::projects_body(cx),
            crate::audit::AUDIT_PAGE => self.audit_body(cx),
            // 更新相关的整块已并入「关于」页（原独立分类页已废除）。
            _ => self.about_body(cx),
        }
    }
}
