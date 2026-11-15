# SHANIU 参考差距清单（2026-11-15）

> 只读分析产物。本轮未修改 `src/`、`src-tauri/`、`evals/`、`docs/smoke/`，未提交、未推送。
> 唯一的产品文档改动是按用户 2026-11-15 的追加指令，在 `docs/09-上游对齐账本.md` §0 增加「基线抬升规则」，未动基线数值、未重生成账本。

## 0. 基线与方法

### 0.1 本轮对照的上游检出

| 上游 | 检出 HEAD | 与远端 | 抓取方式 |
| --- | --- | --- | --- |
| codex | `b016cd18` | `HEAD..@{u}` = 0（即远端 tip） | 本机 `D:\AI\codex` |
| dsh（deepseek-harness） | `39163dd3` | `HEAD..@{u}` = 0 | 本机 `D:\AI\deepseek-harness` |
| ZCode | `fec97590` | `HEAD..@{u}` = 0 | 本机 `D:\AI\ZCode` |

**不抬基线。** `docs/09` 的冻结基线仍是 dsh `fcc0ad4`；本清单只记录「按最新检出核对后看到的差距」，每条都标出它出现在哪个上游版本/commit。

### 0.2 判定口径

- 只比机制与协议，不比 UI 视觉；版本号、注释、命名差异一律不列。
- 每条差距四栏：**上游做法（含代码）** / **SHANIU 现状（含代码）** / **用户可见后果** / **我们具体怎么跟**。
- 「怎么跟」优先在我们自己的适配层做等价实现，不扩大对 vendored 上游代码的改动；只有确实构成语义冲突才归「不跟」并写明冲突点。
- 「是否需要跟最新版本」：上游在这条所在文件的最新提交之后是否又改过。三个仓库本地检出都等于远端 tip，所以除特别注明外均为 **否（截至 2026-11-15 无更新的本地可见提交）**；判不了就写「未判定」，不猜。

### 0.3 三档排序总览

| 档 | 条目 |
| --- | --- |
| 必须改 | D1 请求体缺协议字段、D2 reasoning 不回放、D3 压缩预算不扣输出预留、D7 工作区内受保护路径可写、D10 崩溃后孤立工具调用被剔除 |
| 值得改 | D4 错误分类粒度、D5 重试缺 always/按 provider 覆盖、D6 压缩前无确定性裁剪、D8 审批模式未进提示词、D9 拒绝原因不入 tool 输出、D11 turn/step 口径、D12 TurnEnd 缺 interrupted |
| 不跟 | D13 远端压缩 v2、D14 非 function 工具项回放、D15 ZCode 声明式 option map |

---

## 1. 模型接入层

### D1（必须改）Responses 请求体缺 `store` / `include` / `tool_choice` / `parallel_tool_calls` / `prompt_cache_key`

**上游做法（codex）**：`codex-api/src/common.rs:325-354` 把 Responses 请求体定义成显式结构，协议字段全部落成结构体字段（摘录，省略 `skip_serializing_if` 与尾部 `access_programs` / `request_country`）：

```rust
pub struct ResponsesApiRequest {
    pub model: String,
    pub stream: bool,
    pub service_tier: Option<String>,
    pub input: Vec<ResponseItem>,
    pub tools: Option<ResponsesApiTools>,
    pub tool_choice: String,
    pub parallel_tool_calls: bool,
    pub reasoning: Option<Reasoning>,
    pub store: bool,
    pub stream_options: Option<StreamOptions>,
    pub include: Vec<String>,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_options: Option<PromptCacheOptions>,
    pub text: Option<TextControls>,
    pub client_metadata: Option<HashMap<String, String>>,
}
```

**SHANIU 现状**：`src-tauri/src/model_client/mod.rs:220-244` 只写 `model/instructions/input/stream/tools`，外加可选 `reasoning.effort`：

```rust
let mut body = serde_json::json!({
    "model": provider.model,
    "instructions": instructions,
    "input": responses_input(history),
    "stream": true,
    "tools": ...,
});
```

**用户可见后果**：
- 不显式 `store:false`，是否落库取决于网关默认值——对隐私敏感用户是未知项；
- 没有 `prompt_cache_key`，多轮长会话拿不到前缀缓存，同样内容重复计费、首字延迟更高；
- 不声明 `parallel_tool_calls`，并行行为由端点默认决定，工具批次不可预测；
- 不声明 `include`，为 D2 的回放能力设了天花板。

