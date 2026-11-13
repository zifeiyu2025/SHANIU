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
    /// 400/413/422：请求本身被拒（413 为请求体过大）。
    BadRequest,
    /// 流正常结束，但没有任何增量、没有工具调用、也没有 usage。
    EmptyResponse,
    /// 上下文超出模型窗口（确定性失败，不重试）。
    ContextOverflow,
    /// 请求/回答被内容安全策略拦截（确定性失败，不重试）。
    ContentFiltered,
    /// 端点明确表示回答未完成（`response.incomplete`，如达到 max_tokens / 内容过滤）。
    Incomplete,
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
    /// 端点 `Retry-After` 响应头（毫秒）。只有非 2xx 响应才可能有；解析不出即 `None`。
    /// 重试策略据此优先于指数退避（`model_client/retry.rs`）。
    pub retry_after_ms: Option<u64>,
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
        Self { kind, http_status, provider_code, provider_message, human, retry_after_ms: None }
    }

    /// 带上端点的 `Retry-After`（毫秒）。重试策略据此优先于指数退避。
    pub fn with_retry_after(mut self, retry_after_ms: Option<u64>) -> Self {
        self.retry_after_ms = retry_after_ms;
        self
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
            ModelErrorKind::EmptyResponse => "EMPTY_RESPONSE",
            ModelErrorKind::ContextOverflow => "CONTEXT_OVERFLOW",
            ModelErrorKind::ContentFiltered => "CONTENT_FILTERED",
            ModelErrorKind::Incomplete => "INCOMPLETE",
            ModelErrorKind::Unknown => "UNKNOWN",
        }
    }

    /// HTTP 状态 + 错误体 → 结构化错误。
    ///
    /// 分类顺序：先看错误体里的额度关键词（有些端点用 400/403 报余额不足），
    /// 再看 400/413/422 能否从错误体里判出更具体的原因（上下文超长/内容过滤），
    /// 最后才按状态码兜底。这样“余额不足”不会被误报成“密钥无效”，
    /// 真实端点用 HTTP 400 + `context_length_exceeded` 报超长时也能走到反应式压缩。
    pub fn from_status(status: u16, body: &str) -> Self {
        let (provider_code, provider_message) = provider_fields(body);
        let kind = if quota_hit(provider_code.as_deref(), provider_message.as_deref()) {
            ModelErrorKind::Quota
        } else if matches!(status, 400 | 413 | 422) {
            // 只接受错误体**明确**判出的原因；参数错误、纯文本错误体、判不出来的
            // 一律保持 BAD_REQUEST。宁可漏判，也不能错判成超长——错判会触发一次
            // 无意义的摘要与重放，白烧额度。413（Payload Too Large）只认超长：
            // 内容过滤的措辞出现在 413 里，不足以说明是上下文问题。
            match body_kind(provider_code.as_deref(), provider_message.as_deref()) {
                ModelErrorKind::ContextOverflow => ModelErrorKind::ContextOverflow,
                ModelErrorKind::ContentFiltered if status != 413 => ModelErrorKind::ContentFiltered,
                _ => ModelErrorKind::BadRequest,
            }
        } else {
            match status {
                401 | 403 => ModelErrorKind::Auth,
                402 => ModelErrorKind::Quota,
                404 | 405 => ModelErrorKind::EndpointUnsupported,
                429 => ModelErrorKind::RateLimited,
                500..=599 => ModelErrorKind::Server,
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
            ModelErrorKind::EmptyResponse => "模型返回了空响应".to_string(),
            ModelErrorKind::ContextOverflow => "上下文超出模型窗口限制".to_string(),
            ModelErrorKind::ContentFiltered => "请求被内容安全策略拒绝".to_string(),
            ModelErrorKind::Incomplete => "回答未完成".to_string(),
            ModelErrorKind::Unknown => format!("未知错误（HTTP {status}）"),
        };
        Self::new(kind, Some(status), provider_code, provider_message, human)
    }

    /// 流内错误体（SSE 的 `error` / `response.failed` 事件）→ 结构化错误。
    ///
    /// 这类事件没有 HTTP 状态码。分类只能靠错误体里的 `code` / `type` / `message`；
    /// 数字型 `code`（如 `"503"`）按状态码语义处理。取不到可判定信息就不猜。
    pub fn from_body(value: Option<&serde_json::Value>) -> Self {
        let (provider_code, provider_message) = value.map(provider_fields_value).unwrap_or((None, None));
        let kind = body_kind(provider_code.as_deref(), provider_message.as_deref());
        let human = match kind {
            ModelErrorKind::Quota => "余额不足".to_string(),
            ModelErrorKind::ContextOverflow => "上下文超出模型窗口限制".to_string(),
            ModelErrorKind::ContentFiltered => "请求被内容安全策略拒绝".to_string(),
            ModelErrorKind::Auth => "密钥无效".to_string(),
            ModelErrorKind::RateLimited => "请求过于频繁，已被限流".to_string(),
            ModelErrorKind::Timeout => "网络超时".to_string(),
            ModelErrorKind::Network => "模型连接中断".to_string(),
            ModelErrorKind::Server => "模型服务端错误".to_string(),
            ModelErrorKind::BadRequest => "请求被拒绝".to_string(),
            ModelErrorKind::EmptyResponse => "模型返回了空响应".to_string(),
            ModelErrorKind::Incomplete => "回答未完成".to_string(),
            ModelErrorKind::EndpointUnsupported => "该地址不支持此接口".to_string(),
            // 真的判不出来时才退回端点原文：它比"未知错误"更有信息量。
            ModelErrorKind::Unknown => provider_message
                .clone()
                .unwrap_or_else(|| "模型服务返回错误".to_string()),
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

    /// 流正常结束却什么都没产出 → 空响应（可重试；判定见 `agent_loop`）。
    pub fn empty_response() -> Self {
        Self::new(
            ModelErrorKind::EmptyResponse,
            None,
            None,
            None,
            "模型返回了空响应".to_string(),
        )
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

    /// 流在终态标记（`response.completed` / `finish_reason`）之前结束。
    ///
    /// 与 [`Self::empty_response`] 区分：这里已经有内容，只是没有收到"说完"的信号。
    pub fn stream_truncated() -> Self {
        Self::new(
            ModelErrorKind::Network,
            None,
            None,
            Some("流在完成标记前结束".to_string()),
            "模型连接中断（回答未收到完成标记）".to_string(),
        )
    }

    /// 端点明确表示回答未完成（`response.incomplete`）→ 带出可判定的原因。
    pub fn incomplete(reason: &str) -> Self {
        let lowered = reason.to_ascii_lowercase();
        let human = if (lowered.contains("max") && lowered.contains("token")) || lowered == "length" {
            format!("达到最大输出长度，回答被截断（{reason}）")
        } else if lowered.contains("content") && lowered.contains("filter") {
            format!("回答被内容安全策略过滤（{reason}）")
        } else {
            format!("回答未完成（{reason}）")
        };
        Self::new(
            ModelErrorKind::Incomplete,
            None,
            Some(sanitize(reason)),
            Some(sanitize(reason)),
            human,
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

/// 流内错误体 → 大类（P0-3）。`from_status` 对 400/422 也复用它，避免两套关键词。
///
/// 顺序固定：额度 → 内容过滤 → 上下文 → 限流 → 鉴权 → 超时 → 网络 → 服务端 → 请求格式。
/// 内容过滤必须排在上下文之前：内容策略是确定性拒绝，摘要与重放救不了，
/// 让 "content_filter" + 超长措辞的错误体落到上下文超长只会白烧一次压缩。
/// 之后先判具体原因再看兜底，避免 "context_length_exceeded" 被当成普通 400。
fn body_kind(code: Option<&str>, message: Option<&str>) -> ModelErrorKind {
    let code = code.unwrap_or_default().to_ascii_lowercase();
    let message = message.unwrap_or_default().to_ascii_lowercase();
    let haystack = format!("{code} {message}");
    let hit = |needles: &[&str]| needles.iter().any(|needle| haystack.contains(needle));

    if quota_hit(Some(&code), Some(&message)) {
        return ModelErrorKind::Quota;
    }
    if hit(&[
        "content_filter",
        "content_policy_violation",
        "content policy",
        "safety system",
        "内容过滤",
        "安全策略",
    ]) {
        return ModelErrorKind::ContentFiltered;
    }
    if hit(&[
        "context_length_exceeded",
        "maximum context length",
        "context window",
        "too many tokens",
        "prompt is too long",
        "token limit",
        "上下文超",
        "超出上下文",
    ]) {
        return ModelErrorKind::ContextOverflow;
    }
    if hit(&["rate_limit", "rate limit", "too many requests"]) {
        return ModelErrorKind::RateLimited;
    }
    if hit(&[
        "invalid_api_key",
        "authentication_error",
        "unauthorized",
        "invalid api key",
        "permission_error",
        "invalid token",
        "密钥无效",
    ]) {
        return ModelErrorKind::Auth;
    }
    if hit(&["timeout", "timed out", "etimedout", "deadline exceeded", "超时"]) {
        return ModelErrorKind::Timeout;
    }
    if hit(&[
        "econnreset",
        "econnrefused",
        "epipe",
        "connection reset",
        "connection closed",
        "connection error",
        "network error",
        "broken pipe",
        "stream error",
        "disconnected",
        "连接中断",
        "连接重置",
    ]) {
        return ModelErrorKind::Network;
    }
    if hit(&[
        "server_error",
        "internal_error",
        "internal server error",
        "overloaded",
        "service_unavailable",
        "temporarily unavailable",
        "bad gateway",
        "gateway timeout",
        "upstream error",
    ]) {
        return ModelErrorKind::Server;
    }
    if hit(&[
        "invalid_request",
        "bad_request",
        "invalid request",
        "bad request",
        "unsupported",
        "invalid model",
        "unknown parameter",
    ]) {
        return ModelErrorKind::BadRequest;
    }
    // 数字型 code 按状态码语义兜底（"401" / "429" / "503" 等）。
    if let Ok(status) = code.parse::<u16>() {
        return match status {
            401 | 403 => ModelErrorKind::Auth,
            402 => ModelErrorKind::Quota,
            404 | 405 => ModelErrorKind::EndpointUnsupported,
            429 => ModelErrorKind::RateLimited,
            500..=599 => ModelErrorKind::Server,
            400 | 422 => ModelErrorKind::BadRequest,
            _ => ModelErrorKind::Unknown,
        };
    }
    ModelErrorKind::Unknown
}

impl From<&ModelError> for crate::events::TurnErrorInfo {
    fn from(error: &ModelError) -> Self {
        Self {
            code: error.code().to_string(),
            message: error.human.clone(),
            http_status: error.http_status,
            provider_code: error.provider_code.clone(),
            provider_message: error.provider_message.clone(),
            // 次数由采样循环填（它才知道本轮试了几次）；这里只搬运端点原值。
            attempts: None,
            // 仅在因 `Retry-After` 超过上限而放弃时有值；原样给秒，不四舍五入。
            retry_after_secs: error.retry_after_ms.map(|ms| ms / 1000),
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

    /// 真实端点最常见的形态：HTTP 400 + 错误体点名上下文超长。
    /// 判据见 `evals/repo/fixtures/repo_feat_context_overflow.rs` 与失败面题
    /// `fail-context-overflow-http400`：必须落到独立分类，才能走反应式压缩。
    #[test]
    fn http_400_context_overflow_body_maps_to_context_overflow() {
        let body = r#"{"error":{"code":"context_length_exceeded","message":"This model's maximum context length is 8192 tokens."}}"#;
        let error = ModelError::from_status(400, body);
        assert_eq!(error.kind, ModelErrorKind::ContextOverflow);
        assert_eq!(error.code(), "CONTEXT_OVERFLOW");
        assert_eq!(error.http_status, Some(400));
        assert_eq!(error.provider_code.as_deref(), Some("context_length_exceeded"));
        assert!(!error.human.is_empty());
    }

    /// 普通坏请求（参数、模型名等）不得被错误体关键词带偏——宁可漏判，
    /// 也不能判成超长去压缩重放。
    #[test]
    fn http_400_plain_bad_request_stays_bad_request() {
        for body in [
            r#"{"error":{"code":"invalid_request_error","message":"model not found"}}"#,
            // 错误体能判成限流，但 400 不是限流形态：只接受超长/内容过滤，其余保持原语义。
            r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#,
            "<html>bad request</html>",
        ] {
            let error = ModelError::from_status(400, body);
            assert_eq!(error.kind, ModelErrorKind::BadRequest, "错误体: {body}");
            assert_eq!(error.code(), "BAD_REQUEST");
        }
    }

    /// 422 与 400 同一类语义（请求本身被拒），错误体点名超长时同样要识别。
    #[test]
    fn http_422_context_overflow_body_maps_to_context_overflow() {
        let body = r#"{"error":{"message":"prompt is too long: 300000 tokens > 128000 maximum"}}"#;
        assert_eq!(kind(422, body), ModelErrorKind::ContextOverflow);
    }

    /// 413 只认错误体明确指向超长的形态；其余（含仅有内容过滤措辞）保持坏请求。
    #[test]
    fn http_413_only_explicit_overflow_body_maps_to_context_overflow() {
        let overflow = r#"{"error":{"message":"too many tokens in request"}}"#;
        assert_eq!(kind(413, overflow), ModelErrorKind::ContextOverflow);

        let plain = r#"{"error":{"message":"payload too large"}}"#;
        assert_eq!(kind(413, plain), ModelErrorKind::BadRequest);

        let filtered = r#"{"error":{"code":"content_filter","message":"blocked by policy"}}"#;
        assert_eq!(kind(413, filtered), ModelErrorKind::BadRequest);
    }

    /// 内容过滤优先于超长：内容策略是确定性拒绝，压缩救不了。
    /// 错误体同时带策略词与超长措辞时，必须判内容过滤，不得触发压缩。
    #[test]
    fn content_filter_wins_over_context_overflow() {
        let body =
            r#"{"error":{"code":"content_filter","message":"blocked by content policy; prompt is too long"}}"#;
        let error = ModelError::from_status(400, body);
        assert_eq!(error.kind, ModelErrorKind::ContentFiltered);
        assert_ne!(error.kind, ModelErrorKind::ContextOverflow);
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

    /// P0-3：流内错误体没有 HTTP 状态码，必须按 `code` / `message` 里的可判定信息分类。
    ///
    /// 反向对照：旧实现除余额外一律 `Unknown`，下面每一条都会红在 `kind` 上。
    #[test]
    fn stream_error_body_is_classified_by_actionable_fields() {
        let cases: &[(&str, ModelErrorKind)] = &[
            (
                r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#,
                ModelErrorKind::RateLimited,
            ),
            (
                r#"{"error":{"code":"context_length_exceeded","message":"This model's maximum context length is 128000 tokens"}}"#,
                ModelErrorKind::ContextOverflow,
            ),
            (
                r#"{"error":{"type":"server_error","message":"upstream exploded"}}"#,
                ModelErrorKind::Server,
            ),
            (
                r#"{"error":{"code":"overloaded_error","message":"Overloaded"}}"#,
                ModelErrorKind::Server,
            ),
            (r#"{"message":"upstream request timeout"}"#, ModelErrorKind::Timeout),
            (r#"{"message":"connection reset by peer"}"#, ModelErrorKind::Network),
            (
                r#"{"error":{"code":"invalid_api_key","message":"bad key"}}"#,
                ModelErrorKind::Auth,
            ),
            (
                r#"{"error":{"code":"invalid_request_error","message":"bad model"}}"#,
                ModelErrorKind::BadRequest,
            ),
            (
                r#"{"error":{"code":"content_filter","message":"blocked by policy"}}"#,
                ModelErrorKind::ContentFiltered,
            ),
            (r#"{"error":{"code":"503","message":"oops"}}"#, ModelErrorKind::Server),
            (r#"{"message":"完全看不懂的一次失败"}"#, ModelErrorKind::Unknown),
        ];
        for (body, expected) in cases {
            let value: serde_json::Value = serde_json::from_str(body).unwrap();
            let error = ModelError::from_body(Some(&value));
            assert_eq!(error.kind, *expected, "错误体分类错误：{body} → {error:?}");
        }
    }

    /// P0-1：`response.incomplete` 的原因要变成用户能看懂的分类，而不是"未知错误"。
    #[test]
    fn incomplete_reason_maps_to_actionable_human() {
        let max_tokens = ModelError::incomplete("max_output_tokens");
        assert_eq!(max_tokens.kind, ModelErrorKind::Incomplete);
        assert_eq!(max_tokens.code(), "INCOMPLETE");
        assert!(max_tokens.human.contains("长度"), "{}", max_tokens.human);
        assert_eq!(max_tokens.provider_code.as_deref(), Some("max_output_tokens"));

        let filtered = ModelError::incomplete("content_filter");
        assert!(filtered.human.contains("内容安全策略"), "{}", filtered.human);

        let other = ModelError::incomplete("server_shutdown");
        assert!(other.human.contains("server_shutdown"), "{}", other.human);
    }
}
