import { describe, expect, it } from 'vitest';
import { nodesFromHistory } from '../nodes/from-history';
import type { SessionEntry } from '../transport/types';

describe('nodesFromHistory 档案投影', () => {
  it('用户与助手消息各成节点，顺序保持', () => {
    const entries: SessionEntry[] = [
      { kind: 'message', role: 'user', text: '第一条' },
      { kind: 'message', role: 'assistant', text: '回复' },
    ];
    const nodes = nodesFromHistory(entries);
    expect(nodes.map((n) => n.kind)).toEqual(['user_message', 'agent_message']);
  });

  it('重放出来的正文不是流式态：streaming 必须为 false', () => {
    // 打开历史会话时若还挂着 streaming，光标会一直在、工具卡也停在"运行中"
    const nodes = nodesFromHistory([{ kind: 'message', role: 'assistant', text: '旧回复' }]);
    expect(nodes[0]).toMatchObject({ streaming: false });
  });

  it('reasoning 单独成节点，绝不并进正文', () => {
    const nodes = nodesFromHistory([
      { kind: 'message', role: 'assistant', text: '答案', reasoning: '思路' },
    ]);
    expect(nodes.map((n) => n.kind)).toEqual(['reasoning', 'agent_message']);
  });

  it('system 提示词不显示给用户', () => {
    // 04 §6.1 不变量③：系统提示每轮重算，它不是这一轮的内容
    expect(nodesFromHistory([{ kind: 'message', role: 'system', text: '你是助手' }])).toHaveLength(0);
  });

  it('工具调用带上配对的输出并记为完成', () => {
    const nodes = nodesFromHistory([
      { kind: 'tool_call', call_id: 'c1', tool: 'unified_exec', arguments: '{"command":"ls"}', output: 'a.txt\n' },
    ]);
    expect(nodes[0]).toMatchObject({
      kind: 'tool_call',
      callId: 'c1',
      tool: 'unified_exec',
      args: { command: 'ls' },
      status: 'ok',
      stdout: 'a.txt\n',
    });
  });

  it('arguments 不是合法 JSON 时原样保留，不丢整张卡', () => {
    const nodes = nodesFromHistory([
      { kind: 'tool_call', call_id: 'c1', tool: 'x', arguments: '{坏参数' },
    ]);
    expect(nodes[0]).toMatchObject({ kind: 'tool_call', args: '{坏参数' });
  });

  it('有 call 无 output 记为停止而不是成功（档案不记录终态，只给得出下界）', () => {
    const nodes = nodesFromHistory([
      { kind: 'tool_call', call_id: 'c1', tool: 'x', arguments: '{}' },
    ]);
    expect(nodes[0]).toMatchObject({ status: 'cancelled' });
  });
});
