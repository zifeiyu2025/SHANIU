# D3 任务清单

> 前置：无（可与 D1/D2 并行，但同一棵树上不要与评测线抢跑）。

- [ ] T1 `ResolvedModel` 增 `max_output: u32`，取 `max_tokens_effective`；补所有构造点。
- [ ] T2 `compaction/mod.rs`：新增 `FALLBACK_MAX_OUTPUT`/`DEFAULT_HEADROOM_TOKENS`，改写 `window_allowance`/`compact_token_budget`/`manual_token_budget` 签名与公式。
- [ ] T3 `KEEP_RATIO_PERCENT` 30 → 16（`:27`）；`USER_TEXT_KEEP_PERCENT` 保持 30 并在注释里写明两者不是一回事。
- [ ] T4 `agent_loop/mod.rs:839-845` 传入 `resolved.max_output`。
- [ ] T5 先算后改：用公式手算 200K/128K/32K 三组期望值，写进新用例（`spec.md` §3.2）。
- [ ] T6 改既有 5 处断言（`spec.md` §3.1），禁止跑一次抄一次。
- [ ] T7 反向验证 5 条（`spec.md` §3.3），记录输出后恢复。
- [ ] T8 文档：`docs/04-协议与上下文.md` 预算章节同步公式与默认值来源。
- [ ] T9 评测对照：一条长会话样本，对比改动前后摘要次数与总 token（结论写进提交说明，不改代码）。
- [ ] T10 验收：`cargo test compaction`、全量测试、clippy、fmt、check-deps、check-doc-claims 全绿；一笔提交，非 force。
