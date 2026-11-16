# D7b（已评估、未采纳）沙箱改用独立低权 Windows 账户 — 规格 v1

> 状态：**已评估、未采纳**（2026-11-16 用户拍板「对齐参考，不独家造第二套身份模型」）。
> 本文件保留调研与设计结论备查，**不实施、代码不动**。
>
> 未采纳理由（上游代码复核，详见 `docs/16 §12.5` 与 D7 `tasks.md` P1）：
> - codex（`b016cd18`）与我们**同一套**令牌 + deny 结构：`CreateRestrictedToken` 带
>   `WRITE_RESTRICTED`，`.git` 的 deny ACE 命名只存在于 restricting 列表的 capability SID，
>   掩码虽含 `DELETE`，删除位同样不在 pass 2 评估 → **同样挡不住删除**；
> - dsh（`39163dd3`）只用 `Everyone: deny FILE_DELETE_CHILD` 断「父目录捷径」，
>   不禁对象自身 `DELETE`，`del .git\config` 仍成功；
> - zcode（`fec97590`）Windows 侧无文件系统沙箱，只有 Job Object + 命令级守卫。
>
> 即：**没有参考实现能用 OS 边界挡住删除**；D7b 会成为独家第二套身份模型，其代价
> （需管理员或服务助手、独立 `CARGO_HOME`、EDR 敏感、多一份凭据）与「对齐参考」冲突。
> 已知限制按「与 codex 同源」处理，不再作为缺口追。
>
> 原来源：D7 待批项 P1——规格 §3.4#4 「删 `.git/index` 必失败」在现有
> `WRITE_RESTRICTED` 机制内做不到（依据见 `docs/16 §12.5`）。
>
> 前置事实（2026-11-16 本机探针，写约束档 + `.git` 已带 deny ACE）：
> `echo x> .git\config` 被拒；`ren .git\index index2` 被拒；
> **`del .git\config` 成功；`rmdir /s /q .git` 成功**。

## 1. 为什么换账户能修，而现有机制不能

DACL 对受限令牌做两次检查：

- **pass 1**：用令牌的**普通 SID** 过整条 DACL——所有访问位都参与；
- **pass 2**：用 **restricting SID** 再过一遍——`WRITE_RESTRICTED` 下**只评估写访问位**。

工作区写 SID 只存在于 restricting 列表，所以它的 deny ACE 只在 pass 2 命中；
`DELETE` / `FILE_DELETE_CHILD` 不是写访问位 → pass 2 不评估，pass 1 又不认识这条 SID
→ 删除永远放行。

换成**独立本地账户**后，该账户的用户 SID 是令牌的**普通 SID**：deny ACE 打在它身上，
pass 1 就会对 `DELETE` 生效。而用户本人的 SID 是另一条，**不受影响**——这是与
「deny Everyone」方案的关键区别（后者会把用户自己的 `git commit` 一起打死）。

## 2. 目标与不变量

1. 沙箱进程以专用低权账户身份运行，**不能**读写用户主目录、不能改 `.git`；
2. 用户本人在普通终端里的 `git commit` / `git checkout` / `git gc` **完全不受影响**；
3. 「工作区根 / TEMP / 工具链目录可写」的既有边界不退化（`cargo test` 仍能跑）；
4. 任何一环失败都**不得静默降级**：报错并让用户显式选择退回受限令牌档；
5. 账户与凭据的创建/删除都是**幂等**的，重复启动不重复建。

## 3. 改法

### 3.1 账户创建与生命周期

- 名字：`ShaniuSandbox$<机器指纹前 8 位>`（避免多用户/多装冲突）；只加入 `Users` 组，
  **绝不**加入 `Administrators`。
- 凭据：32 字节 CSPRNG 密码，转 UTF-16 后 `CryptProtectData`（当前用户作用域）落盘到
  `<data_root>/sandbox/account.bin`；文件 DACL 只给当前用户。密码**不进日志、不进事件**。
- 账户 SID 写进 `config`（`[sandbox].account_sid`）；启动时比对实际 SID，
  **不一致即 fail-closed**（防同名账户劫持）。
- 登录权限：`SeDenyInteractiveLogonRight` + `SeDenyRemoteInteractiveLogonRight`；
  **不得**授予 `SeDenyBatchLogonRight`（沙箱用 batch 登录）。
- 密码永不过期（`net user <name> /expires:never`）；账户禁用自动改密。
- 创建时机：首次启用该档时**一次性提升**（UAC）。不在安装器里做也可以，但必须在
  UI 上把「为什么需要管理员」写清楚。

### 3.2 沙箱启动：以该身份运行

