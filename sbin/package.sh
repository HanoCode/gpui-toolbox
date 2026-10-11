#!/usr/bin/env bash
# 打包 Duck Toolbox 为 macOS 应用包（.app，可选 .dmg / .zip / -app.tar.gz）。
#
# 用法：
#   sbin/package.sh                      # release 构建 + 组装 dist/Duck.app
#   sbin/package.sh --dmg                # 额外产出 dist/Duck-<ver>.dmg
#   sbin/package.sh --zip                # 额外产出 dist/Duck-<ver>.zip
#   sbin/package.sh --app-archive        # 额外产出更新归档 dist/duck-<ver>-macos-<arch>-app.tar.gz
#   sbin/package.sh --universal          # 通用二进制（arm64 + x86_64）
#   sbin/package.sh --target <triple>    # 指定单一目标架构
#   sbin/package.sh --sign <identity>    # 用指定证书签名（默认 ad-hoc）
#   sbin/package.sh --notarize           # 上传 Apple 公证并装订票据（需 Developer ID 签名与凭据）
#   sbin/package.sh --no-sign            # 跳过签名
#   sbin/package.sh --clean              # 打包前先 cargo clean
#
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/_common.sh"

usage() {
    sed -n '2,14p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
}

# ---- 参数 -------------------------------------------------------------------

target_triple=""
universal=0
make_dmg=0
make_zip=0
make_app_archive=0
do_sign=1
sign_identity="-" # ad-hoc
do_notarize=0
do_clean=0
do_build=1

while [ $# -gt 0 ]; do
    case "$1" in
        --universal) universal=1 ;;
        --target)
            [ $# -ge 2 ] || die "--target 需要一个 Rust target triple"
            target_triple="$2"
            shift
            ;;
        --dmg) make_dmg=1 ;;
        --zip) make_zip=1 ;;
        --app-archive) make_app_archive=1 ;;
        --sign)
            [ $# -ge 2 ] || die "--sign 需要一个证书标识"
            sign_identity="$2"
            shift
            ;;
        --no-sign) do_sign=0 ;;
        --notarize) do_notarize=1 ;;
        --no-build) do_build=0 ;;
        --clean) do_clean=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "未知参数：${1}（用 -h 查看用法）" ;;
    esac
    shift
done

if [ "$universal" -eq 1 ] && [ -n "$target_triple" ]; then
    die "--universal 与 --target 不能同时使用"
fi

require_cmd cargo
[ "$(uname -s)" = "Darwin" ] || die "打包 .app 需要 macOS 环境"

version="$(duck_version)"
[ -n "$version" ] || die "无法从 Cargo.toml 解析版本号"

# ---- 发行合规门禁 -----------------------------------------------------------
# `assets/licenses/THIRD_PARTY_LICENSES.md` 既随产物分发，也被内嵌进应用
# （设置 → 关于 → 查看开源许可）。依赖一变它就陈旧，所以在打包前先拦一道。
# 退出码三态：0 一致 / 1 已过期（拦下）/ 2 本机查不了（只警告，不挡打包）。
if command -v python3 >/dev/null 2>&1; then
    if python3 "${DUCK_ROOT}/sbin/licenses.py" --check; then
        :
    else
        status=$?
        if [ "${status}" -eq 1 ]; then
            die "第三方许可汇总已过期：跑 python3 sbin/licenses.py 并提交结果"
        fi
        warn "跳过第三方许可校验（本机环境不支持，退出码 ${status}）"
    fi
else
    warn "跳过第三方许可校验（本机没有 python3）"
fi

step "$DUCK_APP_NAME v$version · 打包 macOS 应用"

# ---- 可选清理 ---------------------------------------------------------------

if [ "$do_clean" -eq 1 ]; then
    step "cargo clean"
    cargo clean
fi

# ---- 编译 -------------------------------------------------------------------

built_bin=""

build_for_host() {
    local triple
    triple="$(duck_host_triple)"
    if [ -n "$triple" ]; then
        info "宿主架构：$triple"
        cargo build --release -p "$DUCK_CRATE" --target "$triple"
        built_bin="$DUCK_ROOT/target/$triple/release/$DUCK_BIN_NAME"
    else
        info "未知架构，使用 cargo 默认目标"
        cargo build --release -p "$DUCK_CRATE"
        built_bin="$DUCK_ROOT/target/release/$DUCK_BIN_NAME"
    fi
}

