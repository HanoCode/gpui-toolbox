#!/usr/bin/env python3
"""校验英译数据源 `locales/en.toml`，并同步 key 清单。

背景：i18n 已迁到 rust-i18n。英译的**唯一数据源**是
`crates/duck-theme/locales/en.toml`，由 `crates/duck-theme/src/i18n_backend.rs`
在**编译期**装载（`rust_i18n::i18n!("locales", fallback = "en")`）。

本脚本是它的**独立校验器**：用标准库 `tomllib` 解析（与 rust-i18n 的 loader 是两套
实现，结论一致才有意义），并把 key 清单写到
`crates/duck-theme/tests/data/en_keys.txt`。Rust 侧两条断言依赖这份清单：

- `every_listed_key_resolves_through_the_projection` —— 清单里每条 key 都要能在
  rust-i18n 后端里查到，即 **miss = 0**。miss 的表现是「英文界面静默显示中文」，
  编译过、`cargo check` 也不报，属于最难发现的坏法。
- `backend_and_key_list_agree_on_the_entry_count` —— 后端条目数 == 清单行数，
  让两个解析器的结果逐条对齐。

**唯一的硬检查：顶层每个值都必须是字符串。** 这一条同时挡住两类会让 key「改形」的
写法（后果都是全表 miss 而**不报错**）：

1. 顶层子表（`[plain]` / `[some.key]`）——loader 会按 `.` 拼 key，实际 key 变成
   `plain.外观`；
2. 值写成数组 / 数字 / 布尔 —— loader 会当成空串。

注：key **带引号**（`"a.b" = "x"`）时 `.` 是字面量的一部分，不会被展开成嵌套表，
所以 key 里出现 `.` 是允许的（本表里就有 `10.0.1.13 或主机名` 这类）。
反过来说，**新增条目务必给 key 加引号**。

用法：
    python3 scripts/i18n_en_toml.py            # 校验 + 同步 key 清单
    python3 scripts/i18n_en_toml.py --check    # 只校验、不写文件（提交前 / CI）

新增英译：在 `locales/en.toml` 里加一行 `"中文源串" = "English"`（**顺序无关**），
再跑一次本脚本。复数形态不在 TOML 里 —— 见 `src/i18n.rs` 的 `EN_PLURAL`。
"""

import sys
import tomllib
from pathlib import Path

TOML_PATH = Path("crates/duck-theme/locales/en.toml")
KEYS_PATH = Path("crates/duck-theme/tests/data/en_keys.txt")


def fail(message: str) -> None:
    print(f"✗ {message}", file=sys.stderr)
    raise SystemExit(1)


def byte_sorted(keys) -> list[str]:
    """按 UTF-8 字节序升序 —— 与 Python 的默认 `sorted` 不同（后者按码位）。"""
    return sorted(keys, key=lambda key: key.encode("utf-8"))


def main() -> None:
    check_only = "--check" in sys.argv[1:]

    # `tomllib` 遇到重复 key 直接抛错，所以这一步同时也是「key 唯一」检查。
    table = tomllib.loads(TOML_PATH.read_text(encoding="utf-8"))

    not_string = byte_sorted(key for key, value in table.items() if not isinstance(value, str))
    if not_string:
        fail(
            "en.toml 里有非字符串值（子表 / 数组 / 数字）——rust-i18n 会把它们当空串，"
            "而顶层子表还会让其余 key 被拼成 `section.key` 从而全表 miss："
            f"{not_string[:5]}"
        )

    keys = byte_sorted(table)
    if not keys:
        fail("en.toml 解析出来是空的 —— 检查文件是不是被截断了")
    payload = "\n".join(keys) + "\n"

    if check_only:
        current = KEYS_PATH.read_text(encoding="utf-8") if KEYS_PATH.exists() else ""
        if current != payload:
            fail("key 清单与 en.toml 不一致 —— 跑 `python3 scripts/i18n_en_toml.py` 同步")
        print(f"✓ en.toml {len(keys)} 条，key 清单一致")
        return

    KEYS_PATH.parent.mkdir(parents=True, exist_ok=True)
    KEYS_PATH.write_text(payload, encoding="utf-8")
    print(f"✓ en.toml {len(keys)} 条 → 已同步 {KEYS_PATH}")


if __name__ == "__main__":
    main()
