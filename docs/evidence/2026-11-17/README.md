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

## 追加：0.2.9 真机冒烟 P2 展示口径（N3 截断三口径 / N6 拒绝卡折叠态）

**N3 根因**：一个回合的结局被三处各判一次——`turn-error.ts` 本地自加的 `max-tokens` 分支
画成红色「本轮运行失败」、`TurnProcessNodeView` 的默认分支画成「已完成，用时 N 秒」、
`turn-max-tokens.ts` 再画一条黄色截断提示。三处各自读 `reason.kind`，于是同屏出现「失败 + 已完成」。

**N3 修法**：新增 `conversation-nodes/turn-ending.ts` 作为唯一判定
（`completed` / `truncated` / `stopped` / `failed` / `unknown`）；行首结局行（`turn-error.ts`）、截断提示行（`turn-max-tokens.ts`）、
组标题（`TurnProcessNodeView.tsx`）都只读它，组件侧只剩展示映射。截断只有一条黄色提示行，
组标题为「已截断」（8 套词典各加 `message.turnProcess.truncated`），不再出现「失败」或「已完成」。

**N6 根因**：rollout 档案只存 `tool_call` 的 `output` 文本、没有 `isError`；回放只按
`[exit code: N]` 判失败，而「拒绝说明：type=…」「路径越界（工作区之外）: …」
「路径越界（包含 ..）: …」都不带退出码 → 被读成成功，卡片折叠仍是 `data-state="ok"`。

**N6 修法**：`src/transport/adapter.ts` 的回放路径按上述宿主稳定行首标记还原错误态
（兼容 apply_patch 的「第 N 处（…）：」段前缀），并加一条「源码正文里含相同标记的 read」
负例防误判；未改上游渲染器，展开后正文仍是档案原文。

**用例与原始输出**：

| 文件 | 内容 |
|---|---|
| `n3n6-01-red-n3.log` | 先红：`src/turn-ending.spec.ts` / `src/turn-process-outcome.spec.ts` 4 failed / 4 passed（`expected { id: '1', role: 'update' } to be null`、`expected { kind: 'took', elapsedMs: 5000 } to deeply equal { kind: 'truncated' }`） |
| `n3n6-01-red-n6.log` | 先红：回放用例 1 failed（拒绝说明被当成成功，`isError` 仍为 `false`） |
| `n3n6-02-green-n3.log` | 修复后：`turn-ending.spec.ts` + `turn-process-outcome.spec.ts` 8 passed |
| `n3n6-02-green-n6.log` | 修复后：`src/transport/adapter.spec.ts` 回放用例 passed |
| `n3n6-03-reverse-n3.log` | 反向验证：把结局判定退回「三处各判」（`turn-error` 重新吞 `max-tokens`、顶部默认分支），3 failed / 5 passed |
| `n3n6-03-reverse-n6.log` | 反向验证：回放退回只认 `[exit code: N]`，1 failed |
| `n3n6-04-green-restored.log` | 恢复修复后：8 passed |
| `n3n6-typecheck.log` | `pnpm typecheck` 通过（无输出即通过） |
| `n3n6-vitest-full.log` | `pnpm test`（已排除 `scratch/**`）：506 passed |
| `n3n6-cargo-test.log` | `cargo test`：lib 1148 passed / 4 ignored、集成 25 passed |
| `n3n6-clippy.log` | `cargo clippy --all-targets -- -D warnings` 通过 |
| `n3n6-check-deps.log` | `check-deps.ps1`：0 违规 |
| `n3n6-check-doc-claims.log` | `check-doc-claims.ps1`：A 类缺口 0 / B 类 5（既有） |
| `n3n6-gate-exit-codes.txt` | 三个 TS 门禁退出码：typecheck / check-deps / check-doc-claims 均 0 |

**仍未核实**：N3/N6 的界面口径只在单测与回放层证明；真机重跑后才能把
`docs/smoke/2026-11-17-9af8431/REPORT.md` 的 N3/N6 改判 PASS。

## 追加：0.2.9 真机冒烟 P2 步数口径（N4 导出丢步 / N5 重开多一步）

基准提交 `efa2681`（本批在其上做，`4421ee3` 已包含）。

