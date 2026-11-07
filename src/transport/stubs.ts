/**
 * 后端缺失能力的显式 stub（ADR-006 接线模型的核销清单）。
 * 规则：每个 stub 必须有 SH 任务编号归属；接线里程碑 = 从本文件删除对应条目。
 * stub 只承诺"UI 可用但不触碰真实能力"，绝不伪造成功——未接线的方法一律显式抛错。
 */
import { Service, type Context } from '@deepseek-ai/cordis'
import type {
  ISessions, SessionListState, SessionRetainInfo,
} from '@deepseek-ai/dsh-api-session-controller/client'
import { createSnapshotStore } from '@deepseek-ai/dsh-client-store'
import type { HostObservable } from '@deepseek-ai/dsh-client-ui-slots'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import type { TypertResourceKind } from '@deepseek-ai/dsh-typert-protocol'

/** 一个未接线的 remote 命名空间面：任何方法调用都显式抛错，属性探测返回抛错函数。 */
export function stubRemoteNamespace(name: string): unknown {
  const thrower = () => {
    throw new Error(`stub(remote.${name}): 该命令面未接线，见 transport/stubs.ts 清单`)
  }
  return new Proxy({}, {
    get(_t, prop) {
      if (typeof prop === 'symbol') return undefined
      return thrower
    },
  })
}

/**
 * batch-1/2 组装面涉及的 remote 命名空间（dotted inject + 属性访问两用）。
 *
 * 核销进度（适配层接管情况见 `transport/adapter.ts` 与 `transport/settings-face.ts`，
 * 逐项对应 docs/05 卡号）：
 * - `session` —— 已由适配层接管（SH-017 第一步：消息流、工具卡、终端输出、中断、
 *   审批请求转译）；`session.search` 由 SH-113 接宿主 SQLite FTS5 派生索引（正文/工具
 *   输出片段 + hasMore 收窄提示；wire 为 query-only，没有续页），SH-040 未接线清单中的
 *   `session.search` 项已归属此卡核销。
 *   同会话内的分页游标对齐与历史回放仍缺，分属 SH-094/SH-033。
 * - `toolResults` —— 已由适配层接管（`watch` 从本进程的 journal 取那条
 *   `tool/result` 的 message；本进程没观察过的会话如实报 `available:false`，
 *   不从档案猜一份结果）。后端没有工具输出的增量事件，所以值只在落定时交一帧。
 * - `llm` / `settings` / `credentials` —— 已由适配层接管（读 `get_settings`，写
 *   `save_providers` / `store_api_key` / `delete_api_key`）。设置页写得出而后端吃不下
 *   的部分逐项拒绝并给原因，转 SH-096（推理档）/ SH-097（自定义请求头）/
 *   SH-098（端点模型发现）。
 * - `permissionPresets` —— 已由适配层接管（2026-11-07 拍板）：目录 = 静态三挡映射到
 *   宿主 `sandbox.mode`（approval 维不动，裁决矩阵当前不消费它），切换经
 *   `/permission <preset>` 走 `save_settings`。映射语义见 `transport/adapter.ts` 的
 *   PRESET_TO_SANDBOX 注记；上游实验档 `auto` 不进目录。
 * - `commands` —— 部分接管：`/permission`（挡位芯片的提交路径）、`/plan off`
 *   （PlanChip 退出 → 指引审批卡：宿主 PlanState 只认 exit_plan_mode 工具执行，前端
 *   无真实退出手段）与空 `list`（"+"菜单据此恢复到客户端贡献行）；其余命令行宿主
 *   没有命令日志面，如实未命中。
 * - 其余命名空间 —— 后端没有对应命令面，保持显式抛错。
 */
export const REMOTE_NAMESPACES = [
  'session', 'commands', 'toolResults', 'userQuestions', 'workspace', 'workspaceFiles',
  'directoryPicker', 'job', 'terminal', 'settings', 'subagents', 'modelSelection',
  'credentials', 'llm', 'permissionPresets',
  'pluginManager', 'pluginInventory', 'pluginRegistryProbe', 'schedule',
] as const

/**
 * `remote` 服务 stub：$on/$emit/$call 事件与调用面 + 命名空间子对象。
 * SH-017（工具/终端/diff 接线）与 SH-008（mock 事件源）核销。
 */
export function stubRemote(): unknown {
  const base = {
    $on: () => () => {},
    $emit: () => false,
    $call: () => Promise.reject(new Error('stub(remote): SH-017 接线前不可用')),
    // 流：永不产帧（无后端即无数据，UI 停在连接/加载态，不伪造成功）。
    // 注意：new Promise<T>() 泛型形式会被当前 vite 转换管线破坏（v8.3.5 oxc 打印 bug），
    // 必须用类型注解形式声明。
    $stream: () => {
      const pending: Promise<IteratorResult<never>> = new Promise(() => {})
      return {
        [Symbol.asyncIterator]: () => ({
          next: () => pending,
          return: async () => ({ done: true as const, value: undefined as never }),
        }),
      }
    },
    $mount: async () => {
      throw new Error('stub(remote).$mount: 由适配层取代，见 transport/shims')
    },
  } as Record<string | symbol, unknown>
  return new Proxy(base, {
    get(target, prop) {
      if (prop in target) return target[prop]
      if (typeof prop === 'symbol') return undefined
      if ((REMOTE_NAMESPACES as readonly string[]).includes(prop)) {
        return stubRemoteNamespace(prop)
      }
      return stubRemoteNamespace(String(prop))
    },
  })
}

