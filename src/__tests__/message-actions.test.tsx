import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen } from '@testing-library/react';
import { ChatView } from '../components/chat/ChatView';
import { MessageActions } from '../components/message/MessageActions';
import { createSessionStore } from '../store/session-store';
import { createMockTransport } from '../transport/mock';

/** jsdom 没有剪贴板：按用例注入成功或失败的实现。 */
function stubClipboard(impl: { writeText: (t: string) => Promise<void> } | null) {
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: impl });
}

describe('MessageActions', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('复制成功给出「已复制」', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    stubClipboard({ writeText });
    render(<MessageActions text="这段要复制" />);

    fireEvent.click(screen.getByRole('button', { name: '复制' }));
    // 等可观察结果而不是调用发生：setState 在 writeText 的 promise 之后才落地
    await vi.waitFor(() => expect(screen.getByRole('button', { name: '已复制' })).toBeTruthy());
    expect(writeText).toHaveBeenCalledWith('这段要复制');
  });

  it('反馈 1.5s 后回落，按钮不会长期停在「已复制」骗人', async () => {
    vi.useFakeTimers();
    stubClipboard({ writeText: vi.fn().mockResolvedValue(undefined) });
    render(<MessageActions text="x" />);
    fireEvent.click(screen.getByRole('button', { name: '复制' }));
    await act(async () => {
      vi.advanceTimersByTime(0);
    });
    expect(screen.getByRole('button', { name: '已复制' })).toBeTruthy();

    act(() => {
      vi.advanceTimersByTime(1500);
    });
    expect(screen.getByRole('button', { name: '复制' })).toBeTruthy();
  });

  it('剪贴板不可用/被拒时如实报「复制失败」，不假装成功', async () => {
    stubClipboard(null);
    render(<MessageActions text="x" />);
    fireEvent.click(screen.getByRole('button', { name: '复制' }));
    await vi.waitFor(() => expect(screen.getByRole('button', { name: '复制失败' })).toBeTruthy());
  });

  it('抛错的 writeText 也走失败分支', async () => {
    stubClipboard({
      writeText: () => Promise.reject(new Error('NotAllowedError')),
    });
    render(<MessageActions text="x" />);
    fireEvent.click(screen.getByRole('button', { name: '复制' }));
    await vi.waitFor(() => expect(screen.getByRole('button', { name: '复制失败' })).toBeTruthy());
  });
});

describe('消息行操作条接线', () => {
  it('用户行与正文行各有一条操作条；流式中的正文不给', () => {
    const stub = { writeText: vi.fn().mockResolvedValue(undefined) };
    stubClipboard(stub);
    const store = createSessionStore();
    render(<ChatView store={store} transport={createMockTransport()} />);

    act(() => {
      store.submitUserTask('第一条');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'agent_message_delta', delta: '半截回复' });
    });
    // 只有用户行：正文还在流式，复制一条没写完的回复不是"复制这轮回答"
    expect(screen.getAllByTestId('message-actions')).toHaveLength(1);

    act(() => {
      store.dispatch({ type: 'turn_completed', turn: 1, reason: 'completed' });
    });
    expect(screen.getAllByTestId('message-actions')).toHaveLength(2);
  });

  it('本轮最后一条正文标 always，更早的标 hover（dsh endsWithResponse）', () => {
    stubClipboard({ writeText: vi.fn().mockResolvedValue(undefined) });
    const store = createSessionStore();
    render(<ChatView store={store} transport={createMockTransport()} />);

    act(() => {
      store.submitUserTask('任务');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'agent_message_delta', delta: '第一段' });
      store.dispatch({ type: 'tool_call_begin', call_id: 'c1', tool: 'unified_exec', args: {} });
      store.dispatch({ type: 'agent_message_delta', delta: '第二段' });
      store.dispatch({ type: 'turn_completed', turn: 1, reason: 'completed' });
    });

    const rows = screen.getAllByTestId('agent-row');
    expect(rows[0].getAttribute('data-actions-reveal')).toBe('hover');
    expect(rows[rows.length - 1].getAttribute('data-actions-reveal')).toBe('always');
    expect(screen.getAllByTestId('user-row')).toHaveLength(1);
  });
});
