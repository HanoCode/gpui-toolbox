//! **品牌表**：同一个线协议下面站着的那些牌子，以及每个牌子自己的连接参数。
//!
//! 表本体是生成的（`brands_gen.rs`，见 `scripts/gen_brands.py` 的头部注释），
//! 本模块只做三件事：给它类型、给它查询、给它到 [`DatabaseKind`] 的桥。
//!
//! # 为什么要有这张表
//!
//! 引擎档位只有 8 个（内建 7 + agent），而现实里的牌子有上百个 ——
//! `mysql` 一个协议下面站着 MySQL / MariaDB / TiDB / OceanBase / Doris / StarRocks …
//! 12 个牌子，它们的**缺省端口和惯用账号各不相同**（3306 / 3306 / 4000 / 2883 / 9030 …）。
//!
//! 档位表达不了这个粒度，所以品牌单独成表：界面拿它给出正确的名字 / 图标 / 端口 / 账号，
//! 档案里用一个 `profile` 键记住牌子（dbx 同一个字段叫 `driver_profile`）。
//! 这也是 dbx 的做法 —— 它的 `profiles/catalog.yaml` 就是这张表（109 条）。
//!
//! # 模式族（`family` / `mode`）
//!
//! 有些牌子**一次连接要先选模式**：OceanBase 有 MySQL 模式与 Oracle 模式，
//! GBase 有 8a（MySQL 系）与 8s（Informix 系），InterSystems 有 IRIS 与 Caché。
//! 同一个产品、不同的服务端协议 —— 连接参数自然也不同（OceanBase 两种模式的
//! 惯用账号就分别是 `root` 与 `SYS`）。
//!
//! 界面判据只有一条：**[`family_of`] 返回多于一条就画模式切换器**。族是数据，
//! 不推 —— 「同协议就是一族」是错的（Kafka / RocketMQ / RabbitMQ 同协议，
//! 但选哪个不是「模式」，而是「连哪台」）。

use duck_components::DatabaseLogo;

use super::DatabaseKind;
use super::brands_gen;

/// 品牌表里的一条。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Brand {
    /// 品牌键（dbx 品牌表的 id），落进 `ConnectionConfig::profile`。
    pub key: &'static str,
    /// 中文源串（界面过 `tr`）。
    pub label: &'static str,
    /// 走哪个引擎档位（[`DatabaseKind::wire_name`] 的同字面量）。
    pub kind: &'static str,
    /// agent 档的驱动键；内建档为空串。
    pub driver: &'static str,
    /// 模式族 id（单模式时等于 [`Self::key`]）。
    pub family: &'static str,
    /// 该品牌在本族里的模式名（单模式时为空串）。
    pub mode: &'static str,
    /// 缺省端口（`None` = 没有 TCP 端口）。
    pub port: Option<u16>,
    /// 惯用账号（空串 = 没有惯例）。
    pub user: &'static str,
    /// 该品牌的必需 URL 参数（如星环 Argo 的 `auth=noSasl`）。
    pub url_params: &'static str,
    /// 品牌图标；`None` = 本仓图标表里没有它（界面回落主题单色图标）。
    pub logo: Option<DatabaseLogo>,
}

impl Brand {
    /// 引擎档位。
    ///
    /// ⚠️ `from_wire` 对未知值是**回落 SQLite**（它的既有约定，见该函数文档）。
    /// 这张表是生成的、取值受生成脚本守着，所以这里不额外兜底 ——
    /// 真出了未知值，`brands_gen.rs` 里的自检用例会先红。
    pub(super) fn kind(self) -> DatabaseKind {
        DatabaseKind::from_wire(self.kind)
    }

    /// agent 档的驱动键（内建档返回 `None`）。
    ///
    /// 「要不要去驱动商店装点什么」问的就是它 —— 别另立一个 `needs_driver()` 布尔：
    /// 两者说的是同一件事，分开写迟早分叉。
    pub(super) fn driver(self) -> Option<&'static str> {
        (!self.driver.is_empty()).then_some(self.driver)
    }
}

/// 全部品牌（顺序：族、模式、键）。
pub(super) fn all() -> &'static [Brand] {
    &brands_gen::BRANDS
}

