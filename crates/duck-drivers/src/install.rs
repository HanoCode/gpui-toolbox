//! 驱动商店：**安装 / 更新 / 卸载 / 离线搬运**，以及给 `duck-db-agent` 用的启动描述。
//!
//! # 目录布局
//!
//! ```text
//! <base>/                       ← `app_data_dir()/drivers`
//!   state.json                  ← 已装版本 / JRE / 下载源（见 `state.rs`）
//!   drivers/<key>/agent.jar     ← jar 轨载荷
//!   drivers/<key>/agent         ← native 轨载荷（可执行）
//!   jre-21/                     ← 托管 JRE（见 `jre.rs`）
//!   download-cache/             ← 下下来的工件原件（重装不再下；离线导出也从这里拿）
//!   staging/<key>-<stamp>/      ← 解包中转站，装完就删
//! ```
//!
//! # 安装是「下载 → 校验 → 解包 → 原子替换」
//!
//! 1. **取工件**：注册表说本平台有 native 就用 native，没有才回落 jar（`registry.rs`）。
//! 2. **下**：先看缓存里那份摘要对不对，不对或没有才走网络（`download.rs`）。
//! 3. **校验**：解包**之前**重新读盘算一遍 SHA-256 —— 注册表承诺的摘要与磁盘上的字节
//!    必须一致，否则删掉缓存、报 [`DriverError::Integrity`](crate::DriverError::Integrity)。
//! 4. **解包**到 `staging/`，认载荷、对包内元数据（`archive.rs`）。
//! 5. **替换**：新版本先落到 `drivers/.<key>.new-<stamp>`，再把旧目录改名让位，最后
//!    把新目录改成正式名字。任何一步失败都不会留下半个 `drivers/<key>` ——
//!    用户要么是旧版本、要么是新版本，不会是「装了一半跑不起来」。
//! 6. **记账**：更新 `state.json`。
//!
//! # 并发
//!
//! 同一时刻只允许一个安装动作（[`DriverStore`] 内部一把 `Mutex<()>` 串起来）：
//! 两个驱动同时解包到同一片 staging、或者一个刚把 `drivers/<key>` 改名让位而另一个
//! 正读它，都是不该出现的中间态。用户连点几个「安装」的体验是**排队**，不是并行。
//!
//! # 离线包
//!
//! 导出 = 一个 **tar**（不压缩，我们只有 zstd *解码器*，没有压缩器），里面是
//! `manifest.json`（注册表条目 + 导出时间）+ `packages/<key>.tar.zst`（**原件**）。
//! 导入就是「从本地包走一遍安装流程」，不碰网络。这样离线搬运出来的东西与在线装的
//! 逐字节相同 —— 校验摘要是同一套。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::events::{DriverEvent, EventSink, ProgressStep};
use crate::jre::{self, JavaRuntimeMode};
use crate::registry::{ArtifactFormat, ArtifactKind, DriverInfo, DriverRegistry};
use crate::state::{DriverState, InstalledDriver};
use crate::{DownloadSource, DriverError, Result};

/// 取消标志：界面点「取消」时置位，安装流程在每个可中断点查它。
///
/// 用 `Arc<AtomicBool>` 而不是通道：这个标志要被下载回调**高频**读（每 64 KB 一次），
/// 而且**读方不需要阻塞等待**。
#[derive(Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    /// 一个新的（未取消）标志。
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// 请求取消。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// 是否已请求取消。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// 下载回调要的回答。
    fn flow(&self) -> crate::Flow {
        if self.is_cancelled() {
            crate::Flow::Abort
        } else {
            crate::Flow::Continue
        }
    }

    /// 在可中断点上检查一次。
    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(DriverError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl std::fmt::Debug for CancelFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelFlag")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// 商店的统计口径（界面上的「已安装 / 未安装 / 可更新 / 占用」）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreSummary {
    /// 驱动目录里一共多少条。
    pub total: usize,
    /// 已安装多少条。
    pub installed: usize,
    /// 其中多少条有新版。
    pub updatable: usize,
    /// 已安装工件的字节数合计。
    pub bytes_installed: u64,
    /// 托管 JRE 是否已装好（jar 轨驱动的先决条件）。
    pub jre_ready: bool,
    /// 托管 JRE 的键。
    pub jre_key: String,
}

/// 离线包预览（导入前给用户看「这里面有什么」）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfflinePackage {
    /// 包里的驱动（键 + 版本），按包内顺序。
    pub drivers: Vec<(String, String)>,
    /// 导出的时间（Unix 秒）。
    pub exported_at: u64,
    /// 包文件字节数。
    pub bytes: u64,
}

/// 驱动进程的启动描述（交给 `duck-db-agent`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// 要跑的程序（native 载荷本身，或 `java`）。
    pub program: PathBuf,
    /// 参数。
    pub args: Vec<String>,
    /// 工作目录（驱动目录）。
    pub working_dir: PathBuf,
    /// 轨别（界面/日志用）。
    pub kind: ArtifactKind,
}

/// 驱动商店。
///
/// 一个进程里通常只有一个（`DriverStore::new()`）；测试与离线校验用
/// [`DriverStore::new_at`] 指到临时目录。
pub struct DriverStore {
    base_dir: PathBuf,
    state: Mutex<DriverState>,
    /// 安装动作的串行锁（见模块文档「并发」）。
    busy: Mutex<()>,
}

impl std::fmt::Debug for DriverStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverStore")
            .field("base_dir", &self.base_dir)
            .finish()
    }
}

impl Default for DriverStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DriverStore {
    /// 用应用数据目录下的 `drivers/`。
    #[must_use]
    pub fn new() -> Self {
        Self::new_at(duck_platform::app_data_dir().join("drivers"))
    }

    /// 指到某个目录（测试、离线校验）。
    #[must_use]
    pub fn new_at(base_dir: PathBuf) -> Self {
        let state = DriverState::load(&base_dir);
        Self {
            base_dir,
            state: Mutex::new(state),
            busy: Mutex::new(()),
        }
    }

