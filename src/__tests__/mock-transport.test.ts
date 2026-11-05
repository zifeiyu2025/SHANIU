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
    // dev 模式中途插入的工具调用：begin → 双通道 delta → end 全链路可观察（SH-017）
    expect(types).toContain('tool_call_begin');
    const channels = events
      .filter(
        (e): e is Extract<EventMsg, { type: 'tool_call_output_delta' }> =>
          e.type === 'tool_call_output_delta',
      )
      .map((e) => e.channel);
    expect(channels).toContain('stdout');
    expect(channels).toContain('stderr');
    const end = events.find(
      (e): e is Extract<EventMsg, { type: 'tool_call_end' }> => e.type === 'tool_call_end',
    );
    expect(end?.exit_code).toBe(0);
    expect(end?.result).toBe('ok');
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

  it('取消发生在工具调用期间：仍补发 end(exit_code=null) 再以 cancelled 收尾', async () => {
    const transport = createMockTransport();
    const events: EventMsg[] = [];
    const off = transport.onEvent((msg) => {
      events.push(msg);
      // 等 begin 真正到达再取消，确保覆盖到工具执行中的中断路径
      if (msg.type === 'tool_call_begin') void transport.cancel();
    });

    await transport.sendTask('边跑边取消');
    off();

    const end = events.find(
      (e): e is Extract<EventMsg, { type: 'tool_call_end' }> => e.type === 'tool_call_end',
    );
    expect(end).toBeDefined();
    expect(end?.exit_code).toBeNull();
    expect(end?.error).toBe('已取消');
    expect(end?.result).toBe('cancelled');
    const types = events.map((e) => e.type);
    expect(types.indexOf('tool_call_end')).toBeLessThan(types.indexOf('turn_completed'));
    const last = events[events.length - 1] as Extract<EventMsg, { type: 'turn_completed' }>;
    expect(last.reason).toBe('cancelled');
  });

  it('两轮任务各生成独立 callId，跨轮卡片不互相覆盖', async () => {
    const transport = createMockTransport();
    const ids: string[] = [];
    transport.onEvent((msg) => {
      if (msg.type === 'tool_call_begin') ids.push(msg.call_id);
    });

    await transport.sendTask('第一轮');
    await transport.sendTask('第二轮');

    expect(ids).toHaveLength(2);
    expect(new Set(ids).size).toBe(2);
  });

  it('三轮工具调用：前两轮带 result，第三轮刻意不发（旧形态回退样本）', async () => {
    const transport = createMockTransport();
    const ends: Array<Extract<EventMsg, { type: 'tool_call_end' }>> = [];
    transport.onEvent((msg) => {
      if (msg.type === 'tool_call_end') ends.push(msg);
    });

    await transport.sendTask('第一轮');
    await transport.sendTask('第二轮');
    await transport.sendTask('第三轮');

    expect(ends).toHaveLength(3);
    expect(ends[0].result).toBe('ok');
    expect(ends[1].result).toBe('ok');
    // dev 模式要能看到"走回退反推"的旧形态，故第三轮不产生 result 键
    expect('result' in ends[2]).toBe(false);
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
