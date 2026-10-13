//! 第三方客户端连接档案的导入（DBeaver / TablePlus）。
//!
//! 只做**读**：把别人家的档案映射成本仓的 [`ConnectionConfig`]，不动对方的文件。
//! 口令一律不带过来（各家存法不同，多数还是加密的）——导入后由用户自己补。
//!
//! 纯函数、零 I/O：形状认不出来就报 [`ImportError`]，**不猜、也不「尽力而为」地
//! 编造连接**（一个猜出来的 host 比一条导入失败更难排查）。文案由界面层给。
//!
//! 认得的两种形状：
//! - **DBeaver** `data-sources.json`：根上有 `connections` 字典，每条含 `provider` 与
//!   `configuration{ host, port, database, url, user }`。各版本字段有出入，
//!   所以 `url` 里的 JDBC 串是兜底来源。
//! - **TablePlus** `.tableplusconnection`：字段平铺在根上，
//!   `Driver` / `Host` / `Port` / `User` / `Database` / `Name` / `SSLMode`。

use serde_json::Value as Json;

use crate::config::{ConnectionConfig, DatabaseKind, TlsMode};

/// 认出来的档案来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    /// DBeaver 的 `data-sources.json`。
    DBeaver,
    /// TablePlus 的 `.tableplusconnection`。
    TablePlus,
}

/// 导入失败的原因（界面层翻文案，内核不吐成品中文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportError {
    /// 内容不是合法 JSON。
    NotJson,
    /// 是 JSON，但形状既不像 DBeaver 也不像 TablePlus。
    Unrecognised,
    /// 认出来了，但一条都没有本仓支持的引擎（Oracle / Redis / DynamoDB 之类会被跳过）。
    NoSupportedConnections,
}

/// 从第三方档案里读出连接。
///
/// 返回的 [`ConnectionConfig::id`] 一律是空串：id 的分配归调用方（它才知道现有连接
/// 占了哪些 id），这里不生成「看起来像但不保证唯一」的临时值。
pub fn import_third_party(
    json: &str,
) -> Result<(ImportSource, Vec<ConnectionConfig>), ImportError> {
    let root: Json = serde_json::from_str(json).map_err(|_| ImportError::NotJson)?;
    // 两种形状各试一次：`Some(空)` = 形状认出来了但没有本仓支持的引擎，
    // `None` = 根本不是这一家的档案。DBeaver 先试（它一份文件可能带多条连接）。
    let (source, connections) = match (from_dbeaver(&root), from_tableplus(&root)) {
        (Some(connections), _) => (ImportSource::DBeaver, connections),
        (None, Some(connections)) => (ImportSource::TablePlus, connections),
        (None, None) => return Err(ImportError::Unrecognised),
    };
    if connections.is_empty() {
        return Err(ImportError::NoSupportedConnections);
    }
    Ok((source, connections))
}

/// DBeaver `data-sources.json`：`connections` 是「id → 条目」的字典。
///
/// 认不出来（没有 `connections` 对象）返回 `None`；认得出来但某条不支持就**跳过那一条**，
/// 不让一颗坏连接的螺丝卡住整份档案。
fn from_dbeaver(root: &Json) -> Option<Vec<ConnectionConfig>> {
    let connections = root.get("connections")?.as_object()?;
    let mut out = Vec::new();
    for entry in connections.values() {
        // 老版本只写 `driver`（形如 `postgres-jdbc-42`），取第一段当 provider。
        let provider = entry
            .get("provider")
            .and_then(Json::as_str)
            .or_else(|| entry.get("driver").and_then(Json::as_str));
        let Some(kind) = provider.and_then(kind_of_provider) else {
            continue;
        };
        let configuration = entry.get("configuration").unwrap_or(&Json::Null);
        let url = text(configuration, "url").unwrap_or_default();
        let (url_host, url_port, url_database) = parse_jdbc(&url);
        let mut config = ConnectionConfig {
            kind,
            host: text(configuration, "host").or(url_host).unwrap_or_default(),
            port: text(configuration, "port")
                .and_then(|port| port.parse::<u16>().ok())
                .or(url_port)
                .or_else(|| kind.default_port()),
            database: text(configuration, "database")
                .or(url_database)
                .unwrap_or_default(),
            user: text(configuration, "user").unwrap_or_default(),
            ..ConnectionConfig::default()
        };
        config.name = text(entry, "name").unwrap_or_else(|| fallback_name(&config));
        // DBeaver 把 TLS 开关记在 `configuration.ssl` 上；开了就按「校验证书」档带过来
        // （比默认禁掉安全，用户建连时若证书不通会立刻看见，而不是静默明文连上）。
        if configuration
            .get("ssl")
            .and_then(Json::as_bool)
            .unwrap_or(false)
        {
            config.tls = TlsMode::Trust;
        }
        out.push(config);
    }
    Some(out)
}

