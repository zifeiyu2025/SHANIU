# 证据 2026-11-14：写约束档下 `node --test` / `npm test` 的「能改不能测」

本目录是 `docs/16` §10.7.6 的依据：真机复现「工作区内改完文件后跑测试被沙箱拒绝」，
并逐条验证替代路径是否真的可用。**不改 DACL、不改默认档行为**，只回答「能不能绕过」与
「绕不过时给什么出路」。

## 方法

用产品同一条入口 `shaniu_lib::sandbox::prepare_spawn_with`（`policy=workspace_write`、
`program="cmd"`、`args=["/C", <line>]`，与 `tools/unified_exec.rs` 拼命令的方式一致），
在 `%USERPROFILE%\.shaniu-probe\` 下的最小 Node 工程里跑七组对照。

- 探针源码：`probe.rs`（当时放在 `src-tauri/tests/tmp_piped_stdio_probe.rs` 的临时集成测试，
  跑完已从仓库删除；放回该路径即可复跑）。
- 工程：`package.json` 的 `test` 脚本为 `node --test`，单个用例文件 `test/stats.test.js`。
- 机制：`RestrictedToken` = 默认档（`flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED`，
  restricting 列表 `[logon, Everyone, 工作区写 SID, temp 写 SID]`）；`Logical` = 显式回退档
  （`flags=0`、无 restricting SID、不设 default DACL）。

复跑：

```powershell
# 把 probe.rs 复制回 src-tauri/tests/tmp_piped_stdio_probe.rs 后
cd src-tauri
cargo test --offline --test tmp_piped_stdio_probe -- --nocapture --test-threads=1
```

## 结果（`run-2026-11-14.log` 原文）

环境：Windows 10.0.26300.9818、node v26.13.1、npm 随 node 安装。

| # | 命令 | 机制 | 退出码 | 结论 |
|---|---|---|---|---|
| 0 | `node --test`（沙箱外对照） | 无沙箱 | 0 | 工程本身是绿的 |
| 1 | `node --test` | RestrictedToken | 1 | **`Error: spawn EPERM` / `errno: -4048` / `syscall: 'spawn'`** |
| 2 | `node test/stats.test.js` | RestrictedToken | 0 | 通过（测试在主进程内执行） |
| 3 | `node --test --test-isolation=none` | RestrictedToken | 0 | 通过（改为同进程执行） |
| 4 | `node --test` | Logical | 0 | 通过 |
| 5 | `npm test`（脚本就是 `node --test`） | RestrictedToken | 1 | **同 #1 的 EPERM** |
| 6 | `npm test` | Logical | 0 | 通过 |

## 关键事实

1. **失败点在 node 内部起子进程那一步，不是 npm 自身**：`npm test` 的输出里 npm 已经把
   `> node --test` 跑起来了，失败栈落在 `node:internal/test_runner/runner:581`，即
   `node:test` 为每个测试文件派生进程时被拒。所以「换成 npm 就行 / 用 npm 包一层」不成立。
2. **`--test-isolation=none` 确实能绕开**，因为不再派生进程；代价是测试共享同一进程，
   隔离语义变了（有些用例依赖进程隔离，结果可能不同）。这属于**改命令语义**，不能默认替用户加。
3. **`logical` 档可用**：它没有 restricting SID，pass-2 不生效，命名管道客户端写打开能通过；
   代价是没有路径级写边界（`enforcement=partial`），与 `docs/16` 对回退档的定义一致。
4. 不能靠「把用户 SID 加进 restricting 列表」来修：写边界正是靠 restricting 列表里**没有**
   用户 SID 才成立；加进去等于让用户原本可写的所有路径重新可写，越界写会一起放开。
   命名管道 DACL 的成因见 `docs/evidence/2026-11-13/pipe-eprem-probe/`（默认 SD 模板、
   与令牌 default DACL 无关）。

## 处置

代码只做「可解释 + 有出路」：命中 `denied=piped-stdio` 时，在 `[sandbox]` 行后追加
`sandbox::piped_stdio_hint` 的一段说明（不是项目代码问题、原样重试无效、三条替代），
模型与用户看到同一份。不自动改命令、不自动降档、不把失败记成通过。
