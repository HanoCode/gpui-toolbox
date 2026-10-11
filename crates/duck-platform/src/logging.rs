//! 结构化日志：写按天分文件的滚动日志，不阻塞调用方（每次写入只做一次短临界区）。
//!
//! 未用 `tracing-appender` —— 它需要额外的后台线程 + 消息通道；这里的写入量是「每次事件
//! 一行」，用一个 `Mutex<File>` 就够了，换来少一层依赖和一个少一处可能丢日志的缓冲。

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::filter::LevelFilter;

use crate::paths;

/// 日志文件后缀。目录里只处理这个后缀，别的东西（用户自己放的）一概不碰。
const LOG_SUFFIX: &str = ".log";

/// 默认保留天数（设置页的默认档位）。
pub const LOG_RETENTION_DEFAULT_DAYS: u32 = 7;

/// 「保留期」的哨兵值：把它传给 [`prune_logs`] 时不按时间删任何文件。
///
/// **它不是「一个文件都不删」**：目录占用兜底（[`LOG_DIR_MAX_BYTES`]）与保留期无关，
/// 照旧生效。两件事回答的是不同的问题——「多久之前的不要了」和「这台机器上最多占多少」。
pub const LOG_RETENTION_FOREVER: u32 = 0;

/// 单个日志文件的大小上限：写满就轮转出 `.1.log`，而不是继续往同一个文件里塞。
///
/// 8 MiB 这个数的依据：一条 INFO 行约 150 字节，8 MiB ≈ 5.5 万行 —— 比「一次故障排查
/// 想看的量」多一个数量级，又小到查看器读得完（它读文件末尾 512 KB）。
pub const LOG_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 同一个日期最多留几段。
///
/// `duck-YYYY-MM-DD.log` 是第 0 段（当前在写的），`.1.log` 是第 1 段，序号越大越老。
/// 上限的存在让「一天最多占多少」有确定答案：5 段 × 8 MiB = 40 MiB，而不是
/// 「按天滚动」听起来无害、实际能吃到整块磁盘。
pub const LOG_SEGMENTS_PER_DAY: u32 = 5;

/// 日志目录的占用兜底（字节）。
///
/// 保留期管的是「多久之前的不要了」，它管的是「最多占多少」。两者的失效方式不同：
/// 保留期设成 30 天 + `DUCK_LOG=debug`，前者一点没错，磁盘却会满。所以它是**兜底**，
/// 不看保留期设置。
pub const LOG_DIR_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// 取不到时间的文件用这个日期戳：它排在所有真实日期之后，也就是**最后才考虑删**。
///
/// 空串会更简单，但它排在最前，等于「认不出时间就先删它」——与
/// [`prune_logs`]「认不出日期就当它不过期」的取向正好相反。
const UNKNOWN_STAMP: &str = "9999-99-99";

/// 当前打开着的日志文件。
struct OpenFile {
    /// 打开它时的日期戳（换日就要换文件）。
    day: String,
    file: File,
    /// 已经写进去多少字节。自己数而不是每次 `stat`：写入路径上多一次系统调用不值。
    written: u64,
}

/// 按天滚动的日志写入器：内部只在换日、或当天写满时重新打开文件。
pub struct RollingWriter {
    dir: PathBuf,
    /// 单文件上限。做成字段而不是直接读常量，是为了让测试能用几十字节去验证轮转，
    /// 而不是真的写满 8 MiB。生产路径恒为 [`LOG_FILE_MAX_BYTES`]（见 [`Self::new`]）。
    max_bytes: u64,
    /// 当前文件；`None` 表示尚未打开过。
    state: Mutex<Option<OpenFile>>,
}

