import type { Draft } from 'immer';
import { newErrorNode } from '../../nodes/builders';
import type { EventMsg } from '../../transport/types';
import type { SessionState } from '../session-store';

type ErrorEvent = Extract<EventMsg, { type: 'error' }>;

export function applyError(state: Draft<SessionState>, msg: ErrorEvent): void {
  state.nodes.push(newErrorNode(msg.message));
}
