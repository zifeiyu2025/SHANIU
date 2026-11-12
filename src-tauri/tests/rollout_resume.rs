//! SH-031 端到端：进程重启后从档案续跑（docs/04 §3.1）。
//!
//! 这里刻意模拟"重启"而不是在同一作用域里连着调两次：把 agent/writer/sink 全部 drop 掉，
//! 只留磁盘上的 JSONL，再靠 `resume` + `open_existing` 把会话捡回来。
//! 判据三条，缺一不可：
//! 1. 档案**只有一条 session_meta**——续跑走的是追加，不是重新建档；
//! 2. 重放出的历史里，工具调用与输出**成对存在**（SH-082 的教训：不成对就 400）；
//! 3. 续跑新增的条目落在旧条目之后，且旧条目**没有被重写一遍**（`persisted` 参数的作用）。

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use shaniu_lib::agent_loop::{AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink};
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent, ResponseItem, Role, Usage};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::rollout::{self, resume, ConfigSnapshot, RolloutItem, RolloutWriter};
use shaniu_lib::tool_router::{PlanState, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolRouter, ToolSpec};
use tokio_util::sync::CancellationToken;

fn temp_dir(tag: &str) -> shaniu_lib::test_support::TestTempDir {
    shaniu_lib::test_support::TestTempDir::new(&format!("resume-e2e-{tag}"))
}

/// 按脚本轮次吐事件的桩后端；脚本用尽后只出文本，保证循环必然收口。
struct Scripted(Mutex<Vec<Vec<ModelStreamEvent>>>);

#[async_trait]
impl ModelBackend for Scripted {
    async fn stream(
        &self,
        _history: &[HistoryItem],
        _tools: &[ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
        let mut queue = self.0.lock().unwrap();
        // 脚本用尽后只出文本：保证任何续跑都必然收口，不会把测试挂在无限循环上
        let script = if queue.is_empty() {
            vec![ModelStreamEvent::OutputTextDelta { delta: "收尾".into() }]
        } else {
            queue.remove(0)
        };
        Ok(Box::pin(futures::stream::iter(script)))
    }
}

/// 命中即回显的工具，命名 read_file → 风险 read → 门禁自动放行，不需要人工审批。
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
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(format!("echo:{args}")))
    }
}

struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
        Ok(())
    }
}

/// 真机用例必须代答审批：`AutoReadOnly` 下写入/执行会发 `exec_approval_request`，
/// 而审批门是**阻塞等待**的——用 NullSink 不回执，整轮就永远卡在那里（本机第一次跑就是这样挂住的）。
/// 生产里回执来自审批卡 UI，这里等价于"用户全程点总是允许"。
struct AutoApprove(Arc<ApprovalBroker>);

impl EventSink for AutoApprove {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        if let EventMsg::ExecApprovalRequest { request_id, .. } = msg {
            self.0.resolve(request_id, shaniu_lib::events::ApprovalDecision::AllowAlways);
        }
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
            approval: "AutoReadOnly".into(),
            workspace_root: ws.display().to_string(),
            ..Default::default()
        },
    }
}

fn loop_with(agent_ws: &Path, backend: Arc<Scripted>, writer: Arc<RolloutWriter>) -> AgentLoop {
    let mut router = ToolRouter::new();
    router.register(Arc::new(EchoTool::default()));
    AgentLoop {
        backend,
        router: Arc::new(router),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: agent_ws.to_path_buf(),
        max_turns: 3,
        token_budget: 0,
        tree_budget: Default::default(),
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite })),
        rollout: Some(writer),
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
        retry: shaniu_lib::model_client::RetryPolicy::disabled(),
        verify: Default::default(),
    }
}

/// 结构断言：每条带 tool_calls 的 assistant 之后紧跟等量且 id 对应的 tool 消息。
fn assert_tool_calls_answered(messages: &[Value]) {
    let mut i = 0;
    while i < messages.len() {
        let Some(calls) = messages[i]["tool_calls"].as_array() else { i += 1; continue };
        let ids: Vec<&str> = calls.iter().map(|c| c["id"].as_str().unwrap()).collect();
        for (n, id) in ids.iter().enumerate() {
            let answer = &messages[i + 1 + n];
            assert_eq!(answer["role"], "tool", "tool_call {id} 之后缺应答消息");
            assert_eq!(answer["tool_call_id"], *id, "应答顺序与 tool_calls 不对应");
        }
        i += 1 + ids.len();
    }
}

