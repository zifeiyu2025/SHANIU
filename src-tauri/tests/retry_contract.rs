//! 模型请求重试的契约测试（`scratch/spec-retry/spec.md` §10.2 C1–C12）。
//!
//! 全部用桩后端，不联网；断言的是**行为与事件序列**，不是字面量快照。
//! 三条验收级硬约束在这里各有用例兜底：
//! 1. 只在首个可见增量前重试（正文与推理增量都算）——C2 / C3 / C9b；
//! 2. 退避等待可被取消打断——C7；
//! 3. `Retry-After` 超上限时文案带出要求秒数与上限、`TurnErrorInfo` 带原始秒数——C11。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use shaniu_lib::agent_loop::{AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink, RetryPhase, TurnEndReason, TurnErrorInfo};
use shaniu_lib::model_client::{HistoryItem, ModelError, ModelStreamEvent, ResponseItem, RetryPolicy, Usage};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use tokio_util::sync::CancellationToken;

// ============================ 桩后端 ============================

/// 一次 `stream()` 调用要做的事。
enum Step {
    /// 建流失败（HTTP 错误等）。
    Fail(ModelError),
    /// 流按脚本吐出全部事件后正常结束。
    Events(Vec<ModelStreamEvent>),
    /// 流永不结束（验证空闲超时）。
    Hang,
    /// 先吐一个正文增量，再挂住（验证“吐过正文就不重试”）。
    DeltaThenHang(String),
}

struct Scripted {
    steps: Mutex<VecDeque<Step>>,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps.into()), calls: AtomicUsize::new(0) })
    }
}

#[async_trait::async_trait]
impl ModelBackend for Scripted {
    async fn stream(
        &self,
        _history: &[HistoryItem],
        _tools: &[ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.steps.lock().unwrap().pop_front() {
            Some(Step::Fail(error)) => Err(error),
            Some(Step::Events(items)) => Ok(Box::pin(futures::stream::iter(items))),
            Some(Step::Hang) => Ok(Box::pin(futures::stream::pending())),
            Some(Step::DeltaThenHang(delta)) => {
                Ok(Box::pin(futures::stream::iter(vec![ModelStreamEvent::OutputTextDelta { delta }]).chain(futures::stream::pending())))
            }
            // 请求数超出用例预期 = 重试次数算错，必须炸掉而不是静默复用脚本。
            None => panic!("模型后端脚本用尽：实际请求数超出用例预期"),
        }
    }
}

// ============================ 事件记录 ============================

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<EventMsg>>,
    /// C7 用：一看到 `scheduled` 就取消，确保取消点落在退避等待里。
    cancel_on_scheduled: Mutex<Option<CancellationToken>>,
}

impl EventSink for Recorder {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        self.events.lock().unwrap().push(msg.clone());
        if let EventMsg::ModelRetry { phase: RetryPhase::Scheduled, .. } = msg {
            if let Some(token) = self.cancel_on_scheduled.lock().unwrap().as_ref() {
                token.cancel();
            }
        }
        Ok(())
    }
}

// ============================ 装配与断言工具 ============================

/// 退避压到毫秒级：用例只验证顺序与条件，不验证真实等待时长（那是纯逻辑单测的事）。
fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
        ..RetryPolicy::new(5)
    }
}

struct Harness {
    agent: AgentLoop,
    sink: Arc<Recorder>,
    backend: Arc<Scripted>,
    cancel: CancellationToken,
    budget: shaniu_lib::agent_loop::TreeBudget,
}

fn harness(steps: Vec<Step>, retry: RetryPolicy) -> Harness {
    harness_with_rollout(steps, retry, None)
}

