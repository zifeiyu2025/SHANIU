//! 健身函数 FF-102（docs/02 §6）：权限 min 合成与审批冒泡。
//!
//! 三条安全语义各自有独立的负向用例，不做「看起来合理」的断言：
//! 1. 子代理**看不到**白名单外的工具（声明层），即使伪造调用也被拒（门禁层），
//!    且执行器命中次数必须为 0——拒绝要发生在副作用之前；
//! 2. 收窄点 = `min(父代理, profile, allowed_tools)`：explorer 只读、交集为空就是空，
//!    `resume` 不得把已收窄的能力重新放大；
//! 3. 审批请求必须带来源代理与任务摘要（ADR-103 冒泡），否则用户在统一时间线上
//!    分不清是哪个子代理在要权限。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use shaniu_lib::agent_loop::{AgentInstance, AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{ApprovalDecision, EventMsg, EventSink};
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent, ResponseItem, Usage};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::subagent_registry::{SpawnError, SpawnNarrowing, SubagentRegistry};
use shaniu_lib::tool_router::orchestration::AgentScope;
use shaniu_lib::tool_router::{PlanState, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolRouter, ToolSpec};
use tokio_util::sync::CancellationToken;

// ============================== 测试基建 ==============================

struct DiscardSink;

impl EventSink for DiscardSink {
    fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct CapturingSink(Arc<Mutex<Vec<EventMsg>>>);

impl EventSink for CapturingSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        self.0.lock().unwrap().push(msg.clone());
        Ok(())
    }
}

/// 记录每次采样收到的工具声明名，并按脚本发起一次工具调用。
struct ScriptedBackend {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    tool: String,
}

#[async_trait]
impl ModelBackend for ScriptedBackend {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
        self.seen
            .lock()
            .unwrap()
            .push(tools.iter().map(|tool| tool.name.clone()).collect());
        let already_answered = history
            .iter()
            .any(|item| matches!(item, HistoryItem::FunctionCallOutput { .. }));
        let events = if already_answered {
            vec![ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 1, cached_input_tokens: None, output_tokens: 1 },
            }]
        } else {
            vec![
                ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                    name: self.tool.clone(),
                    arguments: "{}".into(),
                    call_id: "c1".into(),
                }),
                ModelStreamEvent::Finished,
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// 只记录命中次数的工具：用来证明「拒绝发生在执行之前」。
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
        Ok(ToolOutput::ok("执行了"))
    }
}

fn counting_router(names: &[&str], hits: Arc<AtomicUsize>) -> ToolRouter {
    let mut router = ToolRouter::new();
    for name in names {
        router.register(Arc::new(CountingTool {
            spec: ToolSpec::new(name, "测试工具", serde_json::json!({ "type": "object" })),
            hits: hits.clone(),
        }));
    }
    router
}

fn engine(router: ToolRouter, backend: Arc<dyn ModelBackend>) -> AgentLoop {
    AgentLoop {
        backend,
        router: Arc::new(router),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 3,
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
        verify: Default::default(),
    }
}

fn root_scope() -> AgentScope {
    AgentScope {
        agent_id: "root".into(),
        parent_id: None,
        depth: 0,
        cancel: CancellationToken::new(),
        sink: Arc::new(DiscardSink),
        allowed_tools: None,
    }
}

// ============================== 用例 ==============================

/// 白名单外的工具：声明层不出现、门禁层直接拒绝、执行器命中 0 次、不弹审批。
#[tokio::test]
async fn child_cannot_reach_tools_outside_its_allowlist() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(ScriptedBackend {
        seen: seen.clone(),
        tool: "unified_exec".into(),
    });
    let engine = engine(counting_router(&["read_file", "unified_exec"], hits.clone()), backend);

    let mut child = AgentInstance::child("child-1", "root", "只读探索", CancellationToken::new());
    child.allowed_tools = Some(Arc::new(["read_file".to_string()].into_iter().collect()));

    let events = Arc::new(Mutex::new(Vec::new()));
    engine
        .run_instance(&mut child, Arc::new(CapturingSink(events.clone())))
        .await;

    let declarations = seen.lock().unwrap();
    assert!(
        declarations[0].contains(&"read_file".to_string()),
        "白名单内的工具必须仍然可见：{:?}",
        declarations[0]
    );
    assert!(
        !declarations[0].contains(&"unified_exec".to_string()),
        "白名单外的工具不得下发模型：{:?}",
        declarations[0]
    );

    assert_eq!(hits.load(Ordering::SeqCst), 0, "被拒绝的工具不得执行");
    let events = events.lock().unwrap();
    assert!(
        !events.iter().any(|event| matches!(event, EventMsg::ExecApprovalRequest { .. })),
        "白名单外的工具连审批都不该弹：弹了就等于给模型一个提权申请入口"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventMsg::ToolCallEnd { error: Some(error), .. } if error.contains("策略拒绝")
        )),
        "拒绝必须回填为策略拒绝，而不是用户拒绝"
    );
}

