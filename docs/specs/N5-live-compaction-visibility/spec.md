# N5（P2）压缩提示实时显示 — 规格 v3（R5 残留收敛）

> v1（`4a8d0be`）的口径与实现已被真机否决，本文件取代 v1 的 §2/§3/§4；v1 §1 的链路定责仍然成立（后端已广播、适配层已接收），错的只是"发出什么形状"和"何时发出"。
> v2 已由 `408dee9` 落地，真机第五轮 A 组翻绿；v3 只收 R5 的五条残留（四条修、一条登记），判据不放宽。
> 范围：`src/transport/`、`src/header-actions/`、`src-tauri/`，以及 `src/dsh/` 的**一处**最小 vendored 改动（R5-1，见 §8.1）；不改渲染器、不改 `docs/smoke/`。

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

## 8. R5 残留（真机第五轮 `408dee9`）

| 编号 | 级别 | 处置 | 一句话 |
| --- | --- | --- | --- |
| R5-1 | P2 | 修 | 压缩节点默认折进两级组内，不展开取 UIA 全树命中 0 |
| R5-4 | P2 | 修 | 导出三条压缩事务 `turn=null`/`time=0`/无 `trigger` |
| R5-5 | P3 | 修 | 失败区块「本本轮运行失败」重复；压缩失败日志 0 行 |
| R5-3 | P3 | 修 | `/compact` 完成卡文案是英文 |
| R5-2 | P3 | 只登记 | 实时挂当轮、重开挂 range 首轮，重开「记录时刻不可用」 |

### 8.1 R5-1 压缩节点必须不展开就可见

**先查上游有没有 default-open 的配置表达**（要求：能配置就不 fork）：

- `conversation-nodes/common.ts:52-74` 的 `chatNode` 只有 `location` / `visibility` / `groupTree` 三个可选参数；
- `chat-settings.ts` 与 `presentation-policy.ts` 只提供 `collapseTiming`（收起时机）与视图模式（compact/standard/detailed/verbose），**没有任何按节点类型豁免折叠的开关**；
- 上游对"留在 Turn、不进过程披露"的唯一既有表达就是 `groupTree: 'turn'`：`turn-error.ts:116`、`turn-max-tokens.ts:72`、`turn-tail.ts:138`、`turn-process.ts:339` 四处都是这个写法；
- `isAtOrOutside(node,'turn')` 决定 `turn-process-presentation` 跳过、`chat-snapshot-builder` 不把它计为过程成员，因此**不会被「已完成，用时 N 秒」→「已完成分析」两级组折叠**。

结论：没有纯配置入口；最小改动是给 vendored 的 `compaction.ts:55` 补 `{ groupTree: 'turn' }`，与 turn-error 同口径。这不是 fork 渲染器，只是把定义该传的选项传对。

**代价与登记**：`packages/client/ui-chat/src/client/conversation-nodes/compaction.ts` 从此成为 vendored drift，必须同一笔提交里在 `align/overrides.json` 加条目（reason 与 guard 写明「上游若把压缩节点移出过程组，删本地 groupTree」），并在 `docs/07-vendor本地改动清单.md` 追加第 34 行；随后 `node scripts/align-report.mjs` 重生成账本（CI 有「账本过期即失败」）。

**判据（下一轮真机）**：不展开任何组，UIA 全树里 `compaction` 节点命中 ≥1。本地用例把这条钉成 `buildViewNode(...).groupTree === 'turn'`（折叠决策的唯一输入），不靠截图。

### 8.2 R5-4 导出压缩事务的 turn / time / trigger

**根因**：`src/header-actions/session-events.ts` 的 `compaction_summary` 分支合成三条 log-only 事件时，`turn` 硬编码 `null`、`time` 走 `entryTime(entry)`（档案 `at_ms` 恒 0）、`trigger` 根本没有来源——`c6a89be` 补的 `trigger` 只加在 `CompactionEnded` 事件上，没进档案，导出合成分支自然拿不到。

**落法（三层各自最小）**：

