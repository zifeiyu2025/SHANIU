# DSH 裁剪决策记录

> 适用对象：`src/dsh/` 的 vendor 副本，以及三张引用了它的映射表
> （`src/dsh/aliases.ts`、`src/dsh/tsconfig.base.json`、`tsconfig*.json`）。
>
> **结论先行**：映射表只允许指向真实存在的源码。新增映射与新增包必须同一次提交完成；
> 只加映射不拷代码，会变成下一处 `landlock-run`。

## 1. 为什么要这份记录

上游 `D:\AI\deepseek-harness\packages\` 有约 370 个包，本仓库按需 vendor 了 241 个。
`tsconfig.base.json` 是从上游整份拷贝的，裁剪包时没有同步修剪 paths，于是
**164 条映射指向不存在的源码**——「映射在、代码不在」成了常态，谁碰它都得重做一次审计。

这一轮把映射剪到只剩真实存在的条目，并把判断依据记在这里。目标是**让异常重新变成异常**：
之后再出现一条指向缺失源码的映射，就是新问题，而不是噪音。

## 2. 审计方法（下次照做）

判定标准只有一条：**把映射目标（文件 / 目录 / 通配前缀）拿去 `existsSync`**。
对 `paths` 的多值条目，任一目标存在即算有效。

两个踩过的坑，必须避开：

1. **JSONC 去注释不能用正则。** `paths` 的键里带 `/*`（形如 `ui-*/client`），
   正则 `/\/\*[\s\S]*?\*\//g` 会把键里的 `/*` 当成块注释起点，在 `*/` 处闭合，
   吞掉中间整段 JSON —— 第一版因此得出 105 条的错数。正确做法是字符串感知的扫描：
   跟踪 `inString` / `escape`，只在字符串外剥 `//` 与 `/* */`。
2. **提取包根不要用偏移判断。** `$path.IndexOf('/src/') -gt 0` 在路径以 `/src/dsh` 开头时
   返回 0，条件恒假，整批被静默跳过。用正则锚定包根，如
   `^(/src/dsh/(?:packages/[^/]+/[^/]+|vendor/[^/]+))`。

脚本属一次性工具，按 AGENTS §3 收在 `%TEMP%`，不留在仓库；上面两条足以复现。
守门测试 `src/dsh-mapping.spec.ts` 会持续检查同样的不变量（见 §6）。

## 3. 修剪结果（2026-11-09）

| 文件 | 修剪前 | 修剪后 | 删除 | 说明 |
| --- | --- | --- | --- | --- |
| `src/dsh/tsconfig.base.json` | 572 条 | 408 条 | 164 条 | 指向未 vendor 的包；vitest 的解析门面 |
| `tsconfig.dsh-vendor.json` | 15 条 | 12 条 | 3 条 | 全部是 `dsh-client-ui-message-feedback`（未 vendor） |
| `tsconfig.json` | 705 条 | 346 条 | 359 条 | 类型缓存声明表；死项指向 `dsh-types-client` 下未生成的 d.ts |
| `src/dsh/aliases.ts` | 494 条 | 494 条 | 0 条 | 早已只映射存在的包；本轮只加 `landlock-run` 防护 |

验证：`pnpm typecheck`、`pnpm build`、`pnpm test` 全绿（生产图与类型面都未断）。

## 4. 裁剪分组与判定依据

包名省略 `@deepseek-ai/dsh-` 前缀；`(/)` 表示同一包的多个子路径。

### A. 宿主能力已在 Rust 侧实现

- 会话与存储：`session-persistence-jsonl`、`session-query-sqlite`、`session-snapshot`、
  `session-checkpoint-policy`、`session-telemetry`、`session-telemetry-otel`、
  `session-log-deepseek`、`storage-json`、`storage-sqlite`、`session/chunk-rows`
- 执行与工具：`terminal-bash`、`pwsh-local`、`pwsh-sandbox`、`tool-bash-persistent`、
  `tool-pwsh`、`tool-pwsh-persistent`、`tool-terminal`、`tool-fs-search`、
  `tool-str-replace-editor`、`tool-web`、`tool-working-directory`、
  `tool-workspace-dependencies`、`tool-schedule`、`tool-goal`、`tool-session-query`、
  `tool-skill`、`tool-lsp`、`tool-ralph`、`tool-cordis(/host)`
- 上下文与约束：`compaction-basic`、`compaction-tool-result-pruner`、`authorization(/types)`、
  `fs-observation-policy`、`time-context`、`tmux-context`、`repeat-tool-reminder`、
  `goal-round-driver`、`file-reference-local`、`deliverable-file-references`、
  `message-quotes(/client)`、`harness-dev-context`
- 呈现与元编程：`agent-preset`、`agent-preset-registry/tools`、`agent-tool-presentation`、
  `agent/brand`、`typert-generator`、`typert-loader`

