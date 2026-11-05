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

impl TurnEndReason {
    /// FF-2 单一事实源（SH-078）：变体清单的唯一权威，`tests/event_schema.rs` 据此穷举比对 TS 镜像。
    /// 无通配臂 match 保证新增变体而不更新此处时**编译失败**——模式清单与返回数组必须逐条一致。
    pub fn all_variants() -> &'static [Self] {
        match Self::Completed {
            Self::Completed | Self::Cancelled | Self::Failed | Self::BudgetExhausted => {
                &[Self::Completed, Self::Cancelled, Self::Failed, Self::BudgetExhausted]
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoChannel {
    Stdout,
    Stderr,
}

impl IoChannel {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Stdout {
            Self::Stdout | Self::Stderr => &[Self::Stdout, Self::Stderr],
        }
    }
}

/// 工具风险四级（ADR-003）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Read,
    WriteInWorkspace,
    Execute,
    Destructive,
    /// 计划评审（Plan Mode 退出确认，ADR-003）。
    PlanReview,
}

impl RiskLevel {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Read {
            Self::Read | Self::WriteInWorkspace | Self::Execute | Self::Destructive | Self::PlanReview => &[
                Self::Read,
                Self::WriteInWorkspace,
                Self::Execute,
                Self::Destructive,
                Self::PlanReview,
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
    /// 仅 execute 级有效：本会话记住选择（ADR-003），destructive 永不记忆。
    AllowAlways,
}

impl ApprovalDecision {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Allow {
            Self::Allow | Self::Deny | Self::AllowAlways => &[Self::Allow, Self::Deny, Self::AllowAlways],
        }
    }
}

/// 工具调用终态（docs/04 §1.2.1）：`ToolCallEnd.result` 的取值域。
/// 存在时是**唯一**状态判据；缺失时前端才走 `exit_code`/`error` 的两步反推回退。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallResult {
    /// 工具执行成功。
    Ok,
    /// 执行失败（含超时、非零退出、工具层错误）。
    Error,
    /// 用户/系统取消（CancellationToken）。
    Cancelled,
    /// 审批拒绝。
    Denied,
}

impl ToolCallResult {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Ok {
            Self::Ok | Self::Error | Self::Cancelled | Self::Denied => {
                &[Self::Ok, Self::Error, Self::Cancelled, Self::Denied]
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    TurnStarted { turn: u32 },
    TurnCompleted { turn: u32, reason: TurnEndReason },
    AgentMessageDelta { delta: String },
    ToolCallBegin { call_id: String, tool: String, args: serde_json::Value },
    ToolCallOutputDelta { call_id: String, channel: IoChannel, delta: String },
    ToolCallEnd {
        call_id: String,
        exit_code: Option<i32>,
        error: Option<String>,
        /// additive（FF-2，docs/04 §1.2.1）：`None` 时不产生该键，旧消费端无感知。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<ToolCallResult>,
    },
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
