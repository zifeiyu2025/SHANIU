//! SH-052 真机端到端（需要密钥与外网，按里程碑手动跑）：
//! root 用真实模型派生两个子代理并行作答，再汇总。
//!
//! 判据不是"没报错"，而是：
//! 1. 注册表里确实出现两个**不同**的 agent_id，且都 Done；
//! 2. 两个子代理的 ExitSummary 里带各自真实产出的数字（证明子代理真的跑了模型）；
//! 3. 捕获到的事件里，子代理的事件归属到自己的 agent_id，而不是伪装成 root。
//!
//! 运行：
//! `SHANIU_TEST_API_KEY=... cargo test --test orchestrate_real -- --ignored --nocapture`

use std::sync::{Arc, Mutex};

use shaniu_lib::agent_loop::{AgentInstance, AgentLoop, ResponsesBackend};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink};
use shaniu_lib::model_client::provider::ProviderConfig;
use shaniu_lib::model_client::{HistoryItem, Role};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::subagent_registry::orchestrator::RegistryOrchestrator;
use shaniu_lib::subagent_registry::{AgentStatus, SubagentRegistry};
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use tokio_util::sync::CancellationToken;

/// 记录事件并保留归属身份；`scoped` 返回带新身份的出口（验证 ADR-104 接线）。
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

