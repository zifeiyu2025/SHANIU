# D1（必须改）Responses 请求体补齐协议字段 — 规格 v1

> 来源：`docs/analysis/reference-delta-2026-11-15.md` §1 D1。
> 本轮**只出规格，不改代码**：评测线正在同一棵树上跑 Phase 2，动 `src-tauri` 会污染其运行结果。
> 范围：`src-tauri/src/model_client/{mod,provider}.rs`、`src-tauri/src/compaction/mod.rs`、`src-tauri/src/config.rs`、`src-tauri/tests/responses_request_shape.rs`。

## 1. 现状与上游对照

### 上游（codex `b016cd18`）

`codex-api/src/common.rs:325-354` 把请求体定义成显式结构，协议字段全部落成结构体字段：

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

其中 `include` 固定声明 `reasoning.encrypted_content`（`core/src/client.rs:1284`），`prompt_cache_key` 由 `PromptCacheKey` 策略按会话派生（`core/src/client.rs:308` 的 `prompt_cache_key_policy`，不是硬编码常量）。

### SHANIU 现状

`model_client/mod.rs:220-244` 只写 5 个键（`model`/`instructions`/`input`/`stream`/`tools`），外加可选 `reasoning.effort`：

```rust
let mut body = serde_json::json!({
    "model": provider.model,
    "instructions": instructions,
    "input": responses_input(history),
    "stream": true,
    "tools": ...,
});
```

同一份请求构造在压缩路径还有一份（`compaction/mod.rs:308-316` 的 `build_request`），两处必须同口径，否则摘要请求与主请求的端点行为不一致。

### 差异与后果

| 缺的键 | 用户可见后果 |
| --- | --- |
| `store` | 是否落库取决于网关默认值，隐私敏感用户无法预期 |
| `prompt_cache_key` | 多轮长会话拿不到前缀缓存，重复计费、首字更慢 |
| `parallel_tool_calls` | 并行行为由端点默认决定，工具批次不可预测 |
| `tool_choice` | 端点默认值可能不是 `auto`（部分网关默认 `none` 或必选） |
| `include` | D2 的密文回放拿不到数据（见 §5 依赖） |

## 2. 改法

### 2.1 配置面（`config.rs`）

- `ModelConfig`（每模型行）增加一个布尔项：
  ```rust
  /// 是否发送 Responses 的扩展字段（store/tool_choice/parallel_tool_calls/prompt_cache_key/include）。
  /// 默认 true；不认这些键的老网关可关掉，换回 2026-11 之前的 5 键请求体。
  #[serde(default = "default_true")]
  pub responses_extras: bool,
  ```
- `ResolvedModel` 增加 `parallel_tool_calls: bool`（默认 `true`）与 `responses_extras: bool`。
  两者都由模型行解析而来，`effective_*` 命名沿用既有 `context_window_effective` 的风格。

**接口变化**：`ModelConfig` / `ResolvedModel` 各加字段；旧 `config.toml` 因 `serde(default)` 原样可读，不需要迁移。

### 2.2 请求面（`model_client/provider.rs`）

`ProviderConfig` 增加：

```rust
pub responses_extras: bool,
pub parallel_tool_calls: bool,
```

`from_model_config` / `from_resolved` 同步赋值（`provider.rs:22-42`）。

### 2.3 请求体（`model_client/mod.rs`）

`build_responses_request` 增加一个参数 `cache_key: Option<&str>`，在 `responses_extras == true` 时补齐：

```rust
body["store"] = serde_json::json!(false);
body["tool_choice"] = serde_json::json!("auto");
body["parallel_tool_calls"] = serde_json::json!(provider.parallel_tool_calls);
body["include"] = serde_json::json!(include);            // D1 阶段为 []；D2 打开后为 ["reasoning.encrypted_content"]
if let Some(key) = cache_key { body["prompt_cache_key"] = serde_json::json!(key); }
```

- `store` 恒 `false`：本仓库的档案（rollout）是唯一事实源，不使用服务端存储。
- `cache_key` 传会话 id（`AgentLoop.session_id`），fork 后的新会话用新 id，resume 沿用原 id（缓存命中率最高）。
- `include` 在 D1 单独落地时固定空数组；只有 D2 的 `reasoning_replay` 开关打开才写入 `reasoning.encrypted_content`。这样 D1 不会替用户多付密文体积。