**依据**：`src-tauri/src/` 下已有对应模块（`session_query`、`rollout`、`tools`、
`tool_router`、`compaction`、`agent_loop`、`permission`）；`src/` 侧无 import。
**理由**：能力已移到宿主，前端不需要这些 Node 实现。

### B. 客户端 UI / 设置面板未接线

`ui-settings-account(/client)`、`ui-settings-agent-loop`、`ui-settings-plugin-inventory`、
`ui-settings-plugins`、`ui-settings-session-log`、`ui-settings-shell`、`ui-settings-subagent`、
`ui-settings-web-search`、`ui-shortcuts`、`ui-agent-preset`、`ui-agent-team(/client)`、
`ui-brand-official`、`ui-cordis(/client)`、`ui-message-feedback`、`ui-plugin-market`、
`ui-creation-recommendations(/client)`、`ui-plugin-recommendations(/client)`、
`plugin-recommendations(/types)`、`creation-recommendations(/types)`、
`plugin-package-inventory-deepseek`、`skill-badge`、`skill-office`、`persona`

**依据**：`src/dsh/packages/client/` 下没有对应目录；我们自己的设置入口只有
`model-config`、`mcp-trust` 两张卡。**理由**：未接线的面板不 vendor，避免死 UI。

### C. 实验性、未立项

全部 `experimental-*`：`browser-use-chrome-devtools-mcp`、`browser-use-playwright-mcp`、
`browser-use-runtime(/mcp)`、`browser-use-stagehand-native`、
`client-ui-claude-code-mods(/client)`、`client-ui-forge(/client/types)`、
`client-ui-stats(/client)`、`client-ui-voice-input(/client)`、
`computer-use-cua-driver-mcp`、`computer-use-cua-driver-native`、`cua-macos`、
`forge(/github/session/types)`、`inspector(/client)`、`liveshare/viewer-declarations`、
`ptc-runtime-python`、`session-inspector(/client)`、`speech-to-text-sensevoice`、
`subagent-oar`、`tool-subagent-oar`、`webworker-packer`、`webworker-runtime`

**依据**：`docs/03-三期需求文档.md` 属远期规划池，禁止提前实现。
**理由**：未立项的实验能力不进入 vendor。

### D. 远端 / SSH / 网络通道未启用

`ssh(/protocol/schemas)`、`ssh-helper-runtime`、`sandbox-ssh`、`subprocess-ssh`、`fs-ssh`、
`remote-mock`、`acp`、`subagent-acp`、`webhook`、`webhook-github`、`web-deepseek-api`、
`web-fetch-http`、`web-search-deepseek`、`browser-use(/brand)`

**依据**：`src/transport/` 只有 `tauri` 与 `mock` 两种传输，没有远端/SSH 模式。
**理由**：没有消费者，路径不存在。

### E. 账号、模型供应、会话标题

`deepseek-account-platform(/types)`、`llm-pi-ai`、`session-title-all-prompts-llm`、
`session-title-first-prompt-llm`、`session-title-llm`

**依据**：账号只有 DPAPI 本地存储（AGENTS §3）；标题生成未接入模型侧。
**理由**：与当前凭据/模型链路无交集。

### F. 插件协议与 HMR

`hook-protocol`、`hooks-claude-code`、`hooks-codex`、`cordis-plugin-hmr`、
`cordis-plugin-logger-console`、`client-hmr`

**依据**：`docs/02-二期需求文档.md` 的 M5–M7 不含 hooks/HMR；`src/dsh/vendor/` 只
vendor 了 7 个 cordis 插件，不含 hmr 与 logger-console。**理由**：未立项。

### G. 测试支撑

`agent-loop-testkit`、`client-test-runtime`、`tool-testkit`、`llm-mock-server`、`llm-replay`、
`loader-smoke`

**依据**：只被上游测试引用；本仓库 vitest 通过 `exclude: ['src/dsh/**']` 不跑上游测试。
**理由**：测试支撑不进入产品图。

### H. 目录选择器的宿主侧

`host-directory-picker-auto(/*)`、`host-directory-picker-browse(/*)`、
`host-directory-picker-native(/*)`

**依据**：客户端侧 `client/ui-directory-picker-browse`、`ui-directory-picker-native` **已 vendor**
并在 `dsh-boot.ts` 挂载；宿主侧由 Rust 命令实现（`src-tauri/src/lib.rs` 的目录选择器命令）。
**理由**：只裁宿主实现，客户端面板保留。

### I. CLI 入口

`profile-boot`、`lib/bin.js`

**依据**：桌面端不发布 CLI，`apps/cli` 未 vendor。**理由**：无消费者。

### J. 子代理的 JS 实现

