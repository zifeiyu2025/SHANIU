# Phase 1 中间数据（3 题 × 4 臂 × 1 轮）

生成时间：2026-11-15。原始数据见同目录 `phase1-*/summary.json` 与 `raw/`（ndjson 事件流 + 判据全文）。

## 1. 口径快照

| 项 | 值 |
|---|---|
| 模型 | `deepseek-flash`（四方同一模型） |
| 协议 | Responses（`https://api.deepseek.com/v1`）。**SHANIU 臂例外：走 `openai-chat`**，协议不同，只作能力近似，不可与其它臂并列 |
| SHANIU | 本仓库 HEAD `26f9ebc` + 评测侧改动（`src-tauri/tests/evals_repo.rs`），走 `openai-chat` |
| codex | `@openai/codex@0.169.0`（npm 全局预编译包） |
| dsh | `@deepseek-ai/dsh@0.2.2-rc.1`（npm 全局预编译包）；上游源码 `D:\AI\deepseek-harness` HEAD `39163dd3` 为 **0.2.2-rc.3**，比装的**高一个 rc** |
| zcode | 无 npm 发布包（`@zcode/cli` 为 `private`），从 `D:\AI\ZCode` 源码 `fec97590`（v3.18.2）构建：`pnpm install` + `pnpm run build:sea:prepare` → `dist/zcode.cjs`；CLI 0.17.1 |
| 工具链 | node `v26.13.1`、npm `11.20.0`、pnpm `10.33.2`（zcode 未发布包声明 engine 为 node 24.14.0，本机不符） |
| 题集 | `evals/repo-tasks.json` 复用题 3 道：`repo-doc-max-turns-drift`、`repo-bug-compaction-tail`、`repo-refactor-rename-title-api` |
| 工作区 | 四方 `git ls-files -s` 指纹逐题一致：`bd329ad2…` / `7a9df680…` / `8a1bd752…` |
| 判据 | 同一份 `evals/repo/judge/run.ps1`；在**无 ACL 副本**上执行（见发现 4） |
| 权限 | codex `-s workspace-write`；dsh `--permission-mode workspace-write --approval-policy never`；zcode `--mode yolo`；SHANIU `ApprovalMode/Auto`+工作区写 |

## 2. 结果

> **口径警告：** 下表 SHANIU 列的数字来自 `openai-chat`，其它三臂来自 Responses。两者协议不同，SHANIU 的数字只能当作「该 harness 能力上界近似」，**不能并列当作同口径结果**。在 SHANIU 的 Responses 缺陷修好、重跑 Responses 臂之前，任何「四方同口径」的解读都无效。

通过情况：**4 臂 × 3 题 = 12/12 全 PASS**，且反作弊守卫全部未命中（`tampered=false`），`forbid_paths` 全部未触碰。

| 题 | 臂 | 结果 | agent 墙钟 | 输入 token（含缓存） | 缓存命中 | 输出 token | 模型请求 | diff |
|---|---|---|---|---|---|---|---|---|
| bug-compaction-tail | SHANIU | PASS | 87.2s | 152,997 | 133,248 | 3,197 | 10 | +1/-1 1 文件 |
| bug-compaction-tail | codex | PASS | 191.0s | 244,019 | 221,568 | 1,869 | n/a | +1/-1 1 文件 |
| bug-compaction-tail | dsh | PASS | 196.4s | 1,774,858 | 1,724,800 | 28,854 | 33 | +1/-1 1 文件 +1 未跟踪 |
| bug-compaction-tail | zcode | PASS | 135.5s | 989,113 | 481,792 | 5,466 | 14 | +1/-1 1 文件 |
| refactor-rename-title-api | SHANIU | PASS | 79.5s | 273,365 | 256,000 | 4,020 | 16 | +8/-8 1 文件 |
| refactor-rename-title-api | codex | PASS | 271.9s | 459,126 | 437,632 | 5,042 | n/a | +8/-8 1 文件 |
| refactor-rename-title-api | dsh | PASS | 165.2s | 1,247,505 | 1,213,568 | 23,857 | 33 | +8/-8 1 文件 |
| refactor-rename-title-api | zcode | PASS | 106.6s | 426,646 | 203,136 | 2,070 | 7 | +8/-8 1 文件 |
| doc-max-turns-drift | SHANIU | PASS | 30.6s | 183,282 | 169,344 | 3,929 | 12 | +1/-1 1 文件 |
| doc-max-turns-drift | codex | PASS | 120.0s | 451,721 | 430,208 | 2,529 | n/a | +1/-1 1 文件 |
| doc-max-turns-drift | dsh | PASS | 35.8s | 272,339 | 255,360 | 4,156 | 11 | +1/-1 1 文件 |
| doc-max-turns-drift | zcode | PASS | 35.0s | 777,458 | 375,296 | 5,036 | 11 | +1/-1 1 文件 |

