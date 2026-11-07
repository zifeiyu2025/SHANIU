//! 健身函数 FF-101（docs/02 §6）：父代理取消后，子代理存活数 500ms 内归零。
//!
//! 度量口径：
//! - **起点** = 父令牌 `cancel()` 返回的那一刻；
//! - **终点** = 两个子代理都收到 `TurnCompleted{Cancelled}`，且它们持有的模型流全部 drop；
//! - **存活数** 取后端里仍被持有的流句柄数量，而不是注册表状态字段——状态是记账，
//!   流句柄才是「真的还有东西在跑」。
//!
//! 用**永不结束**的流：快速结束的脚本量不到取消传播，只会量到「任务本来就跑完了」。
//! 假绿防护：取消前先断言两条流确实已建立（`live == 2`），否则测的是空转。

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::Stream;
use shaniu_lib::agent_loop::{AgentInstance, AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink, TurnEndReason};
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::subagent_registry::{ExitStatus, SubagentRegistry};
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use tokio_util::sync::CancellationToken;

const STOP_BUDGET: Duration = Duration::from_millis(500);

/// 流离开后减一：这是「子代理是否还活着」的唯一可信判据。
struct StreamGuard(Arc<AtomicUsize>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 永久挂起的模型流；持有 guard，drop 即证明循环/任务真的退出了。
struct HangingStream {
    _guard: StreamGuard,
}

impl Stream for HangingStream {
    type Item = ModelStreamEvent;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

struct HangingBackend {
    live: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ModelBackend for HangingBackend {
    async fn stream(
        &self,
        _history: &[HistoryItem],
        _tools: &[ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, String> {
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(HangingStream { _guard: StreamGuard(self.live.clone()) }))
    }
}

#[derive(Default)]
struct CancelledSink {
    cancelled: AtomicUsize,
}

impl EventSink for CancelledSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        if let EventMsg::TurnCompleted { reason: TurnEndReason::Cancelled, .. } = msg {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

fn engine(live: Arc<AtomicUsize>) -> Arc<AgentLoop> {
    Arc::new(AgentLoop {
        backend: Arc::new(HangingBackend { live }),
        router: Arc::new(ToolRouter::new()),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 8,
        token_budget: 0,
        policy: policy::SessionPolicy {
            approval: ApprovalMode::Ask,
            sandbox: SandboxMode::WorkspaceWrite,
        },
        rollout: None,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
    })
}

fn root_agent() -> AgentInstance {
    AgentInstance::root(
        vec![HistoryItem::Message {
            role: shaniu_lib::model_client::Role::User,
            text: "父任务".into(),
            reasoning: None,
        }],
        0,
        CancellationToken::new(),
    )
}

async fn wait_until(what: &str, timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "等待 {what} 超时");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// FF-101 主用例：父令牌取消 → 两条子流在 500ms 内关闭、两个子代理都按 cancelled 收口。
#[tokio::test]
async fn parent_cancellation_kills_every_child_within_half_second() {
    let live = Arc::new(AtomicUsize::new(0));
    let engine = engine(live.clone());
    let registry = Arc::new(SubagentRegistry::new(4));
    let root = root_agent();
    let sink_a = Arc::new(CancelledSink::default());
    let sink_b = Arc::new(CancelledSink::default());

    let a = registry
        .spawn(engine.clone(), &root, "子任务 A", sink_a.clone(), None)
        .expect("派生子代理 A");
    let b = registry
        .spawn(engine, &root, "子任务 B", sink_b.clone(), None)
        .expect("派生子代理 B");

    // 先确认两条模型流真的建立了：否则下面测到的是「还没开始就取消」。
    wait_until("两条子代理流建立", Duration::from_secs(3), || live.load(Ordering::SeqCst) == 2).await;
    assert_eq!(registry.running_count(), 2);

    let started = Instant::now();
    root.cancel.cancel();

    let exit_a = registry
        .wait_exit(&a, Duration::from_secs(3))
        .await
        .expect("子代理 A 应在取消后结束");
    let exit_b = registry
        .wait_exit(&b, Duration::from_secs(3))
        .await
        .expect("子代理 B 应在取消后结束");
    let elapsed = started.elapsed();

    // 存活数归零是 FF-101 的判据本体。
    wait_until("子代理流全部关闭", STOP_BUDGET, || live.load(Ordering::SeqCst) == 0).await;

    assert_eq!(exit_a.status, ExitStatus::Cancelled);
    assert_eq!(exit_b.status, ExitStatus::Cancelled);
    assert_eq!(registry.live_count(), 0);
    assert_eq!(
        sink_a.cancelled.load(Ordering::SeqCst) + sink_b.cancelled.load(Ordering::SeqCst),
        2,
        "两个子代理都必须广播 TurnCompleted{{Cancelled}}，否则前端泳道会一直显示运行中"
    );
    assert!(
        elapsed < STOP_BUDGET,
        "父取消到整树收口用了 {elapsed:?}，超出 FF-101 的 500ms 预算"
    );
    println!("[FF-101] 父取消 → 两个子代理收口 + 流关闭 = {elapsed:?}（预算 {STOP_BUDGET:?}）");
}

/// 注册表被释放时不得留下无主任务（docs/02 R102）：任务内部只持 Weak，
/// 因此外部句柄全部 drop 时 Drop 一定执行。
#[tokio::test]
async fn dropping_the_registry_aborts_running_children() {
    let live = Arc::new(AtomicUsize::new(0));
    let engine = engine(live.clone());
    let root = root_agent();
    let sink = Arc::new(CancelledSink::default());
    {
        let registry = Arc::new(SubagentRegistry::new(2));
        registry
            .spawn(engine, &root, "长任务", sink, None)
            .expect("派生子代理");
        wait_until("子代理流建立", Duration::from_secs(3), || live.load(Ordering::SeqCst) == 1).await;
        // registry 在这里离开作用域：Drop 取消令牌并 abort 任务。
    }

    wait_until("子代理流随注册表释放而关闭", Duration::from_secs(2), || {
        live.load(Ordering::SeqCst) == 0
    })
    .await;
    // 父令牌从未取消，说明关闭来自 Drop 而非级联——证明僵尸清理路径真的生效。
    assert!(!root.cancel.is_cancelled());
}

/// 取消一个子代理不得牵连兄弟（令牌树只在父→子方向传播）。
#[tokio::test]
async fn cancelling_one_child_keeps_its_sibling_alive() {
    let live = Arc::new(AtomicUsize::new(0));
    let engine = engine(live.clone());
    let registry = Arc::new(SubagentRegistry::new(4));
    let root = root_agent();
    let a = registry
        .spawn(engine.clone(), &root, "A", Arc::new(CancelledSink::default()), None)
        .expect("派生 A");
    let b = registry
        .spawn(engine, &root, "B", Arc::new(CancelledSink::default()), None)
        .expect("派生 B");
    wait_until("两条流建立", Duration::from_secs(3), || live.load(Ordering::SeqCst) == 2).await;

    assert!(registry.cancel(&a));
    registry.wait_exit(&a, Duration::from_secs(2)).await.expect("A 应结束");
    wait_until("A 的流关闭", Duration::from_secs(1), || live.load(Ordering::SeqCst) == 1).await;
    assert_eq!(registry.running_count(), 1);

    root.cancel.cancel();
    registry.wait_exit(&b, Duration::from_secs(2)).await.expect("B 应结束");
    wait_until("全部关闭", Duration::from_secs(2), || live.load(Ordering::SeqCst) == 0).await;
}
