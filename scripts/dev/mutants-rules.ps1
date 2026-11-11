<#
.SYNOPSIS
  对 src-tauri 的指定文件跑 cargo-mutants（默认 agent_loop/rules.rs）。

.DESCRIPTION
  这个跑法里的每一处都不是可选项，三条都是踩出来的：

  1) `--in-place` 必须开。rules 的用例要读**仓库根**的 AGENTS.md；
     tests/event_schema.rs 在编译期 include_str! 包外的 ../src/transport/types.ts。
     cargo-mutants 默认把 crate 复制到临时目录再变异，包外文件不跟着走，基线直接失败
     （表现为 4 个 rules 用例 panic「读仓库 AGENTS.md: NotFound」）。

  2) `-C=--lib` 必须传。不加的话每个变异体都要重链 40+ 个集成测试，
     单变异体从 ~21s 涨到不可接受；变异测试只关心 lib 单测。

  3) shard 是 **0 起始**的：`--shard 4/4` 会直接报
     "shard k must be less than n"，最后一片是 `3/4`。

  输出一律写 %TEMP%（mutants.out 体积大，落仓库会污染 git status）。
  本机实测：单变异体约 21s，rules.rs 的 142 个分 4 片约 50 分钟。

  注意：`--in-place` 期间源文件会被临时改写、每个变异体后还原。
  跑完脚本会兜底核对 git status；若还有改动，提交前先看 git diff。

.PARAMETER File
  相对 src-tauri 的文件路径。

.PARAMETER Shards
  分片总数（串行跑完所有片）。

.PARAMETER Shard
  只跑某一片（0 起始）。给 -1 表示按 Shards 跑全部。

.PARAMETER Filter
  只跑名字匹配该正则的变异体（cargo-mutants 的 -F），常用来复跑某个函数。

.PARAMETER TestFilter
  传给 cargo test 的用例过滤（默认只跑 rules 模块单测）。

.EXAMPLE
  pwsh scripts/dev/mutants-rules.ps1

.EXAMPLE
  pwsh scripts/dev/mutants-rules.ps1 -Shard 0

.EXAMPLE
  pwsh scripts/dev/mutants-rules.ps1 -Shards 1 -Filter 'selection|dropped_note|render_note'
#>
param(
    [string]$File = "src/agent_loop/rules.rs",
    [int]$Shards = 4,
    [int]$Shard = -1,
    [string]$Filter = "",
    [string]$TestFilter = "agent_loop::rules::"
)

$ErrorActionPreference = "Stop"
$crate = (Resolve-Path (Join-Path $PSScriptRoot "..\..\src-tauri")).Path
Push-Location $crate
try {
    $shardList = if ($Shard -ge 0) { @($Shard) } else { 0..($Shards - 1) }
    foreach ($s in $shardList) {
        # cargo-mutants 不会递归创建 -o 的父目录，%TEMP%\shaniu-mutants 不存在就直接报 os error 3。
        $outDir = Join-Path $env:TEMP "shaniu-mutants\shard$s"
        New-Item -ItemType Directory -Force -Path $outDir | Out-Null
        $cli = @(
            "mutants",
            "--file", $File,
            "--in-place",
            "--shard", "$s/$Shards",
            "-C=--lib",
            "--cargo-test-arg=$TestFilter",
            "--timeout", "120",
            "-o", $outDir
        )
        if ($Filter) { $cli += @("-F", $Filter) }
        Write-Host "==> cargo $($cli -join ' ')"
        & cargo @cli
        Write-Host "shard $s 退出码 $LASTEXITCODE（2 = 有存活变异体，属正常结果；4 = 基线失败）"
    }

    # --in-place 结束后源文件应回到原样；没回去就别提交。
    $dirty = & git status --porcelain -- $File
    if ($dirty) {
        Write-Warning "变异测试后 $File 仍有改动，提交前先 git diff -- $File`n$dirty"
    }
}
finally {
    Pop-Location
}
