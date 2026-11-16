# 2026-11-17 `apply_patch` 多 `@@` 块修复证据

## 背景

评测 `repo-feat-context-overflow-code`（`evals/results-compare/phase2-shaniu-envfix-r2/`，被测
`bb5e17c`）32 轮打满、判据红在 `E0004`（非穷尽 match）。逐轮读事件流后发现一个独立于
「轮次预算」的工具缺陷：模型按 codex/dsh 的 V4A 写法在**同一个 `*** Update File` 段里用了多个
`@@` 块**，我们的解析器把 `@@` 当无意义标记丢弃、把整段并成一条连续序列，于是必然
「上下文不匹配」。

## 证据（事件流行号）

文件：`evals/results-compare/phase2-shaniu-envfix-r2/raw/rollouts/repo-feat-context-overflow-code/repo-feat-context-overflow-code-r1.jsonl`

| 补丁（行） | `@@` 块数 | 结果 |
|---|---|---|
| 79 | 6 | 拒：`未找到补丁期望的 39 行序列`（覆盖 error.rs 全部改动） |
| 82 | 1 | 成功（= 79 的第 1 块：imports + 常量） |
| 84 | 1 | 成功（= 79 的第 2 块：新增变体） |
| 86 | 3 | 拒：`未找到补丁期望的 19 行序列`（= 79 的第 3/4/5 块） |
| 89 | 1 | 成功（= 79 的第 3 块：`code()` 臂） |
| 91 | 2 | 拒（= 79 的第 4 块：`from_status` 文档 + 分支） |
| 94 | 1 | 成功（= 79 的第 4 块：`from_status` 文档） |
| 96 | 1 | 成功（= 79 的第 4 块：`from_status` 分支） |
| 98 | 1 | 成功（= 79 的第 5 块：`human` 臂） |
| 101 | 1 | 成功（= 79 的第 6 块：`from_body`） |
| 104 / 107 | 1 / 1 | 成功（新增关键词判定 helper；修一个 430→429 笔误） |
| 110 | 1 | 拒（块内只有上下文行，模型写法问题，与本缺陷无关） |
| 113 | 1 | 成功（补 error.rs 单测） |

14 次 `apply_patch` 里**多块补丁 3 次、3 次全被拒**；单块补丁 11 次、10 次成功。
多块 → 必失败，相关度 100%。

代价可以精确到轮：补丁 79 那 6 块覆盖的 error.rs 改动，实际用了 **10 个往返**
（79 + 82…101），其中 86、91 又被同一原因拒了两次；多块支持后这批改动是 **1 个往返**，
净浪费 **9 轮 / 32 轮**。剩下 5 轮里模型去写 helper 与单测，`error_codes.rs`（代码常量登记）
与 `key_check.rs`（缺 arm 的 match）到最后都没改到。

## 参考实现

- `src/dsh/packages/fs/fs-patch/src/parse.ts`：`readUpdate` 按 `@@` 切块（含 `@@ <锚点原文>`）。
- `src/dsh/packages/fs/fs-patch/src/apply.ts`：`applyHunks` 用 `lineIndex` 游标逐块向后推进，
  任一块失败即整体报错。

## 改动

`src-tauri/src/tools/apply_patch.rs`

- `struct UpdateHunk { anchor, seq }`：`PatchOp::Update` 由单条 `seq` 改为 `hunks: Vec<UpdateHunk>`。
- 解析：`@@` 起新块，`@@ <原文>` 记锚点；空块报错，只含上下文行的块照旧报错。
- 应用：新增 `apply_hunks`，每块从上一块结束位置向后找；锚点行先定位再找块；
  任一块失败返回 `第 N 个 @@ 块：…`，整单回滚不变。
- `first_match` 增加 `from` 起点；`apply_hunk_seq` 返回命中位置供游标推进。
- 工具描述补一句「同一 Update 段可用多个 `@@` 块；一个块对不上则整单失败回滚」。

判据/断言**未放宽**：五级匹配严格度、整单回滚、空块与纯上下文块拒绝全部保留。

## 用例与原始输出

