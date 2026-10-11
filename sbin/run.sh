#!/usr/bin/env bash
# 编译并启动 Duck Toolbox 桌面应用（开发用）。
#
# 用法：
#   sbin/run.sh                        # debug 构建并启动
#   sbin/run.sh --release              # release 构建并启动
#   sbin/run.sh --no-build             # 跳过编译，直接运行已有产物
#   sbin/run.sh --log-level debug      # 设置 RUST_LOG
#   sbin/run.sh -- --some-flag value   # "--" 之后的参数原样透传给应用
#
# 说明：macOS 只认 .app bundle 的 Dock 图标（gpui 没有运行时设置图标的 API），
# 故 dev 运行也会组装一个轻量 bundle（target/dev-bundle/，二进制用软链、零拷贝）
# 再从其中启动；stdout/stderr 仍直连终端，Ctrl-C 行为不变。
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/_common.sh"

usage() {
    sed -n '2,12p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
}

profile="debug"
do_build=1
log_level=""
app_args=()

while [ $# -gt 0 ]; do
    case "$1" in
        --release) profile="release" ;;
        --debug) profile="debug" ;;
        --no-build) do_build=0 ;;
        --log-level)
            [ $# -ge 2 ] || die "--log-level 需要一个值"
            log_level="$2"
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        --)
            shift
            app_args=("$@")
            break
            ;;
        *) die "未知参数：${1}（应用参数请放在 -- 之后，用 -h 查看用法）" ;;
    esac
    shift
done

require_cmd cargo
cd "$DUCK_ROOT"

if [ "$do_build" -eq 1 ]; then
    step "构建 ${DUCK_CRATE}（${profile}）"
    if [ "$profile" = release ]; then
        cargo build --release -p "$DUCK_CRATE"
    else
        cargo build -p "$DUCK_CRATE"
    fi
fi

bin="$DUCK_ROOT/target/$profile/$DUCK_BIN_NAME"
[ -x "$bin" ] || die "未找到可执行文件：${bin}（去掉 --no-build 重新构建）"

# ---- dev bundle（Dock 图标）---------------------------------------------------
# gpui 无运行时设 Dock 图标的 API，macOS 只在从 .app bundle 启动时才读取
# CFBundleIconFile。注意必须**真实拷贝**二进制：软链会被 realpath 解析回
# target/debug/duck，LaunchServices 随之识别不到 bundle（bundleID=NULL，
# Dock 图标不生效）——实测结论，勿改回软链。
bundle_root="$DUCK_ROOT/target/dev-bundle"
dev_app_dir="$bundle_root/$DUCK_APP_NAME-dev.app"
icon_src="$DUCK_ROOT/assets/AppIcon.icns"
if [ "$(uname -s)" = "Darwin" ] && [ -f "$icon_src" ]; then
    step "组装 dev bundle（Dock 图标）"
    mkdir -p "${dev_app_dir}/Contents/MacOS" "${dev_app_dir}/Contents/Resources"
    # 先删再拷：历史 bundle 可能残留软链，cp 到指向源文件的软链上会报
    # "is <源文件>" 拒绝执行（set -e 下直接中止脚本）。
    rm -f "${dev_app_dir}/Contents/MacOS/${DUCK_BIN_NAME}"
    rm -f "${dev_app_dir}/Contents/Resources/AppIcon.icns"
    cp -f "$bin" "${dev_app_dir}/Contents/MacOS/${DUCK_BIN_NAME}"
    cp -f "$icon_src" "${dev_app_dir}/Contents/Resources/AppIcon.icns"
    cat >"${dev_app_dir}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>zh_CN</string>
    <key>CFBundleExecutable</key>
    <string>${DUCK_BIN_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>${DUCK_BUNDLE_ID}.dev</string>
    <key>CFBundleName</key>
    <string>${DUCK_APP_NAME}</string>
    <key>CFBundleDisplayName</key>
    <string>${DUCK_APP_NAME} (dev)</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST
    bin="${dev_app_dir}/Contents/MacOS/${DUCK_BIN_NAME}"
    ok "dev bundle：${dev_app_dir}"
else
    warn "未找到 assets/AppIcon.icns，dev 运行将使用系统默认图标"
fi

if [ -n "$log_level" ]; then
    export RUST_LOG="$log_level"
    info "RUST_LOG=$RUST_LOG"
fi

step "启动 ${DUCK_APP_NAME}（${profile}）"
info "$bin"
if [ "${#app_args[@]}" -gt 0 ]; then
    info "应用参数：${app_args[*]}"
fi

# 前台运行并接管进程：Ctrl-C 直接退出应用，而不是只杀掉脚本
exec "$bin" ${app_args[@]+"${app_args[@]}"}
