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
use shaniu_lib::tool_router::{
    PlanState, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolRouter, ToolSpec,
};
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
    ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
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

/// 一轮内发多次同类工具调用；每次的 `arguments` 可相同（验证批量）或不同（验证不批量）。
struct MultiCallBackend {
    tool: String,
    calls: Vec<(String, String)>,
}

#[async_trait]
impl ModelBackend for MultiCallBackend {
    async fn stream(
        &self,
        history: &[HistoryItem],
        _tools: &[ToolDeclaration],
        _instructions: &str,
    ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
        let tool_already_ran = history
            .iter()
            .any(|h| matches!(h, HistoryItem::FunctionCallOutput { .. }));
        let events = if tool_already_ran {
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }]
        } else {
            self.calls
                .iter()
                .map(|(call_id, arguments)| {
                    ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                        name: self.tool.clone(),
                        arguments: arguments.clone(),
                        call_id: call_id.clone(),
                    })
                })
                .collect()
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 每个 turn 发一次同类调用：用来证明批量窗口**不跨 turn**。
struct SequentialBackend {
    tool: String,
    emitted: Arc<AtomicUsize>,
}

#[async_trait]
impl ModelBackend for SequentialBackend {
    async fn stream(
        &self,
        history: &[HistoryItem],
        _tools: &[ToolDeclaration],
        _instructions: &str,
    ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
        let outputs = history
            .iter()
            .filter(|h| matches!(h, HistoryItem::FunctionCallOutput { .. }))
            .count();
        let events = if outputs >= 2 {
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }]
        } else {
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: self.tool.clone(),
                arguments: r#"{"command":"git status"}"#.into(),
                call_id: format!("c{}", self.emitted.fetch_add(1, Ordering::SeqCst) + 1),
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
    harness_with(Arc::new(ScriptedBackend { tool: tool_name.to_string() }), tool_name, 2)
}

fn harness_with(backend: Arc<dyn ModelBackend>, tool_name: &str, max_turns: u32) -> Harness {
    let hits = Arc::new(AtomicUsize::new(0));
    let mut router = ToolRouter::new();
    router.register(Arc::new(CountingTool {
        spec: ToolSpec::new(tool_name, "契约测试用工具", serde_json::json!({"type":"object"})),
        hits: hits.clone(),
    }));
    let broker = Arc::new(ApprovalBroker::new());
    let events = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentLoop {
        backend,
        router: Arc::new(router),
        broker: broker.clone(),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns,
        token_budget: 0,
        tree_budget: Default::default(),
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
            approval: ApprovalMode::OnRequest,
            sandbox: SandboxMode::WorkspaceWrite,
        })),
        rollout: None,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
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

// ============================== on-failure 提权（SH-120） ==============================

/// 沙箱执行一律拒绝；只有在 danger 沙箱下才成功。用来验证提权重试**真的换了沙箱**，
/// 而不是"再跑一次同样的沙箱然后假装成功"。
struct SandboxDeniedTool {
    spec: ToolSpec,
    hits: Arc<AtomicUsize>,
    danger_hits: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolExecutor for SandboxDeniedTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn execute(&self, _args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        if ctx.sandbox == SandboxMode::DangerFullAccess {
            self.danger_hits.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::ok("ok"))
        } else {
            Err(ToolError::SandboxDenied("受限令牌拒绝执行".into()))
        }
    }
}

struct OnFailureHarness {
    agent: AgentLoop,
    hits: Arc<AtomicUsize>,
    danger_hits: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<EventMsg>>>,
    broker: Arc<ApprovalBroker>,
    cancel: CancellationToken,
}

fn on_failure_harness(approval: ApprovalMode) -> OnFailureHarness {
    let hits = Arc::new(AtomicUsize::new(0));
    let danger_hits = Arc::new(AtomicUsize::new(0));
    let mut router = ToolRouter::new();
    router.register(Arc::new(SandboxDeniedTool {
        spec: ToolSpec::new("unified_exec", "沙箱拒绝样本", serde_json::json!({"type":"object"})),
        hits: hits.clone(),
        danger_hits: danger_hits.clone(),
    }));
    let broker = Arc::new(ApprovalBroker::new());
    let events = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentLoop {
        backend: Arc::new(ScriptedBackend { tool: "unified_exec".into() }),
        router: Arc::new(router),
        broker: broker.clone(),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 2,
        token_budget: 0,
        tree_budget: Default::default(),
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
            approval,
            sandbox: SandboxMode::WorkspaceWrite,
        })),
        rollout: None,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
    };
    OnFailureHarness { agent, hits, danger_hits, events, broker, cancel: CancellationToken::new() }
}

