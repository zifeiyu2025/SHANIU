/**
 * SH-017 适配层契约测试：判据是**行为与契约**，不是整段 JSON 字面量快照。
 *
 * 这里刻意复用 DSH 自己的 wire 校验器（`assertSessionWireEvent`）——适配层的输出
 * 必须过客户端运行时真正会过的那道闸，而不是我在测试里另写一份"看起来一样"的规则。
 */
import { describe, expect, it, vi } from 'vitest'
import { assertSessionWireEvent } from '../dsh/packages/api/session-controller/src/client/session-wire-event.ts'
import { createAdapter } from './adapter'
import type { HostBridge } from './tauri'
import type { AgentEvent, EventMsg } from './types'

/** 宿主 `get_settings` 的真实形状（04 §1.3）：服务商文档 + 逐家 hasApiKey。 */
const HOST_SETTINGS = {
  baseUrl: 'https://lc/v1',
  model: 'LongCat-2.5',
  maxTurns: 32,
  workspaceRoot: 'W',
  hasApiKey: true,
  sandboxMode: 'workspace_write',
  approvalMode: 'ask',
  activeProvider: 'longcat',
  providerRevision: 4,
  providers: [
    {
      id: 'longcat',
      label: 'LongCat',
      provider: 'openai-chat',
      baseUrl: 'https://lc/v1',
      model: 'LongCat-2.5',
      secretRef: 'LONGCAT_API_KEY',
      hasApiKey: true,
      models: [{ id: 'LongCat-2.5', name: 'LongCat 2.5', contextWindow: 128_000, maxTokens: 8_192, input: ['text'] }],
    },
    {
      id: 'ghost',
      label: 'Ghost',
      provider: 'openai-chat',
      baseUrl: 'https://g/v1',
      model: 'g-1',
      secretRef: 'GHOST_API_KEY',
      hasApiKey: false,
      models: [],
    },
  ],
}

interface ScriptedBridge extends HostBridge {
  readonly calls: { command: string; args: Record<string, unknown> | undefined }[]
  emit(sessionId: string, msg: EventMsg, agentId?: string): void
}

function scriptedBridge(
  settings: Record<string, unknown> = HOST_SETTINGS,
  archive: Record<string, unknown>[] = [],
  renameFailure?: unknown,
  saveFailure?: unknown,
): ScriptedBridge {
  const calls: ScriptedBridge['calls'] = []
  const handlers = new Set<(event: AgentEvent) => void>()
  let counter = 0
  // SH-100 的侧车状态（每份桥独立克隆，避免测试间串味）
  const workspaceFlags = { archived: [] as string[], pinned: [] as string[], order: [] as string[] }
  // 宿主语义的关键一条：save_settings 落盘 config.toml，之后的 get_settings 回读新值。
  // 测试桥按同一语义模拟（每份桥独立的克隆，避免测试间串味），挡位切换的投影推帧
  // 才能测到"切换后 UI 显示新挡位"而不是停在旧值。
  const state = structuredClone(settings)
  return {
    calls,
    async invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
      calls.push({ command, args })
      if (command === 'get_settings') return state as T
      if (command === 'save_settings') {
        if (saveFailure !== undefined) throw saveFailure
        state['baseUrl'] = args?.['base_url']
        state['model'] = args?.['model']
        state['workspaceRoot'] = args?.['workspace_root']
        if (args?.['sandbox_mode'] !== undefined) state['sandboxMode'] = args['sandbox_mode']
        return undefined as T
      }
      if (command === 'create_session') return 'agent-new' as T
      if (command === 'send_task') return (args?.agentId ?? 'agent-new') as T
      if (command === 'load_session') return archive as T
      if (command === 'list_sessions') {
        return [{ agentId: 'agent-new', updatedAt: 1, title: '首条任务', workspaceRoot: 'W' }] as T
      }
      if (command === 'fork_session') return 'agent-fork' as T
      if (command === 'rename_session') {
        // 模拟宿主语义：去掉首尾空白后为空 = 清除（回 null），否则回生效标题
        if (renameFailure !== undefined) throw renameFailure
        const title = String(args?.title ?? '').trim()
        return (title === '' ? null : title) as T
      }
      // SH-100 的宿主替身：归档/置顶/顺序按真实宿主语义模拟（含集合互斥与排序补位）
      if (command === 'get_workspace_flags') {
        return { archivedSessionIds: [...workspaceFlags.archived], pinnedSessionIds: [...workspaceFlags.pinned], sessionOrder: [...workspaceFlags.order] } as T
      }
      if (command === 'archive_session') {
        const id = String(args?.sessionId ?? '')
        // 运行中的会话：不带 stopActivity 拒绝（session-active），带了先取消再归档
        if (id === 'agent-running' && args?.stopActivity !== true) throw { code: 'session-active', message: `会话 ${id} 正在运行：先停止任务，或携带 stopActivity 归档` }
        if (id === 'agent-ghost') throw { code: 'not-found', message: `会话档案不存在：${id}` }
        if (!workspaceFlags.archived.includes(id)) workspaceFlags.archived.push(id)
        workspaceFlags.pinned = workspaceFlags.pinned.filter(pinned => pinned !== id)
        return { archivedSessionIds: [...workspaceFlags.archived] } as T
      }
      if (command === 'unarchive_session') {
        const id = String(args?.sessionId ?? '')
        workspaceFlags.archived = workspaceFlags.archived.filter(item => item !== id)
        return { archivedSessionIds: [...workspaceFlags.archived] } as T
      }
      if (command === 'pin_session') {
        const id = String(args?.sessionId ?? '')
        if (id === 'agent-ghost') throw { code: 'not-found', message: `会话档案不存在：${id}` }
        if (workspaceFlags.archived.includes(id)) throw { code: 'pin-archived', message: `会话 ${id} 已归档：先取消归档才能置顶` }
        if (!workspaceFlags.pinned.includes(id)) workspaceFlags.pinned.unshift(id)
        return { pinnedSessionIds: [...workspaceFlags.pinned] } as T
      }
      if (command === 'unpin_session') {
        const id = String(args?.sessionId ?? '')
        if (id === 'agent-ghost') throw { code: 'not-found', message: `会话档案不存在：${id}` }
        workspaceFlags.pinned = workspaceFlags.pinned.filter(item => item !== id)
        return { pinnedSessionIds: [...workspaceFlags.pinned] } as T
      }
      if (command === 'insert_session_before') {
        const id = String(args?.sessionId ?? '')
        const before = args?.beforeSessionId as string | undefined
        if (id === 'agent-ghost' || before === 'agent-ghost') throw { code: 'not-found', message: '会话档案不存在' }
        if (before !== undefined && !workspaceFlags.order.includes(before)) throw { code: 'move-invalid', message: `锚点会话 ${before} 不在排序账户里` }
        workspaceFlags.order = workspaceFlags.order.filter(item => item !== id)
        if (before === undefined) workspaceFlags.order.push(id)
        else workspaceFlags.order.splice(workspaceFlags.order.indexOf(before), 0, id)
        return { sessionOrder: [...workspaceFlags.order] } as T
      }
      return undefined as T
    },
    listenEvents(handler) {
      handlers.add(handler)
      return () => handlers.delete(handler)
    },
    emit(sessionId, msg, agentId = 'root') {
      counter += 1
      const event: AgentEvent = {
        event_id: `01J00000000000000000000${String(counter).padStart(2, '0')}`,
        session_id: sessionId,
        agent_id: agentId,
        parent_id: agentId === 'root' ? null : 'root',
        ts_ms: 1_700_000_000_000,
        msg,
      }
      for (const handler of handlers) handler(event)
    },
  }
}

/** 取快照 records 里的 journal 事件（跳过 assistant-stream 帧）。 */
function eventsOf(snapshot: Record<string, unknown>): Record<string, unknown>[] {
  return (snapshot['records'] as { event: Record<string, unknown> }[]).map(record => record.event)
}

/** 会话面在测试里的窄形状：只声明被测方法，避免每处都写类型断言。 */
interface SessionFace {
  prompt(request: Record<string, unknown>): Promise<Record<string, unknown>>
  follow(request: Record<string, unknown>): AsyncGenerator<Record<string, unknown>>
  page(request?: Record<string, unknown>): Promise<Record<string, unknown>>
  list(): Promise<Record<string, unknown>>
  create(request?: Record<string, unknown>): Promise<Record<string, unknown>>
  cancel(request?: Record<string, unknown>): Promise<Record<string, unknown>>
  fork(request: Record<string, unknown>): Promise<Record<string, unknown>>
  modelCatalog(): Promise<Record<string, unknown>>
  selectModel(request: Record<string, unknown>): Promise<Record<string, unknown>>
  rename(request?: Record<string, unknown>): Promise<Record<string, unknown>>
  attachment(): Promise<never>
}

interface RemoteFace {
  readonly session: SessionFace
  readonly workspaceFiles: Record<string, () => Promise<unknown>>
  $on(event: string, listener: (this: unknown, ...args: never[]) => unknown): () => void
}

function faceOf(remote: unknown): RemoteFace {
  return remote as RemoteFace
}

const ENVELOPE_KEYS = ['type', 'seq', 'time', 'data', 'ignorable', 'surfaceOp', 'sourceEventSeqs']
const SURFACE_TYPES = ['user/message', 'assistant/message', 'tool/result']