小计：

| 臂 | agent 墙钟合计 | 输入 token 合计 | 输出 token 合计 | 与最低者比（输入） |
|---|---|---|---|---|
| SHANIU | 197.3s | 609,644 | 11,146 | 1.0× |
| zcode | 277.1s | 2,193,217 | 12,572 | 3.6× |
| codex | 582.9s | 1,154,866 | 9,440 | 1.9× |
| dsh | 397.4s | 3,294,702 | 56,867 | 5.4× |

判据/编译耗时（`judge_ms`，含副本复制 ≈10s）不计入 agent 墙钟：SHANIU 1.3–84s，其余三方 73–95s。

## 3. 实测发现

**发现 1：这套题对 flash 档没有分辨力。**
四方全绿、diff 完全一致（同一批行号、同样的 +1/-1 与 +8/-8）。也就是说：用「通过率」比不出差距，用「改动行数」也比不出差距。当前唯一有区分度的是成本维度（token、墙钟、模型请求数）。原因大概率是这三题都是「一行改错/常量漂移/重命名」量级，flash 足够。

**发现 2：dsh 0.2.2-rc.1 在 Windows 上把自己的工作区变成自己不可访问（参考实现缺陷，不是模型能力差距）。**

取证：

- **ACE 内容**：dsh 给工作区根目录写了一条显式拒绝——`Everyone` 对 `DeleteSubdirectoriesAndFiles` 的 Deny；同时还有 dsh 自己的 `S-1-4-*` 授权项。dsh 内置的 `diagnose-windows-sandbox-acl` skill 正文把这两条都称为「expected（预期）」并明确「脚本永不删除 deny ACE」，也就是说它自己知道这条 ACE 存在。
- **对照**：同批次 codex 臂的工作区没有这条 `Everyone` Deny（按复制属性逐项比对）。
- **拒绝次数（只数 `tool_result` 且结果含 `is denied`，原始字符串出现次数会混入模型复述与 skill 正文，不采用）**：`bug-compaction-tail` 13 次、`refactor-rename-title-api` 6 次、`doc-max-turns-drift` 2 次。
- **受影响命令**：`Set-Location`、`Get-Item`、`Test-Path`、`Get-ChildItem`、`New-Item`（`glob` 因此返回空）；工具输出统一为 `Access to the path '…\ws\<task>' is denied`。
- **副作用**：该 Deny 同时禁止删除子项，导致 cargo 在原地写 `target` 时清旧产物失败（`os error 5`），这也是判据必须改到无 ACL 副本上跑的直接原因（发现 4）。

结论：这是 **dsh 参考实现的沙箱自伤**，不是模型能力差距。但它把 dsh 的 token 抬到另三方的 3–10 倍，并让「dsh 的模型能力」与「dsh 的沙箱缺陷」无法在同一组数字里分开。

**发现 3：SHANIU 的 Responses 路径会把用户消息丢掉（Phase 0 实测，本仓库产品缺陷）。**
同一题走 `openai-responses`：4 轮、0 文件改动、模型回复「你这条消息里没有具体任务内容」；改走 `openai-chat`：12 轮、正常修好、判据 PASS。根因在 `src-tauri/src/model_client/mod.rs` 的 `responses_input()`：`HistoryItem::Message` 直接 `serde_json::to_value` 成 `{"type":"message","role":"user","text":"…"}`，而 Responses 协议要的是 `content` 数组（`[{"type":"input_text","text":"…"}]`）。端点收下 200 但不解析，于是模型看不到任务。
→ 在「统一 Responses」口径下，SHANIU 目前**跑不了**。本文件的 SHANIU 数字全部来自 `openai-chat`，与另三方的协议不同，**只能作为「该 harness 能力上界的近似」，不能直接当 harness 差距**。

