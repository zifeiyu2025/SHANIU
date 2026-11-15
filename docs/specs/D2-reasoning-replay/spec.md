# D2（必须改）捕获并回放 reasoning item — 规格 v1

> 来源：`docs/analysis/reference-delta-2026-11-15.md` §1 D2。
> 本轮**只出规格，不改代码**（理由同 D1）。
> 范围：`src-tauri/src/model_client/mod.rs`、`src-tauri/src/agent_loop/mod.rs`、`src-tauri/src/compaction/mod.rs`、`src-tauri/src/rollout/*`、`src-tauri/tests/*`。
> 依赖：**D1 必须先落地**（`include` 与 `ProviderConfig` 开关在 D1 里）。

## 1. 现状与上游对照

### 上游（codex `b016cd18`）

`protocol/src/models.rs:1179-1192` 的 `Reasoning` 是可回放的完整项：

```rust
Reasoning {
    id: Option<ResponseItemId>,
    summary: Vec<ReasoningItemReasoningSummary>,
    content: Option<Vec<ReasoningItemContent>>,
    encrypted_content: Option<String>,
},
```

请求侧固定声明 `include: ["reasoning.encrypted_content"]`（`core/src/client.rs:1284`）；历史按原顺序回放，`Reasoning` 排在 assistant 消息之前。非 OpenAI 端点会剥其它 item 的加密内容，但 `Reasoning` 在豁免名单里（`client.rs:1257-1274` 的 `matches!` 分支）。

### SHANIU 现状

`model_client/mod.rs:100-101` 的历史项只有：

```rust
FunctionCall { name: String, arguments: String, call_id: String },
FunctionCallOutput { call_id: String, output: String },
```

`mod.rs:263-266` 的注释已经把话说死：

```rust
/// `reasoning` 在这里**不发**：Responses 回放推理要独立的 `reasoning` item（需
/// `encrypted_content`），我们没有加密推理可回放；把明文 `reasoning` 当消息字段塞进去
/// 反而是陌生键。ChatCompletions 侧仍按 SH-083 回传。
```

**这个前提不成立**（本轮核对结论）：密文不是我们加密出来的，是响应里给的，收下来原样回放即可。

### 差异与后果

- Responses 臂每轮推理从零开始：重复分析、重复读同一批文件。
- 严格网关对"缺少上一轮 reasoning item 的续写"直接 400，表现为带工具调用的多轮更容易失败。
- 长任务里 Responses 臂比 Chat 臂更容易"失忆"。

## 2. 改法

### 2.1 数据模型（`model_client/mod.rs`）

`HistoryItem` 增加一个变体（与 `Message` 同级）：

```rust
/// 一个可回放的推理项。`encrypted_content` 是端点给的密文，原样保存、原样回放；
/// 我们不生成也不解读它。`summary` 用于 UI/摘要，回放时按原结构送回去。
Reasoning {
    id: Option<String>,
    summary: Vec<ReasoningSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted_content: Option<String>,
},
```

