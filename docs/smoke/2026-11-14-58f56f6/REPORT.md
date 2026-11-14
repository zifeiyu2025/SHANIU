# 收口状态收敛（Phase 0–5）人工冒烟报告

- **commit**：`58f56f6`（Phase 4 收尾；Phase 1–3 分别为 `c3ceb38`、`a9a0667`、`9b0f09f`）；本次真机冒烟在 `main`=`2f512ef`（2026-11-14 20:48）上构建执行
- **构建**：release 便携包，独立 `CARGO_TARGET_DIR=scratch\target-smoke` 编译 `scratch\target-smoke\release\shaniu.exe`（5m11s），再经 `scripts/package-portable.ps1 -Exe … -OutDir scratch\smoke-dist` 打成 `SHANIU-Portable-0.2.6.zip`
- **数据目录**：A=`scratch\smoke-1411\SHANIU-Portable`（隔离副本，预置 LongCat 提供商与工作区 `scratch\smoke-proj`；为触发压缩把 `[agent] token_budget` 由 500000 改为 **5000**，属测试配置改动，非产品默认）；B=`scratch\smoke-1411-fresh\SHANIU-Portable`（全新数据目录）
- **执行人 / 时间**：人工真机（GUI）执行，2026-11-14 21:00–21:45（Asia/Shanghai）
- **机器可核部分**：CI run `43595461027`（gate 7 项 + product-smoke 全绿）、`43593671916`、`43591351670`、`43587236592`

> 十项人工冒烟已按当前 `main` 构建的便携包真机执行完毕：**6 项 PASS、4 项 FAIL**。
> 证据见 `shots/`（截图）与 `evidence/`（脱敏日志/档案/导出）。下方「机器已核」表为
> 自动化证据，保持原样，不与人工结论混算。

## 逐项记录

| # | 场景 | 步骤（可复现的最小步骤） | 预期（只写本方案承诺） | 实际 | 结论 | 证据 |
|---|---|---|---|---|---|---|
| 1 | 新建会话完成 + 强杀重开 | 新建会话「只回复：hi」→ 等回合结束；任务管理器结束进程 → 重开并打开该会话 | 完成：1 轮 1 步、单条结束行、无横幅；重开：仍 `completed`、单条、1 轮 1 步 | 首轮显示「已完成」、1 轮 1 步、单条 `turn_end`；强杀重开仍 `completed`、1 轮 1 步，无「已中止/状态未知」横幅；rollout 未重复追加 `turn_end` | PASS | `shots/01-live-completed.png`、`shots/01-reopen.png`、`evidence/01-rollout-before-kill.jsonl` |
| 2 | 触发 exec 审批 | 让模型执行需要审批的命令；卡片出现后强杀重开 | 卡片出现；重开后仍保留并显示「确认中」，不自动撤 | 审批卡正常出现（`风险 execute：unified_exec（echo smoke-approval-probe）`、拒绝/允许一次、状态栏「1 个请求等你审批」）；**强杀重开后该回合与卡片整体消失**，rollout/events 均无该条用户消息（用户消息未落盘即被强杀丢失） | FAIL | `shots/02-approval-card.png`、`shots/02-reopen.png`、`evidence/02-rollout-after-kill.jsonl`、`evidence/02-events-after-kill.jsonl` |
| 3 | 压缩后追问 | 长会话触发压缩（顶部压缩通知）→ 追问一句 → 再重开 | 摘要不占回合、无中止横幅；重开轮次与实时一致 | `token_budget=5000`（测试配置）下确实触发自动压缩（日志 `上下文已压缩（SH-032） compressed_items=1/10`）；摘要未当作用户发言、无中止横幅；但实时「4 轮 5 步」→ 重开「2 轮 3 步」→ 再次重开「4 轮 8 步」，轮次/步数与实时不一致；压缩提示实时不显示（重开后才以「上下文注入」出现） | FAIL | `shots/03-live-after-followup.png`、`shots/03-reopen.png`、`evidence/03-events.jsonl`、`evidence/03-rollout.jsonl` |
| 4 | 小改动并跑测试 | 让模型改一个文件并跑一次测试 | 工具卡、验证通知正常；「结果未知」只在真的没有结论时出现 | 模型改 `src/sum.js` + `test/sum.test.js`，`pnpm test` 经审批执行 exit 0，显示「已完成，用时 4 分 44 秒」，无「结果未知」；重开后步数 9→8（同一轮次计数偏差，见新问题 N6） | PASS | `shots/04-tool-approval.png`、`shots/04-test-passed.png` |
| 5 | 新数据目录首发 | 用全新数据目录启动 | 会话列表显示空状态，不是「暂不可用」 | 全新数据目录首发，会话树仅「默认工作区/未分组」，无「暂不可用」 | PASS | `shots/05-fresh-data-dir.png` |
| 6 | 正常关窗走托盘 | 点关闭（进托盘）→ 从托盘重开 | 不出现中止横幅 | 点关闭后进程存活、窗口隐藏；用 UIAutomation 双击托盘图标唤回，无中止横幅，会话仍为已完成 | PASS | `shots/06-before-close.png`、`shots/06-tray-restore.png` |
| 7 | 长工具静默 > 60s | 跑一条超过 60s 的命令，静默期间观察 | 不自动变「已停止」；状态保持进行中/未知 | `Start-Sleep 90` 静默超过 60s 期间状态保持「正在调用工具」，未自动变「已停止」（承诺项满足）；**但命令结束后压缩（`compressed_items=10`）令后续模型请求持续失败**：provider 500 `Cannot resolve tool name for tool_call_id='call_0e6f3061a37b43cf96c3ef09'`，重试 5 次后「本轮运行失败 模型服务端错误」，再提问同样失败（新问题 N1） | PASS（承诺项；附带 P0 缺陷） | `shots/07-silent-72s.png`、`shots/07-after-90s.png`、`shots/07-session-broken-after-compaction.png`、`evidence/07-shaniu.log.2026-11-14.log` |
| 8 | 子代理泳道 | 触发子代理；父会话结束后看泳道；再强杀重开 | 泳道状态正确；重开不出现假中止 | `task` 工具并行起两个子代理（worker-1/worker-2），父会话汇总成功（实时 1 轮 3 步）；页头「2 个子智能体」目录显示两者均绿点已完成（14.8K tok/48s、24.6K tok/25s）；强杀重开后父会话仍已完成，无假中止（重开后计 1 轮 2 步，见 N6） | PASS | `shots/08-live-state.png`、`shots/08-subagent-lane.png`、`shots/08-subagent-lane-crop.png`、`shots/08-reopen.png`、`evidence/08-rollout.jsonl`、`evidence/08-events.jsonl` |
| 9 | 导出 | 对压缩过的会话导出 | 摘要不以用户发言出现；轮次与界面一致 | 对压缩过的「简单问候」导出 Markdown 与 Session ZIP：摘要均以 `assistant/message`（`turn:0`、`source:model`）出现，**未当作用户发言**；但导出仅 3 轮，界面「4 轮 8 步」，rollout 实际 7 个 `turn_end`（含 2 个 failed）——**三处轮次不一致** | FAIL（摘要呈现 PASS；轮次一致性 FAIL） | `evidence/09-export.md`、`evidence/09-session-log.zip`、`shots/09-export-session.png` |
| 10 | 旧档案重开 | 打开一份无 `TurnEnd`、无 `task_id` 的旧档案 | 显示「未知」，不伪造 `completed`/`interrupted` | 手工构造无 `turn_end`、无 `task_id` 的旧档案：回合内如实显示「本轮状态未知」，**但顶部回合过程控件同时显示「已完成，用时 17 小时 44 分 56 秒」**（未知结局被渲染成完成），即伪造 `completed` | FAIL | `evidence/10-legacy.jsonl`、`evidence/10-legacy-ui.txt`、`shots/10-legacy-open.png` |

