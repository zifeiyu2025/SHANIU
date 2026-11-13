/**
 * 待审批快照的 wire 形状（阶段 3：审批可达性）。
 *
 * 与 `src-tauri/src/permission/mod.rs` 的 `PendingApprovalView` /
 * `PendingApprovalsSnapshot` 逐字段同形（serde `camelCase`）。查询命令
 * `pending_approvals` 与事件 `approval/pending-changed` 共用这一份载荷——
 * 两种来源必须能互相替代，否则刷新重建与实时路径会给出两套真相。
 *
 * 用途有两个，缺一不可：
 *  1. 刷新/崩溃后没有旧的事件流：按快照把仍挂着的审批卡重建出来；
 *  2. 全局横幅：按 `sessionId + requestId` 定位，不依赖"当前会话"或列表下标
 *     （多会话共用同一个窗体时，当前会话可能是另一个）。
 */
import type { EventMsg, JsonValue, RiskLevel } from './types'

/** 待审批表变化事件（Tauri 事件名）。载荷是 `PendingApprovalsSnapshotWire`。 */
export const APPROVAL_PENDING_CHANGED_EVENT = 'approval/pending-changed'

/** 一条挂起的审批（`PendingApprovalView`）。可选字段在 Rust 侧是 `Option`，序列化为 `null`。 */
export interface PendingApprovalWire {
  requestId: string
  /** 请求所属会话：横幅跳转与会话标题解析都只认它。 */
  sessionId: string
  agentId?: string | null
  tool: string
  risk: RiskLevel
  args: JsonValue
  reason?: string | null
  callId?: string | null
  taskSummary?: string | null
  batchScope?: string | null
  createdAtMs: number
}

/** 待审批表完整快照（`PendingApprovalsSnapshot`）。 */
export interface PendingApprovalsSnapshotWire {
  approvals: PendingApprovalWire[]
  /** 后端单调递增：小于已处理值的快照是倒序到达，直接丢弃。 */
  revision: number
  /** 前端续约间隔（毫秒），后端权威值，前端不自造。 */
  heartbeatMs: number
  /** 租约 TTL（毫秒）：超过即视为无人可答，由宿主统一拒绝。 */
  ttlMs: number
  /** 此刻是否至少有一条新鲜租约；false = 页面心跳已中断。 */
  attended: boolean
}

/**
 * 把快照条目还原成事件流里的审批请求。
 *
 * 刷新后的页面没有 `ExecApprovalRequest` 事件可重放，只能由快照合成一条同形消息，
 * 交给与实时路径完全相同的 `requestApproval`——两条路径共用一处渲染与回执逻辑。
 * `null` 统一折成 `undefined`：事件协议里可选字段缺席，不是显式 null。
 */
export function approvalRequestOf(
  view: PendingApprovalWire,
): Extract<EventMsg, { type: 'exec_approval_request' }> {
  return {
    type: 'exec_approval_request',
    request_id: view.requestId,
    tool: view.tool,
    args: view.args,
    risk: view.risk,
    ...(view.agentId == null ? {} : { agent_id: view.agentId }),
    ...(view.taskSummary == null ? {} : { task_summary: view.taskSummary }),
    ...(view.reason == null ? {} : { reason: view.reason }),
    ...(view.callId == null ? {} : { call_id: view.callId }),
    ...(view.batchScope == null ? {} : { batch_scope: view.batchScope }),
  }
}
