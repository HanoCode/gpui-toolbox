#!/usr/bin/env bash
# 打包 Duck Toolbox 为 RPM 包（Fedora / RHEL / CentOS 等）。
# 必须跑在 Linux 环境中（macOS 上无法构建 RPM）。
#
# 用法：
#   sbin/package-rpm.sh                         # release 构建 + 组装 .rpm
#   sbin/package-rpm.sh --target <triple>       # 指定目标架构（如 aarch64-unknown-linux-gnu）
#   sbin/package-rpm.sh --no-build              # 跳过编译，直接用现有产物
#   sbin/package-rpm.sh --clean                 # 打包前先 cargo clean
#
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/_common.sh"

usage() {
    sed -n '2,11p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
}

# ---- 参数 -------------------------------------------------------------------

target_triple=""
do_clean=0
do_build=1

while [ $# -gt 0 ]; do
    case "${1}" in
        --target)
            [ $# -ge 2 ] || die "--target 需要一个 Rust target triple"
            target_triple="${2}"
            shift
            ;;
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

require_cmd rpmbuild
[ "$(uname -s)" = "Linux" ] || die "打包 RPM 需要在 Linux 环境运行"

version="$(duck_version)"
[ -n "${version}" ] || die "无法从 Cargo.toml 解析版本号"

# ---- 发行合规门禁 -----------------------------------------------------------

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

step "Duck Toolbox v${version} · 打包 RPM"

# ---- 可选清理 ---------------------------------------------------------------

if [ "${do_clean}" -eq 1 ]; then
    step "cargo clean"
    cargo clean
fi

# ---- 编译 -------------------------------------------------------------------

built_bin=""

# 从宿主架构推断 Rust target triple
host_triple() {
    case "$(uname -m)" in
        x86_64) echo "x86_64-unknown-linux-gnu" ;;
        aarch64) echo "aarch64-unknown-linux-gnu" ;;
        loongarch64) echo "loongarch64-unknown-linux-gnu" ;;
        armv7hl) echo "armv7hl-unknown-linux-gnueabihf" ;;
        i686) echo "i686-unknown-linux-gnu" ;;
        *) echo "$(uname -m)-unknown-linux-gnu" ;;
    esac
}

if [ -n "${target_triple}" ]; then
    step "编译目标：${target_triple}"
    command -v rustup >/dev/null 2>&1 || die "--target 需要 rustup"
    if ! rustup target list --installed 2>/dev/null | grep -qx "${target_triple}"; then
        info "安装 target：${target_triple}"
        rustup target add "${target_triple}"
    fi
    cargo build --release -p "${DUCK_CRATE}" --target "${target_triple}"
    built_bin="${DUCK_ROOT}/target/${target_triple}/release/${DUCK_BIN_NAME}"
else
    if [ "${do_build}" -eq 1 ]; then
        step "编译 release 产物"
        cargo build --release -p "${DUCK_CRATE}"
    fi
    built_bin="${DUCK_ROOT}/target/release/${DUCK_BIN_NAME}"
    host="$(host_triple)"
    info "宿主架构：${host}"
fi

[ -f "${built_bin}" ] || die "未找到可执行文件：${built_bin}（去掉 --no-build 重新构建）"

# ---- 确定平台标识 ------------------------------------------------------------

# 架构映射：target triple → PlatformKey 对外格式
arch_from_triple() {
    case "${1}" in
        x86_64-*-linux*) echo "x64" ;;
        aarch64-*-linux*) echo "arm64" ;;
        loongarch64-*-linux*) echo "loong64" ;;
        *) echo "" ;;
    esac
}

# RPM 架构名（rpmbuild --showrc 格式）
rpm_arch_from_triple() {
    case "${1}" in
        x86_64-*-linux*) echo "x86_64" ;;
        aarch64-*-linux*) echo "aarch64" ;;
        loongarch64-*-linux*) echo "loongarch64" ;;
        armv7hl-*-linux*) echo "armv7hl" ;;
        i686-*-linux*) echo "i686" ;;
        *) echo "" ;;
    esac
}

# 取 host triple 来确定架构
host="$(host_triple)"
platform_arch="$(arch_from_triple "${host}")"
[ -z "${platform_arch}" ] && platform_arch="$(uname -m)"
rpm_arch="$(rpm_arch_from_triple "${host}")"
[ -z "${rpm_arch}" ] && rpm_arch="$(uname -m)"

platform="linux-${platform_arch}"
info "平台标识：${platform}  RPM 架构：${rpm_arch}"

# ---- 组装构建环境 -----------------------------------------------------------
# RPM 构建规范要求在 rpmbuild/ 目录下工作。
# 我们把产物 bin 打包进 SOURCES/<name>-<ver>.tar.gz，
# %install 再从那里 COPY 到 %{buildroot}（这样 mock 环境里也能找到文件）。

rpm_topdir="${DUCK_DIST_DIR}/rpmbuild"
rm -rf "${rpm_topdir}"
mkdir -p "${rpm_topdir}"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}

# 把可执行文件和桌面入口打进源码 tar.gz（%install 从这里取文件）
# 打包时套一层目录以便 %install 知道去哪里找
src_dir="duck-toolbox-${version}"
src_tar="${rpm_topdir}/SOURCES/${src_dir}.tar.gz"

step "组装源码包"
mkdir -p "${DUCK_DIST_DIR}/${src_dir}"

# 可执行文件
cp "${built_bin}" "${DUCK_DIST_DIR}/${src_dir}/${DUCK_BIN_NAME}"
chmod 755 "${DUCK_DIST_DIR}/${src_dir}/${DUCK_BIN_NAME}"

# 许可证
if [ -f "${DUCK_ROOT}/LICENSE" ]; then
    cp "${DUCK_ROOT}/LICENSE" "${DUCK_DIST_DIR}/${src_dir}/"
