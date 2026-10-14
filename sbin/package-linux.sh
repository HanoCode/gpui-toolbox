#!/usr/bin/env bash
# 打包 Duck Toolbox 为 Linux 可执行包（tar.gz / .deb，可选分发归档 / 更新归档）。
#
# 用法：
#   sbin/package-linux.sh                          # release 构建 + 组装 dist/Duck-<ver>-linux-x64/
#   sbin/package-linux.sh --tarball                # 额外产出分发 tar.gz
#   sbin/package-linux.sh --deb                   # 额外产出 .deb 安装包
#   sbin/package-linux.sh --update-archive         # 额外产出更新归档
#   sbin/package-linux.sh --target <triple>        # 指定目标架构（如 aarch64-unknown-linux-gnu）
#   sbin/package-linux.sh --no-build               # 跳过编译，直接用现有产物
#   sbin/package-linux.sh --clean                  # 打包前先 cargo clean
#
# 三种分发格式不是一回事：
#   --tarball        给人手动下载的便携目录，里面套一层 Duck-<ver>-linux-<arch>/
#   --deb            Debian/Ubuntu 原生包，可用 dpkg -i 安装，会注册桌面入口和菜单项
#   --update-archive 自更新器认的那一份，duck 必须落在压缩包**根**上——安装器按
#                    「解包后唯一那个 duck」换位
#
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/_common.sh"

usage() {
    sed -n '2,18p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
}

# ---- 参数 -------------------------------------------------------------------

target_triple=""
make_tarball=0
make_deb=0
make_update_archive=0
do_clean=0
do_build=1

while [ $# -gt 0 ]; do
    case "${1}" in
        --target)
            [ $# -ge 2 ] || die "--target 需要一个 Rust target triple"
            target_triple="${2}"
            shift
            ;;
        --tarball) make_tarball=1 ;;
        --deb) make_deb=1 ;;
        --update-archive) make_update_archive=1 ;;
        --no-build) do_build=0 ;;
        --clean) do_clean=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "未知参数：${1}（用 -h 查看用法）"
            ;;
    esac
    shift
done

require_cmd cargo
[ "$(uname -s)" = "Linux" ] || die "打包 Linux 需要在 Linux 环境运行"

version="$(duck_version)"
[ -n "${version}" ] || die "无法从 Cargo.toml 解析版本号"

# ---- 发行合规门禁 -----------------------------------------------------------
# 与 package.sh 共用同一套逻辑

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

step "Duck Toolbox v${version} · 打包 Linux"

# ---- 可选清理 ---------------------------------------------------------------

if [ "${do_clean}" -eq 1 ]; then
    step "cargo clean"
    cargo clean
fi

# ---- 编译 -------------------------------------------------------------------

built_bin=""
deb_arch=""  # deb 包架构，在构建 deb 时设置

# 从 target triple 推断平台键里的架构段。
# 认不出来时输出空串，由调用方决定是报错还是回退。
# 注意：这与 Cargo/rustc 的架构名不同——它是对外的线格式，
# 跟 crates/duck-update/src/platform.rs 的 PlatformKey::key() 绑死。
arch_from_triple() { # $1 = target triple
    case "${1}" in
        x86_64-*-linux*) echo "x64" ;;
        aarch64-*-linux*) echo "arm64" ;;
        loongarch64-*-linux*) echo "loong64" ;;
        *) echo "" ;;
    esac
}

# deb 包架构名（dpkg --print-architecture 格式）
deb_arch_from_triple() { # $1 = target triple
    case "${1}" in
        x86_64-*-linux*) echo "amd64" ;;
        aarch64-*-linux*) echo "arm64" ;;
        loongarch64-*-linux*) echo "loong64" ;;
        *) echo "" ;;
    esac
}

# 推断 deb 包架构（用于无 --target 时）
deb_arch_from_host() {
    case "$(uname -m)" in
        x86_64) echo "amd64" ;;
        aarch64) echo "arm64" ;;
        loongarch64) echo "loong64" ;;
        *)
            echo "$(uname -m)"
            ;;
    esac
}

