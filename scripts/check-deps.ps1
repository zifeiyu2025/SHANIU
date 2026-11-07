#Requires -Version 5.1

<#
.SYNOPSIS
    依赖方向检查（FF-1，docs/01 §5 健身函数表，阈值 = 0）。

.DESCRIPTION
    白名单 fail-closed：每个模块允许依赖谁，按 docs/01 §2.3.1（E02–E20）逐条正向登记在 $allow 里。
    - 表外模块 → 违规（要求先去 §2.3.1 登记，再同步脚本），避免 rollout / compaction /
      subagent_registry / mcp_client 这类新模块一落地就天然免检；
    - 不在允许集合里的边 → 违规；
    - 三类特殊角色不参与普通分层：同模块自引用一律放行；config/events 任何模块可依赖、
      但它们自身只能依赖彼此；组合根 lib/main/app_state 跳过判定。

    识别方式（对绕过写法有要求，勿简化回单行正则）：
      1. 逐字符状态机剥离注释与字符串内容（Normal / LineComment / BlockComment / String / Char / RawString），
         字符串内的 `//`、`/*`、`{`、`}` 一律不当语法；raw string（`r"…"`、`r#"…"#`）原样跳过且不做
         转义处理——否则 `r"C:\"` 的结尾引号会被当成转义吃掉，把后续整段代码静默剥光；行号一一对应；
      2. `use` 语句先拼成逻辑行（rustfmt 会把长 import 折行），再把 use 树展开成完整路径
         （`use crate::{a::B, c::D};` → crate::a::B 与 crate::c::D）；
      3. 路径解析成绝对模块路径再判定：`crate::p` → p；`super::p` → 模块路径去掉末 1 段后接 p
         （`super` 层数 ≥ 段数时即 crate 根）；`self::p` → 模块路径 + p；裸路径按外部 crate 跳过；
      4. `#[cfg(test)]` 仅在其后紧跟 `mod xxx {` 时进入跳过态，按花括号深度退出——
         测试模块之后的生产代码照样检查，中间插一条 `#[cfg(test)] use ...;` 不会盲掉整个文件。

    退出码：0 = 通过；1 = 有违规；2 = 找不到后端源码目录；3 = 脚本自身异常（不是违规，别去改业务代码）。

.NOTES
    无副作用、不联网、可重复执行；仓库根由 $PSScriptRoot 推导。
    docs/01 §5 把本脚本列为本地检查路径，故必须同时兼容 Windows PowerShell 5.1 与 PowerShell 7。
#>

$ErrorActionPreference = 'Stop'

# 显式 UTF-8 输出，避免中文违规说明在 CI 日志里乱码（进程内生效，无持久影响）
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