describe('适配层把 EventMsg 流映射成 DSH 会话 journal', () => {
  it('一次纯文本回合：事件逐条 +1、surface 规则与 wire 校验全过', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '你好' }] })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '你' })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '好' })
    bridge.emit('agent-new', { type: 'token_usage', turn: 1, input_tokens: 10, cached_input_tokens: 2, output_tokens: 4 })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)

    expect(journal.map(event => event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message', 'assistant/message', 'step/end', 'turn/end',
    ])
    journal.forEach((event, index) => {
      expect(event['seq']).toBe(index)
      expect(Object.keys(event).every(key => ENVELOPE_KEYS.includes(key))).toBe(true)
      expect('surfaceOp' in event).toBe(SURFACE_TYPES.includes(String(event['type'])))
      // 这道断言就是客户端运行时会做的那道闸
      expect(() => assertSessionWireEvent(event)).not.toThrow()
    })
    expect(snapshot['cursor']).toBe(journal.at(-1)?.['seq'])

    const assistant = journal.find(event => event['type'] === 'assistant/message')?.['data'] as Record<string, unknown>
    expect((assistant['message'] as { content: unknown[] }).content).toEqual([{ type: 'text', text: '你好' }])
    // inputTokens 是**未缓存**输入（10 − 2 = 8）：上游 TokenUsage 语义里它是 uncached
    // 桶（usage-projection.ts 的 bucketsFrom 直接拿它当 uncachedInputTokens），
    // total = uncached + output + read。旧断言把全量 prompt 当 input，缓存算了两遍。
    expect(assistant['usage']).toEqual({ inputTokens: 8, outputTokens: 4, cacheReadTokens: 2, cacheWriteTokens: 0, totalTokens: 14 })
    expect((journal.find(event => event['type'] === 'turn/end')?.['data'] as { reason: unknown }).reason)
      .toEqual({ kind: 'completed' })

    await iterator.return?.(undefined)
    dispose()
  })

  it('助手流帧：revision 从快照基线起逐帧 +1，end 引用真实事件 seq', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '想一下' }] })
    // 客户端先打开 follow、再产生活跃流帧：snapshot 的 revision 是基线。
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    expect((snapshot['assistantStream'] as { revision: number }).revision).toBe(0)
    expect(eventsOf(snapshot).map(event => event['type']))
      .toEqual(['turn/start', 'step/start', 'user/message'])

    bridge.emit('agent-new', { type: 'reasoning_delta', delta: '先' })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '答案' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    // 4 帧助手流 + 1 条 assistant/message 事件帧
    const frames: Record<string, unknown>[] = []
    for (let i = 0; i < 5; i += 1) frames.push((await iterator.next()).value as Record<string, unknown>)

    const stream = frames
      .filter(frame => frame['type'] === 'assistant-stream')
      .map(frame => frame['frame'] as Record<string, unknown>)
    expect(stream.map(frame => frame['revision'])).toEqual([1, 2, 3, 4])
    expect(stream[0]?.['type']).toBe('start')
    expect(stream.at(-1)?.['type']).toBe('end')
    const outcome = stream.at(-1)?.['outcome'] as { kind: string; seq: number }
    expect(outcome.kind).toBe('committed')

    // committed 必须指向真实存在的 assistant/message
    const page = await session.page({ sessionId: 'agent-new' })
    const journal = (page['value'] as { records: { event: Record<string, unknown> }[] }).records.map(record => record.event)
    expect(journal[outcome.seq]?.['type']).toBe('assistant/message')
    expect((journal[outcome.seq]?.['data'] as { message: { content: unknown[] } }).message.content)
      .toEqual([{ type: 'reasoning', text: '先' }, { type: 'text', text: '答案' }])

    await iterator.return?.(undefined)
    dispose()
  })

  it('两次采样迭代各开一次尝试：同一次内 attemptId 恒定，跨 attempt 不同', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-8', content: [{ type: 'text', text: '两步' }] })
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    await iterator.next()

    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '第一段' })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'read_file', args: {} })
    bridge.emit('agent-new', { type: 'turn_started', turn: 2 })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '第二段' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 2, reason: 'completed' })

    const attemptIds: string[] = []
    for (let i = 0; i < 13; i += 1) {
      const frame = (await iterator.next()).value as Record<string, unknown>
      if (frame['type'] === 'assistant-stream') attemptIds.push((frame['frame'] as { attemptId: string }).attemptId)
    }
    // 每次尝试 3 帧（start/chunk/end）：attemptId 若随 revision 变化，
    // 客户端会把同一段的每帧当成"又开了一次尝试"，流式气泡就无法定稿。
    expect(attemptIds).toHaveLength(6)
    expect(new Set(attemptIds.slice(0, 3)).size).toBe(1)
    expect(new Set(attemptIds.slice(3, 6)).size).toBe(1)
    expect(attemptIds[0]).not.toBe(attemptIds[3])

    await iterator.return?.(undefined)
    dispose()
  })

  it('工具调用：arguments 落原始 JSON 字符串，输出与退出码并进 tool/result', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '跑一下' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'unified_exec', args: { cmd: 'ls' } })
    bridge.emit('agent-new', { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stdout', delta: 'a.txt' })
    bridge.emit('agent-new', { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stderr', delta: '警告' })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    const call = journal.find(event => event['type'] === 'tool/call')?.['data'] as Record<string, unknown>
    expect(call['arguments']).toBe('{"cmd":"ls"}')
    expect(call['callId']).toBe('c1')
    expect(call['name']).toBe('unified_exec')

    const result = journal.find(event => event['type'] === 'tool/result')?.['data'] as Record<string, unknown>
    const message = result['message'] as { content: { text: string }[]; isError: boolean; source: Record<string, unknown> }
    expect(message.content[0]?.text).toBe('a.txt\n警告\n[exit code: 0]')
    expect(message.isError).toBe(false)
    expect(message['source']).toEqual({ kind: 'tool', callId: 'c1' })
    expect(result['error']).toBeUndefined()
    journal.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())

    await iterator.return?.(undefined)
    dispose()
  })

  it('审批拒绝与取消都落成可判定的终态，不靠错误文案猜测', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '删掉它' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c2', tool: 'apply_patch', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c2', exit_code: null, error: '用户拒绝', result: 'denied' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'cancelled' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    const resultEvent = journal.find(event => event['type'] === 'tool/result')
    const result = resultEvent?.['data'] as { message: { isError: boolean }; error: { name: string; code: string } }
    expect(result.error.name).toBe('Denied')
    expect(result.message.isError).toBe(true)
    // validator 的前提：带 error 时 isError 必须为真，否则 wire 校验直接抛
    expect(() => assertSessionWireEvent(resultEvent!)).not.toThrow()
    expect((journal.find(event => event['type'] === 'turn/end')?.['data'] as { reason: unknown }).reason)
      .toEqual({ kind: 'aborted', reason: { kind: 'user' } })

    await iterator.return?.(undefined)
    dispose()
  })

  it('采样迭代推进 step 而不是新开端回', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '两轮' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'read_file', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' })
    bridge.emit('agent-new', { type: 'turn_started', turn: 2 })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 2, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    expect(journal.filter(event => event['type'] === 'turn/start')).toHaveLength(1)
    const steps = journal.filter(event => event['type'] === 'step/start')
    expect(steps).toHaveLength(2)
    expect((steps[1]?.['data'] as { step: number }).step).toBe(1)
    expect(journal.filter(event => event['type'] === 'turn/end')).toHaveLength(1)
    // 没有任何 delta 就不落假正文
    expect(journal.filter(event => event['type'] === 'assistant/message')).toHaveLength(0)

    await iterator.return?.(undefined)
    dispose()
  })

  it('中断收口时半截正文落档并打 interrupted 标记', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-9', content: [{ type: 'text', text: '讲讲' }] })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '说到一半' })
    // 宿主确认取消后才发这条：中断路径的终态由后端给出，适配层不自己判
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'cancelled' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    const assistant = journal.find(event => event['type'] === 'assistant/message')?.['data'] as {
      interrupted?: boolean
      message: { content: unknown[] }
    }
    expect(assistant.interrupted).toBe(true)
    expect(assistant.message.content).toEqual([{ type: 'text', text: '说到一半' }])
    journal.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())

    await iterator.return?.(undefined)
    dispose()
  })

  it('没观察过的会话交空快照，而不是挂着不返回', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-unknown' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    expect(snapshot['type']).toBe('snapshot')
    expect(snapshot['cursor']).toBe(-1)
    expect(snapshot['records']).toEqual([])
    expect(snapshot['assistantStream']).toEqual({ revision: 0 })
    // 空态之后不该再有帧：下一次 next 必须干净地结束而不是永久挂起
    expect((await iterator.next()).done).toBe(true)
    dispose()
  })
})