| 文件 | 内容 |
|---|---|
| `apply-patch-multi-hunk-red.log` | 修复前：3 条新用例全红（`上下文不匹配：未找到补丁期望的 2 行序列`、`@@ fn beta() {` 无法识别、缺 `第 2 个 @@ 块` 文案） |
| `apply-patch-multi-hunk-green.log` | 修复后：`32 passed; 0 failed`（`tools::apply_patch`） |
| `cargo-test-lib-full.log` | 修复后：`1146 passed; 0 failed; 4 ignored` |
| `clippy.log` | `cargo clippy --all-targets -- -D warnings` 通过（`Finished dev profile`） |

新用例（`src-tauri/src/tools/apply_patch.rs`）：

- `multi_hunk_update_applies_each_hunk_at_its_own_location`（1413）：两处远隔的改动一次落盘。
  反向对照：把 `@@` 重新当无意义标记丢弃，本条必红。
- `hunk_anchor_skips_to_the_named_line`（1433）：`@@ <原文>` 锚点生效，不会命中文件里更早的同名上下文。
  反向对照：忽略锚点文本，本条改到 `alpha` 那段，断言必红。
- `multi_hunk_second_hunk_mismatch_writes_nothing`（1456）：**反向验证**——第二块对不上时整条补丁
  失败且一个字节都不写，防「为了多块能过而改成部分应用」。

## 追加：0.2.9 真机冒烟 P1 之二（N2 审批挂起时强杀）

基准提交 `efa2681`（`src-tauri` 与 `src` 均为该提交 + 本次改动）。

**问题**：审批卡挂起时强杀，重开后卡片消失、结局被写成 `turn_end reason=interrupted`
（界面「已停止」）、步数 2→3。真机原始档案见
`docs/smoke/2026-11-17-9af8431/evidence/02-rollout-after-kill.jsonl`（第 12 行
`turn=2 reason=interrupted`，其后无任何执行进展）。

**根因（两处，同一口径）**：

1. `reconcile.rs` 的 `interruption_evidence` 把「档案里出现过 `TurnEnd`」当作中断证据。
   那个 `TurnEnd` 属于**第一轮**，对第二轮什么也没证明；第二轮只有用户提问 + system
   提示词 + 一个审批请求，宿主既不知道用户会不会批准，也不知道命令跑没跑，只能报「未知」。
2. 回放时 `closeStep` 无条件计数：第二轮开着的那一步在实时路径里从未闭合（进程死在
   这次采样上），重建却替它闭合并计一步——凭空多出来的那一步。

**修法**：结局只认「最后一条提问之后」同时有 `AbortNote` **与**真实执行进展；裁决由档案
自身回答（新增 `ApprovalRequest` / `ApprovalDecision` 两条档案项，请求在审批卡发出时落档、
裁决在真的有人作答时落档），「有请求无裁决」在 `load_session` 投影成 `approval_expired`，
前端重开时渲染成一行通知「这条审批在程序关闭前未作答，已失效」（不是卡，也没有回执通道）。
空 step 不计数。

**用例与原始输出**：

| 文件 | 内容 |
|---|---|
| `n2-reconcile-red.log` | 反向对照一：把 `interruption_evidence` 退回旧判据（有 `TurnEnd` 即中断），3 条新用例全红（`left: "interrupted" / right: "indeterminate"`） |
| `n2-reconcile-green.log` | 修复后：`rollout::reconcile` 8 passed |
| `n2-steps-red.log` | 反向对照二：`closeStep` 去掉 `stepHasContent` 门槛（空 step 也计数），N2 用例红：`steps: 3, turns: 2` |
| `n2-steps-green.log` | 修复后：N2 两条用例绿（`steps: 2, turns: 1`，重复重开数值不变） |
| `n2-cargo-test-full.log` | `cargo test` 全量：lib 1148 passed / 4 ignored（含新增 3 条 reconcile 用例）、集成 25 passed |
| `n2-clippy.log` | `cargo clippy --all-targets -- -D warnings` 通过 |
| `n2-vitest-full.log` | `pnpm vitest run --exclude "scratch/**"`：501 passed |
| `n2-typecheck.log` | `pnpm typecheck` 在**提交树**（`scratch/verify-n2` 干净 worktree）上通过 |

**仍未核实**：真机重跑（强杀 → 重开）尚未执行；本批只证明判据、档案留痕、回放与步数口径，
冒烟结论仍以 `docs/smoke/2026-11-17-9af8431/REPORT.md` 的 FAIL 为准，直到重跑。

