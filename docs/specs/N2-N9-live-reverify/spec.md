# N2–N9 真机复验规格

- 状态：待确认
- 日期：2026-11-17
- 被测二进制：`dist-portable\SHANIU-Portable\SHANIU.exe`
  - 28,803,584 B，SHA256 `0ABD6B0A93EEE86FA418924A6FAC407A1440F604430BA91280A85177DFDEC071`
  - 与 N1 真机复验同一份产物；`git diff --stat 9e057fd..HEAD` 不含产品代码，无需重编 Rust
- 判据来源：`docs/smoke/2026-11-17-9af8431/REPORT.md` 第 60–67 行（N2–N9 原文，不改）
- 证据落点：`docs/evidence/2026-11-17/n2n9-live/`

## 1. 目标

用**真机**（同一份 release exe + WebView2 CDP）逐条重跑 N2–N9，给出 PASS/FAIL，并在
`docs/smoke/2026-11-17-9af8431/REPORT.md` 追加「后续复验」节：

- 保留 9af8431 原判据与原结论，不修改原表；
- 新增一节逐条写「复验时点 / 被测提交 / 实际现象 / 结论 / 证据」；
- `docs/evidence/2026-11-17/README.md` 追加同类索引节。

**不改产品代码，不改判据。**本轮产物只有：两个驱动脚本、证据文件、REPORT/README 增补。

## 2. 为什么脚本化模型端点

八项全部是宿主/界面/导出侧逻辑（结局判定、步数、裁剪留痕、补丁回执、拒绝态回放），
模型只需按协议**确定性**发出 `tool_calls` 或 `finish_reason=length`，不需要真实模型能力。

- 方案 A（采用）：扩一个按场景脚本化应答的本地 OpenAI 兼容端点，配 release exe + CDP 驱动。
  - 优点：每条判据的触发条件完全可控、可重复；不联网、不烧真实 token；证据可复算。
  - 缺点：需要自己实现 SSE `tool_calls` 分片与辅助请求识别。
- 方案 B（否决）：接真实模型跑自然对话。触发截断/越界/多块补丁失败不可控，复验不可重复。
- 方案 C（否决）：只跑 Rust/vitest 单测。用户要的是真机复验，单测已在修复提交里覆盖。

为不破坏既有冒烟，**新建** `scripts/smoke/scenario-model.mjs`，不改 `stub-model.mjs`
（`run-smoke.mjs` 依赖它固定回 `烟测回复 <nonce>`）。

## 3. 新增文件与职责

### 3.1 `scripts/smoke/scenario-model.mjs`

导出 `startScenarioStub({ port, scenario, requestLog })`，返回 `{ url, close, state }`。

请求分类（按顺序判定，辅助请求**不推进**场景步号）：

1. `title`：`messages[0].role === 'system'` 且内容含
   `Create a concise title for an AI coding-assistant session` → 流式回一个短标题。
2. `compaction`：最后一条消息文本含
   `请把上面这段对话与工具执行历史压缩成一份交接摘要` → 流式回固定摘要。
3. `main`：取 `scenario.steps[state.mainIndex++]`。

主请求 SSE 形状（与 `src-tauri/src/model_client/mod.rs` 的解析口径一致）：

- 文本步：`delta:{role:'assistant',content}` 分 2 片 → `finish_reason`；
- 工具步：`delta:{tool_calls:[{index,id,function:{name,arguments}}]}`（`arguments` 至少切 2 片）
  → `finish_reason:'tool_calls'`；
- 末尾统一补 usage chunk 与 `data: [DONE]`；
- `finish_reason` 支持 `stop` / `length` / `tool_calls`。

每个主请求向 `requestLog` 追加一行：`{ at, kind, mainIndex, messageCount, lastUserText(截断),
toolResultCount, overflow }`。`steps` 用尽后回固定文本并置 `overflow: true`（驱动据此判红）。

### 3.2 `scripts/smoke/n2n9-live.mjs`

命令行：`node scripts/smoke/n2n9-live.mjs --scenario <n2|n3|n4|n5|n6|n7|n8|n9> --exe <path> --out <dir>`

复用 `run-smoke.mjs` 已验证的 CDP 连法（`/json/list` 轮询、`Runtime.evaluate`、
`Page.captureScreenshot`、`Input.insertText` + Enter），并新增：

- `launch()` / `kill()` / `relaunchSameDataRoot()`：每条启动都用
  `SHANIU_CDP_PORT` + `SHANIU_DATA_DIR`，`taskkill /PID <pid> /T /F` 收尾；
