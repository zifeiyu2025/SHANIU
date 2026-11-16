# Phase 2 四臂对照报告（deepseek-flash · 12 题）

> 数据目录：`evals/results-compare/phase2-{codex,dsh,zcode,shaniu}/`
> 统一快照：`0bee58e`；统一判据：`evals/repo/judge/run.ps1`；统一权限：workspace-write + 不询问。
> 报告生成于 2026-11-16，覆盖 codex 续跑、dsh 续跑、zcode 补跑之后的最终状态。

## 0. 结论先行

1. **codex 12/12 PASS，dsh 12/12 PASS，zcode 11 PASS + 1 TIMEOUT。**
2. **没有出现「flash 也过不去的题」**——两道新增难题（`search-surface-lost`、`cut-pairing`）codex 与 dsh 都一次过，zcode 只在 `cut-pairing` 上时间打满。**不需要动用 pro**。
3. **SHANIU 9 PASS / 3 FAIL**，三个 FAIL 全部是**模型请求收到 HTTP 400（BadRequest）后 agent loop 以 `Failed` 收口**，在 400 之前零文件改动。这是 harness 协议层缺陷，不是模型能力差距；按三分法归入「零改动收工」，但归因必须写明。
4. 本轮暴露三个**环境/参考实现层故障**（codex 磁盘满、dsh 外部清理删工作区、zcode 超时），均已取证并在下方单列，不与能力差距混算。

## 1. 口径与可比性

| 维度 | 口径 |
|---|---|
| 模型 | `deepseek-flash`（全臂一致） |
| 协议 | Responses：SHANIU `openai-responses`、codex `wire_api=responses`、dsh `openai-responses`、zcode `api.type=openai-responses` |
| 题集 | 12 题（10 复用 + 2 新增），均过「红→绿」验证 |
| 轮次 | 各 1 轮，顺序执行不并发 |
| 前置红 | 12/12 `precondition_red=true`（准备工作区先跑判据必须红） |
| 工作区指纹 | 12/12 与 `phase2-prep/summary.json` 一致（`--expect-tree` 硬闸门） |

**不可比项（不换算）**

- `turns`：codex 取 `turn.completed`（整轮恒为 1）、dsh 取 `status/step_end`、zcode 取 `model_request_completed`、SHANIU 取 `usage_turns`。四者语义不同，**不折算成统一口径**，只用于各臂内部判断「是否打满」。
- **SHANIU 墙钟与其它臂不可比**：它是 Responses 协议、且与 codex 臂**并发**跑，CPU 与端点配额都被争抢；同时本轮它还遭遇 4 次 400 中断。
- input token 是 `input_total`（含缓存命中），各臂缓存命中率不同，只作量级参考。

## 2. 通过率

| 臂 | PASS | FAIL | TIMEOUT | INVALID | 备注 |
|---|---|---|---|---|---|
| codex | 12 | 0 | 0 | 0 | 7 题续跑复用 + 5 题实跑 |
| dsh | 12 | 0 | 0 | 0 | 7 题续跑复用 + 5 题实跑 |
| zcode | 11 | 0 | 1 | 0 | `cut-pairing` 两次 960s 硬超时 |
| SHANIU | 9 | 3 | 0 | 0 | 3 个 FAIL 均为零改动、由 400 中断 |

- codex 总数不含其首轮因磁盘满产生的假红：`refactor-now-ms-exit` 首轮判据红是 `os error 112`，续跑时用旧工作区**只重判**（不重跑 agent）得 `REPO-CHECK: PASS`。原日志 `raw/codex-repo-refactor-now-ms-exit-r1.judge.log` 与重判日志 `.resume.judge.log` 都保留。
- zcode 的 `cut-pairing`：首轮 960s 被杀后判据报「守卫目标缺失」，属不可归因；归档首轮文件后重跑，仍在 960s 处被杀（`turns=21`、`diff=0`），**两次一致，判 TIMEOUT**。

## 3. SHANIU 三分法（单列，不与通过率混算）

| 分类 | 数量 | 题 |
|---|---|---|
| 真失败（有改动但判据红） | 0 | — |
| 零改动收工 | 3 | `repo-bug-retry-after`、`repo-bug-search-surface-lost`、`repo-doc-claim-after-rename` |
| 轮数打满但通过 | 4 | `repo-feat-context-overflow-code`(32/32)、`repo-feat-title-cap`(24/24)、`repo-bug-cut-pairing`(32/32)、`repo-doc-max-turns-drift`(12/12) |
| 正常通过（未打满） | 5 | `apply-patch-read-guard`、`rollout-damaged`、`compaction-tail`、`refactor-now-ms-exit`、`refactor-rename-title-api` |

