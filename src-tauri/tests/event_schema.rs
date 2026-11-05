//! 健身函数 FF-2（docs §5）：事件协议兼容契约测试。
//!
//! 不做整段 JSON 字面量快照（AGENTS §5 禁止），而是验证行为契约：
//! 1) 判别标签集合与前端镜像 `src/transport/types.ts` 完全一致；
//! 2) 字段命名 snake_case（前端镜像依赖）；
//! 3) 反序列化忽略未知字段（旧前端容忍新协议，前向兼容）；
//! 4) 每个变体可无损 round-trip；
//! 5) SH-074：枚举**变体**穷举比对（标签集合只能查事件类型漂移，查不出变体漂移）。

use serde::Serialize;
use shaniu_lib::events::{
    ApprovalDecision, EventMsg, IoChannel, RiskLevel, ToolCallResult, TurnEndReason,
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
        EventMsg::ToolCallEnd { call_id: "c1".into(), exit_code: Some(0), error: None, result: None },
        // 新增（SH-074）：result 有值时的序列化路径也要进快照，别只测 None 分支
        EventMsg::ToolCallEnd {
            call_id: "c2".into(),
            exit_code: None,
            error: Some("用户拒绝".into()),
            result: Some(ToolCallResult::Denied),
        },
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
    // 同一标签可对应多个变体（如 tool_call_end 的 result 有/无两种），比较的是集合
    actual.dedup();
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

/// SH-074：`result` 走 `skip_serializing_if`，`None` 时不产生键；消费端须同时容忍"键缺失"与"显式 null"。
#[test]
fn tool_call_end_result_is_additive_and_null_tolerant() {
    let json = serde_json::to_value(EventMsg::ToolCallEnd {
        call_id: "c1".into(),
        exit_code: Some(0),
        error: None,
        result: None,
    })
    .unwrap();
    assert!(json.get("result").is_none(), "result=None 不得产生键（旧消费端零感知）");

    let missing: EventMsg = serde_json::from_value(serde_json::json!({
        "type": "tool_call_end", "call_id": "c1", "exit_code": 0, "error": null
    }))
    .expect("键缺失必须可反序列化");
    let explicit_null: EventMsg = serde_json::from_value(serde_json::json!({
        "type": "tool_call_end", "call_id": "c1", "exit_code": 0, "error": null, "result": null
    }))
    .expect("显式 null 必须可反序列化");
    assert_eq!(missing, explicit_null);

    let with_result = serde_json::to_value(EventMsg::ToolCallEnd {
        call_id: "c1".into(),
        exit_code: None,
        error: Some("用户拒绝".into()),
        result: Some(ToolCallResult::Denied),
    })
    .unwrap();
    assert_eq!(with_result["result"], "denied", "变体名必须是 snake_case");
}

/// 取出前端镜像里 `export type X = ...;` 的声明体，只在声明内匹配，避免全文件模糊命中。
fn ts_type_union(type_name: &str) -> String {
    let ts = include_str!("../../src/transport/types.ts");
    let marker = format!("export type {type_name} ");
    let start = ts
        .find(&marker)
        .unwrap_or_else(|| panic!("前端镜像缺少 {type_name} 声明"));
    let rest = &ts[start..];
    let end = rest.find(';').expect("类型声明缺少分号");
    rest[..end].to_string()
}

fn assert_union_covers<T: Serialize>(type_name: &str, variants: &[T]) {
    let union = ts_type_union(type_name);
    for variant in variants {
        let value = serde_json::to_value(variant).expect("序列化失败");
        let literal = format!("'{}'", value.as_str().expect("枚举值应为字符串字面量"));
        assert!(
            union.contains(&literal),
            "前端镜像 {type_name} 缺少变体 {literal}（Rust 新增变体必须同步 src/transport/types.ts）。声明：{union}"
        );
    }
}

/// SH-074：FF-2 从"事件标签集合"升级为"**枚举变体穷举**"。
/// 任一 Rust 枚举新增变体而 TS 镜像未同步时必须失败。
#[test]
fn every_enum_variant_is_mirrored_in_frontend_types() {
    assert_union_covers(
        "RiskLevel",
        &[
            RiskLevel::Read,
            RiskLevel::WriteInWorkspace,
            RiskLevel::Execute,
            RiskLevel::Destructive,
            RiskLevel::PlanReview,
        ],
    );
    assert_union_covers(
        "ToolCallResult",
        &[
            ToolCallResult::Ok,
            ToolCallResult::Error,
            ToolCallResult::Cancelled,
            ToolCallResult::Denied,
        ],
    );
    assert_union_covers(
        "ApprovalDecision",
        &[ApprovalDecision::Allow, ApprovalDecision::Deny, ApprovalDecision::AllowAlways],
    );
    assert_union_covers("IoChannel", &[IoChannel::Stdout, IoChannel::Stderr]);
    assert_union_covers(
        "TurnEndReason",
        &[
            TurnEndReason::Completed,
            TurnEndReason::Cancelled,
            TurnEndReason::Failed,
            TurnEndReason::BudgetExhausted,
        ],
    );
}
