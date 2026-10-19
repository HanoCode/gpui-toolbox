//! 插件工具的通用执行页（声明式 IO，ADR-1）。
//!
//! 插件在清单 `[tools.io]` 里声明「输入字段 + 参数 + 输出形状」，本页按声明
//! 渲染**一套**界面，不为单个工具写页面——声明即契约（见
//! `docs/ifm-plugin-port-plan.md`）。
//!
//! 布局对齐参考项目 `ifm_ai_tools` 的工具页原型（见
//! `docs/ifm-ui-layout-analysis.md` §5 原型 B/C）：参数是**行内条**（10px 标签 +
//! 窄控件、按需折行），主/次操作按钮在参数下方的内容流里，左侧输入 / 右侧结果
//! 双栏（标签在上、面板撑满、动作按钮在面板下方）。
//!
//! 状态挂在视图实体上：工具箱页为每条打开的路由各建一个 [`ToolRunView`]
//! （见 `toolbox.rs` 的 `ToolRoute`），切走再切回来，填过的参数与上次结果
//! 原样保留。
//!
//! 执行走 `duck-plugin-js` 轨：沙箱在后台线程跑（10 秒上限），结果回主线程
//! 后**落进结果区**（不再是只有一条 toast 的过渡形态），通知与日志口径与
//! 旧路径完全一致（[`crate::plugins::emit_outcome`]）。

use duck_components::{
    ButtonVariant, DuckIcon, button, caption_heading,
    content::{
        DiffLine, DiffLineKind, DiffSection, DiffSegment, DiffSummary, diff_summary, diff_view,
        progress_bar,
    },
    dropdown_select, dropdown_select_searchable, field_error, icon_text_button, meta_text, notify,
    selectable_text, switch_control,
};
use duck_plugin::{
    ExecutionModel, FieldKind, FieldSpec, FieldsLayout, FileAccess, IoLayout, IoSpec, OutputKind,
    ParamKind, ParamSpec, ParamStyle, PathPick, PluginError, TableDoc,
};
use duck_theme::{Lang, active_theme, tr, trf};
use duck_tools::ToolRegistry;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ObjectFit, ParentElement, Render, Role, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled, StyledImage as _, UniformListScrollHandle, Window,
    div, img, prelude::FluentBuilder as _, px, relative, uniform_list,
};
use gpui_component::{
    Icon, h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    v_flex,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::pages::tool_table::{TableEditor, render_table};
use crate::plugins::PluginRunContext;

/// 下拉候选超过这个数就换成可搜索下拉（顶部带搜索框）。
///
/// 判据是「要不要滚屏找」：8 项以内一眼能扫完，25 项（国家/地区）就得滚动，
/// 而滚动时看不见全部候选，打字过滤反而更快。
const SEARCHABLE_SELECT_THRESHOLD: usize = 12;

/// 多行文本域的自动生长范围（行数）。
const TEXTAREA_MIN_ROWS: usize = 4;
const TEXTAREA_MAX_ROWS: usize = 14;

/// 前台拉取进度事件的间隔。
///
/// 长任务插件（合并 / 比对）一秒能吐几十行日志，150ms 聚合一次既跟得上，
/// 也不会把一次运行的几千次 `notify` 全压在主线程上。
const PROGRESS_POLL: Duration = Duration::from_millis(150);

/// 过程日志最多保留的行数（环形丢弃最旧的）。
///
/// 合并一个版本包就是几千行：全留会吃内存、也会让 `uniform_list` 的行高计算
/// 失真。参考项目同样是「日志窗口」，翻不到更早的历史属正常。
const LOG_MAX_LINES: usize = 2000;
/// 超过上限时一次丢多少行（避免每来一行都搬一次内存）。
const LOG_TRIM_LINES: usize = 200;
/// 日志行的固定行高（`uniform_list` 要求等高）。
const LOG_ROW_HEIGHT: f32 = 17.;

/// 长任务页（`layout = "job"`）右栏日志的固定宽度（参考原版 Tailwind `w-80`）。
const JOB_LOG_WIDTH: f32 = 320.;

/// 过程日志的级别（process 轨 `log` 事件自报；缺省 = info）。
///
/// 只用来给日志行着色：级别是 `error` 不会把一次运行判失败——那是
/// `error` 事件 / 非零退出码的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogLevel {
    Info,
    Warn,
    Error,
    Success,
}

impl LogLevel {
    fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("warn") => Self::Warn,
            Some("error") => Self::Error,
            Some("success") => Self::Success,
            _ => Self::Info,
        }
    }
}

/// 一条过程日志（接收时刻的本机时间打点 + 级别）。
#[derive(Clone)]
struct LogLine {
    time: String,
    level: LogLevel,
    text: String,
}

/// 长任务页的统计卡数据（插件结束时发的 `body`）。
///
/// 形状是 `{"stats":[{"label":…,"value":…}…],"output":…}`：标签在插件侧就取好词
/// （插件表 → 源串），宿主只负责摆格子。没有 body 或形状不符 = `None`，
/// 长任务页退回纯文本结果框。
struct JobStats {
    entries: Vec<(String, String)>,
    /// 结果输出位置（目录 / 文件路径），整行渲染。
    output: String,
}

impl JobStats {
    fn decode(body: Option<&serde_json::Value>) -> Option<Self> {
        let entries = body?.get("stats")?.as_array()?;
        if entries.is_empty() {
            return None;
        }
        let parsed: Vec<(String, String)> = entries
            .iter()
            .filter_map(|entry| {
                let label = entry.get("label")?.as_str()?.to_owned();
                let value = entry.get("value")?.as_str()?.to_owned();
                Some((label, value))
            })
            .collect();
        if parsed.is_empty() {
            return None;
        }
        Some(Self {
            entries: parsed,
            output: body
                .and_then(|body| body.get("output"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }
}

/// 输入字段对应的控件状态（与 `io.fields` 按下标一一对应）。
enum FieldControl {
    /// 单行文本。
    Text(Entity<InputState>),
    /// 多行文本域。
    Textarea(Entity<TextareaState>),
}

/// 参数项对应的控件状态（与 `io.params` 按下标一一对应）。
enum ParamControl {
    /// 单行文本。
    Text(Entity<InputState>),
    /// 路径（输入框 + 「选择…」：原生面板按 `pick` 选文件或目录）。
    Path(Entity<InputState>),
    /// 数值（文本框承载，提交前校验可解析）。
    Number(Entity<InputState>),
    /// 开关。
    Switch(bool),
    /// 下拉 / 药丸选择（存选中值；`pills` 与 `select` 提交值完全一致）。
    Select(String),
    /// 只读展示（不参与运行请求；页内扫描回填）。
    Display(String),
    /// 区间（两个数值输入；提交为 `{id}` 与 `{id}__max`）。
    Range(Entity<InputState>, Entity<InputState>),
}

/// 一次成功运行的产出（渲染进结果区）。
struct RunOutput {
    text: String,
    kind: OutputKind,
    /// 输出标签（插件表源串；空 = 宿主缺省「结果」）。
    label: String,
    /// 结构化差异（`kind = "diff"` 且拿到了可用的行数据时才有）。
    ///
    /// 为 `None` 时 `diff` 形状退回纯文本渲染——「有差异才画差异」，绝不为了一行
    /// 两列的空壳把普通输出也塞进双栏。
    diff: Option<DiffPayload>,
    /// 结构化表格（`kind = "table"`；body 缺省时从 Markdown 表解析）。
    ///
    /// 为 `None` 时同样退回纯文本：不是每份「表格」都值得画成表。
    table: Option<TableDoc>,
    /// 可编辑状态（清单声明了 `[io.output.cells]` 且拿到了表时才有）。
    ///
    /// 挂在 `RunOutput` 上而不是页面实体上：编辑状态的生命周期就该和「这一份
    /// 结果」一致——结果换了，编辑状态跟着换，不会拿旧表的改动去改新表。
    editor: Option<Entity<TableEditor>>,
    /// 内联预览的文件路径（`kind = "preview"` 且 body 里给了可用路径时才有）。
    ///
    /// 为 `None` 时按 `fallback` 形状渲染——预览是增强，不是前置条件。
    preview: Option<std::path::PathBuf>,
}

/// 差异渲染所需的成品（已从插件的 wire 结构映射过来）。
///
/// 在**结果回来那一刻**算好，而不是每帧算：两万行的比对每帧重映射一遍，滚动就会掉帧。
struct DiffPayload {
    sections: std::rc::Rc<Vec<DiffSection>>,
    summary: DiffSummary,
}

/// 一次执行的原始产出（两条轨的公共形状）。
///
/// JS 轨没有结构化通道（沙箱只回一段字符串），所以它的 `body` 永远是 `None`；
/// `kind = "diff"` 的 JS 插件靠 [`DiffPayload::decode`] 解析 `output` 里的
/// unified diff 来兜底。
struct Executed {
    output: String,
    body: Option<serde_json::Value>,
}

impl Executed {
    fn text_only(output: String) -> Self {
        Self { output, body: None }
    }
}

/// 预览文件路径的可渲染判定。
///
/// `kind = "preview"` 的供给是 body 里的 `{"preview": "<路径>"}`；路径必须是
/// **已落盘**的文件，扩展名落在 GPUI 图片管线认识的集合里（光栅格式 +
/// SVG）。任何一环不满足都返回 `None`——宿主按 `fallback` 渲染文本，
/// 绝不给用户一个裂图。
fn decode_preview(body: Option<&serde_json::Value>) -> Option<std::path::PathBuf> {
    const SUPPORTED: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "svgz"];
    let raw = body?.get("preview")?.as_str()?;
    let path = std::path::Path::new(raw);
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if !SUPPORTED.contains(&extension.as_str()) {
        return None;
    }
    if !path.is_file() {
        return None;
    }
    Some(path.to_path_buf())
}

impl DiffPayload {
    /// 把插件的产出变成可直接渲染的差异；拿不到就返回 `None`（退回纯文本）。
    ///
    /// 两条来源，优先级从高到低：
    /// 1. **结构化 body**（process 轨）：字段就是 `duck_plugin::diff::DiffDoc`；
    /// 2. **unified diff 文本**（JS 轨的唯一路径，也是 process 轨没发 body 时的兜底）。
    ///
    /// 解析失败**不报错**：差异渲染是「锦上添花」，为它把一次成功的运行判失败
    /// 是本末倒置——文字结果照样在，用户复制得到东西。
    fn decode(body: Option<&serde_json::Value>, text: &str) -> Option<Self> {
        let doc = match body {
            Some(value) => serde_json::from_value::<duck_plugin::DiffDoc>(value.clone()).ok(),
            None => None,
        };
        let doc = doc.or_else(|| duck_plugin::DiffDoc::from_unified(text))?;
        if doc.is_empty() {
            return None;
        }
        let stats = doc.stats();
        let sections: Vec<DiffSection> = doc
            .files
            .iter()
            .map(|file| DiffSection {
                title_a: file.title_a.clone(),
                title_b: file.title_b.clone(),
                lines: file.rows.iter().map(diff_line).collect(),
            })
            .collect();
        Some(Self {
            sections: std::rc::Rc::new(sections),
            summary: DiffSummary {
                added: stats.added,
                removed: stats.removed,
                modified: stats.modified,
            },
        })
    }
}

/// 一条 wire 行 → 渲染行。
fn diff_line(row: &duck_plugin::DiffRow) -> DiffLine {
    let segments = |spans: Vec<duck_plugin::DiffSpan>| -> Vec<DiffSegment> {
        spans
            .into_iter()
            .map(|span| DiffSegment {
                hit: span.hit,
                text: span.text,
            })
            .collect()
    };
    DiffLine {
        kind: match row.kind {
            duck_plugin::DiffRowKind::Equal => DiffLineKind::Equal,
            duck_plugin::DiffRowKind::Delete => DiffLineKind::Delete,
            duck_plugin::DiffRowKind::Insert => DiffLineKind::Insert,
            duck_plugin::DiffRowKind::Change => DiffLineKind::Change,
            duck_plugin::DiffRowKind::Gap => DiffLineKind::Gap,
        },
        old_no: row.old_ln,
        new_no: row.new_ln,
        old: segments(row.old_segments()),
        new: segments(row.new_segments()),
    }
}

/// 长任务进度（process 轨 `progress` 事件的最近一帧）。
struct ProgressInfo {
    current: u64,
    /// `None` = 不定总量（转圈，不画百分比）。
    total: Option<u64>,
    /// 阶段说明或最近一行日志。
    message: Option<String>,
}

impl ProgressInfo {
    /// 完成比例 0..=1（不定总量时恒 0）。
    fn ratio(&self) -> f32 {
        match self.total {
            Some(total) if total > 0 => (self.current as f32 / total as f32).clamp(0., 1.),
            _ => 0.,
        }
    }
}

/// runner 线程 → 前台 UI 的单向搬运队列。
///
/// `ProgressSink` 的回调在 runner 自己的线程里跑，碰不到 `cx`；这里只入队，
/// 前台每 [`PROGRESS_POLL`] 拉一次。用 `Mutex<Vec<_>>` 而不是 channel：读端
/// 永不阻塞（channel 的 `recv_timeout` 会在前台任务里睡着，拖动窗口就顿了）。
#[derive(Clone, Default)]
struct ProgressQueue(Arc<Mutex<Vec<ProgressEvent>>>);

/// 一条待搬运的进度事件。
enum ProgressEvent {
    Log {
        line: String,
        level: Option<String>,
    },
    Progress {
        current: u64,
        total: Option<u64>,
        message: Option<String>,
    },
}

impl ProgressQueue {
    /// 取走当前所有事件（不阻塞）。
    fn drain(&self) -> Vec<ProgressEvent> {
        self.0
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }
}

impl duck_plugin_process::ProgressSink for ProgressQueue {
    fn on_log(&mut self, line: &str, level: Option<&str>) {
        if let Ok(mut queue) = self.0.lock() {
            queue.push(ProgressEvent::Log {
                line: line.to_owned(),
                level: level.map(str::to_owned),
            });
        }
    }

    fn on_progress(&mut self, current: u64, total: Option<u64>, message: Option<&str>) {
        if let Ok(mut queue) = self.0.lock() {
            queue.push(ProgressEvent::Progress {
                current,
                total,
                message: message.map(str::to_owned),
            });
        }
    }
}

/// 插件工具的通用执行页视图。
pub(crate) struct ToolRunView {
    ctx: PluginRunContext,
    io: IoSpec,
    fields: Vec<FieldControl>,
    params: Vec<ParamControl>,
    /// 执行中（沙箱 10 秒上限内不重复派发）。
    running: bool,
    error: Option<SharedString>,
    output: Option<RunOutput>,
    /// 长任务最近一帧进度（process 轨才有）。
    progress: Option<ProgressInfo>,
    /// 长任务页的统计卡（插件结束 body 里的 stats；通用页不渲染）。
    stats: Option<JobStats>,
    /// 页内扫描进行中（`[io] scan` 声明的「重新扫描」按钮转圈）。
    scanning: bool,
    /// 过程日志（process 轨的 `log` 事件，环形保留最近 [`LOG_MAX_LINES`] 行）。
    ///
    /// 用 `Arc<Vec<_>>`：`uniform_list` 的闭包要求 `'static`，每帧都要把行数据
    /// 搬进去；`Arc` 只是引用计数加一，不复制几千个字符串。
    logs: Arc<Vec<LogLine>>,
    /// 日志列表的滚动句柄：运行中每来新行滚到底（用户手动上翻时不抢）。
    log_scroll: UniformListScrollHandle,
    /// 差异视图的滚动句柄（每次运行换新：上一次的滚动位置对新结果没有意义）。
    diff_scroll: UniformListScrollHandle,
    /// 本次运行的取消令牌（process 轨才有；无它就不显示「取消」）。
    cancel: Option<duck_plugin_process::CancelHandle>,
    /// 本次运行是被用户取消的（收尾文案与审计用，不当失败）。
    cancelled: bool,
    /// 本次运行要不要把表格编辑并进 `params.__edits`。
    ///
    /// 只有点「应用改动」那一次置真，派发后立刻清掉：否则「改了几格没提交，再去
    /// 改参数点运行」会把编辑悄悄带上——那是一次用户没要求的写入。
    include_edits: bool,
    /// 工作台左列表（`layout = "workbench"`）：扫描回填的 (value, label) 对。
    workbench_items: Vec<(String, String)>,
    /// 左列表当前点中的条目（只是高亮；回填的权威值在目标参数控件里）。
    workbench_selected: Option<String>,
    /// 工作台首帧自动扫描是否已触发过（防渲染循环里反复派发）。
    workbench_autoscanned: bool,
}

/// 插件文案取词：插件表 → 宿主表 → 源串。
fn ptext(cx: &App, lang: Lang, plugin_id: &str, source: &str) -> String {
    crate::plugins::plugin_text_owned(cx, lang, plugin_id, source).to_owned()
}

impl ToolRunView {
    /// 建一份执行页：按 `[io]` 声明逐项建控件，参数取清单缺省值。
    pub(crate) fn new(ctx: PluginRunContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let lang = active_theme(cx).lang();
        let plugin_id = ctx.plugin_id.clone();
        let io = ctx.io.clone();

        // 输入字段（沉底区）：按声明顺序建。
        let mut fields = Vec::with_capacity(io.fields.len());
        for field in &io.fields {
            let placeholder = SharedString::from(ptext(cx, lang, &plugin_id, &field.placeholder));
            let control = match field.kind {
                FieldKind::Text => FieldControl::Text(
                    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)),
                ),
                FieldKind::Textarea => FieldControl::Textarea(cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .placeholder(placeholder)
                        .auto_grow(TEXTAREA_MIN_ROWS, TEXTAREA_MAX_ROWS)
                })),
            };
            fields.push(control);
        }