#[tokio::test]
async fn restart_then_resume_continues_the_same_archive() {
    let dir = temp_dir("restart");
    let rollout_dir = dir.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();

    // ── 第一次进程生命周期：跑一轮带工具调用的任务 ──
    {
        let writer = Arc::new(RolloutWriter::create(&rollout_dir, &meta("a1", &dir)).unwrap());
        let backend = Arc::new(Scripted(Mutex::new(vec![vec![
            ModelStreamEvent::OutputTextDelta { delta: "先看一眼".into() },
            ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
                call_id: "c1".into(),
            }),
            ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 3, cached_input_tokens: None, output_tokens: 1 },
            },
        ]])));
        let agent = loop_with(&dir, backend, writer);
        let summary = agent.run_task("第一轮任务", Arc::new(NullSink), CancellationToken::new()).await;
        assert!(summary.turns >= 2, "有工具调用至少要两轮： {:?}", summary);
    } // agent / writer / sink 全部 drop：等价于进程退出

    let path = rollout_dir.join("a1.jsonl");
    let first_round = std::fs::read_to_string(&path).unwrap();

    // ── "重启"：只靠磁盘上的档案把会话捡回来 ──
    let resumed = resume::resume(&path).expect("档案应可重放");
    assert_eq!(resumed.agent_id, "a1");
    assert_eq!(resumed.parent_id, None);
    assert!(
        resumed.history.iter().any(|h| matches!(h, HistoryItem::FunctionCall { call_id, .. } if call_id == "c1")),
        "工具调用必须重放出来：{:?}",
        resumed.history
    );
    assert!(
        resumed.history.iter().any(|h| matches!(h, HistoryItem::FunctionCallOutput { call_id, .. } if call_id == "c1")),
        "工具输出必须重放出来，否则续跑第一次请求就 400：{:?}",
        resumed.history
    );
    // 重放出的历史直接喂给请求构造器：结构必须合法（SH-082 的判据），
    // 且合并后的条数不作定值断言——文本与同轮调用本就该合成一条 assistant
    let msgs = shaniu_lib::model_client::chat_messages("SYS", &resumed.history);
    assert_eq!(msgs.first().unwrap()["role"], "system");
    // `5344f43` 起历史首位是环境快照（system），与 SYS 合成后 system 消息不止一条：
    // 这里锁的是"第一条非 system 消息是用户任务"，不再假设它固定在下标 1。
    let first_user = msgs.iter().find(|m| m["role"] != "system").expect("重放历史里必须有用户任务");
    assert_eq!(first_user["content"], "第一轮任务", "用户任务应是第一条非 system 消息");
    assert!(
        msgs.iter().any(|m| m["role"] == "assistant" && m["tool_calls"].is_array()),
        "重放历史里必须带工具调用的 assistant 消息：{msgs:?}"
    );
    assert_tool_calls_answered(&msgs);

    // ── 第二次进程生命周期：追加到同一份档案，不重新建档 ──
    let writer = Arc::new(RolloutWriter::open_existing(&path).unwrap());
    let backend = Arc::new(Scripted(Mutex::new(vec![vec![ModelStreamEvent::OutputTextDelta {
        delta: "第二轮接着做".into(),
    }]])));
    let agent = loop_with(&dir, backend, writer);
    let sink: Arc<dyn EventSink> = Arc::new(NullSink);
    agent
        .run_with_history(resumed.history.clone(), resumed.history.len(), sink, CancellationToken::new())
        .await;

    let after = std::fs::read_to_string(&path).unwrap();
    let metas = after.lines().filter(|l| l.contains("\"type\":\"session_meta\"")).count();
    assert_eq!(metas, 1, "续跑必须走追加而不是重新建档，档案里只能有一条 session_meta");
    assert!(after.starts_with(first_round.as_str()), "旧条目不得被重写或截断");
    assert!(after.len() > first_round.len(), "第二轮应追加了新条目");

    let replayed = resume::resume(&path).unwrap();
    assert_eq!(
        replayed.history.len(),
        resumed.history.len() + 1,
        "第二轮只新增一条助手文本，旧条目不该重复：{:?}",
        replayed.history
    );
    assert_eq!(
        replayed.history.iter().filter(|h| matches!(h, HistoryItem::Message { text, .. } if text == "第一轮任务")).count(),
        1,
        "用户任务不得在档案里出现两次"
    );
}

