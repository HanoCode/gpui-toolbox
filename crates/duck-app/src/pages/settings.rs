//! 设置窗口：完全对照 FluxDown `SettingsView` 的结构 ——
//! 左侧「搜索 + 分类导航」，右侧「页面标题区 + 可滚动内容区」。
//!
//! 分类页内容沿用外观 / 行为 / 通知 / 关于四组设置；外观页按参考实现的宽视口
//! 双列分组卡片布局（左：主题与配色；右：界面）。所有改动即时生效，走
//! [`duck_theme::update_appearance`] 统一投影到两层主题。
//!
//! 「工具」页是**注册表驱动**的：左侧列出 `ToolRegistry` 里当前列出的工具，
//! 右侧按该工具的 [`ToolSettingSpec`] 逐项渲染，外加通用的启用开关与使用次数。
//! 新增工具设置不需要改本文件——加一条 `settings()` 就在界面上出现。
//!
//! 窗口为独立辅助窗口（`duck_shell::AuxiliaryWindowView`），已打开则前置聚焦。

use std::rc::Rc;

use duck_components::{
    ButtonVariant, ControlExt, DuckIcon, LOGO_ASSET_PATH, NavItem, beta_badge, body_text, button,
    content, dropdown_select, meta_text, nav_list, notify, option_row, option_row_stacked,
    settings_section, switch_control, title_text, toolbar_action_button,
};
use duck_shell::AuxiliaryWindowView;
use duck_theme::{
    Appearance, BuiltinThemeId, Density, FONT_SIZE_RANGE, FONT_SIZES, Lang, Locale, NAV_GAP_RANGE,
    SidebarSide, ThemeMode, ThemePreference, accent_from_rgb, active_theme, default_accent, t, tr,
    trf, update_appearance,
};
use duck_tools::ToolRegistry;
use duck_tools::settings as tool_settings;
use duck_tools::settings::{SettingValue, ToolSettingKind, ToolSettingSpec};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, Div, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, Role,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, Toggled, WeakEntity,
    Window, div, img, percentage, prelude::FluentBuilder as _, px, relative, size,
};
use gpui_component::{
    Icon, Root,
    button::Button,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex,
    input::{Input, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    tooltip::Tooltip,
    v_flex,
};

use crate::app::Desktop;
use crate::diagnostics;
use crate::pages::logs::LogView;
use crate::pages::status_support::{
    NumberConfirm, TextConfirm, format_bytes, open_number_prompt, open_text_prompt,
};
use crate::settings::{self, AuxWindow};
use crate::update;

/// 设置窗口初始尺寸：比主窗口（1180×760）小一档，视觉上从属于主窗口。
const SETTINGS_WINDOW_SIZE: gpui::Size<gpui::Pixels> = size(px(1000.), px(660.));
/// 设置窗口最小尺寸：仍能容纳 200px 分类导航 + 内容区左右留白。
const SETTINGS_WINDOW_MIN: gpui::Size<gpui::Pixels> = size(px(860.), px(560.));

/// 分类导航列宽（对齐参考实现 `SIDEBAR_WIDTH`）。
const SIDEBAR_WIDTH: f32 = 200.;
/// 内容区左右留白（对齐参考实现 `CONTENT_PADDING_LEFT/RIGHT`）。
/// 工具箱工作区与设置页共用同一节奏，`pub(crate)` 供 `pages::toolbox` 引用。
pub(crate) const CONTENT_PADDING_LEFT: f32 = 28.;
pub(crate) const CONTENT_PADDING_RIGHT: f32 = 24.;

/// 主题卡片宽度（对齐参考实现 `THEME_CARD_WIDTH`）。
const THEME_CARD_WIDTH: f32 = 120.;
/// 卡片内预览块高度。
const THEME_PREVIEW_HEIGHT: f32 = 52.;

/// 「关于」页 logo 的边长。
const ABOUT_LOGO_SIZE: f32 = 88.;
/// logo 圆角：应用图标本身是圆角方形，按比例取 88 的四分之一略弱。
const ABOUT_LOGO_RADIUS: f32 = 20.;
/// 品牌说明的最大宽度：再宽行长就不好读了（约 40 个汉字）。
const ABOUT_DESCRIPTION_MAX_WIDTH: f32 = 460.;
/// 预览块左侧栏宽度。
const THEME_PREVIEW_SIDEBAR_WIDTH: f32 = 28.;
/// 预览块内的示意条高度。
const THEME_PREVIEW_BAR_HEIGHT: f32 = 3.;
/// 强调色色点直径。
const COLOR_DOT_SIZE: f32 = 28.;

/// 可选强调色（预设色点）。首项即默认强调色，与 `duck_theme::DEFAULT_ACCENT_RGB` 同源。
const ACCENTS: [u32; 6] = [
    duck_theme::DEFAULT_ACCENT_RGB,
    0x8B5CF6,
    0x06B6D4,
    0x22C55E,
    0xF97316,
    0xEC4899,
];

/// 可选界面缩放档位。
const SCALES: [f32; 4] = [0.9, 1.0, 1.1, 1.25];

/// 可选侧栏项间距档位（px）。落在 `duck_theme::NAV_GAP_RANGE` 内。
const NAV_GAPS: [f32; 6] = [0., 4., 8., 12., 16., 24.];

/// 字号下拉中「跟随主题」项的值。
const FONT_SIZE_THEME: &str = "theme";

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
struct Target {
    page: &'static str,
    tool: Option<String>,
}

impl Target {
    /// 取走全局待处理槽里的跳转请求。
    fn take(cx: &mut App) -> Option<Self> {
        Desktop::take_settings_pending(cx).map(|(page, tool)| Self { page, tool })
    }
}

fn open_or_focus(cx: &mut App) {
    if let Some(handle) = *Desktop::settings_window(cx) {
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
    *Desktop::settings_window(cx) = None;

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
            *Desktop::settings_window(cx) = Some(handle);
            // 记下「这个窗口开着」，供下次启动恢复（见「设置 → 行为」的开关）。
            Desktop::remember_aux_open(cx);
            cx.activate(true);
        }
        Err(error) => crate::app::report_window_failure("设置窗口", &error, cx),
    }
}

/// 设置窗口标题栏左侧的侧栏折叠按钮。
///
/// 与主窗口 shell 的那个按钮同一规格（同一图标与旋转、同一提示语、同样拦住标题栏拖动）：
/// 同一个动作在两个窗口里不该长得不一样。
fn sidebar_toggle(view: WeakEntity<SettingsWindowView>, cx: &App) -> AnyElement {
    let label = SharedString::from(sidebar_label(cx, sidebar_collapsed(&view, cx)));
    let state = view.clone();
    div()
        .id("settings-sidebar-toggle-tooltip")
        .flex_none()
        // 标题栏整条是拖动区：不拦住左键按下，点按钮就变成拖窗口（对齐 shell 的处理）。
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        // 提示语在悬停时才求值：chrome 不随折叠状态重绘，取词不能定格在构建那一刻。
        .tooltip(move |window, cx| {
            Tooltip::new(SharedString::from(sidebar_label(
                cx,
                sidebar_collapsed(&view, cx),
            )))
            .build(window, cx)
        })
        .child(
            toolbar_action_button(
                "settings-sidebar-toggle",
                label,
                Icon::new(DuckIcon::PanelRight)
                    .size(active_theme(cx).extended().icon.md)
                    .rotate(percentage(0.5)),
                false,
                false,
                cx,
            )
            .on_click(move |_, _, cx| {
                if let Ok(()) = state.update(cx, |view, cx| {
                    view.sidebar_collapsed = !view.sidebar_collapsed;
                    cx.notify();
                }) {}
            }),
        )
        .into_any_element()
}

/// 读折叠状态（视图已释放时按展开算——那时按钮也没机会点了）。
fn sidebar_collapsed(view: &WeakEntity<SettingsWindowView>, cx: &App) -> bool {
    view.upgrade()
        .is_some_and(|view| view.read(cx).sidebar_collapsed)
}

/// 折叠按钮的提示语：收起时提示「展开侧栏」，反之亦然（源串即键）。
fn sidebar_label(cx: &App, collapsed: bool) -> &'static str {
    tr(
        cx,
        if collapsed {
            "展开侧栏"
        } else {
            "收起侧栏"
        },
    )
}

/// 日志清理结果的提示文案：删了就说删了几个，一个没删也明说（沉默会让人以为按钮没生效）。
fn pruned_message(removed: usize, cx: &App) -> SharedString {
    if removed == 0 {
        SharedString::from(tr(cx, "没有需要清理的日志。"))
    } else {
        SharedString::from(trf(cx, "已删除 {} 个过期日志。", &[&removed.to_string()]))
    }
}

/// 一个 MiB 的字节数。只用于把 [`duck_platform`] 的日志上限常量写成给人看的话。
const MIB: u64 = 1024 * 1024;

/// 分类页描述（导航与搜索索引共用）。
struct SettingsPageSpec {
    key: &'static str,
    title: &'static str,
    description: &'static str,
    icon: DuckIcon,
}

/// 日志分类的 key。
///
/// 它是唯一**内容区不套滚动容器**的分类（见 [`SettingsWindowView::render_content`]），
/// 规格表与渲染分支两处引用必须同源，别各写一遍字面量。
const LOGS_PAGE: &str = "logs";

/// 数据分类的 key（规格表与渲染分支同源）。
const DATA_PAGE: &str = "data";

