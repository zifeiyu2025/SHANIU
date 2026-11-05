import type { Draft } from 'immer';
import type { EventMsg } from '../../transport/types';
import type { SessionState } from '../session-store';
import { applyTurnEvent } from './turn';
import { applyAgentMessageDelta } from './agent-message';
import { applyReasoningDelta } from './reasoning';
import { applyTokenUsage } from './usage';
import { applyError } from './error';
import { applyApprovalEvent } from './approval';
import { applyToolCallEvent } from './tool-call';

/**
 * EventMsg → store 归约器入口。
 * 未列出的 plan_delta / turn_diff（后续里程碑）与
 * 未知事件类型一律忽略：旧前端必须能容忍新协议事件（前向兼容，见 docs §5 FF-2）。
 */
export function applyEvent(state: Draft<SessionState>, msg: EventMsg): void {
  switch (msg.type) {
    case 'turn_started':
    case 'turn_completed':
      return applyTurnEvent(state, msg);
    case 'agent_message_delta':
      return applyAgentMessageDelta(state, msg);
    case 'reasoning_delta':
      return applyReasoningDelta(state, msg);
    case 'tool_call_begin':
    case 'tool_call_output_delta':
    case 'tool_call_end':
      return applyToolCallEvent(state, msg);
    case 'token_usage':
      return applyTokenUsage(state, msg);
    case 'error':
      return applyError(state, msg);
    case 'exec_approval_request':
    case 'exec_approval_resolved':
      return applyApprovalEvent(state, msg);
    default:
      break;
  }
}
