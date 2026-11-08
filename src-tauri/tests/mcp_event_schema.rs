//! FF-2 的 MCP 分支（SH-063）：`McpEvent` 与前端镜像 `src/transport/mcp-wire.ts` 的
//! 判别标签、字段命名与 round-trip 契约。
//!
//! 与 `event_schema.rs` 同口径：不做整段 JSON 快照，只验行为契约——标签集合双向相等、
//! 字段 snake_case、未知字段可忽略、每个变体可无损往返。

use serde_json::{json, Value};
use shaniu_lib::events::RiskLevel;
use shaniu_lib::mcp_client::guard::{Exemption, RuleId};
use shaniu_lib::mcp_client::runtime::{
    ApprovedToolView, GuardHitView, McpEvent, McpStatus, ReviewPayload, ReviewTool,
};

/// 与 `src/transport/mcp-wire.ts` 的 `McpEventWire` 字面量逐一对应。
const EXPECTED_TAGS: [&str; 4] = [
    "server_status_changed",
    "manifest_review",
    "trust_resolved",
    "exemption_changed",
];

fn review_payload() -> ReviewPayload {
    ReviewPayload {
        server_id: "srv".into(),
        status: McpStatus::Review,
        fingerprint: "f".repeat(64),
        transport: "stdio".into(),
        executable: "C:/node.exe".into(),
        executable_sha256: "a".repeat(64),
        argv: vec!["server.js".into()],
        cwd: "C:/work".into(),
        env: vec!["TOKEN = sha256:12345678…".into()],
        protocol_version: "2025-06-18".into(),
        server_name: "fake".into(),
        server_version: "1.0.0".into(),
        allow_network: false,
        unsandboxed: false,
        tools: vec![ReviewTool {
            dispatch_name: "mcp__srv__echo".into(),
            raw_name: "echo".into(),
            description: "Echo".into(),
            schema: json!({"type": "object"}),
            annotations: Some(json!({"readOnlyHint": true})),
            server_read_only_hint: true,
            risk: RiskLevel::Destructive,
            approved: false,
            fingerprint: "f".repeat(64),
            guard_hits: vec![GuardHitView {
                rule: "instruction_override".into(),
                fragment: "ignore previous instructions".into(),
            }],
            guard_explanation: Some("命中".into()),
            guard_exempted: false,
        }],
        rejected_tools: Vec::new(),
        approved_tools: vec![ApprovedToolView {
            dispatch_name: "mcp__srv__echo".into(),
            risk: RiskLevel::Destructive,
        }],
        exemptions: vec![Exemption {
            server_id: "srv".into(),
            fingerprint: "f".repeat(64),
            dispatch_name: "mcp__srv__echo".into(),
            rule: RuleId::InstructionOverride,
            hit_fragment: "ignore previous instructions".into(),
            granted_at_ms: 1_760_000_000_000,
        }],
        warnings: vec!["unsandboxed".into()],
        reason: Some("首次连接".into()),
    }
}

fn all_variants() -> Vec<McpEvent> {
    let variants = vec![
        McpEvent::ServerStatusChanged {
            server_id: "srv".into(),
            status: McpStatus::Review,
            reason: Some("首次连接".into()),
        },
        McpEvent::ManifestReview {
            server_id: "srv".into(),
            review: Box::new(review_payload()),
        },
        McpEvent::TrustResolved {
            server_id: "srv".into(),
            fingerprint: "f".repeat(64),
            status: McpStatus::Trusted,
            tools: vec![ApprovedToolView {
                dispatch_name: "mcp__srv__echo".into(),
                risk: RiskLevel::Read,
            }],
        },
        McpEvent::ExemptionChanged {
            server_id: "srv".into(),
            fingerprint: "f".repeat(64),
            exemptions: review_payload().exemptions,
        },
    ];
    // 编译期穷举：新增变体而不更新这里时编译失败，标签集合随之扩大，EXPECTED_TAGS 不同步则测试必红。
    for event in &variants {
        match event {
            McpEvent::ServerStatusChanged { .. }
            | McpEvent::ManifestReview { .. }
            | McpEvent::TrustResolved { .. }
            | McpEvent::ExemptionChanged { .. } => {}
        }
    }
    variants
}

#[test]
fn mcp_event_tags_exactly_match_frontend_mirror() {
    let mut actual: Vec<String> = all_variants()
        .iter()
        .map(|event| {
            serde_json::to_value(event)
                .expect("序列化失败")
                .get("type")
                .and_then(Value::as_str)
                .expect("缺少 type 判别标签")
                .to_string()
        })
        .collect();
    actual.sort();
    actual.dedup();
    let mut expected: Vec<String> = EXPECTED_TAGS.iter().map(|tag| tag.to_string()).collect();
    expected.sort();
    assert_eq!(
        actual, expected,
        "McpEvent 与 src/transport/mcp-wire.ts 的判别标签集合漂移——必须同步修改前端镜像"
    );
}

#[test]
fn review_payload_fields_are_snake_case() {
    let event = McpEvent::ManifestReview {
        server_id: "srv".into(),
        review: Box::new(review_payload()),
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "manifest_review");
    // Box<T> 在 serde 里是透明的：前端拿到的是对象而不是 {0: ...}。
    assert_eq!(
        json["review"]["executable_sha256"].as_str().map(str::len),
        Some(64)
    );
    for field in [
        "server_id",
        "fingerprint",
        "protocol_version",
        "server_name",
        "allow_network",
        "approved_tools",
        "rejected_tools",
        "exemptions",
    ] {
        assert!(
            json["review"].get(field).is_some(),
            "字段 {field} 缺失——前端镜像要求 snake_case"
        );
    }
    assert!(
        json["review"].get("executableSha256").is_none(),
        "禁止 camelCase 漂移"
    );
    // server 自报的只读提示必须与用户档位分开展示（ADR-107）。
    assert_eq!(json["review"]["tools"][0]["server_read_only_hint"], true);
    assert_eq!(json["review"]["tools"][0]["risk"], "destructive");
    assert_eq!(
        json["review"]["tools"][0]["guard_hits"][0]["rule"], "instruction_override",
        "注入命中片段必须可解释"
    );
    assert_eq!(
        json["review"]["exemptions"][0]["rule"],
        "instruction_override"
    );
}

#[test]
fn status_values_are_snake_case() {
    for (status, expected) in [
        (McpStatus::Idle, "idle"),
        (McpStatus::Review, "review"),
        (McpStatus::Trusted, "trusted"),
        (McpStatus::Blocked, "blocked"),
        (McpStatus::Degraded, "degraded"),
        (McpStatus::Failed, "failed"),
    ] {
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            Value::String(expected.into())
        );
    }
}

#[test]
fn unknown_fields_are_ignored_and_variants_round_trip() {
    // 前向兼容：宿主新增字段，旧前端仍能反序列化。
    let raw = json!({
        "type": "server_status_changed",
        "server_id": "srv",
        "status": "review",
        "future_field": {"nested": true}
    });
    let event: McpEvent = serde_json::from_value(raw).expect("未知字段必须被忽略");
    assert_eq!(
        event,
        McpEvent::ServerStatusChanged {
            server_id: "srv".into(),
            status: McpStatus::Review,
            reason: None,
        }
    );

    for original in all_variants() {
        let json = serde_json::to_value(&original).expect("序列化失败");
        let back: McpEvent = serde_json::from_value(json).expect("反序列化失败");
        assert_eq!(original, back, "McpEvent 必须无损 round-trip");
    }
}
