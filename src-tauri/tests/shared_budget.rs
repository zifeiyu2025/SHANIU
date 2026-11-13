//! 健身函数 FF-103（docs/02 §6）：预算共账——任一代理触顶后，整树不再发起新模型请求。
//!
//! 判据不是「某个代理收到 BudgetExhausted 事件」（那只能证明它自己停了），而是
//! **后端收到的请求数不再增长**。因此用计数后端直接量发起侧，脚本/事件只作为辅助证据。
//!
//! 用量口径：每次采样 input=10、output=5，共 15。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use shaniu_lib::agent_loop::{
    AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim, TreeBudget,
};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink};
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent, ResponseItem, Usage};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::subagent_registry::{ExitStatus, SubagentRegistry};
use shaniu_lib::tool_router::orchestration::AgentScope;
use shaniu_lib::tool_router::{PlanState, ToolError, ToolExecutor, ToolOutput, ToolRouter, ToolSpec};
use tokio_util::sync::CancellationToken;

const USAGE_PER_REQUEST: u64 = 15;
const WAIT: Duration = Duration::from_secs(5);

/// 每次采样都返回「调用工具 + 上报用量」，逼出多轮，便于跨过预算线。
struct CountingBackend {
    requests: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelBackend for CountingBackend {
    async fn stream(
        &self,
        _history: &[HistoryItem],
        _tools: &[ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
        let call = self.requests.fetch_add(1, Ordering::SeqCst) + 1;
        let events = vec![
            ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"x":1}"#.into(),
                call_id: format!("c{call}"),
            }),
            ModelStreamEvent::Completed {
                usage: Usage {
                    input_tokens: 10,
                    cached_input_tokens: None,
                    output_tokens: 5,
                },
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 名字 `read_file` → 风险 read → 自动放行；本测试不关心工具行为。
struct EchoTool {
    spec: ToolSpec,
}

impl Default for EchoTool {
    fn default() -> Self {
        Self {
            spec: ToolSpec::new("read_file", "回显", serde_json::json!({ "type": "object" })),
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for EchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn execute(&self, _args: serde_json::Value, _ctx: &shaniu_lib::tool_router::ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("echo"))
    }
}

struct DiscardSink;

impl EventSink for DiscardSink {
    fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
        Ok(())
    }
}

fn engine(requests: Arc<CountingBackend>, budget: TreeBudget) -> Arc<AgentLoop> {
    let mut router = ToolRouter::new();
    router.register(Arc::new(EchoTool::default()));
    Arc::new(AgentLoop {
        backend: requests,
        router: Arc::new(router),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 5,
        token_budget: 0,
        manual_token_budget: 0,
        tree_budget: budget,
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
        verify: Default::default(),
    })
}

fn root_scope(cancel: CancellationToken) -> AgentScope {
    AgentScope {
        agent_id: "root".into(),
        parent_id: None,
        depth: 0,
        cancel,
        sink: Arc::new(DiscardSink),
        allowed_tools: None,
    }
}

/// FF-103 主用例：全树预算触顶后，后来者连第一次模型请求都发不出去。
#[tokio::test]
async fn tree_budget_exhaustion_stops_every_agent_before_a_new_request() {
    let requests = Arc::new(CountingBackend { requests: AtomicUsize::new(0) });
    // 上限 30 = 两次采样；第 3 轮必须被拦在采样之前。
    let budget = TreeBudget::new(Some(30), None);
    let engine = engine(requests.clone(), budget.clone());
    let registry = Arc::new(SubagentRegistry::new(2));
    let cancel = CancellationToken::new();

    let a = registry
        .spawn(engine.clone(), &root_scope(cancel.clone()), "子任务 A", None)
        .expect("派生 A");
    let exit_a = registry.wait_exit(&a, WAIT).await.expect("A 应结束");
    assert_eq!(exit_a.status, ExitStatus::Failed, "预算耗尽没做完 → failed");
    assert_eq!(requests.requests.load(Ordering::SeqCst), 2);
    assert_eq!(budget.tokens_spent(), 2 * USAGE_PER_REQUEST);

    // 第二个代理共享同一本账：此时已触顶，它不得发出任何请求。
    let b = registry
        .spawn(engine, &root_scope(cancel), "子任务 B", None)
        .expect("派生 B");
    let exit_b = registry.wait_exit(&b, WAIT).await.expect("B 应结束");
    assert_eq!(exit_b.status, ExitStatus::Failed);
    assert_eq!(
        requests.requests.load(Ordering::SeqCst),
        2,
        "触顶后整树不得再发起模型请求（FF-103）"
    );
}

/// `spawn_agent.budget_tokens` 只收窄该子树：子树自己触顶后停下，
/// 兄弟仍可继续，且子树的消耗照旧汇总到全树账本。
#[tokio::test]
async fn spawn_budget_narrows_only_its_own_subtree() {
    let requests = Arc::new(CountingBackend { requests: AtomicUsize::new(0) });
    let budget = TreeBudget::new(Some(1000), None);
    let engine = engine(requests.clone(), budget.clone());
    let registry = Arc::new(SubagentRegistry::new(2));
    let cancel = CancellationToken::new();

    let limited = registry
        .spawn_with_budget(
            engine.clone(),
            &root_scope(cancel.clone()),
            "只要一轮预算的子任务",
            None,
            Some(USAGE_PER_REQUEST),
        )
        .expect("派生子树");
    let exit = registry.wait_exit(&limited, WAIT).await.expect("子树应结束");
    assert_eq!(exit.status, ExitStatus::Failed);
    assert_eq!(requests.requests.load(Ordering::SeqCst), 1, "子树自己的上限先生效");
    assert_eq!(
        budget.tokens_spent(),
        USAGE_PER_REQUEST,
        "子树消耗必须向上汇总到全树账本"
    );

    // 兄弟不受该子树上限影响；它自己跑到 max_turns（5 轮）才收口。
    let sibling = registry
        .spawn(engine, &root_scope(cancel), "兄弟任务", None)
        .expect("派生兄弟");
    let exit = registry.wait_exit(&sibling, WAIT).await.expect("兄弟应结束");
    assert_eq!(exit.status, ExitStatus::Failed, "跑到轮数上限 → failed");
    assert_eq!(requests.requests.load(Ordering::SeqCst), 1 + 5);
    assert_eq!(budget.tokens_spent(), USAGE_PER_REQUEST * 6);
}