impl OnFailureHarness {
    async fn run(self) -> (TurnSummary, usize, usize, Vec<EventMsg>) {
        let sink: Arc<dyn EventSink> = Arc::new(CapturingSink(self.events.clone()));
        let summary = tokio::time::timeout(
            Duration::from_secs(10),
            self.agent.run_task("任务", sink, self.cancel.clone()),
        )
        .await
        .expect("任务超时：on-failure 未按契约收敛");
        (
            summary,
            self.hits.load(Ordering::SeqCst),
            self.danger_hits.load(Ordering::SeqCst),
            self.events.lock().unwrap().clone(),
        )
    }
}

/// 等第 `skip+1` 个审批请求出现，返回 (request_id, reason)。
async fn wait_for_request(
    events: &Arc<Mutex<Vec<EventMsg>>>,
    skip: usize,
) -> (String, Option<String>) {
    for _ in 0..500 {
        let found = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                EventMsg::ExecApprovalRequest { request_id, reason, .. } => {
                    Some((request_id.clone(), reason.clone()))
                }
                _ => None,
            })
            .nth(skip);
        if let Some(found) = found {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("未观察到第 {} 个审批请求", skip + 1);
}

/// `on-failure`：沙箱拒绝**之后**才问，批准后以 danger 沙箱重试一次。
#[tokio::test]
async fn on_failure_asks_after_sandbox_denial_and_retries_with_danger() {
    let h = on_failure_harness(ApprovalMode::OnFailure);
    let events = h.events.clone();
    let broker = h.broker.clone();
    let task = tokio::spawn(async move { h.run().await });

    let (request_id, reason) = wait_for_request(&events, 0).await;
    assert_eq!(request_id, "appr-c1-escalate", "提权请求必须挂在原调用上");
    assert!(
        reason.unwrap_or_default().contains("沙箱拒绝"),
        "审批卡必须说明是沙箱拒绝后的提权"
    );
    assert!(broker.resolve(&request_id, ApprovalDecision::Allow));

    let (summary, hits, danger_hits, events) = task.await.unwrap();
    assert_eq!(summary.reason, shaniu_lib::events::TurnEndReason::Completed);
    assert_eq!(hits, 2, "首次沙箱执行 + 提权后重试");
    assert_eq!(danger_hits, 1, "只有用户批准的那次才走 danger 沙箱");
    let requests = events
        .iter()
        .filter(|e| matches!(e, EventMsg::ExecApprovalRequest { .. }))
        .count();
    assert_eq!(requests, 1, "一次失败只问一次，不重复打断");
}

/// `on-failure`：用户拒绝提权 → 不重试、不走 danger，原始沙箱失败回填模型。
#[tokio::test]
async fn on_failure_denied_keeps_original_failure() {
    let h = on_failure_harness(ApprovalMode::OnFailure);
    let events = h.events.clone();
    let broker = h.broker.clone();
    let task = tokio::spawn(async move { h.run().await });

    let (request_id, _reason) = wait_for_request(&events, 0).await;
    assert!(broker.resolve(&request_id, ApprovalDecision::Deny));

    let (summary, hits, danger_hits, events) = task.await.unwrap();
    assert_eq!(summary.reason, shaniu_lib::events::TurnEndReason::Completed);
    assert_eq!(hits, 1, "拒绝提权不得重试");
    assert_eq!(danger_hits, 0, "拒绝提权不得走 danger 沙箱");
    assert!(
        events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { error: Some(msg), .. } if msg.contains("沙箱拒绝")
        )),
        "沙箱失败原因必须回填（模型据此换策略而非重试）"
    );
}

/// `never`：沙箱拒绝也不弹窗、不提权——失败直接回填。
#[tokio::test]
async fn never_mode_does_not_prompt_or_escalate_on_sandbox_denial() {
    let h = on_failure_harness(ApprovalMode::Never);
    let task = tokio::spawn(async move { h.run().await });

    let (_summary, hits, danger_hits, events) = task.await.unwrap();
    assert_eq!(hits, 1, "never 档不重试");
    assert_eq!(danger_hits, 0);
    assert!(
        !events.iter().any(|e| matches!(e, EventMsg::ExecApprovalRequest { .. })),
        "never 档不得发任何审批卡"
    );
}

// ============================== 批量确认（D3：参数完全相同才批量） ==============================