# 白名单（正向建表）：模块 → 允许依赖的目标模块，逐条取自 docs/01 §2.3.1（E02–E20）。
# 正向建表而非"黑名单取补集"：后者等于"未列出的边一律允许"，会把漏洞换个位置留着。
# 改规则必须先改 §2.3.1，再改本节——禁止把架构规则只藏在脚本里。
# 三类特殊角色见下方 $skipOwners / $infraModules，不参与普通分层判定。
#
# 注意空数组不是漏登记：permission / sandbox / model_client / events / config 的合法目标
# 只有共享基础设施 config、events，而它们由下方 $infraModules 全局放行，故表内无需再列，
# 顺手补进去只会造成冗余。要给这些模块新增业务依赖，必须先改 §2.3.1，再写进这里。
$allow = @{
    'agent_loop'   = @('model_client', 'tool_router', 'permission', 'rollout', 'compaction', 'session_reference', 'skills', 'jobs')  # E02–E04、E06、E23、E35、E40、E43
    'tool_router'  = @('model_client', 'tools', 'jobs')                          # E08、E11、E43
    'tools'        = @('tool_router', 'sandbox', 'permission', 'skills', 'jobs')  # E12–E14、E41、E43
    'permission'   = @()
    'sandbox'      = @()
    'model_client' = @()
    'events'       = @()
    'config'       = @()
    'rollout'      = @('model_client')                                            # E21
    'compaction'   = @('model_client')                                             # E22
    'session_query'= @('rollout', 'model_client')                                  # E24、E25（SH-113 只读派生索引）
    'model_discovery' = @('app_state')                                             # E26（SH-098 命令壳取 tauri::State；config 属 infra 全局放行）
    'session_title'   = @('model_client', 'rollout', 'app_state')                   # E27（SH-104；config 属 infra 全局放行）
    'session_export'  = @()                                                          # E28（SH-114 批 2 ZIP 编码；零业务依赖，档案读取在组合根 lib.rs）
    'session_compact' = @('compaction', 'model_client', 'rollout')                    # E29（SH-115 /compact；config 属 infra 全局放行，守卫在组合根 lib.rs）
    'workspace_files' = @('tool_router', 'rollout', 'app_state')                                 # E30（SH-092 UI 预览读：shell_usable 复用 + 档案首行取会话根；config 属 infra 全局放行）
    'file_actions'    = @()                                                          # E31（SH-092 批 2 原生另存为：base64 解码 + 保存对话框，零业务依赖）
    'open_in_app'     = @()                                                          # E32（SH-092 批 3 open-in-app：注册表关联 + ShellExecute，零业务依赖）
    'file_references' = @('workspace_files', 'app_state')                             # E33（SH-090 批 1 @file 候选：复用会话根解析 + tauri::State 取句柄）
    'session_reference' = @('model_client', 'rollout', 'workspace_files', 'app_state') # E34（SH-090 批 2 会话引用：历史项类型 + 读被引用档案 + 会话根 + State 句柄）
    'event_journal'   = @()                                                          # E36（SH-094 批 1 可续播事件日志：只追加/游标读 AgentEvent；events 属 infra 全局放行，零业务依赖）
    'gateway'         = @('event_journal')                                           # E37（SH-094 批 2 loopback WS gateway：续播复用 event_journal::read_after；events/config 属 infra 全局放行）
    'attachments'     = @()                                                          # E38（SH-116 批 1 会话附件暂存：base64/ULID/元数据只依赖 std 库，零业务依赖）
    'skills'          = @('model_client', 'workspace_files', 'app_state')            # E39（SH-091 技能系统：HistoryItem 同源 + 会话根解析 + State 句柄；config 属 infra 全局放行）
    'subagent_registry' = @('agent_loop', 'rollout')                                  # E42（SH-051 子代理注册表：复用 run_instance/AgentInstance + 每代理档案写入器）
    'jobs'            = @('app_state')                                                # E43（SH-089 后台作业注册表：从装配层取注册表句柄）
    'goals'           = @('app_state')                                                # E44（SH-088 会话目标持久层：从装配层取数据根）
}
# rollout 已落地（SH-030）：它只借用 model_client 的 HistoryItem 作为条目载荷，
# 不反向依赖循环或工具层——档案是被读写的对象，不是参与者。
# 组合根 / 装配层：只做接线，允许依赖任意模块，不参与分层判定（§2.3.3）。
# 豁免判据按**模块身份**，不按名字：
#   - lib / main 是 crate 根 / bin 根，Rust 里只能是文件 → 按顶层文件精确路径豁免；
#   - app_state 是装配层**模块**，将来长成目录（app_state/mod.rs、app_state/*.rs）仍属装配层
#     → 按 owner 豁免，避免重构 CG 后 CI 无端 fail-closed。
# 同名目录不是组合根（如 src/lib/）：$skipFiles 按完整相对路径匹配，子目录里的 lib.rs 不会被豁免（假绿防线）。
$skipFiles = @('lib.rs', 'main.rs')
$skipOwners = @('app_state')
# 共享基础设施：任何模块都可依赖它们，不必逐条登记；它们自身只能依赖彼此（§2.3.3）
$infraModules = @('config', 'events')

