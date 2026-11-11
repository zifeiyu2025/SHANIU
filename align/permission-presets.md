# 权限档位对齐：DSH ↔ SHANIU

> 状态：**已批准（2026-11-11），档位语义的唯一事实源**｜本文件只做对照与裁决记录；实现状态见 §9
> 位置理由：`docs/0x` 是契约原文，`align/` 是「文档 ↔ 实现」对照物的既定落点
> （`ledger.md` 对上游、`overrides.json` 对偏离、`doc-claims.md` 对文档断言）。
> 档位语义是**对照结论**而不是新契约，放 `align/`；批准后本文件成为档位语义的
> 唯一事实来源，以后改档位先改这里，再改代码与 `docs/`。

## 1. DSH 的模型（源码事实）

DSH 把权限拆成两个独立旋钮，预设只是两个旋钮的一个**捆绑**：

| 旋钮 | 取值 | 出处 |
| --- | --- | --- |
| `sandbox/mode` | `read-only` / `workspace-write` / `danger-full-access` | `packages/sandbox/sandbox-policy/src/session-mode.ts:42` |
| `approval/policy` | `ask` / `never` | `packages/interaction/user-approval/src/index.ts:71` |

- 切档 = 追加一条 `permission/preset` 事件 + 两条旋钮事件；
  `permission/preset` 只在「两个预设捆绑相同」时用来保留用户意图。
- 预设表可配置；**出厂默认表只有两档**：`workspace-write`（+`ask`）与
  `danger-full-access`（+`never`）（`packages/interaction/permission-presets/src/index.ts:182-191`）。
- `read-only` 有完整显示名与说明，但**不在出厂默认表里**——它属于
  「部署可以配置的档」，不是出厂可选档。
- 旋钮值匹配不到任何预设时，读侧返回派生值 `custom`；`custom` 永远不是切换目标。
- `auto` 是保留 id，只有实验性自动审查插件在线时才出现在**当前会话**目录里，
  不能作为新会话默认档。

## 2. DSH 档位 / 状态全表

判定口径：**一致** / **我们更严** / **我们更松** / **我们扩展**（缺口单列）。

| id | 中文名 | English | 说明原文 | 捆绑 | 语义 | 可作默认 | 判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `read-only` | 仅可查看 | Read Only | 只读查看，不产生任何写入<br>`Read-only access, no writes at all` | `read-only` + `ask` | 文件写入全部拒绝；执行本身不被审批层拦，由沙箱在写入点拒绝 | 仅当部署把它写进预设表 | **我们更严**：我们连命令执行都拒绝，且额外做工作区外读隔离（见 §4.2） |
| `workspace-write` | 工作区内修改 | Workspace Write | 「仅在工作区内读写」<br>`Read and write inside the workspace only`<br>宿主 catalog：`Write inside the workspace and permitted temporary directories; wider retries require approval.` | `workspace-write` + `ask` | 工作区与允许的临时目录内可写；越界重试需要审批 | 是（出厂默认表） | **当前更严**（区内写入多问一次）；取证支持对齐，见 §4.1；命令执行仍更严 |
| `danger-full-access` | 完全权限 | Full access | 「不受工作区边界限制」<br>`Not limited by the workspace boundary`<br>宿主 catalog：`Full file access without approval prompts.` | `danger-full-access` + `never` | 绕过文件沙箱；不再弹审批；需要审批的操作在 `never` 下确定性拒绝 | 是（出厂默认表） | **一致**（文件边界/不弹窗/需审批者拒绝）；计划评审放行是本仓扩展 |
| `auto`（EXP） | 自动审查 | Auto review | 「无沙箱运行；每次原生工具调用和 PTC 内层调用前由同一模型进行实验性审查。」<br>`Run without a sandbox after an experimental same-model review of every native tool call and PTC inner call.` | `danger-full-access` + `ask` + 每次调用同模型审查 | 无沙箱；审查器拒绝后 `ask` 把决定交回用户，子代理被钉成 `never` 时拒绝即终局 | 否（仅当前会话，插件在线才出现） | **缺口**：缺同模型审查器，不提供 |
| `custom` | Custom（原文即英文） | Custom | `Current sandbox and approval settings do not match a preset.` | 无 | **派生状态**：当前两维组合不匹配任何预设；不是切换目标，不进事件 | 否 | **缺口**：需补派生值，禁止回落成具体档 |
| `unavailable` / `unreadable` | 不可用 / 读取失败 | Unavailable / Unreadable | 「不可用」/「读取失败」 | 无 | 两个不同位置：设置命名空间读不到 = `unavailable`；**本会话档案读不到** = `unreadable`。后者是本仓扩展（`presentation.ts` 注释明说由 Shanzhu adapter 发出，已记 `docs/07`） | 否 | **我们扩展**（状态，不是档） |

