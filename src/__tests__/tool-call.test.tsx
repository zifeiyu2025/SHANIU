import { describe, expect, it } from 'vitest';
import { act, render, screen, within } from '@testing-library/react';
import { produce } from 'immer';
import { applyEvent } from '../store/reducers';
import { TerminalBlock } from '../components/blocks/TerminalBlock';
import { ToolCallCard } from '../components/blocks/ToolCallCard';
import { ChatView } from '../components/chat/ChatView';
import { createSessionStore } from '../store/session-store';
import { createMockTransport } from '../transport/mock';
import { initialSessionState } from '../store/session-store';
import type { SessionState } from '../store/session-store';
import type { ToolCallNode, ToolCallStatus } from '../nodes/types';
import type { EventMsg, ToolCallResult } from '../transport/types';

function initialState(): SessionState {
  return initialSessionState();
}

function reduce(state: SessionState, msg: EventMsg): SessionState {
  return produce(state, (draft) => applyEvent(draft, msg));
}

const BEGIN: EventMsg = {
  type: 'tool_call_begin',
  call_id: 'c1',
  tool: 'unified_exec',
  args: { command: 'npm run build' },
};

describe('tool_call 归约器', () => {
  it('begin → delta → end：分通道累积输出，exit_code 0 收敛 ok', () => {
    let state = reduce(initialState(), BEGIN);
    expect(state.nodes).toHaveLength(1);
    expect(state.nodes[0]).toMatchObject({
      kind: 'tool_call',
      callId: 'c1',
      tool: 'unified_exec',
      status: 'running',
      stdout: '',
      stderr: '',
    });

    // 逐 delta 分别断言，避免"一次性断言最终数组"掩盖中间态错误
    state = reduce(state, { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stdout', delta: 'build ok\n' });
    expect(state.nodes).toHaveLength(1);
    expect(state.nodes[0]).toMatchObject({ status: 'running', stdout: 'build ok\n', stderr: '' });

    state = reduce(state, { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stderr', delta: 'warn\n' });
    expect(state.nodes).toHaveLength(1);
    expect(state.nodes[0]).toMatchObject({ status: 'running', stdout: 'build ok\n', stderr: 'warn\n' });

    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null });
    expect(state.nodes[0]).toMatchObject({ status: 'ok', exitCode: 0, error: null });
  });

  it('同一 callId 的 begin 重放不新增节点（rollout replay）', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_begin', call_id: 'c1', tool: 'unified_exec', args: {} });
    expect(state.nodes).toHaveLength(1);
  });

  it('跨轮复用同一 call_id 时新建卡片，不吞掉新一轮调用', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stdout', delta: '1st\n' });
    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null });
    state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'completed' });

    // DeepSeek 系 call_0/call_1 每轮从 0 重来：第 2 轮同名 begin 必须建新卡
    state = reduce(state, { type: 'turn_started', turn: 2 });
    state = reduce(state, { type: 'tool_call_begin', call_id: 'c1', tool: 'unified_exec', args: {} });
    expect(state.nodes).toHaveLength(2);

    state = reduce(state, { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stdout', delta: '2nd\n' });
    // 输出落到第 2 轮的新卡，历史卡片不被污染
    expect(state.nodes[0]).toMatchObject({ status: 'ok', stdout: '1st\n' });
    expect(state.nodes[1]).toMatchObject({ status: 'running', stdout: '2nd\n' });
  });

  it('多调用并存：交错的 delta 各归其节点', () => {
    let state = reduce(initialState(), { type: 'tool_call_begin', call_id: 'c1', tool: 'unified_exec', args: {} });
    state = reduce(state, { type: 'tool_call_begin', call_id: 'c2', tool: 'fs_read', args: {} });
    expect(state.nodes).toHaveLength(2);

    state = reduce(state, { type: 'tool_call_output_delta', call_id: 'c2', channel: 'stdout', delta: 'two\n' });
    state = reduce(state, { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stdout', delta: 'one\n' });

    expect(state.nodes[0]).toMatchObject({ callId: 'c1', stdout: 'one\n' });
    expect(state.nodes[1]).toMatchObject({ callId: 'c2', stdout: 'two\n' });
  });

  it('未知 call_id 的 delta / end 静默忽略，不产生幽灵节点', () => {
    let state = reduce(initialState(), { type: 'tool_call_output_delta', call_id: 'ghost', channel: 'stdout', delta: 'x' });
    state = reduce(state, { type: 'tool_call_end', call_id: 'ghost', exit_code: 0, error: null });
    expect(state.nodes).toEqual([]);
  });

  it('非零退出码收敛为 error', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: 127, error: null });
    expect(state.nodes[0]).toMatchObject({ status: 'error', exitCode: 127 });
  });

  it('error 字段优先于退出码（0 退出仍判失败）', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: '输出超过上限被截断' });
    expect(state.nodes[0]).toMatchObject({ status: 'error', error: '输出超过上限被截断' });
  });

  it('turn 取消/失败时 running 节点收敛，不留永久 spinner', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'cancelled' });
    expect(state.nodes[0]).toMatchObject({ status: 'cancelled' });

    let failed = reduce(initialState(), BEGIN);
    failed = reduce(failed, { type: 'turn_completed', turn: 1, reason: 'failed' });
    expect(failed.nodes[0]).toMatchObject({ status: 'error' });
  });

  it('completed 却仍 running（后端漏发 end）也收敛为 error，不留永久 spinner', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'completed' });
    expect(state.nodes[0]).toMatchObject({ status: 'error' });
  });

  // 真实后端取消顺序（agent_loop/mod.rs:309）：end{exit_code:None, error:"已取消"} → turn_completed(cancelled)
  it('取消时先补 end（退出码缺失）再 turn_completed，最终收敛为 cancelled', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: null, error: '已取消' });
    // 单看 end 事件只能判失败，取消语义要等 turn_completed 才能确认
    expect(state.nodes[0]).toMatchObject({ status: 'error' });

    state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'cancelled' });
    expect(state.nodes[0]).toMatchObject({ status: 'cancelled', error: '已取消' });
  });

  it('已按退出码判定的失败不会被后续取消覆盖', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: 1, error: null });
    state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'cancelled' });
    expect(state.nodes[0]).toMatchObject({ status: 'error', exitCode: 1 });
  });

  // ── SH-074：result 为权威判据（docs/04 §1.2.1）────────────────────────────
  it('result 存在时收到 end 即定状态，不必等 turn_completed（result/status 同分支写完）', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' });
    expect(state.nodes[0]).toMatchObject({ status: 'ok', result: 'ok' });
  });

  it('result 为唯一判据：四种取值经 turn_completed(cancelled) 后均不被改写', () => {
    const cases: Array<[ToolCallResult, ToolCallStatus]> = [
      ['ok', 'ok'],
      ['error', 'error'],
      ['cancelled', 'cancelled'],
      ['denied', 'denied'],
    ];
    for (const [result, expected] of cases) {
      let state = reduce(initialState(), BEGIN);
      state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: null, error: null, result });
      state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'cancelled' });
      expect(state.nodes[0]).toMatchObject({ status: expected, result });
    }
  });

  it('denied 不被后续取消抹成 cancelled（独立状态）', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, {
      type: 'tool_call_end',
      call_id: 'c1',
      exit_code: null,
      error: '用户拒绝',
      result: 'denied',
    });
    state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'cancelled' });
    expect(state.nodes[0]).toMatchObject({ status: 'denied', error: '用户拒绝' });
  });

  it('result=cancelled 且 exit_code=1：状态 cancelled，exitCode 作为证据字段保留', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: 1, error: null, result: 'cancelled' });
    expect(state.nodes[0]).toMatchObject({ status: 'cancelled', exitCode: 1 });
  });

  it('result=ok 但 error 非空：状态判 ok，error 原样保留（前端不修正）', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: '输出被截断', result: 'ok' });
    expect(state.nodes[0]).toMatchObject({ status: 'ok', error: '输出被截断' });
  });

  it('运行时未知的 result 值归 error：不留 running、原文保留（灰度/版本错配防线）', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, {
      type: 'tool_call_end',
      call_id: 'c1',
      exit_code: null,
      error: null,
      result: 'timeout' as unknown as ToolCallResult,
    });
    expect(state.nodes[0]).toMatchObject({ status: 'error', result: 'timeout' });
  });

  it('权威信息只齐一半（result 有值但 status 仍 running）：轮次终点仍兜底收敛', () => {
    let state = reduce(initialState(), BEGIN);
    state = reduce(state, { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' });
    // 人为造出"漏写 status"的残缺节点，验证联合条件确实能兜住
    state = produce(state, (draft) => {
      const node = draft.nodes[0];
      if (node.kind === 'tool_call') node.status = 'running';
    });
    state = reduce(state, { type: 'turn_completed', turn: 1, reason: 'completed' });
    expect(state.nodes[0]).toMatchObject({ status: 'error' });
  });
});

