//! D2 端到端：Responses 的 reasoning item 从响应进历史、再原样回到下一轮请求。
//!
//! 单元用例只盯单个函数；这里把链路串起来跑真实 HTTP 与真实档案：
//! 1. 第一轮响应里的 `reasoning`（含 `encrypted_content`）必须被解析成历史项；
//! 2. 第二轮请求的 `input` 里必须出现同一份密文，且排在对应 assistant 消息**之前**；
//! 3. 档案里必须有 reasoning 项，`resume` 重建的历史仍带它且顺序不变；
//! 4. 压缩请求只要区间覆盖到 reasoning，就必须原样带上它；压掉时自然不出现。
//!
//! 反向对照：
//! - 把 `reasoning_replay` 关掉，第一条断言拿不到密文；
//! - 把 `agent_loop` 的 push 顺序挪到 assistant 消息之后，第 2 条顺序断言变红。

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt as _;
use serde_json::Value;
use shaniu_lib::agent_loop::{AgentLoop, ModelBackend, ModelStream};
use shaniu_lib::compaction::Target;
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink};
use shaniu_lib::model_client::{
    stream_responses, HistoryItem, ModelStreamEvent, ProviderConfig, RequestOptions, ResponseItem,
    Role, Usage,
};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::rollout::{self, resume, ConfigSnapshot, RolloutWriter};
use shaniu_lib::session_compact::summarize_body;
use shaniu_lib::tool_router::{
    PlanState, ToolContext, ToolError, ToolExecutor, ToolOutput, ToolRouter, ToolSpec,
};
use tokio_util::sync::CancellationToken;

const ENCRYPTED: &str = "gAAA-reasoning-ciphertext";

fn temp_dir(tag: &str) -> shaniu_lib::test_support::TestTempDir {
    shaniu_lib::test_support::TestTempDir::new(&format!("reasoning-replay-{tag}"))
}

fn options() -> RequestOptions {
    RequestOptions {
        cache_key: Some("sess-d2".into()),
        responses_extras: true,
        parallel_tool_calls: true,
        reasoning_replay: true,
    }
}

/// 两轮脚本化端点：每轮留档请求体，按顺序回对应的 SSE。
struct ScriptedServer {
    port: u16,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl ScriptedServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("取端口").port();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = bodies.clone();
        std::thread::spawn(move || {
            for round in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let payload = read_request_body(&mut stream);
                let body: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
                let include_reasoning = body["include"]
                    .as_array()
                    .is_some_and(|keys| keys.iter().any(|key| key == "reasoning.encrypted_content"));
                if body != Value::Null {
                    captured.lock().expect("锁").push(body);
                }
                // 真实端点只在声明 include 时退密文；这里照做，反向验证才成立。
                let sse = if round == 0 && include_reasoning {
                    round_one_sse(true)
                } else if round == 0 {
                    round_one_sse(false)
                } else {
                    round_two_sse().to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self { port, bodies }
    }

    fn provider(&self) -> ProviderConfig {
        ProviderConfig {
            provider: "openai-responses".into(),
            base_url: format!("http://127.0.0.1:{}", self.port),
            model: "gpt-test".into(),
            reasoning_effort: None,
            headers: Default::default(),
        }
    }

    fn body(&self, round: usize) -> Value {
        self.bodies
            .lock()
            .expect("锁")
            .get(round)
            .cloned()
            .unwrap_or_else(|| panic!("端点没收到第 {round} 轮请求"))
    }
}

/// 第一轮：推理项 + 工具调用 + 正文；模拟真实 Responses 的 output_item.done。
/// `encrypted` 为 false 时按端点行为**不带** `encrypted_content`（只声明 include 才退密文）。
fn round_one_sse(encrypted: bool) -> String {
    let reasoning_tail = if encrypted {
        format!("\"summary\":[{{\"type\":\"summary_text\",\"text\":\"先看文件再改\"}}],\"encrypted_content\":\"{ENCRYPTED}\"}}}}")
    } else {
        "\"summary\":[{\"type\":\"summary_text\",\"text\":\"先看文件再改\"}]}}".to_string()
    };
    format!(
        concat!(
            "event: response.output_text.delta\ndata: {{\"delta\":\"先读 README。\"}}\n\n",
            "event: response.output_item.done\ndata: {{\"item\":{{\"type\":\"reasoning\",\"id\":\"rs_1\",{}\n\n",
            "event: response.output_item.done\ndata: {{\"item\":{{\"type\":\"function_call\",",
            "\"name\":\"read_file\",\"arguments\":\"{{\\\"path\\\":\\\"README.md\\\"}}\",\"call_id\":\"c1\"}}}}\n\n",
            "event: response.completed\n",
            "data: {{\"response\":{{\"usage\":{{\"input_tokens\":10,\"output_tokens\":5}}}}}}\n\n",
        ),
        reasoning_tail
    )
}

/// 第二轮：纯文本收口。
fn round_two_sse() -> &'static str {
    concat!(
        "event: response.output_text.delta\ndata: {\"delta\":\"改好了。\"}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":3}}}\n\n",
    )
}

