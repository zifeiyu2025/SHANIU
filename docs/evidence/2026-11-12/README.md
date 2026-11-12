# 证据 2026-11-12：令牌 / AccessCheck / 路径访问探针

本目录是 `docs/13`、`docs/14` 中 ACL 相关断言的**观测产物**。全部只读，不含任何 DACL 写入。
产生时的代码基准：`origin/main = 3b21b86`（行号以该提交为准）。

## 文件

| 文件 | 内容 |
| --- | --- |
| `token-access-probe-20261112-183149.jsonl` | 51 行观测：1 行令牌摘要 + 30 行 AccessCheck + 20 行路径 API 探测 |
| `code-refs-2026-11-12.md` | 代码行摘录（含行号），用于替代「口头结论」 |
| `README.md` | 本文件：口径、复跑命令、已核实/无法核实清单 |

## 复跑

```powershell
# 当前进程令牌（自校验基线）
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/dev/sandbox-token-probe.ps1

# 指定进程令牌（例如一个在跑的 AppContainer 子进程）
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/dev/sandbox-token-probe.ps1 -TokenPid <pid>
```

脚本不调用任何写 DACL 的 API；`-TokenPid` 时会先 `DuplicateTokenEx` 成 impersonation token，
在该身份下跑 `GetFileAttributes` / `FindFirstFile` / `CreateFile`。
输出默认写到 `scratch/evidence-<当天日期>/`（过程稿）；**要作为证据留档，跑完手动拷进本目录并更新下面两张表。**

## 已核实（有落盘证据）

| 结论 | 证据 | 边界 |
| --- | --- | --- |
| 祖先 ACE 走**后台预置**而非 exec 路径：`lib.rs:3116-3121` 启动即调 `provision_ancestors_in_background`，后台线程在 `mod.rs:530-531` / `:627` 调 `ensure_ancestor_grant`；`grant_chain` 生产无调用点 | `code-refs-2026-11-12.md` §1、§2 | 静态代码事实，可用 `origin/main` 复核 |
| `glob-0.3.4` 在 `fill_todo` 的 if 守卫里静默跳过失败层级（该 crate 无 `flatten`）；`.flatten()` 在 `tauri-utils-2.10.2/src/acl/build.rs:76-78` | `code-refs-2026-11-12.md` §4 | 结论「权限清单静默为空」不变，只更正归因 |
| 四处写权限为**持久化**（`grant_scope_persistent`），不是「随退出还原」 | `code-refs-2026-11-12.md` §1 末段 | 与 `mod.rs:436-437` 修正后的注释一致 |
| 当前用户令牌：`isAppContainer=0`、`integritySid=S-1-16-8192`、`SeChangeNotifyPrivilege=Bypass` | `token-access-probe-*.jsonl` 的 `type:"token"` 行 | **只代表当前用户令牌**，不是容器令牌 |

## 无法核实 / 待测（附缺什么、怎么补）

`AccessCheck` 那一行是「工具不可用」；其余各行是本次禁止 DACL 写入或未采集而无法硬证。

| 断言 | 出处 | 缺什么 / 为什么 | 测法 |
| --- | --- | --- | --- |
| 容器令牌只有 `SeChangeNotifyPrivilege`、无 `SeBackupPrivilege`；lowbox（`TokenIsAppContainer=1`）；对 `C:\`、`C:\Users` 等 `AccessCheck` 全 DENIED | `docs/13:19`、`docs/13:29` | 本机取到的是当前用户令牌（`isAppContainer=0`），**没有容器令牌样本**；`AccessCheck` 本机不可用（见下节） | 起一个沙箱会话，`-TokenPid <子进程 pid>` 复跑；换机器先做 `AccessCheck` 自校验 |
| 掩码最小集 `(RA,S)` 够、单独 `FILE_READ_ATTRIBUTES`/`READ_CONTROL` 不够 | `docs/14:45-52` | 需要给祖先目录写 ACE 才能构造四种掩码对照 | 在**一次性测试目录**（非用户真实目录）上分别写四种掩码，容器内跑 `fs::metadata` + `read_dir`，共 4 组 |
| `C:\Users` 是否需要管理员（owner=SYSTEM、`WRITE_DAC` 不可得） | `docs/13:23/25`、`docs/04:252` | 需要真实写一次 ACE 才知道是否被拒 | `Get-Acl C:\Users` 看 owner + `Set-Acl` 试写（只在明确同意后）；或查 `AccessCheck`（当前不可用） |
| 首次/二次启动耗时（24–26s、40s、3ms、86s 等） | `docs/04:244`、`docs/13:17-18`、`docs/13:28`、`docs/14:63-77` | 需要真实写盘 + 计时日志，本批未采集 | 首次冷启动与二次启动各跑 3 次，保留 `ancestor-ace.jsonl` 与计时日志 |
| 跨机容器 SID 一致性 | `docs/13:36` | 需要在第二台机器上跑同一脚本比对 SID | 第二台机器执行同一命令，比对 `token` 行里的包 SID |
| 冷构建权限清单为空可归因到祖先 ACE 缺失 | `docs/13:32` | 需要保留该次 `cargo build` 的完整输出与 `ancestor-ace.jsonl`，本批未采集 | 复现 A/B 两组冷构建，保留完整 stdout/stderr 与账本 |

## `AccessCheck` 在本机不可用（重要口径）

- 现象：脚本对每条路径都返回 `err:1338@check(sdValid=True)`。
  `1338 = ERROR_INVALID_SECURITY_DESCRIPTOR`，但同一描述符过 `IsValidSecurityDescriptor` 为 **True**。
- 已排除：`GetFileSecurityW` 与 `GetNamedSecurityInfoW(SE_FILE_OBJECT, si=0x5)` 两条取 SD 的路径都试过；
  `AccessCheck` 的 ClientToken 已改为 `DuplicateTokenEx(tt=2, SecurityImpersonation)` 得到的 impersonation token
  （`1309 ERROR_NO_IMPERSONATION_TOKEN` 已消除）；`PRIVILEGE_SET` 输出缓冲已初始化。仍 1338。
- 结论：**这是工具不可用，不是被测对象的结果**。脚本对每个 `err:*` 行追加
  `fallback_createfile`（同一掩码直接 `CreateFileW`，内核最硬判定），但该字段只能证明「按该掩码打开会怎样」，
  **不能冒充 `AccessCheck` 的授权判定**。
- 将来在别的机器上复核前，**先确认 `AccessCheck` 管线可用**：用本进程令牌对已知可读对象自校验，必须返回 allowed；
  自校验不通就先修探针，不要拿 `fallback_createfile` 当结论。