补充原文（`danger-full-access` 的确认弹窗，中/英）：
「启用完全权限后，新会话将减少确认步骤，并且可以直接执行更多操作，包括敏感操作、文件修改或外部命令。仅建议在你信任后续任务时使用。」

## 3. 逐档映射到我们的 `sandbox × approval → decide`

我们的 `ApprovalMode` 有 4 值，DSH 只有 2 值：

| DSH | 我们 | 关系 |
| --- | --- | --- |
| `ask` | `on_request` | 近似：都按风险询问；**差异**是我方 execute 批准一次后会记入会话，DSH 无此记忆 |
| `never` | `never` | 等价：都不询问，需要审批的操作确定性拒绝（fail-closed） |
| — | `on_failure` | 本仓扩展（沙箱拒绝后问一次提权），DSH 无对应 |
| — | `full_access` | 本仓扩展（且要求 danger 沙箱），DSH 无对应；按上轮决定**不进预设** |

`decide()` 结果（L=Allow 放行，P=Prompt 询问，D=Deny/Forbidden 拒绝）：

| DSH 档 | 我们的组合 | Read | WriteInWorkspace | Execute | Destructive | PlanReview | 结论 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `read-only` | `read_only` + `on_request` | L | D | **D** | P | P | **我们更严**：写入语义一致，执行与工作区外读隔离都比 DSH 严（§4.2） |
| `workspace-write` | `workspace_write` + `on_request` | L | **P→L**（§4.1） | P（可记忆） | P | P | **当前更严**：区内写入多问一次，取证后对齐；执行仍比 DSH 严 |
| `danger-full-access` | `danger_full_access` + `never` | L | L | L | D | D→**L+审计**（上轮批准的目标） | **一致**（文件边界/不弹窗/需审批者拒绝）；计划评审放行是本仓扩展 |
| `auto` | `danger_full_access` + `on_request` | L | L | P | P | P | **缺口**：只映射到旋钮，缺「同模型逐调用审查器」，不能声称等价 |
| `custom` | 任意不匹配组合（如 `workspace_write` + `on_failure`） | — | — | — | — | — | **缺口**：我方计划里会把组合塌缩到某个预设，DSH 明确要求 `custom` |

## 4. 取证：两处语义差异（2026-11-11）

### 4.1 `workspace-write` 下 dsh 是否对区内写入弹审批

**dsh 结论：不弹。区内写入直接放行，只有「越界重试」和沙箱拒绝后的提权才问。**

证据（源码）：

- `packages/fs/tool-fs/src/sandbox.ts:88-97`：写工具在没有 `sandbox_permissions`
  时直接返回会话的 standing policy，**不调用 approval**；只有显式请求更宽档位才走
  `approveEscalation`。
- `packages/sandbox/sandbox/src/escalation.ts:172-180`：请求的模式等于当前有效模式 →
  原样返回、不审批；只有**严格更宽**的模式才要求审批（`:189-196` 才真正发 request）。
- 宿主 catalog 原文（`packages/interaction/permission-presets/src/index.ts:185`）：
  `Write inside the workspace and permitted temporary directories; wider retries require approval.`
  ——「wider retries」才需要审批，区内写入不在其列。
- 沙箱边界由文件系统后端执行（`sandbox-windows-acl/src/index.ts`），不是由审批层逐次询问。

