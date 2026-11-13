/**
 * 审批观察者租约（阶段 3）：页面在，才允许后端挂起等待审批。
 *
 * 为什么要租约而不是一次性闩锁：闩锁只说得出「前端曾经就绪」，说不出「此刻还有
 * 人能点卡片」。页面崩溃、被挂起、刷新到一半时闩锁仍然是开的，后端就会一直等一个
 * 永远不会来的回执。租约把「有人在看」变成可过期的证据：
 * - 页面启动 `approval_attach`，之后每 5s `approval_heartbeat` 续约；
 * - 宿主只看 TTL（15s，`APPROVAL_LEASE_TTL_MS`）：超时即视为无人可答，
 *   由看门狗统一按 Deny 收口；
 * - 页面卸载/隐藏时 `approval_detach`：**只停止续期**，不立即判死。刷新会触发
 *   detach，新页面还没 attach 时若立刻撤销可达性，上一页挂着的审批会被误杀。
 *
 * 心跳返回 false 说明租约已不存在（宿主重启 / 看门狗清理 / 页面换了 viewer id），
 * 此时立刻重新 attach——沉默地继续心跳只会让租约永远续不上，所有审批被拒。
 */
import type { HostBridge } from './tauri'

/** 前端续约间隔。与后端 `APPROVAL_LEASE_HEARTBEAT_MS` 同名同值；快照里也有权威值。 */
export const APPROVAL_LEASE_HEARTBEAT_MS = 5_000

export interface ApprovalLease {
  /** 停止续约并退租；重复调用无副作用。 */
  dispose(): void
}

export interface ApprovalLeaseOptions {
  /** 观察者 id（测试用固定值）；缺省每次启动生成一个，刷新即换新。 */
  viewerId?: string
  /** 续约间隔（测试可缩短）；缺省 5s。 */
  heartbeatMs?: number
}

/** 生成观察者 id：每次启动唯一。刷新产生新 id，旧租约按 TTL 自然过期。 */
function newViewerId(): string {
  const random = globalThis.crypto?.randomUUID?.()
  if (random !== undefined) return `viewer-${random}`
  return `viewer-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`
}

/**
 * 启动租约：attach → 周期心跳 → 卸载时 detach。
 *
 * 失败不抛给调用方：审批链路的兜底在后端（没有新鲜租约就不挂起请求），
 * 前端这里抛错只会让启动流程失败，反而连「重新 attach」的机会都没有。
 *
 * @param bridge - 宿主编解码出口（Tauri 或包装后的 WS 桥，命令面一致）。
 * @param options - 测试注入点（viewerId / 心跳间隔）。
 * @returns 停止租约的句柄。
 */
export function startApprovalLease(
  bridge: HostBridge,
  options: ApprovalLeaseOptions = {},
): ApprovalLease {
  const viewerId = options.viewerId ?? newViewerId()
  const heartbeatMs = options.heartbeatMs ?? APPROVAL_LEASE_HEARTBEAT_MS
  let stopped = false

  const attach = (): void => {
    if (stopped) return
    void bridge.invoke('approval_attach', { viewerId }).catch((reason: unknown) => {
      console.warn('[SHANIU] 审批租约登记失败，稍后由心跳重试', reason)
    })
  }

  const beat = (): void => {
    if (stopped) return
    void bridge.invoke<boolean>('approval_heartbeat', { viewerId })
      .then((alive) => {
        // 租约没了（宿主重启 / 看门狗清理）：重新登记，不能装作还在续约。
        if (!alive) attach()
      })
      .catch((reason: unknown) => {
        console.warn('[SHANIU] 审批租约心跳失败，下个周期重试', reason)
      })
  }

  const stop = (): void => {
    if (stopped) return
    stopped = true
    clearInterval(timer)
    // 页面卸载时 WebView 可能已经断开通路：退租失败不影响正确性，TTL 仍会收口。
    void bridge.invoke('approval_detach', { viewerId }).catch(() => {})
  }

  attach()
  const timer = setInterval(beat, heartbeatMs)
  // 用 pagehide 而不是 beforeunload：WebView2 刷新/关窗都会走到它，且不会阻塞卸载。
  globalThis.addEventListener?.('pagehide', stop)

  return {
    dispose() {
      globalThis.removeEventListener?.('pagehide', stop)
      stop()
    },
  }
}