**我们具体怎么跟**：照搬字段集合，在我们自己的 `build_responses_request` 里补齐，按 provider 能力开关（不认的端点不塞）。落点 `model_client/mod.rs:220-244` + `ProviderConfig` 增加三个可选字段。代价：小（约半天，含 fixture）。
**上游版本**：codex `b016cd18`（`codex-api/src/common.rs` 最近提交 `65a5410f`，2026-11-14，与本条无关）。**是否需追最新**：否。

---

### D2（必须改）reasoning item 不回放，Responses 的推理状态每轮清零

**上游做法（codex）**：`protocol/src/models.rs:1179-1192` 的 `ResponseItem::Reasoning` 是完整可回放项，保留 `summary`、`content` 与 `encrypted_content`；请求里固定声明 `include: ["reasoning.encrypted_content"]`（`core/src/client.rs:1284`），历史里原样保存、下一轮作为 input 项回放。非 OpenAI 端点会剥其它 item 的加密内容，但 `Reasoning` 明确在豁免名单里（`client.rs:1257-1274` 的 `matches!` 分支）。

```rust
// models.rs:1179-1192（摘录）
Reasoning {
    id: Option<ResponseItemId>,
    summary: Vec<ReasoningItemReasoningSummary>,
    content: Option<Vec<ReasoningItemContent>>,
    encrypted_content: Option<String>,
},
```

**SHANIU 现状**：`model_client/mod.rs:100-101` 的历史项只有 `FunctionCall` / `FunctionCallOutput`，没有 reasoning 项；`mod.rs:263-266` 明确写着：

```rust
/// `reasoning` 在这里**不发**：Responses 回放推理要独立的 `reasoning` item（需
/// `encrypted_content`），我们没有加密推理可回放；把明文 `reasoning` 当消息字段塞进去
/// 反而是陌生键。ChatCompletions 侧仍按 SH-083 回传。
```

**用户可见后果**：Responses 侧多轮任务每轮都从零推理，模型会重复分析、重复读同一批文件；部分严格网关对「缺少上一轮 reasoning item」的续写直接 400（尤其带工具调用的多轮）。这也是我们目前 Responses 臂在长任务上比 chat 臂更容易失忆的直接原因。

**我们具体怎么跟**：等价实现，不引入上游代码。`HistoryItem` 增加 `Reasoning { summary, content, encrypted_content }`；从 SSE 的 `response.output_item.done` 捕获并写入 rollout；请求时按 provider 设置决定是否保留 `encrypted_content`；压缩时把 reasoning 项当普通可压内容。落点 `model_client/mod.rs`（解析 + 请求）、`rollout/mod.rs`（新 item 类型 + 兼容旧档）、`agent_loop`（收尾落盘）。代价：中（约 1.5–2 天，含旧 rollout 向后兼容与回放 fixture）。
**上游版本**：codex `b016cd18`。`include: reasoning.encrypted_content` 可追到 `591cb6149a`（2025-07-23，#1641「Always send entire request context」）；`ResponseItem::Reasoning` 的完整回放形状未能定位到单一提交，按检出 HEAD 记。**是否需追最新**：否。

---

### D3（必须改）压缩预算不扣「输出预留 + headroom」，保留比例是上游近两倍

**上游做法（dsh）**：`packages/compaction/compaction-basic/src/config.ts:181-197` 的阈值与保留量都把「预留输出 `O`」和「headroom `B`」算进去：

```ts
const pressureBudgetTokens = messageBudgetTokens - policy.headroomTokens
const thresholdTokens = Math.floor(Math.min(
  contextWindow * policy.thresholdRatio,
  pressureBudgetTokens,
))
const retainTokens = policy.retainTokens === undefined
  ? Math.floor(messageBudgetTokens * policy.retainRatio)
  : policy.retainTokens
```

默认值在 `config.ts:20`、`:23`、`:75`：`thresholdRatio = 0.8`、`retainRatio = 0.16`、`headroomTokens = 65536`；`W` 取模型声明的上下文窗口，`O` 取该模型的 max output（`messageBudgetTokens = W - O`）。

