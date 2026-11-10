//! 四级风险分级与裁决策略（docs §8 permission/policy.rs，ADR-003）。
//!
//! 纯函数：输入（风险/审批模式/沙箱模式/会话记忆）→ 裁决（Allow/Prompt/Deny）。
//! 规则矩阵（ADR-007 四档审批，随 turn 生效）：
//! - read：一律放行；
//! - write_in_workspace：只读沙箱拒绝；danger 放行；workspace-write 下
//!   `never/on-failure/full-access` 放行、`on-request` 询问；
//! - execute：只读沙箱拒绝（ADR-002：只读禁止命令执行）；`on-request` 下本会话记住过则放行、
//!   否则询问；其余三档放行（`on-failure` 的提权询问发生在沙箱拒绝之后，不在门禁里）；
//! - destructive：`on-request` 每次必问（永不记忆）；`never/on-failure` 直接拒绝（fail-closed，
//!   不询问也不放行）；`full-access` 放行（前提是 danger 沙箱，由 `ApprovalMode::effective` 保证）。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

use crate::config::{ApprovalMode, SandboxMode};
use crate::events::RiskLevel;

/// 代理级工具白名单（ADR-103「权限只降不升」）。
///
/// `None` = 没有额外限制（root 的形态）；`Some` = 显式收窄到这些工具。
/// 收窄在三个执行点同时生效：模型声明、审批门禁、编排派生。
pub type ToolAllowlist = Arc<HashSet<String>>;

/// 两个白名单取交集：任何一侧的收窄都不能被另一侧抹掉。
///
/// `None` 代表「该侧无限制」，因此 `(None, x)` 的结果就是 `x`。
/// 这是 min(父策略, 子代理声明) 的唯一实现——把它做成纯函数，
/// 派生路径（spawn/resume/profile）才不会各写一份略有出入的合并逻辑。
pub fn intersect_allowlist(
    parent: Option<&ToolAllowlist>,
    child: Option<&ToolAllowlist>,
) -> Option<ToolAllowlist> {
    match (parent, child) {
        (None, None) => None,
        (Some(parent), None) => Some(parent.clone()),
        (None, Some(child)) => Some(child.clone()),
        (Some(parent), Some(child)) => {
            Some(Arc::new(parent.intersection(child.as_ref()).cloned().collect()))
        }
    }
}

/// 只读工具集（`profile=explorer`）：按风险分级从**路由的真实工具表**里取 Read 档。
///
/// 不在 profile 里硬编码名单：新增只读工具会自动落入 explorer，新增写入/执行工具
/// 也不会因为忘改名单被 explorer 意外拿到——名单与风险分级共用同一份事实源。
pub fn readonly_allowlist(tool_names: impl IntoIterator<Item = String>) -> ToolAllowlist {
    Arc::new(
        tool_names
            .into_iter()
            .filter(|name| risk_of(name) == RiskLevel::Read)
            .collect(),
    )
}

/// 裁决结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Prompt,
    Deny,
}

/// 会话级策略快照（审批模式 + 沙箱模式），随任务启动固化。
#[derive(Debug, Clone, Copy)]
pub struct SessionPolicy {
    pub approval: ApprovalMode,
    pub sandbox: SandboxMode,
}

/// 工具名 → 风险等级；未知工具按 destructive（最高风险，fail-closed）。
pub fn risk_of(tool_name: &str) -> RiskLevel {
    match tool_name {
        "read_file" | "list_dir" | "search" => RiskLevel::Read,
        // todo_write（SH-108）：会话内任务清单，不触碰工作区，上游同名工具从不审批；
        // Read 档让它进计划态白名单（计划态列 todo 是上游规划工作流的形态）。
        "todo_write" => RiskLevel::Read,
        // skill：SH-091 只读取本地 SKILL.md 正文，不写不执行；后续动作仍逐次审批。
        "skill" => RiskLevel::Read,
        // web_search（SH-072）：只发关键词给公开搜索页，不读不写本地文件，
        // 结果按不可信内容包裹回填。按只读档，免审批（用户已明确不新增打断点）。
        // web_fetch：只读公开 http(s) 页面，私网/非文本在抓取前直拒；同样免审批但留审计。
        "web_search" | "web_fetch" => RiskLevel::Read,
        // view_image（SH-072）：只读工作区内图片；工作区外的路径在工具层直接拒绝。
        "view_image" => RiskLevel::Read,
        "apply_patch" => RiskLevel::WriteInWorkspace,
        "unified_exec" => RiskLevel::Execute,
        // terminal（SH-070，docs/02 ADR-106）：能起任意进程且是持久会话，按执行档。
        // 与 unified_exec 同档：不额外加严（沙箱边界仍是真边界），也不放宽。
        "terminal" => RiskLevel::Execute,
        // SH-052 编排四工具（docs/02 §2.2 风险分级）：派生/恢复/注入消息会改变
        // 子代理行为面，按写入档审批；wait_agent 只读结果，不审批（否则每等一次都要点一下）。
        "spawn_agent" | "resume_agent" | "send_agent_message" => RiskLevel::WriteInWorkspace,
        "wait_agent" => RiskLevel::Read,
        // Plan Mode：进入免确认（read 档）；退出必须用户评审（ADR-003）
        "enter_plan_mode" => RiskLevel::Read,
        "exit_plan_mode" => RiskLevel::PlanReview,
        _ => RiskLevel::Destructive,
    }
}

