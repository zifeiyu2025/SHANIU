# 真实仓库端到端评测（repo eval）

这套评测回答一个很窄的问题：**在当前代码库上，SHANIU 自己能不能把一件真实的活干完。**
判据全部是仓库自带的真实用例或真实脚本，不靠人看。

## 形态

- 基座：**固定提交 `0bee58e` 的快照**，不跟随 `HEAD`。`main` 前进不会让题集失效。
- 题目：把一条真实交付过的行为回退掉（bugfix）、给一条缺失行为加验收用例（feature）、
  制造一次半完成的重命名（refactor）、制造文档与代码的偏差（doc-sync）。
- 判据：每题一个 judge key，由 `evals/repo/judge/run.ps1` 执行，成功 = 退出码 0 且输出 `REPO-CHECK: PASS`。
- 防作弊：judge 与 `guards.json` 放在**工作区之外**；工作区的 git 历史里只有「本题初始状态」一条提交，
  `git show` 拿不到答案；测试区/判据文件被改会被守卫直接判失败。

## 评测环境注记（工作区副本的 `AGENTS.md`）

评测里的命令跑在 AppContainer 内。全量 `cargo test` 有 41 条用例在**该环境内**必然红，
全部落在 `sandbox::*`、`tools::terminal`、`tools::unified_exec`：它们要在沙箱里再创建受限令牌、
写 DACL，嵌套必然被拒（`0x80070005`、`CreateRestrictedToken` 参数错误 `0x80070057`、DACL 写拒绝）。

- **不是回归**：同一测试二进制在宿主机上 `cargo test --lib -- sandbox:: tools::terminal tools::unified_exec`
  的结果是 108 passed / 0 failed / 2 ignored；CI 也是绿的。差异只来自容器嵌套，不来自代码。
- **处理方式**：`prepare_workspace` 在**每题基线提交之前**把这段事实与替代命令
  （`cargo test --lib <模块前缀>`、`cargo test --test <名>`、`cargo check --all-targets`）幂等写进
  工作区副本的 `AGENTS.md`。仓库里的 `AGENTS.md` 不动，判据与 `guards.json` 不动，通过率口径不动。
- **为什么不让这些用例「变绿」**：给它们加环境跳过 = 在真机上同时失去 41 条覆盖；改判据 = 放松。
  让评测环境说清「哪些红是环境造成的、该跑什么」才是信息增量，且不掩盖真实回归。

依据与实测见 `RESULTS-2026-11-13-c.md`「评测自身的问题」第 2 条与「失败分层归因」。

## 运行

```powershell
# 1) 管线自检（不需要密钥；验证 快照→补丁→预热→判据必须红）
cargo test --manifest-path src-tauri/Cargo.toml --test evals_repo evals_repo_pipeline_selfcheck --offline -- --ignored --nocapture

# 2) 基线（需要 SHANIU_TEST_API_KEY 等端点变量；repeats 默认 3）
cargo test --manifest-path src-tauri/Cargo.toml --test evals_repo evals_repo_v0 --offline -- --ignored --nocapture

# 3) 读报告
node evals/score-repo.mjs evals/results/<epoch>-<pid>-repo.json
```

## 任务文件里的两个开关

- `patch`：`null` 表示本题靠 `fixtures` 落盘（新增验收用例），实施报告里 `patch_check` 记 `none`。
- `warmup_compile`（默认 `true`）：初始状态是否应当编译通过。默认预热失败 = 准备失败 = `invalid`；
  `repo-refactor-rename-title-api` 这类「改了名字、调用点没跟上」的题初始必然编译不过，置 `false`：
  预热失败只记账（报告 `warmup_ok:false`），判据仍必须前置红。

## 补丁失效了怎么重建（不要删题）

补丁对着 `0bee58e` 生成；如果 `git apply --check` 变红，按下面顺序处理：

1. **先换应用方式**：`git apply --3way --check <patch>`，能过就说明只是上下文轻微漂移，改用 `--3way` 应用并在任务文件里记录。
2. **仍然红**：说明目标行真的变了，需要重新生成补丁，而不是删题：
   - 建一个 `0bee58e` 的干净快照（`git read-tree 0bee58e` + `git checkout-index -a --prefix=<dir>/`）；
   - 在快照里手工注入本题的初始状态（回退行为 / 半完成重命名 / 文档偏差），**不要改测试**；
   - `git -C <快照> diff -- <改动文件>` 导出为 `evals/repo/patches/<id>.patch`；
   - 重算守卫：`evals/repo/judge/guards.json` 里该题的 `tail_hash` / `whole_hash` / `assert_min`；
   - 重跑 `evals_repo_pipeline_selfcheck`，确认该题「判据红」。
3. **题集与快照必须同源**：`base_commit` 与快照都只能来自同一个提交；一处 `HEAD`、一处常量会让整份报告 `valid:false`。

## 为什么本轮没有前端任务

前端任务需要工作区里有 `node_modules`：复制它会把工作区体积和可写面都放大，靠安装则引入网络与版本漂移，
两者都会破坏「干净树、可重复、互不污染」这条前提。扩题时单独定「只读依赖挂载」方案，再补前端题。

## 指标口径

见 `evals/repo-tasks.json` 与报告 `summary`：完成率三口径、轮次只在已知样本上算、打断次数固定用
`allow_once` 口径（避免「总是允许」的记忆吞掉后续真实摩擦）、超时与耗时单列。10 题样本下单题翻转就是 10pp，
**只做描述统计，不做显著性结论**；若 10/10 全绿，结论写「本集无分辨力」，不写「能力达标」。
