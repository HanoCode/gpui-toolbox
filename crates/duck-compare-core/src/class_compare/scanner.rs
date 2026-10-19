//! 应用目录扫描：找到「服务目录 → `WEB-INF/classes` 里的 `.class`」。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/class_compare/scanner.rs`，
//! 三处与宿主相关的改动：
//!
//! 1. `rayon::par_iter()` → 普通迭代。源仓并行只是为了让「扫几百个服务目录」快一点，
//!    结果顺序在两边都按收集顺序确定；本仓的插件进程没必要为一次目录遍历多引一个
//!    线程池依赖（`rayon` 会带进 crossbeam 一整串）。IO 密集，串行足够。
//! 2. `ClassEntry` 提到 [`super::models`]（源仓定义在本文件里）——它要序列化成
//!    进度/日志载荷，和其余数据结构放一起更顺。
//! 3. 无 tauri、无 emit：进度回调由调用方传入（见 [`super::scan_and_compare`]）。
//!
//! 目录假设（源仓口径，不猜）：`app_dir` 下 depth ≤ 2 的某个目录就是服务目录，
//! 类在它下面的 `WEB-INF/classes`，包在 `WEB-INF/lib`。

use std::path::{Path, PathBuf};

use duck_classfile::VersionExtractor;
use walkdir::WalkDir;

use super::models::{ClassEntry, ClassInfo};

/// 扫一侧环境。
pub struct Scanner {
    pub env_name: String,
    pub app_dir: PathBuf,
    /// 服务目录名前缀（`""` = 全部）。源仓 UI 上叫「服务前缀」，`"*"` 会先被修剪掉。
    pub service_pattern: String,
    /// 是否连 `WEB-INF/lib` 下的 JAR 一起比。
    pub compare_lib: bool,
    /// 不参与比对的 JAR 名，支持 `*` / `?` 通配，也支持「裸串包含」。
    pub jar_exclude_patterns: Vec<String>,
}

impl Scanner {
    pub fn new(
        env_name: String,
        app_dir: PathBuf,
        service_pattern: String,
        compare_lib: bool,
        jar_exclude_patterns: Vec<String>,
    ) -> Self {
        Self {
            env_name,
            app_dir,
            service_pattern,
            compare_lib,
            jar_exclude_patterns,
        }
    }

    /// Phase 1：只列清单，**不读任何文件内容**。
    ///
    /// 先把两边的键取出来求交集，Phase 2 只对交集做版本提取——大环境里两边的类
    /// 可能有十几万，全部读一遍字节码要几分钟，而真正要比的往往只有几千个。
    pub fn list_paths(&self) -> Vec<ClassEntry> {
        let service_dirs: Vec<walkdir::DirEntry> = WalkDir::new(&self.app_dir)
            .max_depth(2)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| self.is_service_dir(entry))
            .collect();

