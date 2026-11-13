# 失败面题集（harness under abnormal model input）

这组题测的不是模型能力，而是**宿主在模型端行为异常时的收口**：流断开、限流、配额、上下文超长、
长会话压缩、响应不完整。模型端由 runner 内置的本地 mock 端点按题单剧本扮演，因此「模型做错了」
与「harness 坏了」可以分开：剧本固定，判据只看宿主侧可观测事实（请求序列与时间、事件流、
回合结局、rollout 落盘顺序、resume 重放结果）。

## 组织

| 路径 | 作用 |
| --- | --- |
| `evals/failure-tasks.json` | 题单与登记：题号、场景、注入剧本、预置历史、判据键、判据来源 |
| `evals/failure/judge/run.ps1` | 每题一个判据分支；输出 `REPO-CHECK: PASS\|FAIL`，退出码 0/1 |
| `evals/failure/runner/` | 独立二进制 `failure-suite`：起 mock、驱动 `AgentLoop`、落观测、调判据 |
| `evals/results/*-failure.json` | 每次运行的观测与裁决（与 repo 题集同一命名口径） |

判据契约与 [`evals/repo/judge/run.ps1`](../repo/judge/run.ps1) 相同，只是观察对象从工作区文件
换成了宿主行为观测。

## 注入默认关闭

- 失败注入只存在于 `runner/src/mock.rs`，只由本 runner 启动；产品运行路径与 `tasks.json` /
  `repo-tasks.json` 都不引用它。
- runner 只执行带 `inject` 块的题；没有剧本的题不会被它驱动。
- mock 监听 `127.0.0.1:0`（临时端口），不写任何全局配置，不影响正常题集。

## 运行

必须用独立 target 目录，避免占用主 target 锁；不要跑全量 `cargo test`（这里是独立二进制）：

```powershell
$env:CARGO_TARGET_DIR = "D:\MyWorkspace\SHANIU\target\evals-failure"
cargo run --manifest-path evals/failure/runner/Cargo.toml --offline
# 只跑一题：
cargo run --manifest-path evals/failure/runner/Cargo.toml --offline -- --only fail-quota-exhausted
```

首次会从零编译 `shaniu_lib` 依赖树（独立 target），耗时以实际为准。

退出码：`0` = 每题都拿到了判据结论（红也是结论）；`2` = 基础设施错误（判据脚本/mock/观测缺失）。

## 题与判据来源

| 题号 | 注入 | 判据要点 | 判据来源 |
| --- | --- | --- | --- |
| `fail-stream-truncated-before-delta` | 首发流无终态标记即 EOF | 重试一次并成功；正文只一份 | `agent_loop/mod.rs#L1367-L1375`、`#L1426-L1430`、`model_client/retry.rs#L50-L59` |
| `fail-stream-truncated-after-delta` | 吐过增量后 EOF | 不重试、终局 `NETWORK`、半截正文不落历史 | `agent_loop/mod.rs#L1318-L1322`、`#L1371-L1375`、`#L1426-L1427` |
| `fail-rate-limit-retry-after` | 429 + `Retry-After: 1` | 实等 ≈1000ms 后重试；正文只一份 | `model_client/mod.rs#L703-L717`、`retry.rs#L67-L74` |
| `fail-quota-exhausted` | 429 + `insufficient_quota` | 一次终局 `QUOTA`，零重试，provider 字段保留 | `model_client/error.rs#L96-L114`、`retry.rs#L50-L59` |
| `fail-incomplete-max-output` | `response.incomplete` / `max_output_tokens` | 终局 `INCOMPLETE`，零重试，文案点明最大输出长度 | `model_client/mod.rs#L163-L171`、`error.rs#L214-L222`、`agent_loop/mod.rs#L1345-L1351` |
| `fail-context-overflow-stream-replay` | 流内 `response.failed` + `context_length_exceeded` | 压缩一次、重放变小且含摘要、收口 | `agent_loop/mod.rs#L1389-L1425`、`#L580-L665` |
| `fail-context-overflow-http400` | HTTP 400 + 同一错误体 | 同流内判据；**当前 HEAD 预期红**（缺口见下） | `model_client/error.rs#L100-L114`、`evals/repo/fixtures/repo_feat_context_overflow.rs#L9-L22` |
| `fail-long-session-compaction-anchor` | 预置超预算历史 + 小预算 | 锚点顺序正确；resume 重放首条为摘要；重放请求不含被遮蔽标记 | `agent_loop/mod.rs#L2263`、`rollout/resume.rs#L64-L145` |

## 已知缺口（`fail-context-overflow-http400` 为什么预期红）

真实端点通常用 **HTTP 400** 报上下文超长。当前 `ModelError::from_status` 对 400/422 一律归
`BadRequest`，只有**流内** `from_body` 才会识别上下文关键词，所以 HTTP 400 形态走不到
`ContextOverflow` 补救压缩。仓库自己的
[`repo_feat_context_overflow.rs`](../repo/fixtures/repo_feat_context_overflow.rs) 已经把
「400 上下文超长要有独立码」登记为要求。该题保留为**缺口探测**：判据不放宽，红必须作为产品
缺口上报；只有实现补齐后才会变绿。

## 本轮实测（2026-11-13，commit `c5afab0`，工作树除本套件新增文件外干净）

`run_id=1794570544938-15180`，观测见 `evals/results/1794570544938-15180-failure.json`。

| 题 | 请求数 | 关键实测 | 判定 |
| --- | --- | --- | --- |
| `fail-stream-truncated-before-delta` | 2 | 重试 scheduled.delay=50ms，实测间隔 58ms，回合 completed | PASS |
| `fail-stream-truncated-after-delta` | 1 | 零重试，回合 failed/`NETWORK`，无助手历史行 | PASS |
| `fail-rate-limit-retry-after` | 2 | scheduled.delay=1000ms，实测间隔 1014ms，回合 completed | PASS |
| `fail-quota-exhausted` | 1 | 零重试，回合 failed/`QUOTA`，`http_status=429`、`provider_code=insufficient_quota` | PASS |
| `fail-incomplete-max-output` | 1 | 零重试，回合 failed/`INCOMPLETE`，无助手历史行 | PASS |
| `fail-context-overflow-stream-replay` | 3 | 压缩 1 次（`context_overflow`），重放变小且含摘要，回合 completed | PASS |
| `fail-context-overflow-http400` | 1 | 回合 failed/`BAD_REQUEST`，零压缩；`provider_code` 仍是 `context_length_exceeded` | **FAIL（缺口确认）** |
| `fail-long-session-compaction-anchor` | 2 | 压缩 1 次（`pressure`），锚点在种子历史之后，resume 历史 13→4 且含摘要 | PASS |

缺口证据：HTTP 400 的错误体已经带了 `context_length_exceeded`，但宿主只按状态码把它归成
`BAD_REQUEST`，所以压缩一次都没发生（上表请求数 1、压缩触发 0）。这不是判据放宽与否的问题，
是 `from_status` 没有消费错误体的直接结果。

## 判据纪律

- 判据只读观测里的宿主事实，不读产品日志，不做「看起来完成了」的推断。
- 发现判据站不住时，先给证据（`文件:行` 或观测字段），再改判据；不得为了让题变绿而放宽。
- `expect=gap` 的红是测量结论；`expect=pass` 的红才是需要排查的异常。
