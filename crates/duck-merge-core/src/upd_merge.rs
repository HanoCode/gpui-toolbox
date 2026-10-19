//! 需求单 `.upd` 说明文件的追加合并。
//!
//! **本文件逐字复刻**自参考仓 `ifm_ai_tools/src-tauri/src/upd_merge.rs`
//! （552 行，含 7 条上游单测）。与 Tauri 零耦合：只把 `crate::merge_common`
//! 的调用点原样保留（本 crate 内同名模块），逻辑未动。
//!
//! 需求单各包通常会把数据库变更说明放在 `reqsql/<用户>/<需求单号>.upd` 下，
//! 本模块把这些分散的 `.upd` 按**需求单包处理顺序**依次追加到单个
//! `merge_upd.xml`，每段正文前后用 XML 风格注释标注来源路径，便于人工核对
//! 与按序执行。
//!
//! 三条硬约束：
//! 1. 源文件编码可能是 UTF-8（含/不含 BOM）、UTF-16 或 GBK，统一解码后以
//!    UTF-8 写出；
//! 2. 每段正文前标注 `<!-- <路径> beg -->`、后标注 `<!-- <路径> end -->`；
//! 3. 只追加，不改写正文，也绝不修改源文件。
//!
//! 注意：输出使用 XML 注释作为**可视分隔符**，但整体不保证是合法 XML ——
//! `.upd` 正文是自由文本（可能自带 XML 片段甚至 XML 注释），因此不能按
//! XML 文档解析。这一点是刻意为之，因为约束 3 要求正文原样保留。
//!
//! 同名（同一相对路径）的 `.upd` 跨包只保留**修改日期最新**的一份：同一需求单
//! 号被重复投递时不会重复追加两段内容。
//!
//! 生命周期：条目正文在**收集阶段**（`collect_upd_entries`）即读入内存
//! （`UpdEntry::content`），合并阶段**不再回读源文件**。因此本模块不受
//! 「源文件所在目录被清理或搬迁」的影响 —— 需求单合并的临时解压目录 `unzip/`
//! 被清理、SP 合并里 `database/` 之类的目录被服务合并阶段搬进 `app/`，
//! 都不会再让 UPD 合并报「源文件不存在」。
//!
//! `UpdEntry::src` 仅保留作来源追溯与诊断（日志/报错里指认是哪个包、哪个路径），
//! 不再作为读取来源；任何新增调用方都**不要**用 `src` 去读盘。

// clippy 豁免逐条列名、**只为本文件服务**：这些都是「改写法才消失」的建议，而改
// 写法正是移植要避免的成本（`collapsible_if` 的正确写法要 Rust 1.88 的 let_chains，
// 本仓 MSRV 是 1.85）。
#![allow(clippy::collapsible_if)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use encoding_rs::GBK;
use serde::Serialize;
use walkdir::WalkDir;

/// 合并源条目。加入顺序即为在输出文件中的先后顺序。
///
/// 正文（`content`）在收集阶段就从源文件读入，合并阶段只做拼接：
/// 这样条目的有效性不再依赖源文件在磁盘上「还活着」。
#[derive(Debug, Clone)]
pub struct UpdEntry {
    /// 注释中使用的显示路径，相对 `T*` 目录，如 `reqsql/lfyh/a.upd`
    pub display_path: String,
    /// 来源需求单包名（zip 文件名去扩展名）
    pub source_pkg: String,
    /// 源文件完整路径（**仅作来源追溯/诊断**，合并阶段不再读取）
    pub src: PathBuf,
    /// 源文件修改时间，用于同名条目的跨包去重
    pub mtime: Option<chrono::NaiveDateTime>,
    /// 收集阶段读入的**原始正文**（编码转换推迟到合并阶段，以便日志如实报出原编码）
    pub content: Vec<u8>,
}

