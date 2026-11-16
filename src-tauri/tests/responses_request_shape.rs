//! Responses 协议请求体的形状与配对守门。
//!
//! 两次实测缺陷都是这条路径打出来的：
//!
//! 1. **2026-11-15**：`responses_input()` 把 `HistoryItem` 的 serde 形态原样发出
//!    （`{"role":"user","text":"…"}`），而 Responses 的 message 输入项只认 `content`
//!    数组——`text` 是陌生键，端点忽略，用户消息静默消失（评测 4 轮 0 文件改动）。
//! 2. **2026-11-16（P0）**：历史里出现「有调用、没输出」的半截工具记录时，端点直接
//!    400 `No tool output found for tool call <call_id>.`——12 题里 3 题因此 FAIL
//!    （`repo-bug-retry-after` 等，400 只用 129～164ms，是服务端立即拒绝）。
//!
//! 这里不用「我们发的形状等于我们以为的形状」这种自证式断言：本地假端点按真实端点的
//! 规则**校验**请求体，不合规就回 400、错误文案照抄真实端点。整个测试走真实 HTTP 往返
//! （`stream_responses` → 端点），所以构造、发送、建流三段都在覆盖范围内。

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use shaniu_lib::model_client::{
    stream_responses, HistoryItem, ModelStreamEvent, ProviderConfig, RequestOptions, Role,
};

const TASK: &str = "把 README 第三段译成中文，写到 docs/readme.zh.md";

/// 夹具来源：`evals/repo-tasks.json` 里 `repo-bug-retry-after` 的原始 prompt。
const RETRY_AFTER_PROMPT: &str = "这是真实仓库快照。`cargo test --lib model_client::retry` 有失败。\n\
请定位并修复实现，让重试策略在端点给出 Retry-After 时优先采用它、超过上限时放弃重试。\n\
范围：只改 src-tauri/src 下的实现；不要改测试，不要动 scripts/dev。完成后跑 \
`cargo test --lib model_client::retry` 确认。";

/// 端点 400 原话里的 call_id（`phase2-shaniu/summary.json`，
/// `No tool output found for tool call call_01_X7pFedLZ8pmzxHEd0PmX7825.`）。
const UNFINISHED_CALL_ID: &str = "call_01_X7pFedLZ8pmzxHEd0PmX7825";

/// 一次性本地端点：留档请求体，按真实端点规则校验，再回 SSE（或 400）。
struct CapturingServer {
    port: u16,
    body: Arc<Mutex<Option<serde_json::Value>>>,
}

