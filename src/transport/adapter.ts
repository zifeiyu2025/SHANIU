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
 * - 历史回放：档案历史（`load_session`）→ journal 事件的重建是 SH-033 的范畴，
 *   本层只为当前进程内观察到的回合建 journal。
 * - 审批：走 typert 事件面（`approval/request`），不是 journal 事件。
 */
import { RemoteError } from '@deepseek-ai/dsh-typert-protocol'
import type { AgentEvent, EventMsg } from './types'
import type { HostBridge } from './tauri'

const OK = <T>(value: T) => ({ ok: true as const, value })
const FAIL = (message: string) =>
  ({ ok: false as const, error: new RemoteError('gateway/internal', message, {}) })

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

  /** 从 `from` 起把已入队的条目全部交出，追平后挂起等新帧。 */
  async *tail(from: number): AsyncGenerator<Record<string, unknown>> {
    let index = from
    for (;;) {
      while (index < this.entries.length) yield this.entries[index++]
      await new Promise<void>(resolve => this.waiters.push(resolve))
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
}

/** 一个会话在当前进程内的 journal 与回合状态。 */
interface Journal {
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
   * 本次尝试的稳定身份。它必须**不随帧变化**：start/chunk/end 三帧共用一个 attemptId，
   * 客户端才把它们认成同一段流（按 revision 生成会让每一帧都"换了一次尝试"）。
   */
  attemptId: string
  usage: Record<string, number> | undefined
  outputs: Map<string, { stdout: string[]; stderr: string[] }>
}

interface SettingsView {
  baseUrl: string
  model: string
  workspaceRoot: string
  hasApiKey: boolean
}

export interface AdapterOptions {
  readonly bridge: HostBridge
  readonly now?: () => number
}

/** 与 `config.rs` 的 `[model].provider` 缺省同源。 */
const PROVIDER_ID = 'openai-compatible'
const PROVIDER_NAME = '本地配置端点'

function newJournal(createdAt: number): Journal {
  return {
    seq: -1, revision: 0, log: new FrameLog(), createdAt,
    turn: 0, step: 0, taskOpen: false, text: '', reasoning: '', chunkIndex: 0,
    attemptOpen: false, attemptSeq: -1, attemptId: '', usage: undefined, outputs: new Map(),
  }
}

function unimplemented(name: string, milestone: string): Promise<never> {
  return Promise.reject(new RemoteError('gateway/internal', `适配层未接线：remote.${name}（见 docs/05 ${milestone}）`, {}))
}

export function createAdapter(options: AdapterOptions): { remote: unknown; dispose(): void } {
  const { bridge } = options
  const now = options.now ?? ((): number => Date.now())
  const journals = new Map<string, Journal>()
  const titles = new Map<string, string>()
  /** 审批监听器（DSH 的 ui-approval 在装配时注册）。 */
  const approvalListeners: ((request: Record<string, unknown>, next: () => Promise<never>) => Promise<string>)[] = []
  const projectionLog = new FrameLog()
  let projectionSeq = 0
  let settingsCache: SettingsView | undefined

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
    next: settingsCache?.hasApiKey === true && settingsCache.model !== ''
      ? { provider: PROVIDER_ID, model: settingsCache.model }
      : { provider: '', model: '' },
  })

  /** 空会话的快照帧：cursor -1、无记录、revision 0，且仍带上客户端要求的两个基线。 */
  const emptySnapshot = (id: string, createdAt: number): Record<string, unknown> => ({
    type: 'snapshot',
    header: { version: 0, id, createdAt, isSeeded: false },
    cursor: -1,
    records: [],
    hasMore: false,
    projections: { asOfSeq: 0, values: { modelSelection: currentModelSelection(), title: null } },
    assistantStream: { revision: 0 },
  })

  /** 打开一次助手流：startedAfterSeq 是它将替换的那条消息的 seq。 */
  const ensureAttempt = (journal: Journal): void => {
    if (journal.attemptOpen) return
    journal.attemptOpen = true
    journal.chunkIndex = 0
    journal.attemptSeq = journal.seq
    journal.attemptId = `shaniu-attempt-${journal.turn}-${journal.step}-${journal.revision + 1}`
    pushStreamFrame(journal, { type: 'start', startedAfterSeq: journal.attemptSeq, turn: journal.turn, step: journal.step })
  }

  /**
   * 收口一次助手流：有正文或思考才落 `assistant/message`（空回合不落假消息）。
   * 帧的 end 必须引用刚落的那条事件的 seq，客户端据此把流式气泡换成定稿卡片；
   * 没有落事件时 outcome 只能是 abandoned，否则 end 会指向一个不存在的事件。
   * `interrupted` 只在中断路径上打标：半截正文是真实发生过的输出，不能假装没说过，
   * 但也不能当成正常答完——两者在界面上是两种状态。
   */
  const closeAttempt = (journal: Journal, committed: boolean, interrupted = false): void => {
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
          source: { kind: 'model', provider: PROVIDER_ID, model: settingsCache?.model ?? '' },
        },
        stream: [],
        ...(journal.usage === undefined ? {} : { usage: journal.usage }),
        ...(interrupted ? { interrupted: true } : {}),
      }, { surfaceOp: 'append' })
    }
    pushStreamFrame(journal, {
      type: 'end',
      index: journal.chunkIndex,
      outcome: committed && hasBody
        ? { kind: 'committed', eventType: 'assistant/message', seq }
        : { kind: 'abandoned' },
    })
    journal.text = ''
    journal.reasoning = ''
  }

  const openTask = (journal: Journal): void => {
    journal.turn += 1
    journal.step = 0
    journal.taskOpen = true
  }

  const closeTask = (journal: Journal, reason: Record<string, unknown>, interrupted: boolean): void => {
    closeAttempt(journal, true, interrupted)
    if (!journal.taskOpen) return
    pushEvent(journal, 'step/end', { turn: journal.turn, step: journal.step })
    pushEvent(journal, 'turn/end', { turn: journal.turn, reason })
    journal.taskOpen = false
  }

  const readSettings = async (): Promise<SettingsView | undefined> => {
    try {
      settingsCache = await bridge.invoke<SettingsView>('get_settings')
      return settingsCache
    } catch (reason) {
      // 读不到宿主设置时模型席位留空，而不是编一个默认档位：席位空着用户会去配，
      // 编出来的档位会让用户以为已经配好了。
      console.warn('[SHANIU] 读取宿主设置失败', reason)
      return undefined
    }
  }

  const catalogOf = (view: SettingsView | undefined): Record<string, unknown> => {
    if (view === undefined || !view.hasApiKey || view.model === '') {
      return { default: { provider: '', model: '' }, routableProviders: [], groups: [], failures: [] }
    }
    return {
      default: { provider: PROVIDER_ID, model: view.model },
      routableProviders: [PROVIDER_ID],
      groups: [{ id: PROVIDER_ID, name: PROVIDER_NAME, models: [{ id: view.model, name: view.model }] }],
      failures: [],
    }
  }

  /** 审批：把后端的请求交给 DSH 审批卡，裁决回执只认后端签发过的 request_id。 */
  const requestApproval = async (msg: Extract<EventMsg, { type: 'exec_approval_request' }>): Promise<void> => {
    const listener = approvalListeners[0]
    if (listener === undefined) {
      // 没有审批 UI 在听 = 没有人能裁决：按拒绝收口，绝不因为「没人反对」就执行。
      await bridge.invoke('approve', { requestId: msg.request_id, decision: 'deny' }).catch(() => {})
      return
    }
    const next = async (): Promise<never> => {
      throw new RemoteError('gateway/internal', 'approval/request 没有下游处理者', {})
    }
    let decision = 'deny'
    try {
      const outcome = await listener(
        { toolName: msg.tool, args: msg.args, risk: msg.risk },
        next,
      )
      // DSH 的 PendingApproval 只回答 allowed-once / rejected；「本会话记住」需要
      // SH-025 把这档语义接进审批卡，这里不自造第三态。
      decision = outcome === 'allowed-once' ? 'allow' : 'deny'
    } catch {
      decision = 'deny'
    }
    await bridge.invoke('approve', { requestId: msg.request_id, decision })
      .catch((reason: unknown) => { console.error('[SHANIU] 审批回执送不出', reason) })
  }

  const onEvent = (event: AgentEvent): void => {
    const journal = journalOf(event.session_id)
    const msg = event.msg
    switch (msg.type) {
      case 'turn_started': {
        // 首条用户消息在本层落地时就开了回合（照 mock 的既定顺序：turn/start →
        // step/start → user/message，输入行必须落在回合内，否则回合头不显示用时）。
        // 因此后端的第一次 turn_started 通常是空操作；从第二次起才推进 step——
        // 后端每个采样迭代都发 turn_started，它对应 DSH 的 step 而不是 DSH 的回合。
        if (!journal.taskOpen) {
          openTask(journal)
          pushEvent(journal, 'turn/start', { turn: journal.turn })
          pushEvent(journal, 'step/start', { turn: journal.turn, step: 0 })
        } else if (msg.turn > 1) {
          pushEvent(journal, 'step/end', { turn: journal.turn, step: journal.step })
          journal.step = msg.turn - 1
          pushEvent(journal, 'step/start', { turn: journal.turn, step: journal.step })
        }
        return
      }
      case 'agent_message_delta':
        ensureAttempt(journal)
        journal.text += msg.delta
        pushStreamFrame(journal, {
          type: 'chunk', index: journal.chunkIndex, time: now(),
          chunk: { type: 'text-delta', index: 0, text: msg.delta },
        })
        journal.chunkIndex += 1
        return
      case 'reasoning_delta':
        ensureAttempt(journal)
        journal.reasoning += msg.delta
        pushStreamFrame(journal, {
          type: 'chunk', index: journal.chunkIndex, time: now(),
          chunk: { type: 'reasoning-delta', index: 0, text: msg.delta },
        })
        journal.chunkIndex += 1
        return
      case 'token_usage': {
        // 三桶齐全才满足上游判据 total = input + output + read + write。
        const read = msg.cached_input_tokens ?? 0
        journal.usage = {
          inputTokens: msg.input_tokens,
          outputTokens: msg.output_tokens,
          cacheReadTokens: read,
          cacheWriteTokens: 0,
          totalTokens: msg.input_tokens + msg.output_tokens + read,
        }
        return
      }
      case 'tool_call_begin':
        // 工具调用先定稿此前的正文：助手消息在 DSH 里排在调用之前。
        closeAttempt(journal, true)
        pushEvent(journal, 'tool/call', {
          turn: journal.turn,
          step: journal.step,
          callId: msg.call_id,
          name: msg.tool,
          // DSH 的 tool/call.arguments 是模型产出的原始 JSON 字符串，不是对象。
          arguments: typeof msg.args === 'string' ? msg.args : JSON.stringify(msg.args ?? {}),
        })
        journal.outputs.set(msg.call_id, { stdout: [], stderr: [] })
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
        return
      }
      case 'turn_completed':
        // cancelled 的半截正文是真实发生过的输出：落进档案，但打 interrupted，
        // 不能当成正常答完。
        closeTask(journal, msg.reason === 'completed'
          ? { kind: 'completed' }
          : msg.reason === 'cancelled'
            ? { kind: 'aborted', reason: { kind: 'user' } }
            : { kind: 'error', error: { message: `回合以 ${msg.reason} 结束`, code: msg.reason } },
        msg.reason === 'cancelled')
        return
      case 'error':
        // 全局错误不终结回合（回合终结只认 turn_completed）：这里只定稿已有的流，
        // 错误文案由随后的 turn_completed 呈现，避免在此造出一个假回合。
        closeAttempt(journal, false)
        console.warn('[SHANIU] 后端错误事件', msg.message)
        return
      case 'exec_approval_request':
        void requestApproval(msg)
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

  const sessionNamespace = {
    list: async () => {
      const rows = await bridge.invoke<{ agentId: string; updatedAt: number; title: string; workspaceRoot: string }[]>('list_sessions')
      return OK({
        items: rows.map(row => ({
          agentAvailable: true,
          sessionId: row.agentId,
          cwd: row.workspaceRoot,
          running: journals.get(row.agentId)?.taskOpen === true,
          blank: (titles.get(row.agentId) ?? row.title) === '',
          updatedAt: row.updatedAt,
          formatStatus: 'current',
        })),
      })
    },
    create: async (request: { cwd?: string } = {}) => {
      const agentId = await bridge.invoke<string>('create_session', { workspaceRoot: request.cwd })
      journals.set(agentId, newJournal(now()))
      return OK({ sessionId: agentId })
    },
    fork: async (request: { sessionId: string; throughSeq?: number }) => {
      const journal = journals.get(request.sessionId)
      if (journal !== undefined && request.throughSeq !== undefined && request.throughSeq < journal.seq) {
        // 中间游标的分支要求「事件 seq → 档案条目」的对齐表，而信封目前没有落盘
        // （docs/04 §1.1 的 SH-094 前置项）。这里宁可不接，也不按 journal 条数猜一个
        // keep_items——猜错就是静默丢掉一段模型历史。
        return FAIL('按游标分支需要事件与档案的对齐关系，尚未持久化（见 docs/05 SH-094）')
      }
      const agentId = await bridge.invoke<string>('fork_session', {
        agentId: request.sessionId,
        // 末尾分支 = 整份档案续一份。后端 nearest_safe_cut 会把请求量钳到档案实际条数
        // 并回退到最后一个调用/输出配对的边界，所以这里给"全部"而不是猜一个条数。
        keepItems: Number.MAX_SAFE_INTEGER,
      })
      journals.set(agentId, newJournal(now()))
      return OK({ sessionId: agentId })
    },
    prompt: async (request: { sessionId?: string; requestId?: string; content?: readonly { type: string; text?: string }[] }) => {
      const text = (request.content ?? []).filter(part => part.type === 'text').map(part => part.text ?? '').join('')
      const agentId = await bridge.invoke<string>('send_task', { task: text, agentId: request.sessionId })
      const journal = journalOf(agentId)
      if (!titles.has(agentId)) pushTitle(agentId, text.trim().slice(0, 12) || '新会话')
      // 回合在这里开，不等后端的第一条事件：输入行必须落在回合内，
      // 而 send_task 已经返回 agent_id，说明后端确实接下了这个任务。
      if (!journal.taskOpen) openTask(journal)
      pushEvent(journal, 'turn/start', { turn: journal.turn })
      pushEvent(journal, 'step/start', { turn: journal.turn, step: journal.step })
      // rpcId 必须等于 prompt 的 requestId：客户端据此撤掉本地乐观回显（session.ts 的
      // observeSubmissionMessage），否则同一条消息会出现两遍。
      pushEvent(journal, 'user/message', {
        id: `shaniu-user-${journal.seq + 1}`,
        role: 'user',
        content: [{ type: 'text', text }],
        source: { kind: 'user', ...(request.requestId === undefined ? {} : { rpcId: request.requestId }) },
      }, { surfaceOp: 'append' })
      return OK({ accepted: true })
    },
    cancel: async (request: { sessionId?: string } = {}) => {
      // 这里不定稿任何东西：中断的收口权在后端的 turn_completed{cancelled}，
      // 提前定稿会造出一个后端没承认过的终态。
      await bridge.invoke('cancel_task', { sessionId: request.sessionId })
      return OK({ accepted: true })
    },
    follow: async function* (request: unknown): AsyncGenerator<Record<string, unknown>> {
      const sessionId = addressOf(request)
      if (sessionId === undefined) {
        yield emptySnapshot('', now())
        return
      }
      const journal = journals.get(sessionId)
      if (journal === undefined) {
        // 没有观察过这个会话：交空快照（不替它建 journal——follow 不该凭空造会话身份），
        // 但也不能挂着不返回，客户端要的是确定的空态。
        yield emptySnapshot(sessionId, now())
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
          },
        },
        // 客户端总是带 assistantStream:true 打开，缺这个基线即 gateway/internal。
        assistantStream: { revision: baseline.revision },
      }
      for await (const frame of journal.log.tail(from)) yield frame
    },
    control: async function* (): AsyncGenerator<Record<string, unknown>> {
      await readSettings()
      const from = projectionLog.length
      yield {
        type: 'baseline',
        value: {
          projections: Object.fromEntries([...journals.keys()].map(id => [id, {
            asOfSeq: 0,
            values: { modelSelection: currentModelSelection(), title: titles.get(id) ?? null },
          }])),
        },
      }
      // 基线之后必须持续产帧：流一结束客户端按 carrier 错误重连。
      for await (const frame of projectionLog.tail(from)) yield frame
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
    search: async () => OK({ items: [], hasMore: false }),
    modelCatalog: async () => OK(catalogOf(await readSettings())),
    selectModel: async (request: { model?: string }) => {
      const view = await readSettings()
      if (view === undefined) return FAIL('读不到宿主设置，无法保存模型选择')
      const model = request.model ?? view.model
      await bridge.invoke('save_settings', { baseUrl: view.baseUrl, model, workspaceRoot: view.workspaceRoot })
      await readSettings()
      projectionSeq += 1
      for (const id of journals.keys()) {
        projectionLog.append({
          type: 'projection', sessionId: id, seq: projectionSeq,
          changes: [{ key: 'modelSelection', val: currentModelSelection() }],
        })
      }
      return OK({ provider: PROVIDER_ID, model })
    },
    canOpenWorkspacePath: async () => OK(true),
    openWorkspacePath: async (request: { path?: string } = {}) => {
      await bridge.invoke('open_in_explorer', { path: request.path ?? '' })
      return OK({ opened: true })
    },
    initializeDefaultModel: async () => {
      await readSettings()
      return OK(undefined)
    },
    rename: () => unimplemented('session.rename', 'SH-033'),
    attachment: () => unimplemented('session.attachment', 'SH-040'),
    updateQueue: () => unimplemented('session.updateQueue', 'SH-018'),
    state: () => unimplemented('session.state', 'SH-033'),
    wait: () => unimplemented('session.wait', 'SH-033'),
    events: () => unimplemented('session.events', 'SH-033'),
    observeEvent: () => unimplemented('session.observeEvent', 'SH-094'),
    observeProjections: () => unimplemented('session.observeProjections', 'SH-094'),
    observeState: () => unimplemented('session.observeState', 'SH-094'),
    workspacePathApplications: () => unimplemented('session.workspacePathApplications', 'SH-092'),
  }

  const namespaces = new Map<string, unknown>([['session', sessionNamespace]])

  const remote: unknown = new Proxy({
    $host: { home: undefined, isLoopback: true },
    $on(event: string, listener: (request: Record<string, unknown>, next: () => Promise<never>) => Promise<string>): () => void {
      if (event !== 'approval/request') return () => {}
      approvalListeners.push(listener)
      return () => {
        const index = approvalListeners.indexOf(listener)
        if (index >= 0) approvalListeners.splice(index, 1)
      }
    },
    $emit: () => false,
    $call: () => Promise.reject(new Error('adapter(remote).$call: 该调用面尚未接线')),
    $stream: (streamOptions: { open: (signal?: AbortSignal) => AsyncIterable<unknown> }) => {
      let iterator: AsyncIterator<unknown> | null = null
      let generation = 0
      const ensure = (): void => {
        // 惰性开启：journal 流的 initialRequest 要到首次迭代才就位。
        if (iterator === null) iterator = (streamOptions.open())[Symbol.asyncIterator]()
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
            await iterator?.return?.(undefined)
            iterator = null
            return { done: true as const, value: undefined as never }
          },
        }),
        restart: () => {
          generation += 1
          void iterator?.return?.(undefined)
          iterator = null
        },
        dispose: async () => {
          await iterator?.return?.(undefined)
          iterator = null
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

  const unlisten = bridge.listenEvents(onEvent)
  void readSettings()

  return {
    remote,
    dispose() {
      unlisten()
    },
  }
}