### 3.1 三个零改动收工的归因（事件流层面）

结论：**不是「模型没发起工具调用」，不是「工具调用被拒」，也不是「轮次没用完自行收工」**，而是**下一次模型请求收到 HTTP 400，agent loop 以 `reason=Failed` 收口**。

证据（`evals/results-compare/phase2-shaniu/raw/shaniu-run.log`，逐行可查）：

| 题 | 证据行 | 内容 |
|---|---|---|
| `repo-bug-retry-after` | 12 | `[req] task=repo-bug-retry-after-r1 seq=4 end err ms=164 kind=BadRequest code=BAD_REQUEST` |
| 同上 | 13 | `[agent] end task=repo-bug-retry-after ms=4336 turns=Some(4) reason=Failed` |
| `repo-bug-search-surface-lost` | 402 | `[req] task=repo-bug-search-surface-lost-r1 seq=5 end err ms=129 kind=BadRequest code=BAD_REQUEST` |
| 同上 | 403 | `[agent] end ... ms=6766 turns=Some(5) reason=Failed` |
| `repo-doc-claim-after-rename` | 550 | `[req] task=repo-doc-claim-after-rename-r1 seq=5 end err ms=141 kind=BadRequest code=BAD_REQUEST` |
| 同上 | 551 | `[agent] end ... ms=38156 turns=Some(5) reason=Failed` |

补充事实：

- 全轮共 **4 次** `BadRequest`，第 4 次落在 `repo-bug-apply-patch-read-guard` 的 `seq=12`（`shaniu-run.log:70`），但那题在被 400 打断前已改完文件，判据 PASS。
- 三题的 `agent_ms` 分别只有 4336ms / 6766ms / 38156ms，`diff=0`；而 400 的响应时间只有 129~164ms，说明是**服务端立即拒绝**，不是超时。
- 触发点都在第 4~5 次请求，即**历史里刚出现工具调用/工具结果之后**的回合：`68d4528` 修的是用户消息形状（`text` → `content[].input_text`），工具结果与 `function_call` 条目的形状仍是嫌疑点，需要接入层继续排查。
- 结论对报告的影响：**这 3 个 FAIL 不应计入「harness 不行」或「模型不行」的能力结论**，它们是同一 harness 缺陷的三个实例；SHANIU 的有效能力样本目前只有 9 题。

## 4. 逐题明细

### 4.1 agent 段墙钟（秒）与 token（in/out，千）

格式：`agent 墙钟 / input_total / output`。墙钟只取 `agent_ms`（不含判据），判据耗时另计（`judge_ms`，各臂差异小，未列表）。

| 题 | codex | dsh | zcode | SHANIU |
|---|---|---|---|---|
| repo-bug-retry-after | - / 231/2 | - / 2366/29 | 96 / 639/4 | 4 / 20/0.5 |
| repo-bug-apply-patch-read-guard | - / 577/6 | - / 3450/45 | 131 / 1376/8 | 21 / 175/2 |
| repo-bug-rollout-damaged | - / 375/3 | - / 2003/25 | 145 / 755/6 | 90 / 439/3 |
| repo-bug-compaction-tail | - / 569/3 | - / 2634/32 | 80 / 516/3 | 104 / 326/4 |
| repo-feat-context-overflow-code | - / 1092/13 | - / 1880/26 | 287 / 3409/23 | 83 / 1207/13 |
| repo-feat-title-cap | - / 966/6 | - / 1328/22 | 259 / 2751/14 | 168 / 398/5 |
| repo-refactor-now-ms-exit | - / 677/8 | - / 2367/26 | 156 / 1065/8 | 108 / 321/4 |
| repo-refactor-rename-title-api | 264 / 500/9 | 235 / 2196/21 | 93 / 698/3 | 101 / 178/3 |
| repo-bug-search-surface-lost | 464 / 1332/10 | 262 / 4289/37 | 537 / 7686/52 | 7 / 22/0.5 |
| repo-bug-cut-pairing | 713 / 2578/25 | 234 / 2957/39 | 960(硬超时) / N/A | 233 / 1603/19 |
| repo-doc-max-turns-drift | 131 / 188/2 | 38 / 412/5 | 59 / 1027/6 | 24 / 174/3 |
| repo-doc-claim-after-rename | 250 / 391/7 | 223 / 2596/32 | 93 / 776/4 | 38 / 58/0.7 |
| **合计 in/out** | 9.48M / 94k | 28.48M / 339k | 20.70M / 130k | 4.92M / 59k |
| **已测中位 agent 墙钟** | 264s（5 题） | 234s（5 题） | 138s（12 题） | 96s（12 题，**不可比**） |

