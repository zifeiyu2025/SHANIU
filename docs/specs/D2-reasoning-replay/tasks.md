# D2 任务清单

> 前置：D1 的 `include` / `ProviderConfig.reasoning_replay` 已落地。
>
> 2026-11-16 落地完成。T2/T5 按 `spec.md` §2.2 的同款偏差接到 D1 的 `RequestOptions`（不扩 `ProviderConfig`）；
> T8 的集成用例另起一笔提交（端到端回放用例单列）。

- [x] T1 `HistoryItem` 增 `Reasoning { id, summary, encrypted_content }` 与 `ReasoningSummary`。
- [x] T2 `ResponseItem` 增同名变体；`parse_item` 补 `"reasoning"` 分支。
- [x] T3 `responses_input` 序列化 reasoning 项（空密文省略键）。
- [x] T4 `chat_messages` 跳过 reasoning 项。
- [x] T5 `agent_loop`：收 `OutputItem(Reasoning)`，在 assistant 消息前入历史。
- [x] T6 `compaction::item_tokens` 计入密文；`tail_target` 加"推理与消息不拆"规则。
- [x] T7 档案消费方补分支：界面投影与检索索引显式忽略；resume/导出按 serde 原样；摘要与轮次计数天然不计。
- [x] T8 先写红用例：3 条单元（另加开关关闭值守门）+ 2 条压缩用例已随本笔；集成文件另起一笔。
- [x] T9 反向验证 6 条（`spec.md` §3.4），记录输出后恢复；第 1、3 条在集成用例上做。
- [x] T10 文档：`docs/04` §4.3、`docs/12` 差异说明、`docs/08` §3.8 发布说明（降级不可逆）。
- [x] T11 验收：全量测试、clippy、fmt、check-deps、check-doc-claims 全绿；一笔提交，非 force 推送。
