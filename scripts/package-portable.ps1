# 便携版装配（docs/01 §9.2）：release 产物 → SHANIU-Portable 目录 + ZIP。
#
# 用法：
#   pnpm tauri build                      # 产出 release exe（tauri.conf.json 里 bundle.active=false，
#                                         #   只出可执行文件，不再打 MSI）
#   pnpm package:portable                 # 生成 dist-portable/SHANIU-Portable 与 ZIP
#
# exe 取自 -Exe > $env:CARGO_TARGET_DIR\release\shaniu.exe > src-tauri\target\release\shaniu.exe；
# 三处都找不到就直接报错，不拿旧产物凑。
#
# -Stage 可选（N7 补）：不传时仍是 `$OutDir\SHANIU-Portable`（历史行为）。要保留上一版的
# 未压缩目录、只新增本版时用 `-Stage dist-release\SHANIU-Portable-0.2.7`，否则 stage 里的
# SHANIU.exe 会被本版覆盖——0.2.7 出包时正是这个原因加的，不改默认值以免影响既有一键出包。
param(
  [string]$Exe = "",
  [string]$OutDir = "dist-portable",
  [string]$Stage = ""
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

# exe 解析顺序（N7）：显式 -Exe > $env:CARGO_TARGET_DIR\release\shaniu.exe > src-tauri\target\release\shaniu.exe。
# 为什么认 CARGO_TARGET_DIR：真机冒烟会用独立 target 目录构建（不污染主构建缓存），
# 旧脚本只认固定路径，会把上一次留下的旧 exe 拿去校验，报错还指向别处。
$exeCandidates = @()
if (-not [string]::IsNullOrWhiteSpace($Exe)) {
  $exeCandidates += $Exe
} else {
  $cargoTarget = $env:CARGO_TARGET_DIR
  if (-not [string]::IsNullOrWhiteSpace($cargoTarget)) {
    if ([System.IO.Path]::IsPathRooted($cargoTarget)) {
      $exeCandidates += (Join-Path $cargoTarget "release\shaniu.exe")
    } else {
      # 相对路径两处都试：用户从仓库根设 `scratch\target-x`，而 cargo 按工作区根
      # （src-tauri）落盘，真机上二者不一致；只认一处就会退回旧 exe（N7 的成因）。
      $exeCandidates += (Join-Path (Join-Path $root $cargoTarget) "release\shaniu.exe")
      $exeCandidates += (Join-Path (Join-Path (Join-Path $root "src-tauri") $cargoTarget) "release\shaniu.exe")
    }
  }
  $exeCandidates += (Join-Path $root "src-tauri\target\release\shaniu.exe")
}
$Exe = $exeCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $Exe) {
  $tried = ($exeCandidates | ForEach-Object { "  - $_" }) -join "`r`n"
  throw "[package-portable] 找不到 release 产物，已尝试：`r`n$tried`r`n先跑 pnpm tauri build；用独立 target 目录构建时请设 CARGO_TARGET_DIR 或传 -Exe <path>。"
}

# 产物模式校验：custom-protocol 构建会把 dist/ 整个内嵌进 exe（资源文件名作为查找键明文可见）。
# 裸 `cargo build --release` 不内嵌前端，启动后连 http://localhost:5173，没有 dev server
# 就是永久白屏（docs/01 已实测）。这里用"dist/index.html 引用的资源文件名是否在 exe 里"
# 判定，不能只看 `localhost:5173`——devUrl 在两种构建里都内嵌在配置中。
$distIndex = Join-Path $root "dist\index.html"
if (-not (Test-Path $distIndex)) {
  throw "[package-portable] 找不到 dist\index.html：先跑 pnpm build（或用 pnpm tauri build，它会先构建前端）"
}
$refs = [regex]::Matches((Get-Content $distIndex -Raw), 'assets/([A-Za-z0-9._-]+)') |
  ForEach-Object { $_.Groups[1].Value } | Select-Object -Unique
if ($refs.Count -eq 0) {
  throw "[package-portable] dist\index.html 未引用任何 assets/*，无法校验产物模式"
}
$blob = [System.Text.Encoding]::GetEncoding(28591).GetString([System.IO.File]::ReadAllBytes($Exe))
$missing = @($refs | Where-Object { -not $blob.Contains($_) })
if ($missing.Count -gt 0) {
  throw "[package-portable] 产物不是 custom-protocol 构建（未内嵌前端资源：$($missing -join ', ')）。请用 pnpm tauri build 产出 exe，不要用裸 cargo build --release。"
}

