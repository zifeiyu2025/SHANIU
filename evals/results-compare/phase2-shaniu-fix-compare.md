# SHANIU 臂：修复前 vs 修复后（`a09977b` 修 400）

- 修复前：`evals/results-compare/phase2-shaniu/`（2026-11-15 21:31 收口，12 题 9 PASS / 3 FAIL）
- 修复后：`phase2-shaniu-a09977b/`（10:38 起跑，10:52 中断，已出 3 题）+ `phase2-shaniu-a09977b-r2/`（13:24:48–13:59:48 续跑剩余 9 题）
- 被测改动：`a09977b fix(model_client): Responses 工具调用补齐配对输出，修 400`
- 判据：`evals/repo/judge/run.ps1` + `guards.json`，只认「退出码 0 且 stdout 含 `REPO-CHECK: PASS`」

## 口径与可比性

| 项 | 修复前 | 修复后 |
|---|---|---|
| 题数 | 12（`--repeats 1`） | 12（3 题复用 + 9 题新跑） |
| 并发 | 无，单 driver 顺序 | 无，单 driver 顺序 |
| 模型/协议 | `deepseek-flash` / `openai-chat` | 同左 |
| 被测二进制 | 工作树当时构建 | `evals_repo-0d55e89eb29d0b0d.exe`，mtime **10:39:47**，13:24 续跑时**未重建** |
| 独立 CARGO_TARGET_DIR | `target\shaniu-phase2` | `target\shaniu-a09977b`（收尾已删） |

- 续跑的 9 题与中断轮的 3 题是**同一份二进制、同一棵树**：`git` HEAD=`a09977b`，`src-tauri` 自 10:37:29 起无改动；中断轮工作区与预置快照闲置在同一路径，pid 19172 的 `outcome.json` 原样复用（见 `phase2-shaniu-a09977b-r2/raw/prior/`）。
- **必须在报告里点出的偏差**：工作树带一处不属于本次改动的未提交修改 `src-tauri/src/sandbox/acl.rs`（417+/87-，10:37:29 落盘，早于两段开跑）。它进了被测二进制，两段一致，所以本对照内部可比；但它不是 `a09977b` 的一部分。
- `wall` = agent 段 + 判据段（Rust 运行器口径，不含每题准备/预热；准备均值 57s/题，未计入）。
- 墙钟只在**本臂内**可比（同机同日、同二进制、顺序无并发）。跨臂不可直接比：codex/dsh/zcode 走 Responses、SHANIU 走 chat，见 `report.md`「协议差异」。

## 逐题对照

| 题目 | 轮上限 | 修复前 | 轮 | wall(ms) | in/out | 改动 | 修复后 | 轮 | wall(ms) | in/out | 改动 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| repo-bug-retry-after | 24 | FAIL | 3 | 93,454 | 19,919 / 474 | 0 | **PASS** | 14 | 153,404 | 178,753 / 3,164 | 1 |
| repo-bug-apply-patch-read-guard | 24 | PASS | 11 | 79,147 | 175,455 / 1,874 | 1 | PASS | 18 | 169,244 | 549,842 / 4,157 | 1 |
| repo-bug-rollout-damaged | 24 | PASS | 17 | 124,543 | 438,891 / 2,921 | 1 | PASS | 20 | 222,175 | 528,864 / 5,502 | 1 |
| repo-bug-compaction-tail | 24 | PASS | 20 | 132,928 | 325,870 / 4,450 | 1 | PASS | 11 | 213,495 | 216,300 / 3,603 | 1 |
| repo-feat-context-overflow-code | 32 | PASS | 32 | 159,706 | 1,207,064 / 12,750 | 5 | PASS | 32 | 175,201 | 977,827 / 9,396 | 5 |
| repo-feat-title-cap | 24 | PASS | 24 | 221,802 | 397,794 / 5,219 | 1 | PASS | 24 | 232,917 | 502,839 / 6,461 | 1 |
| repo-refactor-now-ms-exit | 24 | PASS | 19 | 177,811 | 321,322 / 4,248 | 1 | **FAIL** | 24 | 161,260 | 464,046 / 4,042 | 0 |
| repo-refactor-rename-title-api | 20 | PASS | 14 | 132,412 | 178,314 / 3,422 | 1 | PASS | 20 | 78,416 | 264,924 / 4,484 | 1 |
| repo-bug-search-surface-lost | 28 | FAIL | 4 | 38,168 | 22,067 / 513 | 0 | FAIL | 28 | 266,609 | 1,168,828 / 7,400 | 0 |
| repo-bug-cut-pairing | 32 | PASS | 32 | 314,829 | 1,603,464 / 19,441 | 3 | **FAIL** | 32 | 202,885 | 1,419,950 / 26,077 | 1 |
| repo-doc-max-turns-drift | 12 | PASS | 12 | 25,592 | 173,859 / 2,637 | 1 | PASS | 11 | 34,192 | 168,812 / 3,576 | 1 |
| repo-doc-claim-after-rename | 16 | FAIL | 4 | 96,082 | 57,667 / 714 | 0 | **PASS** | 16 | 214,996 | 468,920 / 4,559 | 2 |

