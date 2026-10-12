//! 数据库品牌 logo（dbx 项目同源，全彩 SVG）。
//!
//! 品牌标志有固有色（MySQL 蓝、Redis 红…），**不能**走 `DuckIcon` 的单色
//! `IconNamed` 通道（会被 `currentColor` 渲染成主题前景色）；这里走
//! `gpui::img(Resource::Embedded(path))`——`svg_renderer` 渲染时保留 SVG 内的
//! `fill` 颜色，品牌色随资源走。
//!
//! 资源由 [`ComponentAssets`] 一并并入应用 `AssetSource`（同 `logo.png` 的路径）。

use gpui::{IntoElement, Pixels, Styled as _, img, px};

/// 一张数据库品牌 logo。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DatabaseLogo {
    Clickhouse,
    Duckdb,
    Dynamodb,
    Elasticsearch,
    Influxdb,
    Mariadb,
    Mongodb,
    Mysql,
    Neo4j,
    Oracle,
    Postgres,
    Redis,
    Redshift,
    Sqlite,
    Sqlserver,
}

impl DatabaseLogo {
    /// 全部 logo（资源枚举与测试用）。
    pub const ALL: &'static [DatabaseLogo] = &[
        Self::Clickhouse,
        Self::Duckdb,
        Self::Dynamodb,
        Self::Elasticsearch,
        Self::Influxdb,
        Self::Mariadb,
        Self::Mongodb,
        Self::Mysql,
        Self::Neo4j,
        Self::Oracle,
        Self::Postgres,
        Self::Redis,
        Self::Redshift,
        Self::Sqlite,
        Self::Sqlserver,
    ];

    /// 嵌入资源路径。
    pub const fn asset_path(self) -> &'static str {
        match self {
            Self::Clickhouse => "duck/dblogos/clickhouse.svg",
            Self::Duckdb => "duck/dblogos/duckdb.svg",
            Self::Dynamodb => "duck/dblogos/dynamodb.svg",
            Self::Elasticsearch => "duck/dblogos/elasticsearch.svg",
            Self::Influxdb => "duck/dblogos/influxdb.svg",
            Self::Mariadb => "duck/dblogos/mariadb.svg",
            Self::Mongodb => "duck/dblogos/mongodb.svg",
            Self::Mysql => "duck/dblogos/mysql.svg",
            Self::Neo4j => "duck/dblogos/neo4j.svg",
            Self::Oracle => "duck/dblogos/oracle.svg",
            Self::Postgres => "duck/dblogos/postgres.svg",
            Self::Redis => "duck/dblogos/redis.svg",
            Self::Redshift => "duck/dblogos/redshift.svg",
            Self::Sqlite => "duck/dblogos/sqlite.svg",
            Self::Sqlserver => "duck/dblogos/sqlserver.svg",
        }
    }

    pub(crate) const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Clickhouse => include_bytes!("../assets/dblogos/clickhouse.svg"),
            Self::Duckdb => include_bytes!("../assets/dblogos/duckdb.svg"),
            Self::Dynamodb => include_bytes!("../assets/dblogos/dynamodb.svg"),
            Self::Elasticsearch => include_bytes!("../assets/dblogos/elasticsearch.svg"),
            Self::Influxdb => include_bytes!("../assets/dblogos/influxdb.svg"),
            Self::Mariadb => include_bytes!("../assets/dblogos/mariadb.svg"),
            Self::Mongodb => include_bytes!("../assets/dblogos/mongodb.svg"),
            Self::Mysql => include_bytes!("../assets/dblogos/mysql.svg"),
            Self::Neo4j => include_bytes!("../assets/dblogos/neo4j.svg"),
            Self::Oracle => include_bytes!("../assets/dblogos/oracle.svg"),
            Self::Postgres => include_bytes!("../assets/dblogos/postgres.svg"),
            Self::Redis => include_bytes!("../assets/dblogos/redis.svg"),
            Self::Redshift => include_bytes!("../assets/dblogos/redshift.svg"),
            Self::Sqlite => include_bytes!("../assets/dblogos/sqlite.svg"),
            Self::Sqlserver => include_bytes!("../assets/dblogos/sqlserver.svg"),
        }
    }

    /// 渲染为 `size` 见方的全彩品牌图（驱动选择卡 / 详情页头用）。
    #[must_use]
    pub fn element(self, size: Pixels) -> impl IntoElement {
        img(self.asset_path()).size(size)
    }
}

/// 卡片里的默认 logo 边长。
#[must_use]
pub fn card_logo_size() -> Pixels {
    px(22.)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use gpui::AssetSource as _;

    use crate::ComponentAssets;
    use crate::dblogos::DatabaseLogo;

    #[test]
    fn every_logo_path_is_unique_and_loads_svg() -> gpui::Result<()> {
        let mut seen = HashSet::new();
        for logo in DatabaseLogo::ALL {
            assert!(seen.insert(logo.asset_path()), "duplicate {logo:?}");
            let bytes = ComponentAssets.load(logo.asset_path())?;
            assert!(
                bytes
                    .is_some_and(|bytes| bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml")),
                "{logo:?} missing or not an svg"
            );
        }
        assert!(ComponentAssets.load("duck/dblogos/nope.svg")?.is_none());
        Ok(())
    }

    #[test]
    fn logos_are_listed_under_duck_dblogos() -> gpui::Result<()> {
        let listed = ComponentAssets.list("duck/dblogos/")?;
        assert_eq!(listed.len(), DatabaseLogo::ALL.len());
        Ok(())
    }
}
