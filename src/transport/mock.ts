/**
 * Mock 数据源（SH-008 会话全流程联调的 mock 后端，ADR-006 接线模型）。
 * 仅纯浏览器开发态启用（dsh-boot 按 `__TAURI_INTERNALS__` 分流，绝不进 Tauri 运行时）。
 * 与 stubs.ts 的区别：mock 允许"伪造成功"——按真实形状预置数据、驱动真实客户端投影
 * （workspace-controller 的 follow 流/模型机制原样运行），用于把界面各区域填充到可联调状态。
 */
import type { SessionSummary } from '@deepseek-ai/dsh-api-session-controller/types'
import type { WorkspaceView } from '@deepseek-ai/dsh-api-workspace-controller/client'
import type { SessionId } from '@deepseek-ai/dsh-session/types'

const WORKSPACE_PATH = 'D:\\MyWorkspace\\SHANIU'
const WORKSPACE_ID = 'mock-workspace-1'
const SESSION_IDS = ['mock-session-alpha', 'mock-session-beta']

/** 工作区行内的会话集（create/attach 时推进，follow 以 upsert 帧广播）。 */
const WORKSPACE_SESSION_IDS: string[] = [...SESSION_IDS]

/** 跨调用共享的 follow 帧队列（baseline 后挂起，push 即广播）。 */
function createFrameQueue<T>() {
  const values: T[] = []
  const waiters: ((value: T) => void)[] = []
  return {
    push(value: T): void {
      const waiter = waiters.shift()
      if (waiter !== undefined) waiter(value)
      else values.push(value)
    },
    async next(): Promise<T> {
      if (values.length > 0) return values.shift()!
      return new Promise<T>(resolve => waiters.push(resolve))
    },
  }
}
const workspaceFrames = createFrameQueue<{ readonly type: 'upsert'; readonly workspace: WorkspaceView }>()

function mockSessionRow(id: string, minutesAgo: number, running = false): SessionSummary {
  // wire 形状（types.ts 的 SessionSummary，sessionId 字段）——manager.projectList 按
  // entry.sessionId 重建 client 行；给成 client 形状（id 字段）会整行退化为 undefined key
  return {
    agentAvailable: true,
    sessionId: id as SessionId,
    cwd: WORKSPACE_PATH,
    running,
    blank: false,
    updatedAt: Date.now() - minutesAgo * 60_000,
    formatStatus: 'current',
  }
}

/**
 * 预置会话行的共享可变注册表：workspace 分组（sessionIds）与 session.list RPC
 * 消费同一份数据，保证侧栏与主区一致。
 */
const MOCK_SESSION_ROWS = new Map<string, SessionSummary>([
  [SESSION_IDS[0], mockSessionRow(SESSION_IDS[0], 42)],
  [SESSION_IDS[1], mockSessionRow(SESSION_IDS[1], 7, true)],
])

/**
 * `remote.session` 命名空间 mock：喂真实的 session-controller 客户端运行时。
 * 四个面 = manager 消费契约（见 api-session-controller/src/client/transport.ts）：
 * ① list RPC（侧栏列表）② control 流（会话级控制基线）③ follow 流（journal 窗口，
 * 空 records = 空会话原样渲染）④ page + 各 unary（prompt/cancel/rename/…）。
 */
function mockSessionNamespace(): unknown {
  return {
    list: async () => OK({ items: [...MOCK_SESSION_ROWS.values()] }),
    create: async (payload: { workspaceId?: string; cwd?: string } = {}) => {
      const id = `mock-session-${MOCK_SESSION_ROWS.size + 1}`
      MOCK_SESSION_ROWS.set(id, mockSessionRow(id, 0))
      // 新会话挂进工作区行（workspace-view.sessionIds），推 upsert 帧——
      // 会话视图的 chip/inert 门控读取 sessionWorkspace（未挂接 = 输入框 inert）
      WORKSPACE_SESSION_IDS.push(id)
      workspaceFrames.push({ type: 'upsert', workspace: mockWorkspaceView() })
      void payload
      return OK({ sessionId: id as SessionId })
    },
    fork: async () => OK({ sessionId: `mock-session-${MOCK_SESSION_ROWS.size + 1}` as SessionId }),
    search: async () => OK({ items: [], hasMore: false }),
    selectModel: async (req: { provider: string; model: string }) =>
      OK({ selected: { provider: req.provider, model: req.model } }),
    projections: async () => OK(null),
    // 模型目录（composer 模型席位数据源，形状 = session-controller 的 ModelCatalog）
    modelCatalog: async () => OK({
      default: { provider: 'mock-provider', model: 'mock-model-a' },
      routableProviders: ['mock-provider'],
      groups: [{
        id: 'mock-provider',
        name: 'Mock Provider',
        models: [
          { id: 'mock-model-a', name: 'Mock Model A', description: 'mock 旗舰模型' },
          { id: 'mock-model-b', name: 'Mock Model B', description: 'mock 轻量模型' },
        ],
      }],
      failures: [],
    }),
    control: async function* (): AsyncGenerator<unknown> {
      // 控制基线带 per-session 投影：modelSelection（composer 模型席位的当前选择来源）
      yield {
        type: 'baseline',
        value: {
          projections: Object.fromEntries([...MOCK_SESSION_ROWS.keys()].map(id => [id, {
            asOfSeq: 0,
            values: {
              modelSelection: {
                lastUsed: null,
                next: { provider: 'mock-provider', model: 'mock-model-a' },
              },
            },
          }])),
        },
      }
      await new Promise<void>(() => {})
    },
    follow: async function* (_request: unknown): AsyncGenerator<unknown> {
      yield {
        type: 'snapshot',
        header: { version: 0, id: '' as SessionId, createdAt: Date.now(), isSeeded: false },
        // 空 journal：cursor = 会话 emptyCursor（-1），与 journal 流的空页校验一致
        cursor: -1,
        records: [],
        hasMore: false,
        projections: {
          asOfSeq: 0,
          values: {
            modelSelection: { lastUsed: null, next: { provider: 'mock-provider', model: 'mock-model-a' } },
          },
        },
        assistantStream: { revision: 0 },
      }
      await new Promise<void>(() => {})
    },
    page: async () => OK({ records: [], hasMore: false }),
    prompt: async () => OK({}),
    cancel: async () => OK({ accepted: false }),
    rename: async () => OK({}),
    attachment: async () => OK({}),
    updateQueue: async () => OK({}),
  }
}

