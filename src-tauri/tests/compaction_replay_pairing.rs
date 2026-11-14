//! 压缩后重放的配对契约（2026-11-14 真机冒烟 N1）。
//!
//! 真机现象：`token_budget=5000` 触发压缩后，端点回 500
//! `Cannot resolve tool name for tool_call_id='call_0e6f3061a37b43cf96c3ef09'`，
//! 重试 5 次全失败，之后该会话每次提问都失败——会话等于废了。
//!
//! 根因是压缩切点落在「调用 → 输出」组中间：调用被摘要取代、输出留在保留段，
//! 重放时 `role=tool` 指向一个端点解析不到的 `tool_call_id`。
//!
//! 这里用本地严格端点复现该判定（真机错误文案原样），断言两件事：
//! 1. 压缩切点不会拆散工具调用组，压缩后的请求一次成功；
//! 2. 即使历史已经坏了（旧版本档案 / 外部裁剪），请求也能继续，不靠重试。

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use shaniu_lib::compaction;
use shaniu_lib::model_client::{stream_chat, HistoryItem, ModelStreamEvent, ProviderConfig, Role};

/// 本地端点：收到请求先按真机规则校验 `role=tool` 的 `tool_call_id` 是否已声明，
/// 未声明就回 500（与真机同一句错误文案），否则回一段合法 SSE。
struct StrictEndpoint {
    port: u16,
    stop: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl StrictEndpoint {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let thread_stop = stop.clone();
        let thread_calls = calls.clone();
        let thread_bodies = bodies.clone();
        std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Windows 上 accept 出的 socket 会继承非阻塞标志，必须切回阻塞。
                        stream.set_nonblocking(false).unwrap();
                        thread_calls.fetch_add(1, Ordering::SeqCst);
                        let request = read_request(&mut stream);
                        let body = request_body(&request);
                        thread_bodies.lock().unwrap().push(body.clone());
                        let response = match orphan_tool_call_id(&body) {
                            Some(call_id) => format!(
                                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\
Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                                error_body(&call_id).len(),
                                error_body(&call_id)
                            ),
                            None => format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{SSE}"
                            ),
                        };
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            stop,
            calls,
            bodies,
        }
    }

    fn provider(&self) -> ProviderConfig {
        ProviderConfig {
            provider: "openai-chat".into(),
            base_url: format!("http://127.0.0.1:{}", self.port),
            model: "test-model".into(),
            reasoning_effort: None,
            headers: Default::default(),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_body(&self) -> serde_json::Value {
        self.bodies
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("端点没有收到请求")
    }
}

impl Drop for StrictEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

const SSE: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\r\n\r\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\r\n\r\n\
data: [DONE]\r\n\r\n";

fn error_body(call_id: &str) -> String {
    format!(
        "{{\"error\":{{\"message\":\"Internal server error: Cannot resolve tool name for \
tool_call_id='{call_id}'. Either 'tool_calls[].function.name' in assistant message or 'name' in \
tool message must be provided.\",\"type\":\"internal_error\"}}}}"
    )
}

/// 真机判定：每个 `role=tool` 的 `tool_call_id` 必须能在**前面**的 assistant
/// `tool_calls` 里声明过。返回第一处孤立的 id。
fn orphan_tool_call_id(body: &serde_json::Value) -> Option<String> {
    let messages = body.get("messages")?.as_array()?;
    let mut declared = std::collections::HashSet::new();
    for message in messages {
        if message.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            for call in message
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(id) = call.get("id").and_then(|i| i.as_str()) {
                    declared.insert(id.to_string());
                }
            }
        }
        if message.get("role").and_then(|r| r.as_str()) == Some("tool") {
            let id = message
                .get("tool_call_id")
                .and_then(|i| i.as_str())
                .unwrap_or_default();
            if !declared.contains(id) {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        match body_start(&request) {
            Some(start) => {
                let head = String::from_utf8_lossy(&request[..start]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:").map(str::trim))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= start + length {
                    break;
                }
            }
            None => continue,
        }
    }
    request
}

fn body_start(request: &[u8]) -> Option<usize> {
    request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn request_body(request: &[u8]) -> serde_json::Value {
    let start = body_start(request).expect("请求缺少空行分隔");
    serde_json::from_slice(&request[start..]).unwrap_or(serde_json::Value::Null)
}

fn call(id: &str) -> HistoryItem {
    HistoryItem::FunctionCall {
        name: "unified_exec".into(),
        arguments: "{\"path\":\"package.json\"}".into(),
        call_id: id.into(),
    }
}

fn output(id: &str, text: &str) -> HistoryItem {
    HistoryItem::FunctionCallOutput {
        call_id: id.into(),
        output: text.into(),
    }
}

fn message(role: Role, text: &str) -> HistoryItem {
    HistoryItem::Message {
        role,
        text: text.into(),
        reasoning: None,
    }
}

fn summary(text: &str) -> HistoryItem {
    HistoryItem::Message {
        role: Role::User,
        text: text.into(),
        reasoning: None,
    }
}

/// 真机里被拆散的那一组：预算 100 → 保留额度 30 token，旧切点正好落在输出上。
fn smoke_history() -> Vec<HistoryItem> {
    vec![
        message(Role::User, "任务目标"),
        message(Role::Assistant, "先读文件"),
        call("call_0e6f3061a37b43cf96c3ef09"),
        output("call_0e6f3061a37b43cf96c3ef09", &"x".repeat(100)),
        message(Role::User, "继续"),
    ]
}

fn apply_target(history: &mut Vec<HistoryItem>, target: compaction::Target) {
    history.splice(target.range_start..target.range_end, [summary("压缩摘要")]);
}

/// 正向：压缩切点不拆组，压缩后的第一条请求就成功（旧实现这里 500）。
#[tokio::test]
async fn compaction_keeps_tool_pairing_and_first_request_succeeds() {
    let endpoint = StrictEndpoint::start();
    let mut history = smoke_history();
    let target = compaction::plan_manual(&history, 100).expect("有可压区间");
    assert_eq!(
        target.range_end, 2,
        "切点必须回退到调用组的起点，而不是落在输出上"
    );
    apply_target(&mut history, target);

    let events: Vec<ModelStreamEvent> =
        stream_chat(&endpoint.provider(), "k", &history, &[], "SYS")
            .await
            .expect("压缩后的请求不能 500")
            .collect()
            .await;
    assert!(events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::Finished)));
    assert_eq!(endpoint.calls(), 1, "不能靠重试把失败掩盖过去");
    assert!(orphan_tool_call_id(&endpoint.last_body()).is_none());
}