/// 读到请求头，再按 `Content-Length` 补齐正文。
fn read_request_body(stream: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        let Some(headers_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..headers_end]).to_string();
        let body_start = headers_end + 4;
        let expected = body_start + content_length(&head);
        while buf.len() < expected {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        return String::from_utf8_lossy(&buf[body_start..]).to_string();
    }
    String::new()
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn item_index(input: &[Value], predicate: impl Fn(&Value) -> bool) -> Option<usize> {
    input.iter().position(predicate)
}

/// 真实 HTTP：第一轮拿推理项 → 按 agent_loop 的顺序组历史 → 第二轮请求必须回放。
#[tokio::test]
async fn second_round_replays_the_reasoning_item_before_its_message() {
    let server = ScriptedServer::start();
    let provider = server.provider();

    // ── 第一轮 ──
    let mut history = vec![
        HistoryItem::Message { role: Role::System, text: "环境快照".into(), reasoning: None },
        HistoryItem::Message { role: Role::User, text: "改 README".into(), reasoning: None },
    ];
    let mut stream = stream_responses(&provider, "sk-test", &history, &[], "SYS", &options())
        .await
        .expect("第一轮应建流");
    let mut reasoning = None;
    let mut assistant = String::new();
    let mut calls = Vec::new();
    while let Some(event) = stream.next().await {
        match event {
            ModelStreamEvent::OutputTextDelta { delta } => assistant.push_str(&delta),
            ModelStreamEvent::OutputItem(ResponseItem::Reasoning {
                id,
                summary,
                encrypted_content,
            }) => {
                reasoning = Some(HistoryItem::Reasoning { id, summary, encrypted_content });
            }
            ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
            }) => calls.push((name, arguments, call_id)),
            _ => {}
        }
    }
    let reasoning = reasoning.expect("第一轮必须解析出 reasoning 项");
    match &reasoning {
        HistoryItem::Reasoning { encrypted_content, .. } => {
            assert_eq!(encrypted_content.as_deref(), Some(ENCRYPTED), "密文要原样收下");
        }
        other => panic!("应是 Reasoning 项：{other:?}"),
    }

    // agent_loop 的入历史顺序：推理 → assistant 消息 → 调用 → 工具输出。
    history.push(reasoning);
    history.push(HistoryItem::Message {
        role: Role::Assistant,
        text: assistant,
        reasoning: None,
    });
    for (name, arguments, call_id) in calls {
        history.push(HistoryItem::FunctionCall { name, arguments, call_id });
    }
    history.push(HistoryItem::FunctionCallOutput {
        call_id: "c1".into(),
        output: "# README".into(),
    });

    // ── 第二轮：同一条链路再发一次 ──
    let mut stream = stream_responses(&provider, "sk-test", &history, &[], "SYS", &options())
        .await
        .expect("第二轮应建流");
    while stream.next().await.is_some() {}

    let body = server.body(1);
    assert_eq!(body["include"], serde_json::json!(["reasoning.encrypted_content"]));
    let input = body["input"].as_array().expect("input 必须是数组");
    let reasoning_at = item_index(input, |item| item["type"] == "reasoning")
        .unwrap_or_else(|| panic!("第二轮必须回放 reasoning：{body}"));
    assert_eq!(input[reasoning_at]["encrypted_content"], ENCRYPTED);
    assert_eq!(input[reasoning_at]["id"], "rs_1");
    assert_eq!(input[reasoning_at]["summary"][0]["type"], "summary_text");
    let assistant_at = item_index(input, |item| item["role"] == "assistant")
        .unwrap_or_else(|| panic!("第二轮必须带 assistant 消息：{body}"));
    assert!(
        reasoning_at < assistant_at,
        "reasoning 必须排在 assistant 消息之前（{reasoning_at} vs {assistant_at}）：{body}"
    );
    let call_at = item_index(input, |item| item["type"] == "function_call")
        .unwrap_or_else(|| panic!("第二轮必须带上一轮的工具调用：{body}"));
    assert!(assistant_at < call_at, "调用必须跟在消息之后：{body}");

    // ── 压缩请求：区间覆盖到推理就必须带上它，压掉时不出现 ──
    let kept = summarize_body(&provider, &history, Target { range_start: 0, range_end: 5 }, &options());
    assert!(
        kept["input"]
            .as_array()
            .expect("压缩 input")
            .iter()
            .any(|item| item["type"] == "reasoning" && item["encrypted_content"] == ENCRYPTED),
        "压缩区间覆盖推理时必须原样带上：{kept}"
    );
    let dropped = summarize_body(&provider, &history, Target { range_start: 0, range_end: 2 }, &options());
    assert!(
        !dropped["input"]
            .as_array()
            .expect("压缩 input")
            .iter()
            .any(|item| item["type"] == "reasoning"),
        "推理落在被压区间内时不该出现在摘要请求里：{dropped}"
    );
}