**SHANIU 现状**：`src-tauri/src/compaction/mod.rs:36-39` 只取窗口 80%，`mod.rs:49-54` 直接 `min(configured, window*80%)`；保留量 `mod.rs:273` 是 `budget * 30%`：

```rust
const KEEP_RATIO_PERCENT: usize = 30;
fn window_allowance(window: usize) -> usize { window.saturating_mul(4) / 5 }
```

`mod.rs:253-257` 的注释已经承认「低估的代价是撞上上下文溢出」，但公式里没有任何输出预留。

**用户可见后果**：以 200k 窗口、16k max output、默认 headroom 64k 为例——SHANIU 阈值 160k、保留 48k；dsh 阈值 120k（`min(160k, 184k-64k)`）、保留约 29k（`184k×16%`）。SHANIU 压缩更晚，一次大工具输出就可能 400；压完保留更多，别人压 1 次我们要压 2–3 次，摘要开销与失忆风险都翻倍。模型声明了 `max_tokens` 却完全没参与预算，等于白存。

**我们具体怎么跟**：照搬公式（纯算术，不依赖上游运行时）。`window_allowance` 改为接收 `max_tokens`，按 `W - O - headroom` 再与 `W*0.8` 取小；保留比例从 30% 调到 16%（或做成配置项，默认 16%）。落点 `compaction/mod.rs:36-66`、`agent_loop/mod.rs:839-845` 传参、`docs/04` 预算口径同步。代价：小（约半天 + 既有断言更新）。
**上游版本**：dsh `39163dd3`；预算公式在 `config.ts:181-197`，headroom 相关最近提交 `ec7030af`（2026-09-19，「default summary output cap to headroom」），`0.8/0.16` 两个默认比例可追溯到更早版本，未逐行定版。**是否需追最新**：否。

---

### D4（值得改）错误分类：结构化 code 优先、状态码门控，我们仍是关键词并集

**上游做法（codex）**：`codex-api/src/error.rs:96-112` 先取错误体里的 `code`，再与状态码组合精确匹配，命中才给专用分类：

```rust
// codex-api/src/error.rs:96-112（摘录）
let code = error.get("code")?.as_str()?;
match (code, status) {
    ("inference_hook_denied", None | Some(StatusCode::FORBIDDEN)) => { /* ... */ }
    ("context_length_exceeded", None | Some(StatusCode::BAD_REQUEST)) => {
        Some(ApiError::ContextWindowExceeded)
    }
    ("invalid_access_program" | "unsupported_access_program", None) => { /* ... */ }
    ("content_policy_violation", None | Some(StatusCode::BAD_REQUEST)) => { /* ... */ }
```

**SHANIU 现状**：`model_client/error.rs:305-335` 把 `code` 和 `message` 拼成一个 haystack 做子串命中，`error.rs:102-142` 再按「额度 → 内容过滤 → 上下文 → 限流 → 鉴权 → …」兜底。上下文超长已经能识别（`error.rs:324-333`，有 `fail-context-overflow-http400` 夹具），但 `inference_hook_denied`、`invalid_access_program` 这类结构化拒绝会落到「鉴权/普通 400」：用户看到的是错误的解释，重试策略也可能误判。

**用户可见后果**：企业网关的 hook 拒绝被显示成「密钥无效/无权限」，用户去改 key 也修不好；`invalid_access_program` 被当普通 400，既不提示换模型也不提示改配置。

**我们具体怎么跟**：等价实现。`body_kind` 改成「结构化 code 精确匹配优先（带状态码门控）→ 关键词兜底」，补三个专用 kind 与文案。落点 `error.rs:305-335` + `error.rs:102-142` + 各自 fixture。代价：小（约半天）。
**上游版本**：codex `b016cd18`。**是否需追最新**：否。

---

### D5（值得改）重试只有「有限次数」，缺 always 模式与按 provider 覆盖

**上游做法（dsh）**：`packages/llm/llm/src/retry-policy.ts:49-52` 的 `mode` 允许 `'always'`——同一个 provider 路由上无限重试，由外层取消/超时收口；重试码表也可按 provider 覆盖。

