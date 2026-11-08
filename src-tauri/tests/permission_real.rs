//! SH-054 真机验证（需要密钥与外网，按里程碑手动跑）：
//! 真实模型 + 真实审批门禁，覆盖两条 SH-054 的权限路径：
//!
//! 1. **审批来源冒泡**：子代理调用 `unified_exec` 触发真实审批挂起，
//!    `ExecApprovalRequest` 必须自带该子代理的 `agent_id` 与任务摘要；放行后命令真实执行。
//! 2. **越权被拦截**（核心）：把同一个子代理以更窄的父白名单续跑（`resume`），
//!    模型在真实历史里再次调用 `unified_exec`，门禁必须直接判 `Forbidden`
//!    （回填"策略拒绝"），**不弹审批**、**执行器命中 0 次**——拒绝发生在副作用之前。
//!
//! 运行（密钥只经环境变量传入，不落盘、不打印）：
//! `SHANIU_TEST_API_KEY=... cargo test --test permission_real -- --ignored --nocapture`
//!
//! 判据不是"没报错"，而是上面两条都有可观测的事件证据。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use shaniu_lib::agent_loop::{AgentLoop, ResponsesBackend};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{ApprovalDecision, EventMsg, EventSink};
use shaniu_lib::model_client::provider::ProviderConfig;
use shaniu_lib::permission::{policy, ToolAllowlist};
use shaniu_lib::subagent_registry::{SpawnNarrowing, SubagentRegistry};
use shaniu_lib::tool_router::orchestration::AgentScope;
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use tokio_util::sync::CancellationToken;

/// 记录事件并保留归属身份；`scoped` 返回带新身份的出口（ADR-104 接线）。
struct RecorderSink {
    agent_id: String,
    events: Arc<Mutex<Vec<(String, EventMsg)>>>,
}

impl EventSink for RecorderSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        self.events
            .lock()
            .map_err(|error| error.to_string())?
            .push((self.agent_id.clone(), msg.clone()));
        Ok(())
    }

    fn scoped(&self, agent_id: &str, _parent_id: Option<&str>) -> Option<Arc<dyn EventSink>> {
        Some(Arc::new(RecorderSink {
            agent_id: agent_id.to_string(),
            events: self.events.clone(),
        }))
    }
}

fn provider() -> ProviderConfig {
    ProviderConfig {
        provider: std::env::var("SHANIU_TEST_PROVIDER").unwrap_or_else(|_| "openai-chat".into()),
        base_url: std::env::var("SHANIU_TEST_BASE_URL")
            .unwrap_or_else(|_| "https://api.longcat.chat/openai/v1".into()),
        model: std::env::var("SHANIU_TEST_MODEL").unwrap_or_else(|_| "LongCat-2.5".into()),
        reasoning_effort: None,
        headers: Default::default(),
    }
}

fn allowlist(names: &[&str]) -> ToolAllowlist {
    Arc::new(names.iter().map(|name| name.to_string()).collect())
}

fn scope(sink: Arc<dyn EventSink>, allowed_tools: Option<ToolAllowlist>) -> AgentScope {
    AgentScope {
        agent_id: "root".into(),
        parent_id: None,
        depth: 0,
        cancel: CancellationToken::new(),
        sink,
        allowed_tools,
    }
}

