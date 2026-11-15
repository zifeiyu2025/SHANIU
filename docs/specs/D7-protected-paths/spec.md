# D7（必须改）工作区受保护路径只读 — 规格 v1

> 来源：`docs/analysis/reference-delta-2026-11-15.md` §3 D7。
> 本轮**只出规格，不改代码**。
> 范围：`src-tauri/src/sandbox/acl.rs`、`src-tauri/src/tool_router/mod.rs`、`src-tauri/src/permission/mod.rs`、`src-tauri/src/tools/unified_exec.rs`、`src-tauri/tests/sandbox_probe.rs`。
> 结论先行：**原估「中」偏乐观，实际是「大」**——只加 deny ACE 会让 `git commit` 在 workspace-write 下直接失败，必须同批补命令级升级（§2.3），否则是对现有工作流的硬回归。

## 1. 现状与上游对照

### 上游（codex `b016cd18`）

可写根不是裸目录，而是 `WritableRoot { root, read_only_subpaths, protected_metadata_names }`（`protocol/src/protocol.rs:1338-1343`），写判定先查子路径（`:1347-1372`）：

```rust
if path.starts_with(subpath) {
    return false;
}
!self.path_contains_protected_metadata_name(path)
```

默认子路径由 `protocol/src/permissions.rs:2382-2414` 生成：顶层 `.git`（目录或 worktree 的 gitdir 指针文件都会解析保护）、`.agents`、`.codex`。

### SHANIU 现状

`sandbox/acl.rs:115-123` 的可写范围只有「工作区根 + TEMP/TMP + 工具链目录」，工作区内没有任何只读子路径；`AclManager` 只会写 `GRANT_ACCESS` 允许 ACE（`acl.rs:45-51`），全仓没有在用的 deny ACE。

`sandbox_probe.rs` 的既有探针也只验证「工作区内可写 / 工作区外不可写」，没有「工作区内某子路径不可写」。

### 差异与后果

在 workspace-write 下，模型（或它启动的脚本）可以改 `.git/hooks/pre-commit`、`.git/config`。下一次用户在该仓库执行 `git commit` 时，钩子以**用户身份**执行任意命令——沙箱边界被一次性绕过，且不留痕迹（rollout 里只有一条 shell 调用）。这是本轮清单里唯一能转化为"宿主代码执行"的差距。

## 2. 改法

### 2.1 受保护子路径（`sandbox/acl.rs`）

```rust
/// 工作区内只读的元数据路径（对齐 codex 的 read_only_subpaths）：
/// - `.git`：目录，或 worktree 的 `gitdir: …` 指针文件；指针目标同样保护；
/// - `.agents` / `.codex`：代理自己的配置面。
pub fn protected_subpaths(workspace_root: &Path) -> Vec<PathBuf>
```

规则：

1. `workspace_root/.git` 存在则加入；若它是文件且首行是 `gitdir: <path>`，把解析后的目录也加入（相对路径按工作区根解析）。
2. `.agents`、`.codex` 存在则加入。
3. 返回绝对、规范化（`dunce::canonicalize` 或既有等价工具）路径，避免 `..` 与前缀绕过。

`is_protected_path(workspace_root, candidate) -> Option<PathBuf>`：对 candidate 做同样规范化后逐个 `starts_with`，命中返回命中的保护根（用于报错文案）。

### 2.2 应用 deny ACE

在 `AclManager` 上新增（复用既有的"先备份再改"路径，禁止另起一套）：

```rust
const DENY_WRITE_MASK: u32 =
    FILE_GENERIC_WRITE | DELETE | FILE_DELETE_CHILD | WRITE_DAC | WRITE_OWNER; // 0x001F_0156

fn deny_write(&mut self, dir: &Path, sid: PSID) -> Result<(), String>
```

- 用 `SetEntriesInAclW` 追加一条 `ACCESS_DENIED_ACE_TYPE`（`NO_MULTIPLE_TRUSTEE`、`SUB_CONTAINERS_AND_OBJECTS_INHERIT`），再 `SetNamedSecurityInfoW(DACL_SECURITY_INFORMATION)`。
- **顺序**：先给工作区根授 allow（现状不变），再对每个存在的保护子路径加 deny。deny 对同一 SID 优先于继承来的 allow，所以 `.git` 子树整棵只读。
- 备份：`.git` 自身的 DACL 必须进入既有备份账本（`AclManager` 的 `.dacl` 文件），卸载/退出按现状还原；只读保护是**持久**的，与祖先 ACE 同档，不随会话结束撤销。
- **失败即拒绝启动**：deny 写不进去（权限、只读卷、非常规 ACL）时返回 `Err`，沙箱启动失败并提示具体路径。理由：静默放行等于没有这条边界。
- 审计：`acl-audit.jsonl` 记一条 `deny_protected`，含路径、掩码、结果。

### 2.3 命令级升级（同批必须项）

只加 deny 会让 `git add/commit/checkout/stash` 在 workspace-write 下失败。上游的等价机制是"命令申请升级 → 审批 → 沙箱外执行"，我们目前没有（`permission::GateOutcome` 只有 `Allowed/Denied/Forbidden/Cancelled`，`GateOutcome` 不携带执行档位）。

最小落地：

1. `permission::GateOutcome` 增加 `AllowedEscalated`（仅 `ApprovalMode` 不是 `Never` 时可达）。
2. `tools/unified_exec.rs:472-530`：请求的 `ctx.sandbox` 为 `WorkspaceWrite` 且审批结果为 `AllowedEscalated` 时，走 `PreparedSpawn::Plain`（不加容器）。
3. 触发条件**不靠猜命令**：执行失败返回 `err 5` 且 stderr 命中保护路径时，回填给模型一条结构化提示"该路径受保护，需申请升级重试"，模型显式带升级标记再调；用户在审批卡上看到"将以你的身份、在沙箱外执行"。
4. 审计：升级执行写 `sandbox_escalated` 事件（含命令、审批 id）。

