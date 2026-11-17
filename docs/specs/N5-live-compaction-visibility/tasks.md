# N5 任务清单 v4（R6 实时压缩空白）

> v4：真机 `2026-11-17-9af8431` 的 N1。规格见 `spec.md` §13。所有条目未完成前不得结束回合；反向验证必须贴红输出。

## v4 第一笔：复现（先红）

33. [x] 回滚上一轮临时调查改动：`src/transport/mock.ts` 的 `/compact-replay` 分支、`index.html` 的错误 overlay、`adapter.spec.ts` 的 `TEMP 0.2.9 实时回放调查` 用例（`git diff` 逐文件确认后恢复，不动另一条线在 `adapter.ts`/`types.ts`/`session-events.ts`/`src-tauri` 的改动）。
    - 2026-11-17 复核：三处均不存在（`mock.ts` 无 `compact-replay`、`index.html` 无 `overlay`、`adapter.spec.ts` 无 `TEMP 0.2.9`），工作树干净，无需回滚。
34. [x] 建 `src/transport/fixtures/r6-live-compaction.ts`：从 `docs/smoke/2026-11-17-9af8431/evidence/03-events-compaction.jsonl` 逐字摘录 3 条 `system_prompt`、2 组 `compaction_*`、首尾 `tool_call_output_delta` 各 1 条（其余 1998 条省略写注释），附来源行号/批次注释。
    - 已随 `6bb19e4` 落地。
35. [x] `adapter.spec.ts` 加 `R6 实时压缩不改写 system 头`：② foldedSurface 含 system 节点、③ `inspectSystemPrompt` 全链 `uncertain===false` 且 `effective.text` 为最后一条系统提示词、④ checkpoint 节点/摘要/wire 形状护栏。先在未改代码上跑红并记录输出。
    - 已随 `6bb19e4` 落地（用例名「R6 真机复现（0.2.9 事件原样回放）」）。
36. [ ] 浏览器现场：dev 下 `remote` 临时接 `createAdapter({ bridge: mockBridge })`，按真实节奏喂同一批事件，压缩窗口截全树 + 收 console 异常；记录「空白 / 未复现」结论与栈，还原临时接线（不提交）。
    - 2026-11-17 处置：**未做**。改为「契约用例反向验证 + 真机复验」双证据，理由见 `docs/evidence/2026-11-17/README.md` 的 N1 段（浏览器现场需临时改 `remote` 接线，属一次性调查动作；真机复验是同一条链路的最终判据，优先级更高）。

## v4 第二笔：修复

37. [x] `adapter.ts pushCompactionCheckpoint`：`sysIdx = surfaceSeqs.findIndex(type === 'system/message')`，`sysIdx<0` 全量、`===0` 跳过头部、`>0` 取 `slice(sysIdx+1)`、空段回退 `slice(0, sysIdx)`；空 surface 占位锚点分支与 `sourceEventSeqs` 拼装不变。
    - 已随 `c6287ee` 落地；实现取「最后一段连续的非 system 节点」，等价于规格的 `slice(sysIdx+1)`，且 system 位于末尾时同样不遮蔽。
38. [x] 反向验证：判据退回 `surface[0]` → 用例 ②③ 红；`anchors` 恒全量 → 同上红；记录输出后恢复并重跑绿。
    - 2026-11-17：`docs/evidence/2026-11-17/n1-r6-rev-surface0.txt`（`expected [ 12 ] to include 3`）、`n1-r6-rev-all-nodes.txt`（同断言）；恢复后 `n1-r6-adapter-green.txt`（187 passed）。
39. [x] 全量：`pnpm typecheck`、`pnpm exec vitest run --exclude "scratch/**"`、`cargo test -- --test-threads=1`、`cargo clippy --all-targets -- -D warnings`、`.\scripts\check-deps.ps1`、`.\scripts\dev\check-doc-claims.ps1`。
    - 2026-11-17：typecheck 0；vitest 45 files / 512 passed；cargo test lib 1153 passed / 4 ignored + 集成全绿；clippy 0；check-deps 0 违规；check-doc-claims A 类缺口 0 / B 类 5（既有）。日志 `docs/evidence/2026-11-17/n1-*.txt`。**环境注**：`check-doc-claims.ps1` 无 BOM，PowerShell 5.1 直跑会按 ANSI 解码中文注释而解析失败，必须用 `pwsh` 7 跑（CI 默认即 pwsh）。