build_for_triple() { # $1 = target triple
    local triple="$1"
    command -v rustup >/dev/null 2>&1 || die "--target/--universal 需要 rustup"
    if ! rustup target list --installed 2>/dev/null | grep -qx "$triple"; then
        info "安装 target：$triple"
        rustup target add "$triple"
    fi
    cargo build --release -p "$DUCK_CRATE" --target "$triple"
    built_bin="$DUCK_ROOT/target/$triple/release/$DUCK_BIN_NAME"
}

arm_bin=""
x86_bin=""

if [ "$universal" -eq 1 ]; then
    step "编译通用二进制（arm64 + x86_64）"
    build_for_triple "aarch64-apple-darwin"
    arm_bin="$built_bin"
    build_for_triple "x86_64-apple-darwin"
    x86_bin="$built_bin"
elif [ -n "$target_triple" ]; then
    step "编译目标：$target_triple"
    build_for_triple "$target_triple"
else
    if [ "$do_build" -eq 1 ]; then
        step "编译 release 产物"
        build_for_host
    else
        built_bin="$DUCK_ROOT/target/release/$DUCK_BIN_NAME"
    fi
fi

if [ "$universal" -eq 0 ]; then
    if [ -z "$built_bin" ]; then
        built_bin="$DUCK_ROOT/target/release/$DUCK_BIN_NAME"
    fi
    [ -f "$built_bin" ] || die "未找到可执行文件：${built_bin}（去掉 --no-build 重新构建）"
fi

# ---- 组装 .app --------------------------------------------------------------

app_dir="$DUCK_DIST_DIR/$DUCK_APP_NAME.app"
contents="$app_dir/Contents"

step "组装 $DUCK_APP_NAME.app"
rm -rf "$app_dir"
mkdir -p "$contents/MacOS" "$contents/Resources"

if [ "$universal" -eq 1 ]; then
    require_cmd lipo
    lipo -create -output "$contents/MacOS/$DUCK_BIN_NAME" "$arm_bin" "$x86_bin"
    info "已合并架构：$(lipo -archs "$contents/MacOS/$DUCK_BIN_NAME")"
else
    cp "$built_bin" "$contents/MacOS/$DUCK_BIN_NAME"
    if command -v lipo >/dev/null 2>&1; then
        info "架构：$(lipo -archs "$contents/MacOS/$DUCK_BIN_NAME" 2>/dev/null || echo unknown)"
    fi
fi
chmod +x "$contents/MacOS/$DUCK_BIN_NAME"

# 应用图标（可选）：把 1024x1024 的 .icns 放到 assets/AppIcon.icns 即会自动打入
icon_line=""
icon_src="$DUCK_ROOT/assets/AppIcon.icns"
if [ -f "$icon_src" ]; then
    cp "$icon_src" "$contents/Resources/AppIcon.icns"
    icon_line="    <key>CFBundleIconFile</key>
    <string>AppIcon</string>"
    ok "已打入应用图标"
else
    warn "未找到 assets/AppIcon.icns，跳过应用图标（不影响运行）"
fi

# 深链接与文件关联：让系统知道 `duck://` 归我们、`.ducktool` 用什么打开。
#
# 少这一段的话，`open duck://tool/http.replay` 会报「没有应用程序可以打开该 URL」，
# 双击插件包也不会走到我们的解析（`crates/duck-app/src/deeplink.rs`）。
#
# 两个字符串各有一份 Rust 侧的同源常量，改这里就得跟着改：
#   - `duck`      → `duck_platform::URL_SCHEME`
#   - `ducktool`  → `duck_app::deeplink` 的 `PLUGIN_PACKAGE_EXTENSION`
# 契约测试 `crates/duck-platform/tests/packaging_matches_the_code.rs` 会盯着这两处。
#
# `CFBundleTypeRole` 取 `Viewer`：我们只把插件包转手交给插件中心，不声称能编辑它。
cat >"$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>zh_CN</string>
    <key>CFBundleExecutable</key>
    <string>$DUCK_BIN_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$DUCK_BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$DUCK_APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$DUCK_APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
