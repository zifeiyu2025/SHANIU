import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { produce } from 'immer';
import { applyEvent } from '../store/reducers';
import { ApprovalCard } from '../components/approval/ApprovalCard';
import type { EventMsg, Transport } from '../transport/types';
import { initialSessionState } from '../store/session-store';
import type { SessionState } from '../store/session-store';

function initialState(): SessionState {
  return initialSessionState();
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

  it('execute 级显示三个按钮，默认焦点就在拒绝上', () => {
    const { transport } = makeTransport();
    render(<ApprovalCard node={baseNode} transport={transport} />);
    expect(screen.getByText('命令执行')).toBeTruthy();
    expect(screen.getByRole('button', { name: '本会话记住' })).toBeTruthy();
    // docs/01 §风险表：默认焦点在「拒绝」是审批疲劳的硬防线，断言焦点本身而不是副作用
    expect(document.activeElement).toBe(screen.getByRole('button', { name: '拒绝' }));
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

  it('Esc = 拒绝（dsh 的 approval.reject 绑 Esc）', () => {
    const { transport, approve } = makeTransport();
    render(<ApprovalCard node={baseNode} transport={transport} />);
    fireEvent.keyDown(screen.getByTestId('approval-appr-c1'), { key: 'Escape' });
    expect(approve).toHaveBeenCalledWith('appr-c1', 'deny');
  });

  it('Enter 不绑允许：默认焦点在拒绝时回车只会拒绝，不会无脑放行', () => {
    const { transport, approve } = makeTransport();
    render(<ApprovalCard node={baseNode} transport={transport} />);
    // 卡片容器上的 Enter 必须什么都不做（dsh 会在这里调 onAllow，我们按 ADR-003 拒绝这条路）
    fireEvent.keyDown(screen.getByTestId('approval-appr-c1'), { key: 'Enter' });
    expect(approve).not.toHaveBeenCalled();
  });

  it('Esc 让位：修饰键、按键重复、输入法组词、焦点在输入控件里都不裁决', () => {
    const { transport, approve } = makeTransport();
    render(
      <div>
        <ApprovalCard node={baseNode} transport={transport} />
        <input data-testid="other-input" />
      </div>,
    );
    const card = screen.getByTestId('approval-appr-c1');

    fireEvent.keyDown(card, { key: 'Escape', ctrlKey: true });
    fireEvent.keyDown(card, { key: 'Escape', shiftKey: true });
    fireEvent.keyDown(card, { key: 'Escape', repeat: true });
    fireEvent.keyDown(card, { key: 'Escape', isComposing: true });
    expect(approve).not.toHaveBeenCalled();

    // 焦点落在输入控件里时，Esc 归那个控件
    const input = screen.getByTestId('other-input');
    input.focus();
    fireEvent.keyDown(input, { key: 'Escape', bubbles: true });
    expect(approve).not.toHaveBeenCalled();
  });

  it('裁决后按钮不消失：整组禁用 + aria-busy + 进行中文案，回执到达后定稿', async () => {
    let resolveApprove: () => void = () => {};
    const approve = vi.fn().mockImplementation(
      () => new Promise<void>((r) => { resolveApprove = r; }),
    );
    const transport = { approve } as unknown as Transport;
    render(<ApprovalCard node={baseNode} transport={transport} />);

    fireEvent.click(screen.getByRole('button', { name: '允许' }));
    const group = screen.getByRole('group', { name: '审批裁决' });
    expect(group.getAttribute('aria-busy')).toBe('true');
    // 三个按钮都还在，但全部禁用——连发两次裁决会让后端收到重复回执
    expect(screen.getAllByRole('button')).toHaveLength(3);
    for (const btn of screen.getAllByRole('button')) {
      expect(btn).toHaveProperty('disabled', true);
    }
    expect(screen.getByRole('status').textContent).toBe('正在允许…');

    resolveApprove();
    await waitFor(() => expect(group.getAttribute('aria-busy')).toBe('false'));
    expect(screen.getByRole('status').textContent).toBe('正在允许…'); // 等后端 exec_approval_resolved 才改口
  });

  it('回执失败时回滚，按钮重新可用', async () => {
    const approve = vi.fn().mockRejectedValue(new Error('IPC 断了'));
    const transport = { approve } as unknown as Transport;
    render(<ApprovalCard node={baseNode} transport={transport} />);

    fireEvent.click(screen.getByRole('button', { name: '拒绝' }));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toContain('IPC 断了'));
    expect(screen.getByRole('button', { name: '拒绝' })).toHaveProperty('disabled', false);
    expect(screen.getByRole('group', { name: '审批裁决' }).getAttribute('aria-busy')).toBe('false');
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('已裁决节点：按钮保留但禁用，给出结果文案', async () => {
    const { transport } = makeTransport();
    render(<ApprovalCard node={{ ...baseNode, status: 'denied' }} transport={transport} />);
    expect(screen.getByRole('status').textContent).toBe('已拒绝');
    for (const btn of screen.getAllByRole('button')) {
      expect(btn).toHaveProperty('disabled', true);
    }
    await waitFor(() => expect(transport.approve).not.toHaveBeenCalled());
  });

  it('plan_review 展示计划正文与「批准计划/拒绝修改」，不提供本会话记住', () => {
    const { transport } = makeTransport();
    const node = {
      ...baseNode,
      requestId: 'appr-plan',
      tool: 'exit_plan_mode',
      risk: 'plan_review' as const,
      args: { plan: '## 步骤\n1. 读代码\n2. 改文件' },
    };
    render(<ApprovalCard node={node} transport={transport} />);
    expect(screen.getByText('计划评审')).toBeTruthy();
    expect(screen.getByText('实施计划待批准')).toBeTruthy();
    expect(screen.getByTestId('plan-appr-plan').textContent).toContain('改文件');
    expect(screen.queryByRole('button', { name: '本会话记住' })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: '批准计划' }));
    expect(transport.approve).toHaveBeenCalledWith('appr-plan', 'allow');
  });

  it('plan_review 已批准显示「计划已批准」', () => {
    const { transport } = makeTransport();
    render(
      <ApprovalCard
        node={{
          ...baseNode,
          requestId: 'appr-plan',
          tool: 'exit_plan_mode',
          risk: 'plan_review' as const,
          args: { plan: '## 步骤' },
          status: 'allowed' as const,
        }}
        transport={transport}
      />,
    );
    expect(screen.getByText('计划已批准')).toBeTruthy();
    // 已定稿的评审卡：按钮仍在但禁用，plan_review 只有两个裁决（无「本会话记住」）
    expect(screen.getAllByRole('button')).toHaveLength(2);
    for (const btn of screen.getAllByRole('button')) {
      expect(btn).toHaveProperty('disabled', true);
    }
  });
});
