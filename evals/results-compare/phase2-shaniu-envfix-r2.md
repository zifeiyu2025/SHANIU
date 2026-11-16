# SHANIU 臂整轮重跑（修复后的评测驱动器，干净分母）

- 运行：2026-11-16 15:19:42 → 16:03:50（外层总耗时 44m08s）
- 结果目录：`evals/results-compare/phase2-shaniu-envfix-r2/`（新建，未覆盖任何既有目录）
- 被测提交：`bb5e17c`（`05ccacc` + `55c1ae8` + `bb5e17c`，即环境泄漏修复后的驱动器）
- 判据：`evals/repo/judge/run.ps1` + `guards.json`，只认「退出码 0 且 stdout 含 `REPO-CHECK: PASS`」
- 题集：`evals/repo-tasks.json` 全部 12 题，`--repeats 1`

## 口径与可比性

| 项 | 本轮 |
|---|---|
| 并发 | **无**。单 driver（`run-arm.mjs` → 单进程 Rust 运行器）顺序跑，`.eval.lock` 全程独占 |
| 被测二进制 | 由 `scratch/clean-wt`（detached `bb5e17c`）构建，`git status` 0 行 |
| 外层 `CARGO_TARGET_DIR` | `scratch/clean-target`（只服务 harness 编译，按 `55c1ae8` 口径；被测 agent 在子进程边界被 `env_remove`） |
| 模型/协议 | `deepseek-flash` / `openai-chat`（SHANIU 臂既有口径） |
| 前置判据 | 12 题全部 `precondition_red=true`（补丁生效、判据区分） |
| invalid / tampered / timeout | 1 / 0 / 0 |

**为什么换 `scratch/` 跑**：主工作树有并行开发线的未提交改动（`src-tauri/src/sandbox/acl.rs` 等 3 个文件 + 1 个未跟踪文件），会进被测二进制。上一轮报告已把这一点列为偏差，本轮改在干净 worktree 上跑以拿到真正可归因的分母。该 worktree 是 `bb5e17c` 的 detached checkout，与主树 HEAD 同提交、不含任何 WIP。

## 逐题结果

| 题目 | 轮上限 | 本轮 | 轮 | wall(ms) | in / out | 改动 | 上轮（脏分母） |
|---|---|---|---|---|---|---|---|
| repo-bug-retry-after | 24 | PASS | 18 | 264,594 | 283,082 / 5,635 | 1 | PASS (14) |
| repo-bug-apply-patch-read-guard | 24 | PASS | 22 | 126,228 | 416,963 / 5,325 | 1 | PASS (18) |
| repo-bug-rollout-damaged | 24 | PASS | 15 | 104,438 | 199,389 / 3,209 | 1 | PASS (20) |
| repo-bug-compaction-tail | 24 | PASS | 12 | 122,966 | 342,841 / 4,735 | 1 | PASS (11) |
| repo-feat-context-overflow-code | 32 | **FAIL** | 32 打满 | 127,702 | 999,484 / 13,050 | 1 | PASS (32) |
| repo-feat-title-cap | 24 | PASS | 24 打满 | 196,247 | 549,455 / 6,628 | 1 | PASS (24) |
| repo-refactor-now-ms-exit | 24 | **PASS** | 15 | 168,429 | 208,106 / 4,067 | 1 | FAIL (24, 0 改动) |
| repo-refactor-rename-title-api | 20 | PASS | 14 | 117,444 | 257,993 / 4,140 | 1 | PASS (20) |
| repo-bug-search-surface-lost | 28 | **PASS** | 28 打满 | 156,933 | 1,134,000 / 10,354 | 3 | FAIL (28, 0 改动) |
| repo-bug-cut-pairing | 32 | **PASS** | 32 打满 | 320,414 | 1,639,072 / 22,994 | 3 | FAIL (32, 1 文件 E0308) |
| repo-doc-max-turns-drift | 12 | PASS | 12 打满 | 32,933 | 200,282 / 3,858 | 1 | PASS (11) |
| repo-doc-claim-after-rename | 16 | PASS | 13 | 201,747 | 300,721 / 3,217 | 2 | PASS (16) |

合计：**11 PASS / 1 FAIL**（上轮 9 PASS / 3 FAIL）。`invalid=0`、`tampered=0`、`timeout=0`。判据中位轮次 16.5，打断合计 60。

逐题变化：3 题 FAIL→PASS（`now-ms-exit`、`search-surface-lost`、`cut-pairing`），1 题 PASS→FAIL（`context-overflow-code`），其余 8 题保持 PASS。

## 三分法

