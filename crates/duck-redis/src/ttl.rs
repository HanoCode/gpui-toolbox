//! 键 TTL 的**纯逻辑**：芯片分类、紧凑标签与本地范围过滤。
//!
//! 移植自 zedis 的 `zedis-core/src/ttl.rs`（设计文档 §4.5「叶子显示类型图标 + TTL 标记」）。
//! 本模块零网络、零 gpui：**颜色映射留给界面层**，内核只判语义档位，
//! 这样明暗主题可以各自配色，纯逻辑也照常可单测。

/// 「即将过期」阈值：低于它的存活 TTL 用警示色渲染。
///
/// 两分钟是仍然够用户看见芯片、打开键、赶在键消失前跑 `EXPIRE` 或复制值的最小窗口。
pub const TTL_EXPIRING_THRESHOLD_SECS: i64 = 120;

const SECS_PER_HOUR: i64 = 3600;
const SECS_PER_DAY: i64 = 86_400;
const SECS_PER_WEEK: i64 = 7 * SECS_PER_DAY;

/// 键树的 TTL 范围过滤：只在**已加载的 TTL 缓存**上做本地筛选（不再发 Redis 往返）。
///
/// 档位对齐 zedis：内存分析直方图桶 + 运维口径的「即将过期」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum TtlFilter {
    /// 不设 TTL 约束——每个键都通过这一维。
    #[default]
    All,
    /// 永久键（`TTL` 为 `-1`）。
    NoTtl,
    /// 即将过期：`0 ≤ ttl < TTL_EXPIRING_THRESHOLD_SECS`。
    Expiring,
    /// 一小时内过期：`0 ≤ ttl < 3600`。
    Lt1h,
    /// 一天内过期：`0 ≤ ttl < 86400`。
    Lt1d,
    /// 一周内过期：`0 ≤ ttl < 7d`。
    Lt7d,
    /// 长存活：`ttl ≥ 7d`。
    Gte7d,
}

impl TtlFilter {
    /// 全部档位（顺序即菜单顺序）。
    pub const ALL: [Self; 7] = [
        Self::All,
        Self::NoTtl,
        Self::Expiring,
        Self::Lt1h,
        Self::Lt1d,
        Self::Lt7d,
        Self::Gte7d,
    ];

    /// 稳定 wire 值（菜单项 id 用）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::NoTtl => "no_ttl",
            Self::Expiring => "expiring",
            Self::Lt1h => "lt_1h",
            Self::Lt1d => "lt_1d",
            Self::Lt7d => "lt_7d",
            Self::Gte7d => "gte_7d",
        }
    }

    /// 解析 wire 值；未知值回落 [`TtlFilter::All`]。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "no_ttl" => Self::NoTtl,
            "expiring" => Self::Expiring,
            "lt_1h" => Self::Lt1h,
            "lt_1d" => Self::Lt1d,
            "lt_7d" => Self::Lt7d,
            "gte_7d" => Self::Gte7d,
            _ => Self::All,
        }
    }

    /// 一个缓存 TTL 是否命中本档。
    ///
    /// `None`（还没取到）与 `-2`（`SCAN` 与 `TTL` 之间键已消失）**永远不命中**
    /// 非 `All` 档——界面只按已经知道的样本过滤，绝不为了过滤去跑全键空间 TTL。
    #[must_use]
    pub fn matches(self, ttl_secs: Option<i64>) -> bool {
        if matches!(self, Self::All) {
            return true;
        }
        let Some(ttl) = ttl_secs else {
            return false;
        };
        match self {
            Self::All => true,
            Self::NoTtl => ttl == -1,
            Self::Expiring => (0..TTL_EXPIRING_THRESHOLD_SECS).contains(&ttl),
            Self::Lt1h => (0..SECS_PER_HOUR).contains(&ttl),
            Self::Lt1d => (0..SECS_PER_DAY).contains(&ttl),
            Self::Lt7d => (0..SECS_PER_WEEK).contains(&ttl),
            Self::Gte7d => ttl >= SECS_PER_WEEK,
        }
    }
}

