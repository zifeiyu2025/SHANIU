import { describe, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { App } from '../App';
import { createMockTransport } from '../transport/mock';

describe('App 窗口 chrome', () => {
  it('标题栏按钮切换右侧工具面板（默认关闭）', () => {
    render(<App transport={createMockTransport()} />);
    expect(screen.queryByTestId('tool-panel')).toBeNull();

    act(() => {
      fireEvent.click(screen.getByRole('button', { name: '打开工具面板' }));
    });
    expect(screen.getByTestId('tool-panel')).toBeTruthy();

    // 面板打开后标题栏与面板内部各有一个同名按钮：点面板内部那个
    const panel = screen.getByTestId('tool-panel');
    act(() => {
      fireEvent.click(within(panel).getByRole('button', { name: '关闭工具面板' }));
    });
    expect(screen.queryByTestId('tool-panel')).toBeNull();
  });

  it('侧栏折叠/展开（折叠钮在侧栏行，折叠后为 56px 轨）', () => {
    render(<App transport={createMockTransport()} />);
    expect(screen.getByRole('button', { name: '折叠侧栏' })).toBeTruthy();

    act(() => {
      fireEvent.click(screen.getByRole('button', { name: '折叠侧栏' }));
    });
    expect(screen.queryByRole('button', { name: '折叠侧栏' })).toBeNull();
    expect(screen.getByRole('button', { name: '展开侧栏' })).toBeTruthy();

    act(() => {
      fireEvent.click(screen.getByRole('button', { name: '展开侧栏' }));
    });
    expect(screen.getByRole('button', { name: '折叠侧栏' })).toBeTruthy();
  });

  it('浏览器模式（非 Tauri）不渲染窗口控制按钮', () => {
    render(<App transport={createMockTransport()} />);
    expect(screen.queryByRole('button', { name: '最小化' })).toBeNull();
    expect(screen.queryByRole('button', { name: '最大化' })).toBeNull();
    expect(screen.queryByRole('button', { name: '关闭' })).toBeNull();
  });

  it('IPC 握手成功后标题栏状态点变绿', async () => {
    render(<App transport={createMockTransport()} />);
    const dot = screen.getByTestId('link-state');
    expect(dot.className).toContain('bg-dimmed'); // 连接中
    await act(async () => {
      await new Promise((r) => setTimeout(r, 80));
    });
    expect(dot.className).toContain('bg-success'); // ping 已回显
  });

  it('提交任务后从空态进入会话流', () => {
    const { unmount } = render(<App transport={createMockTransport()} />);
    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);

    fireEvent.change(input, { target: { value: '跑个任务' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    // 用户气泡立即出现（store 同步记录）；composer 从 hero 切换为沉底实例，输入框为空
    expect(screen.getByText('跑个任务')).toBeTruthy();
    expect(screen.getByPlaceholderText(/描述你想要构建的内容/)).toHaveProperty('value', '');
    unmount();
  });

  // SH-081：事件订阅挂在 App 层，不随「新会话」重建——旧轮的在途事件会灌进刚清空的新会话。
  // 用真实计时器 + waitFor 跑 mock 全轮，比假定时器更贴近真机时序，也不受慢速 runner 牵连。
  it('流式中点新会话：旧轮在途事件不污染新会话', async () => {
    const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
    const transport = createMockTransport();
    const cancel = vi.spyOn(transport, 'cancel');
    render(<App transport={transport} />);

    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);
    act(() => {
      fireEvent.change(input, { target: { value: '旧任务' } });
      fireEvent.keyDown(input, { key: 'Enter' });
    });

    expect(screen.getByTestId('turn-status').textContent).toContain('运行中');
    // 旧轮确实跑到了会话里（污染前提成立）：等到工具卡出现再动手
    await waitFor(
      () => expect(screen.queryAllByTestId(/^tool-call-/).length).toBeGreaterThan(0),
      { timeout: 5000 },
    );
    expect(screen.getByText('旧任务')).toBeTruthy();

    act(() => {
      fireEvent.click(screen.getByRole('button', { name: '新会话' }));
    });
    expect(cancel).toHaveBeenCalledTimes(1); // 旧轮在源头被请求中断
    expect(screen.getByTestId('greeting')).toBeTruthy(); // 已回到空态

    await act(async () => {
      await wait(2000);
    }); // 放足时间让旧轮把剩余增量/工具卡/用量/轮次终态全部发完

    expect(screen.queryByText('旧任务')).toBeNull();
    expect(screen.getByTestId('greeting')).toBeTruthy(); // 欢迎语只在 nodes 为空时渲染
    expect(screen.queryAllByTestId(/^tool-call-/)).toHaveLength(0);
    expect(screen.queryByTestId('token-usage')).toBeNull();
    expect(screen.queryByTestId('turn-status')).toBeNull();

    // 新会话未被门禁锁死：提交新任务后事件流恢复
    const nextInput = screen.getByPlaceholderText(/描述你想要构建的内容/);
    act(() => {
      fireEvent.change(nextInput, { target: { value: '新任务' } });
      fireEvent.keyDown(nextInput, { key: 'Enter' });
    });
    await waitFor(() => expect(screen.getByTestId('streaming-cursor')).toBeTruthy(), {
      timeout: 5000,
    });
    expect(screen.getByText('新任务')).toBeTruthy();
    expect(screen.getByTestId('turn-status').textContent).toContain('运行中');
  });
});