/// 等待某个代理的事件出现；超时返回 None（不 panic，便于给出完整现场）。
async fn wait_for(
    events: &Arc<Mutex<Vec<(String, EventMsg)>>>,
    timeout: Duration,
    mut predicate: impl FnMut(&str, &EventMsg) -> bool,
) -> Option<(String, EventMsg)> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(found) = events
            .lock()
            .unwrap()
            .iter()
            .find(|(agent_id, msg)| predicate(agent_id, msg))
            .cloned()
        {
            return Some(found);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[ignore = "SH-054 真机验证需要密钥与外网，按里程碑手动运行"]
async fn real_model_is_blocked_when_it_reaches_outside_the_narrowed_allowlist() {
    let api_key = std::env::var("SHANIU_TEST_API_KEY").expect("需要 SHANIU_TEST_API_KEY");
    let provider = provider();
    println!(
        "[permission-real] 端点 {} / {}（密钥经环境变量传入，不打印）",
        provider.base_url, provider.model
    );

    let mut router = ToolRouter::new();
    shaniu_lib::tools::register_builtin(&mut router, Arc::new(PlanState::new()));
    let workspace = std::env::temp_dir().join("shaniu-permission-real");
    std::fs::create_dir_all(&workspace).expect("创建工作区");

    let engine = Arc::new(AgentLoop {
        backend: Arc::new(ResponsesBackend { provider, api_key }),
        router: Arc::new(router),
        broker: Arc::new(shaniu_lib::permission::ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: workspace,
        max_turns: 6,
        token_budget: 0,
        tree_budget: Default::default(),
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
            approval: ApprovalMode::OnRequest,
            sandbox: SandboxMode::WorkspaceWrite,
        })),
        rollout: None,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: Some("qa-permission-real".into()),
        jobs: None,
        stop_hook: None,
    });
    let broker = engine.broker.clone();

    let registry = Arc::new(SubagentRegistry::new(1));
    let events = Arc::new(Mutex::new(Vec::new()));
    let parent = scope(
        Arc::new(RecorderSink { agent_id: "root".into(), events: events.clone() }),
        None,
    );

    // ===== 场景 1：真实审批挂起 + 审批来源冒泡 + 放行后真实执行 =====
    let task = "请立即调用 unified_exec 工具执行命令 `echo shaniu-perm-one`，然后只回复 DONE。\
                必须使用 unified_exec，不要使用其它工具，也不要只解释。";
    let child_id = registry
        .spawn_with_constraints(
            engine.clone(),
            &parent,
            task,
            None,
            None,
            SpawnNarrowing {
                profile: None,
                allowed_tools: Some(&["read_file".to_string(), "unified_exec".to_string()]),
            },
        )
        .expect("派生应成功");

    let (request_id, origin) = {
        let found = wait_for(&events, Duration::from_secs(180), |agent_id, msg| {
            matches!(
                msg,
                EventMsg::ExecApprovalRequest { tool, .. } if tool.as_str() == "unified_exec" && agent_id == child_id
            )
        })
        .await
        .unwrap_or_else(|| panic!("真实模型没有触发 unified_exec 审批；事件现场：{events:#?}"));
        match found.1 {
            EventMsg::ExecApprovalRequest { request_id, agent_id, task_summary, .. } => {
                (request_id, (agent_id, task_summary))
            }
            _ => unreachable!(),
        }
    };
    let exec_call_id = events
        .lock()
        .unwrap()
        .iter()
        .find_map(|(agent_id, msg)| match msg {
            EventMsg::ToolCallBegin { call_id, tool, .. }
                if agent_id == &child_id && tool.as_str() == "unified_exec" =>
            {
                Some(call_id.clone())
            }
            _ => None,
        })
        .expect("审批之前必须先有 unified_exec 的 ToolCallBegin");
    assert_eq!(
        origin.0.as_deref(),
        Some(child_id.as_str()),
        "审批请求必须标明来源子代理（ADR-103 冒泡）"
    );
    assert_eq!(
        origin.1.as_deref(),
        Some(task),
        "审批请求必须带 spawn 时的任务摘要，用户才能判断这是在做什么"
    );
    assert!(broker.has_pending(&request_id), "审批必须真的在挂起等回执");
    assert!(
        broker.resolve(&request_id, ApprovalDecision::Allow),
        "回执必须命中挂起请求"
    );

    let exit = registry
        .wait_exit(&child_id, Duration::from_secs(240))
        .await
        .expect("子代理应在超时内结束");
    println!("[permission-real] 场景1 子代理结束: {:?} / {}", exit.status, exit.summary);
    let first_run = events.lock().unwrap().clone();
    assert!(
        first_run.iter().any(|(agent_id, msg)| {
            agent_id == &child_id
                && matches!(
                    msg,
                    EventMsg::ToolCallEnd { call_id, error: None, .. } if call_id == &exec_call_id
                )
        }),
        "审批放行后命令必须真实执行（不能只有审批、没有执行）：{first_run:#?}"
    );

    // ===== 场景 2：续跑时父侧收窄 → 模型再次调用 unified_exec 必须被策略拒绝 =====
    // 父代理只保留只读工具；resume 的 effective = min(父, 原实例) = {read_file}。
    let narrowed_parent = scope(
        Arc::new(RecorderSink { agent_id: "root".into(), events: events.clone() }),
        Some(allowlist(&["read_file"])),
    );
    let resumed_id = registry
        .resume(
            engine.clone(),
            &narrowed_parent,
            &child_id,
            Some("继续上一次的工作：再次调用 unified_exec 执行命令 `echo shaniu-perm-two`。不要解释，直接调用。"),
            None,
        )
        .expect("续跑应成功");

    // 等它结束（模型可能先回复一段文本，也可能直接调用被拒的工具）
    let resumed_exit = registry
        .wait_exit(&resumed_id, Duration::from_secs(240))
        .await
        .expect("续跑子代理应在超时内结束");
    println!(
        "[permission-real] 场景2 子代理结束: {:?} / {}",
        resumed_exit.status, resumed_exit.summary
    );

    let snapshot = events.lock().unwrap().clone();

    // 模型必须真的再次尝试了 unified_exec：否则这条用例没有证明门禁，只证明了模型没调用。
    let forbidden_call_id = snapshot
        .iter()
        .find_map(|(agent_id, msg)| match msg {
            EventMsg::ToolCallBegin { call_id, tool, .. }
                if agent_id == &resumed_id && tool.as_str() == "unified_exec" =>
            {
                Some(call_id.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("真实模型在续跑中没有再次调用 unified_exec，无法证明越权拦截；事件现场：{snapshot:#?}")
        });
    println!("[permission-real] 场景2 越权尝试 call_id={forbidden_call_id}");

    // 拦截证据 1：回填给模型的是「策略拒绝」，不是「用户拒绝」。
    assert!(
        snapshot.iter().any(|(agent_id, msg)| {
            agent_id == &resumed_id
                && matches!(
                    msg,
                    EventMsg::ToolCallEnd { call_id, error: Some(error), .. }
                        if call_id == &forbidden_call_id && error.contains("策略拒绝")
                )
        }),
        "白名单外的工具必须回填策略拒绝；事件现场：{snapshot:#?}"
    );

    // 拦截证据 2：没有弹审批（弹了就等于给了模型一次提权申请入口）。
    assert!(
        !snapshot.iter().any(|(agent_id, msg)| {
            agent_id == &resumed_id && matches!(msg, EventMsg::ExecApprovalRequest { .. })
        }),
        "收窄掉的工具不得发起审批请求"
    );

    // 拦截证据 3：执行器根本没有被命中（没有输出增量），也没有成功结束的执行事件。
    assert!(
        !snapshot.iter().any(|(agent_id, msg)| {
            agent_id == &resumed_id
                && matches!(
                    msg,
                    EventMsg::ToolCallOutputDelta { call_id, .. } if call_id == &forbidden_call_id
                )
        }),
        "被拒绝的 unified_exec 不得产生任何命令输出"
    );
    assert!(
        !snapshot.iter().any(|(agent_id, msg)| {
            agent_id == &resumed_id
                && matches!(
                    msg,
                    EventMsg::ToolCallEnd { call_id, error: None, .. } if call_id == &forbidden_call_id
                )
        }),
        "越权调用不得出现成功结束：拒绝必须发生在副作用之前"
    );

    println!("[permission-real] 结论：真实模型越权调用被策略拒绝，未弹审批、未执行命令");
}