| 分档 | 上轮（脏分母） | 本轮 |
|---|---|---|
| 真失败（有改动、判据红） | 1：`cut-pairing`（E0308） | 1：`context-overflow-code`（E0004） |
| 零改动收工（0 文件、判据红） | 2：`now-ms-exit`(24)、`search-surface-lost`(28) | **0** |
| 轮数打满但通过 | 4：`context-overflow`、`title-cap`、`rename-title-api`、`doc-claim` | 4：`title-cap`(24/24)、`search-surface-lost`(28/28)、`cut-pairing`(32/32)、`doc-max-turns-drift`(12/12) |
| 未打满即通过 | 5 | 7 |

「零改动收工」清零，是环境泄漏修复后最直接的读数变化：上轮这两题把轮次烧在 `CARGO_TARGET_DIR` 绕行上，本轮全部落到真实定位与修复。

## 环境核验（硬证据）

- `raw/rollouts/**/*.jsonl` 中 `CARGO_TARGET_DIR` 命中 **1 处**，且只是 `cut-pairing` 的 reasoning 文本，**没有任何工具调用携带该变量**（上轮同一项是 11 处、9 个绕行工具调用）。
- 400 类端点故障（`No tool output found` / `HTTP 400` / `4000103`）在 `raw/shaniu-run.log` 命中 **0 处**；事件流里 2 处命中均为题面/推理中提到的「HTTP 400」（`context-overflow` 题目本身语义），不是故障。
- 环境故障痕迹（`os error 112` / `os error 3` / `磁盘空间不足` / `robocopy`）在运行日志命中 **0 处**。
- 工作区 `src-tauri/target` 有真实写入（每题预热与 agent 的 `cargo` 都落在工作区默认目录），没有越过沙箱写外部 target。

## 墙钟与 token

| 指标 | 上轮 12 题（脏分母） | 本轮 12 题 | 变化 |
|---|---|---|---|
| agent 段合计 | 1,618,002 ms（27.0 min） | 1,372,137 ms（22.9 min） | -15.2% |
| 判据段合计 | 506,792 ms（8.4 min） | 567,938 ms（9.5 min） | +12.1% |
| wall 合计（agent+判据） | 2,124,794 ms（35.4 min） | 1,940,075 ms（32.3 min） | -8.7% |
| input tokens | 6,909,905 | 6,531,388 | -5.5% |
| output tokens | 82,421 | 87,212 | +5.8% |
| 总轮数 | 250 | 237 | -5.2% |

wall 口径与上轮一致（agent 段 + 判据段，不含每题准备/预热；本轮准备合计未计入，单题 setup 见 `raw/shaniu-run.log`）。跨臂不可直接比（SHANIU 走 chat，其余三臂走 Responses）。

## `repo-bug-search-surface-lost`：结论与证据

**结论：不是题目问题，上一轮的红是能力/轮次与同一环境泄漏叠加，本轮在干净驱动器上可解 —— PASS。**

- 本轮：28 轮打满（`budget_exhausted`）、**3 文件改动**（`session_query/documents.rs` +35/-5、`session_query/mod.rs` +14/-3、`session_query/query.rs` +2/-2）、判据 `REPO-CHECK: PASS`、wall 156,933 ms。
- 上一轮：28 轮打满、**0 文件改动**、判据红在 `src\session_query\tests.rs:765:5`「搜到已压缩的原文必须标注模型当前不再看到：确定端口为 3306」。
- 事件流（`raw/rollouts/repo-bug-search-surface-lost/repo-bug-search-surface-lost-r1.jsonl`，103 条）显示它这次**先读判据测试再定位**，按层修复：
  1. `read_file` 判据测试 `session_query/tests.rs` + `list_dir session_query`，随后通读 `documents.rs` / `mod.rs` / `query.rs` / `schema.rs` / `rollout/resume.rs` / `lib.rs`；
  2. `apply_patch` 在 `documents.rs` 补语义文档层标注（`shadowed_*` 一带，6 次补丁）；
  3. `apply_patch` 在 `mod.rs` 的持久化索引行读取处、`query.rs` 的 SQL 列（`current` / `persisted` / `live_doc` 相关 SELECT）补齐标注（各 4 次补丁）；
  4. 中途 `cargo test --lib session_query::` 验证两轮，最后 `cargo check --all-targets` **exit 0**（9.5s）后收口。
- 卡点与上轮不同：上轮「0 改动」= 没找到落点；本轮找到了落点并改对，但 28 轮预算刚好用尽（收口 `budget_exhausted`，不是放弃）。这题属于「轮数打满但通过」，轮次预算偏紧，题目与判据本身区分正常（前置必红、后置可绿）。

## `repo-feat-context-overflow-code` 回归归因

32 轮打满、1 文件（`src-tauri/src/model_client/error.rs` +139/-1）、判据红在编译错误 `E0004`（非穷尽 match），判据尾巴原文：

