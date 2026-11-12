//! 提示词入轨迹的端到端判据（spec-sysprompt A1–A3 的 Rust 侧）。
//!
//! 核心口径只有一条：**记录点在实际下发路径上**。所以这里断言的是
//! `记录文本 == redact(传给 model_client 的 instructions)`，
//! 而不是拿 `assemble()` 重算一遍去比——后者只能证明"组装是确定的"，
//! 证明不了"记录的是实际发出去的那份"。

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use shaniu_lib::agent_loop::{AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{AgentEvent, ApprovalDecision, EventMsg, EventSink};
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent, ResponseItem, Usage};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::rollout::{self, ConfigSnapshot, RolloutItem, RolloutWriter};
use shaniu_lib::tool_router::{PlanState, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolRouter, ToolSpec};
use tokio_util::sync::CancellationToken;

fn temp_dir(tag: &str) -> shaniu_lib::test_support::TestTempDir {
    shaniu_lib::test_support::TestTempDir::new(&format!("sysprompt-{tag}"))
}

/// 记录每次请求实际收到的 `instructions`，并按脚本吐事件。
///
/// 脚本用尽后只出文本，保证循环必然收口，不会把测试挂在无限循环上。
struct Capturing {
    scripts: Mutex<Vec<Vec<ModelStreamEvent>>>,
    instructions: Mutex<Vec<String>>,
}

impl Capturing {
    fn new(scripts: Vec<Vec<ModelStreamEvent>>) -> Self {
        Self { scripts: Mutex::new(scripts), instructions: Mutex::new(Vec::new()) }
    }

    fn sent(&self) -> Vec<String> {
        self.instructions.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelBackend for Capturing {
    async fn stream(
        &self,
        _history: &[HistoryItem],
        _tools: &[ToolDeclarationShim],
        instructions: &str,
    ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
        self.instructions.lock().unwrap().push(instructions.to_string());
        let mut queue = self.scripts.lock().unwrap();
        let script = if queue.is_empty() {
            vec![ModelStreamEvent::OutputTextDelta { delta: "收尾".into() }]
        } else {
            queue.remove(0)
        };
        Ok(Box::pin(futures::stream::iter(script)))
    }
}

/// 命中即回显的工具，命名 read_file → 门禁自动放行，不需要人工审批。
struct EchoTool {
    spec: ToolSpec,
}

impl Default for EchoTool {
    fn default() -> Self {
        Self { spec: ToolSpec::new("read_file", "回显", serde_json::json!({"type":"object"})) }
    }
}

#[async_trait]
impl ToolExecutor for EchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(format!("echo:{args}")))
    }
}

/// 收集事件；遇到审批请求立刻代答（与生产审批卡等价），保证阻塞门不会挂住。
struct Recording {
    events: Mutex<Vec<EventMsg>>,
    broker: Arc<ApprovalBroker>,
}

impl Recording {
    fn new(broker: Arc<ApprovalBroker>) -> Self {
        Self { events: Mutex::new(Vec::new()), broker }
    }

    fn prompts(&self) -> Vec<(String, String)> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|msg| match msg {
                EventMsg::SystemPrompt { text, sha256, .. } => Some((text.clone(), sha256.clone())),
                _ => None,
            })
            .collect()
    }
}

impl EventSink for Recording {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        if let EventMsg::ExecApprovalRequest { request_id, .. } = msg {
            self.broker.resolve(request_id, ApprovalDecision::AllowAlways);
        }
        self.events.lock().unwrap().push(msg.clone());
        Ok(())
    }
}

fn meta(agent: &str, ws: &Path) -> RolloutItem {
    RolloutItem::SessionMeta {
        session_id: agent.into(),
        agent_id: agent.into(),
        parent_id: None,
        created_at: 1_700_000_000_000,
        config_snapshot: ConfigSnapshot {
            model: "m".into(),
            sandbox: "WorkspaceWrite".into(),
            approval: "OnRequest".into(),
            workspace_root: ws.display().to_string(),
            ..Default::default()
        },
    }
}

