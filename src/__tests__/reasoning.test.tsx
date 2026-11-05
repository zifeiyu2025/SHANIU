import { describe, expect, it } from 'vitest';
import { act, fireEvent, render, screen } from '@testing-library/react';
import { ChatView } from '../components/chat/ChatView';
import { createSessionStore } from '../store/session-store';
import { createMockTransport } from '../transport/mock';

// SH-083/FF-5 口径的前端一侧：思考增量必须单独成节点、默认折叠、正文开始时收口。
// 混进 agent_message 会让"思考过程"被当成答案内容，也就毁掉折叠这个可用性前提。
describe('reasoning 节点', () => {
  it('reasoning_delta 归约成独立节点，后续增量累加到同一节点', () => {
    const store = createSessionStore();
    store.dispatch({ type: 'turn_started', turn: 1 });
    store.dispatch({ type: 'reasoning_delta', delta: '先看 ' });
    store.dispatch({ type: 'reasoning_delta', delta: '文件结构' });

    const nodes = store.getState().nodes;
    expect(nodes).toHaveLength(1);
    expect(nodes[0]).toMatchObject({ kind: 'reasoning', text: '先看 文件结构', streaming: true });
  });

  it('正文开始即收口思考节点，两类节点不并存流式', () => {
    const store = createSessionStore();
    store.dispatch({ type: 'turn_started', turn: 1 });
    store.dispatch({ type: 'reasoning_delta', delta: '想完了' });
    store.dispatch({ type: 'agent_message_delta', delta: '答案是' });

    const nodes = store.getState().nodes;
    expect(nodes.map((n) => n.kind)).toEqual(['reasoning', 'agent_message']);
    expect(nodes[0]).toMatchObject({ streaming: false });
    expect(nodes[1]).toMatchObject({ streaming: true });
  });

  it('turn_completed 冻结仍在流式的思考节点（不留永久 spinner）', () => {
    const store = createSessionStore();
    store.dispatch({ type: 'turn_started', turn: 1 });
    store.dispatch({ type: 'reasoning_delta', delta: '只思考没答' });
    store.dispatch({ type: 'turn_completed', turn: 1, reason: 'cancelled' });

    expect(store.getState().nodes[0]).toMatchObject({ kind: 'reasoning', streaming: false });
  });

  it('渲染：默认折叠，展开才见正文；流式期间标题为"思考中"', () => {
    const store = createSessionStore();
    render(<ChatView store={store} transport={createMockTransport()} />);
    act(() => {
      store.submitUserTask('任务');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'reasoning_delta', delta: '私密推理原文' });
    });

    const toggle = screen.getByRole('button', { name: /思考中/ });
    expect(screen.queryByTestId('reasoning-body')).toBeNull(); // 默认收起
    fireEvent.click(toggle);
    expect(screen.getByTestId('reasoning-body').textContent).toContain('私密推理原文');

    act(() => {
      store.dispatch({ type: 'agent_message_delta', delta: '答案' });
    });
    // 正文开始后标题降级为"思考过程"，且答案独立可见
    expect(screen.getByRole('button', { name: /思考过程/ })).toBeTruthy();
    expect(screen.getByText('答案')).toBeTruthy();
  });
});
