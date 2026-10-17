//! 驱动目录：商店列表与分类侧栏的**唯一数据源**。
//!
//! 表本身是静态数据（见 `catalog_gen.rs` 的生成说明），本模块只做两件事：
//! 给它类型、给它查询。
//!
//! # 为什么是编译期静态表而不是运行时读文件
//!
//! 商店列表要在**没有网络**时也能画出来（离线环境靠它挑要导入哪个包），而分类归属
//! 又不会随版本变；把 55 条编进二进制，换掉的是「多一份要跟着发布的资源文件」。
//! 真正的动态部分（版本、工件大小、是否有新版）全在 [`crate::registry`] 里。
//!
//! # 名称是**源串**，不是成品文案
//!
//! 表里的 `label` 与分类 `label` 都是中文源串（如 `"达梦 DM8"` / `"国产数据库"`），
//! 界面渲染时过 `tr(cx, label)`，英文界面显示 `locales/en.toml` 里的译文。
//! 内核不查表、不拼文案——这条与全仓 i18n 口径一致（`AGENTS.md` §3.3）。

use crate::catalog_gen;

/// 商店侧栏的一个分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverCategory {
    /// 稳定键（`sql` / `analytics` / …），落盘与筛选用。
    pub key: &'static str,
    /// 中文源串（界面过 `tr`）。
    pub label: &'static str,
    /// 展示顺序。
    pub order: u16,
}

/// 商店列表里的一条驱动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverEntry {
    /// 注册表键（`drivers.<key>`），也是安装目录名。
    pub key: &'static str,
    /// 中文源串（界面过 `tr`）。
    pub label: &'static str,
    /// 所属分类键（对应 [`DriverCategory::key`]）。
    pub category: &'static str,
    /// 展示顺序（沿用 dbx 的驱动商店顺序）。
    pub order: u16,
    /// 缺省端口（`None` = 没有 TCP 端口：文件库、云端 REST 端点）。
    ///
    /// ⚠️ **一个协议一条端口是错的**：端口随**品牌**走，不随协议走 ——
    /// Kafka 9092 / RocketMQ 9876 / RabbitMQ 5672 在 dbx 侧都是 `mq` 一个 dbType，
    /// 端口却是三个数。本表的值取自 dbx 的**品牌表**（`profiles/catalog.yaml`），
    /// 不是连接类型 YAML 里那个挂在协议上的 `defaultPort`（生成脚本里解释了缘由）。
    pub default_port: Option<u16>,
    /// 品牌的**惯用管理账号**（dbx 品牌表的 `user`），新建连接时预填。
    ///
    /// 空串是合法值（不少品牌没有惯例账号，如 Redis / ClickHouse 的默认用户名
    /// 各版本不同）。界面把空串当「不预填」，占位符露出来 —— 见 `EngineRef::default_user`。
    pub default_user: &'static str,
    /// 品牌的**必需 URL 参数**（dbx 品牌表的 `urlParams`），新建连接时预填。
    ///
    /// 这是「不填就根本连不上」的那一类：Argo / Impala 要 `auth=noSasl`、
    /// Kyuubi 要 `auth=NONE` —— 少了它，服务端直接拒。多数品牌是空串。
    ///
    /// 落到线上是 `ConnectRequest::url_params`（JDBC URL 里 `?` 之后那截）。
    pub default_url_params: &'static str,
    /// 运行方式（dbx 的 `runtimeMode`），取值见 [`RuntimeMode`]。
    pub runtime_mode: &'static str,
    /// 能不能**当数据源浏览**（dbx 的 `capabilities.metadataBrowse`）。
    ///
    /// 这是「要不要把它放进连接表单的引擎下拉」的判据。别拿 `queryExecution` 顶替：
    /// etcd / Kafka 的 agent 确实能「执行」（发命令），但它们**没有 schema 树**，
    /// 接进目录页就是一棵永远空的树 —— 那是「不支持」，不是「执行失败」。
    pub metadata_browse: bool,
}