    /// 根目录。
    #[must_use]
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// 某条驱动的安装目录。
    #[must_use]
    pub fn driver_dir(&self, key: &str) -> PathBuf {
        self.base_dir.join("drivers").join(key)
    }

    /// 下载缓存目录。
    #[must_use]
    pub fn cache_dir(&self) -> PathBuf {
        self.base_dir.join("download-cache")
    }

    /// 解包中转站。
    #[must_use]
    pub fn staging_dir(&self) -> PathBuf {
        self.base_dir.join("staging")
    }

    /// 状态快照。
    #[must_use]
    pub fn state(&self) -> DriverState {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }

    /// 当前下载源。
    #[must_use]
    pub fn download_source(&self) -> DownloadSource {
        self.state().download_source
    }

    /// 换下载源并落盘。
    ///
    /// # Errors
    /// 写盘失败。
    pub fn set_download_source(&self, source: DownloadSource) -> Result<()> {
        self.mutate_state(|state| state.download_source = source)
    }

    /// 改 Java 运行时档位并落盘。
    ///
    /// # Errors
    /// 写盘失败。
    pub fn set_java_runtime(&self, runtime: crate::JavaRuntime) -> Result<()> {
        self.mutate_state(|state| state.java_runtime = runtime)
    }

    /// 已安装的那条驱动（没装返回 `None`）。
    #[must_use]
    pub fn installed(&self, key: &str) -> Option<InstalledDriver> {
        self.state().drivers.get(key).cloned()
    }

    /// 该用哪个 `java`（按档位解析，见 `jre::resolve_java`）。
    #[must_use]
    pub fn java_for(&self, jre_key: &str) -> Option<PathBuf> {
        jre::resolve_java(&self.base_dir, &self.state().java_runtime, jre_key)
    }

    /// 统计（`registry` 为 `None` 时不算「可更新」）。
    #[must_use]
    pub fn summary(&self, registry: Option<&DriverRegistry>) -> StoreSummary {
        let state = self.state();
        let installed_entries: Vec<&InstalledDriver> = state.drivers.values().collect();
        let platform = crate::platform::current();
        StoreSummary {
            total: crate::catalog::catalog().len(),
            installed: state
                .drivers
                .keys()
                .filter(|key| crate::catalog::entry(key).is_some())
                .count(),
            updatable: installed_entries
                .iter()
                .filter(|driver| {
                    registry
                        .and_then(|registry| registry.driver(&driver.key))
                        .is_some_and(|info| info.version != driver.version)
                })
                .count(),
            bytes_installed: installed_entries.iter().map(|driver| driver.bytes).sum(),
            jre_ready: jre::resolve_java(&self.base_dir, &state.java_runtime, jre::DEFAULT_KEY)
                .is_some(),
            jre_key: {
                let _ = platform;
                jre::DEFAULT_KEY.to_owned()
            },
        }
    }