**我方现状：更严。** `decide(WriteInWorkspace, OnRequest, WorkspaceWrite) = Prompt`，
每个 `apply_patch` 都要点一次。

**能否对齐（改成一致）：可以，前提是下面三条同时成立。**

1. `apply_patch` 的**所有**变更路径都过 `resolve_for_write`：
   dry-run 全量校验 + Add/Update/Delete 各自再解析一次
   （`src-tauri/src/tools/apply_patch.rs:541-548`、`:557`、`:587`、`:628`）。
   回滚只写回已解析出的绝对路径（`apply_patch.rs:480-495`）。
2. 路径校验本身有效：拒 `..`（`tool_router/mod.rs:781-788`）、
   拒重解析点（`:746-766`、`:791-798`）、最深存在祖先 canonicalize 后必须在根内
   （`:807-822`）。已有测试：悬空符号链接（`:884-907`）、`..`（`:1098-1101`）、
   新建深路径（`:1094-1096`）。
3. 命令侧 ACL 仍是真边界，且有真进程探针：
   区内写入成功（`tests/sandbox_probe.rs:71-79`）、工作区外写入失败（`:97-111`）、
   工作区外读取失败（`:115-131`）；沙箱不得退化为无沙箱执行（`:60`）。

**补齐后才许放宽**（本轮一并做，见 tasks）：

- 绝对路径指向工作区外（不含 `..`）→ 拒绝；
- 工作区内**已存在**的符号链接指向工作区外文件 → 拒绝（现有测试只覆盖悬空链接）；
- 能造出 junction/mount point 时覆盖「重解析点位于中间层级」→ 拒绝；
- 以上三条先各写一条**反向对照**（去掉校验 → 变红），再还原。

**必须如实记录的残余边界**（不因本轮改动消失）：

- 工作区内**硬链接**指向工作区外的文件对象：路径校验看不出来（canonicalize 后仍在根内）。
  dsh 自己的 ACL 后端也把 hard-link 列为已记录边界（`sandbox-windows-acl/README.zh.md:70`）。
- `resolve_for_write` 与 `fs::write` 之间存在 TOCTOU 窗口；并发替换路径组件理论上可绕过。
- 回滚复用先前解析结果，不再重新校验（同一窗口）。

### 4.2 `read-only` 下 dsh 是否允许执行命令、只挡写

**dsh 结论：允许执行，写被 ACL 拒绝，读与网络都不设限。**

证据：

- `packages/sandbox/sandbox-windows-acl/src/index.ts:69`：read-only 只接受
  `tempDir = null/undefined`；`:90-94`：read-only 与「零授权」配对。
- `README.zh.md:70`：read-only 下不存在显式写入授权，写入与删除都被拒绝；
  `README.zh.md:104`：read-only 的 restricting 列表（登录 SID + Everyone）
  **不含写入 SID**。
- 已知边界（`sandbox-windows-acl/src/index.ts:26-31`）：
  「writes are restricted；**reads, network, and process visibility are NOT**」。
- 功能探针（`sandbox-local/src/index.ts:94-113`）只证明「read-only 能创建令牌并 spawn
  `cmd /c exit 0`」，**不证明写入被拒**。

**我方现状：更严，而且不是同一形态。**

- 审批层：`decide(Execute, *, ReadOnly) = Deny`（policy.rs 的 Execute 分支）。
- 沙箱层三重拒绝：`prepare_spawn`（`sandbox/mod.rs:167-171`）、
  `prepare_terminal`（`:235-237`）、`prepare_stdio`（`:750-752`）；
  真进程用例 `tests/sandbox_probe.rs:82-93` 明确断言「read-only 必须拒绝命令执行」。
- 读隔离：我们的 AppContainer **额外**拦工作区外读取（`tests/sandbox_probe.rs:115-131`），
  dsh 明确不拦读。
- 网络：两边都不按策略断网；我们额外拦 loopback（`:211-236`），dsh 连 loopback 也不拦。

**能否对齐：本轮不能证明，保留现状。**

- ACL 原语本身有真进程证据：`Access::Read` 的 AppContainer 能读、写同一目录失败
  （`sandbox/windows_impl.rs:2275-2295`）。
