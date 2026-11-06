/**
 * Mock 数据源（SH-008 会话全流程联调的 mock 后端，ADR-006 接线模型）。
 * 仅纯浏览器开发态启用（dsh-boot 按 `__TAURI_INTERNALS__` 分流，绝不进 Tauri 运行时）。
 * 与 stubs.ts 的区别：mock 允许"伪造成功"——按真实形状预置数据、驱动真实客户端投影
 * （workspace-controller 的 follow 流/模型机制原样运行），用于把界面各区域填充到可联调状态。
 */
import type { SessionSummary } from '@deepseek-ai/dsh-api-session-controller/types'
import type { ModelSelection } from '@deepseek-ai/dsh-api-session-controller/types'
import type { WorkspaceView } from '@deepseek-ai/dsh-api-workspace-controller/client'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import { createModelsMock } from './mock-models'

/**
 * 模型设置页与 composer 模型席位共用的那份 mock 事实源（见 mock-models.ts）。
 * 单例：设置页写入的 provider 必须就是席位读到的那一个。
 * emit 走 mock 的事件总线，写入后设置页与席位才会各自收到失效推送。
 */
const MODELS = createModelsMock({ emit: mockEmit, onSelectionChanged: pushModelSelection })

const WORKSPACE_PATH = 'D:\\MyWorkspace\\SHANIU'
const WORKSPACE_ID = 'mock-workspace-1'
const DEFAULT_WORKSPACE_ID = 'default-workspace'
/**
 * 默认工作区的目录。真实落盘位置由 Rust 侧决定（用户口径是 data 目录下的 Workspace），
 * 这里只作为 mock 展示值；注意 docs/01:426 把自身 data 目录对 Agent 默认拒绝读写，
 * 工作区又是 Agent 可写作用域，落盘前必须先解决这条边界冲突。
 */
const DEFAULT_WORKSPACE_PATH = `${WORKSPACE_PATH}\\Workspace`

/**
 * 权限挡位：值与上游 permission-presets 一致。
 * name 直接给机器值——上游 displayPermissionPreset 只在 name === value 或等于其英文默认标签时
 * 才走界面词典，给自定义英文名会让中文界面露出英文标签。
 */
const PERMISSION_PRESETS = [
  { value: 'read-only', name: 'read-only', description: '只读查看，不产生任何写入' },
  { value: 'workspace-write', name: 'workspace-write', description: '仅在工作区内读写' },
  { value: 'danger-full-access', name: 'danger-full-access', description: '不受工作区边界限制' },
  { value: 'auto', name: 'auto', description: '自动审查后再决定是否放行' },
] as const

const DEFAULT_PERMISSION = 'read-only'
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

/**
 * 广播式帧通道：控制流每个订阅者都要看到同一份变化（单消费者队列会把帧分给其中一条流）。
 * 用于 modelSelection 投影——席位读投影，不回推就会在选档后回弹。
 */
function createFrameBroadcast<T>() {
  interface Subscriber { buffer: T[]; waiters: ((value: T) => void)[] }
  const subscribers = new Set<Subscriber>()
  return {
    push(value: T): void {
      for (const subscriber of [...subscribers]) {
        const waiter = subscriber.waiters.shift()
        if (waiter !== undefined) waiter(value)
        else subscriber.buffer.push(value)
      }
    },
    async *subscribe(): AsyncGenerator<T> {
      const subscriber: Subscriber = { buffer: [], waiters: [] }
      subscribers.add(subscriber)
      try {
        for (;;) {
          if (subscriber.buffer.length > 0) yield subscriber.buffer.shift()!
          else yield await new Promise<T>(resolve => subscriber.waiters.push(resolve))
        }
      } finally {
        subscribers.delete(subscriber)
      }
    },
  }
}

const projectionFrames = createFrameBroadcast<Record<string, unknown>>()
let projectionSeq = 0