1. 档案层 `src-tauri/src/rollout/mod.rs:74` 的 `RolloutItem::Compacted` 增加
   `#[serde(default)] at_ms: u64` 与 `#[serde(default, skip_serializing_if = "Option::is_none")] trigger: Option<String>`。
   旧档没有这两个键，`serde(default)` 解码为 `0` / `None`，不伪造。`trigger` 存字符串（与同枚举既有的 `reason: String` 同风格），取值由 `CompactionTrigger::as_str()` 提供（无通配臂 match，新增变体不更新即编译失败），保证只有一份机器值。
2. 写入层：`agent_loop::append_anchor` 加 `at_ms`/`trigger` 两个参数（两处调用分别传 `now_ms()` + 实际 `trigger`）；`session_compact::write_anchor` 写 `now_ms()` + `Manual`。时刻取**落档那一刻**，不用压缩开始时刻。
3. 投影层 `lib.rs` 的 `summary_entry` 加 `at_ms` / `trigger` 透传；`SessionEntry::CompactionSummary` 增加 `#[serde(default, skip_serializing_if = "Option::is_none")] trigger: Option<String>`（`at_ms` 已有）。两处 `RolloutItem::Compacted` 解构补字段（`project_entries` 与 `project_slots_with`），`rollout::resume` 的解构用 `..` 之外的显式字段也要同步。
4. 导出层 `session-events.ts`：
   - `turn`：`const compactionTurn = open ? turn : null`，三条事件共用。**冲突取舍**：`dsh repair.ts`（`openTurnClosers` 只对"日志末尾还开着"的回合补 closer）与 codex `thread_history_projection`（投影只陈述档案里已成立的归属）都指向"不虚构归属"，所以锚点落在回合外时坚持 `null`，不把它归给"即将开始的下一轮"；锚点写在回合内（溢出补救）时如实带当轮号。代价：回合外锚点无法回答"它属于哪一轮"，与上游 `compaction/*` 的 `turn: null` 语义一致。
   - `time`：`entryTime(entry)` 读 `at_ms`（新档有真实时刻；旧档仍 `0 + timeUnreliable: true`）。
   - `trigger`：`entry.trigger === undefined ? {} : { trigger: entry.trigger }` 三处都加。**已知代价**：上游 `compaction/*` 事件类型没有 `trigger` 字段，这是审计扩展；`formatSessionMarkdown` 不读它，但若该导出被当原生 v4 严格校验，未知键可能被拒——与 §3 A2 的「缺 `shadowedRange`」同性质，宁可显式记录也不丢事实。

**用例**：`src/header-actions/session-events.spec.ts`（既有导出用例文件）加三条断言——回合内锚点 `turn === 1`、回合外锚点 `turn === null`、`time` 等于档案 `at_ms` 且 `trigger === 'manual'`；Rust 侧在 `tests/rollout_resume.rs` 或 `tests/compaction_resume.rs` 补一条"旧档无新键可解码、新档回读保留 `at_ms`/`trigger`"。

### 8.3 R5-3 `/compact` 完成卡改中文

`src/transport/adapter.ts:4144/4151` 两处英文：

- `No compactable history yet.` → `暂无可压缩的历史。`
- `Compacted ${n} history items (~${t} tokens).` → `已压缩 ${n} 条历史（约 ${t} tokens）。`

同步更新 `adapter.spec.ts` 里对这两句的期望（含手动成功用例）。判据：`/compact` 成功/空历史的命令卡文本不含 ASCII 句子。

### 8.4 R5-5 失败文案去重 + 压缩失败日志

**去重**：源码里没有「本本轮」字面量。失败区块的标题是词典 `message.turnError` =「本轮运行失败」，正文来自宿主 `COMPACTION_FAILED` 的 `CompactAbort::Failed(format!("压缩请求发起失败，本轮中止：{e}"))`（`agent_loop/mod.rs:536`）；UIA 全树把两段相邻铺平后再读，就成了「本本轮运行失败」。修法：正文前缀抽成常量 `COMPACT_REQUEST_FAILED = "压缩请求发起失败："`（保留"压缩请求发起失败"这个可 grep 的事实，去掉第二个「本轮」），并附单测断言拼出的正文不含「本轮」。

**为什么不在 UI 用例里锁**：`vite.config.ts:75` 把 `src/dsh/**` 整体排除在 SHANIU 测试面之外（上游测试依赖其 host 运行时），写进 `chat-view.client.spec.tsx` 的断言不会进任何门禁，等于死代码且增加 vendored 测试漂移；标题文案本身由上游词典固定，正文由本侧常量决定，锁前缀即可锁住「只出现一次」。

