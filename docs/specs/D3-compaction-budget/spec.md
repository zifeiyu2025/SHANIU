# D3（必须改）压缩预算扣「输出预留 + headroom」，保留比例对齐 — 规格 v1

> 来源：`docs/analysis/reference-delta-2026-11-15.md` §2 D3。
> 本轮**只出规格，不改代码**。
> 范围：`src-tauri/src/compaction/mod.rs`、`src-tauri/src/config.rs`、`src-tauri/src/agent_loop/mod.rs`。

## 1. 现状与上游对照

### 上游（dsh `39163dd3`）

`packages/compaction/compaction-basic/src/config.ts:181-197`：

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

- `messageBudgetTokens = W - O`（窗口减该模型 max output，`W` 取模型声明窗口，`O` 取路由输出预留）。
- 默认值（`config.ts:20/23/75`，`types.ts:17-24` 的注释同口径）：`thresholdRatio=0.8`、`retainRatio=0.16`、`headroomTokens=65536`。

### SHANIU 现状

`compaction/mod.rs:27` 保留 30%；`mod.rs:36-39` 只取窗口 80%；`mod.rs:49-54` 取 `min(configured, window*80%)`：

```rust
const KEEP_RATIO_PERCENT: usize = 30;
fn window_allowance(window: usize) -> usize {
    let window = if window == 0 { FALLBACK_CONTEXT_WINDOW } else { window };
    window.saturating_mul(4) / 5
}
pub fn compact_token_budget(configured: usize, window: usize) -> usize {
    if configured == 0 { return 0; }
    configured.min(window_allowance(window))
}
```

`max_tokens` 已经能解析（`config.rs:334` 的 `max_tokens_effective`），但**没有进 `ResolvedModel`**（`config.rs:450-471`），压缩侧完全看不到输出预留。

### 差异与后果（200K 窗口 / 16K 输出 / 默认 headroom 64K）

|  | 阈值 | 保留 |
| --- | --- | --- |
| SHANIU | 160K | 48K |
| dsh | 120K（`min(160K, 184K-64K)`） | 约 29K（`184K×16%`） |

- 压缩更晚：一次大工具输出就可能 400；
- 压完保留更多：别人压 1 次我们要 2–3 次，摘要开销与失忆风险翻倍；
- 模型声明了 `max_tokens` 却没参与预算，等于白存。

## 2. 改法

### 2.1 把输出预留带进解析结果（`config.rs`）

`ResolvedModel` 增加：

```rust
/// 该模型的单次输出上限（`max_tokens_effective` 的结果）。压缩预算要用它扣掉
/// 「下一轮回复要占的地方」；0 = 未知，压缩侧回退到保守默认。
pub max_output: u32,
```

**接口变化**：`ResolvedModel` 加字段；所有构造点（配置解析与测试夹具）由编译器逐个报出。

### 2.2 预算公式（`compaction/mod.rs:27-66`）

```rust
const FALLBACK_MAX_OUTPUT: usize = 16_384;
const DEFAULT_HEADROOM_TOKENS: usize = 65_536;
/// 保留额度占「窗口减输出」的比例。取 16%（与 dsh 默认一致）。
const KEEP_RATIO_PERCENT: usize = 16;

fn window_and_output(window: usize, max_output: usize) -> (usize, usize) {
    let w = if window == 0 { FALLBACK_CONTEXT_WINDOW } else { window };
    let o = if max_output == 0 { FALLBACK_MAX_OUTPUT } else { max_output };
    (w, o.min(w / 4))          // 输出预留不超过窗口 1/4，防小窗口被压成 0
}

fn window_allowance(window: usize, max_output: usize) -> usize {
    let (w, o) = window_and_output(window, max_output);
    let message_budget = w.saturating_sub(o);
    // headroom 不超过消息预算的一半：小窗口（32K）下固定 64K 会把阈值压成 0，
    // 每轮都压缩比不压缩更糟。大窗口（≥192K）时与上游的 65536 完全一致。
    let headroom = DEFAULT_HEADROOM_TOKENS.min(message_budget / 2);
    message_budget.saturating_sub(headroom).min(w.saturating_mul(4) / 5)
}
```

- 保留额度：`:273` 与 `:296-300` 的 `budget * KEEP_RATIO_PERCENT / 100` 改成 `budget * 16 / 100`（把常量改为 16 即可，两处都读常量）。
- `USER_TEXT_KEEP_PERCENT = 30`（`:322`）**不动**：它约束的是「摘要里保留多少用户原文」，不是上下文保留额度，与上游的 `retainRatio` 不是一回事。
- `compact_token_budget(configured, window, max_output)`、`manual_token_budget(configured, window, max_output)` 两个签名都加参数。

### 2.3 调用点（`agent_loop/mod.rs:839-845`）

```rust
token_budget: crate::compaction::compact_token_budget(
    cfg.agent.token_budget, window, resolved.max_output as usize),
manual_token_budget: crate::compaction::manual_token_budget(
    cfg.agent.token_budget, window, resolved.max_output as usize),
```