/// 单个文件合并后的记录，用于日志与清单输出。
#[derive(Debug, Clone, Serialize)]
pub struct UpdMergeRecord {
    pub display_path: String,
    pub source_pkg: String,
    /// 源文件检测到的编码名，如 `UTF-8` / `GBK` / `UTF-16LE`
    pub encoding: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdMergeSummary {
    pub output_file: String,
    pub files: usize,
    pub total_bytes: u64,
    pub records: Vec<UpdMergeRecord>,
}

/// 收集 `T*` 目录下所有 `.upd` 文件并追加到 `out`。
///
/// 同一目录内按相对路径字典序排列，保证单包内顺序稳定；跨包顺序由调用方
/// 按包处理顺序依次调用决定。返回本次收集到的文件数。
///
/// **同名去重**：不同需求单包中出现**同一相对路径**的 `.upd`（同一用户、同一
/// 需求单号被重复投递/返工）时，只保留修改日期最新的一份，不再重复追加。
/// 日期相同或任一方缺失时保留先处理的那一份。
pub fn collect_upd_entries(
    t_dir: &Path,
    source_pkg: &str,
    out: &mut Vec<UpdEntry>,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();

    for entry in WalkDir::new(t_dir).follow_links(false).into_iter() {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let is_upd = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("upd"));
        if !is_upd {
            continue;
        }

        let rel = match entry.path().strip_prefix(t_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel = rel.to_string_lossy().replace('\\', "/");

        // 跳过 macOS 资源分叉、编辑器临时文件等隐藏项
        if rel.split('/').any(|seg| seg.starts_with('.')) {
            continue;
        }

        found.push((rel, entry.path().to_path_buf()));
    }

    found.sort_by(|a, b| a.0.cmp(&b.0));

    let count = found.len();
    for (display_path, src) in found {
        let mtime = crate::merge_common::file_mtime(&src);
        let existing = out.iter().position(|e| e.display_path == display_path);

        // 同名（同一相对路径）.upd 跨包只保留修改日期最新的一份；
        // 已登记的那份更新时，本条整条跳过 —— 连正文都不必读。
        if let Some(i) = existing {
            if !crate::merge_common::newer_mtime(mtime, out[i].mtime) {
                continue;
            }
        }

        // 正文在此刻读入内存（此刻源文件必然存在）；此后源文件被清理/搬走
        // 都不影响合并阶段。读取失败直接报错，不静默降级。
        let content = std::fs::read(&src)
            .map_err(|e| format!("读取 upd 文件失败 {}: {}", src.display(), e))?;

        match existing {
            Some(i) => {
                out[i].src = src;
                out[i].source_pkg = source_pkg.to_string();
                out[i].mtime = mtime;
                out[i].content = content;
            }
            None => out.push(UpdEntry {
                display_path,
                source_pkg: source_pkg.to_string(),
                src,
                mtime,
                content,
            }),
        }
    }

    Ok(count)
}

/// 解码 `.upd` 文本，返回 `(解码结果, 编码名)`。
///
/// 判定顺序：BOM（UTF-8 / UTF-16LE / UTF-16BE）→ 严格 UTF-8 → 回退 GBK。
///
/// 这里刻意**不做**“字节模式猜测是否是 UTF-8”：需求单 `.upd` 的实际编码只有
/// UTF-8 与 GBK 两种，严格 UTF-8 解码失败即可直接判定为 GBK；若引入启发式
/// 猜测，反而可能把 GBK 文本误按 UTF-8 有损解码而产出乱码。
fn decode_upd_text(bytes: &[u8], path: &Path) -> Result<(String, &'static str), String> {
    if bytes.is_empty() {
        return Ok((String::new(), "UTF-8"));
    }

    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(bytes[3..].to_vec())
            .map(|s| (s, "UTF-8(BOM)"))
            .map_err(|_| format!("UTF-8 BOM 解码失败: {}", path.display()));
    }

    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&units)
            .map(|s| (s, "UTF-16LE"))
            .map_err(|_| format!("UTF-16LE 解码失败: {}", path.display()));
    }

    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&units)
            .map(|s| (s, "UTF-16BE"))
            .map_err(|_| format!("UTF-16BE 解码失败: {}", path.display()));
    }

    if let Ok(s) = std::str::from_utf8(bytes) {
        return Ok((s.to_string(), "UTF-8"));
    }

    let (cow, _, _had_errors) = GBK.decode(bytes);
    Ok((cow.to_string(), "GBK"))
}

