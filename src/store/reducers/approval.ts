import type { Draft } from 'immer';
import { newApprovalNode } from '../../nodes/builders';
import type { EventMsg } from '../../transport/types';
import type { SessionState } from '../session-store';

/**
 * 审批事件归约：请求 → pending 卡片；回执 → 更新同 requestId 节点状态。
 */
export function applyApprovalEvent(state: Draft<SessionState>, msg: EventMsg): void {
  switch (msg.type) {
    case 'exec_approval_request': {
      state.nodes.push(
        newApprovalNode(msg.request_id, msg.tool, msg.args, msg.risk),
      );
      return;
    }
    case 'exec_approval_resolved': {
      const node = [...state.nodes].reverse().find(
        (n) => n.kind === 'approval' && n.requestId === msg.request_id,
      );
      if (node && node.kind === 'approval') {
        node.status = msg.decision === 'deny' ? 'denied' : 'allowed';
      }
      return;
    }
    default:
      return;
  }
}