`-` = 续跑复用，原始 agent 墙钟未落盘（见第 7 节）。zcode 的 `cut-pairing` token 未知：进程被硬杀，`result.usage` 事件没写出来，`summary.json` 里是 0 而非真实值，按规矩记 N/A 不估算。

补充：zcode 的 12 题 agent 墙钟中位 138s，是四臂里最快的；但它唯一的 TIMEOUT 也在这一臂，说明它是「普遍快、单题撞墙」而非稳定落后。SHANIU 的 96s 不可比（Responses 协议 + 与 codex 并发 + 3 题被 400 提前打断，样本本身就偏短）。

### 4.2 改动面（文件数 / +增/-删）

| 题 | codex | dsh | zcode | SHANIU |
|---|---|---|---|---|
| repo-bug-retry-after | 1 / +5/-1 | N/A | 1 / +9/-2 | 0 / 0 |
| repo-bug-apply-patch-read-guard | 1 / +3/-1 | N/A | 1 / +6/-1 | 1 / +3/-1 |
| repo-bug-rollout-damaged | 1 / +3/-0 | N/A | 1 / +3/-0 | 1 / +3/-0 |
| repo-bug-compaction-tail | 1 / +1/-1 | N/A | 1 / +1/-1 | 1 / +1/-1 |
| repo-feat-context-overflow-code | 5 / +79/-1 | N/A | 4 / +93/-1 | 5 / +100/-2 |
| repo-feat-title-cap | 1 / +10/-1 | N/A | 2 / +38/-3 | 1 / +38/-1 |
| repo-refactor-now-ms-exit | 1 / +2/-8 | N/A | 1 / +5/-9 | 1 / +2/-8 |
| repo-refactor-rename-title-api | 1 / +8/-8 | 1 / +8/-8 | 1 / +8/-8 | 1 / +8/-8 |
| repo-bug-search-surface-lost | 3 / +49/-8 | 4 / +65/-10 | 4 / +66/-17 | 0 / 0 |
| repo-bug-cut-pairing | 3 / +110/-16 | 3 / +88/-7 | 0 / 0（超时） | 3 / +91/-14 |
| repo-doc-max-turns-drift | 1 / +1/-1 | 1 / +1/-1 | 1 / +1/-1 | 1 / +1/-1 |
| repo-doc-claim-after-rename | 2 / +2/-2 | 2 / +2/-2 | 2 / +2/-2 | 0 / 0 |

- 全臂 `forbid_hit=false`，没有跳过判据直接改测试区/判据的行为。
- dsh 前 7 题显示 N/A：工作区被外部磁盘清理删除，`git diff` 已无法重算（见第 5.2 节），不是漏采。

## 5. 关键发现

### 5.1 dsh 的 Windows DACL 自伤：参考实现缺陷，不是模型能力差距

- 12 题事件流里 `is denied` 共 **360 次**（分题：51/25/29/27/36/14/35/4/34/34/50/21，`raw/dsh-*-r1.ndjson` 可复核）。
- 即便如此 dsh **12/12 PASS**，说明它在被拒绝后能换路径绕过，代价是更多轮次与 token（28.48M in，是 codex 的 3 倍）。
- ACE 取证（`Everyone` 显式 Deny `DeleteSubdirectoriesAndFiles` + dsh 自家 `S-1-4-*` grants、受影响的 `Set-Location`/`Get-Item`/`Test-Path`/`Get-ChildItem`/`New-Item`、`os error 5` 因果链）见 `phase1-intermediate.md` 发现 2。Phase 2 的工作区已被外部清理删除，无法重新读取 ACL，本轮只有拒绝计数。

### 5.2 环境故障（与能力无关，均已处置）

| 故障 | 证据 | 处置 |
|---|---|---|
| codex 臂中断 | `robocopy code=16` + `磁盘空间不足 (os error 112)`，C 盘被 12 题工作区占满（旧 tmpRoot 24.69GB） | 回收 19.69GB 后 `--resume` 续跑；第 7 题假红用旧工作区重判纠正 |
| dsh 臂中断 | `%TEMP%` 下 `pristine/` 与 8 个已完成工作区被外部清理删除，`robocopy` 报 `code=16 源目录不存在`；同题判据报 `couldn't create a temp dir: os error 3` | 驱动器加「快照目录按需重建」；`--resume` 重跑被判据环境故障污染的那题，PASS |
| zcode `cut-pairing` 超时 | 960s 硬超时 ×2；事件流 96 条 shell、12 次 `cargo test`、21 次 `streamRecovery.updated`（端点流中断重试），`diff=0` | 判 TIMEOUT，不归因能力；不估算 token |

