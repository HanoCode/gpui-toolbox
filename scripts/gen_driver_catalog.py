#!/usr/bin/env python3
"""从 dbx 检出目录生成 `crates/duck-drivers/src/catalog_gen.rs` 的驱动目录静态表。

用法：
    python3 scripts/gen_driver_catalog.py <dbx 检出目录>

例如：
    python3 scripts/gen_driver_catalog.py temp/dbx

数据来源（dbx 为 Apache-2.0）：
  - `<dbx>/plugins/connection-types/*.yaml`：连接类型清单。取 `driverStoreVisible`
    的条目（`driverStoreOrder` 排序）、`agentKey`、`label`、`defaultPort`、
    `runtimeMode`、`capabilities.metadataBrowse`；
    另取 `managedDrivers[].storeVisible` 与 `driverProfiles[].storeVisible`
    里的驱动键（如 sqlite-worker / oracle-oci）。
  - `<dbx>/plugins/connection-types/profiles/catalog.yaml`：**品牌表**（109 条）。
    它给三样东西：
      1. `dbType -> category` 的归类（sql / analytics / domestic / document /
         lightweight / graph_ai / timeseries / mq / registry_config）；
      2. **每个品牌的缺省端口与惯用账号**（`port` / `user`）—— 这是「连这个库该填
         什么」的真源，比连接类型 YAML 的 `defaultPort` 更细：`mq.yaml` 给的是一个
         泛化的 `8080`，而品牌表里 Kafka 9092 / RocketMQ 9876 / RabbitMQ 5672 各是各的。
      3. **品牌的必需 URL 参数**（`urlParams`），例如 Argo `auth=noSasl`、
         Kyuubi `auth=NONE` —— 少了它那台服务器根本连不上。
    所以端口 / 账号 / urlParams **一律以品牌表为准**，连接类型 YAML 只作兜底。

⚠️ 品牌表按**品牌 id** 索引，而我们的键多数取 `agentKey`，两边有几个对不上名
（见 `BRAND_ALIAS`）—— 那是既成事实，只能逐条钉住，别想用一个规则推出来。

产物是**纯数据**：九个分类 + 驱动条目（键、名称、分类、缺省端口、运行方式、
能否浏览元数据、顺序）。本仓不引入 YAML 解析依赖 —— 生成一次、提交结果，
改 dbx 侧数据时重跑本脚本。

⚠️ **这张表的键集合与 dbx 的 `agent-registry.json` 逐条相同（各 55 条）**，不是巧合：
`driverStoreVisible` 就是「这条驱动要从商店里装」的意思。所以「表里有 ⇒ 注册表里也有」
可以当不变式用（`duck-drivers` 的联网用例钉住了它）。改 dbx 侧数据时这两边要一起看。
"""

from __future__ import annotations

import glob
import os
import sys

try:
    import yaml
except ImportError:  # pragma: no cover - 提示装依赖
    sys.exit("需要 pyyaml：pip install pyyaml")

# 分类：wire 值 -> (中文标题, 图标名, 顺序)。顺序即侧栏顺序。
CATEGORIES = [
    ("sql", "关系型数据库", 10),
    ("analytics", "数仓分析", 20),
    ("domestic", "国产数据库", 30),
    ("lightweight", "轻量数据库", 40),
    ("document", "文档/缓存/检索", 50),
    ("graph_ai", "图谱/向量", 60),
    ("timeseries", "时序数据库", 70),
    ("mq", "消息队列", 80),
    ("registry_config", "注册与配置", 90),
]

# dbType 不在 catalog.yaml 里时的兜底归类。
FALLBACK_CATEGORY = {
    "sqlite": "lightweight",
    "duckdb": "lightweight",
    "rqlite": "lightweight",
    "turso": "lightweight",
    "h2": "lightweight",
    "h2-legacy": "lightweight",
    "redis": "document",
    "valkey": "document",
    "memcached": "document",
    "mongodb": "document",
    "mongodb-legacy": "document",
    "couchdb": "document",
    "elasticsearch": "document",
    "easysearch": "document",
    "meilisearch": "document",
    "manticoresearch": "document",
    "solr": "document",
    "kafka": "mq",
    "rocketmq": "mq",
    "rabbitmq": "mq",
    "mqtt": "mq",
    "mq": "mq",
    "etcd": "registry_config",
    "etcd2": "registry_config",
    "zookeeper": "registry_config",
    "consul": "registry_config",
    "nacos": "registry_config",
    "influxdb": "timeseries",
    "influxdb3": "timeseries",
    "tdengine": "timeseries",
    "iotdb": "timeseries",
    "questdb": "timeseries",
    "victoriametrics": "timeseries",
    "neo4j": "graph_ai",
    "nebula": "graph_ai",
    "qdrant": "graph_ai",
    "milvus": "graph_ai",
    "weaviate": "graph_ai",
    "chromadb": "graph_ai",
    "sqlite-worker": "lightweight",
    "oracle-oci": "sql",
    "sqlserver-legacy": "sql",
    "access": "document",
    "cache": "document",
}

