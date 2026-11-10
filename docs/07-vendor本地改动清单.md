# vendor 本地改动清单（DSH）

> 状态：活文档｜上游基线：`D:\AI\deepseek-harness` @ `fcc0ad4621`（2026-11-04）
> 作用：跟上游同步前，先看这份清单。**凡是改动 `src/dsh/` 下上游文件的提交，必须在同一笔里追加一行**；只读、未改的上游文件不登记。

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
| 4 | `packages/client/ui-sidebar-terminal/src/client/{terminal.module.css,TerminalCleanup.module.css,TerminalTitle.module.css}` | `--dsw-alias-bg-l1/-2` → `--dsw-alias-bg-layer-1/-2` | 上游 CSS 引用的令牌在真实主题里不存在，面板背景透明；由 `src/theme-tokens.spec.ts` 机检兜底 | SH-071 纠错、fcc0ad4621 |
| 5 | `packages/client/ui-primitives/src/AnimatedRows.tsx`、`packages/client/ui-workspace/src/client/rows/WorkspaceBrowser.tsx` | 修复 UIA Expand 导致的渲染进程卡死；补 ARIA 树键盘模型 | Windows 辅助功能树展开会触发上游动画路径的重入；键盘可访问性缺失 | SH-101（ff384d9） |
| 6 | `packages/client/ui-approval/src/client/ApprovalCard.tsx` | 默认焦点落在「拒绝」；修复 `allow-once` 被记忆为永久放行 | 审批默认必须是安全侧；一次性放行不得降级成 `allow_always` | SH-025（5d5342a） |
| 7 | `packages/client/ui-settings-general/src/client/{SettingsRoot.module.css,chrome.module.css}` | 设置按钮紧凑化并右对齐 | 用户明确要求；纯样式 | a10cd6c |
| 8 | `packages/client/ui-settings-models/src/client/ModelsSection.tsx` | 恢复上游的 `intro` 段落（2 处，空态与主态各一）；文案走词典 `intro`（`zh` / `en`），不写在 JSX 里 | 此前 vendor 导入时被移除，导致模型设置页从 `h2` 直接跳到卡片、缺一句页面说明；恢复的是上游原节点，不是新增 | 2026-11-10（v0.2.1 视觉对齐批，04） |
| 9 | React 19 / 严格类型门禁批次（28 个文件，见下） | 类型级修复：显式返回类型、`useRef` 初值、可选属性收窄、`readonly` 对齐；不改运行行为 | `tsc -b` 恢复零错（原 246 错）；React 19 类型定义更严 | 97b5250 |

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

## 同步上游时怎么用

```powershell
# 1) 看本仓库改过哪些 vendored 文件
git log --diff-filter=M --name-only -- src/dsh

# 2) 对照上游基线（路径按包替换）
git diff --no-index "D:\AI\deepseek-harness\packages\<pkg>" "src\dsh\packages\<pkg>"

# 3) 上游已修的项：删本表行 + 移除本地补丁；上游新增的扩展点：优先改走扩展点
```

新增/调整本表时，同一提交内更新，不要事后补记——事后补记等于没记。
