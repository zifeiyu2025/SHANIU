/**
 * MCP 连接器的纯逻辑 + 条目注册契约。
 *
 * 渲染快照不测：这一页的语义都在「默认不批准 / 默认 destructive / 显式启用要真下传」、
 * 「卡片摘要的状态聚合」与「一份 UI 两个挂载点（同一组件）」这几点上，直接测这几点。
 */
import { describe, expect, it } from 'vitest'
import { apply } from './index'
import { GeneralConfigReadOnlyRow } from '../config-readonly/ConfigReadOnly'
import { McpConnector } from './McpConnector'
import {
  approvedCount,
  buildDecisions,
  defaultSelections,
  needsInjectionConsent,
  SELECTABLE_RISKS,
  shortFingerprint,
  summarizeServers,
} from './model'
import type { McpServerSummaryWire, ReviewPayloadWire, ReviewToolWire } from '../transport/mcp-wire'

function tool(overrides: Partial<ReviewToolWire>): ReviewToolWire {
  return {
    dispatch_name: 'mcp__srv__echo',
    raw_name: 'echo',
    description: 'Echo',
    schema: { type: 'object' },
    server_read_only_hint: false,
    risk: 'destructive',
    approved: false,
    fingerprint: 'f'.repeat(64),
    guard_hits: [],
    guard_exempted: false,
    ...overrides,
  }
}

function review(tools: readonly ReviewToolWire[]): ReviewPayloadWire {
  return {
    server_id: 'srv',
    status: 'review',
    fingerprint: 'f'.repeat(64),
    transport: 'stdio',
    executable: 'C:/node.exe',
    executable_sha256: 'a'.repeat(64),
    approved_at_ms: 0,
    tools_stale: false,
    argv: ['server.js'],
    cwd: 'C:/work',
    env: ['TOKEN = sha256:12345678…'],
    protocol_version: '2025-06-18',
    server_name: 'fake',
    server_version: '1.0.0',
    allow_network: false,
    unsandboxed: false,
    tools,
    rejected_tools: [],
    approved_tools: [],
    exemptions: [],
    warnings: [],
  }
}

/** 摘要视图的测试替身：把键与计数原样拼出来，便于断言聚合顺序。 */
function translate(key: string, params: { count: number }): string {
  return `${key}:${params.count}`
}

function server(status: string, overrides: Partial<McpServerSummaryWire> = {}): McpServerSummaryWire {
  return {
    id: 'srv',
    enabled: true,
    command: 'C:/node.exe',
    args: ['server.js'],
    unsandboxed: false,
    allow_network: false,
    status,
    fingerprint: 'f'.repeat(64),
    approved_tools: [],
    exemption_count: 0,
    reason: null,
    ...overrides,
  }
}

describe('默认勾选', () => {
  it('没有信任记录时一律不勾、档位 destructive', () => {
    const selections = defaultSelections(review([tool({}), tool({ dispatch_name: 'mcp__srv__b' })]))
    expect(selections['mcp__srv__echo']).toEqual({ approved: false, risk: 'destructive', enableInjection: false })
    expect(approvedCount(selections)).toBe(0)
  })

  it('已批准的工具沿用记录里的档位；plan_review 回落到 destructive', () => {
    const selections = defaultSelections(review([
      tool({ approved: true, risk: 'read' }),
      tool({ dispatch_name: 'mcp__srv__plan', approved: true, risk: 'plan_review' }),
    ]))
    expect(selections['mcp__srv__echo'].risk).toBe('read')
    expect(selections['mcp__srv__plan'].risk).toBe('destructive')
  })

  it('用户可选档位不含 plan_review', () => {
    expect(SELECTABLE_RISKS).not.toContain('plan_review')
  })
})

