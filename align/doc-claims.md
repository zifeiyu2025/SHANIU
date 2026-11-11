# 文档断言 ↔ 代码 反向核查清单

> 状态：生效（2026-11-11）｜重跑：`pwsh scripts/dev/check-doc-claims.ps1`
> 来源：2026-11-11 反向流程（抽断言阶段只读 `docs/` 与 `AGENTS.md`，清单落盘后才开始看代码）
> 位置理由：这是**核查账本**，不是契约。`docs/0x` 是唯一契约来源，把账本放进 `docs/` 会被当成规范原文读。
> `align/` 已经是「文档 ↔ 实现」对照物的落点（`ledger.md` 对上游、`overrides.json` 对偏离），
> 文档断言对代码归这里，与 `ledger.md` 同一层级。

## 这是什么 / 不是什么

每条可判定承诺一行：文档位置 + 原句 + 期望符号/字面量 + 类型。脚本对当前树重判，输出
A（文档有、代码找不到）/ B（不可机械判定）/ 通过数，退出码 0 / 1 / 2。

**覆盖边界**：本表是**已登记**的断言集，不是从散文自动抽取的。文档新增承诺要人工追加一行
（ID 续号），删除承诺要人工删行；脚本不会自己发现「新写的句子」。所以：

- 绿 = 已登记的 182 行承诺在**当前 HEAD** 上还站得住；
- 绿 ≠ docs 与代码完全一致，也 ≠ 实现正确（见 B 类与下面的盲区）。
- 「文档位置」列的行号只用于人工回查，文档增删后会漂移；脚本只按**文件**判，不校验行号
  （`docs/10` 已经定过口径：引用不写数字行号，留符号）。

## 判定口径

| 类型 | 判据 | 说明 |
| --- | --- | --- |
| 符号 | 至少一处调用点 | 排除注释行、纯定义行；只定义不调用不算兑现 |
| 文件 | 路径存在 | 目录搬家时按文件名全仓回退 |
| 命令 | 命令名（蛇形）至少一处调用点 | `generate_handler!` 里那处算调用点 |
| 字面量 | 源码中按原样字符串出现 | 事件名、环境变量名、产物文件名 |
| 测试 | 用例名存在 | `#[test]` 天然无调用点，不按调用点判 |
| 结构 | 脚本内命名检查 | 缺席断言与 `format!` 模板这类 token 判不了的（见下表） |

**结构检查**（脚本内实现，改命名规则会红）：

| 检查 | 断言 | 判据 |
| --- | --- | --- |
| `cfg:custom-protocol-no-default` | `custom-protocol` feature 存在且**不设** default | 解析 `src-tauri/Cargo.toml` 的 `[features]` |
| `fmt:mcp-dispatch-name` | MCP 分发名 `mcp__<server>__<tool>` | `DISPATCH_PREFIX = "mcp__"` + `format!("{DISPATCH_PREFIX}{server_id}__")` |
| `fmt:mcp-container-profile` | 每 server 一个 AppContainer profile `shaniu.mcp.<id>` | `MCP_PROFILE_PREFIX = "shaniu.mcp."` |
| `fmt:mcp-result-envelope` | 输出包成 `<<<MCP_RESULT:<nonce> …` | 源码里的包裹模板字面量 |
| `fmt:eval-result-filename` | 产物路径 `<epoch>-<pid>-<role>.json` | `format!("{}-{}-{role}.json", now_ms(), id())` |

## 可信边界：6 种判定器盲区

读这份清单前先知道：**「判定通过」不等于「实现正确」，下面 6 类形态机器判不了或会误判**。

1. **serde `rename_all` 生成的驼峰名**：`configRoot` 在 Rust/TS 源码里零字面量，由 `ProductDirsView` 的 `#[serde(rename_all = "camelCase")]`（`config.rs:1272-1279`）产出 → 会被误报成「文档有、代码无」。**已补**：驼峰零命中时回退查蛇形字段，并要求同文件确有 camelCase 重命名。
2. **wire 蛇形名 ↔ 驼峰枚举变体**：`allowlist_hit` / `model_retry` / `subagent_spawned` 只以字符串和注释出现，实际是 `AllowlistHit` / `ModelRetry` / `SubagentSpawned` 变体并被 emit → 会被误判成「无调用点」。**已补**：蛇形名回退查驼峰变体。
3. **`#[test]` 天然无调用点**：测试函数只有定义，按「必须有调用点」判会整类误杀。**已补**（存在性判定）；「有分辨力」仍属 B 类。
4. **名字以 `&str` 常量存在**：工具名 `send_agent_message`、窗口类名 `tray_icon_app`、事件名 `user-terminal://frame` 是常量并被注册/emit 消费，字符串命中不等于死代码。**不补**：字符串命中与「被消费」在 AST 之外无法区分；脚本只在「命中全在字符串/注释、且无定义无调用」时判 **B**，不判 pass 也不判 A。
5. **basename 被当符号**：`sandbox_probe`、`mcp_sandbox` 是文件名而非标识符，按符号搜会得到注释/字符串命中。**已按类型分派**：`类型=文件` 的行按 basename 找（同名文件或目录即算）；其它类型里出现裸名字仍不猜。
6. **正则与通配**：`^[a-z0-9][a-z0-9-]{0,31}$`、`MAX_FILE_*` 无法按符号判定，只能人工核。**不补**。