# 从宿主架构推断 Rust target triple
host_triple() {
    case "$(uname -m)" in
        x86_64) echo "x86_64-unknown-linux-gnu" ;;
        aarch64) echo "aarch64-unknown-linux-gnu" ;;
        loongarch64) echo "loongarch64-unknown-linux-gnu" ;;
        *)
            # 龙芯可能报告 loongarch64 或自定义名，兜底用 uname -m 原样
            echo "$(uname -m)-unknown-linux-gnu"
            ;;
    esac
}

if [ -n "${target_triple}" ]; then
    if [ "${do_build}" -eq 1 ]; then
        step "编译目标：${target_triple}"
        command -v rustup >/dev/null 2>&1 || die "--target 需要 rustup"
        if ! rustup target list --installed 2>/dev/null | grep -qx "${target_triple}"; then
            info "安装 target：${target_triple}"
            rustup target add "${target_triple}"
        fi
        # DUCK_UPDATE_PUBKEY 在编译期内建进二进制（duck-update::signature）。
        # 未设置时内建空值，更新校验关闭，不影响正常运行。
        if [ -n "${DUCK_UPDATE_PUBKEY:-}" ]; then
            export DUCK_UPDATE_PUBKEY
        fi
        cargo build --release -p "${DUCK_CRATE}" --target "${target_triple}"
    fi
    built_bin="${DUCK_ROOT}/target/${target_triple}/release/${DUCK_BIN_NAME}"
    archive_arch="$(arch_from_triple "${target_triple}")"
else
    if [ "${do_build}" -eq 1 ]; then
        step "编译 release 产物"
        cargo build --release -p "${DUCK_CRATE}"
    fi
    built_bin="${DUCK_ROOT}/target/release/${DUCK_BIN_NAME}"
    # 尝试从宿主 triple 推断架构
    host="$(host_triple)"
    archive_arch="$(arch_from_triple "${host}")"
    if [ -z "${archive_arch}" ]; then
        # 回退：用 uname -m 本身（不一定对，但比空字符串好）
        archive_arch="$(uname -m)"
        warn "无法推断更新归档架构段，使用宿主架构：${archive_arch}"
    fi
    info "宿主架构：${host}"
fi

if [ -z "${built_bin}" ]; then
    built_bin="${DUCK_ROOT}/target/release/${DUCK_BIN_NAME}"
fi
[ -f "${built_bin}" ] || die "未找到可执行文件：${built_bin}（去掉 --no-build 重新构建）"

# 确定平台键（对外线格式）
if [ -z "${archive_arch}" ]; then
    platform="linux-unknown"
else
    platform="linux-${archive_arch}"
fi
info "平台标识：${platform}"

# ---- 组装便携目录 -----------------------------------------------------------

stage="${DUCK_DIST_DIR}/Duck-${version}-${platform}"
step "组装 ${stage}"
rm -rf "${stage}"
mkdir -p "${stage}"

# 可执行文件
cp "${built_bin}" "${stage}/${DUCK_BIN_NAME}"
chmod +x "${stage}/${DUCK_BIN_NAME}"

# 许可证
license_src="${DUCK_ROOT}/LICENSE"
if [ -f "${license_src}" ]; then
    cp "${license_src}" "${stage}/LICENSE"
    ok "已复制许可证"
fi

# 应用图标（可选）：把 512x512 的 PNG 放到 assets/AppIcon.png 即会自动打入
icon_src="${DUCK_ROOT}/assets/AppIcon.png"
if [ -f "${icon_src}" ]; then
    cp "${icon_src}" "${stage}/app-icon.png"
    ok "已打入应用图标"
else
    warn "未找到 assets/AppIcon.png，跳过应用图标（不影响运行）"
fi

