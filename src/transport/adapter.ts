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
import { createSettingsFace } from './settings-face'
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
  /**
   * 档案条目下标 → 该条目回放出的最后一个 journal seq（SH-033 的重建产物）。
   * 空数组表示这个会话的 journal 全部来自当前进程观察，没有可对账的档案前缀。
   */
  entryToSeq: number[]
}

/**
 * `load_session` 的档案投影（与 `src-tauri/src/lib.rs` 的 `SessionEntry` 同形）。
 * 字段名跟着 Rust 的 serde 走：这里刻意**不**重命名，改名就会和后端序列化结果漂移。
 */
type SessionEntry =
  | { kind: 'message'; role: string; text: string; reasoning?: string }
  | { kind: 'tool_call'; call_id: string; tool: string; arguments: string; output?: string }

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

function newJournal(createdAt: number): Journal {
  return {
    seq: -1, revision: 0, log: new FrameLog(), createdAt,
    turn: 0, step: 0, taskOpen: false, text: '', reasoning: '', chunkIndex: 0,
    attemptOpen: false, attemptSeq: -1, attemptId: '', usage: undefined, outputs: new Map(),
    entryToSeq: [],
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
          source: { kind: 'model', ...face.activeSelection() },
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
      pushEvent(journal, 'turn/start', { turn: journal.turn })
    }

    entries.forEach((entry, index) => {
      if (entry.kind === 'message' && entry.role === 'user') {
        startTurn()
        openStep(0)
        pushEvent(journal, 'user/message', {
          id: `shaniu-replay-user-${index}`,
          role: 'user',
          content: [{ type: 'text', text: entry.text }],
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
          // 不经 prompt 起来的回合（`resume_session` 续跑）也要报运行中，否则
          // 恢复出来的会话跑着任务却没有 Stop 按钮。
          dispatch('api-session/status', event.session_id, true)
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
        // 运行态必须收回：输入卡的 Stop 按钮由 `api-session/status` 决定（`running`
        // 为真才显示），只发 journal 的 turn/end 不发状态，按钮会永远停在发送态。
        dispatch('api-session/status', event.session_id, false)
        return
      case 'error':
        // 全局错误不终结回合（回合终结只认 turn_completed）：这里只定稿已有的流，
        // 错误文案由随后的 turn_completed 呈现，避免在此造出一个假回合。
        closeAttempt(journal, false)
        console.warn('[SHANIU] 后端错误事件', msg.message)
        return
      case 'exec_approval_request':
        void requestApproval(event.session_id, msg)
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
      // 先回宿主读一次：没有缓存时 workspaceRoot() 是空串，会把合法请求误判成
      // "给了别的目录"。建档是一次显式用户动作，多一个 invoke 不算代价。
      await face.refresh()
      const root = face.workspaceRoot()
      // 后端只在配置的工作区根上跑（create_session 不收 cwd）。界面给了别的目录
      // 就必须拒绝：静默用宿主根建会话，等于用户以为会话在 A、实际工具在 B 里跑。
      if (request.cwd !== undefined && request.cwd !== '' && request.cwd !== root) {
        return FAIL(`宿主只在配置的工作区根（${root}）上运行，暂不支持在 ${request.cwd} 建会话（见 docs/05 SH-099）`)
      }
      const agentId = await bridge.invoke<string>('create_session')
      journals.set(agentId, newJournal(now()))
      // 工作区分组按 sessionIds 显示：新会话建完就要推一帧，否则侧栏的工作区树
      // 里看不到它，要重开窗口才出现。
      workspaceLog.append({ type: 'upsert', workspace: await hostWorkspaceView() })
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
      dispatch('api-session/status', agentId, true)
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
        // 没在当前进程里观察过：能从档案重建就重建（SH-033 的"打开旧会话"），
        // 重建不出内容才交空快照。档案读不回是硬错误——静默交空态会让用户以为
        // 会话没了，而实际只是宿主读失败。
        const entries = await bridge.invoke<SessionEntry[]>('load_session', { agentId: sessionId })
        if (entries.length === 0) {
          yield emptySnapshot(sessionId, now())
          return
        }
        const replayed = replayArchive(entries)
        journals.set(sessionId, replayed)
        if (!titles.has(sessionId)) {
          const firstUser = entries.find(entry => entry.kind === 'message' && entry.role === 'user')
          const text = firstUser?.kind === 'message' ? firstUser.text : ''
          pushTitle(sessionId, text.trim().slice(0, 12) || '新会话')
        }
        const from = replayed.log.length
        yield {
          type: 'snapshot',
          header: { version: 0, id: sessionId, createdAt: replayed.createdAt, isSeeded: false },
          cursor: replayed.seq,
          records: replayed.log.eventsUpTo(from),
          hasMore: false,
          projections: {
            asOfSeq: 0,
            values: { modelSelection: currentModelSelection(), title: titles.get(sessionId) ?? null },
          },
          assistantStream: { revision: replayed.revision },
        }
        for await (const frame of replayed.log.tail(from)) yield frame
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
      await face.refresh()
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
    modelCatalog: async () => face.session.modelCatalog(),
    selectModel: async (request: { sessionId?: string; provider?: string; model?: string; reasoningEffort?: string }) => {
      const selected = await face.session.selectModel({
        provider: request.provider ?? '',
        model: request.model ?? '',
        ...(request.reasoningEffort === undefined ? {} : { reasoningEffort: request.reasoningEffort }),
      })
      if (!selected.ok) return selected
      projectionSeq += 1
      for (const id of journals.keys()) {
        projectionLog.append({
          type: 'projection', sessionId: id, seq: projectionSeq,
          changes: [{ key: 'modelSelection', val: currentModelSelection() }],
        })
      }
      return selected
    },
    canOpenWorkspacePath: async () => OK(true),
    openWorkspacePath: async (request: { path?: string } = {}) => {
      await bridge.invoke('open_in_explorer', { path: request.path ?? '' })
      return OK({ opened: true })
    },
    initializeDefaultModel: async () => {
      await face.refresh()
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

  /**
   * 工作区面：宿主只有一个工作区 = `config.toml` 的 `[workspace].root`。
   *
   * 只读成员给真状态（一个工作区、无归档、无置顶），会改状态的成员一律显式未接线：
   * 后端没有多工作区注册表，"添加工作区"若返回成功就等于在界面上造出一个
   * 运行时根本不会用的目录。多工作区与归档/置顶分属 SH-099/SH-100。
   */
  const HOST_WORKSPACE_ID = 'default-workspace'
  const workspaceLog = new FrameLog()
  const bootedAt = new Date().toISOString()

  const hostWorkspaceView = async (): Promise<Record<string, unknown>> => {
    const path = face.workspaceRoot()
    const rows = await bridge.invoke<{ agentId: string; workspaceRoot: string; updatedAt: number }[]>('list_sessions')
      .catch((): [] => [])
    const mine = rows.filter(row => row.workspaceRoot === path)
    const latest = mine.reduce((max, row) => Math.max(max, row.updatedAt), 0)
    return {
      workspaceId: HOST_WORKSPACE_ID,
      path,
      // 上游按 title === 'default-workspace' 判定首启自动标题并渲染成「默认工作区」
      title: HOST_WORKSPACE_ID,
      sessionIds: mine.map(row => row.agentId),
      createdAt: bootedAt,
      updatedAt: latest === 0 ? bootedAt : new Date(latest).toISOString(),
    }
  }

  const workspaceNamespace = {
    follow: async function* (): AsyncGenerator<Record<string, unknown>> {
      await face.refresh()
      const from = workspaceLog.length
      yield {
        type: 'baseline',
        value: { items: [await hostWorkspaceView()], archivedSessionIds: [], pinnedSessionIds: [] },
      }
      for await (const frame of workspaceLog.tail(from)) yield frame
    },
    initializeDefault: async () => {
      await face.refresh()
      return OK({ workspace: await hostWorkspaceView() })
    },
    // 单工作区只有一种合法顺序，回写真顺序而不是抛错：这是状态读取，不是变更成功
    insertBefore: async () => OK({ workspaceIds: [HOST_WORKSPACE_ID] }),
    create: () => unimplemented('workspace.create', 'SH-099'),
    rename: () => unimplemented('workspace.rename', 'SH-099'),
    delete: () => unimplemented('workspace.delete', 'SH-099'),
    archiveSession: () => unimplemented('workspace.archiveSession', 'SH-100'),
    unarchiveSession: () => unimplemented('workspace.unarchiveSession', 'SH-100'),
    pinSession: () => unimplemented('workspace.pinSession', 'SH-100'),
    unpinSession: () => unimplemented('workspace.unpinSession', 'SH-100'),
    insertSessionBefore: () => unimplemented('workspace.insertSessionBefore', 'SH-100'),
  }

  const namespaces = new Map<string, unknown>([
    ['session', sessionNamespace],
    ['workspace', workspaceNamespace],
    ['settings', face.settings],
    ['llm', face.llm],
    ['credentials', face.credentials],
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
  void face.refresh()

  return {
    remote,
    dispose() {
      unlisten()
    },
  }
}
