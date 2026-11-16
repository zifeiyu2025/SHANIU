# Phase 2「零改动收工」机制归因（2026-11-15）

> **后续修正（2026-11-16 入库时补，阅读以本节为准）**：本文是 2026-11-15 的只读分析快照，
> 其中三处已被后续提交推进：
>
> 1. **§8 的「根因只有一个：修复提醒的注入时机」不是最终修法。** 400 实际由
>    `a09977b fix(model_client): Responses 工具调用补齐配对输出` 修掉：发送前给每个没有同
>    `call_id` 输出的调用补一条紧跟其后的 `function_call_output`（与 codex
>    `context_manager/normalize.rs::ensure_call_outputs_present` 同口径）。修复后整轮重跑
>    `evals/results-compare/phase2-shaniu-envfix-r2.md` 里 400 类故障命中 0 处。
> 2. **§8 建议的「把 repair 提醒挪到工具批次结束后统一 push」未采纳，至今未实现。**
>    `src-tauri/src/agent_loop/mod.rs:2454-2469` 仍在每条工具调用结束后立即 `history.push`。
> 3. **§6 关于 `CARGO_TARGET_DIR` 的【推断】已落地：** `05ccacc` 让评测子进程摘掉该变量，
>    `bac8252` 用 A/B 证明当时两处回归是它的假回归。
>
> 另：§1、§5、§7 引用的 `mod.rs` 行号是 2026-11-15 工作树快照，此后 `mod.rs` 有改动，
> 行号会漂移，按符号名查。

只读分析，未改任何代码/配置。数据来自 `evals/results-compare/phase2-shaniu/`，以及该轮评测生成的逐事件 rollout（harness 的 `RolloutWriter` 产物）。

## 0. 先给结论

三个失败回合**都不是**「读了代码判断不用改」，**不是**「写入工具失败」，**也不是**「没读就结束」。它们是**读到一半被一次 HTTP 400 直接终止**，0 文件改动是进程被终止后的结果，不是 agent 的选择。

归因链（每一环都有事件或代码依据）：

