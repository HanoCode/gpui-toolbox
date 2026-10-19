//! JAR 扫描：走目录找 `.jar`，打开 ZIP 挨个读 `.class` 抽版本。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/jar_compare/scanner.rs`。
//! 改动只有一处：`scan_jars` 里的 `par_iter()` 换成普通迭代（理由与 class 比对
//! 那份相同，见 [`super::super::class_compare::scanner`]）。
//!
//! 两个「源仓口径」值得记下来，因为它们决定结果对不对：
//!
//! 1. **JAR 版本 = 内部 class 版本里出现次数最多的那个**（`BTreeMap` + 计数取最大，
//!    平票时取版本串序更小的）。生产包里绝大多数类是同一个 JDK 编译的，众数就是它；
//!    个别类用别的版本编（或 `getVersion()` 缺失记作 `-1`）不影响结论。
//! 2. **服务名从相对路径推**：`lib/…` 归 `lib`，`server/<svc>/…` 归 `<svc>`。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use duck_classfile::VersionExtractor;
use walkdir::WalkDir;
use zip::ZipArchive;

use super::models::{JarClassInfo, JarInfo};

/// Phase 1 的产物：只含元数据，ZIP 还没打开。
#[derive(Debug, Clone)]
pub struct JarEntry {
    /// 等于 `jar_rel_path`（配对键）。
    pub key: String,
    pub env_name: String,
    pub jar_name: String,
    pub jar_path: PathBuf,
    pub jar_rel_path: String,
    pub server_name: String,
    pub file_size: u64,
}

pub struct JarScanner {
    pub env_name: String,
    pub app_dir: PathBuf,
    /// 只扫命中这个模式的目录（`""` = 全扫；`a+b` = 任一命中）。
    pub service_pattern: String,
    pub jar_exclude_patterns: Vec<String>,
}

impl JarScanner {
    pub fn new(
        env_name: String,
        app_dir: PathBuf,
        service_pattern: String,
        jar_exclude_patterns: Vec<String>,
    ) -> Self {
        Self {
            env_name,
            app_dir,
            service_pattern,
            jar_exclude_patterns,
        }
    }

    /// Phase 1：列出所有 JAR 的元数据，**不打开 ZIP**。
    pub fn list_jars(&self) -> Vec<JarEntry> {
        WalkDir::new(&self.app_dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension() == Some("jar".as_ref()))
            .filter(|entry| !self.should_exclude_jar(entry.file_name().to_string_lossy().as_ref()))
            .filter(|entry| self.is_in_service_dir(entry.path()))
            .map(|entry| {
                let path = entry.path().to_path_buf();
                let jar_rel_path = self.rel_path(&path);
                let server_name = Self::extract_server_name(&jar_rel_path);
                JarEntry {
                    key: jar_rel_path.clone(),
                    env_name: self.env_name.clone(),
                    jar_name: path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                    jar_path: path,
                    jar_rel_path,
                    server_name,
                    file_size: entry.metadata().map(|meta| meta.len()).unwrap_or(0),
                }
            })
            .collect()
    }

    /// Phase 2：逐个打开 ZIP 读 class。
    pub fn scan_jars(&self, entries: &[JarEntry]) -> Vec<JarInfo> {
        entries
            .iter()
            .filter_map(|entry| self.scan_single_jar(&entry.jar_path))
            .collect()
    }

    /// 全量扫描。
    pub fn scan(&self) -> Vec<JarInfo> {
        let entries = self.list_jars();
        self.scan_jars(&entries)
    }

    fn rel_path(&self, jar_path: &Path) -> String {
        jar_path
            .strip_prefix(&self.app_dir)
            .unwrap_or(jar_path)
            .to_string_lossy()
            .trim_start_matches('/')
            .trim_start_matches('\\')
            .to_string()
    }

