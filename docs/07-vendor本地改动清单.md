# vendor 本地改动清单（DSH）

> 状态：活文档｜上游基线：`D:\AI\deepseek-harness` @ `fcc0ad4621`（2026-11-04）
> 作用：跟上游同步前，先看这份清单。**凡是改动 `src/dsh/` 下上游文件的提交，必须在同一笔里追加一行**；只读、未改的上游文件不登记。
> 修订：2026-11-11，依据 `docs/09` 首轮账本核对（上游 `fcc0ad4621`，逐文件 sha256 比对）——#9 补登 7 个漏登文件；#8 `ModelsSection.tsx` 标注退役（已与上游一致）；#4 移除从未改动的 `TerminalGuide.module.css` 并把计数更正为 3 个文件 5 处。这份清单此前有漏登与多列，不是天生准确。

## 为什么需要这份清单

缝合项目最容易死的地方，是升级上游时不知道哪些是自己改的：一次 `copy -Recurse` 覆盖，所有本地修复静默消失，而且没人记得曾经有过。症状通常延迟出现（终端空白、侧栏卡死、按钮错位），排查时又会误以为是上游回归。

规则：

1. 改 `src/dsh/` 下**上游已有文件** = vendor 改动，必须登记。
2. 能走扩展点解决的，不许改上游文件（例：SH-132 的能力视图走 `settings.models.provider-card`，见 `src/model-capabilities/`）。
3. 同步上游时逐行对照本表；上游已修的同项，删除本表行并移除本地补丁。
4. 登记粒度 = 文件；同一提交改多个文件可共用一行说明，但文件必须逐个列出。

## 清单

