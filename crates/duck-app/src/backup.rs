//! 数据备份 / 恢复 / 重置：把「设置 + 快捷键 + 插件 + 日志」打包带走，也能原样收回。
//!
//! 分层：本模块只做**编排**——读哪些目录、写哪些条目、按什么顺序；真正的字节搬运在
//! [`duck_platform::archive`]（`tar.gz` 读写 + 逃逸 / 链接 / 炸弹三道校验）。
//!
//! 三条贯穿全模块的规则：
//!
//! 1. **快照前必须 flush**：`kv` 的写是 400ms 去抖的（见 `duck-store/writer.rs`），
//!    不先落盘就快照，会导出上一批的值——「刚改完设置立刻导出」是最容易踩的场景。
//! 2. **先验证，后动手**：导入时先把归档完整解包到暂存目录（逃逸 / 链接 / 上限全在这一步
//!    被拒），确认无误才去改现状。所以「包坏了」这条路径天然不破坏现状。
//! 3. **破坏性操作前先留退路**：导入与重置都会先把现状导出到 `backups/`，报告里带上路径。
//!    比「失败自动回滚」更可靠——回滚本身失败时，用户手上至少还有一个完整的包。
//!
//! 归档里**没有**凭据（`credentials.json` 与 `vault.key` 都是本机私有的东西，
//! 不属于可以随手分享的应用数据），这一点会写进清单的 `excludes` 字段，
//! 让用户看到「包里明确没有什么」。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use duck_platform::archive::{self, Entry, Source};
use duck_store::Table;
use gpui::App;
use serde::{Deserialize, Serialize};

/// 归档布局版本。**改动归档结构时必须 +1**：导入侧据此拒绝「比本程序新」的包，
/// 而不是拿老规则去猜新布局。
pub const FORMAT_VERSION: u32 = 1;

/// 归档内清单文件名。读侧第一个看它，缺它即判「不是本应用的备份包」。
const MANIFEST_NAME: &str = "manifest.json";
/// kv 快照在归档内的路径。
const SETTINGS_KV: &str = "settings/kv.json";
/// 快捷键覆盖文件在归档内的路径。
const SETTINGS_KEYBINDINGS: &str = "settings/keybindings.toml";
/// 插件目录在归档内的前缀。
const PLUGINS_PREFIX: &str = "plugins";
/// 日志目录在归档内的前缀。
const LOGS_PREFIX: &str = "logs";

// 清单里的内容块 id：给机器读的（排障时一眼看出包里有什么），不走 i18n。
const INCLUDE_SETTINGS: &str = "settings";
const INCLUDE_KEYBINDINGS: &str = "keybindings";
const INCLUDE_PLUGINS: &str = "plugins";
const INCLUDE_LOGS: &str = "logs";
/// 明确排除项：写进清单，避免用户以为「导出 = 一切都在里面」。
const EXCLUDE_CREDENTIALS: &str = "credentials";