$repoRoot = Split-Path -Parent $PSScriptRoot

# 去掉注释与字符串内容，只留代码骨架；块注释/字符串的跨行状态在文件内传递。
function Get-CodeText {
    param([string[]]$Lines)

    $inBlock = $false
    $inString = $false
    # raw string 状态：-1 表示不在其中；>=0 表示已进入，值为 `#` 的个数（结束符 `"` + 同等数量 `#`）
    $rawHashes = -1
    foreach ($line in $Lines) {
        $sb = [System.Text.StringBuilder]::new()
        $i = 0
        while ($i -lt $line.Length) {
            $c = $line[$i]
            if ($rawHashes -ge 0) {
                $needle = '"' + (('#' * $rawHashes) -join '')
                $end = $line.IndexOf($needle, $i)
                if ($end -lt 0) { $i = $line.Length }
                else { $i = $end + $needle.Length; $rawHashes = -1 }
                continue
            }
            if ($inBlock) {
                if ($c -eq '*' -and $i + 1 -lt $line.Length -and $line[$i + 1] -eq '/') { $inBlock = $false; $i += 2 }
                else { $i++ }
                continue
            }
            if ($inString) {
                if ($c -eq '\') { $i += 2; continue }   # 转义：跳过被转义字符，避免 \" 提前收尾
                if ($c -eq '"') { $inString = $false }
                $i++
                continue
            }
            if ($c -eq '/' -and $i + 1 -lt $line.Length) {
                if ($line[$i + 1] -eq '/') { break }
                if ($line[$i + 1] -eq '*') { $inBlock = $true; $i += 2; continue }
            }
            # raw string 起始：r" / r#" / r##" …（后面不做转义处理）
            if ($c -eq 'r') {
                $raw = [regex]::Match($line.Substring($i), '^r(#*)(")')
                if ($raw.Success) {
                    $rawHashes = $raw.Groups[1].Value.Length
                    $i += $raw.Length
                    continue
                }
            }
            if ($c -eq '"') { $inString = $true; $i++; continue }
            if ($c -eq "'") {
                # 只把 'x' / '\n' 当字符字面量，避免把生命周期 &'a 误当成字符串
                $m = [regex]::Match($line.Substring($i), "^'(\\.[^']|[^\\'])'")
                if ($m.Success) { $i += $m.Length; continue }
            }
            [void]$sb.Append($c)
            $i++
        }
        $sb.ToString()
    }
}

# 标出测试模块占据的行：仅 "#[cfg(test)] + mod xxx {" 形态成立，按大括号配平跳过，闭合后恢复。
function Get-TestLineMask {
    param([string[]]$Code)

    $skip = [bool[]]::new($Code.Count)
    $i = 0
    while ($i -lt $Code.Count) {
        if ($Code[$i] -match '^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*$') {
            $j = $i + 1
            while ($j -lt $Code.Count -and ($Code[$j].Trim() -eq '' -or $Code[$j] -match '^\s*#')) { $j++ }
            # 只认 "#[cfg(test)] + mod xxx" 形态；`mod tests;`（无函数体声明）不构成跳过条件
            if ($j -lt $Code.Count -and
                $Code[$j] -match '^\s*(pub\s+)?mod\s+[A-Za-z_]\w*' -and
                $Code[$j].Trim() -notmatch ';\s*$') {
                $skip[$i] = $true
                $depth = 0
                $opened = $false
                for ($k = $j; $k -lt $Code.Count; $k++) {
                    $skip[$k] = $true
                    $depth += ([regex]::Matches($Code[$k], '\{')).Count
                    $depth -= ([regex]::Matches($Code[$k], '\}')).Count
                    if ($depth -gt 0) { $opened = $true }
                    if ($opened -and $depth -le 0) { break }
                }
                $i = $k + 1
                continue
            }
        }
        $i++
    }
    , $skip
}

# use 树展开：crate::{a::B, c::{d, e}} → crate::a::B / crate::c::d / crate::c::e
function Add-UseNodes {
    param([string]$Prefix, [string]$Body, $Out)

    $items = [System.Collections.Generic.List[string]]::new()
    $depth = 0
    $cur = [System.Text.StringBuilder]::new()
    foreach ($ch in $Body.ToCharArray()) {
        if ($ch -eq '{') { $depth++ }
        elseif ($ch -eq '}') { $depth-- }
        if ($ch -eq ',' -and $depth -eq 0) { $items.Add($cur.ToString()); [void]$cur.Clear(); continue }
        [void]$cur.Append($ch)
    }
    if ($cur.Length -gt 0) { $items.Add($cur.ToString()) }

    foreach ($raw in $items) {
        $item = ($raw.Trim() -replace '\s+as\s+\w+$', '')
        if ($item -eq '') { continue }
        $open = $item.IndexOf('{')
        if ($open -ge 0) {
            $head = $item.Substring(0, $open).Trim()
            $inner = $item.Substring($open + 1, $item.LastIndexOf('}') - $open - 1)
            if ($head -eq '') { Add-UseNodes -Prefix $Prefix -Body $inner -Out $Out }
            else { Add-UseNodes -Prefix ($Prefix + $head) -Body $inner -Out $Out }
        }
        elseif ($item -eq 'self') { $Out.Add($Prefix.TrimEnd(':')) }
        else { $Out.Add($Prefix + $item) }
    }
}

function Expand-UsePaths {
    param([string]$Text)

    $body = ($Text -replace '^\s*(pub\s+)?use\s+', '').TrimEnd(';').Trim() -replace '^::', ''
    $out = [System.Collections.Generic.List[string]]::new()
    Add-UseNodes -Prefix '' -Body $body -Out $out
    $out
}

# super:: / self:: → crate::<当前模块路径>
function ConvertTo-AbsolutePath {
    param([string]$Text, [string[]]$ModulePath)

    for ($k = 6; $k -ge 1; $k--) {
        $supers = ('super::' * $k) -join ''
        $depth = [Math]::Max(0, $ModulePath.Count - $k)
        $prefix = 'crate::'
        if ($depth -gt 0) { $prefix += (($ModulePath[0..($depth - 1)] -join '::') + '::') }
        $Text = $Text.Replace($supers, $prefix)
    }
    if ($ModulePath.Count -gt 0) { $Text = $Text.Replace('self::', 'crate::' + ($ModulePath -join '::') + '::') }
    else { $Text = $Text.Replace('self::', 'crate::') }
    $Text
}

function Invoke-Check {
    # 路径也在 try 内解析：任何脚本自身故障都走 exit 2，绝不与"发现违规"的 exit 1 撞码
    # 不用 Join-Path 三参数形式（PowerShell 6+ 才有），保持 5.1 兼容
    $srcRoot = Join-Path (Join-Path $repoRoot 'src-tauri') 'src'

    if (-not (Test-Path -LiteralPath $srcRoot)) {
        Write-Host "[check-deps] 找不到后端源码目录: $srcRoot" -ForegroundColor Red
        exit 2
    }

    $files = Get-ChildItem -LiteralPath $srcRoot -Recurse -Filter '*.rs' -File
    $violations = [System.Collections.Generic.List[string]]::new()
    $seen = @{}

    foreach ($file in $files) {
        # 不用 [IO.Path]::GetRelativePath（.NET Core 专有，Windows PowerShell 5.1 没有）
        $rel = $file.FullName.Substring($srcRoot.Length + 1).Replace('\', '/')
        $parts = $rel -split '/'
        $owner = $parts[0] -replace '\.rs$', ''

        # 组合根 / 装配层跳过判定（§2.3.3）：lib/main 按顶层文件路径，app_state 按模块身份
        if ($skipFiles -contains $rel -or $skipOwners -contains $owner) { continue }

        # 白名单 fail-closed：表外模块一律违规，且提示去改文档（不是改脚本）
        if (-not $allow.ContainsKey($owner)) {
            $violations.Add(('src-tauri/src/{0}  模块 `{1}` 未登记 docs/01 §2.3.1 允许边表（fail-closed）→ 请先在 §2.3.1 登记该模块的允许边，再同步本脚本' -f $rel, $owner))
            continue
        }
        $allowed = $allow[$owner]

        # 文件所在模块路径：tools/fs_read.rs → tools::fs_read；agent_loop/mod.rs → agent_loop；lib.rs → 空
        $modulePath = @()
        if ($parts.Count -gt 1) {
            $modulePath = @($parts[0..($parts.Count - 2)])
            if ($parts[$parts.Count - 1] -ne 'mod.rs') {
                $modulePath += ($parts[$parts.Count - 1] -replace '\.rs$', '')
            }
        }

        $code = @(Get-CodeText -Lines (Get-Content -LiteralPath $file.FullName -Encoding UTF8))
        $skip = Get-TestLineMask -Code $code

        # 拼逻辑行：长 use 列表会被 rustfmt 折行，逐行匹配会漏
        $i = 0
        while ($i -lt $code.Count) {
            if ($skip[$i]) { $i++; continue }
            $lineNo = $i + 1
            $buf = $code[$i].Trim()
            $isUse = $buf -match '^\s*(pub\s+)?use\s'

            if ($isUse) {
                while ($buf -notmatch ';' -and $i + 1 -lt $code.Count) {
                    $i++
                    if (-not $skip[$i]) { $buf += ' ' + $code[$i].Trim() }
                }
            }

            $buf = ConvertTo-AbsolutePath -Text $buf -ModulePath $modulePath

            $moduleIds = @()
            if ($isUse) {
                foreach ($p in (Expand-UsePaths -Text $buf)) {
                    if ($p -match '^crate::([A-Za-z_]\w*)') { $moduleIds += $Matches[1] }
                }
            }
            $moduleIds += @([regex]::Matches($buf, '\bcrate::([A-Za-z_]\w*)') | ForEach-Object { $_.Groups[1].Value })

            foreach ($mod in ($moduleIds | Sort-Object -Unique)) {
                if ($mod -eq $owner) { continue }              # 同模块自引用不是依赖边
                if ($infraModules -contains $mod) { continue } # config/events 任何模块都可依赖
                if ($allowed -contains $mod) { continue }
                $key = "$rel|$lineNo|$mod"
                if ($seen.ContainsKey($key)) { continue }
                $seen[$key] = $true
                $violations.Add(('src-tauri/src/{0}:{1}  {2} → {3}（依赖方向违规：docs/01 §2.3.1 允许边表未包含该边）' -f $rel, $lineNo, $owner, $mod))
            }
            $i++
        }
    }

    Write-Host '[check-deps] 依赖方向检查（FF-1，阈值 0，白名单 fail-closed，依据 docs/01 §2.3.1）'
    Write-Host ("[check-deps] 已登记 {0} 个模块的允许边；扫描 {1} 个 Rust 源文件" -f $allow.Count, $files.Count)

    if ($violations.Count -gt 0) {
        Write-Host ''
        foreach ($v in $violations) { Write-Host "  违规  $v" -ForegroundColor Red }
        Write-Host ''
        Write-Host ("[check-deps] 失败：{0} 条违规（阈值 0）" -f $violations.Count) -ForegroundColor Red
        exit 1
    }

    Write-Host '[check-deps] 依赖方向检查通过（0 违规）' -ForegroundColor Green
    exit 0
}

try {
    Invoke-Check
}
catch {
    # 与"发现违规"严格区分：脚本自身故障是 3，代码有违规才是 1
    Write-Host "[check-deps] 脚本执行异常（非违规，请修脚本）：$($_.Exception.Message)" -ForegroundColor Yellow
    exit 3
}
