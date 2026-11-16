# D5 重试 always 模式（2026-11-16）

## 改了什么

- `src-tauri/src/model_client/retry.rs`
  - 新增 `RetryMode { Bounded（默认）, Always }`，`RetryPolicy.mode` 默认 `Bounded`。
  - `enabled()` / `accepts(kind)`：bounded 走原白名单与次数，always 恒允许（上游语义）。
  - `delay()`：always 无次数上限；`Retry-After` 超过 10s 上限时改用本地指数退避
    （对齐 dsh `llm-retry/index.ts:228-233`），bounded 仍“超上限即放弃”。
- `src-tauri/src/config.rs`：`[model] retry_mode`（`bounded` 缺省），拼错直接报错。
  `RetryMode` 枚举归属 `config`（共享基础设施）：`config` 不能反向依赖
  `model_client`（FF-1），由 `model_client::retry` 按 E20 正向引用；首版放错方向，
  CI gate 的 `check-deps` 报了 2 条违规，已在后续提交修正。
- `src-tauri/src/agent_loop/mod.rs`
  - `from_config` 把 `retry_mode` 灌进策略；always 下 `max_retries` 不参与。
  - `await_retry`：always 不发 `exhausted`，事件 `unbounded = true`、`max_retries = 0`；
    既有三件事原样保留（取消 `select!`、溢出补救优先、首次可见增量前才重试）。
- `src-tauri/src/events.rs`：`ModelRetry` 新增 additive 字段 `unbounded: bool`。
- `src/transport/types.ts` / `adapter.ts`：always 落 `llm/retry` 的 `mode: 'always'`，
  不带 `maxRetries`（上游 always 事件形状）；bounded 分支一字未改。
- `docs/04-协议规范.md`：`[model] retry_mode` 默认语义 + `model_retry` 事件新字段。

## 验证

| 步骤 | 文件 | 结果 |
| --- | --- | --- |
| 恢复实现后 | [01-green.txt](01-green.txt) | 3 条 always 用例 + 默认值用例绿；lib 1138 绿；event_schema 11 绿；clippy 干净 |
| 反向验证（退回旧写法） | [02-reverse-red.txt](02-reverse-red.txt) | 同一 3 条用例全红（单测 2 条 + 集成 1 条） |
| 前端 | [03-frontend.txt](03-frontend.txt) | adapter 179 绿；`tsc -b --force` 无错误 |

集成用例 `always_mode_retries_past_the_bounded_budget` 用假后端脚本给 6 次网络失败、
`max_retries` 只配 1，断言第 7 次成功、6 条 scheduled 都 `unbounded = true`、无 exhausted。

## 默认值等价证明

`RetryMode` 的 `#[default]` 是 `Bounded`，`ModelConfig::default().retry_mode` 是 `Bounded`，
`RetryPolicy::new(n)` 仍是 `Bounded`；配置缺省时解析结果与 D5 之前相同（有单测）。
always 只能由用户显式写 `retry_mode = "always"` 打开。