describe('适配层的命令面映射', () => {
  it('create 走宿主建档、prompt 带上会话身份', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    expect(await session.create({ cwd: 'W' })).toEqual({ ok: true, value: { sessionId: 'agent-new' } })
    expect(bridge.calls.map(call => call.command)).toContain('create_session')

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-2', content: [{ type: 'text', text: '继续' }] })
    expect(bridge.calls.find(call => call.command === 'send_task')?.args).toEqual({ task: '继续', agentId: 'agent-new' })
    dispose()
  })

  it('list 返回带 sessionId 的 wire 形状，running 反映真实在跑的回合', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const idle = (await session.list())['value'] as { items: Record<string, unknown>[] }
    expect(idle.items[0]).toMatchObject({ sessionId: 'agent-new', cwd: 'W', running: false, formatStatus: 'current' })

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-3', content: [{ type: 'text', text: '在跑' }] })
    const busy = (await session.list())['value'] as { items: { running: boolean }[] }
    expect(busy.items[0]?.running).toBe(true)

    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })
    const done = (await session.list())['value'] as { items: { running: boolean }[] }
    expect(done.items[0]?.running).toBe(false)
    dispose()
  })

  it('cancel 只转发给宿主，不提前造终态', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-4', content: [{ type: 'text', text: '停' }] })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '半截' })
    await session.cancel({ sessionId: 'agent-new' })
    expect(bridge.calls.some(call => call.command === 'cancel_task')).toBe(true)

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)
    // 没有 turn_completed 就不该有 turn/end，也不该提前定稿半截正文
    expect(journal.some(event => event['type'] === 'turn/end')).toBe(false)
    expect(journal.some(event => event['type'] === 'assistant/message')).toBe(false)
    expect(snapshot['cursor']).toBe(journal.at(-1)?.['seq'])

    await iterator.return?.(undefined)
    dispose()
  })

  it('rename 写回宿主并同步本地标题缓存，清空后投影回落 null 由客户端派生', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-5', content: [{ type: 'text', text: '第一条消息' }] })

    const renamed = await session.rename({ sessionId: 'agent-new', title: '  我的会话  ' })
    // 宿主回的是去掉首尾空白后的生效标题，适配层原样上交（不替宿主做归一化）
    expect(renamed).toMatchObject({ ok: true, value: { title: '我的会话' } })
    // 请求也原样转发：归一化是宿主（rollout::set_title）的职责，适配层不复制一份规则
    expect(bridge.calls.find(call => call.command === 'rename_session')?.args)
      .toEqual({ agentId: 'agent-new', title: '  我的会话  ' })
    // 本地缓存同步：下一次快照的标题投影用新名字，而不是改名前的旧值
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as { projections: { values: { title: string | null } } }
    expect(snapshot.projections.values.title).toBe('我的会话')
    await iterator.return?.(undefined)

    // 空标题 = 清除自定义：投影回落 null（客户端据此派生标题），而不是留下空串
    const cleared = await session.rename({ sessionId: 'agent-new', title: '   ' })
    expect(cleared).toMatchObject({ ok: true, value: { title: '' } })
    const iterator2 = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot2 = (await iterator2.next()).value as { projections: { values: { title: string | null } } }
    expect(snapshot2.projections.values.title).toBeNull()
    await iterator2.return?.(undefined)
    dispose()
  })

  it('rename 失败按 wire 契约回 {ok:false}：超长标题映射 session/title-invalid，其余按网关内部错误', async () => {
    const bridge = scriptedBridge(HOST_SETTINGS, [], { code: 'title-invalid', message: '标题最长 120 字，当前 121 字' })
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const invalid = await session.rename({ sessionId: 'agent-new', title: '字'.repeat(121) })
    expect(invalid['ok']).toBe(false)
    const error = invalid['error'] as { name: string; code: string; message: string; details: { sessionId?: string } }
    // DSH 为非法标题声明的正式错误码，不是 rejection——客户端读 result.ok 就能拿到原因
    expect(error.name).toBe('RemoteError')
    expect(error.code).toBe('session/title-invalid')
    expect(error.message).toContain('120')
    expect(error.details.sessionId).toBe('agent-new')
    dispose()

    const bridge2 = scriptedBridge(HOST_SETTINGS, [], { code: 'not-found', message: '会话档案不存在：ghost' })
    const { remote: remote2, dispose: dispose2 } = createAdapter({ bridge: bridge2 })
    const session2 = faceOf(remote2).session
    const missing = await session2.rename({ sessionId: 'ghost', title: '名字' })
    expect(missing['ok']).toBe(false)
    expect((missing['error'] as { code: string }).code).toBe('gateway/internal')
    expect((missing['error'] as { message: string }).message).toContain('不存在')
    dispose2()
  })

  it('page 交出已提交事件，throughSeq 按内层信封的 seq 截断', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-10', content: [{ type: 'text', text: '分页' }] })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '正文' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const whole = (await session.page({ sessionId: 'agent-new' }))['value'] as { records: { event: Record<string, unknown> }[]; cursor: number }
    expect(whole.records.map(record => record.event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message', 'assistant/message', 'step/end', 'turn/end',
    ])
    expect(whole.cursor).toBe(5)
    const windowed = (await session.page({ sessionId: 'agent-new', throughSeq: 2 }))['value'] as { records: { event: { seq: number } }[] }
    expect(windowed.records.map(record => record.event.seq)).toEqual([0, 1, 2])
    dispose()
  })

  it('按游标分支用档案对账表换算条目数，表外才拒绝', async () => {
    // 一个条目会展开成多条 journal 事件：seq 与档案条目数不是一回事，
    // 直接把 seq 当 keep_items 传给后端就会从历史中间切断（有 call 无 output → 续跑 400）。
    const archive = [
      { kind: 'message', role: 'user', text: '第一步' },
      { kind: 'message', role: 'assistant', text: '先看', reasoning: '想' },
      { kind: 'tool_call', call_id: 'c1', tool: 'read_file', arguments: '{}', output: '内容' },
      { kind: 'message', role: 'user', text: '第二步' },
    ]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    // 档案四条 → 事件 13 条：条目 2（那条 tool_call）展开到的最后一个 seq 是 5，
    // 而 6/7 只是 step/end、turn/end 这类边界事件。截断点落在 8（边界事件上）也必须
    // 只保留前 3 个完整条目——按 seq 直接当条数就会切出"有调用没结果"的历史。
    const forked = await session.fork({ sessionId: 'agent-old', throughSeq: 8 })
    expect(forked['ok']).toBe(true)
    const forkCall = bridge.calls.find(call => call.command === 'fork_session')
    expect(forkCall?.args).toEqual({ agentId: 'agent-old', keepItems: 3 })

    // 截断点落在对账表覆盖不到的位置：宁可不接，也不猜一个条目数
    const beyond = await session.fork({ sessionId: 'agent-old', throughSeq: journal.length + 50 })
    expect(beyond['ok']).toBe(false)
    expect(bridge.calls.filter(call => call.command === 'fork_session')).toHaveLength(1)

    // 分叉出来的会话必须立刻能看到拷来的历史。此前 fork 预置了一个空 journal，
    // follow 因此命中"本进程已观察过"直接交空快照——用户点完分叉看到的是一个
    // 历史全空的会话，而档案里条目明明拷过去了。
    const forkedIterator = session.follow({ address: { kind: 'session', sessionId: 'agent-fork' } })
    const forkedEvents = eventsOf((await forkedIterator.next()).value as Record<string, unknown>)
    expect(forkedEvents.map(event => event.type)).toContain('user/message')
    expect(forkedEvents.map(event => event.type)).toContain('assistant/message')
    await forkedIterator.return?.(undefined)

    // 只有实时回合、没从档案重建过的会话同样没有对账表
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-11', content: [{ type: 'text', text: '分支' }] })
    const unaligned = await session.fork({ sessionId: 'agent-new', throughSeq: 1 })
    expect(unaligned['ok']).toBe(false)
    await iterator.return?.(undefined)
    dispose()
  })

  it('没有审批 UI 在听时按拒绝收口，绝不静默放行', async () => {
    const bridge = scriptedBridge()
    const { dispose } = createAdapter({ bridge })
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r1', tool: 'unified_exec', args: {}, risk: 'execute' })
    await vi.waitFor(() => {
      expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r1', decision: 'deny' })
    })
    dispose()
  })

  it('审批卡回答 allowed-once 时回 allow', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    // 第二个参数就是 DSH 审批卡会收到的监听器：它回答 allowed-once / rejected。
    faceOf(remote).$on('approval/request', async () => 'allowed-once')
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r2', tool: 'unified_exec', args: {}, risk: 'execute' })
    await vi.waitFor(() => {
      expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r2', decision: 'allow' })
    })
    dispose()
  })

  it('审批请求按 DSH 声明的载荷派发，并把会话作用域绑给监听器', async () => {
    // 真机踩过一次：监听器是普通函数、靠 `this` 认会话（ui-approval 的第一行就是
    // ctx.sessions.scopeOf(this)）。裸调 listener 会让审批卡根本不出现，后端只看到
    // 一个没人裁决的请求，最终按拒绝收口——表现像"用户拒绝了"，实际是接线错了。
    const bridge = scriptedBridge()
    const scope = { sessionId: 'agent-new' }
    const { remote, dispose } = createAdapter({ bridge, approvalScope: () => scope })
    const seen: { self: unknown; request: Record<string, unknown> }[] = []
    faceOf(remote).$on('approval/request', async function (this: unknown, request: Record<string, unknown>) {
      seen.push({ self: this, request })
      return 'allowed-once'
    })
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r9', tool: 'write_file', args: { path: 'a.txt' }, risk: 'write_in_workspace' })
    await vi.waitFor(() => { expect(seen).toHaveLength(1) })
    expect(seen[0]!.self).toBe(scope)
    expect(seen[0]!.request).toMatchObject({ agent: { id: 'agent-new' }, toolName: 'write_file' })
    // args/risk 不在 DSH 的载荷声明里，必须并进 reason，否则卡片只写工具名不说执行什么
    expect(String(seen[0]!.request['reason'])).toContain('write_file')
    expect(String(seen[0]!.request['reason'])).toContain('a.txt')
    expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r9', decision: 'allow' })
    dispose()
  })

  it('审批监听器抛错时按拒绝收口并留下日志，不冒充用户裁决', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const logged = vi.spyOn(console, 'error').mockImplementation(() => {})
    faceOf(remote).$on('approval/request', async () => {
      throw new Error('作用域取不到')
    })
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r10', tool: 'write_file', args: {}, risk: 'write_in_workspace' })
    await vi.waitFor(() => {
      expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r10', decision: 'deny' })
    })
    // 静默按拒绝回执会让一次接线错误长得像一次正常否决
    expect(logged.mock.calls.some(call => String(call[0]).includes('抛错'))).toBe(true)
    logged.mockRestore()
    dispose()
  })

  it('回合的开与收都广播 api-session/status——Stop 按钮只认这个', async () => {
    // 输入卡的 Stop 条件是 `running && !sending`，而 `running` 来自
    // `api-session/status`（session-controller client 的 $on）与 session.list 摘要，
    // 不看 journal 的 turn/end。只发 journal 不发状态，真机表现就是"跑着长任务
    // 却没有停止按钮"——中断通道本身是通的，用户按不到。
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const seen: [string, boolean][] = []
    const face = remote as unknown as {
      $on(event: string, listener: (...args: never[]) => unknown): () => void
      session: { prompt(request: Record<string, unknown>): Promise<unknown> }
    }
    face.$on('api-session/status', (sessionId: string, running: boolean) => {
      seen.push([sessionId, running])
      return Promise.resolve('')
    })
    await face.session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '跑个长的' }] })
    expect(seen).toEqual([['agent-new', true]])
    bridge.emit('agent-new', { type: 'turn_started', turn: 1 })
    // 同一个回合里不该重复报运行中
    expect(seen).toEqual([['agent-new', true]])
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })
    expect(seen).toEqual([['agent-new', true], ['agent-new', false]])
    dispose()
  })

  it('工具结果检视流：冷读报不可用，结果落定后交真值', async () => {
    // 详情面板的数据本来就在 journal 里，此前只是没入口：落到"未接线"代理时，
    // 点开工具卡只会报命令面未接线。语义照 DSH host：available:false 与
    // "结果本身是 JSON null" 必须分得开。
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const face = remote as unknown as {
      toolResults: { watch(request: Record<string, unknown>, signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> }
    }

    // 本进程没观察过的会话：不猜一份结果出来，直接不可用
    const cold = face.toolResults.watch({ sessionId: 'agent-ghost', seq: 3 })
    expect((await cold.next()).value).toEqual({ available: false })
    await cold.return(undefined)

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-20', content: [{ type: 'text', text: '列目录' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c9', tool: 'unified_exec', args: { cmd: 'ls' } })
    const pending = face.toolResults.watch({ sessionId: 'agent-new', seq: 999 })
    // 结果还没落定时先交不可用，而不是挂住
    expect((await pending.next()).value).toEqual({ available: false })
    bridge.emit('agent-new', { type: 'tool_call_output_delta', call_id: 'c9', channel: 'stdout', delta: 'port.txt\n' })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c9', exit_code: 0, error: null })
    const paged = (await session.page({ sessionId: 'agent-new' }))['value'] as Record<string, unknown>
    const resultSeq = (eventsOf(paged).find(event => event.type === 'tool/result') as { seq: number }).seq
    const live = face.toolResults.watch({ sessionId: 'agent-new', seq: resultSeq })
    const first = (await live.next()).value as { available: boolean; value?: Record<string, unknown> }
    // 落定前那一帧可能是 false（取决于订阅与入队的先后），继续读到 available 为止
    const settledFrame = first.available
      ? first
      : (await live.next()).value as { available: boolean; value?: Record<string, unknown> }
    expect(settledFrame.available).toBe(true)
    expect(settledFrame.value).toMatchObject({ toolCallId: 'c9', content: [{ type: 'text', text: 'port.txt\n\n[exit code: 0]' }] })
    await live.return(undefined)
    await pending.return(undefined)
    dispose()
  })

  it('侧栏冷标题走摘要投影提示块；重开会话时标题从宿主回显预热，不从档案重新派生', async () => {
    const archive = [{ kind: 'message', role: 'user', text: '一条很长的用户消息，本地派生规则只取前十二个字' }]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session

    // list 面：有标题的行带 cached 投影提示块，标题用宿主回显（已合并 titles.json）
    const listed = (await session.list())['value'] as { items: Record<string, unknown>[] }
    expect(listed.items[0]).toMatchObject({
      sessionId: 'agent-new',
      projections: { kind: 'cached', asOfSeq: 0, values: { title: '首条任务' } },
    })

    // 重开：宿主回显标题优先于本地派生——档案首条用户消息的前 12 字不是宿主标题，
    // 若标题被本地重派生，这里会拿到"一条很长的用户消息，本地派"而失败
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as { projections: { values: { title: string | null } } }
    expect(snapshot.projections.values.title).toBe('首条任务')
    await iterator.return?.(undefined)
    dispose()
  })

  it('未接线的命名空间显式抛错并指向核销清单', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const workspaceFiles = faceOf(remote).workspaceFiles
    await expect(workspaceFiles['read']!()).rejects.toThrow(/未接线/)
    await expect(workspaceFiles['read']!()).rejects.toThrow(/SH-040/)
    await expect(faceOf(remote).session.attachment()).rejects.toThrow(/SH-040/)
    dispose()
  })

  it('重开旧会话：从档案重建 journal，回合边界与 wire 校验都成立', async () => {
    const archive = [
      { kind: 'message', role: 'user', text: '把端口改成 9090' },
      { kind: 'message', role: 'assistant', text: '我先看一眼', reasoning: '要确认文件位置' },
      { kind: 'tool_call', call_id: 'c1', tool: 'unified_exec', arguments: '{"cmd":"ls"}', output: 'port.txt\n[exit code: 0]' },
      { kind: 'message', role: 'assistant', text: '改好了' },
      { kind: 'tool_call', call_id: 'c2', tool: 'apply_patch', arguments: '{}', output: '失败\n[exit code: 1]' },
    ]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const iterator = faceOf(remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)

    expect(journal.map(event => event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message',
      'assistant/message', 'tool/call', 'tool/result',
      'step/end', 'step/start',
      'assistant/message', 'tool/call', 'tool/result',
      'step/end', 'turn/end',
    ])
    journal.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())
    expect(snapshot['cursor']).toBe(journal.at(-1)?.['seq'])

    // 档案里的思考原文回到 reasoning 块，不混进正文
    const assistant = journal.find(event => event['type'] === 'assistant/message')?.['data'] as {
      message: { content: { type: string }[] }
    }
    expect(assistant.message.content.map(block => block.type)).toEqual(['reasoning', 'text'])
    // 退出码标记是档案里本来就有的尾标记，重建时不能再追加一次
    const results = journal.filter(event => event['type'] === 'tool/result')
      .map(event => (event['data'] as { message: { content: { text: string }[] } }).message.content[0]?.text)
    expect(results[0]).toBe('port.txt\n[exit code: 0]')
    expect((journal.filter(event => event['type'] === 'tool/result')[1]?.['data'] as { message: { isError: boolean } }).message.isError)
      .toBe(true)

    await iterator.return?.(undefined)
    dispose()
  })

  it('没有观察过的会话档案也为空时交空快照，不挂起', async () => {
    const bridge = scriptedBridge(undefined, [])
    const { remote, dispose } = createAdapter({ bridge })
    const iterator = faceOf(remote).session.follow({ address: { kind: 'session', sessionId: 'agent-empty' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    expect(snapshot['cursor']).toBe(-1)
    expect(snapshot['records']).toEqual([])
    expect((await iterator.next()).done).toBe(true)
    dispose()
  })

  it('同一份历史：实时跑出来的事件序列与重开时重建的完全一致', async () => {
    // 这条等价性是 lib.rs 里"展示与回放必须同源"的前端面判据：不一致就意味着用户
    // 重开旧会话看到的分组（回合头、用时、工具卡归属）和刚跑完时不一样。
    const liveBridge = scriptedBridge()
    const live = createAdapter({ bridge: liveBridge })
    const liveSession = faceOf(live.remote).session
    await liveSession.prompt({ sessionId: 'agent-new', requestId: 'rpc-eq', content: [{ type: 'text', text: '第一步' }] })
    liveBridge.emit('agent-new', { type: 'agent_message_delta', delta: '先看' })
    liveBridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'read_file', args: {} })
    liveBridge.emit('agent-new', { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stdout', delta: '内容' })
    liveBridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' })
    liveBridge.emit('agent-new', { type: 'turn_started', turn: 2 })
    liveBridge.emit('agent-new', { type: 'agent_message_delta', delta: '改好了' })
    liveBridge.emit('agent-new', { type: 'turn_completed', turn: 2, reason: 'completed' })

    const liveIterator = liveSession.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const liveTypes = eventsOf((await liveIterator.next()).value as Record<string, unknown>)
      .map(event => event['type'])
    await liveIterator.return?.(undefined)
    live.dispose()

    // 同一轮任务落进档案的形状（FunctionCall + FunctionCallOutput 并成一条 ToolCall）
    const archive = [
      { kind: 'message', role: 'user', text: '第一步' },
      { kind: 'message', role: 'assistant', text: '先看' },
      { kind: 'tool_call', call_id: 'c1', tool: 'read_file', arguments: '{}', output: '内容\n[exit code: 0]' },
      { kind: 'message', role: 'assistant', text: '改好了' },
    ]
    const reopened = createAdapter({ bridge: scriptedBridge(undefined, archive) })
    const replayIterator = faceOf(reopened.remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const replayTypes = eventsOf((await replayIterator.next()).value as Record<string, unknown>)
      .map(event => event['type'])
    await replayIterator.return?.(undefined)
    reopened.dispose()

    expect(replayTypes).toEqual(liveTypes)
  })

  it('模型席位只反映宿主真实配置，选模写回服务商文档并推投影', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const catalog = (await session.modelCatalog())['value'] as {
      groups: { id: string; models: { id: string }[] }[]
      routableProviders: string[]
      failures: { id: string; message: string }[]
      default: Record<string, string>
    }
    // 没配密钥的那家进 failures 并带原因，而不是从目录里静默消失：
    // 用户看得见"为什么不能用"，才会去配，而不是以为自己没添加成功。
    expect(catalog.groups).toEqual([{ id: 'longcat', name: 'LongCat', models: [{ id: 'LongCat-2.5', name: 'LongCat 2.5' }] }])
    expect(catalog.routableProviders).toEqual(['longcat'])
    expect(catalog.failures).toEqual([{ id: 'ghost', name: 'Ghost', message: '未配置密钥（引用名 GHOST_API_KEY）' }])
    expect(catalog.default).toEqual({ provider: 'longcat', model: 'LongCat-2.5' })

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-12', content: [{ type: 'text', text: '换模型' }] })
    const before = bridge.calls.length
    const chosen = await session.selectModel({ sessionId: 'agent-new', provider: 'longcat', model: 'LongCat-2.5' })
    expect(chosen['ok']).toBe(true)
    const write = bridge.calls.slice(before).find(call => call.command === 'save_providers')
    const saved = write?.args as { providers: Record<string, unknown>[]; activeProvider: string; expectedRevision: number }
    // 席位切换写的是**整份**文档：没被选中的那家也必须原样在场，否则切一次模型
    // 就把别的服务商删了。
    expect(saved.providers.map(row => row.id)).toEqual(['longcat', 'ghost'])
    expect(saved.activeProvider).toBe('longcat')
    expect(saved.expectedRevision).toBe(4)
    expect(saved.providers[0]).toMatchObject({
      id: 'longcat',
      base_url: 'https://lc/v1',
      secret_ref: 'LONGCAT_API_KEY',
      model: 'LongCat-2.5',
      // 嵌套结构体按 04 §4 的落盘名送，Tauri 只转顶层参数名
      models: [{ id: 'LongCat-2.5', name: 'LongCat 2.5', context_window: 128_000, max_tokens: 8_192, input: ['text'] }],
    })
    expect(saved.providers[1]).toMatchObject({ id: 'ghost', model: 'g-1', secret_ref: 'GHOST_API_KEY' })
    dispose()
  })

  it('席位选一个清单外的模型就是拒绝，不写出一份后端会拒收的文档', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const result = await faceOf(remote).session.selectModel({ sessionId: 'a', provider: 'longcat', model: '不存在的模型' })
    expect(result['ok']).toBe(false)
    expect((result['error'] as Error).message).toContain('模型清单里没有')
    expect(bridge.calls.some(call => call.command === 'save_providers')).toBe(false)
    dispose()
  })

  it('只有 [model] 回落段的安装：席位显示未配置，不编一个存不回密钥的档位', async () => {
    const bridge = scriptedBridge({ ...HOST_SETTINGS, activeProvider: 'openai-responses', providerRevision: 0, hasApiKey: true, providers: [] })
    const { remote, dispose } = createAdapter({ bridge })
    const catalog = (await faceOf(remote).session.modelCatalog())['value'] as { groups: unknown[]; default: Record<string, string> }
    // `[model].provider` 带连字符，不是 DSH 合法的凭据引用名；把它当档位显示，
    // 用户在界面里填的 Key 会存成 OPENAI_RESPONSES_API_KEY，而后端读的是 openai-responses。
    expect(catalog.groups).toEqual([])
    expect(catalog.default).toEqual({ provider: '', model: '' })
    dispose()
  })
})

