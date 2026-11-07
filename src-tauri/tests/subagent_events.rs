//! SH-053：子代理生命周期事件的行为契约（含 FF-104 事件兼容）。
//!
//! 判据不是「注册表状态字段变了」，而是**出口真的广播了带归属的事件**：前端泳道
//! 只能看见事件，看不见注册表。用真实 `AgentLoop::run_instance` 跑脚本后端，
//! 记录 sink 收到的每一条事件及其 `scoped` 身份。
//!
//! 假绿防护：
//! - 每个用例先断言子代理确实跑到了预期阶段（wait_exit / 流建立），再查事件；
//! - FF-104 用例直接比对旧事件的 JSON 键集合，证明新增变体是 additive。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use shaniu_lib::agent_loop::{AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink, SubagentExitStatus};
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent, Usage};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::subagent_registry::SubagentRegistry;
use shaniu_lib::tool_router::orchestration::AgentScope;
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
struct Recorded {
    agent_id: String,
    parent_id: Option<String>,
    msg: EventMsg,
}

/// 记录型出口：`scoped` 复制同一个收集器但换身份，因此能验证 ADR-104 的归属。
struct RecordingSink {
    log: Arc<Mutex<Vec<Recorded>>>,
    agent_id: String,
    parent_id: Option<String>,
}

impl RecordingSink {
    fn root() -> (Arc<Self>, Arc<Mutex<Vec<Recorded>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(Self { log: log.clone(), agent_id: "root".into(), parent_id: None });
        (sink, log)
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        self.log.lock().unwrap().push(Recorded {
            agent_id: self.agent_id.clone(),
            parent_id: self.parent_id.clone(),
            msg: msg.clone(),
        });
        Ok(())
    }

    fn scoped(&self, agent_id: &str, parent_id: Option<&str>) -> Option<Arc<dyn EventSink>> {
        Some(Arc::new(Self {
            log: self.log.clone(),
            agent_id: agent_id.to_string(),
            parent_id: parent_id.map(str::to_string),
        }))
    }
}

fn recorded(log: &Mutex<Vec<Recorded>>) -> Vec<Recorded> {
    log.lock().unwrap().clone()
}

/// 脚本耗尽即挂起：用来量「运行中」的阶段（steering / 取消）。
struct ScriptedBackend(Mutex<std::collections::VecDeque<Vec<ModelStreamEvent>>>);