/// explorer profile = 只读；与 allowed_tools 求交集；交集为空就是空集。
#[tokio::test]
async fn explorer_profile_only_sees_read_tools_and_intersects_with_allowlist() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(ScriptedBackend {
        seen: seen.clone(),
        tool: "read_file".into(),
    });
    let engine = Arc::new(engine(counting_router(&["read_file", "unified_exec"], hits), backend));
    let registry = Arc::new(SubagentRegistry::new(2));
    let parent = root_scope();

    let agent_id = registry
        .spawn_with_constraints(
            engine.clone(),
            &parent,
            "只读探索",
            None,
            None,
            SpawnNarrowing { profile: Some("explorer"), allowed_tools: None },
        )
        .expect("explorer 派生应成功");
    registry.wait_exit(&agent_id, Duration::from_secs(5)).await;

    let first_declarations = seen.lock().unwrap()[0].clone();
    assert!(first_declarations.contains(&"read_file".to_string()));
    assert!(
        !first_declarations.contains(&"unified_exec".to_string()),
        "explorer 不得看到执行工具：{:?}",
        first_declarations
    );

    // 显式 allowed_tools 与 profile 取交集：只读 ∩ 执行 = 空集
    let empty_id = registry
        .spawn_with_constraints(
            engine.clone(),
            &parent,
            "交集为空",
            None,
            None,
            SpawnNarrowing {
                profile: Some("explorer"),
                allowed_tools: Some(&["unified_exec".to_string()]),
            },
        )
        .expect("交集为空也应允许派生（只是没有可用工具）");
    registry.wait_exit(&empty_id, Duration::from_secs(5)).await;
    let second_declarations = seen.lock().unwrap().last().cloned().unwrap();
    assert!(
        second_declarations.is_empty(),
        "profile 与 allowed_tools 的交集必须生效：{:?}",
        second_declarations
    );
}

/// resume 不得把已收窄的工具集放大回全量。
#[tokio::test]
async fn resume_keeps_the_narrowed_allowlist() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(ScriptedBackend {
        seen: seen.clone(),
        tool: "read_file".into(),
    });
    let engine = Arc::new(engine(counting_router(&["read_file", "unified_exec"], hits), backend));
    let registry = Arc::new(SubagentRegistry::new(2));
    let parent = root_scope();

    let agent_id = registry
        .spawn_with_constraints(
            engine.clone(),
            &parent,
            "只读任务",
            None,
            None,
            SpawnNarrowing { profile: None, allowed_tools: Some(&["read_file".to_string()]) },
        )
        .expect("收窄派生应成功");
    registry.wait_exit(&agent_id, Duration::from_secs(5)).await;

    let resumed = registry
        .resume(engine.clone(), &parent, &agent_id, None, None)
        .expect("退出后应可续跑");
    registry.wait_exit(&resumed, Duration::from_secs(5)).await;

    let declarations = seen.lock().unwrap();
    assert!(declarations.len() >= 2, "派生与续跑都必须采样：{declarations:?}");
    for (index, names) in declarations.iter().enumerate() {
        assert!(
            !names.contains(&"unified_exec".to_string()),
            "第 {index} 次采样出现了白名单外的工具：{names:?}"
        );
    }
    assert!(
        declarations.last().unwrap().contains(&"read_file".to_string()),
        "续跑后收窄集合内的工具必须仍然可用：{:?}",
        declarations.last()
    );
}

/// 拼错的工具名与未知 profile 必须 fail-closed，且不留下半成品条目。
#[tokio::test]
async fn invalid_narrowing_parameters_fail_closed() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(ScriptedBackend {
        seen,
        tool: "read_file".into(),
    });
    let engine = Arc::new(engine(counting_router(&["read_file"], hits), backend));
    let registry = Arc::new(SubagentRegistry::new(2));
    let parent = root_scope();

    let unknown_tool = registry.spawn_with_constraints(
        engine.clone(),
        &parent,
        "拼错工具名",
        None,
        None,
        SpawnNarrowing { profile: None, allowed_tools: Some(&["read_fiel".to_string()]) },
    );
    assert!(matches!(unknown_tool, Err(SpawnError::UnknownTool { .. })));

    let unknown_profile = registry.spawn_with_constraints(
        engine.clone(),
        &parent,
        "未知 profile",
        None,
        None,
        SpawnNarrowing { profile: Some("admin"), allowed_tools: None },
    );
    assert!(matches!(unknown_profile, Err(SpawnError::UnknownProfile { .. })));

    assert_eq!(registry.live_count(), 0, "拒绝的派生不得登记条目");
}

/// 审批冒泡：子代理发起的审批请求必须带 agent_id 与任务摘要，且拒绝后不执行。
#[tokio::test]
async fn child_approval_request_carries_origin_and_task_summary() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(ScriptedBackend {
        seen,
        tool: "unified_exec".into(),
    });
    let engine = Arc::new(engine(counting_router(&["read_file", "unified_exec"], hits.clone()), backend));
    let broker = engine.broker.clone();
    let events = Arc::new(Mutex::new(Vec::new()));

    let mut child = AgentInstance::child("child-7", "root", "统计仓库文件数", CancellationToken::new());
    let sink: Arc<dyn EventSink> = Arc::new(CapturingSink(events.clone()));
    let run = tokio::spawn(async move {
        engine.run_instance(&mut child, sink).await;
    });

    let mut request = None;
    for _ in 0..300 {
        request = events.lock().unwrap().iter().find_map(|event| match event {
            EventMsg::ExecApprovalRequest { request_id, agent_id, task_summary, .. } => {
                Some((request_id.clone(), agent_id.clone(), task_summary.clone()))
            }
            _ => None,
        });
        if request.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (request_id, agent_id, task_summary) = request.expect("子代理的 execute 必须挂起等审批");
    assert_eq!(agent_id.as_deref(), Some("child-7"), "审批卡必须标明来源代理");
    assert_eq!(task_summary.as_deref(), Some("统计仓库文件数"), "审批卡必须带任务摘要");

    assert!(broker.resolve(&request_id, ApprovalDecision::Deny));
    run.await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 0, "拒绝后不得执行");
}