fn loop_with(
    ws: &Path,
    backend: Arc<Capturing>,
    writer: Arc<RolloutWriter>,
    sandbox: SandboxMode,
    max_turns: u32,
) -> (AgentLoop, Arc<ApprovalBroker>) {
    let mut router = ToolRouter::new();
    router.register(Arc::new(EchoTool::default()));
    let broker = Arc::new(ApprovalBroker::new());
    let agent = AgentLoop {
        backend,
        router: Arc::new(router),
        broker: broker.clone(),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: ws.to_path_buf(),
        max_turns,
        token_budget: 0,
        tree_budget: Default::default(),
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
            approval: ApprovalMode::OnRequest,
            sandbox,
        })),
        rollout: Some(writer),
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
        retry: shaniu_lib::model_client::RetryPolicy::disabled(),
        verify: Default::default(),
    };
    (agent, broker)
}

fn prompt_records(path: &Path) -> Vec<(String, String, u64, u32)> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| {
            if !line.contains("\"type\":\"system_prompt\"") {
                return None;
            }
            match serde_json::from_str::<RolloutItem>(line) {
                Ok(RolloutItem::SystemPrompt { text, sha256, at_ms, turn }) => {
                    Some((text, sha256, at_ms, turn))
                }
                other => panic!("system_prompt 行无法解析：{other:?}"),
            }
        })
        .collect()
}

/// A1：一次 run 只记一条，且就是实际下发的那份；续跑一轮再记一条。
#[tokio::test]
async fn one_record_per_run_from_the_actual_instructions() {
    let dir = temp_dir("once");
    let rollout_dir = dir.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();
    let path = rollout_dir.join("a1.jsonl");

    let backend = Arc::new(Capturing::new(vec![
        vec![
            ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
                call_id: "c1".into(),
            }),
            ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 3, cached_input_tokens: None, output_tokens: 1 },
            },
        ],
        vec![ModelStreamEvent::OutputTextDelta { delta: "做完了".into() }],
    ]));
    let writer = Arc::new(RolloutWriter::create(&rollout_dir, &meta("a1", &dir)).unwrap());
    {
        let (agent, broker) = loop_with(&dir, backend.clone(), writer, SandboxMode::WorkspaceWrite, 5);
        let sink = Arc::new(Recording::new(broker));
        let summary = agent.run_task("任务", sink.clone(), CancellationToken::new()).await;
        assert!(summary.turns >= 2, "带工具调用的任务至少两轮：{:?}", summary);
        assert_eq!(sink.prompts().len(), 1, "同一 run 内提示词不变 → 只能记一次");
    }

    let records = prompt_records(&path);
    assert_eq!(records.len(), 1, "多轮任务只能落一条系统提示词");

    let sent = backend.sent();
    assert!(!sent.is_empty(), "后端必须收到过系统提示");
    let (text, sha256, _at, turn) = &records[0];
    assert_eq!(*turn, 1, "首次记录发生在第 1 轮");
    assert_eq!(
        text,
        &shaniu_lib::config::redact(&sent[0]),
        "记录的必须是实际下发的那份（经统一脱敏后的落盘形态）"
    );
    assert_eq!(
        sha256,
        &rollout::sha256_hex(text),
        "指纹必须与落盘文本自洽，否则 resume 会把正常档案判成损坏"
    );

    // 续跑：新 run 重新组装 → 新记录（同一会话内第二次下发）。
    let writer = Arc::new(RolloutWriter::open_existing(&path).unwrap());
    let backend2 = Arc::new(Capturing::new(vec![vec![ModelStreamEvent::OutputTextDelta {
        delta: "继续".into(),
    }]]));
    let (agent, broker) = loop_with(&dir, backend2, writer, SandboxMode::WorkspaceWrite, 5);
    agent
        .run_task("第二轮任务", Arc::new(Recording::new(broker)), CancellationToken::new())
        .await;

    let records = prompt_records(&path);
    assert_eq!(records.len(), 2, "两个 run 各记一条：{:?}", records);
    assert_eq!(records[1].3, 1, "续跑的第 1 轮再次记录");
}