**日志**：压缩失败目前只有前端痕迹与 rollout，日志 0 行。三处失败点统一补 `warn!`，tag 与成功日志同源、可一条 grep 命中：

- `session_compact.rs::emit_ended` 的 `Err` 分支（手动）；
- `agent_loop/mod.rs:735` 溢出补救 `Failed`（在原 warn 的文案前加 tag，保留原标题）；
- `agent_loop/mod.rs:1305-1311` 轮前预算 `Failed`（新增一条）。

统一文案 `上下文压缩失败（SH-032）`，级别 `warn`：成功是 `info`（正常路径不该刷警告），失败必须 warn 才会进用户可见日志级别。

**判据**：新增 `src-tauri/tests/compaction_failure_log.rs`（仿 `now_ms_single_source.rs` 的源扫描用例），断言 `src/agent_loop/mod.rs` 与 `src/session_compact.rs` 内 `上下文压缩失败（SH-032）` 出现 ≥3 次——改回"只在前端留痕"或删任一分支即红。

### 8.5 R5-2 已知限制（只登记，不修）

写入本规格 §12 与 `tasks.md`：

- **实时**：节点锚在**当轮**（`journal.turn` 由适配层按用户消息自增）。
- **重开**：档案里没有"锚点属于哪一轮"，只能按 `[range_start, range_end)` 推导，落在 **range 首轮**。
- 两者位置可能不同，这是档案信息不足的必然结果，不是渲染 bug。
- **「记录时刻不可用」**：旧档案 `Compacted` 无 `at_ms`（本规格之前写入的），投影为 `0`，界面按 A0-4 如实显示"记录时刻不可用"；R5-4 之后的新档有真实时刻。不为了好看回填导出时刻。
- 不改代码；若将来要给旧档补时刻，必须作为独立迁移任务（不可逆地改写档案），不在本线做。

## 9. 反向验证（v3，逐条改回去必须红）

1. R5-1：删掉 `{ groupTree: 'turn' }` → 新用例红（`groupTree` 为 `undefined`）。
2. R5-4：导出三条事件 `turn` 固定 `null` → 回合内用例红；删 `trigger` 透传 → `trigger` 断言红；`time` 改回导出时刻 → `at_ms` 断言红；Rust 删 `#[serde(default)]` → 旧档解码用例红。
3. R5-3：文案改回英文 → 命令卡用例红。
4. R5-5：前缀改回含「本轮中止」→ 正文单测红；删任一失败日志 → 日志扫描用例红（<3 处或某条关头缺失）。
5. 每条记录命令与关键输出后恢复，恢复后全量重跑绿。

## 10. 验证命令（v3 与 v2 相同）

`pnpm typecheck`；`pnpm exec vitest run --exclude "scratch/**"`；`cargo test -- --test-threads=1`；`cargo clippy --all-targets -- -D warnings`；`.\scripts\check-deps.ps1`；`.\scripts\dev\check-doc-claims.ps1`；`node scripts/align-report.mjs --check`。Rust 只对改动文件跑 `rustfmt`（禁止 `cargo fmt`）。

## 11. 提交与发布（v3）

四笔，按可独立回滚切分：

1. `fix(N5): 压缩节点默认展开到 Turn 层（R5-1）` —— `compaction.ts` 一行 + 用例 + `align/overrides.json` + `docs/07` 第 34 行 + 重生成的账本；
2. `feat(N5): 导出压缩事务带 turn/time/trigger（R5-4）` —— Rust 三层 + TS 两处 + 用例 + 本规格 §8.2；
3. `fix(N5): 压缩文案中文化与失败日志（R5-3/R5-5）` —— `adapter.ts` 两句 + `agent_loop` 正文与三处 warn + 两个用例文件；
4. `docs(N5): 登记 R5-2 已知限制` —— 只改本规格与 `tasks.md`。

只显式 `git add` 上述路径；**不碰 `docs/smoke/`**；推前 `git fetch`，不带 `--force`；盯 gate + product-smoke + 对齐账本到结论再报。

## 12. 已知限制（R5-2 登记）

实时节点挂当轮、重开挂 range 首轮；重开旧档出现「记录时刻不可用」。原因与取舍见 §8.5，不修。

