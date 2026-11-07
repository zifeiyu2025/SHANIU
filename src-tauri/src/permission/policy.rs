//! 四级风险分级与裁决策略（docs §8 permission/policy.rs，ADR-003）。
//!
//! 纯函数：输入（风险/审批模式/沙箱模式/会话记忆）→ 裁决（Allow/Prompt/Deny）。
//! 规则矩阵：
//! - read：一律放行；
//! - write_in_workspace：只读沙箱拒绝；danger 放行；其余按审批模式询问；
//! - execute：只读沙箱拒绝（ADR-002：只读禁止命令执行）；本会话记住过则放行；否则询问；
//! - destructive：每次必问（永不记忆）。

use crate::config::{ApprovalMode, SandboxMode};
use crate::events::RiskLevel;

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
        "apply_patch" => RiskLevel::WriteInWorkspace,
        "unified_exec" => RiskLevel::Execute,
        // Plan Mode：进入免确认（read 档）；退出必须用户评审（ADR-003）
        "enter_plan_mode" => RiskLevel::Read,
        "exit_plan_mode" => RiskLevel::PlanReview,
        _ => RiskLevel::Destructive,
    }
}

/// 裁决：是否需要人工审批（Prompt）/ 直接放行 / 直接拒绝。
pub fn decide(
    risk: RiskLevel,
    _approval_mode: ApprovalMode,
    sandbox_mode: SandboxMode,
    remembered_execute: bool,
) -> Decision {
    match risk {
        RiskLevel::Read => Decision::Allow,
        RiskLevel::WriteInWorkspace => match sandbox_mode {
            SandboxMode::ReadOnly => Decision::Deny,
            SandboxMode::DangerFullAccess => Decision::Allow,
            SandboxMode::WorkspaceWrite => Decision::Prompt,
        },
        RiskLevel::Execute => match sandbox_mode {
            SandboxMode::ReadOnly => Decision::Deny,
            _ if remembered_execute => Decision::Allow,
            _ => Decision::Prompt,
        },
        RiskLevel::Destructive | RiskLevel::PlanReview => Decision::Prompt,
    }
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
            assert_eq!(decide(RiskLevel::Read, ApprovalMode::Ask, sandbox, false), Decision::Allow);
        }
        assert_eq!(risk_of("read_file"), RiskLevel::Read);
        assert_eq!(risk_of("list_dir"), RiskLevel::Read);
        assert_eq!(risk_of("search"), RiskLevel::Read);
        // todo_write（SH-108）：会话内清单不触碰工作区，永不审批
        assert_eq!(risk_of("todo_write"), RiskLevel::Read);
    }

    #[test]
    fn write_is_gated_by_sandbox_mode() {
        assert_eq!(decide(RiskLevel::WriteInWorkspace, ApprovalMode::Ask, SandboxMode::ReadOnly, false), Decision::Deny);
        assert_eq!(decide(RiskLevel::WriteInWorkspace, ApprovalMode::Ask, SandboxMode::WorkspaceWrite, false), Decision::Prompt);
        assert_eq!(decide(RiskLevel::WriteInWorkspace, ApprovalMode::AutoReadOnly, SandboxMode::WorkspaceWrite, false), Decision::Prompt);
        assert_eq!(decide(RiskLevel::WriteInWorkspace, ApprovalMode::Ask, SandboxMode::DangerFullAccess, false), Decision::Allow);
        assert_eq!(risk_of("apply_patch"), RiskLevel::WriteInWorkspace);
    }

    #[test]
    fn execute_is_prompted_rememberable_and_denied_in_readonly() {
        assert_eq!(decide(RiskLevel::Execute, ApprovalMode::Ask, SandboxMode::ReadOnly, false), Decision::Deny);
        assert_eq!(decide(RiskLevel::Execute, ApprovalMode::Ask, SandboxMode::WorkspaceWrite, false), Decision::Prompt);
        assert_eq!(decide(RiskLevel::Execute, ApprovalMode::Ask, SandboxMode::WorkspaceWrite, true), Decision::Allow);
        assert_eq!(risk_of("unified_exec"), RiskLevel::Execute);
    }

    #[test]
    fn destructive_is_always_prompted_and_never_remembered() {
        for sandbox in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
            assert_eq!(decide(RiskLevel::Destructive, ApprovalMode::Ask, sandbox, false), Decision::Prompt);
            assert_eq!(decide(RiskLevel::Destructive, ApprovalMode::Ask, sandbox, true), Decision::Prompt);
        }
    }

    #[test]
    fn unknown_tool_is_destructive_by_default() {
        assert_eq!(risk_of("no_such_tool"), RiskLevel::Destructive);
    }

    #[test]
    fn plan_review_is_always_prompted() {
        for sandbox in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
            assert_eq!(decide(RiskLevel::PlanReview, ApprovalMode::Ask, sandbox, true), Decision::Prompt);
        }
        assert_eq!(risk_of("exit_plan_mode"), RiskLevel::PlanReview);
        assert_eq!(risk_of("enter_plan_mode"), RiskLevel::Read);
    }

    #[test]
    fn plan_mode_whitelist_only_read_and_exit() {
        assert!(plan_mode_allows("read_file"));
        assert!(plan_mode_allows("list_dir"));
        assert!(plan_mode_allows("search"));
        // 计划态列 todo 是上游规划工作流的形态（清单就是规划产物）
        assert!(plan_mode_allows("todo_write"));
        assert!(plan_mode_allows("exit_plan_mode"));
        assert!(!plan_mode_allows("apply_patch"));
        assert!(!plan_mode_allows("unified_exec"));
        assert!(!plan_mode_allows("enter_plan_mode"));
    }
}