## 机器已核（可复核，不需人工）

| 承诺 | 自动化证据 |
|---|---|
| 无结论的回合显示「状态未知」，不伪造中止 | `adapter.spec.ts`：`60s 无帧且宿主确认空闲：…对账没有结论时保持打开`、`重开档案按结局行收口：失败回合不会在回放里变成 completed` |
| 同一回合「实时」与「重开重建」同轮次同结局 | `adapter.spec.ts`：`同一用户回合：实时与重开重建的轮次和结局一致`；`宿主恢复对账（阶段 3）` 三条 |
| 恢复只追加一次结论 | `rollout::reconcile`：`reconcile_appends_indeterminate_exactly_once`、`concurrent_reconcile_appends_exactly_once` |
| 旧档照常读、不因缺字段报错 | `legacy_ending_counts_as_concluded_without_new_line`、`legacy_turn_end_projection_omits_new_fields`、`header-actions/session-events.spec.ts` 旧口径导出用例 |
| 压缩摘要不占回合、重开轮次 +0 | `adapter.spec.ts`：`压缩过的会话重开：摘要不占回合，轮次与实时路径一致、不补中止横幅` |
| step 与用户回合号分口径 | `adapter.spec.ts`：`step 游标只认采样迭代号：事件面 turn 不会漏进用户回合号` |
| 发布形态可启动 | CI `product-smoke`：release 构建 + CDP 冒烟（`43595461027`） |

## 本轮新问题（未修复，按严重度）

| 编号 | 严重度 | 问题 | 复现步骤 | 证据 |
|---|---|---|---|---|
| N1 | P0 | 压缩后模型请求持续失败，会话永久不可用：压缩把带 `tool_call_id` 的助手消息并入摘要后，provider 找不到对应 `function.name`，返回 500 `Cannot resolve tool name for tool_call_id='call_0e6f3061a37b43cf96c3ef09'`，重试 5 次仍失败，之后每次提问都失败 | 把 `token_budget` 调小（本次 5000）→ 让模型跑一条耗时/输出较长的工具（`Start-Sleep 90`）→ 回合结束后触发压缩（日志 `compressed_items=10`）→ 发任意一句「继续」 | `shots/07-session-broken-after-compaction.png`、`evidence/07-shaniu.log.2026-11-14.log`、`evidence/09-export.md`（`turn/end` 内完整错误） |
| N2 | P1 | 审批中强杀会丢失整回合：卡片出现时强杀重开，该回合与用户消息一起消失，rollout/events 均无记录（用户消息未在请求前落盘） | 触发 exec 审批 → 卡片未处理时任务管理器结束进程 → 重开 | `shots/02-approval-card.png`、`shots/02-reopen.png`、`evidence/02-rollout-after-kill.jsonl` |
| N3 | P1 | 轮次/步数三处不一致，且多次重开数字还会变：实时 4 轮 5 步 → 重开 2 轮 3 步 → 再重开 4 轮 8 步；同会话导出 3 轮、rollout 7 个 `turn_end` | 压缩过的会话反复重开观察顶部计数；再导出同一会话对比 | `shots/03-live-after-followup.png`、`shots/03-reopen.png`、`shots/09-export-session.png`、`evidence/03-rollout.jsonl` |
| N4 | P1 | 旧档案伪造「已完成」：无 `turn_end` 时回合内写「本轮状态未知」，顶部回合过程控件却写「已完成，用时 …」，同屏两种口径 | 造一份无 `turn_end`、无 `task_id` 的 rollout → 启动后打开 | `shots/10-legacy-open.png`、`evidence/10-legacy-ui.txt` |
| N5 | P2 | 压缩提示实时不显示：压缩发生时界面无提示，只在重开后以「上下文注入」条目出现 | 触发压缩后保持窗口观察，再重开对比 | `shots/03-live-after-followup.png`、`shots/03-reopen.png` |
| N6 | P2 | step 计数重开后减少：4→3、5→3、9→8、3→2，多次重开还在变 | 任意完成多步的会话强杀重开，对比重开前/后顶部「N 轮 M 步」 | `shots/04-test-passed.png`、`shots/08-live-state.png`、`shots/08-reopen.png` |
| N7 | P3 | `pnpm release:portable` 不认独立 `CARGO_TARGET_DIR`：打包脚本 `package-portable.ps1` 只读 `src-tauri\target\release\shaniu.exe`，会校验旧 exe 并失败 | 用独立 `CARGO_TARGET_DIR` 构建后直接跑 `pnpm release:portable` | 本次改为 `scripts/package-portable.ps1 -Exe scratch\target-smoke\release\shaniu.exe -OutDir scratch\smoke-dist` 完成打包 |

