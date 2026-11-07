export type TurnEndReason = 'completed' | 'cancelled' | 'failed' | 'budget_exhausted';
export type IoChannel = 'stdout' | 'stderr';
export type RiskLevel = 'read' | 'write_in_workspace' | 'execute' | 'destructive' | 'plan_review';
export type ApprovalDecision = 'allow' | 'deny' | 'allow_always';
export type ToolCallResult = 'ok' | 'error' | 'cancelled' | 'denied';
export type SubagentExitStatus = 'done' | 'failed' | 'cancelled';

export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export type EventMsg =
  | { type: 'turn_started'; turn: number }
  | { type: 'turn_completed'; turn: number; reason: TurnEndReason }
  | { type: 'agent_message_delta'; delta: string }
  | { type: 'reasoning_delta'; delta: string }
  | { type: 'tool_call_begin'; call_id: string; tool: string; args: JsonValue }
  | { type: 'tool_call_output_delta'; call_id: string; channel: IoChannel; delta: string }
  | { type: 'tool_call_end'; call_id: string; exit_code: number | null; error: string | null; result?: ToolCallResult | null }
  | { type: 'exec_approval_request'; request_id: string; tool: string; args: JsonValue; risk: RiskLevel; agent_id?: string; task_summary?: string }
  | { type: 'exec_approval_resolved'; request_id: string; decision: ApprovalDecision }
  | { type: 'plan_delta'; delta: string }
  | { type: 'turn_diff'; files: string[] }
  | { type: 'error'; message: string }
  | { type: 'token_usage'; turn: number; input_tokens: number; cached_input_tokens?: number | null; output_tokens: number }
  | { type: 'subagent_spawned'; child_id: string; task: string; name: string; label: string; created_at_ms: number }
  | { type: 'subagent_exited'; child_id: string; status: SubagentExitStatus; summary: string }
  | { type: 'subagent_message'; child_id: string; sender_id: string; text: string; rpc_id?: string | null };

export interface AgentEvent {
  event_id: string;
  session_id: string;
  agent_id: string;
  parent_id: string | null;
  ts_ms: number;
  msg: EventMsg;
}
