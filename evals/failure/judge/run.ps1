param(
    [Parameter(Mandatory = $true)][string]$TaskId,
    [Parameter(Mandatory = $true)][string]$Observation
)

# 失败面题集的判据。契约与 evals/repo/judge/run.ps1 相同：
# 最后一行输出 REPO-CHECK: PASS / REPO-CHECK: FAIL，退出码 0 / 1。
# 判据只读观测里的宿主事实，不读产品日志、不做「看起来完成了」的推断。

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $Observation)) {
    Write-Host "REPO-CHECK: FAIL - 观测文件不存在: $Observation"
    exit 1
}

$document = Get-Content -LiteralPath $Observation -Raw | ConvertFrom-Json
$task = @($document.tasks | Where-Object { $_.id -eq $TaskId }) | Select-Object -First 1
if ($null -eq $task) {
    Write-Host "REPO-CHECK: FAIL - 观测里找不到题 $TaskId"
    exit 1
}

$script:failures = New-Object System.Collections.Generic.List[string]

function Fail([string]$message) {
    $script:failures.Add($message) | Out-Null
}

function Assert-True([bool]$condition, [string]$message) {
    if (-not $condition) { Fail $message }
}

function Get-Requests($t) { @($t.requests) }

function Get-Events($t, [string]$type) {
    @($t.events | Where-Object { $_.type -eq $type })
}

function Get-Retries($t) { Get-Events $t "model_retry" }

function Get-Probe($request, [string]$name) {
    $property = $request.probes.PSObject.Properties[$name]
    if ($null -eq $property) { return $false }
    return [bool]$property.Value
}

function Get-RolloutItems($t) {
    if ($null -eq $t.rollout) { return @() }
    return @($t.rollout.items)
}

function Assert-NoAssistantRow($t, [string]$label) {
    $rows = @(Get-RolloutItems $t | Where-Object {
            $_.type -eq "response_item" -and $_.role -eq "assistant"
        })
    Assert-True ($rows.Count -eq 0) "$label`: 半截/失败正文不应落成助手历史，实际 $($rows.Count) 条"
}

function Assert-OverflowReplay($t, [string]$label) {
    $requests = @(Get-Requests $t)
    Assert-True ($requests.Count -eq 3) "$label`: 应有 3 次请求（首发 / 摘要 / 重放），实际 $($requests.Count)"
    if ($requests.Count -eq 3) {
        Assert-True ($requests[0].kind -eq "sample") "$label`: 第 1 次应是主采样请求"
        Assert-True ($requests[1].kind -eq "summary") "$label`: 第 2 次应是摘要请求，实际 kind=$($requests[1].kind)"
        Assert-True ($requests[2].kind -eq "sample") "$label`: 第 3 次应是重放本步"
        Assert-True (Get-Probe $requests[0] "SEED_OVERFLOW_SHADOW") "$label`: 原请求应包含被摘要区间的标记"
        Assert-True (Get-Probe $requests[2] "【交接摘要】") "$label`: 重放请求应包含压缩摘要"
        Assert-True (-not (Get-Probe $requests[2] "SEED_OVERFLOW_SHADOW")) "$label`: 重放请求不应再含被摘要区间的标记"
        Assert-True ([int]$requests[2].input_chars -lt [int]$requests[0].input_chars) `
            "$label`: 重放请求必须变小（$($requests[2].input_chars) < $($requests[0].input_chars)）"
    }

    $started = @(Get-Events $t "compaction_started")
    $summarized = @(Get-Events $t "compaction_summarized")
    $ended = @(Get-Events $t "compaction_ended")
    Assert-True ($started.Count -eq 1) "$label`: 应恰好压缩 1 次，实际 $($started.Count)"
    Assert-True ($summarized.Count -eq 1) "$label`: 应恰好产出 1 份摘要，实际 $($summarized.Count)"
    Assert-True ($ended.Count -eq 1) "$label`: 应恰好收口 1 次压缩，实际 $($ended.Count)"
    if ($started.Count -ge 1) {
        Assert-True ($started[0].trigger -eq "context_overflow") `
            "$label`: 压缩触发原因应为 context_overflow，实际 $($started[0].trigger)"
    }
    if ($summarized.Count -ge 1) {
        Assert-True ([int]$summarized[0].shadowed_items -ge 1) `
            "$label`: 摘要必须真的遮蔽历史（shadowed_items=$($summarized[0].shadowed_items)）"
    }
    if ($ended.Count -ge 1) {
        Assert-True ([bool]$ended[0].ok) "$label`: 压缩应以 ok=true 收口"
    }
    Assert-True ($task.turn.reason -eq "completed") "$label`: 回合应以 completed 收口，实际 $($task.turn.reason)"

    $deltas = @(Get-Events $t "agent_message_delta")
    Assert-True ($deltas.Count -eq 1) "$label`: 只应交付重放后那一份正文，实际 $($deltas.Count) 份"
    if ($deltas.Count -ge 1) {
        Assert-True ($deltas[0].delta -like "*压缩后重放成功*") "$label`: 正文应来自重放后的答复"
    }
}

