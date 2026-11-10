# dsh 后端参照：能力清单与对位表

> 状态：能力清单 v1（2026-11-11），对位表同文维护。
> 参考源集合：**codex 运行时源码 + ZCode 运行时源码 + dsh 全栈（前端 + 后端）**。
> 实现栈不变：Tauri 2 / Rust 后端 / React 前端。本文只回答"dsh 后端怎么做、我们差在哪"，不包含实现方案。

## 1. 判据与边界

1. **判据是行为不是结构**：同一输入进得去、可观察结果与副作用一致才算对齐；代码结构不要求相同，语言不同不是差距。
2. **只取协议、状态机、数据结构、错误语义**，不搬它的运行时与生态依赖（Node、pnpm workspace、cordis 插件总线、打包脚本）。
3. **不纳入对齐的部分显式列出**（§2），否则差异表会混进大量无关项。

`align/ledger.json` 只做前端文件级对位，**不扩到后端**：后端语言栈不同，扩进去只会得到一张全是 `n/a` 的空账本。后端对位由本文维护，`docs/10` §2 挂链。

## 2. 不纳入对齐的部分

| 范围 | 具体对象（`src/dsh/` 下） | 不纳入的理由 |
| --- | --- | --- |
| Node 运行时与包管理 | pnpm workspace、`packages/bundle/*`、`packages/sdk/*`、`packages/host/webserver` | 我们由 Tauri 宿主与 Rust 进程模型承担，替换掉才是目的 |
| 插件/依赖注入框架 | `packages/extensions/cordis-*`、`packages/boot/plugin-*` | 属生态机制，不是可观察行为 |
| 遥测与账号 | `packages/host/product-telemetry-otel`、`packages/identity/*`、`packages/api/account-controller` | 我们不接云端账号体系 |
| 客户端 UI 包 | `packages/client/ui-*`、`packages/client/*` | 前端另有对位账本，不在本文重复 |
| 平台专属实验包 | `packages/experimental/*`、`packages/ptc-runtime/ptc-runtime-node`、`src/dsh/native/*` | 未进入稳定面，且多数绑定 Node 运行时 |
| 构建与分发链 | `scripts/`、`native/system` | 与用户可观察行为无关 |

## 3. 能力清单（按能力域）

### 3.1 会话与轨迹

- **追加写事件日志，不重写历史**：目录切换等事件以追加方式进入会话日志，稳定系统提示前缀不被替换（`src/dsh/packages/session/working-directory/README.md`；测试 `working-directory.spec.ts` 验证重载后可 `snapshotEvents()` 回放）。
- **事件结构固定为 `{type, seq, time, data}`**，会话文件是事件 artifact（`src/dsh/packages/session/session-format/src/types.ts:28-42`）。
- **格式链负责物理恢复**：`session-format-catalog` 串起 `session-format` 与 v0→v1…v4 的迁移编解码，读侧做 header-only 分类、单趟行恢复（`src/dsh/packages/session/session-format-catalog/README.md`）。
- **投影与查询分离**：`session-projection` / `session-projection-cache` / `session-query` / `session-stats` / `session-turn-outline` 从事件流派生可查询视图，而不是另存一份真相。
- **导出**：`/export` 命令与 ZIP 路由，支持 `sessionId` 与 `includeDescendants`，流式写 `application/zip`（`src/dsh/packages/session-query/session-log-export/src/index.ts:75-115`、`:173-233`）。

### 3.2 模型调用与流式

- **增量拼装集中在累加器**：`text-delta` / `reasoning-delta` / `tool-call-delta` 被压成 chunks 并保留时间戳与索引，`usage`、`finish` 等原样保留（`src/dsh/packages/llm/llm/src/assistant-stream.ts:99-193`）。
- **可逆**：`expandAssistantStream` 按 `dt` 还原原始 `TimedStreamChunk` 序列，用于回放与统计（同文件 `:196-220`）。
- **用量统计单独投影**：`token-meter` 按 `request/header`、`step/start`、`step/end`、`assistant/message` 推进，assistant 侧用同一套流拼装结果计算（`src/dsh/packages/llm/token-meter/src/measurement-projection.ts:44-78`）。

### 3.3 失败与重试

- **可重试码是白名单**：`EMPTY_RESPONSE`、`RATE_LIMIT`、`SERVER`、`TIMEOUT`、`TRANSPORT`（`src/dsh/packages/llm/llm/src/retry-policy.ts:14-24`）。
- **退避有上限与抖动**：`initialDelayMs * 2^(retry-1)` 乘抖动、封顶 `maxDelayMs`（`src/dsh/packages/llm/llm-retry/src/index.ts:59-64`）。
- **策略按 provider 固化**，区分 `normal` 与 `always`，并接受服务端 `Retry-After`（`retry-policy.ts:149-195`、`llm-retry/src/index.ts:190-237`）。
- **重试本身是会话事件**：重试过程写入 session，用户与导出都能看到。

