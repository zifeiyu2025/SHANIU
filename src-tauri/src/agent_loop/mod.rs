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
pub mod prompt;

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::events::{EventMsg, ToolCallResult};
use crate::model_client::{
    self, provider::ProviderConfig, HistoryItem, ModelStreamEvent, ResponseItem, Role, Usage,
};
use crate::permission::{policy, ApprovalBroker};
use crate::tool_router::{PlanState, ToolContext, ToolRouter};

pub use budget::TurnBudget;

// ============================== 模型后端抽象 ==============================

/// 模型流（转发 model_client 的别名）。
pub use crate::model_client::ModelStream;

/// 模型后端：把 (历史, 工具声明, 系统提示) 变成模型事件流。
///
/// `instructions` 每轮由 `prompt::system_instruction` 生成（docs/04 §6.1），
/// 不写入 history，避免逐轮重复累积。
#[async_trait::async_trait]
pub trait ModelBackend: Send + Sync {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
        instructions: &str,
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
        instructions: &str,
    ) -> Result<ModelStream, String> {
        let stream =
            model_client::stream_model(&self.provider, &self.api_key, history, tools, instructions)
                .await?;
        Ok(Box::pin(stream))
    }
}

// ============================== 循环本体 ==============================

pub struct AgentLoop {
    pub backend: Arc<dyn ModelBackend>,
    pub router: Arc<ToolRouter>,
    pub broker: Arc<ApprovalBroker>,
    pub plan_state: Arc<PlanState>,
    pub workspace_root: PathBuf,
    pub max_turns: u32,
    pub policy: policy::SessionPolicy,
}

impl AgentLoop {
    pub fn from_config(
        backend: Arc<dyn ModelBackend>,
        router: Arc<ToolRouter>,
        broker: Arc<ApprovalBroker>,
        plan_state: Arc<PlanState>,
        cfg: &AppConfig,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            backend,
            router,
            broker,
            plan_state,
            workspace_root,
            max_turns: TurnBudget::from_config(cfg).max_turns,
            policy: policy::SessionPolicy {
                approval: cfg.approval.mode,
                sandbox: cfg.sandbox.mode,
            },
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
            // 系统提示每轮重算（docs/04 §6.1 不变量③）：计划态与沙箱模式变化必须反映到模型
            let instructions = prompt::system_instruction(
                &self.workspace_root,
                self.policy.sandbox,
                self.plan_state.is_active(),
            );
            // 计划态下只向模型声明只读工具 + exit_plan_mode（ADR-003 双层防御第一层）
            let declarations = declarations_for(&self.router, self.plan_state.is_active());
            let mut stream = match self.backend.stream(&history, &declarations, &instructions).await {
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
                // 未知工具 = 协议违规：不经门禁直接拒绝回填（模型自我纠正），不打扰人工
                if self.router.get(name).is_none() {
                    emit(&sink, EventMsg::ToolCallBegin {
                        call_id: call_id.clone(),
                        tool: name.clone(),
                        args: serde_json::from_str(arguments)
                            .unwrap_or(serde_json::Value::String(arguments.clone())),
                    });
                    emit(&sink, EventMsg::ToolCallEnd {
                        call_id: call_id.clone(),
                        exit_code: None,
                        error: Some(format!("未知工具: {name}")),
                        result: Some(ToolCallResult::Error),
                    });
                    history.push(HistoryItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: format!("未知工具: {name}"),
                    });
                    continue;
                }

                emit(&sink, EventMsg::ToolCallBegin {
                    call_id: call_id.clone(),
                    tool: name.clone(),
                    args: serde_json::from_str(arguments)
                        .unwrap_or(serde_json::Value::String(arguments.clone())),
                });

                // 计划态执行守卫（双层防御；声明已收窄，此处兜底）
                if self.plan_state.is_active() && !policy::plan_mode_allows(name) {
                    emit(&sink, EventMsg::ToolCallEnd {
                        call_id: call_id.clone(),
                        exit_code: None,
                        error: Some("计划模式仅只读工具可用".into()),
                        result: Some(ToolCallResult::Error),
                    });
                    history.push(HistoryItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: "计划模式仅只读工具可用；请先调用 exit_plan_mode 提交计划".into(),
                    });
                    continue;
                }

