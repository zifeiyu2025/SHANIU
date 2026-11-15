# D10 任务清单

> 前置：无。

- [x] T1 `resume.rs`：剔除改原位补写，新增 `ORPHAN_TOOL_OUTPUT` 常量（`resume.rs:201-219`，只在内存补，不回写档案）。
- [x] T2 `Resumed.dropped_unsafe_tail` → `patched_orphan_calls`，构造点全部同步；全仓 `dropped_unsafe_tail` 已清零。
- [x] T3 `lib.rs` 五处文案改为「补了 N 条工具调用的结果未知占位」并说明"模型不会假设它们成功"。
- [x] T4 红用例 5 条已落（`tests/rollout_resume.rs:467/484/504/524/552`）。
- [x] T5 `tests/compaction_resume.rs` 3 条断言按「摘要 + 调用 + 占位」改写（+35 行）。
- [ ] T6 反向验证 5 条（§3.3）：**未勾——提交 `b0d8e70` 未归档逐条输出**。行为本身由 T4/T5 用例覆盖，
  但「退回旧写法必须变红」的执行记录缺失；按本清单口径，没有记录就不算完成，待补跑并补记。
- [x] T7 `grep -n dropped_unsafe_tail` 清零；`grep "丢弃"` 已无落单调用相关文案。
  **偏差注**：`lib.rs` 里仍保留 9 处「丢弃」，均为末行撕裂、过期审批卡、未知 id 等**其它**语义，不是 D10 的文案残留。
- [x] T8 文档：`docs/04` 恢复语义、`docs/11` 崩溃恢复一节均已补（`b0d8e70` 各 +2 行）。
- [x] T9 验收：全量测试、clippy、fmt、check-deps、check-doc-claims 全绿；一笔提交 `b0d8e70`，非 force。
  CI run **43741686195 success**。
