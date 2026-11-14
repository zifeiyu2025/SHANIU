# N5（P2）压缩提示实时显示 — 规格 v2（真机返工）

> v1（`4a8d0be`）的口径与实现已被真机否决，本文件取代 v1 的 §2/§3/§4；v1 §1 的链路定责仍然成立（后端已广播、适配层已接收），错的只是"发出什么形状"和"何时发出"。
> 范围：`src/transport/`（适配层 + 用例）与 `src-tauri/` 三笔 additive；不改 `src/dsh/` vendored 代码。

## 0. 为什么 v1 单测绿、真机红

v1 的用例是**自造序列**：先 `prompt`（或夹具预置 surface 事件）再发 `compaction_*`，于是 `pushCompactionCheckpoint` 读到的 `journal.surfaceSeqs` 一定非空，`replace` 一定命中。它证明的只是"有 surface 时能替换"，而真机要证明的是"压缩发生的那一刻能不能自己立起锚点"。真机路径里 `surfaceSeqs` 为空（或替换被消费端整帧丢弃）时，`pushCompactionCheckpoint` 静默 `return undefined`，用例与真机就此分叉：

| 真机现象 | 直接位置 | 本质 |
|---|---|---|
| 实时无节点、连摘要都没有 | `src/transport/adapter.ts` `pushCompactionCheckpoint` 的 `startSeq === undefined → return undefined` | 唯一出口无声，没有降级 |
| 重开只有一条 notice | `adapter.ts` `replayArchive` 的 `entry.kind === 'compaction_summary'` 分支 | 档案首条即锚点 → surface 空 → 退回通知 |
| 手动 `/compact` 铺原始摘要 | `adapter.ts` 手动 `/compact` 执行段把命令返回的摘要当普通 `user/message` append | 手动路径根本没走 checkpoint |

v1 还漏了一件事：实时与回放**只有"有 surface"时才同形**，而真机的两种失败恰好都是 surface 为空。

## 1. 第一步：用 r4 真实事件复现（必须先红）

**禁止自造事件。** 夹具逐字节取自真机证据，只在测试内做类型断言，不改写任何字段值。

新增 `src/transport/fixtures/r4-compaction.ts`（测试专用，非产品代码）：

1. `R4_HOST_EVENTS`：从 `docs/smoke/2026-11-14-58f56f6/r4/evidence/r4-events-compaction.txt` 原样摘录的宿主事件 JSON 行（主实例 4 次压缩 + 失败实例 1 次），每行附来源行号注释；用 `as EventMsg` 断言，不做字段补全。
2. `R4_EXPORT_JOURNAL`：`evidence/live-export-session-log.zip` 解出的 `session.v4.jsonl` 的 53 条真实 journal 事件（原样），用于「会话已有真实历史」的基线。
3. `R4_ARCHIVE_ENTRY`：`evidence/r4-rollout-analysis.txt` 里真实 `compaction_summary` 条目的字段（`text`/`range_start`/`range_end`），用于重开路径。
4. 文件头注释写明证据路径、被测提交 `9d0cfd9`、采集批次 `58f56f6`。

`src/transport/adapter.spec.ts` 新增 3 条用例（复用现有 fake bridge / `follow` 支架）：

- **R1 实时（真实事件）**：把 `R4_HOST_EVENTS` 按原顺序 `bridge.emit` 进同一会话，断言 journal 里
  ①恰 1 条 `user/message` 且 `source.kind === 'compact-checkpoint'`；
  ②该事件过 `assertSessionWireEvent`；
  ③`compactionDefinition.buildViewNode` 产出 `kind: 'compaction'` 且卡片摘要非空；
  ④不存在 `source.kind === 'compaction-notice'` 的 `developer/message`。
- **R2 重开（真实档案）**：用 `R4_ARCHIVE_ENTRY` 走 `replayArchive`，断言 ①产出 ≥1 条 checkpoint、②0 条 `compaction-notice`、③每条 checkpoint 都能被 `compactionDefinition` 认成节点。
- **R3 手动（真实手动事件）**：`R4_HOST_EVENTS` 里的 `trigger: 'manual'` 段 + 命令返回，断言恰好 1 条压缩节点、0 条「原始摘要正文」形态的普通 `user/message`（判据：无 `source.kind` 且 `content` 含 `## 用户原文`）。

**验收门槛**：先在未改代码上跑这 3 条，必须红；把红输出（用例名 + 断言差异）记入报告。若 R1 在现有代码上意外为绿，说明夹具没有命中真机分支，必须回到证据重取，不得改断言迁就。

## 2. 修复决策

### D1 `pushCompactionCheckpoint` 永不无声（`adapter.ts`）

签名不变（`journal, compactionId, summary, hiddenSeqs`），内部分两支：

- `journal.surfaceSeqs` 非空：保持 replace，`startSeq = surfaceSeqs[0]`、`endSeq = 末项`，`sourceEventSeqs = [...surfaceSeqs, ...hiddenSeqs 去重]`。
- **为空**：发 `surfaceOp: 'append'` 的 checkpoint（上游 `buildViewNode` 只要求 `state.checkpoint` 存在，append 型 checkpoint 同样是可见节点），并写一条 `console.warn`：`[shaniu] 压缩节点降级为 append：journal.surfaceSeqs 为空（compactionId=…）`。

理由：这是"实时路径自己产出可替换锚点"的最小实现——锚点就是这条 checkpoint 本身，后续 surface 事件顺序 append 在它之后，不再依赖"event 先到、surface 后到"的时序巧合。返回值不再可能为 `undefined`，调用点无需再判空。