合计：修复前 **9 PASS / 3 FAIL**，修复后 **9 PASS / 3 FAIL**；总数持平，**构成变了**（2 题救回、2 题回归、1 题仍红）。`invalid=0`、`tampered=0`、`timeout=0`。

## 三分法

| 分档 | 修复前 | 修复后 |
|---|---|---|
| 真失败（有改动、判据红） | 0 | 1：`repo-bug-cut-pairing`（1 文件，`E0308` 未修完即打满 32 轮） |
| 零改动收工（0 文件、判据红） | 3：`retry-after`(3 轮)、`search-surface-lost`(4)、`doc-claim-after-rename`(4) | 2：`now-ms-exit`(24 轮打满)、`search-surface-lost`(28 轮打满) |
| 轮数打满但通过 | 4：`context-overflow`(32/32)、`title-cap`(24/24)、`cut-pairing`(32/32)、`doc-max-turns-drift`(12/12) | 4：`context-overflow`(32/32)、`title-cap`(24/24)、`rename-title-api`(20/20)、`doc-claim-after-rename`(16/16) |
| 未打满即通过 | 5 | 5 |

修复前的 3 个「零改动」全部是 400 `No tool output found for tool call …` 在第 3~4 轮把轮次打断（`in` 只有 2 万上下）。修复后同样三题：

- `retry-after` FAIL→PASS（14 轮、1 文件）；
- `doc-claim-after-rename` FAIL→PASS（16 轮、2 文件）；
- `search-surface-lost` 仍 FAIL，但**不再是 3 轮早退**：28 轮打满、116.9 万 input、0 改动，判据红在 `session_query::tests::documents_mark_compacted_originals_as_shadowed` 与 `searching_a_compacted_original_says_the_model_no_longer_sees_it`。按本轮口径这是「零改动收工」，不是端点故障。

本轮 9 题全程无 400：`target\evals-repo-rollouts-*\*.jsonl` 中无 `No tool output found` / `HTTP 400` 命中。

## 墙钟与 token 汇总

| 指标 | 修复前 12 题 | 修复后 12 题 | 变化 |
|---|---|---|---|
| agent 段合计 | 981,663 ms（16.4 min） | 1,618,002 ms（27.0 min） | +64.8% |
| 判据段合计 | 614,811 ms（10.2 min） | 506,792 ms（8.4 min） | -17.6% |
| wall 合计 | 1,596,474 ms（26.6 min） | 2,124,794 ms（35.4 min） | +33.1% |
| input tokens | 4,921,686 | 6,909,905 | +40.4% |
| 其中 cached | 4,619,904 | 6,501,248 | +40.7% |
| output tokens | 58,663 | 82,421 | +40.5% |
| 总轮数 | 192 | 250 | +30.2% |

