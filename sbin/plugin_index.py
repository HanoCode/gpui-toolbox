#!/usr/bin/env python3
"""把仓库里的插件打包成 tar.gz，并生成插件市场索引 plugin-index.json。

用法：
    sbin/plugin_index.py --plugins-dir plugins --out-dir dist/plugin-index \
        --base-url https://raw.githubusercontent.com/HanoCode/gpui-toolbox/plugin-index

产出（都在 `--out-dir` 下）：
    plugin-index.json                     市场索引
    packages/<id>-<version>.tar.gz        插件包

# 镜像通道（可选）

给了 `--mirror-base`（CDN 的公开前缀，如 Cloudflare R2 / 阿里云 OSS 上的目录）时，
`archive.url` 写成**镜像地址**、`--base-url` 那条分支原址退成 `archive.mirrors[0]`；
客户端按 `url` → `mirrors` 的顺序试，第一个能用的就下。不传就只写分支原址、
`mirrors` 是空表。顺序与 `sbin/manifest.py` 完全一致——国内客户端先走镜像，
取不到再回源。

# 与客户端的关系

读端是 `crates/duck-plugin-registry/src/index.rs`：那边定义文档形状，也定义「哪些
字段缺了就必须拒绝」。这里是写端，两边必须同步改——**漏一个必填字段，客户端会拒掉
整份索引**（刻意的，不是静默跳过：凭一份半懂的文档去下载并安装更糟）。

# 为什么归档布局是「清单在顶层」

读端 `content_root()` 接受两种打包习惯：清单在归档顶层，或整个插件在一个顶层目录里。
这里选**顶层**（等价于 `tar czf pkg.tar.gz -C plugins/<dir> .`）：少一层没有信息量的
目录名，解包后 `plugins/<id>/plugin.toml` 直接就位。

# 为什么执着于「可复现」

同一个插件目录，今天与明天打出来的包必须**字节一致**：否则索引里的 sha256 每次
发布都在变，客户端会把一个内容没变的插件认成「有更新」，白下一遍。所以 gzip 与 tar
的 mtime 都钉死、成员排序、uid/gid/uname/gname 归零、压缩级别固定。每次运行还会
当场打两遍比对摘要，把「有人不小心把 mtime 放回去了」变成一次发布失败而不是
一堆无意义的「可更新」。

# 版本号即不可变

`packages/<id>-<version>.tar.gz` 一旦发布过，内容就不许再变——变了的唯一正当做法是
**抬版本号**。检测到同名同版本但摘要不同会直接报错退出，而不是覆盖：覆盖会让已经
拿到旧索引的客户端下到对不上的字节，然后在摘要校验那里失败。

# 为什么是 Python

与 `sbin/manifest.py` 同一取舍：JSON 与 SHA-256 在 shell 里都要靠拼字符串，文件名
带空格或非 ASCII 就出错；三个 runner 镜像都自带 python3。写端不进客户端二进制。
TOML 用 stdlib 的 `tomllib`（Python ≥ 3.11），不引第三方依赖。
"""

import argparse
import gzip
import hashlib
import json
import os
import re
import sys
import tarfile
import tempfile
from datetime import datetime, timezone

# 与 crates/duck-plugin-registry/src/index.rs 的 SUPPORTED_INDEX_VERSION 对应。
INDEX_VERSION = 1
# 与 crates/duck-plugin/src/manifest.rs 的 SUPPORTED_API_VERSION 对应。
SUPPORTED_API_VERSION = 1
# 与 index.rs 的 validate_entry 里那个 match 对应。
EXECUTION_MODELS = ("js", "wasm", "process")

# 与 index.rs 的 is_valid_plugin_id 同源：非空、仅 a-z 0-9 . -、
# 不以 . / - 开头或结尾、无 `..`、长度 ≤ 128。
PLUGIN_ID = re.compile(r"^[a-z0-9](?:[a-z0-9.-]{0,126}[a-z0-9])?$")
# 语义化版本（与 index.rs 用 semver 解析同一套规则；这里只做形状检查）。
SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")