/// 把 `entries` 依次追加合并到 `output_file`。
///
/// 输出为 UTF-8 编码；每段正文前后写入 XML 风格注释标记；正文原样保留。
///
/// **不读盘**：正文取自 `UpdEntry::content`（收集阶段已读入内存）。因此调用方
/// 只需保证「收集」发生得早于源文件被清理/搬迁，本函数自身随时可调用。
pub fn merge_upd_entries(
    entries: &[UpdEntry],
    output_file: &Path,
    cancel: &AtomicBool,
) -> Result<UpdMergeSummary, String> {
    if let Some(parent) = output_file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建输出目录失败 {}: {}", parent.display(), e))?;
    }

    let mut records: Vec<UpdMergeRecord> = Vec::with_capacity(entries.len());
    let mut body = String::new();
    let mut total_bytes = 0u64;

    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            return Err("用户取消".to_string());
        }

        // 使用收集阶段的内存快照，不回读源文件：源文件可能已被清理
        // （需求单合并的 unzip/ 临时目录）或已被搬运（SP 合并的服务合并阶段）。
        let bytes = entry.content.as_slice();
        let (text, encoding) = decode_upd_text(bytes, &entry.src)?;

        total_bytes += bytes.len() as u64;

        // 段落分隔：beg 标记 → 正文（原样）→ 确保换行 → end 标记 → 空行
        body.push_str(&format!("<!-- ===== {} beg ===== -->\n", entry.display_path));
        body.push_str(&text);
        if !text.is_empty() && !text.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&format!("<!-- ===== {} end ===== -->\n\n", entry.display_path));

        records.push(UpdMergeRecord {
            display_path: entry.display_path.clone(),
            source_pkg: entry.source_pkg.clone(),
            encoding: encoding.to_string(),
            bytes: bytes.len() as u64,
        });
    }

    let mut out = String::new();
    out.push_str("<!-- merge_upd.xml - 需求单 upd 说明文件合并结果 (自动生成) -->\n");
    out.push_str(&format!(
        "<!-- 生成时间: {} -->\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    out.push_str(&format!("<!-- 合并文件数: {} -->\n", entries.len()));
    out.push_str("<!-- 正文按需求单包处理顺序追加，源文件编码已统一转换为 UTF-8，正文未做任何修改 -->\n");
    out.push_str("<!-- 正文为自由文本，本文件使用 XML 注释仅作分隔，不作为 XML 文档解析 -->\n");

    if !records.is_empty() {
        out.push_str("<!--\n  文件清单:\n");
        for (i, r) in records.iter().enumerate() {
            out.push_str(&format!(
                "    [{}] {} (来源包: {}, 编码: {}, {} 字节)\n",
                i + 1,
                r.display_path,
                r.source_pkg,
                r.encoding,
                r.bytes
            ));
        }
        out.push_str("-->\n");
    }
    out.push('\n');
    out.push_str(&body);

    std::fs::write(output_file, out.as_bytes()).map_err(|e| {
        format!("写入合并文件失败 {}: {}", output_file.display(), e)
    })?;

    Ok(UpdMergeSummary {
        output_file: output_file.to_string_lossy().to_string(),
        files: records.len(),
        total_bytes,
        records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "upd_merge_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn mkdir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
    }

    #[test]
    fn decode_plain_utf8_and_gbk_and_bom() {
        let root = tmp_root("decode");

        // 纯 UTF-8
        let utf8 = root.join("utf8.upd");
        std::fs::write(&utf8, "中文说明".as_bytes()).unwrap();
        let (text, enc) = decode_upd_text(&std::fs::read(&utf8).unwrap(), &utf8).unwrap();
        assert_eq!(text, "中文说明");
        assert_eq!(enc, "UTF-8");

        // UTF-8 BOM 必须剥掉 BOM
        let bom = root.join("bom.upd");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("中文".as_bytes());
        std::fs::write(&bom, &bytes).unwrap();
        let (text, enc) = decode_upd_text(&std::fs::read(&bom).unwrap(), &bom).unwrap();
        assert_eq!(text, "中文");
        assert_eq!(enc, "UTF-8(BOM)");

        // GBK：这些字节不是合法 UTF-8，必须回退 GBK 且不出现替换字符
        let gbk = root.join("gbk.upd");
        let gbk_bytes: Vec<u8> = GBK.encode("中文说明").0.into_owned();
        std::fs::write(&gbk, &gbk_bytes).unwrap();
        let (text, enc) = decode_upd_text(&std::fs::read(&gbk).unwrap(), &gbk).unwrap();
        assert_eq!(enc, "GBK");
        assert_eq!(text, "中文说明");
        assert!(!text.contains('\u{FFFD}'), "GBK 解码不应产生替换字符");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decode_utf16le_with_bom() {
        let root = tmp_root("utf16");
        let path = root.join("u16.upd");
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "中文".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();

        let (text, enc) = decode_upd_text(&std::fs::read(&path).unwrap(), &path).unwrap();
        assert_eq!(text, "中文");
        assert_eq!(enc, "UTF-16LE");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_picks_upd_recursively_and_ignores_others() {
        let root = tmp_root("collect");
        let t_dir = root.join("T202305185852");
        mkdir(&t_dir.join("reqsql").join("lfyh"));
        mkdir(&t_dir.join("ifmonline").join("WEB-INF"));
        mkdir(&t_dir.join("__MACOSX"));

        std::fs::write(t_dir.join("reqsql").join("lfyh").join("b.upd"), b"b").unwrap();
        std::fs::write(t_dir.join("reqsql").join("lfyh").join("a.upd"), b"a").unwrap();
        std::fs::write(t_dir.join("reqsql").join("lfyh").join("C.UPD"), b"c").unwrap();
        std::fs::write(t_dir.join("reqsql").join("lfyh").join("note.txt"), b"x").unwrap();
        std::fs::write(t_dir.join("ifmonline").join("WEB-INF").join("d.upd"), b"d").unwrap();
        std::fs::write(t_dir.join("__MACOSX").join("._e.upd"), b"e").unwrap();

        let cancel = AtomicBool::new(false);
        let mut entries: Vec<UpdEntry> = Vec::new();
        let n = collect_upd_entries(&t_dir, "PKG_A", &mut entries, &cancel).unwrap();

        assert_eq!(n, 4, "只收 .upd（大小写不敏感），隐藏目录内不计");
        let paths: Vec<&str> = entries
            .iter()
            .map(|e| e.display_path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec![
                "ifmonline/WEB-INF/d.upd",
                "reqsql/lfyh/C.UPD",
                "reqsql/lfyh/a.upd",
                "reqsql/lfyh/b.upd",
            ],
            "同一包内必须按相对路径字典序"
        );
        assert!(entries.iter().all(|e| e.source_pkg == "PKG_A"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 同名（同一相对路径）.upd 跨包只保留修改日期最新的一份，不重复追加。
    #[test]
    fn collect_dedupes_same_path_upd_by_mtime() {
        fn at(y: i32, m: u32, d: u32) -> chrono::NaiveDateTime {
            chrono::NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        }

        let root = tmp_root("dedupe");
        let pkg_old = root.join("PKG_OLD");
        let pkg_new = root.join("PKG_NEW");
        let rel = Path::new("reqsql").join("lfyh").join("20260929001.upd");
        mkdir(&pkg_old.join("reqsql").join("lfyh"));
        mkdir(&pkg_new.join("reqsql").join("lfyh"));
        std::fs::write(pkg_old.join(&rel), b"old").unwrap();
        std::fs::write(pkg_new.join(&rel), b"new").unwrap();
        crate::merge_common::set_file_mtime(&pkg_old.join(&rel), at(2026, 1, 1));
        crate::merge_common::set_file_mtime(&pkg_new.join(&rel), at(2026, 6, 1));

        let cancel = AtomicBool::new(false);
        let mut entries: Vec<UpdEntry> = Vec::new();
        collect_upd_entries(&pkg_old, "PKG_OLD", &mut entries, &cancel).unwrap();
        collect_upd_entries(&pkg_new, "PKG_NEW", &mut entries, &cancel).unwrap();

        assert_eq!(entries.len(), 1, "同名 .upd 只保留一份");
        assert_eq!(entries[0].source_pkg, "PKG_NEW", "保留日期最新的一份");
        assert_eq!(std::fs::read(&entries[0].src).unwrap(), b"new");
        assert_eq!(entries[0].content, b"new", "正文必须来自日期最新的一份");

        // 反向：日期较旧的包后处理，不得覆盖已收集的新日期条目
        let mut e2: Vec<UpdEntry> = Vec::new();
        collect_upd_entries(&pkg_new, "PKG_NEW", &mut e2, &cancel).unwrap();
        collect_upd_entries(&pkg_old, "PKG_OLD", &mut e2, &cancel).unwrap();
        assert_eq!(e2.len(), 1);
        assert_eq!(e2[0].source_pkg, "PKG_NEW");
        assert_eq!(std::fs::read(&e2[0].src).unwrap(), b"new");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_appends_in_order_with_markers_and_utf8_output() {
        let root = tmp_root("merge");
        let pkg_a = root.join("unzip").join("PKG_A");
        let pkg_b = root.join("unzip").join("PKG_B");
        mkdir(&pkg_a.join("reqsql").join("lfyh"));
        mkdir(&pkg_b.join("reqsql").join("lfyh"));

        std::fs::write(pkg_a.join("reqsql").join("lfyh").join("a.upd"), "AAA\n").unwrap();
        let gbk_bytes: Vec<u8> = GBK.encode("BBB中文").0.into_owned();
        std::fs::write(pkg_b.join("reqsql").join("lfyh").join("b.upd"), &gbk_bytes).unwrap();

        let cancel = AtomicBool::new(false);
        let mut entries: Vec<UpdEntry> = Vec::new();
        collect_upd_entries(&pkg_a, "PKG_A", &mut entries, &cancel).unwrap();
        collect_upd_entries(&pkg_b, "PKG_B", &mut entries, &cancel).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].display_path, "reqsql/lfyh/a.upd");
        assert_eq!(entries[0].source_pkg, "PKG_A");

        let out_file = root.join("merge_upd.xml");
        let summary = merge_upd_entries(&entries, &out_file, &cancel).unwrap();
        assert_eq!(summary.files, 2);
        assert_eq!(summary.records[0].encoding, "UTF-8");
        assert_eq!(summary.records[1].encoding, "GBK");

        let content = std::fs::read(&out_file).unwrap();
        let text = String::from_utf8(content).expect("输出必须是合法 UTF-8");
        assert!(text.contains("<!-- ===== reqsql/lfyh/a.upd beg ===== -->"));
        assert!(text.contains("<!-- ===== reqsql/lfyh/a.upd end ===== -->"));
        assert!(text.contains("<!-- ===== reqsql/lfyh/b.upd beg ===== -->"));
        assert!(text.contains("BBB中文"), "GBK 正文必须已正确转码");
        assert!(!text.contains('\u{FFFD}'));

        // 包顺序：a 段必须整体位于 b 段之前
        let pos_a = text.find("a.upd beg").unwrap();
        let pos_b = text.find("b.upd beg").unwrap();
        assert!(pos_a < pos_b, "必须按加入顺序（包顺序）追加");

        // 源文件不被修改
        assert_eq!(
            std::fs::read(pkg_a.join("reqsql").join("lfyh").join("a.upd")).unwrap(),
            b"AAA\n"
        );
        assert_eq!(
            std::fs::read(pkg_b.join("reqsql").join("lfyh").join("b.upd")).unwrap(),
            gbk_bytes
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 回归用例（「SP 包合并提示 upd 路径不存在」根因）：正文在收集阶段就进了内存，
    /// 源文件随后被清理/搬走也不得影响合并。
    #[test]
    fn merge_succeeds_after_sources_are_removed() {
        let root = tmp_root("lifecycle");
        let t_dir = root.join("T202609018888");
        mkdir(&t_dir.join("reqsql").join("lfyh"));
        std::fs::write(t_dir.join("reqsql").join("lfyh").join("a.upd"), "AAA\n").unwrap();

        let cancel = AtomicBool::new(false);
        let mut entries: Vec<UpdEntry> = Vec::new();
        collect_upd_entries(&t_dir, "PKG_A", &mut entries, &cancel).unwrap();
        assert_eq!(entries.len(), 1, "收集阶段应拿到条目");
        assert_eq!(entries[0].content, b"AAA\n", "正文必须在收集阶段入内存");

        // 模拟源文件被清理（unzip 临时目录）或被服务合并阶段搬走（database/）
        std::fs::remove_dir_all(&t_dir).unwrap();
        assert!(!entries[0].src.is_file(), "源文件确已不存在");

        let out_file = root.join("merge_upd.xml");
        let summary = merge_upd_entries(&entries, &out_file, &cancel).unwrap();
        assert_eq!(summary.files, 1);
        assert_eq!(summary.records[0].bytes, 4);

        let text = std::fs::read_to_string(&out_file).unwrap();
        assert!(text.contains("<!-- ===== reqsql/lfyh/a.upd beg ===== -->"));
        assert!(text.contains("AAA"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_empty_entries_still_writes_header() {
        let root = tmp_root("empty");
        let out_file = root.join("merge_upd.xml");
        let cancel = AtomicBool::new(false);

        let summary = merge_upd_entries(&[], &out_file, &cancel).unwrap();
        assert_eq!(summary.files, 0);

        let text = std::fs::read_to_string(&out_file).unwrap();
        assert!(text.contains("合并文件数: 0"));
        assert!(!text.contains("beg"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
