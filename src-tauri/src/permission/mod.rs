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
pub use policy::{decide, risk_of, Decision, SessionPolicy, ToolAllowlist};

/// 门禁结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    Allowed,
    /// 用户明确拒绝（审批卡回执 Deny）。
    Denied,
    /// 策略性拒绝：沙箱不允许或工具不在本代理解效工具集内。
    /// 与 `Denied` 分开是为了让回填给模型的理由准确——把策略拒绝说成
    /// "用户拒绝"会让模型误以为换个说法就能再过一次审批。
    Forbidden,
    Cancelled,
}

/// 一次门禁的代理身份（ADR-103 审批冒泡与记忆作用域）。
#[derive(Debug, Clone, Copy)]
pub struct GateSubject<'a> {
    /// 触发本次调用的代理；审批记忆按它分区。
    pub agent_id: &'a str,
    /// 子代理任务摘要；root 为 None。
    pub task_summary: Option<&'a str>,
    /// 本代理的有效工具白名单；None = 无额外限制。
    pub allowed_tools: Option<&'a ToolAllowlist>,
}

impl<'a> GateSubject<'a> {
    /// root 代理（无任务摘要、无白名单收窄）。
    pub fn root(agent_id: &'a str) -> Self {
        Self { agent_id, task_summary: None, allowed_tools: None }
    }
}

/// 门禁的执行环境：事件出口与取消令牌成对出现。
/// 打包只为一件事：`gate` 的参数已经承载了全部安全输入（身份/白名单/策略），
/// 执行环境再逐项铺开就没人能一眼看清哪些是"决策依据"、哪些是"投递手段"。
#[derive(Clone, Copy)]
pub struct GateEnv<'a> {
    pub sink: &'a Arc<dyn EventSink>,
    pub cancel: &'a CancellationToken,
}

/// 审批 broker：pending 请求表 + execute 级会话记忆（按 agent_id 分域）。
#[derive(Default)]
pub struct ApprovalBroker {
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalDecision>>>,
    /// `agent_id → 已记住的 execute 工具`。ADR-103：子代理的 AllowAlways 只对它自己
    /// 生效，不能放行父代理或兄弟代理——按 agent 分域是这条语义的唯一实现点。
    remembered_execute: Mutex<HashMap<String, HashSet<String>>>,
}