impl CapturingServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("取端口").port();
        let body = Arc::new(Mutex::new(None));
        let captured = body.clone();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let payload = read_request_body(&mut stream);
            let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
            let verdict = validate_input(&parsed);
            *captured.lock().expect("锁") = Some(parsed);
            let response = match verdict {
                Ok(()) => {
                    let sse = concat!(
                        "event: response.output_text.delta\ndata: {\"delta\":\"ok\"}\n\n",
                        "event: response.completed\n",
                        "data: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                        sse.len()
                    )
                }
                Err(message) => {
                    let error = serde_json::json!({
                        "error": {
                            "message": message,
                            "type": "invalid_request_error",
                            "param": serde_json::Value::Null,
                            "code": serde_json::Value::Null,
                        }
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                        error.len()
                    )
                }
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        Self { port, body }
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

    fn body(&self) -> serde_json::Value {
        self.body
            .lock()
            .expect("锁")
            .clone()
            .expect("端点必须收到请求体")
    }
}

/// 按真实端点规则校验 Responses `input`。
///
/// 只校**已被真实端点打脸证实**的规则，文案照抄：
/// - message 项必须有 `content`，且不带同级 `text`（`Unrecognized request argument`）；
/// - `function_call` 的 `arguments` 必须是 JSON 字符串；
/// - 每个 `function_call` 必须在它之后有一条同 `call_id` 的 `function_call_output`，
///   否则 `No tool output found for tool call <id>.`；
/// - 每条 `function_call_output` 必须在它之前有一条同 `call_id` 的调用，否则
///   `No tool call found for function call output with call_id <id>.`。
fn validate_input(body: &serde_json::Value) -> Result<(), String> {
    let input = body["input"]
        .as_array()
        .ok_or_else(|| "Missing required parameter: 'input'".to_string())?;
    let mut calls: Vec<(usize, String)> = Vec::new();
    let mut outputs: Vec<(usize, String)> = Vec::new();
    for (idx, item) in input.iter().enumerate() {
        if item.get("text").is_some() {
            return Err("Unrecognized request argument supplied: text".into());
        }
        match item["type"].as_str() {
            Some("function_call") => {
                let call_id = item["call_id"]
                    .as_str()
                    .ok_or_else(|| format!("Missing required parameter: 'input[{idx}].call_id'"))?;
                if item["name"].as_str().unwrap_or_default().is_empty() {
                    return Err(format!("Missing required parameter: 'input[{idx}].name'"));
                }
                if !item["arguments"].is_string() {
                    return Err(format!(
                        "Invalid type for 'input[{idx}].arguments': expected a string."
                    ));
                }
                calls.push((idx, call_id.to_string()));
            }
            Some("function_call_output") => {
                let call_id = item["call_id"]
                    .as_str()
                    .ok_or_else(|| format!("Missing required parameter: 'input[{idx}].call_id'"))?;
                if !item["output"].is_string() {
                    return Err(format!(
                        "Invalid type for 'input[{idx}].output': expected a string."
                    ));
                }
                outputs.push((idx, call_id.to_string()));
            }
            Some("reasoning") => {
                if !item["summary"].is_array() {
                    return Err(format!("Missing required parameter: 'input[{idx}].summary'"));
                }
            }
            // 图片消息不带 type（只有 role + content），与 message 同一条校验路径
            Some("message") | None => {
                if item.get("role").is_none() {
                    return Err(format!("Missing required parameter: 'input[{idx}].type'"));
                }
                match &item["content"] {
                    serde_json::Value::String(_) => {}
                    serde_json::Value::Array(parts) if !parts.is_empty() => {
                        for (part_idx, part) in parts.iter().enumerate() {
                            match part["type"].as_str() {
                                Some("input_text" | "output_text") if part["text"].is_string() => {}
                                Some("input_image") if part["image_url"].is_string() => {}
                                other => {
                                    return Err(format!(
                                        "Invalid content part at 'input[{idx}].content[{part_idx}]': {other:?}"
                                    ))
                                }
                            }
                        }
                    }
                    _ => return Err(format!("Missing required parameter: 'input[{idx}].content'")),
                }
            }
            Some(other) => {
                return Err(format!("Invalid value for 'input[{idx}].type': '{other}'"));
            }
        }
    }
    for (call_idx, call_id) in &calls {
        if !outputs.iter().any(|(idx, id)| id == call_id && idx > call_idx) {
            return Err(format!("No tool output found for tool call {call_id}."));
        }
    }
    for (out_idx, call_id) in &outputs {
        if !calls.iter().any(|(idx, id)| id == call_id && idx < out_idx) {
            return Err(format!(
                "No tool call found for function call output with call_id {call_id}."
            ));
        }
    }
    Ok(())
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
        let Some(headers_end) = find_headers_end(&buf) else {
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

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
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

/// 完整一对调用/输出 + 三种角色的文本，用于形状核对。
fn history() -> Vec<HistoryItem> {
    vec![
        // system 注入（环境快照 / 停止钩子提醒）走同一个 message 形状
        HistoryItem::Message { role: Role::System, text: "环境快照".into(), reasoning: None },
        HistoryItem::Message { role: Role::User, text: TASK.into(), reasoning: None },
        HistoryItem::Message {
            role: Role::Assistant,
            text: "先读 README。".into(),
            reasoning: Some("先看文件再改".into()),
        },
        HistoryItem::FunctionCall {
            name: "read_file".into(),
            arguments: r#"{"path":"README.md"}"#.into(),
            call_id: "c1".into(),
        },
        HistoryItem::FunctionCallOutput { call_id: "c1".into(), output: "# README".into() },
    ]
}

/// 评测现场的形状：调用写了、输出没写，之后又推进了一轮。
///
/// prompt 与 call_id 逐字取自本机评测产物；「调用之后没有输出」这一段是中断/存档在
/// `agent_loop` 的 `break 'outer` 与 rollout 两次写入之间留下的真实形态。
fn unfinished_call_history() -> Vec<HistoryItem> {
    vec![
        HistoryItem::Message { role: Role::User, text: RETRY_AFTER_PROMPT.into(), reasoning: None },
        HistoryItem::Message {
            role: Role::Assistant,
            text: "先看重试实现。".into(),
            reasoning: None,
        },
        HistoryItem::FunctionCall {
            name: "read_file".into(),
            arguments: r#"{"path":"src-tauri/src/model_client/retry.rs"}"#.into(),
            call_id: UNFINISHED_CALL_ID.into(),
        },
        HistoryItem::Message { role: Role::User, text: "继续。".into(), reasoning: None },
    ]
}

/// 缺陷本体 1：用户任务文本必须出现在 `content` 数组里，而不是同级的 `text` 字段。
#[tokio::test]
async fn user_text_reaches_the_wire_inside_the_content_array() {
    let server = CapturingServer::start();
    let mut stream = stream_responses(&server.provider(), "sk-test", &history(), &[], "SYS", &RequestOptions::default())
        .await
        .expect("本地端点应成功建流");
    let events: Vec<ModelStreamEvent> = stream.by_ref().collect().await;
    // 流本身要能走完：请求构造错时端点会 400，这里先把传输面排除掉
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelStreamEvent::Completed { .. })),
        "本地端点应回 completed：{events:?}"
    );

    let body = server.body();
    assert_eq!(body["instructions"], "SYS", "系统提示走顶层 instructions");

    let input = body["input"].as_array().expect("input 必须是数组");
    let user = input
        .iter()
        .find(|item| item["role"] == "user")
        .expect("请求里必须有 user 消息");
    // 反向对照：改回 `text` 字段时，下面这两条断言（以及端点的 400）就是红的落点
    assert!(user.get("text").is_none(), "用户文本发到了同级 text 字段：{user}");
    assert_eq!(user["content"][0]["type"], "input_text");
    assert_eq!(user["content"][0]["text"], TASK);
}

