#!/usr/bin/env bash
# 构建 process 轨插件的可执行文件并**投放**到各插件目录（开发用）。
#
# 用法：
#   sbin/plugins.sh              # debug
#   sbin/plugins.sh --release
#
# 为什么需要这一步：process 轨的清单只能声明**相对插件目录**的入口
# （`[process] bin = "bin/run"`），而 cargo 的产物在 `target/<profile>/`。
# 把二进制拷进插件目录是「安装包长什么样」的预览——发布版由打包脚本做同一件事。
# `plugins/*/bin/` 已进 `.gitignore`：产物不入库，缺了就重跑本脚本。
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/_common.sh"

profile="debug"
while [ $# -gt 0 ]; do
    case "$1" in
        --release) profile="release" ;;
        --debug) profile="debug" ;;
        -h | --help)
            sed -n '2,12p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) die "未知参数：${1}" ;;
    esac
    shift
done

require_cmd cargo
cd "$DUCK_ROOT"

step "构建 process 轨插件（${profile}）"
# 这里必须与 `binary_for` 的值域一致：声明了 `[process]` 却漏建二进制时，
# 下面的 `cp target/${profile}/${bin}` 会失败并把整个脚本带停。
build_args=(
    -p duck-sql-tool -p duck-merge-tool -p duck-compare-tool
    -p duck-files-tool -p duck-net-tool -p duck-misc-tool
    -p duck-codec-tool -p duck-finance-tool
)
if [ "$profile" = release ]; then
    cargo build --release "${build_args[@]}"
else
    cargo build "${build_args[@]}"
fi

# id 前缀 → 二进制。一个二进制服务整族：同族的每个插件目录都拿到同一份 `bin/run`。
# 新加一个**族**时在这里加一行；同族里新加工具不用改脚本。
binary_for() {
    case "$1" in
        duck.sql.*) echo "duck-sql-tool" ;;
        duck.merge.*) echo "duck-merge-tool" ;;
        duck.compare.*) echo "duck-compare-tool" ;;
        duck.files.*) echo "duck-files-tool" ;;
        duck.net.*) echo "duck-net-tool" ;;
        duck.misc.*) echo "duck-misc-tool" ;;
        duck.codec.*) echo "duck-codec-tool" ;;
        # 金融族：PDM 阅读器与渠道网关（含 ESB 代码生成）同一个内核二进制。
        duck.finance.*) echo "duck-finance-tool" ;;
        # 速查手册的 id 留在 `duck.text.*` 下（参考仓把它归在「文本/生成」），
        # 但内核在 `duck-misc-core`。这一条必须在通配那条**前面**。
        duck.text.cheat-sheets) echo "duck-misc-tool" ;;
        # 文本族的两个 process 轨工具（数据格式转换、随机文本生成）与文件族同内核。
        duck.text.*) echo "duck-files-tool" ;;
        *) echo "" ;;
    esac
}

staged=0
for dir in plugins/*/; do
    manifest="${dir}plugin.toml"
    [ -f "$manifest" ] || continue
    grep -q '^\[process\]' "$manifest" || continue
    # 清单的第一行 `id = "..."` 就是插件 id，也就是工具的 id（ADR-7）。
    plugin_id="$(sed -n 's/^id = "\(.*\)"$/\1/p' "$manifest" | head -1)"
    bin="$(binary_for "$plugin_id")"
    if [ -z "$bin" ]; then
        die "${dir} 声明了 [process]，但 id 前缀（${plugin_id}）没有对应二进制——在 binary_for 里加一行"
    fi
    mkdir -p "${dir}bin"
    cp "target/${profile}/${bin}" "${dir}bin/run"
    chmod +x "${dir}bin/run"
    staged=$((staged + 1))
done

step "已投放 ${staged} 个插件二进制"
