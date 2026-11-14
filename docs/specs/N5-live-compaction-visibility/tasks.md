# N5 任务清单

1. [ ] `src/transport/adapter.ts`：`CompactionTrace` 增 `startSeq`；`compaction_started` 记录该 seq。
2. [ ] `src/transport/adapter.ts`：新增模块内 `pushCompactionCheckpoint(journal, compactionId, hiddenSeq)`（`user/message` + `surfaceOp:'replace'` + `source.kind='compact-checkpoint'`）。
3. [ ] `src/transport/adapter.ts`：实时成功分支删 `developer/message` 通知，改发 checkpoint；失败分支保持通知。
4. [ ] `src/transport/adapter.ts`：回放 `compaction_summary` 改为 start → summary → checkpoint，删通知。
5. [ ] `src/transport/adapter.spec.ts`：更新压缩相关既有用例期望（通知 → checkpoint）。
6. [ ] `src/transport/adapter.spec.ts`：新增 4 组回归用例（实时可见 / 节点可被 UI 识别 / 失败仍出声 / 重开不重复）。
7. [ ] 反向验证：分别退回「实时只发通知」「回放保留通知」，跑出红，记录输出后恢复。
8. [ ] 全量验证：`pnpm typecheck`、`pnpm test`、`cargo test`、`cargo clippy --all-targets -- -D warnings`、`.\scripts\check-deps.ps1`。
9. [ ] 单笔提交（只 add 本任务路径），`git fetch` 后非 force 推送。
10. [ ] 盯 CI（gate + product-smoke）到结论，逐项报告结果。
