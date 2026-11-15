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
   **不当作已接受偏差**，见下方「待批项 P1」；`docs/16` §12.5 记录了探针与评估路径。
3. **T4 的早拒从 `tool_router::resolve_for_write` 下移到 `tools/apply_patch.rs`**：
   `scripts/check-deps.ps1` 按 `docs/01 §2.3.1` 只允许 `tools → sandbox`，`tool_router`
   不含 sandbox 边；硬加边要改门禁权威表，等于为一个函数放宽架构约束。`resolve_for_write`
   的唯一生产调用方就是 `apply_patch`（4 处），下移后强制力与文案不变，
   组件边界（`.gitignore` 可写）用例也随之下移。

---

## 待批项 P1（**未接受**，需用户裁决）：删除防护

§3.4#4「删 `.git/index` 必失败」**当前不成立**。这不是「先放着」，也不是已接受的偏差；
本条需要改身份模型或放宽其他边界，超出 D7 授权范围，**必须由用户拍板**。

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

### 候选方案与代价（供裁决，未实施）

| 方案 | 做法 | 代价 / 风险 |
|---|---|---|
| A（正确解） | 沙箱子进程改用**独立低权 Windows 账户**：deny ACE 打在该账户用户 SID 上，走 pass 1，DELETE 生效；用户本人 SID 不受影响 | 大：账户/配置文件/`CARGO_HOME`/temp/凭据管理，安装与升级链路全要改 |
| B（中） | 对「会触碰 `.git` 的命令」切 AppContainer 档执行 | 中：要按命令选档、补容器授权；AppContainer 读限制更严，行为差异大 |
| C（小，仅纵深） | `unified_exec` 命令级拒绝删除类操作（`del` / `rmdir` / `Remove-Item` / `git clean -fdx`…）并留审计 | 小：**可绕过**（`[IO.File]::Delete`、编译出的 helper、脚本），不能当验收判据 |

建议：先上 C 作纵深（不改任何验收判据），A 另立规格。**在用户裁决前，§3.4#4 保持未达成**。
