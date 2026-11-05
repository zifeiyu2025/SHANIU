import type { Draft } from 'immer';
import { newReasoningNode } from '../../nodes/builders';
import type { EventMsg } from '../../transport/types';
import type { SessionState } from '../session-store';

type ReasoningDelta = Extract<EventMsg, { type: 'reasoning_delta' }>;

/**
 * 推理增量落到**末尾的流式 reasoning 节点**上；正文节点开始流式时，reasoning 节点必须收口
 * （否则"思考中"和"回答中"两个 spinner 同时转，用户分不清模型在想还是在建）。
 */
export function applyReasoningDelta(state: Draft<SessionState>, msg: ReasoningDelta): void {
  const last = state.nodes[state.nodes.length - 1];
  if (last && last.kind === 'reasoning' && last.streaming) {
    last.text += msg.delta;
    return;
  }
  state.nodes.push(newReasoningNode(msg.delta, true));
}
