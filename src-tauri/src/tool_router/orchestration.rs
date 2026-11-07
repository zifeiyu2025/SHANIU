//! 编排能力契约（SH-052，docs/02 §2.2）。
//!
//! 放这里而不是 `tools/orchestrate.rs` 的原因：四个工具只应「翻译参数 + 调能力」，
//! 不能直接依赖 `agent_loop`/`subagent_registry`——那会让 `agent_loop → tool_router →
//! tools → agent_loop` 成环（FF-1）。具体实现由组合根注入。
//!
//! `tool_router` 不反向依赖 `agent_loop`/`subagent_registry`，只持本文件的 DTO 与 trait。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::events::EventSink;

/// 编排四工具的固定名单（声明过滤与执行门控共用同一份，避免两处漂移）。
pub const AGENT_TOOL_NAMES: [&str; 4] = ["spawn_agent", "resume_agent", "wait_agent", "send_agent_message"];

pub fn is_agent_tool(name: &str) -> bool {
    AGENT_TOOL_NAMES.contains(&name)
}

/// 工具执行时「谁在调用」：身份、父边、深度、取消令牌、事件出口。
///
/// 由 `agent_loop::run_instance` 从 `AgentInstance` 构造；工具与编排实现都只读它。
#[derive(Clone)]
pub struct AgentScope {
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub depth: u32,
    pub cancel: CancellationToken,
    pub sink: Arc<dyn EventSink>,
}

/// 子代理终态投影（`pending` = 超时仍未结束，docs/02 §2.2「超时不杀代理」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutcomeState {
    Done,
    Failed,
    Cancelled,
    Pending,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

/// `wait_agent` 回填给父模型的结构化结果（docs/02 §2.1 ExitSummary 的投影）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentOutcome {
    pub agent_id: String,
    pub state: AgentOutcomeState,
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<AgentTokenUsage>,
}

/// 编排能力：由组合根注入到 `ToolContext`（实现见 `subagent_registry`）。
#[async_trait]
pub trait AgentOrchestrator: Send + Sync {
    /// 派生一个子代理，立即返回 id（不阻塞）。
    ///
    /// `budget_tokens` = 该子树自己的 token 上限（`spawn_agent.budget_tokens`）：
    /// 它不会放宽全树预算，只能在全树之内进一步收窄。
    async fn spawn_agent(
        &self,
        parent: &AgentScope,
        task: &str,
        budget_tokens: Option<u64>,
    ) -> Result<String, String>;

    /// 从一个已退出子代理的历史恢复继续工作；`task` 为 None 时用默认续跑指令。
    async fn resume_agent(
        &self,
        parent: &AgentScope,
        agent_id: &str,
        task: Option<&str>,
    ) -> Result<String, String>;

    /// 等待一组子代理；共享一个总超时，超时项以 `pending` 返回，不杀代理。
    async fn wait_agents(&self, agent_ids: &[String], timeout: Duration) -> Vec<AgentOutcome>;

    /// 向运行中的子代理注入一条 steering 消息（进入其下一轮上下文）。
    fn send_message(&self, agent_id: &str, message: &str) -> Result<(), String>;
}