#[tokio::test]
async fn fork_creates_child_archive_linked_to_parent_without_touching_source() {
    let dir = temp_dir("fork");
    let rollout_dir = dir.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();

    {
        let writer = Arc::new(RolloutWriter::create(&rollout_dir, &meta("pa", &dir)).unwrap());
        let backend = Arc::new(Scripted(Mutex::new(vec![vec![
            ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
                call_id: "k1".into(),
            }),
            ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 2, cached_input_tokens: None, output_tokens: 1 },
            },
        ]])));
        let agent = loop_with(&dir, backend, writer);
        agent.run_task("父任务", Arc::new(NullSink), CancellationToken::new()).await;
    }

    let src = rollout_dir.join("pa.jsonl");
    let before = std::fs::read_to_string(&src).unwrap();
    let all = rollout::read(&src).unwrap().items.len();

    // 故意要求"全部条目"——若末尾是未配对的调用，fork 必须自己回退到安全点
    let forked = resume::fork(&src, &rollout_dir, "kid", all).unwrap();
    assert_eq!(std::fs::read_to_string(&src).unwrap(), before, "fork 不得改动父档案");

    let child = resume::resume(&forked.path).unwrap();
    assert_eq!(child.parent_id.as_deref(), Some("pa"), "子档案首行要指回父代理");
    let msgs = shaniu_lib::model_client::chat_messages("SYS", &child.history);
    assert_tool_calls_answered(&msgs);

    // 子会话续跑写自己的档案，父档案保持不动
    let child_before = std::fs::read_to_string(&forked.path).unwrap();
    let writer = Arc::new(RolloutWriter::open_existing(&forked.path).unwrap());
    let backend = Arc::new(Scripted(Mutex::new(vec![vec![ModelStreamEvent::OutputTextDelta {
        delta: "子会话继续".into(),
    }]])));
    let agent = loop_with(&dir, backend, writer);
    agent
        .run_with_history(child.history.clone(), child.history.len(), Arc::new(NullSink), CancellationToken::new())
        .await;

    assert_eq!(std::fs::read_to_string(&src).unwrap(), before, "子会话的写入不该渗进父档案");
    let grew = std::fs::read_to_string(&forked.path).unwrap();
    assert_eq!(grew.lines().filter(|l| l.contains("\"type\":\"session_meta\"")).count(), 1);
    assert!(grew.starts_with(child_before.as_str()), "子档案续跑同样只能追加");
    assert!(grew.len() > child_before.len(), "子会话的新条目应落在子档案里");
}