$icon_line
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>
            <string>$DUCK_BUNDLE_ID</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>duck</string>
            </array>
        </dict>
    </array>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>
            <string>Duck Toolbox Plugin Package</string>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>CFBundleTypeExtensions</key>
            <array>
                <string>ducktool</string>
            </array>
        </dict>
    </array>
    <key>LSMinimumSystemVersion</key>
    <string>$DUCK_MIN_MACOS</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
PLIST

printf 'APPL????' >"$contents/PkgInfo"

# ---- 签名 -------------------------------------------------------------------

if [ "$do_sign" -eq 1 ]; then
    require_cmd codesign
    if [ "$sign_identity" = "-" ]; then
        step "ad-hoc 签名（本地运行足够，分发给他人需正式证书）"
        codesign --force --deep --sign - "$app_dir"
    else
        step "使用证书签名：$sign_identity"
        codesign --force --deep --options runtime --timestamp \
            --sign "$sign_identity" "$app_dir"
    fi
    codesign --verify --verbose=2 "$app_dir" 2>&1 | sed 's/^/    /' || warn "签名校验未完全通过"
else
    warn "已跳过签名：Gatekeeper 可能拦截首次启动（右键 → 打开 可绕过）"
fi

# ---- 公证（可选）--------------------------------------------------------------
#
# 这一步解决的是「**从网上下载下来的**那份能不能打开」，与签名不是一回事：签名证明包没被
# 篡改过，公证是 Apple 出具的票据。缺票据时，从浏览器 / GitHub 拿到的副本带 quarantine
# 属性，Gatekeeper 会以「无法验证开发者」拦下首次启动（右键 → 打开 能绕，但那不该是分发
# 给用户时的正常路径）。票据装订进包内之后，断网也验得过。
#
# 两个前置条件，缺一不可：
#   1. 必须用 **Developer ID** 证书签名 —— ad-hoc 签的包 Apple 不收（下面直接拦下）；
#   2. 凭据二选一（都从环境变量取，不写进仓库）：
#        MACOS_NOTARY_PROFILE                           keychain 里存好的 profile
#        MACOS_NOTARY_APPLE_ID / MACOS_NOTARY_TEAM_ID / Apple ID 三件套，密码是
#        MACOS_NOTARY_PASSWORD                          **App 专用密码**而非账号密码
#
# 顺序：先公证、再把票据装订回 .app，**之后**才产出对外分发的 zip / dmg / 更新归档 ——
# 反过来做，发出去的那份里就没有票据。装订对象是 .app 本体；dmg 作为外层容器另有自己的
# 票据，需要时对它再跑一次 `notarytool submit` + `stapler staple`。
#
# 想接进 CI：在 release.yml 那条 macOS 腿上传 `--notarize` 并把上面三个变量经 `env:`
# 透传（与 `MACOS_SIGN_IDENTITY` 同一套写法即可，都不配时本段完全不执行）。

if [ "$do_notarize" -eq 1 ]; then
    require_cmd xcrun
    if [ "$do_sign" -eq 0 ]; then
        die "--notarize 与 --no-sign 互斥：公证的前提是先有一份 Apple 认可的签名"
    fi
    if [ "$sign_identity" = "-" ]; then
        die "--notarize 需要 Developer ID 证书：请显式传 --sign \"Developer ID Application: <名称> (<团队 ID>)\""
    fi

    notary_submit=()
    if [ -n "${MACOS_NOTARY_PROFILE:-}" ]; then
        notary_submit=(--keychain-profile "${MACOS_NOTARY_PROFILE}")
    elif [ -n "${MACOS_NOTARY_APPLE_ID:-}" ] && [ -n "${MACOS_NOTARY_TEAM_ID:-}" ] && [ -n "${MACOS_NOTARY_PASSWORD:-}" ]; then
        notary_submit=(
            --apple-id "${MACOS_NOTARY_APPLE_ID}"
            --team-id "${MACOS_NOTARY_TEAM_ID}"
            --password "${MACOS_NOTARY_PASSWORD}"
        )
    else
        die "--notarize 缺凭据：设 MACOS_NOTARY_PROFILE，或同时设 MACOS_NOTARY_APPLE_ID / MACOS_NOTARY_TEAM_ID / MACOS_NOTARY_PASSWORD"
    fi

    require_cmd ditto
    # 公证只收 zip / dmg / pkg，不收裸 .app：先打一个只用于上传的临时 zip，传完即删。
    notary_zip="$DUCK_DIST_DIR/.notarize-upload.zip"
    rm -f "$notary_zip"
    step "上传公证（排队 + 出票，通常几分钟）"
    ditto -c -k --sequesterRsrc --keepParent "$app_dir" "$notary_zip"
    xcrun notarytool submit "$notary_zip" "${notary_submit[@]}" --wait
    rm -f "$notary_zip"

    step "装订票据（stapler）"
    xcrun stapler staple "$app_dir"
    xcrun stapler validate "$app_dir"
