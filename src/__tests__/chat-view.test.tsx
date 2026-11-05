import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen } from '@testing-library/react';
import { ChatView } from '../components/chat/ChatView';
import { createSessionStore } from '../store/session-store';
import { createMockTransport } from '../transport/mock';
import type { Transport } from '../transport/types';

/** 只记调用、不产事件的传输替身：队列/取消类用例关心的是"有没有真的发出去"。 */
function spyTransport() {
  const sendTask = vi.fn().mockResolvedValue(undefined);
  const cancel = vi.fn().mockResolvedValue(undefined);
  const transport: Transport = {
    ping: vi.fn().mockResolvedValue('n'),
    sendTask,
    cancel,
    getSettings: vi.fn().mockResolvedValue({
      baseUrl: 'https://mock.local/v1',
      model: 'M0-Mock',
      workspaceRoot: 'D:/MyWorkspace/demo',
      hasApiKey: false,
    }),
    saveSettings: vi.fn().mockResolvedValue(undefined),
    approve: vi.fn().mockResolvedValue(undefined),
    onEvent: vi.fn().mockReturnValue(() => {}),
  };
  return { transport, sendTask, cancel };
}

/** jsdom 没有布局引擎：滚动几何只能自己定义（scrollTo 已由 test-setup 提供空实现）。 */
function fakeScrollMetrics(
  el: HTMLElement,
  { scrollHeight, clientHeight }: { scrollHeight: number; clientHeight: number },
) {
  Object.defineProperty(el, 'scrollHeight', { configurable: true, value: scrollHeight });
  Object.defineProperty(el, 'clientHeight', { configurable: true, value: clientHeight });
}

