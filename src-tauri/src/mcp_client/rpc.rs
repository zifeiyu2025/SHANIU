//! JSON-RPC 2.0 / NDJSON 编解码（MCP v1 只走 stdio 传输）。
//!
//! 只做协议层，不碰进程与注册表：
//! - 入站统一解成 [`Incoming`]（请求 / 通知 / 响应三态），调用方据此关联 pending 请求；
//! - 单行上限与 `sandbox` 的读线程同源（`MAX_MCP_STDIO_LINE_BYTES`），这里是第二道；
//! - **server→client 请求默认拒绝**（docs/02 §3.1.4 C-R6）：v1 只应答 `ping`，
//!   `sampling/createMessage`、`roots/list`、`elicitation/create` 与任何未知方法一律回
//!   `-32601`——恶意 server 不能借 sampling 消耗模型额度，也不能借 roots 读取工作区外路径。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use crate::sandbox::windows_impl::MAX_MCP_STDIO_LINE_BYTES;

/// `initialize.protocolVersion`（v1 只声明这一版）。
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

// JSON-RPC 2.0 错误码（只列 v1 实际使用的）
/// 收到的对象不是合法请求。
pub const INVALID_REQUEST: i64 = -32600;
/// 方法不存在或不支持（server→client 请求的默认拒绝码）。
pub const METHOD_NOT_FOUND: i64 = -32601;
/// 客户端内部错误。
pub const INTERNAL_ERROR: i64 = -32603;

/// 请求/响应关联标识：MCP 允许数字或字符串，两种都要能原样回显。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::String(s) => write!(f, "{s}"),
        }
    }
}

/// JSON-RPC 错误体（server 回给我们的失败响应）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// 入站消息（client 视角的三态）。
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// server→client 请求（带 id，必须应答；v1 除 `ping` 外一律拒绝）。
    Request { id: RequestId, method: String },
    /// server→client 通知（无 id，不应答）。`notifications/tools/list_changed` 走这里。
    Notification { method: String },
    /// 对客户端请求的响应。
    Response {
        id: RequestId,
        result: Option<Value>,
        error: Option<RpcErrorObject>,
    },
}

/// server→client 请求的处理结论（C-R6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientRequestVerdict {
    /// 正常应答（v1 只有 `ping`：空结果，不涉及采样/工作区/用户交互）。
    Supported,
    /// 回 `-32601` 并记录告警；调用方负责把方法名写进审计。
    Unsupported,
}

/// 编解码错误。全部按协议错误处理——**断开该连接**，不做「跳过这一行继续读」。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RpcError {
    #[error("单行超过 {limit} 字节上限（实际 {actual} 字节）")]
    TooLarge { limit: usize, actual: usize },
    #[error("JSON 解析失败: {0}")]
    Parse(String),
    #[error("非法 JSON-RPC 消息: {0}")]
    Invalid(String),
}

/// 解一行 NDJSON（不含行尾换行；行尾 CR/LF 由读线程剥离）。
pub fn decode_line(line: &str) -> Result<Incoming, RpcError> {
    decode_line_with_limit(line, MAX_MCP_STDIO_LINE_BYTES)
}

/// 带上限的解码入口（单测用小上限，避免构造 8MB 字符串）。
fn decode_line_with_limit(line: &str, limit: usize) -> Result<Incoming, RpcError> {
    if line.len() > limit {
        return Err(RpcError::TooLarge { limit, actual: line.len() });
    }
    let value: Value = serde_json::from_str(line).map_err(|e| RpcError::Parse(e.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| RpcError::Invalid("顶层必须是 JSON 对象".into()))?;

    match object.get("jsonrpc").and_then(Value::as_str) {
        Some("2.0") => {}
        _ => return Err(RpcError::Invalid("jsonrpc 必须为 \"2.0\"".into())),
    }

    let id = match object.get("id") {
        // JSON-RPC 允许解析失败时回 `"id": null`；我们从不发不可解析的请求，因此按无 id 处理。
        None | Some(Value::Null) => None,
        Some(raw) => Some(parse_id(raw)?),
    };

    let method = match object.get("method") {
        Some(Value::String(name)) if !name.is_empty() => Some(name.clone()),
        Some(_) => return Err(RpcError::Invalid("method 必须是非空字符串".into())),
        None => None,
    };

    match (method, id) {
        (Some(method), Some(id)) => Ok(Incoming::Request { id, method }),
        (Some(method), None) => Ok(Incoming::Notification { method }),
        (None, Some(id)) => {
            let result = object.get("result").cloned();
            let error = match object.get("error") {
                None | Some(Value::Null) => None,
                Some(raw) => Some(
                    serde_json::from_value::<RpcErrorObject>(raw.clone())
                        .map_err(|e| RpcError::Invalid(format!("error 字段不合法: {e}")))?,
                ),
            };
            match (result, error) {
                (Some(result), None) => Ok(Incoming::Response { id, result: Some(result), error: None }),
                (None, Some(error)) => Ok(Incoming::Response { id, result: None, error: Some(error) }),
                (Some(_), Some(_)) => {
                    Err(RpcError::Invalid("响应不能同时含 result 与 error".into()))
                }
                (None, None) => Err(RpcError::Invalid("响应缺少 result 或 error".into())),
            }
        }
        (None, None) => Err(RpcError::Invalid("既无 method 也无 id".into())),
    }
}

fn parse_id(raw: &Value) -> Result<RequestId, RpcError> {
    match raw {
        Value::Number(number) => number
            .as_i64()
            .map(RequestId::Number)
            .ok_or_else(|| RpcError::Invalid("id 必须是整数或字符串".into())),
        Value::String(text) => Ok(RequestId::String(text.clone())),
        _ => Err(RpcError::Invalid("id 必须是整数或字符串".into())),
    }
}

/// 编码一条请求（不带行尾换行）。
pub fn encode_request(id: &RequestId, method: &str, params: Option<Value>) -> String {
    let mut message = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
    });
    if let Some(params) = params {
        message["params"] = params;
    }
    message.to_string()
}

