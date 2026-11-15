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
   只在**写访问**评估时参与（`CreateRestrictedToken` 文档），实测删除仍成功；
   AppContainer 档连普通文件都删不掉，探针不具区分力。破坏性删除未防护（非提权路径，
   写入/新建/改名仍被拒）。详见 `docs/16` §12.5。
3. **T4 的早拒从 `tool_router::resolve_for_write` 下移到 `tools/apply_patch.rs`**：
   `scripts/check-deps.ps1` 按 `docs/01 §2.3.1` 只允许 `tools → sandbox`，`tool_router`
   不含 sandbox 边；硬加边要改门禁权威表，等于为一个函数放宽架构约束。`resolve_for_write`
   的唯一生产调用方就是 `apply_patch`（4 处），下移后强制力与文案不变，
   组件边界（`.gitignore` 可写）用例也随之下移。
