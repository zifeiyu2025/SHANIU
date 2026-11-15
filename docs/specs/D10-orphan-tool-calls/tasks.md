# D10 任务清单

> 前置：无。

- [ ] T1 `resume.rs:186-198`：剔除改补写，新增 `ORPHAN_TOOL_OUTPUT` 常量。
- [ ] T2 `Resumed.dropped_unsafe_tail` → `patched_orphan_calls`，补所有构造点。
- [ ] T3 `lib.rs` 五处文案（`:609/:625/:712/:797/:3088`）改为"补了 N 条结果未知"。
- [ ] T4 先写红用例 5 条（`tests/rollout_resume.rs`，`spec.md` §3.1）。
- [ ] T5 改既有 3 条断言（`tests/compaction_resume.rs:294-340`）为"摘要 + 调用 + 占位"。
- [ ] T6 反向验证 5 条（§3.3），记录输出后恢复。
- [ ] T7 全仓 `grep -n dropped_unsafe_tail` 清零；`grep -n "丢弃"` 确认文案无残留。
- [ ] T8 文档：`docs/04` 恢复语义、`docs/11-使用指南.md` 崩溃恢复一节补说明。
- [ ] T9 验收：全量测试、clippy、fmt、check-deps、check-doc-claims 全绿；一笔提交，非 force。
