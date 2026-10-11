//! 极简 i18n 层：语言枚举 + 以「中文源串」为键的英译表。
//!
//! 设计取舍：**源串即键**（`t(lang, "外观")`），缺表项自动回落源串，所以新增文案
//! 也不会翻成空串；英译集中在 [`EN`]（按 key 字节序排序，供二分查找），并有
//! `cargo test` 里的两条守卫：表必须有序、源码里的中文文案必须全部登记。

use std::sync::OnceLock;

use gpui::App;
use serde::{Deserialize, Serialize};

use crate::manager::active_theme;

/// 用户在「外观 → 语言」里的选择。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Locale {
    /// 跟随系统语言。
    #[default]
    System,
    /// 简体中文。
    ZhHans,
    /// English。
    En,
}

impl Locale {
    /// 下拉里的全部档位（顺序即展示顺序）。
    pub const ALL: [Self; 3] = [Self::System, Self::ZhHans, Self::En];

    /// 偏好 wire 值（落盘 / 比较用）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::ZhHans => "zh-Hans",
            Self::En => "en",
        }
    }

    /// 解析 wire 值；未知值回落到「跟随系统」。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "zh-Hans" => Self::ZhHans,
            "en" => Self::En,
            _ => Self::System,
        }
    }

    /// 下拉项名称：语言名按本族写法展示，不随界面语言变化。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::ZhHans => "简体中文",
            Self::En => "English",
        }
    }

    /// 解析成具体语言；`System` 读系统语言。
    #[must_use]
    pub fn resolve(self) -> Lang {
        match self {
            Self::System => system_lang(),
            Self::ZhHans => Lang::Zh,
            Self::En => Lang::En,
        }
    }
}

/// 具体生效语言（没有「跟随系统」这一档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// 简体中文（源串本身）。
    Zh,
    /// English。
    En,
}

