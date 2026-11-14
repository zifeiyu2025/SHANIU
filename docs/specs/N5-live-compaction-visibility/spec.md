# N5（P2）压缩提示实时不显示 — 规格

> 范围：只改前端适配层（`src/transport/`），不动 `src-tauri/`、不动 `src/dsh/` 的 vendored 代码。
> 目标：压缩发生的当下，聊天时间线出现一条可见的压缩条目；重开后同一位置仍只有这一条，文案/位置与实时一致。

## 1. 链路定责（依据）

| 环节 | 结论 | 依据 |
|---|---|---|
| 后端广播 | **已广播** | 两轮真机证据里 `compaction_started / compaction_summarized / compaction_ended` 三件套齐全：`docs/smoke/2026-11-14-58f56f6/r2/evidence/03-events-final.jsonl:2083-2085`、同文件 `2134-2136`、`2283-2285`；第一轮 `evidence/03-events.jsonl:722-724`。发送链路见 `src-tauri/src/events.rs:791-811`（落盘 → gateway 广播 → Tauri IPC）。 |
| 前端接收 | **已接收并落了 journal** | `src/transport/adapter.ts:2519-2599`：`compaction_started→compaction/start`、`compaction_summarized→compaction/summary`、`compaction_ended→compaction/end`，并在 ended 追加一条 `developer/message`（`source.kind='compaction-notice'`, `form:'notice'`）。现有用例 `src/transport/adapter.spec.ts:1284-1313` 断言了这条通知存在。 |
| 界面显示 | **收下了但没有可渲染的「压缩」节点** | 聊天面板的压缩行只由 `compactionDefinition` 产出，而它要求一个 **checkpoint**：`user/message` 替换事件 + `source.kind='compact-checkpoint'`（`src/dsh/packages/client/ui-chat/src/client/conversation-nodes/compaction.ts:31-57`，识别逻辑在 `command.ts:78-93`；`buildViewNode` 找不到 checkpoint 直接 `return null`）。适配层两条路径都**从不发**这个事件，所以实时永远不会出现压缩行。 |

补充（「只有重开才有」的来源）：重开时 `adapter.ts:1893-1917` 会为每个 `compaction_summary` 条目补一条 `compaction/summary` + 一条 `developer/message` 通知；这条通知被渲染成「上下文注入」上下文行。实时那条同形通知落在当前**未收口回合的步内**（`turn/step` = 当前），随回合过程收拢；回放那条落在**回合边界**，才单独成行。两者不是同一条渲染路径，因此表现为「实时没有、重开一条」。

结论：**不是没广播，也不是没接收，是「收下后没有生成可见的压缩节点」**；重开看到的是回放路径补的替代物。

## 2. 方案（决策）

复用 dsh 既有的自动压缩节点 `CompactionNodeView`/`CompactionItem`——它就是为「压缩发生」设计的既有视觉，不新增任何组件或样式；同时把两条路径统一到**同一个事件形状**，保证实时/重开同位、同文、只一条。

### 2.1 适配层统一发出 checkpoint 锚点

新增内部函数（`src/transport/adapter.ts` 模块内，`pushEvent` 附近）：

```
pushCompactionCheckpoint(journal, compactionId, hiddenSeq)
  → pushEvent(journal, 'user/message', {
      id: `shaniu-compact-checkpoint-${compactionId}`,
      role: 'user',
      content: [],
      source: { kind: 'compact-checkpoint', compactionId },
    }, { surfaceOp: 'replace', sourceEventSeqs: [hiddenSeq] })
```

- `surfaceOp: 'replace'` + `sourceEventSeqs` 满足 `isReplacementSurfaceEvent` 与 wire 校验；
- `hiddenSeq` 指向本次压缩的 `compaction/start` 事件 seq（该事件本身不产出节点，隐藏它零损失），避免自引用；
- `messageDefinition` 明确排除 checkpoint（`message.ts:46-50`），不会多出一条用户气泡；`compactionDefinition` 据此产出唯一压缩节点。

### 2.2 实时路径（`case 'compaction_ended'`，成功分支）

- `CompactionTrace` 增加 `startSeq?: number`；`compaction_started` 分支把 `pushEvent` 返回的 seq 存进去。
- `compaction_ended && ok` 时：**删掉**原来的 `developer/message` 通知，改为 `pushCompactionCheckpoint(journal, msg.compaction_id, trace.startSeq)`。
  - 卡片摘要来自同批 `compaction/summary`（`command.ts:101-131` 从 summary 事件取文本与 shadowed 计数/token），实时信息最全。