40. [ ] `git fetch` → 两笔提交（只 add 本任务路径，不碰 `docs/smoke/`）→ 非 force 推送 → 盯 CI `gate` + `product-smoke` 到实际结论后汇报；冲突按语义合并，不丢另一线的审批改动。

## v3 历史（已完成）

# N5 任务清单 v3（R5 残留收敛）

> v2 已由 `408dee9` 落地、真机第五轮 A 组翻绿；v3 只追加 R5 五条（四条修、一条登记），v2 的第 14 项（真机结案）保留给用户另排。

## 第一笔：R5-1 压缩节点默认展开

15. [x] `src/dsh/packages/client/ui-chat/src/client/conversation-nodes/compaction.ts` 的 `chatNode(...)` 补 `{ groupTree: 'turn' }`（与 `turn-error.ts:116` 同口径）。
16. [x] 用例：`src/transport/adapter.spec.ts` 的 R1 断言 `buildViewNode(...).groupTree === 'turn'`。
17. [x] 账本：`align/overrides.json` 加 `ui-chat/src/client/conversation-nodes/compaction.ts` 条目（reason/guard 写明上游若移出过程组即删本地选项）；`docs/07` 追加第 34 行；`node scripts/align-report.mjs` 重生成后 `--check`。
18. [x] 反向验证：删 `{ groupTree: 'turn' }` → 用例红（`expected undefined to be 'turn'`）；恢复后绿。

## 第二笔：R5-4 导出压缩事务带 turn/time/trigger

19. [x] Rust 档案层：`RolloutItem::Compacted` 加 `#[serde(default)] at_ms: u64`、`#[serde(default, skip_serializing_if="Option::is_none")] trigger: Option<String>`；`CompactionTrigger::as_str()`（无通配臂）；补齐全部构造点与解构点（`agent_loop`、`session_compact`、`lib.rs`、`rollout/resume.rs`、各测试）。
20. [x] Rust 写入层：`append_anchor` 带 `at_ms`/`trigger`（两处调用点传 `now_ms()` 与实际 trigger），`session_compact::write_anchor` 写 `now_ms()` + `Manual`。
21. [x] Rust 投影层：`summary_entry` 透传 `at_ms`/`trigger`；`SessionEntry::CompactionSummary` 加 `trigger`；`lib.rs:1191/1315` 两处解构补字段。
22. [x] TS：`src/transport/types.ts` 的 `compaction_summary` 条目加 `trigger?: string`；`session-events.ts` 三条事件用 `turn: open ? turn : null`、`time` 取 `at_ms`、按有值才写 `trigger`。
23. [x] 用例：`session-events.spec.ts` 三条（回合内 turn=1 / 回合外 turn=null / time+trigger 来自档案）；Rust 两条（旧档兼容 + 新档回读；`as_str` 与 wire 名一致）。
24. [x] 反向验证：`turn` 固定 null → 回合内用例红；删 `trigger` 透传 → 两条用例红；`time` 用导出时刻 → 时刻断言红；删 `#[serde(default)]` → 旧档解码红；`as_str` 写错 → wire 一致用例红；均已恢复并绿。

## 第三笔：R5-3 + R5-5

25. [x] `adapter.ts:4144/4151` 两句改中文，同步 `adapter.spec.ts` 期望；补「无可压历史」分支断言。
26. [x] `agent_loop/mod.rs:536` 正文前缀改为常量 `COMPACT_REQUEST_FAILED = "压缩请求发起失败："`（单测断言拼出的正文不含「本轮」）。不在 `src/dsh/**` 测试里加断言：`vite.config.ts` 已整体排除该目录，写了也不进门禁。
27. [x] 三处失败 `warn!(…, "上下文压缩失败（SH-032）")`（`session_compact::emit_ended` Err、溢出补救 Failed、轮前预算 Failed）。
28. [x] 新增 `src-tauri/tests/compaction_failure_log.rs` 源扫描用例：两文件合计 ≥3 处 tag，且三条关头各自命中。
29. [x] 反向验证：文案改回英文 → `/compact` 用例红；前缀恢复「本轮中止」→ 正文单测红；删轮前预算日志 → 扫描用例红；均已恢复并绿。