/**
 * 批量注册 `remote.<ns>` 点分服务（cordis inject 的 'remote.session' 等按服务名解析）。
 *
 * `covered` 列出**已由别处提供**的命名空间（适配层接管的那几个），这里就跳过它们——
 * 后注册的 stub 会把真实现覆盖掉，症状是"消息流能跑但设置页永远抛错"。
 */
export function installRemoteNamespaceStubs(ctx: Context, covered: readonly string[] = []): void {
  for (const ns of REMOTE_NAMESPACES) {
    if (covered.includes(ns)) continue
    ctx.reflect.provide(`remote.${ns}`, stubRemoteNamespace(ns))
  }
}

const REFUSED: HostObservable<boolean> = {
  getSnapshot: () => false,
  subscribe: () => () => {},
}

const ALLOWED: HostObservable<boolean> = {
  getSnapshot: () => true,
  subscribe: () => () => {},
}

/**
 * `remoteAccess` 服务 stub：按构造参数统一回答全部动作。
 * Tauri 态 = 全拒（SH-025 审批接线时核销）；浏览器 mock 态 = 全许（mock 后端授权语义）。
 * 同时发布 slots 全局 `remoteAccess` keyed hook（SidebarRoot 等 UI 的 useRemoteAccess 座席），
 * 与上游 mountAccess 的 provideRoot 范式一致。
 */
export class StubRemoteAccess extends Service {
  private readonly granted: boolean
  private readonly answer: HostObservable<boolean>

  constructor(ctx: Context, allow: boolean) {
    super(ctx, 'remoteAccess')
    this.granted = allow
    this.answer = allow ? ALLOWED : REFUSED
    ctx.slots.provideRoot({
      keyedHooks: { remoteAccess: () => this.answer },
    })
  }

  source(_kind: TypertResourceKind, _id: string, _action: string): HostObservable<boolean> {
    return this.answer
  }

  whenAccess(_action: string, callback: () => unknown): () => void {
    if (!this.granted) return () => {}
    // 授权态（mock）：答案权恒真 → 回调立即挂载其 effect；返回统一卸载器
    return this.runEffect(callback)
  }

  whenResourceAccess(
    _kind: TypertResourceKind, _id: string, _action: string, callback: () => unknown,
  ): () => void {
    return this.whenAccess('', callback)
  }

  private runEffect(callback: () => unknown): () => void {
    const effect = callback() as unknown
    let done = false
    const once = (disposers: readonly (() => void)[]): (() => void) => () => {
      if (done) return
      done = true
      for (const dispose of [...disposers].reverse()) dispose()
    }
    if (effect === undefined) return once([])
    // 与 ctx.effect 同语义：生成器一次性跑完，把 yield 出的 disposer 留着，
    // 逆序统一卸载。此前在这里逐个立即调用，等于「注册完马上拆」——
    // 受 whenAccess 保护的侧栏面板因此从未出现。
    if (typeof effect === 'function') return once([effect as () => void])
    if (typeof effect === 'object' && effect !== null && Symbol.iterator in effect) {
      const disposers: (() => void)[] = []
      for (const yielded of effect as Iterable<unknown>) {
        if (typeof yielded === 'function') disposers.push(yielded as () => void)
      }
      return once(disposers)
    }
    return once([])
  }
}

/**
 * `sessions` 服务最小面：空会话列表 + 未接线的创建/绑定（SH-033 会话管理接线时核销）。
 * 面形状与上游 ui-session 测试的 createSessionsBench 一致：list 为空快照库，
 * retainInfo 按会话给出零引用源，binding 恒为 undefined；其余成员经 Proxy 显式抛错。
 */
export function stubSessions(): ISessions {
  const list = createSnapshotStore<SessionListState>({
    ids: [],
    byId: {},
    phase: 'ready',
    projectionsBySession: {},
  })
  const retention = new Map<SessionId, ReturnType<typeof createSnapshotStore<SessionRetainInfo>>>()
  const base = {
    list,
    create: async () => {
      throw new Error('stub(sessions).create: SH-033 接线前不可创建会话')
    },
    binding: (_id: SessionId) => undefined,
    retainInfo: (id: SessionId) => {
      let source = retention.get(id)
      if (source === undefined) {
        source = createSnapshotStore<SessionRetainInfo>({ referenceCount: 0, retainedBy: {} })
        retention.set(id, source)
      }
      return source
    },
  }
  // Proxy 在运行时兜底"未接线成员显式抛错"；symbol 属性放行 undefined（cordis
  // 内部会探测 tracker 等符号位）；静态面经 unknown 断言（ISessions 完整面属
  // SH-033 接线范畴，stub 只承诺已列成员可用）
  return new Proxy(base, {
    get(target, prop) {
      if (prop in target) return target[prop as keyof typeof target]
      if (typeof prop === 'symbol') return undefined
      throw new Error(`stub(sessions).${String(prop)}: SH-033 接线前不可用`)
    },
  }) as unknown as ISessions
}