### 5.3 新增两题的分辨力

- `repo-bug-search-surface-lost`（一个字段贯穿投影/写索引/查询/输出四层）：codex/dsh/zcode 全过，改动 3~4 文件；SHANIU 因 400 中断未答。**对 flash 没有区分度，但对 harness 有**——它要求跨 3 文件同步修改，单点修补过不了。
- `repo-bug-cut-pairing`（压缩切点必须保住 call/output 配对 + 双锚点序号）：codex/dsh 过（+110/-16、+88/-7），**zcode 两次超时**，是本轮唯一拉开差距的题。

### 5.4 SHANIU 的 Responses 缺陷仍存在

`68d4528` 修好了用户消息形状，但本轮 4 次 400 表明**工具调用/工具结果条目**的序列化仍有问题（触发点都在含工具结果的回合）。这是本轮 SHANIU 三个 FAIL 的直接原因，优先级高于任何能力对比结论。

## 6. 是否需要 pro

**不需要。** 依据：三臂 12 题里没有任何一题是「flash 在正常 harness 下过不去」；唯一的 TIMEOUT 是 zcode 在单题上时间打满（第二次复现），属参考实现的时间/工具层问题，不是「模型不会」。按既定规则（只在 flash 失败的题上跑 pro），当前没有触发条件。

## 7. 数据可用性（缺口与原因）

| 缺口 | 范围 | 原因 |
|---|---|---|
| agent 墙钟缺失 | codex 前 7 题、dsh 前 7 题 | 首轮在写 summary 前中断；事件流无时间戳，无法反推（zcode 事件流有时间戳，故完整） |
| diff 缺失 | dsh 前 7 题 | 工作区被外部清理删除 |
| token 缺失 | zcode `cut-pairing` | 进程被硬杀，usage 事件未落盘 |

以上缺口一律记 `N/A`，**不做估算**。dsh 前 7 题的整题墙钟仍可从首轮日志 `phase2-dsh.log` 的 `done` 行读到（310.6s / 354.4s / 269.8s / 325.6s / 262.5s / 259.2s / 293.3s），但无法拆出 agent 段。

## 8. 清理与保留

**已回收（本轮，显式删除）**

| 对象 | 体积 | 说明 |
|---|---|---|
| `%TEMP%\shaniu-evals-repo-ws-7420` | 15.05 GB | codex 首轮 12 题工作区副本，磁盘满后回收 |
| `target\shaniu-phase2` | 4.64 GB | codex 臂专用 `CARGO_TARGET_DIR` |
| `target\dsh-home` | 5.2 MB | dsh 独立 HOME（会话/配置） |
| `target\zcode-personal` | 0.9 KB | zcode provider 配置，**含 API key**，已删 |
| **合计** | **≈ 19.69 GB** | |

**自动回收（无需人工）**

- 各轮 `%TEMP%\shaniu-cmp-*`（工作区副本 + 判据副本 + 快照 worktree）：进程退出即删，当前 `%TEMP%` 下已无残留。
- 判据副本：每题判完立即 `rm`（12 题 × 4 臂），不再留在盘上。
- codex 首轮遗留的每题工作区（`--resume-ws` 指向的 `ws/`）：复用后逐题 `dropOldWorkspace`，C 盘可用 58.9 → 83.3GB；这部分与上表 15.05GB 是同一个目录，不重复计。

**保留**

- `evals/results-compare/phase2-{codex,dsh,zcode,shaniu}/`：`summary.json`、判据日志（含 `.resume.judge.log` 与 `raw/archive/` 首轮取证）、事件流、`progress.log`，共 70.9MB。
- `evals/results-compare/phase2-prep/`、各轮运行日志（`phase2-*.log`）。
- 主 `target\`、`dist-release\`、`docs\smoke\**`、`docs\analysis\**` 未动。
- `%TEMP%\shaniu-d7-target`（12.1GB）与 `%TEMP%\shaniu-d7b-evidence` 属**另一条线正在使用**（取证时有 `clippy-driver` 进程持有），本轮未触碰。
- `D:\AI\ZCode` 为源码仓库，构建产物保留以便复跑。

## 9. 复现入口

```text
node evals/compare/run-arm.mjs --arm codex --resume --resume-ws "<旧tmpRoot>\ws" \
  --expect-tree "<12 题指纹>" --out evals/results-compare/phase2-codex
```

续跑、进度行、预算行、收尾行、判据副本即删、快照目录自愈的说明见 `evals/compare/README.md` 的「断点续跑」小节。