// AGENTS §5：UI 组件至少覆盖「流式中间态」与「完成态」两种渲染
describe('ChatView', () => {
  afterEach(() => {
    vi.useRealTimers();
  });
  it('流式中间态：用户任务与部分回复可见、输入仍可用、主按钮是停止生成', () => {
    const store = createSessionStore();
    const transport = createMockTransport();
    render(<ChatView store={store} transport={transport} />);

    act(() => {
      store.submitUserTask('读一下 README');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'agent_message_delta', delta: '正在读取' });
    });

    // 用户气泡与 header crumb 标题都来自首条任务（dsh displayTitle 口径），分别断言
    expect(screen.getByTestId('user-bubble').textContent).toContain('读一下 README');
    expect(screen.getByTestId('session-title').textContent).toBe('读一下 README');
    expect(screen.getByText('正在读取')).toBeTruthy();
    expect(screen.getByTestId('streaming-cursor')).toBeTruthy();
    expect(screen.getByRole('button', { name: '停止生成' })).toBeTruthy();
    // dsh InputBar.tsx:171 `editable = live && !locked && !machineBusy`——running 不进锁判据，
    // 流式期间照样能打字（停止不锁输入）。
    expect(
      (screen.getByPlaceholderText(/描述你想要构建的内容/) as HTMLTextAreaElement).disabled,
    ).toBe(false);
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
    expect(screen.getByRole('button', { name: '发送消息' })).toBeTruthy();
    expect(
      (screen.getByPlaceholderText(/描述你想要构建的内容/) as HTMLTextAreaElement).disabled,
    ).toBe(false);
    // dsh StatsPills：紧凑总量（10 输入 + 5 输出 = 15）
    expect(screen.getByTestId('token-usage').textContent).toContain('15');
  });

  it('挂起审批才锁输入（dsh blocked），主按钮仍是停止生成', () => {
    const store = createSessionStore();
    const { transport } = spyTransport();
    render(<ChatView store={store} transport={transport} />);

    act(() => {
      store.submitUserTask('跑个命令');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({
        type: 'exec_approval_request',
        request_id: 'appr-1',
        tool: 'unified_exec',
        args: { command: 'rm -rf /tmp/x' },
        risk: 'execute',
      });
    });

    expect(
      (screen.getByPlaceholderText(/等待审批裁决/) as HTMLTextAreaElement).disabled,
    ).toBe(true);
    // blocked 时主按钮回到停止（dsh primaryStops 的 `empty || blocked` 分支）
    expect(screen.getByRole('button', { name: '停止生成' })).toBeTruthy();
  });

  it('流式中排队：入队时不发送，本轮结束自动按序发出', async () => {
    const store = createSessionStore();
    const { transport, sendTask } = spyTransport();
    render(<ChatView store={store} transport={transport} />);

    // 走真实用户路径：空态 hero 卡发第一条
    fireEvent.change(screen.getByPlaceholderText(/描述你想要构建的内容/), {
      target: { value: '第一条' },
    });
    fireEvent.click(screen.getByRole('button', { name: '发送消息' }));
    expect(sendTask).toHaveBeenCalledTimes(1);
    // async act 把 sendTask 的微任务冲掉：sending 是"IPC 在途"，回执到了就该落回 false
    await act(async () => {
      store.dispatch({ type: 'turn_started', turn: 1 });
    });

    fireEvent.change(screen.getByPlaceholderText(/描述你想要构建的内容/), {
      target: { value: '第二条' },
    });
    fireEvent.click(screen.getByRole('button', { name: '排队发送' }));

    expect(store.getState().queued).toEqual(['第二条']);
    expect(screen.getByTestId('queue-preview').textContent).toBe('第二条');
    expect(sendTask).toHaveBeenCalledTimes(1); // 排队期间绝不并发提交（后端只有一个任务槽）

    await act(async () => {
      store.dispatch({ type: 'turn_completed', turn: 1, reason: 'completed' });
    });

    expect(sendTask).toHaveBeenCalledTimes(2);
    expect(sendTask).toHaveBeenLastCalledWith('第二条');
    expect(store.getState().queued).toEqual([]);
    expect(screen.queryByTestId('queue-dock')).toBeNull(); // 队列空时整块不渲染
  });

  it('排队后可以撤回', async () => {
    const store = createSessionStore();
    const { transport, sendTask } = spyTransport();
    render(<ChatView store={store} transport={transport} />);

    // 队列条挂在会话流的输入卡上方，所以先进入会话态
    fireEvent.change(screen.getByPlaceholderText(/描述你想要构建的内容/), {
      target: { value: '第一条' },
    });
    fireEvent.click(screen.getByRole('button', { name: '发送消息' }));
    await act(async () => {
      store.dispatch({ type: 'turn_started', turn: 1 });
    });

    act(() => {
      store.enqueueTask('甲');
      store.enqueueTask('乙');
    });
    // 多条默认折叠成计数头（dsh QueueDock：collapsed 初值 true，rowCount===1 才直接列明细）
    expect(screen.getByTestId('queue-dock').textContent).toContain('2 条排队消息');
    expect(screen.queryAllByRole('button', { name: '删除排队消息' })).toHaveLength(0);

    fireEvent.click(screen.getByRole('button', { name: /2 条排队消息/ }));
    expect(screen.getAllByRole('button', { name: '删除排队消息' })).toHaveLength(2);

    fireEvent.click(screen.getAllByRole('button', { name: '删除排队消息' })[0]);
    expect(store.getState().queued).toEqual(['乙']);
    expect(sendTask).toHaveBeenCalledTimes(1); // 只在空闲跃变时放队首，撤回不触发发送
  });

  it('首条发出后输入卡不换实例：正在输入的下一条草稿与焦点都还在', () => {
    const store = createSessionStore();
    const { transport, sendTask } = spyTransport();
    render(<ChatView store={store} transport={transport} />);

    const input = screen.getByPlaceholderText(/描述你想要构建的内容/);
    input.focus();
    fireEvent.change(input, { target: { value: '第一条' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(sendTask).toHaveBeenCalledWith('第一条');

    // 进入会话态后必须是同一个 DOM 节点（dsh 的 composer 常驻，只由 CSS 位移）
    expect(screen.getByPlaceholderText(/描述你想要构建的内容/)).toBe(input);

    fireEvent.change(input, { target: { value: '还没打完的下一条' } });
    act(() => {
      store.dispatch({ type: 'turn_started', turn: 1 });
    });
    expect((input as HTMLTextAreaElement).value).toBe('还没打完的下一条');
    expect(document.activeElement).toBe(input);
  });

  it('上翻超过阈值出现「回到底部」，点击后回到贴底态', () => {
    const store = createSessionStore();
    const { transport } = spyTransport();
    render(<ChatView store={store} transport={transport} />);
    act(() => {
      store.submitUserTask('任务');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'agent_message_delta', delta: '内容' });
    });

    const scroller = screen.getByTestId('scroll-body');
    fakeScrollMetrics(scroller, { scrollHeight: 2000, clientHeight: 600 });
    expect(screen.queryByTestId('to-bottom')).toBeNull(); // 贴底时不出现

    scroller.scrollTop = 0; // 距底 1400px，远超 dsh 的 24px 阈值
    fireEvent.scroll(scroller);
    expect(screen.getByTestId('to-bottom')).toBeTruthy();

    fireEvent.click(screen.getByTestId('to-bottom'));
    expect(screen.queryByTestId('to-bottom')).toBeNull();
  });

  it('贴底时新内容跟着滚到底，上翻时不再被拽回', () => {
    const store = createSessionStore();
    const { transport } = spyTransport();
    render(<ChatView store={store} transport={transport} />);
    act(() => {
      store.submitUserTask('任务');
      store.dispatch({ type: 'turn_started', turn: 1 });
    });

    const scroller = screen.getByTestId('scroll-body');
    fakeScrollMetrics(scroller, { scrollHeight: 2000, clientHeight: 600 });
    const spy = vi.fn();
    scroller.scrollTo = spy;

    // 贴底（距底 0px）→ 新内容必须跟随
    scroller.scrollTop = 1400;
    fireEvent.scroll(scroller);
    act(() => {
      store.dispatch({ type: 'agent_message_delta', delta: '增量一' });
    });
    expect(spy).toHaveBeenCalled();

    // 上翻 → 同样的新内容不得把用户拽回去
    spy.mockClear();
    scroller.scrollTop = 0;
    fireEvent.scroll(scroller);
    act(() => {
      store.dispatch({ type: 'agent_message_delta', delta: '增量二' });
    });
    expect(spy).not.toHaveBeenCalled();
  });

  it('Esc = 停止生成；浮层开着时 Esc 归浮层，不顺手取消任务', () => {
    const store = createSessionStore();
    const { transport, cancel } = spyTransport();
    render(<ChatView store={store} transport={transport} />);

    act(() => {
      store.dispatch({ type: 'turn_started', turn: 1 });
    });

    const dialog = document.createElement('div');
    dialog.setAttribute('role', 'dialog');
    document.body.appendChild(dialog);
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(cancel).not.toHaveBeenCalled(); // 分层：设置卡先吃 Esc
    dialog.remove();

    fireEvent.keyDown(window, { key: 'Escape' });
    expect(cancel).toHaveBeenCalledTimes(1);
  });

  it('空闲时不挂 Esc 监听（没在跑的东西可停）', () => {
    const store = createSessionStore();
    const { transport, cancel } = spyTransport();
    render(<ChatView store={store} transport={transport} />);
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(cancel).not.toHaveBeenCalled();
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
    expect(screen.getByTestId('user-bubble').textContent).toContain('旧任务');
    expect(screen.queryByTestId('greeting')).toBeNull(); // 进入会话流后欢迎语卸载

    vi.setSystemTime(new Date('2026-11-05T14:30:00'));
    // 「新会话」按钮按 dsh 席位只在侧栏（App 层用例覆盖点击），这里验会话区契约：
    // store.reset() 递增代次 → 会话区必须重挂载欢迎语并重新取时
    act(() => {
      store.reset();
    });
    expect(screen.queryByTestId('user-bubble')).toBeNull(); // 会话已清空
    expect(screen.getByTestId('session-title').textContent).toBe('新会话');
    expect(screen.getByTestId('greeting').textContent).toBe('下午好呀，接下来交给我吧');
  });

  it('空态下新建会话：换 key 重新挂载取时，且欢迎语节点不重复', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-11-05T12:30:00'));
    const store = createSessionStore();
    render(<ChatView store={store} transport={createMockTransport()} />);
    expect(screen.getByTestId('greeting').textContent).toBe('中午好呀，要不要先休息一下');

    vi.setSystemTime(new Date('2026-11-05T14:30:00'));
    act(() => {
      store.reset();
    });
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