- 失败分支维持现状（`developer/message` 失败通知 + `compaction/end.error`），因为失败时没有「压缩发生」可标记。

### 2.3 回放路径（`entry.kind === 'compaction_summary'`）

改成与实时同构的三步：

1. `compaction/start`（记 seq，给 checkpoint 当 `hiddenSeq`）；
2. `compaction/summary`（保留现有 `summary` 文本）；
3. `pushCompactionCheckpoint(journal, compactionId, startSeq)`，`compactionId` 仍用 `shaniu-replay-compaction-${index}`。

**删掉** 回放的 `developer/message` 通知。这样重开后：一条压缩卡、零条「上下文注入」替代物；与实时同形同位置口径（都锚在被压缩区间的锚点处）。

### 2.4 文案一致性

- 实时/回放的卡片文本都只取自 `compaction/summary.summary`（实时=模型摘要，回放=档案里同一条摘要），不会出现两套措辞。
- 回放没有 `history_tokens_after`/压力值可填，`shadowedTokenCount` 由 summary 事件带（实时有、回放无）；**不伪造**缺失数字，卡片按可选字段渲染。这是「不回放不可知事实」的既有纪律（同 A0-4）。
- 旧的 `compactedNotice()` 通知文案仅在**失败**路径保留。

### 2.5 明确不做

- 不改 `src-tauri/`（后端已正确广播）。
- 不改 `src/dsh/` vendored 代码（避免上游漂移，`align` 账本不受影响）。
- 不给压缩新增 toast/横幅等第二套视觉。

## 3. 回归用例

文件：`src/transport/adapter.spec.ts`

1. **实时即出现**：模拟 `prompt → compaction_started → summarized → ended(ok)`，断言 journal 中：
   - 恰有一条 `user/message` 且 `source.kind === 'compact-checkpoint'`、`source.compactionId === 'c2'`、`surfaceOp === 'replace'`；
   - **没有** `source.kind === 'compaction-notice'` 的 `developer/message`；
   - 所有事件过 `assertSessionWireEvent`。
2. **能被 UI 认成压缩节点**：用同一份 journal 事件调用 `compactionDefinition.match`，断言返回 `{ id: 'c2', role: 'update' }`；构造 `state = { checkpoint, summary }` 走 `buildViewNode`，断言产出 `kind: 'compaction'` 且 `summary` 文本正确（证明「可见」而不只是「落 journal」）。
3. **压缩失败仍出声**：沿用/调整现有 `ok:false` 用例——失败不产 checkpoint、仍有一条 `form:'notice'` 的失败通知。
4. **重开不重复**：用两条 `compaction_summary` 条目走回放，断言 journal 中 checkpoint 恰 2 条（每个锚点一条）、`developer/message` 的 `compaction-notice` 为 0 条；同一会话再回放一次，条数不变（无累积）。
5. **反向验证**：
   - 临时把 `pushCompactionCheckpoint` 改回发 `developer/message` 通知 → 用例 1/2 必红；
   - 临时保留回放通知 → 用例 4 必红。
   - 两次反向都在本地跑出红，再恢复，留命令与输出摘要进报告。

## 4. 验证命令

- 前端：`pnpm typecheck`、`pnpm test`
- 仓库根：`.\scripts\check-deps.ps1`
- 不改 Rust，仍跑一遍 `cargo test`（确认无跨层影响）与 `cargo clippy --all-targets -- -D warnings`
- 格式化：只对改动文件跑 `rustfmt`（本任务无 Rust 改动，预计不触发）

## 5. 提交与发布

- 只 `git add src/transport/adapter.ts src/transport/adapter.spec.ts docs/specs/N5-live-compaction-visibility/spec.md docs/specs/N5-live-compaction-visibility/tasks.md`。
- **不碰** 其他会话的 `docs/smoke/`。
- 推前 `git fetch`，`git push` 不带 `--force`；盯 GitHub Actions 到 gate + product-smoke 出结论再报。

## 6. 风险与替代

- 视觉差异：压缩卡（`CompactionItem`）与「上下文注入」行不是同一外观。理由：现有「上下文注入」行在实时只能落在回合内并被回合过程收拢，无法与重开同位；checkpoint 是上游为「压缩发生」预留的唯一实时可见载体。若评审要求必须沿用上下文注入行的外观，备选方案是在 vendored `ui-chat` 里为 `compaction-notice` 增加「回合边界常显」规则——代价是改上游代码并登记 align 账本，本规格不采用。
- 兼容：改后旧档案重开时由 `compaction_summary` 回放生成 checkpoint，历史面板不受影响（`SessionEntry` 形状未变）。
