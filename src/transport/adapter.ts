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
  PERMISSION_PRESETS, PERMISSION_PRESET_OPTIONS, PERMISSION_UNAVAILABLE, presetOfSandbox, sandboxOfPreset,
} from './permission-presets'
import { CommandDefinitionId } from '@deepseek-ai/dsh-commands/brand'
import type { CommandDescriptor } from '@deepseek-ai/dsh-commands/types'
import { requestExport } from './command-triggers'
import {
  catalogEntryOf, catalogV3Of, descriptorOf, foldTimingEvent, initTimingState, laneIdOf,
  relaySourceOf, settledNoticeOf, timingProjectionOf,
  type SubagentCatalogEntry, type SubagentCatalogEventData, type SubagentTimingState,
} from './subagent-wire'
import type { AgentEvent, CompactionTrigger, EventMsg, SessionEntryLike, TurnErrorInfo } from './types'
import { archiveImageToPrompt, imageContentBlocks, type PromptImageRef } from './image-ref'
import type { HostBridge } from './tauri'

const OK = <T>(value: T) => ({ ok: true as const, value })
const FAIL = (message: string) =>
  ({ ok: false as const, error: new RemoteError('gateway/internal', message, {}) })

/** 宿主错误 → 可读文案。视觉门禁是 `{ code, message }` 结构化对象，优先取 message。 */
function hostErrorText(reason: unknown): string {
  if (typeof reason === 'object' && reason !== null) {
    const message = (reason as { message?: unknown }).message
    if (typeof message === 'string' && message !== '') return message
  }
  return reason instanceof Error ? reason.message : String(reason)
}

/** 视觉门禁：带图但当前模型不支持。重试不会变好，必须直接告诉用户。 */
function isModelNoVision(reason: unknown): boolean {
  return typeof reason === 'object' && reason !== null
    && (reason as { code?: unknown }).code === 'MODEL_NO_VISION'
}

/**
 * 宿主的会话 id 是裸字符串；进 DSH 的类型化载荷时补上品牌。品牌运行时即擦除
 * （铸造函数在宿主编译域，typecheck 域外），这里的显式转换表达"裸 id 进类型化载荷"。
 */
const asSessionId = (id: string) => id as unknown as SessionId
const asWorkspaceId = (id: string) => id as unknown as WorkspaceId

/**
 * 审批卡标题里的一句话对象：只说「是什么」。
 *
 * 完整参数不进标题——`JSON.stringify` 会把换行转义成 `\n`（两个字符），160 字符实际
 * 覆盖不到 80 个真实字符，任何有限长度都会在多文件补丁/长命令上截断。原文由详情槽
 * 按 `call_id` 从聊天快照取（spec §1.1 / D1）。
 */
function approvalSubject(msg: Extract<EventMsg, { type: 'exec_approval_request' }>): string {
  const args = msg.args
  if (args === null || typeof args !== 'object' || Array.isArray(args)) return ''
  const fields = args as Record<string, unknown>
  if (msg.tool === 'apply_patch') {
    const patch = fields['patch']
    if (typeof patch !== 'string') return ''
    const files = patch.match(/^\*\*\* (?:Add|Update|Delete) File: /gm)?.length ?? 0
    return files === 0 ? '' : `（${files} 个文件）`
  }
  const command = fields['command']
  if (typeof command !== 'string') return ''
  const firstLine = command.split('\n', 1)[0]?.trim() ?? ''
  if (firstLine === '') return ''
  return `（${firstLine.length > 60 ? `${firstLine.slice(0, 60)}…` : firstLine}）`
}

/**
 * `batch_scope` 是范围标识，不是文案：协议里不写中文，措辞在这里映射。
 * 未识别的标识必须显式暴露——静默当成「不批量」会让卡片少说一句它本应说的事。
 */
function batchScopeText(scope: string | undefined): string | undefined {
  if (scope === undefined) return undefined
  if (scope === 'same-args') {
    return '批准后，本轮内再次发起完全相同的调用不再询问；参数有任何变化都会重新确认。'
  }
  return `批准范围标识为 ${scope}（前端未识别，请按该标识的含义理解覆盖范围）`
}

/** 上游 token-meter 的用量四桶（usage-projection.ts bucketsFrom 的 wire 形状）。 */
interface TokenBuckets {
  readonly uncachedInputTokens: number
  readonly outputTokens: number
  readonly cacheReadTokens: number
  readonly cacheWriteTokens: number
}

/**
 * `session.search` wire 的一条命中（与 vendored `SessionSearchResultItem` 同形）。
 * 单独命名而不是内联：搜索结果在命令面、`SearchPageWire` 与未来消费方之间
 * 只能有一个形状。
 */
interface SearchHitWire {
  readonly sessionId: string
  readonly snippet: string
}

/**
 * 宿主 `search_sessions` 的返回（SH-113）：与 vendored `session.search` wire
 * 一一对应——`items` 只有 `sessionId` + `snippet`，`hasMore` 是「命中会话数超过
 * 宿主固定上限、请收窄查询」提示（`slots.ts:280`），**不是**分页/续页信号。
 * DSH 客户端没有 Show more 请求、不传 cursor/limit，故命令面也只有 `{query}`。
 */
interface SearchPageWire {
  readonly items: readonly SearchHitWire[]
  readonly hasMore: boolean
  /**
   * 宿主本次**跳过**的会话档案数（读不回来：被占用、权限、半截文件）。
   * 数字本身只够说「这次不完整」；具体 id 在宿主日志里（`shaniu.log` 的
   * `本次搜索跳过了读不回来的会话档案`），排障按那条汇总查。
   */
  readonly skippedSessions?: number
}

/**
 * 目标域的 wire 形状（SH-088），与 vendored `@deepseek-ai/dsh-goal/client` 的
 * `GoalSnapshot` / `GoalProjection` / `GoalView` 逐字段同形。宿主持久层
 * （`src-tauri/src/goals.rs`）只产出 durable 部分；`activation` 是进程内状态，
 * 由适配层叠加（上游同样不持久化）。
 */
type GoalPhase = 'active' | 'paused' | 'blocked' | 'complete'
type GoalActivation = 'armed' | 'disarmed'

interface GoalSnapshotWire {
  readonly id: string
  readonly revision: number
  readonly objective: string
  readonly phase: GoalPhase
  readonly blockedReason?: { readonly code: string; readonly message: string }
  readonly maxGoalRounds: number
}

interface GoalProjectionWire {
  readonly goal: GoalSnapshotWire
  readonly roundsStarted: number
  readonly createdAt: number
  readonly updatedAt: number
}

interface GoalViewWire extends GoalSnapshotWire {
  readonly roundsStarted: number
  readonly createdAt: number
  readonly updatedAt: number
  readonly activation: GoalActivation
}