    fn scan_single_jar(&self, jar_path: &Path) -> Option<JarInfo> {
        let jar_name = jar_path.file_name()?.to_string_lossy().to_string();

        let file = std::fs::File::open(jar_path).ok()?;
        let metadata = file.metadata().ok()?;
        let mut archive = ZipArchive::new(file).ok()?;

        let mut classes: Vec<JarClassInfo> = Vec::new();
        for index in 0..archive.len() {
            let Ok(mut entry) = archive.by_index(index) else {
                continue;
            };
            let name = entry.name().to_string();
            if !name.ends_with(".class") {
                continue;
            }
            let mut bytes = Vec::new();
            if entry.read_to_end(&mut bytes).is_err() {
                continue;
            }
            let version = VersionExtractor::extract(&bytes);
            let class_path_no_suffix = name
                .replace('/', ".")
                .trim_end_matches(".class")
                .to_string();
            let class_name = class_path_no_suffix
                .split('.')
                .next_back()
                .unwrap_or("")
                .to_string();
            let package_name = if class_path_no_suffix.contains('.') {
                class_path_no_suffix
                    .rsplitn(2, '.')
                    .last()
                    .unwrap_or("")
                    .to_string()
            } else {
                String::new()
            };
            classes.push(JarClassInfo {
                class_path: class_path_no_suffix,
                version,
                class_name,
                package_name,
            });
        }

        // 一个 class 都没有的「jar」多半是空壳或损坏，不进结果。
        if classes.is_empty() {
            return None;
        }

        let jar_version = Self::calculate_jar_version(&classes);
        let jar_rel_path = self.rel_path(jar_path);
        let server_name = Self::extract_server_name(&jar_rel_path);

        Some(JarInfo {
            env_name: self.env_name.clone(),
            jar_name,
            jar_path: jar_path.to_string_lossy().to_string(),
            jar_rel_path,
            server_name,
            file_size: metadata.len(),
            classes,
            jar_version,
        })
    }

    fn extract_server_name(jar_rel_path: &str) -> String {
        let first_component = jar_rel_path.split('/').next().unwrap_or("lib");
        if first_component == "lib" {
            first_component.to_string()
        } else if first_component == "server" {
            jar_rel_path
                .split('/')
                .nth(1)
                .unwrap_or("unknown")
                .to_string()
        } else {
            first_component.to_string()
        }
    }

    /// 众数版本。`BTreeMap` 保证遍历有序；平票时**版本串更大的那个**胜出
    /// （`then_with(|| va.cmp(vb))` 走 `max_by`）。
    ///
    /// ⚠️ 源仓那行注释写的是「on count tie, lower version string wins」，代码实际是
    /// **高**者胜。复刻以代码为准（注释与行为不一致是源仓的笔误），单测钉住实际行为。
    fn calculate_jar_version(classes: &[JarClassInfo]) -> String {
        let mut version_count: BTreeMap<String, usize> = BTreeMap::new();
        for class in classes {
            if class.version != "-1" {
                *version_count.entry(class.version.clone()).or_insert(0) += 1;
            }
        }
        version_count
            .into_iter()
            .max_by(|(va, ca), (vb, cb)| ca.cmp(cb).then_with(|| va.cmp(vb)))
            .map(|(version, _)| version)
            .unwrap_or_else(|| "-1".to_string())
    }

    /// 路径是否落在要扫的服务目录里。
    ///
    /// 三种写法都要认（源仓口径）：空模式 = 全扫；`lcpt+applib` = 任一命中；
    /// 含 `/` 的按子串匹配，不含的按「某个路径段以它开头」匹配。
    fn is_in_service_dir(&self, path: &Path) -> bool {
        let pattern = self.service_pattern.trim_end_matches('*');
        if pattern.is_empty() {
            return true;
        }
        if pattern.contains('+') {
            return pattern.split('+').any(|part| {
                if part.contains('/') {
                    path.to_string_lossy().contains(part)
                } else {
                    path.components()
                        .any(|component| component.as_os_str().to_string_lossy().starts_with(part))
                }
            });
        }
        if pattern.contains('/') || pattern.contains('\\') {
            return path.to_string_lossy().contains(pattern);
        }
        path.components()
            .any(|component| component.as_os_str().to_string_lossy().starts_with(pattern))
    }

    fn should_exclude_jar(&self, jar_name: &str) -> bool {
        self.jar_exclude_patterns
            .iter()
            .any(|pattern| glob_match(pattern, jar_name))
    }
}

