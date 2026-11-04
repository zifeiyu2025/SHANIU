import type { Draft } from 'immer';
import type { EventMsg } from '../../transport/types';
import type { SessionState } from '../session-store';

type TurnEvent = Extract<EventMsg, { type: 'turn_started' | 'turn_completed' }>;

export function applyTurnEvent(state: Draft<SessionState>, msg: TurnEvent): void {
  switch (msg.type) {
    case 'turn_started':
      state.status = 'running';
      state.currentTurn = msg.turn;
      break;
    case 'turn_completed':
      state.status = 'idle';
      state.lastTurnEnd = msg.reason;
      // 冻结所有仍在流式中的节点（含 agent 消息）
      for (const node of state.nodes) {
        if (node.kind === 'agent_message') node.streaming = false;
      }
      break;
  }
}