- 但**产品里没有 read-only 执行路径**：要新开三处 spawn 分支（命令/终端/MCP）、
  新增「read-only 下区内写→拒、区外写→拒、区外读→拒」的真进程探针，
  并改写 ADR-002「只读禁止命令执行」的既有决定。这不是预设接线，是新增一种沙箱执行档。
- 因此按你的规则：**保留现状，并在表中标「我们更严」**，不写成「与 dsh 一致」。
  若以后要做，单独立项，先补探针再改策略。

## 5. 缺口清单

1. **`auto` 的审查器缺失**：我们没有任何「每次工具调用前由同模型裁决」的组件。
   只把旋钮设成 `danger-full-access × ask` 会得到「无沙箱但仍弹审批」，与 DSH
   文案承诺相反。结论：**不提供 `auto`**，不自造同名档。
2. **`custom` 派生态缺失**：上一轮方案里的 `id_of(sandbox, approval)` 在非规范组合
   下回落到「该沙箱最严预设」，这会把 `custom` 显示成一个具体档位，等于撒谎。
   需改为返回派生值 `custom`（不可选、不可作默认）。
3. **`read-only` 的执行语义更严**：DSH 的 read-only 是「沙箱挡写、命令照跑」，
   我们是「门禁直接拒绝 Execute」。这是 ADR-002 的既有决定，**不是缺陷**，
   但文案不能写成「只读命令可执行」。
4. **`workspace-write` 的工作区内写入更严**：DSH 在 `ask` 下工作区内写入不弹窗，
   我们每个补丁都问。取证（§4.1）支持对齐：路径校验与 ACL 都成立，补三条测试后
   把 `WriteInWorkspace` 在 `on_request` 下改为 `Allow`。注意这只解决「写」，
   `Execute` 在 `on_request` 下仍询问，所以无人值守仍需预授权档（见 §6/§7）。

## 6. 反向：我们有 / 计划有、DSH 没有

| 本仓档位 | DSH 是否有 | 处理 |
| --- | --- | --- |
| `workspace-write-auto`（计划新增：`workspace_write` + `never`） | **没有**。DSH 从不把 `workspace-write` 与 `never` 捆绑；它唯一的无人值守路径是 `danger-full-access`+`never` | **明确标为扩展**：id 保留，文案自拟；**「扩展」只作内部标记（本表/代码注释/`docs/07`），界面不出现该字样**；理由——上一条硬要求禁止把无人值守做成完全访问，DSH 没有「工作区内无人值守」档，这个组合只能由我们补 |
| `on_failure` / `full_access` | 没有 | 保留在 `config.toml`，**不进预设**；生效时按 DSH 口径显示为 `custom` |
| `unavailable`（我们已有的前端状态） | 没有 | 已有扩展，`docs/07` 已登记 |
| `enter_plan_mode` / `exit_plan_mode`（PlanReview） | DSH 的计划模式不走审批策略 | 本仓扩展；按上轮批准：预授权档（`never`/`on_failure`/`full_access`）放行并在时间线留痕，`on_request` 仍询问 |

## 7. 对齐后的档位表（已批准并落地）

| 顺序 | id | 捆绑 | 文案来源 | 性质 |
| --- | --- | --- | --- | --- |
| 1 | `read-only` | `read_only` + `on_request` | DSH 原文（仅可查看 / Read Only；只读查看，不产生任何写入） | DSH 已知档（本部署配置启用） |
| 2 | `workspace-write` | `workspace_write` + `on_request` | DSH 原文（工作区内修改 / Workspace Write；仅在工作区内读写） | DSH 出厂档 |
| 3 | `danger-full-access` | `danger_full_access` + `never` | DSH 原文（完全权限 / Full access；不受工作区边界限制）+ DSH 确认弹窗 | DSH 出厂档 |
| 4 | `workspace-write-auto` | `workspace_write` + `never` | **自拟**（文案见下；内部标扩展，界面不展示） | 扩展（DSH 无） |
| — | `custom` | 派生 | DSH 原文（Custom） | 派生状态，不可选 |
| — | `unavailable` | 派生 | 本仓原文（读取失败 / Unreadable） | 本仓扩展 |