/// 裁决：是否需要人工审批（Prompt）/ 直接放行 / 直接拒绝。
pub fn decide(
    risk: RiskLevel,
    approval_mode: ApprovalMode,
    sandbox_mode: SandboxMode,
    remembered_execute: bool,
) -> Decision {
    // 前提校验先做：`full-access` 只在 danger 沙箱下成立，否则按 on-request 裁决。
    let (approval_mode, _) = approval_mode.effective(sandbox_mode);
    match risk {
        RiskLevel::Read => Decision::Allow,
        RiskLevel::WriteInWorkspace => match sandbox_mode {
            SandboxMode::ReadOnly => Decision::Deny,
            SandboxMode::DangerFullAccess => Decision::Allow,
            SandboxMode::WorkspaceWrite => match approval_mode {
                // 沙箱就是边界：边界内的写入不再逐次打断
                ApprovalMode::Never | ApprovalMode::OnFailure => Decision::Allow,
                ApprovalMode::OnRequest => Decision::Prompt,
                // effective 已把 full-access 在非 danger 沙箱下回落，这里不可达；
                // 保守当作 on-request 处理，避免将来改 effective 时静默放宽。
                ApprovalMode::FullAccess => Decision::Prompt,
            },
        },
        RiskLevel::Execute => match sandbox_mode {
            SandboxMode::ReadOnly => Decision::Deny,
            _ => match approval_mode {
                // on-failure 的询问发生在**沙箱拒绝之后**（执行侧），门禁先放行
                ApprovalMode::Never | ApprovalMode::OnFailure | ApprovalMode::FullAccess => {
                    Decision::Allow
                }
                ApprovalMode::OnRequest => {
                    if remembered_execute {
                        Decision::Allow
                    } else {
                        Decision::Prompt
                    }
                }
            },
        },
        RiskLevel::Destructive => match approval_mode {
            // 不询问的档位不能把破坏性操作静默放行：直接失败（fail-closed）
            ApprovalMode::Never | ApprovalMode::OnFailure => Decision::Deny,
            ApprovalMode::OnRequest => Decision::Prompt,
            ApprovalMode::FullAccess => Decision::Allow,
        },
        // 计划评审是用户对计划的人工会签，不是风险分级：非交互档不能自动放行，
        // 否则模型可以自行退出计划态（AGENTS §11.3 退化红线）。
        RiskLevel::PlanReview => match approval_mode {
            ApprovalMode::OnRequest => Decision::Prompt,
            _ => Decision::Deny,
        },
    }
}

/// 会话策略句柄：`AgentLoop` 每个 turn 开头取一次快照，因此运行中切换审批档
/// 从**下一个 turn** 起生效，而不是改一个进程级全局开关（ADR-007）。
#[derive(Debug, Clone)]
pub struct SessionPolicyHandle {
    inner: Arc<RwLock<SessionPolicy>>,
    /// 生效档被钳制的说明（目前只有 full-access 前提不满足一种）。
    note: Arc<Mutex<Option<String>>>,
}

impl SessionPolicyHandle {
    pub fn new(policy: SessionPolicy) -> Self {
        let (approval, note) = normalize(policy);
        Self {
            inner: Arc::new(RwLock::new(SessionPolicy { approval, ..policy })),
            note: Arc::new(Mutex::new(note.map(str::to_string))),
        }
    }

    pub fn snapshot(&self) -> SessionPolicy {
        *self.inner.read().expect("policy 锁中毒")
    }

    /// 重置会话策略（任务启动时由装配层调用）：上一任务运行中切换的档位
    /// 不得泄漏到下一任务——那等于把"会话内切换"变成了事实上的全局开关。
    pub fn reset(&self, policy: SessionPolicy) {
        let (approval, note) = normalize(policy);
        *self.inner.write().expect("policy 锁中毒") = SessionPolicy { approval, ..policy };
        *self.note.lock().expect("policy note 锁中毒") = note.map(str::to_string);
    }

    /// 运行中切换沙箱档（会话内 `/permission`）。下一 turn 生效，任务启动时由
    /// `reset_policy` 用**本会话档案里的最后一档**重新装载——切档不跨会话泄漏。
    pub fn set_sandbox(&self, mode: SandboxMode) {
        self.inner.write().expect("policy 锁中毒").sandbox = mode;
    }

