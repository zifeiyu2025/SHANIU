/**
 * 适配层（SH-017）：SHANIU 的 `EventMsg` 事件流 ⇄ DSH 客户端消费的会话 journal 词汇表。
 *
 * 这一层是 ADR-006 规定的唯一对接面：DSH 的 UI 代码不因后端而改动，全部阻抗差在这里消化。
 *
 * 三条硬约束决定了映射的写法（违反任何一条，客户端判协议错而不是降级显示）：
 * ① journal 事件的 `seq` 必须逐条 +1，且 snapshot 的 cursor 必须等于末条 record 的 seq
 *    （`client/transport.ts` 的 follows 判据）；
 * ② 事件信封只允许 `type/seq/time/data/ignorable/surfaceOp/sourceEventSeqs` 六个键，
 *    且 `user/message`、`assistant/message`、`tool/result` 这三类 surface 事件**必须**带
 *    `surfaceOp`、其余类型**禁止**带（`assertSessionWireEvent` + `validateSurfaceMetadata`）；
 * ③ `assistant-stream` 帧的 revision 从 snapshot 基线起逐帧 +1，缺帧即 carrier 错误。
 *
 * 后端没有的能力在这里**如实缺省**，不伪造：
 * - 工具输出增量：DSH 词汇表里没有「输出流事件」，终端卡片只读 `tool/result` 的最终内容，
 *   所以 stdout/stderr 在此累积到终态一次性呈现（逐字流式输出属于 SH-094 的传输面）。
 * - 历史回放：`follow` 打开本进程没观察过的会话时读 `load_session` 重建 journal
 *   （SH-033），重建出的事件序列与实时路径逐项一致——否则"重开旧会话"和"刚跑完"
 *   会给出两份不同的真相。用量不伪造：档案没有回合级汇总就不写 `usage`。
 * - 审批：走 typert 事件面（`approval/request`），不是 journal 事件。
 */
import { RemoteError } from '@deepseek-ai/dsh-typert-protocol'
import type { SettingsPathOpView } from '@deepseek-ai/dsh-settings/types'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import type { WorkspaceId } from '@deepseek-ai/dsh-workspace/types'
import type { MessageId } from '@deepseek-ai/dsh-llm'
import { createSettingsFace } from './settings-face'
import {
  PERMISSION_PRESETS, PERMISSION_PRESET_OPTIONS, presetOfSandbox, sandboxOfPreset,
} from './permission-presets'
import { CommandDefinitionId } from '@deepseek-ai/dsh-commands/brand'
import type { CommandDescriptor } from '@deepseek-ai/dsh-commands/types'
import { requestExport } from './command-triggers'
import {
  catalogEntryOf, foldTimingEvent, initTimingState, laneIdOf, timingProjectionOf,
  type SubagentCatalogEntry, type SubagentTimingState,
} from './subagent-wire'
import type { AgentEvent, EventMsg } from './types'
import type { HostBridge } from './tauri'

const OK = <T>(value: T) => ({ ok: true as const, value })
const FAIL = (message: string) =>
  ({ ok: false as const, error: new RemoteError('gateway/internal', message, {}) })

/**
 * 宿主的会话 id 是裸字符串；进 DSH 的类型化载荷时补上品牌。品牌运行时即擦除
 * （铸造函数在宿主编译域，typecheck 域外），这里的显式转换表达"裸 id 进类型化载荷"。
 */
const asSessionId = (id: string) => id as unknown as SessionId
const asWorkspaceId = (id: string) => id as unknown as WorkspaceId

/** 上游 token-meter 的用量四桶（usage-projection.ts bucketsFrom 的 wire 形状）。 */
interface TokenBuckets {
  readonly uncachedInputTokens: number
  readonly outputTokens: number
  readonly cacheReadTokens: number
  readonly cacheWriteTokens: number
}

/**
 * 宿主 `search_sessions` 的返回（SH-113）：与 vendored `session.search` wire
 * 一一对应——`items` 只有 `sessionId` + `snippet`，`hasMore` 是「命中会话数超过
 * 宿主固定上限、请收窄查询」提示（`slots.ts:280`），**不是**分页/续页信号。
 * DSH 客户端没有 Show more 请求、不传 cursor/limit，故命令面也只有 `{query}`。
 */
interface SearchPageWire {
  readonly items: readonly { sessionId: string; snippet: string }[]
  readonly hasMore: boolean
}

/** `session.search` wire 的一条命中（与 vendored `SessionSearchResultItem` 同形）。 */
interface SearchHitWire {
  readonly sessionId: string
  readonly snippet: string
}

/** 客户端排队的一条待发消息（回合进行中的提交；回合收口后按序自动补发）。 */
interface QueuedSubmission {
  /** inbox 行与 updateQueue 的 itemId（上游 = UserMessage.id）。 */
  readonly id: string
  /** 客户端提交身份；补发落 durable user/message 时回填 source.rpcId，客户端据此退休回声。 */
  readonly requestId: string
  text: string
  /** 用户气泡展示文本（不含附件块）；缺省回落到 text。 */
  displayText?: string
  /** 排队消息的可持久图片引用（回合收口补发时重建 image 内容块）。 */
  images?: readonly PromptImageRef[]
}

/**
 * 底部指标 pill 的折叠状态，字段语义照上游 session-stats：全量日志折叠出的回合/步数
 * 与墙钟时间，与客户端分页了多少历史无关。时间按事件到达时刻测量（墙钟口径，与上游
 * 同义）；档案重建没有可信时刻，只折叠计数、不折叠时间。
 */
interface SessionMetrics {
  turns: number
  steps: number
  llmMs: number
  toolMs: number
  ttftMs: number
  ttftSteps: number
  decodeMs: number
  decodeTokens: number
  buckets: TokenBuckets
}

const zeroMetrics = (): SessionMetrics => ({
  turns: 0, steps: 0, llmMs: 0, toolMs: 0, ttftMs: 0, ttftSteps: 0, decodeMs: 0, decodeTokens: 0,
  buckets: { uncachedInputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0 },
})

/**
 * 会话的帧序列：journal 事件与助手流帧**同轴**入队，消费者按自己的下标追平。
 *
 * 为什么不用「订阅式广播」：异步生成器的订阅要等到首次 next() 才真正登记，
 * 在那之前入队的帧两头都不沾（既不在快照里，也不在订阅缓冲里），客户端就会
 * 看到 seq 跳号并判协议错。按下标读取不存在注册时机问题——晚来的消费者
 * 从自己捕获的下标继续读，一条不丢。
 */
class FrameLog {
  private readonly entries: Record<string, unknown>[] = []
  private readonly waiters: (() => void)[] = []

  get length(): number {
    return this.entries.length
  }

  append(entry: Record<string, unknown>): void {
    this.entries.push(entry)
    for (const wake of this.waiters.splice(0)) wake()
  }