/// 备份 / 恢复失败。
#[derive(Debug)]
pub enum BackupError {
    /// 归档读写失败（含逃逸 / 链接 / 解包上限）。
    Archive(archive::ArchiveError),
    /// 文件系统失败。
    Io(std::io::Error),
    /// JSON 序列化 / 反序列化失败。
    Json(serde_json::Error),
    /// 归档里没有清单——不是本应用产出的备份包。
    MissingManifest,
    /// 清单解析失败或字段不合法。
    BadManifest(String),
    /// 包的格式版本比本程序新，拒绝导入。
    UnsupportedVersion { found: u32, supported: u32 },
    /// 拒绝在一个不安全的路径上做递归删除（守卫见 [`assert_safe_to_remove`]）。
    UnsafePath(PathBuf),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Archive(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "I/O failed: {error}"),
            Self::Json(error) => write!(f, "malformed JSON: {error}"),
            Self::MissingManifest => write!(f, "the archive has no backup manifest"),
            Self::BadManifest(detail) => write!(f, "bad backup manifest: {detail}"),
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "the backup was written by a newer version (format {found} > {supported})"
            ),
            Self::UnsafePath(path) => {
                write!(
                    f,
                    "refusing to remove a path outside the data dir: {}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for BackupError {}

impl From<archive::ArchiveError> for BackupError {
    fn from(error: archive::ArchiveError) -> Self {
        Self::Archive(error)
    }
}

impl From<std::io::Error> for BackupError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for BackupError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// 应用数据目录集合。
///
/// 做成显式结构而不是到处调 `duck_platform::*_dir()`：备份逻辑要能被测试驱动到临时
/// 目录上，没有这层注入就只能靠改环境变量，那会让测试互相干扰。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    /// 应用数据目录（macOS 为 `~/Library/Application Support/DuckToolbox`）。
    pub app_data: PathBuf,
    /// 配置目录（`keybindings.toml` 的家）。
    pub config: PathBuf,
}

impl DataDirs {
    /// 从平台标准目录取。
    #[must_use]
    pub fn from_platform() -> Self {
        Self {
            app_data: duck_platform::app_data_dir(),
            config: duck_platform::config_dir(),
        }
    }

    /// 插件安装目录。
    #[must_use]
    pub fn plugins(&self) -> PathBuf {
        self.app_data.join(PLUGINS_PREFIX)
    }

    /// 插件私有数据目录。
    #[must_use]
    pub fn plugin_data(&self) -> PathBuf {
        self.app_data.join("plugin-data")
    }

    /// 日志目录。
    #[must_use]
    pub fn logs(&self) -> PathBuf {
        self.app_data.join("logs")
    }

    /// 自动备份落点（导入 / 重置前的退路）。
    #[must_use]
    pub fn backups(&self) -> PathBuf {
        self.app_data.join("backups")
    }

    /// 快捷键覆盖文件。
    #[must_use]
    pub fn keybindings(&self) -> PathBuf {
        self.config.join("keybindings.toml")
    }
}

/// 待写入归档的数据快照。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// kv 全表（应用设置）。
    pub kv: Vec<(String, String)>,
    /// 快捷键覆盖文件内容（不存在时为 `None`）。
    pub keybindings: Option<String>,
}

/// 导出选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportOptions {
    /// 是否把已安装的插件（含包本体）打进归档。
    pub include_plugins: bool,
    /// 是否把日志打进归档。默认关：日志体积大，且排障之外的场景用不到。
    pub include_logs: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            include_plugins: true,
            include_logs: false,
        }
    }
}

/// 归档清单（`manifest.json` 的 wire 形状）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupManifest {
    /// 归档布局版本。
    pub format_version: u32,
    /// 产出该包的应用版本。
    pub app_version: String,
    /// 产出时间（Unix 秒）。
    pub created_at: i64,
    /// 包含的内容块 id。
    pub includes: Vec<String>,
    /// 明确排除的内容 id。
    pub excludes: Vec<String>,
}

/// 导出结果。
#[derive(Debug, Clone)]
pub struct ExportReport {
    /// 产出的归档路径。
    pub path: PathBuf,
    /// 压缩后的字节数。
    pub bytes: u64,
    /// 写进归档的清单。
    pub manifest: BackupManifest,
}

/// 默认导出文件名：`duck-backup-YYYYMMDD-HHmmss.tar.gz`。
#[must_use]
pub fn default_export_name() -> String {
    format!("duck-backup-{}.tar.gz", duck_platform::file_stamp())
}

/// 当前编译版本，写进清单（「这个包是哪一版产的」是排障时第一个要问的）。
fn app_version() -> String {
    duck_update::compiled_version().to_string()
}

/// 收集当前环境里该备份的数据。
///
/// 附带一次 [`duck_store::flush`] —— 见模块注释第 1 条。
#[must_use]
pub fn collect(cx: &App) -> Snapshot {
    duck_store::flush(cx);
    let kv = duck_store::Store::global(cx)
        .map(|store| store.kv_all())
        .unwrap_or_default();
    let keybindings = std::fs::read_to_string(DataDirs::from_platform().keybindings()).ok();
    Snapshot { kv, keybindings }
}

