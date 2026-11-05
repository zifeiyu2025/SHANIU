import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen } from '@testing-library/react';
import { ChatView } from '../components/chat/ChatView';
import { createSessionStore } from '../store/session-store';
import { createMockTransport } from '../transport/mock';

// AGENTS §5：UI 组件至少覆盖「流式中间态」与「完成态」两种渲染
describe('ChatView', () => {
  afterEach(() => {
    vi.useRealTimers();
  });
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
    // dsh StatsPills：紧凑总量（10 输入 + 5 输出 = 15）
    expect(screen.getByTestId('token-usage').textContent).toContain('15');
  });

  it('空态：输入卡片居中', () => {
    const store = createSessionStore();
    render(<ChatView store={store} transport={createMockTransport()} />);
    expect(screen.getByPlaceholderText(/描述你想要构建的内容/)).toBeTruthy();
    expect(screen.getByTestId('greeting').textContent).toBeTruthy();
  });

  it('新建对话：清空会话并让欢迎语重新取一次时间', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-11-05T12:30:00'));
    const store = createSessionStore();
    render(<ChatView store={store} transport={createMockTransport()} />);
    expect(screen.getByTestId('greeting').textContent).toBe('中午好呀，要不要先休息一下');

    act(() => {
      store.submitUserTask('旧任务');
    });
    expect(screen.getByText('旧任务')).toBeTruthy();
    expect(screen.queryByTestId('greeting')).toBeNull(); // 进入会话流后欢迎语卸载

    vi.setSystemTime(new Date('2026-11-05T14:30:00'));
    fireEvent.click(screen.getByRole('button', { name: '新会话' }));
    expect(screen.queryByText('旧任务')).toBeNull(); // 会话已清空
    expect(screen.getByTestId('greeting').textContent).toBe('下午好呀，接下来交给我吧');
  });

  it('空态下点新会话：换 key 重新挂载取时，且欢迎语节点不重复', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-11-05T12:30:00'));
    render(
      <ChatView store={createSessionStore()} transport={createMockTransport()} />,
    );
    expect(screen.getByTestId('greeting').textContent).toBe('中午好呀，要不要先休息一下');

    vi.setSystemTime(new Date('2026-11-05T14:30:00'));
    fireEvent.click(screen.getByRole('button', { name: '新会话' }));
    expect(screen.getAllByTestId('greeting')).toHaveLength(1);
    expect(screen.getByTestId('greeting').textContent).toBe('下午好呀，接下来交给我吧');
  });

  it('父组件重渲染不刷新欢迎语（只在启动/新建对话取时）', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-11-05T12:30:00'));
    const store = createSessionStore();
    const transport = createMockTransport();
    const { rerender } = render(<ChatView store={store} transport={transport} />);

    vi.setSystemTime(new Date('2026-11-05T14:30:00'));
    rerender(
      <ChatView
        store={store}
        transport={transport}
        headerAction={<span data-testid="header-extra" />}
      />,
    );
    expect(screen.getByTestId('header-extra')).toBeTruthy();
    expect(screen.getByTestId('greeting').textContent).toBe('中午好呀，要不要先休息一下');
  });
});