/// [`DriverEntry::runtime_mode`] 的取值。
///
/// 这不是我们的选择，是对端的既有事实（dbx 的 `runtimeMode`），四个值各对应一种
/// 「工件在哪」，直接决定我们**能不能装**：
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    /// 独立子进程（`agent`）。**只有这一档我们能拿到工件** —— 见 [`crate::registry`]。
    Agent,
    /// 编进 dbx 宿主进程的 Rust crate（`native`）。
    ///
    /// ⚠️ **这一档不等于「注册表里没有」**（本文件曾经这么写，是错的）。实测
    /// `agent-registry.json`（55 条，与 [`catalog_gen`] 的键集合逐条相同）：
    /// `rabbitmq` / `rocketmq` 声明 `native`，却**有六平台齐全的原生工件**；
    /// 而 `kafka` / `sqlserver-legacy` 在注册表里有条目、**没有 native 工件**
    /// （它们走的是 `jre` + `jar` 那一档）。真正「连条目都没有」的是
    /// PostgreSQL / MySQL / Redis 这类 —— 那才是「别去商店里找」的对象。
    ///
    /// 一句话：**能不能下载看注册表，不看 `runtimeMode`**（两者的差集见
    /// [`agent_sources`] 的文档）。
    Native,
    /// 本地文件库（`file`）：SQLite / DuckDB，开文件就用，不需要装驱动。
    ///
    /// ⚠️ 后半句对 SQLite 成立、对 **DuckDB 不成立**：`duckdb` 在注册表里是一个
    /// 正经工件（0.1.32，macos-aarch64 / macos-x64 / linux-aarch64 / linux-x64 /
    /// windows-aarch64 / windows-x64 六平台齐全），dbx 的 `duckdb.yaml` 也写着
    /// `managedDrivers[0].storeVisible: true` —— 意思是「从商店装」。
    /// 「开文件就用」说的是**它的进程**，那个进程本身得先下下来。
    /// `sqlite-worker` 同理（只有两个 linux 工件）。
    File,
    /// 通用 JDBC 插件（`external`）：要用户自备 jar，我们只能给托管 JRE。
    External,
}

impl RuntimeMode {
    /// 解析 dbx 的取值；未知值按 [`Agent`](Self::Agent) 处理。
    ///
    /// 回落成 `Agent` 是有意的：这是**四个值里唯一有工件可装**的一档，
    /// 未知值把它当成「得装点什么」最保守；当成 `Native` 会让商店少画一行。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "native" => Self::Native,
            "file" => Self::File,
            "external" => Self::External,
            _ => Self::Agent,
        }
    }

    /// wire 值（落盘 / 日志用）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Native => "native",
            Self::File => "file",
            Self::External => "external",
        }
    }

    /// 这一档的驱动**要从商店下载**吗。
    #[must_use]
    pub const fn needs_install(self) -> bool {
        matches!(self, Self::Agent | Self::External)
    }
}

impl DriverEntry {
    /// 运行方式。
    #[must_use]
    pub fn runtime(self) -> RuntimeMode {
        RuntimeMode::from_wire(self.runtime_mode)
    }

    /// 这条驱动能不能作为**数据源**接进来（见 [`DriverEntry::metadata_browse`]）。
    #[must_use]
    pub const fn is_data_source(self) -> bool {
        self.metadata_browse
    }
}

/// 全部分类（顺序即展示顺序）。
#[must_use]
pub fn categories() -> &'static [DriverCategory] {
    &catalog_gen::CATEGORIES
}

/// 全部驱动条目。
#[must_use]
pub fn catalog() -> &'static [DriverEntry] {
    &catalog_gen::ENTRIES
}

/// 按注册表键查一条驱动。
#[must_use]
pub fn entry(key: &str) -> Option<&'static DriverEntry> {
    catalog().iter().find(|entry| entry.key == key)
}

