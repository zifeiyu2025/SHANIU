import type {
  AgentMessageNode,
  ApprovalNode,
  ConversationNode,
  ErrorNode,
  RiskLevel,
  ToolCallNode,
  UserMessageNode,
} from './types';

let seq = 0;

function nextId(prefix: string): string {
  seq += 1;
  return `${prefix}-${seq}`;
}

export function newUserMessageNode(text: string): UserMessageNode {
  return { kind: 'user_message', id: nextId('user'), text };
}

export function newAgentMessageNode(text: string, streaming: boolean): AgentMessageNode {
  return { kind: 'agent_message', id: nextId('agent'), text, streaming };
}

export function newErrorNode(message: string): ErrorNode {
  return { kind: 'error', id: nextId('error'), message };
}

export function newApprovalNode(
  requestId: string,
  tool: string,
  args: unknown,
  risk: RiskLevel,
): ApprovalNode {
  return { kind: 'approval', id: nextId('approval'), requestId, tool, args, risk, status: 'pending' };
}

export function newToolCallNode(callId: string, tool: string, args: unknown): ToolCallNode {
  return {
    kind: 'tool_call',
    id: nextId('tool'),
    callId,
    tool,
    args,
    status: 'running',
    stdout: '',
    stderr: '',
  };
}

export function isConversationNode(value: unknown): value is ConversationNode {
  if (typeof value !== 'object' || value === null) return false;
  const kind = (value as { kind?: unknown }).kind;
  return (
    kind === 'user_message' ||
    kind === 'agent_message' ||
    kind === 'error' ||
    kind === 'approval' ||
    kind === 'tool_call'
  );
}
