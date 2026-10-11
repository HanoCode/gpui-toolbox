#!/usr/bin/env python3
"""汇总第三方依赖的开源许可，生成 `assets/licenses/THIRD_PARTY_LICENSES.md`。

用法：
    python3 sbin/licenses.py            # 生成 / 覆盖汇总文件
    python3 sbin/licenses.py --check    # 校验文件与当前依赖是否一致（CI / 打包用）

`--check` 的退出码是**三态**的，因为「内容过期」与「这台机器查不了」得区别对待
（`sbin/package.sh` 只拦前者）：
    0  一致
    1  已过期（依赖变了但没重新生成），或文件还没生成过
    2  环境不支持（缺 tomllib / cargo 缓存里没有源码目录，先跑 `cargo fetch`）

⚠️ **「缓存只缺一部分」也算 2，不算 1。** 渲染结果依赖「本机缓存能否定位到每个包的
源码目录」：定位不到就会被写成「未定位」、末尾还会多出「未能定位的组件」一节，于是
**必然**与提交的文件不同。那是本机缓存不全，与「依赖变了」毫无关系——不区分开就会
在打包时误拦（实测：CI 的 macOS 腿先跑了一段 `cargo test`，缓存里只有那两个 crate 的
依赖，于是 `package.sh` 判「已过期」而失败；同一提交在缓存完整的机器上是通过的）。

它回答的是发行合规里最常被问的一句：「这个二进制里到底带了哪些别人的代码、按什么条款」。

# 为什么是 Python 而不是 `cargo about` / `cargo-license`

那两个工具都是要额外 `cargo install` 的第三方二进制，而本仓的 CI 只在
`ubuntu-latest` 上跑一段短脚本（见 `.github/workflows/release.yml`：发布作业**不编 Rust**），
为一份文本去装一个会自行升级的依赖不划算。反过来，**数据全都已经在本地**：
`Cargo.lock` 有完整的依赖图（含版本与来源），cargo 缓存里有每个包随源码附带的
`LICENSE*` 全文。拼起来就够了，且全程离线、可用同一套脚本在三个平台复跑。

# 来源覆盖（缺一类就是漏版权声明）

- `registry+…` —— crates.io 上的包，缓存目录 `$CARGO_HOME/registry/src/*/<name>-<version>/`。
- `git+…` —— 本仓经 `[patch.crates-io]` 换进来的 gpui-fast / zed 系列等，
  缓存目录 `$CARGO_HOME/git/checkouts/<repo>-<hash>/<短 rev>/`（是 workspace 仓库，
  所以要**按 `Cargo.toml` 里的 `package.name` 找**，不能按目录名）。
- 无 `source` 的包 —— 本仓自己的 12 个 crate，属本项目，版权见根 `LICENSE`，**不入表**。

# 可复现

输出**不含时间戳**、全部按名称排序、许可全文按内容去重后按标签排序：
**缓存完整时**同一份 `Cargo.lock` 在任意机器上跑出的字节完全一致，`--check` 才有意义
（缓存不完整的情况见文件头的退出码 2）。
所以**同名同版本的内容不可手工改**——改了会被 `--check` 判为过期。
"""

import argparse
import hashlib
import os
import re
import sys
from collections import Counter
from pathlib import Path

# `--check` 的退出码（见文件头）：0 一致 / 1 过期 / 2 环境不支持。
EXIT_STALE = 1
EXIT_UNSUPPORTED = 2


def unsupported(message: str) -> "NoReturn":
    """环境问题（而不是「内容过期」）：调用方可以只警告、不拦。"""
    print(f"无法汇总第三方许可：{message}", file=sys.stderr)
    raise SystemExit(EXIT_UNSUPPORTED)


if sys.version_info < (3, 11):
    unsupported("需要 Python 3.11+（依赖 tomllib）")
import tomllib  # noqa: E402  （必须在版本检查之后导入）

# 仓库根：本文件在 sbin/ 下。
REPO = Path(__file__).resolve().parent.parent
LOCK = REPO / "Cargo.lock"
OUT = REPO / "assets" / "licenses" / "THIRD_PARTY_LICENSES.md"