    /// 清掉下载缓存，返回清掉的字节数。
    pub fn clear_download_cache(&self) -> u64 {
        let dir = self.cache_dir();
        let bytes = dir_size(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        bytes
    }

    /// 装一条驱动（已装则升级）。
    ///
    /// # Errors
    /// 见模块文档的六个步骤；任何一步失败都保持现状（不部分生效）。
    pub fn install(
        &self,
        registry: &DriverRegistry,
        key: &str,
        sink: &EventSink,
        cancel: &CancelFlag,
    ) -> Result<InstalledDriver> {
        let _serial = self.busy.lock().map_err(|_| lock_error())?;
        let result = self.install_locked(registry, key, sink, cancel);
        match &result {
            Ok(_) => sink.send(DriverEvent::step(key, ProgressStep::Finished)),
            Err(error) => sink
                .send(DriverEvent::step(key, ProgressStep::Finished).with_error(error.to_string())),
        }
        result
    }

    fn install_locked(
        &self,
        registry: &DriverRegistry,
        key: &str,
        sink: &EventSink,
        cancel: &CancelFlag,
    ) -> Result<InstalledDriver> {
        let info = registry.driver(key).ok_or_else(|| {
            DriverError::Format(format!("the registry has no driver named {key:?}"))
        })?;
        let platform = crate::platform::current();
        let resolved = info.artifact_for(platform).ok_or_else(|| {
            DriverError::Unsupported(format!(
                "{key} has no artifact for {}",
                platform.unwrap_or("this platform")
            ))
        })?;
        cancel.check()?;

        // jar 轨先确保有 Java：装到一半才发现没 JVM，等于白下一遍。
        if resolved.kind == ArtifactKind::Jar {
            let jre_key = info.jre_key().unwrap_or(jre::DEFAULT_KEY).to_owned();
            self.ensure_jre(registry, &jre_key, sink, cancel)?;
        }

        let archive = self.obtain_artifact(key, resolved.info, sink, cancel)?;
        cancel.check()?;

        sink.send(DriverEvent::step(key, ProgressStep::Extract));
        let payload = self.extract_to_staging(key, &archive)?;

        // 包内元数据对不上就不装：说明这个包不是给这条驱动的（缓存串了、镜像串了）。
        if let Some(manifest) =
            crate::archive::read_package_manifest(payload.parent_for_manifest())?
            && let Some(declared) = manifest.driver(key)
            && declared.version != info.version
        {
            let _ = std::fs::remove_dir_all(payload.staging());
            return Err(DriverError::Format(format!(
                "the package declares version {} for {key}, the registry announced {}",
                declared.version, info.version
            )));
        }

        sink.send(DriverEvent::step(key, ProgressStep::Install));
        cancel.check()?;
        let installed = self.replace_driver_dir(key, &payload, resolved.kind, info)?;
        let _ = std::fs::remove_dir_all(payload.staging());
        Ok(installed)
    }

    /// 升级全部有新版可用的驱动。
    ///
    /// 单条失败不中断其余（返回每条的成败），失败原因由调用方展示。
    pub fn upgrade_all(
        &self,
        registry: &DriverRegistry,
        sink: &EventSink,
        cancel: &CancelFlag,
    ) -> Vec<(String, Result<InstalledDriver>)> {
        let state = self.state();
        let mut pending: Vec<String> = state
            .drivers
            .values()
            .filter(|driver| {
                registry
                    .driver(&driver.key)
                    .is_some_and(|info| info.version != driver.version)
            })
            .map(|driver| driver.key.clone())
            .collect();
        pending.sort();
        let count = pending.len() as u32;

        let mut outcomes = Vec::new();
        for (index, key) in pending.into_iter().enumerate() {
            if cancel.is_cancelled() {
                outcomes.push((key, Err(DriverError::Cancelled)));
                break;
            }
            let result = self.install(
                registry,
                &key,
                &sink.at_count(index as u32 + 1, count),
                cancel,
            );
            outcomes.push((key, result));
        }
        outcomes
    }

    /// 卸载一条驱动（含它的安装目录）。
    ///
    /// **不动托管 JRE**：多个 jar 轨驱动共用一个 JVM，卸载一个不等于别人不用了。
    /// 要清 JRE 用 [`Self::uninstall_jre`]。
    ///
    /// # Errors
    /// 删目录失败。
    pub fn uninstall(&self, key: &str, sink: &EventSink) -> Result<()> {
        let _serial = self.busy.lock().map_err(|_| lock_error())?;
        sink.send(DriverEvent::step(key, ProgressStep::Remove));
        let dir = self.driver_dir(key);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|error| DriverError::Io(format!("{}: {error}", dir.display())))?;
        }
        self.mutate_state(|state| {
            state.drivers.remove(key);
        })?;
        sink.send(DriverEvent::step(key, ProgressStep::Finished));
        Ok(())
    }

    /// 确保某个 JRE 可用（托管档才装；系统 / 自定义档只做**检查**）。
    ///
    /// 返回该 JRE 键对应的 `java` 路径。
    ///
    /// # Errors
    /// 托管档下载 / 解包 / 落位失败；系统与自定义档找不到可用的 `java`。
    pub fn ensure_jre(
        &self,
        registry: &DriverRegistry,
        jre_key: &str,
        sink: &EventSink,
        cancel: &CancelFlag,
    ) -> Result<PathBuf> {
        let runtime = self.state().java_runtime;
        if runtime.mode != JavaRuntimeMode::Managed {
            return jre::resolve_java(&self.base_dir, &runtime, jre_key).ok_or_else(|| {
                DriverError::Unsupported(format!(
                    "no java runtime is available for the {:?} mode",
                    runtime.mode
                ))
            });
        }

        if let Some(java) = jre::managed_java(&self.base_dir, jre_key) {
            return Ok(java);
        }

        let jre = registry.jre(jre_key).ok_or_else(|| {
            DriverError::Format(format!("the registry has no JRE named {jre_key:?}"))
        })?;
        let platform = crate::platform::current();
        let info = platform
            .and_then(|platform| jre.platforms.get(platform))
            .ok_or_else(|| {
                DriverError::Unsupported(format!(
                    "JRE {jre_key} has no artifact for {}",
                    platform.unwrap_or("this platform")
                ))
            })?;

        sink.send(DriverEvent::step(jre_key, ProgressStep::JreDownload));
        let archive = self.obtain_artifact(jre_key, info, sink, cancel)?;
        cancel.check()?;

        sink.send(DriverEvent::step(jre_key, ProgressStep::Extract));
        let staging = self
            .staging_dir()
            .join(format!("jre-{jre_key}-{}", stamp()));
        let _ = std::fs::remove_dir_all(&staging);
        crate::archive::extract_tar_zstd(&archive, &staging)?;
        // 归档在顶层套了一层目录（实测 `dbx-jre/`），剥掉它再落位。
        let root = jre::archive_root(&staging);

        sink.send(DriverEvent::step(jre_key, ProgressStep::Install));
        let target = jre::managed_dir(&self.base_dir, jre_key);
        let previous = target.with_extension("old");
        let _ = std::fs::remove_dir_all(&previous);
        if target.exists() {
            std::fs::rename(&target, &previous)
                .map_err(|error| DriverError::Io(format!("{}: {error}", target.display())))?;
        }
        if let Err(error) = std::fs::rename(&root, &target) {
            // 让位失败就把旧目录放回去，别把用户已有的 JRE 弄丢。
            let _ = std::fs::rename(&previous, &target);
            let _ = std::fs::remove_dir_all(&staging);
            return Err(DriverError::Io(format!("{}: {error}", target.display())));
        }
        let _ = std::fs::remove_dir_all(&previous);
        let _ = std::fs::remove_dir_all(&staging);

        self.mutate_state(|state| {
            state
                .jre_versions
                .insert(jre_key.to_owned(), jre.version.clone());
        })?;

        let java = jre::managed_java(&self.base_dir, jre_key).ok_or_else(|| {
            DriverError::Format(format!(
                "JRE {jre_key} was unpacked but contains no bin/java"
            ))
        })?;
        // 探一次版本：能跑起来才算装好（架构不对的 JRE 会在这里现形）。
        match jre::probe_major_version(&java) {
            Ok(major) if major >= jre::MINIMUM_MAJOR => {
                sink.send(DriverEvent::step(jre_key, ProgressStep::Finished));
                Ok(java)
            }
            Ok(major) => {
                let _ = std::fs::remove_dir_all(&target);
                Err(DriverError::Unsupported(format!(
                    "the installed Java runtime reports major version {major}, {}+ is required",
                    jre::MINIMUM_MAJOR
                )))
            }
            Err(message) => {
                let _ = std::fs::remove_dir_all(&target);
                Err(DriverError::Format(format!(
                    "the installed JRE is not runnable: {message}"
                )))
            }
        }
    }

    /// 卸载托管 JRE（只删托管目录，不碰系统 / 自定义档的 java）。
    ///
    /// # Errors
    /// 删目录失败。
    pub fn uninstall_jre(&self, jre_key: &str) -> Result<()> {
        let dir = jre::managed_dir(&self.base_dir, jre_key);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|error| DriverError::Io(format!("{}: {error}", dir.display())))?;
        }
        self.mutate_state(|state| {
            state.jre_versions.remove(jre_key);
        })
    }

    /// 驱动的启动描述：jar 轨给 `java -jar …`，native 轨给可执行文件本身。
    ///
    /// # Errors
    /// 驱动没装、载荷文件缺失、jar 轨找不到可用的 `java`。
    pub fn launch_spec(&self, key: &str) -> Result<LaunchSpec> {
        let driver = self.installed(key).ok_or_else(|| {
            DriverError::Unsupported(format!("the driver {key:?} is not installed"))
        })?;
        let dir = self.driver_dir(key);
        let program = driver.entry_path(&dir);
        if !program.is_file() {
            return Err(DriverError::Format(format!(
                "the installed driver {key:?} has no payload at {}",
                program.display()
            )));
        }

        match driver.kind {
            ArtifactKind::Native => Ok(LaunchSpec {
                program,
                args: Vec::new(),
                working_dir: dir,
                kind: ArtifactKind::Native,
            }),
            ArtifactKind::Jar => {
                let jre_key = driver.jre.as_deref().unwrap_or(jre::DEFAULT_KEY);
                let java = self.java_for(jre_key).ok_or_else(|| {
                    DriverError::Unsupported(format!(
                        "no java runtime is available for the driver {key:?}"
                    ))
                })?;
                Ok(LaunchSpec {
                    program: java,
                    args: jar_launch_args(key, &program),
                    working_dir: dir,
                    kind: ArtifactKind::Jar,
                })
            }
        }
    }

    /// 导出离线包（tar）。
    ///
    /// `keys` 为空表示「全部已安装的驱动」。需要联网补齐缓存里缺的工件。
    ///
    /// # Errors
    /// 取工件失败、写 tar 失败。
    pub fn export_offline(
        &self,
        registry: &DriverRegistry,
        keys: &[String],
        destination: &Path,
        sink: &EventSink,
        cancel: &CancelFlag,
    ) -> Result<OfflinePackage> {
        let state = self.state();
        let selected: Vec<(String, DriverInfo)> = if keys.is_empty() {
            let mut all: Vec<String> = state.drivers.keys().cloned().collect();
            all.sort();
            all
        } else {
            let mut given = keys.to_vec();
            given.sort();
            given
        }
        .into_iter()
        .filter_map(|key| registry.driver(&key).map(|info| (key, info.clone())))
        .collect();

        if selected.is_empty() {
            return Err(DriverError::Unsupported(
                "there is nothing to export: no installed driver matches the registry".to_owned(),
            ));
        }

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| DriverError::Io(format!("{}: {error}", parent.display())))?;
        }
        let file = std::fs::File::create(destination)
            .map_err(|error| DriverError::Io(format!("{}: {error}", destination.display())))?;
        let mut builder = tar::Builder::new(file);

        // 先收齐工件（缺的补下），再写 tar：tar 是单遍写的，中途才发现缺文件
        // 会留下一个结构完整但内容不全的包。
        let mut archives: Vec<(String, String, PathBuf)> = Vec::new();
        let count = selected.len() as u32;
        for (index, (key, info)) in selected.iter().enumerate() {
            let platform = crate::platform::current();
            let resolved = info.artifact_for(platform).ok_or_else(|| {
                DriverError::Unsupported(format!("{key} has no artifact for this platform"))
            })?;
            let archive = self.obtain_artifact(
                key,
                resolved.info,
                &sink.at_count(index as u32 + 1, count),
                cancel,
            )?;
            archives.push((key.clone(), info.version.clone(), archive));
        }

        let manifest = OfflineManifest {
            kind: OFFLINE_KIND.to_owned(),
            version: OFFLINE_VERSION,
            exported_at: unix_seconds(),
            drivers: archives
                .iter()
                .map(|(key, version, path)| OfflineEntry {
                    key: key.clone(),
                    version: version.clone(),
                    package: format!("packages/{key}.tar.zst"),
                    sha256: crate::download::sha256_file(path).unwrap_or_default(),
                    size: std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
                    format: ArtifactFormat::TarZstd,
                })
                .collect(),
        };
        append_bytes(
            &mut builder,
            "manifest.json",
            serde_json::to_vec_pretty(&manifest)
                .map_err(|error| DriverError::Format(format!("offline manifest: {error}")))?
                .as_slice(),
        )?;
        for (key, _version, path) in &archives {
            builder
                .append_path_with_name(path, format!("packages/{key}.tar.zst"))
                .map_err(|error| DriverError::Io(format!("{}: {error}", path.display())))?;
        }
        builder
            .finish()
            .map_err(|error| DriverError::Io(format!("{}: {error}", destination.display())))?;

        let bytes = std::fs::metadata(destination)
            .map(|meta| meta.len())
            .unwrap_or(0);
        sink.send(DriverEvent::step("offline", ProgressStep::Finished));
        Ok(OfflinePackage {
            drivers: archives
                .into_iter()
                .map(|(key, version, _)| (key, version))
                .collect(),
            exported_at: manifest.exported_at,
            bytes,
        })
    }

    /// 看一个离线包里有什么（不解包到安装目录）。
    ///
    /// # Errors
    /// 不是 tar、没有 manifest、结构不符。
    pub fn inspect_offline(&self, package: &Path) -> Result<OfflinePackage> {
        let (_temp, manifest) = self.read_offline(package)?;
        Ok(OfflinePackage {
            drivers: manifest
                .drivers
                .iter()
                .map(|entry| (entry.key.clone(), entry.version.clone()))
                .collect(),
            exported_at: manifest.exported_at,
            bytes: std::fs::metadata(package)
                .map(|meta| meta.len())
                .unwrap_or(0),
        })
    }

    /// 从离线包安装（不碰网络）。
    ///
    /// # Errors
    /// 包结构不对、摘要不符、解包 / 落位失败。
    pub fn import_offline(
        &self,
        package: &Path,
        sink: &EventSink,
        cancel: &CancelFlag,
    ) -> Result<Vec<(String, Result<InstalledDriver>)>> {
        let _serial = self.busy.lock().map_err(|_| lock_error())?;
        let (temp, manifest) = self.read_offline(package)?;
        let mut outcomes = Vec::new();
        let count = manifest.drivers.len() as u32;

        for (index, entry) in manifest.drivers.iter().enumerate() {
            if cancel.is_cancelled() {
                outcomes.push((entry.key.clone(), Err(DriverError::Cancelled)));
                break;
            }
            let archive = temp.join(&entry.package);
            let result = self.install_from_local_archive(
                entry,
                &archive,
                &sink.at_count(index as u32 + 1, count),
                cancel,
            );
            if let Err(error) = &result {
                sink.send(
                    DriverEvent::step(&entry.key, ProgressStep::Finished)
                        .with_error(error.to_string()),
                );
            }
            outcomes.push((entry.key.clone(), result));
        }
        Ok(outcomes)
    }

    // ── 内部 ──────────────────────────────────────────────────────────────

    /// 取工件：缓存里那份摘要对就用它，否则下载。
    fn obtain_artifact(
        &self,
        key: &str,
        info: &crate::registry::ArtifactInfo,
        sink: &EventSink,
        cancel: &CancelFlag,
    ) -> Result<PathBuf> {
        cancel.check()?;
        let cache = self.cache_dir();
        std::fs::create_dir_all(&cache)
            .map_err(|error| DriverError::Io(format!("{}: {error}", cache.display())))?;
        let filename = info
            .url
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("artifact.tar.zst")
            .to_owned();
        let destination = cache.join(&filename);

        if destination.is_file()
            && let Some(expected) = info.sha256.as_deref()
            && let Ok(actual) = crate::download::sha256_file(&destination)
            && crate::download::digest_matches(expected, &actual)
        {
            tracing::debug!(%filename, "reusing the cached artifact");
            return Ok(destination);
        }

        sink.send(DriverEvent::transfer(
            key,
            ProgressStep::Download,
            0,
            info.size,
        ));
        let urls = self.download_source().artifact_candidate_urls(&info.url);
        let downloaded =
            crate::download::stream(&urls, info.size, &destination, &|done, total| {
                sink.send(DriverEvent::transfer(
                    key,
                    ProgressStep::Download,
                    done,
                    total,
                ));
                cancel.flow()
            })?;

        sink.send(DriverEvent::step(key, ProgressStep::Verify));
        self.verify_artifact(&destination, info, downloaded.sha256())?;
        Ok(destination)
    }

    /// 摘要与体积双重把关（**重新读盘**算，不信传输途中那个）。
    fn verify_artifact(
        &self,
        path: &Path,
        info: &crate::registry::ArtifactInfo,
        in_flight_sha: &str,
    ) -> Result<()> {
        let actual_size = std::fs::metadata(path)
            .map_err(|error| DriverError::Io(format!("{}: {error}", path.display())))?
            .len();
        if actual_size != info.size {
            let _ = std::fs::remove_file(path);
            return Err(DriverError::Integrity(format!(
                "{} is {actual_size} bytes, the registry announced {}",
                path.display(),
                info.size
            )));
        }
        let actual = crate::download::sha256_file(path)?;
        if let Some(expected) = info.sha256.as_deref()
            && !crate::download::digest_matches(expected, &actual)
        {
            let _ = std::fs::remove_file(path);
            return Err(DriverError::Integrity(format!(
                "{} does not match the announced sha256",
                path.display()
            )));
        }
        if !crate::download::digest_matches(in_flight_sha, &actual) {
            tracing::warn!(
                path = %path.display(),
                "the artifact changed between the transfer and the verification?"
            );
        }
        Ok(())
    }

    fn extract_to_staging(&self, key: &str, archive: &Path) -> Result<StagedPayload> {
        let staging = self.staging_dir().join(format!("{key}-{}", stamp()));
        let _ = std::fs::remove_dir_all(&staging);
        // 解包是把外部字节写进用户目录的那一步，先确保中转站干净。
        crate::archive::extract_tar_zstd(archive, &staging)?;
        let payload = crate::archive::find_payload(&staging)?;
        Ok(StagedPayload {
            path: payload.path,
            kind: payload.kind,
            staging,
        })
    }

    /// 把 staging 里的载荷挪成正式安装目录（先让位、后改名）。
    fn replace_driver_dir(
        &self,
        key: &str,
        payload: &StagedPayload,
        kind: ArtifactKind,
        info: &DriverInfo,
    ) -> Result<InstalledDriver> {
        let drivers_dir = self.base_dir.join("drivers");
        std::fs::create_dir_all(&drivers_dir)
            .map_err(|error| DriverError::Io(format!("{}: {error}", drivers_dir.display())))?;

        let new_dir = drivers_dir.join(format!(".{key}.new-{}", stamp()));
        let _ = std::fs::remove_dir_all(&new_dir);
        std::fs::create_dir_all(&new_dir)
            .map_err(|error| DriverError::Io(format!("{}: {error}", new_dir.display())))?;

        let entry = match kind {
            ArtifactKind::Jar => new_dir.join("agent.jar"),
            ArtifactKind::Native => new_dir.join(crate::platform::executable_name("agent")),
        };
        std::fs::copy(&payload.path, &entry).map_err(|error| {
            DriverError::Io(format!(
                "{} -> {}: {error}",
                payload.path.display(),
                entry.display()
            ))
        })?;
        if kind == ArtifactKind::Native {
            set_executable(&entry)?;
        }

        let target = self.driver_dir(key);
        let previous = drivers_dir.join(format!(".{key}.old-{}", stamp()));
        if target.exists() {
            std::fs::rename(&target, &previous)
                .map_err(|error| DriverError::Io(format!("{}: {error}", target.display())))?;
        }
        if let Err(error) = std::fs::rename(&new_dir, &target) {
            let _ = std::fs::rename(&previous, &target);
            let _ = std::fs::remove_dir_all(&new_dir);
            return Err(DriverError::Io(format!("{}: {error}", target.display())));
        }
        let _ = std::fs::remove_dir_all(&previous);

        let driver = InstalledDriver {
            key: key.to_owned(),
            version: info.version.clone(),
            installed_at: unix_seconds(),
            kind,
            jre: (kind == ArtifactKind::Jar)
                .then(|| info.jre_key().unwrap_or(jre::DEFAULT_KEY).to_owned()),
            bytes: std::fs::metadata(&entry)
                .map(|meta| meta.len())
                .unwrap_or(0),
        };
        self.mutate_state(|state| {
            state.drivers.insert(key.to_owned(), driver.clone());
        })?;
        Ok(driver)
    }

    /// 从本地包（离线导入）走一遍安装：验摘要 → 解包 → 落位。
    fn install_from_local_archive(
        &self,
        entry: &OfflineEntry,
        archive: &Path,
        sink: &EventSink,
        cancel: &CancelFlag,
    ) -> Result<InstalledDriver> {
        cancel.check()?;
        if !archive.is_file() {
            return Err(DriverError::Format(format!(
                "the offline package is missing {}",
                archive.display()
            )));
        }
        sink.send(DriverEvent::step(&entry.key, ProgressStep::Verify));
        let actual = crate::download::sha256_file(archive)?;
        if !entry.sha256.is_empty() && !crate::download::digest_matches(&entry.sha256, &actual) {
            return Err(DriverError::Integrity(format!(
                "{} does not match the sha256 recorded in the offline manifest",
                archive.display()
            )));
        }

        sink.send(DriverEvent::step(&entry.key, ProgressStep::Extract));
        let payload = self.extract_to_staging(&entry.key, archive)?;
        let kind = payload.kind;
        // 离线包不带注册表条目，就地造一个够用的版本信息。
        let info = DriverInfo {
            version: entry.version.clone(),
            label: String::new(),
            min_app_version: String::new(),
            jre: jre::DEFAULT_KEY.to_owned(),
            jar: None,
            native: Default::default(),
            external_driver_required: false,
        };

        if kind == ArtifactKind::Jar {
            let runtime = self.state().java_runtime;
            if jre::resolve_java(&self.base_dir, &runtime, jre::DEFAULT_KEY).is_none() {
                let _ = std::fs::remove_dir_all(payload.staging());
                return Err(DriverError::Unsupported(format!(
                    "no java runtime is available for the jar driver {:?}",
                    entry.key
                )));
            }
        }

        sink.send(DriverEvent::step(&entry.key, ProgressStep::Install));
        let installed = self.replace_driver_dir(&entry.key, &payload, kind, &info);
        let _ = std::fs::remove_dir_all(payload.staging());
        installed
    }

    /// 把离线包解到临时目录并读它的 manifest。
    fn read_offline(&self, package: &Path) -> Result<(PathBuf, OfflineManifest)> {
        let temp = self.staging_dir().join(format!("offline-{}", stamp()));
        let _ = std::fs::remove_dir_all(&temp);
        crate::archive::extract_tar_any(package, &temp)?;

        let manifest_path = temp.join(OFFLINE_MANIFEST);
        let bytes = std::fs::read(&manifest_path).map_err(|error| {
            DriverError::Format(format!(
                "{} is not a driver offline package: {error}",
                package.display()
            ))
        })?;
        let manifest: OfflineManifest = serde_json::from_slice(&bytes)
            .map_err(|error| DriverError::Format(format!("offline manifest: {error}")))?;
        if manifest.kind != OFFLINE_KIND {
            return Err(DriverError::Format(format!(
                "unexpected offline package kind {:?}",
                manifest.kind
            )));
        }
        Ok((temp, manifest))
    }

    /// 改状态并落盘（所有状态变更的唯一出口）。
    fn mutate_state<T>(&self, mutate: impl FnOnce(&mut DriverState) -> T) -> Result<T> {
        let mut state = self.state.lock().map_err(|_| lock_error())?;
        let result = mutate(&mut state);
        state.save(&self.base_dir)?;
        Ok(result)
    }
}

