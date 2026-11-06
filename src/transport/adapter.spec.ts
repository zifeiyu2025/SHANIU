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
  emit(sessionId: string, msg: EventMsg): void
}

function scriptedBridge(
  settings: Record<string, unknown> = HOST_SETTINGS,
  archive: Record<string, unknown>[] = [],
): ScriptedBridge {
  const calls: ScriptedBridge['calls'] = []
  const handlers = new Set<(event: AgentEvent) => void>()
  let counter = 0
  return {
    calls,
    async invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
      calls.push({ command, args })
      if (command === 'get_settings') return settings as T
      if (command === 'create_session') return 'agent-new' as T
      if (command === 'send_task') return (args?.agentId ?? 'agent-new') as T
      if (command === 'load_session') return archive as T
      if (command === 'list_sessions') {
        return [{ agentId: 'agent-new', updatedAt: 1, title: '首条任务', workspaceRoot: 'W' }] as T
      }
      if (command === 'fork_session') return 'agent-fork' as T
      return undefined as T
    },
    listenEvents(handler) {
      handlers.add(handler)
      return () => handlers.delete(handler)
    },
    emit(sessionId, msg) {
      counter += 1
      const event: AgentEvent = {
        event_id: `01J00000000000000000000${String(counter).padStart(2, '0')}`,
        session_id: sessionId,
        agent_id: 'root',
        parent_id: null,
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
  rename(): Promise<Record<string, unknown>>
}

interface RemoteFace {
  readonly session: SessionFace
  readonly workspaceFiles: Record<string, () => Promise<unknown>>
  $on(event: string, listener: () => Promise<string>): () => void
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
    expect(assistant['usage']).toEqual({ inputTokens: 10, outputTokens: 4, cacheReadTokens: 2, cacheWriteTokens: 0, totalTokens: 16 })
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

  it('未接线的命名空间显式抛错并指向核销清单', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const workspaceFiles = faceOf(remote).workspaceFiles
    await expect(workspaceFiles['read']!()).rejects.toThrow(/未接线/)
    await expect(workspaceFiles['read']!()).rejects.toThrow(/SH-040/)
    await expect(faceOf(remote).session.rename()).rejects.toThrow(/SH-033/)
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