    pub fn note(&self) -> Option<String> {
        self.note.lock().expect("policy note 锁中毒").clone()
    }

    /// 运行中切换审批档。`full-access` 前提不满足时**返回错误而不是回落**——
    /// 用户明确点名 full-access 却得到一个更严的档，必须让他知道。
    pub fn set_approval(&self, mode: ApprovalMode) -> Result<(), String> {
        let sandbox = self.snapshot().sandbox;
        if let (_, Some(reason)) = mode.effective(sandbox) {
            return Err(reason.to_string());
        }
        self.inner.write().expect("policy 锁中毒").approval = mode;
        *self.note.lock().expect("policy note 锁中毒") = None;
        Ok(())
    }
}

fn normalize(policy: SessionPolicy) -> (ApprovalMode, Option<&'static str>) {
    policy.approval.effective(policy.sandbox)
}

/// 一条命令前缀白名单规则（ADR-007③）。`id` 就是规则原文——审计与界面共用，
/// 改名等于破坏历史可追溯性（同 mcp_client::guard 的规则 id 约定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixRule {
    pub id: String,
    tokens: Vec<String>,
}

/// 前缀白名单（会话内、来自用户 config.toml；**不落全局规则文件**，ADR-008）。
#[derive(Debug, Clone, Default)]
pub struct PrefixAllowlist {
    rules: Vec<PrefixRule>,
}

impl PrefixAllowlist {
    pub fn empty() -> Self {
        Self::default()
    }

    /// 从配置装载。非法规则（含 shell 元字符 / 空串）被跳过并返回原因，
    /// 由装配层播报——静默丢弃会让用户以为规则生效了。
    pub fn from_config(entries: &[String]) -> (Self, Vec<String>) {
        let mut rules = Vec::new();
        let mut rejected = Vec::new();
        for entry in entries {
            let id = entry.trim();
            if id.is_empty() {
                rejected.push("空白规则被忽略".to_string());
                continue;
            }
            match tokenize(id) {
                Some(tokens) if !tokens.is_empty() => rules.push(PrefixRule { id: id.to_string(), tokens }),
                _ => rejected.push(format!(
                    "规则 `{id}` 含 shell 元字符或引号未闭合，已忽略（前缀匹配会被 `&&`/`|`/重定向绕过）"
                )),
            }
        }
        (Self { rules }, rejected)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// 命令命中哪条规则（取最长匹配，审计里记最具体的那条）。
    /// 命令含 shell 元字符时一律不命中：`cargo test && del x` 不能被 `cargo test` 放行。
    /// 命中后还要过参数级只读守卫（`findstr` 这类带文件路径参数的除外），
    /// 否则"前缀对"不等于"这次调用只读"。
    pub fn match_command(&self, command: &str) -> Option<&PrefixRule> {
        let tokens = tokenize(command)?;
        if !readonly_guard_allows(&tokens) {
            return None;
        }
        self.rules
            .iter()
            .filter(|rule| {
                tokens.len() >= rule.tokens.len()
                    && rule.tokens.iter().enumerate().all(|(index, expected)| {
                        // 首 token 是程序名：Windows 下大小写不敏感；其余参数保持精确
                        if index == 0 {
                            tokens[index].eq_ignore_ascii_case(expected)
                        } else {
                            tokens[index] == *expected
                        }
                    })
            })
            .max_by_key(|rule| rule.tokens.len())
    }
}

/// 不产生文件写入的重定向形式（**整词精确匹配**，`2>&1` / `NUL` 大小写不敏感）。
///
/// 只认这几种：`2>&1`、`1>&2` 是句柄合并，`>NUL`/`1>NUL`/`2>NUL` 丢弃到空设备——
/// 三者都写不到文件系统，因此不构成绕过前缀白名单的写入路径。
/// 刻意**不做子串判断**：`2>&1x`、`>NUL.txt`、`2> NUL`（空格拆开）、`>con` 一律不算，
/// 它们要么写文件、要么把判定降级成"包含即放行"。
const SAFE_REDIRECT_WORDS: &[&str] = &["2>&1", "1>&2", ">nul", "1>nul", "2>nul"];

fn is_safe_redirect(word: &str) -> bool {
    SAFE_REDIRECT_WORDS.contains(&word.to_ascii_lowercase().as_str())
}

/// 参数级只读守卫：前缀命中之后再过一道，防止"程序名对、参数越界"的调用免审。
///
/// 按**参数形态**统一校验，不再按程序名特判：任何参数 token 指向工作区外
/// ——盘符（`C:`）、UNC（`\\server\share`）、`..` 上跳、以分隔符开头的绝对/
/// 根相对路径——一律落回审批。`type C:\Users\<u>\.ssh\id_rsa`、`dir C:\Users`
/// 这类"程序名在只读白名单里、参数却指向工作区外"的调用由此被拦下。
///
/// 在通用形态之上保留两条程序级加严：
/// - `findstr`：开关必须在白名单内（`/G`、`/F`、`/D` 能读其它文件）；
/// - `git`：拒绝 `--output*` / `-O*`——它们会把 diff/log/show 的结果写成文件。
fn readonly_guard_allows(tokens: &[String]) -> bool {
    let program = program_name(&tokens[0]);
    if tokens[1..].iter().any(|arg| escapes_workspace(arg)) {
        return false;
    }
    match program.as_str() {
        "findstr" => findstr_flag_allows(&tokens[1..]),
        "git" => !tokens[1..].iter().any(|arg| is_git_output_arg(arg)),
        _ => true,
    }
}

/// 程序名归一化：取最后一段路径、去 `.exe`、小写（Windows 下大小写不敏感）。
fn program_name(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    let base = lower.rsplit(['\\', '/']).next().unwrap_or(lower.as_str());
    base.trim_end_matches(".exe").to_string()
}

/// 单个参数是否指向工作区外。
///
/// 判据只认"能不能落到工作区外"，不认"看起来像路径"：
/// - 盘符：`C:`、`d:`（`/C:literal` 首字符不是字母，不会误判）；
/// - UNC：`\\server\share`；
/// - `..` 上跳；
/// - 根相对/绝对路径：以 `\` 开头的视为越过当前盘根。
///
/// `/` 开头在 cmd 里通常是开关（`/b`、`/S`、`/C:TODO`），不能一概当成绝对路径；
/// 只有含第二个分隔符（`/etc/passwd`）或除开关名外还带路径的才按越界处理。
fn escapes_workspace(arg: &str) -> bool {
    let bytes = arg.as_bytes();
    // `tokenize` 只在**引号外**把 `%`/`^`/`!` 当元字符（引号内原样收词），
    // 而 cmd 在引号内照样展开 `%NAME%`。到了这里说明分词器没拦，静态无法
    // 判定最终路径 → 一律按越界处理，落回审批（宁可多问一次，不能漏）。
    if arg.contains(['%', '^', '!']) {
        return true;
    }
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true;
    }
    // 以 `\` 开头 = 从当前盘根出发（UNC 的 `\\server` 也落在这里）。
    if arg.starts_with('\\') {
        return true;
    }
    if arg.contains("..") {
        return true;
    }
    if let Some(rest) = arg.strip_prefix('/') {
        if rest.is_empty() || rest.contains(['/', '\\']) {
            return true;
        }
    }
    false
}

