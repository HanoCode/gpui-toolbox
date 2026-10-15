//! 键列表 / 键树的排序口径（移植自 zedis `views/key_tree/build.rs::KeySort`）。
//!
//! 本模块零网络、零 gpui：只描述「两行谁在前」，不关心行是列表项还是树节点。
//! 键树的兄弟排序与平铺列表的排序共用同一把尺子，两种形态下同一档位的读法一致。

/// 同层排序档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum KeySort {
    /// 按键名升序（缺省，也是服务端 `SCAN` 的自然顺序）。
    #[default]
    NameAsc,
    /// 按键名降序。
    NameDesc,
    /// 按剩余 TTL 升序（最先过期的在前）。
    TtlAsc,
    /// 按剩余 TTL 降序。
    TtlDesc,
}

impl KeySort {
    /// 全部档位（顺序即菜单顺序）。
    pub const ALL: [Self; 4] = [Self::NameAsc, Self::NameDesc, Self::TtlAsc, Self::TtlDesc];

    /// 稳定 wire 值（菜单项 id 用）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::NameAsc => "name_asc",
            Self::NameDesc => "name_desc",
            Self::TtlAsc => "ttl_asc",
            Self::TtlDesc => "ttl_desc",
        }
    }

    /// 解析 wire 值；未知值回落 [`KeySort::NameAsc`]。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "name_desc" => Self::NameDesc,
            "ttl_asc" => Self::TtlAsc,
            "ttl_desc" => Self::TtlDesc,
            _ => Self::NameAsc,
        }
    }

    /// 是否按 TTL 排（菜单按需隐藏 TTL 档时用得上）。
    #[must_use]
    pub const fn is_ttl(self) -> bool {
        matches!(self, Self::TtlAsc | Self::TtlDesc)
    }

    /// 两行的先后。
    ///
    /// **文件夹恒在键前面**（两个方向都是）：文件夹是容器，混在键里排会让人
    /// 找不到「往下钻」的入口。这条在 `NameAsc` 下与「按名排」结果一致，
    /// 在降序与 TTL 档下才显出差别。
    #[must_use]
    pub fn compare(self, a: Sortable<'_>, b: Sortable<'_>) -> std::cmp::Ordering {
        // 反向比较：`true`（文件夹）排前面。
        b.is_folder.cmp(&a.is_folder).then_with(|| match self {
            Self::NameAsc => a.label.cmp(b.label),
            Self::NameDesc => b.label.cmp(a.label),
            Self::TtlAsc => {
                let (group_a, ttl_a) = ttl_rank(a.ttl_secs);
                let (group_b, ttl_b) = ttl_rank(b.ttl_secs);
                group_a
                    .cmp(&group_b)
                    .then(ttl_a.cmp(&ttl_b))
                    // 同 TTL（多数是「都没有」）时按键名，否则每次重排顺序都可能不同。
                    .then_with(|| a.label.cmp(b.label))
            }
            Self::TtlDesc => {
                let (group_a, ttl_a) = ttl_rank(a.ttl_secs);
                let (group_b, ttl_b) = ttl_rank(b.ttl_secs);
                group_a
                    .cmp(&group_b)
                    .then(ttl_b.cmp(&ttl_a))
                    .then_with(|| a.label.cmp(b.label))
            }
        })
    }
}

/// 参与比较的一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sortable<'a> {
    /// 显示名（平铺列表里就是键名，树里是节点名）。
    pub label: &'a str,
    /// 是不是文件夹行（树里的容器节点）。平铺列表恒为 `false`。
    pub is_folder: bool,
    /// 剩余秒数（`-1` 永久 / `-2` 未知 / `None` 未取到，见 `crate::ttl`）。
    pub ttl_secs: Option<i64>,
}

