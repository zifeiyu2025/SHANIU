/**
 * Mock 数据源（SH-008 会话全流程联调的 mock 后端，ADR-006 接线模型）。
 * 仅纯浏览器开发态启用（dsh-boot 按 `__TAURI_INTERNALS__` 分流，绝不进 Tauri 运行时）。
 * 与 stubs.ts 的区别：mock 允许"伪造成功"——按真实形状预置数据、驱动真实客户端投影
 * （workspace-controller 的 follow 流/模型机制原样运行），用于把界面各区域填充到可联调状态。
 */
import type {
  ISessions, SessionListState, SessionRetainInfo, SessionSummary,
} from '@deepseek-ai/dsh-api-session-controller/client'
import type { WorkspaceView } from '@deepseek-ai/dsh-api-workspace-controller/client'
import { createSnapshotStore } from '@deepseek-ai/dsh-client-store'
import type { SessionId } from '@deepseek-ai/dsh-session/types'

const WORKSPACE_PATH = 'D:\\MyWorkspace\\SHANIU'
const SESSION_IDS = ['mock-session-alpha', 'mock-session-beta']

function mockSessionRow(id: string, title: string, minutesAgo: number, running = false): SessionSummary {
  return {
    id: id as SessionId,
    title,
    displayTitle: title,
    running,
    retainedBy: {},
    blank: false,
    updatedAt: Date.now() - minutesAgo * 60_000,
    cwd: WORKSPACE_PATH,
  }
}

/**
 * `sessions` 服务 mock：预置会话列表 + 可变更（create 即造行，全流程 mock 语义）。
 * 面形状与 stubSessions 相同的四个成员 + 其余成员经 Proxy 抛错。
 */
export function mockSessions(): ISessions {
  const rows = new Map<string, SessionSummary>([
    [SESSION_IDS[0], mockSessionRow(SESSION_IDS[0], 'SH-008 mock：读改跑联调', 42)],
    [SESSION_IDS[1], mockSessionRow(SESSION_IDS[1], '沙箱探针测试记录', 7, true)],
  ])
  const list = createSnapshotStore<SessionListState>({
    ids: [...rows.keys()] as SessionId[],
    byId: Object.fromEntries(rows) as SessionListState['byId'],
    phase: 'ready',
    projectionsBySession: {},
  })
  const publish = (): void => {
    list.set({
      ids: [...rows.keys()] as SessionId[],
      byId: Object.fromEntries(rows) as SessionListState['byId'],
      phase: 'ready',
      projectionsBySession: {},
    })
  }
  const retention = new Map<string, ReturnType<typeof createSnapshotStore<SessionRetainInfo>>>()
  const base = {
    // 上游会话搜索单次上限常量（SESSION_SEARCH_RESULT_LIMIT）
    searchResultLimit: 20,
    list,
    create: async (options?: { sessionId?: SessionId }) => {
      const id = options?.sessionId ?? `mock-session-${rows.size + 1}`
      rows.set(id, mockSessionRow(id, '新会话', 0))
      publish()
      return id as SessionId
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
  return new Proxy(base, {
    get(target, prop) {
      if (prop in target) return target[prop as keyof typeof target]
      if (typeof prop === 'symbol') return undefined
      throw new Error(`mock(sessions).${String(prop)}: 该成员尚未 mock，见 transport/mock.ts`)
    },
  }) as unknown as ISessions
}

function mockWorkspaceView(): WorkspaceView {
  return {
    workspaceId: 'mock-workspace-1' as WorkspaceView['workspaceId'],
    path: WORKSPACE_PATH,
    title: 'SHANIU',
    sessionIds: SESSION_IDS as unknown as WorkspaceView['sessionIds'],
    createdAt: new Date(Date.now() - 86_400_000).toISOString(),
    updatedAt: new Date(Date.now()).toISOString(),
  }
}

const OK = <T>(value: T) => ({ ok: true as const, value })

/** mock 的 `remote.workspace` 命名空间：follow 基线帧 + 本地可变更方法。 */
function mockWorkspaceNamespace(): unknown {
  return {
    // follow 帧形状 = api-workspace-controller 的 WorkspaceFollowFrame（baseline + 增量闭联合集）
    follow: async function* (): AsyncGenerator<unknown> {
      yield {
        type: 'baseline',
        value: { items: [mockWorkspaceView()], archivedSessionIds: [], pinnedSessionIds: [] },
      }
      // 保持"已连接"：无后端即无后续帧（绝不结束流）
      await new Promise<void>(() => {})
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
    insertBefore: async () => OK({ workspaceIds: ['mock-workspace-1'] }),
    archiveSession: async () => OK({ archivedSessionIds: [] }),
    unarchiveSession: async () => OK({ archivedSessionIds: [] }),
    pinSession: async () => OK({ pinnedSessionIds: [...SESSION_IDS] }),
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
  ])
  const base = {
    $on: () => () => {},
    $emit: () => false,
    $call: () => Promise.reject(new Error('mock(remote).$call: 该调用尚未 mock，见 transport/mock.ts')),
    // 转发 open() 的异步可迭代：mock follow 帧经真实客户端投影机制流动。
    // 信封形状对齐 gateway RemoteStream 的产出（{generation, value, accept}），
    // RemoteSnapshotStream 消费时按 generation 分代并调用 accept() 做流控确认。
    $stream: (options: { open: (signal?: AbortSignal) => AsyncIterable<unknown> }) => {
      async function* forward(): AsyncGenerator<unknown> {
        for await (const frame of options.open()) {
          yield { generation: 0, value: frame, accept: () => {} }
        }
      }
      return forward()
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