/// 兜底：历史已经坏了（旧版本档案、外部裁剪）也不能让会话永久不可用。
/// 反向对照：撤掉 `degrade_orphan_tool_outputs`，`stream_chat` 会返回 SERVER 错误。
#[tokio::test]
async fn orphan_tool_output_from_broken_history_is_downgraded_not_fatal() {
    let endpoint = StrictEndpoint::start();
    let history = vec![
        message(Role::User, "这是旧版本压缩后留下的历史"),
        output("call_0e6f3061a37b43cf96c3ef09", "package.json 的内容"),
    ];

    let events: Vec<ModelStreamEvent> =
        stream_chat(&endpoint.provider(), "k", &history, &[], "SYS")
            .await
            .expect("孤立输出必须被降级成文本，而不是让请求 500")
            .collect()
            .await;
    assert!(events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::Finished)));
    assert_eq!(endpoint.calls(), 1, "第一条请求就必须成功，不能靠重试");
    assert!(orphan_tool_call_id(&endpoint.last_body()).is_none());
    // 信息不丢：原始输出以文本形式仍发给模型。
    let body = endpoint.last_body();
    let text = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("package.json 的内容"),
        "降级不能丢内容：{text}"
    );
    assert!(
        text.contains("call_0e6f3061a37b43cf96c3ef09"),
        "要留下 call_id 供对账：{text}"
    );
}
