import { describe, expect, it } from 'vitest'
import { formatSessionMarkdown } from '../dsh/packages/session-query/session-log-export/src/markdown'
import { sessionEntriesToEvents } from './session-events'

/** 取出某类事件，便于按类型断言（不关心其余字段）。 */
function ofType(events: ReturnType<typeof sessionEntriesToEvents>, type: string): Record<string, unknown>[] {
  return events.filter(event => (event as { type: string }).type === type) as never
}

describe('档案条目 → 逻辑事件', () => {
  it('每个用户消息开一个新回合，编号递增，末尾收口', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '第一条' },
      { kind: 'message', role: 'assistant', text: '回复一' },
      { kind: 'message', role: 'user', text: '第二条' },
      { kind: 'message', role: 'assistant', text: '回复二' },
    ], '标题')

    const starts = ofType(events, 'turn/start')
    expect(starts.map(event => (event.data as { turn: number }).turn)).toEqual([1, 2])
    const ends = ofType(events, 'turn/end')
    expect(ends.map(event => (event.data as { turn: number }).turn)).toEqual([1, 2])
    // 末尾一定有收口：formatter 靠 turn/end 结束当前回合
    expect((events.at(-1) as { type: string }).type).toBe('turn/end')
  })

  it('用户消息的图片按附件块导出，不压成正文文本', () => {
    const events = sessionEntriesToEvents([
      {
        kind: 'message',
        role: 'user',
        text: '看看这张图',
        images: [
          {
            path: '/data/images/a.png', sha256: 'abc', width: 800, height: 600,
            tokens: 120, mime: 'image/png', attachment_id: 'att-1', bytes: 2048,
          },
          // 没有 attachment_id 的引用（`view_image` 回填）不可回读，不能当图片块
          { path: '/tmp/b.png', sha256: 'def', width: 10, height: 10, tokens: 1, mime: 'image/png' },
        ],
      },
      { kind: 'message', role: 'assistant', text: '收到' },
    ], '标题')

    const content = (ofType(events, 'user/message')[0]?.data as { content: Record<string, unknown>[] }).content
    expect(content[0]).toEqual({ type: 'text', text: '看看这张图' })
    expect(content[1]).toEqual({
      type: 'image',
      attachment: { attachmentId: 'att-1', mediaType: 'image/png', bytes: 2048, width: 800, height: 600 },
    })
    expect(content).toHaveLength(2)

    // 端到端：上游 formatter 把 image 块渲染成可见的附件行，而不是丢掉
    const markdown = formatSessionMarkdown(events)
    expect(markdown).toContain('[Image: attachment (800×600, 2048 bytes)]')
  })

  it('工具调用与结果成对展开，callId 保持一致', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '跑个命令' },
      { kind: 'tool_call', call_id: 'call-1', tool: 'unified_exec', arguments: '{"cmd":"ls"}', output: 'a.ts' },
    ], '标题')

    const call = ofType(events, 'tool/call')[0]?.data as Record<string, unknown>
    const result = ofType(events, 'tool/result')[0]?.data as Record<string, unknown>
    expect(call.callId).toBe('call-1')
    expect(call.name).toBe('unified_exec')
    expect(call.arguments).toBe('{"cmd":"ls"}')
    // formatter 用 `data.message.toolCallId` 回填，两处必须同源，否则结果会变成 "unknown" 卡片
    expect((result.message as { toolCallId: string }).toolCallId).toBe('call-1')
  })

  it('没有输出的工具调用只发 call，不伪造空 result', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '取消掉' },
      { kind: 'tool_call', call_id: 'call-2', tool: 'unified_exec', arguments: '{}' },
    ], '标题')
    expect(ofType(events, 'tool/call')).toHaveLength(1)
    expect(ofType(events, 'tool/result')).toHaveLength(0)
  })

  it('reasoning 不进事件（上游 formatter 明确省略思考内容）', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '问' },
      { kind: 'message', role: 'assistant', text: '答', reasoning: '这段思考不该出现' },
    ], '标题')
    const serialized = JSON.stringify(events)
    expect(serialized).not.toContain('这段思考不该出现')
  })

  it('system 消息被跳过，用户与助手之外的 role 不产生消息事件', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'system', text: '系统提示' },
      { kind: 'message', role: 'user', text: '问' },
      { kind: 'message', role: 'assistant', text: '答' },
    ], '标题')
    expect(ofType(events, 'user/message')).toHaveLength(1)
    expect(ofType(events, 'assistant/message')).toHaveLength(1)
    expect(JSON.stringify(events)).not.toContain('系统提示')
  })

  it('seq 单调递增，标题事件在首位', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '问' },
      { kind: 'message', role: 'assistant', text: '答' },
    ], '标题')
    expect((events[0] as { type: string }).type).toBe('session/title')
    const seqs = events.map(event => (event as { seq: number }).seq)
    expect(seqs).toEqual(seqs.map((_, index) => index + 1))
  })

  it('空档案只产标题事件，不产空回合', () => {
    const events = sessionEntriesToEvents([], '空')
    expect(events).toHaveLength(1)
    expect((events[0] as { type: string }).type).toBe('session/title')
  })

  it('档案结局行如实进 turn/end：失败不写成成功，原始错误一并带出', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '问' },
      { kind: 'message', role: 'assistant', text: '半截' },
      {
        kind: 'turn_end',
        turn: 1,
        reason: 'failed',
        at_ms: 1_700_000_000_123,
        error: {
          code: 'AUTH',
          message: '密钥无效',
          http_status: 401,
          provider_code: 'invalid_api_key',
          provider_message: 'Invalid API key',
        },
      },
    ], '标题')

    const end = ofType(events, 'turn/end')[0] as { time: number; data: Record<string, unknown> }
    // 反向对照：把 reason 写死成 completed（修复前的行为），这两条必红。
    expect(end.data['reason']).toBe('failed')
    expect(end.data['reason']).not.toBe('completed')
    expect((end.data['error'] as { code: string }).code).toBe('AUTH')
    // 导出时间取事件发生时刻，不是导出那一刻。
    expect(end.time).toBe(1_700_000_000_123)
  })

  it('旧档案没有结局行 → unknown，不冒充 completed', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '问' },
      { kind: 'message', role: 'assistant', text: '答' },
    ], '标题')

    const end = ofType(events, 'turn/end')[0] as { data: Record<string, unknown> }
    // 反向对照：缺省 completed 会把所有旧档案的失败回合洗成成功。
    expect(end.data['reason']).toBe('unknown')
  })

  it('事件时间取档案 at_ms，不再让整份导出只有一两个时间', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '问', at_ms: 1_700_000_000_000 },
      { kind: 'message', role: 'assistant', text: '答', at_ms: 1_700_000_012_000 },
      { kind: 'tool_call', call_id: 'c1', tool: 'unified_exec', arguments: '{}', output: 'ok', at_ms: 1_700_000_013_000 },
    ], '标题')

    const times = events.map(event => (event as { time: number }).time)
    // 反向对照：改回 `time: Date.now()`（导出那一刻），不同事件会塌成同一毫秒或仅剩标题/收口两个值。
    expect(times).toEqual(expect.arrayContaining([
      1_700_000_000_000,
      1_700_000_012_000,
      1_700_000_013_000,
    ]))
    expect(ofType(events, 'turn/start')[0]?.time).toBe(1_700_000_000_000)
  })

  it('旧档案没有 at_ms → time 为 0 且标 timeUnreliable，不假装时间可信', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '问' },
      { kind: 'message', role: 'assistant', text: '答' },
    ], '标题')

    for (const event of events as Array<{ time: number; timeUnreliable?: boolean }>) {
      expect(event.time).toBe(0)
      expect(event.timeUnreliable).toBe(true)
    }
  })
})