# .desktop 文件（Linux 桌面集成）
# 让系统知道这是一个桌面应用，可以在菜单中显示
cat >"${stage}/duck-toolbox.desktop" <<DESKTOP
[Desktop Entry]
Name=Duck Toolbox
Comment=Duck Toolbox - Desktop Tools Shell
Exec=${DUCK_BIN_NAME}
Icon=duck-toolbox
Type=Application
Terminal=false
Categories=Utility;Development;
Keywords=toolbox;desktop;tools;
DESKTOP

# AppStream 元数据（可选，用于软件包管理器）
mkdir -p "${stage}/metainfo"
cat >"${stage}/metainfo/duck-toolbox.appdata.xml" <<APPSTREAM
<?xml version="1.0" encoding="UTF-8"?>
<component type="desktop-application">
  <id>com.duck.toolbox</id>
  <name>Duck Toolbox</name>
  <summary>Desktop Tools Shell based on GPUI</summary>
  <metadata_license>MIT</metadata_license>
  <project_license>MIT</project_license>
  <description>
    <p>Duck Toolbox is a desktop tools shell built on GPUI for macOS, Windows and Linux.</p>
  </description>
  <launchable type="desktop-id">duck-toolbox.desktop</launchable>
  <url type="homepage">https://github.com/hanoicode/duck-toolbox</url>
  <content_rating type="oars-1.1"/>
</component>
APPSTREAM

ok "已生成桌面入口和元数据"

# ---- 分发 tar.gz（套一层目录，给人下载用）-----------------------------------

if [ "${make_tarball}" -eq 1 ]; then
    require_cmd tar
    step "生成分发 tar.gz"
    tarball="${DUCK_DIST_DIR}/Duck-${version}-${platform}.tar.gz"
    rm -f "${tarball}"
    # 归档里套一层目录，tar -tzf 可以看到 Duck-${version}-${platform}/
    tar -czf "${tarball}" -C "${DUCK_DIST_DIR}" "Duck-${version}-${platform}"
    info "分发包：${tarball}  （$(human_size "${tarball}")）"
fi

# ---- 更新归档（自更新器认的那一份，duck 落在根）----------------------------

if [ "${make_update_archive}" -eq 1 ]; then
    require_cmd tar
    step "生成更新归档"
    # 更新归档名是 duck-<version>-<platform>.tar.gz（如 duck-0.1.0-linux-x64.tar.gz）
    # 与 PlatformKey::key() 一致
    update_archive="${DUCK_DIST_DIR}/duck-${version}-${platform}.tar.gz"
    rm -f "${update_archive}"
    # 点名单个文件：tar -czf <archive> -C <dir> <name>
    # 这样 duck 可执行文件落在压缩包根上——安装器按「解包后唯一那个可执行文件」换位
    tar -czf "${update_archive}" -C "${stage}" "${DUCK_BIN_NAME}"
    info "更新归档：${update_archive}  （$(human_size "${update_archive}")）"
fi

# ---- .deb 包（Debian/Ubuntu 原生格式）-----------------------------------------

if [ "${make_deb}" -eq 1 ]; then
    require_cmd dpkg-deb
    step "构建 .deb 包"

    # 确定 deb 架构
    if [ -n "${target_triple}" ]; then
        deb_arch="$(deb_arch_from_triple "${target_triple}")"
    else
        deb_arch="$(deb_arch_from_host)"
    fi
    if [ -z "${deb_arch}" ]; then
        warn "无法确定 deb 包架构，使用宿主架构：$(uname -m)"
        deb_arch="$(uname -m)"
    fi

    # deb 包名（Debian 规范：小写、包名与版本之间有冒号但这里不用、架构用 dpkg 格式）
    deb_pkg="duck-toolbox_${version}_${deb_arch}.deb"

    # 临时目录用于构建 deb
    deb_tmp="$(mktemp -d)"
    deb_stage="${deb_tmp}/duck-toolbox-${version}"
    mkdir -p "${deb_stage}/DEBIAN"
    mkdir -p "${deb_stage}/usr/bin"
    mkdir -p "${deb_stage}/usr/share/applications"
    mkdir -p "${deb_stage}/usr/share/doc/duck-toolbox"
    mkdir -p "${deb_stage}/usr/share/metainfo"

    # 复制可执行文件
    cp "${built_bin}" "${deb_stage}/usr/bin/duck-toolbox"
    chmod 755 "${deb_stage}/usr/bin/duck-toolbox"

    # 复制许可证
    if [ -f "${license_src}" ]; then
        cp "${license_src}" "${deb_stage}/usr/share/doc/duck-toolbox/copyright"
        gzip -9 -n "${deb_stage}/usr/share/doc/duck-toolbox/copyright" 2>/dev/null || true
    fi

    # changelog（debian 规范：必须有）
    cat >"${deb_stage}/usr/share/doc/duck-toolbox/changelog.Debian" <<CHANGELOG