/// TTL 芯片的语义档位（**颜色由界面层映射**，内核只给语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlChipKind {
    /// `-1`：无过期。中性 / 弱化色。
    Perm,
    /// `0..TTL_EXPIRING_THRESHOLD_SECS`：即将过期。单一警示色，
    /// 让用户一眼看见「活不过本次会话」的键。
    Expiring,
    /// `>= TTL_EXPIRING_THRESHOLD_SECS`：存活充裕。健康色。
    Live,
}

/// 该 TTL 该不该画芯片、画哪一档。
///
/// `-2`（`SCAN` 与 `TTL` 之间键被删）返回 `None`——那是转瞬即逝的竞态，不显示。
#[must_use]
pub fn ttl_chip_kind(ttl_secs: i64) -> Option<TtlChipKind> {
    match ttl_secs {
        -2 => None,
        -1 => Some(TtlChipKind::Perm),
        secs if secs < TTL_EXPIRING_THRESHOLD_SECS => Some(TtlChipKind::Expiring),
        _ => Some(TtlChipKind::Live),
    }
}

/// 键树里的紧凑 TTL 标签：最多两位数字 + 单位字母——`12s`、`4m`、`23h`、`7d`、`52w`、`3y`。
/// 永久键用 `∞`。
///
/// 只有 `-2`（键已不存在）返回 `None`——不画芯片。
#[must_use]
pub fn format_ttl_chip(ttl_secs: i64) -> Option<String> {
    if ttl_secs == -2 {
        return None;
    }
    if ttl_secs == -1 {
        return Some("∞".into());
    }
    // 每一档都控制在三个字符内（两位数字 + 单位），固定宽度的树内芯片才排得整齐；
    // 精度随量级下降，但**芯片不说谎**：≥100 天升到周、再到年，
    // 而不是把一年的 TTL 截成 `99d`。
    let label = if ttl_secs < 60 {
        format!("{ttl_secs}s")
    } else if ttl_secs < SECS_PER_HOUR {
        format!("{}m", ttl_secs / 60)
    } else if ttl_secs < SECS_PER_DAY {
        format!("{}h", ttl_secs / SECS_PER_HOUR)
    } else if ttl_secs < 100 * SECS_PER_DAY {
        format!("{}d", ttl_secs / SECS_PER_DAY)
    } else if ttl_secs < 700 * SECS_PER_DAY {
        // 100–699 天：14w–99w，仍是两位数字。
        format!("{}w", ttl_secs / SECS_PER_WEEK)
    } else {
        format!("{}y", ttl_secs / (365 * SECS_PER_DAY))
    };
    Some(label)
}

#[cfg(test)]
mod tests {
    use super::{
        TTL_EXPIRING_THRESHOLD_SECS, TtlChipKind, TtlFilter, format_ttl_chip, ttl_chip_kind,
    };

    /// 芯片分档：`-2` 不画、`-1` 永久、阈值前后分别是警示 / 健康。
    #[test]
    fn chip_kind_dispatch() {
        assert_eq!(ttl_chip_kind(-2), None);
        assert_eq!(ttl_chip_kind(-1), Some(TtlChipKind::Perm));
        assert_eq!(ttl_chip_kind(0), Some(TtlChipKind::Expiring));
        assert_eq!(ttl_chip_kind(119), Some(TtlChipKind::Expiring));
        // 阈值本身算存活（严格小于）。
        assert_eq!(
            ttl_chip_kind(TTL_EXPIRING_THRESHOLD_SECS),
            Some(TtlChipKind::Live)
        );
        assert_eq!(ttl_chip_kind(3600), Some(TtlChipKind::Live));
    }

    /// `All` 档对任何输入都放行（含未知 / 缺失）。
    #[test]
    fn filter_all_passes_everything() {
        assert!(TtlFilter::All.matches(None));
        assert!(TtlFilter::All.matches(Some(-2)));
        assert!(TtlFilter::All.matches(Some(-1)));
        assert!(TtlFilter::All.matches(Some(30)));
    }