  /**
   * 从 `from` 起把已入队的条目全部交出，追平后挂起等新帧。
   *
   * `signal` 中止时立刻返回：消费者关流（`return()` / `dispose()`）时生成器正挂起在
   * 等待帧上，没有这道闸就永远收不到 return——真机表现是"关掉会话再打开，新流开不起来
   * 或旧流一直挂着"，比丢帧难归因得多。
   */
  async *tail(from: number, signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> {
    // 用函数读中止态：await 之后 signal 的状态可能已变，直接读布尔会让类型收窄
    // 认为"它永远是 false"。
    const closed = (): boolean => signal?.aborted === true
    let index = from
    for (;;) {
      while (index < this.entries.length) yield this.entries[index++]
      if (closed()) return
      await new Promise<void>(resolve => {
        const wake = (): void => { resolve() }
        this.waiters.push(wake)
        signal?.addEventListener('abort', wake, { once: true })
      })
      if (closed()) return
    }
  }

  /** `[0, upTo)` 区间内的 journal 事件条目（快照与 page 的 records 都是这个视图）。 */
  eventsUpTo(upTo: number): Record<string, unknown>[] {
    return this.entries.slice(0, upTo).filter(entry => entry['type'] === 'event')
  }

  /** 事件条目里的 `event.seq`（seq 在信封内层，不在条目外层——按外层取会全数漏掉）。 */
  static eventSeq(entry: Record<string, unknown>): number {
    return (entry['event'] as { seq: number }).seq
  }

  /** 开发期诊断：只读拷贝，不改变 `tail` 的游标语义。 */
  dump(): readonly Record<string, unknown>[] {
    return this.entries.slice()
  }
}

/** 一个会话在当前进程内的 journal 与回合状态。 */
interface Journal {
  /** 诊断用代次：同一 id 的 journal 被替换时能看出来（follow 捕获的是旧对象）。 */
  serial: number
  seq: number
  revision: number
  log: FrameLog
  createdAt: number
  /** DSH 的 turn = 一次用户任务；后端的 turn = 任务内一次采样迭代（= DSH 的 step）。 */
  turn: number
  step: number
  taskOpen: boolean
  text: string
  reasoning: string
  chunkIndex: number
  attemptOpen: boolean
  attemptSeq: number
  /**
   * 进行中尝试的重连基线（wire `SessionAssistantStreamBaseline.activeAttempt`）：
   * follow 在流式中途打开时，客户端必须拿这份紧凑快照重建已吐出的正文；只给
   * `revision` 会让中途 mounted 的视图把后续 chunk 当成「没有 start 的孤儿」丢掉，
   * 表现就是实时态只有用户气泡（B-20）。
   */
  attemptStream: { time: number; chunk: Record<string, unknown> }[]
  attemptTurn: number
  attemptStep: number
  attemptStartedAfterSeq: number
  /**
   * 本次尝试的稳定身份。它必须**不随帧变化**：start/chunk/end 三帧共用一个 attemptId，
   * 客户端才把它们认成同一段流（按 revision 生成会让每一帧都"换了一次尝试"）。
   */
  attemptId: string
  usage: Record<string, number> | undefined
  outputs: Map<string, { stdout: string[]; stderr: string[] }>
  /**
   * 档案条目下标 → 该条目回放出的最后一个 journal seq（SH-033 的重建产物）。
   * 空数组表示这个会话的 journal 全部来自当前进程观察，没有可对账的档案前缀。
   */
  entryToSeq: number[]
  /** 底部指标折叠状态（sessionStats/tokenUsage 的数据源），见 SessionMetrics 注记。 */
  metrics: SessionMetrics
  /** 本步计时的起点（step/start 时刻）；undefined = 本步还没开始计时（档案重建路径）。 */
  stepStartAt: number | undefined
  /** 本步首个非空 delta 的时刻；undefined = 本步还没吐出 token。 */
  firstDeltaAt: number | undefined
  /** 最近一个已经计过「回合数」的 turn；同 turn 的后续 step/end 不再重复计回合。 */
  countedTurn: number | undefined
  /** 进行中工具调用的起点（tool/call → tool/result 按 callId 配对结算）。 */
  toolStartAt: Map<string, number>
  /** 进行中工具调用的名字（callId → 工具名；plan 态推导与结算都要按名字认）。 */
  callTools: Map<string, string>
  /** 最近一次用量采样的 prompt 侧压力（contextPressure 投影的分子，不含输出）。 */
  pressure: { pressureTokens: number; projectedTokens: number } | undefined
  /** 本会话的待发队列（回合进行中的提交；按序补发）。 */
  queue: QueuedSubmission[]
  /**
   * `todos` 投影（上游 tool-todo 的 fold 语义）：todo/write 整表替换、turn/start 清空
   * （turn/end 保留已完成的清单可见）。undefined = 尚无写入或已被清空（投影缺席，
   * TodoDock 不渲染）；快照仅在非 undefined 时挂键。
   */
  todos: Record<string, unknown>[] | undefined
  /** 进行中工具调用的参数（callId → 解析后的 args；todo/write 推导与结算都要用）。 */
  callArgs: Map<string, unknown>
}

/**
 * `load_session` 的档案投影（与 `src-tauri/src/lib.rs` 的 `SessionEntry` 同形）。
 * 字段名跟着 Rust 的 serde 走：这里刻意**不**重命名，改名就会和后端序列化结果漂移。
 */
type SessionEntry =
  | { kind: 'message'; role: string; text: string; reasoning?: string }
  | { kind: 'tool_call'; call_id: string; tool: string; arguments: string; output?: string }

/**
 * 会话附件在 prompt 里的落地形状（SH-116 批 1）：宿主只暂存文件字节，模型侧
 * 以 `<attachment>` 文本块接收；文本按 128 KiB 窗口内联（截断标注），二进制只给
 * 占位。图片上传在 fetch 钩子处显式拒绝，这里的 image 分支只处理历史/内联形态。
 */
interface PromptAttachmentPart {
  readonly type: string
  readonly text?: string
  readonly receiptId?: string
  readonly name?: string
  readonly mediaType?: string
}

interface AttachmentReadWire {
  readonly name: string
  readonly mediaType: string
  readonly bytes: number
  readonly data: string
  readonly truncated: boolean
  readonly width?: number
  readonly height?: number
}

/** 持久图片引用：与 DSH `ImageAttachmentRef` 同形，随 user/message 内容落 journal。 */
interface PromptImageRef {
  readonly attachmentId: string
  readonly mediaType: string
  readonly bytes: number
  readonly width: number
  readonly height: number
  readonly name?: string
}

const IMAGE_MARKER_PREFIX = '<!-- shaniu-image:'

/**
 * 图片引用在**任务文本**里的隐藏标记：rollout 只存用户消息文本，没有附件字段；
 * 用一行 HTML 注释（渲染时不可见）把引用带过去，重开档案时解析回 image 内容块。
 */
function encodeImageMarker(image: PromptImageRef): string {
  return `${IMAGE_MARKER_PREFIX}${encodeURIComponent(JSON.stringify(image))} -->`
}

/** 从档案用户文本里剥离图片标记；返回干净正文与可重建的图片引用。 */
function decodeImageMarkers(text: string): { text: string; images: PromptImageRef[] } {
  const images: PromptImageRef[] = []
  const kept: string[] = []
  for (const line of text.split('\n')) {
    if (!line.startsWith(IMAGE_MARKER_PREFIX) || !line.endsWith(' -->')) {
      kept.push(line)
      continue
    }
    try {
      const raw = JSON.parse(decodeURIComponent(line.slice(IMAGE_MARKER_PREFIX.length, -4))) as Partial<PromptImageRef>
      if (typeof raw.attachmentId === 'string' && typeof raw.mediaType === 'string'
        && typeof raw.bytes === 'number' && typeof raw.width === 'number' && typeof raw.height === 'number') {
        images.push({
          attachmentId: raw.attachmentId,
          mediaType: raw.mediaType,
          bytes: raw.bytes,
          width: raw.width,
          height: raw.height,
          ...(typeof raw.name === 'string' ? { name: raw.name } : {}),
        })
        continue
      }
    } catch {
      // 坏标记当普通文本保留：不因一条注释丢掉用户正文
    }
    kept.push(line)
  }
  return { text: kept.join('\n'), images }
}

const TEXTUAL_MEDIA = /^(?:text\/)|^(?:application\/(?:json|xml|x-yaml|yaml|toml|sql|x-sh|x-httpd-php))/i
const TEXTUAL_EXT = /\.(?:txt|md|markdown|json|jsonl|toml|yaml|yml|rs|ts|tsx|js|jsx|mjs|cjs|py|go|java|kt|c|cc|cpp|h|hpp|cs|css|html|htm|xml|sh|bash|ps1|sql|csv|tsv|log|ini|cfg|conf)$/i
const ATTACHMENT_READ_BYTES = 128 * 1024

function markdownFence(body: string): string {
  let longest = 0
  for (const match of body.matchAll(/`+/g)) longest = Math.max(longest, match[0].length)
  return '`'.repeat(Math.max(3, longest + 1))
}

function base64ToText(data: string): string {
  const binary = atob(data)
  const bytes = Uint8Array.from(binary, char => char.charCodeAt(0))
  return new TextDecoder().decode(bytes)
}

/**
 * 附件在任务文本里的展示形态：Markdown 标题 + 代码栅栏。模型能明确看到文件名/
 * 类型/字节数；档案回放时用户气泡也不至于露出原始 XML 标签（批 1 的已知偏差）。
 */
function attachmentBlock(name: string, mediaType: string, bytes: number, body?: string, truncated = false): string {
  const safeName = name.replaceAll('`', "'").replaceAll('\n', ' ')
  const head = `**附件 \`${safeName}\`**（${mediaType}，${bytes} 字节${truncated ? '，已截断' : ''}）`
  if (body === undefined) return `${head}（未内联字节）`
  const fence = markdownFence(body)
  return `${head}\n${fence}text\n${body}\n${fence}`
}

/**
 * 把 DSH prompt 的 content parts 拼成交给宿主的文本：文件回执经 `read_attachment`
 * 读回，文本内联、二进制占位；无法解析的形态显式失败，不静默丢附件。
 */
async function composePromptText(
  bridge: HostBridge,
  sessionId: string | undefined,
  content: readonly PromptAttachmentPart[] | undefined,
): Promise<{ ok: true; text: string; displayText: string; images: PromptImageRef[] } | { ok: false; error: string }> {
  let text = ''
  let displayText = ''
  let afterAttachment = false
  const images: PromptImageRef[] = []
  const append = (block: string): void => {
    afterAttachment = true
    text += text === '' ? block : `\n\n${block}`
  }
  for (const part of content ?? []) {
    if (part.type === 'text') {
      const chunk = part.text ?? ''
      if (afterAttachment && chunk !== '') text += '\n\n'
      afterAttachment = false
      text += chunk
      displayText += chunk
      continue
    }
    if (part.type === 'image') {
      // 内联图片（无回执）没有 durable id，只能给占位，不能进档案重开。
      append(attachmentBlock(part.name ?? 'image', part.mediaType ?? 'image/*', 0))
      continue
    }
    if (part.type !== 'file') {
      return { ok: false, error: `不支持的附件类型：${part.type}` }
    }
    if (sessionId === undefined) return { ok: false, error: '附件需要先创建会话再发送' }
    if (part.receiptId === undefined) {
      return { ok: false, error: '附件尚未上传完成：缺少回执 id' }
    }
    let read: AttachmentReadWire
    try {
      read = await bridge.invoke<AttachmentReadWire>('read_attachment', {
        sessionId,
        receiptId: part.receiptId,
        maxBytes: ATTACHMENT_READ_BYTES,
      })
    } catch (reason) {
      const message = reason instanceof Error ? reason.message : String(reason)
      return { ok: false, error: `读取附件失败：${message}` }
    }
    if (read.mediaType.startsWith('image/')) {
      if (read.width === undefined || read.height === undefined || read.width <= 0 || read.height <= 0) {
        return { ok: false, error: '图片附件缺少有效尺寸，拒绝伪造图片引用' }
      }
      const image: PromptImageRef = {
        attachmentId: part.receiptId,
        mediaType: read.mediaType,
        bytes: read.bytes,
        width: read.width,
        height: read.height,
        name: read.name,
      }
      images.push(image)
      append(`${attachmentBlock(read.name, read.mediaType, read.bytes)}\n${encodeImageMarker(image)}`)
    } else if (TEXTUAL_MEDIA.test(read.mediaType) || TEXTUAL_EXT.test(read.name)) {
      append(attachmentBlock(read.name, read.mediaType, read.bytes, base64ToText(read.data), read.truncated))
    } else {
      append(attachmentBlock(read.name, read.mediaType, read.bytes))
    }
  }
  return { ok: true, text, displayText, images }
}

export interface AdapterOptions {
  readonly bridge: HostBridge
  readonly now?: () => number
  /**
   * 取某个会话在客户端侧的 Agent 作用域 Context。
   *
   * typert 的 waterfall 监听器是靠 `this` 认会话的：DSH 审批卡的第一行就是
   * `ctx.sessions.scopeOf(this)`，拿不到 sessionId 就直接 `next()` 放行（见
   * `client/ui-approval/src/client/index.ts:45`）。适配层自己派发事件，就必须自己
   * 把这个 `this` 补上——裸调 listener 会让审批卡根本不出现，而后端只看到一个
   * 没人裁决的请求，最终按拒绝收口。
   */
  readonly approvalScope?: (sessionId: string) => unknown
}

let journalSerial = 0

function newJournal(createdAt: number): Journal {
  journalSerial += 1
  return {
    serial: journalSerial,
    seq: -1, revision: 0, log: new FrameLog(), createdAt,
    turn: 0, step: 0, taskOpen: false, text: '', reasoning: '', chunkIndex: 0,
    attemptOpen: false, attemptSeq: -1, attemptId: '',
    attemptStream: [], attemptTurn: 0, attemptStep: 0, attemptStartedAfterSeq: -1,
    usage: undefined, outputs: new Map(),
    entryToSeq: [],
    metrics: zeroMetrics(),
    stepStartAt: undefined, firstDeltaAt: undefined, countedTurn: undefined,
    toolStartAt: new Map(), callTools: new Map(), pressure: undefined,
    queue: [],
    todos: undefined, callArgs: new Map(),
  }
}

function unimplemented(name: string, milestone: string): Promise<never> {
  return Promise.reject(new RemoteError('gateway/internal', `适配层未接线：remote.${name}（见 docs/05 ${milestone}）`, {}))
}

export function createAdapter(options: AdapterOptions): { remote: unknown; dispose(): void } {
  const { bridge } = options
  const approvalScope = options.approvalScope
  const now = options.now ?? ((): number => Date.now())
  const journals = new Map<string, Journal>()
  const titles = new Map<string, string>()
  /**
   * 审批监听器（DSH 的 ui-approval 在装配时注册）。`this` 由 typert 派发时绑定，
   * 所以这里保留函数本体、由调用点用 `.call(scope, …)` 触发。
   */
  const approvalListeners: ((
    this: unknown, request: Record<string, unknown>, next: () => Promise<never>,
  ) => Promise<string>)[] = []
  /**
   * 数据面事件的监听器（`settings/document-updated`、`llm/adapters-updated`、
   * `credentials/*`）。DSH 的席位与设置页都靠这些通知重读目录，收不到通知的形态是
   * "设置里改了服务商，席位还显示上一个"——那不是报错，是更难发现的错。
   */
  const listeners = new Map<string, ((...args: unknown[]) => unknown)[]>()
  const projectionLog = new FrameLog()
  let projectionSeq = 0
  /** 开发期 follow 生命周期取证（进入/快照/结束）；真机排查"视图没接上 journal"用。 */
  const followTrace: Record<string, unknown>[] = []
  const traceFollow = (sessionId: string | undefined, phase: string, extra: Record<string, unknown> = {}): void => {
    if (!import.meta.env.DEV) return
    followTrace.push({ at: now(), sessionId, phase, ...extra })
    if (followTrace.length > 50) followTrace.shift()
  }
  /**
   * 计划态（上游 `plan` 投影的数据源）。宿主 PlanState 是进程级全局（app_state.rs 的
   * Arc<PlanState>，跨任务/跨会话持久），这里镜像它：模型调 enter_plan_mode 执行成功
   * 置真、exit_plan_mode 执行成功置假。唯一已知缺口：适配层重启丢状态而宿主持久——
   * 宿主没有"读当前计划态"的命令面，如实声明（docs/05 SH-105）。
   */
  let planActive = false
  /** 排队消息的 id 序列（inbox 行身份）。 */
  let queueSeq = 0

  /**
   * 子代理泳道登记表（SH-109）。后端没有子会话——子代理是 agent_loop task，事件经
   * `AgentEvent` 信封（`agent_id ≠ 'root'`，`session_id` 仍是宿主任务会话）到达；
   * 适配层把 `agent_id` 合成为泳道的 childSessionId（ULID 全局唯一，`laneIdOf`），
   * 每个泳道一份独立 journal，父 journal 落 `subagent/catalog` 事件。
   *
   * 懒建口径：见到该 agent 的第一条事件才登记——"transport 亲眼见过它在干活"，
   * catalog 事件写 v1 `mode:'unknown'`（上游 v3 不允许 unknown，诚实标注"没拿到
   * spawn 元数据"；元数据要等 SH-053 的 `SubagentSpawned` 消息，届时升级 v3
   * continuable）。泳道行在侧栏/目录树的出现由 session-controller 的 projectList
   * 消费 `subagentCatalog` 投影自动完成，UI 零改动。
   */
  interface AgentLane {
    readonly parentId: string
    /** transport 首次见到该 agent 的时刻（懒建时刻，不是后端 spawn 时刻——如实标注）。 */
    readonly createdAt: number
    /** 泳道 timing 折叠（subagentTiming 投影的数据源，语义照上游 projection.ts）。 */
    timing: SubagentTimingState
  }
  const agents = new Map<string, AgentLane>()

  const dispatch = (name: string, ...args: unknown[]): void => {
    for (const listener of listeners.get(name) ?? []) {
      try {
        void listener(...args)
      } catch (reason) {
        // 监听器抛错不能把已提交的写回滚掉——DSH 自己的事件面同样是"包含式派发"。
        console.error(`[SHANIU] ${name} 监听器抛错`, reason)
      }
    }
  }

  /** 设置页 / 凭据 / 模型目录三张面：事实来源是宿主的 `[[providers]]` 文档。 */
  const face = createSettingsFace(bridge, { emit: dispatch })

  const journalOf = (sessionId: string): Journal => {
    let journal = journals.get(sessionId)
    if (journal === undefined) {
      journal = newJournal(now())
      journals.set(sessionId, journal)
    }
    return journal
  }

  const pushEvent = (journal: Journal, type: string, data: unknown, extra: Record<string, unknown> = {}): number => {
    journal.seq += 1
    journal.log.append({ type: 'event', event: { type, seq: journal.seq, time: now(), ...extra, data } })
    return journal.seq
  }

  const pushStreamFrame = (journal: Journal, frame: Record<string, unknown>): void => {
    journal.revision += 1
    journal.log.append({
      type: 'assistant-stream',
      frame: { ...frame, attemptId: journal.attemptId, revision: journal.revision },
    })
  }

  const pushTitle = (sessionId: string, title: string): void => {
    titles.set(sessionId, title)
    projectionSeq += 1
    projectionLog.append({ type: 'projection', sessionId, seq: projectionSeq, changes: [{ key: 'title', val: title }] })
  }

  const currentModelSelection = (): Record<string, unknown> => ({
    lastUsed: null,
    next: face.activeSelection(),
  })

  /**
   * `permissions` 投影（{currentValue}）：挡位全局共享，所有会话读同一份宿主沙箱档。
   * 快照里给不出映射（快照读失败回落 read_only）也照实给——UI 的芯片要有一个确定态。
   * 反推复用 `./permission-presets` 的同一张表，与设置页 `permission` 域的读视图同源。
   */
  const currentPermission = (): string => presetOfSandbox(face.sandboxMode())
  const permissionsProjection = (): Record<string, unknown> => ({ currentValue: currentPermission() })

  /** 挡位切换成功后向所有已知会话广播一帧（值来自刚刷新的 face，照 selectModel 的广播方式）。 */
  const pushPermissionToAll = (): void => {
    projectionSeq += 1
    for (const id of journals.keys()) {
      projectionLog.append({
        type: 'projection', sessionId: id, seq: projectionSeq,
        changes: [{ key: 'permissions', val: permissionsProjection() }],
      })
    }
  }

  /**
   * 底部指标投影：没跑出内容的会话不挂键，界面据此隐藏 pill/仪表而不是显示一套零值
   * （照 mock 的既定口径）。tokenUsage 只在真的烧过 token 后挂；contextPressure 还
   * 需要生效模型给得出上下文窗口——没有分母就不挂，占用仪表按数据缺席隐藏。
   */
  const metricsProjectionsOf = (journal: Journal): Record<string, unknown> => {
    const out: Record<string, unknown> = {}
    const m = journal.metrics
    if (m.steps > 0 || m.turns > 0) {
      out.sessionStats = {
        turns: m.turns, steps: m.steps, llmMs: m.llmMs, toolMs: m.toolMs,
        ttftMs: m.ttftMs, ttftSteps: m.ttftSteps, decodeMs: m.decodeMs, decodeTokens: m.decodeTokens,
      }
    }
    const b = m.buckets
    if (b.uncachedInputTokens > 0 || b.outputTokens > 0 || b.cacheReadTokens > 0 || b.cacheWriteTokens > 0) {
      out.tokenUsage = { ...b }
    }
    if (journal.pressure !== undefined) {
      const contextWindow = face.activeContextWindow()
      if (contextWindow !== undefined) {
        out.contextPressure = { ...journal.pressure, contextWindow }
      }
    }
    return out
  }

  /** 指标变化的结算点统一从这里推帧；没有合格的键就不推（不伪造一帧空投影）。 */
  const pushMetricsFrame = (sessionId: string, journal: Journal): void => {
    const changes = Object.entries(metricsProjectionsOf(journal)).map(([key, val]) => ({ key, val }))
    if (changes.length === 0) return
    projectionSeq += 1
    projectionLog.append({ type: 'projection', sessionId, seq: projectionSeq, changes })
  }

  // ---- 子代理泳道（SH-109）：懒建、catalog/timing 投影 ----

  /** 父会话的 `subagentCatalog` 投影值：没有登记泳道就缺席（不伪造空目录）。 */
  const catalogProjectionOf = (parentId: string): SubagentCatalogEntry[] | undefined => {
    const entries: SubagentCatalogEntry[] = []
    for (const [laneId, lane] of agents) {
      if (lane.parentId !== parentId) continue
      entries.push(catalogEntryOf({
        version: 1, childId: laneIdOf(laneId), childCreatedAt: lane.createdAt, mode: 'unknown',
      }))
    }
    return entries.length === 0 ? undefined : entries
  }

  /** catalog 变化的统一推帧点（父会话的投影值整表替换，上游 catalog fold 的 wire 语义）。 */
  const pushCatalogFrame = (parentId: string): void => {
    const catalog = catalogProjectionOf(parentId)
    if (catalog === undefined) return
    projectionSeq += 1
    projectionLog.append({
      type: 'projection', sessionId: parentId, seq: projectionSeq,
      changes: [{ key: 'subagentCatalog', val: catalog }],
    })
  }

  /**
   * 懒建泳道：该 agent 的首条事件到达时登记 + 父 journal 落 `subagent/catalog`
   * 事件（回放可重建的持久事实）+ 父投影推一帧（侧栏/目录树立即出现该行）。
   */
  const ensureLane = (parentId: string, agentId: string): void => {
    if (agents.has(agentId)) return
    agents.set(agentId, { parentId, createdAt: now(), timing: initTimingState() })
    pushEvent(journalOf(parentId), 'subagent/catalog', {
      version: 1, childId: laneIdOf(agentId), childCreatedAt: now(), mode: 'unknown',
    })
    pushCatalogFrame(parentId)
  }

  /**
   * 泳道 timing 推进。turn 开合时推投影帧；其余事件只折叠不推帧——运行中泳道的
   * 时长由 UI 实时钟读（`activityDuration` 对 running 泳道取 now），`active.through`
   * 只在"回合中断在半路"的收口展示里用到，逐 delta 推帧是纯浪费。
   */
  const foldLaneTiming = (laneId: string, eventType: 'turn/start' | 'turn/end' | 'advance', reasonKind?: string): void => {
    const lane = agents.get(laneId)
    if (lane === undefined) return
    lane.timing = foldTimingEvent(lane.timing, eventType, now(), reasonKind)
    if (eventType === 'advance') return
    const timing = timingProjectionOf(lane.timing)
    if (timing === undefined) return
    projectionSeq += 1
    projectionLog.append({
      type: 'projection', sessionId: laneId, seq: projectionSeq,
      changes: [{ key: 'subagentTiming', val: timing }],
    })
  }

  /**
   * 快照/基线里的泳道位：泳道挂 `subagentTiming`（未见回合则缺席），父会话挂
   * `subagentCatalog`（无登记泳道则缺席）。投影缺席 = UI 按无数据判读，不伪造。
   */
  const subagentProjectionsOf = (sessionId: string): Record<string, unknown> => {
    const lane = agents.get(sessionId)
    if (lane !== undefined) {
      const timing = timingProjectionOf(lane.timing)
      return timing === undefined ? {} : { subagentTiming: timing }
    }
    const catalog = catalogProjectionOf(sessionId)
    return catalog === undefined ? {} : { subagentCatalog: catalog }
  }

  // ---- 底部指标的折叠动作（字段语义照上游 session-stats；时间取事件到达时刻）----

  /** 一次采样迭代开始计时（实时路径才有：档案重建没有可信时刻，不伪造时间）。 */
  const beginStepMetrics = (journal: Journal): void => {
    journal.stepStartAt = now()
    journal.firstDeltaAt = undefined
  }

  /** 一步收口：计步 + 给该回合计一次回合数（上游口径：有闭合 step 的回合才计）。 */
  const closeStepMetrics = (journal: Journal): void => {
    journal.metrics.steps += 1
    if (journal.countedTurn !== journal.turn) {
      journal.metrics.turns += 1
      journal.countedTurn = journal.turn
    }
  }

  /**
   * 一条助手消息落定：结算本步的模型墙钟（step/start → 落定）、首 token 延迟与
   * 解码段（首 delta → 落定，decodeTokens 取最近一次采样的输出桶）。结算后清掉
   * 计时光标——同一步若再落一条消息，各段独立测量、总量保持可加。
   */
  const settleAssistantMetrics = (journal: Journal): void => {
    const settledAt = now()
    const m = journal.metrics
    const stepStart = journal.stepStartAt
    const firstDelta = journal.firstDeltaAt
    if (stepStart !== undefined) {
      m.llmMs += Math.max(0, settledAt - stepStart)
      journal.stepStartAt = undefined
    }
    if (firstDelta !== undefined) {
      m.ttftSteps += 1
      if (stepStart !== undefined) m.ttftMs += Math.max(0, firstDelta - stepStart)
      m.decodeMs += Math.max(0, settledAt - firstDelta)
      m.decodeTokens += journal.usage?.['outputTokens'] ?? 0
      journal.firstDeltaAt = undefined
    }
  }

  /** 一次工具调用结算（callId 配对；没配上对的 begin（被中断）自然漏算，不伪造时长）。 */
  const settleToolMetrics = (journal: Journal, callId: string): void => {
    const startedAt = journal.toolStartAt.get(callId)
    journal.toolStartAt.delete(callId)
    if (startedAt !== undefined) journal.metrics.toolMs += Math.max(0, now() - startedAt)
  }

  // ---- 计划态投影（上游 plan = {active, pending}；pending 是命令在途位，宿主没有
  // /plan 命令面所以恒 false）----

  /** inactive 时返回 undefined：投影键缺席 = 无芯片（PlanChip 与 plan 占位都按此判据）。 */
  const planProjection = (): Record<string, unknown> | undefined =>
    planActive ? { active: true, pending: false } : undefined

  /** 快照/基线里的 plan 位：仅计划态挂键（inactive 缺席，UI 回到无芯片态）。 */
  const planProjectionValues = (): Record<string, unknown> =>
    planActive ? { plan: { active: true, pending: false } } : {}

  const pushPlanToAll = (): void => {
    projectionSeq += 1
    for (const id of journals.keys()) {
      projectionLog.append({
        type: 'projection', sessionId: id, seq: projectionSeq,
        changes: [{ key: 'plan', val: planProjection() ?? { active: false, pending: false } }],
      })
    }
  }

  const setPlanActive = (active: boolean): void => {
    if (planActive === active) return
    planActive = active
    pushPlanToAll()
  }

  // ---- todos 投影（上游 tool-todo 的 journal 事件与 fold 语义，SH-108）----

  /** `todo_write` 的合法 status（上游 STATUSES）。 */
  const TODO_STATUSES = new Set(['pending', 'in_progress', 'completed'])

  /**
   * `todo_write` 参数 → 整表清单；不可信输入一律返回 undefined（不 emit、不伪造）。
   *
   * 这是后端 `tools/todo.rs::validate` 的 TS 镜像（同一组纯谓词）：trim 后非空、
   * trim 后判重、status 三枚举、条目只接受 content/status 两键。实时路径从执行
   * 结果（result=ok）取信，回放路径拿不到执行位，只能用同一组谓词重演后端裁决——
   * 校验被拒的调用留不下 todo/write，镜像谓词保证两条路径逐项相等。
   * 部署口径 ALLOW_PARALLEL_IN_PROGRESS=true（对齐上游 base bundle），故无单活跃检查。
   */
  const parseTodos = (args: unknown): Record<string, unknown>[] | undefined => {
    if (typeof args !== 'object' || args === null) return undefined
    const raw = (args as { todos?: unknown }).todos
    if (!Array.isArray(raw)) return undefined
    const seen = new Set<string>()
    const todos: Record<string, unknown>[] = []
    for (const item of raw) {
      if (typeof item !== 'object' || item === null || Array.isArray(item)) return undefined
      const record = item as Record<string, unknown>
      const keys = Object.keys(record)
      if (keys.length !== 2 || !keys.includes('content') || !keys.includes('status')) return undefined
      const content = record['content']
      const status = record['status']
      if (typeof content !== 'string' || typeof status !== 'string') return undefined
      const trimmed = content.trim()
      if (trimmed === '' || !TODO_STATUSES.has(status) || seen.has(trimmed)) return undefined
      seen.add(trimmed)
      todos.push({ content: trimmed, status })
    }
    return todos
  }

  /** todos 投影变化统一从这里推帧；无值时交显式 null（客户端据此清掉旧清单）。 */
  const pushTodosFrame = (sessionId: string, journal: Journal): void => {
    projectionSeq += 1
    projectionLog.append({
      type: 'projection', sessionId, seq: projectionSeq,
      changes: [{ key: 'todos', val: journal.todos ?? null }],
    })
  }

  /**
   * 开回合 = 上游 fold 的 turn/start 清空位：上一回合的清单收起（turn/end 不清，
   * 完成的清单在回合结束前保持可见）。仅当确有旧值时推 null 帧——没有写入过的
   * 会话不需要一帧「本来就是空」的投影。
   */
  const pushTurnStart = (sessionId: string | undefined, journal: Journal): void => {
    pushEvent(journal, 'turn/start', { turn: journal.turn })
    if (journal.todos !== undefined) {
      journal.todos = undefined
      if (sessionId !== undefined) pushTodosFrame(sessionId, journal)
    }
  }

  /** 快照/基线里的 todos 位：仅已有清单时挂键（投影缺席 = TodoDock 不渲染，不伪造）。 */
  const todosProjectionValues = (journal: Journal): Record<string, unknown> =>
    journal.todos !== undefined ? { todos: journal.todos } : {}

  // ---- 客户端排队（运行中提交 → inbox 投影行 → 回合收口按序自动补发）----

  const queueRowOf = (item: QueuedSubmission): Record<string, unknown> => ({
    id: item.id,
    role: 'user',
    content: [{ type: 'text', text: item.text }],
    source: { kind: 'user', rpcId: item.requestId },
  })

  /** inbox 投影只带 next-turn（宿主没有 next-step 注入面）；空队列交空数组以清除旧行。 */
  const pushInboxFrame = (sessionId: string, journal: Journal): void => {
    projectionSeq += 1
    projectionLog.append({
      type: 'projection', sessionId, seq: projectionSeq,
      changes: [{ key: 'inbox', val: { 'next-turn': journal.queue.map(queueRowOf) } }],
    })
  }

  /** 把一条排队消息真正发出去并开回合（与 prompt 同一事件序列；rpcId 回填原始请求身份）。 */
  const launchQueuedTurn = (sessionId: string, journal: Journal, item: QueuedSubmission): void => {
    const shown = item.displayText ?? item.text
    if (!titles.has(sessionId)) pushTitle(sessionId, shown.trim().slice(0, 12) || '新会话')
    journal.queue = journal.queue.filter(candidate => candidate.id !== item.id)
    pushInboxFrame(sessionId, journal)
    if (!journal.taskOpen) openTask(journal)
    pushTurnStart(sessionId, journal)
    pushEvent(journal, 'step/start', { turn: journal.turn, step: journal.step })
    beginStepMetrics(journal)
    pushEvent(journal, 'user/message', {
      id: `shaniu-user-${journal.seq + 1}`,
      role: 'user',
      content: [
        ...(item.images ?? []).map(image => ({ type: 'image', attachment: image })),
        ...(shown === '' ? [] : [{ type: 'text', text: shown }]),
      ],
      source: { kind: 'user', rpcId: item.requestId },
    }, { surfaceOp: 'append' })
    dispatch('api-session/status', sessionId, true)
  }

  const delay = (ms: number): Promise<void> => new Promise(resolve => setTimeout(resolve, ms))

  /**
   * 回合收口后按序补发排队消息。单任务闸是进程级的，且释放时机在任务协程收口之后
   * （turn_completed 先于 end_task 到达）：补发对短时拒绝做有限重试；闸被其他会话的
   * 任务占着就留给下一次收口再试——每个任务都会发 turn_completed，队列不会饿死。
   * 先领取后发送：补发在途时用户移除该行会得到"可能已经开始发送"的如实拒绝。
   */
  const drainQueues = async (): Promise<void> => {
    for (const [sessionId, journal] of journals.entries()) {
      const item = journal.queue[0]
      if (item === undefined || journal.taskOpen) continue
      journal.queue = journal.queue.filter(candidate => candidate.id !== item.id)
      let sent = false
      for (let attempt = 0; attempt < 5 && !sent; attempt += 1) {
        try {
          await bridge.invoke<string>('send_task', { task: item.text, agentId: sessionId })
          sent = true
        } catch {
          await delay(250)
        }
      }
      if (sent) {
        launchQueuedTurn(sessionId, journal, item)
        return
      }
      // 五次仍被拒：闸被别的任务占着。退回队首，等下一个 turn_completed 再试。
      journal.queue = [item, ...journal.queue.filter(candidate => candidate.id !== item.id)]
      pushInboxFrame(sessionId, journal)
      return
    }
  }

  /** 空会话的快照帧：cursor -1、无记录、revision 0，且仍带上客户端要求的两个基线。 */
  const emptySnapshot = (id: string, createdAt: number): Record<string, unknown> => ({
    type: 'snapshot',
    header: { version: 0, id, createdAt, isSeeded: false },
    cursor: -1,
    records: [],
    hasMore: false,
    projections: {
      asOfSeq: 0,
      values: {
        modelSelection: currentModelSelection(), title: null, permissions: permissionsProjection(),
        ...(planProjectionValues()),
      },
    },
    assistantStream: { revision: 0 },
  })

  /** 打开一次助手流：startedAfterSeq 是它将替换的那条消息的 seq。 */
  const ensureAttempt = (journal: Journal): void => {
    if (journal.attemptOpen) return
    journal.attemptOpen = true
    journal.chunkIndex = 0
    journal.attemptSeq = journal.seq
    journal.attemptId = `shaniu-attempt-${journal.turn}-${journal.step}-${journal.revision + 1}`
    journal.attemptTurn = journal.turn
    journal.attemptStep = journal.step
    journal.attemptStartedAfterSeq = journal.attemptSeq
    journal.attemptStream = []
    pushStreamFrame(journal, { type: 'start', startedAfterSeq: journal.attemptSeq, turn: journal.turn, step: journal.step })
  }

  /**
   * 追加一帧助手流（start 后才会到这里）：同一份 `{time, chunk}` 同时进 journal 流帧
   * 与重连基线，快照重建出的前缀与后续帧的 index 才能严格接上。
   */
  const pushAssistantChunk = (journal: Journal, chunk: Record<string, unknown>): void => {
    ensureAttempt(journal)
    const time = now()
    journal.attemptStream.push({ time, chunk })
    pushStreamFrame(journal, { type: 'chunk', index: journal.chunkIndex, time, chunk })
    journal.chunkIndex += 1
  }

  /**
   * 收口一次助手流：有正文或思考才落 `assistant/message`（空回合不落假消息）。
   * 帧的 end 必须引用刚落的那条事件的 seq，客户端据此把流式气泡换成定稿卡片；
   * 没有落事件时 outcome 只能是 abandoned，否则 end 会指向一个不存在的事件。
   * `interrupted` 只在中断路径上打标：半截正文是真实发生过的输出，不能假装没说过，
   * 但也不能当成正常答完——两者在界面上是两种状态。
   */
  const closeAttempt = (sessionId: string, journal: Journal, committed: boolean, interrupted = false): void => {
    if (!journal.attemptOpen) return
    journal.attemptOpen = false
    const hasBody = journal.text.length > 0 || journal.reasoning.length > 0
    let seq = journal.attemptSeq
    if (hasBody) {
      const content: Record<string, unknown>[] = []
      if (journal.reasoning.length > 0) content.push({ type: 'reasoning', text: journal.reasoning })
      if (journal.text.length > 0) content.push({ type: 'text', text: journal.text })
      seq = pushEvent(journal, 'assistant/message', {
        turn: journal.turn,
        step: journal.step,
        message: {
          id: `shaniu-assistant-${journal.seq + 1}`,
          role: 'assistant',
          content,
          source: { kind: 'model', ...face.activeSelection() },
        },
        stream: [],
        ...(journal.usage === undefined ? {} : { usage: journal.usage }),
        ...(interrupted ? { interrupted: true } : {}),
      }, { surfaceOp: 'append' })
      // 消息落定即一次模型墙钟结算；工具调用前的定稿（tool_call_begin 路径）同此口径。
      settleAssistantMetrics(journal)
      pushMetricsFrame(sessionId, journal)
    }
    pushStreamFrame(journal, {
      type: 'end',
      index: journal.chunkIndex,
      outcome: committed && hasBody
        ? { kind: 'committed', eventType: 'assistant/message', seq }
        : { kind: 'abandoned' },
    })
    journal.attemptStream = []
    journal.text = ''
    journal.reasoning = ''
  }

  const openTask = (journal: Journal): void => {
    journal.turn += 1
    journal.step = 0
    journal.taskOpen = true
  }

  const closeTask = (sessionId: string, journal: Journal, reason: Record<string, unknown>, interrupted: boolean): void => {
    closeAttempt(sessionId, journal, true, interrupted)
    if (!journal.taskOpen) return
    pushEvent(journal, 'step/end', { turn: journal.turn, step: journal.step })
    closeStepMetrics(journal)
    pushEvent(journal, 'turn/end', { turn: journal.turn, reason })
    journal.taskOpen = false
    pushMetricsFrame(sessionId, journal)
  }

  /**
   * 档案历史 → journal 事件重建（SH-033 的"打开旧会话"）。
   *
   * 档案里没有 DSH 的回合边界——它记的是模型侧条目序列。这里按与实时路径同构的规则
   * 重建：一条用户消息开一个回合（一次用户任务），一条 assistant 消息开一个 step
   * （一次采样迭代）。不这么重建的话，重开的会话里每条消息都没有归属，回合头与
   * 「用时 N 秒」都无处可算。
   *
   * 两处刻意不伪造：
   * - 用量：档案没有回合级 usage 汇总，缺就不写 `usage` 字段，不回填一个看起来
   *   合理的数——那会让用户拿它当真实账单。
   * - 终态：档案里的工具输出**已经**带着后端写的 `[exit code: N]` 尾标记，这里只
   *   读它、不再追加一次。
   */
  const replayArchive = (entries: readonly SessionEntry[]): Journal => {
    const journal = newJournal(now())
    let stepOpen = false
    /**
     * 当前 step 里是否已经落过助手消息。实时路径里"正文 + 它的工具调用 + 结果"属于
     * **同一次采样迭代**，只有下一次迭代才换 step；重建时若每条助手消息都新开 step，
     * 同一个档案回合会被拆成两套步序，回合头与「用时 N 秒」的分组就和实时跑出来的不一样。
     */
    let stepHasAssistant = false
    const openStep = (step: number): void => {
      pushEvent(journal, 'step/start', { turn: journal.turn, step })
      stepOpen = true
      stepHasAssistant = false
    }
    const closeStep = (): void => {
      if (!stepOpen) return
      pushEvent(journal, 'step/end', { turn: journal.turn, step: journal.step })
      // 重建只有计数、没有可信时刻：步/回合数如实折叠，墙钟一律不计。
      closeStepMetrics(journal)
      stepOpen = false
    }
    const closeTurn = (): void => {
      closeStep()
      if (!journal.taskOpen) return
      pushEvent(journal, 'turn/end', { turn: journal.turn, reason: { kind: 'completed' } })
      journal.taskOpen = false
    }
    const startTurn = (): void => {
      closeTurn()
      journal.turn += 1
      journal.step = 0
      journal.taskOpen = true
      // 档案重建没有投影流可推，只折叠状态：最终快照带出的就是 fold 到尾的值
      pushTurnStart(undefined, journal)
    }

    entries.forEach((entry, index) => {
      if (entry.kind === 'message' && entry.role === 'user') {
        startTurn()
        openStep(0)
        const parsed = decodeImageMarkers(entry.text)
        pushEvent(journal, 'user/message', {
          id: `shaniu-replay-user-${index}`,
          role: 'user',
          content: [
            ...parsed.images.map(image => ({ type: 'image', attachment: image })),
            ...(parsed.text === '' ? [] : [{ type: 'text', text: parsed.text }]),
          ],
          source: { kind: 'user' },
        }, { surfaceOp: 'append' })
      } else if (entry.kind === 'message') {
        // 档案开头就是助手消息（压缩锚点之类）：也得给它一个回合，否则这条消息
        // 在界面上没有归属，会被静默丢掉。
        if (!journal.taskOpen) startTurn()
        if (!stepOpen) {
          journal.step += 1
          openStep(journal.step)
        } else if (stepHasAssistant) {
          // 同一个 step 里已经有过一次助手输出 → 这是下一次采样迭代，换 step
          closeStep()
          journal.step += 1
          openStep(journal.step)
        }
        stepHasAssistant = true
        const content: Record<string, unknown>[] = []
        if (entry.reasoning !== undefined && entry.reasoning !== '') content.push({ type: 'reasoning', text: entry.reasoning })
        content.push({ type: 'text', text: entry.text })
        pushEvent(journal, 'assistant/message', {
          turn: journal.turn,
          step: journal.step,
          message: {
            id: `shaniu-replay-assistant-${index}`,
            role: 'assistant',
            content,
            source: { kind: 'model', ...face.activeSelection() },
          },
          stream: [],
        }, { surfaceOp: 'append' })
      } else {
        if (!journal.taskOpen) startTurn()
        if (!stepOpen) { journal.step += 1; openStep(journal.step) }
        pushEvent(journal, 'tool/call', {
          turn: journal.turn,
          step: journal.step,
          callId: entry.call_id,
          name: entry.tool,
          arguments: entry.arguments,
        })
        // todo/write 重建（SH-108）：档案没有独立的 journal 事件，事实源就是这条
        // 工具调用记录本身。推导规则与实时路径同构——调用留下了结果、参数通过
        // parseTodos 的同一组谓词（后端校验被拒的调用只留错误文本，重演谓词即得
        // 同一裁决），才补一条 todo/write；位置在上游序列的 tool/call 与 tool/result 之间。
        if (entry.tool === 'todo_write' && entry.output !== undefined) {
          let args: unknown
          try {
            args = JSON.parse(entry.arguments)
          } catch {
            args = undefined
          }
          const todos = parseTodos(args)
          if (todos !== undefined) {
            journal.todos = todos
            pushEvent(journal, 'todo/write', { todos })
          }
        }
        const exited = entry.output !== undefined
          ? /\[exit code: (-?\d+)\]\s*$/.exec(entry.output)?.[1]
          : undefined
        const failed = entry.output === undefined || (exited !== undefined && exited !== '0')
        pushEvent(journal, 'tool/result', {
          turn: journal.turn,
          step: journal.step,
          message: {
            id: `shaniu-replay-result-${index}`,
            role: 'tool',
            source: { kind: 'tool', callId: entry.call_id },
            toolCallId: entry.call_id,
            content: entry.output === undefined || entry.output === ''
              ? []
              : [{ type: 'text', text: entry.output }],
            isError: failed,
          },
          ...(entry.output === undefined
            ? { error: { name: 'Interrupted', code: 'interrupted', reason: '该调用没有留下结果' } }
            : {}),
        }, { surfaceOp: 'append' })
        // 计划态从档案重建：有输出 = 工具真的执行过（enter 免审批必执行；exit 获批
        // 才有输出，denied/中断不留）。被别的会话事件更晚翻转的边角属已知近似
        // （宿主 PlanState 全局，见 SH-105 注记）。
        if (entry.tool === 'enter_plan_mode' && !failed) setPlanActive(true)
        if (entry.tool === 'exit_plan_mode' && !failed) setPlanActive(false)
      }
      journal.entryToSeq[index] = journal.seq
    })
    closeTurn()
    return journal
  }

  /** 审批：把后端的请求交给 DSH 审批卡，裁决回执只认后端签发过的 request_id。 */
  const requestApproval = async (
    sessionId: string, msg: Extract<EventMsg, { type: 'exec_approval_request' }>,
  ): Promise<void> => {
    const listener = approvalListeners[0]
    if (listener === undefined) {
      // 没有审批 UI 在听 = 没有人能裁决：按拒绝收口，绝不因为「没人反对」就执行。
      console.warn('[SHANIU] 没有审批界面在听 approval/request，本次请求按拒绝回执收口')
      await bridge.invoke('approve', { requestId: msg.request_id, decision: 'deny' }).catch(() => {})
      return
    }
    const next = async (): Promise<never> => {
      throw new RemoteError('gateway/internal', 'approval/request 没有下游处理者', {})
    }
    // 载荷照 user-approval 的事件声明（ApprovalRequestEvent）：必填只有 toolName，
    // 卡片把 reason 当描述文本。`args`/`risk` 不在声明里，直接塞进去会被无声忽略——
    // 表现就是卡片只写"要执行工具"而不说执行什么，所以把它们并进 reason。
    const preview = JSON.stringify(msg.args)
    const reason = `风险 ${msg.risk}：${msg.tool} ${preview.length > 160 ? `${preview.slice(0, 160)}…` : preview}`
    let decision = 'deny'
    try {
      const outcome = await listener.call(approvalScope?.(sessionId), {
        agent: { id: sessionId },
        toolName: msg.tool,
        reason,
        displayReason: { en: reason },
      }, next)
      // DSH 的 PendingApproval 只回答 allowed-once / rejected；「本会话记住」需要
      // SH-025 把这档语义接进审批卡，这里不自造第三态。
      decision = outcome === 'allowed-once' ? 'allow' : 'deny'
    } catch (reason) {
      // 抛错和"用户点了拒绝"必须分开：静默按拒绝收口会让一次接线错误长得像一次正常否决。
      console.error('[SHANIU] 审批界面处理请求时抛错，本次按拒绝收口', reason)
      decision = 'deny'
    }
    await bridge.invoke('approve', { requestId: msg.request_id, decision })
      .catch((reason: unknown) => { console.error('[SHANIU] 审批回执送不出', reason) })
  }

  const onEvent = (event: AgentEvent): void => {
    const msg = event.msg
    // 子代理事件（agent_id ≠ 'root'）路由进该 agent 的泳道 journal（懒建，SH-109）；
    // session_id 仍是宿主侧任务会话（泳道的父）。根事件走原路径。
    const laneId = event.agent_id !== '' && event.agent_id !== 'root' && event.agent_id !== event.session_id
      ? event.agent_id
      : undefined
    const sessionId = laneId ?? event.session_id
    if (laneId !== undefined) ensureLane(event.session_id, laneId)
    // 泳道 timing 的 advance 折叠：delta/tool/usage 等事件推进 active.through（不推帧，
    // 见 foldLaneTiming 注记）；回合开合在各自 case 里折叠。
    if (laneId !== undefined && msg.type !== 'turn_started' && msg.type !== 'turn_completed') {
      foldLaneTiming(laneId, 'advance')
    }
    const journal = journalOf(sessionId)
    switch (msg.type) {
      case 'turn_started': {
        // 首条用户消息在本层落地时就开了回合（照 mock 的既定顺序：turn/start →
        // step/start → user/message，输入行必须落在回合内，否则回合头不显示用时）。
        // 因此后端的第一次 turn_started 通常是空操作；从第二次起才推进 step——
        // 后端每个采样迭代都发 turn_started，它对应 DSH 的 step 而不是 DSH 的回合。
        if (!journal.taskOpen) {
          openTask(journal)
          pushTurnStart(sessionId, journal)
          if (laneId !== undefined) foldLaneTiming(laneId, 'turn/start')
          pushEvent(journal, 'step/start', { turn: journal.turn, step: 0 })
          beginStepMetrics(journal)
          // 不经 prompt 起来的回合（`resume_session` 续跑）也要报运行中，否则
          // 恢复出来的会话跑着任务却没有 Stop 按钮。
          dispatch('api-session/status', sessionId, true)
        } else if (msg.turn > 1) {
          pushEvent(journal, 'step/end', { turn: journal.turn, step: journal.step })
          closeStepMetrics(journal)
          journal.step = msg.turn - 1
          pushEvent(journal, 'step/start', { turn: journal.turn, step: journal.step })
          beginStepMetrics(journal)
        }
        return
      }
      case 'agent_message_delta':
        journal.text += msg.delta
        if (journal.firstDeltaAt === undefined) journal.firstDeltaAt = now()
        pushAssistantChunk(journal, { type: 'text-delta', index: 0, text: msg.delta })
        return
      case 'reasoning_delta':
        journal.reasoning += msg.delta
        if (journal.firstDeltaAt === undefined) journal.firstDeltaAt = now()
        pushAssistantChunk(journal, { type: 'reasoning-delta', index: 0, text: msg.delta })
        return
      case 'token_usage': {
        // 上游 TokenUsage 语义：inputTokens 是**未命中缓存**的输入（bucketsFrom 直接
        // 拿它当 uncachedInputTokens 桶，total = input + output + read + write）。后端
        // 的 input_tokens 是全量 prompt（含缓存命中），必须先扣掉 cached 再进 usage，
        // 否则 totalTokens 和用量桶都把缓存部分算两遍。
        const read = msg.cached_input_tokens ?? 0
        const uncached = Math.max(0, msg.input_tokens - read)
        journal.usage = {
          inputTokens: uncached,
          outputTokens: msg.output_tokens,
          cacheReadTokens: read,
          cacheWriteTokens: 0,
          totalTokens: uncached + msg.output_tokens + read,
        }
        // 指标同步折叠：用量桶累加（后端没有重试事件可做 last-wins 替换，逐次累加是
        // 当前事件源下唯一诚实的口径）；压力分子取 prompt 侧全量（上游 pressureFrom
        // = 未缓存输入 + 缓存读写，即整个 prompt）。
        journal.metrics.buckets = {
          uncachedInputTokens: journal.metrics.buckets.uncachedInputTokens + uncached,
          outputTokens: journal.metrics.buckets.outputTokens + msg.output_tokens,
          cacheReadTokens: journal.metrics.buckets.cacheReadTokens + read,
          cacheWriteTokens: journal.metrics.buckets.cacheWriteTokens,
        }
        journal.pressure = { pressureTokens: msg.input_tokens, projectedTokens: msg.input_tokens }
        pushMetricsFrame(sessionId, journal)
        return
      }
      case 'tool_call_begin':
        // 工具调用先定稿此前的正文：助手消息在 DSH 里排在调用之前。
        closeAttempt(sessionId, journal, true)
        pushEvent(journal, 'tool/call', {
          turn: journal.turn,
          step: journal.step,
          callId: msg.call_id,
          name: msg.tool,
          // DSH 的 tool/call.arguments 是模型产出的原始 JSON 字符串，不是对象。
          arguments: typeof msg.args === 'string' ? msg.args : JSON.stringify(msg.args ?? {}),
        })
        journal.outputs.set(msg.call_id, { stdout: [], stderr: [] })
        journal.toolStartAt.set(msg.call_id, now())
        journal.callTools.set(msg.call_id, msg.tool)
        journal.callArgs.set(msg.call_id, msg.args)
        return
      case 'tool_call_output_delta': {
        const buffer = journal.outputs.get(msg.call_id) ?? { stdout: [], stderr: [] }
        buffer[msg.channel].push(msg.delta)
        journal.outputs.set(msg.call_id, buffer)
        return
      }
      case 'tool_call_end': {
        const buffer = journal.outputs.get(msg.call_id)
        journal.outputs.delete(msg.call_id)
        const toolName = journal.callTools.get(msg.call_id)
        journal.callTools.delete(msg.call_id)
        const toolArgs = journal.callArgs.get(msg.call_id)
        journal.callArgs.delete(msg.call_id)
        const lines = [...(buffer?.stdout ?? []), ...(buffer?.stderr ?? [])]
        // result 存在时是唯一状态判据（docs/04 §1.2.1）；缺失才回退 exit_code/error。
        const failed = msg.result !== undefined
          ? msg.result !== 'ok'
          : (msg.exit_code ?? 0) !== 0 || (msg.error !== null && msg.error !== undefined)
        const textParts: string[] = []
        if (lines.length > 0) textParts.push(lines.join('\n'))
        // 终端卡片按尾标记解析退出码（terminal-card-model.ts 的既有约定）。
        if (msg.exit_code !== null && msg.exit_code !== undefined) textParts.push(`[exit code: ${msg.exit_code}]`)
        const error = msg.result === 'denied'
          ? { name: 'Denied', code: 'denied', reason: msg.error ?? '用户拒绝了该操作' }
          : msg.result === 'cancelled'
            ? { name: 'Interrupted', code: 'interrupted', reason: msg.error ?? '已取消' }
            : failed && msg.error !== null && msg.error !== undefined
              ? { name: 'ToolError', code: 'tool_error', reason: msg.error }
              : undefined
        // validator 要求：带 data.error 时 message.isError 必须为 true。
        const isError = error !== undefined || failed
        // todo_write（SH-108）：执行成功才落 `todo/write` journal 事件（上游顺序
        // tool/call → todo/write → tool/result），整表替换进 todos 投影。失败的
        // 调用（校验拒绝/取消/拒绝）什么都不落——回声复活成清单才是伪造。
        if (toolName === 'todo_write' && !failed) {
          const todos = parseTodos(toolArgs)
          if (todos !== undefined) {
            journal.todos = todos
            pushEvent(journal, 'todo/write', { todos })
            pushTodosFrame(sessionId, journal)
          }
        }
        pushEvent(journal, 'tool/result', {
          turn: journal.turn,
          step: journal.step,
          message: {
            id: `shaniu-result-${msg.call_id}`,
            role: 'tool',
            source: { kind: 'tool', callId: msg.call_id },
            toolCallId: msg.call_id,
            content: textParts.length === 0 ? [] : [{ type: 'text', text: textParts.join('\n') }],
            isError,
          },
          ...(error === undefined ? {} : { error }),
        }, { surfaceOp: 'append' })
        settleToolMetrics(journal, msg.call_id)
        pushMetricsFrame(sessionId, journal)
        // 计划态按工具**执行结果**推导：enter_plan_mode 免审批必执行；exit_plan_mode
        // 获批执行才退出（denied/interrupted 都保持计划态，与宿主 PlanState 一致）。
        if (toolName === 'enter_plan_mode' && !failed) setPlanActive(true)
        if (toolName === 'exit_plan_mode' && !failed) setPlanActive(false)
        return
      }
      case 'turn_completed':
        // cancelled 的半截正文是真实发生过的输出：落进档案，但打 interrupted，
        // 不能当成正常答完。
        closeTask(sessionId, journal, msg.reason === 'completed'
          ? { kind: 'completed' }
          : msg.reason === 'cancelled'
            ? { kind: 'aborted', reason: { kind: 'user' } }
            : { kind: 'error', error: { message: `回合以 ${msg.reason} 结束`, code: msg.reason } },
        msg.reason === 'cancelled')
        if (laneId !== undefined) foldLaneTiming(laneId, 'turn/end', msg.reason)
        // 运行态必须收回：输入卡的 Stop 按钮由 `api-session/status` 决定（`running`
        // 为真才显示），只发 journal 的 turn/end 不发状态，按钮会永远停在发送态。
        dispatch('api-session/status', sessionId, false)
        // 闸随任务协程收口释放（晚于本事件）：排队消息在这里按序补发，对短时拒绝重试。
        void drainQueues()
        return
      case 'error':
        // 全局错误不终结回合（回合终结只认 turn_completed）：这里只定稿已有的流，
        // 错误文案由随后的 turn_completed 呈现，避免在此造出一个假回合。
        closeAttempt(sessionId, journal, false)
        console.warn('[SHANIU] 后端错误事件', msg.message)
        return
      case 'exec_approval_request':
        // 泳道的审批请求按泳道会话作用域派发：审批卡落在该泳道的 composer 上，
        // `this` 绑定泳道会话（上游子代理审批的同款作用域语义；冒泡聚合属 SH-054）。
        void requestApproval(sessionId, msg)
        return
      default:
        // exec_approval_resolved / plan_delta / turn_diff：后端当前不产生，客户端无需感知。
        return
    }
  }

  const addressOf = (request: unknown): string | undefined => {
    const address = (request as { address?: { kind?: string; sessionId?: string; childSessionId?: string } } | undefined)?.address
    return address?.kind === 'session' ? address.sessionId : address?.childSessionId
  }

  /**
   * 立即提交一条任务并开回合（`session.prompt` 的"非排队"路径本体）。SH-115 抽出：
   * `/plan <message>` 的上游语义是"进入计划态并把消息交给模型开始只读调研"（plan-mode
   * host 插件的 handler 里 `agent.steer(createUserMessage(...))`），桌面壳没有运行中
   * 注入面（SH-053），空闲态下等价动作就是走这条与用户输入完全相同的提交链。
   */
  const submitPromptNow = async (sessionId: string | undefined, text: string, rpcId?: string, displayText?: string, images: readonly PromptImageRef[] = []): Promise<void> => {
    const agentId = await bridge.invoke<string>('send_task', { task: text, agentId: sessionId })
    const journal = journalOf(agentId)
    const shown = displayText ?? text
    if (!titles.has(agentId)) pushTitle(agentId, shown.trim().slice(0, 12) || '新会话')
    // 回合在这里开，不等后端的第一条事件：输入行必须落在回合内，
    // 而 send_task 已经返回 agent_id，说明后端确实接下了这个任务。
    if (!journal.taskOpen) openTask(journal)
    dispatch('api-session/status', agentId, true)
    pushTurnStart(agentId, journal)
    pushEvent(journal, 'step/start', { turn: journal.turn, step: journal.step })
    beginStepMetrics(journal)
    // rpcId 必须等于 prompt 的 requestId：客户端据此撤掉本地乐观回显（session.ts 的
    // observeSubmissionMessage），否则同一条消息会出现两遍。
    pushEvent(journal, 'user/message', {
      id: `shaniu-user-${journal.seq + 1}`,
      role: 'user',
      content: [
        ...images.map(image => ({ type: 'image', attachment: image })),
        ...(shown === '' ? [] : [{ type: 'text', text: shown }]),
      ],
      source: { kind: 'user', ...(rpcId === undefined ? {} : { rpcId }) },
    }, { surfaceOp: 'append' })
  }

  const sessionNamespace = {
    list: async () => {
      const rows = await bridge
        .invoke<{ agentId: string; updatedAt: number; title: string; workspaceRoot: string; blank: boolean }[]>('list_sessions')
      return OK({
        items: rows.map(row => {
          const title = titles.get(row.agentId) ?? row.title
          return {
            agentAvailable: true,
            sessionId: row.agentId,
            cwd: row.workspaceRoot,
            running: journals.get(row.agentId)?.taskOpen === true,
            // blank 的权威判定在宿主（折叠前缀无 turn，SH-100 探索报告 list.ts:51）：
            // 后端按 item_count==0 给出，face 不用 title 猜——title 为空但已有
            // turn 的会话（如被截断的 fork）不是 blank。
            blank: row.blank,
            updatedAt: row.updatedAt,
            formatStatus: 'current',
            // 标题走 wire 摘要的投影提示块（cached）：客户端不给没打开过的会话建
            // journal，侧栏冷标题只能从这里来（manager.applyListBlock 只填还没有
            // sequenced 值的键，不会盖掉改名时的实时写入）。空标题缺席=未知，
            // 界面按"未命名"渲染，不发显式 null 去清别人的值。
            ...(title === '' ? {} : { projections: { kind: 'cached', asOfSeq: 0, values: { title } } }),
          }
        }),
      })
    },
    create: async (request: { workspaceId?: string; cwd?: string } = {}) => {
      // 先回宿主读一次：没有缓存时 workspaceRoot() 是空串，会把 workspaceId 解析错。
      // 建档是一次显式用户动作，多一个 invoke 不算代价。
      await face.refresh()
      // 工作区解析（SH-099）：workspaceId 是 composer 选择工作区的正式载荷（上游
      // createInWorkspace 同形状）；cwd 作为未指名工作区时的目录回落。解析成路径
      // 后交 create_session —— 会话按 cwd 绑定（config_snapshot.workspace_root），
      // 工具沙箱作用域跟随。
      const registered = registeredWorkspaces()
      const byId = request.workspaceId === undefined
        ? undefined
        : registered.find(ws => ws.id === request.workspaceId)
      if (request.workspaceId !== undefined && byId === undefined) {
        return FAIL(`工作区 ${request.workspaceId} 不存在，先在侧栏添加它`)
      }
      const cwd = byId?.path ?? (request.cwd !== undefined && request.cwd !== '' ? request.cwd : face.workspaceRoot())
      // 空白复用（上游 createInWorkspace 的 findReusableBlank 同构）：先在工作区
      // 成员里找第一条可复用的空白（blank + cwd 匹配 + 未归档），带 sessionId 请求
      // 创建——宿主二次校验后复用同 id 或新造。没有这步，每次点新建/每次启动恢复
      // 都会无声积累一份空白档案（真机曾积累到 10 份）。
      const [flags, rows] = await Promise.all([
        readWorkspaceFlags(),
        bridge
          .invoke<{ agentId: string; workspaceRoot: string; updatedAt: number; blank: boolean }[]>('list_sessions')
          .catch((): { agentId: string; workspaceRoot: string; updatedAt: number; blank: boolean }[] => []),
      ])
      const reusableBlank = rows.find(row =>
        row.blank && row.workspaceRoot === cwd
          && !flags.archivedSessionIds.includes(row.agentId)
          && journals.get(row.agentId)?.taskOpen !== true,
      )?.agentId
      const agentId = await bridge.invoke<string>('create_session', {
        ...(cwd === face.workspaceRoot() ? {} : { workspacePath: cwd }),
        ...(reusableBlank === undefined ? {} : { sessionId: reusableBlank }),
      })
      // 复用空白会话时宿主的 `create_session` 会回同一个 id（上游 findReusableBlank 同构），
      // 而视图的 `follow` 可能已经在这个 id 上打开并捕获了 journal 对象。这里**不能**
      // 用新 journal 覆盖同 id 的旧对象：follow 的 tail 会永远挂在孤儿 journal 上，新任务
      // 的事件写进另一个对象——真机表现就是"实时态只有用户气泡，重载后档案全对"（B-20）。
      // 已有 journal 就保留（空白复用的语义就是接着这个会话继续）；只有从未观察过才新建。
      if (!journals.has(agentId)) journals.set(agentId, newJournal(now()))
      // 工作区分组按 sessionIds 显示：新会话建完就要推一帧，否则侧栏的工作区树
      // 里看不到它，要重开窗口才出现。目标工作区可能不是默认那家，按路径找视图。
      const target = registered.find(ws => ws.path === cwd) ?? registered[0]
      if (target !== undefined) {
        workspaceLog.append({ type: 'upsert', workspace: await workspaceViewFor(target, rows, flags) })
      }
      return OK({ sessionId: agentId })
    },
    fork: async (request: { sessionId: string; throughSeq?: number }) => {
      const journal = journals.get(request.sessionId)
      let keepItems = Number.MAX_SAFE_INTEGER
      if (request.throughSeq !== undefined) {
        const through = request.throughSeq
        // 截断点必须换算成**档案条目数**：journal 的 seq 是事件序号，一个条目会展开
        // 成好几条事件，拿 seq 当条数传给后端就会从历史中间切断（有 call 无 output，
        // 续跑第一次请求直接 400）。所以只用重建时记下的对账表换算，表外一律拒绝。
        const map = journal?.entryToSeq ?? []
        if (map.length === 0) {
          return FAIL('该会话还没有档案↔事件的对账表（打开过一次会话后即可按游标分支）')
        }
        if (through > map[map.length - 1]!) {
          return FAIL('截断点超出已对账的档案前缀：那之后的事件还没有对应的档案条目可换算')
        }
        let kept = 0
        for (const [index, seq] of map.entries()) {
          if (seq <= through) kept = index + 1
        }
        keepItems = kept
      }
      const agentId = await bridge.invoke<string>('fork_session', {
        agentId: request.sessionId,
        // 末尾分支 = 整份档案续一份。后端 nearest_safe_cut 会把请求量钳到档案实际条数
        // 并回退到最后一个调用/输出配对的边界，所以"全部"给 MAX_SAFE_INTEGER 是安全的。
        keepItems,
      })
      // 分支出来的会话**不能**预置空 journal：那样 follow 会命中"本进程已观察过"，
      // 直接交一个空快照，用户看到的是一个历史全空的会话，而档案里明明拷来了条目。
      // 留空让第一次 follow 走档案重建，与"重开旧会话"同一条路径。
      journals.delete(agentId)
      // fork 会话继承源会话的工作区（可能在任意一家）：逐家重推受影响的视图
      const flags = await readWorkspaceFlags()
      const rows = await bridge
        .invoke<{ agentId: string; workspaceRoot: string; updatedAt: number }[]>('list_sessions')
        .catch((): { agentId: string; workspaceRoot: string; updatedAt: number }[] => [])
      for (const ws of registeredWorkspaces()) {
        workspaceLog.append({ type: 'upsert', workspace: await workspaceViewFor(ws, rows, flags) })
      }
      return OK({ sessionId: agentId })
    },
    prompt: async (request: { sessionId?: string; requestId?: string; content?: readonly PromptAttachmentPart[] }) => {
      const composed = await composePromptText(bridge, request.sessionId, request.content)
      if (!composed.ok) return FAIL(composed.error)
      const text = composed.text
      // 本会话回合进行中 → 客户端排队（上游宿主 durable inbox 的前端等价物）：
      // 登记进队列 + 推 inbox 投影行（QueueDock 出行、可编辑/移除），回合收口后按序
      // 自动补发。跨会话撞全局单任务闸仍走原路报错（队列只管本会话的收口补发）。
      if (request.sessionId !== undefined) {
        const busy = journals.get(request.sessionId)
        if (busy?.taskOpen === true && request.requestId !== undefined) {
          queueSeq += 1
          busy.queue = [...busy.queue, {
            id: `shaniu-queued-${queueSeq}`,
            requestId: request.requestId,
            text,
            displayText: composed.displayText,
            images: composed.images,
          }]
          pushInboxFrame(request.sessionId, busy)
          return OK({ accepted: true })
        }
      }
      submitPromptNow(request.sessionId, text, request.requestId, composed.displayText, composed.images)
      return OK({ accepted: true })
    },
    cancel: async (request: { sessionId?: string } = {}) => {
      // 这里不定稿任何东西：中断的收口权在后端的 turn_completed{cancelled}，
      // 提前定稿会造出一个后端没承认过的终态。
      await bridge.invoke('cancel_task', { sessionId: request.sessionId })
      return OK({ accepted: true })
    },
    follow: async function* (request: unknown, signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> {
      const sessionId = addressOf(request)
      if (sessionId === undefined) {
        traceFollow(undefined, 'no-address')
        yield emptySnapshot('', now())
        return
      }
      traceFollow(sessionId, 'enter', { known: journals.has(sessionId) })
      // 让 WS gateway 为这个会话补订阅：页面重载后 follow 只重建档案，不补订阅的话
      // 运行中任务的后半段实时帧不会推过来（gateway 只发已订阅会话）。
      bridge.observeEvents?.(sessionId)
      const journal = journals.get(sessionId)
      if (journal === undefined) {
        // 没在当前进程里观察过：能从档案重建就重建（SH-033 的"打开旧会话"），
        // 重建不出内容才交空快照。档案读不回是硬错误——静默交空态会让用户以为
        // 会话没了，而实际只是宿主读失败。
        const entries = await bridge.invoke<SessionEntry[]>('load_session', { agentId: sessionId })
        if (entries.length === 0) {
          traceFollow(sessionId, 'end-empty')
          yield emptySnapshot(sessionId, now())
          return
        }
        const replayed = replayArchive(entries)
        journals.set(sessionId, replayed)
        if (!titles.has(sessionId)) {
          // 宿主回显优先：list_sessions 的 title 已合并 titles.json 的自定义标题
          // （缺省回落后端自己的派生规则）。本地再从档案派生一份会把它盖掉——
          // 重启后用户改过的名字就丢了（真机实测暴露的缺陷）。
          const rows = await bridge.invoke<{ agentId: string; title: string }[]>('list_sessions')
            .catch((): { agentId: string; title: string }[] => [])
          const hostTitle = rows.find(row => row.agentId === sessionId)?.title ?? ''
          if (hostTitle !== '') {
            pushTitle(sessionId, hostTitle)
          } else {
            const firstUser = entries.find(entry => entry.kind === 'message' && entry.role === 'user')
            const text = firstUser?.kind === 'message' ? decodeImageMarkers(firstUser.text).text : ''
            pushTitle(sessionId, text.trim().slice(0, 12) || '新会话')
          }
        }
        const from = replayed.log.length
        traceFollow(sessionId, 'snapshot-replayed', { from, revision: replayed.revision, records: replayed.log.eventsUpTo(from).length })
        yield {
          type: 'snapshot',
          header: { version: 0, id: sessionId, createdAt: replayed.createdAt, isSeeded: false },
          cursor: replayed.seq,
          records: replayed.log.eventsUpTo(from),
          hasMore: false,
          projections: {
            asOfSeq: 0,
            values: {
              modelSelection: currentModelSelection(),
              title: titles.get(sessionId) ?? null,
              permissions: permissionsProjection(),
              ...(planProjectionValues()),
              // 档案重建出的任务清单（todo/write 推导）随快照带出：重开旧会话
              // TodoDock 直接回到最后一份清单（上游回放语义）。
              ...todosProjectionValues(replayed),
              // 重建已折叠出步/回合计数：重开旧会话底部指标立即有值（时间不计，见
              // replay 的注记）；旧档案没有用量，tokenUsage/contextPressure 不挂。
              ...metricsProjectionsOf(replayed),
              // 泳道位：父会话挂 subagentCatalog、泳道挂 subagentTiming（缺席=无数据）。
              ...subagentProjectionsOf(sessionId),
            },
          },
          assistantStream: { revision: replayed.revision },
        }
        try {
          for await (const frame of replayed.log.tail(from, signal)) yield frame
        } finally {
          traceFollow(sessionId, 'end-replayed')
        }
        return
      }
      // 下标在同步块里一次捕获：cursor / revision / records 必须描述同一时刻，
      // 之后产生的帧由 tail(from) 按位置补齐，不存在"注册太晚丢帧"的窗口。
      const from = journal.log.length
      const baseline = {
        cursor: journal.seq,
        records: journal.log.eventsUpTo(from),
        revision: journal.revision,
      }
      traceFollow(sessionId, 'snapshot-live', { serial: journal.serial, from, revision: baseline.revision, records: baseline.records.length })
      yield {
        type: 'snapshot',
        header: { version: 0, id: sessionId, createdAt: journal.createdAt, isSeeded: false },
        // cursor 必须等于末条 record 的 seq，否则客户端触发 page 补齐并判协议错。
        cursor: baseline.cursor,
        records: baseline.records,
        hasMore: false,
        projections: {
          asOfSeq: 0,
          values: {
            modelSelection: currentModelSelection(),
            title: titles.get(sessionId) ?? null,
            permissions: permissionsProjection(),
            ...(planProjectionValues()),
            // 有排队消息才挂 inbox：QueueDock/steer 占位都按投影缺席=无队列判读。
            ...(journal.queue.length > 0
              ? { inbox: { 'next-turn': journal.queue.map(queueRowOf) } }
              : {}),
            ...todosProjectionValues(journal),
            ...metricsProjectionsOf(journal),
            // 泳道位：父会话挂 subagentCatalog、泳道挂 subagentTiming（缺席=无数据）。
            ...subagentProjectionsOf(sessionId),
          },
        },
        // 客户端总是带 assistantStream:true 打开，缺这个基线即 gateway/internal。
        // 流式中途打开时必须带 activeAttempt（含已吐出的紧凑前缀），否则客户端拿不到
        // 这个尝试的身份，后续 chunk 会被当成孤儿丢掉（实时态空白，B-20 根因）。
        assistantStream: {
          revision: baseline.revision,
          ...(journal.attemptOpen
            ? {
                activeAttempt: {
                  attemptId: journal.attemptId,
                  startedAfterSeq: journal.attemptStartedAfterSeq,
                  turn: journal.attemptTurn,
                  step: journal.attemptStep,
                  nextIndex: journal.chunkIndex,
                  // 快照必须是不可变值：交给客户端后再追加的 chunk 不能改到这份基线。
                  stream: journal.attemptStream.map(item => ({ time: item.time, chunk: item.chunk })),
                },
              }
            : {}),
        },
      }
      try {
        for await (const frame of journal.log.tail(from, signal)) yield frame
      } finally {
        traceFollow(sessionId, 'end-live')
      }
    },
    control: async function* (signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> {
      await face.refresh()
      const from = projectionLog.length
      yield {
        type: 'baseline',
        value: {
          projections: Object.fromEntries([...journals.entries()].map(([id, journal]) => [id, {
            asOfSeq: 0,
            values: {
              modelSelection: currentModelSelection(),
              title: titles.get(id) ?? null,
              permissions: permissionsProjection(),
              ...(planProjectionValues()),
              ...(journal.queue.length > 0
                ? { inbox: { 'next-turn': journal.queue.map(queueRowOf) } }
                : {}),
              ...todosProjectionValues(journal),
              ...metricsProjectionsOf(journal),
              // 泳道位：父会话挂 subagentCatalog、泳道挂 subagentTiming（缺席=无数据）。
              ...subagentProjectionsOf(id),
            },
          }])),
        },
      }
      // 基线之后必须持续产帧：流一结束客户端按 carrier 错误重连。`signal` 一路传进
      // tail——客户端 teardown 时 `await control.dispose()` 要能落地，不能挂在
      // 「等下一帧」的 await 上（与 session.follow 同一类悬挂）。
      for await (const frame of projectionLog.tail(from, signal)) yield frame
    },
    attachment: async (request: { sessionId?: string; attachment?: PromptImageRef } = {}) => {
      const sessionId = request.sessionId
      const image = request.attachment
      if (sessionId === undefined || image === undefined) {
        return FAIL('附件读取缺少 sessionId 或 attachment')
      }
      try {
        const read = await bridge.invoke<AttachmentReadWire>('read_attachment', {
          sessionId,
          receiptId: image.attachmentId,
          maxBytes: 32 * 1024 * 1024,
        })
        return OK({
          attachment: {
            ...image,
            mediaType: read.mediaType,
            bytes: read.bytes,
            width: read.width ?? image.width,
            height: read.height ?? image.height,
          },
          data: read.data,
        })
      } catch (reason) {
        const message = reason instanceof Error ? reason.message : String(reason)
        return FAIL(`读取历史附件失败：${message}`)
      }
    },
    page: async (request: { sessionId?: string; throughSeq?: number } = {}) => {
      const journal = request.sessionId === undefined ? undefined : journals.get(request.sessionId)
      if (journal === undefined) return OK({ records: [], hasMore: false, cursor: -1 })
      const through = request.throughSeq ?? journal.seq
      const records = journal.log
        .eventsUpTo(journal.log.length)
        .filter(entry => FrameLog.eventSeq(entry) <= through)
      return OK({ records, hasMore: false, cursor: records.length === 0 ? -1 : through })
    },
    projections: async () => OK(null),
    search: async (request: { query?: string } = {}) => {
      const query = (typeof request.query === 'string' ? request.query : '').trim()
      // 空/纯空白按上游语义回空表（WorkspaceBrowser 自己也会短路空 query），
      // 且不该为它付宿主 SQLite 打开成本。
      if (query === '') return OK({ items: [], hasMore: false })
      try {
        // DSH wire 只送 {query}：没有 cursor、没有 limit，宿主也不做续页。
        const page = await bridge.invoke<SearchPageWire>('search_sessions', { query })
        const items: SearchHitWire[] = []
        const seen = new Set<string>()
        // 宿主已按 sessionId 去重只给最强命中；这里再兜一层，避免前端
        // contentBySession 静默丢掉重复条目。
        for (const hit of Array.isArray(page?.items) ? page.items : []) {
          if (typeof hit?.sessionId !== 'string' || hit.sessionId === '' || seen.has(hit.sessionId)) continue
          seen.add(hit.sessionId)
          items.push({
            sessionId: hit.sessionId,
            snippet: typeof hit?.snippet === 'string' ? hit.snippet : String(hit?.snippet ?? ''),
          })
        }
        // hasMore 只驱动 UI 一行「结果较多，请收窄查询」提示，不是分页信号。
        return OK({ items, hasMore: page?.hasMore === true })
      } catch (reason) {
        // wire 契约：失败以 {ok:false} 回给客户端，不是 rejection。宿主给的是
        // {code,message}（session_query::SearchError 的序列化形状）；索引/查询/档案
        // 读失败一律如实回显，不返回空表冒充"没搜到"。
        const failure = reason as { code?: unknown; message?: unknown }
        const message = typeof failure?.message === 'string' ? failure.message : String(reason)
        return FAIL(message)
      }
    },
    modelCatalog: async () => face.session.modelCatalog(),
    selectModel: async (request: { sessionId?: string; provider?: string; model?: string; reasoningEffort?: string }) => {
      const selected = await face.session.selectModel({
        provider: request.provider ?? '',
        model: request.model ?? '',
        ...(request.reasoningEffort === undefined ? {} : { reasoningEffort: request.reasoningEffort }),
      })
      if (!selected.ok) return selected
      projectionSeq += 1
      for (const [id, journal] of journals.entries()) {
        // 模型切换会换上下文窗口：有过压力采样的会话随 modelSelection 一帧补推新的
        // 占用读数，仪表不用等到下回合才反映新容量。
        const window = face.activeContextWindow()
        const pressure = journal.pressure !== undefined && window !== undefined
          ? [{ key: 'contextPressure', val: { ...journal.pressure, contextWindow: window } }]
          : []
        projectionLog.append({
          type: 'projection', sessionId: id, seq: projectionSeq,
          changes: [{ key: 'modelSelection', val: currentModelSelection() }, ...pressure],
        })
      }
      return selected
    },
    canOpenWorkspacePath: async () => OK(true),
    /**
     * SH-092 批 3：某文件当前的注册处理器（上游 `NativeFileApplication` 数组）。
     * application 打开时宿主会重新校验一次关联表——这里只做传递，不缓存处理器列表。
     */
    workspacePathApplications: async (request: { path?: string } = {}) => {
      try {
        return OK(await bridge.invoke<Array<Record<string, unknown>>>('workspace_path_applications', { path: request.path ?? '' }))
      } catch (reason) {
        return FAIL((reason as { message?: string } | undefined)?.message ?? '无法读取文件关联')
      }
    },
    openWorkspacePath: async (request: { path?: string; action?: 'reveal'; application?: string } = {}) => {
      try {
        await bridge.invoke('open_workspace_path', {
          path: request.path ?? '',
          ...(request.action === undefined ? {} : { action: request.action }),
          ...(request.application === undefined ? {} : { application: request.application }),
        })
        return OK({ opened: true })
      } catch (reason) {
        return FAIL((reason as { message?: string } | undefined)?.message ?? '无法打开该路径')
      }
    },
    initializeDefaultModel: async () => {
      await face.refresh()
      return OK(undefined)
    },
    rename: async (request: { sessionId?: string; title?: string } = {}) => {
      const sessionId = request.sessionId ?? ''
      let accepted: string | null
      try {
        accepted = await bridge.invoke<string | null>('rename_session', {
          agentId: sessionId,
          title: request.title ?? '',
        })
      } catch (reason) {
        // wire 契约：失败以 {ok:false} 回给客户端，不是 rejection——manager.rename 直接读
        // result.ok，穿透的 rejection 会绕过 UI 的错误处理（重命名对话框收不到原因）。
        // 宿主给的是 {code,message}（rollout::RenameError 的序列化形状）；title-invalid
        // 映射 DSH 为非法标题声明的正式错误码 session/title-invalid，其余按网关内部错误回显文案。
        const failure = reason as { code?: unknown; message?: unknown }
        const message = typeof failure?.message === 'string' ? failure.message : String(reason)
        if (failure?.code === 'title-invalid') {
          return { ok: false as const, error: new RemoteError('session/title-invalid', message, { sessionId: asSessionId(sessionId) }) }
        }
        return FAIL(message)
      }
      // 本地标题缓存必须跟着改：它优先于宿主回显，留着旧值会让侧栏在下一次读列表时
      // 又变回改名前的样子——用户看到的就是"改了一半"。
      if (accepted === null) titles.delete(sessionId)
      else titles.set(sessionId, accepted)
      // DSH 的返回类型要求一个"提交位置"。标题不是 journal 事件（我们的词汇表里没有
      // title 事件，surfaceOp 也只允许 5 类），所以给提交时刻的 journal 末尾 seq：
      // 客户端只用它做投影排序，不会据此回放内容。
      return OK({ title: accepted ?? '', seq: journals.get(sessionId)?.seq ?? -1 })
    },
    /**
     * 客户端队列的行内操作（上游语义见 commands.ts:447-519；这里操作适配层自己的
     * 待发队列——补发前的编辑/移除都是真实生效的）。steer 的上游语义（注入进行中的
     * 回合）需要宿主注入面，后端没有：落成"移到队首、下一轮优先补发"的真实近似，
     * 行保持可见。删除/替换在途消息的竞态与上游同款：先领取后发送，迟到操作收到
     * queue-item-not-found（行内文案"可能已经开始发送"正是为这个竞态写的）。
     */
    updateQueue: async (
      request: { sessionId?: string; itemId?: string; action?: { kind?: string; content?: readonly { type: string; text?: string }[] } } = {},
    ) => {
      const journal = request.sessionId === undefined ? undefined : journals.get(request.sessionId)
      const index = journal?.queue.findIndex(item => item.id === request.itemId) ?? -1
      if (journal === undefined || index < 0 || request.action === undefined) {
        return {
          ok: false as const,
          error: new RemoteError('session/queue-item-not-found',
            `队列里没有 ${request.itemId ?? '<无 id>'} 这条待发消息（它可能已经开始发送）`,
            // 队列行 id 就是 UserMessage.id（wire 的 MessageId 品牌）
            { itemId: (request.itemId ?? '') as unknown as MessageId }),
        }
      }
      const item = journal.queue[index]!
      switch (request.action.kind) {
        case 'edit': {
          const text = (request.action.content ?? [])
            .filter(part => part.type === 'text').map(part => part.text ?? '').join('')
          if (text.trim() === '') {
            return {
              ok: false as const,
              error: new RemoteError('gateway/bad-request', '编辑后的消息不能为空', {}),
            }
          }
          item.text = text
          item.displayText = text
          item.images = []
          pushInboxFrame(request.sessionId!, journal)
          return OK({ accepted: true })
        }
        case 'remove': {
          journal.queue = journal.queue.filter(candidate => candidate.id !== item.id)
          pushInboxFrame(request.sessionId!, journal)
          // 回声退休靠这条 journal 事件（outcome canceled）：只推投影会让本地回声行
          // 在 rpcId 对账里复活成"发送中"的幽灵行。
          pushEvent(journal, 'agent/inbox/spliced', {
            target: 'next-turn',
            start: index,
            removedCount: 1,
            inserted: [],
            outcome: 'canceled',
          })
          return OK({ accepted: true })
        }
        case 'steer': {
          // 无注入面：真实近似 = 提到队首，当前回合收口后第一个补发。
          journal.queue = [item, ...journal.queue.filter(candidate => candidate.id !== item.id)]
          pushInboxFrame(request.sessionId!, journal)
          return OK({ accepted: true })
        }
        default:
          return {
            ok: false as const,
            error: new RemoteError('gateway/bad-request', `未知的队列操作 ${String(request.action.kind)}`, {}),
          }
      }
    },
    state: () => unimplemented('session.state', 'SH-033'),
    wait: () => unimplemented('session.wait', 'SH-033'),
    events: () => unimplemented('session.events', 'SH-033'),
    observeEvent: () => unimplemented('session.observeEvent', 'SH-094'),
    observeProjections: () => unimplemented('session.observeProjections', 'SH-094'),
    observeState: () => unimplemented('session.observeState', 'SH-094'),
  }

  /**
   * 工作区面（SH-099/100/110/111/112）：注册表 = 可见默认项（`[workspace].root`
   * 派生，id 固定）+ 宿主 `[[workspaces]]`，顺序由 `[workspace].order` 决定。
   * 会改状态的成员全部接到宿主命令；宿主吃不下的按 wire 错误码如实拒绝。
   */
  const HOST_WORKSPACE_ID = 'default-workspace'
  const workspaceLog = new FrameLog()
  const bootedAt = new Date().toISOString()

  /** 归档/置顶/手动排序的宿主侧车状态（SH-100，`get_workspace_flags` 的形状）。 */
  interface WorkspaceFlags {
    readonly archivedSessionIds: readonly string[]
    readonly pinnedSessionIds: readonly string[]
    readonly sessionOrder: readonly string[]
  }
  let workspaceFlags: WorkspaceFlags = { archivedSessionIds: [], pinnedSessionIds: [], sessionOrder: [] }
  const readWorkspaceFlags = async (): Promise<WorkspaceFlags> => {
    workspaceFlags = await bridge
      .invoke<WorkspaceFlags>('get_workspace_flags')
      .catch((): WorkspaceFlags => workspaceFlags)
    return workspaceFlags
  }

  /**
   * 工作区注册表（SH-099）：默认工作区来自 [workspace].root（id 固定），额外
   * 工作区来自宿主 `[[workspaces]]`（face.refresh 后随快照读取）。默认项与上游
   * registry 项同构：标题可改（落 [workspace].title），可删除（[workspace].hidden
   * 标记——摘出清单、目录与会话保留）。**删除后不复活**：重新添加同路径由宿主
   * 全新注册成新 id（SH-110，上游 `removedIds` 不变式要求 id 永不复用）。
   *
   * 顺序（SH-111）：存过的 `[workspace].order` 是权威，未列出的按注册表序补尾。
   * 与宿主 `workspace_order` 是同一份规则的两面——两侧漂移会让拖完重启顺序跳变。
   */
  interface RegisteredWorkspace { readonly id: string; readonly path: string; readonly title: string }
  const registeredWorkspaces = (): RegisteredWorkspace[] => {
    const registry: RegisteredWorkspace[] = [
      ...face.workspaceHidden()
        ? []
        : [{ id: HOST_WORKSPACE_ID, path: face.workspaceRoot(), title: face.workspaceTitle() }],
      ...face.workspaces(),
    ]
    const seen = new Set<string>()
    const ranked: RegisteredWorkspace[] = []
    for (const id of face.workspaceOrder()) {
      const ws = registry.find(candidate => candidate.id === id)
      if (ws !== undefined && !seen.has(id)) {
        seen.add(id)
        ranked.push(ws)
      }
    }
    return [...ranked, ...registry.filter(ws => !seen.has(ws.id))]
  }

  /**
   * 一个工作区的视图。`sessionIds` 按 SH-100 的手动顺序排（侧车序 ∩ 本工作区现存，
   * 缺的按列表序补位）——上游 per-workspace `sessionIds` 就是展示顺序，且其 getter
   * 本就按 cwd 动态过滤（entity.ts:102-104），SHANIU 的按 path 过滤与之同构；归档
   * 会话**保留槽位**，隐藏由客户端按 archivedSessionIds 集过滤。
   */
  const workspaceViewFor = async (
    ws: RegisteredWorkspace,
    rows: { agentId: string; workspaceRoot: string; updatedAt: number }[],
    flags: WorkspaceFlags,
  ): Promise<Record<string, unknown>> => {
    const mine = rows.filter(row => row.workspaceRoot === ws.path)
    const latest = mine.reduce((max, row) => Math.max(max, row.updatedAt), 0)
    const ordered = [
      ...flags.sessionOrder.filter(id => mine.some(row => row.agentId === id)),
      ...mine.filter(row => !flags.sessionOrder.includes(row.agentId)).map(row => row.agentId),
    ]
    return {
      workspaceId: ws.id,
      path: ws.path,
      // 上游按 title === 'default-workspace' 判定首启自动标题并渲染成「默认工作区」
      title: ws.title,
      sessionIds: ordered,
      createdAt: bootedAt,
      updatedAt: latest === 0 ? bootedAt : new Date(latest).toISOString(),
    }
  }

  const allWorkspaceViews = async (flags: WorkspaceFlags): Promise<Record<string, unknown>[]> => {
    const rows = await bridge
      .invoke<{ agentId: string; workspaceRoot: string; updatedAt: number }[]>('list_sessions')
      .catch((): { agentId: string; workspaceRoot: string; updatedAt: number }[] => [])
    return Promise.all(registeredWorkspaces().map(ws => workspaceViewFor(ws, rows, flags)))
  }

  /** 集合变化检测：全量替换帧只在集合真的变了时发（上游 feed 的同口径）。 */
  const sameSet = (a: readonly string[], b: readonly string[]): boolean =>
    a.length === b.length && a.every((id, index) => id === b[index])

  /** 宿主 WorkspaceOpError → wire 错误码（上游 workspace-controller types.ts 的同一批语义）。 */
  /** 变更上下文：details 类型按 vendored RemoteErrorDetailsMap 的形状要求携带。 */
  interface WorkspaceOpContext {
    readonly sessionId?: string
    readonly beforeSessionId?: string
    /** 工作区级操作的目标 id：给了它，not-found 走 workspace/not-found 而不是会话级。 */
    readonly workspaceId?: string
  }
  const workspaceOpFailure = (
    reason: unknown,
    context: WorkspaceOpContext = {},
  ): { ok: false; error: RemoteError } => {
    const failure = reason as { code?: unknown; message?: unknown }
    const message = typeof failure?.message === 'string' ? failure.message : String(reason)
    const code = typeof failure?.code === 'string' ? failure.code : ''
    switch (code) {
      // activity 空：SHANIU 的活动判定只有"任务闸占着"一个来源，没有更细的清单可给
      case 'session-active':
        return {
          ok: false,
          error: new RemoteError('workspace/session-active', message, {
            sessionId: asSessionId(context.sessionId ?? ''),
            activity: [],
          }),
        }
      case 'not-found':
        // 上游 commands.ts：工作区级 WorkspaceOrderInvalidError → workspace/not-found
        // （details 只要 workspaceId）；会话级才是 session/not-found
        return context.workspaceId !== undefined
          ? {
            ok: false,
            error: new RemoteError('workspace/not-found', message, { workspaceId: asWorkspaceId(context.workspaceId) }),
          }
          : {
            ok: false,
            error: new RemoteError('session/not-found', message, { sessionId: asSessionId(context.sessionId ?? '') }),
          }
      case 'pin-archived':
        return { ok: false, error: new RemoteError('gateway/bad-request', message, {}) }
      case 'move-invalid':
        return {
          ok: false,
          error: new RemoteError('workspace/move-invalid', message, {
            workspaceId: asWorkspaceId(HOST_WORKSPACE_ID),
            sessionId: asSessionId(context.sessionId ?? ''),
            ...(context.beforeSessionId === undefined ? {} : { beforeSessionId: asSessionId(context.beforeSessionId) }),
          }),
        }
      default:
        return { ok: false, error: new RemoteError('gateway/internal', message, {}) }
    }
  }

  /** 变更后统一收口：重读宿主侧车 → 对比推帧（archived/pinned/order）→ 返回集合值。 */
  const applyFlagsMutation = async (
    previous: WorkspaceFlags,
    value: Record<string, unknown>,
    orderChanged: boolean,
  ): Promise<Record<string, unknown>> => {
    const next = await readWorkspaceFlags()
    if (!sameSet(previous.archivedSessionIds, next.archivedSessionIds)) {
      workspaceLog.append({ type: 'archived', archivedSessionIds: next.archivedSessionIds })
    }
    if (!sameSet(previous.pinnedSessionIds, next.pinnedSessionIds)) {
      workspaceLog.append({ type: 'pinned', pinnedSessionIds: next.pinnedSessionIds })
    }
    if (orderChanged || !sameSet(previous.sessionOrder, next.sessionOrder)) {
      // 手动顺序在 sidecar 是全局扁平表，任何一家都可能受影响：逐家重推视图
      for (const view of await allWorkspaceViews(next)) {
        workspaceLog.append({ type: 'upsert', workspace: view })
      }
    }
    return value
  }

  const workspaceNamespace = {
    follow: async function* (signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> {
      await face.refresh()
      const flags = await readWorkspaceFlags()
      const from = workspaceLog.length
      yield {
        type: 'baseline',
        value: { items: await allWorkspaceViews(flags), archivedSessionIds: flags.archivedSessionIds, pinnedSessionIds: flags.pinnedSessionIds },
      }
      // 同 control：signal 传进 tail，客户端 teardown 的 `dispose()` 才能落地。
      for await (const frame of workspaceLog.tail(from, signal)) yield frame
    },
    initializeDefault: async () => {
      await face.refresh()
      // 上游 registry.initializeDefault：`defaultWorkspaceId` 标记还在但实体已删
      // → 返回 undefined（deleteKnown 不清标记）。SHANIU 的 hidden 就是那个「删过」
      // 的事实，所以**不复活**。契约是 `WorkspaceValue | undefined`——裸 undefined，
      // 不是 `{workspace: undefined}`：后者让 ClientWorkspaceModel.upsert(undefined)
      // 抛 TypeError，被 navigation 兜底成「默认工作区初始化失败」toast（SH-112）。
      if (face.workspaceHidden()) return OK(undefined)
      const flags = await readWorkspaceFlags()
      const views = await allWorkspaceViews(flags)
      // 顺序可被拖拽改（SH-111），默认项不一定在 [0]：按 id 找，找不到 = 无默认项
      const view = views.find(item => item['workspaceId'] === HOST_WORKSPACE_ID)
      return view === undefined ? OK(undefined) : OK({ workspace: view })
    },
    /** 工作区拖拽排序（SH-111）：宿主存完整顺序，回值 = 完整顺序 + 推 order 帧。 */
    insertBefore: async (request: { workspaceId?: string; beforeWorkspaceId?: string } = {}) => {
      const workspaceId = request.workspaceId ?? ''
      try {
        const value = await bridge.invoke<{ workspaceIds: string[] }>('insert_workspace_before', {
          workspaceId,
          ...(request.beforeWorkspaceId === undefined ? {} : { beforeWorkspaceId: request.beforeWorkspaceId }),
        })
        // 宿主刚写过 config：face 缓存跟上，后续 baseline/视图才按新顺序排
        await face.refresh()
        workspaceLog.append({ type: 'order', workspaceIds: value.workspaceIds })
        return OK({ workspaceIds: value.workspaceIds })
      } catch (reason) {
        return workspaceOpFailure(reason, { workspaceId })
      }
    },
    /** 注册新工作区（SH-099）：路径来自目录选择流；幂等重复注册 created:false。 */
    create: async (request: { path?: string } = {}) => {
      try {
        const value = await bridge.invoke<Record<string, unknown>>('create_workspace', { path: request.path ?? '' })
        // 宿主刚写过 config（新注册或默认项复活）：face 的缓存快照必须跟上，
        // 否则后续 allWorkspaceViews 的注册表还是旧的
        await face.refresh()
        await readWorkspaceFlags()
        const workspace = value['workspace'] as Record<string, unknown> | undefined
        if (workspace !== undefined) workspaceLog.append({ type: 'upsert', workspace })
        return OK(value)
      } catch (reason) {
        return workspaceOpFailure(reason, {})
      }
    },
    rename: async (request: { workspaceId?: string; title?: string } = {}) => {
      try {
        const value = await bridge.invoke<Record<string, unknown>>('rename_workspace', {
          workspaceId: request.workspaceId ?? '',
          title: request.title ?? '',
        })
        // 宿主刚写过 config：face 缓存跟上（默认项标题落在 [workspace].title）
        await face.refresh()
        const workspace = value['workspace'] as Record<string, unknown> | undefined
        if (workspace !== undefined) workspaceLog.append({ type: 'upsert', workspace })
        return OK(value)
      } catch (reason) {
        return workspaceOpFailure(reason, { workspaceId: request.workspaceId ?? '' })
      }
    },
    delete: async (request: { workspaceId?: string } = {}) => {
      try {
        await bridge.invoke<Record<string, unknown>>('delete_workspace', { workspaceId: request.workspaceId ?? '' })
        // 宿主刚写过 config（额外项摘除或默认项落 hidden）：face 缓存跟上
        await face.refresh()
        workspaceLog.append({ type: 'remove', workspaceId: request.workspaceId ?? '' })
        return OK({ removed: request.workspaceId ?? '' })
      } catch (reason) {
        return workspaceOpFailure(reason, { workspaceId: request.workspaceId ?? '' })
      }
    },
    archiveSession: async (request: { sessionId?: string; stopActivity?: boolean } = {}) => {
      const sessionId = request.sessionId ?? ''
      const previous = await readWorkspaceFlags()
      try {
        const value = await bridge.invoke<Record<string, unknown>>('archive_session', {
          sessionId,
          ...(request.stopActivity === true ? { stopActivity: true } : {}),
        })
        return OK(await applyFlagsMutation(previous, value, false))
      } catch (reason) {
        return workspaceOpFailure(reason, { sessionId: request.sessionId ?? '' })
      }
    },
    unarchiveSession: async (request: { sessionId?: string } = {}) => {
      const previous = await readWorkspaceFlags()
      try {
        const value = await bridge.invoke<Record<string, unknown>>('unarchive_session', { sessionId: request.sessionId ?? '' })
        return OK(await applyFlagsMutation(previous, value, false))
      } catch (reason) {
        return workspaceOpFailure(reason)
      }
    },
    pinSession: async (request: { sessionId?: string } = {}) => {
      const previous = await readWorkspaceFlags()
      try {
        const value = await bridge.invoke<Record<string, unknown>>('pin_session', { sessionId: request.sessionId ?? '' })
        return OK(await applyFlagsMutation(previous, value, false))
      } catch (reason) {
        return workspaceOpFailure(reason, { sessionId: request.sessionId ?? '' })
      }
    },
    unpinSession: async (request: { sessionId?: string } = {}) => {
      const previous = await readWorkspaceFlags()
      try {
        const value = await bridge.invoke<Record<string, unknown>>('unpin_session', { sessionId: request.sessionId ?? '' })
        return OK(await applyFlagsMutation(previous, value, false))
      } catch (reason) {
        return workspaceOpFailure(reason, { sessionId: request.sessionId ?? '' })
      }
    },
    insertSessionBefore: async (request: { sessionId?: string; beforeSessionId?: string } = {}) => {
      const previous = await readWorkspaceFlags()
      try {
        const value = await bridge.invoke<Record<string, unknown>>('insert_session_before', {
          sessionId: request.sessionId ?? '',
          ...(request.beforeSessionId === undefined ? {} : { beforeSessionId: request.beforeSessionId }),
        })
        // 顺序变了：upsert 一帧携带重排后的 sessionIds（上游 order 变化的同款推送）
        return OK(await applyFlagsMutation(previous, value, true))
      } catch (reason) {
        return workspaceOpFailure(reason, { sessionId: request.sessionId ?? '', beforeSessionId: request.beforeSessionId })
      }
    },
  }

  /**
   * 工具结果检视流（Trajectory 的详情面板）。数据本来就在 journal 里，缺的只是入口：
   * 此前落到"未接线"代理，点开工具卡详情只会报命令面未接线。
   *
   * 语义照 DSH 的 host 实现（`client/ui-trajectory/src/index.ts:105`）：冷读先交
   * `{available:false}`，值落定再交一帧——`available:false` 与"结果本身是 JSON null"
   * 必须分得开。结果落定后不再变化（后端没有工具输出的增量事件，见文件头），所以交完
   * 真值就收流；消费者拆流走 `$stream` 的 dispose 路径，不靠 signal。
   *
   * 交出去的是 journal 里那条 `tool/result` 的 `message`——DSH host 存的是工具自己的
   * JSON 返回值，而 SHANIU 的后端把输出收成文本消息（含 `[exit code: N]` 尾标），
   * message 是这一侧最接近的事实形状。
   */
  const toolResultsNamespace = {
    watch: async function* (
      request: { sessionId?: string; seq?: number } = {}, signal?: AbortSignal,
    ): AsyncGenerator<Record<string, unknown>> {
      const journal = request.sessionId === undefined ? undefined : journals.get(request.sessionId)
      const seq = request.seq ?? -1
      const settled = (): Record<string, unknown> | undefined => journal?.log
        .eventsUpTo(journal.log.length)
        .map(entry => entry['event'] as Record<string, unknown> | undefined)
        .find(event => event !== undefined && event.type === 'tool/result' && event.seq === seq)
      if (journal === undefined) {
        yield { available: false }
        return
      }
      const first = settled()
      if (first !== undefined) {
        yield { available: true, value: (first.data as Record<string, unknown>).message ?? null }
        return
      }
      yield { available: false }
      for await (const entry of journal.log.tail(journal.log.length, signal)) {
        const event = entry['event'] as Record<string, unknown> | undefined
        if (event === undefined || event.type !== 'tool/result' || event.seq !== seq) continue
        yield { available: true, value: (event.data as Record<string, unknown>).message ?? null }
        return
      }
    },
  }

  /**
   * `remote.permissionPresets`：权限挡位目录（形状照上游 PermissionCatalog，mock 同款）。
   * 目录是静态三挡（映射见 PRESET_TO_SANDBOX 注记），defaultPreset 动态反推自宿主
   * 当前沙箱档——宿主重启后 UI 显示的就是 config.toml 里真实生效的那档。静态目录
   * 不会失效，所以不发上游的 `permission-presets/catalog-changed` 失效事件。
   */
  const permissionPresetsNamespace = {
    catalog: async () => {
      // 首开竞态兜底：适配层装配时的那次 face.refresh 是异步的，芯片在它完成前
      // 打开会拿到 undefined 快照、回落成 read_only——先同步读一次宿主再回答。
      await face.refresh()
      const options = PERMISSION_PRESET_OPTIONS.map(option => ({ ...option }))
      return OK({
        options,
        defaultOptions: options.map(option => ({ ...option })),
        defaultPreset: currentPermission(),
      })
    },
  }

  // ---- 宿主命令面（SH-115）----

  /**
   * 宿主命令目录：`/` 菜单行的数据源。只列**真实可执行**的命令——goal/feedback/file
   * 没有宿主语义（goal/feedback 的宿主插件不存在、file 依赖未接线的附件面），不上
   * 目录（不伪造可点但跑不通的行）；model/file 的菜单行由 vendored 客户端包自己
   * 贡献（modelDirectories / 附件面），不归这张表。文案一律上游原文——菜单行的
   * 本地化 label/描述/图标由 vendored presentation 按 definitionId 自动取 ui-commands
   * 词典（`description.compact` 等），这里不需要也无法复刻。
   * `plan` 的 `attachments: false` 与上游不同（上游 true）：上游把附件转交模型，
   * 我们没有附件面，如实声明不接受。
   */
  const HOST_COMMAND_DESCRIPTORS: readonly CommandDescriptor[] = [
    {
      definitionId: CommandDefinitionId('@deepseek-ai/dsh-command-compact'),
      name: 'compact',
      description: 'Compact older conversation history',
    },
    {
      definitionId: CommandDefinitionId('@deepseek-ai/dsh-plan-mode'),
      name: 'plan',
      description: 'Enter or leave plan mode',
      input: { hint: '[off|message]', attachments: false },
    },
    {
      definitionId: CommandDefinitionId('@deepseek-ai/dsh-permission-presets'),
      name: 'permission',
      description: 'Switch the permission preset (sandbox mode + approval policy)',
      input: { hint: '<preset>' },
    },
    {
      definitionId: CommandDefinitionId('@deepseek-ai/dsh-session-log-export'),
      name: 'export',
      description: 'Download this Session log as a ZIP archive',
    },
  ]

  /** 上游宿主执行器会把 run/done 写进会话日志渲染成流节点；桌面壳没有命令日志面，
   *  以 pushEvent 把同一对事件推进活动 journal（仅活态、不落档案——重启后节点消失
   *  而命令效果持久，这个不对称如实存在，与"没有日志面就不伪造"同一口径）。 */
  const pushCommandRun = (sessionId: string, name: string, args: string): { commandId: string } => {
    const journal = journalOf(sessionId)
    const commandId = `shaniu-cmd-${journal.seq + 1}`
    pushEvent(journal, 'command/run', {
      commandId,
      name,
      ...(args === '' ? {} : { args }),
      source: { kind: 'user' },
    })
    return { commandId }
  }

  const pushCommandDone = (
    sessionId: string,
    commandId: string,
    result: { kind: 'success' | 'error'; text: string },
  ): void => {
    pushEvent(journalOf(sessionId), 'command/done', { commandId, kind: result.kind, text: result.text })
  }

  /**
   * `remote.commands`（SH-115）：`list` 供 `/` 菜单与输入触发器解析（含中文别名
   * 计划/压缩/权限/导出，vendored `resolveCommand` 现成），`execute` 真执行。
   * 返回值契约：value 非 undefined 即"命中命令"（session.ts 的 matched 判据），
   * `value.result` 是 CommandResult——ui-commands 据此发本地 `command/executed`，
   * PlanChip 的退出路径只看 value 是否为 undefined。未命中行一律 OK(undefined)。
   */
  const commandsNamespace = {
    list: async () => OK(HOST_COMMAND_DESCRIPTORS),
    execute: async (sessionId: string, line: string, _args: readonly unknown[] = []): Promise<
      | { ok: true; value: { result: { kind: 'success' | 'error'; text: string } } }
      | { ok: true; value: undefined }
      | { ok: false; error: RemoteError }
    > => {
      const input = line.trim()

      // ---- /plan：进入/退出计划态（真实退出，替换 SH-106 期间的"指引"方案）----
      // 上游 handler（plan-mode host 插件）逐字对齐：off → 置假；带消息 → 进入并把
      // 消息交给模型；bare → 进入。PlanState 进程级全局，工具路径（enter/exit_
      // plan_mode）与命令路径写同一份状态，芯片投影经 setPlanActive 即时随行。
      if (/^\/plan\b/.test(input)) {
        const message = input.replace(/^\/plan\b/, '').trim()
        if (message === 'off') {
          const run = pushCommandRun(sessionId, 'plan', 'off')
          const active = await bridge.invoke<boolean>('set_plan_mode', { active: false })
          setPlanActive(active)
          const result = { kind: 'success' as const, text: 'Plan mode off.' }
          pushCommandDone(sessionId, run.commandId, result)
          return OK({ result })
        }
        const run = pushCommandRun(sessionId, 'plan', message)
        const active = await bridge.invoke<boolean>('set_plan_mode', { active: true })
        setPlanActive(active)
        if (message !== '') {
          const busy = journals.get(sessionId)
          if (busy?.taskOpen === true) {
            // 运行中提交本该走上游的 steer（运行中注入）；桌面壳没有注入面（SH-053），
            // 如实报错让草稿留在输入框，用户可以停下任务后重发。
            const result = {
              kind: 'error' as const,
              text: '已进入计划态，但运行中的会话暂不能接收这条消息（steering 待 SH-053）；停止任务后可直接发送。',
            }
            pushCommandDone(sessionId, run.commandId, result)
            return OK({ result })
          }
          await submitPromptNow(sessionId, message)
        }
        const result = {
          kind: 'success' as const,
          text: 'Plan mode on. Use /plan off to leave.',
        }
        pushCommandDone(sessionId, run.commandId, result)
        return OK({ result })
      }

      // ---- /compact：回合外按需压缩（SH-115）----
      if (/^\/compact\b/.test(input)) {
        if (input !== '/compact') {
          const result = { kind: 'error' as const, text: 'Usage: /compact (no arguments)' }
          const run = pushCommandRun(sessionId, 'compact', input.slice('/compact'.length).trim())
          pushCommandDone(sessionId, run.commandId, result)
          return OK({ result })
        }
        const run = pushCommandRun(sessionId, 'compact', '')
        let result: { kind: 'success' | 'error'; text: string }
        try {
          const outcome = await bridge.invoke<{ summary: string; shadowedItems: number; shadowedTokens: number } | null>(
            'compact_session', { agentId: sessionId },
          )
          if (outcome === null) {
            result = { kind: 'success', text: 'No compactable history yet.' }
          } else {
            // 摘要正文推一条 user 消息：重开/回放时档案锚点本来就投影成 user 消息
            // （lib.rs project_entries），活态推同形状，两边渲染一致。
            const journal = journalOf(sessionId)
            pushEvent(journal, 'user/message', {
              id: `shaniu-compact-${journal.seq + 1}`,
              role: 'user',
              content: [{ type: 'text', text: outcome.summary }],
              source: { kind: 'user' },
            }, { surfaceOp: 'append' })
            result = {
              kind: 'success',
              text: `Compacted ${outcome.shadowedItems} history items (~${outcome.shadowedTokens} tokens).`,
            }
          }
        } catch (reason) {
          result = { kind: 'error', text: reason instanceof Error ? reason.message : String(reason) }
        }
        pushCommandDone(sessionId, run.commandId, result)
        return OK({ result })
      }

      // ---- /permission：挡位切换（SH-105 既有实现，文案对齐上游 permission-presets）----
      const permissionMatch = /^\/permission\s*([a-z][a-z-]*)?\s*$/.exec(input)
      if (permissionMatch !== null) {
        const run = pushCommandRun(sessionId, 'permission', permissionMatch[1] ?? '')
        let result: { kind: 'success' | 'error'; text: string }
        const requested = permissionMatch[1]
        if (requested === undefined) {
          result = {
            kind: 'success',
            text: `current preset ${currentPermission()} (available: ${PERMISSION_PRESETS.join(', ')})`,
          }
        } else {
          const sandbox = sandboxOfPreset(requested)
          if (sandbox === undefined) {
            result = {
              kind: 'error',
              text: `unknown preset "${requested}" (available: ${PERMISSION_PRESETS.join(', ')})`,
            }
          } else {
            try {
              await face.applySandboxMode(sandbox)
              result = { kind: 'success', text: `preset ${requested}` }
            } catch (reason) {
              // wire 契约：失败以 result.error 回给客户端，不是 rejection——UI 的错误
              // 处理读 result.ok，穿透的 rejection 会绕过它（挡位芯片收不到失败原因）。
              result = { kind: 'error', text: reason instanceof Error ? reason.message : String(reason) }
            }
          }
        }
        if (result.kind === 'success' && requested !== undefined) {
          pushPermissionToAll()
          // 与设置页写挡位走**同一条**收尾链（修订 +1 + `settings/document-updated`
          // 广播）：少了这次广播，设置页的 describe 缓存不失效——芯片切完再打开设置，
          // 权限行还显示上一档（真机 E-06）。
          face.commitPermissionPreset()
        }
        pushCommandDone(sessionId, run.commandId, result)
        return OK({ result })
      }

      // ---- /export：发起会话日志 ZIP 下载（执行者在 header-actions，SH-114 批 2）----
      if (/^\/export\b/.test(input)) {
        const run = pushCommandRun(sessionId, 'export', input.slice('/export'.length).trim())
        let result: { kind: 'success' | 'error'; text: string }
        if (input !== '/export') {
          result = { kind: 'error', text: 'The /export command does not accept arguments.' }
        } else if (requestExport(asSessionId(sessionId))) {
          result = { kind: 'success', text: 'Session log download requested.' }
        } else {
          result = { kind: 'error', text: 'The export surface is not available in this build.' }
        }
        pushCommandDone(sessionId, run.commandId, result)
        return OK({ result })
      }

      return OK(undefined)
    },
  }

  /**
   * 目录选择流（SH-099）：侧栏「添加工作区」的 occupant（ui-directory-picker-native）
   * 经此拿宿主目录。pick 返回路径或 null（取消）——上游 remote 契约同形状。
   */
  const directoryPickerNamespace = {
    pick: async () => {
      try {
        return OK(await bridge.invoke<string | null>('pick_folder'))
      } catch (reason) {
        const message = reason instanceof Error ? reason.message : String(reason)
        return { ok: false as const, error: new RemoteError('gateway/internal', message, {}) }
      }
    },
  }

  /**
   * `remote.subagents`（SH-109）：泳道子会话的 prompt / interruptByUser 面。会话
   * 控制器对 addressed 子会话硬性路由到这两个方法（`sessions/session.ts` 的 prompt
   * 与 Stop 路径），适配层必须提供命名空间面。桌面真实态在 SH-053 交付前没有子代理
   * 注入/逐个中断面——按上游错误码显式拒绝，不伪造受理；懒建泳道（unknown 模式）的
   * composer 本来就是只读的，正常路径到不了这里。
   */
  const subagentsNamespace = {
    prompt: async (request: { childSessionId?: string } = {}) => {
      throw new RemoteError('subagent/delivery-unavailable', '子代理 steering 待后端 M5 交付（docs/05 SH-053）', {
        childSessionId: laneIdOf(request.childSessionId ?? ''),
      })
    },
    interruptByUser: async () => {
      throw new RemoteError('gateway/internal', '子代理逐个中断待后端 M5 交付（docs/05 SH-053）', {})
    },
  }

  /**
   * `remote.settings` 面：设置面本体直连，唯一例外是 `permission` 域写入成功后补一帧
   * 挡位投影。设置页的挡位行写的是宿主 `[sandbox] mode`，而对话页的挡位芯片读的是
   * 会话 `permissions` 投影——两条链不共享状态。少了这一帧，用户在设置页改完档回到
   * 对话页，芯片上还是旧档（`/permission` 命令路径本来就推，两条写面必须行为一致）。
   */
  const settingsNamespace = {
    describe: (): ReturnType<typeof face.settings.describe> => face.settings.describe(),
    mutate: async (ns: string, ops: readonly SettingsPathOpView[], expectedRevision?: number) => {
      const result = await face.settings.mutate(ns, ops, expectedRevision)
      if (result.ok && ns === 'permission') pushPermissionToAll()
      return result
    },
  }

  /**
   * `remote.workspaceFiles`（SH-092「文档预览真实读取」）。
   *
   * **边界为什么不带 workspaceId**：宿主按会话档案首行固化的根判定越界，客户端只能交出
   * 路径、交不出作用域——交出作用域等于让越界方指定它能看哪些目录。
   * **`changes` 为什么如实拒**：宿主没有文件监视面。契约自带 `workspace-file/watch-unsupported`
   * 这个码就是给这种情况用的；挂在一条永不发声的流上会把"没有实时刷新"伪装成"已接好"。
   * **base64 为什么不直接过 IPC**：Tauri 的 JSON 返回值容不下 `Uint8Array`，而字节窗口
   * 上限 1 MiB，编解码代价可控（宿主 `workspace_files.rs` 的 MAX_BYTES）。
   */
  const workspaceFileFailure = (reason: unknown) => {
    const raw = (reason ?? {}) as { code?: string; message?: string; details?: Record<string, unknown> }
    const detail = raw.details ?? {}
    const message = typeof raw.message === 'string' ? raw.message : String(reason)
    const path = String(detail['path'] ?? '')
    const fail = <C extends string, D extends Record<string, unknown>>(code: C, details: D) =>
      ({ ok: false as const, error: new RemoteError(code as never, message, details as never) })
    switch (raw.code) {
      case 'workspace-file/not-found': return fail('workspace-file/not-found', { path })
      case 'workspace-file/outside-workspace': return fail('workspace-file/outside-workspace', { path })
      case 'workspace-file/too-large': return fail('workspace-file/too-large', { path, limit: Number(detail['limit'] ?? 0) })
      case 'workspace-file/not-text': return fail('workspace-file/not-text', { path })
      case 'workspace-file/not-regular-file': return fail('workspace-file/not-regular-file', { path, kind: String(detail['kind'] ?? 'other') })
      case 'workspace-file/not-directory': return fail('workspace-file/not-directory', { path, kind: String(detail['kind'] ?? 'other') })
      case 'workspace/not-found': return fail('workspace/not-found', { workspaceId: String(detail['workspaceId'] ?? '') })
      default: return fail('gateway/internal', {})
    }
  }

  const bytesFromBase64 = (body: string): Uint8Array => {
    const binary = atob(body)
    const data = new Uint8Array(binary.length)
    for (let index = 0; index < binary.length; index += 1) data[index] = binary.charCodeAt(index)
    return data
  }

  const workspaceFilesNamespace = {
    stat: async (sessionId: string, path: string) => {
      try {
        return OK(await bridge.invoke<Record<string, unknown>>('workspace_file_stat', { sessionId, path }))
      } catch (reason) {
        return workspaceFileFailure(reason)
      }
    },
    read: async (sessionId: string, path: string, range: { offset?: number; limit?: number } = {}) => {
      try {
        return OK(await bridge.invoke<Record<string, unknown>>('workspace_file_read', {
          sessionId,
          path,
          ...(range.offset === undefined ? {} : { offset: range.offset }),
          ...(range.limit === undefined ? {} : { limit: range.limit }),
        }))
      } catch (reason) {
        return workspaceFileFailure(reason)
      }
    },
    readBytes: async (
      sessionId: string,
      path: string,
      options: { range?: { offset?: number; length?: number }; baseFile?: string } = {},
    ) => {
      try {
        const value = await bridge.invoke<Record<string, unknown>>('workspace_file_read_bytes', {
          sessionId,
          path,
          ...(options.range?.offset === undefined ? {} : { offset: options.range.offset }),
          ...(options.range?.length === undefined ? {} : { length: options.range.length }),
          ...(options.baseFile === undefined || options.baseFile === '' ? {} : { baseFile: options.baseFile }),
        })
        const { data, ...rest } = value
        return OK({ ...rest, data: bytesFromBase64(String(data ?? '')) })
      } catch (reason) {
        return workspaceFileFailure(reason)
      }
    },
    list: async (sessionId: string, path: string) => {
      try {
        return OK(await bridge.invoke<Record<string, unknown>>('workspace_file_list', { sessionId, path }))
      } catch (reason) {
        return workspaceFileFailure(reason)
      }
    },
    changes: async function* (_sessionId: string, path: string, _signal?: AbortSignal): AsyncGenerator<never> {
      // 契约要求首帧是 ready；这里没有可声明的监视器，如实带码中断而不是挂一条空流。
      throw new RemoteError('workspace-file/watch-unsupported' as never, '宿主没有文件监视面（SH-092 未含 watcher）', { path } as never)
    },
  }

  /**
   * `@file` 候选（SH-090 批 1）：路径-only 候选，模型自己用 read_file/list_dir 取材。
   * 作用域由宿主按会话档案的 workspace_root 决定，客户端只交查询文本。
   */
  const fileReferencesNamespace = {
    list: async (sessionId: string, query: string) => {
      try {
        return OK(await bridge.invoke<Array<Record<string, unknown>>>('list_file_references', { sessionId, query }))
      } catch (reason) {
        return FAIL((reason as { message?: string } | undefined)?.message ?? '无法列出文件引用候选')
      }
    },
  }
  /**
   * 会话引用候选（SH-090 批 2）：宿主读 rollout 摘要、按同工作区优先排序；
   * mention 的 canonical `dsh-session:` URI 由宿主生成。发送时的校验与上下文捕获在
   * agent_loop 的 pre-step 展开里完成（见 session_reference.rs）。
   */
  const sessionReferenceResolverNamespace = {
    candidates: async (sessionId: string, query: string) => {
      try {
        return OK(await bridge.invoke<Array<Record<string, unknown>>>('list_session_references', { sessionId, query }))
      } catch (reason) {
        return FAIL((reason as { message?: string } | undefined)?.message ?? '无法列出会话引用候选')
      }
    },
  }

  /**
   * SH-040 核销：桌面壳没有插件管理宿主面。面板按宿主真话渲染「不可管理」空态，
   * 而不是让一次探测把整页打成错误页。
   */
  const pluginInventoryNamespace = {
    list: async () => OK({ managementAvailable: false }),
  }
  /** 插件管理的读写面一律如实拒绝（面板在 managementAvailable=false 时不会走到这里）。 */
  const failWith = (code: string, message: string) =>
    ({ ok: false as const, error: new RemoteError(code as never, message, {} as never) })
  const unavailablePluginManager = async () =>
    failWith('plugin-manager/unavailable', '桌面壳不提供插件管理（插件面板为只读空态）')
  const pluginManagerNamespace = new Proxy({}, {
    get: (_target, method) => (typeof method === 'symbol' ? undefined : unavailablePluginManager),
  })
  const pluginRegistryProbeNamespace = {
    fastest: async () => failWith('plugin-registry/unavailable', '桌面壳不提供插件源探测'),
  }
  /**
   * SH-040 核销：自动化任务没有宿主调度面。目录/历史/额度给**空结果**让面板渲染
   * 空态；创建/编辑/运行等写面显式拒绝，不为「排期」伪造成功。
   */
  const scheduleUnavailable = async () =>
    failWith('schedule/unavailable', '桌面壳不提供自动化任务写入（面板为只读空态）')
  const scheduleNamespace = {
    catalog: async () => OK([]),
    history: async () => OK({ items: [], hasMore: false }),
    deliveryLimits: async () => OK({}),
    list: async () => OK({ items: [], shown: 0, total: 0, limit: 100, offset: 0 }),
    view: async () => OK({}),
    create: scheduleUnavailable,
    edit: scheduleUnavailable,
    run: scheduleUnavailable,
    deleteInactive: scheduleUnavailable,
  }

  const namespaces = new Map<string, unknown>([
    ['session', sessionNamespace],
    ['workspace', workspaceNamespace],
    ['workspaceFiles', workspaceFilesNamespace],
    ['fileReferences', fileReferencesNamespace],
    ['sessionReferenceResolver', sessionReferenceResolverNamespace],
    ['directoryPicker', directoryPickerNamespace],
    ['toolResults', toolResultsNamespace],
    ['settings', settingsNamespace],
    ['llm', face.llm],
    ['credentials', face.credentials],
    ['permissionPresets', permissionPresetsNamespace],
    ['commands', commandsNamespace],
    ['subagents', subagentsNamespace],
    ['pluginInventory', pluginInventoryNamespace],
    ['pluginManager', pluginManagerNamespace],
    ['pluginRegistryProbe', pluginRegistryProbeNamespace],
    ['schedule', scheduleNamespace],
  ])

  const remote: unknown = new Proxy({
    $host: { home: undefined, isLoopback: true },
    $on(event: string, listener: (...args: never[]) => unknown): () => void {
      if (event === 'approval/request') {
        const next = listener as (this: unknown, request: Record<string, unknown>, next: () => Promise<never>) => Promise<string>
        approvalListeners.push(next)
        return (): void => {
          const index = approvalListeners.indexOf(next)
          if (index >= 0) approvalListeners.splice(index, 1)
        }
      }
      const list = listeners.get(event) ?? []
      const next = listener as (...args: unknown[]) => unknown
      list.push(next)
      listeners.set(event, list)
      return (): void => {
        const index = (listeners.get(event) ?? []).indexOf(next)
        if (index >= 0) list.splice(index, 1)
      }
    },
    $emit: () => false,
    $call: () => Promise.reject(new Error('adapter(remote).$call: 该调用面尚未接线')),
    $stream: (streamOptions: { name?: string; open: (signal: AbortSignal) => AsyncIterable<unknown> }) => {
      let iterator: AsyncIterator<unknown> | null = null
      let generation = 0
      const streamName = streamOptions.name ?? 'remote stream'
      /**
       * 当前物理代次的中止开关。关流/换代必须能**打断挂起中的生成器**——生成器挂在
       * 「等下一帧」上的时候，`return()` 要等它自己醒来才生效（见 `FrameLog.tail` 的
       * signal 闸）。没有这个开关，`dispose()` 就永远不落地。
       */
      let generationAbort: AbortController | undefined
      const ensure = (): void => {
        // 惰性开启：journal 流的 initialRequest 要到首次迭代才就位。
        if (iterator !== null) return
        generationAbort = new AbortController()
        iterator = (streamOptions.open(generationAbort.signal))[Symbol.asyncIterator]()
      }
      const closeGeneration = (reason: string): void => {
        generationAbort?.abort(new Error(reason))
        generationAbort = undefined
        const current = iterator
        iterator = null
        void current?.return?.(undefined)
      }
      return {
        [Symbol.asyncIterator]: () => ({
          next: async () => {
            ensure()
            const result = await iterator!.next()
            if (result.done) return { done: true as const, value: undefined as never }
            return { done: false as const, value: { generation, value: result.value, accept: () => {} } }
          },
          return: async () => {
            closeGeneration(`${streamName} consumer closed`)
            return { done: true as const, value: undefined as never }
          },
        }),
        restart: () => {
          generation += 1
          closeGeneration(`${streamName} generation restarted`)
        },
        dispose: async () => {
          closeGeneration(`${streamName} disposed`)
        },
      }
    },
    $mount: async () => {
      throw new Error('adapter(remote).$mount: SHANIU 没有 typert 生成物，命名空间由适配层直接提供')
    },
  } as Record<string | symbol, unknown>, {
    get(target, prop) {
      if (prop in target) return target[prop]
      if (typeof prop === 'symbol') return undefined
      const known = namespaces.get(prop)
      if (known !== undefined) return known
      // 未接线的命名空间显式抛错并指回核销清单，不返回假数据让界面看起来正常。
      return new Proxy({}, {
        get: (_t, method) => () => unimplemented(`${String(prop)}.${String(method)}`, 'SH-040'),
      })
    },
  })

  // 开发期诊断（`import.meta.env.DEV` 静态裁剪）：真机排查"事件到了适配层却没渲染"
  // 时用它看 journal 的事件/助手流 revision 是否连续，不必往生产代码里塞日志。
  if (import.meta.env.DEV) {
    ;(globalThis as Record<string, unknown>).__SHANIU_ADAPTER_DUMP__ = () => {
      const result: Record<string, unknown> = {}
      for (const [id, journal] of journals) {
        const entries = journal.log.dump()
        const events = entries
          .filter(entry => entry['type'] === 'event')
          .map(entry => (entry['event'] as { type: string }))
        const streams = entries
          .filter(entry => entry['type'] === 'assistant-stream')
          .map(entry => entry['frame'] as { type: string; revision: number; index?: number; attemptId?: string })
        const gaps: string[] = []
        for (let index = 1; index < streams.length; index += 1) {
          const previous = streams[index - 1]!
          const current = streams[index]!
          if (current.revision !== previous.revision + 1) gaps.push(`${previous.revision}->${current.revision}`)
        }
        result[id] = {
          serial: journal.serial,
          seq: journal.seq,
          revision: journal.revision,
          taskOpen: journal.taskOpen,
          attemptOpen: journal.attemptOpen,
          chunkIndex: journal.chunkIndex,
          events: events.map(event => event.type),
          streams: streams.map(frame => ({ type: frame.type, revision: frame.revision, index: frame.index, attemptId: frame.attemptId })),
          gaps,
        }
      }
      result['__follow'] = followTrace
      return result
    }
  }

  const unlisten = bridge.listenEvents(onEvent)
  void face.refresh()

  return {
    remote,
    dispose() {
      unlisten()
    },
  }
}
