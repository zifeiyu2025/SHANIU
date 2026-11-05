//! 健身函数 FF-5（docs/01 §5）：用户提交 → 首个 UI 增量事件，P95 < 3s。
//!
//! 计时口径（写死在这里，避免每次里程碑各量各的）：
//! - **起点** = 调用 `AgentLoop::run_task` 的那一刻，等价于前端 `sendTask` 抵达后端、进入主循环；
//! - **终点** = `EventSink` 收到第一条 `AgentMessageDelta` 的时刻。刻意不取 `TurnStarted`：
//!   它不含内容，用户此刻仍看不到任何输出，拿它当"首增量"会让指标失真成纯本地握手时间；
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

/// 只记录"第一条 AgentMessageDelta 的到达时刻"，其余事件忽略。
struct FirstDeltaSink(Mutex<Option<Instant>>);

impl EventSink for FirstDeltaSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        if matches!(msg, EventMsg::AgentMessageDelta { .. }) {
            let mut slot = self.0.lock().unwrap();
            if slot.is_none() {
                *slot = Some(Instant::now());
            }
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
    ) -> Result<ModelStream, String> {
        tokio::time::sleep(self.first_byte).await;
        Ok(Box::pin(futures::stream::iter(self.script.clone())))
    }
}

fn script() -> Vec<ModelStreamEvent> {
    vec![
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

/// 单次测量：返回 run_task 入口 → 首个 AgentMessageDelta 的耗时。
async fn measure_once(first_byte: Duration) -> Duration {
    let sink = Arc::new(FirstDeltaSink(Mutex::new(None)));
    let agent = AgentLoop {
        backend: Arc::new(StubBackend { first_byte, script: script() }),
        router: Arc::new(ToolRouter::new()),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 1,
        policy: policy::SessionPolicy {
            approval: ApprovalMode::Ask,
            sandbox: SandboxMode::WorkspaceWrite,
        },
    };
    let started = Instant::now();
    let _ = agent
        .run_task("FF-5 计时探针", sink.clone(), CancellationToken::new())
        .await;
    let first = sink
        .0
        .lock()
        .unwrap()
        .unwrap_or_else(|| panic!("整轮跑完仍未收到 AgentMessageDelta：计时终点缺失，FF-5 无从度量"));
    first - started
}

fn p95(mut xs: Vec<Duration>) -> Duration {
    xs.sort();
    let idx = (xs.len() as f64 * 0.95).ceil() as usize - 1;
    xs[idx]
}

async fn measure_all(first_byte: Duration, n: usize) -> Vec<Duration> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(measure_once(first_byte).await);
    }
    out
}

#[tokio::test]
async fn first_delta_latency_within_budget_on_harness_path() {
    let samples = measure_all(Duration::ZERO, SAMPLES).await;
    let measured = p95(samples.clone());
    let max = *samples.iter().max().unwrap();
    println!(
        "[FF-5] 桩后端（回环豁免口径）N={SAMPLES} P95={measured:?} max={max:?} 预算={BUDGET:?}"
    );
    assert!(
        measured < BUDGET,
        "提交→首增量 P95 = {measured:?}，超出 3s 预算（max={max:?}）"
    );
}

/// 计时口径本身的守卫：真实延迟必须被量到，否则上一条是在量空气。
#[tokio::test]
async fn latency_meter_observes_backend_first_byte() {
    let samples = measure_all(Duration::from_millis(400), 5).await;
    let measured = p95(samples.clone());
    println!("[FF-5] 400ms 首字节桩 P95={measured:?}（应显著大于 0）");
    assert!(
        measured >= Duration::from_millis(300),
        "计时器只量到 {measured:?}：终点取的不是首增量，或起点在 sleep 之后——FF-5 口径失效"
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
    };
    let backend = Arc::new(shaniu_lib::agent_loop::ResponsesBackend {
        provider: provider.clone(),
        api_key,
    });

    let mut samples = Vec::new();
    for _ in 0..3 {
        let sink = Arc::new(FirstDeltaSink(Mutex::new(None)));
        let agent = AgentLoop {
            backend: backend.clone(),
            router: Arc::new(ToolRouter::new()),
            broker: Arc::new(ApprovalBroker::new()),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 1,
            policy: policy::SessionPolicy {
                approval: ApprovalMode::Ask,
                sandbox: SandboxMode::ReadOnly,
            },
        };
        let started = Instant::now();
        let _ = agent
            .run_task("只回复两个字", sink.clone(), CancellationToken::new())
            .await;
        let first = sink
            .0
            .lock()
            .unwrap()
            .unwrap_or_else(|| panic!("真实端点未产生首增量（{}）", provider.model));
        samples.push(first - started);
    }
    let measured = p95(samples.clone());
    println!(
        "[FF-5] 真实端点 {} N=3 样本={:?} P95={measured:?}",
        provider.model, samples
    );
    assert!(measured < BUDGET, "真实端点提交→首增量 P95 = {measured:?}，超出 3s 预算");
}
