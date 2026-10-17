#!/usr/bin/env python3
"""从 dbx 检出目录生成 `crates/duck-app/src/pages/db/brands_gen.rs` 的**品牌表**。

用法：
    python3 scripts/gen_brands.py <dbx 检出目录>

例如：
    python3 scripts/gen_brands.py temp/dbx

## 这张表回答什么

「用户想连的那个牌子，具体该怎么连」——同一个线协议下面往往站着十几个牌子：

    品牌        协议(dbType)   缺省端口   惯用账号
    MySQL       mysql          3306      root
    MariaDB     mysql          3306      root      ← 同协议、不同牌子
    TiDB        mysql          4000      root
    OceanBase   mysql          2883      root
    Doris       mysql          9030      root
    …           （12 个牌子共用 mysql 这一个协议）

本仓的引擎档位（`DatabaseKind`）只有 7 个内建 + 1 个 agent，**表达不了这个粒度**。
所以品牌单独成一张表：界面拿它给出正确的名字 / 图标 / 缺省端口 / 惯用账号，
档案里用一个 `profile` 键记住牌子（dbx 叫 `driver_profile`）。

## 数据来源（dbx 为 Apache-2.0）

1. `<dbx>/plugins/connection-types/profiles/catalog.yaml`：**品牌表本体**（109 条），
   每条的 `port` / `user` / `urlParams` 就是这个牌子的连接缺省值。
2. `<dbx>/plugins/connection-types/*.yaml`：协议的性质 —— 它怎么跑
   （`runtimeMode`）、要靠哪个 agent（`agentKey`）。

## 只收「我们真的连得上」的品牌

解析规则（按顺序）：

- 协议是本仓**内建**的那几个（mysql / postgres / sqlite / clickhouse / sqlserver /
  mongodb / elasticsearch）→ 走内建档，不装驱动；
- 否则若协议的 `runtimeMode` 是 `agent` 且它的驱动键**在本仓驱动表里且可接**
  （=`duck_drivers::agent_sources()` 那一档）→ 走 agent 档；
- 其余**直接不收**（原生的 GaussDB / ClickHouse 云版、要自备 jar 的 JDBC、走 SSH 的
  rqlite、以及 redis —— 本仓有它自己的页）。收进来只会多出一张点进去报错的卡。

⚠️ 两张表必须同源：这里判「可接」用的是同一份驱动表，所以品牌表里没有
「点了却装不上」的项。

## 模式族

有些牌子**一次连接要选模式**：OceanBase 有 MySQL 模式与 Oracle 模式，GBase 有 8a 与 8s，
InterSystems 有 IRIS 与 Caché —— 它们的服务端是同一个产品，协议与缺省值却不同。
dbx 用 `switchOceanbaseMode` 这类函数硬编码了这几族；我们把它做成数据（`family` / `mode`），
界面按「同族的品牌 > 1」自动画一个模式切换器。

⚠️ 族是**产品事实**，推不出来：别想着用「dbType 相同就是一族」去推 —— `mq` 下面
Kafka / RocketMQ / RabbitMQ 同 dbType 却是三个独立产品，选哪个不是「模式」而是「连哪台」。
"""

from __future__ import annotations

import glob
import os
import re
import sys

try:
    import yaml
except ImportError:  # pragma: no cover - 提示装依赖
    sys.exit("需要 pyyaml：pip install pyyaml")


# 本仓内建的协议档（与 `DatabaseKind::wire_name` 同字面量）。
# dbx 的 dbType -> 我们的档位。只列**名字不一样**的那条。
BUILTIN_KINDS = {
    "mysql": "mysql",
    "postgres": "postgres",
    "sqlite": "sqlite",
    "clickhouse": "clickhouse",
    "sqlserver": "sqlserver",
    "mongodb": "mongo",
    "elasticsearch": "elasticsearch",
}