## 备注

- 已知偏差：旧档案的轮次统计可能与历史界面不同（历史数字把采样迭代计入了回合），按 §8-6 接受并在此注明。
- 测试噪音：见 `docs/17-测试卫生清单.md`（并发跑两份 `cargo test` 时的沙箱容器握手偶发失败，不涉及 CI）。
- 本轮测试配置：A 数据目录把 `[agent] token_budget` 由 500000 改为 5000 以触发压缩；其余配置（LongCat 提供商、工作区 `smoke-proj`）为隔离副本的预置项，未改产品默认值。所有运行均在 `scratch\` 下的临时数据目录，未触碰用户现有数据目录。
- 覆盖缺口：N3/N6 与「机器已核」中「同一回合实时与重开同轮次同结局」「压缩摘要重开轮次 +0」的自动化用例直接冲突——单测通过但真机可复现，说明现有用例未覆盖「多次重开 + 压缩后失败回合」组合，建议补真机用例（本轮按红线未修）。
- 脱敏：`evidence/` 内日志、事件、导出均已检查，无 `sk-` 密钥、`api_key`、`Bearer` 令牌。

---

# 第二轮复跑（2026-11-14 22:57–23:35，main=`95c1166`）

> 上一轮记录（6 PASS / 4 FAIL）原样保留；本轮在同一份报告追加。仍遵守红线：未改产品代码、未提交，只新增 `r2/` 下证据与本段结论。
> 十项本轮结论：**10 PASS**。

## 构建与运行环境

- **代码**：`main`=`95c1166`，包含 `d1e6b08`（压缩配对）、`57e90fb`（顶部状态口径）、`2d78912`（轮次/步数口径）、`1b0bc73`（提交即落盘）、`e52f9f8`（账本）、`95c1166`（基线校验加固）
- **构建**：独立 `CARGO_TARGET_DIR=scratch\target-smoke-r2`，`pnpm tauri build --no-bundle`（4 分 50 秒），再经 `scripts/package-portable.ps1 -Exe scratch\target-smoke-r2\release\shaniu.exe -OutDir scratch\smoke-dist-r2` 打包；`shaniu.exe` 28,562,944 B，SHA256 `506D4AD66A38852C8D52CD012A72FD243C9A093AC9CBC18AE9AA0E0B0ECBFDAA`（未复用旧二进制）
- **数据目录 A2**：`scratch\smoke-r2\SHANIU-Portable`（隔离副本，LongCat 提供商 + 工作区 `scratch\smoke-proj`；为触发压缩把 `[agent] token_budget` 设为 **5000**，测试配置，非产品默认）
- **数据目录 B2**：`scratch\smoke-r2-fresh\SHANIU-Portable`（全新，未启动过）
- **证据根**：`docs\smoke\2026-11-14-58f56f6\r2\`（`shots\` 截图、`evidence\` 脱敏档案）

## 逐项记录（本轮）

| # | 场景 | 实际（本轮） | 结论 | 证据 |
|---|---|---|---|---|
| 1 | 新建会话完成 + 强杀重开 | 实时「已完成，用时 1 秒」、1 轮 1 步；强杀重开后仍 `completed`、1 轮 1 步、无「已中止/状态未知」横幅；rollout 强杀前后文件哈希一致（`1967A4…C95`），未重复追加 `turn_end` | PASS | `r2/shots/01-live-completed.png`、`r2/shots/01-reopen.png`、`r2/evidence/01-rollout-before-kill.jsonl`、`r2/evidence/01-rollout-after-kill.jsonl` |
| 2 | 触发 exec 审批 + 挂起强杀 | 审批卡出现（`风险 execute：unified_exec（echo smoke-r2-approval）`、状态栏「1 个请求等你审批」）；挂起时强杀重开：**该条提问仍在**，顶部回合过程控件与回合内状态**两处均「本轮状态未知」**，未凭空消失，2 轮 2 步；rollout `turn_end turn=2 reason=indeterminate` | PASS | `r2/shots/02-approval-card.png`、`r2/shots/02-reopen.png`、`r2/evidence/02-rollout-after-kill.jsonl`、`r2/evidence/02-events-after-kill.jsonl`、`r2/evidence/07-rollout.jsonl` |
| 3 | 压缩后追问 + 多次重开 | 触发压缩（日志 `compressed_items=3`）后追问「继续，用一句话说明当前进度。」得到正常回答（「命令…已成功执行完毕，输出 line 1 到 line 2000 共 2000 行、无报错，无未决事项。」）；全程日志 `Cannot resolve tool name` 计数 **0**；实时第 2 轮后 2 轮 3 步、第 3 轮后 3 轮 5 步；重开 3 轮 5 步、再重开仍 3 轮 5 步（数值不再变化）；rollout 3 条 `turn_end`（iterations 2+1+2=5）；导出 3 轮，四处对得上 | PASS | `r2/shots/03-live-after-compaction.png`、`r2/shots/07-after-70s.png`、`r2/shots/03-reopen1.png`、`r2/shots/03-reopen2.png`、`r2/evidence/03-rollout-final.jsonl`、`r2/evidence/03-events-final.jsonl` |
| 4 | 小改动并跑测试 | 模型在 `src/sum.js` 新增 `subtract(a,b)`、在 `test/sum.test.js` 加 3 条断言；`pnpm test` 经审批执行，输出 `sum.test.js OK`、`[exit code: 0]`；「已完成，用时 2 分 49 秒」、1 轮 13 步，无假「结果未知」 | PASS | `r2/shots/04-tool-approval.png`、`r2/shots/04-test-passed.png`、`r2/evidence/04-rollout.jsonl`、`r2/evidence/04-events.jsonl` |
| 5 | 新数据目录首发 | B2 全新目录首发：会话树仅「默认工作区 / 未分组」，显示空状态「探索未至之境」，无「暂不可用」 | PASS | `r2/shots/05-fresh-data-dir.png` |
| 6 | 正常关窗走托盘 | 向主窗口发送 `WM_CLOSE`（等价点关闭）后进程存活、主窗口隐藏；UIAutomation 双击托盘图标唤回，会话仍为「已完成」1 轮 13 步，无中止横幅 | PASS | `r2/shots/06-before-close.png`、`r2/shots/06-closed-to-tray.png`、`r2/shots/06-tray-restore.png` |
| 7 | 长工具静默 > 60s | `Start-Sleep -Seconds 90` 静默到 **1 分 22 秒**时状态仍为「正在调用工具」，未变「已停止」；完成后「已完成，用时 2 分 6 秒」、3 轮 4 步；rollout `turn_end turn=3 iterations=2` | PASS | `r2/shots/07-silent-70s.png`、`r2/shots/07-after-90s.png`、`r2/evidence/07-rollout.jsonl`、`r2/evidence/07-events.jsonl` |
| 8 | 子代理泳道 | `task` 并行起两个子代理；泳道两条均绿点「已完成」（worker-1 9.5K tok/41 秒、worker-2 14.4K tok/1 分 23 秒）；父会话汇总「已完成，用时 1 分 43 秒」1 轮 3 步；强杀重开后仍 1 轮 3 步、泳道条目保留、出现两条 `subagent-settled` 上下文注入，无假中止 | PASS | `r2/shots/08-live-done.png`、`r2/shots/08-subagent-lane.png`、`r2/shots/08-reopen.png`、`r2/evidence/08-rollout.jsonl`、`r2/evidence/08-events.jsonl` |
| 9 | 导出 | 对压缩过的会话（3 轮）导出：Session ZIP 内 `session.v4.jsonl` 为 `turn/start=3`、`turn/end=3`、`user/message=3`；Markdown 恰 3 条用户发言，摘要未以用户发言出现；与界面 3 轮 5 步、rollout 3 条 `turn_end` 一致 | PASS | `r2/evidence/09-session-log.zip`、`r2/evidence/09-export.md`、`r2/shots/09-export-session.png` |
| 10 | 旧档案重开 | B2 打开无 `turn_end`、无 `task_id` 的 `sess-legacy-r2-0001`：顶部回合过程控件与回合内状态**两处均「本轮状态未知」**，附「没有找到本轮的收口记录…」，未出现「已完成，用时 X 小时」，1 轮 1 步 | PASS | `r2/evidence/10-legacy.jsonl`、`r2/evidence/10-legacy-ui.txt`、`r2/shots/10-legacy-open.png` |

## 四项重点复验

1. **压缩 P0（→ 第 3 项）PASS**：压缩后继续提问得到正常回答，日志 `Cannot resolve tool name` 全程 0 次，重试失败未再现。
2. **旧档重开（→ 第 10 项）PASS**：顶部与回合内口径一致，均为「本轮状态未知」，无伪「已完成，用时 X 小时」。
3. **轮次/步数（→ 第 3、9 项）PASS**：实时 3 轮 5 步、重开 3 轮 5 步、再重开 3 轮 5 步、导出 3 轮、rollout 3 条 `turn_end`（2+1+2=5），重复重开数值不变。
4. **审批卡挂起强杀（→ 第 2 项）PASS**：提问与回合保留，两处「本轮状态未知」，rollout 写入 `turn_end turn=2 reason=indeterminate`。

## 上一轮新问题在本轮的状态

| 编号 | 状态 | 本轮依据 |
|---|---|---|
| N1（P0 压缩后永久失败） | 已修复 | 第 3 项压缩后追问正常；日志 `Cannot resolve tool name` 计数 0 |
| N2（P1 审批中强杀丢回合） | 已修复 | 第 2 项提问仍在，`turn_end reason=indeterminate` |
| N3（P1 轮次/步数三处不一致） | 已修复 | 第 3、9 项实时/重开/再重开/导出/rollout 全部对齐 |
| N4（P1 旧档伪「已完成」） | 已修复 | 第 10 项两处均为「本轮状态未知」 |
| N5（P2 压缩提示实时不显示） | 未复验 | 本轮未设置实时观察压缩提示的检查点，不改变上一轮结论 |
| N6（P2 step 重开减少） | 未再现 | 第 3 项两次重开 3 轮 5 步不变；第 8 项重开 1 轮 3 步 |
| N7（P3 打包脚本不认 `CARGO_TARGET_DIR`） | 仍存在 | 本轮仍分两步：`tauri build` + `package-portable.ps1 -Exe …` |

## 本轮新问题（未修复）

| 编号 | 严重度 | 问题 | 复现步骤 | 证据 |
|---|---|---|---|---|
| N8 | P2 | 连续压缩时摘要自嵌套、向空转退化：单回合内发生 12 次压缩，摘要里「## 用户原文（压缩时逐字保留，不得改写）」最多嵌套 **11 层**，`history_len` 由 5 增至 28，摘要体积持续膨胀（小改动回合因此用到 92.9K tok、13 步） | 把 `[agent] token_budget` 设为 5000 → 让模型做一个多步小改动并跑测试 → 观察日志 `compressed_items=1` 反复出现、rollout 内 `compacted` 行层层嵌套 | `r2/evidence/04-rollout.jsonl`、日志 `T15:13–15:16` 共 12 条 `上下文已压缩`、`r2/shots/04-test-passed.png` |

### N8 修复复验（2026-11-15 00:06–00:30，被测提交 `93eb2bf`）

> 被测修复：`93eb2bf fix(compaction): 连续压缩不再把旧摘要当用户原文嵌套（真机 N8）`。本次在 `ceffc67` 上重建（该提交仅归档第一轮证据，代码同 `93eb2bf`），未复用旧二进制，按 N8 原复现条件执行。

**构建与运行**

- 构建：`CARGO_TARGET_DIR=scratch\target-n8` → `pnpm tauri build --no-bundle`（5 分 17 秒）；`SHANIU.exe` 28,563,456 B，SHA256 `84ACEF225ED82330E28A9317F2CDDFE1CA8C60AB3A3B3B699AB6E0497CB63156`。相对 `CARGO_TARGET_DIR` 被 tauri 解析到 `src-tauri\scratch\target-n8`，与 N7 同类。
- 数据目录 C（小预算）：`scratch\smoke-n8\SHANIU-Portable`，`[agent] token_budget=5000`，LongCat + 工作区 `scratch\smoke-proj`，全新会话；会话 `sess-1794672587958-0002`。
- 数据目录 D（常规预算）：`scratch\smoke-n8-default\SHANIU-Portable`，`token_budget=500000`，全新会话；会话 `sess-1794673459052-0002`。

**逐条结论（对应任务四项）**

| # | 检查点 | 实测 | 结论 | 证据 |
|---|---|---|---|---|
| 1 | 单回合压缩次数、摘要嵌套 | 该回合 `上下文已压缩` **3 条**（修复前 12 条），`compressed_items`=1/10/5，`history_len`=9/5/8（修复前 5→28）；三条摘要中 `## 用户原文` 标题各出现 **1 次**，无层层嵌套（修复前最深 11 层）；但该段首位是上一次摘要正文（残留见 N9） | PASS（次数与嵌套层数），残留见 N9 | `r3/evidence/n8-log-excerpt.txt`、`r3/evidence/n8-compacted-summaries.txt` |
| 2 | 该回合 token 与步数 | 实时「1 轮 6 步」、rollout `turn_end turn=1 iterations=6 reason=completed`；UI「用量 50.6K tok」，日志 6 次调用 input 合计 49,467 + output 1,174 = 50,641（修复前 13 步 / 92.9K tok） | PASS | `r3/shots/01-n8-turn-done.png`、`r3/evidence/n8-rollout.jsonl`、`r3/evidence/n8-log-excerpt.txt` |
| 3 | 常规预算不受影响 | `token_budget=500000` 新会话正常完成（1 轮 2 步、10.3K tok、4 秒），全日志无 `上下文已压缩`；强杀重开仍 1 轮 2 步、回答保留；rollout 1 条 `turn_end`。默认预算在冒烟中无法填满 50 万 token，故「压缩仍会触发」由上一行小预算会话的 3 次压缩证明，本行验证的是「不产生多余压缩、问答正常、计数一致」 | PASS（含口径说明） | `r3/shots/04-default-budget-live.png`、`r3/shots/05-default-budget-reopen.png`、`r3/evidence/n8-default-budget-rollout.jsonl`、`r3/evidence/n8-default-budget-log-excerpt.txt` |
| 4 | 压缩后追问 | 「继续，用一句话说明当前进度。」4 秒得到正常回答（「当前进度：五步任务已全部完成…」）；全程 `Cannot resolve tool name` 计数 **0** | PASS | `r3/shots/02-n8-followup.png`、`r3/evidence/n8-log-excerpt.txt` |

