# N5 任务清单 v2（真机返工）

## 第一笔：复现 + 修复

1. [x] 建 `src/transport/fixtures/r4-compaction.ts`：从 r4 真机事件档案逐字摘录宿主事件（第 1–2203 行实时段、第 4799–4801 行手动 `/compact`、第 4740 行重开锚点），附来源与批次注释；仅排除 2000 行 `tool_call_output_delta`（不改 surface 成员）。
2. [x] `adapter.spec.ts` 加 R1/R2/R3 三条用例，在未改代码上跑红并记录输出（`assertSystemHeadRewrite` 原文报错 / 0 checkpoint / 含「## 用户原文」的普通 user/message）。
3. [x] D1：surface 为空时不再静默 `return undefined`，改为自建空内容 user/message 占位锚点后按替换落节点 + `console.warn`；system 头节点留在区间之外。
4. [x] D2：回放 `compaction_summary` 与实时同形（start → summary → checkpoint），删除 notice 兜底分支。
5. [x] D3：删手动 `/compact` 的原始摘要 append；`compaction_ended` 成功分支去掉 manual 排除；同一 `compactionId` 只落一条 checkpoint。
6. [x] 更新既有压缩用例期望（摘要只作 checkpoint 正文、不再有「已压缩」通知、手动成功只推 run + done）。
7. [x] 反向验证 1/2：退回 system 头不跳过 → R1 红（上游错误原文）；退回「只发 notice」→ R2 红；恢复原始摘要 append → R3 红；均已恢复并重跑绿。

## 第二笔：additive（单独提交 `c6a89be`）

8. [x] A1：`CompactionEnded` 补 `trigger`（Rust 事件 + 四个发出点 + 手动 `emit_ended` + TS 类型 + 适配层改用 `msg.trigger`，删 trace 副本）；`tests/event_schema.rs` 与 `tests/wire_field_consumers.rs` 同步。
9. [x] A2：`session-events.ts` 在锚点处补 `compaction/start|summary|end` log-only 事件 + 导出用例；结论写进提交信息（「导出侧主动跳过，非未落档」）。
10. [x] A3：手动压缩补「上下文已压缩（SH-032）」日志行（`trigger=manual` + 条数）。
11. [x] 反向验证 3：注释掉导出三条事件 → 用例红（`expected [1, 0, 0] to deeply equal [1, 1, 1]`），恢复后 26 条全绿。

## 收尾

12. [x] 全量：`pnpm typecheck`、`pnpm exec vitest run --exclude "scratch/**"`（44 files / 489 passed）、`cargo test -- --test-threads=1`、`cargo clippy --all-targets -- -D warnings`、`check-deps`（0 违规）、`check-doc-claims`（A 类缺口 0）。
13. [x] 两笔提交（只 add 本任务路径，不碰 `docs/smoke/`）：`chore(N5)` = 三笔 additive；`fix(N5)` = 复现用例 + D1–D3 + 规格 v2。
14. [ ] `git fetch` 后非 force 推送，盯 CI 到结论，逐项报告；写明「为什么 v1 用例绿而真机红」。真机结案由用户另行安排。
