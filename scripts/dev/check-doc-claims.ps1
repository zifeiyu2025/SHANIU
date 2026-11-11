<#
.SYNOPSIS
  重跑「文档断言 ↔ 代码」反向核查：`align/doc-claims.md` 里每条可判定断言在当前树上是否兑现。

.DESCRIPTION
  为什么要重跑：这套核查原先只跑过一次，脚本在 %TEMP%，清单在 gitignore 的 scratch/。
  文档一改就没人再核，承诺与实现的偏差会静默累积。本脚本把那次一次性动作变成可重跑的门禁式检查。

  为什么断言集是文件而不是现抽：从散文里「抽可判定断言」本身需要人工判断，机器抽不出来。
  本脚本重跑的是**已登记**的断言集；文档新增承诺要人工追加一行（ID 续号），否则不在覆盖范围内。
  覆盖边界只到此为止，别把「绿」读成「docs 与代码完全一致」。

  判定口径（与 `src-tauri/tests/doc_code_collision.rs` 同精神，多一层类型分派）：
    符号 = 至少一处调用点（排除注释行、排除纯定义行）；
    文件 = 路径存在（找不到再按文件名全仓 glob，容忍目录搬家）；
    命令 = 命令名（蛇形）至少一处调用点，`generate_handler!` 里那处算调用点；
    字面量 = 源码里按原样字符串出现（`user-terminal://frame` 这类）；
    测试 = 用例名存在（`#[test]` 天然无调用点，不按调用点判）；
    结构 = 脚本内命名检查（缺席断言、`format!` 模板这类按 token 判不了的）。

  已补的三类盲区（这轮实测到的、能机械处理的）：
    1) serde `rename_all = "camelCase"`：驼峰名在源码零字面量时回退查蛇形字段，
       且要求字段所在文件确实带 camelCase 重命名——`configRoot` 不再假 A；
    2) wire 蛇形名 ↔ 驼峰枚举变体：`allowlist_hit` ↔ `AllowlistHit` 视作同一承诺；
    3) `#[test]` 函数只有定义：命中行上方 3 行内有 `#[test]`/`#[tokio::test]` 即算兑现。

  不补的三类（写在这里当**已知边界**，别拿绿当「都验过」）：
    a) 工具名/窗口类名/事件名以 `&str` 常量存在——字符串命中不等于被注册/emit 消费。
       脚本只在「命中全在字符串/注释、且既无定义也无调用」时判 B，不判 pass 也不判 A；
    b) basename 被当符号（`sandbox_probe`、`mcp_sandbox` 是文件名，不是标识符）。
       `类型=文件` 的行已按 basename 处理（文件或目录同名即算），其它类型里出现裸名字仍不猜；
    c) 正则与通配（`^[a-z0-9][a-z0-9-]{0,31}$`、`MAX_FILE_*`）只能人工核。
  补它们需要单独一张卡：比 3 行启发式贵得多，且容易变成「猜」。

  退出码（与 check-align-ledger.ps1 同一套契约）：
    0 = 无 A 类缺口；
    1 = 有 A 类缺口（文档承诺了、代码里找不到）；
    2 = 环境不可比（清单缺失 / 解析出 0 行 / 源码树为空 / 断言的文档文件不存在 /
        清单引用了脚本没有的结构检查），此时**不下结论**——不可比就不给绿。

  本脚本与 CI 的关系：只在本地跑，不替代 `cargo test`。它查的是「docs 的符号级承诺还在不在」，
  查不了「实现是否正确」，也查不了测试有没有分辨力（见清单的 B 类）。

.PARAMETER Claims
  断言清单。默认 `align/doc-claims.md`。

.PARAMETER ShowPass
  逐条打印通过项及判定依据（`call` / `serde-camel` / `wire-variant` / `rust-test` / `path` /
  `literal` / `struct`），排查「为什么这条算过」时用。

.EXAMPLE
  pwsh scripts/dev/check-doc-claims.ps1

