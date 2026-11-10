//! 模型端点错误的统一分类与人话映射（审计 A0：错误在 UI/日志/导出三处都查不到）。
//!
//! 设计原则：
//! 1. **分类与文案只在一处**：HTTP 状态与错误体 → [`ModelError`]；界面只读 `human`，
//!    不再各自拼“回合以 failed 结束”。
//! 2. **原始信息必须留得住**：`provider_code` / `provider_message` / `http_status`
//!    原样带出（脱敏后），供界面展开与导出对账。
//! 3. **脱敏先于截断**：与 `error_snippet` 同一条纪律，先整串替换凭据再截断。

use std::fmt;

/// 错误大类。前端只认 `code()` 的稳定字符串，不依赖具体 HTTP 码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelErrorKind {
    /// 401/403：密钥无效或无权限。
    Auth,
    /// 402，或错误体点名余额/额度不足。
    Quota,
    /// 429：触发限流。
    RateLimited,
    /// 建连/首包/流空闲超时。
    Timeout,
    /// 连接失败、SSE 断流等网络问题。
    Network,
    /// 5xx：服务端错误。
    Server,
    /// 404/405：该地址没有这个接口（常见于把 base_url 指到只支持 chat 的端点）。
    EndpointUnsupported,
    /// 400/422：请求本身被拒。
    BadRequest,
    /// 其它。
    Unknown,
}

/// 一次模型端点失败的完整描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub http_status: Option<u16>,
    /// 端点错误体里的 `error.code` / `error.type`（脱敏后）。
    pub provider_code: Option<String>,
    /// 端点错误体里的 `error.message`（脱敏后，已截断）。
    pub provider_message: Option<String>,
    /// 给用户看的一句话。
    pub human: String,
}

impl ModelError {
    /// 组合一个错误；`provider_code` / `provider_message` 已经过脱敏。
    pub fn new(
        kind: ModelErrorKind,
        http_status: Option<u16>,
        provider_code: Option<String>,
        provider_message: Option<String>,
        human: String,
    ) -> Self {
        Self { kind, http_status, provider_code, provider_message, human }
    }