补 1-3 各花 3-5 行启发式；补 4-6 需要 AST / 类型信息，属于另一张卡。**这轮到此为止，不硬补。**

## 断言清单
| ID | 文档位置 | 原句（逐字摘录，长句截断） | 期望符号 / 字面量 | 类型 | 备注 |
| --- | --- | --- | --- | --- | --- |
| R2-001 | 01:L96 | `scripts/check-deps.ps1` 按本表**正向校验**（白名单 + fail-closed） | `scripts/check-deps.ps1` 按本表**正向校验**（白名单 + fail-closed） | 文件 | |
| R2-002 | 01:L104 | `agent_loop/mod.rs:263` `self.broker.gate(...)` | `ApprovalBroker::gate` 调用点 | 符号 | |
| R2-003 | 01:L108 | `ToolDeclaration` 是两侧共享 DTO（`tool_router/spec.rs:8`、`mod.rs:19`） | `ToolDeclaration` | 符号 | |
| R2-004 | 01:L112 | 向注册表自登记（`tools/mod.rs:14`） | `tools/mod.rs` 注册调用 | 符号 | |
| R2-005 | 01:L113 | 命令执行前取沙箱策略（`tools/unified_exec.rs:21`） | `tools/unified_exec.rs` + 沙箱策略调用 | 文件 | |
| R2-006 | 01:L147 | `tool_router` 用 `try_register`/`re…` | `try_register`（tool_router） | 符号 | |
| R2-007 | 01:L142 | 复用 `AgentLoop::run_instance` 作为唯一执行路径与 `AgentInstance/TurnSummary/TokenTotals` | `run_instance`、`AgentInstance`、`TurnSummary`、`TokenTotals` | 符号 | |
| R2-008 | 01:L145 | `RegistryOrchestrator` 实现 `tool_router::orchestration::AgentOrchestrator` 并复用 `AgentScope/AgentOutcome` | `RegistryOrchestrator`、`AgentOrchestrator`、`AgentScope`、`AgentOutcome` | 符号 | |
| R2-009 | 01:L146 | `ToolAllowlist` 与 `intersect_allowlist` 是…三处共用的安全 DTO | `ToolAllowlist`、`intersect_allowlist` | 符号 | |
| R2-010 | 01:L151 | 端点元数据只有一套防御性解析（`parse_metadata`） | `parse_metadata` | 符号 | |
| R2-011 | 01:L153 | 共享内核承载 `ImageBudget` 与 `attachment()` | `ImageBudget`、`attachment` | 符号 | |
| R2-012 | 01:L154 | `view_image` 存字节并构造引用 | `view_image` | 符号 | |
| R2-013 | 01:L160 | 越界判定复用 `tool_router::resolve_in_workspace` … 复用 `workspace_files::MAX_FILE_…` | `MAX_FILE_` | 符号 | |
| R2-014 | 01:L161 | 错误分类复用 `model_client::ModelError` | `ModelError` 被 key_check 消费 | 符号 | |
| R2-015 | 01:L162 | 在**已有托盘图标**上挂 `Shell_NotifyIcon(NIF_INFO)` 气泡…用 `EnumWindows` 按窗口类 `tray_icon_app` | `Shell_NotifyIcon`、`EnumWindows`、`tray_icon_app` | 符号 | |
| R2-016 | 01:L168 | `config/model_catalog.rs` … `Config::builtin()` 的默认值来源（`include_str!` 一份 JSON 表 + 一个查表函数） | `config/model_catalog.rs`、`Config::builtin`、`include_str!` | 文件+符号 | |
| R2-017 | 01:L169 | `images` 是共享叶子，承载 `ImageRef` + `image_store` | `ImageRef`、`image_store` | 符号 | |
| R2-018 | 01:L189 | `ToolSpec`/`ToolDeclaration` 桥接、`PlanState`、`ToolContext` 都在这一层 | `PlanState`、`ToolContext`（tool_router） | 符号 | |
| R2-019 | 01:L328 | `resume_after_parent_narrowing_cannot_widen_child`（父侧收窄后 resume 不得回宽） | `resume_after_parent_narrowing_cannot_widen_child` | 测试 | |
| R2-020 | 01:L397 | 依赖方向 … 模块依赖扫描脚本 … `scripts/check-deps.ps1` | 依赖方向 … 模块依赖扫描脚本 … `scripts/check-deps.ps1` | 文件 | 与 R2-001 同源，去重后按一条计 |
| R2-021 | 01:L398 | `src-tauri/tests/event_schema.rs` | `src-tauri/tests/event_schema.rs` | 文件 | |
| R2-022 | 01:L399 | `src-tauri/tests/sandbox_probe.rs` | `src-tauri/tests/sandbox_probe.rs` | 文件 | |
| R2-023 | 01:L400 | `src-tauri/tests/sandbox_failclosed.rs` | `src-tauri/tests/sandbox_failclosed.rs` | 文件 | |
| R2-024 | 01:L401 | `src-tauri/tests/latency_budget.rs` | `src-tauri/tests/latency_budget.rs` | 文件 | |
| R2-025 | 01:L402 | `src-tauri/tests/approval_contract.rs` | `src-tauri/tests/approval_contract.rs` | 文件 | |
| R2-026 | 01:L403 | `src/__tests__/streaming.test.tsx` | `src/__tests__/streaming.test.tsx` | 文件 | |
| R2-027 | 01:L556 | 实现见 `config.rs:1495` `decide_root`（记录路径 `:1412`，可写探测 `:1444`） | `decide_root` | 符号 | |
| R2-028 | 01:L559 | **默认工作区固定为 `<数据根>\Workspace`**（… `AppState::workspace_root`） | `AppState::workspace_root` | 符号 | |
| R2-029 | 01:L611 | `src/host/boot-progress.ts` | `src/host/boot-progress.ts` | 文件 | |
| R2-030 | 01:L612 | `src/main.tsx` + `BootProgress.done()` | `BootProgress.done` | 符号 | |
| R2-031 | 01:L614 | Rust 侧另有 3 s 强制显示兜底（`lib.rs`，`FRONTEND_READY` 原子位 + …） | `FRONTEND_READY` | 符号 | |
| R2-032 | 01:L615 | `index.html` 12 s 看门狗…调 `window.__shaniuBootSettle()` 撤销 | `__shaniuBootSettle` | 符号 | |
| R2-033 | 01:L609 | 按上次解析结果（`localStorage['shaniu.theme.resolved']`…）预涂 `--boot-bg` | `shaniu.theme.resolved`、`--boot-bg` | 字面量 | |
| R2-034 | 01:L572 | `src-tauri/Cargo.toml` 声明 `custom-protocol = ["tauri/custom-protocol"]`，**不设 default** | `cfg:custom-protocol-no-default` | 结构 | 缺席断言；原按「字面量存在」判，判不了「不设」 |
| R2-035 | 01:L434 | 命令级沙箱 … `SandboxTransformRequest` 模式 | 命令级沙箱 … `SandboxTransformRequest` 模式 | 跳过 | 参考实现，不判定 |
| R2-036 | 02:L19 | 父代理可通过 `spawn_agent / resume_agent / wait_agent / send_agent_message` 四类工具… | `spawn_agent`、`resume_agent`、`wait_agent`、`send_agent_message` | 符号 | |
| R2-037 | 02:L55 | `subagent_registry`：进程内注册表，记录 id→task handle、状态机 | `subagent_registry` 模块 | 文件 | |
| R2-038 | 02:L101 | `EventMsg` 外包装为 `AgentEvent { agent_id, parent_id, msg: EventMsg }`…新增消息：`SubagentSpawned`、`SubagentExited(ExitSummary)` | `AgentEvent`、`SubagentSpawned`、`SubagentExited` | 符号 | |
| R2-039 | 02:L119 | 注册：`tools/list` → 转为内部 ToolSpec，命名 `mcp__<server>__<tool>` | `fmt:mcp-dispatch-name` | 结构 | `format!` 产物，运行期字符串验不了；改为验模板 |
| R2-040 | 02:L201 | `server_id` 来自…`id`（`^[a-z0-9][a-z0-9-]{0,31}$`），**不采用 server 自报名称** | `server_id` 来自…`id`（`^[a-z0-9][a-z0-9-]{0,31}$`），**不采用 server 自报名称** | 字面量 | |
| R2-041 | 02:L204 | `ToolRouter` 新增 `try_register(executor) -> Result<(), RegisterError>` 与 `remove(name)` | `try_register`、`remove`、`RegisterError` | 符号 | |
| R2-042 | 02:L266 | 已按**纯新增**实现 `sandbox::spawn_restricted_stdio` | `spawn_restricted_stdio` | 符号 | |
| R2-043 | 02:L267 | 每个 server 一个 AppContainer profile `shaniu.mcp.<id>` | `fmt:mcp-container-profile` | 结构 | 同上，验前缀常量而非运行期 profile 名 |
| R2-044 | 02:L279 | 现状名单（`sandbox::windows_impl::WINDOWS_CORE_ENV_VARS`…） | `WINDOWS_CORE_ENV_VARS` | 符号 | |
| R2-045 | 02:L321 | 配置来源是 `McpServerConfig.source`（`McpConfigSource::{User, Workspace}`…）…`connect_and_review_with` 在 spawn 前断言 `source == User` | `McpServerConfig`、`McpConfigSource`、`connect_and_review_with` | 符号 | |
| R2-046 | 02:L322 | `tests/mcp_trust.rs`… | `tests/mcp_trust.rs`… | 文件 | |
| R2-047 | 02:L328 | `tests/mcp_guard.rs`（FF-105） | `tests/mcp_guard.rs`（FF-105） | 文件 | |
| R2-048 | 02:L329 | `tests/mcp_namespace.rs` | `tests/mcp_namespace.rs` | 文件 | |
| R2-049 | 02:L330 | `tests/mcp_trust.rs` | `tests/mcp_trust.rs` | 文件 | |
| R2-050 | 02:L331 | `tests/mcp_process.rs` | `tests/mcp_process.rs` | 文件 | |
| R2-051 | 02:L307 | **新增命令（6 个）**：`mcp_list_servers()`、`mcp_list_exemptions()`、`mcp_connect_and_review(server_id, accept_new_binary?)`… | **新增命令（6 个）**：`mcp_list_servers()`、`mcp_list_exemptions()`、`mcp_connect_and_review(server_id, accept_new_binary?)`… | 命令 | |
| R2-052 | 02:L364 | 新增内置工具 `terminal`（风险分级 execute）…`{action: open/write/read/close, session_id?}` | `terminal` 工具 | 符号 | |
| R2-053 | 02:L372 | Web 能力走 **Rust 侧** `src-tauri/src/tools/web/` | Web 能力走 **Rust 侧** `src-tauri/src/tools/web/` | 文件 | |
| R2-054 | 02:L378 | `SearchProvider` trait（`id` + `search`）…`FallbackProvider` 回退链 | `SearchProvider`、`FallbackProvider` | 符号 | |
| R2-055 | 02:L388 | 搜索页抓取与 `web_fetch` 共用 `WebClient` | `WebClient` | 符号 | |
| R2-056 | 02:L182 | MCP 工具输出统一包成 `<<<MCP_RESULT:<per-process-random-128bit> ... END:<same-nonce>>>>` | `fmt:mcp-result-envelope` | 结构 | nonce 运行期生成；验包裹模板 |
| R2-057 | 02:L487 | `src-tauri/tests/cascade_cancel.rs` | `src-tauri/tests/cascade_cancel.rs` | 文件 | |
| R2-058 | 02:L488 | `src-tauri/tests/permission_min.rs` | `src-tauri/tests/permission_min.rs` | 文件 | |
| R2-059 | 02:L489 | `src-tauri/tests/shared_budget.rs` | `src-tauri/tests/shared_budget.rs` | 文件 | |
| R2-060 | 02:L491 | `src-tauri/tests/mcp_guard.rs` | `src-tauri/tests/mcp_guard.rs` | 文件 | 与 R2-047 同址，按一条计 |
| R2-061 | 02:L492 | `src-tauri/tests/orchestrate_depth.rs` | `src-tauri/tests/orchestrate_depth.rs` | 文件 | |
| R2-062 | 04:L43 | `allowlist_hit` \| `allowlist_hit`、`rule_id` | SH-120 新增（additive） | `allowlist_hit` 变体 + 三字段 | 符号 | |
| R2-063 | 04:L48 | `model_retry` \| `model_retry`、`retry_id` | `ModelRetry` 变体 | 符号 | |
| R2-064 | 04:L68 | 子代理事件（SH-053，additive，**已实现**）：`subagent_spawned {child_id, task, name, label, created_at_ms}`… | `subagent_spawned`、`subagent_exited`、`subagent_message` | 符号 | |
| R2-065 | 04:L76 | `send_task` \| `send_task`、`sandbox_mode` | `send_task` 的 `sandbox_mode` | 符号 | |
| R2-066 | 04:L83 | `archive_session` / `unarchive_session` | `archive_session`、`unarchive_session` | 命令 | |
| R2-067 | 04:L84 | `pin_session` / `unpin_session` | `pin_session`、`unpin_session` | 命令 | |
| R2-068 | 04:L85 | `insert_session_before` | `insert_session_before` | 命令 | |
| R2-069 | 04:L86 | 归档/置顶/顺序侧车（`rollouts/session-order.json`…） | `session-order.json` | 字面量 | |
| R2-070 | 04:L87 | `create_workspace` \| `{path}` … 规范拼写 = canonicalize…再剥掉 Windows `\\?\` verbatim 前缀 | `create_workspace` 命令 | 命令 | |
| R2-071 | 04:L88 | `rename_workspace` / `delete_workspace` | `rename_workspace`、`delete_workspace` | 命令 | |
| R2-072 | 04:L89 | `insert_workspace_before` | `insert_workspace_before` | 命令 | |
| R2-073 | 04:L90 | `create_session` …固化进 `config_snapshot.workspace_root` | `create_session` + `config_snapshot` | 命令 | |
| R2-074 | 04:L91-95 | `get_settings` / `save_settings` / `save_providers` / `delete_api_key` / `store_api_key` / `has_api_key` | `get_settings` / `save_settings` / `save_providers` / `delete_api_key` / `store_api_key` / `has_api_key` | 命令 | |
| R2-075 | 04:L97 | `get_product_dirs` \| `get_product_dirs`、`configRoot`、`workspaceRoot` | `{configRoot, dataRoot, workspaceRoot, homeDir}` | `get_product_dirs` | 命令 | |
| R2-076 | 04:L98 | `discover_models` \| `discover_models`、`base_url` | `discover_models` | 命令 | |
| R2-077 | 04:L99 | `pick_folder` / `open_in_explorer` | `pick_folder`、`open_in_explorer` | 命令 | |
| R2-078 | 04:L100 | `read_clipboard_text` / `write_clipboard_text` | `read_clipboard_text`、`write_clipboard_text` | 命令 | |
| R2-079 | 04:L101 | `read_session_events` \| `read_session_events`、`afterEventId` | `read_session_events` | 命令 | |
| R2-080 | 04:L102 | `latest_session_event_id` | `latest_session_event_id` | 命令 | |
| R2-081 | 04:L103 | `sandbox_status` \| `sandbox_status`、`enforcement` | `{mode, os, os_enabled, enforcement, profile, note}` | `sandbox_status` | 命令 | |
| R2-082 | 04:L104 | `session_sandbox_mode` | `session_sandbox_mode` | 命令 | |
| R2-083 | 04:L105 | `set_session_sandbox_mode` | `set_session_sandbox_mode` | 命令 | |
| R2-084 | 04:L106 | `gateway_info` \| `gateway_info` | `{port, token}` | `gateway_info` | 命令 | |
| R2-085 | 04:L107 | `workspace_file_stat` / `workspace_file_read` / `workspace_file_read_bytes` / `workspace_file_list` | `workspace_file_stat` / `workspace_file_read` / `workspace_file_read_bytes` / `workspace_file_list` | 命令 | |
| R2-086 | 04:L108 | `search_sessions` …（`<data_root>/cache/session-search.db`…） | `search_sessions` + db 路径 | 命令 | |
| R2-087 | 04:L109 | `export_probe` \| `{sessionId}` \| `ExportHeader{…}` | `export_probe` | 命令 | |
| R2-088 | 04:L110 | `pick_save_file` … 返回值是宿主签发的保存票据 | `pick_save_file` | 命令 | |
| R2-089 | 04:L111 | `save_file_bytes` | `save_file_bytes` | 命令 | |
| R2-090 | 04:L112 | `upload_attachment` | `upload_attachment` | 命令 | |
| R2-091 | 04:L113 | `read_attachment` | `read_attachment` | 命令 | |
| R2-092 | 04:L114 | `open_in_app_catalog` | `open_in_app_catalog` | 命令 | |
| R2-093 | 04:L115 | `open_in_app_launch` | `open_in_app_launch` | 命令 | |
| R2-094 | 04:L116 | `workspace_path_applications` | `workspace_path_applications` | 命令 | |
| R2-095 | 04:L117 | `open_workspace_path` | `open_workspace_path` | 命令 | |
| R2-096 | 04:L118 | `list_file_references` | `list_file_references` | 命令 | |
| R2-097 | 04:L119 | `list_session_references` | `list_session_references` | 命令 | |
| R2-098 | 04:L120 | `list_skills` \| `{sessionId}` | 命令 | 命令 | |
| R2-099 | 04:L121 | `export_session_log` \| `export_session_log`、`dest` | 命令 | 命令 | |
| R2-100 | 04:L122 | `set_plan_mode` / `get_plan_mode` | `set_plan_mode`、`get_plan_mode` | 命令 | |
| R2-101 | 04:L123 | `compact_session` \| `{agentId}` | 命令 | 命令 | |
| R2-102 | 04:L124 | `frontend_ready` \| `frontend_ready` | 命令 | 命令 | |
| R2-103 | 04:L125 | `rename_session` \| `rename_session`、`agent_id` | 命令 | 命令 | |
| R2-104 | 04:L126 | `set_approval_mode` \| `{mode}` | 命令 | 命令 | |
| R2-105 | 04:L127 | `session_of_agent` \| `session_of_agent` | 命令 | 命令 | |
| R2-106 | 04:L128 | `send_subagent_message` | `send_subagent_message` | 命令 | |
| R2-107 | 04:L129 | `interrupt_subagent` | `interrupt_subagent` | 命令 | |
| R2-108 | 04:L130 | `probe_capabilities` \| `probe_capabilities`、`force` | 命令 | 命令 | |
| R2-109 | 04:L131 | `fetch_model_metadata` | `fetch_model_metadata` | 命令 | |
| R2-110 | 04:L132 | `goal_*`（6 条）：`goal_get`；`goal_create`；`goal_edit`；`goal_pause`；`go…` | `goal_get`、`goal_create`、`goal_edit`、`goal_pause`、`goal_resume` | 命令 | |
| R2-111 | 04:L133 | `job_*`（3 条）：`job_list`；`job_read`；`job_kill` | `job_list`、`job_read`、`job_kill` | 命令 | |
| R2-112 | 04:L170 | 常驻状态条 \| `shell.bottom` \| root（单实例） | `shell.bottom` 槽位 id | 字面量 | |
| R2-113 | 04:L173 | 样式只用 `--dsw-alias-*` 令牌…（`theme-tokens.spec.ts` 机检） | `theme-tokens.spec.ts` | 文件 | |
| R2-114 | 04:L181 | 每个已信任的 stdio server 跑在**自己**的容器 profile `shaniu.mcp.<id>` 里（`sandbox::ensure_mcp_container`） | `ensure_mcp_container` | 符号 | |
| R2-115 | 04:L200 | **只碰这些位置**（`sandbox/acl.rs::write_scope` + `\Device\Null`） | `write_scope` | 符号 | |
| R2-116 | 04:L221 | `restore_null_device()` 还原设备 DACL、`recover_pending()` 按备份还原目录 DACL | `restore_null_device`、`recover_pending` | 符号 | |
| R2-117 | 04:L224 | `AclManager::revoke(路径, SID)` 与 `device_acl::revoke_container_access(SID)` | `AclManager::revoke`、`device_acl::revoke_container_access` | 符号 | |
| R2-118 | 04:L227 | 触发点：停止 / 撤信 / 卸载 → `release_mcp_server`；进程退出 → `release_all_mcp_containers` | `release_mcp_server`、`release_all_mcp_containers` | 符号 | |
| R2-119 | 04:L237 | `sandbox::tests::mcp_null_device_grant_is_revoked_on_release` | `mcp_null_device_grant_is_revoked_on_release` | 测试 | |
| R2-120 | 04:L247 | `user_terminal_shells` | `user_terminal_shells` | 命令 | |
| R2-121 | 04:L248-253 | `user_terminal_open` / `_write` / `_resize` / `_close` / `_close_all` / `_active_count` | `user_terminal_open` / `_write` / `_resize` / `_close` / `_close_all` / `_active_count` | 命令 | |
| R2-122 | 04:L259 | 终端数据出口是 Tauri 事件 `user-terminal://frame` | `user-terminal://frame` | 字面量 | |
| R2-123 | 04:L266 | 提示通道 `user-terminal://tray-hint` | `user-terminal://tray-hint` | 字面量 | |
| R2-124 | 04:L297-310 | 工具表：`unified_exec`、`apply_patch`、`read_file`、`list_dir`、`search`、`plan`、`todo_write`、`spawn_agent`、`resume_agent`、`wait_agent`、`send_agent_message`、`terminal`、`web_search`、`web_fetch`、`view_image` | `unified_exec`、`apply_patch`、`read_file`、`list_dir`、`search`、`todo_write`、`web_search`、`web_fetch` | 符号 | 2026-11-11 裁决：`request_user_input` 删文档行不实现（`f84e804`），本行同步删去该名 |
| R2-125 | 04:L328 | `SubagentRef` \| `SubagentRef`、`rollout_path` | 父文件中的子代理引用 | `SubagentRef` | 符号 | |
| R2-126 | 04:L329 | `SandboxMode` \| `SandboxMode`、`at_ms` | **会话内档位切换事件** | `SandboxMode` rollout 项 | 符号 | |
| R2-127 | 04:L510 | 内置表（精确）\| `resources/model-catalog.json` … `include_str!` | `resources/model-catalog.json` | 文件 | |
| R2-128 | 04:L519 | **能力项 id（唯一登记表 `capability::ITEMS`）** | `capability::ITEMS` | 符号 | |
| R2-129 | 04:L554 | 缓存键 = … + **探针版本**（`PROBE_VERSION`） | `PROBE_VERSION` | 符号 | |
| R2-130 | 04:L630 | **环境声明细则**（`prompt.rs::environment_block`） | `environment_block` | 符号 | |
| R2-131 | 04:L637 | **前缀稳定（缓存）细则**（`prompt.rs::SessionPrompt`） | `SessionPrompt` | 符号 | |
| R2-132 | 04:L642 | `system_prompt_is_identical_across_turns` 直接断言这一点 | `system_prompt_is_identical_across_turns` | 测试 | |
| R2-133 | 04:L644 | **项目规则细则**（`rules.rs::discover`） | `discover`（rules.rs） | 符号 | |
| R2-134 | 04:L648 | 文件头写 `<!-- shaniu-rules: resident 0.,1.0,2.,2.2,3.,4.2,4.3,11.3 -->` | 声明行前缀 `shaniu-rules:` | 字面量 | |
| R2-135 | 04:L659 | `injected_rules_must_contain_iron_law_and_degradation_redline` … `ondemand_note_count_m…` | `injected_rules_must_contain_iron_law_and_degradation_redline` | 测试 | |
| R2-136 | 04:L660 | `absent_declaration_falls_back_to_tier_layers_verbatim`、`broken_declaration_falls_back_to_legacy_with_banner_on_first_line` | `absent_declaration_falls_back_to_tier_layers_verbatim`、`broken_declaration_falls_back_to_legacy_with_banner_on_first_line` | 测试 | |
| R2-137 | 04:L677 | 超过 `PROMPT_TOKEN_LIMIT = 2500` 的不变量① | `PROMPT_TOKEN_LIMIT` | 符号 | |
| R2-138 | 04:L699 | `PER_TASK_TIMEOUT` 是**评测脚本的护栏**（`tests/evals_baseline.rs`）…（`SHANIU_EVAL_MAX_TURNS` 可改） | `PER_TASK_TIMEOUT`、`tests/evals_baseline.rs` | 文件+符号 | |
| R2-139 | 04:L741 | `error_code` 取受控词表（`agent_loop/error_codes.rs`），不得写成自由文本 | `agent_loop/error_codes.rs` | 文件 | 2026-11-11 裁决后文档措辞已改（`dc37515`），不再是「枚举」 |
| R2-140 | 04:L751 | **产物路径只有一种形态**：`evals/results/<epoch>-<pid>-<role>.json` | `fmt:eval-result-filename` | 结构 | 文件名运行期生成；验 `format!` 模板 |
| R2-141 | 04:L769 | 前端启动时与后端握手 `protocol_version` | `protocol_version` | 字面量 | |
| R2-142 | 08:L6 | 本版版本号 `0.2.5`（`package.json` / `src-tauri/Cargo.toml` / `src-tauri/tauri.conf.json` 三处一致…） | `0.2.5` | 配置 | |
| R2-143 | 08:L14 | `tauri.conf.json` 的 `bundle.active` 为 `false`…`pnpm release:portable` | `bundle.active=false` + npm script | 配置 | |
| R2-144 | 08:L15 | 兜底 `SHANIU-Portable-<版本>.zip`（`SHANIU.exe` + `portable.marker` + `README.txt`） | `portable.marker` | 字面量 | |
| R2-145 | 08:L17 | 判定顺序 `SHANIU_DATA_DIR` > 已有记录 > `portable.marker`… | `SHANIU_DATA_DIR` | 字面量 | |
| R2-146 | 08:L17 | （`%LOCALAPPDATA%\SHANIU\data-root\<exe目录…` | `data-root` | 字面量 | |
| R2-147 | 08:L39 | CSP…`img-src 'self' blob:; worker-src 'self' blob:` | `worker-src` | 配置 | |
| R2-148 | 08:L42 | 产物门禁 `check-dist-assets` ok | `check-dist-assets` | 字面量 | |
| R2-149 | 08:L216 | `sandbox.os = "logical"`…逐 server 的 `unsandboxed = true` 显式豁免（必须填 `unsandboxed_reason`…） | `sandbox.os`、`unsandboxed_reason`、`allow_network` | 配置 | |
| R2-150 | 08:L220 | `PROMPT_VERSION` → `2026-11-10.1` | `PROMPT_VERSION` | 符号 | 日期值可能已再变，按符号判定 |
| R2-151 | 08:L235 | shell 列表只探测 `cmd` 与 `pwsh.exe`，不提供 Git Bash / WSL | `pwsh.exe` 探测 | 字面量 | |
| R2-152 | 08:L255 | `ExitSummary.artifacts` 恒空（同 SH-051 归因缺口） | `artifacts` 字段 | 符号 | 文档自称缺口，不进 A |
| R2-153 | 07:L22 | `aliases.ts`（**本地新增，非上游文件**）…维护 `@deepseek-ai/dsh-*` → 实际路径的别名表 | `src/dsh/aliases.ts` | 文件 | |
| R2-154 | 07:L26 | `treeKeyboard.ts` 为本地新增 | `packages/client/ui-workspace/src/client/rows/treeKeyboard.ts` | 文件 | |
| R2-155 | 07:L33 | 新增 `PermissionPresetDescriptionKey`、`PRESET_DESCRIPTION_KEYS`、`DEFAULT_PRESET_DESCRIPTIONS`、`displayPermissionPres…` | `PermissionPresetDescriptionKey`、`PRESET_DESCRIPTION_KEYS`、`DEFAULT_PRESET_DESCRIPTIONS` | 符号 | |
| R2-156 | 07:L34 | 新增 `presetDetail()`：下拉 `detail` 走词典 | `presetDetail` | 符号 | |
| R2-157 | 07:L36 | 新增 `ctx.provide('settingsShell', { openSection })` | `'settingsShell'` | 字面量 | |
| R2-158 | 07:L49 | 新增哨兵 `PERMISSION_UNAVAILABLE = 'unavailable'` | `PERMISSION_UNAVAILABLE` | 符号 | |
| R2-159 | 07:L93 | `src/header-actions/ExplorerButton.tsx`、`src/header-actions/explorer-button.css` | `src/header-actions/ExplorerButton.tsx`、`src/header-actions/explorer-button.css` | 文件 | |
| R2-160 | 07:L94 | `src/header-actions/session-events.ts` … 多一个可选 `timeUnreliable: true` | `timeUnreliable` | 符号 | |
| R2-161 | 10:L86 | 失败行现在给出具体原因与失败码…（`MessageItem.tsx:132-165`） | `MessageItem.tsx` | 文件 | |
| R2-162 | 10:L92 | `.github/workflows/ci.yml` 的 `product-smoke` job | `.github/workflows/ci.yml` 的 `product-smoke` job | 配置 | |
| R2-163 | 10:L205 | `#![cfg(windows)]`（测试文件级）…`dependency_direction` / `sandbox_probe` / `sandbox_failclosed` / `mcp_sandbox` | `#![cfg(windows)]`（测试文件级）…`dependency_direction` / `sandbox_probe` / `sandbox_failclosed` / `mcp_sandbox` | 文件 | |
| R2-164 | 10:L230 | `scripts/package-portable.ps1` / `check-deps.ps1` | `scripts/package-portable.ps1`、`scripts/check-deps.ps1` | 文件 | |
| R2-165 | 10:L231 | `src/host/desktop-env.ts:34-38` `detectDevice()` 三平台 | `detectDevice` | 符号 | |
| R2-166 | 10:L252 | 构建失败且错误信息含 `SHANIU 只支持 Windows` | 构建失败且错误信息含 `SHANIU 只支持 Windows` | 字面量 | 2026-11-11 以代码为准统一为「只」（`e73a204`） |
| R2-167 | 10:L313 | `src-tauri/src/tools/web/search.rs` 定义 `SearchFailure{Parse, Blocked, Network}`，`SearchError::guidance()`…`finish()` | `SearchFailure`、`SearchError`、`guidance` | 符号 | |
| R2-168 | 10:L345 | `unimplemented()` 现为 `adapter.ts:646-648`…「当前版本还不支持这个操作（{name}）」 | `unimplemented` + 文案 | 符号 | |
| R2-169 | 10:L359 | `TurnErrorInfo{code,message,http_status,provider_code,provider_message,attempts}`（`events.rs:45-61`…） | `TurnErrorInfo` + 6 字段 | 符号 | |
| R2-170 | 10:L360 | `key_check.rs:279` `check_api_key`，分类 `:204` `from_model_error`，三态 `valid/invali…` | `check_api_key`、`from_model_error` | 符号 | |
| R2-171 | 10:L364 | 核验引用：`src-tauri/src/sandbox/acl.rs:260`、`src-tauri/src…` | `src-tauri/src/sandbox/acl.rs` | 符号 | |
| R2-172 | 10:L324 | 一键导出诊断包 … **未实现** | — | — | 文档自称未实现，不进 A |
| R2-173 | 10:L325 | 配置 schema 版本化 … **未实现** | — | — | 文档自称未实现，不进 A |
| R2-174 | 12:L121 | **已实现**：`src-tauri/src/model_client/retry.rs:15` `RetryPolicy`——白名单 `:50`… | `RetryPolicy` | 符号 | |
| R2-175 | 12:L127 | `src-tauri/src/tools/web/search.rs`：`SearchProvider` trait + 内置 Bing（免 key）…`fetch.rs` 抓取 | `tools/web/fetch.rs` | 文件 | |
| R2-176 | 12:L130 | `app_state.rs` 的 `begin_task`/`cancel_active`；`agent_loop/mod.rs` 的 … `ToolCallResult::Cancelled` | `begin_task`、`cancel_active`、`ToolCallResult::Cancelled` | 符号 | |
| R2-177 | 12:L132 | `src-tauri/src/jobs.rs`、`tools/job_tools.rs` | `src-tauri/src/jobs.rs`、`src-tauri/src/tools/job_tools.rs` | 文件 | |
| R2-178 | 12:L133 | `src-tauri/src/model_client/error.rs`：错误大类 …（Auth/Quota/RateLimited/Timeout/Network/Server/EndpointUnsupported/BadRequest） | `model_client/error.rs` | 文件 | |
| R2-179 | 12:L119 | `src-tauri/src/session_export/mod.rs` 导出 ZIP | `src-tauri/src/session_export/mod.rs` 导出 ZIP | 文件 | |
| R2-180 | 12:L129 | `src-tauri/src/compaction/mod.rs`…`session_compact.rs` 接线 | `session_compact.rs` | 文件 | |
| R2-181 | 12:L131 | `src-tauri/src/subagent_registry/mod.rs`：状态机、子令牌级联、并发上限、`wait_agent` 收口 | `subagent_registry/mod.rs` | 文件 | |
| R2-182 | 04:L97 | `get_product_dirs` 返回 `{configRoot, dataRoot, workspaceRoot, homeDir}` | `configRoot`、`dataRoot`、`workspaceRoot`、`homeDir` | 符号 | 盲区 1 的活例：四个驼峰名在 Rust/TS 源码里零字面量，由 `#[serde(rename_all = "camelCase")]` 生成 |

