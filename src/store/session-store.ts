import type { TurnEndReason } from '../transport/types';
import { createSnapshotStore, type SnapshotStore } from './engine';
import { applyEvent } from './reducers';
import { newUserMessageNode } from '../nodes/builders';
import type { ConversationNode } from '../nodes/types';

export interface TokenUsageState {
  inputTokens: number;
  outputTokens: number;
}

export interface SessionState {
  status: 'idle' | 'running';
  currentTurn: number;
  lastTurnEnd: TurnEndReason | null;
  nodes: ConversationNode[];
  usage: TokenUsageState | null;
}

export type SessionStore = SnapshotStore<SessionState> & {
  /** 应用一条后端事件（EventMsg → store 归约）。 */
  dispatch(msg: Parameters<typeof applyEvent>[1]): void;
  /** 记录用户任务节点；transport.sendTask 由视图层调用（store 不依赖 transport）。 */
  submitUserTask(task: string): void;
};

export function createSessionStore(): SessionStore {
  const store = createSnapshotStore<SessionState>({
    status: 'idle',
    currentTurn: 0,
    lastTurnEnd: null,
    nodes: [],
    usage: null,
  });
  return {
    getState: store.getState,
    setState: store.setState,
    subscribe: store.subscribe,
    update: store.update,
    dispatch(msg) {
      store.update((state) => applyEvent(state, msg));
    },
    submitUserTask(task) {
      store.update((state) => {
        state.nodes.push(newUserMessageNode(task));
      });
    },
  };
}
