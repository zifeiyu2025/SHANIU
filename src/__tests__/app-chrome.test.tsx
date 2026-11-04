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

  it('标题栏按钮折叠/展开左侧栏', () => {
    render(<App transport={createMockTransport()} />);
    expect(screen.getByText('SHANIU')).toBeTruthy();

    act(() => {
      fireEvent.click(screen.getByRole('button', { name: '折叠侧栏' }));
    });
    expect(screen.queryByText('SHANIU')).toBeNull();

    act(() => {
      fireEvent.click(screen.getByRole('button', { name: '展开侧栏' }));
    });
    expect(screen.getByText('SHANIU')).toBeTruthy();
  });

  it('浏览器模式（非 Tauri）不渲染窗口控制按钮', () => {
    render(<App transport={createMockTransport()} />);
    expect(screen.queryByRole('button', { name: '最小化' })).toBeNull();
    expect(screen.queryByRole('button', { name: '最大化' })).toBeNull();
    expect(screen.queryByRole('button', { name: '关闭' })).toBeNull();
  });

  it('新会话按钮清空会话节点', () => {
    const { unmount } = render(<App transport={createMockTransport()} />);
    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);

    // 通过 store 直接灌入消息（App 内部 store 不经 input 提交也能被重置）
    act(() => {
      screen.getByRole('button', { name: '发送' }); // 输入卡片存在
    });
    expect(screen.getByTestId('empty-hero')).toBeTruthy();

    // 模拟有会话内容：通过键盘输入并提交
    fireEvent.change(input, { target: { value: '跑个任务' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(screen.queryByTestId('empty-hero')).toBeNull();

    // 侧栏有两处「新会话」（主按钮 + 工作区列表项），点主按钮（DOM 顺序在前）
    fireEvent.click(screen.getAllByRole('button', { name: '新会话' })[0]);
    expect(screen.getByTestId('empty-hero')).toBeTruthy();
    unmount();
  });
});
