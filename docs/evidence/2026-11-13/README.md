# 证据 2026-11-13：三面统一走默认档（第三期真机）

本目录是 `docs/16` §11「第三期：三面统一走默认档」的**观测产物**。
本批只有一组证据：`three-faces-default/`（exec / 交互终端 / MCP server 三面都走
`restricted_token` 写约束，AppContainer 只作可选档）。

## 产生时间与基准提交

- 产生时间：2026-11-13 00:48–00:49（`drive-final.log` 内的环境快照记为 00:49）。
- 基准：当时 `HEAD = 57bec6d`，工作树另有 **20 个未提交改动**（同一份环境快照）。
- 这批改动随后按序提交为 `5fcfd29`…`fe016d2`，本目录与 `docs/16` §11 随 `fcd6bdc` 入库。
- 因此**证据里的 `shaniu.exe` 不是由任一干净提交单独构建的**：它是当时工作树（含上述未提交改动）
  的构建产物。复跑只能证明机制仍成立，不能当作某个干净提交的回归证明。

## 文件

| 文件 | 内容 |
| --- | --- |
| `three-faces-default/harness/run-mock.ps1` | 起本地 mock 模型（127.0.0.1:18765），并给进程注入 `WORKSPACE`/`OUTSIDE` |
| `three-faces-default/harness/run-vite.ps1` | 起 vite dev（5173） |
| `three-faces-default/harness/run-app.ps1` | 起真实 exe：固定数据根、`SHANIU_CDP_PORT=9224`、显式清空 `SHANIU_SANDBOX_OS` |
| `three-faces-default/harness/drive.mjs` | 经 CDP 调前端同一条 IPC：`sandbox_status` → MCP 首连审查/批准 → 终端 → MCP 读写 |
| `three-faces-default/drive-final.log` | 最终一次全链路输出，本批主证 |
| `three-faces-default/drive-run3.log` | 中间失败：mock 没拿到 `WORKSPACE`；保留作反证，**不是通过记录** |
| `three-faces-default/data-root/config.toml` | 本次配置：`[sandbox] mode = "workspace_write"`，不写 `os` |
| `three-faces-default/workspace/hello.txt` | 被 MCP 读到的区内文件（内容 `hello-from-workspace`） |

按 `three-faces-default/.gitignore`，运行期产物不入库：`data-root/rollouts/`、`data-root/sessions/`、
`data-root/secrets/`、`workspace/mcp-inside.txt`、`workspace/term-out.txt`。后两个是本轮真机写入的
结果文件，复跑会重新生成。

## 复跑命令

前置：Windows x64；`node` 26 在 PATH；真实第三方 MCP server
`@modelcontextprotocol/server-filesystem` 装在 `D:\AI\mcp-node-test`（`config.toml` 写死了该路径）；
先构建 `src-tauri\target\debug\shaniu.exe`。

```powershell
$E = 'd:\MyWorkspace\SHANIU\docs\evidence\2026-11-13\three-faces-default'
pwsh -NoProfile -File "$E\harness\run-mock.ps1"   # 1. mock 模型 18765
pwsh -NoProfile -File "$E\harness\run-vite.ps1"   # 2. vite 5173
pwsh -NoProfile -File "$E\harness\run-app.ps1"    # 3. 真实 exe，CDP 9224
# 4. 另开一个终端跑驱动
$env:CDP_PORT = '9224'; $env:MCP_SERVER_ID = 'fs'; $env:DATA_ROOT = "$E\data-root"
node "$E\harness\drive.mjs" *> "$E\drive-rerun.log"
Test-Path 'C:\Users\jun\shaniu-mcp-escaped.txt'   # 期望 False
```

预期与 `drive-final.log` 一致：`sandbox_status` 为 `restricted_token` / `full` / `idle`；MCP 首连
`status=review`、16 个工具；`mcp resolve: status=trusted approved=16`；终端 `TERM-REAL-OK`、
`exit=0` 且整树回收；MCP 读 `hello-from-workspace`、写区内成功、写区外 `EPERM`；
`list_directory` 恰好三个文件。

## 已核实（有落盘证据）

| 结论 | 证据 | 边界 |
| --- | --- | --- |
| 默认档状态为受限令牌写约束，未起容器 | `drive-final.log:3`：`"os":"restricted_token"`、`"enforcement":"full"`、`"ancestor":"idle"` | 只代表本机、本配置（未写 `[sandbox] os`） |
| MCP 首连进入审查，暴露 16 个工具，沙箱面为 `restricted_token` | `drive-final.log:4-6` | 工具集来自所装的 `server-filesystem` 版本 |
| 批准后 `status=trusted approved=16` | `drive-final.log:7` | 信任落盘在 `data-root/secrets/mcp-trust.json`（按约定不入库） |
| 交互终端可开、可执行、可退出、可回收 | `drive-final.log:19-32`：`TERM-REAL-OK`、`exit=0`、`close` 报「整棵进程树已回收」 | 未做 Ctrl+C 注入 |
| MCP 读区内、写区内、写区外被拒、目录复核 | `drive-final.log:33-40`：`hello-from-workspace`、`Successfully wrote`、`EPERM … shaniu-mcp-escaped.txt`、三个文件 | 「区外被拒」来自 SHANIU 写约束：`config.toml` 把 `C:\Users\jun` 也放进了 server 自身允许目录 |
| 配置只有 `mode = "workspace_write"`，没有 `os`、没有环境变量覆盖 | `data-root/config.toml:14-15`、`harness/run-app.ps1:5-8` | 静态文件事实 |

## 无法核实 / 待测（附缺什么、怎么补）

| 断言 | 出处 | 缺什么 / 为什么 | 测法 |
| --- | --- | --- | --- |
| exe 与干净提交的对应关系 | 本目录 `README.md`「基准提交」 | 运行时工作树有 20 个未提交改动，没有单提交构建记录 | 回 `57bec6d` 重放同一批改动后重新构建复跑；或接受「机制复验」口径 |
| 区外文件 `C:\Users\jun\shaniu-mcp-escaped.txt` 确实不存在 | `drive-final.log:38` | 日志只证明 `EPERM`；档案里没有落盘查询输出 | 复跑后执行 `Test-Path 'C:\Users\jun\shaniu-mcp-escaped.txt'`，期望 `False` |
| exe 侧 stderr / 启动日志 | `harness/run-app.ps1:3` | `app-console.log` 未归档，档案里无法复核 | 复跑 `run-app.ps1` 会重新生成该文件 |
| Ctrl+C 注入 | `docs/16` §11.3 | 功能未实现，属未验证项而非绿灯 | 实现后补一条真机用例，判定「按 `\x03` 后 shell 进程退出」 |
| AppContainer 可选档在真机上仍可用 | `docs/16` §11.4 | 本批只跑默认档，没有容器档真机日志 | 显式 `os = "app_container"` 跑一次 MCP/终端，并保留日志与 `ancestor-ace.jsonl` |
| `drive-run3.log` 的失败原因可复现 | `docs/16` §11.5 末段 | 中间失败，仅证明 mock 缺 `WORKSPACE` 时链路会红 | 清空 `WORKSPACE` 再跑 `run-mock.ps1`，观察同一条错误 |
