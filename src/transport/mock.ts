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
import {
  catalogEntryOf, foldTimingEvent, initTimingState, timingProjectionOf,
  type SubagentCatalogEntry, type SubagentTimingState,
} from './subagent-wire'
import { createModelsMock } from './mock-models'

/**
 * 模型设置页与 composer 模型席位共用的那份 mock 事实源（见 mock-models.ts）。
 * 单例：设置页写入的 provider 必须就是席位读到的那一个。
 * emit 走 mock 的事件总线，写入后设置页与席位才会各自收到失效推送。
 */
const MODELS = createModelsMock({ emit: mockEmit, onSelectionChanged: pushModelSelection })

/** 宿主 home：`$host.home` 与路径缩写用，也作默认工作区的 mock 目录。 */
const HOME_PATH = 'D:\\MyWorkspace\\SHANIU'
const DEFAULT_WORKSPACE_ID = 'default-workspace'
/**
 * 默认工作区的目录。真实落盘位置由 Rust 侧决定：安装版 `%LOCALAPPDATA%\SHANIU\Workspace`、
 * 便携版 `<exe>\data\Workspace`（docs/01 §9.3），桌面壳启动时经 `get_product_dirs` 覆盖进来
 * （见 setDefaultWorkspacePath）。这里的常量只是纯浏览器联调的展示值——取本机确实存在的目录，
 * 桌面壳的「资源管理器」按存在的目录校验，指向不存在的路径就只是点了没反应。
 * 注意 docs/01 §9.3 把自身 data 目录对 Agent 默认拒绝读写，工作区又是 Agent 可写作用域——
 * 边界例外已写明放行 `<数据根>\Workspace` 子树。
 */
const DEFAULT_WORKSPACE_PATH = HOME_PATH

/**
 * 权限挡位：值与上游 permission-presets 一致，但**不含 `auto`（实验档）**——那是
 * "让当前模型自动审查每次工具调用再决定放行"的档位，SHANIU 后端没有对应语义，
 * 用户裁定移除（2026-11-07）。桌面侧这张面整个未挂（见 transport/stubs.ts 的
 * `sandbox.mode × approval.mode` 映射说明），mock 不单独保留一个后端给不出的挡位。
 * name 直接给机器值——上游 displayPermissionPreset 只在 name === value 或等于其英文默认标签时
 * 才走界面词典，给自定义英文名会让中文界面露出英文标签。
 */
const PERMISSION_PRESETS = [
  { value: 'read-only', name: 'read-only', description: '只读查看，不产生任何写入' },
  { value: 'workspace-write', name: 'workspace-write', description: '仅在工作区内读写' },
  { value: 'danger-full-access', name: 'danger-full-access', description: '不受工作区边界限制' },
] as const

const DEFAULT_PERMISSION = 'read-only'

/** 一个工作区在 mock 里的全部可变状态。 */
interface MockWorkspace {
  path: string
  title: string
  /** 行内会话（工作区视图的 sessionIds，侧栏分组与 inert 门控都读它）。 */
  sessionIds: string[]
  createdAt: string
  updatedAt: string
}

/**
 * 出厂态：只有默认工作区，里面一条会话也没有——首启的新会话由界面现场建出来。
 * 工作区行与 `remote.session.list` 各自消费这两份注册表，创建路径同一次写入两侧。
 */
const WORKSPACES = new Map<string, MockWorkspace>([
  [DEFAULT_WORKSPACE_ID, {
    path: DEFAULT_WORKSPACE_PATH,
    title: DEFAULT_WORKSPACE_ID,
    sessionIds: [],
    createdAt: new Date(Date.now() - 86_400_000).toISOString(),
    updatedAt: new Date().toISOString(),
  }],
])

/**
 * 桌面壳把默认工作区指到宿主解析出的 `<数据根>\Workspace`（boot 时经 `get_product_dirs` 读取）。
 * 必须早于界面读取工作区基线；行由 `workspaceView` 现场投影，改注册表即生效。
 * @param path - 宿主给出的默认工作区绝对路径。
 */
export function setDefaultWorkspacePath(path: string): void {
  const workspace = WORKSPACES.get(DEFAULT_WORKSPACE_ID)
  if (workspace !== undefined) workspace.path = path
}

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

/** 每个会话的 mock journal：已提交事件（供重开 follow 回放）+ seq 游标 + 帧广播 + 流式 revision。 */
interface MockJournal {
  seq: number
  revision: number
  readonly records: unknown[]
  readonly frames: ReturnType<typeof createFrameBroadcast<Record<string, unknown>>>
}

const JOURNALS = new Map<string, MockJournal>()

function journalOf(sessionId: string): MockJournal {
  let journal = JOURNALS.get(sessionId)
  if (journal === undefined) {
    journal = { seq: -1, revision: 0, records: [], frames: createFrameBroadcast<Record<string, unknown>>() }
    JOURNALS.set(sessionId, journal)
  }
  return journal
}

/** 会话标题：侧栏行与会话头都读 `title` 投影。出厂为空，首条用户消息之后才起名。 */
const SESSION_TITLES = new Map<string, string>()

/** 固定演示剧本：本地 mock，不联网、不调用任何模型。 */
const DEMO_REPLY = '你好！我是 SHANIU 的助手。这条回复来自本地 mock 剧本，用于演示流式渲染、消息动作条与用量统计。'

/** 出厂未配置服务商时的失败回合：message 直接上屏（该 code 没有官方词典条目）。 */
const NO_MODEL_CODE = 'NO_MODEL_CONFIGURED'
const NO_MODEL_MESSAGE = '还没有配置可用的模型服务商，点输入框里的「模型配置」填一个 API Key。'

function chunksOf(text: string, size: number): string[] {
  const parts: string[] = []
  for (let index = 0; index < text.length; index += size) parts.push(text.slice(index, index + size))
  return parts
}

/** 演示用整轮统计：字段与上游 sessionStats / tokenUsage / contextPressure 三个投影一一对应。 */
interface MockMetrics {
  turns: number
  steps: number
  llmMs: number
  toolMs: number
  ttftMs: number
  ttftSteps: number
  decodeMs: number
  decodeTokens: number
  uncachedInputTokens: number
  outputTokens: number
  cacheReadTokens: number
  cacheWriteTokens: number
}

