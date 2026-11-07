//! 代理实例（SH-050）：`AgentLoop` 是共享执行引擎，`AgentInstance` 是每个代理独有的运行态。
//!
//! root 与子代理走同一套 turn 主循环（ADR-101「子代理即复用 agent_loop，避免第二套执行
//! 引擎」）。差异只在这里：身份与父边、深度、取消令牌、请求历史、落盘游标、档案写入器、
//! 作业属主。把这些从 `AgentLoop` 里拆出来，子代理才可能在同一个引擎上并行运行而不共享
//! 任何可变状态。

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::model_client::{HistoryItem, Role, Usage};
use crate::rollout::RolloutWriter;

/// 子代理允许的最大嵌套深度（docs/02 §2.3 决策：硬限 2 = root(0) + 子(1)）。
pub const MAX_AGENT_DEPTH: u32 = 1;

/// 单个代理实例的累计用量（docs/02 §2.1 ExitSummary 的 `token_usage` 来源）。
///
/// 与预算共账（SH-055）区分：这里只统计「本实例实际消耗」，不做树级合计与硬顶判定；
/// 父子汇总由注册表/编排层负责。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenTotals {
    /// 累加一次模型响应上报的用量。端点不报 usage 时保持 0，不猜估算值。
    pub fn absorb(&mut self, usage: &Usage) {
        self.input_tokens += u64::from(usage.input_tokens);
        if let Some(cached) = usage.cached_input_tokens {
            self.cached_input_tokens += u64::from(cached);
        }
        self.output_tokens += u64::from(usage.output_tokens);
    }
}

pub struct AgentInstance {
    /// 全局唯一 id；root 代理固定为 `root`（docs/02 §2.1）。
    pub agent_id: String,
    /// 父代理 id；root 为 None。
    pub parent_id: Option<String>,
    /// root = 0，子代理 = 1（`MAX_AGENT_DEPTH` 之上不允许再 spawn）。
    pub depth: u32,
    /// 本实例的取消令牌。父取消时由注册表级联取消子实例的令牌。
    pub cancel: CancellationToken,
    /// 独立请求历史：子代理**不继承**父历史（ADR-102）。
    pub history: Vec<HistoryItem>,
    /// `history` 中已经落盘的条数（resume/续跑用；子代理从 0 起）。
    pub recorded: usize,
    /// 本实例的档案写入器；None = 本次运行不落盘（测试、无目录启动）。
    pub rollout: Option<Arc<RolloutWriter>>,
    /// 后台作业属主（SH-089）。None 时沿用 `AgentLoop` 的会话 id。
    pub session_id: Option<String>,
    /// 本实例累计 token 用量（ExitSummary 的 `token_usage`，见 `TokenTotals`）。
    pub usage: TokenTotals,
    /// 父代理经 `send_agent_message` 注入的 steering 消息（SH-052）。
    /// 每轮采样前由 `run_instance` 排空并追加为 user 消息；注册表持同一 Arc 用于写入。
    pub inbox: Arc<Mutex<Vec<String>>>,
}

impl AgentInstance {
    /// root 代理实例：`history` 由调用方给（新会话 = 一条 user 消息；resume = 重放历史）。
    pub fn root(history: Vec<HistoryItem>, persisted: usize, cancel: CancellationToken) -> Self {
        Self {
            agent_id: "root".to_string(),
            parent_id: None,
            depth: 0,
            cancel,
            recorded: persisted.min(history.len()),
            history,
            rollout: None,
            session_id: None,
            usage: TokenTotals::default(),
            inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 子代理实例：初始上下文只有 spawn 的 task 描述（ADR-102，不继承父历史）。
    pub fn child(
        agent_id: impl Into<String>,
        parent_id: impl Into<String>,
        task: &str,
        cancel: CancellationToken,
    ) -> Self {
        let history = vec![HistoryItem::Message {
            role: Role::User,
            text: task.to_string(),
            reasoning: None,
        }];
        Self {
            agent_id: agent_id.into(),
            parent_id: Some(parent_id.into()),
            depth: 1,
            cancel,
            recorded: 0,
            history,
            rollout: None,
            session_id: None,
            usage: TokenTotals::default(),
            inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 排空并返回尚未消费的 steering 消息（每轮采样前调用）。
    pub fn drain_messages(&self) -> Vec<String> {
        let mut inbox = self.inbox.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *inbox)
    }

    /// 深度上的 spawn 资格：只有 root 能继续分叉（ADR-101 硬限）。
    pub fn can_spawn(&self) -> bool {
        self.depth < MAX_AGENT_DEPTH
    }

    /// 本实例最后一条 assistant 文本（子代理 ExitSummary 的摘要来源）。
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.history.iter().rev().find_map(|item| match item {
            HistoryItem::Message { role: Role::Assistant, text, .. } if !text.trim().is_empty() => {
                Some(text.as_str())
            }
            _ => None,
        })
    }
}
