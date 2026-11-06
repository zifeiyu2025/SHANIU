/**
 * 宿主侧的三个客户端基线服务：connection / typert / modules。
 *
 * 为什么不是 mock，也不是 stub：这三张面在 DSH 里描述的是**远端 gateway** 的生命周期
 * （连接代次、typert 客户端注册、发布通道），而 SHANIU 桌面是单进程本地壳——适配层
 * 与 UI 在同一个 WebView 里，没有可以"断开/重连"的远端，也没有发布通道。这里给的是
 * 本地恒定值：既不为它们编造后端命令，也不让 UI 因为读不到服务而白屏。
 *
 * 每一处仍然是可读的事实：`state:'ready'` 说的是"命令面就在进程内"，
 * `isLoopback:true` 说的是"没有跨机传输"。真实传输面接入后（SH-094 的 loopback WS）
 * 这些值应由 gateway 握手帧取代，本文件随之删除。
 */
import type { ConnectionGeneration } from '@deepseek-ai/dsh-client-connection/client'

export interface HostServices {
  readonly connection: unknown
  readonly typert: unknown
  readonly modules: unknown
}

const idleObservable = <T>(value: T) => ({
  getSnapshot: () => value,
  subscribe: () => () => {},
})

/**
 * @param homeDir - 宿主 `get_product_dirs` 的家目录；界面用它把显示路径缩写。
 *                  必须与运行时默认工作区的回落同源，否则会缩成另一个前缀。
 */
export function hostServices(homeDir: string): HostServices {
  // 形状必须是 ConnectionGeneration（{id, host}）：权限目录按 generation.id 做栅栏，
  // 给错形状会让它永远读不出目录，输入卡的挡位控件就静默不渲染。
  const generation: ConnectionGeneration = { id: 1, host: { home: homeDir } }
  const state = idleObservable('ready')
  const noRelease = idleObservable<undefined>(undefined)
  return {
    connection: new Proxy(
      {
        generation: idleObservable(generation),
        state,
        interactions: {},
        isLoopback: true,
      } as Record<string | symbol, unknown>,
      {
        get(target, prop) {
          if (prop in target) return target[prop]
          if (typeof prop === 'symbol') return undefined
          throw new Error(`host(connection).${String(prop)}: 本地壳没有这个连接生命周期成员`)
        },
      },
    ),
    // typert 的 registerClient 只在远端上下文注册时用；本地壳直接吃适配层。
    typert: new Proxy(
      { contexts: { registerClient: () => () => {} } } as Record<string | symbol, unknown>,
      {
        get(target, prop) {
          if (prop in target) return target[prop]
          if (typeof prop === 'symbol') return undefined
          throw new Error(`host(typert).${String(prop)}: 本地壳没有远端上下文注册面`)
        },
      },
    ),
    // 发布通道：SHANIU 不走 DSH 的模块发布，界面读到的是"空闲、无失败"。
    modules: new Proxy(
      {
        version: 'client',
        entries: {
          state: idleObservable({ syncing: false, failures: [] }),
          releaseChange: noRelease,
          dismissRelease: () => {},
          retry: async () => {},
        },
      } as Record<string | symbol, unknown>,
      {
        get(target, prop) {
          if (prop in target) return target[prop]
          if (typeof prop === 'symbol') return undefined
          throw new Error(`host(modules).${String(prop)}: 本地壳没有模块发布面`)
        },
      },
    ),
  }
}
