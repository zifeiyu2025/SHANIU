// conversation-nodes 模式（ADR-006）：会话流统一建模为 node 序列。
// M2 增加 approval 节点；diff / plan 节点随对应里程碑加入。

export type RiskLevel = 'read' | 'write_in_workspace' | 'execute' | 'destructive' | 'plan_review';
export type ApprovalNodeStatus = 'pending' | 'allowed' | 'denied';

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

export interface ApprovalNode {
  kind: 'approval';
  id: string;
  requestId: string;
  tool: string;
  args: unknown;
  risk: RiskLevel;
  status: ApprovalNodeStatus;
}

export type ConversationNode = UserMessageNode | AgentMessageNode | ErrorNode | ApprovalNode;