# 品牌 id -> 图标变体名（`duck_components::DatabaseLogo`）。
# 没列的品牌按「分段驼峰」推（`greatsql` -> `Greatsql`），推不出来就留空。
# ⚠️ 留空是**有意的**：dbx 的图标目录里没有星环 Argo 的图，宁缺毋滥 ——
# 界面回落成主题单色图标，好过就近安一个别家的品牌标。
BRAND_LOGO = {
    "dm": "Dameng",
    "gbase": "Gbase",
    "gbase8a": "Gbase",
    "gbase8s": "Gbase",
    "h2": "H2",
    "h2-legacy": "H2",
    "iris": "Iris",
    "cache": "Iris",
    "ignite": "ApacheIgnite",
    "ignite3": "ApacheIgnite",
    "kylin": "ApacheKylin",
    "transwarp-inceptor": "TranswarpInceptor",
    "custom_mysql": "Mysql",
    "custom_postgres": "Postgres",
    "argo": "",
    "manticoresearch": "ManticoreSearch",
    "victoriametrics": "VictoriaMetrics",
    "cloudflare-d1": "CloudflareD1",
    "prestosql": "Presto",
    # 同一家的两种形态共用一张图（推出来的名字在本仓图标表里不存在）。
    "oceanbase-oracle": "Oceanbase",
    "mongodb-legacy": "Mongodb",
    "oracle-oci": "Oracle",
}

# 模式族：品牌 id -> (族 id, 该品牌在这个族里的模式名)。
# 族 id 取该族「主」品牌的名字；`mode` 是画在切换器上的字。
FAMILY = {
    # OceanBase：同一套服务端两种语法。MySQL 模式走 mysql 协议，
    # Oracle 模式走它自己的 agent —— 这正是「同一个牌子两条连法」的典型。
    "oceanbase": ("oceanbase", "MySQL 模式"),
    "oceanbase-oracle": ("oceanbase", "Oracle 模式"),
    # GBase：8a 是 MySQL 系、8s 是 Informix 系（不是同一个协议！）。
    "gbase8a": ("gbase", "8a"),
    "gbase8s": ("gbase", "8s"),
    # H2：2.x 与 2.1 之前的老档（鉴权与 URL 形态不同）。
    "h2": ("h2", "2.x"),
    "h2-legacy": ("h2", "2.1 及更早"),
    # InterSystems：IRIS 是 Caché 的后继，同一个服务的两种产品线。
    "iris": ("iris", "IRIS"),
    "cache": ("iris", "Caché"),
    # Apache Ignite：2.x 与 3.x 的线协议不兼容。
    "ignite": ("ignite", "2.x"),
    "ignite3": ("ignite", "3.x"),
    # Oracle：Thin（纯 Rust/go 客户端）与 OCI（胖驱动，要 Instant Client）。
    "oracle": ("oracle", "Thin"),
    "oracle-oci": ("oracle", "OCI（胖驱动）"),
    # MongoDB：内建驱动与 legacy 协议档。
    "mongodb": ("mongodb", "MongoDB 协议"),
    "mongodb-legacy": ("mongodb", "Legacy 线路"),
}

# 族里的**缺省模式**（族 id -> 品牌键）：选择器上那张卡默认按谁开、切换器上谁是第一个。
#
# 为什么不能按名字排序推：Oracle 一族按字符串排是「OCI（胖驱动）」在前，而 OCI 要用户
# 另装 Instant Client，**不该是缺省**；H2 按字符串排会把 legacy 排到前面。
# 这是产品决定，不是排序问题 —— 所以逐族点出来。
FAMILY_PRIMARY = {
    "oceanbase": "oceanbase",
    "gbase": "gbase8a",
    "h2": "h2",
    "iris": "iris",
    "ignite": "ignite",
    "oracle": "oracle",
    "mongodb": "mongodb",
}

# 品牌表里**没有**、但本仓确实接得上的牌子。两个来源：
#   1. dbx 把它们挂在别的品牌名下当 `driverProfiles`（`oracle-oci`）；
#   2. dbx 那一档的 `runtimeMode` 是 `native`（`redshift`）—— 注册表里没有工件、
#      dbx 是把它编进宿主的，而本仓走的是**同线协议**（Redshift 就是 PostgreSQL 线协议，
#      它的 `redshift.yaml` 写着 `pgLikeStructure: true`，端口 5439）。
#
# ⚠️ 为什么要收 `redshift`：不收的话，那张手写卡落到 `Builtin(Postgres)`，
# 表单标题写成「PostgreSQL」、端口预填 **5432**（卡片上明明写着 `:5439`）——
# 卡与表单一句话对不上，用户照填就连不上。
#
# 键 -> (品牌键, 名称, 档位, 驱动键, 端口, 账号, urlParams)；族与图标由键推
#（`FAMILY` / `BRAND_LOGO`，取不到时图标回落分段驼峰）。
EXTRA_BRANDS = {
    "oracle-oci": ("oracle-oci", "Oracle（OCI 胖驱动）", "agent", "oracle-oci", 1521, "system", ""),
    "redshift": ("redshift", "Redshift", "postgres", "", 5439, "awsuser", ""),
}

