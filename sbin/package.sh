#!/usr/bin/env bash
# Duck Toolbox 全平台打包入口（macOS 菜单式）。
#
# 用法（macOS 上直接运行会显示交互菜单）：
#   sbin/package.sh                      # 交互式菜单
#   sbin/package.sh --ci                 # CI 静默模式，参数全走下面
#
# CI 模式参数：
#   --mac                只打包 macOS（.app / .zip / .dmg / 更新归档）
#   --win                只打包 Windows（.zip）
#   --linux              只打包 Linux（.tar.gz 便携 + .deb）
#   --rpm                只打包 RPM（需要 Linux 环境）
#   --all                全平台（macOS + Windows + Linux）
#   --clean              打包前先 cargo clean
#   --no-build           跳过编译
#
# 平台支持矩阵：
#   macOS     macOS 10.15+     .app · .zip · .dmg · -app.tar.gz（更新归档）
#   Windows   Windows 10+      .zip 便携 + 更新归档 zip
#   Linux     Ubuntu 20.04+    .tar.gz 便携 + .deb + 更新归档 tar.gz
#   RPM       Fedora 35+       .rpm（需要 Linux 环境）
#
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/_common.sh"

usage() {
    sed -n '4,22p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
}

# ---- 参数 -------------------------------------------------------------------

ci_mode=0
do_mac=0
do_win=0
do_linux=0
do_rpm=0
do_clean=0
do_build=1

while [ $# -gt 0 ]; do
    case "${1}" in
        --ci) ci_mode=1 ;;
        --mac) do_mac=1 ;;
        --win) do_win=1 ;;
        --linux) do_linux=1 ;;
        --rpm) do_rpm=1 ;;
        --all)
            do_mac=1; do_win=1; do_linux=1; do_rpm=1
            ;;
        --clean) do_clean=1 ;;
        --no-build) do_build=0 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "未知参数：${1}（用 -h 查看用法）" ;;
    esac
    shift
done

# ---- 菜单展示 ---------------------------------------------------------------

show_menu() {
    printf '\n%s=== Duck Toolbox 全平台打包 ===%s\n\n' "${C_BOLD}" "${C_RESET}"
    printf '  %s[1]%s  macOS     .app / .zip / .dmg / 更新归档\n' "${C_GREEN}" "${C_RESET}"
    printf '  %s[2]%s  Windows   .zip 便携 + 更新归档\n' "${C_GREEN}" "${C_RESET}"
    printf '  %s[3]%s  Linux     .tar.gz 便携 + .deb\n' "${C_GREEN}" "${C_RESET}"
    printf '  %s[4]%s  Linux RPM .rpm（需 Linux 环境）\n' "${C_GREEN}" "${C_RESET}"
    printf '  %s[5]%s  全平台    全部构建\n' "${C_YELLOW}" "${C_RESET}"
    printf '\n  %s[6]%s  清理      cargo clean 后再构建\n' "${C_DIM}" "${C_RESET}"
    printf '  %s[7]%s  仅构建    跳过编译，直接用现有产物\n' "${C_DIM}" "${C_RESET}"
    printf '  %s[0]%s  退出\n' "${C_RED}" "${C_RESET}"
    printf '\n%s请选择（可多选如 1+2+3）：%s' "${C_BOLD}" "${C_RESET}"
}

run_mac() {
    step ">>> 打包 macOS"
    local args=()
    [ "${do_clean}" -eq 1 ] && args+=(--clean)
    [ "${do_build}" -eq 0 ] && args+=(--no-build)
    args+=(--dmg --zip --app-archive)
    bash "${DUCK_ROOT}/sbin/package-macos.sh" "${args[@]}"
}

run_win() {
    step ">>> 打包 Windows"
    local args=()
    [ "${do_clean}" -eq 1 ] && args+=(-Clean)
    [ "${do_build}" -eq 0 ] && args+=(-NoBuild)
    args+=(-Zip -UpdateArchive)
    if command -v pwsh >/dev/null 2>&1; then
        pwsh "${DUCK_ROOT}/sbin/package.ps1" "${args[@]}"
    else
        warn "跳过 Windows（macOS 上无 PowerShell）"
        info "在 Windows 上手动运行：sbin\\package.ps1 -Zip -UpdateArchive"
    fi
}

run_linux() {
    step ">>> 打包 Linux"
    local args=()
    [ "${do_clean}" -eq 1 ] && args+=(--clean)
    [ "${do_build}" -eq 0 ] && args+=(--no-build)
    args+=(--tarball --deb --update-archive)
    if [ "$(uname -s)" = "Linux" ]; then
        bash "${DUCK_ROOT}/sbin/package-linux.sh" "${args[@]}"
    else
        warn "跳过 Linux（macOS 上无法构建 .deb）"
        info "在 Linux 上手动运行：./sbin/package-linux.sh --tarball --deb --update-archive"
    fi
}

run_rpm() {
    step ">>> 打包 Linux RPM"
    local args=()
    [ "${do_clean}" -eq 1 ] && args+=(--clean)
    [ "${do_build}" -eq 0 ] && args+=(--no-build)
    if [ "$(uname -s)" = "Linux" ]; then
        bash "${DUCK_ROOT}/sbin/package-rpm.sh" "${args[@]}"
    else
        warn "跳过 RPM（macOS 上无法构建 RPM）"
        info "在 Linux 上手动运行：./sbin/package-rpm.sh"
    fi
}

# ---- 交互模式 ---------------------------------------------------------------

interact() {
    show_menu
    printf '\n'

    local selection=""
    read -r selection

    # 重置（防止残留）
    do_mac=0; do_win=0; do_linux=0; do_rpm=0

    case "${selection}" in
        0) echo "已取消"; exit 0 ;;
        1) do_mac=1 ;;
        2) do_win=1 ;;
        3) do_linux=1 ;;
        4) do_rpm=1 ;;
        5) do_mac=1; do_win=1; do_linux=1; do_rpm=1 ;;
        6) do_clean=1; interact; return ;;
        7) do_build=0; interact; return ;;
        *plus*|*[\ \t]*)
            echo "${selection}" | tr '+' '\n' | tr -d ' ' | grep -v '^$' | while read -r n; do
                case "${n}" in
                    1) do_mac=1 ;;
                    2) do_win=1 ;;
                    3) do_linux=1 ;;
                    4) do_rpm=1 ;;
                    5) do_mac=1; do_win=1; do_linux=1; do_rpm=1 ;;
                esac
            done
            ;;
    esac
}

# ---- 主流程 -----------------------------------------------------------------

if [ "${ci_mode}" -eq 0 ] && [ $# -eq 0 ]; then
    interact
fi

if [ "${do_mac}" -eq 0 ] && [ "${do_win}" -eq 0 ] && [ "${do_linux}" -eq 0 ] && [ "${do_rpm}" -eq 0 ]; then
    die "未指定任何平台（用 -h 查看用法）"
fi

[ "$(uname -s)" = "Darwin" ] || die "全平台菜单入口需要在 macOS 上运行"

require_cmd cargo

version="$(duck_version)"
[ -n "${version}" ] || die "无法从 Cargo.toml 解析版本号"

step "Duck Toolbox v${version} · 全平台打包"
echo

[ "${do_mac}"  -eq 1 ] && run_mac
[ "${do_win}"  -eq 1 ] && run_win
[ "${do_linux}" -eq 1 ] && run_linux
[ "${do_rpm}"   -eq 1 ] && run_rpm

echo
step "全部完成"
info "产物目录：${DUCK_DIST_DIR}/"
ls -la "${DUCK_DIST_DIR}"/ 2>/dev/null | sed 's/^/    /' || true