/// C13 用：带真实 `RolloutWriter` 的装配，让断言能落在**落盘文件**而不是内存事件。
fn harness_with_rollout(
    steps: Vec<Step>,
    retry: RetryPolicy,
    rollout: Option<Arc<shaniu_lib::rollout::RolloutWriter>>,
) -> Harness {
    let backend = Scripted::new(steps);
    let sink = Arc::new(Recorder::default());
    let budget = shaniu_lib::agent_loop::TreeBudget::unlimited();
    let agent = AgentLoop {
        backend: backend.clone(),
        router: Arc::new(ToolRouter::new()),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 3,
        token_budget: 0,
        tree_budget: budget.clone(),
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
            approval: ApprovalMode::OnRequest,
            sandbox: SandboxMode::WorkspaceWrite,
        })),
        rollout,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
        retry,
        verify: Default::default(),
    };
    Harness { agent, sink, backend, cancel: CancellationToken::new(), budget }
}

impl Harness {
    async fn run(self) -> (TurnEndReason, Vec<EventMsg>, usize) {
        let sink: Arc<dyn EventSink> = self.sink.clone();
        let summary = self.agent.run_task("重试契约", sink, self.cancel).await;
        let events = self.sink.events.lock().unwrap().clone();
        (summary.reason, events, self.backend.calls.load(Ordering::SeqCst))
    }
}

fn count<F: Fn(&EventMsg) -> bool>(events: &[EventMsg], predicate: F) -> usize {
    events.iter().filter(|event| predicate(event)).count()
}

fn phases(events: &[EventMsg]) -> Vec<RetryPhase> {
    events
        .iter()
        .filter_map(|event| match event {
            EventMsg::ModelRetry { phase, .. } => Some(*phase),
            _ => None,
        })
        .collect()
}

fn retry_ids(events: &[EventMsg]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            EventMsg::ModelRetry { retry_id, .. } => Some(retry_id.clone()),
            _ => None,
        })
        .collect()
}

fn turn_error(events: &[EventMsg]) -> TurnErrorInfo {
    events
        .iter()
        .find_map(|event| match event {
            EventMsg::TurnCompleted { error, .. } => error.clone(),
            _ => None,
        })
        .expect("回合结束时必须带出结构化失败原因")
}

fn server_error() -> ModelError {
    ModelError::from_status(500, r#"{"error":{"code":"server_error","message":"upstream exploded"}}"#)
}

fn rate_limited() -> ModelError {
    ModelError::from_status(429, r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#)
}

fn usage() -> Usage {
    Usage { input_tokens: 7, cached_input_tokens: None, output_tokens: 3 }
}

fn answered() -> Vec<ModelStreamEvent> {
    vec![
        ModelStreamEvent::OutputTextDelta { delta: "答案".into() },
        ModelStreamEvent::Completed { usage: usage() },
    ]
}

// ============================ C1–C12 ============================

/// C1：先 429，再成功。重试可见，正文与 usage 只落一份。
#[tokio::test]
async fn c1_transient_failure_retries_and_succeeds_once() {
    let (reason, events, calls) =
        harness(vec![Step::Fail(rate_limited()), Step::Events(answered())], fast_policy()).run().await;

    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(calls, 2, "应当恰好发两次请求");
    assert_eq!(phases(&events), vec![RetryPhase::Scheduled, RetryPhase::Started], "顺序必须是先 scheduled 再 started");
    assert_eq!(retry_ids(&events).len(), 2);
    assert_eq!(retry_ids(&events)[0], retry_ids(&events)[1], "同一条重试链必须共用一个 retry_id");
    assert_eq!(
        count(&events, |event| matches!(event, EventMsg::AgentMessageDelta { .. })),
        1,
        "正文只允许出现一份"
    );
    assert_eq!(count(&events, |event| matches!(event, EventMsg::TokenUsage { .. })), 1, "usage 只记成功的那一次");
}

/// C2：正文已吐出后再失败——禁止重试，否则用户会看到两遍正文。
#[tokio::test]
async fn c2_failure_after_text_delta_is_terminal() {
    let (reason, events, calls) = harness(
        vec![Step::Events(vec![
            ModelStreamEvent::OutputTextDelta { delta: "半句".into() },
            ModelStreamEvent::Failed { message: "断流".into(), error: Some(server_error()) },
        ])],
        fast_policy(),
    )
    .run()
    .await;

    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 1, "有正文增量就不该再发第二次请求");
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { .. })), 0);
    let info = turn_error(&events);
    assert_eq!(info.code, "SERVER");
    assert_eq!(info.provider_message.as_deref(), Some("upstream exploded"), "原始原因必须原样保留");
    assert_eq!(info.attempts, Some(0));
}

