//! 终端智能提示（命令自动补全）的应用层支撑。
//!
//! 分工（与 `duck-term::suggest` 匹配引擎互补）：
//!
//! - **本模块**：历史存储（duck-store 单 kv 键，按 host 隔离）、本地路径候选
//!   （`std::fs` 现查）、建议弹层的几何与外观、以及 L2 的 shell 集成脚本
//!   （bash / zsh 自适应，补发 `OSC 133` 标记）。
//! - **`terminal_canvas.rs`**：把以上接进会话与按键流（建议刷新、导航、接受
//!   回填、注入时机）—— 那些都要摸 `TerminalSession` 的私有状态，留在一起。
//!
//! 铁律（见 `docs/terminal-autocomplete-design.md`）：
//!
//! - 备用屏（vim / top）一律禁弹；
//! - 不接受建议时按键字节序与现状完全一致，零副作用；
//! - 没有把握就宁可漏弹，绝不插错一个字符。

use std::collections::HashMap;
use std::path::Path;

use duck_components::body_text;
use duck_theme::{active_theme, tr};
use gpui::{
    AnyElement, App, IntoElement, ParentElement as _, Pixels, SharedString, div,
    prelude::FluentBuilder as _, px, Styled as _,
};
use gpui_component::v_flex;

use duck_term::{CellSize, SuggestSource, Suggestion};

/// 历史 kv 键：`HashMap<host, Vec<String>>`（新命令在尾部）。
///
/// 按仓约定「结构化配置 = 结构体 → serde_json → 单 kv 键」；历史是**展示层
/// 数据**而不是行为配置，但量级（每 host 上限几百条短串）远够不着我方任何
/// 结构化表 —— 单键最简单，且写入走 `kv_put` 的 400ms 去抖。
pub(crate) const HISTORY_KEY: &str = "terminal.history";
/// 每个 host 保留的命令条数。截断丢**最旧**的（表尾是最近）。
const HISTORY_PER_HOST: usize = 500;
/// 本地会话的历史命名空间。
pub(crate) const LOCAL_HOST: &str = "localhost";
/// 一次路径候选最多列多少项目录项。超出就截断 —— 弹层也只有 8 行。
const PATH_CANDIDATE_CAP: usize = 32;
/// 建议弹层的宽度（逻辑像素）。
const POPUP_WIDTH: f32 = 380.;
/// 弹层与光标格之间的呼吸距离。
const POPUP_GAP: f32 = 2.;

/// 建议弹层的运行时状态（每条会话一份）。
#[derive(Default)]
pub(crate) struct SuggestState {
    /// 当前候选。空 = 弹层收起。
    pub items: Vec<Suggestion>,
    /// 键盘高亮位。
    pub selected: usize,
}

impl SuggestState {
    pub(crate) fn visible(&self) -> bool {
        !self.items.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
        self.selected = 0;
    }
}

/// 读历史（坏 JSON / 缺键一律回落空表，不炸设置读取）。
pub(crate) fn load_history(cx: &App) -> HashMap<String, Vec<String>> {
    duck_store::kv_get(cx, HISTORY_KEY)
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// 写历史（`kv_put` 自带 400ms 去抖；崩溃丢最后几条可接受）。
pub(crate) fn save_history(cx: &App, history: &HashMap<String, Vec<String>>) {
    if let Ok(json) = serde_json::to_string(history) {
        duck_store::kv_put(cx, HISTORY_KEY, json);
    }
}

/// 追加命令到某 host 的历史：先去重（挪到尾部 = 最近），再按上限截头。
pub(crate) fn remember_command(history: &mut HashMap<String, Vec<String>>, host: &str, command: &str) {
    let command = command.trim();
    if command.is_empty() {
        return;
    }
    let list = history.entry(host.to_owned()).or_default();
    list.retain(|seen| seen != command);
    list.push(command.to_owned());
    let excess = list.len().saturating_sub(HISTORY_PER_HOST);
    if excess > 0 {
        list.drain(..excess);
    }
}

/// 会话的历史命名空间：SSH 用目标串（`user@host:port`），本地统一 `localhost`。
pub(crate) fn history_host(target: Option<&duck_ssh::Target>) -> String {
    target.map_or_else(|| LOCAL_HOST.to_owned(), |target| target.destination())
}

/// 输入行里最后一个空白分隔 token（路径补全的当前词）。
///
/// 与引擎的 `last_token_start` 同一条规则：输入以空白结尾时空词（列全目录项）。
pub(crate) fn last_word(input: &str) -> &str {
    let at = input
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(at, ch)| at + ch.len_utf8());
    &input[at..]
}

