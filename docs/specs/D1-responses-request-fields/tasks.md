# D1 任务清单

> 前置：无。与 D2 同批时，本清单先完成（顺序见 `spec.md` §5）。

- [ ] T1 `config.rs`：`ModelConfig` 增 `responses_extras: bool`（默认 true）、`parallel_tool_calls: bool`（默认 true）；`ResolvedModel` 同步暴露。
- [ ] T2 `model_client/provider.rs`：`ProviderConfig` 增两字段并在两个构造函数赋值。
- [ ] T3 `model_client/mod.rs`：`build_responses_request` 增 `cache_key: Option<&str>`，补 `store`/`tool_choice`/`parallel_tool_calls`/`prompt_cache_key`/`include`（D1 阶段 `include=[]`）。
- [ ] T4 呼叫点接线：`agent_loop/mod.rs:1256-1271` 传 `Some(session_id)`；`compaction/mod.rs:308-316` 改走同一构造入口。
- [ ] T5 先写红用例：`responses_body_carries_protocol_fields`、`responses_extras_can_be_disabled`、`compaction_request_shares_the_same_fields`（`tests/responses_request_shape.rs`）。
- [ ] T6 反向验证：按 `spec.md` §3.2 五条逐条造红并记录输出，恢复后重跑绿。
- [ ] T7 既有 body 断言清理：全仓 `grep -n '"tools"' src-tauri/tests` 确认无整体 equal 断言漏改。
- [ ] T8 文档：`docs/04-协议与上下文.md`（或等价的请求体章节）补五个键与 `responses_extras` 开关。
- [ ] T9 验收：`cargo test`、`clippy -D warnings`、`check-deps.ps1`、`check-doc-claims.ps1` 全绿；一条提交，非 force 推送。