/// C3：推理增量同样算“可见输出”。
#[tokio::test]
async fn c3_failure_after_reasoning_delta_is_terminal() {
    let (reason, events, calls) = harness(
        vec![Step::Events(vec![
            ModelStreamEvent::ReasoningDelta { delta: "想了想".into() },
            ModelStreamEvent::Failed { message: "断流".into(), error: Some(server_error()) },
        ])],
        fast_policy(),
    )
    .run()
    .await;

    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 1, "思考过程也是可见输出：重试会让它出现两遍");
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { .. })), 0);
    assert_eq!(turn_error(&events).attempts, Some(0));
}

/// C4：401 一次终局——重试一万次也不会变。
#[tokio::test]
async fn c4_auth_failure_is_never_retried() {
    let (reason, events, calls) = harness(
        vec![Step::Fail(ModelError::from_status(401, r#"{"error":{"code":"invalid_api_key","message":"bad key"}}"#))],
        fast_policy(),
    )
    .run()
    .await;

    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 1);
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { .. })), 0);
    assert_eq!(count(&events, |event| matches!(event, EventMsg::Error { .. })), 1, "失败只落一条 Error");
    let info = turn_error(&events);
    assert_eq!(info.code, "AUTH");
    assert_eq!(info.attempts, Some(0));
}

/// C5：次数用尽。5 次重试 + 最后一次终局失败，`attempts` 如实记录。
#[tokio::test]
async fn c5_exhausts_retries_then_fails() {
    let steps = (0..6).map(|_| Step::Fail(server_error())).collect();
    let (reason, events, calls) = harness(steps, fast_policy()).run().await;

    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 6, "1 次首试 + 5 次重试");
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { phase: RetryPhase::Scheduled, .. })), 5);
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { phase: RetryPhase::Started, .. })), 5);
    let all = phases(&events);
    assert_eq!(all.last(), Some(&RetryPhase::Exhausted), "放弃点必须是最后一个重试阶段");
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { phase: RetryPhase::Exhausted, .. })), 1);
    assert_eq!(turn_error(&events).attempts, Some(5));
}

/// C6：空响应（无 delta、无 calls、无 usage）触发重试；第二次正常则完成。
#[tokio::test]
async fn c6_empty_response_is_retried() {
    let (reason, events, calls) = harness(vec![Step::Events(vec![]), Step::Events(answered())], fast_policy()).run().await;

    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(calls, 2, "空响应必须重试");
    assert_eq!(phases(&events), vec![RetryPhase::Scheduled, RetryPhase::Started]);
    assert_eq!(count(&events, |event| matches!(event, EventMsg::AgentMessageDelta { .. })), 1);
}

/// C7：退避等待必须能被取消打断——`select!` 而不是裸 `sleep`。
#[tokio::test]
async fn c7_cancel_during_backoff_is_not_ignored() {
    let case = harness(
        vec![Step::Fail(server_error()), Step::Events(answered())],
        RetryPolicy { initial_delay: Duration::from_secs(2), max_delay: Duration::from_secs(2), ..RetryPolicy::new(5) },
    );
    *case.sink.cancel_on_scheduled.lock().unwrap() = Some(case.cancel.clone());

    let (reason, events, calls) = tokio::time::timeout(Duration::from_secs(1), case.run())
        .await
        .expect("取消没有打断退避等待：仍在睡满 2 秒");

    assert_eq!(reason, TurnEndReason::Cancelled);
    assert_eq!(calls, 1, "取消后不得再发请求");
    assert_eq!(phases(&events), vec![RetryPhase::Scheduled], "取消发生在等待中：不发 started、不发 exhausted");
    assert_eq!(count(&events, |event| matches!(event, EventMsg::Error { .. })), 0);
}

