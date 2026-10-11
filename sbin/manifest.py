#!/usr/bin/env python3
"""从已打包的产物生成 Duck Toolbox 的更新 manifest（update.json）。

用法：
    sbin/manifest.py --version 0.2.0 --tag v0.2.0 --repo HanoCode/gpui-toolbox \
        --asset-dir dist --notes-file notes.md --out dist/update.json

它只做一件客户端做不了的事：把 manifest 里**得从产物本身算出来**的字段填上——
`sha256` 与 `size`——再把下载地址拼出来。文档形状与「哪些字段是必须的」由
`crates/duck-update/src/manifest.rs` 定义：那边是读端（会拒绝的都在那边），这里是写端。

# 为什么是 Python 而不是 shell 或再加一个 Rust bin

JSON 转义与 SHA-256 在 shell 里都要靠拼字符串，文件名带空格或非 ASCII 时会出错；
而 GitHub 的三个 runner 镜像（macOS / Ubuntu / Windows）都自带 python3，不必编译、
不必进 `sbin/*.sh` 那套 bash 3.2 的兼容约束。写端只在 CI 与本地发版时用，
不进客户端二进制。

# 加速通道

给了 `--mirror-base`（CDN 的公开前缀，如 Cloudflare R2 / 阿里云 OSS 上的目录）时，
manifest 会把**镜像地址当主地址**、GitHub Release 原址放进 `mirrors` 作回退；
客户端按 `url` → `mirrors` 的顺序试，第一个能用的就下。不传这个参数就只写 GitHub
地址——客户端照常工作，只是慢一些。换通道因此是一次「只动 manifest」的操作，
客户端一个字节都不用改。

# 签名

给了 `--sign-key <私钥文件>` 时，除了 `update.json` 还会写出 `update.json.sig`
（minisign 线格式，见 `sbin/minisign_sign.py`）。客户端内建了公钥时会**强制**校验：
`<out>` 与 `<out>.sig` 两个文件都要上传到发布页，地址就是 manifest 地址后面加 `.sig`。

**不给私钥不等于「客户端也不校验」**：内建了公钥的构建会直接拒绝一份没签名的 manifest。
这是刻意的——本地开发构建（没有内建公钥）照常能用，而正式发布漏签会当场暴露，
不会悄悄放行一份可以被投毒的文档。
"""

import argparse
import hashlib
import json
import os
import sys
import time
from datetime import datetime, timezone

# 与 crates/duck-update/src/manifest.rs 的 SUPPORTED_MANIFEST_VERSION 对应。
MANIFEST_VERSION = 1

# 平台键 → 产物文件名模板。**这是与客户端的线格式契约**：
#   键     = crates/duck-update/src/platform.rs 的 PlatformKey::key()
#   文件名 = sbin/package.sh / sbin/package.ps1 的产出名
# 改任何一边都要同步另一边：漏掉一个平台，那个平台的用户拿到的是
# 「the manifest has no entry for <平台>」这样一条**硬失败**——刻意的，不是静默跳过。
ASSETS = {
    "macos-arm64": "duck-{version}-macos-arm64-app.tar.gz",
    "macos-x64": "duck-{version}-macos-x64-app.tar.gz",
    "windows-x64": "duck-{version}-windows-x64.zip",
}

GITHUB_BASE = "https://github.com/{repo}/releases/download"

# 分块读的大小：足够快，又不至于把几十 MB 的包整个读进内存。
BLOCK = 1024 * 1024


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


def entry_for(args, key, name, path):
    """一条 `files` 条目：地址 + 备用地址 + 摘要 + 大小。"""
    github_url = "{base}/{tag}/{name}".format(
        base=GITHUB_BASE.format(repo=args.repo), tag=args.tag, name=name
    )

    if args.mirror_base:
        # 镜像当主地址：国内客户端先走它，取不到再回 GitHub 原址。
        url = "{base}/{tag}/{name}".format(
            base=args.mirror_base.rstrip("/"), tag=args.tag, name=name
        )
        mirrors = [github_url]
    else:
        url = github_url
        mirrors = []

    size = os.path.getsize(path)
    if size == 0:
        # 客户端在解析期就拒绝零字节（进度会除零，而且「读少了」与「读完了」
        # 无法区分）。在这里拦下，报错时能直接点名是哪个文件。
        raise SystemExit("产物是空文件：{}".format(path))

    return {
        "url": url,
        "mirrors": mirrors,
        "sha256": sha256_of(path),
        "size": size,
        # 产物级签名（逐包签）**没有启用**：整份 update.json 的 detached 签名已经覆盖了
        # 这里每一个 sha256 与每一条地址，再签一遍每个包只是把信任建立在同一把钥匙上。
        # 字段留着是因为客户端 schema 里有它。
        "signature": None,
    }