                // 审批门禁（ADR-003）：裁决 → 询问回执 → 放行/拒绝
                let args_val =
                    serde_json::from_str(arguments).unwrap_or(serde_json::Value::String(arguments.clone()));
                match self
                    .broker
                    .gate(name, &args_val, call_id, self.policy, &sink, &cancel)
                    .await
                {
                    crate::permission::GateOutcome::Allowed => {}
                    crate::permission::GateOutcome::Denied => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("用户拒绝".into()),
                            result: Some(ToolCallResult::Denied),
                        });
                        history.push(HistoryItem::FunctionCallOutput {
                            call_id: call_id.clone(),
                            output: "用户拒绝了该工具调用".into(),
                        });
                        continue;
                    }
                    crate::permission::GateOutcome::Cancelled => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("已取消".into()),
                            result: Some(ToolCallResult::Cancelled),
                        });
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                }

                let tool_ctx = ToolContext::new(
                    self.workspace_root.clone(),
                    cancel.clone(),
                    call_id.clone(),
                    sink.clone(),
                    self.policy.sandbox,
                );
                let result = self.router.execute(name, args_val, &tool_ctx).await;
                let output_text = match &result {
                    Ok(out) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: Some(i32::from(!out.success)),
                            error: None,
                            // 成功/失败是两回事：exit_code 只表达是否非零退出，result 才区分语义
                            result: Some(if out.success {
                                ToolCallResult::Ok
                            } else {
                                ToolCallResult::Error
                            }),
                        });
                        out.output.clone()
                    }
                    Err(crate::tool_router::ToolError::Cancelled) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("已取消".into()),
                            result: Some(ToolCallResult::Cancelled),
                        });
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                    Err(e) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some(e.to_string()),
                            result: Some(ToolCallResult::Error),
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