/** `modules` 服务 mock：静态组装下无动态模块系统，仅覆盖 entries 空面（设置-常规的发布行读取）。 */
export function mockModules(): unknown {
  const idleState = {
    getSnapshot: () => ({ syncing: false, failures: [] }),
    subscribe: () => () => {},
  }
  const noRelease = {
    getSnapshot: () => undefined,
    subscribe: () => () => {},
  }
  return new Proxy(
    {
      version: 'client',
      entries: {
        state: idleState,
        releaseChange: noRelease,
        dismissRelease: () => {},
        retry: async () => {},
      },
    } as Record<string | symbol, unknown>,
    {
      get(target, prop) {
        if (prop in target) return target[prop]
        if (typeof prop === 'symbol') return undefined
        throw new Error(`mock(modules).${String(prop)}: 该成员尚未 mock，见 transport/mock.ts`)
      },
    },
  )
}

/** `connection` 服务 mock：generation 恒为已连接（触发 session-controller 的 handleConnected）。 */
export function mockConnection(): unknown {
  return new Proxy(
    {
      generation: { getSnapshot: () => 1, subscribe: () => () => {} },
      state: { getSnapshot: () => 'ready', subscribe: () => () => {} },
      interactions: {},
      isLoopback: true,
    } as Record<string | symbol, unknown>,
    {
      get(target, prop) {
        if (prop in target) return target[prop]
        if (typeof prop === 'symbol') return undefined
        throw new Error(`mock(connection).${String(prop)}: 该成员尚未 mock，见 transport/mock.ts`)
      },
    },
  )
}

/** `typert` 服务 mock：仅覆盖 contexts.registerClient 注册面（返回解绑函数）。 */
export function mockTypert(): unknown {
  return new Proxy(
    {
      contexts: { registerClient: () => () => {} },
    } as Record<string | symbol, unknown>,
    {
      get(target, prop) {
        if (prop in target) return target[prop]
        if (typeof prop === 'symbol') return undefined
        throw new Error(`mock(typert).${String(prop)}: 该成员尚未 mock，见 transport/mock.ts`)
      },
    },
  )
}

function mockWorkspaceView(): WorkspaceView {
  return {
    workspaceId: WORKSPACE_ID as WorkspaceView['workspaceId'],
    path: WORKSPACE_PATH,
    title: 'SHANIU',
    sessionIds: WORKSPACE_SESSION_IDS as unknown as WorkspaceView['sessionIds'],
    createdAt: new Date(Date.now() - 86_400_000).toISOString(),
    updatedAt: new Date(Date.now()).toISOString(),
  }
}

const OK = <T>(value: T) => ({ ok: true as const, value })