# 视为「许可正文」的文件名前缀（大小写不敏感，取 basename 全大写比较）。
TEXT_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")

# 单个正文文件的大小上限：正常许可全文只有几 KB，超过这个量级的多半是被误并进来的东西。
MAX_TEXT_BYTES = 64 * 1024

# 生成的汇总里，每个「许可全文」段落前最多列几个适用组件名（剩下的只报数）。
SAMPLE_NAMES = 4

# 本仓自己的 crate 前缀：这些是本地 path 依赖，没有 source，天然被过滤掉；
# 这里只是把「万一被写进表」的情况也堵死。
OWN_PREFIX = "duck-"


def cargo_home() -> Path:
    """cargo 缓存根：优先 `CARGO_HOME`，否则 `~/.cargo`。"""
    env = os.environ.get("CARGO_HOME")
    if env:
        return Path(env).expanduser()
    return Path.home() / ".cargo"


def read_toml(path: Path):
    """读 TOML；失败返回 `None`（缓存里偶尔有不完整的清单，不该让整轮生成崩掉）。"""
    try:
        with open(path, "rb") as handle:
            return tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError):
        return None


def package_manifest(lock_entry: dict) -> dict:
    """把 `Cargo.lock` 的一条 `[[package]]` 规整成内部行。

    `Cargo.lock` 只给了名字 / 版本 / 来源，**没有许可字段**——那是各包自己
    `Cargo.toml` 里的事实，得去缓存里现读。所以这一步只做搬运与来源分类。
    """
    source = lock_entry.get("source")
    origin = None
    kind = None
    if source is None:
        kind = "local"
    elif source.startswith("registry+"):
        kind = "registry"
        origin = "crates.io"
    elif source.startswith("git+"):
        kind = "git"
        # `git+<url>?<query>#<rev>`：取 url 部分，去掉 `.git` 尾巴当出处名。
        url = source[len("git+") :].split("?", 1)[0].split("#", 1)[0]
        origin = re.sub(r"\.git$", "", url)
    else:
        kind = source.split("+", 1)[0]
        origin = source
    return {
        "name": lock_entry["name"],
        "version": lock_entry["version"],
        "kind": kind,
        "origin": origin,
        "source": source,
    }


def git_slug(url: str) -> str:
    """仓库 URL → cargo 的 checkout 目录前缀（末段去 `.git`）。

    cargo 的规则：`https://github.com/longbridge/gpui-fast` → `gpui-fast-<hash>`；
    `…/xim-rs.git` → `xim-rs-<hash>`。
    """
    tail = url.rstrip("/").split("/")[-1]
    return re.sub(r"\.git$", "", tail)


def find_crate_dir(row: dict, cargo: Path) -> Path | None:
    """定位某个包在缓存里的源码目录；找不到返回 `None`（会在报告里点名）。"""
    if row["kind"] == "registry":
        src_root = cargo / "registry" / "src"
        if not src_root.is_dir():
            return None
        target = f"{row['name']}-{row['version']}"
        for index in sorted(src_root.iterdir()):
            candidate = index / target
            if candidate.is_dir():
                return candidate
        return None

    if row["kind"] == "git":
        # `git+<url>?…#<rev>` → 取 rev 前 7 位当 checkout 的子目录名。
        rev = row["source"].split("#", 1)[-1]
        short = rev[:7]
        slug = git_slug(row["source"][len("git+") :].split("?", 1)[0].split("#", 1)[0])
        checkouts = cargo / "git" / "checkouts"
        if not checkouts.is_dir():
            return None
        repos = sorted(p for p in checkouts.iterdir() if p.name.startswith(f"{slug}-"))
        for repo in repos:
            root = repo / short
            if not root.is_dir():
                continue
            found = locate_member(root, row["name"])
            if found is not None:
                return found
        return None

    return None


