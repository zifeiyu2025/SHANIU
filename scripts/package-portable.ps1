# 便携版装配（docs/01 §9.2）：release 产物 → SHANIU-Portable 目录 + ZIP。
#
# 用法：
#   pnpm tauri build                      # 产出 release exe（tauri.conf.json 里 bundle.active=false，
#                                         #   只出可执行文件，不再打 MSI）
#   pnpm package:portable                 # 生成 dist-portable/SHANIU-Portable 与 ZIP
param(
  [string]$Exe = "",
  [string]$OutDir = "dist-portable"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Exe)) {
  $Exe = Join-Path $root "src-tauri\target\release\shaniu.exe"
}
if (-not (Test-Path $Exe)) {
  throw "[package-portable] 找不到 release 产物：$Exe。先跑 pnpm tauri build"
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
$stage = Join-Path $root "$OutDir\SHANIU-Portable"
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