describe('设置 / 凭据 / 目录三张面经 remote 可达', () => {
  interface FaceProxy {
    settings: Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>
    llm: Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>
    credentials: Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>
    $on(event: string, listener: (...args: never[]) => unknown): () => void
  }

  const faces = (remote: unknown): FaceProxy => remote as FaceProxy

  it('三个命名空间由适配层直供，不再落到"未接线"代理', async () => {
    const { remote, dispose } = createAdapter({ bridge: scriptedBridge() })
    const face = faces(remote)
    expect((await face.settings['describe']!())['ok']).toBe(true)
    const listed = await face.llm['listConfigurableProviders']!()
    expect((listed['value'] as unknown[]).length).toBeGreaterThan(0)
    const described = await face.credentials['describe']!(['LONGCAT_API_KEY', 'GHOST_API_KEY'])
    expect(described['value']).toMatchObject({
      LONGCAT_API_KEY: { configured: true, source: 'shaniu-dpapi' },
      GHOST_API_KEY: { configured: false },
    })
    dispose()
  })

  it('存密钥后向已注册的监听器广播；退订后不再收到', async () => {
    const { remote, dispose } = createAdapter({ bridge: scriptedBridge() })
    const face = faces(remote)
    const seen: string[] = []
    const off = face.$on('llm/adapters-updated', () => { seen.push('adapters') })
    await face.credentials['set']!('MINIMAX_API_KEY', 'sk-m')
    expect(seen).toEqual(['adapters'])
    off()
    await face.credentials['set']!('MINIMAX_API_KEY', 'sk-m')
    // 退订之后不该再被叫到，否则监听器会越积越多
    expect(seen).toEqual(['adapters'])
    dispose()
  })
})