**轮次/步数四处一致（小预算会话，2 轮）**

- 实时 2 轮 7 步（6+1）→ 强杀重开 2 轮 7 步 → 导出 `session.v4.jsonl` 为 `turn/start=2`、`turn/end=2`、`user/message=2` → rollout 2 条 `turn_end`（iterations 6+1=7）。四处一致。

**残留问题**

| 编号 | 严重度 | 问题 | 复现步骤 / 时间点 | 证据 |
|---|---|---|---|---|
| N9 | P2 | 「## 用户原文（压缩时逐字保留，不得改写）」段首仍是上一次压缩摘要（模型生成内容），不是用户原文：第 2、3 次摘要的该段依次包含第 1、2 次摘要正文，之后才是用户原话。层数已从 11 层降到 1 层，但「摘要在用户原文段内自重」仍在。触发源是首次空转压缩：`2026-11-14T16:10:31.117818Z` `compressed_items=1`，摘要自述「压缩对象为空集…本轮未收到具体用户任务」，该空摘要随后被当作用户原文保留 | 新建数据目录 + `token_budget=5000` → 一条多步任务（大输出命令 + 改文件 + 跑测试）→ 看 3 条 `compacted.summary` 的 `## 用户原文` 段首 | `r3/evidence/n8-compacted-summaries.txt`（第 10–18、29–37 行）、`r3/evidence/n8-rollout.jsonl`（第 12/18/26 行 `compacted`）、`r3/evidence/n8-log-excerpt.txt` |