/// `findstr` 的开关白名单：只有列出的开关与 `/c:字面量` 免问；
/// `/G`、`/F`、`/D` 这类能读其它文件的组合一律落回审批。
fn findstr_flag_allows(args: &[String]) -> bool {
    const FLAGS: &[&str] = &[
        "/b", "/e", "/i", "/l", "/m", "/n", "/o", "/p", "/r", "/s", "/v",
    ];
    args.iter().all(|arg| {
        let lower = arg.to_ascii_lowercase();
        if lower.starts_with('/') {
            return FLAGS.contains(&lower.as_str()) || lower.starts_with("/c:");
        }
        true
    })
}

/// `git` 的输出类参数：会把结果写进文件，绕过只读语义。
fn is_git_output_arg(arg: &str) -> bool {
    arg.starts_with("--output") || arg.starts_with("-O")
}

/// 极简分词：只服务前缀匹配。返回 `None` = 不可安全匹配（元字符/未闭合引号/空）。
///
/// 未加引号的 shell 元字符不再一律拒绝，而是先整词收集、收词后判定：
/// 整词恰好是安全重定向（`2>&1`、`>NUL`）就丢弃该词，其余含元字符的词一律返回 `None`。
/// 引号内的元字符仍是普通字符（cmd 不会解释它们），保持原语义。
fn tokenize(command: &str) -> Option<Vec<String>> {
    let mut raw: Vec<(String, bool)> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    let mut has_meta = false;
    for ch in command.chars() {
        if in_quotes {
            match ch {
                '"' => in_quotes = false,
                _ => current.push(ch),
            }
            continue;
        }
        match ch {
            '"' => {
                in_quotes = true;
                has_token = true;
            }
            // cmd 在执行前会重新解释这些字符（含 %变量% 展开），
            // 因此"前段匹配"无法代表整条命令的真实语义。
            '&' | '|' | ';' | '<' | '>' | '^' | '%' | '!' | '`' | '\n' | '\r' => {
                current.push(ch);
                has_token = true;
                has_meta = true;
            }
            ch if ch.is_whitespace() => {
                if has_token {
                    raw.push((std::mem::take(&mut current), has_meta));
                    has_token = false;
                    has_meta = false;
                }
            }
            ch => {
                current.push(ch);
                has_token = true;
            }
        }
    }
    if in_quotes || !has_token && raw.is_empty() {
        return None;
    }
    if has_token {
        raw.push((current, has_meta));
    }
    let mut tokens = Vec::with_capacity(raw.len());
    for (word, meta) in raw {
        if !meta {
            tokens.push(word);
        } else if !is_safe_redirect(&word) {
            return None;
        }
    }
    Some(tokens)
}

