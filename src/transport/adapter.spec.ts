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

interface ScriptedBridge extends HostBridge {
  readonly calls: { command: string; args: Record<string, unknown> | undefined }[]
  emit(sessionId: string, msg: EventMsg): void
}

function scriptedBridge(
  settings = { baseUrl: 'https://x/v1', model: 'm', workspaceRoot: 'W', hasApiKey: true },
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

  it('按游标分支在游标对齐持久化前明确拒绝，不猜档案条数', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-11', content: [{ type: 'text', text: '分支' }] })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const refused = await session.fork({ sessionId: 'agent-new', throughSeq: 1 })
    expect(refused['ok']).toBe(false)
    expect(bridge.calls.some(call => call.command === 'fork_session')).toBe(false)

    const tail = await session.fork({ sessionId: 'agent-new' })
    expect(tail).toEqual({ ok: true, value: { sessionId: 'agent-fork' } })
    const forkCall = bridge.calls.find(call => call.command === 'fork_session')
    expect(forkCall?.args).toEqual({ agentId: 'agent-new', keepItems: Number.MAX_SAFE_INTEGER })
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

  it('模型席位只反映宿主真实配置，选模写回设置并推投影', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const catalogA = (await session.modelCatalog())['value'] as { groups: unknown[]; routableProviders: string[] }
    expect(catalogA.groups).toHaveLength(1)
    expect(catalogA.routableProviders).toEqual(['openai-compatible'])

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-12', content: [{ type: 'text', text: '换模型' }] })
    await session.selectModel({ model: 'other-model' })
    expect(bridge.calls.find(call => call.command === 'save_settings')?.args).toEqual({
      baseUrl: 'https://x/v1', model: 'other-model', workspaceRoot: 'W',
    })
    dispose()
  })

  it('没配 Key 就是空目录，席位不编默认档', async () => {
    const bridge = scriptedBridge({ baseUrl: '', model: '', workspaceRoot: 'W', hasApiKey: false })
    const { remote, dispose } = createAdapter({ bridge })
    const catalog = (await faceOf(remote).session.modelCatalog())['value'] as { groups: unknown[]; default: Record<string, string> }
    expect(catalog.groups).toEqual([])
    expect(catalog.default).toEqual({ provider: '', model: '' })
    dispose()
  })
})
