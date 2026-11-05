// conversation-nodes 模式（ADR-006）：会话流统一建模为 node 序列。
// M2 增加 approval / tool_call 节点；diff / plan 节点随对应里程碑加入。

import type { RiskLevel, ToolCallResult } from '../transport/types';

// 协议枚举单源在 transport/types.ts（docs/04 的镜像）；这里再导出以保住既有消费方
// （ApprovalCard / builders 都从 nodes/types 取 RiskLevel），且只走 type 依赖、不引运行时值。
export type { RiskLevel };

export type ApprovalNodeStatus = 'pending' | 'allowed' | 'denied';
/** 终态直接复用协议镜像的 `ToolCallResult`（单源，避免两处联合手工同步漂移）。 */
export type ToolCallStatus = 'running' | ToolCallResult;

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

/** 工具调用节点：按 callId 与后端 tool_call_begin/output_delta/end 三段事件关联。 */
export interface ToolCallNode {
  kind: 'tool_call';
  id: string;
  callId: string;
  tool: string;
  args: unknown;
  status: ToolCallStatus;
  /** 后端权威结果（协议 additive 字段）：存在时状态只按它判定，缺失时走两步反推回退。 */
  result?: ToolCallResult;
  exitCode?: number | null;
  error?: string | null;
  stdout: string;
  stderr: string;
}

export type ConversationNode =
  | UserMessageNode
  | AgentMessageNode
  | ErrorNode
  | ApprovalNode
  | ToolCallNode;