1. 一次命令类工具返回非零退出（三题各有不同触发源，见 §3）；
2. `repair.observe` 判定「上一条命令执行失败」，生成「自动修复提醒」，并在**逐条工具调用循环内部**立刻 `history.push(System)`（[mod.rs 2317-2332](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs#L2317-L2332)）；
3. 如果这次失败调用**不是该批次的最后一条**，这条 System 消息就落在两条 `function_call_output` 之间；
4. 下一次模型请求被端点以 HTTP 400 `No tool output found for tool call <call_01_...>` 拒绝（三题 + read-guard 四题全部如此）；
5. 该错误是终局：回合内已经有工具调用（`!calls.is_empty()`）时直接 `GiveUp`，不重试（[mod.rs 1531-1532](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs#L1531-L1532)、[1552-1563](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs#L1552-L1563)），`reason=Failed`，`attempts=0`；
6. 0 文件改动，判据红。

最强的实测证据是 12 题的全量对照：**恰好 4 题出现「批次中间」的修复提醒，这 4 题全部触发该 400；其余 8 题只出现「批次末尾」的提醒，一次都没触发。**

| 任务 | 批次中间提醒 | 批次末尾提醒 | 触发 400 | 结果 |
|---|---:|---:|---|---|
| repo-bug-retry-after | 1 | 0 | 是 | FAIL / 0 文件 |
| repo-bug-search-surface-lost | 1 | 0 | 是 | FAIL / 0 文件 |
| repo-doc-claim-after-rename | 1 | 0 | 是 | FAIL / 0 文件 |
| repo-bug-apply-patch-read-guard | 1 | 2 | 是 | PASS（判据）/ 收口 Failed |
| compaction-tail / rollout-damaged / cut-pairing / doc-max-turns-drift / feat-context-overflow-code / feat-title-cap / refactor-now-ms-exit / refactor-rename-title-api | 0 | 1–4 | 否 | 其余 8 题 |

【实测】相关性（4/4 vs 0/8）与代码路径都成立。

---

## 1. 证据来源与口径

- `evals/results-compare/phase2-shaniu/summary.json`：12 题的 status / diff / usage / detail。
- `evals/results-compare/phase2-shaniu/raw/shaniu-run.log`：只有请求级日志（`[req] start/end`、`err kind=BadRequest`），**没有工具调用**，不足以回答 §2 的问题。
- `evals/results-compare/phase2-shaniu/raw/shaniu-stdout.log`：复现命令与报告落点，同样没有工具调用。
- 真正的逐事件流在 `target/evals-repo-rollouts-7420/<task>/<task>-r1.jsonl`（该轮 harness pid=7420）。每行一条：`response_item`（`message` / `function_call` / `function_call_output`）、`turn_end`（turn、iterations、reason、error）。下文的 `at_ms` 是 Unix epoch 毫秒，可换算成本地时间（例如三题首条消息分别落在 20:55 / 21:20 / 21:30，与环境快照一致）。
- 代码依据：[agent_loop/mod.rs](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs)、[agent_loop/repair.rs](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/repair.rs)、[tools/unified_exec.rs](file:///d:/MyWorkspace/SHANIU/src-tauri/src/tools/unified_exec.rs)、[tests/evals_repo.rs](file:///d:/MyWorkspace/SHANIU/src-tauri/tests/evals_repo.rs)。行号基于 2026-11-15 当时的工作树，仓库里这些文件存在未提交改动。

关于「轮数」的口径：【实测】三题的 rollout `turn_end.turn` 都是 **1**（`iterations` 分别为 4 / 5 / 5），而 summary 里的 `usage.turns` 是 3 / 4 / 4，正好等于三题的模型请求次数（= assistant 消息数）。所以 summary 的 `usage.turns` 不能当作「跑了几个回合」读；外层回合数只有 1。该字段在 Node 聚合器里的精确语义未在本次数据中落盘，此点标记为【推断】。

---

## 2. Q1：三个回合到底做了什么

### 2.1 `repo-bug-retry-after`（summary：4336 ms，`usage.turns=3`，0 文件）

| at_ms | 事件 | 内容（原文截断） |
|---|---|---|
| 1794747316330 | assistant | 「我先看一下仓库结构和相关实现。」 |
| 1794747316331 | call `list_dir` | `{"path":"."}` |
| 1794747316331 | call `search` | `{"query":"retry","path":"src-tauri/src","max_results":50}` |
| 1794747316331 | output | 目录清单 / `agent_loop/mod.rs:163 ...` 等命中 |
| 1794747317244 | call `list_dir` | `{"path":"src-tauri/src/model_client"}` |
| 1794747317244 | call `read_file` | `{"path":"src-tauri/src/model_client/retry.rs"}` |
| 1794747317244 | output | retry.rs 全文，其中第 67 行：`pub fn delay(&self, retry: u32, _provider_retry_after_ms: Option<u64>, random01: f64)` |
| 1794747318930 | call `unified_exec` | `{"command":"cargo test --lib model_client::retry 2>&1","cwd":"src-tauri","run_in_background":true}` |
| 1794747318930 | call `search` | `{"query":"Retry-After","path":"docs","max_results":20}` |
| 1794747318930 | output | `参数非法: run_in_background 需要会话作用域（请从会话内调用）` |
| 1794747318930 | system | `自动修复提醒（第 1/2 次）：上一条命令执行失败。…` |
| 1794747318930 | output | `04-协议规范.md:48: | model_retry ...` |
| 1794747319097 | turn_end | `turn=1 iterations=4 reason=failed`，`BAD_REQUEST / invalid_request_error`：`No tool output found for tool call call_01_gA3OS9XR78mJ94OSvbwL6255.` |

序列：读目录 → 搜 retry → 读 `model_client/` → 读 `retry.rs` → 起测试 + 查文档 → 400 终止。

### 2.2 `repo-bug-search-surface-lost`（summary：6766 ms，`usage.turns=4`，0 文件）

| at_ms | 事件 | 内容（原文截断） |
|---|---|---|
| 1794748857490 | assistant + 2 call | `list_dir {"path":"."}`、`unified_exec {"command":"git log --oneline -5"}` |
| 1794748858969 | 2 call | `dir src-tauri\src /b & echo ---- & dir src-tauri\src\session* /b`、`search {"query":"session_query","path":"src-tauri/src"}` |
| 1794748860458 | 2 call | `dir /b /s src-tauri\src\session_query src-tauri\src\compaction src-tauri\src\rollout`、`cd src-tauri && cargo test --lib session_query:: -- --list 2>&1 \| more` |
| 1794748860458 | output | `error: failed to open: d:\MyWorkspace\SHANIU\target\shaniu-phase2\debug\.cargo-build-lock ... 拒绝访问。(os error 5)`（因接了 `\| more`，退出码记 0） |
| 1794748861955 | 2 call | `dir /b src-tauri && ... && type src-tauri\.cargo\config.toml 2>nul ...`、`read_file {"path":"src-tauri/src/session_query/mod.rs"}` |
| 1794748861955 | output | 目录清单，`[exit code: 1]`（探测文件不存在的正常非零） |
| 1794748861956 | system | `自动修复提醒（第 1/2 次）：…` |
| 1794748861956 | output | `session_query/mod.rs` 全文（`1: //! SH-113 会话全文搜索...`） |
| 1794748862453 | turn_end | `turn=1 iterations=5 reason=failed`，同样 `No tool output found for tool call call_01_QFI1bEZhcSZpqPSybpwz8649.` |

序列：看目录/git 历史 → 找 session_query 文件 → 试图列测试（被 cargo 锁拒绝）→ 探测 `.cargo/config.toml`（exit 1）+ 读实现 → 400 终止。

### 2.3 `repo-doc-claim-after-rename`（summary：38156 ms，`usage.turns=4`，0 文件）

| at_ms | 事件 | 内容（原文截断） |
|---|---|---|
| 1794749410622 | assistant + 2 call | `list_dir {}`、`search {"query":"parse_metadata","max_results":50}` |
| 1794749410622 | output | `align/doc-claims.md:68: | R2-010 | ... | parse_metadata | 符号 |` |
| 1794749412331 | 2 call | `search {"query":"parse_endpoint_metadata"}`、`read_file {"path":"align/doc-claims.md"}` |
| 1794749412331 | output | `src-tauri/src/capability.rs:309: pub fn parse_endpoint_metadata(...)`；清单全文 |
| 1794749414671 | 2 call | `read_file {"path":"docs/01-架构需求文档.md","offset":130,"limit":40}`、`read_file {"path":"scripts/dev/check-doc-claims.ps1"}` |
| 1794749446025 | 2 call | `unified_exec {"command":"pwsh -NoProfile -File scripts/dev/check-doc-claims.ps1","timeout_ms":180000}`、`read_file {"path":"src-tauri/src/capability.rs","offset":280,"limit":60}` |
| 1794749446025 | output | `[doc-claims] ... 通过 304（判定点）/ A 类缺口 1 / B 类 ...` |
| 1794749446025 | system | `自动修复提醒（第 1/2 次）：…` |
| 1794749446025 | output | `capability.rs` 280–339 行 |
| 1794749446423 | turn_end | `turn=1 iterations=5 reason=failed`，`No tool output found for tool call call_01_7Y3LpnLEoZ7pgwFhlxWg7581.` |

这题 38 s 的来源很清楚：1794749414672 → 1794749446025 之间有 **31.35 s**，是 `check-doc-claims.ps1` 的实际运行时间。

---

## 3. Q2：三分法逐条判定

| 假设 | 判定 | 证据 |
|---|---|---|
| 「根本没读就结束」 | **否** | 三题分别有 6 / 8 / 8 次工具调用，全部是 `list_dir` / `search` / `read_file` / `unified_exec`，且每次调用都有对应输出（6/6、8/8、8/8）。 |
| 「读了代码后判断不需要改」 | **否** | 三题唯一的 assistant 文本都是开场一句（「我先看一下…」），没有任何「无需修改 / 已经是正确行为」的结论，也没有收尾消息。`retry-after` 甚至已经读到缺陷行（`retry.rs:67` 的 `_provider_retry_after_ms`），但它下一步是去跑测试，不是宣布不用改。 |
| 「想改但写入工具失败」 | **否**（三题都没有进入写入阶段） | 三题的 `apply_patch` / 写文件调用数均为 **0**，summary diff 为 0 文件；不存在写入参数错误、沙箱拒绝或路径错误的记录。 |
| **实际情况** | —— | 读到关键文件后推进「验证」→ 首个命令失败 → 修复提醒插进批次中间 → 下一请求 400 → 进程级终止。 |

【实测】前三条均由 rollout 的调用清单与输出计数直接反证。

---

## 4. Q3：如果算「失败」，失败在哪一层

先明确：**不是写入失败**。失败分两层，触发层与致命层不同。

| 题目 | 触发层（首个失败） | 致命层 | 证据 |
|---|---|---|---|
| retry-after | **工具参数层**：`run_in_background` 需要会话作用域（eval 里 `jobs=None`、`session_id=None`） | 协议层：HTTP 400 `No tool output found` | 输出原文 `参数非法: run_in_background 需要会话作用域（请从会话内调用）`；代码 [unified_exec.rs 533-539](file:///d:/MyWorkspace/SHANIU/src-tauri/src/tools/unified_exec.rs#L533-L539)；schema 却始终宣告该参数（[310-313](file:///d:/MyWorkspace/SHANIU/src-tauri/src/tools/unified_exec.rs#L310-L313)）；harness 传 None（[evals_repo.rs 1302-1303](file:///d:/MyWorkspace/SHANIU/src-tauri/tests/evals_repo.rs#L1302-L1303)） |
| search-surface-lost | **命令层**：`type ... 2>nul` 探测不存在文件，退出码 1（正常语义） | 协议层：同上 | 输出 `[exit code: 1]`；另有一次真实沙箱拒绝：`cargo-build-lock ... 拒绝访问 (os error 5)`，但那是另一条命令，且当时被 `\| more` 掩成退出码 0 |
| doc-claim-after-rename | **命令层**：`check-doc-claims.ps1` 按设计报 `A 类缺口 1` 并返回非零 | 协议层：同上 | 输出 `[doc-claims] ... A 类缺口 1`；这正是任务要求 agent 先去观察的「红」 |

对「工具输出没回来」这一项的核查：【实测】**本地没有丢输出**。三题的 `function_call` 与 `function_call_output` 数量一一对应（6/6、8/8、8/8），包括报错的那条 `call_01` 的输出也已经写入 rollout。400 里的「No tool output found」是**端点对请求历史配对的判定**，不是本地工具结果缺失。

【推断】端点侧的具体配对规则是「每个 `function_call` 的输出必须紧跟在该调用之后、且工具输出块中间不能夹非工具条目」。依据有三条：① 报错的永远是被提示隔开的那个 `call_01`；② 同一批 `list_dir + read_file`（中间没有 System）能正常进入下一请求；③ 代码里空转提醒刻意「排在全部工具输出之后」（[mod.rs 2366-2373](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs#L2366-L2373) 的 SH-122 注释），说明作者已知这个顺序约束。
验证方法：在请求发出前把 history 序列化落盘（或加一个只读 replay harness），对比「提醒在末尾」与「提醒在中间」两种 payload；两者只差这一条 System 的位置。

补充一个 eval 专属问题：`run_in_background` 的 schema 在评测里照样宣告，但 harness 不注入 job registry，模型合法使用该参数必然 400/参数非法。这不是模型乱用工具，是**工具面与运行时能力不一致**（生产 UI 有 jobs，评测没有）。

---

## 5. Q4：收工靠什么判定，有没有把「没做事」当「做完了」

【实测】三题都不是模型主动结束，也不是轮数/预算：

- 没有「完成」类 assistant 消息，只有开场一句。
- `turn=1`，三题上限分别是 24 / 28 / 16，远未接近。
- 终止点是模型请求错误：`raw/shaniu-run.log` 里每条 `seq=N err kind=BadRequest code=BAD_REQUEST` 之后紧跟 `[agent] end ... reason=Failed`；rollout `turn_end.reason="failed"`。
- 不重试：eval 用 `RetryPolicy::disabled()`（[evals_repo.rs 1305](file:///d:/MyWorkspace/SHANIU/src-tauri/tests/evals_repo.rs#L1305)），且本回合已有工具调用时 `GiveUp` 分支直接跳过重试（[mod.rs 1531-1532](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs#L1531-L1532)），所以 `attempts=0`。

有没有把「没做事」当「做完了」：【实测】**判据层面没有**。判据独立执行，status 由判据决定（[evals_repo.rs 1339-1352](file:///d:/MyWorkspace/SHANIU/src-tauri/tests/evals_repo.rs#L1339-L1352)），三题 0 文件就是 FAIL。

但有两处可观测性坑，会让人误读：

1. summary 的 `usage.turns=3/4/4` 看起来像「跑了三四轮」，实际外层只有 1 轮；它是模型请求次数。
2. `detail` 的拼法不对称：**FAIL 只显示判据输出，不显示 agent 的终止错误**；同样一个 400，在 PASS 的 read-guard 上却被写进 `detail`（`判据通过｜收口=Failed｜错误=[No tool output found ...]`）。这会让「失败原因」在汇总里完全不可见。

另外，read-guard 的 PASS 确实说明「判据通过 ≠ agent 完成了收尾」：它在崩溃前已经改了文件，判据只认文件结果，不看收口。这不是「没做事算做完」，但属于「通过但没有收尾」。

---

## 6. Q5：四个 `BudgetExhausted` 但判据通过的回合，有没有无效循环

先纠正一处：实际上限是 **feat-context=32、title-cap=24、cut-pairing=32、doc-max=12**（不是 12/24/24/32；见 `evals/repo-tasks.json` 的 `max_turns`）。

| 任务 | 上限/实跑 | 调用数 | apply_patch | 补丁失败 | cargo 锁拒绝 | 完全相同的重复调用 | 结束时在做什么 |
|---|---:|---:|---:|---:|---:|---:|---|
| feat-context-overflow-code | 32/32 | 41 | 16（11 成功） | 5（3 上下文不匹配 + 2 只有上下文行） | 1 | 0 | 用 `%CD%\..\target` 跑 `cargo test --test repo_feat_context_overflow`，目录创建失败（os error 3）→ 预算耗尽 |
| feat-title-cap | 24/24 | 30 | 7（6 成功） | 1（上下文不匹配） | 1 | 1（同一条 test 命令跑两次） | 用 `CARGO_TARGET_DIR=%CD%\target` 跑通（`10 passed; 0 failed`）后，预算耗尽 |
| cut-pairing | 32/32 | 44 | 5（4 成功） | 1（28 行上下文不匹配） | 2 | 0 | 全量 `cargo test --lib` 之后，下一轮读文件时耗尽 |
| doc-max-turns-drift | 12/12 | 19 | 1（成功） | 0 | 2 | 0 | 在探测 cargo/target 配置时耗尽 |

【实测】逐条看，结论是**没有语义上的「反复改同一处」**：

- `feat-context` 的 5 次补丁失败都是上下文不匹配/格式不合法，随后重发的是**更窄的不同 hunk**；16 次补丁里没有两次参数完全相同。
- `cut-pairing` 的 1 次补丁失败是 28 行大 hunk 对不上，改成小块后成功；4 次成功分别落在 `compaction/mod.rs`（2 次不同 hunk）、`rollout/resume.rs`、`session_query/documents.rs`。
- 唯一接近「同一处改三次」的是 `feat-title-cap`：patch 5 自己新增了一个测试，patch 6 把该测试的输入从 `20/20` 改成 `19/19`，patch 7 再改该测试的断言（期望空白折叠）。**改的是它自己刚写的测试块，生产代码只改了 1 次**；且没有改既有测试。重复命令也只有一条 test 命令（中间插了一次测试断言修正），不是盲重跑。
- 四题的系统消息里**没有出现过任何「重复/空转」类提醒**；该轮 12 题都没有出现此类文案，因此没有可佐证的「被检测为零转」记录。

【实测】预算真正花在哪：四题都撞到同一条环境错误——

`error: failed to open: d:\MyWorkspace\SHANIU\target\shaniu-phase2\debug\.cargo-build-lock ... 拒绝访问。(os error 5)`

agent 只能反复探测 `.cargo/config.toml`、`src-tauri/target`、环境变量，最后 `feat-title-cap` 用 `set CARGO_TARGET_DIR=%CD%\target` 才跑通。

【实测】原因就在环境变量本身。`repo-doc-max-turns-drift` 的最后一轮直接执行了 `set CARGO_TARGET_DIR`，输出原文：

`CARGO_TARGET_DIR=d:\MyWorkspace\SHANIU\target\shaniu-phase2`

而 agent 的工作区在 `C:\Users\jun\AppData\Local\Temp\shaniu-evals-repo-ws-7420`，沙箱只允许写工作区，于是 cargo 去仓库里建/开 `.cargo-build-lock` 被拒。harness 自己也明确注释「刻意不设 CARGO_TARGET_DIR」（[evals_repo.rs 1077-1080](file:///d:/MyWorkspace/SHANIU/src-tauri/tests/evals_repo.rs#L1077-L1080)），预热产物落在 `ws/src-tauri/target`，与报错路径不是一处。

【推断】该变量是评测进程从**外层启动脚本/shell**继承来的；具体是哪一个脚本设置，未在本轮数据中落盘。
验证方法：查该轮启动脚本或父 shell 的 `$env:CARGO_TARGET_DIR`；或清空该变量重跑这四题，看锁错误是否消失、轮数是否下降。

**判定**：这四题属于「啰嗦但方向对」，不算语义缺陷（判据也确实通过）；但预算被一个可修的环境坑吃掉，属于另一类成本问题，不建议归为「模型能力问题」。

---

## 7. Q6：`apply-patch-read-guard` 的 `No tool output found` 是哪一环丢的

【实测】跑的序列（关键段）：

| at_ms | 事件 | 内容 |
|---|---|---|
| 1794747409881 | user | `cargo test --lib tools::apply_patch` 有失败；恢复「写前必须先读」守卫 |
| 1794747420880 | call ×2 | `search ...`、`unified_exec {"command":"git log --oneline -15 -- src-tauri/src/tools/apply_patch.rs"}` |
| 1794747422413 | call ×2 | `read_file tool_router/mod.rs ...`、`unified_exec {"command":"git log --oneline -5 && git show --stat HEAD \| head -20"}` |
| 1794747422413 | output | `'head' 不是内部或外部命令...`，`[exit code: 255]` |
| 1794747422413 | system | `自动修复提醒（第 1/2 次）...`（该批最后一条 → 无副作用） |
| 1794747427675 | call `apply_patch` | 修 `tools/apply_patch.rs` |
| 1794747427675 | output | `补丁应用成功（1 处）： ~ src-tauri/src/tools/apply_patch.rs`（+3/-1） |
| 1794747429481 | call `unified_exec` | `cargo test --lib tools::apply_patch` |
| 1794747429481 | output | `cargo-build-lock ... 拒绝访问`，`[exit code: 101]` |
| 1794747429481 | system | `自动修复提醒（第 1/2 次）...`（单调用批次最后一条 → 无副作用，下一请求成功） |
| 1794747430744 | call ×2 | `unified_exec {"command":"dir /a .cargo 2>nul & type .cargo\\config.toml 2>nul"}`、`list_dir {"path":"target"}` |
| 1794747430744 | output | 目录输出，`[exit code: 1]` |
| 1794747430745 | system | `自动修复提醒（第 2/2 次）...`（**中间**：`call_01=list_dir` 的输出还没入历史） |
| 1794747430745 | output | `target` 目录清单：`.rustc_info.json`、`CACHEDIR.TAG`、`debug/` |
| 1794747430878 | turn_end | `turn=1 iterations=12 reason=failed`，`No tool output found for tool call call_01_X7pFedLZ8pmzxHEd0PmX7825.` |

回答「哪一环丢的」：

- **本地没有丢**：18 次调用对应 18 条输出，`call_01_X7pFedLZ8pmzxHEd0PmX7825`（`list_dir {"path":"target"}`）的输出就在崩溃前 133 ms 写入了 rollout。
- **丢在请求装配/端点解析这一环**：同 §4 的机制，`自动修复提醒（第 2/2 次）` 被插在 `call_00` 与 `call_01` 的两条输出之间；下一次请求被端点判定为「`call_01` 没有工具输出」。
- **后续没有「绕过去」**：400 之后直接 `reason=failed` 终止，没有第二次机会。判据之所以通过，是因为补丁在 1794747427675 就已经落盘（+3/-1），**判据只看文件，不看 agent 是否收尾**。这属于「崩溃前恰好改完」的偶然 PASS，不是恢复成功。

对照也在这题里：1794747422413 与 1794747429481 两次提醒都落在批次末尾，下一请求都正常；只有 1794747430745 那次落在中间才崩。同一题内的三种位置正好构成 A/B 对照。

---

## 8. 机制根因与建议修复方向（本轮未实施）

根因只有一个：**修复提醒的注入时机**。

- 失败路径：[mod.rs 2317-2332](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs#L2317-L2332) 在每条工具调用执行完立刻 `history.push(Role::System, ...)`；而工具输出同样是逐条 push（[record_tool_result 2672](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs#L2660-L2688)）。
- 正确做法代码里已有先例：[mod.rs 2366-2373](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs#L2366-L2373) 的空转提醒是「排在全部工具输出之后」；预算收口也特意放在批次结束之后（[2381-2388](file:///d:/MyWorkspace/SHANIU/src-tauri/src/agent_loop/mod.rs#L2381-L2388)）。
- 建议：把 repair 的 `Nudge / GiveUp / RejectedRepair / SuspiciousRepair` 文案也放进一个待发队列，在工具批次循环结束后统一 push（与 `anomaly_reminders` 合并即可）。理由：这不改变任何判定语义，只改变注入位置；按本轮 12 题样本应能消除这 4 次 400（【推断】，需重跑同一套题验证，观察点：mid 提醒归零、400 归零、三题是否进入编辑阶段）。该做法也符合已有 SH-122 的约定。
- 次要建议（评测有效性）：
  1. eval 里给 `run_in_background` 注入 job registry，或在 `jobs=None` 时不宣告该参数，避免「合法参数必失败」。
  2. 子进程启动前清空/重定向 `CARGO_TARGET_DIR` 到工作区内，避免 4 题的预算被 cargo 锁拒绝吃掉。
  3. FAIL 的 `detail` 也附上 `turn_end.error`（与 PASS 一致），否则汇总里看不到真正的终止原因。

---

## 9. 事件流不足、只能到「推断」的部分

1. **端点为什么判「没有工具输出」**：我们只有 provider 的错误文本和本地历史顺序，没有请求体。`function_call_output` 是按 `call_id` 匹配还是按相邻位置匹配，无法从现有数据证明；上面的 4/4 vs 0/8 相关性 + 代码路径足以定位到「中间插入的 System」，但具体解析规则是推断。
2. **模型的意图**：三题在崩溃前都没有表达「要不要改」，无法证明它「本来会改」。能证明的只是它没有进入写入阶段。
3. **`usage.turns` 的精确语义**：只能确认它等于模型请求次数（3/4/4 与 assistant 消息数一致），聚合器源码未在本次数据中核对。
4. **`CARGO_TARGET_DIR` 的来源**：值本身已被 agent 的 `set CARGO_TARGET_DIR` 输出实测（`d:\MyWorkspace\SHANIU\target\shaniu-phase2`），也能确认 harness 自己没设；但「外层哪个脚本设置的」没有落盘，需要看该轮启动脚本/shell。