/// 体积判据：二十轮无变化，档案里仍然只有一条系统提示词。
#[tokio::test]
async fn twenty_unchanged_turns_do_not_grow_the_archive_linearly() {
    let dir = temp_dir("volume");
    let rollout_dir = dir.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();
    let path = rollout_dir.join("vol.jsonl");

    // 每轮都派一个工具调用，强制跑满 20 轮。
    let scripts: Vec<Vec<ModelStreamEvent>> = (0..20)
        .map(|i| {
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "read_file".into(),
                arguments: format!("{{\"i\":{i}}}"),
                call_id: format!("c{i}"),
            })]
        })
        .collect();
    let backend = Arc::new(Capturing::new(scripts));
    let writer = Arc::new(RolloutWriter::create(&rollout_dir, &meta("vol", &dir)).unwrap());
    let (agent, broker) = loop_with(&dir, backend, writer, SandboxMode::WorkspaceWrite, 20);
    let sink = Arc::new(Recording::new(broker));
    let summary = agent.run_task("长任务", sink.clone(), CancellationToken::new()).await;

    assert_eq!(summary.turns, 20, "应跑满 20 轮");
    assert_eq!(sink.prompts().len(), 1, "事件同样只发一次");
    assert_eq!(
        prompt_records(&path).len(),
        1,
        "提示词未变时档案体积不得随轮数线性增长"
    );
}

/// A4（Rust 侧）：提示词变化 → 两个不同版本；事件按 run 计数，不按轮计数。
#[tokio::test]
async fn changed_prompt_produces_a_second_version() {
    let dir = temp_dir("versions");
    let rollout_dir = dir.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();
    let path = rollout_dir.join("ver.jsonl");

    // 第一次：WorkspaceWrite
    let backend = Arc::new(Capturing::new(vec![vec![ModelStreamEvent::OutputTextDelta {
        delta: "一".into(),
    }]]));
    let writer = Arc::new(RolloutWriter::create(&rollout_dir, &meta("ver", &dir)).unwrap());
    let (agent, broker) = loop_with(&dir, backend, writer, SandboxMode::WorkspaceWrite, 3);
    agent
        .run_task("第一轮", Arc::new(Recording::new(broker)), CancellationToken::new())
        .await;

    // 第二次：ReadOnly（换档位 → 组装出的提示词不同）
    let writer = Arc::new(RolloutWriter::open_existing(&path).unwrap());
    let backend = Arc::new(Capturing::new(vec![vec![ModelStreamEvent::OutputTextDelta {
        delta: "二".into(),
    }]]));
    let (agent, broker) = loop_with(&dir, backend, writer, SandboxMode::ReadOnly, 3);
    let sink = Arc::new(Recording::new(broker));
    agent.run_task("第二轮", sink, CancellationToken::new()).await;

    let records = prompt_records(&path);
    assert_eq!(records.len(), 2, "换档位产生的提示词是合法的新版本");
    assert_ne!(records[0].1, records[1].1, "两版指纹必须不同（否则去重把合法变化吞了）");
    // 反向对照：同一次 run 内若每轮都发事件，这里会 > 1。
    assert_eq!(prompt_records(&path).len(), 2, "每个 run 恰好一条，不随轮数增长");
}

