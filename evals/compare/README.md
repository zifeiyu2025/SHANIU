# 任务级能力基线对照（compare）

回答一个问题：在**同一批真实仓库任务、同一个模型**上，我们的 harness 与主业 harness（codex / dsh）
差多少、差在哪一层。

本目录只放对照跑法本身；题面与判据复用 `evals/repo-tasks.json` 与 `evals/repo/judge/`。

## 口径（已确认）

| 维度 | 口径 |
|---|---|
| 模型 | `deepseek-flash`（全量）；`deepseek-v4-pro` 只在 flash 失败的题上跑一次，用来区分「模型不够」与「harness 不行」 |
| 协议 | 三方统一 **Responses**：SHANIU `openai-responses`、codex `wire_api="responses"`、dsh `openai-responses` |
| 端点 | `https://api.deepseek.com/v1`，密钥只从环境变量/本机密钥库读，不入库不入日志 |
| 题集 | 12 题 = 10 题复用 + 2 题新增（`repo-bug-search-surface-lost`、`repo-bug-cut-pairing`）；新增题均已过「红→绿」验证，判据未放宽 |
| 轮次 | 全量 1 轮；差距 Top3 各定向复核 2 次 |
| 权限 | workspace-write + 不询问（SHANIU `SandboxMode::WorkspaceWrite` + broker 自动 Allow；codex `--sandbox workspace-write --ask-for-approval never`；dsh `--permission-mode workspace-write --approval-policy never`） |
| 判据 | 同一份 `evals/repo/judge/run.ps1`，退出码 0 且 stdout 含 `REPO-CHECK: PASS` |
| 判据落点 | judge 在**无 ACL 的副本**上跑（`robocopy /MIR /COPY:DAT`，排除 `target`/`src-tauri\target`/`node_modules`）。实测理由：dsh 的 Windows 沙箱会写 `Everyone: Deny DeleteSubdirectoriesAndFiles`，cargo 构建脚本删产物报 `os error 5`，原地判据会假红；副本同时挡住「预置构建产物骗判据」 |
| 前置红 | 每题 agent 前判据必须红，否则 `invalid`，不进分母 |
| 工作区 | 同一 `0bee58e` 快照 + 同一补丁/fixture + 同一 `AGENTS.md` 注记；启动前做工作区指纹校验，不一致即 `invalid` |

> SHANIU 既往用 `openai-chat` + LongCat 跑出的三轮分数，协议与模型都不同，**不作为本对照的历史基线**。

## 运行

```text
# 1) 先取「同一棵树」的权威指纹（任一臂 prepare-only 都行，准备逻辑必须逐字一致）
node evals/compare/run-arm.mjs --arm codex --tasks <id,...> --prepare-only \
  --out evals/results-compare/prep-<...>
# 2) 各臂都带 --expect-tree，不一致直接判 invalid 而不是事后人眼比对
node evals/compare/run-arm.mjs --arm <shaniu|codex|dsh|zcode> --tasks <id,...> \
  --expect-tree "<id>=<sha256>,..." --out evals/results-compare/<run-id>
```

工作区指纹 = `git ls-files -s` 全文（CRLF→LF）的 sha256，只含 blob 哈希与路径，不含时间，
所以能跨进程、跨 harness 比对。Rust 侧同一算法打在 `[prep] ... tree=` 日志里。

### 断点续跑

```text
node evals/compare/run-arm.mjs --arm codex --resume \
  --resume-ws "%TEMP%\shaniu-cmp-<旧运行>\ws" --expect-tree "<...>" \
  --out evals/results-compare/phase2-codex
```

- `--resume`：已存在 `<out>/raw/<arm>-<task>-r1.ndjson` 且 `.judge.log` 的题直接复用，
  **不重跑 agent**（重跑会覆盖事件流）。续跑记录标 `resumed: true`，`wall_ms` 留 null。
- `--resume-ws`：旧工作区根目录。判据日志说红、但工作区还在时**只重跑判据**（结果写
  `.resume.judge.log`，原 `judge.log` 保留作证据）——用于纠正「判据自己跑挂了」的假红，
  例如磁盘满导致的 `os error 112`。该题重判后旧工作区立即删除，给后面的题腾空间。