/// TablePlus `.tableplusconnection`：一份文件一条连接，字段平铺在根上。
///
/// 与 [`from_dbeaver`] 同构：`None` = 不是 TablePlus 的档案（没有 `Driver`）；
/// `Some(空)` = 是 TablePlus 的档案，但这个引擎本仓不支持。
fn from_tableplus(root: &Json) -> Option<Vec<ConnectionConfig>> {
    let driver = root.get("Driver").and_then(Json::as_str)?;
    let Some(kind) = kind_of_tableplus_driver(driver) else {
        return Some(Vec::new());
    };
    let host = text(root, "Host").unwrap_or_default();
    if kind.needs_host() && host.is_empty() {
        // 缺主机名的远端连接导进来也连不上，当作「没有可用连接」。
        return Some(Vec::new());
    }
    let mut config = ConnectionConfig {
        kind,
        host,
        port: text(root, "Port")
            .and_then(|port| port.parse::<u16>().ok())
            .or_else(|| kind.default_port()),
        database: text(root, "Database").unwrap_or_default(),
        user: text(root, "User").unwrap_or_default(),
        ..ConnectionConfig::default()
    };
    config.name = text(root, "Name").unwrap_or_else(|| fallback_name(&config));
    // `SSLMode` 常见取值 `disable` / `prefer` / `require` / `verify-ca` / `verify-full`；
    // 除 `disable` 外一律按「校验证书」带过来（保守：宁可多验一次）。
    if let Some(mode) = text(root, "SSLMode")
        && mode != "disable"
    {
        config.tls = TlsMode::Trust;
    }
    Some(vec![config])
}

/// 引擎名 → 本仓支持的档位；认不出来（Oracle / Redis / DynamoDB …）返回 `None`。
///
/// 覆盖 [`DatabaseKind::ALL`] 的**全部七档**：DBeaver 的 provider 多数是引擎的小写名
/// （`clickhouse` / `sqlserver` / `mongodb` / `elasticsearch`），SQL Server 另收 `mssql`
/// 这个常见别名。
fn kind_of_provider(provider: &str) -> Option<DatabaseKind> {
    // DBeaver 的 `driver` 形如 `postgres-jdbc-42`，只取第一段比对。
    let provider = provider.split('-').next().unwrap_or(provider);
    match provider.to_ascii_lowercase().as_str() {
        "postgresql" | "postgres" => Some(DatabaseKind::Postgres),
        "mysql" | "mariadb" => Some(DatabaseKind::Mysql),
        "sqlite" => Some(DatabaseKind::Sqlite),
        "clickhouse" => Some(DatabaseKind::ClickHouse),
        "sqlserver" | "mssql" => Some(DatabaseKind::SqlServer),
        "mongodb" => Some(DatabaseKind::Mongo),
        "elasticsearch" => Some(DatabaseKind::Elasticsearch),
        _ => None,
    }
}

/// TablePlus 的 `Driver` 取值（`PostgreSQL` / `MySQL` / `MariaDB` / `SQLite` /
/// `ClickHouse` / `SQL Server` / `MongoDB` / `Elasticsearch` …）。
///
/// 与人读的名字比对：`SQL Server` 带空格，先去掉全部空格再小写化，`MSSQL` / `Mongo`
/// 这类别名一并收下。覆盖 [`DatabaseKind::ALL`] 的全部七档。
fn kind_of_tableplus_driver(driver: &str) -> Option<DatabaseKind> {
    let driver = driver.to_ascii_lowercase().replace(' ', "");
    match driver.as_str() {
        "postgresql" | "postgres" => Some(DatabaseKind::Postgres),
        "mysql" | "mariadb" => Some(DatabaseKind::Mysql),
        "sqlite" => Some(DatabaseKind::Sqlite),
        "clickhouse" => Some(DatabaseKind::ClickHouse),
        "sqlserver" | "mssql" => Some(DatabaseKind::SqlServer),
        "mongodb" | "mongo" => Some(DatabaseKind::Mongo),
        "elasticsearch" => Some(DatabaseKind::Elasticsearch),
        _ => None,
    }
}

/// 取一个字符串字段（数字也收，DBeaver 的 `port` 两种写法都见过）。
fn text(value: &Json, key: &str) -> Option<String> {
    let field = value.get(key)?;
    if let Some(text) = field.as_str() {
        return (!text.trim().is_empty()).then(|| text.to_owned());
    }
    field.as_u64().map(|number| number.to_string())
}

/// 没给名字时按「主机 / 库名」拼一个，至少让列表里认得出是哪条。
fn fallback_name(config: &ConnectionConfig) -> String {
    match config.kind {
        DatabaseKind::Sqlite => config.database.clone(),
        _ => format!("{}@{}", config.database, config.host),
    }
}