fi

# ---- 分发产物 ---------------------------------------------------------------

if [ "$make_zip" -eq 1 ]; then
    require_cmd ditto
    step "生成 zip"
    ditto -c -k --sequesterRsrc --keepParent \
        "$app_dir" "$DUCK_DIST_DIR/$DUCK_APP_NAME-$version.zip"
fi

if [ "$make_dmg" -eq 1 ]; then
    require_cmd hdiutil
    step "生成 dmg"
    rm -f "$DUCK_DIST_DIR/$DUCK_APP_NAME-$version.dmg"
    hdiutil create -volname "$DUCK_APP_NAME" -srcfolder "$app_dir" \
        -ov -format UDZO "$DUCK_DIST_DIR/$DUCK_APP_NAME-$version.dmg" >/dev/null
fi

# ---- 更新归档（自更新器认的那一份）------------------------------------------

if [ "$make_app_archive" -eq 1 ]; then
    # 归档名里的架构段是对外**线格式**——manifest 的 `macos-arm64` / `macos-x64` 键
    # 由它拼出。取不到就报错，不猜：猜错会让这个包永远没人更新得到。
    if [ "$universal" -eq 1 ]; then
        archive_arch="universal"
    elif [ -n "$target_triple" ]; then
        archive_arch="$(duck_arch_from_triple "$target_triple")"
        if [ -z "$archive_arch" ]; then
            die "无法从 ${target_triple} 推断更新归档的架构段"
        fi
    else
        archive_arch="$(duck_arch_from_triple "$(duck_host_triple)")"
        if [ -z "$archive_arch" ]; then
            die "无法从宿主架构推断更新归档的架构段"
        fi
    fi

    require_cmd tar
    step "生成更新归档（-app.tar.gz）"
    app_archive="$DUCK_DIST_DIR/duck-$version-macos-$archive_arch-app.tar.gz"
    rm -f "$app_archive"
    # `-C dist` 且只点名 Duck.app：归档里必须是**恰好一个** .app 落在根——安装器
    # 按「解包后唯一那个 .app」换位（见 duck-update/src/install/macos.rs）。
    # 多包一层目录会让它找不到；塞进去两个 .app 会被它拒绝。
    tar -czf "$app_archive" -C "$DUCK_DIST_DIR" "$DUCK_APP_NAME.app"
    info "更新归档：${app_archive}  （$(human_size "$app_archive")）"
fi

# ---- 汇总 -------------------------------------------------------------------

step "完成"
info "应用包：$app_dir  （$(human_size "$app_dir")）"
if [ "$make_zip" -eq 1 ]; then
    info "zip：$DUCK_DIST_DIR/$DUCK_APP_NAME-$version.zip  （$(human_size "$DUCK_DIST_DIR/$DUCK_APP_NAME-$version.zip")）"
fi
if [ "$make_dmg" -eq 1 ]; then
    info "dmg：$DUCK_DIST_DIR/$DUCK_APP_NAME-$version.dmg  （$(human_size "$DUCK_DIST_DIR/$DUCK_APP_NAME-$version.dmg")）"
fi
if [ "$make_app_archive" -eq 1 ]; then
    info "更新归档：$app_archive  （$(human_size "$app_archive")）"
fi
info "启动：open \"$app_dir\""
