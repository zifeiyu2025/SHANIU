//! EventMsg 单一事件协议（ADR-005，共享内核——前后端契约）。
//!
//! 兼容性铁律（AGENTS §2 / docs §5 FF-2）：仅允许 additive 变更——
//! 新增事件类型或新增 `Option` 字段；字段命名 snake_case，与 `src/transport/types.ts` 镜像。
//! 修改本文件必须同步前端镜像并更新 `tests/event_schema.rs`。

use serde::{Deserialize, Serialize};
use tauri::Emitter;

/// 后端 → 前端事件流通道。
pub const EVENT_CHANNEL: &str = "shaniu://event";
/// IPC 握手回显通道（M0 验收：ping/pong）。
pub const PONG_CHANNEL: &str = "shaniu://pong";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Cancelled,
    Failed,
    BudgetExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoChannel {
    Stdout,
    Stderr,
}

/// 工具风险四级（ADR-003）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Read,
    WriteInWorkspace,
    Execute,
    Destructive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    TurnStarted { turn: u32 },
    TurnCompleted { turn: u32, reason: TurnEndReason },
    AgentMessageDelta { delta: String },
    ToolCallBegin { call_id: String, tool: String, args: serde_json::Value },
    ToolCallOutputDelta { call_id: String, channel: IoChannel, delta: String },
    ToolCallEnd { call_id: String, exit_code: Option<i32>, error: Option<String> },
    ExecApprovalRequest { request_id: String, tool: String, args: serde_json::Value, risk: RiskLevel },
    ExecApprovalResolved { request_id: String, decision: ApprovalDecision },
    PlanDelta { delta: String },
    TurnDiff { files: Vec<String> },
    Error { message: String },
    TokenUsage {
        turn: u32,
        input_tokens: u32,
        /// 新增字段必须 optional（FF-2）：旧消费端可忽略。
        #[serde(default)]
        cached_input_tokens: Option<u32>,
        output_tokens: u32,
    },
}

/// 事件出口抽象：agent_loop 及各模块经此广播（便于测试注入与 M1 复用）。
pub trait EventSink: Send + Sync {
    fn emit(&self, msg: &EventMsg) -> Result<(), String>;
}

/// Tauri 广播桥：EventMsg 序列化后经 `shaniu://event` 广播到 WebView。
pub struct TauriEventSink(pub tauri::AppHandle);

impl EventSink for TauriEventSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        self.0
            .emit(EVENT_CHANNEL, msg)
            .map_err(|e| format!("事件广播失败: {e}"))
    }
}

/// IPC 握手：回显 nonce 证明 invoke → 后端 → 事件通道 → 前端链路连通。
pub fn emit_pong(app: &tauri::AppHandle, nonce: &str) -> Result<(), String> {
    app.emit(
        PONG_CHANNEL,
        serde_json::json!({ "nonce": nonce }),
    )
    .map_err(|e| format!("pong 广播失败: {e}"))
}

/// 测试支撑：丢弃全部事件（工具/循环单测的 sink 注入件）。
#[cfg(test)]
#[derive(Debug, Default)]
pub struct NullEventSink;

#[cfg(test)]
impl EventSink for NullEventSink {
    fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
        Ok(())
    }
}
