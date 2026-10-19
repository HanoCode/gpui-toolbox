#!/usr/bin/env python3
"""从参考仓的 `cheat-sheets-data.ts` 生成 `crates/duck-misc-core/src/cheat_sheets/*.cheat`。

## 为什么要脚本而不是手抄

参考仓那张表是 2390 行 TS（20 张速查表，其中 5 张是上百科级 markdown）。手抄一遍
必然掉字、必然把 ``` 里的空格改掉。这里按「改数据改脚本、再重跑」的纪律做成幂等
生成：**要改速查表内容，先改脚本的输入（参考仓文件），再重跑；不要直接改 .cheat**。

## 输入

`--source` 指向参考仓的 `cheat-sheets-data.ts`（默认按本机路径猜）。文件里每个条目是
一个 `{ name, slug, category, content, commands }` 对象字面量，`content` 是模板字符串，
`commands` 是 `cheatCommands(...)` 包起来的另一段模板字符串。

## 输出格式（本仓扩展，`.cheat` 的超集）

```
%% name Git
%% category 版本控制
%% content
<markdown 正文>
%% commands
<navi 语法：% 标签 / # 描述 / $ var: <命令> / <命令行>>
```

`%%` 三行是**分节指令**，解析器先切段再分别处理；`%% commands` 之后的语法完全按
navi 原样（`#` 是描述、`$` 是变量、其余是模板行），所以参考页「导入 .cheat 目录」的
能力对本仓同样是原生兼容的 —— 导入的文件不需要这三行，整份按命令区解析。

## 转义

TS 模板字符串里的反引号、`$`、反斜杠三种转义序列，落到文本里都要去掉反斜杠；
`\n`（反斜杠 + n 两个字符，出现在 awk 脚本里）必须原样保留。`unescape_ts` 只处理
前三种，正是这个原因。
"""

from __future__ import annotations

import argparse
import pathlib
import re

DEFAULT_SOURCE = pathlib.Path.home() / "code/code/ifm_ai_tools/backup/src/tools/cheat-sheets-data.ts"
# 数据落在内核 crate 里，由 `include_str!` 编译进二进制（参考仓也是把表编译进前端）；
# 放 `plugins/` 会让这个插件成为唯一带数据目录的特例。
OUT_DIR = pathlib.Path(__file__).resolve().parent.parent / "crates/duck-misc-core/src/cheat_sheets"

# 参考仓的分类顺序（`cheatSheetCategories`）：左侧列表按它排，其余分类排后面。
CATEGORY_ORDER = [
    "编程语言",
    "前端框架",
    "版本控制",
    "数据库",
    "CLI/Shell",
    "容器/云",
    "网络/Web",
    "数据格式",
    "工具",
]


def unescape_ts(text: str) -> str:
    """TS 模板字符串的转义还原：只吃 `` \\` `` / `\\$` / `\\\\`，其余原样。"""
    out: list[str] = []
    index = 0
    while index < len(text):
        char = text[index]
        if char == "\\" and index + 1 < len(text):
            following = text[index + 1]
            if following in "`$\\":
                out.append(following)
                index += 2
                continue
        out.append(char)
        index += 1
    return "".join(out)


def read_template(source: str, start: int) -> tuple[str, int]:
    """从 `start`（反引号之后）读到配对的闭合反引号，返回原文与闭引号下标。"""
    index = start
    while index < len(source):
        char = source[index]
        if char == "\\":
            index += 2
            continue
        if char == "`":
            return unescape_ts(source[start:index]), index
        index += 1
    raise ValueError("模板字符串没有闭合")


def field(source: str, name: str, *, after: int = 0) -> tuple[str, int]:
    """取 `name: '...'` 的单引号值，返回 (值, 值之后的下标)。"""
    match = re.compile(rf"{name}:\s*'([^']*)'").search(source, after)
    if match is None:
        raise ValueError(f"找不到字段 {name}")
    return match.group(1), match.end()


def parse_entries(source: str) -> list[dict[str, str]]:
    """按对象块切分。

    锚点用「2 空格缩进的对象开头 + name 字段」而不是 `slug:` —— 字段顺序是
    name → slug → category，拿 slug 当锚点时 category 还在**后面**，会取到上一个条目
    的分类。这个坑第一版就踩了（第一个条目直接 IndexError）。
    """
    blocks = re.split(r"\n  \{\n    name: '", source)[1:]
    entries: list[dict[str, str]] = []
    for block in blocks:
        name = block[: block.index("'")]
        slug_match = re.search(r"slug:\s*'([^']*)'", block)
        category_match = re.search(r"category:\s*'([^']*)'", block)
        if slug_match is None or category_match is None:
            continue

        content = ""
        commands = ""
        content_match = re.compile(r"content:\s*`").search(block)
        if content_match is not None:
            content, cursor = read_template(block, content_match.end())
            commands_match = re.compile(r"commands:\s*cheatCommands\(\s*`").search(block, cursor)
            if commands_match is not None:
                commands, _ = read_template(block, commands_match.end())

        entries.append(
            {
                "name": name,
                "slug": slug_match.group(1),
                "category": category_match.group(1),
                "content": content,
                "commands": commands,
            }
        )
    return entries


def render(entry: dict[str, str]) -> str:
    body = [
        f"%% name {entry['name']}",
        f"%% category {entry['category']}",
        "%% content",
        entry["content"].rstrip("\n"),
    ]
    if entry["commands"].strip():
        body += ["%% commands", entry["commands"].strip("\n")]
    return "\n".join(body) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=pathlib.Path, default=DEFAULT_SOURCE)
    parser.add_argument("--check", action="store_true", help="只比对，不写盘")
    args = parser.parse_args()

    if not args.source.is_file():
        print(f"找不到源文件：{args.source}")
        print("用 --source 指定参考仓的 cheat-sheets-data.ts")
        return 1

    entries = parse_entries(args.source.read_text(encoding="utf-8"))
    if len(entries) < 15:
        # 解析数过少＝锚点规则失效，宁可报错也不要写出一半的表。
        print(f"只解析出 {len(entries)} 张表，明显不对；检查源文件格式")
        return 1

    order = {name: index for index, name in enumerate(CATEGORY_ORDER)}
    entries.sort(key=lambda entry: (order.get(entry["category"], len(order)), entry["slug"]))

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    written = 0
    for entry in entries:
        target = OUT_DIR / f"{entry['slug']}.cheat"
        text = render(entry)
        if args.check:
            current = target.read_text(encoding="utf-8") if target.is_file() else ""
            if current != text:
                print(f"不同步：{target}")
                return 1
            continue
        target.write_text(text, encoding="utf-8")
        written += 1

    if args.check:
        print(f"{len(entries)} 张速查表与源文件一致")
    else:
        print(f"写出 {written} 张速查表 → {OUT_DIR}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
