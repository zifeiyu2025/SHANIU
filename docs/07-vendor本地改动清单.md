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
