import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { Composer } from '../components/chat/Composer';
import { createSessionStore } from '../store/session-store';
import { stubTransport } from './stub-transport';
import type { SandboxMode } from '../transport/types';

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
    render(<Composer store={store} transport={transport} running={false} blocked={false} modelLabel="M0-Mock" sandboxMode="read_only" onOpenSettings={() => {}} />);

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
    render(<Composer store={store} transport={transport} running={false} blocked={false} modelLabel="M0-Mock" sandboxMode="read_only" onOpenSettings={() => {}} />);

    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);
    fireEvent.change(input, { target: { value: '两行' } });
    fireEvent.keyDown(input, { key: 'Enter', shiftKey: true });

    expect(sendTask).not.toHaveBeenCalled();
  });

  it('输入法组词中的 Enter 不提交（isComposing 与 keyCode 229 两条路径）', () => {
    const store = createSessionStore();
    const { transport, sendTask } = spyTransport();
    render(<Composer store={store} transport={transport} running={false} blocked={false} modelLabel="M0-Mock" sandboxMode="read_only" onOpenSettings={() => {}} />);
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
    render(<Composer store={store} transport={transport} running={false} blocked={false} modelLabel="M0-Mock" sandboxMode="read_only" onOpenSettings={() => {}} />);
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
    render(<Composer store={store} transport={transport} running={false} blocked={false} modelLabel="M0-Mock" sandboxMode="read_only" onOpenSettings={() => {}} />);

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
    render(<Composer store={store} transport={transport} running blocked={false} modelLabel="M0-Mock" sandboxMode="read_only" onOpenSettings={() => {}} />);

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

// SH-040 之后，输入卡的权限与模型席位不再是占位：它们显示后端真状态，点击落到设置卡。
describe('Composer 权限与模型席位', () => {
  function renderComposer(over: Partial<{ modelLabel: string; sandboxMode: SandboxMode; onOpenSettings: () => void }> = {}) {
    const store = createSessionStore();
    const { transport } = spyTransport();
    const onOpenSettings = vi.fn();
    render(
      <Composer
        store={store}
        transport={transport}
        running={false}
        blocked={false}
        modelLabel="deepseek-flash"
        sandboxMode="workspace_write"
        onOpenSettings={onOpenSettings}
        {...over}
      />,
    );
    return { store, onOpenSettings };
  }

  it('显示真档位而不是写死的「完全权限」', () => {
    renderComposer({ sandboxMode: 'workspace_write' });
    expect(screen.getByTestId('composer-sandbox').textContent).toContain('工作区可写');
  });

  it('换一个档位就换一个标签：席位是状态镜像，不是常量', () => {
    renderComposer({ sandboxMode: 'read_only' });
    expect(screen.getByTestId('composer-sandbox').textContent).toContain('只读');
  });

  it('danger 档用中性标签，机器名不外泄', () => {
    renderComposer({ sandboxMode: 'danger_full_access' });
    expect(screen.getByTestId('composer-sandbox').textContent).toContain('完全访问');
    expect(screen.getByTestId('composer-sandbox').textContent).not.toContain('danger');
  });

  it('模型席位显示 settings 里的真模型名', () => {
    renderComposer({ modelLabel: 'LongCat-2.5' });
    expect(screen.getByTestId('composer-model').textContent).toContain('LongCat-2.5');
  });

  it('两个席位点击都打开设置卡——本仓没有内联切换能力', () => {
    const { onOpenSettings } = renderComposer();
    fireEvent.click(screen.getByTestId('composer-sandbox'));
    fireEvent.click(screen.getByTestId('composer-model'));
    expect(onOpenSettings).toHaveBeenCalledTimes(2);
  });

  it('席位 title 带上当前档位，悬停就能确认后端状态', () => {
    renderComposer({ sandboxMode: 'read_only' });
    expect(screen.getByTestId('composer-sandbox').getAttribute('title')).toContain('只读');
  });
});