impl ApprovalBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 工具执行前的门禁：白名单 → 裁决 → 必要时发审批请求并等待回执。
    ///
    /// 白名单先于风险分级：被收窄掉的工具连询问都不问（问一次就执行一次
    /// 等于把"只降不升"变成了"可以申请提权"，ADR-103 不允许）。
    pub async fn gate(
        &self,
        tool: &str,
        args: &Value,
        call_id: &str,
        policy: policy::SessionPolicy,
        subject: GateSubject<'_>,
        env: GateEnv<'_>,
    ) -> GateOutcome {
        if env.cancel.is_cancelled() {
            return GateOutcome::Cancelled;
        }
        if let Some(allowlist) = subject.allowed_tools {
            if !allowlist.contains(tool) {
                return GateOutcome::Forbidden;
            }
        }
        let risk = risk_of(tool);
        let remembered = self
            .remembered_execute
            .lock()
            .expect("remembered 锁中毒")
            .get(subject.agent_id)
            .is_some_and(|tools| tools.contains(tool));
        match policy::decide(risk, policy.approval, policy.sandbox, remembered) {
            Decision::Allow => GateOutcome::Allowed,
            Decision::Deny => GateOutcome::Forbidden,
            Decision::Prompt => {
                self.request_approval(tool, args, call_id, risk, subject, env).await
            }
        }
    }

    /// 发审批请求并挂起等待；取消时撤单。
    async fn request_approval(
        &self,
        tool: &str,
        args: &Value,
        call_id: &str,
        risk: RiskLevel,
        subject: GateSubject<'_>,
        env: GateEnv<'_>,
    ) -> GateOutcome {
        let request_id = format!("appr-{call_id}");
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        self.pending
            .lock()
            .expect("pending 锁中毒")
            .insert(request_id.clone(), tx);
        let _ = env.sink.emit(&EventMsg::ExecApprovalRequest {
            request_id: request_id.clone(),
            tool: tool.to_string(),
            args: args.clone(),
            risk,
            // ADR-103 审批冒泡：请求自带来源代理与任务摘要，前端不必从泳道
            // 元数据反推"是谁在要权限"——反推在泳道未建立时必然失败。
            agent_id: Some(subject.agent_id.to_string()),
            task_summary: subject.task_summary.map(str::to_string),
        });
        let outcome = tokio::select! {
            resp = rx => match resp {
                // 允许一次 = 只放行这一次，**不记忆**。ADR-003 的"本会话记住"只由
                // AllowAlways 触发；把 Allow 也写进 remembered_execute 会让用户点一次
                // 「允许一次」后，同进程后续 execute 全部静默放行（真机复测发现）。
                Ok(ApprovalDecision::Allow) => GateOutcome::Allowed,
                Ok(d @ ApprovalDecision::AllowAlways) => {
                    self.remember_if_execute(tool, risk, subject.agent_id);
                    GateOutcome::from(d)
                }
                Ok(ApprovalDecision::Deny) => GateOutcome::Denied,
                Err(_) => GateOutcome::Cancelled, // 发送端已被撤单
            },
            _ = env.cancel.cancelled() => GateOutcome::Cancelled,
        };
        if outcome != GateOutcome::Allowed {
            // 未放行的请求撤单（已 resolve 的 remove 无副作用）
            self.pending.lock().expect("pending 锁中毒").remove(&request_id);
        }
        outcome
    }

    fn remember_if_execute(&self, tool: &str, risk: RiskLevel, agent_id: &str) {
        if risk == RiskLevel::Execute {
            self.remembered_execute
                .lock()
                .expect("remembered 锁中毒")
                .entry(agent_id.to_string())
                .or_default()
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
    use std::sync::{Arc, Mutex};

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
            .gate("read_file", &serde_json::json!({}), "c0", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::ReadOnly }, GateSubject::root("root"), GateEnv { sink: &sink(), cancel: &cancel })
            .await;
        assert_eq!(out, GateOutcome::Allowed);
    }

    #[tokio::test]
    async fn write_denied_in_readonly_sandbox() {
        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let out = broker
            .gate("apply_patch", &serde_json::json!({}), "c1", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::ReadOnly }, GateSubject::root("root"), GateEnv { sink: &sink(), cancel: &cancel })
            .await;
        assert_eq!(out, GateOutcome::Forbidden, "沙箱拒绝属于策略拒绝，不是用户拒绝");
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
            b.gate("unified_exec", &args(), "c2", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s, cancel: &c }).await
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
            b.gate("unified_exec", &args(), "c3", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s2, cancel: &c2 }).await
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
            .gate("unified_exec", &args(), "c4", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &sink, cancel: &cancel })
            .await;
        assert_eq!(out, GateOutcome::Allowed);
    }

    #[tokio::test]
    async fn allow_once_does_not_remember_execute() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let sink = sink();
        // 第一次 execute：允许一次
        let b = broker.clone();
        let s = sink.clone();
        let c1 = cancel.clone();
        let gate1 = tokio::spawn(async move {
            b.gate("unified_exec", &args(), "c6", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s, cancel: &c1 }).await
        });
        let mut first_prompted = false;
        for _ in 0..200 {
            if broker.has_pending("appr-c6") {
                first_prompted = true;
                assert!(broker.resolve("appr-c6", ApprovalDecision::Allow));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(first_prompted, "第一次 execute 必须挂起等审批");
        assert_eq!(gate1.await.unwrap(), GateOutcome::Allowed);
        // 第二次 execute 必须重新询问：Allow 不得写入 remembered_execute
        let b = broker.clone();
        let s2 = sink.clone();
        let c2 = cancel.clone();
        let gate2 = tokio::spawn(async move {
            b.gate("unified_exec", &args(), "c7", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s2, cancel: &c2 }).await
        });
        let mut prompted = false;
        for _ in 0..200 {
            if broker.has_pending("appr-c7") {
                prompted = true;
                assert!(broker.resolve("appr-c7", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(prompted, "「允许一次」不得记忆 execute：第二次必须挂起等审批");
        assert_eq!(gate2.await.unwrap(), GateOutcome::Denied);
    }

    #[tokio::test]
    async fn cancel_during_prompt_cancels_and_unregisters() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let gate = tokio::spawn({
            let b = broker.clone();
            let s = sink();
            let c = cancel.clone();
            async move { b.gate("unified_exec", &args(), "c5", policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s, cancel: &c }).await }
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

    /// 收窄后的工具必须直接拒绝，且**不得**发审批请求：问一次就能放行
    /// 等于把「只降不升」变成「可以申请提权」（ADR-103）。
    #[tokio::test]
    async fn allowlist_denies_unlisted_tool_without_prompt() {
        #[derive(Default)]
        struct CapturingSink(Mutex<Vec<EventMsg>>);
        impl EventSink for CapturingSink {
            fn emit(&self, msg: &EventMsg) -> Result<(), String> {
                self.0.lock().unwrap().push(msg.clone());
                Ok(())
            }
        }

        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let events = Arc::new(CapturingSink::default());
        let allowlist: ToolAllowlist =
            Arc::new(["read_file".to_string()].into_iter().collect());
        let subject = GateSubject {
            agent_id: "child-1",
            task_summary: Some("只读探索"),
            allowed_tools: Some(&allowlist),
        };
        let out = broker
            .gate(
                "unified_exec",
                &args(),
                "c8",
                policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite },
                subject,
                GateEnv { sink: &(events.clone() as Arc<dyn EventSink>), cancel: &cancel },
            )
            .await;
        assert_eq!(out, GateOutcome::Forbidden);
        assert!(
            events.0.lock().unwrap().is_empty(),
            "白名单外的工具连审批都不该发，否则等于给模型一次提权申请入口"
        );
    }

    /// ADR-103：AllowAlways 的记忆按 agent_id 分域——子代理放行过的 execute
    /// 不得让父代理或兄弟代理静默放行。
    #[tokio::test]
    async fn allow_always_memory_is_scoped_to_the_requesting_agent() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let policy = policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite };

        // 子代理 A 选择「本会话记住」
        let gate_a = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            async move {
                broker
                    .gate("unified_exec", &args(), "c9", policy, GateSubject::root("child-a"), GateEnv { sink: &sink(), cancel: &cancel })
                    .await
            }
        });
        for _ in 0..200 {
            if broker.has_pending("appr-c9") {
                assert!(broker.resolve("appr-c9", ApprovalDecision::AllowAlways));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(gate_a.await.unwrap(), GateOutcome::Allowed);

        // 兄弟代理 B 必须重新询问，而不是继承 A 的记忆
        let gate_b = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            async move {
                broker
                    .gate("unified_exec", &args(), "c10", policy, GateSubject::root("child-b"), GateEnv { sink: &sink(), cancel: &cancel })
                    .await
            }
        });
        let mut prompted = false;
        for _ in 0..200 {
            if broker.has_pending("appr-c10") {
                prompted = true;
                assert!(broker.resolve("appr-c10", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(prompted, "兄弟代理不得继承另一个子代理的 AllowAlways 记忆");
        assert_eq!(gate_b.await.unwrap(), GateOutcome::Denied);
    }

    /// 审批冒泡（ADR-103）：请求事件必须自带来源代理与任务摘要。
    #[tokio::test]
    async fn approval_request_carries_origin_agent_and_task_summary() {
        #[derive(Default)]
        struct CapturingSink(Mutex<Vec<EventMsg>>);
        impl EventSink for CapturingSink {
            fn emit(&self, msg: &EventMsg) -> Result<(), String> {
                self.0.lock().unwrap().push(msg.clone());
                Ok(())
            }
        }

        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let events = Arc::new(CapturingSink::default());
        let gate = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let events = events.clone();
            async move {
                broker
                    .gate(
                        "unified_exec",
                        &args(),
                        "c11",
                        policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite },
                        GateSubject {
                            agent_id: "child-9",
                            task_summary: Some("统计仓库文件数"),
                            allowed_tools: None,
                        },
                        GateEnv { sink: &(events as Arc<dyn EventSink>), cancel: &cancel },
                    )
                    .await
            }
        });
        for _ in 0..200 {
            if broker.has_pending("appr-c11") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let request = events.0.lock().unwrap().iter().find_map(|event| match event {
            EventMsg::ExecApprovalRequest { agent_id, task_summary, .. } => {
                Some((agent_id.clone(), task_summary.clone()))
            }
            _ => None,
        });
        assert_eq!(
            request,
            Some((Some("child-9".to_string()), Some("统计仓库文件数".to_string())))
        );

        cancel.cancel();
        let _ = gate.await.unwrap();
    }
}