        // 参数（置顶区）：数值参数预填缺省值——多数场景用户直接点运行。
        let mut params = Vec::with_capacity(io.params.len());
        for param in &io.params {
            let placeholder = SharedString::from(ptext(cx, lang, &plugin_id, &param.placeholder));
            let default = SharedString::from(param.default.clone());
            let control = match param.kind {
                ParamKind::Text => ParamControl::Text(
                    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)),
                ),
                ParamKind::Path => ParamControl::Path(
                    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)),
                ),
                ParamKind::Number => ParamControl::Number(cx.new(|cx| {
                    let state = InputState::new(window, cx).placeholder(placeholder);
                    if default.is_empty() {
                        state
                    } else {
                        state.default_value(default)
                    }
                })),
                ParamKind::Switch => ParamControl::Switch(param.default == "true"),
                // `pills` 与 `select` 的存储与提交完全一致，只是渲染成按钮组。
                ParamKind::Select | ParamKind::Pills => ParamControl::Select(param.default.clone()),
                ParamKind::Display => ParamControl::Display(String::new()),
                ParamKind::Range => ParamControl::Range(
                    cx.new(|cx| {
                        let state = InputState::new(window, cx).placeholder(placeholder);
                        if default.is_empty() {
                            state
                        } else {
                            state.default_value(default)
                        }
                    }),
                    cx.new(|cx| InputState::new(window, cx)),
                ),
            };
            params.push(control);
        }

        Self {
            ctx,
            io,
            fields,
            params,
            running: false,
            error: None,
            output: None,
            progress: None,
            stats: None,
            scanning: false,
            logs: Arc::new(Vec::new()),
            log_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            cancel: None,
            cancelled: false,
            include_edits: false,
            workbench_items: Vec::new(),
            workbench_selected: None,
            workbench_autoscanned: false,
        }
    }

    // ── 取值与校验（都只读 `self`；错误以 `Option`/`Err` 形式交回调用方写状态）──

    /// 主输入：字段值按声明顺序拼接（多个字段以换行分隔）。
    fn input_text(&self, cx: &App) -> String {
        self.fields
            .iter()
            .map(|control| match control {
                FieldControl::Text(state) => state.read(cx).value().to_string(),
                FieldControl::Textarea(state) => state.read(cx).value().to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 必填校验：输入字段（声明的 `required`）。`Some(文案)` = 未通过。
    fn validate_required(&self, cx: &App) -> Option<SharedString> {
        for (spec, control) in self.io.fields.iter().zip(&self.fields) {
            if !spec.required {
                continue;
            }
            let empty = match control {
                FieldControl::Text(state) => state.read(cx).value().trim().is_empty(),
                FieldControl::Textarea(state) => state.read(cx).value().trim().is_empty(),
            };
            if empty {
                return Some(SharedString::from(tr(cx, "请先填写必填项。")));
            }
        }
        None
    }

    /// 参数在当前选择下是否可见（`visible_when` 全部命中）。
    ///
    /// 隐藏的参数**同时**不参与取值——否则插件会收到「用户从没见过的旋钮」
    /// 的默认值，而条件之所以存在，就是因为那个值在别的模式下没有意义。
    fn param_visible(&self, index: usize, _cx: &App) -> bool {
        let Some(spec) = self.io.params.get(index) else {
            return false;
        };
        let Some(conditions) = spec.visible_when.as_ref() else {
            return true;
        };
        conditions.iter().all(|(key, expected)| {
            self.io
                .params
                .iter()
                .zip(&self.params)
                .find(|(other, _)| &other.id == key)
                .is_some_and(|(_, control)| match control {
                    ParamControl::Select(value) => value == expected,
                    ParamControl::Switch(on) => on.to_string() == *expected,
                    _ => false,
                })
        })
    }

    /// 参数区序列化成 JSON（`duck.params` 的注入值）。
    ///
    /// 数值项在这里做「能解析」校验：把类型错误拦在派发前，插件侧只收合法串。
    /// `Err(文案)` = 校验失败，不派发。
    fn params_json(&self, cx: &App) -> Result<String, SharedString> {
        let lang = active_theme(cx).lang();
        let plugin_id = self.ctx.plugin_id.clone();
        let required_error = || SharedString::from(tr(cx, "请先填写必填项。"));
        let number_error =
            |label: &str| SharedString::from(trf(cx, "「{}」需要是一个数字。", &[label]));

        let mut map = serde_json::Map::new();
        for (index, (spec, control)) in self.io.params.iter().zip(&self.params).enumerate() {
            if !self.param_visible(index, cx) {
                continue;
            }
            match control {
                ParamControl::Text(state) | ParamControl::Path(state) => {
                    let value = state.read(cx).value().trim().to_owned();
                    if !value.is_empty() {
                        map.insert(spec.id.clone(), serde_json::Value::String(value));
                    } else if !spec.default.is_empty() {
                        map.insert(
                            spec.id.clone(),
                            serde_json::Value::String(spec.default.clone()),
                        );
                    } else if spec.required {
                        return Err(required_error());
                    }
                }
                ParamControl::Number(state) => {
                    let raw = state.read(cx).value().trim().to_owned();
                    let raw = if raw.is_empty() {
                        spec.default.clone()
                    } else {
                        raw
                    };
                    if raw.is_empty() {
                        if spec.required {
                            return Err(required_error());
                        }
                        continue;
                    }
                    if raw.parse::<f64>().is_err() {
                        return Err(number_error(&ptext(cx, lang, &plugin_id, &spec.label)));
                    }
                    map.insert(spec.id.clone(), serde_json::Value::String(raw));
                }
                ParamControl::Range(low, high) => {
                    for (key, state) in
                        [(spec.id.clone(), low), (format!("{}__max", spec.id), high)]
                    {
                        let raw = state.read(cx).value().trim().to_owned();
                        let raw = if raw.is_empty() {
                            spec.default.clone()
                        } else {
                            raw
                        };
                        if raw.is_empty() {
                            if spec.required {
                                return Err(required_error());
                            }
                            continue;
                        }
                        if raw.parse::<f64>().is_err() {
                            return Err(number_error(&ptext(cx, lang, &plugin_id, &spec.label)));
                        }
                        map.insert(key, serde_json::Value::String(raw));
                    }
                }
                ParamControl::Switch(on) => {
                    map.insert(spec.id.clone(), serde_json::Value::String(on.to_string()));
                }
                ParamControl::Select(value) => {
                    map.insert(spec.id.clone(), serde_json::Value::String(value.clone()));
                }
                // 展示参数是只读的：不进运行请求（插件消费不到它）。
                ParamControl::Display(_) => {}
            }
        }

        // 输入字段**另按 id 放一份**。
        //
        // `input` 是「所有字段按顺序用换行拼起来」的单串——单个 textarea 的工具
        // 够用，但两个字段（A/B 两段文本比对）就分不清哪行属于哪边了。所以这里
        // 再按 `fields[i].id` 各放一份，插件按 id 取即可；不知道这回事的老插件
        // 照旧读 `input`，多出来的键不影响它们。
        for (spec, control) in self.io.fields.iter().zip(&self.fields) {
            let value = match control {
                FieldControl::Text(state) => state.read(cx).value().to_string(),
                FieldControl::Textarea(state) => state.read(cx).value().to_string(),
            };
            map.insert(spec.id.clone(), serde_json::Value::String(value));
        }

        // 表格编辑：只在点「应用改动」那一次带上（见 `include_edits`）。
        //
        // 键用 `__edits` 是因为它不可能是清单里的参数 id（那些是标识符），
        // 语义是**前置修改**——插件先落库，再照常执行原本的动作。
        if self.include_edits {
            let edits = self
                .output
                .as_ref()
                .and_then(|output| output.editor.as_ref())
                .map(|editor| editor.read(cx).edits(cx))
                .unwrap_or_default();
            if !edits.is_empty() {
                match serde_json::to_value(&edits) {
                    Ok(value) => {
                        map.insert(duck_plugin::EDIT_PARAM.to_owned(), value);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to serialize table edits");
                    }
                }
            }
        }

        serde_json::to_string(&serde_json::Value::Object(map)).map_err(|error| {
            tracing::warn!(%error, "failed to serialize plugin params");
            SharedString::from(tr(cx, "参数序列化失败。"))
        })
    }

    /// 本次运行授权的用户文件：`path` 参数里真填了值的那些路径。
    ///
    /// 与 `params` 里的同名字段重复是**故意**的：`params` 回答「这个参数是什么」，
    /// `files` 回答「本次运行被授权碰哪些路径」（ADR-3：per-run，不落盘、不记忆）。
    /// 没要文件权限的插件一律给空——免得插件以为自己拿到了授权。
    fn path_files(&self, cx: &App) -> Vec<String> {
        if self.io.allow_files == FileAccess::None {
            return Vec::new();
        }
        self.io
            .params
            .iter()
            .zip(&self.params)
            .enumerate()
            .filter(|(index, _)| self.param_visible(*index, cx))
            .filter_map(|(_, (_, control))| match control {
                ParamControl::Path(state) => {
                    let value = state.read(cx).value().trim().to_owned();
                    (!value.is_empty()).then_some(value)
                }
                _ => None,
            })
            .collect()
    }

    // ── 执行 ─────────────────────────────────────────────────────────────

    /// 表格编辑控件要 `window` 才建得出来，而结果是后台回调里落的（那里没有
    /// 窗口）⇒ 在第一次渲染时补建。只建一次：`editor` 有值就返回。
    fn ensure_table_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spec) = self.io.output.as_ref() else {
            return;
        };
        if spec.cells.is_empty() {
            return;
        }
        // 先 clone 再借用 output：两个字段都在 `self` 上，同时借会打架。
        let cells = spec.cells.clone();
        let Some(table) = self.output.as_ref().and_then(|output| output.table.clone()) else {
            return;
        };
        if self
            .output
            .as_ref()
            .is_some_and(|output| output.editor.is_some())
        {
            return;
        }
        // ⚠️ 没有行标识 ⇒ 整表只读。
        //
        // 这不是保守：编辑的价值在于「改完之后它还在」，而重跑后行序一变，
        // 「第 3 行」就换了人。插件不给 `row_ids` 就等于说「这张表不打算被编辑」
        // （比如 Excel 解析结果——它没有可落库的去处）。
        if table.row_ids.iter().all(String::is_empty) {
            return;
        }
        let editor = cx.new(|cx| TableEditor::new(table, &cells, window, cx));
        if let Some(output) = self.output.as_mut() {
            output.editor = Some(editor);
        }
    }

    /// 「应用改动」：把表格里的脏改动并进 `params.__edits` 再跑一次。
    fn apply_table_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 到这里才读所有格子的值：渲染期去算「脏没脏」要每帧读上千个 `InputState`，
        // 而本页的既有口径是「只在提交时读一次输入」（参数区也一样）。
        let dirty = self
            .output
            .as_ref()
            .and_then(|output| output.editor.as_ref())
            .map(|editor| editor.read(cx).edits(cx).len())
            .unwrap_or(0);
        if dirty == 0 {
            duck_components::notify::info(tr(cx, "表格没有改动。"), window, cx);
            return;
        }
        self.include_edits = true;
        self.run(cx);
    }

    /// 「应用表格改动」按钮：有可编辑表时出现。
    ///
    /// 不做「有脏改动才显示」——那要求每帧比对每个格子。常驻的代价是一次误点
    /// （点击时提示「表格没有改动」），换的是滚动时不掉帧。
    fn render_apply_edits(&self, id_prefix: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.output
            .as_ref()
            .and_then(|output| output.editor.as_ref())?;
        let theme = active_theme(cx);
        let icon = theme.extended().icon.sm;
        Some(
            h_flex()
                .w_full()
                .flex_none()
                .justify_end()
                .child(
                    icon_text_button(
                        SharedString::from(format!("{id_prefix}-apply-edits")),
                        tr(cx, "应用表格改动"),
                        Icon::new(DuckIcon::Check).size(icon),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.apply_table_edits(window, cx)),
                    ),
                )
                .into_any_element(),
        )
    }

    /// 收集 → 校验 → 后台沙箱执行 → 结果回填。
    fn run(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let input = self.input_text(cx);
        let params = match self.validate_required(cx) {
            Some(error) => Err(error),
            None => self.params_json(cx),
        };
        let params = match params {
            Ok(params) => params,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };

        self.error = None;
        self.running = true;
        self.cancelled = false;
        self.progress = None;
        self.stats = None;
        // 编辑只随「应用改动」那一次提交：`params_json` 已经读过它了，这里立刻
        // 清掉——否则下一次普通运行还会把同一批编辑再写一遍。
        self.include_edits = false;
        // 每次运行换一扇干净的日志窗口（上一次的日志与本次无关，留着会误读）。
        self.logs = Arc::new(Vec::new());
        self.log_scroll = UniformListScrollHandle::new();
        // process 轨才有进度与取消（JS 轨仍是 10 秒沙箱，跑完即回）。
        let queue = ProgressQueue::default();
        let cancel = duck_plugin_process::CancelHandle::new();
        let mut queue_for_worker = queue.clone();
        let cancel_for_worker = cancel.clone();
        let process_track = matches!(self.ctx.manifest.execution_model, ExecutionModel::Process);
        self.cancel = process_track.then(|| cancel.clone());
        cx.notify();

        // 派发前把执行上下文摊平成 `'static` 的一份（后台闭包读不到 `cx`）。
        let dir = self.ctx.dir.clone();
        let data_dir = self.ctx.data_dir.clone();
        let manifest = self.ctx.manifest.clone();
        let granted = self.ctx.granted.clone();
        let tool_id = self.ctx.tool_id.clone();
        let lang = active_theme(cx).lang().tag().to_owned();
        // 两条轨的失败文案在派发前译好：后台闭包里没有 `cx`，`tr` 拿不到语言。
        let missing_bin = tr(
            cx,
            "插件二进制缺失：请先运行 sbin/plugins.sh 构建 process 轨插件。",
        )
        .to_owned();
        let unsupported = tr(cx, "该插件的执行模型尚未接入。").to_owned();
        let model = manifest.execution_model;
        let bin = self.ctx.bin.clone();
        // process 轨的请求体：`[io]` 参数原样带过去（已是一段 JSON），
        // 授权路径单独列一份（插件据此知道「这次能碰哪些文件」）。
        let params_value: serde_json::Value =
            serde_json::from_str(&params).unwrap_or(serde_json::Value::Null);
        let files = self.path_files(cx);
        let execution = cx.background_executor().spawn(async move {
            match model {
                ExecutionModel::Js => duck_plugin_js::run_tool(
                    &dir, &data_dir, &manifest, &granted, &tool_id, &input, &params, &lang,
                )
                .map(|outcome| Executed::text_only(outcome.output)),
                ExecutionModel::Process => {
                    let Some(bin) = bin else {
                        return Err(PluginError::Execution(missing_bin));
                    };
                    if !bin.is_file() {
                        return Err(PluginError::Execution(missing_bin));
                    }
                    let request = duck_plugin_process::RunRequest {
                        tool: tool_id,
                        input,
                        params: params_value,
                        files,
                        lang,
                        kv: std::collections::BTreeMap::new(),
                        data: data_dir.to_string_lossy().into_owned(),
                        action: None,
                    };
                    duck_plugin_process::run(
                        &dir,
                        &bin,
                        &request,
                        &mut queue_for_worker,
                        &cancel_for_worker,
                    )
                    .map(|outcome| Executed {
                        output: outcome.output,
                        body: outcome.body,
                    })
                }
                ExecutionModel::Wasm => Err(PluginError::Execution(unsupported)),
            }
        });

        // 进度泵：runner 线程只往队列里塞，这里每 [`PROGRESS_POLL`] 拉一次刷 UI。
        // 循环条件是 `running`，收尾时再 drain 一次，避免最后几行掉在窗口外。
        if process_track {
            let queue = queue.clone();
            cx.spawn(async move |this, cx| {
                loop {
                    let pending = queue.drain();
                    let alive = this
                        .update(cx, |this, cx| {
                            if this.absorb_progress(pending) {
                                cx.notify();
                            }
                            this.running
                        })
                        .unwrap_or(false);
                    if !alive {
                        break;
                    }
                    cx.background_executor().timer(PROGRESS_POLL).await;
                }
                let tail = queue.drain();
                let _ = this.update(cx, |this, cx| {
                    if this.absorb_progress(tail) {
                        cx.notify();
                    }
                });
            })
            .detach();
        }

        let display_name = self.ctx.display_name;
        let plugin_id = self.ctx.plugin_id.clone();
        let kind = self
            .io
            .output
            .as_ref()
            .map_or(OutputKind::Text, |spec| spec.kind);
        // 表格解不出来时的退路（`code` / `log` / `text`）：一个工具既有表格输出
        // 又有代码输出时，靠它保住那半边原来的画法。
        let kind_fallback = self
            .io
            .output
            .as_ref()
            .and_then(|spec| spec.fallback)
            .unwrap_or(OutputKind::Text);
        let label = self
            .io
            .output
            .as_ref()
            .map_or(String::new(), |spec| spec.label.clone());
        let run_tool_id = self.ctx.tool_id.clone();
        let cancel_for_ui = cancel.clone();

        cx.spawn(async move |this, cx| {
            let outcome = execution.await;
            let cancelled = cancel_for_ui.is_cancelled();
            let _ = this.update(cx, move |this, cx| {
                this.running = false;
                this.cancel = None;
                // 用户主动取消不是失败：runner 给的是内核英文错（cancelled by user），
                // 直接铺到界面上既刺眼又看不懂，这里换成中性回执。
                if cancelled {
                    this.cancelled = true;
                    this.error = None;
                    crate::update::on_notify_window(cx, |_, cx| {
                        crate::audit::record_event(
                            cx,
                            crate::audit::AuditKind::Plugin,
                            crate::audit::action::INVOKE,
                            crate::audit::AuditOutcome::Cancelled,
                            &plugin_id,
                            trf(cx, "取消插件工具：{}", &[display_name]),
                        );
                    });
                    cx.notify();
                    return;
                }
                // 审计与提示在「最该看到它的窗口」上投递（与旧 toast 路径同口径）。
                crate::update::on_notify_window(cx, |window, cx| match &outcome {
                    Ok(result) => {
                        let detail = trf(cx, "调用插件工具：{}", &[display_name]);
                        crate::audit::record_event(
                            cx,
                            crate::audit::AuditKind::Plugin,
                            crate::audit::action::INVOKE,
                            crate::audit::AuditOutcome::Ok,
                            &plugin_id,
                            detail,
                        );
                        crate::plugins::emit_text(display_name, &result.output, window, cx);
                    }
                    Err(error) => {
                        tracing::warn!(tool = %display_name, %error, "plugin tool execution failed");
                        let detail = trf(cx, "插件执行失败：{}", &[&error.to_string()]);
                        crate::audit::record_event(
                            cx,
                            crate::audit::AuditKind::Plugin,
                            crate::audit::action::INVOKE,
                            crate::audit::AuditOutcome::Failed,
                            &plugin_id,
                            detail,
                        );
                        duck_components::notify::error(
                            trf(cx, "插件执行失败：{}", &[&error.to_string()]),
                            window,
                            cx,
                        );
                    }
                });
                match &outcome {
                    Ok(result) => {
                        ToolRegistry::record_use(cx, &run_tool_id);
                        // 差异只对声明了 `kind = "diff"` 的工具解：别的形状解出来也是白解。
                        let diff = if kind == OutputKind::Diff {
                            DiffPayload::decode(result.body.as_ref(), &result.output)
                        } else {
                            None
                        };
                        // 表格同理：body 优先，拿不到就试 Markdown 表（部分工具本来
                        // 就在拼 Markdown，零改动即可变成真表格）。
                        let columns = this
                            .io
                            .output
                            .as_ref()
                            .map(|output| output.columns.clone())
                            .unwrap_or_default();
                        let table = if kind == OutputKind::Table {
                            TableDoc::decode(result.body.as_ref(), &columns)
                                .or_else(|| TableDoc::from_markdown(&result.output))
                                .filter(|table| !table.is_empty())
                        } else {
                            None
                        };
                        // 预览同理：body 里给路径才算数，解不出退回 fallback。
                        let preview = if kind == OutputKind::Preview {
                            decode_preview(result.body.as_ref())
                        } else {
                            None
                        };
                        // 统计卡只在插件真的发了 stats body 时出现；没发就退回纯文本。
                        this.stats = JobStats::decode(result.body.as_ref());
                        // 声明了 table / preview 但这次没解出对应的载荷（代码 /
                        // 报文骨架 / 变更清单 / 扩展名不认识）
                        // ⇒ 按清单给的退路渲染。不退的话代码就丢了等宽字体。
                        let kind = if (kind == OutputKind::Table && table.is_none())
                            || (kind == OutputKind::Preview && preview.is_none())
                        {
                            kind_fallback
                        } else {
                            kind
                        };
                        this.output = Some(RunOutput {
                            text: result.output.clone(),
                            kind,
                            label: label.clone(),
                            diff,
                            table,
                            // 编辑控件要 `window` 才建得出来，而这里是后台回来的
                            // 回调（没有窗口）⇒ 留到下一次 `render` 里懒建。
                            editor: None,
                            preview,
                        });
                        // 上一次的滚动位置不适用于新结果。
                        this.diff_scroll = UniformListScrollHandle::new();
                        this.error = None;
                    }
                    Err(error) => {
                        this.error = Some(SharedString::from(error.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 把搬运队列里的事件写进进度状态；返回是否有变化（决定要不要 `notify`）。
    fn absorb_progress(&mut self, events: Vec<ProgressEvent>) -> bool {
        let mut changed = false;
        for event in events {
            match event {
                ProgressEvent::Progress {
                    current,
                    total,
                    message,
                } => {
                    self.progress = Some(ProgressInfo {
                        current,
                        total,
                        message,
                    });
                    changed = true;
                }
                // 日志进日志窗（原型 D 右侧那条滚动日志），不再只留最后一行当阶段说明：
                // 出错复核靠的正是被覆盖掉的那些中间行。
                ProgressEvent::Log { line, level } => {
                    self.push_log(line, LogLevel::parse(level.as_deref()));
                    changed = true;
                }
            }
        }
        changed
    }

    /// 追加一行过程日志（环形保留最近 [`LOG_MAX_LINES`] 行）。
    fn push_log(&mut self, text: String, level: LogLevel) {
        if self.logs.len() >= LOG_MAX_LINES {
            Arc::make_mut(&mut self.logs).drain(..LOG_TRIM_LINES);
        }
        Arc::make_mut(&mut self.logs).push(LogLine {
            time: now_hhmmss(),
            level,
            text,
        });
        // 只在运行中自动跟随：跑完之后用户上翻看历史，不该被拽回底部。
        if self.running {
            self.scroll_logs_to_bottom();
        }
    }

    /// 把日志列表滚到最后一行（`Bottom` 策略 = 让它贴底可见）。
    fn scroll_logs_to_bottom(&self) {
        if let Some(last) = self.logs.len().checked_sub(1) {
            self.log_scroll.scroll_to_item(last, ScrollStrategy::Bottom);
        }
    }

    /// 清空日志窗（用户嫌挡视线时用；下一次运行也会自动换新）。
    fn clear_logs(&mut self, cx: &mut Context<Self>) {
        self.logs = Arc::new(Vec::new());
        cx.notify();
    }

    /// 取消长任务：置位令牌，runner 先让插件体面收尾，超时才 `SIGTERM`/`SIGKILL`。
    ///
    /// 只置位、不立刻改 `running`：真正的收尾要等进程真的退出，
    /// 否则按钮会先变回「运行」而进程还在跑。
    fn cancel_run(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
            cx.notify();
        }
    }

    /// 重置：参数回缺省值、清掉输入/结果/日志（参考项目的「重置」）。
    ///
    /// 只回**声明里的缺省值**，不做「上次运行的值」记忆——那需要落 KV，
    /// 而 ADR-4 的快照通道还没有工具在用；用户点重置的意图就是「回到刚打开时」。
    fn reset_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (index, spec) in self.io.params.iter().enumerate() {
            let default = spec.default.clone();
            let Some(control) = self.params.get_mut(index) else {
                continue;
            };
            match control {
                ParamControl::Text(state) | ParamControl::Path(state) => {
                    state.update(cx, |state, cx| state.set_value(default, window, cx));
                }
                ParamControl::Number(state) => {
                    state.update(cx, |state, cx| state.set_value(default, window, cx));
                }
                ParamControl::Range(low, high) => {
                    low.update(cx, |state, cx| state.set_value(default, window, cx));
                    // 区间的高位没有缺省值（`range` 的 `default` 只描述下界）。
                    high.update(cx, |state, cx| state.set_value(String::new(), window, cx));
                }
                ParamControl::Switch(on) => *on = spec.default == "true",
                ParamControl::Select(value) => *value = default,
                // 展示参数的值来自扫描，重置即清空（下一次扫描再回填）。
                ParamControl::Display(value) => *value = String::new(),
            }
        }
        for index in 0..self.fields.len() {
            self.set_field_value(index, String::new(), window, cx);
        }
        self.output = None;
        self.error = None;
        self.progress = None;
        self.stats = None;
        self.cancelled = false;
        self.logs = Arc::new(Vec::new());
        cx.notify();
    }

    /// 页内扫描（`[io] scan` 声明才有的「重新扫描」）：以 `action = "scan"` 再拉
    /// 一次插件二进制，把返回的版本列表按声明序回填只读展示参数
    /// （第 1 个 ← 逗号连接的完整列表，第 2 个 ← 「首 → 末」区间）。
    ///
    /// 与 [`Self::run`] 同一条 process 通道，但没有进度泵：扫描是秒级操作，
    /// 按钮转个圈（`scanning`）就够了。
    fn scan(&mut self, cx: &mut Context<Self>) {
        if self.scanning || self.running {
            return;
        }
        let Some(scan) = self.io.scan.clone() else {
            return;
        };
        let src = self
            .io
            .params
            .iter()
            .position(|param| param.id == scan.src)
            .and_then(|index| self.params.get(index))
            .map(|control| match control {
                ParamControl::Path(state) => state.read(cx).value().trim().to_owned(),
                _ => String::new(),
            })
            .unwrap_or_default();
        // 工作台的扫描喂的是左列表，`scan.src` 可以是可选目录（空 = 只列内置）；
        // 长任务页的扫描没有目录就无从扫起，仍然拦下。
        if src.is_empty() && self.io.layout != IoLayout::Workbench {
            self.error = Some(SharedString::from(tr(cx, "请先选择要扫描的目录。")));
            cx.notify();
            return;
        }
        self.error = None;
        self.scanning = true;
        cx.notify();

        // 派发前摊平成 `'static` 的一份（后台闭包读不到 `cx`）。
        let dir = self.ctx.dir.clone();
        let bin = self.ctx.bin.clone();
        let tool_id = self.ctx.tool_id.clone();
        let lang = active_theme(cx).lang().tag().to_owned();
        let missing_bin = tr(
            cx,
            "插件二进制缺失：请先运行 sbin/plugins.sh 构建 process 轨插件。",
        )
        .to_owned();
        let src_key = scan.src.clone();
        let files = if !src.is_empty() && self.io.allow_files != FileAccess::None {
            vec![src.clone()]
        } else {
            Vec::new()
        };
        let cancel = duck_plugin_process::CancelHandle::new();
        let execution = cx.background_executor().spawn(async move {
            let Some(bin) = bin else {
                return Err(PluginError::Execution(missing_bin));
            };
            if !bin.is_file() {
                return Err(PluginError::Execution(missing_bin));
            }
            let request = duck_plugin_process::RunRequest {
                tool: tool_id,
                input: String::new(),
                params: serde_json::json!({ src_key.clone(): src }),
                files,
                lang,
                kv: std::collections::BTreeMap::new(),
                // 扫描只认目录、只回一串版本名，**不碰插件自己的库**：
                // 给它数据目录等于放开一条「扫描时顺手改状态」的路，而 scan
                // 的契约是只读的。要读库的动作走正常运行那条路径。
                data: String::new(),
                action: Some("scan".into()),
            };
            duck_plugin_process::run(
                &dir,
                &bin,
                &request,
                &mut duck_plugin_process::NullSink,
                &cancel,
            )
        });

        cx.spawn(async move |this, cx| {
            let outcome = execution.await;
            let _ = this.update(cx, |this, cx| {
                this.scanning = false;
                match outcome {
                    Ok(result) => {
                        let filled = this.fill_displays_from_scan(result.body.as_ref());
                        // 工作台布局：同一份结果喂左列表（`list` 键）。
                        let listed = this.fill_workbench_list(result.body.as_ref());
                        if !filled && !listed {
                            this.push_log(
                                tr(cx, "扫描完成：目录里没有找到可合并的包。").to_owned(),
                                LogLevel::Warn,
                            );
                        }
                    }
                    Err(error) => {
                        this.push_log(error.to_string(), LogLevel::Error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 扫描结果回填：按声明序填 `display` 参数。返回是否填到了东西。
    fn fill_displays_from_scan(&mut self, body: Option<&serde_json::Value>) -> bool {
        let Some(names) = body
            .and_then(|body| body.get("versions"))
            .and_then(serde_json::Value::as_array)
        else {
            return false;
        };
        let names: Vec<String> = names
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        if names.is_empty() {
            return false;
        }
        let mut display_ordinal = 0usize;
        for (index, spec) in self.io.params.iter().enumerate() {
            if spec.kind != ParamKind::Display {
                continue;
            }
            let value = match display_ordinal {
                0 => names.join(", "),
                1 => format!("{} → {}", names[0], names[names.len() - 1]),
                _ => break,
            };
            if let Some(ParamControl::Display(slot)) = self.params.get_mut(index) {
                *slot = value;
            }
            display_ordinal += 1;
        }
        true
    }

    /// 工作台左列表回填：解析 scan body 的 `list` 键（字符串数组，或
    /// `{value, label}` 对象数组）。返回是否拿到了条目。
    fn fill_workbench_list(&mut self, body: Option<&serde_json::Value>) -> bool {
        let Some(items) = body
            .and_then(|body| body.get("list"))
            .and_then(|value| value.as_array())
        else {
            return false;
        };
        let parsed: Vec<(String, String)> = items
            .iter()
            .filter_map(|item| match item {
                serde_json::Value::String(text) => Some((text.clone(), text.clone())),
                serde_json::Value::Object(map) => {
                    let value = map.get("value")?.as_str()?.to_owned();
                    let label = map
                        .get("label")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(&value)
                        .to_owned();
                    Some((value, label))
                }
                _ => None,
            })
            .collect();
        if parsed.is_empty() {
            return false;
        }
        // 已选项不在新列表里就摘掉高亮（值还在参数控件里，不影响运行）。
        if let Some(selected) = &self.workbench_selected {
            if !parsed.iter().any(|(value, _)| value == selected) {
                self.workbench_selected = None;
            }
        }
        self.workbench_items = parsed;
        true
    }

    /// 点中工作台列表项：回填 `[io.workbench]` target 参数并更新高亮。
    fn pick_workbench_item(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workbench) = self.io.workbench.clone() else {
            return;
        };
        let Some(index) = self
            .io
            .params
            .iter()
            .position(|param| param.id == workbench.target)
        else {
            return;
        };
        match self.params.get_mut(index) {
            // select / pills：存选中值，与「用户在下拉里点了一项」完全同义。
            Some(ParamControl::Select(slot)) => *slot = value.clone(),
            // text：写回输入框（`set_value` 期间不派发 Change，见 gpui-base）。
            Some(ParamControl::Text(state) | ParamControl::Path(state)) => {
                state.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
            }
            _ => return,
        }
        self.workbench_selected = Some(value);
        cx.notify();
    }

    /// 工作台首帧自动扫描：列表是导航不是结果，空着就先扫一次。
    ///
    /// 只在渲染入口触发一次（`workbench_autoscanned` 钉住）：渲染里改状态 +
    /// `notify` 会再进来一帧，但没有第二次派发，不会成环。
    fn autoscan_workbench(&mut self, cx: &mut Context<Self>) {
        if self.workbench_autoscanned
            || self.scanning
            || self.running
            || self.io.workbench.is_none()
            || self.io.scan.is_none()
        {
            return;
        }
        self.workbench_autoscanned = true;
        self.scan(cx);
    }

    /// 工作台左列表（`[io.workbench]` 声明了才有）：固定宽、滚动、
    /// 悬停强调色薄底 / 选中 `nav_selected`。空态一句话，不当错误。
    fn render_workbench_list(&self, id_prefix: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let list_id = SharedString::from(format!("{id_prefix}-workbench-list"));
        let selected = self.workbench_selected.clone();
        let items = self.workbench_items.clone();
        let empty = items.is_empty();
        v_flex()
            .w(px(240.))
            .flex_none()
            .min_h_0()
            .gap(tokens.spacing.xxs)
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .child(pane_label(SharedString::from(tr(cx, "列表")), cx))
                    .child(
                        icon_text_button(
                            SharedString::from(format!("{id_prefix}-workbench-refresh")),
                            tr(cx, "刷新"),
                            Icon::new(DuckIcon::RotateCw).size(extended.icon.sm),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .disabled(self.scanning)
                        .on_click(cx.listener(|this, _, _, cx| this.scan(cx))),
                    ),
            )
            .child(
                div()
                    .id(list_id)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .rounded(tokens.radius.md)
                    .border(extended.stroke.thin)
                    .border_color(tokens.colors.border)
                    .bg(tokens.colors.surface)
                    .p(tokens.spacing.xs)
                    .when(empty, |block| {
                        block.child(
                            div()
                                .p(tokens.spacing.sm)
                                .child(meta_text(tr(cx, "暂无条目。选好目录后点「刷新」。"), cx)),
                        )
                    })
                    .children(
                        items
                            .into_iter()
                            .enumerate()
                            .map(move |(index, (value, label))| {
                                let is_selected = selected.as_ref() == Some(&value);
                                let value_for_click = value.clone();
                                div()
                                    .id(SharedString::from(format!("{id_prefix}-wb-{index}")))
                                    .w_full()
                                    .px(tokens.spacing.sm)
                                    .py(tokens.spacing.xxs)
                                    .rounded(tokens.radius.sm)
                                    .truncate()
                                    .role(Role::ListItem)
                                    .aria_label(SharedString::from(label.clone()))
                                    .when(is_selected, |row| row.bg(extended.colors.nav_selected))
                                    .when(!is_selected, |row| {
                                        row.hover(|style| style.bg(extended.colors.hover_tint))
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.pick_workbench_item(
                                            value_for_click.clone(),
                                            window,
                                            cx,
                                        )
                                    }))
                                    .child(meta_text(SharedString::from(label), cx))
                                    .into_any_element()
                            }),
                    ),
            )
            .into_any_element()
    }

    /// 复制结果到剪贴板（结果区尾随按钮）。
    fn copy_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(output) = &self.output else {
            return;
        };
        if let Err(reason) = duck_platform::write_text(&output.text) {
            tracing::warn!(%reason, "failed to copy plugin output");
            return;
        }
        notify::success(tr(cx, "复制到剪贴板"), window, cx);
    }

    /// 导出：表格给 CSV、其余给 Markdown（面板里预填扩展名）。
    ///
    /// 内容在开面板**之前**固化：用户可能在面板上停很久，回来时结果早换了。
    fn export_output(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(output) = &self.output else {
            return;
        };
        let (content, extension) = match &output.table {
            Some(table) => (table.to_csv(), "csv"),
            None => (output.text.clone(), "md"),
        };
        let name = format!("{}.{extension}", self.export_stem());
        crate::file_dialogs::save_file_in_window(cx, &name, move |path, window, _, cx| {
            match std::fs::write(&path, content.as_bytes()) {
                Ok(()) => notify::success(
                    trf(cx, "已导出到 {}", &[&path.display().to_string()]),
                    window,
                    cx,
                ),
                Err(error) => {
                    notify::error(trf(cx, "导出失败：{}", &[&error.to_string()]), window, cx)
                }
            }
        });
    }

    /// 另存为（原样文本；表格存 CSV 同 [`Self::export_output`]）。
    fn save_output(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(output) = &self.output else {
            return;
        };
        let content = output.text.clone();
        let name = format!("{}.txt", self.export_stem());
        crate::file_dialogs::save_file_in_window(cx, &name, move |path, window, _, cx| {
            match std::fs::write(&path, content.as_bytes()) {
                Ok(()) => notify::success(
                    trf(cx, "已保存到 {}", &[&path.display().to_string()]),
                    window,
                    cx,
                ),
                Err(error) => {
                    notify::error(trf(cx, "保存失败：{}", &[&error.to_string()]), window, cx)
                }
            }
        });
    }

    /// 导出文件名的词干：工具 id 的最后一段（`duck.text.timestamp` → `timestamp`）。
    fn export_stem(&self) -> String {
        self.ctx
            .tool_id
            .rsplit('.')
            .next()
            .unwrap_or("result")
            .to_owned()
    }

    /// 粘贴：把剪贴板文本写进第一个输入字段。
    fn paste_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = duck_platform::read_text() else {
            return;
        };
        self.set_field_value(0, text, window, cx);
        cx.notify();
    }

    /// 清空：全部输入字段置空。
    fn clear_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for index in 0..self.fields.len() {
            self.set_field_value(index, String::new(), window, cx);
        }
        cx.notify();
    }

    /// 一个输入字段的当前文本。
    fn field_value(&self, index: usize, cx: &App) -> String {
        match self.fields.get(index) {
            Some(FieldControl::Text(state)) => state.read(cx).value().to_string(),
            Some(FieldControl::Textarea(state)) => state.read(cx).value().to_string(),
            None => String::new(),
        }
    }

    /// 交换两个输入字段（`[io] swap_fields`）。
    ///
    /// 单向转换试反了方向时，用户的直觉是「换过来再跑一次」；让他手工剪切粘贴
    /// 两段几百行的报文，就是把一次点击变成最容易出错的操作。
    fn swap_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.io.fields.len() != 2 {
            return;
        }
        let first = self.field_value(0, cx);
        let second = self.field_value(1, cx);
        self.set_field_value(0, second, window, cx);
        self.set_field_value(1, first, window, cx);
        cx.notify();
    }

    /// 写一个输入字段的值（`index` 与 `io.fields` 对应）。
    fn set_field_value(
        &mut self,
        index: usize,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.fields.get(index) {
            Some(FieldControl::Text(state)) => {
                state.update(cx, |state, cx| state.set_value(text, window, cx));
            }
            Some(FieldControl::Textarea(state)) => {
                state.update(cx, |state, cx| state.set_value(text, window, cx));
            }
            None => {}
        }
    }
}

/// 本机时区的「时:分:秒」（日志行打点用）。
///
/// 参考项目在事件缺时间戳时也用 `toLocaleTimeString` 兜底；我们统一在接收侧
/// 打点（运行中的日志是流式到达的，接收时刻就是发生时刻）。
fn now_hhmmss() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0);
    let local = now + i64::from(duck_platform::local_utc_offset_seconds(now));
    let secs_of_day = local.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// 参数标签：10px 三级文字色（对齐参考项目 `text-2xs` + muted 的参数标签档）。
fn param_label(text: SharedString, cx: &App) -> gpui::Div {
    let theme = active_theme(cx);
    let badge = theme.extended().badge;
    div()
        .flex_none()
        .text_size(badge.size)
        .line_height(badge.line_height)
        .text_color(theme.extended().colors.text_tertiary)
        .child(text)
}

/// 栏目标签：12px 三级文字色（参考项目 `text-xs` + muted 的「输入 / 输出」抬头，
/// 不画分隔线）。
fn pane_label(text: SharedString, cx: &App) -> gpui::Div {
    let theme = active_theme(cx);
    let xs = theme.tokens().typography.xs;
    div()
        .flex_none()
        .text_size(xs.size)
        .line_height(xs.line_height)
        .text_color(theme.extended().colors.text_tertiary)
        .child(text)
}

/// 一个输入字段的编辑器（通用页与并排双栏共用）。
///
/// 多行框自带边框与内边距（`appearance(false)` 去掉上游的双层描边），单行框
/// 直接用 `Input` 的默认外观——两者的圆角与边框色因此来自同一套 token。
fn render_field_editor(
    plugin_id: &str,
    index: usize,
    spec: &FieldSpec,
    control: &FieldControl,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens().clone();
    let extended = theme.extended().clone();
    let lang = theme.lang();
    let label = SharedString::from(ptext(cx, lang, plugin_id, &spec.label));
    match control {
        FieldControl::Text(state) => div()
            .flex_none()
            .w_full()
            .child(Input::new(state).w_full().aria_label(label))
            .into_any_element(),
        FieldControl::Textarea(state) => div()
            .w_full()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .rounded(theme.components().input_radius)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.surface)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .child(
                Textarea::new(state)
                    .appearance(false)
                    .aria_label(SharedString::from(format!("{label}-{index}")))
                    .w_full()
                    .text_size(tokens.typography.sm.size)
                    .line_height(tokens.typography.sm.line_height),
            )
            .into_any_element(),
    }
}

/// 参数控件渲染：每类参数一个控件，回调经 `WeakEntity` 回写状态。
fn render_param_control(
    view: &gpui::WeakEntity<ToolRunView>,
    tool_id: &str,
    plugin_id: &str,
    index: usize,
    spec: &ParamSpec,
    control: &ParamControl,
    cx: &mut Context<ToolRunView>,
) -> AnyElement {
    let lang = active_theme(cx).lang();
    let id = SharedString::from(format!("tool-run-param-{tool_id}-{index}"));

    match control {
        ParamControl::Text(state) | ParamControl::Number(state) => {
            Input::new(state).w_full().into_any_element()
        }
        // 路径 = 输入框 + 「选择…」。手填永远可用（用户可能从日志里复制路径），
        // 面板只是省事：`pick` 决定选文件还是选目录。
        //
        // 面板必须走 `file_dialogs`（gpui 的异步回调面板）：`rfd` 的 `runModal`
        // 会重入 AppKit 事件循环并 abort，界面里一律禁用（见 `file_dialogs` 模块注释）。
        ParamControl::Path(state) => {
            let target = state.clone();
            let prompt = ptext(cx, lang, plugin_id, &spec.label);
            let pick = spec.pick;
            h_flex()
                .w_full()
                .min_w_0()
                .gap(tokens_gap(cx))
                .child(div().flex_1().min_w_0().child(Input::new(state).w_full()))
                // `flex_none`：窄到极限时只压输入框，按钮永不缩水/被裁。
                // 参考项目这个按钮是「图标 + 文字 + 底色」的次级样式（`bg-overlay`），
                // Ghost 无底色，看起来像一段被裁掉的裸文字。
                .child(
                    div().flex_none().child(
                        icon_text_button(
                            id,
                            tr(cx, "浏览"),
                            Icon::new(DuckIcon::FolderOpen)
                                .size(active_theme(cx).extended().icon.sm),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(move |_, window, cx| {
                            let target = target.clone();
                            // 用户可能在面板上停留很久：回调里只写这一个输入框，
                            // 不再回读页面其它状态。
                            let fill = move |path: std::path::PathBuf,
                                             window: &mut Window,
                                             cx: &mut App| {
                                let text = path.to_string_lossy().to_string();
                                target.update(cx, |state, cx| {
                                    state.set_value(text, window, cx);
                                });
                            };
                            match pick {
                                PathPick::Folder => crate::file_dialogs::pick_folder_in_app(
                                    window, cx, &prompt, fill,
                                ),
                                PathPick::File => {
                                    crate::file_dialogs::open_file_in_app(window, cx, &prompt, fill)
                                }
                            }
                        }),
                    ),
                )
                .into_any_element()
        }
        ParamControl::Switch(on) => {
            let view = view.clone();
            switch_control(id, *on, move |value, _, cx| {
                if let Ok(()) = view.update(cx, |this, cx| {
                    if let Some(ParamControl::Switch(slot)) = this.params.get_mut(index) {
                        *slot = *value;
                    }
                    cx.notify();
                }) {}
            })
            .into_any_element()
        }
        ParamControl::Select(current) => {
            let options: Vec<(SharedString, SharedString)> = spec
                .options
                .iter()
                .map(|option| {
                    (
                        SharedString::from(option.value.clone()),
                        SharedString::from(ptext(cx, lang, plugin_id, &option.label)),
                    )
                })
                .collect();
            // 候选多时换成可搜索下拉：随机文本生成的「国家/地区」有 25 项、
            // SQL 质量检查的规则组有十几项，纯菜单要滚好几屏才能找到。
            if options.len() > SEARCHABLE_SELECT_THRESHOLD {
                let view = view.clone();
                dropdown_select_searchable(
                    id,
                    options,
                    current.clone(),
                    tr(cx, "搜索…"),
                    move |value, _, cx| {
                        if let Ok(()) = view.update(cx, |this, cx| {
                            if let Some(ParamControl::Select(slot)) = this.params.get_mut(index) {
                                *slot = value.to_string();
                            }
                            cx.notify();
                        }) {}
                    },
                    cx,
                )
                .into_any_element()
            } else {
                let view = view.clone();
                dropdown_select(
                    id,
                    options,
                    current.clone(),
                    move |value, _, cx| {
                        if let Ok(()) = view.update(cx, |this, cx| {
                            if let Some(ParamControl::Select(slot)) = this.params.get_mut(index) {
                                *slot = value.to_string();
                            }
                            cx.notify();
                        }) {}
                    },
                    cx,
                )
                .into_any_element()
            }
        }
        // 区间：两个等宽数字框。宽度由外层给定，子项一律 `flex_1`（`w_full` 会撑破）。
        ParamControl::Range(low, high) => h_flex()
            .w_full()
            .min_w_0()
            .gap(tokens_gap(cx))
            .child(div().flex_1().min_w_0().child(Input::new(low).w_full()))
            .child(div().flex_1().min_w_0().child(Input::new(high).w_full()))
            .into_any_element(),
        // 只读展示（通用执行页的兜底形态；长任务页走 `render_display_box` 同款）。
        ParamControl::Display(value) => render_display_box(value, cx).into_any_element(),
    }
}

/// 行内参数单元（参考原型 B 的「label + 窄控件」）：标签 10px 弱者色紧跟控件，
/// 带固定窄宽（文本 160 / 数字 88 / 区间 200），下拉与开关保持自然宽；
/// 路径参数**独占一行**（`w_full`）。
///
/// ⚠️ 路径不能与别的参数挤同一行：两个 path 并排时「flex_1 + min_w(280)」在折行
/// 判定里都算放得下（280 × 2 < 容器宽），实际排版却因按钮/标签的最小内容宽度
/// 溢出容器，右端的「选择…」被裁掉（目录比对页实证）。参考原型 B 里路径本来就
/// 各占一行（包目录 / 结果目录上下排），整行布局既忠实也不依赖 flex 折行。
fn render_param_item(
    view: &gpui::WeakEntity<ToolRunView>,
    tool_id: &str,
    plugin_id: &str,
    index: usize,
    spec: &ParamSpec,
    control: &ParamControl,
    cx: &mut Context<ToolRunView>,
) -> AnyElement {
    let lang = active_theme(cx).lang();
    let gap = active_theme(cx).tokens().spacing.sm;
    let label = SharedString::from(ptext(cx, lang, plugin_id, &spec.label));
    // 路径是绝对路径，给固定窄宽会把路径切得只剩结尾：让它整行铺开、输入框吃
    // 剩余宽度，「选择…」按钮永远完整可见。展示框同理（整行放版本清单）。
    let flexible = matches!(control, ParamControl::Path(_) | ParamControl::Display(_));
    let width = match control {
        ParamControl::Text(_) => Some(px(160.)),
        ParamControl::Number(_) => Some(px(88.)),
        ParamControl::Path(_) | ParamControl::Display(_) => None,
        ParamControl::Range(..) => Some(px(200.)),
        ParamControl::Switch(_) | ParamControl::Select(_) => None,
    };
    let control = render_param_control(view, tool_id, plugin_id, index, spec, control, cx);
    let control = match width {
        Some(width) => div().w(width).min_w_0().child(control).into_any_element(),
        None if flexible => div().flex_1().min_w_0().child(control).into_any_element(),
        None => control,
    };
    let mut item = h_flex()
        .items_center()
        .gap(gap)
        .child(param_label(label, cx))
        .child(control);
    item = if flexible {
        // `w_full`：在 flex_wrap 容器里占满一整行，天然独占一行（不靠折行判定）。
        item.w_full().min_w_0()
    } else {
        item.flex_none()
    };
    item.into_any_element()
}

/// 区间两个输入框之间的间距（控件内紧密档）。
fn tokens_gap(cx: &App) -> gpui::Pixels {
    active_theme(cx).tokens().spacing.xs + active_theme(cx).tokens().spacing.xxs
}

/// 只读展示框：`display` 参数的形态（扫描后由宿主回填）。
fn render_display_box(value: &str, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens().clone();
    let extended = theme.extended().clone();
    let mut holder = div()
        .w_full()
        .min_w_0()
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.surface)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs);
    if value.is_empty() {
        holder = holder.child(meta_text(tr(cx, "扫描后回填。"), cx));
    } else {
        holder = holder.child(
            div()
                .w_full()
                .min_w_0()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(selectable_text(SharedString::from(value.to_owned()))),
        );
    }
    holder.into_any_element()
}

/// 药丸按钮组（`ParamKind::Pills`）：候选直接摊开，选中项强调色实底。
///
/// 只在长任务页渲染；通用执行页把 `pills` 当 `select` 画成下拉（提交值一致）。
#[allow(clippy::too_many_arguments)]
fn render_pill_group(
    view: &gpui::WeakEntity<ToolRunView>,
    id_prefix: &str,
    plugin_id: &str,
    index: usize,
    spec: &ParamSpec,
    current: &str,
    cx: &mut Context<ToolRunView>,
) -> AnyElement {
    let lang = active_theme(cx).lang();
    let tokens = active_theme(cx).tokens().clone();
    let extended = active_theme(cx).extended().clone();
    let height = active_theme(cx).density().control;
    let mut row = h_flex()
        .w_full()
        .min_w_0()
        .flex_wrap()
        .gap(tokens.spacing.sm);
    for option in &spec.options {
        let label = SharedString::from(ptext(cx, lang, plugin_id, &option.label));
        let selected = option.value == current;
        let id = SharedString::from(format!("{id_prefix}-pill-{index}-{}", option.value));
        let value = option.value.clone();
        let view = view.clone();
        row = row.child(
            div()
                .id(id)
                .flex_none()
                .h(height)
                .px(tokens.spacing.lg)
                .flex()
                .items_center()
                .rounded(tokens.radius.md)
                .text_size(tokens.typography.sm.size)
                .role(Role::Button)
                .aria_label(label.clone())
                .when(selected, |pill| {
                    pill.bg(tokens.colors.accent)
                        .text_color(tokens.colors.accent_foreground)
                })
                .when(!selected, |pill| {
                    pill.bg(tokens.colors.muted)
                        .text_color(tokens.colors.muted_foreground)
                        .hover(|style| style.bg(extended.colors.hover_tint))
                })
                .on_click(move |_, _, cx| {
                    if let Ok(()) = view.update(cx, |this, cx| {
                        if let Some(ParamControl::Select(slot)) = this.params.get_mut(index) {
                            *slot = value.clone();
                        }
                        cx.notify();
                    }) {}
                })
                .child(label),
        );
    }
    row.into_any_element()
}

/// 长任务页的一个参数块：开关 = 控件在左标签在右（原版复选框行形态）；
/// 其余 = 标签在上（必填加红星）、控件整行铺开。首个展示参数的标签行右端
/// 挂「重新扫描」（原版把按钮放在待合并列表旁）。
#[allow(clippy::too_many_arguments)]
fn job_param_row(
    view: &gpui::WeakEntity<ToolRunView>,
    id_prefix: &str,
    tool_id: &str,
    plugin_id: &str,
    index: usize,
    spec: &ParamSpec,
    control: &ParamControl,
    scan_here: bool,
    scanning: bool,
    cx: &mut Context<ToolRunView>,
) -> AnyElement {
    let lang = active_theme(cx).lang();
    let tokens = active_theme(cx).tokens().clone();
    let extended = active_theme(cx).extended().clone();
    let label = SharedString::from(ptext(cx, lang, plugin_id, &spec.label));

    if matches!(control, ParamControl::Switch(_)) {
        return h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(render_param_control(
                view, tool_id, plugin_id, index, spec, control, cx,
            ))
            .child(
                div()
                    .min_w_0()
                    .text_size(tokens.typography.sm.size)
                    .line_height(tokens.typography.sm.line_height)
                    .text_color(tokens.colors.muted_foreground)
                    .child(label),
            )
            .into_any_element();
    }

    let mut head = h_flex()
        .w_full()
        .min_w_0()
        .items_center()
        .gap(tokens.spacing.xxs);
    if spec.required {
        head = head.child(
            div()
                .flex_none()
                .text_size(tokens.typography.sm.size)
                .text_color(extended.colors.status_failed)
                .child("*"),
        );
    }
    head = head.child(param_label(label, cx));
    if scan_here {
        let scan_id = SharedString::from(format!("{id_prefix}-scan"));
        head = head.child(div().flex_1()).child(
            icon_text_button(
                scan_id,
                tr(cx, "重新扫描"),
                Icon::new(DuckIcon::RotateCw).size(extended.icon.sm),
                ButtonVariant::Ghost,
                cx,
            )
            .disabled(scanning)
            .on_click(cx.listener(|this, _, _, cx| this.scan(cx))),
        );
    }

    let body = if matches!(spec.kind, ParamKind::Pills) {
        match control {
            ParamControl::Select(current) => {
                render_pill_group(view, id_prefix, plugin_id, index, spec, current, cx)
            }
            _ => render_param_control(view, tool_id, plugin_id, index, spec, control, cx),
        }
    } else {
        render_param_control(view, tool_id, plugin_id, index, spec, control, cx)
    };

    v_flex()
        .w_full()
        .min_w_0()
        .gap(tokens.spacing.xxs)
        .child(head)
        .child(body)
        .into_any_element()
}

/// 页面顶部页签参数（`style = "tabs"`）的下标。
///
/// 一个工具最多一个（装载期已校验）：它表示「当前模式」，其余参数的显隐靠
/// `visible_when` 挂在它身上。
fn tab_param_index(io: &IoSpec) -> Option<usize> {
    io.params
        .iter()
        .position(|spec| spec.style == ParamStyle::Tabs)
}

/// 当前页签：`(参数下标, 选中值)`；没有 `style = "tabs"` 参数时 `None`。
///
/// 收成一个 `Option` 是因为 clippy 要求合并两层 `if let`，而 `let` 链要 1.88
/// （MSRV 1.85）⇒ 用 `?` + `match`，两处调用点都不用再嵌套。
fn current_tab(io: &IoSpec, controls: &[ParamControl]) -> Option<(usize, String)> {
    let index = tab_param_index(io)?;
    match controls.get(index) {
        Some(ParamControl::Select(current)) => Some((index, current.clone())),
        _ => None,
    }
}

/// 模式页签：页面顶部那一条分段控件（参考项目 `AppSegmented`）。
///
/// 选项多时横向滚动而不是折行——折成两排之后「当前在哪一档」反而更难扫。
#[allow(clippy::too_many_arguments)]
fn render_mode_tabs(
    view: &gpui::WeakEntity<ToolRunView>,
    id_prefix: &str,
    plugin_id: &str,
    index: usize,
    spec: &ParamSpec,
    current: &str,
    cx: &mut Context<ToolRunView>,
) -> AnyElement {
    let lang = active_theme(cx).lang();
    let labels: Vec<SharedString> = spec
        .options
        .iter()
        .map(|option| SharedString::from(ptext(cx, lang, plugin_id, &option.label)))
        .collect();
    let selected = spec
        .options
        .iter()
        .position(|option| option.value == current)
        .unwrap_or(0);
    let values: Vec<String> = spec
        .options
        .iter()
        .map(|option| option.value.clone())
        .collect();
    let view = view.clone();
    let id = SharedString::from(format!("{id_prefix}-tabs"));
    let tabs = duck_components::segmented_tabs(
        id,
        labels,
        selected,
        move |picked, _, cx| {
            let Some(value) = values.get(picked).cloned() else {
                return;
            };
            if let Ok(()) = view.update(cx, |this, cx| {
                if let Some(ParamControl::Select(slot)) = this.params.get_mut(index) {
                    *slot = value;
                }
                cx.notify();
            }) {}
        },
        cx,
    );
    div()
        .id(SharedString::from(format!("{id_prefix}-tabs")))
        .w_full()
        .flex_none()
        .overflow_x_scroll()
        .child(tabs)
        .into_any_element()
}

/// 长任务页的统计卡（插件 `body` 的 stats；没有 body 时该位置放纯文本结果框）。
fn render_stats_card(stats: &JobStats, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens().clone();
    let extended = theme.extended().clone();
    let sm = tokens.typography.sm;
    let mut card = v_flex()
        .w_full()
        .min_w_0()
        .rounded(tokens.radius.md)
        .border(extended.stroke.thin)
        .border_color(tokens.colors.border)
        .bg(tokens.colors.surface)
        .px(tokens.spacing.md)
        .py(tokens.spacing.sm)
        .gap(tokens.spacing.xs)
        .child(caption_heading(tr(cx, "合并统计"), cx));
    let mut grid = div()
        .w_full()
        .flex()
        .flex_wrap()
        .gap_x(tokens.spacing.lg)
        .gap_y(tokens.spacing.xxs);
    for (label, value) in &stats.entries {
        grid = grid.child(
            h_flex()
                .min_w(relative(0.4))
                .flex()
                .items_center()
                .gap(tokens.spacing.xxs)
                .child(meta_text(SharedString::from(format!("{label}:")), cx))
                .child(
                    div()
                        .min_w_0()
                        .text_size(sm.size)
                        .line_height(sm.line_height)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(value.clone())),
                ),
        );
    }
    card = card.child(grid);
    if !stats.output.is_empty() {
        card = card.child(
            h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap(tokens.spacing.xxs)
                .child(meta_text(tr(cx, "输出"), cx))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(tokens.colors.muted_foreground)
                        .child(SharedString::from(stats.output.clone())),
                ),
        );
    }
    card.into_any_element()
}

/// 长任务页的纯文本结果框（插件没发 stats body 时的兜底）。
fn render_job_result(output: &RunOutput, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens().clone();
    let extended = theme.extended().clone();
    let is_code = output.kind == OutputKind::Code;
    let text = SharedString::from(output.text.clone());
    div()
        .w_full()
        .min_w_0()
        .rounded(tokens.radius.md)
        .border(extended.stroke.thin)
        .border_color(tokens.colors.border)
        .bg(tokens.colors.surface)
        .px(tokens.spacing.md)
        .py(tokens.spacing.sm)
        .when(is_code, |body| {
            body.text_size(tokens.typography.mono_md.size)
                .line_height(tokens.typography.mono_md.line_height)
        })
        .child(selectable_text(text))
        .into_any_element()
}

impl ToolRunView {
    /// 长任务页（`layout = "job"`）：左配置 + 动作 + 进度 + 统计卡，
    /// 右侧固定日志栏。结构与参考页面 `SpMerge.vue` 对齐。
    fn render_job(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let lang = theme.lang();
        let header_height = theme.density().toolbar_button;
        let id_prefix = SharedString::from(format!("tool-run-{}", self.ctx.tool_id));
        let view = cx.entity().downgrade();
        let tool_id = self.ctx.tool_id.clone();
        let plugin_id = self.ctx.plugin_id.clone();

        // ── 左栏：垂直分组配置 ──
        let first_display = self
            .io
            .params
            .iter()
            .position(|spec| spec.kind == ParamKind::Display);
        let has_scan = self.io.scan.is_some();
        let scanning = self.scanning;
        let mut left = v_flex()
            .id(SharedString::from(format!("{id_prefix}-job-left")))
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .gap(tokens.spacing.lg);

        if let Some((index, current)) = current_tab(&self.io, &self.params) {
            left = left.child(render_mode_tabs(
                &view,
                &id_prefix,
                &plugin_id,
                index,
                &self.io.params[index],
                &current,
                cx,
            ));
        }

        let mut current_group = String::new();
        for (index, spec) in self.io.params.iter().enumerate() {
            if !self.param_visible(index, cx) || spec.style == ParamStyle::Tabs {
                continue;
            }
            let Some(control) = self.params.get(index) else {
                continue;
            };
            if spec.group != current_group {
                current_group = spec.group.clone();
                if !spec.group.is_empty() {
                    left = left.child(caption_heading(
                        SharedString::from(ptext(cx, lang, &plugin_id, &spec.group)),
                        cx,
                    ));
                }
            }
            left = left.child(job_param_row(
                &view,
                &id_prefix,
                &tool_id,
                &plugin_id,
                index,
                spec,
                control,
                has_scan && Some(index) == first_display,
                scanning,
                cx,
            ));
        }

        // 输入字段（合并族没有；声明了就照常摆）。
        let multiple_fields = self.io.fields.len() > 1;
        for (index, spec) in self.io.fields.iter().enumerate() {
            if multiple_fields {
                let label = SharedString::from(ptext(cx, lang, &plugin_id, &spec.label));
                left = left.child(param_label(label, cx));
            }
            match self.fields.get(index) {
                Some(FieldControl::Text(state)) => {
                    left = left.child(Input::new(state).w_full());
                }
                Some(FieldControl::Textarea(state)) => {
                    left = left.child(
                        Textarea::new(state)
                            .appearance(false)
                            .aria_label(SharedString::from(ptext(
                                cx,
                                lang,
                                &plugin_id,
                                &spec.label,
                            )))
                            .w_full(),
                    );
                }
                None => {}
            }
        }

        // ── 动作行：主按钮（插件自报文案）+ 重置 + 取消 ──
        let run_label: SharedString = if self.running {
            tr(cx, "执行中…").into()
        } else if self.io.run_label.is_empty() {
            tr(cx, "运行").into()
        } else {
            ptext(cx, lang, &plugin_id, &self.io.run_label).into()
        };
        let mut action_row = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                button(
                    SharedString::from(format!("{id_prefix}-run")),
                    run_label,
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(self.running)
                .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
            )
            .child(
                button(
                    SharedString::from(format!("{id_prefix}-reset")),
                    tr(cx, "重置"),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.running)
                .on_click(cx.listener(|this, _, window, cx| this.reset_all(window, cx))),
            );
        if let Some(error) = self.error.clone() {
            action_row = action_row.child(field_error(error, cx));
        } else if self.cancelled {
            action_row = action_row.child(meta_text(tr(cx, "已取消。"), cx));
        }
        if self.running && self.cancel.is_some() {
            action_row = action_row.child(div().flex_1()).child(
                button(
                    SharedString::from(format!("{id_prefix}-cancel")),
                    tr(cx, "取消"),
                    ButtonVariant::Destructive,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.cancel_run(cx))),
            );
        }
        left = left.child(action_row);

        // ── 进度（长任务才有）──
        if let Some(progress) = &self.progress {
            let ratio = progress.ratio();
            let stage = match &progress.message {
                Some(message) if !message.trim().is_empty() => {
                    SharedString::from(message.trim().to_owned())
                }
                _ => SharedString::from(tr(cx, "进行中…")),
            };
            let mut head = h_flex().w_full().items_center().justify_between().child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(meta_text(stage.clone(), cx)),
            );
            if progress.total.is_some() && !stage.contains('%') {
                head = head.child(meta_text(format!("{:.0}%", ratio * 100.), cx));
            }
            let mut bar = progress_bar(
                SharedString::from(format!("{id_prefix}-progress")),
                ratio,
                cx,
            )
            .w_full();
            if progress.total.is_none() && self.running {
                bar = bar.loading(true);
            }
            left = left.child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .gap(tokens.spacing.xxs)
                    .child(head)
                    .child(bar),
            );
        }

        // ── 统计卡（没有 body 时退回纯文本结果框）──
        if let Some(stats) = self.stats.as_ref() {
            left = left.child(render_stats_card(stats, cx));
        } else if let Some(output) = self.output.as_ref() {
            match output.table.as_ref() {
                // 表格在左栏里要给定高：它自己吃 `flex_1`，而左栏是可滚动的内容流。
                Some(table) => {
                    left = left.child(div().w_full().h(px(260.)).flex().child(render_table(
                        table,
                        output.editor.as_ref(),
                        &id_prefix,
                        cx,
                    )));
                    if let Some(apply) = self.render_apply_edits(&id_prefix, cx) {
                        left = left.child(apply);
                    }
                }
                None => left = left.child(render_job_result(output, cx)),
            }
        }

        // ── 右栏：固定日志（时间戳 + 级别着色 + 空态提示）──
        let logs = self.logs.clone();
        let has_logs = !logs.is_empty();
        let mono = tokens.typography.mono_md;
        let badge = extended.badge;
        let tertiary = extended.colors.text_tertiary;
        // 四档级别色在闭包外算好（Hsla 是 Copy）：uniform_list 的渲染闭包
        // 是 'static 的，抓不到 `cx`。
        let info_color = tokens.colors.foreground;
        let warn_color = extended.colors.warning;
        let error_color = extended.colors.status_failed;
        let success_color = extended.colors.success;
        let log_count = logs.len();
        let mut right = v_flex()
            .w(px(JOB_LOG_WIDTH))
            .flex_none()
            .min_h_0()
            .gap(tokens.spacing.xs)
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .min_h(header_height)
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .flex_none()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .child(pane_label(SharedString::from(tr(cx, "日志")), cx))
                            .child(meta_text(
                                SharedString::from(trf(cx, "{} 行", &[&log_count.to_string()])),
                                cx,
                            )),
                    )
                    .child(
                        icon_text_button(
                            SharedString::from(format!("{id_prefix}-clear-logs")),
                            tr(cx, "清空"),
                            Icon::new(DuckIcon::Eraser).size(extended.icon.sm),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.clear_logs(cx))),
                    ),
            );
        if has_logs {
            right = right.child(
                div()
                    .id(SharedString::from(format!("{id_prefix}-job-logs")))
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .rounded(tokens.radius.md)
                    .border(extended.stroke.thin)
                    .border_color(tokens.colors.border)
                    .bg(tokens.colors.surface)
                    .px(tokens.spacing.md)
                    .py(tokens.spacing.xs)
                    .child(
                        uniform_list(
                            SharedString::from(format!("{id_prefix}-job-log-list")),
                            log_count,
                            move |range, _, _| {
                                range
                                    .map(|index| {
                                        let (time, color, text) = match logs.get(index) {
                                            Some(line) => {
                                                let color = match line.level {
                                                    LogLevel::Info => info_color,
                                                    LogLevel::Warn => warn_color,
                                                    LogLevel::Error => error_color,
                                                    LogLevel::Success => success_color,
                                                };
                                                (line.time.clone(), color, line.text.clone())
                                            }
                                            None => (String::new(), tertiary, String::new()),
                                        };
                                        h_flex()
                                            .w_full()
                                            .h(px(LOG_ROW_HEIGHT))
                                            .flex_none()
                                            .gap(tokens.spacing.sm)
                                            .overflow_hidden()
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .text_size(badge.size)
                                                    .line_height(badge.line_height)
                                                    .text_color(tertiary)
                                                    .child(SharedString::from(time)),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_size(mono.size)
                                                    .line_height(mono.line_height)
                                                    .text_color(color)
                                                    .child(SharedString::from(text)),
                                            )
                                            .into_any_element()
                                    })
                                    .collect()
                            },
                        )
                        .track_scroll(&self.log_scroll),
                    ),
            );
        } else {
            let hint = if self.io.log_hint.is_empty() {
                SharedString::from(tr(cx, "运行后日志会显示在这里。"))
            } else {
                SharedString::from(ptext(cx, lang, &plugin_id, &self.io.log_hint))
            };
            right = right.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(tokens.radius.md)
                    .border_dashed()
                    .border(extended.stroke.thin)
                    .border_color(tokens.colors.border)
                    .child(meta_text(hint, cx)),
            );
        }

        // 页面至少撑满视口；工作区行吃掉剩余高度（与通用页同款骨架）。
        v_flex()
            .w_full()
            .min_h(relative(1.))
            .gap(tokens.spacing.md)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .gap(tokens.spacing.lg)
                    .child(left)
                    .child(right),
            )
            .id(SharedString::from(format!("{id_prefix}-job-page")))
            .into_any_element()
    }
}