# 归档里不要带的东西：系统垃圾与生成物。插件自己的点文件（如 `.env`）照打——
# 那是作者的意图，不该由打包脚本替他猜。
EXCLUDED_NAMES = {".DS_Store", "Thumbs.db", "desktop.ini", "__MACOSX"}
EXCLUDED_DIRS = {".git", "__pycache__", "node_modules", "target"}

MANIFEST_FILE = "plugin.toml"
BLOCK = 1024 * 1024


def die(message):
    """一句话说清哪里不对，然后退出。CI 日志里第一行就要能看懂。"""
    raise SystemExit(message)


def sha256_of(path):
    """文件的 SHA-256，小写十六进制。流式读，不整包进内存。"""
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(BLOCK), b""):
            digest.update(block)
    return digest.hexdigest()


def now_rfc3339():
    """当前 UTC 时间，形如 2026-10-11T05:00:00Z。"""
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def load_toml(path):
    """读一份 TOML。`tomllib` 是 3.11 才进 stdlib 的，早于此版本直接说清楚。"""
    try:
        import tomllib  # noqa: PLC0415 — 故意延迟导入，好给旧解释器一句人话
    except ModuleNotFoundError:  # pragma: no cover — 取决于跑脚本的解释器版本
        die(
            "需要 Python 3.11 及以上（用 stdlib 的 tomllib 读 plugin.toml）；"
            "当前是 {}.{}".format(*sys.version_info[:2])
        )
    with open(path, "rb") as handle:
        return tomllib.load(handle)


def manifest_of(plugin_dir):
    """读一个插件目录的清单，并把读端会拒的规则先在这里拒掉。"""
    path = os.path.join(plugin_dir, MANIFEST_FILE)
    if not os.path.isfile(path):
        die("{} 下没有 {}".format(plugin_dir, MANIFEST_FILE))
    raw = load_toml(path)

    where = os.path.basename(plugin_dir)
    plugin_id = str(raw.get("id", ""))
    if not PLUGIN_ID.match(plugin_id) or ".." in plugin_id:
        die("插件 {} 的 id 不合法：{!r}（只允许小写字母、数字、. 与 -）".format(where, plugin_id))
    if not str(raw.get("name", "")).strip():
        die("插件 {} 缺少 name".format(where))
    version = str(raw.get("version", ""))
    if not SEMVER.match(version):
        die("插件 {} 的 version 不是语义化版本：{!r}".format(where, version))
    api_version = raw.get("api_version")
    if api_version != SUPPORTED_API_VERSION:
        die(
            "插件 {} 的 api_version 是 {!r}，本索引只支持 {}".format(
                where, api_version, SUPPORTED_API_VERSION
            )
        )
    model = str(raw.get("execution_model", ""))
    if model not in EXECUTION_MODELS:
        die(
            "插件 {} 的 execution_model 是 {!r}，只支持 {}".format(
                where, model, "、".join(EXECUTION_MODELS)
            )
        )
    return {
        "raw": raw,
        "id": plugin_id,
        "name": str(raw["name"]),
        "version": version,
        "description": str(raw.get("description", "")),
        "category": str(raw.get("category", "")),
        "tags": [str(tag) for tag in raw.get("tags", [])],
        "author": str(raw.get("author", "")),
        "homepage": str(raw.get("homepage", "")),
        "api_version": api_version,
        "execution_model": model,
        "capabilities": capabilities_of(raw),
        "dir": plugin_dir,
    }