/// 把快照写成 `tar.gz`。
pub fn write_archive(
    dest: &Path,
    dirs: &DataDirs,
    snapshot: &Snapshot,
    options: ExportOptions,
    app_version: &str,
) -> Result<ExportReport, BackupError> {
    if let Some(parent) = dest.parent() {
        duck_platform::ensure_dir(parent)?;
    }

    let mut includes = vec![INCLUDE_SETTINGS.to_owned()];
    let mut entries = vec![Entry {
        name: SETTINGS_KV.to_owned(),
        source: Source::Bytes(kv_json(&snapshot.kv).into_bytes()),
    }];
    if let Some(text) = &snapshot.keybindings {
        includes.push(INCLUDE_KEYBINDINGS.to_owned());
        entries.push(Entry {
            name: SETTINGS_KEYBINDINGS.to_owned(),
            source: Source::Bytes(text.clone().into_bytes()),
        });
    }
    if options.include_plugins && dirs.plugins().is_dir() {
        includes.push(INCLUDE_PLUGINS.to_owned());
        entries.push(Entry {
            name: PLUGINS_PREFIX.to_owned(),
            source: Source::Dir(dirs.plugins()),
        });
    }
    if options.include_logs && dirs.logs().is_dir() {
        includes.push(INCLUDE_LOGS.to_owned());
        entries.push(Entry {
            name: LOGS_PREFIX.to_owned(),
            source: Source::Dir(dirs.logs()),
        });
    }

    let manifest = BackupManifest {
        format_version: FORMAT_VERSION,
        app_version: app_version.to_owned(),
        created_at: duck_store::now_seconds(),
        includes,
        excludes: vec![EXCLUDE_CREDENTIALS.to_owned()],
    };
    // 清单放第一个条目：`read_member` 顺序扫描，常见的「先读清单」路径少走几步。
    entries.insert(
        0,
        Entry {
            name: MANIFEST_NAME.to_owned(),
            source: Source::Bytes(serde_json::to_vec_pretty(&manifest)?),
        },
    );

    let bytes = archive::create(dest, &entries)?;
    Ok(ExportReport {
        path: dest.to_path_buf(),
        bytes,
        manifest,
    })
}

/// 导出到 `dest`（先收集、再写包）。
pub fn export(
    dest: &Path,
    dirs: &DataDirs,
    options: ExportOptions,
    cx: &App,
) -> Result<ExportReport, BackupError> {
    let snapshot = collect(cx);
    let report = write_archive(dest, dirs, &snapshot, options, &app_version())?;
    tracing::info!(
        path = %report.path.display(),
        bytes = report.bytes,
        includes = ?report.manifest.includes,
        "data export written"
    );
    Ok(report)
}

/// 已校验并解包好的归档内容。
#[derive(Debug, Clone)]
pub struct Loaded {
    /// 包内清单。
    pub manifest: BackupManifest,
    /// 包内的 kv 快照。
    pub kv: Vec<(String, String)>,
    /// 包内的快捷键文件内容。
    pub keybindings: Option<String>,
    /// 解包落点（调用方用完负责清理）。
    pub staging: PathBuf,
}