/// Plan Mode 白名单：计划态下仅只读工具 + 退出评审可用（ADR-003 只读计划态）。
/// `enter_plan_mode` 虽为 read 档，但属模式控制而非数据读取，已在计划态时不再放行（幂等进入由工具自身容错）。
pub fn plan_mode_allows(tool_name: &str) -> bool {
    if tool_name == "enter_plan_mode" {
        return false;
    }
    matches!(risk_of(tool_name), RiskLevel::Read) || tool_name == "exit_plan_mode"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_tools_are_always_allowed() {
        for sandbox in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
            assert_eq!(decide(RiskLevel::Read, ApprovalMode::OnRequest, sandbox, false), Decision::Allow);
        }
        assert_eq!(risk_of("read_file"), RiskLevel::Read);
        assert_eq!(risk_of("list_dir"), RiskLevel::Read);
        assert_eq!(risk_of("search"), RiskLevel::Read);
        // todo_write（SH-108）：会话内清单不触碰工作区，永不审批
        assert_eq!(risk_of("todo_write"), RiskLevel::Read);
        // web_search（SH-072）：只发关键词给公开搜索页，按只读档免审批
        assert_eq!(risk_of("web_search"), RiskLevel::Read);
        // web_fetch：只读公开页面，私网/非文本在抓取前直拒
        assert_eq!(risk_of("web_fetch"), RiskLevel::Read);
        assert_eq!(risk_of("view_image"), RiskLevel::Read);
    }

    #[test]
    fn write_is_gated_by_sandbox_mode() {
        assert_eq!(decide(RiskLevel::WriteInWorkspace, ApprovalMode::OnRequest, SandboxMode::ReadOnly, false), Decision::Deny);
        assert_eq!(decide(RiskLevel::WriteInWorkspace, ApprovalMode::OnRequest, SandboxMode::WorkspaceWrite, false), Decision::Prompt);
        assert_eq!(decide(RiskLevel::WriteInWorkspace, ApprovalMode::OnRequest, SandboxMode::DangerFullAccess, false), Decision::Allow);
        assert_eq!(risk_of("apply_patch"), RiskLevel::WriteInWorkspace);
        // SH-052（docs/02 §2.2）：编排工具不能吃未知工具的 destructive 默认档，
        // 否则每个子代理派生都要人工点一次；wait_agent 只读结果，直接放行。
        for tool in ["spawn_agent", "resume_agent", "send_agent_message"] {
            assert_eq!(risk_of(tool), RiskLevel::WriteInWorkspace, "{tool}");
        }
        assert_eq!(risk_of("wait_agent"), RiskLevel::Read);
    }

    #[test]
    fn execute_is_prompted_rememberable_and_denied_in_readonly() {
        assert_eq!(decide(RiskLevel::Execute, ApprovalMode::OnRequest, SandboxMode::ReadOnly, false), Decision::Deny);
        assert_eq!(decide(RiskLevel::Execute, ApprovalMode::OnRequest, SandboxMode::WorkspaceWrite, false), Decision::Prompt);
        assert_eq!(decide(RiskLevel::Execute, ApprovalMode::OnRequest, SandboxMode::WorkspaceWrite, true), Decision::Allow);
        assert_eq!(risk_of("unified_exec"), RiskLevel::Execute);
    }

    /// SH-120 / ADR-007②：`never` = 不询问。沙箱内的写入/执行照常放行；
    /// 沙箱拒绝与破坏性操作直接失败——"不询问"不等于"全部允许"。
    #[test]
    fn never_never_prompts_and_fails_closed_on_destructive() {
        for sandbox in [SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
            assert_eq!(decide(RiskLevel::WriteInWorkspace, ApprovalMode::Never, sandbox, false), Decision::Allow);
            assert_eq!(decide(RiskLevel::Execute, ApprovalMode::Never, sandbox, false), Decision::Allow);
            assert_eq!(decide(RiskLevel::Destructive, ApprovalMode::Never, sandbox, false), Decision::Deny);
        }
        // 只读沙箱仍是硬边界：不询问 ≠ 绕过沙箱
        assert_eq!(decide(RiskLevel::Execute, ApprovalMode::Never, SandboxMode::ReadOnly, false), Decision::Deny);
        // 计划评审不能被模型自行放行
        assert_eq!(decide(RiskLevel::PlanReview, ApprovalMode::Never, SandboxMode::WorkspaceWrite, false), Decision::Deny);
    }

    /// `on_failure`：门禁不打断（沙箱内放行），提权询问发生在沙箱拒绝之后；
    /// 破坏性操作同样 fail-closed。
    #[test]
    fn on_failure_allows_in_sandbox_and_fails_closed_on_destructive() {
        assert_eq!(decide(RiskLevel::WriteInWorkspace, ApprovalMode::OnFailure, SandboxMode::WorkspaceWrite, false), Decision::Allow);
        assert_eq!(decide(RiskLevel::Execute, ApprovalMode::OnFailure, SandboxMode::WorkspaceWrite, false), Decision::Allow);
        assert_eq!(decide(RiskLevel::Execute, ApprovalMode::OnFailure, SandboxMode::ReadOnly, false), Decision::Deny);
        assert_eq!(decide(RiskLevel::Destructive, ApprovalMode::OnFailure, SandboxMode::WorkspaceWrite, false), Decision::Deny);
        assert_eq!(decide(RiskLevel::PlanReview, ApprovalMode::OnFailure, SandboxMode::WorkspaceWrite, false), Decision::Deny);
    }

    /// `full_access`：前提是 danger 沙箱。前提不满足时 `effective` 回落 on_request，
    /// 即"允许面收敛"——不是静默放宽。
    #[test]
    fn full_access_requires_danger_sandbox() {
        assert_eq!(
            decide(RiskLevel::WriteInWorkspace, ApprovalMode::FullAccess, SandboxMode::DangerFullAccess, false),
            Decision::Allow
        );
        assert_eq!(
            decide(RiskLevel::Destructive, ApprovalMode::FullAccess, SandboxMode::DangerFullAccess, false),
            Decision::Allow
        );
        // 前提不满足：写操作回到 on_request（Prompt），不是 Allow
        assert_eq!(
            decide(RiskLevel::WriteInWorkspace, ApprovalMode::FullAccess, SandboxMode::WorkspaceWrite, false),
            Decision::Prompt
        );
        let (effective, note) = ApprovalMode::FullAccess.effective(SandboxMode::WorkspaceWrite);
        assert_eq!(effective, ApprovalMode::OnRequest);
        assert!(note.expect("应给出回落说明").contains("danger_full_access"));
        assert_eq!(ApprovalMode::FullAccess.effective(SandboxMode::DangerFullAccess).0, ApprovalMode::FullAccess);
    }

    #[test]
    fn destructive_is_always_prompted_and_never_remembered() {
        for sandbox in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
            assert_eq!(decide(RiskLevel::Destructive, ApprovalMode::OnRequest, sandbox, false), Decision::Prompt);
            assert_eq!(decide(RiskLevel::Destructive, ApprovalMode::OnRequest, sandbox, true), Decision::Prompt);
        }
    }

    #[test]
    fn unknown_tool_is_destructive_by_default() {
        assert_eq!(risk_of("no_such_tool"), RiskLevel::Destructive);
    }

    #[test]
    fn plan_review_is_always_prompted() {
        for sandbox in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
            assert_eq!(decide(RiskLevel::PlanReview, ApprovalMode::OnRequest, sandbox, true), Decision::Prompt);
        }
        assert_eq!(risk_of("exit_plan_mode"), RiskLevel::PlanReview);
        assert_eq!(risk_of("enter_plan_mode"), RiskLevel::Read);
    }

    /// SH-120：会话策略句柄的重置与切换——运行中切换只改在当前会话，
    /// `full_access` 前提不满足时**报错**而不是悄悄降档。
    #[test]
    fn session_policy_handle_switches_and_resets() {
        let handle = SessionPolicyHandle::new(SessionPolicy {
            approval: ApprovalMode::FullAccess,
            sandbox: SandboxMode::WorkspaceWrite,
        });
        // 构造时就钳制：full_access 前提不满足 → on_request + 说明
        assert_eq!(handle.snapshot().approval, ApprovalMode::OnRequest);
        assert!(handle.note().expect("应留下回落说明").contains("full_access"));

        assert!(handle.set_approval(ApprovalMode::FullAccess).is_err());
        assert!(handle.set_approval(ApprovalMode::Never).is_ok());
        assert_eq!(handle.snapshot().approval, ApprovalMode::Never);
        assert!(handle.note().is_none());

        // reset 把档位拉回配置，不保留上一任务的选择
        handle.reset(SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite });
        assert_eq!(handle.snapshot().approval, ApprovalMode::OnRequest);

        // danger 沙箱下 full_access 才成立
        let danger = SessionPolicyHandle::new(SessionPolicy {
            approval: ApprovalMode::FullAccess,
            sandbox: SandboxMode::DangerFullAccess,
        });
        assert_eq!(danger.snapshot().approval, ApprovalMode::FullAccess);
    }

    /// SH-120 / ADR-007③：前缀白名单的匹配面。重点不是"能匹配"，而是
    /// **什么时候必须不匹配**——任何 shell 组合都会让"前段相同"失去意义。
    #[test]
    fn prefix_allowlist_rejects_shell_composition() {
        let (rules, rejected) = PrefixAllowlist::from_config(&[
            "cargo test".to_string(),
            "git status".to_string(),
            "cargo test && del important".to_string(),
            "  ".to_string(),
        ]);
        assert_eq!(rejected.len(), 2, "含元字符/空白的规则必须被拒绝: {rejected:?}");
        assert_eq!(rules.rule_count(), 2);

        assert_eq!(rules.match_command("cargo test").map(|r| r.id.as_str()), Some("cargo test"));
        assert_eq!(rules.match_command("cargo test -- --nocapture").map(|r| r.id.as_str()), Some("cargo test"));
        // 程序名大小写不敏感（Windows），参数保持精确
        assert!(rules.match_command("CARGO test").is_some());
        assert!(rules.match_command("cargo Test").is_none());

        // 组合命令/重定向/管道/变量展开一律不命中
        for command in [
            "cargo test && del important",
            "cargo test | more",
            "cargo test > out.txt",
            "cargo test; del x",
            "cargo test %EVIL%",
            "cargo test \"unterminated",
        ] {
            assert!(rules.match_command(command).is_none(), "不应命中: {command}");
        }
        // 只是前缀相同、不是同一程序：不命中
        assert!(rules.match_command("cargo testing").is_none());
    }

    /// SH-119 降噪 (b)：不产生文件写入的重定向尾巴按**整词语法**放行。
    /// 判据是"能不能落到文件系统"，不是"看起来像重定向"。
    #[test]
    fn safe_redirect_tails_are_dropped_but_file_redirects_still_rejected() {
        let (rules, _) =
            PrefixAllowlist::from_config(&["cargo test".to_string(), "dir".to_string()]);
        for command in [
            "cargo test 2>&1",
            "cargo test 2>&1 -- --nocapture",
            "cargo test >NUL",
            "cargo test 2>nul",
            "dir /b 1>NUL",
            "cargo test 2>&1 1>&2",
        ] {
            assert!(rules.match_command(command).is_some(), "安全重定向应命中: {command}");
        }
        // 写文件 / 写其它设备 / 语法变形 / 与其它元字符混用：一律落回审批
        for command in [
            "cargo test > out.txt",
            "cargo test >> out.txt",
            "cargo test >NUL.txt",
            "cargo test 2>&1x",
            "cargo test 2> NUL",
            "cargo test >NUL && del important",
            "cargo test 2>&1 | more",
            "dir >con",
        ] {
            assert!(rules.match_command(command).is_none(), "不得命中: {command}");
        }
    }

    /// SH-119 降噪 (c)：`findstr` 只按"本地相对搜索"免问；带盘符/路径/`..`
    /// 或使用能读其它文件的开关时，落回审批。
    #[test]
    fn findstr_prefix_only_covers_relative_searches() {
        let (rules, rejected) =
            PrefixAllowlist::from_config(&crate::config::default_allow_prefixes());
        assert!(rejected.is_empty(), "{rejected:?}");
        for command in [
            "findstr TODO *.*",
            "findstr /S /N /C:\"TODO\" *.*",
            "findstr /I /S todo *.rs",
            "findstr /V \"skip me\" notes.txt",
        ] {
            assert!(rules.match_command(command).is_some(), "只读相对搜索应命中: {command}");
        }
        for command in [
            "findstr TODO C:\\Windows\\win.ini",
            "findstr TODO ..\\secret.txt",
            "findstr /G:patterns.txt *.*",
            "findstr /F:list.txt TODO",
            "findstr /D:C:\\Users TODO *.*",
            "findstr TODO D:notes.txt",
        ] {
            assert!(rules.match_command(command).is_none(), "越界搜索不得命中: {command}");
        }
    }

    /// 审计 H4：守卫按参数形态统一校验，`type`/`dir`/`git diff` 三条
    /// "程序名在只读白名单、参数却越界"的零提示路径必须落回审批。
    #[test]
    fn readonly_guard_blocks_out_of_workspace_arguments() {
        let (rules, rejected) =
            PrefixAllowlist::from_config(&crate::config::default_allow_prefixes());
        assert!(rejected.is_empty(), "{rejected:?}");
        for command in [
            r"type C:\Users\alice\.ssh\id_rsa",
            r"dir C:\Users",
            r"git diff --output=C:\x.txt",
            r"git log -OC:\x.txt",
            r"git show --output=out.txt",
            r"type ..\..\secret.txt",
            r"dir \\server\share",
            r"type \Windows\win.ini",
            r"findstr TODO C:\Windows\win.ini",
        ] {
            assert!(rules.match_command(command).is_none(), "越界参数不得命中: {command}");
        }
        // 工作区内的相对参数仍免问（守卫不能把正常只读调用一并拦掉）
        for command in [
            "dir",
            "dir src",
            "dir /b",
            "type README.md",
            "git diff",
            "git diff --stat",
            "git log --oneline",
        ] {
            assert!(rules.match_command(command).is_some(), "工作区内只读应命中: {command}");
        }
    }

    /// 审计 H1：`tokenize` 只在引号外把 `%`/`^`/`!` 当元字符，引号内会原样收词，
    /// 而 cmd 在引号内照样展开 `%NAME%`。因此 `type "%USERPROFILE%\.ssh\id_rsa"`
    /// 这条路径必须由参数级守卫兜住，否则免审批读任意文件。
    #[test]
    fn readonly_guard_blocks_quoted_cmd_expansion() {
        let (rules, rejected) =
            PrefixAllowlist::from_config(&crate::config::default_allow_prefixes());
        assert!(rejected.is_empty(), "{rejected:?}");
        // 引号形态：分词器放行、只有参数级守卫能拦（撤掉判据即变红）
        for command in [
            r#"type "%USERPROFILE%\.ssh\id_rsa""#,
            r#"dir "%SystemRoot%\System32\config""#,
            r#"findstr TODO "%USERPROFILE%\.ssh\id_rsa""#,
            r#"type "!USERPROFILE!\.ssh\id_rsa""#,
        ] {
            assert!(
                rules.match_command(command).is_none(),
                "引号内的 cmd 展开字符不得命中只读白名单: {command}"
            );
        }
        // 无引号形态：分词器本来就拦下，保留为回归（防止将来放宽元字符判定）
        for command in [
            r"type %USERPROFILE%\.ssh\id_rsa",
            r"dir %SystemRoot%\System32\config",
            r"type ^C:\Users\x\.ssh\id_rsa",
        ] {
            assert!(
                rules.match_command(command).is_none(),
                "未加引号的 cmd 展开/转义字符不得命中: {command}"
            );
        }
        // 反向对照：不含 %/^/! 的工作区内相对路径仍然免审批，别把正常用法打死
        for command in [
            "type README.md",
            "dir src",
            r"dir src\permission",
            "findstr TODO src\\main.rs",
            "git diff --stat",
        ] {
            assert!(
                rules.match_command(command).is_some(),
                "工作区内只读调用应继续命中: {command}"
            );
        }
    }

    #[test]
    fn default_prefixes_are_readonly_or_build_only() {
        let defaults = crate::config::default_allow_prefixes();
        for forbidden in ["cargo run", "git commit", "git push", "del", "rm", "curl", "powershell"] {
            assert!(
                !defaults.iter().any(|rule| rule == forbidden),
                "默认前缀不得包含 {forbidden}"
            );
        }
        let (rules, rejected) = PrefixAllowlist::from_config(&defaults);
        assert!(rejected.is_empty(), "默认规则自身必须合法: {rejected:?}");
        assert_eq!(rules.rule_count(), defaults.len());
    }

    #[test]
    fn plan_mode_whitelist_only_read_and_exit() {
        assert!(plan_mode_allows("read_file"));
        assert!(plan_mode_allows("list_dir"));
        assert!(plan_mode_allows("search"));
        assert!(plan_mode_allows("web_search"));
        assert!(plan_mode_allows("web_fetch"));
        assert!(plan_mode_allows("view_image"));
        // 计划态列 todo 是上游规划工作流的形态（清单就是规划产物）
        assert!(plan_mode_allows("todo_write"));
        assert!(plan_mode_allows("exit_plan_mode"));
        assert!(!plan_mode_allows("apply_patch"));
        assert!(!plan_mode_allows("unified_exec"));
        assert!(!plan_mode_allows("enter_plan_mode"));
    }

    #[test]
    fn allowlist_intersection_only_narrows() {
        let parent: ToolAllowlist = Arc::new(
            ["read_file", "unified_exec"].iter().map(|name| name.to_string()).collect(),
        );
        let child: ToolAllowlist = Arc::new(
            ["read_file", "apply_patch"].iter().map(|name| name.to_string()).collect(),
        );
        let both = intersect_allowlist(Some(&parent), Some(&child)).expect("两侧都有收窄");
        assert_eq!(both.as_ref(), &HashSet::from(["read_file".to_string()]));
        assert_eq!(intersect_allowlist(Some(&parent), None).unwrap(), parent);
        assert_eq!(intersect_allowlist(None, Some(&child)).unwrap(), child);
        assert!(intersect_allowlist(None, None).is_none());

        // explorer 的只读档由风险分级推导：执行/写入工具不得进入
        let readonly = readonly_allowlist(
            ["read_file", "unified_exec", "apply_patch"]
                .iter()
                .map(|name| name.to_string()),
        );
        assert!(readonly.contains("read_file"));
        assert!(!readonly.contains("unified_exec"));
        assert!(!readonly.contains("apply_patch"));
    }
}
