//! 健身函数 FF-6（docs/01 §5）：审批不可绕过契约测试。
//!
//! 不测"看起来弹了审批框"，而测**硬契约**：
//! 1) 没有后端签发的 request_id 回执时，execute/destructive 工具执行次数必须为 0；
//! 2) 伪造的 request_id 不被受理（审批 UI/前端不可伪造裁决）；
//! 3) 用户拒绝后工具不执行，且拒绝原因回填模型（可自我纠正而非重试）；
//! 4) 只有显式 Allow 才放行，且 destructive 级即使 AllowAlways 也不进会话记忆。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use shaniu_lib::agent_loop::{AgentLoop, ModelBackend, ModelStream, TurnSummary};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{ApprovalDecision, EventMsg, EventSink, RiskLevel};
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent, ResponseItem, ToolDeclaration};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::tool_router::{ToolContext, ToolError, ToolExecutor, ToolOutput, ToolRouter, ToolSpec};
use shaniu_lib::tools::plan::PlanState;
use tokio_util::sync::CancellationToken;

// ============================== 测试基建 ==============================

#[derive(Clone, Default)]
struct CapturingSink(Arc<Mutex<Vec<EventMsg>>>);

impl EventSink for CapturingSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        self.0.lock().unwrap().push(msg.clone());
        Ok(())
    }
}

/// 录音工具：命中次数即"是否真的执行了"。
struct CountingTool {
    spec: ToolSpec,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolExecutor for CountingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok("executed"))
    }
}

/// 脚本后端：第 1 轮发一次工具调用，第 2 轮收尾成文。
struct ScriptedBackend {
    tool: String,
}

#[async_trait]
impl ModelBackend for ScriptedBackend {
    async fn stream(
        &self,
        history: &[HistoryItem],
        _tools: &[ToolDeclaration],
        _instructions: &str,
    ) -> Result<ModelStream, String> {
        // 已有工具回填 → 视为完成（避免无限循环）
        let tool_already_ran = history
            .iter()
            .any(|h| matches!(h, HistoryItem::FunctionCallOutput { .. }));
        let events = if tool_already_ran {
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }]
        } else {
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: self.tool.clone(),
                arguments: "{}".into(),
                call_id: "c1".into(),
            })]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct Harness {
    agent: AgentLoop,
    hits: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<EventMsg>>>,
    broker: Arc<ApprovalBroker>,
    cancel: CancellationToken,
}

/// `tool_name` 决定风险等级：`unified_exec` = execute（询问）、未知名 = destructive。
fn harness(tool_name: &str) -> Harness {
    let hits = Arc::new(AtomicUsize::new(0));
    let mut router = ToolRouter::new();
    router.register(Arc::new(CountingTool {
        spec: ToolSpec::new(tool_name, "契约测试用工具", serde_json::json!({"type":"object"})),
        hits: hits.clone(),
    }));
    let broker = Arc::new(ApprovalBroker::new());
    let events = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentLoop {
        backend: Arc::new(ScriptedBackend { tool: tool_name.to_string() }),
        router: Arc::new(router),
        broker: broker.clone(),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 2,
        policy: policy::SessionPolicy {
            approval: ApprovalMode::Ask,
            sandbox: SandboxMode::WorkspaceWrite,
        },
    };
    Harness { agent, hits, events, broker, cancel: CancellationToken::new() }
}

impl Harness {
    async fn run(self) -> (TurnSummary, usize, Vec<EventMsg>) {
        let sink: Arc<dyn EventSink> = Arc::new(CapturingSink(self.events.clone()));
        let summary = tokio::time::timeout(Duration::from_secs(10), self.agent.run_task("任务", sink, self.cancel.clone()))
            .await
            .expect("任务超时：门禁未按契约收敛");
        let hits = self.hits.load(Ordering::SeqCst);
        let events = self.events.lock().unwrap().clone();
        (summary, hits, events)
    }
}

// ============================== 契约 ==============================

#[tokio::test]
async fn no_receipt_means_no_execution() {
    // 回执始终不到（前端崩溃/用户走神）→ 取消才解挂；工具一次都不能执行
    let h = harness("unified_exec");
    let cancel = h.cancel.clone();
    let task = tokio::spawn(async move { h.run().await });

    // 先让请求出现，再取消——证明挂起期间没有偷偷执行
    tokio::time::sleep(Duration::from_millis(120)).await;
    cancel.cancel();

    let (summary, hits, _events) = task.await.unwrap();
    assert_eq!(hits, 0, "未获审批回执前，工具执行次数必须为 0");
    assert_eq!(
        summary.reason,
        shaniu_lib::events::TurnEndReason::Cancelled,
        "取消应正常收敛"
    );
}

