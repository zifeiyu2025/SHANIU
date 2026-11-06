import { describe, expect, it } from 'vitest'
import { mockRemote } from './mock'

/** 只声明本文件用到的几个面，其余成员按 mock 的实际形状留着。 */
interface WireEvent {
  type: string
  seq: number
  data: Record<string, unknown>
}

interface JournalSnapshot {
  records: { type: string; event: WireEvent }[]
}

interface SessionFace {
  create(payload?: { workspaceId?: string; cwd?: string }): Promise<{ ok: true; value: { sessionId: string } }>
  prompt(request: { requestId?: string; sessionId?: string; content: readonly unknown[] }): Promise<unknown>
  selectModel(selection: Record<string, unknown>): Promise<unknown>
  follow(request: { address: { kind: 'session'; sessionId: string } }): AsyncGenerator<JournalSnapshot>
}

interface CredentialFace {
  set(ref: string, value: string): Promise<unknown>
  unset(ref: string): Promise<unknown>
}

const remote = mockRemote() as { session: SessionFace; credentials: CredentialFace }

/**
 * 回放一个会话的 journal，直到已提交 `turnEnds` 个回合为止；
 * 成功回合带流式分块，需要等，故按轮次计数而不是"看到一条就算"。
 */
async function eventsAfterTurns(sessionId: string, turnEnds: number): Promise<WireEvent[]> {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const stream = remote.session.follow({ address: { kind: 'session', sessionId } })
    const first = await stream.next()
    await stream.return?.(undefined)
    if (first.done !== true) {
      const events = first.value.records.map(record => record.event)
      if (events.filter(event => event.type === 'turn/end').length >= turnEnds) return events
    }
    await new Promise<void>(resolve => { setTimeout(resolve, 20) })
  }
  throw new Error('回合没有在预期时间内结束')
}

const send = async (sessionId: string, text: string): Promise<void> => {
  await remote.session.prompt({ sessionId, requestId: `rpc-${text}`, content: [{ type: 'text', text }] })
}

describe('mock 回合剧本与模型配置状态一致', () => {
  it('没配任何服务商时发送只报错，不产出助手回答', async () => {
    const created = await remote.session.create()
    const sessionId = created.value.sessionId
    await send(sessionId, '未配置时发一条')
    const events = await eventsAfterTurns(sessionId, 1)

    expect(events.filter(event => event.type === 'assistant/message')).toEqual([])
    expect(events.find(event => event.type === 'turn/end')?.data).toMatchObject({
      reason: { kind: 'error', error: { code: 'NO_MODEL_CONFIGURED' } },
    })
  })

  it('配好服务商后同一会话走成功回合，失败回合照样占一个回合号', async () => {
    const created = await remote.session.create()
    const sessionId = created.value.sessionId
    try {
      await send(sessionId, '先失败一轮')
      const firstTurn = await eventsAfterTurns(sessionId, 1)
      expect(firstTurn.filter(event => event.type === 'turn/end')[0]?.data).toMatchObject({
        turn: 0,
        reason: { kind: 'error' },
      })

      await remote.credentials.set('DEEPSEEK_API_KEY', 'mock-verify-key')
      await remote.session.selectModel({ provider: 'deepseek', model: 'deepseek-v4-flash' })
      await send(sessionId, '配好了再发')
      const events = await eventsAfterTurns(sessionId, 2)

      // 回合号必须接着往下走：写死 0 会让第二轮落在已关闭的回合上，过程头不出现
      expect(events.filter(event => event.type === 'turn/end')[1]?.data).toMatchObject({
        turn: 1,
        reason: { kind: 'completed' },
      })
      const usage = events.filter(event => event.type === 'assistant/message')[0]?.data.usage
      expect(usage).toMatchObject({ inputTokens: 9600, outputTokens: 300, totalTokens: 9900 })
    } finally {
      await remote.credentials.unset('DEEPSEEK_API_KEY')
    }
  })
})