const METRICS = new Map<string, MockMetrics>()

/** 一个演示回合的统计增量：decode 900 tok / 5.2s ≈ 173 tok/s，与参照截图同量级。 */
const PER_TURN = {
  llmMs: 3800,
  ttftMs: 300,
  ttftSteps: 1,
  decodeMs: 5200,
  decodeTokens: 900,
  uncachedInputTokens: 9600,
  outputTokens: 300,
}

/** 该会话当前的三个统计投影；没有跑过回合就不挂（界面据此隐藏底部指标）。 */
function metricsProjectionsOf(sessionId: string): Record<string, unknown> {
  const metrics = METRICS.get(sessionId)
  if (metrics === undefined) return {}
  const total = metrics.uncachedInputTokens + metrics.outputTokens + metrics.cacheReadTokens + metrics.cacheWriteTokens
  return {
    sessionStats: {
      turns: metrics.turns,
      steps: metrics.steps,
      llmMs: metrics.llmMs,
      toolMs: metrics.toolMs,
      ttftMs: metrics.ttftMs,
      ttftSteps: metrics.ttftSteps,
      decodeMs: metrics.decodeMs,
      decodeTokens: metrics.decodeTokens,
    },
    tokenUsage: {
      uncachedInputTokens: metrics.uncachedInputTokens,
      outputTokens: metrics.outputTokens,
      cacheReadTokens: metrics.cacheReadTokens,
      cacheWriteTokens: metrics.cacheWriteTokens,
    },
    // 上下文占用：projectedTokens / contextWindow 决定那个百分数；没跑过模型就是 0%
    contextPressure: {
      pressureTokens: total,
      projectedTokens: total,
      contextWindow: 1_000_000,
    },
  }
}

/**
 * 回合收尾时把统计推给界面。`'none'` 是没跑到模型就失败的回合：只推进轮/步计数，
 * 用量桶保持 0——真实后端此时不下发 usage，上游也只在 decodeMs>0 时才画速度。
 */
function pushTurnMetrics(sessionId: string, usage: 'model' | 'none'): void {
  const metrics = METRICS.get(sessionId) ?? {
    turns: 0, steps: 0, llmMs: 0, toolMs: 0, ttftMs: 0, ttftSteps: 0, decodeMs: 0, decodeTokens: 0,
    uncachedInputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0,
  }
  metrics.turns += 1
  metrics.steps += 1
  if (usage === 'model') {
    metrics.llmMs += PER_TURN.llmMs
    metrics.ttftMs += PER_TURN.ttftMs
    metrics.ttftSteps += PER_TURN.ttftSteps
    metrics.decodeMs += PER_TURN.decodeMs
    metrics.decodeTokens += PER_TURN.decodeTokens
    metrics.uncachedInputTokens += PER_TURN.uncachedInputTokens
    metrics.outputTokens += PER_TURN.outputTokens
  }
  METRICS.set(sessionId, metrics)
  projectionFrames.push({
    type: 'projection',
    sessionId,
    seq: ++projectionSeq,
    changes: Object.entries(metricsProjectionsOf(sessionId)).map(([key, val]) => ({ key, val })),
  })
}

/**
 * 演示用助手回合：按真实 wire 形状推 journal 事件与 assistant-stream 帧。
 * 两条硬约束来自客户端的 journal 流（gateway/client/journal-stream.ts）：
 * 事件 seq 必须严格 +1（否则触发 page 补齐并报协议错），
 * assistant-stream 的 revision 必须跨回合单调 +1。
 */
function startMockTurn(sessionId: string, requestId: string | undefined, text: string): void {
  const journal = journalOf(sessionId)
  const pushEvent = (type: string, data: unknown, extra: Record<string, unknown> = {}, time = Date.now()): number => {
    journal.seq += 1
    const record = { type: 'event', event: { type, seq: journal.seq, time, ...extra, data } }
    journal.records.push(record)
    journal.frames.push(record)
    return journal.seq
  }
  void (async () => {
    if (!SESSION_TITLES.has(sessionId)) {
      const title = text.trim().slice(0, 12) || '新会话'
      SESSION_TITLES.set(sessionId, title)
      projectionFrames.push({
        type: 'projection',
        sessionId,
        seq: ++projectionSeq,
        changes: [{ key: 'title', val: title }],
      })
    }
    const turnStart = Date.now()
    // turn/step 必须逐轮递增：写死 0 会让第二轮的 turn/start 落在一个已关闭的回合上，
    // 过程头（已完成，用时 N 秒）就不出现
    const turn = METRICS.get(sessionId)?.turns ?? 0
    const step = METRICS.get(sessionId)?.steps ?? 0
    pushEvent('turn/start', { turn }, {}, turnStart)
    pushEvent('step/start', { turn, step })
    const userSeq = pushEvent('user/message', {
      id: `mock-user-${journal.seq + 1}`,
      role: 'user',
      content: [{ type: 'text', text }],
      // rpcId 必须等于 prompt 请求里的 requestId，客户端据此去重掉本地乐观回显
      source: { kind: 'user', ...(requestId === undefined ? {} : { rpcId: requestId }) },
    }, { surfaceOp: 'append' })

    const catalog = await MODELS.session.modelCatalog()
    if (catalog.ok && catalog.value.groups.length === 0) {
      // 一个服务商都没配（出厂态）：真实后端在这一步就失败，mock 不凭空编一段回答。
      // code 不在上游可续跑集合（RATE_LIMIT/SERVER/…）里，界面因此只留这条错误行。
      pushEvent('step/end', { turn, step })
      pushEvent('turn/end', {
        turn,
        reason: { kind: 'error', error: { code: NO_MODEL_CODE, message: NO_MODEL_MESSAGE } },
      }, {}, Date.now())
      pushTurnMetrics(sessionId, 'none')
      return
    }

    const attemptId = `mock-attempt-${sessionId}-${journal.revision + 1}`
    // 回答来源如实取目录：优先用席位的当前选择，没选则退到第一条可路由路由的首个模型
    const modelSource = catalog.ok
      ? (catalog.value.default.provider !== ''
        ? { provider: catalog.value.default.provider, model: catalog.value.default.model }
        : { provider: catalog.value.groups[0]?.id ?? '', model: catalog.value.groups[0]?.models[0]?.id ?? '' })
      : { provider: '', model: '' }
    const pushStream = (frame: Record<string, unknown>): void => {
      journal.revision += 1
      journal.frames.push({ type: 'assistant-stream', frame: { ...frame, attemptId, revision: journal.revision } })
    }
    pushStream({ type: 'start', startedAfterSeq: userSeq, turn, step })
    let index = 0
    for (const chunk of chunksOf(DEMO_REPLY, 6)) {
      await new Promise<void>(resolve => setTimeout(resolve, 45))
      pushStream({ type: 'chunk', index, time: Date.now(), chunk: { type: 'text-delta', index: 0, text: chunk } })
      index += 1
    }
    const assistantSeq = pushEvent('assistant/message', {
      turn, step,
      message: {
        id: `mock-assistant-${journal.seq + 1}`,
        role: 'assistant',
        content: [{ type: 'text', text: DEMO_REPLY }],
        source: { kind: 'model', provider: modelSource.provider, model: modelSource.model },
      },
      stream: [],
      // 三个桶齐全时必须满足 total = input + output + read + write
      usage: { inputTokens: 9600, outputTokens: 300, totalTokens: 9900, cacheReadTokens: 0, cacheWriteTokens: 0 },
    }, { surfaceOp: 'append' })
    pushStream({ type: 'end', index, outcome: { kind: 'committed', eventType: 'assistant/message', seq: assistantSeq } })
    pushEvent('step/end', { turn, step })
    // turn/end 的时间决定「已完成，用时 N 秒」的显示时长
    pushEvent('turn/end', { turn, reason: { kind: 'completed' } }, {}, turnStart + 4000)
    // 回合关闭后再发布统计：上游的轮/步计数只认已关闭的 step
    pushTurnMetrics(sessionId, 'model')
  })()
}