- `storeKey()`：CDP 调 `store_api_key({ provider:'SMOKE_KEY', key:'smoke-key' })`；
- `openLatestSession()`：重开后点侧栏第一条会话（失败则 Ctrl+N 后按标题匹配）；
- `turnFooter()`：从 `document.body.innerText` 取最后一个 `/(\d+)\s*轮\s*(\d+)\s*步/`；
- `toolCard(callId)`：`[data-chat-call-id="<id>"]` 的 `data-state` / 正文；
- `exportZip(dest)`：点页头「导出/Export」→ 唤起原生「另存为」→ 用 PowerShell
  `System.Windows.Forms.SendKeys` 输入绝对路径并回车 → 轮询 `dest` 出现 →
  `Expand-Archive` 解压取 `session.v4.jsonl`；20 秒未出现记 FAIL 并截图；
- `rolloutFile()`：在 `<dataRoot>` 下递归找最新 `*.jsonl` 且首行含 `session_meta`；
- 结束时写 `verdict.json`：`{ scenario, checks:[{name, ok, detail}], artifacts:[...] }`。

配置模板（每场景独立数据根；工作区必须在数据根**之外**）：

```toml
active_provider = "smoke"

[workspace]
root = "<out>/<scenario>/workspace"

[approval]
mode = "on_request"        # 需要审批的场景；N3/N4/N7/N8 用 full_access 免卡

[sandbox]
mode = "workspace_write"

[agent]
token_budget = 500000

[[providers]]
id = "smoke"
label = "Smoke"
provider = "openai-chat"
base_url = "http://127.0.0.1:<stubPort>/v1"
model = "smoke-1"
secret_ref = "SMOKE_KEY"
```

## 4. 场景矩阵（共 8 项）

> 工具参数口径：`unified_exec {command}`、`read_file {path}`、
> `apply_patch {patch}`（`*** Begin Patch` … `*** End Patch`，V4A 段/`@@` 块）。

### N2 审批挂起强杀（P1，修 `4421ee3`）

- 配置：`on_request` + `workspace_write`。
- 脚本：`unified_exec echo n2-<nonce>` → 文本「已执行。」
- 操作：发消息 → 等 `[data-approval-key]` → 截图 → **不裁决**，`taskkill` 整树。
- 重开同一数据根，断言：
  1. `body.innerText` 含「本轮状态未知」；
  2. `body.innerText` **不含**「已停止」「任务已停止」；
  3. 无 `[data-approval-key]`；
  4. 含「这条审批在程序关闭前未作答，已失效」；
  5. rollout 最后一条 `turn_end.reason === "indeterminate"`。
- 证据：`n2-before-kill.png`、`n2-reopen.png`、`n2-requests.jsonl`、`n2-rollout.jsonl`。

### N3 截断（P2，修 `5467acf` + `0c3cedb`）

- 配置：`full_access`。
- 脚本：单步文本，`finish_reason:"length"`。
- 断言（实时 + 重开后各一遍）：
  1. 含「输出达到上限」或「被截断」提示恰好 **1 处**（按包含「被截断」的元素计数 = 1）；
  2. 回合组标题含「已截断」；
  3. 不含「本轮运行失败」；
  4. 不含「已完成」。
- 证据：`n3-live.png`、`n3-reopen.png`、`n3-requests.jsonl`、`n3-rollout.jsonl`。

### N4 导出步数（P2，修 `2e394f2`）

- 配置：`full_access`，`agent.token_budget=500000`。
- 脚本（2 回合，每回合 2 步）：
  1. `read_file note.md` → 文本「第一轮完成。」
  2. `unified_exec echo n4-2` → 文本「第二轮完成。」
- 操作：发 2 条用户消息 → 等 2 轮结束 → UI 导出到 `<out>/n4-export.zip`。
- 断言（解析 `session.v4.jsonl`）：
  1. 两条 `turn/end` 均带数值 `iterations`，且与 rollout 两回合 `iterations` 一致；
  2. 至少一个回合条目出现 `step === 2`（步号非恒 1）；
  3. 所有条目 `step` 满足 `1 <= step <= 该回合 iterations`；
  4. 缺权威步数用例：把本次 rollout 复制为 `legacy-n4-<ts>.jsonl`，删除所有
     `turn_end.iterations` 后放回 rollout 目录，重开并导出 → 对应 `turn/end`
     必须带 `stepsUnavailable: true`，且该回合条目不带 `step`。
- 证据：`n4-session.v4.jsonl`、`n4-legacy-session.v4.jsonl`、`n4-rollout.jsonl`、
  `n4-export.png`、`n4-requests.jsonl`。

