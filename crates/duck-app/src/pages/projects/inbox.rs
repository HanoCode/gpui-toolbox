//! 文件收件箱：外部脚本 / AI 助手把 JSON 丢进 `<应用数据目录>/project-inbox/`，
//! 运行期自动追加成任务（参考 `temp/TO-DO-Panel` 的 `todo-inbox`）。
//!
//! # 为什么要有这条通道
//!
//! 「从文件导入」已经有一条**手动**的路（`dialogs::open_import`：选文件 → 预览 →
//! 确认）。它缺的是「没有人在按按钮」的那些场景：终端里跑完一段脚本、AI 助手拆出
//! 五条待办、另一台机器同步过来一份清单 —— 那些情况下用户要的是**写个文件就完事**，
//! 而不是回主窗点四层。
//!
//! # 口径（三条，与手动导入**刻意不同**）
//!
//! 1. **只追加，不改不删**：收件箱不参与任何更新路径，已有任务一条都不碰。
//! 2. **不按标题去重**：手动导入会挡「库里已有的同名任务」（那是人工选择，防手抖），
//!    但外部写入是「这条新工作确实又发生了一次」——「周一例会」每周一条是正常的。
//!    收件箱靠**文件被处理后移走**来保证同一个文件不会被吃两遍。
//! 3. **失败也要留痕**：解析失败 / 超限的文件同样移进 `processed/`，并生成
//!    `<名字>.report.json` 说明原因。留在原地只会每 2 秒重试一次，把日志刷满。
//!
//! # 静置判定
//!
//! 文件**最后修改**后满 [`SETTLE`] 才读：外部写入方常常「创建 → 分几次写 → 关闭」，
//! 早读会读到半截 JSON。超 [`MAX_BYTES`] 的文件不读、直接归档并报错（1 MiB 足够装
//! 几千条任务；再大的文件该走手动导入看预览）。
//!
//! # 崩溃窗口
//!
//! 顺序是「先落库、后移走」。两者之间进程被杀，下一次会**再导入一遍**（重复任务
//! 可见、可删）。反过来先移走再落库，崩溃就是**静默丢任务** —— 对一个「收活」的
//! 通道，重复比丢失好收场。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gpui::{App, Global};

use duck_components::notify;
use duck_project::{ArchiveScope, CalendarDate, ProjectStore, now_seconds};
use duck_theme::trf;

use super::import::{self, SourceFormat};

/// 轮询间隔。收件箱是**本机**通道，2 秒足够「写完就看见」，又不至于一直占着 CPU。
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 文件最后修改后静置多久才读（见模块头「静置判定」）。
const SETTLE: Duration = Duration::from_secs(1);

/// 单个文件大小上限；超了不读，直接归档并报错。
const MAX_BYTES: u64 = 1024 * 1024;

/// 收件箱目录名（在应用数据目录下）。
pub(crate) const DIR_NAME: &str = "project-inbox";

/// 处理完的文件放这里。
const PROCESSED_DIR: &str = "processed";

/// 收件箱的绝对路径。目录不存在时由 [`run_round`] 建出来 —— 让用户能「先看到它」，
/// 而不是读文档才知道该往哪儿放文件。
#[must_use]
pub(crate) fn inbox_dir() -> PathBuf {
    duck_platform::app_data_dir().join(DIR_NAME)
}

/// 这个文件名该不该吃。
///
/// 只认名字（不看内容）：`*.json`、不以 `.` 开头（编辑器 / 脚本的临时文件）、
/// 且不是我们自己写出的 `*.report.json`。
fn is_candidate(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".json") && !name.starts_with('.') && !lower.ends_with(".report.json")
}

/// 已经写完了吗（最后修改距今满 [`SETTLE`]）。
fn settled(modified: Option<SystemTime>) -> bool {
    match modified.and_then(|at| at.elapsed().ok()) {
        Some(age) => age >= SETTLE,
        // 拿不到修改时间（某些网络盘）：不赌，按「还没写完」处理，下一轮再试。
        None => false,
    }
}

/// 一个文件的处理结果（也是报告里的一条）。
struct Outcome {
    /// 原始文件名。
    file: String,
    /// 建出来的任务数。
    imported: usize,
    /// 解析时被丢掉的行数（空行 / 标题为空）。
    dropped: usize,
    /// 失败原因（英文，来自内核或 IO；报告与日志用它）。
    error: Option<String>,
}