`window` 的取值沿用既有 `resolved.context_window`；`max_output` 用 2.1 的新字段。

### 2.4 口径同步

`docs/04-协议与上下文.md` 的预算章节（`context_window * 4/5` 的旧表述）同步成新公式，并写清三个默认值来自哪里。

## 3. 会先变红的用例

### 3.1 会红的既有用例（预计，必须逐条确认）

`compaction/mod.rs` 单元测试：

- `compact_budget_follows_window_and_never_uses_configured_limit_blindly`（`:488-498`）：`128_000 → 102_400` 的断言会变成 `56_000`（`112K-56K` 与 `102.4K` 取小）。这条变红正是"公式确实生效"的证据，不是要保住的旧行为。
- `manual_budget_is_clamped_by_window_and_survives_disabled_auto_compaction`（`:503-512`）：同上，三条 102_400 全变。
- `keep_allowance_boundary_keeps_the_item_that_fits_exactly`（`:560-575`）：注释写死 "budget 100_000 → 额度 30_000"，30% → 16% 后边界值变 `16_000`。
- `manual_plan_respects_keep_allowance_when_budget_is_tight`（`:757`）：budget 100 → 额度 30 变 16。
- `:618-622` 的手动压缩夹具（预算 100、保留 30）需要重排，否则断言对象变了。

### 3.2 新增用例（先红）

- `budget_subtracts_output_reservation_and_headroom`：
  ```rust
  assert_eq!(compact_token_budget(500_000, 200_000, 16_000), 120_000); // 与 dsh 同值
  assert_eq!(compact_token_budget(500_000, 128_000, 16_000), 56_000);
  ```
- `tiny_window_never_collapses_to_zero_budget`：`compact_token_budget(500_000, 32_000, 4_000)` 必须 `>= 8_000`（不许出现 0 或负数回绕）。
- `unknown_output_falls_back_to_conservative_reservation`：`max_output=0` 时结果等于按 `16_384` 算出的值。
- `retain_ratio_is_sixteen_percent`：`tail_target` 在 budget=100_000 时保留额度 16_000（用既有的 `kept + cost > allowance` 边界构造）。
- `max_tokens_effective_reaches_compaction`：`ResolvedModel.max_output` 取 `max_tokens_effective`（内置模型回落与显式配置各一条）。

### 3.3 反向验证（必须红）

1. 把 `max_output` 从公式里去掉 → 3.2 第一条红（200K 会算回 160K）；
2. 保留比例改回 30 → `retain_ratio_is_sixteen_percent` 与两条边界用例红；
3. 去掉 `o.min(w/4)` → 小窗口用例红（32K 窗口 4K 输出被压成 0 或负）；
4. 去掉 headroom 下限 → `tiny_window` 红（阈值塌成 0）；
5. `ResolvedModel.max_output` 写死 0 之外的常量 → 最后一条红。

## 4. 影响面

- **用户可见**：压缩更早、压得更狠、每次摘要后剩余空间更大。主观体验是"长会话更少 400，但更早看到压缩提示（若前端展示）"。
- **成本**：摘要调用次数可能增加（阈值更低），但每次输入更小；净效应随窗口大小变化，需在评测线用一条长会话样本对比 token 总量。
- **既有行为**：`token_budget = 0` 仍表示关闭自动压缩（`:49-51` 的短路保持不变）；`manual_token_budget` 在 `configured=0` 时仍按窗口算。
- **配置兼容**：`agent.token_budget` 语义不变；用户显式配置若大于新阈值，会被新阈值压住（与今日"被 80% 压住"同理）。

## 5. 风险

| 风险 | 说明 | 处置 |
| --- | --- | --- |
| 小窗口压缩过频 | 32K 窗口下阈值只有 ~14K | 用 `tiny_window` 用例守住"不为 0"；实测 32K 模型另跑一条样本 |
| 公式与上游有细微差 | `headroom` 多了 `min(message_budget/2)` 夹取 | 这是**有意的等价实现**：上游假设窗口 ≥192K；我们支持 32K–128K，需要夹取。写进 §2.2 注释与 `docs/04` |
| 既有断言大面积改写 | 5 处测试写死了旧数值 | 先算新值再改断言，禁止"跑什么就写什么" |

## 6. 代价

约 0.5–1 天：公式与签名 + `ResolvedModel` 字段 + 5 条新用例 + 5 处既有断言重算 + 文档口径。

## 7. 验证命令

`cargo test compaction -- --test-threads=1`；`cargo test -- --test-threads=1`；`cargo clippy --all-targets -- -D warnings`；`cargo fmt --check`；`.\scripts\check-deps.ps1`；`.\scripts\dev\check-doc-claims.ps1`。

## 8. 提交策略

一笔 `fix(compaction): 预算扣输出预留与 headroom，保留比例对齐 16%`；不 force；不碰 `evals/`。