**SHANIU 现状**：`model_client/retry.rs:14-24` 只有 `max_retries: u32`；`retry.rs:26-36` 默认 5 次 / 500ms / 10s 封顶 / 0.1 抖动（与 dsh 默认值一致），`retry.rs:50-59` 白名单为 `RateLimited | Timeout | Network | Server | EmptyResponse`。没有 always 模式，也没有 provider 级覆盖。

**用户可见后果**：长任务遇到持续 429（共享配额时段）时，5 次退避后整轮直接失败，用户只能手动重发；而 dsh 的 always 模式会在可取消的前提下一直等，用户看到的是「重试中」而不是「失败」。

**我们具体怎么跟**：等价实现。`RetryPolicy` 增加 `mode: Bounded(u32) | Always`，`Always` 必须同时持有 `CancellationToken` 且每轮发 `model_retry` 事件（不能让用户以为卡死）；provider 配置允许覆盖重试码表。落点 `retry.rs` + `config.rs` 模型段。代价：小-中（约 1 天，含取消语义测试）。
**上游版本**：dsh `39163dd3`。`always` 模式引入于 `b58e3326`（2026-07-25，per-provider retry policies）；重试码表 schema 在 `035ef9b37d`（2026-11-03）改为发布内置码表，`always` 模式本身未变。**是否需追最新**：`always` 部分否；码表 schema 部分以 2026-11-03 为准（跟随时按最新 schema 抄）。

---

## 2. 压缩与上下文管理

### D6（值得改）压缩前没有确定性裁剪，长工具输出直接进摘要请求

**上游做法（dsh）**：`compaction-basic/src/index.ts:321` 在上下文超长时先做 model-free prune，再测量、再挑区间；常规路径在 `:362-372` 也是「prune → 重新测量（`:366`）→ 阈值判定（`:368`）→ selectCompactableRange（`:372`）」：

```ts
// 摘录：两处拼接（:321 的 context-overflow 分支与 :362-372 的常规路径）
this.ctx.get('toolResultPruner')?.pruneSession(agent.session)
// …重新测量…
const range = selectCompactableRange(agent.session, measurement, spec.retainTokens)
```

**SHANIU 现状**：`compaction/mod.rs:308-316` 的 `build_request` 把被压区间「原样 + 总结指令」整段发给模型，没有先截断工具输出；`plan_if_needed`（`mod.rs:258-280`）只做区间选择。

**用户可见后果**：一次 400KB 的工具输出会让总结请求本身接近或超过窗口，压缩失败；即使成功，摘要请求的输入 token 也是白白多付的。用户表现为「压缩失败/压缩很慢」。

**我们具体怎么跟**：等价实现。新增纯函数式裁剪（保留头尾各 N 行 + 中间省略标记 + 原始字节数），在 `plan_if_needed` / `build_request` 之前对工具输出做一次替换；不进 rollout，只影响本次摘要请求。落点 `compaction/mod.rs`，与 dsh 的 `toolResultPruner` 行为对齐但不引入其包。代价：中（约 1 天，含「省略后仍能理解」的用例）。
**上游版本**：dsh `39163dd3`。**是否需追最新**：否。

---

### 2.1 核对为「已等价」的两项（不列为差距）

- **摘要写回形态与不可继承性**：codex 用固定前缀标识摘要（`prompts/templates/compact/summary_prefix.md:1`，`compact.rs:408` 拼接，`compact.rs:656-658` 用 `starts_with` 判摘要），再把最近的真实用户消息以独立 item 保留（`compact.rs:69` 的 `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000`）。SHANIU `compaction/mod.rs:376-422` 的 `compose_summary` 同样「固定头 + 逐字保留首条用户消息 + 预算内保留最近用户原文 + 分隔符 + 模型摘要」，并用 `mod.rs:218-221` 的 `is_generated_summary` 在二次压缩时把摘要排除在「用户原文」之外。**机制等价，无差距。**
- **压缩后不追溯改写旧档案**：codex 的 rollout 是追加事件（`rollout/src/policy.rs:42-46` 把 `Compacted` / `RetainedContext` / `WorldState` 都当可持久化条目），会话内替换历史但不回写旧行；SHANIU `rollout/mod.rs:74-91` 同样是追加一条 `RolloutItem::Compacted { range_start, range_end, summary, ... }`。**方向一致，维持。**

---

## 3. 审批与沙箱

