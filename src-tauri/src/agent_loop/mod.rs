//! turn 主循环（docs §8 agent_loop/mod.rs，ADR-001）：
//! 请求模型（流式）→ 收集 tool calls → 执行 → 结果回填 → 再请求。
//!
//! 单一职责边界：
//! - 模型流经 `ModelBackend` 注入（真实 Responses 实现 + 测试脚本实现）；
//! - 工具执行经 `ToolRouter`（M2 起在路由层前置审批裁决）；
//! - 事件全部经 `EventSink` 广播；取消经 `CancellationToken`（drop 模型流断开连接）。
//!
//! 每个任务 = 若干 turn；turn 内无工具调用即任务完成（completed）。

pub mod budget;

use std::path::PathBuf;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::events::EventMsg;
use crate::model_client::{
    self, provider::ProviderConfig, HistoryItem, ModelStreamEvent, ResponseItem, Role, Usage,
};
use crate::tool_router::{ToolContext, ToolRouter};

pub use budget::TurnBudget;

// ============================== 模型后端抽象 ==============================

/// 模型流（agent_loop 只依赖此别名；Pin<Box> 保证可安全轮询）。
pub type ModelStream = std::pin::Pin<Box<dyn Stream<Item = ModelStreamEvent> + Send>>;

/// 模型后端：把 (历史, 工具声明) 变成模型事件流。
#[async_trait::async_trait]
pub trait ModelBackend: Send + Sync {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
    ) -> Result<ModelStream, String>;
}

/// 避免 agent_loop 直接耦合 model_client 的声明类型（转发即可，无重复定义）。
pub use crate::model_client::ToolDeclaration as ToolDeclarationShim;

/// 真实后端：Responses SSE（密钥由调用方从 SecretStore 解密后注入）。
pub struct ResponsesBackend {
    pub provider: ProviderConfig,
    pub api_key: String,
}

#[async_trait::async_trait]
impl ModelBackend for ResponsesBackend {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
    ) -> Result<ModelStream, String> {
        let stream = model_client::stream_responses(&self.provider, &self.api_key, history, tools).await?;
        Ok(Box::pin(stream))
    }
}

// ============================== 循环本体 ==============================

pub struct AgentLoop {
    pub backend: Arc<dyn ModelBackend>,
    pub router: Arc<ToolRouter>,
    pub workspace_root: PathBuf,
    pub max_turns: u32,
}