def capabilities_of(raw):
    """摊出 `[capabilities]` 表。缺省即「什么都没声明」。

    形状必须与 `duck_plugin::Capabilities` 完全一致——多写一个键，客户端会拒掉
    整份索引（`deny_unknown_fields`）。这里先拦一道，报错时能点名是哪个插件。
    """
    caps = raw.get("capabilities", {})
    if not isinstance(caps, dict):
        die("capabilities 必须是表（[capabilities]），实际是 {}".format(type(caps).__name__))
    known = {
        "filesystem_read",
        "filesystem_write",
        "network_hosts",
        "notifications",
        "process_exec",
    }
    unknown = sorted(set(caps) - known)
    if unknown:
        die(
            "capabilities 里有客户端读不懂的键：{}；"
            "确认框必须能如实报出每一项权限，所以宁可让整份索引失败".format("、".join(unknown))
        )
    out = {}
    for key in ("filesystem_read", "filesystem_write", "network_hosts", "process_exec"):
        value = caps.get(key, [])
        if not isinstance(value, list):
            die("capabilities.{} 必须是字符串数组".format(key))
        out[key] = [str(item) for item in value]
    out["notifications"] = bool(caps.get("notifications", False))
    return out


def collect_files(root):
    """插件目录下要打进包的文件，相对路径升序（顺序影响 tar 字节，必须稳定）。"""
    collected = []
    for base, dirs, names in os.walk(root):
        dirs[:] = sorted(
            name for name in dirs if name not in EXCLUDED_DIRS and name not in EXCLUDED_NAMES
        )
        for name in dirs:
            full = os.path.join(base, name)
            if os.path.islink(full):
                # 客户端在解包时拒收链接条目，所以链接也打不进包；与其留下一个
                # 「本地能跑、装到别人机器上文件是空的」的坑，不如在这里说清楚。
                die("插件里不接受符号链接：{}".format(full))
        for name in sorted(names):
            if name in EXCLUDED_NAMES:
                continue
            full = os.path.join(base, name)
            if os.path.islink(full):
                die("插件里不接受符号链接：{}".format(full))
            if not os.path.isfile(full):
                die("插件里不接受非普通文件：{}".format(full))
            collected.append((os.path.relpath(full, root), full))
    collected.sort()
    return collected


def pack(plugin_dir, destination):
    """把一个插件目录打成**可复现**的 tar.gz，写进 `destination`。

    可复现的四件事：gzip mtime 归零、tar 成员 mtime 归零、uid/gid/uname/gname 归零、
    成员按路径排序。缺任何一条，内容没变也会算出新摘要。
    """
    files = collect_files(plugin_dir)
    if not files:
        die("{} 里一个文件都没有".format(plugin_dir))

    with open(destination, "wb") as raw:
        # mtime=0：gzip 头里的时间戳是「同一份内容、不同时间」摘要变掉的头号原因。
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.GNU_FORMAT) as tar:
                for relative, full in files:
                    info = tar.gettarinfo(full, arcname=relative)
                    info.mtime = 0
                    info.uid = 0
                    info.gid = 0
                    info.uname = ""
                    info.gname = ""
                    # 目录与文件都不打进去：目录由文件路径隐含，省掉一类顺序敏感点。
                    if not info.isfile():
                        continue
                    # 只保留「可执行」这一位：其余权限位没有语义，带上只会引入噪声。
                    info.mode = 0o755 if os.access(full, os.X_OK) else 0o644
                    with open(full, "rb") as handle:
                        tar.addfile(info, handle)


def pack_twice_and_check(plugin_dir, work_dir):
    """打两遍并比对摘要，返回 (sha256, size)。

    这是把「可复现」从口头约定变成一次发布失败：真有人把 mtime 放回归档里，
    这里当场就炸，而不是让客户端把每个插件都当成「有更新」。
    """
    first = os.path.join(work_dir, "repro-a.tar.gz")
    second = os.path.join(work_dir, "repro-b.tar.gz")
    pack(plugin_dir, first)
    pack(plugin_dir, second)
    digest_a = sha256_of(first)
    digest_b = sha256_of(second)
    if digest_a != digest_b:
        die(
            "打包不可复现：{} 两次打出的摘要不同。"
            "多半是归档里混进了时间戳或文件顺序——那会让每次发布都变成一次假更新".format(
                plugin_dir
            )
        )
    return digest_a, os.path.getsize(first), first


