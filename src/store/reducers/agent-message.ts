import type { Draft } from 'immer';
import { newAgentMessageNode } from '../../nodes/builders';
import type { EventMsg } from '../../transport/types';
import type { SessionState } from '../session-store';

type AgentMessageDelta = Extract<EventMsg, { type: 'agent_message_delta' }>;

/** 追加到末尾的流式 agent 节点；否则新开一个流式节点。 */
export function applyAgentMessageDelta(state: Draft<SessionState>, msg: AgentMessageDelta): void {
  const last = state.nodes[state.nodes.length - 1];
  if (last && last.kind === 'agent_message' && last.streaming) {
    last.text += msg.delta;
    return;
  }
  // 正文开始 = 思考结束：收口末尾仍在流式的 reasoning 节点，避免两个 spinner 并转
  if (last && last.kind === 'reasoning' && last.streaming) last.streaming = false;
  state.nodes.push(newAgentMessageNode(msg.delta, true));
}
