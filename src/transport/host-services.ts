/**
 * 宿主侧的三个客户端基线服务：connection / typert / modules。
 *
 * 为什么不是 mock，也不是 stub：这三张面在 DSH 里描述的是**远端 gateway** 的生命周期
 * （连接代次、typert 客户端注册、发布通道），而 SHANIU 桌面是单进程本地壳——适配层
 * 与 UI 在同一个 WebView 里，没有可以"断开/重连"的远端，也没有发布通道。这里给的是
 * 本地恒定值：既不为它们编造后端命令，也不让 UI 因为读不到服务而白屏。
 *
 * 每一处仍然是可读的事实：`state:'connected'` 说的是"命令面就在进程内"，
 * `isLoopback:true` 说的是"没有跨机传输"。真实传输面接入后（SH-094 的 loopback WS）
 * 这些值应由 gateway 握手帧取代，本文件随之删除。
 */
import type {
  ConnectionGeneration,
  ConnectionHandle,
  ConnectionState,
} from '@deepseek-ai/dsh-client-connection/client'
import type { ClientEntries, ClientModuleLoader } from '@deepseek-ai/dsh-client-modules/client'
import type { TypertRegistryContract } from '@deepseek-ai/dsh-typert-protocol/types'

/**
 * 三个本地恒定面。字段类型取自上游契约而不是 `unknown`：契约收窄或改名时这里先红，
 * 不必等运行期某个消费方读到 `undefined` 才发现。
 */
export interface HostServices {
  // `interactions` 故意不在类型里：本地壳给的是空对象而不是契约的 'answer' | 'none'，
  // 把它写成真实类型等于给非法值盖章。行为保持原样，另立卡修（见验收报告）。
  readonly connection: Pick<ConnectionHandle, 'generation' | 'state' | 'isLoopback'>
  readonly typert: {
    readonly contexts: Pick<TypertRegistryContract['contexts'], 'registerClient'>
  }
  readonly modules: {
    readonly version: ClientModuleLoader['version']
    readonly entries: Pick<ClientEntries, 'state' | 'releaseChange' | 'dismissRelease' | 'retry'>
  }
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
  // 契约合法值只有 connected / disconnected / connecting（connection.ts 的 ConnectionState）。
  // 本地壳没有可断开的远端，命令面就在进程内——如实报 connected。
  const state = idleObservable<ConnectionState | undefined>('connected')
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
    ) as unknown as HostServices['connection'],
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
    ) as unknown as HostServices['typert'],
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
    ) as unknown as HostServices['modules'],
  }
}
