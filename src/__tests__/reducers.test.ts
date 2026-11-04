import { describe, expect, it } from 'vitest';
import { produce } from 'immer';
import { applyEvent } from '../store/reducers';
import type { SessionState } from '../store/session-store';
import type { EventMsg } from '../transport/types';

function initialState(): SessionState {
  return { status: 'idle', currentTurn: 0, lastTurnEnd: null, nodes: [], usage: null };
}

function reduce(state: SessionState, msg: EventMsg): SessionState {
  return produce(state, (draft) => applyEvent(draft, msg));
}

describe('reducers', () => {
  it('turn_started 置 running 并记录轮次', () => {
    const state = reduce(initialState(), { type: 'turn_started', turn: 3 });
    expect(state.status).toBe('running');
    expect(state.currentTurn).toBe(3);
  });

  it('agent_message_delta 首次新建流式节点，后续追加', () => {
    let state = reduce(initialState(), { type: 'agent_message_delta', delta: '你' });
    state = reduce(state, { type: 'agent_message_delta', delta: '好' });
    expect(state.nodes).toHaveLength(1);
    const node = state.nodes[0];
    expect(node.kind).toBe('agent_message');
    if (node.kind === 'agent_message') {
      expect(node.text).toBe('你好');
      expect(node.streaming).toBe(true);
    }
  });

  it('冻结后再来 delta 会新开节点', () => {
    let state = reduce(initialState(), { type: 'agent_message_delta', delta: '第一段' });
    state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'completed' });
    state = reduce(state, { type: 'agent_message_delta', delta: '第二轮' });
    expect(state.nodes).toHaveLength(2);
  });

  it('turn_completed 冻结流式节点并记录结束原因', () => {
    let state = reduce(initialState(), { type: 'turn_started', turn: 1 });
    state = reduce(state, { type: 'agent_message_delta', delta: '部分' });
    state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'cancelled' });
    expect(state.status).toBe('idle');
    expect(state.lastTurnEnd).toBe('cancelled');
    const node = state.nodes[0];
    expect(node.kind === 'agent_message' && node.streaming).toBe(false);
  });

  it('token_usage 记录用量', () => {
    const state = reduce(
      initialState(),
      { type: 'token_usage', turn: 1, input_tokens: 10, cached_input_tokens: null, output_tokens: 20 },
    );
    expect(state.usage).toEqual({ inputTokens: 10, outputTokens: 20 });
  });

  it('error 追加错误节点', () => {
    const state = reduce(initialState(), { type: 'error', message: 'boom' });
    expect(state.nodes).toHaveLength(1);
    expect(state.nodes[0]).toMatchObject({ kind: 'error', message: 'boom' });
  });

  it('M1+ 事件与未知事件类型被忽略（前向兼容）', () => {
    let state = reduce(initialState(), { type: 'turn_started', turn: 1 });
    state = reduce(state, { type: 'tool_call_begin', call_id: 'c1', tool: 'unified_exec', args: {} });
    // 运行时收到未知 type 字符串也不得抛错
    state = reduce(state, { type: 'future_event', payload: 1 } as unknown as EventMsg);
    expect(state.nodes).toEqual([]);
    expect(state.status).toBe('running');
  });
});
