# D7b 任务清单（**已评估、未采纳**）

> 来源：`docs/specs/D7-protected-paths/tasks.md` 待批项 P1。2026-11-16 先拍板「做」，
> 同日改口径为「**对齐参考，不独家造第二套身份模型**」，本方案**停止实施**。
> 以下 T1–T12 全部**不实施**，保留作备查；代码一行未动（本仓 `git log` 无 D7b 实现提交）。
>
> 提权取证 spike 已按要求停掉：UAC 弹窗未被批准、提权进程未启动，**未产生任何本地账户、
> 计划任务或 ACL 改动**（`Get-LocalUser ShaniuSandboxProbe*` 为空、
> `Get-ScheduledTask *Shaniu*` 为空），临时脚本已从 `%TEMP%` 删除。

## 环境前置（2026-11-16 本机核实，历史记录）

- 当前会话**不是**管理员：`IsInRole(Administrator)` = `False`；`whoami /user` =
  `S-1-5-21-3226398052-62638092-3539548929-1001`（`jun` 在 Administrators 组里，但拿到的是
  UAC 过滤令牌）。
- 实测建账户被拒：`net user ShaniuSandboxProbe /add` →
  `System error 5 has occurred. Access is denied.`（exit 2），**未产生任何账户**。
- `sudo.exe` 存在但不可用：`已在此计算机上禁用 Sudo`（exit `-2147024891`）。
- 结论：本机既不能建账户，也不能 `CreateProcessAsUser`（缺 `SeAssignPrimaryTokenPrivilege`
  / `SeIncreaseQuotaPrivilege`）。T1 / T3 / T9 / T10 的「绿」只有两条路：
  1. 以管理员身份启动本会话后本机验证；
  2. 交给 CI（`windows-latest` 以管理员运行，`cargo test` 会收集探针）验证。

## 红证据（D7 旧档，2026-11-16 本机实测，可直接复核）

`WorkspaceWrite` + `OsSandbox::RestrictedToken`，`.git` 已带 D7 的 deny ACE：

| 命令 | 结果 |
|---|---|
| `echo x> .git\config` | exit 1，`Access is denied.` |
| `ren .git\index index2` | exit 1，`Access is denied.` |
| `del .git\config` | **exit 0，文件被删** |
| `rmdir /s /q .git` | **exit 0，整个 `.git` 被删** |

这就是 T9 里 `lowpriv_del_git_config_is_denied` / `lowpriv_rmdir_git_is_denied` 的「红」基准：
换成新档后同样两条命令必须非 0 且目标仍在。

- [ ] T1 账户模块：创建/删除/凭据（DPAPI）/SID 校验，幂等；单测覆盖同名账户与 SID 不一致。
- [ ] T2 登录权限：`SeDenyInteractiveLogonRight` + `SeDenyRemoteInteractiveLogonRight`，
  不设 `SeDenyBatchLogonRight`；账户只入 `Users`。
- [ ] T3 `spawn_as_account`：`LogonUser(BATCH)` + `CreateProcessAsUser`，保留管道/ConPTY
  继承；令牌 `DISABLE_MAX_PRIVILEGE | LUA_TOKEN`。
- [ ] T4 新配置档 `OsSandbox::LowPrivAccount` + `[sandbox].account_sid` / `account_name`；
  读取时校验，不一致 fail-closed。
- [ ] T5 profile 目录树（`USERPROFILE`/`TEMP`/`CARGO_HOME`/`RUSTUP_HOME`）创建与授权；
  `write_scope` 的 TEMP 项切到 profile。
- [ ] T6 deny ACE 改用账户用户 SID（`.git` / `.agents` / `.codex`，含 worktree gitdir），
  账本沿用 `(path, sid, kind=deny)`。
- [ ] T7 失败回退：无管理员 / `LogonUser` 失败 / 缺特权 / deny ACE 失败 → 明确报错 +
  `sandbox_lowpriv_unavailable` 事件，不静默降级。
- [ ] T8 卸载：按账本撤 deny ACE → 删账户 → 删 profile/凭据 → 清 config + 审计事件。
- [ ] T9 真机探针：`lowpriv_del_git_config_is_denied`、`lowpriv_rmdir_git_is_denied`、
  `lowpriv_write_git_config_is_denied`、`lowpriv_workspace_write_still_works`、
  `lowpriv_user_git_commit_still_works`；`lowpriv_cargo_build_in_sandbox` 标 `#[ignore]`。
- [ ] T10 反向验证三条（见 `spec.md` §5），记录输出后恢复。
- [ ] T11 文档：`docs/16` 新增 §13、`docs/11` 常见问题补「需要管理员」条目；`docs/04` 补事件。
- [ ] T12 验收：全量测试、探针、clippy、check-deps、check-doc-claims 全绿；三笔提交，非 force，跟到 CI 结论。
