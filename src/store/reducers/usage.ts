import type { Draft } from 'immer';
import type { EventMsg } from '../../transport/types';
import type { SessionState } from '../session-store';

type TokenUsageEvent = Extract<EventMsg, { type: 'token_usage' }>;

/** 覆盖式记录最近一轮的 token 用量（累计口径在 M3 compaction 时再定）。 */
export function applyTokenUsage(state: Draft<SessionState>, msg: TokenUsageEvent): void {
  state.usage = { inputTokens: msg.input_tokens, outputTokens: msg.output_tokens };
}
