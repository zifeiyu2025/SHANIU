/**
 * MCP 信任页的纯逻辑 + 段落注册契约。
 *
 * 渲染快照不测：这一页的语义都在「默认不批准 / 默认 destructive / 显式启用要真下传」
 * 与「注册 id 被 settingsShell.openSection 引用」两点上，直接测这两点。
 */
import { describe, expect, it } from 'vitest'
import { apply } from './index'
import { McpTrustSection } from './McpTrustSection'
import {
  approvedCount,
  buildDecisions,
  defaultSelections,
  needsInjectionConsent,
  SELECTABLE_RISKS,
  shortFingerprint,
} from './model'
import type { ReviewPayloadWire, ReviewToolWire } from '../transport/mcp-wire'

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

describe('段落注册', () => {
  it('以 settings.section id=mcp 注册，供设置页与 settingsShell.openSection 使用', () => {
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
    expect(registered).toHaveLength(1)
    expect(registered[0].options).toMatchObject({ name: 'settings.section', id: 'mcp', locale: 'shaniuMcp' })
    expect(registered[0].component).toBe(McpTrustSection)
  })
})
