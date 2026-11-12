@echo off
set SHANIU_DATA_DIR=d:\MyWorkspace\SHANIU\docs\evidence\2026-11-12\write-restricted\data-root
set SHANIU_SANDBOX_OS=restricted_token
set SHANIU_CDP_PORT=9223
"d:\MyWorkspace\SHANIU\src-tauri\target\debug\shaniu.exe" > "d:\MyWorkspace\SHANIU\docs\evidence\2026-11-12\write-restricted\app-stdout.log" 2> "d:\MyWorkspace\SHANIU\docs\evidence\2026-11-12\write-restricted\app-stderr.log"