describe('权限挡位与底部指标的数据面', () => {
  interface PermissionTestFace {
    permissionPresets: { catalog(): Promise<Record<string, unknown>> }
    commands: { execute(sessionId: string, line: string, args?: readonly string[]): Promise<Record<string, unknown>> }
    session: SessionFace & {
      control(): AsyncGenerator<Record<string, unknown>>
      updateQueue(request: Record<string, unknown>): Promise<Record<string, unknown>>
    }
  }

  const faceOf2 = (remote: unknown): PermissionTestFace => remote as PermissionTestFace

  it('挡位目录随宿主沙箱档反推默认，切换写 save_settings 并让快照/基线显示新挡位', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf2(remote)

    // 目录：静态三挡（无实验档 auto），默认反推自宿主 sandboxMode（workspace_write）
    const catalog = (await face.permissionPresets.catalog())['value'] as {
      options: { value: string; name: string }[]
      defaultOptions: { value: string }[]
      defaultPreset: string
    }
    expect(catalog.options.map(option => option.value)).toEqual(['read-only', 'workspace-write', 'danger-full-access'])
    expect(catalog.defaultOptions.map(option => option.value)).toEqual(['read-only', 'workspace-write', 'danger-full-access'])
    expect(catalog.defaultPreset).toBe('workspace-write')

    // 切挡前的快照基线：挡位投影 = 宿主当前档
    await face.session.create({})
    const before = face.session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snap1 = (await before.next()).value as { projections: { values: Record<string, unknown> } }
    expect(snap1.projections.values['permissions']).toEqual({ currentValue: 'workspace-write' })
    await before.return?.(undefined)

    // 提交：/permission 命令 → save_settings，其余必填参数传宿主现值原样写回
    const callsBefore = bridge.calls.length
    const result = await face.commands.execute('agent-new', '/permission danger-full-access')
    expect(result).toMatchObject({ ok: true, value: { matched: true } })
    expect(bridge.calls.slice(callsBefore).find(call => call.command === 'save_settings')?.args)
      .toEqual({ base_url: 'https://lc/v1', model: 'LongCat-2.5', workspace_root: 'W', sandbox_mode: 'danger_full_access' })

    // 宿主落盘后 get_settings 回读新值：目录默认与重开快照都反映新挡位
    const catalog2 = (await face.permissionPresets.catalog())['value'] as { defaultPreset: string }
    expect(catalog2.defaultPreset).toBe('danger-full-access')
    const after = face.session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snap2 = (await after.next()).value as { projections: { values: Record<string, unknown> } }
    expect(snap2.projections.values['permissions']).toEqual({ currentValue: 'danger-full-access' })
    await after.return?.(undefined)

    // control 基线同样带挡位（客户端重连时不丢当前值）
    const control = face.session.control()
    const baseline = (await control.next()).value as {
      value: { projections: Record<string, { values: Record<string, unknown> }> }
    }
    expect(baseline.value.projections['agent-new']?.values['permissions'])
      .toEqual({ currentValue: 'danger-full-access' })
    await control.return?.(undefined)
    dispose()
  })

  it('commands.execute：非本命令的行与未知挡位都回未命中，不写宿主', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf2(remote)
    // matched 判据是"返回值非 undefined"（session.ts）：未命中必须 ok:true + value undefined
    const other = await face.commands.execute('agent-new', '/plan on')
    expect(other).toMatchObject({ ok: true })
    expect(other['value']).toBeUndefined()
    const unknownPreset = await face.commands.execute('agent-new', '/permission auto')
    expect(unknownPreset['value']).toBeUndefined()
    const bare = await face.commands.execute('agent-new', '/permission')
    expect(bare['value']).toBeUndefined()
    expect(bridge.calls.some(call => call.command === 'save_settings')).toBe(false)
    dispose()
  })

  it('挡位切换被宿主拒绝时按 {ok:false} 回给客户端，目录默认保持旧档', async () => {
    // wire 契约：UI 的错误处理读 result.ok，穿透 rejection 会绕过它
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, '沙箱模式取值非法，可用：read_only / workspace_write / danger_full_access')
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf2(remote)
    const result = await face.commands.execute('agent-new', '/permission danger-full-access')
    expect(result['ok']).toBe(false)
    expect((result['error'] as { message: string }).message).toContain('沙箱模式取值非法')
    const catalog = (await face.permissionPresets.catalog())['value'] as { defaultPreset: string }
    expect(catalog.defaultPreset).toBe('workspace-write')
    dispose()
  })

  it('底部指标：回合/步/墙钟折叠成 sessionStats，首个采样后用量与占用投影出现', async () => {
    const bridge = scriptedBridge()
    // 受控时钟：墙钟口径的断言（llm/ttft/decode）必须确定性，不赌事件处理耗时
    let clock = 1_000_000
    const { remote, dispose } = createAdapter({ bridge, now: () => clock })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-m1', content: [{ type: 'text', text: '指标' }] })
    clock += 100
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '正文' })
    clock += 200
    bridge.emit('agent-new', { type: 'token_usage', turn: 1, input_tokens: 10, cached_input_tokens: 2, output_tokens: 4 })
    clock += 300
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const values = ((await iterator.next()).value as { projections: { values: Record<string, unknown> } }).projections.values
    // llmMs = step/start → 消息落定（600）；ttftMs = 起点 → 首 delta（100）；
    // decodeMs = 首 delta → 落定（500），decodeTokens 取该次采样的输出桶
    expect(values['sessionStats']).toEqual({
      turns: 1, steps: 1, llmMs: 600, toolMs: 0, ttftMs: 100, ttftSteps: 1, decodeMs: 500, decodeTokens: 4,
    })
    // 用量四桶：uncached = 10 − 2 = 8（缓存只算一次）
    expect(values['tokenUsage']).toEqual({ uncachedInputTokens: 8, outputTokens: 4, cacheReadTokens: 2, cacheWriteTokens: 0 })
    // 占用：分子 = prompt 侧全量（10），分母来自生效模型的 contextWindow
    expect(values['contextPressure']).toEqual({ pressureTokens: 10, projectedTokens: 10, contextWindow: 128_000 })
    await iterator.return?.(undefined)
    dispose()
  })

  it('底部指标：工具墙钟按 callId 配对结算，没等到结果的调用不计时长', async () => {
    const bridge = scriptedBridge()
    let clock = 2_000_000
    const { remote, dispose } = createAdapter({ bridge, now: () => clock })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-m2', content: [{ type: 'text', text: '跑工具' }] })
    clock += 100
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'unified_exec', args: {} })
    clock += 700
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' })
    clock += 100
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c2', tool: 'read_file', args: {} })
    clock += 100
    // c2 没等到 tool_call_end 就收口：漏算时长是诚实缺省，不编一个值
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const values = ((await iterator.next()).value as { projections: { values: Record<string, unknown> } }).projections.values
    expect(values['sessionStats']).toEqual({
      turns: 1, steps: 1, llmMs: 0, toolMs: 700, ttftMs: 0, ttftSteps: 0, decodeMs: 0, decodeTokens: 0,
    })
    // 没烧过 token：用量/占用投影不挂键，pill 与仪表按缺席隐藏
    expect(values['tokenUsage']).toBeUndefined()
    expect(values['contextPressure']).toBeUndefined()
    await iterator.return?.(undefined)
    dispose()
  })

  it('重开旧会话：回放折叠出回合/步计数，没有可信时刻与用量就不伪造', async () => {
    const archive = [
      { kind: 'message', role: 'user', text: '第一问' },
      { kind: 'message', role: 'assistant', text: '答一' },
      { kind: 'tool_call', call_id: 'c1', tool: 'read_file', arguments: '{}', output: 'ok\n[exit code: 0]' },
      { kind: 'message', role: 'user', text: '第二问' },
      { kind: 'message', role: 'assistant', text: '答二' },
    ]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const iterator = faceOf(remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const values = ((await iterator.next()).value as { projections: { values: Record<string, unknown> } }).projections.values
    expect(values['sessionStats']).toEqual({
      turns: 2, steps: 2, llmMs: 0, toolMs: 0, ttftMs: 0, ttftSteps: 0, decodeMs: 0, decodeTokens: 0,
    })
    expect(values['tokenUsage']).toBeUndefined()
    expect(values['contextPressure']).toBeUndefined()
    expect(values['permissions']).toEqual({ currentValue: 'workspace-write' })
    await iterator.return?.(undefined)
    dispose()
  })
})