`workspace-write-auto` 文案（已定稿并落地）：
- 中文名：**工作区内自动**；英文名：**Workspace Write (auto)**
- 中文说明：**在工作区内读写与执行，不再逐次确认**（破坏性操作仍由 `decide` 拒绝，见 §3 的 `danger-full-access` 行同一套 fail-closed）
- 英文说明：**Read, write and run inside the workspace without per-action confirmation**
- 语言覆盖：除 `zh` / `en` 之外的 6 套语言里 **`ja` 已译**；**`es` / `fr` / `de` / `ru` / `pt` 五套走英文原文回落**（与上游 `auto.*` 只译 `ja` 的做法一致）。
  这是**刻意安排，不是漏翻**——看到半套翻译不要去补；要补就先把 `auto.*` 一起补齐，
  并同步改 `docs/07` 第 30 行（那里是给同步上游的人看的同一句话）。

## 8. 这次对齐对上一轮方案的影响

1. 预设 id 保留 `read-only` / `workspace-write` / `danger-full-access`，
   `danger-full-access` 的捆绑正好就是 DSH 出厂值（`never`），无需发明。
2. `workspace-write-auto` **不是 DSH id**，保留为扩展，但「扩展」只写在本表、
   代码注释与 `docs/07`，**界面文案不出现该字样**；也不能把它说成 DSH 的 `auto`
   （`auto` = 无沙箱 + 同模型审查，语义完全不同）。
3. `id_of` 在非规范组合下必须返回 `custom`，不得回落成某个具体预设；
   **用例覆盖 `on_failure`、`full_access` 与任意混搭，且断言 `custom` 不可选**。
4. 不实现、不展示 `auto`；DSH 的 `custom` 作为派生值补齐。
5. ④-1：`workspace-write` 对齐 DSH——`decide(WriteInWorkspace, OnRequest, WorkspaceWrite)`
   改为 `Allow`；合并前必须补齐 §4.1 的三条路径测试，并把硬链接/TOCTOU 残余边界
   留在本表。`Execute` 不变（仍更严）。
6. ④-2：`read-only` 保留现状（执行仍拒绝），表中标「我们更严」，不得写「一致」。
7. 其余（两笔提交范围、PlanReview 放行 + 时间线留痕、`job_*` 分级、
   守卫测试先断言 20 个工具）不变。

## 9. 实施状态（2026-11-11）

**落点**

- 预设表：`src-tauri/src/permission/preset.rs`（`PRESETS` 四档 + `CUSTOM` 派生）；
  前端同一张表：`src/transport/permission-presets.ts`（`PRESET_BUNDLES` / `PRESET_TO_SANDBOX`）。
- 三条单一写入面：设置页 `save_settings { permission_preset? }`、会话 `set_session_permission_preset`、
  任务 `send_task { permission_preset? }`。都不接受「只改沙箱」或「只改审批」。
- `set_approval_mode` 已删（死代码，且是唯一能只改一维的入口）。
- 档案：新事件 `PermissionPreset { preset, at_ms }`；`SandboxMode` 降为只读兼容
  （`last_permission_preset` 无预设事件时用「旧沙箱档 + 当前部署审批档」精确匹配，
  不命中如实 `custom`）。

**裁决落地**

- ④-1（`workspace-write` 区内写入对齐 DSH）在 `4573527` 落地，三条路径测试（绝对路径、
  已存在软链接、中间层 junction）同笔。
- ④-2（`read-only` 仍拒绝执行）保留，§2 与 §3 标「我们更严」。
- 计划态是 Read 档、计划模式下不能执行命令，是 ④-2 的副作用，单独立项**待议**，不在本轮。

**残余边界**：§4.1 的硬链接与 TOCTOU 两条不因本轮改动消失，仍按原文记录。

**本文件随第二笔入库**：档位语义的唯一事实源只躺在工作区就等于没有——下一个人改档位时
不会读到它，也就不会知道自己正在违反一条已批准的裁决。