### D7（必须改）工作区内受保护路径可写：`.git/hooks`、`.git/config`、`.codex`

**上游做法（codex）**：可写根不是一个裸目录，而是 `WritableRoot { root, read_only_subpaths, protected_metadata_names }`（`protocol/src/protocol.rs:1338-1343`），写判定先查子路径（`protocol.rs:1347-1372`）：

```rust
if path.starts_with(subpath) {
    return false;
}
!self.path_contains_protected_metadata_name(path)
```

默认子路径由 `permissions.rs:2382-2414` 生成：顶层 `.git`（目录或 worktree 的 gitdir 指针文件都会被解析保护）、`.agents`、`.codex`。

**SHANIU 现状**：`src-tauri/src/sandbox/acl.rs:115-123` 的可写范围只有「工作区根 + TEMP/TMP + 工具链目录」，工作区内没有任何只读子路径：

```rust
// acl.rs:115-123（摘录）
pub fn write_scope(workspace_root: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    paths.push(workspace_root.to_path_buf());
    for key in ["TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            paths.push(PathBuf::from(value));
        }
    }
    paths.push(std::env::temp_dir());
    // 之后是工具链目录（CARGO_HOME / RUSTUP_HOME / PNPM_HOME / NPM_CONFIG_PREFIX）
```

**用户可见后果**：workspace-write 档下，agent（或被注入的网页/文档内容）可以写 `<ws>/.git/hooks/pre-commit` 或改 `.git/config` 的 `core.hooksPath`，下一次用户自己 commit 时在沙箱外执行任意代码；`.codex` 之类的 agent 配置也在可写范围。这是从「只能写工作区」升级为「能持久化代码执行」的实质越权。

**我们具体怎么跟**：照搬机制，落到我们自己的 ACL 层。`write_scope` 之外新增 `protected_subpaths(workspace_root)`（`.git`、`.git` 指针解析出的 gitdir、`.agents`、`.codex`），写判定/授权时先拒绝；Windows 上 ACL 是「允许 ACE + 祖先继承」，需要显式 deny ACE 或授权前路径判定，二者都要真机验证（要求：区外写被拒、`.git/hooks` 写被拒、正常工作区构建跑通）。落点 `sandbox/acl.rs:115-123` + 判定前置 + `sandbox/mod.rs` 的模式文案。代价：中（约 1–2 天，含真机验收，不改上游 vendored 代码）。
**上游版本**：codex `b016cd18`；受保护元数据路径引入提交 `0156b1e61f`（2026-04-28，#19846）。**是否需追最新**：否。

---

### D8（值得改）审批模式没有进入系统提示词，模型不知道「现在到底会不会弹审批」

**上游做法（dsh）**：`packages/interaction/user-approval/src/index.ts` 把审批策略作为独立系统提示段注入（`:195-200` 按当前策略返回不同语句）；`never` 模式下有固定的 `NEVER_SENTENCE`（`:75`），明确告诉模型「审批已关闭，需要审批的动作会被自动拒绝，不要再请求升级」。

**SHANIU 现状**：`agent_loop/prompt.rs:582-587` 的「审批与安全」段是写死的：

```rust
"## 审批与安全\n\
- 写入、命令执行都可能弹审批；被拒绝时说明原因并征求指示，不要绕过或换个说法重试。\n\
- 破坏性操作（递归删除、强制推送、工作区外写入、上传）逐次询问；..."
```

它只反映沙箱模式，不读 `ApprovalMode`（`src-tauri/src/config.rs:1103-1113` 的 Never / OnFailure / OnRequest / FullAccess）。

**用户可见后果**：在 Never/FullAccess 下模型仍说「我需要你批准才能继续」，但界面不会弹审批，用户以为卡住；反过来在 OnFailure 下模型可能过度保守，不敢先跑命令看失败原因。

**我们具体怎么跟**：等价实现。`PromptContext` 增加 `approval_mode`，安全段按模式改写（never → 「审批已关闭，直接执行，不要再要求用户批准」；on-failure → 「先执行，失败后再申请升级」）。落点 `agent_loop/prompt.rs:582-587` + 调用方。代价：小（约半天，含快照测试）。
**上游版本**：dsh `39163dd3`。**是否需追最新**：否。

---