describe('Plan 芯片与客户端排队', () => {
  interface QueueTestFace {
    commands: { execute(sessionId: string, line: string): Promise<Record<string, unknown>> }
    session: SessionFace & {
      updateQueue(request: Record<string, unknown>): Promise<Record<string, unknown>>
    }
  }

  const faceOf3 = (remote: unknown): QueueTestFace => remote as QueueTestFace

  /** 开一次 follow 取投影值（用完即关，避免生成器挂住）。 */
  async function projectionValuesOf(session: SessionFace, sessionId: string): Promise<Record<string, unknown>> {
    const iterator = session.follow({ address: { kind: 'session', sessionId } })
    const values = ((await iterator.next()).value as { projections: { values: Record<string, unknown> } }).projections.values
    await iterator.return?.(undefined)
    return values
  }

  it('Plan 芯片：enter/exit_plan_mode 按执行结果驱动 plan 投影，被拒的退出保持计划态', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf3(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-p1', content: [{ type: 'text', text: '做个计划' }] })

    // enter_plan_mode 免审批必执行 → 计划态开
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'e1', tool: 'enter_plan_mode', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'e1', exit_code: 0, error: null, result: 'ok' })
    expect(await projectionValuesOf(session, 'agent-new')).toMatchObject({ plan: { active: true, pending: false } })

    // exit_plan_mode 被拒（审批卡点了拒绝）→ 保持计划态，与宿主 PlanState 一致
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'x1', tool: 'exit_plan_mode', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'x1', exit_code: null, error: '用户拒绝', result: 'denied' })
    expect(await projectionValuesOf(session, 'agent-new')).toMatchObject({ plan: { active: true } })

    // 获批执行 → 退出，投影键缺席（芯片与 plan 占位都按缺席判读）
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'x2', tool: 'exit_plan_mode', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'x2', exit_code: 0, error: null, result: 'ok' })
    expect((await projectionValuesOf(session, 'agent-new'))['plan']).toBeUndefined()
    dispose()
  })

  it('重开旧会话：计划态从档案重建——exit 有输出（获批）才算退出', async () => {
    const plannedAndExited = [
      { kind: 'message', role: 'user', text: '规划一下' },
      { kind: 'tool_call', call_id: 'e1', tool: 'enter_plan_mode', arguments: '{}', output: '已进入计划模式' },
      { kind: 'tool_call', call_id: 'x1', tool: 'exit_plan_mode', arguments: '{}', output: '计划已批准' },
    ]
    const exited = createAdapter({ bridge: scriptedBridge(undefined, plannedAndExited) })
    const values1 = await projectionValuesOf(faceOf(exited.remote).session, 'agent-old')
    expect(values1['plan']).toBeUndefined()
    exited.dispose()

    const stillPlanning = [
      { kind: 'message', role: 'user', text: '规划一下' },
      { kind: 'tool_call', call_id: 'e1', tool: 'enter_plan_mode', arguments: '{}', output: '已进入计划模式' },
    ]
    const planning = createAdapter({ bridge: scriptedBridge(undefined, stillPlanning) })
    const values2 = await projectionValuesOf(faceOf(planning.remote).session, 'agent-old')
    expect(values2['plan']).toEqual({ active: true, pending: false })
    planning.dispose()
  })

  it('/plan off 给可行动指引（退出只能走审批卡），其余 /plan 变体照旧未命中', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf3(remote)
    const off = await face.commands.execute('agent-new', '/plan off')
    expect(off['ok']).toBe(false)
    expect((off['error'] as { message: string }).message).toContain('审批卡')
    const activate = await face.commands.execute('agent-new', '/plan')
    expect(activate).toMatchObject({ ok: true })
    expect(activate['value']).toBeUndefined()
    dispose()
  })

  it('客户端排队：运行中提交进 inbox 投影（rpcId 对账），回合收口按序自动补发', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf3(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q1', content: [{ type: 'text', text: '第一条' }] })
    // 运行中再提交：accepted 但不再调 send_task（缓存进队列）
    const queued = await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q2', content: [{ type: 'text', text: '排队第二条' }] })
    expect(queued).toMatchObject({ ok: true, value: { accepted: true } })
    expect(bridge.calls.filter(call => call.command === 'send_task')).toHaveLength(1)

    // 排队中的快照：inbox 行带原始 rpcId（QueueDock 行与回声对账的 join key）
    const values = await projectionValuesOf(session, 'agent-new')
    expect(values['inbox']).toEqual({
      'next-turn': [{
        id: expect.any(String), role: 'user',
        content: [{ type: 'text', text: '排队第二条' }],
        source: { kind: 'user', rpcId: 'rpc-q2' },
      }],
    })

    // 收口 → 自动补发：send_task 第二次调用 + durable user/message 回填原 rpcId
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })
    await vi.waitFor(async () => {
      const paged = (await session.page({ sessionId: 'agent-new' }))['value'] as { records: { event: Record<string, unknown> }[] }
      expect(eventsOf(paged).filter(event => event['type'] === 'user/message')).toHaveLength(2)
    })
    const paged = (await session.page({ sessionId: 'agent-new' }))['value'] as { records: { event: Record<string, unknown> }[] }
    const second = eventsOf(paged).filter(event => event['type'] === 'user/message')[1]
    expect((second?.['data'] as { source: { rpcId?: string } }).source.rpcId).toBe('rpc-q2')
    expect(bridge.calls.filter(call => call.command === 'send_task')).toHaveLength(2)
    expect(bridge.calls.filter(call => call.command === 'send_task')[1]?.args).toEqual({ task: '排队第二条', agentId: 'agent-new' })
    // 队列清空：重开快照不再挂 inbox
    expect((await projectionValuesOf(session, 'agent-new'))['inbox']).toBeUndefined()
    dispose()
  })

  it('updateQueue：编辑/steer 改补发内容与顺序，移除发 spliced 事件退休回声，未知行如实拒绝', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf3(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q1', content: [{ type: 'text', text: '第一条' }] })
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q2', content: [{ type: 'text', text: '第二条' }] })
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q3', content: [{ type: 'text', text: '第三条' }] })

    // 编辑排队的第二条（第二次排队 = shaniu-queued-1）：补发文本随之改变
    const edited = await session.updateQueue({
      sessionId: 'agent-new', itemId: 'shaniu-queued-1',
      action: { kind: 'edit', content: [{ type: 'text', text: '改过的第二条' }] },
    })
    expect(edited).toMatchObject({ ok: true, value: { accepted: true } })
    // steer 第三条（shaniu-queued-2）→ 移到队首（当前回合收口后第一个补发；行保持可见）
    const steered = await session.updateQueue({ sessionId: 'agent-new', itemId: 'shaniu-queued-2', action: { kind: 'steer' } })
    expect(steered).toMatchObject({ ok: true })
    // 移除第二条 → journal 记 agent/inbox/spliced{outcome:canceled}，客户端据此退休回声行
    const removed = await session.updateQueue({ sessionId: 'agent-new', itemId: 'shaniu-queued-1', action: { kind: 'remove' } })
    expect(removed).toMatchObject({ ok: true })

    // 未知行 / 空编辑：如实拒绝而不是静默成功（补发收口前做，排队项还都在）
    const missing = await session.updateQueue({ sessionId: 'agent-new', itemId: 'nope', action: { kind: 'remove' } })
    expect(missing['ok']).toBe(false)
    expect((missing['error'] as { code: string }).code).toBe('session/queue-item-not-found')
    const blank = await session.updateQueue({
      sessionId: 'agent-new', itemId: 'shaniu-queued-2',
      action: { kind: 'edit', content: [{ type: 'text', text: '   ' }] },
    })
    expect(blank['ok']).toBe(false)
    expect((blank['error'] as { code: string }).code).toBe('gateway/bad-request')

    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })
    await vi.waitFor(() => {
      expect(bridge.calls.filter(call => call.command === 'send_task')).toHaveLength(2)
    })
    // 补发的是 steer 到队首的第三条；被移除的第二条永远不会发出
    expect(bridge.calls.filter(call => call.command === 'send_task')[1]?.args)
      .toEqual({ task: '第三条', agentId: 'agent-new' })
    const paged = (await session.page({ sessionId: 'agent-new' }))['value'] as { records: { event: Record<string, unknown> }[] }
    const spliced = eventsOf(paged).find(event => event['type'] === 'agent/inbox/spliced')
    expect(spliced).toBeDefined()
    // 这条事件走客户端运行时会过的同一道 wire 闸
    expect(() => assertSessionWireEvent(spliced!)).not.toThrow()
    expect((spliced!['data'] as { outcome: string; removedCount: number }).outcome).toBe('canceled')
    expect((spliced!['data'] as { removedCount: number }).removedCount).toBe(1)
    dispose()
  })
})

