# 仓库评测判据（在评测工作区之外执行，agent 改不到本文件）
#
# 约定：成功 = 退出码 0 且 stdout 含 "REPO-CHECK: PASS"；
#       失败 = 退出码 1 且 stdout 含 "REPO-CHECK: FAIL <原因>"。
# 每题都先跑守卫（guards.json）：测试区/判据文件被改过就直接失败，不进入业务判据。

param(
    [Parameter(Mandatory = $true)][string]$Task,
    [Parameter(Mandatory = $true)][string]$Root
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path $Root).Path
$here = $PSScriptRoot
$manifest = Join-Path $Root 'src-tauri\Cargo.toml'
$env:CARGO_TARGET_DIR = Join-Path $Root 'target'

function Fail([string]$reason, [string]$extra = '') {
    Write-Output "REPO-CHECK: FAIL $reason"
    if ($extra) { Write-Output ($extra.Trim() | Select-Object -First 1) }
    exit 1
}

function Pass { Write-Output 'REPO-CHECK: PASS'; exit 0 }

function Get-LfText([string]$path) {
    $bytes = [System.IO.File]::ReadAllBytes($path)
    $text = [System.Text.Encoding]::UTF8.GetString($bytes)
    return $text.Replace("`r`n", "`n")
}

function Get-Sha([string]$text) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $hash = $sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($text))
    return ([BitConverter]::ToString($hash) -replace '-', '').ToLowerInvariant()
}

function Invoke-Cargo {
    param([string[]]$CargoArgs)
    $all = @('test', '--manifest-path', $manifest, '--offline', '--color', 'never') + $CargoArgs
    $out = & cargo @all 2>&1 | Out-String
    return @{ Code = $LASTEXITCODE; Out = $out }
}

function Expect-CargoOk([string[]]$CargoArgs, [string[]]$MustMention = @()) {
    $r = Invoke-Cargo $CargoArgs
    if ($r.Code -ne 0) { Fail "cargo 失败: $($CargoArgs -join ' ')" ($r.Out -split "`n" | Select-Object -Last 25) }
    if ($r.Out -notmatch 'test result: ok') { Fail "cargo 无 ok 结果: $($CargoArgs -join ' ')" ($r.Out -split "`n" | Select-Object -Last 15) }
    foreach ($m in $MustMention) {
        if ($r.Out -notmatch [regex]::Escape($m)) { Fail "缺少用例 $m" ($r.Out -split "`n" | Select-Object -Last 15) }
    }
}

function Assert-Absent([string]$pattern, [string[]]$files) {
    foreach ($f in $files) {
        $p = Join-Path $Root $f
        if (-not (Test-Path $p)) { Fail "缺少文件 $f" }
        if ((Get-LfText $p) -match $pattern) { Fail "仍有残留匹配 /$pattern/ 于 $f" }
    }
}

function Assert-AbsentInTree([string]$pattern, [string]$dir, [string]$filter = '*.rs') {
    $hits = Get-ChildItem (Join-Path $Root $dir) -Recurse -Filter $filter -File |
        Where-Object { (Get-LfText $_.FullName) -match $pattern }
    if ($hits) { Fail "仍有残留匹配 /$pattern/ 于 $($hits[0].FullName.Substring($Root.Length + 1))" }
}

function Assert-Present([string]$pattern, [string]$file) {
    $p = Join-Path $Root $file
    if (-not (Test-Path $p)) { Fail "缺少文件 $file" }
    if ((Get-LfText $p) -notmatch $pattern) { Fail "缺少 /$pattern/ 于 $file" }
}

# ---------- 守卫：测试区 / 判据文件不许改 ----------
function Test-Guards {
    $guardFile = Join-Path $here 'guards.json'
    if (-not (Test-Path $guardFile)) { Fail 'guards.json 缺失，判据未就绪' }
    $guards = (Get-Content $guardFile -Raw -Encoding UTF8 | ConvertFrom-Json).PSObject.Properties[$Task]
    if (-not $guards) { return }
    foreach ($g in $guards.Value) {
        $p = Join-Path $Root $g.path
        if (-not (Test-Path $p)) { Fail "守卫目标缺失 $($g.path)" }
        $text = Get-LfText $p
        switch ($g.mode) {
            'whole_hash' {
                if ((Get-Sha $text) -ne $g.value) { Fail "判据文件被改: $($g.path)" }
            }
            'tail_hash' {
                $idx = $text.IndexOf('#[cfg(test)]')
                if ($idx -lt 0) { Fail "守卫目标无测试区: $($g.path)" }
                if ((Get-Sha $text.Substring($idx)) -ne $g.value) { Fail "测试区被改: $($g.path)" }
            }
            'assert_min' {
                $n = ([regex]::Matches($text, 'assert(_eq|_ne)?!')).Count
                if ($n -lt [int]$g.value) { Fail "断言被删减: $($g.path) 现有 $n < 要求 $($g.value)" }
            }
            default { Fail "未知守卫模式 $($g.mode)" }
        }
    }
}