/// staging 里的载荷（连带它的中转目录，装完要清）。
struct StagedPayload {
    path: PathBuf,
    kind: ArtifactKind,
    staging: PathBuf,
}

impl StagedPayload {
    fn staging(&self) -> &Path {
        &self.staging
    }

    /// 包内元数据在解包目录根上（载荷在 `drivers/` 子目录里）。
    fn parent_for_manifest(&self) -> &Path {
        &self.staging
    }
}

/// jar 轨的 JVM 参数。
///
/// 前半段照抄 dbx 的既有实践（`agent_driver.rs` 的 `agent_java_args_with_extra_opts`）：
///
/// - `-XX:+IgnoreUnrecognizedVMOptions`：老 JVM 不认识 `--add-opens` 时**忽略而不是退出**
///   —— 否则用户看到的是一句 `Unrecognized option`，而真正的问题（JRE 太低）被掩盖。
/// - 三个 `-D*.encoding=UTF-8`：中文库名 / 表名不乱码。
/// - `-Djava.net.useSystemProxies=false` 与三条空 proxy：**数据库连接不走 HTTP 代理**
///   （JVM 默认会读系统代理，企业网里会让直连的 1521 / 3306 也去绕代理）。
/// - `--add-opens=java.sql/java.sql`：JDBC 驱动反射进 `java.sql` 是常态。
/// - `-XX:TieredStopAtLevel=1 -XX:+UseSerialGC`：驱动进程是短命的客户端，
///   要的是**启动快**，不是吞吐。
fn jar_launch_args(key: &str, jar: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "-XX:+IgnoreUnrecognizedVMOptions",
        "-Dfile.encoding=UTF-8",
        "-Dsun.stdout.encoding=UTF-8",
        "-Dsun.stderr.encoding=UTF-8",
        "-Djava.net.useSystemProxies=false",
        "-Dhttp.proxyHost=",
        "-Dhttps.proxyHost=",
        "-DsocksProxyHost=",
        "-Doracle.net.disableOob=true",
        "-Doracle.jdbc.javaNetNio=false",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();

    // 达梦驱动在主机名带 `%`（多网卡 / IPv6 zone）时反射 `NetworkInterface.index`。
    if key == "dameng" {
        args.push("--add-opens=java.base/java.net=ALL-UNNAMED".to_owned());
    }
    // Ignite 的 GridUnsafe / BinaryContext 反射 JDK 内部；不开这些每次连接都
    // ExceptionInInitializerError。
    if key == "ignite" || key == "ignite3" {
        args.extend(
            [
                "--add-opens=java.base/jdk.internal.misc=ALL-UNNAMED",
                "--add-opens=java.base/java.nio=ALL-UNNAMED",
                "--add-opens=java.base/sun.nio.ch=ALL-UNNAMED",
                "--add-opens=java.base/java.util=ALL-UNNAMED",
                "--add-opens=java.base/java.lang=ALL-UNNAMED",
                "--add-opens=java.base/java.lang.reflect=ALL-UNNAMED",
            ]
            .into_iter()
            .map(str::to_owned),
        );
    }
    // Informix 的 JDBC 驱动在双栈环境下会挑错协议族。
    if key == "informix" {
        args.push("-Djava.net.preferIPv4Stack=true".to_owned());
    }

    args.push("--add-opens=java.sql/java.sql=ALL-UNNAMED".to_owned());
    args.extend(
        ["-XX:TieredStopAtLevel=1", "-XX:+UseSerialGC", "-jar"]
            .into_iter()
            .map(str::to_owned),
    );
    args.push(jar.to_string_lossy().into_owned());
    args
}

