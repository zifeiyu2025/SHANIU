# D7b 任务清单（**待批，未开工**）

> 来源：`docs/specs/D7-protected-paths/tasks.md` 待批项 P1。是否实施由用户裁决；
> 本文件不勾任何项，代码一行未动。

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
