#!/usr/bin/env bash
# Duck Toolbox 构建脚本共用库。
# 由 run.sh / package.sh 通过 source 引入，不单独执行。
# 兼容 macOS 自带的 bash 3.2：不使用关联数组 / mapfile / ${var,,}，
# 且所有变量展开一律写成 ${VAR}（避免 bash 3.2 的 UTF-8 标识符解析 bug，见下方"环境"）。

# ---- 路径与常量 -------------------------------------------------------------

# 仓库根目录：本文件位于 <root>/sbin/
DUCK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
readonly DUCK_ROOT

readonly DUCK_CRATE="duck-app"          # 承载可执行文件的 crate 名
readonly DUCK_BIN_NAME="duck"           # [[bin]] name
readonly DUCK_APP_NAME="Duck"           # .app 显示名
readonly DUCK_BUNDLE_ID="com.duck.toolbox"
readonly DUCK_MIN_MACOS="11.0"

# 产物落盘目录（已在 .gitignore 中忽略）
readonly DUCK_DIST_DIR="$DUCK_ROOT/dist"

# ---- 环境 -------------------------------------------------------------------

# macOS 自带 bash 3.2 在 UTF-8 locale 下有个标识符解析 bug：变量展开后紧跟多字节
# 字符时，会把该字符的字节并入变量名 —— `"$VAR（"` 被解析成 `"$VAR\xEF…"`，
# 报 "unbound variable"（错误信息里显示成 `VAR?: unbound variable`）。
# （在 C / POSIX locale 下反而正常，因为此时按单字节处理。）
# 规避措施有两层：
#   1) sbin/ 下所有脚本的变量展开一律写成 ${VAR} —— 花括号界定边界，任何 locale 都正确；
#   2) 在 bash 3.x 上把 LC_CTYPE 退到 C，兜住任何遗留写法。
# C locale 只影响 bash 自身的字符分类；UTF-8 字节仍原样透传到终端，中文显示不受影响。
if [ -z "${LC_ALL:-}" ]; then
    case "${BASH_VERSION:-}" in
        3.*)
            case "${LC_CTYPE:-${LANG:-}}" in
                *UTF-8* | *utf8* | *UTF8*)
                    LC_CTYPE=C
                    export LC_CTYPE
                    ;;
            esac
            ;;
    esac
fi

# 从 [workspace.package] 读取版本号
duck_version() {
    awk -F'"' '/^version = /{print $2; exit}' "$DUCK_ROOT/Cargo.toml"
}

# 宿主架构对应的 Rust target triple
duck_host_triple() {
    case "$(uname -m)" in
        arm64 | aarch64) echo "aarch64-apple-darwin" ;;
        x86_64) echo "x86_64-apple-darwin" ;;
        *) echo "" ;;
    esac
}

# Rust target triple → 更新 manifest 平台键里的架构段（macos-arm64 / macos-x64）。
# 认不出来时输出空串，由调用方决定是报错还是回退。
# 注意：这与 Cargo/rustc 的架构名（aarch64 / x86_64）**不同**——它是对外的线格式，
# 跟 crates/duck-update/src/platform.rs 的 PlatformKey::key() 绑死，不能随手改。
duck_arch_from_triple() { # $1 = target triple
    case "$1" in
        aarch64-apple-darwin) echo "arm64" ;;
        x86_64-apple-darwin) echo "x64" ;;
        *) echo "" ;;
    esac
}

# ---- 输出 -------------------------------------------------------------------

if [ -t 1 ]; then
    C_RESET=$'\033[0m'
    C_DIM=$'\033[2m'
    C_BOLD=$'\033[1m'
    C_RED=$'\033[31m'
    C_GREEN=$'\033[32m'
    C_YELLOW=$'\033[33m'
    C_BLUE=$'\033[36m'
else
    C_RESET=""; C_DIM=""; C_BOLD=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_BLUE=""
fi

step() { printf '%s==>%s %s%s%s\n' "$C_BLUE" "$C_RESET" "$C_BOLD" "$*" "$C_RESET"; }
info() { printf '    %s%s%s\n' "$C_DIM" "$*" "$C_RESET"; }
ok() { printf '%s  ok%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%swarn%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die() {
    printf '%serror%s %s\n' "$C_RED" "$C_RESET" "$*" >&2
    exit 1
}

human_size() { # $1 = 文件路径
    if [ -e "$1" ]; then
        du -sh "$1" 2>/dev/null | awk '{print $1}'
    else
        echo "-"
    fi
}

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "缺少命令：${1}（请先安装）"
}
