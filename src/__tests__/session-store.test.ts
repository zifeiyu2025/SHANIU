import { describe, expect, it, vi } from 'vitest';
import { createSessionStore } from '../store/session-store';

describe('session store', () => {
  it('dispatch 驱动快照更新且保持结构共享', () => {
    const store = createSessionStore();
    const before = store.getState().nodes;
    store.dispatch({ type: 'turn_started', turn: 1 });
    store.dispatch({ type: 'agent_message_delta', delta: 'hi' });

    const after = store.getState();
    expect(after.status).toBe('running');
    expect(after.nodes).toHaveLength(1);
    // 结构共享：旧 nodes 引用不变，新数组是新引用
    expect(after.nodes).not.toBe(before);
  });

  it('选择器订阅：切片不变不通知（subscribeWithSelector 语义）', () => {
    const store = createSessionStore();
    const spy = vi.fn();
    store.subscribe((s) => s.status, spy);

    store.dispatch({ type: 'turn_started', turn: 1 }); // status 变化 → 通知
    store.dispatch({ type: 'agent_message_delta', delta: 'x' }); // status 不变 → 不通知

    expect(spy).toHaveBeenCalledTimes(1);
    expect(spy).toHaveBeenCalledWith('running', 'idle');
  });

  it('submitUserTask 记录用户节点', () => {
    const store = createSessionStore();
    store.submitUserTask('帮我读代码');
    const node = store.getState().nodes[0];
    expect(node).toMatchObject({ kind: 'user_message', text: '帮我读代码' });
  });

  it('全量订阅在每次状态变化时收到前后快照', () => {
    const store = createSessionStore();
    const seen: Array<{ status: string; prev: string }> = [];
    store.subscribe((state, prev) => seen.push({ status: state.status, prev: prev.status }));
    store.dispatch({ type: 'turn_started', turn: 1 });
    store.dispatch({ type: 'turn_completed', turn: 1, reason: 'completed' });
    expect(seen).toEqual([
      { status: 'running', prev: 'idle' },
      { status: 'idle', prev: 'running' },
    ]);
  });

  // SH-088：待发队列的三条硬约束——不入空串、取走是原子的、越界撤回不动别的
  describe('待发队列', () => {
    it('enqueue 去空白、空串不入队；shift 取走即出队且保序', () => {
      const store = createSessionStore();
      store.enqueueTask('  甲  ');
      store.enqueueTask('   ');
      store.enqueueTask('乙');
      expect(store.getState().queued).toEqual(['甲', '乙']);

      expect(store.shiftQueuedTask()).toBe('甲');
      expect(store.getState().queued).toEqual(['乙']);
      expect(store.shiftQueuedTask()).toBe('乙');
      expect(store.shiftQueuedTask()).toBeNull(); // 空队列取不到，也不会报错
    });

    it('removeQueuedTask 按下标撤回，越界不动状态', () => {
      const store = createSessionStore();
      store.enqueueTask('甲');
      store.enqueueTask('乙');
      const before = store.getState();

      store.removeQueuedTask(9);
      expect(store.getState().queued).toEqual(['甲', '乙']);
      // immer 结构共享：没改动就不产生新快照
      expect(store.getState().queued).toBe(before.queued);

      store.removeQueuedTask(0);
      expect(store.getState().queued).toEqual(['乙']);
    });
  });

  // SH-081：App 级全局订阅不受 reset 影响，旧轮在途事件必须被代次门禁丢弃
  describe('会话窗口门禁', () => {
    it('reset 后旧轮的在途事件不再落入新会话', () => {
      const store = createSessionStore();
      store.submitUserTask('旧任务');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'agent_message_delta', delta: '旧轮输出' });
      store.setSending(true);
      store.enqueueTask('旧会话的排队');
      expect(store.getState().nodes).toHaveLength(2);

      store.reset();

      store.dispatch({ type: 'agent_message_delta', delta: '幽灵回复' });
      store.dispatch({ type: 'tool_call_begin', call_id: 'old-1', tool: 'unified_exec', args: {} });
      store.dispatch({ type: 'token_usage', turn: 1, input_tokens: 9, output_tokens: 9 });
      store.dispatch({ type: 'turn_completed', turn: 1, reason: 'cancelled' });

      const after = store.getState();
      expect(after.nodes).toEqual([]);
      expect(after.usage).toBeNull();
      expect(after.status).toBe('idle');
      expect(after.lastTurnEnd).toBeNull();
      // 排队消息与在途标记同属上一个会话：带进新会话会在下一次空闲跃变时凭空发出去
      expect(after.queued).toEqual([]);
      expect(after.sending).toBe(false);
    });

    it('新会话提交任务后重开窗口，事件恢复归约', () => {
      const store = createSessionStore();
      store.reset();
      store.dispatch({ type: 'agent_message_delta', delta: '仍属旧会话，丢弃' });
      expect(store.getState().nodes).toEqual([]);

      store.submitUserTask('新任务');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'agent_message_delta', delta: '新轮输出' });

      const after = store.getState();
      expect(after.status).toBe('running');
      expect(after.nodes.map((n) => n.kind)).toEqual(['user_message', 'agent_message']);
    });
  });
});