总量涨是**预期内**的：修复前 3 题在第 3~4 轮就被 400 掐断，几乎不耗墙钟和 token；修复后这些轮次被真正用满。按题比更有意义，例如 `search-surface-lost` 从 38s/4 轮变成 267s/28 轮——同一道题现在才真正跑起来。

## 两个回归的归因（不作美化）

1. `repo-refactor-now-ms-exit` PASS→FAIL：24 轮打满、**0 改动**，判据红在 `src_has_exactly_one_now_ms`（期望 `src/clock.rs` + `src/session_query/schema.rs`，实得只有未改动的 `src/clock.rs`）。
2. `repo-bug-cut-pairing` PASS→FAIL：32 轮打满、1 文件，判据红在编译错误 `E0308`（`compaction.rs:606` 附近 `tail_target(...)` 返回 `Target` 而调用处按 `Option<Target>` 用）。

这两题**不能归因于 `a09977b` 本身**。两段（以及修复前那轮）都带着同一个环境泄漏：外层用独立 `CARGO_TARGET_DIR` 编译 harness 时，该变量被子进程继承，agent 在自己的命令里 `cargo` 会去写工作区之外的 target；沙箱拒绝后 agent 反复绕行（rollout 的 `prompt_attribution` 里可见 `set CARGO_TARGET_DIR=%CD%\..\target\shaniu-build` 一类命令；`now-ms-exit` 3 处、`cut-pairing` 2 处）。这个泄漏所有臂、所有轮都有，属于既有偏差，不是本轮新增，但它确实吃掉轮次预算——`feat-title-cap`/`doc-max-turns-drift` 在 11-15 的零改动分析里也被点过名。

### A/B 定论（2026-11-16 14:21–14:33，`phase2-ab-envfix/`）