/// TTL 名次：`(组, 秒)`。
///
/// 组 `0` = 有存活过期时间的键，组 `1` = 其余（永不过期、未知、文件夹）。
/// 组 `1` 在 **两个方向** 上都排在最后——「永不过期」不是一个很大的数字，
/// 把它放到降序的队首会让这一档真正要找的键（快过期的）沉到看不见的地方。
#[must_use]
pub fn ttl_rank(ttl_secs: Option<i64>) -> (u8, i64) {
    match ttl_secs {
        Some(ttl) if ttl >= 0 => (0, ttl),
        _ => (1, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::{KeySort, Sortable, ttl_rank};

    fn leaf<'a>(label: &'a str, ttl: Option<i64>) -> Sortable<'a> {
        Sortable {
            label,
            is_folder: false,
            ttl_secs: ttl,
        }
    }

    fn folder(label: &str) -> Sortable<'_> {
        Sortable {
            label,
            is_folder: true,
            ttl_secs: None,
        }
    }

    /// 名称两档：升 / 降，且文件夹恒在键之前。
    #[test]
    fn name_modes_and_folders_first() {
        assert_eq!(
            KeySort::NameAsc.compare(leaf("a", None), leaf("b", None)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            KeySort::NameDesc.compare(leaf("a", None), leaf("b", None)),
            std::cmp::Ordering::Greater
        );
        // 降序下文件夹仍然在前（"z" 的文件夹排在 "a" 的键前面）。
        assert_eq!(
            KeySort::NameDesc.compare(folder("z"), leaf("a", None)),
            std::cmp::Ordering::Less
        );
        // 升序下也是。
        assert_eq!(
            KeySort::NameAsc.compare(folder("z"), leaf("a", None)),
            std::cmp::Ordering::Less
        );
    }

    /// TTL 名次：有存活期的进组 0，其余（永久 / 未知 / 未取到）进组 1。
    #[test]
    fn ttl_rank_groups() {
        assert_eq!(ttl_rank(Some(0)), (0, 0));
        assert_eq!(ttl_rank(Some(30)), (0, 30));
        assert_eq!(ttl_rank(Some(-1)), (1, 0), "永不过期不参加秒数比较");
        assert_eq!(ttl_rank(Some(-2)), (1, 0));
        assert_eq!(ttl_rank(None), (1, 0));
    }

    /// TTL 升序：先过期的在前；永不过期 / 未知**两个方向**都垫底。
    #[test]
    fn ttl_asc_puts_expiring_first_and_never_last() {
        assert_eq!(
            KeySort::TtlAsc.compare(leaf("a", Some(30)), leaf("b", Some(600))),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            KeySort::TtlAsc.compare(leaf("a", Some(600)), leaf("b", Some(-1))),
            std::cmp::Ordering::Less,
            "快过期的排在永不过期之前"
        );
        assert_eq!(
            KeySort::TtlAsc.compare(leaf("a", None), leaf("b", Some(600))),
            std::cmp::Ordering::Greater,
            "未取到的 TTL 同样垫底"
        );
    }

    /// TTL 降序：长存活在前，但永不过期**依然垫底**（不是排到最前）。
    #[test]
    fn ttl_desc_keeps_never_expiring_last() {
        assert_eq!(
            KeySort::TtlDesc.compare(leaf("a", Some(600)), leaf("b", Some(30))),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            KeySort::TtlDesc.compare(leaf("a", Some(30)), leaf("b", Some(-1))),
            std::cmp::Ordering::Less,
            "永不过期在降序里也是最后，不是第一"
        );
    }

    /// 同 TTL 时按键名兜底，排序结果稳定。
    #[test]
    fn equal_ttl_falls_back_to_name() {
        assert_eq!(
            KeySort::TtlAsc.compare(leaf("a", None), leaf("b", None)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            KeySort::TtlDesc.compare(leaf("a", None), leaf("b", None)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            KeySort::TtlAsc.compare(leaf("a", Some(60)), leaf("b", Some(60))),
            std::cmp::Ordering::Less
        );
    }

    /// wire 往返与档位分类。
    #[test]
    fn roundtrip_and_ttl_classification() {
        for sort in KeySort::ALL {
            assert_eq!(KeySort::from_wire(sort.wire_name()), sort);
        }
        assert_eq!(KeySort::from_wire("nope"), KeySort::NameAsc);
        assert!(!KeySort::NameAsc.is_ttl() && !KeySort::NameDesc.is_ttl());
        assert!(KeySort::TtlAsc.is_ttl() && KeySort::TtlDesc.is_ttl());
    }
}
