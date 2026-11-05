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
  /**
   * 提交在途（dsh `input.phase === 'sending'`）：IPC 已发出但后端还没开始产事件。
   * 放 store 而不是 Composer 局部 state，是因为空态用的是 hero 输入卡、有消息后换成
   * 沉底输入卡——两个 Composer 不是同一个实例，局部 state 会在第一次发送时随组件卸载丢失。
   */
  sending: boolean;
  /**
   * 待发队列（SH-088，对应 dsh QueueDock 的 `inbox['next-turn']`）：流式期间按「排队发送」
   * 存下的任务，本轮回到 idle 后按序发出。后端只有一个任务槽（`begin_task` 会拒并发），
   * 所以这里做的是排队而不是插话——插话需要后端注入运行中的上下文，属 M5 steering 面。
   */
  queued: string[];
}

export type SessionStore = SnapshotStore<SessionState> & {
  /**
   * 应用一条后端事件（EventMsg → store 归约）。
   * 会话窗口关闭期间的事件属于上一会话的在途残留，直接丢弃（见 createSessionStore）。
   */
  dispatch(msg: Parameters<typeof applyEvent>[1]): void;
  /** 记录用户任务节点并开启当前会话窗口；transport.sendTask 由视图层调用（store 不依赖 transport）。 */
  submitUserTask(task: string): void;
  /** 标记/解除提交在途。 */
  setSending(sending: boolean): void;
  /** 把任务排进待发队列（队列里不放空串）。 */
  enqueueTask(task: string): void;
  /** 取出并移除队首；队列空时返回 null。原子的取走语义，避免重复发送。 */
  shiftQueuedTask(): string | null;
  /** 按下标撤回一条排队消息。 */
  removeQueuedTask(index: number): void;
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
    sending: false,
    queued: [],
  };
}

/** 初始会话状态：与 store 的默认值同源，供测试构造基线（省得每加一个字段就改三处字面量）。 */
export function initialSessionState(): SessionState {
  return emptySession(0);
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
    setSending(sending) {
      store.update((state) => {
        state.sending = sending;
      });
    },
    enqueueTask(task) {
      const trimmed = task.trim();
      if (trimmed === '') return;
      store.update((state) => {
        state.queued.push(trimmed);
      });
    },
    shiftQueuedTask() {
      // 先取走再返回：StrictMode 下同一个 effect 会跑两遍，若只读不删就会把同一条排队
      // 消息发两次；删在同一个 immer 事务里，第二遍看到的必然是空队列。
      let taken: string | null = null;
      store.update((state) => {
        if (state.queued.length > 0) taken = state.queued.shift() ?? null;
      });
      return taken;
    },
    removeQueuedTask(index) {
      store.update((state) => {
        if (index >= 0 && index < state.queued.length) state.queued.splice(index, 1);
      });
    },
    reset() {
      windowOpen = false;
      store.setState(emptySession(store.getState().generation + 1));
    },
  };
}
