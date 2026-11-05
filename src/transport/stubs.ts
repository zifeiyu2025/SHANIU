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

/** batch-1/2 组装面涉及的 remote 命名空间（dotted inject + 属性访问两用）。 */
export const REMOTE_NAMESPACES = [
  'session', 'commands', 'toolResults', 'userQuestions', 'workspace', 'workspaceFiles',
  'directoryPicker', 'job', 'terminal', 'settings', 'subagents', 'modelSelection',
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

/** 批量注册 `remote.<ns>` 点分服务（cordis inject 的 'remote.session' 等按服务名解析）。 */
export function installRemoteNamespaceStubs(ctx: Context): void {
  for (const ns of REMOTE_NAMESPACES) {
    ctx.reflect.provide(`remote.${ns}`, stubRemoteNamespace(ns))
  }
}

/** `configForms` 服务最小面：所有命名空间 ready + 空值，写操作拒绝。 */
export function stubConfigForms() {
  const form = {
    getSnapshot: () => ({
      status: 'ready' as const,
      value: undefined,
      base: undefined,
      user: undefined,
      revision: undefined,
      writable: false,
    }),
    subscribe: () => () => {},
    mutate: () => Promise.resolve(false),
    set: () => Promise.resolve(false),
    unset: () => Promise.resolve(false),
  }
  return {
    get: () => form,
    developerTools: { enabled: { getSnapshot: () => false, subscribe: () => () => {} } },
  }
}

const REFUSED: HostObservable<boolean> = {
  getSnapshot: () => false,
  subscribe: () => () => {},
}

/**
 * `remoteAccess` 服务 stub：一切动作一律拒绝（SH-025 审批接线时核销）。
 * Service 子类：构造即自注册到 ctx（与上游 test-runtime 的 StaticRemoteAccess 同范式）。
 */
export class StubRemoteAccess extends Service {
  constructor(ctx: Context) {
    super(ctx, 'remoteAccess')
  }

  source(_kind: TypertResourceKind, _id: string, _action: string): HostObservable<boolean> {
    return REFUSED
  }

  whenAccess(_action: string, _callback: () => unknown): () => void {
    return () => {}
  }

  whenResourceAccess(
    _kind: TypertResourceKind, _id: string, _action: string, _callback: () => unknown,
  ): () => void {
    return () => {}
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