| # | 文件（`src/dsh/` 下） | 改了什么 | 为什么 | 对应工单 / 上游 |
| --- | --- | --- | --- | --- |
| 1 | `aliases.ts`（**本地新增，非上游文件**） | 维护 `@deepseek-ai/dsh-*` → 实际路径的别名表；多笔提交持续追加 | 只 vendor 了用到的包，入口需要显式映射；上游没有等价物 | SH-071/088/089/090/091/092/093/109、0238af4 |
| 2 | `tsconfig.base.json` | 别名映射剪到真实存在的条目 | 映射到不存在的包会让 `tsc -b` 报错，且掩盖真实缺包 | 466192d、1f5d13f |
| 3 | `packages/client/ui-sidebar-terminal/src/client/{face.ts,index.ts,terminal.tsx,TerminalCleanup.tsx,TerminalGuide.tsx}` | 把上游对 `@deepseek-ai/dsh-api-terminal-controller/{client,types}` 的导入改为本地 `./model.ts`；`index.ts` 的 `target()` 只接受终端形状参数 | 控制器包未 vendor（依赖宿主 PTY/终端服务），本地 `model.ts`（新增，约 146 行）实现同一契约；窄化参数避免把任意导航参数当终端请求 | SH-071（0dc03f0） |
| 4 | `packages/client/ui-sidebar-terminal/src/client/{terminal.module.css,TerminalCleanup.module.css,TerminalTitle.module.css}` | `--dsw-alias-bg-l1/-2` → `--dsw-alias-bg-layer-1/-2`（3 个文件 5 处） | 上游包间漂移：`fcc0ad4621` 的 `ui-theme` 已改名 `bg-layer-*`，而同一基线的 `ui-sidebar-terminal` 仍在引用 `bg-l1/-2`，面板背景透明。**下次同步**：先看上游 terminal 是否已改用 `bg-layer-*`——已改就删本地补丁；同步 theme 时反向核对，别把令牌名改回 `bg-l*`。由 `src/theme-tokens.spec.ts` 机检兜底 | SH-071 纠错、fcc0ad4621 |
| 5 | `packages/client/ui-primitives/src/AnimatedRows.tsx`、`packages/client/ui-workspace/src/client/rows/{WorkspaceBrowser.tsx,treeKeyboard.ts}`（`treeKeyboard.ts` 为本地新增） | 修复 UIA Expand 导致的渲染进程卡死；补 ARIA 树键盘模型（方向键/Home/End 在树内移动焦点，**比上游多**，见下方同步说明第 4 条） | Windows 辅助功能树展开会触发上游动画路径的重入；键盘可访问性缺失 | SH-101（ff384d9） |
| 6 | `packages/client/ui-approval/src/client/ApprovalCard.tsx` | 默认焦点落在「拒绝」；修复 `allow-once` 被记忆为永久放行 | 审批默认必须是安全侧；一次性放行不得降级成 `allow_always` | SH-025（5d5342a） |
| 7 | `packages/client/ui-settings-general/src/client/{SettingsRoot.module.css,chrome.module.css}` | 设置按钮紧凑化并右对齐 | 用户明确要求；纯样式 | a10cd6c |
| 8 | `packages/client/ui-settings-models/src/client/ModelsSection.tsx`（**已退役 2026-11-11**） | ~~恢复上游的 `intro` 段落（2 处，空态与主态各一）；文案走词典 `intro`（`zh` / `en`），不写在 JSX 里~~ | 退役理由：`docs/09` 核对实测该文件与上游 `fcc0ad4621` 逐字节一致，本地回填已与上游同形，不再构成 vendor 改动；同步上游时无需再逐行对照本行 | 2026-11-10（v0.2.1 视觉对齐批，04）；2026-11-11 退役 |
| 9 | React 19 / 严格类型门禁批次（35 个文件，见下；其中 7 个于 2026-11-11 补登） | 类型级修复：显式返回类型、`useRef` 初值、可选属性收窄、`readonly` 对齐；不改运行行为 | `tsc -b` 恢复零错（原 246 错）；React 19 类型定义更严 | 97b5250 |
| 10 | `packages/client/ui-layout/src/client/AppFrame.tsx` | `inert: 条件 ? '' : undefined` → `inert: 条件`（5 处） | React 19 已按布尔处理 `inert`；空串写法在新类型定义下不合法，语义等价，属 React 19 兼容修复 | 2026-11-10（0.2.2 类型门禁） |
| 11 | `packages/client/ui-permission-presets/src/client/locales.ts` | 在 8 套语言词典里各加 `preset.readOnly/workspaceWrite/fullAccess.description` 三键（共 24 条） | 三档的说明行是 UI 文案；此前由宿主目录下发（只有中文），en/ja/… 界面会串中文 | e9166a9（P3-16）。**同步上游时**：上游若已把这三句收进自身词典，删本地 24 条改指上游键；若上游改了预设机器值（`read-only` / `workspace-write`），`PRESET_DESCRIPTION_KEYS` 的映射必须同步 |
| 12 | `packages/client/ui-permission-presets/src/client/presentation.ts` | 新增 `PermissionPresetDescriptionKey`、`PRESET_DESCRIPTION_KEYS`、`DEFAULT_PRESET_DESCRIPTIONS`、`displayPermissionPresetDescription()`（词典缺失时回落 en 原文） | 把「挡位机器值 → 词典键」的映射与回落收在一处，UI 不必各自判断内置/自定义 | 同上。**同步上游时**：上游若自带等价映射函数，删本地实现改调上游；保留则只跟随键名变化 |
| 13 | `packages/client/ui-permission-presets/src/client/index.ts` | 新增 `presetDetail()`：下拉 `detail` 走词典，只有词典不认得的自定义挡位才回落宿主 `description` | 让宿主串只出现在兜底分支；否则删掉 `description` 后自定义挡位的说明会被一起吞掉 | 同上 |
| 14 | `packages/client/ui-permission-presets/src/client/PermissionSelect.tsx` | `optionDescription()` 从「只有 Auto 走词典」改为「Auto 走 `auto.description`，其余内置挡位走 `displayPermissionPresetDescription()`，自定义挡位回落宿主描述」 | 芯片 tooltip 与下拉必须同一取文案规则，否则同一挡位在两处说法不同（一处中文一处英文） | 同上。**同步上游时**：上游若已统一取文案，删本地函数改调上游 |
| 15 | `packages/client/ui-settings-general/src/client/index.ts` | 新增 `ctx.provide('settingsShell', { openSection })`，并把返回的 `disposeShell()` 加进拆卸函数 | 宿主「模型配置」座位（`src/model-config/`）要能直达设置-模型分段；上游只有 `settings.open` 快捷键，没有「打开到某分段」的入口 | b61d5e0。**同步上游时**：上游若提供等价入口，删本地服务并改调用方；保留时必须确认拆卸链仍调用 `disposeShell`（漏了会跨会话泄漏服务句柄） |
| 16 | `packages/spill/spill/src/types.ts` | `@deepseek-ai/dsh-session` → `@deepseek-ai/dsh-session/types` | 宿主域/浏览器域拆分后，包根入口把 Node 侧类型带进 client 编译域，`tsc -b` 报错 | 97b5250（类型门禁）。**同步上游时**：上游若已提供 browser-safe 子路径，按其路径改；否则保留 |
| 17 | `packages/web/web/src/types.ts` | `@deepseek-ai/dsh-llm` → `@deepseek-ai/dsh-llm/error` | 同上：只取 `HarnessError`，不拖入根入口的 Node 侧导出 | 97b5250。同上 |
| 18 | `packages/util/time/src/rrule.ts` | `import rrulePackage from 'rrule'` + 解构 → `import { Weekday, rrulestr } from 'rrule'`（含注释说明） | 上游依赖自家打包器做 CJS 互操作；SHANIU 的 Vite/rolldown 解析到 rrule 的 ESM 构建（只有命名导出），默认导入拿到 `undefined`，构建期即红 | a6da7e0。**同步上游时**：上游若改成命名导入，删本地改动；若上游仍用默认导入，保留（否则构建失败） |
| 19 | `packages/util/official-bundles/src/plugin-manager.ts` | `PROTECTED_PLUGINS` 去掉 `@deepseek-ai/dsh-client-ui-settings-plugin-inventory` | 该包已按「设置只留通用 + 模型」删除；保护清单留着不存在的名字，会让「受保护插件」判断指向空气 | b1f9fa7。**同步上游时**：上游若也删了该包，两边一致、无需保留；若上游重新引入，先确认是否要恢复名单 |
| 20 | `packages/client/ui-theme/src/client/styles.ts`（及 `tests/client-styles.client.spec.ts`） | 8 张主题样式表从 `document.createElement('style')` 改为构造式样式表（`new CSSStyleSheet()` + `document.adoptedStyleSheets`），新增 `themeStyleSheets()` 供用例按引用断言顺序/归属；用例断言对象由 `style[data-plugin]` 节点改为 `adoptedStyleSheets` | 打包构建下宿主 CSP 的 `style-src` 只放行静态资源内带 nonce 的标签，运行时新建的 `<style>` 被整表拦截（H2 实测 8 张全灭、界面近乎全黑）。构造式样式表不经 HTML/CSS 解析器，不受 `style-src` 约束，且无需放宽 `'unsafe-inline'` | 2026-11-10（H2 CSP 第二步）。**同步上游时**：上游若已改用构造式样式表或给 `<style>` 打 nonce，删本地实现改跟上游；同步时不要退回 `<style>` 注入，否则打包界面直接失效 |
| 21 | `packages/client/ui-primitives/src/markdown/{render.tsx,MarkdownDelegate.tsx,MarkdownText.module.css}` | 图片加载失败态由纯 `<span>` 改为：目标为绝对 http(s) 且宿主提供 `openExternal` 标签时渲染 `<button type="button">`（点击调全局 `window.open`，由装配期宿主垫片落到 `open_external_url`），其余目标保持纯文本；`fileImages.labels` 增加可选 `openExternal`；新增 `.imageAltAction` 样式 | SH-137：裸 `<a href>` 在 Tauri 里会把整个应用导航到外网，且用户明确要求「失败图片可点 + 非 http(s) 不渲染链接」；`<span>` 无焦点、无提示 | 2026-11-10（SH-137）。**同步上游时**：上游若已把失败图片做成可点，删本地实现；同步后必须确认仍是按钮/宿主 IPC 而非裸锚点 |
| 22 | `packages/client/ui-chat/src/client/{locale.ts,chat/ChatView.tsx}` | 8 套语言词典各加 `image.openExternal`（共 8 条）；`ChatView` 的 `fileImages.labels` 传入该键 | 失败图片的「在浏览器打开」提示是 UI 文案，不写死在 JSX；与 `image.*` 其余键同处维护 | 同上。**同步上游时**：上游若已加等价键，删本地 8 条改指上游键 |
| 23 | `packages/client/ui-conversation/src/client/contract/records.ts`、`packages/client/ui-chat/src/client/conversation-nodes/{turn-error.ts,event-projection.ts}`、`packages/client/ui-chat/src/client/chat/{MessageItem.tsx,MessageItem.module.css}`、`packages/client/ui-chat/src/client/locale.ts` | 上游行为：`TurnErrorNode` 只有一句 `message` + `code`，没有任何原始诊断。本地：`TurnErrorNode` 增加可选 `detail`（HTTP 状态 / 端点 code / 端点 message，宿主已脱敏），`displayFailure` 透传（AUTH 也保留 detail），`TurnErrorItem` 增加可展开的 `<details>` 与复制按钮，8 套词典各加 `message.turnError.detail/.copy/.copied`。**diff 形态 = 纯附加**：所有新字段可选，未提供时渲染与上游完全一致 | A0 走查：401 在 UI 上只有"本轮运行失败"，用户与支持都无法对账；一句人话与原始证据必须同时可见 | 2026-11-11（A0-2）。**同步上游时**：上游若已提供等价原始诊断，删本地字段与组件改动改跟上游；只保留 `detail` 的构造（`adapter.ts` 属外壳层，不在此表） |
| 24 | `packages/client/ui-chat/src/client/chat/{message-chrome.ts,TurnTriggerNodeView.tsx}`、`packages/client/ui-chat/src/client/locale.ts` | 上游行为：`formatMessageClock(time, …)` 无条件 `new Date(time)`，`0` 会显示成 1970 年；回合头 `<time dateTime={…}>` 同理。本地：`time <= 0` 视为「档案未记录时刻」，返回词典键 `clock.unreliable`（8 套语言各 1 条），回合头在未知时改用 `<span>` 不挂 `dateTime`。**diff 形态 = 纯附加分支**：`time > 0` 时与上游逐字一致 | A0 走查：旧档案没有事件时刻，回放曾用打开会话那一刻冒充，重开后时间全变成同一分钟；修复后新档有真实时刻，旧档必须明说「记录时刻不可用」而不是显示 1970 或假时钟 | 2026-11-11（A0-4）。**同步上游时**：上游若自己区分了「无时刻」，删本地分支改跟上游；同步后必须确认 `time = 0` 不再渲染成 1970 |
| 25 | `packages/client/ui-chat/src/client/chat/ApprovalCommand.tsx`（重写）、`packages/client/ui-chat/src/client/chat/ApprovalCommand.module.css`（**本地新增**）、`packages/client/ui-approval/src/client/{contract/slots.ts,index.ts,ApprovalPanel.tsx}`、`packages/interaction/user-approval/src/types.ts`、`packages/client/ui-chat/tests/approval-command.client.spec.tsx` | 审批详情从「只取 `args.command`」扩展为按工具结构化展示：**为什么不能只改适配层**——详情槽 `conversation.approval.detail` 在 `ui-approval` 中是**单定义**，重复注册直接抛错（`ui-slots` 的 register 语义），宿主插件无法再注册一个来覆盖 `ui-chat` 的实现；适配层只能补数据（`callId`、取完整参数），渲染必须改这一处 vendored 组件。展示内容：`apply_patch` 默认列出每个文件的路径/操作/±行数与 `@@` 头，命令类完整展示不截断，其余工具键值 + 原始 JSON 折叠；完整原文始终在卡片内可展开、可复制；解析失败显示原因与原文而不是空白。链路为 additive optional：`ApprovalRequestEvent` / `ApprovalPresentationRequest` / `PendingApproval` / `ApprovalCardInput` 各加 `description?`，`ApprovalPanel` 有值时透传；未提供时渲染与上游一致。已结束的 Tool call 在 paired `call` 仍带 `argsRaw` 时也提供详情（提权询问走这条路径） | 走查缺陷：`callId` 未传导致详情槽对所有工具失效，卡片只剩一个 160 字符的 JSON 前缀；`JSON.stringify` 把换行转义成两位字符，160 字符实际覆盖不到 80 个真实字符，`apply_patch` 连改哪个文件都看不全——用户批准了自己没看全的操作 | 2026-11-11（A0-T7 / D3）。**同步上游时**：上游若把审批详情做成结构化组件，删本地实现改跟上游；若上游仍只渲染 `command`，保留本地实现，并确认 `description` 仍是 optional（必填会破坏旧调用方） |
| 26 | `packages/client/ui-workspace/src/client/rows/WorkspaceBrowser.tsx`、`.../WorkspaceBrowser.module.css`、`.../locales.ts`、`.../contract/slots.ts`、`.../index.ts`、`packages/client/ui-workspace/tests/workspace-browser.client.spec.tsx`、`packages/api/session-controller/src/types.ts`、`.../client/sessions/manager.ts`、`.../client/contract/sessions.ts` | 会话搜索的失败与残缺显式化：`SessionSearchValue` 增加可选 `skippedSessions`、`SessionSearchRequest` 增加可选 `retry`（宿主无冷却时忽略）；`manager` / `ISessions` 透传该字段；浏览器结果区新增 `search.failed`（错误原因）与 `search.skipped`（「N 个会话无法检索」）两行，失败态提供「重试」（经注入的 `retrySearchIndex` 先清宿主失败记忆再重查），8 套语言各加 `search.failed/.skipped/.retry` 三键；`WorkspaceBrowserInjected` 增加可选 `retrySearchIndex`。**diff 形态 = 纯附加**：新字段全 optional，未提供时渲染与上游一致 | 用户明确要求（SH-113）：搜索失败不能被渲染成「无匹配会话」，残缺结果必须说清「这次不完整」；冷却期不能只有干等一条路，用户可能刚清出磁盘或解除占用。改 `session-controller` 契约是因为 `hasMore` 也会被 manager 重建，若只在适配层塞字段会在这一层被丢掉 | SH-113（2026-11-11）。**同步上游时**：上游若自己区分搜索失败与残缺，删本地分支改跟上游；同步后必须确认失败态不再落到 `search.noMatches`，且 `skippedSessions` 未被 manager 的重建逻辑丢弃 |
| 27 | `packages/client/ui-settings-models/src/client/{ModelsSection.tsx,ModelsSection.module.css,locales.ts}` | `.savedNotice` 由成功绿（`--dsw-alias-state-success-primary`）改为中性（`--dsw-alias-label-secondary`）；`credentialConfigured` 的 zh/en 改为「已配置（未校验有效性）」；账号行绿点由 `aria-hidden` 改为 `role="img"` + `title/aria-label={accountSignedIn}`。**6 套可选语言（ja/es/fr/de/ru/pt）的同键已同步**为「已配置（未校验有效性）」；这 6 套属遗留词典（语言行按已注册语言渲染，8 种都在可选列表里，见 `packages/client/locale/src/client/index.ts:583`），本卡不做全站翻译 | A0 走查：错 Key 保存后仍显示绿色「已保存」，把「写进本机」渲染成「校验通过」；绿点同病——用户会把它读成「密钥有效」。保存成功与密钥有效是两件事，有效性只在 `src/model-capabilities/` 的密钥三态（有效 / 无效 / 无法确认）里下结论 | 2026-11-11（A0-5）。**同步上游时**：上游若自己区分「已保存」与「已校验」，删本地改动改跟上游；否则保留，并确认 `.savedNotice` 没有退回 success 色、`credentialConfigured` 没有退回「已配置」这种会被读成「有效」的说法 |
| 28 | `packages/client/ui-permission-presets/src/client/presentation.ts` | 新增哨兵 `PERMISSION_UNAVAILABLE = 'unavailable'`（不在预置目录里、宿主从不写入，仅由适配层在「会话档位记录读不出来」时发出）；`PRESET_LABEL_KEYS` 增 `unavailable → preset.unreadable`，`DEFAULT_PRESET_LABELS` 同步补 en 文案。值必须与 `src/transport/permission-presets.ts` 的同名常量一致（`src/transport/permission-presets.spec.ts` 锁住） | 读档失败时若回落到某个具体档位，选择器会把「未知」渲染成「仅可查看」，用户随后会拿错误前提去确认下一条操作（D2） | 580d464（2026-11-11）。**同步上游时**：上游若已有等价哨兵（同名常量或同样的「不可读」机器值），删本地实现改跟上游；只同步词典键不够——必须确认上游确实不会回落到某个具体档位 |
| 29 | `packages/client/ui-permission-presets/src/client/locales.ts` | 8 个主字典（`settings.permission`）与 8 个 access 字典（`permission.access`）各加 `preset.unreadable` 一键，共 16 条：zh/en 为「读取失败 / Unreadable」，ja/es/fr/de/ru/pt 各自译文 | 与第 28 行配套：哨兵必须有话说；文案不能借用「仅可查看」，否则读档失败会被读成一种真实限制 | 同上。**同步上游时**：上游若已有等价哨兵与文案键，删本地 16 条改指上游键；只删文案不删哨兵会留下无话可说的档位 |
| 30 | `packages/client/ui-permission-presets/src/client/{presentation.ts,locales.ts}` | **2026-11-11 曾新增、2026-11-12 已移除扩展档部分**（历史如实保留，不抹掉）：当时 `presentation.ts` 加了 `WORKSPACE_WRITE_AUTO_PRESET = 'workspace-write-auto'` 与派生档 `CUSTOM_PRESET = 'custom'`，`locales.ts` 主字典各加 `preset.workspaceWriteAuto` / `preset.custom`，access 字典各加两组 label + `*.description`，共 48 条。移除后只保留 `custom`：`presentation.ts` 的 `WORKSPACE_WRITE_AUTO_PRESET` 与两套映射里的 auto 键删除；`locales.ts` 删 auto 相关 24 条（8 主字典 label + 8 access label + 8 access description），剩 `custom` 24 条。**`custom` 的说明文案语言回落是刻意安排，不是漏翻**：`ja` 已译，`es` / `fr` / `de` / `ru` / `pt` 的 `preset.custom.description` 走英文原文回落——与上游 `auto.*` 只译 `ja` 的惯例一致，看到半套翻译不要去补。**`custom` 只做展示、不进目录**：适配层与设置页都拒绝把它写入 | 宿主安全面是 `(sandbox, approval)` 两维，预设是两维的绑定。`workspace-write-auto` 是自造扩展档，2026-11-12 用户裁定「权限档位对齐 dsh 就行，不要搞复杂」后移除——想不被打断就选完全权限（DSH 同款答案），不再保留第四档。`on_failure`/`full_access` 等非标准组合（含删档后的 `workspace_write` + `never`）必须如实显示「自定义」，回落成具体档会让用户按错误的档位前提去确认操作 | 2026-11-11（权限预设对齐）；2026-11-12（用户裁定移除扩展档，提交说明写明「对齐 dsh、不保留扩展档」）。**同步上游时**：上游若引入同名扩展档或自定义派生态，先确认产品是否还要这一档（当前裁定是不要），再决定删本地键改指上游；无论如何都要确认 `PRESET_LABEL_KEYS` / `PRESET_DESCRIPTION_KEYS` 覆盖 `custom`、且 `custom` 不进 `PermissionCatalog.options` |