配套：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningSummary {
    #[serde(rename = "type")]
    pub kind: String,   // 端点给什么存什么（通常 "summary_text"）
    pub text: String,
}
```

**旧档案兼容**：新增变体不影响读取旧 rollout（旧档案里没有它）；反向（旧版本读新档案）会因未知变体反序列化失败，见 §5。

### 2.2 解析（`mod.rs:194-217` 的 `parse_item`）

增加分支：

```rust
"reasoning" => Some(ResponseItem::Reasoning {
    id: item.get("id").and_then(|v| v.as_str()).map(str::to_string),
    summary: parse_summary(item.get("summary")),
    encrypted_content: item.get("encrypted_content").and_then(|v| v.as_str()).map(str::to_string),
}),
```

`ResponseItem` 同步加 `Reasoning { .. }` 变体。SSE 侧不需要新事件：`response.output_item.done` 已经走 `parse_item`（`mod.rs:139-142`）。

### 2.3 入历史（`agent_loop/mod.rs`）

- `ModelStreamEvent::OutputItem(ResponseItem::Reasoning { .. })` 分支（`:1437-1442` 旁边）把项收进本轮 `reasoning_items: Vec<HistoryItem>`。
- 在 `:1659-1661` 推 assistant 消息**之前**，按顺序把 `reasoning_items` 推进 history：

```rust
for item in reasoning_items.drain(..) { history.push(item); }
if !text.trim().is_empty() || reasoning.is_some() {
    history.push(HistoryItem::Message { role: Role::Assistant, text, reasoning });
}
```

- 落档复用 `:360-395` 的 `record_new_history`：它按 `recorded` 游标追加，新变体自动进 rollout，无需新 `RolloutItem`。

### 2.4 回放（`mod.rs:270` 的 `responses_input`）

`HistoryItem::Reasoning` 序列化为 input 项：

```json
{"type":"reasoning","id":"rs_…","summary":[{"type":"summary_text","text":"…"}],"encrypted_content":"…"}
```

- `encrypted_content` 为空时省掉该键（部分端点不接受空串）。
- 位置必须排在对应 assistant 消息之前；顺序由 2.3 的 push 顺序保证。

### 2.5 Chat 路径（`mod.rs:399` 的 `chat_messages`）

`HistoryItem::Reasoning` **跳过**：ChatCompletions 侧的推理走 `Message.reasoning` 字段（SH-083），不能把 Responses 的 item 塞进 chat 消息数组。

### 2.6 预算与压缩（`compaction/mod.rs`）

- `item_tokens`（`:143-157`）增加分支：`approx_tokens(summary 文本) + encrypted_content.len()/4`。密文确实占输入 token，不计数会让预算系统性低估、压缩更晚触发（与 D3 同向恶化）。
- **切点规则**：reasoning 是"前向附着"——若 `range_end` 落在 `Reasoning` 与其后的 `Message` 之间（即 `history[range_end]` 是 `Message`、`history[range_end-1]` 是 `Reasoning`），把 `range_end` 前移一条，让这一对留在保留侧。理由：只看保留侧时，"有消息没有推理"是无害的，但"推理丢了、消息还在"会让严格端点在续写时对不上号。这条规则加在 `plan_if_needed` / `keep_from_tail` 计算切点的同一处，不要在下游再补。

### 2.7 档案消费方

`rollout/resume.rs` 的重放、`session_reference.rs:303-320` 的会话摘要、`session_compact.rs` 的计数都要有新变体的分支（编译器会逐个报错，别用 `_ =>` 吞掉）。规则：

| 消费方 | 处理 |
| --- | --- |
| resume 重放 | 原样保留（顺序不变） |
| 会话摘要 / 引用 | 忽略（不展示密文与推理） |
| turn 计数 | 不算一轮、不算一条用户消息 |
| 导出 | 跟随 `rollout` 的 serde，原样导出 |

### 2.8 接口变化

- `HistoryItem`、`ResponseItem` 各加一个变体（破坏性：所有 `match` 必须补齐）。
- `build_responses_request` 的 `include` 由 D1 的 `ProviderConfig.reasoning_replay` 控制（D2 新增该开关，默认 `true`）。
- 无 Tauri command / 前端事件契约变化（reasoning 不进 `EventMsg`）。

## 3. 会先变红的用例

### 3.1 单元（`model_client/mod.rs` 的 `#[cfg(test)]`）

- `reasoning_item_is_parsed_from_output_item_done`：构造 SSE `response.output_item.done`，断言 `OutputItem(ResponseItem::Reasoning { id: Some("rs_1"), summary: [{"type":"summary_text","text":"think"}], encrypted_content: Some("gAAA") })`。
- `chat_messages_skips_reasoning_items`：历史 `[Reasoning, Message(Assistant), Message(User)]`，断言 chat 数组长度为 2 且不含 `reasoning` 顶层键。
- `responses_input_places_reasoning_before_assistant_message`：断言 `input[0]["type"] == "reasoning"`、`input[1]["role"] == "assistant"`。

### 3.2 集成（`tests/responses_reasoning_replay.rs`，新建）