/// 从 JDBC 串里取 `(host, port, database)`。
///
/// 形状：`jdbc:<engine>://<host>[:<port>][/<database>][?<params>]`；
/// SQLite 是 `jdbc:sqlite:<path>`（没有 `//`，冒号后就是本地路径）。
fn parse_jdbc(url: &str) -> (Option<String>, Option<u16>, Option<String>) {
    let Some(rest) = url.strip_prefix("jdbc:") else {
        return (None, None, None);
    };
    // `jdbc:<engine>:<地址>`：引擎名之后才是地址。
    let Some((_, address)) = rest.split_once(':') else {
        return (None, None, None);
    };
    let Some(address) = address.strip_prefix("//") else {
        // 没有 `//`：按本地文件路径处理（sqlite 就是这个形状）。
        return (
            None,
            None,
            (!address.is_empty()).then(|| address.to_owned()),
        );
    };
    let address = address.split(['?', '#']).next().unwrap_or(address);
    let (authority, database) = match address.split_once('/') {
        Some((authority, database)) => (
            authority,
            (!database.is_empty()).then(|| database.to_owned()),
        ),
        None => (address, None),
    };
    // 用户信息（`user:pass@host`）在这里没有意义，出现也剥掉。
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host.to_owned(), port.parse::<u16>().ok()),
        None => (authority.to_owned(), None),
    };
    ((!host.is_empty()).then_some(host), port, database)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DBeaver 的真实形状：`connections` 字典 + `configuration`。
    const DBEAVER: &str = r#"{
      "folders": {},
      "connections": {
        "postgres-jdbc-18f1": {
          "provider": "postgresql",
          "driver": "postgres-jdbc",
          "name": "analytics",
          "save-password": true,
          "configuration": {
            "host": "db.internal",
            "port": "5433",
            "database": "analytics",
            "url": "jdbc:postgresql://db.internal:5433/analytics",
            "user": "reporter",
            "ssl": true
          }
        },
        "mysql5-2": {
          "provider": "mysql",
          "name": "shop",
          "configuration": {
            "url": "jdbc:mysql://127.0.0.1:3306/shop?useSSL=false",
            "user": "root"
          }
        },
        "oracle-thin-9": {
          "provider": "oracle",
          "name": "legacy",
          "configuration": { "host": "ora", "port": "1521", "database": "XE" }
        }
      }
    }"#;

    #[test]
    fn dbeaver_entries_map_and_unsupported_providers_are_skipped() {
        let (source, connections) = import_third_party(DBEAVER).expect("应当认得 DBeaver");
        assert_eq!(source, ImportSource::DBeaver);
        // Oracle 那条被跳过，剩下两条。
        assert_eq!(connections.len(), 2);

        let pg = connections
            .iter()
            .find(|cfg| cfg.name == "analytics")
            .expect("postgres 那条应在");
        assert_eq!(pg.kind, DatabaseKind::Postgres);
        assert_eq!(pg.host, "db.internal");
        assert_eq!(pg.port, Some(5433));
        assert_eq!(pg.database, "analytics");
        assert_eq!(pg.user, "reporter");
        assert_eq!(pg.tls, TlsMode::Trust, "`ssl: true` 要带过来");
        assert!(pg.id.is_empty(), "id 由调用方分配");

        // 只给了 `url`：host / port / 库名都要从 JDBC 串里推出来，
        // 并且 `?useSSL=false` 之类的查询串不能被算进库名。
        let mysql = connections
            .iter()
            .find(|cfg| cfg.name == "shop")
            .expect("mysql 那条应在");
        assert_eq!(mysql.kind, DatabaseKind::Mysql);
        assert_eq!(mysql.host, "127.0.0.1");
        assert_eq!(mysql.port, Some(3306));
        assert_eq!(mysql.database, "shop");
        assert_eq!(mysql.tls, TlsMode::Disable);
    }

    /// TablePlus：字段平铺在根上，一份文件一条。
    #[test]
    fn tableplus_single_connection_round_trips() {
        let json = r#"{
          "Driver": "PostgreSQL",
          "Name": "staging",
          "Host": "10.0.0.7",
          "Port": 5432,
          "User": "deploy",
          "Database": "app",
          "SSLMode": "require"
        }"#;
        let (source, connections) = import_third_party(json).expect("应当认得 TablePlus");
        assert_eq!(source, ImportSource::TablePlus);
        let config = &connections[0];
        assert_eq!(config.kind, DatabaseKind::Postgres);
        assert_eq!(config.name, "staging");
        assert_eq!(config.host, "10.0.0.7");
        assert_eq!(config.port, Some(5432));
        assert_eq!(config.user, "deploy");
        assert_eq!(config.database, "app");
        assert_eq!(config.tls, TlsMode::Trust);
    }

    /// 认不出来的输入要给得出「为什么不行」，而不是给一条编出来的连接。
    #[test]
    fn unrecognised_input_reports_its_reason() {
        assert_eq!(
            import_third_party("not json at all"),
            Err(ImportError::NotJson)
        );
        // 合法 JSON，但不是任何一家的连接档案形状。
        assert_eq!(
            import_third_party(r#"{"hello":"world"}"#),
            Err(ImportError::Unrecognised)
        );
        // 是 DBeaver 的形状，但里面只有不支持的引擎。
        assert_eq!(
            import_third_party(r#"{"connections":{"a":{"provider":"oracle"}}}"#),
            Err(ImportError::NoSupportedConnections)
        );
        // TablePlus 的壳子但引擎不支持 —— 形状认得出，是「没有支持的连接」。
        // （Redis 由独立的 `duck-redis` 模块负责，不在本 crate 的 db 引擎矩阵内。）
        assert_eq!(
            import_third_party(r#"{"Driver":"Redis","Host":"cache"}"#),
            Err(ImportError::NoSupportedConnections)
        );
    }

    /// 七档引擎都能从两家的档案里认出来。
    ///
    /// 回归护栏：映射表曾经只认 postgresql / mysql / mariadb / sqlite，ClickHouse /
    /// SQL Server / MongoDB / Elasticsearch 的档案会被整份当作「不支持的引擎」跳过。
    #[test]
    fn every_supported_engine_maps_from_both_sources() {
        // DBeaver：`provider` 是引擎的小写名；SQL Server 常见 `sqlserver-jdbc-*`（取第一段）。
        let dbeaver = r#"{"connections":{
          "a":{"provider":"clickhouse","configuration":{"host":"ch","port":"8123"}},
          "b":{"provider":"sqlserver-jdbc-42","configuration":{"host":"mssql","port":"1433"}},
          "c":{"provider":"mongodb","configuration":{"host":"mongo","port":"27017"}},
          "d":{"provider":"elasticsearch","configuration":{"host":"es","port":"9200"}}
        }}"#;
        let (source, connections) = import_third_party(dbeaver).expect("DBeaver 四档都应认得");
        assert_eq!(source, ImportSource::DBeaver);
        assert_eq!(connections.len(), 4, "四条都要导入，不能跳过任何一条");
        let kinds: Vec<DatabaseKind> = connections.iter().map(|cfg| cfg.kind).collect();
        for expected in [
            DatabaseKind::ClickHouse,
            DatabaseKind::SqlServer,
            DatabaseKind::Mongo,
            DatabaseKind::Elasticsearch,
        ] {
            assert!(kinds.contains(&expected), "{expected:?} 应当被导入");
        }

        // TablePlus：`Driver` 是人读的名字（`SQL Server` 带空格），一份文件一条。
        for (driver, expected) in [
            ("ClickHouse", DatabaseKind::ClickHouse),
            ("SQL Server", DatabaseKind::SqlServer),
            ("MSSQL", DatabaseKind::SqlServer),
            ("MongoDB", DatabaseKind::Mongo),
            ("Elasticsearch", DatabaseKind::Elasticsearch),
        ] {
            let json = format!(r#"{{"Driver":"{driver}","Host":"db.local","Port":1}}"#);
            let (source, connections) = import_third_party(&json).expect("TablePlus 应认得");
            assert_eq!(source, ImportSource::TablePlus);
            assert_eq!(
                connections[0].kind, expected,
                "{driver} 应映射成 {expected:?}"
            );
        }
    }

    /// JDBC 串的各种形状（含 sqlite 的本地路径与带查询串的 URL）。
    #[test]
    fn jdbc_urls_are_parsed_conservatively() {
        assert_eq!(
            parse_jdbc("jdbc:postgresql://host:5433/db"),
            (Some("host".to_owned()), Some(5433), Some("db".to_owned()))
        );
        assert_eq!(
            parse_jdbc("jdbc:mysql://host/db?useSSL=false&serverTimezone=UTC"),
            (Some("host".to_owned()), None, Some("db".to_owned()))
        );
        // 没有库名 / 端口。
        assert_eq!(
            parse_jdbc("jdbc:postgresql://host"),
            (Some("host".to_owned()), None, None)
        );
        // SQLite：冒号后就是本地路径，没有 `//`。
        assert_eq!(
            parse_jdbc("jdbc:sqlite:/Users/me/app.db"),
            (None, None, Some("/Users/me/app.db".to_owned()))
        );
        // 不是 JDBC 串：什么都不给。
        assert_eq!(parse_jdbc("postgres://host/db"), (None, None, None));
    }
}