/// 某个分类下的驱动（保持表内顺序）。
pub fn entries_in(category_key: &str) -> impl Iterator<Item = &'static DriverEntry> {
    catalog()
        .iter()
        .filter(move |entry| entry.category == category_key)
}

/// 某条驱动所属分类的展示名（查不到分类时回落分类键本身）。
#[must_use]
pub fn category_label_of(key: &str) -> &'static str {
    entry(key)
        .and_then(|entry| categories().iter().find(|item| item.key == entry.category))
        .map_or("", |item| item.label)
}

/// 能用**子进程轨**接进来当数据源的驱动 —— 连接选择器与「引擎」下拉的数据源。
///
/// 三条一起卡，缺一条就会在界面上多出一个点进去必失败的入口：
///
/// 1. [`DriverEntry::is_data_source`]（`metadataBrowse`）：etcd / ZooKeeper 的 agent
///    确实能「执行」命令，但它们**没有 schema 树**，接进来是一棵永远空的树；
/// 2. [`RuntimeMode::Agent`]：`native` 那几条（Kafka / RocketMQ / SQL Server 兼容组件）
///    是**编进 dbx 宿主**的 Rust crate，注册表里没有它们的工件；
/// 3. [`RuntimeMode::needs_install`]：本仓只实现了「下载安装」这一条路，没有第二条
///    兜底 —— 装不了的驱动就是跑不起来（`LaunchSpec` 也会拒）。
///
/// ⚠️ **这个筛选比「注册表里有工件」窄，是有意的**，别按注册表直接放宽：
/// `runtime_mode` 是 dbx 声明的「驱动住在哪」，**不是**「能不能下」。按注册表放宽会
/// 让 5 条一起冒出来 —— `duckdb` / `sqlite-worker` / `rabbitmq` / `rocketmq` /
/// `kafka`。其中 `duckdb` **已实测**说的是它自己那套 worker 协议
/// （`{id, method, params}` / `{id, ok, result|error}`，**没有 `jsonrpc`**，
/// 动词是 `execute` 而不是 `execute_query`，连接参数是一个文件路径），
/// 不是本仓 agent 轨的 JSON-RPC —— 先放出来就是「装得上、连不上」。
/// 另外四条**尚未核对协议**，要放宽就逐条实测，别按「它们是一家人」猜。
///
/// ⚠️ 返回的是**迭代器**而不是 `Vec`：调用方把它塞进界面网格，不必先分配一份。
/// 顺序沿用表内顺序（`order` 已是生成时的排序）。
pub fn agent_sources() -> impl Iterator<Item = &'static DriverEntry> {
    catalog().iter().filter(|entry| {
        // 判据是「数据源 + 装得出来 + **本仓真会去驱动它**」。第三条原先靠
        // `runtime() == Agent` 表达，但它说的是「dbx 怎么跑」，不是「我们跑不跑」：
        // `file` 档里 `duckdb` 的方言由**可下载的 worker** 说（本仓已实作，见
        // `duck-db` 的 `drivers::duckdb`），而 `sqlite-worker` 是给 SSH 隧道场景的，
        // 本仓用原生 rusqlite，装了也没用。所以 file 档逐条点名，不整档放行。
        entry.is_data_source()
            && (entry.runtime().needs_install() || SIDECAR_FILE_ENGINES.contains(&entry.key))
    })
}

/// `file` 档里**本仓会驱动一个子进程**的那几条（其余 file 条目走原生实现或不用）。
const SIDECAR_FILE_ENGINES: &[&str] = &["duckdb"];

#[cfg(test)]
mod tests {
    use super::*;

    /// 每条驱动都落在**已声明**的分类里。
    #[test]
    fn every_entry_belongs_to_a_declared_category() {
        for entry in catalog() {
            assert!(
                categories().iter().any(|item| item.key == entry.category),
                "{} 的分类 {} 未声明",
                entry.key,
                entry.category
            );
        }
    }