用脚本化 SSE 跑两轮：

- 第一轮返回 `reasoning(encrypted="gAAA")` + `function_call(c1)`；工具回包后第二轮请求发出。
- 断言第二轮请求 `input` 里存在 `{"type":"reasoning","encrypted_content":"gAAA"}`，且位于 assistant 消息之前。
- 断言 rollout 文件包含 `"Reasoning"` 项；`resume` 后重建的 history 仍含该项。
- 断言压缩请求（触发 `/compact`）的 `input` 里同样保留 reasoning 项，除非它落在被压区间内。

### 3.3 预算/切点（`compaction/mod.rs` 单元测试）

- `reasoning_tokens_are_counted`：同一段密文计入后 `history_tokens` 必须增大 `len/4`。
- `cut_never_separates_reasoning_from_its_message`：构造 `[User, Reasoning, Assistant, User]`，保留额度只够最后一条时，断言 `range_end == 1`（Reasoning 与 Assistant 一起留下）而不是 `2`。

### 3.4 反向验证（必须红）

1. 把 `include` 关掉（`reasoning_replay=false`）→ 3.2 第一条红（拿不到密文）；
2. 删掉 `parse_item` 的 `"reasoning"` 分支 → 3.1 第一条红；
3. 把 push 顺序改到 assistant 消息之后 → 3.1 第三条红；
4. `chat_messages` 不再跳过 → 3.1 第二条红；
5. 去掉切点前移规则 → 3.3 第二条红；
6. 不把密文计入预算 → 3.3 第一条红。

逐条记录输出后恢复，最终全绿。

## 4. 影响面

### 4.1 会变红的既有用例（预计）

- `model_client/mod.rs` 里所有对 `HistoryItem` 做穷尽 `match` 的测试构造器（编译器会指出）；
- `tests/responses_request_shape.rs`：若用例按位置取 `input[0]`，而夹具里新增了 reasoning，需要重新编号；
- `tests/compaction_*`：凡断言切点下标的用例，遇到新夹具可能偏移（只有引入 reasoning 夹具的才会变，不引入的不受影响）。
- `session_reference` 的既有断言：会话摘要文本不变（reasoning 被忽略），预期不需要改。

### 4.2 用户可见行为

- Responses 臂多轮：模型不再重复推理，长任务成功率与成本改善；请求体变大（密文体积），首轮略慢。
- 会话导出里出现 `Reasoning` 项（含密文）；这是"可见但不可读"的数据，导出体积增加，发布说明要写。
- **回退不可逆**：用新版本产生的档案，旧版本读会失败（未知变体）。发布说明必须写"降级前先导出/备份"。

## 5. 风险

| 风险 | 说明 | 处置 |
| --- | --- | --- |
| 旧版本读新档案失败 | `HistoryItem` 是 serde 枚举，无 `#[serde(other)]` | 发布说明写明；不为此加兜底（加了会静默丢推理项） |
| 密文体积大 | 计费与预算都受影响 | 计入预算（2.6）；`reasoning_replay=false` 可关 |
| 严格端点不接受回放形状 | 少数字段（如 `content`）可能必填 | 先用夹具覆盖 OpenAI 形状；失败时按端点错误码补字段，不猜 |
| 切点规则写散 | 两处计算切点会漂移 | 规则只写在 `keep_from_tail` 一处，另一处调用它 |

## 6. 代价

约 1.5–2 天：数据模型 + 解析/回放 + 切点规则 + 6 条断言 + 旧档案回归。测试夹具（第二轮流）占大头。

## 7. 验证命令

`cargo test --test responses_reasoning_replay -- --test-threads=1`；`cargo test -- --test-threads=1`；`cargo clippy --all-targets -- -D warnings`；`cargo fmt --check`；`.\scripts\check-deps.ps1`；`.\scripts\dev\check-doc-claims.ps1`。

## 8. 提交策略

一笔 `feat(model): 捕获并回放 Responses reasoning item`，依赖 D1 已合并；不 force；不碰 `evals/`。