    /// 永久档与即将过期档：边界与「未知不命中」。
    #[test]
    fn filter_no_ttl_and_expiring() {
        assert!(TtlFilter::NoTtl.matches(Some(-1)));
        assert!(!TtlFilter::NoTtl.matches(Some(0)));
        assert!(!TtlFilter::NoTtl.matches(None));
        assert!(TtlFilter::Expiring.matches(Some(0)));
        assert!(TtlFilter::Expiring.matches(Some(119)));
        assert!(!TtlFilter::Expiring.matches(Some(120)));
        assert!(!TtlFilter::Expiring.matches(Some(-1)));
        // 未知 / 已消失的键在不设限的档位之外一律不命中。
        assert!(!TtlFilter::Expiring.matches(None));
        assert!(!TtlFilter::Expiring.matches(Some(-2)));
    }

    /// 时 / 天 / 周三档桶的边界。
    #[test]
    fn filter_hour_day_week_buckets() {
        assert!(TtlFilter::Lt1h.matches(Some(3599)));
        assert!(!TtlFilter::Lt1h.matches(Some(3600)));
        assert!(TtlFilter::Lt1d.matches(Some(3600)));
        assert!(!TtlFilter::Lt1d.matches(Some(86_400)));
        assert!(TtlFilter::Lt7d.matches(Some(86_400)));
        assert!(!TtlFilter::Lt7d.matches(Some(7 * 86_400)));
        assert!(TtlFilter::Gte7d.matches(Some(7 * 86_400)));
        assert!(!TtlFilter::Gte7d.matches(Some(7 * 86_400 - 1)));
    }

    /// wire 往返：每个档位都能原样解析回来；未知值回落 `All`。
    #[test]
    fn filter_roundtrip_wire_names() {
        for filter in TtlFilter::ALL {
            assert_eq!(TtlFilter::from_wire(filter.wire_name()), filter);
        }
        assert_eq!(TtlFilter::from_wire("nope"), TtlFilter::All);
    }

    /// 芯片标签：秒 / 分 / 时 / 天四档。
    #[test]
    fn chip_seconds_minutes_hours_days() {
        assert_eq!(format_ttl_chip(0).as_deref(), Some("0s"));
        assert_eq!(format_ttl_chip(59).as_deref(), Some("59s"));
        assert_eq!(format_ttl_chip(60).as_deref(), Some("1m"));
        assert_eq!(format_ttl_chip(3599).as_deref(), Some("59m"));
        assert_eq!(format_ttl_chip(3600).as_deref(), Some("1h"));
        assert_eq!(format_ttl_chip(86_399).as_deref(), Some("23h"));
        assert_eq!(format_ttl_chip(86_400).as_deref(), Some("1d"));
    }

    /// 超过 99 天爬升到周、再到年（不把一年截成 `99d`）。
    #[test]
    fn chip_climbs_to_weeks_then_years_past_99_days() {
        assert_eq!(format_ttl_chip(99 * 86_400).as_deref(), Some("99d"));
        assert_eq!(format_ttl_chip(100 * 86_400).as_deref(), Some("14w"));
        assert_eq!(format_ttl_chip(365 * 86_400).as_deref(), Some("52w"));
        assert_eq!(format_ttl_chip(699 * 86_400).as_deref(), Some("99w"));
        assert_eq!(format_ttl_chip(700 * 86_400).as_deref(), Some("1y"));
        assert_eq!(format_ttl_chip(3 * 365 * 86_400).as_deref(), Some("3y"));
    }

    /// 永久键用无穷符号；键已消失不画芯片。
    #[test]
    fn chip_perm_and_missing() {
        assert_eq!(format_ttl_chip(-1).as_deref(), Some("∞"));
        assert!(format_ttl_chip(-2).is_none());
    }
}