### 3.4 工具调用与审批

- **注册中心管 schema 与可见性**：`ToolsService.register()` 校验 schema/output，`restrict()` 设 allow/deny，`view()` 按 scope chain 合并并支持 shadow（`src/dsh/packages/core/tools/src/index.ts:98-218`）。
- **执行器契约只有两件事**：`describe()` 产出模型可见 schema，`execute()` 批量执行（`src/dsh/packages/core/tools/src/executor.ts:63-85`）。
- **原生执行器与 PTC 执行器分工**：PTC 只暴露 `run_code` 一个直接工具，其余调用走受控管道（`src/dsh/packages/core/tool-executor-native/src/index.ts:7-35`、`tool-executor-ptc/src/index.ts:23-62`）。
- **审批请求结构**：`{agent, toolName, callId, reason, signal}`；策略只有 `ask` / `never` 两档（`src/dsh/packages/interaction/user-approval/src/index.ts:124-149`、`:167-220`）。
- **预设把沙箱与审批绑在一起**：`PresetSpec` 同时设置 sandbox 与 approval；默认 `workspace-write/ask`，`AUTO_PRESET` 固定 `danger-full-access/ask`（`src/dsh/packages/interaction/permission-presets/src/index.ts:57-86`、`:152-190`）。

### 3.5 沙箱与权限

- **策略三档 + 可写白名单**：`read-only` / `workspace-write` / `danger-full-access`，按 session / override / default 解析，并维护 workspace root 与额外可写目录（`src/dsh/packages/sandbox/sandbox-policy/src/index.ts:128-205`）。
- **Windows 侧用写受限令牌 + 写 SID + DACL 交集**：`WRITE_RESTRICTED` token、workspace/temp write SIDs（`src/dsh/packages/sandbox/sandbox-windows-acl/src/index.ts:2-44`）。
- **fail-closed**：参数或准备不合法直接拒绝，不降级为无沙箱（同文件 `:154-217`）。
- **超时是独立策略包**：`packages/guard/timeout-policy`。

### 3.6 MCP 接入

- **两种传输**：stdio 与 streamable-http，配置含命令/URL、超时、重连、模板展开（`src/dsh/packages/mcp/mcp-client/src/index.ts:73-138`）。
- **命名冲突按 serverName 处理**：启动失败按配置决定是否致命（同文件 `:195-221`）。
- **管理面分离**：manager 负责 entry 校验、生命周期、`restart/probe/watch`（`src/dsh/packages/mcp/mcp-manager/src/index.ts:1-18`、`:120-165`）。
- **资源单独成面**：`list_mcp_resources` / `list_mcp_resource_templates` / `read_mcp_resource` 按 agent scope 解析 provider（`src/dsh/packages/mcp/mcp-resources/src/index.ts:46-126`）。

### 3.7 检索与网页搜索

- **provider 注册制**：`ctx.web` 注册 search/fetch provider，按"已配置 / 可用 / 多候选 / 缺失"选择（`src/dsh/packages/web/web/src/index.ts:69-81`）。
- **搜索前先判认证状态**（`searchReady()`），执行后截断结果（同文件 `:176-220`）。
- **会话检索独立**：`packages/experimental/session-search`（实验面，不在稳定面内）。

### 3.8 提示词与上下文

- **顺序是集中配置**：`SECTION_ORDERS` 与 `CONTEXT_ORDERS` 固定系统提示与运行时上下文的拼接次序（`src/dsh/packages/core/system-prompt/src/index.ts:136-185`）。
- **组装是瀑布**：`SystemPrompt.assemble()` 合并全局/作用域 section、context、工具 schema、变量，并触发 `system-prompt/assemble` 扩展点（同文件 `:620-695`）。
- **上下文注入有固定格式**：`@path` 文件引用模板（`packages/context/file-reference/src/index.ts:16-18`）；跨会话引用带"不可信只读"声明与预算裁剪、省略提示（`packages/context/session-reference/src/index.ts:57-64`、`:310-369`）。

### 3.9 压缩与缓存

- **两种触发**：压力（`pressure`）与上下文溢出（`context-overflow`），由 `compactIfNeeded` 返回可压缩范围或 `null`（`src/dsh/packages/compaction/compaction/src/index.ts:124-140`）。
- **手动压缩只在 idle 执行**，带持久标记、锁、取消与可见失败（同文件 `:142-166`）。
- **过程是事件**：`compaction/start|summary|end|prune`，失败原因记在 `end.error`（`packages/compaction/compaction/src/types.ts:17-90`）。
- **结果记录替换与丢弃范围**：`CompactionResult` 带 summary、被替换区间、被丢弃/摘要的 surface seqs 与 token 计数（同文件 `:93-120`）。
- **图片可卸载**：`packages/compaction/compaction-image-offload`。