# 注册表里有、但连接类型 YAML 没给出的驱动键（键 -> (名称, dbType)）。
EXTRA_KEYS = {
    "cache": ("InterSystems Cache", "cache"),
    "duckdb": ("DuckDB", "duckdb"),
    "etcd2": ("etcd v2", "etcd2"),
    "gbase8s": ("南大通用 GBase 8s", "gbase8s"),
    "h2-legacy": ("H2 (Legacy)", "h2-legacy"),
    "mongodb": ("MongoDB (Legacy)", "mongodb"),
    "oracle-oci": ("Oracle (OCI)", "oracle-oci"),
    "sqlite-worker": ("SQLite SSH Worker", "sqlite-worker"),
    "sqlserver-legacy": ("SQL Server (Legacy)", "sqlserver-legacy"),
    "kafka": ("Apache Kafka", "kafka"),
    "rabbitmq": ("RabbitMQ", "rabbitmq"),
    "rocketmq": ("Apache RocketMQ", "rocketmq"),
}

# 我们的驱动键 -> 品牌表里的 id。只有名字对不上的才列（6 条）。
#
# 为什么会不一样：我们的键取连接类型 YAML 的 `agentKey`，而品牌表按「商店里那一栏
# 叫什么」编号。两套编号各自成形，改不了同一个 —— 所以逐条钉住，比事后猜稳。
# 别名指向的品牌同样是端口 / 账号的来源（例：`oracle-oci` 与 `oracle` 同一个服务端，
# 端口账号当然一样）。
BRAND_ALIAS = {
    "dameng": "dm",
    "transwarp": "transwarp-inceptor",
    "etcd2": "etcd-v2",
    "oracle-oci": "oracle",
    "sqlserver-legacy": "sqlserver",
    "sqlite-worker": "sqlite",
}


