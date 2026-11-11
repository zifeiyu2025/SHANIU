//! 密钥三态校验（A0-1）。
//!
//! 保存密钥后打一次 `GET {base_url}/models`：这个端点不产生推理费用，只回答
//! "这个 Key 能不能被端点接受"。结果分三态：
//!
//! - `valid`：端点接受了密钥；
//! - `invalid`：端点明确拒绝（401/403，或错误体点名密钥/权限）；
//! - `unknown`：**无法判断**——网络不通、超时、限流、服务端错误、端点根本没有
//!   模型列表接口。这一态绝不能显示成"无效"或"有效"，否则用户会照着错误结论
//!   去改一个本来正确的 Key。
//!
//! 错误分类复用 [`ModelError`](crate::model_client::ModelError)：状态码与错误体的
//! 映射只在一处维护，原始 `code` / `message`（脱敏后）随结果带出，供界面展开与
//! 日志对账。密钥本身**永不**进入结果、日志或错误消息。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::Manager;

use crate::model_client::{ModelError, ModelErrorKind};

/// 校验请求上限：`/models` 没有正当理由慢慢响应；8 秒足够区分"网络不通"。
const CHECK_TIMEOUT_SECS: u64 = 8;
/// 响应体解析上限：只取错误体/模型列表头部，避免超大响应拖垮界面。
const MAX_CHECK_BODY_BYTES: usize = 256 * 1024;

/// 三态结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCheckState {
    /// 端点接受了密钥。
    Valid,
    /// 端点明确拒绝。
    Invalid,
    /// 判断不了（网络/限流/端点不支持/服务端错误）。不等于无效。
    Unknown,
}

/// 一次校验的完整结果（camelCase 出到前端）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyCheck {
    pub state: KeyCheckState,
    /// 稳定原因码：`ok` / `key_invalid` / `missing` / `no_provider` / `network` /
    /// `timeout` / `endpoint_unsupported` / `rate_limited` / `server` / `quota` /
    /// `bad_request` / `storage` / `no_endpoint` / `unknown`。
    pub reason: String,
    /// 一句人话。
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// 端点错误体的 `code` / `type`（脱敏后）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    /// 端点错误体的 `message`（脱敏、截断）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_message: Option<String>,
    /// 本次请求耗时（毫秒）；没发出请求时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

impl KeyCheck {
    fn new(state: KeyCheckState, reason: &str, message: impl Into<String>) -> Self {
        Self {
            state,
            reason: reason.to_string(),
            message: message.into(),
            http_status: None,
            provider_code: None,
            provider_message: None,
            latency_ms: None,
        }
    }

    fn with_latency(mut self, latency: Duration) -> Self {
        self.latency_ms = Some(latency.as_millis() as u64);
        self
    }
}

/// 端点校验的核心实现。`api_key` 必须非空（调用方已判断）；错误永不携带密钥。
pub async fn check_endpoint_key(
    base_url: &str,
    headers: &BTreeMap<String, String>,
    api_key: &str,
) -> KeyCheck {
    let base = base_url.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return KeyCheck::new(
            KeyCheckState::Unknown,
            "no_endpoint",
            "端点地址必须以 http:// 或 https:// 开头，暂时无法校验",
        );
    }
    let url = format!("{base}/models");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(CHECK_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return KeyCheck::new(
                KeyCheckState::Unknown,
                "unknown",
                format!("无法初始化校验请求：{error}"),
            );
        }
    };

    let started = Instant::now();
    let mut request = client.get(&url).header("accept", "application/json");
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = match request.bearer_auth(api_key).send().await {
        Ok(response) => response,
        Err(error) => {
            return from_model_error(ModelError::from_reqwest(&error))
                .with_latency(started.elapsed());
        }
    };

    let status = response.status().as_u16();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            // 读响应失败（连接被对端掐断等）：同样只能算"判断不了"。
            return from_model_error(ModelError::from_reqwest(&error))
                .with_latency(started.elapsed());
        }
    };
    let body = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_CHECK_BODY_BYTES)]);

    if !(200..300).contains(&status) {
        return from_model_error(ModelError::from_status(status, &body))
            .with_latency(started.elapsed());
    }

    // 2xx 还不算结论：登录页、网关首页也可能回 200。只有"看起来是模型清单"
    // 才说明端点真的回答了这个问题；否则归入"端点不支持"，不冒充有效。
    if model_list_present(&body) {
        KeyCheck::new(KeyCheckState::Valid, "ok", "密钥有效")
            .with_latency(started.elapsed())
    } else {
        let mut check = KeyCheck::new(
            KeyCheckState::Unknown,
            "endpoint_unsupported",
            "端点有响应，但返回的不是模型列表，无法据此判断密钥是否有效",
        );
        check.http_status = Some(status);
        check.with_latency(started.elapsed())
    }
}