# 品牌表里冗余的行：与另一个品牌完全重合（同协议、同端口、同账号、同驱动），
# 收进来只是多一张卡或多一个点不出差别的模式按钮。
#   gbase           = gbase8a 的协议级别名
#   custom_*        = “自带协议实现”的自由档，没有连接参数可言
#   mongodb-legacy  = 它的区别在 dbx 是一个独立 driver_profile，而我们没有那条驱动
#                     （本仓的 `mongodb` 键本身走的就是 legacy 线路），收进来会画出一个
#                     行为完全一样的「模式」按钮 —— 那比没有更糟。
#   mysql/postgres/sqlite/… 这些**不删** —— 它们是主品牌，卡片要留着。
DROP_BRANDS = {"gbase", "custom_mysql", "custom_postgres", "mongodb-legacy"}

# 反查用：本仓驱动表的键（由 `gen_driver_catalog.py` 生成，这里只读键名与可接性）。
# ⚠️ 这两个路径都相对**本仓**，与参数里的 dbx 检出目录无关。
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CATALOG_GEN = os.path.join(REPO, "crates", "duck-drivers", "src", "catalog_gen.rs")

# 图标变体清单（`DatabaseLogo`）也读**磁盘上的真源**，不在这里抄一份：
# 抄一份的后果是脚本里写了个不存在的变体名，而那是编译期错误 —— 生成时才炸，
# 比在生成时点出来晚得多。
DBLOGOS = os.path.join(REPO, "crates", "duck-components", "src", "dblogos.rs")