/// 离线包的类型标识。
const OFFLINE_KIND: &str = "duck-driver-offline";
/// 离线包格式版本。
const OFFLINE_VERSION: u32 = 1;
/// 离线包里 manifest 的文件名。
const OFFLINE_MANIFEST: &str = "manifest.json";

/// 离线包的 manifest。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OfflineManifest {
    kind: String,
    version: u32,
    exported_at: u64,
    drivers: Vec<OfflineEntry>,
}

/// 离线包里的一条驱动。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OfflineEntry {
    key: String,
    version: String,
    package: String,
    sha256: String,
    size: u64,
    #[serde(default)]
    format: ArtifactFormat,
}

/// 往 tar 里追加一段内存字节。
fn append_bytes(builder: &mut tar::Builder<std::fs::File>, name: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, name, bytes)
        .map_err(|error| DriverError::Io(format!("{name}: {error}")))
}

/// 目录字节数（算不出来算 0）。
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                dir_size(&path)
            } else {
                entry.metadata().map(|meta| meta.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// 给原生载荷加可执行位。
fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(path)
            .map_err(|error| DriverError::Io(format!("{}: {error}", path.display())))?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)
            .map_err(|error| DriverError::Io(format!("{}: {error}", path.display())))?;
    }
    let _ = path;
    Ok(())
}