    /// 稳定错误码（前端按它做本地化与统计，不要依赖 HTTP 码）。
    pub fn code(&self) -> &'static str {
        match self.kind {
            ModelErrorKind::Auth => "AUTH",
            ModelErrorKind::Quota => "QUOTA",
            ModelErrorKind::RateLimited => "RATE_LIMIT",
            ModelErrorKind::Timeout => "TIMEOUT",
            ModelErrorKind::Network => "NETWORK",
            ModelErrorKind::Server => "SERVER",
            ModelErrorKind::EndpointUnsupported => "ENDPOINT_UNSUPPORTED",
            ModelErrorKind::BadRequest => "BAD_REQUEST",
            ModelErrorKind::Unknown => "UNKNOWN",
        }
    }

    /// HTTP 状态 + 错误体 → 结构化错误。
    ///
    /// 分类顺序：先看错误体里的额度关键词（有些端点用 400/403 报余额不足），
    /// 再看状态码。这样“余额不足”不会被误报成“密钥无效”。
    pub fn from_status(status: u16, body: &str) -> Self {
        let (provider_code, provider_message) = provider_fields(body);
        let kind = if quota_hit(provider_code.as_deref(), provider_message.as_deref()) {
            ModelErrorKind::Quota
        } else {
            match status {
                401 | 403 => ModelErrorKind::Auth,
                402 => ModelErrorKind::Quota,
                404 | 405 => ModelErrorKind::EndpointUnsupported,
                429 => ModelErrorKind::RateLimited,
                500..=599 => ModelErrorKind::Server,
                400 | 422 => ModelErrorKind::BadRequest,
                _ => ModelErrorKind::Unknown,
            }
        };
        let human = match kind {
            ModelErrorKind::Auth => "密钥无效".to_string(),
            ModelErrorKind::Quota => "余额不足".to_string(),
            ModelErrorKind::RateLimited => "请求过于频繁，已被限流".to_string(),
            ModelErrorKind::Timeout => "网络超时".to_string(),
            ModelErrorKind::Network => "无法连接到模型服务".to_string(),
            ModelErrorKind::Server => format!("模型服务端错误（HTTP {status}）"),
            ModelErrorKind::EndpointUnsupported => format!("该地址不支持此接口（HTTP {status}）"),
            ModelErrorKind::BadRequest => format!("请求被拒绝（HTTP {status}）"),
            ModelErrorKind::Unknown => format!("未知错误（HTTP {status}）"),
        };
        Self::new(kind, Some(status), provider_code, provider_message, human)
    }

    /// 流内错误体（SSE 的 `error` / `response.failed` 事件）→ 结构化错误。
    ///
    /// 这类事件没有 HTTP 状态码，只能按错误体里的关键词分类；取不到就不猜。
    /// 用户可见的一句话优先用端点原文（已脱敏），它比"未知错误"更有信息量。
    pub fn from_body(value: Option<&serde_json::Value>) -> Self {
        let (provider_code, provider_message) = value.map(provider_fields_value).unwrap_or((None, None));
        let kind = if quota_hit(provider_code.as_deref(), provider_message.as_deref()) {
            ModelErrorKind::Quota
        } else {
            ModelErrorKind::Unknown
        };
        let human = match kind {
            ModelErrorKind::Quota => "余额不足".to_string(),
            _ => provider_message.clone().unwrap_or_else(|| "模型服务返回错误".to_string()),
        };
        Self::new(kind, None, provider_code, provider_message, human)
    }

    /// reqwest 传输错误 → 分类（超时 / 连接失败 / 其它网络问题）。
    pub fn from_reqwest(error: &reqwest::Error) -> Self {
        let (kind, human) = if error.is_timeout() {
            (ModelErrorKind::Timeout, "网络超时".to_string())
        } else if error.is_connect() {
            (ModelErrorKind::Network, "无法连接到模型服务".to_string())
        } else {
            (ModelErrorKind::Network, "模型请求失败".to_string())
        };
        Self::new(kind, None, None, Some(sanitize(&error.to_string())), human)
    }

    /// 本地限时（首包/流空闲）→ 超时。
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ModelErrorKind::Timeout, None, None, Some(sanitize(&message.into())), "网络超时".to_string())
    }

    /// SSE 断流等流内错误 → 网络问题。
    pub fn stream_broken(message: impl Into<String>) -> Self {
        Self::new(
            ModelErrorKind::Network,
            None,
            None,
            Some(sanitize(&message.into())),
            "模型连接中断".to_string(),
        )
    }

    /// 构造 HTTP 客户端失败（配置问题，不是网络问题）。
    pub fn client_build(message: impl Into<String>) -> Self {
        Self::new(
            ModelErrorKind::Unknown,
            None,
            None,
            Some(sanitize(&message.into())),
            "无法初始化模型请求".to_string(),
        )
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.human)
    }
}

impl std::error::Error for ModelError {}

/// 允许测试桩与旧的字符串错误无缝接入（生产路径一律显式构造分类）。
impl From<String> for ModelError {
    fn from(message: String) -> Self {
        Self::new(
            ModelErrorKind::Unknown,
            None,
            None,
            Some(sanitize(&message)),
            message,
        )
    }
}

impl From<&str> for ModelError {
    fn from(message: &str) -> Self {
        Self::from(message.to_string())
    }
}

/// 错误体里的额度/余额关键词（部分端点用 400/403 报余额不足）。
fn quota_hit(code: Option<&str>, message: Option<&str>) -> bool {
    let haystack = format!("{} {}", code.unwrap_or_default(), message.unwrap_or_default()).to_ascii_lowercase();
    [
        "insufficient_quota",
        "insufficient_balance",
        "insufficient balance",
        "quota_exceeded",
        "quota exceeded",
        "billing_hard_limit_reached",
        "余额不足",
        "额度不足",
    ]
    .iter()
    .any(|needle| haystack.contains(needle))
}

impl From<&ModelError> for crate::events::TurnErrorInfo {
    fn from(error: &ModelError) -> Self {
        Self {
            code: error.code().to_string(),
            message: error.human.clone(),
            http_status: error.http_status,
            provider_code: error.provider_code.clone(),
            provider_message: error.provider_message.clone(),
        }
    }
}

