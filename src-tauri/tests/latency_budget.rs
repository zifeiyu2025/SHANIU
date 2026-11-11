//! 健身函数 FF-5（docs/01 §5）：用户提交 → 首个 UI 增量事件，P95 < 3s。
//!
//! 计时口径（写死在这里，避免每次里程碑各量各的）：
//! - **起点** = 调用 `AgentLoop::run_task` 的那一刻，等价于前端 `sendTask` 抵达后端、进入主循环；
//! - **终点** = `EventSink` 收到**首个可展示增量**的时刻——`AgentMessageDelta` 或
//!   `ReasoningDelta` 任一先到即算。刻意不取 `TurnStarted`（它不含内容，量出来是本地
//!   握手时间），也刻意**不只盯正文**：推理模型正文前要先想完，LongCat-2.5 实测首正文
//!   P95 7.1s，用旧口径这条不变量对该档位永远不成立。业界同一裁决——codex 的 TTFT 计入
//!   `OutputTextDelta | ReasoningSummaryDelta | ReasoningContentDelta`，ZCode 同时上报
//!   `model_first_content`（含思考/工具）与 `model_first_text`，两者各量各的、不相减。
//!   首正文仍单独打印，作"用户多久看到答案"的参考指标，不参与门禁。
//! - **样本** = 同一进程内连续 N 次独立任务，P95 取升序排列后第 `ceil(0.95*N)` 个（N=20 → 第 19 位）。
//!
//! 本机回环豁免（01 §5 括注）：默认用例用**桩后端**，不联网，量到的是 harness 自身开销
//! （循环调度 + 事件发射 + sink），它是真实指标的**下界**而非全部，因此不进"真实端点"的口径。
//! 真实端点（含广域网 TTFB）按里程碑手动测：`cargo test -- --ignored --nocapture`，
//! 需要 `SHANIU_TEST_API_KEY`。密钥只从环境变量读，永不入库、永不入日志（AGENTS §3）。
//!
//! 假绿防护：`measurement_..._control` 给桩加 400ms 首字节延迟，断言测得值确实 ≥ 300ms——
//! 若计时口径接错（比如量到 TurnStarted），这条会立刻红。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use shaniu_lib::agent_loop::{AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink};
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent, Usage};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use tokio_util::sync::CancellationToken;

/// P95 阈值（01 §5）：3s。
const BUDGET: Duration = Duration::from_millis(3000);
const SAMPLES: usize = 20;

/// 只记录两个时刻：**首个可展示增量**（正文或思考，任一先到）与**首条正文**。
///
/// 口径按业界成熟实现定（docs/05 SH-075 性能工单的裁决）：
/// - codex `response_event_records_turn_ttft` 把 `OutputTextDelta | ReasoningSummaryDelta
///   | ReasoningContentDelta` 全算作 TTFT，另设 TTFM 只在正文项记录；
/// - ZCode 同批上报 `model_first_content`（含思考/工具）与 `model_first_text`。
///
/// 理由：用户点下发送后，"屏幕开始动"才是可感知的反馈；把思考态排除在外，等于要求
/// 推理模型在正文之前不给任何反馈，那条预算对这类模型天然不可能满足（LongCat 实测 7.1s）。
struct FirstDeltaSink {
    first_displayable: Mutex<Option<Instant>>,
    first_text: Mutex<Option<Instant>>,
}

impl EventSink for FirstDeltaSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        match msg {
            EventMsg::AgentMessageDelta { .. } => {
                self.first_displayable.lock().unwrap().get_or_insert_with(Instant::now);
                self.first_text.lock().unwrap().get_or_insert_with(Instant::now);
            }
            EventMsg::ReasoningDelta { .. } => {
                self.first_displayable.lock().unwrap().get_or_insert_with(Instant::now);
            }
            _ => {}
        }
        Ok(())
    }
}

/// 桩后端：`stream()` 内先 sleep 再吐脚本，等价于真实端点的首字节延迟。
struct StubBackend {
    first_byte: Duration,
    script: Vec<ModelStreamEvent>,
}