impl AgentLoop {
    pub fn from_config(
        backend: Arc<dyn ModelBackend>,
        router: Arc<ToolRouter>,
        cfg: &AppConfig,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            backend,
            router,
            workspace_root,
            max_turns: TurnBudget::from_config(cfg).max_turns,
        }
    }

    /// 执行一个完整任务（若干 turn）。失败/取消以 TurnCompleted 事件收口，
    /// 同时返回 TurnSummary（永不 panic、错误不静默）。
    pub async fn run_task(
        &self,
        task: &str,
        sink: Arc<dyn crate::events::EventSink>,
        cancel: CancellationToken,
    ) -> TurnSummary {
        let mut history = vec![HistoryItem::Message { role: Role::User, text: task.to_string() }];
        let mut reason = TurnEndReasonLocal::Completed;
        let mut last_turn = 0u32;
        let emit = |sink: &Arc<dyn crate::events::EventSink>, msg: EventMsg| {
            let _ = sink.emit(&msg);
        };

        'outer: for turn in 1..=self.max_turns {
            last_turn = turn;
            emit(&sink, EventMsg::TurnStarted { turn });

            // 1) 采样（流式）：取消 → drop 流 = 断开连接
            let mut stream = match self
                .backend
                .stream(&history, &self.router.declarations())
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    emit(&sink, EventMsg::Error { message: e });
                    reason = TurnEndReasonLocal::Failed;
                    break 'outer;
                }
            };

            // 2) 消费流：转发增量、收集输出项
            let mut assistant_text = String::new();
            let mut full_message: Option<String> = None;
            let mut calls: Vec<(String, String, String)> = Vec::new(); // (name, arguments, call_id)
            let mut usage: Option<Usage> = None;
            loop {
                let mut stream_pin = stream.as_mut(); // Pin<&mut dyn S>，Next 借用其存活
                let next_item = stream_pin.next();
                tokio::select! {
                    _ = cancel.cancelled() => {
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                    item = next_item => match item {
                        None => break,
                        Some(ModelStreamEvent::OutputTextDelta { delta }) => {
                            assistant_text.push_str(&delta);
                            emit(&sink, EventMsg::AgentMessageDelta { delta });
                        }
                        Some(ModelStreamEvent::OutputItem(ResponseItem::Message { role: _, text })) => {
                            full_message = Some(text);
                        }
                        Some(ModelStreamEvent::OutputItem(ResponseItem::FunctionCall { name, arguments, call_id })) => {
                            calls.push((name, arguments, call_id));
                        }
                        Some(ModelStreamEvent::Completed { usage: u }) => {
                            usage = Some(u);
                        }
                        Some(ModelStreamEvent::Failed { message }) => {
                            emit(&sink, EventMsg::Error { message });
                            reason = TurnEndReasonLocal::Failed;
                            break 'outer;
                        }
                    }
                }
            }
            if let Some(u) = usage {
                emit(&sink, EventMsg::TokenUsage {
                    turn,
                    input_tokens: u.input_tokens,
                    cached_input_tokens: u.cached_input_tokens,
                    output_tokens: u.output_tokens,
                });
            }

            // 3) 助手输出入历史（完整消息优先，增量累计兜底）
            let text = full_message.unwrap_or(assistant_text);
            if !text.trim().is_empty() {
                history.push(HistoryItem::Message { role: Role::Assistant, text });
            }

            // 4) 无工具调用 → 任务完成
            if calls.is_empty() {
                break;
            }

            // 5) 本轮已到预算上限且仍需工具 → 预算耗尽
            if turn == self.max_turns {
                emit(&sink, EventMsg::Error { message: format!("已达最大轮数上限（{turn}）") });
                reason = TurnEndReasonLocal::BudgetExhausted;
                break;
            }

            // 6) 工具调用入历史 → 执行 → 输出回填
            for (name, arguments, call_id) in &calls {
                history.push(HistoryItem::FunctionCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                    call_id: call_id.clone(),
                });
            }
            for (name, arguments, call_id) in &calls {
                emit(&sink, EventMsg::ToolCallBegin {
                    call_id: call_id.clone(),
                    tool: name.clone(),
                    args: serde_json::from_str(arguments)
                        .unwrap_or(serde_json::Value::String(arguments.clone())),
                });
                let tool_ctx = ToolContext::new(
                    self.workspace_root.clone(),
                    cancel.clone(),
                    call_id.clone(),
                    sink.clone(),
                );
                let result = self.router.execute(name, parse_args(arguments), &tool_ctx).await;
                let output_text = match &result {
                    Ok(out) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: Some(i32::from(!out.success)),
                            error: None,
                        });
                        out.output.clone()
                    }
                    Err(crate::tool_router::ToolError::Cancelled) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("已取消".into()),
                        });
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                    Err(e) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some(e.to_string()),
                        });
                        e.to_string()
                    }
                };
                history.push(HistoryItem::FunctionCallOutput {
                    call_id: call_id.clone(),
                    output: output_text,
                });
            }
        }

        emit(&sink, EventMsg::TurnCompleted {
            turn: last_turn,
            reason: match reason {
                TurnEndReasonLocal::Completed => crate::events::TurnEndReason::Completed,
                TurnEndReasonLocal::Cancelled => crate::events::TurnEndReason::Cancelled,
                TurnEndReasonLocal::Failed => crate::events::TurnEndReason::Failed,
                TurnEndReasonLocal::BudgetExhausted => crate::events::TurnEndReason::BudgetExhausted,
            },
        });
        TurnSummary { turns: last_turn, reason: reason.into() }
    }
}

