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
});