Test-Guards

switch ($Task) {
    'retry-after' {
        Expect-CargoOk @('--lib', 'model_client::retry') @('provider_retry_after_wins_and_oversize_gives_up ... ok')
    }
    'apply-patch-read-guard' {
        Expect-CargoOk @('--lib', 'tools::apply_patch') @(
            'update_without_read_is_rejected ... ok',
            'stale_after_external_change_is_rejected_and_current_content_returned ... ok'
        )
    }
    'rollout-damaged' {
        Expect-CargoOk @('--lib', 'rollout::') @(
            'tolerates_torn_last_line_and_reports_middle_corruption ... ok'
        )
    }
    'compaction-tail' {
        Expect-CargoOk @('--lib', 'compaction::') @(
            'always_keeps_the_newest_item ... ok',
            'keep_allowance_boundary_keeps_the_item_that_fits_exactly ... ok'
        )
    }
    'context-overflow-code' {
        Expect-CargoOk @('--test', 'repo_feat_context_overflow')
        Expect-CargoOk @('--lib', 'agent_loop::error_codes')
        Expect-CargoOk @('--lib', 'model_client')
        Assert-Present 'ContextOverflow' 'src-tauri\src\model_client\error.rs'
    }
    'title-cap' {
        Expect-CargoOk @('--test', 'repo_feat_title_cap')
        Expect-CargoOk @('--lib', 'session_title')
    }
    'now-ms-exit' {
        Expect-CargoOk @('--test', 'now_ms_single_source')
        Expect-CargoOk @('--lib', 'clock::')
        Expect-CargoOk @('--lib', 'session_query::')
    }
    'rename-title-api' {
        Expect-CargoOk @('--lib', 'session_title')
        Assert-AbsentInTree 'title_from_output' 'src-tauri\src'
        Assert-AbsentInTree 'title_from_output' 'src-tauri\tests'
        Assert-Present 'pub fn title_from_reply' 'src-tauri\src\session_title.rs'
    }
    'doc-max-turns' {
        $doc = Get-LfText (Join-Path $Root 'docs\04-协议规范.md')
        $code = Get-LfText (Join-Path $Root 'src-tauri\src\config.rs')
        $docVals = [regex]::Matches($doc, '(?m)^\s*max_turns\s*=\s*(\d+)') | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
        $codeVals = [regex]::Matches($code, '(?m)^\s*max_turns:\s*(\d+)\s*,') | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
        if (-not $docVals) { Fail '文档里找不到 max_turns = N' }
        if (-not $codeVals) { Fail '代码里找不到 max_turns 数字默认值' }
        if (($docVals -join ',') -ne ($codeVals -join ',')) {
            Fail "文档值 [$($docVals -join ',')] 与代码默认值 [$($codeVals -join ',')] 不一致"
        }
        Expect-CargoOk @('--lib', 'config::')
    }
    'doc-claim-rename' {
        Push-Location $Root
        try {
            $out = pwsh -NoProfile -File 'scripts/dev/check-doc-claims.ps1' 2>&1 | Out-String
            $code = $LASTEXITCODE
        } finally { Pop-Location }
        if ($code -ne 0) { Fail "check-doc-claims 退出码 $code" ($out -split "`n" | Select-Object -Last 20) }
        $ledger = Get-LfText (Join-Path $Root 'align\doc-claims.md')
        $row = ($ledger -split "`n" | Where-Object { $_ -match 'R2-010' } | Select-Object -First 1)
        if (-not $row) { Fail 'R2-010 行被删（不许删行蒙混）' }
        if ($row -notmatch 'parse_endpoint_metadata') { Fail 'R2-010 行未同步到新符号' }
        Assert-Present 'parse_endpoint_metadata' 'docs\01-架构需求文档.md'
        Assert-Absent '\bparse_metadata\b' @('src-tauri\src\capability.rs', 'src-tauri\src\model_discovery.rs')
    }
    default { Fail "未知判据 $Task" }
}

Pass