/// 校验归档并把内容解包到 `staging`。
///
/// **不动任何现状**：清单缺失 / 版本过新 / 结构非法 / 含链接 / 会逃逸，都在这里被拦住。
pub fn load_archive(src: &Path, staging: &Path) -> Result<Loaded, BackupError> {
    let raw = archive::read_member(src, MANIFEST_NAME)?.ok_or(BackupError::MissingManifest)?;
    let manifest: BackupManifest = serde_json::from_slice(&raw)
        .map_err(|error| BackupError::BadManifest(error.to_string()))?;
    if manifest.format_version > FORMAT_VERSION {
        return Err(BackupError::UnsupportedVersion {
            found: manifest.format_version,
            supported: FORMAT_VERSION,
        });
    }

    if staging.exists() {
        std::fs::remove_dir_all(staging)?;
    }
    archive::extract(src, staging)?;

    let kv = read_kv(staging)?;
    let keybindings = std::fs::read_to_string(staging.join(SETTINGS_KEYBINDINGS)).ok();
    Ok(Loaded {
        manifest,
        kv,
        keybindings,
        staging: staging.to_path_buf(),
    })
}

/// 导入策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// 完全以包为准：包内没有的键会被清掉。
    Replace,
    /// 只覆盖包内出现的键，本机独有的保留。
    Merge,
}

/// 导入选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportOptions {
    /// 是否把包内的插件合并进插件目录（按文件名覆盖同名内容）。
    pub import_plugins: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            import_plugins: true,
        }
    }
}

/// 导入结果。
#[derive(Debug, Clone)]
pub struct ImportReport {
    /// 恢复回来的 kv 键数。
    pub keys: usize,
    /// 是否覆盖了快捷键文件。
    pub keybindings: bool,
    /// 是否合并了插件目录。
    pub plugins: bool,
    /// 导入前的自动备份路径。
    pub backup: PathBuf,
    /// 包内清单。
    pub manifest: BackupManifest,
}

/// 从归档恢复。
///
/// 顺序是刻意的：**校验 → 解包到暂存 → 备份现状 → 才动手**。所以「包坏了」不会留下
/// 半截状态；而「应用过程中出错」还有一个完整的 pre-import 包可回退。
pub fn import(
    src: &Path,
    dirs: &DataDirs,
    mode: ImportMode,
    options: ImportOptions,
    cx: &App,
) -> Result<ImportReport, BackupError> {
    let stamp = duck_platform::file_stamp();
    let staging = dirs.backups().join(format!(".staging-{stamp}"));
    let loaded = load_archive(src, &staging)?;

    // 退路：先把现状整包导出（含插件），再动任何东西。
    let backup = dirs.backups().join(format!("pre-import-{stamp}.tar.gz"));
    let snapshot = collect(cx);
    write_archive(
        &backup,
        dirs,
        &snapshot,
        ExportOptions {
            include_plugins: true,
            include_logs: false,
        },
        &app_version(),
    )?;

    match mode {
        ImportMode::Replace => duck_store::kv_replace_all(cx, loaded.kv.clone()),
        ImportMode::Merge => {
            for (key, value) in &loaded.kv {
                duck_store::kv_put(cx, key.clone(), value.clone());
            }
        }
    }
    duck_store::flush(cx);

    let mut wrote_keybindings = false;
    if let Some(text) = &loaded.keybindings {
        duck_platform::ensure_dir(&dirs.config)?;
        std::fs::write(dirs.keybindings(), text)?;
        wrote_keybindings = true;
    }

    let mut merged_plugins = false;
    let staged_plugins = loaded.staging.join(PLUGINS_PREFIX);
    if options.import_plugins && staged_plugins.is_dir() {
        copy_dir_merge(&staged_plugins, &dirs.plugins())?;
        merged_plugins = true;
    }

    let _ = std::fs::remove_dir_all(&loaded.staging);
    let report = ImportReport {
        keys: loaded.kv.len(),
        keybindings: wrote_keybindings,
        plugins: merged_plugins,
        backup,
        manifest: loaded.manifest,
    };
    // 备份路径与「包里到底带了什么」进日志：提示条上说不下这么多，排障时却样样要问。
    tracing::info!(
        keys = report.keys,
        keybindings = report.keybindings,
        plugins = report.plugins,
        from_version = %report.manifest.app_version,
        backup = %report.backup.display(),
        "data import applied"
    );
    Ok(report)
}