#[async_trait::async_trait]
impl ModelBackend for ScriptedBackend {
    async fn stream(
        &self,
        _history: &[HistoryItem],
        _tools: &[ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, String> {
        let script = self.0.lock().unwrap().pop_front();
        match script {
            Some(events) if !events.is_empty() => Ok(Box::pin(futures::stream::iter(events))),
            _ => Ok(Box::pin(futures::stream::pending())),
        }
    }
}

fn engine(turns: Vec<Vec<ModelStreamEvent>>) -> Arc<AgentLoop> {
    Arc::new(AgentLoop {
        backend: Arc::new(ScriptedBackend(Mutex::new(turns.into()))),
        router: Arc::new(ToolRouter::new()),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: std::env::temp_dir(),
        max_turns: 4,
        token_budget: 0,
        tree_budget: Default::default(),
        policy: policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite },
        rollout: None,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
    })
}

fn completed_turn(text: &str) -> Vec<ModelStreamEvent> {
    vec![
        ModelStreamEvent::OutputTextDelta { delta: text.into() },
        ModelStreamEvent::Completed { usage: Usage { input_tokens: 11, cached_input_tokens: Some(3), output_tokens: 7 } },
    ]
}

fn root_scope(cancel: CancellationToken, sink: Arc<dyn EventSink>) -> AgentScope {
    AgentScope { agent_id: "root".into(), parent_id: None, depth: 0, cancel, sink, allowed_tools: None }
}

/// 上游 names.ts 的 NAME_PATTERN（catalog v3 的 name 必须合法，`lead` 为根保留）。
fn is_lane_name(value: &str) -> bool {
    let mut chars = value.chars();
    let first = chars.next();
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    value.len() <= 32
        && first.is_some_and(|c| c.is_ascii_lowercase())
        && rest_ok
        && value != "lead"
}

/// 派生 → 跑完 → 收口：两个生命周期事件都必须带子代理归属与泳道元数据。
#[tokio::test]
async fn spawn_and_exit_broadcast_lane_metadata_under_the_child_identity() {
    let (sink, log) = RecordingSink::root();
    let registry = Arc::new(SubagentRegistry::new(4));
    let task = "探索仓库的 transport 边界\n第二行背景说明";
    let id = registry
        .spawn(engine(vec![completed_turn("子任务结论")]), &root_scope(CancellationToken::new(), sink), task, None)
        .expect("派生子代理");
    let exit = registry.wait_exit(&id, Duration::from_secs(5)).await.expect("子代理应跑完");
    assert_eq!(exit.summary, "子任务结论", "先证明子代理真的产出了结果，再查事件");

    let events = recorded(&log);
    let spawned = events
        .iter()
        .find_map(|r| match &r.msg {
            EventMsg::SubagentSpawned { child_id, task, name, label, created_at_ms } => {
                Some((r, child_id, task, name, label, *created_at_ms))
            }
            _ => None,
        })
        .expect("派生必须广播 subagent_spawned，否则前端泳道只能懒建成 v1 unknown");
    let (record, child_id, event_task, name, label, created_at_ms) = spawned;
    assert_eq!(child_id, &id);
    assert_eq!(event_task, task, "task 必须是全文：泳道首条 user 消息与审批摘要都靠它");
    assert!(is_lane_name(name), "catalog v3 的 name 必须过上游 NAME_PATTERN，得到 {name:?}");
    assert_eq!(label, "探索仓库的 transport 边界", "label 取 task 首行（上游 catalog v3 的展示标签）");
    assert!(created_at_ms > 1_700_000_000_000, "childCreatedAt 必须是墙钟毫秒，得到 {created_at_ms}");
    assert_eq!(record.agent_id, id, "生命周期事件归属子代理自己（ADR-104）");
    assert_eq!(record.parent_id.as_deref(), Some("root"));

    let exited = events
        .iter()
        .find_map(|r| match &r.msg {
            EventMsg::SubagentExited { child_id, status, summary } => Some((r, child_id, *status, summary)),
            _ => None,
        })
        .expect("收口必须广播 subagent_exited，否则父会话看不到结算通知、泳道徽标不收口");
    let (record, child_id, status, summary) = exited;
    assert_eq!(child_id, &id);
    assert_eq!(status, SubagentExitStatus::Done);
    assert_eq!(summary, "子任务结论");
    assert_eq!(record.agent_id, id);

    let spawn_at = events.iter().position(|r| matches!(r.msg, EventMsg::SubagentSpawned { .. })).unwrap();
    let exit_at = events.iter().position(|r| matches!(r.msg, EventMsg::SubagentExited { .. })).unwrap();
    assert!(spawn_at < exit_at, "spawned 必须先于 exited，前端按序折叠 catalog");
}

/// 兄弟泳道名必须互不相同：上游 catalog 以 name 作为模型可寻址标识，重名会串树。
#[tokio::test]
async fn sibling_lane_names_are_unique() {
    let (sink, log) = RecordingSink::root();
    let registry = Arc::new(SubagentRegistry::new(4));
    let scope = root_scope(CancellationToken::new(), sink);
    let engine = engine(vec![completed_turn("A"), completed_turn("B"), completed_turn("C")]);
    let mut ids = Vec::new();
    for task in ["任务 A", "任务 B", "任务 C"] {
        ids.push(registry.spawn(engine.clone(), &scope, task, None).expect("派生"));
    }
    for id in &ids {
        registry.wait_exit(id, Duration::from_secs(5)).await.expect("应跑完");
    }

    let names: Vec<String> = recorded(&log)
        .iter()
        .filter_map(|r| match &r.msg {
            EventMsg::SubagentSpawned { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 3, "三个子代理各发一条 spawned");
    for name in &names {
        assert!(is_lane_name(name), "非法泳道名 {name:?}");
    }
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "兄弟泳道名重复：{names:?}");
}

/// steering 注入必须在**子泳道**里可见（上游落为子会话 user/message，source=agent-message）。
#[tokio::test]
async fn steering_is_broadcast_into_the_child_lane() {
    let (sink, log) = RecordingSink::root();
    let registry = Arc::new(SubagentRegistry::new(2));
    let cancel = CancellationToken::new();
    let id = registry
        .spawn(engine(vec![]), &root_scope(cancel.clone(), sink), "长任务", None)
        .expect("派生");
    registry.send_message(&id, "请改看 src/transport", None).expect("运行中可注入");

    let events = recorded(&log);
    let message = events
        .iter()
        .find_map(|r| match &r.msg {
            EventMsg::SubagentMessage { child_id, sender_id, text, .. } => Some((r, child_id, sender_id, text)),
            _ => None,
        })
        .expect("注入必须广播 subagent_message，否则泳道里看不到 steering");
    let (record, child_id, sender_id, text) = message;
    assert_eq!(child_id, &id);
    assert_eq!(sender_id, "root");
    assert_eq!(text, "请改看 src/transport");
    assert_eq!(record.agent_id, id, "steering 事件归子泳道，不是父会话");

    cancel.cancel();
    let exit = registry.wait_exit(&id, Duration::from_secs(5)).await.expect("取消后应结束");
    assert_eq!(exit.status, shaniu_lib::subagent_registry::ExitStatus::Cancelled);
    let cancelled = recorded(&log)
        .iter()
        .filter_map(|r| match &r.msg {
            EventMsg::SubagentExited { status, .. } => Some(*status),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cancelled, vec![SubagentExitStatus::Cancelled], "取消也要发 exited，泳道才不会永久转圈");
}

/// 已结束的泳道拒绝注入，且不得伪造一条 steering 事件。
#[tokio::test]
async fn steering_to_an_exited_child_is_rejected_without_an_event() {
    let (sink, log) = RecordingSink::root();
    let registry = Arc::new(SubagentRegistry::new(2));
    let id = registry
        .spawn(engine(vec![completed_turn("结论")]), &root_scope(CancellationToken::new(), sink), "任务", None)
        .expect("派生");
    registry.wait_exit(&id, Duration::from_secs(5)).await.expect("应跑完");
    let before = recorded(&log).len();

    assert!(registry.send_message(&id, "迟到的引导", None).is_err(), "已结束子代理必须 fail-closed");
    assert_eq!(recorded(&log).len(), before, "拒绝路径不得广播任何事件");
}

/// SH-053 批 3：steering 的拒绝理由必须是**类型**，命令面才能映射成上游错误码
/// （`subagent/not-resumable` / `subagent/delivery-unavailable`），不靠文案匹配。
#[tokio::test]
async fn steering_rejections_are_typed_for_the_command_face() {
    use shaniu_lib::subagent_registry::SteerError;

    let (sink, _log) = RecordingSink::root();
    let registry = Arc::new(SubagentRegistry::new(2));
    assert_eq!(
        registry.send_message("01UNKNOWN", "引导", None),
        Err(SteerError::UnknownAgent { agent_id: "01UNKNOWN".into() }),
        "不在注册表里的 id 必须归 UnknownAgent（泳道已随进程消失）"
    );

    let id = registry
        .spawn(engine(vec![completed_turn("结论")]), &root_scope(CancellationToken::new(), sink), "任务", None)
        .expect("派生");
    registry.wait_exit(&id, Duration::from_secs(5)).await.expect("应跑完");
    assert_eq!(
        registry.send_message(&id, "迟到的引导", None),
        Err(SteerError::Exited { agent_id: id.clone() }),
        "已结束的子代理必须归 Exited（前端据此回 not-resumable，而不是假装受理）"
    );
}

/// FF-104：新增变体是 additive——旧事件的载荷键集合一个都不能多/少。
#[tokio::test]
async fn existing_event_payloads_keep_their_v1_shape() {
    let (sink, log) = RecordingSink::root();
    let registry = Arc::new(SubagentRegistry::new(2));
    let id = registry
        .spawn(engine(vec![completed_turn("结论")]), &root_scope(CancellationToken::new(), sink), "任务", None)
        .expect("派生");
    registry.wait_exit(&id, Duration::from_secs(5)).await.expect("应跑完");

    let events = recorded(&log);
    let keys_of = |msg: &EventMsg| -> std::collections::BTreeSet<String> {
        serde_json::to_value(msg).unwrap().as_object().unwrap().keys().cloned().collect()
    };
    let turn_started = events.iter().find(|r| matches!(r.msg, EventMsg::TurnStarted { .. })).expect("应有 turn_started");
    assert_eq!(
        keys_of(&turn_started.msg),
        ["type", "turn"].into_iter().map(str::to_string).collect(),
        "旧事件载荷被新协议污染"
    );

    let turn_completed = events.iter().find(|r| matches!(r.msg, EventMsg::TurnCompleted { .. })).expect("应有 turn_completed");
    assert_eq!(
        keys_of(&turn_completed.msg),
        ["type", "turn", "reason"].into_iter().map(str::to_string).collect(),
        "旧事件载荷被新协议污染"
    );

    // 旧消费方（不认识三个新 tag）仍能把整条流解析完：未知 tag 不是解析错误。
    for record in &events {
        let json = serde_json::to_string(&record.msg).unwrap();
        let _: serde_json::Value = serde_json::from_str(&json).expect("事件必须可无损序列化");
    }
}