---

## B 类：不可机械判定（B ≠ 已验证）

第一轮登记 12 条 B。本轮把其中 4 组（5 行）改成了结构检查（R2-034 不设 default、R2-039/R2-043
`format!` 命名模板、R2-056 nonce 包裹、R2-140 产物文件名）——判法从「查那个字符串存在」改成
「查生成它的模板还在」，能抓住「改了命名规则没同步文档」，所以它们不再计入 B。

剩下的 8 条分两种，都**没有验证过**：

**(a) 脚本直接标 B 的 5 行**（运行输出里能看到，原因是「只以字符串/注释出现」或「文档自称未实现」）：

| ID | 断言 | 为什么判不了 |
| --- | --- | --- |
| R2-015 | `tray_icon_app` 窗口类名 | 只在字符串/注释里出现；字符串命中证明不了被 `RegisterClass` 消费（盲区 4） |
| R2-035 | `SandboxTransformRequest` 模式 | 参考实现（codex）概念，本仓本就不该有 |
| R2-152 | `ExitSummary.artifacts` 恒空 | 「恒空」是运行期属性；文档自己标了缺口 |
| R2-172 | 一键导出诊断包**未实现** | 判「未实现」等于判全仓缺席，缺席只能人工核 |
| R2-173 | 配置 schema 版本化**未实现** | 同上 |

