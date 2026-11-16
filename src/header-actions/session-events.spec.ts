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

  it('被裁剪的工具输出带「已裁剪」判读行，原文仍完整（N7）', () => {
    const output = '头'.repeat(20) + '裁剪前原文尾部'
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '跑个命令' },
      {
        kind: 'tool_call',
        call_id: 'call-9',
        tool: 'unified_exec',
        arguments: '{}',
        output,
        pruned: { original_chars: 18_955, head_chars: 4_096, tail_chars: 1_024, omitted_chars: 13_835 },
      },
    ], '标题')

    const text = (ofType(events, 'tool/result')[0]?.data as {
      message: { content: { text: string }[] }
    }).message.content[0]?.text ?? ''
    // 反向对照：去掉 session-events 里的 decorateToolOutput，下面三条立刻变红——
    // 导出只剩完整原文，扫「已裁剪」为 0，读者会以为模型看过全文。
    expect(text).toContain('已裁剪')
    expect(text).toContain('原始 18955 字符')
    expect(text).toContain('中段省略 13835 字符')
    // 选定的口径：导出保留档案原文，但必须明说这是**裁剪前原文**，不冒充完整上下文
    expect(text).toContain('裁剪前原文')
    expect(text.endsWith(output)).toBe(true)
    // 端到端：Markdown 导出同样能看到判读行
    expect(formatSessionMarkdown(events)).toContain('已裁剪')
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
      { kind: 'turn_end', turn: 1, reason: 'completed', at_ms: 1_700_000_001_500 },
      {
        kind: 'compaction_summary',
        text: '## 摘要正文',
        range_start: 0,
        range_end: 2,
        at_ms: anchorAt,
        trigger: 'manual',
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
      .toEqual(['compactionId', 'summary', 'trigger'])
    // 锚点在回合外（手动 `/compact`）：上一回合已 turn/end、下一回合还没 turn/start。
    // 不把它归给"即将开始的下一轮"——只陈述档案里已成立的归属（R5-4）。
    expect((start[0]!['data'] as { turn: number | null }).turn).toBeNull()
    expect((end[0]!['data'] as { turn: number | null }).turn).toBeNull()
    expect('error' in (end[0]!['data'] as object)).toBe(false)
    // 三条都带 trigger：导出必须能自己分辨手动/自动，不必再拿事件流反推。
    for (const event of [start[0]!, summary[0]!, end[0]!]) {
      expect((event['data'] as { trigger?: string }).trigger).toBe('manual')
    }

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

  it('回合内压缩（溢出补救/预算）如实带当轮号：归属来自档案已成立的事实', () => {
    // 档案顺序就是真实发生顺序：用户提问 → 溢出补救（锚点）→ 回合结局。
    // 锚点落在 turn/start 与 turn/end 之间，导出必须写 `turn: 1` 而不是 null。
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '第一问', at_ms: 1_700_000_000_000 },
      { kind: 'message', role: 'assistant', text: '第一答', at_ms: 1_700_000_001_000 },
      {
        kind: 'compaction_summary',
        text: '溢出后压缩',
        range_start: 0,
        range_end: 2,
        at_ms: 1_700_000_002_000,
        trigger: 'context_overflow',
      },
      { kind: 'turn_end', turn: 1, reason: 'completed', at_ms: 1_700_000_003_000 },
    ], '标题')

    const start = ofType(events, 'compaction/start')
    const end = ofType(events, 'compaction/end')
    expect([start.length, end.length]).toEqual([1, 1])
    expect((start[0]!['data'] as { turn: number | null }).turn).toBe(1)
    expect((end[0]!['data'] as { turn: number | null }).turn).toBe(1)
    expect((start[0]!['data'] as { trigger?: string }).trigger).toBe('context_overflow')
    // 时刻取档案的落档时刻，不是导出那一刻
    expect(start[0]!['time']).toBe(1_700_000_002_000)
  })

  it('旧档案（无 at_ms / trigger）如实标不可信，不编造触发原因', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '旧问' },
      { kind: 'compaction_summary', text: '旧摘要', range_start: 0, range_end: 1 },
    ], '标题')

    for (const event of [...ofType(events, 'compaction/start'), ...ofType(events, 'compaction/end')]) {
      expect(event['time']).toBe(0)
      expect(event['timeUnreliable']).toBe(true)
      // 未知触发 = 不落键（不是 `trigger: null`，也不是猜一个值）
      expect('trigger' in (event['data'] as object)).toBe(false)
    }
    const summary = ofType(events, 'compaction/summary')
    expect(Object.keys(summary[0]!['data'] as object).sort()).toEqual(['compactionId', 'summary'])
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

    // 新档的 `iterations` 不再只是审计信息：它进 `turn/end`，并作为步数核对基准（N4）。
    const fresh = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '唯一问题' },
      { kind: 'message', role: 'assistant', text: '回答' },
      { kind: 'turn_end', turn: 1, task_id: 'agent#1', iterations: 5, reason: 'completed', at_ms: 20 },
    ], '标题')
    const freshEnd = ofType(fresh, 'turn/end')[0] as { data: Record<string, unknown> }
    expect(freshEnd.data['turn']).toBe(1)
    expect(freshEnd.data['reason']).toBe('completed')
    expect(freshEnd.data['iterations']).toBe(5)
    // 条目没有档案步号 → 不做步数核对，如实标记而不是拿消息条数摊。
    expect(freshEnd.data['stepsUnavailable']).toBe(true)
    // 旧档没有 iterations：字段缺席 + 同样标记不可核对。
    expect(ends[0].data['iterations']).toBeUndefined()
    expect(ends[0].data['stepsUnavailable']).toBe(true)
  })

  it('N4：step 用档案真实步号，turn/end 带 iterations；缺步号只留标记', () => {
    const events = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '改 sum' },
      { kind: 'message', role: 'assistant', text: '', step: 0 },
      { kind: 'tool_call', call_id: 'c1', tool: 'read_file', arguments: '{}', output: 'a', step: 0 },
      { kind: 'tool_call', call_id: 'c2', tool: 'unified_exec', arguments: '{}', output: 'done', step: 1 },
      { kind: 'message', role: 'assistant', text: '改好了', step: 1 },
      { kind: 'turn_end', turn: 1, task_id: 'agent#1', iterations: 2, reason: 'completed', at_ms: 20 },
    ], '标题')

    const body = events.filter(event =>
      event.type === 'assistant/message' || event.type === 'tool/call' || event.type === 'tool/result')
    // 反向对照：退回 `step: 1` 常量后，这里的步号会全变成 1，与 iterations=2 对不上。
    expect(body.map(event => (event.data as { step?: number }).step)).toEqual([0, 0, 0, 1, 1, 1])
    const end = ofType(events, 'turn/end')[0] as { data: Record<string, unknown> }
    expect(end.data['iterations']).toBe(2)
    expect(end.data['stepsUnavailable']).toBeUndefined()

    // 缺档案步号：事件里连 `step` 键都不写，回合结局标不可核对（不编、不凑）。
    const unverified = sessionEntriesToEvents([
      { kind: 'message', role: 'user', text: '跑一下' },
      { kind: 'message', role: 'assistant', text: '好' },
      { kind: 'turn_end', turn: 1, task_id: 'agent#1', iterations: 1, reason: 'completed', at_ms: 20 },
    ], '标题')
    const assistant = ofType(unverified, 'assistant/message')[0] as { data: Record<string, unknown> }
    expect('step' in assistant.data).toBe(false)
    const unverifiedEnd = ofType(unverified, 'turn/end')[0] as { data: Record<string, unknown> }
    expect(unverifiedEnd.data['iterations']).toBe(1)
    expect(unverifiedEnd.data['stepsUnavailable']).toBe(true)
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
