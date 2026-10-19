//! 从 `.class` 字节码 / 反编译源码里抽版本号。
//!
//! **逐字复刻**参考仓 `class_compare/version_extractor.rs`（167 行）。唯一改动：
//! `once_cell::sync::Lazy` → `std::sync::LazyLock`（本仓不引 `once_cell`，
//! MSRV 1.85 的标准库版语义完全一致），以及 `crate::decompiler` → `crate::classfile`。

// 复刻件的代码形状不改：`if let Some(..) { if cond { .. } }` 的正确写法要 Rust 1.88
// 的 let_chains（本仓 MSRV 1.85）；`captures_iter(..)` 后立刻 `return` 是源仓原样
// （语义等价于只取第一个匹配），改成 `.next()` 属于重写逻辑，不是移植该干的事。
#![allow(clippy::collapsible_if, clippy::never_loop)]

use regex::Regex;

use crate::classfile::{self, CpEntry};

static VERSION_PATTERN: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(\d+\.\d+\.\d+(?:\.\d+)?)").expect("valid regex"));

static HUNDSUN_PATTERN: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"@version\s+(\d+\.\d+\.\d+(?:\.\d+)?)").expect("valid regex"));

/// 版本号标准化：
/// - `X.Y.Z` → `X.Y.0.Z`（3段转4段，在第二段后插入0）
/// - `X.Y` → `X.Y.0.0`
/// - 其他格式保持不变
pub fn normalize(ver: &str) -> String {
    let parts: Vec<&str> = ver.split('.').collect();
    match parts.len() {
        2 => format!("{}.{}.0.0", parts[0], parts[1]),
        3 => format!("{}.{}.0.{}", parts[0], parts[1], parts[2]),
        _ => ver.to_string(),
    }
}

pub struct VersionExtractor;

impl VersionExtractor {
    pub fn extract(bytes: &[u8]) -> String {
        let ver = Self::extract_raw(bytes);
        if ver == "-1" { ver } else { normalize(&ver) }
    }

    pub fn extract_from_source(source: &str) -> String {
        let ver = Self::extract_raw_from_source(source);
        if ver == "-1" { ver } else { normalize(&ver) }
    }

    fn extract_raw(bytes: &[u8]) -> String {
        if let Some(ver) = Self::from_method(bytes, "getVersion") {
            if Self::is_valid(&ver) {
                return ver;
            }
        }
        if let Some(ver) = Self::from_method(bytes, "getversion") {
            if Self::is_valid(&ver) {
                return ver;
            }
        }

        if let Some(ver) = Self::from_hundsun_constant(bytes) {
            if Self::is_valid(&ver) {
                return ver;
            }
        }

        if let Some(ver) = Self::scan_constant_pool(bytes) {
            if Self::is_valid(&ver) {
                return ver;
            }
        }

        "-1".to_string()
    }

    /// 从反编译后的 Java 源码中提取版本号（原始版本，未标准化）
    fn extract_raw_from_source(source: &str) -> String {
        // 1. 查找 getVersion() 方法体中的 return 语句
        static GET_VERSION_BODY: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| {
                Regex::new(r"(?s)getVersion\s*\([^)]*\)\s*\{(.*?)\n\}")
            .expect("valid regex")
            });
        if let Some(caps) = GET_VERSION_BODY.captures(source) {
            let body = &caps[1];
            // 查找 return "xxx" 或 return "xxx";
            static RETURN_STRING: std::sync::LazyLock<Regex> =
                std::sync::LazyLock::new(|| {
                    Regex::new(r#"return\s+"([^"]*)"\s*;"#).expect("valid regex")
                });
            for cap in RETURN_STRING.captures_iter(body) {
                let val = &cap[1];
                if let Some(v) = VERSION_PATTERN.captures(val) {
                    return v[1].to_string();
                }
            }
        }

        // 2. 查找 HUNDSUN_VERSION 字段定义
        static HUNDSUN_FIELD: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| {
                Regex::new(r#"HUNDSUN_VERSION\s*=\s*"([^"]*)""#).expect("valid regex")
            });
        if let Some(caps) = HUNDSUN_FIELD.captures(source) {
            let val = &caps[1];
            if let Some(v) = HUNDSUN_PATTERN.captures(val) {
                return v[1].to_string();
            }
            if let Some(v) = VERSION_PATTERN.captures(val) {
                return v[1].to_string();
            }
        }

        // 3. 扫描源码中的 @version x.y.z 注释或字符串
        for cap in HUNDSUN_PATTERN.captures_iter(source) {
            return cap[1].to_string();
        }

        // 4. 扫描源码中的版本号字符串字面量
        static STRING_LITERAL: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| {
                Regex::new(r##""(\d+\.\d+\.\d+(?:\.\d+)?)""##).expect("valid regex")
            });
        if let Some(cap) = STRING_LITERAL.captures(source) {
            return cap[1].to_string();
        }

        "-1".to_string()
    }

    fn from_method(bytes: &[u8], method_name: &str) -> Option<String> {
        let class = classfile::parse_class_file(bytes).ok()?;
        let has_method = class.methods.iter().any(|m| m.name == method_name);
        if !has_method {
            return None;
        }
        for entry in &class.constant_pool.entries {
            if let CpEntry::Utf8(s) = entry {
                if let Some(cap) = VERSION_PATTERN.captures(s) {
                    return Some(cap[1].to_string());
                }
            }
        }
        None
    }

    fn from_hundsun_constant(bytes: &[u8]) -> Option<String> {
        let class = classfile::parse_class_file(bytes).ok()?;
        for entry in &class.constant_pool.entries {
            if let CpEntry::Utf8(s) = entry {
                if s.contains("HUNDSUN_VERSION") {
                    if let Some(cap) = HUNDSUN_PATTERN.captures(s) {
                        return Some(cap[1].to_string());
                    }
                    if let Some(cap) = VERSION_PATTERN.captures(s) {
                        return Some(cap[1].to_string());
                    }
                }
            }
        }
        None
    }

    fn scan_constant_pool(bytes: &[u8]) -> Option<String> {
        let class = classfile::parse_class_file(bytes).ok()?;
        for entry in &class.constant_pool.entries {
            if let CpEntry::Utf8(s) = entry {
                if let Some(cap) = VERSION_PATTERN.captures(s) {
                    return Some(cap[1].to_string());
                }
            }
        }
        None
    }

    fn is_valid(ver: &str) -> bool {
        ver != "-1" && VERSION_PATTERN.is_match(ver)
    }
}
