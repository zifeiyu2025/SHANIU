# D10（必须改）崩溃后孤立工具调用补「结果未知」，不再剔除 — 规格 v1

> 来源：`docs/analysis/reference-delta-2026-11-15.md` §4 D10。
> 本轮**只出规格，不改代码**。
> 范围：`src-tauri/src/rollout/resume.rs`、`src-tauri/src/lib.rs`、`src-tauri/tests/{rollout_resume,compaction_resume}.rs`。

## 1. 现状与上游对照

### 上游（codex `b016cd18`）

`core/src/context_manager/normalize.rs:21-140` 的 `ensure_call_outputs_present`：恢复历史时**不丢调用**，给每个没有回包的调用补一条 output。补的内容是固定的 `aborted`（`:76`、`:113`）：

```rust
output: FunctionCallOutputPayload::from_text("aborted".to_string()),
```

dsh 侧同一语义（`packages/core/session/src/repair.ts:53` 的 `openTurnClosers`、`:209` 的 `interruptedTurnClosers`、`:189` 的 `ToolNotStartedError`）：回合没闭合就补一个"调用已中断"的收口项，而不是把调用删掉。

### SHANIU 现状

`rollout/resume.rs:186-198` 直接**剔除**：

```rust
let before = history.len();
history.retain(|item| match item {
    HistoryItem::FunctionCall { call_id, .. } => answered.contains(call_id),
    _ => true,
});
let dropped_unsafe_tail = before - history.len();
```

`Resumed.dropped_unsafe_tail` 经 `lib.rs:609/625/712/797/3088` 报到界面（"重放丢弃 N 条…"）。

### 差异与后果

- 模型看到的是"我那轮什么都没调用"：它不知道 `cargo test` 到底跑没跑、`apply_patch` 到底落没落盘，下一步会**重复执行**（对非幂等命令是二次副作用），或者基于错误前提继续。
- 用户看到"丢弃 N 条"，但不知道是哪条、有没有副作用。
- 与上游相比，少了一条"结果未知"的显式语义。

## 2. 改法

### 2.1 补写而不是剔除（`rollout/resume.rs:186-198`）

```rust
/// 崩溃恢复时补在孤立调用后面的结果文本。措辞要让模型做两件事：
/// 1) 不假设它执行成功；2) 需要时重新调用，但先检查副作用。
pub const ORPHAN_TOOL_OUTPUT: &str =
    "工具调用结果未记录：宿主在结果写入前中止（会话已恢复）。\
不要假设该调用已执行或已成功；如需继续，请先确认副作用，再决定是否重新调用。";

let answered: HashSet<String> = ...;           // 现状保留
let before = history.len();
let mut patched_orphan_calls = 0usize;
let mut fixed: Vec<HistoryItem> = Vec::with_capacity(history.len());
for item in history {
    match &item {
        HistoryItem::FunctionCall { call_id, .. } if !answered.contains(call_id) => {
            patched_orphan_calls += 1;
            let call_id = call_id.clone();
            fixed.push(item);
            // 紧跟在调用之后，顺序与"应答路径"一致：call 全在前、output 全在后是常态，
            // 但恢复场景下必须逐对相邻，否则模型会把 output 对到别的调用上。
            fixed.push(HistoryItem::FunctionCallOutput {
                call_id,
                output: ORPHAN_TOOL_OUTPUT.to_string(),
            });
        }
        _ => fixed.push(item),
    }
}
```

- **只在内存里补**，不回写档案（档案是 append-only，见 `rollout/src/policy.rs` 的对齐结论）。重复 resume 会得到同样的结果，天然幂等。
- `answered` 的判定口径不变（按 `call_id` 单向）。

### 2.2 字段与文案（`Resumed` + `lib.rs`）

- `Resumed.dropped_unsafe_tail: usize` → `patched_orphan_calls: usize`（语义变了，名字必须跟着变，不留旧名）。
- `lib.rs` 五处文案：`"重放丢弃 {n} 条未完成的工具调用"` → `"重放时补了 {n} 条工具调用的「结果未知」占位，模型不会假设它们成功"`。

### 2.3 不改的部分（明确非目标）