### 3.10 取消与中断

- **信号只读**：工具执行拿到的是 `AbortSignal`，调用方与工具都不能替换或移除（`src/dsh/packages/core/tools/tests/execution-signal-types.spec.ts`）。
- **分发前可取消**：取消发生在工具分发前时返回 `TOOL_ABORTED_BEFORE_DISPATCH`（`packages/core/tools/src/events.ts`、`tests/activation.spec.ts`）。
- **取消会替换成功结果**：调用方取消优先于已完成的结果。
- **取消不污染持久化**：被取消的目录变更事件不进入会话事件日志（`packages/session/working-directory/tests/working-directory.spec.ts`）。

### 3.11 子代理与后台任务

- **工具立即返回 activation**：`subagent` 工具结果只有 `{kind: 'activation', name}`，不等最终输出；等待由 subagent service 提供（`src/dsh/packages/subagent/tool-subagent/README.md`）。
- **结果以完成通知回传**：子任务消息与 completion notice 独立追加到父历史，不占父回合的同步等待。
- **子代理有独立持久会话**：local spawn/fork 子代理保留 durable sessions；外部 ACP/Codex/Claude Code 只执行一次（`packages/subagent/subagent/README.md`）。
- **共享边界**：admission、cancellation、ownership、discovery 在父子间共享。
- **后台任务**：`packages/jobs/*` 提供 job 生命周期与本地实现（`jobs`、`jobs-local`、`tool-jobs`）。

### 3.12 错误语义与文案

- **错误码到文案的映射是显式的**：压缩错误码逐条映射成命令返回文本，覆盖 busy / cancelled / changed / summary / commit / persistence（`src/dsh/packages/compaction/command-compact/src/index.ts:23-56`）。
- **格式错误单独成类**：`SessionFormatError` 与 `SessionFormatUnsupportedMigrationError`（`packages/session/session-format/src/error.ts:1-9`）。
- **未找到全局统一错误码表**：除上述局部映射外，后端没有单一 `error-code → 用户文案` 的集中定义（调研结论，非判断）。

## 4. 能力对位表

> "我们的现状"列必须能落到本仓库文件；判级：**缺**（无对应实现）/ **弱**（有但覆盖面或语义不足）/ **异**（机制不同、行为大体等价）/ **等价**。