impl Lang {
    /// 中文源串 → 当前语言文案；[`Lang::Zh`] 直接返回源串。
    #[must_use]
    pub fn text(self, source: &'static str) -> &'static str {
        t(self, source)
    }
}

/// 系统语言探测结果：进程内只算一次（GUI 进程从 Finder 启动时环境变量常为空，
/// 需要读一次 macOS 全局偏好，代价不值得每帧重复）。
#[must_use]
pub fn system_lang() -> Lang {
    static CACHE: OnceLock<Lang> = OnceLock::new();
    *CACHE.get_or_init(detect_system_lang)
}

fn detect_system_lang() -> Lang {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(key)
            && let Some(lang) = lang_from_tag(&value)
        {
            return lang;
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(lang) = macos_language() {
        return lang;
    }
    // 探测不到时按项目主语言（中文）兜底。
    Lang::Zh
}

/// `zh_CN` / `zh-Hans` / `en_US` 形态的语言标签 → 语言；`C` / `POSIX` / 空值返回 `None`。
fn lang_from_tag(tag: &str) -> Option<Lang> {
    let tag = tag.trim().to_ascii_lowercase();
    if tag.is_empty() || tag == "c" || tag == "posix" {
        return None;
    }
    Some(if tag.starts_with("zh") {
        Lang::Zh
    } else {
        Lang::En
    })
}

/// macOS：读 `AppleLocale`（如 `zh_CN`），与「系统设置 → 语言与地区」一致。
#[cfg(target_os = "macos")]
fn macos_language() -> Option<Lang> {
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    lang_from_tag(&String::from_utf8_lossy(&output.stdout))
}

/// 取文案：`lang` 为中文时返回源串本身，英文时查表，缺表项回落源串。
#[must_use]
pub fn t(lang: Lang, source: &'static str) -> &'static str {
    match lang {
        Lang::Zh => source,
        Lang::En => translate_en(source).unwrap_or(source),
    }
}

/// 取文案（从全局主题读当前语言）。
#[must_use]
pub fn tr(cx: &App, source: &'static str) -> &'static str {
    t(active_theme(cx).lang(), source)
}

/// 带占位符的文案：源串里用 `{}` 站位，按 `args` 顺序逐个替换。
#[must_use]
pub fn tf(lang: Lang, source: &'static str, args: &[&str]) -> String {
    let mut out = String::with_capacity(source.len() + 16);
    let mut rest = t(lang, source);
    for arg in args {
        match rest.find("{}") {
            Some(index) => {
                out.push_str(&rest[..index]);
                out.push_str(arg);
                rest = &rest[index + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// 带占位符的文案（从全局主题读当前语言）。
#[must_use]
pub fn trf(cx: &App, source: &'static str, args: &[&str]) -> String {
    tf(active_theme(cx).lang(), source, args)
}

/// 英译查表：未登记的源串返回 `None`（调用方回落源串）。
fn translate_en(source: &str) -> Option<&'static str> {
    EN.binary_search_by_key(&source, |(key, _)| *key)
        .ok()
        .map(|index| EN[index].1)
}

/// 英译表（按中文源串的 UTF-8 字节序升序，二分查找依赖这一不变量）。
static EN: &[(&str, &str)] = &[
    ("47 个", "47"),
    ("AI 助手", "AI Assistant"),
    (
        "CARGO_MANIFEST_DIR 必须存在",
        "CARGO_MANIFEST_DIR must be set",
    ),
    (
        "CDP 控制本地浏览器嗅探媒体地址",
        "Sniffs media URLs through a CDP-controlled browser",
    ),
    (
        "DASH 页面会返回音视频分离的分片流，工具会自动合并；分片走有界 Range 请求，需保留完整请求头。",
        "DASH pages return separate audio and video segment streams which the tool merges; segments use bounded Range requests, so full headers must be kept.",
    ),
    (
        "Duck Toolbox 是一个基于 GPUI 构建的桌面工具箱外壳：把常用小工具收进同一个窗口，能力由插件扩展。",
        "Duck Toolbox is a desktop toolbox shell built on GPUI: it gathers everyday utilities into a single window and grows through plugins.",
    ),
    (
        "Duck Toolbox 的示例外壳：自绘标题栏、活动栏、可拖拽侧栏、工具栏与状态栏。",
        "A sample Duck Toolbox shell: custom title bar, activity bar, resizable sidebar, toolbar and status bar.",
    ),
    ("HTTP 请求回放", "HTTP request replay"),
    ("JS 脚本", "JS script"),
    (
        "MIT 许可，版权归 HanoCode。",
        "MIT license, copyright HanoCode.",
    ),
    ("SSH 终端", "SSH Terminal"),
    ("WASM 组件", "WASM component"),
    (
        "WebP / AVIF / PNG 互转与压缩",
        "Converts and compresses WebP / AVIF / PNG",
    ),
    ("{} 个", "{}"),
    ("{} 个工具", "{} tools"),
    ("{} 个文件 · {}", "{} files · {}"),
    (
        "{} 个组件 · {} 份许可全文",
        "{} components · {} license texts",
    ),
    (
        "{} 个组件 · {} 份许可全文 · {} 种许可声明",
        "{} components · {} license texts · {} license expressions",
    ),
    ("{} 分钟", "{} min"),
    ("{} 分钟前", "{} min ago"),
    ("{} 天", "{} days"),
    ("{} 天内用过 {} 个", "{} used in the last {} days"),
    (
        "{} 天内还没用过任何工具",
        "Nothing used in the last {} days",
    ),
    ("{} 天前", "{} d ago"),
    ("{} 小时前", "{} h ago"),
    ("{} 条 · {} 条未读", "{} total · {} unread"),
    ("{} 项匹配", "{} matches"),
    (
        "「{}」不是可用的组合。",
        "\"{}\" is not a usable combination.",
    ),
    (
        "「{}」不能用作全局快捷键：媒体键与音量键要额外的系统授权，还会抢走系统控制。",
        "\"{}\" cannot be a global shortcut: media and volume keys need extra system permission and would take over the system controls.",
    ),
    (
        "「{}」后端尚未接入",
        "\\\"{}\\\" backend is not wired up yet",
    ),
    ("「{}」执行完成", "\"{}\" finished."),
    (
        "「{}」的插件目录会被删除，它贡献的工具也会一起消失。",
        "The plugin folder for \"{}\" will be deleted, along with any tools it contributes.",
    ),
    (
        "「更多操作」菜单尚未接入业务逻辑",
        "The “More actions” menu is not wired up yet",
    ),
    ("上一步", "Back"),
    ("上一页", "Previous page"),
    ("上传限速", "Upload limit"),
    ("下一步", "Next"),
    ("下一页", "Next page"),
    ("下载", "download"),
    ("下载并安装", "Download and install"),
    ("下载的是直链还是分片", "Direct link or segments?"),
    ("下载限速", "Download limit"),
    ("不使用代理", "No proxy"),
    ("两者同时", "Both"),
    ("个", ""),
    ("中文", "Chinese"),
    ("主窗口", "Main window"),
    ("主题", "Theme"),
    ("主题与配色", "Theme & colors"),
    (
        "主题导入尚未接入（外壳无主题文件读写）",
        "Theme import is not wired up (the shell has no theme file IO)",
    ),
    (
        "主题导出（完整）尚未接入",
        "Theme export (full) is not wired up",
    ),
    (
        "主题导出（差异快照）尚未接入",
        "Theme export (diff snapshot) is not wired up",
    ),
    ("主题模式", "Theme mode"),
    ("主题画廊尚未提供", "No theme gallery is provided yet"),
    ("主题色", "Accent color"),
    ("亮色", "Light"),
    ("亮色主题", "Light theme"),
    (
        "仅 UI 外壳示例，未接入任何业务能力。",
        "A UI shell sample only; no business logic is wired up.",
    ),
    ("仅警告与错误", "Warnings and errors"),
    ("从备份导入？", "Import from this backup?"),
    ("从备份文件导入", "Import from a backup"),
    ("从左侧选择工具", "Pick a tool from the sidebar"),
    (
        "从左侧选择工具后，这里显示该工具的帮助与设置。",
        "Pick a tool on the left; its help and settings appear here.",
    ),
    (
        "从市场安装，或将插件放入 {} 后重启应用。",
        "Install from the marketplace, or drop plugins into {} and restart.",
    ),
    (
        "从抓包记录重建请求",
        "Rebuilds requests from captured traffic",
    ),
    ("从插件市场开始", "Start from the plugin center"),
    (
        "任务列表中的图标风格，与主题相互独立。",
        "Icon style used in task lists; independent of the theme.",
    ),
    (
        "任务列表的行高从 44 收到 30。",
        "Task rows shrink from 44 to 30.",
    ),
    ("作者", "Author"),
    ("使用", "Usage"),
    ("使用主题字体", "Use theme font"),
    ("使用引导", "Getting started"),
    ("使用次数", "Times used"),
    ("侧栏", "Sidebar"),
    ("侧栏项间距", "Sidebar item spacing"),
    ("保存后复制", "Copy after saving"),
    (
        "保存常用命令，一键复制执行",
        "Saves frequent commands for one-click copy",
    ),
    ("保存并刷新", "Save and refresh"),
    (
        "保留 EXIF 等元数据；关闭可进一步压缩体积。",
        "Keeps EXIF and other metadata; turning it off shrinks the file further.",
    ),
    ("保留临时文件", "Keep temporary files"),
    ("保留元数据", "Keep metadata"),
    ("保留时长", "Keep for"),
    ("信息", "Information"),
    ("修改", "Change"),
    ("偏好", "Preferences"),
    (
        "停用后从工具箱列表隐藏，设置项保留。",
        "Disabled tools are hidden from the toolbox list; their settings are kept.",
    ),
    (
        "全局快捷键 {} 没能生效。{}可在「设置 → 行为」里换一个。",
        "The global shortcut {} is not working. {}Pick another one under Settings → Behaviour.",
    ),
    (
        "全局生效，与上面的应用内键位互不影响：应用内键位只在 Duck 有焦点时管用，这一条在任何程序里都能把 Duck 叫出来。",
        "Global, and independent of the in-app keys above: those only apply while Duck has focus, whereas this one summons Duck from any app.",
    ),
    ("全选", "Select All"),
    ("全部", "All"),
    ("全部关掉", "Off"),
    ("全部分类", "All categories"),
    ("全部工具", "All tools"),
    ("全部开始", "Resume all"),
    ("全部提示", "All notifications"),
    ("全部暂停", "Pause all"),
    ("全部清除", "Clear everything"),
    ("全部状态", "All statuses"),
    ("全部级别", "All levels"),
    ("共 {} 行", "{} lines"),
    ("关", "Off"),
    ("关于", "About"),
    ("关于 Duck Toolbox", "About Duck Toolbox"),
    (
        "关掉后点「卸载」会直接开始，不再询问。",
        "When off, clicking Uninstall starts straight away without asking.",
    ),
    ("关键词", "Keywords"),
    ("关闭", "Close"),
    ("关闭全部", "Close all"),
    ("关闭其他", "Close others"),
    (
        "关闭后仅能解析公开分享，私有链接会失败。",
        "When off, only public shares resolve; private links fail.",
    ),
    (
        "关闭时只弹出一个快速面板，盖在当前工作之上，不动主窗口；打开时唤起完整主窗口（经典行为）。",
        "When off, summoning shows only a quick panel over your current work and does not touch the main window; when on, it raises the full main window (the classic behaviour).",
    ),
    (
        "关闭时按合并处理：只覆盖备份里出现的设置，本机独有的保留。",
        "When off, merge instead: only settings present in the backup are overwritten; local-only ones are kept.",
    ),
    ("关闭标签页", "Close tab"),
    ("其他", "Other"),
    (
        "内容区右栏停靠在哪一侧；目前仅工具箱页提供右栏内容，切换即时生效。",
        "Which side the content-area right sidebar docks on. Only the toolbox page has right-sidebar content for now. Applies immediately.",
    ),
    (
        "内嵌 Lucide 子集，线宽统一 1.75",
        "Bundled Lucide subset with a uniform 1.75 stroke",
    ),
    ("内置主题 {}", "Built-in theme {}"),
    (
        "内置工具已下线，能力全部来自插件。装好的插件会出现在工具箱的侧栏分类里。",
        "Built-in tools are retired; every capability now comes from plugins. Installed plugins appear as categories in the toolbox sidebar.",
    ),
    (
        "内置引擎复用本机浏览器会话，无需额外安装。",
        "The built-in engine reuses your local browser session; nothing to install.",
    ),
    ("内置浏览器（CDP）", "Built-in browser (CDP)"),
    (
        "再删除窗口位置、自定义快捷键与日志，回到初次启动的样子。",
        "Also removes window positions, custom keybindings and logs — back to a first-launch state.",
    ),
    ("写入文件", "Write files"),
    (
        "写满就轮转到下一段；一天最多留 {} 段，目录总量兜底 {}。",
        "A full file rolls into the next segment; up to {} segments per day, with a {} cap on the whole directory.",
    ),
    ("分享页优先", "Share page first"),
    ("分布在 {} 个分类", "Across {} categories"),
    ("分类", "Categories"),
    ("分组", "Group"),
    ("分钟", "minutes"),
    ("切换侧栏", "Toggle sidebar"),
    ("切换外观", "Toggle appearance"),
    (
        "列出全部已注册工具与其派生索引",
        "Lists every registered tool with its derived index",
    ),
    (
        "列表、确认与开机启动",
        "Lists, confirmations, and launch at login",
    ),
    ("列表与确认交互", "Lists & confirmations"),
    ("列表使用紧凑行高", "Use compact list rows"),
    ("刚刚", "Just now"),
    ("删除工具前弹确认", "Confirm before deleting a tool"),
    ("删除这条通知", "Delete this notification"),
    ("刷新", "Refresh"),
    ("剩余 {}", "{} free"),
    ("剪切", "Cut"),
    (
        "剪贴板内容未识别到可处理的工具",
        "No tool target recognized in the clipboard",
    ),
    ("功能正在建设中", "Coming soon"),
    (
        "勾选要放行的能力；取消勾选即拒绝，之后可在插件中心逐项调整。",
        "Tick the capabilities to allow; unticked ones are denied, and you can adjust them item by item in the Plugin Center later.",
    ),
    ("包体积", "Package size"),
    ("包含子目录", "Include subdirectories"),
    ("升级", "Upgrade"),
    ("单文件上限", "Max file size"),
    ("卸载", "Uninstall"),
    (
        "卸载插件「{}」失败：{}",
        "Failed to uninstall plugin \"{}\": {}",
    ),
    ("卸载插件前弹确认", "Ask before uninstalling a plugin"),
    ("卸载插件？", "Uninstall plugin?"),
    ("压缩", "compress"),
    ("压缩包", "archive"),
    ("压缩包解包", "Archive extraction"),
    ("压缩归档", "Archives"),
    ("原样重放", "Replay as-is"),
    ("发现新版本 {}。", "Version {} is available."),
    ("取消", "Cancel"),
    (
        "取自 workspace 版本号，与打包产物一致。",
        "Taken from the workspace version, matching the packaged build.",
    ),
    (
        "只按下了修饰键，还要再按一个普通键。",
        "Only modifiers were pressed; add a regular key as well.",
    ),
    (
        "只显示文件末尾，更早的内容没有载入。",
        "Showing the end of the file only; earlier content was not loaded.",
    ),
    ("可更新", "Update available"),
    (
        "可用占位符：{name} 原名、{index} 序号、{ext} 扩展名。",
        "Placeholders: {name} original name, {index} serial number, {ext} extension.",
    ),
    ("右侧", "Right"),
    ("右栏位置", "Right sidebar side"),
    ("同意并安装", "Agree and install"),
    ("同意并更新", "Agree and update"),
    (
        "同时派生选中导航图标色与链接文字色。",
        "Also derives the selected nav icon color and link text color.",
    ),
    ("启动", "Startup"),
    ("启动后自动检查更新", "Check for updates on launch"),
    ("启动时恢复上次窗口", "Reopen last session's windows"),
    ("启用", "Enabled"),
    ("启用唤起快捷键", "Enable the summon shortcut"),
    ("启用插件", "Enabled"),
    ("命令", "Commands"),
    ("命令片段", "Command snippets"),
    ("命令面板", "Command palette"),
    (
        "命令面板：用键盘完成导航、切换与常用动作。",
        "Command palette: navigate, switch and run common actions from the keyboard.",
    ),
    ("命名模板", "Naming template"),
    ("唤起", "Summon"),
    ("唤起时打开主窗口", "Open the main window when summoned"),
    ("嗅探引擎", "Sniffing engine"),
    ("回放", "replay"),
    ("回退到上一版本", "Roll back to the previous version"),
    ("回退失败：{}", "Rollback failed: {}"),
    ("回退已就绪", "Rollback ready"),
    ("图标路径必须是合法 UTF-8", "icon path must be valid UTF-8"),
    ("图标集", "Icon set"),
    ("图片", "image"),
    ("图片格式转换", "Image conversion"),
    (
        "在上一档的基础上，再删除已安装的插件与插件数据。",
        "Does the above, then removes installed plugins and their data.",
    ),
    (
        "在别的程序里也能用；Duck 已经在前台时再按一次会把它收起来。",
        "Works from other apps too; press it again while Duck is in front to put it away.",
    ),
    ("在设置中打开", "Open in Settings"),
    ("在访达中显示", "Reveal in Finder"),
    ("处理中…", "Working…"),
    ("备份", "Backup"),
    ("备份、恢复与重置", "Backup, restore and reset"),
    ("备份中包含日志", "Include logs"),
    ("复制诊断信息", "Copy diagnostics"),
    ("复制诊断信息失败", "Failed to copy diagnostics"),
    (
        "复用已登录会话，自动识别分享链接",
        "Reuses signed-in sessions to detect share links",
    ),
    ("复用登录会话", "Reuse signed-in session"),
    ("外观", "Appearance"),
    ("媒体", "media"),
    ("媒体处理", "Media"),
    ("字体", "Font"),
    ("字体大小", "Font size"),
    ("安装", "Install"),
    ("安装中…", "Installing…"),
    (
        "安装后，本插件会请求访问以下资源：",
        "Once installed, this plugin requests access to:",
    ),
    ("安装插件「{}」", "Install plugin \"{}\""),
    (
        "安装插件「{}」失败：{}",
        "Failed to install plugin \"{}\": {}",
    ),
    ("安装目录", "Install location"),
    ("完成", "Done"),
    (
        "完成后保留分片文件，便于排查合并问题。",
        "Keeps segment files after completion, which helps when debugging a merge.",
    ),
    ("完成后关机", "Shut down when done"),
    ("完整主题", "Full theme"),
    ("宽松", "Comfortable"),
    ("导入", "Import"),
    (
        "导入会用包里的值覆盖同名设置，本机独有的设置保留。导入前的数据会自动备份。继续？",
        "Settings present in the backup overwrite the local ones; local-only settings are kept. Your current data is backed up first. Continue?",
    ),
    (
        "导入前会自动把当前数据备份到上面的目录。",
        "Your current data is backed up to the folder above before importing.",
    ),
    ("导入失败：{}", "Import failed: {}"),
    (
        "导入将完全以备份包为准，包里没有的设置会被清掉。导入前的数据会自动备份。继续？",
        "The backup becomes the source of truth; settings missing from it are cleared. Your current data is backed up first. Continue?",
    ),
    ("导入时完全覆盖", "Replace everything on import"),
    ("导入配置", "Import config"),
    (
        "导入配置尚未接入业务逻辑",
        "Importing a config is not wired up yet",
    ),
    ("导出", "Export"),
    ("导出…", "Export…"),
    ("导出备份", "Export a backup"),
    ("导出失败：{}", "Export failed: {}"),
    ("导航", "Navigation"),
    (
        "将插件放入 {} 后重启应用。",
        "Drop plugins into {} and restart the app.",
    ),
    (
        "尚未获取插件清单，点「刷新」从市场获取。",
        "No plugin list yet; use Refresh to fetch it from the marketplace.",
    ),
    ("尚未配置更新源。", "No update source is configured yet."),
    ("展开侧栏", "Expand sidebar"),
    ("展开右侧栏", "Expand right sidebar"),
    ("工作区", "Workspace"),
    ("工具", "Tool"),
    ("工具不可用", "Tool unavailable"),
    ("工具信息", "Tool info"),
    ("工具启停与工具级设置", "Tool toggles and per-tool settings"),
    ("工具箱", "Toolbox"),
    (
        "工具箱页侧栏条目之间的留白（0 为贴紧）。",
        "Spacing between toolbox sidebar entries; 0 packs them tight.",
    ),
    (
        "工具箱：把常用小工具收进同一个窗口，启停与设置都在「设置 → 工具」。",
        "Toolbox: keep your everyday utilities in one window; enable and configure them in Settings → Tools.",
    ),
    (
        "工具设置项在设置窗口中统一调整，改动即时生效。",
        "Tool settings live in the Settings window; changes apply immediately.",
    ),
    ("工具贡献", "Tool contributions"),
    ("左侧", "Left"),
    ("差异快照（仅设置）", "Diff snapshot (settings only)"),
    (
        "已下载并校验，安装包在 {}。",
        "Downloaded and verified; the installer is at {}.",
    ),
    ("已停用", "Disabled"),
    ("已关闭。", "Turned off."),
    (
        "已创建示例工具「未命名」",
        "Created the sample tool “Untitled”",
    ),
    ("已删除 {} 个过期日志。", "Deleted {} expired log files."),
    ("已取消更新。", "Update cancelled."),
    ("已启用", "Enabled"),
    (
        "已回退到上一版本，重启后生效。",
        "Rolled back to the previous version; a restart applies it.",
    ),
    ("已复制诊断信息", "Diagnostics copied"),
    ("已安装", "Installed"),
    ("已安装工具", "Installed tools"),
    ("已安装插件", "Installed plugins"),
    (
        "已导入 {} 项设置，重启应用后完全生效。",
        "Imported {} settings; restart the app to apply everything.",
    ),
    ("已导出到 {}（{} 行）", "Exported to {} ({} lines)"),
    ("已导出到 {}（{}）", "Exported to {} ({})"),
    ("已授予 {} / {} 项能力", "{} of {} capabilities granted"),
    ("已是最新版本 {}。", "Version {} is the latest."),
    (
        "已清空 {} 张表、删除 {} 项内容，重启后生效。",
        "Cleared {} tables and removed {} items; restart to apply.",
    ),
    ("已装版本", "Installed version"),
    ("已识别为「{}」", "Recognized as \\\"{}\\\""),
    (
        "已识别为「{}」，到工具箱确认处理",
        "Recognized as \\\"{}\\\" — confirm in the toolbox",
    ),
    (
        "市场共 {} 个插件 · 上次更新：{}",
        "{} plugins in the marketplace · last updated: {}",
    ),
    ("市场源", "Marketplace source"),
    (
        "市场源与已安装插件",
        "Marketplace source and installed plugins",
    ),
    ("市场版本", "Marketplace version"),
    ("布局", "Layout"),
    ("帮助", "Help"),
    ("平均启动耗时", "Average start-up time"),
    ("平均耗时", "Average duration"),
    ("并发嗅探数", "Concurrent sniffs"),
    ("应用", "Application"),
    ("应用内 + 系统通知", "In-app + system notification"),
    ("应用内与系统通知通道", "In-app and system channels"),
    ("应用内提示", "In-app toast"),
    ("应用内横幅", "In-app banner"),
    ("开", "On"),
    ("开发工具", "Developer"),
    ("开发者", "Developer"),
    ("开发者模式", "Developer mode"),
    ("开机自动启动", "Launch at login"),
    (
        "开机自启设置失败：{}",
        "Failed to update launch at login: {}",
    ),
    ("开源许可", "Open source licenses"),
    ("开源许可窗口", "Licenses window"),
    ("引导", "Getting started"),
    ("引导窗口", "Onboarding window"),
    ("当前位置是只读的。", "The current location is read-only."),
    (
        "当前位置没有写入权限。",
        "The current location is not writable.",
    ),
    ("当前占用", "Current usage"),
    (
        "当前平台不支持就地更新。",
        "In-place updates are not supported on this platform.",
    ),
    (
        "当前数据会先自动备份到上面那个目录。重置后需要重启应用才会完全生效。",
        "Your current data is backed up to the folder above first. Restart the app after resetting for it to fully apply.",
    ),
    (
        "当前没有可配置的工具。",
        "No tools are available to configure.",
    ),
    ("当前版本", "Current version"),
    ("当前生效主题", "Active theme"),
    (
        "当前视图没有内容可导出。",
        "There is nothing in the current view to export.",
    ),
    (
        "当前运行的不是打包后的 .app。",
        "This build is not running from a packaged .app.",
    ),
    ("快捷键", "Shortcut"),
    ("快速入口", "Quick actions"),
    ("忽略", "Dismiss"),
    ("怎么用", "How to use"),
    ("恢复", "Restore"),
    ("手动配置", "Manual"),
    ("打开", "Open"),
    ("打开工作区", "Open workspace"),
    ("打开工具箱", "Open toolbox"),
    ("打开插件中心", "Open plugin center"),
    ("打开文档失败：{}", "Could not open the docs: {}"),
    ("打开日志", "Open logs"),
    ("打开日志目录", "Open log directory"),
    (
        "打开的工具会以标签页保留在这里。",
        "Open tools stay here as tabs.",
    ),
    ("打开目录", "Open folder"),
    ("执行前确认", "Confirm before running"),
    (
        "执行命令前弹出确认框，避免误触。",
        "Asks for confirmation before running a command, to avoid accidental hits.",
    ),
    ("执行模型", "Execution model"),
    ("批量", "batch"),
    ("批量重命名", "Batch rename"),
    (
        "把上次退出时开着的设置、日志等窗口一并打开，各自回到原来的位置。",
        "Reopen the settings, logs and other windows that were open at quit, each back where it was.",
    ),
    (
        "把视频页面的链接粘贴到顶栏搜索框，或用浏览器打开后点「解析」。",
        "Paste the video page URL into the top bar search box, or open it in a browser and hit Parse.",
    ),
    (
        "把设置、快捷键与已安装的插件打成一个归档文件。",
        "Packs your settings, keybindings and installed plugins into a single archive.",
    ),
    ("抓包", "capture"),
    (
        "投递到操作系统通知中心。需要已打包成应用并授权；开发构建下通常不显示。",
        "Delivered to the OS notification center. Requires a packaged app with permission granted; usually not shown in a development build.",
    ),
    ("拷贝", "Copy"),
    (
        "按 0.8 ~ 1.5 倍缩放全部尺寸 token，并同步重算密度档位。",
        "Scales every size token by 0.8–1.5× and recomputes density tiers.",
    ),
    (
        "按 KB/s 输入，0 视为无限制。",
        "Enter KB/s; 0 means unlimited.",
    ),
    (
        "按上面的保留时长删掉旧日志，不等下一次自动清理。",
        "Delete old logs using the retention period above, without waiting for the next automatic cleanup.",
    ),
    (
        "按下组合键…（Esc 取消）",
        "Press a combination… (Esc to cancel)",
    ),
    (
        "按模板与正则整理文件名",
        "Organizes file names with templates and regex",
    ),
    (
        "换个关键词试试，或到插件市场安装更多工具。",
        "Try another keyword, or install more tools from the plugin marketplace.",
    ),
    ("排序", "Sort"),
    (
        "控件高度与行高的疏密档位，与界面缩放叠加生效；「紧凑」同屏显示更多内容，「宽松」更易点选。",
        "Density of control heights and row heights, combined with UI scale. Compact fits more on screen; comfortable is easier to hit.",
    ),
    ("插件", "Plugins"),
    ("插件「{}」已停用", "Plugin \"{}\" disabled"),
    ("插件「{}」已卸载。", "Plugin \"{}\" uninstalled."),
    ("插件「{}」已启用", "Plugin \"{}\" enabled"),
    (
        "插件「{}」已安装（v{}）。",
        "Plugin \"{}\" installed (v{}).",
    ),
    (
        "插件「{}」的权限已更新",
        "Permissions for plugin \"{}\" updated",
    ),
    ("插件中心", "Plugin Center"),
    ("插件中心窗口", "Plugin center window"),
    ("插件信息", "Plugin details"),
    (
        "插件包请从插件中心安装。",
        "Install plugin packages from the Plugin Center.",
    ),
    ("插件市场源", "Plugin marketplace source"),
    ("插件执行失败：{}", "Plugin execution failed: {}"),
    ("插件数量", "Plugins"),
    (
        "插件清单已更新，共 {} 个插件。",
        "Plugin list updated: {} plugins.",
    ),
    (
        "插件清单的获取地址；留空回落默认源，保存后立即重新获取。",
        "URL the plugin list is fetched from; leave it empty to use the default source. Saving refetches immediately.",
    ),
    (
        "插件：在插件市场挑选并安装新工具，装完就在工具箱里。",
        "Plugins: browse the plugin center to install new tools; they show up in the toolbox.",
    ),
    ("搜索工具、命令…", "Search tools, commands…"),
    ("搜索插件…", "Search plugins…"),
    ("搜索设置...", "Search settings…"),
    ("撤消", "Undo"),
    ("收起侧栏", "Collapse sidebar"),
    ("收起右侧栏", "Collapse right sidebar"),
    (
        "放宽级别或换一个关键字，或清空过滤条件看全部。",
        "Loosen the level or try another keyword, or clear the filters to see everything.",
    ),
    ("数据", "Data"),
    ("数据库", "Database"),
    ("数据文件", "Data file"),
    ("文件名", "filename"),
    ("文件名编码", "Filename encoding"),
    ("文件图标", "File icons"),
    ("文件访问", "File access"),
    ("文档转换", "Documents"),
    ("新增", "New"),
    ("新建工具", "New tool"),
    (
        "无法打开{}，详见日志。",
        "Could not open {}; see the logs for details.",
    ),
    ("无法重启应用：{}", "Could not relaunch the app: {}"),
    ("无通知", "No notifications"),
    ("无限制", "Unlimited"),
    ("日志", "Logs"),
    (
        "日志体积较大，只在需要排查问题时才勾选。",
        "Logs are bulky; enable only when troubleshooting.",
    ),
    ("日志保留", "Log retention"),
    ("日志文件与运行记录", "Log files and run history"),
    ("日志目录", "Log directory"),
    (
        "日志目录里所有 .log 的总大小。",
        "Total size of every .log in the log directory.",
    ),
    ("日志窗口", "Logs window"),
    ("显示 {} / {} 行", "{} of {} lines shown"),
    ("显示全部", "Show All"),
    (
        "显示标记为开发者的工具，仅在排查问题时使用。",
        "Shows tools marked as developer-only. Use it for troubleshooting only.",
    ),
    ("显示活动栏", "Show activity bar"),
    ("显示通知铃铛", "Show notification bell"),
    ("显示顶栏附加入口", "Show extra top bar entries"),
    ("暗色", "Dark"),
    ("暗色主题", "Dark theme"),
    (
        "暗色预设 Default Dark，强调色取自主题 token",
        "Dark preset Default Dark; the accent comes from theme tokens",
    ),
    ("更多", "More"),
    ("更多主题", "More themes"),
    ("更多操作", "More actions"),
    ("更新", "Update"),
    (
        "更新后，本插件会请求访问以下资源：",
        "Once updated, this plugin requests access to:",
    ),
    ("更新失败：{}", "Update failed: {}"),
    (
        "更新已安装，重启后生效。",
        "The update is installed; it takes effect after a restart.",
    ),
    ("更新已就绪", "Update ready"),
    ("更新插件「{}」", "Update plugin \"{}\""),
    ("更新源", "Update source"),
    ("更新源与版本", "Update source and version"),
    ("更新状态", "Update status"),
    ("最小化", "Minimize"),
    ("最新发布", "Latest release"),
    (
        "最近 100 条投递记录，只在本次运行内保留。",
        "The last 100 deliveries, kept only for this run.",
    ),
    ("最近 {} 次启动的平均值", "Average of the last {} launches"),
    ("最近使用", "Recently used"),
    ("服务", "Services"),
    ("未发现插件", "No plugins found"),
    (
        "未在本机授予的能力一律被拒绝；卸载插件即收回全部权限。",
        "Capabilities not granted on this machine are denied; uninstalling the plugin takes them all back.",
    ),
    ("未安装", "Not installed"),
    ("未生效：{}", "Not working: {}"),
    ("未请求任何能力", "No capabilities requested"),
    ("未配置", "Not configured"),
    ("本周执行", "Runs this week"),
    ("本地开发", "Local development"),
    (
        "本机正文字号；其余文字与行高按同一比例适配，与界面缩放叠加生效。",
        "Body text size for this machine; all other text and line heights scale by the same ratio, stacked on top of the UI scale.",
    ),
    ("本次启动后开始记录", "Recording starts with this launch"),
    (
        "本次更新新增 {} 项权限，请确认。",
        "This update adds {} new permission(s). Please confirm.",
    ),
    ("本程序许可", "Application license"),
    ("查看开源许可", "View open source licenses"),
    ("查看文档", "View docs"),
    ("标准", "Standard"),
    ("标签", "Tags"),
    ("检查更新", "Check for updates"),
    ("检查更新失败：{}", "Update check failed: {}"),
    (
        "检测到 {} 次未查看的崩溃，可在「设置 → 关于」页查看诊断信息。",
        "Detected {} unviewed crashes. See Settings → About for diagnostics.",
    ),
    ("概览", "Overview"),
    ("欢迎使用 Duck Toolbox", "Welcome to Duck Toolbox"),
    ("正在下载更新… {} / {}", "Downloading update… {} / {}"),
    ("正在安装更新…", "Installing the update…"),
    ("正在校验更新…", "Verifying the update…"),
    ("正在检查更新…", "Checking for updates…"),
    ("正在获取插件清单…", "Fetching the plugin list…"),
    (
        "此模块暂未提供侧栏内容。",
        "This module has no sidebar content yet.",
    ),
    (
        "每次从工具箱运行该工具后累加。",
        "Increases every time the tool is run from the toolbox.",
    ),
    ("永久保留", "Keep forever"),
    ("没有匹配的工具。", "No matching tools."),
    ("没有匹配的插件", "No matching plugins"),
    ("没有匹配的行", "No matching lines"),
    ("没有匹配的项", "Nothing matches"),
    (
        "没有找到这个工具，可能是插件尚未安装。",
        "That tool is not available. Its plugin may not be installed.",
    ),
    ("没有日志文件", "No log files"),
    ("没有需要清理的日志。", "No log files needed cleaning."),
    ("法律信息", "Legal"),
    ("注册表", "registry"),
    ("注册表检查", "Registry inspector"),
    ("浅色", "Light"),
    ("深色", "Dark"),
    ("清单无效", "Invalid manifest"),
    ("清理", "Clean up"),
    ("清空", "Clear all"),
    (
        "清空所有设置与使用记录，保留插件与日志。",
        "Clears all settings and usage history; keeps plugins and logs.",
    ),
    (
        "演示：全部开始（未接入任务后端）",
        "Demo: resume all (no task backend wired up)",
    ),
    (
        "演示：全部暂停（未接入任务后端）",
        "Demo: pause all (no task backend wired up)",
    ),
    (
        "演示：刷新（未接入工具后端）",
        "Demo: refresh (tool backend not wired up)",
    ),
    (
        "演示：完成后关机 · {} 分钟（未接入宿主电源能力）",
        "Demo: shut down when done · {} min (no power control wired up)",
    ),
    (
        "演示：完成后关机 · 立即（未接入宿主电源能力）",
        "Demo: shut down when done · immediately (no power control wired up)",
    ),
    ("片段", "snippet"),
    ("版本", "Version"),
    ("版本、更新与发布记录", "Version, updates and release notes"),
    ("版本与当前生效主题", "Version and active theme"),
    ("版本发布记录", "Release notes"),
    ("状态", "Status"),
    ("独立进程", "Standalone process"),
    ("用「{}」处理", "Handle with \\\"{}\\\""),
    (
        "由 Cargo.lock 汇总，随程序内置，离线可查。",
        "Summarised from Cargo.lock and embedded in the app; readable offline.",
    ),
    ("界面", "Interface"),
    ("界面密度", "Interface density"),
    (
        "界面显示语言；切换后立即生效，界面文案与提示都会跟随。",
        "Display language; takes effect immediately across the interface.",
    ),
    ("界面缩放", "UI scale"),
    (
        "留空即用内置默认源（GitHub Releases）。",
        "Leave empty to use the built-in default (GitHub Releases).",
    ),
    (
        "登录系统后自动运行 Duck。",
        "Run Duck automatically after signing in.",
    ),
    ("目标环境", "Target environment"),
    ("直链", "direct link"),
    ("直链优先", "Direct link first"),
    ("确定", "OK"),
    ("确认重置？", "Confirm reset?"),
    ("秒", "s"),
    ("窗口", "Window"),
    (
        "窗口右上角的提示条。无论哪一档，通知都会记入历史（顶栏铃铛）。",
        "The toast strip at the top-right of the window. At any level, notifications are still recorded in history (the bell in the top bar).",
    ),
    (
        "窗口最左侧的功能轨（概览 / 工具箱 / 设置…）。关闭后内容区占满窗口。",
        "The rail on the far left of the window (Overview / Toolbox / Settings…). When off, content fills the window.",
    ),
    ("立即", "Immediately"),
    ("立即清理", "Clean up now"),
    ("第 {} 步，共 {} 步", "Step {} of {}"),
    (
        "第一次启动时的三步向导，随时可以再看一遍。",
        "The three-step wizard shown on first launch; you can replay it anytime.",
    ),
    ("第三方组件", "Third-party components"),
    ("第三方组件许可汇总", "Third-party license summary"),
    ("简体中文", "简体中文"),
    ("简约", "Minimal"),
    ("粘贴", "Paste"),
    ("系统", "System"),
    ("系统代理", "System proxy"),
    (
        "系统拒绝了这个组合，多半已被其他程序占用。",
        "The system refused this combination, so another program most likely owns it.",
    ),
    ("系统通知", "System notification"),
    ("系统默认浏览器", "System default browser"),
    ("紧凑", "Compact"),
    ("累计崩溃", "Crashes"),
    ("累计执行", "Total runs"),
    ("终端", "terminal"),
    (
        "编译 Windows 资源（duck.ico）失败",
        "failed to compile Windows resources (duck.ico)",
    ),
    ("编辑", "Edit"),
    ("缩放", "Zoom"),
    (
        "缩放尺寸、间距、图标与文字档位。",
        "Scales sizes, spacing, icons and text tiers.",
    ),
    ("网盘", "netdisk"),
    ("网盘直链解析", "Cloud drive direct links"),
    ("网络", "Network"),
    ("能力声明", "Capabilities"),
    ("能力授权", "Capability grants"),
    ("自动", "Auto"),
    ("自动刷新", "Auto refresh"),
    ("自动备份位置", "Automatic backup folder"),
    (
        "自动探测编码，处理多分卷",
        "Detects encoding and handles multi-part archives",
    ),
    ("自动检测", "Auto detect"),
    (
        "自动检测失败时，可手动指定 UTF-8 或 GBK。",
        "Specify UTF-8 or GBK manually when auto-detection fails.",
    ),
    ("自定义...", "Custom…"),
    ("自定义延迟", "Custom delay"),
    ("自定义强调色", "Custom accent"),
    ("自定义限速", "Custom limit"),
    (
        "至少要按一个修饰键：⌥ / ⌃ / ⌘（Windows 上是 Alt / Ctrl / Win）。",
        "At least one modifier is required: ⌥ / ⌃ / ⌘ (Alt / Ctrl / Win on Windows).",
    ),
    ("英文", "English"),
    ("获取中…", "Fetching…"),
    (
        "获取插件清单失败：{}",
        "Failed to fetch the plugin list: {}",
    ),
    ("行为", "Behavior"),
    ("视图", "View"),
    ("视频统一解析", "Unified video parsing"),
    ("解压", "extract"),
    ("解压后删除源文件", "Delete source after extracting"),
    ("解析", "parse"),
    (
        "解析引擎会复用本机浏览器已登录的会话，因此会员内容也能取到直链。",
        "The parser reuses your local browser session, so member-only content resolves too.",
    ),
    ("解析顺序", "Resolution order"),
    ("设置", "Settings"),
    ("设置窗口", "Settings window"),
    ("设置项", "Settings"),
    ("识别粘贴", "Smart paste"),
    ("诊断信息", "Diagnostics"),
    ("诊断操作", "Diagnostic actions"),
    ("试一下", "Try it"),
    (
        "该工具已被移除，或开发者模式已关闭。",
        "This tool has been removed, or developer mode is off.",
    ),
    (
        "该工具暂无独立设置。",
        "This tool has no settings of its own yet.",
    ),
    (
        "该工具没有提供帮助内容。",
        "This tool provides no help content.",
    ),
    (
        "该工具的操作区域尚未接入，后端就绪后在此展示。",
        "This tool's workspace is not wired up yet; it will appear here once the backend is ready.",
    ),
    ("该插件没有工具贡献", "This plugin contributes no tools"),
    ("语言", "Language"),
    (
        "语言与明暗都能随时在「设置 → 外观」里改。",
        "Language and light/dark can be changed anytime in Settings → Appearance.",
    ),
    ("请求", "request"),
    ("读取失败：{}", "Could not read: {}"),
    ("读取文件", "Read files"),
    (
        "调整筛选条件或清空搜索词。",
        "Adjust the filters or clear the search.",
    ),
    ("质量", "Quality"),
    ("起始序号", "Start index"),
    (
        "超出保留期的日志会被自动删除；日志按天滚动，一天一个文件。",
        "Logs older than the retention period are deleted automatically; logs roll daily, one file per day.",
    ),
    ("超时", "Timeout"),
    ("跟随主题", "Follow theme"),
    ("跟随系统", "Follow system"),
    (
        "跟随系统时随 macOS 外观切换，切换即时生效。",
        "Follows the macOS appearance and switches instantly.",
    ),
    (
        "跟随系统时随系统语言切换。",
        "Follows the system language when set to system.",
    ),
    (
        "跟随系统，当前生效的是{}",
        "Following the system; currently {}",
    ),
    ("跳过", "Skip"),
    ("转换", "convert"),
    ("较上周 +12.4%", "+12.4% vs last week"),
    ("较上周 +3", "+3 vs last week"),
    ("较上周 -6.1%", "-6.1% vs last week"),
    ("输出格式", "Output format"),
    ("过滤日志…", "Filter logs…"),
    ("运行", "Run"),
    (
        "运行一次应用就会在日志目录里留下 .log。",
        "Running the app leaves a .log in the log directory.",
    ),
    ("运行平台", "Platform"),
    ("运行环境", "Environment"),
    ("返回", "Back"),
    ("返回列表", "Back to list"),
    (
        "这一步是可选的，之后可以在左边的插件中心随时回来。",
        "This step is optional; you can come back to the plugin center anytime.",
    ),
    ("这是一条应用内提示", "This is an in-app toast"),
    ("这是一条系统通知", "This is a system notification"),
    ("进程执行", "Process execution"),
    ("退出", "Quit"),
    ("退出 Duck Toolbox", "Quit Duck Toolbox"),
    ("选择", "Select"),
    ("选择外观", "Pick a look"),
    ("选择文件…", "Choose file…"),
    (
        "选择此电脑上可用的字体。字体不可用时回退到主题字体；新安装的字体可能需要重启应用后才能读取。",
        "Pick a font installed on this computer. Missing fonts fall back to the theme font; newly installed fonts may need an app restart.",
    ),
    ("通用", "General"),
    ("通知", "Notifications"),
    ("通知历史", "Notification history"),
    ("通知通道", "Channels"),
    (
        "配置更新源并检查更新后，这里会显示源上版本的发布说明。",
        "Set an update source and check for updates to see the release notes published there.",
    ),
    ("配置源", "Configure source"),
    ("配色", "Colors"),
    ("重做", "Redo"),
    (
        "重启后运行上一版本。现在重启？",
        "The previous version runs after a restart. Restart now?",
    ),
    (
        "重启后运行新版本。现在重启？",
        "The new version runs after a restart. Restart now?",
    ),
    ("重启并应用更新", "Restart and apply the update"),
    ("重启并运行上一版本", "Restart into the previous version"),
    ("重命名", "rename"),
    ("重新查看引导", "Replay the wizard"),
    ("重置", "Reset"),
    ("重置失败：{}", "Reset failed: {}"),
    ("重置并删插件", "Reset and remove plugins"),
    ("重置应用设置", "Reset app settings"),
    ("重置设置", "Reset settings"),
    ("重置设置与插件", "Reset settings and plugins"),
    ("重载", "Reload"),
    ("隐藏 Duck Toolbox", "Hide Duck Toolbox"),
    ("隐藏其他", "Hide Others"),
    (
        "静态 JSON manifest 的 https 地址。",
        "https URL of a static JSON manifest.",
    ),
    ("面板", "Panel"),
    (
        "顶栏的插件中心按钮与语言切换；关闭后二者一并隐藏。",
        "The plugin marketplace button and the language switch in the top bar. Turning this off hides both.",
    ),
    (
        "顶栏的通知中心入口；关闭后通知仍记入历史，只是顶栏不再显示铃铛。",
        "The notification center entry in the top bar. When off, notifications are still recorded in history; only the bell is hidden.",
    ),
    ("项目管理", "Projects"),
    ("预发布", "Staging"),
    ("首页", "Home"),
    ("（未设置）", "(not set)"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// 表必须有序，否则二分查找会漏词。
    #[test]
    fn table_is_sorted() {
        for pair in EN.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "英译表顺序错误：{} 应在 {} 之后",
                pair[0].0,
                pair[1].0
            );
        }
    }

    /// 取词与占位符替换的基本行为。
    #[test]
    fn lookups_and_placeholders() {
        assert_eq!(t(Lang::Zh, "外观"), "外观");
        assert_eq!(t(Lang::En, "外观"), "Appearance");
        assert_eq!(t(Lang::En, "未登记的文案"), "未登记的文案");
        assert_eq!(tf(Lang::En, "{} 个工具", &["7"]), "7 tools");
        assert_eq!(tf(Lang::Zh, "剩余 {}", &["12 GB"]), "剩余 12 GB");
        // 参数多于占位符时多余的忽略，少于时保留未替换的占位符。
        assert_eq!(tf(Lang::En, "剩余 {}", &[]), "{} free");
        assert_eq!(
            Locale::ALL.map(Locale::wire_name),
            ["system", "zh-Hans", "en"]
        );
        assert_eq!(Locale::from_wire("en"), Locale::En);
        assert_eq!(Locale::from_wire("zh-Hans"), Locale::ZhHans);
        assert_eq!(Locale::from_wire("nope"), Locale::System);
    }

    /// 语言标签解析（`LANG` / `LC_*` / macOS `AppleLocale` 同一条规则）。
    #[test]
    fn language_tags() {
        assert_eq!(lang_from_tag("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(lang_from_tag("zh-Hans"), Some(Lang::Zh));
        assert_eq!(lang_from_tag("en_US.UTF-8"), Some(Lang::En));
        assert_eq!(lang_from_tag("ja_JP"), Some(Lang::En));
        assert_eq!(lang_from_tag("C"), None);
        assert_eq!(lang_from_tag("POSIX"), None);
        assert_eq!(lang_from_tag("  "), None);
    }

    /// 源码里的中文文案必须全部登记，避免新增文案漏翻。
    ///
    /// 只扫非测试代码：`#[cfg(test)]` 之后（含断言消息、panic 文案）不属于界面文案。
    #[test]
    fn every_chinese_literal_is_translated() {
        let mut missing: Vec<String> = Vec::new();
        for file in rust_sources() {
            let text = std::fs::read_to_string(&file).expect("读取源码");
            for (line_index, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("#[cfg(test)]") {
                    break;
                }
                if trimmed.starts_with("//") {
                    continue;
                }
                for literal in literals(line) {
                    if !contains_cjk(literal) {
                        continue;
                    }
                    if translate_en(literal).is_none() {
                        missing.push(format!("{}:{} → {literal}", file.display(), line_index + 1));
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "以下文案缺少英译：\n{}",
            missing.join("\n")
        );
    }

    fn workspace_crates() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates 目录")
            .to_path_buf()
    }

    fn rust_sources() -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect(&workspace_crates(), &mut files);
        files.sort();
        files
    }

    fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // 集成测试目录（`crates/*/tests/`）不是界面代码：里面的断言与
                // panic 文案不进 EN 表，否则守卫会把测试夹具当文案拦下。
                if path.file_name().is_some_and(|name| name == "tests") {
                    continue;
                }
                collect(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }

    /// 极简字面量扫描：逐行取 `"…"` 之间的内容（字符串内的 `\"` 视为转义）。
    fn literals(line: &str) -> Vec<&str> {
        let mut found = Vec::new();
        let mut start: Option<usize> = None;
        let mut escaped = false;
        for (index, ch) in line.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' if start.is_some() => escaped = true,
                '"' => match start.take() {
                    Some(from) => found.push(&line[from..index]),
                    None => start = Some(index + 1),
                },
                _ => {}
            }
        }
        found
    }

    fn contains_cjk(text: &str) -> bool {
        text.chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
    }
}