/// 错误分类 → 三态结论。只有 `Auth` 判无效，其余一律"判断不了"。
fn from_model_error(error: ModelError) -> KeyCheck {
    let (state, reason, message): (KeyCheckState, &str, String) = match error.kind {
        ModelErrorKind::Auth => (
            KeyCheckState::Invalid,
            "key_invalid",
            "API Key 无效或没有访问权限".to_string(),
        ),
        ModelErrorKind::Quota => (
            KeyCheckState::Unknown,
            "quota",
            "端点报余额/额度不足，暂时无法确认密钥".to_string(),
        ),
        ModelErrorKind::RateLimited => (
            KeyCheckState::Unknown,
            "rate_limited",
            "端点正在限流，稍后再校验".to_string(),
        ),
        ModelErrorKind::Timeout => (
            KeyCheckState::Unknown,
            "timeout",
            "连接端点超时，网络可能不通".to_string(),
        ),
        ModelErrorKind::Network => (
            KeyCheckState::Unknown,
            "network",
            "无法连接到端点（网络不通或地址不对）".to_string(),
        ),
        ModelErrorKind::Server => (
            KeyCheckState::Unknown,
            "server",
            format!("端点服务端错误（HTTP {}），暂时无法校验", error.http_status.unwrap_or(0)),
        ),
        ModelErrorKind::EndpointUnsupported => (
            KeyCheckState::Unknown,
            "endpoint_unsupported",
            "该端点不提供模型列表接口，无法在线校验（不代表密钥无效）".to_string(),
        ),
        ModelErrorKind::BadRequest => (
            KeyCheckState::Unknown,
            "bad_request",
            "端点拒绝了这次查询，无法据此判断密钥".to_string(),
        ),
        ModelErrorKind::EmptyResponse => (
            KeyCheckState::Unknown,
            "empty_response",
            "端点返回了空响应，无法据此判断密钥".to_string(),
        ),
        ModelErrorKind::Unknown => (KeyCheckState::Unknown, "unknown", error.human.clone()),
    };
    KeyCheck {
        state,
        reason: reason.to_string(),
        message,
        http_status: error.http_status,
        provider_code: error.provider_code,
        provider_message: error.provider_message,
        latency_ms: None,
    }
}

/// 2xx 响应是否像模型清单（`data` / `models` 数组，元素带 `id`）。
fn model_list_present(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let items = value.get("data").and_then(serde_json::Value::as_array);
    let items = items.or_else(|| value.get("models").and_then(serde_json::Value::as_array));
    items.is_some_and(|list| list.iter().all(|item| item.get("id").is_some()))
}