/// 降级判据：档案写不进去时，对话必须正常收口，只留一条可见的「轨迹可能不完整」。
///
/// 用独占文件锁模拟"目录/文件不可写"：`File::lock` 在 Windows 上走 `LockFileEx`，
/// 另一句柄对锁定区间的写入会失败，效果等同磁盘满/ACL 拒绝，但无需改 ACL。
/// Unix 的 flock 是建议锁、不阻塞写入，所以这里只跑 Windows（本项目交付平台）。
#[cfg(windows)]
#[tokio::test]
async fn archive_write_failure_degrades_instead_of_failing_the_turn() {
    use shaniu_lib::events::TurnEndReason;

    let dir = temp_dir("degrade");
    let rollout_dir = dir.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();
    let path = rollout_dir.join("deg.jsonl");

    let writer = Arc::new(RolloutWriter::create(&rollout_dir, &meta("deg", &dir)).unwrap());
    // 建档已完成（meta 已落盘），再上锁：之后的 system_prompt / 历史 / TurnEnd 写入都会失败。
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    lock.lock().expect("无法锁定档案文件");

    let backend = Arc::new(Capturing::new(vec![vec![ModelStreamEvent::OutputTextDelta {
        delta: "档案写不进去也要把话说完".into(),
    }]]));
    let (agent, broker) = loop_with(&dir, backend, writer, SandboxMode::WorkspaceWrite, 3);
    let sink = Arc::new(Recording::new(broker));
    let summary = agent.run_task("任务", sink.clone(), CancellationToken::new()).await;

    let events = sink.events.lock().unwrap().clone();
    // 反向对照：改回「写失败即中止」，下面两条必红（TurnCompleted 会变成 failed）。
    assert!(
        matches!(
            events.iter().find_map(|m| match m {
                EventMsg::TurnCompleted { reason, .. } => Some(*reason),
                _ => None,
            }),
            Some(TurnEndReason::Completed)
        ),
        "档案不可写不得把本轮判失败：{events:?}"
    );
    assert_eq!(summary.turns, 1, "对话应正常跑完这一轮");

    let degraded: Vec<&String> = events
        .iter()
        .filter_map(|m| match m {
            EventMsg::RolloutDegraded { message } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(degraded.len(), 1, "降级通知必须只发一次，不随重试刷屏");
    assert!(
        degraded[0].contains("轨迹可能不完整"),
        "通知必须让用户知道轨迹不完整：{}",
        degraded[0]
    );

    // 释放锁后，下一轮应能自然补齐（游标停在失败条目上，不在失败时丢历史）。
    drop(lock);
    let backend = Arc::new(Capturing::new(vec![vec![ModelStreamEvent::OutputTextDelta {
        delta: "恢复".into(),
    }]]));
    let writer = Arc::new(RolloutWriter::open_existing(&path).unwrap());
    let (agent, broker) = loop_with(&dir, backend, writer, SandboxMode::WorkspaceWrite, 3);
    agent
        .run_task("恢复后继续", Arc::new(Recording::new(broker)), CancellationToken::new())
        .await;
    assert!(
        !prompt_records(&path).is_empty(),
        "锁释放后系统提示词应补写进档案（降级不是丢弃）"
    );
}

#[test]
fn journal_redacts_system_prompt_like_every_other_event() {
    let dir = temp_dir("redact");
    let secret = "sk-abcdefgh12345678";
    let event = AgentEvent {
        event_id: ulid::Ulid::generate().to_string(),
        session_id: "session-a".into(),
        agent_id: "root".into(),
        parent_id: None,
        ts_ms: 1,
        msg: EventMsg::SystemPrompt {
            text: format!("工作区说明\n密钥={secret}\n"),
            sha256: "deadbeef".into(),
            at_ms: 1,
            turn: 1,
            step: 0,
        },
    };

    // 反向对照：不经 encode 的原始序列化里明文存在——证明下面的断言不是空转。
    let raw = serde_json::to_string(&event).unwrap();
    assert!(raw.contains(secret), "原始序列化应含明文，否则脱敏断言测不到东西");

    shaniu_lib::event_journal::append(&dir, &event).expect("journal 写入失败");
    let written = std::fs::read_to_string(dir.join(format!(
        "session-a{}",
        shaniu_lib::event_journal::EVENT_LOG_SUFFIX
    )))
    .unwrap();
    assert!(!written.contains(secret), "journal 落盘后不得出现明文密钥");
    assert!(written.contains("[REDACTED]"), "应留下脱敏占位符：{written}");

    // 边界：只覆盖已登记密钥与已知形态；未登记凭据不在保证范围内（见 tasks.md D1）。
}