### 2.4 压缩路径同口径（`compaction/mod.rs`）

`compaction::build_request` 与主请求共用同一个构造函数（把 `build_responses_request` 提为唯一入口，压缩侧只传不同的 `instructions`/`history`）。**不允许**两处各写一份 `serde_json::json!`。

## 3. 会先变红的用例

文件：`src-tauri/tests/responses_request_shape.rs`（既有文件，现有用例只断言 `input` 形状）。

### 3.1 新增用例（先红）

- `responses_body_carries_protocol_fields`：
  ```rust
  assert_eq!(body["store"], false);
  assert_eq!(body["tool_choice"], "auto");
  assert_eq!(body["parallel_tool_calls"], true);
  assert_eq!(body["prompt_cache_key"], "sess-1");
  assert!(body["include"].as_array().unwrap().is_empty());
  ```
- `responses_extras_can_be_disabled`：`responses_extras=false` 时五个键**一个都不出现**（`body.get("store").is_none()` 等）。
- `compaction_request_shares_the_same_fields`：调用压缩侧入口，断言同样带 `store`/`tool_choice`/`parallel_tool_calls`（防止两处漂移）。

### 3.2 反向验证（必须红）

1. 删 `store` 一行 → 3.1 第一条红；
2. 删 `tool_choice` → 第一条红；
3. 把 `parallel_tool_calls` 写死 `true` 并忽略配置，用 `false` 档跑 → 第一条红（说明配置没接上）；
4. 压缩侧改回自己拼 `json!` → 第三条红；
5. 把 `responses_extras` 的默认值改成 `false` → 既有默认路径的用例红。

记录命令与关键输出后恢复，恢复后全量重跑绿。

## 4. 影响面

- **既有用例**：`responses_request_shape.rs` 现有断言不检查这五个键，预期不变；若某条用例对 body 做整体 `assert_eq!`，需要补键（实现时逐个确认）。
- **既有行为**：
  - 端点若严格拒绝未知键，默认开启会 400。这是**有意的行为变化**，用 `responses_extras=false` 回退；发布说明要写这一句。
  - `prompt_cache_key` 命中后计费下降、首字变快，用户可感知但无需操作。
  - `parallel_tool_calls=true` 不改变我们自己的执行顺序：工具批次的串并行由 `agent_loop` 决定，这个键只影响模型一次返回几个调用。
- **档案/导出**：无变化（请求体不进 rollout）。

## 5. 与 D2 的依赖

- D1 的 `include` 是 D2 拿到 `encrypted_content` 的前提；D2 的 `reasoning_replay` 开关依赖 D1 的 `ProviderConfig` 新字段。
- **可分别落地**：D1 单独落地时 `include=[]`，现有行为不受影响；D2 落地时把 `include` 打开即可。
- **但端到端回放用例必须两条同批**：只做 D1 时无法验证"密文进历史再回放"；只做 D2 时 `include` 缺位，用例只能靠夹具伪造密文。建议 D1 与 D2 作为同一批的两笔提交，顺序 D1 → D2。

## 6. 代价与风险

- 代价：约 0.5–1 天（配置字段 + 两处请求构造收敛 + 3 条用例 + 既有 body 断言清理）。
- 风险 1：老网关拒绝未知键。缓解：`responses_extras` 单开关回退；发布说明写明。
- 风险 2：`prompt_cache_key` 用错（例如 fork 复用旧 id）会串缓存。判据：fork 用例断言新会话的 key 不等于旧 id。
- 风险 3：压缩路径改走统一入口后，`instructions` 与工具的差异可能被顺手"统一"掉。约束：只统一字段集合，不改 `instructions` 与 `tools` 的入参。

## 7. 验证命令

`cargo test --test responses_request_shape -- --test-threads=1`；`cargo test -- --test-threads=1`；`cargo clippy --all-targets -- -D warnings`；`rustfmt` 只跑改动文件；`.\scripts\check-deps.ps1`；`.\scripts\dev\check-doc-claims.ps1`。

## 8. 提交策略

一笔 `feat(model): Responses 请求体补齐协议字段`，只 add 本规格涉及的路径；不碰 `docs/smoke/`；推前 `git fetch`，非 force。
