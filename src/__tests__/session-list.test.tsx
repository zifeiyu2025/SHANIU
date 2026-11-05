import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { SessionList } from '../components/sessions/SessionList';
import { createSessionStore } from '../store/session-store';
import { stubTransport } from './stub-transport';
import type { SessionEntry, SessionSummary, Transport } from '../transport/types';

function summary(over: Partial<SessionSummary> & { agentId: string }): SessionSummary {
  return {
    parentId: null,
    createdAt: Date.now() - 5 * 60_000,
    updatedAt: Date.now() - 5 * 60_000,
    model: 'M0-Mock',
    workspaceRoot: 'D:/MyWorkspace/demo',
    title: '把端口改成 9090',
    itemCount: 4,
    ...over,
  };
}

const history: SessionEntry[] = [
  { kind: 'message', role: 'user', text: '把端口改成 9090' },
  { kind: 'message', role: 'assistant', text: '已改好' },
  { kind: 'tool_call', call_id: 'c1', tool: 'unified_exec', arguments: '{}', output: 'ok' },
];

/** 列表是异步取回的（command 返回 Promise），断言前必须先等它落地。 */
async function renderWith(transport: Transport, store = createSessionStore()) {
  const view = render(<SessionList store={store} transport={transport} />);
  return { view, store };
}

describe('SessionList 会话列表', () => {
  it('列出摘要：标题、相对时间与模型', async () => {
    const { store } = await renderWith(
      stubTransport({ listSessions: vi.fn().mockResolvedValue([summary({ agentId: 's1' })]) }),
    );
    expect(await screen.findByTitle('把端口改成 9090')).toBeTruthy();
    expect(store.getState().agentId).toBeNull();
    expect(screen.getByTestId('session-meta').textContent).toContain('5 分钟前');
    expect(screen.getByTestId('session-meta').textContent).toContain('M0-Mock');
  });

  it('没有会话时给的是可解释的空态，而不是假条目', async () => {
    await renderWith(stubTransport({ listSessions: vi.fn().mockResolvedValue([]) }));
    expect(await screen.findByText('还没有会话。发一条任务就会建档。')).toBeTruthy();
  });

  it('列表读不出时把错误显示出来，不静默退化成空列表', async () => {
    await renderWith(
      stubTransport({ listSessions: vi.fn().mockRejectedValue(new Error('档案目录被占用')) }),
    );
    expect(await screen.findByText(/档案目录被占用/)).toBeTruthy();
  });

  it('点一条会话：读档案 → 历史进 store → 认下 agent_id', async () => {
    const { store } = await renderWith(
      stubTransport({
        listSessions: vi.fn().mockResolvedValue([summary({ agentId: 's1' })]),
        loadSession: vi.fn().mockResolvedValue(history),
      }),
    );
    fireEvent.click(await screen.findByTitle('把端口改成 9090'));

    await waitFor(() => expect(store.getState().agentId).toBe('s1'));
    expect(store.getState().nodes).toHaveLength(3);
    // 重放出来的节点不能还挂着流式态：否则光标常驻、工具卡停在"运行中"
    expect(store.getState().nodes.some((n) => 'streaming' in n && n.streaming)).toBe(false);
  });

  it('打开历史会话换代次，且上一个会话的在途事件不再灌进来', async () => {
    const { store } = await renderWith(
      stubTransport({
        listSessions: vi.fn().mockResolvedValue([summary({ agentId: 's1' })]),
        loadSession: vi.fn().mockResolvedValue(history),
      }),
    );
    const before = store.getState().generation;
    fireEvent.click(await screen.findByTitle('把端口改成 9090'));
    await waitFor(() => expect(store.getState().agentId).toBe('s1'));
    expect(store.getState().generation).toBeGreaterThan(before);

    // 打开会话不等于提交任务，窗口保持关闭——SH-081 的门禁到这里仍然成立
    store.dispatch({ type: 'agent_message_delta', delta: '旧会话残留' });
    expect(store.getState().nodes.some((n) => 'text' in n && n.text.includes('旧会话残留'))).toBe(false);

    // 而在这个会话里真的提交任务时，窗口必须重开，否则新回复收不进来
    store.submitUserTask('继续改');
    store.dispatch({ type: 'agent_message_delta', delta: '新回复' });
    expect(store.getState().nodes.some((n) => 'text' in n && n.text.includes('新回复'))).toBe(true);
  });

  it('分支按钮按已落盘条目数请求 fork，并立刻打开新会话', async () => {
    const forkSession = vi.fn().mockResolvedValue('s2');
    const { store } = await renderWith(
      stubTransport({
        listSessions: vi.fn().mockResolvedValue([summary({ agentId: 's1', itemCount: 7 })]),
        loadSession: vi.fn().mockResolvedValue(history),
        forkSession,
      }),
    );
    fireEvent.click(await screen.findByLabelText('从这条会话分支'));

    // keepItems 必须等于档案里的条目数：前端不猜截断点，安全切分是后端的职责（SH-031）
    await waitFor(() => expect(forkSession).toHaveBeenCalledWith('s1', 7));
    await waitFor(() => expect(store.getState().agentId).toBe('s2'));
  });

  it('当前会话高亮只认 store 的 agentId', async () => {
    const { store } = await renderWith(
      stubTransport({
        listSessions: vi.fn().mockResolvedValue([
          summary({ agentId: 's1' }),
          summary({ agentId: 's2', title: '另一条' }),
        ]),
        loadSession: vi.fn().mockResolvedValue(history),
      }),
    );
    await screen.findByTitle('另一条');
    fireEvent.click(screen.getByTitle('另一条'));
    await waitFor(() => expect(store.getState().agentId).toBe('s2'));
    expect(screen.getByTitle('另一条').getAttribute('aria-current')).toBe('true');
    expect(screen.getByTitle('把端口改成 9090').getAttribute('aria-current')).toBeNull();
  });

  it('带父档案的条目标出分支来源（fork 出来的会话要能看出来）', async () => {
    await renderWith(
      stubTransport({
        listSessions: vi.fn().mockResolvedValue([summary({ agentId: 's2', parentId: 's1' })]),
      }),
    );
    await screen.findByTitle('把端口改成 9090');
    expect(screen.getByTestId('session-meta').textContent).toContain('分支');
  });
});