- 泄漏源头（代码位置）：[run-arm.mjs](../../compare/run-arm.mjs) 用 `...process.env` 拼 env、[evals_repo.rs](../../../src-tauri/tests/evals_repo.rs) 拉子进程整份继承。agent 在 `now-ms-exit` 事件流第 46 行自己打出 `CARGO_TARGET_DIR=[D:\MyWorkspace\SHANIU\target\shaniu-a09977b] [exit code: 0]`。
- 修复：`05ccacc`，只在评测侧改两处——`childEnv()` 统一 `delete env.CARGO_TARGET_DIR`（四臂共用），`evals_repo.rs` 子进程 spawn 处 `.env_remove("CARGO_TARGET_DIR")` 兜底。判据侧 `run.ps1` 自设该变量，不受影响。
- 复跑条件：干净工作树 `%TEMP%\shaniu-ab-wt`（detached 在 `05ccacc`，`git status` 0 行，被测路径无未提交改动）；外层**故意继续设** `CARGO_TARGET_DIR=%TEMP%\shaniu-ab-target` 做反向验证。
- 范围收窄 + 端到端复验：`05ccacc` 第一版连 harness 自身的 `cargo test` 也摘了变量，导致外层编译丢了独立 target（A/B 的 worktree 因此涨到 4.45 GB）。`55c1ae8` 改回「只摘被测 agent、外层保留」，并在该提交的干净工作树上再验一次：`repo-doc-max-turns-drift` PASS（12 轮 / 1 文件），harness 产物落在独立 target（4.32 GB，exe 在 `debug\build\shaniu\<hash>\out\`），主树 `src-tauri\target` mtime 仍停在 11-13 17:50、未被改动，agent 事件流 `CARGO_TARGET_DIR` 提及 0。

| 题目 | 泄漏轮（本轮 9 题） | 修复后 A/B | 绕行工具调用 | 判据 |
|---|---|---|---|---|
| `repo-refactor-now-ms-exit` | FAIL，24 轮打满，**0 改动** | **PASS**，23 轮，1 文件（+2），收口 Completed | 7 → **0** | REPO-CHECK: PASS |
| `repo-bug-cut-pairing` | FAIL，32 轮打满，1 文件 `E0308` | **PASS**，32 轮，3 文件（+113），收口 BudgetExhausted | 4 → **0** | REPO-CHECK: PASS |

- 两题事件流里 `CARGO_TARGET_DIR` 提及 **0 次**（泄漏轮合计 11 处、9 个绕行工具调用）；工作区内 `src-tauri/target` 生成 5.26 GB，证明预热与 agent 的 `cargo` 都落回工作区默认目录。A/B 全程无 400（`No tool output found` / `4000103` / `HTTP 400` 命中 0）。
- 判定：**两处回归都是环境泄漏导致的假回归**，不是模型改不对；`a09977b` 与 SHANIU 的改写能力都不该背这个锅。`cut-pairing` 修复后仍是 32 轮打满才通过，属于「轮数打满但通过」，与修复前同档。

结论：本轮只能回答「400 修没修好」，答案是**修好了**；「SHANIU 能力是否提升」这个问题的分子被上述两处噪声污染，不能从 9/12 → 9/12 直接读数。泄漏已在 `05ccacc` 修掉并用这两题验证，其余 10 题尚未在修复后的驱动器上重跑。

## 遗留与下一轮建议

1. ~~把 `CARGO_TARGET_DIR` 的作用域收窄到 harness 外层编译~~ **已处理**：`05ccacc` 两层拦截 + 上述 A/B 验证。剩余 10 题若要有干净的能力对照，需用同一驱动器整轮重跑（本次只验了回归的两题）。
2. `src-tauri/src/sandbox/acl.rs` 的未提交改动在并行线上；本次 A/B 已绕开它（改用 `%TEMP%` 的 detached worktree），但那意味着 A/B 的二进制不含该 WIP，与 12 题那轮不是同一份二进制——跨轮比较时要注意这一点。
3. `repo-bug-search-surface-lost` 两轮都红：修复前是 400 早退，修复后是 28 轮真跑仍红，说明是**模型能力/题目本身**的失败，值得单独看它的 rollout。

## 取证清单

- 续跑 9 题：`phase2-shaniu-a09977b-r2/summary.json`（入库）、`progress.log`（逐题时刻+结论，按运行日志 epoch 换算）、`raw/shaniu-run.log`、`raw/shaniu-stdout.log`、`../phase2-shaniu-a09977b-r2.run.log`。后四者是 `*.log`，按仓库 `.gitignore` 不入库，只在本机磁盘留档。
- 复用 3 题（原样归档，未重跑）：`phase2-shaniu-a09977b-r2/raw/prior/` 下 `outcome.json` / `spec.json` / `child.log` / `*.jsonl`（事件流）
- 中断轮原件：`phase2-shaniu-a09977b/`（`progress.log`、`raw/shaniu-run.log`）保持不动
- 回收：`%TEMP%\shaniu-evals-repo-ws-12428`（9.40 GB）、`target\evals-repo-pristine-12428`（0.06 GB）、`target\evals-repo-pristine-19172`（0.06 GB）、`target\shaniu-a09977b`（4.64 GB），合计 14.16 GB；C 盘 80.81→89.68 GB，D 盘 546.25→550.74 GB。保留 `target\evals-repo-rollouts-12428`（1.24 MB）与 `-19172`（0.45 MB）作事件流证据。
- 中断轮自己的临时工作区（`shaniu-evals-repo-ws-19172`，13:22 实测 4.74 GB）在本轮收尾前已被外部清理掉，未计入回收量。
- A/B（`phase2-ab-envfix/`）：`summary.json`、`raw/shaniu-run.log`、`raw/shaniu-stdout.log`、`raw/children/`（两题 outcome/spec/child）、`raw/rollouts/`（两题事件流）。