        let mut results = Vec::new();
        for entry in &service_dirs {
            let service_name = entry.file_name().to_string_lossy().to_string();
            let class_dir = entry.path().join("WEB-INF/classes");
            if class_dir.exists() {
                results.extend(self.list_class_paths(&class_dir, &service_name));
            }
            if self.compare_lib {
                let lib_dir = entry.path().join("WEB-INF/lib");
                if lib_dir.exists() {
                    results.extend(self.list_lib_paths(&lib_dir, &service_name));
                }
            }
        }
        results
    }

    fn list_class_paths(&self, dir: &Path, service_name: &str) -> Vec<ClassEntry> {
        WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension() == Some("class".as_ref()))
            .map(|entry| {
                let path = entry.path().to_path_buf();
                let relative = path.strip_prefix(dir).unwrap_or(&path);
                let class_path = relative
                    .to_string_lossy()
                    .replace(".class", "")
                    .replace(std::path::MAIN_SEPARATOR, ".");
                let class_name = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let package_name = Self::extract_package(&class_path);
                ClassEntry {
                    key: format!("{service_name}#{package_name}#{class_name}"),
                    env_name: self.env_name.clone(),
                    server_name: service_name.to_string(),
                    class_name,
                    package_name,
                    class_path,
                    file_path: path.to_string_lossy().to_string(),
                    is_jar_level: false,
                }
            })
            .collect()
    }

    /// `WEB-INF/lib` 只看 depth 1 的 `.jar`：更深的目录里躺着的是解压残留，不是依赖。
    fn list_lib_paths(&self, dir: &Path, service_name: &str) -> Vec<ClassEntry> {
        WalkDir::new(dir)
            .max_depth(1)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension() == Some("jar".as_ref()))
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| {
                !self.should_exclude_jar(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .as_ref(),
                )
            })
            .map(|jar_path| {
                let jar_name = jar_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let class_name = jar_name.trim_end_matches(".jar").to_string();
                let file_path = jar_path.to_string_lossy().to_string();
                let class_path = format!("LIB/{class_name}");
                ClassEntry {
                    key: format!("{service_name}#LIB#{class_name}"),
                    env_name: self.env_name.clone(),
                    server_name: service_name.to_string(),
                    class_name: class_name.clone(),
                    package_name: "LIB".to_string(),
                    class_path,
                    file_path,
                    is_jar_level: true,
                }
            })
            .collect()
    }

    /// Phase 2：只给**指定条目**抽版本。
    pub fn scan_entries(&self, entries: &[ClassEntry]) -> Vec<ClassInfo> {
        let mut results = Vec::with_capacity(entries.len());

        for entry in entries {
            let version = if entry.is_jar_level {
                // JAR 级条目没有单一版本号，用「大小 + 修改时间」当指纹：
                // 换包必然改其一，够判「变没变」。
                std::fs::metadata(&entry.file_path)
                    .ok()
                    .map(|metadata| {
                        let size = metadata.len();
                        let mtime = metadata
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|duration| duration.as_secs())
                            .unwrap_or(0);
                        format!("jar:{size}:{mtime}")
                    })
                    .unwrap_or_default()
            } else {
                // 读不出来的 `.class` 直接跳过（源仓口径）：它连版本都抽不到，
                // 进结果表只会变成一条无从解释的空行。
                let Ok(bytes) = std::fs::read(&entry.file_path) else {
                    continue;
                };
                VersionExtractor::extract(&bytes)
            };
            results.push(ClassInfo {
                env_name: entry.env_name.clone(),
                server_name: entry.server_name.clone(),
                class_name: entry.class_name.clone(),
                package_name: entry.package_name.clone(),
                class_path: entry.class_path.clone(),
                file_path: entry.file_path.clone(),
                version,
            });
        }

        results
    }

    /// 全量扫描（清单 + 版本一起）。
    pub fn scan(&self) -> Vec<ClassInfo> {
        let entries = self.list_paths();
        self.scan_entries(&entries)
    }

    fn is_service_dir(&self, entry: &walkdir::DirEntry) -> bool {
        entry.file_type().is_dir()
            && self.is_pattern_match(entry.file_name().to_string_lossy().as_ref())
    }

    fn is_pattern_match(&self, name: &str) -> bool {
        let pattern = self.service_pattern.trim_end_matches('*');
        name.starts_with(pattern)
    }

    fn should_exclude_jar(&self, jar_name: &str) -> bool {
        self.jar_exclude_patterns
            .iter()
            .any(|pattern| glob_match(pattern, jar_name))
    }

    fn extract_package(class_path: &str) -> String {
        match class_path.rfind('.') {
            Some(last_dot) => class_path[..last_dot].to_string(),
            None => String::new(),
        }
    }
}

/// 通配匹配（源仓口径）：没有 `*` / `?` 时按「相等或包含」算，有就按正则。
fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        return name == pattern || name.contains(pattern);
    }
    let escaped = regex::escape(pattern);
    let regex_str = format!("^{}$", escaped.replace(r"\*", ".*").replace(r"\?", "."));
    regex::Regex::new(&regex_str)
        .map(|regex| regex.is_match(name))
        .unwrap_or(false)
}