def locate_member(root: Path, name: str, depth: int = 3) -> Path | None:
    """在 checkout 里按 `package.name` 找成员 crate（workspace 仓库的目录名不等于包名）。

    广度受限（默认 3 层）地走目录树；命中即返回。git 依赖只有 31 个、仓库不到 20 个，
    这点遍历开销可以忽略。
    """
    stack = [(root, 0)]
    while stack:
        current, level = stack.pop(0)
        manifest = current / "Cargo.toml"
        data = read_toml(manifest) if manifest.is_file() else None
        if data and (data.get("package") or {}).get("name") == name:
            return current
        if level >= depth:
            continue
        try:
            children = sorted(p for p in current.iterdir() if p.is_dir())
        except OSError:
            continue
        for child in children:
            # 跳过明显无关的大目录，避免在 checkout 里白走一遍。
            if child.name in {".git", "target", "node_modules"}:
                continue
            stack.append((child, level + 1))
    return None


def workspace_license(start: Path, stop: Path) -> str | None:
    """向上找 workspace 清单里的 `[workspace.package].license`（成员 crate 常写 `license.workspace = true`）。"""
    current = start
    while True:
        data = read_toml(current / "Cargo.toml")
        if data:
            license = (data.get("workspace", {}).get("package", {}) or {}).get("license")
            if isinstance(license, str):
                return license
        if current == stop or current.parent == current:
            return None
        current = current.parent


def resolve_license(crate_dir: Path, stop_at: Path) -> str:
    """取某包的许可声明：`package.license` → `license-file` → workspace 继承 → 未知。"""
    data = read_toml(crate_dir / "Cargo.toml") or {}
    package = data.get("package") or {}
    license = package.get("license")
    if isinstance(license, str) and license.strip():
        return license.strip()
    if isinstance(license, dict) and license.get("workspace"):
        inherited = workspace_license(crate_dir.parent, stop_at)
        if inherited:
            return inherited
    if package.get("license-file"):
        return f"(见 {package['license-file']})"
    return "未声明"


def license_texts(crate_dir: Path, stop_at: Path) -> list[tuple[str, str]]:
    """收集某包随源码附带的许可正文：`[(文件名, 正文)]`。

    先看 crate 自己的目录；没有就向上找（git 依赖的 workspace 根常只放一份）。
    返回按文件名排序，保证可复现。
    """
    candidates: list[Path] = []
    current = crate_dir
    for _ in range(4):
        try:
            entries = sorted(p for p in current.iterdir() if p.is_file())
        except OSError:
            entries = []
        for entry in entries:
            if entry.name.upper().startswith(TEXT_PREFIXES):
                candidates.append(entry)
        if candidates:
            break
        if current == stop_at or current.parent == current:
            break
        current = current.parent

    texts: list[tuple[str, str]] = []
    for path in candidates:
        try:
            if path.stat().st_size > MAX_TEXT_BYTES:
                continue
            raw = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        normalized = normalize_text(raw)
        if normalized.strip():
            texts.append((path.name, normalized))
    return texts


def normalize_text(raw: str) -> str:
    """统一换行、去掉行尾空白与首尾空行——只为了让「内容相同」判断与输出稳定。"""
    text = raw.replace("\r\n", "\n").replace("\r", "\n")
    lines = [line.rstrip() for line in text.split("\n")]
    return "\n".join(lines).strip("\n") + "\n"


