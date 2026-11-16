# D4 错误分类：结构化 code 优先（2026-11-16）

## 改了什么

- `src-tauri/src/model_client/error.rs`
  - 新增 `ModelErrorKind::HookDenied`（`inference_hook_denied`）与
    `ModelErrorKind::AccessProgramUnsupported`（`invalid_access_program` /
    `unsupported_access_program`），受控码 `HOOK_DENIED`、`ACCESS_PROGRAM_UNSUPPORTED`。
  - 新增 `structured_kind(code, status)`：结构化 `code`（缺失时 `provider_fields`
    回落到 `type`）优先分类，并按**合法状态码集合**门控；不在集合里返回 `None`。
  - 旧规则原样搬进 `legacy_status_kind`，作为结构化字段判不出/冲突时的兜底。
  - `from_body`（流内错误体）同样先走 `structured_kind`，无状态码时不门控。
- `src-tauri/src/agent_loop/error_codes.rs`：登记两个新受控码，`ALL` 同步。
- `src-tauri/src/key_check.rs`：两类新错误在三态校验里判 `Unknown`（不是密钥无效）。
- `src-tauri/src/model_client/retry.rs`：白名单用例把两类新错误列入确定性失败。
- `docs/04-协议规范.md` §6.6：写清优先级、门控表、流内语义与「无开关」。

## 验证

| 步骤 | 文件 | 结果 |
| --- | --- | --- |
| 先红用例 | [01-red-test.txt](01-red-test.txt) | 2 条新用例红（`Auth` / `Unknown`） |
| 反向验证（退回旧写法） | [02-reverse-red.txt](02-reverse-red.txt) | 同一 2 条用例必红 |
| 恢复实现 | [03-green.txt](03-green.txt) | `model_client::` 61 passed |

- `cargo clippy --all-targets -- -D warnings`：通过。
- 全量 `cargo test --lib`：1133 passed / 1 failed，唯一失败为
  `sandbox::tests::mcp_server_runs_inside_container_and_reads_workspace`
  （容器内读工作区文件为空，本机无可用容器运行时；本提交未触碰 `src/sandbox/`，与
  D4 无关，按环境红灯如实记录，未改断言也未跳过）。

## 反向退化会怎样

把 `from_status` / `from_body` 里的 `structured_kind` 调用摘掉，两个用例立刻红：
网关钩子拒绝会显示成「密钥无效」，访问程序不支持会显示成普通 400，用户按提示改
key 永远修不好。