**N5 结论：已被 `4421ee3` 覆盖，本批不重复改。**
该笔把回放 `closeStep` 加上 `stepHasContent` 门槛（空 step 不计数），并锁住重复重开。
证据：反向对照 `n2-steps-red.log`（旧实现重开 `steps: 3, turns: 2`）与修复后
`n2-steps-green.log`（`steps: 2, turns: 1`），用例
`N2：审批挂起时强杀——重开留「已失效」记录、不造假卡、步数与实时一致`。
N5 报的症状正是「挂起强杀 → 重开多一步」，所以重开口径不再动。

**N4 根因**：导出只读 `load_session` 的投影条目，投影只带回合结局的总步数、不记每条
属于第几步；导出于是丢掉 `turn/end.iterations`、把每条 `step` 写死 1。真机 04 的第 1 步
只有工具调用、没有助手正文，也不能靠消息条数反推。

**N4 修法（真值在宿主还原，导出侧不猜）**：

- `project_slots_with` 按迭代边界还原：`agent_loop` 每个采样迭代先写模型侧条目
  （助手消息 / 推理 / 工具调用），再写该批全部输出，所以「输出之后再出现的第一个模型侧
  条目」就是下一步；`StepCursor` 只在这时 +1（0 起，与实时 `step/start` 同码）。
- 还原值必须等于结局行 `iterations` 才保留；结局缺 `iterations`、与还原值对不上、
  或整回合没有结局行，都把本回合已填 `step` 清回 `None`。
- `SessionEntry.Message/ToolCall` 透传可选 `step`（旧档线形状不变，`step` 键不写）；
  重开带 `step` 时按它对齐，缺省才退回旧的「按助手消息切步」口径。
- 导出 `turn/end` 写 `iterations`，条目写真实 `step`；缺 `iterations` 或步号个数与
  `iterations` 不符时写 `stepsUnavailable: true`，绝不补数。

**用例**：

| 文件 | 内容 |
|---|---|
| `rust-n4-red.log` | 反向对照一：Rust 推导退回「步号恒 0」，投影用例 1 failed |
| `rust-n4-green.log` | 修复后：`cargo test --lib projection_` 3 passed |
| `n4-red.log` | 反向对照二：导出退回 `step: 1` 且不带 `iterations`，3 条 N4 用例红 |
| `n4-green.log` | 修复后：N4 用例 3 passed（含四处一致与重复重开） |
| `n4-cargo-test-full.log` | `cargo test` 全量：lib 1150 passed / 4 ignored，集成全绿 |
| `n4-clippy.log` | `cargo clippy --all-targets -- -D warnings` 通过 |
| `n4-typecheck.log` | `pnpm typecheck` 通过 |
| `n4-vitest-full.log` | `pnpm test`（已排除 `scratch/**`）：45 files / 509 passed |
| `n4-check-deps.log` | `check-deps.ps1`：0 违规 |
| `n4-check-doc-claims.log` | `check-doc-claims.ps1`：A 类缺口 0（B 类为既有项） |

**仍未核实**：真机重跑（多步会话导出、挂起强杀重开）尚未执行；本批只证明投影/导出/重开
三处口径与用例，`docs/smoke/2026-11-17-9af8431/REPORT.md` 的既有结论不改，N4/N5 仍以
重跑为准。

---

## 0.2.9 真机冒烟收尾批（N7 / N8 / N9，2026-11-17）

来源：`docs/smoke/2026-11-17-9af8431/REPORT.md` 的「新增问题 N7/N8/N9」与
`evidence/03-prune-compaction.txt`、`evidence/F4-patch.txt`。三条都是「界面/回执说了与事实
不符的话」，判据与冒烟既有结论一律未改。

### N7（P2）裁剪没有留痕 → 界面/导出把完整原文冒充成「模型看到的」

**根因**：D6 裁剪只改运行态 `history`，不改档案，也不发任何事件。档案里永远是工具输出
原文，于是重开与导出只能看见全文；`tool_call_output_delta` 也早于裁剪发生。旧规范
（docs/04 §6.4）甚至把「不需要新增事件类型」写成了设计意图——这正是问题的来源。