/// 错误体里提取 `code` / `message`。
///
/// 兼容三种常见形态：`{"error":{"code","message"}}`、`{"error":{"type","message"}}`、
/// 顶层 `{"code","message"}`（部分自建网关）。取不到就返回 `None`，不猜。
fn provider_fields(body: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, None);
    };
    provider_fields_value(&value)
}

/// 同 [`provider_fields`]，输入已是解析后的 JSON（流内事件用）。
fn provider_fields_value(value: &serde_json::Value) -> (Option<String>, Option<String>) {
    let error = value.get("error").unwrap_or(value);
    let code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(json_scalar);
    let message = error.get("message").and_then(json_scalar);
    (code.map(|text| sanitize(&text)), message.map(|text| sanitize(&text)))
}

/// JSON 标量转字符串（数字型 code 也保留）。
fn json_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// 脱敏 → 截断到 300 字符（顺序不可交换，理由同 `error_snippet`）。
fn sanitize(text: &str) -> String {
    crate::config::redact(text).chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::{ModelError, ModelErrorKind};

    fn kind(status: u16, body: &str) -> ModelErrorKind {
        ModelError::from_status(status, body).kind
    }

    /// 反向对照：删掉任意一个状态分支，对应断言必红。
    #[test]
    fn status_maps_to_kind() {
        assert_eq!(kind(401, "{}"), ModelErrorKind::Auth);
        assert_eq!(kind(403, "{}"), ModelErrorKind::Auth);
        assert_eq!(kind(402, "{}"), ModelErrorKind::Quota);
        assert_eq!(kind(404, "{}"), ModelErrorKind::EndpointUnsupported);
        assert_eq!(kind(405, "{}"), ModelErrorKind::EndpointUnsupported);
        assert_eq!(kind(429, "{}"), ModelErrorKind::RateLimited);
        assert_eq!(kind(500, "{}"), ModelErrorKind::Server);
        assert_eq!(kind(503, "{}"), ModelErrorKind::Server);
        assert_eq!(kind(400, "{}"), ModelErrorKind::BadRequest);
        assert_eq!(kind(422, "{}"), ModelErrorKind::BadRequest);
        assert_eq!(kind(418, "{}"), ModelErrorKind::Unknown);
    }

    /// 额度不足优先于状态码：403 + insufficient_quota 必须是“余额不足”，
    /// 不能被报成“密钥无效”。
    #[test]
    fn quota_keyword_wins_over_status() {
        let body = r#"{"error":{"code":"insufficient_quota","message":"You exceeded your quota"}}"#;
        let error = ModelError::from_status(403, body);
        assert_eq!(error.kind, ModelErrorKind::Quota);
        assert_eq!(error.human, "余额不足");
        assert_eq!(error.http_status, Some(403));
        assert_eq!(error.provider_code.as_deref(), Some("insufficient_quota"));
    }

    #[test]
    fn provider_fields_extracts_code_and_message() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"bad model"}}"#;
        let error = ModelError::from_status(400, body);
        assert_eq!(error.provider_code.as_deref(), Some("invalid_request_error"));
        assert_eq!(error.provider_message.as_deref(), Some("bad model"));
    }

    #[test]
    fn non_json_body_keeps_status_only() {
        let error = ModelError::from_status(502, "<html>bad gateway</html>");
        assert_eq!(error.kind, ModelErrorKind::Server);
        assert_eq!(error.provider_code, None);
        assert_eq!(error.provider_message, None);
    }

    /// 审计 M9 同款：错误体回显凭据时，提取到的字段必须已脱敏。
    #[test]
    fn provider_message_is_redacted() {
        let secret = "opaque-error-echo-a0-0001";
        crate::config::register_secret(secret);
        let body = format!(r#"{{"error":{{"code":"invalid_api_key","message":"bad key {secret}"}}}}"#);
        let error = ModelError::from_status(401, &body);
        let message = error.provider_message.expect("应提取 message");
        assert!(!message.contains(secret), "错误体回显了凭据: {message}");
        assert!(message.contains(crate::config::REDACTED));
    }
}
