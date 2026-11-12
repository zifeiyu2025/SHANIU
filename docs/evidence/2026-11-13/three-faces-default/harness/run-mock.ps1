# mock 模型进程必须自己拿到 WORKSPACE/OUTSIDE：Win32_Process.Create 不继承调用方 shell 的环境变量。
$env:WORKSPACE = 'D:\MyWorkspace\SHANIU\docs\evidence\2026-11-13\three-faces-default\workspace'
$env:OUTSIDE = $env:USERPROFILE
node 'd:\MyWorkspace\SHANIU\docs\evidence\2026-11-13\three-faces-default\harness\mock-model.mjs' *> 'd:\MyWorkspace\SHANIU\docs\evidence\2026-11-13\three-faces-default\mock-model.log'