/// 本地路径候选：当前词的目录项补全。
///
/// `word` 可以带 `/`（`src/ma` → 列 `src/` 下以 `ma` 开头的项，候选带全前缀）。
/// 目录项补 `/`，文件不补；`.` 开头的隐藏项只在词也以 `.` 开头时才列。
/// SSH 会话不在此列 —— 远端列表要走 SFTP 通道，另路（见设计文档 M3）。
pub(crate) fn path_candidates(cwd: &Path, word: &str) -> Vec<String> {
    let Some((dir, partial)) = split_dir(word) else {
        return Vec::new();
    };
    let base = if dir.is_empty() {
        cwd.to_path_buf()
    } else if dir.starts_with('/') {
        std::path::PathBuf::from(&dir)
    } else {
        cwd.join(&dir)
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(partial) || (name.starts_with('.') && !partial.starts_with('.')) {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let mut candidate = format!("{dir}{name}");
        if is_dir {
            candidate.push('/');
        }
        out.push(candidate);
        if out.len() >= PATH_CANDIDATE_CAP {
            break;
        }
    }
    out.sort();
    out
}

/// 把当前词拆成「目录前缀 + 未打完的部分」（均不带尾斜杠；`src/ma` → `src/`, `ma`）。
fn split_dir(word: &str) -> Option<(String, &str)> {
    match word.rfind('/') {
        Some(at) => Some((word[..=at].to_owned(), &word[at + 1..])),
        None => Some((String::new(), word)),
    }
}

/// 建议弹层：绝对定位在光标格下（空间不足翻到上方）。
///
/// 与预编辑覆盖层同一套定位法 —— 包含块是文字层 wrapper，`left/top` 用
/// 「列 × 格宽 / 行 × 行高」，与行、光标共享原点，不做任何补偿。
#[allow(clippy::too_many_arguments)]
pub(crate) fn suggest_popup(
    items: &[Suggestion],
    selected: usize,
    cursor_row: usize,
    cursor_col: usize,
    cell: CellSize,
    line_height: Pixels,
    screen_rows: usize,
    cx: &App,
) -> Option<AnyElement> {
    if items.is_empty() {
        return None;
    }
    let theme = active_theme(cx);
    let count = items.len();
    // 全程 f32：`Pixels` 的混合算术方向在 gpui 里是 `f32 * Pixels -> f32`，
    // 直接混算容易踩反，统一先落回标量。
    let row_h = cell.height;
    let line_h = line_height.to_f64() as f32;
    let popup_height = count as f32 * line_h + 8.;
    // 光标行下面放不下（连 2 行余量都没有）就翻到上面。
    let below = cursor_row + count + 2 < screen_rows;
    let top = if below {
        (cursor_row + 1) as f32 * row_h + POPUP_GAP
    } else {
        ((cursor_row as f32 * row_h) - popup_height).max(POPUP_GAP)
    };
    let rows: Vec<AnyElement> = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let is_selected = index == selected % count;
            let color = match item.source {
                SuggestSource::History => theme.tokens().colors.foreground,
                SuggestSource::Builtin | SuggestSource::Path => {
                    theme.tokens().colors.muted_foreground
                }
            };
            div()
                .h(line_height)
                .flex_none()
                .whitespace_nowrap()
                .overflow_hidden()
                .when(is_selected, |this| {
                    this.bg(theme.extended().colors.drop_target)
                })
                .child(
                    body_text(text_for(item, cx), cx)
                        .text_color(color)
                        .ml(px(6.)),
                )
                .into_any_element()
        })
        .collect();
    Some(
        div()
            .absolute()
            .left(px(cursor_col as f32 * cell.width))
            .top(px(top))
            .w(px(POPUP_WIDTH))
            .py(px(4.))
            .bg(theme.extended().colors.chrome)
            .border_1()
            .border_color(theme.extended().colors.hairline)
            .rounded(px(6.))
            .overflow_hidden()
            .children(rows)
            .into_any_element(),
    )
}

/// 弹层里的展示文本。来源标签是给用户的提示 ——「这条是从哪来的」。
fn text_for(item: &Suggestion, cx: &App) -> SharedString {
    let tag = match item.source {
        SuggestSource::History => tr(cx, "历史"),
        SuggestSource::Builtin => tr(cx, "命令"),
        SuggestSource::Path => tr(cx, "路径"),
    };
    format!("{tag} · {}", item.text).into()
}

/// L2 的 shell 集成片段：给**没装**任何 `OSC 133` 集成的 bash / zsh 补上
/// A/B/C/D 四个标记，让提示符判定与命令采集在裸服务器上也能精确工作。
///
/// 设计约束：
///
/// - **自适应**：`$ZSH_VERSION` / `$BASH_VERSION` 判 shell，其余 shell（fish、
///   csh……）静默跳过 —— 一个字节都不多发；
/// - **幂等**：bash 用 `case` 防重复拼接，zsh 的 `precmd_functions` 追加在
///   重进 shell 前只会发生一次（本片段每次会话只注入一次）；
/// - **零宽不破坏排版**：bash 的标记包在 `\[\]` 里，zsh 的包在 `%{ %}` 里，
///   提示符宽度计算不受影响；
/// - **退出码透传**：`D;$?` 是 `PROMPT_COMMAND` / `precmd` 的第一件事 ——
///   那一刻 `$?` 还是上一条命令的；
/// - 已知代价：注入命令本身会进 shell 历史、会回显一行 —— 换取的是**可选项**
///   的精确补全，设置页里默认关。
pub(crate) const SHELL_INTEGRATION_SCRIPT: &str = concat!(
    "__duck_i(){ printf '\\033]133;%s\\007' \"$1\"; }; ",
    "if [ -n \"$ZSH_VERSION\" ]; then ",
    "__duck_p(){ __duck_i \"D;$?\"; __duck_i A; }; ",
    "__duck_e(){ __duck_i C; }; ",
    "typeset -ga precmd_functions; typeset -ga preexec_functions; ",
    "precmd_functions+=(__duck_p); preexec_functions+=(__duck_e); ",
    "PS1=\"$PS1\"%{$'\\033]133;B\\007'%}; ",
    "elif [ -n \"$BASH_VERSION\" ]; then ",
    "__duck_pc(){ __duck_i \"D;$?\"; __duck_i A; }; ",
    "case \"$PROMPT_COMMAND\" in *__duck_pc*) ;; *) ",
    "PROMPT_COMMAND=\"__duck_pc${PROMPT_COMMAND:+;$PROMPT_COMMAND}\";; esac; ",
    "case \"$PS1\" in *'133;B'*) ;; *) PS1=\"$PS1\\[\\033]133;B\\007\\]\";; esac; ",
    "case \"$PS0\" in *'133;C'*) ;; *) PS0=\"\\[\\033]133;C\\007\\]$PS0\";; esac; ",
    "fi"
);
