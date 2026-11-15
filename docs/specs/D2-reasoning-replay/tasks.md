# D2 任务清单

> 前置：D1 的 `include` / `ProviderConfig.reasoning_replay` 已落地。

- [ ] T1 `HistoryItem` 增 `Reasoning { id, summary, encrypted_content }` 与 `ReasoningSummary`。
- [ ] T2 `ResponseItem` 增同名变体；`parse_item` 补 `"reasoning"` 分支。
- [ ] T3 `responses_input` 序列化 reasoning 项（空密文省略键）。
- [ ] T4 `chat_messages` 跳过 reasoning 项。
- [ ] T5 `agent_loop`：收 `OutputItem(Reasoning)`，在 assistant 消息前入历史。
- [ ] T6 `compaction::item_tokens` 计入密文；`keep_from_tail` 加"推理与消息不拆"规则。
- [ ] T7 档案消费方补分支：`resume.rs`、`session_reference.rs`、`session_compact.rs`（按 `spec.md` §2.7 表）。
- [ ] T8 先写红用例：3 条单元 + 1 个集成文件 + 2 条压缩用例。
- [ ] T9 反向验证 6 条（`spec.md` §3.4），记录输出后恢复。
- [ ] T10 文档：`docs/04` 推理回放章节、`docs/12-dsh后端参照.md` 的差异说明、发布说明（降级不可逆）。
- [ ] T11 验收：全量测试、clippy、fmt、check-deps、check-doc-claims 全绿；一笔提交，非 force 推送。