/// 与 class 比对那份同名函数一致（源仓也是各写一份）。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{fake_class, write_zip};

    fn class_info(version: &str) -> JarClassInfo {
        JarClassInfo {
            class_path: "com.demo.A".to_string(),
            version: version.to_string(),
            class_name: "A".to_string(),
            package_name: "com.demo".to_string(),
        }
    }

    /// JAR 版本取内部 class 版本的**众数**；`-1`（抽不到）不参与投票。
    #[test]
    fn jar_version_is_the_mode_of_inner_classes() {
        let classes = vec![
            class_info("1.2.0.4"),
            class_info("1.2.0.4"),
            class_info("1.2.0.3"),
            class_info("-1"),
        ];
        assert_eq!(JarScanner::calculate_jar_version(&classes), "1.2.0.4");

        // 平票时取版本串**更大**的那个（源代码如此，见 `calculate_jar_version` 的文档）。
        let tie = vec![class_info("1.2.0.4"), class_info("1.2.0.3")];
        assert_eq!(JarScanner::calculate_jar_version(&tie), "1.2.0.4");

        // 全是 -1 = 没有可用版本，源仓也返回 -1。
        assert_eq!(JarScanner::calculate_jar_version(&[class_info("-1")]), "-1");
    }

    /// 服务名从相对路径推：`lib/…` → `lib`，`server/<svc>/…` → `<svc>`。
    #[test]
    fn server_name_comes_from_the_path() {
        assert_eq!(JarScanner::extract_server_name("lib/foo.jar"), "lib");
        assert_eq!(
            JarScanner::extract_server_name("server/lcpt-metal-api/lib/foo.jar"),
            "lcpt-metal-api"
        );
        assert_eq!(JarScanner::extract_server_name("applib/foo.jar"), "applib");
        // `server` 后面没有路径段时兜底成 unknown（源仓口径：`nth(1).unwrap_or("unknown")`）。
        assert_eq!(JarScanner::extract_server_name("server"), "unknown");
    }

    /// 三种模式写法：空 = 全扫；`a+b` = 任一命中；含 `/` 按子串。
    #[test]
    fn service_pattern_accepts_all_three_forms() {
        let scanner = |pattern: &str| {
            JarScanner::new(
                "env".to_string(),
                PathBuf::from("/tmp"),
                pattern.to_string(),
                Vec::new(),
            )
        };
        let path = Path::new("/data/lcpt-metal-api/lib/foo.jar");

        assert!(scanner("").is_in_service_dir(path));
        assert!(scanner("lcpt*").is_in_service_dir(path));
        assert!(scanner("lcpt+applib").is_in_service_dir(path));
        assert!(scanner("lcpt-metal-api").is_in_service_dir(path));
        assert!(scanner("lcpt-metal-api/lib").is_in_service_dir(path));
        assert!(!scanner("applib").is_in_service_dir(path));
        assert!(!scanner("other+applib").is_in_service_dir(path));
    }

    /// Phase 1 只列路径：不打开 ZIP 也能拿到清单（空文件也列得出来）。
    #[test]
    fn lists_jars_without_opening_them() {
        let root = std::env::temp_dir().join("duck-jar-scan-list");
        let _ = std::fs::remove_dir_all(&root);
        for rel in ["lib/foo.jar", "lib/skip-me.jar", "lcpt-a/lib/bar.jar"] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(&path, b"not a zip").expect("write");
        }

        let scanner = JarScanner::new(
            "env".to_string(),
            root.clone(),
            String::new(),
            vec!["skip-*".to_string()],
        );
        let entries = scanner.list_jars();
        let mut rel_paths: Vec<&str> = entries.iter().map(|e| e.jar_rel_path.as_str()).collect();
        rel_paths.sort_unstable();

        assert_eq!(rel_paths, vec!["lcpt-a/lib/bar.jar", "lib/foo.jar"]);
        assert_eq!(entries[0].key, entries[0].jar_rel_path);
        let _ = std::fs::remove_dir_all(root);
    }

    /// 真 ZIP 走一遍：开包读 class、抽版本、按相对路径配对。
    #[test]
    fn scans_a_real_jar_and_matches_by_relative_path() {
        let root = std::env::temp_dir().join("duck-jar-scan-real");
        let _ = std::fs::remove_dir_all(&root);
        let jar = root.join("lib/foo.jar");
        std::fs::create_dir_all(jar.parent().expect("parent")).expect("dirs");

        let class = fake_class("1.2.4");
        let manifest = b"Manifest-Version: 1.0\n";
        write_zip(
            &jar,
            &[
                ("com/demo/A.class", &class),
                ("com/demo/B.class", &class),
                ("META-INF/MANIFEST.MF", manifest),
            ],
        );

        let scanner = JarScanner::new("env".to_string(), root.clone(), String::new(), Vec::new());
        let entries = scanner.list_jars();
        assert_eq!(entries.len(), 1);
        let jars = scanner.scan_jars(&entries);

        assert_eq!(jars.len(), 1);
        assert_eq!(jars[0].jar_rel_path, "lib/foo.jar");
        assert_eq!(jars[0].server_name, "lib");
        assert_eq!(jars[0].classes.len(), 2);
        assert_eq!(jars[0].jar_version, "1.2.0.4");
        let _ = std::fs::remove_dir_all(root);
    }
}
