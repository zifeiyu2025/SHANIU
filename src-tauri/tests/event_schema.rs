//! 健身函数 FF-2（docs §5）：事件协议兼容契约测试。
//!
//! 不做整段 JSON 字面量快照（AGENTS §5 禁止），而是验证行为契约：
//! 1) 判别标签集合与前端镜像 `src/transport/types.ts` 完全一致；
//! 2) 字段命名 snake_case（前端镜像依赖）；
//! 3) 反序列化忽略未知字段（旧前端容忍新协议，前向兼容）；
//! 4) 每个变体可无损 round-trip。

use shaniu_lib::events::{
    ApprovalDecision, EventMsg, IoChannel, RiskLevel, TurnEndReason,
};

/// 与 src/transport/types.ts 的 type 判别值逐一对应。
const EXPECTED_TAGS: [&str; 12] = [
    "turn_started",
    "turn_completed",
    "agent_message_delta",
    "tool_call_begin",
    "tool_call_output_delta",
    "tool_call_end",
    "exec_approval_request",
    "exec_approval_resolved",
    "plan_delta",
    "turn_diff",
    "error",
    "token_usage",
];

fn all_variants() -> Vec<EventMsg> {
    vec![
        EventMsg::TurnStarted { turn: 1 },
        EventMsg::TurnCompleted { turn: 1, reason: TurnEndReason::Cancelled },
        EventMsg::AgentMessageDelta { delta: "你好".into() },
        EventMsg::ToolCallBegin { call_id: "c1".into(), tool: "unified_exec".into(), args: serde_json::json!({"cmd": "ls"}) },
        EventMsg::ToolCallOutputDelta { call_id: "c1".into(), channel: IoChannel::Stdout, delta: "out".into() },
        EventMsg::ToolCallEnd { call_id: "c1".into(), exit_code: Some(0), error: None },
        EventMsg::ExecApprovalRequest { request_id: "r1".into(), tool: "unified_exec".into(), args: serde_json::json!({}), risk: RiskLevel::Destructive },
        EventMsg::ExecApprovalResolved { request_id: "r1".into(), decision: ApprovalDecision::Deny },
        EventMsg::PlanDelta { delta: "step 1".into() },
        EventMsg::TurnDiff { files: vec!["a.rs".into()] },
        EventMsg::Error { message: "boom".into() },
        EventMsg::TokenUsage { turn: 1, input_tokens: 10, cached_input_tokens: Some(4), output_tokens: 20 },
    ]
}

#[test]
fn event_type_tags_exactly_match_frontend_mirror() {
    let mut actual: Vec<String> = all_variants()
        .iter()
        .map(|m| {
            serde_json::to_value(m)
                .expect("序列化失败")
                .get("type")
                .and_then(|t| t.as_str())
                .expect("缺少 type 判别标签")
                .to_string()
        })
        .collect();
    actual.sort();
    let mut expected: Vec<String> = EXPECTED_TAGS.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(actual, expected, "EventMsg 与前端镜像的判别标签集合漂移——必须同步修改 src/transport/types.ts");
}

#[test]
fn serialization_uses_snake_case_field_names() {
    let json = serde_json::to_value(EventMsg::TokenUsage {
        turn: 2,
        input_tokens: 100,
        cached_input_tokens: None,
        output_tokens: 50,
    })
    .unwrap();
    assert_eq!(json["type"], "token_usage");
    for field in ["turn", "input_tokens", "output_tokens", "cached_input_tokens"] {
        assert!(json.get(field).is_some(), "字段 {field} 缺失——前端镜像要求 snake_case");
    }
    assert!(json.get("inputTokens").is_none(), "禁止 camelCase 漂移");

    let json = serde_json::to_value(EventMsg::ToolCallOutputDelta {
        call_id: "c".into(),
        channel: IoChannel::Stderr,
        delta: "x".into(),
    })
    .unwrap();
    assert_eq!(json["channel"], "stderr", "IoChannel 应为 snake_case");
}

#[test]
fn deserializer_ignores_unknown_fields_and_values_are_snake_case() {
    // 新后端新增字段 → 旧前端/旧消费端必须忽略未知字段
    let raw = serde_json::json!({
        "type": "agent_message_delta",
        "delta": "hi",
        "future_field": { "nested": true }
    });
    let msg: EventMsg = serde_json::from_value(raw).expect("未知字段必须被忽略");
    assert_eq!(msg, EventMsg::AgentMessageDelta { delta: "hi".into() });

    // 枚举值 snake_case：write_in_workspace
    let raw = serde_json::json!({
        "type": "exec_approval_request",
        "request_id": "r",
        "tool": "apply_patch",
        "args": {},
        "risk": "write_in_workspace"
    });
    let msg: EventMsg = serde_json::from_value(raw).expect("risk=write_in_workspace 应可反序列化");
    match msg {
        EventMsg::ExecApprovalRequest { risk, .. } => assert_eq!(risk, RiskLevel::WriteInWorkspace),
        other => panic!("变体不符: {other:?}"),
    }
}

#[test]
fn every_variant_round_trips_losslessly() {
    for original in all_variants() {
        let json = serde_json::to_value(&original).expect("序列化失败");
        let back: EventMsg = serde_json::from_value(json).expect("反序列化失败");
        assert_eq!(original, back);
    }
}

#[test]
fn optional_fields_omitted_when_none() {
    // cached_input_tokens = None 时序列化为 null（serde Option 默认），消费端须容忍
    let json = serde_json::to_value(EventMsg::TokenUsage {
        turn: 1,
        input_tokens: 1,
        cached_input_tokens: None,
        output_tokens: 1,
    })
    .unwrap();
    assert!(json["cached_input_tokens"].is_null());
    let back: EventMsg = serde_json::from_value(json).unwrap();
    assert_eq!(
        back,
        EventMsg::TokenUsage { turn: 1, input_tokens: 1, cached_input_tokens: None, output_tokens: 1 }
    );
}