**(b) 脚本判 pass、但只证明了「存在」的 3 组**（pass 不是验证，必须跑起来）：

| 组 | 断言 | 为什么 pass 不算数 |
| --- | --- | --- |
| 1 | R2-019 / R2-119 / R2-132 / R2-135 / R2-136 五条用例 | 用例名存在只证明「写了」，证不了「不做就失败」——反向对照要执行 |
| 2 | 02 «退避 1s..8s、崩溃只重启一次» | 行为时序，只能跑 |
| 3 | 04 §6.1 前缀稳定 / 30% 保留 / 单条 4000 token 上限 | 策略行为，只能跑 |

**B ≠ 已验证**：保留这些条目，是为了让「没验」这件事可见，而不是为了让总数好看。

## 最近一次重跑

- 2026-11-11，HEAD `f9545be`：182 行断言，通过 305 个判定点 / A 0 / B 5，退出码 0。
- 三条盲区规则的活例（`-ShowPass` 可见）：`configRoot` → `serde-camel -> config_root`；
  `allowlist_hit` 类 → `wire-variant -> AllowlistHit`；5 处 `#[test]` → `rust-test`。
- 反向对照（证明会红）：塞一个不存在的符号 → A + 退出码 1；清单不存在 / 解析 0 行 /
  某行无可判定 token → 退出码 2。四条都实测过。
- 记录写完即失效——重跑看输出，别信这里的数字。

## 历史（第一轮，2026-11-11）

- 181 行断言；独立断言 175 条（1 条参考实现跳过、2 条同源去重、3 条文档自称未实现单列 B）
- 当轮结果：A 3 条、引用行号漂移 1 条、B 12 条、其余成立。三条 A 已按用户裁决落地：
  `request_user_input` 删文档行不实现（`f84e804`）、`error_code` 改受控词表并补承载物（`dc37515`）、
  Windows 文案以代码为准（`e73a204`）。