**修法**（偏离 dsh 的 log-only `compaction/prune`，理由见下）：
- `compaction::PruneStats` 增加逐条 `details`（`call_id / original_chars / head / tail`），
  `prune_tool_outputs` 不再只报聚合条数。
- 新增事件 `tool_output_pruned`（additive）：运行中的界面据此落一条开发者通知。
- 新增档案项 `ToolOutputPruned`（append-only，不进模型历史）：`load_session_full` 按
  `call_id` 挂成 `SessionEntry::ToolCall.pruned`，重开与导出据此判读；同一 id 多条取最后。
- 前端 `src/transport/prune-marker.ts` 一份文案给三处共用（重开卡片、实时通知、导出）。
- **口径选择**：档案正文保留**裁剪前原文**，前面加判读行
  `[已裁剪] … 原始 N 字符，保留头 A + 尾 B，中段省略 M 字符。以下为裁剪前原文（完整）。`
  不选「导出改成省略版」——档案要保住可复查的原始输出；但只回显原文不标注同样不可接受。
  规则已写进 docs/04 §6.4 D6 与 §3.1/§1 的表里。
- 偏离 dsh 的理由：dsh 的 `compaction/prune` 是 log-only（不投影到界面），满足不了
  「界面与导出都能判读」这条验收；这里保留 log-only 的「不改原文」语义，只把留痕
  变成可投影的独立档案项。

### N8（P3）`apply_patch` 失败回执的「实际读到」取到了预演结果

**根因**：多块补丁在内存里逐块预演，`apply_hunk_seq` 匹配用 `file_lines`（预演后），
失败详情也拿它渲染，于是把预演出来的 `A3` 当成磁盘内容报回（磁盘仍是 `A2`）。

**修法**：`apply_hunk_seq` 增加 `disk_lines` 参数，`mismatch_detail` 一律取磁盘原文；
上下文不足时的行数也改口径为「磁盘文件共 N 行」，不再拿被前几块改短的行数当文件事实。

### N9（P3）写后读状态文案断言「未读取过」

**根因**：读取登记表按**本任务**创建，跨任务必然查不到；文案把「本任务没有记录」写成
「未读取过」。方向（拒绝写）没错，错在陈述了一件并不知道的事。

**修法**：文案改为
`{path} 在本任务内没有可用的读取记录（可能尚未读取，或上次读取发生在本次修改之前、内容可能已过期）`，
不再断言「没读过」。判据未动（仍拒绝），只改措辞；同时重导出了
`evals/repo/patches/repo-bug-apply-patch-read-guard.patch` 使评估补丁仍可应用。

**用例与反向验证**：

| 文件 | 内容 |
|---|---|
| `n7n8n9-rev-n7-rust.log` | 反向一：去掉 `details.push` / 投影恒 `None` → 2 failed（N7 后端） |
| `n7n8n9-rev-n7-fe.log` | 反向二：去掉导出/重开的 `decorateToolOutput` → 2 failed（N7 前端） |
| `n7n8n9-rev-n8.log` | 反向三：`mismatch_detail` 退回 `file_lines` → 1 failed，回执复现「实际读到 A3」 |
| `n7n8n9-rev-n9.log` | 反向四：文案退回「未读取过 …」→ 1 failed |
| `n7n8n9-cargo-test.log` | 修复后 `cargo test -- --test-threads=1` 全量绿 |
| `n7n8n9-clippy.log` | `cargo clippy --all-targets -- -D warnings` 通过 |
| `n7n8n9-typecheck.log` | `pnpm typecheck` 通过 |
| `n7n8n9-vitest.log` | `vitest run --exclude "scratch/**"`：45 files / 512 passed |
| `n7n8n9-check-deps.log` | `check-deps.ps1`：0 违规 |
| `n7n8n9-check-doc-claims.log` | `check-doc-claims.ps1`：A 类缺口 0（B 类为既有项） |
| `n7n8n9-align-report.log` | `align-report --check`：退出 2，**环境不可比**（本地 DSH 检出不在冻结基线 fcc0ad46），与本批改动无关 |

**仍未核实**：真机未重跑。界面/导出是否真的能判读、真机裁剪是否触发，仍以
`docs/smoke/` 下次重跑为准；本批只证明代码路径与用例。

