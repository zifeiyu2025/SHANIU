# 便携版装配（docs/01 §9.2）：release 产物 → SHANIU-Portable 目录 + ZIP。
#
# 用法：
#   pnpm tauri build --bundles msi        # 先产出 release exe（MSI 与便携版共用同一产物）
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
  throw "[package-portable] 找不到 release 产物：$Exe。先跑 pnpm tauri build --bundles msi"
}
$version = (Get-Content (Join-Path $root "package.json") -Raw | ConvertFrom-Json).version
$stage = Join-Path $root "$OutDir\SHANIU-Portable"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage | Out-Null
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