**顺带观察（未定性，P3）**

- 同一回合时长重开后由「7 分 15 秒」变为「2 分 42 秒」，追问回合 4 秒 → 3 秒；轮次/步数不变。疑为重开后按回合内步骤耗时重算、不含审批等待，需单独立项确认，未计入 N8 结论。证据 `r3/shots/01-n8-turn-done.png`、`r3/shots/02-n8-followup.png`、`r3/shots/03-n8-reopen.png`。

**证据索引（`r3/`）**

- 截图：`01-n8-turn-done.png`（回合完成，1 轮 6 步 / 50.6K tok）、`02-n8-followup.png`（追问后 2 轮 7 步）、`03-n8-reopen.png`（强杀重开 2 轮 7 步）、`04-default-budget-live.png`、`05-default-budget-reopen.png`
- 档案：`n8-rollout.jsonl`、`n8-events.jsonl`、`n8-log-excerpt.txt`、`n8-compacted-summaries.txt`、`n8-followup-session-log.zip`（导出，2 轮）、`n8-default-budget-rollout.jsonl`、`n8-default-budget-log-excerpt.txt`
- 全部运行在 `scratch\` 隔离目录，未改产品代码（`multiply` 在测试工程中已存在，模型仅补断言并改的是 `scratch\smoke-proj`）。

## 本轮证据索引（`r2/`）

- 截图（`r2/shots/`）：`01-live-completed.png`、`01-reopen.png`、`02-approval-card.png`、`02-reopen.png`、`03-live-after-compaction.png`、`03-reopen1.png`、`03-reopen2.png`、`04-tool-approval.png`、`04-test-passed.png`、`05-fresh-data-dir.png`、`06-before-close.png`、`06-closed-to-tray.png`、`06-tray-restore.png`、`07-silent-40s.png`、`07-silent-70s.png`、`07-after-70s.png`、`07-after-90s.png`、`08-live.png`、`08-live-done.png`、`08-subagent-lane.png`、`08-reopen.png`、`09-export-session.png`、`10-legacy-open.png`
- 档案（`r2/evidence/`）：`01-rollout-before-kill.jsonl`、`01-rollout-after-kill.jsonl`、`02-rollout-after-kill.jsonl`、`02-events-after-kill.jsonl`、`03-rollout-live.jsonl`（第 2 轮时点）、`03-rollout-final.jsonl`（3 轮终态）、`03-events-live.jsonl`、`03-events-final.jsonl`、`04-rollout.jsonl`、`04-events.jsonl`、`07-rollout.jsonl`、`07-events.jsonl`、`08-rollout.jsonl`、`08-events.jsonl`、`09-session-log.zip`、`09-export.md`、`10-legacy.jsonl`、`10-legacy-ui.txt`
- 日志：`scratch\smoke-r2\SHANIU-Portable\data\logs\shaniu.log.2026-11-14`（本轮未复制进证据，避免与上一轮同名覆盖；关键行已摘入本段）

## 备注（本轮）

- 第 2 项执行时「新建会话」点击未生效，该提问落在既有「打招呼」会话；不影响本项结论，该会话在第 7 项又追加了 `Start-Sleep 90` 一轮。
- 第 4 项会话受 `token_budget=5000` 影响发生 12 次压缩，1 轮 13 步/92.9K tok 属测试配置放大，不能当作产品默认性能。
- 第 8 项子代理不单独落 rollout 文件，证据为父会话档案 + 泳道截图 + `events` 中两条 `subagent-settled` 注入。
- 第 9 项使用最终 3 轮状态重新导出，覆盖了本轮早先的 2 轮导出文件（`09-session-log.zip`、`09-export.md`）。
- 第 6 项关闭方式为向主窗口发送 `WM_CLOSE`（等价点关闭），未强杀进程。
- 第 5、10 项在 B2 全新数据目录执行；旧档案 `sess-legacy-r2-0001` 无 `turn_end`、无 `task_id`。
- 全部运行均在 `scratch\` 下的隔离数据目录，未触碰用户现有数据目录。

## 第四轮真机复验：N5 实时压缩节点 + N9 残留（2026-11-15 01:20–02:00，被测提交 `9d0cfd9`）

被测二进制：现场重建（`CARGO_TARGET_DIR=scratch\target-n5r4` + `pnpm tauri build --no-bundle`，5 分 14 秒），
`git rev-parse HEAD` = `origin/main` = `9d0cfd9`，SHA256 `147C500E…CD06`，大小 28,563,456 B。
数据目录 `scratch\smoke-r4`（`token_budget=5000`）、`scratch\smoke-r4-auto`（`token_budget=100`）、`scratch\smoke-r4-fail`（同 100，`base_url=http://127.0.0.1:9/v1` 注入失败）。未改产品代码。