- 每题完成打一行 `[cmp] done <task> <STATUS> wall=… in=… out=… diff=… @ <ISO 时间>`，
  同时追加到 `<out>/progress.log`；开跑前打 `[cmp] 预算：N 题 × 单题上限 …`；
  正常结束、异常退出、Ctrl+C 都有 `[cmp] 收尾：…` 行。
- 判据副本（2~4GB/份）判完即删，12 题不再累积占用。

### zcode 臂（无 npm 发布包，只能源码构建）

`@zcode/cli` 的 `package.json` 是 `private`，仓库里也没有预编译产物，因此这一臂**必须**从源码
构建（这是唯一一处与「一律用官方预编译包」不同，理由即「没有可装的包」）：

```text
cd D:\AI\ZCode
pnpm install --frozen-lockfile
pnpm run build:sea:prepare          # 先构建全部 workspace 依赖，再构建 @zcode/cli
node apps\zcode-cli\packages\cli\dist\zcode.cjs --version
```

它没有 `--model` / `--base-url` 参数，模型与端点在「个人 provider 配置」里；驱动器会写到
`target/zcode-personal/provider_config.json`（`target/` 不入库，key 不落仓库、不落日志），
并用 `ZCODE_PERSONAL_PROVIDER_CONFIG_FILE` / `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` 注入：
`api.type=openai-responses`、`baseUrl=https://api.deepseek.com/v1`、`personalModelIds=["deepseek-flash"]`。
内置目录里的 DeepSeek 走 `anthropic-messages`，**不能**拿来做「统一 Responses」对照。
无头口径：`-p <prompt> --cwd <ws> --mode yolo --output-format stream-json --no-browser`。

> 已知坑：Rust 的 `EVAL_AGENTS_NOTE` 字符串用行尾 `\` 续行，**会吃掉续行的前导空白**。
> Node 侧重写这段注记时必须同样不缩进，否则 `AGENTS.md` 的 blob 哈希不同，指纹闸门会直接判不一致。

## 反作弊（写死）

- `guards.json` 覆盖的 tail_hash / whole_hash / assert_min 区、判据脚本、fixture 落盘的验收用例，
  任何改动一律该题判 `invalid`，只有题面明确允许改的部分除外。
- 新增题在题面里同样写明这一条。
- judge 与 `guards.json` 始终在**工作区之外**执行。

## 保留 diff

每题跑完记录：改动文件列表、每文件增删行数、是否触碰 `forbid_paths`、以及完整 patch 路径。
自主完成率相同但改动量差两个数量级时，不视为平手。

## 指标

- 自主完成率 = 判据通过 / 有效题
- 人工介入：非交互跑法下无真人，用代理指标——审批/权限拒绝次数、打断次数、自动纠错轮次、工具报错次数
- token：SHANIU 从 `EventMsg::TokenUsage` 累加；codex 取 `codex exec --json` 的 `turn.completed.usage`；
  dsh 取 `--json` 事件/持久化 session，取不到标 `N/A`，不估算
- 墙钟：整题 / agent 段 / 判据段分列；判据与编译耗时单列，**不计入差距**
- 失败归因：模型能力 / 工具设计 / 权限或沙箱 / 上下文管理 / harness bug

## 阶段

1. **Phase 0**：`repo-doc-max-turns-drift` 三方各 1 次，只验证「同模型 + 同判据 + token 采集」链路通。
2. **Phase 1**：再加 `repo-bug-compaction-tail`、`repo-refactor-rename-title-api`，三方各 1 次；
   结束后先交中间数据（通过情况、token、墙钟、题集分辨力初判），不等 Phase 2。
3. **Phase 2**：全量 12 题 × 3 方 × 1 轮（SHANIU 臂等 Responses 修复提交号后，用同一套题补跑）；Top3 差距各复核 2 次。

止损：任一臂接不进同一模型 → 标 `BLOCKED`，不换模型硬凑；任一臂 Phase 1 有 ≥2 题因 harness 侧故障跑不完 → 先记录不进全量。

## 入库

- 脚本：`evals/compare/`
- 结果：`evals/results-compare/<run-id>/`（`report.md`、`summary.json`、精简事件流、diff 摘要）
- 全量原始事件流留在 `target/`，不入库