switch ($task.judge_key) {
    "truncate-before-delta" {
        $requests = @(Get-Requests $task)
        Assert-True ($requests.Count -eq 2) "首发 + 重试应共 2 次请求，实际 $($requests.Count)"
        $retries = @(Get-Retries $task)
        $phases = @($retries | ForEach-Object { $_.phase })
        Assert-True ($phases -contains "scheduled") "缺少 model_retry.scheduled（流截断未进入重试）"
        Assert-True ($phases -contains "started") "缺少 model_retry.started（重试未真正发起）"
        $scheduled = @($retries | Where-Object { $_.phase -eq "scheduled" }) | Select-Object -First 1
        if ($null -ne $scheduled) {
            Assert-True ([int]$scheduled.delay_ms -eq 50) "scheduled.delay_ms 应为 50，实际 $($scheduled.delay_ms)"
        }
        Assert-True ($task.turn.reason -eq "completed") "回合应以 completed 收口，实际 $($task.turn.reason)"
        $deltas = @(Get-Events $task "agent_message_delta")
        Assert-True ($deltas.Count -eq 1) "正文只应交付一次，实际 $($deltas.Count)"
        if ($deltas.Count -ge 1) {
            Assert-True ($deltas[0].delta -like "*已恢复*") "正文应来自重试后的正式回答"
        }
    }

    "truncate-after-delta" {
        $requests = @(Get-Requests $task)
        Assert-True ($requests.Count -eq 1) "吐过增量后不得重试，应只请求 1 次，实际 $($requests.Count)"
        Assert-True (@(Get-Retries $task).Count -eq 0) "吐过增量后不应出现 model_retry"
        Assert-True ($task.turn.reason -eq "failed") "回合应以 failed 收口，实际 $($task.turn.reason)"
        Assert-True ($null -ne $task.turn_error) "failed 回合应带结构化 error"
        if ($null -ne $task.turn_error) {
            Assert-True ($task.turn_error.code -eq "NETWORK") "错误码应为 NETWORK，实际 $($task.turn_error.code)"
            Assert-True ([int]$task.turn_error.attempts -eq 0) "attempts 应为 0，实际 $($task.turn_error.attempts)"
        }
        Assert-True (@(Get-Events $task "tool_call_begin").Count -eq 0) "流截断后不得执行任何工具"
        Assert-NoAssistantRow $task "流截断"
        $deltas = @(Get-Events $task "agent_message_delta")
        Assert-True ($deltas.Count -eq 1) "被截断前的增量应只播报一次，实际 $($deltas.Count)"
    }

    "rate-limit-retry-after" {
        $requests = @(Get-Requests $task)
        Assert-True ($requests.Count -eq 2) "限流后应重试一次，共 2 次请求，实际 $($requests.Count)"
        $gaps = @($task.request_gaps_ms)
        Assert-True ($gaps.Count -ge 1) "缺少请求间隔，无法核对 Retry-After"
        if ($gaps.Count -ge 1) {
            Assert-True ([int]$gaps[0] -ge 950) "请求间隔应 >= 950ms（按 Retry-After: 1），实际 $($gaps[0])ms"
            Assert-True ([int]$gaps[0] -le 3000) "请求间隔不应超过 3000ms，实际 $($gaps[0])ms"
        }
        $retries = @(Get-Retries $task)
        $phases = @($retries | ForEach-Object { $_.phase })
        Assert-True ($phases -contains "scheduled") "缺少 model_retry.scheduled"
        Assert-True ($phases -contains "started") "缺少 model_retry.started"
        $scheduled = @($retries | Where-Object { $_.phase -eq "scheduled" }) | Select-Object -First 1
        if ($null -ne $scheduled) {
            Assert-True ([int]$scheduled.delay_ms -eq 1000) "scheduled.delay_ms 应取 Retry-After=1000，实际 $($scheduled.delay_ms)"
        }
        Assert-True ($task.turn.reason -eq "completed") "回合应以 completed 收口，实际 $($task.turn.reason)"
        $deltas = @(Get-Events $task "agent_message_delta")
        Assert-True ($deltas.Count -eq 1) "正文只应交付一次，实际 $($deltas.Count)"
    }

    "quota-exhausted" {
        $requests = @(Get-Requests $task)
        Assert-True ($requests.Count -eq 1) "配额打满不得重试，应只请求 1 次，实际 $($requests.Count)"
        Assert-True (@(Get-Retries $task).Count -eq 0) "配额打满不应出现 model_retry"
        Assert-True ($task.turn.reason -eq "failed") "回合应以 failed 收口，实际 $($task.turn.reason)"
        Assert-True ($null -ne $task.turn_error) "failed 回合应带结构化 error"
        if ($null -ne $task.turn_error) {
            Assert-True ($task.turn_error.code -eq "QUOTA") "错误码应为 QUOTA（错误体优先于状态码），实际 $($task.turn_error.code)"
            Assert-True ([int]$task.turn_error.http_status -eq 429) "http_status 应为 429，实际 $($task.turn_error.http_status)"
            Assert-True ($task.turn_error.provider_code -eq "insufficient_quota") `
                "provider_code 应原样保留 insufficient_quota，实际 $($task.turn_error.provider_code)"
        }
    }

    "incomplete-max-output" {
        $requests = @(Get-Requests $task)
        Assert-True ($requests.Count -eq 1) "响应不完整不得重试，应只请求 1 次，实际 $($requests.Count)"
        Assert-True (@(Get-Retries $task).Count -eq 0) "响应不完整不应出现 model_retry"
        Assert-True ($task.turn.reason -eq "failed") "回合应以 failed 收口，实际 $($task.turn.reason)"
        Assert-True ($null -ne $task.turn_error) "failed 回合应带结构化 error"
        if ($null -ne $task.turn_error) {
            Assert-True ($task.turn_error.code -eq "INCOMPLETE") "错误码应为 INCOMPLETE，实际 $($task.turn_error.code)"
            Assert-True ($task.turn_error.message -like "*最大输出长度*") `
                "错误文案应点明达到最大输出长度，实际: $($task.turn_error.message)"
        }
        Assert-NoAssistantRow $task "响应不完整"
    }

    "context-overflow-stream-replay" {
        Assert-OverflowReplay $task "流内上下文超长"
    }

    "context-overflow-http400" {
        Assert-OverflowReplay $task "HTTP 400 上下文超长"
    }

    "long-session-compaction-anchor" {
        $requests = @(Get-Requests $task)
        Assert-True ($requests.Count -eq 2) "应先摘要再作答，共 2 次请求，实际 $($requests.Count)"
        if ($requests.Count -eq 2) {
            Assert-True ($requests[0].kind -eq "summary") "第 1 次应是摘要请求，实际 kind=$($requests[0].kind)"
            Assert-True ($requests[1].kind -eq "sample") "第 2 次应是主采样请求，实际 kind=$($requests[1].kind)"
            Assert-True (Get-Probe $requests[0] "SEED_LONG_SHADOW") "摘要请求应覆盖被遮蔽的历史段落"
            Assert-True (Get-Probe $requests[1] "【交接摘要】") "压缩后的主请求应包含摘要"
            Assert-True (-not (Get-Probe $requests[1] "SEED_LONG_SHADOW")) "压缩后的主请求不应再含被遮蔽标记"
        }

        $started = @(Get-Events $task "compaction_started")
        Assert-True ($started.Count -eq 1) "应恰好压缩 1 次，实际 $($started.Count)"
        if ($started.Count -ge 1) {
            Assert-True ($started[0].trigger -eq "pressure") "预算压缩的 trigger 应为 pressure，实际 $($started[0].trigger)"
            Assert-True ([int]$started[0].budget_tokens -eq 400) "budget_tokens 应为 400，实际 $($started[0].budget_tokens)"
        }

        $items = @(Get-RolloutItems $task)
        Assert-True ($null -ne $task.rollout) "该题必须写施工档案（rollout）"
        $anchorIndex = -1
        for ($i = 0; $i -lt $items.Count; $i++) {
            if ($items[$i].type -eq "compacted") { $anchorIndex = $i; break }
        }
        Assert-True ($anchorIndex -ge 0) "档案里找不到 Compacted 锚点"
        if ($anchorIndex -ge 0) {
            $before = @()
            if ($anchorIndex -gt 0) {
                $before = @($items[0..($anchorIndex - 1)] | Where-Object { $_.type -eq "response_item" })
            }
            Assert-True ($before.Count -ge 2) "锚点之前应已有被遮蔽的历史条目，实际 $($before.Count)"
            Assert-True ([int]$items[$anchorIndex].range_start -eq 0) "锚点 range_start 应为 0，实际 $($items[$anchorIndex].range_start)"
            $after = @()
            if ($anchorIndex -lt ($items.Count - 1)) {
                $after = @($items[($anchorIndex + 1)..($items.Count - 1)] | Where-Object { $_.type -eq "response_item" -and $_.role -eq "assistant" })
            }
            Assert-True ($after.Count -ge 1) "锚点之后应能看到压缩后的助手回复"
            if ($after.Count -ge 1) {
                Assert-True ($after[-1].text_head -like "*长会话压缩后继续*") "压缩后的助手回复内容不符"
            }
        }

        Assert-True ($null -ne $task.rollout.resume) "缺少 resume 观测"
        if ($null -ne $task.rollout -and $null -ne $task.rollout.resume) {
            $resume = $task.rollout.resume
            Assert-True ([bool]$resume.ok) "resume 失败: $($resume.error)"
            if ($resume.ok) {
                Assert-True ([int]$resume.applied_compactions -eq 1) "resume 应应用 1 个压缩锚点，实际 $($resume.applied_compactions)"
                Assert-True ([int]$resume.history_len -lt [int]$resume.seeded_len) `
                    "resume 后的历史应变短（$($resume.history_len) < $($resume.seeded_len)）"
                Assert-True ([bool]$resume.first_user_contains_summary) `
                    "resume 后首条用户历史应包含摘要，实际 head: $($resume.first_user_text_head)"
            }
        }

        Assert-True ($task.turn.reason -eq "completed") "回合应以 completed 收口，实际 $($task.turn.reason)"
        $deltas = @(Get-Events $task "agent_message_delta")
        Assert-True ($deltas.Count -eq 1) "只应交付一份正文，实际 $($deltas.Count)"
    }

    default {
        Fail "未知判据键: $($task.judge_key)"
    }
}

if ($script:failures.Count -eq 0) {
    Write-Host "REPO-CHECK: PASS"
    exit 0
}

foreach ($failure in $script:failures) {
    Write-Host "  - $failure"
}
Write-Host "REPO-CHECK: FAIL"
exit 1