impl Outcome {
    fn new(file: String) -> Self {
        Self {
            file,
            imported: 0,
            dropped: 0,
            error: None,
        }
    }

    /// 报告里的状态词。**进文件**，别改（与 TO-DO-Panel 同款三档）。
    fn status(&self) -> &'static str {
        match (self.error.is_some(), self.imported) {
            (true, 0) => "rejected",
            (true, _) => "partial",
            (false, 0) => "empty",
            (false, _) => "imported",
        }
    }
}

/// 处理一个文件：读 → 解析 → 落库。**不负责**归档（由 [`archive`] 收尾）。
fn process_file(path: &Path, store: &ProjectStore, today: CalendarDate) -> Outcome {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_owned();
    let mut outcome = Outcome::new(name);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            outcome.error = Some(format!("read failed: {error}"));
            return outcome;
        }
    };
    let parsed = match import::parse(&text, SourceFormat::Json, today) {
        Ok(parsed) => parsed,
        Err(error) => {
            outcome.error = Some(error.to_string());
            return outcome;
        }
    };
    outcome.dropped = parsed.dropped;
    let projects = store.list_projects(ArchiveScope::Live).unwrap_or_default();
    let result = (|| -> duck_project::Result<()> {
        for row in &parsed.rows {
            let project_id = row.project.as_ref().and_then(|name| {
                projects
                    .iter()
                    .find(|project| project.name.eq_ignore_ascii_case(name))
                    .map(|project| project.id)
            });
            let mut tag_ids = Vec::new();
            for name in &row.tags {
                tag_ids.push(store.ensure_tag(name, duck_project::DEFAULT_PROJECT_COLOR)?);
            }
            store.create_task(import::new_task(row, project_id, tag_ids))?;
            outcome.imported += 1;
        }
        Ok(())
    })();
    if let Err(error) = result {
        outcome.error = Some(error.to_string());
    }
    outcome
}

/// 把处理完的文件挪进 `processed/`，并在旁边写报告。
///
/// 名字前加一个时间戳：同一个文件名一周里写十次也不会互相覆盖，报告能一条条排开。
fn archive(dir: &Path, file: &str, outcome: &Outcome) -> std::io::Result<()> {
    let processed = dir.join(PROCESSED_DIR);
    fs::create_dir_all(&processed)?;
    let stem = file.strip_suffix(".json").unwrap_or(file);
    let base = format!("{}-{stem}", now_seconds());
    let archived = processed.join(format!("{base}.json"));
    fs::rename(dir.join(file), &archived)?;
    let report = serde_json::json!({
        "version": 1,
        "file": outcome.file,
        "status": outcome.status(),
        "imported": outcome.imported,
        "dropped": outcome.dropped,
        "error": outcome.error,
    });
    fs::write(
        processed.join(format!("{base}.report.json")),
        serde_json::to_vec_pretty(&report).unwrap_or_default(),
    )
}

/// 扫一轮：把所有「已写完且不是报告」的 JSON 处理掉。
///
/// `dir` 是入参而不是现取 [`inbox_dir`]：单测要能指向临时目录。
fn poll_once(dir: &Path, store: &ProjectStore, today: CalendarDate) -> Vec<Outcome> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| is_candidate(name))
        .collect();
    // 顺序稳定：同一批文件按名字处理，报告与日志可复现。
    names.sort();

    let mut outcomes = Vec::new();
    for name in names {
        let path = dir.join(&name);
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.len() > MAX_BYTES {
            let mut outcome = Outcome::new(name.clone());
            outcome.error = Some(format!("file is larger than {MAX_BYTES} bytes"));
            if let Err(error) = archive(dir, &name, &outcome) {
                tracing::warn!(%error, %name, "failed to archive an oversized inbox file");
            }
            outcomes.push(outcome);
            continue;
        }
        if !settled(meta.modified().ok()) {
            continue;
        }
        let outcome = process_file(&path, store, today);
        if let Err(error) = archive(dir, &name, &outcome) {
            // 归档失败 → 文件留在原地 → 下一轮会**再导入一次**。这里只能把话说清楚，
            // 静默假装成功会让用户对着两份任务猜哪份是真的。
            tracing::error!(%error, %name, "failed to archive a processed inbox file");
        }
        outcomes.push(outcome);
    }
    outcomes
}