/// C8：重试不消耗轮数预算（同一逻辑轮的传输恢复）。
#[tokio::test]
async fn c8_retry_does_not_claim_another_round() {
    let case = harness(vec![Step::Fail(server_error()), Step::Events(answered())], fast_policy());
    let budget = case.budget.clone();
    let (reason, _, _) = case.run().await;

    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(budget.rounds_started(), 1, "重试不是新一轮");
}

/// C9a：零增量时空闲超时属于可重试的 TIMEOUT。
#[tokio::test(start_paused = true)]
async fn c9a_idle_timeout_before_any_delta_is_retried() {
    let (reason, events, calls) = harness(vec![Step::Hang, Step::Events(answered())], fast_policy()).run().await;

    assert_eq!(reason, TurnEndReason::Completed, "零增量超时必须允许重试，否则端点抖动直接报废整轮");
    assert_eq!(calls, 2);
    assert_eq!(phases(&events), vec![RetryPhase::Scheduled, RetryPhase::Started]);
}

/// C9b：吐过正文后才空闲超时——不重试。
#[tokio::test(start_paused = true)]
async fn c9b_idle_timeout_after_delta_is_terminal() {
    let (reason, events, calls) =
        harness(vec![Step::DeltaThenHang("说了半句".into()), Step::Events(answered())], fast_policy()).run().await;

    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 1);
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { .. })), 0);
    assert_eq!(turn_error(&events).code, "TIMEOUT");
}

/// C10：`Completed{usage}` 之后再 `Failed` 属协议异常——已结算的采样不得重发。
#[tokio::test]
async fn c10_failure_after_usage_is_not_retried() {
    let (reason, events, calls) = harness(
        vec![Step::Events(vec![
            ModelStreamEvent::Completed { usage: usage() },
            ModelStreamEvent::Failed { message: "迟到的失败".into(), error: Some(server_error()) },
        ])],
        fast_policy(),
    )
    .run()
    .await;

    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 1, "拿到 usage 后重试会把已结算的采样再算一遍");
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { .. })), 0);
    assert_eq!(turn_error(&events).attempts, Some(0));
}

/// C11：`Retry-After` 超过上限 → 一次都不重试，但必须说清“要求 120 秒 / 上限 10 秒”。
#[tokio::test]
async fn c11_retry_after_over_cap_explains_why_it_gave_up() {
    let (reason, events, calls) = harness(
        vec![Step::Fail(rate_limited().with_retry_after(Some(120_000)))],
        RetryPolicy::default(),
    )
    .run()
    .await;

    let expected = "服务端要求等待 120 秒，超过上限 10 秒，已放弃重试";
    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 1, "端点要求等待超过上限：不得擅自缩短后重试");
    assert_eq!(
        count(&events, |event| matches!(event, EventMsg::ModelRetry { phase: RetryPhase::Scheduled, .. })),
        0,
        "放弃时不该先发 scheduled"
    );
    let exhausted = events
        .iter()
        .find_map(|event| match event {
            EventMsg::ModelRetry { phase: RetryPhase::Exhausted, message, delay_ms, .. } => Some((message.clone(), *delay_ms)),
            _ => None,
        })
        .expect("放弃点必须留下 exhausted 事件");
    assert_eq!(exhausted.0, expected);
    assert_eq!(exhausted.1, 0);
    let error_message = events
        .iter()
        .find_map(|event| match event {
            EventMsg::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("终局必须发 Error");
    assert_eq!(error_message, expected);
    let info = turn_error(&events);
    assert_eq!(info.message, expected);
    assert_eq!(info.retry_after_secs, Some(120), "端点原值必须带进 TurnErrorInfo");
    assert_eq!(info.attempts, Some(0));
}

/// C12：有工具调用但无文本、随后流内失败——不是空响应，不能重试，工具不得执行。
#[tokio::test]
async fn c12_failure_with_tool_calls_but_no_text_is_not_empty_response() {
    let (reason, events, calls) = harness(
        vec![Step::Events(vec![
            ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "echo".into(),
                arguments: r#"{"text":"hi"}"#.into(),
                call_id: "call-1".into(),
            }),
            // 无结构化错误 → UNKNOWN（不在白名单），一次终局。
            ModelStreamEvent::Failed { message: "上半截没了".into(), error: None },
        ])],
        fast_policy(),
    )
    .run()
    .await;

    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 1, "有工具调用就不是空响应：重试会重复执行工具");
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { .. })), 0);
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ToolCallBegin { .. })), 0, "失败尝试里的调用不得执行");
    let info = turn_error(&events);
    assert_eq!(info.code, "UNKNOWN");
    assert_eq!(info.attempts, Some(0));
}

