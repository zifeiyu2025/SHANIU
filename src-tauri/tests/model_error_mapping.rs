//! 审计 A0-2：端点错误必须映射成一句人话，且原始 code/message 在整个传递链上
//! 保持可对账（脱敏后）。
//!
//! 这里用真实 HTTP 往返测 `stream_body`，而不是只测 `from_status`：401 的映射
//! 失败过一次（UI/日志/导出三处都没有 401），只测纯函数会漏掉"非 2xx 分支
//! 根本没构造结构化错误"这类断链。

use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use shaniu_lib::model_client::{stream_body, ModelErrorKind, ProviderConfig};

const SECRET: &str = "sk-test-leak-canary-12345678";

/// 一次性本地端点：读完请求头后回一段固定响应。
struct LocalServer {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl LocalServer {
    fn start(status_line: &'static str, body: &'static str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Windows 上 accept 出的 socket 会继承非阻塞标志，必须切回阻塞。
                        stream.set_nonblocking(false).unwrap();
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        let response = format!(
                            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
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
        Self { port, stop }
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
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[tokio::test]
async fn unauthorized_maps_to_auth_and_keeps_redacted_detail() {
    shaniu_lib::config::register_secret(SECRET);
    let server = LocalServer::start(
        "401 Unauthorized",
        r#"{"error":{"code":"invalid_api_key","message":"Incorrect API key provided: sk-test-leak-canary-12345678"}}"#,
    );
    let error = match stream_body(&server.provider(), SECRET, serde_json::json!({})).await {
        Ok(_) => panic!("401 必须返回错误"),
        Err(error) => error,
    };

    assert_eq!(error.kind, ModelErrorKind::Auth);
    assert_eq!(error.human, "密钥无效");
    assert_eq!(error.http_status, Some(401));
    assert_eq!(error.provider_code.as_deref(), Some("invalid_api_key"));
    let detail = error.provider_message.clone().unwrap_or_default();
    // 正向：原始 message 保留下来供展开/复制。
    assert!(detail.contains("Incorrect API key"), "应保留端点原文：{detail}");
    // 反向对照：撤掉脱敏，密钥会原样漏进 UI、日志与导出。
    assert!(detail.contains("[REDACTED]"), "密钥必须脱敏：{detail}");
    assert!(!detail.contains(SECRET), "密钥不得以明文出现：{detail}");
}

#[tokio::test]
async fn quota_in_400_body_wins_over_bad_request() {
    let server = LocalServer::start(
        "400 Bad Request",
        r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#,
    );
    let error = match stream_body(&server.provider(), "sk-x", serde_json::json!({})).await {
        Ok(_) => panic!("400 必须返回错误"),
        Err(error) => error,
    };

    // 反向对照：只看状态码会把它归成 BAD_REQUEST，用户看到"请求被拒绝"而不是"余额不足"。
    assert_eq!(error.kind, ModelErrorKind::Quota);
    assert_eq!(error.human, "余额不足");
    assert_eq!(error.http_status, Some(400));
    assert_eq!(error.provider_code.as_deref(), Some("insufficient_quota"));
}

#[tokio::test]
async fn unsupported_endpoint_is_distinguishable() {
    let server = LocalServer::start("404 Not Found", "<html>not found</html>");
    let error = match stream_body(&server.provider(), "sk-x", serde_json::json!({})).await {
        Ok(_) => panic!("404 必须返回错误"),
        Err(error) => error,
    };

    assert_eq!(error.kind, ModelErrorKind::EndpointUnsupported);
    assert_eq!(error.human, "该地址不支持此接口（HTTP 404）");
    assert!(error.provider_message.is_none(), "非 JSON 错误体不得被臆造出 message");
}