def rust_str(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = sys.argv[1]
    ct_dir = os.path.join(root, "plugins", "connection-types")
    if not os.path.isdir(ct_dir):
        sys.exit(f"找不到连接类型目录：{ct_dir}")

    profiles = yaml.safe_load(open(os.path.join(ct_dir, "profiles", "catalog.yaml")))
    category_of: dict[str, str] = {}
    # 品牌表：id -> (缺省端口, 惯用账号, 必需 urlParams, dbType)
    brands: dict[str, tuple] = {}
    for profile in profiles["profiles"]:
        db_type = profile.get("dbType")
        category = profile.get("category")
        if db_type and category and db_type not in category_of:
            category_of[db_type] = category
        port = profile.get("port")
        brands[profile["id"]] = (
            int(port) if port is not None else None,
            (profile.get("user") or "").strip(),
            (profile.get("urlParams") or "").strip(),
            db_type or "",
        )

    # 键 -> (顺序, 名称, dbType, 缺省端口, 运行方式, 可浏览元数据)
    entries: dict[str, tuple] = {}
    for path in sorted(glob.glob(os.path.join(ct_dir, "*.yaml"))):
        data = yaml.safe_load(open(path))
        db_type = data.get("dbType")
        # `runtimeMode` 是「这东西怎么跑起来」，直接决定我们能不能拿到工件：
        #   agent    = 独立子进程（**可下载**，我们必须走这条）
        #   native   = 编进 dbx 宿主的 Rust crate（对我们**不存在**这个工件）
        #   file     = 本地文件库（SQLite / DuckDB，自己开文件，不需要驱动）
        #   external = 通用 JDBC 插件（要用户自备 jar）
        mode = data.get("runtimeMode") or "agent"
        caps = data.get("capabilities") or {}
        # 「能不能当数据源浏览」用它，而不是 `queryExecution`：etcd / kafka 之类也能
        # 「执行」（发命令），但它们没有 schema 树，接到目录页里就是一棵永远空的树。
        browse = bool(caps.get("metadataBrowse"))
        port = data.get("defaultPort")
        port = int(port) if port is not None else None

        def add(key, order, label, *, _port=port, _browse=browse, _mode=mode):
            entries.setdefault(
                key,
                (int(order) if order is not None else 900, label, db_type, _port, _mode, _browse),
            )

        if data.get("driverStoreVisible"):
            add(data.get("agentKey") or db_type, data.get("driverStoreOrder"), data.get("label") or db_type)
        for managed in data.get("managedDrivers") or []:
            if managed.get("storeVisible"):
                add(managed["key"], managed.get("storeOrder"), managed.get("label") or managed["key"])
        for profile in data.get("driverProfiles") or []:
            if profile.get("storeVisible"):
                add(
                    profile.get("agentKey") or db_type,
                    profile.get("storeOrder"),
                    profile.get("label") or profile.get("agentKey") or db_type,
                )

    for key, (label, db_type) in EXTRA_KEYS.items():
        entries.setdefault(key, (800, label, db_type, None, "agent", False))

    rows = []
    for key, (order, label, db_type, port, mode, browse) in entries.items():
        category = category_of.get(db_type) or FALLBACK_CATEGORY.get(key) or FALLBACK_CATEGORY.get(db_type) or "sql"
        # 品牌表说了算：端口 / 惯用账号 / 必需 urlParams 都按品牌取。
        #
        # 为什么不信连接类型 YAML 的 `defaultPort`：那个值挂在**协议**上，多个品牌共用
        # 一份时会一起被顶上同一个数 —— `mq.yaml` 的 8080 就是这么让 Kafka / RocketMQ /
        # RabbitMQ 全变成 8080 的（品牌表里它们本是 9092 / 9876 / 5672）。同理
        # `gbase.yaml` 的 5258 把 8s 也带成了 5258（8s 是 9088）。
        #
        # 端口 0 在品牌表里的意思是「这台没有 TCP 端口」（SQLite / Access / DuckDB
        # 这类文件库），映射成 `None`，与 `DriverEntry::default_port` 的口径一致。
        user, url_params = "", ""
        brand = brands.get(key) or brands.get(BRAND_ALIAS.get(key, ""))
        if brand is not None:
            brand_port, user, url_params, _brand_type = brand
            if brand_port is not None:
                port = brand_port if brand_port > 0 else None
        rows.append((order, key, label, category, port, user, url_params, mode, browse))

    lines = [
        "//! 驱动目录静态表 —— **由脚本生成，不要手改**。",
        "//!",
        "//! 生成命令：`python3 scripts/gen_driver_catalog.py <dbx 检出目录>`",
        "//! 数据来源：dbx（Apache-2.0）的 `plugins/connection-types/*.yaml` 与",
        "//! `plugins/connection-types/profiles/catalog.yaml`。见脚本头部注释。",
        "",
        "use super::{DriverCategory, DriverEntry};",
        "",
        "/// 侧栏分类（顺序即展示顺序）。",
        "pub(super) const CATEGORIES: [DriverCategory; %d] = [" % len(CATEGORIES),
    ]
    for wire, label, order in CATEGORIES:
        lines.append(
            f"    DriverCategory {{ key: {rust_str(wire)}, label: {rust_str(label)}, order: {order} }},"
        )
    lines += ["];", "", f"/// 驱动条目（{len(rows)} 条）。", "pub(super) const ENTRIES: [DriverEntry; %d] = [" % len(rows)]
    for order, key, label, category, port, user, url_params, mode, browse in sorted(rows):
        lines.append(
            "    DriverEntry { key: %s, label: %s, category: %s, order: %d, default_port: %s, "
            "default_user: %s, default_url_params: %s, runtime_mode: %s, metadata_browse: %s },"
            % (
                rust_str(key),
                rust_str(label),
                rust_str(category),
                order,
                "Some(%d)" % port if port is not None else "None",
                rust_str(user),
                rust_str(url_params),
                rust_str(mode),
                "true" if browse else "false",
            )
        )
    lines.append("];")
    lines.append("")

    out = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "crates",
        "duck-drivers",
        "src",
        "catalog_gen.rs",
    )
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as handle:
        handle.write("\n".join(lines))
    print(f"已写入 {out}（{len(rows)} 条驱动、{len(CATEGORIES)} 个分类）")


if __name__ == "__main__":
    main()
