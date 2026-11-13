import type { SessionImageRefWire } from './image-ref'

export type TurnEndReason = 'completed' | 'cancelled' | 'failed' | 'budget_exhausted';
export type IoChannel = 'stdout' | 'stderr';
export type RiskLevel = 'read' | 'write_in_workspace' | 'execute' | 'destructive' | 'plan_review';
export type ApprovalDecision = 'allow' | 'deny' | 'allow_always';
export type ToolCallResult = 'ok' | 'error' | 'cancelled' | 'denied';
export type SubagentExitStatus = 'done' | 'failed' | 'cancelled';

export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

/**
 * 宿主 `load_session` 返回的一条档案条目（与 `src-tauri/src/lib.rs` 的 `SessionEntry` 同形）。
 *
 * **唯一一份声明**：它有两个消费面——实时 journal 回放（`adapter.ts`）与导出/复制
 * Markdown（`header-actions/session-events.ts`），两者的**转换逻辑**各自保留（一个直接落
 * journal 事件，一个翻译成上游 `SessionEvent`），但字段定义只能有一份。此前两处各写一份，
 * 漂移已经发生过一次：`images` 只出现在其中一份上，导出因此丢掉用户图片（D1）。
 *
 * 字段名跟 Rust serde 走，不重命名——改名就会和后端序列化结果漂移。
 */
export type SessionEntryLike =
  | {
    readonly kind: 'message'
    readonly role: string
    readonly text: string
    readonly reasoning?: string
    /**
     * 用户消息附带的图片引用（`src-tauri/src/lib.rs` 的 `Message.images`）。
     * 导出/复制读它，而不是把图片压成正文里的 `[图片: …]` 文本——上游 formatter
     * 本来就认 `image` 块（`markdown.ts` 的 `visibleContent`）。
     */
    readonly images?: readonly SessionImageRefWire[]
    /** 事件落档时刻（Unix 毫秒）；缺省或 0 = 旧档案未记录（A0-4）。 */
    readonly at_ms?: number
  }
  | {
    readonly kind: 'tool_call'
    readonly call_id: string
    readonly tool: string
    readonly arguments: string
    readonly output?: string
    /** 事件落档时刻；缺省或 0 = 旧档案未记录（A0-4）。 */
    readonly at_ms?: number
  }
  | {
    /** 回合结局（A0-3）：`completed` / `cancelled` / `failed` / `budget_exhausted`。 */
    readonly kind: 'turn_end'
    readonly turn: number
    readonly reason: string
    readonly at_ms: number
    readonly error?: TurnErrorInfo
  }

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

/** 压缩触发原因（与 Rust `CompactionTrigger` 同形）。 */
export type CompactionTrigger = 'pressure' | 'manual' | 'context_overflow';

/** 写后验证「无法判定」的封闭原因（与 Rust `InconclusiveReason` 同形，不设 catch-all）。 */
export type InconclusiveReason = 'timeout' | 'command_missing' | 'pre_existing_red' | 'env_failure';

/** 写后验证「未执行」的封闭原因（与 Rust `SkipReason` 同形）。 */
export type SkipReason = 'no_source_write' | 'no_command_configured';

/**
 * 写后验证四态（与 Rust `VerifyState` 同形，serde 外部标签）：
 * `'passed'` / `'failed'` 是裸字符串，带原因的两个是单键对象。
 *
 * 消费端纪律：遇到**认不出**的形态按 `failed` 展示（fail-closed），
 * 绝不当成通过——否则后端新增一个原因码就会在前端变成绿色。
 */
export type VerifyState = 'passed' | 'failed' | { inconclusive: InconclusiveReason } | { skipped: SkipReason };

export type EventMsg =
  | { type: 'turn_started'; turn: number }
  | { type: 'turn_completed'; turn: number; reason: TurnEndReason; error?: TurnErrorInfo; unverified?: boolean }
  | { type: 'verification_result'; turn: number; state: VerifyState; commands: string[]; duration_ms: number; summary: string; diagnostics: string[] }
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
  | { type: 'system_prompt'; text: string; sha256: string; at_ms: number; turn: number; step: number }
  | { type: 'compaction_started'; compaction_id: string; trigger: CompactionTrigger; range_start: number; range_end: number; estimated_tokens: number; budget_tokens: number; turn: number | null }
  | { type: 'compaction_summarized'; compaction_id: string; trigger: CompactionTrigger; range_start: number; range_end: number; shadowed_items: number; shadowed_tokens: number; summary: string }
  | { type: 'compaction_ended'; compaction_id: string; ok: boolean; error?: string | null; history_tokens_after?: number | null };

export interface AgentEvent {
  event_id: string;
  session_id: string;
  agent_id: string;
  parent_id: string | null;
  ts_ms: number;
  msg: EventMsg;
}