| 项 | 结论 | 现象与判据 | 证据 |
|---|---|---|---|
| A-1 实时压缩节点 | **FAIL** | 01:58:49 自动压缩（`compressed_items=3 history_len=2`）后立刻取现场：UIA 全树里「压缩 / compact / 用户原文」关键字命中 **0**，会话区没被替换，既没有「上下文已压缩」卡片也没有摘要正文；上一轮用户气泡、思考行、底部「深度求索中，用时 14 秒」原样保留 | `r4/shots/04-live-auto-compaction-no-node.jpg`、`r4/evidence/r4-compaction-log.txt`、`r4/evidence/r4-ui-observations.txt` |
| A-2 强杀重开唯一性/一致性 | **FAIL（不一致）** | 重开后同一次压缩只出现 **1 条**（不重复，这一点达标），但它是 `compaction-notice` 兜底通知（「上下文已压缩：历史第 1–3 条被一条摘要取代；原文仍保留在会话档案里。」），不是节点；实时 0 条 vs 重开 1 条，**呈现不一致**，也不符合「形状与上下文注入同口径」 | `r4/shots/06-reopen-after-kill-notice.jpg`、`r4/evidence/r4-ui-observations.txt` |
| A-3 手动 `/compact` | **FAIL** | 发送后先出现命令卡片 `> compact · 正在压缩…`（单条）；压缩完成后命令卡片消失，会话区被**原始摘要正文**占据（以「## 用户原文（压缩时逐字保留，不得改写）」开头，含「1. 任务目标 … 5. 下一步」，时间 01:51，带「复制」），全树没有「上下文已压缩」节点 | `r4/shots/02-manual-compact-running.jpg`、`r4/shots/03-manual-compact-raw-summary.jpg`、`r4/evidence/r4-rollout-analysis.txt`（compacted #4） |
| A-4 失败压缩通知 | **PASS** | 注入 `base_url=127.0.0.1:9` 后自动压缩失败（`compaction_ended{ok:false,error:"压缩请求发起失败，本轮中止：无法连接到模型服务"}`），界面出现「处理失败」+ 通知行「上下文压缩未生效：压缩请求发起失败，本轮中止：无法连接到模型服务。历史保持原样，可以继续对话。」 | `r4/shots/05-failed-compaction-notice.jpg`、`r4/evidence/r4-events-compaction.txt` |
| B N9 残留 | **PASS** | 4 条 `compacted`（range 0-12 / 0-3 / 0-32 / 0-3），每条「## 用户原文」段长度 225/225/338/338，内容分别是第 1、3 条真实用户发言（逐字），段内**没有**任何模型生成的摘要分节（`1. 任务目标` 等只出现在 `---` 之后）；`history_len` = 3 / 8 / 3 / 2，有界 | `r4/evidence/r4-compacted-usertext.txt`、`r4/evidence/r4-rollout-analysis.txt` |
| C 回归 | **PASS** | 实时 3 轮 14 步 = 重开 3 轮 14 步 = 导出 `turn/start` 3 / `turn/end` 3 = rollout `turn_end` 3 条（iterations 11+1+2=14）；压缩后追问「继续，用一句话说明当前进度。」5 秒内正常回答；日志扫描 `Cannot resolve tool name` 命中 0 | `r4/shots/01-live-turn-done.png`、`r4/evidence/r4-export-counts.txt`、`r4/evidence/r4-rollout-analysis.txt`、`r4/evidence/r4-toolname-scan.txt` |