/// 重置范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetScope {
    /// 应用设置 + 使用记录（保留窗口位置、插件与日志）。
    Settings,
    /// 设置 + 插件（插件包与插件数据一并删除）。
    SettingsAndPlugins,
    /// 全清：再删窗口位置、快捷键覆盖与日志。
    Everything,
}

/// 重置结果。
#[derive(Debug, Clone)]
pub struct ResetReport {
    /// 重置前的自动备份路径。
    pub backup: PathBuf,
    /// 被清空的表。
    pub tables: Vec<Table>,
    /// 被删除的目录 / 文件。
    pub removed: Vec<PathBuf>,
}

/// 清哪些表。
#[must_use]
pub fn reset_tables(scope: ResetScope) -> Vec<Table> {
    match scope {
        ResetScope::Settings | ResetScope::SettingsAndPlugins => {
            vec![Table::Kv, Table::Recent, Table::ToolState]
        }
        ResetScope::Everything => {
            vec![Table::Kv, Table::Recent, Table::ToolState, Table::Windows]
        }
    }
}

/// 恢复出厂状态。
///
/// 同样先自动备份现状——重置是「把东西扔掉」，而人后悔是常态。
pub fn reset(scope: ResetScope, dirs: &DataDirs, cx: &App) -> Result<ResetReport, BackupError> {
    let stamp = duck_platform::file_stamp();
    let backup = dirs.backups().join(format!("pre-reset-{stamp}.tar.gz"));
    let snapshot = collect(cx);
    write_archive(
        &backup,
        dirs,
        &snapshot,
        ExportOptions {
            include_plugins: true,
            include_logs: false,
        },
        &app_version(),
    )?;

    let tables = reset_tables(scope);
    duck_store::clear_tables(cx, &tables);
    duck_store::flush(cx);

    let mut removed: Vec<PathBuf> = Vec::new();
    if matches!(
        scope,
        ResetScope::SettingsAndPlugins | ResetScope::Everything
    ) {
        for dir in [dirs.plugins(), dirs.plugin_data()] {
            if dir.is_dir() {
                assert_safe_to_remove(dirs, &dir)?;
                std::fs::remove_dir_all(&dir)?;
                removed.push(dir);
            }
        }
    }
    if scope == ResetScope::Everything {
        let binding = dirs.keybindings();
        if binding.is_file() {
            std::fs::remove_file(&binding)?;
            removed.push(binding);
        }
    }

    let report = ResetReport {
        backup,
        tables,
        removed,
    };
    tracing::info!(
        ?scope,
        tables = report.tables.len(),
        removed = report.removed.len(),
        backup = %report.backup.display(),
        "data reset applied"
    );
    Ok(report)
}

/// 删除前的守卫：路径必须**绝对**、位于 `dirs.app_data` 之下、且不等于数据目录本身。
///
/// 这不是防「恶意输入」（路径全部来自平台目录，不含用户输入），而是防**改错的常量**——
/// 一个拼错的 `join("")` 就可能让递归删除落到应用数据目录本身上。
fn assert_safe_to_remove(dirs: &DataDirs, path: &Path) -> Result<(), BackupError> {
    let inside = path.is_absolute() && path.starts_with(&dirs.app_data) && path != dirs.app_data;
    if inside {
        Ok(())
    } else {
        Err(BackupError::UnsafePath(path.to_path_buf()))
    }
}

/// kv 快照 → JSON（键序稳定，便于人工比对两个包）。
fn kv_json(kv: &[(String, String)]) -> String {
    let map: BTreeMap<&str, &str> = kv.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    serde_json::to_string_pretty(&map).unwrap_or_else(|_| "{}".to_owned())
}

/// 读包内的 kv 快照；包内缺这一项说明它不是完整的本应用备份。
fn read_kv(staging: &Path) -> Result<Vec<(String, String)>, BackupError> {
    let path = staging.join(SETTINGS_KV);
    if !path.is_file() {
        return Err(BackupError::BadManifest(format!("missing {SETTINGS_KV}")));
    }
    let text = std::fs::read_to_string(&path)?;
    let map: BTreeMap<String, String> = serde_json::from_str(&text)?;
    Ok(map.into_iter().collect())
}

