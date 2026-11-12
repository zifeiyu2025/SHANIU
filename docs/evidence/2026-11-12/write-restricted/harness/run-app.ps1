$env:SHANIU_DATA_DIR = 'd:\MyWorkspace\SHANIU\docs\evidence\2026-11-12\write-restricted\data-root'
$env:SHANIU_SANDBOX_OS = 'restricted_token'
$env:SHANIU_CDP_PORT = '9223'
& 'd:\MyWorkspace\SHANIU\src-tauri\target\debug\shaniu.exe' *> 'd:\MyWorkspace\SHANIU\docs\evidence\2026-11-12\write-restricted\app-console.log'
