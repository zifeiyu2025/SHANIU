param(
  [string]$DataDir = 'd:\MyWorkspace\SHANIU\docs\evidence\2026-11-13\three-faces-default\data-root',
  [string]$Log = 'd:\MyWorkspace\SHANIU\docs\evidence\2026-11-13\three-faces-default\app-console.log'
)
# 第三批验收：**不设 SHANIU_SANDBOX_OS**，默认档 = 写约束受限令牌（exec/终端/MCP 三面一致）。
Remove-Item Env:\SHANIU_SANDBOX_OS -ErrorAction SilentlyContinue
$env:SHANIU_DATA_DIR = $DataDir
$env:SHANIU_CDP_PORT = '9224'
& 'd:\MyWorkspace\SHANIU\src-tauri\target\debug\shaniu.exe' *> $Log