impl RollingWriter {
    /// 在 `dir` 下写 `duck-YYYY-MM-DD.log`。
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_file_limit(dir, LOG_FILE_MAX_BYTES)
    }

    /// 指定单文件上限（只有测试用得上小值）。
    fn with_file_limit(dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self {
            dir: dir.into(),
            max_bytes,
            state: Mutex::new(None),
        }
    }

    /// 当天的日志文件路径（用于「在文件管理器中显示日志」）。
    ///
    /// 指的是**当前在写的那个**（第 0 段），不是当天最老的那个。
    #[must_use]
    pub fn current_path(&self) -> PathBuf {
        self.dir.join(file_name(&today_stamp()))
    }

    /// 写入一段字节；换日、或当天这个文件写满时自动滚动。
    fn write_bytes(&self, bytes: &[u8]) -> io::Result<usize> {
        let today = today_stamp();
        let arrive = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        // 锁中毒时继续用内部数据：一条日志写失败不该让整个应用崩。
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        let needs_open = match guard.as_ref() {
            None => true,
            // 换日：直接开新一天的文件，旧那天的原样留着（清理交给 `prune_logs`）。
            Some(open) if open.day != today => true,
            // 写满：`written > 0` 这一条是防「单条日志比上限还大」时每写一次就轮转一次 ——
            // 空文件不再轮转，那条超大日志就落在新文件里，之后照常。
            Some(open) => open.written > 0 && open.written.saturating_add(arrive) > self.max_bytes,
        };
        if needs_open {
            std::fs::create_dir_all(&self.dir)?;
            if guard.as_ref().is_some_and(|open| open.day == today) {
                rotate_day(&self.dir, &today);
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(file_name(&today)))?;
            *guard = Some(OpenFile {
                day: today,
                file,
                written: 0,
            });
        }
        if let Some(open) = guard.as_mut() {
            open.file.write_all(bytes)?;
            open.file.flush()?;
            open.written = open.written.saturating_add(arrive);
        }
        Ok(bytes.len())
    }
}

/// 把某一天的当前文件挪成 `.1.log`，已有的段各推一格；最老的一段被挤掉。
///
/// 所有文件操作都忽略失败（目标被占用、权限、源不存在）。轮转失败最坏的结果是继续往
/// 旧文件里写 —— 比在写日志的路径上抛错好得多。
fn rotate_day(dir: &Path, day: &str) {
    // `saturating_sub` 只是让常量被改成 1 时也不下溢；那时等价于「不留历史段」。
    let oldest = LOG_SEGMENTS_PER_DAY.saturating_sub(1);
    let _ = std::fs::remove_file(dir.join(segment_name(day, oldest)));
    for index in (1..oldest).rev() {
        let from = dir.join(segment_name(day, index));
        if from.exists() {
            let _ = std::fs::rename(&from, dir.join(segment_name(day, index + 1)));
        }
    }
    let _ = std::fs::rename(dir.join(file_name(day)), dir.join(segment_name(day, 1)));
}

/// 第 `index` 段的文件名。`index == 0` 就是当前文件（见 [`file_name`]）。
fn segment_name(day: &str, index: u32) -> String {
    format!("duck-{day}.{index}.log")
}

impl io::Write for &RollingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_bytes(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `MakeWriter` 让 `tracing-subscriber` 每次事件拿到一个写句柄。
pub struct RollingHandle<'a>(&'a RollingWriter);

impl io::Write for RollingHandle<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_bytes(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RollingWriter {
    type Writer = RollingHandle<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        RollingHandle(self)
    }
}

/// 安装全局日志订阅器，返回当天的日志文件路径。
///
/// 级别由环境变量 `DUCK_LOG` 决定（`trace` / `debug` / `info` / `warn` / `error`），
/// 默认 `info`。重复调用（订阅器已安装）时静默返回路径。
#[must_use]
pub fn init_logging() -> PathBuf {
    let dir = paths::log_dir();
    let _ = paths::ensure_dir(&dir);
    let writer = RollingWriter::new(dir.clone());
    let path = writer.current_path();

    let level = level_from_env();
    let _ = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_line_number(true)
        .with_max_level(level)
        .try_init();

    path
}