def previous_packages(previous_dir):
    """上一次发布的包：文件名 → (路径, 摘要)。

    只认文件名，不读索引：目录本身是事实来源，省掉「索引与目录不一致时信谁」的问题。
    """
    found = {}
    if not previous_dir:
        return found
    packages = os.path.join(previous_dir, "packages")
    if not os.path.isdir(packages):
        return found
    for name in sorted(os.listdir(packages)):
        if not name.endswith(".tar.gz"):
            continue
        path = os.path.join(packages, name)
        if os.path.isfile(path):
            found[name] = (path, sha256_of(path))
    return found


def archive_url(base, file_name):
    """一个包的公开地址。`base` 是已经去掉尾斜杠的前缀。"""
    return "{}/packages/{}".format(base, file_name)


def build(args):
    """跑完整个发布流程，返回 (索引文档, 概要行列表)。"""
    if not os.path.isdir(args.plugins_dir):
        die("插件目录不存在：{}".format(args.plugins_dir))

    base_url = args.base_url.rstrip("/")
    if not base_url.startswith("https://"):
        # 读端在解析期就拒非 https（`EntryProblem::InsecureUrl`）。在这里先拦，
        # 免得生成一份必然被拒的索引。
        die("--base-url 必须是 https:// 开头，实际是 {!r}".format(args.base_url))

    mirror_base = args.mirror_base.rstrip("/") if args.mirror_base else ""
    if mirror_base and not mirror_base.startswith("https://"):
        # 备用地址和主地址一样承重（分支原址走不通时全靠它），读端同样只收 https。
        die("--mirror-base 必须是 https:// 开头，实际是 {!r}".format(args.mirror_base))

    packages_dir = os.path.join(args.out_dir, "packages")
    os.makedirs(packages_dir, exist_ok=True)

    previous = previous_packages(args.previous_dir)
    referenced = set()
    entries = []
    summary = []

    # 打包用的中间文件放临时目录，不落在产物目录里——否则 force push 时会把
    # 半成品一起带上去。
    with tempfile.TemporaryDirectory(prefix="duck-plugin-index-") as work_dir:
        for name in sorted(os.listdir(args.plugins_dir)):
            plugin_dir = os.path.join(args.plugins_dir, name)
            if not os.path.isdir(plugin_dir) or name.startswith("."):
                continue
            if not os.path.isfile(os.path.join(plugin_dir, MANIFEST_FILE)):
                # 不是插件目录（比如放说明文档的子目录）——跳过比报错合适。
                continue

            manifest = manifest_of(plugin_dir)
            digest, size, packed = pack_twice_and_check(plugin_dir, work_dir)
            file_name = "{}-{}.tar.gz".format(manifest["id"], manifest["version"])
            referenced.add(file_name)

            destination = os.path.join(packages_dir, file_name)
            if file_name in previous:
                old_path, old_digest = previous[file_name]
                if old_digest != digest:
                    die(
                        "{} 已经发布过，但这次打出来的内容不同（{} → {}）。"
                        "同名同版本必须字节一致：请先抬 {} 的 version 再发。".format(
                            file_name, old_digest[:12], digest[:12], manifest["id"]
                        )
                    )
                # 内容一致：用上一次那份，分支上不会出现「同一文件反复被重写」的噪声。
                with open(old_path, "rb") as source, open(destination, "wb") as target:
                    target.write(source.read())
            else:
                os.replace(packed, destination)

            entries.append(
                {
                    "id": manifest["id"],
                    "name": manifest["name"],
                    "version": manifest["version"],
                    "description": manifest["description"],
                    "category": manifest["category"],
                    "tags": manifest["tags"],
                    "api_version": manifest["api_version"],
                    "execution_model": manifest["execution_model"],
                    "author": manifest["author"],
                    "homepage": manifest["homepage"],
                    "capabilities": manifest["capabilities"],
                    "archive": {
                        "url": archive_url(mirror_base or base_url, file_name),
                        # 给了镜像才把分支原址留成备用；没给就是空表（照写，不省键：
                        # 读端有 #[serde(default)]，但写出来让线上格式只有一种形状）。
                        "mirrors": (
                            [archive_url(base_url, file_name)] if mirror_base else []
                        ),
                        "sha256": digest,
                        "size": size,
                    },
                }
            )
            summary.append(
                "  {:<28} v{:<10} {:>8}  {}".format(
                    manifest["id"], manifest["version"], format_size(size), file_name
                )
            )

    # 上一次发布过、这次索引里不再引用的包照样带着走：客户端可能还揣着 6 小时前的
    # 索引缓存，删掉包会让它点安装时 404。宁可让分支多攒几份旧包。
    carried = 0
    for file_name, (path, _digest) in sorted(previous.items()):
        if file_name in referenced:
            continue
        destination = os.path.join(packages_dir, file_name)
        if os.path.exists(destination):
            continue
        with open(path, "rb") as source, open(destination, "wb") as target:
            target.write(source.read())
        carried += 1

    if not entries:
        # 空索引是合法文档，但几乎总是「--plugins-dir 指错了」。宁可在这里停住：
        # 发一份空索引出去，所有用户的市场会一起变空。
        die("{} 下一个插件都没有，索引会是空的".format(args.plugins_dir))

    entries.sort(key=lambda entry: entry["id"])
    document = {
        "index_version": INDEX_VERSION,
        "generated_at": args.generated_at or now_rfc3339(),
        "plugins": entries,
    }
    if carried:
        summary.append("  （另带入 {} 个不再被索引引用的旧包）".format(carried))
    if mirror_base:
        # 发布日志里要能一眼看出「这次到底指向谁」，否则镜像没配上会等到用户
        # 装不动才发现。
        summary.append("  包地址：主 {}　备用 {}".format(mirror_base, base_url))
    else:
        summary.append("  包地址：{}（未配置镜像）".format(base_url))
    return document, summary


