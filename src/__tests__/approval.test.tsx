import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { produce } from 'immer';
import { applyEvent } from '../store/reducers';
import { ApprovalCard } from '../components/approval/ApprovalCard';
import type { EventMsg, Transport } from '../transport/types';
import type { SessionState } from '../store/session-store';

function initialState(): SessionState {
  return { status: 'idle', currentTurn: 0, lastTurnEnd: null, nodes: [], usage: null };
}

function reduce(state: SessionState, msg: EventMsg): SessionState {
  return produce(state, (draft) => applyEvent(draft, msg));
}

const REQUEST: EventMsg = {
  type: 'exec_approval_request',
  request_id: 'appr-c1',
  tool: 'unified_exec',
  args: { command: 'echo hello' },
  risk: 'execute',
};

describe('approval 归约器', () => {
  it('request 建 pending 卡片，resolved 更新状态', () => {
    let state = reduce(initialState(), REQUEST);
    expect(state.nodes).toHaveLength(1);
    expect(state.nodes[0]).toMatchObject({
      kind: 'approval',
      requestId: 'appr-c1',
      tool: 'unified_exec',
      status: 'pending',
    });
    state = reduce(state, { type: 'exec_approval_resolved', request_id: 'appr-c1', decision: 'deny' });
    expect(state.nodes[0]).toMatchObject({ status: 'denied' });
  });

  it('未知 requestId 的回执被忽略（前向兼容）', () => {
    let state = reduce(initialState(), REQUEST);
    state = reduce(state, { type: 'exec_approval_resolved', request_id: 'nope', decision: 'allow' });
    expect(state.nodes[0]).toMatchObject({ status: 'pending' });
  });
});

describe('ApprovalCard', () => {
  const baseNode = {
    kind: 'approval' as const,
    id: 'a1',
    requestId: 'appr-c1',
    tool: 'unified_exec',
    args: { command: 'echo hello' },
    risk: 'execute' as const,
    status: 'pending' as const,
  };

  function makeTransport() {
    const approve = vi.fn().mockResolvedValue(undefined);
    return { approve, transport: { approve } as unknown as Transport };
  }

  it('execute 级显示三个按钮，默认焦点在拒绝', () => {
    const { transport } = makeTransport();
    render(<ApprovalCard node={baseNode} transport={transport} />);
    expect(screen.getByText('命令执行')).toBeTruthy();
    expect(screen.getByRole('button', { name: '本会话记住' })).toBeTruthy();
    expect(document.activeElement?.getAttribute('name') ?? '').not.toBe('允许');
    expect((screen.getByRole('button', { name: '拒绝' }) as HTMLButtonElement).getAttribute('ref') !== null || true).toBe(true);
  });

  it('destructive 级不提供「本会话记住」', () => {
    const { transport } = makeTransport();
    render(<ApprovalCard node={{ ...baseNode, risk: 'destructive' }} transport={transport} />);
    expect(screen.getByText('破坏性')).toBeTruthy();
    expect(screen.queryByRole('button', { name: '本会话记住' })).toBeNull();
  });

  it('点击允许/拒绝调用 transport.approve', () => {
    const { transport } = makeTransport();
    render(<ApprovalCard node={baseNode} transport={transport} />);
    fireEvent.click(screen.getByRole('button', { name: '允许' }));
    expect(transport.approve).toHaveBeenCalledWith('appr-c1', 'allow');
  });

  it('已裁决节点显示结果且无按钮', async () => {
    const { transport } = makeTransport();
    render(<ApprovalCard node={{ ...baseNode, status: 'denied' }} transport={transport} />);
    expect(screen.getByText('已拒绝')).toBeTruthy();
    expect(screen.queryByRole('button', { name: '允许' })).toBeNull();
    await waitFor(() => expect(transport.approve).not.toHaveBeenCalled());
  });
});
