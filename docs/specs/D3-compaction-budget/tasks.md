# D3 任务清单

> 前置：无（可与 D1/D2 并行，但同一棵树上不要与评测线抢跑）。
>
> 2026-11-16 落地。**与规格的一处偏差**：`spec.md` §3.2 手算 200K/16K 时写 120_000，
> 但同一段公式（184K − 65_536）应为 **118_464**；按公式落地并断言 118_464。
> T9 未跑真机长会话（评测线属他人工作区，本轮不并行占用），只做公式级对照并写进提交说明。

- [x] T1 `ResolvedModel` 增 `max_output: u32`，取 `max_tokens_effective`；补所有构造点。
- [x] T2 `compaction/mod.rs`：新增 `FALLBACK_MAX_OUTPUT`/`DEFAULT_HEADROOM_TOKENS`，改写 `window_allowance`/`compact_token_budget`/`manual_token_budget` 签名与公式。
- [x] T3 `KEEP_RATIO_PERCENT` 30 → 16（`:27`）；`USER_TEXT_KEEP_PERCENT` 保持 30 并在注释里写明两者不是一回事。
- [x] T4 `agent_loop/mod.rs` 传入 `resolved.max_output`。
- [x] T5 先算后改：200K/128K/32K 三组期望值写进新用例（`spec.md` §3.2）。
- [x] T6 改既有 5 处断言（`spec.md` §3.1），按公式重算，不抄运行结果。
- [x] T7 反向验证 5 条（`spec.md` §3.3），记录输出后恢复。
- [x] T8 文档：`docs/04` §6.4 同步公式、保留比例与默认值来源。
- [x] T9 评测对照：本轮只做公式级对照（阈值 160K→118.5K、保留额度 30%→16%），结论写进提交说明。
- [x] T10 验收：`cargo test compaction`、全量测试、clippy、fmt、check-deps、check-doc-claims 全绿；一笔提交，非 force。