/// 时间戳（秒 + 毫秒 + 进程号）：staging / 让位目录名要唯一。
fn stamp() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    format!("{millis}-{}", std::process::id())
}

/// Unix 秒（用于 `installed_at` 与 `exported_at`）。
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn lock_error() -> DriverError {
    DriverError::Io("the driver store lock was poisoned".to_owned())
}

/// 给事件补上批量的第几个 / 共几个。
trait BatchEvents {
    /// 生成一条带批量位置的事件（这里只需要位置，步骤由调用方给）。
    fn at_count(&self, index: u32, count: u32) -> EventSink;
}

impl BatchEvents for EventSink {
    fn at_count(&self, index: u32, count: u32) -> EventSink {
        // 位置信息通过一个「装饰过的」sink 传递：包装一层，把每条事件都打上位置。
        // 用 `SinkAt` 而不是在每条事件上手动 `.at(..)`，是因为批量路径里事件是在
        // 被调用方内部生成的（下载进度等），调用方拿不到它们。
        self.decorate(move |event| event.at(index, count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::STATE_FILE;

    fn temp_base(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-drivers-store-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn cancel_flag_is_shared_between_clones() {
        let flag = CancelFlag::new();
        let clone = flag.clone();
        assert!(!clone.is_cancelled());
        flag.cancel();
        assert!(clone.is_cancelled());
        assert!(matches!(clone.check(), Err(DriverError::Cancelled)));
        assert_eq!(clone.flow(), crate::Flow::Abort);
    }

    /// 未安装时给「未安装」，不是空 panic。
    #[test]
    fn launch_spec_requires_an_installed_driver() {
        let store = DriverStore::new_at(temp_base("launch-missing"));
        let error = store.launch_spec("dameng").expect_err("没装就该报错");
        assert!(matches!(error, DriverError::Unsupported(_)), "{error:?}");
        let _ = std::fs::remove_dir_all(store.base_dir());
    }

    /// jar 轨的 JVM 参数：通用项都在，达梦 / Informix / Ignite 的专属项按需出现。
    #[test]
    fn jar_launch_args_carry_the_known_workarounds() {
        let args = jar_launch_args("dameng", Path::new("/x/agent.jar"));
        assert!(args.contains(&"-XX:+IgnoreUnrecognizedVMOptions".to_owned()));
        assert!(args.contains(&"--add-opens=java.base/java.net=ALL-UNNAMED".to_owned()));
        assert!(args.contains(&"--add-opens=java.sql/java.sql=ALL-UNNAMED".to_owned()));
        assert_eq!(args.last().map(String::as_str), Some("/x/agent.jar"));
        assert!(args.contains(&"-jar".to_owned()));

        let informix = jar_launch_args("informix", Path::new("/x/agent.jar"));
        assert!(informix.contains(&"-Djava.net.preferIPv4Stack=true".to_owned()));
        assert!(!informix.contains(&"--add-opens=java.base/java.net=ALL-UNNAMED".to_owned()));

        let ignite = jar_launch_args("ignite", Path::new("/x/agent.jar"));
        assert!(ignite.contains(&"--add-opens=java.base/jdk.internal.misc=ALL-UNNAMED".to_owned()));
    }

    /// 卸载不碰 JRE，也不动别的驱动。
    #[test]
    fn uninstall_only_removes_its_own_directory() {
        let store = DriverStore::new_at(temp_base("uninstall"));
        let sink = EventSink::silent();
        std::fs::create_dir_all(store.driver_dir("dameng")).expect("建目录");
        std::fs::create_dir_all(store.driver_dir("oracle")).expect("建目录");
        store
            .mutate_state(|state| {
                state.drivers.insert(
                    "dameng".to_owned(),
                    InstalledDriver {
                        key: "dameng".to_owned(),
                        version: "1".to_owned(),
                        installed_at: 0,
                        kind: ArtifactKind::Jar,
                        jre: Some("21".to_owned()),
                        bytes: 1,
                    },
                );
            })
            .expect("写状态");

        store.uninstall("dameng", &sink).expect("卸载");
        assert!(!store.driver_dir("dameng").exists());
        assert!(store.driver_dir("oracle").exists());
        assert!(store.installed("dameng").is_none());
        let _ = std::fs::remove_dir_all(store.base_dir());
    }

    /// 摘要 / 体积不符必须删掉缓存里的那份（不能让下一次安装「复用」坏文件）。
    #[test]
    fn a_bad_digest_drops_the_cached_artifact() {
        let store = DriverStore::new_at(temp_base("bad-digest"));
        std::fs::create_dir_all(store.cache_dir()).expect("建缓存目录");
        let path = store.cache_dir().join("x.tar.zst");
        std::fs::write(&path, b"hello").expect("写文件");

        let info = crate::registry::ArtifactInfo {
            url: "https://example.com/x.tar.zst".to_owned(),
            sha256: Some("00".repeat(32)),
            size: 5,
            format: Some(ArtifactFormat::TarZstd),
            delta: None,
        };
        let error = store
            .verify_artifact(&path, &info, "00")
            .expect_err("摘要不符必须报错");
        assert!(matches!(error, DriverError::Integrity(_)), "{error:?}");
        assert!(!path.exists(), "坏工件必须被删掉");
        let _ = std::fs::remove_dir_all(store.base_dir());
    }

    /// 体积不符也要拦（注册表给的 size 是第二道闸）。
    #[test]
    fn a_size_mismatch_is_an_integrity_error() {
        let store = DriverStore::new_at(temp_base("bad-size"));
        std::fs::create_dir_all(store.cache_dir()).expect("建缓存目录");
        let path = store.cache_dir().join("x.tar.zst");
        std::fs::write(&path, b"hello").expect("写文件");
        let info = crate::registry::ArtifactInfo {
            url: "https://example.com/x.tar.zst".to_owned(),
            sha256: None,
            size: 999,
            format: Some(ArtifactFormat::TarZstd),
            delta: None,
        };
        let error = store
            .verify_artifact(&path, &info, "whatever")
            .expect_err("体积不符必须报错");
        assert!(matches!(error, DriverError::Integrity(_)), "{error:?}");
        let _ = std::fs::remove_dir_all(store.base_dir());
    }

    /// 注册表里没有的键不猜：报错而不是装个空壳。
    #[test]
    fn installing_an_unknown_key_is_rejected() {
        let store = DriverStore::new_at(temp_base("unknown-key"));
        let registry = DriverRegistry::default();
        let error = store
            .install(&registry, "nope", &EventSink::silent(), &CancelFlag::new())
            .expect_err("注册表里没有就必须报错");
        assert!(matches!(error, DriverError::Format(_)), "{error:?}");
        let _ = std::fs::remove_dir_all(store.base_dir());
    }

    /// 统计：总数来自驱动目录，已装数只数认得出来的键。
    #[test]
    fn summary_counts_installed_and_updatable() {
        let store = DriverStore::new_at(temp_base("summary"));
        store
            .mutate_state(|state| {
                for key in ["dameng", "not-in-catalog"] {
                    state.drivers.insert(
                        key.to_owned(),
                        InstalledDriver {
                            key: key.to_owned(),
                            version: "0.0.1".to_owned(),
                            installed_at: 0,
                            kind: ArtifactKind::Jar,
                            jre: None,
                            bytes: 10,
                        },
                    );
                }
            })
            .expect("写状态");

        let summary = store.summary(None);
        assert_eq!(summary.total, crate::catalog::catalog().len());
        assert_eq!(summary.installed, 1, "不在驱动目录里的键不计入");
        assert_eq!(summary.bytes_installed, 20);
        assert_eq!(summary.updatable, 0, "没有注册表就不谈可更新");

        let registry = DriverRegistry::parse(
            br#"{"drivers":{"dameng":{"version":"0.0.2","jar":{"url":"https://e/a","size":1}}}}"#,
        )
        .expect("注册表");
        assert_eq!(store.summary(Some(&registry)).updatable, 1);
        let _ = std::fs::remove_dir_all(store.base_dir());
    }

    /// 状态文件与安装目录都在 base 下（换目录就能整体搬走）。
    #[test]
    fn everything_lives_under_the_base_dir() {
        let store = DriverStore::new_at(temp_base("layout"));
        assert_eq!(
            store.driver_dir("dameng"),
            store.base_dir().join("drivers/dameng")
        );
        assert_eq!(store.cache_dir(), store.base_dir().join("download-cache"));
        store
            .set_download_source(DownloadSource::Mirror)
            .expect("写盘");
        assert!(store.base_dir().join(STATE_FILE).is_file());
        assert_eq!(store.download_source(), DownloadSource::Mirror);
        let _ = std::fs::remove_dir_all(store.base_dir());
    }
}