`subagent-dsh-sdk`、`subagent-fork-in-process`

**依据**：子代理注册表在 `src-tauri/src/subagent_registry/`（二期），实现走 Rust。
**理由**：JS 实现与宿主注册表重复。

## 5. 已知残缺与例外

这一节登记**还没修、但已确认不炸**的缺口。新增一处必须写在这里，否则视为回归。

1. **`landlock-run` 的显式防护（唯一允许的映射不一致）**
   - `src/dsh/packages/sandbox/sandbox-local/src/index.ts:34` 与 `profiles.ts:7` 静态
     import `@deepseek-ai/node-addon-system/landlock-run`。
   - Landlock 是 Linux 专有 LSM，原生平台包从未 vendor；补真别名等于把 Linux 原生依赖
     拉进 Windows 构建。
   - 处理：`aliases.ts` 把它指向 `src/dsh/shims/landlock-run.ts`，任何调用立即抛出
     「landlock-run 不可用：Landlock 是 Linux 专有 LSM……Windows 请走 windows-acl 沙箱链」。
   - **类型面仍指向真实声明**（`src/dsh/tsconfig.base.json`），否则 `sandbox-local` 的
     类型会退化成桩——这是有意为之的唯一一条「别名 ≠ paths」，由
     `src/dsh-mapping.spec.ts` 钉住，不要当成不一致去"修"。
   - 未改成条件导入的原因：`landlockLauncher()` / `probe()` 是同步接口，改异步会牵连
     `confine()` 的同步分支，改动面大于收益；换成桩后引用链上已无 Linux 原生依赖。

2. **`session-log-export` 的悬空类型导入**
   - `src/dsh/packages/session-query/session-log-export/src/client/index.ts:8`：
     `import type {} from '@deepseek-ai/dsh-client-ui-message-feedback/client'`，
     该包未 vendor。
   - 运行期被擦除，且 `files` 列表不含该文件，typecheck 不报；`pnpm build` 通过即证明
     产物无影响。**判断：保留**（属于上游源码原样移植，删掉会加大 vendor 同步 diff）。

3. **`src/dsh/packages/bundle/web-app/package.json` 的 45 条死依赖**
   - 该清单声明了 `@deepseek-ai/schemastery`、`commander`、`open` 等未安装的包。
   - pnpm 只安装根 `package.json`，`src/dsh` 不是 workspace，因此这些条目**不产生安装**，
     是惰性声明。**判断：不清理**——它属于上游清单原样移植，删除会让 vendor 同步 diff
     变大；等真正需要该清单驱动构建时再按需重建。

4. **`tsconfig.json` 是类型缓存声明表，不是源码表（本轮这一刀可回滚）**
   - 其中的 346 条映射指向 `node_modules/.cache/dsh-types-*` 的生成 d.ts，由
     `pnpm typecheck`（`tsc -b --force`）产出；缓存目录本身不进版本库。
   - 本轮把从未产出 d.ts 的 359 条剪掉（705 → 346）。
   - **怎么回**：`git checkout 466192d^ -- tsconfig.json`
     （等价：`git show 466192d^:tsconfig.json > tsconfig.json`）。
   - **回滚后 `pnpm typecheck` 仍然通过**——已实测（2026-11-09，`tsc -b --force` 退出码 0）。
     原因：那 359 条的目标确实不存在，但 `paths` 只在对应模块被 import 时才参与解析，
     这些包没有任何源码引用，条目纯属惰性声明。
   - **回滚的代价**：回到「映射在、代码不在」的常态；这 359 条不再被任何检查覆盖
     （`src/dsh-mapping.spec.ts` 只守 `aliases.ts` 与 `tsconfig.base.json`，
     不检查这张缓存表），下次审计又得重数一遍。
   - 什么时候该回滚：只有当上下游工具链开始直接读这张表、或者某天这些 d.ts 真的产出时。
     回滚本身不会让检查变红，所以"不敢动"不是理由，想留就留、想剪就剪。

## 6. 维护规则

1. **加包三件套**：拷源码 → `aliases.ts` 加别名 → `tsconfig.base.json` 加 paths，同一次提交。
   若包只用于类型（不进 bundle），可以不进 `aliases.ts`，但必须在这里登记。
2. **删包三件套**：源码、别名、paths 一起删；`tsconfig.json` 的缓存项在下次
   `pnpm typecheck` 后按需再剪。
3. **不允许"先加映射、回头再放代码"**：`src/dsh-mapping.spec.ts` 会对两侧逐条
   `existsSync`，缺一条就红。
4. **`aliases.ts` 现已手工维护**（上游生成脚本不在本仓库）；改它时同步看
   `tsconfig.base.json`，反之亦然。
5. 唯一允许的不一致是 §5.1 的 `landlock-run`，且由测试钉住两侧的具体取值。