// ── agent_loop 层：顺序、落档与 resume ─────────────────────────────────────

/// 按脚本轮次吐事件的桩后端；同时记下每轮实际收到的历史。
struct RecordingBackend {
    queues: Mutex<Vec<Vec<ModelStreamEvent>>>,
    seen: Mutex<Vec<Vec<HistoryItem>>>,
}

#[async_trait]
impl ModelBackend for RecordingBackend {
    async fn stream(
        &self,
        history: &[HistoryItem],
        _tools: &[shaniu_lib::agent_loop::ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, shaniu_lib::model_client::ModelError> {
        self.seen.lock().unwrap().push(history.to_vec());
        let mut queues = self.queues.lock().unwrap();
        let script = if queues.is_empty() {
            vec![ModelStreamEvent::OutputTextDelta { delta: "收尾".into() }, ModelStreamEvent::Finished]
        } else {
            queues.remove(0)
        };
        Ok(Box::pin(futures::stream::iter(script)))
    }
}

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

fn meta(agent_id: &str, dir: &std::path::Path) -> rollout::RolloutItem {
    rollout::RolloutItem::SessionMeta {
        session_id: agent_id.into(),
        agent_id: agent_id.into(),
        parent_id: None,
        created_at: 1_700_000_000_000,
        config_snapshot: ConfigSnapshot {
            model: "m".into(),
            sandbox: "WorkspaceWrite".into(),
            approval: "AutoReadOnly".into(),
            workspace_root: dir.display().to_string(),
            ..Default::default()
        },
    }
}

fn agent_loop(backend: Arc<RecordingBackend>, writer: Arc<RolloutWriter>, ws: &std::path::Path) -> AgentLoop {
    let mut router = ToolRouter::new();
    router.register(Arc::new(EchoTool::default()));
    AgentLoop {
        backend,
        router: Arc::new(router),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: ws.to_path_buf(),
        max_turns: 3,
        token_budget: 0,
        manual_token_budget: 0,
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
    }
}

/// 真实回合：推理项排在 assistant 消息前入历史，并原样落档、可 resume。
#[tokio::test]
async fn agent_loop_records_reasoning_before_the_message_and_resume_keeps_it() {
    let dir = temp_dir("agent-loop");
    let rollout_dir = dir.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();

    let path: PathBuf;
    {
        let writer = Arc::new(RolloutWriter::create(&rollout_dir, &meta("d2", &dir)).unwrap());
        path = writer.path().to_path_buf();
        let backend = Arc::new(RecordingBackend {
            queues: Mutex::new(vec![
                // 第一轮：推理 + 正文 + 工具调用（reasoning 先到，模拟真实事件顺序）
                vec![
                    ModelStreamEvent::OutputItem(ResponseItem::Reasoning {
                        id: Some("rs_1".into()),
                        summary: vec![shaniu_lib::model_client::ReasoningSummary {
                            kind: "summary_text".into(),
                            text: "先看文件再改".into(),
                        }],
                        encrypted_content: Some(ENCRYPTED.into()),
                    }),
                    ModelStreamEvent::OutputTextDelta { delta: "先读 README。".into() },
                    ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                        name: "read_file".into(),
                        arguments: "{}".into(),
                        call_id: "c1".into(),
                    }),
                    ModelStreamEvent::Completed {
                        usage: Usage {
                            input_tokens: 10,
                            cached_input_tokens: None,
                            output_tokens: 5,
                        },
                    },
                ],
                // 第二轮：纯文本收口
                vec![
                    ModelStreamEvent::OutputTextDelta { delta: "改好了。".into() },
                    ModelStreamEvent::Finished,
                ],
            ]),
            seen: Mutex::new(Vec::new()),
        });
        let seen = backend.clone();
        let agent = agent_loop(backend, writer, &dir);
        let summary =
            agent.run_task("改 README", Arc::new(NullSink), CancellationToken::new()).await;
        assert_eq!(summary.turns, 2, "有工具调用应跑两轮：{summary:?}");

        let seen = seen.seen.lock().unwrap();
        assert!(seen.len() >= 2, "至少两轮采样：{}", seen.len());
        let second = seen.last().unwrap();
        let reasoning_at = second
            .iter()
            .position(|item| matches!(item, HistoryItem::Reasoning { .. }))
            .unwrap_or_else(|| panic!("第二轮历史必须带推理项：{second:#?}"));
        let assistant_at = second
            .iter()
            .position(|item| {
                matches!(item, HistoryItem::Message { role: Role::Assistant, .. } if !item_text(item).is_empty())
            })
            .unwrap_or_else(|| panic!("第二轮历史必须带 assistant 消息：{second:#?}"));
        assert!(
            reasoning_at < assistant_at,
            "推理项必须在 assistant 消息之前（{reasoning_at} vs {assistant_at}）：{second:#?}"
        );
    } // 等价于进程退出：只留磁盘档案

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.lines().any(|line| line.contains("\"type\":\"reasoning\"") && line.contains(ENCRYPTED)),
        "档案里必须有带密文的 reasoning 项：{raw}"
    );

    let resumed = resume::resume(&path).expect("档案应可重放");
    let reasoning_at = resumed
        .history
        .iter()
        .position(|item| matches!(item, HistoryItem::Reasoning { .. }))
        .unwrap_or_else(|| panic!("resume 必须重放出推理项：{:#?}", resumed.history));
    match &resumed.history[reasoning_at] {
        HistoryItem::Reasoning { encrypted_content, .. } => {
            assert_eq!(encrypted_content.as_deref(), Some(ENCRYPTED));
        }
        other => panic!("应是 Reasoning 项：{other:?}"),
    }
    let assistant_at = resumed
        .history
        .iter()
        .position(|item| {
            matches!(item, HistoryItem::Message { role: Role::Assistant, .. } if item_text(item).contains("先读 README"))
        })
        .unwrap_or_else(|| panic!("resume 必须重放出 assistant 消息：{:#?}", resumed.history));
    assert!(
        reasoning_at < assistant_at,
        "resume 后顺序必须不变（{reasoning_at} vs {assistant_at}）"
    );
}

fn item_text(item: &HistoryItem) -> &str {
    match item {
        HistoryItem::Message { text, .. } => text,
        _ => "",
    }
}