/// `DUCK_LOG` 解析；未知值回落到 `info`。
fn level_from_env() -> LevelFilter {
    match std::env::var("DUCK_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        "off" => LevelFilter::OFF,
        _ => LevelFilter::INFO,
    }
}

/// 清理日志目录：先按保留期删，再按占用兜底删，返回被删掉的路径。
///
/// 两级的取舍各自成文，见 [`LOG_RETENTION_FOREVER`] 与 [`LOG_DIR_MAX_BYTES`]。
#[must_use]
pub fn prune_logs(dir: &Path, keep_days: u32) -> Vec<PathBuf> {
    prune_logs_with(dir, keep_days, LOG_DIR_MAX_BYTES)
}

/// [`prune_logs`] 的实现。上限做成参数，是为了让测试能用小目录验证「占用兜底与保留期
/// 设置无关」这条语义，而不必造出 256 MiB 的文件。
fn prune_logs_with(dir: &Path, keep_days: u32, max_bytes: u64) -> Vec<PathBuf> {
    let mut removed = prune_by_age(dir, keep_days);
    removed.extend(enforce_size_cap(dir, max_bytes));
    removed
}

/// 按保留期删；[`LOG_RETENTION_FOREVER`] 时不做任何事。
///
/// 「这个文件属于哪一天」优先看**文件名里的日期**（写入方按天命名，语义最准，也不受
/// 拷贝 / 解压改动 mtime 的影响），名字里没有 `YYYY-MM-DD` 时才回退到文件的修改时间。
///
/// `keep_days` 数的是**包含今天在内**的天数窗口：`7` 表示留下今天与之前 6 天，更早的删。
/// 目录不存在、读不出条目都当「没有可清理的」，单个文件删失败（被占用、权限）只跳过它
/// 自己——清理是后台动作，不该因此报错打断调用方。
fn prune_by_age(dir: &Path, keep_days: u32) -> Vec<PathBuf> {
    if keep_days == LOG_RETENTION_FOREVER {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let today = today_days();
    // 保留 [cutoff, today] 闭区间。
    let cutoff = today - i64::from(keep_days) + 1;
    let cutoff_stamp = stamp_of_days(cutoff);

    let mut removed = Vec::new();
    for entry in entries.flatten() {
        if !is_log_file(&entry) {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let expired = match name.as_deref().and_then(log_file_stamp) {
            // 文件名日期是 `YYYY-MM-DD`，按字典序比较就等于按时间比较。
            Some(stamp) => stamp < cutoff_stamp,
            None => mtime_day(&entry).is_some_and(|day| day < cutoff),
        };
        if expired && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

/// 目录占用兜底：`.log` 总字节超过 `max_bytes` 时按「最老优先」删，直到回到线下。
///
/// **永不删最新的那一个文件**。两个理由：它多半正是当前在写的那个（Windows 上打开着的
/// 文件也删不掉，白试一次）；而「把日志删到一条不剩」比「稍微超一点」更像一次故障。
///
/// 排序键是 `(日期戳, 段序号)`：日期优先，所以先整个删掉旧的日子的；同一天里段序号大的
/// 更老（见 [`segment_name`]），所以同一段里也是从最老的开始。认不出日期、也取不到修改
/// 时间的文件落到 [`UNKNOWN_STAMP`]，排在最后。
///
/// 超限判定用的是**删除前**的总量，删到线下就停 —— 不做「尽量接近」的贪心，因为再删一个
/// 就是删一条还算新的日志，而占用本来就是兜底而不是精明账。
#[must_use]
pub fn enforce_size_cap(dir: &Path, max_bytes: u64) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    // `(路径, 日期戳, 段序号, 字节数)`。三个排序 / 累加字段都先抄出来：后面要排序，
    // 不能一边排一边回头问 `DirEntry`。
    let mut files: Vec<(PathBuf, String, u32, u64)> = Vec::new();
    for entry in entries.flatten() {
        if !is_log_file(&entry) {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let day = name
            .as_deref()
            .and_then(log_file_stamp)
            .unwrap_or_else(|| mtime_stamp(&entry));
        let segment = name.as_deref().map_or(0, log_file_segment);
        let bytes = entry.metadata().map_or(0, |meta| meta.len());
        files.push((path, day, segment, bytes));
    }

    let mut total: u64 = files.iter().map(|(_, _, _, bytes)| *bytes).sum();
    if total <= max_bytes {
        return Vec::new();
    }

    // 最老在前：日期升序；同一天里段序号**降序**（序号大的更老）。
    files.sort_by(|left, right| left.1.cmp(&right.1).then(right.2.cmp(&left.2)));

    let mut removed = Vec::new();
    for (index, (path, _, _, bytes)) in files.iter().enumerate() {
        // 最后一个是「最新的那一个」，不论还超不超都留着。
        if total <= max_bytes || index + 1 == files.len() {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*bytes);
            removed.push(path.clone());
        }
    }
    removed
}

/// 目录里 `.log` 的 `(文件数, 总字节数)`（设置页的「当前占用」）。
///
/// 与清理走同一套「什么算日志文件」的判据（[`is_log_file`]），所以这个数字和
/// [`enforce_size_cap`] 眼里的总量是同一个。
#[must_use]
pub fn logs_summary(dir: &Path) -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut count = 0;
    let mut bytes = 0;
    for entry in entries.flatten() {
        if !is_log_file(&entry) {
            continue;
        }
        count += 1;
        bytes += entry.metadata().map_or(0, |meta| meta.len());
    }
    (count, bytes)
}

/// 是不是一个该被日志逻辑管的文件：`.log` 后缀 + 真的普通文件。
///
/// 只要后缀不看前缀（用户自己丢进来的 `.log` 也在内），但必须是文件 —— 目录也能叫
/// `x.log`，对它 `remove_file` 只会失败。
fn is_log_file(entry: &std::fs::DirEntry) -> bool {
    entry.file_name().to_string_lossy().ends_with(LOG_SUFFIX)
        && entry.file_type().is_ok_and(|kind| kind.is_file())
}

fn file_name(stamp: &str) -> String {
    format!("duck-{stamp}.log")
}

/// 段序号：`duck-YYYY-MM-DD.3.log` → 3；`duck-YYYY-MM-DD.log` → 0。
///
/// 只认 `.log` 前面那一段纯数字。别的命名（`foo.log`）与解析不出来的都当 0，
/// 也就是「当前段」——它们没有段的概念，不该被当成最老的先删。
///
/// 公开是因为**查看器也要用它**（`pages::logs` 按 `(日期, 段序号)` 排文件列表）：
/// 命名规则只有写入方知道，读出方再猜一遍必然会漂。段序号大的更老，这是这条规则的一部分。
#[must_use]
pub fn log_file_segment(name: &str) -> u32 {
    let Some(stem) = name.strip_suffix(LOG_SUFFIX) else {
        return 0;
    };
    let Some((_, tail)) = stem.rsplit_once('.') else {
        return 0;
    };
    tail.parse().unwrap_or(0)
}

/// 修改时间 → 日期戳；取不到时给 [`UNKNOWN_STAMP`]（那它不会被优先删）。
fn mtime_stamp(entry: &std::fs::DirEntry) -> String {
    mtime_day(entry).map_or_else(|| UNKNOWN_STAMP.to_owned(), stamp_of_days)
}

/// 文件名里第一个 `YYYY-MM-DD` 形态的日期；没有则 `None`。
///
/// 扫描而不是按前缀精确匹配：查看器认得目录里任何 `.log`（见 `pages::logs`），
/// 清理也按同一宽容度处理——只是「认不出日期」的那些改看修改时间。
///
/// 公开的理由与 [`log_file_segment`] 相同：写出的名字得由读出的那一侧解释，规则一处一份。
#[must_use]
pub fn log_file_stamp(name: &str) -> Option<String> {
    let is_date = |window: &[u8]| {
        window[4] == b'-'
            && window[7] == b'-'
            && window[..4].iter().all(u8::is_ascii_digit)
            && window[5..7].iter().all(u8::is_ascii_digit)
            && window[8..10].iter().all(u8::is_ascii_digit)
    };
    let bytes = name.as_bytes();
    (0..bytes.len().saturating_sub(9))
        .find(|start| is_date(&bytes[*start..*start + 10]))
        // 日期串必然落在 ASCII 区，但起点两侧可能是多字节字符，`get` 拒绝切在中间。
        .and_then(|start| name.get(start..start + 10))
        .map(ToOwned::to_owned)
}

/// 文件的修改时间落在哪一天（UTC 天序号）；取不到时 `None`（那就当它不过期）。
fn mtime_day(entry: &std::fs::DirEntry) -> Option<i64> {
    let modified = entry.metadata().ok()?.modified().ok()?;
    let seconds = modified.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(seconds.div_euclid(86_400))
}

/// 当天日期戳 `YYYY-MM-DD`（UTC）。
///
/// 用纯算术把「自 epoch 的天数」转成公历日期（Howard Hinnant 的 civil_from_days），
/// 避免为一行文件名引入时间库或 `unsafe`。注意：这里是 **UTC** 日期，
/// 日志按天滚动时跨时区的边界差异可接受。
///
/// `pub(crate)` 供 [`crate::crash`] 复用：崩溃日志与运行日志按同一套规则命名。
pub(crate) fn today_stamp() -> String {
    stamp_of_days(today_days())
}

/// 时刻戳 `YYYY-MM-DD HH:MM:SSZ`（UTC）。
///
/// 崩溃现场需要「什么时候崩的」，只精确到天不够用；复用同一套公历换算，
/// 免得为一行时间戳再引一个时间库。秒是最细的粒度——崩溃日志不需要毫秒。
pub(crate) fn utc_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_secs() as i64);
    let within = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        within / 3_600,
        within % 3_600 / 60,
        within % 60
    )
}