// ---- 子代理泳道演示（SH-109）：输入 /subagent-demo 触发 ----
// 浏览器联调态没有真实子代理后端（M5 未开工），这条剧本按上游 wire 形状伪造
// 「父代理 spawn 两条泳道并行探索」的完整一轮：父 journal 落 `subagent/catalog` v3、
// 两条泳道各自 descriptor/turn/tool 事件流与 timing 折叠（subagent-wire 的单点语义）、
// 运行状态经 api-session/status、投影值经控制流帧。形状与适配层将来产出的一致，
// SH-053 接真实事件源时 UI 零改动。

/** 演示触发命令：在会话输入框原样发送它。 */
const SUBAGENT_DEMO_COMMAND = '/subagent-demo'

/** 演示泳道登记：laneId → 父会话与中断标志（remote.subagents.interruptByUser 置位）。 */
const DEMO_LANES = new Map<string, { parentSessionId: string; aborted: boolean }>()

/** 演示泳道的父目录（subagentCatalog 投影值，控制流推帧与控制基线共用）。 */
const DEMO_CATALOG = new Map<string, SubagentCatalogEntry[]>()

/** 演示泳道的 timing 折叠（subagentTiming 投影值，语义照上游 projection.ts）。 */
const DEMO_TIMING = new Map<string, SubagentTimingState>()

/** 演示节奏：泳道流式帧间隔。 */
const LANE_TICK_MS = 90

/** 演示运行序号（泳道 id 的一部分，保证重复触发不撞同一 journal）。 */
let demoRun = 0

const sleep = (ms: number): Promise<void> => new Promise(resolve => setTimeout(resolve, ms))

/** 一条演示泳道的剧本输入。 */
interface MockLaneSpec {
  readonly name: string
  readonly label: string
  readonly reply: string
  readonly tool: { name: string; args: string; output: string }
}

const LANE_SPECS: readonly MockLaneSpec[] = [
  {
    name: 'explorer-a',
    label: '探索 transport 模块边界',
    reply: 'transport 层共 6 个模块：adapter/tauri/mock/stubs/settings-face/shims。adapter 是唯一对接面，事件入口 onEvent 按 EventMsg 分派到会话 journal。',
    tool: { name: 'read_file', args: '{"path":"src/transport/adapter.ts"}', output: '/* 适配层源码约 1900 行：事件路由、journal 帧、投影推帧，此处省略 */' },
  },
  {
    name: 'explorer-b',
    label: '汇总测试基线',
    reply: 'vitest 当前 156 例全绿，覆盖 adapter 契约、mock 回合剧本、投影折叠与挂载注册四类面。',
    tool: { name: 'unified_exec', args: '{"command":"pnpm test"}', output: 'Test Files  9 passed (9)\n     Tests  156 passed (156)' },
  },
]

/** 演示 journal 的一条事件推进（与 startMockTurn 内联版同一规则：seq +1、records/frames 双写）。 */
function pushDemoEvent(journal: MockJournal, type: string, data: unknown, extra: Record<string, unknown> = {}, time = Date.now()): number {
  journal.seq += 1
  const record = { type: 'event', event: { type, seq: journal.seq, time, ...extra, data } }
  journal.records.push(record)
  journal.frames.push(record)
  return journal.seq
}

/** 控制流投影帧（与 pushPermission 同一通道；帧会建出该会话的投影 store）。 */
function pushLaneProjection(sessionId: string, changes: { key: string; val: unknown }[]): void {
  projectionFrames.push({ type: 'projection', sessionId: sessionId as SessionId, seq: ++projectionSeq, changes })
}

/** subagentCatalog / subagentTiming 的当前投影值（控制基线用；帧路径另有整表推帧）。 */
export function subagentProjectionsOf(sessionId: string): Record<string, unknown> {
  const timing = DEMO_TIMING.get(sessionId)
  if (timing !== undefined) {
    const view = timingProjectionOf(timing)
    return view === undefined ? {} : { subagentTiming: view }
  }
  const catalog = DEMO_CATALOG.get(sessionId)
  return catalog === undefined ? {} : { subagentCatalog: catalog }
}