.EXAMPLE
  pwsh scripts/dev/check-doc-claims.ps1 -ShowPass
#>
param(
    [string]$Claims = "",
    [switch]$ShowPass
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if (-not $Claims) { $Claims = Join-Path $repoRoot "align\doc-claims.md" }

$logPrefix = "[doc-claims]"
if (-not (Test-Path $Claims)) {
    Write-Host "$logPrefix 找不到断言清单：$Claims" -ForegroundColor Yellow
    exit 2
}

# ============================== 源码树 ==============================
# 为什么扫这些根：docs 的承诺落在这几处；`evals/` 也算（产物路径形态由 evals 侧决定）。
# `src/dsh/**` 一并扫：vendored 前端也是 docs 引用对象，但只作为「存在」证据，不当实现证据。
$exts = @(".rs", ".ts", ".tsx", ".css", ".json", ".ps1", ".mjs", ".js", ".yml", ".yaml", ".toml", ".html")
$scanRoots = @("src", "src-tauri", "scripts", ".github", "resources", "evals") |
    ForEach-Object { Join-Path $repoRoot $_ } |
    Where-Object { Test-Path $_ }
if (-not $scanRoots) {
    Write-Host "$logPrefix 找不到任何源码根（工作目录不对？）：$repoRoot" -ForegroundColor Yellow
    exit 2
}

$files = Get-ChildItem -Path $scanRoots -Recurse -File -ErrorAction SilentlyContinue |
    Where-Object {
        $exts -contains $_.Extension -and
        $_.FullName -notmatch '\\(node_modules|target|dist|dist-portable|dist-release|\.git)\\' -and
        # 排除自身：脚本头注释里举的 `configRoot` 例子会把「零命中」变成「命中」，自己给自己发绿。
        $_.FullName -ne $PSCommandPath
    }
$script:cache = @{}
foreach ($f in $files) {
    try { $script:cache[$f.FullName] = [IO.File]::ReadAllText($f.FullName) } catch {}
}
if ($script:cache.Count -lt 50) {
    Write-Host "$logPrefix 源码树只读到 $($script:cache.Count) 个文件，环境不可比" -ForegroundColor Yellow
    exit 2
}

function Format-Path([string]$full) {
    if ($full.StartsWith($repoRoot)) { return $full.Substring($repoRoot.Length).TrimStart("\") }
    return $full
}

# ============================== 行级分类 ==============================
# 一次扫描同时给出：总命中 / 注释 / 字符串 / 定义 / 调用 / 测试定义。
# 测试定义单独计数：`#[test] fn` 天然没有调用点，混进「定义」就会被判成缺口。
function Get-CodeStats([string]$key) {
    $s = [pscustomobject]@{
        files = 0; total = 0; comment = 0; def = 0; string = 0; call = 0; testDef = 0; onlyTestFiles = $true
    }
    # 定义行必须是「关键字 + 名字紧邻」。早先写成 `^\s*const\b[^\n]*?NAME`，会把
    # `const key = PRESET_DESCRIPTION_KEYS.get(v)` 这类**使用了**常量的行也算成定义，
    # 于是一个被正常调用的常量反被判成「无调用点」——纯误报。
    $defRe = "(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:export\s+)?(?:default\s+)?(?:async\s+)?(?:unsafe\s+)?(?:fn|struct|enum|trait|const|static|type|mod|class|function|interface)\s+(?:mut\s+)?" +
        [regex]::Escape($key) + "\b"
    foreach ($kv in $script:cache.GetEnumerator()) {
        $text = $kv.Value
        if (-not $text.Contains($key)) { continue }
        $s.files++
        if ($kv.Key -notmatch '\\tests?\\|\\__tests__\\|\.spec\.|\.test\.') { $s.onlyTestFiles = $false }
        $lines = $text -split "`n"
        for ($i = 0; $i -lt $lines.Count; $i++) {
            $ln = $lines[$i]
            $idx = $ln.IndexOf($key)
            if ($idx -lt 0) { continue }
            $s.total++
            $t = $ln.TrimStart()
            $before = $ln.Substring(0, $idx)
            $quotes = ([regex]::Matches($before, '"').Count + [regex]::Matches($before, "'").Count)
            if ($t.StartsWith("//") -or $t.StartsWith("*") -or $t.StartsWith("/*") -or
                $t.StartsWith("#") -or $t.StartsWith("<!--")) {
                $s.comment++
            }
            elseif (($quotes % 2) -eq 1) {
                $s.string++
            }
            elseif ($ln -match $defRe) {
                $s.def++
                for ($j = $i - 1; $j -ge [Math]::Max(0, $i - 3); $j--) {
                    if ($lines[$j] -match '^\s*#\[(tokio::)?test\b') { $s.testDef++; break }
                }
            }
            else {
                $s.call++
            }
        }
    }
    return $s
}

function ConvertTo-Snake([string]$s) {
    return [regex]::Replace($s, "(?<=[a-z0-9])([A-Z])", "_`$1").ToLowerInvariant()
}

function ConvertTo-Pascal([string]$s) {
    $parts = $s -split "_" | Where-Object { $_ }
    return ($parts | ForEach-Object { $_.Substring(0, 1).ToUpperInvariant() + $_.Substring(1) }) -join ""
}

# 盲区 1 的旁证：蛇形字段所在文件必须真的声明了 camelCase 重命名，否则「同名蛇形字段」
# 可能只是巧合（两个不相干的字段名撞上了）。
function Test-CamelCaseHost([string]$snake) {
    foreach ($kv in $script:cache.GetEnumerator()) {
        if (-not $kv.Value.Contains($snake)) { continue }
        if ($kv.Value -match 'rename_all\s*=\s*"camelCase"') { return $true }
    }
    return $false
}

function Find-Fragment([string]$fragment) {
    $hits = @()
    foreach ($kv in $script:cache.GetEnumerator()) {
        if ($kv.Value.Contains($fragment)) { $hits += (Format-Path $kv.Key) }
    }
    return $hits
}

# ============================== 结构检查 ==============================
# 为什么单独实现：缺席断言（「不设 default」）与 `format!` 模板（`mcp__{server}__{tool}`）
# 都不是「某个符号存在」，硬塞进符号判定只会得到假绿或假红。每个检查 5-10 行，改命名规则时会红。
function Invoke-StructuralCheck([string]$name) {
    switch ($name) {
        "cfg:custom-protocol-no-default" {
            $toml = Join-Path $repoRoot "src-tauri\Cargo.toml"
            if (-not (Test-Path $toml)) { return @{ ok = $false; detail = "src-tauri/Cargo.toml 不存在" } }
            $text = [IO.File]::ReadAllText($toml)
            if ($text -notmatch '(?m)^\s*custom-protocol\s*=\s*\[\s*"tauri/custom-protocol"\s*\]') {
                return @{ ok = $false; detail = "[features] 里没有 custom-protocol = [`"tauri/custom-protocol`"]" }
            }
            # 缺席断言：出现 `default = [` 就是默认开启，cargo check 会强依赖 dist/ 存在。
            if ($text -match '(?m)^\s*default\s*=') {
                return @{ ok = $false; detail = "[features] 出现 default = ...（默认开启后 cargo check 要求 dist/ 已存在）" }
            }
            return @{ ok = $true; detail = "custom-protocol 存在且无 default" }
        }
        "fmt:mcp-dispatch-name" {
            # 命名规则由「常量前缀 + format! 拼接」两段组成，单查任一段都抓不住改规则。
            $prefix = Find-Fragment 'const DISPATCH_PREFIX: &str = "mcp__"'
            $tpl = Find-Fragment 'format!("{DISPATCH_PREFIX}{server_id}__")'
            if (-not $prefix) { return @{ ok = $false; detail = "找不到 DISPATCH_PREFIX = `"mcp__`" 常量" } }
            if (-not $tpl) { return @{ ok = $false; detail = "找不到 format!(`"{DISPATCH_PREFIX}{server_id}__`") 模板" } }
            return @{ ok = $true; detail = "$($tpl[0])" }
        }
        "fmt:mcp-container-profile" {
            $hit = Find-Fragment 'pub const MCP_PROFILE_PREFIX: &str = "shaniu.mcp."'
            if (-not $hit) { return @{ ok = $false; detail = "找不到 MCP_PROFILE_PREFIX = `"shaniu.mcp.`"" } }
            return @{ ok = $true; detail = "$($hit[0])" }
        }
        "fmt:mcp-result-envelope" {
            $hit = Find-Fragment '<<<MCP_RESULT:{nonce}'
            if (-not $hit) { return @{ ok = $false; detail = "找不到 `<<<MCP_RESULT:{nonce}` 包裹模板" } }
            return @{ ok = $true; detail = "$($hit[0])" }
        }
        "fmt:eval-result-filename" {
            $hit = Find-Fragment 'format!("{}-{}-{role}.json"'
            if (-not $hit) { return @{ ok = $false; detail = "找不到 `format!(`"{}-{}-{role}.json`")` 模板" } }
            return @{ ok = $true; detail = "$($hit[0])" }
        }
        default {
            return @{ ok = $false; detail = "清单引用了脚本没有的结构检查：$name"; unknown = $true }
        }
    }
}

# ============================== 清单解析 ==============================
# 表格单元格里允许出现转义竖线（`a` \| `b`），按「未转义的 | 」切。
# 只认「ID + 两位文档号:行号」开头的行：文末的 B 类表格也是 `| R2-xxx |` 开头，不能算断言。
$rows = Get-Content $Claims -Encoding UTF8 | Where-Object { $_ -match '^\| R2-\d{3} \| \d{2}:L\d' }
if (-not $rows) {
    Write-Host "$logPrefix 清单里解析出 0 条断言：$Claims（格式变了？）" -ForegroundColor Yellow
    exit 2
}

$results = New-Object System.Collections.Generic.List[object]
foreach ($row in $rows) {
    $cells = @($row -split '(?<!\\)\|' | ForEach-Object { $_.Trim() })
    $id = $cells[1]
    $pos = $cells[2]
    # 6 列表（ID/位置/原句/期望/类型/备注）；原表里夹带文档原始表格列的行多一格，原句并回一列。
    if ($cells.Count -ge 9) {
        $quote = "$($cells[3]) | $($cells[4])"
        $expect = $cells[5]; $kind = $cells[6]; $note = $cells[7]
    }
    else {
        $quote = $cells[3]; $expect = $cells[4]; $kind = $cells[5]; $note = $cells[6]
    }
    $results.Add([pscustomobject]@{
            id = $id; pos = $pos; quote = $quote; expect = $expect; kind = $kind; note = $note
        })
}

function Get-DocFile([string]$pos) {
    if ($pos -notmatch '^(\d{2}):') { return $null }
    $num = $Matches[1]
    return Get-ChildItem -Path (Join-Path $repoRoot "docs") -Filter "$num-*.md" -File -ErrorAction SilentlyContinue |
        Select-Object -First 1
}

# ============================== 逐条判定 ==============================
$verdicts = New-Object System.Collections.Generic.List[object]
$envError = $null

foreach ($r in $results) {
    # B 类：清单显式标注「不判」的，直接跳过。
    if ($r.kind -eq "—" -or $r.kind -eq "跳过" -or $r.expect -match '^—$' -or $r.note -match '不进 A|文档自称') {
        $verdicts.Add([pscustomobject]@{ id = $r.id; pos = $r.pos; key = $r.expect; kind = $r.kind; verdict = "B"; reason = "清单标注不判" })
        continue
    }

    $docFile = Get-DocFile $r.pos
    if ($null -eq $docFile) {
        $envError = "断言 $($r.id) 的文档位置无法解析：$($r.pos)"
        break
    }

    # 结构检查：期望列里带 `cfg:` / `fmt:` 前缀。
    if ($r.kind -eq "结构" -or $r.expect -match '`(cfg|fmt):') {
        $m = [regex]::Match($r.expect, '`((?:cfg|fmt):[A-Za-z0-9-]+)`')
        if (-not $m.Success) {
            $verdicts.Add([pscustomobject]@{ id = $r.id; pos = $r.pos; key = $r.expect; kind = $r.kind; verdict = "A"; reason = "结构检查名缺失" })
            continue
        }
        $check = Invoke-StructuralCheck $m.Groups[1].Value
        if ($check.unknown) { $envError = "断言 $($r.id)：$($check.detail)"; break }
        $verdicts.Add([pscustomobject]@{
                id = $r.id; pos = $r.pos; key = $m.Groups[1].Value; kind = "结构"
                verdict = $(if ($check.ok) { "pass" } else { "A" })
                reason = $(if ($check.ok) { "struct: $($check.detail)" } else { $check.detail })
            })
        continue
    }

    # 期望列里的反引号 token：普通行按形状分派，字面量行原样，命令行只认蛇形命令名。
    $tokens = @([regex]::Matches($r.expect, '`([^`]+)`') | ForEach-Object { $_.Groups[1].Value })
    if ($r.kind -eq "命令") {
        $tokens += @([regex]::Matches($r.quote, '`([^`]+)`') | ForEach-Object { $_.Groups[1].Value })
    }
    $tokens = @($tokens | Where-Object { $_ -and $_ -notmatch '^\s*$' } | Select-Object -Unique)
    if (-not $tokens) { $tokens = @($r.expect) }

    foreach ($tok in $tokens) {
        $isPath = $false
        $isBasename = $false
        $isLiteral = ($r.kind -eq "字面量")
        if ($r.kind -eq "命令") {
            # 命令名常写成 `foo(a, b?)`：参数不是判定对象，只认蛇形命令名。
            $bare = $tok -replace '\(.*$', ''
            $keys = @((($bare -split '[^A-Za-z0-9_]+') | Where-Object { $_ -match '^[a-z][a-z0-9_]{3,}$' }))
        }
        elseif ($isLiteral) {
            $keys = @($tok)
        }
        elseif ($r.kind -eq "配置" -and $tok -match '^[0-9][0-9A-Za-z._-]*$') {
            # 版本号这类纯数字 token 不是标识符，按原样字符串找（`0.2.4` 三处一致）
            $isLiteral = $true; $keys = @($tok)
        }
        elseif ($tok -match '/' -or $tok -match '^[A-Za-z0-9_./-]+\.(rs|ts|tsx|ps1|json|toml|yml|yaml|css|html|md|mjs)$') {
            $isPath = $true; $keys = @($tok)
        }
        elseif ($tok -match '[#!(=]') {
            # 代码片段（`#![cfg(windows)]`、`format!(...)`、`KEY = ...`）：拆开只会得到 `cfg`/`windows`
            # 这种到处都有的词，反而查不出问题。按原样字符串找。
            $isLiteral = $true; $keys = @($tok)
        }
        elseif ($tok -match '^[A-Za-z_][A-Za-z0-9_]*$') {
            # 类型=文件 却只给了不带扩展名的名字：按 basename 找（`sandbox_failclosed` → `*.rs` 里那个）
            if ($r.kind -eq "文件") { $isBasename = $true }
            $keys = @($tok)
        }
        else {
            $keys = @($tok -split '[^A-Za-z0-9_]+' | Where-Object { $_ -match '^[A-Za-z_][A-Za-z0-9_]{2,}$' })
            if ($r.kind -eq "命令") { $keys = @($keys | Where-Object { $_ -match '^[a-z][a-z0-9_]{3,}$' }) }
            # 类型=文件 的行常常给的是「一句话里列了几个文件名」，拆出来的裸名字同样按 basename 找。
            if ($r.kind -eq "文件") { $isBasename = $true }
        }
        if (-not $keys) { continue }

        foreach ($k in $keys) {
            if ($isBasename) {
                # 用已读入的路径表判，不再递归 Get-ChildItem：文件 `x.rs` 与目录 `x/` 都算命中。
                $pat = '\\' + [regex]::Escape($k) + '(\\|\.[^\\]+$)'
                $hitFile = @($script:cache.Keys | Where-Object { $_ -match $pat } | Select-Object -First 1)
                $verdicts.Add([pscustomobject]@{
                        id = $r.id; pos = $r.pos; key = $k; kind = "文件"
                        verdict = $(if ($hitFile) { "pass" } else { "A" })
                        reason = $(if ($hitFile) { "basename: $(Format-Path $hitFile[0])" } else { "同名文件/目录不存在（按 basename 回退）" })
                    })
                continue
            }
            if ($isPath) {
                $p = $k -replace '^\*+', '' -replace '\*+$', '' -replace '/$', ''
                $hit = Test-Path (Join-Path $repoRoot $p)
                if (-not $hit -and $p -match '/') {
                    $hit = Test-Path (Join-Path $repoRoot ($p -split '/' -join '\'))
                }
                if (-not $hit) {
                    $leaf = Split-Path $p -Leaf
                    $hit = [bool](Get-ChildItem -Path $repoRoot -Recurse -File -Filter $leaf -ErrorAction SilentlyContinue |
                        Where-Object { $_.FullName -notmatch '\\(node_modules|target|dist|\.git)\\' } |
                        Select-Object -First 1)
                }
                $verdicts.Add([pscustomobject]@{
                        id = $r.id; pos = $r.pos; key = $k; kind = "文件"
                        verdict = $(if ($hit) { "pass" } else { "A" })
                        reason = $(if ($hit) { "path" } else { "路径不存在（含按文件名回退）" })
                    })
                continue
            }

            if ($isLiteral) {
                $stats = Get-CodeStats $k
                $verdicts.Add([pscustomobject]@{
                        id = $r.id; pos = $r.pos; key = $k; kind = "字面量"
                        verdict = $(if ($stats.total -gt 0) { "pass" } else { "A" })
                        reason = $(if ($stats.total -gt 0) { "literal: $($stats.files) 个文件" } else { "字面量零命中" })
                    })
                continue
            }

            $stats = Get-CodeStats $k
            if ($stats.call -gt 0) {
                $verdicts.Add([pscustomobject]@{ id = $r.id; pos = $r.pos; key = $k; kind = $r.kind; verdict = "pass"; reason = "call" })
                continue
            }
            if ($stats.testDef -gt 0) {
                $verdicts.Add([pscustomobject]@{ id = $r.id; pos = $r.pos; key = $k; kind = $r.kind; verdict = "pass"; reason = "rust-test" })
                continue
            }

            # 盲区 1：驼峰名由 serde 生成。
            if ($k -cmatch '[a-z][A-Z]') {
                $snake = ConvertTo-Snake $k
                if ($snake -ne $k) {
                    $snakeStats = Get-CodeStats $snake
                    if ($snakeStats.total -gt 0 -and (Test-CamelCaseHost $snake)) {
                        $verdicts.Add([pscustomobject]@{ id = $r.id; pos = $r.pos; key = $k; kind = $r.kind; verdict = "pass"; reason = "serde-camel -> $snake" })
                        continue
                    }
                }
            }

            # 盲区 2：wire 蛇形名 ↔ 驼峰枚举变体。
            if ($k -cmatch '^[a-z][a-z0-9]*(_[a-z0-9]+)+$') {
                $pascal = ConvertTo-Pascal $k
                $pascalStats = Get-CodeStats $pascal
                if ($pascalStats.call -gt 0) {
                    $verdicts.Add([pscustomobject]@{ id = $r.id; pos = $r.pos; key = $k; kind = $r.kind; verdict = "pass"; reason = "wire-variant -> $pascal" })
                    continue
                }
            }

            # 盲区 4/5：只在字符串或注释里出现。「被当常量消费」与「死代码」在 AST 之外分不开，
            # 判 pass 是假绿、判 A 是假红，唯一诚实的格是 B（B ≠ 已验证）。
            if ($stats.def -eq 0 -and $stats.call -eq 0 -and $stats.total -gt 0) {
                $verdicts.Add([pscustomobject]@{
                        id = $r.id; pos = $r.pos; key = $k; kind = $r.kind; verdict = "B"
                        reason = "只以字符串/注释出现（str=$($stats.string) cmt=$($stats.comment)），盲区 4/5"
                    })
                continue
            }

            $reason = if ($stats.total -eq 0) {
                "零命中"
            }
            else {
                "无调用点（def=$($stats.def) cmt=$($stats.comment) str=$($stats.string) total=$($stats.total)）"
            }
            $verdicts.Add([pscustomobject]@{ id = $r.id; pos = $r.pos; key = $k; kind = $r.kind; verdict = "A"; reason = $reason })
        }
    }
}

if ($envError) {
    Write-Host "$logPrefix 环境不可比：$envError" -ForegroundColor Yellow
    exit 2
}

# 一条断言都没产出判定 = 那一行没被读到（列数不对 / 表格格式变了）。这类静默漏判必须变红，
# 否则「通过数」只是少算了分母，看着绿实际没查。
$judged = @{}
foreach ($v in $verdicts) { $judged[$v.id] = 1 }
$unjudged = @($results | Where-Object { -not $judged.ContainsKey($_.id) })
if ($unjudged.Count -gt 0) {
    $names = ($unjudged | ForEach-Object { "$($_.id)（$($_.pos)）" }) -join "、"
    Write-Host "$logPrefix 有 $($unjudged.Count) 条断言没产生判定，清单格式可能变了：$names" -ForegroundColor Yellow
    exit 2
}

# ============================== 输出 ==============================
$pass = @($verdicts | Where-Object { $_.verdict -eq "pass" })
$gaps = @($verdicts | Where-Object { $_.verdict -eq "A" })
$bClass = @($verdicts | Where-Object { $_.verdict -eq "B" })

Write-Host "$logPrefix 清单：$(Format-Path $Claims)（$($results.Count) 行断言）"
Write-Host "$logPrefix 源码树：$($script:cache.Count) 个文件"
Write-Host "$logPrefix 通过 $($pass.Count)（判定点）/ A 类缺口 $($gaps.Count) / B 类不可判 $($bClass.Count)"

if ($ShowPass) {
    Write-Host ""
    Write-Host "== 通过（判定依据）=="
    $pass | ForEach-Object { "PASS`t{0}`t{1}`t{2}`t{3}" -f $_.id, $_.kind, $_.key, $_.reason }
}

if ($bClass.Count -gt 0) {
    Write-Host ""
    Write-Host "== B 类：不可机械判定（B ≠ 已验证）=="
    $bClass | ForEach-Object { "B`t{0}`t{1}`t{2}`t{3}" -f $_.id, $_.pos, $_.key, $_.reason }
}

if ($gaps.Count -gt 0) {
    Write-Host ""
    Write-Host "== A 类：文档有、代码找不到 =="
    $gaps | ForEach-Object { "A`t{0}`t{1}`t{2}`t{3}" -f $_.id, $_.pos, $_.key, $_.reason }
    Write-Host ""
    Write-Host "$logPrefix 有 A 类缺口：要么改代码兑现，要么按裁决改文档并同步本清单" -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "$logPrefix 无 A 类缺口（B 类仍需人工/运行期核验）" -ForegroundColor Green
exit 0
