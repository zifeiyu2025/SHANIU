import {
  newAgentMessageNode,
  newReasoningNode,
  newToolCallNode,
  newUserMessageNode,
} from './builders';
import type { ConversationNode } from './types';
import type { SessionEntry } from '../transport/types';

/** 档案里的 arguments 是 JSON 字符串；解析失败就原样带上，重放不该因为一处坏参数就丢整张卡。 */
function parseArguments(raw: string): unknown {
  try {
    return JSON.parse(raw) as unknown;
  } catch {
    return raw;
  }
}

/**
 * 会话档案 → 可渲染节点序列（SH-033「打开会话」）。
 *
 * 一处必须说清的局限：**档案不记录工具终态**。`HistoryItem::FunctionCallOutput` 只有
 * 一段输出文本，没有 exit code 也没有 ok/error/cancelled 判别（那是 `tool_call_end`
 * 事件才有的信息，而事件不落档）。所以重放出来的状态是从「有没有配对输出」推的**下界**：
 * 有输出记 ok（它确实跑完并回填了，成败细节在输出文本里），无输出记 cancelled
 * （对应 SH-082 说的"有 call 无 output"尾巴——它确实没有应答）。
 * 这个下界不能与实时事件流的四态等价看待，因此这里刻意不去猜 error。
 */
export function nodesFromHistory(entries: readonly SessionEntry[]): ConversationNode[] {
  const nodes: ConversationNode[] = [];
  for (const entry of entries) {
    if (entry.kind === 'message') {
      // system 消息是每轮重算的提示词（04 §6.1 不变量③），不是用户看得见的一轮内容，
      // 重放时不显示——显示它等于把内部提示词摊给用户。
      if (entry.role === 'system') continue;
      // 思考增量单独成节点（SH-083 同一口径）：它不是答案内容，要能折叠。
      if (entry.reasoning) nodes.push(newReasoningNode(entry.reasoning, false));
      if (entry.role === 'user') nodes.push(newUserMessageNode(entry.text));
      else nodes.push(newAgentMessageNode(entry.text, false));
      continue;
    }
    const node = newToolCallNode(entry.call_id, entry.tool, parseArguments(entry.arguments));
    if (entry.output === undefined) {
      node.status = 'cancelled';
    } else {
      node.status = 'ok';
      node.stdout = entry.output;
    }
    nodes.push(node);
  }
  return nodes;
}