interface GoalRefWire {
  readonly id: string
  readonly revision: number
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

const IMAGE_MARKER_PREFIX = '<!-- shaniu-image:'

/** 展示引用 → `send_task` 的结构化参数（Rust `ImageAttachmentInput`，camelCase）。 */
function toSendTaskImages(images: readonly PromptImageRef[]): Record<string, unknown>[] {
  return images.map(image => ({
    attachmentId: image.attachmentId,
    mediaType: image.mediaType,
    bytes: image.bytes,
    width: image.width,
    height: image.height,
    ...(image.name === undefined ? {} : { name: image.name }),
  }))
}

/**
 * **旧档案兼容**：早期前端把图片引用编码成任务文本里的一行 HTML 注释
 * （`<!-- shaniu-image:{json} -->`）。新版本走 `send_task` 的结构化 `images`
 * 参数（对齐 DSH `AttachmentStore.admitPromptContent` 的"编码上传 → 引用"模型），
 * 标记只保留解码能力，不再写入。
 */
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
      append(attachmentBlock(read.name, read.mediaType, read.bytes))
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

/**
 * 失败回合的原始诊断（供 UI 展开与复制，审计 A0）。
 * 文案只在展开区出现，不参与"一句人话"的展示；后端已做过脱敏。
 */
function errorDetail(info: TurnErrorInfo): string {
  const lines: string[] = []
  if (info.http_status !== undefined) lines.push(`HTTP ${info.http_status}`)
  if (info.provider_code !== undefined) lines.push(`code: ${info.provider_code}`)
  if (info.provider_message !== undefined) lines.push(info.provider_message)
  else if (info.message !== '') lines.push(info.message)
  // 重试次数只读档案里的 `TurnErrorInfo.attempts`——前端不另存计数器。两份数据
  // 迟早对不上；重开应用后能看到的次数必须与档案落档的是同一个值。
  // 0 = 首次请求就失败，不是「重试过」，不写这一行。
  if (info.attempts !== undefined && info.attempts > 0) lines.push(`重试 ${info.attempts} 次后仍失败`)
  if (info.retry_after_secs !== undefined) lines.push(`服务端要求等待 ${info.retry_after_secs} 秒`)
  return lines.join('\n')
}

/**
 * 档案里的结局字符串 → DSH `turn/end` 的 reason 形状（A0-3）。
 *
 * 档案只存代码值；这里只做形状翻译，不改写结局——`failed` 绝不能被翻译成
 * `completed` 或直接省略。
 */
function archiveTurnReason(reason: string, error?: TurnErrorInfo): Record<string, unknown> {
  switch (reason) {
    case 'completed':
      return { kind: 'completed' }
    case 'cancelled':
      return { kind: 'aborted', reason: { kind: 'user' } }
    case 'failed':
      return {
        kind: 'error',
        error: {
          code: error?.code ?? 'UNKNOWN',
          message: error?.message ?? '回合失败',
          ...(error === undefined ? {} : { detail: errorDetail(error) }),
        },
      }
    case 'budget_exhausted':
      return { kind: 'error', error: { code: 'BUDGET_EXHAUSTED', message: '本轮已达到轮次上限' } }
    default:
      return { kind: 'unknown' }
  }
}

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

/**
 * 未接线能力的统一拒绝。
 *
 * 消息会原样出现在界面上，所以只写用户能读懂的话；卡号（docs/05 SH-033/SH-040…）
 * 留在调用点注释与提交信息里。带编号的用户文案会被使用者当成"产品里的神秘代码"，
 * 既不解释发生了什么，也不告诉他能做什么。
 */
function unimplemented(name: string): Promise<never> {
  return Promise.reject(new RemoteError('gateway/internal', `当前版本还不支持这个操作（${name}）`, {}))
}

export function createAdapter(options: AdapterOptions): { remote: unknown; dispose(): void } {
  const { bridge } = options
  const approvalScope = options.approvalScope
  // 回放旧事件时临时指向档案信封里的 `ts_ms`：泳道的时序/时长若取回放墙钟，
  // 重启后的数字会全部趋零，与实时不一致。回放是同步块，不会串到实时事件上。
  let replayClock: number | undefined
  const now = options.now ?? ((): number => replayClock ?? Date.now())
  const journals = new Map<string, Journal>()
  const titles = new Map<string, string>()
  /**
   * compactionId → 触发原因。`CompactionEnded` 不带 trigger（只有 started/summarized 带），
   * 而失败通知要不要插取决于手动还是自动：手动 `/compact` 的结局由命令卡片自己展示，
   * 再插一条就是重复。契约上每次压缩都必然有成对的 started/ended（失败/取消也发 ended），
   * 所以这里不会无界增长。
   */
  const compactionTriggers = new Map<string, CompactionTrigger>()
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
   * 目标域（SH-088）：durable 投影从宿主持久层读（`goal_get`），进程内 activation
   * 与上游一致只活在适配层内存里（armed/disarmed，永不落盘）。`goalLoaded` 区分
   * "还没读过"与"读过、确实没有目标"，避免每次快照都付一次 IPC。
   */
  const goalCache = new Map<string, GoalProjectionWire | null>()
  const goalLoaded = new Set<string>()
  const goalActivations = new Map<string, GoalActivation>()

  /**
   * 子代理泳道登记表（SH-109）。后端没有子会话——子代理是 agent_loop task，事件经
   * `AgentEvent` 信封（`agent_id ≠ 'root'`，`session_id` 仍是宿主任务会话）到达；
   * 适配层把 `agent_id` 合成为泳道的 childSessionId（ULID 全局唯一，`laneIdOf`），
   * 每个泳道一份独立 journal，父 journal 落 `subagent/catalog` 事件。
   *
   * 懒建口径：见到该 agent 的第一条事件才登记——"transport 亲眼见过它在干活"，
   * catalog 事件写 v1 `mode:'unknown'`（上游 v3 不允许 unknown，诚实标注"没拿到
   * spawn 元数据"）。正常路径下 `subagent_spawned` 先于任何活动到达，走
   * `registerSpawned` 直接建 v3 continuable；懒建只是乱序/旧档案的兜底。
   * 泳道行在侧栏/目录树的出现由 session-controller 的 projectList
   * 消费 `subagentCatalog` 投影自动完成，UI 零改动。
   */
  interface AgentLane {
    readonly parentId: string
    /** 父 catalog 的当前事实：懒建为 v1 unknown，拿到 spawn 元数据后升级 v3 continuable。 */
    catalog: SubagentCatalogEventData
    /** 是否已往父 journal 写过 catalog 事件——上游 fold 是 append-only，写第二条会多出一行。 */
    catalogWritten: boolean
    /** 泳道名 / 展示标签 / 任务全文（spawn 元数据；懒建时缺席，不猜）。 */
    name?: string
    label?: string
    task?: string
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
    journal.log.append({
      type: 'event',
      event: {
        type,
        seq: journal.seq,
        time: now(),
        ...extra,
        data,
      },
    })
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

  // ---- 目标域（SH-088）：durable 投影 + 进程内 activation ----

  /** 进程内续跑许可；缺省 disarmed（与上游 runtimeState 的初始值一致）。 */
  const goalActivationOf = (sessionId: string): GoalActivation => goalActivations.get(sessionId) ?? 'disarmed'

  /** 读宿主 durable 目标；读失败按"没有目标"判读（不伪造一个空目标）。 */
  const loadGoal = async (sessionId: string): Promise<GoalProjectionWire | null> => {
    if (goalLoaded.has(sessionId)) return goalCache.get(sessionId) ?? null
    try {
      const projection = await bridge.invoke<GoalProjectionWire | null>('goal_get', { sessionId })
      goalCache.set(sessionId, projection)
      goalLoaded.add(sessionId)
      return projection
    } catch {
      return null
    }
  }

  /** 目标投影整值推帧；clear 交显式 null，客户端据此收起目标条。 */
  const pushGoalFrame = (sessionId: string, projection: GoalProjectionWire | null): void => {
    goalCache.set(sessionId, projection)
    goalLoaded.add(sessionId)
    projectionSeq += 1
    projectionLog.append({
      type: 'projection', sessionId, seq: projectionSeq,
      changes: [{ key: 'goal', val: projection }],
    })
  }

  /** 进程内 activation 边沿：只有真的变化才派发（上游 setActivation 同口径）。 */
  const pushGoalActivation = (
    sessionId: string,
    projection: GoalProjectionWire | null,
    activation: GoalActivation,
  ): void => {
    if (goalActivationOf(sessionId) === activation) return
    goalActivations.set(sessionId, activation)
    dispatch('goal/activation-changed', {
      sessionId,
      ...(projection === null
        ? {}
        : { goal: { id: projection.goal.id, revision: projection.goal.revision, activation } }),
    })
  }

  /** 宿主目标错误 → RemoteResult 失败（code 与上游 GoalErrorCode 同名）。 */
  const goalFailure = (reason: unknown) => {
    const raw = (reason ?? {}) as { code?: unknown; message?: unknown }
    const code = typeof raw.code === 'string' ? raw.code : 'gateway/internal'
    const message = typeof raw.message === 'string' ? raw.message : String(reason)
    return { ok: false as const, error: new RemoteError(code as never, message, {} as never) }
  }

  /** durable 投影 + 进程内 activation → wire 的 GoalView。 */
  const goalViewOf = (sessionId: string, projection: GoalProjectionWire): GoalViewWire => ({
    ...projection.goal,
    roundsStarted: projection.roundsStarted,
    createdAt: projection.createdAt,
    updatedAt: projection.updatedAt,
    activation: goalActivationOf(sessionId),
  })

  /**
   * 目标写操作共用收尾链：先落宿主 durable，再推投影帧，最后按上游口径推 activation
   * 边沿（create/resume → armed，pause/clear → disarmed，edit 保持）。
   */
  const commitGoal = async (
    sessionId: string,
    call: () => Promise<GoalProjectionWire>,
    activation: GoalActivation | 'keep',
  ): Promise<GoalProjectionWire> => {
    const projection = await call()
    pushGoalFrame(sessionId, projection)
    const next = activation === 'keep' ? goalActivationOf(sessionId) : activation
    pushGoalActivation(sessionId, projection, next)
    return projection
  }

  /** 命令面（`/goal`）与 Remote 面共用的目标动词；两边不许各写一份。 */
  const goalActions = {
    read: (sessionId: string): Promise<GoalProjectionWire | null> => loadGoal(sessionId),
    create: (sessionId: string, request: { objective: string; maxGoalRounds?: number }) =>
      commitGoal(sessionId, () => bridge.invoke<GoalProjectionWire>('goal_create', {
        sessionId,
        objective: request.objective,
        ...(request.maxGoalRounds === undefined ? {} : { maxGoalRounds: request.maxGoalRounds }),
      }), 'armed'),
    edit: (sessionId: string, ref: GoalRefWire, request: { objective?: string; maxGoalRounds?: number }) =>
      commitGoal(sessionId, () => bridge.invoke<GoalProjectionWire>('goal_edit', {
        sessionId,
        goalId: ref.id,
        revision: ref.revision,
        ...(request.objective === undefined ? {} : { objective: request.objective }),
        ...(request.maxGoalRounds === undefined ? {} : { maxGoalRounds: request.maxGoalRounds }),
      }), 'keep'),
    pause: (sessionId: string, ref: GoalRefWire) =>
      commitGoal(sessionId, () => bridge.invoke<GoalProjectionWire>('goal_pause', {
        sessionId, goalId: ref.id, revision: ref.revision,
      }), 'disarmed'),
    resume: (sessionId: string, ref: GoalRefWire) =>
      commitGoal(sessionId, () => bridge.invoke<GoalProjectionWire>('goal_resume', {
        sessionId, goalId: ref.id, revision: ref.revision,
      }), 'armed'),
    clear: async (sessionId: string, ref: GoalRefWire): Promise<GoalRefWire> => {
      const tombstone = await bridge.invoke<GoalRefWire>('goal_clear', {
        sessionId, goalId: ref.id, revision: ref.revision,
      })
      pushGoalFrame(sessionId, null)
      pushGoalActivation(sessionId, null, 'disarmed')
      return tombstone
    },
  }

  /**
   * 目标域 Remote 面（SH-088）：`ui-goal` 的 get/create/edit/pause/resume/clear。
   * `get` 与上游一样返回当前视图或 `undefined`（没有目标不是错误）；写操作把宿主
   * 的结构化错误码原样带回（`GOAL_STALE_REVISION` 等）。
   */
  const goalsNamespace = {
    get: async (sessionId: string) => {
      try {
        const projection = await goalActions.read(sessionId)
        return OK(projection === null ? undefined : goalViewOf(sessionId, projection))
      } catch (reason) {
        return goalFailure(reason)
      }
    },
    create: async (sessionId: string, request: { objective: string; maxGoalRounds?: number }) => {
      try {
        const projection = await goalActions.create(sessionId, request)
        return OK({ ref: { id: projection.goal.id, revision: projection.goal.revision } })
      } catch (reason) {
        return goalFailure(reason)
      }
    },
    edit: async (sessionId: string, ref: GoalRefWire, request: { objective?: string; maxGoalRounds?: number }) => {
      try {
        return OK(goalViewOf(sessionId, await goalActions.edit(sessionId, ref, request)))
      } catch (reason) {
        return goalFailure(reason)
      }
    },
    pause: async (sessionId: string, ref: GoalRefWire) => {
      try {
        return OK(goalViewOf(sessionId, await goalActions.pause(sessionId, ref)))
      } catch (reason) {
        return goalFailure(reason)
      }
    },
    resume: async (sessionId: string, ref: GoalRefWire) => {
      try {
        return OK(goalViewOf(sessionId, await goalActions.resume(sessionId, ref)))
      } catch (reason) {
        return goalFailure(reason)
      }
    },
    clear: async (sessionId: string, ref: GoalRefWire) => {
      try {
        return OK(await goalActions.clear(sessionId, ref))
      } catch (reason) {
        return goalFailure(reason)
      }
    },
  }

  const currentModelSelection = (): Record<string, unknown> => ({
    lastUsed: null,
    next: face.activeSelection(),
  })

  /**
   * `permissions` 投影（{currentValue}）：**按会话**读宿主档案里的最后一档，
   * 没有则回落部署默认（`config.toml`）。会话内切档只写本会话档案，不写配置
   * ——这是与设置页那行的分界线（对齐 DSH 的 `sandbox/mode` 会话事件）。
   */
  const sessionPresetCache = new Map<string, string>()
  /**
   * 读宿主档案失败的会话。与「档案没记录档位」（`session_sandbox_mode` 回 null，
   * 此时跟随部署默认是对的）不是一回事：读失败时不能回落到任何具体档，
   * 否则「读不到」会被渲染成「只读」，用户按错误前提去点确认。
   */
  const sessionPresetUnreadable = new Set<string>()

  /** 读宿主 durable 的本会话档位并写入缓存；读失败只记 unknown，不伪造一个档。 */
  const loadSessionPermission = async (sessionId: string): Promise<void> => {
    if (sessionPresetCache.has(sessionId)) return
    try {
      const mode = await bridge.invoke<string | null>('session_sandbox_mode', { sessionId })
      sessionPresetCache.set(sessionId, presetOfSandbox(mode ?? face.sandboxMode()))
      sessionPresetUnreadable.delete(sessionId)
    } catch {
      sessionPresetUnreadable.add(sessionId)
    }
  }

  const currentPermission = (sessionId: string): string => {
    const known = sessionPresetCache.get(sessionId)
    if (known !== undefined) return known
    return sessionPresetUnreadable.has(sessionId)
      ? PERMISSION_UNAVAILABLE
      : presetOfSandbox(face.sandboxMode())
  }
  const permissionsProjection = (sessionId: string): Record<string, unknown> => ({
    currentValue: currentPermission(sessionId),
  })

  /** 只给**目标会话**推一帧：A 会话切档不许改到 B 的芯片（§4.2 反向对照）。 */
  const pushPermission = (sessionId: string): void => {
    projectionSeq += 1
    projectionLog.append({
      type: 'projection', sessionId, seq: projectionSeq,
      changes: [{ key: 'permissions', val: permissionsProjection(sessionId) }],
    })
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
    for (const lane of agents.values()) {
      if (lane.parentId !== parentId) continue
      entries.push(catalogEntryOf(lane.catalog))
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
    const catalog: SubagentCatalogEventData = {
      version: 1, childId: laneIdOf(agentId), childCreatedAt: now(), mode: 'unknown',
    }
    agents.set(agentId, { parentId, catalog, catalogWritten: true, timing: initTimingState() })
    pushEvent(journalOf(parentId), 'subagent/catalog', catalog)
    pushCatalogFrame(parentId)
  }

  /**
   * 泳道 timing 推进。turn 开合时推投影帧；其余事件只折叠不推帧——运行中泳道的
   * 时长由 UI 实时钟读（`activityDuration` 对 running 泳道取 now），`active.through`
   * 只在"回合中断在半路"的收口展示里用到，逐 delta 推帧是纯浪费。
   */
  const foldLaneTiming = (laneId: string, eventType: 'turn/start' | 'turn/end' | 'advance' | 'subagent/descriptor', reasonKind?: string): void => {
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

  /**
   * spawn 元数据到达（SH-053）：建/升级泳道为 v3 continuable，父 journal 落 catalog、
   * 子 journal 落 descriptor 与任务全文，并点亮运行态。
   *
   * 上游 catalog fold 是 **append-only**（`catalog.ts` 的 appendChunkedList，不按 childId
   * 去重），所以已经懒建写过 v1 的泳道只升级内存事实 + 推投影帧，不追加第二条 catalog
   * ——否则目录树会多出一行。后端保证 spawned 先于子代理任何活动，这条兜底只在乱序时走到。
   */
  const registerSpawned = (parentId: string, msg: Extract<EventMsg, { type: 'subagent_spawned' }>): void => {
    const laneId = msg.child_id
    const lane: AgentLane = agents.get(laneId)
      ?? { parentId, catalog: catalogV3Of(laneId, msg.created_at_ms, msg.label, msg.name), catalogWritten: false, timing: initTimingState() }
    lane.catalog = catalogV3Of(laneId, msg.created_at_ms, msg.label, msg.name)
    lane.name = msg.name
    lane.label = msg.label
    lane.task = msg.task
    agents.set(laneId, lane)

    if (!lane.catalogWritten) {
      pushEvent(journalOf(parentId), 'subagent/catalog', lane.catalog)
      lane.catalogWritten = true
    }
    pushCatalogFrame(parentId)

    const journal = journalOf(laneId)
    pushEvent(journal, 'subagent/descriptor', descriptorOf(msg.label))
    foldLaneTiming(laneId, 'subagent/descriptor')
    // 回合在这里开（与 submitPromptNow 同序）：任务行必须落在回合内，否则回合头不显示用时。
    // 后端随后的第一条 turn_started 因 taskOpen 已真而成为空操作，timing 由这里折叠。
    if (!journal.taskOpen) openTask(journal)
    dispatch('api-session/status', laneId, true)
    pushTurnStart(laneId, journal)
    pushEvent(journal, 'step/start', { turn: journal.turn, step: journal.step })
    beginStepMetrics(journal)
    foldLaneTiming(laneId, 'turn/start')
    pushEvent(journal, 'user/message', {
      id: `shaniu-lane-task-${journal.seq + 1}`,
      role: 'user',
      content: [{ type: 'text', text: msg.task }],
      source: relaySourceOf(parentId),
    }, { surfaceOp: 'append' })
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
        ...imageContentBlocks(item.images ?? []),
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
          await bridge.invoke<string>('send_task', {
            task: item.text,
            agentId: sessionId,
            ...(item.images === undefined || item.images.length === 0
              ? {}
              : { images: toSendTaskImages(item.images) }),
          })
          sent = true
        } catch (reason) {
          // 视觉门禁不是瞬时拒绝：重试五次也不会通过。这条已从队列取出，直接丢弃
          // 并留一条通知（对齐 `rollout_degraded` 的 `developer/message` 形态）。
          if (isModelNoVision(reason)) {
            const text = hostErrorText(reason)
            pushEvent(journal, 'developer/message', {
              turn: journal.turn,
              step: journal.step,
              message: {
                id: `shaniu-novision-${item.id}`,
                role: 'developer',
                source: { kind: 'model-no-vision' as const, form: 'notice' as const, summary: text },
                content: [{ type: 'text', text }],
              },
            }, { surfaceOp: 'append' })
            return
          }
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
        modelSelection: currentModelSelection(), title: null, permissions: permissionsProjection(id),
        ...(planProjectionValues()),
        // 新会话不可能已有目标；显式 null 让客户端进入"无目标"态而不是加载态。
        goal: null,
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
  const replayArchive = (entries: readonly SessionEntryLike[]): Journal => {
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
    const closeTurn = (reason: Record<string, unknown> = { kind: 'unknown' }): void => {
      closeStep()
      if (!journal.taskOpen) return
      pushEvent(journal, 'turn/end', { turn: journal.turn, reason })
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

    // 回放期间所有事件时刻都来自档案；没有任何条目时刻时是 0，读侧据此显示
    // 「记录时刻不可用」，绝不回落到打开会话那一刻（A0-4）。
    replayClock = 0
    try {
      entries.forEach((entry, index) => {
        // 每条档案条目带自己的落档时刻：本条事件取它，不再取打开会话那一刻。
        replayClock = 'at_ms' in entry && (entry.at_ms ?? 0) > 0 ? entry.at_ms : 0
        if (entry.kind === 'turn_end') {
          // 结局行：新档案才有；旧档案走到末尾时 closeTurn() 会落 unknown。
          if (entry.turn === journal.turn) closeTurn(archiveTurnReason(entry.reason, entry.error))
          journal.entryToSeq[index] = journal.seq
          return
        }
        if (entry.kind === 'message' && entry.role === 'user') {
          startTurn()
          openStep(0)
          // 新档案带结构化 `images`；旧档案只有正文里的 marker，按旧形态解码。
          const archived = (entry.images ?? [])
            .map(archiveImageToPrompt)
            .filter((image): image is PromptImageRef => image !== undefined)
          const parsed = archived.length === 0
            ? decodeImageMarkers(entry.text)
            : { text: entry.text, images: archived }
          pushEvent(journal, 'user/message', {
            id: `shaniu-replay-user-${index}`,
            role: 'user',
            content: [
              ...imageContentBlocks(parsed.images),
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
      // 末尾收口：取最后一条条目的时刻；旧档案没有 at_ms → 0（读侧显示「记录时刻不可用」）。
      closeTurn()
    } finally {
      replayClock = undefined
    }
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
    // 来源代理与任务摘要：优先用后端事件里的权威字段（ADR-103）；
    // 泳道元数据只在事件没带字段时兜底。泳道未建立时反推必然失败——那正是
    // "子代理的审批显示在父时间线上、却看不出是谁在要权限"的根因。
    const lane = agents.get(sessionId)
    const childId = msg.agent_id !== undefined && msg.agent_id !== 'root' ? msg.agent_id : undefined
    const originName = lane?.name ?? childId
    const originTask = msg.task_summary ?? lane?.label ?? lane?.task
    const origin = originName === undefined
      ? ''
      : `子代理 ${originName}${originTask === undefined ? '' : `（${originTask}）`}：`
    // 标题只回答「是什么」：来源 + 风险 + 工具 + 一句话对象。
    // 完整参数不进标题——`JSON.stringify` 把换行转义成 `\n`（两位字符），任何有限长度
    // 都会在多文件补丁/长命令上截断（spec §1.1）。原文由详情槽按 `call_id` 取。
    const title = `${origin}风险 ${msg.risk}：${msg.tool}${approvalSubject(msg)}`
    // 正文回答「为什么现在问」和「批准覆盖多远」：`msg.reason` 是后端给的原因，
    // `batch_scope` 是范围标识（措辞由前端映射，中文不进协议）。两者互补，
    // 原因已经说过的内容不再重复一次。
    const scopeText = batchScopeText(msg.batch_scope)
    const reasonText = msg.reason?.trim()
    const description = [reasonText, scopeText]
      .filter((part): part is string => part !== undefined && part !== '')
      .join(' ') || undefined
    let decision = 'deny'
    try {
      const outcome = await listener.call(approvalScope?.(sessionId), {
        agent: { id: sessionId },
        toolName: msg.tool,
        reason: title,
        displayReason: { en: title },
        ...(msg.call_id === undefined ? {} : { callId: msg.call_id }),
        ...(description === undefined ? {} : { description }),
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

  /**
   * 允许列表连续放行的合并计数（`allowlist_hit`）。
   *
   * 为什么合并：长任务里同一条前缀规则可能连续命中几十次，每次插一条通知会把
   * 时间线刷满，用户最后会把这类提示当噪音关掉。合并口径是「同一回合 + 同一工具」：
   * 工具变化时先结算上一条，回合收口时结算最后一条；跨回合不合并（文案说的是
   * 「本轮」，跨回合合并会让用户以为本回合也放行了）。
   */
  const allowlistPending = new Map<string, { tool: string; count: number }>()

  /** 结算并落一条通知；没有待结算项时是空操作。 */
  const flushAllowlist = (sessionId: string, journal: Journal): void => {
    const pending = allowlistPending.get(sessionId)
    if (pending === undefined) return
    allowlistPending.delete(sessionId)
    const text = pending.count === 1
      ? `系统已自动放行 ${pending.tool}：命中本次会话的允许列表，未弹审批。`
      : `本轮 ${pending.count} 次 ${pending.tool} 已自动放行：命中本次会话的允许列表，未弹审批。`
    pushEvent(journal, 'developer/message', {
      turn: journal.turn,
      step: journal.step,
      message: {
        id: `shaniu-allowlist-${journal.seq + 1}`,
        role: 'developer',
        source: { kind: 'allowlist-notice' as const, form: 'notice' as const, summary: text },
        content: [{ type: 'text', text }],
      },
    }, { surfaceOp: 'append' })
  }

  const onEvent = (event: AgentEvent): void => {
    const msg = event.msg
    // 子代理事件（agent_id ≠ 'root'）路由进该 agent 的泳道 journal（懒建，SH-109）；
    // session_id 仍是宿主侧任务会话（泳道的父）。根事件走原路径。
    const laneId = event.agent_id !== '' && event.agent_id !== 'root' && event.agent_id !== event.session_id
      ? event.agent_id
      : undefined
    const sessionId = laneId ?? event.session_id
    if (msg.type === 'subagent_spawned') {
      // 元数据先于活动到达：直接建 v3 泳道，不落 unknown 的懒建中间态（catalog 是 append-only）。
      registerSpawned(event.session_id, msg)
      return
    }
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
      case 'allowlist_hit': {
        // 放行依据必须可追溯：不弹窗不等于不留痕。这里只累积计数（见 allowlistPending），
        // 结算成一条通知——逐条落会把长任务的时间线刷满，用户最终会把提示关掉。
        const pending = allowlistPending.get(sessionId)
        if (pending !== undefined && pending.tool === msg.tool) {
          pending.count += 1
          return
        }
        flushAllowlist(sessionId, journal)
        allowlistPending.set(sessionId, { tool: msg.tool, count: 1 })
        return
      }
      case 'turn_completed': {
        // 跨事件不结算（同一工具的执行结果夹在两次放行之间），只在本回合收口时
        // 把最后一条累积落下来。
        flushAllowlist(sessionId, journal)
        // 审计 A0：失败回合必须带上结构化原因（如 401 → AUTH）。导出的档案里
        // 由此能看出"为什么失败"；只有后端没给结构时，才退回最后兜底文案。
        const failure = msg.reason === 'completed'
          ? undefined
          : msg.error === undefined
            ? { message: `回合以 ${msg.reason} 结束`, code: msg.reason }
            : {
                message: msg.error.message,
                code: msg.error.code,
                detail: errorDetail(msg.error),
              }
        // cancelled 的半截正文是真实发生过的输出：落进档案，但打 interrupted，
        // 不能当成正常答完。
        closeTask(sessionId, journal, msg.reason === 'completed'
          ? { kind: 'completed' }
          : msg.reason === 'cancelled'
            ? { kind: 'aborted', reason: { kind: 'user' } }
            : { kind: 'error', error: failure },
        msg.reason === 'cancelled')
        if (laneId !== undefined) foldLaneTiming(laneId, 'turn/end', msg.reason)
        // 运行态必须收回：输入卡的 Stop 按钮由 `api-session/status` 决定（`running`
        // 为真才显示），只发 journal 的 turn/end 不发状态，按钮会永远停在发送态。
        dispatch('api-session/status', sessionId, false)
        // 闸随任务协程收口释放（晚于本事件）：排队消息在这里按序补发，对短时拒绝重试。
        void drainQueues()
        return
      }
      case 'subagent_exited': {
        // 结算通知落**父** journal（上游 createSettlementMessage 的位置）：用户在父会话里
        // 看到"哪个子代理、怎么结束、收尾说了什么"，泳道自身由 turn_completed 收口。
        const lane = agents.get(msg.child_id)
        const parentId = lane?.parentId ?? event.session_id
        const parentJournal = journalOf(parentId)
        const notice = settledNoticeOf(msg.child_id, lane?.name ?? msg.child_id, msg.status, msg.summary)
        pushEvent(parentJournal, 'user/message', {
          id: `shaniu-settled-${msg.child_id}-${parentJournal.seq + 1}`,
          role: 'user',
          content: notice.content,
          source: notice.source,
        }, { surfaceOp: 'append' })
        // 排队阶段就被取消的子代理没有 turn_completed，运行态仍必须在这里收回，
        // 否则目录树徽标与泳道 Stop 按钮永远停在运行中。
        dispatch('api-session/status', msg.child_id, false)
        return
      }
      case 'subagent_message': {
        // steering 落**子**泳道的 user/message：UI 注入带 rpc_id（source=user+rpcId，
        // 客户端据此撤销乐观回显），工具/父代理注入落 agent-message/relay。
        // 父 journal 不被污染——泳道点开才看得到中途改了什么。
        const laneJournal = journalOf(msg.child_id)
        pushEvent(laneJournal, 'user/message', {
          id: msg.rpc_id ?? `shaniu-steer-${msg.child_id}-${laneJournal.seq + 1}`,
          role: 'user',
          content: [{ type: 'text', text: msg.text }],
          source: msg.rpc_id === undefined || msg.rpc_id === null
            ? relaySourceOf(msg.sender_id)
            : { kind: 'user' as const, rpcId: msg.rpc_id },
        }, { surfaceOp: 'append' })
        return
      }
      case 'rollout_degraded':
        // 观测失败不冒充失败：对话照常收口，这里只插一条宿主通知（form: 'notice'，
        // 与上游 ContextForm 的一次性告知同义），不参与模型请求。
        pushEvent(journal, 'developer/message', {
          turn: journal.turn,
          step: journal.step,
          message: {
            id: `shaniu-degraded-${journal.seq + 1}`,
            role: 'developer',
            source: { kind: 'archive-degraded' as const, form: 'notice' as const, summary: msg.message },
            content: [{ type: 'text', text: msg.message }],
          },
        }, { surfaceOp: 'append' })
        return
      case 'compaction_started':
        // 有损操作必须出声（对齐上游 `compaction/start`）：起点先落 journal，之后
        // 无论成功、失败还是取消，时间线上都能回答「这次上下文为什么变短」。
        compactionTriggers.set(msg.compaction_id, msg.trigger)
        pushEvent(journal, 'compaction/start', {
          turn: journal.turn,
          compactionId: msg.compaction_id,
        })
        return
      case 'compaction_summarized': {
        // 摘要与「丢了什么」同一条（对齐上游 `compaction/summary`）。上游的
        // `shadowedSeqs` 是会话 seq 集合，我们没有等价物——不编造，只给真实的
        // token 数与摘要；条数由下面的通知说清楚。
        pushEvent(journal, 'compaction/summary', {
          turn: journal.turn,
          compactionId: msg.compaction_id,
          summary: [{ type: 'text', text: msg.summary }],
          shadowedTokenCount: msg.shadowed_tokens,
        })
        // 手动 `/compact` 由命令卡片自己展示结果，不重复插通知；自动压缩在聊天里
        // 没有别的痕迹，不补这一行用户只会发现「上下文忽然变短」。
        if (msg.trigger === 'pressure') {
          const text = `上下文已压缩：${msg.shadowed_items} 条历史（约 ${msg.shadowed_tokens} token）`
            + '被一条摘要取代，原文仍保留在会话档案里。'
          pushEvent(journal, 'developer/message', {
            turn: journal.turn,
            step: journal.step,
            message: {
              id: `shaniu-compacted-${msg.compaction_id}`,
              role: 'developer',
              source: { kind: 'compaction-notice' as const, form: 'notice' as const, summary: text },
              content: [{ type: 'text', text }],
            },
          }, { surfaceOp: 'append' })
        }
        return
      }
      case 'compaction_ended': {
        // 成败必须分流，不能只看 `error` 是否非空：后端「写失败 → 回滚内存历史」的方案
        // 会发 `ok:false` 且不带 `error`，而 UI 只认 `error` 判成败
        // （trajectory-compaction-definition.ts：status = error === undefined ? 'complete' : 'error'）。
        // 只透传 `error` 的话，刚被显式化的失败会在前端被抹平成正常结束。
        const trigger = compactionTriggers.get(msg.compaction_id)
        compactionTriggers.delete(msg.compaction_id)
        const reason = typeof msg.error === 'string' && msg.error.trim() !== '' ? msg.error : undefined
        const failure = msg.ok ? undefined : (reason ?? '宿主未给出原因')
        pushEvent(journal, 'compaction/end', {
          turn: journal.turn,
          compactionId: msg.compaction_id,
          // 失败但宿主没给原因时也必须给一句话：`compaction/end` 的 `error` 是唯一的
          // 失败信号，空着等于把失败说成成功。
          ...(failure === undefined ? {} : { error: reason ?? `压缩未生效（${failure}）` }),
        })
        // 手动 `/compact` 的结局由命令卡片（`command/done`）自己展示，不再重复；
        // 自动压缩没有别的痕迹，失败时不补这一行，用户只会发现「上下文没变短」。
        if (failure !== undefined && trigger !== 'manual') {
          const text = `上下文压缩未生效：${failure}。历史保持原样，可以继续对话。`
          pushEvent(journal, 'developer/message', {
            turn: journal.turn,
            step: journal.step,
            message: {
              id: `shaniu-compact-failed-${msg.compaction_id}`,
              role: 'developer',
              source: { kind: 'compaction-notice' as const, form: 'notice' as const, summary: text },
              content: [{ type: 'text', text }],
            },
          }, { surfaceOp: 'append' })
        }
        return
      }
      case 'system_prompt':
        // 提示词入轨迹：映射成上游的 `system/message`，只在该版本首次出现时由后端发出。
        // 用 `journal.turn/step`（DSH 口径）而不是后端的采样轮次：后者是 turn 内的
        // 第几次请求，直接写进 turn 会让卡片显示错误的会话轮次（同 tool/call 的折叠约定）。
        // `append` 语义由上游 `inspectSystemPrompt` 解释：第一张为初始提示词，之后自动标
        // 「系统提示词已更新」。
        // 文本已在后端 journal 落盘前统一脱敏；这里不做二次处理，否则展示的就不是模型
        // 实际收到的那份。
        pushEvent(journal, 'system/message', {
          turn: journal.turn,
          step: journal.step,
          message: {
            id: `shaniu-system-${journal.seq + 1}`,
            role: 'system',
            source: { kind: 'system-prompt' as const },
            content: [{ type: 'text', text: msg.text }],
          },
        }, { surfaceOp: 'append' })
        return
      case 'model_retry':
        // 重试不是新 UI：上游 `RetryNodeView` 由两个 durable 会话事件驱动——
        // `llm/retry`（即将等待）与 `llm/retry-started`（等待结束、已重发）。
        // 后端把三阶段压成一条 `model_retry`，这里只做翻译。
        // `exhausted` 刻意不落 journal：上游没有对应事件，且随后的
        // `turn_completed(failed)` 已给出终局原因，多落一条会在时间线上留个悬空节点。
        if (msg.phase === 'scheduled') {
          pushEvent(journal, 'llm/retry', {
            retryId: msg.retry_id,
            turn: journal.turn,
            // 后端的一次 turn 对应 DSH 的一个 step（见 turn_started 的折叠注记）。
            step: journal.step,
            provider: 'shaniu',
            mode: 'normal',
            policyKey: 'default',
            retry: msg.retry,
            maxRetries: msg.max_retries,
            delayMs: msg.delay_ms,
            failure: { message: msg.message, code: msg.code },
          })
        } else if (msg.phase === 'started') {
          pushEvent(journal, 'llm/retry-started', {
            retryId: msg.retry_id,
            turn: journal.turn,
            step: journal.step,
            retry: msg.retry,
          })
        }
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

  /**
   * 整树回放（SH-056 批 1）：宿主的父会话事件档案按 `agent_id` 记全了子代理事件，
   * 重开旧会话时把子代理事件再走一遍**与实时相同**的 [`onEvent`] 路由，泳道的
   * catalog / descriptor / 任务全文 / 正文 / 工具卡 / 收口就按档案重建出来。
   *
   * 三条刻意口径：
   * - **父事件跳过**：父时间线已由 rollout 档案重建（事件档案没有用户消息），两边
   *   都放会重复渲染；
   * - **时间取事件信封的 `ts_ms`**：见 `replayClock`，否则泳道时长全部趋零；
   * - **结算通知由 `subagent_exited` 重新合成**：落点在同一条父 journal 的末尾，
   *   而不是实时路径的原位——rollout 条目没有时间戳，无法可靠插回原位（docs/05
   *   SH-056 已声明该偏差）。
   *
   * 事件档案读失败只降级为"泳道不重建"，不把整个会话打成打不开：父时间线仍然从
   * rollout 档案重建，失败原因留在控制台，避免把可用的主视图一起罚掉。
   */
  const replayLanes = async (sessionId: string): Promise<void> => {
    let after: string | undefined
    for (;;) {
      const page = await bridge.invoke<{ events: AgentEvent[]; has_more: boolean }>('read_session_events', {
        sessionId,
        ...(after === undefined ? {} : { afterEventId: after }),
        limit: 500,
      }).catch((reason: unknown) => {
        console.warn('[SHANIU] 子代理事件回放失败：泳道无法从档案重建', reason)
        return undefined
      })
      if (page === undefined) return
      for (const event of page.events) {
        if (event.agent_id === '' || event.agent_id === 'root' || event.agent_id === event.session_id) continue
        replayClock = event.ts_ms
        try {
          onEvent(event)
        } finally {
          replayClock = undefined
        }
      }
      const next = page.events.at(-1)?.event_id
      if (!page.has_more || next === undefined || next === after) return
      after = next
    }
  }

  /**
   * 从 rollout 档案重建一个会话的 journal（SH-033；SH-056 批 2 从 `follow` 抽出）。
   *
   * 两条入口共用同一套口径：跟随父会话（重启后重开旧会话）与跟随泳道（子代理
   * 没有自己的 rollout，先反查父会话再整树回放）。返回 `undefined` = 档案为空，
   * 调用方决定交空快照还是继续尝试别的来源。
   *
   * 标题回填同样只在这里做一次：宿主 `list_sessions` 的 title 已合并自定义标题，
   * 本地再从档案派生会把它盖掉（重启后用户改过的名字就丢了）。
   */
  const rebuildFromArchive = async (sessionId: string): Promise<Journal | undefined> => {
    const entries = await bridge.invoke<SessionEntryLike[]>('load_session', { agentId: sessionId })
    if (entries.length === 0) return undefined
    const replayed = replayArchive(entries)
    journals.set(sessionId, replayed)
    if (!titles.has(sessionId)) {
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
    // 泳道从父会话的事件档案按 agent_id 折叠重建（SH-056 批 1）。
    await replayLanes(sessionId)
    return replayed
  }

  /**
   * 反查子代理的归属会话（SH-056 批 2）。宿主侧是遍历事件档案的 IO 扫描；
   * 失败时如实返回 null（警告留痕），调用方据此决定抛原始错误还是交空态。
   */
  const ownerSessionOf = async (agentId: string): Promise<string | null> =>
    bridge.invoke<string | null>('session_of_agent', { agentId }).catch((reason: unknown) => {
      console.warn('[SHANIU] 子代理归属反查失败：泳道无法直达恢复', reason)
      return null
    })

  /**
   * 档案重建后的首个快照与后续帧（`follow` 的两条入口共用）。
   *
   * 必须在取 `from` 之前完成泳道回放：结算通知会追加进父 journal，晚取会把它们
   * 排除在快照之外。
   */
  const followReplayed = async function* (
    sessionId: string,
    replayed: Journal,
    signal?: AbortSignal,
  ): AsyncGenerator<Record<string, unknown>> {
    const from = replayed.log.length
    traceFollow(sessionId, 'snapshot-replayed', { from, revision: replayed.revision, records: replayed.log.eventsUpTo(from).length })
    // 档位先读进缓存再产快照：读值只有一个来源（`currentPermission`），
    // 失败时它就是 unknown 哨兵，不会在这里另算一份。
    await loadSessionPermission(sessionId)
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
          permissions: permissionsProjection(sessionId),
          ...(planProjectionValues()),
          // 档案重建出的任务清单（todo/write 推导）随快照带出：重开旧会话
          // TodoDock 直接回到最后一份清单（上游回放语义）。
          ...todosProjectionValues(replayed),
          // 重建已折叠出步/回合计数：重开旧会话底部指标立即有值（时间不计，见
          // replay 的注记）；旧档案没有用量，tokenUsage/contextPressure 不挂。
          ...metricsProjectionsOf(replayed),
          // 泳道位：父会话挂 subagentCatalog、泳道挂 subagentTiming（缺席=无数据）。
          ...subagentProjectionsOf(sessionId),
          // 目标投影：重开旧会话时从宿主持久层读回当前目标（没有则 null）。
          goal: await loadGoal(sessionId),
        },
      },
      assistantStream: { revision: replayed.revision },
    }
    try {
      for await (const frame of replayed.log.tail(from, signal)) yield frame
    } finally {
      traceFollow(sessionId, 'end-replayed')
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
    // 新会话不继承任何会话的档位：不带 `sandboxMode`，由宿主按部署默认建档
    // （`send_task(sandbox_mode: None)` → config 的 `[sandbox] mode`）。
    // 用户要给新会话提权，得在新会话里自己设一次——这是有意的摩擦，
    // 跨会话传档等于把「这一次提权」变成永久提权（授权作用域越界）。
    const agentId = await bridge.invoke<string>('send_task', {
      task: text,
      agentId: sessionId,
      // 图片引用走结构化参数（对齐 DSH 的 admission：UI 只交引用，宿主读字节并校验）；
      // 无图时不发这个键，`{ task, agentId }` 的既有形状不变。
      ...(images.length === 0 ? {} : { images: toSendTaskImages(images) }),
    })
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
        ...imageContentBlocks(images),
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
      // 补发上游的 `api-session/added`（session-controller 在会话创建时广播的那条）：
      // 界面新建会话只传 workspaceId，`manager.create` 的占位行因此**没有 cwd**，
      // 而资源管理器按钮与官方 `workspace.openLocal` 都以列表行的 cwd 为前置条件——
      // 少了这条事件，新会话在本次运行内一直"点了没反应"，要重开窗口（list 刷新）才补上。
      // 复用空白会话时 rows 里已有该行，按它的 blank / 工作区如实上报。
      const reused = rows.find(row => row.agentId === agentId)
      dispatch('api-session/added', {
        agentAvailable: true,
        sessionId: agentId,
        cwd: reused?.workspaceRoot ?? cwd,
        updatedAt: Date.now(),
        running: journals.get(agentId)?.taskOpen === true,
        blank: reused?.blank ?? true,
        formatStatus: 'current',
      })
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
      try {
        await submitPromptNow(request.sessionId, text, request.requestId, composed.displayText, composed.images)
      } catch (reason) {
        // 视觉门禁等结构化错误必须回到输入区，而不是变成未处理的 rejection：
        // 后端没接任务，前端也不该留下乐观回显。
        return FAIL(hostErrorText(reason))
      }
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
      // M3：本视图持有的订阅是引用计数对（observe/release）。退出有三条路径——正常
      // return、消费方 .return() 关闭生成器、抛错——都在 finally 里释放，否则 gateway
      // 的订阅表只增不减（页面切换后仍在为看不见的会话推帧）。
      const held: string[] = []
      const hold = (id: string): void => {
        if (id === '' || held.includes(id)) return
        held.push(id)
        bridge.observeEvents?.(id)
      }
      try {
        hold(sessionId)
        let journal = journals.get(sessionId)
        if (journal === undefined) {
          // 没在当前进程里观察过：能从档案重建就重建（SH-033 的"打开旧会话"）。
          // 宿主对"档案不存在"是**抛错**而不是空表（load_session 的诚实失败），
          // 子代理正好走这一支——先记下错误，反查不到归属再把原始错误抛回去。
          let replayed: Journal | undefined
          let failure: unknown
          try {
            replayed = await rebuildFromArchive(sessionId)
          } catch (error: unknown) {
            failure = error
          }
          if (replayed !== undefined) {
            yield* followReplayed(sessionId, replayed, signal)
            return
          }
          // 重启后直接打开泳道（SH-056 批 2）：子代理没有自己的 rollout 档案，
          // 它的全部事件记在父会话的事件档案里。反查归属会话 → 重建父整树 →
          // 泳道 journal 由 `replayLanes` 经同一 `onEvent` 建出来，然后落到下面的
          // 实时快照分支（它带 activeAttempt 基线，比档案快照更能处理"档案断在流
          // 中途"的泳道）。
          const ownerId = await ownerSessionOf(sessionId)
          if (ownerId !== null && ownerId !== sessionId) {
            // 子代理事件经父会话频道广播（gateway 按信封 `session_id` 分发）：只订阅
            // 泳道 id 收不到任何帧，这里把父会话一并补订阅。
            hold(ownerId)
            if (!journals.has(ownerId)) await rebuildFromArchive(ownerId)
            // 父 rollout 为空也必须回放：泳道的持久事实在事件档案里，不在 rollout 里。
            if (!journals.has(sessionId)) await replayLanes(ownerId)
            journal = journals.get(sessionId)
          }
          if (journal === undefined) {
            // 真实会话的档案读失败仍是硬错误：不静默交空态让用户以为会话没了。
            if (failure !== undefined) throw failure
            traceFollow(sessionId, 'end-empty')
            yield emptySnapshot(sessionId, now())
            return
          }
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
        await loadSessionPermission(sessionId)
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
              permissions: permissionsProjection(sessionId),
              ...(planProjectionValues()),
              // 有排队消息才挂 inbox：QueueDock/steer 占位都按投影缺席=无队列判读。
              ...(journal.queue.length > 0
                ? { inbox: { 'next-turn': journal.queue.map(queueRowOf) } }
                : {}),
              ...todosProjectionValues(journal),
              ...metricsProjectionsOf(journal),
              // 泳道位：父会话挂 subagentCatalog、泳道挂 subagentTiming（缺席=无数据）。
              ...subagentProjectionsOf(sessionId),
              // 目标投影：每次快照都带上当前 durable 目标（没有则 null）。
              goal: await loadGoal(sessionId),
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
      } finally {
        for (const id of held) bridge.releaseEvents?.(id)
      }
    },
    control: async function* (signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> {
      await face.refresh()
      const from = projectionLog.length
      // 目标值来自宿主持久层（异步读），先把所有会话的目标读齐再产基线。
      const goalById = new Map(await Promise.all(
        [...journals.keys()].map(async id => [id, await loadGoal(id)] as const),
      ))
      // 会话档位同理：每个会话的档位来自它自己的档案，先把缓存读齐再产基线。
      await Promise.all([...journals.keys()].map(id => loadSessionPermission(id)))
      yield {
        type: 'baseline',
        value: {
          projections: Object.fromEntries([...journals.entries()].map(([id, journal]) => [id, {
            asOfSeq: 0,
            values: {
              modelSelection: currentModelSelection(),
              title: titles.get(id) ?? null,
              permissions: permissionsProjection(id),
              ...(planProjectionValues()),
              ...(journal.queue.length > 0
                ? { inbox: { 'next-turn': journal.queue.map(queueRowOf) } }
                : {}),
              ...todosProjectionValues(journal),
              ...metricsProjectionsOf(journal),
              // 泳道位：父会话挂 subagentCatalog、泳道挂 subagentTiming（缺席=无数据）。
              ...subagentProjectionsOf(id),
              goal: goalById.get(id) ?? null,
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
    page: async (request: {
      sessionId?: string
      throughSeq?: number
      beforeSeq?: number
      maxMessages?: number
    } = {}) => {
      const journal = request.sessionId === undefined ? undefined : journals.get(request.sessionId)
      // `SessionPage` 只有 records/hasMore：cursor 是 `SessionEventsPage` 的字段，
      // 这里多返回一个客户端不认识的键，等于让「契约之外的事实」先存在再说。
      if (journal === undefined) return OK({ records: [], hasMore: false })
      const through = request.throughSeq ?? journal.seq
      const before = request.beforeSeq
      const pool = journal.log
        .eventsUpTo(journal.log.length)
        .filter((entry) => {
          const seq = FrameLog.eventSeq(entry)
          // beforeSeq 排他（它指向「当前已加载的最早一条」，不能重复返回）
          return seq <= through && (before === undefined || seq < before)
        })
      const maxMessages = request.maxMessages
      if (maxMessages === undefined || maxMessages <= 0 || pool.length === 0) {
        return OK({ records: pool, hasMore: false })
      }
      // 从末尾倒扫，数到第 maxMessages 条 append 消息就停；切割点再退到该回合的
      // `turn/start`，否则工具调用会和它的结果落在两页里，客户端只能看到半个回合。
      let cut = pool.length
      let count = 0
      for (let index = pool.length - 1; index >= 0; index--) {
        const event = pool[index]!['event'] as { type?: string; surfaceOp?: string }
        if (event.surfaceOp !== 'append' || (event.type !== 'user/message' && event.type !== 'assistant/message')) continue
        count += 1
        if (count < maxMessages) continue
        cut = index
        for (let back = index; back >= 0; back--) {
          if ((pool[back]!['event'] as { type?: string }).type === 'turn/start') {
            cut = back
            break
          }
        }
        break
      }
      return OK({ records: pool.slice(cut), hasMore: cut > 0 })
    },
    projections: async () => OK(null),
    search: async (request: { query?: string; retry?: boolean } = {}) => {
      // 界面「重试」先清掉宿主的失败记忆；这一步必须在空 query 短路之前，
      // 否则带着 retry 的空请求会直接返回，冷却永远解不开。
      if (request.retry === true) {
        try {
          await bridge.invoke('retry_session_index')
        } catch (reason) {
          return FAIL(`重试索引失败：${hostErrorText(reason)}`)
        }
      }
      const query = (typeof request.query === 'string' ? request.query : '').trim()
      // 空/纯空白按上游语义回空表（WorkspaceBrowser 自己也会短路空 query），
      // 且不该为它付宿主 SQLite 打开成本。
      if (query === '') return OK({ items: [], hasMore: false, skippedSessions: 0 })
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
        // skippedSessions 同理：只驱动一行「N 个会话无法检索」，表示这次结果不完整。
        return OK({
          items,
          hasMore: page?.hasMore === true,
          skippedSessions: typeof page?.skippedSessions === 'number' ? page.skippedSessions : 0,
        })
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
    // 未接线成员（docs/05）：状态/等待/事件 = SH-033，观察面 = SH-094。
    state: () => unimplemented('session.state'),
    wait: () => unimplemented('session.wait'),
    events: () => unimplemented('session.events'),
    observeEvent: () => unimplemented('session.observeEvent'),
    observeProjections: () => unimplemented('session.observeProjections'),
    observeState: () => unimplemented('session.observeState'),
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
        defaultPreset: presetOfSandbox(face.sandboxMode()),
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
      definitionId: CommandDefinitionId('@deepseek-ai/dsh-command-goal'),
      name: 'goal',
      description: 'Set or view the goal for a long-running task',
      // 上游允许附件随目标提交；桌面壳没有把附件编入目标轮的通道，如实声明不接受。
      input: { hint: '[<objective>|clear|edit <objective>|pause|resume]', attachments: false },
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
      // 消息交给模型；bare → 进入。PlanState 按会话存（宿主以 sessionId 为键），
      // 工具路径与命令路径写同一份状态，芯片投影经 setPlanActive 即时随行。
      if (/^\/plan\b/.test(input)) {
        const message = input.replace(/^\/plan\b/, '').trim()
        if (message === 'off') {
          const run = pushCommandRun(sessionId, 'plan', 'off')
          const active = await bridge.invoke<boolean>('set_plan_mode', { sessionId, active: false })
          setPlanActive(active)
          const result = { kind: 'success' as const, text: 'Plan mode off.' }
          pushCommandDone(sessionId, run.commandId, result)
          return OK({ result })
        }
        const run = pushCommandRun(sessionId, 'plan', message)
        const active = await bridge.invoke<boolean>('set_plan_mode', { sessionId, active: true })
        setPlanActive(active)
        if (message !== '') {
          const busy = journals.get(sessionId)
          if (busy?.taskOpen === true) {
            // 运行中提交本该走上游的 steer（运行中注入）；桌面壳没有注入面（SH-053），
            // 如实报错让草稿留在输入框，用户可以停下任务后重发。
            const result = {
              kind: 'error' as const,
              text: '已进入计划态，但运行中的会话暂不能接收这条消息；停止任务后可直接发送。',
            }
            pushCommandDone(sessionId, run.commandId, result)
            return OK({ result })
          }
          try {
            await submitPromptNow(sessionId, message)
          } catch (reason) {
            // 与输入区同一条错误口径：命令卡片直接显示后端原话（如视觉门禁）。
            const result = { kind: 'error' as const, text: hostErrorText(reason) }
            pushCommandDone(sessionId, run.commandId, result)
            return OK({ result })
          }
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

      // ---- /goal：目标条的人类入口（SH-088）----
      // 语法与文案对齐上游 `command-goal`：show / create / edit / pause / resume / clear。
      // 附件面如实声明不支持（descriptor attachments=false），不假装把附件送进目标轮。
      if (/^\/goal\b/.test(input)) {
        const raw = input.replace(/^\/goal\b/, '').trim()
        const usage = 'Usage: /goal [<objective>|clear|edit <objective>|pause|resume]'
        const run = pushCommandRun(sessionId, 'goal', raw)
        const done = (result: { kind: 'success' | 'error'; text: string }) => {
          pushCommandDone(sessionId, run.commandId, result)
          return OK({ result })
        }
        const report = (title: string, view: GoalViewWire): string => {
          const blocker = view.phase === 'blocked' && view.blockedReason !== undefined
            ? [`Blocker: ${view.blockedReason.code}: ${view.blockedReason.message}`]
            : []
          const commands = view.phase === 'active'
            ? (view.activation === 'armed'
              ? '/goal edit <objective>, /goal pause, /goal clear'
              : '/goal edit <objective>, /goal resume, /goal clear')
            : view.phase === 'complete'
              ? '/goal <objective>, /goal clear'
              : '/goal edit <objective>, /goal resume, /goal clear'
          return [
            title,
            `Status: ${view.phase}`,
            ...blocker,
            `Objective: ${view.objective}`,
            `Rounds: ${view.roundsStarted}/${view.maxGoalRounds}`,
            `Activation: ${view.activation}`,
            '',
            `Commands: ${commands}`,
          ].join('\n')
        }
        try {
          const current = await goalActions.read(sessionId)
          const ref = current === null ? undefined : { id: current.goal.id, revision: current.goal.revision }

          if (raw === '') {
            return done(current === null
              ? { kind: 'success', text: `No goal is currently set.\n${usage}` }
              : { kind: 'success', text: report('Goal', goalViewOf(sessionId, current)) })
          }
          if (raw.toLowerCase() === 'clear') {
            if (ref === undefined) return done({ kind: 'success', text: 'No goal to clear.' })
            await goalActions.clear(sessionId, ref)
            return done({ kind: 'success', text: 'Goal cleared.' })
          }
          if (raw.toLowerCase() === 'pause') {
            if (ref === undefined) {
              return done({ kind: 'error', text: `No goal is currently set; /goal pause requires one. ${usage}` })
            }
            return done({ kind: 'success', text: report('Goal paused', goalViewOf(sessionId, await goalActions.pause(sessionId, ref))) })
          }
          if (raw.toLowerCase() === 'resume') {
            if (ref === undefined) {
              return done({ kind: 'error', text: `No goal is currently set; /goal resume requires one. ${usage}` })
            }
            return done({ kind: 'success', text: report('Goal resumed', goalViewOf(sessionId, await goalActions.resume(sessionId, ref))) })
          }
          if (raw.toLowerCase() === 'edit') {
            return done({ kind: 'error', text: `Goal editing requires a replacement objective.\n${usage}` })
          }
          if (/^edit(?=\s)/iu.test(raw)) {
            const objective = raw.slice(4).trim()
            if (current !== null && current.goal.phase === 'complete') {
              const created = await goalActions.create(sessionId, { objective })
              return done({ kind: 'success', text: report('Goal created', goalViewOf(sessionId, created)) })
            }
            if (ref === undefined) {
              return done({ kind: 'error', text: `No goal is currently set; /goal edit requires one. ${usage}` })
            }
            const edited = await goalActions.edit(sessionId, ref, { objective })
            return done({ kind: 'success', text: report('Goal updated', goalViewOf(sessionId, edited)) })
          }

          if (current !== null && current.goal.phase !== 'complete') {
            return done({
              kind: 'error',
              text: `A goal is already ${current.goal.phase}. Use /goal edit <objective> to change it or /goal clear before replacing it.`,
            })
          }
          const created = await goalActions.create(sessionId, { objective: raw })
          return done({ kind: 'success', text: report('Goal created', goalViewOf(sessionId, created)) })
        } catch {
          return done({
            kind: 'error',
            text: 'The goal command is not valid for the current state. Run /goal to view available commands.',
          })
        }
      }

      // ---- /permission：挡位切换（SH-105 既有实现，文案对齐上游 permission-presets）----
      const permissionMatch = /^\/permission\s*([a-z][a-z-]*)?\s*$/.exec(input)
      if (permissionMatch !== null) {
        const run = pushCommandRun(sessionId, 'permission', permissionMatch[1] ?? '')
        let result: { kind: 'success' | 'error'; text: string }
        const requested = permissionMatch[1]
        if (requested === undefined) {
          await loadSessionPermission(sessionId)
          const current = currentPermission(sessionId)
          result = {
            kind: 'success',
            text: current === PERMISSION_UNAVAILABLE
              ? `current preset unknown（会话档案读取失败）(available: ${PERMISSION_PRESETS.join(', ')})`
              : `current preset ${current} (available: ${PERMISSION_PRESETS.join(', ')})`,
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
              // 会话内切档只写本会话档案（宿主 `set_session_sandbox_mode`）：
              // **绝不写 config.toml**——那是设置页的职责。重进会话靠档案事件回放。
              const applied = await bridge.invoke<string>('set_session_sandbox_mode', {
                sessionId,
                mode: sandbox,
              })
              sessionPresetCache.set(sessionId, presetOfSandbox(applied))
              result = { kind: 'success', text: `preset ${requested}` }
            } catch (reason) {
              // wire 契约：失败以 result.error 回给客户端，不是 rejection——UI 的错误
              // 处理读 result.ok，穿透的 rejection 会绕过它（挡位芯片收不到失败原因）。
              result = { kind: 'error', text: reason instanceof Error ? reason.message : String(reason) }
            }
          }
        }
        if (result.kind === 'success' && requested !== undefined) {
          pushPermission(sessionId)
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
   * `remote.subagents`（SH-109 / SH-053）：泳道子会话的 prompt / interruptByUser 面。
   * 会话控制器对 addressed 子会话硬性路由到这两个方法（`sessions/session.ts` 的 prompt
   * 与 Stop 路径），适配层必须提供命名空间面。
   *
   * 语义照上游（`subagent/src/index.ts`）：prompt 只收文本（图片需要附件仓储，宿主没有
   * → `subagent/attachment-invalid`），requestId 随消息回传成 `source.rpcId` 供客户端
   * 撤销乐观回显；interruptByUser 对 absent/idle/已完成一律受理为 no-op。
   *
   * 偏差（已在 docs/05 SH-053 卡注）：宿主只有 inbox 语义——两种 delivery 都投进
   * 子代理的下一个采样迭代，不是流内 steer；子代理退出后不能经此续聊（回
   * `subagent/not-resumable`，续跑走 `resume_agent` 工具）。
   */
  interface SubagentPromptRequestWire {
    readonly requestId?: string
    readonly childSessionId?: string
    readonly delivery?: 'queue' | 'steer'
    readonly content?: readonly { readonly type: string; readonly text?: string }[]
    readonly clientTimeZone?: string
  }

  /** 宿主类型化错误 → 上游 RemoteError（code 原样透传，不靠文案匹配）。 */
  const subagentErrorOf = (reason: unknown, childSessionId: string): RemoteError => {
    const raw = (reason ?? {}) as { code?: string; message?: string }
    const code = typeof raw.code === 'string' ? raw.code : 'gateway/internal'
    const message = typeof raw.message === 'string' ? raw.message : String(reason)
    return new RemoteError(code as never, message, { childSessionId } as never)
  }

  const subagentsNamespace = {
    prompt: async (request: SubagentPromptRequestWire = {}) => {
      const childSessionId = laneIdOf(request.childSessionId ?? '')
      const parts = request.content ?? []
      if (parts.some(part => part.type !== 'text')) {
        return {
          ok: false as const,
          error: new RemoteError('subagent/attachment-invalid', '宿主没有附件仓储，子代理只接受文本消息', {
            childSessionId,
          } as never),
        }
      }
      const text = parts.map(part => part.text ?? '').join('\n').trim()
      if (text === '') {
        return { ok: false as const, error: new RemoteError('gateway/bad-request', '消息不能为空', {} as never) }
      }
      try {
        const messageId = await bridge.invoke<string>('send_subagent_message', {
          agentId: childSessionId,
          message: text,
          rpcId: request.requestId,
        })
        return OK({ messageId })
      } catch (reason) {
        return { ok: false as const, error: subagentErrorOf(reason, childSessionId) }
      }
    },
    interruptByUser: async (childSessionId: string) => {
      try {
        await bridge.invoke<boolean>('interrupt_subagent', { agentId: laneIdOf(childSessionId) })
        return OK({ accepted: true as const })
      } catch (reason) {
        return { ok: false as const, error: subagentErrorOf(reason, laneIdOf(childSessionId)) }
      }
    },
  }

  /**
   * `remote.settings` 面：设置面本体直连，唯一例外是 `permission` 域写入成功后刷新
   * 各会话的挡位投影。设置页写的是**部署默认**（`config.toml`），会话内切档写的是
   * 本会话档案——两者是两条链。默认变了要让**没有自己事件**的会话跟着变，所以这里
   * 清缓存重读（有事件的会话仍读回自己的档），再逐会话推帧。
   */
  const refreshPermissionsAll = async (): Promise<void> => {
    sessionPresetCache.clear()
    await Promise.all([...journals.keys()].map(id => loadSessionPermission(id)))
    for (const id of journals.keys()) pushPermission(id)
  }

  const settingsNamespace = {
    describe: (): ReturnType<typeof face.settings.describe> => face.settings.describe(),
    mutate: async (ns: string, ops: readonly SettingsPathOpView[], expectedRevision?: number) => {
      const result = await face.settings.mutate(ns, ops, expectedRevision)
      if (result.ok && ns === 'permission') await refreshPermissionsAll()
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
      // 宿主未提供 watcher（docs/05 SH-092 未含），消息不带卡号——它会显示给使用者。
      throw new RemoteError('workspace-file/watch-unsupported' as never, '当前版本不监视文件变更', { path } as never)
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

  /**
   * 技能目录（SH-091）：宿主按会话档案的 workspace_root 发现 SKILL.md，只返回
   * user-invocable 摘要；`/name` 的正文注入在 agent_loop 请求副本里完成。
   */
  const skillsNamespace = {
    list: async (request: { sessionId: string }) => {
      try {
        return OK({
          skills: await bridge.invoke<Array<Record<string, unknown>>>('list_skills', {
            sessionId: request.sessionId,
          }),
        })
      } catch (reason) {
        return FAIL((reason as { message?: string } | undefined)?.message ?? '无法列出技能目录')
      }
    },
  }

  /**
   * `remote.job`（SH-089）：后台作业面。
   *
   * 上游 gateway 推 `job.list`/`job.follow` 流；桌面壳没有作业事件通道，
   * 适配层用宿主的 `job_list`/`job_read` 轮询还原同名帧语义：
   * rows 整表替换、opened 锚点 + output 增量 + 落定 status。注册表在进程内存，
   * 进程退出即清空（`docs/05` SH-089 注）。
   */
  const JOB_POLL_MS = 400
  /**
   * 列表轮询的静默退避：行集合连续多次不变就降到 2s 一次，变化即回 400ms。
   * 只用在 `list`（花名册，平时不动）上；`follow` 不退避——它的轮询延迟直接变成
   * 终端输出的观感，宁可多问几次宿主。
   */
  const JOB_IDLE_POLL_MS = 2000
  const JOB_IDLE_AFTER = 3
  const JOB_TERMINAL = new Set(['completed', 'killed', 'failed'])
  interface JobRowWire {
    readonly id: string
    readonly status: string
    readonly output: { readonly total: number; readonly earliest: number }
  }
  interface JobChunkWire {
    readonly at: number
    readonly text: string
    readonly channel?: string
    readonly gapBefore?: true
  }
  interface JobReadWire {
    readonly job: JobRowWire
    readonly chunks: readonly JobChunkWire[]
    readonly next: number
    readonly lossy: boolean
  }
  const waitAbortable = (ms: number, signal?: AbortSignal): Promise<void> => new Promise((resolve) => {
    if (signal?.aborted === true) {
      resolve()
      return
    }
    const done = (): void => {
      clearTimeout(timer)
      signal?.removeEventListener('abort', done)
      resolve()
    }
    const timer = setTimeout(done, ms)
    signal?.addEventListener('abort', done, { once: true })
  })

  const jobNamespace = {
    list: async function* (
      request: { sessionId: string },
      signal?: AbortSignal,
    ): AsyncGenerator<Record<string, unknown>> {
      let previous = ''
      let idle = 0
      while (signal?.aborted !== true) {
        const jobs = await bridge.invoke<JobRowWire[]>('job_list', { sessionId: request.sessionId })
        const signature = JSON.stringify(jobs)
        if (signature !== previous) {
          previous = signature
          idle = 0
          yield { type: 'rows', jobs }
        } else {
          idle += 1
        }
        await waitAbortable(idle >= JOB_IDLE_AFTER ? JOB_IDLE_POLL_MS : JOB_POLL_MS, signal)
      }
    },
    follow: async function* (
      request: { sessionId?: string; jobId: string; from?: number },
      signal?: AbortSignal,
    ): AsyncGenerator<Record<string, unknown>> {
      let cursor = request.from
      let anchored = false
      while (signal?.aborted !== true) {
        const read = await bridge.invoke<JobReadWire>('job_read', {
          ...(request.sessionId === undefined ? {} : { sessionId: request.sessionId }),
          jobId: request.jobId,
          ...(cursor === undefined ? {} : { from: cursor }),
        })
        if (!anchored) {
          cursor = cursor ?? read.job.output.earliest
          yield { type: 'opened', job: read.job, from: cursor }
          anchored = true
        }
        if (read.chunks.length > 0) {
          yield {
            type: 'output',
            chunks: read.chunks,
            next: read.next,
            ...(read.lossy ? { lossy: true as const } : {}),
          }
          cursor = read.next
        }
        // 落定且输出已排干：交终态帧并结束这一代（服务端会 close 观察流）。
        const position = cursor ?? read.job.output.earliest
        if (JOB_TERMINAL.has(read.job.status) && position >= read.job.output.total) {
          yield { type: 'status', job: read.job }
          return
        }
        await waitAbortable(JOB_POLL_MS, signal)
      }
    },
    kill: async (request: { sessionId: string; jobId: string }) => {
      try {
        return OK(await bridge.invoke<{ outcome: 'requested' | 'already-finished' }>('job_kill', {
          sessionId: request.sessionId,
          jobId: request.jobId,
        }))
      } catch (reason) {
        const raw = (reason ?? {}) as { code?: string; message?: string }
        const code = raw.code === 'job/not-found' ? 'job/not-found' : 'gateway/internal'
        const details = code === 'job/not-found'
          ? { sessionId: request.sessionId, jobId: request.jobId }
          : {}
        return {
          ok: false as const,
          error: new RemoteError(code as never, raw.message ?? '无法终止后台作业', details as never),
        }
      }
    },
  }

  const namespaces = new Map<string, unknown>([
    ['session', sessionNamespace],
    ['workspace', workspaceNamespace],
    ['workspaceFiles', workspaceFilesNamespace],
    ['fileReferences', fileReferencesNamespace],
    ['sessionReferenceResolver', sessionReferenceResolverNamespace],
    ['skills', skillsNamespace],
    ['goals', goalsNamespace],
    ['job', jobNamespace],
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
      // 未接线的命名空间显式抛错（核销清单见 docs/05 SH-040），不返回假数据让界面看起来正常。
      return new Proxy({}, {
        get: (_t, method) => () => unimplemented(`${String(prop)}.${String(method)}`),
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