### N5 挂起强杀步数不增（P2，修 `4421ee3`）

- 配置：`on_request` + `workspace_write`。
- 前置：第 1 回合文本（1 步）；第 2 回合 `read_file note.md` + 文本（2 步）。
- 触发：第 3 回合 `unified_exec echo n5-<nonce>` → 审批卡出现后强杀。
- 断言（重开后）：
  1. `turnFooter()` 仍为 `2 轮 2 步`（不得变成 2 轮 3 步）；
  2. rollout 中 `turn_end` 行数仍为 2，无第 3 条；
  3. 含「本轮状态未知」，不含「已停止」。
- 证据：`n5-before-kill.png`、`n5-reopen.png`、`n5-rollout-before.jsonl`、
  `n5-rollout-after.jsonl`、`n5-requests.jsonl`。

### N6 拒绝/越界工具卡折叠态（P2，修 `38114d9`）

- 配置：`on_request` + `workspace_write`。
- 夹具：工作区 `marker.txt` 正文含
  `路径越界（工作区之外）: 这是文档里的示例文本，不是拒绝` 与
  `拒绝说明：type=user_denied；这是示例`；工作区外 `outside.txt`。
- 脚本单回合三次调用（call id 固定）：
  1. `read_file ../outside.txt`（`c-n6-out`）→ 路径越界；
  2. `read_file marker.txt`（`c-n6-marker`）→ 正常读取（负例）；
  3. `unified_exec echo n6-denied`（`c-n6-deny`）→ 审批卡出现后点「拒绝」。
- 断言（实时 + 重开后各一遍）：
  - `c-n6-out` 与 `c-n6-deny` 的 `data-state !== "ok"`（应为 `error`）；
  - `c-n6-marker` 的 `data-state === "ok"`；
  - 两张错误卡正文仍保留原拒绝说明（未被吞）。
- 证据：`n6-live.png`、`n6-reopen.png`、`n6-cards-live.json`、`n6-cards-reopen.json`、
  `n6-requests.jsonl`、`n6-rollout.jsonl`。

### N7 确定性裁剪留痕（P2，修 `cec329d`）

- 配置：`full_access`，`agent.token_budget=500000`（裁剪只看单项 > 8192 字符，与预算无关）。
- 脚本：
  1. `unified_exec python -c "for i in range(1,2001): print('line', i)"`（输出约 2 万字符）；
  2. 文本「命令完成。」（发起此次请求时触发裁剪与实时通知）。
- 断言：
  1. 实时界面出现通知，含「的输出在发给模型前被确定性裁剪」与「卡片里显示的仍是裁剪前原文」；
  2. 重开后该工具卡正文含 `[已裁剪]` 判读行，且仍含原文尾部 `line 2000`；
  3. rollout 保留裁剪前完整原文（该工具输出长度 > 8192 且含 `line 2000`）；
  4. 导出文本扫「已裁剪」出现次数 > 0。
- 证据：`n7-live.png`、`n7-reopen.png`、`n7-export.png`、`n7-session.v4.jsonl`、
  `n7-rollout.jsonl`、`n7-requests.jsonl`。

### N8 多块补丁第 2 块失败（P3，修 `45ddcc9`）

- 配置：`full_access`。
- 夹具 `data.txt`（A2）：
  ```
  a1
  a2
  a3
  ```
- 脚本：一次 `apply_patch`，两段同文件：
  ```
  *** Begin Patch
  *** Update File: data.txt
  @@
  -a1
  +a1-first
  *** Update File: data.txt
  @@
  -a1-first
  +second
  *** End Patch
  ```
- 断言：
  1. 补丁卡 `data-state === "error"`；
  2. 回执含「实际读到 `a1`」（磁盘原文 A2），**不含**「实际读到 `a1-first`」；
  3. `data.txt` 仍为 A2（整单回滚）。
- 证据：`n8-live.png`、`n8-card.json`、`n8-data-after.txt`、`n8-requests.jsonl`、`n8-rollout.jsonl`。

### N9 读取记录过期文案（P3，修 `45ddcc9`）

- 配置：`full_access`。
- 夹具 `note.txt` 初始 `alpha\nbeta\n`。
- 脚本：
  1. 第 1 回合：`read_file note.txt` + `apply_patch`（`alpha`→`alpha-1`）；
  2. 文本「已修改。」；
  3. 第 2 回合：直接 `apply_patch`（`alpha-1`→`alpha-2`，不再读）；
  4. 文本「第二次未写。」。
