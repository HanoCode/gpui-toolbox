//! 设置窗口的数据快照与选择器状态（不含渲染代码）。

use super::*;

/// 工具列的一行（注册表里摊平出来的 `'static` 快照）。
///
/// 渲染期要一边读注册表一边持 `&mut Context` 挂回调，两者不能同时借用，
/// 所以先把要用的字段抄出来。
pub struct ToolPick {
    pub(super) id: &'static str,
    pub(super) name: &'static str,
    pub(super) icon: DuckIcon,
    pub(super) beta: bool,
    pub(super) settings: &'static [ToolSettingSpec],
}

/// 「字体」下拉的可搜索选项：`value` 为字体族（空串 = 跟随主题字体）。
#[derive(Clone)]
pub struct FontOption {
    pub(super) value: SharedString,
    pub(super) label: SharedString,
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

pub type FontSelectState = SelectState<SearchableVec<FontOption>>;

/// 枚举本机可用字体族（非空、排序、去重），首项为「使用主题字体」。
pub(super) fn font_options(cx: &App) -> SearchableVec<FontOption> {
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
pub(super) const ICON_PACKS: [(&str, &str); 4] = [
    ("system", "系统"),
    ("lucide", "简约"),
    ("material", "Material"),
    ("catppuccin", "Catppuccin"),
];

/// 图标包预览样例：图标 + 品牌色（`None` 跟随 `mutedForeground`）。
pub(super) fn icon_pack_preview(pack: &str) -> Vec<(DuckIcon, Option<u32>)> {
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
/// 「AI」页正在编辑的厂商草稿。
///
/// 输入框是**实体**而不是 `String`：`InputState` 自己管光标、选区与输入法组合，
/// 每次渲染重新造一个会把这些全丢掉（中文输入到一半就会被打断）。
pub struct AiDraft {
    /// `None` = 新增（保存时才生成 id）；`Some(id)` = 编辑已有档案。
    pub(super) id: Option<String>,
    /// 当前选中的模板 id（决定端点、预置模型与是否需要密钥）。
    pub(super) template_id: String,
    pub(super) name: Entity<InputState>,
    pub(super) base_url: Entity<InputState>,
    pub(super) model: Entity<InputState>,
    /// API key（掩码框）。编辑已有档案时**解密回填**：
    /// 清空它就是「清除密钥」的明确意图——与终端页的连接口令同一个口径
    /// （那里也是解密回显，见 `pages/terminal/helpers.rs::apply_password`）。
    pub(super) api_key: Entity<InputState>,
    /// 从服务端 `GET /models` 拉到的模型清单（空 = 还没拉，或拉失败）。
    ///
    /// 与模板自带的 `default_models` **分开存**：那份是离线兜底，这份是权威清单。
    /// 拉到了就优先展示，但拉失败不该把兜底一起清掉 —— 断网时用户还得能挑模型。
    pub(super) live_models: Vec<String>,
    /// 正在拉取清单。用来防连点，也给提示文案一个「进行中」的态。
    pub(super) models_loading: bool,
    /// 拉取失败的原因（空 = 没有）。与 `error` 分开：它只说清单拉不到，
    /// 不该挡住「保存档案」这件事。
    pub(super) models_error: Option<SharedString>,
    /// 保存被拒的原因（空 = 没有）。
    pub(super) error: Option<SharedString>,
}

/// 知识库索引的一份快照（来源清单 + 统计）。
///
/// 单独存一份而不是每次渲染去查：`KbIndex::open_default()` 要打开 `ai.db`，
/// 那是 I/O，不是渲染该做的事。刷新时机见 [`SettingsWindowView::ai_kb_reload`]。
pub struct KbSnapshot {
    pub(super) sources: Vec<KbSource>,
    pub(super) stats: KbStats,
}

/// 后台索引线程 → 前台的消息。
pub enum KbEvent {
    /// 扫描 / 分块进度（内核的 [`IndexProgress`] 原样转发）。
    Progress(IndexProgress),
    /// 收尾。`added` = 这次新登记成功的来源；`stats` = 收工后的统计；
    /// `error` = 整个任务失败（单个来源失败走 `Progress(Failed)`，不算整体失败）。
    Done {
        added: Option<KbSource>,
        stats: Option<KbStats>,
        error: Option<String>,
    },
}

/// 这次索引任务的意图——决定收尾时怎么更新来源清单。
pub enum KbJobKind {
    /// 「重建索引」：清单不变，只重扫。
    Rebuild,
    /// 「添加目录」：成功后要把这个来源加进清单。
    Add(KbSource),
}

/// 正在跑（或刚跑完、还没被前台收走）的一次索引任务。
///
/// **不记 [`KbJobKind`]**：收尾时怎么合并来源清单，靠线程回传的 `added` 判断就够了；
/// 记一份意图只是多一个要与 `added` 保持一致的字段（错配了反而更难查）。
pub struct KbJob {
    /// 后台线程持有另一端；前台用 `try_recv` 轮询（`std::sync::mpsc` 无法 `await`）。
    pub(super) receiver: mpsc::Receiver<KbEvent>,
    /// 停止位。置位后线程在**下一批块之间**退出，不是立刻——正在写的那批要让事务收尾。
    pub(super) cancel: Arc<AtomicBool>,
    /// 进度日志（尾部保留 [`KB_LOG_LIMIT`] 行）。
    pub(super) lines: Vec<SharedString>,
    /// 已收到收尾事件。任务**不会**在这一帧被丢掉（日志要留在屏上），
    /// 所以界面靠它区分「还在跑」与「跑完了」。
    pub(super) finished: bool,
}
