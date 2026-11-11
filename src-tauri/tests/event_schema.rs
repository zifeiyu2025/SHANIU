//! 健身函数 FF-2（docs §5）：事件协议兼容契约测试。
//!
//! 不做整段 JSON 字面量快照（AGENTS §5 禁止），而是验证行为契约：
//! 1) EventMsg tag 与前端镜像 `src/transport/types.ts` **机械绑定**（从 types.ts 声明提取
//!    `type: '...'` 字面量集合，与 Rust 侧 tag 做双向集合相等，SH-079）；
//! 2) 字段命名 snake_case（前端镜像依赖）；
//! 3) 反序列化忽略未知字段（旧前端容忍新协议，前向兼容）；
//! 4) 每个变体可无损 round-trip；
//! 5) SH-074：枚举**变体**穷举比对（标签集合只能查事件类型漂移，查不出变体漂移）；
//!    SH-079：枚举穷举升级为**双向集合相等**（TS 单方多出变体也必红）。

use serde::Serialize;
use shaniu_lib::events::{
    AgentEvent, ApprovalDecision, EventMsg, IoChannel, RetryPhase, RiskLevel, SubagentExitStatus, ToolCallResult,
    TurnEndReason, TurnErrorInfo,
};

/// 与 src/transport/types.ts 的 type 判别值逐一对应。
const EXPECTED_TAGS: [&str; 19] = [
    "turn_started",
    "turn_completed",
    "agent_message_delta",
    "reasoning_delta",
    "tool_call_begin",
    "tool_call_output_delta",
    "tool_call_end",
    "exec_approval_request",
    "exec_approval_resolved",
    "allowlist_hit",
    "plan_delta",
    "turn_diff",
    "error",
    "model_retry",
    "token_usage",
    "subagent_spawned",
    "subagent_exited",
    "subagent_message",
    "system_prompt",
];