第 9 行的文件（全部只做类型修复）：

```
packages/api/gateway/src/client/index.ts
packages/api/session-controller/src/client/sessions/manager.ts
packages/api/session-controller/src/client/transport.ts
packages/client/file-upload/src/client/runtime.ts
packages/client/ui-chat/src/client/chat/{ChatView.tsx,flow-motion.ts,searchable-hidden.ts,text-interaction.ts,use-chat-scroll.ts,use-chat-viewport.ts,use-process-scroll.ts}
packages/client/ui-chat/src/client/contract/slots.ts
packages/client/ui-conversation/src/client/contract/slots.ts
packages/client/ui-conversation/src/client/input/editor/{DraftEditor.tsx,view-binding.ts}
packages/client/ui-conversation/src/client/skeleton/EmptyHero.tsx
packages/client/ui-primitives/src/markdown/useViewportHighlighting.ts
packages/client/ui-primitives/src/{useAnchoredMaxHeight.ts,useMenuSubmenu.ts}
packages/client/ui-session/src/client/index.ts
packages/client/ui-sidebar-browser/src/client/view/BrowserBody.tsx
packages/client/ui-sidebar-documentpreview/src/client/{image/ImageBody.tsx,pdf/pdf.tsx,zoom/ZoomViewport.tsx}
packages/client/ui-sidebar-right/src/client/{shell/SidebarRight.tsx,tab-domain.ts}
packages/client/ui-workflow-run/src/client/WorkflowRunPanel.tsx
packages/core/session/src/request-header.ts
```