/// Tauri 命令：按服务商 id 读取已存密钥与端点，做一次免费用校验。
///
/// 返回 `KeyCheck` 而不是 `Result<_, String>`：每一种失败都是**结论**，界面要能
/// 区分"密钥错"和"网络不通"，把它们都折叠成一条错误字符串就回到 A0 之前了。
#[tauri::command]
pub async fn check_api_key(app: tauri::AppHandle, provider: String) -> KeyCheck {
    let state = app.state::<crate::app_state::AppState>();
    let cfg = state.config();
    let provider_id = provider.trim();
    let Some(row) = cfg.providers.iter().find(|p| p.id == provider_id) else {
        return KeyCheck::new(
            KeyCheckState::Unknown,
            "no_provider",
            "设置里找不到这家服务商，先保存服务商配置再校验",
        );
    };
    let stored = match crate::config::SecretStore::new(&state.config_root()).load_key(&row.secret_name()) {
        Ok(key) => key,
        Err(error) => {
            return KeyCheck::new(
                KeyCheckState::Unknown,
                "storage",
                format!("读取本机密钥失败：{error}"),
            );
        }
    };
    let Some(key) = stored.filter(|key| !key.trim().is_empty()) else {
        return KeyCheck::new(
            KeyCheckState::Unknown,
            "missing",
            "还没有保存 API Key",
        );
    };
    if row.base_url.trim().is_empty() {
        return KeyCheck::new(
            KeyCheckState::Unknown,
            "no_endpoint",
            "还没有填写端点地址，无法在线校验",
        );
    }
    check_endpoint_key(&row.base_url, &row.headers, &key).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 单请求桩：读掉请求头后按给定状态与正文应答。
    fn stub(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let url = format!("http://{}", listener.local_addr().expect("取地址"));
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                let response = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        url
    }

    fn run(url: &str) -> KeyCheck {
        let runtime = tokio::runtime::Runtime::new().expect("运行时");
        runtime.block_on(check_endpoint_key(url, &BTreeMap::new(), "test-key"))
    }

    /// 反向对照：删掉 `model_list_present` 判断，200 的空壳响应会被判成有效。
    #[test]
    fn model_list_means_valid() {
        let url = stub("200 OK", r#"{"data":[{"id":"m1"}]}"#);
        let result = run(&url);
        assert_eq!(result.state, KeyCheckState::Valid);
        assert_eq!(result.reason, "ok");
    }

    /// 反向对照：把 Auth 也归到 Unknown，这个断言必红（配错与网络不通混为一谈）。
    #[test]
    fn unauthorized_means_invalid_with_provider_fields() {
        let url = stub(
            "401 Unauthorized",
            r#"{"error":{"code":"invalid_api_key","message":"Incorrect API key provided"}}"#,
        );
        let result = run(&url);
        assert_eq!(result.state, KeyCheckState::Invalid);
        assert_eq!(result.reason, "key_invalid");
        assert_eq!(result.http_status, Some(401));
        assert_eq!(result.provider_code.as_deref(), Some("invalid_api_key"));
        assert_eq!(result.provider_message.as_deref(), Some("Incorrect API key provided"));
    }

    /// 端点没有 /models：必须说"判断不了"，不能判无效。
    #[test]
    fn missing_models_endpoint_is_unknown() {
        let url = stub("404 Not Found", r#"{"error":{"message":"not found"}}"#);
        let result = run(&url);
        assert_eq!(result.state, KeyCheckState::Unknown);
        assert_eq!(result.reason, "endpoint_unsupported");
        assert!(result.message.contains("不代表密钥无效"), "{}", result.message);
    }

    /// 200 但不是模型清单（网关首页/登录页）：同样是"判断不了"。
    #[test]
    fn html_200_is_not_valid() {
        let url = stub("200 OK", "<html>welcome</html>");
        let result = run(&url);
        assert_eq!(result.state, KeyCheckState::Unknown);
        assert_eq!(result.reason, "endpoint_unsupported");
    }

    /// 连不上：network，且不能带出密钥。
    #[test]
    fn connection_refused_is_network() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let url = format!("http://{}", listener.local_addr().expect("取地址"));
        drop(listener); // 端口立刻关闭，后续连接被拒
        let result = run(&url);
        assert_eq!(result.state, KeyCheckState::Unknown);
        assert_eq!(result.reason, "network");
        assert!(!result.message.contains("test-key"));
        assert_ne!(result.message, "API Key 无效或没有访问权限");
    }
}