- `nearest_safe_cut`（`:227-235`）保持不变：它服务的是 fork 截断，语义是"不要用户截在工具对中间"，与本条的恢复修补是两件事。
- `model_client::degrade_orphan_tool_outputs`（`:467`）保持不变：它处理反方向（有 output 无 call）。
- 不新增 `ToolCallResult::Unknown` 事件：本条只影响**恢复重建的历史**，不产生运行态事件；加一个只在一个场景出现的事件变体会让 `events.rs` 的契约变复杂。

### 2.4 接口变化

- `Resumed` 字段改名（破坏性：`lib.rs` 与测试）；
- 无 Tauri command 结构变化（只是字段值语义变），但**用户看到的提示文案会变**。

## 3. 会先变红的用例

### 3.1 新增（`tests/rollout_resume.rs`，先红）

- `orphan_call_gets_unknown_output_on_resume`：档案 `[user, function_call(c1)]`；断言
  `resumed.history == [user, call(c1), output(c1, contains("结果未记录"))]`、
  `patched_orphan_calls == 1`、`history.len() == 3`。
- `orphan_patch_stays_adjacent_to_its_call`：档案 `[user, call(c1), user2]`；断言 output 在 `c1` 之后、`user2` 之前。
- `resume_is_idempotent_for_patched_calls`：同一档案连续 resume 两次，两次 history 完全相同（证明没有回写/追加）。
- `two_calls_only_one_answered_patches_only_the_open_one`：两个调用、只回了一个，只补一条。
- `compacted_history_still_patches_open_call`：档案里压缩锚点把调用留下、输出压掉时，重放后仍补占位。

### 3.2 会红的既有用例（预计）

`tests/compaction_resume.rs:294-340` 的三条（`anchor_written_after_an_unfinished_round_is_still_applied` 等）断言 `history.len() == 1` / `dropped_unsafe_tail == 1`；改后应为 `len == 3`（摘要 + call + 占位）且 `patched_orphan_calls == 1`。这是**语义变化本身**，不是回归。

`src-tauri/src/rollout/resume.rs` 内部若有用例断言"剔除后条数"，同样按新语义改写。

### 3.3 反向验证（必须红）

1. 恢复旧的 `retain` 剔除 → 3.1 第一条红；
2. 把占位插到历史末尾而不是紧跟调用 → 第二条红；
3. 把补写结果回写档案（append 一条 `ResponseItem`）→ 第三条红（第二次 resume 会出现两条）；
4. 只处理"最后一个"孤立调用 → 第四条红；
5. 在压缩锚点之前就判配对（而不是按最终历史）→ 第五条红。

## 4. 影响面

- **用户可见**：崩溃/强杀后重开，不再看到"丢弃 N 条"，而是"补了 N 条结果未知"；模型会明确告知"不确定上一条命令是否执行"，可能主动检查（例如先 `git status`）而不是盲目重跑。
- **模型行为**：非幂等命令可能被二次执行——这是上游同样接受的代价，靠提示文本约束。若未来要更稳，可在文本里点名"写盘类命令先核对文件"。
- **档案**：完全不改盘上内容；导出、搜索、`session_reference` 都不受影响。
- **其他调用方**：`lib.rs` 五处字段与文案、相关测试。

## 5. 风险

| 风险 | 说明 | 处置 |
| --- | --- | --- |
| 模型把占位当成工具真的返回了 | 文本可能被误读 | 首句固定"工具调用结果未记录"，并明确"不要假设已成功" |
| 二次副作用 | 上游同款代价 | 文本要求先确认副作用；不自动重放 |
| 占位被压缩当普通输出 | 参与预算与摘要 | 可接受：它就是一条真实的历史条目 |
| 字段改名影响面漏改 | `lib.rs` 5 处 | 编译器兜底 + 全仓 grep 旧名 |

## 6. 代价

约 0.5–1 天：resume 改写 + 5 条新用例 + 3 条旧断言重写 + `lib.rs` 文案。

## 7. 验证命令

`cargo test --test rollout_resume -- --test-threads=1`；`cargo test --test compaction_resume -- --test-threads=1`；`cargo test -- --test-threads=1`；`cargo clippy --all-targets -- -D warnings`；`cargo fmt --check`；`.\scripts\check-deps.ps1`；`.\scripts\dev\check-doc-claims.ps1`。

## 8. 提交策略

一笔 `fix(rollout): 恢复时为孤立工具调用补「结果未知」`；不 force；不碰 `evals/`、`docs/smoke/`。