describe('todos 投影（SH-108：todo_write → journal todo/write → TodoDock 数据面）', () => {
  interface TodoTestFace {
    session: SessionFace & {
      control(): AsyncGenerator<Record<string, unknown>>
    }
  }

  const faceOfTodo = (remote: unknown): TodoTestFace => remote as TodoTestFace

  const TODO_ARGS = {
    todos: [
      { content: ' 调研现状 ', status: 'completed' },
      { content: '写实现', status: 'in_progress' },
    ],
  }
  /** trim 后的整表（上游 toTodoList 语义：入库存 trim 结果）。 */
  const TRIMMED_TODOS = [
    { content: '调研现状', status: 'completed' },
    { content: '写实现', status: 'in_progress' },
  ]

  it('todo_write 执行成功：todo/write 落在 tool/call 与 tool/result 之间，投影带 trim 后整表，事件过 wire 闸', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOfTodo(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-t1', content: [{ type: 'text', text: '列个清单' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw1', tool: 'todo_write', args: TODO_ARGS })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw1', exit_code: 0, error: null, result: 'ok' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)
    expect(journal.map(event => event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message', 'tool/call', 'todo/write', 'tool/result',
    ])
    const write = journal.find(event => event['type'] === 'todo/write')
    expect((write!['data'] as { todos: unknown }).todos).toEqual(TRIMMED_TODOS)
    // 这条事件走客户端运行时会过的同一道 wire 闸
    expect(() => assertSessionWireEvent(write!)).not.toThrow()
    // 投影整表替换：TodoDock 从这里拿渲染数据
    expect((snapshot['projections'] as { values: Record<string, unknown> }).values['todos']).toEqual(TRIMMED_TODOS)
    await iterator.return?.(undefined)
    dispose()
  })

  it('失败的调用不落清单：result 非 ok 不 emit，参数不可信时即便 ok 也拒绝推导', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOfTodo(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-t2', content: [{ type: 'text', text: '列个清单' }] })

    // 校验被拒（后端 Err 路径）→ ToolCallResult::Error → 不落
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw2', tool: 'todo_write', args: TODO_ARGS })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw2', exit_code: null, error: 'invalid todos: duplicate content "调研现状"', result: 'error' })
    let snapshot = (await session.follow({ address: { kind: 'session', sessionId: 'agent-new' } }).next()).value as Record<string, unknown>
    expect(eventsOf(snapshot).some(event => event['type'] === 'todo/write')).toBe(false)
    expect((snapshot['projections'] as { values: Record<string, unknown> }).values['todos']).toBeUndefined()

    // 防御位：执行结果 ok 但参数不是对象形状（协议违规）→ 一样不落，不伪造清单
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw3', tool: 'todo_write', args: 'not-an-object' })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw3', exit_code: 0, error: null, result: 'ok' })
    snapshot = (await session.follow({ address: { kind: 'session', sessionId: 'agent-new' } }).next()).value as Record<string, unknown>
    expect(eventsOf(snapshot).filter(event => event['type'] === 'todo/write')).toHaveLength(0)
    expect((snapshot['projections'] as { values: Record<string, unknown> }).values['todos']).toBeUndefined()
    dispose()
  })

  it('回合边界：turn/end 保留已完成的清单可见，下一次 turn/start 清空（显式 null 帧 + 快照缺席）', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOfTodo(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-t3a', content: [{ type: 'text', text: '第一件事' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw1', tool: 'todo_write', args: TODO_ARGS })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw1', exit_code: 0, error: null, result: 'ok' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    // turn/end 不清空：已完成的清单在回合结束后仍可见（上游 fold 注释的语义）
    const control = session.control()
    const baseline = (await control.next()).value as { value: { projections: Record<string, { values: Record<string, unknown> }> } }
    expect(baseline.value.projections['agent-new']!.values['todos']).toEqual(TRIMMED_TODOS)

    // 新回合（turn/start）清空：同一客户端收到显式 null 帧
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-t3b', content: [{ type: 'text', text: '第二件事' }] })
    const frame = (await control.next()).value as { changes: { key: string; val: unknown }[] }
    expect(frame.changes).toEqual([{ key: 'todos', val: null }])
    await control.return?.(undefined)
    // 新观察者从快照进来：todos 键缺席（投影缺席 = TodoDock 不渲染）
    const values = await (async () => {
      const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
      const snap = (await iterator.next()).value as Record<string, unknown>
      await iterator.return?.(undefined)
      return (snap['projections'] as { values: Record<string, unknown> }).values
    })()
    expect(values['todos']).toBeUndefined()
    dispose()
  })

  it('重开旧会话：清单从档案重建（trim 镜像同源），校验被拒的调用不伪造', async () => {
    const archive = [
      { kind: 'message', role: 'user', text: '列个清单' },
      { kind: 'tool_call', call_id: 'tw1', tool: 'todo_write', arguments: JSON.stringify(TODO_ARGS), output: 'Updated todo list: 0 pending, 1 in progress, 1 completed.\n[exit code: 0]' },
      // 后端校验拒绝的调用只留错误文本（Err 路径无 exit 标记）：镜像谓词重演同一裁决
      { kind: 'tool_call', call_id: 'tw2', tool: 'todo_write', arguments: '{"todos":[{"content":"x","status":"pending"},{"content":"x","status":"pending"}]}', output: '参数非法: invalid todos: duplicate content "x"' },
      { kind: 'message', role: 'assistant', text: '清单好了' },
    ]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const iterator = faceOfTodo(remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)
    const types = journal.map(event => event['type'])
    // todo/write 紧跟第一份调用的 tool/call、先于 tool/result；第二份调用没有 todo/write
    expect(types.filter(type => type === 'todo/write')).toHaveLength(1)
    const writeIndex = types.indexOf('todo/write')
    expect(types[writeIndex - 1]).toBe('tool/call')
    expect(types[writeIndex + 1]).toBe('tool/result')
    expect((journal[writeIndex]!['data'] as { todos: unknown }).todos).toEqual(TRIMMED_TODOS)
    journal.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())
    // 投影随快照带出：重开旧会话 TodoDock 直接回到最后一份清单
    expect((snapshot['projections'] as { values: Record<string, unknown> }).values['todos']).toEqual(TRIMMED_TODOS)
    await iterator.return?.(undefined)
    dispose()
  })

  it('同一份历史：todo 的实时序列与重开重建序列逐项一致（含 todo/write 载荷）', async () => {
    const liveBridge = scriptedBridge()
    const live = createAdapter({ bridge: liveBridge })
    const liveSession = faceOfTodo(live.remote).session
    await liveSession.prompt({ sessionId: 'agent-new', requestId: 'rpc-teq', content: [{ type: 'text', text: '第一步' }] })
    liveBridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw1', tool: 'todo_write', args: TODO_ARGS })
    liveBridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw1', exit_code: 0, error: null, result: 'ok' })
    liveBridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const liveIterator = liveSession.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const liveJournal = eventsOf((await liveIterator.next()).value as Record<string, unknown>)
    await liveIterator.return?.(undefined)
    live.dispose()

    const archive = [
      { kind: 'message', role: 'user', text: '第一步' },
      { kind: 'tool_call', call_id: 'tw1', tool: 'todo_write', arguments: JSON.stringify(TODO_ARGS), output: '[exit code: 0]' },
    ]
    const reopened = createAdapter({ bridge: scriptedBridge(undefined, archive) })
    const replayIterator = faceOfTodo(reopened.remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const replayJournal = eventsOf((await replayIterator.next()).value as Record<string, unknown>)
    await replayIterator.return?.(undefined)
    reopened.dispose()

    expect(replayJournal.map(event => event['type'])).toEqual(liveJournal.map(event => event['type']))
    const liveWrite = liveJournal.find(event => event['type'] === 'todo/write')
    const replayWrite = replayJournal.find(event => event['type'] === 'todo/write')
    expect(replayWrite!['data']).toEqual(liveWrite!['data'])
  })
})