**B 的补充（压缩次数）**：14 步共压缩 4 次（3 次自动 + 1 次手动），约每 3–4 步一次，仍随步数增长；
但每次压缩后 `history_len` 落在 2–8，没有无界膨胀。`compacted` 时间点与摘要原文见 `r4-compacted-usertext.txt`
（原文照抄，未删改）。

**A 的定位线索（黑盒只能到这，照实写）**

- 实时压缩后既无节点也无替换，最接近的代码分支是 `pushCompactionCheckpoint` 在 `journal.surfaceSeqs` 为空时**静默返回 undefined**（[adapter.ts](file:///d:/MyWorkspace/SHANIU/src/transport/adapter.ts) 约 L1005–L1020）。但本轮无法证明是「surface 未登记」还是「替换事件被消费端丢弃」，需要打点复现。
- 手动 `/compact` 完成后出现原始摘要正文，形态与「重开时首条即摘要」的重放路径一致（[adapter.ts](file:///d:/MyWorkspace/SHANIU/src/transport/adapter.ts) 约 L1890–L1990：首条 `compaction_summary` 之前没有 surface → 落 notice；有 surface → 走 checkpoint）。手动压缩为何把摘要铺成普通消息，与本轮证据吻合但未逐行证实。
- 顺带发现的不对称（当前不影响判据）：`EventMsg::CompactionEnded` 不带 `trigger`，而 `compaction_started` / `compaction_summarized` 都带（[events.rs](file:///d:/MyWorkspace/SHANIU/src-tauri/src/events.rs) 的 `CompactionEnded` 分支）。适配层目前从 trace 取 trigger，所以没炸；但协议上建议 additive 补齐，否则以后任何新增消费方都会踩空。

**顺带观察（未定性）**

- 导出 `session.v4.jsonl`（53 行）里 `compaction` 事件为 **0**，而宿主 `data\sessions\*.events.jsonl` 有 9 条 compaction 事件。导出链路把压缩事件丢了；本轮四处计数不依赖它，但导入/回放对账会缺证据。
- 手动压缩不写 `上下文已压缩（SH-032）` 日志行（该行只在自动压缩路径出现），排障时别拿它当手动压缩的判据。

**证据索引（`r4/`）**

- 截图（`r4/shots/`）：`01-live-turn-done.png`、`02-manual-compact-running.jpg`、`03-manual-compact-raw-summary.jpg`、`04-live-auto-compaction-no-node.jpg`、`05-failed-compaction-notice.jpg`、`06-reopen-after-kill-notice.jpg`、`live-compaction-1.png`、`live-compaction-2.png`、`live-compaction3-3.png`
- 档案（`r4/evidence/`）：`r4-build.txt`、`r4-compaction-log.txt`、`r4-rollout-analysis.txt`、`r4-compacted-usertext.txt`、`r4-events-compaction.txt`、`r4-export-counts.txt`、`r4-toolname-scan.txt`、`r4-ui-observations.txt`、`live-export-session-log.zip`

## 第五轮真机复验：N5 返工（2026-11-15 03:54–04:45，被测提交 `408dee9`）

被测二进制：现场重建，未复用旧包（`CARGO_TARGET_DIR=scratch\target-r5` 全新目录 + `pnpm tauri build --no-bundle`，339.7 秒），
`git rev-parse HEAD` = `origin/main` = `408dee9`（含 `c6a89be`），产物 28,564,480 B，SHA256 `775C9851…9BC2`（详见 `r5/evidence/r5-build.txt`）。
数据目录：`scratch\smoke-r5-auto`（`token_budget=100`，自动压缩）、`scratch\smoke-r5-fail`（同 100 + `base_url=http://127.0.0.1:9/v1`）、`scratch\smoke-r5-default`（`token_budget=500000`）。未改产品代码。

| 项 | 结论 | 现象与判据 | 证据 |
|---|---|---|---|
| A1 实时压缩节点 | **PASS（带条件）** | 04:15:47.041 自动压缩（`compressed_items=4`）；04:15:54 立刻取 UIA 全树：**0 条**「上下文已压缩」，也没有任何 notice —— 节点此时被折叠在「已完成，用时 9秒」→「已完成分析」两级组内。04:16:35 用真实鼠标展开两级后取全树：**1 条** `button "上下文已压缩 点击查看压缩摘要"`（两棵重复文档各映射一次，unique=1），点击展开可见完整摘要（`## 用户原文（压缩时逐字保留，不得改写）` → `1. 任务目标`…`5. 下一步`，319 字符，与档案 summary 一致）。会话区已被替换（原消息收进组内，正文显示摘要）。**注**：若按「不展开也必须全树可见」的字面口径，本项仍算不达标，见残留 R5-1 | `r5/shots/a1-live-collapsed-tree.png`、`a1-expand-turn-group.png`、`a1-expand-analysis-group.png`、`a1-node-summary-expanded.png`、`r5/evidence/r5-ui-observations.txt` |
| A2 强杀重开 | **PASS** | 强杀后重开，同一次压缩仍只有 **1 条**，且形状与实时**同形**：`button "上下文已压缩 点击查看压缩摘要"`，**不是** notice（r4 的 `compaction-notice` 兜底文案 0 命中）。2 轮 2 步与实时一致 | `r5/shots/a2-reopen-1.png`…`a2-reopen-4-node.png` |
| A3 手动 `/compact` | **PASS** | 命令卡完成后**仍是命令卡**：`button "compact Compacted 2 history items (~200 tokens)."`（val=collapsed）；会话区 `## 用户原文` 命中 **0**（不再被原始摘要正文占据）；压缩节点仍 **1 条**，未出现两条。日志 04:18:49.695096Z `trigger="manual"` | `r5/shots/a3-command-menu.png`、`a3-after-compact-card.png`、`r5/evidence/r5-log-compaction.txt` |
| A4 失败压缩 | **PASS** | 注入不可达 provider 后，本轮以失败区块呈现「处理失败 / 压缩请求发起失败，本轮中止：无法连接到模型服务」（`COMPACTION_FAILED` + 「原始错误 ▶」），**没有**压缩节点、没有摘要正文；`compacted` 仍 2 条（range 0-4 ×2），历史原样 | `r5/shots/a4-fail-1-start.png`、`a4-fail-2-baseline-3turns.png`、`a4-fail-3-notice.png`、`r5/evidence/r5-rollout-analysis.txt` |
| B 回归 | **PASS** | 四处一致：实时 **4 轮 5 步** = 重开 **4 轮 5 步** = 导出 `turn/start` 4 / `turn/end` 4 = rollout `turn_end` 4 条（iterations 1+1+1+2=5）。压缩后追问正常完成，轮内 `read_file` 成功读到 `smoke-proj`。`Cannot resolve tool name` 四路扫描 **0**。常规预算（500000）同一回合 23 条 response_item、多次工具调用，`SH-032` 0 行、`compacted` 0 条 | `r5/shots/b-followup-toolcall-4turns.png`、`b-reopen-counts-4turns.png`、`b-default-budget-1.png`、`b-default-budget-2.png`、`r5/evidence/r5-toolname-scan.txt`、`r5-export-counts.txt` |
| C 导出与日志 | **PASS** | 导出 `session.v4.jsonl` 36 行：`compaction/start|summary|end` 各 **5** 条（每次压缩 3 条 log-only 事件），summary 非空（319/194/177/208/208 字符）；`turn/start` 4、`turn/end` 4，**未虚增回合**。手动压缩有 SH-032 行（04:18:49.695096Z，`trigger="manual"`） | `r5/shots/c-session-menu.png`、`c-save-dialog.png`、`c-export-toast.png`、`r5/evidence/r5-export-counts.txt` |

**残留（照实报，均不改变上面 PASS 判据）**

- **R5-1（A1 可见性，P2）**：压缩节点默认藏在「轮次过程」(`已完成，用时 N秒`) → `已完成分析` 两级折叠组内；不展开时 UIA 全树与界面上都看不到「上下文已压缩」。所以「自动压缩后立刻取全树即可见」的字面要求未满足——本轮 PASS 判的是"形态已从通知变成节点且含摘要"。
  - 最小复现：`token_budget=100` 的会话发任意一句 → 等日志出现 `SH-032` → 不点任何折叠组直接取全树，命中 0；点开两级后命中 1。
  - 日志时间点：`2026-11-14T20:15:47.041068Z`（压缩）→ `20:15:54`（折叠态取样 0 条）→ `20:16:35`（展开后 1 条）。
- **R5-2（A2 位置与时间，P3）**：实时节点挂在当轮组内，重开挂在压缩 range 首轮组内；重开视图出现 `text "记录时刻不可用"`（重放缺时间戳）。
- **R5-3（A3 文案，P3）**：`/compact` 命令卡完成后文案为英文 `Compacted 2 history items (~200 tokens).`，未本地化。
- **R5-4（C 导出自描述不全，P2）**：导出里的压缩事件 `turn=null`、`time=0`、`timeUnreliable=true`，且**没有 trigger 字段**——能看到 3 条 log-only 事件和摘要，但无法从导出区分手动/自动、也无法归到具体回合。`c6a89be` 的自描述只补在了宿主侧事件，导出合成分支没带上。
  - 最小复现：任一有压缩的会话 → 「···」→「下载 Session 日志」→ 看 `session.v4.jsonl` 的 `compaction/*`。
- **R5-5（A4 文案与可观测性，P3）**：失败区块标题为「本本轮运行失败」（重复一个「本」）；且压缩失败在日志里 **0 行**，只有 UI 与 rollout `turn_end.error` 有痕迹，排障得翻档案。
- **R5-6（测试环境，非产品问题）**：`token_budget=100` 时每个步骤都触发压缩（04:27:34 / 04:28:08 / 04:28:16 各一次），4 轮产生了 5 个压缩节点，属测试配置放大；常规预算（500000）不产生任何压缩，两侧对照成立。
- 另记：本轮 `smoke-r5-default` 的会话在观察窗口内一直停在「等待审批」（模型连续请求 `unified_exec`），随后被强杀，因此没有 `turn_end`；「常规预算不产生多余压缩」的结论只依赖 `SH-032=0` 与 `compacted=0`，与这个未完成回合无关。

**证据索引（`r5/`）**

- 构建：`r5/evidence/r5-build.txt`
- 截图（`r5/shots/`）：`a1-live-collapsed-tree.png`、`a1-expand-turn-group.png`、`a1-expand-analysis-group.png`、`a1-node-summary-expanded.png`、`a2-reopen-1..3.png`、`a2-reopen-4-node.png`、`a3-command-menu.png`、`a3-after-compact-card.png`、`a4-fail-1-start.png`、`a4-fail-2-baseline-3turns.png`、`a4-fail-3-notice.png`、`b-followup-turn3.png`、`b-followup-toolcall-4turns.png`、`b-reopen-counts-4turns.png`、`b-reopen-node.png`、`b-default-budget-1.png`、`b-default-budget-2.png`、`c-session-menu.png`、`c-save-dialog.png`、`c-export-toast.png`
- 档案（`r5/evidence/`）：`r5-build.txt`、`r5-ui-observations.txt`、`r5-log-compaction.txt`、`r5-rollout-analysis.txt`、`r5-export-counts.txt`、`r5-toolname-scan.txt`、`r5-session.v4.jsonl`、`dsh-session-sess-1794686409659-0001.zip`
- 全部运行在 `scratch\` 隔离目录，未改产品代码。
