// @vitest-environment jsdom
/**
 * SH-132 能力视图（宿主插件）契约测试。
 *
 * 覆盖本卡承诺的五件事：
 *  1. 走的是上游 `settings.models.provider-card` 扩展点，键控 `llm-pi-ai`（不改 vendored 组件）；
 *  2. 值 / 来源 / 三态 / 内置表数据日期同屏可见，`mismatch` 有独立 data-state；
 *  3. **只标改动的那一项**：其余项的来源不被顺手改成手动；
 *  4. 设置写入广播后重新读取（否则徽标停在保存前）；
 *  5. 读取失败如实显示，不粉饰成"没有能力"。
 */
import { describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'
import { act, render, screen, waitFor } from '@testing-library/react'
import { Context } from '@deepseek-ai/cordis'
import { apply as capabilitiesApply, inject as capabilitiesInject } from './index'
import { CapabilityPanel } from './CapabilityPanel'
import { zh } from './locales'
import type { CapabilitiesWire, CapabilityProviderWire } from './wire'

/** 词典直译（测试只关心键是否被用到）。 */
function translate(key: string, params?: Record<string, unknown>): string {
  const template = (zh as Record<string, string>)[key] ?? key
  return template.replace(/\{(\w+)\}/g, (_match, name: string) => String(params?.[name] ?? `{${name}}`))
}

/** 内置表数据日期（UTC 2026-11-09）。 */
const DATA_DATE = Date.UTC(2026, 10, 9)

const FLASH: CapabilityProviderWire = {
  id: 'deepseek',
  label: 'DeepSeek',
  models: [{
    id: 'deepseek-flash',
    name: 'DeepSeek Flash',
    contextWindow: 393_216,
    maxTokens: 32_768,
    input: ['text', 'image'],
    capabilities: {
      vision: { value: true, source: 'builtin', state: 'verified', evidence: '红图→红色；绿图→深绿色', catalogUpdatedAt: DATA_DATE },
      tools: { value: true, source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
      reasoning: { levels: ['low', 'high', 'max'], verifiedLevels: ['low', 'high', 'max'], source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
      context_window: { value: 393_216, source: 'builtin', state: 'unverified', catalogUpdatedAt: DATA_DATE },
      max_output: { value: 32_768, source: 'builtin', state: 'unverified', catalogUpdatedAt: DATA_DATE },
      builtin_web_search: { value: false, source: 'unknown', state: 'unverified' },
    },
  }],
}

/** 纯文本模型声明视觉：只有视觉一项是手动 + 不符，其余仍是内置表。 */
const V4_PRO: CapabilityProviderWire = {
  id: 'deepseek',
  label: 'DeepSeek',
  models: [{
    id: 'deepseek-v4-pro',
    name: 'DeepSeek V4 Pro',
    input: ['text', 'image'],
    capabilities: {
      vision: { value: true, source: 'manual', state: 'mismatch', evidence: '红图→无法识别；绿图→无法识别' },
      tools: { value: true, source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
      reasoning: { levels: ['low', 'high', 'max'], source: 'builtin', state: 'unverified', catalogUpdatedAt: DATA_DATE },
      context_window: { value: 131_072, source: 'builtin', state: 'unverified', catalogUpdatedAt: DATA_DATE },
    },
  }],
}

function panelElement(
  providers: readonly CapabilityProviderWire[],
  options: { readonly subscribe?: (handler: () => void) => () => void } = {},
): ReactNode {
  const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
  const wire: CapabilitiesWire = { providers: async () => providers }
  return (
    <Panel
      t={translate}
      wire={wire}
      subscribe={options.subscribe ?? (() => () => {})}
      provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
    />
  )
}

describe('src/model-capabilities：插件接缝', () => {
  it('占用 settings.models.provider-card，键控 llm-pi-ai，不碰模型行', () => {
    expect(capabilitiesInject).toEqual(['slots', 'locale', 'remote'])

    const waited: string[] = []
    const registrations: { key?: string }[] = []
    const ctx = new Context()
    ctx.provide('locale', { register: () => () => {}, bind: () => translate } as never)
    ctx.provide('remote', { $on: () => () => {} } as never)
    ctx.provide('slots', {
      inject: (key: string, contribution: () => void) => {
        waited.push(key)
        contribution()
        return () => {}
      },
      register: (options: { key?: string }) => { registrations.push(options); return () => {} },
    } as never)

    capabilitiesApply(ctx)
    expect(waited).toEqual(['settings.models.provider-card'])
    expect(registrations).toHaveLength(1)
    expect(registrations[0]?.key).toBe('llm-pi-ai')
  })
})

describe('src/model-capabilities：值 + 来源 + 三态', () => {
  it('内置表来源带数据日期，值与档位按视图原样显示', async () => {
    render(panelElement([FLASH]))
    await screen.findByText('DeepSeek Flash')
    expect(screen.getAllByText(zh['source.builtin']).length).toBeGreaterThan(0)
    expect(screen.getAllByText(zh['state.verified']).length).toBeGreaterThan(0)
    expect(screen.getByText(/2026-11-09/)).toBeTruthy()
    expect(screen.getByText('393,216')).toBeTruthy()
    expect(screen.getByText('low/high/max')).toBeTruthy()
  })

  it('不符项有独立 data-state 与证据，未验证项不冒充"测过不行"', async () => {
    render(panelElement([V4_PRO]))
    await screen.findByText('DeepSeek V4 Pro')
    const mismatch = document.querySelector('.shaniu-capabilities-chip[data-state="mismatch"]')
    expect(mismatch).toBeTruthy()
    expect(mismatch?.textContent).toContain(zh['state.mismatch'])
    expect(mismatch?.getAttribute('title')).toContain('红图→无法识别')
    expect(document.querySelectorAll('.shaniu-capabilities-chip[data-state="unverified"]').length).toBeGreaterThan(0)
  })

  it('手改一项后只有那一项是手动，其余来源不变', async () => {
    render(panelElement([V4_PRO]))
    await screen.findByText('DeepSeek V4 Pro')
    const manual = document.querySelectorAll('.shaniu-capabilities-source[data-source="manual"]')
    expect(manual).toHaveLength(1)
    expect(manual[0]?.closest('.shaniu-capabilities-chip')?.getAttribute('data-item')).toBe('vision')
    expect(document.querySelectorAll('.shaniu-capabilities-source[data-source="builtin"]').length).toBe(3)
  })

  it('检测中：显示「检测中」而非「未验证」，轮询到结论后自行更新', async () => {
    vi.useFakeTimers()
    try {
      const PROBING: CapabilityProviderWire = {
        id: 'deepseek',
        label: 'DeepSeek',
        models: [{
          id: 'deepseek-flash',
          name: 'DeepSeek Flash',
          capabilities: {
            vision: { value: true, source: 'manual', state: 'probing' },
            tools: { value: true, source: 'endpoint', state: 'probing' },
          },
        }],
      }
      let calls = 0
      const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
      render(
        <Panel
          t={translate}
          wire={{ providers: async () => { calls += 1; return calls === 1 ? [PROBING] : [FLASH] } }}
          subscribe={() => () => {}}
          provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
        />,
      )
      await act(async () => { await vi.advanceTimersByTimeAsync(0) })
      const chip = document.querySelector('.shaniu-capabilities-chip[data-state="probing"]')
      expect(chip?.textContent).toContain(zh['state.probing'])
      // 成本量级必须在界面上，不能被当成免费操作。
      expect(screen.getByText(zh.cost)).toBeTruthy()
      // 「检测中」必须看得出在动：要有已耗时计数与预计时长，不能只显示一个静态词。
      expect(screen.getByText(/预计 10–20 秒/)).toBeTruthy()

      await act(async () => { await vi.advanceTimersByTimeAsync(2_000) })
      expect(document.querySelector('.shaniu-capabilities-chip[data-state="probing"]')).toBeNull()
      expect(document.querySelector('.shaniu-capabilities-chip[data-state="verified"]')).toBeTruthy()
      expect(screen.queryByText(/预计 10–20 秒/)).toBeNull()
    } finally {
      vi.useRealTimers()
    }
  })

  it('不可判：独立状态、说明文案与手动重探入口', async () => {
    const UNJUDGEABLE: CapabilityProviderWire = {
      id: 'deepseek',
      label: 'DeepSeek',
      models: [{
        id: 'deepseek-flash',
        name: 'DeepSeek Flash',
        capabilities: {
          vision: { value: true, source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
          tools: { value: true, source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
          reasoning: {
            levels: ['low', 'high', 'max'],
            source: 'builtin',
            state: 'unjudgeable',
            evidence: '效应/噪声 0.8 低于门槛 3——数据不足，无法判定',
            catalogUpdatedAt: DATA_DATE,
          },
        },
      }],
    }
    const reprobe = vi.fn(async () => {})
    const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
    render(
      <Panel
        t={translate}
        wire={{ providers: async () => [UNJUDGEABLE], reprobe }}
        subscribe={() => () => {}}
        provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
      />,
    )
    await screen.findByText('DeepSeek Flash')
    const chip = document.querySelector('.shaniu-capabilities-chip[data-item="reasoning"]')
    // 「不可判」不能显示成「未验证」：前者是测过之后的结论，后者是没测成。
    expect(chip?.getAttribute('data-state')).toBe('unjudgeable')
    expect(chip?.textContent).toContain(zh['state.unjudgeable'])
    expect(chip?.getAttribute('title')).toContain('门槛 3')
    // 文案要告诉用户"不影响使用"，避免被读成能力不可用。
    expect(screen.getByText(zh.unjudgeableHint)).toBeTruthy()
    // 终态只能手动打破：按钮存在，点击走 force 重探。
    const button = screen.getByRole('button', { name: zh.retry })
    await act(async () => { button.click() })
    expect(reprobe).toHaveBeenCalledWith('deepseek', 'deepseek-flash')
  })

  it('读取失败如实显示，不显示成"没有能力"', async () => {
    const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
    render(
      <Panel
        t={translate}
        wire={{ providers: async () => { throw new Error('get_settings unavailable') } }}
        subscribe={() => () => {}}
        provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
      />,
    )
    await screen.findByText(/get_settings unavailable/)
  })

  it('设置写入广播后重新读取能力视图', async () => {
    let rows: readonly CapabilityProviderWire[] = [FLASH]
    const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
    let refresh: (() => void) | undefined
    const subscribe = (handler: () => void): (() => void) => {
      refresh = handler
      return () => {}
    }
    render(
      <Panel
        t={translate}
        wire={{ providers: async () => rows }}
        subscribe={subscribe}
        provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
      />,
    )
    await screen.findByText('DeepSeek Flash')
    rows = [V4_PRO]
    refresh?.()
    await waitFor(() => expect(screen.getByText('DeepSeek V4 Pro')).toBeTruthy())
  })
})