def rust_str(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def camel(brand_id: str) -> str:
    """品牌 id -> 图标变体名（分段驼峰）。"""
    parts = re.split(r"[-_]", brand_id)
    return "".join(part[:1].upper() + part[1:] for part in parts if part)


def load_serviceable() -> set[str]:
    """本仓驱动表里「可接」的驱动键（= `agent_sources()` 那一档）。

    判据与 `duck_drivers::agent_sources` 一致：可浏览元数据 + agent 轨。
    两者都读同一份生成的表，所以品牌表不会比商店更宽。

    ⚠️ 按**条目块**切，不按行切：`catalog_gen.rs` 生成时是一行一条，但进仓的是
    **过了一遍 rustfmt 的展开形态**（一条横跨十几行）。按行的正则在后一种上
    静默读成空表 —— 症状是品牌表从 68 条掉到 22 条，而且脚本一声不吭。
    所以末尾还有一道「解析出来的明显太少就报错」的闸。
    """
    path = CATALOG_GEN
    if not os.path.exists(path):
        sys.exit(f"找不到本仓驱动表：{path}（先跑 gen_driver_catalog.py）")
    source = open(path).read()
    keys: set[str] = set()
    for body in re.findall(r"DriverEntry \{(.*?)\}", source, re.S):
        if "category:" not in body:
            continue
        key = re.search(r'key: "([^"]+)"', body)
        if key is None:
            continue
        if 'runtime_mode: "agent"' in body and "metadata_browse: true" in body:
            keys.add(key.group(1))
    total = len(re.findall(r"DriverEntry \{", source))
    if len(keys) * 2 < total:
        sys.exit(f"驱动表只解析出 {len(keys)}/{total} 条可接驱动，判据与文件形态脱节了")
    return keys


def load_logo_variants() -> set[str]:
    """`DatabaseLogo` 的变体名集合（目录即是真源，见 `gen_dblogos.py`）。"""
    path = DBLOGOS
    if not os.path.exists(path):
        sys.exit(f"找不到品牌图标表：{path}")
    source = open(path).read()
    match = re.search(r"pub enum DatabaseLogo \{(.*?)\n\}", source, re.S)
    if match is None:
        sys.exit(f"{path} 里没有 `pub enum DatabaseLogo`")
    return set(re.findall(r"^\s{4}([A-Za-z0-9]+),", match.group(1), re.M))


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = sys.argv[1]
    ct_dir = os.path.join(root, "plugins", "connection-types")
    if not os.path.isdir(ct_dir):
        sys.exit(f"找不到连接类型目录：{ct_dir}")

    serviceable = load_serviceable()
    variants = load_logo_variants()

    profiles = yaml.safe_load(open(os.path.join(ct_dir, "profiles", "catalog.yaml")))
    # 协议的性质：dbType -> runtimeMode / agentKey / 分类
    protocols: dict[str, dict] = {}
    for path in sorted(glob.glob(os.path.join(ct_dir, "*.yaml"))):
        data = yaml.safe_load(open(path))
        db_type = data.get("dbType")
        if db_type:
            protocols.setdefault(db_type, data)

    rows: list[tuple] = []
    unknown_logos: set[str] = set()

    def add(brand_key: str, label: str, kind: str, driver: str, port, user: str, url_params: str):
        family, mode = FAMILY.get(brand_key, (brand_key, ""))
        logo = BRAND_LOGO.get(brand_key, camel(brand_key))
        if logo and logo not in variants:
            # 变体名推错了 / 图标目录里确实没有 —— 后者是正常的（Argo），
            # 前者是脚本该修的。两者都收敛成 `None`，但要说一声。
            unknown_logos.add(logo)
            logo = ""
        rows.append((brand_key, label, kind, driver, family, mode, port, user, url_params, logo))

    skipped: dict[str, list[str]] = {}
    for profile in profiles["profiles"]:
        brand_key = profile["id"]
        db_type = profile.get("dbType") or ""
        if brand_key in DROP_BRANDS:
            continue
        descriptor = protocols.get(db_type, {})
        mode = descriptor.get("runtimeMode") or "agent"
        port = profile.get("port")
        port = int(port) if port is not None else None
        port = port if (port or 0) > 0 else None
        user = (profile.get("user") or "").strip()
        url_params = (profile.get("urlParams") or "").strip()
        label = profile.get("label") or brand_key

        if db_type in BUILTIN_KINDS:
            add(brand_key, label, BUILTIN_KINDS[db_type], "", port, user, url_params)
            continue
        if mode == "agent":
            # 驱动键优先取**品牌自己的名字**（gbase8s 就是一条独立驱动），
            # 取不到才回落到协议的 agentKey。
            driver = brand_key if brand_key in serviceable else descriptor.get("agentKey") or db_type
            if driver in serviceable:
                add(brand_key, label, "agent", driver, port, user, url_params)
                continue
        skipped.setdefault(f"{mode}:{db_type}", []).append(brand_key)

    for _key, (bkey, label, kind, driver, port, user, url_params) in EXTRA_BRANDS.items():
        add(bkey, label, kind, driver, port, user, url_params)

    def family_rank(row: tuple) -> tuple:
        """族内排序：缺省模式排第一，其余按模式名。"""
        family = row[4]
        primary = FAMILY_PRIMARY.get(family)
        return (family, 0 if row[0] == primary else 1, row[5], row[0])

    rows.sort(key=family_rank)

    lines = [
        "//! 品牌表 —— **由脚本生成，不要手改**。",
        "//!",
        "//! 生成命令：`python3 scripts/gen_brands.py <dbx 检出目录>`",
        "//! 数据来源与收录规则见脚本头部注释。",
        "",
        "use duck_components::DatabaseLogo;",
        "",
        "use super::brands::Brand;",
        "",
        f"/// 品牌（{len(rows)} 条）。",
        f"pub(super) const BRANDS: [Brand; {len(rows)}] = [",
    ]
    for key, label, kind, driver, family, mode, port, user, url_params, logo in rows:
        lines.append(
            "    Brand { key: %s, label: %s, kind: %s, driver: %s, family: %s, mode: %s, "
            "port: %s, user: %s, url_params: %s, logo: %s },"
            % (
                rust_str(key),
                rust_str(label),
                rust_str(kind),
                rust_str(driver),
                rust_str(family),
                rust_str(mode),
                "Some(%d)" % port if port is not None else "None",
                rust_str(user),
                rust_str(url_params),
                "Some(DatabaseLogo::%s)" % logo if logo else "None",
            )
        )
    lines.append("];")
    lines.append("")

    out = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "crates",
        "duck-app",
        "src",
        "pages",
        "db",
        "brands_gen.rs",
    )
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as handle:
        handle.write("\n".join(lines))
    print(f"已写入 {out}（{len(rows)} 条品牌）")

    # 跳过的按成因归组打印 —— 这是「还有哪些库没接」的那张清单。
    print("\n未收录（我们还没有它的连接方式）：")
    for cause, brands in sorted(skipped.items(), key=lambda item: (-len(item[1]), item[0])):
        print(f"  {cause:<24} {len(brands):>2} 条  {', '.join(brands[:10])}")
    if unknown_logos:
        print("\n⚠️ 下面的图标变体名在本仓图标表里不存在，已按「无图」处理：")
        print(f"  {', '.join(sorted(unknown_logos))}")


if __name__ == "__main__":
    main()