/**
 * 一条演示泳道的完整剧本：descriptor → 回合（流式应答 + 工具调用）→ 收口。
 * timing 按 subagent-wire 的上游语义折叠（turn 开合推帧，其余只推进不推帧）；
 * `aborted` 是 interruptByUser 置的中断位，剧本在每个检查点退出并按 aborted 收口。
 */
function runMockLane(laneId: string, spec: MockLaneSpec): Promise<void> {
  const lane = DEMO_LANES.get(laneId)
  if (lane === undefined) return Promise.resolve()
  const journal = journalOf(laneId)
  const timingState = (): SubagentTimingState => DEMO_TIMING.get(laneId) ?? initTimingState()
  const pushTimingFrame = (): void => {
    const view = timingProjectionOf(timingState())
    if (view !== undefined) pushLaneProjection(laneId, [{ key: 'subagentTiming', val: view }])
  }
  const pushStream = (frame: Record<string, unknown>): void => {
    journal.revision += 1
    journal.frames.push({
      type: 'assistant-stream',
      frame: { ...frame, attemptId: `mock-lane-attempt-${laneId}`, revision: journal.revision },
    })
  }
  return (async () => {
    const turn = 0
    pushDemoEvent(journal, 'subagent/descriptor', {
      version: 3, mode: 'continuable', provider: 'shaniu-mock', label: spec.label,
    })
    DEMO_TIMING.set(laneId, foldTimingEvent(timingState(), 'subagent/descriptor', Date.now()))
    pushTimingFrame()
    mockEmit('api-session/status', laneId, true)
    pushDemoEvent(journal, 'turn/start', { turn })
    DEMO_TIMING.set(laneId, foldTimingEvent(timingState(), 'turn/start', Date.now()))
    pushTimingFrame()
    pushDemoEvent(journal, 'step/start', { turn, step: 0 })
    pushStream({ type: 'start', startedAfterSeq: journal.seq, turn, step: 0 })
    let index = 0
    for (const chunk of chunksOf(spec.reply, 8)) {
      if (lane.aborted) break
      await sleep(LANE_TICK_MS)
      pushStream({ type: 'chunk', index, time: Date.now(), chunk: { type: 'text-delta', index: 0, text: chunk } })
      index += 1
      DEMO_TIMING.set(laneId, foldTimingEvent(timingState(), 'advance', Date.now()))
    }
    if (lane.aborted) {
      pushStream({ type: 'end', index, outcome: { kind: 'abandoned' } })
    } else {
      const assistantSeq = pushDemoEvent(journal, 'assistant/message', {
        turn, step: 0,
        message: {
          id: `mock-lane-assistant-${journal.seq + 1}`,
          role: 'assistant',
          content: [{ type: 'text', text: spec.reply }],
          source: { kind: 'model', provider: 'shaniu-mock', model: 'lane-model' },
        },
        stream: [],
        usage: { inputTokens: 1200, outputTokens: 180, totalTokens: 1380, cacheReadTokens: 0, cacheWriteTokens: 0 },
      }, { surfaceOp: 'append' })
      pushStream({ type: 'end', index, outcome: { kind: 'committed', eventType: 'assistant/message', seq: assistantSeq } })
    }
    pushDemoEvent(journal, 'step/end', { turn, step: 0 })
    if (!lane.aborted) {
      await sleep(LANE_TICK_MS * 2)
      DEMO_TIMING.set(laneId, foldTimingEvent(timingState(), 'advance', Date.now()))
      pushDemoEvent(journal, 'step/start', { turn, step: 1 })
      pushDemoEvent(journal, 'tool/call', {
        turn, step: 1, callId: `mock-lane-call-${laneId}`, name: spec.tool.name, arguments: spec.tool.args,
      })
      await sleep(LANE_TICK_MS * 3)
      DEMO_TIMING.set(laneId, foldTimingEvent(timingState(), 'advance', Date.now()))
      pushDemoEvent(journal, 'tool/result', {
        turn, step: 1,
        message: {
          id: `mock-lane-result-${laneId}`,
          role: 'tool',
          source: { kind: 'tool', callId: `mock-lane-call-${laneId}` },
          toolCallId: `mock-lane-call-${laneId}`,
          content: [{ type: 'text', text: spec.tool.output }],
          isError: false,
        },
      }, { surfaceOp: 'append' })
      pushDemoEvent(journal, 'step/end', { turn, step: 1 })
    }
    pushDemoEvent(journal, 'turn/end', {
      turn,
      reason: lane.aborted ? { kind: 'aborted', reason: { kind: 'user' } } : { kind: 'completed' },
    })
    DEMO_TIMING.set(laneId, foldTimingEvent(timingState(), 'turn/end', Date.now(), lane.aborted ? 'aborted' : 'completed'))
    pushTimingFrame()
    mockEmit('api-session/status', laneId, false)
  })()
}

/**
 * 泳道演示剧本：父回合落用户命令 → catalog v3 事件与投影帧 → 两条泳道并行跑 →
 * 父代理汇总收口。父回合的 turn/step 与消息流沿用 startMockTurn 的 wire 规则。
 */
