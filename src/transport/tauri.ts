/**
 * 宿主桥（SH-017）：适配层唯一的 Tauri 出入口。
 *
 * 之所以抽这一层：适配层的契约测试必须在没有 Tauri 运行时的环境里跑（vitest 没有
 * `__TAURI_INTERNALS__`），而真实接线又只能通过 Tauri 走。测试注入脚本化的桥，
 * 桌面运行时用 `tauriBridge()`——两边共用同一份映射代码，不维护第二套逻辑。
 */
import type { AgentEvent } from './types'

export interface HostBridge {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>
  /** 订阅 `shaniu://event`；返回卸载器。 */
  listenEvents(handler: (event: AgentEvent) => void): () => void
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
      let dispose: (() => void) | undefined
      void import('@tauri-apps/api/event').then(({ listen }) =>
        listen<AgentEvent>('shaniu://event', ({ payload }) => { handler(payload) }),
      ).then((unlisten) => {
        dispose = unlisten
      }).catch((reason: unknown) => {
        console.error('[SHANIU] 事件通道订阅失败，会话流不会更新', reason)
      })
      return () => { dispose?.() }
    },
  }
  return bridge
}