impl Render for ToolRunView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 可编辑表格的控件要窗口才建得出来，而结果是从后台回调里落的（那里没有
        // 窗口）⇒ 在渲染入口补建。放在最前面：两条布局分支都要用到它。
        self.ensure_table_editor(window, cx);
        // 工作台左列表：首帧没数据就自动扫一次（只触发一次，见方法注释）。
        if self.io.layout == IoLayout::Workbench {
            self.autoscan_workbench(cx);
        }
        // 长任务页（`layout = "job"`）走自己的布局：左配置 + 进度 + 统计卡，
        // 右侧固定日志栏（参考原版 SpMerge.vue 这类长任务工具页）。
        if self.io.layout == IoLayout::Job {
            return self.render_job(cx);
        }
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let lang = theme.lang();
        // 栏头行高：两栏共用，保证左右面板顶边对齐。
        let header_height = theme.density().toolbar_button;
        let view = cx.entity().downgrade();
        let tool_id = self.ctx.tool_id.clone();
        let plugin_id = self.ctx.plugin_id.clone();
        let id_prefix = SharedString::from(format!("tool-run-{tool_id}"));

        // 页面至少撑满视口（内容更多时照常增长、由外层滚动），
        // 工作区吃掉剩余高度，不留空白。
        let mut page = v_flex().w_full().min_h(relative(1.)).gap(tokens.spacing.md);

        // ── 模式页签（`style = "tabs"`）：页面顶部一条分段控件 ──
        // 多模式工具不该把 9 个模式压成一个下拉：页签让「有哪几种模式」一眼可见，
        // 而当前模式的参数由 `visible_when` 决定，切页签就是换一整块面板。
        if let Some((index, current)) = current_tab(&self.io, &self.params) {
            page = page.child(render_mode_tabs(
                &view,
                &id_prefix,
                &plugin_id,
                index,
                &self.io.params[index],
                &current,
                cx,
            ));
        }

        // ── 参数：行内条（10px 标签紧贴窄控件，宽度不够就折行）──
        // 参考原型 B：参数不套卡片、不各占一行；标签与控件之间只留一个字的间隙。
        // 工作台布局下，target 参数由左列表承载（列表就是它的可视形态），跳过。
        let workbench_target = self.io.workbench.as_ref().map(|spec| spec.target.as_str());
        if !self.io.params.is_empty() {
            let mut strip = div()
                .w_full()
                .flex_none()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(tokens.spacing.md);
            for (index, spec) in self.io.params.iter().enumerate() {
                let Some(control) = self.params.get(index) else {
                    continue;
                };
                // 条件不成立的参数整项不渲染（连标签一起），不留空槽。
                // 模式页签自己已经画在顶部，不再占参数条的位置。
                if !self.param_visible(index, cx)
                    || spec.style == ParamStyle::Tabs
                    || workbench_target == Some(spec.id.as_str())
                {
                    continue;
                }
                strip = strip.child(render_param_item(
                    &view, &tool_id, &plugin_id, index, spec, control, cx,
                ));
            }
            page = page.child(strip);
        }

        // ── 执行行：主按钮紧随参数（内容流里，不做卡片页脚）──
        // 取消按钮贴右（长任务才出现）：它是「逃生门」，不该和主操作挤在一起。
        let run_label = if self.running {
            tr(cx, "执行中…")
        } else {
            tr(cx, "运行")
        };
        let mut action_row = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                button(
                    SharedString::from(format!("{id_prefix}-run")),
                    SharedString::from(run_label),
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(self.running)
                .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
            )
            // 重置紧贴主按钮（次要动作，不跟「取消」抢右端）：参数回缺省 + 清结果/日志。
            .child(
                button(
                    SharedString::from(format!("{id_prefix}-reset")),
                    tr(cx, "重置"),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.running)
                .on_click(cx.listener(|this, _, window, cx| this.reset_all(window, cx))),
            );
        if let Some(error) = self.error.clone() {
            action_row = action_row.child(field_error(error, cx));
        } else if self.cancelled {
            action_row = action_row.child(meta_text(tr(cx, "已取消。"), cx));
        }
        if self.running && self.cancel.is_some() {
            action_row = action_row.child(div().flex_1()).child(
                button(
                    SharedString::from(format!("{id_prefix}-cancel")),
                    tr(cx, "取消"),
                    ButtonVariant::Destructive,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.cancel_run(cx))),
            );
        }
        page = page.child(action_row);

        // ── 进度（长任务才有）：阶段说明 + 百分比 + 进度条 ──
        // `total` 缺省时走不确定态动画（转圈），不画一个永远 0% 的死条。
        if let Some(progress) = &self.progress {
            let ratio = progress.ratio();
            let stage = match &progress.message {
                Some(message) if !message.trim().is_empty() => {
                    SharedString::from(message.trim().to_owned())
                }
                _ => SharedString::from(tr(cx, "进行中…")),
            };
            let mut head = h_flex().w_full().items_center().justify_between().child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(meta_text(stage.clone(), cx)),
            );
            // 有的插件把百分比写进 message（「合并中… 72%」），再在右边补一个就重复了。
            if progress.total.is_some() && !stage.contains('%') {
                head = head.child(meta_text(format!("{:.0}%", ratio * 100.), cx));
            }
            let mut bar = progress_bar(
                SharedString::from(format!("{id_prefix}-progress")),
                ratio,
                cx,
            )
            .w_full();
            if progress.total.is_none() && self.running {
                bar = bar.loading(true);
            }
            page = page.child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .gap(tokens.spacing.xxs)
                    .child(head)
                    .child(bar),
            );
        }

        // ── 工作区（撑满剩余高度）：左输入 / 右结果 ──
        let input_title = match self.io.fields.as_slice() {
            [only] => SharedString::from(ptext(cx, lang, &plugin_id, &only.label)),
            _ => SharedString::from(tr(cx, "输入")),
        };
        // 栏头：12px 弱色标签在左，动作按钮在右；两栏共用同一行高，
        // 面板顶边才会对齐（这是「输入框和结果框没对齐」的根因）。
        let mut left_pane = v_flex().flex_1().min_w_0().min_h_0().gap(tokens.spacing.xs);
        left_pane = left_pane.child(
            h_flex()
                .w_full()
                .flex_none()
                .min_h(header_height)
                .items_center()
                .justify_between()
                .child(pane_label(input_title, cx))
                .child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap(tokens.spacing.xxs)
                        .when(self.io.swap_fields, |row| {
                            row.child(
                                icon_text_button(
                                    SharedString::from(format!("{id_prefix}-swap")),
                                    tr(cx, "交换"),
                                    Icon::new(DuckIcon::ArrowRightLeft).size(extended.icon.sm),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.swap_fields(window, cx)),
                                ),
                            )
                        })
                        .child(
                            icon_text_button(
                                SharedString::from(format!("{id_prefix}-paste")),
                                tr(cx, "粘贴"),
                                Icon::new(DuckIcon::Clipboard).size(extended.icon.sm),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.paste_input(window, cx)),
                            ),
                        )
                        .child(
                            icon_text_button(
                                SharedString::from(format!("{id_prefix}-clear")),
                                tr(cx, "清空"),
                                Icon::new(DuckIcon::Eraser).size(extended.icon.sm),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.clear_input(window, cx)),
                            ),
                        ),
                ),
        );
        let multiple_fields = self.io.fields.len() > 1;
        if self.io.fields_layout == FieldsLayout::Row && self.io.fields.len() == 2 {
            // A/B 并排：左右各一列、各带自己的标签，中间的关系一眼可见。
            let mut columns = h_flex()
                .w_full()
                .flex_1()
                .min_h_0()
                .items_stretch()
                .gap(tokens.spacing.md);
            for (index, spec) in self.io.fields.iter().enumerate() {
                let mut column = v_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .gap(tokens.spacing.xxs);
                let label = SharedString::from(ptext(cx, lang, &plugin_id, &spec.label));
                column = column.child(param_label(label, cx));
                if let Some(control) = self.fields.get(index) {
                    column =
                        column.child(render_field_editor(&plugin_id, index, spec, control, cx));
                }
                columns = columns.child(column);
            }
            left_pane = left_pane.child(columns);
        } else {
            for (index, spec) in self.io.fields.iter().enumerate() {
                if multiple_fields {
                    let label = SharedString::from(ptext(cx, lang, &plugin_id, &spec.label));
                    left_pane = left_pane.child(param_label(label, cx));
                }
                if let Some(control) = self.fields.get(index) {
                    left_pane =
                        left_pane.child(render_field_editor(&plugin_id, index, spec, control, cx));
                }
            }
        }

        // 右栏：结果 + 过程日志（原型 D 的 job 布局）。
        //
        // 三种组合：
        //   · 有输出 + 有日志 → 上下各半（结果可复制、日志供复核）
        //   · 只有输出（普通工具）→ 结果独占
        //   · 只有日志（长任务进行中）→ 日志独占，不摆一块「还没有结果」的空态
        let result_title = match &self.output {
            Some(output) if !output.label.is_empty() => {
                SharedString::from(ptext(cx, lang, &plugin_id, &output.label))
            }
            _ => SharedString::from(tr(cx, "结果")),
        };
        let logs = self.logs.clone();
        let has_logs = !logs.is_empty();
        let mut right_pane = v_flex().flex_1().min_w_0().min_h_0().gap(tokens.spacing.sm);

        if self.output.is_some() || !has_logs {
            let mut result_header = h_flex()
                .w_full()
                .flex_none()
                .min_h(header_height)
                .items_center()
                .justify_between()
                .child(pane_label(result_title, cx));
            // 差异的 `+N −M ~K` 放在标题旁：它是「这份结果值不值得看」的第一判据，
            // 不该埋进面板里当第一行。
            if let Some(summary) = self
                .output
                .as_ref()
                .and_then(|output| output.diff.as_ref().map(|diff| diff.summary))
            {
                result_header = result_header.child(diff_summary(summary, cx));
            }
            if self.output.is_some() {
                // 结果动作：复制 / 导出 / 保存。原版每个结果区都有这三下
                // （`ClassCompare` 导出 CSV、`NginxGenerator` 下载配置……）——
                // 「能看到」与「能带走」是两件事。
                result_header = result_header.child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap(tokens.spacing.xxs)
                        .child(
                            icon_text_button(
                                SharedString::from(format!("{id_prefix}-copy")),
                                tr(cx, "复制"),
                                Icon::new(DuckIcon::Copy).size(extended.icon.sm),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.copy_output(window, cx)),
                            ),
                        )
                        .child(
                            icon_text_button(
                                SharedString::from(format!("{id_prefix}-export")),
                                tr(cx, "导出"),
                                Icon::new(DuckIcon::Download).size(extended.icon.sm),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.export_output(window, cx)),
                            ),
                        )
                        .child(
                            icon_text_button(
                                SharedString::from(format!("{id_prefix}-save")),
                                tr(cx, "保存"),
                                Icon::new(DuckIcon::Save).size(extended.icon.sm),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_output(window, cx)),
                            ),
                        ),
                );
            }
            right_pane = right_pane.child(result_header);
            match &self.output {
                // `kind = "diff"` 且拿到了行数据：A/B 双栏 + 虚拟滚动。
                Some(output) if output.diff.is_some() => {
                    let sections = output
                        .diff
                        .as_ref()
                        .map(|diff| diff.sections.clone())
                        .expect("diff payload was just checked");
                    right_pane =
                        right_pane.child(div().w_full().flex_1().min_h_0().child(diff_view(
                            SharedString::from(format!("{id_prefix}-diff")),
                            sections,
                            self.diff_scroll.clone(),
                            cx,
                        )));
                }
                // `kind = "table"` 且拿到了列/行数据：画真表格（列对齐 + 斑马纹）。
                Some(output) if output.table.is_some() => {
                    let table = output
                        .table
                        .as_ref()
                        .expect("table payload was just checked");
                    right_pane = right_pane.child(render_table(
                        table,
                        output.editor.as_ref(),
                        &id_prefix,
                        cx,
                    ));
                    if let Some(apply) = self.render_apply_edits(&id_prefix, cx) {
                        right_pane = right_pane.child(apply);
                    }
                }
                // `kind = "preview"` 且拿到了可渲染的文件：GPUI 图片管线内联
                // （光栅图 + SVG 全彩，`Contain` 保持比例不裁切）。路径一行小字
                // 放图下：预览只是「看得见」，文件在哪、怎么取用还是要说清楚。
                Some(output) if output.preview.is_some() => {
                    let path = output
                        .preview
                        .clone()
                        .expect("preview payload was just checked");
                    right_pane = right_pane.child(
                        v_flex()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .gap(tokens.spacing.xxs)
                            .child(
                                div()
                                    .id(SharedString::from(format!("{id_prefix}-preview")))
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_hidden()
                                    .rounded(tokens.radius.md)
                                    .border(extended.stroke.thin)
                                    .border_color(tokens.colors.border)
                                    .bg(tokens.colors.surface)
                                    .p(tokens.spacing.sm)
                                    .child(
                                        img(path.clone())
                                            .size_full()
                                            .object_fit(ObjectFit::Contain),
                                    ),
                            )
                            .child(div().px(tokens.spacing.xs).child(meta_text(
                                SharedString::from(path.display().to_string()),
                                cx,
                            ))),
                    );
                }
                Some(output) => {
                    let text = SharedString::from(output.text.clone());
                    let is_code = output.kind == OutputKind::Code;
                    let typography = tokens.typography.clone();
                    right_pane = right_pane.child(
                        div()
                            .id(SharedString::from(format!("{id_prefix}-result")))
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .rounded(tokens.radius.md)
                            .border(extended.stroke.thin)
                            .border_color(tokens.colors.border)
                            .bg(tokens.colors.surface)
                            .px(tokens.spacing.md)
                            .py(tokens.spacing.sm)
                            .child(
                                div()
                                    .when(is_code, |body| {
                                        body.text_size(typography.mono_md.size)
                                            .line_height(typography.mono_md.line_height)
                                    })
                                    .child(selectable_text(text)),
                            ),
                    );
                }
                None => {
                    right_pane = right_pane.child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(tokens.radius.md)
                            .border_dashed()
                            .border(extended.stroke.thin)
                            .border_color(tokens.colors.border)
                            .child(meta_text(tr(cx, "运行后结果会显示在这里。"), cx)),
                    );
                }
            }
        }

        // ── 过程日志窗：等宽、逐行、虚拟滚动（几千行也不掉帧）──
        if has_logs {
            let log_count = logs.len();
            let mono = tokens.typography.mono_md;
            let log_fg = extended.colors.text_tertiary;
            let log_pad = tokens.spacing.md;
            // 有结果时日志只占下 40%（复核用，不该把结果挤成一条）；
            // 进行中（还没有结果）日志独占整栏。
            let share = self.output.is_some();
            right_pane = right_pane.child(
                v_flex()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .when(share, |block| block.max_h(relative(0.4)))
                    .gap(tokens.spacing.xxs)
                    .child(
                        h_flex()
                            .w_full()
                            .flex_none()
                            .min_h(header_height)
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .flex_none()
                                    .items_center()
                                    .gap(tokens.spacing.xs)
                                    .child(pane_label(SharedString::from(tr(cx, "日志")), cx))
                                    .child(meta_text(
                                        SharedString::from(trf(
                                            cx,
                                            "{} 行",
                                            &[&log_count.to_string()],
                                        )),
                                        cx,
                                    )),
                            )
                            .child(
                                icon_text_button(
                                    SharedString::from(format!("{id_prefix}-clear-logs")),
                                    tr(cx, "清空"),
                                    Icon::new(DuckIcon::Eraser).size(extended.icon.sm),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.clear_logs(cx))),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .rounded(tokens.radius.md)
                            .border(extended.stroke.thin)
                            .border_color(tokens.colors.border)
                            .bg(tokens.colors.surface)
                            .px(log_pad)
                            .py(tokens.spacing.xs)
                            .child(
                                uniform_list(
                                    SharedString::from(format!("{id_prefix}-logs")),
                                    log_count,
                                    move |range, _, _| {
                                        range
                                            .map(|index| {
                                                let text = logs
                                                    .get(index)
                                                    .map(|line| line.text.clone())
                                                    .unwrap_or_default();
                                                div()
                                                    .w_full()
                                                    .h(px(LOG_ROW_HEIGHT))
                                                    .flex_none()
                                                    .truncate()
                                                    .text_size(mono.size)
                                                    .line_height(mono.line_height)
                                                    .text_color(log_fg)
                                                    .child(SharedString::from(text))
                                                    .into_any_element()
                                            })
                                            .collect()
                                    },
                                )
                                .track_scroll(&self.log_scroll),
                            ),
                    ),
            );
        }

        // 满高并排行用显式 flex_row（h_flex 交叉轴居中会把栏高度压扁）。
        if !self.io.fields.is_empty() {
            page = page.child(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .gap(tokens.spacing.md)
                    .child(left_pane)
                    .child(right_pane),
            );
        } else {
            page = page.child(right_pane);
        }

        // ── 工作台（原型 F 骨架）：左列表 + 右侧完整执行区 ──
        // 右侧就是通用执行页的全部内容（页签 / 参数 / 运行 / 结果），
        // 左列表来自 scan 的 `list`，点中即回填 target 参数。
        if self.io.layout == IoLayout::Workbench {
            page = div()
                .flex()
                .flex_row()
                .w_full()
                .flex_1()
                .min_h_0()
                .gap(tokens.spacing.md)
                .child(self.render_workbench_list(&id_prefix, cx))
                .child(page);
        }

        page.id(SharedString::from(format!("{id_prefix}-page")))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 结构化 body → 渲染行：行号、种类、行内片段都要按 wire 字段落位。
    #[test]
    fn diff_payload_reads_structured_body() {
        let body = serde_json::json!({
            "files": [{
                "title_a": "旧",
                "title_b": "新",
                "rows": [
                    {"kind": "equal", "old_ln": 1, "new_ln": 1, "old_text": "same", "new_text": "same"},
                    {"kind": "change", "old_ln": 2, "new_ln": 2, "old_text": "a1", "new_text": "a2",
                     "old_spans": [{"hit": false, "text": "a"}, {"hit": true, "text": "1"}],
                     "new_spans": [{"hit": false, "text": "a"}, {"hit": true, "text": "2"}]},
                    {"kind": "insert", "new_ln": 3, "new_text": "added"}
                ]
            }]
        });
        let payload = DiffPayload::decode(Some(&body), "").expect("diff");
        assert_eq!(payload.summary.added, 2);
        assert_eq!(payload.summary.removed, 1);
        assert_eq!(payload.summary.modified, 1);
        let section = &payload.sections[0];
        assert_eq!(section.title_a, "旧");
        assert_eq!(section.lines.len(), 3);
        assert_eq!(section.lines[1].kind, DiffLineKind::Change);
        // 行内片段：命中标记原样带过来（渲染器据此上高亮底色）。
        assert_eq!(
            section.lines[1].new[1],
            DiffSegment {
                hit: true,
                text: "2".into()
            }
        );
        // 只有右侧的行：左侧行号为空。
        assert_eq!(section.lines[2].old_no, None);
        assert_eq!(section.lines[2].new_no, Some(3));
    }

    /// 没有 body 时按 unified diff 兜底（JS 轨插件唯一能走的路径）。
    #[test]
    fn diff_payload_falls_back_to_unified_text() {
        let text = "--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let payload = DiffPayload::decode(None, text).expect("diff");
        assert_eq!(payload.sections[0].lines.len(), 2);
        assert_eq!(payload.summary.added, 1);
        assert_eq!(payload.summary.removed, 1);
    }

    /// 既没 body 也不是 unified diff：返回 `None`，调用方按纯文本渲染（不报错）。
    #[test]
    fn diff_payload_is_none_for_plain_text() {
        assert!(DiffPayload::decode(None, "一行普通输出\n第二行\n").is_none());
        // 空 body 也不能当成「有差异」——否则会画一块空的双栏。
        let empty = serde_json::json!({"files": []});
        assert!(DiffPayload::decode(Some(&empty), "").is_none());
    }
}