function startMockSubagentDemo(parentId: string, requestId: string | undefined): void {
  const journal = journalOf(parentId)
  const createdAt = Date.now()
  // 泳道 id 带 run 序号：演示可重复触发，两次运行的 lane journal 互不串味
  demoRun += 1
  const laneIds = LANE_SPECS.map(spec => `mock-subagent-${spec.name}-run${demoRun}`)
  const catalog: SubagentCatalogEntry[] = []
  LANE_SPECS.forEach((spec, i) => {
    const childId = laneIds[i]! as SessionId
    const data = {
      version: 3 as const, childId, childCreatedAt: createdAt + i,
      mode: 'continuable' as const, label: spec.label, name: spec.name,
    }
    pushDemoEvent(journal, 'subagent/catalog', data)
    catalog.push(catalogEntryOf(data))
    DEMO_LANES.set(laneIds[i]!, { parentSessionId: parentId, aborted: false })
  })
  DEMO_CATALOG.set(parentId, catalog)
  pushLaneProjection(parentId, [{ key: 'subagentCatalog', val: catalog }])
  // 父回合号接续既有计数（会话里跑过普通回合后再演示，回合头才不落在已关闭的回合上）
  const turn = METRICS.get(parentId)?.turns ?? 0
  pushDemoEvent(journal, 'turn/start', { turn })
  pushDemoEvent(journal, 'step/start', { turn, step: 0 })
  pushDemoEvent(journal, 'user/message', {
    id: `mock-user-${journal.seq + 1}`,
    role: 'user',
    content: [{ type: 'text', text: SUBAGENT_DEMO_COMMAND }],
    // rpcId 对账客户端的乐观回显：命中即退休本地行，避免消息出现两遍
    source: { kind: 'user', ...(requestId === undefined ? {} : { rpcId: requestId }) },
  }, { surfaceOp: 'append' })
  void (async () => {
    // 泳道错峰起步；全部收口后父代理才汇总（wait_agent 的剧本形态）
    await Promise.all(LANE_SPECS.map(async (spec, i) => {
      await sleep(i * 400)
      await runMockLane(laneIds[i]!, spec)
    }))
    const summary = `两条子代理已完成：${LANE_SPECS.map(spec => spec.name).join('、')}。各自的探索结果已并入本轮上下文。`
    const pushStream = (frame: Record<string, unknown>): void => {
      journal.revision += 1
      journal.frames.push({
        type: 'assistant-stream',
        frame: { ...frame, attemptId: `mock-demo-parent-${journal.revision}`, revision: journal.revision },
      })
    }
    pushStream({ type: 'start', startedAfterSeq: journal.seq, turn, step: 0 })
    let index = 0
    for (const chunk of chunksOf(summary, 8)) {
      await sleep(LANE_TICK_MS)
      pushStream({ type: 'chunk', index, time: Date.now(), chunk: { type: 'text-delta', index: 0, text: chunk } })
      index += 1
    }
    const assistantSeq = pushDemoEvent(journal, 'assistant/message', {
      turn, step: 0,
      message: {
        id: `mock-demo-summary-${journal.seq + 1}`,
        role: 'assistant',
        content: [{ type: 'text', text: summary }],
        source: { kind: 'model', provider: 'shaniu-mock', model: 'lane-model' },
      },
      stream: [],
      usage: { inputTokens: 2600, outputTokens: 90, totalTokens: 2690, cacheReadTokens: 0, cacheWriteTokens: 0 },
    }, { surfaceOp: 'append' })
    pushStream({ type: 'end', index, outcome: { kind: 'committed', eventType: 'assistant/message', seq: assistantSeq } })
    pushDemoEvent(journal, 'step/end', { turn, step: 0 })
    pushDemoEvent(journal, 'turn/end', { turn, reason: { kind: 'completed' } })
    pushTurnMetrics(parentId, 'model')
  })()
}

/**
 * `remote.subagents`（SH-109）：泳道子会话的 prompt / interruptByUser 面（上游
 * session-controller 对 addressed 子会话的硬路由，见 sessions/session.ts）。演示
 * 语义：prompt = 向运行中的演示泳道注入一条 steering 消息（当前 step 内落
 * user/message + 短应答）；interruptByUser = 置中断位，剧本在下一检查点按 aborted
 * 收口。非演示泳道如实拒绝（上游错误码），不伪造受理。
 */
function mockSubagentsNamespace(): unknown {
  const steer = (laneId: string, requestId: string | undefined, text: string): void => {
    // continuable 子会话的续聊：新开一个回合（与父会话 prompt 的事件序列同构）
    const journal = journalOf(laneId)
    const turn = 1
    const step = 0
    pushDemoEvent(journal, 'turn/start', { turn })
    pushDemoEvent(journal, 'step/start', { turn, step })
    pushDemoEvent(journal, 'user/message', {
      id: `mock-lane-user-${journal.seq + 1}`,
      role: 'user',
      content: [{ type: 'text', text }],
      source: { kind: 'user', ...(requestId === undefined ? {} : { rpcId: requestId }) },
    }, { surfaceOp: 'append' })
    const reply = `收到，补充上下文已并入当前探索：${text}`
    journal.revision += 1
    const attemptId = `mock-lane-steer-${journal.revision}`
    journal.frames.push({ type: 'assistant-stream', frame: { type: 'start', startedAfterSeq: journal.seq, turn, step, attemptId, revision: journal.revision } })
    const assistantSeq = pushDemoEvent(journal, 'assistant/message', {
      turn, step,
      message: {
        id: `mock-lane-steer-reply-${journal.seq + 1}`,
        role: 'assistant',
        content: [{ type: 'text', text: reply }],
        source: { kind: 'model', provider: 'shaniu-mock', model: 'lane-model' },
      },
      stream: [],
    }, { surfaceOp: 'append' })
    journal.revision += 1
    journal.frames.push({ type: 'assistant-stream', frame: { type: 'end', index: 0, outcome: { kind: 'committed', eventType: 'assistant/message', seq: assistantSeq }, attemptId, revision: journal.revision } })
    pushDemoEvent(journal, 'step/end', { turn, step })
    pushDemoEvent(journal, 'turn/end', { turn, reason: { kind: 'completed' } })
  }
  return {
    prompt: async (request: { childSessionId?: string; requestId?: string; content?: readonly { type: string; text?: string }[] } = {}) => {
      const laneId = request.childSessionId
      const lane = laneId === undefined ? undefined : DEMO_LANES.get(laneId)
      if (laneId === undefined || lane === undefined) {
        return FAIL('subagent/not-found', '没有这个子代理会话（演示泳道经 /subagent-demo 创建）')
      }
      if (lane.aborted) {
        return FAIL('subagent/delivery-unavailable', '该子代理已结束，steering 通道已关闭')
      }
      const text = (request.content ?? [])
        .filter(part => part.type === 'text')
        .map(part => part.text ?? '')
        .join('')
      steer(laneId, request.requestId, text)
      return OK({ messageId: `mock-lane-steer-${Date.now()}` })
    },
    interruptByUser: async (childSessionId?: string) => {
      const lane = childSessionId === undefined ? undefined : DEMO_LANES.get(childSessionId)
      if (lane === undefined) {
        return FAIL('subagent/not-found', '没有这个子代理会话')
      }
      lane.aborted = true
      return OK({ accepted: true })
    },
  }
}