duck-toolbox (${version}) stable; urgency=low

  * Initial release

 -- Duck Toolbox <duck@example.com>  $(date -R)
CHANGELOG
    gzip -9 -n "${deb_stage}/usr/share/doc/duck-toolbox/changelog.Debian" 2>/dev/null || true

    # 复制桌面入口
    cp "${stage}/duck-toolbox.desktop" "${deb_stage}/usr/share/applications/"

    # 复制 AppStream 元数据
    if [ -f "${stage}/metainfo/duck-toolbox.appdata.xml" ]; then
        cp "${stage}/metainfo/duck-toolbox.appdata.xml" "${deb_stage}/usr/share/metainfo/"
    fi

    # DEBIAN/control（包控制信息）
    # maintainter、section、priority 按 Debian 规范填写
    cat >"${deb_stage}/DEBIAN/control" <<CONTROL
Package: duck-toolbox
Version: ${version}
Architecture: ${deb_arch}
Maintainer: Duck Toolbox <duck@example.com>
Description: Desktop Tools Shell based on GPUI
 Duck Toolbox is a desktop tools shell built on GPUI for
 macOS, Windows and Linux. It provides a unified interface
 for desktop productivity tools.
CONTROL

    # md5sums（文件校验）
    (
        cd "${deb_stage}"
        find . -type f -exec md5sum {} \; | sed 's|^\(.*\) \(./\)|\1 \2|'
    ) >"${deb_stage}/DEBIAN/md5sums"

    # debian-binary（deb 格式版本）
    echo "2.0" >"${deb_stage}/DEBIAN/debian-binary"

    # 设置所有权（deb 包必须为 root）
    chown -R root:root "${deb_stage}"

    # 构建 .deb
    deb_path="${DUCK_DIST_DIR}/${deb_pkg}"
    dpkg-deb --build --root-owner-group "${deb_stage}" "${deb_path}" >/dev/null 2>&1

    # 清理临时目录
    rm -rf "${deb_tmp}"

    info "deb 包：${deb_path}  （$(human_size "${deb_path}")）"
    ok "deb 包构建完成"
fi

# ---- 汇总 -------------------------------------------------------------------

step "完成"
info "便携目录：${stage}  （$(human_size "${stage}")）"
if [ "${make_tarball}" -eq 1 ]; then
    info "分发 tar.gz：${DUCK_DIST_DIR}/Duck-${version}-${platform}.tar.gz  （$(human_size "${DUCK_DIST_DIR}/Duck-${version}-${platform}.tar.gz")）"
fi
if [ "${make_deb}" -eq 1 ]; then
    info "deb 包：${DUCK_DIST_DIR}/duck-toolbox_${version}_${deb_arch}.deb  （$(human_size "${DUCK_DIST_DIR}/duck-toolbox_${version}_${deb_arch}.deb")）"
    info "安装：sudo dpkg -i duck-toolbox_${version}_${deb_arch}.deb"
fi
if [ "${make_update_archive}" -eq 1 ]; then
    info "更新归档：${DUCK_DIST_DIR}/duck-${version}-${platform}.tar.gz  （$(human_size "${DUCK_DIST_DIR}/duck-${version}-${platform}.tar.gz")）"
fi
info "启动：${stage}/${DUCK_BIN_NAME}"
