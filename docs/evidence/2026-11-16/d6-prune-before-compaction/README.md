# D6 压缩前确定性裁剪（2026-11-16）

## 改了什么

- `src-tauri/src/compaction/mod.rs`
  - 新增 `prune_tool_outputs` / `prune_text` / `PruneStats`：单条工具输出 > 8_192 字符时
    保留头 4_096 + 尾 1_024，中段换成带原始字符数的省略标记；只动
    `FunctionCallOutput`，按 `char` 切（UTF-8 安全），幂等。
  - 新增 `plan_if_needed_pruning`：压力到阈值才裁；裁完重新估算，回到预算内返回
    `None`（省掉摘要调用）。端点观测值不按裁剪量下调，观测值仍超预算就照常压缩。
- `src-tauri/src/agent_loop/mod.rs`
  - 轮首预算检查改走 `plan_if_needed_pruning`。
  - 溢出补救在 `plan_manual` 之前先裁一轮（端点已经拒绝过一次，摘要请求要更小）。
  - 新增 `CountingResultBackend`（脚本 + 请求计数）与集成用例。
- `docs/04-协议规范.md` §6.4：写清顺序、阈值、观测值不下调、rollout 不重写与用例名。

## 为什么不止断言裁剪形状

要求是证明「省掉的那次模型调用」。集成用例
`pruning_skips_the_summary_call_when_it_brings_history_back_under_budget` 使用假后端
**计数**：历史 200_000 字符工具输出 + 预算 30_000，裁剪后应只发 1 次采样请求；
退回不裁剪的实现则发 2 次（摘要 + 采样），见 [02-reverse-red.txt](02-reverse-red.txt)。

## 与 dsh 的对照

- dsh `compaction-tool-result-pruner`：`thresholdChars = 8192`、`headChars/tailChars`
  保留、中段写省略标记；只处理工具输出。这里照抄的是结论（头尾 + 标记 + 幂等），
  阈值与头尾字符数同值。
- dsh `compaction-basic`：`pruneSession → 重新测量 → 阈值判定 → selectRange`。
  这里把 prune 收进 `plan_if_needed_pruning`，保持同一顺序。
- 差异（有意）：dsh 把裁剪结果记成事件写回会话；这里不新增事件、不重写 rollout，
  只改运行态历史（见 README 上一节与 docs/04 的说明）。理由：现有协议没有
  “内容替换”事件，新增事件会把 D6 变成 FF 变更，收益与风险不成比例；原文留在
  档案里反而可恢复。

## 验证

| 步骤 | 文件 | 结果 |
| --- | --- | --- |
| 实现后 | [01-green.txt](01-green.txt) | 5 条 D6 用例绿；lib 1143 绿；clippy 干净 |
| 反向验证（退回不裁剪） | [02-reverse-red.txt](02-reverse-red.txt) | 请求计数 2 ≠ 1，用例红 |
