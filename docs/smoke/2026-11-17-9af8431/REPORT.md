# SHANIU 0.2.9 真机 GUI 冒烟（被测提交 `9af8431`）

- 时间：2026-11-17 02:40–03:40（本地，UTC+8）
- 被测二进制：`dist-release\SHANIU-Portable-0.2.9\SHANIU.exe`
  - 大小 28,724,224 B，SHA256 `DF7FB6336FB764673F0C3F57ACBA6CD219782C2F3BCBDCCF860D1654848D175F`
  - `FileVersion` = `ProductVersion` = `0.2.9`
- 被测提交：`9af8431aabf5456287e79a305360cfa494fe177f`（= `origin/main`）
- 未改产品代码，未改判据；本轮只新增 `docs/smoke/2026-11-17-9af8431/`。
- 清单与判据沿用 [2026-11-14-58f56f6/REPORT.md](file:///d:/MyWorkspace/SHANIU/docs/smoke/2026-11-14-58f56f6/REPORT.md) 的十项与 r2–r6 口径。

## 运行环境与方法

| 数据根 | 用途 | 关键配置 |
|---|---|---|
| `root-a` | 十项 1/2/4/7/8 与复验 1/4/5 | `approval=on_request`、`token_budget=500000`、沙箱 `workspace_write`、工作区 `proj`、CDP 9444 |
| `root-b` | 复验 1（full_access）、十项 5/6/10 | `approval=full_access`、`token_budget=500000`、工作区 `proj-b`、CDP 9446 |
| `root-c` | 十项 3/9、复验 3/6 | `token_budget=5000`（强制压缩）、工作区 `proj-c`、CDP 9445 |
| `root-d` | 复验 2（截断） | `base_url=http://127.0.0.1:9447/v1`（本地代理注入 `max_tokens=200`）、CDP 9448 |

- 全部数据根用 `SHANIU_DATA_DIR` 隔离，测试工程在 `scratch\smoke-1117\proj*`，未触碰用户数据目录。
- 驱动：直接运行上述 exe（未重新构建），CDP 连 WebView2 做真实点击/输入/滚动/截图。
- 工具卡正文随滚动虚拟化卸载，正文取证采用「先滚到卡片、再读 DOM」同一调用内完成。
- 截断注入：本地 Node 反向代理 9447 把请求体 `max_tokens` 改为 200，`/v1/*` 转发到上游 `/openai/v1/*`。

## 十项总表

**PASS 8 / FAIL 2 / BLOCKED 0**

| # | 场景 | 预期（本方案承诺） | 实际 | 结论 | 证据 |
|---|---|---|---|---|---|
| 1 | 新建会话完成 + 强杀重开 | 1 轮 1 步、单条结束行、无横幅；重开仍 `completed` | 实时「已完成」1 轮 1 步，单条 `turn_end`；强杀重开后仍 `completed`、1 轮 1 步、无中止横幅；强杀前后档案 SHA256 一致 `CBB0335E…B74F3E`（未重复追加） | PASS | `evidence/01-rollout-{before,after}-kill.jsonl`、`shots/01-live-completed.png`、`shots/01-reopen.png` |
| 2 | 触发 exec 审批 + 挂起强杀 | 卡片出现；重开后仍保留并显示「确认中」，不自动撤 | 卡片正常出现（`风险 execute：unified_exec（echo smoke-approval-096）`、「1 个请求等你审批」、拒绝/允许一次）；挂起强杀重开后**提问仍在**（用户消息已落盘，r1 的丢回合已修）、命令未重复执行（`dup.txt` 仍 1 行），但**审批卡不保留**，结局写 `reason=interrupted`、界面「已停止」+「任务已停止」，且步数 2→3 | **FAIL** | `shots/02-approval-card.png`、`shots/02-reopen-after-kill.png`、`evidence/02-rollout-{before,after}-kill.jsonl`、`evidence/02-events-after-kill.jsonl` |
| 3 | 压缩后追问 + 重开 | 摘要不占回合、无中止横幅；重开轮次与实时一致 | 确实触发压缩（`SH-032 compressed_items=2` / `compressed_items=7`）并做确定性裁剪（`pruned_items=1 chars_before=18955 chars_after=5181`）；追问「继续，用一句话说明当前进度」正常作答；摘要未当用户发言、无中止横幅；重开 3 轮 5 步 = rollout `iterations 2+2+1=5`。**但压缩发生的实时窗口里会话区整块空白、输入框消失，压缩节点不实时出现（要重开才有）** | **FAIL** | `shots/03-compaction-live-blank.png`、`shots/03-reopen-compaction-node.png`、`shots/03-reopen-final-3turns.png`、`evidence/03-rollout-compaction.jsonl`、`evidence/03-prune-compaction.txt` |
| 4 | 小改动并跑测试 | 工具卡、验证通知正常；不出现假「结果未知」 | `apply_patch` 改 `src/sum.js` 为箭头函数（「补丁应用成功（1 处）」），`npm test` 经审批执行 `sum.test.js OK` / `[exit code: 0]`，1 轮 6 步，无「结果未知」 | PASS | `evidence/04-rollout-modify-sum.jsonl`（第 1 回合）、`shots/04-approval-npm.png` |
| 5 | 新数据目录首发 | 空状态，不是「暂不可用」 | 全新 `root-b` 首发：会话树仅工作区项，主区「探索未至之境」空态，无「暂不可用」 | PASS | `shots/05-fresh-data-root.png` |
| 6 | 正常关窗走托盘 | 不出现中止横幅；托盘可唤回 | `WM_CLOSE` 后 `Tauri Window` `visible=False`、进程存活；托盘项「SHANIU 仍在后台运行 · 右键退出」唤回，会话仍「已完成」 | PASS | `evidence/06-tray-enum.txt`、`shots/06-before-close.png`、`shots/06-after-close.png`、`shots/06-restored-from-tray.png` |
| 7 | 长工具静默 > 60s | 静默期间不自动变「已停止」 | `Start-Sleep 90` 静默到 1 分 05 秒仍显示「正在调用工具」；完成后 `[exit code: 0]`、该回合 2 步 `completed`（第 1 次因审批被拒未执行，第 2 次放行后完成） | PASS | `shots/07-silent-30s.png`、`shots/07-silent-65s.png`、`evidence/04-rollout-modify-sum.jsonl`（第 7 回合） |
| 8 | 子代理泳道 | 泳道状态正确；父会话汇总正常 | `spawn_agent(profile=explorer)` → `wait_agent` 返回 `state=done` 与结论文本；父会话「已完成」1 轮 3 步，页头「1 个子智能体」，无假中止 | PASS | `shots/08-subagent-live.png`、`shots/08-subagent-done.png`、`shots/08-subagent-text.png`、`evidence/04-rollout-modify-sum.jsonl`（第 5 回合） |
| 9 | 导出 | 摘要不以用户发言出现；轮次与界面一致 | 真实 UI 导出 ZIP：`turn/start=3`、`turn/end=3`、`user/message=3`、`assistant/message=4`、`compaction/*` 各 2；Markdown 用户发言恰 3 条，摘要未冒充用户。轮次与界面 3 轮一致（**步数见复验 6：导出不带步数**） | PASS | `evidence/03-session.v4.jsonl`、`evidence/03-session-log.zip`、`evidence/03-export.md`、`shots/09-export-click.png` |
| 10 | 旧档案重开 | 显示「未知」，不伪造 `completed`/`interrupted` | 手造无 `turn_end`、无 `task_id` 的档案：顶部回合控件与回合内**两处均「本轮状态未知」**，附「没有找到本轮的收口记录…」，1 轮 1 步，无「已完成」 | PASS | `evidence/10-legacy-rollout.jsonl`、`shots/10-legacy-list.png`、`shots/10-legacy-open.png` |

## 六项重点复验

**PASS 3 / FAIL 3 / BLOCKED 0**

| 项 | 结论 | 现象与判据 | 证据 |
|---|---|---|---|
| F1 审批档如实进提示词 | **PASS** | on_request 系统提示逐字含「审批档为 on-request：写入与命令执行可能在执行前弹审批；被拒绝时说明原因并征求指示，不要绕过或换个说法重试」；full_access 逐字含「审批档为 full-access：全部放行且不会弹审批卡；不要请求批准」。行为对得上：on_request 下 `dir`（白名单）无卡片直接执行、`echo` 弹卡；全部助手文本里「等你批准」0 处、「请批准」0 处；full_access 下 `echo full-access-096` 直接执行（`[sandbox] mode=danger_full_access`），回复只说「命令已执行…退出码 0」 | `evidence/F1-approval-mode-prompt.txt`、`evidence/04-rollout-modify-sum.jsonl`、`evidence/F1-rollout-full-access.jsonl`、`shots/02-dir-allowlist-no-card.png`、`shots/02-approval-card.png` |
| F2 回合结局：截断 / 强杀重开 | **FAIL** | 截断：档案 `reason=max_tokens`、`error.code=INCOMPLETE`、`provider_code=length`；界面有「输出达到上限，这条回答被截断（内容不完整）」、`MAX_TOKENS`、黄色「发送“继续”可让模型接着输出」、无独立错误横幅——但同一行行首红字仍是「**本轮运行失败**」，且回合组标题写「已完成，用时 5 秒」，同屏三种口径。强杀重开：写 `reason=interrupted`、界面「已停止」，而判据（r2 起）要求挂起审批的回合为「状态未知」/`indeterminate`，且审批卡应保留 | `evidence/F2-turn-endings.txt`、`evidence/F2-rollout-truncated.jsonl`、`shots/F2-truncated-live.png`、`shots/F2-truncated-reopen.png`、`shots/02-reopen-after-kill.png`、`evidence/02-rollout-after-kill.jsonl` |
| F3 压缩前确定性裁剪 + 压缩节点 | **FAIL** | 裁剪确实执行：`D6 确定性裁剪完成 pruned_items=1 chars_before=18955 chars_after=5181`，第二次甚至「裁剪后已回到预算内，跳过本次摘要调用」；模型侧确实收到「中间省略 13835 字符，保留头 4096 + 尾 1024」。**但**：①界面工具卡（折叠只显示「编辑」，展开是完整 2000 行原文）与导出都没有「已裁剪」标记——导出扫「已裁剪」0 次；②导出仍保留完整原文（`line 1500` 命中 1 次）；③压缩发生时实时会话区整块空白、输入框消失，压缩节点要重开后才出现；④重开后节点在（「上下文已压缩 · 点击查看压缩摘要」），摘要正文仍需点一次（r6 的 R6-1 未变） | `evidence/03-prune-compaction.txt`、`evidence/03-session.v4.jsonl`、`shots/03-compaction-live-blank.png`、`shots/03-reopen-compaction-node.png`、`shots/03-summary-expanded.png` |
| F4 多块补丁：三处一次落地 / 失败整条不写 | **PASS** | 一次 `apply_patch` 含 3 个 `@@`，回执「补丁应用成功（1 处）：~ src/sections.js」；事后 `search` 命中第 5/60/115 行分别为 A2/B2/C2，`node test/sections.test.js` 通过。故意坏补丁（第 2 块指向不存在的 `sectionZ`）失败后，磁盘第 5 行仍是 `A2`、`sectionZ` 未命中 → 整条不写。**附带新问题 N8**：失败回执里「实际读到」是第 1 块预演后的 `A3`，与磁盘 `A2` 不一致 | `evidence/F4-patch.txt`、`evidence/04-rollout-modify-sum.jsonl`（第 2、3 回合）、`shots/F4-patch-fail.png` |
| F5 工具被拒原因一致 + 可行动替代 | **PASS（带残留 N6）** | 沙箱越界：模型收到「路径越界（工作区之外）: D:/…/outside/x.txt」，界面展开同一张卡读到**逐字相同**的文本；用户拒绝：模型收到「type=user_denied；…用户拒绝了这次调用…可行做法：说明这次操作要达成什么，或换一个更小的动作再问…」，界面展开也逐字相同。替代做法两侧都有（模型回复里给出「写工作区内/调整工作区/确认后再试」与「1–100 先验证/授权完整/落文件」）。**残留**：两类拒绝的卡片 `data-state="ok"`、折叠标题不标已拒绝/失败（越界卡折叠只显示「编辑」），不展开看不出这条没有执行 | `evidence/F5-denial-reasons.txt`、`evidence/00-ui-observations.txt`、`shots/F5-sandbox-expanded.png`、`shots/F5-sandbox-collapsed.png`、`shots/F5-user-denied-card.png` |
| F6 轮次/步数四处一致 + 审批挂起强杀后提问仍在 | **FAIL**（提问仍在这一点 PASS） | 轮次四处一致：界面 3 轮 = rollout 3 条 `turn_end` = 导出 `turn/start 3`/`turn/end 3`。**步数不一致**：rollout `iterations 2+2+1=5` = 界面 5 步，但导出 `turn/end` 不带 `iterations`/`step`，所有带 step 的条目恒为 1 → 「5 步」在导出侧无法核对；另一会话重开后从 2 步变 3 步（该回合无 `iterations`，多出的 1 步无来源）。审批挂起强杀：提问仍在、未重复执行（PASS） | `evidence/F6-counts.txt`、`shots/03-reopen-final-3turns.png`、`shots/02-reopen-after-kill.png`、`shots/02-dir-allowlist-no-card.png` |

## 本轮新问题（未修复，按严重度）

| 编号 | 严重度 | 问题 | 复现步骤 | 证据 |
|---|---|---|---|---|
| N1 | P1 | 压缩发生的实时窗口会话区整块空白、输入框消失，压缩节点不实时出现：截图时窗口只剩标题与会话列表，正文与 composer 全部空白；重开后才以「上下文已压缩」节点恢复 | `token_budget=5000` 的会话发一条会触发压缩的长任务 → 等日志出现 `上下文已压缩（SH-032）`/`D6 确定性裁剪完成` → 立刻截图并读 DOM | `shots/03-compaction-live-blank.png`、`evidence/03-prune-compaction.txt` |
| N2 | P1 | 审批挂起强杀重开：审批卡丢失、结局写 `interrupted`（界面「已停止」），而 r2 判据要求 `indeterminate`+「状态未知」并把卡片保留为「确认中」 | 让模型执行需审批命令 → 卡片出现后任务管理器结束进程 → 重开该会话 | `shots/02-approval-card.png`、`shots/02-reopen-after-kill.png`、`evidence/02-rollout-after-kill.jsonl` |
| N3 | P2 | 截断回合的三重口径：行首红字「本轮运行失败」、正文「回答被截断」、回合组标题「已完成」；判据要求只说「被截断」 | 见 F2 复现（本地代理把 `max_tokens` 改 200 后发一条长回答请求） | `shots/F2-truncated-live.png`、`evidence/F2-turn-endings.txt` |
| N4 | P2 | 导出丢步口径：`turn/end` 无 `iterations`，`assistant/tool` 的 `step` 恒为 1，界面/rollout 的「N 步」在导出里查不到 | 任一多步会话 → 「···」→「下载 Session 日志」→ 看 `session.v4.jsonl` 的 `turn/end` 与 `step` | `evidence/F6-counts.txt`、`evidence/03-session.v4.jsonl`、`evidence/09-session.v4.jsonl` |
| N5 | P2 | 强杀重开后步数 +1：无 `iterations` 的回合被计 1 步（2 轮 2 步 → 2 轮 3 步；「从1输出到30000」3 轮 4 步同理） | 挂起审批时强杀 → 重开看页脚 | `shots/02-reopen-after-kill.png`、`evidence/00-ui-observations.txt` |
| N6 | P2 | 被拒工具的卡片没有状态标记：沙箱越界卡折叠只显示「编辑」、用户拒绝卡折叠只显示「工具调用unified_exec · 命令」，两者 DOM 都是 `data-state="ok"`；原因只在展开正文里（滚动离开还会因虚拟化卸载退回标题） | 触发一次越界写 / 点「拒绝」→ 不展开卡片直接看标题 | `shots/F5-sandbox-collapsed.png`、`evidence/00-ui-observations.txt` |
| N7 | P2 | 被裁剪的工具输出在界面与导出都没有「已裁剪」标记：界面工具卡是未裁剪原文，导出 `line 1500` 仍在、扫「已裁剪」= 0；只有模型复述里出现「中间省略」 | 小预算触发 `D6 确定性裁剪` → 看工具卡正文与导出 ZIP | `evidence/03-prune-compaction.txt` |
| N8 | P3 | `apply_patch` 失败回执的「实际读到」是本次补丁第 1 块预演后的内容（`A3`），磁盘真实内容是 `A2`，会误导模型重建补丁 | 对一个文件提交「第 1 块可命中、第 2 块上下文不存在」的多块补丁 → 看回执「文件中最接近的位置」 | `evidence/F4-patch.txt`、`shots/F4-patch-fail.png` |
| N9 | P3 | 跨回合读取状态失效（写后失效，方向正确）但文案写「未读取过 src/sections.js」，与上一回合刚读过的事实不符 | 第 1 回合 read 并 patch 同一文件 → 第 2 回合直接 patch → 回执 `write_file_not_read` | `evidence/F4-patch.txt` |
| N10 | 测试环境（非产品） | `root-d` 第 1 回合是 `ENDPOINT_UNSUPPORTED`（HTTP 404），成因是截断代理首版没给上游加 `/openai` 前缀；第 2 回合才是截断证据 | 代理把 `/v1/*` 直连上游而非 `/openai/v1/*` | `evidence/F2-rollout-truncated.jsonl`（第 1 条 `turn_end`） |

## 与历轮结论的对照

| 历轮项 | 本轮 | 依据 |
|---|---|---|
| r1 N2（审批中强杀丢回合）→ r2 已修 | **部分回归**：提问不再丢，但卡片丢失、结局从 `indeterminate` 变 `interrupted` | 十项 2、N2 |
| r2 N3（轮次三处不一致）→ 已修 | 保持（3/3/3/3） | F6 |
| r3/r6 R6-1（压缩节点默认可见、摘要需点一次） | 保持：节点在 Turn 层可见，摘要仍需点一次 | F3 |
| r5 N5（压缩提示实时不显示） | 仍存在且更重：实时整块空白，节点完全不出现 | N1 |
| r6 轮次回归（实时=重开=导出=rollout） | 轮次保持；**步数在导出侧缺失** | F6 |
| 十项 10（旧档伪「已完成」） | 保持修复：两处均「本轮状态未知」 | 十项 10 |

## 证据索引（`evidence/`）

- 环境与总览：`00-env.txt`、`00-ui-observations.txt`
- 十项：`01-rollout-before-kill.jsonl`、`01-rollout-after-kill.jsonl`、`02-rollout-before-kill.jsonl`、`02-rollout-after-kill.jsonl`、`02-events-after-kill.jsonl`、`03-rollout-compaction.jsonl`、`03-events-compaction.jsonl`、`03-prune-compaction.txt`、`03-session.v4.jsonl`、`03-session-log.zip`、`03-export.md`、`04-rollout-modify-sum.jsonl`、`05-rollout-1-to-30000.jsonl`、`06-tray-enum.txt`、`09-session.v4.jsonl`、`09-session-log.zip`、`09-tool-cards.txt`、`10-legacy-rollout.jsonl`
- 六项复验：`F1-approval-mode-prompt.txt`、`F1-rollout-full-access.jsonl`、`F2-turn-endings.txt`、`F2-rollout-truncated.jsonl`、`F4-patch.txt`、`F5-denial-reasons.txt`、`F6-counts.txt`

## 证据索引（`shots/`）

- 十项：`01-live-completed.png`、`01-reopen.png`、`02-approval-card.png`、`02-dir-allowlist-no-card.png`、`02-reopen-after-kill.png`、`03-compaction-live-blank.png`、`03-reopen-compaction-node.png`、`03-reopen-final-3turns.png`、`03-summary-expanded.png`、`04-approval-npm.png`、`05-fresh-data-root.png`、`06-before-close.png`、`06-after-close.png`、`06-restored-from-tray.png`、`07-silent-30s.png`、`07-silent-65s.png`、`08-subagent-live.png`、`08-subagent-done.png`、`08-subagent-text.png`、`09-export-click.png`、`10-legacy-list.png`、`10-legacy-open.png`
- 六项复验：`F2-truncated-live.png`、`F2-truncated-reopen.png`、`F4-patch-fail.png`、`F5-sandbox-expanded.png`、`F5-sandbox-collapsed.png`、`F5-user-denied-card.png`
- 启动现场：`00-boot.png`

## 清理

- 测试结束时结束 SHANIU 进程、关闭本地代理，删除 `scratch\smoke-1117`（含 4 个数据根、4 个测试工程、导出件与截图）。
- 回收量：**140 个文件 / 9,241,067 B（8.81 MB）**，`Test-Path scratch\smoke-1117` → `False`；`scratch\` 下其余为本轮之前已存在的目录，未动。
- 仓库内除本目录外无新增/残留文件（`git status` 仅 `docs/smoke/2026-11-17-9af8431/`）。
