# D7 任务清单

> 前置：无。**必须整批落地**（只做保护、不做升级会让 git 写操作回归）。

- [x] T1 `sandbox/acl.rs`：`protected_subpaths`（含 `gitdir:` 指针解析）+ `is_protected_path`。
- [x] T2 `AclManager::deny_write`：deny ACE + 复用既有备份账本 + 审计行；失败即 Err。
- [x] T3 启动接线：授 allow 后对存在的保护子路径逐个 deny；deny 失败终止沙箱启动并报路径。
- [x] T4 写类路径早拒与中文文案：**落在 `tools/apply_patch.rs::resolve_patch_path`**（见偏差 3）。
- [x] T5 `GateOutcome::AllowedEscalated` + 审批卡文案 + `PrefixAllowlist` 不记忆升级。
- [x] T6 `tools/unified_exec.rs:472-530` 升级时走 `PreparedSpawn::Plain`；`sandbox_escalated` 事件。
- [x] T7 回填提示：命中保护路径的 `err 5` 转成"申请升级重试"的结构化提示。
- [x] T8 先写红用例：3 条单元 + 2 条工具层 + 2 条真机探针（`spec.md` §3.1–3.3）。
- [x] T9 反向验证 5 条（§3.4），含"删 `.git/index`"探针，记录输出后恢复。
- [x] T10 卸载回归：装/卸后 `.git` DACL 逐字节一致。
- [x] T11 文档：`docs/16-沙箱改为只约束写.md` 补保护子路径与升级通道；`docs/11-使用指南.md` 补"git 写操作为什么要升级"。
- [x] T12 验收：全量测试、探针、clippy、fmt、check-deps、check-doc-claims 全绿；一笔提交，非 force。
  结论：CI run `43757124549` success（含 8.3 短名修复与字段消费登记两笔补丁）。

## 落地偏差（2026-11-16）

1. **T10 的卸载实现换了**：`REVOKE_ACCESS` 按 Win32 文档只移除 `ACCESS_ALLOWED_ACE` /
   `SYSTEM_AUDIT_ACE`，**不动 deny ACE**（实测 `considered=1 revoked=0 unchanged=1`）。
   改为确定性重建 DACL（逐条复制、跳过匹配项）。T10 判据用仓库既有的
   `compare_restored`（DACL 本体逐字节一致，只白名单 `SE_DACL_AUTO_INHERITED`）。
2. **§3.4#4「删 `.git/index` 必失败」未达成**：`WRITE_RESTRICTED` 的 restricting SID
   只在**写访问**评估时参与（`CreateRestrictedToken` 文档），实测 `del` / `rmdir /s /q`
   均成功；AppContainer 档连普通文件都删不掉，探针不具区分力。
   **不当作已接受偏差**，见下方「P1（已关闭）」；`docs/16` §12.5 记录了探针与评估路径。
3. **T4 的早拒从 `tool_router::resolve_for_write` 下移到 `tools/apply_patch.rs`**：
   `scripts/check-deps.ps1` 按 `docs/01 §2.3.1` 只允许 `tools → sandbox`，`tool_router`
   不含 sandbox 边；硬加边要改门禁权威表，等于为一个函数放宽架构约束。`resolve_for_write`
   的唯一生产调用方就是 `apply_patch`（4 处），下移后强制力与文案不变，
   组件边界（`.gitignore` 可写）用例也随之下移。

---

## P1（已关闭，对齐参考）：删除防护

§3.4#4「删 `.git/index` 必失败」**当前不成立**。这不是「先放着」，也不是已接受的偏差；
用户已于 2026-11-16 拍板「对齐参考」，本条按下文「状态」关闭，保留事实与评估路径备查。

### 事实（2026-11-16 本机探针，`WorkspaceWrite` + `OsSandbox::RestrictedToken`）

工作区 `.git` 已带 D7 的 deny ACE（掩码 `0x001F_0156`），命令逐条执行：

| 命令 | 结果 |
|---|---|
| `echo x> .git\config` | exit 1，`Access is denied.`（写/覆盖被拒） |
| `ren .git\index index2` | exit 1，`Access is denied.`（改名被拒） |
| `del .git\config` | **exit 0，文件真的没了** |
| `rmdir /s /q .git` | **exit 0，整个 `.git` 被删** |

即：写/新建/改名被挡，**删除不受任何约束**，包括对 `.git` 的递归删除。

### 为什么在本机制内修不了（访问位评估路径）

1. 写约束档的令牌是 `CreateRestrictedToken(..., DISABLE_MAX_PRIVILEGE | LUA_TOKEN |
   WRITE_RESTRICTED)`，工作区写 SID **只出现在 restricting 列表**
   （`src-tauri/src/sandbox/windows_impl.rs:184-238`），不在令牌的普通 SID 列表里。
2. `WRITE_RESTRICTED` 的官方口径是「restricting SID **只在评估写访问时**参与」
   （`CreateRestrictedToken` 的 flags 说明）。内核因此做两次检查：
   - pass 1：用令牌**普通 SID** 过整条 DACL——所有访问位都算；
   - pass 2：用 **restricting SID** 再过一遍——**只对写访问位**做。
