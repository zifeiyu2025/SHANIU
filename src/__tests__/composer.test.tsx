import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { Composer } from '../components/chat/Composer';
import { createSessionStore } from '../store/session-store';
import { stubTransport } from './stub-transport';

function spyTransport() {
  const sendTask = vi.fn().mockResolvedValue('stub-session');
  const transport = stubTransport({ sendTask }, { baseUrl: '', workspaceRoot: '' });
  return { transport, sendTask };
}

// SH-088：键盘判据必须与 dsh keymap 一致（Enter 提交 / Shift+Enter 换行 / 组词中不提交）
describe('Composer 键盘与主按钮', () => {
  it('Enter 提交并清空草稿', () => {
    const store = createSessionStore();
    const { transport, sendTask } = spyTransport();
    render(<Composer store={store} transport={transport} running={false} blocked={false} />);

    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);
    fireEvent.change(input, { target: { value: '  写个测试  ' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    expect(sendTask).toHaveBeenCalledWith('写个测试', null);
    expect((input as HTMLTextAreaElement).value).toBe('');
    expect(store.getState().nodes.map((n) => n.kind)).toEqual(['user_message']);
  });

  it('Shift+Enter 走原生换行，不提交', () => {
    const store = createSessionStore();
    const { transport, sendTask } = spyTransport();
    render(<Composer store={store} transport={transport} running={false} blocked={false} />);

    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);
    fireEvent.change(input, { target: { value: '两行' } });
    fireEvent.keyDown(input, { key: 'Enter', shiftKey: true });

    expect(sendTask).not.toHaveBeenCalled();
  });

  it('输入法组词中的 Enter 不提交（isComposing 与 keyCode 229 两条路径）', () => {
    const store = createSessionStore();
    const { transport, sendTask } = spyTransport();
    render(<Composer store={store} transport={transport} running={false} blocked={false} />);
    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);
    fireEvent.change(input, { target: { value: '候选词' } });

    fireEvent.keyDown(input, { key: 'Enter', isComposing: true });
    expect(sendTask).not.toHaveBeenCalled();

    // 部分 WebView 只在 keyCode 上留痕（dsh keymap.ts:51-55 因此三重守卫）
    fireEvent.keyDown(input, { key: 'Enter', keyCode: 229 });
    expect(sendTask).not.toHaveBeenCalled();
  });

  it('组词结束后的 10ms 窗口内 Enter 仍不提交（compositionend 与 keydown 顺序不保证）', () => {
    const store = createSessionStore();
    const { transport, sendTask } = spyTransport();
    render(<Composer store={store} transport={transport} running={false} blocked={false} />);
    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);
    fireEvent.change(input, { target: { value: '已经选完词' } });

    // compositionend 之后立刻按 Enter：窗口期内仍按"组词残留"处理，不提交
    fireEvent.compositionEnd(input);
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(sendTask).not.toHaveBeenCalled();
  });

  it('空草稿时主按钮不可点；有草稿且空闲时是发送消息', () => {
    const store = createSessionStore();
    const { transport } = spyTransport();
    render(<Composer store={store} transport={transport} running={false} blocked={false} />);

    const send = screen.getByRole('button', { name: '发送消息' });
    expect(send).toHaveProperty('disabled', true);

    fireEvent.change(screen.getByPlaceholderText(/描述你想要构建的内容/), {
      target: { value: 'x' },
    });
    expect(screen.getByRole('button', { name: '发送消息' }).getAttribute('disabled')).toBeNull();
  });

  it('运行中且草稿为空 → 停止生成；草稿非空 → 排队发送（停止不锁输入）', () => {
    const store = createSessionStore();
    const { transport, sendTask } = spyTransport();
    render(<Composer store={store} transport={transport} running blocked={false} />);

    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);
    expect((input as HTMLTextAreaElement).disabled).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: '停止生成' }));
    expect(transport.cancel).toHaveBeenCalledTimes(1);

    fireEvent.change(input, { target: { value: '下一条' } });
    fireEvent.click(screen.getByRole('button', { name: '排队发送' }));
    expect(store.getState().queued).toEqual(['下一条']);
    expect(sendTask).not.toHaveBeenCalled();
  });
});