const RUNNING_NODE: ToolCallNode = {
  kind: 'tool_call',
  id: 't1',
  callId: 'c1',
  tool: 'unified_exec',
  args: { command: 'npm run build' },
  status: 'running',
  stdout: 'building...\n',
  stderr: '',
};

describe('TerminalBlock', () => {
  const lines = (count: number) => Array.from({ length: count }, (_, i) => `L${i + 1}`).join('\n');
  const renderedLines = () =>
    (screen.getByTestId('terminal-stdout').textContent ?? '').split('\n');

  it('只渲染末尾 200 行，stdout/stderr 截断提示各自独立', () => {
    render(<TerminalBlock stdout={lines(250)} stderr={lines(240)} />);

    const stdoutNotice = screen.getByTestId('terminal-truncated-stdout');
    const stderrNotice = screen.getByTestId('terminal-truncated-stderr');
    expect(stdoutNotice.textContent).toContain('200');
    expect(stderrNotice.textContent).toContain('200');
    expect(renderedLines()).toHaveLength(200);
    expect(renderedLines()[0]).toBe('L51');
    expect(renderedLines()[199]).toBe('L250');
  });

  // 恰好边界：证伪 off-by-one（199/200 不截断，201 才截断且首行为第 2 行）
  it('199 行：全量渲染且无截断提示', () => {
    render(<TerminalBlock stdout={lines(199)} stderr="" />);
    expect(renderedLines()).toHaveLength(199);
    expect(screen.queryByTestId('terminal-truncated-stdout')).toBeNull();
  });

  it('200 行（含末尾换行）：全量渲染且无截断提示', () => {
    render(<TerminalBlock stdout={`${lines(200)}\n`} stderr="" />);
    expect(renderedLines()).toHaveLength(200);
    expect(screen.queryByTestId('terminal-truncated-stdout')).toBeNull();
  });

  it('201 行（含末尾换行）：渲染末尾 200 行且给出提示', () => {
    render(<TerminalBlock stdout={`${lines(201)}\n`} stderr="" />);
    expect(screen.getByTestId('terminal-truncated-stdout')).toBeTruthy();
    const rendered = renderedLines();
    expect(rendered).toHaveLength(200);
    expect(rendered[0]).toBe('L2');
    expect(rendered[199]).toBe('L201');
  });

  it('空输出不渲染空壳', () => {
    const view = render(<TerminalBlock stdout="" stderr="" />);
    expect(view.container.firstChild).toBeNull();
  });

  it('props 更新：追加输出后按新内容重渲染', () => {
    const view = render(<TerminalBlock stdout={'L1\nL2'} stderr="" />);
    expect(renderedLines()).toHaveLength(2);

    view.rerender(<TerminalBlock stdout={lines(10)} stderr="" />);
    const rendered = renderedLines();
    expect(rendered).toHaveLength(10);
    expect(rendered[9]).toBe('L10');
    expect(screen.queryByTestId('terminal-truncated-stdout')).toBeNull();
  });
});