$version = (Get-Content (Join-Path $root "package.json") -Raw | ConvertFrom-Json).version

# 版本源核对（N7）：exe 的属性版本必须与四处版本源一致，否则说明拿到的不是这次构建的产物。
# 只做一致性判断、不写版本号：版本基线仍由提交显式抬升。
$confVersion = (Get-Content (Join-Path $root "src-tauri\tauri.conf.json") -Raw | ConvertFrom-Json).version
$cargoToml = Get-Content (Join-Path $root "src-tauri\Cargo.toml") -Raw
$cargoVersion = ([regex]::Match($cargoToml, '(?m)^version\s*=\s*"([^"]+)"')).Groups[1].Value
if ($confVersion -ne $version -or $cargoVersion -ne $version) {
  throw "[package-portable] 版本源不一致：package.json=$version / tauri.conf.json=$confVersion / Cargo.toml=$cargoVersion"
}
$exeVersion = (Get-Item $Exe).VersionInfo.FileVersion
if ($exeVersion -ne $version) {
  throw "[package-portable] 产物版本 $exeVersion 与版本源 $version 不一致（多半是旧 exe）：$Exe。先重新 pnpm tauri build，或用 -Exe 指定本次构建的产物。"
}
$stage = if ([string]::IsNullOrWhiteSpace($Stage)) {
  Join-Path $root "$OutDir\SHANIU-Portable"
} elseif ([System.IO.Path]::IsPathRooted($Stage)) {
  $Stage
} else {
  Join-Path $root $Stage
}
# **只删脚本自己产出的文件**。data\ 是便携版的真实运行数据（配置、DPAPI 密钥、
# 会话档案、日志、默认工作区），曾出现过在默认 stage 目录里直接双击试跑留下的 data\，
# 旧版 `Remove-Item $stage -Recurse -Force` 会在下次出包时把它整棵删掉。
if (Test-Path (Join-Path $stage "data")) {
  throw "[package-portable] $stage\data 已存在（便携版运行数据）。为防误删，脚本拒绝覆盖：请先手动移走 data\ 或改用 -OutDir。"
}
if (Test-Path $stage) {
  $own = @("SHANIU.exe", "README.txt", "portable.marker")
  # 先查后删：发现非本脚本内容就整体停止，不能删了一半再报错。
  $unknown = @(Get-ChildItem $stage -Force | Where-Object { $own -notcontains $_.Name })
  if ($unknown.Count -gt 0) {
    throw "[package-portable] $stage 里有非本脚本产出的内容：$($unknown.Name -join ', ')。已停止，未删除任何文件。"
  }
  foreach ($name in $own) {
    $p = Join-Path $stage $name
    if (Test-Path $p) { Remove-Item $p -Force }
  }
}
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Copy-Item $Exe (Join-Path $stage "SHANIU.exe")
New-Item -ItemType File -Path (Join-Path $stage "portable.marker") | Out-Null
$lines = @(
  "SHANIU 便携版 $version",
  "======================",
  "",
  "双击 SHANIU.exe 启动。portable.marker 存在即启用便携模式：",
  "全部数据（配置、密钥、会话、日志、默认工作区）都落在本目录的 data 下，",
  "不写入 %APPDATA% / %LOCALAPPDATA%。移动整个目录即可迁移。",
  "",
  "注意：",
  "1. 密钥使用 Windows DPAPI 加密，绑定当前机器与当前用户；把目录拷到另一台",
  "   机器后旧密钥无法解密，程序会提示重新输入（不会回退成明文）。",
  "2. 建议把本目录放在受 BitLocker 等加密保护的介质上。",
  "3. 需要 Microsoft Edge WebView2 运行时（Windows 11 自带；旧系统请先安装）。"
)
Set-Content -Path (Join-Path $stage "README.txt") -Value ($lines -join "`r`n") -Encoding UTF8
$zip = Join-Path $root "$OutDir\SHANIU-Portable-$version.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
Write-Output "[package-portable] 目录：$stage"
Write-Output "[package-portable] 压缩包：$zip"