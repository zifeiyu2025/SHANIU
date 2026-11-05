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
  /**
   * 会话代次：`reset()` 递增。既是事件窗口的账（SH-081 丢弃在途旧事件），
   * 也是视图重挂载的 key（欢迎语只在启动/新建对话取一次时间，SH-080）。
   */
  generation: number;
}

export type SessionStore = SnapshotStore<SessionState> & {
  /**
   * 应用一条后端事件（EventMsg → store 归约）。
   * 会话窗口关闭期间的事件属于上一会话的在途残留，直接丢弃（见 createSessionStore）。
   */
  dispatch(msg: Parameters<typeof applyEvent>[1]): void;
  /** 记录用户任务节点并开启当前会话窗口；transport.sendTask 由视图层调用（store 不依赖 transport）。 */
  submitUserTask(task: string): void;
  /** 新会话：清空当前会话状态并关闭窗口，直到下一次 submitUserTask。 */
  reset(): void;
};

function emptySession(generation: number): SessionState {
  return {
    status: 'idle',
    currentTurn: 0,
    lastTurnEnd: null,
    nodes: [],
    usage: null,
    generation,
  };
}

export function createSessionStore(): SessionStore {
  const store = createSnapshotStore<SessionState>(emptySession(0));
  // 会话窗口：初建即开，reset() 关闭，submitUserTask() 为重开的信号。
  // EventMsg 不带会话标识，跨会话唯一的判别依据是「这一轮是不是本会话用户提交的」：
  // 点新会话时旧轮可能仍在流式产出（真实 IPC 里连 cancel 回执都还没到），
  // 不过滤就会把旧轮的增量/工具卡/用量/轮次终态灌进刚清空的新会话（SH-081）。
  let windowOpen = true;
  return {
    getState: store.getState,
    setState: store.setState,
    subscribe: store.subscribe,
    update: store.update,
    dispatch(msg) {
      if (!windowOpen) return;
      store.update((state) => applyEvent(state, msg));
    },
    submitUserTask(task) {
      windowOpen = true;
      store.update((state) => {
        state.nodes.push(newUserMessageNode(task));
      });
    },
    reset() {
      windowOpen = false;
      store.setState(emptySession(store.getState().generation + 1));
    },
  };
}