**发现 4：判据必须在无 ACL 副本上跑。**
原地判据（`CARGO_TARGET_DIR=<ws>/target`）在 dsh 臂上必然假红：构建脚本删 `target` 里产物报 `os error 5`。改成 `robocopy /MIR /COPY:DAT` 复制到无 ACL 副本判据后复绿（且 dsh 的改动确实是对的）。顺带挡住「预置构建产物骗判据」。

**发现 5：`turns` 指标跨臂不可比。**
codex `--json` 只在结束时发一次 `turn.completed`（内部工具循环不计数），所以它的「1 轮」不等于「1 次模型请求」；dsh 报 step_end（33），zcode 报 `modelRequestCount`（7–14），SHANIU 报轮数（10–16）。后续只能用 zcode/dsh/SHANIU 的「模型请求数」，codex 需另找字段或标 n/a。**报告保留这一条说明，不再尝试把各臂换算成统一口径。**

## 4. 分辨力初判

- 作为**能力**对照：这 3 题无效（全绿）。
- 作为**成本/效率**对照：有效，且差距很大（输入 token 5.4×、墙钟 3×）。
- 结论：需要换更硬的题（多文件、跨模块、需要读代码才能定位、或长上下文压缩类），否则 Phase 2 全量也会得到「四方 100%」的结论。已按此新增 2 题（见第 6 节）；Phase 2 是否真有分辨力，以这 2 题的四臂结果为准，本文件不预判。

## 5. 成本（已花）

输入合计 ≈ 7.25M token、输出 ≈ 90k token（四方各 3 次）。按 `deepseek-flash` 单价折算为个位数人民币量级；另加 zcode 源码构建（`pnpm install` 2m22s + 全包构建约 2m）。

## 6. 裁决落地（2026-11-15）

1. **`pro` 天花板校准取消。** 试点四方全绿，跑 pro 不产生信息。规则改为：**只在 flash 失败的题上跑一次 pro**，用来区分「模型不够」还是「harness 不行」。
2. **SHANIU 的 Responses 缺陷**（用户消息发成 `text` 而不是 `content` 数组）已交模型接入层修复。等对方给出提交号后重跑 Responses 臂；在此之前 SHANIU 的 chat 数字按第 2 节口径警告处理——协议不同，仅作能力近似，不与其它臂并列。
3. **题集加难度**：新增 2 题（10 → 12），按「跨 3+ 文件、不改就必红」设计，均已过红→绿验证才入集：

| 新题 | 形态 | 触及实现文件 | 红→绿证据 |
|---|---|---|---|
| `repo-bug-search-surface-lost` | 一个字段贯穿「投影 → 写索引 → 查询 → 输出」四层，任何一层丢失标注都会红；不给测试名，只给模块命令 | `session_query/documents.rs`、`session_query/mod.rs`、`session_query/query.rs` | 红：判据 FAIL（`documents_mark_compacted_originals_as_shadowed`、`searching_a_compacted_original_says_the_model_no_longer_sees_it`）；绿：参考修复后 `REPO-CHECK: PASS` |
| `repo-bug-cut-pairing` | 配对边界成对出现：切点优先回缩、配对起点在 0 时整对压掉；同时覆盖自动/手动两个入口、分叉安全切点、连续锚点序号 | `compaction/mod.rs`、`rollout/resume.rs`、`session_query/documents.rs` | 红：6 条夹具中 5 条 FAIL；绿：参考修复后 6/6 PASS，判据 `REPO-CHECK: PASS` |

判据未放宽：`search-surface-lost` 直接复用仓库既有断言（`whole_hash` 锁 `session_query/tests.rs`）；`cut-pairing` 新增 6 条夹具断言，守卫锁夹具文件、`compaction/mod.rs` 测试区、`resume.rs` 测试区与 `session_query/tests.rs`。两题题面都写明：改动测试代码、守卫区（`guards.json` 覆盖的 hash 区）或判据脚本，一律该题判 `invalid`。

原候选 3 题（`repo-feat-export-trigger-trace` 等）不再按原样入集：它们仍偏「单点改动」，与新裁决要求的方向不符，不用来凑数。Phase 2 的题集按 12 题计（10 复用 + 2 新增）。