/// 递归合并目录：同名文件覆盖，其余保留。
fn copy_dir_merge(src: &Path, dest: &Path) -> Result<(), BackupError> {
    duck_platform::ensure_dir(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        // 归档侧已经拒过链接，这里是第二道保险：真出现链接就跳过，绝不跟随。
        if file_type.is_symlink() {
            tracing::warn!(path = %entry.path().display(), "skipping a link while importing");
            continue;
        }
        let target = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_merge(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录；进程内自增序号避免并行测试撞同一个目录。
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("duck-backup-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建立临时目录");
        dir
    }

    fn dirs_for(root: &Path) -> DataDirs {
        DataDirs {
            app_data: root.join("app-data"),
            config: root.join("config"),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            kv: vec![
                ("appearance".to_owned(), "{\"mode\":\"dark\"}".to_owned()),
                ("locale".to_owned(), "en".to_owned()),
            ],
            keybindings: Some("[[bindings]]\nkey = \"cmd+k\"\n".to_owned()),
        }
    }

    /// 导出 → 解包 → 内容一致（不依赖 gpui）。
    #[test]
    fn archive_round_trips_settings() {
        let dir = scratch("round-trip");
        let dirs = dirs_for(&dir);
        std::fs::create_dir_all(dirs.app_data.join("plugins/demo")).unwrap();
        std::fs::write(
            dirs.app_data.join("plugins/demo/plugin.toml"),
            "id=\"demo\"",
        )
        .unwrap();

        let archive = dir.join("backup.tar.gz");
        let report = write_archive(
            &archive,
            &dirs,
            &snapshot(),
            ExportOptions {
                include_plugins: true,
                include_logs: false,
            },
            "9.9.9",
        )
        .expect("导出");

        assert_eq!(report.manifest.format_version, FORMAT_VERSION);
        assert_eq!(report.manifest.app_version, "9.9.9");
        assert!(report.bytes > 0);
        assert!(
            report
                .manifest
                .includes
                .contains(&INCLUDE_SETTINGS.to_owned())
        );
        assert!(
            report
                .manifest
                .includes
                .contains(&INCLUDE_PLUGINS.to_owned())
        );
        assert!(
            !report.manifest.includes.contains(&INCLUDE_LOGS.to_owned()),
            "没开日志就不该声明含日志"
        );
        assert_eq!(
            report.manifest.excludes,
            vec![EXCLUDE_CREDENTIALS.to_owned()]
        );

        let staging = dir.join("staging");
        let loaded = load_archive(&archive, &staging).expect("读包");
        assert_eq!(loaded.kv, snapshot().kv);
        assert_eq!(
            loaded.keybindings.as_deref(),
            Some("[[bindings]]\nkey = \"cmd+k\"\n")
        );
        assert!(
            staging.join("plugins/demo/plugin.toml").is_file(),
            "插件目录要能从包里解出来"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缺清单的归档必须被拒，且**不动**任何现状。
    #[test]
    fn rejects_archive_without_manifest() {
        let dir = scratch("no-manifest");
        let archive = dir.join("other.tar.gz");
        archive::create(
            &archive,
            &[Entry {
                name: "notes.txt".to_owned(),
                source: Source::Bytes(b"not a backup".to_vec()),
            }],
        )
        .expect("造一个不相干的包");

        let staging = dir.join("staging");
        let error = load_archive(&archive, &staging).expect_err("缺清单必须报错");
        assert!(
            matches!(error, BackupError::MissingManifest),
            "错误应是 MissingManifest，实际 {error:?}"
        );
        assert!(!staging.exists() || std::fs::read_dir(&staging).unwrap().next().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未来版本的包：拒绝，而不是按老规则去猜。
    #[test]
    fn rejects_future_format_version() {
        let dir = scratch("future-version");
        let archive = dir.join("future.tar.gz");
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION + 1,
            app_version: "99.0.0".to_owned(),
            created_at: 0,
            includes: vec![INCLUDE_SETTINGS.to_owned()],
            excludes: vec![],
        };
        archive::create(
            &archive,
            &[
                Entry {
                    name: MANIFEST_NAME.to_owned(),
                    source: Source::Bytes(serde_json::to_vec(&manifest).unwrap()),
                },
                Entry {
                    name: SETTINGS_KV.to_owned(),
                    source: Source::Bytes(b"{}".to_vec()),
                },
            ],
        )
        .expect("造包");

        let error = load_archive(&archive, &dir.join("staging")).expect_err("未来版本必须被拒");
        assert!(
            matches!(error, BackupError::UnsupportedVersion { .. }),
            "错误应是 UnsupportedVersion，实际 {error:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 有清单但缺 kv 快照：判为不完整包。
    #[test]
    fn rejects_manifest_without_kv_snapshot() {
        let dir = scratch("no-kv");
        let archive = dir.join("partial.tar.gz");
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION,
            app_version: "0.1.0".to_owned(),
            created_at: 0,
            includes: vec![INCLUDE_SETTINGS.to_owned()],
            excludes: vec![],
        };
        archive::create(
            &archive,
            &[Entry {
                name: MANIFEST_NAME.to_owned(),
                source: Source::Bytes(serde_json::to_vec(&manifest).unwrap()),
            }],
        )
        .expect("造包");

        let error = load_archive(&archive, &dir.join("staging")).expect_err("缺 kv 必须报错");
        assert!(
            matches!(error, BackupError::BadManifest(_)),
            "错误应是 BadManifest，实际 {error:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 合并目录：同名覆盖、异名保留。
    #[test]
    fn copy_dir_merge_overwrites_and_keeps() {
        let dir = scratch("merge");
        let src = dir.join("src");
        let dest = dir.join("dest");
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::create_dir_all(dest.join("a")).unwrap();
        std::fs::write(src.join("a/x.txt"), "new").unwrap();
        std::fs::write(src.join("top.txt"), "added").unwrap();
        std::fs::write(dest.join("a/x.txt"), "old").unwrap();
        std::fs::write(dest.join("a/keep.txt"), "kept").unwrap();

        copy_dir_merge(&src, &dest).expect("合并");

        assert_eq!(
            std::fs::read_to_string(dest.join("a/x.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("a/keep.txt")).unwrap(),
            "kept"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("top.txt")).unwrap(),
            "added"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 重置范围的分级：只有「全清」才动窗口几何。
    #[test]
    fn reset_scopes_are_layered() {
        assert_eq!(
            reset_tables(ResetScope::Settings),
            vec![Table::Kv, Table::Recent, Table::ToolState]
        );
        assert!(
            !reset_tables(ResetScope::SettingsAndPlugins).contains(&Table::Windows),
            "删插件不该把窗口位置也清掉"
        );
        assert!(reset_tables(ResetScope::Everything).contains(&Table::Windows));
    }

    /// 删除守卫：数据目录本身与目录之外的路径一律拒绝。
    #[test]
    fn removal_guard_rejects_shallow_paths() {
        let dir = scratch("guard");
        let dirs = dirs_for(&dir);
        std::fs::create_dir_all(dirs.plugins()).unwrap();

        assert!(assert_safe_to_remove(&dirs, &dirs.plugins()).is_ok());
        assert!(
            assert_safe_to_remove(&dirs, &dirs.app_data).is_err(),
            "不许删数据目录本身"
        );
        assert!(
            assert_safe_to_remove(&dirs, &dir.join("elsewhere")).is_err(),
            "不许删数据目录之外的路径"
        );
        assert!(
            assert_safe_to_remove(&dirs, Path::new("relative/plugins")).is_err(),
            "相对路径一律拒绝"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