#[async_trait::async_trait]
impl ModelBackend for StubBackend {
    async fn stream(
        &self,
        _history: &[HistoryItem],
        _tools: &[ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
        tokio::time::sleep(self.first_byte).await;
        Ok(Box::pin(futures::stream::iter(self.script.clone())))
    }
}

fn script() -> Vec<ModelStreamEvent> {
    vec![
        // 按思考模型的现实形态脚本化：先 reasoning，再正文
        ModelStreamEvent::ReasoningDelta { delta: "先想想".into() },
        ModelStreamEvent::OutputTextDelta { delta: "首增量".into() },
        ModelStreamEvent::Completed {
            usage: Usage {
                input_tokens: 8,
                cached_input_tokens: None,
                output_tokens: 2,
            },
        },
    ]
}

/// 单次测量：返回 (run_task 入口 → 首个可展示增量, → 首条正文) 两个耗时。
async fn measure_once(first_byte: Duration) -> (Duration, Duration) {
    let sink = Arc::new(FirstDeltaSink {
        first_displayable: Mutex::new(None),
        first_text: Mutex::new(None),
    });
    let agent = AgentLoop {
        backend: Arc::new(StubBackend { first_byte, script: script() }),
        router: Arc::new(ToolRouter::new()),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 1,
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
        retry: shaniu_lib::model_client::RetryPolicy::disabled(),
    };
    let started = Instant::now();
    let _ = agent
        .run_task("FF-5 计时探针", sink.clone(), CancellationToken::new())
        .await;
    let displayable = sink
        .first_displayable
        .lock()
        .unwrap()
        .unwrap_or_else(|| panic!("整轮跑完既无正文也无思考增量：计时终点缺失，FF-5 无从度量"));
    let text = sink
        .first_text
        .lock()
        .unwrap()
        .unwrap_or_else(|| panic!("收到思考却没收到正文：脚本或事件转发断了"));
    (displayable - started, text - started)
}

fn p95(mut xs: Vec<Duration>) -> Duration {
    xs.sort();
    let idx = (xs.len() as f64 * 0.95).ceil() as usize - 1;
    xs[idx]
}

async fn measure_all(first_byte: Duration, n: usize) -> (Vec<Duration>, Vec<Duration>) {
    let mut displayable = Vec::with_capacity(n);
    let mut text = Vec::with_capacity(n);
    for _ in 0..n {
        let (d, t) = measure_once(first_byte).await;
        displayable.push(d);
        text.push(t);
    }
    (displayable, text)
}

#[tokio::test]
async fn first_delta_latency_within_budget_on_harness_path() {
    let (displayable, text) = measure_all(Duration::ZERO, SAMPLES).await;
    let measured = p95(displayable.clone());
    let text_measured = p95(text.clone());
    let max = *displayable.iter().max().unwrap();
    println!(
        "[FF-5] 桩后端（回环豁免口径）N={SAMPLES} 首个可见反馈 P95={measured:?} max={max:?}；首正文 P95={text_measured:?}；预算={BUDGET:?}"
    );
    assert!(
        measured < BUDGET,
        "提交→首个可见反馈 P95 = {measured:?}，超出 3s 预算（max={max:?}）"
    );
    // 两个指标必须各量各的：正文一定不早于可见反馈，反过来就是终点取错了事件
    assert!(
        text_measured >= measured,
        "首正文({text_measured:?}) 竟早于首个可见反馈({measured:?})——口径取反了"
    );
}

/// 计时口径本身的守卫：真实延迟必须被量到，否则上一条是在量空气。
#[tokio::test]
async fn latency_meter_observes_backend_first_byte() {
    let (displayable, _text) = measure_all(Duration::from_millis(400), 5).await;
    let measured = p95(displayable.clone());
    println!("[FF-5] 400ms 首字节桩 P95={measured:?}（应显著大于 0）");
    assert!(
        measured >= Duration::from_millis(300),
        "计时器只量到 {measured:?}：终点取的不是首个可见增量，或起点在 sleep 之后——FF-5 口径失效"
    );
    assert!(measured < BUDGET, "带 400ms 延迟的 P95 = {measured:?} 仍应落在预算内");
}

/// 真实端点口径（按里程碑手动运行，不进 CI）。
///
/// 运行：`SHANIU_TEST_API_KEY=... cargo test --test latency_budget -- --ignored --nocapture`
/// base_url/model 可用 SHANIU_TEST_BASE_URL / SHANIU_TEST_MODEL 覆盖；密钥只读环境变量。
///
/// 首次实测基线（2026-11-05，本机 → 广域网，N=3）：
/// - `deepseek-flash`：[375ms, 1.13s, 885ms] → P95 = 1.13s **达标**；
/// - `LongCat-2.5`（推理模型）：[2.51s, 7.12s, 1.93s] → P95 = 7.12s **超预算**，
///   本用例在该端点下必然失败——这是 01 §5 的不变量对推理模型不成立的实测证据，
///   已按 §5 的失败响应登记为性能工单（见 docs/05 SH-075 注），阈值口径待用户裁决，
///   本文件不擅自放宽断言。
#[tokio::test]
#[ignore = "FF-5 真实端点测量需要密钥与外网，按里程碑手动运行（见文件头）"]
async fn first_delta_latency_within_budget_on_real_endpoint() {
    let Ok(api_key) = std::env::var("SHANIU_TEST_API_KEY") else {
        panic!("未设置 SHANIU_TEST_API_KEY：真实端点口径无法测量（勿用空密钥伪装成通过）");
    };
    let provider = shaniu_lib::model_client::provider::ProviderConfig {
        provider: std::env::var("SHANIU_TEST_PROVIDER").unwrap_or_else(|_| "openai-chat".into()),
        base_url: std::env::var("SHANIU_TEST_BASE_URL")
            .unwrap_or_else(|_| "https://api.longcat.chat/openai/v1".into()),
        model: std::env::var("SHANIU_TEST_MODEL").unwrap_or_else(|_| "LongCat-2.5".into()),
        reasoning_effort: None,
            headers: Default::default(),
    };
    let backend = Arc::new(shaniu_lib::agent_loop::ResponsesBackend {
        provider: provider.clone(),
        api_key,
    });

    let mut displayable_samples = Vec::new();
    let mut text_samples = Vec::new();
    for _ in 0..3 {
        let sink = Arc::new(FirstDeltaSink {
            first_displayable: Mutex::new(None),
            first_text: Mutex::new(None),
        });
        let agent = AgentLoop {
            backend: backend.clone(),
            router: Arc::new(ToolRouter::new()),
            broker: Arc::new(ApprovalBroker::new()),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 1,
            token_budget: 0,
        tree_budget: Default::default(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: ApprovalMode::OnRequest,
                sandbox: SandboxMode::ReadOnly,
            })),
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: None,
            retry: shaniu_lib::model_client::RetryPolicy::disabled(),
        };
        let started = Instant::now();
        let _ = agent
            .run_task("只回复两个字", sink.clone(), CancellationToken::new())
            .await;
        let displayable = sink
            .first_displayable
            .lock()
            .unwrap()
            .unwrap_or_else(|| panic!("真实端点既无正文也无思考增量（{}）", provider.model));
        displayable_samples.push(displayable - started);
        // 首正文允许缺失：推理模型可能整轮只想不答（本轮预算内没吐出正文）
        let text_at: Option<Instant> = *sink.first_text.lock().unwrap();
        if let Some(t) = text_at {
            text_samples.push(t - started);
        }
    }
    let measured = p95(displayable_samples.clone());
    let text_measured = if text_samples.is_empty() {
        Duration::MAX
    } else {
        p95(text_samples.clone())
    };
    println!(
        "[FF-5] 真实端点 {} N=3 首个可见反馈样本={:?} P95={measured:?}；首正文 P95={text_measured:?}",
        provider.model, displayable_samples
    );
    assert!(
        measured < BUDGET,
        "真实端点提交→首个可见反馈 P95 = {measured:?}，超出 3s 预算（首正文 {text_measured:?} 仅作参考，不参与门禁）"
    );
}