/// 「收件箱轮询已挂上」的哨兵：`install` 可能被调多次，靠它保证只挂一条链。
struct InboxLoop;

impl Global for InboxLoop {}

/// 挂上收件箱轮询（幂等）。由 `app::install` 调用。
pub(crate) fn install(cx: &mut App) {
    if cx.has_global::<InboxLoop>() {
        return;
    }
    cx.set_global(InboxLoop);
    cx.spawn(async move |cx| {
        // 连接只开一次，但**每轮都重试**（与 `remind` 同口径）：库还没建好时不该永久放弃。
        let mut store: Option<ProjectStore> = None;
        loop {
            if store.is_none() {
                match ProjectStore::open_default() {
                    Ok(opened) => store = Some(opened),
                    Err(error) => {
                        tracing::warn!(%error, "failed to open the projects database for the inbox");
                    }
                }
            }
            if let Some(store) = store.as_ref() {
                cx.update(|cx| run_round(store, cx));
            }
            cx.background_executor().timer(POLL_INTERVAL).await;
        }
    })
    .detach();
}

/// 跑一轮并按需回执。回执只报「有文件被处理」的轮次：轮询每 2 秒一次，空转不吭声。
fn run_round(store: &ProjectStore, cx: &mut App) {
    let dir = inbox_dir();
    // 目录不存在就建出来：用户看得见它，才知道能往这儿放文件。
    if let Err(error) = fs::create_dir_all(&dir) {
        tracing::warn!(%error, "failed to create the project inbox directory");
        return;
    }
    let now = now_seconds();
    let offset = duck_platform::local_utc_offset_seconds(now);
    let today = CalendarDate::from_local_unix_seconds(now, offset);
    let outcomes = poll_once(&dir, store, today);
    if outcomes.is_empty() {
        return;
    }

    let imported: usize = outcomes.iter().map(|outcome| outcome.imported).sum();
    let failures: Vec<String> = outcomes
        .iter()
        .filter_map(|outcome| outcome.error.as_ref().map(|_| outcome.file.clone()))
        .collect();
    for outcome in &outcomes {
        match &outcome.error {
            Some(error) => tracing::warn!(
                file = %outcome.file,
                imported = outcome.imported,
                %error,
                "project inbox file was rejected"
            ),
            None => tracing::info!(
                file = %outcome.file,
                imported = outcome.imported,
                dropped = outcome.dropped,
                "project inbox file imported"
            ),
        }
    }
    if imported > 0 {
        // 打开着的页面 / 浮窗各自重读（它们自己不看这个目录）。
        super::bus::notify(cx);
    }
    let imported_text = imported.to_string();
    crate::update::on_notify_window(cx, move |window, cx| {
        if imported > 0 {
            notify::success(
                trf(cx, "收件箱已导入 {} 个任务", &[&imported_text]),
                window,
                cx,
            );
        }
        for file in &failures {
            notify::error(trf(cx, "收件箱处理失败：{}", &[file]), window, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_project::TaskQuery;

    fn scratch(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("duck-inbox-{tag}-{stamp}"));
        fs::create_dir_all(&dir).expect("临时目录");
        dir
    }

    fn today() -> CalendarDate {
        CalendarDate::new(2026, 10, 16).expect("合法日期")
    }

    fn store() -> ProjectStore {
        ProjectStore::open_in_memory().expect("内存库")
    }

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).expect("写文件");
        // 绕过静置判定：把修改时间往前拨（测试不该真的 sleep 一秒）。
        let old = SystemTime::now() - SETTLE - Duration::from_secs(1);
        let file = fs::File::options()
            .write(true)
            .open(dir.join(name))
            .expect("打开");
        file.set_modified(old).expect("拨时间");
    }

    #[test]
    fn only_plain_json_files_are_picked_up() {
        assert!(is_candidate("agent-1.json"));
        assert!(is_candidate("A.JSON"));
        assert!(
            !is_candidate(".agent.tmp.json"),
            "点文件是写到一半的临时文件"
        );
        assert!(!is_candidate("agent-1.report.json"), "报告不是输入");
        assert!(!is_candidate("agent-1.csv"), "收件箱只吃 JSON");
    }

    #[test]
    fn a_dropped_file_becomes_tasks_and_is_archived() {
        let dir = scratch("happy");
        let store = store();
        write(
            &dir,
            "agent.json",
            r#"{"tasks":[{"title":"修登录超时","priority":"P1"},{"title":"补周报"}]}"#,
        );

        let outcomes = poll_once(&dir, &store, today());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].imported, 2);
        assert_eq!(outcomes[0].status(), "imported");
        assert!(!dir.join("agent.json").exists(), "原文件必须被移走");
        let processed = dir.join(PROCESSED_DIR);
        let reports: Vec<String> = fs::read_dir(&processed)
            .expect("processed")
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .collect();
        assert!(
            reports.iter().any(|name| name.ends_with(".report.json")),
            "必须留下报告：{reports:?}"
        );
        let tasks = store.list_tasks(&TaskQuery::new()).expect("查任务");
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn a_half_written_file_is_left_for_the_next_round() {
        let dir = scratch("settle");
        let store = store();
        // 刚写完（没拨时间）：这一轮不许读。
        fs::write(dir.join("wip.json"), r#"[{"title":"写到一半"}]"#).expect("写");
        let outcomes = poll_once(&dir, &store, today());
        assert!(outcomes.is_empty(), "静置期内的文件不读");
        assert!(dir.join("wip.json").exists());
        assert!(
            store
                .list_tasks(&TaskQuery::new())
                .expect("查任务")
                .is_empty()
        );
    }

    #[test]
    fn broken_json_is_archived_with_a_reason() {
        let dir = scratch("broken");
        let store = store();
        write(&dir, "bad.json", "{ 这不是 JSON }");
        let outcomes = poll_once(&dir, &store, today());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status(), "rejected");
        assert!(outcomes[0].error.is_some());
        assert!(!dir.join("bad.json").exists(), "失败也要移走，否则每轮重试");
        assert!(
            store
                .list_tasks(&TaskQuery::new())
                .expect("查任务")
                .is_empty()
        );
    }

    #[test]
    fn an_oversized_file_is_rejected_without_being_read() {
        let dir = scratch("huge");
        let store = store();
        let body = "0".repeat(MAX_BYTES as usize + 1);
        write(&dir, "huge.json", &body);
        let outcomes = poll_once(&dir, &store, today());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status(), "rejected");
        let dir_after = dir.join(PROCESSED_DIR);
        assert!(dir_after.exists());
    }

    #[test]
    fn the_same_file_name_can_be_dropped_twice() {
        let dir = scratch("twice");
        let store = store();
        write(&dir, "agent.json", r#"[{"title":"周一例会"}]"#);
        poll_once(&dir, &store, today());
        write(&dir, "agent.json", r#"[{"title":"周一例会"}]"#);
        poll_once(&dir, &store, today());
        let tasks = store.list_tasks(&TaskQuery::new()).expect("查任务");
        assert_eq!(
            tasks.len(),
            2,
            "收件箱只追加：两个文件就是两条（靠移走文件防重复，不靠标题）"
        );
    }

    #[test]
    fn an_empty_file_reports_as_empty() {
        let dir = scratch("empty");
        let store = store();
        write(&dir, "none.json", "[]");
        let outcomes = poll_once(&dir, &store, today());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status(), "empty");
    }

    #[test]
    fn a_project_name_in_the_file_links_to_the_existing_project() {
        let dir = scratch("project");
        let store = store();
        let project = store
            .create_project(duck_project::NewProject::new("Alpha"))
            .expect("建项目");
        write(
            &dir,
            "agent.json",
            r#"[{"title":"接上项目","project":"alpha"}]"#,
        );
        poll_once(&dir, &store, today());
        let tasks = store.list_tasks(&TaskQuery::new()).expect("查任务");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].project_id, Some(project));
    }

    #[test]
    fn a_row_without_a_title_is_dropped_and_counted() {
        let dir = scratch("dropped");
        let store = store();
        write(&dir, "mixed.json", r#"[{"title":"有标题"},{"title":""}]"#);
        let outcomes = poll_once(&dir, &store, today());
        assert_eq!(outcomes[0].imported, 1);
        assert!(outcomes[0].dropped >= 1, "空标题要记进 dropped");
        let tasks = store.list_tasks(&TaskQuery::new()).expect("查任务");
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            store.get_task(tasks[0].id).expect("读").unwrap().title,
            "有标题"
        );
    }

    #[test]
    fn nothing_happens_when_the_directory_does_not_exist() {
        let store = store();
        let outcomes = poll_once(Path::new("/nonexistent/duck-inbox"), &store, today());
        assert!(outcomes.is_empty());
    }
}