/// 其余角色/条目一并核对：assistant 文本、system 注入、工具调用与工具输出。
#[tokio::test]
async fn other_roles_and_tool_items_use_responses_shapes() {
    let server = CapturingServer::start();
    let mut stream = stream_responses(&server.provider(), "sk-test", &history(), &[], "SYS", &RequestOptions::default())
        .await
        .expect("本地端点应成功建流");
    let _: Vec<ModelStreamEvent> = stream.by_ref().collect().await;

    let body = server.body();
    let input = body["input"].as_array().expect("input 必须是数组");

    let assistant = input
        .iter()
        .find(|item| item["role"] == "assistant")
        .expect("缺少 assistant 消息");
    assert_eq!(assistant["type"], "message");
    assert_eq!(assistant["content"][0]["type"], "output_text");
    assert_eq!(assistant["content"][0]["text"], "先读 README。");
    // reasoning 回放要独立的 reasoning item（带 encrypted_content）；明文塞消息项是陌生键
    assert!(assistant.get("reasoning").is_none(), "{input:?}");

    let system = input
        .iter()
        .find(|item| item["role"] == "system")
        .expect("缺少 system 注入");
    assert_eq!(system["content"][0]["type"], "input_text");

    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("缺少 function_call");
    assert_eq!(call["name"], "read_file");
    assert_eq!(call["call_id"], "c1");
    assert_eq!(call["arguments"], r#"{"path":"README.md"}"#, "arguments 必须是 JSON 字符串");

    // 已经配好对的调用不能再补一条占位输出（否则同一 call_id 出现两条输出）
    let outputs: Vec<_> = input
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 1, "配对完整时不得重复补位：{outputs:?}");
    assert_eq!(outputs[0]["output"], "# README");
}

/// P0 回放：半截工具记录（有调用、没输出）必须在上 wire 前补齐。
///
/// 旧代码原样发出 → 本地端点按真实规则回 400
/// `No tool output found for tool call call_01_X7pFedLZ8pmzxHEd0PmX7825.`；
/// 修复后第一次请求就成功，不靠重试、不丢消息。
#[tokio::test]
async fn unfinished_tool_call_is_repaired_before_the_request() {
    let server = CapturingServer::start();
    let mut stream =
        stream_responses(&server.provider(), "sk-test", &unfinished_call_history(), &[], "SYS", &RequestOptions::default())
            .await
            .expect("修复后第一次请求就该建流成功");
    let events: Vec<ModelStreamEvent> = stream.by_ref().collect().await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelStreamEvent::Completed { .. })),
        "配对补齐后端点应完成：{events:?}"
    );

    let body = server.body();
    let input = body["input"].as_array().expect("input 必须是数组");
    let call_idx = input
        .iter()
        .position(|item| item["type"] == "function_call")
        .expect("调用项必须保留（不能为了让端点闭嘴把调用删掉）");
    assert_eq!(input[call_idx]["call_id"], UNFINISHED_CALL_ID);
    // 补位输出紧跟调用，call_id 一致
    assert_eq!(input[call_idx + 1]["type"], "function_call_output");
    assert_eq!(input[call_idx + 1]["call_id"], UNFINISHED_CALL_ID);
    assert_eq!(input[call_idx + 1]["output"], "（本轮在该工具调用前被取消，未执行）");
    // 后续那条用户消息仍在原位（只是中间多了一条输出）
    assert_eq!(input.last().expect("最后一条")["content"][0]["text"], "继续。");
}
