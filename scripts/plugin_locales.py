#!/usr/bin/env python3
"""校验插件自带的翻译目录 `locales/*.toml`。

背景：插件文案不再走宿主的英译表（第三方插件没法往宿主的
`crates/duck-theme/locales/en.toml` 里加条目），改由**插件自己带翻译**：

    <plugin_dir>/
    ├── plugin.toml        # 可选 `[i18n]` 段：source = "zh-Hans" / locales = [...]
    ├── main.js
    └── locales/
        ├── en.toml        # 文件名 = 语言 tag
        └── zh-Hans.toml

装载与降级链在 `crates/duck-plugin/src/i18n.rs`（运行期读，不是编译期）。
完整方案见 `docs/plugin-i18n-design.md`。

本脚本是**独立校验器**：用标准库 `tomllib` 解析（与 Rust 侧的 `toml` 是两套实现，
结论一致才有意义），检查 Rust loader **刻意宽容**而不拦的那些问题：

1. **空译文**——loader 会丢掉它（回落源串），所以界面不会出错，但作者以为自己翻了；
2. **Rust loader 会静默跳过的文件**（坏 TOML / 超限）——脚本必须让它可见；
3. **清单 `[i18n] locales` 声明列表**——loader 对非法声明只记 warn 就跳过，写错一个
   路径会让那个语言的译文**整份消失**（界面回落中文）而没有任何硬错误；
   顺带报出「在盘上但没被声明」的文件（运行时读不到）；
4. `--strict`：各语言与 `source` 的 key 集合对齐（漏翻 / 多翻）。

为什么这些不进 Rust 的硬门禁：插件是**运行期数据**，一个坏翻译文件不该让插件加载失败、
更不该让宿主起不来（`duck-plugin::i18n::PluginLocales::load` 永不失败，只记 warn）。
所以「宽容运行 + 脚本严查」是刻意的分工。

用法：
    python3 scripts/plugin_locales.py plugins/text-stat          # 校验单个插件
    python3 scripts/plugin_locales.py plugins                    # 校验目录下所有插件
    python3 scripts/plugin_locales.py plugins --strict           # 额外查 key 集合对齐

⚠️ 本脚本**只读**，从不写文件——所以不像 `scripts/i18n_en_toml.py` 那样需要一个
`--check` 来区分「只看不写」。`--check` 作为等价别名收下（习惯用法），
但**未知 flag 一律报错**：笔误被静默忽略会让门禁悄悄降级成非 strict。

默认只扫仓库的 `plugins/`（随发行版分发的示例插件）。`crates/*/tests/fixtures*/`
里放着**故意写坏**的样本（坏 TOML、空译文、缺语言），是给 Rust 单测用的，
不参与本脚本——需要时显式传路径。
"""

import sys
import tomllib
from pathlib import Path, PurePosixPath

DEFAULT_ROOTS = ("plugins",)

LOCALES_DIR = "locales"
MANIFEST_FILE = "plugin.toml"
DEFAULT_SOURCE_TAG = "zh-Hans"

# 与 `crates/duck-plugin/src/i18n.rs` 的常量保持一致：脚本要比 Rust loader 更早报警。
MAX_FILE_BYTES = 256 * 1024
MAX_ENTRIES = 20_000


def fail(message: str) -> None:
    print(f"✗ {message}", file=sys.stderr)
    raise SystemExit(1)


def warn(message: str) -> None:
    print(f"! {message}", file=sys.stderr)