def build_document(args, notes):
    """整份文档。纯函数，便于在本地对着一个临时目录验证。"""
    files = {}
    missing = []

    # 按平台键排序：输出对 diff 友好，也不受文件系统枚举顺序影响。
    for key in sorted(ASSETS):
        name = ASSETS[key].format(version=args.version)
        path = os.path.join(args.asset_dir, name)
        if not os.path.isfile(path):
            missing.append(name)
            continue
        files[key] = entry_for(args, key, name, path)

    if missing:
        detail = "、".join(missing)
        if not args.allow_missing:
            raise SystemExit(
                "缺少产物：{}\n"
                "缺哪个平台，那个平台就永远收不到更新（客户端会报「没有对应条目」）。\n"
                "确实要发一个残缺的版本，加 --allow-missing。".format(detail)
            )
        print("警告：缺少产物 {}，本次 manifest 不含这些平台".format(detail), file=sys.stderr)

    if not files:
        raise SystemExit("一个产物都没有，manifest 会是空的")

    document = {
        "manifest_version": MANIFEST_VERSION,
        "channel": args.channel,
        "version": args.version,
        "notes": notes,
        "published_at": args.published_at or now_rfc3339(),
    }
    # 只在真的立了门槛时才写这个键：默认输出保持与老版本逐字段一致，
    # 这样「契约测试逐字段照抄生成器输出」这件事不会被一个空字段打乱。
    if args.min_app_version:
        document["min_app_version"] = args.min_app_version
    document["files"] = files
    return document


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description="从已打包的产物生成 Duck Toolbox 的 update.json",
    )
    parser.add_argument("--version", required=True, help="语义化版本号，如 0.2.0")
    parser.add_argument("--tag", required=True, help="Git 标签，如 v0.2.0；用于拼下载地址")
    parser.add_argument(
        "--repo", required=True, help="owner/repo，如 HanoCode/gpui-toolbox"
    )
    parser.add_argument("--asset-dir", required=True, help="产物所在目录")
    parser.add_argument("--out", required=True, help="写出的 update.json 路径")
    parser.add_argument(
        "--notes-file",
        default="",
        help="发布说明（Markdown 原样保留，由界面决定怎么显示）",
    )
    parser.add_argument(
        "--mirror-base",
        default="",
        help="CDN 公开前缀；给了它就把镜像当主地址、GitHub 原址放进 mirrors",
    )
    parser.add_argument(
        "--channel",
        default="stable",
        choices=["stable", "beta"],
        help="发布通道",
    )
    parser.add_argument(
        "--published-at",
        default="",
        help="发布时间戳（RFC 3339）；默认取当前 UTC 时间，给定值便于可复现地生成",
    )
    parser.add_argument(
        "--allow-missing",
        action="store_true",
        help="允许缺少某个平台的产物（默认不允许：残缺的发布是更糟的失败）",
    )
    parser.add_argument(
        "--min-app-version",
        default="",
        help="能装这份更新的最低应用版本；立门槛后更老的客户端会被明确拒绝而不是半懂地装",
    )
    parser.add_argument(
        "--sign-key",
        default="",
        help="私钥文件（sbin/minisign_sign.py --generate-key 产出）；给了就写出 <out>.sig",
    )
    parser.add_argument(
        "--sig-out",
        default="",
        help="签名输出路径；默认 <out>.sig（客户端按同一个地址加 .sig 去取）",
    )
    parser.add_argument(
        "--sign-comment",
        default="",
        help='写进 "trusted comment:" 的正文；默认是时间戳 + 文件名',
    )
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)

    notes = ""
    if args.notes_file:
        with open(args.notes_file, encoding="utf-8") as handle:
            notes = handle.read().strip()

    document = build_document(args, notes)

    out_dir = os.path.dirname(os.path.abspath(args.out))
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    # 先序列化成字节，再落盘、再签名：签的必须是**客户端将读到的那串字节**。
    # 分两次 dump 会埋下「签的和写的差一个换行」这种最难查的坑。
    payload = (json.dumps(document, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    with open(args.out, "wb") as handle:
        handle.write(payload)

    platforms = "、".join(sorted(document["files"]))
    print("已写出 {}（v{}，{}）".format(args.out, args.version, platforms))

    write_signature(args, payload)
    return 0


def write_signature(args, payload):
    """写出 `<out>.sig`。没给 `--sign-key` 就把它当成一次**明确的**选择说清楚。"""
    if not args.sign_key:
        print(
            "警告：没有 --sign-key，这份 manifest 没有签名；"
            "内建了公钥的客户端会拒绝它（这是设计如此，不是 bug）。",
            file=sys.stderr,
        )
        return

    # manifest.py 与 minisign_sign.py 同在 sbin/ 下；直接跑脚本时 sys.path[0] 就是这里，
    # 但被别处 import 时不是，所以显式补一下。
    here = os.path.dirname(os.path.abspath(__file__))
    if here not in sys.path:
        sys.path.insert(0, here)
    import minisign_sign

    seed, key_id = minisign_sign.load_key(args.sign_key)
    comment = args.sign_comment or "timestamp:{0}\tfile:{1}".format(
        int(time.time()), os.path.basename(args.out)
    )
    signature = minisign_sign.sign_bytes(seed, key_id, payload, comment)

    out = args.sig_out or (args.out + ".sig")
    with open(out, "w", encoding="utf-8") as handle:
        handle.write(signature)
    print("已写出签名 {}".format(out))


if __name__ == "__main__":
    sys.exit(main())