3. D7 的 deny ACE 只匹配工作区写 SID，所以它**只在 pass 2 命中**。`DELETE` /
   `FILE_DELETE_CHILD` 不是写访问位 → pass 2 不评估 → 永远不生效；pass 1 里工作区
   写 SID 不在令牌 SID 列表里 → 也不命中。
4. 因此「把 deny DELETE 收窄成只作用于 `.git` 内的目标文件」**不解决问题**：失败原因
   不是 ACE 的作用域，而是这条 SID 根本不参与 DELETE 判定。加在文件上、加在目录上、
   加 `INHERIT_ONLY`，结果都一样。
5. 反向验证：AppContainer 档下容器 SID 是**普通** SID（走 pass 1），删除确实被拒；
   但那一档连普通文件都删不掉，探针不具区分力，也不能当作默认档。

### 候选方案对照（供裁决，均未实施）

| 方案 | 机制 | 能挡住什么 | 代价 | 能否当验收判据 |
|---|---|---|---|---|
| **A 独立低权账户** | 沙箱进程以专用本地账户运行；deny ACE 打在**该账户用户 SID**（普通 SID，pass 1 评估所有访问位） | 写 / 新建 / **删除** / 删除子项 / 改名；用户本人在普通终端的 git 不受影响 | **大**：需管理员（建账户 + `CreateProcessAsUser` 的 `SeAssignPrimaryToken`/`SeIncreaseQuota`），或新增服务助手；每账户独立 `CARGO_HOME`，冷构建慢、磁盘翻倍；EDR 敏感；多一个凭据文件 | **能**：`lowpriv_del_git_config_is_denied` 可红→绿 |
| **B 按命令切 AppContainer** | 触碰 `.git` 的命令改用容器身份执行；容器 SID 是普通 SID，删除被拒 | 写 / 新建 / 删除 / 改名（容器内的全部路径约束） | **中**：按命令选档、补容器授权；AppContainer 读限制更严，行为差异大 | **能**（但同一档连普通文件都删不掉，探针需针对容器补齐区分力） |
| **C 命令级黑名单** | `unified_exec` 里拦 `del` / `rmdir` / `Remove-Item` / `git clean` 等 | 只挡**字面匹配到的**调用 | 小，但**可绕过**（`[IO.File]::Delete`、helper、脚本），且误伤临时文件/构建产物的正常删除 | **不能**：给出「已挡住」的错误安全感，用户已明确否决，**不实施** |

A 的完整规格见 `docs/specs/D7b-lowpriv-sandbox-account/`（含账户生命周期、启动身份、
SID 选择、目录与凭据、卸载回退、探针与反向验证、代价与风险）。

**状态：已关闭（对齐参考口径，2026-11-16）。** 用户拍板「对齐参考，不独家造第二套身份
模型」，因此 A **不采纳**（规格改为「已评估、未采纳」）、B 不作为默认档、C 已被否决。
关闭依据（上游代码复核，`docs/16` §12.5 有同一份记录）：

- codex `b016cd18`：与 D7 **同一套** `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE |
  LUA_TOKEN | WRITE_RESTRICTED)`（`windows-sandbox-rs/src/token.rs:500`）+ capability
  SID 的 deny ACE（掩码含 `DELETE | FILE_DELETE_CHILD`，`acl.rs:838-866`），删除位同样
  不在 pass 2 评估 → 同样挡不住删除，且没有删除类反向用例。
- dsh `39163dd3`：`Everyone: deny FILE_DELETE_CHILD`（`acl.ts:353-361`）只断「父目录
  捷径」，不碰对象自身 `DELETE`，`del .git\config` 仍成功；不敢 deny `DELETE` 是因为
  Everyone 会连宿主 git 一起挡。
- zcode `fec97590`：无文件系统沙箱，只有 Job Object + 命令级守卫
  （`guarded/windows-delete.ts:5-28`），不构成 OS 边界。

**已知限制（与 codex 同源，不再作为缺口追）**：沙箱子进程删除 `.git` 内文件/整个
`.git` 仍可成功；写 / 改 / 新建 / 改名仍被拒，「往 hooks 放新文件」这条提权路径被堵。
文档与 UI 不得表述为「删除已挡住」。

**同批对齐 codex 的授权细节**：写授权（`Access::Write`）去掉 `FILE_DELETE_CHILD`，只留
`DELETE` 随 ACE 继承到子对象（父目录的 delete-child allow 会绕过子对象 deny，见 codex
`acl.rs:513-517` 注释）。回归锁 `write_grant_omits_delete_child_but_keeps_delete`。
**偏差注**：已安装环境的旧持久 ACE 不会自动收敛（`grant` 的 `AlreadyPresent` 分支复用
既有 ACE），旧掩码里的 `FILE_DELETE_CHILD` 要等卸载/撤销后重新授权才会消失；该位在当前
`WRITE_RESTRICTED` 下惰性，不影响现档行为，但切 pass-1 身份前必须先重建授权。