/** mock 的 `remote.workspace` 命名空间：follow 活流（baseline + 队列广播）+ 本地可变更方法。 */
function mockWorkspaceNamespace(): unknown {
  return {
    // follow 帧形状 = api-workspace-controller 的 WorkspaceFollowFrame（baseline + 增量闭联合集）
    follow: async function* (_request: unknown): AsyncGenerator<unknown> {
      yield {
        type: 'baseline',
        value: { items: [mockWorkspaceView()], archivedSessionIds: [], pinnedSessionIds: [] },
      }
      // 活流：baseline 后消费帧队列（create/attach 推 upsert），无帧则挂起保持"已连接"
      for (;;) {
        yield await workspaceFrames.next()
      }
    },
    create: async (input: { path: string }) => OK({
      workspace: {
        ...mockWorkspaceView(),
        path: input.path,
        title: input.path.split(/[\\/]/).pop() ?? input.path,
      },
    }),
    rename: async (_id: string, title: string) => OK({ workspace: { ...mockWorkspaceView(), title } }),
    initializeDefault: async () => OK({ workspace: mockWorkspaceView() }),
    delete: async () => OK({}),
    insertBefore: async () => OK({ workspaceIds: [WORKSPACE_ID] }),
    archiveSession: async () => OK({ archivedSessionIds: [] }),
    unarchiveSession: async () => OK({ archivedSessionIds: [] }),
    pinSession: async () => OK({ pinnedSessionIds: [...WORKSPACE_SESSION_IDS] }),
    unpinSession: async () => OK({ pinnedSessionIds: [] }),
    insertSessionBefore: async () => OK({ workspace: mockWorkspaceView() }),
  }
}

/** 尚未 mock 的命名空间维持显式抛错面（按界面迭代逐个补）。 */
function mockNamespace(name: string): unknown {
  const thrower = () => {
    throw new Error(`mock(remote.${name}): 该命令面尚未 mock，见 transport/mock.ts`)
  }
  return new Proxy({}, {
    get(_t, prop) {
      if (typeof prop === 'symbol') return undefined
      return thrower
    },
  })
}

/** 单例 mock remote：保证 `ctx.remote.<ns>` 与 `remote.<ns>` 点分服务同源。 */
const MOCK_REMOTE = buildMockRemote()

function buildMockRemote(): unknown {
  const namespaces = new Map<string, unknown>([
    ['workspace', mockWorkspaceNamespace()],
    ['session', mockSessionNamespace()],
  ])
  const base = {
    $on: () => () => {},
    $emit: () => false,
    $call: () => Promise.reject(new Error('mock(remote).$call: 该调用尚未 mock，见 transport/mock.ts')),
    // 转发 open() 的异步可迭代：mock follow 帧经真实客户端投影机制流动。
    // 对齐 gateway RemoteStream 语义：
    // - 惰性开启：open() 在首次迭代时才调用（journal 流的 initialRequest 在 open(request) 时才就位）
    // - 信封 {generation, value, accept}：物理重启（restart）递增 generation，
    //   消费方据此重置 opening-snapshot 期待；accept() 为流控确认
    // - 流对象带 restart/dispose（RemoteSnapshotStream 重连/回收调用）
    $stream: (options: { open: (signal?: AbortSignal) => AsyncIterable<unknown> }) => {
      const state = {
        gen: 0,
        iter: null as AsyncIterator<unknown> | null,
      }
      const ensureIter = (): void => {
        if (state.iter === null) {
          state.iter = (options.open() as AsyncIterable<unknown>)[Symbol.asyncIterator]()
        }
      }
      const restart = (): void => {
        state.gen += 1
        void state.iter?.return?.(undefined)
        state.iter = null
      }
      const dispose = async (): Promise<void> => {
        await state.iter?.return?.(undefined)
        state.iter = null
      }
      return {
        [Symbol.asyncIterator]: () => ({
          next: async () => {
            ensureIter()
            const result = await state.iter!.next()
            if (result.done) return { done: true as const, value: undefined as never }
            return {
              done: false as const,
              value: { generation: state.gen, value: result.value, accept: () => {} },
            }
          },
          return: async () => {
            await state.iter?.return?.(undefined)
            state.iter = null
            return { done: true as const, value: undefined as never }
          },
        }),
        restart,
        dispose,
      }
    },
    $mount: async () => {
      throw new Error('mock(remote).$mount: 由适配层取代，见 transport/shims')
    },
  } as Record<string | symbol, unknown>
  return new Proxy(base, {
    get(target, prop) {
      if (prop in target) return target[prop]
      if (typeof prop === 'symbol') return undefined
      if (!namespaces.has(prop)) namespaces.set(prop, mockNamespace(prop))
      return namespaces.get(prop)
    },
  })
}

export function mockRemote(): unknown {
  return MOCK_REMOTE
}

const NAMESPACES = [
  'session', 'commands', 'toolResults', 'userQuestions', 'workspace', 'workspaceFiles',
  'directoryPicker', 'job', 'terminal', 'settings', 'subagents', 'modelSelection',
] as const

/** 批量注册 mock 的 `remote.<ns>` 点分服务（与 ctx.remote.<ns> 同源同实例）。 */
export function installMockRemoteNamespaces(ctx: {
  reflect: { provide(name: string, value: unknown): unknown }
}): void {
  const remote = MOCK_REMOTE as Record<string, unknown>
  for (const name of NAMESPACES) {
    ctx.reflect.provide(`remote.${name}`, remote[name])
  }
}