def collect(cargo: Path) -> tuple[list[dict], list[dict], list[str]]:
    """扫一遍依赖图。

    返回 `(组件行, 许可全文组, 未定位的组件名)`：组件行按名称排序，全文组按标签排序。
    """
    lock = read_toml(LOCK)
    if not lock or "package" not in lock:
        unsupported(f"读不到依赖图：{LOCK}")

    rows: list[dict] = []
    groups: dict[str, dict] = {}
    missing: list[str] = []

    for entry in lock["package"]:
        row = package_manifest(entry)
        if row["kind"] == "local":
            continue
        if row["name"].startswith(OWN_PREFIX):
            continue

        name = row["name"]
        label = f"{name} {row['version']}"
        crate_dir = find_crate_dir(row, cargo)
        if crate_dir is None:
            missing.append(label)
            row["license"] = "未定位"
            row["texts"] = []
            rows.append(row)
            continue

        # 向上搜索的边界：git 依赖到 checkout 根为止，registry 包就用自己的目录。
        stop_at = crate_dir
        if row["kind"] == "git":
            parts = crate_dir.parts
            if "checkouts" in parts:
                index = parts.index("checkouts")
                # …/git/checkouts/<repo>/<rev>/… → 停在 <rev> 这一层。
                stop_at = Path(*parts[: index + 3])

        row["license"] = resolve_license(crate_dir, stop_at)
        digest_ids: list[str] = []
        for filename, text in license_texts(crate_dir, stop_at):
            digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
            group = groups.setdefault(digest, {"text": text, "names": [], "files": set()})
            group["names"].append(name)
            group["files"].add(filename)
            digest_ids.append(digest)
        row["texts"] = digest_ids
        rows.append(row)

    # 给每个「按内容去重」的正文分配稳定 id：先按标签、再按首个组件名排序。
    for group in groups.values():
        group["licenses"] = Counter()
        group["label"] = sorted(group["files"])[0]
    for row in rows:
        for digest in row["texts"]:
            groups[digest]["licenses"][row["license"]] += 1

    def sort_key(digest: str):
        group = groups[digest]
        label = group["licenses"].most_common(1)
        primary = label[0][0] if label else group["label"]
        return (primary, group["label"], sorted(group["names"])[0])

    ordered = sorted(groups, key=sort_key)
    group_rows: list[dict] = []
    for number, digest in enumerate(ordered, start=1):
        group = groups[digest]
        most = group["licenses"].most_common(1)
        group_rows.append(
            {
                "id": number,
                "digest": digest,
                "text": group["text"],
                "label": most[0][0] if most else group["label"],
                "file": group["label"],
                "names": sorted(set(group["names"])),
            }
        )

    rows.sort(key=lambda item: (item["name"], item["version"]))
    return rows, group_rows, sorted(missing)


def render(rows: list[dict], groups: list[dict], missing: list[str]) -> str:
    """渲染 Markdown 汇总。"""
    index = {group["digest"]: group for group in groups}
    licenses = Counter(row["license"] for row in rows)

    out: list[str] = []
    # 首行是**给读端**的机器可读统计：客户端内嵌这份文件后只解析这一行，
    # 不去正则匹配下面的中文 Markdown（文案一改，正则就悄悄失配）。
    # 它是 HTML 注释，任何 Markdown 渲染器都不会显示。
    out.append(
        f"<!-- duck-licenses: components={len(rows)} texts={len(groups)} "
        f"expressions={len(licenses)} -->"
    )
    out.append("# 第三方开源许可")
    out.append("")
    out.append(
        "本文件由 `sbin/licenses.py` 依据 `Cargo.lock` 与本机 cargo 缓存生成，**请勿手工编辑**。"
    )
    out.append("")
    out.append("- 重新生成：`python3 sbin/licenses.py`")
    out.append("- 校验是否最新：`python3 sbin/licenses.py --check`")
    out.append("")
    out.append(
        f"- 统计：**{len(rows)}** 个第三方组件，"
        f"**{len(licenses)}** 种许可声明，**{len(groups)}** 份许可全文（按内容去重）。"
    )
    out.append(
        "- 口径：`Cargo.lock` 中所有带 `source` 的依赖（crates.io 与 git 两路）；"
        "本仓库自身的 crate 不在列，其版权见仓库根 `LICENSE`（MIT）。"
    )
    out.append("")

    out.append("## 许可分布")
    out.append("")
    out.append("| 许可 | 组件数 |")
    out.append("| --- | --- |")
    for license, count in sorted(licenses.items(), key=lambda item: (-item[1], item[0])):
        out.append(f"| {escape(license)} | {count} |")
    out.append("")

    out.append("## 组件清单")
    out.append("")
    out.append("| 组件 | 版本 | 许可 | 来源 | 许可全文 |")
    out.append("| --- | --- | --- | --- | --- |")
    for row in rows:
        anchors = " ".join(
            f"[{index[digest]['id']}](#text-{index[digest]['id']})" for digest in row["texts"]
        )
        origin = row["origin"] or "-"
        out.append(
            f"| {escape(row['name'])} | {escape(row['version'])} | {escape(row['license'])} "
            f"| {escape(origin)} | {anchors or '—'} |"
        )
    out.append("")

    out.append("## 许可全文")
    out.append("")
    out.append("以下正文按内容去重：同一份文本只列一次，段首给出适用的组件。")
    out.append("")
    for group in groups:
        names = group["names"]
        sample = "、".join(names[:SAMPLE_NAMES])
        more = f" 等 {len(names)} 个" if len(names) > SAMPLE_NAMES else ""
        out.append(f"<a id=\"text-{group['id']}\"></a>")
        out.append("")
        out.append(f"### 文本 {group['id']} · {group['label']}")
        out.append("")
        out.append(f"来源文件：`{group['file']}`")
        out.append("")
        out.append(f"适用组件：{sample}{more}")
        out.append("")
        out.append("```text")
        out.append(group["text"].rstrip("\n"))
        out.append("```")
        out.append("")

    if missing:
        out.append("## 未能定位的组件")
        out.append("")
        out.append("下列组件在本机 cargo 缓存里找不到源码目录（通常是缓存不完整）：")
        out.append("")
        for name in missing:
            out.append(f"- {escape(name)}")
        out.append("")

    return "\n".join(out).rstrip("\n") + "\n"