describe('ToolCallCard', () => {
  it('流式中间态：活动指示 + 执行中徽标 + 已有输出可见', () => {
    render(<ToolCallCard node={RUNNING_NODE} />);

    expect(screen.getByTestId('tool-call-c1-spinner')).toBeTruthy();
    expect(screen.getByText('执行中')).toBeTruthy();
    expect(screen.getByText(/npm run build/)).toBeTruthy();
    expect(screen.getByTestId('tool-call-c1-stdout').textContent).toContain('building...');
    expect(screen.queryByTestId('tool-call-c1-stderr')).toBeNull();
  });

  it('完成态：spinner 消失、stderr 走 danger 色、错误信息可见', () => {
    render(
      <ToolCallCard
        node={{
          ...RUNNING_NODE,
          status: 'error',
          exitCode: 1,
          stdout: 'build failed\n',
          stderr: 'error TS2304\n',
          error: '命令以非零状态退出',
        }}
      />,
    );

    expect(screen.queryByTestId('tool-call-c1-spinner')).toBeNull();
    expect(screen.getByText('失败')).toBeTruthy();
    expect(screen.getByRole('alert').textContent).toContain('非零状态退出');
    const stderr = screen.getByTestId('tool-call-c1-stderr');
    expect(stderr.textContent).toContain('TS2304');
    expect(stderr.className).toContain('text-danger');
  });

  it('无输出时不渲染终端块', () => {
    render(<ToolCallCard node={{ ...RUNNING_NODE, status: 'ok', exitCode: 0, stdout: '', stderr: '' }} />);
    expect(screen.getByText('完成')).toBeTruthy();
    expect(screen.queryByTestId('tool-call-c1-block')).toBeNull();
  });

  it('取消态保留 error 文案但降为中性色（真实失败原因不能被吞）', () => {
    render(<ToolCallCard node={{ ...RUNNING_NODE, status: 'cancelled', error: '命令超时' }} />);
    expect(screen.getByText('已取消')).toBeTruthy();
    const alert = screen.getByRole('alert');
    expect(alert.textContent).toBe('命令超时');
    expect(alert.className).not.toContain('text-danger');
  });

  it('denied：独立徽标「已拒绝」、无 spinner、文案中性色', () => {
    render(
      <ToolCallCard
        node={{ ...RUNNING_NODE, status: 'denied', result: 'denied', error: '用户拒绝' }}
      />,
    );
    const card = screen.getByTestId('tool-call-c1');
    expect(card.getAttribute('data-status')).toBe('denied');
    expect(within(card).getByText('已拒绝')).toBeTruthy();
    expect(within(card).queryByTestId('tool-call-c1-spinner')).toBeNull();
    const alert = within(card).getByRole('alert');
    expect(alert.textContent).toBe('用户拒绝');
    expect(alert.className).toContain('text-tertiary');
    expect(alert.className).not.toContain('text-danger');
  });

  it('result=ok 却带 error 文案：徽标仍「完成」，文案中性色（不出绿标红字矛盾）', () => {
    render(
      <ToolCallCard
        node={{ ...RUNNING_NODE, status: 'ok', result: 'ok', exitCode: 0, error: '输出被截断' }}
      />,
    );
    const card = screen.getByTestId('tool-call-c1');
    expect(card.getAttribute('data-status')).toBe('ok');
    expect(within(card).getByText('完成')).toBeTruthy();
    expect(within(card).getByRole('alert').className).not.toContain('text-danger');
  });

  it('未知 result 值不抛错：降级为 error 且原文可见（防整棵会话白屏）', () => {
    const node = {
      ...RUNNING_NODE,
      status: 'error',
      result: 'timeout',
      error: '未知的结果取值',
    } as unknown as ToolCallNode;
    render(<ToolCallCard node={node} />);
    const card = screen.getByTestId('tool-call-c1');
    expect(card.getAttribute('data-status')).toBe('error');
    expect(within(card).getByText('失败')).toBeTruthy();
    expect(within(card).getByRole('alert').textContent).toBe('未知的结果取值');
  });
});