- `LogonUserW(name, ".", password, LOGON32_LOGON_BATCH, LOGON32_PROVIDER_DEFAULT)`
  拿到主令牌 → `CreateProcessAsUserW`。
- **不能用 `CreateProcessWithLogonW`**：它不支持句柄继承，而本仓的 exec 依赖继承的
  管道 / ConPTY 句柄；换过去等于重写 stdio 通道。
- **不能用 `CreateProcessWithTokenW`**：需要 `SeImpersonatePrivilege`，普通用户没有。
- `CreateProcessAsUserW` 需要 `SeAssignPrimaryTokenPrivilege` +
  `SeIncreaseQuotaPrivilege`。**这是本条最大的成本**：应用要么以管理员身份运行，
  要么装一个最小权限的服务助手代为 `CreateProcessAsUser`（服务需要 IPC 面）。
- 令牌仍需 `DISABLE_MAX_PRIVILEGE | LUA_TOKEN`（剥特权）；**不再需要
  `WRITE_RESTRICTED` + restricting SID**——工作区边界由 DACL 的 allow ACE 决定。

### 3.3 deny ACE 打在哪个 SID

- 目标 SID：**沙箱账户的用户 SID**（`S-1-5-21-…-<rid>`，从 `LogonUser` 令牌取
  `TokenUser`，不要用 `net user` 解析字符串）。
- 位置与掩码沿用 D7：`.git` / `.agents` / `.codex` 三处（worktree 的 `gitdir:` 目标
  一并），`DENY_ACCESS`，掩码 `DELETE | FILE_DELETE_CHILD`（写位保留作纵深）。
- 账本沿用 `sandbox/ancestor-ace.jsonl`：`(path, sid, kind=deny)`；卸载按 `(path, sid)`
  精确删除。
- **不得**对用户本人 SID、`Everyone`、`Users` 写任何 deny ACE。

### 3.4 目录、环境与凭据

| 变量 | 取值 | 授权 |
|---|---|---|
| `USERPROFILE` | `<data_root>/sandbox/profile` | 账户 SID 全控 |
| `TEMP` / `TMP` | `<data_root>/sandbox/profile/temp` | 账户 SID 全控 |
| `CARGO_HOME` | `<data_root>/sandbox/profile/cargo` | 账户 SID 全控 |
| `RUSTUP_HOME` | `<data_root>/sandbox/profile/rustup` | 账户 SID 全控 |
| 工作区根 | 用户选定目录 | 账户 SID `Access::Write`（沿用 `acl::grant`） |

- 不用系统默认的 `C:\Users\<账户>\...`：那是系统在建 profile 时按模板写的，
  我们无法保证权限形状，且会在未登录时创建残缺 profile。
- 工具链（rustc/cargo/node）仍在系统目录，账户只需**读 + 执行**；写缓存全部落到
  上表目录。
- 凭据文件 `account.bin` 的读权限**只给当前用户**，沙箱账户不可读。

### 3.5 接缝（现有代码位置）

- `sandbox/windows_impl.rs`：新增 `spawn_as_account()`，与 `spawn_restricted_command`
  并列；配置 `OsSandbox` 增 `LowPrivAccount` 档。
- `sandbox/acl.rs`：`grant` / `deny_write` 只换「传给谁」的 SID，机制不变；
  `write_scope` 的 TEMP 一项换成 3.4 的 profile 目录。
- `sandbox/mod.rs`：`grant_write_scope` 之后追加 `deny_protected_subpaths(account_sid)`。
- `sandbox/persistent` 账本：键空间不变。
- `config`：新增 `[sandbox].account_sid` / `account_name`；读取时校验。
- `events`：新增 `sandbox_lowpriv_unavailable`（additive），说明失败原因与建议动作。

### 3.6 失败回退

| 失败点 | 行为 |
|---|---|
| 账户不存在且无管理员权限创建 | 失败，提示「需要管理员创建沙箱账户」，事件 `sandbox_lowpriv_unavailable`；**不**自动退回受限令牌档 |
| `LogonUser` 失败（密码/账户被禁用） | 同上；同时提示可重新生成凭据 |
| `CreateProcessAsUser` 缺特权 | 同上；提示「以管理员身份运行或安装服务助手」 |
| deny ACE 写入失败 | fail-closed（沿用 D7 口径）：拒绝启动沙箱 exec |
| 用户在设置里显式切回 `restricted_token` | 允许；UI 必须标注「该档挡不住删除」 |

### 3.7 卸载

1. 按账本删掉所有 `(path, account_sid)` 的 deny ACE（D7 的确定性重建逻辑复用）。
2. 删账户：`NetUserDel`（需管理员）；失败则记录并提示手动删除。
3. 删 `<data_root>/sandbox/profile`、`account.bin`；`account_sid` 从 config 清空。
4. 留一条审计：`sandbox_account_removed`（含账户名，不含密码）。

