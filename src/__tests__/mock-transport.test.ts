import { describe, expect, it } from 'vitest';
import { createMockTransport } from '../transport/mock';
import type { EventMsg } from '../transport/types';

describe('mock transport', () => {
  it('ping 原样返回 nonce', async () => {
    const transport = createMockTransport();
    await expect(transport.ping('nonce-abc')).resolves.toBe('nonce-abc');
  });

  it('sendTask 按序发出一轮完整事件流', async () => {
    const transport = createMockTransport();
    const events: EventMsg[] = [];
    transport.onEvent((msg) => events.push(msg));

    await transport.sendTask('写个 demo');

    const types = events.map((e) => e.type);
    expect(types[0]).toBe('turn_started');
    expect(types.filter((t) => t === 'agent_message_delta').length).toBeGreaterThan(0);
    expect(types).toContain('token_usage');
    expect(types[types.length - 1]).toBe('turn_completed');
    const last = events[events.length - 1] as Extract<EventMsg, { type: 'turn_completed' }>;
    expect(last.reason).toBe('completed');
    // 事件内容须包含任务文本（回显）
    const deltas = events
      .filter((e): e is Extract<EventMsg, { type: 'agent_message_delta' }> => e.type === 'agent_message_delta')
      .map((e) => e.delta)
      .join('');
    expect(deltas).toContain('写个 demo');
  });

  it('cancel 中断流式输出并以 cancelled 结束，且取消后不再有新 delta', async () => {
    const transport = createMockTransport();
    const events: EventMsg[] = [];
    transport.onEvent((msg) => events.push(msg));

    const running = transport.sendTask('长任务');
    await new Promise((r) => setTimeout(r, 150)); // 等若干 delta 流出
    const deltaCountBeforeCancel = events.filter((e) => e.type === 'agent_message_delta').length;
    expect(deltaCountBeforeCancel).toBeGreaterThan(0);

    await transport.cancel();
    await running;

    const deltaCountAfter = events.filter((e) => e.type === 'agent_message_delta').length;
    expect(deltaCountAfter).toBe(deltaCountBeforeCancel);
    const last = events[events.length - 1] as Extract<EventMsg, { type: 'turn_completed' }>;
    expect(last.type).toBe('turn_completed');
    expect(last.reason).toBe('cancelled');
  });

  it('onEvent 退订后不再收到事件', async () => {
    const transport = createMockTransport();
    const received: EventMsg[] = [];
    const off = transport.onEvent((msg) => received.push(msg));
    off();

    await transport.sendTask('无人监听');
    expect(received).toEqual([]);
  });
});
