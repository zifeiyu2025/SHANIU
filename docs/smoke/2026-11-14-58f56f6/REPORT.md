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