fn all_variants() -> Vec<EventMsg> {
    let variants = vec![
        EventMsg::TurnStarted { turn: 1 },
        EventMsg::TurnCompleted { turn: 1, reason: TurnEndReason::Cancelled, error: None },
        // 审计 A0：失败回合的结构化错误（additive）也要进 round-trip 与序列化覆盖
        EventMsg::TurnCompleted {
            turn: 2,
            reason: TurnEndReason::Failed,
            error: Some(TurnErrorInfo {
                code: "AUTH".into(),
                message: "密钥无效".into(),
                http_status: Some(401),
                provider_code: Some("invalid_api_key".into()),
                provider_message: Some("Invalid API key".into()),
                attempts: Some(5),
                retry_after_secs: Some(30),
            }),
        },
        EventMsg::AgentMessageDelta { delta: "你好".into() },
        EventMsg::ReasoningDelta { delta: "先想想".into() },
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
        EventMsg::ExecApprovalRequest { request_id: "r1".into(), tool: "unified_exec".into(), args: serde_json::json!({}), risk: RiskLevel::Destructive, agent_id: None, task_summary: None, reason: None, call_id: None, batch_scope: None },
        // SH-054：审批冒泡的 additive 字段（子代理来源）也必须进快照
        EventMsg::ExecApprovalRequest {
            request_id: "r2".into(),
            tool: "unified_exec".into(),
            args: serde_json::json!({"cmd": "ls"}),
            risk: RiskLevel::Execute,
            agent_id: Some("01JCHILD".into()),
            task_summary: Some("统计仓库文件数".into()),
            // SH-118⑤ / SH-120：风险原因（additive）
            reason: Some("执行命令需要人工确认".into()),
            // 审批卡详情定位（additive）：不让前端解析 `appr-` 前缀
            call_id: Some("c2".into()),
            // 批量范围（additive，A0 收紧后）：只有 Execute 且参数指纹可算时才有值
            batch_scope: Some("same-args".into()),
        },
        EventMsg::ExecApprovalResolved { request_id: "r1".into(), decision: ApprovalDecision::Deny },
        // SH-120：前缀白名单放行审计（additive）
        EventMsg::AllowlistHit {
            tool: "unified_exec".into(),
            command: "cargo test --lib".into(),
            rule_id: "cargo test".into(),
        },
        EventMsg::PlanDelta { delta: "step 1".into() },
        EventMsg::TurnDiff { files: vec!["a.rs".into()] },
        EventMsg::Error { message: "boom".into() },
        // 模型请求重试（additive）：三阶段都要进 round-trip 与标签集合
        EventMsg::ModelRetry {
            turn: 1,
            retry: 2,
            max_retries: 5,
            phase: RetryPhase::Scheduled,
            delay_ms: 8_000,
            code: "RATE_LIMIT".into(),
            message: "请求过于频繁，已被限流".into(),
            retry_id: "01JRETRY".into(),
        },
        EventMsg::ModelRetry {
            turn: 1,
            retry: 2,
            max_retries: 5,
            phase: RetryPhase::Started,
            delay_ms: 0,
            code: "RATE_LIMIT".into(),
            message: "请求过于频繁，已被限流".into(),
            retry_id: "01JRETRY".into(),
        },
        EventMsg::ModelRetry {
            turn: 3,
            retry: 1,
            max_retries: 5,
            phase: RetryPhase::Exhausted,
            delay_ms: 0,
            code: "RATE_LIMIT".into(),
            message: "服务端要求等待 30 秒，超过上限 10 秒，已放弃重试".into(),
            retry_id: "01JRETRY2".into(),
        },
        EventMsg::TokenUsage { turn: 1, input_tokens: 10, cached_input_tokens: Some(4), output_tokens: 20 },
        // SH-053：三个 additive 子代理事件。exited 的三个终态都要进清单，
        // 否则 status 的某个变体可能从未被序列化验证过。
        EventMsg::SubagentSpawned {
            child_id: "01JCHILD".into(),
            task: "探索仓库".into(),
            name: "worker-1".into(),
            label: "探索仓库".into(),
            created_at_ms: 1_760_000_000_000,
        },
        EventMsg::SubagentExited { child_id: "01JCHILD".into(), status: SubagentExitStatus::Done, summary: "结论".into() },
        EventMsg::SubagentExited { child_id: "01JCHILD".into(), status: SubagentExitStatus::Failed, summary: "失败".into() },
        EventMsg::SubagentExited { child_id: "01JCHILD".into(), status: SubagentExitStatus::Cancelled, summary: "已取消".into() },
        EventMsg::SubagentMessage { child_id: "01JCHILD".into(), sender_id: "root".into(), text: "请改看 transport".into(), rpc_id: Some("rpc-9".into()) },
        // 提示词入轨迹（additive）：载荷原样 round-trip
        EventMsg::SystemPrompt {
            text: "你是 SHANIU。\n## 工作区\nD:\\ws".into(),
            sha256: "a1b2c3".into(),
            at_ms: 1_760_000_000_000,
            turn: 1,
            step: 0,
        },
    ];
    // 编译期穷举证明（SH-078）：无通配臂 match，EventMsg 新增变体而不更新上面的 vec 时编译失败；
    // vec 更新后 tag 集合随之扩大，EXPECTED_TAGS 不同步则标签集测试必红——双向闭环。
    for msg in &variants {
        match msg {
            EventMsg::TurnStarted { .. }
            | EventMsg::TurnCompleted { .. }
            | EventMsg::AgentMessageDelta { .. }
            | EventMsg::ReasoningDelta { .. }
            | EventMsg::ToolCallBegin { .. }
            | EventMsg::ToolCallOutputDelta { .. }
            | EventMsg::ToolCallEnd { .. }
            | EventMsg::ExecApprovalRequest { .. }
            | EventMsg::ExecApprovalResolved { .. }
            | EventMsg::AllowlistHit { .. }
            | EventMsg::PlanDelta { .. }
            | EventMsg::TurnDiff { .. }
            | EventMsg::Error { .. }
            | EventMsg::ModelRetry { .. }
            | EventMsg::TokenUsage { .. }
            | EventMsg::SubagentSpawned { .. }
            | EventMsg::SubagentExited { .. }
            | EventMsg::SubagentMessage { .. }
            | EventMsg::SystemPrompt { .. } => {}
        }
    }
    variants
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

fn frontend_mirror() -> String {
    include_str!("../../src/transport/types.ts").to_owned()
}

fn ts_object_fields(declaration: &str) -> std::collections::BTreeMap<String, bool> {
    let start = declaration.find('{').expect("TS 对象声明缺少左括号");
    let end = declaration.rfind('}').expect("TS 对象声明缺少右括号");
    declaration[start + 1..end].split(';').filter_map(|field| {
        let field = field.trim();
        if field.is_empty() {
            return None;
        }
        let (name, _) = field.split_once(':').expect("TS 字段缺少类型声明");
        let name = name.trim();
        Some((name.trim_end_matches('?').to_string(), name.ends_with('?')))
    }).collect()
}

#[test]
fn event_payload_fields_match_frontend_mirror() {
    let ts = frontend_mirror();
    let variants = all_variants();
    for tag in EXPECTED_TAGS {
        let marker = format!("type: '{tag}'");
        let declaration = ts.lines().find(|line| line.contains(&marker)).expect("TS 事件载荷缺失");
        let fields = ts_object_fields(declaration);
        let mut actual = std::collections::BTreeSet::new();
        for msg in &variants {
            let json = serde_json::to_value(msg).unwrap();
            if json["type"] != tag {
                continue;
            }
            actual.extend(json.as_object().unwrap().keys().cloned());
            for (field, optional) in &fields {
                if *optional {
                    let mut absent = json.clone();
                    absent.as_object_mut().unwrap().remove(field);
                    serde_json::from_value::<EventMsg>(absent).unwrap_or_else(|error| {
                        panic!("TS 的 {tag}.{field} 标为 optional，但 Rust 拒绝缺省: {error}")
                    });
                } else {
                    assert!(json.get(field).is_some(), "TS 要求 {tag}.{field} 必填，但 Rust 未发送");
                }
            }
        }
        assert_eq!(actual, fields.keys().cloned().collect(), "{tag} 载荷字段与 TS 镜像漂移");
    }
}

#[test]
fn envelope_keeps_existing_payload_and_matches_frontend_fields() {
    let ts = frontend_mirror();
    let declaration = ts.split_once("export interface AgentEvent ").expect("信封镜像缺失").1;
    let fields = ts_object_fields(declaration);
    for msg in all_variants() {
        let event = AgentEvent {
            event_id: ulid::Ulid::generate().to_string(),
            session_id: "session-a".into(),
            agent_id: "root".into(),
            parent_id: None,
            ts_ms: 123,
            msg: msg.clone(),
        };
        let json = serde_json::to_value(&event).unwrap();
        let actual: std::collections::BTreeSet<_> = json.as_object().unwrap().keys().cloned().collect();
        assert_eq!(actual, fields.keys().cloned().collect(), "信封字段与 TS 镜像漂移");
        assert!(json.get("type").is_none(), "事件标签留在 msg 内，不混入信封");
        assert!(json.get("parent_id").unwrap().is_null(), "root 的 parent_id 明确为 null");
        assert_eq!(serde_json::from_value::<EventMsg>(json["msg"].clone()).unwrap(), msg);
        assert_eq!(serde_json::from_value::<AgentEvent>(json).unwrap(), event);
    }
}

/// 取出前端镜像里 `export type X = ...;` 的声明体，只在声明内匹配，避免全文件模糊命中。
fn ts_type_union(type_name: &str) -> String {
    let ts = frontend_mirror();
    let marker = format!("export type {type_name} ");
    let start = ts
        .find(&marker)
        .unwrap_or_else(|| panic!("前端镜像缺少 {type_name} 声明"));
    let rest = &ts[start..];
    let end = rest.find(';').expect("类型声明缺少分号");
    rest[..end].to_string()
}

/// 提取 TS 声明体内的单引号字符串字面量集合（SH-079）。
/// 适用前提：枚举镜像 union 形如 `export type X = 'a' | 'b';`（当前五个枚举镜像全部满足）；
/// 若将来镜像声明带注释，会给出多余成员必红而非漏报（fail-safe 方向）。
fn ts_union_literals(union_body: &str) -> std::collections::BTreeSet<String> {
    let mut literals = std::collections::BTreeSet::new();
    let mut pos = 0usize;
    while let Some(rel_open) = union_body[pos..].find('\'') {
        let open = pos + rel_open;
        let Some(rel_close) = union_body[open + 1..].find('\'') else {
            panic!("TS 声明单引号不配对（位置 {open}）：{union_body}")
        };
        let close = open + 1 + rel_close;
        literals.insert(union_body[open + 1..close].to_string());
        pos = close + 1;
    }
    literals
}

/// SH-079：从"单向包含"升级为**双向集合相等**——
/// Rust 加变体前端不跟、或前端单方多出/乱改变体，两侧漂移必红。
fn assert_union_covers<T: Serialize>(type_name: &str, variants: &[T]) {
    let union = ts_type_union(type_name);
    let rust_set: std::collections::BTreeSet<String> = variants
        .iter()
        .map(|v| {
            let value = serde_json::to_value(v).expect("序列化失败");
            value.as_str().expect("枚举值应为字符串字面量").to_string()
        })
        .collect();
    let ts_set = ts_union_literals(&union);
    assert_eq!(
        rust_set,
        ts_set,
        "前端镜像 {type_name} 与 Rust 枚举漂移（双向集合相等）：Rust-only={:?} TS-only={:?}——必须同步 src/transport/types.ts。声明：{union}",
        rust_set.difference(&ts_set).collect::<Vec<_>>(),
        ts_set.difference(&rust_set).collect::<Vec<_>>()
    );
}

/// SH-074/078：FF-2 从"事件标签集合"升级为"**枚举变体穷举**"，变体清单不再硬编码——
/// 由 events.rs 各枚举的 `all_variants()`（无通配臂 match，编译期穷举证明）单一供给。
/// 任一 Rust 枚举新增变体而 TS 镜像未同步时：先编译失败逼清单同步，再断言失败指向 TS 镜像。
#[test]
fn every_enum_variant_is_mirrored_in_frontend_types() {
    assert_union_covers("RiskLevel", RiskLevel::all_variants());
    assert_union_covers("ToolCallResult", ToolCallResult::all_variants());
    assert_union_covers("ApprovalDecision", ApprovalDecision::all_variants());
    assert_union_covers("IoChannel", IoChannel::all_variants());
    assert_union_covers("TurnEndReason", TurnEndReason::all_variants());
    assert_union_covers("SubagentExitStatus", SubagentExitStatus::all_variants());
    assert_union_covers("RetryPhase", RetryPhase::all_variants());
}

/// SH-079：EventMsg tag 与前端镜像**机械绑定**（此前 EXPECTED_TAGS 与 types.ts 无人校验关联）。
/// 从 types.ts 的 EventMsg 声明提取所有 `type: '...'` 字面量，与 Rust 侧 tag 做双向集合相等。
/// EventMsg union 里只有判别字段带字符串字面量（其余字段引用枚举名，无内联字面量），
/// 故提取出的集合恰为 tag 集合。
#[test]
fn event_msg_tags_are_mechanically_bound_to_frontend_mirror() {
    let ts = frontend_mirror();
    let marker = "export type EventMsg ";
    let start = ts.find(marker).expect("前端镜像缺少 EventMsg 声明");
    let rest = &ts[start..];
    // EventMsg 成员是对象字面量（成员内含 ';'），声明终止于下一个顶层 export，而非首个 ';'
    let end = rest[marker.len()..]
        .find("\nexport ")
        .map(|i| marker.len() + i)
        .unwrap_or(rest.len());
    let decl = &rest[..end];

    let mut ts_tags = std::collections::BTreeSet::new();
    let mut pos = 0usize;
    while let Some(rel) = decl[pos..].find("type:") {
        let start = pos + rel + "type:".len();
        let Some(qopen_rel) = decl[start..].find('\'') else { break };
        let qopen = start + qopen_rel;
        let Some(qclose_rel) = decl[qopen + 1..].find('\'') else {
            panic!("EventMsg 声明内 type: 后单引号不配对（位置 {qopen}）")
        };
        let qclose = qopen + 1 + qclose_rel;
        ts_tags.insert(decl[qopen + 1..qclose].to_string());
        pos = qclose + 1;
    }

    let mut rust_tags = std::collections::BTreeSet::new();
    for msg in all_variants() {
        let tag = serde_json::to_value(&msg)
            .expect("序列化失败")
            .get("type")
            .and_then(|t| t.as_str())
            .expect("缺少 type 判别标签")
            .to_string();
        rust_tags.insert(tag);
    }

    assert_eq!(
        rust_tags,
        ts_tags,
        "EventMsg 判别标签与 types.ts 声明漂移：Rust-only={:?} TS-only={:?}",
        rust_tags.difference(&ts_tags).collect::<Vec<_>>(),
        ts_tags.difference(&rust_tags).collect::<Vec<_>>()
    );
}
