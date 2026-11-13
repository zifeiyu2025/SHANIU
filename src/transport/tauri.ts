/**
 * 宿主桥（SH-017）：适配层唯一的 Tauri 出入口。
 *
 * 之所以抽这一层：适配层的契约测试必须在没有 Tauri 运行时的环境里跑（vitest 没有
 * `__TAURI_INTERNALS__`），而真实接线又只能通过 Tauri 走。测试注入脚本化的桥，
 * 桌面运行时用 `tauriBridge()`——两边共用同一份映射代码，不维护第二套逻辑。
 */
import type { AgentEvent } from './types'
import type { PendingApprovalsSnapshotWire } from './pending-approval'

export interface HostBridge {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>
  /** 订阅 `shaniu://event`；返回卸载器。 */
  listenEvents(handler: (event: AgentEvent) => void): () => void
  /**
   * 可选：订阅待审批表变化（`approval/pending-changed`）。载荷与 `pending_approvals`
   * 查询同形；适配层据此重建审批卡，全局横幅据此显示待审批总数。
   * 测试桥与旧 WS 桥可以不实现，调用方按「没有就不订阅」处理。
   */
  listenApprovalPending?(handler: (snapshot: PendingApprovalsSnapshotWire) => void): () => void
  /**
   * 可选：告诉事件通道"这个会话正在被观察"（适配层 follow 入口调用）。
   * WS gateway 据此补订阅；Tauri 全局广播与测试桥无需实现。
   * 与 `releaseEvents` 是引用计数对：follow 进入 +1、退出 -1，归零才真正退订。
   *
   * `cursor`：`undefined` = 沿用旧规则（新会话全量 / 既有会话 live-only / 休眠续播）；
   * `''` = 显式全量补播；`id` = 从该 event_id 之后补播；`null` = 只接 live。
   */
  observeEvents?(sessionId: string, cursor?: string | null): void
  /** 可选：观察者退场（与 `observeEvents` 配对）；WS gateway 引用归零后转休眠并退订。 */
  releaseEvents?(sessionId: string): void
}

let bridge: HostBridge | undefined

/** 桌面运行时桥：懒加载 Tauri API，纯浏览器下调用即抛（不静默退回 mock）。 */
export function tauriBridge(): HostBridge {
  if (bridge !== undefined) return bridge
  bridge = {
    async invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
      const { invoke } = await import('@tauri-apps/api/core')
      return await invoke<T>(command, args)
    },
    listenEvents(handler: (event: AgentEvent) => void): () => void {
      let unlisten: (() => void) | undefined
      // 订阅是异步的（动态 import）。若在登记完成前就被卸载，必须一登记完就立刻拆掉，
      // 否则这个监听器会一直留在 Tauri 侧，回调指向一个已经卸载的适配层。
      let closed = false
      void import('@tauri-apps/api/event').then(({ listen }) =>
        listen<AgentEvent>('shaniu://event', ({ payload }) => { handler(payload) }),
      ).then((off) => {
        unlisten = off
        if (closed) off()
      }).catch((reason: unknown) => {
        console.error('[SHANIU] 事件通道订阅失败，会话流不会更新', reason)
      })
      return () => {
        closed = true
        unlisten?.()
      }
    },
    listenApprovalPending(handler: (snapshot: PendingApprovalsSnapshotWire) => void): () => void {
      let unlisten: (() => void) | undefined
      let closed = false
      // 与 listenEvents 同一套"登记前就卸载"的保护：审批快照通道漏拆会让回调
      // 指向已卸载的适配层，表现为关掉页面后仍在重建卡片。
      void import('@tauri-apps/api/event').then(({ listen }) =>
        listen<PendingApprovalsSnapshotWire>('approval/pending-changed', ({ payload }) => {
          handler(payload)
        }),
      ).then((off) => {
        unlisten = off
        if (closed) off()
      }).catch((reason: unknown) => {
        console.error('[SHANIU] 待审批通道订阅失败，刷新后可能看不到待审批', reason)
      })
      return () => {
        closed = true
        unlisten?.()
      }
    },
  }
  return bridge
}
