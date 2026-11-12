param(
  [string]$DataDir = 'd:\MyWorkspace\SHANIU\docs\evidence\2026-11-12\write-only-default\data-root',
  [string]$Log = 'd:\MyWorkspace\SHANIU\docs\evidence\2026-11-12\write-only-default\app-console.log'
)
# 第二期验收：**不设 SHANIU_SANDBOX_OS**（第二期起不需要环境变量开关）。
Remove-Item Env:\SHANIU_SANDBOX_OS -ErrorAction SilentlyContinue
$env:SHANIU_DATA_DIR = $DataDir
$env:SHANIU_CDP_PORT = '9223'
& 'd:\MyWorkspace\SHANIU\src-tauri\target\debug\shaniu.exe' *> $Log
