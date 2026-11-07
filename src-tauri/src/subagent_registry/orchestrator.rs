//! `AgentOrchestrator` 的注册表实现（SH-052，docs/02 §2.2）。
//!
//! 放在 `subagent_registry` 而不是 `tools/orchestrate.rs`：工具层只持 trait，
//! 实现需要同时摸到注册表与执行引擎，属于组合根的装配职责（FF-1 无环）。
//! 依赖方向：`subagent_registry → tool_router`（E45）、`subagent_registry → agent_loop`（E42）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::agent_loop::AgentLoop;
use crate::subagent_registry::{ExitStatus, ExitSummary, SubagentRegistry};
use crate::tool_router::orchestration::{
    AgentOrchestrator, AgentOutcome, AgentOutcomeState, AgentScope, AgentTokenUsage,
};

/// 把注册表与执行引擎绑定成 root 代理可用的编排能力。
pub struct RegistryOrchestrator {
    registry: Arc<SubagentRegistry>,
    engine: Arc<AgentLoop>,
}

impl RegistryOrchestrator {
    pub fn new(registry: Arc<SubagentRegistry>, engine: Arc<AgentLoop>) -> Self {
        Self { registry, engine }
    }

    pub fn registry(&self) -> &Arc<SubagentRegistry> {
        &self.registry
    }
}

#[async_trait]
impl AgentOrchestrator for RegistryOrchestrator {
    async fn spawn_agent(
        &self,
        parent: &AgentScope,
        task: &str,
        budget_tokens: Option<u64>,
        profile: Option<&str>,
        allowed_tools: Option<&[String]>,
    ) -> Result<String, String> {
        // 每代理 rollout 在 SH-056 落地；当前子代理不落盘（诚实降级，见 docs/05 SH-052）。
        self.registry
            .spawn_with_constraints(
                self.engine.clone(),
                parent,
                task,
                None,
                budget_tokens,
                crate::subagent_registry::SpawnNarrowing { profile, allowed_tools },
            )
            .map_err(|error| error.to_string())
    }

    async fn resume_agent(
        &self,
        parent: &AgentScope,
        agent_id: &str,
        task: Option<&str>,
    ) -> Result<String, String> {
        self.registry
            .resume(self.engine.clone(), parent, agent_id, task, None)
            .map_err(|error| error.to_string())
    }

    async fn wait_agents(&self, agent_ids: &[String], timeout: Duration) -> Vec<AgentOutcome> {
        let mut outcomes = Vec::new();
        let mut waiting: Vec<String> = Vec::new();
        for agent_id in agent_ids {
            if self.registry.status_of(agent_id).is_some() {
                waiting.push(agent_id.clone());
            } else {
                outcomes.push(pending(agent_id, "未知子代理 id（不在本会话注册表中）。"));
            }
        }

        let deadline = tokio::time::Instant::now() + timeout;
        while !waiting.is_empty() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let futures = waiting
                .iter()
                .map(|agent_id| {
                    let agent_id = agent_id.clone();
                    let registry = self.registry.clone();
                    let future = async move {
                        let exit = registry.wait_exit(&agent_id, remaining).await;
                        (agent_id, exit)
                    };
                    Box::pin(future)
                        as std::pin::Pin<
                            Box<dyn std::future::Future<Output = (String, Option<ExitSummary>)> + Send>,
                        >
                })
                .collect::<Vec<_>>();
            // 并发等待：先结束的先回，慢的不阻塞快的；超时项按 pending 返回，不杀代理。
            let ((agent_id, exit), _, _) = futures::future::select_all(futures).await;
            waiting.retain(|candidate| candidate != &agent_id);
            outcomes.push(match exit {
                Some(exit) => outcome_from_exit(&agent_id, exit),
                None => pending(&agent_id, "等待超时，子代理仍在运行；稍后再调用 wait_agent。"),
            });
        }
        outcomes
    }

    fn send_message(&self, agent_id: &str, message: &str) -> Result<(), String> {
        self.registry.send_message(agent_id, message, None).map_err(|error| error.to_string())
    }
}

fn outcome_from_exit(agent_id: &str, exit: ExitSummary) -> AgentOutcome {
    let state = match exit.status {
        ExitStatus::Done => AgentOutcomeState::Done,
        ExitStatus::Failed => AgentOutcomeState::Failed,
        ExitStatus::Cancelled => AgentOutcomeState::Cancelled,
    };
    AgentOutcome {
        agent_id: agent_id.to_string(),
        state,
        summary: exit.summary,
        artifacts: exit.artifacts,
        token_usage: Some(AgentTokenUsage {
            input_tokens: exit.token_usage.input_tokens,
            cached_input_tokens: exit.token_usage.cached_input_tokens,
            output_tokens: exit.token_usage.output_tokens,
        }),
    }
}

fn pending(agent_id: &str, summary: &str) -> AgentOutcome {
    AgentOutcome {
        agent_id: agent_id.to_string(),
        state: AgentOutcomeState::Pending,
        summary: summary.to_string(),
        artifacts: Vec::new(),
        token_usage: None,
    }
}
