import { describe, expect, it } from 'vitest';
import { act, fireEvent, render, screen, within } from '@testing-library/react';
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
});
