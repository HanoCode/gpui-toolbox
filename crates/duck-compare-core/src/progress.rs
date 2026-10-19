//! 扫描进度：到哪一步了、第几批、扫了多少。
//!
//! 两个比对工具（class / jar）的阶段**完全同构**，只是计数单位不同（个类 vs 个包），
//! 所以共用这一份。单位由插件层决定措辞——内核只报数字，不拼句子（理由见
//! [`crate::class_compare`] 的模块头：内核里出现中文句子，i18n 就只能靠反查模板）。

/// 进度阶段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// 正在列基准侧清单（只读目录项，不打开任何文件）。
    BaseListing,
    /// 基准侧清单已出。
    BaseListed { count: usize },
    /// 对比侧清单已出。
    TargetListed { count: usize },
    /// 正在给基准侧抽版本，`batch` / `batches` 从 1 开始。
    BaseVersions {
        batch: usize,
        batches: usize,
        scanned: usize,
    },
    /// 正在给对比侧抽版本。
    TargetVersions {
        batch: usize,
        batches: usize,
        scanned: usize,
    },
    /// 配对判定中。
    Comparing,
}

/// 一次进度上报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub percent: u32,
    pub stage: Stage,
}

/// 分批扫并上报进度。
///
/// ⚠️ **与参考仓的一处有意偏差**：源仓 class 比对在这个循环里写着
/// `if pct > 50 { break; }`（注释 `// cap at 50`），本意是「百分比别越过下一阶段」，
/// 但 `break` 跳出的是**扫描循环本身**。条目数落在某些区间时（例如 51–149 个类，
/// 批数 2–3、分母取到 1），第二批算出的百分比是 60 或 70，于是**后面的批次整批不扫**，
/// 结果表静默少掉一半——用户只会以为工具漏了。这里把两件事拆开：
/// 百分比夹到 `high`，扫描一路到底。
pub fn scan_in_batches<T, R>(
    items: &[T],
    batch_size: usize,
    low: u32,
    high: u32,
    mut scan: impl FnMut(&[T]) -> Vec<R>,
    stage: impl Fn(usize, usize, usize) -> Stage,
    progress: &mut dyn FnMut(Progress),
) -> Vec<R> {
    if items.is_empty() {
        return Vec::new();
    }
    let batch_size = batch_size.max(1);
    let batches = items.len().div_ceil(batch_size);
    let mut scanned: Vec<R> = Vec::with_capacity(items.len());

    for (index, chunk) in items.chunks(batch_size).enumerate() {
        scanned.extend(scan(chunk));
        let step = (index + 1) as u32 * (high - low) / batches as u32;
        progress(Progress {
            percent: (low + step).min(high),
            stage: stage(index + 1, batches, scanned.len()),
        });
    }
    scanned
}