/// 编码一条通知（无 id）。
pub fn encode_notification(method: &str, params: Option<Value>) -> String {
    let mut message = json!({
        "jsonrpc": "2.0",
        "method": method,
    });
    if let Some(params) = params {
        message["params"] = params;
    }
    message.to_string()
}

/// 编码成功响应。
pub fn encode_result(id: &RequestId, result: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
    .to_string()
}

/// 编码失败响应。
pub fn encode_error(id: &RequestId, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
    .to_string()
}

/// 该 server→client 方法是否在 v1 支持范围内。
///
/// 只有 `ping`：它是协议级探活，不消耗模型额度、不读工作区、不与用户交互。
pub fn client_request_verdict(method: &str) -> ClientRequestVerdict {
    if method == "ping" {
        ClientRequestVerdict::Supported
    } else {
        ClientRequestVerdict::Unsupported
    }
}

/// 为 server→client 请求生成应答（每条请求都必须应答，否则 server 侧悬挂）。
///
/// 拒绝时错误文本带原方法名：这是「可解释拒绝」的最低要求，调用方还要把它写进审计。
pub fn answer_client_request(id: &RequestId, method: &str) -> (String, ClientRequestVerdict) {
    match client_request_verdict(method) {
        ClientRequestVerdict::Supported => (encode_result(id, json!({})), ClientRequestVerdict::Supported),
        ClientRequestVerdict::Unsupported => (
            encode_error(id, METHOD_NOT_FOUND, &format!("不支持 server→client 请求: {method}")),
            ClientRequestVerdict::Unsupported,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_response_with_numeric_id() {
        let decoded = decode_line(r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[]}}"#).unwrap();
        match decoded {
            Incoming::Response { id, result, error } => {
                assert_eq!(id, RequestId::Number(7));
                assert_eq!(result, Some(json!({ "tools": [] })));
                assert!(error.is_none(), "成功响应不应带 error");
            }
            other => panic!("应解成响应: {other:?}"),
        }
    }

    #[test]
    fn decodes_error_response_as_error_not_result() {
        let decoded = decode_line(
            r#"{"jsonrpc":"2.0","id":"call-1","error":{"code":-32602,"message":"bad params"}}"#,
        )
        .unwrap();
        match decoded {
            Incoming::Response { id, result, error } => {
                assert_eq!(id, RequestId::String("call-1".into()));
                assert!(result.is_none(), "失败响应不应带 result");
                let error = error.expect("应解析出 error");
                assert_eq!(error.code, -32602);
                assert_eq!(error.message, "bad params");
            }
            other => panic!("应解成响应: {other:?}"),
        }
    }

    #[test]
    fn decodes_notification_without_id() {
        let decoded =
            decode_line(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#).unwrap();
        assert_eq!(
            decoded,
            Incoming::Notification { method: "notifications/tools/list_changed".into() }
        );
    }

    #[test]
    fn decodes_server_to_client_request() {
        let decoded =
            decode_line(r#"{"jsonrpc":"2.0","id":3,"method":"sampling/createMessage"}"#).unwrap();
        assert_eq!(
            decoded,
            Incoming::Request { id: RequestId::Number(3), method: "sampling/createMessage".into() }
        );
    }

    #[test]
    fn rejects_non_object_wrong_version_and_float_id() {
        assert!(matches!(decode_line("[1,2]"), Err(RpcError::Invalid(_))));
        assert!(matches!(
            decode_line(r#"{"jsonrpc":"1.0","id":1,"result":{}}"#),
            Err(RpcError::Invalid(_))
        ));
        assert!(matches!(
            decode_line(r#"{"jsonrpc":"2.0","id":1.5,"result":{}}"#),
            Err(RpcError::Invalid(_))
        ));
        assert!(matches!(decode_line("not json"), Err(RpcError::Parse(_))));
    }

    #[test]
    fn rejects_response_with_both_or_neither_result_and_error() {
        assert!(matches!(
            decode_line(r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1,"message":"x"}}"#),
            Err(RpcError::Invalid(_))
        ));
        assert!(matches!(
            decode_line(r#"{"jsonrpc":"2.0","id":1}"#),
            Err(RpcError::Invalid(_))
        ));
        assert!(matches!(
            decode_line(r#"{"jsonrpc":"2.0"}"#),
            Err(RpcError::Invalid(_))
        ));
    }

    #[test]
    fn oversized_line_is_rejected_before_parsing() {
        let line = "x".repeat(100);
        match decode_line_with_limit(&line, 64) {
            Err(RpcError::TooLarge { limit, actual }) => {
                assert_eq!(limit, 64);
                assert_eq!(actual, 100);
            }
            other => panic!("超限应被拒绝: {other:?}"),
        }
        // 上限内但非 JSON：走解析错误，而不是「超限」。
        assert!(matches!(decode_line_with_limit(&line, 128), Err(RpcError::Parse(_))));
    }

    #[test]
    fn request_round_trip_preserves_id_method_and_params() {
        let id = RequestId::Number(42);
        let wire = encode_request(&id, "tools/call", Some(json!({ "name": "read", "arguments": {} })));
        assert!(!wire.contains('\n'), "编码结果不含换行，由 writer 追加");
        match decode_line(&wire).unwrap() {
            Incoming::Request { id: decoded_id, method } => {
                assert_eq!(decoded_id, id);
                assert_eq!(method, "tools/call");
            }
            other => panic!("应解成请求: {other:?}"),
        }
    }

    #[test]
    fn notification_round_trip_has_no_id() {
        let wire = encode_notification("notifications/initialized", None);
        assert!(!wire.contains("\"id\""), "通知不得带 id: {wire}");
        assert_eq!(
            decode_line(&wire).unwrap(),
            Incoming::Notification { method: "notifications/initialized".into() }
        );
    }

    #[test]
    fn sampling_roots_elicitation_are_rejected_with_method_not_found() {
        for method in ["sampling/createMessage", "roots/list", "elicitation/create"] {
            assert_eq!(
                client_request_verdict(method),
                ClientRequestVerdict::Unsupported,
                "{method} 在 v1 必须拒绝"
            );
            let id = RequestId::Number(9);
            let (wire, verdict) = answer_client_request(&id, method);
            assert_eq!(verdict, ClientRequestVerdict::Unsupported);
            match decode_line(&wire).unwrap() {
                Incoming::Response { id: decoded, result, error } => {
                    assert_eq!(decoded, id, "拒绝也要带回原 id，否则 server 侧悬挂");
                    assert!(result.is_none());
                    let error = error.expect("拒绝必须是 error 响应");
                    assert_eq!(error.code, METHOD_NOT_FOUND);
                    assert!(error.message.contains(method), "错误文本须可解释: {}", error.message);
                }
                other => panic!("应解成响应: {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_client_request_is_rejected_too() {
        let (wire, verdict) = answer_client_request(&RequestId::String("x".into()), "some/future_method");
        assert_eq!(verdict, ClientRequestVerdict::Unsupported);
        match decode_line(&wire).unwrap() {
            Incoming::Response { error: Some(error), .. } => {
                assert_eq!(error.code, METHOD_NOT_FOUND);
                assert!(error.message.contains("some/future_method"));
            }
            other => panic!("未知方法也应回错误响应: {other:?}"),
        }
    }

    #[test]
    fn ping_is_answered_with_empty_result() {
        let id = RequestId::Number(1);
        let (wire, verdict) = answer_client_request(&id, "ping");
        assert_eq!(verdict, ClientRequestVerdict::Supported);
        match decode_line(&wire).unwrap() {
            Incoming::Response { id: decoded, result, error } => {
                assert_eq!(decoded, id);
                assert_eq!(result, Some(json!({})));
                assert!(error.is_none());
            }
            other => panic!("ping 应回空结果: {other:?}"),
        }
    }

    #[test]
    fn error_response_round_trips_with_string_id() {
        let id = RequestId::String("req-a".into());
        let wire = encode_error(&id, INVALID_REQUEST, "bad");
        match decode_line(&wire).unwrap() {
            Incoming::Response { id: decoded, error: Some(error), .. } => {
                assert_eq!(decoded, id);
                assert_eq!(error.code, INVALID_REQUEST);
            }
            other => panic!("应解成错误响应: {other:?}"),
        }
    }
}
