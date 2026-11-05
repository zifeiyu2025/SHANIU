// 与 src-tauri/src/events.rs 的 EventMsg 保持镜像（ADR-005 单一事件协议）。
// 兼容性规则（docs §5 FF-2）：只允许新增事件类型或新增 optional 字段，新增字段必须可被旧消费端忽略。
// 修改本文件必须同步修改 Rust 侧并更新 src-tauri/tests/event_schema.rs 快照。

export type TurnEndReason = 'completed' | 'cancelled' | 'failed' | 'budget_exhausted';
export type IoChannel = 'stdout' | 'stderr';
export type RiskLevel = 'read' | 'write_in_workspace' | 'execute' | 'destructive' | 'plan_review';
export type ApprovalDecision = 'allow' | 'deny' | 'allow_always';
/** 工具调用终态（docs/04 §1.2.1）：存在时为唯一状态判据，缺失时前端走两步反推回退。 */
export type ToolCallResult = 'ok' | 'error' | 'cancelled' | 'denied';

export type EventMsg =
  | { type: 'turn_started'; turn: number }
  | { type: 'turn_completed'; turn: number; reason: TurnEndReason }
  | { type: 'agent_message_delta'; delta: string }
  /** 思考模型的推理增量（additive，FF-2）：与正文分开，旧消费端按未知 type 忽略。 */
  | { type: 'reasoning_delta'; delta: string }
  | { type: 'tool_call_begin'; call_id: string; tool: string; args: unknown }
  | { type: 'tool_call_output_delta'; call_id: string; channel: IoChannel; delta: string }
  | {
      type: 'tool_call_end';
      call_id: string;
      exit_code: number | null;
      error: string | null;
      /** additive（FF-2）：后端 `None` 时不产生该键，故"缺失"与"显式 null"都表示未提供。 */
      result?: ToolCallResult | null;
    }
  | {
      type: 'exec_approval_request';
      request_id: string;
      tool: string;
      args: unknown;
      risk: RiskLevel;
    }
  | { type: 'exec_approval_resolved'; request_id: string; decision: ApprovalDecision }
  | { type: 'plan_delta'; delta: string }
  | { type: 'turn_diff'; files: string[] }
  | { type: 'error'; message: string }
  | {
      type: 'token_usage';
      turn: number;
      input_tokens: number;
      cached_input_tokens?: number | null;
      output_tokens: number;
    };

/** 设置数据（settings dialog 的读写载荷）。 */
export interface Settings {
  baseUrl: string;
  model: string;
  workspaceRoot: string;
  hasApiKey: boolean;
}

export interface SettingsInput {
  baseUrl: string;
  model: string;
  workspaceRoot: string;
  /** 留空 = 不改动已保存的 Key。 */
  apiKey?: string;
}

/** 前端与后端之间的传输层抽象（ADR-006：Web 开发模式用 mock，Tauri 模式用 IPC）。 */
export interface Transport {
  /** IPC 握手：后端经事件通道回显 nonce，证明 invoke + 事件通道双向连通（M0 验收）。 */
  ping(nonce: string): Promise<string>;
  /** 提交任务，触发一轮 agent turn（M1 接入 agent_loop）。 */
  sendTask(task: string): Promise<void>;
  /** 请求中断当前 turn（CancellationToken 语义，M1 接入）。 */
  cancel(): Promise<void>;
  /** 读取设置。 */
  getSettings(): Promise<Settings>;
  /** 保存设置（apiKey 留空不覆盖已存 Key）。 */
  saveSettings(input: SettingsInput): Promise<void>;
  /** 审批裁决回执（M2 接入 permission broker）。 */
  approve(requestId: string, decision: ApprovalDecision): Promise<void>;
  /** 订阅事件流，返回退订函数。 */
  onEvent(handler: (msg: EventMsg) => void): () => void;
}
