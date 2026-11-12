//! 评测判据：端点因为上下文超限拒绝请求时，必须落到独立的受控错误码。
//!
//! 这是评测注入文件（不是业务代码）：agent 不许修改它，改动会被判据守卫拦下。

use shaniu_lib::agent_loop::error_codes;
use shaniu_lib::model_client::{ModelError, ModelErrorKind};

#[test]
fn context_length_message_maps_to_context_overflow() {
    let body = r#"{"error":{"type":"invalid_request_error","message":"This model's maximum context length is 8192 tokens. However, your messages resulted in 9000 tokens."}}"#;
    let err = ModelError::from_status(400, body);
    assert_eq!(err.kind, ModelErrorKind::ContextOverflow);
    assert_eq!(err.code(), "CONTEXT_OVERFLOW");
    assert!(error_codes::is_known("CONTEXT_OVERFLOW"));
    assert!(!err.human.is_empty());
}

#[test]
fn too_many_tokens_on_413_maps_to_context_overflow() {
    let body = r#"{"error":{"message":"too many tokens in request"}}"#;
    assert_eq!(ModelError::from_status(413, body).kind, ModelErrorKind::ContextOverflow);
}

#[test]
fn plain_bad_request_keeps_bad_request_code() {
    let err = ModelError::from_status(400, r#"{"error":{"message":"model not found"}}"#);
    assert_eq!(err.kind, ModelErrorKind::BadRequest);
    assert_eq!(err.code(), "BAD_REQUEST");
}