/// 本轮向模型声明的工具集合（ADR-003 计划态只读的第一层防御）：
/// 计划态下仅保留只读工具与 `exit_plan_mode`，写入/执行工具对模型不可见。
fn declarations_for(router: &ToolRouter, plan_active: bool) -> Vec<crate::model_client::ToolDeclaration> {
    let all = router.declarations();
    if !plan_active {
        return all;
    }
    all.into_iter()
        .filter(|d| policy::plan_mode_allows(&d.name))
        .collect()
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
            _instructions: &str,
        ) -> Result<ModelStream, String> {
            let mut q = self.0.lock().unwrap();
            match q.pop_front() {
                Some(script) if !script.is_empty() => Ok(Box::pin(futures::stream::iter(script))),
                // 空脚本 = 永不产出的事件流（取消/超时类确定性测试用）
                _ => Ok(Box::pin(futures::stream::pending())),
            }
        }
    }

    struct EchoTool {
        spec: ToolSpec,
    }

    impl Default for EchoTool {
        fn default() -> Self {
            // 命名 read_file → 风险 read → 门禁自动放行（回显工具用于循环测试）
            Self {
                spec: ToolSpec::new("read_file", "回显", serde_json::json!({"type":"object"})),
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
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        let (_sink, store) = CapturingSink::new();
        let broker = Arc::new(ApprovalBroker::new());
        let agent = AgentLoop {
            backend: Arc::new(ScriptedBackend::new(turns)),
            router: Arc::new(router),
            broker: broker.clone(),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 3,
            policy: policy::SessionPolicy {
                approval: crate::config::ApprovalMode::Ask,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            },
        };
        (agent, store, CancellationToken::new(), broker)
    }

    fn usage_ev() -> ModelStreamEvent {
        ModelStreamEvent::Completed { usage: Usage { input_tokens: 10, cached_input_tokens: None, output_tokens: 5 } }
    }

    fn fn_call_ev() -> ModelStreamEvent {
        ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
            name: "read_file".into(),
            arguments: r#"{"x":1}"#.into(),
            call_id: "c1".into(),
        })
    }

    // ---- 用例 ----

    #[tokio::test]
    async fn single_turn_completion_emits_expected_events() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![
                ModelStreamEvent::OutputTextDelta { delta: "你好".into() },
                usage_ev(),
            ],
        ]);
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
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![fn_call_ev(), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }, usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallBegin { call_id, tool, .. } if call_id == "c1" && tool == "read_file"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(0), error: None, result: Some(ToolCallResult::Ok) } if call_id == "c1"
        )));
    }

    /// 结果语义探针：同一个假工具按需返回失败/取消，用于钉住 emit 分支的 `result` 取值。
    #[derive(Debug, Clone, Copy)]
    enum ToolBehavior {
        Fail,
        Cancel,
    }

    fn make_loop_with_read_tool(
        behavior: ToolBehavior,
        turns: Vec<Vec<ModelStreamEvent>>,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        struct BehaviorTool {
            spec: ToolSpec,
            behavior: ToolBehavior,
        }
        #[async_trait::async_trait]
        impl ToolExecutor for BehaviorTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                match self.behavior {
                    ToolBehavior::Fail => Ok(ToolOutput::fail("命令以非零状态退出")),
                    ToolBehavior::Cancel => Err(ToolError::Cancelled),
                }
            }
        }
        // 命名 read_file → 风险 read → 门禁自动放行，避免测试里再走审批
        let (mut agent, store, cancel, broker) = make_loop(turns);
        let mut r = ToolRouter::new();
        r.register(Arc::new(BehaviorTool {
            spec: ToolSpec::new("read_file", "假读工具", serde_json::json!({"type":"object"})),
            behavior,
        }));
        agent.router = Arc::new(r);
        (agent, store, cancel, broker)
    }

    #[tokio::test]
    async fn failed_tool_emits_result_error() {
        let (agent, store, cancel, _broker) = make_loop_with_read_tool(
            ToolBehavior::Fail,
            vec![
                vec![fn_call_ev(), usage_ev()],
                vec![ModelStreamEvent::OutputTextDelta { delta: "继续".into() }, usage_ev()],
            ],
        );
        agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        let events = store.lock().unwrap();
        // 非零退出：exit_code 是证据字段，result 才是语义判据
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(1), result: Some(ToolCallResult::Error), .. }
                if call_id == "c1"
        )));
    }

    #[tokio::test]
    async fn cancelled_tool_emits_result_cancelled() {
        let (agent, store, cancel, _broker) = make_loop_with_read_tool(
            ToolBehavior::Cancel,
            vec![vec![fn_call_ev(), usage_ev()]],
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Cancelled);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: None, result: Some(ToolCallResult::Cancelled), .. }
                if call_id == "c1"
        )));
    }

    #[tokio::test]
    async fn budget_exhausted_when_tools_keep_coming() {
        let (agent, store, cancel, _broker) = make_loop(vec![
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
        // 空脚本 = 挂起的事件流：预取消后 select! 只有取消分支就绪（确定性）
        let (agent, store, cancel, _broker) = make_loop(vec![vec![]]);
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
        let (agent, store, cancel, _broker) = make_loop(vec![vec![ModelStreamEvent::Failed { message: "boom".into() }]]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Failed);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::Error { message } if message == "boom"
        )));
    }

    #[tokio::test]
    async fn unknown_tool_error_backfills_and_continues() {
        let (agent, store, cancel, _broker) = make_loop(vec![
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

    #[tokio::test]
    async fn destructive_tool_requires_approval_and_deny_blocks_execution() {
        // wipe_disk 是注册过的假想破坏性工具（未知工具名走路由拒绝路径，见另一用例）
        let (agent, store, cancel, broker) = make_loop_with_destructive(vec![
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "wipe_disk".into(),
                arguments: "{}".into(),
                call_id: "c7".into(),
            }), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "已纠正".into() }, usage_ev()],
        ]);
        let sink = Arc::new(CapturingSink(store.clone()));
        let run = tokio::spawn(async move { agent.run_task("任务", sink, cancel).await });
        // 等待审批请求出现后回执「拒绝」
        for _ in 0..300 {
            if broker.has_pending("appr-c7") {
                assert!(broker.resolve("appr-c7", crate::events::ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let summary = run.await.unwrap();
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ExecApprovalRequest { request_id, risk: crate::events::RiskLevel::Destructive, .. }
                if request_id == "appr-c7"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, error: Some(msg), result: Some(ToolCallResult::Denied), .. }
                if call_id == "c7" && msg.contains("用户拒绝")
        )));
    }

    fn make_loop_with_destructive(
        turns: Vec<Vec<ModelStreamEvent>>,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        struct WipeTool {
            spec: ToolSpec,
        }
        impl Default for WipeTool {
            fn default() -> Self {
                Self {
                    spec: ToolSpec::new("wipe_disk", "破坏性", serde_json::json!({"type":"object"})),
                }
            }
        }
        #[async_trait::async_trait]
        impl ToolExecutor for WipeTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::ok("wiped"))
            }
        }
        let (mut agent, store, cancel, broker) = make_loop(turns);
        let mut router_raw = ToolRouter::new();
        router_raw.register(Arc::new(WipeTool::default()));
        // 直接补注册（保持既有 router 引用结构）
        let router = Arc::new({
            let mut r = ToolRouter::new();
            r.register(Arc::new(EchoTool::default()));
            r.register(Arc::new(WipeTool::default()));
            r
        });
        agent.router = router;
        drop(router_raw);
        (agent, store, cancel, broker)
    }

    // ---- 系统提示下发（docs/04 §6.1）----

    /// 记录 (系统提示, 历史长度) 的后端：验证每轮下发、且不写入 history。
    type Recorded = Arc<Mutex<Vec<(String, usize)>>>;

    struct PromptRecordingBackend(Recorded);

    #[async_trait::async_trait]
    impl ModelBackend for PromptRecordingBackend {
        async fn stream(
            &self,
            history: &[HistoryItem],
            _tools: &[ToolDeclaration],
            instructions: &str,
        ) -> Result<ModelStream, String> {
            self.0.lock().unwrap().push((instructions.to_string(), history.len()));
            // 无工具调用 → 单轮即完成
            Ok(Box::pin(futures::stream::iter(vec![ModelStreamEvent::OutputTextDelta {
                delta: "ok".into(),
            }])))
        }
    }

    fn agent_with_recording(
        workspace: PathBuf,
        plan_active: bool,
    ) -> (AgentLoop, Recorded, Arc<Mutex<Vec<EventMsg>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(PromptRecordingBackend(recorded.clone()));
        let (_sink, store) = CapturingSink::new();
        let plan_state = Arc::new(PlanState::new());
        if plan_active {
            plan_state.activate();
        }
        let agent = AgentLoop {
            backend,
            router: Arc::new(ToolRouter::new()),
            broker: Arc::new(ApprovalBroker::new()),
            plan_state,
            workspace_root: workspace,
            max_turns: 2,
            policy: policy::SessionPolicy {
                approval: crate::config::ApprovalMode::Ask,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            },
        };
        (agent, recorded, store)
    }

    #[tokio::test]
    async fn system_instruction_sent_to_model_and_kept_out_of_history() {
        let ws = std::env::temp_dir();
        let (agent, recorded, store) = agent_with_recording(ws.clone(), false);
        agent
            .run_task("任务", Arc::new(CapturingSink(store)), CancellationToken::new())
            .await;

        let calls = recorded.lock().unwrap();
        assert_eq!(calls.len(), 1, "单轮任务应只采样一次");
        let (prompt, history_len) = &calls[0];
        assert!(prompt.contains(&ws.display().to_string()), "提示须含工作区路径");
        assert!(prompt.contains("工作区写入"), "提示须反映沙箱模式");
        assert!(!prompt.contains("当前状态：计划模式"));
        // 提示不写入 history：历史里只有用户这一条
        assert_eq!(*history_len, 1, "系统提示不得混入 history");
    }

    #[tokio::test]
    async fn system_instruction_reflects_plan_mode() {
        let (agent, recorded, store) = agent_with_recording(std::env::temp_dir(), true);
        agent
            .run_task("先做计划", Arc::new(CapturingSink(store)), CancellationToken::new())
            .await;
        let calls = recorded.lock().unwrap();
        let (prompt, _) = &calls[0];
        assert!(prompt.contains("当前状态：计划模式"), "计划态必须写入提示");
        assert!(prompt.contains("exit_plan_mode"), "计划态须指引退出路径");
    }

    // ---- Plan Mode（ADR-003 只读计划态）----

    #[test]
    fn plan_mode_declarations_hide_write_and_execute_tools() {
        let mut router = ToolRouter::new();
        crate::tools::register_builtin(&mut router, Arc::new(PlanState::new()));

        let names = |plan: bool| -> Vec<String> {
            declarations_for(&router, plan).into_iter().map(|d| d.name).collect()
        };
        let planned = names(true);
        assert!(planned.contains(&"read_file".to_string()));
        assert!(planned.contains(&"list_dir".to_string()));
        assert!(planned.contains(&"search".to_string()));
        assert!(planned.contains(&"exit_plan_mode".to_string()));
        assert!(!planned.contains(&"apply_patch".to_string()), "计划态不得向模型声明写入工具");
        assert!(!planned.contains(&"unified_exec".to_string()), "计划态不得向模型声明命令执行工具");
        assert!(!planned.contains(&"enter_plan_mode".to_string()), "已在计划态，无需重复进入");

        let full = names(false);
        assert!(full.contains(&"apply_patch".to_string()));
        assert!(full.contains(&"unified_exec".to_string()));
    }

    #[tokio::test]
    async fn plan_mode_guard_blocks_write_tool_even_if_called() {
        struct PatchEcho {
            spec: ToolSpec,
        }
        #[async_trait::async_trait]
        impl ToolExecutor for PatchEcho {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::ok("patched"))
            }
        }
        let (mut agent, store, cancel, _broker) = make_loop(vec![
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "apply_patch".into(),
                arguments: r#"{"patch":"*** Begin Patch"}"#.into(),
                call_id: "c5".into(),
            }), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "改为只读调研".into() }, usage_ev()],
        ]);
        // 只注册写入工具（read_file 回显 + apply_patch），并置为计划态
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        router.register(Arc::new(PatchEcho {
            spec: ToolSpec::new("apply_patch", "写入", serde_json::json!({"type":"object"})),
        }));
        agent.router = Arc::new(router);
        let plan_state = Arc::new(PlanState::new());
        plan_state.activate();
        agent.plan_state = plan_state;

        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                EventMsg::ToolCallEnd { call_id, error: Some(msg), .. }
                    if call_id == "c5" && msg.contains("计划模式")
            )),
            "计划态下写入工具必须被执行守卫拒绝"
        );
        // 未被真正执行：没有「成功」的 ToolCallEnd
        assert!(!events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(0), error: None, .. } if call_id == "c5"
        )));
        // 也不应触发审批（守卫先于审批门禁）
        assert!(!events.iter().any(|e| matches!(e, EventMsg::ExecApprovalRequest { .. })));
    }
}