describe('裁决载荷', () => {
  it('未勾选的工具即使填了档位也不批准，且不下传 enable_injection', () => {
    const payload = review([tool({})])
    const decisions = buildDecisions(payload, {
      'mcp__srv__echo': { approved: false, risk: 'destructive', enableInjection: true },
    })
    expect(decisions).toEqual([{
      dispatch_name: 'mcp__srv__echo',
      approved: false,
      risk: 'destructive',
      enable_injection: false,
    }])
  })

  it('显式启用只在已批准时下传', () => {
    const decisions = buildDecisions(review([tool({})]), {
      'mcp__srv__echo': { approved: true, risk: 'execute', enableInjection: true },
    })
    expect(decisions[0].approved).toBe(true)
    expect(decisions[0].risk).toBe('execute')
    expect(decisions[0].enable_injection).toBe(true)
  })

  it('README 口径：每个工具都有裁决项，避免漏掉未表态的工具', () => {
    const payload = review([tool({}), tool({ dispatch_name: 'mcp__srv__b' })])
    expect(buildDecisions(payload, {})).toHaveLength(2)
  })
})

describe('展示辅助', () => {
  it('注入命中且未启用时才需要用户二次确认', () => {
    const hit = { rule: 'instruction_override', fragment: 'ignore previous instructions' }
    expect(needsInjectionConsent(tool({ guard_hits: [hit] }))).toBe(true)
    expect(needsInjectionConsent(tool({ guard_hits: [hit], guard_exempted: true }))).toBe(false)
    expect(needsInjectionConsent(tool({}))).toBe(false)
  })

  it('指纹缩写保留可辨识前缀', () => {
    expect(shortFingerprint('a'.repeat(64))).toBe(`${'a'.repeat(16)}…`)
    expect(shortFingerprint('short')).toBe('short')
  })
})

describe('卡片摘要', () => {
  it('没有服务器时给出空态，不拼出「0 台」', () => {
    expect(summarizeServers([], translate)).toBe('summary.none:0')
  })

  it('按「需要人处理 → 稳定」的顺序聚合，且同状态合并计数', () => {
    const summary = summarizeServers([
      server('trusted', { id: 'a' }),
      server('review', { id: 'b' }),
      server('retrust', { id: 'c' }),
      server('review', { id: 'd' }),
    ], translate)
    expect(summary).toBe('summary.total:4 · summary.review:2 · summary.retrust:1 · summary.trusted:1')
  })

  it('0 台的状态不出现在摘要里', () => {
    const summary = summarizeServers([server('trusted')], translate)
    expect(summary).not.toContain('summary.review')
    expect(summary).not.toContain('summary.retrust')
    expect(summary).toContain('summary.trusted:1')
  })
})

describe('条目注册', () => {
  it('一份 UI 两个挂载点：plugins.item + 临时 settings.section，挂同一组件', () => {
    const registered: { options: Record<string, unknown>; component: unknown }[] = []
    const ctx = {
      effect: (fn: () => void) => { fn() },
      locale: { register: () => () => {}, bind: () => (key: string) => key },
      slots: {
        inject: (_name: string, fn: () => void) => fn(),
        register: (options: Record<string, unknown>, component: unknown) => {
          registered.push({ options, component })
          return () => {}
        },
      },
    }
    apply(ctx as never)
    // 两个挂载点挂同一组件定义 = 同一份数据源与状态，不是复制一份列表 UI。
    const connector = registered.filter((entry) => entry.component === McpConnector)
    expect(connector.map((entry) => entry.options.name)).toEqual(['plugins.item', 'settings.section'])
    for (const entry of connector) {
      expect(entry.options).toMatchObject({ id: 'mcp', locale: 'shaniuMcp' })
      expect((entry.options.label as () => string)()).toBe('label')
    }
    // 配置只读是全局状态：通用设置页那一行是共享横幅，不是第三个「连接器挂载点」。
    const banner = registered.filter((entry) => entry.component === GeneralConfigReadOnlyRow)
    expect(banner.map((entry) => entry.options.name)).toEqual(['settings.general.item'])
    expect(banner[0]?.options).toMatchObject({ id: 'config-readonly', locale: 'shaniuMcp' })
  })
})