fi

# 桌面入口
cat >"${DUCK_DIST_DIR}/${src_dir}/duck-toolbox.desktop" <<DESKTOP
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

# AppStream 元数据
mkdir -p "${DUCK_DIST_DIR}/${src_dir}/metainfo"
cat >"${DUCK_DIST_DIR}/${src_dir}/metainfo/duck-toolbox.appdata.xml" <<APPSTREAM
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

# 打包
tar -czf "${src_tar}" -C "${DUCK_DIST_DIR}" "${src_dir}"
rm -rf "${DUCK_DIST_DIR}/${src_dir}"
info "源码包：${src_tar}  （$(human_size "${src_tar}")）"

# ---- 编写 spec 文件 ---------------------------------------------------------
#
# %install 脚本运行在 rpmbuild 的 BUILDROOT 上下文中：
#   %{_builddir}  → BUILD/<name>-<ver>/
#   %{buildroot}  → 临时安装根（mock / chroot 环境）
# %install 的职责是把 SOURCES 里打包的东西 COPY 到 %{buildroot}。
# 重点：%install 脚本本身跑在宿主机上，只是把文件路径重排列后写入 BUILDROOT，
#       后续 rpmbuild 会按 %{buildroot}/%{_bindir}/ 整理成最终 RPM 的目录结构。

spec_file="${rpm_topdir}/SPECS/duck-toolbox.spec"

cat >"${spec_file}" <<SPEC
%global app_name     duck-toolbox
%global bin_name     duck
%global version      ${version}
%global release      1
%global license      MIT
%global summary     Desktop Tools Shell based on GPUI
%global description Duck Toolbox is a desktop tools shell built on GPUI for macOS, Windows and Linux.

Name:           ${app_name}
Version:        %{version}
Release:        %{release}%{?dist}
Summary:        %{summary}
License:        %{license}
URL:            https://github.com/hanoicode/duck-toolbox
Source0:        %{name}-%{version}.tar.gz
BuildRequires:  gcc

%description
%{description}

%package        ${bin_name}
Summary:        %{summary}
Provides:       %{name} = %{version}-%{release}
%description    ${bin_name}
%{description}

%install
# 从 BUILD/<name>-<ver>/（即 %{_builddir}）复制到 %{buildroot}
# BUILD 目录是 tar.gz 解压后的根，也就是 SOURCES 里的 src_dir
mkdir -p "%{buildroot}%{_bindir}"
mkdir -p "%{buildroot}%{_datadir}/applications"
mkdir -p "%{buildroot}%{_datadir}/metainfo"
mkdir -p "%{buildroot}%{_datadir}/doc/%{name}"

# 可执行文件
install -m 755 %{_builddir}/${src_dir}/%{bin_name} "%{buildroot}%{_bindir}/%{bin_name}"

# 桌面入口
install -m 644 %{_builddir}/${src_dir}/duck-toolbox.desktop \
    "%{buildroot}%{_datadir}/applications/"

# AppStream 元数据
install -m 644 %{_builddir}/${src_dir}/metainfo/duck-toolbox.appdata.xml \
    "%{buildroot}%{_datadir}/metainfo/" 2>/dev/null || true

# 许可证
install -m 644 %{_builddir}/${src_dir}/LICENSE \
    "%{buildroot}%{_datadir}/doc/%{name}/" 2>/dev/null || true

%files ${bin_name}
%{_bindir}/%{bin_name}
%{_datadir}/applications/duck-toolbox.desktop
%{_datadir}/metainfo/duck-toolbox.appdata.xml
%{_datadir}/doc/%{name}/LICENSE

%changelog
* $(date '+%a %b %d %Y') Duck Toolbox <duck@example.com> - %{version}-%{release}
- Initial release
SPEC

info "spec 文件：${spec_file}"

# ---- 构建 RPM ---------------------------------------------------------------

step "构建 RPM（rpmbuild -ba）"
rpmbuild --define "_topdir ${rpm_topdir}" \
    -ba "${spec_file}" 2>&1 | tail -20

# ---- 收集产物 ---------------------------------------------------------------

# 找 RPM 产物（按 rpm_arch 过滤）
rpm_out_dir="${rpm_topdir}/RPMS/${rpm_arch}"
rpm_file=""
if [ -d "${rpm_out_dir}" ]; then
    rpm_file="$(ls "${rpm_out_dir}"/duck-toolbox-*.rpm 2>/dev/null | head -1)"
fi

if [ -n "${rpm_file}" ] && [ -f "${rpm_file}" ]; then
    final_rpm="${DUCK_DIST_DIR}/$(basename "${rpm_file}")"
    cp "${rpm_file}" "${final_rpm}"
    info "RPM 包：${final_rpm}  （$(human_size "${final_rpm}")）"
    ok "RPM 包构建完成"
else
    ls -la "${rpm_topdir}/RPMS/" 2>/dev/null | sed 's/^/    /' || true
    die "RPM 构建失败：未找到产物（检查上面的 rpmbuild 输出）"
fi

# 源码包
srpm_file="$(ls "${rpm_topdir}/SRPMS/duck-toolbox-"*.rpm 2>/dev/null | head -1)"
if [ -n "${srpm_file}" ] && [ -f "${srpm_file}" ]; then
    cp "${srpm_file}" "${DUCK_DIST_DIR}/"
    info "SRPM：${DUCK_DIST_DIR}/$(basename "${srpm_file}")"
fi

# 清理构建目录
rm -rf "${rpm_topdir}"

step "完成"
info "RPM 包：${final_rpm}"
info "安装：sudo rpm -i duck-toolbox-${version}-*.rpm"
info "卸载：sudo rpm -e duck-toolbox"
