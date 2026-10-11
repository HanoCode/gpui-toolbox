//! 状态栏支撑：字节格式化、磁盘剩余空间、数字输入弹窗。
//!
//! 对应 FluxDown `status_bar.rs` 的 `open_number_prompt` / `format_bytes` 与
//! `model::disk` 的剩余空间查询；本工程无下载后端，弹窗只做输入校验，
//! 确认后由调用方决定如何应用。

use std::rc::Rc;

use duck_components::{field_hint, field_label, title_text};
use gpui::{App, AppContext as _, ParentElement as _, Styled as _, Window, px};
use gpui_component::{
    WindowExt as _,
    dialog::DialogButtonProps,
    input::{Input, InputState},
    v_flex,
};

/// 数字输入弹窗的确认回调。
pub type NumberConfirm = Rc<dyn Fn(i64, &mut Window, &mut App)>;

/// 文本输入弹窗的确认回调。
pub type TextConfirm = Rc<dyn Fn(&str, &mut Window, &mut App)>;

use crate::units::{GB, KB, MB};

/// 人类可读的字节数：≥1 GB / MB 保留 1 位小数，KB 取整，其余原样。
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 数字输入校验：空串 / 非法输入返回 `None`（弹窗保持打开），低于下限收敛到 `min`。
#[must_use]
pub fn parse_number_input(text: &str, min: i64) -> Option<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<i64>().ok().map(|value| value.max(min))
}

/// 查询家目录所在卷的剩余空间；非 macOS 无实现，返回 `None`（状态栏隐藏该单元格）。
///
/// # Safety
///
/// `statfs` 只写它自己的出参结构体，前提是那个结构体**类型正确且已初始化**——
/// 这里用 `libc::statfs`（不是 `statfs64`）配 `libc::statfs` 变量，两者同源；
/// `std::mem::zeroed()` 对它也是合法的（POD，无无效位模式）。路径是 `CString`，
/// 保证以 NUL 结尾且不含内部 NUL（`CString::new` 会拦下后者）。
#[cfg(target_os = "macos")]
#[must_use]
pub fn disk_free_bytes() -> Option<u64> {
    let home = std::env::var("HOME").ok()?;
    let path = std::ffi::CString::new(home).ok()?;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::statfs(path.as_ptr(), &mut fs) };
    if result != 0 {
        return None;
    }
    // 用饱和乘法：`f_bavail * f_bsize` 在网络卷上是两个 u64 相乘，理论上能溢出；
    // debug 构建下溢出即 panic，而这里只是「状态栏少显示一个数字」的场合。
    Some((fs.f_bavail as u64).saturating_mul(fs.f_bsize as u64))
}

/// 非 macOS 暂不实现。
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn disk_free_bytes() -> Option<u64> {
    None
}

/// 输入弹窗的字段描述：数字与文本两种弹窗共用同一套框体，只是后缀 / 占位与校验不同。
struct PromptSpec {
    title: &'static str,
    label: &'static str,
    /// 输入框右侧的固定后缀；空串表示不加。
    suffix: &'static str,
    /// 占位文案；空串表示不加。
    placeholder: &'static str,
    hint: Option<&'static str>,
}

/// 输入弹窗的内核确认回调：返回 `false` 表示校验未通过，弹窗保持打开。
type PromptConfirm = Rc<dyn Fn(&str, &mut Window, &mut App) -> bool>;

/// 输入弹窗通用实现：`on_ok` 返回 `false` 表示校验未通过，弹窗保持打开。
fn open_prompt(window: &mut Window, cx: &mut App, spec: PromptSpec, on_ok: PromptConfirm) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(spec.placeholder));
    let ok_input = input.clone();
    let content_input = input.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let ok_input = ok_input.clone();
        let on_ok = on_ok.clone();
        let content_input = content_input.clone();
        dialog
            .title(title_text(spec.title, cx))
            .w(px(520.))
            .button_props(
                DialogButtonProps::default()
                    // 按钮文案按弹窗渲染时的语言解析（弹窗开着时不跟随切换）。
                    .ok_text(duck_theme::tr(cx, "确定"))
                    .cancel_text(duck_theme::tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                let tokens = duck_theme::active_theme(cx).tokens().clone();
                let mut row = Input::new(&content_input).w_full();
                if !spec.suffix.is_empty() {
                    row = row.suffix(field_hint(spec.suffix, cx).flex_none());
                }
                let mut column = v_flex()
                    .w_full()
                    .gap(tokens.spacing.sm)
                    .child(field_label(spec.label, cx))
                    .child(row);
                if let Some(hint) = spec.hint {
                    column = column.child(field_hint(hint, cx));
                }
                content.child(column)
            })
            .on_ok(move |_, window, cx| {
                let text = ok_input.read(cx).value().to_string();
                on_ok(&text, window, cx)
            })
    });
    input.update(cx, |input, cx| input.focus(window, cx));
}

/// 文本输入弹窗：标题 + 输入 + 可选说明，确定时回调原始文本（不做校验）。
pub fn open_text_prompt(
    window: &mut Window,
    cx: &mut App,
    title: &'static str,
    label: &'static str,
    placeholder: &'static str,
    hint: Option<&'static str>,
    on_confirm: TextConfirm,
) {
    open_prompt(
        window,
        cx,
        PromptSpec {
            title,
            label,
            suffix: "",
            placeholder,
            hint,
        },
        Rc::new(move |text, window, cx| {
            on_confirm(text, window, cx);
            true
        }),
    );
}

/// 数字输入弹窗：标题 + 带单位后缀的输入 + 可选说明，确定时回调合法值。
pub fn open_number_prompt(
    window: &mut Window,
    cx: &mut App,
    title: &'static str,
    label: &'static str,
    unit: &'static str,
    hint: Option<&'static str>,
    min: i64,
    on_confirm: NumberConfirm,
) {
    open_prompt(
        window,
        cx,
        PromptSpec {
            title,
            label,
            suffix: unit,
            placeholder: "",
            hint,
        },
        Rc::new(move |text, window, cx| {
            let Some(value) = parse_number_input(text, min) else {
                return false;
            };
            on_confirm(value, window, cx);
            true
        }),
    );
}
