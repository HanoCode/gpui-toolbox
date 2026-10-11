# 打包 Duck Toolbox 为 Windows 便携目录（可选分发 zip / 更新归档 zip）。
#
# 用法（在 Windows 上，PowerShell 5.1+）：
#   .\sbin\package.ps1                                  # release 构建 + 组装 dist/Duck-<ver>-<platform>/
#   .\sbin\package.ps1 -Zip                             # 额外产出分发用 dist/Duck-<ver>-<platform>.zip
#   .\sbin\package.ps1 -UpdateArchive                   # 额外产出更新归档 dist/duck-<ver>-<platform>.zip
#   .\sbin\package.ps1 -Target x86_64-pc-windows-msvc   # 指定单一目标架构
#   .\sbin\package.ps1 -NoBuild                         # 跳过编译，直接用现有产物
#   .\sbin\package.ps1 -Clean                           # 打包前先 cargo clean
#
# 两种 zip 不是一回事：
#   -Zip           给人手动下载的便携目录，里面套一层 Duck-<ver>-<platform>/；
#   -UpdateArchive 自更新器认的那一份，duck.exe 必须落在压缩包**根**上——安装器按
#                  「解包后唯一那个 duck.exe」换位（crates/duck-update/src/install/windows.rs）。
# `<platform>` 是对外线格式，与 duck-update 的 PlatformKey::key() 一致（windows-x64）。
#
# 说明：应用图标由 duck-app 的 build.rs 经 winresource 嵌入 .exe，
# 无需在打包目录中额外放置 .ico。

param(
    [string]$Target = "",
    [switch]$Zip,
    [switch]$UpdateArchive,
    [switch]$NoBuild,
    [switch]$Clean
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$DistDir = Join-Path $Root "dist"

function Die([string]$Message) {
    throw $Message
}

# ---- 版本号（根 Cargo.toml 的 [workspace.package].version）--------------------

$VersionLine = Select-String -Path (Join-Path $Root "Cargo.toml") `
    -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $VersionLine) {
    Die "无法从根 Cargo.toml 解析版本号"
}
$Version = $VersionLine.Matches[0].Groups[1].Value

# ---- 可选清理 ------------------------------------------------------------------

if ($Clean) {
    Write-Host "==> cargo clean"
    Push-Location $Root
    cargo clean
    Pop-Location
}

# ---- 编译 ----------------------------------------------------------------------

Push-Location $Root
try {
    if ($NoBuild) {
        Write-Host "==> 跳过编译"
    }
    elseif ($Target) {
        Write-Host "==> cargo build --release -p duck-app --target $Target"
        cargo build --release -p duck-app --target $Target
    }
    else {
        Write-Host "==> cargo build --release -p duck-app"
        cargo build --release -p duck-app
    }
}
finally {
    Pop-Location
}

# ---- 定位产物 ------------------------------------------------------------------

if ($Target) {
    $BinPath = Join-Path $Root "target\$Target\release\duck.exe"
}
else {
    $BinPath = Join-Path $Root "target\release\duck.exe"
}
if (-not (Test-Path $BinPath)) {
    Die "未找到可执行文件：$BinPath（去掉 -NoBuild 重新构建）"
}

if ($Target -match "aarch64") {
    $Platform = "windows-arm64"
}
else {
    $Platform = "windows-x64"
}

# ---- 组装便携目录 ---------------------------------------------------------------

$Stage = Join-Path $DistDir "Duck-$Version-$Platform"
if (Test-Path $Stage) {
    Remove-Item -Recurse -Force $Stage
}
New-Item -ItemType Directory -Path $Stage | Out-Null

Copy-Item $BinPath (Join-Path $Stage "duck.exe")
$License = Join-Path $Root "LICENSE"
if (Test-Path $License) {
    Copy-Item $License $Stage
}

Write-Host "==> 已组装 $Stage"

# ---- 可选：分发 zip（套一层目录，给人下载用）------------------------------------

if ($Zip) {
    $ZipPath = Join-Path $DistDir "Duck-$Version-$Platform.zip"
    if (Test-Path $ZipPath) {
        Remove-Item -Force $ZipPath
    }
    Compress-Archive -Path $Stage -DestinationPath $ZipPath
    Write-Host "==> 已生成分发 zip $ZipPath"
}

# ---- 可选：更新归档（duck.exe 落在根，给自更新器用）----------------------------

if ($UpdateArchive) {
    $UpdateZip = Join-Path $DistDir "duck-$Version-$Platform.zip"
    if (Test-Path $UpdateZip) {
        Remove-Item -Force $UpdateZip
    }
    # 点名单个文件而不是目录：`Compress-Archive -Path <目录>` 会把目录本身包进去，
    # 那样 duck.exe 就落在包内一层子目录下，安装器在根上找不到它。
    $ExePath = Join-Path $Stage "duck.exe"
    Compress-Archive -Path $ExePath -DestinationPath $UpdateZip
    Write-Host "==> 已生成更新归档 $UpdateZip"
}

Write-Host "==> 完成（v$Version · $Platform）"