/// 分类清单（顺序即导航顺序）。
const PAGES: &[SettingsPageSpec] = &[
    SettingsPageSpec {
        key: "appearance",
        title: "外观",
        description: "主题与配色",
        icon: DuckIcon::SlidersHorizontal,
    },
    SettingsPageSpec {
        key: "behavior",
        title: "行为",
        description: "列表、确认与开机启动",
        icon: DuckIcon::Zap,
    },
    SettingsPageSpec {
        key: "tools",
        title: "工具",
        description: "工具启停与工具级设置",
        icon: DuckIcon::Box,
    },
    SettingsPageSpec {
        key: "plugins",
        title: "插件",
        description: "市场源与已安装插件",
        icon: DuckIcon::Library,
    },
    SettingsPageSpec {
        key: "notify",
        title: "通知",
        description: "应用内与系统通知通道",
        icon: DuckIcon::Bell,
    },
    SettingsPageSpec {
        key: LOGS_PAGE,
        title: "日志",
        description: "日志文件与运行记录",
        icon: DuckIcon::FileText,
    },
    SettingsPageSpec {
        key: DATA_PAGE,
        title: "数据",
        description: "备份、恢复与重置",
        icon: DuckIcon::Archive,
    },
    SettingsPageSpec {
        key: "about",
        title: "关于",
        description: "版本、更新与发布记录",
        icon: DuckIcon::Info,
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
const TOOL_PICKER_WIDTH: f32 = 176.;

/// 工具列的一行（注册表里摊平出来的 `'static` 快照）。
///
/// 渲染期要一边读注册表一边持 `&mut Context` 挂回调，两者不能同时借用，
/// 所以先把要用的字段抄出来。
struct ToolPick {
    id: &'static str,
    name: &'static str,
    icon: DuckIcon,
    beta: bool,
    settings: &'static [ToolSettingSpec],
}

/// 「字体」下拉的可搜索选项：`value` 为字体族（空串 = 跟随主题字体）。
#[derive(Clone)]
struct FontOption {
    value: SharedString,
    label: SharedString,
}

impl SelectItem for FontOption {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

type FontSelectState = SelectState<SearchableVec<FontOption>>;

/// 枚举本机可用字体族（非空、排序、去重），首项为「使用主题字体」。
fn font_options(cx: &App) -> SearchableVec<FontOption> {
    let mut items = vec![FontOption {
        value: SharedString::from(""),
        label: SharedString::from(tr(cx, "使用主题字体")),
    }];
    let mut families = cx.text_system().all_font_names();
    families.retain(|name| !name.trim().is_empty());
    families.sort_unstable();
    families.dedup();
    items.extend(families.into_iter().map(|name| FontOption {
        value: SharedString::from(name.clone()),
        label: SharedString::from(name),
    }));
    SearchableVec::new(items)
}

/// 内置文件图标包（id, 标签）。
const ICON_PACKS: [(&str, &str); 4] = [
    ("system", "系统"),
    ("lucide", "简约"),
    ("material", "Material"),
    ("catppuccin", "Catppuccin"),
];

/// 图标包预览样例：图标 + 品牌色（`None` 跟随 `mutedForeground`）。
fn icon_pack_preview(pack: &str) -> Vec<(DuckIcon, Option<u32>)> {
    match pack {
        // 系统图标随平台变化，用一个硬盘图标示意（对齐参考实现）。
        "system" => vec![(DuckIcon::HardDrive, None)],
        "material" => vec![
            (DuckIcon::Film, Some(0xEF4444)),
            (DuckIcon::Archive, Some(0xF59E0B)),
            (DuckIcon::FileText, Some(0x3B82F6)),
            (DuckIcon::SquareTerminal, Some(0x22C55E)),
        ],
        // Catppuccin 柔和色板。
        "catppuccin" => vec![
            (DuckIcon::Film, Some(0xF38BA8)),
            (DuckIcon::Archive, Some(0xFAB387)),
            (DuckIcon::FileText, Some(0x89B4FA)),
            (DuckIcon::SquareTerminal, Some(0xA6E3A1)),
        ],
        _ => vec![
            (DuckIcon::Film, None),
            (DuckIcon::Archive, None),
            (DuckIcon::FileText, None),
            (DuckIcon::SquareTerminal, None),
        ],
    }
}

/// 设置窗口内容视图。
pub struct SettingsWindowView {
    search: Entity<InputState>,
    /// 当前选中分类的 key。
    selected: SharedString,
    /// 「卸载插件前弹确认」的快照（构造时从 `kv` 读一次，切换时同步落盘）。
    confirm_uninstall: bool,
    /// 「开机自动启动」当前是否生效（构造时从系统读一次，切换后按返回结果刷新；
    /// 系统登记是唯一事实来源，本字段只是快照）。
    autostart_enabled: bool,
    /// 「启动时恢复上次窗口」是否开启（构造时从 `kv` 读一次，切换时同步更新）。
    ///
    /// 缺省**关**：辅助窗口是「用户主动叫出来的」，开机时自动弹出一堆窗口是打扰。
    restore_windows: bool,
    /// 文件图标包（[`ICON_PACKS`] 的 id）。
    icon_pack: SharedString,
    /// 「字体」可搜索下拉（本机字体族枚举）。
    font_select: Entity<FontSelectState>,
    /// 上次同步进下拉的字体偏好，避免每帧重复 `set_selected_value`。
    font_synced: Option<SharedString>,
    /// 自定义强调色取色器是否展开（对齐参考实现：选中「自定义」时展开）。
    custom_accent_open: bool,
    /// 「工具」页选中的工具 id；`None` 表示跟随列表首项。
    selected_tool: Option<SharedString>,
    /// 搜索框占位文案使用的语言；语言切换后同步刷新（占位只在构建时定型）。
    search_placeholder_lang: Option<Lang>,
    /// 分类导航列是否收起（标题栏左侧的按钮切换）。
    ///
    /// 会话内保留即可：窗口关掉即释放，重开是新视图、默认展开。
    /// 不用 `SidebarPanel` 那套动画：设置窗口只有几个分类，收起是瞬时的事，
    /// 为此引入可拖宽分栏（会多一条拖拽热区）不划算。
    sidebar_collapsed: bool,
    /// 「日志」页的查看器。**懒建**：查看器自带每秒轮询的任务，没进过日志页就不该有它在跑；
    /// 建过一次就留着，离开再回来时选中的文件还在。
    logs_view: Option<Entity<LogView>>,
    /// 日志目录的 `(文件数, 总字节数)` 快照。
    ///
    /// 存在这里而不是每次渲染去 `read_dir`：查看器**本来**就每秒扫一遍目录，这里跟着它的
    /// `notify` 抄一份即可（见 [`Self::logs_view`] 的订阅）。渲染里做系统调用是另一回事。
    logs_summary: (usize, u64),
    /// 插件市场源输入框（「插件」页）。改动前从 `market` 读一次初值。
    market_source: Entity<InputState>,
    /// 日志保留天数（[`crate::pages::logs::RETENTION_CHOICES`] 里的一档，0 = 永久保留）。
    /// 构造时从 `kv` 读一次快照，改档位时同步更新。
    log_retention_days: u32,
    /// 「唤起快捷键」录入控件的焦点句柄（键帽框）。
    hotkey_focus: FocusHandle,
    /// 是否正在录入快捷键。`true` 时下一次按键被当成新键位。
    hotkey_recording: bool,
    /// 「唤起快捷键」开关的快照（构造时从 `kv` 读一次；切换失败时按真实值回摆）。
    hotkey_enabled: bool,
    /// 「唤起时打开主窗口」开关的快照（构造时从 `kv` 读一次）。
    launcher_opens_main_window: bool,
    /// 录入控件失焦时退出录入的订阅。**必须留着** —— drop 掉监听就没了。
    _hotkey_blur: Subscription,
    #[allow(dead_code)]
    _font_subscription: Subscription,
}

impl SettingsWindowView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        // 快捷键录入框：点到别处就退出录入。不做这一步的话，界面会一直停在
        // 「按下组合键…」，而按键早就跑到别的控件那里去了。
        let hotkey_focus = cx.focus_handle();
        let hotkey_blur = cx.on_blur(&hotkey_focus, window, |this, _, cx| {
            if this.hotkey_recording {
                this.hotkey_recording = false;
                cx.notify();
            }
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
            hotkey_focus,
            hotkey_recording: false,
            hotkey_enabled: settings::hotkey_enabled(cx),
            launcher_opens_main_window: settings::launcher_opens_main_window(cx),
            _hotkey_blur: hotkey_blur,
            _font_subscription: font_subscription,
        }
    }

    /// 「日志」页的查看器，按需创建（见字段注释）。
    ///
    /// 建好后订阅它：查看器每秒扫一次目录、长了就 `notify`，这里跟着把占用数字抄过来，
    /// 数字才不是「进这一页时的快照」。`notify` 只在真有变化时发（见
    /// `LogView::reload`），所以订阅不会让设置窗口每秒白重绘一次。
    fn logs_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<LogView> {
        if let Some(view) = &self.logs_view {
            return view.clone();
        }
        let view = cx.new(|cx| LogView::embedded(window, cx));
        cx.observe(&view, |this, view, cx| {
            let summary = view.read(cx).summary();
            if this.logs_summary != summary {
                this.logs_summary = summary;
                cx.notify();
            }
        })
        .detach();
        self.logs_summary = view.read(cx).summary();
        self.logs_view = Some(view.clone());
        view
    }

    /// 「日志」页顶部的保留设置：保留时长下拉 + 立即清理。
    ///
    /// 只出现在设置页宿主里；独立日志窗口保持「纯查看器」——那里没有配置的位置。
    fn logs_retention_section(&self, cx: &mut Context<Self>) -> Div {
        let options: Vec<(SharedString, SharedString)> = crate::pages::logs::RETENTION_CHOICES
            .iter()
            .map(|days| {
                let label = if *days == duck_platform::LOG_RETENTION_FOREVER {
                    SharedString::from(tr(cx, "永久保留"))
                } else {
                    SharedString::from(trf(cx, "{} 天", &[&days.to_string()]))
                };
                (SharedString::from(days.to_string()), label)
            })
            .collect();

        let this = cx.entity().downgrade();
        let retention_row = option_row(
            tr(cx, "保留时长"),
            Some(
                tr(
                    cx,
                    "超出保留期的日志会被自动删除；日志按天滚动，一天一个文件。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-log-retention",
                options,
                SharedString::from(self.log_retention_days.to_string()),
                move |value, window, cx| {
                    // 下拉回传的是档位的稳定值（天数字符串）；非法值理论上不会出现，丢掉即可。
                    let Ok(days) = value.as_ref().parse::<u32>() else {
                        return;
                    };
                    if !crate::pages::logs::RETENTION_CHOICES.contains(&days) {
                        return;
                    }
                    crate::pages::logs::set_retention_days(cx, days);
                    // 改成更短的保留期要立刻见效，所以当场按新值清一次：`kv_put` 是异步
                    // 落盘的，回读 `retention_days` 只会拿到旧值。
                    let removed = crate::pages::logs::prune_with(days);
                    if let Ok(()) = this.update(cx, |this, cx| {
                        this.log_retention_days = days;
                        this.refresh_logs_view(cx);
                        cx.notify();
                    }) {}
                    duck_components::notify::success(pruned_message(removed, cx), window, cx);
                },
                cx,
            ),
            cx,
        );

        let clean_row = option_row(
            tr(cx, "立即清理"),
            Some(tr(cx, "按上面的保留时长删掉旧日志，不等下一次自动清理。").into()),
            button(
                "duck-settings-log-prune",
                tr(cx, "清理"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                let removed = crate::pages::logs::prune_with(this.log_retention_days);
                this.refresh_logs_view(cx);
                duck_components::notify::success(pruned_message(removed, cx), window, cx);
            })),
            cx,
        );

        // 下面两项只读：两个数字都是**已生效的**兜底值，不是可调的设置。
        // 摆出来是因为「按天滚动」听起来无害，实际单日、整目录各有一条上限；
        // 不写出来，用户只能靠磁盘满了才发现。
        let segments = duck_platform::LOG_SEGMENTS_PER_DAY.to_string();
        let directory_cap = format!("{} MB", duck_platform::LOG_DIR_MAX_BYTES / MIB);
        let per_file_row = option_row(
            tr(cx, "单文件上限"),
            Some(
                trf(
                    cx,
                    "写满就轮转到下一段；一天最多留 {} 段，目录总量兜底 {}。",
                    &[&segments, &directory_cap],
                )
                .into(),
            ),
            meta_text(
                format!("{} MB", duck_platform::LOG_FILE_MAX_BYTES / MIB),
                cx,
            ),
            cx,
        );

        let (files, bytes) = self.logs_summary;
        let usage_row = option_row(
            tr(cx, "当前占用"),
            Some(tr(cx, "日志目录里所有 .log 的总大小。").into()),
            meta_text(
                trf(
                    cx,
                    "{} 个文件 · {}",
                    &[&files.to_string(), &format_bytes(bytes)],
                ),
                cx,
            ),
            cx,
        );

        settings_section(
            Some(tr(cx, "日志保留").into()),
            [
                retention_row.into_any_element(),
                clean_row.into_any_element(),
                per_file_row.into_any_element(),
                usage_row.into_any_element(),
            ],
            cx,
        )
    }

    /// 让日志查看器立刻重扫目录：清理刚删掉的文件不该在列表里留到下一次轮询。
    ///
    /// 查看器是**懒建**的（见字段注释）：没进过日志页就没有它，也就没有要刷的东西。
    fn refresh_logs_view(&self, cx: &mut Context<Self>) {
        if let Some(view) = &self.logs_view {
            view.update(cx, |view, cx| view.rescan(cx));
        }
    }

    /// 语言切换后同步搜索框占位文案。
    fn sync_search_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    fn focus_page(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if PAGES.iter().any(|page| page.key == target.page) {
            self.selected = SharedString::from(target.page);
        }
        if let Some(tool) = target.tool {
            self.selected_tool = Some(SharedString::from(tool));
        }
        self.search
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// 取走外部投递的跳转请求（工具箱工作区「在设置中打开」）。
    ///
    /// 在 `render` 开头调用，且早于 `visible_pages` / 正文渲染：同一帧里就切到目标
    /// 分类，不会先闪一下旧页面。没有请求时零开销。
    fn apply_pending_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = Target::take(cx) {
            self.focus_page(target, window, cx);
        }
    }

    /// 搜索过滤后的可见分类；选中项被过滤掉时回落到第一个可见分类。
    ///
    /// 搜索按**当前语言**的标题与描述匹配（切到英文后可用 `theme` 搜到外观页）。
    fn visible_pages(&self, cx: &Context<Self>) -> Vec<&'static SettingsPageSpec> {
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

    /// 左侧分类导航：搜索框 + 导航列表（chrome 底 + hairline 右缘）。
    ///
    /// 分类切换的选中底是滑动高亮块（与 shell 活动栏同一动效），图标色随之淡变。
    fn render_sidebar(
        &self,
        pages: &[&'static SettingsPageSpec],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();

        let items: Vec<NavItem> = pages
            .iter()
            .map(|page| {
                NavItem::new(
                    SharedString::from(format!("settings-nav-{}", page.key)),
                    SharedString::from(tr(cx, page.title)),
                    page.icon,
                )
            })
            .collect();
        let selected = pages.iter().position(|page| self.selected == page.key);
        let targets: Vec<&'static SettingsPageSpec> = pages.to_vec();
        let this = cx.entity().downgrade();
        let nav = nav_list(
            "settings-nav",
            items,
            selected,
            move |index, _, cx| {
                let Some(page) = targets.get(index) else {
                    return;
                };
                let key = SharedString::from(page.key);
                if let Ok(()) = this.update(cx, |this, cx| {
                    if this.selected != key {
                        this.selected = key.clone();
                        cx.notify();
                    }
                }) {}
            },
            cx,
        );

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
                    .overflow_y_scrollbar()
                    .child(nav),
            )
    }

    /// 右侧内容：页面标题区（hairline 底线）+ 内容区。
    ///
    /// 内容区默认套一层 y 滚动容器；**日志页是唯一例外**：查看器自己就要占满剩余高度、
    /// 在内部滚动（工具条固定、正文滚），外面再套一层会把它的正文挤成一个可以滚出视口的
    /// 孩子，自动跟随也就失去意义了。所以那一页直接整个交给查看器。
    fn render_content(
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
    fn render_page_body(
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
            "notify" => self.notify_body(cx),
            // 日志页的内容由 `render_content` 直接接管（它不套滚动容器，返回的也不是 `Div`）。
            // 留一个空臂而不是落到 `_`，是为了让人一眼看出「这里有意为之」而不是漏了分支。
            LOGS_PAGE => div(),
            // 数据页整块搬去了 `pages::data`：它只读 kv、不需要视图状态，
            // 留在这里只会让本就过长的本文件再胖一圈。
            DATA_PAGE => super::data::body(cx),
            // 更新相关的整块已并入「关于」页（原独立分类页已废除）。
            _ => self.about_body(cx),
        }
    }

    // ── 工具：左侧工具列 + 右侧该工具的设置 ────────────────────────────
    fn tools_body(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let colors = tokens.colors;
        let lang = theme.lang();

        // 当前列出的工具（含已停用，不含未开开发者模式的 dev 工具）。
        let listed: Vec<ToolPick> = ToolRegistry::listed(cx)
            .into_iter()
            .map(|tool| {
                let meta = tool.meta();
                ToolPick {
                    id: tool.id(),
                    name: meta.name(),
                    icon: meta.icon(),
                    beta: meta.is_beta(),
                    settings: meta.settings(),
                }
            })
            .collect();

        // 选中项失效（工具被停用 / 开发者模式关掉）时回落列表首项。
        let active_id: &'static str = self
            .selected_tool
            .as_ref()
            .and_then(|id| listed.iter().find(|pick| pick.id == id.as_ref()))
            .map_or_else(|| listed.first().map_or("", |pick| pick.id), |pick| pick.id);

        // 左列：可点选的工具列表。
        let this = cx.entity().downgrade();
        let tool_count = listed.len();
        let picker: Vec<AnyElement> =
            listed
                .iter()
                .enumerate()
                .map(|(index, pick)| {
                    let id = pick.id;
                    let selected = id == active_id;
                    let this = this.clone();
                    // 焦点环色要 `move` 进闭包 → 先拷出来（`Hsla` 是 `Copy`）。
                    let row_ring = colors.ring;
                    // 可访问名用**显式** `aria_label`、复用下面那句可见文案：一是别让读屏
                    // 依赖「从子节点文字派生名字」这条本仓库无从验证的假设，二是
                    // `gpui_base::test_support` 的 `label()` 只读节点上的显式 label
                    // （`test_support.rs:318`），不显式给就断言不了。
                    let tool_label = SharedString::from(t(lang, pick.name));
                    h_flex()
                        .id(SharedString::from(format!("settings-tool-{id}")))
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.xs)
                        .px(tokens.spacing.sm)
                        .py(tokens.spacing.xs)
                        .rounded(theme.components().button_radius)
                        // 无障碍：GPUI 的 a11y 树**只收录带 role 的节点**，没有 role 就等于
                        // 对读屏完全不存在。这一列与 `kit::segmented_tabs` / `kit::nav_list`
                        // 同构（一组「选一个」的导航项），角色对齐 `Tab`，容器在渲染处补
                        // `TabList`。名字见上面 `tool_label`；位置/总数让读屏能播报
                        // 「第 M / N 项」。
                        .role(Role::Tab)
                        .aria_selected(selected)
                        .aria_label(tool_label.clone())
                        .aria_position_in_set(index + 1)
                        .aria_size_of_set(tool_count)
                        .focusable()
                        .tab_stop(true)
                        // 焦点用**描边**而不是底色（与 `navigation_row` / `activity_button`
                        // 同款）。先占 1px 透明边，描边出现时不引起位移。
                        .border_1()
                        .border_color(gpui::transparent_black())
                        .focus_visible(move |style| style.border_color(row_ring))
                        .cursor_pointer()
                        .when(selected, |row| row.bg(colors.muted))
                        .when(!selected, |row| {
                            row.hover(|style| style.bg(extended.colors.hover_tint))
                        })
                        .on_click(move |_, _, cx| {
                            let next = SharedString::from(id);
                            if let Ok(()) = this.update(cx, |this, cx| {
                                if this.selected_tool.as_ref() != Some(&next) {
                                    this.selected_tool = Some(next);
                                    cx.notify();
                                }
                            }) {}
                        })
                        .child(Icon::new(pick.icon).size(extended.icon.md).text_color(
                            if selected {
                                colors.primary
                            } else {
                                colors.muted_foreground
                            },
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .text_color(colors.foreground)
                                .child(tool_label),
                        )
                        .when(pick.beta, |row| row.child(beta_badge(cx)))
                        .into_any_element()
                })
                .collect();

        // 右列：选中工具的通用项 + 该工具的设置项。
        let mut sections: Vec<AnyElement> = Vec::new();
        if let Some(pick) = listed.iter().find(|pick| pick.id == active_id) {
            let tool_id = pick.id;
            let enabled = tool_settings::is_enabled(cx, tool_id);
            sections.push(
                settings_section(
                    Some(tr(cx, "通用").into()),
                    [
                        option_row(
                            tr(cx, "启用"),
                            Some(tr(cx, "停用后从工具箱列表隐藏，设置项保留。").into()),
                            switch_control(
                                SharedString::from(format!("settings-tool-enabled-{tool_id}")),
                                enabled,
                                move |value, _, cx| {
                                    tool_settings::set_enabled(cx, tool_id, *value);
                                    cx.refresh_windows();
                                },
                            ),
                            cx,
                        )
                        .into_any_element(),
                        option_row(
                            tr(cx, "使用次数"),
                            Some(tr(cx, "每次从工具箱运行该工具后累加。").into()),
                            body_text(tool_settings::runs(cx, tool_id).to_string(), cx)
                                .into_any_element(),
                            cx,
                        )
                        .into_any_element(),
                    ],
                    cx,
                )
                .into_any_element(),
            );

            let rows: Vec<AnyElement> = pick
                .settings
                .iter()
                .map(|spec| {
                    let control = tool_setting_control(tool_id, spec, cx);
                    let description = (!spec.description().is_empty())
                        .then(|| SharedString::from(t(lang, spec.description())));
                    // 下拉自带宽度，横排在窄窗口下会被挤扁，统一走竖排。
                    let stacked = matches!(spec.kind(), ToolSettingKind::Select { .. });
                    if stacked {
                        option_row_stacked(t(lang, spec.label()), description, control, cx)
                    } else {
                        option_row(t(lang, spec.label()), description, control, cx)
                    }
                    .into_any_element()
                })
                .collect();

            sections.push(
                if rows.is_empty() {
                    settings_section(
                        Some(t(lang, "设置项").into()),
                        [meta_text(tr(cx, "该工具暂无独立设置。"), cx).into_any_element()],
                        cx,
                    )
                } else {
                    settings_section(Some(t(lang, "设置项").into()), rows, cx)
                }
                .into_any_element(),
            );
        } else {
            sections.push(meta_text(tr(cx, "当前没有可配置的工具。"), cx).into_any_element());
        }

        // 开发者模式是全局开关，与具体工具无关，单独一节放在最后。
        let dev_mode = ToolRegistry::dev_mode(cx);
        sections.push(
            settings_section(
                Some(tr(cx, "开发者").into()),
                [option_row(
                    tr(cx, "开发者模式"),
                    Some(tr(cx, "显示标记为开发者的工具，仅在排查问题时使用。").into()),
                    switch_control("settings-tool-dev-mode", dev_mode, move |value, _, cx| {
                        ToolRegistry::set_dev_mode(cx, *value);
                        cx.refresh_windows();
                    }),
                    cx,
                )
                .into_any_element()],
                cx,
            )
            .into_any_element(),
        );

        h_flex()
            .w_full()
            .items_start()
            .gap(tokens.spacing.lg)
            .child(
                v_flex()
                    .id("settings-tool-list")
                    // 行按 `Role::Tab` 报，容器就要是 `TabList`（与 `kit::segmented_tabs`
                    // / `kit::nav_list` 一致），读屏才会把它当一组标签而不是一串孤立按钮。
                    .role(Role::TabList)
                    .flex_none()
                    .w(px(TOOL_PICKER_WIDTH))
                    .gap(tokens.spacing.xxs)
                    .children(picker),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(tokens.spacing.lg)
                    .children(sections),
            )
    }

    // ── 外观：单列纵向（主题与配色 → 界面）──────────────────────────────
    fn appearance_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();
        let lang = active_theme(cx).lang();

        // 左列：主题与配色。
        let appearance = active_theme(cx).appearance().clone();
        let mode = active_theme(cx).mode();

        let mode_options = [
            ("system", tr(cx, "跟随系统")),
            ("light", tr(cx, "亮色")),
            ("dark", tr(cx, "暗色")),
        ]
        .map(|(value, label)| (SharedString::from(value), SharedString::from(label)));
        let mode_current = appearance.preference.wire_name();
        let theme_mode_row = option_row(
            tr(cx, "主题模式"),
            Some(tr(cx, "跟随系统时随 macOS 外观切换，切换即时生效。").into()),
            dropdown_select(
                "duck-settings-theme-mode",
                mode_options,
                mode_current,
                |value, _, cx| {
                    let preference = match value.as_ref() {
                        "light" => ThemePreference::Light,
                        "dark" => ThemePreference::Dark,
                        _ => ThemePreference::System,
                    };
                    update_appearance(cx, move |appearance| appearance.preference = preference);
                },
                cx,
            ),
            cx,
        );

        let slot_label = match mode {
            ThemeMode::Dark => tr(cx, "暗色主题"),
            ThemeMode::Light => tr(cx, "亮色主题"),
        };
        let cards: Vec<gpui::AnyElement> = BuiltinThemeId::presets_for(mode)
            .map(|id| {
                theme_card(id, appearance.theme.for_mode(mode) == id, &appearance, cx)
                    .into_any_element()
            })
            .collect();
        let theme_cards_row = option_row_stacked(
            tr(cx, "主题"),
            None,
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(meta_text(slot_label, cx))
                .child(
                    h_flex()
                        .id("duck-theme-card-list")
                        // 卡片按 `Role::RadioButton` + `aria_toggled` 报，容器配 `RadioGroup`。
                        .role(Role::RadioGroup)
                        .gap(tokens.spacing.sm)
                        .flex_wrap()
                        .children(cards),
                )
                .child(theme_actions(cx)),
            cx,
        );

        let preset_selected = ACCENTS
            .iter()
            .any(|value| accent_from_rgb(*value) == appearance.accent);
        let custom_selected = self.custom_accent_open && !preset_selected;
        let custom_icon_size = active_theme(cx).extended().icon.md;
        let custom_ring = tokens.colors.ring;
        let custom_dot = div()
            .id("duck-accent-dot-custom")
            .size(px(COLOR_DOT_SIZE))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(appearance.accent)
            .border_2()
            .border_color(if custom_selected {
                tokens.colors.foreground
            } else {
                appearance.accent
            })
            // 无障碍：同预设色点（无 role 就不进 a11y 树）；子节点只有一个图标，
            // 没有文字可派生名字，所以显式给——复用自定义取色器那句中英对照文案。
            .role(Role::RadioButton)
            .aria_toggled(if custom_selected {
                Toggled::True
            } else {
                Toggled::False
            })
            .aria_label(tr(cx, "自定义强调色"))
            .focusable()
            .tab_stop(true)
            // 同 `color_dot`：选中态描边是 `foreground`，焦点 `ring` 与之错开。
            .focus_visible(move |style| style.border_color(custom_ring))
            .cursor_pointer()
            .when(!custom_selected, |this| {
                // 色点语义：悬停加深自身描边到正文色，够明显的对比环。
                this.hover(move |style| style.border_color(tokens.colors.foreground))
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.custom_accent_open = !this.custom_accent_open;
                cx.notify();
            }))
            .child(
                Icon::new(DuckIcon::SlidersHorizontal)
                    .size(custom_icon_size)
                    .text_color(duck_theme::foreground_for(appearance.accent)),
            );

        let dots = h_flex()
            .id("duck-accent-dot-list")
            // 预设色点也是「选一个」，与主题卡片同构 → 同样的单选组语义。
            .role(Role::RadioGroup)
            .gap(tokens.spacing.sm)
            .flex_wrap()
            .children(ACCENTS.iter().enumerate().map(|(index, value)| {
                let color = accent_from_rgb(*value);
                let selected = color == appearance.accent
                    || (appearance.accent == default_accent()
                        && *value == duck_theme::DEFAULT_ACCENT_RGB);
                color_dot(
                    index,
                    color,
                    selected,
                    selected.then_some(DuckIcon::Check),
                    cx,
                )
                .into_any_element()
            }))
            .child(custom_dot);
        let mut accent_column = v_flex().w_full().gap(tokens.spacing.sm).child(dots);
        if self.custom_accent_open {
            let picker = accent_picker(window, cx, appearance.accent);
            accent_column = accent_column.child(
                // 不传 featured colors：预设色已由上方色点提供，避免重复的 a11y 色块 id。
                ColorPicker::new(&picker)
                    .label(tr(cx, "自定义强调色"))
                    .featured_colors(Vec::new()),
            );
        }
        let theme_color_row = option_row_stacked(
            tr(cx, "主题色"),
            Some(tr(cx, "同时派生选中导航图标色与链接文字色。").into()),
            accent_column,
            cx,
        );

        // 文件图标：图标包选择卡片（预览样例文件图标，与主题相互独立）。
        let pack_cards: Vec<AnyElement> = ICON_PACKS
            .iter()
            .map(|(id, label)| {
                icon_pack_card(
                    id,
                    SharedString::from(tr(cx, label)),
                    self.icon_pack == *id,
                    cx.listener(move |this, _, _, cx| {
                        if this.icon_pack != *id {
                            this.icon_pack = SharedString::from(*id);
                            cx.notify();
                        }
                    }),
                    cx,
                )
                .into_any_element()
            })
            .collect();
        let icon_pack_row = option_row_stacked(
            tr(cx, "文件图标"),
            Some(tr(cx, "任务列表中的图标风格，与主题相互独立。").into()),
            h_flex()
                .id("duck-icon-pack-list")
                // 同主题卡片：一组「选一个」。
                .role(Role::RadioGroup)
                .gap(tokens.spacing.sm)
                .flex_wrap()
                .children(pack_cards),
            cx,
        );

        let theme_section = settings_section(
            Some(tr(cx, "主题与配色").into()),
            [
                theme_mode_row.into_any_element(),
                theme_cards_row.into_any_element(),
                theme_color_row.into_any_element(),
                icon_pack_row.into_any_element(),
            ],
            cx,
        );

        // 语言：与其它外观项一样直接读写 `Appearance.language`（无中间占位状态）。
        let language_options: Vec<(SharedString, SharedString)> = Locale::ALL
            .iter()
            .map(|locale| {
                (
                    SharedString::from(locale.wire_name()),
                    SharedString::from(t(lang, locale.label())),
                )
            })
            .collect();
        let language_row = option_row(
            tr(cx, "语言"),
            Some(tr(cx, "界面显示语言；切换后立即生效，界面文案与提示都会跟随。").into()),
            dropdown_select(
                "duck-settings-language",
                language_options,
                appearance.language.wire_name(),
                |value, _, cx| {
                    update_appearance(cx, move |appearance| {
                        appearance.language = Locale::from_wire(&value);
                    });
                },
                cx,
            ),
            cx,
        );
        // 语言并入「界面」分组，整页单列纵向排列。

        // 右列：界面。
        let scale_options: Vec<(SharedString, SharedString)> = SCALES
            .iter()
            .map(|scale| {
                let percent = (scale * 100.).round() as i32;
                (
                    SharedString::from(percent.to_string()),
                    SharedString::from(format!("{percent}%")),
                )
            })
            .collect();
        let scale_current = SharedString::from((appearance.ui_scale * 100.).round().to_string());
        let scale_row = option_row(
            tr(cx, "界面缩放"),
            Some(tr(cx, "缩放尺寸、间距、图标与文字档位。").into()),
            dropdown_select(
                "duck-settings-ui-scale",
                scale_options,
                scale_current,
                |value, _, cx| {
                    let Ok(percent) = value.parse::<f32>() else {
                        return;
                    };
                    update_appearance(cx, move |appearance| {
                        appearance.ui_scale = (percent / 100.).clamp(0.8, 1.5);
                    });
                },
                cx,
            ),
            cx,
        );

        // 界面密度：与「界面缩放」是两个独立的轴 —— 缩放是等比放大，密度是改变档位关系
        // （紧凑档下侧栏行高比控件更紧）。二者叠加生效。
        let density_options: Vec<(SharedString, SharedString)> = Density::ALL
            .iter()
            .map(|density| {
                (
                    SharedString::from(density.wire_name()),
                    SharedString::from(tr(cx, density.label())),
                )
            })
            .collect();
        let density_row = option_row(
            tr(cx, "界面密度"),
            Some(
                tr(
                    cx,
                    "控件高度与行高的疏密档位，与界面缩放叠加生效；「紧凑」同屏显示更多内容，「宽松」更易点选。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-density",
                density_options,
                appearance.density.wire_name(),
                |value, _, cx| {
                    let density = Density::from_wire_name(&value);
                    update_appearance(cx, move |appearance| appearance.density = density);
                },
                cx,
            ),
            cx,
        );

        let mut font_options = vec![(
            SharedString::from(FONT_SIZE_THEME),
            SharedString::from(tr(cx, "跟随主题")),
        )];
        font_options.extend(FONT_SIZES.iter().map(|font_size| {
            (
                SharedString::from(font_size.to_string()),
                SharedString::from(format!("{} px", *font_size as i32)),
            )
        }));
        let font_current = appearance.font_size.map_or_else(
            || SharedString::from(FONT_SIZE_THEME),
            |font_size| SharedString::from(font_size.to_string()),
        );
        let font_row = option_row(
            tr(cx, "字体大小"),
            Some(
                tr(
                    cx,
                    "本机正文字号；其余文字与行高按同一比例适配，与界面缩放叠加生效。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-font-size",
                font_options,
                font_current,
                |value, _, cx| {
                    let (min, max) = FONT_SIZE_RANGE;
                    let next = value
                        .parse::<f32>()
                        .ok()
                        .filter(|font_size| (min..=max).contains(font_size));
                    update_appearance(cx, move |appearance| appearance.font_size = next);
                },
                cx,
            ),
            cx,
        );

        // 字体：本机字体族可搜索下拉；确认选择即写外观偏好（空值 = 跟随主题字体）。
        let current_family = appearance.font_family.clone().unwrap_or_default();
        if self.font_synced.as_ref() != Some(&current_family) {
            self.font_synced = Some(current_family.clone());
            self.font_select.update(cx, |select, cx| {
                select.set_selected_value(&current_family, window, cx);
            });
        }
        let font_family_row = option_row(
            tr(cx, "字体"),
            Some(tr(cx, "选择此电脑上可用的字体。字体不可用时回退到主题字体；新安装的字体可能需要重启应用后才能读取。").into()),
            Select::new(&self.font_select)
                .placeholder(tr(cx, "使用主题字体"))
                .w(px(240.))
                .max_w_full()
                .h(active_theme(cx).density().control)
                .py_0(),
            cx,
        );

        let interface_section = settings_section(
            Some(tr(cx, "界面").into()),
            [
                language_row.into_any_element(),
                scale_row.into_any_element(),
                density_row.into_any_element(),
                font_family_row.into_any_element(),
                font_row.into_any_element(),
            ],
            cx,
        );

        // 布局：改的是**结构**（右栏停靠侧、间距多大、哪些 chrome 显示），不是文字与尺寸。
        // 与「界面」分组的边界是：凡改变「有没有 / 在哪」的项进这里，改「多大 / 多密」的进界面。
        //
        // ⚠️ 命名必须点明「右栏」：窗口左侧还有活动栏与页面导航，笼统的「侧栏位置」会被
        // 理解成挪那些东西（2026-10-11 用户实测报告「侧栏调整不生效」，根因即此歧义 +
        // 效果只在工具箱页可见）。描述里必须写明作用范围。
        let side_options: Vec<(SharedString, SharedString)> =
            [SidebarSide::Left, SidebarSide::Right]
                .into_iter()
                .map(|side| {
                    (
                        SharedString::from(side.wire_name()),
                        SharedString::from(tr(cx, side.label())),
                    )
                })
                .collect();
        let side_row = option_row(
            tr(cx, "右栏位置"),
            Some(
                tr(
                    cx,
                    "内容区右栏停靠在哪一侧；目前仅工具箱页提供右栏内容，切换即时生效。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-sidebar-side",
                side_options,
                appearance.right_sidebar_side.wire_name(),
                |value, _, cx| {
                    let side = SidebarSide::from_wire_name(&value);
                    update_appearance(cx, move |appearance| appearance.right_sidebar_side = side);
                },
                cx,
            ),
            cx,
        );

        let nav_gap_options: Vec<(SharedString, SharedString)> = NAV_GAPS
            .iter()
            .map(|gap| {
                (
                    SharedString::from(gap.to_string()),
                    SharedString::from(format!("{} px", *gap as i32)),
                )
            })
            .collect();
        let nav_gap_current = SharedString::from(appearance.nav_gap.round().to_string());
        let nav_gap_row = option_row(
            tr(cx, "侧栏项间距"),
            Some(tr(cx, "工具箱页侧栏条目之间的留白（0 为贴紧）。").into()),
            dropdown_select(
                "duck-settings-nav-gap",
                nav_gap_options,
                nav_gap_current,
                |value, _, cx| {
                    let Ok(gap) = value.parse::<f32>() else {
                        return;
                    };
                    let (min, max) = NAV_GAP_RANGE;
                    update_appearance(cx, move |appearance| {
                        appearance.nav_gap = gap.clamp(min, max);
                    });
                },
                cx,
            ),
            cx,
        );

        let show_activity_bar_row = option_row(
            tr(cx, "显示活动栏"),
            Some(
                tr(
                    cx,
                    "窗口最左侧的功能轨（概览 / 工具箱 / 设置…）。关闭后内容区占满窗口。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-show-activity-bar",
                appearance.show_activity_bar,
                |value, _, cx| {
                    update_appearance(cx, |appearance| appearance.show_activity_bar = *value);
                },
            ),
            cx,
        );

        let show_notification_bell_row = option_row(
            tr(cx, "显示通知铃铛"),
            Some(
                tr(
                    cx,
                    "顶栏的通知中心入口；关闭后通知仍记入历史，只是顶栏不再显示铃铛。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-show-notification-bell",
                appearance.show_notification_bell,
                |value, _, cx| {
                    update_appearance(cx, |appearance| {
                        appearance.show_notification_bell = *value;
                    });
                },
            ),
            cx,
        );

        let show_top_bar_extras_row = option_row(
            tr(cx, "显示顶栏附加入口"),
            Some(tr(cx, "顶栏的插件中心按钮与语言切换；关闭后二者一并隐藏。").into()),
            switch_control(
                "duck-settings-show-top-bar-extras",
                appearance.show_top_bar_extras,
                |value, _, cx| {
                    update_appearance(cx, |appearance| appearance.show_top_bar_extras = *value);
                },
            ),
            cx,
        );

        let layout_section = settings_section(
            Some(tr(cx, "布局").into()),
            [
                side_row.into_any_element(),
                nav_gap_row.into_any_element(),
                show_activity_bar_row.into_any_element(),
                show_notification_bell_row.into_any_element(),
                show_top_bar_extras_row.into_any_element(),
            ],
            cx,
        );

        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(theme_section)
            .child(interface_section)
            .child(layout_section)
    }

    // ── 行为 ─────────────────────────────────────────────────────────────
    fn behavior_body(&self, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();
        let confirm_row = option_row(
            tr(cx, "卸载插件前弹确认"),
            Some(tr(cx, "关掉后点「卸载」会直接开始，不再询问。").into()),
            switch_control(
                "duck-settings-confirm-uninstall",
                self.confirm_uninstall,
                cx.listener(|this, next: &bool, _, cx| {
                    this.confirm_uninstall = *next;
                    settings::set_confirm_uninstall(cx, *next);
                    cx.notify();
                }),
            ),
            cx,
        );
        let startup_section = settings_section(
            Some(tr(cx, "启动").into()),
            [
                option_row(
                    tr(cx, "开机自动启动"),
                    Some(tr(cx, "登录系统后自动运行 Duck。").into()),
                    switch_control(
                        "duck-settings-autostart",
                        self.autostart_enabled,
                        cx.listener(|this, next: &bool, window, cx| {
                            // 先按目标值落状态，失败再按系统真实值回摆——开关不闪两次。
                            this.autostart_enabled = *next;
                            if let Err(error) = duck_platform::set_launch_at_login(*next) {
                                this.autostart_enabled = duck_platform::launch_at_login_enabled();
                                duck_components::notify::error(
                                    trf(cx, "开机自启设置失败：{}", &[&error.to_string()]),
                                    window,
                                    cx,
                                );
                            }
                            cx.notify();
                        }),
                    ),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "启动时恢复上次窗口"),
                    Some(
                        tr(
                            cx,
                            "把上次退出时开着的设置、日志等窗口一并打开，各自回到原来的位置。",
                        )
                        .into(),
                    ),
                    switch_control(
                        "duck-settings-restore-windows",
                        self.restore_windows,
                        cx.listener(|this, next: &bool, _, cx| {
                            this.restore_windows = *next;
                            settings::set_restore_windows_on_launch(cx, *next);
                            cx.notify();
                        }),
                    ),
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        );
        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(settings_section(
                Some(tr(cx, "行为").into()),
                [confirm_row.into_any_element()],
                cx,
            ))
            .child(self.hotkey_section(cx))
            .child(startup_section)
    }

    // ── 唤起（全局快捷键）───────────────────────────────────────────────
    fn hotkey_section(&self, cx: &mut Context<Self>) -> Div {
        // 状态行的三种可能，按「用户最需要知道什么」排序：登记不上 → 说原因；关了 →
        // 说关了；否则说用法。失败排在最前是因为它最容易被误解成「功能没做」——
        // 后面那句「已关闭」还会盖住原因（开关之所以关，正是因为它登记不上）。
        let status: SharedString = if let Some(error) = duck_platform::last_error() {
            let reason = crate::hotkey::explain(cx, &error);
            trf(cx, "未生效：{}", &[reason.as_ref()]).into()
        } else if !self.hotkey_enabled {
            tr(cx, "已关闭。").into()
        } else {
            tr(
                cx,
                "在别的程序里也能用；Duck 已经在前台时再按一次会把它收起来。",
            )
            .into()
        };

        let enabled_row = option_row(
            tr(cx, "启用唤起快捷键"),
            Some(
                tr(
                    cx,
                    "全局生效，与上面的应用内键位互不影响：应用内键位只在 Duck 有焦点时管用，这一条在任何程序里都能把 Duck 叫出来。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-hotkey-enabled",
                self.hotkey_enabled,
                cx.listener(|this, next: &bool, window, cx| {
                    // 先按目标值落状态，失败再按真实值回摆 —— 与「开机自启」同一个形状，
                    // 保证开关显示的就是真实情况。
                    this.hotkey_enabled = *next;
                    if let Err(error) = crate::hotkey::set_enabled(cx, *next) {
                        this.hotkey_enabled = duck_platform::active_spec().is_some();
                        let spec = crate::hotkey::raw_spec(cx);
                        crate::hotkey::report_failure(window, cx, &spec, &error);
                    }
                    cx.notify();
                }),
            ),
            cx,
        );

        // `tr()` 返回 `&'static str`（`Copy`），两处都直接用，不需要 `.clone()`。
        let hotkey_label = tr(cx, "快捷键");
        let binding_row = option_row(
            hotkey_label,
            Some(status),
            self.hotkey_recorder(hotkey_label.into(), cx),
            cx,
        );

        // 唤起语义。缺省关：`⌥Space` 自带启动器语义，默认只弹浮层而不把主窗切到前台。
        // 开着则回到旧行为（完整主窗），也是浮窗那条路出问题时的兜底。
        let opens_main_window_row = option_row(
            tr(cx, "唤起时打开主窗口"),
            Some(
                tr(
                    cx,
                    "关闭时只弹出一个快速面板，盖在当前工作之上，不动主窗口；打开时唤起完整主窗口（经典行为）。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-launcher-main-window",
                self.launcher_opens_main_window,
                cx.listener(|this, next: &bool, _, cx| {
                    this.launcher_opens_main_window = *next;
                    settings::set_launcher_opens_main_window(cx, *next);
                    cx.notify();
                }),
            ),
            cx,
        );

        settings_section(
            Some(tr(cx, "唤起").into()),
            [
                enabled_row.into_any_element(),
                binding_row.into_any_element(),
                opens_main_window_row.into_any_element(),
            ],
            cx,
        )
    }

    /// 快捷键录入控件：一个可聚焦的「键帽框」。
    ///
    /// 点一下进入录入，之后按下的第一个「修饰键 + 普通键」组合就是新键位，Esc 取消；
    /// 纯键盘走到这儿按 Enter / Space 也能进录入（见 [`Self::on_hotkey_key_down`]）。
    /// 用裸 `div` + `track_focus` 而不是按钮：这里要的是**原始按键**，而按钮会把
    /// 按键当「激活」处理掉，拿不到组合。
    ///
    /// `action_label` 是该行的可见标签（「快捷键」），只用来拼可访问名：行标签是
    /// **兄弟节点**，不会自动并进控件的名字里去。
    fn hotkey_recorder(&self, action_label: SharedString, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let chrome = extended.colors;
        let recording = self.hotkey_recording;
        let label: SharedString = if recording {
            tr(cx, "按下组合键…（Esc 取消）").into()
        } else {
            crate::hotkey::pretty_spec(cx).into()
        };

        div()
            .id("duck-settings-hotkey-recorder")
            // 无障碍：`track_focus` 只置 `focusable`、**不置 `tab_stop`**，所以此前这个
            // 控件 Tab 根本到不了。可访问名 = 行标签 + 当前键位（两段文案都取自既有
            // i18n 表，不新增待翻译串）；只报键位等于报了个值却没说它是什么。录入中
            // `label` 就是「按下组合键…（Esc 取消）」，恰好状态自述。
            .role(Role::Button)
            .aria_label(SharedString::from(format!("{action_label} {label}")))
            .tab_stop(true)
            .track_focus(&self.hotkey_focus)
            .h(theme.density().control)
            .min_w(px(220.))
            .px(tokens.spacing.md)
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme.components().input_radius)
            .border(extended.stroke.thin)
            .border_color(if recording {
                tokens.colors.ring
            } else {
                tokens.colors.border
            })
            .bg(if recording {
                chrome.hover_tint
            } else {
                tokens.colors.surface
            })
            .text_size(tokens.typography.sm.size)
            .text_color(tokens.colors.foreground)
            .cursor_pointer()
            .focus_visible({
                let ring = tokens.colors.ring;
                move |style| style.border_color(ring)
            })
            .when(!recording, |element| {
                element.hover(move |style| style.bg(chrome.hover_tint))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // 先要焦点，再进录入：没有焦点的话按键根本不会派发到这里。
                    window.focus(&this.hotkey_focus, cx);
                    if !this.hotkey_recording {
                        this.hotkey_recording = true;
                        cx.notify();
                    }
                }),
            )
            .on_key_down(cx.listener(Self::on_hotkey_key_down))
            .child(label)
            .into_any_element()
    }

    /// 录入中的一次按键。
    ///
    /// 三段：Esc 退出、只有修饰键就继续等、其余都当成「就是它了」。
    /// 全程 `stop_propagation` —— 否则 `⌘1`（切路由）之类的应用内键位会在录入的
    /// 同一瞬间也生效，用户看到的是一边录一边跳页。
    fn on_hotkey_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if !self.hotkey_recording {
            // 键盘入口：鼠标是靠 `on_mouse_down` 进录入的，纯键盘本来没有入口——那样
            // `tab_stop(true)` 只是多了一个「能聚焦但动不了」的站，比不可聚焦更糟。
            // 这里补 Enter / Space（而且要**无修饰键**，免得抢走 `⌘↵` 之类的应用内键位）。
            if !keystroke.modifiers.modified()
                && (keystroke.key.eq_ignore_ascii_case("enter")
                    || keystroke.key.eq_ignore_ascii_case("space"))
            {
                cx.stop_propagation();
                self.hotkey_recording = true;
                cx.notify();
            }
            return;
        }
        // Esc 取消（不带修饰键的那个 Esc，`⌘Esc` 是另一回事）。
        if keystroke.key.eq_ignore_ascii_case("escape") && !keystroke.modifiers.modified() {
            self.hotkey_recording = false;
            cx.notify();
            return;
        }
        let modifiers = crate::hotkey::modifiers_of(keystroke.modifiers);
        if modifiers.is_empty() {
            // 还没按到主键（用户会先按住修饰键再按主键）。继续等。
            return;
        }
        cx.stop_propagation();
        match duck_platform::Hotkey::from_key(&keystroke.key, modifiers) {
            Ok(hotkey) => {
                self.hotkey_recording = false;
                self.hotkey_enabled = true;
                if let Err(error) = crate::hotkey::rebind(cx, &hotkey) {
                    // 登记失败时 `kv` 与系统登记都没动，所以这里也要把开关摆回真实值。
                    self.hotkey_enabled = duck_platform::active_spec().is_some();
                    crate::hotkey::report_failure(window, cx, hotkey.spec(), &error);
                }
                cx.notify();
            }
            Err(duck_platform::HotkeyError::ModifierOnly) => {
                // 还在按修饰键，继续等主键。不退出录入，也不提示。
            }
            Err(error) => {
                self.hotkey_recording = false;
                crate::hotkey::report_failure(window, cx, &keystroke.key, &error);
                cx.notify();
            }
        }
    }

    // ── 插件：市场源 + 发现快照 + 逐插件启停 ─────────────────────────────
    fn plugins_body(&self, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();
        let entries = crate::plugins::PluginEntry::entries(cx);
        let rows: Vec<AnyElement> = if entries.is_empty() {
            let dir = crate::plugins::primary_plugins_dir();
            vec![
                body_text(tr(cx, "未发现插件"), cx).into_any_element(),
                meta_text(
                    trf(
                        cx,
                        "将插件放入 {} 后重启应用。",
                        &[dir.display().to_string().as_str()],
                    ),
                    cx,
                )
                .into_any_element(),
            ]
        } else {
            entries
                .into_iter()
                .map(|entry| plugin_row(entry, cx))
                .collect()
        };

        // 市场源：原先在插件中心顶部占一整行，挪到这里（那边只留一行状态结论）。
        let source_row = option_row_stacked(
            tr(cx, "市场源"),
            Some(
                tr(
                    cx,
                    "插件清单的获取地址；留空回落默认源，保存后立即重新获取。",
                )
                .into(),
            ),
            h_flex()
                .w_full()
                .gap(tokens.spacing.sm)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&self.market_source).control(cx).w_full()),
                )
                .child(
                    button(
                        "duck-settings-market-source-save",
                        tr(cx, "保存并刷新"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        let value = this.market_source.read(cx).value().to_string();
                        crate::market::set_source(cx, value);
                        crate::market::refresh(cx);
                    })),
                ),
            cx,
        );

        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(settings_section(
                Some(tr(cx, "插件市场源").into()),
                [source_row.into_any_element()],
                cx,
            ))
            .child(settings_section(
                Some(tr(cx, "已安装插件").into()),
                rows,
                cx,
            ))
    }

    // ── 通知 ─────────────────────────────────────────────────────────────
    /// 「通知」页：两条通道的开关 + 三条试一下 + 历史概览。
    ///
    /// 三个按钮不是装饰：通知是**跨窗口投递**的（落在当前活跃窗口上），而这一页恰好
    /// 就开在一个窗口里——用户需要能在自己的环境里确认「到底弹不弹、弹在哪」，
    /// 否则关掉横幅之后唯一的判断依据就是「我觉得好像没弹」。
    fn notify_body(&self, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();
        // 读**运行时**的通道档位，不读 `kv`：`kv_put` 是去抖落盘的（400ms），改完立刻
        // 回读拿到的是旧值 —— 下拉会在下一次重绘时弹回上一档，看起来就是「点了没反应」。
        // 运行时那份由 `settings::set_notify_*` 同步更新，与真正决定弹不弹的是同一个值。
        let channels = notify::channels(cx);

        let banners_row = option_row(
            tr(cx, "应用内横幅"),
            Some(
                tr(
                    cx,
                    "窗口右上角的提示条。无论哪一档，通知都会记入历史（顶栏铃铛）。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-notify-banners",
                notify::BannerLevel::ALL
                    .into_iter()
                    .map(|level| {
                        (
                            SharedString::from(level.wire()),
                            Self::banner_label(level, cx),
                        )
                    })
                    .collect::<Vec<_>>(),
                SharedString::from(channels.banners.wire()),
                cx.processor(|_, value: SharedString, _, cx| {
                    // 认不出来的值丢掉：下拉只回传上面那三个稳定值。
                    let Some(level) = notify::BannerLevel::from_wire(value.as_ref()) else {
                        return;
                    };
                    settings::set_notify_banners(cx, level);
                    // 没有本地快照要维护：上面那句已经把运行时档位改掉了，这次重绘读的就是它。
                    cx.notify();
                }),
                cx,
            ),
            cx,
        );

        let system_row = option_row(
            tr(cx, "系统通知"),
            Some(
                tr(
                    cx,
                    "投递到操作系统通知中心。需要已打包成应用并授权；开发构建下通常不显示。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-notify-system",
                channels.system,
                cx.listener(|_, on: &bool, _, cx| {
                    settings::set_notify_system(cx, *on);
                    cx.notify();
                }),
            ),
            cx,
        );

        // 三个按钮不是装饰：通知是**跨窗口投递**的（落在当前活跃窗口上），而这一页恰好
        // 就开在一个窗口里——用户需要能在自己的环境里确认「到底弹不弹、弹在哪」，
        // 否则关掉横幅之后唯一的判断依据就是「我觉得好像没弹」。
        //
        // 这一行**特意不写说明**：上面两行的说明已经说清了两条通道各是什么、去哪看，
        // 再重复一遍就成了噪音（`option_row_stacked` 的说明位本来就允许为空）。
        let try_row = option_row_stacked(
            tr(cx, "试一下"),
            None,
            h_flex()
                .w_full()
                .gap(tokens.spacing.sm)
                .flex_wrap()
                .child(
                    button(
                        "duck-settings-notify-toast",
                        tr(cx, "应用内提示"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        duck_components::notify::success(tr(cx, "这是一条应用内提示"), window, cx);
                    }),
                )
                .child(
                    button(
                        "duck-settings-notify-system-demo",
                        tr(cx, "系统通知"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        duck_components::notify::system(
                            crate::APP_NAME,
                            tr(cx, "这是一条系统通知"),
                            window,
                            cx,
                        );
                    }),
                )
                .child(
                    button(
                        "duck-settings-notify-both",
                        tr(cx, "两者同时"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        duck_components::notify::system_with_toast(
                            crate::APP_NAME,
                            tr(cx, "应用内 + 系统通知"),
                            window,
                            cx,
                        );
                    }),
                ),
            cx,
        );

        // 历史：这一页只报个数并给一个清空入口，**列表留给顶栏铃铛**——两处都渲染一份
        // 列表就得维护两条时间格式 / 空态 / 滚动路径，而它们没有任何理由不一样。
        let (total, unread) = (notify::history(cx).len(), notify::unread_count(cx));
        let history_row = option_row(
            tr(cx, "通知历史"),
            Some(tr(cx, "最近 100 条投递记录，只在本次运行内保留。").into()),
            h_flex()
                .gap(tokens.spacing.sm)
                .items_center()
                .child(meta_text(
                    trf(
                        cx,
                        "{} 条 · {} 条未读",
                        &[&total.to_string(), &unread.to_string()],
                    ),
                    cx,
                ))
                .child(
                    button(
                        "duck-settings-notify-clear",
                        tr(cx, "清空"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(total == 0)
                    // 清空只动 `notify` 的全局历史，不需要 `self`；但历史里那份快照
                    // （`total` / `unread`）是本次渲染算出来的，得靠 `cx.notify()` 重算。
                    .on_click(cx.listener(|_, _, _, cx| {
                        duck_components::notify::clear_history(cx);
                        cx.notify();
                    })),
                ),
            cx,
        );

        div().w_full().child(settings_section(
            Some(tr(cx, "通知").into()),
            [
                banners_row.into_any_element(),
                system_row.into_any_element(),
                try_row.into_any_element(),
                history_row.into_any_element(),
            ],
            cx,
        ))
    }

    /// 横幅档位的中文标签（`kv` 里存的是 [`notify::BannerLevel::wire`]）。
    fn banner_label(level: notify::BannerLevel, cx: &App) -> SharedString {
        match level {
            notify::BannerLevel::All => tr(cx, "全部提示").into(),
            notify::BannerLevel::WarningsAndErrors => tr(cx, "仅警告与错误").into(),
            notify::BannerLevel::Off => tr(cx, "全部关掉").into(),
        }
    }

    // ── 关于 ─────────────────────────────────────────────────────────────
    /// 「关于」＝ 居中品牌头 + 更新 + 版本发布记录 + 信息。
    ///
    /// 更新相关的整块（检查 / 下载 / 安装 / 自动检查 / 更新源）原先是独立分类页，
    /// 现并入这里：用户想知道「我装的是哪一版、源上有没有新版、改了什么」，
    /// 只该有一个去处，而不是在两页之间来回找。
    fn about_body(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let colors = tokens.colors;
        let muted = colors.muted_foreground;
        let appearance = theme.appearance().clone();
        let mode = theme.mode();
        let version = duck_update::compiled_version().to_string();

        // —— 品牌头：logo / 名称 / 版本徽章 / 一句话说明，整块居中 ——
        let header = v_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.md)
            .pt(tokens.spacing.lg)
            .pb(tokens.spacing.xl)
            .child(
                div()
                    .size(px(ABOUT_LOGO_SIZE))
                    .rounded(px(ABOUT_LOGO_RADIUS))
                    .overflow_hidden()
                    .child(img(LOGO_ASSET_PATH).size_full()),
            )
            .child(
                div()
                    .text_size(tokens.typography.xl.size)
                    .line_height(tokens.typography.xl.line_height)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.foreground)
                    .child(crate::APP_NAME),
            )
            .child(
                h_flex()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xxs)
                    .rounded_full()
                    .bg(colors.muted)
                    .child(meta_text(format!("v{version}"), cx)),
            )
            .child(
                div()
                    .max_w(px(ABOUT_DESCRIPTION_MAX_WIDTH))
                    .text_center()
                    .child(
                        body_text(
                            tr(
                                cx,
                                "Duck Toolbox 是一个基于 GPUI 构建的桌面工具箱外壳：把常用小工具收进同一个窗口，能力由插件扩展。",
                            ),
                            cx,
                        )
                        .text_color(muted),
                    ),
            );

        // —— 更新：原「更新」分类页整块并入（版本号已见上方徽章，不再重复一行）——
        let phase = update::phase(cx);
        let running = phase.is_running();

        let configured = update::source(cx);
        let source_row = option_row(
            tr(cx, "更新源"),
            Some(tr(cx, "静态 JSON manifest 的 https 地址。").into()),
            h_flex()
                .min_w_0()
                .gap(tokens.spacing.sm)
                .child(
                    div().min_w_0().truncate().child(
                        body_text(
                            if configured.trim().is_empty() {
                                tr(cx, "未配置")
                            } else {
                                configured.as_ref()
                            },
                            cx,
                        )
                        .text_color(muted),
                    ),
                )
                .child(
                    button(
                        "duck-settings-update-source",
                        tr(cx, "修改"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, window, cx| {
                        // 文案先取出来：`tr` 借 `cx`，而 `open_text_prompt` 要 `&mut`。
                        let title = tr(cx, "更新源");
                        let hint = tr(cx, "留空即用内置默认源（GitHub Releases）。");
                        let on_confirm: TextConfirm =
                            Rc::new(|value, _, cx| update::set_source(value.trim().to_owned(), cx));
                        open_text_prompt(
                            window,
                            cx,
                            title,
                            title,
                            update::default_source(),
                            Some(hint),
                            on_confirm,
                        );
                    })),
                ),
            cx,
        );

        let auto_row = option_row(
            tr(cx, "启动后自动检查更新"),
            None,
            switch_control(
                "duck-settings-update-auto",
                update::auto_check(cx),
                cx.listener(|_, next: &bool, _, cx| update::set_auto_check(*next, cx)),
            ),
            cx,
        );

        let mut status = v_flex().w_full().gap(tokens.spacing.sm);
        if let Some(text) = update::describe(&phase, cx) {
            status = status.child(body_text(text, cx).text_color(muted));
        }
        if let update::Phase::Manual { reason, .. } = &phase {
            status = status.child(meta_text(update::manual_reason(*reason, cx), cx));
        }
        if let update::Phase::Downloading { downloaded, total } = phase {
            let ratio = if total == 0 {
                0.
            } else {
                downloaded as f32 / total as f32
            };
            status = status.child(content::progress_bar(
                "duck-settings-update-progress",
                ratio,
                cx,
            ));
        }

        let mut actions = h_flex().gap(tokens.spacing.sm).flex_wrap();
        if running {
            actions = actions.child(
                button(
                    "duck-settings-update-cancel",
                    tr(cx, "取消"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|_, _, _, cx| update::cancel(cx))),
            );
        } else {
            actions = actions.child(
                button(
                    "duck-settings-update-check",
                    tr(cx, "检查更新"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|_, _, _, cx| update::check_now(cx))),
            );
            if matches!(phase, update::Phase::Available(_)) {
                actions = actions.child(
                    button(
                        "duck-settings-update-install",
                        tr(cx, "下载并安装"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| update::install_now(cx))),
                );
            }
            if matches!(phase, update::Phase::Installed) {
                actions = actions.child(
                    button(
                        "duck-settings-update-restart",
                        tr(cx, "重启并应用更新"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| update::restart(cx))),
                );
            }
            if matches!(phase, update::Phase::RolledBack) {
                actions = actions.child(
                    button(
                        "duck-settings-update-restart-rollback",
                        tr(cx, "重启并运行上一版本"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| update::restart(cx))),
                );
            }
            // 只要上一版还在盘上，回退就是一个**正常的**操作，而不是失败之后的补救出口——
            // 所以它是个常驻的次要按钮，跟「检查更新」并排，而不是藏在错误提示里。
            if update::can_rollback() && !matches!(phase, update::Phase::RolledBack) {
                actions = actions.child(
                    button(
                        "duck-settings-update-rollback",
                        tr(cx, "回退到上一版本"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| update::rollback_now(cx))),
                );
            }
        }
        status = status.child(actions);

        let status_row = option_row_stacked(tr(cx, "更新状态"), None, status, cx);

        let update_section = settings_section(
            Some(tr(cx, "更新").into()),
            [
                status_row.into_any_element(),
                auto_row.into_any_element(),
                source_row.into_any_element(),
            ],
            cx,
        );

        // —— 版本发布记录：内容来自更新源 manifest，紧跟更新块（它就是检查的产物）——
        let notes: AnyElement = match update::release_notes(cx) {
            Some(notes) => v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .children(notes.lines().map(|line| {
                    // 只做换行还原：manifest 里的 Markdown 标题按原样当正文读，
                    // 不引一套 Markdown 渲染（渲染器会把 `##` 之类直接漏给用户看）。
                    if line.trim().is_empty() {
                        div().h(tokens.spacing.xs).into_any_element()
                    } else {
                        body_text(line.to_string(), cx)
                            .text_color(muted)
                            .into_any_element()
                    }
                }))
                .into_any_element(),
            None => meta_text(
                tr(cx, "配置更新源并检查更新后，这里会显示源上版本的发布说明。"),
                cx,
            )
            .into_any_element(),
        };
        let notes_section = settings_section(
            Some(tr(cx, "版本发布记录").into()),
            [option_row_stacked(tr(cx, "最新发布"), None, notes, cx).into_any_element()],
            cx,
        );

        // —— 信息：版本明细与当前生效主题 ——
        let info_section = settings_section(
            Some(tr(cx, "信息").into()),
            [
                option_row(
                    tr(cx, "当前版本"),
                    Some(tr(cx, "取自 workspace 版本号，与打包产物一致。").into()),
                    body_text(version, cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "当前生效主题"),
                    None,
                    body_text(appearance.theme.for_mode(mode).label(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "使用引导"),
                    Some(tr(cx, "第一次启动时的三步向导，随时可以再看一遍。").into()),
                    button(
                        "duck-settings-onboarding",
                        tr(cx, "重新查看引导"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| super::onboarding::open_onboarding(cx))),
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        );

        // —— 诊断：出问题时最该先看的一处（排在「信息」之后，平时不打扰）——
        let diagnostics_actions = h_flex()
            .gap(tokens.spacing.sm)
            .flex_wrap()
            .child(
                button(
                    "duck-settings-diagnostics-copy",
                    tr(cx, "复制诊断信息"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|_, _, _, cx| diagnostics::copy_report(cx))),
            )
            .child(
                button(
                    "duck-settings-diagnostics-logs",
                    tr(cx, "打开日志目录"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|_, _, _, _| {
                    duck_platform::reveal_in_finder(&diagnostics::log_dir())
                })),
            );

        let diagnostics_section = settings_section(
            Some(tr(cx, "诊断信息").into()),
            [
                option_row(
                    tr(cx, "运行平台"),
                    None,
                    body_text(diagnostics::platform(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "插件数量"),
                    None,
                    body_text(diagnostics::plugin_count(cx).to_string(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "累计崩溃"),
                    None,
                    body_text(diagnostics::crash_total().to_string(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "数据文件"),
                    None,
                    body_text(duck_platform::db_path().display().to_string(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "日志目录"),
                    None,
                    body_text(diagnostics::log_dir().display().to_string(), cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
                option_row_stacked(tr(cx, "诊断操作"), None, diagnostics_actions, cx)
                    .into_any_element(),
            ],
            cx,
        );

        // —— 法律信息：发行合规要求「许可可查」，全文随程序内嵌（见 `pages::licenses`）——
        let license_summary = super::licenses::stats().map_or_else(
            || SharedString::from(tr(cx, "第三方组件许可汇总")),
            |stats| {
                SharedString::from(trf(
                    cx,
                    "{} 个组件 · {} 份许可全文",
                    &[&stats.components.to_string(), &stats.texts.to_string()],
                ))
            },
        );
        let legal_section = settings_section(
            Some(tr(cx, "法律信息").into()),
            [
                option_row(
                    tr(cx, "本程序许可"),
                    Some(tr(cx, "MIT 许可，版权归 HanoCode。").into()),
                    button(
                        "duck-settings-licenses",
                        tr(cx, "查看开源许可"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| super::licenses::open_licenses(cx))),
                    cx,
                )
                .into_any_element(),
                option_row(
                    tr(cx, "第三方组件"),
                    Some(tr(cx, "由 Cargo.lock 汇总，随程序内置，离线可查。").into()),
                    body_text(license_summary, cx).text_color(muted),
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        );

        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(header)
            .child(update_section)
            .child(notes_section)
            .child(info_section)
            .child(diagnostics_section)
            .child(legal_section)
    }
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

/// 按 schema 渲染一项工具设置的控件。
///
/// 类型不符（schema 改过 / 落盘值来自旧版本）时不 panic，回落成只读文本——
/// 设置界面崩掉比少一个控件糟糕得多。
fn tool_setting_control(
    tool_id: &'static str,
    spec: &'static ToolSettingSpec,
    cx: &App,
) -> AnyElement {
    let spec = *spec;
    let key = spec.key();
    match (spec.kind(), tool_settings::current(cx, tool_id, &spec)) {
        (ToolSettingKind::Toggle { .. }, SettingValue::Toggle(on)) => switch_control(
            SharedString::from(format!("settings-tool-switch-{tool_id}-{key}")),
            on,
            move |value, _, cx| {
                tool_settings::apply(cx, tool_id, &spec, SettingValue::Toggle(*value));
                cx.refresh_windows();
            },
        )
        .into_any_element(),
        (ToolSettingKind::Select { options, .. }, SettingValue::Select(current)) => {
            let items = options.iter().map(|option| {
                (
                    SharedString::from(option.value()),
                    SharedString::from(tr(cx, option.label())),
                )
            });
            dropdown_select(
                SharedString::from(format!("settings-tool-select-{tool_id}-{key}")),
                items,
                SharedString::from(current),
                move |value, _, cx| {
                    // 下拉回传的是稳定值；按 schema 校验一次，非法值（理论上不会）直接丢弃。
                    let picked = options
                        .iter()
                        .map(tool_settings::SelectOption::value)
                        .find(|option| *option == value.as_ref());
                    let Some(picked) = picked else {
                        return;
                    };
                    tool_settings::apply(cx, tool_id, &spec, SettingValue::Select(picked));
                    cx.refresh_windows();
                },
                cx,
            )
        }
        (ToolSettingKind::Text { placeholder, .. }, SettingValue::Text(text)) => {
            let shown = if text.is_empty() {
                meta_text(tr(cx, "（未设置）"), cx).into_any_element()
            } else {
                body_text(text, cx).into_any_element()
            };
            h_flex()
                .items_center()
                .gap(px(6.))
                .child(shown)
                .child(
                    button(
                        SharedString::from(format!("settings-tool-text-{tool_id}-{key}")),
                        tr(cx, "编辑"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let on_confirm: TextConfirm = Rc::new(move |value, _, cx| {
                            tool_settings::apply(
                                cx,
                                tool_id,
                                &spec,
                                SettingValue::Text(value.to_owned()),
                            );
                            cx.refresh_windows();
                        });
                        open_text_prompt(
                            window,
                            cx,
                            spec.label(),
                            spec.label(),
                            placeholder,
                            None,
                            on_confirm,
                        );
                    }),
                )
                .into_any_element()
        }
        (
            ToolSettingKind::Number {
                min,
                max: _,
                default: _,
                unit,
            },
            SettingValue::Number(value),
        ) => {
            let shown = if unit.is_empty() {
                value.to_string()
            } else {
                format!("{value} {unit}")
            };
            h_flex()
                .items_center()
                .gap(px(6.))
                .child(body_text(shown, cx))
                .child(
                    button(
                        SharedString::from(format!("settings-tool-number-{tool_id}-{key}")),
                        tr(cx, "编辑"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let on_confirm: NumberConfirm = Rc::new(move |value, _, cx| {
                            tool_settings::apply(cx, tool_id, &spec, SettingValue::Number(value));
                            cx.refresh_windows();
                        });
                        open_number_prompt(
                            window,
                            cx,
                            spec.label(),
                            spec.label(),
                            unit,
                            None,
                            min,
                            on_confirm,
                        );
                    }),
                )
                .into_any_element()
        }
        (_, other) => body_text(setting_value_display(&other), cx).into_any_element(),
    }
}

/// 设置值的朴素展示（仅用于类型不符的兜底分支）。
fn setting_value_display(value: &SettingValue) -> String {
    match value {
        SettingValue::Toggle(on) => on.to_string(),
        SettingValue::Select(option) => (*option).to_owned(),
        SettingValue::Text(text) => text.clone(),
        SettingValue::Number(number) => number.to_string(),
    }
}

/// 页面标题 + 一行描述（[`title_text`] + xs 二级文字）。
fn page_heading(title: &str, description: &str, cx: &App) -> Div {
    let tokens = active_theme(cx).tokens().clone();
    v_flex()
        .w_full()
        .gap(tokens.spacing.xxs)
        .child(title_text(title, cx))
        .child(meta_text(description, cx))
}

/// 「插件」页的一行：展示名 / 模型·版本·工具数·能力摘要 / 启停开关。
///
/// 清单无效的插件保留展示（回落目录名 + 英文错误说明），开关换成警示图标——
/// 让投放错的包可诊断，而不是静默消失。
fn plugin_row(entry: crate::plugins::PluginEntry, cx: &App) -> AnyElement {
    let description: SharedString = match entry.model {
        Some(model) => {
            let model_label = crate::plugins::model_display(model, cx);
            let tools_label = trf(cx, "{} 个工具", &[entry.tools.to_string().as_str()]);
            let mut parts = vec![
                model_label.to_string(),
                format!("v{}", entry.version.as_deref().unwrap_or("?")),
                tools_label,
            ];
            if let Some(caps) = &entry.capabilities {
                let mut labels: Vec<&str> = Vec::new();
                if !caps.filesystem_read.is_empty() || !caps.filesystem_write.is_empty() {
                    labels.push(tr(cx, "文件访问"));
                }
                if !caps.network_hosts.is_empty() {
                    labels.push(tr(cx, "网络"));
                }
                if caps.notifications {
                    labels.push(tr(cx, "通知"));
                }
                if !caps.process_exec.is_empty() {
                    labels.push(tr(cx, "进程执行"));
                }
                if !labels.is_empty() {
                    parts.push(labels.join(" / "));
                }
                // 授予概况：清单里声明的 ≠ 本机放行的。不带这一行的话，用户在确认框里
                // 收回去的权限在这页上完全看不出来（详情页能改，但先得知道要去看）。
                if let Some(plugin_id) = entry.id.as_deref() {
                    let (granted, declared) = crate::plugins::grant_summary(cx, plugin_id, caps);
                    if declared > 0 {
                        parts.push(trf(
                            cx,
                            "已授予 {} / {} 项能力",
                            &[&granted.to_string(), &declared.to_string()],
                        ));
                    }
                }
            }
            SharedString::from(parts.join(" · "))
        }
        None => SharedString::from(format!(
            "{} — {}",
            tr(cx, "清单无效"),
            entry.error.as_deref().unwrap_or("")
        )),
    };

    let control = match entry.id.clone() {
        Some(plugin_id) => {
            let display_name = entry.name.clone();
            switch_control(
                SharedString::from(format!("settings-plugins-toggle-{plugin_id}")),
                entry.enabled,
                move |value: &bool, window, cx| {
                    crate::plugins::set_enabled(cx, &plugin_id, *value);
                    let label = if *value {
                        trf(cx, "插件「{}」已启用", &[display_name.as_str()])
                    } else {
                        trf(cx, "插件「{}」已停用", &[display_name.as_str()])
                    };
                    duck_components::notify::success(label, window, cx);
                    cx.refresh_windows();
                },
            )
            .into_any_element()
        }
        None => Icon::new(DuckIcon::TriangleAlert).into_any_element(),
    };

    option_row(entry.name, Some(description), control, cx).into_any_element()
}

/// 自定义强调色取色器的槽位；随窗口缓存，避免每帧重建。
struct AccentPickerSlot {
    picker: Entity<ColorPickerState>,
    synced: Hsla,
    _subscription: Subscription,
}

/// 取色器实体：提交颜色即写入外观设置。
fn accent_picker(
    window: &mut Window,
    cx: &mut Context<SettingsWindowView>,
    current: Hsla,
) -> Entity<ColorPickerState> {
    let slot = window.use_keyed_state(
        SharedString::from("duck-settings-accent-picker"),
        cx,
        move |window, cx| {
            let picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(current));
            let subscription = cx.subscribe(
                &picker,
                |_: &mut AccentPickerSlot, _, event: &ColorPickerEvent, cx| {
                    let ColorPickerEvent::Change(Some(value)) = event else {
                        return;
                    };
                    update_appearance(cx, |appearance| appearance.accent = *value);
                },
            );
            AccentPickerSlot {
                picker,
                synced: current,
                _subscription: subscription,
            }
        },
    );
    slot.update(cx, |slot, cx| {
        if slot.synced != current {
            slot.synced = current;
            slot.picker
                .update(cx, |picker, cx| picker.set_value(current, window, cx));
        }
    });
    slot.read(cx).picker.clone()
}

/// 主题卡片：预览块 + 标签 + 选中勾。
fn theme_card(
    id: BuiltinThemeId,
    selected: bool,
    appearance: &Appearance,
    cx: &Context<SettingsWindowView>,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let colors = tokens.colors;
    let preview = id.preview();
    let row_hover = theme.extended().colors.hover_tint;
    let check_size = theme.extended().icon.sm;
    let opacity = theme.extended().opacity;
    let mode = theme.mode();
    let bar = |width: gpui::DefiniteLength, color: Hsla| {
        div()
            .w(width)
            .h(px(THEME_PREVIEW_BAR_HEIGHT))
            .rounded_full()
            .bg(color)
    };
    let preview_block = h_flex()
        .w_full()
        .h(px(THEME_PREVIEW_HEIGHT))
        .rounded(tokens.radius.md)
        .border_1()
        .border_color(preview.border)
        .bg(preview.background)
        .overflow_hidden()
        .child(
            v_flex()
                .w(px(THEME_PREVIEW_SIDEBAR_WIDTH))
                .h_full()
                .items_center()
                .justify_center()
                .gap(px(THEME_PREVIEW_BAR_HEIGHT))
                .bg(preview.surface)
                .child(bar(px(16.).into(), preview.primary))
                .child(bar(
                    px(16.).into(),
                    preview.muted_foreground.opacity(opacity.muted),
                ))
                .child(bar(
                    px(16.).into(),
                    preview.muted_foreground.opacity(opacity.muted),
                )),
        )
        .child(
            v_flex()
                .flex_1()
                .h_full()
                .p(tokens.spacing.xs)
                .justify_center()
                .gap(px(THEME_PREVIEW_BAR_HEIGHT))
                .child(bar(relative(1.), preview.foreground.opacity(opacity.faint)))
                .child(bar(
                    relative(1.),
                    preview.muted_foreground.opacity(opacity.faint),
                ))
                .child(bar(
                    relative(0.6),
                    preview.muted_foreground.opacity(opacity.faint),
                )),
        );

    let mut next = appearance.clone();
    next.theme = next.theme.with(mode, id);
    // 焦点环色要 `move` 进闭包 → 先拷出来（`Hsla` 是 `Copy`）。
    let ring = colors.ring;
    let ring_strong = colors.foreground;
    div()
        .id(SharedString::from(format!(
            "duck-theme-card-{}",
            id.wire_name()
        )))
        .w(px(THEME_CARD_WIDTH))
        .p(tokens.spacing.sm)
        .rounded(tokens.radius.lg)
        .border_1()
        .border_color(if selected {
            colors.primary
        } else {
            colors.border
        })
        .bg(colors.surface)
        // 无障碍：此前整排主题卡片对读屏是不存在的（a11y 树只收录带 role 的节点）。
        // 语义上是「一组里选一个」→ 单选按钮；可访问名取**显式** `aria_label`
        // （卡片自己的可见标签），不依赖「从子节点文字派生」这条本仓库验证不了的假设
        // ——`test_support.rs:318` 的 `label()` 也只认显式 label。容器在调用点补
        // `RadioGroup`。
        .role(Role::RadioButton)
        .aria_toggled(if selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .aria_label(id.label())
        .focusable()
        .tab_stop(true)
        // 焦点描边一律 `ring`；**例外**：选中态边框本身就是 `colors.primary`(=`ring`)，
        // 两者重合会让焦点看上去「没反应」——偏偏就落在用户最可能停留的那张卡上。
        // 所以选中时换成 `colors.foreground`：仓库既有的强描边色（色点的选中/悬停
        // 用的就是它），对 `surface` 与 `primary` 都有足够反差。
        .focus_visible(move |style| style.border_color(if selected { ring_strong } else { ring }))
        .cursor_pointer()
        .when(!selected, |this| {
            this.hover(move |style| style.bg(row_hover))
        })
        .on_click(move |_, _, cx| {
            let next = next.clone();
            update_appearance(cx, move |appearance| *appearance = next.clone());
        })
        .child(preview_block)
        .child(
            h_flex()
                .mt(tokens.spacing.xs)
                .gap(tokens.spacing.xs)
                .justify_between()
                .items_center()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(colors.foreground)
                .child(div().min_w_0().truncate().child(id.label()))
                .children(selected.then(|| {
                    Icon::new(DuckIcon::Check)
                        .size(check_size)
                        .text_color(colors.primary)
                })),
        )
}

/// 文件图标包卡片：预览样例文件图标 + 标签 + 选中勾（样式对齐主题卡片）。
fn icon_pack_card(
    id: &str,
    label: SharedString,
    selected: bool,
    on_select: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let colors = tokens.colors;
    let row_hover = theme.extended().colors.hover_tint;
    let check_size = theme.extended().icon.sm;
    let preview_icon = theme.extended().icon.lg;
    let muted = colors.muted_foreground;
    // 焦点环色要 `move` 进闭包 → 先拷出来。
    let ring = colors.ring;
    let ring_strong = colors.foreground;
    let icons = icon_pack_preview(id);
    let preview = h_flex()
        .w_full()
        .h(px(THEME_PREVIEW_HEIGHT))
        .justify_center()
        .items_center()
        .gap(tokens.spacing.xs)
        .rounded(tokens.radius.md)
        .bg(colors.background)
        .overflow_hidden()
        .children(icons.into_iter().map(|(icon, brand)| {
            let color = brand.map_or(muted, |value| accent_from_rgb(value));
            Icon::new(icon).size(preview_icon).text_color(color)
        }));
    div()
        .id(SharedString::from(format!("duck-icon-pack-card-{id}")))
        .w(px(THEME_CARD_WIDTH))
        .p(tokens.spacing.sm)
        .rounded(tokens.radius.lg)
        .border_1()
        .border_color(if selected {
            colors.primary
        } else {
            colors.border
        })
        .bg(colors.surface)
        // 无障碍：同 `theme_card` —— 无 role 的节点不进 a11y 树；语义为单选按钮。
        // 可访问名取**显式** `aria_label`（复用下面那句可见文案），不依赖
        // 「从子节点文字派生」这条本仓库验证不了的假设。
        .role(Role::RadioButton)
        .aria_toggled(if selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .aria_label(label.clone())
        .focusable()
        .tab_stop(true)
        // 同上：选中态边框已是 `colors.primary`(=`ring`)，焦点环改用 `foreground` 才看得见。
        .focus_visible(move |style| style.border_color(if selected { ring_strong } else { ring }))
        .cursor_pointer()
        .when(!selected, |this| {
            this.hover(move |style| style.bg(row_hover))
        })
        .on_click(on_select)
        .child(preview)
        .child(
            h_flex()
                .mt(tokens.spacing.xs)
                .gap(tokens.spacing.xs)
                .justify_between()
                .items_center()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(colors.foreground)
                .child(div().min_w_0().truncate().child(label))
                .children(selected.then(|| {
                    Icon::new(DuckIcon::Check)
                        .size(check_size)
                        .text_color(colors.primary)
                })),
        )
}

/// 主题卡片下方的动作行：导入 / 导出（差异 · 完整）/ 更多主题。
/// 外壳未接入主题文件读写，动作均为占位提示。
fn theme_actions(cx: &mut Context<SettingsWindowView>) -> impl IntoElement {
    let tokens = active_theme(cx).tokens();
    h_flex()
        .mt(tokens.spacing.xs)
        .gap(tokens.spacing.sm)
        .flex_wrap()
        .child(
            button(
                "duck-settings-theme-import",
                tr(cx, "导入"),
                ButtonVariant::Secondary,
                cx,
            )
            .flex_shrink_0()
            .on_click(|_, window, cx| {
                duck_components::notify::info(
                    tr(cx, "主题导入尚未接入（外壳无主题文件读写）"),
                    window,
                    cx,
                );
            }),
        )
        .child(
            Button::new("duck-settings-theme-export")
                .outline()
                .control(cx)
                .label(tr(cx, "导出"))
                .dropdown_caret(true)
                .dropdown_menu_with_anchor(Anchor::TopLeft, |menu, _, cx| {
                    let detail = tr(cx, "主题导出（差异快照）尚未接入");
                    let full = tr(cx, "主题导出（完整）尚未接入");
                    menu.item(PopupMenuItem::new(tr(cx, "差异快照（仅设置）")).on_click(
                        move |_, window, cx| {
                            duck_components::notify::info(detail, window, cx);
                        },
                    ))
                    .item(
                        PopupMenuItem::new(tr(cx, "完整主题")).on_click(move |_, window, cx| {
                            duck_components::notify::info(full, window, cx);
                        }),
                    )
                }),
        )
        .child(
            button(
                "duck-settings-theme-more",
                tr(cx, "更多主题"),
                ButtonVariant::Link,
                cx,
            )
            .flex_shrink_0()
            .on_click(|_, window, cx| {
                duck_components::notify::info(tr(cx, "主题画廊尚未提供"), window, cx);
            }),
        )
}

/// 强调色色点。
fn color_dot(
    index: usize,
    color: Hsla,
    selected: bool,
    icon: Option<DuckIcon>,
    cx: &Context<SettingsWindowView>,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let colors = tokens.colors;
    let icon_size = theme.extended().icon.md;
    let ring = colors.ring;
    // 色点里只有一个勾图标、没有文字，AccessKit **没有东西可派生**可访问名 → 只能显式给。
    // 用色值本身当名字（`#8B5CF6`）是零 i18n 成本且信息量真实的选择；
    // 「蓝 / 紫 / 青」这类**本地化色名**需要往 `i18n.rs` 加串，留待后续（AGENTS 3.3）。
    let accent_rgb = ACCENTS[index];
    div()
        .id(("duck-accent-dot", index))
        .size(px(COLOR_DOT_SIZE))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(color)
        .border_2()
        .border_color(if selected { colors.foreground } else { color })
        // 无障碍：色点此前对读屏根本不存在（a11y 树只收录带 role 的节点）。
        // 一组预设色 = 「选一个」→ 单选按钮，容器在调用点补 `RadioGroup`。
        .role(Role::RadioButton)
        .aria_toggled(if selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .aria_label(SharedString::from(format!("#{accent_rgb:06X}")))
        .focusable()
        .tab_stop(true)
        // 焦点描边用 `ring`：选中态描边是 `foreground`，两者正好错开，两种状态都看得见。
        // `border_2` 本来就占着位，只改色不引起位移。
        .focus_visible(move |style| style.border_color(ring))
        .cursor_pointer()
        .when(!selected, |this| {
            // 色点语义：悬停描边加深到正文色。
            this.hover(move |style| style.border_color(colors.foreground))
        })
        .on_click(move |_, _, cx| {
            update_appearance(cx, move |appearance| appearance.accent = color);
        })
        .when_some(icon, |this, icon| {
            this.child(
                Icon::new(icon)
                    .size(icon_size)
                    .text_color(duck_theme::foreground_for(color)),
            )
        })
}