## 4. 会先变红的用例

### 4.1 真机探针（`tests/sandbox_probe.rs`）

| 用例 | 断言 |
|---|---|
| `lowpriv_del_git_config_is_denied` | 沙箱内 `del .git\config` **非 0**，且文件仍存在 |
| `lowpriv_rmdir_git_is_denied` | 沙箱内 `rmdir /s /q .git` **非 0**，且 `.git` 仍在 |
| `lowpriv_write_git_config_is_denied` | 沿用 D7，写/改名仍被拒（防回归） |
| `lowpriv_workspace_write_still_works` | `echo ok> inside.txt` 为 0 且落盘 |
| `lowpriv_user_git_commit_still_works` | **宿主 shell**（非沙箱）里 `git init` + commit 成功 |
| `lowpriv_cargo_build_in_sandbox` | 冷构建成功（`#[ignore]`，只在本机手动跑） |

红→绿的证明方式：先在**旧档**上跑同一段删除命令（`OsSandbox::RestrictedToken`），
记录「exit 0 + 文件消失」；切到新档后同一条命令必须变红（非 0 + 文件仍在）。
两者都写进用例注释，避免以后有人把「新档能删」当成环境问题。

### 4.2 单元

- `account_sid_is_not_the_user_sid`：deny ACE 的目标 SID ≠ 当前进程用户 SID。
- `acl_ledger_round_trips_account_sid`：装卸后 DACL 逐字节一致（复用 `compare_restored`）。
- `spawn_env_points_to_profile`：`TEMP` / `CARGO_HOME` 落在 `<data_root>/sandbox/profile` 下。

## 5. 反向验证（必须红）

1. 把 deny ACE 的 SID 换回 `workspace_write_sid`（旧的 restricting SID）→
   `lowpriv_del_git_config_is_denied` 必须**变绿失败**（即删除重新成功），
   证明确实是「账户 SID 走 pass 1」在起作用，不是别的副作用。
2. 把 deny ACE 的掩码去掉 `DELETE` → 删除用例必须变绿（放行），证明掩码可观测。
3. 注释掉 `deny_protected_subpaths` 调用 → 写/删用例同时变绿（放行）。

## 6. 代价与风险

- **需要提升权限**：账户创建与 `CreateProcessAsUser` 都要求管理员。应用当前是普通
  用户进程；要么常驻提权，要么新增一个服务助手（多一个 IPC 攻击面、多一份签名与
  升级维护）。
- **构建缓存翻倍**：每个账户独立的 `CARGO_HOME`，首次冷构建极慢、磁盘占用翻倍。
- **EDR / 杀软敏感**：创建本地账户 + 跨账户创建进程，企业环境常见拦截。
- **凭据文件**：拿到 `account.bin` + 本机登录态即可用该账户身份本地执行；虽然权限低，
  但仍是一个新的持久化面。
- **多用户 / 多装机**：账户名冲突、残留账户、卸载失败留下孤儿账户。
- **profile 创建失败**：无交互登录的账户不建 profile，必须先手写目录并授权（3.4 已按
  此处理，但不能回退到系统默认行为）。
- **仍不覆盖**：用户自己把 `.git` 里的文件删掉、以及沙箱外的任何进程——本条只约束
  「以沙箱身份运行的东西」。

## 7. 验证命令

```powershell
$env:CARGO_TARGET_DIR="$env:TEMP\shaniu-d7b-target"
cargo test --manifest-path src-tauri/Cargo.toml --test sandbox_probe -- --test-threads=1
cargo test --manifest-path src-tauri/Cargo.toml --lib -- sandbox::acl
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
pwsh -NoProfile -File .\scripts\check-deps.ps1
pwsh -NoProfile -File .\scripts\dev\check-doc-claims.ps1
```

## 8. 提交策略

至少三笔，避免一次性大改：

1. 账户模块（创建/删除/凭据/校验）+ 单测；
2. `spawn_as_account` + 配置档 + 失败回退 + 事件；
3. 探针与反向验证 + 文档（`docs/16` §13、`docs/11` 常见问题）。

## 9. 未决问题（需用户裁决）

1. 是否接受「应用需管理员运行」或「新增服务助手」？（这是硬前提）
2. 是否接受每账户独立 cargo 缓存带来的首次冷构建成本？
3. 本规格实施后，`restricted_token` 档保留为默认还是降为可选？
4. 企业环境被 EDR 拦截时，是一律 fail-closed，还是允许用户显式切回旧档？