function mockSessionRow(id: string, cwd: string): SessionSummary {
  // wire 形状（types.ts 的 SessionSummary，sessionId 字段）——manager.projectList 按
  // entry.sessionId 重建 client 行；给成 client 形状（id 字段）会整行退化为 undefined key
  return {
    agentAvailable: true,
    sessionId: id as SessionId,
    cwd,
    running: false,
    blank: false,
    updatedAt: Date.now(),
    formatStatus: 'current',
  }
}

/**
 * 会话行注册表：workspace 分组（sessionIds）与 session.list RPC 消费同一份数据，
 * 保证侧栏与主区一致。出厂为空——首启那条由界面建出来。
 */
const MOCK_SESSION_ROWS = new Map<string, SessionSummary>()

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
      const workspaceId = payload.workspaceId !== undefined && WORKSPACES.has(payload.workspaceId)
        ? payload.workspaceId
        : DEFAULT_WORKSPACE_ID
      const workspace = WORKSPACES.get(workspaceId)!
      MOCK_SESSION_ROWS.set(id, mockSessionRow(id, payload.cwd ?? workspace.path))
      // 新建会话吃设置里的「权限默认档」；已存在的会话保持各自当前值（上游语义）
      PERMISSION_BY_SESSION.set(id, MODELS.permissionDefault() ?? DEFAULT_PERMISSION)
      // 新会话挂进工作区行（workspace-view.sessionIds），推 upsert 帧——
      // 会话视图的 chip/inert 门控读取 sessionWorkspace（未挂接 = 输入框 inert）
      workspace.sessionIds.push(id)
      pushWorkspaceFrame(workspaceId)
      return OK({ sessionId: id as SessionId })
    },
    fork: async () => OK({ sessionId: `mock-session-${MOCK_SESSION_ROWS.size + 1}` as SessionId }),
    search: async () => OK({ items: [], hasMore: false }),
    selectModel: async (req: ModelSelection) => MODELS.session.selectModel(req),
    projections: async () => OK(null),
    // 模型目录（composer 模型席位数据源）与设置页同源：只有已配置的服务商进得来。
    modelCatalog: () => MODELS.session.modelCatalog(),
    control: async function* (): AsyncGenerator<unknown> {
      // 控制基线带 per-session 投影：modelSelection（composer 模型席位的当前选择来源）。
      // 泳道（演示子代理）不在侧栏行注册表里，但投影 store 由基线/帧建出——
      // 目录树与泳道徽标读的就是这些值，基线必须把它们带上。
      const selection = await currentModelSelection()
      const ids = [...MOCK_SESSION_ROWS.keys(), ...DEMO_LANES.keys()]
      yield {
        type: 'baseline',
        value: {
          projections: Object.fromEntries(ids.map(id => [id, {
            asOfSeq: 0,
            values: {
              modelSelection: { lastUsed: null, next: selection },
              permissions: { currentValue: PERMISSION_BY_SESSION.get(id) ?? DEFAULT_PERMISSION },
              title: SESSION_TITLES.get(id) ?? null,
              ...metricsProjectionsOf(id),
              ...subagentProjectionsOf(id),
            },
          }])),
        },
      }
      // 基线之后持续推送选择变化：席位读投影，不回推会导致选档后回弹
      for await (const frame of projectionFrames.subscribe()) yield frame
    },
    follow: async function* (request: unknown): AsyncGenerator<unknown> {
      // 会话地址是联合类型：`{kind:'session', sessionId}` 或 `{kind:'subagent', childSessionId}`
      const address = (request as { address?: { kind?: string; sessionId?: string; childSessionId?: string } } | undefined)?.address
      const sessionId = address?.kind === 'session' ? address.sessionId : address?.childSessionId
      const journal = sessionId === undefined ? undefined : journalOf(sessionId)
      yield {
        type: 'snapshot',
        header: { version: 0, id: '' as SessionId, createdAt: Date.now(), isSeeded: false },
        // 回放已提交事件：cursor 必须等于最后一条 record 的 seq（空则 -1），
        // 否则 journal 流会触发 page 补齐并判协议错
        cursor: journal?.seq ?? -1,
        records: journal?.records ?? [],
        hasMore: false,
        projections: {
          asOfSeq: 0,
          values: {
            modelSelection: { lastUsed: null, next: await currentModelSelection() },
            permissions: { currentValue: (sessionId === undefined ? undefined : PERMISSION_BY_SESSION.get(sessionId)) ?? DEFAULT_PERMISSION },
            title: sessionId === undefined ? null : (SESSION_TITLES.get(sessionId) ?? null),
            ...(sessionId === undefined ? {} : metricsProjectionsOf(sessionId)),
          },
        },
        assistantStream: { revision: journal?.revision ?? 0 },
      }
      // 存档回放完毕后接着推实时帧（助手回合的事件与流式块）
      if (journal !== undefined) {
        for await (const frame of journal.frames.subscribe()) yield frame
      }
    },
    page: async () => OK({ records: [], hasMore: false }),
    prompt: async (request: { requestId?: string; sessionId?: string; content?: readonly { type: string; text?: string }[] } = {}) => {
      const text = (request.content ?? [])
        .filter(part => part.type === 'text')
        .map(part => part.text ?? '')
        .join('')
      // 子代理泳道演示（SH-109）：/subagent-demo 触发两条泳道并行探索的完整剧本
      if (request.sessionId !== undefined && text.trim() === SUBAGENT_DEMO_COMMAND) {
        startMockSubagentDemo(request.sessionId, request.requestId)
        return OK({ accepted: true })
      }
      if (request.sessionId !== undefined) startMockTurn(request.sessionId, request.requestId, text)
      return OK({ accepted: true })
    },
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
      generation: { getSnapshot: () => ({ id: 1, host: { home: HOME_PATH } }), subscribe: () => () => {} },
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

/**
 * 工作区行。默认工作区的 title 就是 'default-workspace'，上游据此判定首启自动标题并
 * 渲染成「默认工作区」（见 api-workspace-controller/src/default-workspace.ts），
 * 没有独立的 isDefault 字段。
 */
