//! 审批 broker（docs §8 permission/mod.rs，ADR-003：集中裁决、审批不可绕过）。
//!
//! 职责：工具执行前的门禁（gate）——按 policy 裁决；需要询问时发
//! `ExecApprovalRequest` 并挂起等待前端回执（approve 命令 → resolve）。
//! 审批 UI 不可伪造：裁决只接受后端校验过的 request_id 回执。

pub mod policy;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::events::{ApprovalDecision, EventMsg, EventSink, RiskLevel};

// re-export 便于 use permission::{policy, ...}
pub use policy::{decide, risk_of, Decision, SessionPolicy};

/// 门禁结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    Allowed,
    Denied,
    Cancelled,
}

/// 审批 broker：pending 请求表 + execute 级会话记忆。
#[derive(Default)]
pub struct ApprovalBroker {
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalDecision>>>,
    remembered_execute: Mutex<HashSet<String>>,
}

impl ApprovalBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 工具执行前的门禁：裁决 → 必要时发审批请求并等待回执。
    pub async fn gate(
        &self,
        tool: &str,
        args: &Value,
        call_id: &str,
        policy: policy::SessionPolicy,
        sink: &Arc<dyn EventSink>,
        cancel: &CancellationToken,
    ) -> GateOutcome {
        if cancel.is_cancelled() {
            return GateOutcome::Cancelled;
        }
        let risk = risk_of(tool);
        let remembered = self
            .remembered_execute
            .lock()
            .expect("remembered 锁中毒")
            .contains(tool);
        match policy::decide(risk, policy.approval, policy.sandbox, remembered) {
            Decision::Allow => GateOutcome::Allowed,
            Decision::Deny => GateOutcome::Denied,
            Decision::Prompt => self.request_approval(tool, args, call_id, risk, sink, cancel).await,
        }
    }

    /// 发审批请求并挂起等待；取消时撤单。
    async fn request_approval(
        &self,
        tool: &str,
        args: &Value,
        call_id: &str,
        risk: RiskLevel,
        sink: &Arc<dyn EventSink>,
        cancel: &CancellationToken,
    ) -> GateOutcome {
        let request_id = format!("appr-{call_id}");
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        self.pending
            .lock()
            .expect("pending 锁中毒")
            .insert(request_id.clone(), tx);
        let _ = sink.emit(&EventMsg::ExecApprovalRequest {
            request_id: request_id.clone(),
            tool: tool.to_string(),
            args: args.clone(),
            risk,
        });
        let outcome = tokio::select! {
            resp = rx => match resp {
                // allow_always 仅 execute 级记忆（ADR-003：destructive 永不记忆）
                Ok(d @ ApprovalDecision::Allow) => {
                    self.remember_if_execute(tool, risk);
                    GateOutcome::from(d)
                }
                Ok(d @ ApprovalDecision::AllowAlways) => {
                    self.remember_if_execute(tool, risk);
                    GateOutcome::from(d)
                }
                Ok(ApprovalDecision::Deny) => GateOutcome::Denied,
                Err(_) => GateOutcome::Cancelled, // 发送端已被撤单
            },
            _ = cancel.cancelled() => GateOutcome::Cancelled,
        };
        if outcome != GateOutcome::Allowed {
            // 未放行的请求撤单（已 resolve 的 remove 无副作用）
            self.pending.lock().expect("pending 锁中毒").remove(&request_id);
        }
        outcome
    }

    fn remember_if_execute(&self, tool: &str, risk: RiskLevel) {
        if risk == RiskLevel::Execute {
            self.remembered_execute
                .lock()
                .expect("remembered 锁中毒")
                .insert(tool.to_string());
        }
    }

    /// 前端回执入口（approve 命令）；返回是否命中 pending。
    pub fn resolve(&self, request_id: &str, decision: ApprovalDecision) -> bool {
        match self
            .pending
            .lock()
            .expect("pending 锁中毒")
            .remove(request_id)
        {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// 是否有待审批请求（测试/前端状态查询）。
    pub fn has_pending(&self, request_id: &str) -> bool {
        self.pending.lock().expect("pending 锁中毒").contains_key(request_id)
    }
}

impl From<ApprovalDecision> for GateOutcome {
    fn from(d: ApprovalDecision) -> Self {
        match d {
            ApprovalDecision::Allow | ApprovalDecision::AllowAlways => GateOutcome::Allowed,
            ApprovalDecision::Deny => GateOutcome::Denied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApprovalMode, SandboxMode};
    use crate::events::NullEventSink;
    use std::sync::Arc;

    fn sink() -> Arc<dyn EventSink> {
        Arc::new(NullEventSink)
    }

    fn args() -> Value {
        serde_json::json!({"command": "echo x"})
    }

    #[tokio::test]
    async fn read_tool_passes_without_request() {
        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let out = broker
            .gate("read_file", &serde_json::json!({}), "c0", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::ReadOnly }, &sink(), &cancel)
            .await;
        assert_eq!(out, GateOutcome::Allowed);
    }

    #[tokio::test]
    async fn write_denied_in_readonly_sandbox() {
        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let out = broker
            .gate("apply_patch", &serde_json::json!({}), "c1", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::ReadOnly }, &sink(), &cancel)
            .await;
        assert_eq!(out, GateOutcome::Denied);
    }

    #[tokio::test]
    async fn prompt_resolves_to_deny_then_allows_after_remember() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let sink = sink();

        // execute → Prompt：后台 resolve 为 deny
        let b = broker.clone();
        let s = sink.clone();
        let c = cancel.clone();
        let gate1 = tokio::spawn(async move {
            b.gate("unified_exec", &args(), "c2", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, &s, &c).await
        });
        // 等待请求挂起后回执 deny
        for _ in 0..200 {
            if broker.has_pending("appr-c2") {
                assert!(broker.resolve("appr-c2", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(gate1.await.unwrap(), GateOutcome::Denied);

        // 再问一次 → resolve allow_always → 本会话记忆，后续直接放行
        let b = broker.clone();
        let s2 = sink.clone();
        let c2 = cancel.clone();
        let gate2 = tokio::spawn(async move {
            b.gate("unified_exec", &args(), "c3", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, &s2, &c2).await
        });
        for _ in 0..200 {
            if broker.has_pending("appr-c3") {
                assert!(broker.resolve("appr-c3", ApprovalDecision::AllowAlways));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(gate2.await.unwrap(), GateOutcome::Allowed);

        // 记忆生效：不再询问
        let out = broker
            .gate("unified_exec", &args(), "c4", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, &sink, &cancel)
            .await;
        assert_eq!(out, GateOutcome::Allowed);
    }

    #[tokio::test]
    async fn cancel_during_prompt_cancels_and_unregisters() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let gate = tokio::spawn({
            let b = broker.clone();
            let s = sink();
            let c = cancel.clone();
            async move { b.gate("unified_exec", &args(), "c5", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, &s, &c).await }
        });
        for _ in 0..200 {
            if broker.has_pending("appr-c5") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancel.cancel();
        assert_eq!(gate.await.unwrap(), GateOutcome::Cancelled);
        assert!(!broker.has_pending("appr-c5"), "取消后必须撤单");
    }
}