/// 真机口径（需要密钥，按里程碑手动跑）：恢复出来的会话必须**真的能把上一轮的活接着做完**。
///
/// 只断言"没报错"是不够的——400 会被收口成 Failed，而模型看不见上下文时会"成功地做错事"。
/// 所以判据落在文件系统上：任务 A 建文件、任务 B 在**新进程装配**里改同一个文件，
/// 最终内容必须是 B 的结果，且档案仍是单条 session_meta + 两轮条目连续。
///
/// 运行：`SHANIU_TEST_API_KEY=... cargo test --test rollout_resume -- --ignored --nocapture`
#[tokio::test]
#[ignore = "SH-031 真机恢复验证需要密钥与外网，按里程碑手动运行"]
async fn resume_continues_real_work_on_a_real_endpoint() {
    use shaniu_lib::agent_loop::ResponsesBackend;
    use shaniu_lib::model_client::provider::ProviderConfig;
    use shaniu_lib::tools;

    let api_key = std::env::var("SHANIU_TEST_API_KEY").expect("需要 SHANIU_TEST_API_KEY");
    let provider = ProviderConfig {
        provider: std::env::var("SHANIU_TEST_PROVIDER").unwrap_or_else(|_| "openai-chat".into()),
        base_url: std::env::var("SHANIU_TEST_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com/v1".into()),
        model: std::env::var("SHANIU_TEST_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
        reasoning_effort: None,
            headers: Default::default(),
    };
    println!("[resume-e2e] 端点 {} {}", provider.base_url, provider.model);

    let dir = temp_dir("real");
    let rollout_dir = dir.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();

    let build = |writer: Arc<RolloutWriter>| -> (AgentLoop, Arc<ApprovalBroker>) {
        let mut router = ToolRouter::new();
        tools::register_builtin(&mut router, Arc::new(PlanState::new()), true);
        let broker = Arc::new(ApprovalBroker::new());
        let agent = AgentLoop {
            backend: Arc::new(ResponsesBackend {
                provider: provider.clone(),
                api_key: api_key.clone(),
            }),
            router: Arc::new(router),
            broker: broker.clone(),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: dir.to_path_buf(),
            max_turns: 8,
            token_budget: 0,
        tree_budget: Default::default(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: ApprovalMode::OnRequest,
                sandbox: SandboxMode::WorkspaceWrite,
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
    };

    // 任务 A：建文件（一个完整的进程生命周期）
    {
        let writer = Arc::new(RolloutWriter::create(&rollout_dir, &meta("real", &dir)).unwrap());
        let (agent, broker) = build(writer);
        let summary = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            agent.run_task(
                "在工作区创建 port.txt，内容只写一个数字 8080，不要做别的事。",
                Arc::new(AutoApprove(broker)),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("任务 A 在 120s 内未收口");
        assert_eq!(summary.reason, shaniu_lib::events::TurnEndReason::Completed, "任务 A 应正常收口");
    }
    let path = rollout_dir.join("real.jsonl");
    let after_a = resume::resume(&path).unwrap();
    println!("[resume-e2e] A 之后历史 {} 条", after_a.history.len());

    // 任务 B：新装配 + 续写同一份档案。120s 上限是刻意加的——真机用例挂死的根因通常是
    // 某个门在等不会来的回执，让它超时报错比让人盯十分钟空白终端有用。
    let writer = Arc::new(RolloutWriter::open_existing(&path).unwrap());
    let persisted = after_a.history.len();
    let mut history = after_a.history.clone();
    history.push(HistoryItem::Message {
        role: Role::User,
        text: "把 port.txt 里的数字改成 9090，只改这一个文件。".into(),
        reasoning: None,
    });
    let (agent, broker) = build(writer);
    let summary = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        agent.run_with_history(history, persisted, Arc::new(AutoApprove(broker)), CancellationToken::new()),
    )
    .await
    .expect("恢复续跑在 120s 内未收口：检查是否有门在等不会来的回执");
    assert_eq!(
        summary.reason,
        shaniu_lib::events::TurnEndReason::Completed,
        "恢复后的会话若历史重放不合法，这里会以 Failed 收口（端点 400）"
    );

    let final_text = std::fs::read_to_string(dir.join("port.txt")).unwrap_or_default();
    assert!(final_text.contains("9090"), "任务 B 的改动必须落盘，实际内容：{final_text:?}");
    assert!(!final_text.contains("8080"), "改完不应残留旧值：{final_text:?}");

    let after_b = std::fs::read_to_string(&path).unwrap();
    assert_eq!(after_b.lines().filter(|l| l.contains("\"type\":\"session_meta\"")).count(), 1);
    let replayed = resume::resume(&path).unwrap();
    assert!(replayed.history.len() > persisted, "续跑应追加条目而不是重写：{persisted} → {}", replayed.history.len());
    println!("[resume-e2e] B 之后历史 {} 条，port.txt={:?}", replayed.history.len(), final_text.trim());
}