/// 按品牌键查。
pub(super) fn brand(key: &str) -> Option<&'static Brand> {
    all().iter().find(|brand| brand.key == key)
}

/// **模式族**：同族的全部品牌（含自己），单模式品牌返回只含自己的一条。
///
/// 这是界面的判据：长度 > 1 才画模式切换器（见模块文档）。
pub(super) fn family_of(key: &str) -> &'static [&'static Brand] {
    let Some(me) = brand(key) else {
        return &[];
    };
    // 只在同族里筛，且顺序沿用表内顺序（生成时已按族 / 模式排好）。
    // 返回的是**静态切片**：调用方在 `render` 里用，不该为它分配一份 `Vec`。
    family_slice(me.family)
}

/// 族 id → 该族的品牌切片（带缓存，避免每次 `render` 都扫一遍表）。
fn family_slice(family: &str) -> &'static [&'static Brand] {
    // 表的规模是几十条，且 `render` 每帧都会问 —— 用 `OnceLock` 按族缓存。
    // 缓存的键是族 id，值是**过滤后的引用切片**，进程生命周期内有效。
    static CACHE: std::sync::OnceLock<
        std::collections::HashMap<&'static str, Vec<&'static Brand>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let mut map: std::collections::HashMap<&'static str, Vec<&'static Brand>> =
            std::collections::HashMap::new();
        for brand in all() {
            map.entry(brand.family).or_default().push(brand);
        }
        map
    });
    cache.get(family).map_or(&[], Vec::as_slice)
}