/// D3：同一 turn 内**参数完全相同**的调用只产生 1 个审批事件，三次都真的执行。
/// 卡片必须写明范围（`batch_scope`），用户有权知道这句话覆盖了多远。
#[tokio::test]
async fn same_turn_identical_calls_batch_into_one_prompt() {
    let command = r#"{"command":"git status"}"#;
    let backend = Arc::new(MultiCallBackend {
        tool: "unified_exec".into(),
        calls: vec![
            ("c1".into(), command.into()),
            ("c2".into(), command.into()),
            ("c3".into(), command.into()),
        ],
    });
    let h = harness_with(backend, "unified_exec", 2);
    let events = h.events.clone();
    let broker = h.broker.clone();
    let task = tokio::spawn(async move { h.run().await });

    let (request_id, _reason) = wait_for_request(&events, 0).await;
    let scope = events
        .lock()
        .unwrap()
        .iter()
        .find_map(|e| match e {
            EventMsg::ExecApprovalRequest { batch_scope, .. } => Some(batch_scope.clone()),
            _ => None,
        });
    assert_eq!(
        scope,
        Some(Some("same-args".to_string())),
        "命令类批准后确实会开窗口，卡片必须写明范围"
    );
    assert!(broker.resolve(&request_id, ApprovalDecision::Allow));

    let (summary, hits, events) = task.await.unwrap();
    assert_eq!(summary.reason, shaniu_lib::events::TurnEndReason::Completed);
    assert_eq!(hits, 3, "批量只减少弹窗，不减少执行");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, EventMsg::ExecApprovalRequest { .. }))
            .count(),
        1,
        "同一 turn 内参数完全相同的调用只问一次"
    );
}

/// D3 反向契约：命令变了风险就变——批准 `git status` 不得连带 `git push --force`。
/// 第三次回到 `git status` 也重新询问：窗口只有一个槽位，被中间那次覆盖。
#[tokio::test]
async fn same_turn_different_commands_prompt_each_time() {
    let backend = Arc::new(MultiCallBackend {
        tool: "unified_exec".into(),
        calls: vec![
            ("c1".into(), r#"{"command":"git status"}"#.into()),
            ("c2".into(), r#"{"command":"git push --force"}"#.into()),
            ("c3".into(), r#"{"command":"git status"}"#.into()),
        ],
    });
    let h = harness_with(backend, "unified_exec", 2);
    let events = h.events.clone();
    let broker = h.broker.clone();
    let task = tokio::spawn(async move { h.run().await });

    for index in 0..3 {
        let (request_id, _reason) = wait_for_request(&events, index).await;
        assert!(broker.resolve(&request_id, ApprovalDecision::Allow));
    }

    let (summary, hits, events) = task.await.unwrap();
    assert_eq!(summary.reason, shaniu_lib::events::TurnEndReason::Completed);
    assert_eq!(hits, 3, "每次批准后都要真的执行");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, EventMsg::ExecApprovalRequest { .. }))
            .count(),
        3,
        "参数一变就必须重新确认：批准 A 不等于批准 B"
    );
}

/// 批量窗口不跨 turn：第 2 个 turn 的同类调用必须重新询问。
#[tokio::test]
async fn batch_window_does_not_cross_turns() {
    let backend = Arc::new(SequentialBackend {
        tool: "unified_exec".into(),
        emitted: Arc::new(AtomicUsize::new(0)),
    });
    let h = harness_with(backend, "unified_exec", 4);
    let events = h.events.clone();
    let broker = h.broker.clone();
    let task = tokio::spawn(async move { h.run().await });

    let (first, _) = wait_for_request(&events, 0).await;
    assert!(broker.resolve(&first, ApprovalDecision::Allow));
    let (second, _) = wait_for_request(&events, 1).await;
    assert!(broker.resolve(&second, ApprovalDecision::Allow));

    let (summary, hits, events) = task.await.unwrap();
    assert_eq!(summary.reason, shaniu_lib::events::TurnEndReason::Completed);
    assert_eq!(hits, 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, EventMsg::ExecApprovalRequest { .. }))
            .count(),
        2,
        "跨 turn 不得复用上一轮的批准"
    );
}

/// destructive 永不批量：同一 turn 三次调用必须三次都问。
#[tokio::test]
async fn destructive_calls_never_batch() {
    let backend = Arc::new(MultiCallBackend {
        tool: "mcp__danger__op".into(),
        calls: vec![
            ("c1".into(), "{}".into()),
            ("c2".into(), "{}".into()),
            ("c3".into(), "{}".into()),
        ],
    });
    let h = harness_with(backend, "mcp__danger__op", 2);
    let events = h.events.clone();
    let broker = h.broker.clone();
    let task = tokio::spawn(async move { h.run().await });

    for index in 0..3 {
        let (request_id, _reason) = wait_for_request(&events, index).await;
        assert!(broker.resolve(&request_id, ApprovalDecision::Allow));
    }

    let (summary, hits, events) = task.await.unwrap();
    assert_eq!(summary.reason, shaniu_lib::events::TurnEndReason::Completed);
    assert_eq!(hits, 3);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, EventMsg::ExecApprovalRequest { .. }))
            .count(),
        3,
        "破坏性操作每次必问"
    );
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
                        approval: ApprovalMode::OnRequest,
                        sandbox: SandboxMode::WorkspaceWrite,
                    },
                    shaniu_lib::permission::GateSubject::root("root"),
                    shaniu_lib::permission::GateEnv { sink: &sink, cancel: &cancel },
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