/** 把一次选择变化推给每个已知会话的控制流。 */
function pushModelSelection(selection: ModelSelection): void {
  for (const id of MOCK_SESSION_ROWS.keys()) {
    projectionFrames.push({
      type: 'projection',
      sessionId: id as SessionId,
      seq: ++projectionSeq,
      changes: [{ key: 'modelSelection', val: { lastUsed: null, next: selection } }],
    })
  }
}

/** 每个会话的当前权限挡位；`permissions` 投影只带 currentValue，未设置即默认档。 */
const PERMISSION_BY_SESSION = new Map<string, string>()

/** 把一次挡位切换推给对应会话（宿主 `/permission` 命令的确认帧）。 */
function pushPermission(sessionId: string, currentValue: string): void {
  projectionFrames.push({
    type: 'projection',
    sessionId,
    seq: ++projectionSeq,
    changes: [{ key: 'permissions', val: { currentValue } }],
  })
}

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
 * 席位投影里的当前选择：直接取同源目录的默认项，
 * 免得设置页删掉某条路由后投影还指向一个不存在的分组。
 */
async function currentModelSelection(): Promise<ModelSelection> {
  const catalog = await MODELS.session.modelCatalog()
  return catalog.ok ? catalog.value.default : { provider: '', model: '' }
}

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
    selectModel: async (req: ModelSelection) => MODELS.session.selectModel(req),
    projections: async () => OK(null),
    // 模型目录（composer 模型席位数据源）与设置页同源：只有已配置的服务商进得来。
    modelCatalog: () => MODELS.session.modelCatalog(),
    control: async function* (): AsyncGenerator<unknown> {
      // 控制基线带 per-session 投影：modelSelection（composer 模型席位的当前选择来源）
      const selection = await currentModelSelection()
      yield {
        type: 'baseline',
        value: {
          projections: Object.fromEntries([...MOCK_SESSION_ROWS.keys()].map(id => [id, {
            asOfSeq: 0,
            values: {
              modelSelection: { lastUsed: null, next: selection },
              permissions: { currentValue: PERMISSION_BY_SESSION.get(id) ?? DEFAULT_PERMISSION },
            },
          }])),
        },
      }
      // 基线之后持续推送选择变化：席位读投影，不回推会导致选档后回弹
      for await (const frame of projectionFrames.subscribe()) yield frame
    },
    follow: async function* (request: unknown): AsyncGenerator<unknown> {
      const sessionId = (request as { sessionId?: string } | undefined)?.sessionId
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
            modelSelection: { lastUsed: null, next: await currentModelSelection() },
            permissions: { currentValue: (sessionId === undefined ? undefined : PERMISSION_BY_SESSION.get(sessionId)) ?? DEFAULT_PERMISSION },
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
      // 形状必须是 ConnectionGeneration（{id, host}）：权限目录按 generation.id 栅栏，
      // 给裸数字会让它永远读不出目录，输入卡的挡位控件就静默不渲染
      generation: { getSnapshot: () => ({ id: 1, host: { home: WORKSPACE_PATH } }), subscribe: () => () => {} },
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

function mockWorkspaceView(id: string = WORKSPACE_ID): WorkspaceView {
  const isDefault = id === DEFAULT_WORKSPACE_ID
  return {
    workspaceId: id as WorkspaceView['workspaceId'],
    path: isDefault ? DEFAULT_WORKSPACE_PATH : WORKSPACE_PATH,
    // 上游用「title === 'default-workspace'」判定默认工作区，界面据此渲染成「默认工作区」；
    // 不是独立选项，也没有 isDefault 字段（见 api/workspace-controller/src/default-workspace.ts）
    title: isDefault ? DEFAULT_WORKSPACE_ID : 'SHANIU',
    sessionIds: (isDefault ? [] : WORKSPACE_SESSION_IDS) as unknown as WorkspaceView['sessionIds'],
    createdAt: new Date(Date.now() - 86_400_000).toISOString(),
    updatedAt: new Date(Date.now()).toISOString(),
  }
}

const OK = <T>(value: T) => ({ ok: true as const, value })

/** `remote.permissionPresets`：进程级目录（与上游 PermissionCatalog 同形）。 */
function mockPermissionPresetsNamespace(): unknown {
  return {
    catalog: async () => OK({
      options: PERMISSION_PRESETS.map(preset => ({ ...preset })),
      // 自动审查是实验档，不作为新会话的默认值
      defaultOptions: PERMISSION_PRESETS.filter(preset => preset.value !== 'auto').map(preset => ({ ...preset })),
      defaultPreset: DEFAULT_PERMISSION,
    }),
  }
}

/**
 * `remote.commands`：目前只需要 `/permission <preset>` 这一条。
 * 返回值非 undefined 即"命中命令"（SessionFace.command 据此给出 matched）。
 */
function mockCommandsNamespace(): unknown {
  return {
    execute: async (sessionId: string, line: string) => {
      const match = /^\/permission\s+([a-z][a-z-]*)\s*$/.exec(line.trim())
      if (match === null) return OK(undefined)
      const preset = match[1]!
      if (!PERMISSION_PRESETS.some(candidate => candidate.value === preset)) return OK(undefined)
      PERMISSION_BY_SESSION.set(sessionId, preset)
      pushPermission(sessionId, preset)
      return OK({ matched: true })
    },
  }
}

/** mock 的 `remote.workspace` 命名空间：follow 活流（baseline + 队列广播）+ 本地可变更方法。 */
function mockWorkspaceNamespace(): unknown {
  return {
    // follow 帧形状 = api-workspace-controller 的 WorkspaceFollowFrame（baseline + 增量闭联合集）
    follow: async function* (_request: unknown): AsyncGenerator<unknown> {
      yield {
        type: 'baseline',
        // 默认工作区排在最前：它的 title 就是 'default-workspace'，界面会渲染成「默认工作区」
        value: { items: [mockWorkspaceView(DEFAULT_WORKSPACE_ID), mockWorkspaceView()], archivedSessionIds: [], pinnedSessionIds: [] },
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
    // 上游语义：注册表为空且无任何持久化/归档会话时才创建；mock 里默认工作区已存在，直接回它
    initializeDefault: async () => OK({ workspace: mockWorkspaceView(DEFAULT_WORKSPACE_ID) }),
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

/**
 * mock 的事件总线：`$on` 按注册序登记、`$emit` 同步派发。
 * 设置页与席位都靠推送失效收敛（settings/document-updated、credentials/reference-updated、
 * llm/adapters-updated），空壳 $on 会让写入后界面停在旧视图——注册序就是它们的刷新序。
 */
const mockListeners = new Map<string, Set<(payload: unknown) => void>>()

function mockOn(name: string, listener: (payload: unknown) => void): () => void {
  const listeners = mockListeners.get(name) ?? new Set<(payload: unknown) => void>()
  listeners.add(listener)
  mockListeners.set(name, listeners)
  return () => { listeners.delete(listener) }
}

function mockEmit(name: string, payload?: unknown): boolean {
  const listeners = mockListeners.get(name)
  if (listeners === undefined || listeners.size === 0) return false
  for (const listener of [...listeners]) listener(payload)
  return true
}

function buildMockRemote(): unknown {
  const namespaces = new Map<string, unknown>([
    ['workspace', mockWorkspaceNamespace()],
    ['session', mockSessionNamespace()],
    // 模型设置页的三张面：由同一份 mock-models 事实源应答（见 mock-models.ts）
    ['llm', MODELS.llm],
    ['settings', MODELS.settings],
    ['credentials', MODELS.credentials],
    // 权限挡位：进程目录 + `/permission` 命令写回
    ['permissionPresets', mockPermissionPresetsNamespace()],
    ['commands', mockCommandsNamespace()],
  ])
  const base = {
    $on: mockOn,
    $emit: mockEmit,
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
  'credentials', 'llm', 'permissionPresets',
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