```
251 ~         ModelErrorKind::Unknown => (KeyCheckState::Unknown, "unknown", error.human.clone()),
252 ~         ModelErrorKind::ContextOverflow => todo!(),
    |  For more information about this error, try `rustc --explain E0004`.
 error: could not compile `shaniu` (lib) due to 1 previous error
```

它新增了 `ModelErrorKind::ContextOverflow` 变体，却在一个 match 里漏了 arm（另一处 match 是否补齐未在判据里体现，判据只报了第一处 `E0004`）。32 轮内没收敛到可编译状态。这是**真失败**，不是环境问题：该题本轮 `CARGO_TARGET_DIR` 绕行 0 处、400 故障 0 次、前置判据正常红。上轮该题 PASS（同样 32 轮打满），属于模型在预算边缘的稳定性差异，不是驱动器变化导致的系统性回归。

**取证订正（2026-11-16，只读复核事件流）**：

- 判据里那行 `ModelErrorKind::ContextOverflow => todo!()` **不是模型写进代码的内容**，是 rustc 对 `E0004` 的补 arm 建议（`251 ~` / `252 ~` 两行相邻的 `~` 是建议格式）。事件流里 `todo!` 只出现在 3 条 `verification` 输出中，14 次 `apply_patch` 的补丁正文里一次都没有。所以「占位符留在生产代码」不是本次事故的成因，`cargo check` 报的始终是缺 arm。
- 该题收口是 `turn_completed.reason = failed`（`turn_error.code = VERIFY_FAILED`），**不是 `budget_exhausted`**：32 轮打满后宿主在统一收口点补跑了 `cargo check --all-targets`（exit 101），据此把收口改成 `failed`。`raw/shaniu-run.log` 该题为 `[agent] end ... reason=Failed`、`收口=Failed`。
- 闸门**跑过且报了红**，没有漏判：整题只有 1 条 `verification_result`（turn 32），失败原因是模型 45 次工具调用（14 `apply_patch` / 15 `search` / 12 `read_file` / 4 `list_dir`）里 0 次命令调用，直到第 32 轮仍在 `apply_patch`，循环内闸门（只在「本轮不要工具」时触发）一次都没被问到。缺的是「悬崖之前的裁决」，不是「裁决错了」——已按此补「截止检查点」（见 `docs/04-协议规范.md` §截止检查点）。


## r1 作废说明

`phase2-shaniu-envfix-r1/` 只跑成 2 题（`retry-after` PASS、`apply-patch-read-guard` FAIL）后，题 3 起 10 题全部 `INVALID`：子进程报 `子进程读不到任务集: Os { code: 3, kind: NotFound }`。核查确认不是驱动器缺陷 —— 当时用的 `%TEMP%\shaniu-evals-clean-wt` 被外部清理器删掉了 `.git` 与 `evals/repo*` 等**只读、未写过**的文件（只剩 `evals/results`、`src`、`src-tauri`、`target` 四个壳），与 `run-arm.mjs` 注释里记录的 `%TEMP%` 只读目录被判定陈旧删除是同一现象。r1 的两题结果**不进分母**，目录保留作废证据。

## 回收与取证

回收（实测，`Get-PSDrive` 前后）：

| 路径 | 体量 |
|---|---|
| `%TEMP%\shaniu-evals-repo-ws-9200`（复用工作区，19,916 文件） | 15.85 GB |
| `scratch/clean-target`（harness 独立 target，4,113 文件） | 4.32 GB |
| `scratch/clean-wt`（worktree，含 pristine/children/rollouts 已归档） | 0.13 GB |
| 合计 | **20.30 GB** |

C 盘 75.24 → 90.57 GB（+15.33），D 盘 595.23 → 599.42 GB（+4.19），实测释放 **19.52 GB**（与统计值的差额来自删除期间其他进程/缓存，不取高值）。`git worktree list` 只剩主树；`%TEMP%` 无 `shaniu-*` 残留；r1 残骸 7.42 GB 已在重跑前回收。

保留的证据（不删）：

- `summary.json`、`progress.log`（注：SHANIU 臂走 Rust 运行器不调用 `logProgress`，该文件是跑完后按 `summary.json` 回填的，逐题实时时刻以 `raw/shaniu-run.log` 为准）、`run.log`、`1794816230538-9200-repo.json`（Rust 侧原始报告）
- `raw/shaniu-run.log`（逐题时刻、轮次、setup/check、收口原因）、`raw/shaniu-stdout.log`
- `raw/rollouts/`（12 题事件流，1.67 MB）、`raw/children/`（每题 outcome/spec/child 日志）
- `phase2-shaniu-envfix-r1/`（作废轮原件）
