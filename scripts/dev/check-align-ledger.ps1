<#
.SYNOPSIS
  推前自检：入库的上游对齐账本（align/ledger.md）是否与 HEAD 的代码一致。

.DESCRIPTION
  align/ledger.md 是**生成物**。改了 `src/dsh/packages/client` 下任何 vendored 文件却没跑
  `pnpm align:report` 重新生成，CI 的「上游对齐账本（ledger.md 过期即失败）」就会红。
  这件事连着红过两回，所以把 CI 的判据原样搬到本地，让人**推之前**就知道。

  为什么用 `git archive HEAD` 抽一棵干净树再跑：
  align-report.mjs 是逐文件**内容哈希**比对。在工作树里直接跑，会把别人正在写的 WIP
  一起算成 drift——"账本过期"的假象就是这么来的。`git archive HEAD` 只取已提交内容，
  没有未提交改动、没有未跟踪文件，结论只反映"这次要推的东西"。

  与 CI 一致的三件事（这条线不放宽、不替代 CI）：
    1) 判定用同一条 `align-report.mjs --check`（`pnpm align:report --check` 就是它）；
    2) 上游用 CI 冻结的那个 commit——直接从 `.github/workflows/ci.yml` 读，不另抄一份；
    3) 上游/本地的相对路径不变，只是把本地那侧换成干净树。

  退出码：0 = 账本新鲜；1 = 账本过期；2 = 环境不满足（缺 node / 缺上游 / 上游不在冻结
  commit），此时**不下结论**——不可比就不给绿。

  本地这层只做提前告知：绿了不等于 CI 一定绿（CI 还跑别的检查），红了则一定会红。

.PARAMETER Upstream
  上游 deepseek-harness 的本地检出。默认取 env:DSH_UPSTREAM，再缺省为 D:\AI\deepseek-harness。

.EXAMPLE
  pwsh scripts/dev/check-align-ledger.ps1

.EXAMPLE
  pwsh scripts/dev/check-align-ledger.ps1 -Upstream 'D:\AI\deepseek-harness'
#>
param(
    [string]$Upstream = ""
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

if (-not $Upstream) {
    $Upstream = if ($env:DSH_UPSTREAM) { $env:DSH_UPSTREAM } else { "D:\AI\deepseek-harness" }
}

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    Write-Host "[align-precheck] 找不到 node：本检查复用 node 版生成器。" -ForegroundColor Yellow
    exit 2
}
if (-not (Test-Path (Join-Path $Upstream ".git"))) {
    Write-Host "[align-precheck] 上游检出不存在：$Upstream（用 -Upstream 或 DSH_UPSTREAM 指定）" -ForegroundColor Yellow
    exit 2
}

# 冻结 commit 只认 ci.yml 里那一处字面量：本地抄一份数字，迟早与 CI 各说各话。
$ci = Join-Path $repoRoot ".github\workflows\ci.yml"
$frozen = $null
$matched = Select-String -Path $ci -Pattern 'fetch --quiet --depth 1 origin ([0-9a-f]{40})' |
    Select-Object -First 1
if ($matched) { $frozen = $matched.Matches[0].Groups[1].Value }

$upHead = (& git -C $Upstream rev-parse HEAD 2>$null)
if (-not $frozen) {
    Write-Host "[align-precheck] 没能从 ci.yml 读到冻结 commit，跳过版本核对。" -ForegroundColor Yellow
}
elseif ($upHead -ne $frozen) {
    Write-Host "[align-precheck] 上游不是 CI 冻结的 commit，结论不可比：" -ForegroundColor Yellow
    Write-Host "    CI  : $frozen"
    Write-Host "    本地: $upHead"
    Write-Host "    切到冻结 commit 后再跑，别拿别的版本下结论。"
    exit 2
}

$tree = Join-Path $env:TEMP ("shaniu-align-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
$zip = "$tree.zip"
$prevUpstream = $env:DSH_UPSTREAM
New-Item -ItemType Directory -Force -Path $tree | Out-Null
try {
    # 用 zip 而不是 tar：Windows 的 bsdtar 解 git archive 的 tar 时，中文路径会报
    # "Invalid empty pathname"（docs/0x-*.md 直接解不出来）；zip + Expand-Archive 正常。
    & git -C $repoRoot archive --format=zip -o $zip HEAD
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[align-precheck] git archive 失败（退出码 $LASTEXITCODE）" -ForegroundColor Yellow
        exit 2
    }
    Expand-Archive -Path $zip -DestinationPath $tree -Force

    Write-Host "[align-precheck] 干净树：$tree"
    Write-Host "[align-precheck] 只含 HEAD 已提交内容，工作树里别人的 WIP 不会算成 drift"
    $env:DSH_UPSTREAM = $Upstream
    Push-Location $tree
    try {
        & node (Join-Path $tree "scripts\align-report.mjs") --check
        $code = $LASTEXITCODE
    }
    finally {
        Pop-Location
    }

    if ($code -eq 0) {
        Write-Host "[align-precheck] 账本新鲜（与 HEAD 一致）" -ForegroundColor Green
        exit 0
    }
    if ($code -eq 2) {
        Write-Host "[align-precheck] 生成器环境不满足（见上），不是账本结论" -ForegroundColor Yellow
        exit 2
    }
    Write-Host "[align-precheck] 账本过期：跑 pnpm align:report 重新生成 align/ledger.md 并一起提交。" -ForegroundColor Red
    Write-Host "                 账本是生成物，只提交代码不提交它，CI 必然红。"
    exit 1
}
finally {
    $env:DSH_UPSTREAM = $prevUpstream
    if (Test-Path $zip) { Remove-Item -Force $zip }
    if (Test-Path $tree) { Remove-Item -Recurse -Force $tree }
}
