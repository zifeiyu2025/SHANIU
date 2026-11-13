//! 失败面题集的本地 mock 端点。
//!
//! 只服务 `evals/failure-tasks.json`：按题单里的 `inject.steps` 逐次请求出剧本。
//! 产品路径与正常题集都不引用本模块，因此失败注入默认不存在。
//!
//! 协议按 Responses SSE 的最小子集实现：HTTP/1.1 + `Connection: close`，
//! 流式响应用连接关闭定界（这也是「没有终态标记就 EOF」这条注入的物理形态）。

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 一条注入剧本。字段与 `evals/failure-tasks.json` 的 `inject.steps` 一一对应。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// 200 SSE：只发 `response.created` 就关连接（无终态标记）。
    TruncateBeforeDelta,
    /// 200 SSE：发 `response.created` + 一条正文增量后关连接（无终态标记）。
    TruncateAfterDelta { text: String },
    /// HTTP 429 + `Retry-After`。
    RateLimit { retry_after_secs: u64 },
    /// HTTP 429，错误体点名配额不足。
    Quota,
    /// 200 SSE：发增量后 `response.incomplete`。
    Incomplete { text: String, reason: String },
    /// 200 SSE：流内 `response.failed`（无 HTTP 状态码）。
    OverflowStream { code: String, message: String },
    /// 非 2xx 的上下文超长（真实端点最常见形态）。
    OverflowHttp { status: u16, code: String, message: String },
    /// 正常作答（正文增量 + `response.completed`）。
    Text {
        text: String,
        #[serde(default)]
        usage: UsageSpec,
    },
    /// 压缩摘要请求的正常作答：协议与 `Text` 相同，语义上便于题单阅读。
    Summary { text: String },
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageSpec {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Self::TruncateBeforeDelta => "truncate_before_delta",
            Self::TruncateAfterDelta { .. } => "truncate_after_delta",
            Self::RateLimit { .. } => "rate_limit",
            Self::Quota => "quota",
            Self::Incomplete { .. } => "incomplete",
            Self::OverflowStream { .. } => "overflow_stream",
            Self::OverflowHttp { .. } => "overflow_http",
            Self::Text { .. } => "text",
            Self::Summary { .. } => "summary",
        }
    }
}

/// 一次请求的观测。判据全部落在这些字段上，不靠日志猜。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RequestRecord {
    pub index: usize,
    /// 相对 mock 启动的毫秒数（用于测 Retry-After 的实际等待）。
    pub at_ms: u64,
    pub method: String,
    pub path: String,
    /// 请求类型：summary = 压缩摘要请求，sample = 主采样/重放请求。
    /// 依据是产品压缩指令里的固定措辞；措辞变了这张表就会误分类，判据会 fail-closed。
    pub kind: String,
    /// 请求体 `tools` 数组长度（当前产品在本题场景里不注册工具，仅作旁证）。
    pub tools_count: usize,
    /// `input` 数组序列化后的字符数：用来证明重放请求确实变小了。
    pub input_chars: usize,
    /// `input` 的前 4000 字符（判据只需要标记命中，不落全文）。
    pub input_head: String,
    /// 题单声明的探针标记是否出现在整个请求体里。
    pub probes: Value,
    /// 实际服务的剧本动作；`null` = 剧本已用尽。
    pub served: Option<String>,
    pub step_exhausted: bool,
}

struct State {
    steps: Mutex<VecDeque<Action>>,
    requests: Arc<Mutex<Vec<RequestRecord>>>,
    probes: Vec<String>,
    started: Instant,
}

pub struct MockServer {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<RequestRecord>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl MockServer {
    /// 绑定 127.0.0.1:0 并开始收请求；端口在 `port`。
    pub async fn spawn(steps: Vec<Action>, probes: Vec<String>) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(State {
            steps: Mutex::new(steps.into()),
            requests: requests.clone(),
            probes,
            started: Instant::now(),
        });
        let handle = tokio::spawn(accept_loop(listener, state));
        Ok(Self { port, requests, handle })
    }
}

async fn accept_loop(listener: TcpListener, state: Arc<State>) {
    loop {
        match listener.accept().await {
            Ok((socket, _)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    handle_conn(socket, state).await;
                });
            }
            Err(_) => return,
        }
    }
}

