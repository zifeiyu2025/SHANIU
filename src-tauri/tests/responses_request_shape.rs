//! Responses 协议请求体的形状守门（2026-11-15 评测线实测缺陷）。
//!
//! 现象：按统一 Responses 口径跑评测，4 轮 0 文件改动，模型反复表示「这条消息里没有
//! 具体任务内容」；同一题换 chat 协议立刻通过。根因在 `responses_input()`：它把
//! `HistoryItem` 的 serde 形态原样发出（`{"role":"user","text":"…"}`），而 Responses 的
//! message 输入项只认 `content` 数组——`text` 是陌生键，端点不报错、直接忽略，用户消息
//! 就此静默消失。
//!
//! 这里走真实 HTTP 往返而不只断言 `build_responses_request` 的返回值：只测纯函数会漏掉
//! 「构造对了、发送时又换成另一份 body」这类断链，也看不到请求体在 wire 上的实际形状。

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use shaniu_lib::model_client::{
    stream_responses, HistoryItem, ModelStreamEvent, ProviderConfig, Role,
};

const TASK: &str = "把 README 第三段译成中文，写到 docs/readme.zh.md";

/// 一次性本地端点：留档请求体，再回一段合法的 Responses SSE。
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
            *captured.lock().expect("锁") = serde_json::from_str(&payload).ok();
            let sse = concat!(
                "event: response.output_text.delta\ndata: {\"delta\":\"ok\"}\n\n",
                "event: response.completed\n",
                "data: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
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

/// 缺陷本体：用户任务文本必须出现在 `content` 数组里，而不是同级的 `text` 字段。
#[tokio::test]
async fn user_text_reaches_the_wire_inside_the_content_array() {
    let server = CapturingServer::start();
    let mut stream = stream_responses(&server.provider(), "sk-test", &history(), &[], "SYS")
        .await
        .expect("本地端点应成功建流");
    let events: Vec<ModelStreamEvent> = stream.by_ref().collect().await;
    // 流本身要能走完：请求构造错时端点常常直接 400，这里先把传输面排除掉
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
    // 反向对照：改回 `text` 字段时，下面这两条断言就是红的落点
    assert!(user.get("text").is_none(), "用户文本发到了同级 text 字段：{user}");
    assert_eq!(user["content"][0]["type"], "input_text");
    assert_eq!(user["content"][0]["text"], TASK);

    // 全树扫一遍：任何 input 项都不许出现响应端不认的外部 `text`
    for item in input {
        assert!(item.get("text").is_none(), "input 项含陌生 text 字段：{item}");
    }
}

/// 其余角色/条目一并核对：assistant 文本、system 注入、工具调用与工具输出。
#[tokio::test]
async fn other_roles_and_tool_items_use_responses_shapes() {
    let server = CapturingServer::start();
    let mut stream = stream_responses(&server.provider(), "sk-test", &history(), &[], "SYS")
        .await
        .expect("本地端点应成功建流");
    let _: Vec<ModelStreamEvent> = stream.by_ref().collect().await;

    let body = server.body();
    let input = body["input"].as_array().expect("input 必须是数组");
    let item = |role: &str| {
        input
            .iter()
            .find(|item| item["role"] == role)
            .unwrap_or_else(|| panic!("缺少 role={role} 的条目：{input:?}"))
    };

    // system 注入：同样是 message + content 数组，否则提醒会被静默吞掉
    assert_eq!(item("system")["type"], "message");
    assert_eq!(item("system")["content"][0]["type"], "input_text");
    assert_eq!(item("system")["content"][0]["text"], "环境快照");

    // assistant 文本：codex 同口径（ContentItem::OutputText）
    assert_eq!(item("assistant")["type"], "message");
    assert_eq!(item("assistant")["content"][0]["type"], "output_text");
    assert_eq!(item("assistant")["content"][0]["text"], "先读 README。");
    // reasoning 回放要独立的 reasoning item（带 encrypted_content）；明文塞进消息项是陌生键
    assert!(item("assistant").get("reasoning").is_none(), "{input:?}");

    // 工具调用：arguments 是 JSON **字符串**，不是对象
    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("缺少 function_call");
    assert_eq!(call["name"], "read_file");
    assert_eq!(call["call_id"], "c1");
    assert_eq!(call["arguments"], r#"{"path":"README.md"}"#);

    // 工具输出：output 是字符串
    let output = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("缺少 function_call_output");
    assert_eq!(output["call_id"], "c1");
    assert_eq!(output["output"], "# README");

    // 工具声明与 Responses 的扁平形状一致（`function` 对象是 chat 的形状）
    assert_eq!(body["stream"], true);
}
