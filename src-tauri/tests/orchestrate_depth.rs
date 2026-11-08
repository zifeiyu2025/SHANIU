//! 健身函数 FF-106（docs/02 §6）：`depth ≥ 1` 的代理没有 spawn 工具。
//!
//! 两层判据缺一不可：
//! 1. **声明层**：子代理的模型请求里根本不出现四个编排工具（模型看不到就不会调用）；
//! 2. **分发层**：即使模型伪造工具名，`ToolRouter::execute` 也必须拒绝。
//!
//! 只测第一层会漏掉「模型幻觉工具名」；只测第二层会把「没有能力」误当成「没有入口」。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use shaniu_lib::agent_loop::{AgentInstance, AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::EventSink;
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent, Role, Usage};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::tool_router::orchestration::{
    AgentOrchestrator, AgentOutcome, AgentScope, AGENT_TOOL_NAMES,
};
use shaniu_lib::tool_router::{PlanState, ToolContext, ToolError, ToolRouter};
use tokio_util::sync::CancellationToken;

struct DiscardSink;

impl EventSink for DiscardSink {
    fn emit(&self, _msg: &shaniu_lib::events::EventMsg) -> Result<(), String> {
        Ok(())
    }
}

/// 记录每次采样收到的工具声明名。
struct CaptureBackend {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl ModelBackend for CaptureBackend {
    async fn stream(
        &self,
        _history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, String> {
        self.seen
            .lock()
            .unwrap()
            .push(tools.iter().map(|tool| tool.name.clone()).collect());
        Ok(Box::pin(futures::stream::iter(vec![ModelStreamEvent::Completed {
            usage: Usage { input_tokens: 1, cached_input_tokens: None, output_tokens: 1 },
        }])))
    }
}

/// 只用于让声明层放行的空编排实现；本测试不真正 spawn。
struct NoopOrchestrator;

#[async_trait]
impl AgentOrchestrator for NoopOrchestrator {
    async fn spawn_agent(
        &self,
        _parent: &AgentScope,
        _task: &str,
        _budget_tokens: Option<u64>,
        _profile: Option<&str>,
        _allowed_tools: Option<&[String]>,
    ) -> Result<String, String> {
        Err("本测试不派生子代理".into())
    }

    async fn resume_agent(
        &self,
        _parent: &AgentScope,
        _agent_id: &str,
        _task: Option<&str>,
    ) -> Result<String, String> {
        Err("本测试不恢复子代理".into())
    }

    async fn wait_agents(&self, _agent_ids: &[String], _timeout: Duration) -> Vec<AgentOutcome> {
        Vec::new()
    }

    fn send_message(&self, _agent_id: &str, _message: &str) -> Result<(), String> {
        Err("本测试不注入消息".into())
    }
}

fn router() -> ToolRouter {
    let mut router = ToolRouter::new();
    shaniu_lib::tools::register_builtin(&mut router, Arc::new(PlanState::new()));
    router
}

fn engine(seen: Arc<Mutex<Vec<Vec<String>>>>) -> AgentLoop {
    AgentLoop {
        backend: Arc::new(CaptureBackend { seen }),
        router: Arc::new(router()),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 1,
        token_budget: 0,
        tree_budget: Default::default(),
        policy: policy::SessionPolicy {
            approval: ApprovalMode::Ask,
            sandbox: SandboxMode::WorkspaceWrite,
        },
        rollout: None,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
    }
}

fn scope(depth: u32) -> AgentScope {
    AgentScope {
        agent_id: if depth == 0 { "root".into() } else { "child".into() },
        parent_id: (depth > 0).then(|| "root".to_string()),
        depth,
        cancel: CancellationToken::new(),
        sink: Arc::new(DiscardSink),
        allowed_tools: None,
    }
}

#[tokio::test]
async fn child_declarations_never_include_orchestration_tools() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let engine = engine(seen.clone());
    let mut child = AgentInstance::child("child", "root", "子任务", CancellationToken::new());
    engine.run_instance(&mut child, Arc::new(DiscardSink)).await;

    let declarations = seen.lock().unwrap();
    assert_eq!(declarations.len(), 1, "应恰好采样一次");
    for tool in AGENT_TOOL_NAMES {
        assert!(
            !declarations[0].iter().any(|name| name == tool),
            "子代理不应看到编排工具 {tool}；实际声明: {:?}",
            declarations[0]
        );
    }
}

#[tokio::test]
async fn root_with_orchestrator_does_see_orchestration_tools() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let engine = engine(seen.clone());
    let mut root = AgentInstance::root(
        vec![HistoryItem::Message {
            role: Role::User,
            text: "根任务".into(),
            reasoning: None,
        }],
        0,
        CancellationToken::new(),
    );
    engine
        .run_instance_with_orchestrator(
            &mut root,
            Arc::new(DiscardSink),
            Some(Arc::new(NoopOrchestrator)),
        )
        .await;

    let declarations = seen.lock().unwrap();
    assert_eq!(declarations.len(), 1);
    for tool in AGENT_TOOL_NAMES {
        assert!(
            declarations[0].iter().any(|name| name == tool),
            "root 必须看到编排工具 {tool}；实际声明: {:?}",
            declarations[0]
        );
    }
}

#[tokio::test]
async fn dispatch_layer_rejects_forged_orchestration_calls_from_children() {
    let router = router();
    // 防假绿：工具确实注册在路由里，拒绝必须来自门控而不是「未知工具」。
    let specs = router.specs();
    for tool in AGENT_TOOL_NAMES {
        assert!(specs.iter().any(|spec| spec.name == tool), "{tool} 应已注册");
    }

    let ctx = ToolContext::new(
        std::env::temp_dir(),
        CancellationToken::new(),
        "call-1",
        Arc::new(DiscardSink),
        SandboxMode::WorkspaceWrite,
    )
    .with_scope(scope(1));
    for tool in AGENT_TOOL_NAMES {
        let error = router
            .execute(tool, serde_json::json!({ "task": "伪造调用" }), &ctx)
            .await
            .expect_err("子代理伪造编排工具名必须被拒");
        match error {
            ToolError::InvalidArgs(message) => {
                assert!(message.contains("不允许"), "{tool} 的拒绝理由应说明深度门控: {message}");
            }
            other => panic!("{tool} 应返回 InvalidArgs，实际: {other:?}"),
        }
    }

    // 没有代理上下文（例如测试/装配遗漏）也不得放行，避免「忘记注入 scope」变成后门。
    let bare = ToolContext::new(
        std::env::temp_dir(),
        CancellationToken::new(),
        "call-2",
        Arc::new(DiscardSink),
        SandboxMode::WorkspaceWrite,
    );
    let error = router
        .execute("spawn_agent", serde_json::json!({ "task": "无上下文" }), &bare)
        .await
        .expect_err("没有代理上下文不得调用编排工具");
    assert!(matches!(error, ToolError::InvalidArgs(message) if message.contains("没有代理上下文")));
}