/// 由档位 + 驱动键反查品牌（**没有 `profile` 的旧档案走这条**）。
///
/// 只有「一对一」的才给答案：`mysql` 协议下面站着 12 个牌子，单凭档位无法知道
/// 用户当年选的是哪个 —— 那就**不给品牌**，界面回落成档位自己的名字与图标，
/// 总好过随便挑一个牌子把 MySQL 显示成 MariaDB。
pub(super) fn infer(kind: DatabaseKind, driver: Option<&str>) -> Option<&'static Brand> {
    let mut found = None;
    for brand in all() {
        if brand.kind() != kind {
            continue;
        }
        if brand.driver() != driver {
            continue;
        }
        // 同族的多条（GBase 8a/8s 各自一条驱动，算不同品牌；h2/h2-legacy 共用一个驱动键
        // 但不同 URL 形态）—— 有歧义就不认。
        if found.is_some() {
            return None;
        }
        found = Some(brand);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 品牌键唯一 —— 重复键会让反查挑中错的那条。
    #[test]
    fn keys_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for brand in all() {
            assert!(seen.insert(brand.key), "品牌键重复：{}", brand.key);
        }
    }

    /// 档位字面量都解析得回来，且「agent 档 ⟺ 有驱动键」两边一致 ——
    /// 错配会让界面既画不出「去装驱动」，也问不到端口。
    #[test]
    fn kinds_round_trip() {
        for brand in all() {
            assert_eq!(
                brand.kind().wire_name(),
                brand.kind,
                "{} 的档位 {} 不是已知取值",
                brand.key,
                brand.kind
            );
            assert_eq!(
                brand.kind() == DatabaseKind::Agent,
                brand.driver().is_some(),
                "{} 的档位与驱动键对不上（agent 档必须有驱动键）",
                brand.key
            );
        }
    }

    /// 族里至少包含自己，且族内模式名不重复（重复了切换器会画两个一样的按钮）。
    #[test]
    fn families_are_well_formed() {
        for brand in all() {
            let family = family_of(brand.key);
            assert!(
                family.iter().any(|item| item.key == brand.key),
                "{} 不在自己的族里",
                brand.key
            );
            let mut modes = std::collections::HashSet::new();
            for item in family {
                let mode = if item.mode.is_empty() {
                    item.key
                } else {
                    item.mode
                };
                assert!(
                    modes.insert(mode),
                    "族 {} 里模式名重复：{mode}",
                    brand.family
                );
            }
        }
    }

    /// **模式族的关键一条**：OceanBase 的两种模式必须同族、且走不同的档位 ——
    /// 这正是「同一个牌子两条连法」的样板（MySQL 模式走内建 mysql 协议、
    /// Oracle 模式走它自己的 agent）。
    #[test]
    fn oceanbase_has_two_modes_on_two_tracks() {
        let family = family_of("oceanbase");
        assert_eq!(family.len(), 2, "{family:?}");
        let mysql_mode = brand("oceanbase").expect("oceanbase");
        let oracle_mode = brand("oceanbase-oracle").expect("oceanbase-oracle");
        assert_eq!(mysql_mode.kind(), DatabaseKind::Mysql);
        assert!(mysql_mode.driver().is_none());
        assert_eq!(oracle_mode.kind(), DatabaseKind::Agent);
        assert_eq!(oracle_mode.driver(), Some("oceanbase-oracle"));
        // 两种模式的端口相同（同一个服务端），账号不同（root / SYS）。
        assert_eq!(mysql_mode.port, Some(2883));
        assert_eq!(oracle_mode.port, Some(2883));
        assert_eq!(mysql_mode.user, "root");
        assert_eq!(oracle_mode.user, "SYS");
    }

    /// 单模式品牌不会画出切换器。
    #[test]
    fn single_mode_brands_have_a_family_of_one() {
        // ⚠️ 键是**品牌键**，不是驱动键：达梦的品牌键是 `dm`（品牌表按 dbx 的
        // 「商店里那一栏叫什么」编号），它的驱动键才是 `dameng`。
        for key in ["postgres", "mysql", "mariadb", "tidb", "dm", "db2", "sundb"] {
            let brand = brand(key).unwrap_or_else(|| panic!("品牌表里没有 {key}"));
            let family = family_of(key);
            assert_eq!(family.len(), 1, "{key} 不该有模式切换器：{family:?}");
            assert_eq!(brand.mode, "");
        }
    }

    /// 反查只认没有歧义的那些。
    #[test]
    fn inference_is_unambiguous_only() {
        // 只有一条品牌的内建协议 → 认得出来。
        assert_eq!(
            infer(DatabaseKind::ClickHouse, None).map(|brand| brand.key),
            Some("clickhouse")
        );
        // agent 的达梦只有一条驱动 → 认得出来（驱动键 `dameng`，品牌键 `dm`）。
        assert_eq!(
            infer(DatabaseKind::Agent, Some("dameng")).map(|brand| brand.key),
            Some("dm")
        );
        assert_eq!(
            infer(DatabaseKind::Agent, Some("db2")).map(|brand| brand.key),
            Some("db2")
        );
        // 一个驱动键对多个品牌（H2 的 2.x / legacy 共用一个驱动）→ 不猜。
        assert!(infer(DatabaseKind::Agent, Some("h2")).is_none());
        // 一个协议对多个内建品牌 → 不猜。MySQL 协议下站着 12 个牌子、
        // PostgreSQL 协议下站着 4 个（cloudberry / opentenbase / cockroachdb …），
        // 单凭档位无法知道用户当年选的是哪一个。
        assert!(infer(DatabaseKind::Mysql, None).is_none());
        assert!(infer(DatabaseKind::Postgres, None).is_none());
        // 没见过的驱动键 → 没有品牌。
        assert!(infer(DatabaseKind::Agent, Some("no-such-driver")).is_none());
    }

    /// **模式族的第二种形态**：一族两条都在 agent 轨、但各有一条驱动
    /// （GBase 8a 是 MySQL 系、8s 是 Informix 系，不是同一个协议）。
    #[test]
    fn gbase_modes_are_two_agent_tracks_on_two_ports() {
        let family = family_of("gbase8a");
        assert_eq!(family.len(), 2, "{family:?}");
        let eight_a = brand("gbase8a").expect("gbase8a");
        let eight_s = brand("gbase8s").expect("gbase8s");
        assert_eq!(eight_a.driver(), Some("gbase8a"));
        assert_eq!(eight_s.driver(), Some("gbase8s"));
        // 端口不是同一个：8a 是 5258，8s 是 9088（生成脚本修掉的那个 bug）。
        assert_eq!(eight_a.port, Some(5258));
        assert_eq!(eight_s.port, Some(9088));
    }
}
