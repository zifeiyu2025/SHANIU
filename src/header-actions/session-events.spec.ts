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

  it('压缩锚点补三条 log-only 事务事件，摘要不伪装成用户发言', () => {
    const anchorAt = 1_700_000_500_000
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '第一问', at_ms: 1_700_000_000_000 },
      { kind: 'message', role: 'assistant', text: '第一答', at_ms: 1_700_000_001_000 },
      {
        kind: 'compaction_summary',
        text: '## 摘要正文',
        range_start: 0,
        range_end: 2,
        at_ms: anchorAt,
      },
      { kind: 'message', role: 'user', text: '第二问', at_ms: 1_700_000_600_000 },
      { kind: 'message', role: 'assistant', text: '第二答', at_ms: 1_700_000_601_000 },
    ], '标题')

    const start = ofType(events, 'compaction/start')
    const summary = ofType(events, 'compaction/summary')
    const end = ofType(events, 'compaction/end')
    // 反向对照：删掉该分支里的三条 push（A2 修复前的行为），这三条断言必红——
    // 导出档案里 compaction 恒为 0，宿主 events 里的压缩记录在导出里凭空消失。
    expect([start.length, summary.length, end.length]).toEqual([1, 1, 1])
    // 顺序固定：start → summary → end（上游按此配对，乱序即孤儿事务）
    expect(events.map(event => (event as { type: string }).type)
      .filter(type => type.startsWith('compaction/'))).toEqual([
      'compaction/start', 'compaction/summary', 'compaction/end',
    ])

    const idOf = (event: Record<string, unknown>): unknown => (event['data'] as { compactionId: string }).compactionId
    expect(idOf(start[0]!)).toBe('shaniu-export-compaction-1')
    expect(idOf(summary[0]!)).toBe(idOf(start[0]!))
    expect(idOf(end[0]!)).toBe(idOf(start[0]!))
    // 正文取锚点 text；`shadowed*`/`provider`/`model` 没有可信来源，一条也不编造
    expect((summary[0]!['data'] as { summary: unknown }).summary)
      .toEqual([{ type: 'text', text: '## 摘要正文' }])
    expect(Object.keys(summary[0]!['data'] as object).sort())
      .toEqual(['compactionId', 'summary'])
    // 锚点在回合外：上一回合已 turn/end、下一回合还没 turn/start
    expect((start[0]!['data'] as { turn: number | null }).turn).toBeNull()
    expect((end[0]!['data'] as { turn: number | null }).turn).toBeNull()
    expect('error' in (end[0]!['data'] as object)).toBe(false)

    for (const event of [start[0]!, summary[0]!, end[0]!]) {
      // 与相邻事件同一口径：取锚点的 at_ms
      expect(event['time']).toBe(anchorAt)
      // log-only：带 surfaceOp 会改变 surface 并触发上游 system 头保护
      expect('surfaceOp' in event).toBe(false)
    }

    // N3 不变：摘要不当用户发言，用户消息仍只有档案里那两条，且正文不含摘要
    const users = ofType(events, 'user/message')
    expect(users).toHaveLength(2)
    expect(users.map(event => (event['data'] as { content: { text?: string }[] }).content[0]?.text))
      .toEqual(['第一问', '第二问'])
    const serialized = JSON.stringify(events)
    expect(serialized.split('## 摘要正文')).toHaveLength(2) // 只出现一次（summary 事件内）

    // seq 仍从 1 连续递增（新增三条事件不破坏本地单调序号）
    const seqs = events.map(event => (event as { seq: number }).seq)
    expect(seqs).toEqual(seqs.map((_, index) => index + 1))
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

  it('结局行的 turn 是旧口径也不影响导出轮次：按档案位置收口', () => {
    // 旧档的 turn 是本次 run 内的采样迭代号（这里是 3），导出侧的 turn 按用户消息自增（1）。
    // 反向对照：恢复 `entry.turn === turn` 后，这条结局被丢弃，导出回落到 unknown。
    const legacy = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '唯一问题' },
      { kind: 'message', role: 'assistant', text: '回答' },
      { kind: 'turn_end', turn: 3, reason: 'completed', at_ms: 20 },
    ], '标题')
    const ends = ofType(legacy, 'turn/end') as { data: Record<string, unknown> }[]
    expect(ends).toHaveLength(1)
    expect(ends[0].data['turn']).toBe(1)
    expect(ends[0].data['reason']).toBe('completed')

    // 新档同结论：turn 是用户回合号，task_id/iterations 只作审计，不改变导出结果。
    const fresh = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '唯一问题' },
      { kind: 'message', role: 'assistant', text: '回答' },
      { kind: 'turn_end', turn: 1, task_id: 'agent#1', iterations: 5, reason: 'completed', at_ms: 20 },
    ], '标题')
    expect(ofType(fresh, 'turn/end').map(event => event.data)).toEqual(ends.map(event => event.data))
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