describe('ChatView 工具调用接线', () => {
  it('流式中间态与完成态分两次断言：delta 到达时为 running，end 后才收敛', () => {
    const store = createSessionStore();
    render(<ChatView store={store} transport={createMockTransport()} />);

    act(() => {
      store.submitUserTask('跑一下构建');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch(BEGIN);
      store.dispatch({ type: 'tool_call_output_delta', call_id: 'c1', channel: 'stdout', delta: 'build ok\n' });
    });

    const card = screen.getByTestId('tool-call-c1');
    expect(card.getAttribute('data-status')).toBe('running');
    expect(screen.getByTestId('tool-call-c1-spinner')).toBeTruthy();
    expect(screen.getByTestId('tool-call-c1-stdout').textContent).toContain('build ok');

    act(() => {
      store.dispatch({ type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null });
    });

    expect(screen.getByTestId('tool-call-c1').getAttribute('data-status')).toBe('ok');
    expect(screen.queryByTestId('tool-call-c1-spinner')).toBeNull();
    expect(screen.getByTestId('tool-call-c1-stdout').textContent).toContain('build ok');
  });

  it('多工具调用按 callId 各自定位，输出不互相串', () => {
    const store = createSessionStore();
    render(<ChatView store={store} transport={createMockTransport()} />);

    act(() => {
      store.submitUserTask('并行跑两个命令');
      store.dispatch({ type: 'turn_started', turn: 1 });
      store.dispatch({ type: 'tool_call_begin', call_id: 'call-a', tool: 'unified_exec', args: { command: 'a' } });
      store.dispatch({ type: 'tool_call_begin', call_id: 'call-b', tool: 'fs_read', args: { path: 'b' } });
      store.dispatch({ type: 'tool_call_output_delta', call_id: 'call-b', channel: 'stdout', delta: 'from-b\n' });
      store.dispatch({ type: 'tool_call_output_delta', call_id: 'call-a', channel: 'stdout', delta: 'from-a\n' });
    });

    // testid 作用域化后，两个并存的调用各自只有一份 -stdout，不会多匹配
    expect(screen.getByTestId('tool-call-call-a-stdout').textContent).toContain('from-a');
    expect(screen.getByTestId('tool-call-call-b-stdout').textContent).toContain('from-b');
    expect(screen.getByTestId('tool-call-call-a-stdout').textContent).not.toContain('from-b');
    expect(screen.getByTestId('tool-call-call-b-stdout').textContent).not.toContain('from-a');
  });
});