def format_size(size):
    """人类可读的字节数，只给日志看，规则简单即可。"""
    if size >= 1024 * 1024:
        return "{:.1f} MB".format(size / (1024 * 1024))
    if size >= 1024:
        return "{:.0f} KB".format(size / 1024)
    return "{} B".format(size)


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description="把仓库里的插件打包并生成 plugin-index.json",
    )
    parser.add_argument("--plugins-dir", default="plugins", help="插件根目录（默认为 plugins）")
    parser.add_argument(
        "--out-dir",
        default="",
        help="写出索引与 packages/ 的目录（--check 时不需要）",
    )
    parser.add_argument(
        "--base-url",
        required=True,
        help="插件包的公开前缀（索引分支根），如 "
        "https://raw.githubusercontent.com/HanoCode/gpui-toolbox/plugin-index",
    )
    parser.add_argument(
        "--mirror-base",
        default="",
        help="CDN 公开前缀；给了它就把镜像当主地址、--base-url 那条分支原址放进 mirrors",
    )
    parser.add_argument(
        "--previous-dir",
        default="",
        help="上一次发布的目录（含 packages/），用于沿用旧包并拦住同名改内容",
    )
    parser.add_argument(
        "--generated-at",
        default="",
        help="索引时间戳（RFC 3339）；默认取当前 UTC 时间，给定值便于可复现地生成",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="只校验清单与打包能否成功，不写任何文件（CI 里当 lint 用）",
    )
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)

    if not args.check and not args.out_dir:
        die("缺少 --out-dir（--check 时才可以省）")

    if args.check:
        # 复用主流程，但把产物丢进临时目录：校验逻辑与真正发布时走的是同一条路，
        # 不会出现「lint 过了、发布还是炸」。
        with tempfile.TemporaryDirectory() as scratch:
            args.out_dir = scratch
            document, summary = build(args)
        print("校验通过，共 {} 个插件：".format(len(document["plugins"])))
        for line in summary:
            print(line)
        return 0

    document, summary = build(args)

    out = os.path.join(args.out_dir, "plugin-index.json")
    with open(out, "w", encoding="utf-8") as handle:
        # ensure_ascii=False：中文插件名与说明原样写出，人工审阅时读得懂。
        json.dump(document, handle, ensure_ascii=False, indent=2)
        handle.write("\n")

    print("已写出 {}（共 {} 个插件）：".format(out, len(document["plugins"])))
    for line in summary:
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
