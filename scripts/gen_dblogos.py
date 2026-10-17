#!/usr/bin/env python3
"""生成 `crates/duck-components/src/dblogos.rs` 的品牌 logo 静态表。

用法：
    python3 scripts/gen_dblogos.py <dbx 检出目录>

例如：
    python3 scripts/gen_dblogos.py temp/dbx

数据来源（dbx 为 Apache-2.0）：`<dbx>/docs/public/icons/database/*`。
脚本把这些图标拷进 `crates/duck-components/assets/dblogos/`（已存在的不动），
再按目录现状重生成 `dblogos.rs` 的三个 match（枚举 / `ALL` / `asset_path` /
`bytes`）。**目录是唯一真源**：手工加图标后重跑本脚本即可，不要手改生成段。

变体命名规则：文件名去扩展后按 `-` / `_` 分段、各段首字母大写拼起来
（`apache_ignite` -> `ApacheIgnite`）；单词档只把首字母大写
（`neo4j` -> `Neo4j`，与既有 15 个变体的命名一致）。个别缩写给可读名，
见脚本里的 `SPECIAL`（`dm` -> `Dameng`、`spark-logo` -> `Spark`…）。

格式说明：SVG / PNG / WebP **都渲染得了** —— gpui 的 `ImageAssetLoader`
先 `image::guess_format`，位图走 `image` crate 解码，识别不出才交给
`svg_renderer`。所以 dbx 只有位图的那批品牌（瀚高 / GBase / 崖山…）也一并收。
"""

from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
ASSETS = REPO / "crates" / "duck-components" / "assets" / "dblogos"
OUT = REPO / "crates" / "duck-components" / "src" / "dblogos.rs"

# 文件名（去扩展） -> 变体名。机械规则给不出可读名的缩写在这里点名。
SPECIAL = {
    "dm": "Dameng",
    "db2": "Db2",
    "spark-logo": "Spark",
    "manticoresearch": "ManticoreSearch",
    "victoriametrics": "VictoriaMetrics",
}

HEADER = '''//! 数据库品牌 logo（dbx 项目同源，全彩图）。
//!
//! 品牌标志有固有色（MySQL 蓝、Redis 红…），**不能**走 `DuckIcon` 的单色
//! `IconNamed` 通道（会被 `currentColor` 渲染成主题前景色）；这里走
//! `gpui::img(Resource::Embedded(path))`——SVG 由 `svg_renderer` 渲染（保留
//! SVG 内的 `fill` 颜色），PNG / WebP 位图由 `image` crate 解码，品牌色都随
//! 资源走。
//!
//! 资源与枚举**都由 [`scripts/gen_dblogos.py`] 生成、不要手改**：
//!
//! ```bash
//! python3 scripts/gen_dblogos.py temp/dbx
//! ```
//!
//! 数据源是 dbx（Apache-2.0）`docs/public/icons/database/` 下的整目录；dbx
//! 只有位图的品牌（瀚高 / GBase / 崖山 / NebulaGraph…）也一并收进。

use gpui::{IntoElement, Pixels, Styled as _, img, px};
'''

FOOTER = '''
/// 卡片里的默认 logo 边长。
#[must_use]
pub fn card_logo_size() -> Pixels {
    px(22.)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use gpui::AssetSource as _;

    use crate::ComponentAssets;
    use crate::dblogos::DatabaseLogo;

    #[test]
    fn every_logo_path_is_unique_and_loads_an_image() -> gpui::Result<()> {
        let mut seen = HashSet::new();
        for logo in DatabaseLogo::ALL {
            assert!(seen.insert(logo.asset_path()), "duplicate {logo:?}");
            let loaded = ComponentAssets.load(logo.asset_path())?;
            let bytes: &[u8] = loaded.as_deref().unwrap_or_default();
            // SVG 有多种合法开头（`<?xml` 声明 / DOCTYPE / `<!--` 注释 / 前导空白），
            // gpui 的 svg_renderer 都吃得下；位图认 PNG / WebP 的魔数就够。
            let start = bytes
                .iter()
                .position(|b| !b.is_ascii_whitespace())
                .unwrap_or(0);
            let trimmed = &bytes[start..];
            let is_svg = trimmed.starts_with(b"<svg")
                || trimmed.starts_with(b"<?xml")
                || trimmed.starts_with(b"<!DOCTYPE")
                || trimmed.starts_with(b"<!--");
            let is_bitmap = bytes.starts_with(b"\\x89PNG")
                || (bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP");
            assert!(
                is_svg || is_bitmap,
                "{logo:?} missing or not a recognised image format"
            );
        }
        assert!(ComponentAssets.load("duck/dblogos/nope.svg")?.is_none());
        Ok(())
    }

    #[test]
    fn logos_are_listed_under_duck_dblogos() -> gpui::Result<()> {
        let listed = ComponentAssets.list("duck/dblogos/")?;
        assert_eq!(listed.len(), DatabaseLogo::ALL.len());
        Ok(())
    }
}
'''