#[tokio::test]
async fn forged_request_id_is_rejected() {
    let h = harness("unified_exec");
    let broker = h.broker.clone();
    let cancel = h.cancel.clone();
    let events = h.events.clone();
    let task = tokio::spawn(async move { h.run().await });

    // 等真实请求出现，取到真 ID 后伪造一个邻居 ID 试图放行
    let mut real_id = None;
    for _ in 0..500 {
        real_id = events.lock().unwrap().iter().find_map(|e| match e {
            EventMsg::ExecApprovalRequest { request_id, .. } => Some(request_id.clone()),
            _ => None,
        });
        if real_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let real_id = real_id.expect("未观察到审批请求");
    assert!(
        !broker.resolve("appr-forged", ApprovalDecision::Allow),
        "伪造的 request_id 不得命中 pending"
    );
    assert!(
        !broker.resolve(&format!("{real_id}-x"), ApprovalDecision::Allow),
        "篡改的 request_id 不得命中 pending"
    );

    cancel.cancel();
    let (_summary, hits, _events) = task.await.unwrap();
    assert_eq!(hits, 0, "伪造回执不得放行工具执行");
}

#[tokio::test]
async fn deny_blocks_execution_and_backfills_reason() {
    let h = harness("unified_exec");
    let broker = h.broker.clone();
    let events = h.events.clone();
    let task = tokio::spawn(async move { h.run().await });

    let mut real_id = None;
    for _ in 0..500 {
        real_id = events.lock().unwrap().iter().find_map(|e| match e {
            EventMsg::ExecApprovalRequest { request_id, .. } => Some(request_id.clone()),
            _ => None,
        });
        if real_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        broker.resolve(&real_id.expect("未观察到审批请求"), ApprovalDecision::Deny),
        "真实 ID 必须命中 pending"
    );

    let (summary, hits, events) = task.await.unwrap();
    assert_eq!(hits, 0, "用户拒绝后不得执行");
    assert_eq!(summary.reason, shaniu_lib::events::TurnEndReason::Completed, "拒绝应回填模型并继续");
    assert!(
        events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { error: Some(msg), .. } if msg.contains("用户拒绝")
        )),
        "拒绝原因必须回填（模型据此换策略而非重试）"
    );
}

#[tokio::test]
async fn only_explicit_allow_executes_once() {
    let h = harness("unified_exec");
    let broker = h.broker.clone();
    let events = h.events.clone();
    let task = tokio::spawn(async move { h.run().await });

    let mut real_id = None;
    for _ in 0..500 {
        real_id = events.lock().unwrap().iter().find_map(|e| match e {
            EventMsg::ExecApprovalRequest { request_id, .. } => Some(request_id.clone()),
            _ => None,
        });
        if real_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        broker.resolve(&real_id.expect("未观察到审批请求"), ApprovalDecision::Allow),
        "真实 ID 必须命中 pending"
    );

    let (_summary, hits, _events) = task.await.unwrap();
    assert_eq!(hits, 1, "放行后工具应恰好执行一次");
}

#[tokio::test]
async fn destructive_allow_always_is_not_remembered() {
    // 未知工具名 → destructive 级；即使同会话内已 AllowAlways 过，也必须逐次询问
    let h = harness("wipe_everything");
    let broker = h.broker.clone();
    let events = h.events.clone();
    let task = tokio::spawn(async move { h.run().await });

    let mut real_id = None;
    for _ in 0..500 {
        real_id = events.lock().unwrap().iter().find_map(|e| match e {
            EventMsg::ExecApprovalRequest { request_id, .. } => Some(request_id.clone()),
            _ => None,
        });
        if real_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let req = events.lock().unwrap().iter().find_map(|e| match e {
        EventMsg::ExecApprovalRequest { risk, .. } => Some(*risk),
        _ => None,
    });
    assert_eq!(req, Some(RiskLevel::Destructive), "未知/高危工具必须标为 destructive");
    assert!(
        broker.resolve(&real_id.unwrap(), ApprovalDecision::AllowAlways),
        "真实 ID 必须命中 pending"
    );
    let (_summary, hits, _events) = task.await.unwrap();
    assert_eq!(hits, 1, "AllowAlways 对 destructive 仍放行本次调用");

    // ADR-003：destructive 永不记忆——第二次调用必须重新弹审批
    let second_events = Arc::new(Mutex::new(Vec::new()));
    let sink: Arc<dyn EventSink> = Arc::new(CapturingSink(second_events.clone()));
    let cancel = CancellationToken::new();
    let gate = tokio::spawn({
        let broker = broker.clone();
        let sink = sink.clone();
        let cancel = cancel.clone();
        async move {
            broker
                .gate(
                    "wipe_everything",
                    &serde_json::json!({}),
                    "c2",
                    policy::SessionPolicy {
                        approval: ApprovalMode::Ask,
                        sandbox: SandboxMode::WorkspaceWrite,
                    },
                    &sink,
                    &cancel,
                )
                .await
        }
    });

    let mut reprompted = false;
    for _ in 0..300 {
        reprompted = second_events.lock().unwrap().iter().any(|e| {
            matches!(e, EventMsg::ExecApprovalRequest { request_id, .. } if request_id == "appr-c2")
        });
        if reprompted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reprompted, "destructive 二次调用必须重新询问，不得被会话记忆放行");

    cancel.cancel();
    assert_eq!(
        gate.await.unwrap(),
        shaniu_lib::permission::GateOutcome::Cancelled,
        "撤单后门禁应归为取消"
    );
}