#[tokio::test]
#[ignore = "SH-052 真机端到端需要密钥与外网，按里程碑手动运行"]
async fn root_spawns_two_real_children_and_collects_their_answers() {
    let api_key = std::env::var("SHANIU_TEST_API_KEY").expect("需要 SHANIU_TEST_API_KEY");
    let provider = ProviderConfig {
        provider: std::env::var("SHANIU_TEST_PROVIDER").unwrap_or_else(|_| "openai-chat".into()),
        base_url: std::env::var("SHANIU_TEST_BASE_URL")
            .unwrap_or_else(|_| "https://api.longcat.chat/openai/v1".into()),
        model: std::env::var("SHANIU_TEST_MODEL").unwrap_or_else(|_| "LongCat-2.5".into()),
        reasoning_effort: None,
        headers: Default::default(),
    };
    println!("[orchestrate-e2e] 端点 {} {}", provider.base_url, provider.model);

    let mut router = ToolRouter::new();
    shaniu_lib::tools::register_builtin(&mut router, Arc::new(PlanState::new()), true);
    let engine = Arc::new(AgentLoop {
        backend: Arc::new(ResponsesBackend { provider, api_key }),
        router: Arc::new(router),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 10,
        token_budget: 0,
        tree_budget: Default::default(),
        // spawn_agent 属写入档，无人值守的测试进程无法应答审批弹窗；这里用完全信任沙箱，
        // 验证目标在于编排链路本身，权限门控另有 FF-102 覆盖。
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
            approval: ApprovalMode::OnRequest,
            sandbox: SandboxMode::DangerFullAccess,
        })),
        rollout: None,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: Some("qa-orchestrate".into()),
        jobs: None,
        stop_hook: None,
        retry: shaniu_lib::model_client::RetryPolicy::disabled(),
        verify: Default::default(),
    });

    let registry = Arc::new(SubagentRegistry::new(2));
    let orchestrator: Arc<dyn shaniu_lib::tool_router::orchestration::AgentOrchestrator> =
        Arc::new(RegistryOrchestrator::new(registry.clone(), engine.clone()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink: Arc<dyn EventSink> = Arc::new(RecorderSink {
        agent_id: "root".into(),
        events: events.clone(),
    });

    let mut root = AgentInstance::root(
        vec![HistoryItem::Message {
            role: Role::User,
            text: "你必须按顺序完成，不要跳步：\n\
                   1) 调用 spawn_agent 两次。第一次 task 写「只回答一个数字：17」，第二次 task 写「只回答一个数字：42」。\n\
                   2) 调用 wait_agent 等待这两个子代理结束。\n\
                   3) 用一句话汇总两个子代理各自回答的数字。\n\
                   不要自己编数字，必须来自子代理结果。"
                .into(),
            reasoning: None,
        }],
        0,
        CancellationToken::new(),
    );

    let summary = engine
        .run_instance_with_orchestrator(&mut root, sink, Some(orchestrator))
        .await;
    println!("[orchestrate-e2e] root 结束: {:?}", summary.reason);
    println!("[orchestrate-e2e] root 最后文本: {:?}", root.last_assistant_text());

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 2, "必须真的派生两个子代理，实际: {snapshot:?}");
    let ids: Vec<String> = snapshot.iter().map(|entry| entry.agent_id.clone()).collect();
    assert_ne!(ids[0], ids[1], "两个子代理必须有不同身份");
    for entry in &snapshot {
        assert_eq!(entry.status, AgentStatus::Exited, "子代理 {} 应已结束", entry.agent_id);
        let exit = entry.exit.as_ref().expect("结束必须有 ExitSummary");
        println!("[orchestrate-e2e] 子代理 {} => {:?} / {}", entry.agent_id, exit.status, exit.summary);
        assert!(exit.summary.contains("17") || exit.summary.contains("42"), "子代理应给出真实数字: {}", exit.summary);
    }

    let events = events.lock().unwrap();
    for id in &ids {
        assert!(
            events.iter().any(|(agent_id, _)| agent_id == id),
            "子代理 {id} 的事件必须带自己的 agent_id；实际归属: {:?}",
            events.iter().map(|(agent_id, _)| agent_id.as_str()).collect::<std::collections::BTreeSet<_>>()
        );
    }
    // 防止事件把所有子代理都挂成 root（SH-053 前端泳道分离的前提）。
    assert!(
        events.iter().any(|(agent_id, _)| agent_id == "root"),
        "root 自己的事件必须仍在"
    );

    // SH-053：真实模型派生出的每个子代理都必须广播泳道元数据与收口事件，
    // 否则前端只能懒建成 v1 unknown（无名字、无结算通知）。
    let mut lane_names = Vec::new();
    for id in &ids {
        let spawned = events
            .iter()
            .find_map(|(agent_id, msg)| match msg {
                EventMsg::SubagentSpawned { child_id, name, label, created_at_ms, .. } if child_id == id => {
                    Some((agent_id.clone(), name.clone(), label.clone(), *created_at_ms))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("子代理 {id} 缺少 subagent_spawned 事件"));
        assert_eq!(spawned.0, *id, "spawned 必须归属子代理自己");
        assert!(is_lane_name(&spawned.1), "非法泳道名 {:?}", spawned.1);
        assert!(!spawned.2.is_empty(), "label 不能为空（catalog v3 必填）");
        assert!(spawned.3 > 1_700_000_000_000, "created_at_ms 必须是墙钟毫秒");
        lane_names.push(spawned.1.clone());

        let exited = events
            .iter()
            .find_map(|(agent_id, msg)| match msg {
                EventMsg::SubagentExited { child_id, status, summary } if child_id == id => {
                    Some((agent_id.clone(), *status, summary.clone()))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("子代理 {id} 缺少 subagent_exited 事件"));
        assert_eq!(exited.0, *id);
        assert_eq!(exited.1, shaniu_lib::events::SubagentExitStatus::Done);
        assert!(exited.2.contains("17") || exited.2.contains("42"), "结算摘要应带真实产出: {}", exited.2);
        println!("[orchestrate-e2e] 泳道 {} name={} label={}", id, spawned.1, spawned.2);
    }
    lane_names.sort();
    lane_names.dedup();
    assert_eq!(lane_names.len(), 2, "两个真实子代理的泳道名必须互不相同");
}

/// 上游 names.ts 的 NAME_PATTERN（`lead` 为根代理保留）。
fn is_lane_name(value: &str) -> bool {
    let mut chars = value.chars();
    let first = chars.next();
    value.len() <= 32
        && first.is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && value != "lead"
}