第 9 行补登（2026-11-11，`docs/09` 首轮账本核对发现漏登；改动性质与上面同批，全部是 React 19 类型兼容）：

```
packages/client/ui-dockkit/src/components/TabLayout.tsx
packages/client/ui-primitives/src/HoverCard.tsx
packages/client/ui-primitives/src/JsonTree.tsx
packages/client/ui-primitives/src/Menu.tsx
packages/client/ui-primitives/src/TextShimmer.tsx
packages/client/ui-primitives/src/Tooltip.tsx
packages/client/ui-primitives/src/image-drag.ts
```

## 有意偏离（非 `src/dsh/` 文件）

这些文件不在 `src/dsh/` 下，不算 vendor 改动；但对齐上游时故意少做了一部分，同步/对照 UI 时不要按「缺了就补」处理。

| 文件 | 偏离 | 理由 | 时间 |
| --- | --- | --- | --- |
| `src/header-actions/ExplorerButton.tsx`、`src/header-actions/explorer-button.css` | 保留 DSH `ui-open-in-app/OpenTargetButton.module.css` 紧凑态的 24px 高 / 11px / 0.5px `border-l4` 描边，但**去掉 chevron 半区与其分隔线**，整颗按钮一个点击区 | 上游那半个 chevron 是真实的下拉入口；本项目只有一个打开目标（系统文件管理器），画出来却不能展开属于无效交互诱导，比没有更糟 | 2026-11-10（v0.2.1，08 修） |
| `src/header-actions/session-events.ts` | 导出的 JSONL 事件在 `time` 之外多一个可选 `timeUnreliable: true`（**只在档案没有事件时刻时出现**）；同时 `time` 取档案 `at_ms`，不再取导出那一刻 | 旧档案没记事件时刻，`time` 只能写 0；不加标记就等于把 0 当成真实时间交给审计。字段是附加的，上游 formatter 不读；有真实时刻时输出的键与上游一致 | 2026-11-11（A0-4）。**同步上游时**：上游事件模型若支持"时刻未知"，改用上游字段；否则保留，并在导出说明里维持该键 |

## 同步上游时怎么用

```powershell
# 1) 看本仓库改过哪些 vendored 文件
git log --diff-filter=M --name-only -- src/dsh

# 2) 对照上游基线（路径按包替换）
git diff --no-index "D:\AI\deepseek-harness\packages\<pkg>" "src\dsh\packages\<pkg>"

# 3) 上游已修的项：删本表行 + 移除本地补丁；上游新增的扩展点：优先改走扩展点
# 4) WorkspaceBrowser 的树形键盘导航是本地增补：上游只有鼠标展开/收起。方向与
#    「对齐 DSH」相反，但只增不减、不影响上游行为，同步时保留；若上游后来自己
#    加了键盘模型，删本地 treeKeyboard.ts 并改用上游实现。
```

新增/调整本表时，同一提交内更新，不要事后补记——事后补记等于没记。