function workspaceView(id: string): WorkspaceView {
  const workspace = WORKSPACES.get(id)!
  return {
    workspaceId: id as WorkspaceView['workspaceId'],
    path: workspace.path,
    title: workspace.title,
    sessionIds: workspace.sessionIds as unknown as WorkspaceView['sessionIds'],
    createdAt: workspace.createdAt,
    updatedAt: workspace.updatedAt,
  }
}

/** 变更过后广播一行工作区（follow 的增量面）。 */
function pushWorkspaceFrame(id: string): void {
  const workspace = WORKSPACES.get(id)
  if (workspace === undefined) return
  workspace.updatedAt = new Date().toISOString()
  workspaceFrames.push({ type: 'upsert', workspace: workspaceView(id) })
}

const OK = <T>(value: T) => ({ ok: true as const, value })
const FAIL = (code: string, message: string) => ({ ok: false as const, error: { code, message } })

/** `remote.permissionPresets`：进程级目录（与上游 PermissionCatalog 同形）。 */
function mockPermissionPresetsNamespace(): unknown {
  return {
    catalog: async () => OK({
      // 挡位清单本身就不含实验档 auto（见 PERMISSION_PRESETS 注记），目录与默认值同表
      options: PERMISSION_PRESETS.map(preset => ({ ...preset })),
      defaultOptions: PERMISSION_PRESETS.map(preset => ({ ...preset })),
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
    // 命令目录给空表（与适配层同口径）：ui-commands 的预热读不到 list 会在
    // 输入 "/" 时报 warmup failed，slash 演示命令就被挡住了
    list: async () => OK([]),
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
        // 出厂只有默认工作区；用户后来添加的目录由 create 进注册表
        value: { items: [...WORKSPACES.keys()].map(workspaceView), archivedSessionIds: [], pinnedSessionIds: [] },
      }
      // 活流：baseline 后消费帧队列（create/attach 推 upsert），无帧则挂起保持"已连接"
      for (;;) {
        yield await workspaceFrames.next()
      }
    },
    create: async (input: { path: string }) => {
      const id = `mock-workspace-${WORKSPACES.size + 1}`
      const now = new Date().toISOString()
      WORKSPACES.set(id, {
        path: input.path,
        title: input.path.split(/[\\/]/).pop() ?? input.path,
        sessionIds: [],
        createdAt: now,
        updatedAt: now,
      })
      pushWorkspaceFrame(id)
      return OK({ workspace: workspaceView(id) })
    },
    rename: async (id: string, title: string) => {
      const workspace = WORKSPACES.get(id)
      if (workspace !== undefined) workspace.title = title
      pushWorkspaceFrame(id)
      return OK({ workspace: workspaceView(id) })
    },
    // 上游语义：注册表为空且无任何持久化/归档会话时才创建；mock 里默认工作区已存在，直接回它
    initializeDefault: async () => OK({ workspace: workspaceView(DEFAULT_WORKSPACE_ID) }),
    delete: async () => OK({}),
    insertBefore: async () => OK({ workspaceIds: [...WORKSPACES.keys()] }),
    archiveSession: async () => OK({ archivedSessionIds: [] }),
    unarchiveSession: async () => OK({ archivedSessionIds: [] }),
    pinSession: async () => OK({ pinnedSessionIds: [...WORKSPACES.values()].flatMap(workspace => workspace.sessionIds) }),
    unpinSession: async () => OK({ pinnedSessionIds: [] }),
    insertSessionBefore: async () => OK({ workspace: workspaceView(DEFAULT_WORKSPACE_ID) }),
  }
}

/** mock 的 `remote.workspaceFiles` 命名空间：目录清空，只剩保持打开的变更流。 */
function mockWorkspaceFilesNamespace(): unknown {
  const unavailable = async () => FAIL('workspace-files/unavailable', 'mock 尚未接线文件读取')
  return {
    list: async (_sessionId: string, path: string) => OK({ path, entries: [], truncated: false }),
    // 文档预览会按需读文件；mock 没有真实工作区文件，如实失败而不是给空内容
    read: unavailable,
    readBytes: unavailable,
    // 变更流必须保持打开：结束会被文件面板当成 watch 失败并弹提示
    changes: async function* (_sessionId: string, _path: string, signal?: AbortSignal): AsyncGenerator<unknown> {
      yield { kind: 'ready' }
      await new Promise<void>((resolve) => {
        if (signal?.aborted === true) resolve()
        else signal?.addEventListener('abort', () => resolve(), { once: true })
      })
    },
  }
}

/**
 * 目录选择器的 mock 目录树：只建目录（选择器只列目录），键为归一后的绝对路径。
 * 浏览与新建都写这份表，新建出来的目录随后能真的列出来。
 */
const MOCK_DIRECTORY_TREE = new Map<string, string[]>([
  [HOME_PATH, ['docs', 'evals', 'scripts', 'src', 'src-tauri']],
  [`${HOME_PATH}\\docs`, []],
  [`${HOME_PATH}\\evals`, []],
  [`${HOME_PATH}\\scripts`, []],
  [`${HOME_PATH}\\src`, ['brand', 'dsh', 'header-actions', 'host', 'model-config', 'transport']],
  [`${HOME_PATH}\\src\\brand`, []],
  [`${HOME_PATH}\\src\\dsh`, ['packages', 'vendor']],
  [`${HOME_PATH}\\src\\header-actions`, []],
  [`${HOME_PATH}\\src\\host`, []],
  [`${HOME_PATH}\\src\\model-config`, []],
  [`${HOME_PATH}\\src\\transport`, []],
  [`${HOME_PATH}\\src-tauri`, ['src']],
])

