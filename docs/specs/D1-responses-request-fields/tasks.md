# D1 任务清单

> 前置：无。与 D2 同批时，本清单先完成（顺序见 `spec.md` §5）。
>
> 2026-11-16 落地完成。T2 按 `spec.md` §2.2 的偏差改为 `RequestOptions`（不扩 `ProviderConfig`）。

- [x] T1 `config.rs`：`ModelConfig` 增 `responses_extras: bool`（默认 true）、`parallel_tool_calls: bool`（默认 true）；`ResolvedModel` 同步暴露。
- [x] T2 `model_client/provider.rs`：新增 `RequestOptions`（`cache_key` / `responses_extras` / `parallel_tool_calls`，`Default` 为无键 + 双开）；`ProviderConfig` 形状不变。
- [x] T3 `model_client/mod.rs`：`build_responses_request` 增 `options: &RequestOptions`，补 `store`/`tool_choice`/`parallel_tool_calls`/`prompt_cache_key`/`include`（D1 阶段 `include=[]`）。
- [x] T4 呼叫点接线：`ResponsesBackend.options` 随后端携带（回合内压缩同口径）；手动压缩 `compact_session` 传同一会话键；标题生成跟随开关但不带缓存键。
- [x] T5 先写红用例：`responses_body_carries_protocol_fields`、`responses_extras_can_be_disabled`、`compaction_request_shares_the_same_fields`（`tests/responses_request_shape.rs`），另加默认值守门 `responses_extras_are_on_by_default`。
- [x] T6 反向验证：按 `spec.md` §3.2 五条逐条造红并记录输出，恢复后重跑绿。
- [x] T7 既有 body 断言清理：`grep -n '"tools"' src-tauri/tests` 确认无整体 equal 断言漏改（命中项均为事件 schema / 能力探针，与请求体无关）。
- [x] T8 文档：`docs/04-协议规范.md` 的 `[model]` 段补两个开关、新增 §4.2 请求体五键表。
- [x] T9 验收：`cargo test`、`clippy -D warnings`、`check-deps.ps1`、`check-doc-claims.ps1` 全绿；一条提交，非 force 推送。