    /// 分类键与驱动键都唯一 —— 重复键会让商店列表出现两行同一条驱动。
    #[test]
    fn keys_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for entry in catalog() {
            assert!(seen.insert(entry.key), "驱动键重复：{}", entry.key);
        }
        let mut seen = std::collections::HashSet::new();
        for item in categories() {
            assert!(seen.insert(item.key), "分类键重复：{}", item.key);
        }
    }

    /// 分类顺序与驱动顺序都是升序 —— 表是生成的，顺序错了说明生成脚本排序丢了。
    #[test]
    fn orders_are_ascending() {
        let items: Vec<u16> = categories().iter().map(|item| item.order).collect();
        assert!(items.windows(2).all(|pair| pair[0] < pair[1]), "{items:?}");
    }

    /// 按分类取子集与全量的划分是一致的。
    #[test]
    fn category_filter_partitions_the_catalog() {
        let total: usize = categories()
            .iter()
            .map(|item| entries_in(item.key).count())
            .sum();
        assert_eq!(total, catalog().len());
    }

    /// 运行方式必须是**已知四值之一** —— 表是生成的，dbx 侧多一个取值时
    /// 这里要能发现（解析回落会把未知值悄悄当成 `agent`）。
    #[test]
    fn runtime_modes_round_trip() {
        for entry in catalog() {
            let mode = RuntimeMode::from_wire(entry.runtime_mode);
            assert_eq!(
                mode.wire_name(),
                entry.runtime_mode,
                "{} 的运行方式 {} 不是已知取值",
                entry.key,
                entry.runtime_mode
            );
            // 有工件可装的只有 agent 与 external 两档。
            assert_eq!(
                mode.needs_install(),
                matches!(mode, RuntimeMode::Agent | RuntimeMode::External)
            );
        }
    }

    /// 未知取值按 `agent` 处理（四档里唯一「有东西可装」的那档）。
    #[test]
    fn an_unknown_runtime_mode_falls_back_to_agent() {
        assert_eq!(RuntimeMode::from_wire(""), RuntimeMode::Agent);
        assert_eq!(RuntimeMode::from_wire("whatever"), RuntimeMode::Agent);
        assert_eq!(RuntimeMode::from_wire("native"), RuntimeMode::Native);
    }

    /// 商店里**绝大多数**条目都是可下载的 agent 轨 —— 这条不是判据，是护栏：
    /// 真掉到一半以下，说明生成的 `runtimeMode` 读错了（比如把 YAML 的键名拼错，
    /// 全体回落成某个值），而单看某一条是发现不了的。
    #[test]
    fn the_store_is_mostly_downloadable_agents() {
        let agents = catalog()
            .iter()
            .filter(|entry| entry.runtime() == RuntimeMode::Agent)
            .count();
        assert!(
            agents * 2 > catalog().len(),
            "agent 轨只剩 {agents}/{} 条，检查生成脚本里的 runtimeMode 取值",
            catalog().len()
        );
    }

    /// 缺省端口若给出，必须是可用的 TCP 端口（0 不是——那更像「忘了填」）。
    #[test]
    fn declared_ports_are_usable() {
        for entry in catalog() {
            if let Some(port) = entry.default_port {
                assert!(port > 0, "{} 的缺省端口是 0", entry.key);
            }
        }
    }

    /// **端口随品牌走，不随协议走** —— 这条是生成脚本最容易退化的地方。
    ///
    /// 反例就是它修掉的那个 bug：`mq.yaml` 把协议缺省端口写成 8080，三个消息队列
    /// 品牌一起被顶上 8080，而它们真实端口是 9092 / 9876 / 5672。生成脚本若哪天
    /// 又把品牌表那一步丢了，这条会红。
    #[test]
    fn ports_follow_the_brand_not_the_protocol() {
        let port = |key: &str| entry(key).expect(key).default_port;
        assert_eq!(port("kafka"), Some(9092));
        assert_eq!(port("rocketmq"), Some(9876));
        assert_eq!(port("rabbitmq"), Some(5672));
        // GBase 8a / 8s 共用 `gbase` 一个 dbType，端口却是 5258 / 9088。
        assert_eq!(port("gbase8a"), Some(5258));
        assert_eq!(port("gbase8s"), Some(9088));
        // 文件库没有 TCP 端口（品牌表里写 0，映射成 None）。
        assert_eq!(port("duckdb"), None);
        assert_eq!(port("access"), None);
    }

    /// 品牌的惯用账号 / 必需 URL 参数都来自 dbx 的品牌表，抽查几个有代表性的。
    #[test]
    fn brands_carry_their_own_account_and_url_params() {
        let user = |key: &str| entry(key).expect(key).default_user;
        assert_eq!(user("dameng"), "SYSDBA");
        assert_eq!(user("db2"), "db2inst1");
        assert_eq!(user("oracle"), "system");
        assert_eq!(user("h2"), "sa");
        assert_eq!(user("iris"), "_SYSTEM");
        // 没有惯例账号的品牌留空 —— 界面按「空串 = 不预填」处理。
        assert_eq!(user("etcd"), "");
        assert_eq!(user("mongodb"), "");

        // `urlParams` 是「不填就连不上」的那类：星环 Argo 要 auth=noSasl。
        assert_eq!(
            entry("argo").expect("argo").default_url_params,
            "auth=noSasl"
        );

        // 两个字段都是源串，不允许夹带空白（会原样填进输入框）。
        for entry in catalog() {
            assert_eq!(
                entry.default_user,
                entry.default_user.trim(),
                "{}",
                entry.key
            );
            assert_eq!(
                entry.default_url_params,
                entry.default_url_params.trim(),
                "{}",
                entry.key
            );
        }
    }

    /// [`agent_sources`] 的三条筛选各自都真的在起作用 —— 光断言「结果非空」不足以
    /// 说明筛掉了什么，所以逐条点出**被筛掉**的那几个代表。
    #[test]
    fn agent_sources_exclude_the_unusable_modes() {
        let keys: Vec<&str> = agent_sources().map(|entry| entry.key).collect();
        // 数据源、agent 轨、可下载：达梦 / Oracle 在内。
        assert!(keys.contains(&"dameng"), "{keys:?}");
        assert!(keys.contains(&"oracle"), "{keys:?}");
        // ① 不是数据源（`metadataBrowse: false`）：能执行命令但没有 schema 树。
        assert!(!keys.contains(&"etcd"), "etcd 不该出现在连接选择器里");
        assert!(!keys.contains(&"zookeeper"), "ZooKeeper 同上");
        // ② `native`：编进 dbx 宿主，注册表里根本没有它的工件。
        assert!(!keys.contains(&"kafka"), "Kafka 是 native 轨，装不了");
        assert!(!keys.contains(&"sqlserver-legacy"), "同上");
        // ③ `file` 档**逐条点名**：`duckdb` 的方言由可下载的 worker 说（本仓已实作），
        //    所以它在册；`sqlite-worker` 是 SSH 隧道场景用的，本仓有原生 SQLite，不在册。
        assert!(
            keys.contains(&"duckdb"),
            "DuckDB 有 worker 工件、本仓也接了，该在册：{keys:?}"
        );
        assert!(
            !keys.contains(&"sqlite-worker"),
            "SQLite SSH Worker 本仓不用（原生 rusqlite）"
        );

        // 三条筛选都可被独立观测：去掉任一条都会让上面的断言之一变红。
        assert!(agent_sources().all(|entry| entry.is_data_source()));
        assert!(agent_sources().all(|entry| entry.runtime().needs_install()
            || super::SIDECAR_FILE_ENGINES.contains(&entry.key)));
        assert!(agent_sources().count() > 40, "可用驱动少得可疑");
    }
}