/// SH-018 的验收判据（docs/05：中断按钮 500ms 内停止）。
///
/// 这条一直只有文档里的数字、没有测量，和 FF-7 是同一类缺口。口径写死：
/// - **起点** = 调用 `CancellationToken::cancel()` 的那一刻（用户按下停止键在后端的对应点）；
/// - **终点** = `run_task` 返回，且事件流收到 `TurnCompleted { Cancelled }`。
///
/// 用**永不结束**的慢流桩：真实端点被取消时也是"流还在吐、连接被 drop"，
/// 快速结束的脚本量不到取消传播路径，只会量到"任务本来就跑完了"。
#[tokio::test]
async fn cancellation_stops_within_half_second() {
    const STOP_BUDGET: Duration = Duration::from_millis(500);
    /// 每 900ms 吐一个增量、永不结束的流——间隔**故意大于 500ms 预算**：
/// 取消只有在整段响应结束后才被检查的实现，这里必然要等到下一个增量才停，实测会超过预算；
/// 只有把取消和取增量放在同一个 select 里（当前实现），才能在流中间立刻收手。
    struct NeverEndingBackend;

    #[async_trait::async_trait]
    impl ModelBackend for NeverEndingBackend {
        async fn stream(
            &self,
            _history: &[HistoryItem],
            _tools: &[ToolDeclarationShim],
            _instructions: &str,
        ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
            let items = futures::stream::unfold(0u32, |n| async move {
                tokio::time::sleep(Duration::from_millis(900)).await;
                Some((
                    ModelStreamEvent::OutputTextDelta { delta: format!("片段{n}") },
                    n + 1,
                ))
            });
            Ok(Box::pin(items))
        }
    }

    /// 只关心两件事：有没有收到终态、以及是哪个终态。
    #[derive(Default)]
    struct EndReason(Mutex<Option<&'static str>>);

    impl EventSink for EndReason {
        fn emit(&self, msg: &EventMsg) -> Result<(), String> {
            if let EventMsg::TurnCompleted { reason, .. } = msg {
                *self.0.lock().unwrap() = Some(match reason {
                    shaniu_lib::events::TurnEndReason::Cancelled => "cancelled",
                    shaniu_lib::events::TurnEndReason::Completed => "completed",
                    shaniu_lib::events::TurnEndReason::Failed => "failed",
                    shaniu_lib::events::TurnEndReason::BudgetExhausted => "budget_exhausted",
                });
            }
            Ok(())
        }
    }

    let end = Arc::new(EndReason::default());
    let cancel = CancellationToken::new();
    let agent = Arc::new(AgentLoop {
        backend: Arc::new(NeverEndingBackend),
        router: Arc::new(ToolRouter::new()),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 8,
        token_budget: 0,
        tree_budget: Default::default(),
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite })),
        rollout: None,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
        retry: shaniu_lib::model_client::RetryPolicy::disabled(),
    });
    let sink: Arc<dyn EventSink> = end.clone();
    let token = cancel.clone();
    let running = tokio::spawn(async move { agent.run_task("一直吐流的任务", sink, token).await });

    // 让流真正跑起来（至少吐过几个增量），否则量到的是"还没开始就停了"
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!running.is_finished(), "永不结束的流竟已跑完，说明取消传播路径根本没被走到");

    let started = Instant::now();
    cancel.cancel();
    let summary = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("取消后 5s 仍未返回：取消没有贯穿到模型流")
        .expect("任务 panic");
    let elapsed = started.elapsed();

    println!("[SH-018] cancel() → run_task 返回 = {elapsed:?}（预算 {STOP_BUDGET:?}），终态={:?}", summary.reason);
    assert!(
        elapsed < STOP_BUDGET,
        "按下停止到主循环返回用了 {elapsed:?}，超出 500ms 预算"
    );
    assert_eq!(summary.reason, shaniu_lib::events::TurnEndReason::Cancelled);
    assert_eq!(
        *end.0.lock().unwrap(),
        Some("cancelled"),
        "后端自己按 cancelled 收口了，但没广播出去——前端会一直转圈"
    );
}
