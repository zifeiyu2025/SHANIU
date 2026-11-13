/**
 * 待审批横幅的数据面：`pending_approvals` 查询 + `approval/pending-changed` 订阅。
 *
 * 与适配层共用同一份 wire 类型（`transport/pending-approval.ts`）：横幅显示的数量
 * 必须与后端待审批表一致，不能各算一套。横幅不消费事件流——它只需要「现在有几条、
 * 分别属于哪个会话」。
 */
import { tauriBridge, type HostBridge } from '../transport/tauri'
import type { PendingApprovalsSnapshotWire } from '../transport/pending-approval'

/** 横幅用到的全部宿主调用；测试注入脚本化实现，运行时走 `hostApprovalBannerWire()`。 */
export interface ApprovalBannerWire {
  /** 当前待审批快照（后端权威值）。 */
  pending(): Promise<PendingApprovalsSnapshotWire>
  /** 订阅待审批表变化；桥不支持时返回空卸载器。 */
  subscribe(handler: (snapshot: PendingApprovalsSnapshotWire) => void): () => void
}

/** 桌面运行时的实现（命令名与 `lib.rs` 的 `generate_handler!` 逐字对应）。 */
export function hostApprovalBannerWire(bridge: HostBridge = tauriBridge()): ApprovalBannerWire {
  return {
    pending: () => bridge.invoke<PendingApprovalsSnapshotWire>('pending_approvals'),
    subscribe: (handler) => bridge.listenApprovalPending?.(handler) ?? ((): void => {}),
  }
}