def manifest_i18n(plugin_dir: Path) -> tuple[str, list[str]]:
    """读清单 `[i18n]`，返回 `(source_tag, declared)`。

    `source` 没有 / 写空串 → 回落 `DEFAULT_SOURCE_TAG`；`locales` 没有 / 不是数组 → `[]`
    （`[]` 表示「自动发现 `locales/*.toml`」，与 Rust 侧的 `Option<Vec<String>>` 同义）。

    这里**不**调 `PluginManifest::parse`：清单可能有别的毛病（而本脚本只关心翻译）。
    只做「有就读、没有就算了」，清单本身的合法性由 Rust 侧与 `cargo test` 保证。
    """
    manifest = plugin_dir / MANIFEST_FILE
    if not manifest.is_file():
        return DEFAULT_SOURCE_TAG, []
    try:
        parsed = tomllib.loads(manifest.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as error:
        warn(f"{manifest} 解析失败（清单问题，不在本脚本范围）：{error}")
        return DEFAULT_SOURCE_TAG, []
    section = parsed.get("i18n")
    if not isinstance(section, dict):
        return DEFAULT_SOURCE_TAG, []

    tag = section.get("source")
    source_tag = tag if isinstance(tag, str) and tag.strip() else DEFAULT_SOURCE_TAG

    declared = section.get("locales")
    if isinstance(declared, list) and all(isinstance(item, str) for item in declared):
        return source_tag, declared
    if declared is not None:
        warn(f"{manifest} 的 [i18n] locales 不是字符串数组，本脚本按「未声明」处理")
    return source_tag, []


def check_declared(plugin_dir: Path, declared: list[str], on_disk: list[Path]) -> list[Path]:
    """校验清单声明的 `locales` 列表，返回声明项对应的绝对路径。

    ⚠️ 这一步存在的理由：**Rust loader 对非法声明只记 warn 就跳过**（`resolve_declared`），
    所以清单里写错一个路径，那个语言的译文会**整份消失**（界面回落中文）而没有任何
    硬错误——正是本脚本要提前抓的那类静默失效。规则与 Rust 侧逐条对齐。
    """
    paths = []
    for raw in declared:
        if not raw.strip():
            fail(f"{plugin_dir}: [i18n] locales 里有空串")
        relative = PurePosixPath(raw)
        if relative.is_absolute():
            fail(f"{plugin_dir}: [i18n] locales 里的 {raw!r} 是绝对路径——Rust loader 会拒收")
        # `PurePosixPath` 与 Rust 的 `Path::components()` 一样会把 `.` 归一化掉，
        # 所以 `locales/./en.toml` 两边都判为合法（它逃不出插件目录）。
        if not relative.parts or relative.parts[0] != LOCALES_DIR:
            fail(f"{plugin_dir}: [i18n] locales 里的 {raw!r} 必须位于 {LOCALES_DIR}/ 下")
        if ".." in relative.parts:
            fail(f"{plugin_dir}: [i18n] locales 里的 {raw!r} 含 `..`，会逃出插件目录")
        if relative.suffix != ".toml":
            fail(f"{plugin_dir}: [i18n] locales 里的 {raw!r} 不是 .toml")
        path = plugin_dir / relative
        if not path.is_file():
            fail(
                f"{plugin_dir}: [i18n] locales 声明了 {raw!r}，但文件不存在"
                "——该语言整份不装载，界面会回落中文"
            )
        paths.append(path)

    declared_set = set(paths)
    for path in on_disk:
        if path not in declared_set:
            warn(
                f"{path} 没写进清单 [i18n] locales —— 一旦声明了列表就**只**装载声明的那些，"
                "这个文件运行时读不到"
            )
    return paths


def load_locale(path: Path) -> dict[str, str]:
    """解析一个语言文件，返回 `源串 → 译文`。任何问题都计入失败。"""
    tag = path.stem

    size = path.stat().st_size
    if size > MAX_FILE_BYTES:
        fail(f"{path} 有 {size} 字节，超过上限 {MAX_FILE_BYTES}——Rust loader 会整份跳过")

    try:
        table = tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as error:
        fail(f"{path}（tag={tag}）不是合法 TOML——Rust loader 会整份跳过：{error}")

    if not table:
        fail(f"{path}（tag={tag}）解析出来是空的——检查是不是被截断了")

    # 顶层子表 / 数组 / 数字：Rust loader 反序列化成 `BTreeMap<String, String>` 会失败，
    # 也就是整份跳过。这与「扁平字符串表」的约定不符，必须拦。
    not_string = sorted(key for key, value in table.items() if not isinstance(value, str))
    if not_string:
        fail(
            f"{path}（tag={tag}）里有非字符串值——翻译表必须是扁平的 `\"源串\" = \"译文\"`："
            f"{not_string[:5]}"
        )

    if len(table) > MAX_ENTRIES:
        fail(f"{path}（tag={tag}）有 {len(table)} 条，超过上限 {MAX_ENTRIES}")

    # 空译文：Rust loader 会丢掉它（回落源串），界面因此不会出错——但作者以为自己翻了。
    empty = sorted(key for key, value in table.items() if not value.strip())
    if empty:
        warn(
            f"{path}（tag={tag}）有 {len(empty)} 条空译文（会被当成「没翻」而回落源串）："
            f"{[key[:20] for key in empty[:5]]}"
        )

    return table


def check_plugin(plugin_dir: Path, strict: bool) -> int:
    """校验一个插件目录，返回条目数（0 = 该插件不带翻译）。"""
    locales_dir = plugin_dir / LOCALES_DIR
    on_disk = sorted(locales_dir.glob("*.toml")) if locales_dir.is_dir() else []
    source_tag, declared = manifest_i18n(plugin_dir)

    # 声明列表先过闸：非法路径 / 文件缺失在这里直接 fail，顺带报出「在盘上但没声明」的文件。
    declared_paths = check_declared(plugin_dir, declared, on_disk) if declared else []

    if not on_disk:
        if locales_dir.is_dir():
            warn(f"{locales_dir} 存在但没有 .toml——空目录建议删掉，免得被当成「还没翻译」")
        return 0

    # 装载以**盘上的文件**为准：没被声明的也要校验（check_declared 已额外提醒运行时读不到），
    # 否则「一个坏文件只要不进声明列表就能躲过本脚本」。
    tables: dict[str, dict[str, str]] = {}
    for path in on_disk:
        tables[path.stem] = load_locale(path)

    total = sum(len(table) for table in tables.values())

    if strict and source_tag not in tables:
        # 源串语言没有对应文件是**正常布局**，不是问题：中文即源串的插件（绝大多数）
        # 本来就不会再写一份 `locales/zh-Hans.toml`——查不到就回落源串本身。
        # 只有「别的语言」才需要对齐检查。
        pass
    elif strict:
        baseline = set(tables[source_tag])
        for tag, table in tables.items():
            if tag == source_tag:
                continue
            missing = sorted(baseline - set(table))
            extra = sorted(set(table) - baseline)
            if missing:
                warn(
                    f"{plugin_dir}/{LOCALES_DIR}/{tag}.toml 比源串少 {len(missing)} 条"
                    f"（英文界面会回落中文）：{[key[:20] for key in missing[:5]]}"
                )
            if extra:
                warn(
                    f"{plugin_dir}/{LOCALES_DIR}/{tag}.toml 比源串多 {len(extra)} 条"
                    f"（源串里查不到，永远不会被用到）："
                    f"{[key[:20] for key in extra[:5]]}"
                )

    tags = ", ".join(f"{tag}({len(table)})" for tag, table in sorted(tables.items()))
    suffix = f"（清单声明 {len(declared_paths)} 个）" if declared_paths else ""
    print(f"✓ {plugin_dir}: {len(tables)} 种语言 / {total} 条 — {tags}{suffix}")
    return total


def iter_plugin_dirs(root: Path):
    """产出一个扫描根下的插件目录：根本身是插件就直接给，否则看它的一级子目录。

    子目录**不要求**有 `plugin.toml`：本脚本只关心 `locales/` 是否存在。
    """
    if (root / LOCALES_DIR).is_dir() or (root / MANIFEST_FILE).is_file():
        yield root
        return
    for child in sorted(root.iterdir()):
        if child.is_dir() and not child.name.startswith("."):
            yield child


def main() -> None:
    argv = sys.argv[1:]

    # ⚠️ 未知 flag 必须拦：`--stict` 这种笔误若被静默忽略，门禁会**悄悄降级**成
    # 非 strict 校验，而输出看起来一切正常——最坏的一类失效。
    # `--check` 是照 `i18n_en_toml.py` 的习惯传进来的（那个脚本默认**写文件**，
    # 所以要 `--check` 表示「只看不写」）；本脚本**只读**、本来就是 check-only，
    # 于是收下它当等价别名，而不是让人以为漏了个开关。
    unknown = [
        arg
        for arg in argv
        if arg.startswith("--") and arg not in ("--strict", "--check", "--all")
    ]
    if unknown:
        fail(f"未知参数：{unknown}（可用：--strict / --check / --all）")

    strict = "--strict" in argv
    args = [arg for arg in argv if not arg.startswith("--")]

    if "--all" in argv or not args:
        roots = [Path(item) for item in DEFAULT_ROOTS]
    else:
        roots = [Path(arg) for arg in args]
        for root in roots:
            if not root.exists():
                fail(f"路径不存在：{root}")

    checked = 0
    for root in roots:
        if not root.exists():
            continue
        for plugin_dir in iter_plugin_dirs(root):
            if check_plugin(plugin_dir, strict) > 0:
                checked += 1

    if checked == 0:
        fail("一个带 locales/ 的插件都没扫到——检查扫描路径")

    print(f"✓ 共校验 {checked} 个插件的翻译目录" + ("（strict）" if strict else ""))


if __name__ == "__main__":
    main()