def escape(text: str) -> str:
    """Markdown 表格单元格内转义竖线（许可声明里会出现 `MIT | Apache-2.0` 这类写法）。"""
    return str(text).replace("|", "\\|")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--check",
        action="store_true",
        help="只校验已生成的文件是否与当前依赖一致；不一致退出 1（不改文件）",
    )
    parser.add_argument("--out", type=Path, default=OUT, help=f"输出路径（默认 {OUT.relative_to(REPO)}）")
    args = parser.parse_args()

    cargo = cargo_home()
    if not (cargo / "registry" / "src").is_dir() and not (cargo / "git" / "checkouts").is_dir():
        unsupported(f"cargo 缓存里没有源码目录：{cargo}（先跑一次 `cargo fetch`）")

    rows, groups, missing = collect(cargo)
    text = render(rows, groups, missing)

    if args.check:
        # 先判「能不能查」再判「一不一致」：只要还有包定位不到，`text` 就带着「未定位」
        # 与末尾那节「未能定位的组件」，拿它去比等于拿一个残缺的快照比完整文件，必然
        # 不相等。这里必须走 2（本机查不了）而不是 1（内容过期），否则就是假阳性——
        # 详见文件头的退出码说明。
        if missing:
            unsupported(
                f"cargo 缓存不完整：{len(missing)} 个组件定位不到源码目录，"
                "先跑 `cargo fetch --locked` 再校验"
            )
        if not args.out.is_file():
            print(
                f"缺少 {args.out.relative_to(REPO)}，请先跑 python3 sbin/licenses.py",
                file=sys.stderr,
            )
            return EXIT_STALE
        current = args.out.read_text(encoding="utf-8")
        if current != text:
            print(
                f"{args.out.relative_to(REPO)} 已过期（依赖变了但汇总没重生成）。\n"
                "请跑：python3 sbin/licenses.py 并提交结果。",
                file=sys.stderr,
            )
            return EXIT_STALE
        print(f"{args.out.relative_to(REPO)} 是最新的。")
        return 0

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(text, encoding="utf-8")
    size = len(text.encode("utf-8"))
    print(
        f"已写入 {args.out.relative_to(REPO)}："
        f"{len(rows)} 个组件 / {len(groups)} 份全文 / {size / 1024:.0f} KiB"
    )
    if missing:
        print(f"⚠️  {len(missing)} 个组件未能在缓存里定位，已列进文件末尾。", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