| 能力项 | dsh 做法 | 我们的现状 | 差距分级 | 建议 |
| --- | --- | --- | --- | --- |
| 会话事件持久化 | 追加写事件日志，`{type,seq,time,data}`，格式链 v0→v4 迁移，投影与查询分离 | `src-tauri/src/rollout/mod.rs` 逐行 flush 的 JSONL，坏行可见；`events.rs` 定义事件；`event_journal.rs` 提供分页/游标 | 异 | 不照搬格式链；保留"追加写 + 坏行可见 + 可回放"三条语义（已具备） |
| 会话导出 | ZIP，支持 `includeDescendants`，另有 Markdown | `src-tauri/src/session_export/mod.rs` 导出 ZIP（会话 JSONL）；界面另有"复制会话为 Markdown" | 弱 | 改造后采纳：ZIP 内加清单与子会话选项（子代理已有父子边，具备条件） |
| 模型流式拼装 | 累加器压缩 delta，可逆展开，时间戳与索引保留 | `src-tauri/src/model_client/mod.rs`、`provider.rs` 解析流式增量；`src-tauri/src/capability.rs` 记录模型能力（声明 + 实测探针，与流式无关） | 异 | 不采纳其数据结构；确认工具参数增量拼装与 usage 统计的等价性即可 |
| 模型请求重试 | 可重试码白名单 + 指数退避 + 抖动 + 上限 + `Retry-After`，重试写会话 | **未检索到模型侧重试**：`src-tauri/src` 内仅 `mcp_client/runtime.rs:1827` 有重连退避 | 缺 | **优先照搬行为**（见 §5） |
| 工具注册与可见性 | 注册中心校验 schema，allow/deny + scope chain + shadow | `src-tauri/src/tool_router/spec.rs`、`mod.rs`：ToolSpec + 风险分级 + 执行器 | 异 | 不采纳其 scope chain；保留我们的风险分级 |
| 审批 | 请求带 `{agent, toolName, callId, reason, signal}`，策略 `ask`/`never` | `src-tauri/src/permission/mod.rs`、`policy.rs`：集中裁决、`GateOutcome` 区分拒绝类型、审批不可绕过 | 等价 | 不采纳其两档策略；我们的档位更细 |
| 权限预设 | 预设同时绑定 sandbox + approval，默认 `workspace-write/ask` | `src-tauri/src/config.rs` 的 sandbox/approval 分段；界面"权限"只读展示 | 弱 | 改造后采纳"档位绑定"的表达，做成可选预设；不动执行前生效点 |
| 沙箱 | 三档策略 + Windows 写受限令牌 + DACL 交集，fail-closed | `src-tauri/src/sandbox/mod.rs`：AppContainer + 受限令牌 + JobObject，三档，fail-closed；`acl.rs`、`windows_impl.rs` | 异 | 不采纳其实现；机制不同但行为目标一致 |
| MCP 接入 | stdio/streamable-http、serverName 命名空间、manager 的 restart/probe/watch、资源三件套 | `src-tauri/src/mcp_client/`：runtime、registry、trust、fingerprint、module_tree、manifest、session、rpc、guard、output | 弱 | 逐项核对 probe/watch 与资源面；命名冲突与信任指纹我们已有 |
| 网页搜索 | provider 注册制，搜索前判认证 | `src-tauri/src/tools/web/search.rs`：`SearchProvider` trait + 内置 Bing（免 key），失败区分无结果/解析失败/被拦截；`fetch.rs` 抓取 | 异 | 不采纳其 provider 生态；我们的失败三分法更严格，保留 |
| 提示词组装 | section/context 顺序集中配置，assemble 瀑布 | `src-tauri/src/agent_loop/prompt.rs`：每实例只组装一次、`PROMPT_VERSION` + 全文指纹进日志、易变量走尾部消息 | 等价 | 不采纳其扩展点；内容锁已覆盖 |
| 压缩 | `pressure`/`context-overflow` 触发，idle 手动压缩，`start/summary/end/prune` 事件，记录替换与丢弃范围 | `src-tauri/src/compaction/mod.rs`：预算判定 + 摘要、区间替换、保留 30%；`session_compact.rs` 接线 | 弱 | 改造后采纳 `prune` 事件可见性与结果范围记录；区间语义保留 |
| 取消与中断 | AbortSignal 只读，分发前取消有专用错误码，取消不污染持久化 | `src-tauri/src/app_state.rs` 的 `begin_task`/`cancel_active`；`agent_loop/mod.rs` 的取消检查与 `ToolCallResult::Cancelled` | 等价 | 对齐错误码命名即可 |
| 子代理 | 工具立即返回 activation，完成通知回传，子代理独立 durability | `src-tauri/src/subagent_registry/mod.rs`：状态机、子令牌级联、并发上限、`wait_agent` 收口 | 弱 | **改造后采纳**"立即返回 + 通知"（见 §5） |
| 后台任务 | `jobs` + `jobs-local` + `tool-jobs` | `src-tauri/src/jobs.rs`、`tools/job_tools.rs` | 异 | 不采纳其实现；核对生命周期语义 |
| 错误语义与文案 | 局部显式映射（压缩/格式），无全局错误码表 | `src-tauri/src/model_client/error.rs`：错误大类 + 人话映射 + 原始信息保留（Auth/Quota/RateLimited/Timeout/Network/Server/EndpointUnsupported/BadRequest） | 弱 | 照搬"稳定错误码 + 文案集中"；**先核界面是否已消费**（0.2.4 真机仍只显示「处理失败」，见 `docs/10` §9 第 7 条） |

## 5. 先行建议（明显优于我们，值得先做）

1. **模型请求重试（`llm-retry` 的行为面）**：我们目前没有模型侧重试，只有 MCP 重连退避。限流、超时、传输断流、空响应这四类失败会让用户直接看到失败回合；按 dsh 的白名单 + 指数退避 + 抖动 + 上限 + `Retry-After` 落地，并把重试写成会话事件，收益最大且不依赖任何 Node 生态。
2. **子代理"立即返回 activation + 完成通知"**：我们现在靠 `wait_agent` 同步收口，长任务会占住父回合。改为激活即返回、完成后以通知追加到父会话，可与现有父子边和事件协议复用。
3. **会话导出补清单与子会话**：ZIP 里加条目清单与 `includeDescendants`，排障时不用让用户另外说明"导的是哪一层"。优先级低于前两项。

## 6. 维护规则

- 表中每行"我们的现状"必须给文件路径；改动实现后同步刷新本行，不另开散落记录。
- 新增能力域时先补 §3 做法与文件，再补 §4 一行；没有 dsh 对应物的，写"无参照"并说明自研理由。
- 判级变化（尤其"缺 → 弱 / 弱 → 等价"）在 `docs/10` 变更记录留一行。