## 第四笔：R5-2 登记

30. [x] 规格 §8.5/§12 已写入已知限制与原因（实时当轮 / 重开 range 首轮 / 旧档「记录时刻不可用」），不改代码。

## 收尾

31. [x] 全量：`pnpm typecheck`、`pnpm exec vitest run --exclude "scratch/**"`（491 passed）、`cargo test -- --test-threads=1`（1095 passed）、`cargo clippy --all-targets -- -D warnings`（0，`compact_in_place` 加 8 参数 allow）、`check-deps`（0 违规）、`check-doc-claims`（A 类缺口 0）、`align-report --check`（通过）。
32. [x] 五笔提交（`e816b7b` `6806856` `26f0603` `d8ab83f` `02250ba`，只 add 本任务路径，不碰 `docs/smoke/`）；`git fetch` 后非 force 推送 `73546f9..02250ba`；CI run [43664959027](https://github.com/zifeiyu2025/SHANIU/actions/runs/43664959027) 全绿：`product-smoke` success、`gate` success（依赖方向 / 文档断言 / clippy / 后端测试 / 前端测试 / 上游对齐账本 / 前端构建 七步全 ✓）。

## v2 历史（已完成）

### v2 第一笔：复现 + 修复

1. [x] 建 `src/transport/fixtures/r4-compaction.ts`：从 r4 真机事件档案逐字摘录宿主事件（第 1–2203 行实时段、第 4799–4801 行手动 `/compact`、第 4740 行重开锚点），附来源与批次注释；仅排除 2000 行 `tool_call_output_delta`（不改 surface 成员）。
2. [x] `adapter.spec.ts` 加 R1/R2/R3 三条用例，在未改代码上跑红并记录输出（`assertSystemHeadRewrite` 原文报错 / 0 checkpoint / 含「## 用户原文」的普通 user/message）。
3. [x] D1：surface 为空时不再静默 `return undefined`，改为自建空内容 user/message 占位锚点后按替换落节点 + `console.warn`；system 头节点留在区间之外。
4. [x] D2：回放 `compaction_summary` 与实时同形（start → summary → checkpoint），删除 notice 兜底分支。
5. [x] D3：删手动 `/compact` 的原始摘要 append；`compaction_ended` 成功分支去掉 manual 排除；同一 `compactionId` 只落一条 checkpoint。
6. [x] 更新既有压缩用例期望（摘要只作 checkpoint 正文、不再有「已压缩」通知、手动成功只推 run + done）。
7. [x] 反向验证 1/2：退回 system 头不跳过 → R1 红（上游错误原文）；退回「只发 notice」→ R2 红；恢复原始摘要 append → R3 红；均已恢复并重跑绿。

### v2 第二笔：additive（单独提交 `c6a89be`）

8. [x] A1：`CompactionEnded` 补 `trigger`（Rust 事件 + 四个发出点 + 手动 `emit_ended` + TS 类型 + 适配层改用 `msg.trigger`，删 trace 副本）；`tests/event_schema.rs` 与 `tests/wire_field_consumers.rs` 同步。
9. [x] A2：`session-events.ts` 在锚点处补 `compaction/start|summary|end` log-only 事件 + 导出用例；结论写进提交信息（「导出侧主动跳过，非未落档」）。
10. [x] A3：手动压缩补「上下文已压缩（SH-032）」日志行（`trigger=manual` + 条数）。
11. [x] 反向验证 3：注释掉导出三条事件 → 用例红（`expected [1, 0, 0] to deeply equal [1, 1, 1]`），恢复后 26 条全绿。

### v2 收尾（已完成）

12. [x] 全量：`pnpm typecheck`、`pnpm exec vitest run --exclude "scratch/**"`（44 files / 489 passed）、`cargo test -- --test-threads=1`、`cargo clippy --all-targets -- -D warnings`、`check-deps`（0 违规）、`check-doc-claims`（A 类缺口 0）。
13. [x] 两笔提交（只 add 本任务路径，不碰 `docs/smoke/`）：`chore(N5)` = 三笔 additive；`fix(N5)` = 复现用例 + D1–D3 + 规格 v2。
14. [ ] `git fetch` 后非 force 推送，盯 CI 到结论，逐项报告；写明「为什么 v1 用例绿而真机红」。真机结案由用户另行安排。
