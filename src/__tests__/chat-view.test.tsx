import { describe, expect, it } from 'vitest';
import { act, render, screen } from '@testing-library/react';
import { ChatView } from '../components/chat/ChatView';
import { createSessionStore } from '../store/session-store';
import { createMockTransport } from '../transport/mock';

// AGENTS §5：UI 组件至少覆盖「流式中间态」与「完成态」两种渲染
describe('ChatView', () => {
  it('流式中间态：用户任务与部分回复可见、输入禁用、停止按钮出现', () => {
    const store = createSessionStore();
    const transport = createMockTransport();
    render(<ChatView store={store} transport={transport} />);

    act(() => {
      store.submitUserTask('读一下 README');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'agent_message_delta', delta: '正在读取' });
    });

    expect(screen.getByText('读一下 README')).toBeTruthy();
    expect(screen.getByText('正在读取')).toBeTruthy();
    expect(screen.getByTestId('streaming-cursor')).toBeTruthy();
    expect(screen.getByRole('button', { name: '停止' })).toBeTruthy();
    expect(
      (screen.getByPlaceholderText(/描述你想要构建的内容/) as HTMLTextAreaElement).disabled,
    ).toBe(true);
    expect(screen.getByTestId('turn-status').textContent).toContain('运行中');
  });

  it('完成态：全文可见、输入恢复、用量展示', () => {
    const store = createSessionStore();
    const transport = createMockTransport();
    render(<ChatView store={store} transport={transport} />);

    act(() => {
      store.submitUserTask('任务');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'agent_message_delta', delta: '完整回复' });
      store.dispatch({ type: 'token_usage', turn: 1, input_tokens: 10, output_tokens: 5 });
      store.dispatch({ type: 'turn_completed', turn: 1, reason: 'completed' });
    });

    expect(screen.getByText('完整回复')).toBeTruthy();
    expect(screen.queryByTestId('streaming-cursor')).toBeNull();
    expect(screen.getByRole('button', { name: '发送' })).toBeTruthy();
    expect(
      (screen.getByPlaceholderText(/描述你想要构建的内容/) as HTMLTextAreaElement).disabled,
    ).toBe(false);
    expect(screen.getByTestId('token-usage').textContent).toContain('↑10');
  });

  it('空态：欢迎区 + 输入卡片居中', () => {
    const store = createSessionStore();
    render(<ChatView store={store} transport={createMockTransport()} />);
    expect(screen.getByTestId('empty-hero')).toBeTruthy();
    expect(screen.getByPlaceholderText(/描述你想要构建的内容/)).toBeTruthy();
  });
});