### D9（值得改）拒绝后的 tool 输出不带「为什么被拒」

**上游做法（codex）**：沙箱/审批拒绝会带上具体规则来源，模型拿到的是可行动的原因（`core/src/safety.rs:14-16`）：

```rust
"writing outside of the project; rejected by user approval settings";
"writing is blocked by read-only sandbox; rejected by user approval settings";
```

**SHANIU 现状**：`agent_loop/mod.rs:1955-1975`、`:2171-2180` 的拒绝分支只回一句通用文本（「用户拒绝了该工具调用」「策略拒绝：当前代理无权使用该工具」），不带触发规则；`ToolCallResult` 只有 `Denied`（`events.rs:154-163`），两种拒绝共用同一结果类型。

**用户可见后果**：模型只能反复试探同一类操作，或在「用户拒绝」与「工作区外写」之间猜；用户看到的是模型来回来去问，而不是一次说明白。

**我们具体怎么跟**：等价实现。拒绝结果里带 `reason_code`（user_denied / policy_forbidden / sandbox_denied / outside_workspace）与一句人话，落 tool 输出 + 事件。落点 `agent_loop/mod.rs` 拒绝分支、`permission`、`events.rs`。代价：小（约半天）。
**上游版本**：codex `b016cd18`。**是否需追最新**：否。

---

### 3.1 核对为「已对齐」的两项

- **审批默认档**：codex 默认 `OnRequest`；SHANIU `ApprovalMode::default()` 也是 `OnRequest`，且 on-failure 的「失败后升级重试」已在 `agent_loop/mod.rs` 落地。无差距。
- **沙箱写入边界表达**：dsh 的 workspace-write 也是「工作区 + 额外根 + `/tmp` + `os.tmpdir()`」（`packages/sandbox/sandbox/src/roots.ts:96-105`），与 SHANIU 的 `write_scope` 同构；差异只在 D7 的受保护子路径。

---

## 4. 回合收口与恢复

### D10（必须改）崩溃/强杀后，未回结果的工具调用被「剔除」，上游是「补一条结果未知」

**上游做法（codex，2026-11-10 新增）**：`core/src/context_manager/normalize.rs:21-140` 的 `ensure_call_outputs_present` 对没有回包的调用补一条 `function_call_output`，文本为 `"aborted"`，保证历史自洽：

```rust
// normalize.rs:21 ensure_call_outputs_present(...)
// :76 / :113 给每个未回包的调用补一条 output
output: FunctionCallOutputPayload::from_text("aborted".to_string()),
```