实现时确认 `validateSurfaceMetadata` 对 append 型是否允许携带 `sourceEventSeqs`；若要求 append 必须不带，则 append 分支省略该字段（只留 `surfaceOp: 'append'`）。

顺带删掉 `pushCompactionNotice` 在成功/回放路径的调用（失败路径保留）。

### D2 回放与实时同形（`replayArchive`）

`entry.kind === 'compaction_summary'` 分支改为：`compaction/start` → `compaction/summary` → `pushCompactionCheckpoint`（不再判断返回值、不再发 notice）。档案首条即锚点时走 D1 的 append 分支，重开后看到的就是节点而不是通知。

### D3 手动 `/compact` 走 checkpoint（`adapter.ts` 命令执行段）

- 删除"把 `compact_session` 返回的摘要作为普通 `user/message` 推入会话区"的代码路径（真机第 3 项的直接原因）。
- `compaction_ended` 成功分支**去掉 `trigger !== 'manual'` 排除**：手动也发 checkpoint，与自动同口径。
- checkpoint 的 `source` **不写 `sourceCommandId`**：上游 `compactionDefinition.match` 对带 `sourceCommandId` 的 checkpoint 显式 `return null`（手动压缩另有命令卡）；写空即等价于"自动口径"的节点，符合"同口径"要求，也不伪造上游语义。
- 去重：同一 `compactionId` 只发一条，命令返回与事件流都到时不产生第二条。

### D4 不改 vendored 代码

`src/dsh/*` 只读。若实现中发现上游契约确实无法承载 append 型 checkpoint，停止并回报，不在 vendored 里打补丁。

## 3. 三笔 additive（单独一笔提交）

- **A1 `CompactionEnded` 补 `trigger`**：`src-tauri/src/events.rs` 变体加 `trigger: CompactionTrigger`（与 started/summarized 同型），所有发出点（自动/手动/失败）都填；`src/transport/types.ts` 对应字段设为必填；`adapter.ts` 改用 `msg.trigger`，删掉 `CompactionTrace.trigger` 的兜底。事件顺序/失败用例同步更新期望。
- **A2 导出补压缩事务**：结论先写清——`session.v4.jsonl` 里 compaction 为 0 是**导出侧主动跳过**（`src/header-actions/session-events.ts` 的 `sessionEntriesToEvents` 明确跳过 `kind === 'compaction_summary'`，属 N3 的"不把摘要伪装成用户发言"），不是事件没落档（宿主 events 里 9 条齐全）。修法：在锚点条目的位置补三条 **log-only** 事件 `compaction/start`、`compaction/summary`（`summary` 取 `entry.text`、`shadowedTokenCount` 不填）、`compaction/end`（`ok: true`），id 用 `shaniu-export-compaction-${index}`；不新增任何 surface 事件，历史用户回合保持不变。`turn`/`at_ms` 能取到就取，取不到不伪造。
- **A3 手动压缩补日志**：在手动 `compact_session` 路径写与自动同一条「上下文已压缩（SH-032）」日志行（含 `trigger=manual` 与条数），位置与文案对齐自动路径。

**A1/A2 的参照与取舍**：A1 取 dsh `fcc0ad4` 的 `repair.ts` 口径——结局事件自己带足以独立判读的字段，不靠消费端从上下文补推（故前端删掉 trace 副本）。A2 取 dsh「不猜、不补」：`compaction/summary` 只写档案里确有的正文，`shadowedRange`/`shadowedSeqs` 在档案里是历史条目下标而非 surface seq，`shadowedTokenCount`/`provider`/`model` 无可信来源，一律留空。已知代价：该导出档案若被 DSH 当原生 v4 导入做关系校验，会因子段缺失报错（`relationships.ts` 的 `span()` 强制要求）；SHANIU 导出链路不做该校验，且伪造比缺失更坏，故按缺失落地。

## 4. 回归与反向验证

正向：R1/R2/R3 + 现有压缩用例（通知→checkpoint、失败仍出声、重开不重复）+ 导出用例（每锚点 3 条事件、0 条伪用户消息）。

反向（必须红，记录命令与输出后恢复）：

1. 把 D1 空 surface 分支退回 `return undefined` → R1/R2 红；
2. 恢复手动/回放发 `compaction-notice`、恢复手动铺原始摘要 → R3 红；
3. 删除导出侧三条 log-only 事件 → 导出用例红。

## 5. 验证命令

`pnpm typecheck`；`pnpm exec vitest run --exclude "scratch/**"`；`cargo test -- --test-threads=1`；`cargo clippy --all-targets -- -D warnings`；`.\scripts\check-deps.ps1`；`.\scripts\dev\check-doc-claims.ps1`。Rust 只对改动文件跑 `rustfmt`（禁止 `cargo fmt`）。

## 6. 提交与发布

- 两笔提交：`fix(N5)`（复现用例 + D1–D3）、`chore(N5)`（A1–A3）。
- 只显式 `git add` 本任务路径；**不碰 `docs/smoke/`**（夹具是逐字摘录到 `src/transport/fixtures/`，不引用运行期 `docs/smoke/`）。
- 推前 `git fetch`，不带 `--force`；盯 gate + product-smoke 到结论再报；真机结案另行安排。

## 7. 风险

- replace 整段 surface 依赖"压缩发生在回合边界、被压缩区间即当前全部前置历史"；真机证据（range 0–N、压缩后 `history_len` 很小）符合该前提。若将来出现"压缩只覆盖历史中段"，需要后端在事件里带可映射的区间，本规格不预先实现。
- append 型 checkpoint 的卡片位置在"历史顶部"还是"当前位置"取决于回放顺序；R2 会把它钉死在真实档案的顺序上。