fn parse_args(arguments: &str) -> serde_json::Value {
    serde_json::from_str(arguments).unwrap_or(serde_json::Value::String(arguments.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnEndReasonLocal {
    Completed,
    Cancelled,
    Failed,
    BudgetExhausted,
}

impl From<TurnEndReasonLocal> for crate::events::TurnEndReason {
    fn from(r: TurnEndReasonLocal) -> Self {
        match r {
            TurnEndReasonLocal::Completed => Self::Completed,
            TurnEndReasonLocal::Cancelled => Self::Cancelled,
            TurnEndReasonLocal::Failed => Self::Failed,
            TurnEndReasonLocal::BudgetExhausted => Self::BudgetExhausted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnSummary {
    pub turns: u32,
    pub reason: crate::events::TurnEndReason,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventSink, TurnEndReason};
    use crate::model_client::ToolDeclaration;
    use crate::tool_router::{ToolError, ToolExecutor, ToolOutput, ToolSpec};
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ---- 测试基建：捕获 sink + 脚本后端 + echo 工具 ----

    #[derive(Clone)]
    struct CapturingSink(Arc<Mutex<Vec<EventMsg>>>);

    impl CapturingSink {
        fn new() -> (Self, Arc<Mutex<Vec<EventMsg>>>) {
            let store = Arc::new(Mutex::new(Vec::new()));
            (Self(store.clone()), store)
        }
    }

    impl EventSink for CapturingSink {
        fn emit(&self, msg: &EventMsg) -> Result<(), String> {
            self.0.lock().unwrap().push(msg.clone());
            Ok(())
        }
    }

    struct ScriptedBackend(Mutex<VecDeque<Vec<ModelStreamEvent>>>);

    impl ScriptedBackend {
        fn new(turns: Vec<Vec<ModelStreamEvent>>) -> Self {
            Self(Mutex::new(turns.into()))
        }
    }

    #[async_trait::async_trait]
    impl ModelBackend for ScriptedBackend {
        async fn stream(
            &self,
            _history: &[HistoryItem],
            _tools: &[ToolDeclaration],
        ) -> Result<ModelStream, String> {
            let mut q = self.0.lock().unwrap();
            let script = q.pop_front().ok_or("脚本耗尽")?;
            Ok(Box::pin(futures::stream::iter(script)))
        }
    }

    struct EchoTool {
        spec: ToolSpec,
    }

    impl Default for EchoTool {
        fn default() -> Self {
            Self {
                spec: ToolSpec::new("echo", "回显", serde_json::json!({"type":"object"})),
            }
        }
    }

    #[async_trait::async_trait]
    impl ToolExecutor for EchoTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(format!("echo:{args}")))
        }
    }

    fn make_loop(
        turns: Vec<Vec<ModelStreamEvent>>,
    ) -> (AgentLoop, Arc<Mutex<Vec<EventMsg>>>, CancellationToken) {
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        let (_sink, store) = CapturingSink::new();
        let agent = AgentLoop {
            backend: Arc::new(ScriptedBackend::new(turns)),
            router: Arc::new(router),
            workspace_root: std::env::temp_dir(),
            max_turns: 3,
        };
        (agent, store, CancellationToken::new())
    }

    fn usage_ev() -> ModelStreamEvent {
        ModelStreamEvent::Completed { usage: Usage { input_tokens: 10, cached_input_tokens: None, output_tokens: 5 } }
    }

    fn fn_call_ev() -> ModelStreamEvent {
        ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
            name: "echo".into(),
            arguments: r#"{"x":1}"#.into(),
            call_id: "c1".into(),
        })
    }

    // ---- 用例 ----

    #[tokio::test]
    async fn single_turn_completion_emits_expected_events() {
        let (agent, store, cancel) = make_loop(vec![vec![
            ModelStreamEvent::OutputTextDelta { delta: "你好".into() },
            usage_ev(),
        ]]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        let tags: Vec<&str> = events.iter().map(|e| match e {
            EventMsg::TurnStarted { .. } => "turn_started",
            EventMsg::AgentMessageDelta { .. } => "delta",
            EventMsg::TokenUsage { .. } => "usage",
            EventMsg::TurnCompleted { .. } => "turn_completed",
            other => panic!("意外事件: {other:?}"),
        }).collect();
        assert_eq!(tags, ["turn_started", "delta", "usage", "turn_completed"]);
    }

    #[tokio::test]
    async fn tool_call_cycle_then_complete() {
        let (agent, store, cancel) = make_loop(vec![
            vec![fn_call_ev(), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }, usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallBegin { call_id, tool, .. } if call_id == "c1" && tool == "echo"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(0), error: None } if call_id == "c1"
        )));
    }

    #[tokio::test]
    async fn budget_exhausted_when_tools_keep_coming() {
        let (agent, store, cancel) = make_loop(vec![
            vec![fn_call_ev(), usage_ev()],
            vec![fn_call_ev(), usage_ev()],
            vec![fn_call_ev(), usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::BudgetExhausted);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::Error { message } if message.contains("最大轮数")
        )));
    }

    #[tokio::test]
    async fn cancelled_token_yields_cancelled_completion() {
        let (agent, store, cancel) = make_loop(vec![vec![ModelStreamEvent::OutputTextDelta { delta: "x".into() }]]);
        cancel.cancel();
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Cancelled);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::TurnCompleted { reason: TurnEndReason::Cancelled, .. }
        )));
    }

    #[tokio::test]
    async fn model_failure_maps_to_failed() {
        let (agent, store, cancel) = make_loop(vec![vec![ModelStreamEvent::Failed { message: "boom".into() }]]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Failed);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::Error { message } if message == "boom"
        )));
    }

    #[tokio::test]
    async fn unknown_tool_error_backfills_and_continues() {
        let (agent, store, cancel) = make_loop(vec![
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "no_such_tool".into(),
                arguments: "{}".into(),
                call_id: "c9".into(),
            }), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "已纠正".into() }, usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, error: Some(msg), .. } if call_id == "c9" && msg.contains("未知工具")
        )));
    }
}