- 断言：
  1. 第 2 回合补丁卡 `data-state === "error"`；
  2. 正文含
     `note.txt 在本任务内没有可用的读取记录（可能尚未读取，或上次读取发生在本次修改之前、内容可能已过期）`；
  3. 正文**不含**「未读取过」；
  4. `note.txt` 仍为第 1 回合结果 `alpha-1`。
- 证据：`n9-live.png`、`n9-card.json`、`n9-note-after.txt`、`n9-requests.jsonl`、`n9-rollout.jsonl`。

## 5. 运行方式与硬约束

每个场景一条独立 PowerShell 命令（启动 exe、跑驱动、收尾必须在同一条命令内，
否则命令行会话结束会回收子进程树）：

```powershell
node scripts/smoke/n2n9-live.mjs --scenario n2 --exe "dist-portable\SHANIU-Portable\SHANIU.exe" --out "docs/evidence/2026-11-17/n2n9-live"
```

驱动内部负责启动/杀进程；PowerShell 只做外层超时与日志重定向。

已踩过的约束继续遵守：

1. 工作区根在数据根之外，否则启动弹「工作区设置不可用」、CDP 端口不开；
2. CDP 端口走 `SHANIU_CDP_PORT`，数据根走 `SHANIU_DATA_DIR`；
3. 输出重定向到 `.txt`，不用 `| Select-Object -First`（EPIPE 会杀 node）；
4. 证据文件用 `.txt`/`.json`（`.gitignore` 忽略 `*.log`）；
5. 中文脚本用 `pwsh -NoProfile -ExecutionPolicy Bypass -File`。

## 6. 判定与落档

- 单项 PASS：该场景全部断言为真。
- 单项 FAIL：任一断言为假；`verdict.json` 记录失败断言与截图。
- 环境起不来（CDP 未出现 / 导出对话框未完成）记 BLOCKED，并在 REPORT 写明卡点，
  不得把 BLOCKED 写成 PASS。
- REPORT 追加节格式：

```markdown
## 后续复验（N2–N9，被测 4d92256 / exe SHA256 0ABD6B…C071）

| 项 | 修复提交 | 复验结论 | 实际现象 | 证据 |
|---|---|---|---|---|
| N2 | 4421ee3 | PASS/FAIL | … | … |
...
```

- 原「十项总表」「六项重点复验」及 9af8431 原结论**一字不改**。

## 7. 验收标准

1. 八个场景全部有 `verdict.json` 与截图，无缺项；
2. REPORT / README 增补节与 `verdict.json` 结论逐条一致；
3. `git status` 只出现：两个新脚本、证据目录、REPORT/README 修改；
4. 产品代码与判据零改动。

## 8. 需要确认

- 是否同意按上述 8 场景矩阵执行；导出两项（N4/N7）允许用 PowerShell SendKeys
  驱动原生「另存为」对话框，失败则如实记 BLOCKED。
- 完成后是否立即提交并推送（默认：先报告结果，提交/推送等你确认）。

## 9. 执行期校正（实测口径，正文方案不改）

落地时按实测收窄了六处，细节与理由见 [tasks.md](file:///d:/MyWorkspace/SHANIU/docs/specs/N2-N9-live-reverify/tasks.md)：

1. **N4 步号 0 起**：导出 `step` 与实时 `step/start` 同码（`0`/`1`），断言改为
   `new Set(steps).size >= 2 && steps.includes(1)` 且 `0 <= step < iterations`，不是「出现 step===2」。
2. **N5 前置为 2 轮 3 步**：第 2 回合是 `read_file` + 收尾两步，故重开前后都应是 3 步，
   判据改为「与强杀前逐项相等」，并新增「挂起回合 `indeterminate` 且不带 `iterations`」。
3. **N7 用 `token_budget=5000` + 端点不回报 `usage`**：「单项 >8192 字符即裁剪、与预算无关」
   不成立——不触发预算就不会裁剪，且 `observed ≥ 预算` 会连带压缩把回合吞进摘要。
4. **N8 改为同一 `Update File` 段两个 `@@` 块**：两次 `*** Update File:` 头会按段顺序写盘，
   验不到同一 `apply_hunks` 口径；另加前置 `read_file` 以免先撞 `write_file_not_read`。
5. **导出保存对话框改 `WM_SETTEXT` + `BM_CLICK`**：SendKeys 依赖前台焦点，实测从未成功；
   且 PowerShell 5.1 读 `.ps1` 需 UTF-8 **BOM**，否则中文「保存」匹配串乱码。
6. **N6 实时只判「失败」标记**：实时流只给错误码（`ToolError: tool_error`/`Denied: denied`），
   完整拒绝说明只在档案回放出现，文案判据按重开判，该口径差记为附带观察。