/// 白名单之外的额外守卫：`max_retries = 0` 与旧行为逐字一致（无任何 model_retry 事件）。
#[tokio::test]
async fn disabled_policy_keeps_legacy_single_attempt() {
    let (reason, events, calls) = harness(vec![Step::Fail(server_error())], RetryPolicy::disabled()).run().await;

    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 1);
    assert_eq!(count(&events, |event| matches!(event, EventMsg::ModelRetry { .. })), 0);
    assert_eq!(turn_error(&events).attempts, Some(0));
    // 顺带确认错误分类没被重试逻辑改掉。
    assert_eq!(turn_error(&events).code, "SERVER");
}

/// C13：重试耗尽必须落进档案——重开应用后仍能从 `turn_end` 读出「重试过 5 次」。
///
/// 用户报「发消息要等半天才失败」时，界面上只有一次失败；若次数只活在内存事件里，
/// 翻档案就只能看到一次请求失败，前面几十秒的等待没有解释。断言只读落盘文件。
#[tokio::test]
async fn c13_exhausted_attempts_are_persisted_in_rollout() {
    use shaniu_lib::rollout::{self, ConfigSnapshot, RolloutItem, ROLLOUT_DIR};

    let data_root = shaniu_lib::test_support::TestTempDir::new("retry-rollout");
    let meta = RolloutItem::SessionMeta {
        session_id: "sess-retry".into(),
        agent_id: "agent-retry".into(),
        parent_id: None,
        created_at: 1_700_000_000_000,
        config_snapshot: ConfigSnapshot {
            provider: "openai-chat".into(),
            base_url: "https://x/v1".into(),
            model: "m".into(),
            sandbox: "WorkspaceWrite".into(),
            approval: "Ask".into(),
            workspace_root: data_root.display().to_string(),
        },
    };
    let writer = Arc::new(rollout::RolloutWriter::create(&data_root.join(ROLLOUT_DIR), &meta).unwrap());
    let path = writer.path().to_path_buf();

    let steps = (0..6).map(|_| Step::Fail(server_error())).collect();
    let (reason, _, calls) = harness_with_rollout(steps, fast_policy(), Some(writer)).run().await;
    assert_eq!(reason, TurnEndReason::Failed);
    assert_eq!(calls, 6, "首试 + 5 次重试");

    let archive = rollout::read(&path).unwrap();
    let (end_reason, end_error) = archive
        .items
        .iter()
        .find_map(|item| match item {
            RolloutItem::TurnEnd { reason, error, .. } => Some((reason.clone(), error.clone())),
            _ => None,
        })
        .expect("重试耗尽后档案必须有 turn_end");
    assert_eq!(end_reason, "failed");
    let info = end_error.expect("失败回合必须带结构化原因");
    // 反向对照：删掉 GiveUp 分支的 `info.attempts = Some(retries)`，这里变 None 即红。
    assert_eq!(info.attempts, Some(5), "重开应用后必须能从档案看出这轮重试过 5 次");
    assert_eq!(info.code, "SERVER");
}
