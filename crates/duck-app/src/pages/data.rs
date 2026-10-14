//! 「数据」设置页：备份 / 恢复 / 重置。
//!
//! 这一页只做**界面与流程**，真正的编排在 [`crate::backup`]。三个动作都走「原生对话框
//! 确认 → 干完给一条结果提示」，并且都遵守同一条约定：
//!
//! - 导出：弹「另存为」（`rfd`）拿目标路径。
//! - 导入 / 重置：**先弹确认框**，再动手；`backup` 那一层会先把现状自动整包备份。
//!
//! 两个开关（是否含日志、是否覆盖式导入）落在 `kv` 里，与「日志保留天数」同一个做法。
//! 但读写要成对地注意一点：`kv_put` 是 400ms 去抖的，写完立刻重绘会读回旧值、把开关
//! 弹回去，所以这里改完一律 `flush` 一次（[`set_flag`]）。
//!
//! 持久化数据在导入 / 重置后不会立刻反映到已建好的视图上（主题、语言、插件索引都是
//! 启动时装进内存的），所以两条成功提示里都写了「重启后生效」。

use std::path::PathBuf;

use duck_components::{
    ButtonVariant, body_text, button, option_row, option_row_stacked, settings_section,
    switch_control,
};
use duck_theme::{active_theme, tr, trf};
use gpui::{App, Div, IntoElement, ParentElement, Styled, Window};
use gpui_component::v_flex;

use crate::backup::{self, DataDirs, ExportOptions, ImportMode, ImportOptions, ResetScope};

/// 「导出时包含日志」的 kv 键。
const KV_INCLUDE_LOGS: &str = "backup_include_logs";
/// 「导入时完全覆盖」的 kv 键。
const KV_IMPORT_REPLACE: &str = "backup_import_replace";

/// 读一个布尔开关；未设置时取 `default`。
fn flag(cx: &App, key: &str, default: bool) -> bool {
    duck_store::kv_get(cx, key).map_or(default, |value| value == "true")
}

/// 写一个布尔开关并**立刻落盘 + 重绘**。
///
/// 少了 `flush` 这一步，开关会弹回去：重绘时 `kv_get` 读的还是去抖窗口里的旧值。
fn set_flag(cx: &mut App, key: &str, value: bool) {
    duck_store::kv_put(cx, key, if value { "true" } else { "false" });
    duck_store::flush(cx);
    cx.refresh_windows();
}

/// 页面内容：备份 / 恢复 / 重置三段。
pub(super) fn body(cx: &mut App) -> Div {
    let tokens = active_theme(cx).tokens().clone();
    let dirs = DataDirs::from_platform();

    let backup_section = settings_section(
        Some(tr(cx, "备份").into()),
        [
            export_row(cx),
            include_logs_row(cx),
            location_row(&dirs, cx),
        ],
        cx,
    );
    let restore_section = settings_section(
        Some(tr(cx, "恢复").into()),
        [import_row(cx), replace_mode_row(cx)],
        cx,
    );
    let reset_section = settings_section(
        Some(tr(cx, "重置").into()),
        [
            reset_row(ResetScope::Settings, cx),
            reset_row(ResetScope::SettingsAndPlugins, cx),
            reset_row(ResetScope::Everything, cx),
        ],
        cx,
    );

    v_flex()
        .w_full()
        .gap(tokens.spacing.lg)
        .child(backup_section)
        .child(restore_section)
        .child(reset_section)
}

/// 「导出备份」一行。
fn export_row(cx: &App) -> gpui::AnyElement {
    let include_logs = flag(cx, KV_INCLUDE_LOGS, false);
    option_row(
        tr(cx, "导出备份"),
        Some(
            tr(
                cx,
                "把设置、快捷键、AI 会话与已安装的插件打成一个归档文件。",
            )
            .into(),
        ),
        button(
            "duck-data-export",
            tr(cx, "导出…"),
            ButtonVariant::Primary,
            cx,
        )
        .on_click(move |_, window, cx| run_export(include_logs, window, cx)),
        cx,
    )
    .into_any_element()
}