def variant_of(stem: str) -> str:
    """文件名（去扩展） -> 枚举变体名。"""
    if stem in SPECIAL:
        return SPECIAL[stem]
    if "-" in stem or "_" in stem:
        return "".join(part[:1].upper() + part[1:] for part in stem.replace("-", "_").split("_"))
    return stem[:1].upper() + stem[1:]


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    dbx_icons = Path(sys.argv[1]) / "docs" / "public" / "icons" / "database"
    if not dbx_icons.is_dir():
        sys.exit(f"找不到图标目录：{dbx_icons}")

    # ① 拷贝：目标目录没有的才拷（已存在的不动 —— 本仓的 15 张与 dbx 同源，
    #    但万一将来手工调过某张，覆盖会无声丢掉调整）。
    copied = 0
    for src in sorted(dbx_icons.iterdir()):
        if src.suffix.lower() not in (".svg", ".png", ".webp"):
            continue
        dst = ASSETS / src.name
        if not dst.exists():
            shutil.copyfile(src, dst)
            copied += 1
    print(f"拷入 {copied} 个新图标（目录现共 {len(list(ASSETS.iterdir()))} 个文件）")

    # ② 同名多格式只留一张（svg > png > webp）：`milvus.png` + `milvus.webp`
    #    这类成对的，位图里挑压缩更好的 webp 之外一律留 png；有 svg 必留 svg。
    #    落选的从资产目录删掉 —— 没进枚举的文件只会白占仓库。
    by_stem: dict[str, list[Path]] = {}
    for path in sorted(ASSETS.iterdir()):
        if path.is_file() and path.suffix.lower() in (".svg", ".png", ".webp"):
            by_stem.setdefault(path.stem, []).append(path)
    keep: list[str] = []
    for stem, paths in sorted(by_stem.items()):
        order = {".svg": 0, ".png": 1, ".webp": 2}
        paths.sort(key=lambda p: order.get(p.suffix.lower(), 9))
        keep.append(paths[0].name)
        for loser in paths[1:]:
            loser.unlink()
            print(f"丢弃 {loser.name}（保留 {paths[0].name}）")

    files = sorted(keep)
    seen: set[str] = set()
    rows: list[tuple[str, str]] = []  # (变体名, 文件名)
    for name in files:
        variant = variant_of(Path(name).stem)
        if variant in seen:
            sys.exit(f"变体名冲突：{name} -> {variant}")
        seen.add(variant)
        rows.append((variant, name))

    # 每段都以 `\n` 收尾再拼接（HEADER / FOOTER 自带换行，行模板不带 —— 统一在这里补）。
    body: list[str] = []
    body += ["/// 一张数据库品牌 logo。\n", "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]\n", "pub enum DatabaseLogo {\n"]
    body += [f"    {variant},\n" for variant, _ in rows]
    body += ["}\n\n", "impl DatabaseLogo {\n", "    /// 全部 logo（资源枚举与测试用）。\n", "    pub const ALL: &'static [DatabaseLogo] = &[\n"]
    body += [f"        Self::{variant},\n" for variant, _ in rows]
    body += ["];\n\n", "    /// 嵌入资源路径。\n", "    pub const fn asset_path(self) -> &'static str {\n", "        match self {\n"]
    body += [f'            Self::{variant} => "duck/dblogos/{name}",\n' for variant, name in rows]
    body += ["        }\n", "    }\n\n", "    pub(crate) const fn bytes(self) -> &'static [u8] {\n", "        match self {\n"]
    body += [f'            Self::{variant} => include_bytes!("../assets/dblogos/{name}"),\n' for variant, name in rows]
    body += ["        }\n", "    }\n\n", "    /// 渲染为 `size` 见方的全彩品牌图（驱动选择卡 / 详情页头用）。\n", "    #[must_use]\n", "    pub fn element(self, size: Pixels) -> impl IntoElement {\n", "        img(self.asset_path()).size(size)\n", "    }\n", "}\n"]
    OUT.write_text(HEADER + "".join(body) + FOOTER, encoding="utf-8")
    print(f"写入 {OUT.relative_to(REPO)}：{len(rows)} 个变体")


if __name__ == "__main__":
    main()
