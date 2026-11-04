// conversation-nodes 模式（ADR-006）：会话流统一建模为 node 序列。
// M0 仅含消息类节点；tool_call / approval / diff / plan 节点随对应里程碑加入。

export interface UserMessageNode {
  kind: 'user_message';
  id: string;
  text: string;
}

export interface AgentMessageNode {
  kind: 'agent_message';
  id: string;
  text: string;
  /** 流式中为 true；turn 结束后冻结为 false。 */
  streaming: boolean;
}

export interface ErrorNode {
  kind: 'error';
  id: string;
  message: string;
}

export type ConversationNode = UserMessageNode | AgentMessageNode | ErrorNode;