describe('workspace 面：归档/置顶/手动排序（SH-100）', () => {
  interface WorkspaceTestFace {
    workspace: {
      follow(): AsyncGenerator<Record<string, unknown>>
      archiveSession(request: Record<string, unknown>): Promise<Record<string, unknown>>
      unarchiveSession(request: Record<string, unknown>): Promise<Record<string, unknown>>
      pinSession(request: Record<string, unknown>): Promise<Record<string, unknown>>
      unpinSession(request: Record<string, unknown>): Promise<Record<string, unknown>>
      insertSessionBefore(request: Record<string, unknown>): Promise<Record<string, unknown>>
    }
  }

  const faceOfWorkspace = (remote: unknown): WorkspaceTestFace => remote as WorkspaceTestFace

  it('baseline 带归档/置顶集合；归档推 archived 全量替换帧', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    const iterator = face.workspace.follow()
    const baseline = ((await iterator.next()).value as { value: Record<string, unknown> }).value
    expect(baseline['archivedSessionIds']).toEqual([])
    expect(baseline['pinnedSessionIds']).toEqual([])

    const result = await face.workspace.archiveSession({ sessionId: 'agent-new' })
    expect(result['ok']).toBe(true)
    expect((result['value'] as { archivedSessionIds: string[] }).archivedSessionIds).toEqual(['agent-new'])
    const frame = (await iterator.next()).value as { type: string; archivedSessionIds: string[] }
    expect(frame.type).toBe('archived')
    expect(frame.archivedSessionIds).toEqual(['agent-new'])
    await iterator.return?.(undefined)
    dispose()
  })

  it('置顶 prepend（最近置顶在前）推 pinned 帧；归档即掉置顶（同一次写）', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    await face.workspace.pinSession({ sessionId: 'a' })
    await face.workspace.pinSession({ sessionId: 'b' })
    // 已置顶的幂等不重排
    await face.workspace.pinSession({ sessionId: 'a' })
    const iterator = face.workspace.follow()
    const baseline = ((await iterator.next()).value as { value: Record<string, unknown> }).value
    // prepend：最近置顶在前（上游 pinnedSessionIds 的 unshift 语义）
    expect(baseline['pinnedSessionIds']).toEqual(['b', 'a'])

    // 已置顶的会话归档：archived 与 pinned 两帧都推（一次写里两个集合都变了）
    await face.workspace.archiveSession({ sessionId: 'a' })
    const frames: Record<string, unknown>[] = []
    for (let i = 0; i < 2; i += 1) frames.push((await iterator.next()).value as Record<string, unknown>)
    expect(frames.map(frame => frame['type']).sort()).toEqual(['archived', 'pinned'])
    const pinned = frames.find(frame => frame['type'] === 'pinned') as { pinnedSessionIds: string[] }
    expect(pinned.pinnedSessionIds).toEqual(['b'])
    await iterator.return?.(undefined)
    dispose()
  })

  it('错误按类映射 wire 码：运行中/不存在/置顶已归档；stopActivity 先取消再归档', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    const active = await face.workspace.archiveSession({ sessionId: 'agent-running' })
    expect(active['ok']).toBe(false)
    expect((active['error'] as { code: string }).code).toBe('workspace/session-active')
    expect((active['error'] as Error).message).toContain('agent-running')

    const ghost = await face.workspace.archiveSession({ sessionId: 'agent-ghost' })
    expect((ghost['error'] as { code: string }).code).toBe('session/not-found')

    // stopActivity=true：宿主先取消再归档，一次成功
    const stopped = await face.workspace.archiveSession({ sessionId: 'agent-running', stopActivity: true })
    expect(stopped['ok']).toBe(true)
    expect((stopped['value'] as { archivedSessionIds: string[] }).archivedSessionIds).toContain('agent-running')

    // 已归档的会话置顶：gateway/bad-request
    const pinArchived = await face.workspace.pinSession({ sessionId: 'agent-running' })
    expect(pinArchived['ok']).toBe(false)
    expect((pinArchived['error'] as { code: string }).code).toBe('gateway/bad-request')
    dispose()
  })

  it('insertSessionBefore：insertBefore 式移动推 upsert 帧；锚点缺失按 move-invalid', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    await face.workspace.insertSessionBefore({ sessionId: 'agent-new' })
    const forked = await face.workspace.insertSessionBefore({ sessionId: 'agent-fork', beforeSessionId: 'agent-new' })
    expect(forked['ok']).toBe(true)
    expect((forked['value'] as { sessionOrder: string[] }).sessionOrder).toEqual(['agent-fork', 'agent-new'])

    const iterator = face.workspace.follow()
    const baseline = ((await iterator.next()).value as { value: Record<string, unknown> }).value
    const view = (baseline['items'] as { sessionIds: string[] }[])[0]!
    // 视图顺序按侧车序排：现存才进账户，fork 在 new 前面
    expect(view['sessionIds']).toEqual(['agent-new'])
    await iterator.return?.(undefined)

    const badAnchor = await face.workspace.insertSessionBefore({ sessionId: 'agent-new', beforeSessionId: 'agent-ghost' })
    expect(badAnchor['ok']).toBe(false)
    expect((badAnchor['error'] as { code: string }).code).toBe('session/not-found')
    dispose()
  })

  it('取消归档/取消置顶：集合回到空，帧随之推送', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    await face.workspace.archiveSession({ sessionId: 'agent-new' })
    await face.workspace.pinSession({ sessionId: 'agent-fork' })
    await face.workspace.unarchiveSession({ sessionId: 'agent-new' })
    await face.workspace.unpinSession({ sessionId: 'agent-fork' })
    const iterator = face.workspace.follow()
    const baseline = ((await iterator.next()).value as { value: Record<string, unknown> }).value
    expect(baseline['archivedSessionIds']).toEqual([])
    expect(baseline['pinnedSessionIds']).toEqual([])
    await iterator.return?.(undefined)
    dispose()
  })
})

describe('子代理泳道（SH-109）：AgentEvent 信封的 agent_id 路由', () => {
  /** 泳道面的窄声明：follow + 控制流基线。 */
  interface ControlFace {
    session: SessionFace & {
      control(): AsyncGenerator<{ value: { projections: Record<string, { values: Record<string, unknown> }> } }>
    }
  }

  it('懒建泳道：非 root 事件路由进泳道 journal，父 journal 只落 catalog 引用事件', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session as ControlFace['session']
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '派子代理去探索' }] })
    // 后端 spawn 后子代理的事件带 agent_id（session_id 仍是宿主任务会话）
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '我去派子代理' })
    bridge.emit('agent-new', { type: 'turn_started', turn: 1 }, 'lane-a')
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '开始探索' }, 'lane-a')
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'l1', tool: 'read_file', args: { path: 'src/transport' } }, 'lane-a')
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'l1', exit_code: 0, error: null, result: 'ok' }, 'lane-a')

    // 泳道 journal：回合开 → 工具调用（delta 进的是助手流，不是 journal 事件）
    const lane = session.follow({ address: { kind: 'subagent', parentSessionId: 'agent-new', childSessionId: 'lane-a', mode: 'unknown' } })
    const laneSnapshot = (await lane.next()).value as Record<string, unknown>
    const laneEvents = eventsOf(laneSnapshot)
    // tool_call_begin 会先定稿此前的正文（assistant/message），再落调用
    expect(laneEvents.map(event => event['type'])).toEqual(['turn/start', 'step/start', 'assistant/message', 'tool/call', 'tool/result'])
    laneEvents.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())

    // 父 journal：只有 catalog 引用事件，没有泳道的工具调用——回合/步数/指标不被污染
    const parent = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const parentSnapshot = (await parent.next()).value as Record<string, unknown>
    const parentEvents = eventsOf(parentSnapshot)
    expect(parentEvents.map(event => event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message', 'subagent/catalog',
    ])
    expect((parentEvents.at(-1)?.['data'] as Record<string, unknown>)).toMatchObject({
      version: 1, childId: 'lane-a', mode: 'unknown',
    })
    parentEvents.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())

    // 控制基线：父挂 subagentCatalog（懒建泳道 = v1 unknown），泳道 timing 未见 descriptor 如实缺席
    const control = session.control()
    const baseline = (await control.next()).value
    expect(baseline.value.projections['agent-new']?.values['subagentCatalog']).toEqual([
      { id: 'lane-a', createdAt: expect.any(Number), mode: 'unknown' },
    ])
    expect(baseline.value.projections['lane-a']?.values['subagentTiming']).toBeUndefined()
    await control.return?.(undefined)

    await lane.return?.(undefined)
    await parent.return?.(undefined)
    dispose()
  })

  it('泳道完成态：turn/end 落定、运行状态收回，wire 规则全程过闸', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session as ControlFace['session']
    const statuses: [string, boolean][] = []
    ;(remote as unknown as {
      $on(event: string, listener: (...args: never[]) => unknown): () => void
    }).$on('api-session/status', (id: string, running: boolean) => {
      statuses.push([id, running])
      return Promise.resolve('')
    })

    bridge.emit('agent-new', { type: 'turn_started', turn: 1 }, 'lane-a')
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '结论：模块边界清晰' }, 'lane-a')
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' }, 'lane-a')

    // 运行状态的开与收都按泳道 id 广播（目录树徽标与 Stop 按钮的数据源）
    expect(statuses).toEqual([['lane-a', true], ['lane-a', false]])

    const lane = session.follow({ address: { kind: 'subagent', parentSessionId: 'agent-new', childSessionId: 'lane-a', mode: 'unknown' } })
    const snapshot = (await lane.next()).value as Record<string, unknown>
    const events = eventsOf(snapshot)
    expect(events.map(event => event['type'])).toEqual(['turn/start', 'step/start', 'assistant/message', 'step/end', 'turn/end'])
    expect((events.at(-1)?.['data'] as { reason: unknown }).reason).toEqual({ kind: 'completed' })
    events.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())
    await lane.return?.(undefined)
    dispose()
  })

  it('泳道的审批请求按泳道会话作用域派发（审批卡落在泳道 composer 上）', async () => {
    const bridge = scriptedBridge()
    const scopes = new Map<string, unknown>([['lane-a', { sessionId: 'lane-a' }]])
    const { remote, dispose } = createAdapter({ bridge, approvalScope: id => scopes.get(id) })
    const seen: { self: unknown; request: Record<string, unknown> }[] = []
    faceOf(remote).$on('approval/request', async function (this: unknown, request: Record<string, unknown>) {
      seen.push({ self: this, request })
      return 'allowed-once'
    })
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r-l1', tool: 'apply_patch', args: { path: 'a.txt' }, risk: 'write_in_workspace' }, 'lane-a')
    await vi.waitFor(() => { expect(seen).toHaveLength(1) })
    expect(seen[0]!.self).toEqual({ sessionId: 'lane-a' })
    expect(seen[0]!.request).toMatchObject({ agent: { id: 'lane-a' }, toolName: 'apply_patch' })
    expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r-l1', decision: 'allow' })
    dispose()
  })

  it('remote.subagents 桌面真实态按上游错误码显式拒绝，不伪造受理', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const subagents = (remote as unknown as {
      subagents: {
        prompt(request: Record<string, unknown>): Promise<never>
        interruptByUser(childSessionId: string): Promise<never>
      }
    }).subagents
    await expect(subagents.prompt({ childSessionId: 'lane-a', requestId: 'r1', delivery: 'queue', content: [] }))
      .rejects.toThrow('SH-053')
    await expect(subagents.interruptByUser('lane-a')).rejects.toThrow('SH-053')
    dispose()
  })
})