/// 「备份中包含日志」一行。
fn include_logs_row(cx: &App) -> gpui::AnyElement {
    let checked = flag(cx, KV_INCLUDE_LOGS, false);
    option_row(
        tr(cx, "备份中包含日志"),
        Some(tr(cx, "日志体积较大，只在需要排查问题时才勾选。").into()),
        switch_control("duck-data-include-logs", checked, |next, _, cx| {
            set_flag(cx, KV_INCLUDE_LOGS, *next)
        }),
        cx,
    )
    .into_any_element()
}

/// 「自动备份位置」一行：显示路径 + 在文件管理器里打开。
fn location_row(dirs: &DataDirs, cx: &App) -> gpui::AnyElement {
    let backups = dirs.backups();
    let shown = backups.display().to_string();
    let for_reveal = backups.clone();
    option_row_stacked(
        tr(cx, "自动备份位置"),
        None,
        v_flex()
            .w_full()
            .gap(active_theme(cx).tokens().spacing.sm)
            .child(body_text(shown, cx))
            .child(
                button(
                    "duck-data-reveal",
                    tr(cx, "打开目录"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, _, _| duck_platform::reveal_in_finder(&for_reveal)),
            ),
        cx,
    )
    .into_any_element()
}

/// 「从备份文件导入」一行。
fn import_row(cx: &App) -> gpui::AnyElement {
    let replace = flag(cx, KV_IMPORT_REPLACE, false);
    option_row(
        tr(cx, "从备份文件导入"),
        Some(tr(cx, "导入前会自动把当前数据备份到上面的目录。").into()),
        button(
            "duck-data-import",
            tr(cx, "选择文件…"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(move |_, window, cx| run_import(replace, window, cx)),
        cx,
    )
    .into_any_element()
}

/// 「导入时完全覆盖」一行。
fn replace_mode_row(cx: &App) -> gpui::AnyElement {
    let checked = flag(cx, KV_IMPORT_REPLACE, false);
    option_row(
        tr(cx, "导入时完全覆盖"),
        Some(
            tr(
                cx,
                "关闭时按合并处理：只覆盖备份里出现的设置，本机独有的保留。",
            )
            .into(),
        ),
        switch_control("duck-data-import-replace", checked, |next, _, cx| {
            set_flag(cx, KV_IMPORT_REPLACE, *next)
        }),
        cx,
    )
    .into_any_element()
}

/// 重置三档中的一行。
fn reset_row(scope: ResetScope, cx: &App) -> gpui::AnyElement {
    let (title, description, label, variant) = match scope {
        ResetScope::Settings => (
            tr(cx, "重置应用设置"),
            tr(cx, "清空所有设置与使用记录，保留插件与日志。"),
            tr(cx, "重置设置"),
            ButtonVariant::Secondary,
        ),
        ResetScope::SettingsAndPlugins => (
            tr(cx, "重置设置与插件"),
            tr(cx, "在上一档的基础上，再删除已安装的插件与插件数据。"),
            tr(cx, "重置并删插件"),
            ButtonVariant::Secondary,
        ),
        ResetScope::Everything => (
            tr(cx, "全部清除"),
            tr(
                cx,
                "再删除窗口位置、自定义快捷键与日志，并清空 AI 会话与知识库索引，回到初次启动的样子。",
            ),
            tr(cx, "全部清除"),
            ButtonVariant::Destructive,
        ),
    };
    option_row(
        title,
        Some(description.into()),
        button(reset_id(scope), label, variant, cx)
            .on_click(move |_, window, cx| run_reset(scope, window, cx)),
        cx,
    )
    .into_any_element()
}

/// 每档重置按钮的稳定 id。
const fn reset_id(scope: ResetScope) -> &'static str {
    match scope {
        ResetScope::Settings => "duck-data-reset-settings",
        ResetScope::SettingsAndPlugins => "duck-data-reset-plugins",
        ResetScope::Everything => "duck-data-reset-all",
    }
}

/// 选目标路径 → 导出 → 提示。用户取消对话框时静默返回。
fn run_export(include_logs: bool, window: &mut Window, cx: &mut App) {
    let Some(path) = duck_platform::save_file(backup::default_export_name()) else {
        return;
    };
    let dirs = DataDirs::from_platform();
    let options = ExportOptions {
        include_plugins: true,
        include_logs,
        include_ai_db: true,
    };
    match backup::export(&with_tar_gz(path), &dirs, options, cx) {
        Ok(report) => duck_components::notify::success(
            trf(
                cx,
                "已导出到 {}（{}）",
                &[
                    &report.path.display().to_string(),
                    &duck_theme::bytes(report.bytes),
                ],
            ),
            window,
            cx,
        ),
        Err(error) => duck_components::notify::error(
            trf(cx, "导出失败：{}", &[&error.to_string()]),
            window,
            cx,
        ),
    }
}

/// 选包 → 确认 → 导入 → 提示。
fn run_import(replace: bool, window: &mut Window, cx: &mut App) {
    let Some(source) = duck_platform::open_file() else {
        return;
    };
    let question = if replace {
        tr(
            cx,
            "导入将完全以备份包为准，包里没有的设置会被清掉。导入前的数据会自动备份。继续？",
        )
    } else {
        tr(
            cx,
            "导入会用包里的值覆盖同名设置，本机独有的设置保留。导入前的数据会自动备份。继续？",
        )
    };
    if !duck_platform::confirm(tr(cx, "从备份导入？"), question) {
        return;
    }

    let dirs = DataDirs::from_platform();
    let mode = if replace {
        ImportMode::Replace
    } else {
        ImportMode::Merge
    };
    match backup::import(&source, &dirs, mode, ImportOptions::default(), cx) {
        Ok(report) => {
            // 老包没有 ai.db：如实说「包里的会话没带过来」，而不是假装全都在。
            let message = if report.ai_db {
                trf(
                    cx,
                    "已导入 {} 项设置与 AI 会话，重启应用后完全生效。",
                    &[&report.keys.to_string()],
                )
            } else {
                trf(
                    cx,
                    "已导入 {} 项设置，重启应用后完全生效（包里没有 AI 会话）。",
                    &[&report.keys.to_string()],
                )
            };
            duck_components::notify::success(message, window, cx)
        }
        Err(error) => duck_components::notify::error(
            trf(cx, "导入失败：{}", &[&error.to_string()]),
            window,
            cx,
        ),
    }
}

/// 确认 → 重置 → 提示。
fn run_reset(scope: ResetScope, window: &mut Window, cx: &mut App) {
    if !duck_platform::confirm(
        tr(cx, "确认重置？"),
        tr(
            cx,
            "当前数据会先自动备份到上面那个目录。重置后需要重启应用才会完全生效。",
        ),
    ) {
        return;
    }
    let dirs = DataDirs::from_platform();
    match backup::reset(scope, &dirs, cx) {
        Ok(report) => {
            let tables = report.tables.len().to_string();
            let removed = report.removed.len().to_string();
            let counts: [&str; 2] = [&tables, &removed];
            let message = if report.ai_db_cleared {
                trf(
                    cx,
                    "已清空 {} 张表、删除 {} 项内容，并清空 AI 会话与知识库索引，重启后生效。",
                    &counts,
                )
            } else {
                trf(cx, "已清空 {} 张表、删除 {} 项内容，重启后生效。", &counts)
            };
            duck_components::notify::success(message, window, cx)
        }
        Err(error) => duck_components::notify::error(
            trf(cx, "重置失败：{}", &[&error.to_string()]),
            window,
            cx,
        ),
    }
}

/// 补上 `.tar.gz` 后缀：`rfd` 的「另存为」只预填名字，用户可以改掉。
fn with_tar_gz(path: PathBuf) -> PathBuf {
    let name = path.to_string_lossy();
    if name.ends_with(".tar.gz") {
        path
    } else {
        PathBuf::from(format!("{name}.tar.gz"))
    }
}