async fn handle_conn(mut socket: TcpStream, state: Arc<State>) {
    let Ok((method, path, body)) = read_request(&mut socket).await else {
        return;
    };
    let body_json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let input_text = serde_json::to_string(&body_json.get("input").cloned().unwrap_or(Value::Null))
        .unwrap_or_default();
    let tools_count = body_json
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let kind = if body.contains("压缩成一份交接摘要") { "summary" } else { "sample" };
    let probes: Value = Value::Object(
        state
            .probes
            .iter()
            .map(|probe| (probe.clone(), Value::Bool(body.contains(probe.as_str()))))
            .collect(),
    );

    let action = state.steps.lock().map(|mut steps| steps.pop_front()).ok().flatten();
    {
        let mut requests = state.requests.lock().expect("请求表锁中毒");
        let record = RequestRecord {
            index: requests.len(),
            at_ms: u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            method,
            path,
            kind: kind.to_string(),
            tools_count,
            input_chars: input_text.chars().count(),
            input_head: head(&input_text, 4000),
            probes,
            served: action.as_ref().map(|a| a.name().to_string()),
            step_exhausted: action.is_none(),
        };
        requests.push(record);
    }

    match action {
        Some(action) => respond(&mut socket, &action).await,
        None => {
            write_http(
                &mut socket,
                500,
                &[],
                &json!({
                    "error": {
                        "code": "eval_script_exhausted",
                        "message": "mock 剧本已用尽：宿主发出了超出题单预期的请求"
                    }
                })
                .to_string(),
            )
            .await;
        }
    }
}

async fn respond(socket: &mut TcpStream, action: &Action) {
    match action {
        Action::TruncateBeforeDelta => {
            write_sse(socket, &[created_event()]).await;
            let _ = socket.shutdown().await;
        }
        Action::TruncateAfterDelta { text } => {
            write_sse(socket, &[created_event(), delta_event(text)]).await;
            let _ = socket.shutdown().await;
        }
        Action::RateLimit { retry_after_secs } => {
            let headers = vec![("Retry-After".to_string(), retry_after_secs.to_string())];
            write_http(
                socket,
                429,
                &headers,
                &json!({"error":{"code":"rate_limit_exceeded","message":"slow down"}}).to_string(),
            )
            .await;
        }
        Action::Quota => {
            write_http(
                socket,
                429,
                &[],
                &json!({
                    "error": {
                        "code": "insufficient_quota",
                        "message": "You exceeded your current quota, please check your plan and billing details."
                    }
                })
                .to_string(),
            )
            .await;
        }
        Action::Incomplete { text, reason } => {
            write_sse(
                socket,
                &[
                    created_event(),
                    delta_event(text),
                    ("response.incomplete", json!({
                        "type": "response.incomplete",
                        "response": {
                            "status": "incomplete",
                            "incomplete_details": { "reason": reason }
                        }
                    })),
                ],
            )
            .await;
        }
        Action::OverflowStream { code, message } => {
            write_sse(
                socket,
                &[
                    created_event(),
                    ("response.failed", json!({
                        "type": "response.failed",
                        "response": {
                            "status": "failed",
                            "error": { "code": code, "message": message }
                        }
                    })),
                ],
            )
            .await;
        }
        Action::OverflowHttp { status, code, message } => {
            write_http(
                socket,
                *status,
                &[],
                &json!({"error":{"code":code,"message":message}}).to_string(),
            )
            .await;
        }
        Action::Text { text, usage } => {
            write_sse(
                socket,
                &[
                    created_event(),
                    delta_event(text),
                    ("response.completed", json!({
                        "type": "response.completed",
                        "response": {
                            "status": "completed",
                            "usage": {
                                "input_tokens": usage.input_tokens,
                                "output_tokens": usage.output_tokens
                            }
                        }
                    })),
                ],
            )
            .await;
        }
        Action::Summary { text } => {
            write_sse(
                socket,
                &[
                    created_event(),
                    delta_event(text),
                    ("response.completed", json!({
                        "type": "response.completed",
                        "response": { "status": "completed" }
                    })),
                ],
            )
            .await;
        }
    }
}

fn created_event() -> (&'static str, Value) {
    ("response.created", json!({"type":"response.created","response":{"status":"in_progress"}}))
}

fn delta_event(text: &str) -> (&'static str, Value) {
    ("response.output_text.delta", json!({"type":"response.output_text.delta","delta":text}))
}

async fn write_sse(socket: &mut TcpStream, events: &[(&str, Value)]) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for (event, data) in events {
        let frame = format!("event: {event}\ndata: {data}\n\n");
        if socket.write_all(frame.as_bytes()).await.is_err() {
            return;
        }
    }
    let _ = socket.flush().await;
}

async fn write_http(socket: &mut TcpStream, status: u16, headers: &[(String, String)], body: &str) {
    let reason = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let _ = socket.write_all(body.as_bytes()).await;
    let _ = socket.flush().await;
}

async fn read_request(socket: &mut TcpStream) -> io::Result<(String, String, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        let read = socket.read(&mut tmp).await?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "请求头不完整"));
        }
        buf.extend_from_slice(&tmp[..read]);
        if buf.len() > 8 * 1024 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "请求头过大"));
        }
    };
    let head_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("POST").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = socket.read(&mut tmp).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..read]);
    }
    body.truncate(content_length);
    Ok((method, path, String::from_utf8_lossy(&body).to_string()))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

pub fn head(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}
