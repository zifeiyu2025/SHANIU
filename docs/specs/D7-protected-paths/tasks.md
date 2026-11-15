# D7 任务清单

> 前置：无。**必须整批落地**（只做保护、不做升级会让 git 写操作回归）。

- [ ] T1 `sandbox/acl.rs`：`protected_subpaths`（含 `gitdir:` 指针解析）+ `is_protected_path`。
- [ ] T2 `AclManager::deny_write`：deny ACE + 复用既有备份账本 + 审计行；失败即 Err。
- [ ] T3 启动接线：授 allow 后对存在的保护子路径逐个 deny；deny 失败终止沙箱启动并报路径。
- [ ] T4 `tool_router/mod.rs:874` `resolve_for_write` 加受保护路径早拒与中文文案。
- [ ] T5 `GateOutcome::AllowedEscalated` + 审批卡文案 + `PrefixAllowlist` 不记忆升级。
- [ ] T6 `tools/unified_exec.rs:472-530` 升级时走 `PreparedSpawn::Plain`；`sandbox_escalated` 事件。
- [ ] T7 回填提示：命中保护路径的 `err 5` 转成"申请升级重试"的结构化提示。
- [ ] T8 先写红用例：3 条单元 + 2 条工具层 + 2 条真机探针（`spec.md` §3.1–3.3）。
- [ ] T9 反向验证 5 条（§3.4），含"删 `.git/index`"探针，记录输出后恢复。
- [ ] T10 卸载回归：装/卸后 `.git` DACL 逐字节一致。
- [ ] T11 文档：`docs/16-沙箱改为只约束写.md` 补保护子路径与升级通道；`docs/11-使用指南.md` 补"git 写操作为什么要升级"。
- [ ] T12 验收：全量测试、探针、clippy、fmt、check-deps、check-doc-claims 全绿；一笔提交，非 force。