/// 文件用时刻戳 `YYYYMMDD-HHmmss`（UTC）。
///
/// 数据备份包命名用（`duck-backup-20261011-143005.tar.gz`）。与日志的 `YYYY-MM-DD`
/// 同一套公历换算，只是换个拼法：文件名里不要冒号（Windows 禁止）与空格。
#[must_use]
pub fn file_stamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_secs() as i64);
    stamp_of_seconds(seconds)
}

/// 秒 → `YYYYMMDD-HHmmss`。
fn stamp_of_seconds(seconds: i64) -> String {
    let within = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        within / 3_600,
        within % 3_600 / 60,
        within % 60
    )
}

/// 天序号 → `YYYY-MM-DD`。
fn stamp_of_days(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// 自 Unix epoch 起的天数（UTC）。
fn today_days() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_secs() as i64);
    seconds.div_euclid(86_400)
}

/// 自 Unix epoch 起的天数 → `(年, 月, 日)`。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{
        LOG_FILE_MAX_BYTES, LOG_SEGMENTS_PER_DAY, RollingWriter, civil_from_days, enforce_size_cap,
        file_name, log_file_segment, log_file_stamp, logs_summary, prune_logs, prune_logs_with,
        rotate_day, segment_name, stamp_of_days, today_days,
    };
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    /// 临时目录：带进程号与标签，避免与并发跑的其它测试撞名。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-logprune-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 相对今天偏移 `days`（负数为过去）的日期戳。
    fn stamp_days_ago(days: i64) -> String {
        stamp_of_days(today_days() - days)
    }

    #[test]
    fn epoch_is_1970_01_01() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn known_dates_round_trip() {
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_089), (2025, 1, 1));
    }

    #[test]
    fn a_stamp_is_looked_up_inside_the_name() {
        assert_eq!(
            log_file_stamp("duck-2026-10-04.log").as_deref(),
            Some("2026-10-04")
        );
        assert_eq!(
            log_file_stamp("2026-10-04.log").as_deref(),
            Some("2026-10-04")
        );
        assert_eq!(
            log_file_stamp("app.2026-10-04.0.log").as_deref(),
            Some("2026-10-04")
        );
        // 轮转出来的段同样认得出：清理按同一套日期判过期，不会因为多了 `.3` 就漏掉。
        assert_eq!(
            log_file_stamp("duck-2026-10-04.3.log").as_deref(),
            Some("2026-10-04")
        );
        assert_eq!(log_file_stamp("session.log"), None);
        assert_eq!(log_file_stamp("duck-26-10-04.log"), None, "两位年份不算");
        assert_eq!(log_file_stamp("duck-2026-10-0x.log"), None, "非数字不算");
    }

    #[test]
    fn segment_numbers_are_read_off_the_name() {
        assert_eq!(log_file_segment("duck-2026-10-04.log"), 0, "当前段没有序号");
        assert_eq!(log_file_segment("duck-2026-10-04.1.log"), 1);
        assert_eq!(log_file_segment("duck-2026-10-04.12.log"), 12);
        assert_eq!(log_file_segment("foo.log"), 0, "没有点：当当前段");
        assert_eq!(
            log_file_segment("foo.bar.log"),
            0,
            "点后面不是数字：当当前段"
        );
        assert_eq!(log_file_segment("duck-2026-10-04.1.log.1"), 0, "后缀不对");
    }

    /// 写入方怎么命名、读出方就怎么解释。
    ///
    /// 拼法只有一处（[`file_name`] / [`segment_name`]），而解释它的两个函数是**公开的**
    /// （查看器要按 `(日期, 段序号)` 排列表）。这条把两边钉在一起：谁改了拼法立刻红，
    /// 而不是等用户发现日志列表的顺序乱了。
    #[test]
    fn the_written_names_round_trip_through_the_parsers() {
        let day = "2026-10-04";
        assert_eq!(log_file_stamp(&file_name(day)).as_deref(), Some(day));
        assert_eq!(log_file_segment(&file_name(day)), 0);

        for index in 1..LOG_SEGMENTS_PER_DAY {
            let name = segment_name(day, index);
            assert_eq!(log_file_stamp(&name).as_deref(), Some(day), "第 {index} 段");
            assert_eq!(log_file_segment(&name), index);
        }
    }

    /// 写满就轮转：旧内容挪进 `.1.log`，新内容从新的 `.log` 开始。
    #[test]
    fn a_full_file_rolls_into_a_numbered_segment() {
        let dir = temp_dir("roll");
        // 32 字节上限 + 每条 11 字节 → 第三次写入会超，于是轮转。
        let writer = RollingWriter::with_file_limit(&dir, 32);
        for _ in 0..3 {
            (&writer).write_all(b"0123456789\n").expect("写日志");
        }
        (&writer).write_all(b"0123456789\n").expect("写日志");

        let today = stamp_of_days(today_days());
        let current = std::fs::read(dir.join(format!("duck-{today}.log"))).expect("读当前段");
        let rolled = std::fs::read(dir.join(format!("duck-{today}.1.log"))).expect("读第 1 段");
        assert_eq!(rolled.len(), 22, "轮转前写了两条");
        assert_eq!(current.len(), 22, "轮转后又写了两条");
        assert_eq!(logs_summary(&dir), (2, 44));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全程不碰上限时一个段都不该多出来 —— 轮转不是「每次启动换一个文件」。
    #[test]
    fn a_file_under_the_limit_is_never_rolled() {
        let dir = temp_dir("noroll");
        let writer = RollingWriter::new(&dir);
        (&writer).write_all(b"hello\n").expect("写日志");

        let today = stamp_of_days(today_days());
        assert!(dir.join(format!("duck-{today}.log")).exists());
        assert_eq!(logs_summary(&dir).0, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 生产用的那个上限常量真的接在轮转上（而不是只有测试里的小值才生效）。
    ///
    /// 分步写是为了不把整块上限一次压进内存；步长取自常量，所以常量改了测试跟着改，
    /// 不会悄悄失去约束力。
    #[test]
    fn the_production_limit_is_the_one_that_rolls() {
        let dir = temp_dir("prodlimit");
        let writer = RollingWriter::new(&dir);
        let today = stamp_of_days(today_days());
        let rolled = dir.join(format!("duck-{today}.1.log"));
        let step = vec![b'x'; usize::try_from(LOG_FILE_MAX_BYTES / 8).unwrap()];

        for _ in 0..7 {
            (&writer).write_all(&step).expect("写日志");
        }
        assert!(!rolled.exists(), "上限的 7/8 不该轮转");

        for _ in 0..2 {
            (&writer).write_all(&step).expect("写日志");
        }
        assert!(rolled.exists(), "越过上限必须轮转出新段");
        assert_eq!(
            std::fs::metadata(&rolled).unwrap().len(),
            LOG_FILE_MAX_BYTES,
            "轮转出去的正好是写满的那个文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 段窗口是有界的：反复轮转只会把最老的那一段挤掉，不会越攒越多。
    #[test]
    fn rotate_keeps_at_most_the_segment_window() {
        let dir = temp_dir("rotate");
        let day = "2026-10-11";
        let rounds = usize::try_from(LOG_SEGMENTS_PER_DAY).unwrap() + 3;
        for round in 0..rounds {
            std::fs::write(
                dir.join(format!("duck-{day}.log")),
                format!("round {round}"),
            )
            .expect("写当前段");
            rotate_day(&dir, day);
        }

        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let expected: Vec<String> = (1..LOG_SEGMENTS_PER_DAY)
            .map(|index| format!("duck-{day}.{index}.log"))
            .collect();
        assert_eq!(names, expected, "只剩 {LOG_SEGMENTS_PER_DAY} - 1 段");

        let read = |index: u32| {
            std::fs::read_to_string(dir.join(format!("duck-{day}.{index}.log"))).expect("读段")
        };
        assert_eq!(read(1), format!("round {}", rounds - 1));
        // `.N-1` 是窗口里最老的一段，也就是**比它更老的那些真的没了** ——
        // 它正好是 `N-1` 轮之前写下的那一条。
        let oldest = LOG_SEGMENTS_PER_DAY - 1;
        assert_eq!(
            read(oldest),
            format!("round {}", rounds - usize::try_from(oldest).unwrap())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 占用兜底：最老的先删，删到线下就停，同一天里段序号大的算更老。
    #[test]
    fn the_size_cap_deletes_oldest_first() {
        let dir = temp_dir("cap");
        let day = |ago: i64| stamp_of_days(today_days() - ago);
        let oldest = format!("duck-{}.log", day(2));
        let older = format!("duck-{}.log", day(1));
        let newest_but_one = format!("duck-{}.1.log", day(0));
        let newest = format!("duck-{}.log", day(0));
        for name in [&oldest, &older, &newest_but_one, &newest] {
            std::fs::write(dir.join(name), vec![b'x'; 100]).expect("写日志");
        }

        // 总量 400，上限 250 → 删掉最老的两个，剩 200。
        let mut removed: Vec<String> = enforce_size_cap(&dir, 250)
            .into_iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        removed.sort();
        let mut expected = vec![oldest, older];
        expected.sort();
        assert_eq!(removed, expected);
        assert_eq!(logs_summary(&dir), (2, 200));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 兜底也不会把目录删空：只剩一个文件时，哪怕它自己就超限，也留着。
    #[test]
    fn the_size_cap_never_takes_the_last_file() {
        let dir = temp_dir("capsingle");
        std::fs::write(dir.join("only.log"), vec![b'x'; 4096]).expect("写日志");

        assert!(enforce_size_cap(&dir, 10).is_empty());
        assert!(dir.join("only.log").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「永久保留」拦得住按时间删，拦不住占用兜底——两者回答的是不同的问题。
    #[test]
    fn the_size_cap_applies_even_when_age_pruning_is_off() {
        let dir = temp_dir("capforever");
        std::fs::write(dir.join("duck-1999-01-01.log"), vec![b'x'; 300]).expect("写旧日志");
        std::fs::write(dir.join("duck-2026-10-11.log"), vec![b'x'; 100]).expect("写新日志");

        let removed = prune_logs_with(&dir, super::LOG_RETENTION_FOREVER, 200);
        let names: Vec<String> = removed
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["duck-1999-01-01.log"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_has_no_summary_instead_of_failing() {
        let dir = std::env::temp_dir().join(format!("duck-logsum-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(logs_summary(&dir), (0, 0));
        assert!(enforce_size_cap(&dir, 1).is_empty());
    }

    /// 保留窗口是「含今天在内的 keep_days 天」：7 天 = 今天 + 前 6 天。
    #[test]
    fn pruning_keeps_the_window_and_drops_older_files() {
        let dir = temp_dir("window");
        let kept = [stamp_days_ago(0), stamp_days_ago(6)];
        let dropped = [stamp_days_ago(7), stamp_days_ago(40)];
        for stamp in kept.iter().chain(dropped.iter()) {
            std::fs::write(dir.join(format!("duck-{stamp}.log")), b"x").expect("写日志");
        }
        // 非 `.log` 的东西一概不碰。
        std::fs::write(dir.join("notes.txt"), b"x").expect("写非日志");

        let mut removed: Vec<String> = prune_logs(&dir, 7)
            .into_iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        removed.sort();

        let mut expected: Vec<String> = dropped
            .iter()
            .map(|stamp| format!("duck-{stamp}.log"))
            .collect();
        expected.sort();
        assert_eq!(removed, expected);

        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            3,
            "留下两条 + 非日志"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「永久保留」是个哨兵值：按时间一个文件都不删，哪怕全都很老。
    ///
    /// 注意它**只**管按时间那一级 —— 占用兜底照旧生效，见
    /// [`the_size_cap_applies_even_when_age_pruning_is_off`]。
    #[test]
    fn keep_forever_deletes_nothing() {
        let dir = temp_dir("forever");
        std::fs::write(dir.join("duck-1999-01-01.log"), b"x").expect("写日志");
        assert!(prune_logs(&dir, super::LOG_RETENTION_FOREVER).is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 名字里没有日期的 `.log`：只能看修改时间——刚写的必须留下，很旧的删掉。
    #[test]
    fn an_undated_log_falls_back_to_its_modification_time() {
        let dir = temp_dir("mtime");
        let fresh = dir.join("fresh.log");
        let old = dir.join("old.log");
        std::fs::write(&fresh, b"x").expect("写新日志");
        std::fs::write(&old, b"x").expect("写旧日志");
        let long_ago = SystemTime::now() - Duration::from_secs(30 * 86_400);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .expect("打开旧日志")
            .set_modified(long_ago)
            .expect("把修改时间拨回 30 天前");

        let removed = prune_logs(&dir, 7);
        assert_eq!(removed, vec![old], "只删按修改时间算过期的那个");
        assert!(fresh.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_prunes_nothing_instead_of_failing() {
        let dir = std::env::temp_dir().join(format!("duck-logprune-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(prune_logs(&dir, 7).is_empty());
    }
}