上游自测描述即「Handle unfinished tool calls in restored history (#60519)」。

**上游做法（dsh）**：`packages/core/session/src/repair.ts` 的 `openTurnClosers`（`:53`）在恢复会话时补齐三层收口，`interruptedTurnClosers`（`:209`）是它的重开入口：未启动的工具补 `ToolNotStartedError`，启动了但结果未知的补 `ToolOutcomeUnknownError`，最后补 `step/end` 与 `turn/end`。`repair.ts` 的 `CLOSER_TEXT` 明确告诉模型「上一条命令可能已经产生副作用，先核验再继续，不要盲目重跑」。

**SHANIU 现状**：`src-tauri/src/rollout/resume.rs:187-196` 从前到后重放，遇到未回包的调用「原位剔除」：

```rust
// resume.rs:187-196（摘要）
// 历史里只有 function_call、没有对应 output 的项，重放时按位置删除，
// 不把孤立的 call 送给模型/压缩。
```

`events.rs:154-163` 的 `ToolCallResult` 也只有 `Ok/Error/Cancelled/Denied`，没有「结果未知」。

**用户可见后果**：强杀/崩溃后重开，模型**根本不知道**刚才那条 `rm`/`git push`/构建执行过没有——它既不会去核验，也不会告知用户；最坏情况是重复执行有副作用的命令。上游是「显式告诉你结果未知，先查再动」。这是当前恢复链上最实质的一条差距。

**我们具体怎么跟**：照搬机制。`resume.rs` 把剔除改为补 `HistoryItem::FunctionCallOutput { output: "…结果未知（进程在工具执行期间退出）…" }`；同时 `ToolCallResult` 增加 `Unknown`，UI 侧显示「结果未知」而不是「失败」。落点 `rollout/resume.rs:187-196`、`events.rs:154-163`、`src/transport/adapter.ts`（新增 outcome 映射）、`docs/04` 恢复章节。代价：中（约 1–1.5 天，含旧档回放测试）。
**上游版本**：codex `39b42aaa`（2026-11-10，#60519）；dsh `8de4e518`（2026-09-26，recovery 容忍 pending-free tool/result）。**是否需追最新**：否。

---

### D11（值得改）turn 的计数口径：我们按「采样步」，上游按「用户回合」

**上游做法（dsh）**：`packages/core/agent-loop/src/agent.ts:340-346` 里 `phase.turn` 只在用户提交新请求时递增；一次请求内的每次模型调用是 `step`，有独立的 `step/start`、`step/end`（`agent.ts:367`、`:394`）。**上游（codex）**：`TurnStarted` / `TurnComplete` 同样以用户请求为界（`session/mod.rs:2716-2725` 发 `TurnStarted`），一次请求内不设模型调用次数上限（靠取消与 token 预算收口），仓库内搜不到 `max_turns`。

**SHANIU 现状**：`agent_loop/mod.rs:1070-1100` 的主循环是 `for turn in 1..=max_turns`，每轮 = 一次模型采样 + 一批工具，`TurnStarted/TurnEnded` 也按这个粒度发；`agent_loop/budget.rs:15-27` 的 `max_turns` 就是这个循环的上界（`0` 表示无限）。

**用户可见后果**：同一个「用户问一句、模型调 6 次工具」的任务，上游 UI 显示 1 个回合 / 6 步，SHANIU 显示 6 个回合；配置 `max_turns` 时用户以为是「问几句」，实际是「模型最多采样几次」，长工具链会在用户毫无预期时被截断（并报 `budget_exhausted`）。崩溃恢复的收口粒度也随之不同。

**我们具体怎么跟**：等价实现，分两步。第一步只在事件层加 `run_start/run_end`（用户回合），`turn` 语义保持不变，UI 同时展示「回合/步」——不动内部预算；第二步（可选）把 `max_turns` 改名 `max_steps` 并新增 `max_runs`，配置迁移时保留旧键别名。落点 `events.rs`、`agent_loop/budget.rs`、`agent_loop/mod.rs`、`src/transport/adapter.ts`。代价：中-大（约 2–3 天，事件契约 + 前端 + 文档 + 回放）。
**上游版本**：dsh `39163dd3`；codex `b016cd18`。**是否需追最新**：否。

---

### D12（值得改）TurnEnd 缺 `interrupted`，输出截断被归成失败

**上游做法（dsh）**：`packages/core/session/src/types.ts:211-227` 的 turn 结束原因是枚举，含 `blocked`、`max-tokens`、`interrupted` 等；恢复补出的 `turn/end` 用 `interrupted`（`repair.ts:210`），`max-tokens` 是「部分完成」而不是错误。

**SHANIU 现状**：`events.rs:23-28` 只有 `Completed / Cancelled / Failed / BudgetExhausted`；`max_tokens` 截断走 `ModelErrorKind::Incomplete` → `TurnEndReason::Failed`。崩溃重开后宿主不补 `turn/end`，只有前端 adapter 兜底成 `indeterminate`（`src/transport/adapter.ts:2143`）。

**用户可见后果**：被上限截断的一次长回答在 UI 上是「失败」，用户会以为全丢了；崩溃中断在界面是「未知」，但档案里没有对应的 `turn/end`，导出、CLI、evals 等下游消费者看到的仍是「没有结论」，与真正的错误无法区分。

**我们具体怎么跟**：照搬变体。`TurnEndReason` 加 `Interrupted`（恢复时由宿主补发）与 `MaxTokens`（从 `Incomplete` 分流），同步 `all_variants`、TS 镜像、`tests/event_schema.rs` 与文案。落点 `events.rs:23-39`、`src/transport/adapter.ts`。代价：小-中（约 1 天）。
**上游版本**：dsh `39163dd3`。**是否需追最新**：否。

---

## 5. 「不跟」及理由

| 条目 | 上游机制 | 冲突点 / 不跟理由 |
| --- | --- | --- |
| D13 远端压缩 v2 | codex `compact.rs:459` 调 `replace_compacted_history`（定义在 `session/mod.rs:4853`）+ `compact_remote_v2`，以及 `ResponseItem::Compaction` / `ContextCompaction`（`models.rs:1390`、`1406`） | 这套依赖 OpenAI 托管压缩端点与 `compaction_response_id`。我们是多 provider 适配层，远端压缩在多数端点上不存在；本地压缩 + D3/D6 已覆盖同等目标。**语义冲突：依赖上游服务端能力。** 若将来正式接入该端点，再单独立项。 |
| D14 非 function 工具项回放 | codex `ResponseItem::LocalShellCall`（`models.rs:1193`）、`CustomToolCall`（`1282`）、`WebSearchCall`、`ToolSearchCall` | SHANIU 的工具面只有 function tools，没有本地 shell 专用调用、自定义工具或托管搜索。解析层仍保持「未知 item 忽略且不报错」（`model_client/mod.rs:194-213`），这是有意的安全降级。**语义冲突：工具集不同。** |
| D15 ZCode 声明式 option map | `packages/model-option-map/src/option-maps.ts` 按模型把 reasoningLevel 等选项编译成 body patch | SHANIU 已有 per-model 的 `reasoning_effort`、`context_window`、`max_tokens` 字段与显式请求构造（`model_client/mod.rs:220-244`），机制等价且更少抽象层。**语义冲突：无，但收益为零。** |

---

## 6. 此前「刻意不跟」的判断复核

### 6.1 「shell 写盘不计文件写」——维持

- codex 的 `core/src/turn_diff_tracker.rs:1-20` 只跟踪 `apply_patch` 的 committed mutation，shell 写盘同样不计入 turn diff；dsh 没有回合末 `git status` 校验。
- SHANIU 的写后验证闸门本身会跑 `git status`/测试（`align/permission-presets.md:330-370`），shell 写盘由闸门兜底，语义上仍能被发现。
- **结论**：判断成立，继续不跟。但建议在使用指南里把「shell 写盘不在文件写计数内，但会被回合末 git 校验捕捉」写成一句话，避免被读成「shell 写盘不算改动」。

### 6.2 「否决额度只给一次」——维持，且上游更严格

- codex 的审批拒绝没有「N 次额度」概念；dsh 的委派子代理会被 pin 在固定审批档（`permission-presets/src/index.ts:79-86` 的 `AUTO_PRESET_SPEC` 与 `:232-250` 的 `pinInitialPermission`），评审拒绝即终局，不会再向人二次请示。
- SHANIU 的 `VetoLedger`（`agent_loop/mod.rs:171-182`）是「计划 1 次 + 验证 1 次，硬顶 2」，与 `align/permission-presets.md:361` 的「计划那一票只给一次」一致；`docs/08:100` 写的「否决硬顶 2」也是这套口径，不是笔误。
- **结论**：维持，且上游方向同样是一次性收口。无需改代码，也无需改文案。

### 6.3 「压缩后不追溯改写旧档案」——维持，且这是上游同款

- codex 把压缩作为事件追加（`rollout/src/policy.rs:42-46`），不回写旧行；dsh 是事件源，压缩追加 summary 节点并记录 shadowed 区间。
- SHANIU 的 `RolloutItem::Compacted`（`rollout/mod.rs:74-91`）同为追加。
- **结论**：不是「落后」，而是与上游一致的设计；维持。

### 6.4 需要推翻重评的一条：Responses「没有加密推理可回放」

- 原判断写在 `model_client/mod.rs:263-266` 的注释里，前提是「我们没有加密推理」。
- 上游实际做法是：从响应里收下 `encrypted_content`，下一轮原样回放（`models.rs:1179-1192`），根本不需要我们自己加密。
- **结论**：前提不成立，归入 D2（必须改）。这是本轮复核中唯一需要推翻的旧判断。

---

## 7. 未判定项

- **上游在 2026-11-15 之后是否还会改这些点**：未判定。三个仓库本地检出都等于远端 tip（`HEAD..@{u}` = 0），看不到未来提交；本清单只对「截至该检出的代码」负责。
- **D9/D8 这类事件契约改动的实际工作量**：未判定到人日级。前端 `src/transport/adapter.ts` 与三处文档联动，需要按真实改动面复估。