/** 目录路径归一：统一反斜杠、去掉尾部分隔符。 */
function normalizeDirectoryPath(path: string): string {
  const unified = path.replace(/\//g, '\\').replace(/\\+$/, '')
  return unified.length === 0 ? HOME_PATH : unified
}

/** 面包屑：根（home，显示完整路径）→ 每一级子目录。 */
function directoryCrumbs(path: string): { name: string; path: string; hidden: boolean }[] {
  const crumbs = [{ name: HOME_PATH, path: HOME_PATH, hidden: false }]
  if (path === HOME_PATH) return crumbs
  let current = HOME_PATH
  for (const segment of path.slice(HOME_PATH.length).split('\\').filter(Boolean)) {
    current = `${current}\\${segment}`
    crumbs.push({ name: segment, path: current, hidden: segment.startsWith('.') })
  }
  return crumbs
}

/** mock 的 `remote.directoryPicker` 命名空间：应用内目录浏览的选择原语。 */
function mockDirectoryPickerNamespace(): unknown {
  const listing = (path: string) => {
    const names = MOCK_DIRECTORY_TREE.get(path)
    if (names === undefined) return FAIL('directory-picker/unreadable', `mock 没有这个目录：${path}`)
    return OK({
      path,
      home: HOME_PATH,
      crumbs: directoryCrumbs(path),
      entries: names.map(name => ({ name, path: `${path}\\${name}`, hidden: name.startsWith('.') })),
      truncated: false,
    })
  }
  return {
    // 系统「选择文件夹」：桌面壳经 Tauri 命令弹原生对话框；纯浏览器没有系统对话框，
    // 返回 null（等同取消）——浏览器态由 browse 占用者接管，不会走到这里。
    pick: async () => {
      if (!('__TAURI_INTERNALS__' in window)) return OK(null)
      const { invoke } = await import('@tauri-apps/api/core')
      return OK(await invoke<string | null>('pick_folder'))
    },
    list: async (path?: string) => listing(normalizeDirectoryPath(path ?? HOME_PATH)),
    createDirectory: async (path: string, name: string) => {
      const parent = normalizeDirectoryPath(path)
      const siblings = MOCK_DIRECTORY_TREE.get(parent)
      if (siblings === undefined) return FAIL('directory-picker/unreadable', `mock 没有这个目录：${parent}`)
      const target = `${parent}\\${name}`
      if (MOCK_DIRECTORY_TREE.has(target)) return FAIL('directory-picker/exists', `目录已存在：${target}`)
      MOCK_DIRECTORY_TREE.set(parent, [...siblings, name].sort((a, b) => a.localeCompare(b)))
      MOCK_DIRECTORY_TREE.set(target, [])
      return OK(target)
    },
  }
}

/** mock 的插件管理面：面板如实显示「此环境不可管理」（真实管理属二期/打包接线）。 */
function mockPluginInventoryNamespace(): unknown {
  return {
    list: async () => OK({ managementAvailable: false }),
  }
}

function mockPluginManagerNamespace(): unknown {
  const unavailable = async () => FAIL('plugin-manager/unavailable', '桌面壳尚未接线插件管理')
  return {
    listBundles: unavailable,
    listPlugins: unavailable,
    registries: unavailable,
    readBundleDetail: unavailable,
    setBundleEnabled: unavailable,
    setPluginEnabled: unavailable,
    removeBundle: unavailable,
    listVersions: unavailable,
    updateBundle: unavailable,
    inspect: unavailable,
    installBundle: unavailable,
    waitForInstall: unavailable,
    cancelInstall: unavailable,
  }
}

function mockPluginRegistryProbeNamespace(): unknown {
  return {
    fastest: async () => FAIL('plugin-registry/unavailable', '桌面壳尚未接线插件源探测'),
  }
}

/**
 * mock 的 `remote.schedule`：目录为空，自动化面板显示空态；
 * 写入面显式失败——不为「创建/运行自动化任务」伪造成功。
 */
function mockScheduleNamespace(): unknown {
  const unavailable = async () => FAIL('schedule/unavailable', 'mock 未实现自动化任务写入')
  return {
    catalog: async () => OK([]),
    history: async () => OK({ items: [], hasMore: false }),
    deliveryLimits: async () => OK({}),
    list: async () => OK({ items: [] }),
    view: async () => OK({}),
    create: unavailable,
    edit: unavailable,
    run: unavailable,
    deleteInactive: unavailable,
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
const mockListeners = new Map<string, Set<(...args: unknown[]) => void>>()

function mockOn(name: string, listener: (...args: unknown[]) => void): () => void {
  const listeners = mockListeners.get(name) ?? new Set<(payload: unknown) => void>()
  listeners.add(listener)
  mockListeners.set(name, listeners)
  return () => { listeners.delete(listener) }
}

function mockEmit(name: string, ...args: unknown[]): boolean {
  const listeners = mockListeners.get(name)
  if (listeners === undefined || listeners.size === 0) return false
  for (const listener of [...listeners]) listener(...args)
  return true
}

function buildMockRemote(): unknown {
  const namespaces = new Map<string, unknown>([
    ['workspace', mockWorkspaceNamespace()],
    ['workspaceFiles', mockWorkspaceFilesNamespace()],
    // 「添加工作区」对话框的浏览原语（应用内目录树）
    ['directoryPicker', mockDirectoryPickerNamespace()],
    // 侧栏面板：插件（只读不可管理）与自动化任务（空目录）
    ['pluginInventory', mockPluginInventoryNamespace()],
    ['pluginManager', mockPluginManagerNamespace()],
    ['pluginRegistryProbe', mockPluginRegistryProbeNamespace()],
    ['schedule', mockScheduleNamespace()],
    ['session', mockSessionNamespace()],
    // 模型设置页的三张面：由同一份 mock-models 事实源应答（见 mock-models.ts）
    ['llm', MODELS.llm],
    ['settings', MODELS.settings],
    ['credentials', MODELS.credentials],
    // 权限挡位：进程目录 + `/permission` 命令写回
    ['permissionPresets', mockPermissionPresetsNamespace()],
    ['commands', mockCommandsNamespace()],
    // 子代理泳道（SH-109）：泳道子会话的 prompt/interruptByUser 面（演示语义）
    ['subagents', mockSubagentsNamespace()],
  ])
  const base = {
    // 固定宿主事实：ui-tool 的读文件行会用 home 缩写路径；缺了它代理会返回假命名空间，
    // 行内 abbreviateHomePath 收到对象而不是字符串，整行被槽位错误边界吞掉。
    // isLoopback 保持 false：此前缺省即 falsy，换成 true 会顺带打开设置页的文档控制器。
    $host: { home: HOME_PATH, isLoopback: false },
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
  'pluginManager', 'pluginInventory', 'pluginRegistryProbe', 'schedule',
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
