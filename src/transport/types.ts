export type TurnEndReason = 'completed' | 'cancelled' | 'failed' | 'budget_exhausted';
export type IoChannel = 'stdout' | 'stderr';
export type RiskLevel = 'read' | 'write_in_workspace' | 'execute' | 'destructive' | 'plan_review';
export type ApprovalDecision = 'allow' | 'deny' | 'allow_always';
export type ToolCallResult = 'ok' | 'error' | 'cancelled' | 'denied';
export type SubagentExitStatus = 'done' | 'failed' | 'cancelled';

export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

/**
 * 回合失败的结构化原因（与 Rust `TurnErrorInfo` 同形，审计 A0）。
 * `code` 是稳定分类码，`provider_code`/`provider_message` 是端点原始值（已脱敏）。
 */
export interface TurnErrorInfo {
  code: string
  message: string
  http_status?: number
  provider_code?: string
  provider_message?: string
  /** 本次失败前已经重试过几次（0 = 首次请求就失败）。 */
  attempts?: number
  /** 端点 `Retry-After` 原值（秒）。仅“因该值超过上限而放弃重试”时填写。 */
  retry_after_secs?: number
}

/** 一次模型请求重试的阶段（与 Rust `RetryPhase` 同形）。 */
export type RetryPhase = 'scheduled' | 'started' | 'exhausted';

export type EventMsg =
  | { type: 'turn_started'; turn: number }
  | { type: 'turn_completed'; turn: number; reason: TurnEndReason; error?: TurnErrorInfo }
  | { type: 'agent_message_delta'; delta: string }
  | { type: 'reasoning_delta'; delta: string }
  | { type: 'tool_call_begin'; call_id: string; tool: string; args: JsonValue }
  | { type: 'tool_call_output_delta'; call_id: string; channel: IoChannel; delta: string }
  | { type: 'tool_call_end'; call_id: string; exit_code: number | null; error: string | null; result?: ToolCallResult | null }
  | { type: 'exec_approval_request'; request_id: string; tool: string; args: JsonValue; risk: RiskLevel; agent_id?: string; task_summary?: string; reason?: string; call_id?: string; batch_scope?: string }
  | { type: 'exec_approval_resolved'; request_id: string; decision: ApprovalDecision }
  | { type: 'allowlist_hit'; tool: string; command: string; rule_id: string }
  | { type: 'plan_delta'; delta: string }
  | { type: 'turn_diff'; files: string[] }
  | { type: 'error'; message: string }
  | { type: 'model_retry'; turn: number; retry: number; max_retries: number; phase: RetryPhase; delay_ms: number; code: string; message: string; retry_id: string }
  | { type: 'token_usage'; turn: number; input_tokens: number; cached_input_tokens?: number | null; output_tokens: number }
  | { type: 'subagent_spawned'; child_id: string; task: string; name: string; label: string; created_at_ms: number }
  | { type: 'subagent_exited'; child_id: string; status: SubagentExitStatus; summary: string }
  | { type: 'subagent_message'; child_id: string; sender_id: string; text: string; rpc_id?: string | null }
  | { type: 'rollout_degraded'; message: string }
  | { type: 'system_prompt'; text: string; sha256: string; at_ms: number; turn: number; step: number };

export interface AgentEvent {
  event_id: string;
  session_id: string;
  agent_id: string;
  parent_id: string | null;
  ts_ms: number;
  msg: EventMsg;
}