### 2.4 逻辑层早拒（`tool_router/mod.rs:874`）

`resolve_for_write` 在现有工作区边界检查之后加一层：

```rust
if let Some(root) = crate::sandbox::acl::is_protected_path(workspace_root, &resolved) {
    return Err(ToolError::policy(format!(
        "{} 是工作区内的只读元数据路径（{}），apply_patch 不允许写入",
        resolved.display(), root.display()
    )));
}
```

这样 `apply_patch` 得到的是可读文案，而不是 OS 的 `err 5`；shell 仍由 DACL 兜底。

### 2.5 接口变化

- `sandbox::acl` 新增两个公开函数 + `AclManager::deny_write`。
- `GateOutcome` 新增变体（破坏性：所有 `match` 要补）。
- 无 Tauri command 变更；新增一个事件 `sandbox_escalated`（前端可忽略）。

## 3. 会先变红的用例

### 3.1 单元（`sandbox/acl.rs`）

- `protected_subpaths_covers_git_and_agent_metadata`：临时目录造 `.git/`、`.agents/`，断言清单与规范化结果。
- `gitdir_pointer_target_is_protected`：`.git` 是文件、内容 `gitdir: ../real-git`，断言 `real-git` 也在清单里。
- `is_protected_path_rejects_git_hooks_and_config`：`.git/hooks/pre-commit`、`.git/config` 命中；`src/main.rs`、`.gitignore` 不命中。

### 3.2 工具层（`tool_router` / `apply_patch`）

- `resolve_for_write_rejects_protected_metadata`：`resolve_for_write(ws, ".git/config")` 返回 `ToolError` 且文案含"只读元数据"。
- `apply_patch_rejects_git_hook_patch`：补丁改 `.git/hooks/pre-commit` 整条失败，且**没有写入任何字节**（断言文件不存在/内容未变）。

### 3.3 真机探针（`tests/sandbox_probe.rs`）

- `workspace_dot_git_write_is_denied`：在探针工作区造 `.git/config`，容器内 `cmd /C echo x > .git\config` 必须失败（err 5），且 `.git\config` 内容未变；同一轮里 `src\a.txt` 写入必须成功（证明沙箱本身可用，不是整体写不进）。
- `escalated_write_outside_sandbox_succeeds`：同一命令带升级标记、审批通过后成功，且事件里有 `sandbox_escalated`。

### 3.4 反向验证（必须红）

1. 注释掉 deny 应用 → 3.3 第一条红；
2. 只保护 `.git` 不解析 `gitdir:` 指针 → 3.1 第二条红；
3. 去掉 `resolve_for_write` 的早拒 → 3.2 第一条红（只剩 OS err 5，文案不符）；
4. 把 deny 掩码退化成只有 `FILE_GENERIC_WRITE` → 3.3 第一条仍在但"删除 `.git/index`"探针红（补一条删除探针）；
5. 移除升级通道 → 3.3 第二条红。

## 4. 影响面

- **用户可见（必读）**：workspace-write 下 `git commit` 不再"静默可用"，而是先失败、再提示申请升级；审批卡会写明"沙箱外、以你的身份执行"。这是安全边界的必要代价。
- **既有用例**：`sandbox_probe.rs` 的既有 6 组探针不受影响（探针工作区没有 `.git`）；`apply_patch` 的 20+ 用例路径都不在保护清单里。
- **`git` 之外的破坏**：`npm`/`cargo` 若写 `.git`（极少）同样被拒，提示同一条升级路径。
- **卸载**：保护 ACE 进备份账本，`restore_all` 一并还原；需要一条"装了再卸，`.git` DACL 与原来逐字节一致"的断言。
- **性能**：每个受保护路径多一次 DACL 读取与写入，启动开销毫秒级。

## 5. 风险

| 风险 | 说明 | 处置 |
| --- | --- | --- |
| 所有权异常仓库 | `.git` 属于其他用户，deny 写不进 | 失败即拒绝启动 + 文案给出"以 full-access 运行"或"修复目录所有权"两条出路 |
| 升级通道被滥用 | 模型可能反复申请升级 | 审批记忆按命令前缀（既有 `PrefixAllowlist`），升级不进入记忆，每次都问 |
| DACL 让 git 自己也无法操作 | git 命令在沙箱内彻底失败 | 这正是预期；升级通道承接 |
| 只读卷 / FAT32 | 无法设 deny | 启动失败并明确提示"该工作区不支持只读子路径保护" |

## 6. 代价

- D7-1/2（保护清单 + deny + 早拒 + 探针）：约 1 天。
- D7-3（审批升级 + exec 接线 + 事件 + 探针）：约 1–1.5 天。
- 合计约 2–2.5 天，且**必须同批**；分两批会让中间态无法提交 git。

## 7. 验证命令

`cargo test sandbox -- --test-threads=1`；`cargo test --test sandbox_probe -- --test-threads=1`（需要桌面会话）；`cargo test -- --test-threads=1`；`cargo clippy --all-targets -- -D warnings`；`.\scripts\check-deps.ps1`；`.\scripts\dev\check-doc-claims.ps1`。

## 8. 提交策略

一笔 `feat(sandbox): 工作区受保护路径只读，并补命令级升级`。改的是本仓自有代码，不涉及 vendored 上游，无需 `align/overrides.json` 登记；若最终改了 vendored 路径（预期不会），按既有规矩登记 overrides 与账本。
